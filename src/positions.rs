//! Watch positions per video, kept locally (`positions.json` in the config
//! dir) so playback resumes where you left off and cards show progress.
//!
//! YouTube's own resume position isn't reachable through yt-dlp, so this is
//! ytmini-only; YouTube's watch history still gets the video (mark-watched).

use crate::yt;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    time::{Duration, Instant, SystemTime},
};

/// Don't resume in the first few seconds or the last stretch of a video.
const MIN_RESUME: f64 = 15.0;
const END_MARGIN: f64 = 30.0;
/// Past this fraction a video counts as watched.
const WATCHED: f64 = 0.92;
/// Entries kept (oldest dropped). ~60 bytes each.
const MAX_ENTRIES: usize = 5000;
/// Write at most this often while playing.
const SAVE_EVERY: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Entry {
    /// seconds
    pub pos: f64,
    /// seconds; 0 when unknown (live)
    pub dur: f64,
    /// unix seconds, for pruning
    pub at: u64,
}

impl Entry {
    /// 0..1, or None if the duration is unknown.
    pub fn fraction(&self) -> Option<f32> {
        (self.dur > 0.0).then(|| (self.pos / self.dur).clamp(0.0, 1.0) as f32)
    }
    pub fn watched(&self) -> bool {
        self.fraction().is_some_and(|f| f as f64 >= WATCHED)
    }
}

pub struct Positions {
    map: HashMap<String, Entry>,
    dirty: bool,
    last_save: Instant,
}

fn path() -> Option<std::path::PathBuf> {
    yt::config_dir().ok().map(|d| d.join("positions.json"))
}

static NO_TRACE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Test runs (`--smoke-test`, `YTMINI_NO_HISTORY=1`) keep resume points and
/// the continue card in memory only, so they don't overwrite the real ones.
pub fn set_no_trace() {
    NO_TRACE.store(true, std::sync::atomic::Ordering::Relaxed);
}

pub fn no_trace() -> bool {
    NO_TRACE.load(std::sync::atomic::Ordering::Relaxed)
        || std::env::var_os("YTMINI_NO_HISTORY").is_some()
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl Positions {
    pub fn load() -> Positions {
        let map = path()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Positions {
            map,
            dirty: false,
            last_save: Instant::now(),
        }
    }

    pub fn get(&self, id: &str) -> Option<&Entry> {
        self.map.get(id)
    }

    /// Where to start `id`, if it's worth resuming.
    pub fn resume_at(&self, id: &str) -> Option<f64> {
        let e = self.map.get(id)?;
        let far_enough = e.pos >= MIN_RESUME;
        let not_at_end = e.dur <= 0.0 || e.pos < e.dur - END_MARGIN;
        (far_enough && not_at_end).then_some(e.pos)
    }

    /// Record progress; persisted at most every few seconds (see `maybe_save`).
    pub fn update(&mut self, id: &str, pos: f64, dur: f64) {
        if pos <= 0.0 {
            return;
        }
        self.map.insert(
            id.to_string(),
            Entry {
                pos,
                dur,
                at: now(),
            },
        );
        self.dirty = true;
    }

    /// Mark as fully watched (duration unknown → just flagged by a large position).
    pub fn mark_watched(&mut self, id: &str, dur: Option<f64>) {
        let d = dur.filter(|d| *d > 0.0).unwrap_or(1.0);
        self.map.insert(
            id.to_string(),
            Entry {
                pos: d,
                dur: d,
                at: now(),
            },
        );
        self.dirty = true;
    }

    /// The `n` videos played most recently (past the first half minute).
    pub fn recent(&self, n: usize) -> Vec<String> {
        let mut v: Vec<(&String, &Entry)> = self
            .map
            .iter()
            .filter(|(_, e)| e.pos >= 30.0 || e.watched())
            .collect();
        v.sort_by_key(|(_, e)| std::cmp::Reverse(e.at));
        v.into_iter().take(n).map(|(k, _)| k.clone()).collect()
    }

    /// Videos played to the end.
    pub fn watched_ids(&self) -> std::collections::HashSet<String> {
        self.map
            .iter()
            .filter(|(_, e)| e.watched())
            .map(|(k, _)| k.clone())
            .collect()
    }

    pub fn forget(&mut self, id: &str) {
        self.dirty |= self.map.remove(id).is_some();
    }

    pub fn maybe_save(&mut self) {
        if self.dirty && self.last_save.elapsed() >= SAVE_EVERY {
            self.save();
        }
    }

    pub fn save(&mut self) {
        if !self.dirty || no_trace() {
            return;
        }
        if self.map.len() > MAX_ENTRIES {
            let mut by_age: Vec<(u64, String)> =
                self.map.iter().map(|(k, e)| (e.at, k.clone())).collect();
            by_age.sort();
            for (_, k) in by_age.into_iter().take(self.map.len() - MAX_ENTRIES) {
                self.map.remove(&k);
            }
        }
        if let (Some(p), Ok(json)) = (path(), serde_json::to_vec(&self.map)) {
            let tmp = p.with_extension("json.tmp");
            if std::fs::write(&tmp, json).is_ok() {
                let _ = std::fs::rename(tmp, p);
            }
        }
        self.dirty = false;
        self.last_save = Instant::now();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with(pos: f64, dur: f64) -> Positions {
        let mut p = Positions {
            map: HashMap::new(),
            dirty: false,
            last_save: Instant::now(),
        };
        p.update("abcdefghijk", pos, dur);
        p
    }

    #[test]
    fn resume_rules() {
        assert_eq!(with(5.0, 600.0).resume_at("abcdefghijk"), None); // too early
        assert_eq!(with(300.0, 600.0).resume_at("abcdefghijk"), Some(300.0));
        assert_eq!(with(590.0, 600.0).resume_at("abcdefghijk"), None); // basically finished
        assert!(with(570.0, 600.0).get("abcdefghijk").unwrap().watched());
        assert_eq!(with(120.0, 0.0).resume_at("abcdefghijk"), Some(120.0)); // unknown duration
    }
}
