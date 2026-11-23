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
mod pgpsig;
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
    // `--check-solver`: the session-free part of it (daily CI)
    if args.first().map(String::as_str) == Some("--check-solver") {
        std::process::exit(native::check_solver());
    }

    #[cfg(target_os = "linux")]
    prefer_x11_where_wayland_breaks();

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
    let r = eframe::run_native(
        "ytmini",
        options,
        Box::new(|cc| Ok(Box::new(app::App::new(cc)))),
    );
    #[cfg(target_os = "linux")]
    if let Err(e) = &r {
        retry_on_x11(e);
    }
    r
}

/// WSLg's Wayland compositor drops winit's connection while the window is
/// set up ("Connection reset by peer"); its X server (XWayland) works. In WSL,
/// or with YTMINI_X11=1, hide Wayland so winit takes X11.
#[cfg(target_os = "linux")]
fn prefer_x11_where_wayland_breaks() {
    let wsl = std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .is_ok_and(|s| s.to_lowercase().contains("microsoft"));
    if (wsl || std::env::var_os("YTMINI_X11").is_some()) && x11_reachable() {
        // before any thread starts
        std::env::remove_var("WAYLAND_DISPLAY");
        std::env::remove_var("WAYLAND_SOCKET");
    }
}

/// DISPLAY names an X server we can reach: a local one whose socket exists
/// (WSL with systemd can lose /tmp/.X11-unix while Wayland still works), or
/// a remote one.
#[cfg(target_os = "linux")]
fn x11_reachable() -> bool {
    let Some(d) = std::env::var_os("DISPLAY") else {
        return false;
    };
    let d = d.to_string_lossy();
    match d.strip_prefix(':') {
        Some(n) => {
            let n = n.split('.').next().unwrap_or("0");
            std::path::Path::new(&format!("/tmp/.X11-unix/X{n}")).exists()
        }
        None => !d.is_empty(),
    }
}

/// Wayland failed to open the window elsewhere: start once more on X11 (a
/// process can create only one event loop, so in a new process).
#[cfg(target_os = "linux")]
fn retry_on_x11(e: &eframe::Error) {
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    if !wayland
        || !x11_reachable()
        || std::env::var_os("YTMINI_X11").is_some()
        || !format!("{e:?}").contains("Wayland")
    {
        return;
    }
    eprintln!("ytmini: Wayland failed ({e}); trying X11");
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let status = std::process::Command::new(exe)
        .args(std::env::args_os().skip(1))
        .env("YTMINI_X11", "1")
        .status();
    if let Ok(s) = status {
        std::process::exit(s.code().unwrap_or(1));
    }
}
