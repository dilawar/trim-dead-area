use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use eframe::egui::{self, ColorImage, TextureHandle, TextureOptions};
use tracing::{debug, info, warn};

use crate::analysis::{analyze_file_async, MotionAnalyzer};
use crate::decoder::{decode_video, VideoFrame};
use crate::writer::crop_video_async;

// ── Crop dialog state machine ────────────────────────────────────────────────

enum CropDialog {
    Hidden,
    Confirm { region: [u32; 4] },
    /// ffmpeg is running; we keep the output path here so Done can show it.
    Exporting { region: [u32; 4], output: PathBuf },
    Done { output: PathBuf },
    Failed { message: String },
}

// ── App ──────────────────────────────────────────────────────────────────────

pub struct App {
    file_path: Option<PathBuf>,
    texture: Option<TextureHandle>,
    frame_rx: Option<Receiver<Option<VideoFrame>>>,
    lookahead: Option<VideoFrame>,
    play_start: Option<Instant>,
    paused_at: Option<f64>,
    playing: bool,
    error: Option<String>,

    // Real-time motion analysis (during playback).
    motion_analyzer: MotionAnalyzer,
    pub variance_threshold: f32,
    active_region: Option<[u32; 4]>,
    apply_crop: bool,

    // Full-video background analysis.
    analysis_rx: Option<Receiver<Option<[u32; 4]>>>,
    final_region: Option<[u32; 4]>,
    crop_dialog: CropDialog,
    export_rx: Option<Receiver<Result<(), String>>>,
}

impl App {
    pub fn new(_cc: &eframe::CreationContext) -> Self {
        Self {
            file_path: None,
            texture: None,
            frame_rx: None,
            lookahead: None,
            play_start: None,
            paused_at: None,
            playing: false,
            error: None,
            motion_analyzer: MotionAnalyzer::default(),
            variance_threshold: 5.0,
            active_region: None,
            apply_crop: false,
            analysis_rx: None,
            final_region: None,
            crop_dialog: CropDialog::Hidden,
            export_rx: None,
        }
    }

    pub fn open_file(&mut self, path: PathBuf) {
        info!(path = %path.display(), "opening file");

        let (tx, rx) = mpsc::sync_channel(30);
        thread::spawn({
            let path = path.clone();
            move || decode_video(path, tx)
        });

        let analysis_rx = analyze_file_async(path.clone(), 4, self.variance_threshold);

        self.file_path = Some(path);
        self.frame_rx = Some(rx);
        self.lookahead = None;
        self.play_start = Some(Instant::now());
        self.paused_at = None;
        self.playing = true;
        self.texture = None;
        self.error = None;
        self.motion_analyzer.reset();
        self.active_region = None;
        self.analysis_rx = Some(analysis_rx);
        self.final_region = None;
        self.crop_dialog = CropDialog::Hidden;
        self.export_rx = None;
    }

    fn video_time(&self) -> f64 {
        match self.paused_at {
            Some(t) => t,
            None => self.play_start.map_or(0.0, |s| s.elapsed().as_secs_f64()),
        }
    }

    fn toggle_play_pause(&mut self) {
        if self.playing {
            let t = self.video_time();
            self.paused_at = Some(t);
            self.playing = false;
            info!(at_secs = t, "playback paused");
        } else if let Some(paused) = self.paused_at.take() {
            self.play_start = Some(Instant::now() - Duration::from_secs_f64(paused));
            self.playing = true;
            info!(from_secs = paused, "playback resumed");
        }
    }

    fn poll_frames(&mut self, ctx: &egui::Context) {
        if !self.playing {
            return;
        }
        let now = self.video_time();
        let rx = match &self.frame_rx {
            Some(rx) => rx,
            None => return,
        };

        let mut latest: Option<VideoFrame> = None;

        if let Some(frame) = self.lookahead.take() {
            if frame.pts_secs <= now {
                latest = Some(frame);
            } else {
                self.lookahead = Some(frame);
                if let Some(f) = latest {
                    self.upload_frame(ctx, f);
                }
                return;
            }
        }

        loop {
            match rx.try_recv() {
                Ok(Some(frame)) => {
                    if frame.pts_secs <= now {
                        latest = Some(frame);
                    } else {
                        self.lookahead = Some(frame);
                        break;
                    }
                }
                Ok(None) => {
                    self.playing = false;
                    self.paused_at = Some(now);
                    info!("playback ended");
                    break;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    warn!("decoder thread disconnected unexpectedly");
                    self.playing = false;
                    break;
                }
            }
        }

        if let Some(f) = latest {
            self.upload_frame(ctx, f);
        }
    }

