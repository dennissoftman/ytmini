//! Embedded player: libmpv renders into an OpenGL texture that egui draws,
//! plus the player controls.
//!
//! Rendering happens inside `App::ui`, where eframe keeps the window's GL
//! context current. mpv draws into our FBO (whose colour attachment is a
//! texture registered with egui once for the app's lifetime), then we restore
//! the framebuffer binding and viewport so egui's own painting is unaffected.

use crate::mpv::{self, Mpv, Render};
use crate::yt::{self, Profile, Tools, Video};
use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, Sense, Stroke, Vec2};
use eframe::glow::{self, HasContext};
use std::{
    ffi::c_void,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

// ---------------------------------------------------------------------------
// Video surface (one per app)
// ---------------------------------------------------------------------------

/// A texture + framebuffer mpv renders into. Created once; the texture is
/// owned by egui after registration, so it's resized in place, never replaced.
pub struct Surface {
    tex: glow::Texture,
    fbo: glow::Framebuffer,
    pub id: egui::TextureId,
    size: [i32; 2],
}

impl Surface {
    pub fn new(gl: &glow::Context, frame: &mut eframe::Frame) -> Result<Surface, String> {
        // SAFETY: GL context is current on this (UI) thread.
        unsafe {
            let tex = gl.create_texture()?;
            let fbo = gl.create_framebuffer()?;
            let mut s = Surface {
                tex,
                fbo,
                id: egui::TextureId::default(),
                size: [0, 0],
            };
            s.resize(gl, 2, 2);
            let prev = gl.get_parameter_framebuffer(glow::FRAMEBUFFER_BINDING);
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
            gl.framebuffer_texture_2d(
                glow::FRAMEBUFFER,
                glow::COLOR_ATTACHMENT0,
                glow::TEXTURE_2D,
                Some(tex),
                0,
            );
            let status = gl.check_framebuffer_status(glow::FRAMEBUFFER);
            gl.bind_framebuffer(glow::FRAMEBUFFER, prev);
            if status != glow::FRAMEBUFFER_COMPLETE {
                gl.delete_framebuffer(fbo);
                gl.delete_texture(tex);
                return Err(format!("framebuffer incomplete (0x{status:x})"));
            }
            s.id = frame.register_native_glow_texture(tex);
            Ok(s)
        }
    }

    fn resize(&mut self, gl: &glow::Context, w: i32, h: i32) {
        if self.size == [w, h] {
            return;
        }
        self.size = [w, h];
        // SAFETY: GL context current; we restore the texture binding.
        unsafe {
            let prev = gl.get_parameter_texture(glow::TEXTURE_BINDING_2D);
            gl.bind_texture(glow::TEXTURE_2D, Some(self.tex));
            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                glow::RGBA8 as i32,
                w,
                h,
                0,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(None),
            );
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MIN_FILTER,
                glow::LINEAR as i32,
            );
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MAG_FILTER,
                glow::LINEAR as i32,
            );
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_WRAP_S,
                glow::CLAMP_TO_EDGE as i32,
            );
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_WRAP_T,
                glow::CLAMP_TO_EDGE as i32,
            );
            gl.bind_texture(glow::TEXTURE_2D, prev);
        }
    }

    /// Give the VRAM back when no video is showing.
    pub fn shrink(&mut self, gl: &glow::Context) {
        self.resize(gl, 2, 2);
    }

    pub fn destroy(&self, gl: &glow::Context) {
        // SAFETY: GL current; the texture itself belongs to egui now.
        unsafe { gl.delete_framebuffer(self.fbo) };
    }
}

// ---------------------------------------------------------------------------
// Player
// ---------------------------------------------------------------------------

/// Data the mpv callbacks touch (from mpv's threads). Boxed so it never moves.
struct Shared {
    ctx: egui::Context,
    frame_ready: AtomicBool,
}

unsafe extern "C" fn on_wakeup(data: *mut c_void) {
    // SAFETY: data is the Box<Shared> kept alive until mpv is destroyed.
    let s = unsafe { &*(data as *const Shared) };
    s.ctx.request_repaint();
}

unsafe extern "C" fn on_frame(data: *mut c_void) {
    // SAFETY: as above.
    let s = unsafe { &*(data as *const Shared) };
    s.frame_ready.store(true, Ordering::Release);
    s.ctx.request_repaint();
}

pub struct Embedded {
    // Drop order matters: render context before the core (see Drop).
    render: Option<Render>,
    mpv: Option<Mpv>,
    shared: Option<Box<Shared>>,

    pub title: String,
    pub id: String,
    url: String,
    /// e.g. "1440p · on battery"
    pub profile_label: String,
    /// The ytdl-format currently in use.
    pub format: String,
    /// Playing a download: no quality switching.
    pub local: bool,
    /// For YTMINI_DEBUG's "first frame after …".
    opened_at: Option<Instant>,

    pub pos: f64,
    pub duration: f64,
    pub paused: bool,
    pub volume: f64,
    /// `?` pressed: show the shortcuts overlay.
    pub show_help: bool,
    /// Volume before the speaker icon muted it.
    unmute_to: Option<f64>,
    /// Wheel travel spent pushing past the 100% notch.
    vol_hold: f32,
    pub speed: f64,
    pub buffering: bool,
    pub started: bool,
    pub eof: bool,
    pub cached_until: f64,
    pub error: Option<String>,
    last_log_error: Option<String>,
    /// Set when `title` was learned from mpv; the app updates the window title.
    pub title_changed: bool,
    /// Shown as "Resumed at 12:34 · Start over" for a few seconds.
    resumed_from: Option<(f64, Instant)>,
    title_from_mpv: bool,

    // UI state
    scrub: Option<f64>,
    last_scrub_seek: Instant,
    last_activity: Instant,
    /// Mouse press on the video, for click / double-click / hold-for-speed.
    press: Option<Press>,
    /// SponsorBlock segments to skip (filled in once fetched).
    pub segments: Vec<crate::sponsorblock::Segment>,
    /// Indices into `segments` already skipped (each is skipped once, so
    /// seeking back into one plays it).
    skipped: Vec<usize>,
    /// "Skipped sponsor · Undo" toast: segment index and when.
    skip_toast: Option<(usize, Instant)>,
    /// Seek-bar previews (filled in once fetched); sheets load on hover.
    pub storyboard: Option<crate::storyboard::Storyboard>,
    /// From the description; empty when it has none.
    pub chapters: Vec<crate::chapters::Chapter>,
    sb_sheets: std::collections::HashMap<u32, Option<egui::TextureHandle>>,
    sb_chan: (Sender<SheetMsg>, std::sync::mpsc::Receiver<SheetMsg>),
    /// A single click waiting out the double-click window before it pauses.
    pending_click: Option<(Instant, Pos2)>,
    /// Double-click-and-keep-tapping seek on the left/right side.
    streak: Option<Streak>,
}

/// A downloaded storyboard sheet: (sheet number, image).
type SheetMsg = (u32, egui::ColorImage);
use std::sync::mpsc::Sender;

struct Press {
    at: Instant,
    pos: Pos2,
    /// Started on the left or right third: holding speeds up.
    side: bool,
    /// Speed before the hold, while it's active.
    held_from: Option<f64>,
    /// Speed last sent to mpv during the hold.
    hold_speed: f64,
}

struct Streak {
    /// -1 back, +1 forward
    dir: f64,
    taps: u32,
    total: f64,
    last: Instant,
}

/// Seek per tap in a streak: 5 s for the first three, then 10 s for three, then 30 s.
fn streak_step(tap: u32) -> f64 {
    match tap {
        0..=3 => 5.0,
        4..=6 => 10.0,
        _ => 30.0,
    }
}

/// Holding the side of the video this long plays at `HOLD_SPEED` until release
/// (dragging sideways while holding scales it between `HOLD_MIN` and `HOLD_MAX`).
/// Volume slider: 0–150 % (past 100 is mpv's software boost), with a
/// detent at 100 % so it's easy to get back to normal.
pub const VOLUME_MAX: f64 = 150.0;
/// Speed menu and `<`/`>` steps.
pub const SPEEDS: [f64; 8] = [0.25, 0.5, 0.75, 1.0, 1.25, 1.5, 1.75, 2.0];

