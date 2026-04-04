use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};

use anyhow::{bail, Context as _, Result};
use ffmpeg::format::Pixel;
use crate::bbox::Bbox;
use ffmpeg::media::Type;
use ffmpeg::software::scaling::{context::Context as ScaleCtx, flag::Flags};
use ffmpeg::util::frame::video::Video as FfFrame;
use ffmpeg_next as ffmpeg;
use tracing::{error, info, instrument, warn};

/// Spawn a background thread that crops `input` to `region` and writes the
/// result to `output`. Every frame is included — no skipping.
///
/// If the `ffmpeg` binary is on `PATH` the CLI is used (simpler, format-aware).
/// Otherwise the `ffmpeg-next` crate is used, re-encoding with the same codec
/// as the input.
pub fn crop_video_async(
    input: PathBuf,
    output: PathBuf,
    region: Bbox,
) -> Receiver<Result<(), String>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let result = crop_video(&input, &output, region).map_err(|e| e.to_string());
        let _ = tx.send(result);
    });
    rx
}

fn crop_video(input: &Path, output: &Path, region: Bbox) -> Result<()> {
    if ffmpeg_cli_available() {
        info!("ffmpeg binary found — using CLI");
        crop_via_cli(input, output, region)
    } else {
        info!("ffmpeg binary not found — using crate");
        crop_via_crate(input, output, region)
    }
}

// ── Strategy 1: ffmpeg CLI ────────────────────────────────────────────────────