    fn upload_frame(&mut self, ctx: &egui::Context, frame: VideoFrame) {
        let new_region = self.motion_analyzer.update(&frame, self.variance_threshold);
        if new_region != self.active_region {
            debug!(region = ?new_region, "active region changed");
            self.active_region = new_region;
        }

        let image = ColorImage::from_rgba_unmultiplied(
            [frame.width as usize, frame.height as usize],
            &frame.rgba,
        );
        match &mut self.texture {
            Some(tex) => tex.set(image, TextureOptions::default()),
            None => {
                self.texture =
                    Some(ctx.load_texture("video_frame", image, TextureOptions::default()));
            }
        }
    }

    fn poll_analysis(&mut self) {
        let Some(rx) = &self.analysis_rx else { return };
        if let Ok(result) = rx.try_recv() {
            info!(region = ?result, "background analysis complete");
            self.final_region = result;
            self.analysis_rx = None;
            if let Some(region) = result {
                self.crop_dialog = CropDialog::Confirm { region };
            }
        }
    }

    fn poll_export(&mut self) {
        let Some(rx) = &self.export_rx else { return };
        if let Ok(result) = rx.try_recv() {
            self.export_rx = None;
            // Extract the output path from the current Exporting state.
            let output = match &self.crop_dialog {
                CropDialog::Exporting { output, .. } => output.clone(),
                _ => PathBuf::new(),
            };
            self.crop_dialog = match result {
                Ok(()) => {
                    info!(output = %output.display(), "export complete");
                    CropDialog::Done { output }
                }
                Err(e) => {
                    warn!(error = %e, "export failed");
                    CropDialog::Failed { message: e }
                }
            };
        }
    }

    // ── Crop dialog ──────────────────────────────────────────────────────────

    fn show_crop_dialog(&mut self, ctx: &egui::Context) {
        if matches!(self.crop_dialog, CropDialog::Hidden) {
            return;
        }

        enum Action {
            StartExport { region: [u32; 4], output: PathBuf },
            Dismiss,
        }
        let mut action: Option<Action> = None;

        egui::Window::new("Active Region")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| match &self.crop_dialog {
                CropDialog::Confirm { region } => {
                    let [x, y, w, h] = *region;
                    ui.label("Full-video analysis found the most active region:");
                    ui.monospace(format!("  {w} × {h}  at  ({x}, {y})"));
                    ui.add_space(6.0);
                    ui.label("Write a new video file cropped to this rectangle?");
                    ui.small("All frames will be included — no frames are skipped in the output.");
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        if ui.button("Save Cropped Video…").clicked() {
                            if let Some(output) = rfd::FileDialog::new()
                                .add_filter("MP4 video", &["mp4"])
                                .set_file_name("cropped.mp4")
                                .save_file()
                            {
                                action = Some(Action::StartExport { region: [x, y, w, h], output });
                            }
                        }
                        if ui.button("Dismiss").clicked() {
                            action = Some(Action::Dismiss);
                        }
                    });
                }

