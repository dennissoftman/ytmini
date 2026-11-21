//! Native stream lookup: ask YouTube's player API for the stream URLs
//! ourselves and solve its JS challenges in a long-running, sandboxed deno.
//! ~0.2 s, against ~1.4 s for even a warm yt-dlp (which first downloads the
//! whole watch page).
//!
//! YouTube changes this often, so everything here is a fast path only: any
//! surprise (an error, a live stream, a format we don't pick ourselves, a
//! URL that doesn't answer) returns an error and `resolver` asks yt-dlp. URLs
//! that later fail to play count as strikes; after two, this session goes
//! back to yt-dlp alone.
//!
//! The challenge solver is yt-dlp's own (yt-dlp/ejs, bundled with the
//! yt-dlp we manage), so it's updated along with yt-dlp. Deno runs it with no
//! permissions: it gets the player script on stdin and can't touch files or
//! the network. Cookies go only to youtube.com, as in `innertube`.

use crate::innertube::Client;
use crate::yt::{self, Direct, Tools, Video};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const SOLVER_HOST: &str = include_str!("native_solver.js");
/// How often to check which player script YouTube currently serves.
const PLAYER_CHECK: Duration = Duration::from_secs(30 * 60);
/// The solver is stopped after this long unused.
const IDLE_MAX: Duration = Duration::from_secs(20 * 60);
/// Native URLs that failed to play before this session stops using them.
const MAX_STRIKES: u32 = 2;
const BROWSER_UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36";

/// A YouTube client to ask as (the same ones yt-dlp uses by default).
struct Spec {
    name: &'static str,
    number: u32,
    client: fn() -> Value,
    user_agent: &'static str,
}

/// Premium accounts: no proof-of-origin token needed for its streams, and it
/// has the Premium-only formats.
const WEB_CREATOR: Spec = Spec {
    name: "web_creator",
    number: 62,
    client: || json!({"clientName": "WEB_CREATOR", "clientVersion": "1.20260708.06.00"}),
    user_agent: BROWSER_UA,
};

/// Everyone else: a TV client, whose streams need no token either.
const TV_SAMSUNG_UA: &str = "Mozilla/5.0 (SMART-TV; Linux; Tizen 2.4.0) AppleWebKit/538.1 (KHTML, like Gecko) Version/2.4.0 TV Safari/538.1";
const TV_SAMSUNG: Spec = Spec {
    name: "tv_samsung",
    number: 7,
    client: || {
        json!({
            "clientName": "TVHTML5",
            "clientVersion": "5.20260707",
            "deviceMake": "Samsung",
            "deviceModel": "UKS9800",
            "userAgent": TV_SAMSUNG_UA,
            "osName": "Tizen",
            "osVersion": "2.4.0",
        })
    },
    user_agent: TV_SAMSUNG_UA,
};

fn debug() -> bool {
    std::env::var_os("YTMINI_DEBUG").is_some()
}

// ---------- player script ----------

struct Player {
    id: String,
    code: Arc<String>,
    sts: u64,
    checked: Instant,
}

static PLAYER: Mutex<Option<Player>> = Mutex::new(None);

fn agent() -> ureq::Agent {
    crate::innertube::shared_agent()
}

fn get_text(agent: &ureq::Agent, url: &str) -> Result<String> {
    let mut r = agent.get(url).call().context("contacting YouTube")?;
    anyhow::ensure!(r.status() == 200, "HTTP {} for {url}", r.status());
    Ok(r.body_mut()
        .with_config()
        .limit(16 << 20)
        .read_to_string()?)
}

/// The current player script (id, code, signature timestamp). Public and
/// fetched without cookies; re-checked every half hour.
fn player() -> Result<(String, Arc<String>, u64)> {
    let mut slot = PLAYER.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(p) = slot.as_ref().filter(|p| p.checked.elapsed() < PLAYER_CHECK) {
        return Ok((p.id.clone(), p.code.clone(), p.sts));
    }
    let agent = agent();
    let api = get_text(&agent, "https://www.youtube.com/iframe_api")?;
    let id = player_id(&api).context("no player id in iframe_api")?;
    if let Some(p) = slot.as_mut().filter(|p| p.id == id) {
        p.checked = Instant::now();
        return Ok((p.id.clone(), p.code.clone(), p.sts));
    }
    let code = get_text(
        &agent,
        &format!("https://www.youtube.com/s/player/{id}/player_ias.vflset/en_US/base.js"),
    )?;
    let sts = signature_timestamp(&code).context("no signatureTimestamp in the player")?;
    if debug() {
        eprintln!("native: player {id} (sts {sts})");
    }
    let p = Player {
        id,
        code: Arc::new(code),
        sts,
        checked: Instant::now(),
    };
    let out = (p.id.clone(), p.code.clone(), p.sts);
    *slot = Some(p);
    Ok(out)
}

