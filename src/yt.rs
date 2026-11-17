//! Everything that talks to the outside world: locating yt-dlp / mpv / deno,
//! importing browser cookies, listing videos and launching the player.
//!
//! Nothing in here touches the UI; it is all plain blocking functions that the
//! app runs on worker threads.

use crate::i18n::t;
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
    /// ytmini's own auto-updated copy (vs. one from the system / Homebrew).
    pub ytdlp_managed: bool,
    /// mpv executable for the separate-window player (bundled `Helpers/mpv`
    /// first). `None` is fine as long as libmpv is available.
    pub mpv: Option<PathBuf>,
    pub deno: Option<PathBuf>,
    path_env: OsString,
}

/// What's missing, if anything.
#[derive(Clone, Debug, Default)]
pub struct Missing {
    /// No yt-dlp anywhere — the app can download one.
    pub ytdlp: bool,
    /// Neither libmpv nor an mpv executable.
    pub player: bool,
}

impl Tools {
    /// Find the tools. Checks the filesystem (and loads libmpv once).
    pub fn locate() -> std::result::Result<Tools, Missing> {
        let dirs = search_dirs();
        let path_env = std::env::join_paths(&dirs).unwrap_or_default();
        let managed = crate::deps::Dep::YtDlp.managed();
        let ytdlp_managed = managed.is_some();
        let ytdlp = managed.or_else(|| find_exe("yt-dlp", &dirs));
        let mpv = bundled_helper("mpv").or_else(|| find_exe("mpv", &dirs));
        let player_ok = mpv.is_some() || crate::mpv::available().is_ok();
        match (ytdlp, player_ok) {
            (Some(ytdlp), true) => Ok(Tools {
                ytdlp,
                ytdlp_managed,
                mpv,
                // a system deno is kept up to date by its package manager; else ours
                deno: find_exe("deno", &dirs).or_else(|| crate::deps::Dep::Deno.managed()),
                path_env,
            }),
            (y, p) => Err(Missing {
                ytdlp: y.is_none(),
                player: !p,
            }),
        }
    }

    pub fn ytdlp_cmd(&self) -> Command {
        let mut c = Command::new(&self.ytdlp);
        c.env("PATH", &self.path_env).stdin(Stdio::null());
        if let Some(d) = &self.deno {
            c.arg("--js-runtimes").arg(format!("deno:{}", d.display()));
        }
        hide_console(&mut c);
        c
    }

    /// Extra yt-dlp options for mpv's ytdl hook (`ytdl-raw-options`).
    pub fn ytdl_raw_options(&self) -> Vec<String> {
        self.deno
            .iter()
            .map(|d| format!("js-runtimes=deno:{}", d.display()))
            .collect()
    }
}

/// An executable shipped inside the app bundle (`ytmini.app/Contents/Helpers/<name>`)
/// or next to the binary.
fn bundled_helper(name: &str) -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    let file = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    };
    [dir.join("../Helpers").join(&file), dir.join(&file)]
        .into_iter()
        .find(|p| is_executable(p))
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
    let Ok(meta) = fs::metadata(p) else {
        return false;
    };
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
        "brew install mpv"
    } else if cfg!(windows) {
        "winget install shinchiro.mpv"
    } else {
        "install mpv (libmpv) with your package manager"
    }
}

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

/// `~/Library/Application Support/ytmini` on macOS, `~/.config/ytmini` on Linux,
/// `%APPDATA%\ytmini` on Windows.
/// `YTMINI_CONFIG_DIR` overrides it (testing a fresh install / signed-out state).
pub fn config_dir() -> Result<PathBuf> {
    let d = match std::env::var_os("YTMINI_CONFIG_DIR") {
        Some(d) => PathBuf::from(d),
        None => dirs::config_dir()
            .context("no config directory on this system")?
            .join("ytmini"),
    };
    fs::create_dir_all(&d).with_context(|| format!("creating {}", d.display()))?;
    // Holds the session cookies: private to this user.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&d, fs::Permissions::from_mode(0o700));
    }
    Ok(d)
}

/// A private, throwaway copy of the session cookies for one yt-dlp run
/// (deleted on drop). yt-dlp writes its cookie jar back to `--cookies` when
/// it exits; with several runs at once (hover, player, downloads, feeds)
/// they'd overwrite the real file with each other's copies, which can log
/// the session out. Copies live in the private config dir, never /tmp.
pub struct CookieCopy(PathBuf);

impl CookieCopy {
    pub fn new(src: &Path) -> Result<CookieCopy> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = config_dir()?.join("run");
        fs::create_dir_all(&dir)?;
        let n = N.fetch_add(1, Ordering::Relaxed);
        let p = dir.join(format!("cookies-{}-{n}.txt", std::process::id()));
        // signed out: an empty jar
        let data = fs::read(src).unwrap_or_default();
        write_private(&p.with_extension("tmp"), &p, &data)?;
        Ok(CookieCopy(p))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    /// Leftovers from a crash (call once at startup).
    pub fn clean_stale() {
        if let Ok(rd) = config_dir().and_then(|d| Ok(fs::read_dir(d.join("run"))?)) {
            for e in rd.flatten() {
                let _ = fs::remove_file(e.path());
            }
        }
    }
}

