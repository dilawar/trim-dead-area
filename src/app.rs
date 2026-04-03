use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use eframe::egui::{self, ColorImage, TextureHandle, TextureOptions};

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
        }
    }

    pub fn open_file(&mut self, path: PathBuf) {
        // Buffer ~1 s of 30 fps video in the channel.
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
    }

    fn video_time(&self) -> f64 {
        match self.paused_at {
            Some(t) => t,
            None => self.play_start.map_or(0.0, |s| s.elapsed().as_secs_f64()),
        }
    }

    fn toggle_play_pause(&mut self) {
        if self.playing {
            self.paused_at = Some(self.video_time());
            self.playing = false;
        } else if let Some(paused) = self.paused_at.take() {
            self.play_start = Some(Instant::now() - Duration::from_secs_f64(paused));
            self.playing = true;
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

        // Check the one-frame lookahead buffer first.
        if let Some(frame) = self.lookahead.take() {
            if frame.pts_secs <= now {
                latest = Some(frame);
            } else {
                // Not yet time for this frame — put it back and stop polling.
                self.lookahead = Some(frame);
                if let Some(f) = latest {
                    self.upload_frame(ctx, f);
                }
                return;
            }
        }

        // Drain the channel for all frames up to `now`.
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
                    // End of stream.
                    self.playing = false;
                    self.paused_at = Some(now);
                    break;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
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
        // Keep repainting while video is playing.
        if self.playing {
            ctx.request_repaint();
        }

        self.poll_frames(ctx);

        // Handle drag-and-drop.
        let dropped = ctx.input(|i| {
            i.raw
                .dropped_files
                .first()
                .and_then(|f| f.path.clone())
        });
        if let Some(path) = dropped {
            self.open_file(path);
        }

        // ── Bottom control bar ──────────────────────────────────────────────
        egui::TopBottomPanel::bottom("controls")
            .min_height(48.0)
            .show(ctx, |ui| {
                ui.add_space(8.0);
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
                ui.add_space(8.0);
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

            if let Some(texture) = &self.texture {
                let avail = ui.available_size();
                let tex_size = texture.size_vec2();
                let scale = (avail.x / tex_size.x).min(avail.y / tex_size.y);
                let display_size = tex_size * scale;

                ui.centered_and_justified(|ui| {
                    ui.image((texture.id(), display_size));
                });
            } else {
                ui.centered_and_justified(|ui| {
                    ui.label("Open a video file or drag-and-drop to start playback");
                });
            }
        });
    }
}
