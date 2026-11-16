//! SponsorBlock: community-submitted segments (sponsor reads, "like and
//! subscribe", intros…) that the player skips and marks on the seek bar.
//! https://wiki.sponsor.ajay.app/w/API_Docs
//!
//! Privacy: we ask for a 4-character prefix of the video id's SHA-256, so the
//! server never learns which video you're watching (the API's own k-anonymity
//! scheme), and pick ours out of the matches locally.

use anyhow::{Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::time::Duration;

/// Categories we offer, with the label shown in Settings and the colour
/// SponsorBlock's own extension uses for them.
pub const CATEGORIES: [(&str, &str, [u8; 3]); 7] = [
    ("sponsor", "Sponsor", [0x00, 0xD4, 0x00]),
    ("selfpromo", "Self-promotion", [0xFF, 0xFF, 0x00]),
    ("interaction", "Like/subscribe reminder", [0xCC, 0x00, 0xFF]),
    ("intro", "Intro animation", [0x00, 0xFF, 0xFF]),
    ("outro", "End cards / credits", [0x02, 0x02, 0xED]),
    ("preview", "Preview / recap", [0x00, 0x8F, 0xD6]),
    (
        "music_offtopic",
        "Non-music part (music videos)",
        [0xFF, 0x99, 0x00],
    ),
];

pub fn default_categories() -> Vec<String> {
    vec!["sponsor".into(), "selfpromo".into(), "interaction".into()]
}

/// In the interface language.
pub fn label(cat: &str) -> &str {
    use crate::i18n::t;
    match cat {
        "sponsor" => t("Sponsor"),
        "selfpromo" => t("Self-promotion"),
        "interaction" => t("Like/subscribe reminder"),
        "intro" => t("Intro animation"),
        "outro" => t("End cards / credits"),
        "preview" => t("Preview / recap"),
        "music_offtopic" => t("Non-music part (music videos)"),
        _ => CATEGORIES.iter().find(|c| c.0 == cat).map_or(cat, |c| c.1),
    }
}

pub fn color(cat: &str) -> [u8; 3] {
    CATEGORIES
        .iter()
        .find(|c| c.0 == cat)
        .map_or([0xAA, 0xAA, 0xAA], |c| c.2)
}

#[derive(Clone, Debug, PartialEq)]
pub struct Segment {
    pub start: f64,
    pub end: f64,
    pub category: String,
}

#[derive(Deserialize)]
struct Entry {
    #[serde(rename = "videoID")]
    video_id: String,
    segments: Vec<RawSegment>,
}

#[derive(Deserialize)]
struct RawSegment {
    category: String,
    segment: [f64; 2],
    #[serde(rename = "actionType")]
    action: String,
}

/// Skippable segments for `id` in the given categories, sorted by start.
pub fn fetch(id: &str, categories: &[String]) -> Result<Vec<Segment>> {
    if categories.is_empty() {
        return Ok(Vec::new());
    }
    let hash: String = Sha256::digest(id.as_bytes())
        .iter()
        .take(2)
        .map(|b| format!("{b:02x}"))
        .collect();
    let cats = serde_json::to_string(categories)?;
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(10)))
        .user_agent("ytmini")
        .http_status_as_error(false)
        .build()
        .into();
    let mut resp = agent
        .get(format!("https://sponsor.ajay.app/api/skipSegments/{hash}"))
        .query("categories", &cats)
        .query("actionTypes", r#"["skip"]"#)
        .call()
        .context("SponsorBlock request failed")?;
    match resp.status().as_u16() {
        404 => return Ok(Vec::new()), // no video with this prefix has segments
        200 => {}
        s => anyhow::bail!("SponsorBlock answered HTTP {s}"),
    }
    let body = resp.body_mut().read_to_string()?;
    let entries: Vec<Entry> =
        serde_json::from_str(&body).context("unexpected SponsorBlock reply")?;
    let mut segs: Vec<Segment> = entries
        .into_iter()
        .filter(|e| e.video_id == id)
        .flat_map(|e| e.segments)
        .filter(|s| s.action == "skip" && s.segment[1] - s.segment[0] >= 1.0)
        .map(|s| Segment {
            start: s.segment[0],
            end: s.segment[1],
            category: s.category,
        })
        .collect();
    segs.sort_by(|a, b| a.start.total_cmp(&b.start));
    Ok(segs)
}

#[cfg(test)]
mod tests {
    /// Live: a well-known video with sponsor segments. `cargo test -- --ignored sponsorblock`
    #[test]
    #[ignore]
    fn sponsorblock_live() {
        let segs = super::fetch("XpWfH5Qw1R8", &super::default_categories()).unwrap();
        eprintln!("{segs:?}");
    }
}
