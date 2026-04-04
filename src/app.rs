use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::thread;

use eframe::egui::{self, ColorImage, TextureHandle, TextureOptions};
use tracing::{debug, info, warn};

use crate::analysis::MotionAnalyzer;
use crate::decoder::{decode_video, decode_video_with_analysis, AnalysisMode, VideoFrame};
use crate::writer::crop_video_async;

// ── Application state ────────────────────────────────────────────────────────

#[derive(Debug, PartialEq, Eq)]
pub enum AppState {
    /// No file is loaded.
    Idle,
    /// File opened; first-frame preview decode in flight.
    LoadingPreview,
    /// Preview frame shown; waiting for the user to click Go.
    Ready,
    /// Decode + analysis running; video frames are being displayed.
    Trimming,
    /// Playback finished; waiting for the analysis result before showing the dialog.
    AnalysisPending,
}

// ── Crop dialog state machine ────────────────────────────────────────────────

enum CropDialog {
    Hidden,
    /// Analysis finished but no block exceeded the MAD threshold.
    NoRegion,
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
    /// High-level lifecycle state; replaces the old `playing` and
    /// `waiting_to_show_dialog` booleans.
    pub state: AppState,

    file_path: Option<PathBuf>,
    texture: Option<TextureHandle>,
    frame_rx: Option<Receiver<Option<VideoFrame>>>,
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
    crop_dialog: CropDialog,
    export_rx: Option<Receiver<Result<(), String>>>,

    /// Single-frame preview decoded when a file is first loaded.
    preview_rx: Option<Receiver<Option<VideoFrame>>>,
    /// Total duration of the current video in seconds; used for the progress bar.
    video_duration: Option<f64>,

    /// Threshold value that was used to start the current analysis run.
    last_threshold: f32,
    /// Frames per second of video time sampled for analysis (1–30). Only used in Full mode.
    pub analysis_fps: f32,
    /// Analysis FPS value used to start the current run.
    last_analysis_fps: f32,
    /// Whether to use fast (I-frame only) or full analysis.
    pub analysis_mode: AnalysisMode,
    /// Analysis mode used to start the current run.
    last_analysis_mode: AnalysisMode,
    /// Show "settings changed – restart?" prompt.
    restart_prompt: bool,
}

impl App {
    pub fn new(
        _cc: &eframe::CreationContext,
        initial_file: Option<PathBuf>,
        analysis_fps: f32,
        fast: bool,
    ) -> Self {
        let analysis_mode = if fast { AnalysisMode::Fast } else { AnalysisMode::Full };
        let mut app = Self {
            state: AppState::Idle,
            file_path: None,
            texture: None,
            frame_rx: None,
            current_pts: 0.0,
            error: None,
            motion_analyzer: MotionAnalyzer::default(),
            variance_threshold: 5.0,
            active_region: None,
            analysis_rx: None,
            final_region: None,
            crop_dialog: CropDialog::Hidden,
            export_rx: None,
            preview_rx: None,
            video_duration: None,
            last_threshold: 5.0,
            analysis_fps,
            last_analysis_fps: analysis_fps,
            analysis_mode,
            last_analysis_mode: analysis_mode,
            restart_prompt: false,
        };
        if let Some(path) = initial_file {
            app.open_file(path);
        }
        app
    }

    /// Reset every piece of transient per-run state. Called by both
    /// `open_file` and `start_trim` so neither can accidentally leave
    /// stale data from a previous run.
    fn reset_run_state(&mut self) {
        // Drop any live channels — the background threads will notice the
        // receiver is gone and exit cleanly.
        self.frame_rx = None;
        self.analysis_rx = None;
        self.export_rx = None;
        self.preview_rx = None;

        self.current_pts = 0.0;
        self.video_duration = None;
        self.texture = None;
        self.error = None;
        self.motion_analyzer.reset();
        self.active_region = None;
        self.final_region = None;
        self.crop_dialog = CropDialog::Hidden;
        self.restart_prompt = false;
    }

    /// Load a file without starting analysis. Resets all transient state and
    /// kicks off a background decode of the first frame for preview.
    pub fn open_file(&mut self, path: PathBuf) {
        info!(path = %path.display(), "file loaded");

        self.reset_run_state();

        let (tx, rx) = mpsc::sync_channel(1);
        thread::spawn({
            let path = path.clone();
            move || decode_video(path, tx)
        });

        self.state = AppState::LoadingPreview;
        self.file_path = Some(path);
        self.preview_rx = Some(rx);
    }

    /// (Re-)start decoding + background analysis from the beginning.
    fn start_trim(&mut self) {
        let path = match &self.file_path {
            Some(p) => p.clone(),
            None => return,
        };
        info!(
            path = %path.display(),
            threshold = self.variance_threshold,
            analysis_fps = self.analysis_fps,
            mode = ?self.analysis_mode,
            "starting trim"
        );

        self.reset_run_state();

        let (tx, rx) = mpsc::sync_channel(30);
        let analysis_rx = decode_video_with_analysis(
            path,
            tx,
            self.variance_threshold,
            self.analysis_fps,
            self.analysis_mode,
        );

        self.state = AppState::Trimming;
        self.frame_rx = Some(rx);
        self.analysis_rx = Some(analysis_rx);
        self.last_threshold = self.variance_threshold;
        self.last_analysis_fps = self.analysis_fps;
        self.last_analysis_mode = self.analysis_mode;
    }