/// `…/s/player/<8 hex>/…` (slashes may be escaped as `\/`).
fn player_id(text: &str) -> Option<String> {
    let mut rest = text;
    while let Some(i) = rest.find("player") {
        rest = &rest[i + "player".len()..];
        let r = rest.strip_prefix("\\/").or_else(|| rest.strip_prefix('/'));
        if let Some(r) = r {
            let id: String = r.chars().take_while(|c| c.is_ascii_hexdigit()).collect();
            let after = &r[id.len()..];
            if id.len() == 8 && (after.starts_with('/') || after.starts_with("\\/")) {
                return Some(id);
            }
        }
    }
    None
}

/// `signatureTimestamp:20768` or `sts:20768`.
fn signature_timestamp(code: &str) -> Option<u64> {
    for key in ["signatureTimestamp", "sts"] {
        let mut rest = code;
        while let Some(i) = rest.find(key) {
            rest = &rest[i + key.len()..];
            let r = rest.trim_start().strip_prefix(':').map(str::trim_start);
            if let Some(r) = r {
                let digits: String = r.chars().take_while(char::is_ascii_digit).collect();
                if digits.len() == 5 {
                    return digits.parse().ok();
                }
            }
        }
    }
    None
}

// ---------- challenge solver (deno) ----------

struct Solver {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    used: Instant,
}

