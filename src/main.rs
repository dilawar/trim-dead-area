use std::path::PathBuf;

use clap::Parser;
use tracing::info;
use trim_dead_area::app::App;
use trim_dead_area::bbox::BboxMethod;

/// Detect and crop static dead borders from a video file.
///
/// Open a video in the GUI, press Go, and the app will analyse motion
/// across the full video and offer to crop it to the active region.
/// You can also drag-and-drop a file onto the window at any time.
#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    /// Video file to open on launch (MP4, MKV, MOV, WebM, …).
    file: Option<PathBuf>,

    /// Frames per second of video time sampled during Full analysis (1–30).
    ///
    /// Only used when analysis mode is Full. Higher values are more accurate
    /// but slower. Ignored in Fast mode (I-frames only).
    #[arg(short = 'r', long, value_name = "FPS", default_value_t = 6.0,
          value_parser = parse_analysis_fps)]
    analysis_fps: f32,

    /// Use Fast analysis mode (decode I-frames only).
    ///
    /// Typically 10–60× faster than Full mode. Accuracy depends on how
    /// frequently the source video was keyframed (usually every 1–5 s).
    #[arg(short, long)]
    fast: bool,

    /// Bounding-box computation method (advanced).
    ///
    /// Controls how the per-block motion scores are reduced to a single crop
    /// rectangle. Useful when isolated noise blocks distort the result.
    ///
    /// Values:
    ///   union              — tight envelope of every active block (default)
    ///   percentile:<P>     — trim P% from each edge (e.g. percentile:5)
    ///   density-filter:<N> — require ≥ N active blocks per row/col (e.g. density-filter:2)
    ///   erosion:<N>        — require ≥ N active 4-neighbours (e.g. erosion:1)
    #[arg(long, value_name = "METHOD", default_value = "union")]
    bbox_method: BboxMethod,
}

fn parse_analysis_fps(s: &str) -> Result<f32, String> {
    let n: f32 = s
        .parse()
        .map_err(|_| format!("'{s}' is not a valid number"))?;
    if !(1.0..=30.0).contains(&n) {
        return Err(format!("{n} is out of range (1–30)"));
    }
    Ok(n)
}


fn main() -> eframe::Result {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    info!("starting trim-dead-area v{}", env!("CARGO_PKG_VERSION"));

    let native_options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("Trim Dead Area")
            .with_inner_size([960.0, 640.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native(
        "Trim Dead Area",
        native_options,
        Box::new(move |cc| {
            Ok(Box::new(App::new(
                cc,
                cli.file,
                cli.analysis_fps,
                cli.fast,
                cli.bbox_method,
            )))
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_fps_valid() {
        assert_eq!(parse_analysis_fps("6"), Ok(6.0));
    }

    #[test]
    fn test_parse_fps_boundary_low() {
        assert_eq!(parse_analysis_fps("1"), Ok(1.0));
    }

    #[test]
    fn test_parse_fps_boundary_high() {
        assert_eq!(parse_analysis_fps("30"), Ok(30.0));
    }

    #[test]
    fn test_parse_fps_below_range() {
        assert!(parse_analysis_fps("0").is_err());
    }

    #[test]
    fn test_parse_fps_above_range() {
        assert!(parse_analysis_fps("31").is_err());
    }

    #[test]
    fn test_parse_fps_not_a_number() {
        assert!(parse_analysis_fps("abc").is_err());
    }
}
