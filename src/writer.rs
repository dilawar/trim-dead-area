use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};

use tracing::{error, info, instrument};

/// Spawn a background thread that crops `input` to `region` and writes the
/// result to `output`. Every frame is included — no skipping.
///
/// The channel delivers `Ok(())` on success or `Err(message)` on failure.
pub fn crop_video_async(
    input: PathBuf,
    output: PathBuf,
    region: [u32; 4],
) -> Receiver<Result<(), String>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let result = crop_video(&input, &output, region);
        let _ = tx.send(result);
    });
    rx
}

/// Invoke the `ffmpeg` CLI to crop `input` to `region` and write `output`.
///
/// Uses `crop=w:h:x:y` so all frames are written — the filter does not drop
/// any frames, only the spatial extent is changed. Audio is copied as-is.
#[instrument(fields(
    input  = %input.display(),
    output = %output.display(),
    region = ?region,
))]
fn crop_video(input: &Path, output: &Path, region: [u32; 4]) -> Result<(), String> {
    let [x, y, w, h] = region;
    let input_str = input.to_str().ok_or("input path is not valid UTF-8")?;
    let output_str = output.to_str().ok_or("output path is not valid UTF-8")?;
    let filter = format!("crop={w}:{h}:{x}:{y}");

    info!("running ffmpeg crop");

    let status = std::process::Command::new("ffmpeg")
        .args([
            "-y",            // overwrite output without asking
            "-i", input_str,
            "-vf", &filter,
            "-c:a", "copy",  // copy audio track unchanged
            output_str,
        ])
        .status()
        .map_err(|e| {
            let msg = format!("could not start ffmpeg — is it installed? ({e})");
            error!("{msg}");
            msg
        })?;

    if status.success() {
        info!("crop complete");
        Ok(())
    } else {
        let msg = format!("ffmpeg exited with {status}");
        error!("{msg}");
        Err(msg)
    }
}