impl Drop for Solver {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

static SOLVER: Mutex<Option<Solver>> = Mutex::new(None);

/// yt-dlp's bundled challenge solver scripts (lib, core).
fn ejs_dir(tools: &Tools) -> Option<PathBuf> {
    let dir = tools.ytdlp.parent()?.join("_internal/yt_dlp_ejs/yt/solver");
    (dir.join("lib.min.js").is_file() && dir.join("core.min.js").is_file()).then_some(dir)
}

/// The full solver script, written to ytmini's private config folder.
fn solver_script(tools: &Tools) -> Result<PathBuf> {
    let dir = ejs_dir(tools).context("yt-dlp's challenge solver isn't available")?;
    let lib = std::fs::read_to_string(dir.join("lib.min.js"))?;
    let core = std::fs::read_to_string(dir.join("core.min.js"))?;
    let script = format!("{lib}\nObject.assign(globalThis, lib);\n{core}\n{SOLVER_HOST}");
    let file = yt::config_dir()?.join("plugins").join("ytmini_solver.js");
    if std::fs::read_to_string(&file).ok().as_deref() != Some(script.as_str()) {
        std::fs::create_dir_all(file.parent().context("plugins dir")?)?;
        let tmp = file.with_extension("tmp");
        std::fs::write(&tmp, &script)?;
        std::fs::rename(&tmp, &file)?;
    }
    Ok(file)
}

fn spawn_solver(tools: &Tools) -> Result<Solver> {
    let deno = tools.deno.as_ref().context("no deno")?;
    let script = solver_script(tools)?;
    let mut c = std::process::Command::new(deno);
    // no --allow-* flags: no files, network, env or subprocesses
    c.args([
        "run",
        "--no-prompt",
        "--no-remote",
        "--no-npm",
        "--no-config",
        "--no-lock",
        "--node-modules-dir=none",
    ])
    .arg(&script)
    .env_clear()
    .env("NO_COLOR", "1")
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(if debug() {
        Stdio::inherit()
    } else {
        Stdio::null()
    });
    if let Some(home) = std::env::var_os("HOME") {
        // deno keeps its compile cache under the home folder
        c.env("HOME", home);
    }
    let mut child = c.spawn().context("starting deno")?;
    let stdin = child.stdin.take().context("deno stdin")?;
    let stdout = BufReader::new(child.stdout.take().context("deno stdout")?);
    Ok(Solver {
        child,
        stdin,
        stdout,
        used: Instant::now(),
    })
}

impl Solver {
    fn ask(&mut self, req: &Value) -> Result<Value> {
        writeln!(self.stdin, "{req}")?;
        self.stdin.flush()?;
        let mut line = String::new();
        anyhow::ensure!(self.stdout.read_line(&mut line)? > 0, "deno exited");
        self.used = Instant::now();
        Ok(serde_json::from_str(&line)?)
    }
}

/// Solve n and sig challenges with the given player. Returns
/// challenge → answer maps.
fn solve(
    tools: &Tools,
    player_id: &str,
    code: &str,
    n: &[String],
    sig: &[String],
) -> Result<(HashMap<String, String>, HashMap<String, String>)> {
    let mut requests = Vec::new();
    if !n.is_empty() {
        requests.push(json!({"type": "n", "challenges": n}));
    }
    if !sig.is_empty() {
        requests.push(json!({"type": "sig", "challenges": sig}));
    }
    if requests.is_empty() {
        return Ok(Default::default());
    }
    let mut slot = SOLVER.lock().unwrap_or_else(|e| e.into_inner());
    if slot.is_none() {
        *slot = Some(spawn_solver(tools)?);
    }
    let solver = slot.as_mut().expect("just set");
    let mut req = json!({"player_url": player_id, "requests": requests});
    let mut out = solver.ask(&req);
    if out
        .as_ref()
        .is_ok_and(|o| o.get("type").and_then(Value::as_str) == Some("need_player"))
    {
        req["player"] = json!(code);
        out = solver.ask(&req);
    }
    let out = match out {
        Ok(o) => o,
        Err(e) => {
            *slot = None; // broken pipe etc.: start afresh next time
            return Err(e);
        }
    };
    if out.get("type").and_then(Value::as_str) != Some("result") {
        bail!(
            "challenge solver: {}",
            out.get("error")
                .and_then(Value::as_str)
                .unwrap_or("no result")
        );
    }
    let responses = out
        .get("responses")
        .and_then(Value::as_array)
        .context("solver: no responses")?;
    let mut maps = (HashMap::new(), HashMap::new());
    let kinds = (!n.is_empty())
        .then_some("n")
        .into_iter()
        .chain((!sig.is_empty()).then_some("sig"));
    for (kind, r) in kinds.zip(responses) {
        if r.get("type").and_then(Value::as_str) != Some("result") {
            bail!("challenge solver couldn't solve {kind}");
        }
        let data = r
            .get("data")
            .and_then(Value::as_object)
            .context("solver data")?;
        let m: HashMap<String, String> = data
            .iter()
            .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
            .collect();
        if kind == "n" {
            maps.0 = m;
        } else {
            maps.1 = m;
        }
    }
    Ok(maps)
}

// ---------- formats ----------

#[derive(Debug, Clone)]
struct Fmt {
    /// Plain URL, or the signatureCipher's (url, s, sp).
    url: String,
    cipher: Option<(String, String)>,
    video: bool,
    codec: String,
    height: u32,
    fps: u32,
    bitrate: u64,
    /// 0 low, 1 medium, 2 high (audio)
    quality: u8,
    channels: u32,
    /// audio track preference: 2 original (or the only one), 1 YouTube's
    /// default (e.g. an auto-dub in the viewer's language), 0 other, -1
    /// audio description. yt-dlp prefers the original the same way.
    track_rank: i8,
    /// dynamic-range-compressed or voice-boosted variant
    altered: bool,
}

fn parse_fmt(f: &Value) -> Option<Fmt> {
    // segmented (OTF) and live formats need more than a plain URL
    if f.get("type").and_then(Value::as_str) == Some("FORMAT_STREAM_TYPE_OTF")
        || f.get("targetDurationSec").is_some()
    {
        return None;
    }
    let mime = f.get("mimeType")?.as_str()?;
    let video = mime.starts_with("video/");
    if !video && !mime.starts_with("audio/") {
        return None;
    }
    let codec = mime
        .split("codecs=\"")
        .nth(1)?
        .split(['"', ','])
        .next()?
        .trim()
        .to_string();
    let (url, cipher) = match f.get("url").and_then(Value::as_str) {
        Some(u) => (u.to_string(), None),
        None => {
            let sc = f.get("signatureCipher").and_then(Value::as_str)?;
            let q = parse_query(sc);
            let get = |k: &str| q.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
            (
                get("url")?,
                Some((get("s")?, get("sp").unwrap_or("signature".into()))),
            )
        }
    };
    let num = |k: &str| f.get(k).and_then(Value::as_u64).unwrap_or(0);
    let tags = f
        .get("xtags")
        .and_then(Value::as_str)
        .map(|x| parse_xtags(x).unwrap_or_else(|| vec![("?".into(), String::new())]))
        .unwrap_or_default();
    let tag = |k: &str| tags.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
    let track_rank = match f.get("audioTrack") {
        None => 2,
        Some(t) => {
            let name = t
                .get("displayName")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_ascii_lowercase();
            let acont = tag("acont").unwrap_or("");
            if acont == "original" || name.contains("original") {
                2
            } else if acont == "descriptive" || name.contains("descriptive") {
                -1
            } else if t.get("audioIsDefault").and_then(Value::as_bool) == Some(true) {
                1
            } else {
                0
            }
        }
    };
    Some(Fmt {
        url,
        cipher,
        video,
        codec,
        height: num("height") as u32,
        fps: num("fps") as u32,
        bitrate: num("bitrate"),
        quality: match f.get("audioQuality").and_then(Value::as_str) {
            Some("AUDIO_QUALITY_HIGH") => 2,
            Some("AUDIO_QUALITY_MEDIUM") => 1,
            _ => 0,
        },
        channels: num("audioChannels") as u32,
        track_rank,
        // Language tracks carry tags too (acont, lang); any other tag
        // (drc, voice boost, or one we don't know yet) marks a variant.
        altered: f.get("isDrc").and_then(Value::as_bool).unwrap_or(false)
            || tags.iter().any(|(k, _)| k != "acont" && k != "lang"),
    })
}

/// `xtags`: base64url protobuf, a list of {1: key, 2: value} entries, e.g.
/// acont=dubbed-auto, lang=en-US. None if it doesn't decode.
fn parse_xtags(x: &str) -> Option<Vec<(String, String)>> {
    fn varint(b: &[u8], i: &mut usize) -> Option<usize> {
        let (mut v, mut shift) = (0usize, 0);
        loop {
            let c = *b.get(*i)?;
            *i += 1;
            v |= ((c & 0x7f) as usize) << shift;
            if c & 0x80 == 0 {
                return Some(v);
            }
            shift += 7;
            if shift > 28 {
                return None;
            }
        }
    }
    // (field number, bytes) of each length-delimited field
    fn fields(b: &[u8]) -> Option<Vec<(usize, &[u8])>> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < b.len() {
            let key = varint(b, &mut i)?;
            if key & 7 != 2 {
                return None;
            }
            let len = varint(b, &mut i)?;
            out.push((key >> 3, b.get(i..i.checked_add(len)?)?));
            i += len;
        }
        Some(out)
    }
    let raw = crate::innertube::b64url_decode(x)?;
    let mut tags = Vec::new();
    for (n, entry) in fields(&raw)? {
        if n != 1 {
            return None;
        }
        let kv = fields(entry)?;
        let get = |want: usize| {
            kv.iter()
                .find(|(n, _)| *n == want)
                .map(|(_, v)| String::from_utf8_lossy(v).into_owned())
        };
        tags.push((get(1)?, get(2).unwrap_or_default()));
    }
    Some(tags)
}

