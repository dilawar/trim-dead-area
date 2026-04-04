use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::thread;

use eframe::egui::{self, ColorImage, TextureHandle, TextureOptions};
use tracing::{debug, info, warn};

use crate::analysis::MotionAnalyzer;
use crate::decoder::{decode_video, decode_video_with_analysis, VideoFrame};
use crate::writer::crop_video_async;

// ── Crop dialog state machine ────────────────────────────────────────────────

enum CropDialog {
    Hidden,
    Confirm {
        region: [u32; 4],
    },
    /// ffmpeg is running.
    Exporting {
        region: [u32; 4],
        output: PathBuf,
    },
    Done {
        output: PathBuf,
    },
    Failed {
        message: String,
    },
}

// ── App ──────────────────────────────────────────────────────────────────────

pub struct App {
    file_path: Option<PathBuf>,
    texture: Option<TextureHandle>,
    frame_rx: Option<Receiver<Option<VideoFrame>>>,
    playing: bool,
    /// PTS of the last displayed frame (seconds); used only for the time readout.
    current_pts: f64,
    error: Option<String>,

    // Real-time motion analysis (during playback).
    motion_analyzer: MotionAnalyzer,
    pub variance_threshold: f32,
    active_region: Option<[u32; 4]>,

    // Full-video analysis result channel (fed by the decode thread itself).
    analysis_rx: Option<Receiver<Option<[u32; 4]>>>,
    final_region: Option<[u32; 4]>,
    /// Set when playback finishes before the analysis result arrives.
    waiting_to_show_dialog: bool,
    crop_dialog: CropDialog,
    export_rx: Option<Receiver<Result<(), String>>>,

    /// Single-frame preview decoded when a file is first loaded.
    preview_rx: Option<Receiver<Option<VideoFrame>>>,

    /// Threshold value that was used to start the current analysis run.
    last_threshold: f32,
    /// Show "MAD changed – restart?" prompt.
    restart_prompt: bool,
}

impl App {
    pub fn new(_cc: &eframe::CreationContext, initial_file: Option<PathBuf>) -> Self {
        let mut app = Self {
            file_path: None,
            texture: None,
            frame_rx: None,
            playing: false,
            current_pts: 0.0,
            error: None,
            motion_analyzer: MotionAnalyzer::default(),
            variance_threshold: 5.0,
            active_region: None,
            analysis_rx: None,
            final_region: None,
            waiting_to_show_dialog: false,
            crop_dialog: CropDialog::Hidden,
            export_rx: None,
            preview_rx: None,
            last_threshold: 5.0,
            restart_prompt: false,
        };
        if let Some(path) = initial_file {
            app.open_file(path);
        }
        app
    }

    /// Load a file without starting analysis. Resets all transient state and
    /// kicks off a background decode of the first frame for preview.
    pub fn open_file(&mut self, path: PathBuf) {
        info!(path = %path.display(), "file loaded");

        let (tx, rx) = mpsc::sync_channel(1);
        thread::spawn({
            let path = path.clone();
            move || decode_video(path, tx)
        });

        self.file_path = Some(path);
        self.frame_rx = None;
        self.playing = false;
        self.current_pts = 0.0;
        self.texture = None;
        self.error = None;
        self.motion_analyzer.reset();
        self.active_region = None;
        self.analysis_rx = None;
        self.final_region = None;
        self.waiting_to_show_dialog = false;
        self.crop_dialog = CropDialog::Hidden;
        self.export_rx = None;
        self.preview_rx = Some(rx);
        self.restart_prompt = false;
    }

    /// (Re-)start decoding + background analysis from the beginning.
    fn start_trim(&mut self) {
        let path = match &self.file_path {
            Some(p) => p.clone(),
            None => return,
        };
        info!(path = %path.display(), threshold = self.variance_threshold, "starting trim");

        let (tx, rx) = mpsc::sync_channel(30);
        let analysis_rx = decode_video_with_analysis(path, tx, self.variance_threshold);

        self.frame_rx = Some(rx);
        self.playing = true;
        self.current_pts = 0.0;
        self.texture = None;
        self.error = None;
        self.motion_analyzer.reset();
        self.active_region = None;
        self.analysis_rx = Some(analysis_rx);
        self.final_region = None;
        self.waiting_to_show_dialog = false;
        self.crop_dialog = CropDialog::Hidden;
        self.export_rx = None;
        self.preview_rx = None;
        self.last_threshold = self.variance_threshold;
        self.restart_prompt = false;
    }

