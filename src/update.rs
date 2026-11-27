//! ytmini's own updates: finds a newer release on GitHub and, once the user
//! agrees, downloads and installs it.
//!
//! Trust: every release's `SHA256SUMS.txt` is signed by ytmini's update key
//! (`SHA256SUMS.txt.sig`, Ed25519, made in release.yml from the
//! `UPDATE_SIGNING_KEY` secret). The public half is pinned below, so a
//! release whose files were swapped on GitHub is refused; the user can still
//! download it from the release page. On macOS the new app must also be
//! signed with ytmini's Developer ID (team `MAC_TEAM`).
//!
//! - macOS: the zipped app is unpacked next to the running one, checked, and
//!   swapped in; "Restart" relaunches it.
//! - Windows: the installer runs silently (`/S /relaunch`): it closes ytmini,
//!   installs over it and starts it again.
//! - Linux, and installs we can't replace (a dev build, a read-only folder):
//!   the release page opens instead.
//!
//! Requests go to GitHub only and carry no cookies.

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};

const REPO: &str = "dennissoftman/ytmini";

/// Public half of the update signing key (Ed25519, raw). The private key
/// lives with the maintainer and in the release workflow's secrets.
const UPDATE_KEY: [u8; 32] = [
    0x4a, 0x30, 0x63, 0xef, 0x93, 0x93, 0xe7, 0xe8, 0xb0, 0xd1, 0xd8, 0x8e, 0x77, 0xd0, 0x21, 0x88,
    0xe7, 0xb2, 0xf3, 0x8d, 0xaa, 0x79, 0x14, 0xf0, 0xfd, 0x9c, 0xab, 0x76, 0x47, 0x44, 0x67, 0x09,
];

/// Apple Developer ID team that signs ytmini's macOS releases.
#[cfg(target_os = "macos")]
const MAC_TEAM: &str = "6DB36T6A47";

/// How often a running ytmini looks for a new release.
pub const CHECK_EVERY: Duration = Duration::from_secs(24 * 3600);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    /// "0.5.1"
    pub version: String,
    /// The release page (notes and every download).
    pub page: String,
    /// "What's new", as plain text (from the changelog).
    pub notes: String,
}

/// The running version. `YTMINI_UPDATE_FROM=0.4.0` pretends to be an older
/// one (testing).
pub fn current() -> String {
    std::env::var("YTMINI_UPDATE_FROM").unwrap_or_else(|_| env!("CARGO_PKG_VERSION").into())
}

/// Where releases come from. `YTMINI_UPDATE_URL` points elsewhere (a local
/// mirror, for testing): `<url>/latest.json` and `<url>/<files>`.
fn mirror() -> Option<String> {
    std::env::var("YTMINI_UPDATE_URL")
        .ok()
        .map(|u| u.trim_end_matches('/').to_string())
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_global(Some(Duration::from_secs(600)))
        .user_agent(concat!("ytmini/", env!("CARGO_PKG_VERSION")))
        .build()
        .into()
}

fn fetch_small(agent: &ureq::Agent, url: &str) -> Result<Vec<u8>> {
    let mut r = agent.get(url).call()?;
    Ok(r.body_mut()
        .with_config()
        .limit(2 * 1024 * 1024)
        .read_to_vec()?)
}

/// `(major, minor, patch)` of "0.5.1" or "v0.5.1"; pre-releases aren't offered.
fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let v = v.trim().trim_start_matches('v');
    let mut it = v.split('.');
    let out = (
        it.next()?.parse().ok()?,
        it.next()?.parse().ok()?,
        it.next()?.parse().ok()?,
    );
    it.next().is_none().then_some(out)
}

