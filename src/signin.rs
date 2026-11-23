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
//! values — so it never triggers a Keychain prompt. The import reads the
//! cookie store directly (no yt-dlp): Firefox plain, Chromium-family on macOS
//! decrypted with the Keychain "Safe Storage" key (one prompt), Safari's
//! binary cookie file (needs Full Disk Access).

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

/// Google's rotating sign-in cookies. A browser with a YouTube tab open
/// replaces them every few minutes, and the values it replaced stop working
/// soon after, for ytmini's copy too (yt-dlp's FAQ describes the same).
const ROTATING: &[&str] = &["__Secure-1PSIDTS", "__Secure-3PSIDTS"];

/// A fingerprint of the rotating cookies set for `hosts`: a hash of their
/// values, so the values themselves never leave the function that read them.
fn rotation_mark<'a>(
    rows: impl Iterator<Item = (&'a str, &'a str, &'a str)>,
    hosts: &[&str],
) -> Option<u64> {
    use sha2::{Digest, Sha256};
    let mut picked: Vec<_> = rows
        .filter(|(host, name, _)| {
            ROTATING.contains(name) && hosts.contains(&host.trim_start_matches('.'))
        })
        .collect();
    if picked.is_empty() {
        return None;
    }
    picked.sort();
    let mut h = Sha256::new();
    for (host, name, value) in picked {
        h.update(host.trim_start_matches('.').as_bytes());
        h.update([0]);
        h.update(name.as_bytes());
        h.update([0]);
        h.update(value.as_bytes());
        h.update([0]);
    }
    Some(u64::from_be_bytes(h.finalize()[..8].try_into().ok()?))
}

/// The browser's rotating cookies: for google.com, which only an import
/// changes in ytmini's copy, and for google.com and youtube.com together,
/// which only the browser changes in its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BrowserMark {
    pub google: u64,
    pub all: u64,
}

fn browser_mark_of<'a>(
    rows: impl Iterator<Item = (&'a str, &'a str, &'a str)> + Clone,
) -> Option<BrowserMark> {
    Some(BrowserMark {
        google: rotation_mark(rows.clone(), &["google.com"])?,
        all: rotation_mark(rows, &["google.com", "youtube.com"])?,
    })
}

/// [`rotation_mark`] of the google.com cookies in ytmini's own cookies.txt
/// (compare with [`BrowserMark::google`]).
pub fn jar_mark(jar: &Path) -> Option<u64> {
    let text = std::fs::read_to_string(jar).ok()?;
    let rows: Vec<(&str, &str, &str)> = text
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l
                .strip_prefix("#HttpOnly_")
                .unwrap_or(l)
                .split('\t')
                .collect();
            (f.len() >= 7).then(|| (f[0], f[5], f[6]))
        })
        .collect();
    rotation_mark(rows.into_iter(), &["google.com"])
}

/// One browser profile with a cookie database.
#[derive(Clone, Debug, PartialEq)]
pub struct Session {
    /// "Firefox", "Chrome", …
    pub browser: &'static str,
    /// yt-dlp browser id
    pub id: &'static str,
    pub firefox: bool,
    pub safari: bool,
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
            // decryption: macOS Keychain, Linux keyring; Windows' app-bound
            // encryption can't be read from outside the browser
            crate::yt::Family::Chromium if cfg!(any(target_os = "macos", target_os = "linux")) => {
                false
            }
            crate::yt::Family::Chromium => continue,
            crate::yt::Family::Safari => {
                if let Some(db) = safari_cookie_file().filter(|f| std::fs::File::open(f).is_ok()) {
                    let profile = db.parent().map(Path::to_path_buf).unwrap_or_default();
                    let modified = newest_mtime(&db);
                    out.push(Session {
                        browser: b.name,
                        id: b.id,
                        firefox: false,
                        safari: true,
                        profile,
                        db,
                        modified,
                    });
                }
                continue;
            }
        };
        for root in crate::yt::profile_dirs(b.id) {
            // Firefox: <root>/Profiles/<p> (macOS, Windows) or <root>/<p> (Linux).
            // Chromium: <root>/Default, <root>/Profile 1, …
            let mut parents = vec![root.clone()];
            if firefox {
                parents.push(root.join("Profiles"));
            }
            for parent in parents {
                let Ok(rd) = std::fs::read_dir(&parent) else {
                    continue;
                };
                for e in rd.flatten() {
                    let profile = e.path();
                    let dbs: &[&str] = if firefox {
                        &["cookies.sqlite"]
                    } else {
                        &["Network/Cookies", "Cookies"]
                    };
                    let Some(db) = dbs.iter().map(|r| profile.join(r)).find(|p| p.is_file()) else {
                        continue;
                    };
                    let modified = newest_mtime(&db);
                    out.push(Session {
                        browser: b.name,
                        id: b.id,
                        firefox,
                        safari: false,
                        profile,
                        db,
                        modified,
                    });
                }
            }
        }
    }
    out.sort_by_key(|s| std::cmp::Reverse(s.modified));
    out
}

