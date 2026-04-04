use std::path::Path;
use std::sync::mpsc::{self, Receiver};

use tracing::{debug, error, info, trace};

use crate::decoder::VideoFrame;

// ── Block size ───────────────────────────────────────────────────────────────

/// Side length (in pixels) of each analysis block.
const BLOCK: usize = 16;

// ── Real-time analyser (EMA, used during playback) ───────────────────────────

/// Tracks per-block inter-frame motion via an exponential moving average of
/// the mean absolute difference (MAD) between consecutive grayscale frames.
#[derive(Default)]
pub struct MotionAnalyzer {
    prev_gray: Vec<u8>,
    /// EMA of per-block MAD, in the same units as pixel intensity (0–255).
    motion_map: Vec<f32>,
    pub frame_width: u32,
    pub frame_height: u32,
    cols: usize,
    rows: usize,
}

impl MotionAnalyzer {
    pub fn reset(&mut self) {
        debug!("motion analyzer reset");
        *self = Self::default();
    }

    /// Feed the next displayed frame. Returns the smallest bounding box
    /// `[x, y, w, h]` (pixel coords) covering every block whose EMA motion
    /// score ≥ `threshold`, or `None` while warming up.
    pub fn update(&mut self, frame: &VideoFrame, threshold: f32) -> Option<[u32; 4]> {
        let w = frame.width as usize;
        let h = frame.height as usize;
        let cols = w.div_ceil(BLOCK);
        let rows = h.div_ceil(BLOCK);

        if self.frame_width != frame.width || self.frame_height != frame.height {
            debug!(
                width = w,
                height = h,
                cols,
                rows,
                "motion analyzer initialised"
            );
            self.prev_gray = vec![0u8; w * h];
            self.motion_map = vec![0.0f32; cols * rows];
            self.cols = cols;
            self.rows = rows;
            self.frame_width = frame.width;
            self.frame_height = frame.height;
        }

        let gray = to_gray(&frame.rgba);

        for by in 0..rows {
            for bx in 0..cols {
                let mad = block_mad(&gray, &self.prev_gray, w, bx, by);
                let slot = &mut self.motion_map[by * cols + bx];
                trace!(by, bx, mad, ema = *slot, "block update");
                *slot = 0.95 * *slot + 0.05 * mad;
            }
        }
        self.prev_gray = gray;

        active_bbox(
            &self.motion_map,
            self.cols,
            self.rows,
            self.frame_width,
            self.frame_height,
            threshold,
        )
    }
}

// ── Full-video analyser (running mean, used by background pass) ──────────────

/// Accumulates the **mean** inter-frame MAD across all analysed frames so that
/// every sample contributes equally, unlike the EMA which emphasises recent frames.
pub struct FullVideoAnalyzer {
    prev_gray: Vec<u8>,
    mad_sum: Vec<f64>,
    frames: u64,
    frame_width: u32,
    frame_height: u32,
    cols: usize,
    rows: usize,
}

impl Default for FullVideoAnalyzer {
    fn default() -> Self {
        Self::new()
    }
}

impl FullVideoAnalyzer {
    pub fn new() -> Self {
        Self {
            prev_gray: Vec::new(),
            mad_sum: Vec::new(),
            frames: 0,
            frame_width: 0,
            frame_height: 0,
            cols: 0,
            rows: 0,
        }
    }

    pub fn update(&mut self, frame: &VideoFrame) {
        let w = frame.width as usize;
        let h = frame.height as usize;
        let gray = to_gray(&frame.rgba);
        self.update_gray(w, h, frame.width, frame.height, gray);
    }

    /// Analyse a frame using a pre-extracted luma (Y) plane.
    ///
    /// `y` must be a contiguous row-major byte slice of length `w * h` —
    /// callers are responsible for de-striding before passing it in.
    /// This avoids the RGBA conversion entirely, which is the dominant cost
    /// in [`update`] for large frames.
    pub fn update_y(&mut self, y: Vec<u8>, width: u32, height: u32) {
        self.update_gray(width as usize, height as usize, width, height, y);
    }

