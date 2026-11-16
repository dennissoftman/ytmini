//! Offline downloads: video and audio saved as separate files (no ffmpeg
//! needed; mpv plays them together) in ~/Movies/ytmini, listed in
//! downloads.json next to the settings. A size cap is enforced by deleting
//! the oldest *watched* downloads first.

use crate::yt::{self, Tools, Video};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::process::Stdio;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Entry {
    pub id: String,
    pub title: String,
    pub channel: Option<String>,
    pub duration: Option<f64>,
    /// File names inside `dir()`.
    pub video: Option<String>,
    pub audio: Option<String>,
    pub bytes: u64,
    /// Unix seconds.
    pub added: u64,
    /// e.g. "720p" or "audio"
    pub quality: String,
}

impl Entry {
    pub fn video_path(&self) -> Option<PathBuf> {
        Some(dir().ok()?.join(self.video.as_ref()?))
    }
    pub fn audio_path(&self) -> Option<PathBuf> {
        Some(dir().ok()?.join(self.audio.as_ref()?))
    }
    pub fn as_video(&self) -> Video {
        Video {
            id: self.id.clone(),
            title: Some(self.title.clone()),
            channel: self.channel.clone(),
            duration: self.duration,
            ..Default::default()
        }
    }
    fn delete_files(&self) {
        for p in [self.video_path(), self.audio_path()].into_iter().flatten() {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// Where the files go (visible to you in Finder).
pub fn dir() -> Result<PathBuf> {
    let d = dirs::video_dir()
        .map(|d| d.join("ytmini"))
        .map_or_else(|| yt::config_dir().map(|c| c.join("downloads")), Ok)?;
    std::fs::create_dir_all(&d).with_context(|| format!("creating {}", d.display()))?;
    Ok(d)
}

#[derive(Default, Serialize, Deserialize)]
pub struct Manifest {
    pub entries: Vec<Entry>,
}

impl Manifest {
    fn path() -> Option<PathBuf> {
        yt::config_dir().ok().map(|d| d.join("downloads.json"))
    }

    /// Entries whose files are still there.
    pub fn load() -> Manifest {
        let mut m: Manifest = Self::path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        m.entries.retain(|e| {
            e.video_path().is_some_and(|p| p.exists()) || e.audio_path().is_some_and(|p| p.exists())
        });
        m
    }

    pub fn save(&self) {
        let Some(p) = Self::path() else { return };
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let tmp = p.with_extension("json.tmp");
            if std::fs::write(&tmp, json).is_ok() {
                let _ = std::fs::rename(tmp, p);
            }
        }
    }

    pub fn get(&self, id: &str) -> Option<&Entry> {
        self.entries.iter().find(|e| e.id == id)
    }

    pub fn total_bytes(&self) -> u64 {
        self.entries.iter().map(|e| e.bytes).sum()
    }

    pub fn add(&mut self, e: Entry) {
        self.entries.retain(|x| x.id != e.id);
        self.entries.push(e);
        self.save();
    }

    pub fn remove(&mut self, id: &str) {
        if let Some(i) = self.entries.iter().position(|e| e.id == id) {
            self.entries.remove(i).delete_files();
            self.save();
        }
    }

    pub fn clear(&mut self) {
        for e in self.entries.drain(..) {
            e.delete_files();
        }
        self.save();
    }

    /// Make room under `cap` bytes by deleting watched downloads, oldest
    /// first (never `keep`). Returns how many were deleted.
    pub fn evict(&mut self, cap: u64, watched: impl Fn(&str) -> bool, keep: &str) -> usize {
        let mut order: Vec<(u64, String)> = self
            .entries
            .iter()
            .filter(|e| e.id != keep && watched(&e.id))
            .map(|e| (e.added, e.id.clone()))
            .collect();
        order.sort();
        let mut n = 0;
        for (_, id) in order {
            if self.total_bytes() <= cap {
                break;
            }
            self.remove(&id);
            n += 1;
        }
        n
    }
}

pub enum Progress {
    /// 0.0–1.0
    Running(String, f32),
    Done(String, Result<Entry, String>),
}

/// yt-dlp format for a download of at most `height` (0 = audio only).
/// VP9 first (hardware-decoded and much sharper than H.264 per byte); no
/// AV1 (most Macs can't hardware-decode it, so it'd cost battery offline).
fn video_format(height: u32) -> String {
    format!(
        "bv*[height<={height}][vcodec^=vp][fps<=30]/bv*[height<={height}][vcodec!^=av01][fps<=30]/bv*[height<={height}][vcodec!^=av01]/bv*[height<={height}]/b[height<={height}]"
    )
}

/// Download `video` (blocking; run on a worker thread). Calls `progress`
/// with 0.0–1.0 as it goes.
pub fn download(
    tools: &Tools,
    cookies: &Path,
    video: &Video,
    height: u32,
    mut progress: impl FnMut(f32),
) -> Result<Entry> {
    let d = dir()?;
    let audio_only = height == 0;
    let mut parts: Vec<(&str, String, f32, f32)> = Vec::new(); // (kind, format, from, to)
    if !audio_only {
        parts.push(("video", video_format(height), 0.0, 0.85));
    }
    parts.push((
        "audio",
        "ba[ext=m4a]/ba".into(),
        if audio_only { 0.0 } else { 0.85 },
        1.0,
    ));
    let (mut vfile, mut afile, mut bytes) = (None, None, 0u64);
    for (kind, fmt, from, to) in parts {
        let jar = yt::CookieCopy::new(cookies)?;
        let mut child = tools
            .ytdlp_cmd()
            .env("PYTHONUNBUFFERED", "1")
            .arg("--cookies")
            .arg(jar.path())
            .args(["--no-warnings", "--no-playlist", "--newline", "--no-part", "--no-mtime", "--progress"])
            .args(["-f", &fmt])
            .args([
                "--progress-template",
                "download:ytmini-progress %(progress.downloaded_bytes)s %(progress.total_bytes,progress.total_bytes_estimate)s",
            ])
            .args(["--print", "after_move:ytmini-file %(filepath)s"])
            .arg("-o")
            .arg(d.join(format!("%(id)s.{kind}.%(ext)s")))
            .arg(video.url())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("running yt-dlp")?;
        let out = std::io::BufReader::new(child.stdout.take().context("no stdout")?);
        let mut file = None;
        for line in out.lines().map_while(Result::ok) {
            if let Some(rest) = line.strip_prefix("ytmini-progress ") {
                let mut it = rest.split_whitespace().map(|n| n.parse::<f64>().ok());
                if let (Some(Some(done)), Some(Some(total))) = (it.next(), it.next()) {
                    if total > 0.0 {
                        progress(from + (to - from) * (done / total).min(1.0) as f32);
                    }
                }
            } else if let Some(f) = line.strip_prefix("ytmini-file ") {
                file = Some(PathBuf::from(f.trim()));
            }
        }
        let mut err = String::new();
        if let Some(mut e) = child.stderr.take() {
            use std::io::Read;
            let _ = e.read_to_string(&mut err);
        }
        let status = child.wait()?;
        let file = match (status.success(), file) {
            (true, Some(f)) if f.exists() => f,
            _ => {
                let msg = err
                    .lines()
                    .rev()
                    .find(|l| l.contains("ERROR"))
                    .unwrap_or("download failed");
                anyhow::bail!("{}", msg.trim());
            }
        };
        bytes += std::fs::metadata(&file).map(|m| m.len()).unwrap_or(0);
        let name = file.file_name().map(|n| n.to_string_lossy().into_owned());
        if kind == "video" {
            vfile = name;
        } else {
            afile = name;
        }
    }
    let added = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    Ok(Entry {
        id: video.id.clone(),
        title: video.title().to_string(),
        channel: video.channel.clone(),
        duration: video.duration,
        video: vfile,
        audio: afile,
        bytes,
        added,
        quality: if audio_only {
            "audio".into()
        } else {
            format!("{height}p")
        },
    })
}

/// "1.4 GB"
pub fn fmt_bytes(b: u64) -> String {
    let b = b as f64;
    if b >= 1e9 {
        format!("{:.1} GB", b / 1e9)
    } else {
        format!("{:.0} MB", b / 1e6)
    }
}

#[cfg(test)]
mod tests {
    /// Live: downloads "Me at the zoo" at 480p, checks both files, deletes them.
    /// `cargo test -- --ignored download_live --nocapture`
    #[test]
    #[ignore]
    fn download_live() {
        let tools = crate::yt::Tools::locate().unwrap();
        let cookies = crate::yt::cookie_file().unwrap();
        let v = crate::yt::Video::from_url_or_id("jNQXAC9IVRw").unwrap();
        let t = std::time::Instant::now();
        let mut ticks = 0;
        let e = super::download(&tools, &cookies, &v, 480, |_| ticks += 1).unwrap();
        eprintln!(
            "{e:?} in {:.1}s, {ticks} progress updates",
            t.elapsed().as_secs_f32()
        );
        assert!(e.video_path().unwrap().exists() && e.audio_path().unwrap().exists());
        assert!(e.bytes > 100_000);
        e.delete_files();
    }
}