/// Safari's cookie file (macOS). Readable only with Full Disk Access.
fn safari_cookie_file() -> Option<PathBuf> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let home = dirs::home_dir()?;
    [
        "Library/Containers/com.apple.Safari/Data/Library/Cookies/Cookies.binarycookies",
        "Library/Cookies/Cookies.binarycookies",
    ]
    .iter()
    .map(|r| home.join(r))
    .find(|p| {
        std::fs::metadata(p).is_ok()
            || std::fs::File::open(p)
                .is_err_and(|e| e.kind() == std::io::ErrorKind::PermissionDenied)
    })
}

/// Safari is installed but macOS won't let us read its cookies (no Full Disk Access).
pub fn safari_needs_access() -> bool {
    cfg!(target_os = "macos")
        && std::path::Path::new("/Applications/Safari.app").exists()
        && !sessions().iter().any(|s| s.safari)
}

/// A previously used profile, if it still exists.
pub fn find(id: &str, profile: &Path) -> Option<Session> {
    sessions()
        .into_iter()
        .find(|s| s.id == id && s.profile == profile)
}

/// Pending writes land in `-wal`, so count it as "used".
fn newest_mtime(db: &Path) -> SystemTime {
    ["", "-wal", "-journal"]
        .iter()
        .filter_map(|s| {
            std::fs::metadata(format!("{}{s}", db.display()))
                .and_then(|m| m.modified())
                .ok()
        })
        .max()
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

impl Session {
    /// "Firefox (default-release)" / "Chrome (Profile 1)"
    pub fn label(&self) -> String {
        if self.safari {
            return "Safari".into();
        }
        let p = self
            .profile
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let p = p
            .split_once('.')
            .map_or(p.as_str(), |(_, rest)| rest)
            .to_string(); // Firefox "xyz12.default"
        format!("{} ({p})", self.browser)
    }

    /// Copy the cookie database (plus its write-ahead log) into `dst` and fold
    /// pending writes in. Returns the copy's path.
    ///
    /// Browsers keep recent cookie changes in a SQLite write-ahead log until
    /// they shut down; yt-dlp copies only the main file, so it would miss a
    /// fresh sign-in. Opening the copy with SQLite applies the log.
    fn snapshot_to(&self, dst: &Path) -> Result<PathBuf> {
        let rel = self
            .db
            .strip_prefix(&self.profile)
            .context("cookie db outside profile")?;
        let name = self.profile.file_name().context("profile has no name")?;
        let to = dst.join(name).join(rel);
        let _ = std::fs::remove_dir_all(dst);
        std::fs::create_dir_all(to.parent().context("bad path")?)?;
        for suffix in ["", "-wal", "-journal"] {
            let f = PathBuf::from(format!("{}{suffix}", self.db.display()));
            if f.is_file() {
                std::fs::copy(&f, format!("{}{suffix}", to.display())).with_context(|| {
                    format!("copying {}'s cookies (close it and retry)", self.browser)
                })?;
            }
        }
        crate::yt::restrict_perms(&to)?;
        let db = rusqlite::Connection::open(&to).context("opening cookie database copy")?;
        let _ = db.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()));
        Ok(to)
    }

    /// Is this profile signed in to Google? Checks cookie names only.
    pub fn has_login(&self) -> Result<bool> {
        if self.safari {
            let rows = safari_cookies(&self.db)?;
            return Ok(rows
                .iter()
                .any(|c| LOGIN_COOKIES.contains(&c.name.as_str())));
        }
        let dir = crate::yt::config_dir()?.join("signin-check");
        let _cleanup = RemoveOnDrop(dir.clone());
        let db = self.snapshot_to(&dir)?;
        let (table, host) = if self.firefox {
            ("moz_cookies", "host")
        } else {
            ("cookies", "host_key")
        };
        let names = LOGIN_COOKIES
            .iter()
            .map(|n| format!("'{n}'"))
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "select count(*) from {table} where name in ({names}) \
             and ({host} like '%youtube.com' or {host} like '%google.com')"
        );
        let n: i64 = {
            let conn = rusqlite::Connection::open(&db)?;
            conn.query_row(&sql, [], |r| r.get(0))
                .context("reading cookie names")?
        };
        Ok(n > 0)
    }

    /// Can ytmini read this profile's cookies again without a prompt?
    /// (Chromium browsers on macOS ask for Keychain access.)
    pub fn reads_silently(&self) -> bool {
        self.firefox || self.safari
    }

    /// [`rotation_mark`] of this profile's cookies right now. Only for
    /// profiles that [`reads_silently`](Self::reads_silently).
    pub fn browser_mark(&self) -> Result<Option<BrowserMark>> {
        if self.safari {
            let rows = safari_cookies(&self.db)?;
            return Ok(browser_mark_of(
                rows.iter()
                    .map(|c| (c.host.as_str(), c.name.as_str(), c.value.as_str())),
            ));
        }
        anyhow::ensure!(
            self.firefox,
            "{} can't be read without a prompt",
            self.browser
        );
        let dir = crate::yt::config_dir()?.join("signin-mark");
        let _cleanup = RemoveOnDrop(dir.clone());
        let db = self.snapshot_to(&dir)?;
        let conn = rusqlite::Connection::open(&db)?;
        let mut st = conn.prepare(
            "select host, name, value from moz_cookies \
             where name in ('__Secure-1PSIDTS', '__Secure-3PSIDTS')",
        )?;
        let rows: Vec<(String, String, String)> = st
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(browser_mark_of(
            rows.iter()
                .map(|(h, n, v)| (h.as_str(), n.as_str(), v.as_str())),
        ))
    }

    /// Open Google sign-in in this browser and profile (joins the running
    /// browser if it's open).
    pub fn open_signin(&self) -> Result<()> {
        let mut c = if cfg!(target_os = "macos") && (self.firefox || self.safari) {
            // `open` hands the URL to a running Firefox; the binary would refuse.
            let mut c = Command::new("open");
            c.args([
                "-a",
                if self.safari { "Safari" } else { self.browser },
                SIGN_IN_URL,
            ]);
            c
        } else {
            let exe =
                exe(self.id).with_context(|| format!("couldn't find the {} app", self.browser))?;
            let mut c = Command::new(exe);
            if !self.firefox {
                if let Some(name) = self.profile.file_name() {
                    c.arg(format!("--profile-directory={}", name.to_string_lossy()));
                }
            }
            c.arg(SIGN_IN_URL);
            c
        };
        c.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = c
            .spawn()
            .with_context(|| format!("opening {}", self.browser))?;
        // Reap in the background (it either exits at once, handing off to the
        // running browser, or *is* the browser).
        std::thread::spawn(move || child.wait());
        Ok(())
    }
}