/// The `?` overlay.
pub const SHORTCUTS: [(&str, &str); 15] = [
    ("Space / K", "Play / pause"),
    ("J / L", "Back / forward 10 s"),
    ("← / →", "Back / forward 5 s"),
    ("↑ / ↓", "Volume"),
    ("M", "Mute"),
    ("F", "Fullscreen"),
    ("⇧N / ⇧P", "Next / previous video"),
    ("< / >", "Slower / faster"),
    (", / .", "Frame back / forward (paused)"),
    ("0 – 9", "Jump to 0 % – 90 %"),
    ("Home / End", "Start / end"),
    ("C", "Next chapter"),
    ("⇧C", "Previous chapter"),
    ("?", "These shortcuts"),
    ("Esc", "Exit fullscreen / back"),
];
/// Dragging within this many pixels of the 100% notch lands on it.
const VOLUME_SNAP_PX: f32 = 7.0;
/// Wheel travel (points) needed to push through the 100% notch.
const VOLUME_WHEEL_HOLD: f32 = 24.0;

const HOLD_AFTER: Duration = Duration::from_millis(350);
const HOLD_SPEED: f64 = 1.5;
/// While holding, dragging left eases down to this, right up to `HOLD_MAX`.
const HOLD_MIN: f64 = 0.25;
const HOLD_MAX: f64 = 2.0;
/// A click pauses only once it's clearly not the start of a double-click.
const DOUBLE_CLICK: Duration = Duration::from_millis(300);
/// Hysteresis: once a seek streak runs, the next tap may come this much later
/// (and anywhere on that half) and still count.
const STREAK_GAP: Duration = Duration::from_millis(700);

/// Volume leveling: ffmpeg's dynamic normalizer, gentle settings (half-second
/// frames, max 5× gain) so quiet talk comes up and loud intros come down
/// without pumping.
pub const LEVELING: &str = "lavfi=[dynaudnorm=f=500:g=31:p=0.9:m=5]";

const OBSERVED: &[(&str, std::ffi::c_int)] = &[
    ("time-pos", mpv::FORMAT_DOUBLE),
    ("duration", mpv::FORMAT_DOUBLE),
    ("pause", mpv::FORMAT_FLAG),
    ("volume", mpv::FORMAT_DOUBLE),
    ("speed", mpv::FORMAT_DOUBLE),
    ("paused-for-cache", mpv::FORMAT_FLAG),
    ("demuxer-cache-time", mpv::FORMAT_DOUBLE),
    ("eof-reached", mpv::FORMAT_FLAG),
    ("media-title", mpv::FORMAT_STRING),
];

