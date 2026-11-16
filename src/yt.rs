//! Everything that talks to the outside world: locating yt-dlp / mpv / deno,
//! importing browser cookies, listing videos and launching the player.
//!
//! Nothing in here touches the UI; it is all plain blocking functions that the
//! app runs on worker threads.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::{
    ffi::OsString,
    fs,
    io::Write as _,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
};

/// Videos fetched per page.
pub const PAGE: usize = 30;

// ---------------------------------------------------------------------------
// External tools
// ---------------------------------------------------------------------------

/// Absolute paths to the external programs, plus the PATH we hand to children.
///
/// A macOS `.app` started from Finder gets a minimal PATH (`/usr/bin:/bin:…`),
/// which does not include Homebrew. So we search a few well-known locations
/// ourselves, and give children an extended PATH so that yt-dlp can find deno
/// and mpv can find yt-dlp.
#[derive(Clone, Debug)]
pub struct Tools {
    pub ytdlp: PathBuf,
    pub mpv: PathBuf,
    pub deno: Option<PathBuf>,
    path_env: OsString,
}

/// What's missing, if anything.
#[derive(Clone, Debug, Default)]
pub struct Missing {
    pub ytdlp: bool,
    pub mpv: bool,
}

impl Tools {
    /// Find the tools. Only checks the filesystem — never runs anything — so it
    /// is cheap enough to call from the UI thread.
    pub fn locate() -> std::result::Result<Tools, Missing> {
        let dirs = search_dirs();
        let path_env = std::env::join_paths(&dirs).unwrap_or_default();
        let ytdlp = find_exe("yt-dlp", &dirs);
        let mpv = find_exe("mpv", &dirs);
        match (ytdlp, mpv) {
            (Some(ytdlp), Some(mpv)) => Ok(Tools {
                ytdlp,
                mpv,
                deno: find_exe("deno", &dirs),
                path_env,
            }),
            (y, m) => Err(Missing { ytdlp: y.is_none(), mpv: m.is_none() }),
        }
    }

    fn ytdlp_cmd(&self) -> Command {
        let mut c = Command::new(&self.ytdlp);
        c.env("PATH", &self.path_env).stdin(Stdio::null());
        hide_console(&mut c);
        c
    }
}

/// Put the package-manager locations on this process's PATH (call at startup,
/// before spawning threads) so libmpv's ytdl hook finds yt-dlp and yt-dlp finds deno.
pub fn extend_process_path() {
    if let Ok(p) = std::env::join_paths(search_dirs()) {
        std::env::set_var("PATH", p);
    }
}

/// The current PATH followed by the usual package-manager locations.
fn search_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    let mut extra: Vec<PathBuf> = Vec::new();
    if cfg!(target_os = "macos") {
        extra.extend(["/opt/homebrew/bin", "/usr/local/bin", "/opt/local/bin"].map(PathBuf::from));
    } else if cfg!(unix) {
        extra.extend(["/usr/local/bin", "/usr/bin", "/snap/bin"].map(PathBuf::from));
    }
    if let Some(h) = dirs::home_dir() {
        extra.push(h.join(".local/bin"));
        extra.push(h.join(".deno/bin"));
        if cfg!(windows) {
            extra.push(h.join("scoop/shims"));
        }
    }
    if cfg!(target_os = "macos") || cfg!(unix) {
        extra.extend(["/usr/bin", "/bin"].map(PathBuf::from));
    }
    for d in extra {
        if !dirs.contains(&d) {
            dirs.push(d);
        }
    }
    dirs
}

fn find_exe(name: &str, dirs: &[PathBuf]) -> Option<PathBuf> {
    let names: Vec<String> = if cfg!(windows) {
        vec![format!("{name}.exe"), format!("{name}.com")]
    } else {
        vec![name.to_string()]
    };
    dirs.iter()
        .flat_map(|d| names.iter().map(move |n| d.join(n)))
        .find(|p| is_executable(p))
}

fn is_executable(p: &Path) -> bool {
    let Ok(meta) = fs::metadata(p) else { return false };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Don't flash a console window for every subprocess on Windows.
fn hide_console(_c: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        _c.creation_flags(CREATE_NO_WINDOW);
    }
}

/// Shell command that installs the dependencies on this OS.
pub fn install_hint() -> &'static str {
    if cfg!(target_os = "macos") {
        "brew install yt-dlp mpv deno"
    } else if cfg!(windows) {
        "winget install yt-dlp.yt-dlp shinchiro.mpv DenoLand.Deno"
    } else {
        "install yt-dlp, mpv and deno with your package manager (or pipx install yt-dlp)"
    }
}

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

