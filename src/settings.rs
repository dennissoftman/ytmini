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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Density {
    /// Smaller cards, more per row.
    Compact,
    #[default]
    Comfortable,
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
    pub density: Density,
    /// Sidebar shows icons only.
    pub sidebar_collapsed: bool,
    /// Even out loudness between and within videos (mpv dynaudnorm).
    pub volume_leveling: bool,
    /// Skip SponsorBlock segments.
    pub sponsorblock: bool,
    /// SponsorBlock categories to skip (ids from `sponsorblock::CATEGORIES`).
    pub sb_categories: Vec<String>,
    /// Offline downloads: max height (0 = audio only) and total size cap.
    pub download_height: u32,
    pub download_cap_gb: u64,
    /// Feed filters (home, subscriptions, search; never channel/playlist pages).
    pub show_shorts: bool,
    pub hide_live: bool,
    /// Home and subscriptions only.
    pub hide_watched: bool,
    /// Channel names whose videos are hidden.
    pub muted_channels: Vec<String>,
    /// Hide videos whose title contains any of these (case-insensitive).
    pub muted_words: Vec<String>,
}

impl Settings {
    /// Whether the feed filters hide `v` on `source`.
    pub fn hides(&self, source: &yt::Source, v: &yt::Video, watched: bool) -> bool {
        use yt::Source::*;
        let feed = matches!(source, Feed | Subscriptions);
        if !(feed || matches!(source, Search(_))) || v.is_playlist {
            return false;
        }
        if (v.is_short && !self.show_shorts)
            || (v.is_live && self.hide_live)
            || (feed && watched && self.hide_watched)
        {
            return true;
        }
        if let Some(ch) = v.channel.as_deref() {
            if self
                .muted_channels
                .iter()
                .any(|m| m.eq_ignore_ascii_case(ch))
            {
                return true;
            }
        }
        let title = v.title.as_deref().unwrap_or("").to_lowercase();
        self.muted_words
            .iter()
            .any(|w| !w.trim().is_empty() && title.contains(&w.trim().to_lowercase()))
    }
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
            density: Density::Comfortable,
            sidebar_collapsed: false,
            volume_leveling: true,
            sponsorblock: true,
            sb_categories: crate::sponsorblock::default_categories(),
            download_height: 720,
            download_cap_gb: 10,
            show_shorts: false,
            hide_live: false,
            hide_watched: false,
            muted_channels: Vec::new(),
            muted_words: Vec::new(),
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
