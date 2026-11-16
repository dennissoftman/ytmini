//! User settings, persisted as JSON next to the cookies
//! (`~/Library/Application Support/ytmini/settings.json` on macOS).

use crate::yt::{self, Quality};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlayerMode {
    /// Video inside the ytmini window (libmpv render API).
    #[default]
    Embedded,
    /// mpv in its own window — fallback if embedding breaks after an mpv update.
    Window,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThumbQuality {
    /// High on HiDPI/large cards, standard otherwise.
    #[default]
    Auto,
    /// 320×180 WebP (~14 KB on disk)
    Standard,
    /// 1280×720 source stored as 640×360 JPEG (~60 KB); falls back to
    /// standard for old videos without a 720p thumbnail.
    High,
}

/// The browser profile "Sign in with Google" last imported from.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SignInSource {
    /// yt-dlp browser id
    pub id: String,
    pub profile: std::path::PathBuf,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub player: PlayerMode,
    pub quality: Quality,
    pub thumbs: ThumbQuality,
    /// Disk cache size cap in MiB.
    pub cache_mb: u64,
    /// Delete cached files not used for this many days.
    pub cache_days: u64,
    /// 0–100
    pub volume: f64,
    /// Tell YouTube we watched it, so it shows up in YouTube's watch history.
    pub mark_watched: bool,
    /// Closing the window keeps ytmini in the menu bar / tray.
    pub close_to_tray: bool,
    /// For one-click "Refresh session".
    pub signin_source: Option<SignInSource>,
}

pub const DEFAULT_CACHE_MB: u64 = 256;
pub const DEFAULT_CACHE_DAYS: u64 = 30;

impl Default for Settings {
    fn default() -> Self {
        Settings {
            player: PlayerMode::Embedded,
            quality: Quality::Auto,
            thumbs: ThumbQuality::Auto,
            cache_mb: DEFAULT_CACHE_MB,
            cache_days: DEFAULT_CACHE_DAYS,
            volume: 80.0,
            mark_watched: true,
            close_to_tray: true,
            signin_source: None,
        }
    }
}

impl Settings {
    fn path() -> Option<std::path::PathBuf> {
        yt::config_dir().ok().map(|d| d.join("settings.json"))
    }

    pub fn load() -> Settings {
        Self::path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
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
}
