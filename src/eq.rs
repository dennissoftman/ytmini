//! Auto EQ for songs: a preset per genre, the genre looked up on Deezer's
//! public API (artist + title only: no account, no cookies). A preset you pick
//! by hand sticks to the artist. Applied by mpv as an ffmpeg equalizer, ahead
//! of the volume leveling.

use crate::i18n::t;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Band centres (Hz), one octave apart.
const BANDS: [u32; 10] = [31, 62, 125, 250, 500, 1000, 2000, 4000, 8000, 16000];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Preset {
    Flat,
    Pop,
    Rock,
    HipHop,
    Electronic,
    Jazz,
    Classical,
    Vocal,
    Metal,
    RnB,
}

impl Preset {
    pub const ALL: [Preset; 10] = [
        Preset::Flat,
        Preset::Pop,
        Preset::Rock,
        Preset::HipHop,
        Preset::Electronic,
        Preset::Jazz,
        Preset::Classical,
        Preset::Vocal,
        Preset::Metal,
        Preset::RnB,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Preset::Flat => t("Flat"),
            Preset::Pop => t("Pop"),
            Preset::Rock => t("Rock"),
            Preset::HipHop => t("Hip-hop"),
            Preset::Electronic => t("Electronic"),
            Preset::Jazz => t("Jazz"),
            Preset::Classical => t("Classical"),
            Preset::Vocal => t("Vocal"),
            Preset::Metal => t("Metal"),
            Preset::RnB => t("R&B"),
        }
    }

    /// Gain per band (dB). Gentle curves: ±4.5 dB at most.
    fn gains(self) -> [f32; 10] {
        match self {
            Preset::Flat => [0.0; 10],
            Preset::Pop => [-1.0, 0.0, 1.5, 2.5, 2.0, 0.5, -0.5, 0.5, 1.5, 2.0],
            Preset::Rock => [3.0, 2.5, 1.5, 0.0, -1.0, -0.5, 1.0, 2.0, 3.0, 3.0],
            Preset::HipHop => [4.5, 4.0, 2.0, 1.0, -0.5, -1.0, 0.5, 1.0, 1.5, 2.0],
            Preset::Electronic => [4.0, 3.5, 1.5, 0.0, -1.0, 0.5, 0.0, 1.5, 3.0, 3.5],
            Preset::Jazz => [2.0, 1.5, 1.0, 1.5, -1.0, -1.0, 0.0, 1.0, 2.0, 2.5],
            Preset::Classical => [2.5, 2.0, 1.0, 0.5, -0.5, -0.5, 0.0, 1.0, 2.0, 2.5],
            Preset::Vocal => [-2.0, -1.0, 0.0, 1.5, 3.0, 3.0, 2.5, 1.5, 0.5, 0.0],
            Preset::Metal => [3.5, 3.0, 1.0, 0.0, -1.5, -1.0, 0.5, 2.5, 3.0, 2.5],
            Preset::RnB => [3.0, 3.5, 2.5, 0.5, -1.0, -0.5, 1.0, 1.5, 2.0, 2.0],
        }
    }

    /// mpv `af` entry (None for Flat). Lowered first by the largest boost, so
    /// boosted bands can't clip; the leveling after it brings loudness back.
    pub fn filter(self) -> Option<String> {
        let g = self.gains();
        if g.iter().all(|x| *x == 0.0) {
            return None;
        }
        let max = g.iter().cloned().fold(0.0f32, f32::max);
        let mut parts = vec![format!("volume=volume=-{max:.1}dB")];
        for (f, gain) in BANDS.iter().zip(g) {
            if gain != 0.0 {
                parts.push(format!("equalizer=f={f}:t=o:w=1:g={gain:.1}"));
            }
        }
        Some(format!("lavfi=[{}]", parts.join(",")))
    }

    /// The preset for Deezer's genre names (album genres, most specific
    /// first as Deezer lists them); Flat when nothing fits.
    pub fn for_genres(genres: &[String]) -> Preset {
        for g in genres {
            let g = g.to_lowercase();
            let has = |words: &[&str]| words.iter().any(|w| g.contains(w));
            let p = if has(&["metal"]) {
                Preset::Metal
            } else if has(&["rap", "hip hop", "hip-hop", "reggae", "dancehall"]) {
                Preset::HipHop
            } else if has(&["r&b", "soul", "funk"]) {
                Preset::RnB
            } else if has(&[
                "electro", "dance", "techno", "house", "trance", "dubstep", "disco",
            ]) {
                Preset::Electronic
            } else if has(&["rock", "alternative", "indie", "punk"]) {
                Preset::Rock
            } else if has(&["jazz", "blues"]) {
                Preset::Jazz
            } else if has(&["classical", "opera", "film", "soundtrack"]) {
                Preset::Classical
            } else if has(&["folk", "singer", "acoustic", "country", "chanson"]) {
                Preset::Vocal
            } else if has(&["pop", "latin"]) {
                Preset::Pop
            } else {
                continue;
            };
            return p;
        }
        Preset::Flat
    }
}

