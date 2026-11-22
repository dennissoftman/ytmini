//! DeArrow (experimental): community-written titles and thumbnails that
//! replace clickbait. https://dearrow.ajay.app — titles use the same
//! hash-prefix lookup as SponsorBlock (the server gets 4 hex chars of the id's
//! SHA-256), but DeArrow's rendered thumbnails are fetched by video id.

use anyhow::{Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::time::Duration;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Branding {
    pub title: Option<String>,
    /// Seconds into the video for the replacement thumbnail.
    pub thumb_time: Option<f64>,
}

#[derive(Deserialize)]
struct Raw {
    #[serde(default)]
    titles: Vec<RawTitle>,
    #[serde(default)]
    thumbnails: Vec<RawThumb>,
}

#[derive(Deserialize)]
struct RawTitle {
    title: String,
    #[serde(default)]
    original: bool,
    #[serde(default)]
    votes: i64,
    #[serde(default)]
    locked: bool,
}

#[derive(Deserialize)]
struct RawThumb {
    #[serde(default)]
    timestamp: Option<f64>,
    #[serde(default)]
    original: bool,
    #[serde(default)]
    votes: i64,
    #[serde(default)]
    locked: bool,
}

pub fn fetch(id: &str) -> Result<Branding> {
    let prefix: String = Sha256::digest(id.as_bytes())
        .iter()
        .take(2)
        .map(|b| format!("{b:02x}"))
        .collect();
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(8)))
        .user_agent("ytmini")
        .http_status_as_error(false)
        .build()
        .into();
    let mut resp = agent
        .get(format!("https://sponsor.ajay.app/api/branding/{prefix}"))
        .call()
        .context("DeArrow request failed")?;
    if resp.status().as_u16() == 404 {
        return Ok(Branding::default());
    }
    anyhow::ensure!(
        resp.status().as_u16() == 200,
        "DeArrow answered HTTP {}",
        resp.status()
    );
    let body = resp.body_mut().read_to_string()?;
    let mut all: HashMap<String, Raw> =
        serde_json::from_str(&body).context("unexpected DeArrow reply")?;
    let Some(raw) = all.remove(id) else {
        return Ok(Branding::default());
    };
    // the API lists the best first; trust it once it isn't voted down
    let title = raw
        .titles
        .iter()
        .find(|t| !t.original && (t.locked || t.votes >= 0))
        .map(|t| t.title.trim_start_matches('>').trim().to_string())
        .filter(|t| !t.is_empty());
    let thumb_time = raw
        .thumbnails
        .iter()
        .find(|t| !t.original && (t.locked || t.votes >= 0))
        .and_then(|t| t.timestamp);
    Ok(Branding { title, thumb_time })
}

/// Thumbnail rendered from the video at `t` seconds by DeArrow's service.
pub fn thumb_url(id: &str, t: f64) -> String {
    format!("https://dearrow-thumb.ajay.app/api/v1/getThumbnail?videoID={id}&time={t}")
}

#[cfg(test)]
mod tests {
    /// Live: `cargo test -- --ignored dearrow_live --nocapture`
    #[test]
    #[ignore]
    fn dearrow_live() {
        for id in ["XpWfH5Qw1R8", "z19HM7ANZlo", "-NNCca97v98"] {
            eprintln!("{id}: {:?}", super::fetch(id));
        }
    }
}