impl Session {
    /// Write this profile's YouTube/Google cookies to `dest` (Netscape
    /// cookies.txt, mode 600). Firefox: plain reads. Chromium on macOS: the
    /// "Safe Storage" key from the Keychain (one prompt) decrypts the values.
    /// Safari: its binary cookie file. Fails if the result isn't signed in.
    pub fn export_cookies(&self, dest: &Path) -> Result<()> {
        if self.safari {
            let rows: Vec<Cookie> = safari_cookies(&self.db)?
                .into_iter()
                .filter(|c| c.host.ends_with("youtube.com") || c.host.ends_with("google.com"))
                .collect();
            return write_jar(&rows, dest, &self.label());
        }
        let dir = crate::yt::config_dir()?.join("signin-export");
        let _cleanup = RemoveOnDrop(dir.clone());
        let db = self.snapshot_to(&dir)?;
        self.export_from(&db, dest)
    }

    fn export_from(&self, db: &Path, dest: &Path) -> Result<()> {
        let conn = rusqlite::Connection::open(db)?;
        let filter = "(h like '%youtube.com' or h like '%google.com')";
        let mut rows: Vec<Cookie> = Vec::new();
        if self.firefox {
            let sql = format!(
                "select host as h, path, isSecure, expiry, name, value, isHttpOnly from moz_cookies where {filter}"
            );
            let mut st = conn.prepare(&sql)?;
            let it = st.query_map([], |r| {
                Ok(Cookie {
                    host: r.get(0)?,
                    path: r.get(1)?,
                    secure: r.get::<_, i64>(2)? != 0,
                    // Firefox ≥ 133 stores milliseconds
                    expiry: {
                        let e: i64 = r.get(3)?;
                        if e > 100_000_000_000 {
                            e / 1000
                        } else {
                            e
                        }
                    },
                    name: r.get(4)?,
                    value: r.get(5)?,
                    http_only: r.get::<_, i64>(6)? != 0,
                })
            })?;
            rows.extend(it.flatten());
        } else {
            // DB schema ≥ 24 prefixes each plaintext with SHA-256(host) (32 bytes).
            let version: i64 = conn
                .query_row("select value from meta where key = 'version'", [], |r| {
                    r.get::<_, String>(0)
                        .map(|v| v.parse().unwrap_or(0))
                        .or_else(|_| r.get(0))
                })
                .unwrap_or(0);
            let sql = format!(
                "select host_key as h, path, is_secure, expires_utc, name, value, encrypted_value, is_httponly from cookies where {filter}"
            );
            let mut st = conn.prepare(&sql)?;
            let raw: Vec<(Cookie, Vec<u8>)> = st
                .query_map([], |r| {
                    // microseconds since 1601-01-01 → unix seconds (0 = session cookie)
                    let us: i64 = r.get(3)?;
                    let expiry = if us == 0 {
                        0
                    } else {
                        us / 1_000_000 - 11_644_473_600
                    };
                    Ok((
                        Cookie {
                            host: r.get(0)?,
                            path: r.get(1)?,
                            secure: r.get::<_, i64>(2)? != 0,
                            expiry,
                            name: r.get(4)?,
                            value: r.get(5)?,
                            http_only: r.get::<_, i64>(7)? != 0,
                        },
                        r.get(6)?,
                    ))
                })?
                .collect::<rusqlite::Result<_>>()?;
            // Linux "v11" values need the keyring's password: ask only then
            let need_v11 = raw
                .iter()
                .any(|(c, e)| c.value.is_empty() && e.starts_with(b"v11"));
            let keys = chromium_keys(self.id, need_v11)?;
            let mut v11_failed = 0;
            for (mut c, enc) in raw {
                if c.value.is_empty() {
                    match keys.decrypt(&enc, version >= 24) {
                        Some(v) => c.value = v,
                        None => {
                            v11_failed += usize::from(enc.starts_with(b"v11"));
                            continue;
                        }
                    }
                }
                rows.push(c);
            }
            if v11_failed > 0
                && !rows
                    .iter()
                    .any(|c| LOGIN_COOKIES.contains(&c.name.as_str()))
            {
                if let Some(e) = &keys.keyring_error {
                    anyhow::bail!("couldn't read {}'s key from the keyring: {e}", self.browser);
                }
            }
        }
        write_jar(&rows, dest, &self.label())
    }
}

