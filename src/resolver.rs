//! Fast stream lookups: a couple of long-running yt-dlp processes (see
//! `resolver_plugin.py`) instead of a fresh yt-dlp per video.
//!
//! A one-off run spends ~0.5 s starting Python, ~0.3 s downloading YouTube's
//! player script and ~0.8 s parsing it for the JS challenge, on top of the
//! actual lookups. A warm helper skips all three: ~1.5 s instead of ~3.5 s.
//! Helpers hold a private cookie copy (`CookieCopy`) and are restarted when
//! the cookies change, the network changes, or after a long idle spell.

use crate::yt::{self, CookieCopy, Direct, Tools, Video};
use anyhow::{Context, Result};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

const PLUGIN: &str = include_str!("resolver_plugin.py");
/// Helpers alive at once; more lookups at a time fall back to one-off runs.
const MAX_HELPERS: usize = 2;
/// An unused helper is stopped after this long (it holds ~100 MB).
const IDLE_MAX: Duration = Duration::from_secs(20 * 60);

struct Helper {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    cookies: Option<(SystemTime, u64)>,
    used: Instant,
    generation: u64,
    _jar: CookieCopy,
}

impl Drop for Helper {
    fn drop(&mut self) {
        // the plugin removes its cookie copy on a clean exit; `_jar` does it here
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Pool {
    idle: Vec<Helper>,
    busy: usize,
    generation: u64,
}

static POOL: Mutex<Pool> = Mutex::new(Pool {
    idle: Vec::new(),
    busy: 0,
    generation: 0,
});

fn debug() -> bool {
    std::env::var_os("YTMINI_DEBUG").is_some()
}

fn cookie_stamp(cookies: &Path) -> Option<(SystemTime, u64)> {
    let m = std::fs::metadata(cookies).ok()?;
    Some((m.modified().ok()?, m.len()))
}

/// The plugin folder inside ytmini's private config folder (written fresh
/// whenever the bundled plugin changes).
fn plugin_dir() -> Result<PathBuf> {
    let root = yt::config_dir()?.join("plugins");
    let dir = root.join("ytmini").join("yt_dlp_plugins").join("extractor");
    let file = dir.join("ytmini_resolver.py");
    if std::fs::read_to_string(&file).ok().as_deref() != Some(PLUGIN) {
        std::fs::create_dir_all(&dir)?;
        let tmp = file.with_extension("tmp");
        std::fs::write(&tmp, PLUGIN)?;
        std::fs::rename(&tmp, &file)?;
    }
    Ok(root)
}

fn spawn(tools: &Tools, cookies: &Path, generation: u64) -> Result<Helper> {
    let jar = CookieCopy::new(cookies)?;
    let stamp = cookie_stamp(cookies);
    let mut child = tools
        .ytdlp_cmd()
        .arg("--plugin-dirs")
        .arg(plugin_dir()?)
        .args(["-q", "--no-warnings", "-s", "--no-playlist"])
        .arg("--cookies")
        .arg(jar.path())
        .args(["--extractor-args", yt::RESOLVE_EXTRACTOR_ARGS])
        .arg("ytmini-serve:")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(if debug() {
            Stdio::inherit()
        } else {
            Stdio::null()
        })
        .spawn()
        .context("starting yt-dlp")?;
    let stdin = child.stdin.take().context("yt-dlp stdin")?;
    let stdout = BufReader::new(child.stdout.take().context("yt-dlp stdout")?);
    let mut h = Helper {
        child,
        stdin,
        stdout,
        cookies: stamp,
        used: Instant::now(),
        generation,
        _jar: jar,
    };
    // anything before the ready line is yt-dlp's own chatter
    for _ in 0..50 {
        let line = h.read_line()?;
        if line.contains("\"ready\"") {
            return Ok(h);
        }
    }
    anyhow::bail!("yt-dlp resolver didn't start")
}

impl Helper {
    fn read_line(&mut self) -> Result<String> {
        let mut line = String::new();
        let n = self.stdout.read_line(&mut line)?;
        anyhow::ensure!(n > 0, "yt-dlp resolver exited");
        Ok(line)
    }

    fn ask(&mut self, url: &str, format: &str) -> Result<Result<Direct>> {
        let req = serde_json::json!({ "url": url, "format": format });
        writeln!(self.stdin, "{req}")?;
        self.stdin.flush()?;
        loop {
            let line = self.read_line()?;
            let Ok(rep) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            let Some(ok) = rep.get("ok").and_then(|o| o.as_bool()) else {
                continue;
            };
            self.used = Instant::now();
            return Ok(if ok {
                rep.get("info")
                    .context("no info in the resolver's answer")
                    .and_then(yt::direct_from_info)
            } else {
                Err(anyhow::anyhow!(
                    "yt-dlp couldn't resolve the video: {}",
                    rep.get("error").and_then(|e| e.as_str()).unwrap_or("")
                ))
            });
        }
    }
}

/// Take an idle helper, or the right to start one. `None`: all are busy.
fn take(cookies: &Path) -> Option<(Option<Helper>, u64)> {
    let mut pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
    let stamp = cookie_stamp(cookies);
    let generation = pool.generation;
    // signed in/out or re-imported: those helpers have the old cookies
    pool.idle.retain(|h| h.cookies == stamp);
    if let Some(h) = pool.idle.pop() {
        pool.busy += 1;
        return Some((Some(h), generation));
    }
    if pool.busy + pool.idle.len() < MAX_HELPERS {
        pool.busy += 1;
        return Some((None, generation));
    }
    None
}

fn give_back(h: Option<Helper>) {
    let mut pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
    pool.busy = pool.busy.saturating_sub(1);
    if let Some(h) = h.filter(|h| h.generation == pool.generation) {
        pool.idle.push(h);
    }
}

/// Resolve `video`'s stream URLs for `format` (blocking).
pub fn resolve(tools: &Tools, cookies: &Path, video: &Video, format: &str) -> Result<Direct> {
    let t = Instant::now();
    // our own lookup first (~0.2 s); yt-dlp when it can't
    match crate::native::resolve(tools, cookies, video, format) {
        Ok(d) => {
            if debug() {
                eprintln!(
                    "resolver: {} in {:.2}s (native)",
                    video.id,
                    t.elapsed().as_secs_f32()
                );
            }
            return Ok(d);
        }
        Err(e) => {
            if debug() {
                eprintln!("native: {} not used: {e:#}", video.id);
            }
            if !crate::native::is_skip(&e) {
                // ours failed: if yt-dlp then manages, that's a bug to know about
                let r = resolve_ytdlp(tools, cookies, video, format);
                if r.is_ok() {
                    crate::native::record_failure(Some(&video.id), &format!("{e:#}"));
                }
                return r;
            }
        }
    }
    resolve_ytdlp(tools, cookies, video, format)
}

/// Through the yt-dlp helpers (or a one-off run when they're busy).
fn resolve_ytdlp(tools: &Tools, cookies: &Path, video: &Video, format: &str) -> Result<Direct> {
    let t = Instant::now();
    let Some((helper, generation)) = take(cookies) else {
        if debug() {
            eprintln!(
                "resolver: all helpers busy, one-off yt-dlp for {}",
                video.id
            );
        }
        return yt::resolve(tools, cookies, video, format);
    };
    let warm = helper.is_some();
    let mut helper = match helper.map_or_else(|| spawn(tools, cookies, generation), Ok) {
        Ok(h) => h,
        Err(e) => {
            give_back(None);
            if debug() {
                eprintln!("resolver: {e:#}; one-off yt-dlp instead");
            }
            return yt::resolve(tools, cookies, video, format);
        }
    };
    match helper.ask(&video.url(), format) {
        Ok(r) => {
            if debug() {
                eprintln!(
                    "resolver: {} in {:.2}s ({})",
                    video.id,
                    t.elapsed().as_secs_f32(),
                    if warm { "warm" } else { "cold" }
                );
            }
            give_back(Some(helper));
            r
        }
        // the helper died mid-request: drop it, do this one the old way
        Err(e) => {
            drop(helper);
            give_back(None);
            if debug() {
                eprintln!("resolver: {e:#}; one-off yt-dlp instead");
            }
            yt::resolve(tools, cookies, video, format)
        }
    }
}

/// Start a helper ahead of the first lookup (no network: it only loads
/// yt-dlp), so the first video opens warm. Blocking; call off the UI thread.
pub fn warm_up(tools: &Tools, cookies: &Path) {
    crate::native::warm_up(tools);
    match take(cookies) {
        Some((None, generation)) => give_back(spawn(tools, cookies, generation).ok()),
        Some((Some(h), _)) => give_back(Some(h)),
        None => {}
    }
}

/// Forget all helpers (network changed: their connections are stale).
pub fn reset() {
    let mut pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
    pool.generation += 1;
    pool.idle.clear();
}

/// Stop helpers nobody used for a while (cheap; call now and then).
pub fn reap_idle() {
    crate::native::reap_idle();
    let mut pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
    pool.idle.retain(|h| h.used.elapsed() < IDLE_MAX);
}
