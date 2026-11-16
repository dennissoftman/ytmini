//! YouTube links handed to ytmini from outside: the clipboard ("Play copied
//! link") and, on macOS, "Open in ytmini" in the right-click Services menu.
//! Nothing here sees links you don't hand over.

use eframe::egui;
use std::sync::{Mutex, OnceLock};

#[derive(Clone, Debug, PartialEq)]
pub enum Link {
    Video { id: String, start: Option<f64> },
    Playlist(String),
    Channel { url: String, name: String },
}

impl Link {
    /// Button text, e.g. "Play copied link".
    pub fn verb(&self) -> &'static str {
        match self {
            Link::Video { .. } => "Play copied link",
            Link::Playlist(_) => "Open copied playlist",
            Link::Channel { .. } => "Open copied channel",
        }
    }
}

/// The first YouTube link in `text` (a bare URL, or a URL inside a sentence).
pub fn find(text: &str) -> Option<Link> {
    text.split_whitespace()
        .take(200)
        .find_map(|w| parse(w.trim_matches(|c: char| "<>()[]\"'.,;".contains(c))))
}

/// A single YouTube URL. Strict about the host so ordinary text never matches.
pub fn parse(url: &str) -> Option<Link> {
    let url = url.trim();
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    let (host, path_q) = rest.split_at(rest.find(['/', '?', '#']).unwrap_or(rest.len()));
    let host = host.to_ascii_lowercase();
    let host = ["www.", "m.", "music."]
        .iter()
        .find_map(|p| host.strip_prefix(*p))
        .unwrap_or(&host);
    // Old links put the time in the fragment: …/watch?v=ID#t=1m30s
    let (path_q, fragment) = path_q.split_once('#').unwrap_or((path_q, ""));
    let (path, query) = path_q.split_once('?').unwrap_or((path_q, ""));
    let param = |k: &str| {
        query
            .split('&')
            .chain(fragment.split('&'))
            .find_map(|kv| kv.strip_prefix(k)?.strip_prefix('='))
            .filter(|v| !v.is_empty())
    };
    let start = param("t").or_else(|| param("start")).and_then(parse_time);
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let video = |id: &str| {
        valid_id(id).then(|| Link::Video {
            id: id.to_string(),
            start,
        })
    };

    match host {
        "youtu.be" => video(segs.first()?),
        "youtube.com" | "youtube-nocookie.com" => match segs.as_slice() {
            ["watch"] => match param("v") {
                Some(v) => video(v),
                None => param("list").map(|l| Link::Playlist(l.to_string())),
            },
            ["playlist"] => param("list").map(|l| Link::Playlist(l.to_string())),
            ["shorts" | "live" | "embed" | "v", id, ..] => video(id),
            [h, ..] if h.starts_with('@') && h.len() > 1 => Some(Link::Channel {
                url: format!("https://www.youtube.com/{h}"),
                name: h.to_string(),
            }),
            ["channel" | "c" | "user", name, ..] => Some(Link::Channel {
                url: format!("https://www.youtube.com/{}/{name}", segs[0]),
                name: if segs[0] == "channel" {
                    "Channel".into()
                } else {
                    name.to_string()
                },
            }),
            _ => None,
        },
        _ => None,
    }
}