fn write_jar(rows: &[Cookie], dest: &Path, label: &str) -> Result<()> {
    {
        let mut out =
            String::from("# Netscape HTTP Cookie File\n# Exported by ytmini. Do not share.\n\n");
        // Only Google's and YouTube's cookies leave the browser.
        for c in rows
            .iter()
            .filter(|c| crate::yt::wanted_cookie_host(&c.host))
        {
            let sub = if c.host.starts_with('.') {
                "TRUE"
            } else {
                "FALSE"
            };
            let prefix = if c.http_only { "#HttpOnly_" } else { "" };
            out.push_str(&format!(
                "{prefix}{}\t{sub}\t{}\t{}\t{}\t{}\t{}\n",
                c.host,
                c.path,
                if c.secure { "TRUE" } else { "FALSE" },
                c.expiry.max(0),
                c.name,
                c.value
            ));
        }
        // Check the login on a private temp file before replacing the jar.
        let tmp = dest.with_extension("txt.new");
        let check = dest.with_extension("txt.chk");
        crate::yt::write_private(&tmp, &check, out.as_bytes())?;
        if !crate::yt::logged_in(&check) {
            let _ = std::fs::remove_file(&check);
            anyhow::bail!("no YouTube login found in {label}");
        }
        std::fs::rename(&check, dest).context("saving cookies")?;
        Ok(())
    }
}

/// Deletes a browser-database snapshot however the function using it exits
/// (error, early return, panic): it holds every site's cookies.
struct RemoveOnDrop(PathBuf);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Cookie {
    host: String,
    path: String,
    secure: bool,
    expiry: i64,
    name: String,
    value: String,
    http_only: bool,
}

/// Keys for a Chromium browser's encrypted cookie values: "v10" values use
/// `v10`; "v11" (Linux, keyring-backed) try each of `v11`.
struct ChromiumKeys {
    v10: [u8; 16],
    v11: Vec<[u8; 16]>,
    /// Why the keyring's password couldn't be read (Linux), for the error.
    keyring_error: Option<String>,
}

impl ChromiumKeys {
    fn decrypt(&self, enc: &[u8], hashed_host: bool) -> Option<String> {
        if enc.starts_with(b"v11") {
            self.v11
                .iter()
                .find_map(|k| decrypt_chromium(k, enc, hashed_host))
        } else {
            decrypt_chromium(&self.v10, enc, hashed_host)
        }
    }
}

/// PBKDF2-SHA1 over "saltysalt", as Chromium derives its cookie key.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn chromium_derive(password: &[u8], rounds: u32) -> [u8; 16] {
    let mut key = [0u8; 16];
    pbkdf2::pbkdf2_hmac::<sha1::Sha1>(password, b"saltysalt", rounds, &mut key);
    key
}

