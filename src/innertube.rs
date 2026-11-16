//! Direct calls to YouTube's own web API ("InnerTube", what youtube.com's
//! JavaScript uses), signed with the session cookies like the website does.
//!
//! Used as a fast path: one JSON request (~0.3–0.8 s) instead of yt-dlp
//! downloading the whole web page first. The response layout is YouTube's and
//! changes now and then, so the parser walks the JSON for known video shapes
//! and callers fall back to yt-dlp whenever it finds nothing.
//!
//! Cookie values are only ever sent to youtube.com, never logged.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{path::Path, time::Duration};

const ORIGIN: &str = "https://www.youtube.com";
const CLIENT_VERSION: &str = "2.20251101.00.00";

pub struct Client {
    /// name → value for youtube.com, updated from Set-Cookie replies
    pairs: std::sync::Mutex<Vec<(String, String)>>,
    sapisid: Option<String>,
    agent: ureq::Agent,
    jar: std::path::PathBuf,
    /// Ask for English replies whatever the interface language: for the few
    /// actions that find things by YouTube's English wording.
    english: bool,
}

/// Serialises cookies.txt rewrites across tab threads.
static JAR_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

impl Client {
    /// From a Netscape cookies.txt (yt-dlp's export).
    pub fn new(cookies: &Path) -> Result<Client> {
        let jar = std::fs::read_to_string(cookies).context("reading cookies")?;
        let mut pairs: Vec<(String, String)> = Vec::new();
        for line in jar.lines() {
            let line = line.strip_prefix("#HttpOnly_").unwrap_or(line);
            let f: Vec<&str> = line.split('\t').collect();
            if f.len() < 7 || line.starts_with('#') || !f[0].ends_with("youtube.com") {
                continue;
            }
            if !pairs.iter().any(|(n, _)| n == f[5]) {
                pairs.push((f[5].to_string(), f[6].to_string()));
            }
        }
        let sapisid = ["SAPISID", "__Secure-3PAPISID"]
            .iter()
            .find_map(|n| pairs.iter().find(|(k, _)| k == n))
            .map(|(_, v)| v.clone());
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(15)))
            .http_status_as_error(false)
            .user_agent("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36")
            .build()
            .into();
        Ok(Client {
            pairs: std::sync::Mutex::new(pairs),
            sapisid,
            agent,
            jar: cookies.to_path_buf(),
            english: false,
        })
    }

    /// No cookies at all: for public data we'd rather not tie to the account.
    pub fn anonymous() -> Client {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(15)))
            .http_status_as_error(false)
            .user_agent("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36")
            .build()
            .into();
        Client {
            pairs: std::sync::Mutex::new(Vec::new()),
            sapisid: None,
            agent,
            jar: std::path::PathBuf::new(),
            english: false,
        }
    }

    pub fn signed_in(&self) -> bool {
        self.sapisid.is_some()
    }

    /// POST /youtubei/v1/<endpoint> with `body` merged into the client context.
    pub fn post(&self, endpoint: &str, mut body: Value) -> Result<Value> {
        let (hl, gl) = crate::i18n::youtube_locale();
        let hl = if self.english { "en".to_string() } else { hl };
        body["context"] = json!({"client": {"clientName": "WEB", "clientVersion": CLIENT_VERSION, "hl": hl, "gl": gl}});
        let mut req = self
            .agent
            .post(format!("{ORIGIN}/youtubei/v1/{endpoint}?prettyPrint=false"))
            .header("Origin", ORIGIN)
            .header("X-Origin", ORIGIN)
            .header("X-Youtube-Client-Name", "1")
            .header("X-Youtube-Client-Version", CLIENT_VERSION)
            .header("Content-Type", "application/json");
        let cookie = {
            let p = self.pairs.lock().unwrap_or_else(|e| e.into_inner());
            p.iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("; ")
        };
        if !cookie.is_empty() {
            req = req.header("Cookie", &cookie);
        }
        if let Some(sapisid) = &self.sapisid {
            use sha1::{Digest, Sha1};
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_secs();
            let digest = Sha1::digest(format!("{ts} {sapisid} {ORIGIN}").as_bytes());
            let hash: String = digest.iter().map(|b| format!("{b:02x}")).collect();
            req = req
                .header("Authorization", &format!("SAPISIDHASH {ts}_{hash}"))
                .header("X-Goog-AuthUser", "0");
        }
        let mut resp = req.send(body.to_string()).context("contacting YouTube")?;
        let set: Vec<String> = resp
            .headers()
            .get_all("set-cookie")
            .iter()
            .filter_map(|v| v.to_str().ok().map(str::to_string))
            .collect();
        if !set.is_empty() {
            self.absorb(&set);
        }
        let status = resp.status().as_u16();
        let text = resp
            .body_mut()
            .with_config()
            .limit(32 << 20)
            .read_to_string()
            .context("reading YouTube's reply")?;
        if status == 401 && self.signed_in() {
            bail!("{SESSION_EXPIRED} (HTTP 401)");
        }
        if status != 200 {
            bail!("YouTube API: HTTP {status}");
        }
        let v: Value = serde_json::from_str(&text).context("YouTube API: bad JSON")?;
        if self.signed_in() && reports_signed_out(&v) {
            bail!("{SESSION_EXPIRED}");
        }
        Ok(v)
    }
}

/// Marker in errors when our saved session no longer signs in (Google
/// rotated it away); the app re-imports from the browser.
pub const SESSION_EXPIRED: &str = "YouTube session expired";

/// The reply's own tracking params say `logged_in=0`.
fn reports_signed_out(v: &Value) -> bool {
    v.pointer("/responseContext/serviceTrackingParams")
        .and_then(Value::as_array)
        .is_some_and(|svcs| {
            svcs.iter()
                .filter_map(|s| s.get("params")?.as_array())
                .flatten()
                .any(|p| {
                    p.get("key").and_then(Value::as_str) == Some("logged_in")
                        && p.get("value").and_then(Value::as_str) == Some("0")
                })
        })
}

impl Client {
    /// YouTube refreshes session cookies in its replies (yt-dlp used to save
    /// these for us). Keep them for later requests and write them back to
    /// cookies.txt so the session stays fresh. Values are never logged.
    fn absorb(&self, headers: &[String]) {
        let parsed: Vec<SetCookie> = headers.iter().filter_map(|h| SetCookie::parse(h)).collect();
        if parsed.is_empty() {
            return;
        }
        {
            let mut p = self.pairs.lock().unwrap_or_else(|e| e.into_inner());
            for c in &parsed {
                p.retain(|(n, _)| n != &c.name);
                if !c.expired() {
                    p.push((c.name.clone(), c.value.clone()));
                }
            }
        }
        let _guard = JAR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let Ok(jar) = std::fs::read_to_string(&self.jar) else {
            return;
        };
        let mut lines: Vec<String> = jar.lines().map(str::to_string).collect();
        for c in &parsed {
            let matches = |l: &String| {
                let l = l.strip_prefix("#HttpOnly_").unwrap_or(l);
                let f: Vec<&str> = l.split('\t').collect();
                f.len() >= 7
                    && f[5] == c.name
                    && f[0].trim_start_matches('.') == c.domain.trim_start_matches('.')
            };
            let old = lines.iter().position(matches);
            if c.expired() {
                if let Some(i) = old {
                    lines.remove(i);
                }
                continue;
            }
            let line = format!(
                "{}{}\tTRUE\t{}\t{}\t{}\t{}\t{}",
                if c.http_only { "#HttpOnly_" } else { "" },
                c.domain,
                c.path,
                if c.secure { "TRUE" } else { "FALSE" },
                c.expires.unwrap_or(0),
                c.name,
                c.value
            );
            match old {
                Some(i) => lines[i] = line,
                None => lines.push(line),
            }
        }
        let tmp = self.jar.with_extension("txt.rot");
        let mut text = lines.join("\n");
        text.push('\n');
        let _ = crate::yt::write_private(&tmp, &self.jar, text.as_bytes());
    }
}

