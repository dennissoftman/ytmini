//! Seek-bar previews from YouTube's storyboards: sprite sheets of small
//! frames (e.g. 359×180, one every 5 s, 3×3 per sheet) at i.ytimg.com.
//! Sheets are fetched without cookies, only when you hover the seek bar.

use anyhow::{Context, Result};

#[derive(Clone, Debug, PartialEq)]
pub struct Storyboard {
    /// URL template with `$N` for the sheet name.
    url: String,
    name: String,
    sigh: String,
    pub w: u32,
    pub h: u32,
    count: u32,
    pub cols: u32,
    pub rows: u32,
    /// Seconds between frames (0: spread over the video).
    interval: f64,
}

impl Storyboard {
    /// Parse `base|w#h#count#cols#rows#interval_ms#name#sigh|…`, picking `level`.
    pub fn parse(spec: &str, level: usize) -> Result<Storyboard> {
        let mut parts = spec.split('|');
        let base = parts.next().context("empty spec")?;
        let levels: Vec<&str> = parts.collect();
        let lv = levels
            .get(level)
            .or_else(|| levels.last())
            .context("no storyboard levels")?;
        let f: Vec<&str> = lv.split('#').collect();
        anyhow::ensure!(f.len() >= 8, "bad storyboard level");
        let n = |i: usize| f[i].parse::<u32>().context("bad storyboard number");
        let idx = levels.iter().position(|l| l == lv).unwrap_or(0);
        Ok(Storyboard {
            url: base.replace("$L", &idx.to_string()),
            name: f[6].to_string(),
            sigh: f[7].to_string(),
            w: n(0)?,
            h: n(1)?,
            count: n(2)?.max(1),
            cols: n(3)?.max(1),
            rows: n(4)?.max(1),
            interval: f[5].parse::<f64>().unwrap_or(0.0) / 1000.0,
        })
    }

    /// Frame for time `t`: (sheet number, index within the sheet).
    pub fn frame_at(&self, t: f64, duration: f64) -> (u32, u32) {
        let step = if self.interval > 0.0 {
            self.interval
        } else {
            duration.max(1.0) / self.count as f64
        };
        let i = ((t / step) as u32).min(self.count - 1);
        let per = self.cols * self.rows;
        (i / per, i % per)
    }

    pub fn sheet_url(&self, sheet: u32) -> String {
        let name = self.name.replace("$M", &sheet.to_string());
        format!("{}&sigh={}", self.url.replace("$N", &name), self.sigh)
    }
}

pub fn from_extras(e: &crate::innertube::Extras) -> Option<Storyboard> {
    let (spec, level) = e.storyboard.as_ref()?;
    Storyboard::parse(spec, *level).ok()
}

/// Download and decode one sheet (no cookies).
pub fn load_sheet(url: &str) -> Result<eframe::egui::ColorImage> {
    let bytes = ureq::get(url)
        .call()
        .context("storyboard request")?
        .body_mut()
        .read_to_vec()?;
    let img = image::load_from_memory(&bytes)?.to_rgba8();
    let size = [img.width() as usize, img.height() as usize];
    Ok(eframe::egui::ColorImage::from_rgba_unmultiplied(
        size,
        img.as_raw(),
    ))
}

#[cfg(test)]
mod tests {
    #[test]
    fn spec() {
        let spec = "https://i.ytimg.com/sb/ID/storyboard3_L$L/$N.jpg?sqp=x|48#27#100#10#10#0#default#rs$A|89#45#142#10#10#5000#M$M#rs$B|179#90#142#5#5#5000#M$M#rs$C|359#180#142#3#3#5000#M$M#rs$D";
        let sb = super::Storyboard::parse(spec, 3).unwrap();
        assert_eq!((sb.w, sb.h, sb.cols, sb.rows), (359, 180, 3, 3));
        assert_eq!(sb.frame_at(0.0, 710.0), (0, 0));
        assert_eq!(sb.frame_at(47.0, 710.0), (1, 0)); // frame 9 → second sheet
        assert_eq!(
            sb.sheet_url(1),
            "https://i.ytimg.com/sb/ID/storyboard3_L3/M1.jpg?sqp=x&sigh=rs$D"
        );
    }

    /// Live: `cargo test -- --ignored storyboard_live --nocapture`
    #[test]
    #[ignore]
    fn storyboard_live() {
        let cookies = crate::yt::cookie_file().unwrap();
        let e = crate::innertube::player_extras(&cookies, "z19HM7ANZlo").unwrap();
        let sb = super::from_extras(&e).unwrap();
        // chapters come from the same reply; YTMINI_CHAPTERS=<id> to try another video
        // (signed out: YTMINI_CHAPTER_COOKIES=<empty file>)
        if let Ok(ids) = std::env::var("YTMINI_CHAPTERS") {
            let jar = std::env::var("YTMINI_CHAPTER_COOKIES").map_or(cookies.clone(), Into::into);
            for id in ids.split(',') {
                let e = crate::innertube::player_extras(&jar, id).unwrap();
                let c = crate::chapters::parse(&e.description, e.duration);
                let first: Vec<_> = c
                    .iter()
                    .take(3)
                    .map(|c| (c.start, c.title.as_str()))
                    .collect();
                eprintln!("{id}: {} chapters {first:?}", c.len());
                let y = crate::innertube::chapters(id).unwrap();
                let first: Vec<_> = y
                    .iter()
                    .take(3)
                    .map(|c| (c.start, c.title.as_str()))
                    .collect();
                eprintln!("   youtube: {} chapters {first:?}", y.len());
            }
        }
        let img = super::load_sheet(&sb.sheet_url(1)).unwrap();
        eprintln!(
            "frame {}x{}, grid {}x{}, sheet {:?}",
            sb.w, sb.h, sb.cols, sb.rows, img.size
        );
        assert_eq!(
            img.size,
            [(sb.w * sb.cols) as usize, (sb.h * sb.rows) as usize]
        );
    }
}