#[cfg(target_os = "macos")]
fn chromium_keys(id: &str, _need_v11: bool) -> Result<ChromiumKeys> {
    Ok(ChromiumKeys {
        v10: chromium_key(id)?,
        v11: Vec::new(),
        keyring_error: None,
    })
}

/// Linux: "v10" uses the fixed password "peanuts"; "v11" uses the
/// "<Browser> Safe Storage" password from the desktop keyring (GNOME Keyring,
/// KWallet, KeePassXC… through the Secret Service, or kwallet-query), or an
/// empty one when the browser runs with `--password-store=basic`.
#[cfg(target_os = "linux")]
fn chromium_keys(id: &str, need_v11: bool) -> Result<ChromiumKeys> {
    let mut keys = ChromiumKeys {
        v10: chromium_derive(b"peanuts", 1),
        v11: Vec::new(),
        keyring_error: None,
    };
    if need_v11 {
        match keyring::password(id) {
            Ok(p) => keys.v11.push(chromium_derive(p.as_bytes(), 1)),
            Err(e) => keys.keyring_error = Some(format!("{e:#}")),
        }
        keys.v11.push(chromium_derive(b"", 1));
    }
    Ok(keys)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn chromium_keys(id: &str, _need_v11: bool) -> Result<ChromiumKeys> {
    anyhow::bail!("reading {id}'s cookies isn't supported on this system")
}

/// The keyring entry Chromium browsers keep their cookie password in.
#[cfg(target_os = "linux")]
mod keyring {
    use anyhow::{Context, Result};

    /// "Chrome" → items labelled "Chrome Safe Storage", KWallet folder "Chrome Keys".
    fn browser_name(id: &str) -> Result<&'static str> {
        Ok(match id {
            "chrome" => "Chrome",
            "chromium" => "Chromium",
            "brave" => "Brave",
            "edge" => "Microsoft Edge",
            "opera" => "Opera",
            "vivaldi" => "Vivaldi",
            _ => anyhow::bail!("unknown Chromium browser {id}"),
        })
    }

    pub fn password(id: &str) -> Result<String> {
        let name = browser_name(id)?;
        let label = format!("{name} Safe Storage");
        let ss = secret_service(&label);
        if let Ok(Some(p)) = &ss {
            return Ok(p.clone());
        }
        if let Some(p) = kwallet(name, &label) {
            return Ok(p);
        }
        match ss {
            Err(e) => Err(e),
            _ => anyhow::bail!("no \"{label}\" entry in the keyring"),
        }
    }

    /// Secret Service (D-Bus): the default collection first, unlocking it
    /// if needed (the desktop may ask for the keyring password once).
    fn secret_service(label: &str) -> Result<Option<String>> {
        use secret_service::{blocking::SecretService, EncryptionType};
        let ss = SecretService::connect(EncryptionType::Plain)
            .context("connecting to the Secret Service")?;
        let mut collections = Vec::new();
        if let Ok(c) = ss.get_default_collection() {
            collections.push(c);
        }
        collections.extend(ss.get_all_collections().unwrap_or_default());
        for c in &collections {
            if c.is_locked().unwrap_or(true) && c.unlock().is_err() {
                continue;
            }
            for item in c.get_all_items().unwrap_or_default() {
                if item.get_label().ok().as_deref() == Some(label) {
                    let secret = item.get_secret().context("reading the keyring entry")?;
                    return Ok(Some(String::from_utf8_lossy(&secret).into_owned()));
                }
            }
        }
        Ok(None)
    }

    /// KWallet 5 without Secret Service: its CLI.
    fn kwallet(name: &str, label: &str) -> Option<String> {
        let out = std::process::Command::new("kwallet-query")
            .args([
                "--read-password",
                label,
                "--folder",
                &format!("{name} Keys"),
                "kdewallet",
            ])
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()?;
        let p = String::from_utf8_lossy(&out.stdout).trim_end().to_string();
        (out.status.success() && !p.is_empty() && !p.starts_with("Failed to read")).then_some(p)
    }
}

/// AES key for a Chromium browser's cookies on macOS: PBKDF2 over the
/// "<Browser> Safe Storage" Keychain password (asks the user once).
#[cfg(target_os = "macos")]
fn chromium_key(id: &str) -> Result<[u8; 16]> {
    let (service, account) = match id {
        "chrome" => ("Chrome Safe Storage", "Chrome"),
        "brave" => ("Brave Safe Storage", "Brave"),
        "edge" => ("Microsoft Edge Safe Storage", "Microsoft Edge"),
        "chromium" => ("Chromium Safe Storage", "Chromium"),
        "opera" => ("Opera Safe Storage", "Opera"),
        "vivaldi" => ("Vivaldi Safe Storage", "Vivaldi"),
        _ => anyhow::bail!("unknown Chromium browser {id}"),
    };
    let out = Command::new("security")
        .args(["find-generic-password", "-w", "-s", service, "-a", account])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .context("asking the Keychain")?;
    if !out.status.success() {
        anyhow::bail!("Keychain access to \"{service}\" was denied");
    }
    let password = String::from_utf8_lossy(&out.stdout).trim_end().to_string();
    Ok(chromium_derive(password.as_bytes(), 1003))
}