impl Embedded {
    /// Start playback. Must be called with the GL context current (from `ui`).
    // A plain constructor; a params struct would only move the list elsewhere.
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        tools: &Tools,
        cookies: &Path,
        video: &Video,
        profile: &Profile,
        settings: &crate::settings::Settings,
        resume: Option<f64>,
        gpa: mpv::GetProcAddress,
        ctx: &egui::Context,
        // Pre-resolved stream URLs, or a download's files: skips the ytdl hook.
        direct: Option<yt::Direct>,
    ) -> Result<Embedded, String> {
        let mut opts: Vec<(&str, String)> = vec![
            ("vo", "libmpv".into()),
            // mpv defaults to software decoding; hardware decoding is the biggest battery win.
            ("hwdec", "auto-safe".into()),
            ("ytdl", "yes".into()),
            ("ytdl-format", profile.ytdl_format.clone()),
            (
                "script-opts",
                format!("ytdl_hook-ytdl_path={}", tools.ytdlp.display()),
            ),
            ("keep-open", "yes".into()),
            ("idle", "yes".into()),
            ("osc", "no".into()),
            ("input-default-bindings", "no".into()),
            // ytmini owns the media keys / Now Playing (nowplaying.rs)
            ("input-media-keys", "no".into()),
            ("input-vo-keyboard", "no".into()),
            ("terminal", "no".into()),
            ("audio-client-name", "ytmini".into()),
            (
                "volume",
                format!("{:.0}", settings.volume.clamp(0.0, VOLUME_MAX)),
            ),
            ("volume-max", format!("{VOLUME_MAX:.0}")),
        ];
        if profile.fast {
            opts.push(("profile", "fast".into()));
        }
        if settings.volume_leveling {
            opts.push(("af", LEVELING.into()));
        }
        if profile.audio_only {
            opts.push(("vid", "no".into()));
        }
        // What to load: the YouTube URL (resolved by mpv's ytdl hook), or
        // URLs/files we already have, which skips yt-dlp entirely.
        let target = match &direct {
            Some(d) => {
                opts.push(("ytdl", "no".into()));
                opts.push(("force-media-title", video.title().to_string()));
                if let Some(ua) = &d.user_agent {
                    opts.push(("user-agent", ua.clone()));
                }
                d.video.clone()
            }
            None => video.url(),
        };
        let mpv = Mpv::new(&opts)?;
        mpv.request_log_messages("error");
        for (i, (name, fmt)) in OBSERVED.iter().enumerate() {
            mpv.observe(i as u64 + 1, name, *fmt);
        }
        // change-list handles any characters in the path (spaces, commas).
        mpv.command(&[
            "change-list",
            "ytdl-raw-options",
            "append",
            &format!("cookies={}", cookies.display()),
        ])?;
        for opt in tools.ytdl_raw_options() {
            mpv.command(&["change-list", "ytdl-raw-options", "append", &opt])?;
        }
        // separate audio track for pre-resolved/downloaded video ("append"
        // adds one entry, so ':' in URLs is fine; only valid after init)
        if let Some(a) = direct.as_ref().and_then(|d| d.audio.as_ref()) {
            mpv.command(&["change-list", "audio-files", "append", a])?;
        }

        let render = Render::new(&mpv, gpa)?;
        let shared = Box::new(Shared {
            ctx: ctx.clone(),
            frame_ready: AtomicBool::new(false),
        });
        let data = (&raw const *shared) as *mut c_void;
        // SAFETY: `shared` outlives both the render context and the core (see Drop).
        unsafe {
            mpv.set_wakeup_callback(on_wakeup, data);
            render.set_update_callback(on_frame, data);
        }
        match resume {
            Some(t) => mpv.command(&[
                "loadfile",
                &target,
                "replace",
                "-1",
                &format!("start={t:.1}"),
            ])?,
            None => mpv.command(&["loadfile", &target])?,
        }

        Ok(Embedded {
            render: Some(render),
            mpv: Some(mpv),
            shared: Some(shared),
            title: video.title().to_string(),
            id: video.id.clone(),
            url: video.url(),
            profile_label: if direct.as_ref().is_some_and(|d| d.offline) {
                "offline".into()
            } else {
                profile.label.clone()
            },
            local: direct.as_ref().is_some_and(|d| d.offline),
            opened_at: Some(Instant::now()),
            format: profile.ytdl_format.clone(),
            pos: 0.0,
            duration: 0.0,
            paused: false,
            volume: settings.volume,
            show_help: false,
            unmute_to: None,
            vol_hold: 0.0,
            speed: 1.0,
            buffering: true,
            started: false,
            eof: false,
            cached_until: 0.0,
            error: None,
            last_log_error: None,
            title_changed: false,
            resumed_from: resume.map(|t| (t, Instant::now())),
            title_from_mpv: video.title.is_none(),
            scrub: None,
            last_scrub_seek: Instant::now(),
            last_activity: Instant::now(),
            press: None,
            segments: Vec::new(),
            skipped: Vec::new(),
            skip_toast: None,
            storyboard: None,
            chapters: Vec::new(),
            sb_sheets: std::collections::HashMap::new(),
            sb_chan: std::sync::mpsc::channel(),
            pending_click: None,
            streak: None,
        })
    }

    fn mpv(&self) -> &Mpv {
        self.mpv.as_ref().expect("mpv alive until drop")
    }

    /// Drain mpv events into our state. Call every frame.
    pub fn poll_events(&mut self) {
        let Some(mpv) = &self.mpv else { return };
        while let Some(ev) = mpv.poll_event() {
            match ev.event_id {
                mpv::EVENT_PROPERTY_CHANGE => {
                    // SAFETY: PROPERTY_CHANGE data is an mpv_event_property.
                    let p = unsafe { &*(ev.data as *const mpv::EventProperty) };
                    let (f, b) = (mpv::prop_f64(p), mpv::prop_flag(p));
                    match mpv::prop_name(p) {
                        "time-pos" => self.pos = f.unwrap_or(self.pos),
                        "duration" => self.duration = f.unwrap_or(0.0),
                        "pause" => self.paused = b.unwrap_or(false),
                        "volume" => self.volume = f.unwrap_or(self.volume),
                        "speed" => self.speed = f.unwrap_or(1.0),
                        "paused-for-cache" => self.buffering = b.unwrap_or(false),
                        "demuxer-cache-time" => self.cached_until = f.unwrap_or(0.0),
                        "eof-reached" => self.eof = b.unwrap_or(false),
                        "media-title" => {
                            // Fills in the title when we started from a bare id (`--play`).
                            let t = mpv::prop_string(p).unwrap_or_default();
                            if self.title_from_mpv
                                && !t.is_empty()
                                && !t.starts_with("watch?v=")
                                && t != self.title
                            {
                                self.title = t;
                                self.title_changed = true;
                            }
                        }
                        _ => {}
                    }
                }
                mpv::EVENT_START_FILE => {
                    self.started = false;
                    self.buffering = true;
                    self.error = None;
                }
                mpv::EVENT_PLAYBACK_RESTART => {
                    if let Some(t) = self.opened_at.take() {
                        if std::env::var_os("YTMINI_DEBUG").is_some() {
                            eprintln!("first frame after {:.2}s", t.elapsed().as_secs_f32());
                        }
                    }
                    self.started = true;
                    self.buffering = false;
                    self.last_activity = Instant::now(); // flash the controls when playback (re)starts
                }
                mpv::EVENT_END_FILE => {
                    // SAFETY: END_FILE data is an mpv_event_end_file.
                    let e = unsafe { &*(ev.data as *const mpv::EventEndFile) };
                    if e.reason == mpv::END_FILE_REASON_ERROR {
                        self.error = Some(
                            self.last_log_error
                                .clone()
                                .unwrap_or_else(|| "playback failed (mpv error)".into()),
                        );
                        self.buffering = false;
                    } else if e.reason == mpv::END_FILE_REASON_EOF {
                        self.eof = true;
                    }
                }
                mpv::EVENT_LOG_MESSAGE => {
                    // SAFETY: LOG_MESSAGE data is an mpv_event_log_message.
                    let m = unsafe { &*(ev.data as *const mpv::EventLogMessage) };
                    let text = mpv::c_to_string(m.text).trim().to_string();
                    if !text.is_empty() {
                        self.last_log_error =
                            Some(format!("{}: {text}", mpv::c_to_string(m.prefix)));
                    }
                }
                mpv::EVENT_SHUTDOWN => self.error = Some("mpv shut down".into()),
                _ => {}
            }
        }
        self.auto_skip();
    }

    fn auto_skip(&mut self) {
        if !self.started || self.scrub.is_some() {
            return;
        }
        let pos = self.pos;
        let hit = self
            .segments
            .iter()
            .enumerate()
            .find(|(i, s)| !self.skipped.contains(i) && pos >= s.start && pos < s.end - 0.5);
        if let Some((i, s)) = hit {
            let end = s.end;
            self.skipped.push(i);
            self.skip_toast = Some((i, Instant::now()));
            if self.duration > 0.0 && end >= self.duration - 1.0 {
                // segment runs to the end (outro): finish the video
                self.seek_absolute(self.duration, false);
            } else {
                self.seek_absolute(end, true);
            }
            self.pos = end;
        }
    }

    /// Render the current frame into `surface` at `w`×`h` pixels if mpv has a
    /// new frame or the size changed. GL context must be current.
    fn render(&mut self, gl: &glow::Context, surface: &mut Surface, w: i32, h: i32) {
        let (Some(render), Some(shared)) = (&self.render, &self.shared) else {
            return;
        };
        let resized = surface.size != [w, h];
        let new_frame = shared.frame_ready.swap(false, Ordering::Acquire)
            && render.update() & mpv::RENDER_UPDATE_FRAME != 0;
        if !(resized || new_frame) {
            return;
        }
        surface.resize(gl, w, h);
        // SAFETY: GL current; we save and restore what egui relies on.
        unsafe {
            let prev_fbo = gl.get_parameter_framebuffer(glow::FRAMEBUFFER_BINDING);
            let mut vp = [0i32; 4];
            gl.get_parameter_i32_slice(glow::VIEWPORT, &mut vp);
            if let Err(e) = render.render(surface.fbo.0.get(), w, h) {
                self.error.get_or_insert(e);
            }
            gl.bind_framebuffer(glow::FRAMEBUFFER, prev_fbo);
            gl.viewport(vp[0], vp[1], vp[2], vp[3]);
        }
    }

    /// Click: pause/resume (after the double-click window).
    /// Double-click the left/right third: seek ∓5 s; keep tapping for more
    /// (5 s ×3, then 10 s ×3, then 30 s). Double-click the middle: fullscreen.
    /// Hold the left/right third: 1.5×; drag sideways for 0.25×–2×.
    /// The control-bar strip never pauses: a near-miss on the seek bar or a
    /// button, or the click that reveals hidden controls, used to.
    fn gestures(
        &mut self,
        ctx: &egui::Context,
        resp: &egui::Response,
        area: Rect,
        painter: &egui::Painter,
        act: &mut Actions,
    ) {
        let third = area.width() / 3.0;
        let side_of = |p: Pos2| {
            if p.x < area.left() + third {
                -1.0
            } else if p.x > area.right() - third {
                1.0
            } else {
                0.0
            }
        };
        let zone = Rect::from_min_max(
            area.min,
            Pos2::new(area.right(), area.bottom() - CONTROLS_H - 8.0),
        );
        let (pressed, down, pos) = ctx.input(|i| {
            (
                i.pointer.primary_pressed(),
                i.pointer.primary_down(),
                i.pointer.interact_pos(),
            )
        });
        if pressed && self.error.is_none() && resp.hovered() && !egui::Popup::is_any_open(ctx) {
            if let Some(p) = pos.filter(|p| zone.contains(*p)) {
                self.press = Some(Press {
                    at: Instant::now(),
                    pos: p,
                    side: side_of(p) != 0.0,
                    held_from: None,
                    hold_speed: HOLD_SPEED,
                });
            }
        }
        if let Some(pr) = &mut self.press {
            let moved = pos.is_some_and(|p| p.distance(pr.pos) > 12.0);
            if down {
                if pr.side
                    && pr.held_from.is_none()
                    && !moved
                    && pr.at.elapsed() >= HOLD_AFTER
                    && !self.paused
                {
                    pr.held_from = Some(self.speed);
                    pr.hold_speed = HOLD_SPEED;
                    let _ = self.mpv().set_f64("speed", HOLD_SPEED);
                } else if pr.held_from.is_some() {
                    // Drag sideways from the press point: left → slower, right → faster.
                    let span = (area.width() * 0.25).clamp(120.0, 320.0) as f64;
                    let dx = pos.map_or(0.0, |p| (p.x - pr.pos.x) as f64);
                    let t = (dx / span).clamp(-1.0, 1.0);
                    let target = if t < 0.0 {
                        HOLD_SPEED + (HOLD_MIN - HOLD_SPEED) * -t
                    } else {
                        HOLD_SPEED + (HOLD_MAX - HOLD_SPEED) * t
                    };
                    let target = (target * 20.0).round() / 20.0; // 0.05 steps
                    if (target - pr.hold_speed).abs() > 1e-6 {
                        pr.hold_speed = target;
                        let speed = target;
                        let _ = self.mpv.as_ref().map(|m| m.set_f64("speed", speed));
                    }
                }
                ctx.request_repaint_after(Duration::from_millis(30));
            } else {
                let pr = self.press.take().expect("checked above");
                if let Some(s) = pr.held_from {
                    self.set_speed(s);
                } else if !moved && pr.at.elapsed() < HOLD_AFTER {
                    self.on_click(pr.pos, area, side_of(pr.pos), act);
                }
            }
        }
        if self
            .streak
            .as_ref()
            .is_some_and(|s| s.last.elapsed() >= STREAK_GAP + Duration::from_millis(500))
        {
            self.streak = None;
        }
        if let Some((t, _)) = self.pending_click {
            if t.elapsed() >= DOUBLE_CLICK {
                self.pending_click = None;
                self.toggle_pause();
                self.poke();
            } else {
                ctx.request_repaint_after(DOUBLE_CLICK - t.elapsed());
            }
        }
        // "« 15 s" bubble on the side being tapped
        if let Some(st) = &self.streak {
            let x = if st.dir < 0.0 {
                area.left() + third / 2.0
            } else {
                area.right() - third / 2.0
            };
            let r = Rect::from_center_size(Pos2::new(x, area.center().y), Vec2::new(110.0, 44.0));
            painter.rect_filled(r, 22, Color32::from_black_alpha(150));
            let label = if st.dir < 0.0 {
                format!("◂◂  {} s", st.total as u64)
            } else {
                format!("{} s  ▸▸", st.total as u64)
            };
            painter.text(
                r.center(),
                egui::Align2::CENTER_CENTER,
                label,
                egui::FontId::proportional(17.0),
                Color32::WHITE,
            );
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        // "1.5×" pill while holding
        if let Some(speed) = self
            .press
            .as_ref()
            .filter(|p| p.held_from.is_some())
            .map(|p| p.hold_speed)
        {
            let r = Rect::from_center_size(
                Pos2::new(area.center().x, area.top() + 34.0),
                Vec2::new(84.0, 32.0),
            );
            painter.rect_filled(r, 16, Color32::from_black_alpha(170));
            painter.text(
                r.center(),
                egui::Align2::CENTER_CENTER,
                format!(
                    "{}×  {}",
                    trim_num(speed),
                    if speed < 1.0 { "◂◂" } else { "▸▸" }
                ),
                egui::FontId::proportional(15.0),
                Color32::WHITE,
            );
        }
    }

    /// Storyboard frame for `t`, drawn above the seek bar centred on `x`,
    /// bottom edge at `bottom`. Sheets download (no cookies) on first need.
    fn preview(&mut self, ui: &egui::Ui, x: f32, bottom: f32, t: f64) {
        while let Ok((n, img)) = self.sb_chan.1.try_recv() {
            let tex = ui
                .ctx()
                .load_texture(format!("sb-{}-{n}", self.id), img, Default::default());
            self.sb_sheets.insert(n, Some(tex));
        }
        let Some(sb) = &self.storyboard else { return };
        let (sheet, i) = sb.frame_at(t, self.duration);
        let tex = match self.sb_sheets.get(&sheet) {
            Some(Some(tex)) => tex.clone(),
            Some(None) => {
                ui.ctx().request_repaint_after(Duration::from_millis(50));
                return;
            }
            None => {
                self.sb_sheets.insert(sheet, None);
                let (url, tx, ctx) = (
                    sb.sheet_url(sheet),
                    self.sb_chan.0.clone(),
                    ui.ctx().clone(),
                );
                std::thread::spawn(move || {
                    if let Ok(img) = crate::storyboard::load_sheet(&url) {
                        let _ = tx.send((sheet, img));
                        ctx.request_repaint();
                    }
                });
                return;
            }
        };
        let [tw, th] = tex.size().map(|v| v as f32);
        let (col, row) = ((i % sb.cols) as f32, (i / sb.cols) as f32);
        let (fw, fh) = (sb.w as f32, sb.h as f32);
        if (col + 1.0) * fw > tw + 1.0 || (row + 1.0) * fh > th + 1.0 {
            return; // last sheet is shorter than a full grid
        }
        let uv = Rect::from_min_max(
            Pos2::new(col * fw / tw, row * fh / th),
            Pos2::new((col + 1.0) * fw / tw, (row + 1.0) * fh / th),
        );
        let w = 192.0;
        let r = Rect::from_min_max(
            Pos2::new(x - w / 2.0, bottom - w * fh / fw),
            Pos2::new(x + w / 2.0, bottom),
        );
        let p = ui.painter();
        p.rect_filled(r.expand(2.0), 6, Color32::from_black_alpha(220));
        p.image(tex.id(), r, uv, Color32::WHITE);
    }

    fn on_click(&mut self, pos: Pos2, area: Rect, side: f64, act: &mut Actions) {
        // Continuing a streak: any tap on that half within the (longer) gap.
        if let Some(st) = &mut self.streak {
            if st.last.elapsed() < STREAK_GAP {
                let half = if pos.x < area.center().x { -1.0 } else { 1.0 };
                if half == st.dir {
                    st.taps += 1;
                    st.last = Instant::now();
                    let step = streak_step(st.taps);
                    st.total += step;
                    let d = st.dir * step;
                    self.seek_relative(d);
                } else {
                    st.last = st.last.checked_sub(STREAK_GAP).unwrap_or(st.last);
                    // tap on the other half just ends it
                }
                self.pending_click = None;
                return;
            }
        }
        match self.pending_click.take() {
            // second click close to the first: a double-click
            Some((_, first)) if first.distance(pos) < 60.0 => {
                if side == 0.0 {
                    act.toggle_fullscreen = true;
                } else {
                    let step = streak_step(1);
                    self.streak = Some(Streak {
                        dir: side,
                        taps: 1,
                        total: step,
                        last: Instant::now(),
                    });
                    self.seek_relative(side * step);
                }
            }
            _ => self.pending_click = Some((Instant::now(), pos)),
        }
    }

    // ---------- commands ----------

    pub fn toggle_pause(&mut self) {
        if self.eof {
            self.replay();
            return;
        }
        let _ = self.mpv().set_flag("pause", !self.paused);
    }

    pub fn replay(&mut self) {
        let _ = self.mpv().command(&["seek", "0", "absolute"]);
        let _ = self.mpv().set_flag("pause", false);
        self.eof = false;
    }

    pub fn seek_relative(&mut self, secs: f64) {
        let _ = self
            .mpv()
            .command(&["seek", &format!("{secs}"), "relative+exact"]);
        self.poke();
    }

    pub fn seek_absolute(&self, t: f64, exact: bool) {
        let flags = if exact {
            "absolute+exact"
        } else {
            "absolute+keyframes"
        };
        let _ = self.mpv().command(&["seek", &format!("{t:.2}"), flags]);
    }

    pub fn set_volume(&mut self, v: f64) {
        self.volume = v.clamp(0.0, VOLUME_MAX);
        let _ = self.mpv().set_f64("volume", self.volume);
    }

    pub fn set_leveling(&mut self, on: bool) {
        let _ = self.mpv().set_str("af", if on { LEVELING } else { "" });
    }

    /// C / Shift+C: next chapter, or back to the previous one.
    pub fn jump_chapter(&mut self, forward: bool) {
        let t = self.pos;
        let target = if forward {
            self.chapters
                .iter()
                .find(|c| c.start > t + 0.5)
                .map(|c| c.start)
        } else {
            // like a CD player: >3 s into a chapter goes to its start
            let cur = crate::chapters::at(&self.chapters, t);
            match cur {
                Some(i) if t - self.chapters[i].start > 3.0 => Some(self.chapters[i].start),
                Some(i) if i > 0 => Some(self.chapters[i - 1].start),
                _ => None,
            }
        };
        if let Some(x) = target {
            self.seek_absolute(x, true);
        }
    }

    /// Speaker icon / M key.
    pub fn toggle_mute(&mut self) {
        if self.volume > 0.0 {
            self.unmute_to = Some(self.volume);
            self.set_volume(0.0);
        } else {
            let v = self.unmute_to.take().unwrap_or(100.0);
            self.set_volume(v);
        }
    }

    /// `,` / `.` while paused: one frame back / forward.
    pub fn frame_step(&mut self, forward: bool) {
        let _ = self.mpv().command(&[if forward {
            "frame-step"
        } else {
            "frame-back-step"
        }]);
    }

    /// `<` / `>`: the next slower / faster standard speed.
    pub fn step_speed(&mut self, faster: bool) {
        let cur = self.speed;
        let next = if faster {
            SPEEDS.iter().copied().find(|&x| x > cur + 1e-6)
        } else {
            SPEEDS.iter().rev().copied().find(|&x| x < cur - 1e-6)
        };
        if let Some(x) = next {
            self.set_speed(x);
        }
    }

    pub fn set_speed(&mut self, s: f64) {
        let _ = self.mpv().set_f64("speed", s);
    }

    /// Switch format in place: keep the position, reload with the new format.
    pub fn apply_profile(&mut self, profile: &Profile) {
        if self.local || self.format == profile.ytdl_format {
            return;
        }
        self.format = profile.ytdl_format.clone();
        self.profile_label = profile.label.clone();
        let mpv = self.mpv();
        let _ = mpv.set_str("ytdl-format", &profile.ytdl_format);
        let _ = mpv.set_str("vid", if profile.audio_only { "no" } else { "auto" });
        // if we started from pre-resolved URLs, go back through yt-dlp
        let _ = mpv.set_str("ytdl", "yes");
        let _ = mpv.command(&["change-list", "audio-files", "clr", ""]);
        let pos = mpv.get_f64("time-pos").unwrap_or(self.pos);
        let _ = mpv.command(&[
            "loadfile",
            &self.url,
            "replace",
            "-1",
            &format!("start={pos:.1}"),
        ]);
    }

    /// Register user activity (shows the controls for a while).
    /// While the window is hidden (tray) play audio only: no video decoding
    /// for frames nobody sees. Restores video when shown again.
    pub fn set_background(&mut self, background: bool) {
        let audio_only = self.format == "ba/b";
        let vid = if background || audio_only {
            "no"
        } else {
            "auto"
        };
        let _ = self.mpv().set_str("vid", vid);
    }

    /// Current video height (for the smoke test).
    pub fn height(&self) -> Option<f64> {
        self.mpv.as_ref().and_then(|m| m.get_f64("height"))
    }

    pub fn poke(&mut self) {
        self.last_activity = Instant::now();
    }

    /// Shut down synchronously (used when the app quits).
    pub fn close_now(mut self) {
        self.render.take();
        self.mpv.take(); // terminate_destroy blocks until mpv is done
        self.shared.take();
    }
}