    fn is_trimming(&self) -> bool {
        self.playing || self.analysis_rx.is_some()
    }

    // ── Preview (first-frame thumbnail on file load) ─────────────────────────

    fn poll_preview(&mut self, ctx: &egui::Context) {
        let Some(rx) = &self.preview_rx else { return };
        if let Ok(Some(frame)) = rx.try_recv() {
            self.upload_frame(ctx, frame);
            // Drop the receiver — the decode thread will get a send error and exit.
            self.preview_rx = None;
            ctx.request_repaint();
        }
    }

    // ── Frame polling ────────────────────────────────────────────────────────

    /// Drain the decode channel on every repaint.
    ///
    /// All available frames are consumed; only the latest is displayed so the
    /// video plays as fast as the decoder and screen refresh allow, skipping
    /// intermediate frames freely.
    fn poll_frames(&mut self, ctx: &egui::Context) {
        if !self.playing {
            return;
        }
        if self.frame_rx.is_none() {
            return;
        }

        let mut latest: Option<VideoFrame> = None;
        let mut ended = false;

        let rx = match &self.frame_rx {
            Some(rx) => rx,
            None => return,
        };

        loop {
            match rx.try_recv() {
                Ok(Some(frame)) => {
                    latest = Some(frame);
                }
                Ok(None) => {
                    ended = true;
                    break;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    warn!("decoder thread disconnected unexpectedly");
                    ended = true;
                    break;
                }
            }
        }

        if let Some(frame) = latest {
            self.current_pts = frame.pts_secs;
            self.upload_frame(ctx, frame);
        }

        if ended {
            self.playing = false;
            self.on_playback_ended();
        } else {
            // Ask for another repaint immediately so we keep consuming frames
            // as fast as the decoder produces them.
            ctx.request_repaint();
        }
    }

    fn on_playback_ended(&mut self) {
        info!(final_pts = self.current_pts, "playback ended");
        if let Some(region) = self.final_region {
            self.crop_dialog = CropDialog::Confirm { region };
        } else {
            // Analysis result hasn't arrived yet; show dialog as soon as it does.
            self.waiting_to_show_dialog = true;
        }
    }

    // ── Analysis result polling ──────────────────────────────────────────────