/// "v10"/"v11" + AES-128-CBC (IV of 16 spaces, PKCS#7).
fn decrypt_chromium(key: &[u8; 16], enc: &[u8], hashed_host: bool) -> Option<String> {
    use aes::cipher::{block_padding::Pkcs7, BlockModeDecrypt, KeyIvInit};
    let body = enc
        .strip_prefix(b"v10")
        .or_else(|| enc.strip_prefix(b"v11"))?;
    let mut buf = body.to_vec();
    let dec = cbc::Decryptor::<aes::Aes128>::new(key.into(), &[b' '; 16].into());
    let plain = dec.decrypt_padded::<Pkcs7>(&mut buf).ok()?;
    let plain = if hashed_host && plain.len() >= 32 {
        &plain[32..]
    } else {
        plain
    };
    String::from_utf8(plain.to_vec()).ok()
}

/// Parse Safari's `Cookies.binarycookies`: "cook", big-endian page count and
/// sizes; each page lists cookies (little-endian) with flags, string offsets
/// and an expiry in Mac absolute time (seconds since 2001-01-01).
fn safari_cookies(path: &Path) -> Result<Vec<Cookie>> {
    let data = std::fs::read(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::PermissionDenied {
            anyhow::anyhow!(
                "macOS doesn't let ytmini read Safari's cookies. Allow it in System Settings → \
                 Privacy & Security → Full Disk Access, or use Firefox/Chrome"
            )
        } else {
            anyhow::anyhow!("reading Safari's cookies: {e}")
        }
    })?;
    parse_binarycookies(&data).context("Safari's cookie file has an unexpected format")
}