    fn update_gray(&mut self, w: usize, h: usize, fw: u32, fh: u32, gray: Vec<u8>) {
        let cols = w.div_ceil(BLOCK);
        let rows = h.div_ceil(BLOCK);

        if self.frame_width != fw || self.frame_height != fh {
            self.prev_gray = vec![0u8; w * h];
            self.mad_sum = vec![0.0f64; cols * rows];
            self.cols = cols;
            self.rows = rows;
            self.frame_width = fw;
            self.frame_height = fh;
        }

        for by in 0..rows {
            for bx in 0..cols {
                self.mad_sum[by * cols + bx] += block_mad(&gray, &self.prev_gray, w, bx, by) as f64;
            }
        }
        self.prev_gray = gray;
        self.frames += 1;
    }

    pub fn active_bbox(&self, threshold: f32) -> Option<[u32; 4]> {
        if self.frames == 0 {
            return None;
        }
        // Convert accumulated sum to per-frame mean for thresholding.
        let mean_map: Vec<f32> = self
            .mad_sum
            .iter()
            .map(|&s| (s / self.frames as f64) as f32)
            .collect();

        active_bbox(
            &mean_map,
            self.cols,
            self.rows,
            self.frame_width,
            self.frame_height,
            threshold,
        )
    }
}

// ── Async file analysis ──────────────────────────────────────────────────────

/// Spawn a background thread that decodes `path`, sampling every `skip`-th
/// frame, and sends back the bounding box of the most active region.
///
/// `skip = 1` analyses every frame; `skip = 4` is a reasonable default.
pub fn analyze_file_async(
    path: std::path::PathBuf,
    skip: usize,
    threshold: f32,
) -> Receiver<Option<[u32; 4]>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let result = run_analysis(&path, skip, threshold);
        let _ = tx.send(result);
    });
    rx
}

#[tracing::instrument(skip_all, fields(path = %path.display(), skip, threshold))]
fn run_analysis(path: &Path, skip: usize, threshold: f32) -> Option<[u32; 4]> {
    use ffmpeg::format::Pixel;
    use ffmpeg::media::Type;
    use ffmpeg::software::scaling::{context::Context as ScaleCtx, flag::Flags};
    use ffmpeg_next as ffmpeg;

    if let Err(e) = ffmpeg::init() {
        error!("FFmpeg init: {e}");
        return None;
    }

    let mut ictx = match ffmpeg::format::input(path) {
        Ok(c) => c,
        Err(e) => {
            error!("cannot open file: {e}");
            return None;
        }
    };

    let (stream_idx, tb, mut decoder) = {
        let streams = ictx.streams();
        let stream = match streams.best(Type::Video) {
            Some(s) => s,
            None => {
                error!("no video stream");
                return None;
            }
        };
        let idx = stream.index();
        let r = stream.time_base();
        let tb = r.0 as f64 / r.1 as f64;
        let ctx = match ffmpeg::codec::context::Context::from_parameters(stream.parameters()) {
            Ok(c) => c,
            Err(e) => {
                error!("codec context: {e}");
                return None;
            }
        };
        let dec = match ctx.decoder().video() {
            Ok(d) => d,
            Err(e) => {
                error!("video decoder: {e}");
                return None;
            }
        };
        (idx, tb, dec)
    };

    let mut scaler = match ScaleCtx::get(
        decoder.format(),
        decoder.width(),
        decoder.height(),
        Pixel::RGBA,
        decoder.width(),
        decoder.height(),
        Flags::BILINEAR,
    ) {
        Ok(s) => s,
        Err(e) => {
            error!("scaler: {e}");
            return None;
        }
    };

    let mut analyzer = FullVideoAnalyzer::new();
    let mut decoded = ffmpeg::util::frame::video::Video::empty();
    let mut rgba_frame = ffmpeg::util::frame::video::Video::empty();
    let mut frame_idx: u64 = 0;
    let skip = skip.max(1) as u64;

    let process = |decoded: &ffmpeg::util::frame::video::Video,
                   rgba_frame: &ffmpeg::util::frame::video::Video,
                   analyzer: &mut FullVideoAnalyzer,
                   tb: f64| {
        let pts_secs = decoded.pts().map(|p| p as f64 * tb).unwrap_or(0.0);
        let width = rgba_frame.width() as usize;
        let height = rgba_frame.height() as usize;
        let stride = rgba_frame.stride(0);
        let raw = rgba_frame.data(0);
        let mut rgba = Vec::with_capacity(width * height * 4);
        for row in 0..height {
            let start = row * stride;
            rgba.extend_from_slice(&raw[start..start + width * 4]);
        }
        analyzer.update(&VideoFrame {
            rgba,
            width: width as u32,
            height: height as u32,
            pts_secs,
            duration_secs: None,
        });
    };

    for (stream, packet) in ictx.packets() {
        if stream.index() != stream_idx {
            continue;
        }
        if decoder.send_packet(&packet).is_err() {
            continue;
        }
        while decoder.receive_frame(&mut decoded).is_ok() {
            if frame_idx.is_multiple_of(skip) && scaler.run(&decoded, &mut rgba_frame).is_ok() {
                process(&decoded, &rgba_frame, &mut analyzer, tb);
            }
            frame_idx += 1;
        }
    }

    let _ = decoder.send_eof();
    while decoder.receive_frame(&mut decoded).is_ok() {
        if frame_idx.is_multiple_of(skip) && scaler.run(&decoded, &mut rgba_frame).is_ok() {
            process(&decoded, &rgba_frame, &mut analyzer, tb);
        }
        frame_idx += 1;
    }

    let result = analyzer.active_bbox(threshold);
    info!(
        frames_decoded = frame_idx,
        frames_analysed = analyzer.frames,
        region = ?result,
        "full-video analysis done"
    );
    result
}

