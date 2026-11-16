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
const FORMAT: &str = "bv*[height<=1080]+ba/b";

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
// Playback
// ---------------------------------------------------------------------------

/// Launch mpv in its own window. Returns immediately; the caller owns the child
/// and must `try_wait`/`kill` it.
pub fn play(tools: &Tools, cookies: &Path, video: &Video) -> Result<Child> {
    let log = config_dir()?.join("mpv.log");
    let log_file = fs::File::create(&log).context("creating mpv.log")?;
    let mut c = Command::new(&tools.mpv);
    c.env("PATH", &tools.path_env)
        // `-append` takes one key=value as-is, so a path with spaces/commas is safe.
        .arg(format!("--ytdl-raw-options-append=cookies={}", cookies.display()))
        .arg(format!("--script-opts-append=ytdl_hook-ytdl_path={}", tools.ytdlp.display()))
        .arg(format!("--ytdl-format={FORMAT}"))
        .arg("--force-window=immediate")
        .arg("--keep-open=no")
        .arg(format!("--title={} — ytmini", video.title()))
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
        let mut child = play(&t, &c, &v).unwrap();
        std::thread::sleep(std::time::Duration::from_secs(8));
        assert!(child.try_wait().unwrap().is_none(), "mpv exited early: {:?}", last_player_error());
        child.kill().unwrap();
        child.wait().unwrap();
    }
}