impl Drop for CookieCopy {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Write a file that only this user can read, created that way (no window
/// with default permissions), replacing `dest` atomically via `tmp`.
pub fn write_private(tmp: &Path, dest: &Path, data: &[u8]) -> Result<()> {
    use std::io::Write;
    let _ = fs::remove_file(tmp);
    let mut o = fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    let mut f = o
        .open(tmp)
        .with_context(|| format!("creating {}", tmp.display()))?;
    f.write_all(data)?;
    f.sync_all()?;
    drop(f);
    fs::rename(tmp, dest).with_context(|| format!("saving {}", dest.display()))?;
    Ok(())
}

/// Hosts whose cookies ytmini keeps: YouTube and Google's sign-in. Nothing
/// else (not Gmail, Drive, Pay… even though they share the Google account).
pub fn wanted_cookie_host(host: &str) -> bool {
    let h = host
        .trim_start_matches("#HttpOnly_")
        .trim_start_matches('.');
    matches!(
        h,
        "youtube.com"
            | "www.youtube.com"
            | "m.youtube.com"
            | "music.youtube.com"
            | "google.com"
            | "www.google.com"
            | "accounts.google.com"
    )
}

/// Startup hygiene: drop other sites' cookies from the jar (older versions
/// imported whole browsers through yt-dlp), fix its permissions, and delete
/// any browser-database snapshot left behind by an interrupted sign-in.
pub fn tidy_cookie_storage() {
    let Ok(dir) = config_dir() else { return };
    for leftover in ["signin-check", "signin-export"] {
        let _ = fs::remove_dir_all(dir.join(leftover));
    }
    for leftover in ["cookies.txt.new", "cookies.txt.rot"] {
        let _ = fs::remove_file(dir.join(leftover));
    }
    let jar = dir.join("cookies.txt");
    let Ok(text) = fs::read_to_string(&jar) else {
        return;
    };
    let _ = restrict_perms(&jar);
    let keep = |l: &&str| {
        let l = l.trim();
        if l.is_empty() || (l.starts_with('#') && !l.starts_with("#HttpOnly_")) {
            return true; // comments / header
        }
        wanted_cookie_host(l.split('\t').next().unwrap_or(""))
    };
    let kept: Vec<&str> = text.lines().filter(keep).collect();
    if kept.len() != text.lines().count() {
        let mut out = kept.join("\n");
        out.push('\n');
        let _ = write_private(&dir.join("cookies.txt.new"), &jar, out.as_bytes());
    }
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

pub const ALL_BROWSERS: &[Browser] = &[
    Browser {
        name: "Safari",
        id: "safari",
        family: Family::Safari,
    },
    Browser {
        name: "Chrome",
        id: "chrome",
        family: Family::Chromium,
    },
    Browser {
        name: "Firefox",
        id: "firefox",
        family: Family::Firefox,
    },
    Browser {
        name: "Brave",
        id: "brave",
        family: Family::Chromium,
    },
    Browser {
        name: "Edge",
        id: "edge",
        family: Family::Chromium,
    },
    Browser {
        name: "Chromium",
        id: "chromium",
        family: Family::Chromium,
    },
    Browser {
        name: "Opera",
        id: "opera",
        family: Family::Chromium,
    },
    Browser {
        name: "Vivaldi",
        id: "vivaldi",
        family: Family::Chromium,
    },
];

pub fn profile_dirs(id: &str) -> Vec<PathBuf> {
    let home = dirs::home_dir().unwrap_or_default();
    #[cfg(target_os = "macos")]
    {
        let s = home.join("Library/Application Support");
        let rel: &[&str] = match id {
            "safari" => {
                return vec![
                    home.join("Library/Safari"),
                    "/Applications/Safari.app".into(),
                ]
            }
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
            "firefox" => vec![
                home.join(".mozilla/firefox"),
                home.join("snap/firefox/common/.mozilla/firefox"),
            ],
            "brave" => vec![cfg.join("BraveSoftware/Brave-Browser")],
            "edge" => vec![cfg.join("microsoft-edge")],
            "chromium" => vec![
                cfg.join("chromium"),
                home.join("snap/chromium/common/chromium"),
            ],
            "opera" => vec![cfg.join("opera")],
            "vivaldi" => vec![cfg.join("vivaldi")],
            _ => vec![],
        }
    }
}

/// A logged-in YouTube cookie jar contains these Google session cookies.
/// (Only checks for names; never reads values out.)
pub fn logged_in(cookies: &Path) -> bool {
    fs::read_to_string(cookies)
        .map(|s| s.contains("LOGIN_INFO") || s.contains("SAPISID"))
        .unwrap_or(false)
}

pub fn restrict_perms(_p: &Path) -> Result<()> {
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
    /// Home's "Music" category (YouTube's chip, personalised), as its own tab.
    Music,
    WatchLater,
    History,
    /// Your playlists (Liked videos, Watch later, your own).
    Playlists,
    Search(String),
    /// A channel's uploads. `url` is the channel's base URL.
    Channel {
        url: String,
        name: String,
    },
    ChannelPlaylists {
        url: String,
        name: String,
    },
    Playlist {
        id: String,
        title: String,
    },
    /// Videos saved for offline viewing (local, see downloads.rs).
    Downloads,
    /// One of the home feed's categories ("Music", "Gaming"…): YouTube's
    /// own chip, loaded through its continuation token.
    Topic {
        title: String,
        token: String,
    },
}

impl Source {
    pub fn label(&self) -> String {
        match self {
            Source::Feed => t("Feed").into(),
            Source::Subscriptions => t("Subscriptions").into(),
            Source::Music => t("Music").into(),
            Source::WatchLater => t("Watch later").into(),
            Source::History => t("History").into(),
            Source::Playlists => t("Playlists").into(),
            Source::Search(_) => t("Search").into(),
            Source::Channel { name, .. } | Source::ChannelPlaylists { name, .. } => name.clone(),
            Source::Playlist { title, .. } => title.clone(),
            Source::Downloads => t("Downloads").into(),
            Source::Topic { title, .. } => title.clone(),
        }
    }

    /// Home, or one of its categories.
    pub fn is_home(&self) -> bool {
        matches!(self, Source::Feed | Source::Topic { .. })
    }

    /// Pages reached by navigating (channel/playlist), as opposed to top tabs.
    pub fn is_page(&self) -> bool {
        matches!(
            self,
            Source::Channel { .. } | Source::ChannelPlaylists { .. } | Source::Playlist { .. }
        )
    }

    /// yt-dlp URL; `end` is the last (1-based) item we'll ask for.
    fn url(&self, end: usize) -> String {
        match self {
            Source::Feed => ":ytrec".into(),
            Source::Subscriptions => ":ytsubs".into(),
            Source::WatchLater => ":ytwatchlater".into(),
            Source::History => ":ythistory".into(),
            Source::Playlists => "https://www.youtube.com/feed/playlists".into(),
            Source::Search(q) => format!("ytsearch{end}:{q}"),
            Source::Channel { url, .. } => format!("{}/videos", url.trim_end_matches('/')),
            Source::ChannelPlaylists { url, .. } => {
                format!("{}/playlists", url.trim_end_matches('/'))
            }
            Source::Playlist { id, .. } => format!("https://www.youtube.com/playlist?list={id}"),
            Source::Downloads => String::new(), // local; never fetched
            Source::Topic { .. } => ":ytrec".into(), // native only; yt-dlp has no categories
            // native only too; YouTube's own Music channel instead
            Source::Music => "https://www.youtube.com/channel/UC-9-kyTW8ZkZNDHQJ6FgpwQ".into(),
        }
    }
}

/// One line of `yt-dlp --flat-playlist -j` output.
#[derive(Deserialize)]
struct RawEntry {
    id: String,
    title: Option<String>,
    channel: Option<String>,
    uploader: Option<String>,
    duration: Option<f64>,
    channel_id: Option<String>,
    channel_url: Option<String>,
    uploader_url: Option<String>,
    ie_key: Option<String>,
    url: Option<String>,
    view_count: Option<u64>,
    description: Option<String>,
    #[serde(default)]
    thumbnails: Vec<RawThumb>,
}

#[derive(Deserialize)]
struct RawThumb {
    url: String,
}

impl RawEntry {
    fn into_video(self) -> Option<Video> {
        let is_playlist = self.ie_key.as_deref() == Some("YoutubeTab")
            && self.url.as_deref().is_some_and(|u| u.contains("list="));
        if self.id.len() != 11 && !is_playlist {
            return None; // channels and other non-video results
        }
        // playlist thumbnails are a video's: ".../vi/<video id>/hqdefault.jpg"
        let thumb_id = is_playlist
            .then(|| {
                self.thumbnails.iter().find_map(|t| {
                    let rest = t.url.split("/vi/").nth(1)?;
                    let id: String = rest.chars().take_while(|&c| c != '/').collect();
                    (id.len() == 11).then_some(id)
                })
            })
            .flatten();
        let channel_url = self
            .channel_url
            .or_else(|| {
                self.channel_id
                    .as_ref()
                    .map(|c| format!("https://www.youtube.com/channel/{c}"))
            })
            .or(self.uploader_url);
        Some(Video {
            id: self.id,
            title: self.title,
            channel: self.channel,
            uploader: self.uploader,
            duration: self.duration,
            channel_url,
            is_playlist,
            thumb_id,
            views: self.view_count,
            views_text: None,
            // search results carry a ~150-char snippet; keep it short
            description: self.description.map(|d| d.chars().take(200).collect()),
            age: None,
            is_short: false,
            is_live: false,
            is_music: false,
        })
    }
}

/// A video — or, on playlist listings, a playlist (`is_playlist`).
#[derive(Clone, Debug, Default, Deserialize, serde::Serialize)]
pub struct Video {
    pub id: String,
    pub title: Option<String>,
    pub channel: Option<String>,
    pub uploader: Option<String>,
    pub duration: Option<f64>,
    #[serde(default)]
    pub channel_url: Option<String>,
    #[serde(default)]
    pub is_playlist: bool,
    /// For playlists: the video whose thumbnail represents it.
    #[serde(default)]
    pub thumb_id: Option<String>,
    #[serde(default)]
    pub views: Option<u64>,
    /// YouTube's own wording of the view count when it isn't English
    /// ("1,3 млн просмотров"); shown as is.
    #[serde(default)]
    pub views_text: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    /// "3 days ago" (only from the native API)
    #[serde(default)]
    pub age: Option<String>,
    /// A Short (only known from the native API).
    #[serde(default)]
    pub is_short: bool,
    /// Live now, or an upcoming stream/premiere (only known from the native API).
    #[serde(default)]
    pub is_live: bool,
    /// A song: YouTube marks it with a music note (only known from the native API).
    #[serde(default)]
    pub is_music: bool,
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
            let i = s
                .find("youtu.be/")
                .map(|i| i + 9)
                .or_else(|| s.find("/shorts/").map(|i| i + 8))?;
            s[i..].chars().take(11).collect()
        };
        let ok = id.len() == 11
            && id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        ok.then_some(Video {
            id,
            ..Default::default()
        })
    }

    pub fn title(&self) -> &str {
        self.title.as_deref().unwrap_or(&self.id)
    }
    pub fn channel(&self) -> &str {
        self.channel
            .as_deref()
            .or(self.uploader.as_deref())
            .unwrap_or("")
    }
    pub fn url(&self) -> String {
        format!("https://www.youtube.com/watch?v={}", self.id)
    }
    /// "1.2M views"
    pub fn views_label(&self) -> Option<String> {
        let Some(n) = self.views else {
            return self.views_text.clone();
        };
        // plural form follows the exact count; the number shown is compact
        let s = crate::i18n::tn("{} view", "{} views", n);
        Some(s.replacen(&n.to_string(), &crate::i18n::compact(n), 1))
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
            // Shorts listings carry no length; that isn't "live"
            _ if self.is_short && !self.is_live => String::new(),
            _ => "LIVE".into(),
        }
    }
}

