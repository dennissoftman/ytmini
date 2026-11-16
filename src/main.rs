//! ytmini — minimal YouTube client: yt-dlp for listing/auth, mpv (ffmpeg inside) for playback.
//!
//! On first run it asks which browser to import your YouTube login cookies from,
//! saves them to a private cookies.txt, then gives you feed / search / subs / etc.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

const LIST_LEN: usize = 30;
const FORMAT: &str = "bv*[height<=1080]+ba/b";

/// (display name, yt-dlp browser id, macOS app bundle names to detect it)
const BROWSERS: &[(&str, &str, &[&str])] = &[
    ("Safari", "safari", &["Safari.app"]),
    ("Chrome", "chrome", &["Google Chrome.app"]),
    ("Firefox", "firefox", &["Firefox.app"]),
    ("Brave", "brave", &["Brave Browser.app"]),
    ("Edge", "edge", &["Microsoft Edge.app"]),
    ("Chromium", "chromium", &["Chromium.app"]),
    ("Opera", "opera", &["Opera.app"]),
    ("Vivaldi", "vivaldi", &["Vivaldi.app"]),
];

#[derive(Deserialize)]
struct Video {
    id: String,
    title: Option<String>,
    channel: Option<String>,
    uploader: Option<String>,
    duration: Option<f64>,
}

fn main() -> Result<()> {
    check_deps()?;

    let dir = dirs::config_dir().context("no config dir")?.join("ytmini");
    fs::create_dir_all(&dir)?;
    let cookies = dir.join("cookies.txt");

    let reimport = std::env::args().any(|a| a == "--reimport");
    if reimport || !logged_in(&cookies) {
        import_cookies(&cookies)?;
    }

    loop {
        let cmd = prompt(
            "\n[f]eed  [s]earch  s[u]bs  [w]atch later  [h]istory  [r]e-import cookies  [q]uit > ",
        )?;
        let source = match cmd.as_str() {
            "f" => ":ytrec".to_string(),
            "u" => ":ytsubs".to_string(),
            "w" => ":ytwatchlater".to_string(),
            "h" => ":ythistory".to_string(),
            "s" => {
                let q = prompt("search: ")?;
                if q.is_empty() {
                    continue;
                }
                format!("ytsearch{LIST_LEN}:{q}")
            }
            "r" => {
                import_cookies(&cookies)?;
                continue;
            }
            "q" => break,
            _ => continue,
        };

        println!("loading…");
        let vids = match fetch(&source, &cookies) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("error: {e:#}");
                continue;
            }
        };
        if vids.is_empty() {
            println!("(nothing — if this was the feed, try re-importing cookies)");
            continue;
        }

        for (i, v) in vids.iter().enumerate() {
            let chan = v.channel.as_deref().or(v.uploader.as_deref()).unwrap_or("");
            println!(
                "{:>2}  {:<64}  {:>8}  {}",
                i + 1,
                trunc(v.title.as_deref().unwrap_or("?"), 64),
                fmt_dur(v.duration),
                trunc(chan, 28)
            );
        }

        let pick = prompt("play # (enter = back): ")?;
        if let Ok(n) = pick.parse::<usize>() {
            match vids.get(n.wrapping_sub(1)) {
                Some(v) => play(v, &cookies)?,
                None => println!("no such number"),
            }
        }
    }
    Ok(())
}

// ---------- cookies ----------