pub fn is_newer(candidate: &str, than: &str) -> bool {
    match (parse_version(candidate), parse_version(than)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

/// The latest release, if it's newer than this one. Blocking.
pub fn check() -> Result<Option<Release>> {
    let agent = agent();
    let url = match mirror() {
        Some(m) => format!("{m}/latest.json"),
        None => format!("https://api.github.com/repos/{REPO}/releases/latest"),
    };
    let body = fetch_small(&agent, &url).context("asking GitHub for the latest release")?;
    let v: serde_json::Value = serde_json::from_slice(&body)?;
    let tag = v["tag_name"].as_str().context("no tag_name")?;
    if v["draft"].as_bool() == Some(true) || v["prerelease"].as_bool() == Some(true) {
        return Ok(None);
    }
    let version = tag.trim_start_matches('v').to_string();
    if parse_version(&version).is_none() || !is_newer(&version, &current()) {
        return Ok(None);
    }
    let page = v["html_url"]
        .as_str()
        .filter(|u| u.starts_with("https://"))
        .map(str::to_string)
        .unwrap_or_else(|| format!("https://github.com/{REPO}/releases/tag/{tag}"));
    let notes = clean_notes(v["body"].as_str().unwrap_or_default());
    Ok(Some(Release {
        version,
        page,
        notes,
    }))
}

/// release-please's markdown → plain text: no version heading, section
/// headings kept, bullets as "•", links reduced to their text and commit
/// links dropped.
pub fn clean_notes(md: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    for line in md.lines() {
        let line = line.trim_end();
        if line.starts_with("## ") || line.starts_with("# ") {
            continue; // "## [0.5.0](…) (2026-11-27)"
        }
        let line = match line.strip_prefix("### ") {
            Some(h) => h.to_string(),
            None => match line
                .trim_start()
                .strip_prefix("* ")
                .or_else(|| line.trim_start().strip_prefix("- "))
            {
                Some(b) => format!("•  {b}"),
                None => line.to_string(),
            },
        };
        let line = strip_links(&line);
        if line.is_empty() && out.last().is_none_or(|l| l.is_empty()) {
            continue;
        }
        out.push(line);
    }
    while out.last().is_some_and(|l| l.is_empty()) {
        out.pop();
    }
    out.join("\n")
}

/// `[text](url)` → `text`; ` ([abc1234](url))` (commit, issue) → removed.
fn strip_links(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('[') {
        let after = &rest[i + 1..];
        let link = after.find("](").and_then(|j| {
            let close = after[j + 2..].find(')')?;
            Some((&after[..j], j + 2 + close + 1))
        });
        match link {
            Some((text, len)) => {
                out.push_str(&rest[..i]);
                let is_ref = text.starts_with('#')
                    || (text.len() >= 7 && text.bytes().all(|b| b.is_ascii_hexdigit()));
                if is_ref && out.ends_with('(') && after[len..].starts_with(')') {
                    // " ([abc1234](…))": drop it with its parentheses
                    out.pop();
                    let trimmed = out.trim_end().len();
                    out.truncate(trimmed);
                    rest = &after[len + 1..];
                    continue;
                }
                if !is_ref {
                    out.push_str(text);
                }
                rest = &after[len..];
            }
            None => {
                out.push_str(&rest[..=i]);
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out.trim_end().to_string()
}

/// This system's download in a release, if it has one we can install.
fn asset_name(version: &str) -> Option<String> {
    asset_name_for(std::env::consts::OS, std::env::consts::ARCH, version)
}

fn asset_name_for(os: &str, arch: &str, version: &str) -> Option<String> {
    match (os, arch) {
        ("macos", "aarch64") => Some(format!("ytmini-{version}-macos-arm64.zip")),
        ("windows", "x86_64") => Some(format!("ytmini-{version}-setup.exe")),
        _ => None,
    }
}

/// Can this copy of ytmini update itself, or does the user get the release
/// page? `None` = install in place; `Some(reason)` for the log.
pub fn install_blocker() -> Option<&'static str> {
    if asset_name("0.0.0").is_none() {
        return Some("no installable download for this system");
    }
    #[cfg(target_os = "macos")]
    {
        let Some(app) = app_bundle() else {
            return Some("not running from ytmini.app");
        };
        let parent = app.parent()?;
        if !writable(parent) {
            return Some("the app's folder isn't writable");
        }
    }
    #[cfg(windows)]
    {
        // installed by our installer (a portable copy from the zip has no
        // uninstaller next to it and is left alone)
        let ok = std::env::current_exe()
            .ok()
            .and_then(|e| Some(e.parent()?.join("uninstall.exe").is_file()))
            .unwrap_or(false);
        if !ok {
            return Some("not installed with the ytmini installer");
        }
    }
    None
}

#[cfg(target_os = "macos")]
fn writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".ytmini-write-test-{}", std::process::id()));
    let ok = fs::write(&probe, b"").is_ok();
    let _ = fs::remove_file(&probe);
    ok
}

/// `/…/ytmini.app` when running from an app bundle.
#[cfg(target_os = "macos")]
fn app_bundle() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let app = exe.parent()?.parent()?.parent()?.to_path_buf(); // Contents/MacOS/ytmini
    (app.extension()? == "app").then_some(app)
}

