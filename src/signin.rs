//! "Sign in with Google" using the browser session you already have.
//!
//! We find the browser profile you used most recently (newest cookie
//! database), and:
//!   1. if it's already signed in to YouTube, import its cookies right away;
//!   2. otherwise open Google's sign-in page in that same browser and profile,
//!      watch its cookie database until the sign-in cookies appear, then import.
//!
//! Why not an in-app login: Google offers no sign-in route for third-party
//! YouTube clients (the TV device flow was blocked in 2024) and blocks
//! sign-in inside embedded web views.
//!
//! Detection only reads cookie *names* from a copy of the database — never
//! values — so it never triggers a Keychain prompt. The import (yt-dlp) does
//! decrypt Chromium cookies, so Chrome & co. ask for Keychain access once.

use anyhow::{Context, Result};
use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::SystemTime,
};

const SIGN_IN_URL: &str =
    "https://accounts.google.com/ServiceLogin?service=youtube&continue=https%3A%2F%2Fwww.youtube.com%2F";

/// Cookies Google sets only for a signed-in session.
const LOGIN_COOKIES: &[&str] = &["SAPISID", "__Secure-3PAPISID", "LOGIN_INFO"];

/// One browser profile with a cookie database.
#[derive(Clone, Debug, PartialEq)]
pub struct Session {
    /// "Firefox", "Chrome", …
    pub browser: &'static str,
    /// yt-dlp browser id
    pub id: &'static str,
    pub firefox: bool,
    /// The profile folder ("…/Profiles/abcd.default-release", "…/Chrome/Default").
    pub profile: PathBuf,
    /// The cookie database inside it.
    db: PathBuf,
    modified: SystemTime,
}

/// All profiles with cookies, most recently used first.
pub fn sessions() -> Vec<Session> {
    let mut out = Vec::new();
    for b in crate::yt::ALL_BROWSERS {
        let firefox = match b.family {
            crate::yt::Family::Firefox => true,
            crate::yt::Family::Chromium => false,
            crate::yt::Family::Safari => continue, // binary cookie file, needs Full Disk Access
        };
        for root in crate::yt::profile_dirs(b.id) {
            // Firefox: <root>/Profiles/<p> (macOS, Windows) or <root>/<p> (Linux).
            // Chromium: <root>/Default, <root>/Profile 1, …
            let mut parents = vec![root.clone()];
            if firefox {
                parents.push(root.join("Profiles"));
            }
            for parent in parents {
                let Ok(rd) = std::fs::read_dir(&parent) else { continue };
                for e in rd.flatten() {
                    let profile = e.path();
                    let dbs: &[&str] = if firefox { &["cookies.sqlite"] } else { &["Network/Cookies", "Cookies"] };
                    let Some(db) = dbs.iter().map(|r| profile.join(r)).find(|p| p.is_file()) else { continue };
                    let modified = newest_mtime(&db);
                    out.push(Session { browser: b.name, id: b.id, firefox, profile, db, modified });
                }
            }
        }
    }
    out.sort_by_key(|s| std::cmp::Reverse(s.modified));
    out
}

/// A previously used profile, if it still exists.
pub fn find(id: &str, profile: &Path) -> Option<Session> {
    sessions().into_iter().find(|s| s.id == id && s.profile == profile)
}

/// Pending writes land in `-wal`, so count it as "used".
fn newest_mtime(db: &Path) -> SystemTime {
    ["", "-wal", "-journal"]
        .iter()
        .filter_map(|s| std::fs::metadata(format!("{}{s}", db.display())).and_then(|m| m.modified()).ok())
        .max()
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

impl Session {
    /// "Firefox (default-release)" / "Chrome (Profile 1)"
    pub fn label(&self) -> String {
        let p = self.profile.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let p = p.split_once('.').map_or(p.as_str(), |(_, rest)| rest).to_string(); // Firefox "xyz12.default"
        format!("{} ({p})", self.browser)
    }

    /// Copy the cookie database (plus its write-ahead log) into `dst` and fold
    /// pending writes in. Returns the copy's path.
    ///
    /// Browsers keep recent cookie changes in a SQLite write-ahead log until
    /// they shut down; yt-dlp copies only the main file, so it would miss a
    /// fresh sign-in. Opening the copy with SQLite applies the log.
    fn snapshot_to(&self, dst: &Path) -> Result<PathBuf> {
        let rel = self.db.strip_prefix(&self.profile).context("cookie db outside profile")?;
        let name = self.profile.file_name().context("profile has no name")?;
        let to = dst.join(name).join(rel);
        let _ = std::fs::remove_dir_all(dst);
        std::fs::create_dir_all(to.parent().context("bad path")?)?;
        for suffix in ["", "-wal", "-journal"] {
            let f = PathBuf::from(format!("{}{suffix}", self.db.display()));
            if f.is_file() {
                std::fs::copy(&f, format!("{}{suffix}", to.display()))
                    .with_context(|| format!("copying {}'s cookies (close it and retry)", self.browser))?;
            }
        }
        crate::yt::restrict_perms(&to)?;
        let db = rusqlite::Connection::open(&to).context("opening cookie database copy")?;
        let _ = db.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()));
        Ok(to)
    }

    /// Is this profile signed in to Google? Checks cookie names only.
    pub fn has_login(&self) -> Result<bool> {
        let dir = crate::yt::config_dir()?.join("signin-check");
        let db = self.snapshot_to(&dir)?;
        let (table, host) = if self.firefox { ("moz_cookies", "host") } else { ("cookies", "host_key") };
        let names = LOGIN_COOKIES.iter().map(|n| format!("'{n}'")).collect::<Vec<_>>().join(",");
        let sql = format!(
            "select count(*) from {table} where name in ({names}) \
             and ({host} like '%youtube.com' or {host} like '%google.com')"
        );
        let n: i64 = {
            let conn = rusqlite::Connection::open(&db)?;
            conn.query_row(&sql, [], |r| r.get(0)).context("reading cookie names")?
        };
        let _ = std::fs::remove_dir_all(&dir);
        Ok(n > 0)
    }

    /// Snapshot the profile and return a `--cookies-from-browser` spec for it.
    /// Remove the snapshot with [`clear_snapshot`] after importing.
    pub fn snapshot_spec(&self) -> Result<String> {
        let dir = crate::yt::config_dir()?.join("signin-snapshot");
        self.snapshot_to(&dir)?;
        if !self.firefox {
            // Windows Chromium keeps the cookie key in "Local State" next to the profiles.
            if let Some(ls) = self.profile.parent().map(|p| p.join("Local State")).filter(|p| p.is_file()) {
                let _ = std::fs::copy(ls, dir.join("Local State"));
            }
        }
        let p = if self.firefox { dir } else { dir.join(self.profile.file_name().context("profile has no name")?) };
        Ok(format!("{}:{}", self.id, p.display()))
    }

    /// Open Google sign-in in this browser and profile (joins the running
    /// browser if it's open).
    pub fn open_signin(&self) -> Result<()> {
        let mut c = if cfg!(target_os = "macos") && self.firefox {
            // `open` hands the URL to a running Firefox; the binary would refuse.
            let mut c = Command::new("open");
            c.args(["-a", "Firefox", SIGN_IN_URL]);
            c
        } else {
            let exe = exe(self.id).with_context(|| format!("couldn't find the {} app", self.browser))?;
            let mut c = Command::new(exe);
            if !self.firefox {
                if let Some(name) = self.profile.file_name() {
                    c.arg(format!("--profile-directory={}", name.to_string_lossy()));
                }
            }
            c.arg(SIGN_IN_URL);
            c
        };
        c.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        let mut child = c.spawn().with_context(|| format!("opening {}", self.browser))?;
        // Reap in the background (it either exits at once, handing off to the
        // running browser, or *is* the browser).
        std::thread::spawn(move || child.wait());
        Ok(())
    }
}

