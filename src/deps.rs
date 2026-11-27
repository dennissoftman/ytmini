//! Managed copies of yt-dlp and deno, so the app doesn't need Homebrew.
//!
//! Both are downloaded from their official GitHub releases into
//! `<config dir>/bin/`, verified against the published SHA-256 sums before
//! anything is executed, and swapped in atomically. yt-dlp's sums file must
//! also carry a valid signature by its release key (src/pgpsig.rs).
//!
//! yt-dlp: we use the *unpacked* (`.zip`) build everywhere. The single-file
//! build unpacks itself to a temp dir on every launch, and macOS then scans
//! those fresh files each time — measured 13 s per call vs 0.26 s unpacked.
//! Only the unpacked build has the challenge solver files the fast stream
//! lookup needs (src/native.rs); Windows used the single-file build up to 0.3,
//! and the daily check replaces it (its checksum differs from the zip's).
//! The unpacked build can't self-update, so we check the release's checksum
//! file daily and replace it when it changes. YouTube breaks old yt-dlp
//! versions often, so staying current matters more than pinning.
//!
//! deno: only downloaded if no system deno is found (yt-dlp needs a JS
//! runtime to solve YouTube's challenges).

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, SystemTime},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dep {
    YtDlp,
    Deno,
}

struct Asset {
    /// Release download URL.
    url: String,
    /// URL of the checksum file and the line name to look for in it.
    sums_url: String,
    sums_name: String,
    /// Executable path inside the install dir.
    exe: &'static str,
    zip: bool,
    /// The sums file has a detached signature (`<sums_url>.sig`) by a key
    /// pinned in the app (yt-dlp only; deno doesn't sign its releases).
    signed: bool,
}

impl Dep {
    pub fn name(self) -> &'static str {
        match self {
            Dep::YtDlp => "yt-dlp",
            Dep::Deno => "deno",
        }
    }

    fn asset(self) -> Option<Asset> {
        self.asset_for(std::env::consts::OS, std::env::consts::ARCH)
    }

    fn asset_for(self, os: &str, arch: &str) -> Option<Asset> {
        match self {
            Dep::YtDlp => {
                let base = "https://github.com/yt-dlp/yt-dlp/releases/latest/download";
                let (file, exe, zip) = match (os, arch) {
                    ("macos", _) => ("yt-dlp_macos.zip", "yt-dlp_macos", true),
                    ("linux", "x86_64") => ("yt-dlp_linux.zip", "yt-dlp_linux", true),
                    ("linux", "aarch64") => {
                        ("yt-dlp_linux_aarch64.zip", "yt-dlp_linux_aarch64", true)
                    }
                    ("windows", "aarch64") => ("yt-dlp_win_arm64.zip", "yt-dlp_arm64.exe", true),
                    ("windows", "x86") => ("yt-dlp_win_x86.zip", "yt-dlp.exe", true),
                    ("windows", _) => ("yt-dlp_win.zip", "yt-dlp.exe", true),
                    _ => return None,
                };
                Some(Asset {
                    url: format!("{base}/{file}"),
                    sums_url: format!("{base}/SHA2-256SUMS"),
                    sums_name: file.into(),
                    exe,
                    zip,
                    signed: true,
                })
            }
            Dep::Deno => {
                let target = match (os, arch) {
                    ("macos", "aarch64") => "aarch64-apple-darwin",
                    ("macos", "x86_64") => "x86_64-apple-darwin",
                    ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
                    ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
                    ("windows", "x86_64") => "x86_64-pc-windows-msvc",
                    _ => return None,
                };
                let file = format!("deno-{target}.zip");
                let url =
                    format!("https://github.com/denoland/deno/releases/latest/download/{file}");
                Some(Asset {
                    sums_url: format!("{url}.sha256sum"),
                    url,
                    sums_name: file,
                    exe: if cfg!(windows) { "deno.exe" } else { "deno" },
                    zip: true,
                    signed: false,
                })
            }
        }
    }

    fn dir(self) -> Option<PathBuf> {
        Some(crate::yt::config_dir().ok()?.join("bin").join(self.name()))
    }

    /// Path to the managed executable, if installed.
    pub fn managed(self) -> Option<PathBuf> {
        let exe = self.dir()?.join(self.asset()?.exe);
        exe.is_file().then_some(exe)
    }

    fn sha_file(self) -> Option<PathBuf> {
        Some(self.dir()?.with_extension("sha256"))
    }
}