fn valid_id(id: &str) -> bool {
    id.len() == 11
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// "90", "90s", "1m30s", "1h2m3s" → seconds.
fn parse_time(t: &str) -> Option<f64> {
    if let Ok(n) = t.parse::<f64>() {
        return Some(n);
    }
    let (mut total, mut num) = (0.0, String::new());
    for c in t.chars() {
        if c.is_ascii_digit() {
            num.push(c);
        } else {
            let n: f64 = num.parse().ok()?;
            num.clear();
            total += n * match c {
                'h' => 3600.0,
                'm' => 60.0,
                's' => 1.0,
                _ => return None,
            };
        }
    }
    (num.is_empty() && total > 0.0).then_some(total)
}

// ---------------------------------------------------------------------------
// Inbox: links that arrive from the Services menu (on AppKit's main thread,
// outside egui's frame), picked up by the app on its next frame.
// ---------------------------------------------------------------------------

static INBOX: Mutex<Vec<String>> = Mutex::new(Vec::new());
static CTX: OnceLock<egui::Context> = OnceLock::new();

fn push(text: String) {
    INBOX.lock().unwrap_or_else(|e| e.into_inner()).push(text);
    if let Some(ctx) = CTX.get() {
        ctx.request_repaint();
    }
}

pub fn take() -> Vec<String> {
    std::mem::take(&mut *INBOX.lock().unwrap_or_else(|e| e.into_inner()))
}

/// Call once from the main thread after the app has started.
pub fn install(ctx: &egui::Context) {
    let _ = CTX.set(ctx.clone());
    #[cfg(target_os = "macos")]
    mac::install_services();
}

/// Current clipboard text, if any.
pub fn clipboard_text() -> Option<String> {
    arboard::Clipboard::new().ok()?.get_text().ok()
}

#[cfg(target_os = "macos")]
mod mac {
    use objc2::rc::Retained;
    use objc2::runtime::NSObject;
    use objc2::{define_class, msg_send, MainThreadMarker, MainThreadOnly};
    use objc2_app_kit::{NSApplication, NSPasteboard, NSPasteboardTypeString, NSPasteboardTypeURL};
    use objc2_foundation::NSString;

    define_class!(
        // SAFETY: NSObject has no subclassing requirements; no Drop impl.
        #[unsafe(super(NSObject))]
        #[thread_kind = MainThreadOnly]
        #[name = "YtminiServices"]
        struct Services;

        impl Services {
            /// NSServices → NSMessage "openYouTubeLink" in Info.plist.
            #[unsafe(method(openYouTubeLink:userData:error:))]
            fn open_link(
                &self,
                pboard: &NSPasteboard,
                _user_data: *mut NSString,
                _error: *mut *mut NSString,
            ) {
                // SAFETY: AppKit's pasteboard type constants.
                let (url_t, str_t) = unsafe { (NSPasteboardTypeURL, NSPasteboardTypeString) };
                let text = pboard
                    .stringForType(url_t)
                    .or_else(|| pboard.stringForType(str_t));
                if let Some(t) = text {
                    super::push(t.to_string());
                }
            }
        }
    );

    extern "C" {
        fn NSUpdateDynamicServices();
    }

    pub fn install_services() {
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        // SAFETY: plain NSObject init.
        let provider: Retained<Services> = unsafe { msg_send![Services::alloc(mtm), init] };
        let app = NSApplication::sharedApplication(mtm);
        // SAFETY: the provider implements the selector named in Info.plist.
        unsafe { app.setServicesProvider(Some(&provider)) };
        std::mem::forget(provider); // AppKit doesn't retain it; it lives for the whole run
                                    // SAFETY: AppKit function, no arguments; makes a fresh install show up.
        unsafe { NSUpdateDynamicServices() };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links() {
        let v = |id: &str, start| {
            Some(Link::Video {
                id: id.into(),
                start,
            })
        };
        assert_eq!(
            parse("https://www.youtube.com/watch?v=dQw4w9WgXcQ"),
            v("dQw4w9WgXcQ", None)
        );
        assert_eq!(
            parse("https://youtu.be/dQw4w9WgXcQ?si=abc&t=90"),
            v("dQw4w9WgXcQ", Some(90.0))
        );
        assert_eq!(
            parse("youtube.com/watch?feature=x&v=dQw4w9WgXcQ&t=1m30s"),
            v("dQw4w9WgXcQ", Some(90.0))
        );
        assert_eq!(
            parse("https://m.youtube.com/shorts/dQw4w9WgXcQ"),
            v("dQw4w9WgXcQ", None)
        );
        assert_eq!(
            parse("https://music.youtube.com/watch?v=dQw4w9WgXcQ&list=RDx"),
            v("dQw4w9WgXcQ", None)
        );
        assert_eq!(
            parse("https://www.youtube.com/playlist?list=PLabc"),
            Some(Link::Playlist("PLabc".into()))
        );
        assert_eq!(
            parse("https://www.youtube.com/@mkbhd/videos"),
            Some(Link::Channel {
                url: "https://www.youtube.com/@mkbhd".into(),
                name: "@mkbhd".into()
            })
        );
        assert_eq!(parse("https://example.com/watch?v=dQw4w9WgXcQ"), None);
        assert_eq!(parse("https://notyoutube.com/watch?v=dQw4w9WgXcQ"), None);
        assert_eq!(parse("https://www.youtube.com/feed/history"), None);
        assert_eq!(parse("dev=dQw4w9WgXcQ"), None);
        assert_eq!(
            find("look at this (https://youtu.be/dQw4w9WgXcQ) lol"),
            v("dQw4w9WgXcQ", None)
        );
        assert_eq!(
            parse("https://www.youtube.com/watch?v=dQw4w9WgXcQ#t=2m"),
            v("dQw4w9WgXcQ", Some(120.0))
        );
        assert_eq!(
            parse("https://youtu.be/dQw4w9WgXcQ?t=1h2m3s"),
            v("dQw4w9WgXcQ", Some(3723.0))
        );
        assert_eq!(
            parse("https://www.youtube.com/live/dQw4w9WgXcQ?t=42s"),
            v("dQw4w9WgXcQ", Some(42.0))
        );
        assert_eq!(
            parse("https://www.youtube.com/embed/dQw4w9WgXcQ?start=30"),
            v("dQw4w9WgXcQ", Some(30.0))
        );
        assert_eq!(parse_time("1h2m3s"), Some(3723.0));
        assert_eq!(parse_time("abc"), None);
    }
}
