//! The eframe app: all UI state and drawing. Slow work (yt-dlp) runs on worker
//! threads and reports back over an mpsc channel.

use crate::cache::{self, Limits};
use crate::deps::{self, Dep};
use crate::mpv;
use crate::player::{self, Embedded, Surface};
use crate::positions::Positions;
use crate::signin::{self, Session};
use crate::theme::{accent, bg, card, card_hover, err, err_bg, on_accent, panel, sidebar, text, text_dim, thumb_bg, RADIUS};
use crate::settings::{Density, PlayerMode, Settings, ThumbQuality};
use crate::thumbs::{Thumbs, Variant};
use crate::tray::{self, Tray, TrayEvent};
use crate::yt::{self, Browser, Source, Tools, Video};
use eframe::egui::{
    self, text::LayoutJob, Align, Align2, Color32, CornerRadius, FontId, Key, Layout, Margin, RichText, Sense,
    Stroke, StrokeKind, TextFormat, Vec2,
};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    process::Child,
    sync::{
        mpsc::{channel, Receiver, Sender},
        Arc,
    },
    time::{Duration, SystemTime},
};

// ---------- look ----------


const GAP: f32 = 16.0;

/// Card sizes for a grid density.
#[derive(Clone, Copy)]
struct Metrics {
    min_w: f32,
    pad: f32,
    title: f32,
    meta: f32,
}

fn metrics(d: Density) -> Metrics {
    match d {
        Density::Compact => Metrics { min_w: 200.0, pad: 8.0, title: 13.0, meta: 11.5 },
        Density::Comfortable => Metrics { min_w: 260.0, pad: 10.0, title: 14.5, meta: 12.5 },
    }
}

const BASE_TABS: [Source; 5] =
    [Source::Feed, Source::Subscriptions, Source::WatchLater, Source::History, Source::Playlists];

// ---------- state ----------

enum Screen {
    MissingTools(yt::Missing),
    /// First run without yt-dlp: downloading our own copy.
    Setup,
    Onboarding,
    Main,
}

/// Background install/update of yt-dlp / deno.
#[derive(Default)]
struct DepsState {
    /// What's happening right now, for the setup screen / status bar.
    activity: Option<String>,
    error: Option<String>,
    busy: bool,
    /// "yt-dlp 2026.10.26 (auto-updated)" etc., for Settings.
    versions: Vec<(String, String)>,
}

enum Msg {
    Fetched { source: Source, generation: u64, start: usize, result: Result<Vec<Video>, String> },
    Imported(Result<(), String>),
    Power(yt::Power),
    Switched(Result<(), String>),
    CacheUsage(u64),
    DepProgress(deps::Progress),
    /// A deps job finished: Ok(message for the status bar) / Err.
    DepsDone(Result<Option<String>, String>),
    Versions(Vec<(String, String)>),
    /// Progress of the separate-window player: (video id, pos, dur).
    ExternalProgress(String, f64, f64),
    /// Is the sign-in browser profile signed in to Google yet?
    SignInCheck(Result<bool, String>),
    /// Watch later add (true) / remove finished for a video id.
    WatchLater(bool, String, Result<String, String>),
}

#[derive(Default)]
struct Tab {
    videos: Vec<Video>,
    loading: bool,
    error: Option<String>,
    /// Last page came back short: nothing more to load.
    exhausted: bool,
    /// Bumped on refresh so results from older requests are ignored.
    generation: u64,
    loaded_once: bool,
    scroll_to_top: bool,
    /// Showing results from the disk cache (saved at this unix time) until fresh ones arrive.
    cached_at: Option<u64>,
}

/// Disk-cache name for the personal tabs (search results aren't persisted).
fn list_name(s: &Source) -> Option<&'static str> {
    match s {
        Source::Feed => Some("feed"),
        Source::Subscriptions => Some("subscriptions"),
        Source::WatchLater => Some("watchlater"),
        Source::History => Some("history"),
        Source::Playlists => Some("playlists"),
        Source::Search(_) | Source::Channel { .. } | Source::ChannelPlaylists { .. } | Source::Playlist { .. } => None,
    }
}

/// A Google sign-in in progress in the user's own browser profile.
struct SignIn {
    session: Session,
    /// Google's sign-in page has been opened in the browser.
    opened: bool,
    /// A cookie check is running on a worker.
    checking: bool,
    last_check: std::time::Instant,
    started: std::time::Instant,
}

/// Check the browser this often while waiting for the sign-in.
const SIGNIN_POLL: Duration = Duration::from_secs(3);
/// Give up waiting after this long.
const SIGNIN_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// What a click on a card means.
enum CardAction {
    None,
    Open,
    Channel,
    Quick(Quick),
}

/// Card menu (right-click or ⋯).
#[derive(Clone, Copy)]
enum Quick {
    WatchLater(bool),
    CopyLink,
    OpenInBrowser,
    Watched(bool),
}

struct Player {
    child: Child,
    title: String,
    id: String,
    url: String,
    /// e.g. "720p30 · battery saver"
    profile: String,
    /// The ytdl-format mpv is currently using.
    format: String,
}

pub struct App {
    tools: Option<Tools>,
    cookies: PathBuf,
    screen: Screen,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,

    // onboarding
    browsers: Vec<Browser>,
    /// Browser profiles for "Sign in with Google", most recent first.
    sessions: Vec<Session>,
    session_pick: usize,
    /// For the fade-in when the grid changes.
    view_shown: Source,
    view_since: std::time::Instant,
    /// Where the import in flight comes from, saved on success for "Refresh session".
    import_source: Option<(String, std::path::PathBuf)>,
    custom_spec: String,
    importing: Option<String>,
    import_error: Option<String>,

    // main
    tabs: HashMap<Source, Tab>,
    active: Source,
    search_query: String,
    last_search: Option<Source>,
    player: Option<Player>,
    status: Option<(String, bool)>,
    focus_search: bool,
    thumbs: Thumbs,
    power: yt::Power,
    /// Highest useful video height on the monitor the window is on.
    screen_cap: u32,

    settings: Settings,
    saved_settings: Settings,
    limits: Arc<Limits>,
    cache_usage: Option<u64>,
    show_settings: bool,

    // embedded player
    gpa: Option<mpv::GetProcAddress>,
    surface: Option<Surface>,
    embedded: Option<Embedded>,
    /// Clicked video waiting for `ui` (where the GL context and Frame are available).
    pending_play: Option<Video>,
    smoke: Option<Smoke>,

    // background (tray)
    tray: Option<Tray>,
    tray_rx: Receiver<TrayEvent>,
    hidden: bool,
    quitting: bool,
    deps: DepsState,

    positions: Positions,
    /// Pages to go back to (channel → playlist → …); top tabs clear it.
    nav_back: Vec<Source>,
    signin: Option<SignIn>,
    last_ext_poll: std::time::Instant,
}

/// `--smoke-test <id>`: drives the embedded player through play → seek → back →
/// play → live quality switch → quit-while-playing, printing PASS/FAIL to stderr.
/// For checking teardown after mpv/egui upgrades without clicking around.
struct Smoke {
    video: Video,
    step: u8,
    since: std::time::Instant,
    failed: bool,
    mark: f64,
}

impl Smoke {
    fn log(&mut self, ok: bool, what: &str) {
        self.failed |= !ok;
        eprintln!("[smoke {:>5.1}s] {} {what}", self.since.elapsed().as_secs_f32(), if ok { "PASS" } else { "FAIL" });
    }
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let ctx = &cc.egui_ctx;
        crate::theme::init(ctx);