// ── Shared pixel helpers ─────────────────────────────────────────────────────

fn to_gray(rgba: &[u8]) -> Vec<u8> {
    rgba.chunks_exact(4)
        .map(|p| (0.299 * p[0] as f32 + 0.587 * p[1] as f32 + 0.114 * p[2] as f32).round() as u8)
        .collect()
}

fn block_mad(gray: &[u8], prev: &[u8], stride: usize, bx: usize, by: usize) -> f32 {
    let x0 = bx * BLOCK;
    let y0 = by * BLOCK;
    let x1 = (x0 + BLOCK).min(stride);
    let y1 = (y0 + BLOCK).min(prev.len() / stride);
    let n = ((x1 - x0) * (y1 - y0)) as f32;
    let mut sum = 0u32;
    for y in y0..y1 {
        for x in x0..x1 {
            let i = y * stride + x;
            sum += gray[i].abs_diff(prev[i]) as u32;
        }
    }
    sum as f32 / n
}

fn active_bbox(
    map: &[f32],
    cols: usize,
    rows: usize,
    fw: u32,
    fh: u32,
    threshold: f32,
) -> Option<[u32; 4]> {
    let w = fw as usize;
    let h = fh as usize;
    let mut min_col = cols;
    let mut max_col = 0usize;
    let mut min_row = rows;
    let mut max_row = 0usize;
    let mut found = false;

    for by in 0..rows {
        for bx in 0..cols {
            if map[by * cols + bx] >= threshold {
                min_col = min_col.min(bx);
                max_col = max_col.max(bx);
                min_row = min_row.min(by);
                max_row = max_row.max(by);
                found = true;
            }
        }
    }

    if !found {
        return None;
    }

    let px = (min_col * BLOCK) as u32;
    let py = (min_row * BLOCK) as u32;
    let pw = ((max_col + 1) * BLOCK).min(w) as u32 - px;
    let ph = ((max_row + 1) * BLOCK).min(h) as u32 - py;
    Some([px, py, pw, ph])
}