/// Verified checksum for `name` from a release's signed sums file.
fn signed_sha(agent: &ureq::Agent, base: &str, name: &str) -> Result<String> {
    let sums = fetch_small(agent, &format!("{base}/SHA256SUMS.txt"))
        .context("downloading the release's checksums")?;
    let sig = fetch_small(agent, &format!("{base}/SHA256SUMS.txt.sig"))
        .context("this release has no update signature")?;
    verify_sig(&UPDATE_KEY, &sums, &sig)?;
    let text = String::from_utf8(sums).context("reading checksums")?;
    crate::deps::parse_sha(&text, name).with_context(|| format!("{name} isn't in the checksums"))
}

fn verify_sig(key: &[u8; 32], msg: &[u8], sig: &[u8]) -> Result<()> {
    use ed25519_dalek::{Signature, VerifyingKey};
    let key = VerifyingKey::from_bytes(key).context("bad update key")?;
    let sig: [u8; 64] = sig
        .try_into()
        .ok()
        .context("the update signature is malformed")?;
    key.verify_strict(msg, &Signature::from_bytes(&sig))
        .ok()
        .context("the checksums aren't signed by ytmini's update key; not installing")
}

/// Where downloads are kept until they're installed.
fn work_dir() -> Result<PathBuf> {
    Ok(crate::yt::config_dir()?.join("update"))
}

/// A downloaded, verified update, waiting for "Restart".
#[derive(Clone, Debug)]
#[allow(dead_code)] // one variant per system
pub enum Ready {
    /// macOS: the new app is already in place; relaunch it.
    Relaunch(PathBuf),
    /// Windows: run this installer silently; it restarts ytmini.
    Installer(PathBuf),
}

/// Download and verify `rel`, and on macOS swap the new app in. Blocking;
/// `progress` gets 0–1.
pub fn download(rel: &Release, progress: &dyn Fn(f32)) -> Result<Ready> {
    if let Some(why) = install_blocker() {
        bail!("can't update this copy in place ({why})");
    }
    let name = asset_name(&rel.version).context("no download for this system")?;
    let base = match mirror() {
        Some(m) => m,
        None => format!(
            "https://github.com/{REPO}/releases/download/v{}",
            rel.version
        ),
    };
    let agent = agent();
    let want = signed_sha(&agent, &base, &name)?;

    let dir = work_dir()?;
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir)?;
    let file = dir.join(&name);
    let mut resp = agent
        .get(&format!("{base}/{name}"))
        .call()
        .context("downloading the update")?;
    let total = resp.body().content_length();
    let mut reader = resp
        .body_mut()
        .with_config()
        .limit(1024 * 1024 * 1024)
        .reader();
    let mut out = fs::File::create(&file)?;
    let mut hasher = Sha256::new();
    let (mut buf, mut done) = (vec![0u8; 256 * 1024], 0u64);
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n])?;
        hasher.update(&buf[..n]);
        done += n as u64;
        if let Some(t) = total.filter(|t| *t > 0) {
            progress((done as f32 / t as f32).min(1.0));
        }
    }
    out.flush()?;
    drop(out);
    let got: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if got != want {
        let _ = fs::remove_file(&file);
        bail!("the download doesn't match the signed checksum; discarded");
    }
    install_downloaded(&file)
}