        let (tx, rx) = channel();
        let cookies = yt::cookie_file().unwrap_or_else(|_| PathBuf::from("cookies.txt"));
        let settings = Settings::load();
        let (tray_tx, tray_rx) = channel();
        let tray = Tray::new(ctx, tray_tx).ok();
        let limits = Arc::new(Limits::new(settings.cache_mb, settings.cache_days));
        let mut app = App {
            tools: None,
            cookies,
            screen: Screen::Onboarding,
            tx,
            rx,
            browsers: yt::detect_browsers(),
            sessions: signin::sessions(),
            session_pick: 0,
            import_source: None,
            view_shown: Source::Feed,
            view_since: std::time::Instant::now(),
            custom_spec: String::new(),
            importing: None,
            import_error: None,
            tabs: HashMap::new(),
            active: Source::Feed,
            search_query: String::new(),
            last_search: None,
            player: None,
            status: None,
            focus_search: false,
            thumbs: Thumbs::new(ctx, limits.clone()),
            power: yt::power_state(),
            screen_cap: 1080,
            saved_settings: settings.clone(),
            settings,
            limits,
            cache_usage: None,
            show_settings: false,
            gpa: cc.get_proc_address.clone(),
            surface: None,
            embedded: None,
            pending_play: None,
            smoke: None,
            tray,
            tray_rx,
            hidden: false,
            quitting: false,
            deps: DepsState::default(),
            positions: Positions::load(),
            nav_back: Vec::new(),
            signin: None,
            last_ext_poll: std::time::Instant::now(),
        };
        app.enforce_cache(ctx);
        // `ytmini --play <url|id>` opens straight into the player.
        let mut args = std::env::args().skip(1);
        while let Some(a) = args.next() {
            if a == "--smoke-test" {
                if let Some(v) = args.next().as_deref().and_then(Video::from_url_or_id) {
                    app.settings.player = PlayerMode::Embedded;
                    app.smoke = Some(Smoke { video: v.clone(), step: 0, since: std::time::Instant::now(), failed: false, mark: 0.0 });
                    app.play(&v);
                }
            }
            if a == "--play" {
                if let Some(v) = args.next().as_deref().and_then(Video::from_url_or_id) {
                    app.play(&v);
                }
            }
        }
        let (tx, ctx2) = (app.tx.clone(), ctx.clone());
        yt::watch_power(app.power, move |p| {
            let _ = tx.send(Msg::Power(p));
            ctx2.request_repaint();
        });
        app.check_tools(ctx);
        app
    }

    fn check_tools(&mut self, ctx: &egui::Context) {
        match Tools::locate() {
            Ok(t) => {
                // Switch to our own auto-updating yt-dlp (and get deno if there's
                // none) in the background; the system copy keeps working meanwhile.
                let mut need = Vec::new();
                if !t.ytdlp_managed {
                    need.push(Dep::YtDlp);
                }
                if t.deno.is_none() {
                    need.push(Dep::Deno);
                }
                let update_due = t.ytdlp_managed && deps::check_due(Dep::YtDlp, Duration::from_secs(24 * 3600));
                self.tools = Some(t);
                if !need.is_empty() {
                    self.run_deps_job(ctx, need, false);
                } else if update_due {
                    self.run_deps_job(ctx, vec![Dep::YtDlp], true);
                }
                self.refresh_versions(ctx);
                let on_main = matches!(self.screen, Screen::Main);
                if yt::logged_in(&self.cookies) {
                    self.screen = Screen::Main;
                    if !on_main {
                        self.load(ctx, Source::Feed, false);
                    }
                } else {
                    self.screen = Screen::Onboarding;
                }
            }
            Err(m) if m.ytdlp && !m.player => {
                // Nothing to list videos with yet: download yt-dlp (+ deno) first.
                self.screen = Screen::Setup;
                if !self.deps.busy {
                    self.run_deps_job(ctx, vec![Dep::YtDlp, Dep::Deno], false);
                }
            }
            Err(m) => self.screen = Screen::MissingTools(m),
        }
    }

    /// Install (or, with `update`, check-and-update) deps on a worker thread.
    fn run_deps_job(&mut self, ctx: &egui::Context, list: Vec<Dep>, update: bool) {
        if self.deps.busy {
            return;
        }
        self.deps.busy = true;
        self.deps.error = None;
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        let deno_on_system = self.tools.as_ref().and_then(|t| t.deno.clone()).is_some_and(|p| Dep::Deno.managed() != Some(p));
        std::thread::spawn(move || {
            let report = |p: deps::Progress| {
                let _ = tx.send(Msg::DepProgress(p));
                ctx.request_repaint();
            };
            let mut msgs = Vec::new();
            let mut result = Ok(());
            for dep in list {
                if dep == Dep::Deno && (deno_on_system || Dep::Deno.managed().is_some()) {
                    continue;
                }
                let r = if update {
                    deps::update_if_needed(dep, &report).map(|v| v.map(|v| format!("{} updated to {v}", dep.name())))
                } else {
                    deps::install(dep, &report).map(|exe| {
                        Some(format!("{} {} installed", dep.name(), deps::version(&exe).unwrap_or_default()))
                    })
                };
                match r {
                    Ok(Some(m)) => msgs.push(m),
                    Ok(None) => {}
                    Err(e) => {
                        result = Err(format!("{}: {e:#}", dep.name()));
                        break;
                    }
                }
            }
            let msg = (!msgs.is_empty()).then(|| msgs.join(", "));
            let _ = tx.send(Msg::DepsDone(result.map(|()| msg)));
            ctx.request_repaint();
        });
    }

    /// Ask each tool for its version (spawns processes → worker thread).
    fn refresh_versions(&self, ctx: &egui::Context) {
        let Some(t) = self.tools.clone() else { return };
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let ytdlp_src = if t.ytdlp_managed { "auto-updated by ytmini" } else { "system copy" };
            let mut v = vec![(
                "yt-dlp".to_string(),
                format!("{} · {ytdlp_src}", deps::version(&t.ytdlp).unwrap_or_else(|| "?".into())),
            )];
            v.push((
                "deno".into(),
                match &t.deno {
                    Some(d) => {
                        let src = if Dep::Deno.managed().as_ref() == Some(d) { "downloaded by ytmini" } else { "system copy" };
                        format!("{} · {src}", deps::version(d).unwrap_or_else(|| "?".into()).trim_start_matches("deno "))
                    }
                    None => "not found — YouTube may fail".into(),
                },
            ));
            let player = match mpv::bundled_or_system() {
                Some(true) => "libmpv bundled in ytmini.app",
                Some(false) => "system libmpv",
                None => "separate window only (no libmpv)",
            };
            v.push(("player".into(), player.into()));
            let _ = tx.send(Msg::Versions(v));
            ctx.request_repaint();
        });
    }

    // ---------- background work ----------

    fn spawn_fetch(&self, ctx: &egui::Context, source: Source, generation: u64, start: usize) {
        let Some(tools) = self.tools.clone() else { return };
        let (tx, ctx, cookies) = (self.tx.clone(), ctx.clone(), self.cookies.clone());
        std::thread::spawn(move || {
            let result = yt::fetch(&tools, &cookies, &source, start, yt::PAGE).map_err(|e| format!("{e:#}"));
            let _ = tx.send(Msg::Fetched { source, generation, start, result });
            ctx.request_repaint();
        });
    }

    /// Load the first page of `source` (if not cached, or always when `refresh`).
    fn load(&mut self, ctx: &egui::Context, source: Source, refresh: bool) {
        let tab = self.tabs.entry(source.clone()).or_default();
        if tab.loading && !refresh {
            return;
        }
        if tab.loaded_once && !refresh {
            return;
        }
        if tab.videos.is_empty() {
            if let Some(cached) = list_name(&source).and_then(cache::read_list) {
                tab.videos = cached.videos;
                tab.cached_at = Some(cached.saved_at);
            }
        }
        tab.generation += 1;
        tab.loading = true;
        tab.error = None;
        let generation = tab.generation;
        self.spawn_fetch(ctx, source, generation, 0);
    }

    fn load_more(&mut self, ctx: &egui::Context, source: Source) {
        let tab = self.tabs.entry(source.clone()).or_default();
        if tab.loading || tab.exhausted {
            return;
        }
        tab.loading = true;
        tab.error = None;
        let (generation, start) = (tab.generation, tab.videos.len());
        self.spawn_fetch(ctx, source, generation, start);
    }

    fn start_import(&mut self, ctx: &egui::Context, spec: String) {
        let Some(tools) = self.tools.clone() else { return };
        self.import_error = None;
        self.import_source = None;
        self.importing = Some(spec.clone());
        let (tx, ctx, cookies) = (self.tx.clone(), ctx.clone(), self.cookies.clone());
        std::thread::spawn(move || {
            let r = yt::import_cookies(&tools, &spec, &cookies).map_err(|e| format!("{e:#}"));
            let _ = tx.send(Msg::Imported(r));
            ctx.request_repaint();
        });
    }

    fn handle_messages(&mut self, ctx: &egui::Context) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Fetched { source, generation, start, result } => {
                    let name = list_name(&source);
                    let tab = self.tabs.entry(source).or_default();
                    if tab.generation != generation {
                        continue; // superseded by a refresh
                    }
                    tab.loading = false;
                    match result {
                        Ok(vids) => {
                            tab.exhausted = vids.len() < yt::PAGE;
                            if start == 0 {
                                tab.videos = vids;
                                tab.scroll_to_top = true;
                            } else {
                                let seen: HashSet<String> = tab.videos.iter().map(|v| v.id.clone()).collect();
                                let before = tab.videos.len();
                                tab.videos.extend(vids.into_iter().filter(|v| !seen.contains(&v.id)));
                                if tab.videos.len() == before {
                                    tab.exhausted = true;
                                }
                            }
                            tab.loaded_once = true;
                            tab.cached_at = None;
                            if let Some(name) = name {
                                cache::write_list(name, &tab.videos);
                            }
                        }
                        Err(e) => {
                            let looks_stale = ["Sign in to confirm", "signature", "challenge", "HTTP Error 403", "nsig"]
                                .iter()
                                .any(|k| e.contains(k));
                            tab.error = Some(e);
                            let managed = self.tools.as_ref().is_some_and(|t| t.ytdlp_managed);
                            if looks_stale && managed && deps::check_due(Dep::YtDlp, Duration::from_secs(3600)) {
                                self.run_deps_job(ctx, vec![Dep::YtDlp], true);
                            }
                        }
                    }
                }
                Msg::Power(p) => {
                    self.power = p;
                    self.apply_profile_live(ctx);
                }
                Msg::CacheUsage(n) => self.cache_usage = Some(n),
                Msg::DepProgress(p) => {
                    self.deps.activity = Some(match p {
                        deps::Progress::Downloading { dep, done, total } => match total {
                            Some(t) => format!("Downloading {} — {:.0} / {:.0} MB", dep.name(), done as f64 / 1e6, t as f64 / 1e6),
                            None => format!("Downloading {} — {:.0} MB", dep.name(), done as f64 / 1e6),
                        },
                        deps::Progress::Verifying(dep) => format!("Verifying {} checksum…", dep.name()),
                        deps::Progress::Preparing(dep) => {
                            format!("Preparing {} (macOS checks new apps once — about 15 s)…", dep.name())
                        }
                    });
                }
                Msg::DepsDone(r) => {
                    self.deps.busy = false;
                    self.deps.activity = None;
                    match r {
                        Ok(msg) => {
                            if let Some(m) = msg {
                                self.status = Some((m, false));
                            }
                            self.check_tools(ctx); // pick up the new copies
                        }
                        Err(e) => {
                            self.deps.error = Some(e.clone());
                            if !matches!(self.screen, Screen::Setup) {
                                self.status = Some((format!("Couldn't update tools: {e}"), true));
                            }
                        }
                    }
                }
                Msg::Versions(v) => self.deps.versions = v,
                Msg::ExternalProgress(id, pos, dur) => self.positions.update(&id, pos, dur),
                Msg::Switched(r) => {
                    if let Err(e) = r {
                        self.status = Some((format!("Couldn't switch quality: {e}"), true));
                    }
                }
                Msg::Imported(r) => {
                    self.importing = None;
                    match r {
                        Ok(()) => {
                            self.import_error = None;
                            if let Some((id, profile)) = self.import_source.take() {
                                self.settings.signin_source = Some(crate::settings::SignInSource { id, profile });
                                self.settings.save();
                            }
                            self.screen = Screen::Main;
                            self.tabs.clear(); // new account/cookies: drop everything cached
                            self.status = Some(("Signed in — cookies imported.".into(), false));
                            self.load(ctx, self.active.clone(), true);
                        }
                        Err(e) => self.import_error = Some(e),
                    }
                }
                Msg::SignInCheck(r) => self.on_signin_check(ctx, r),
                Msg::WatchLater(add, id, r) => {
                    match r {
                        Ok(msg) => self.status = Some((msg, false)),
                        Err(e) => self.status = Some((e, true)),
                    }
                    // keep the Watch later tab in step without refetching
                    if let Some(tab) = self.tabs.get_mut(&Source::WatchLater) {
                        if !add {
                            tab.videos.retain(|v| v.id != id);
                        } else {
                            tab.loaded_once = false; // refetch next time it's opened
                        }
                    }
                }
            }
        }
    }

    // ---------- player ----------

    fn play(&mut self, video: &Video) {
        self.stop_player();
        if self.settings.player == PlayerMode::Embedded && self.gpa.is_some() {
            match mpv::available() {
                Ok(()) => {
                    self.pending_play = Some(video.clone());
                    return;
                }
                Err(e) => {
                    self.status = Some((format!("Embedded player unavailable ({e}) — using a separate window"), true));
                }
            }
        }
        self.play_in_window(video);
    }

    /// Start a pending embedded playback. Runs in `ui`, where GL is current.
    fn start_embedded(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        let Some(video) = self.pending_play.take() else { return };
        let (Some(tools), Some(gpa), Some(gl)) = (self.tools.clone(), self.gpa.clone(), frame.gl().cloned()) else {
            return self.play_in_window(&video);
        };
        if self.surface.is_none() {
            match Surface::new(&gl, frame) {
                Ok(s) => self.surface = Some(s),
                Err(e) => {
                    self.status = Some((format!("Embedded player failed ({e}) — using a separate window"), true));
                    return self.play_in_window(&video);
                }
            }
        }
        self.power = yt::power_state();
        let profile = yt::profile(self.settings.quality, self.power, self.screen_cap);
        let resume = self.positions.resume_at(&video.id);
        match Embedded::start(&tools, &self.cookies, &video, &profile, &self.settings, resume, gpa, ctx) {
            Ok(p) => {
                self.status = None;
                ctx.send_viewport_cmd(egui::ViewportCommand::Title(format!("{} — ytmini", video.title())));
                self.embedded = Some(p);
            }
            Err(e) => {
                self.status = Some((format!("Embedded player failed ({e}) — using a separate window"), true));
                self.play_in_window(&video);
            }
        }
    }

    /// Leave the player view: tear mpv down, give back VRAM, restore the window.
    fn close_embedded(&mut self, ctx: &egui::Context, frame: &eframe::Frame) {
        if let Some(p) = self.embedded.take() {
            self.settings.volume = p.volume;
            if p.started {
                self.positions.update(&p.id, p.pos, p.duration);
            }
            self.positions.save();
            drop(p); // frees the render context here (GL current), core shuts down in background
        }
        if let (Some(s), Some(gl)) = (&mut self.surface, frame.gl()) {
            s.shrink(gl);
        }
        if ctx.input(|i| i.viewport().fullscreen.unwrap_or(false)) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Title("ytmini".into()));
    }

    fn play_in_window(&mut self, video: &Video) {
        let Some(tools) = &self.tools else { return };
        self.power = yt::power_state();
        let profile = yt::profile(self.settings.quality, self.power, self.screen_cap);
        let resume = self.positions.resume_at(&video.id);
        match yt::play(tools, &self.cookies, video, &profile, self.settings.mark_watched, resume) {
            Ok(child) => {
                self.status = None;
                self.player = Some(Player {
                    child,
                    title: video.title().to_string(),
                    id: video.id.clone(),
                    url: video.url(),
                    profile: profile.label,
                    format: profile.ytdl_format,
                });
            }
            Err(e) => self.status = Some((format!("Couldn't start mpv: {e:#}"), true)),
        }
    }

    /// If the effective quality changed (power state or menu), retune the
    /// running player in place.
    fn apply_profile_live(&mut self, ctx: &egui::Context) {
        let profile = yt::profile(self.settings.quality, self.power, self.screen_cap);
        if let Some(e) = &mut self.embedded {
            e.apply_profile(&profile);
        }
        let Some(p) = &mut self.player else { return };
        if p.format == profile.ytdl_format {
            return;
        }
        p.format = profile.ytdl_format.clone();
        p.profile = profile.label.clone();
        let (tx, ctx, url) = (self.tx.clone(), ctx.clone(), p.url.clone());
        std::thread::spawn(move || {
            let r = yt::switch_profile(&url, &profile).map_err(|e| format!("{e:#}"));
            let _ = tx.send(Msg::Switched(r));
            ctx.request_repaint();
        });
    }

    /// Track the monitor the window is on (it can be dragged between screens).
    fn update_screen_cap(&mut self, ctx: &egui::Context) {
        let (size, ppp) = ctx.input(|i| (i.viewport().monitor_size, i.viewport().native_pixels_per_point));
        let (Some(size), Some(ppp)) = (size, ppp) else { return };
        let cap = yt::screen_cap(size.x * ppp, size.y * ppp);
        if cap != self.screen_cap {
            self.screen_cap = cap;
            self.apply_profile_live(ctx);
        }
    }

    /// Apply cache limits on a worker thread and report the resulting size.
    fn enforce_cache(&self, ctx: &egui::Context) {
        let (limits, tx, ctx) = (self.limits.clone(), self.tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let _ = tx.send(Msg::CacheUsage(cache::enforce(&limits)));
            ctx.request_repaint();
        });
    }

    fn stop_player(&mut self) {
        if let Some(mut p) = self.player.take() {
            let _ = p.child.kill();
            let _ = p.child.wait(); // reap — no zombies
            if cfg!(unix) {
                let _ = std::fs::remove_file(yt::ipc_path()); // killed mpv can't clean up its socket
            }
        }
    }

    fn poll_player(&mut self) {
        let Some(p) = &mut self.player else { return };
        match p.child.try_wait() {
            Ok(None) => {}
            Ok(Some(status)) => {
                self.status = if status.success() {
                    None
                } else {
                    let why = yt::last_player_error().unwrap_or_else(|| format!("mpv exited with {status}"));
                    Some((format!("Playback failed: {why}"), true))
                };
                self.player = None;
            }
            Err(e) => {
                self.status = Some((format!("Lost track of mpv: {e}"), true));
                self.player = None;
            }
        }
    }

    // ---------- screens ----------

    fn ui_missing(&mut self, ui: &mut egui::Ui, missing: &yt::Missing) {
        let mut retry = false;
        centered_column(ui, 520.0, |ui| {
            ui.label(RichText::new("ytmini").size(34.0).strong().color(accent()));
            ui.add_space(8.0);
            ui.label(RichText::new("No video player found").size(20.0).color(text()));
            ui.add_space(12.0);
            let _ = missing;
            ui.label("This build of ytmini doesn't include libmpv, and mpv isn't installed. Install it with:");
            ui.add_space(8.0);
            code_box(ui, yt::install_hint());
            ui.add_space(16.0);
            if accent_button(ui, "Check again").clicked() {
                retry = true;
            }
        });
        if retry {
            self.check_tools(ui.ctx());
        }
    }

    fn ui_setup(&mut self, ui: &mut egui::Ui) {
        let mut retry = false;
        centered_column(ui, 520.0, |ui| {
            ui.label(RichText::new("ytmini").size(34.0).strong().color(accent()));
            ui.add_space(8.0);
            ui.label(RichText::new("Getting ready").size(20.0).color(text()));
            ui.add_space(6.0);
            ui.label(
                RichText::new(
                    "Downloading yt-dlp (lists videos, ~55 MB) and deno (solves YouTube's checks, ~40 MB) \
                     from their official releases. Checksums are verified; yt-dlp keeps itself up to date.",
                )
                .color(text_dim()),
            );
            ui.add_space(16.0);
            if let Some(a) = &self.deps.activity {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(a);
                });
            } else if self.deps.busy {
                ui.spinner();
            }
            if let Some(e) = &self.deps.error {
                error_box(ui, e);
                ui.add_space(8.0);
                if accent_button(ui, "Try again").clicked() {
                    retry = true;
                }
            }
        });
        if retry {
            self.check_tools(ui.ctx());
        }
    }

    fn ui_onboarding(&mut self, ui: &mut egui::Ui) {
        let mut chosen: Option<String> = None;
        let mut cancel = false;
        let can_cancel = yt::logged_in(&self.cookies) && self.importing.is_none();
        let busy = self.importing.is_some();
        let browsers = self.browsers.clone();
        let mut google = GoogleAction::None;

        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            centered_column(ui, 560.0, |ui| {
                ui.label(RichText::new("ytmini").size(34.0).strong().color(accent()));
                ui.add_space(4.0);
                ui.label(RichText::new("Sign in to YouTube").size(20.0).color(text()));
                ui.add_space(14.0);
                google = self.ui_google_signin(ui, busy);
                ui.add_space(22.0);
                ui.label(RichText::new("Or use a browser you're already signed in to").strong().color(text()));
                ui.label(RichText::new("Cookies are stored only on this computer.").small().color(text_dim()));
                ui.add_space(10.0);

                if browsers.is_empty() {
                    ui.label(RichText::new("No browsers detected — type a yt-dlp spec below.").color(text_dim()));
                }
                for b in &browsers {
                    egui::Frame::new()
                        .fill(card())
                        .corner_radius(RADIUS)
                        .inner_margin(Margin::symmetric(14, 10))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.horizontal(|ui| {
                                ui.vertical(|ui| {
                                    ui.label(RichText::new(b.name).size(16.0).strong().color(text()));
                                    ui.add(
                                        egui::Label::new(RichText::new(yt::browser_note(b.id)).size(12.5).color(text_dim()))
                                            .wrap(),
                                    );
                                });
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    if ui.add_enabled(!busy, accent_btn("Use")).clicked() {
                                        chosen = Some(b.id.to_string());
                                    }
                                });
                            });
                        });
                    ui.add_space(8.0);
                }

                ui.add_space(8.0);
                ui.label(RichText::new("Or a yt-dlp browser spec").color(text_dim()));
                ui.horizontal(|ui| {
                    let r = ui.add_enabled(
                        !busy,
                        egui::TextEdit::singleline(&mut self.custom_spec)
                            .hint_text("chrome:Profile 1")
                            .desired_width(ui.available_width() - 90.0),
                    );
                    let enter = r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                    if (ui.add_enabled(!busy, accent_btn("Import")).clicked() || enter)
                        && !self.custom_spec.trim().is_empty()
                    {
                        chosen = Some(self.custom_spec.trim().to_string());
                    }
                });
                if !self.custom_spec.trim().is_empty() {
                    ui.label(RichText::new(yt::browser_note(&self.custom_spec)).size(12.5).color(text_dim()));
                }

                ui.add_space(16.0);
                if let Some(spec) = &self.importing {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(format!("Reading cookies from {spec}… (answer any system prompt)"));
                    });
                }
                if let Some(err) = &self.import_error {
                    egui::Frame::new()
                        .fill(err_bg())
                        .corner_radius(RADIUS)
                        .inner_margin(Margin::same(12))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.label(RichText::new("Import failed").strong().color(crate::theme::err()));
                            ui.add(egui::Label::new(RichText::new(err).monospace().size(12.0).color(text())).wrap());
                        });
                }
                if can_cancel {
                    ui.add_space(12.0);
                    if ui.button("← Back to videos").clicked() {
                        cancel = true;
                    }
                }
            });
        });

        if let Some(spec) = chosen {
            self.start_import(ui.ctx(), spec);
        }
        match google {
            GoogleAction::Start => {
                if let Some(s) = self.sessions.get(self.session_pick).cloned() {
                    self.start_signin(ui.ctx(), s);
                }
            }
            GoogleAction::CheckNow => {
                if let Some(s) = &mut self.signin {
                    s.last_check = std::time::Instant::now() - SIGNIN_POLL;
                }
            }
            GoogleAction::Cancel => self.signin = None,
            GoogleAction::None => {}
        }
        if cancel {
            self.import_error = None;
            self.screen = Screen::Main;
        }
    }

    fn ui_main(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();

        // Shortcuts: ⌘/Ctrl+R refresh, ⌘/Ctrl+F search.
        let (refresh_key, find_key) = ui.input_mut(|i| {
            (
                i.consume_key(egui::Modifiers::COMMAND, Key::R),
                i.consume_key(egui::Modifiers::COMMAND, Key::F),
            )
        });
        if find_key {
            self.focus_search = true;
        }
        let mut refresh = refresh_key;
        let mut switch_to: Option<Source> = None;
        let mut search: Option<String> = None;

        // ----- sidebar -----
        let narrow = ctx.content_rect().width() < 760.0;
        let collapsed = self.settings.sidebar_collapsed || narrow;
        egui::Panel::left("nav")
            .resizable(false)
            .exact_size(if collapsed { 60.0 } else { 208.0 })
            .frame(egui::Frame::new().fill(sidebar()).inner_margin(Margin { left: 10, right: 10, top: 14, bottom: 10 }))
            .show_separator_line(false)
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                let item = |ui: &mut egui::Ui, icon: Icon, label: &str, selected: bool| -> bool {
                    nav_item(ui, icon, label, selected, collapsed).clicked()
                };
                ui.add_space(2.0);
                if !collapsed {
                    ui.label(RichText::new("ytmini").size(17.0).strong().color(text()));
                    ui.add_space(12.0);
                } else {
                    ui.add_space(4.0);
                }
                let home_of = |t: &Source| t == &self.active || (self.active.is_page() && self.nav_back.first() == Some(t));
                for t in BASE_TABS {
                    if t == Source::WatchLater && !collapsed {
                        ui.add_space(10.0);
                        ui.label(RichText::new("Library").small().color(text_dim()));
                        ui.add_space(2.0);
                    }
                    if item(ui, nav_icon(&t), &t.label(), home_of(&t)) {
                        switch_to = Some(t.clone());
                    }
                }
                if let Some(s) = self.last_search.clone() {
                    ui.add_space(10.0);
                    let label = match &s {
                        Source::Search(q) => format!("“{q}”"),
                        other => other.label(),
                    };
                    if item(ui, Icon::Search, &label, home_of(&s)) {
                        switch_to = Some(s);
                    }
                }
                ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
                    if !narrow {
                        let (icon, tip) = if collapsed { (Icon::Expand, "Expand sidebar") } else { (Icon::Collapse, "Collapse sidebar") };
                        if nav_item(ui, icon, "Collapse", false, collapsed).on_hover_text(tip).clicked() {
                            self.settings.sidebar_collapsed = !self.settings.sidebar_collapsed;
                        }
                    }
                    if item(ui, Icon::Gear, "Settings", self.show_settings) {
                        self.show_settings = !self.show_settings;
                    }
                });
            });

        // ----- top bar: search + refresh -----
        egui::Panel::top("top")
            .frame(egui::Frame::new().fill(bg()).inner_margin(Margin { left: 20, right: 16, top: 12, bottom: 8 }))
            .show_separator_line(false)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let w = (ui.available_width() - 60.0).clamp(160.0, 520.0);
                    let r = ui.add(
                        egui::TextEdit::singleline(&mut self.search_query)
                            .hint_text("Search YouTube")
                            .desired_width(w)
                            .margin(Margin::symmetric(12, 7)),
                    );
                    if self.focus_search {
                        r.request_focus();
                        self.focus_search = false;
                    }
                    if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                        let q = self.search_query.trim();
                        if !q.is_empty() {
                            search = Some(q.to_string());
                        }
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let b = egui::Button::new(RichText::new("↻").size(18.0).color(text_dim())).frame(false);
                        if ui.add(b).on_hover_text("Refresh (⌘R)").clicked() {
                            refresh = true;
                        }
                    });
                });
            });

        // ----- status bar -----
        let active_loading = self.tabs.get(&self.active).is_some_and(|t| t.loading);
        let mut stop = false;
        egui::Panel::bottom("status")
            .frame(egui::Frame::new().fill(panel()).inner_margin(Margin::symmetric(16, 6)))
            .show_separator_line(false)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let cached_at = self.tabs.get(&self.active).and_then(|t| t.cached_at);
                    if active_loading {
                        ui.spinner();
                        let msg = match cached_at {
                            Some(t) => format!("Updating… (showing results from {})", ago(t)),
                            None => "Loading…".into(),
                        };
                        ui.label(RichText::new(msg).color(text_dim()));
                    } else if let Some(p) = &self.player {
                        ui.label(RichText::new("▶").color(accent()));
                        ui.label(RichText::new(format!("Playing: {}", p.title)).color(text()));
                        ui.label(RichText::new(&p.profile).color(text_dim()));
                        if ui.small_button("Stop").clicked() {
                            stop = true;
                        }
                    } else if let Some((msg, is_err)) = &self.status {
                        ui.label(RichText::new(msg).color(if *is_err { err() } else { text_dim() }));
                    } else {
                        let n = self.tabs.get(&self.active).map_or(0, |t| t.videos.len());
                        ui.label(RichText::new(format!("{n} videos")).color(text_dim()));
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let power = if self.power.saver {
                            "battery saver"
                        } else if self.power.on_battery {
                            "on battery"
                        } else {
                            ""
                        };
                        if !power.is_empty() {
                            ui.label(RichText::new(power).small().color(text_dim()))
                                .on_hover_text("Auto quality lowers resolution/frame rate to save power");
                        }
                    });
                });
            });
        if stop {
            self.stop_player();
        }

        // ----- grid -----
        self.ui_page_header(ui);
        let mut clicked: Option<Video> = None;
        let mut open_channel: Option<Video> = None;
        let mut quick: Option<(Quick, Video)> = None;
        if self.active != self.view_shown {
            self.view_shown = self.active.clone();
            self.view_since = std::time::Instant::now();
        }
        let mut more = false;
        let mut reimport_from_empty = false;
        egui::CentralPanel::no_frame().show(ui, |ui| {
            let source = self.active.clone();
            let playing_id = self.player.as_ref().map(|p| p.id.clone());
            let thumbs = &mut self.thumbs;
            let positions = &self.positions;
            let Some(tab) = self.tabs.get_mut(&source) else { return };
            let avail_w = ui.available_width() - 2.0 * GAP;
            let m = metrics(self.settings.density);
            let cols = (((avail_w + GAP) / (m.min_w + GAP)).floor() as usize).max(1);
            let card_w = (avail_w - GAP * (cols as f32 - 1.0)) / cols as f32;
            let thumb_h = card_w * 9.0 / 16.0;
            let title_h = ui.fonts_mut(|f| f.row_height(&FontId::proportional(m.title)));
            let meta_h = ui.fonts_mut(|f| f.row_height(&FontId::proportional(m.meta)));
            let card_h = thumb_h + m.pad + title_h * 2.0 + 4.0 + meta_h + m.pad;

            // Fade the grid in after switching tabs/pages.
            let fade = (self.view_since.elapsed().as_secs_f32() / 0.18).min(1.0);
            if fade < 1.0 {
                ui.multiply_opacity(fade);
                ui.ctx().request_repaint();
            }

            if tab.videos.is_empty() && tab.loading {
                skeleton_grid(ui, cols, Vec2::new(card_w, card_h), thumb_h);
                return;
            }
            if tab.videos.is_empty() {
                centered_column(ui, 520.0, |ui| {
                    if let Some(e) = &tab.error {
                        error_box(ui, e);
                        if source == Source::Feed {
                            ui.add_space(8.0);
                            if ui.button("Re-import cookies…").clicked() {
                                reimport_from_empty = true;
                            }
                        }
                    } else if tab.loaded_once {
                        if source == Source::Feed {
                            ui.label(RichText::new("Your feed came back empty").size(18.0).color(text()));
                            ui.label(
                                RichText::new("That usually means the cookies expired or were rotated.")
                                    .color(text_dim()),
                            );
                            ui.add_space(10.0);
                            if accent_button(ui, "Re-import cookies").clicked() {
                                reimport_from_empty = true;
                            }
                        } else {
                            ui.label(RichText::new("Nothing here.").color(text_dim()));
                        }
                    }
                });
                return;
            }

            let variant = match self.settings.thumbs {
                ThumbQuality::Standard => Variant::Standard,
                ThumbQuality::High => Variant::High,
                // 320 px source is fine up to ~1.2× upscaling
                ThumbQuality::Auto if card_w * ui.ctx().pixels_per_point() > 380.0 => Variant::High,
                ThumbQuality::Auto => Variant::Standard,
            };
            let video_rows = tab.videos.len().div_ceil(cols);
            let show_footer = !tab.exhausted || tab.error.is_some();
            let rows = video_rows + usize::from(show_footer);

            let mut scroll = egui::ScrollArea::vertical().auto_shrink(false);
            if std::mem::take(&mut tab.scroll_to_top) {
                scroll = scroll.vertical_scroll_offset(0.0);
            }
            ui.add_space(GAP * 0.5);
            ui.spacing_mut().item_spacing.y = GAP;
            scroll.show_rows(ui, card_h, rows, |ui, range| {
                // Infinite scroll: start fetching the next page when we're within
                // a screenful of the end, so it's usually there before you arrive.
                let lookahead = range.len().max(2);
                if range.end + lookahead >= video_rows && !tab.loading && !tab.exhausted && tab.error.is_none() {
                    more = true;
                }
                for row in range {
                    ui.horizontal(|ui| {
                        ui.add_space(GAP);
                        ui.spacing_mut().item_spacing.x = GAP;
                        if row < video_rows {
                            for v in tab.videos.iter().skip(row * cols).take(cols) {
                                let playing = playing_id.as_deref() == Some(v.id.as_str());
                                let progress = (!v.is_playlist).then(|| positions.get(&v.id)).flatten().copied();
                                let card = CardInfo { size: Vec2::new(card_w, card_h), thumb_h, playing, progress, m, in_watch_later: source == Source::WatchLater };
                                match video_card(ui, thumbs, variant, v, card) {
                                    CardAction::Open => clicked = Some(v.clone()),
                                    CardAction::Channel => open_channel = Some(v.clone()),
                                    CardAction::Quick(q) => quick = Some((q, v.clone())),
                                    CardAction::None => {}
                                }
                            }
                        } else {
                            // footer row: spinner while the next page loads; retry on error
                            ui.allocate_ui_with_layout(
                                Vec2::new(avail_w, card_h),
                                Layout::top_down(Align::Center),
                                |ui| {
                                    ui.add_space(12.0);
                                    if let Some(e) = tab.error.as_ref().filter(|_| !tab.loading) {
                                        error_box(ui, e);
                                        ui.add_space(8.0);
                                        if accent_button(ui, "Retry").clicked() {
                                            more = true;
                                        }
                                    } else {
                                        ui.spinner();
                                    }
                                },
                            );
                        }
                    });
                }
            });
        });

        // ----- apply actions -----
        if let Some(v) = clicked {
            if v.is_playlist {
                self.open_page(&ctx, Source::Playlist { id: v.id.clone(), title: v.title().to_string() });
            } else {
                self.play(&v);
            }
        }
        if let Some(v) = open_channel {
            if let Some(url) = v.channel_url.clone() {
                self.open_page(&ctx, Source::Channel { url, name: v.channel().to_string() });
            }
        }
        if more {
            self.load_more(&ctx, self.active.clone());
        }
        if let Some((q, v)) = quick {
            self.quick_action(&ctx, q, &v);
        }
        if let Some(q) = search {
            let s = Source::Search(q);
            self.tabs.remove(&s);
            if let Some(old) = self.last_search.replace(s.clone()) {
                if old != s {
                    self.tabs.remove(&old);
                }
            }
            self.nav_back.clear();
            self.active = s.clone();
            self.load(&ctx, s, true);
        }
        if let Some(t) = switch_to {
            self.nav_back.clear();
            self.active = t.clone();
            self.load(&ctx, t, false);
        }
        if refresh {
            self.load(&ctx, self.active.clone(), true);
        }
        if reimport_from_empty {
            self.import_error = None;
            self.browsers = yt::detect_browsers();
            self.sessions = signin::sessions();
            self.session_pick = 0;
            self.screen = Screen::Onboarding;
        }
    }
}