pub fn clear_snapshot() {
    if let Ok(d) = crate::yt::config_dir() {
        let _ = std::fs::remove_dir_all(d.join("signin-snapshot"));
    }
}

/// The browser's executable.
fn exe(id: &str) -> Option<PathBuf> {
    let home = dirs::home_dir().unwrap_or_default();
    let cands: Vec<PathBuf> = if cfg!(target_os = "macos") {
        let (app, bin) = match id {
            "chrome" => ("Google Chrome", "Google Chrome"),
            "brave" => ("Brave Browser", "Brave Browser"),
            "edge" => ("Microsoft Edge", "Microsoft Edge"),
            "chromium" => ("Chromium", "Chromium"),
            "opera" => ("Opera", "Opera"),
            "vivaldi" => ("Vivaldi", "Vivaldi"),
            "firefox" => ("Firefox", "firefox"),
            _ => return None,
        };
        vec![
            PathBuf::from(format!("/Applications/{app}.app/Contents/MacOS/{bin}")),
            home.join(format!("Applications/{app}.app/Contents/MacOS/{bin}")),
        ]
    } else if cfg!(windows) {
        let rel = match id {
            "chrome" => "Google\\Chrome\\Application\\chrome.exe",
            "edge" => "Microsoft\\Edge\\Application\\msedge.exe",
            "brave" => "BraveSoftware\\Brave-Browser\\Application\\brave.exe",
            "firefox" => "Mozilla Firefox\\firefox.exe",
            _ => return None,
        };
        let local = dirs::data_local_dir().unwrap_or_default();
        ["C:\\Program Files", "C:\\Program Files (x86)"]
            .iter()
            .map(|b| PathBuf::from(format!("{b}\\{rel}")))
            .chain(std::iter::once(local.join(rel)))
            .collect()
    } else {
        let names: &[&str] = match id {
            "chrome" => &["google-chrome", "google-chrome-stable"],
            "chromium" => &["chromium", "chromium-browser"],
            "brave" => &["brave-browser", "brave"],
            "edge" => &["microsoft-edge"],
            "vivaldi" => &["vivaldi"],
            "opera" => &["opera"],
            "firefox" => &["firefox"],
            _ => return None,
        };
        let dirs = ["/usr/bin", "/usr/local/bin", "/snap/bin"];
        names.iter().flat_map(|n| dirs.iter().map(move |d| PathBuf::from(d).join(n))).collect()
    };
    cands.into_iter().find(|p| p.is_file())
}

#[cfg(test)]
mod live {
    use super::*;

    /// Finds your most recent browser profile and reports whether it's signed
    /// in; if so, imports from a snapshot (Chrome: one Keychain prompt).
    /// `cargo test -- --ignored current_session --nocapture`
    #[test]
    #[ignore]
    fn current_session() {
        let all = sessions();
        for s in &all {
            eprintln!("{} — {}", s.label(), s.profile.display());
        }
        let s = all.first().expect("a browser profile");
        let logged = s.has_login().unwrap();
        eprintln!("current: {} signed in: {logged}", s.label());
        // Importing Chromium cookies asks for Keychain access, so opt in.
        if logged && (s.firefox || std::env::var_os("YTMINI_IMPORT").is_some()) {
            let tools = crate::yt::Tools::locate().unwrap();
            let out = std::env::temp_dir().join("ytmini-signin-test-cookies.txt");
            let spec = s.snapshot_spec().unwrap();
            crate::yt::import_cookies(&tools, &spec, &out).unwrap();
            clear_snapshot();
            assert!(crate::yt::logged_in(&out));
            let _ = std::fs::remove_file(out);
        }
    }
}
