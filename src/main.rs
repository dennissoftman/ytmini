//! ytmini — a minimal personal YouTube client.
//! yt-dlp lists videos and handles auth (browser cookies); mpv plays them.

#![cfg_attr(windows, windows_subsystem = "windows")] // no console window on Windows

mod app;
mod cache;
mod deps;
mod mpv;
mod player;
mod positions;
mod settings;
mod signin;
mod theme;
mod thumbs;
mod tray;
mod yt;

fn main() -> eframe::Result {
    // A Finder-launched .app gets a bare PATH; libmpv's ytdl hook and yt-dlp's
    // JS runtime (deno) are found through PATH, so extend it before any threads start.
    yt::extend_process_path();

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("ytmini")
            .with_app_id("ytmini")
            .with_inner_size([1280.0, 820.0])
            .with_min_inner_size([520.0, 400.0])
            .with_icon(
                eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon.png"))
                    .unwrap_or_default(),
            ),
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };
    eframe::run_native(
        "ytmini",
        options,
        Box::new(|cc| Ok(Box::new(app::App::new(cc)))),
    )
}