                CropDialog::Exporting { region, .. } => {
                    let [_, _, w, h] = *region;
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(format!("Exporting {w}×{h} crop…"));
                    });
                    ui.small("Running ffmpeg — every frame is written to the output.");
                }

                CropDialog::Done { output } => {
                    ui.label("Export complete!");
                    ui.monospace(output.display().to_string());
                    ui.add_space(4.0);
                    if ui.button("Close").clicked() {
                        action = Some(Action::Dismiss);
                    }
                }

                CropDialog::Failed { message } => {
                    ui.colored_label(egui::Color32::RED, "Export failed:");
                    ui.label(message);
                    ui.add_space(4.0);
                    if ui.button("Close").clicked() {
                        action = Some(Action::Dismiss);
                    }
                }

                CropDialog::Hidden => {}
            });

        match action {
            Some(Action::StartExport { region, output }) => {
                info!(output = %output.display(), region = ?region, "starting export");
                if let Some(input) = &self.file_path {
                    let rx = crop_video_async(input.clone(), output.clone(), region);
                    self.export_rx = Some(rx);
                    self.crop_dialog = CropDialog::Exporting { region, output };
                }
            }
            Some(Action::Dismiss) => {
                self.crop_dialog = CropDialog::Hidden;
            }
            None => {}
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.playing {
            ctx.request_repaint();
        }

        self.poll_frames(ctx);
        self.poll_analysis();
        self.poll_export();

        // Repaint while export is in progress.
        if self.export_rx.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }

        let dropped = ctx.input(|i| {
            i.raw.dropped_files.first().and_then(|f| f.path.clone())
        });
        if let Some(path) = dropped {
            self.open_file(path);
        }

        self.show_crop_dialog(ctx);

        // ── Bottom control bar ───────────────────────────────────────────────
        egui::TopBottomPanel::bottom("controls").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button("Open File").clicked() {
                    if let Some(path) = rfd::FileDialog::new()
                        .add_filter(
                            "Video",
                            &["mp4", "mkv", "avi", "mov", "webm", "flv", "wmv", "ts", "m4v"],
                        )
                        .pick_file()
                    {
                        self.open_file(path);
                    }
                }

                ui.add_enabled_ui(self.file_path.is_some(), |ui| {
                    let label = if self.playing { "⏸ Pause" } else { "▶ Play" };
                    if ui.button(label).clicked() {
                        self.toggle_play_pause();
                    }
                });

                if let Some(path) = &self.file_path {
                    let name = path.file_name().unwrap_or_default().to_string_lossy();
                    ui.label(format!("  {name}"));
                }

                if self.analysis_rx.is_some() {
                    ui.spinner();
                    ui.weak("analysing…");
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let t = self.video_time();
                    let m = (t / 60.0) as u64;
                    let s = t % 60.0;
                    ui.monospace(format!("{m:02}:{s:05.2}"));
                });
            });

            ui.separator();

            ui.horizontal(|ui| {
                ui.label("Motion threshold:");
                ui.add(
                    egui::Slider::new(&mut self.variance_threshold, 0.0..=30.0)
                        .step_by(0.5)
                        .fixed_decimals(1)
                        .suffix(" MAD"),
                );
                ui.separator();
                ui.checkbox(&mut self.apply_crop, "Crop to active region");
                if let Some([x, y, w, h]) = self.active_region {
                    ui.separator();
                    ui.weak(format!("live {w}×{h} @ ({x},{y})"));
                }
            });

            ui.add_space(6.0);
        });

        // ── Central video area ───────────────────────────────────────────────
        egui::CentralPanel::default().show(ctx, |ui| {
            if let Some(err) = &self.error {
                let msg = err.clone();
                ui.centered_and_justified(|ui| {
                    ui.colored_label(egui::Color32::RED, msg);
                });
                return;
            }

            let Some(texture) = &self.texture else {
                ui.centered_and_justified(|ui| {
                    ui.label("Open a video file or drag-and-drop to start playback");
                });
                return;
            };

            let full_size = texture.size_vec2();
            let avail = ui.available_rect_before_wrap();

            let (uv, effective) = match (self.apply_crop, self.active_region) {
                (true, Some([rx, ry, rw, rh])) => {
                    let uv = egui::Rect::from_min_max(
                        egui::pos2(rx as f32 / full_size.x, ry as f32 / full_size.y),
                        egui::pos2(
                            (rx + rw) as f32 / full_size.x,
                            (ry + rh) as f32 / full_size.y,
                        ),
                    );
                    (uv, egui::vec2(rw as f32, rh as f32))
                }
                _ => (
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    full_size,
                ),
            };

            let scale = (avail.width() / effective.x).min(avail.height() / effective.y);
            let disp_size = effective * scale;
            let disp_rect = egui::Rect::from_center_size(avail.center(), disp_size);

            let painter = ui.painter();
            painter.image(texture.id(), disp_rect, uv, egui::Color32::WHITE);

            if !self.apply_crop {
                let sx = disp_size.x / full_size.x;
                let sy = disp_size.y / full_size.y;

                let overlay_rect = |rx: u32, ry: u32, rw: u32, rh: u32| {
                    egui::Rect::from_min_size(
                        disp_rect.min + egui::vec2(rx as f32 * sx, ry as f32 * sy),
                        egui::vec2(rw as f32 * sx, rh as f32 * sy),
                    )
                };

                // Yellow: live EMA region (real-time, updates each frame).
                if let Some([rx, ry, rw, rh]) = self.active_region {
                    painter.rect_stroke(
                        overlay_rect(rx, ry, rw, rh),
                        0.0,
                        egui::Stroke::new(2.0, egui::Color32::YELLOW),
                        egui::StrokeKind::Outside,
                    );
                }

                // Cyan: full-video analysis result (stable, computed once).
                if let Some([rx, ry, rw, rh]) = self.final_region {
                    painter.rect_stroke(
                        overlay_rect(rx, ry, rw, rh),
                        0.0,
                        egui::Stroke::new(2.0, egui::Color32::from_rgb(0, 220, 220)),
                        egui::StrokeKind::Outside,
                    );
                }
            }
        });
    }
}
