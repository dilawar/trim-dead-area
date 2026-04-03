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
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    )
}