#[cfg(target_os = "macos")]
fn install_downloaded(zip: &Path) -> Result<Ready> {
    use anyhow::ensure;
    use std::process::Command;
    let app = app_bundle().context("not running from ytmini.app")?;
    let parent = app.parent().context("app folder")?.to_path_buf();
    // Unpack on the same volume as the app, so the swap is a rename.
    let staging = parent.join(".ytmini-update");
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging)?;
    let st = Command::new("/usr/bin/ditto")
        .args(["-x", "-k"])
        .arg(zip)
        .arg(&staging)
        .status()
        .context("unpacking the update")?;
    ensure!(st.success(), "unpacking the update failed");
    let new = staging.join("ytmini.app");
    let check = (|| -> Result<()> {
        ensure!(
            new.join("Contents/MacOS/ytmini").is_file(),
            "the download holds no ytmini.app"
        );
        let st = Command::new("/usr/bin/codesign")
            .args(["--verify", "--deep", "--strict"])
            .arg(&new)
            .status()?;
        ensure!(st.success(), "the new app's signature is invalid");
        let out = Command::new("/usr/bin/codesign")
            .args(["-d", "--verbose=2"])
            .arg(&new)
            .output()?;
        let info = String::from_utf8_lossy(&out.stderr);
        ensure!(
            info.lines()
                .any(|l| l.trim() == format!("TeamIdentifier={MAC_TEAM}")),
            "the new app isn't signed by ytmini's developer"
        );
        Ok(())
    })();
    if let Err(e) = check {
        let _ = fs::remove_dir_all(&staging);
        return Err(e);
    }
    // Swap: the running process keeps its open files.
    let old = parent.join(".ytmini-old.app");
    let _ = fs::remove_dir_all(&old);
    fs::rename(&app, &old).context("moving the old app aside")?;
    if let Err(e) = fs::rename(&new, &app) {
        let _ = fs::rename(&old, &app);
        return Err(e).context("putting the new app in place");
    }
    let _ = fs::remove_dir_all(&staging);
    let _ = fs::remove_file(zip);
    Ok(Ready::Relaunch(app))
}

#[cfg(windows)]
fn install_downloaded(exe: &Path) -> Result<Ready> {
    Ok(Ready::Installer(exe.to_path_buf()))
}

#[cfg(not(any(target_os = "macos", windows)))]
fn install_downloaded(_: &Path) -> Result<Ready> {
    bail!("updates install from the release page on this system")
}

/// Start the new version (macOS) or the installer (Windows). The caller
/// quits right after.
pub fn finish(ready: &Ready) -> Result<()> {
    use std::process::Command;
    match ready {
        Ready::Relaunch(app) => {
            // wait for this process to exit, then open the new app
            let pid = std::process::id().to_string();
            let mut c = Command::new("/bin/sh");
            c.arg("-c")
                .arg(
                    "while kill -0 \"$1\" 2>/dev/null; do sleep 0.2; done; \
                     exec /usr/bin/open -n ${3:+\"$3\"} \"$2\"",
                )
                .arg("ytmini-relaunch")
                .arg(&pid)
                .arg(app);
            // a test profile stays a test profile
            match std::env::var("YTMINI_CONFIG_DIR") {
                Ok(d) => c.arg(format!("--env=YTMINI_CONFIG_DIR={d}")),
                Err(_) => c.arg(""),
            };
            c.spawn().context("restarting ytmini")?;
        }
        Ready::Installer(exe) => {
            let mut c = Command::new(exe);
            crate::yt::hide_console(&mut c);
            c.args(["/S", "/relaunch"])
                .spawn()
                .context("starting the installer")?;
        }
    }
    Ok(())
}

