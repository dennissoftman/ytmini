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
    cookie: String,
    sapisid: Option<String>,
    agent: ureq::Agent,
}

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
        let cookie = pairs
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("; ");
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(15)))
            .http_status_as_error(false)
            .user_agent("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36")
            .build()
            .into();
        Ok(Client {
            cookie,
            sapisid,
            agent,
        })
    }

    pub fn signed_in(&self) -> bool {
        self.sapisid.is_some()
    }

    /// POST /youtubei/v1/<endpoint> with `body` merged into the client context.
    pub fn post(&self, endpoint: &str, mut body: Value) -> Result<Value> {
        body["context"] = json!({"client": {"clientName": "WEB", "clientVersion": CLIENT_VERSION, "hl": "en", "gl": "US"}});
        let mut req = self
            .agent
            .post(format!("{ORIGIN}/youtubei/v1/{endpoint}?prettyPrint=false"))
            .header("Origin", ORIGIN)
            .header("X-Origin", ORIGIN)
            .header("X-Youtube-Client-Name", "1")
            .header("X-Youtube-Client-Version", CLIENT_VERSION)
            .header("Content-Type", "application/json");
        if !self.cookie.is_empty() {
            req = req.header("Cookie", &self.cookie);
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
        let status = resp.status().as_u16();
        let text = resp
            .body_mut()
            .with_config()
            .limit(32 << 20)
            .read_to_string()
            .context("reading YouTube's reply")?;
        if status != 200 {
            bail!("YouTube API: HTTP {status}");
        }
        serde_json::from_str(&text).context("YouTube API: bad JSON")
    }
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
pub struct Pager {
    client: Client,
    endpoint: &'static str,
    first: Option<Value>,
    token: Option<String>,
    shorts: bool,
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
            client,
            endpoint,
            first: Some(first),
            token: None,
            shorts: matches!(source, Source::History),
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
        let mut out = Vec::new();
        let mut token = None;
        collect(&r, self.shorts, &mut out, &mut token);
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
fn collect(v: &Value, shorts: bool, out: &mut Vec<Video>, token: &mut Option<(bool, String)>) {
    match v {
        Value::Object(m) => {
            for (k, x) in m {
                let item = match k.as_str() {
                    "lockupViewModel" => lockup(x),
                    "videoRenderer"
                    | "gridVideoRenderer"
                    | "playlistVideoRenderer"
                    | "compactVideoRenderer" => renderer(x),
                    "shortsLockupViewModel" => {
                        if shorts {
                            short(x)
                        } else {
                            continue; // shelves of shorts: skip
                        }
                    }
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
                        collect(x, shorts, out, token);
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
        Value::Array(a) => a.iter().for_each(|x| collect(x, shorts, out, token)),
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
                }
            }
        }
    }
    if !v.is_playlist {
        let mut badges = Vec::new();
        find_all(
            x.get("contentImage")?,
            "thumbnailBadgeViewModel",
            &mut badges,
        );
        v.duration = badges
            .iter()
            .filter_map(|b| b.get("text")?.as_str())
            .find_map(parse_duration);
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
                .filter_map(|r| r.get("text")?.as_str().map(str::to_string)),
        );
    }
    for i in infos {
        if let Some(n) = parse_views(&i) {
            v.views = Some(n);
        } else if is_age(i.trim()) {
            v.age = Some(i.trim().to_string());
        }
    }
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
    let views = text(x.pointer("/overlayMetadata/secondaryText")).and_then(|s| parse_views(&s));
    Some(Video {
        id,
        title,
        views,
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
            eprintln!(
                "{:14} p1 {:3} in {t1:.2}s, p2 {b:3} (+{:.2}s)  {}",
                src.label(),
                a.len(),
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