/// `~/Library/Application Support/ytmini` on macOS, `~/.config/ytmini` on Linux,
/// `%APPDATA%\ytmini` on Windows.
pub fn config_dir() -> Result<PathBuf> {
    let d = dirs::config_dir().context("no config directory on this system")?.join("ytmini");
    fs::create_dir_all(&d).with_context(|| format!("creating {}", d.display()))?;
    Ok(d)
}

pub fn cookie_file() -> Result<PathBuf> {
    Ok(config_dir()?.join("cookies.txt"))
}

// ---------------------------------------------------------------------------
// Browsers & cookies
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Family {
    Chromium,
    Firefox,
    Safari,
}

#[derive(Clone, Copy, Debug)]
pub struct Browser {
    pub name: &'static str,
    /// The id yt-dlp's `--cookies-from-browser` understands.
    pub id: &'static str,
    pub family: Family,
}

const ALL_BROWSERS: &[Browser] = &[
    Browser { name: "Safari", id: "safari", family: Family::Safari },
    Browser { name: "Chrome", id: "chrome", family: Family::Chromium },
    Browser { name: "Firefox", id: "firefox", family: Family::Firefox },
    Browser { name: "Brave", id: "brave", family: Family::Chromium },
    Browser { name: "Edge", id: "edge", family: Family::Chromium },
    Browser { name: "Chromium", id: "chromium", family: Family::Chromium },
    Browser { name: "Opera", id: "opera", family: Family::Chromium },
    Browser { name: "Vivaldi", id: "vivaldi", family: Family::Chromium },
];

/// Browsers that have a profile on this machine (that's what yt-dlp reads,
/// so it's a better signal than "the app is installed").
pub fn detect_browsers() -> Vec<Browser> {
    ALL_BROWSERS.iter().copied().filter(|b| profile_dirs(b.id).iter().any(|p| p.exists())).collect()
}

fn profile_dirs(id: &str) -> Vec<PathBuf> {
    let home = dirs::home_dir().unwrap_or_default();
    #[cfg(target_os = "macos")]
    {
        let s = home.join("Library/Application Support");
        let rel: &[&str] = match id {
            "safari" => return vec![home.join("Library/Safari"), "/Applications/Safari.app".into()],
            "chrome" => &["Google/Chrome"],
            "firefox" => &["Firefox"],
            "brave" => &["BraveSoftware/Brave-Browser"],
            "edge" => &["Microsoft Edge"],
            "chromium" => &["Chromium"],
            "opera" => &["com.operasoftware.Opera"],
            "vivaldi" => &["Vivaldi"],
            _ => &[],
        };
        rel.iter().map(|r| s.join(r)).collect()
    }
    #[cfg(windows)]
    {
        let local = dirs::data_local_dir().unwrap_or_default();
        let roaming = dirs::config_dir().unwrap_or_default();
        let _ = home;
        match id {
            "chrome" => vec![local.join("Google/Chrome/User Data")],
            "firefox" => vec![roaming.join("Mozilla/Firefox")],
            "brave" => vec![local.join("BraveSoftware/Brave-Browser/User Data")],
            "edge" => vec![local.join("Microsoft/Edge/User Data")],
            "chromium" => vec![local.join("Chromium/User Data")],
            "opera" => vec![roaming.join("Opera Software/Opera Stable")],
            "vivaldi" => vec![local.join("Vivaldi/User Data")],
            _ => vec![],
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let cfg = home.join(".config");
        match id {
            "chrome" => vec![cfg.join("google-chrome")],
            "firefox" => vec![home.join(".mozilla/firefox"), home.join("snap/firefox/common/.mozilla/firefox")],
            "brave" => vec![cfg.join("BraveSoftware/Brave-Browser")],
            "edge" => vec![cfg.join("microsoft-edge")],
            "chromium" => vec![cfg.join("chromium"), home.join("snap/chromium/common/chromium")],
            "opera" => vec![cfg.join("opera")],
            "vivaldi" => vec![cfg.join("vivaldi")],
            _ => vec![],
        }
    }
}

/// One-line heads-up about what will happen when importing from this browser.
pub fn browser_note(spec: &str) -> &'static str {
    let id = spec.split([':', '+']).next().unwrap_or("").trim().to_ascii_lowercase();
    let family = ALL_BROWSERS.iter().find(|b| b.id == id).map(|b| b.family);
    match family {
        Some(Family::Firefox) => "No prompts — just make sure you're signed in to YouTube.",
        Some(Family::Safari) => {
            "Needs Full Disk Access for ytmini (or your terminal, if run from one): \
             System Settings → Privacy & Security → Full Disk Access."
        }
        Some(Family::Chromium) if cfg!(target_os = "macos") => {
            "macOS will ask for Keychain access to the browser's \"Safe Storage\" key — click Allow."
        }
        Some(Family::Chromium) if cfg!(windows) => {
            "Close the browser first. Recent Chrome/Edge versions may block cookie export — Firefox is more reliable."
        }
        Some(Family::Chromium) => "Your desktop keyring may ask to unlock — allow it.",
        None => "Custom yt-dlp spec, e.g. chrome:Profile 1 or firefox:my-profile.",
    }
}