#[derive(PartialEq)]
enum GoogleAction {
    None,
    Start,
    CheckNow,
    Cancel,
}

impl App {
    fn quick_action(&mut self, ctx: &egui::Context, q: Quick, v: &Video) {
        let url = if v.is_playlist { format!("https://www.youtube.com/playlist?list={}", v.id) } else { v.url() };
        match q {
            Quick::CopyLink => {
                ctx.copy_text(url);
                self.status = Some(("Link copied.".into(), false));
            }
            Quick::OpenInBrowser => {
                if let Err(e) = open_url(&url) {
                    self.status = Some((format!("Couldn't open the browser: {e}"), true));
                }
            }
            Quick::Watched(true) => self.positions.mark_watched(&v.id, v.duration),
            Quick::Watched(false) => self.positions.forget(&v.id),
            Quick::WatchLater(add) => {
                let (tx, ctx, cookies, id, title) = (self.tx.clone(), ctx.clone(), self.cookies.clone(), v.id.clone(), v.title().to_string());
                std::thread::spawn(move || {
                    let r = yt::set_watch_later(&cookies, &id, add)
                        .map(|()| if add { format!("Saved to Watch later: {title}") } else { format!("Removed from Watch later: {title}") })
                        .map_err(|e| format!("Watch later: {e:#}"));
                    let _ = tx.send(Msg::WatchLater(add, id, r));
                    ctx.request_repaint();
                });
            }
        }
    }

