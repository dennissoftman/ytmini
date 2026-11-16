//! Disk cache: thumbnails (original compressed bytes) and the last result of
//! each personal tab (so the app opens instantly with yesterday's feed while
//! fetching a fresh one).
//!
//! Lives in the OS cache dir (`~/Library/Caches/ytmini` on macOS,
//! `~/.cache/ytmini` on Linux, `%LOCALAPPDATA%\ytmini` on Windows), so the OS
//! may also purge it. Eviction is LRU by file mtime (touched on every hit):
//! first anything older than the retention period, then oldest-first until
//! under the size cap.

use crate::yt::Video;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime},
};

/// Limits shared between the UI (settings) and the thumbnail workers.
#[derive(Debug)]
pub struct Limits {
    pub cap_bytes: AtomicU64,
    pub max_age_days: AtomicU64,
}

impl Limits {
    pub fn new(cap_mb: u64, days: u64) -> Limits {
        Limits { cap_bytes: AtomicU64::new(cap_mb * 1024 * 1024), max_age_days: AtomicU64::new(days) }
    }
    pub fn set(&self, cap_mb: u64, days: u64) {
        self.cap_bytes.store(cap_mb * 1024 * 1024, Ordering::Relaxed);
        self.max_age_days.store(days, Ordering::Relaxed);
    }
}

pub fn root() -> Option<PathBuf> {
    let d = dirs::cache_dir()?.join("ytmini");
    fs::create_dir_all(d.join("thumbs")).ok()?;
    fs::create_dir_all(d.join("lists")).ok()?;
    Some(d)
}

fn atomic_write(path: &Path, bytes: &[u8]) {
    let tmp = path.with_extension("tmp");
    if fs::write(&tmp, bytes).is_ok() {
        let _ = fs::rename(&tmp, path);
    }
}

/// Mark as recently used (for LRU eviction).
fn touch(path: &Path) {
    if let Ok(f) = fs::File::options().write(true).open(path) {
        let _ = f.set_modified(SystemTime::now());
    }
}

// ---------- thumbnails ----------

fn thumb_path(key: &str) -> Option<PathBuf> {
    // key is "<11-char id>-<variant>"; ids are [A-Za-z0-9_-], safe as filenames.
    Some(root()?.join("thumbs").join(key))
}

pub fn read_thumb(key: &str) -> Option<Vec<u8>> {
    let p = thumb_path(key)?;
    let bytes = fs::read(&p).ok()?;
    touch(&p);
    Some(bytes)
}

pub fn write_thumb(key: &str, bytes: &[u8]) {
    if let Some(p) = thumb_path(key) {
        atomic_write(&p, bytes);
    }
}

// ---------- lists ----------

#[derive(Serialize, Deserialize)]
pub struct CachedList {
    pub saved_at: u64, // unix seconds
    pub videos: Vec<Video>,
}

fn list_path(name: &str) -> Option<PathBuf> {
    Some(root()?.join("lists").join(format!("{name}.json")))
}

pub fn read_list(name: &str) -> Option<CachedList> {
    let p = list_path(name)?;
    let list = serde_json::from_slice(&fs::read(&p).ok()?).ok()?;
    touch(&p);
    Some(list)
}

pub fn write_list(name: &str, videos: &[Video]) {
    let Some(p) = list_path(name) else { return };
    let saved_at = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    if let Ok(json) = serde_json::to_vec(&CachedList { saved_at, videos: videos.to_vec() }) {
        atomic_write(&p, &json);
    }
}

// ---------- maintenance ----------

/// Apply retention + size cap. Returns the cache size afterwards (bytes).
pub fn enforce(limits: &Limits) -> u64 {
    let Some(root) = root() else { return 0 };
    let cap = limits.cap_bytes.load(Ordering::Relaxed);
    let max_age = Duration::from_secs(limits.max_age_days.load(Ordering::Relaxed) * 86_400);
    let now = SystemTime::now();

    let mut files: Vec<(PathBuf, u64, SystemTime)> = Vec::new();
    for sub in ["thumbs", "lists"] {
        let Ok(rd) = fs::read_dir(root.join(sub)) else { continue };
        for e in rd.flatten() {
            let Ok(m) = e.metadata() else { continue };
            if !m.is_file() {
                continue;
            }
            let mtime = m.modified().unwrap_or(now);
            if now.duration_since(mtime).unwrap_or_default() > max_age {
                let _ = fs::remove_file(e.path());
            } else {
                files.push((e.path(), m.len(), mtime));
            }
        }
    }
    let mut total: u64 = files.iter().map(|f| f.1).sum();
    if total > cap {
        files.sort_by_key(|f| f.2); // oldest first
        // Evict a bit below the cap so we don't run this on every write.
        let target = cap / 10 * 9;
        for (path, len, _) in files {
            if total <= target {
                break;
            }
            if fs::remove_file(&path).is_ok() {
                total -= len;
            }
        }
    }
    total
}

pub fn clear() {
    if let Some(root) = root() {
        for sub in ["thumbs", "lists"] {
            let _ = fs::remove_dir_all(root.join(sub));
        }
        let _ = fs::create_dir_all(root.join("thumbs"));
        let _ = fs::create_dir_all(root.join("lists"));
    }
}
