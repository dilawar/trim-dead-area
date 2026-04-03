use std::path::PathBuf;
use std::sync::mpsc::SyncSender;

/// A single decoded video frame in packed RGBA format.
pub struct VideoFrame {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// Presentation timestamp in seconds.
    pub pts_secs: f64,
}

/// Decode every video frame from `path` and send them over `tx`.
/// Sends `None` to signal end-of-stream or an unrecoverable error.
/// Runs on a dedicated background thread.
pub fn decode_video(path: PathBuf, tx: SyncSender<Option<VideoFrame>>) {
    use ffmpeg_next as ffmpeg;
    use ffmpeg::format::Pixel;
    use ffmpeg::media::Type;
    use ffmpeg::software::scaling::{context::Context as ScaleCtx, flag::Flags};

    macro_rules! bail {
        ($msg:expr) => {{
            eprintln!("{}", $msg);
            let _ = tx.send(None);
            return;
        }};
    }

    if let Err(e) = ffmpeg::init() {
        bail!(format!("FFmpeg init failed: {e}"));
    }

    let mut ictx = match ffmpeg::format::input(&path) {
        Ok(ctx) => ctx,
        Err(e) => bail!(format!("Cannot open '{}': {e}", path.display())),
    };

    // Extract stream info and build the decoder before entering the packet loop
    // so we don't hold a borrow on `ictx` across it.
    let (stream_idx, tb, mut decoder) = {
        let streams = ictx.streams();
        let stream = match streams.best(Type::Video) {
            Some(s) => s,
            None => bail!(format!("No video stream in '{}'", path.display())),
        };
        let idx = stream.index();
        let time_base = stream.time_base();
        let tb = time_base.0 as f64 / time_base.1 as f64;
        let ctx = match ffmpeg::codec::context::Context::from_parameters(stream.parameters()) {
            Ok(c) => c,
            Err(e) => bail!(format!("Codec context error: {e}")),
        };
        let dec = match ctx.decoder().video() {
            Ok(d) => d,
            Err(e) => bail!(format!("Video decoder error: {e}")),
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
        Err(e) => bail!(format!("Scaler error: {e}")),
    };

    let mut decoded = ffmpeg::util::frame::video::Video::empty();
    let mut rgba_frame = ffmpeg::util::frame::video::Video::empty();
    let mut frame_idx: u64 = 0;

    let send_frame = |decoded: &ffmpeg::util::frame::video::Video,
                      rgba_frame: &ffmpeg::util::frame::video::Video,
                      frame_idx: &mut u64,
                      tb: f64|
     -> bool {
        let pts_secs = decoded
            .pts()
            .map(|p| p as f64 * tb)
            .unwrap_or_else(|| *frame_idx as f64 / 30.0);

        let width = rgba_frame.width() as usize;
        let height = rgba_frame.height() as usize;
        let stride = rgba_frame.stride(0);
        let raw = rgba_frame.data(0);

        // Copy each row, stripping any stride padding added by the scaler.
        let mut rgba = Vec::with_capacity(width * height * 4);
        for row in 0..height {
            let start = row * stride;
            rgba.extend_from_slice(&raw[start..start + width * 4]);
        }

        *frame_idx += 1;
        tx.send(Some(VideoFrame {
            rgba,
            width: width as u32,
            height: height as u32,
            pts_secs,
        }))
        .is_ok()
    };

    for (stream, packet) in ictx.packets() {
        if stream.index() != stream_idx {
            continue;
        }
        if decoder.send_packet(&packet).is_err() {
            continue;
        }
        while decoder.receive_frame(&mut decoded).is_ok() {
            if scaler.run(&decoded, &mut rgba_frame).is_err() {
                continue;
            }
            if !send_frame(&decoded, &rgba_frame, &mut frame_idx, tb) {
                return; // UI window was closed.
            }
        }
    }

    // Flush buffered frames from the decoder.
    let _ = decoder.send_eof();
    while decoder.receive_frame(&mut decoded).is_ok() {
        if scaler.run(&decoded, &mut rgba_frame).is_ok() {
            if !send_frame(&decoded, &rgba_frame, &mut frame_idx, tb) {
                return;
            }
        }
    }

    let _ = tx.send(None);
}