fn parse_binarycookies(d: &[u8]) -> Option<Vec<Cookie>> {
    let be = |o: usize| {
        d.get(o..o + 4)
            .map(|b| u32::from_be_bytes(b.try_into().unwrap()) as usize)
    };
    if d.get(0..4)? != b"cook" {
        return None;
    }
    let pages = be(4)?;
    let mut off = 8 + pages * 4;
    let mut out = Vec::new();
    for i in 0..pages {
        let size = be(8 + i * 4)?;
        let page = d.get(off..off + size)?;
        off += size;
        let le = |o: usize| {
            page.get(o..o + 4)
                .map(|b| u32::from_le_bytes(b.try_into().unwrap()) as usize)
        };
        let n = le(4)?;
        for k in 0..n {
            let c = le(8 + k * 4)?;
            let rec = page.get(c..c + le(c)?)?;
            let r32 = |o: usize| {
                rec.get(o..o + 4)
                    .map(|b| u32::from_le_bytes(b.try_into().unwrap()) as usize)
            };
            let f64at = |o: usize| {
                rec.get(o..o + 8)
                    .map(|b| f64::from_le_bytes(b.try_into().unwrap()))
            };
            let cstr = |o: usize| -> Option<String> {
                let s = rec.get(o..)?;
                let end = s.iter().position(|&b| b == 0)?;
                Some(String::from_utf8_lossy(&s[..end]).into_owned())
            };
            let flags = r32(8)?;
            out.push(Cookie {
                host: cstr(r32(16)?)?,
                name: cstr(r32(20)?)?,
                path: cstr(r32(24)?)?,
                value: cstr(r32(28)?)?,
                expiry: f64at(40)? as i64 + 978_307_200,
                secure: flags & 1 != 0,
                http_only: flags & 4 != 0,
            });
        }
    }
    Some(out)
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
            "librewolf" => ("LibreWolf", "librewolf"),
            "waterfox" => ("Waterfox", "waterfox"),
            "floorp" => ("Floorp", "floorp"),
            "zen" => ("Zen", "zen"),
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
            "librewolf" => "LibreWolf\\librewolf.exe",
            "waterfox" => "Waterfox\\waterfox.exe",
            "floorp" => "Ablaze Floorp\\floorp.exe",
            "zen" => "Zen Browser\\zen.exe",
            _ => return None,
        };
        let local = dirs::data_local_dir().unwrap_or_default();
        ["C:\\Program Files", "C:\\Program Files (x86)"]
            .iter()
            .map(|b| PathBuf::from(format!("{b}\\{rel}")))
            .chain(std::iter::once(local.join(rel)))
            .collect()
    } else {
        // command names, then the Flatpak app id
        let names: &[&str] = match id {
            "chrome" => &["google-chrome", "google-chrome-stable", "com.google.Chrome"],
            "chromium" => &["chromium", "chromium-browser", "org.chromium.Chromium"],
            "brave" => &["brave-browser", "brave", "com.brave.Browser"],
            "edge" => &[
                "microsoft-edge",
                "microsoft-edge-stable",
                "com.microsoft.Edge",
            ],
            "vivaldi" => &["vivaldi", "vivaldi-stable", "com.vivaldi.Vivaldi"],
            "opera" => &["opera", "com.opera.Opera"],
            "firefox" => &["firefox", "org.mozilla.firefox"],
            "librewolf" => &["librewolf", "io.gitlab.librewolf-community"],
            "waterfox" => &["waterfox", "net.waterfox.waterfox"],
            "floorp" => &["floorp", "one.ablaze.floorp"],
            "zen" => &["zen", "zen-browser", "app.zen_browser.zen"],
            _ => return None,
        };
        let flatpak_user = home.join(".local/share/flatpak/exports/bin");
        let dirs = [
            PathBuf::from("/usr/bin"),
            PathBuf::from("/usr/local/bin"),
            PathBuf::from("/snap/bin"),
            PathBuf::from("/var/lib/flatpak/exports/bin"),
            flatpak_user,
        ];
        names
            .iter()
            .flat_map(|n| dirs.iter().map(move |d| d.join(n)))
            .collect()
    };
    cands.into_iter().find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a one-page, one-cookie file in Safari's layout and parses it.
    #[test]
    fn safari_binarycookies() {
        let strings = [".youtube.com", "SAPISID", "/", "abc123"];
        let mut rec = vec![0u8; 56];
        let mut offs = Vec::new();
        for s in strings {
            offs.push(rec.len() as u32);
            rec.extend_from_slice(s.as_bytes());
            rec.push(0);
        }
        let len = rec.len() as u32;
        rec[0..4].copy_from_slice(&len.to_le_bytes());
        rec[8..12].copy_from_slice(&5u32.to_le_bytes()); // secure + httponly
        for (i, o) in offs.iter().enumerate() {
            rec[16 + i * 4..20 + i * 4].copy_from_slice(&o.to_le_bytes());
        }
        rec[40..48].copy_from_slice(&(800_000_000f64).to_le_bytes());
        let mut page = vec![0, 0, 1, 0];
        page.extend_from_slice(&1u32.to_le_bytes());
        page.extend_from_slice(&16u32.to_le_bytes()); // cookie offset
        page.extend_from_slice(&0u32.to_le_bytes());
        page.extend_from_slice(&rec);
        let mut file = b"cook".to_vec();
        file.extend_from_slice(&1u32.to_be_bytes());
        file.extend_from_slice(&(page.len() as u32).to_be_bytes());
        file.extend_from_slice(&page);
        let c = parse_binarycookies(&file).unwrap();
        assert_eq!(c.len(), 1);
        let c = &c[0];
        assert_eq!(
            (
                c.host.as_str(),
                c.name.as_str(),
                c.path.as_str(),
                c.value.as_str()
            ),
            (".youtube.com", "SAPISID", "/", "abc123")
        );
        assert!(c.secure && c.http_only);
        assert_eq!(c.expiry, 800_000_000 + 978_307_200);
        assert!(parse_binarycookies(b"nope").is_none());
    }

    #[test]
    fn chromium_decrypt_roundtrip() {
        use aes::cipher::{block_padding::Pkcs7, BlockModeEncrypt, KeyIvInit};
        let mut key = [0u8; 16];
        pbkdf2::pbkdf2_hmac::<sha1::Sha1>(b"test-password", b"saltysalt", 1003, &mut key);
        let enc = |plain: &[u8]| {
            let e = cbc::Encryptor::<aes::Aes128>::new((&key).into(), &[b' '; 16].into());
            let mut out = b"v10".to_vec();
            let mut buf = plain.to_vec();
            buf.resize(plain.len() + 16, 0);
            out.extend(e.encrypt_padded::<Pkcs7>(&mut buf, plain.len()).unwrap());
            out
        };
        assert_eq!(
            decrypt_chromium(&key, &enc(b"hello"), false).as_deref(),
            Some("hello")
        );
        let mut hashed = vec![7u8; 32];
        hashed.extend_from_slice(b"world");
        assert_eq!(
            decrypt_chromium(&key, &enc(&hashed), true).as_deref(),
            Some("world")
        );
        assert_eq!(decrypt_chromium(&key, b"v11garbage", false), None);
        // Linux: v11 values try the keyring key, then the empty password
        let keys = ChromiumKeys {
            v10: [0; 16],
            v11: vec![[1; 16], key],
            keyring_error: None,
        };
        let mut v11 = enc(b"linux");
        v11[..3].copy_from_slice(b"v11");
        assert_eq!(keys.decrypt(&v11, false).as_deref(), Some("linux"));
    }
}