/// Our yt-dlp format strings (`yt::profile`): max height and whether 30 fps
/// is the cap. Anything else (audio only, custom) isn't ours to pick.
fn parse_format(format: &str) -> Option<(u32, bool)> {
    let first = format.split('/').next()?;
    let h = first.strip_prefix("bv*[height<=")?;
    let h: u32 = h.split(']').next()?.parse().ok()?;
    Some((h, first.contains("[fps<=30]")))
}

/// yt-dlp's audio codec preference (opus > vorbis > aac > … > eac3 > ac3);
/// unknown codecs (e.g. iamf) aren't picked.
fn audio_codec_rank(codec: &str) -> Option<u8> {
    let c = codec.to_ascii_lowercase();
    Some(if c == "opus" {
        5
    } else if c == "vorbis" {
        4
    } else if c.starts_with("mp4a") {
        3
    } else if c == "ec-3" {
        2
    } else if c == "ac-3" {
        1
    } else {
        return None;
    })
}

/// The same picks as our yt-dlp format chain: VP9 first, then anything but
/// AV1, then anything; within that the tallest, then highest frame rate,
/// then bitrate. Audio: original track, best quality, channels, codec,
/// bitrate (yt-dlp's default order).
fn pick(formats: &[Fmt], max_h: u32, fps30: bool) -> Option<(Fmt, Fmt)> {
    let fits = |f: &&Fmt| f.video && f.height <= max_h && (!fps30 || f.fps <= 30);
    let groups: [&dyn Fn(&Fmt) -> bool; 3] = [
        &|f| f.codec.starts_with("vp"),
        &|f| !f.codec.starts_with("av01"),
        &|_| true,
    ];
    let video = groups.iter().find_map(|g| {
        formats
            .iter()
            .filter(fits)
            .filter(|f| g(f))
            .max_by_key(|f| (f.height, f.fps, f.bitrate))
    })?;
    let audio = formats
        .iter()
        .filter(|f| !f.video && !f.altered)
        .filter_map(|f| Some((audio_codec_rank(&f.codec)?, f)))
        .max_by_key(|(rank, f)| (f.track_rank, f.quality, f.channels, *rank, f.bitrate))?
        .1;
    Some((video.clone(), audio.clone()))
}

// ---------- URLs ----------

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let hex = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let decoded = (b[i] == b'%')
            .then(|| Some(hex(*b.get(i + 1)?)? * 16 + hex(*b.get(i + 2)?)?))
            .flatten();
        match (decoded, b[i]) {
            (Some(v), _) => {
                out.push(v);
                i += 3;
            }
            (None, b'+') => {
                out.push(b' ');
                i += 1;
            }
            (None, c) => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn percent_encode(s: &str) -> String {
    s.bytes()
        .map(|c| match c {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (c as char).to_string()
            }
            _ => format!("%{c:02X}"),
        })
        .collect()
}

/// `a=1&b=2` → [(a, 1), (b, 2)], decoded.
fn parse_query(q: &str) -> Vec<(String, String)> {
    q.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (percent_decode(k), percent_decode(v))
        })
        .collect()
}

/// A query parameter of `url`, decoded.
fn query_param(url: &str, key: &str) -> Option<String> {
    let q = url.split_once('?')?.1;
    parse_query(q)
        .into_iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v)
}

/// `url` with `key` set to `value` (replaced, or appended).
fn set_param(url: &str, key: &str, value: &str) -> String {
    let (base, q) = url.split_once('?').unwrap_or((url, ""));
    let mut found = false;
    let mut parts: Vec<String> = q
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let k = p.split_once('=').map_or(p, |(k, _)| k);
            if percent_decode(k) == key {
                found = true;
                format!("{k}={}", percent_encode(value))
            } else {
                p.to_string()
            }
        })
        .collect();
    if !found {
        parts.push(format!("{}={}", percent_encode(key), percent_encode(value)));
    }
    format!("{base}?{}", parts.join("&"))
}

