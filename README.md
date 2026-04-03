# trim-dead-area

A video player that automatically detects and removes the static borders and
letterboxing from a video — regions that never change — and crops the output
file to the area that actually has content.

---

## Demo

| | |
|---|---|
| **Input** | [YouTube Short](https://youtube.com/shorts/X9FQ7-t45hU?si=BQCwINot3tXfxBkf) — original video with static black borders |
| **Output** | [YouTube](https://youtu.be/5WyGapgk64A) — cropped result produced by trim-dead-area |

---

## How it works

1. **Open a video.** The player starts immediately and runs through the frames
   as fast as possible (no real-time throttle).
2. **Two analyses run in parallel:**
   - A *real-time* analyser computes a per-block motion score (EMA of
     mean-absolute-difference) for every displayed frame and draws a
     **yellow** bounding box around the live active region.
   - A *background* pass decodes every 4th frame and accumulates the
     mean MAD across the whole file, producing a more stable estimate.
     When it finishes, a **cyan** bounding box appears.
3. **When playback ends** the crop dialog opens automatically, showing the
   most active region found by the full-video analysis.
4. **Save the cropped video.** Confirm the dialog to choose an output path
   (pre-filled as `<original>_cropped.<ext>`). `ffmpeg` re-encodes the
   video with a `crop` filter; every frame is written, none are skipped.

---

## Installation

### Pre-built binaries

Download the latest release for your platform from the
[Releases](../../releases) page:

| Platform | Archive |
|---|---|
| Linux x86\_64 | `trim-dead-area-linux-x86_64.tar.gz` |
| macOS (universal) | `trim-dead-area-macos-universal.tar.gz` |
| Windows x86\_64 | `trim-dead-area-windows-x86_64.zip` (includes FFmpeg DLLs) |

### Build from source

**Prerequisites**

| Dependency | Linux | macOS | Windows |
|---|---|---|---|
| Rust (stable) | `rustup` | `rustup` | `rustup` |
| FFmpeg dev libs | `libavcodec-dev libavformat-dev libavutil-dev libavfilter-dev libswscale-dev libswresample-dev` | `brew install ffmpeg` | [gyan.dev shared build](https://www.gyan.dev/ffmpeg/builds/) — set `FFMPEG_DIR` |
| Clang | `clang` | bundled with Xcode | Visual Studio LLVM tools |
| GTK3 (file dialog) | `libgtk-3-dev` | — | — |

```sh
git clone https://github.com/dilawar/trim-dead-area
cd trim-dead-area
cargo build --release
./target/release/trim-dead-area
```

On Windows, add the FFmpeg `bin/` directory to `PATH` before running.

### Runtime dependency

The **crop export** feature calls `ffmpeg` from your `PATH`. Install it with
your package manager if it is not already present:

```sh
# Debian / Ubuntu
sudo apt install ffmpeg

# macOS
brew install ffmpeg

# Windows — download from https://www.gyan.dev/ffmpeg/builds/
# and add the bin/ folder to your PATH
```

---

## Usage

### Opening a file

- Click **Open File** and choose a video, or
- **Drag and drop** a video file onto the window.

Supported formats: anything FFmpeg can decode (MP4, MKV, AVI, MOV, WebM,
FLV, WMV, TS, M4V, …).

### Playback controls

| Control | Action |
|---|---|
| **▶ Play / ⏸ Pause** | Toggle playback |
| **Open File** | Load a new video |
| Drag & drop | Load a new video |

The time display in the bottom-right shows the PTS of the last displayed frame.

### Motion threshold slider

```
Motion threshold:  ──●────────  5.0 MAD
```

Controls which blocks are considered "active":

- **Lower** — more of the frame is included (looser definition of activity).
- **Higher** — only regions with strong frame-to-frame change are included
  (tighter crop, may miss slow-moving content).

The default of **5.0 MAD** (mean absolute difference, in 8-bit intensity
units) works well for most screen recordings and lecture videos.

### Overlays

| Colour | Meaning |
|---|---|
| **Yellow** | Live bounding box from the real-time EMA analyser — updates every displayed frame. |
| **Cyan** | Stable bounding box from the full-video background analysis — appears once the background pass finishes. |

### Crop-to-active-region (live preview)

Tick **Crop to active region** to preview the display with the dead borders
removed in real time. This does not write any file; it only affects what you
see on screen.

### Exporting the cropped video

When playback finishes the **Active Region Detected** dialog opens:

```
┌─────────────────────────────────────┐
│  Active Region Detected             │
│  The most active region:            │
│    1280 × 720  at  (0, 140)         │
│                                     │
│  Crop the video to this rectangle?  │
│  All frames are written — none      │
│  are skipped.                       │
│                                     │
│  [Save Cropped Video…]  [Dismiss]   │
└─────────────────────────────────────┘
```

1. Click **Save Cropped Video…** — a save dialog opens pre-filled with
   `<original-name>_cropped.<ext>`.
2. Choose a location and confirm.
3. A spinner shows while `ffmpeg` runs.  The output path is shown when done.

> The output is produced by `ffmpeg -vf crop=w:h:x:y -c:a copy`, so the
> audio track is copied without re-encoding and video quality is determined
> by the encoder defaults (libx264 for MP4).

---

## Logging

The application uses [`tracing`](https://docs.rs/tracing) for structured
logging. Set the `RUST_LOG` environment variable to control verbosity:

```sh
# Default (info and above)
./trim-dead-area

# Show per-frame detail and region changes
RUST_LOG=debug ./trim-dead-area

# Show per-block MAD/EMA values (very verbose)
RUST_LOG=trace ./trim-dead-area

# Per-module control
RUST_LOG=trim_dead_area::decoder=debug,trim_dead_area::analysis=warn ./trim-dead-area
```

---

## Project structure

```
src/
├── main.rs       Entry point; initialises the tracing subscriber.
├── lib.rs        Module declarations.
├── app.rs        egui/eframe application — UI, playback loop, dialog state.
├── decoder.rs    FFmpeg video decoder; runs on a background thread.
├── analysis.rs   Real-time (EMA) and full-video (mean MAD) motion analysers.
└── writer.rs     Invokes ffmpeg to write the cropped output file.
```

---

## License

MIT