fn import_cookies(dest: &Path) -> Result<()> {
    let installed: Vec<_> = BROWSERS.iter().filter(|b| app_installed(b.2)).collect();
    let list = if installed.is_empty() { BROWSERS.iter().collect() } else { installed };

    println!("\nImport YouTube login cookies from which browser?");
    println!("(you must be signed in to YouTube in it)");
    for (i, b) in list.iter().enumerate() {
        println!("  {}. {}", i + 1, b.0);
    }
    println!("  or type a yt-dlp spec, e.g. firefox:Profile-Name  /  chrome:Profile 1");

    let ans = prompt("> ")?;
    let spec = match ans.parse::<usize>() {
        Ok(n) if (1..=list.len()).contains(&n) => list[n - 1].1.to_string(),
        _ if !ans.is_empty() => ans,
        _ => bail!("no browser chosen"),
    };

    match spec.split(':').next().unwrap_or("") {
        "safari" => println!(
            "note: Safari cookies need Full Disk Access for your terminal\n      \
             (System Settings → Privacy & Security → Full Disk Access)."
        ),
        "firefox" => {}
        _ => println!("note: macOS may ask for Keychain access — click Allow (once is fine)."),
    }

    let proceed = prompt(&format!("Read cookies from '{spec}' and save to {}? [Y/n] ", dest.display()))?;
    if proceed.eq_ignore_ascii_case("n") {
        bail!("cancelled");
    }

    let _ = fs::remove_file(dest);
    // yt-dlp loads cookies from the browser and writes the jar to --cookies on exit.
    let out = Command::new("yt-dlp")
        .args(["--cookies-from-browser", &spec, "--cookies"])
        .arg(dest)
        .args(["--skip-download", "--flat-playlist", "--playlist-end", "1", "-q", "-j", ":ytrec"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .context("running yt-dlp")?;

    if dest.exists() {
        restrict_perms(dest)?;
    }
    if !logged_in(dest) {
        eprintln!("{}", String::from_utf8_lossy(&out.stderr));
        bail!("no YouTube login found in {spec} — sign in to youtube.com there and run with --reimport");
    }
    println!("✓ logged-in cookies saved");
    Ok(())
}

/// A logged-in YouTube cookie jar contains these Google session cookies.
fn logged_in(cookies: &Path) -> bool {
    fs::read_to_string(cookies)
        .map(|s| s.contains("LOGIN_INFO") || s.contains("SAPISID"))
        .unwrap_or(false)
}

fn restrict_perms(p: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(p, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn app_installed(bundles: &[&str]) -> bool {
    let home_apps = dirs::home_dir().map(|h| h.join("Applications"));
    bundles.iter().any(|b| {
        PathBuf::from("/Applications").join(b).exists()
            || home_apps.as_ref().map_or(false, |h| h.join(b).exists())
    })
}

// ---------- listing & playback ----------

fn fetch(source: &str, cookies: &Path) -> Result<Vec<Video>> {
    let out = Command::new("yt-dlp")
        .arg("--cookies")
        .arg(cookies)
        .args(["--flat-playlist", "-j", "--playlist-end", &LIST_LEN.to_string(), source])
        .stderr(Stdio::piped())
        .output()
        .context("running yt-dlp")?;

    let vids: Vec<Video> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .filter(|v: &Video| v.id.len() == 11) // drop channels/playlists in results
        .collect();

    if vids.is_empty() && !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(vids)
}

fn play(v: &Video, cookies: &Path) -> Result<()> {
    println!("▶ {}  (close the window or press q to come back)", v.title.as_deref().unwrap_or(&v.id));
    let status = Command::new("mpv")
        .arg(format!("--ytdl-raw-options-append=cookies={}", cookies.display()))
        .arg(format!("--ytdl-format={FORMAT}"))
        .arg("--force-window=immediate")
        .arg(format!("--title={}", v.title.as_deref().unwrap_or("ytmini")))
        .arg(format!("https://www.youtube.com/watch?v={}", v.id))
        .status()
        .context("running mpv")?;
    if !status.success() {
        eprintln!("mpv exited with {status} — try `brew upgrade yt-dlp`");
    }
    Ok(())
}

// ---------- helpers ----------

fn check_deps() -> Result<()> {
    for bin in ["yt-dlp", "mpv"] {
        let ok = Command::new(bin)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            bail!("`{bin}` not found — run: brew install yt-dlp mpv deno");
        }
    }
    if Command::new("deno").arg("--version").stdout(Stdio::null()).status().is_err() {
        eprintln!("warning: deno not found; yt-dlp needs a JS runtime for YouTube (brew install deno)");
    }
    Ok(())
}

fn prompt(msg: &str) -> Result<String> {
    print!("{msg}");
    io::stdout().flush()?;
    let mut s = String::new();
    if io::stdin().read_line(&mut s)? == 0 {
        bail!("stdin closed");
    }
    Ok(s.trim().to_string())
}

fn trunc(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n - 1).collect::<String>() + "…"
    }
}

fn fmt_dur(d: Option<f64>) -> String {
    match d {
        Some(d) if d > 0.0 => {
            let s = d as u64;
            if s >= 3600 {
                format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
            } else {
                format!("{}:{:02}", s / 60, s % 60)
            }
        }
        _ => "live?".into(),
    }
}
