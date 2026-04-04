use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, SyncSender};

use tracing::{debug, error, info, warn};

use crate::bbox::{Bbox, BboxMethod};

/// A single decoded video frame in packed RGBA format.
pub struct VideoFrame {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// Presentation timestamp in seconds.
    pub pts_secs: f64,
    /// Total duration of the source video in seconds.
    /// Only set on the **first** frame of each decode run; `None` on all others.
    /// `None` also when the container does not report a reliable duration.
    pub duration_secs: Option<f64>,
}

/// Decode every video frame from `path` and send them over `tx`.
/// Sends `None` to signal end-of-stream or an unrecoverable error.
/// Runs on a dedicated background thread.
#[tracing::instrument(skip(tx), fields(path = %path.display()))]
pub fn decode_video(path: PathBuf, tx: SyncSender<Option<VideoFrame>>) {
    use ffmpeg::format::Pixel;
    use ffmpeg::media::Type;
    use ffmpeg::software::scaling::{context::Context as ScaleCtx, flag::Flags};
    use ffmpeg_next as ffmpeg;

    macro_rules! bail {
        ($msg:expr) => {{
            error!("{}", $msg);
            let _ = tx.send(None);
            return;
        }};
    }

    if let Err(e) = ffmpeg::init() {
        bail!(format!("FFmpeg init failed: {e}"));
    }

    let mut ictx = match ffmpeg::format::input(&path) {
        Ok(ctx) => ctx,
        Err(e) => bail!(format!("cannot open file: {e}")),
    };

    info!(
        streams = ictx.nb_streams(),
        duration_s = ictx.duration() as f64 / f64::from(ffmpeg::ffi::AV_TIME_BASE),
        "opened input"
    );

    // Extract stream info and build the decoder before entering the packet loop
    // so we don't hold a borrow on `ictx` across it.
    let (stream_idx, tb, mut decoder) = {
        let streams = ictx.streams();
        let stream = match streams.best(Type::Video) {
            Some(s) => s,
            None => bail!("no video stream found"),
        };
        let idx = stream.index();
        let time_base = stream.time_base();
        let tb = time_base.0 as f64 / time_base.1 as f64;
        let ctx = match ffmpeg::codec::context::Context::from_parameters(stream.parameters()) {
            Ok(c) => c,
            Err(e) => bail!(format!("codec context error: {e}")),
        };
        let dec = match ctx.decoder().video() {
            Ok(d) => d,
            Err(e) => bail!(format!("video decoder error: {e}")),
        };
        (idx, tb, dec)
    };

    info!(
        stream_index = stream_idx,
        width = decoder.width(),
        height = decoder.height(),
        codec = ?decoder.id(),
        time_base_num = %ictx.stream(stream_idx).unwrap().time_base().0,
        time_base_den = %ictx.stream(stream_idx).unwrap().time_base().1,
        "video stream ready"
    );

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
        Err(e) => bail!(format!("scaler error: {e}")),
    };

    let container_duration = {
        let d = ictx.duration();
        if d > 0 {
            Some(d as f64 / f64::from(ffmpeg::ffi::AV_TIME_BASE))
        } else {
            None
        }
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

        let mut rgba = Vec::with_capacity(width * height * 4);
        for row in 0..height {
            let start = row * stride;
            rgba.extend_from_slice(&raw[start..start + width * 4]);
        }

        debug!(frame = *frame_idx, pts_secs, "decoded frame");
        let duration_secs = if *frame_idx == 0 {
            container_duration
        } else {
            None
        };
        *frame_idx += 1;

        tx.send(Some(VideoFrame {
            rgba,
            width: width as u32,
            height: height as u32,
            pts_secs,
            duration_secs,
        }))
        .is_ok()
    };

    for (stream, packet) in ictx.packets() {
        if stream.index() != stream_idx {
            continue;
        }
        if let Err(e) = decoder.send_packet(&packet) {
            warn!("send_packet error: {e}");
            continue;
        }
        while decoder.receive_frame(&mut decoded).is_ok() {
            if let Err(e) = scaler.run(&decoded, &mut rgba_frame) {
                warn!("scaler run error: {e}");
                continue;
            }
            if !send_frame(&decoded, &rgba_frame, &mut frame_idx, tb) {
                info!("receiver dropped, stopping decode");
                return;
            }
        }
    }

    // Flush buffered frames from the decoder.
    let _ = decoder.send_eof();
    while decoder.receive_frame(&mut decoded).is_ok() {
        if scaler.run(&decoded, &mut rgba_frame).is_ok()
            && !send_frame(&decoded, &rgba_frame, &mut frame_idx, tb)
        {
            info!("receiver dropped during flush, stopping");
            return;
        }
    }

    info!(total_frames = frame_idx, "decode finished");
    let _ = tx.send(None);
}

