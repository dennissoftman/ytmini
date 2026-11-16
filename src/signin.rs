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
        let dir = crate::yt::config_dir()?.join("signin-check");
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
            if let Some(ls) = self
                .profile
                .parent()
                .map(|p| p.join("Local State"))
                .filter(|p| p.is_file())
            {
                let _ = std::fs::copy(ls, dir.join("Local State"));
            }
        }
        let p = if self.firefox {
            dir
        } else {
            dir.join(self.profile.file_name().context("profile has no name")?)
        };
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
    /// cookies.txt, mode 600) without yt-dlp. Firefox: plain reads.
    /// Chromium on macOS: the "Safe Storage" key from the Keychain (one
    /// prompt) decrypts the values. Elsewhere Chromium is left to yt-dlp.
    /// Fails if the result isn't signed in.
    pub fn export_cookies(&self, dest: &Path) -> Result<()> {
        if !self.firefox && !cfg!(target_os = "macos") {
            anyhow::bail!("native Chromium cookie export is macOS-only");
        }
        let dir = crate::yt::config_dir()?.join("signin-export");
        let db = self.snapshot_to(&dir)?;
        let result = self.export_from(&db, dest);
        let _ = std::fs::remove_dir_all(&dir);
        result
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
            let key = chromium_key(self.id)?;
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
            let mut q = st.query([])?;
            while let Some(r) = q.next()? {
                let plain: String = r.get(5)?;
                let enc: Vec<u8> = r.get(6)?;
                let value = if !plain.is_empty() {
                    plain
                } else {
                    match decrypt_chromium(&key, &enc, version >= 24) {
                        Some(v) => v,
                        None => continue,
                    }
                };
                // microseconds since 1601-01-01 → unix seconds (0 = session cookie)
                let us: i64 = r.get(3)?;
                let expiry = if us == 0 {
                    0
                } else {
                    us / 1_000_000 - 11_644_473_600
                };
                rows.push(Cookie {
                    host: r.get(0)?,
                    path: r.get(1)?,
                    secure: r.get::<_, i64>(2)? != 0,
                    expiry,
                    name: r.get(4)?,
                    value,
                    http_only: r.get::<_, i64>(7)? != 0,
                });
            }
        }
        let mut out =
            String::from("# Netscape HTTP Cookie File\n# Exported by ytmini. Do not share.\n\n");
        for c in &rows {
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
        let tmp = dest.with_extension("txt.new");
        std::fs::write(&tmp, out)?;
        crate::yt::restrict_perms(&tmp)?;
        if !crate::yt::logged_in(&tmp) {
            let _ = std::fs::remove_file(&tmp);
            anyhow::bail!("no YouTube login found in {}", self.label());
        }
        std::fs::rename(&tmp, dest).context("saving cookies")?;
        Ok(())
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

/// AES key for a Chromium browser's cookies on macOS: PBKDF2 over the
/// "<Browser> Safe Storage" Keychain password (asks the user once).
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
    let mut key = [0u8; 16];
    pbkdf2::pbkdf2_hmac::<sha1::Sha1>(password.as_bytes(), b"saltysalt", 1003, &mut key);
    Ok(key)
}

/// "v10" + AES-128-CBC (IV of 16 spaces, PKCS#7).
fn decrypt_chromium(key: &[u8; 16], enc: &[u8], hashed_host: bool) -> Option<String> {
    use aes::cipher::{block_padding::Pkcs7, BlockModeDecrypt, KeyIvInit};
    let body = enc.strip_prefix(b"v10")?;
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
        names
            .iter()
            .flat_map(|n| dirs.iter().map(move |d| PathBuf::from(d).join(n)))
            .collect()
    };
    cands.into_iter().find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

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
    }
}

#[cfg(test)]
mod live {
    use super::*;

    /// Native export of the current profile vs yt-dlp's: same cookie names,
    /// and the native file works for a signed-in API call. Chromium asks the
    /// Keychain, so it only runs with YTMINI_IMPORT set.
    /// `cargo test -- --ignored native_export --nocapture`
    #[test]
    #[ignore]
    fn native_export() {
        let s = sessions()
            .into_iter()
            .find(|s| s.firefox || std::env::var_os("YTMINI_IMPORT").is_some())
            .unwrap();
        let tmp = std::env::temp_dir();
        let (a, b) = (tmp.join("ytmini-native.txt"), tmp.join("ytmini-ytdlp.txt"));
        let t = std::time::Instant::now();
        s.export_cookies(&a).unwrap();
        eprintln!(
            "native export of {}: {:.2}s",
            s.label(),
            t.elapsed().as_secs_f32()
        );
        let tools = crate::yt::Tools::locate().unwrap();
        let t = std::time::Instant::now();
        crate::yt::import_cookies(&tools, &s.snapshot_spec().unwrap(), &b).unwrap();
        clear_snapshot();
        eprintln!("yt-dlp export: {:.2}s", t.elapsed().as_secs_f32());
        let names = |p: &Path| -> std::collections::BTreeSet<String> {
            std::fs::read_to_string(p)
                .unwrap()
                .lines()
                .map(|l| l.strip_prefix("#HttpOnly_").unwrap_or(l))
                .filter(|l| !l.starts_with('#') && !l.is_empty())
                .filter_map(|l| {
                    let f: Vec<&str> = l.split('\t').collect();
                    (f.len() >= 7
                        && (f[0].ends_with("youtube.com") || f[0].ends_with("google.com")))
                    .then(|| format!("{} {}", f[0], f[5]))
                })
                .collect()
        };
        let (na, nb) = (names(&a), names(&b));
        eprintln!(
            "native {} cookies, yt-dlp {}; only yt-dlp: {:?}",
            na.len(),
            nb.len(),
            nb.difference(&na).collect::<Vec<_>>()
        );
        // YSC is a per-visit cookie YouTube hands yt-dlp during its own request
        assert!(nb.difference(&na).all(|n| n.ends_with(" YSC")));
        let mut p = crate::innertube::Pager::new(&a, &crate::yt::Source::WatchLater).unwrap();
        assert!(
            !p.next_page().unwrap().unwrap().is_empty(),
            "signed-in call with native cookies"
        );
        let _ = std::fs::remove_file(a);
        let _ = std::fs::remove_file(b);
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
