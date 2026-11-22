//! ytmini — a minimal desktop YouTube client.
//! Lists and account actions go through YouTube's web API with the session
//! imported from your browser; stream links come from our own lookup (yt-dlp
//! as the fallback); libmpv plays them.

#![cfg_attr(windows, windows_subsystem = "windows")] // no console window on Windows

mod app;
mod awake;
mod cache;
mod chapters;
mod crashlog;
mod dearrow;
mod deps;
mod downloads;
mod eq;
mod i18n;
mod innertube;
mod links;
mod mpv;
mod native;
mod nowplaying;
mod player;
mod positions;
mod resolver;
mod settings;
mod signin;
mod sponsorblock;
mod storyboard;
mod textlinks;
mod theme;
mod thumbs;
mod tray;
mod yt;

fn main() -> eframe::Result {
    crashlog::install();
    // `--crash-test`: check the crash log (dev)
    if std::env::args().any(|a| a == "--crash-test") {
        panic!("crash test (asked for with --crash-test)");
    }
    // A Finder-launched .app gets a bare PATH; libmpv's ytdl hook and yt-dlp's
    // JS runtime (deno) are found through PATH, so extend it before any threads start.
    yt::extend_process_path();
    yt::tidy_cookie_storage();
    // `--check-native [ids…]`: test our own stream lookup and exit
    // (scripts/check_native.sh)
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--check-native") {
        std::process::exit(native::self_check(&args[1..]));
    }

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