/// Progress reports for the setup screen.
#[derive(Clone, Debug)]
pub enum Progress {
    Downloading {
        dep: Dep,
        done: u64,
        total: Option<u64>,
    },
    Verifying(Dep),
    /// First launch: macOS checks new executables once (can take ~15 s).
    Preparing(Dep),
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(15)))
        .user_agent("ytmini")
        .build()
        .into()
}

/// Expected SHA-256 for this platform's asset, from the release's sums file
/// (for yt-dlp, only after its signature checks out against the pinned key).
fn expected_sha(agent: &ureq::Agent, asset: &Asset) -> Result<String> {
    let fetch = |url: &str| -> Result<Vec<u8>> {
        Ok(agent
            .get(url)
            .call()?
            .body_mut()
            .with_config()
            .limit(1024 * 1024)
            .read_to_vec()?)
    };
    let sums = fetch(&asset.sums_url).context("fetching checksums")?;
    if asset.signed {
        let sig =
            fetch(&format!("{}.sig", asset.sums_url)).context("fetching checksum signature")?;
        crate::pgpsig::verify_ytdlp(&sums, &sig)
            .context("the checksum file isn't signed by yt-dlp's release key; download not used")?;
    }
    let text = String::from_utf8(sums).context("reading checksums")?;
    parse_sha(&text, &asset.sums_name)
        .with_context(|| format!("{} not listed in checksums", asset.sums_name))
}

/// The hash for `name` in a sums file: `<hash>  <name>` lines, or the
/// PowerShell `Hash : <HASH>` form deno uses for its Windows builds.
pub(crate) fn parse_sha(text: &str, name: &str) -> Option<String> {
    let is_sha = |h: &str| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit());
    text.lines()
        .find_map(|l| {
            let mut it = l.split_whitespace();
            let (hash, file) = (it.next()?, it.next()?);
            (file.trim_start_matches('*') == name).then(|| hash.to_ascii_lowercase())
        })
        // deno's Windows builds publish PowerShell's Get-FileHash output for
        // that one file instead: "Hash : CBA84B…"
        .or_else(|| {
            text.lines().find_map(|l| {
                let (key, val) = l.split_once(':')?;
                (key.trim() == "Hash" && is_sha(val.trim()))
                    .then(|| val.trim().to_ascii_lowercase())
            })
        })
}

/// Is a newer build published than the one installed? (Compares checksums.)
pub fn update_available(dep: Dep) -> Result<bool> {
    let asset = dep.asset().context("unsupported platform")?;
    let installed = dep
        .sha_file()
        .and_then(|p| fs::read_to_string(p).ok())
        .unwrap_or_default();
    Ok(expected_sha(&agent(), &asset)? != installed.trim())
}