impl Drop for Embedded {
    fn drop(&mut self) {
        // The render context must go first, with the GL context current —
        // we're always dropped on the UI thread in a single-window app.
        self.render.take();
        if let Some(mpv) = self.mpv.take() {
            // `shared` must outlive the core: mpv may call on_wakeup until it's gone.
            mpv.destroy_in_background(self.shared.take());
        }
    }
}

// ---------------------------------------------------------------------------
// Player UI
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct Actions {
    pub back: bool,
    pub toggle_fullscreen: bool,
    pub quality: Option<yt::Quality>,
    pub open_in_window: bool,
    pub volume_changed: bool,
    /// Queue navigation (buttons, end-of-video card).
    pub next: bool,
    pub prev: bool,
    pub cancel_autoplay: bool,
}

pub struct ViewCtx<'a> {
    pub gl: &'a glow::Context,
    pub surface: &'a mut Surface,
    pub fullscreen: bool,
    pub quality: yt::Quality,
    pub screen_cap: u32,
    pub accent: Color32,
    /// Titles of the previous / next video in the queue, if any.
    pub prev: Option<String>,
    pub next: Option<String>,
    /// Autoplay countdown (seconds left) shown on the end card.
    pub autoplay_in: Option<f32>,
}

const CONTROLS_H: f32 = 64.0;
const HIDE_AFTER: Duration = Duration::from_millis(2500);

