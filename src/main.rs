#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod avatar;
mod demo;
mod ffmpeg;
mod mic;
mod net;
mod proto;
mod ptt;
mod sounds;
mod update;
mod ui;
mod video;
mod voice;

fn main() -> eframe::Result {
    tracing_subscriber::fmt()
        .with_max_level(if std::env::var_os("BIBICORD_DEBUG").is_some() {
            tracing::Level::DEBUG
        } else {
            tracing::Level::INFO
        })
        .init();

    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let handle = rt.handle().clone();

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("BibiCord")
            .with_inner_size([1120.0, 720.0])
            .with_min_inner_size([760.0, 480.0])
            .with_app_id("dev.bibicord.app")
            .with_icon(
                eframe::icon_data::from_png_bytes(include_bytes!("../packaging/icon/icon-256.png"))
                    .expect("app icon"),
            ),
        ..Default::default()
    };
    let res = eframe::run_native(
        "BibiCord",
        options,
        Box::new(move |cc| Ok(Box::new(ui::App::new(cc, handle)))),
    );
    rt.shutdown_timeout(std::time::Duration::from_millis(500));
    res
}
