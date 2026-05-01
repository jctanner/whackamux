mod app;
mod config;
mod mock;
mod pane;
mod ssh;
mod tmux;

use config::Config;

fn main() -> anyhow::Result<()> {
    env_logger::init();

    let config = if std::path::Path::new("config.yaml").exists() {
        Config::load("config.yaml")?
    } else {
        log::info!("No config.yaml found, using defaults");
        serde_yaml::from_str(
            "hosts:\n  - name: localhost\n    local: true\n",
        )?
    };

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1200.0, 800.0])
            .with_maximized(true)
            .with_title("whackamux"),
        ..Default::default()
    };

    eframe::run_native(
        "whackamux",
        options,
        Box::new(move |cc| Ok(Box::new(app::App::new(cc, config)))),
    )
    .map_err(|e| anyhow::anyhow!("eframe error: {}", e))
}
