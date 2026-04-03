use std::path::Path;

use image::{GrayImage, Luma};

use crate::decoder::VideoFrame;

/// Convert an RGBA `frame` to grayscale (BT.601 luma) and write it as a PNG.
/// The file is named `frame_NNNNNN.png` inside `dir`.
pub fn save_bw_frame(frame: &VideoFrame, dir: &Path, count: u64) {
    let mut gray = GrayImage::new(frame.width, frame.height);

    for (i, pixel) in gray.pixels_mut().enumerate() {
        let base = i * 4;
        let r = frame.rgba[base] as f32;
        let g = frame.rgba[base + 1] as f32;
        let b = frame.rgba[base + 2] as f32;
        let luma = (0.299 * r + 0.587 * g + 0.114 * b).round() as u8;
        *pixel = Luma([luma]);
    }

    let path = dir.join(format!("frame_{count:06}.png"));
    if let Err(e) = gray.save(&path) {
        eprintln!("Failed to save frame {count}: {e}");
    }
}