    fn is_trimming(&self) -> bool {
        matches!(self.state, AppState::Trimming | AppState::AnalysisPending) || self.restart_prompt
    }

    // ── Preview (first-frame thumbnail on file load) ─────────────────────────

    fn poll_preview(&mut self, ctx: &egui::Context) {
        let Some(rx) = &self.preview_rx else { return };
        if let Ok(Some(frame)) = rx.try_recv() {
            if let Some(d) = frame.duration_secs {
                self.video_duration = Some(d);
            }
            self.upload_frame(ctx, frame);
            // Drop the receiver — the decode thread will get a send error and exit.
            self.preview_rx = None;
            self.state = AppState::Ready;
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
        if !matches!(self.state, AppState::Trimming) {
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
            if let Some(d) = frame.duration_secs {
                self.video_duration = Some(d);
            }
            self.upload_frame(ctx, frame);
        }

        if ended {
            self.on_playback_ended(ctx);
        } else {
            // Throttle display to ~8 fps. The decode thread runs freely between
            // repaints, filling the channel buffer; all frames are decoded and
            // analysed but only the latest one per window is shown.
            ctx.request_repaint_after(std::time::Duration::from_millis(125));
        }
    }

    fn on_playback_ended(&mut self, ctx: &egui::Context) {
        info!(
            final_pts = self.current_pts,
            final_region = ?self.final_region,
            analysis_pending = self.analysis_rx.is_some(),
            "playback ended"
        );
        self.state = AppState::Ready;
        self.crop_dialog = match (self.final_region, self.analysis_rx.is_some()) {
            (Some(region), _) => CropDialog::Confirm { region },
            (None, true) => {
                // Analysis result hasn't arrived yet; stay alive until it does.
                self.state = AppState::AnalysisPending;
                ctx.request_repaint_after(std::time::Duration::from_millis(50));
                return;
            }
            (None, false) => {
                // poll_analysis already received the result (None) before the
                // display sentinel arrived — no active region was found.
                CropDialog::NoRegion
            }
        };
    }

    // ── Analysis result polling ──────────────────────────────────────────────

    fn poll_analysis(&mut self) {
        let Some(rx) = &self.analysis_rx else { return };
        if let Ok(result) = rx.try_recv() {
            info!(region = ?result, "analysis result received");
            self.final_region = result;
            self.analysis_rx = None;
            if self.state == AppState::AnalysisPending {
                self.state = AppState::Ready;
                self.crop_dialog = match result {
                    Some(region) => CropDialog::Confirm { region },
                    None => CropDialog::NoRegion,
                };
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
                CropDialog::NoRegion => {
                    ui.label("No active region detected.");
                    ui.add_space(4.0);
                    ui.label("Every block had motion below the threshold.\nTry lowering the Motion threshold and running again.");
                    ui.add_space(6.0);
                    if ui.button("Close").clicked() {
                        action = Some(Action::Dismiss);
                    }
                }

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
                ui.label("Analysis settings changed.");
                ui.label(format!(
                    "  Threshold: {:.1} MAD   Rate: {:.0} fps",
                    self.variance_threshold, self.analysis_fps
                ));
                ui.label("Restart analysis from the beginning with the new values?");
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
            // Don't prompt again unless the settings change again.
            self.last_threshold = self.variance_threshold;
            self.last_analysis_fps = self.analysis_fps;
            self.last_analysis_mode = self.analysis_mode;
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

        // Detect settings change while analysis is running.
        if self.is_trimming()
            && !self.restart_prompt
            && ((self.variance_threshold - self.last_threshold).abs() > f32::EPSILON
                || (self.analysis_fps - self.last_analysis_fps).abs() > f32::EPSILON
                || self.analysis_mode != self.last_analysis_mode)
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
                        egui::RichText::new("Go")
                            .color(egui::Color32::WHITE)
                            .strong(),
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

                if matches!(self.state, AppState::Trimming | AppState::AnalysisPending) {
                    let progress = self.video_duration
                        .filter(|&d| d > 0.0)
                        .map(|d| (self.current_pts / d).clamp(0.0, 1.0) as f32)
                        .unwrap_or(0.0);
                    ui.add(
                        egui::ProgressBar::new(progress)
                            .desired_width(180.0)
                            .text(format!("analysing… {:.0}%", progress * 100.0))
                            .animate(true),
                    );
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

            ui.horizontal(|ui| {
                ui.label("Analysis mode:");
                ui.radio_value(&mut self.analysis_mode, AnalysisMode::Full, "Full");
                ui.radio_value(&mut self.analysis_mode, AnalysisMode::Fast, "Fast (I-frames only)");
                ui.add_space(8.0);
                ui.add_enabled_ui(self.analysis_mode == AnalysisMode::Full, |ui| {
                    ui.label("Rate:");
                    ui.add(
                        egui::Slider::new(&mut self.analysis_fps, 1.0..=30.0)
                            .step_by(1.0)
                            .fixed_decimals(0)
                            .suffix(" fps"),
                    );
                });
                if self.analysis_mode == AnalysisMode::Fast {
                    ui.weak("(keyframes only — much faster, accuracy depends on GOP size)");
                } else {
                    ui.weak("(video time sampled per second)");
                }
            });

            // Debug state label — only compiled into debug builds.
            #[cfg(debug_assertions)]
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                ui.weak(format!("{:?}", self.state));
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