/// Download, verify, install (atomically replacing any previous copy) and
/// warm up `dep`. Blocking; run on a worker thread.
pub fn install(dep: Dep, progress: &dyn Fn(Progress)) -> Result<PathBuf> {
    let asset = dep
        .asset()
        .context("no prebuilt download for this platform")?;
    let dir = dep.dir().context("no config directory")?;
    let bin = dir.parent().context("bad install dir")?.to_path_buf();
    fs::create_dir_all(&bin)?;
    let agent = agent();
    let want = expected_sha(&agent, &asset)?;

    // download → temp file, hashing as we go
    let tmp = bin.join(format!(".{}.download", dep.name()));
    let mut resp = agent
        .get(&asset.url)
        .call()
        .with_context(|| format!("downloading {}", asset.url))?;
    let total = resp.body().content_length();
    let mut reader = resp
        .body_mut()
        .with_config()
        .limit(512 * 1024 * 1024)
        .reader();
    let mut out = fs::File::create(&tmp)?;
    let mut hasher = Sha256::new();
    let (mut buf, mut done, mut last_report) = (vec![0u8; 256 * 1024], 0u64, 0u64);
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n])?;
        hasher.update(&buf[..n]);
        done += n as u64;
        if done - last_report > 512 * 1024 {
            progress(Progress::Downloading { dep, done, total });
            last_report = done;
        }
    }
    out.flush()?;
    drop(out);

    progress(Progress::Verifying(dep));
    let got = hex(&hasher.finalize());
    if got != want {
        let _ = fs::remove_file(&tmp);
        bail!(
            "{} checksum mismatch (expected {want}, got {got}) — download discarded",
            dep.name()
        );
    }

    // unpack into a staging dir, then swap
    let staging = bin.join(format!(".{}.new", dep.name()));
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging)?;
    if asset.zip {
        unzip(&tmp, &staging)?;
    } else {
        fs::rename(&tmp, staging.join(asset.exe))?;
    }
    let _ = fs::remove_file(&tmp);
    let exe_in_staging = staging.join(asset.exe);
    if !exe_in_staging.is_file() {
        bail!("{} missing from the download", asset.exe);
    }
    make_executable(&exe_in_staging)?;

    let old = bin.join(format!(".{}.old", dep.name()));
    let _ = fs::remove_dir_all(&old);
    if dir.exists() {
        fs::rename(&dir, &old)?; // running processes keep their open files
    }
    fs::rename(&staging, &dir)?;
    let _ = fs::remove_dir_all(&old);
    if let Some(p) = dep.sha_file() {
        fs::write(p, &want)?;
    }

    // Warm-up: the first launch of a new executable triggers macOS's one-time
    // security scan. Do it now rather than on the user's first click.
    let exe = dir.join(asset.exe);
    progress(Progress::Preparing(dep));
    let _ = version(&exe);
    mark_checked(dep);
    Ok(exe)
}

fn unzip(zip_path: &Path, dest: &Path) -> Result<()> {
    let mut archive = zip::ZipArchive::new(fs::File::open(zip_path)?).context("opening zip")?;
    for i in 0..archive.len() {
        let mut f = archive.by_index(i)?;
        // enclosed_name rejects absolute paths and `..` (zip-slip)
        let Some(rel) = f.enclosed_name() else {
            continue;
        };
        let out = dest.join(rel);
        if f.is_dir() {
            fs::create_dir_all(&out)?;
            continue;
        }
        if let Some(parent) = out.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut w = fs::File::create(&out)?;
        std::io::copy(&mut f, &mut w)?;
        #[cfg(unix)]
        if let Some(mode) = f.unix_mode() {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&out, fs::Permissions::from_mode(mode & 0o777))?;
        }
    }
    Ok(())
}

fn make_executable(_p: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perm = fs::metadata(_p)?.permissions();
        perm.set_mode(perm.mode() | 0o755);
        fs::set_permissions(_p, perm)?;
    }
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `<exe> --version`, first line.
pub fn version(exe: &Path) -> Option<String> {
    let mut c = Command::new(exe);
    crate::yt::hide_console(&mut c);
    let out = c
        .arg("--version")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    s.lines()
        .next()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
}

fn check_stamp(dep: Dep) -> Option<PathBuf> {
    Some(dep.dir()?.with_extension("checked"))
}

/// Stamp "checked now". The stamp's modified time is what counts; Windows
/// keeps the old time when an empty file is rewritten empty, so set it
/// explicitly (otherwise every launch looked overdue and re-checked in a loop).
fn mark_checked(dep: Dep) {
    if let Some(p) = check_stamp(dep) {
        let now = SystemTime::now();
        let secs = now
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let _ = fs::write(&p, secs.to_string());
        if let Ok(f) = fs::OpenOptions::new().write(true).open(&p) {
            let _ = f.set_modified(now);
        }
    }
}

/// A yt-dlp installed by ytmini ≤ 0.3 on Windows: the single-file build,
/// without the challenge solver files. Replaced at the next start rather
/// than at the next daily check.
pub fn single_file_ytdlp() -> bool {
    let Some(dir) = Dep::YtDlp.dir() else {
        return false;
    };
    Dep::YtDlp.managed().is_some() && !dir.join("_internal").is_dir()
}

/// Has it been longer than `every` since we last checked for an update?
pub fn check_due(dep: Dep, every: Duration) -> bool {
    let age = check_stamp(dep)
        .and_then(|p| fs::metadata(p).ok())
        .and_then(|m| m.modified().ok())
        .and_then(|t| SystemTime::now().duration_since(t).ok());
    age.is_none_or(|a| a > every)
}

