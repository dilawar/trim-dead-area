use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use eframe::egui::{self, ColorImage, TextureHandle, TextureOptions};
use tracing::{debug, info, warn};

use crate::analysis::MotionAnalyzer;
use crate::decoder::{decode_video, VideoFrame};

pub struct App {
    file_path: Option<PathBuf>,
    texture: Option<TextureHandle>,
    frame_rx: Option<Receiver<Option<VideoFrame>>>,
    /// One-frame lookahead: a decoded frame whose PTS is still in the future.
    lookahead: Option<VideoFrame>,
    play_start: Option<Instant>,
    paused_at: Option<f64>,
    playing: bool,
    error: Option<String>,

    // ── Motion analysis ────────────────────────────────────────────────────
    motion_analyzer: MotionAnalyzer,
    /// MAD threshold (0–255 intensity units). Blocks below this are "dead".
    variance_threshold: f32,
    /// Bounding box [x, y, w, h] of the high-motion region, in pixel coords.
    active_region: Option<[u32; 4]>,
    /// When true, only the active region is displayed (UV crop). Otherwise the
    /// full frame is shown with an overlay rectangle.
    apply_crop: bool,
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
        }
    }

    pub fn open_file(&mut self, path: PathBuf) {
        info!(path = %path.display(), "opening file");
        let (tx, rx) = mpsc::sync_channel(30);
        let path_clone = path.clone();
        thread::spawn(move || decode_video(path_clone, tx));

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

    /// Drain the decode channel and update the displayed texture to the most
    /// recent frame whose PTS is ≤ current wall-clock playback position.
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
        // Run motion analysis; update the active region bounding box.
        let new_region = self.motion_analyzer.update(&frame, self.variance_threshold);
        if new_region != self.active_region {
            debug!(region = ?new_region, "active region changed");
            self.active_region = new_region;
        }

        // Always upload the full frame — cropping is handled via UV at draw time.
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
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.playing {
            ctx.request_repaint();
        }

        self.poll_frames(ctx);

        // Handle drag-and-drop.
        let dropped = ctx.input(|i| {
            i.raw.dropped_files.first().and_then(|f| f.path.clone())
        });
        if let Some(path) = dropped {
            self.open_file(path);
        }

        // ── Bottom control bar ──────────────────────────────────────────────
        egui::TopBottomPanel::bottom("controls").show(ctx, |ui| {
            ui.add_space(6.0);

            // Row 1: playback controls.
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

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let t = self.video_time();
                    let m = (t / 60.0) as u64;
                    let s = t % 60.0;
                    ui.monospace(format!("{m:02}:{s:05.2}"));
                });
            });

            ui.separator();

            // Row 2: motion analysis controls.
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

                // Show bounding box dimensions when a region has been found.
                if let Some([x, y, w, h]) = self.active_region {
                    ui.separator();
                    ui.weak(format!("active region  {w}×{h}  @ ({x},{y})"));
                }
            });

            ui.add_space(6.0);
        });

        // ── Central video area ──────────────────────────────────────────────
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

            let full_size = texture.size_vec2(); // full frame dimensions
            let avail = ui.available_rect_before_wrap();

            // Determine which portion of the texture to show (UV in [0,1]²)
            // and what the effective aspect ratio of that portion is.
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

            // Fit `effective` into the available rect while preserving aspect ratio.
            let scale = (avail.width() / effective.x).min(avail.height() / effective.y);
            let disp_size = effective * scale;
            let disp_rect = egui::Rect::from_center_size(avail.center(), disp_size);

            let painter = ui.painter();
            painter.image(texture.id(), disp_rect, uv, egui::Color32::WHITE);

            // Overlay rectangle showing the active region (only when not cropping).
            if !self.apply_crop {
                if let Some([rx, ry, rw, rh]) = self.active_region {
                    let sx = disp_size.x / full_size.x;
                    let sy = disp_size.y / full_size.y;
                    let overlay = egui::Rect::from_min_size(
                        disp_rect.min + egui::vec2(rx as f32 * sx, ry as f32 * sy),
                        egui::vec2(rw as f32 * sx, rh as f32 * sy),
                    );
                    painter.rect_stroke(
                        overlay,
                        0.0,
                        egui::Stroke::new(2.0, egui::Color32::YELLOW),
                        egui::StrokeKind::Outside,
                    );
                }
            }
        });
    }
}