#[cfg(test)]
mod live {
    use super::*;

    /// Native export of the current profile works for a signed-in API call. Chromium asks the
    /// Keychain, so it only runs with YTMINI_IMPORT set.
    /// `cargo test -- --ignored native_export --nocapture`
    #[test]
    #[ignore]
    fn native_export() {
        let s = sessions()
            .into_iter()
            .find(|s| s.firefox || std::env::var_os("YTMINI_IMPORT").is_some())
            .unwrap();
        // real cookies: only inside the private config dir
        let tmp = crate::yt::config_dir().unwrap().join("run");
        std::fs::create_dir_all(&tmp).unwrap();
        let a = tmp.join("ytmini-native.txt");
        let t = std::time::Instant::now();
        s.export_cookies(&a).unwrap();
        eprintln!(
            "native export of {}: {:.2}s",
            s.label(),
            t.elapsed().as_secs_f32()
        );
        let mut p = crate::innertube::Pager::new(&a, &crate::yt::Source::WatchLater).unwrap();
        assert!(
            !p.next_page().unwrap().unwrap().is_empty(),
            "signed-in call with native cookies"
        );
        let _ = std::fs::remove_file(a);
    }

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
            let out = crate::yt::config_dir()
                .unwrap()
                .join("run")
                .join("signin-test-cookies.txt");
            s.export_cookies(&out).unwrap();
            assert!(crate::yt::logged_in(&out));
            let _ = std::fs::remove_file(out);
        }
    }
}

#[cfg(test)]
mod rotation_tests {
    use super::*;

    #[test]
    fn mark_follows_only_the_rotating_google_cookies() {
        let a = [
            (".google.com", "__Secure-1PSIDTS", "one"),
            (".google.com", "__Secure-3PSIDTS", "three"),
            (".youtube.com", "__Secure-1PSIDTS", "yt"),
            (".google.com", "SID", "sid"),
        ];
        let m = browser_mark_of(a.iter().copied()).unwrap();
        // order and other cookies don't matter
        let mut b = a;
        b.reverse();
        b[0].2 = "other sid";
        assert_eq!(browser_mark_of(b.iter().copied()), Some(m));
        // a google.com rotation changes both marks
        let mut c = a;
        c[0].2 = "rotated";
        let mc = browser_mark_of(c.iter().copied()).unwrap();
        assert!(mc.google != m.google && mc.all != m.all);
        // a youtube.com one only the combined mark
        let mut d = a;
        d[2].2 = "yt rotated";
        let md = browser_mark_of(d.iter().copied()).unwrap();
        assert!(md.google == m.google && md.all != m.all);
        assert_eq!(
            browser_mark_of([(".google.com", "SID", "x")].into_iter()),
            None
        );
    }

    /// Read-only, against the signed-in browser: prints only whether the
    /// browser's mark matches ours. `cargo test -- --ignored live_browser_mark`
    #[test]
    #[ignore]
    fn live_browser_mark() {
        let s = sessions()
            .into_iter()
            .find(|s| s.reads_silently() && s.has_login().unwrap_or(false));
        let s = s.expect("a signed-in Firefox or Safari profile");
        let browser = s.browser_mark().unwrap();
        let ours = jar_mark(&crate::yt::cookie_file().unwrap());
        eprintln!(
            "{}: browser mark found {}, ours found {}, same {}",
            s.browser,
            browser.is_some(),
            ours.is_some(),
            browser.is_some() && browser.map(|b| b.google) == ours
        );
        assert!(browser.is_some());
    }

    #[test]
    fn jar_mark_reads_cookies_txt() {
        let jar = std::env::temp_dir().join(format!("ytmini-mark-{}.txt", std::process::id()));
        std::fs::write(
            &jar,
            "# Netscape HTTP Cookie File\n\n\
             #HttpOnly_.google.com\tTRUE\t/\tTRUE\t0\t__Secure-1PSIDTS\tone\n\
             .google.com\tTRUE\t/\tTRUE\t0\t__Secure-3PSIDTS\tthree\n",
        )
        .unwrap();
        let want = rotation_mark(
            [
                (".google.com", "__Secure-1PSIDTS", "one"),
                (".google.com", "__Secure-3PSIDTS", "three"),
            ]
            .into_iter(),
            &["google.com"],
        );
        assert_eq!(jar_mark(&jar), want);
        let _ = std::fs::remove_file(&jar);
    }
}