/// Check and, if needed, install an update. Returns the new version if updated.
pub fn update_if_needed(dep: Dep, progress: &dyn Fn(Progress)) -> Result<Option<String>> {
    let available = update_available(dep)?;
    mark_checked(dep);
    if !available {
        return Ok(None);
    }
    let exe = install(dep, progress)?;
    Ok(Some(version(&exe).unwrap_or_else(|| "new version".into())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assets_exist_for_this_platform() {
        let y = Dep::YtDlp.asset().expect("yt-dlp asset");
        assert!(y.url.ends_with(&y.sums_name));
        let d = Dep::Deno.asset().expect("deno asset");
        assert!(d.sums_url.ends_with(".sha256sum"));
    }

    /// Every yt-dlp download we use is listed in the signed sums file.
    #[test]
    fn ytdlp_assets_are_in_the_signed_sums() {
        let sums = include_str!("testdata/yt-dlp-SHA2-256SUMS");
        for (os, arch) in [
            ("macos", "aarch64"),
            ("macos", "x86_64"),
            ("linux", "x86_64"),
            ("linux", "aarch64"),
            ("windows", "x86_64"),
            ("windows", "x86"),
            ("windows", "aarch64"),
        ] {
            let a = Dep::YtDlp.asset_for(os, arch).unwrap();
            assert!(a.zip && a.signed, "{os} {arch}");
            assert!(parse_sha(sums, &a.sums_name).is_some(), "{}", a.sums_name);
        }
    }

    /// Live: hits GitHub. `cargo test -- --ignored deps_live`
    #[test]
    #[ignore]
    fn deps_live_checksums_resolve() {
        for dep in [Dep::YtDlp, Dep::Deno] {
            let sha = expected_sha(&agent(), &dep.asset().unwrap()).unwrap();
            assert_eq!(sha.len(), 64, "{dep:?}");
        }
    }
}

#[cfg(test)]
mod live_install {
    use super::*;

    /// Downloads ~40 MB into the real bin dir: `cargo test -- --ignored deps_install_deno`
    #[test]
    #[ignore]
    fn deps_install_deno() {
        let exe = install(Dep::Deno, &|p| eprintln!("{p:?}")).unwrap();
        let v = version(&exe).unwrap();
        eprintln!("installed {v}");
        assert!(v.starts_with("deno "), "{v}");
    }

    /// Full yt-dlp install (signature, checksum, unpack) into a scratch
    /// config dir: `cargo test -- --ignored deps_install_ytdlp_scratch`
    #[test]
    #[ignore]
    fn deps_install_ytdlp_scratch() {
        let dir = std::env::temp_dir().join(format!("ytmini-deps-{}", std::process::id()));
        std::env::set_var("YTMINI_CONFIG_DIR", &dir);
        let exe = install(Dep::YtDlp, &|_| {}).unwrap();
        assert!(exe.starts_with(&dir));
        let v = version(&exe).unwrap();
        eprintln!("installed yt-dlp {v}");
        let _ = fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod sums_tests {
    use super::parse_sha;

    #[test]
    fn reads_both_sums_formats() {
        let unix = "9f0bf660393da2eaff044cdc7444cb08c73f235849b61d224d825a194ce7c98e  deno-aarch64-apple-darwin.zip\n";
        assert_eq!(
            parse_sha(unix, "deno-aarch64-apple-darwin.zip").as_deref(),
            Some("9f0bf660393da2eaff044cdc7444cb08c73f235849b61d224d825a194ce7c98e")
        );
        let ps = "\r\nAlgorithm : SHA256\r\nHash      : CBA84B26E0DF5AE7F74FC66B86A95B866872A86292EBD84FA1E959927EDD93BF\r\nPath      : C:\\a\\deno\\deno-x86_64-pc-windows-msvc.zip\r\n\r\n";
        assert_eq!(
            parse_sha(ps, "deno-x86_64-pc-windows-msvc.zip").as_deref(),
            Some("cba84b26e0df5ae7f74fc66b86a95b866872a86292ebd84fa1e959927edd93bf")
        );
        assert_eq!(parse_sha("nothing here", "x.zip"), None);
    }
}