impl Embedded {
    pub fn ui(&mut self, ui: &mut egui::Ui, v: ViewCtx<'_>) -> Actions {
        let mut act = Actions::default();
        let ctx = ui.ctx().clone();

        // Header (hidden in fullscreen)
        if !v.fullscreen {
            ui.horizontal(|ui| {
                ui.add_space(12.0);
                if ui
                    .add(egui::Button::new(egui::RichText::new("←  Back").size(15.0)).frame(false))
                    .clicked()
                {
                    act.back = true;
                }
                ui.add_space(8.0);
                ui.add(
                    egui::Label::new(egui::RichText::new(&self.title).size(16.0).strong())
                        .truncate(),
                );
            });
            ui.add_space(6.0);
        }

        let area = ui.available_rect_before_wrap();
        let resp = ui.allocate_rect(area, Sense::click());
        let painter = ui.painter_at(area);
        painter.rect_filled(area, 0, Color32::BLACK);

        // Activity → controls visible
        if ctx.input(|i| i.pointer.delta() != Vec2::ZERO || i.pointer.any_down()) {
            self.poke();
        }
        let idle = self.last_activity.elapsed();
        let show_controls =
            self.paused || self.eof || !self.started || idle < HIDE_AFTER || self.scrub.is_some();
        if !show_controls && resp.hovered() {
            ctx.set_cursor_icon(egui::CursorIcon::None);
        } else if !self.paused {
            ctx.request_repaint_after(HIDE_AFTER.saturating_sub(idle) + Duration::from_millis(50));
        }

        // Video
        let ppp = ctx.pixels_per_point();
        let (w, h) = (
            (area.width() * ppp).round() as i32,
            (area.height() * ppp).round() as i32,
        );
        if w > 0 && h > 0 {
            self.render(v.gl, v.surface, w, h);
            painter.image(
                v.surface.id,
                area,
                Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                Color32::WHITE,
            );
        }

        self.gestures(&ctx, &resp, area, &painter, &mut act);

        // Overlays: loading, error, end
        let center = area.center();
        if let Some(err) = self.error.clone() {
            let r = Rect::from_center_size(center, Vec2::new(area.width().min(560.0), 150.0));
            ui.scope_builder(egui::UiBuilder::new().max_rect(r), |ui| {
                egui::Frame::new()
                    .fill(Color32::from_black_alpha(220))
                    .corner_radius(12)
                    .inner_margin(14)
                    .show(ui, |ui| {
                        ui.label(
                            egui::RichText::new("Couldn't play this video")
                                .strong()
                                .color(Color32::WHITE),
                        );
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(err)
                                    .size(12.5)
                                    .color(Color32::LIGHT_GRAY),
                            )
                            .wrap(),
                        );
                        ui.horizontal(|ui| {
                            if ui.button("Try in separate window").clicked() {
                                act.open_in_window = true;
                            }
                            if ui.button("Back").clicked() {
                                act.back = true;
                            }
                        });
                    });
            });
        } else if !self.started || self.buffering {
            painter.circle_filled(center, 28.0, Color32::from_black_alpha(140));
            ui.scope_builder(
                egui::UiBuilder::new().max_rect(Rect::from_center_size(center, Vec2::splat(32.0))),
                |ui| {
                    ui.add(egui::Spinner::new().size(28.0).color(Color32::WHITE));
                },
            );
            ctx.request_repaint_after(Duration::from_millis(33));
        } else if self.eof {
            match (&v.next, v.autoplay_in) {
                (Some(next), Some(left)) => {
                    let r =
                        Rect::from_center_size(center, Vec2::new(area.width().min(420.0), 130.0));
                    ui.scope_builder(egui::UiBuilder::new().max_rect(r), |ui| {
                        egui::Frame::new()
                            .fill(Color32::from_black_alpha(215))
                            .corner_radius(12)
                            .inner_margin(16)
                            .show(ui, |ui| {
                                ui.set_width(r.width() - 32.0);
                                ui.label(
                                    egui::RichText::new(format!(
                                        "Up next in {}",
                                        left.ceil() as u32
                                    ))
                                    .size(13.0)
                                    .color(Color32::LIGHT_GRAY),
                                );
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(next)
                                            .size(16.0)
                                            .strong()
                                            .color(Color32::WHITE),
                                    )
                                    .truncate(),
                                );
                                ui.add_space(8.0);
                                ui.horizontal(|ui| {
                                    if ui
                                        .button(egui::RichText::new("▶  Play now").size(14.0))
                                        .clicked()
                                    {
                                        act.next = true;
                                    }
                                    if ui
                                        .button(egui::RichText::new("Cancel").size(14.0))
                                        .clicked()
                                    {
                                        act.cancel_autoplay = true;
                                    }
                                });
                            });
                    });
                    ctx.request_repaint_after(Duration::from_millis(200));
                }
                _ => {
                    let w = if v.next.is_some() { 260.0 } else { 120.0 };
                    let r = Rect::from_center_size(center, Vec2::new(w, 40.0));
                    ui.scope_builder(egui::UiBuilder::new().max_rect(r), |ui| {
                        ui.horizontal_centered(|ui| {
                            if ui
                                .button(egui::RichText::new("↻  Replay").size(16.0))
                                .clicked()
                            {
                                self.replay();
                            }
                            if v.next.is_some()
                                && ui
                                    .button(egui::RichText::new("Next  ⏭").size(16.0))
                                    .clicked()
                            {
                                act.next = true;
                            }
                        });
                    });
                }
            }
        }
        if self.show_help {
            let r = Rect::from_center_size(center, Vec2::new(360.0, 400.0));
            ui.scope_builder(egui::UiBuilder::new().max_rect(r), |ui| {
                egui::Frame::new()
                    .fill(Color32::from_black_alpha(225))
                    .corner_radius(12)
                    .inner_margin(16)
                    .show(ui, |ui| {
                        ui.label(
                            egui::RichText::new("Keyboard shortcuts")
                                .size(16.0)
                                .strong()
                                .color(Color32::WHITE),
                        );
                        ui.add_space(6.0);
                        egui::Grid::new("keys").spacing([18.0, 5.0]).show(ui, |ui| {
                            for (k, what) in SHORTCUTS {
                                ui.label(egui::RichText::new(k).monospace().color(Color32::WHITE));
                                ui.label(egui::RichText::new(what).color(Color32::LIGHT_GRAY));
                                ui.end_row();
                            }
                        });
                    });
            });
        }
        // "Resumed at …" pill, top-left, for the first seconds of a resumed video
        if let Some((t, _)) = self
            .resumed_from
            .filter(|(_, since)| since.elapsed() < Duration::from_secs(8))
        {
            let r = Rect::from_min_size(
                area.left_top() + Vec2::new(16.0, 16.0),
                Vec2::new(250.0, 34.0),
            );
            ui.scope_builder(egui::UiBuilder::new().max_rect(r), |ui| {
                egui::Frame::new()
                    .fill(Color32::from_black_alpha(190))
                    .corner_radius(8)
                    .inner_margin(egui::Margin::symmetric(10, 5))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new(format!("Resumed at {}", fmt_time(t)))
                                    .color(Color32::WHITE),
                            );
                            if ui.link("Start over").clicked() {
                                self.seek_absolute(0.0, true);
                                self.resumed_from = None;
                            }
                        });
                    });
            });
            ctx.request_repaint_after(Duration::from_millis(500));
        }

        // "Skipped sponsor · Undo", bottom-left above the controls
        if let Some((i, since)) = self
            .skip_toast
            .filter(|(_, since)| since.elapsed() < Duration::from_secs(5))
        {
            let _ = since;
            let r = Rect::from_min_size(
                Pos2::new(area.left() + 16.0, area.bottom() - CONTROLS_H - 52.0),
                Vec2::new(320.0, 34.0),
            );
            let what = self
                .segments
                .get(i)
                .map_or("segment", |s| crate::sponsorblock::label(&s.category))
                .to_lowercase();
            ui.scope_builder(egui::UiBuilder::new().max_rect(r), |ui| {
                egui::Frame::new()
                    .fill(Color32::from_black_alpha(190))
                    .corner_radius(8)
                    .inner_margin(egui::Margin::symmetric(10, 5))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new(format!("Skipped {what}"))
                                    .color(Color32::WHITE),
                            );
                            if ui.link("Undo").clicked() {
                                if let Some(s) = self.segments.get(i) {
                                    self.seek_absolute(s.start, true);
                                }
                                self.skip_toast = None;
                            }
                        });
                    });
            });
            ctx.request_repaint_after(Duration::from_millis(500));
        } else {
            self.skip_toast = None;
        }

        // Controls
        if show_controls && self.error.is_none() {
            let bar = Rect::from_min_max(
                Pos2::new(area.left(), area.bottom() - CONTROLS_H),
                area.right_bottom(),
            );
            // soft gradient so controls read over bright video
            let top = bar.top() - 40.0;
            let mut mesh = egui::Mesh::default();
            let (clear, dark) = (Color32::TRANSPARENT, Color32::from_black_alpha(190));
            mesh.colored_vertex(Pos2::new(bar.left(), top), clear);
            mesh.colored_vertex(Pos2::new(bar.right(), top), clear);
            mesh.colored_vertex(bar.right_bottom(), dark);
            mesh.colored_vertex(bar.left_bottom(), dark);
            mesh.add_triangle(0, 1, 2);
            mesh.add_triangle(0, 2, 3);
            painter.add(egui::Shape::mesh(mesh));
            self.controls(ui, bar, &v, &mut act);
        }
        act
    }

    fn controls(&mut self, ui: &mut egui::Ui, bar: Rect, v: &ViewCtx<'_>, act: &mut Actions) {
        let painter = ui.painter_at(bar);
        let pad = 14.0;

        // --- seek bar ---
        let seek = Rect::from_min_max(
            Pos2::new(bar.left() + pad, bar.top() + 2.0),
            Pos2::new(bar.right() - pad, bar.top() + 26.0),
        );
        let sresp = ui.interact(seek, ui.scope_id().with("seek"), Sense::click_and_drag());
        let dur = self.duration.max(0.001);
        let x_to_t = |x: f32| (((x - seek.left()) / seek.width()).clamp(0.0, 1.0) as f64) * dur;
        if let Some(p) = sresp.interact_pointer_pos() {
            let t = x_to_t(p.x);
            if sresp.dragged() {
                self.scrub = Some(t);
                // live scrubbing: cheap keyframe seeks, throttled
                if self.last_scrub_seek.elapsed() > Duration::from_millis(120) {
                    self.seek_absolute(t, false);
                    self.last_scrub_seek = Instant::now();
                }
            }
            if sresp.clicked() || sresp.drag_stopped() {
                self.seek_absolute(t, true);
                self.scrub = None;
                self.pos = t;
            }
            self.poke();
        }
        if !sresp.is_pointer_button_down_on() && !sresp.dragged() {
            self.scrub = None;
        }
        let shown = self.scrub.unwrap_or(self.pos);
        let hover = sresp.hovered() || self.scrub.is_some();
        let track_h = if hover { 6.0 } else { 4.0 };
        let track = Rect::from_center_size(seek.center(), Vec2::new(seek.width(), track_h));
        let frac = |t: f64| (t / dur).clamp(0.0, 1.0) as f32;
        painter.rect_filled(track, 3, Color32::from_white_alpha(50));
        let cached = Rect::from_min_max(
            track.min,
            Pos2::new(
                track.left() + track.width() * frac(self.cached_until),
                track.bottom(),
            ),
        );
        painter.rect_filled(cached, 3, Color32::from_white_alpha(90));
        let played = Rect::from_min_max(
            track.min,
            Pos2::new(track.left() + track.width() * frac(shown), track.bottom()),
        );
        painter.rect_filled(played, 3, v.accent);
        for s in &self.segments {
            let [r, g, b] = crate::sponsorblock::color(&s.category);
            let seg = Rect::from_min_max(
                Pos2::new(track.left() + track.width() * frac(s.start), track.top()),
                Pos2::new(
                    (track.left() + track.width() * frac(s.end))
                        .max(track.left() + track.width() * frac(s.start) + 2.0),
                    track.bottom(),
                ),
            );
            painter.rect_filled(seg, 0, Color32::from_rgba_unmultiplied(r, g, b, 200));
        }
        // chapter boundaries: small gaps in the track, like YouTube's
        for c in self.chapters.iter().skip(1) {
            let x = track.left() + track.width() * frac(c.start);
            painter.rect_filled(
                Rect::from_center_size(
                    Pos2::new(x, track.center().y),
                    Vec2::new(2.0, track_h + 1.0),
                ),
                0,
                Color32::BLACK,
            );
        }
        if hover {
            painter.circle_filled(Pos2::new(played.right(), track.center().y), 7.0, v.accent);
            if let Some(hp) = sresp.hover_pos() {
                let t = x_to_t(hp.x);
                self.preview(
                    ui,
                    hp.x.clamp(seek.left() + 100.0, seek.right() - 100.0),
                    seek.top() - 30.0,
                    t,
                );
                let label = match crate::chapters::at(&self.chapters, t) {
                    Some(i) => format!("{}  ·  {}", fmt_time(t), self.chapters[i].title),
                    None => fmt_time(t),
                };
                let g = painter.layout_no_wrap(label, FontId::proportional(12.0), Color32::WHITE);
                let r = Rect::from_center_size(
                    Pos2::new(
                        hp.x.clamp(seek.left() + 24.0, seek.right() - 24.0),
                        seek.top() - 14.0,
                    ),
                    g.size() + Vec2::new(10.0, 4.0),
                );
                ui.painter()
                    .rect_filled(r, 4, Color32::from_black_alpha(200));
                ui.painter()
                    .galley(r.min + Vec2::new(5.0, 2.0), g, Color32::WHITE);
            }
        }

        // --- buttons row ---
        let row = Rect::from_min_max(
            Pos2::new(bar.left() + pad, seek.bottom() + 2.0),
            Pos2::new(bar.right() - pad, bar.bottom() - 4.0),
        );
        let btn = |ui: &mut egui::Ui, rect: Rect, id: &str| {
            let r = ui.interact(rect, ui.scope_id().with(id), Sense::click());
            if r.hovered() {
                ui.painter()
                    .rect_filled(rect, 6, Color32::from_white_alpha(25));
            }
            r.on_hover_cursor(egui::CursorIcon::PointingHand)
        };

        // prev / play-pause / next
        let sq = Vec2::splat(row.height());
        let prev_r = Rect::from_min_size(row.left_top(), sq);
        let pp = Rect::from_min_size(row.left_top() + Vec2::new(row.height(), 0.0), sq);
        let next_r = Rect::from_min_size(row.left_top() + Vec2::new(row.height() * 2.0, 0.0), sq);
        for (r, title, fwd) in [(prev_r, &v.prev, false), (next_r, &v.next, true)] {
            let col = if title.is_some() {
                Color32::WHITE
            } else {
                Color32::from_white_alpha(70)
            };
            let resp = ui.interact(r, ui.scope_id().with(("q", fwd)), Sense::click());
            if let Some(t) = title {
                if resp.hovered() {
                    ui.painter()
                        .rect_filled(r, 6, Color32::from_white_alpha(25));
                }
                let resp = resp
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .on_hover_text(format!(
                        "{}: {t}  ({})",
                        if fwd { "Next" } else { "Previous" },
                        if fwd { "⇧N" } else { "⇧P" }
                    ));
                if resp.clicked() {
                    if fwd {
                        act.next = true;
                    } else {
                        act.prev = true;
                    }
                }
            }
            skip_icon(&painter, r.center(), fwd, col);
        }
        if btn(ui, pp, "pp").clicked() {
            self.toggle_pause();
        }
        let c = pp.center();
        if self.paused || self.eof {
            let s = 9.0;
            painter.add(egui::Shape::convex_polygon(
                vec![
                    Pos2::new(c.x - s * 0.7, c.y - s),
                    Pos2::new(c.x - s * 0.7, c.y + s),
                    Pos2::new(c.x + s, c.y),
                ],
                Color32::WHITE,
                Stroke::NONE,
            ));
        } else {
            for dx in [-4.5, 4.5] {
                painter.rect_filled(
                    Rect::from_center_size(Pos2::new(c.x + dx, c.y), Vec2::new(4.0, 16.0)),
                    1,
                    Color32::WHITE,
                );
            }
        }

        // time
        let time = if self.duration > 0.0 {
            format!("{} / {}", fmt_time(shown), fmt_time(self.duration))
        } else {
            format!("{}  · LIVE", fmt_time(shown))
        };
        let time_r = painter.text(
            Pos2::new(next_r.right() + 8.0, c.y),
            Align2::LEFT_CENTER,
            time,
            FontId::proportional(14.0),
            Color32::WHITE,
        );
        // current chapter; click → next chapter
        if let Some(i) = crate::chapters::at(&self.chapters, shown) {
            let t = &self.chapters[i].title;
            let max = ((row.width() * 0.3) / 7.5).max(12.0) as usize;
            let t: String = if t.chars().count() > max {
                t.chars().take(max - 1).chain(['…']).collect()
            } else {
                t.clone()
            };
            let g = painter.layout_no_wrap(
                format!("·  {t}  ›"),
                FontId::proportional(13.5),
                Color32::from_white_alpha(210),
            );
            let r = Rect::from_min_size(
                Pos2::new(time_r.right() + 10.0, c.y - g.size().y / 2.0),
                g.size(),
            );
            let resp = ui
                .interact(r.expand(3.0), ui.scope_id().with("chapter"), Sense::click())
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .on_hover_text("Next chapter (C)");
            if resp.hovered() {
                painter.rect_filled(r.expand(4.0), 6, Color32::from_white_alpha(25));
            }
            painter.galley(r.min, g, Color32::WHITE);
            if resp.clicked() {
                self.jump_chapter(true);
            }
        }

        // right side, laid out right-to-left
        let mut right = row.right();
        let mut take = |w: f32| {
            let r = Rect::from_min_max(
                Pos2::new(right - w, row.top()),
                Pos2::new(right, row.bottom()),
            );
            right -= w + 8.0;
            r
        };

        // fullscreen
        let fs = take(row.height());
        if btn(ui, fs, "fs").clicked() {
            act.toggle_fullscreen = true;
        }
        let (fc, k, l) = (fs.center(), 7.0, 5.0);
        let st = Stroke::new(2.0, Color32::WHITE);
        let inward = if v.fullscreen { -1.0 } else { 1.0 };
        for (sx, sy) in [(-1.0, -1.0), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)] {
            let corner = Pos2::new(fc.x + sx * k, fc.y + sy * k);
            painter.line_segment(
                [corner, Pos2::new(corner.x - sx * l * inward, corner.y)],
                st,
            );
            painter.line_segment(
                [corner, Pos2::new(corner.x, corner.y - sy * l * inward)],
                st,
            );
        }

        // quality
        let qr = take(150.0);
        ui.scope_builder(
            egui::UiBuilder::new()
                .max_rect(qr)
                .layout(egui::Layout::right_to_left(egui::Align::Center)),
            |ui| {
                let mut q = v.quality;
                egui::ComboBox::from_id_salt("quality")
                    .selected_text(egui::RichText::new(&self.profile_label).size(13.0))
                    .width(140.0)
                    .show_ui(ui, |ui| {
                        for choice in yt::Quality::choices(v.screen_cap) {
                            ui.selectable_value(&mut q, choice, choice.label());
                        }
                    });
                if q != v.quality {
                    act.quality = Some(q);
                }
            },
        );

        // speed
        let sr = take(70.0);
        ui.scope_builder(
            egui::UiBuilder::new()
                .max_rect(sr)
                .layout(egui::Layout::right_to_left(egui::Align::Center)),
            |ui| {
                let mut s = self.speed;
                egui::ComboBox::from_id_salt("speed")
                    .selected_text(egui::RichText::new(format!("{}×", trim_num(s))).size(13.0))
                    .width(60.0)
                    .show_ui(ui, |ui| {
                        for x in SPEEDS {
                            ui.selectable_value(&mut s, x, format!("{}×", trim_num(x)));
                        }
                    });
                if (s - self.speed).abs() > f64::EPSILON {
                    self.set_speed(s);
                }
            },
        );

        // volume
        let vr = take(150.0);
        if let Some(vol) = self.volume_control(ui, vr, v.accent) {
            self.set_volume(vol);
            act.volume_changed = true;
        }
    }

    /// Speaker icon (click: mute) + slider with a sticky notch at 100%;
    /// the wheel anywhere over it nudges the volume. Returns a new volume.
    fn volume_control(&mut self, ui: &mut egui::Ui, area: Rect, accent: Color32) -> Option<f64> {
        let painter = ui.painter_at(area.expand(8.0));
        let cy = area.center().y;
        let icon = Rect::from_center_size(Pos2::new(area.left() + 13.0, cy), Vec2::splat(26.0));
        let rail = Rect::from_min_max(
            Pos2::new(icon.right() + 8.0, cy - 10.0),
            Pos2::new(area.right() - 40.0, cy + 10.0),
        );
        let x_of = |v: f64| rail.left() + rail.width() * (v / VOLUME_MAX).clamp(0.0, 1.0) as f32;
        let notch_x = x_of(100.0);
        let mut vol = self.volume;

        // icon
        let ir = ui
            .interact(icon, ui.scope_id().with("vol-icon"), Sense::click())
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .on_hover_text(if vol > 0.0 { "Mute" } else { "Unmute" });
        if ir.hovered() {
            painter.rect_filled(icon, 6, Color32::from_white_alpha(25));
        }
        if ir.clicked() {
            if vol > 0.0 {
                self.unmute_to = Some(vol);
                vol = 0.0;
            } else {
                vol = self.unmute_to.take().unwrap_or(100.0);
            }
        }
        speaker_icon(&painter, icon.center(), self.volume);

        // slider
        let sr = ui
            .interact(rail, ui.scope_id().with("vol"), Sense::click_and_drag())
            .on_hover_cursor(egui::CursorIcon::PointingHand);
        if let Some(p) = sr.interact_pointer_pos() {
            if sr.dragged() || sr.clicked() || sr.is_pointer_button_down_on() {
                vol = if (p.x - notch_x).abs() <= VOLUME_SNAP_PX {
                    100.0
                } else {
                    (((p.x - rail.left()) / rail.width()).clamp(0.0, 1.0) as f64) * VOLUME_MAX
                };
            }
        }
        if sr.double_clicked() {
            vol = 100.0;
        }

        // wheel over the whole control; stops at 100% until pushed through
        let over = ui.rect_contains_pointer(area);
        if over {
            // scroll up / swipe up = louder (egui reports up as negative y here)
            let d = -ui.input(|i| i.smooth_scroll_delta.y + i.smooth_scroll_delta.x);
            if d != 0.0 {
                ui.input_mut(|i| i.smooth_scroll_delta = Vec2::ZERO);
                self.poke(); // keep the controls up while adjusting
                let step = d as f64 * 0.5; // ~5% per wheel notch
                let next = (vol + step).clamp(0.0, VOLUME_MAX);
                let crosses = (vol < 100.0 && next > 100.0) || (vol > 100.0 && next < 100.0);
                if crosses {
                    vol = 100.0;
                    self.vol_hold = 0.0;
                } else if vol == 100.0 && next != 100.0 {
                    self.vol_hold += d.abs();
                    if self.vol_hold >= VOLUME_WHEEL_HOLD {
                        vol = next;
                        self.vol_hold = 0.0;
                    }
                } else {
                    vol = next;
                }
            }
        } else {
            self.vol_hold = 0.0;
        }

        // draw
        let active = sr.hovered() || sr.dragged() || over;
        let h = if active { 5.0 } else { 4.0 };
        let track = Rect::from_min_max(
            Pos2::new(rail.left(), cy - h / 2.0),
            Pos2::new(rail.right(), cy + h / 2.0),
        );
        painter.rect_filled(track, 3, Color32::from_white_alpha(55));
        let shown = vol;
        let fill_x = x_of(shown);
        painter.rect_filled(
            Rect::from_min_max(track.min, Pos2::new(fill_x.min(notch_x), track.bottom())),
            3,
            Color32::WHITE,
        );
        if shown > 100.0 {
            // boost past 100% in the accent colour
            painter.rect_filled(
                Rect::from_min_max(
                    Pos2::new(notch_x, track.top()),
                    Pos2::new(fill_x, track.bottom()),
                ),
                3,
                accent,
            );
        }
        let at_notch = shown == 100.0;
        painter.rect_filled(
            Rect::from_center_size(
                Pos2::new(notch_x, cy),
                Vec2::new(2.0, if at_notch { 14.0 } else { 10.0 }),
            ),
            1,
            if at_notch {
                Color32::WHITE
            } else {
                Color32::from_white_alpha(140)
            },
        );
        let knob = Pos2::new(fill_x, cy);
        let r = if sr.dragged() {
            7.5
        } else if active {
            6.5
        } else {
            5.5
        };
        painter.circle_filled(
            knob + Vec2::new(0.0, 1.0),
            r + 1.0,
            Color32::from_black_alpha(90),
        );
        painter.circle_filled(knob, r, Color32::WHITE);
        painter.text(
            Pos2::new(rail.right() + 8.0, cy),
            Align2::LEFT_CENTER,
            format!("{shown:.0}%"),
            FontId::proportional(12.0),
            if active {
                Color32::WHITE
            } else {
                Color32::from_white_alpha(170)
            },
        );

        ((vol - self.volume).abs() > f64::EPSILON).then_some(vol)
    }
}