/// Controls how frames are sampled during analysis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnalysisMode {
    /// Decode every frame and sample at a fixed rate (`analysis_fps` frames per
    /// second of video time). Accurate but CPU-intensive.
    Full,
    /// Decode only I-frames (keyframes) by setting `skip_frame = NonIntra` on
    /// the decoder. Typically 10–60× faster than `Full`; accuracy depends on
    /// how often the source was keyframed (usually every 1–5 s).
    Fast,
}

/// Like [`decode_video`] but also runs [`crate::analysis::FullVideoAnalyzer`]
/// inline on the same thread. The analysis result is sent over the returned
/// channel immediately after the last display frame, so it arrives at the UI
/// with essentially zero extra lag after playback ends.
///
/// In [`AnalysisMode::Full`] every frame is decoded and sampled at `analysis_fps`
/// frames per second of video time. In [`AnalysisMode::Fast`] the decoder is
/// told to skip non-intra frames so only keyframes are decoded and analysed.
pub fn decode_video_with_analysis(
    path: PathBuf,
    display_tx: SyncSender<Option<VideoFrame>>,
    threshold: f32,
    analysis_fps: f32,
    mode: AnalysisMode,
    method: BboxMethod,
) -> Receiver<Option<Bbox>> {
    use crate::analysis::FullVideoAnalyzer;

    let (result_tx, result_rx) = mpsc::channel();

    std::thread::spawn(move || {
        use ffmpeg::codec::discard::Discard;
        use ffmpeg::format::Pixel;
        use ffmpeg::media::Type;
        use ffmpeg::software::scaling::{context::Context as ScaleCtx, flag::Flags};
        use ffmpeg_next as ffmpeg;

        macro_rules! bail {
            ($msg:expr) => {{
                error!("{}", $msg);
                let _ = display_tx.send(None);
                let _ = result_tx.send(None);
                return;
            }};
        }

        if let Err(e) = ffmpeg::init() {
            bail!(format!("FFmpeg init failed: {e}"));
        }

        let mut ictx = match ffmpeg::format::input(&path) {
            Ok(ctx) => ctx,
            Err(e) => bail!(format!("cannot open file: {e}")),
        };

        let (stream_idx, tb, mut decoder) = {
            let streams = ictx.streams();
            let stream = match streams.best(Type::Video) {
                Some(s) => s,
                None => bail!("no video stream found"),
            };
            let idx = stream.index();
            let time_base = stream.time_base();
            let tb = time_base.0 as f64 / time_base.1 as f64;
            let ctx = match ffmpeg::codec::context::Context::from_parameters(stream.parameters()) {
                Ok(c) => c,
                Err(e) => bail!(format!("codec context error: {e}")),
            };
            let mut dec = match ctx.decoder().video() {
                Ok(d) => d,
                Err(e) => bail!(format!("video decoder error: {e}")),
            };
            if mode == AnalysisMode::Fast {
                // Ask the codec to skip all non-intra (non-keyframe) frames.
                // The demuxer still delivers every packet but the decoder
                // silently drops B/P frames, emitting only I-frames.
                dec.skip_frame(Discard::NonIntra);
                info!("fast mode: decoder set to skip non-intra frames");
            }
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
            Err(e) => bail!(format!("scaler error: {e}")),
        };

        let container_duration = {
            let d = ictx.duration();
            if d > 0 {
                Some(d as f64 / f64::from(ffmpeg::ffi::AV_TIME_BASE))
            } else {
                None
            }
        };

        let mut analyzer = FullVideoAnalyzer::new();
        let mut decoded = ffmpeg::util::frame::video::Video::empty();
        let mut rgba_frame = ffmpeg::util::frame::video::Video::empty();
        let mut frame_idx: u64 = 0;
        // Full mode only: PTS of the last analysed frame.
        let mut last_analysis_pts: f64 = f64::NEG_INFINITY;
        let analysis_interval = 1.0 / analysis_fps.max(0.1) as f64;

        // Extract a contiguous Y (luma) plane from a decoded frame, de-striding
        // as necessary. Used by Fast mode to skip the sws_scale call entirely.
        let extract_y = |dec: &ffmpeg::util::frame::video::Video| -> Vec<u8> {
            let w = dec.width() as usize;
            let h = dec.height() as usize;
            let stride = dec.stride(0);
            let raw = dec.data(0);
            let mut y = Vec::with_capacity(w * h);
            for row in 0..h {
                let start = row * stride;
                y.extend_from_slice(&raw[start..start + w]);
            }
            y
        };

        // Build a VideoFrame from an already-scaled RGBA frame.
        // Sets duration_secs on the first frame only.
        let make_frame = |dec: &ffmpeg::util::frame::video::Video,
                          rgba: &ffmpeg::util::frame::video::Video,
                          frame_idx: &mut u64,
                          tb: f64|
         -> VideoFrame {
            let pts_secs = dec
                .pts()
                .map(|p| p as f64 * tb)
                .unwrap_or_else(|| *frame_idx as f64 / 30.0);
            let width = rgba.width() as usize;
            let height = rgba.height() as usize;
            let stride = rgba.stride(0);
            let raw = rgba.data(0);
            let mut rgba_buf = Vec::with_capacity(width * height * 4);
            for row in 0..height {
                let start = row * stride;
                rgba_buf.extend_from_slice(&raw[start..start + width * 4]);
            }
            debug!(frame = *frame_idx, pts_secs, "decoded frame");
            let duration_secs = if *frame_idx == 0 {
                container_duration
            } else {
                None
            };
            *frame_idx += 1;
            VideoFrame {
                rgba: rgba_buf,
                width: width as u32,
                height: height as u32,
                pts_secs,
                duration_secs,
            }
        };

        // Process one decoded frame: analyse it and try to send it for display.
        // Returns false when the display receiver has disconnected (caller should stop).
        macro_rules! process {
            ($dec:expr) => {{
                let pts_secs = $dec
                    .pts()
                    .map(|p| p as f64 * tb)
                    .unwrap_or_else(|| frame_idx as f64 / 30.0);

                if mode == AnalysisMode::Fast {
                    // Fast path: extract Y plane directly — no sws_scale for analysis.
                    // Every frame reaching here is a keyframe (decoder skip_frame=NonIntra).
                    let y = extract_y($dec);
                    analyzer.update_y(y, $dec.width(), $dec.height());
                }

                // Convert to RGBA for display (and for analysis in Full mode).
                if scaler.run($dec, &mut rgba_frame).is_err() {
                    frame_idx += 1;
                    continue; // skip this frame
                }
                let frame = make_frame($dec, &rgba_frame, &mut frame_idx, tb);

                if mode == AnalysisMode::Full && pts_secs - last_analysis_pts >= analysis_interval {
                    analyzer.update(&frame);
                    last_analysis_pts = pts_secs;
                }

                match display_tx.try_send(Some(frame)) {
                    Ok(()) | Err(mpsc::TrySendError::Full(_)) => true,
                    Err(mpsc::TrySendError::Disconnected(_)) => {
                        info!("display receiver dropped, stopping decode");
                        let _ = result_tx.send(analyzer.active_bbox(threshold, method));
                        false
                    }
                }
            }};
        }

        for (stream, packet) in ictx.packets() {
            if stream.index() != stream_idx {
                continue;
            }
            if let Err(e) = decoder.send_packet(&packet) {
                warn!("send_packet error: {e}");
                continue;
            }
            while decoder.receive_frame(&mut decoded).is_ok() {
                if !process!(&decoded) {
                    return;
                }
            }
        }

        let _ = decoder.send_eof();
        while decoder.receive_frame(&mut decoded).is_ok() {
            if !process!(&decoded) {
                return;
            }
        }

        let result = analyzer.active_bbox(threshold, method);
        info!(total_frames = frame_idx, region = ?result, "decode+analysis finished");
        let _ = result_tx.send(result);
        let _ = display_tx.send(None);
    });

    result_rx
}
