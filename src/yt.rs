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

#[derive(Clone, Debug, Deserialize)]
pub struct Video {
    pub id: String,
    pub title: Option<String>,
    pub channel: Option<String>,
    pub uploader: Option<String>,
    pub duration: Option<f64>,
}

impl Video {
    pub fn title(&self) -> &str {
        self.title.as_deref().unwrap_or(&self.id)
    }
    pub fn channel(&self) -> &str {
        self.channel.as_deref().or(self.uploader.as_deref()).unwrap_or("")
    }
    pub fn thumbnail(&self) -> String {
        format!("https://i.ytimg.com/vi/{}/mqdefault.jpg", self.id)
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

/// Cheap (a couple of ms); called when playback starts.
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Quality {
    /// Pick from the power state.
    #[default]
    Auto,
    P1080,
    P720,
    P480,
    AudioOnly,
}

impl Quality {
    pub const ALL: [Quality; 5] = [Quality::Auto, Quality::P1080, Quality::P720, Quality::P480, Quality::AudioOnly];

    pub fn label(self) -> &'static str {
        match self {
            Quality::Auto => "Auto (power-aware)",
            Quality::P1080 => "1080p",
            Quality::P720 => "720p",
            Quality::P480 => "480p",
            Quality::AudioOnly => "Audio only",
        }
    }
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

pub fn profile(q: Quality, p: Power) -> Profile {
    let cap = |h: u32| format!("bv*[height<={h}]+ba/b");
    let mk = |f: String, fast, audio_only, label: &str| Profile { ytdl_format: f, fast, audio_only, label: label.into() };
    match q {
        Quality::P1080 => mk(cap(1080), false, false, "1080p"),
        Quality::P720 => mk(cap(720), false, false, "720p"),
        Quality::P480 => mk(cap(480), true, false, "480p"),
        Quality::AudioOnly => mk("ba/b".into(), true, true, "audio only"),
        Quality::Auto if p.saver => mk(
            // H.264 is hardware-decoded everywhere and the cheapest to decode;
            // fall back step by step if a video doesn't have it.
            "bv*[height<=720][fps<=30][vcodec^=avc1]+ba[abr<=128]/bv*[height<=720][fps<=30]+ba/bv*[height<=720]+ba/b"
                .into(),
            true,
            false,
            "720p30 · battery saver",
        ),
        Quality::Auto if p.on_battery => mk(
            "bv*[height<=1080][fps<=30]+ba/bv*[height<=1080]+ba/b".into(),
            false,
            false,
            "1080p30 · on battery",
        ),
        Quality::Auto => mk(cap(1080), false, false, "1080p"),
    }
}

// ---------------------------------------------------------------------------
// Playback
// ---------------------------------------------------------------------------

/// Launch mpv in its own window. Returns immediately; the caller owns the child
/// and must `try_wait`/`kill` it.
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
        assert!(profile(Quality::Auto, saver).ytdl_format.starts_with("bv*[height<=720][fps<=30][vcodec^=avc1]"));
        assert!(profile(Quality::Auto, batt).ytdl_format.contains("[fps<=30]"));
        assert_eq!(profile(Quality::Auto, Power::default()).ytdl_format, "bv*[height<=1080]+ba/b");
        // a manual choice wins over the power state
        assert_eq!(profile(Quality::P1080, saver).ytdl_format, "bv*[height<=1080]+ba/b");
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
        let mut child = play(&t, &c, &v, &profile(Quality::Auto, power_state())).unwrap();
        std::thread::sleep(std::time::Duration::from_secs(8));
        assert!(child.try_wait().unwrap().is_none(), "mpv exited early: {:?}", last_player_error());
        child.kill().unwrap();
        child.wait().unwrap();
    }
}