/// The playable URL for `f`, with its challenges answered.
fn finish_url(
    f: &Fmt,
    n: &HashMap<String, String>,
    sig: &HashMap<String, String>,
) -> Result<String> {
    let mut url = f.url.clone();
    if let Some((s, sp)) = &f.cipher {
        let answer = sig.get(s).context("signature not solved")?;
        url = set_param(&url, sp, answer);
    }
    // dev: YTMINI_NATIVE_BREAK=1 leaves n unsolved, so YouTube refuses the
    // stream: tests the fallback and failure reporting end to end
    let broken = std::env::var_os("YTMINI_NATIVE_BREAK").is_some();
    if let Some(challenge) = query_param(&url, "n").filter(|_| !broken) {
        let answer = n.get(&challenge).context("n not solved")?;
        url = set_param(&url, "n", answer);
    }
    Ok(url)
}

// ---------- failures ----------

/// "Not for the native lookup": yt-dlp handles it, nothing went wrong.
#[derive(Debug)]
pub struct Skip(pub &'static str);

impl std::fmt::Display for Skip {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for Skip {}

pub fn is_skip(e: &anyhow::Error) -> bool {
    e.is::<Skip>()
}

/// Failures this session, and the latest reason.
static FAILURES: Mutex<(u32, Option<String>)> = Mutex::new((0, None));
const LOG_NAME: &str = "native-lookup.log";

/// Our lookup failed where it shouldn't have (yt-dlp then managed, or the
/// URLs didn't play): append it to `logs/native-lookup.log` (time, versions,
/// video, reason; no links or cookies) and count it for the app to show.
pub fn record_failure(video_id: Option<&str>, reason: &str) {
    let reason = crate::crashlog::redact(reason).replace('\n', " ");
    if debug() {
        eprintln!("native: FAILURE {}: {reason}", video_id.unwrap_or("-"));
    }
    {
        let mut f = FAILURES.lock().unwrap_or_else(|e| e.into_inner());
        f.0 += 1;
        f.1 = Some(reason.clone());
    }
    let Some(dir) = crate::crashlog::dir() else {
        return;
    };
    let path = dir.join(LOG_NAME);
    // keep it small: start over past 256 KB (the old one stays as .old)
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > 256 * 1024) {
        let _ = std::fs::rename(&path, path.with_extension("log.old"));
    }
    let (date, time, zone) = crate::crashlog::now();
    let line = format!(
        "{date} {time} {zone}  ytmini {}  video {}  {reason}\n",
        env!("CARGO_PKG_VERSION"),
        video_id.unwrap_or("-"),
    );
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = f.write_all(line.as_bytes());
    }
}