struct SetCookie {
    name: String,
    value: String,
    domain: String,
    path: String,
    /// unix seconds; None = session cookie
    expires: Option<i64>,
    secure: bool,
    http_only: bool,
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

impl SetCookie {
    fn parse(h: &str) -> Option<SetCookie> {
        let mut parts = h.split(';').map(str::trim);
        let (name, value) = parts.next()?.split_once('=')?;
        let mut c = SetCookie {
            name: name.to_string(),
            value: value.to_string(),
            domain: ".youtube.com".into(),
            path: "/".into(),
            expires: None,
            secure: false,
            http_only: false,
        };
        let mut max_age = None;
        for p in parts {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            match k.to_ascii_lowercase().as_str() {
                "domain" => {
                    c.domain = if v.starts_with('.') {
                        v.to_string()
                    } else {
                        format!(".{v}")
                    }
                }
                "path" => c.path = v.to_string(),
                "max-age" => max_age = v.parse::<i64>().ok(),
                "expires" => c.expires = c.expires.or_else(|| parse_http_date(v)),
                "secure" => c.secure = true,
                "httponly" => c.http_only = true,
                _ => {}
            }
        }
        if let Some(m) = max_age {
            c.expires = Some(unix_now() + m);
        }
        // only YouTube's own cookies belong in this jar
        c.domain.ends_with("youtube.com").then_some(c)
    }

    fn expired(&self) -> bool {
        self.expires.is_some_and(|e| e <= unix_now())
    }
}

/// "Wed, 16-Nov-2027 18:00:00 GMT" (also with spaces instead of dashes).
fn parse_http_date(s: &str) -> Option<i64> {
    let s = s
        .split_once(',')
        .map_or(s, |(_, r)| r)
        .trim()
        .replace('-', " ");
    let f: Vec<&str> = s.split_whitespace().collect();
    let (d, mon, y) = (
        f.first()?.parse::<i64>().ok()?,
        *f.get(1)?,
        f.get(2)?.parse::<i64>().ok()?,
    );
    let m = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ]
    .iter()
    .position(|x| x.eq_ignore_ascii_case(mon))? as i64
        + 1;
    let t: Vec<i64> = f
        .get(3)?
        .split(':')
        .filter_map(|x| x.parse().ok())
        .collect();
    let y = if y < 100 { y + 2000 } else { y };
    // days from civil (Howard Hinnant)
    let (yy, mm) = if m <= 2 { (y - 1, m + 9) } else { (y, m - 3) };
    let era = yy.div_euclid(400);
    let yoe = yy - era * 400;
    let doy = (153 * mm + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + t.first()? * 3600 + t.get(1)? * 60 + t.get(2)?)
}

/// Tell YouTube `video_id` was watched, so it appears in the account's watch
/// history: the two stats pings youtube.com's player sends (same parameters
/// yt-dlp's --mark-watched uses). Blocking.
/// Cheap "are we still signed in?" probe (the account menu; nothing is
/// recorded). Errors with [`SESSION_EXPIRED`] when Google dropped us.
pub fn check_session(cookies: &Path) -> Result<()> {
    let c = Client::new(cookies)?;
    if !c.signed_in() {
        return Ok(());
    }
    c.post("account/account_menu", json!({}))?;
    Ok(())
}

pub fn mark_watched(cookies: &Path, video_id: &str) -> Result<()> {
    let c = Client::new(cookies)?;
    if !c.signed_in() {
        bail!("not signed in");
    }
    let r = c.post("player", json!({ "videoId": video_id }))?;
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-_";
    let mut seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos() as u64
        | 1;
    let cpn: String = (0..16)
        .map(|_| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            ALPHABET[(seed & 63) as usize] as char
        })
        .collect();
    for key in ["videostatsPlaybackUrl", "videostatsWatchtimeUrl"] {
        let url = r
            .pointer(&format!("/playbackTracking/{key}/baseUrl"))
            .and_then(Value::as_str)
            .context("YouTube didn't return tracking URLs")?;
        let (base, query) = url.split_once('?').unwrap_or((url, ""));
        let len: f64 = query
            .split('&')
            .find_map(|p| p.strip_prefix("len="))
            .and_then(|v| v.parse().ok())
            .unwrap_or(1.5);
        let at = format!("{}", len - 1.0);
        let replaced = ["ver", "cpn", "cmt", "el", "st", "et"];
        let mut params: Vec<String> = query
            .split('&')
            .filter(|p| !p.is_empty() && !replaced.contains(&p.split('=').next().unwrap_or("")))
            .map(str::to_string)
            .collect();
        params.extend([
            "ver=2".to_string(),
            format!("cpn={cpn}"),
            format!("cmt={at}"),
            "el=detailpage".to_string(),
            format!("st={at}"),
            format!("et={at}"),
        ]);
        c.get(&format!("{base}?{}", params.join("&")))?;
    }
    Ok(())
}

impl Client {
    /// A plain GET with the session cookies. Only to YouTube over HTTPS:
    /// the URL comes from YouTube's reply, and cookies never go elsewhere.
    fn get(&self, url: &str) -> Result<()> {
        let host = url
            .strip_prefix("https://")
            .and_then(|r| r.split(['/', '?', ':']).next())
            .unwrap_or("");
        if !(host == "youtube.com" || host.ends_with(".youtube.com")) {
            bail!("refusing to send the session to {host:?}");
        }
        let cookie = {
            let p = self.pairs.lock().unwrap_or_else(|e| e.into_inner());
            p.iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("; ")
        };
        let resp = self
            .agent
            .get(url)
            .header("Cookie", &cookie)
            .call()
            .context("contacting YouTube")?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            bail!("HTTP {status}");
        }
        Ok(())
    }
}

#[cfg(test)]
mod history_live {
    /// Removes the ids in YTMINI_REMOVE (comma-separated) from watch history.
    /// `YTMINI_REMOVE=id1,id2 cargo test -- --ignored remove_history --nocapture`
    #[test]
    #[ignore]
    fn remove_history() {
        let cookies = crate::yt::config_dir().unwrap().join("cookies.txt");
        let ids = std::env::var("YTMINI_REMOVE").unwrap();
        for id in ids.split(',').filter(|s| !s.is_empty()) {
            eprintln!(
                "{id}: {:?}",
                super::remove_from_history(&cookies, id).map_err(|e| format!("{e:#}"))
            );
        }
    }
}

#[cfg(test)]
mod chips_live {
    /// Dumps the home feed reply to YTMINI_DUMP_FEED (reads the feed only).
    #[test]
    #[ignore]
    fn dump_feed() {
        let cookies = crate::yt::config_dir().unwrap().join("cookies.txt");
        let c = super::Client::new(&cookies).unwrap();
        let id = std::env::var("YTMINI_BROWSE").unwrap_or_else(|_| "FEwhat_to_watch".into());
        let v = c
            .post("browse", serde_json::json!({ "browseId": id }))
            .unwrap();
        std::fs::write(std::env::var("YTMINI_DUMP_FEED").unwrap(), v.to_string()).unwrap();
    }
}

#[cfg(test)]
mod topics_live {
    /// Home categories load like the feed. `cargo test -- --ignored topics_load --nocapture`
    #[test]
    #[ignore]
    fn topics_load() {
        let cookies = crate::yt::config_dir().unwrap().join("cookies.txt");
        let mut feed = super::Pager::new(&cookies, &crate::yt::Source::Feed).unwrap();
        feed.next_page().unwrap();
        let chips = super::HOME_CHIPS.lock().unwrap().clone();
        eprintln!(
            "{} categories: {:?}",
            chips.len(),
            chips.iter().map(|c| &c.0).collect::<Vec<_>>()
        );
        for (title, token) in chips.into_iter().take(3) {
            let src = crate::yt::Source::Topic {
                title: title.clone(),
                token,
            };
            let mut p = super::Pager::new(&cookies, &src).unwrap();
            let a = p.next_page().unwrap().unwrap_or_default();
            let b = p
                .next_page()
                .map(|v| v.unwrap_or_default().len())
                .unwrap_or(0);
            eprintln!(
                "{title}: {} videos, page 2: {b}; first: {:?}",
                a.len(),
                a.first().map(|v| v.title())
            );
            assert!(!a.is_empty());
        }
    }
}