    /// Go to a channel/playlist page, remembering where we came from.
    fn open_page(&mut self, ctx: &egui::Context, page: Source) {
        if self.active == page {
            return;
        }
        self.nav_back.push(std::mem::replace(&mut self.active, page.clone()));
        self.load(ctx, page, false);
    }

    /// Back / title / Videos-Playlists switch for channel and playlist pages.
    fn ui_page_header(&mut self, ui: &mut egui::Ui) {
        if !self.active.is_page() {
            return;
        }
        let ctx = ui.ctx().clone();
        let mut back = false;
        let mut switch: Option<Source> = None;
        egui::Panel::top("page")
            .frame(egui::Frame::new().fill(bg()).inner_margin(Margin { left: 16, right: 16, top: 12, bottom: 4 }))
            .show_separator_line(false)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    if ui.add(egui::Button::new(RichText::new("←  Back").size(15.0)).frame(false)).clicked() {
                        back = true;
                    }
                    ui.add_space(10.0);
                    let (kind, title) = match &self.active {
                        Source::Playlist { title, .. } => ("Playlist", title.clone()),
                        other => ("Channel", other.label()),
                    };
                    ui.label(RichText::new(kind).small().color(accent()));
                    ui.add(egui::Label::new(RichText::new(title).size(20.0).strong().color(text())).truncate());
                    if let Source::Channel { url, name } | Source::ChannelPlaylists { url, name } = &self.active {
                        let (url, name) = (url.clone(), name.clone());
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            let on_lists = matches!(self.active, Source::ChannelPlaylists { .. });
                            if tab_button(ui, "Playlists", on_lists).clicked() && !on_lists {
                                switch = Some(Source::ChannelPlaylists { url: url.clone(), name: name.clone() });
                            }
                            if tab_button(ui, "Videos", !on_lists).clicked() && on_lists {
                                switch = Some(Source::Channel { url, name });
                            }
                        });
                    }
                });
            });
        // Esc / mouse back button also go back
        let key_back = !ctx.egui_wants_keyboard_input()
            && ctx.input(|i| i.key_pressed(Key::Escape) || i.pointer.button_clicked(egui::PointerButton::Extra1));
        if back || key_back {
            if let Some(prev) = self.nav_back.pop() {
                self.active = prev;
            }
        }
        if let Some(s) = switch {
            self.active = s.clone(); // sibling view: replace, don't stack
            self.load(&ctx, s, false);
        }
    }

    /// Record playback progress (embedded: every frame, cheap; external: poll
    /// mpv over IPC every 5 s on a worker) and persist it periodically.
    fn track_positions(&mut self, ctx: &egui::Context) {
        if let Some(p) = self.embedded.as_ref().filter(|p| p.started) {
            self.positions.update(&p.id, p.pos, p.duration);
        }
        if let Some(p) = &self.player {
            if self.last_ext_poll.elapsed() >= Duration::from_secs(5) {
                self.last_ext_poll = std::time::Instant::now();
                let (tx, ctx, id) = (self.tx.clone(), ctx.clone(), p.id.clone());
                std::thread::spawn(move || {
                    if let Some((pos, dur)) = yt::ipc_progress() {
                        let _ = tx.send(Msg::ExternalProgress(id, pos, dur));
                        ctx.request_repaint();
                    }
                });
            }
        }
        self.positions.maybe_save();
    }

    // ---------- Google sign-in ----------

    fn ui_google_signin(&mut self, ui: &mut egui::Ui, busy: bool) -> GoogleAction {
        let mut act = GoogleAction::None;
        egui::Frame::new().fill(card()).corner_radius(RADIUS).inner_margin(Margin::same(16)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            if self.sessions.is_empty() {
                ui.label(RichText::new("Sign in with Google").size(16.0).strong().color(text()));
                ui.label(RichText::new("Needs Firefox or a Chromium-based browser (Chrome, Brave, Edge…).").color(text_dim()));
                return;
            }
            if let Some(s) = &self.signin {
                let b = s.session.browser;
                if !s.opened {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(RichText::new(format!("Checking {}…", s.session.label())).color(text()));
                    });
                    return;
                }
                ui.label(RichText::new(format!("Sign in to Google in {b}")).size(16.0).strong().color(text()));
                ui.label(
                    RichText::new(format!(
                        "We opened Google's sign-in page in {b}. Once you're signed in, ytmini picks the \
                         session up by itself — leave {b} open."
                    ))
                    .color(text_dim()),
                );
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(RichText::new("Waiting for sign-in…").color(text_dim()));
                    ui.add_space(8.0);
                    if accent_button(ui, "I've signed in").clicked() {
                        act = GoogleAction::CheckNow;
                    }
                    if ui.button("Cancel").clicked() {
                        act = GoogleAction::Cancel;
                    }
                });
                return;
            }
            let pick = self.session_pick.min(self.sessions.len() - 1);
            let chosen = &self.sessions[pick];
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(RichText::new("Sign in with Google").size(16.0).strong().color(text()));
                    let mut note = format!(
                        "Uses your {} session. If you're not signed in there yet, Google's sign-in page opens in it.",
                        chosen.browser
                    );
                    if !chosen.firefox && cfg!(target_os = "macos") {
                        note.push_str(" macOS asks for Keychain access once — click Allow.");
                    }
                    ui.add(egui::Label::new(RichText::new(note).size(12.5).color(text_dim())).wrap());
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.add_enabled(!busy, accent_btn("Sign in")).clicked() {
                        act = GoogleAction::Start;
                    }
                });
            });
            if self.sessions.len() > 1 {
                ui.add_space(6.0);
                let labels: Vec<String> = self.sessions.iter().map(|s| s.label()).collect();
                egui::ComboBox::from_id_salt("signin_profile")
                    .selected_text(labels[pick].clone())
                    .show_ui(ui, |ui| {
                        for (i, l) in labels.iter().enumerate() {
                            ui.selectable_value(&mut self.session_pick, i, l);
                        }
                    });
            }
        });
        act
    }

    /// Step 1: is that profile already signed in? (worker; names only, no prompts)
    fn start_signin(&mut self, ctx: &egui::Context, session: Session) {
        self.import_error = None;
        let now = std::time::Instant::now();
        self.signin = Some(SignIn { session, opened: false, checking: false, last_check: now, started: now });
        self.spawn_signin_check(ctx);
    }

    fn spawn_signin_check(&mut self, ctx: &egui::Context) {
        let Some(s) = &mut self.signin else { return };
        s.checking = true;
        s.last_check = std::time::Instant::now();
        let (tx, ctx, session) = (self.tx.clone(), ctx.clone(), s.session.clone());
        std::thread::spawn(move || {
            let _ = tx.send(Msg::SignInCheck(session.has_login().map_err(|e| format!("{e:#}"))));
            ctx.request_repaint();
        });
    }

    fn on_signin_check(&mut self, ctx: &egui::Context, r: Result<bool, String>) {
        let Some(s) = &mut self.signin else { return };
        s.checking = false;
        match r {
            Ok(true) => self.finish_signin(ctx),
            Ok(false) if !s.opened => match s.session.open_signin() {
                Ok(()) => s.opened = true,
                Err(e) => {
                    self.import_error = Some(format!("{e:#}"));
                    self.signin = None;
                }
            },
            Err(e) if !s.opened => {
                self.import_error = Some(e);
                self.signin = None;
            }
            _ => {} // still waiting; transient read errors are fine
        }
    }

    /// While the user signs in, re-check the browser's cookies every few seconds.
    fn poll_signin(&mut self, ctx: &egui::Context) {
        let Some(s) = &self.signin else { return };
        if !s.opened || s.checking {
            return;
        }
        if s.started.elapsed() > SIGNIN_TIMEOUT {
            self.signin = None;
            self.import_error = Some("Stopped waiting for the Google sign-in. Click Sign in to try again.".into());
            return;
        }
        if s.last_check.elapsed() >= SIGNIN_POLL {
            self.spawn_signin_check(ctx);
        }
        ctx.request_repaint_after(Duration::from_millis(500));
    }

    /// Step 2: the profile is signed in → import a snapshot of its cookies.
    fn finish_signin(&mut self, ctx: &egui::Context) {
        let Some(s) = self.signin.take() else { return };
        let Some(tools) = self.tools.clone() else { return };
        self.importing = Some(s.session.label());
        self.import_error = None;
        self.import_source = Some((s.session.id.to_string(), s.session.profile.clone()));
        let (tx, ctx, cookies) = (self.tx.clone(), ctx.clone(), self.cookies.clone());
        std::thread::spawn(move || {
            let r = s
                .session
                .snapshot_spec()
                .and_then(|spec| yt::import_cookies(&tools, &spec, &cookies))
                .map_err(|e| format!("{e:#}"));
            signin::clear_snapshot();
            let _ = tx.send(Msg::Imported(r));
            ctx.request_repaint();
        });
    }

    fn ui_player(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let fullscreen = ctx.input(|i| i.viewport().fullscreen.unwrap_or(false));
        let mut back = false;
        let mut toggle_fs = false;

        // Keyboard (only when no text field / popup has focus)
        if !ctx.egui_wants_keyboard_input() {
            let (space, left, right, up, down, f, esc) = ctx.input_mut(|i| {
                let k = |i: &mut egui::InputState, key| i.consume_key(egui::Modifiers::NONE, key);
                (k(i, Key::Space), k(i, Key::ArrowLeft), k(i, Key::ArrowRight), k(i, Key::ArrowUp), k(i, Key::ArrowDown), k(i, Key::F), k(i, Key::Escape))
            });
            if let Some(p) = &mut self.embedded {
                if space {
                    p.toggle_pause();
                    p.poke();
                }
                if left {
                    p.seek_relative(-5.0);
                }
                if right {
                    p.seek_relative(5.0);
                }
                if up || down {
                    p.set_volume(p.volume + if up { 5.0 } else { -5.0 });
                    p.poke();
                }
            }
            toggle_fs |= f;
            if esc {
                if fullscreen {
                    toggle_fs = true;
                } else {
                    back = true;
                }
            }
        }

        let mut quality = None;
        let mut open_in_window = None;
        if let (Some(p), Some(surface), Some(gl)) = (&mut self.embedded, &mut self.surface, frame.gl()) {
            let view = player::ViewCtx {
                gl,
                surface,
                fullscreen,
                quality: self.settings.quality,
                screen_cap: self.screen_cap,
                accent: accent(),
            };
            let act = p.ui(ui, view);
            back |= act.back;
            toggle_fs |= act.toggle_fullscreen;
            quality = act.quality;
            if act.open_in_window {
                open_in_window = Some(Video { id: p.id.clone(), title: Some(p.title.clone()), ..Default::default() });
            }
        }

        if toggle_fs {
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(!fullscreen));
        }
        if let Some(q) = quality {
            self.settings.quality = q;
            self.apply_profile_live(&ctx);
        }
        if back || open_in_window.is_some() {
            self.close_embedded(&ctx, frame);
        }
        if let Some(v) = open_in_window {
            self.play_in_window(&v);
        }
    }

    fn ui_settings(&mut self, ctx: &egui::Context) {
        let mut open = true;
        let mut clear = false;
        let mut reimport = false;
        let mut refresh_session = false;
        let mut check_updates = false;
        let before_quality = self.settings.quality;
        egui::Window::new("Settings")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(420.0)
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                let s = &mut self.settings;
                ui.add_space(4.0);
                section(ui, "Playback");
                egui::Grid::new("playback").num_columns(2).spacing([16.0, 10.0]).show(ui, |ui| {
                    ui.label("Player");
                    ui.horizontal(|ui| {
                        ui.selectable_value(&mut s.player, PlayerMode::Embedded, "In this window");
                        ui.selectable_value(&mut s.player, PlayerMode::Window, "Separate window");
                    });
                    ui.end_row();
                    ui.label("Quality");
                    egui::ComboBox::from_id_salt("q").selected_text(s.quality.label()).width(220.0).show_ui(ui, |ui| {
                        for q in yt::Quality::choices(self.screen_cap) {
                            ui.selectable_value(&mut s.quality, q, q.label());
                        }
                    });
                    ui.end_row();
                });
                ui.checkbox(&mut s.mark_watched, "Add videos I watch to my YouTube history");
                ui.label(
                    RichText::new(format!(
                        "Auto never exceeds what this screen can show ({}p), and drops to 1080p30 on battery, 720p30 in Low Power Mode.",
                        self.screen_cap
                    ))
                    .small()
                    .color(text_dim()),
                );

                ui.add_space(10.0);
                section(ui, "Layout");
                ui.horizontal(|ui| {
                    ui.label("Grid");
                    ui.selectable_value(&mut s.density, Density::Comfortable, "Comfortable");
                    ui.selectable_value(&mut s.density, Density::Compact, "Compact");
                });
                ui.checkbox(&mut s.sidebar_collapsed, "Sidebar: icons only");
                ui.add_space(10.0);
                section(ui, "Thumbnails");
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut s.thumbs, ThumbQuality::Auto, "Auto");
                    ui.selectable_value(&mut s.thumbs, ThumbQuality::Standard, "Standard · 320p");
                    ui.selectable_value(&mut s.thumbs, ThumbQuality::High, "High · 640p");
                });
                ui.label(RichText::new("Standard ≈ 14 KB, high ≈ 60 KB each on disk. Auto picks high on sharp (Retina) screens.").small().color(text_dim()));

                ui.add_space(10.0);
                section(ui, "Disk cache");
                egui::Grid::new("cache").num_columns(2).spacing([16.0, 10.0]).show(ui, |ui| {
                    ui.label("Size limit");
                    ui.add(
                        egui::Slider::new(&mut s.cache_mb, 32..=4096)
                            .logarithmic(true)
                            .suffix(" MB")
                            .text(""),
                    );
                    ui.end_row();
                    ui.label("Keep for");
                    ui.add(egui::Slider::new(&mut s.cache_days, 1..=365).logarithmic(true).suffix(" days"));
                    ui.end_row();
                    ui.label("In use");
                    ui.horizontal(|ui| {
                        let used = self.cache_usage.map_or("…".into(), |b| format!("{:.1} MB", b as f64 / 1_048_576.0));
                        ui.label(used);
                        if ui.small_button("Clear").clicked() {
                            clear = true;
                        }
                    });
                    ui.end_row();
                });
                ui.label(
                    RichText::new(format!(
                        "Recommended: {} MB, {} days — about 4,000 high-quality or 18,000 standard thumbnails, plus your last feed/subscriptions for instant startup.",
                        crate::settings::DEFAULT_CACHE_MB,
                        crate::settings::DEFAULT_CACHE_DAYS
                    ))
                    .small()
                    .color(text_dim()),
                );

                ui.add_space(10.0);
                if self.tray.is_some() {
                    section(ui, "Window");
                    ui.checkbox(&mut s.close_to_tray, "Closing the window keeps ytmini in the menu bar");
                    ui.label(RichText::new("A playing video continues as audio only. Quit from the menu-bar icon or with ⌘Q.").small().color(text_dim()));
                    ui.add_space(10.0);
                }
                section(ui, "Components");
                egui::Grid::new("versions").num_columns(2).spacing([16.0, 6.0]).show(ui, |ui| {
                    for (k, v) in &self.deps.versions {
                        ui.label(k);
                        ui.label(RichText::new(v).color(text_dim()));
                        ui.end_row();
                    }
                });
                ui.horizontal(|ui| {
                    let managed = self.tools.as_ref().is_some_and(|t| t.ytdlp_managed);
                    if self.deps.busy {
                        ui.spinner();
                        ui.label(RichText::new(self.deps.activity.clone().unwrap_or_else(|| "Checking…".into())).small());
                    } else if managed && ui.button("Check for yt-dlp update").clicked() {
                        check_updates = true;
                    }
                });
                ui.add_space(10.0);
                section(ui, "Account");
                ui.horizontal(|ui| {
                    if let Some(src) = &self.settings.signin_source {
                        let label = signin::find(&src.id, &src.profile).map(|s| s.label());
                        if let Some(label) = label {
                            if ui.button("Refresh session").on_hover_text(format!("Re-import from {label}")).clicked() {
                                refresh_session = true;
                            }
                        }
                    }
                    if ui.button("Sign in again…").clicked() {
                        reimport = true;
                    }
                });
            });
        if !open || reimport || refresh_session {
            self.show_settings = false;
        }
        if refresh_session {
            let src = self.settings.signin_source.clone();
            if let Some(s) = src.and_then(|src| signin::find(&src.id, &src.profile)) {
                self.import_error = None;
                self.browsers = yt::detect_browsers();
                self.sessions = signin::sessions();
                self.session_pick = self.sessions.iter().position(|x| x == &s).unwrap_or(0);
                self.screen = Screen::Onboarding;
                self.start_signin(ctx, s);
            }
        }
        if self.settings.quality != before_quality {
            self.apply_profile_live(ctx);
        }
        if check_updates {
            self.run_deps_job(ctx, vec![Dep::YtDlp], true);
        }
        if clear {
            cache::clear();
            self.thumbs.clear();
            self.cache_usage = Some(0);
        }
        if reimport {
            self.import_error = None;
            self.browsers = yt::detect_browsers();
            self.sessions = signin::sessions();
            self.session_pick = 0;
            self.screen = Screen::Onboarding;
        }
    }
}