/// (failures this session, latest reason).
pub fn failures() -> (u32, Option<String>) {
    FAILURES.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// (failures logged so far, the latest line's reason), from the log file.
pub fn logged_failures() -> (usize, Option<String>) {
    let Some(text) =
        crate::crashlog::dir().and_then(|d| std::fs::read_to_string(d.join(LOG_NAME)).ok())
    else {
        return (0, None);
    };
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let last = lines
        .last()
        .and_then(|l| l.split("  ").last())
        .map(str::to_string);
    (lines.len(), last)
}

// ---------- strikes ----------

struct Health {
    /// videos whose current URLs came from here
    ours: HashSet<String>,
    strikes: u32,
}

static HEALTH: std::sync::LazyLock<Mutex<Health>> = std::sync::LazyLock::new(|| {
    Mutex::new(Health {
        ours: HashSet::new(),
        strikes: 0,
    })
});

/// A video's stream failed to play: if its URLs came from here, count it.
pub fn report_failure(video_id: &str) {
    let mut h = HEALTH.lock().unwrap_or_else(|e| e.into_inner());
    if h.ours.remove(video_id) {
        h.strikes += 1;
        let strikes = h.strikes;
        drop(h);
        record_failure(
            Some(video_id),
            &format!(
                "its stream links didn't play (strike {strikes} of {MAX_STRIKES}{})",
                if strikes >= MAX_STRIKES {
                    "; yt-dlp only for the rest of this session"
                } else {
                    ""
                }
            ),
        );
    }
}

fn enabled() -> bool {
    std::env::var_os("YTMINI_NO_NATIVE").is_none()
        && HEALTH.lock().unwrap_or_else(|e| e.into_inner()).strikes < MAX_STRIKES
}

// ---------- lookup ----------

/// Resolve `video`'s streams for one of our format strings (blocking).
/// Errors that are a `Skip` mean "not for us" (live, audio only, turned
/// off); any other error is our lookup failing.
pub fn resolve(tools: &Tools, cookies: &Path, video: &Video, format: &str) -> Result<Direct> {
    if !enabled() {
        return Err(Skip("off for this session").into());
    }
    let Some((max_h, fps30)) = parse_format(format) else {
        return Err(Skip("format not handled natively").into());
    };
    if tools.deno.is_none() || ejs_dir(tools).is_none() {
        return Err(Skip("no challenge solver (system yt-dlp or no deno)").into());
    }
    let spec = if crate::innertube::premium() == Some(true) {
        &WEB_CREATOR
    } else {
        &TV_SAMSUNG
    };
    lookup(tools, cookies, video, max_h, fps30, spec)
        .map(|(d, _)| d)
        .map_err(|e| {
            if e.is::<Skip>() {
                e
            } else {
                e.context(spec.name)
            }
        })
}

fn lookup(
    tools: &Tools,
    cookies: &Path,
    video: &Video,
    max_h: u32,
    fps30: bool,
    spec: &Spec,
) -> Result<(Direct, String)> {
    let (player_id, code, sts) = player().context("player script")?;
    let client = Client::new(cookies)?;
    let r = client.post_as(
        "player",
        json!({
            "videoId": video.id,
            "playbackContext": {"contentPlaybackContext": {
                "html5Preference": "HTML5_PREF_WANTS",
                "signatureTimestamp": sts,
            }},
            "contentCheckOk": true,
            "racyCheckOk": true,
        }),
        (spec.client)(),
        spec.number,
        Some(spec.user_agent),
    )?;
    let status = r
        .pointer("/playabilityStatus/status")
        .and_then(Value::as_str)
        .unwrap_or("?");
    if status != "OK" {
        bail!("{} says {status}", spec.name);
    }
    if r.pointer("/videoDetails/isLive").and_then(Value::as_bool) == Some(true)
        || r.pointer("/videoDetails/isPostLiveDvr")
            .and_then(Value::as_bool)
            == Some(true)
    {
        return Err(Skip("live stream").into());
    }
    let formats: Vec<Fmt> = r
        .pointer("/streamingData/adaptiveFormats")
        .and_then(Value::as_array)
        .context("no adaptive formats")?
        .iter()
        .filter_map(parse_fmt)
        .collect();
    let (v, a) = pick(&formats, max_h, fps30).context("no format fits")?;
    let mut n = Vec::new();
    let mut sig = Vec::new();
    for f in [&v, &a] {
        if let Some((s, _)) = &f.cipher {
            sig.push(s.clone());
        }
        if let Some(c) = query_param(&f.url, "n") {
            if !n.contains(&c) {
                n.push(c);
            }
        }
    }
    let (n_map, sig_map) = solve(tools, &player_id, &code, &n, &sig)?;
    let video_url = finish_url(&v, &n_map, &sig_map)?;
    let audio_url = finish_url(&a, &n_map, &sig_map)?;
    // one byte, to be sure YouTube accepts what we built
    let resp = agent()
        .get(&video_url)
        .header("User-Agent", spec.user_agent)
        .header("Range", "bytes=0-0")
        .call()
        .context("checking the stream")?;
    let code = resp.status().as_u16();
    if !(code == 200 || code == 206) {
        bail!("stream check: HTTP {code}");
    }
    {
        let mut h = HEALTH.lock().unwrap_or_else(|e| e.into_inner());
        if h.ours.len() > 200 {
            h.ours.clear();
        }
        h.ours.insert(video.id.clone());
    }
    let picked = format!(
        "{} {}p{} + {} {}ch{}",
        v.codec,
        v.height,
        v.fps,
        a.codec,
        a.channels,
        match a.track_rank {
            2 => "",
            1 => " (default track)",
            _ => " (other track)",
        }
    );
    if debug() {
        eprintln!("native: {} via {} → {picked}", video.id, spec.name);
    }
    let direct = Direct {
        video: video_url,
        audio: Some(audio_url),
        user_agent: Some(spec.user_agent.to_string()),
        offline: false,
        title: r
            .pointer("/videoDetails/title")
            .and_then(Value::as_str)
            .map(str::to_string),
    };
    Ok((direct, picked))
}

// ---------- self-check ----------

/// Videos `--check-native` looks up: a plain video, a song, and a video
/// with auto-dubbed audio tracks (which broke the lookup in 0.1.9).
const CHECK_VIDEOS: [(&str, &str); 3] = [
    ("aqz-KE-bpKQ", "Big Buck Bunny"),
    ("dQw4w9WgXcQ", "song"),
    ("G9Kt7VNB46A", "auto-dubbed tracks"),
];

/// `ytmini --check-native [video ids…]`: look the check videos up the way
/// playback does (both clients, signature + n challenge, a one-byte stream
/// check), without playing anything, so nothing reaches the watch history.
/// Prints one line per lookup. Exit code: 0 all fine, 1 a lookup failed
/// (ours to fix), 2 can't check here (tools missing), 3 the saved session
/// expired (opening ytmini refreshes it from the browser). Each run is
/// appended to `logs/native-check.log`.
pub fn self_check(ids: &[String]) -> i32 {
    let tools = match crate::yt::Tools::locate() {
        Ok(t) => t,
        Err(_) => {
            println!("FAIL  yt-dlp or mpv not found (open ytmini once to install them)");
            return 2;
        }
    };
    if tools.deno.is_none() || ejs_dir(&tools).is_none() {
        println!("FAIL  no challenge solver (deno or ytmini's own yt-dlp missing)");
        return 2;
    }
    let cookies = match crate::yt::cookie_file() {
        Ok(c) if c.exists() => c,
        _ => {
            println!("FAIL  not signed in (no cookies)");
            return 2;
        }
    };
    println!("ytmini {} native lookup check", env!("CARGO_PKG_VERSION"));
    // Google retires our copy of the login every few days; that's not a
    // lookup failure, and every lookup would just fail the same way.
    if let Err(e) = crate::innertube::check_session(&cookies) {
        let e = format!("{e:#}");
        if e.contains(crate::innertube::SESSION_EXPIRED) {
            println!("SIGNED OUT  the saved YouTube session expired.");
            println!("Open ytmini: it refreshes the session from your browser. Then run the check again.");
            write_check_log("signed out", &[]);
            return 3;
        }
    }
    let videos: Vec<(String, String)> = if ids.is_empty() {
        CHECK_VIDEOS
            .iter()
            .map(|(id, what)| (id.to_string(), what.to_string()))
            .collect()
    } else {
        ids.iter().map(|id| (id.clone(), String::new())).collect()
    };
    let mut ok = true;
    let mut log = Vec::new();
    for (i, (id, what)) in videos.iter().enumerate() {
        for spec in [&WEB_CREATOR, &TV_SAMSUNG] {
            if i > 0 || spec.name != WEB_CREATOR.name {
                // spaced out, so YouTube doesn't take it for a bot
                std::thread::sleep(Duration::from_secs(2));
            }
            let video = Video {
                id: id.clone(),
                ..Default::default()
            };
            let t = Instant::now();
            let r = lookup(&tools, &cookies, &video, 1080, false, spec);
            let ms = t.elapsed().as_millis();
            let label = if what.is_empty() {
                id.clone()
            } else {
                format!("{id} ({what})")
            };
            let line = match r {
                Ok((_, picked)) => format!("ok    {label} via {}: {picked}, {ms} ms", spec.name),
                Err(e) => {
                    ok = false;
                    let reason = crate::crashlog::redact(&format!("{e:#}")).replace('\n', " ");
                    format!("FAIL  {label} via {}: {reason}", spec.name)
                }
            };
            println!("{line}");
            log.push(line);
        }
    }
    println!(
        "{}",
        if ok {
            "All good: ytmini finds streams on its own."
        } else {
            "Something failed: ytmini falls back to yt-dlp (slower) until it's fixed."
        }
    );
    write_check_log(if ok { "ok" } else { "FAILED" }, &log);
    if ok {
        0
    } else {
        1
    }
}

fn write_check_log(result: &str, lines: &[String]) {
    let Some(dir) = crate::crashlog::dir() else {
        return;
    };
    let (date, time, zone) = crate::crashlog::now();
    let mut entry = format!(
        "{date} {time} {zone}  ytmini {}  {result}\n",
        env!("CARGO_PKG_VERSION")
    );
    for l in lines {
        entry.push_str(&format!("    {l}\n"));
    }
    let path = dir.join("native-check.log");
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > 256 * 1024) {
        let _ = std::fs::rename(&path, path.with_extension("log.old"));
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = f.write_all(entry.as_bytes());
    }
}