#[cfg(test)]
mod locale_live {
    /// How completely each tab parses in the language set by YTMINI_HL.
    /// Read-only tabs (no search, which would land in search history).
    #[test]
    #[ignore]
    fn locale_parse() {
        use crate::yt::Source;
        let cookies = crate::yt::config_dir().unwrap().join("cookies.txt");
        for src in [
            Source::Feed,
            Source::Subscriptions,
            Source::WatchLater,
            Source::History,
        ] {
            let mut p = super::Pager::new(&cookies, &src).unwrap();
            let v = p.next_page().unwrap().unwrap_or_default();
            let n = v.iter().filter(|v| !v.is_short && !v.is_playlist).count();
            let c = |f: &dyn Fn(&crate::yt::Video) -> bool| {
                v.iter()
                    .filter(|v| !v.is_short && !v.is_playlist && f(v))
                    .count()
            };
            eprintln!(
                "{:14} {n:3} videos: channel {:3} views {:3} age {:3} duration {:3} | e.g. {:?}",
                src.label(),
                c(&|v| v.channel.is_some()),
                c(&|v| v.views_label().is_some()),
                c(&|v| v.age.is_some()),
                c(&|v| v.duration.is_some()),
                v.iter()
                    .find(|v| !v.is_short)
                    .map(|v| (v.views_label(), v.age.clone()))
            );
        }
    }
}

#[cfg(test)]
mod chip_tests {
    #[test]
    fn related_chip_kind() {
        // tails of real tokens: …"\x18\x04\"\x07Related(\x05j\x0fwatch-next-feed"
        let enc = |b: &[u8]| {
            const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
            let mut s = String::new();
            for ch in b.chunks(3) {
                let n = ch
                    .iter()
                    .enumerate()
                    .fold(0u32, |n, (i, &x)| n | (x as u32) << (16 - 8 * i));
                for i in 0..=ch.len() {
                    s.push(A[(n >> (18 - 6 * i) & 63) as usize] as char);
                }
            }
            s
        };
        let rel = enc(b"\x12\x0d\x12\x0bjNQXAC9IVRw\x18\x04\"\x07Related(\x05j\x0fwatch-next-feed");
        let fy = enc(b"\x12\x0d\x12\x0bjNQXAC9IVRw\x18\x05\"\x07For you(\x06j\x0fwatch-next-feed");
        assert_eq!(super::chip_kind(&rel), Some(4));
        assert_eq!(super::chip_kind(&fy), Some(5));
        assert_eq!(super::b64url_decode("aGVsbG8"), Some(b"hello".to_vec()));
    }
}

#[cfg(test)]
mod related_live {
    /// Dumps a `next` continuation (YTMINI_CONT) to YTMINI_DUMP_FEED.
    #[test]
    #[ignore]
    fn dump_cont() {
        let cookies = crate::yt::config_dir().unwrap().join("cookies.txt");
        let tok = std::env::var("YTMINI_CONT").unwrap();
        let v = super::Client::new(&cookies)
            .unwrap()
            .post("next", serde_json::json!({ "continuation": tok }))
            .unwrap();
        std::fs::write(std::env::var("YTMINI_DUMP_FEED").unwrap(), v.to_string()).unwrap();
    }

    /// Watch info + two comment pages, read-only. `YTMINI_NEXT_ID=… cargo test -- --ignored watch_and_comments --nocapture`
    #[test]
    #[ignore]
    fn watch_and_comments() {
        let cookies = crate::yt::config_dir().unwrap().join("cookies.txt");
        let id = std::env::var("YTMINI_NEXT_ID").unwrap_or_else(|_| "jNQXAC9IVRw".into());
        let (rel, info) = super::watch_next(&cookies, &id).unwrap();
        eprintln!("{} related; {:?}", rel.len(), WatchShow(&info));
        let p1 = super::comments(info.comments.as_deref().unwrap()).unwrap();
        eprintln!(
            "count {:?}, {} comments, next {}",
            p1.count,
            p1.items.len(),
            p1.next.is_some()
        );
        for c in p1.items.iter().take(3) {
            eprintln!(
                "  {} · {} · {} likes · {} replies · pinned {}: {:.60}",
                c.author, c.published, c.likes, c.replies, c.pinned, c.text
            );
        }
        let p2 = super::comments(p1.next.as_deref().unwrap()).unwrap();
        eprintln!("page 2: {} comments", p2.items.len());
        assert!(!p1.items.is_empty() && !p2.items.is_empty());
        let pls = super::playlists_for(&cookies, &id).unwrap();
        eprintln!("{} playlists", pls.len());
    }

    /// No ids or tokens in the output.
    struct WatchShow<'a>(&'a super::WatchInfo);
    impl std::fmt::Debug for WatchShow<'_> {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            let w = self.0;
            write!(
                f,
                "like {:?} ({:?}), channel {:?} ({:?} subs), subscribed {:?}, comments {}",
                w.like,
                w.like_count,
                w.channel,
                w.subscribers,
                w.subscribed,
                w.comments.is_some()
            )
        }
    }

    /// Dumps `playlist/get_add_to_playlist` for YTMINI_NEXT_ID (read-only).
    #[test]
    #[ignore]
    fn dump_add_to_playlist() {
        let cookies = crate::yt::config_dir().unwrap().join("cookies.txt");
        let id = std::env::var("YTMINI_NEXT_ID").unwrap();
        let v = super::Client::new(&cookies)
            .unwrap()
            .post(
                "playlist/get_add_to_playlist",
                serde_json::json!({ "videoIds": [id] }),
            )
            .unwrap();
        std::fs::write(std::env::var("YTMINI_DUMP_FEED").unwrap(), v.to_string()).unwrap();
    }

    /// Dumps `next` for YTMINI_NEXT_ID to YTMINI_DUMP_FEED (signed in, read-only).
    #[test]
    #[ignore]
    fn dump_next() {
        let cookies = crate::yt::config_dir().unwrap().join("cookies.txt");
        let id = std::env::var("YTMINI_NEXT_ID").unwrap();
        let v = super::Client::new(&cookies)
            .unwrap()
            .post("next", serde_json::json!({ "videoId": id }))
            .unwrap();
        std::fs::write(std::env::var("YTMINI_DUMP_FEED").unwrap(), v.to_string()).unwrap();
    }

    /// `cargo test -- --ignored related_videos --nocapture`
    #[test]
    #[ignore]
    fn related_videos() {
        let cookies = crate::yt::config_dir().unwrap().join("cookies.txt");
        let id = std::env::var("YTMINI_NEXT_ID").unwrap_or_else(|_| "aqz-KE-bpKQ".into());
        let r = super::related(&cookies, &id).unwrap();
        eprintln!(
            "{} suggestions; first: {:?}",
            r.len(),
            r.iter()
                .take(6)
                .map(|v| (v.title(), v.channel(), v.views_label()))
                .collect::<Vec<_>>()
        );
        assert!(r.len() >= 5);
    }
}

