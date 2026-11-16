//! Media keys, headphone/AirPods buttons and Control Center's "Now Playing"
//! (macOS: MPRemoteCommandCenter + MPNowPlayingInfoCenter). Other systems:
//! no-ops for now.

use eframe::egui;
use std::sync::{Mutex, OnceLock};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Cmd {
    Toggle,
    Play,
    Pause,
    /// Absolute position, seconds (Control Center's scrubber).
    Seek(f64),
    /// Relative, seconds.
    Skip(f64),
}

/// What we last told the system, to skip redundant updates.
#[derive(Clone, Debug, PartialEq)]
pub struct Info {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub duration: f64,
    pub pos: f64,
    pub playing: bool,
    pub rate: f64,
}

impl Info {
    /// Worth re-publishing? The system extrapolates position from rate, so
    /// only state changes and jumps (seeks) need an update.
    pub fn differs(&self, old: &Info, since_secs: f64) -> bool {
        let expected = old.pos
            + if old.playing {
                since_secs * old.rate
            } else {
                0.0
            };
        self.id != old.id
            || self.title != old.title
            || self.playing != old.playing
            || (self.rate - old.rate).abs() > 1e-3
            || (self.duration - old.duration).abs() > 0.5
            || (self.pos - expected).abs() > 2.0
    }
}

static CMDS: Mutex<Vec<Cmd>> = Mutex::new(Vec::new());
static CTX: OnceLock<egui::Context> = OnceLock::new();

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn push(c: Cmd) {
    CMDS.lock().unwrap_or_else(|e| e.into_inner()).push(c);
    if let Some(ctx) = CTX.get() {
        ctx.request_repaint();
    }
}

pub fn take() -> Vec<Cmd> {
    std::mem::take(&mut *CMDS.lock().unwrap_or_else(|e| e.into_inner()))
}

/// Call once from the main thread.
pub fn install(ctx: &egui::Context) {
    let _ = CTX.set(ctx.clone());
    #[cfg(target_os = "macos")]
    mac::install();
}

/// Publish what's playing (`None`: nothing). `art`: thumbnail bytes (JPEG/WebP).
pub fn publish(info: Option<&Info>, art: Option<&[u8]>) {
    #[cfg(target_os = "macos")]
    mac::publish(info, art);
    #[cfg(not(target_os = "macos"))]
    let _ = (info, art);
}

#[cfg(target_os = "macos")]
mod mac {
    use super::{push, Cmd, Info};
    use block2::RcBlock;
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2::AnyThread;
    use objc2_app_kit::NSImage;
    use objc2_core_foundation::CGSize;
    use objc2_foundation::{NSArray, NSData, NSDictionary, NSNumber, NSString};
    use objc2_media_player::*;
    use std::ptr::NonNull;

    const SKIP: f64 = 10.0;

    pub fn install() {
        // SAFETY: MediaPlayer's documented API; handlers run on the main
        // thread and only queue a command.
        unsafe {
            let cc = MPRemoteCommandCenter::sharedCommandCenter();
            let simple = |cmd: Retained<MPRemoteCommand>, c: Cmd| {
                cmd.setEnabled(true);
                let h = RcBlock::new(move |_e: NonNull<MPRemoteCommandEvent>| {
                    push(c);
                    MPRemoteCommandHandlerStatus::Success
                });
                let _ = cmd.addTargetWithHandler(&h);
            };
            simple(cc.togglePlayPauseCommand(), Cmd::Toggle);
            simple(cc.playCommand(), Cmd::Play);
            simple(cc.pauseCommand(), Cmd::Pause);
            for (cmd, d) in [
                (cc.skipForwardCommand(), SKIP),
                (cc.skipBackwardCommand(), -SKIP),
            ] {
                cmd.setPreferredIntervals(&NSArray::from_retained_slice(&[NSNumber::new_f64(
                    SKIP,
                )]));
                simple(Retained::into_super(cmd), Cmd::Skip(d));
            }
            // next/previous track keys (F9/F7) also skip, there's no queue yet
            simple(cc.nextTrackCommand(), Cmd::Skip(SKIP));
            simple(cc.previousTrackCommand(), Cmd::Skip(-SKIP));
            let seek = cc.changePlaybackPositionCommand();
            seek.setEnabled(true);
            let h = RcBlock::new(|e: NonNull<MPRemoteCommandEvent>| {
                let e = e.cast::<MPChangePlaybackPositionCommandEvent>();
                push(Cmd::Seek(e.as_ref().positionTime()));
                MPRemoteCommandHandlerStatus::Success
            });
            let _ = seek.addTargetWithHandler(&h);
        }
    }

    pub fn publish(info: Option<&Info>, art: Option<&[u8]>) {
        // SAFETY: MediaPlayer/Foundation API with valid objects; the keys are
        // MediaPlayer's own constants.
        unsafe {
            let center = MPNowPlayingInfoCenter::defaultCenter();
            let Some(i) = info else {
                center.setNowPlayingInfo(None);
                center.setPlaybackState(MPNowPlayingPlaybackState::Stopped);
                return;
            };
            let title = NSString::from_str(&i.title);
            let artist = NSString::from_str(&i.artist);
            let dur = NSNumber::new_f64(i.duration);
            let pos = NSNumber::new_f64(i.pos);
            let rate = NSNumber::new_f64(if i.playing { i.rate } else { 0.0 });
            let artwork = art.and_then(artwork);
            let mut keys: Vec<&NSString> = vec![
                MPMediaItemPropertyTitle,
                MPMediaItemPropertyArtist,
                MPMediaItemPropertyPlaybackDuration,
                MPNowPlayingInfoPropertyElapsedPlaybackTime,
                MPNowPlayingInfoPropertyPlaybackRate,
            ];
            let mut vals: Vec<&AnyObject> = vec![
                title.as_ref(),
                artist.as_ref(),
                dur.as_ref(),
                pos.as_ref(),
                rate.as_ref(),
            ];
            if let Some(a) = &artwork {
                keys.push(MPMediaItemPropertyArtwork);
                vals.push(a.as_ref());
            }
            let dict = NSDictionary::from_slices(&keys, &vals);
            center.setNowPlayingInfo(Some(&dict));
            center.setPlaybackState(if i.playing {
                MPNowPlayingPlaybackState::Playing
            } else {
                MPNowPlayingPlaybackState::Paused
            });
        }
    }

    fn artwork(bytes: &[u8]) -> Option<Retained<MPMediaItemArtwork>> {
        // SAFETY: NSImage from in-memory data; the block hands back an image
        // the block itself keeps alive.
        unsafe {
            let img = NSImage::initWithData(NSImage::alloc(), &NSData::with_bytes(bytes))?;
            let size = img.size();
            let h = RcBlock::new(move |_s: CGSize| NonNull::from(&*img));
            Some(MPMediaItemArtwork::initWithBoundsSize_requestHandler(
                MPMediaItemArtwork::alloc(),
                CGSize::new(size.width, size.height),
                &h,
            ))
        }
    }
}