/// Fetch the player script and have the solver parse it, so the first video
/// opens as fast as later ones (~1.5 s otherwise). No video is looked up.
/// Blocking; call off the UI thread.
pub fn warm_up(tools: &Tools) {
    if !enabled() || tools.deno.is_none() || ejs_dir(tools).is_none() {
        return;
    }
    let t = Instant::now();
    let r = (|| -> Result<()> {
        let (id, code, _) = player()?;
        let mut slot = SOLVER.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_none() {
            *slot = Some(spawn_solver(tools)?);
        }
        let solver = slot.as_mut().expect("just set");
        let out = solver.ask(&json!({"player_url": id, "player": *code, "requests": []}))?;
        anyhow::ensure!(
            out.get("type").and_then(Value::as_str) == Some("result"),
            "solver didn't take the player"
        );
        Ok(())
    })();
    match r {
        Ok(()) if debug() => {
            eprintln!("native: warmed up in {:.2}s", t.elapsed().as_secs_f32())
        }
        Ok(()) => {}
        Err(e) => record_failure(None, &format!("warm-up: {e:#}")),
    }
}

/// Stop the solver if it sat unused for a while.
pub fn reap_idle() {
    let mut slot = SOLVER.lock().unwrap_or_else(|e| e.into_inner());
    if slot.as_ref().is_some_and(|s| s.used.elapsed() > IDLE_MAX) {
        *slot = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_player_id() {
        assert_eq!(
            player_id(
                r#"var x="https:\/\/www.youtube.com\/s\/player\/ad8d59ae\/www-widgetapi.vflset";"#
            ),
            Some("ad8d59ae".into())
        );
        assert_eq!(
            player_id("/s/player/0123abcd/player_ias.vflset"),
            Some("0123abcd".into())
        );
        assert_eq!(player_id("player/xyz"), None);
    }

    #[test]
    fn finds_sts() {
        assert_eq!(
            signature_timestamp("a,signatureTimestamp:20768,b"),
            Some(20768)
        );
        assert_eq!(signature_timestamp("{sts: 20123}"), Some(20123));
        assert_eq!(signature_timestamp("nothing"), None);
    }

    #[test]
    fn reads_our_format_strings() {
        let p = yt::profile(
            yt::Quality::Auto,
            yt::Power {
                saver: true,
                on_battery: true,
            },
            1440,
        );
        assert_eq!(parse_format(&p.ytdl_format), Some((1080, true)));
        assert_eq!(parse_format("ba/b"), None);
    }

    #[test]
    fn query_roundtrip() {
        let u = "https://x.googlevideo.com/videoplayback?expire=1&n=ab%2Dc&mime=video%2Fwebm";
        assert_eq!(query_param(u, "n").as_deref(), Some("ab-c"));
        let u2 = set_param(u, "n", "zz_1");
        assert_eq!(query_param(&u2, "n").as_deref(), Some("zz_1"));
        assert!(u2.contains("mime=video%2Fwebm"));
        let u3 = set_param(u, "sig", "a=b");
        assert_eq!(query_param(&u3, "sig").as_deref(), Some("a=b"));
    }

    fn fmt(video: bool, codec: &str, h: u32, fps: u32, br: u64) -> Fmt {
        Fmt {
            url: String::new(),
            cipher: None,
            video,
            codec: codec.into(),
            height: h,
            fps,
            bitrate: br,
            quality: 0,
            channels: 2,
            track_rank: 2,
            altered: false,
        }
    }

    #[test]
    fn picks_like_ytdlp() {
        let mut a = fmt(false, "opus", 0, 0, 140_000);
        a.quality = 1;
        let mut aac51 = fmt(false, "mp4a.40.2", 0, 0, 390_000);
        aac51.quality = 2;
        aac51.channels = 6;
        let mut drc = aac51.clone();
        drc.altered = true;
        drc.bitrate = 999_999;
        let mut dub = aac51.clone();
        dub.track_rank = 0;
        dub.bitrate = 999_999;
        let fs = vec![
            fmt(true, "vp9", 1080, 60, 4_600_000),
            fmt(true, "vp9", 720, 30, 1_800_000),
            fmt(true, "avc1.4D401F", 720, 30, 2_500_000),
            fmt(true, "av01.0.08M.08", 720, 30, 900_000),
            a,
            aac51,
            drc,
            dub,
        ];
        // Big Buck Bunny on battery: yt-dlp picked 247 (vp9 720p30) + 258
        let (v, au) = pick(&fs, 1080, true).unwrap();
        assert_eq!((v.codec.as_str(), v.height, v.fps), ("vp9", 720, 30));
        assert_eq!(
            (au.codec.as_str(), au.channels, au.bitrate),
            ("mp4a.40.2", 6, 390_000)
        );
        let (v, _) = pick(&fs, 1080, false).unwrap();
        assert_eq!((v.height, v.fps), (1080, 60));
        // no VP9: anything but AV1
        let fs2: Vec<Fmt> = fs
            .iter()
            .filter(|f| !f.codec.starts_with("vp"))
            .cloned()
            .collect();
        let (v, _) = pick(&fs2, 1080, true).unwrap();
        assert!(v.codec.starts_with("avc1"));
    }

    #[test]
    fn reads_xtags() {
        // from a real auto-dubbed video (2026-11-21)
        assert_eq!(
            parse_xtags("ChQKBWFjb250EgtkdWJiZWQtYXV0bwoNCgRsYW5nEgVlbi1VUw").unwrap(),
            vec![
                ("acont".into(), "dubbed-auto".into()),
                ("lang".into(), "en-US".into())
            ]
        );
        assert_eq!(
            parse_xtags("ChEKBWFjb250EghvcmlnaW5hbAoNCgRsYW5nEgVlcy1VUw").unwrap()[0].1,
            "original"
        );
        assert_eq!(
            parse_xtags("CggKA2RyYxIBMQ").unwrap(),
            vec![("drc".into(), "1".into())]
        );
        assert!(parse_xtags("!!").is_none());
    }

    #[test]
    fn dubbed_tracks_pick_the_original() {
        let audio = |xtags: &str, name: &str, default: bool| {
            json!({
                "itag": 251, "url": "https://x/videoplayback?n=a",
                "mimeType": "audio/webm; codecs=\"opus\"", "bitrate": 130000,
                "audioQuality": "AUDIO_QUALITY_MEDIUM", "audioChannels": 2,
                "xtags": xtags,
                "audioTrack": {"displayName": name, "audioIsDefault": default},
            })
        };
        let fs: Vec<Fmt> = [
            json!({"itag": 248, "url": "https://x/videoplayback", "mimeType": "video/webm; codecs=\"vp9\"", "height": 1080, "fps": 30, "bitrate": 2000000}),
            audio("ChQKBWFjb250EgtkdWJiZWQtYXV0bwoNCgRsYW5nEgVlbi1VUw", "English (US)", true),
            audio("ChEKBWFjb250EghvcmlnaW5hbAoNCgRsYW5nEgVlcy1VUw", "Spanish (US) original", false),
        ]
        .iter()
        .filter_map(parse_fmt)
        .collect();
        assert!(fs.iter().all(|f| !f.altered));
        let (_, a) = pick(&fs, 1080, false).expect("a format fits");
        assert_eq!(a.track_rank, 2);
        // an unknown tag still marks a variant
        let odd = parse_fmt(&audio("CggKA2RyYxIBMQ", "x", true)).unwrap();
        assert!(odd.altered);
    }
}