/// Stream URLs resolved ahead of time (on hover), so playback can skip the
/// ~4 s yt-dlp spends on YouTube's page, player script and JS challenge.
#[derive(Clone, Debug)]
pub struct Direct {
    pub video: String,
    /// Separate audio track (DASH), if any.
    pub audio: Option<String>,
    pub user_agent: Option<String>,
    /// A download on disk rather than resolved URLs.
    pub offline: bool,
    /// The video's title, when the lookup told us (for videos opened from a
    /// bare link: mpv can't learn it from stream URLs).
    pub title: Option<String>,
}

impl Direct {
    /// Still worth using: YouTube's URLs carry their expiry (`expire=`,
    /// Unix seconds, ~6 h out); keep a 10-minute margin.
    pub fn fresh(&self) -> bool {
        if self.offline {
            return true;
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        url_expiry(&self.video).is_none_or(|e| e > now + 600)
    }
}

/// `expire` from a googlevideo URL (query `expire=…` or path `/expire/…/`).
fn url_expiry(url: &str) -> Option<u64> {
    let i = url
        .find("expire=")
        .map(|i| i + 7)
        .or_else(|| url.find("/expire/").map(|i| i + 8))?;
    let digits: String = url[i..].chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// The local address the default route uses: changes when you switch
/// Wi-Fi, turn a VPN on/off or move networks (stream URLs are bound to the
/// IP they were resolved from). No packets are sent: UDP "connect" only
/// picks the route.
pub fn route_fingerprint() -> Option<std::net::IpAddr> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("1.1.1.1:53").ok()?;
    Some(s.local_addr().ok()?.ip())
}

/// yt-dlp extractor args for stream lookups: skip the client-config request
/// (~0.6 s); the watch page already carries that config. Not `webpage`:
/// skipping that trips YouTube's bot check.
pub const RESOLVE_EXTRACTOR_ARGS: &str = "youtube:player_skip=configs";

/// Resolve `video` with a one-off yt-dlp run for `format` (blocking, ~3 s).
/// `resolver::resolve` is the fast path; this is its fallback.
pub fn resolve(tools: &Tools, cookies: &Path, video: &Video, format: &str) -> Result<Direct> {
    let jar = CookieCopy::new(cookies)?;
    let out = tools
        .ytdlp_cmd()
        .arg("--cookies")
        .arg(jar.path())
        .args(["-J", "--no-warnings", "--no-playlist", "-f", format])
        .args(["--extractor-args", RESOLVE_EXTRACTOR_ARGS])
        .arg(video.url())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .context("running yt-dlp")?;
    anyhow::ensure!(out.status.success(), "yt-dlp couldn't resolve the video");
    let j: serde_json::Value = serde_json::from_slice(&out.stdout).context("yt-dlp output")?;
    direct_from_info(&j)
}

/// The stream URLs in a yt-dlp info dict (`-J` output, or the resolver's).
pub fn direct_from_info(j: &serde_json::Value) -> Result<Direct> {
    let url = |f: &serde_json::Value| f.get("url").and_then(|u| u.as_str()).map(str::to_string);
    let user_agent = j
        .pointer("/http_headers/User-Agent")
        .and_then(|u| u.as_str())
        .map(str::to_string);
    let title = j.get("title").and_then(|t| t.as_str()).map(str::to_string);
    if let Some(parts) = j.get("requested_formats").and_then(|r| r.as_array()) {
        let video_part = parts
            .iter()
            .find(|f| f.get("vcodec").and_then(|v| v.as_str()) != Some("none"));
        let audio_part = parts
            .iter()
            .find(|f| f.get("vcodec").and_then(|v| v.as_str()) == Some("none"));
        if let Some(v) = video_part.and_then(url) {
            return Ok(Direct {
                video: v,
                audio: audio_part.and_then(url),
                user_agent,
                offline: false,
                title,
            });
        }
        if let Some(a) = audio_part.and_then(url) {
            return Ok(Direct {
                video: a,
                audio: None,
                user_agent,
                offline: false,
                title,
            });
        }
    }
    let v = url(j).context("no stream URL in yt-dlp's answer")?;
    Ok(Direct {
        video: v,
        audio: None,
        user_agent,
        offline: false,
        title,
    })
}

/// Fetch `count` videos starting at 0-based `start`. Blocking.
#[cfg(test)]
pub fn fetch(
    tools: &Tools,
    cookies: &Path,
    source: &Source,
    start: usize,
    count: usize,
) -> Result<Vec<Video>> {
    let first = start + 1;
    let last = start + count;
    let jar = CookieCopy::new(cookies)?;
    let out = tools
        .ytdlp_cmd()
        .arg("--cookies")
        .arg(jar.path())
        .args(["--flat-playlist", "-j", "--no-warnings"])
        .args([
            "--playlist-start",
            &first.to_string(),
            "--playlist-end",
            &last.to_string(),
        ])
        .arg(source.url(last))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("running yt-dlp")?;

    let vids: Vec<Video> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| serde_json::from_str::<RawEntry>(l).ok())
        .filter_map(RawEntry::into_video)
        .collect();