    fn poll_analysis(&mut self) {
        let Some(rx) = &self.analysis_rx else { return };
        if let Ok(result) = rx.try_recv() {
            info!(region = ?result, "analysis result received");
            self.final_region = result;
            self.analysis_rx = None;
            if self.waiting_to_show_dialog {
                self.waiting_to_show_dialog = false;
                if let Some(region) = result {
                    self.crop_dialog = CropDialog::Confirm { region };
                }
            }
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

    fn poll_export(&mut self) {
        let Some(rx) = &self.export_rx else { return };
        if let Ok(result) = rx.try_recv() {
            self.export_rx = None;
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

        egui::Window::new("Active Region Detected")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| match &self.crop_dialog {
                CropDialog::Confirm { region } => {
                    let [x, y, w, h] = *region;
                    ui.label("The most active region across the full video:");
                    ui.monospace(format!("  {w} × {h}  at  ({x}, {y})"));
                    ui.add_space(6.0);
                    ui.label("Crop the video to this rectangle?");
                    ui.small("All frames are written to the output — none are skipped.");
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        if ui.button("Save Cropped Video…").clicked() {
                            let default_name = self
                                .file_path
                                .as_deref()
                                .and_then(|p| {
                                    let stem = p.file_stem()?.to_string_lossy();
                                    let ext = p
                                        .extension()
                                        .map(|e| e.to_string_lossy())
                                        .unwrap_or("mp4".into());
                                    Some(format!("{stem}_cropped.{ext}"))
                                })
                                .unwrap_or_else(|| "cropped.mp4".into());
                            if let Some(output) = rfd::FileDialog::new()
                                .add_filter("MP4 video", &["mp4"])
                                .set_file_name(&default_name)
                                .save_file()
                            {
                                action = Some(Action::StartExport {
                                    region: [x, y, w, h],
                                    output,
                                });
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
    fn show_restart_prompt(&mut self, ctx: &egui::Context) {
        if !self.restart_prompt {
            return;
        }

        let mut restart = false;
        let mut keep = false;

        egui::Window::new("Restart Analysis?")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(format!(
                    "Motion threshold changed to {:.1} MAD.",
                    self.variance_threshold
                ));
                ui.label("Restart analysis from the beginning with the new value?");
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button("Restart").clicked() {
                        restart = true;
                    }
                    if ui.button("Keep going").clicked() {
                        keep = true;
                    }
                });
            });

        if restart {
            self.start_trim();
        } else if keep {
            // Don't prompt again unless the threshold changes again.
            self.last_threshold = self.variance_threshold;
            self.restart_prompt = false;
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_preview(ctx);
        self.poll_frames(ctx);
        self.poll_analysis();
        self.poll_export();

        // Keep repainting while export spinner is shown.
        if self.export_rx.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }

        let dropped = ctx.input(|i| i.raw.dropped_files.first().and_then(|f| f.path.clone()));
        if let Some(path) = dropped {
            self.open_file(path);
        }

        // Detect MAD threshold change while analysis is running.
        if self.is_trimming()
            && !self.restart_prompt
            && (self.variance_threshold - self.last_threshold).abs() > f32::EPSILON
        {
            self.restart_prompt = true;
        }

        self.show_crop_dialog(ctx);
        self.show_restart_prompt(ctx);

        // ── Bottom control bar ───────────────────────────────────────────────
        egui::TopBottomPanel::bottom("controls").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button("Open File").clicked() {
                    if let Some(path) = rfd::FileDialog::new()
                        .add_filter(
                            "Video",
                            &[
                                "mp4", "mkv", "avi", "mov", "webm", "flv", "wmv", "ts", "m4v",
                            ],
                        )
                        .pick_file()
                    {
                        self.open_file(path);
                    }
                }

                ui.add_enabled_ui(self.file_path.is_some(), |ui| {
                    let go = egui::Button::new(
                        egui::RichText::new("Go").color(egui::Color32::WHITE).strong(),
                    )
                    .fill(egui::Color32::from_rgb(34, 139, 34));
                    if ui.add(go).clicked() {
                        self.start_trim();
                    }
                });

                if let Some(path) = &self.file_path {
                    let name = path.file_name().unwrap_or_default().to_string_lossy();
                    ui.label(format!("  {name}"));
                }

                if self.playing {
                    ui.spinner();
                    ui.weak("analysing…");
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let t = self.current_pts;
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
                ui.weak("(raise to ignore camera shake or compression noise)");
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

            let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
            let effective = full_size;

            let scale = (avail.width() / effective.x).min(avail.height() / effective.y);
            let disp_rect = egui::Rect::from_center_size(avail.center(), effective * scale);

            let painter = ui.painter();
            painter.image(texture.id(), disp_rect, uv, egui::Color32::WHITE);

            let sx = disp_rect.width() / full_size.x;
            let sy = disp_rect.height() / full_size.y;

            let overlay = |rx: u32, ry: u32, rw: u32, rh: u32| {
                egui::Rect::from_min_size(
                    disp_rect.min + egui::vec2(rx as f32 * sx, ry as f32 * sy),
                    egui::vec2(rw as f32 * sx, rh as f32 * sy),
                )
            };

            // Yellow: live EMA region (updates every displayed frame).
            if let Some([rx, ry, rw, rh]) = self.active_region {
                painter.rect_stroke(
                    overlay(rx, ry, rw, rh),
                    0.0,
                    egui::Stroke::new(2.0, egui::Color32::YELLOW),
                    egui::StrokeKind::Outside,
                );
            }

            // Cyan: full-video result (stable once analysis finishes).
            if let Some([rx, ry, rw, rh]) = self.final_region {
                painter.rect_stroke(
                    overlay(rx, ry, rw, rh),
                    0.0,
                    egui::Stroke::new(2.0, egui::Color32::from_rgb(0, 220, 220)),
                    egui::StrokeKind::Outside,
                );
            }
        });
    }
}
