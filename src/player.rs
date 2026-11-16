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
            let mut s = Surface { tex, fbo, id: egui::TextureId::default(), size: [0, 0] };
            s.resize(gl, 2, 2);
            let prev = gl.get_parameter_framebuffer(glow::FRAMEBUFFER_BINDING);
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
            gl.framebuffer_texture_2d(glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::TEXTURE_2D, Some(tex), 0);
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
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32);
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

    pub pos: f64,
    pub duration: f64,
    pub paused: bool,
    pub volume: f64,
    pub speed: f64,
    pub buffering: bool,
    pub started: bool,
    pub eof: bool,
    pub cached_until: f64,
    pub error: Option<String>,
    last_log_error: Option<String>,
    /// Set when `title` was learned from mpv; the app updates the window title.
    pub title_changed: bool,
    title_from_mpv: bool,

    // UI state
    scrub: Option<f64>,
    last_scrub_seek: Instant,
    last_activity: Instant,
}

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
    pub fn start(
        tools: &Tools,
        cookies: &Path,
        video: &Video,
        profile: &Profile,
        volume: f64,
        gpa: mpv::GetProcAddress,
        ctx: &egui::Context,
    ) -> Result<Embedded, String> {
        let mut opts: Vec<(&str, String)> = vec![
            ("vo", "libmpv".into()),
            // mpv defaults to software decoding; hardware decoding is the biggest battery win.
            ("hwdec", "auto-safe".into()),
            ("ytdl", "yes".into()),
            ("ytdl-format", profile.ytdl_format.clone()),
            ("script-opts", format!("ytdl_hook-ytdl_path={}", tools.ytdlp.display())),
            ("keep-open", "yes".into()),
            ("idle", "yes".into()),
            ("osc", "no".into()),
            ("input-default-bindings", "no".into()),
            ("input-vo-keyboard", "no".into()),
            ("terminal", "no".into()),
            ("audio-client-name", "ytmini".into()),
            ("volume", format!("{volume:.0}")),
        ];
        if profile.fast {
            opts.push(("profile", "fast".into()));
        }
        if profile.audio_only {
            opts.push(("vid", "no".into()));
        }
        let mpv = Mpv::new(&opts)?;
        mpv.request_log_messages("error");
        for (i, (name, fmt)) in OBSERVED.iter().enumerate() {
            mpv.observe(i as u64 + 1, name, *fmt);
        }
        // change-list handles any characters in the path (spaces, commas).
        mpv.command(&["change-list", "ytdl-raw-options", "append", &format!("cookies={}", cookies.display())])?;

        let render = Render::new(&mpv, gpa)?;
        let shared = Box::new(Shared { ctx: ctx.clone(), frame_ready: AtomicBool::new(false) });
        let data = (&raw const *shared) as *mut c_void;
        // SAFETY: `shared` outlives both the render context and the core (see Drop).
        unsafe {
            mpv.set_wakeup_callback(on_wakeup, data);
            render.set_update_callback(on_frame, data);
        }
        mpv.command(&["loadfile", &video.url()])?;

        Ok(Embedded {
            render: Some(render),
            mpv: Some(mpv),
            shared: Some(shared),
            title: video.title().to_string(),
            id: video.id.clone(),
            url: video.url(),
            profile_label: profile.label.clone(),
            format: profile.ytdl_format.clone(),
            pos: 0.0,
            duration: 0.0,
            paused: false,
            volume,
            speed: 1.0,
            buffering: true,
            started: false,
            eof: false,
            cached_until: 0.0,
            error: None,
            last_log_error: None,
            title_changed: false,
            title_from_mpv: video.title.is_none(),
            scrub: None,
            last_scrub_seek: Instant::now(),
            last_activity: Instant::now(),
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
                            if self.title_from_mpv && !t.is_empty() && !t.starts_with("watch?v=") && t != self.title {
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
                    self.started = true;
                    self.buffering = false;
                    self.last_activity = Instant::now(); // flash the controls when playback (re)starts
                }
                mpv::EVENT_END_FILE => {
                    // SAFETY: END_FILE data is an mpv_event_end_file.
                    let e = unsafe { &*(ev.data as *const mpv::EventEndFile) };
                    if e.reason == mpv::END_FILE_REASON_ERROR {
                        self.error = Some(
                            self.last_log_error.clone().unwrap_or_else(|| "playback failed (mpv error)".into()),
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
                        self.last_log_error = Some(format!("{}: {text}", mpv::c_to_string(m.prefix)));
                    }
                }
                mpv::EVENT_SHUTDOWN => self.error = Some("mpv shut down".into()),
                _ => {}
            }
        }
    }

    /// Render the current frame into `surface` at `w`×`h` pixels if mpv has a
    /// new frame or the size changed. GL context must be current.
    fn render(&mut self, gl: &glow::Context, surface: &mut Surface, w: i32, h: i32) {
        let (Some(render), Some(shared)) = (&self.render, &self.shared) else { return };
        let resized = surface.size != [w, h];
        let new_frame = shared.frame_ready.swap(false, Ordering::Acquire) && render.update() & mpv::RENDER_UPDATE_FRAME != 0;
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
        let _ = self.mpv().command(&["seek", &format!("{secs}"), "relative+exact"]);
        self.poke();
    }

    pub fn seek_absolute(&self, t: f64, exact: bool) {
        let flags = if exact { "absolute+exact" } else { "absolute+keyframes" };
        let _ = self.mpv().command(&["seek", &format!("{t:.2}"), flags]);
    }

    pub fn set_volume(&mut self, v: f64) {
        self.volume = v.clamp(0.0, 100.0);
        let _ = self.mpv().set_f64("volume", self.volume);
    }

    pub fn set_speed(&mut self, s: f64) {
        let _ = self.mpv().set_f64("speed", s);
    }

    /// Switch format in place: keep the position, reload with the new format.
    pub fn apply_profile(&mut self, profile: &Profile) {
        if self.format == profile.ytdl_format {
            return;
        }
        self.format = profile.ytdl_format.clone();
        self.profile_label = profile.label.clone();
        let mpv = self.mpv();
        let _ = mpv.set_str("ytdl-format", &profile.ytdl_format);
        let _ = mpv.set_str("vid", if profile.audio_only { "no" } else { "auto" });
        let pos = mpv.get_f64("time-pos").unwrap_or(self.pos);
        let _ = mpv.command(&["loadfile", &self.url, "replace", "-1", &format!("start={pos:.1}")]);
    }

    /// Register user activity (shows the controls for a while).
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
}