/// A logged-in YouTube cookie jar contains these Google session cookies.
/// (Only checks for names; never reads values out.)
pub fn logged_in(cookies: &Path) -> bool {
    fs::read_to_string(cookies).map(|s| s.contains("LOGIN_INFO") || s.contains("SAPISID")).unwrap_or(false)
}

/// Export cookies from a browser into `dest` using yt-dlp. Blocking (may sit on a
/// Keychain prompt), so run it on a worker thread.
pub fn import_cookies(tools: &Tools, spec: &str, dest: &Path) -> Result<()> {
    let spec = spec.trim();
    if spec.is_empty() {
        bail!("no browser chosen");
    }
    // Write to a temp file first so a failed import doesn't destroy working cookies.
    let tmp = dest.with_extension("txt.new");
    let _ = fs::remove_file(&tmp);
    // yt-dlp loads cookies from the browser and writes the jar to --cookies on exit.
    let out = tools
        .ytdlp_cmd()
        .args(["--cookies-from-browser", spec, "--cookies"])
        .arg(&tmp)
        .args(["--skip-download", "--flat-playlist", "--playlist-end", "1", "-q", "-j", ":ytrec"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .context("running yt-dlp")?;

    if tmp.exists() {
        restrict_perms(&tmp)?;
    }
    if !logged_in(&tmp) {
        let _ = fs::remove_file(&tmp);
        let err = String::from_utf8_lossy(&out.stderr);
        let err = err.trim();
        if err.is_empty() {
            bail!("no YouTube login found in '{spec}' — sign in to youtube.com in that browser and try again");
        }
        bail!("no YouTube login found in '{spec}'.\n\nyt-dlp said:\n{err}");
    }
    fs::rename(&tmp, dest).context("saving cookies")?;
    Ok(())
}

fn restrict_perms(_p: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(_p, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Listing
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Source {
    Feed,
    Subscriptions,
    WatchLater,
    History,
    Search(String),
}

impl Source {
    pub fn label(&self) -> &str {
        match self {
            Source::Feed => "Feed",
            Source::Subscriptions => "Subscriptions",
            Source::WatchLater => "Watch later",
            Source::History => "History",
            Source::Search(_) => "Search",
        }
    }

    /// yt-dlp URL; `end` is the last (1-based) item we'll ask for.
    fn url(&self, end: usize) -> String {
        match self {
            Source::Feed => ":ytrec".into(),
            Source::Subscriptions => ":ytsubs".into(),
            Source::WatchLater => ":ytwatchlater".into(),
            Source::History => ":ythistory".into(),
            Source::Search(q) => format!("ytsearch{end}:{q}"),
        }
    }
}

#[derive(Clone, Debug, Deserialize, serde::Serialize)]
pub struct Video {
    pub id: String,
    pub title: Option<String>,
    pub channel: Option<String>,
    pub uploader: Option<String>,
    pub duration: Option<f64>,
}

impl Video {
    /// A bare video from a YouTube URL or an 11-character id (for `--play`).
    pub fn from_url_or_id(s: &str) -> Option<Video> {
        let s = s.trim();
        let id = if s.len() == 11 && !s.contains('/') {
            s.to_string()
        } else if let Some(i) = s.find("v=") {
            s[i + 2..].chars().take(11).collect()
        } else {
            let i = s.find("youtu.be/").map(|i| i + 9).or_else(|| s.find("/shorts/").map(|i| i + 8))?;
            s[i..].chars().take(11).collect()
        };
        let ok = id.len() == 11 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        ok.then_some(Video { id, title: None, channel: None, uploader: None, duration: None })
    }

    pub fn title(&self) -> &str {
        self.title.as_deref().unwrap_or(&self.id)
    }
    pub fn channel(&self) -> &str {
        self.channel.as_deref().or(self.uploader.as_deref()).unwrap_or("")
    }
    pub fn url(&self) -> String {
        format!("https://www.youtube.com/watch?v={}", self.id)
    }
    /// `H:MM:SS` / `M:SS`, or `LIVE` when there's no duration.
    pub fn duration_label(&self) -> String {
        match self.duration {
            Some(d) if d > 0.0 => {
                let s = d.round() as u64;
                if s >= 3600 {
                    format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
                } else {
                    format!("{}:{:02}", s / 60, s % 60)
                }
            }
            _ => "LIVE".into(),
        }
    }
}

/// Fetch `count` videos starting at 0-based `start`. Blocking.
pub fn fetch(tools: &Tools, cookies: &Path, source: &Source, start: usize, count: usize) -> Result<Vec<Video>> {
    let first = start + 1;
    let last = start + count;
    let out = tools
        .ytdlp_cmd()
        .arg("--cookies")
        .arg(cookies)
        .args(["--flat-playlist", "-j", "--no-warnings"])
        .args(["--playlist-start", &first.to_string(), "--playlist-end", &last.to_string()])
        .arg(source.url(last))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("running yt-dlp")?;

    let vids: Vec<Video> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .filter(|v: &Video| v.id.len() == 11) // drop channels/playlists in results
        .collect();

    if vids.is_empty() && !out.status.success() {
        bail!("{}", summarize_err(&String::from_utf8_lossy(&out.stderr)));
    }
    Ok(vids)
}

/// yt-dlp's stderr can be long; keep the ERROR lines (or the tail).
fn summarize_err(stderr: &str) -> String {
    let errors: Vec<&str> = stderr.lines().filter(|l| l.contains("ERROR")).collect();
    let lines: Vec<&str> = if errors.is_empty() {
        stderr.lines().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect()
    } else {
        errors
    };
    let mut s = lines.join("\n").trim().to_string();
    if s.is_empty() {
        s = "yt-dlp failed without saying why".into();
    }
    if s.contains("Sign in") || s.contains("signature") || s.contains("challenge") {
        s.push_str("\n(Try updating yt-dlp, make sure deno is installed, or re-import cookies.)");
    }
    s
}

// ---------------------------------------------------------------------------
// Power & quality
// ---------------------------------------------------------------------------

/// What the OS says about power right now.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Power {
    pub on_battery: bool,
    /// macOS Low Power Mode / Windows Battery Saver / Linux "low-power" profile.
    pub saver: bool,
}

/// Cheap (a couple of ms on macOS, a syscall elsewhere).
pub fn power_state() -> Power {
    #[cfg(target_os = "macos")]
    {
        let pmset = |args: &[&str]| {
            Command::new("/usr/bin/pmset")
                .args(args)
                .stderr(Stdio::null())
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
                .unwrap_or_default()
        };
        let saver = pmset(&["-g"]).lines().any(|l| {
            let mut w = l.split_whitespace();
            matches!((w.next(), w.next()), (Some("lowpowermode" | "powermode"), Some("1")))
        });
        let on_battery = pmset(&["-g", "batt"]).contains("'Battery Power'");
        Power { on_battery, saver }
    }
    #[cfg(windows)]
    {
        #[repr(C)]
        #[derive(Default)]
        struct SystemPowerStatus {
            ac_line_status: u8,
            battery_flag: u8,
            battery_life_percent: u8,
            system_status_flag: u8,
            battery_life_time: u32,
            battery_full_life_time: u32,
        }
        #[link(name = "kernel32")]
        extern "system" {
            fn GetSystemPowerStatus(status: *mut SystemPowerStatus) -> i32;
        }
        let mut st = SystemPowerStatus::default();
        // SAFETY: plain Win32 call filling a repr(C) struct we own.
        if unsafe { GetSystemPowerStatus(&mut st) } == 0 {
            return Power::default();
        }
        Power { on_battery: st.ac_line_status == 0, saver: st.system_status_flag == 1 }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let read = |p: &Path| fs::read_to_string(p).map(|s| s.trim().to_string()).unwrap_or_default();
        let saver = read(Path::new("/sys/firmware/acpi/platform_profile")) == "low-power";
        let mut has_mains = false;
        let mut mains_online = false;
        if let Ok(entries) = fs::read_dir("/sys/class/power_supply") {
            for e in entries.flatten() {
                if read(&e.path().join("type")) == "Mains" {
                    has_mains = true;
                    mains_online |= read(&e.path().join("online")) == "1";
                }
            }
        }
        Power { on_battery: has_mains && !mains_online, saver }
    }
}

/// Watches the power state on its own thread and calls `on_change` only when
/// it differs from the last reading.
///
/// macOS: waits on the notify(3) keys IOKit/powerd post when the power source
/// or Low Power Mode changes. Checking a key is a shared-memory read, so we
/// can check every second for free and only run `pmset` when one fires (plus
/// a slow safety re-check). Elsewhere the state is a cheap syscall/sysfs read,
/// so we just poll it.
pub fn watch_power(initial: Power, on_change: impl Fn(Power) + Send + 'static) {
    use std::time::{Duration, Instant};
    std::thread::Builder::new()
        .name("power".into())
        .spawn(move || {
            let mut last = initial;
            let mut changed = power_change_detector();
            let mut last_full = Instant::now();
            loop {
                std::thread::sleep(Duration::from_secs(if cfg!(target_os = "macos") { 1 } else { 2 }));
                let due = last_full.elapsed() >= Duration::from_secs(60);
                if !(changed() || due) {
                    continue;
                }
                last_full = Instant::now();
                let now = power_state();
                if now != last {
                    last = now;
                    on_change(now);
                }
            }
        })
        .expect("spawning power watcher");
}

/// Returns a closure that says "something power-related may have changed".
#[cfg(target_os = "macos")]
fn power_change_detector() -> Box<dyn FnMut() -> bool + Send> {
    use std::ffi::{c_char, c_int};
    extern "C" {
        // libSystem (notify.h)
        fn notify_register_check(name: *const c_char, out_token: *mut c_int) -> u32;
        fn notify_check(token: c_int, check: *mut c_int) -> u32;
    }
    const KEYS: [&std::ffi::CStr; 2] = [c"com.apple.system.powersources.source", c"com.apple.system.lowpowermode"];
    let mut tokens = Vec::new();
    for key in KEYS {
        let mut tok: c_int = 0;
        // SAFETY: valid NUL-terminated name and out-pointer.
        if unsafe { notify_register_check(key.as_ptr(), &mut tok) } == 0 {
            let mut first: c_int = 0;
            // SAFETY: token just registered; the first check always reports a change, consume it.
            unsafe { notify_check(tok, &mut first) };
            tokens.push(tok);
        }
    }
    if tokens.len() < KEYS.len() {
        // Couldn't subscribe: fall back to polling pmset every few seconds.
        let mut n = 0u32;
        return Box::new(move || {
            n += 1;
            n.is_multiple_of(5)
        });
    }
    Box::new(move || {
        tokens.iter().fold(false, |any, &tok| {
            let mut c: c_int = 0;
            // SAFETY: registered token, valid out-pointer.
            let ok = unsafe { notify_check(tok, &mut c) } == 0;
            any | (ok && c != 0)
        })
    })
}

#[cfg(not(target_os = "macos"))]
fn power_change_detector() -> Box<dyn FnMut() -> bool + Send> {
    Box::new(|| true) // reading the state is cheap; just check every tick
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, serde::Serialize)]
pub enum Quality {
    /// Highest the screen can show; lowered on battery / in saver mode.
    #[default]
    Auto,
    Max(u32),
    AudioOnly,
}

/// Standard YouTube heights, highest first.
pub const TIERS: [u32; 6] = [2160, 1440, 1080, 720, 480, 360];

impl Quality {
    pub fn label(self) -> String {
        match self {
            Quality::Auto => "Auto (screen + power aware)".into(),
            Quality::Max(h) => format!("{h}p"),
            Quality::AudioOnly => "Audio only".into(),
        }
    }

    /// Choices for the menu: Auto, every tier the screen can show, audio only.
    pub fn choices(screen_cap: u32) -> Vec<Quality> {
        let mut v = vec![Quality::Auto];
        v.extend(TIERS.iter().filter(|&&h| h <= screen_cap).map(|&h| Quality::Max(h)));
        v.push(Quality::AudioOnly);
        v
    }
}

/// Largest standard tier that fits a fullscreen 16:9 video on a monitor with
/// this many *physical* pixels. Anything higher would just be downscaled.
pub fn screen_cap(width_px: f32, height_px: f32) -> u32 {
    let video_h = height_px.min(width_px * 9.0 / 16.0);
    // 3% slack so e.g. a 1050-line screen still counts as a 1080p screen.
    TIERS.iter().copied().find(|&h| h as f32 <= video_h * 1.03).unwrap_or(360)
}

/// Concrete mpv settings for one playback.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Profile {
    pub ytdl_format: String,
    /// Use mpv's built-in `fast` profile (cheaper scaling, no extras).
    pub fast: bool,
    pub audio_only: bool,
    /// Shown in the status bar, e.g. "720p30 · battery saver".
    pub label: String,
}

/// yt-dlp format chain: preferred variant first, then progressively looser.
/// We skip AV1 in the first pick: most Macs before M3 (and many PCs) can't
/// hardware-decode it, and software AV1 at high res is a battery killer.
fn format_for(h: u32, fps30: bool, saver: bool) -> String {
    let fps = if fps30 { "[fps<=30]" } else { "" };
    let (codec, audio) = if saver { ("[vcodec^=avc1]", "ba[abr<=128]") } else { ("[vcodec!^=av01]", "ba") };
    format!("bv*[height<={h}]{fps}{codec}+{audio}/bv*[height<={h}]{fps}+ba/bv*[height<={h}]+ba/b[height<={h}]/b")
}

pub fn profile(q: Quality, p: Power, screen_cap: u32) -> Profile {
    let mk = |h: u32, fps30: bool, saver: bool, why: &str| Profile {
        ytdl_format: format_for(h, fps30, saver),
        fast: saver || h <= 480,
        audio_only: false,
        label: format!("{h}p{}{why}", if fps30 { "30" } else { "" }),
    };
    match q {
        Quality::AudioOnly => Profile {
            ytdl_format: "ba/b".into(),
            fast: true,
            audio_only: true,
            label: "audio only".into(),
        },
        // Manual pick, still never above the screen (window may have moved).
        Quality::Max(h) => mk(h.min(screen_cap), false, false, ""),
        Quality::Auto if p.saver => mk(screen_cap.min(720), true, true, " · battery saver"),
        Quality::Auto if p.on_battery => mk(screen_cap.min(1080), true, false, " · on battery"),
        Quality::Auto => mk(screen_cap, false, false, ""),
    }
}

// ---------------------------------------------------------------------------
// Playback
// ---------------------------------------------------------------------------

/// Launch mpv in its own window. Returns immediately; the caller owns the child
/// and must `try_wait`/`kill` it.
/// Where mpv listens for JSON IPC commands (a Unix socket, or a named pipe on Windows).
pub fn ipc_path() -> PathBuf {
    let name = format!("ytmini-mpv-{}", std::process::id());
    if cfg!(windows) {
        PathBuf::from(format!(r"\\.\pipe\{name}"))
    } else {
        // temp_dir keeps us under the ~104-byte Unix socket path limit on macOS.
        std::env::temp_dir().join(format!("{name}.sock"))
    }
}

pub fn play(tools: &Tools, cookies: &Path, video: &Video, profile: &Profile) -> Result<Child> {
    let log = config_dir()?.join("mpv.log");
    let log_file = fs::File::create(&log).context("creating mpv.log")?;
    let mut c = Command::new(&tools.mpv);
    c.env("PATH", &tools.path_env)
        // `-append` takes one key=value as-is, so a path with spaces/commas is safe.
        .arg(format!("--ytdl-raw-options-append=cookies={}", cookies.display()))
        .arg(format!("--script-opts-append=ytdl_hook-ytdl_path={}", tools.ytdlp.display()))
        .arg(format!("--ytdl-format={}", profile.ytdl_format))
        // mpv defaults to software decoding; hardware decoding is the biggest battery win.
        .arg("--hwdec=auto-safe")
        .arg("--force-window=immediate")
        .arg("--keep-open=no")
        .arg(format!("--input-ipc-server={}", ipc_path().display()))
        .arg(format!("--title={} — ytmini", video.title()))
        .args(profile.fast.then_some("--profile=fast"))
        .args(profile.audio_only.then_some("--no-video"))
        .arg(video.url())
        .stdin(Stdio::null())
        .stdout(log_file.try_clone()?)
        .stderr(log_file);
    hide_console(&mut c);
    c.spawn().context("starting mpv")
}

/// Switch a running mpv to a new profile without restarting the video: read the
/// position, set the new format, reload the same URL from that position.
/// Blocking (talks to mpv over IPC), so run it on a worker thread.
pub fn switch_profile(url: &str, profile: &Profile) -> Result<()> {
    let mut ipc = Ipc::connect(&ipc_path())?;
    let pos = ipc.request(serde_json::json!(["get_property", "time-pos"])).ok().and_then(|v| v.as_f64());
    ipc.request(serde_json::json!(["set_property", "ytdl-format", profile.ytdl_format]))?;
    ipc.request(serde_json::json!(["set_property", "vid", if profile.audio_only { "no" } else { "auto" }]))?;
    let opts = pos.map(|p| format!("start={p:.1}")).unwrap_or_default();
    // mpv ≥ 0.38: loadfile <url> <flags> <index> <options>
    ipc.request(serde_json::json!(["loadfile", url, "replace", -1, opts]))?;
    Ok(())
}

/// Minimal mpv JSON IPC client: one request at a time, events are skipped.
struct Ipc {
    reader: std::io::BufReader<Box<dyn std::io::Read>>,
    writer: Box<dyn std::io::Write>,
    next_id: u64,
}

impl Ipc {
    fn connect(path: &Path) -> Result<Ipc> {
        #[cfg(unix)]
        let (r, w): (Box<dyn std::io::Read>, Box<dyn std::io::Write>) = {
            let s = std::os::unix::net::UnixStream::connect(path).context("connecting to mpv")?;
            s.set_read_timeout(Some(std::time::Duration::from_secs(3)))?;
            (Box::new(s.try_clone()?), Box::new(s))
        };
        #[cfg(windows)]
        let (r, w): (Box<dyn std::io::Read>, Box<dyn std::io::Write>) = {
            let f = fs::OpenOptions::new().read(true).write(true).open(path).context("connecting to mpv")?;
            (Box::new(f.try_clone()?), Box::new(f))
        };
        Ok(Ipc { reader: std::io::BufReader::new(r), writer: w, next_id: 1 })
    }

    fn request(&mut self, command: serde_json::Value) -> Result<serde_json::Value> {
        use std::io::BufRead;
        let id = self.next_id;
        self.next_id += 1;
        let msg = serde_json::json!({ "command": command, "request_id": id });
        writeln!(self.writer, "{msg}")?;
        self.writer.flush()?;
        let mut line = String::new();
        loop {
            line.clear();
            if self.reader.read_line(&mut line)? == 0 {
                bail!("mpv closed the IPC connection");
            }
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
            if v.get("request_id").and_then(|x| x.as_u64()) != Some(id) {
                continue; // an event or someone else's reply
            }
            return match v.get("error").and_then(|e| e.as_str()) {
                Some("success") => Ok(v.get("data").cloned().unwrap_or(serde_json::Value::Null)),
                Some(e) => bail!("mpv: {e}"),
                None => bail!("mpv: malformed reply"),
            };
        }
    }
}

/// Last error-looking line from mpv's log, for the status bar.
pub fn last_player_error() -> Option<String> {
    let log = fs::read_to_string(config_dir().ok()?.join("mpv.log")).ok()?;
    log.lines()
        .rev()
        .find(|l| {
            let l = l.to_ascii_lowercase();
            l.contains("error") || l.contains("failed")
        })
        .map(|l| l.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(d: Option<f64>) -> Video {
        Video { id: "abcdefghijk".into(), title: None, channel: None, uploader: None, duration: d }
    }

    #[test]
    fn durations() {
        assert_eq!(v(Some(59.0)).duration_label(), "0:59");
        assert_eq!(v(Some(605.0)).duration_label(), "10:05");
        assert_eq!(v(Some(3725.0)).duration_label(), "1:02:05");
        assert_eq!(v(None).duration_label(), "LIVE");
        assert_eq!(v(Some(0.0)).duration_label(), "LIVE");
    }

    #[test]
    fn power_profiles() {
        let saver = Power { on_battery: true, saver: true };
        let batt = Power { on_battery: true, saver: false };
        let ac = Power::default();
        assert!(profile(Quality::Auto, saver, 1440).ytdl_format.starts_with("bv*[height<=720][fps<=30][vcodec^=avc1]"));
        assert!(profile(Quality::Auto, batt, 1440).ytdl_format.starts_with("bv*[height<=1080][fps<=30]"));
        assert!(profile(Quality::Auto, ac, 1440).ytdl_format.starts_with("bv*[height<=1440][vcodec!^=av01]"));
        // a manual choice wins over the power state, but never exceeds the screen
        assert!(profile(Quality::Max(1080), saver, 1440).ytdl_format.starts_with("bv*[height<=1080][vcodec!^=av01]+ba/"));
        assert!(profile(Quality::Max(2160), ac, 1080).ytdl_format.starts_with("bv*[height<=1080]"));
    }

    #[test]
    fn screen_caps() {
        assert_eq!(screen_cap(1920.0, 1080.0), 1080);
        assert_eq!(screen_cap(1680.0, 1050.0), 720); // 16:9 video is 945 tall there
        assert_eq!(screen_cap(3456.0, 2234.0), 1440); // 16" MacBook Pro
        assert_eq!(screen_cap(3840.0, 2160.0), 2160);
        assert_eq!(screen_cap(2560.0, 1600.0), 1440);
        assert_eq!(Quality::choices(1080).len(), 6); // auto, 1080/720/480/360, audio
    }

    #[test]
    fn parse_play_arg() {
        let id = |s: &str| Video::from_url_or_id(s).map(|v| v.id);
        assert_eq!(id("dQw4w9WgXcQ").as_deref(), Some("dQw4w9WgXcQ"));
        assert_eq!(id("https://www.youtube.com/watch?v=dQw4w9WgXcQ&t=1").as_deref(), Some("dQw4w9WgXcQ"));
        assert_eq!(id("https://youtu.be/dQw4w9WgXcQ").as_deref(), Some("dQw4w9WgXcQ"));
        assert_eq!(id("https://youtube.com/shorts/dQw4w9WgXcQ").as_deref(), Some("dQw4w9WgXcQ"));
        assert_eq!(id("hello"), None);
    }

    #[test]
    fn search_url_covers_page() {
        assert_eq!(Source::Search("rust".into()).url(60), "ytsearch60:rust");
    }
}

/// Live tests against YouTube using the real cookie jar: `cargo test -- --ignored`.
#[cfg(test)]
mod live {
    use super::*;

    fn setup() -> (Tools, PathBuf) {
        (Tools::locate().expect("yt-dlp and mpv installed"), cookie_file().unwrap())
    }

    #[test]
    #[ignore]
    fn search_pages_dont_overlap() {
        let (t, c) = setup();
        let s = Source::Search("rust programming".into());
        let p1 = fetch(&t, &c, &s, 0, PAGE).unwrap();
        let p2 = fetch(&t, &c, &s, PAGE, PAGE).unwrap();
        assert!(p1.len() >= 20 && p2.len() >= 20, "{} {}", p1.len(), p2.len());
        let dup = p2.iter().filter(|b| p1.iter().any(|a| a.id == b.id)).count();
        eprintln!("page sizes {} {}, overlap {dup}", p1.len(), p2.len());
        assert!(dup < p2.len() / 2, "page 2 mostly repeats page 1");
    }

    #[test]
    #[ignore]
    fn personal_tabs() {
        let (t, c) = setup();
        for s in [Source::Feed, Source::Subscriptions, Source::WatchLater, Source::History] {
            match fetch(&t, &c, &s, 0, 5) {
                Ok(v) => eprintln!("{s:?}: {} videos", v.len()),
                Err(e) => eprintln!("{s:?}: error {e:#}"),
            }
        }
    }

    #[test]
    #[ignore]
    fn mpv_starts_and_dies() {
        let (t, c) = setup();
        let v = fetch(&t, &c, &Source::Search("big buck bunny".into()), 0, 1).unwrap().remove(0);
        let mut child = play(&t, &c, &v, &profile(Quality::Auto, power_state(), 1080)).unwrap();
        std::thread::sleep(std::time::Duration::from_secs(8));
        assert!(child.try_wait().unwrap().is_none(), "mpv exited early: {:?}", last_player_error());
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[test]
    #[ignore]
    fn live_quality_switch_keeps_position() {
        use std::time::Duration;
        let (t, c) = setup();
        let v = fetch(&t, &c, &Source::Search("big buck bunny 1080p".into()), 0, 1).unwrap().remove(0);
        let mut child = play(&t, &c, &v, &profile(Quality::Max(1080), Power::default(), 2160)).unwrap();
        std::thread::sleep(Duration::from_secs(12));
        let get = |prop: &str| Ipc::connect(&ipc_path()).unwrap().request(serde_json::json!(["get_property", prop]));
        let before = get("time-pos").unwrap().as_f64().unwrap();
        eprintln!("before: pos {before:.1}, height {:?}", get("height").ok());
        switch_profile(&v.url(), &profile(Quality::Max(480), Power::default(), 2160)).unwrap();
        std::thread::sleep(Duration::from_secs(10));
        let after = get("time-pos").unwrap().as_f64().unwrap();
        let height = get("height").unwrap().as_i64().unwrap();
        eprintln!("after: pos {after:.1}, height {height}");
        assert!(child.try_wait().unwrap().is_none(), "mpv died");
        assert!(after >= before, "position went back: {before} -> {after}");
        assert!(height <= 480, "still at {height}p");
        child.kill().unwrap();
        child.wait().unwrap();
    }
}