#[cfg(test)]
mod session_live {
    /// Is the saved session still signed in? Reads the home feed only.
    /// `cargo test -- --ignored session_status --nocapture`
    #[test]
    #[ignore]
    fn session_status() {
        let cookies = crate::yt::config_dir().unwrap().join("cookies.txt");
        eprintln!(
            "probe: {:?}",
            super::check_session(&cookies).map_err(|e| format!("{e:#}"))
        );
        // the probe can only notice expiry if the reply carries logged_in
        let v = super::Client::new(&cookies)
            .unwrap()
            .post("account/account_menu", serde_json::json!({}))
            .unwrap();
        let has = v.to_string().contains("\"logged_in\"");
        eprintln!("account_menu reports logged_in: {has}");
        assert!(has);
        let mut p = super::Pager::new(&cookies, &crate::yt::Source::Feed).unwrap();
        match p.next_page() {
            Ok(v) => {
                let v = v.unwrap_or_default();
                let shorts: Vec<&str> = v
                    .iter()
                    .filter(|x| x.is_short)
                    .map(|x| x.id.as_str())
                    .collect();
                eprintln!("signed in: {} videos, shorts {shorts:?}", v.len());
            }
            Err(e) => eprintln!("error: {e:#}"),
        }
    }
}

#[cfg(test)]
mod mark_live {
    /// Marks "Me at the zoo" watched natively, then checks History lists it first.
    /// `cargo test -- --ignored native_mark_watched --nocapture`
    #[test]
    #[ignore]
    fn native_mark_watched() {
        let cookies = crate::yt::config_dir().unwrap().join("cookies.txt");
        let id = "jNQXAC9IVRw";
        super::mark_watched(&cookies, id).unwrap();
        std::thread::sleep(std::time::Duration::from_secs(3));
        let mut p = super::Pager::new(&cookies, &crate::yt::Source::History).unwrap();
        let page = p.next_page().unwrap().unwrap();
        let pos = page.iter().position(|v| v.id == id);
        eprintln!("position in history: {pos:?}");
        assert!(pos.is_some_and(|p| p < 3));
    }
}

/// The video's storyboard spec (seek-bar preview sprites) and which level
/// YouTube recommends for high-resolution previews. Blocking.
/// What the player view needs beyond the stream, from one `player` call.
pub struct Extras {
    /// Seek-bar preview spec and level, when the video has them.
    pub storyboard: Option<(String, usize)>,
    pub description: String,
    pub duration: Option<f64>,
}

pub fn player_extras(cookies: &Path, video_id: &str) -> Result<Extras> {
    let r = Client::new(cookies)?.post("player", json!({ "videoId": video_id }))?;
    let storyboard = r
        .pointer("/storyboards/playerStoryboardSpecRenderer")
        .and_then(|sb| {
            let spec = sb.get("spec").and_then(Value::as_str)?;
            let level = sb
                .get("highResolutionRecommendedLevel")
                .or_else(|| sb.get("recommendedLevel"))
                .and_then(Value::as_u64)
                .unwrap_or(1) as usize;
            Some((spec.to_string(), level))
        });
    let description = r
        .pointer("/videoDetails/shortDescription")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let duration = r
        .pointer("/videoDetails/lengthSeconds")
        .and_then(Value::as_str)
        .and_then(|s| s.parse().ok());
    Ok(Extras {
        storyboard,
        description,
        duration,
    })
}

/// YouTube's suggestions for a video (the column beside the player on the
/// website), personalised like there. Reads only; nothing is recorded.
#[cfg(test)]
pub fn related(cookies: &Path, video_id: &str) -> Result<Vec<Video>> {
    Ok(watch_next(cookies, video_id)?.0)
}

/// Your rating of a video.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[allow(clippy::enum_variant_names)] // Like::Like reads as YouTube's own words
pub enum Like {
    #[default]
    None,
    Like,
    Dislike,
}

/// The parts of the watch page beyond the video: your rating, the channel
/// and whether you're subscribed, where comments start.
#[derive(Clone, Debug, Default)]
pub struct WatchInfo {
    pub like: Like,
    /// As YouTube shows it ("20 млн").
    pub like_count: Option<String>,
    pub channel_id: Option<String>,
    pub channel: Option<String>,
    pub subscribed: Option<bool>,
    pub subscribers: Option<String>,
    /// Continuation token of the comments (None: comments are off).
    pub comments: Option<String>,
    /// "448 699 364 просмотра", "23 апр. 2005 г." as YouTube writes them.
    pub views: Option<String>,
    pub date: Option<String>,
}