/// ⏮ / ⏭: a triangle and a bar.
fn skip_icon(p: &egui::Painter, c: Pos2, fwd: bool, col: Color32) {
    let d = if fwd { 1.0 } else { -1.0 };
    p.add(egui::Shape::convex_polygon(
        vec![
            Pos2::new(c.x - 5.0 * d, c.y - 6.5),
            Pos2::new(c.x + 4.0 * d, c.y),
            Pos2::new(c.x - 5.0 * d, c.y + 6.5),
        ],
        col,
        Stroke::NONE,
    ));
    p.rect_filled(
        Rect::from_center_size(Pos2::new(c.x + 5.5 * d, c.y), Vec2::new(2.5, 13.0)),
        1,
        col,
    );
}

/// Speaker with 0–2 waves (or a cross when muted).
fn speaker_icon(p: &egui::Painter, c: Pos2, vol: f64) {
    let w = Color32::WHITE;
    p.add(egui::Shape::convex_polygon(
        vec![
            Pos2::new(c.x - 8.0, c.y - 3.5),
            Pos2::new(c.x - 4.5, c.y - 3.5),
            Pos2::new(c.x + 0.5, c.y - 8.0),
            Pos2::new(c.x + 0.5, c.y + 8.0),
            Pos2::new(c.x - 4.5, c.y + 3.5),
            Pos2::new(c.x - 8.0, c.y + 3.5),
        ],
        w,
        Stroke::NONE,
    ));
    let s = Stroke::new(1.6, w);
    if vol <= 0.0 {
        p.line_segment(
            [
                Pos2::new(c.x + 3.5, c.y - 3.5),
                Pos2::new(c.x + 9.5, c.y + 3.5),
            ],
            s,
        );
        p.line_segment(
            [
                Pos2::new(c.x + 3.5, c.y + 3.5),
                Pos2::new(c.x + 9.5, c.y - 3.5),
            ],
            s,
        );
        return;
    }
    let waves = if vol < 50.0 { 1 } else { 2 };
    for k in 0..waves {
        let r = 5.0 + 4.0 * k as f32;
        let pts: Vec<Pos2> = (0..=8)
            .map(|i| {
                let a = -0.9 + 1.8 * i as f32 / 8.0;
                Pos2::new(c.x + 0.5 + r * a.cos(), c.y + r * a.sin())
            })
            .collect();
        p.add(egui::Shape::line(pts, s));
    }
}

pub fn fmt_time(t: f64) -> String {
    let s = t.max(0.0) as u64;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

fn trim_num(x: f64) -> String {
    let s = format!("{x:.2}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}
