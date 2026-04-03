use tracing::{debug, trace};

use crate::decoder::VideoFrame;

/// Side length (in pixels) of each analysis block.
const BLOCK: usize = 16;

/// Tracks per-block inter-frame motion via an exponential moving average of
/// the mean absolute difference (MAD) between consecutive grayscale frames.
pub struct MotionAnalyzer {
    prev_gray: Vec<u8>,
    /// EMA of per-block MAD, in the same units as pixel intensity (0–255).
    motion_map: Vec<f32>,
    pub frame_width: u32,
    pub frame_height: u32,
    cols: usize,
    rows: usize,
}

impl Default for MotionAnalyzer {
    fn default() -> Self {
        Self {
            prev_gray: Vec::new(),
            motion_map: Vec::new(),
            frame_width: 0,
            frame_height: 0,
            cols: 0,
            rows: 0,
        }
    }
}

impl MotionAnalyzer {
    pub fn reset(&mut self) {
        debug!("motion analyzer reset");
        *self = Self::default();
    }

    /// Feed the next displayed frame.
    ///
    /// Returns the smallest bounding box `[x, y, w, h]` (pixel coords) that
    /// covers every block whose EMA motion score ≥ `threshold`.
    /// Returns `None` while the EMA is warming up or when every block is below
    /// the threshold.
    pub fn update(&mut self, frame: &VideoFrame, threshold: f32) -> Option<[u32; 4]> {
        let w = frame.width as usize;
        let h = frame.height as usize;
        let cols = (w + BLOCK - 1) / BLOCK;
        let rows = (h + BLOCK - 1) / BLOCK;

        // Re-initialise if this is the first frame or if resolution changed.
        if self.frame_width != frame.width || self.frame_height != frame.height {
            debug!(
                width = w,
                height = h,
                cols,
                rows,
                block_size = BLOCK,
                "motion analyzer initialised"
            );
            self.prev_gray = vec![0u8; w * h];
            self.motion_map = vec![0.0f32; cols * rows];
            self.cols = cols;
            self.rows = rows;
            self.frame_width = frame.width;
            self.frame_height = frame.height;
        }

        // RGBA → grayscale (BT.601 luma).
        let gray: Vec<u8> = frame
            .rgba
            .chunks_exact(4)
            .map(|p| {
                (0.299 * p[0] as f32 + 0.587 * p[1] as f32 + 0.114 * p[2] as f32).round() as u8
            })
            .collect();

        // Update EMA of mean absolute difference per block.
        for by in 0..rows {
            for bx in 0..cols {
                let x0 = bx * BLOCK;
                let y0 = by * BLOCK;
                let x1 = (x0 + BLOCK).min(w);
                let y1 = (y0 + BLOCK).min(h);
                let n = ((x1 - x0) * (y1 - y0)) as f32;

                let mut sum = 0u32;
                for y in y0..y1 {
                    for x in x0..x1 {
                        let i = y * w + x;
                        sum += gray[i].abs_diff(self.prev_gray[i]) as u32;
                    }
                }

                let slot = &mut self.motion_map[by * cols + bx];
                let mad = sum as f32 / n;
                trace!(by, bx, mad, ema = *slot, "block update");
                *slot = 0.95 * *slot + 0.05 * mad;
            }
        }
        self.prev_gray = gray;

        self.active_bbox(threshold)
    }

    /// Smallest axis-aligned bounding box (pixel coords) of all blocks with
    /// motion score ≥ `threshold`.
    fn active_bbox(&self, threshold: f32) -> Option<[u32; 4]> {
        let w = self.frame_width as usize;
        let h = self.frame_height as usize;

        let mut min_col = self.cols;
        let mut max_col = 0usize;
        let mut min_row = self.rows;
        let mut max_row = 0usize;
        let mut found = false;

        for by in 0..self.rows {
            for bx in 0..self.cols {
                if self.motion_map[by * self.cols + bx] >= threshold {
                    min_col = min_col.min(bx);
                    max_col = max_col.max(bx);
                    min_row = min_row.min(by);
                    max_row = max_row.max(by);
                    found = true;
                }
            }
        }

        if !found {
            trace!(threshold, "no active blocks above threshold");
            return None;
        }

        let px = (min_col * BLOCK) as u32;
        let py = (min_row * BLOCK) as u32;
        let pw = ((max_col + 1) * BLOCK).min(w) as u32 - px;
        let ph = ((max_row + 1) * BLOCK).min(h) as u32 - py;

        trace!(px, py, pw, ph, threshold, "active bbox");
        Some([px, py, pw, ph])
    }
}