fn watch_info(r: &Value) -> WatchInfo {
    let mut w = WatchInfo::default();
    let contents = r
        .pointer("/contents/twoColumnWatchNextResults/results/results/contents")
        .and_then(Value::as_array);
    for c in contents.into_iter().flatten() {
        if let Some(p) = c.get("videoPrimaryInfoRenderer") {
            w.views = text(p.pointer("/viewCount/videoViewCountRenderer/viewCount"));
            w.date = text(p.get("dateText"));
            let seg = p
                .pointer("/videoActions/menuRenderer/topLevelButtons")
                .and_then(Value::as_array)
                .and_then(|b| {
                    b.iter()
                        .find_map(|b| b.get("segmentedLikeDislikeButtonViewModel"))
                });
            if let Some(l) = seg.and_then(|s| s.pointer("/likeButtonViewModel/likeButtonViewModel"))
            {
                w.like = match l
                    .pointer("/likeStatusEntity/likeStatus")
                    .and_then(Value::as_str)
                {
                    Some("LIKE") => Like::Like,
                    Some("DISLIKE") => Like::Dislike,
                    _ => Like::None,
                };
                let which = if w.like == Like::Like {
                    "toggled"
                } else {
                    "default"
                };
                w.like_count = l
                    .pointer(&format!("/toggleButtonViewModel/toggleButtonViewModel/{which}ButtonViewModel/buttonViewModel/title"))
                    .and_then(Value::as_str)
                    .filter(|t| !t.is_empty())
                    .map(str::to_string);
            }
        }
        if let Some(s) = c.get("videoSecondaryInfoRenderer") {
            if let Some(b) = s.pointer("/subscribeButton/subscribeButtonRenderer") {
                w.subscribed = b.get("subscribed").and_then(Value::as_bool);
                w.channel_id = b
                    .get("channelId")
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
            let owner = s.pointer("/owner/videoOwnerRenderer");
            w.channel = owner.and_then(|o| text(o.get("title")));
            w.subscribers = owner.and_then(|o| text(o.get("subscriberCountText")));
            if w.channel_id.is_none() {
                w.channel_id = owner
                    .and_then(|o| o.pointer("/navigationEndpoint/browseEndpoint/browseId"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
        }
        if let Some(sec) = c.get("itemSectionRenderer").filter(|s| {
            s.get("sectionIdentifier").and_then(Value::as_str) == Some("comment-item-section")
        }) {
            w.comments = sec
                .pointer("/contents/0/continuationItemRenderer/continuationEndpoint/continuationCommand/token")
                .and_then(Value::as_str)
                .map(str::to_string);
        }
    }
    w
}

/// The watch page, signed in: suggestions (see below) and [`WatchInfo`].
pub fn watch_next(cookies: &Path, video_id: &str) -> Result<(Vec<Video>, WatchInfo)> {
    let c = Client::new(cookies)?;
    let r = c.post("next", json!({ "videoId": video_id }))?;
    let info = watch_info(&r);
    let results = r
        .pointer("/contents/twoColumnWatchNextResults/secondaryResults/secondaryResults/results")
        .context("no suggestions in the reply")?;
    let tidy = |mut out: Vec<Video>| {
        out.retain(|v| v.id != video_id && !v.is_short);
        out
    };
    // Signed in, the default "All" list is mostly your home feed (nearly the
    // same whatever plays); the "Related" chip is what's actually related.
    if std::env::var_os("YTMINI_DEBUG").is_some() {
        eprintln!("related chip: {:?}", related_chip(results).map(|t| t.len()));
    }
    if let Some(token) = related_chip(results) {
        if let Ok(rel) = c.post("next", json!({ "continuation": token })) {
            let mut out = Vec::new();
            collect(&rel, &mut out, &mut None);
            let out = tidy(out);
            if std::env::var_os("YTMINI_DEBUG").is_some() {
                eprintln!("related list: {}", out.len());
            }
            if out.len() >= 5 {
                return Ok((out, info));
            }
        }
    }
    let mut out = Vec::new();
    collect(results, &mut out, &mut None);
    Ok((tidy(out), info))
}

/// Like, dislike or clear your rating.
pub fn set_like(cookies: &Path, video_id: &str, like: Like) -> Result<()> {
    let endpoint = match like {
        Like::Like => "like/like",
        Like::Dislike => "like/dislike",
        Like::None => "like/removelike",
    };
    Client::new(cookies)?.post(endpoint, json!({ "target": { "videoId": video_id } }))?;
    Ok(())
}

pub fn set_subscribed(cookies: &Path, channel_id: &str, on: bool) -> Result<()> {
    let endpoint = if on {
        "subscription/subscribe"
    } else {
        "subscription/unsubscribe"
    };
    Client::new(cookies)?.post(endpoint, json!({ "channelIds": [channel_id] }))?;
    Ok(())
}

/// One of your playlists, and whether the video is in it.
#[derive(Clone, Debug)]
pub struct PlaylistChoice {
    pub id: String,
    pub title: String,
    pub contains: bool,
}

/// Your playlists for "Save to playlist" (what the website's dialog lists).
pub fn playlists_for(cookies: &Path, video_id: &str) -> Result<Vec<PlaylistChoice>> {
    let r = Client::new(cookies)?.post(
        "playlist/get_add_to_playlist",
        json!({ "videoIds": [video_id] }),
    )?;
    let mut out = Vec::new();
    fn walk(v: &Value, out: &mut Vec<PlaylistChoice>) {
        match v {
            Value::Object(m) => {
                if let Some(o) = m.get("playlistAddToOptionRenderer") {
                    if let (Some(id), Some(title)) = (
                        o.get("playlistId").and_then(Value::as_str),
                        text(o.get("title")),
                    ) {
                        out.push(PlaylistChoice {
                            id: id.to_string(),
                            title,
                            contains: o.get("containsSelectedVideos").and_then(Value::as_str)
                                == Some("ALL"),
                        });
                    }
                    return;
                }
                m.values().for_each(|v| walk(v, out));
            }
            Value::Array(a) => a.iter().for_each(|v| walk(v, out)),
            _ => {}
        }
    }
    walk(&r, &mut out);
    anyhow::ensure!(!out.is_empty(), "no playlists in YouTube's reply");
    Ok(out)
}

pub fn set_in_playlist(cookies: &Path, playlist_id: &str, video_id: &str, add: bool) -> Result<()> {
    let action = if add {
        json!({ "addedVideoId": video_id, "action": "ACTION_ADD_VIDEO" })
    } else {
        json!({ "removedVideoId": video_id, "action": "ACTION_REMOVE_VIDEO_BY_VIDEO_ID" })
    };
    Client::new(cookies)?.post(
        "browse/edit_playlist",
        json!({ "playlistId": playlist_id, "actions": [action] }),
    )?;
    Ok(())
}

#[derive(Clone, Debug, Default)]
pub struct Comment {
    pub author: String,
    pub text: String,
    /// "6 years ago", as YouTube writes it.
    pub published: String,
    pub likes: String,
    pub replies: String,
    pub pinned: bool,
    pub verified: bool,
    /// Written by the video's uploader.
    pub creator: bool,
}

#[derive(Clone, Debug, Default)]
pub struct CommentsPage {
    pub items: Vec<Comment>,
    /// Token for the next page.
    pub next: Option<String>,
    /// "10,644,361 Comments" (first page only).
    pub count: Option<String>,
}

/// A page of comments. Read signed out: reading shouldn't touch the account.
pub fn comments(token: &str) -> Result<CommentsPage> {
    let r = Client::anonymous().post("next", json!({ "continuation": token }))?;
    let mut entities = std::collections::HashMap::new();
    for m in r
        .pointer("/frameworkUpdates/entityBatchUpdate/mutations")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let (Some(k), Some(p)) = (
            m.get("entityKey").and_then(Value::as_str),
            m.pointer("/payload/commentEntityPayload"),
        ) {
            entities.insert(k.to_string(), p);
        }
    }
    let mut page = CommentsPage::default();
    let s = |v: Option<&Value>| v.and_then(Value::as_str).unwrap_or_default().to_string();
    for e in r
        .get("onResponseReceivedEndpoints")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let items = e
            .pointer("/reloadContinuationItemsCommand/continuationItems")
            .or_else(|| e.pointer("/appendContinuationItemsAction/continuationItems"))
            .and_then(Value::as_array);
        for it in items.into_iter().flatten() {
            if let Some(h) = it.get("commentsHeaderRenderer") {
                page.count = text(h.get("countText"));
            } else if let Some(vm) =
                it.pointer("/commentThreadRenderer/commentViewModel/commentViewModel")
            {
                let Some(p) = vm
                    .get("commentKey")
                    .and_then(Value::as_str)
                    .and_then(|k| entities.get(k))
                else {
                    continue;
                };
                page.items.push(Comment {
                    author: s(p.pointer("/author/displayName")),
                    text: s(p.pointer("/properties/content/content")),
                    published: s(p.pointer("/properties/publishedTime")),
                    likes: s(p.pointer("/toolbar/likeCountNotliked")),
                    replies: s(p.pointer("/toolbar/replyCount")),
                    pinned: vm.get("pinnedText").is_some(),
                    verified: p
                        .pointer("/author/isVerified")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    creator: p
                        .pointer("/author/isCreator")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                });
            } else if let Some(c) = it.get("continuationItemRenderer") {
                page.next = c
                    .pointer("/continuationEndpoint/continuationCommand/token")
                    .or_else(|| {
                        c.pointer("/button/buttonRenderer/command/continuationCommand/token")
                    })
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
        }
    }
    Ok(page)
}

/// The continuation token of the watch page's "Related" chip. Found by the
/// chip kind inside the token (field 3 = 4; "For you" is 5), not by its
/// label, which is in your language.
fn related_chip(results: &Value) -> Option<String> {
    results
        .as_array()?
        .iter()
        .find_map(|r| r.pointer("/relatedChipCloudRenderer/content/chipCloudRenderer/chips"))?
        .as_array()?
        .iter()
        .filter_map(|c| {
            c.pointer("/chipCloudChipRenderer/navigationEndpoint/continuationCommand/token")?
                .as_str()
        })
        .find(|t| chip_kind(t) == Some(4))
        .map(str::to_string)
}

/// Field 3 of the chip's protobuf continuation token: the chip kind.
fn chip_kind(token: &str) -> Option<u64> {
    let raw = b64url_decode(&token.replace("%3D", "=").replace("%3d", "="))?;
    // the chip part sits at the end: … 0x18 <kind> 0x22 <len> <label> 0x28 …
    raw.windows(3)
        .rev()
        .find(|w| w[0] == 0x18 && w[2] == 0x22 && w[1] < 0x80)
        .map(|w| u64::from(w[1]))
}

/// Base64 (URL-safe or standard alphabet, padding optional).
fn b64url_decode(s: &str) -> Option<Vec<u8>> {
    let val = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            _ => return None,
        } as u32)
    };
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for &c in s.as_bytes().iter().filter(|&&c| c != b'=') {
        acc = (acc << 6) | val(c)?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// The chapters YouTube shows on the seek bar (the uploader's, else its
/// automatic ones). Asked signed out: chapters are public, and the watch-page
/// request shouldn't count toward anything on the account.
pub fn chapters(video_id: &str) -> Result<Vec<crate::chapters::Chapter>> {
    let r = Client::anonymous().post("next", json!({ "videoId": video_id }))?;
    let maps = r
        .pointer("/playerOverlays/playerOverlayRenderer/decoratedPlayerBarRenderer/decoratedPlayerBarRenderer/playerBar/multiMarkersPlayerBarRenderer/markersMap")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let pick = |key: &str| {
        maps.iter()
            .find(|m| m.get("key").and_then(Value::as_str) == Some(key))
            .and_then(|m| m.pointer("/value/chapters"))
            .and_then(Value::as_array)
            .filter(|c| !c.is_empty())
    };
    let Some(list) = pick("DESCRIPTION_CHAPTERS").or_else(|| pick("AUTO_CHAPTERS")) else {
        return Ok(Vec::new());
    };
    Ok(list
        .iter()
        .filter_map(|c| {
            let c = c.get("chapterRenderer")?;
            Some(crate::chapters::Chapter {
                start: c.get("timeRangeStartMillis")?.as_f64()? / 1000.0,
                title: text(c.get("title"))?,
            })
        })
        .collect())
}

/// Remove `video_id` from your YouTube watch history (what the website's
/// "Remove from watch history" does). Looks through the first few pages of
/// History for it. Returns false if it isn't there. Blocking.
pub fn remove_from_history(cookies: &Path, video_id: &str) -> Result<bool> {
    let mut client = Client::new(cookies)?;
    client.english = true; // the menu item is found by its English title
    let mut body = json!({ "browseId": "FEhistory" });
    let mut endpoint = "browse";
    for _ in 0..5 {
        let r = client.post(endpoint, body)?;
        let mut items = Vec::new();
        find_all(&r, "lockupViewModel", &mut items);
        for it in items {
            if it.get("contentId").and_then(Value::as_str) != Some(video_id) {
                continue;
            }
            let mut entries = Vec::new();
            find_all(it, "listItemViewModel", &mut entries);
            let token = entries.iter().find_map(|e| {
                let title = e.pointer("/title/content")?.as_str()?;
                if !title.contains("Remove from watch history") {
                    return None;
                }
                let mut eps = Vec::new();
                find_all(e, "feedbackEndpoint", &mut eps);
                eps.first()?
                    .get("feedbackToken")?
                    .as_str()
                    .map(str::to_string)
            });
            let token = token.context("no “Remove from watch history” item")?;
            let r = client.post(
                "feedback",
                json!({ "feedbackTokens": [token], "isFeedbackTokenUnencrypted": false, "shouldMerge": false }),
            )?;
            let ok = r
                .pointer("/feedbackResponses/0/isProcessed")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            anyhow::ensure!(ok, "YouTube didn't confirm the removal");
            return Ok(true);
        }
        let mut token = None;
        collect(&r, &mut Vec::new(), &mut token);
        let Some((_, t)) = token else { break };
        endpoint = "browse";
        body = json!({ "continuation": t });
    }
    Ok(false)
}

/// Add or remove `video_id` in Watch later. Blocking.
pub fn set_watch_later(cookies: &Path, video_id: &str, add: bool) -> Result<()> {
    let c = Client::new(cookies)?;
    if !c.signed_in() {
        bail!("not signed in (no SAPISID cookie)");
    }
    let action = if add {
        json!({"action": "ACTION_ADD_VIDEO", "addedVideoId": video_id})
    } else {
        json!({"action": "ACTION_REMOVE_VIDEO_BY_VIDEO_ID", "removedVideoId": video_id})
    };
    let r = c.post(
        "browse/edit_playlist",
        json!({"playlistId": "WL", "actions": [action]}),
    )?;
    if r.get("status").and_then(Value::as_str) != Some("STATUS_SUCCEEDED") {
        bail!("YouTube refused the change");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Listing
// ---------------------------------------------------------------------------

use crate::yt::{Source, Video};

/// Pages through one list with continuation tokens. Each `next_page` is one
/// request; nothing earlier is ever fetched again.
/// The home feed's categories from its latest load: (title, token).
pub static HOME_CHIPS: std::sync::Mutex<Vec<(String, String)>> = std::sync::Mutex::new(Vec::new());

/// "All", "Music", "Gaming"… from a home feed reply (skipping the selected
/// "All", which has no token).
fn home_chips(r: &Value) -> Vec<(String, String)> {
    let Some(chips) = r
        .pointer("/contents/twoColumnBrowseResultsRenderer/tabs/0/tabRenderer/content/richGridRenderer/header/feedFilterChipBarRenderer/contents")
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    chips
        .iter()
        .filter_map(|c| {
            let c = c.get("chipCloudChipRenderer")?;
            let title = text(c.get("text"))?;
            let token = c
                .pointer("/navigationEndpoint/continuationCommand/token")
                .and_then(Value::as_str)?;
            Some((title, token.to_string()))
        })
        .collect()
}

pub struct Pager {
    is_feed: bool,
    client: Client,
    endpoint: &'static str,
    first: Option<Value>,
    token: Option<String>,
    started: bool,
    /// On a channel page, items don't repeat the channel name.
    channel: Option<(String, String)>,
}

impl Pager {
    /// `None` when there's no native route for this source.
    pub fn new(cookies: &Path, source: &Source) -> Result<Pager> {
        let client = Client::new(cookies)?;
        let browse = |id: &str| ("browse", json!({ "browseId": id }));
        let (endpoint, first) = match source {
            Source::Feed => browse("FEwhat_to_watch"),
            Source::Subscriptions => browse("FEsubscriptions"),
            Source::WatchLater => browse("VLWL"),
            Source::History => browse("FEhistory"),
            Source::Playlists => browse("FEplaylist_aggregation"),
            Source::Downloads => bail!("downloads are local"),
            Source::Topic { token, .. } => ("browse", json!({ "continuation": token })),
            Source::Playlist { id, .. } => browse(&format!("VL{id}")),
            // "EgIQAQ==": videos only (no channels, shorts shelves, …)
            Source::Search(q) => ("search", json!({ "query": q, "params": "EgIQAQ%3D%3D" })),
            Source::Channel { url, .. } | Source::ChannelPlaylists { url, .. } => {
                let tab = if matches!(source, Source::Channel { .. }) {
                    "videos"
                } else {
                    "playlists"
                };
                let full = format!("{}/{tab}", url.trim_end_matches('/'));
                let r = client.post("navigation/resolve_url", json!({ "url": full }))?;
                let ep = r
                    .pointer("/endpoint/browseEndpoint")
                    .context("channel not found")?;
                let mut body =
                    json!({ "browseId": ep.get("browseId").cloned().unwrap_or(Value::Null) });
                if let Some(p) = ep.get("params") {
                    body["params"] = p.clone();
                }
                ("browse", body)
            }
        };
        Ok(Pager {
            is_feed: matches!(source, Source::Feed),
            client,
            endpoint,
            first: Some(first),
            token: None,
            started: false,
            channel: match source {
                Source::Channel { url, name } | Source::ChannelPlaylists { url, name } => {
                    Some((name.clone(), url.clone()))
                }
                _ => None,
            },
        })
    }

    /// The next page; `Ok(None)` when the list has ended.
    pub fn next_page(&mut self) -> Result<Option<Vec<Video>>> {
        let body = match (&self.first, &self.token) {
            (Some(b), _) => b.clone(),
            (None, Some(t)) => json!({ "continuation": t }),
            (None, None) => return Ok(None),
        };
        let first = !self.started;
        let r = self.client.post(self.endpoint, body)?; // on error, the same page can be retried
        self.first = None;
        self.token = None;
        self.started = true;
        if first && self.is_feed {
            let chips = home_chips(&r);
            if !chips.is_empty() {
                *HOME_CHIPS.lock().unwrap_or_else(|e| e.into_inner()) = chips;
            }
        }
        let mut out = Vec::new();
        let mut token = None;
        collect(&r, &mut out, &mut token);
        let token = token.map(|(_, t)| t);
        if let Some((name, url)) = &self.channel {
            for v in out
                .iter_mut()
                .filter(|v| v.channel.is_none() && !v.is_playlist)
            {
                v.channel = Some(name.clone());
                v.channel_url = Some(url.clone());
            }
        }
        if first && out.is_empty() {
            bail!("YouTube API: no videos in the reply (layout changed?)");
        }
        self.token = token;
        Ok(Some(out))
    }
}

/// Walk the reply in document order, picking up items we know and the last
/// continuation token. Matched items aren't descended into.
fn collect(v: &Value, out: &mut Vec<Video>, token: &mut Option<(bool, String)>) {
    match v {
        Value::Object(m) => {
            for (k, x) in m {
                let item = match k.as_str() {
                    "lockupViewModel" => lockup(x),
                    "videoRenderer"
                    | "gridVideoRenderer"
                    | "playlistVideoRenderer"
                    | "compactVideoRenderer" => renderer(x),
                    // kept and marked; the app hides them unless "Show Shorts" is on
                    "shortsLockupViewModel" => short(x),
                    "continuationItemRenderer" => {
                        // The list's own "more" token comes right after its items;
                        // tokens before any items belong to other sections/tabs.
                        let ours = token.as_ref().is_none_or(|(after_items, _)| !after_items);
                        if let Some(t) = x
                            .pointer("/continuationEndpoint/continuationCommand/token")
                            .and_then(Value::as_str)
                        {
                            if ours {
                                *token = Some((!out.is_empty(), t.to_string()));
                            }
                        }
                        continue;
                    }
                    "adSlotRenderer" | "channelRenderer" => continue,
                    _ => {
                        collect(x, out, token);
                        continue;
                    }
                };
                if let Some(item) = item {
                    if !out.iter().any(|o: &Video| o.id == item.id) {
                        out.push(item);
                    }
                }
            }
        }
        Value::Array(a) => a.iter().for_each(|x| collect(x, out, token)),
        _ => {}
    }
}

fn text(v: Option<&Value>) -> Option<String> {
    let v = v?;
    if let Some(s) = v
        .get("simpleText")
        .or_else(|| v.get("content"))
        .and_then(Value::as_str)
    {
        return Some(s.to_string());
    }
    let runs = v.get("runs")?.as_array()?;
    Some(
        runs.iter()
            .filter_map(|r| r.get("text").and_then(Value::as_str))
            .collect(),
    )
}

fn is_live_badge(t: &str) -> bool {
    let t = t.trim().to_ascii_uppercase();
    t == "LIVE" || t == "UPCOMING" || t.starts_with("PREMIER")
}

/// "1:02:03" → 3723
fn parse_duration(s: &str) -> Option<f64> {
    let parts: Vec<&str> = s.trim().split(':').collect();
    if parts.len() < 2 || parts.len() > 3 {
        return None;
    }
    parts.iter().try_fold(0.0, |acc, p| {
        p.parse::<u32>().ok().map(|n| acc * 60.0 + n as f64)
    })
}

/// "2,678,833 views" / "1.2M views" / "263K views" → count. "No views" → 0.
fn parse_views(s: &str) -> Option<u64> {
    let s = s.trim();
    if !s.ends_with("views") && !s.ends_with("view") {
        return None;
    }
    let num = s.split_whitespace().next()?.replace(',', "");
    if num.eq_ignore_ascii_case("no") {
        return Some(0);
    }
    let (n, mul) = match num.chars().last()? {
        'K' => (&num[..num.len() - 1], 1e3),
        'M' => (&num[..num.len() - 1], 1e6),
        'B' => (&num[..num.len() - 1], 1e9),
        _ => (num.as_str(), 1.0),
    };
    n.parse::<f64>().ok().map(|x| (x * mul).round() as u64)
}

fn is_age(s: &str) -> bool {
    s.ends_with(" ago") || s.starts_with("Streamed ") || s.starts_with("Premiere")
}

fn channel_from_nav(nav: Option<&Value>) -> Option<String> {
    let ep = nav?.get("browseEndpoint")?;
    if let Some(u) = ep.get("canonicalBaseUrl").and_then(Value::as_str) {
        return Some(format!("{ORIGIN}{u}"));
    }
    let id = ep.get("browseId")?.as_str()?;
    id.starts_with("UC")
        .then(|| format!("{ORIGIN}/channel/{id}"))
}

/// Find all values under `key` anywhere inside `v`.
fn find_all<'a>(v: &'a Value, key: &str, out: &mut Vec<&'a Value>) {
    match v {
        Value::Object(m) => {
            for (k, x) in m {
                if k == key {
                    out.push(x);
                }
                find_all(x, key, out);
            }
        }
        Value::Array(a) => a.iter().for_each(|x| find_all(x, key, out)),
        _ => {}
    }
}

/// The newer "lockup" items (home, subscriptions, history, channel, playlists).
fn lockup(x: &Value) -> Option<Video> {
    let id = x.get("contentId")?.as_str()?.to_string();
    let kind = x.get("contentType").and_then(Value::as_str).unwrap_or("");
    let meta = x.pointer("/metadata/lockupMetadataViewModel")?;
    let title = text(meta.get("title"));
    let mut v = Video {
        id,
        title,
        ..Default::default()
    };
    if kind.contains("PLAYLIST") || kind.contains("PODCAST") {
        v.is_playlist = true;
        let mut srcs = Vec::new();
        find_all(x.get("contentImage")?, "url", &mut srcs);
        v.thumb_id = srcs.iter().filter_map(|u| u.as_str()).find_map(|u| {
            let id: String = u
                .split("/vi/")
                .nth(1)?
                .chars()
                .take_while(|&c| c != '/')
                .collect();
            (id.len() == 11).then_some(id)
        });
    } else if v.id.len() != 11 {
        return None; // channels etc.
    }
    // metadata rows: [channel], [views, age] (order and presence vary)
    let mut other: Vec<String> = Vec::new();
    if let Some(rows) = meta
        .pointer("/metadata/contentMetadataViewModel/metadataRows")
        .and_then(Value::as_array)
    {
        for (ri, row) in rows.iter().enumerate() {
            for part in row
                .get("metadataParts")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let t = part.get("text");
                let Some(content) = t.and_then(|t| t.get("content")).and_then(Value::as_str) else {
                    continue;
                };
                if let Some(n) = parse_views(content) {
                    v.views = Some(n);
                } else if is_age(content) {
                    v.age = Some(content.to_string());
                } else if content.ends_with(" watching") {
                    v.duration = None; // live
                } else if ri == 0 && v.channel.is_none() && !v.is_playlist {
                    v.channel = Some(content.to_string());
                    let nav = t.and_then(|t| t.pointer("/commandRuns/0/onTap/innertubeCommand"));
                    v.channel_url = channel_from_nav(nav);
                } else if !v.is_playlist {
                    other.push(content.to_string());
                }
            }
        }
    }
    // Not English (the interface language): which part is views and which is
    // age differs by language, so show YouTube's own wording as one line.
    if v.views.is_none() && v.age.is_none() && !other.is_empty() {
        v.views_text = Some(other.join(" · "));
    }
    if !v.is_playlist {
        let mut badges = Vec::new();
        find_all(
            x.get("contentImage")?,
            "thumbnailBadgeViewModel",
            &mut badges,
        );
        let texts: Vec<&str> = badges
            .iter()
            .filter_map(|b| b.get("text")?.as_str())
            .collect();
        v.duration = texts.iter().find_map(|t| parse_duration(t));
        let styled_live = badges.iter().any(|b| {
            b.get("badgeStyle")
                .and_then(Value::as_str)
                .is_some_and(|s| s.contains("LIVE"))
        });
        v.is_live = v.duration.is_none() && (styled_live || texts.iter().any(|t| is_live_badge(t)));
    }
    Some(v)
}

/// Classic renderers (search results, Watch later / playlist entries).
fn renderer(x: &Value) -> Option<Video> {
    let id = x.get("videoId")?.as_str()?.to_string();
    let mut v = Video {
        id,
        title: text(x.get("title")),
        ..Default::default()
    };
    let owner = x
        .get("ownerText")
        .or_else(|| x.get("shortBylineText"))
        .or_else(|| x.get("longBylineText"));
    v.channel = text(owner).filter(|s| !s.is_empty());
    v.channel_url = channel_from_nav(owner.and_then(|o| o.pointer("/runs/0/navigationEndpoint")));
    v.duration = x
        .get("lengthSeconds")
        .and_then(|s| {
            s.as_str()
                .and_then(|s| s.parse().ok())
                .or_else(|| s.as_f64())
        })
        .or_else(|| text(x.get("lengthText")).and_then(|s| parse_duration(&s)));
    let mut infos: Vec<String> = Vec::new();
    for k in ["viewCountText", "publishedTimeText"] {
        infos.extend(text(x.get(k)));
    }
    if let Some(runs) = x.pointer("/videoInfo/runs").and_then(Value::as_array) {
        infos.extend(
            runs.iter()
                .filter_map(|r| r.get("text")?.as_str().map(|s| s.trim().to_string()))
                .filter(|s| s.chars().any(char::is_alphanumeric)),
        );
    }
    let mut other = Vec::new();
    for i in infos {
        let i = i.trim().to_string();
        if let Some(n) = parse_views(&i) {
            v.views = Some(n);
        } else if is_age(&i) {
            v.age = Some(i);
        } else if !i.is_empty() {
            other.push(i);
        }
    }
    // not English: YouTube's own wording, in its order (see lockup)
    if v.views.is_none() && v.age.is_none() && !other.is_empty() {
        v.views_text = Some(other.join(" · "));
    }
    v.is_live = v.duration.is_none()
        && (x.get("upcomingEventData").is_some()
            || x.get("thumbnailOverlays").is_some_and(|o| {
                let s = o.to_string();
                s.contains("\"LIVE\"") || s.contains("\"UPCOMING\"")
            }));
    v.description = x
        .pointer("/detailedMetadataSnippets/0/snippetText")
        .or_else(|| x.get("descriptionSnippet"))
        .and_then(|s| text(Some(s)));
    Some(v)
}

fn short(x: &Value) -> Option<Video> {
    let id = x
        .pointer("/onTap/innertubeCommand/reelWatchEndpoint/videoId")?
        .as_str()?
        .to_string();
    let title = text(x.pointer("/overlayMetadata/primaryText"));
    let secondary = text(x.pointer("/overlayMetadata/secondaryText"));
    let views = secondary.as_deref().and_then(parse_views);
    Some(Video {
        id,
        title,
        views,
        views_text: secondary.filter(|_| views.is_none()),
        is_short: true,
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parsers() {
        assert_eq!(parse_duration("1:02:03"), Some(3723.0));
        assert_eq!(parse_duration("2:29"), Some(149.0));
        assert_eq!(parse_duration("LIVE"), None);
        assert_eq!(parse_views("2,678,833 views"), Some(2_678_833));
        assert_eq!(parse_views("1.2M views"), Some(1_200_000));
        assert_eq!(parse_views("263K views"), Some(263_000));
        assert_eq!(parse_views("No views"), Some(0));
        assert_eq!(parse_views("5 hours ago"), None);
        assert!(is_age("Streamed 1 day ago"));
        assert_eq!(parse_http_date("Thu, 01-Jan-1970 00:00:10 GMT"), Some(10));
        assert_eq!(
            parse_http_date("Mon, 16 Nov 2026 18:00:00 GMT"),
            Some(1_794_852_000)
        );
        let c = SetCookie::parse(
            "VISITOR_INFO1_LIVE=abc; Domain=.youtube.com; Max-Age=100; Path=/; Secure; HttpOnly",
        )
        .unwrap();
        assert!(c.secure && c.http_only && !c.expired() && c.value == "abc");
        assert!(SetCookie::parse("X=1; Domain=.google.com").is_none());
    }
}

#[cfg(test)]
mod live {
    use super::*;

    /// Every tab through the native pager: page 1 + page 2, timing and a sample.
    /// `cargo test -- --ignored native_all_tabs --nocapture`
    #[test]
    #[ignore]
    fn native_all_tabs() {
        let cookies = crate::yt::config_dir().unwrap().join("cookies.txt");
        let sources = [
            Source::Feed,
            Source::Subscriptions,
            Source::WatchLater,
            Source::History,
            Source::Playlists,
            Source::Search("rust programming".into()),
            Source::Channel {
                url: "https://www.youtube.com/@NASA".into(),
                name: "NASA".into(),
            },
            Source::ChannelPlaylists {
                url: "https://www.youtube.com/@NASA".into(),
                name: "NASA".into(),
            },
            Source::Playlist {
                id: "PLrAXtmErZgOeiKm4sgNOknGvNjby9efdf".into(),
                title: "t".into(),
            },
        ];
        for src in sources {
            let t = std::time::Instant::now();
            let mut p = Pager::new(&cookies, &src).unwrap();
            let a = p.next_page().unwrap().unwrap_or_default();
            let t1 = t.elapsed().as_secs_f32();
            let b = p
                .next_page()
                .map(|o| o.unwrap_or_default().len())
                .unwrap_or(0);
            let s = a.first().map(|v| {
                format!(
                    "{} | {} | {:?} | {:?} | {:?} | pl={}",
                    v.title(),
                    v.channel(),
                    v.duration,
                    v.views,
                    v.age,
                    v.is_playlist
                )
            });
            let shorts = a.iter().filter(|v| v.is_short).count();
            let live: Vec<&str> = a.iter().filter(|v| v.is_live).map(|v| v.title()).collect();
            eprintln!(
                "{:14} p1 {:3} ({shorts} shorts, {} live {live:?}) in {t1:.2}s, p2 {b:3} (+{:.2}s)  {}",
                src.label(),
                a.len(),
                live.len(),
                t.elapsed().as_secs_f32() - t1,
                s.unwrap_or_default()
            );
            assert!(!a.is_empty(), "{} empty", src.label());
            assert!(a.iter().all(|v| v.is_playlist || v.id.len() == 11));
        }
    }
}

#[cfg(test)]
mod explore {
    use super::*;

    /// Dumps raw responses for each tab to $YTMINI_DUMP (for building the parser).
    #[test]
    #[ignore]
    fn dump_responses() {
        let dir = std::path::PathBuf::from(std::env::var("YTMINI_DUMP").unwrap());
        let c = Client::new(&crate::yt::config_dir().unwrap().join("cookies.txt")).unwrap();
        let reqs = [
            ("feed", "browse", json!({"browseId": "FEwhat_to_watch"})),
            ("subs", "browse", json!({"browseId": "FEsubscriptions"})),
            ("wl", "browse", json!({"browseId": "VLWL"})),
            ("history", "browse", json!({"browseId": "FEhistory"})),
            (
                "playlists",
                "browse",
                json!({"browseId": "FEplaylist_aggregation"}),
            ),
            ("search", "search", json!({"query": "rust programming"})),
            (
                "channel",
                "browse",
                json!({"browseId": "UCLA_DiR1FfKNvjuUpBHmylQ", "params": "EgZ2aWRlb3PyBgQKAjoA"}),
            ),
        ];
        for (name, ep, body) in reqs {
            let t = std::time::Instant::now();
            match c.post(ep, body) {
                Ok(v) => {
                    std::fs::write(dir.join(format!("{name}.json")), v.to_string()).unwrap();
                    eprintln!("{name}: {:.2}s", t.elapsed().as_secs_f32());
                }
                Err(e) => eprintln!("{name}: {e:#}"),
            }
        }
    }
}
