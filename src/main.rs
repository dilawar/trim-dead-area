use std::path::PathBuf;

use tracing::info;
use trim_dead_area::app::App;

fn main() -> eframe::Result {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    info!("starting trim-dead-area v{}", env!("CARGO_PKG_VERSION"));

    // Simple hand-rolled arg parsing: [--analysis-fps <N>] [<video>]
    let mut initial_file: Option<PathBuf> = None;
    let mut analysis_fps: f32 = 6.0;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--analysis-fps" | "-r" => {
                if let Some(val) = args.next() {
                    if let Ok(n) = val.parse::<f32>() {
                        analysis_fps = n.clamp(1.0, 30.0);
                    } else {
                        eprintln!("trim-dead-area: invalid --analysis-fps value: {val}");
                    }
                }
            }
            _ => {
                initial_file = Some(PathBuf::from(arg));
            }
        }
    }

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
        Box::new(move |cc| Ok(Box::new(App::new(cc, initial_file, analysis_fps)))),
    )
}