    if vids.is_empty() && !out.status.success() {
        bail!("{}", summarize_err(&String::from_utf8_lossy(&out.stderr)));
    }
    Ok(vids)
}

/// How far a stream may go (yt-dlp stops early when the list ends).
const STREAM_MAX: usize = 2000;

/// A running `yt-dlp --lazy-playlist` that yields videos as YouTube pages
/// arrive. We read lines only when more videos are wanted; once the pipe
/// buffer (~a page) is full yt-dlp blocks, so it stays one page ahead and
/// "load more" never refetches earlier pages. Dropping it kills the process.
pub struct Stream {
    child: Child,
    _jar: CookieCopy,
    out: std::io::BufReader<std::process::ChildStdout>,
    stderr: std::sync::Arc<std::sync::Mutex<String>>,
    ended: bool,
}

impl Stream {
    /// Start at 0-based `start` (for resuming a stream that was closed while idle).
    pub fn open(tools: &Tools, cookies: &Path, source: &Source, start: usize) -> Result<Stream> {
        let jar = CookieCopy::new(cookies)?;
        let mut child = tools
            .ytdlp_cmd()
            .env("PYTHONUNBUFFERED", "1")
            .arg("--cookies")
            .arg(jar.path())
            .args(["--flat-playlist", "--lazy-playlist", "-j", "--no-warnings"])
            .args([
                "--playlist-start",
                &(start + 1).to_string(),
                "--playlist-end",
                &STREAM_MAX.to_string(),
            ])
            .arg(source.url(STREAM_MAX))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("running yt-dlp")?;
        let out = std::io::BufReader::new(child.stdout.take().context("no stdout")?);
        let stderr = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        if let Some(mut err) = child.stderr.take() {
            let buf = stderr.clone();
            std::thread::spawn(move || {
                use std::io::Read;
                let mut chunk = [0u8; 4096];
                while let Ok(n) = err.read(&mut chunk) {
                    if n == 0 {
                        break;
                    }
                    let mut b = buf.lock().unwrap_or_else(|e| e.into_inner());
                    b.push_str(&String::from_utf8_lossy(&chunk[..n]));
                    if b.len() > 16_384 {
                        let cut = b.len() - 8192;
                        let cut = (cut..b.len()).find(|&i| b.is_char_boundary(i)).unwrap_or(0);
                        b.drain(..cut);
                    }
                }
            });
        }
        Ok(Stream {
            child,
            _jar: jar,
            out,
            stderr,
            ended: false,
        })
    }