impl App {
    fn hide_to_tray(&mut self, ctx: &egui::Context) {
        self.hidden = true;
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        tray::set_dock_visible(false);
        if let Some(p) = &mut self.embedded {
            p.set_background(true);
        }
    }

    fn show_from_tray(&mut self, ctx: &egui::Context) {
        self.hidden = false;
        tray::set_dock_visible(true);
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        if let Some(p) = &mut self.embedded {
            p.set_background(false);
        }
    }

    /// Menu-bar icon actions. Runs in `logic`, so it works while hidden.
    fn handle_tray(&mut self, ctx: &egui::Context) {
        while let Ok(ev) = self.tray_rx.try_recv() {
            match ev {
                TrayEvent::Show => self.show_from_tray(ctx),
                TrayEvent::PlayPause => {
                    if let Some(p) = &mut self.embedded {
                        p.toggle_pause();
                    }
                }
                TrayEvent::Quit => {
                    self.quitting = true;
                    if self.hidden {
                        // a hidden window may not process Close on every platform
                        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    }
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }
        if let Some(t) = &mut self.tray {
            t.set_playing(self.embedded.as_ref().map(|p| p.paused));
        }
    }

    fn drive_smoke(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        let Some(mut sm) = self.smoke.take() else { return };
        ctx.request_repaint_after(Duration::from_millis(100));
        let t = sm.since.elapsed().as_secs_f64();
        let started = self.embedded.as_ref().is_some_and(|p| p.started && !p.buffering);
        let pos = self.embedded.as_ref().map_or(0.0, |p| p.pos);
        if let Some(err) = self.embedded.as_ref().and_then(|p| p.error.clone()) {
            sm.log(false, &format!("player error: {err}"));
            self.quitting = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return; // smoke stays None: stop driving
        }
        let next = |sm: &mut Smoke| {
            sm.step += 1;
            sm.since = std::time::Instant::now();
        };
        match sm.step {
            0 if started && t > 3.0 => {
                sm.log(self.embedded.is_some(), "embedded playback started");
                if let Some(p) = &self.embedded {
                    p.seek_absolute(60.0, true);
                }
                next(&mut sm);
            }
            1 if started && t > 3.0 => {
                sm.log(pos >= 60.0, &format!("seek to 60s (pos {pos:.1})"));
                eprintln!("[smoke] MARK before-back");
                self.close_embedded(ctx, frame);
                next(&mut sm);
            }
            2 if t > 3.0 => {
                sm.log(self.embedded.is_none(), "back to grid, player torn down");
                eprintln!("[smoke] MARK after-back");
                let v = sm.video.clone();
                self.play(&v);
                next(&mut sm);
            }
            3 if started && t > 3.0 => {
                sm.log(pos > 50.0, &format!("second playback resumed where we left off ({pos:.1}s)"));
                self.settings.quality = yt::Quality::Max(480);
                self.apply_profile_live(ctx);
                next(&mut sm);
            }
            4 if started && t > 4.0 => {
                let h = self.embedded.as_ref().and_then(Embedded::height).unwrap_or(0.0);
                sm.log(h > 0.0 && h <= 480.0, &format!("live switch to 480p (height {h})"));
                if self.tray.is_some() {
                    sm.mark = pos;
                    self.hide_to_tray(ctx);
                    next(&mut sm);
                } else {
                    sm.step = 6;
                }
            }
            5 if t > 5.0 => {
                let h = self.embedded.as_ref().and_then(Embedded::height);
                sm.log(pos > sm.mark + 3.0, &format!("keeps playing while hidden ({:.1}s → {pos:.1}s)", sm.mark));
                sm.log(h.is_none(), &format!("audio only while hidden (video height {h:?})"));
                self.show_from_tray(ctx);
                next(&mut sm);
            }
            6 if t > 3.0 => {
                let h = self.embedded.as_ref().and_then(Embedded::height).unwrap_or(0.0);
                sm.log(h > 0.0, &format!("video back after showing window (height {h})"));
                eprintln!("[smoke] quitting during playback; {}", if sm.failed { "SOME FAILED" } else { "ALL PASSED" });
                self.quitting = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                next(&mut sm);
            }
            0..=6 if t > 40.0 => {
                sm.log(false, &format!("timed out at step {}", sm.step));
                self.quitting = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                sm.step = 99;
            }
            _ => {}
        }
        self.smoke = Some(sm);
    }
}

fn section(ui: &mut egui::Ui, title: &str) {
    ui.label(RichText::new(title).strong().size(15.0).color(accent()));
    ui.add_space(2.0);
}

fn ago(unix: u64) -> String {
    let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let d = now.saturating_sub(unix);
    match d {
        0..=89 => "just now".into(),
        90..=5399 => format!("{} min ago", d / 60),
        5400..=129_599 => format!("{} h ago", d / 3600),
        _ => format!("{} days ago", d / 86_400),
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        crate::theme::update(ctx);
        self.drive_smoke(ctx, frame);
        self.handle_messages(ctx);
        self.thumbs.poll(ctx);
        self.poll_player();
        self.update_screen_cap(ctx);
        self.handle_tray(ctx);
        self.track_positions(ctx);
        self.poll_signin(ctx);
        if let Some(e) = &mut self.embedded {
            e.poll_events();
            if std::mem::take(&mut e.title_changed) {
                ctx.send_viewport_cmd(egui::ViewportCommand::Title(format!("{} — ytmini", e.title)));
            }
        }
        // (the smoke test changes settings; never persist those)
        if self.smoke.is_none() && self.settings != self.saved_settings {
            if (self.settings.cache_mb, self.settings.cache_days)
                != (self.saved_settings.cache_mb, self.saved_settings.cache_days)
            {
                self.limits.set(self.settings.cache_mb, self.settings.cache_days);
                self.enforce_cache(ctx);
            }
            self.settings.save();
            self.saved_settings = self.settings.clone();
        }
        if self.player.is_some() {
            // Nobody else wakes us when mpv exits; check twice a second.
            ctx.request_repaint_after(Duration::from_millis(500));
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        // Red close button → hide to the menu bar (Cmd+Q / tray "Quit" really quit).
        if ctx.input(|i| i.viewport().close_requested())
            && self.tray.is_some()
            && self.settings.close_to_tray
            && !self.quitting
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.hide_to_tray(&ctx);
        }
        if self.pending_play.is_some() {
            self.start_embedded(&ctx, frame);
        }
        egui::Frame::new().fill(bg()).show(ui, |ui| {
            ui.set_min_size(ui.available_size());
            match &self.screen {
                Screen::MissingTools(m) => {
                    let m = m.clone();
                    self.ui_missing(ui, &m);
                }
                Screen::Setup => self.ui_setup(ui),
                Screen::Onboarding => self.ui_onboarding(ui),
                Screen::Main if self.embedded.is_some() => self.ui_player(ui, frame),
                Screen::Main => self.ui_main(ui),
            }
        });
        if self.show_settings {
            self.ui_settings(&ctx);
        }
    }

    fn on_exit(&mut self, gl: Option<&eframe::glow::Context>) {
        if let Some(p) = self.embedded.take() {
            self.settings.volume = p.volume;
            if p.started {
                self.positions.update(&p.id, p.pos, p.duration);
            }
            p.close_now(); // GL still current here; nothing outlives the context
        }
        if let (Some(s), Some(gl)) = (&self.surface, gl) {
            s.destroy(gl);
        }
        self.stop_player();
        self.positions.save();
        if self.smoke.is_none() && self.settings != self.saved_settings {
            self.settings.save();
        }
    }
}

// ---------- widgets ----------

struct CardInfo {
    size: Vec2,
    thumb_h: f32,
    playing: bool,
    progress: Option<crate::positions::Entry>,
    m: Metrics,
    in_watch_later: bool,
}

fn video_card(ui: &mut egui::Ui, thumbs: &mut Thumbs, variant: Variant, v: &Video, info: CardInfo) -> CardAction {
    let CardInfo { size, thumb_h, playing, progress, m, in_watch_later } = info;
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    if !ui.is_rect_visible(rect) {
        return CardAction::None;
    }
    let ctx = ui.ctx().clone();
    let hover = ctx.animate_bool_with_time(resp.id, resp.hovered() || resp.context_menu_opened(), 0.12);
    let painter = ui.painter_at(rect.expand(2.0));
    let r = CornerRadius::same(RADIUS);
    painter.rect_filled(rect, r, lerp_color(card(), card_hover(), hover));

    // thumbnail (fades in when it arrives; already-cached ones show at once)
    let thumb = egui::Rect::from_min_size(rect.min, Vec2::new(rect.width(), thumb_h));
    let tr = CornerRadius { nw: RADIUS, ne: RADIUS, sw: 0, se: 0 };
    painter.rect_filled(thumb, tr, thumb_bg());
    let thumb_video = if v.is_playlist { v.thumb_id.as_deref() } else { Some(v.id.as_str()) };
    let watched = progress.is_some_and(|p| p.watched());
    let tex = thumb_video.and_then(|id| thumbs.get(id, variant));
    let shown = ctx.animate_bool_with_time(resp.id.with("thumb"), tex.is_some(), 0.25);
    if let Some(tex) = tex {
        let base = if watched { Color32::from_gray(140) } else { Color32::WHITE };
        egui::Image::from_texture(egui::load::SizedTexture::from_handle(tex))
            .corner_radius(tr)
            .tint(base.gamma_multiply(shown))
            .paint_at(ui, thumb);
    }

    // watch progress: thin accent bar along the bottom of the thumbnail
    if let Some(f) = progress.and_then(|p| p.fraction()).filter(|f| *f > 0.01) {
        let track = egui::Rect::from_min_max(egui::pos2(thumb.left(), thumb.bottom() - 3.0), thumb.right_bottom());
        painter.rect_filled(track, 0, Color32::from_white_alpha(70));
        let done = egui::Rect::from_min_max(track.min, egui::pos2(track.left() + track.width() * f, track.bottom()));
        painter.rect_filled(done, 0, accent());
    }

    // duration / playlist badge
    let dur = if v.is_playlist {
        "PLAYLIST".to_string()
    } else if watched {
        format!("✓ {}", v.duration_label())
    } else {
        v.duration_label()
    };
    let live = dur == "LIVE";
    let g = painter.layout_no_wrap(dur, FontId::proportional(11.5), Color32::WHITE);
    let badge = egui::Rect::from_min_size(
        thumb.right_bottom() - g.size() - Vec2::new(12.0, 10.0),
        g.size() + Vec2::new(8.0, 4.0),
    );
    painter.rect_filled(badge, 4, if live { Color32::from_rgb(0xE5, 0x2D, 0x27) } else { Color32::from_black_alpha(190) });
    painter.galley(badge.min + Vec2::new(4.0, 2.0), g, Color32::WHITE);

    // title (max 2 lines)
    let text_w = rect.width() - 2.0 * m.pad;
    let mut job = LayoutJob::single_section(
        v.title().to_string(),
        TextFormat { font_id: FontId::proportional(m.title), color: text(), ..Default::default() },
    );
    job.wrap.max_width = text_w;
    job.wrap.max_rows = 2;
    job.wrap.break_anywhere = false;
    job.wrap.overflow_character = Some('…');
    let title = painter.layout_job(job);
    let title_pos = egui::pos2(rect.min.x + m.pad, thumb.max.y + m.pad);
    painter.galley(title_pos, title, text());

    // channel · views
    let views = v.views_label().map(|s| format!("  ·  {s}")).unwrap_or_default();
    let views_g = painter.layout_no_wrap(views, FontId::proportional(m.meta), text_dim());
    let mut cjob = LayoutJob::single_section(
        v.channel().to_string(),
        TextFormat { font_id: FontId::proportional(m.meta), color: text_dim(), ..Default::default() },
    );
    cjob.wrap.max_width = (text_w - views_g.size().x).max(40.0);
    cjob.wrap.max_rows = 1;
    cjob.wrap.overflow_character = Some('…');
    let chan = painter.layout_job(cjob);
    let chan_rect = egui::Rect::from_min_size(egui::pos2(title_pos.x, rect.max.y - m.pad - chan.size().y), chan.size());
    // The channel name is its own link (when we know the channel's URL).
    let chan_resp = (v.channel_url.is_some() && !v.channel().is_empty())
        .then(|| ui.interact(chan_rect.expand(2.0), resp.id.with("channel"), Sense::click()));
    let chan_hover = chan_resp.as_ref().is_some_and(|r| r.hovered());
    painter.galley(chan_rect.min, chan, if chan_hover { text() } else { text_dim() });
    painter.galley(egui::pos2(chan_rect.right(), chan_rect.top()), views_g, text_dim());
    if chan_hover {
        painter.line_segment([chan_rect.left_bottom(), chan_rect.right_bottom()], Stroke::new(1.0, text_dim()));
    }

    // outline when playing
    if playing {
        painter.rect_stroke(rect, r, Stroke::new(2.0, accent()), StrokeKind::Outside);
    }

    // ⋯ menu button on hover, top-right of the thumbnail
    let mut action = CardAction::None;
    let menu_rect = egui::Rect::from_center_size(thumb.right_top() + Vec2::new(-20.0, 20.0), Vec2::splat(28.0));
    let menu_resp = ui.interact(menu_rect, resp.id.with("menu"), Sense::click());
    let menu_open = egui::Popup::is_id_open(&ctx, egui::Popup::default_response_id(&menu_resp));
    if hover > 0.0 || menu_open {
        let a = if menu_open { 1.0 } else { hover };
        let fill = Color32::from_black_alpha(if menu_resp.hovered() { 210 } else { 150 }).gamma_multiply(a);
        painter.circle_filled(menu_rect.center(), 13.0, fill);
        painter.text(menu_rect.center(), Align2::CENTER_CENTER, "⋯", FontId::proportional(16.0), Color32::WHITE.gamma_multiply(a));
    }
    let menu = |ui: &mut egui::Ui, action: &mut CardAction| {
        ui.set_min_width(190.0);
        if !v.is_playlist {
            if in_watch_later {
                if ui.button("Remove from Watch later").clicked() {
                    *action = CardAction::Quick(Quick::WatchLater(false));
                }
            } else if ui.button("Save to Watch later").clicked() {
                *action = CardAction::Quick(Quick::WatchLater(true));
            }
            let (label, to) = if watched { ("Mark as unwatched", false) } else { ("Mark as watched", true) };
            if ui.button(label).clicked() {
                *action = CardAction::Quick(Quick::Watched(to));
            }
            ui.separator();
        }
        if v.channel_url.is_some() && ui.button("Go to channel").clicked() {
            *action = CardAction::Channel;
        }
        if ui.button("Copy link").clicked() {
            *action = CardAction::Quick(Quick::CopyLink);
        }
        if ui.button("Open in browser").clicked() {
            *action = CardAction::Quick(Quick::OpenInBrowser);
        }
    };
    egui::Popup::menu(&menu_resp).show(|ui| menu(ui, &mut action));
    resp.context_menu(|ui| menu(ui, &mut action));
    if !matches!(action, CardAction::None) {
        return action;
    }

    if let Some(cr) = chan_resp {
        if cr.clicked() {
            return CardAction::Channel;
        }
        if cr.hovered() {
            ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
        }
    }
    if menu_resp.hovered() {
        ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
        return CardAction::None;
    }
    // details on hover: full title, channel, views, description snippet
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_ui(|ui| {
        ui.set_max_width(340.0);
        ui.label(RichText::new(v.title()).strong().color(text()));
        let mut meta = v.channel().to_string();
        if let Some(vl) = v.views_label() {
            meta = if meta.is_empty() { vl } else { format!("{meta} · {vl}") };
        }
        if let Some(p) = progress.filter(|p| p.dur > 0.0 && !p.watched()) {
            meta.push_str(&format!(" · {} left", fmt_secs(p.dur - p.pos)));
        }
        if !meta.is_empty() {
            ui.label(RichText::new(meta).small().color(text_dim()));
        }
        if let Some(d) = v.description.as_deref().filter(|d| !d.trim().is_empty()) {
            ui.add_space(2.0);
            ui.label(RichText::new(d.trim()).size(12.0).color(text_dim()));
        }
    });
    if resp.clicked() {
        CardAction::Open
    } else {
        CardAction::None
    }
}

fn fmt_secs(s: f64) -> String {
    let s = s.max(0.0).round() as u64;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

/// Grey placeholder cards with a gentle pulse while the first page loads.
fn skeleton_grid(ui: &mut egui::Ui, cols: usize, size: Vec2, thumb_h: f32) {
    let t = ui.input(|i| i.time) as f32;
    let pulse = 0.55 + 0.45 * (t * 2.6).sin().abs();
    let rows = ((ui.available_height() / (size.y + GAP)).ceil() as usize).max(1);
    let painter = ui.painter().clone();
    let origin = ui.max_rect().min + Vec2::new(GAP, GAP * 0.5);
    let fill = card().gamma_multiply(pulse.max(0.4));
    let bar = card_hover().gamma_multiply(pulse);
    for row in 0..rows {
        for col in 0..cols {
            let min = origin + Vec2::new(col as f32 * (size.x + GAP), row as f32 * (size.y + GAP));
            let r = egui::Rect::from_min_size(min, size);
            painter.rect_filled(r, RADIUS, fill);
            let th = egui::Rect::from_min_size(min, Vec2::new(size.x, thumb_h));
            painter.rect_filled(th, CornerRadius { nw: RADIUS, ne: RADIUS, sw: 0, se: 0 }, bar);
            let l1 = egui::Rect::from_min_size(min + Vec2::new(10.0, thumb_h + 12.0), Vec2::new(size.x * 0.8, 10.0));
            let l2 = egui::Rect::from_min_size(min + Vec2::new(10.0, thumb_h + 30.0), Vec2::new(size.x * 0.45, 9.0));
            painter.rect_filled(l1, 4, bar);
            painter.rect_filled(l2, 4, bar);
        }
    }
    ui.ctx().request_repaint();
}

/// Open a URL in the default browser.
fn open_url(url: &str) -> std::io::Result<()> {
    let mut c = if cfg!(target_os = "macos") {
        std::process::Command::new("open")
    } else if cfg!(windows) {
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", "start", ""]);
        c
    } else {
        std::process::Command::new("xdg-open")
    };
    let mut child = c.arg(url).spawn()?;
    std::thread::spawn(move || child.wait());
    Ok(())
}

fn tab_button(ui: &mut egui::Ui, label: &str, selected: bool) -> egui::Response {
    let text = RichText::new(label).size(15.0).color(if selected { text() } else { text_dim() });
    let btn = egui::Button::new(text)
        .fill(if selected { card_hover() } else { Color32::TRANSPARENT })
        .stroke(Stroke::NONE)
        .corner_radius(8)
        .min_size(Vec2::new(0.0, 30.0));
    let resp = ui.add(btn);
    if selected {
        let r = resp.rect;
        ui.painter().line_segment(
            [egui::pos2(r.left() + 10.0, r.bottom() + 3.0), egui::pos2(r.right() - 10.0, r.bottom() + 3.0)],
            Stroke::new(2.0, accent()),
        );
    }
    resp
}

#[derive(Clone, Copy)]
enum Icon {
    Home,
    Subs,
    Later,
    History,
    Lists,
    Search,
    Gear,
    Collapse,
    Expand,
}

fn nav_icon(s: &Source) -> Icon {
    match s {
        Source::Feed => Icon::Home,
        Source::Subscriptions => Icon::Subs,
        Source::WatchLater => Icon::Later,
        Source::History => Icon::History,
        Source::Playlists => Icon::Lists,
        _ => Icon::Search,
    }
}

/// Small line icons drawn with the painter (no icon font needed). `c` = centre, ~16 pt box.
fn paint_icon(p: &egui::Painter, icon: Icon, c: egui::Pos2, color: Color32) {
    use egui::pos2;
    let st = Stroke::new(1.5, color);
    let v = |x: f32, y: f32| c + Vec2::new(x, y);
    match icon {
        Icon::Home => {
            p.line(vec![v(-7.0, -0.5), v(0.0, -7.0), v(7.0, -0.5)], st);
            p.line(vec![v(-5.0, -2.0), v(-5.0, 6.5), v(5.0, 6.5), v(5.0, -2.0)], st);
            p.line(vec![v(-1.8, 6.5), v(-1.8, 2.0), v(1.8, 2.0), v(1.8, 6.5)], st);
        }
        Icon::Subs => {
            p.rect_stroke(egui::Rect::from_center_size(c + Vec2::new(0.0, 1.0), Vec2::new(15.0, 11.0)), 2.5, st, StrokeKind::Middle);
            p.line_segment([v(-4.5, -7.0), v(4.5, -7.0)], st);
            p.add(egui::Shape::convex_polygon(vec![v(-2.0, -2.0), v(3.5, 1.0), v(-2.0, 4.0)], color, Stroke::NONE));
        }
        Icon::Later => {
            p.circle_stroke(c, 7.0, st);
            p.line(vec![v(0.0, -4.0), v(0.0, 0.0), v(3.0, 2.0)], st);
        }
        Icon::History => {
            // counter-clockwise arrow around a clock hand: "earlier"
            let pts: Vec<egui::Pos2> = (0..=24)
                .map(|i| {
                    let a = (-150.0 + i as f32 * 12.5).to_radians();
                    c + Vec2::angled(a) * 7.0
                })
                .collect();
            p.line(pts, st);
            let tip = c + Vec2::angled((-150.0f32).to_radians()) * 7.0;
            p.add(egui::Shape::convex_polygon(vec![tip + Vec2::new(-3.5, -1.0), tip + Vec2::new(2.5, -2.5), tip + Vec2::new(0.5, 3.0)], color, Stroke::NONE));
            p.line(vec![v(0.0, -3.5), v(0.0, 0.0), v(2.5, 2.0)], st);
        }
        Icon::Lists => {
            for y in [-4.5, 0.0, 4.5] {
                p.circle_filled(v(-6.0, y), 1.1, color);
                p.line_segment([v(-3.0, y), v(7.0, y)], st);
            }
        }
        Icon::Search => {
            p.circle_stroke(v(-1.5, -1.5), 5.0, st);
            p.line_segment([v(2.2, 2.2), v(6.5, 6.5)], Stroke::new(2.0, color));
        }
        Icon::Gear => {
            for i in 0..8 {
                let a = i as f32 * std::f32::consts::TAU / 8.0;
                let d = Vec2::angled(a);
                p.line_segment([c + d * 5.5, c + d * 8.0], Stroke::new(2.4, color));
            }
            p.circle_stroke(c, 5.5, st);
            p.circle_stroke(c, 2.0, st);
        }
        Icon::Collapse | Icon::Expand => {
            let s = if matches!(icon, Icon::Collapse) { 1.0 } else { -1.0 };
            for dx in [-2.5, 2.5] {
                p.line(vec![pos2(c.x + dx + 2.5 * s, c.y - 5.0), pos2(c.x + dx - 2.5 * s, c.y), pos2(c.x + dx + 2.5 * s, c.y + 5.0)], st);
            }
        }
    }
}

/// A sidebar row: icon + label, rounded highlight when selected (macOS source-list style).
fn nav_item(ui: &mut egui::Ui, icon: Icon, label: &str, selected: bool, collapsed: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 30.0), Sense::click());
    let hover = ui.ctx().animate_bool_with_time(resp.id, resp.hovered(), 0.12);
    // macOS source-list style: neutral highlight, accent-coloured icon
    let sel = if crate::theme::pal().dark { Color32::from_white_alpha(22) } else { Color32::from_black_alpha(18) };
    let fill = if selected { sel } else { sel.gamma_multiply(0.6 * hover) };
    ui.painter().rect_filled(rect, 6, fill);
    let icon_color = if selected { accent() } else { text_dim() };
    let icon_x = if collapsed { rect.center().x } else { rect.left() + 18.0 };
    paint_icon(ui.painter(), icon, egui::pos2(icon_x, rect.center().y), icon_color);
    if !collapsed {
        let g = ui.painter().layout(
            label.to_string(),
            FontId::proportional(14.0),
            text(),
            rect.width() - 46.0,
        );
        let g = if g.rows.len() > 1 {
            let mut job = LayoutJob::single_section(label.to_string(), TextFormat { font_id: FontId::proportional(14.0), color: text(), ..Default::default() });
            job.wrap.max_width = rect.width() - 46.0;
            job.wrap.max_rows = 1;
            job.wrap.overflow_character = Some('…');
            ui.painter().layout_job(job)
        } else {
            g
        };
        ui.painter().galley(egui::pos2(rect.left() + 36.0, rect.center().y - g.size().y / 2.0), g, text());
    }
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    if collapsed { resp.on_hover_text(label) } else { resp }
}

fn accent_btn(label: &str) -> egui::Button<'_> {
    egui::Button::new(RichText::new(label).strong().color(on_accent()))
        .fill(accent())
        .corner_radius(8)
        .min_size(Vec2::new(72.0, 30.0))
}