/// At startup: remove what an earlier update left behind.
pub fn cleanup() {
    if let Ok(d) = work_dir() {
        let _ = fs::remove_dir_all(d);
    }
    #[cfg(target_os = "macos")]
    if let Some(parent) = app_bundle().and_then(|a| a.parent().map(Path::to_path_buf)) {
        let _ = fs::remove_dir_all(parent.join(".ytmini-old.app"));
        let _ = fs::remove_dir_all(parent.join(".ytmini-update"));
    }
}

/// `--check-update [--install]`: prints what the updater sees; with
/// `--install`, downloads, verifies and installs it (macOS: then relaunches).
/// Exit codes: 0 up to date or installed; 1 failed; 10 an update is available.
pub fn self_check(install: bool) -> i32 {
    println!("ytmini {} update check", current());
    let rel = match check() {
        Ok(Some(r)) => r,
        Ok(None) => {
            println!("ok    up to date");
            return 0;
        }
        Err(e) => {
            println!("FAIL  check: {e:#}");
            return 1;
        }
    };
    println!("new   {} ({})", rel.version, rel.page);
    match install_blocker() {
        Some(why) => println!("info  installs from the release page here: {why}"),
        None => println!("ok    can install in place"),
    }
    if !install {
        return 10;
    }
    let ready = match download(&rel, &|_| {}) {
        Ok(r) => r,
        Err(e) => {
            println!("FAIL  install: {e:#}");
            return 1;
        }
    };
    println!("ok    downloaded and verified: {ready:?}");
    match finish(&ready) {
        Ok(()) => {
            println!("ok    handed over (the new version starts after this exits)");
            0
        }
        Err(e) => {
            println!("FAIL  {e:#}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert!(is_newer("0.5.0", "0.4.1"));
        assert!(is_newer("v0.10.0", "0.9.9"));
        assert!(is_newer("1.0.0", "0.99.99"));
        assert!(!is_newer("0.4.1", "0.4.1"));
        assert!(!is_newer("0.4.0", "0.4.1"));
        assert!(!is_newer("0.5.0-rc1", "0.4.1"));
        assert!(!is_newer("garbage", "0.4.1"));
    }

    #[test]
    fn assets() {
        assert_eq!(
            asset_name_for("macos", "aarch64", "0.5.0").as_deref(),
            Some("ytmini-0.5.0-macos-arm64.zip")
        );
        assert_eq!(
            asset_name_for("windows", "x86_64", "0.5.0").as_deref(),
            Some("ytmini-0.5.0-setup.exe")
        );
        assert_eq!(asset_name_for("linux", "x86_64", "0.5.0"), None);
        assert_eq!(asset_name_for("macos", "x86_64", "0.5.0"), None);
    }

    #[test]
    fn notes() {
        let md = "## [0.4.1](https://github.com/x/y/compare/v0.4.0...v0.4.1) (2026-11-27)\n\n\n\
                  ### Bug Fixes\n\n\
                  * videos load quickly again ([e0bf510](https://github.com/x/y/commit/e0bf510))\n\
                  * see [the docs](https://example.com) now ([#12](https://github.com/x/y/issues/12))\n";
        assert_eq!(
            clean_notes(md),
            "Bug Fixes\n\n•  videos load quickly again\n•  see the docs now"
        );
        assert_eq!(clean_notes(""), "");
    }

    /// RFC 8032 test 2 (one-byte message).
    #[test]
    fn signature() {
        let unhex = |s: &str| -> Vec<u8> {
            (0..s.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
                .collect()
        };
        let key: [u8; 32] =
            unhex("3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c")
                .try_into()
                .unwrap();
        let sig = unhex(
            "92a009a9f0d4cab8720e820b5f642540a2b27b5416503f8fb3762223ebdb69da\
             085ac1e43e15996e458f3613d0f11d8c387b2eaeb4302aeeb00d291612bb0c00",
        );
        assert!(verify_sig(&key, &[0x72], &sig).is_ok());
        assert!(verify_sig(&key, &[0x73], &sig).is_err());
        assert!(verify_sig(&key, &[0x72], &sig[..63]).is_err());
        // the pinned key rejects it
        assert!(verify_sig(&UPDATE_KEY, &[0x72], &sig).is_err());
    }
}