pub struct ViewCtx<'a> {
    pub gl: &'a glow::Context,
    pub surface: &'a mut Surface,
    pub fullscreen: bool,
    pub quality: yt::Quality,
    pub screen_cap: u32,
    pub accent: Color32,
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
                if ui.add(egui::Button::new(egui::RichText::new("←  Back").size(15.0)).frame(false)).clicked() {
                    act.back = true;
                }
                ui.add_space(8.0);
                ui.add(egui::Label::new(egui::RichText::new(&self.title).size(16.0).strong()).truncate());
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
        let show_controls = self.paused || self.eof || !self.started || idle < HIDE_AFTER || self.scrub.is_some();
        if !show_controls && resp.hovered() {
            ctx.set_cursor_icon(egui::CursorIcon::None);
        } else if !self.paused {
            ctx.request_repaint_after(HIDE_AFTER.saturating_sub(idle) + Duration::from_millis(50));
        }

        // Video
        let ppp = ctx.pixels_per_point();
        let (w, h) = ((area.width() * ppp).round() as i32, (area.height() * ppp).round() as i32);
        if w > 0 && h > 0 {
            self.render(v.gl, v.surface, w, h);
            painter.image(
                v.surface.id,
                area,
                Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                Color32::WHITE,
            );
        }

        // Click: pause/resume; double-click: fullscreen.
        if resp.double_clicked() {
            act.toggle_fullscreen = true;
        } else if resp.clicked() && self.error.is_none() {
            self.toggle_pause();
        }

        // Overlays: loading, error, end
        let center = area.center();
        if let Some(err) = self.error.clone() {
            let r = Rect::from_center_size(center, Vec2::new(area.width().min(560.0), 150.0));
            ui.scope_builder(egui::UiBuilder::new().max_rect(r), |ui| {
                egui::Frame::new().fill(Color32::from_black_alpha(220)).corner_radius(12).inner_margin(14).show(ui, |ui| {
                    ui.label(egui::RichText::new("Couldn't play this video").strong().color(Color32::WHITE));
                    ui.add(egui::Label::new(egui::RichText::new(err).size(12.5).color(Color32::LIGHT_GRAY)).wrap());
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
            ui.scope_builder(egui::UiBuilder::new().max_rect(Rect::from_center_size(center, Vec2::splat(32.0))), |ui| {
                ui.add(egui::Spinner::new().size(28.0).color(Color32::WHITE));
            });
            ctx.request_repaint_after(Duration::from_millis(33));
        } else if self.eof {
            let r = Rect::from_center_size(center, Vec2::new(120.0, 40.0));
            if ui.put(r, egui::Button::new(egui::RichText::new("↻  Replay").size(16.0))).clicked() {
                self.replay();
            }
        }

        // Controls
        if show_controls && self.error.is_none() {
            let bar = Rect::from_min_max(Pos2::new(area.left(), area.bottom() - CONTROLS_H), area.right_bottom());
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
            Pos2::new(bar.left() + pad, bar.top() + 6.0),
            Pos2::new(bar.right() - pad, bar.top() + 22.0),
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
        let cached = Rect::from_min_max(track.min, Pos2::new(track.left() + track.width() * frac(self.cached_until), track.bottom()));
        painter.rect_filled(cached, 3, Color32::from_white_alpha(90));
        let played = Rect::from_min_max(track.min, Pos2::new(track.left() + track.width() * frac(shown), track.bottom()));
        painter.rect_filled(played, 3, v.accent);
        if hover {
            painter.circle_filled(Pos2::new(played.right(), track.center().y), 7.0, v.accent);
            if let Some(hp) = sresp.hover_pos() {
                let t = x_to_t(hp.x);
                let label = fmt_time(t);
                let g = painter.layout_no_wrap(label, FontId::proportional(12.0), Color32::WHITE);
                let r = Rect::from_center_size(Pos2::new(hp.x.clamp(seek.left() + 24.0, seek.right() - 24.0), seek.top() - 14.0), g.size() + Vec2::new(10.0, 4.0));
                ui.painter().rect_filled(r, 4, Color32::from_black_alpha(200));
                ui.painter().galley(r.min + Vec2::new(5.0, 2.0), g, Color32::WHITE);
            }
        }

        // --- buttons row ---
        let row = Rect::from_min_max(Pos2::new(bar.left() + pad, seek.bottom() + 2.0), Pos2::new(bar.right() - pad, bar.bottom() - 4.0));
        let btn = |ui: &mut egui::Ui, rect: Rect, id: &str| {
            let r = ui.interact(rect, ui.scope_id().with(id), Sense::click());
            if r.hovered() {
                ui.painter().rect_filled(rect, 6, Color32::from_white_alpha(25));
            }
            r.on_hover_cursor(egui::CursorIcon::PointingHand)
        };

        // play/pause
        let pp = Rect::from_min_size(row.left_top(), Vec2::splat(row.height()));
        if btn(ui, pp, "pp").clicked() {
            self.toggle_pause();
        }
        let c = pp.center();
        if self.paused || self.eof {
            let s = 9.0;
            painter.add(egui::Shape::convex_polygon(
                vec![Pos2::new(c.x - s * 0.7, c.y - s), Pos2::new(c.x - s * 0.7, c.y + s), Pos2::new(c.x + s, c.y)],
                Color32::WHITE,
                Stroke::NONE,
            ));
        } else {
            for dx in [-4.5, 4.5] {
                painter.rect_filled(Rect::from_center_size(Pos2::new(c.x + dx, c.y), Vec2::new(4.0, 16.0)), 1, Color32::WHITE);
            }
        }

        // time
        let time = if self.duration > 0.0 {
            format!("{} / {}", fmt_time(shown), fmt_time(self.duration))
        } else {
            format!("{}  · LIVE", fmt_time(shown))
        };
        painter.text(Pos2::new(pp.right() + 10.0, c.y), Align2::LEFT_CENTER, time, FontId::proportional(14.0), Color32::WHITE);

        // right side, laid out right-to-left
        let mut right = row.right();
        let mut take = |w: f32| {
            let r = Rect::from_min_max(Pos2::new(right - w, row.top()), Pos2::new(right, row.bottom()));
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
            painter.line_segment([corner, Pos2::new(corner.x - sx * l * inward, corner.y)], st);
            painter.line_segment([corner, Pos2::new(corner.x, corner.y - sy * l * inward)], st);
        }

        // quality
        let qr = take(150.0);
        ui.scope_builder(egui::UiBuilder::new().max_rect(qr).layout(egui::Layout::right_to_left(egui::Align::Center)), |ui| {
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
        });

        // speed
        let sr = take(70.0);
        ui.scope_builder(egui::UiBuilder::new().max_rect(sr).layout(egui::Layout::right_to_left(egui::Align::Center)), |ui| {
            let mut s = self.speed;
            egui::ComboBox::from_id_salt("speed")
                .selected_text(egui::RichText::new(format!("{}×", trim_num(s))).size(13.0))
                .width(60.0)
                .show_ui(ui, |ui| {
                    for x in [0.5, 0.75, 1.0, 1.25, 1.5, 1.75, 2.0] {
                        ui.selectable_value(&mut s, x, format!("{}×", trim_num(x)));
                    }
                });
            if (s - self.speed).abs() > f64::EPSILON {
                self.set_speed(s);
            }
        });

        // volume
        let vr = take(130.0);
        ui.scope_builder(egui::UiBuilder::new().max_rect(vr).layout(egui::Layout::left_to_right(egui::Align::Center)), |ui| {
            ui.label(egui::RichText::new("Vol").size(13.0).color(Color32::WHITE));
            let mut vol = self.volume;
            ui.spacing_mut().slider_width = 90.0;
            if ui.add(egui::Slider::new(&mut vol, 0.0..=100.0).show_value(false)).changed() {
                self.set_volume(vol);
                act.volume_changed = true;
            }
        });

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