    /// The next video; `Ok(None)` at the end of the list.
    pub fn next_video(&mut self) -> Result<Option<Video>> {
        use std::io::BufRead;
        let mut line = String::new();
        while !self.ended {
            line.clear();
            if self.out.read_line(&mut line).unwrap_or(0) == 0 {
                self.ended = true;
                break;
            }
            if let Some(v) = serde_json::from_str::<RawEntry>(&line)
                .ok()
                .and_then(RawEntry::into_video)
            {
                return Ok(Some(v));
            }
        }
        let status = self.child.wait().context("waiting for yt-dlp")?;
        if !status.success() {
            std::thread::sleep(std::time::Duration::from_millis(50)); // let the stderr reader finish
            let err = self
                .stderr
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            bail!("{}", summarize_err(&err));
        }
        Ok(None)
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        if !self.ended {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

/// yt-dlp's stderr can be long; keep the ERROR lines (or the tail).
fn summarize_err(stderr: &str) -> String {
    let errors: Vec<&str> = stderr.lines().filter(|l| l.contains("ERROR")).collect();
    let lines: Vec<&str> = if errors.is_empty() {
        stderr
            .lines()
            .rev()
            .take(4)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect()
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
            matches!(
                (w.next(), w.next()),
                (Some("lowpowermode" | "powermode"), Some("1"))
            )
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
        Power {
            on_battery: st.ac_line_status == 0,
            saver: st.system_status_flag == 1,
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let read = |p: &Path| {
            fs::read_to_string(p)
                .map(|s| s.trim().to_string())
                .unwrap_or_default()
        };
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
        Power {
            on_battery: has_mains && !mains_online,
            saver,
        }
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
                std::thread::sleep(Duration::from_secs(if cfg!(target_os = "macos") {
                    1
                } else {
                    2
                }));
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
    const KEYS: [&std::ffi::CStr; 2] = [
        c"com.apple.system.powersources.source",
        c"com.apple.system.lowpowermode",
    ];
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
            Quality::Auto => t("Auto (screen + power aware)").into(),
            Quality::Max(h) => format!("{h}p"),
            Quality::AudioOnly => t("Audio only").into(),
        }
    }

    /// Choices for the menu: Auto, every tier the screen can show, audio only.
    pub fn choices(screen_cap: u32) -> Vec<Quality> {
        let mut v = vec![Quality::Auto];
        v.extend(
            TIERS
                .iter()
                .filter(|&&h| h <= screen_cap)
                .map(|&h| Quality::Max(h)),
        );
        v.push(Quality::AudioOnly);
        v
    }
}

/// Largest standard tier that fits a fullscreen 16:9 video on a monitor with
/// this many *physical* pixels. Anything higher would just be downscaled.
pub fn screen_cap(width_px: f32, height_px: f32) -> u32 {
    let video_h = height_px.min(width_px * 9.0 / 16.0);
    // 3% slack so e.g. a 1050-line screen still counts as a 1080p screen.
    TIERS
        .iter()
        .copied()
        .find(|&h| h as f32 <= video_h * 1.03)
        .unwrap_or(360)
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
///
/// VP9 first: Apple silicon decodes it in hardware like H.264, but at the
/// same size it looks far better (VMAF at 720p: VP9 99, H.264 65), and our
/// measurements showed no meaningful power difference between them or
/// between 720p and 1080p VP9 (~0.3 W over idle either way). AV1 is skipped
/// in the first picks: Macs before M3 decode it in software, which does cost
/// battery.
fn format_for(h: u32, fps30: bool) -> String {
    let fps = if fps30 { "[fps<=30]" } else { "" };
    format!(
        "bv*[height<={h}]{fps}[vcodec^=vp]+ba/bv*[height<={h}]{fps}[vcodec!^=av01]+ba/bv*[height<={h}]{fps}+ba/bv*[height<={h}]+ba/b[height<={h}]/b"
    )
}

pub fn profile(q: Quality, p: Power, screen_cap: u32) -> Profile {
    let mk = |h: u32, fps30: bool, why: &str| Profile {
        ytdl_format: format_for(h, fps30),
        // mpv's `fast` profile only saved power when it was paired with
        // low-quality H.264; with hardware VP9 the default scaler is cheap.
        fast: false,
        audio_only: false,
        label: format!("{h}p{}{why}", if fps30 { "30" } else { "" }),
    };
    match q {
        Quality::AudioOnly => Profile {
            ytdl_format: "ba/b".into(),
            fast: true,
            audio_only: true,
            label: t("audio only").into(),
        },
        // Manual pick, still never above the screen (window may have moved).
        Quality::Max(h) => mk(h.min(screen_cap), false, ""),
        // Power-aware Auto: 1080p30 VP9. Higher frame rates and resolutions
        // are where the real cost is; 1080p itself is nearly free.
        Quality::Auto if p.saver => mk(
            screen_cap.min(1080),
            true,
            &format!(" · {}", t("battery saver")),
        ),
        Quality::Auto if p.on_battery => mk(
            screen_cap.min(1080),
            true,
            &format!(" · {}", t("on battery")),
        ),
        Quality::Auto => mk(screen_cap, false, ""),
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

pub fn play(
    tools: &Tools,
    cookies: &Path,
    video: &Video,
    profile: &Profile,
    resume: Option<f64>,
) -> Result<(Child, CookieCopy)> {
    let jar = CookieCopy::new(cookies)?;
    let mpv = tools
        .mpv
        .as_ref()
        .context("no mpv executable for the separate-window player")?;
    let log = config_dir()?.join("mpv.log");
    let log_file = fs::File::create(&log).context("creating mpv.log")?;
    let mut c = Command::new(mpv);
    c.env("PATH", &tools.path_env)
        // `-append` takes one key=value as-is, so a path with spaces/commas is safe.
        .arg(format!(
            "--ytdl-raw-options-append=cookies={}",
            jar.path().display()
        ))
        .arg(format!(
            "--script-opts-append=ytdl_hook-ytdl_path={}",
            tools.ytdlp.display()
        ))
        .arg(format!("--ytdl-format={}", profile.ytdl_format))
        // mpv defaults to software decoding; hardware decoding is the biggest battery win.
        .arg("--hwdec=auto-safe")
        .arg("--force-window=immediate")
        .arg("--keep-open=no")
        .arg(format!("--input-ipc-server={}", ipc_path().display()))
        .arg(format!("--title={} — ytmini", video.title()))
        .args(
            tools
                .ytdl_raw_options()
                .iter()
                .map(|o| format!("--ytdl-raw-options-append={o}")),
        )
        .args(resume.map(|t| format!("--start={t:.1}")))
        .args(profile.fast.then_some("--profile=fast"))
        .args(profile.audio_only.then_some("--no-video"))
        .arg(video.url())
        .stdin(Stdio::null())
        .stdout(log_file.try_clone()?)
        .stderr(log_file);
    hide_console(&mut c);
    Ok((c.spawn().context("starting mpv")?, jar))
}

/// Switch a running mpv to a new profile without restarting the video: read the
/// position, set the new format, reload the same URL from that position.
/// Blocking (talks to mpv over IPC), so run it on a worker thread.
pub fn switch_profile(url: &str, profile: &Profile) -> Result<()> {
    let mut ipc = Ipc::connect(&ipc_path())?;
    let pos = ipc
        .request(serde_json::json!(["get_property", "time-pos"]))
        .ok()
        .and_then(|v| v.as_f64());
    ipc.request(serde_json::json!([
        "set_property",
        "ytdl-format",
        profile.ytdl_format
    ]))?;
    ipc.request(serde_json::json!([
        "set_property",
        "vid",
        if profile.audio_only { "no" } else { "auto" }
    ]))?;
    let opts = pos.map(|p| format!("start={p:.1}")).unwrap_or_default();
    // mpv ≥ 0.38: loadfile <url> <flags> <index> <options>
    ipc.request(serde_json::json!(["loadfile", url, "replace", -1, opts]))?;
    Ok(())
}

/// Position and duration of the separate-window mpv (for saving progress).
pub fn ipc_progress() -> Option<(f64, f64)> {
    let mut ipc = Ipc::connect(&ipc_path()).ok()?;
    let pos = ipc
        .request(serde_json::json!(["get_property", "time-pos"]))
        .ok()?
        .as_f64()?;
    let dur = ipc
        .request(serde_json::json!(["get_property", "duration"]))
        .ok()
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    Some((pos, dur))
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
            let f = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)
                .context("connecting to mpv")?;
            (Box::new(f.try_clone()?), Box::new(f))
        };
        Ok(Ipc {
            reader: std::io::BufReader::new(r),
            writer: w,
            next_id: 1,
        })
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
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
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

    /// yt-dlp gets a private copy of the cookies, gone when dropped.
    #[test]
    fn expiry_parse() {
        assert_eq!(
            url_expiry("https://rr1.googlevideo.com/videoplayback?expire=1794887000&ei=x"),
            Some(1794887000)
        );
        assert_eq!(
            url_expiry("https://manifest.googlevideo.com/api/manifest/hls/expire/1794887000/ei/x"),
            Some(1794887000)
        );
        assert_eq!(url_expiry("/path/file.webm"), None);
    }

    #[test]
    fn cookie_copy_private() {
        use std::os::unix::fs::PermissionsExt;
        let src = std::env::temp_dir().join(format!("ytmini-cc-{}.txt", std::process::id()));
        fs::write(&src, b"# Netscape HTTP Cookie File\n").unwrap();
        let c = CookieCopy::new(&src).unwrap();
        let p = c.path().to_path_buf();
        assert_ne!(p, src);
        assert_eq!(fs::read(&p).unwrap(), fs::read(&src).unwrap());
        assert_eq!(
            fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
        drop(c);
        assert!(!p.exists());
        let _ = fs::remove_file(src);
    }

    fn v(d: Option<f64>) -> Video {
        Video {
            id: "abcdefghijk".into(),
            duration: d,
            ..Default::default()
        }
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
        let saver = Power {
            on_battery: true,
            saver: true,
        };
        let batt = Power {
            on_battery: true,
            saver: false,
        };
        let ac = Power::default();
        let sp = profile(Quality::Auto, saver, 1440);
        assert!(sp
            .ytdl_format
            .starts_with("bv*[height<=1080][fps<=30][vcodec^=vp]+ba/"));
        assert!(!sp.fast);
        assert!(profile(Quality::Auto, batt, 1440)
            .ytdl_format
            .starts_with("bv*[height<=1080][fps<=30]"));
        assert!(profile(Quality::Auto, ac, 1440)
            .ytdl_format
            .starts_with("bv*[height<=1440][vcodec^=vp]+ba/bv*[height<=1440][vcodec!^=av01]"));
        // a manual choice wins over the power state, but never exceeds the screen
        assert!(profile(Quality::Max(1080), saver, 1440)
            .ytdl_format
            .starts_with("bv*[height<=1080][vcodec^=vp]+ba/"));
        assert!(profile(Quality::Max(2160), ac, 1080)
            .ytdl_format
            .starts_with("bv*[height<=1080]"));
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
        assert_eq!(
            id("https://www.youtube.com/watch?v=dQw4w9WgXcQ&t=1").as_deref(),
            Some("dQw4w9WgXcQ")
        );
        assert_eq!(
            id("https://youtu.be/dQw4w9WgXcQ").as_deref(),
            Some("dQw4w9WgXcQ")
        );
        assert_eq!(
            id("https://youtube.com/shorts/dQw4w9WgXcQ").as_deref(),
            Some("dQw4w9WgXcQ")
        );
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
        (
            Tools::locate().expect("yt-dlp and mpv installed"),
            cookie_file().unwrap(),
        )
    }

    #[test]
    #[ignore]
    fn search_pages_dont_overlap() {
        let (t, c) = setup();
        let s = Source::Search("rust programming".into());
        let p1 = fetch(&t, &c, &s, 0, PAGE).unwrap();
        let p2 = fetch(&t, &c, &s, PAGE, PAGE).unwrap();
        assert!(
            p1.len() >= 20 && p2.len() >= 20,
            "{} {}",
            p1.len(),
            p2.len()
        );
        let dup = p2
            .iter()
            .filter(|b| p1.iter().any(|a| a.id == b.id))
            .count();
        eprintln!("page sizes {} {}, overlap {dup}", p1.len(), p2.len());
        assert!(dup < p2.len() / 2, "page 2 mostly repeats page 1");
    }

    #[test]
    #[ignore]
    fn channel_and_playlists() {
        let (t, c) = setup();
        let feed = fetch(&t, &c, &Source::Feed, 0, 5).unwrap();
        let v = feed
            .iter()
            .find(|v| v.channel_url.is_some())
            .expect("feed entry with channel url");
        let (url, name) = (v.channel_url.clone().unwrap(), v.channel().to_string());
        let vids = fetch(
            &t,
            &c,
            &Source::Channel {
                url: url.clone(),
                name: name.clone(),
            },
            0,
            5,
        )
        .unwrap();
        eprintln!(
            "channel {name}: {} videos, first: {}",
            vids.len(),
            vids[0].title()
        );
        assert!(!vids.is_empty() && vids.iter().all(|v| !v.is_playlist));

        let blender = "https://www.youtube.com/@BlenderOfficial".to_string();
        let lists = fetch(
            &t,
            &c,
            &Source::ChannelPlaylists {
                url: blender,
                name: "Blender".into(),
            },
            0,
            5,
        )
        .unwrap();
        assert!(lists.iter().all(|l| l.is_playlist), "all playlists");
        assert!(
            lists
                .iter()
                .all(|l| l.thumb_id.as_ref().is_some_and(|i| i.len() == 11)),
            "thumbs"
        );
        let first = &lists[0];
        eprintln!("playlist: {} ({})", first.title(), first.id);
        let inside = fetch(
            &t,
            &c,
            &Source::Playlist {
                id: first.id.clone(),
                title: first.title().into(),
            },
            0,
            5,
        )
        .unwrap();
        eprintln!("  {} videos, first: {}", inside.len(), inside[0].title());
        assert!(!inside.is_empty() && inside.iter().all(|v| !v.is_playlist));

        let mine = fetch(&t, &c, &Source::Playlists, 0, 10).unwrap();
        eprintln!(
            "my playlists: {:?}",
            mine.iter().map(|p| p.title()).collect::<Vec<_>>()
        );
        assert!(mine.iter().any(|p| p.id == "LL"), "Liked videos present");
    }

    #[test]
    #[ignore]
    fn personal_tabs() {
        let (t, c) = setup();
        for s in [
            Source::Feed,
            Source::Subscriptions,
            Source::WatchLater,
            Source::History,
        ] {
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
        let v = fetch(&t, &c, &Source::Search("big buck bunny".into()), 0, 1)
            .unwrap()
            .remove(0);
        let (mut child, _jar) = play(
            &t,
            &c,
            &v,
            &profile(Quality::Auto, power_state(), 1080),
            None,
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_secs(8));
        assert!(
            child.try_wait().unwrap().is_none(),
            "mpv exited early: {:?}",
            last_player_error()
        );
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[test]
    #[ignore]
    fn live_quality_switch_keeps_position() {
        use std::time::Duration;
        let (t, c) = setup();
        let v = fetch(&t, &c, &Source::Search("big buck bunny 1080p".into()), 0, 1)
            .unwrap()
            .remove(0);
        let (mut child, _jar) = play(
            &t,
            &c,
            &v,
            &profile(Quality::Max(1080), Power::default(), 2160),
            None,
        )
        .unwrap();
        std::thread::sleep(Duration::from_secs(12));
        let get = |prop: &str| {
            Ipc::connect(&ipc_path())
                .unwrap()
                .request(serde_json::json!(["get_property", prop]))
        };
        let before = get("time-pos").unwrap().as_f64().unwrap();
        eprintln!("before: pos {before:.1}, height {:?}", get("height").ok());
        switch_profile(
            &v.url(),
            &profile(Quality::Max(480), Power::default(), 2160),
        )
        .unwrap();
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

#[cfg(test)]
mod stream_live {
    /// Streams 3 pages of search results: no duplicates, later pages fast.
    /// `cargo test -- --ignored stream_pages --nocapture`
    #[test]
    #[ignore]
    fn stream_pages() {
        let tools = super::Tools::locate().unwrap();
        let cookies = super::config_dir().unwrap().join("cookies.txt");
        let t = std::time::Instant::now();
        let mut s = super::Stream::open(
            &tools,
            &cookies,
            &super::Source::Search("rust programming".into()),
            0,
        )
        .unwrap();
        let mut ids = std::collections::HashSet::new();
        for page in 0..3 {
            let p = std::time::Instant::now();
            for _ in 0..super::PAGE {
                let v = s.next_video().unwrap().expect("more results");
                assert!(ids.insert(v.id.clone()), "duplicate {}", v.id);
            }
            eprintln!(
                "page {page}: {:.2}s (total {:.2}s)",
                p.elapsed().as_secs_f32(),
                t.elapsed().as_secs_f32()
            );
        }
    }
}

#[cfg(test)]
mod watch_later_live {
    /// Adds a video to Watch later, checks it's listed, removes it again.
    /// `cargo test -- --ignored watch_later_roundtrip --nocapture`
    #[test]
    #[ignore]
    fn watch_later_roundtrip() {
        let tools = super::Tools::locate().unwrap();
        let cookies = super::config_dir().unwrap().join("cookies.txt");
        let id = "jNQXAC9IVRw"; // "Me at the zoo"
        crate::innertube::set_watch_later(&cookies, id, true).unwrap();
        let listed = super::fetch(&tools, &cookies, &super::Source::WatchLater, 0, 50).unwrap();
        let found = listed.iter().any(|v| v.id == id);
        crate::innertube::set_watch_later(&cookies, id, false).unwrap();
        eprintln!("listed after add: {found}");
        assert!(found);
    }
}

#[cfg(test)]
mod tidy_tests {
    #[test]
    fn tidy_keeps_only_google_and_youtube() {
        let dir = std::env::temp_dir().join(format!("ytmini-tidy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("signin-export")).unwrap();
        // SAFETY: only this test reads YTMINI_CONFIG_DIR in this process's test run.
        unsafe { std::env::set_var("YTMINI_CONFIG_DIR", &dir) };
        let jar = "# Netscape HTTP Cookie File\n\n\
            .youtube.com\tTRUE\t/\tTRUE\t0\tSID\tfake\n\
            #HttpOnly_.google.com\tTRUE\t/\tTRUE\t0\tHSID\tfake\n\
            accounts.google.com\tFALSE\t/\tTRUE\t0\tLSID\tfake\n\
            .chatgpt.com\tTRUE\t/\tTRUE\t0\tsession\tfake\n\
            .notgoogle.com\tTRUE\t/\tTRUE\t0\tx\tfake\n\
            mail.google.com\tFALSE\t/\tTRUE\t0\tGMAIL\tfake\n\
            #HttpOnly_.evil-youtube.com\tTRUE\t/\tTRUE\t0\ty\tfake\n";
        std::fs::write(dir.join("cookies.txt"), jar).unwrap();
        super::tidy_cookie_storage();
        let out = std::fs::read_to_string(dir.join("cookies.txt")).unwrap();
        unsafe { std::env::remove_var("YTMINI_CONFIG_DIR") };
        assert!(out.contains("\tSID\t") && out.contains("\tHSID\t") && out.contains("\tLSID\t"));
        assert!(
            !out.contains("chatgpt")
                && !out.contains("notgoogle")
                && !out.contains("evil-youtube")
                && !out.contains("GMAIL")
        );
        assert!(!dir.join("signin-export").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode =
                |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&dir.join("cookies.txt")), 0o600);
            assert_eq!(mode(&dir), 0o700);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
