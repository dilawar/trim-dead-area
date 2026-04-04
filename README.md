# trim-dead-area

A video player that automatically detects and removes the static borders and
letterboxing from a video — regions that never change — and crops the output
file to the area that actually has content.

## Demo

[![Demo](https://img.youtube.com/vi/tFr4RkUZhgo/0.jpg)](https://www.youtube.com/watch?v=tFr4RkUZhgo)

## How it works

1. **Open a video.** The first frame is shown immediately as a preview.
2. **Click Go.** A single decode pass begins, running two analyses in
   lockstep on the same thread:
   - A *real-time* analyser computes a per-block motion score (EMA of
     mean-absolute-difference) for every displayed frame and draws a
     **yellow** bounding box around the live active region.
   - Every 4th frame is fed to a *full-video* accumulator that computes
     the mean MAD across the whole file for a more stable estimate.
     When playback ends, a **cyan** bounding box appears.
3. **When playback ends** the crop dialog opens automatically, showing the
   most active region found by the full-video analysis.
4. **Save the cropped video.** Confirm the dialog to choose an output path
   (pre-filled as `<original>_cropped.<ext>`). `ffmpeg` re-encodes the
   video with a `crop` filter; every frame is written, none are skipped.

## Installation

### Pre-built binaries

Download the latest release for your platform from the
[Releases](../../releases) page:

| Platform | Archive |
|---|---|
| Linux x86\_64 | `trim-dead-area-linux-x86_64.tar.gz` |
| macOS (Apple Silicon) | `trim-dead-area-macos.tar.gz` |
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

## Usage

### Opening a file

- Click **Open File** and choose a video, or
- **Drag and drop** a video file onto the window, or
- Pass the path as a command-line argument: `trim-dead-area video.mp4`

Supported formats: anything FFmpeg can decode (MP4, MKV, AVI, MOV, WebM,
FLV, WMV, TS, M4V, …).

### Controls

| Control | Action |
|---|---|
| **Go** | Start (or restart) analysis from the beginning |
| **Open File** | Load a new video |
| Drag & drop | Load a new video |

The time display in the bottom-right shows the PTS of the last displayed frame.

### Motion threshold slider

```
Motion threshold:  ──●────────  5.0 MAD   (raise to ignore camera shake or compression noise)
```

Controls which blocks are considered "active":

- **Lower** — more of the frame is included (looser definition of activity).
- **Higher** — only regions with strong frame-to-frame change are included
  (tighter crop, may miss slow-moving content).

The default of **5.0 MAD** (mean absolute difference, in 8-bit intensity
units) works well for most screen recordings and lecture videos.

If you adjust the slider while analysis is running, a prompt will ask whether
to restart with the new value.

### Overlays

| Colour | Meaning |
|---|---|
| **Yellow** | Live bounding box from the real-time EMA analyser — updates every displayed frame. |
| **Cyan** | Stable bounding box from the full-video analysis — appears once playback ends. |

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
3. A spinner shows while `ffmpeg` runs. The output path is shown when done.

> The output is produced by `ffmpeg -vf crop=w:h:x:y -c:a copy`, so the
> audio track is copied without re-encoding and video quality is determined
> by the encoder defaults (libx264 for MP4).

## License

[GNU Lesser General Public License v3.0](LICENSE)