fn ffmpeg_cli_available() -> bool {
    std::process::Command::new("ffmpeg")
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[instrument(fields(
    input  = %input.display(),
    output = %output.display(),
    region = ?region,
))]
fn crop_via_cli(input: &Path, output: &Path, region: Bbox) -> Result<()> {
    let Bbox { x, y, w, h } = region;
    let input_str = input.to_str().context("input path is not valid UTF-8")?;
    let output_str = output.to_str().context("output path is not valid UTF-8")?;
    let filter = format!("crop={w}:{h}:{x}:{y}");

    info!("running ffmpeg crop");

    let status = std::process::Command::new("ffmpeg")
        .args([
            "-y", "-i", input_str, "-vf", &filter, "-c:a", "copy", output_str,
        ])
        .status()
        .context("could not start ffmpeg — is it installed?")?;

    if status.success() {
        info!("crop complete");
        Ok(())
    } else {
        let msg = format!("ffmpeg exited with {status}");
        error!("{msg}");
        bail!(msg)
    }
}

// ── Strategy 2: ffmpeg-next crate (same codec as input) ──────────────────────

#[instrument(fields(
    input  = %input.display(),
    output = %output.display(),
    region = ?region,
))]
fn crop_via_crate(input: &Path, output: &Path, region: Bbox) -> Result<()> {
    let Bbox { x: cx, y: cy, w: cw, h: ch } = region;

    ffmpeg::init().context("ffmpeg init")?;

    // ── Open input ────────────────────────────────────────────────────────────
    let mut ictx = ffmpeg::format::input(input).context("open input")?;

    let (vid_idx, in_tb, in_rate, codec_id, dec_format, dec_width, dec_height) = {
        let stream = ictx
            .streams()
            .best(Type::Video)
            .context("no video stream")?;
        let idx = stream.index();
        let tb = stream.time_base();
        let rate = stream.avg_frame_rate();
        let codec_id = stream.parameters().id();
        let dec_ctx = ffmpeg::codec::context::Context::from_parameters(stream.parameters())
            .context("decoder ctx")?
            .decoder()
            .video()
            .context("video decoder")?;
        let fmt = dec_ctx.format();
        let width = dec_ctx.width();
        let height = dec_ctx.height();
        (idx, tb, rate, codec_id, fmt, width, height)
    };

    let mut decoder = {
        let stream = ictx.streams().nth(vid_idx).context("video stream gone")?;
        ffmpeg::codec::context::Context::from_parameters(stream.parameters())
            .context("decoder ctx")?
            .decoder()
            .video()
            .context("video decoder")?
    };

    // ── Find encoder matching the input codec ─────────────────────────────────
    let enc_codec = ffmpeg::encoder::find(codec_id).with_context(|| {
        format!("no encoder found for {codec_id:?} — the input codec may be decode-only")
    })?;

    info!(codec = ?codec_id, width = cw, height = ch, "encoding crop with same codec as input");

    // ── Scalers ───────────────────────────────────────────────────────────────
    // Full-size native pixel format → full-size RGBA (allows row-based crop).
    let mut to_rgba = ScaleCtx::get(
        dec_format,
        dec_width,
        dec_height,
        Pixel::RGBA,
        dec_width,
        dec_height,
        Flags::BILINEAR,
    )
    .context("to_rgba scaler")?;

    // Cropped RGBA → encoder pixel format at the crop size.
    let mut to_enc = ScaleCtx::get(Pixel::RGBA, cw, ch, dec_format, cw, ch, Flags::BILINEAR)
        .context("to_enc scaler")?;

    // ── Open output ───────────────────────────────────────────────────────────
    let mut octx = ffmpeg::format::output(output).context("open output")?;

    // ── Add video stream with same codec ──────────────────────────────────────
    let out_vid_idx = octx
        .add_stream(enc_codec)
        .context("add video stream")?
        .index();

    let mut encoder = {
        let mut enc_ctx = ffmpeg::codec::context::Context::from_parameters(
            octx.stream(out_vid_idx).unwrap().parameters(),
        )
        .context("encoder ctx")?
        .encoder()
        .video()
        .context("video encoder")?;

        enc_ctx.set_format(dec_format);
        enc_ctx.set_width(cw);
        enc_ctx.set_height(ch);
        enc_ctx.set_time_base(in_tb);
        enc_ctx.set_frame_rate(Some(in_rate));

        let opened = enc_ctx.open_as(enc_codec).context("open encoder")?;
        octx.stream_mut(out_vid_idx)
            .unwrap()
            .set_parameters(&opened);
        opened
    };
    let out_vid_tb = encoder.time_base();

    // ── Copy audio streams ────────────────────────────────────────────────────
    // Collect (input_index, parameters) before mutably borrowing octx.
    let audio_pairs: Vec<(usize, ffmpeg::codec::Parameters)> = ictx
        .streams()
        .filter(|s| s.parameters().medium() == Type::Audio)
        .map(|s| (s.index(), s.parameters()))
        .collect();

    let audio_map: Vec<(usize, usize)> = audio_pairs
        .into_iter()
        .map(|(in_idx, params)| {
            let out_idx = octx
                .add_stream(ffmpeg::encoder::find(ffmpeg::codec::Id::None).unwrap())
                .expect("add audio stream")
                .index();
            octx.stream_mut(out_idx).unwrap().set_parameters(params);
            (in_idx, out_idx)
        })
        .collect();

    // ── Write header ──────────────────────────────────────────────────────────
    octx.write_header().context("write header")?;

    let mut rgba_full = FfFrame::empty();
    let mut enc_frame = FfFrame::empty();
    let mut decoded = FfFrame::empty();
    let mut pkt_out = ffmpeg::Packet::empty();

    // ── Packet loop ───────────────────────────────────────────────────────────
    for (stream, mut packet) in ictx.packets() {
        let in_idx = stream.index();

        // Audio: copy packets directly.
        if let Some(&(_, out_idx)) = audio_map.iter().find(|(i, _)| *i == in_idx) {
            let out_tb = octx.stream(out_idx).unwrap().time_base();
            packet.rescale_ts(stream.time_base(), out_tb);
            packet.set_stream(out_idx);
            packet.set_position(-1);
            if let Err(err) = packet.write_interleaved(&mut octx) {
                warn!("audio packet write: {err}");
            }
            continue;
        }

        if in_idx != vid_idx {
            continue;
        }

        if decoder.send_packet(&packet).is_err() {
            continue;
        }
        while decoder.receive_frame(&mut decoded).is_ok() {
            encode_cropped_frame(
                &decoded,
                &mut to_rgba,
                &mut to_enc,
                &mut rgba_full,
                &mut enc_frame,
                cx,
                cy,
                cw,
                ch,
                &mut encoder,
                &mut pkt_out,
                &mut octx,
                out_vid_idx,
                in_tb,
                out_vid_tb,
            )?;
        }
    }

    // Flush decoder
    decoder.send_eof().ok();
    while decoder.receive_frame(&mut decoded).is_ok() {
        encode_cropped_frame(
            &decoded,
            &mut to_rgba,
            &mut to_enc,
            &mut rgba_full,
            &mut enc_frame,
            cx,
            cy,
            cw,
            ch,
            &mut encoder,
            &mut pkt_out,
            &mut octx,
            out_vid_idx,
            in_tb,
            out_vid_tb,
        )
        .ok();
    }

    // Flush encoder
    encoder.send_eof().ok();
    drain_encoder(
        &mut encoder,
        &mut pkt_out,
        &mut octx,
        out_vid_idx,
        in_tb,
        out_vid_tb,
    );

    octx.write_trailer().context("write trailer")?;
    info!("crop complete");
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn encode_cropped_frame(
    decoded: &FfFrame,
    to_rgba: &mut ScaleCtx,
    to_enc: &mut ScaleCtx,
    rgba_full: &mut FfFrame,
    enc_frame: &mut FfFrame,
    cx: u32,
    cy: u32,
    cw: u32,
    ch: u32,
    encoder: &mut ffmpeg::encoder::video::Video,
    pkt_out: &mut ffmpeg::Packet,
    octx: &mut ffmpeg::format::context::Output,
    out_vid_idx: usize,
    in_tb: ffmpeg::Rational,
    out_vid_tb: ffmpeg::Rational,
) -> Result<()> {
    // Full-size native → full-size RGBA
    to_rgba.run(decoded, rgba_full).context("to_rgba")?;

    // Crop RGBA by copying only the sub-rectangle rows
    let mut cropped = FfFrame::new(Pixel::RGBA, cw, ch);
    {
        let src_stride = rgba_full.stride(0);
        let src = rgba_full.data(0);
        let dst_stride = cropped.stride(0);
        let dst = cropped.data_mut(0);
        for row in 0..ch as usize {
            let src_off = (cy as usize + row) * src_stride + cx as usize * 4;
            let dst_off = row * dst_stride;
            dst[dst_off..dst_off + cw as usize * 4]
                .copy_from_slice(&src[src_off..src_off + cw as usize * 4]);
        }
    }
    cropped.set_pts(decoded.pts());

    // Cropped RGBA → encoder pixel format
    to_enc.run(&cropped, enc_frame).context("to_enc")?;
    enc_frame.set_pts(decoded.pts());

    encoder.send_frame(enc_frame).context("send frame")?;
    drain_encoder(encoder, pkt_out, octx, out_vid_idx, in_tb, out_vid_tb);
    Ok(())
}

fn drain_encoder(
    encoder: &mut ffmpeg::encoder::video::Video,
    pkt: &mut ffmpeg::Packet,
    octx: &mut ffmpeg::format::context::Output,
    out_vid_idx: usize,
    in_tb: ffmpeg::Rational,
    out_tb: ffmpeg::Rational,
) {
    while encoder.receive_packet(pkt).is_ok() {
        pkt.set_stream(out_vid_idx);
        pkt.rescale_ts(in_tb, out_tb);
        pkt.set_position(-1);
        pkt.write_interleaved(octx).ok();
    }
}