fn accent_button(ui: &mut egui::Ui, label: &str) -> egui::Response {
    ui.add(accent_btn(label))
}

fn error_box(ui: &mut egui::Ui, err: &str) {
    egui::Frame::new()
        .fill(err_bg())
        .corner_radius(RADIUS)
        .inner_margin(Margin::same(12))
        .show(ui, |ui| {
            ui.add(egui::Label::new(RichText::new(err).size(13.0).color(crate::theme::err())).wrap());
        });
}

fn code_box(ui: &mut egui::Ui, text: &str) {
    egui::Frame::new().fill(card()).corner_radius(8).inner_margin(Margin::same(10)).show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label(RichText::new(text).monospace().color(crate::theme::text()));
            if ui.small_button("Copy").clicked() {
                ui.ctx().copy_text(text.to_string());
            }
        });
    });
}

/// A fixed-width column centred horizontally, with some top margin.
fn centered_column(ui: &mut egui::Ui, width: f32, add: impl FnOnce(&mut egui::Ui)) {
    let w = width.min(ui.available_width() - 32.0);
    ui.vertical_centered(|ui| {
        ui.add_space(48.0);
        ui.allocate_ui_with_layout(Vec2::new(w, 0.0), Layout::top_down(Align::Min), add);
    });
}

fn lerp_color(a: Color32, b: Color32, t: f32) -> Color32 {
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()))
}