/// Artist and song from a YouTube title and channel: "Artist - Song
/// (Official Video)" or the channel ("Artist - Topic", "ArtistVEVO").
pub fn artist_and_song(title: &str, channel: &str) -> Option<(String, String)> {
    // drop (…) and […] parts: "(Official Video)", "[4K Remaster]", …
    let mut clean = String::new();
    let mut depth = 0;
    for c in title.chars() {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' if depth > 0 => depth -= 1,
            _ if depth == 0 => clean.push(c),
            _ => {}
        }
    }
    let clean = clean.split('|').next().unwrap_or("").trim().to_string();
    let channel = channel
        .trim()
        .trim_end_matches(" - Topic")
        .trim_end_matches("VEVO")
        .trim()
        .to_string();
    for dash in [" - ", " – ", " — "] {
        if let Some((a, s)) = clean.split_once(dash) {
            let (a, s) = (a.trim(), s.trim());
            if !a.is_empty() && !s.is_empty() {
                return Some((a.to_string(), s.to_string()));
            }
        }
    }
    (!channel.is_empty() && !clean.is_empty()).then_some((channel, clean))
}

/// How a per-artist choice is keyed: the first credited artist, lowercase.
pub fn artist_key(artist: &str) -> String {
    let a = artist.to_lowercase();
    let first = ["feat.", "ft.", " x ", ",", "&"]
        .iter()
        .fold(a.as_str(), |s, sep| s.split(sep).next().unwrap_or(s));
    first.trim().to_string()
}

/// Ask Deezer for the song's album genres. The best result must be by the
/// same artist; otherwise there's no answer rather than a wrong one.
pub fn lookup(artist: &str, song: &str) -> Result<Vec<String>> {
    let agent = crate::innertube::shared_agent();
    let get = |url: &str| -> Result<Value> {
        let text = agent
            .get(url)
            .call()
            .context("contacting Deezer")?
            .body_mut()
            .read_to_string()?;
        Ok(serde_json::from_str(&text)?)
    };
    let q: String = format!("{artist} {song}")
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect();
    let r = get(&format!("https://api.deezer.com/search?limit=5&q={q}"))?;
    let want = artist_key(artist);
    let album = r
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|d| {
            let name = d
                .pointer("/artist/name")
                .and_then(Value::as_str)
                .map(artist_key)
                .unwrap_or_default();
            !name.is_empty() && (name.contains(&want) || want.contains(&name))
        })
        .and_then(|d| d.pointer("/album/id"))
        .and_then(Value::as_u64)
        .context("not on Deezer")?;
    let a = get(&format!("https://api.deezer.com/album/{album}"))?;
    Ok(a.pointer("/genres/data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|g| g.get("name")?.as_str().map(str::to_string))
        .collect())
}

/// What the EQ menu shows: the preset in effect, whether you chose it, and
/// what Auto would pick (None: still looking).
#[derive(Clone, Debug, PartialEq)]
pub struct View {
    pub current: Preset,
    pub manual: bool,
    pub auto: Option<Preset>,
}

/// A choice in the EQ menu: Auto (`None`) or a preset.
pub type Pick = Option<Preset>;

/// The EQ menu: Auto (with its pick), then the presets.
pub fn menu(ui: &mut eframe::egui::Ui, v: &View) -> Option<Pick> {
    let mut pick = None;
    let auto = match v.auto {
        Some(p) => crate::i18n::tf("Auto ({})", &[&p.label()]),
        None => t("Auto (looking up the genre…)").to_string(),
    };
    if ui.selectable_label(!v.manual, auto).clicked() {
        pick = Some(None);
    }
    ui.separator();
    for p in Preset::ALL {
        if ui
            .selectable_label(v.manual && v.current == p, p.label())
            .clicked()
        {
            pick = Some(Some(p));
        }
    }
    pick
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_youtube_titles() {
        assert_eq!(
            artist_and_song(
                "Rick Astley - Never Gonna Give You Up (Official Video) (4K Remaster)",
                "Rick Astley"
            ),
            Some(("Rick Astley".into(), "Never Gonna Give You Up".into()))
        );
        assert_eq!(
            artist_and_song("Şımarık", "Tarkan - Topic"),
            Some(("Tarkan".into(), "Şımarık".into()))
        );
        assert_eq!(artist_key("Daft Punk ft. Pharrell Williams"), "daft punk");
    }

    #[test]
    fn maps_genres() {
        let g =
            |s: &[&str]| Preset::for_genres(&s.iter().map(|x| x.to_string()).collect::<Vec<_>>());
        assert_eq!(g(&["Rap/Hip Hop"]), Preset::HipHop);
        assert_eq!(g(&["Dance", "Pop"]), Preset::Electronic);
        assert_eq!(g(&["Jazz"]), Preset::Jazz);
        assert_eq!(g(&["Rock"]), Preset::Rock);
        assert_eq!(g(&["Something else"]), Preset::Flat);
        assert!(Preset::Flat.filter().is_none());
        assert!(Preset::Rock
            .filter()
            .unwrap()
            .starts_with("lavfi=[volume=volume=-3.0dB,equalizer=f=31"));
    }

    /// `cargo test -- --ignored deezer_genres --nocapture` (asks Deezer)
    #[test]
    #[ignore]
    fn deezer_genres() {
        for (a, s) in [
            ("Rick Astley", "Never Gonna Give You Up"),
            ("Eminem", "Without Me"),
            ("Metallica", "Enter Sandman"),
        ] {
            let g = lookup(a, s).unwrap();
            eprintln!("{a} — {s}: {g:?} → {:?}", Preset::for_genres(&g));
            assert!(!g.is_empty());
        }
    }
}
