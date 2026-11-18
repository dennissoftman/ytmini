//! The eframe app: all UI state and drawing. Slow work (yt-dlp) runs on worker
//! threads and reports back over an mpsc channel.

use crate::cache::{self, Limits};
use crate::deps::{self, Dep};
use crate::i18n::{t, tf, tn};
use crate::mpv;
use crate::player::{self, Embedded, Surface};
use crate::positions::Positions;
use crate::settings::{Density, PlayerMode, Settings, ThumbQuality};
use crate::signin::{self, Session};
use crate::theme::{
    accent, bg, card, card_hover, err_bg, on_accent, panel, sidebar, text, text_dim, thumb_bg,
    RADIUS,
};
use crate::thumbs::{Thumbs, Variant};
use crate::tray::{self, Tray, TrayEvent};
use crate::yt::{self, Source, Tools, Video};
use eframe::egui::{
    self, text::LayoutJob, Align, Align2, Color32, CornerRadius, FontId, Key, Layout, Margin,
    RichText, Sense, Stroke, StrokeKind, TextFormat, Vec2,
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
        Density::Compact => Metrics {
            min_w: 200.0,
            pad: 8.0,
            title: 13.0,
            meta: 11.5,
        },
        Density::Comfortable => Metrics {
            min_w: 260.0,
            pad: 10.0,
            title: 14.5,
            meta: 12.5,
        },
    }
}

/// Hover a card this long and its stream gets resolved in the background.
const HOVER_PREPARE: Duration = Duration::from_millis(350);
/// Resolved stream URLs last ~6 h; use them only while clearly fresh.
/// Stream URLs resolved ahead are kept this long (YouTube's expire after
/// ~6 h, and only work from the network they were resolved on).
const PREPARED_TTL: Duration = Duration::from_secs(3 * 3600);
/// How often the network route is checked (a single local syscall).
const NET_CHECK: Duration = Duration::from_secs(15);
/// How long a click waits for a running resolve before starting its own.
const RESOLVE_WAIT_MAX: Duration = Duration::from_secs(8);

const BASE_TABS: [Source; 7] = [
    Source::Feed,
    Source::Subscriptions,
    Source::Music,
    Source::WatchLater,
    Source::History,
    Source::Playlists,
    Source::Downloads,
];

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
    Related(String, Vec<Video>),
    /// YouTube Music says this video is a song.
    Song(String),
    /// The EQ preset for a song's genre (None: genre unknown).
    Genre(String, Option<crate::eq::Preset>),
    /// Rating, channel, subscription and comments token of a video.
    Watch(String, crate::innertube::WatchInfo),
    /// A page of comments for a video (`append`: a later page).
    Comments(String, Result<crate::innertube::CommentsPage, String>, bool),
    /// The video's description (from the player endpoint).
    Description(String, String),
    /// Your rating didn't reach YouTube: back to this.
    Rated(String, crate::innertube::Like),
    /// Subscription state of a channel (a failed change rolled back).
    Subscribed(String, bool),
    /// Your playlists for "Save to playlist", for a video.
    Playlists(
        String,
        Result<Vec<crate::innertube::PlaylistChoice>, String>,
    ),
    /// Suggestions couldn't be loaded after a few tries.
    RelatedFailed(String, String),
    TopicShelf(String, String, Vec<Video>),
    Chapters(String, Vec<crate::chapters::Chapter>),
    /// Videos streamed in for a tab. `first`: part of the first page of this
    /// generation; `batch_done`: the requested page is complete.
    Chunk {
        source: Source,
        generation: u64,
        videos: Vec<Video>,
        first: bool,
        batch_done: bool,
        exhausted: bool,
    },
    FetchFailed {
        source: Source,
        generation: u64,
        error: String,
    },
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
    /// A message for the status bar (text, is_error).
    Status(String, bool),
    /// SponsorBlock segments for a video id.
    Segments(String, Vec<crate::sponsorblock::Segment>),
    Download(crate::downloads::Progress),
    /// Hover-prepare finished: id, format, result.
    Prepared(String, String, Result<yt::Direct, String>),
    Storyboard(String, crate::storyboard::Storyboard),
    DeArrow(String, crate::dearrow::Branding),
    /// Thumbnail for Now Playing, fetched for a video that wasn't cached.
    NowPlayingArt(String, Vec<u8>),
    /// Silent session refresh finished: (browser label, browser id, profile) or error.
    SessionRefreshed(Result<(String, String, std::path::PathBuf), String>),
    /// Watch later add (true) / remove finished for a video id.
    WatchLater(bool, String, Result<String, String>),
}

#[derive(Default)]
struct Tab {
    videos: Vec<Video>,
    /// First page of a refresh, held back while older (cached) results are on screen.
    staging: Vec<Video>,
    /// Live yt-dlp stream for this tab: send "how many more".
    stream: Option<Sender<usize>>,
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
        Source::Music => Some("music"),
        Source::WatchLater => Some("watchlater"),
        Source::History => Some("history"),
        Source::Playlists => Some("playlists"),
        Source::Search(_)
        | Source::Channel { .. }
        | Source::ChannelPlaylists { .. }
        | Source::Playlist { .. }
        | Source::Downloads
        | Source::Topic { .. } => None,
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
#[derive(Clone, Copy, PartialEq)]
enum Quick {
    WatchLater(bool),
    /// Hide this video's channel from feeds and search.
    MuteChannel,
    Download,
    DeleteDownload,
    RemoveFromHistory,
    CopyLink,
    OpenInBrowser,
    Watched(bool),
    PlayNext,
    AddToQueue,
    SaveTo,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PanelTab {
    UpNext,
    Comments,
}

/// Comments of one video, loaded a page at a time.
#[derive(Default)]
struct CommentsState {
    id: String,
    items: Vec<crate::innertube::Comment>,
    next: Option<String>,
    /// "10,644,361 Comments"
    count: Option<String>,
    loading: bool,
    started: bool,
    error: Option<String>,
}

struct SaveDialog {
    video: Video,
    list: Option<Result<Vec<crate::innertube::PlaylistChoice>, String>>,
}

struct Player {
    child: Child,
    /// mpv's yt-dlp reads this private copy of the cookies.
    _jar: yt::CookieCopy,
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
    /// Safari is installed but its cookies need Full Disk Access.
    safari_blocked: bool,
    /// Browser profiles for "Sign in with Google", most recent first.
    sessions: Vec<Session>,
    session_pick: usize,
    /// For the fade-in when the grid changes.
    view_shown: Source,
    view_since: std::time::Instant,
    started: std::time::Instant,
    /// Where the import in flight comes from, saved on success for "Refresh session".
    import_source: Option<(String, std::path::PathBuf)>,
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
    /// The list a video was started from (a feed grid, the Shorts row…);
    /// next/autoplay walk it. `queue_pos`: the playing video's index in it.
    queue_ctx: Vec<Video>,
    queue_pos: Option<usize>,
    /// "Play next" / "Add to queue": played before the rest of `queue_ctx`.
    up_next: std::collections::VecDeque<Video>,
    /// Played this session, for Previous.
    played: Vec<Video>,
    /// The video playing now, as it was clicked (title, channel, duration).
    now: Option<Video>,
    /// Up next was last scrolled to "Now playing" for this video id.
    up_next_anchor: String,
    /// Suggestions failed for this video id: (id, error).
    related_err: Option<(String, String)>,
    /// YouTube's suggestions for the playing video: (video id, list).
    related: (String, Vec<Video>),
    /// The list the queue context came from, for the panel's heading.
    queue_label: String,
    /// When the current video ended (autoplay countdown), and whether the
    /// countdown was cancelled for it.
    eof_at: Option<std::time::Instant>,
    autoplay_cancelled: bool,
    smoke: Option<Smoke>,

    // links handed over from outside (clipboard, Services menu)
    /// Waiting for `ui` (needs the Frame to leave the player, and a signed-in main screen).
    pending_link: Option<crate::links::Link>,
    /// Start time from a `t=` link, for the video it came with.
    link_start: Option<(String, f64)>,
    /// YouTube link on the clipboard, offered as a button in the top bar.
    clip_link: Option<crate::links::Link>,
    /// Clipboard text already looked at (so a dismissed link isn't offered again).
    clip_seen: String,
    was_focused: bool,
    /// Settings → Feed "hide titles containing" text, as typed.
    muted_words_buf: String,
    settings_tab: SettingsTab,
    /// Home's category rows: title → (token they were loaded with, videos).
    topic_shelves: HashMap<String, (String, Vec<Video>)>,
    topic_loading: std::collections::HashSet<String>,
    /// Home's "Show all": the endless For you feed instead of Home's sections.
    feed_all: bool,
    /// The Up next overlay on the right side of the video.
    up_next_open: bool,
    /// Videos known to be songs (marked in a list, or by YouTube Music).
    songs: std::collections::HashSet<String>,
    /// Auto EQ per song (None: genre unknown), and lookups started.
    eq_found: HashMap<String, Option<crate::eq::Preset>>,
    eq_asked: std::collections::HashSet<String>,
    /// Back on a song: the player shrinks to a bar under the pages and the
    /// song plays on (audio only) instead of stopping.
    mini: bool,
    /// The player's controls are showing (the overlay follows them).
    controls_visible: bool,
    /// Where the overlay was drawn last frame (for "pointer is over it").
    overlay_rect: Option<egui::Rect>,
    /// Index of the queue row being dragged.
    queue_drag: Option<usize>,
    /// Rating / channel / comments token of the playing video.
    watch: Option<(String, crate::innertube::WatchInfo)>,
    /// Description of the playing video: (id, text).
    description: Option<(String, String)>,
    /// "Show more" pressed on it.
    desc_expanded: bool,
    /// Which list the overlay shows.
    panel_tab: PanelTab,
    comments: CommentsState,
    /// The "Save to playlist" window.
    save: Option<SaveDialog>,
    session_refresh: SessionRefresh,
    /// Last background "still signed in?" probe.
    session_checked: std::time::Instant,
    /// Now Playing: last published state and when, the channel name and
    /// thumbnail of what's playing.
    np_last: Option<(crate::nowplaying::Info, std::time::Instant)>,
    np_artist: String,
    np_art: Option<Vec<u8>>,
    downloads: crate::downloads::Manifest,
    /// Stream URLs resolved on hover: id → (ytdl-format, URLs, when).
    prepared: HashMap<String, (String, yt::Direct, std::time::Instant)>,
    /// Resolves running now (hover ×2, the next video ×1).
    preparing: std::collections::HashSet<String>,
    /// The player opened idle at this moment, waiting for a running resolve.
    resolve_wait: Option<std::time::Instant>,
    /// The default route's local address, and when it was last checked.
    net_route: Option<std::net::IpAddr>,
    net_checked: std::time::Instant,
    /// dev (`YTMINI_PLAY_SEQ=id,id,…`): videos to play one after another,
    /// each once the current one has played 4 s (start-up timing). Steps
    /// `back` (as if Back were clicked), `wait:<s>`, `expand`, `pause`, `seek:<s>`, `tray`, `minimize`, `end` (seek to the last
    /// 2 s) and `shot:<png path>` too.
    play_seq: Vec<String>,
    /// dev: a `wait:<s>` step holds the sequence until then.
    play_seq_wait: Option<std::time::Instant>,
    /// Native lookup failures already told about (see `native::failures`).
    native_seen: u32,
    /// DeArrow branding per video id (None while being fetched).
    dearrow: HashMap<String, Option<crate::dearrow::Branding>>,
    dearrow_tx: Option<Sender<String>>,
    /// Card under the pointer and since when (hover-to-prepare).
    hover: Option<(Video, std::time::Instant)>,
    /// In-flight downloads: id → progress 0–1.
    dl_active: HashMap<String, f32>,

    // background (tray)
    tray: Option<Tray>,
    tray_rx: Receiver<TrayEvent>,
    hidden: bool,
    quitting: bool,
    deps: DepsState,

    positions: Positions,
    /// The video you last stopped partway through: first on Home, to
    /// continue (`continue.json`; songs aren't kept).
    last_stopped: Option<Video>,
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
        eprintln!(
            "[smoke {:>5.1}s] {} {what}",
            self.since.elapsed().as_secs_f32(),
            if ok { "PASS" } else { "FAIL" }
        );
    }
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let ctx = &cc.egui_ctx;
        crate::theme::init(ctx);

        let (tx, rx) = channel();
        let cookies = yt::cookie_file().unwrap_or_else(|_| PathBuf::from("cookies.txt"));
        yt::CookieCopy::clean_stale();
        let settings = Settings::load();
        crate::i18n::set(settings.language);
        crate::i18n::set_region(settings.region.clone());
        let (tray_tx, tray_rx) = channel();
        let tray = Tray::new(ctx, tray_tx).ok();
        let limits = Arc::new(Limits::new(settings.cache_mb, settings.cache_days));
        let mut app = App {
            tools: None,
            cookies,
            screen: Screen::Onboarding,
            tx,
            rx,
            safari_blocked: signin::safari_needs_access(),
            sessions: signin::sessions(),
            session_pick: 0,
            import_source: None,
            view_shown: Source::Feed,
            view_since: std::time::Instant::now(),
            started: std::time::Instant::now(),
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
            queue_ctx: Vec::new(),
            queue_pos: None,
            up_next: std::collections::VecDeque::new(),
            played: Vec::new(),
            now: None,
            up_next_anchor: String::new(),
            related_err: None,
            related: (String::new(), Vec::new()),
            queue_label: String::new(),
            eof_at: None,
            autoplay_cancelled: false,
            smoke: None,
            pending_link: None,
            link_start: None,
            clip_link: None,
            clip_seen: String::new(),
            was_focused: false,
            muted_words_buf: String::new(),
            settings_tab: SettingsTab::default(),
            topic_shelves: HashMap::new(),
            topic_loading: std::collections::HashSet::new(),
            feed_all: false,
            up_next_open: false,
            songs: std::collections::HashSet::new(),
            eq_found: HashMap::new(),
            eq_asked: std::collections::HashSet::new(),
            mini: false,
            queue_drag: None,
            controls_visible: true,
            overlay_rect: None,
            watch: None,
            panel_tab: PanelTab::UpNext,
            description: None,
            desc_expanded: false,
            comments: CommentsState::default(),
            save: None,
            session_refresh: SessionRefresh::Idle,
            session_checked: std::time::Instant::now(),
            np_last: None,
            np_artist: String::new(),
            np_art: None,
            downloads: crate::downloads::Manifest::load(),
            prepared: HashMap::new(),
            dearrow: HashMap::new(),
            dearrow_tx: None,
            preparing: std::collections::HashSet::new(),
            resolve_wait: None,
            net_route: yt::route_fingerprint(),
            net_checked: std::time::Instant::now(),
            native_seen: 0,
            play_seq_wait: None,
            play_seq: std::env::var("YTMINI_PLAY_SEQ")
                .map(|v| v.split(',').rev().map(str::to_string).collect())
                .unwrap_or_default(),
            hover: None,
            dl_active: HashMap::new(),
            tray,
            tray_rx,
            hidden: false,
            quitting: false,
            deps: DepsState::default(),
            positions: Positions::load(),
            last_stopped: yt::config_dir()
                .ok()
                .and_then(|d| std::fs::read(d.join("continue.json")).ok())
                .and_then(|b| serde_json::from_slice(&b).ok()),
            nav_back: Vec::new(),
            signin: None,
            last_ext_poll: std::time::Instant::now(),
        };
        app.enforce_cache(ctx);
        app.muted_words_buf = app.settings.muted_words.join(", ");
        crate::links::install(ctx);
        crate::nowplaying::install(ctx);
        // `ytmini --play <url|id>` opens straight into the player.
        let mut args = std::env::args().skip(1);
        while let Some(a) = args.next() {
            // `--settings <n>`: open Settings on tab n (for screenshots)
            if a == "--settings" {
                let n: usize = args.next().and_then(|n| n.parse().ok()).unwrap_or(0);
                app.settings_tab = SettingsTab::ALL[n.min(SettingsTab::ALL.len() - 1)];
                app.show_settings = true;
            }
            if a == "--smoke-test" {
                if let Some(v) = args.next().as_deref().and_then(Video::from_url_or_id) {
                    app.settings.player = PlayerMode::Embedded;
                    app.smoke = Some(Smoke {
                        video: v.clone(),
                        step: 0,
                        since: std::time::Instant::now(),
                        failed: false,
                        mark: 0.0,
                    });
                    app.play(&v);
                }
            }
            // `--up-next`: open the Up next overlay (for screenshots)
            if a == "--up-next" {
                app.up_next_open = true;
            }
            // `--comments`: open it on the Comments tab (screenshots)
            if a == "--comments" {
                app.up_next_open = true;
                app.panel_tab = PanelTab::Comments;
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
                let update_due =
                    t.ytdlp_managed && deps::check_due(Dep::YtDlp, Duration::from_secs(24 * 3600));
                self.tools = Some(t);
                if !need.is_empty() {
                    self.run_deps_job(ctx, need, false);
                } else if update_due {
                    self.run_deps_job(ctx, vec![Dep::YtDlp], true);
                }
                self.refresh_versions(ctx);
                let on_main = matches!(self.screen, Screen::Main);
                // a new yt-dlp (or deno) may have arrived: restart the helpers
                crate::resolver::reset();
                if yt::logged_in(&self.cookies) {
                    let (tools, cookies) = (self.tools.clone(), self.cookies.clone());
                    std::thread::spawn(move || {
                        if let Some(t) = tools {
                            crate::resolver::warm_up(&t, &cookies);
                        }
                    });
                    self.screen = Screen::Main;
                    if !on_main {
                        self.load(ctx, Source::Feed, false);
                        self.prefetch_tabs(ctx);
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
        let deno_on_system = self
            .tools
            .as_ref()
            .and_then(|t| t.deno.clone())
            .is_some_and(|p| Dep::Deno.managed() != Some(p));
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
                    deps::update_if_needed(dep, &report)
                        .map(|v| v.map(|v| tf("{} updated to {}", &[&dep.name(), &v])))
                } else {
                    deps::install(dep, &report).map(|exe| {
                        Some(tf(
                            "{} {} installed",
                            &[&dep.name(), &deps::version(&exe).unwrap_or_default()],
                        ))
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
        let Some(tools) = self.tools.clone() else {
            return;
        };
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let ytdlp_src = if tools.ytdlp_managed {
                t("auto-updated by ytmini")
            } else {
                t("system copy")
            };
            let mut v = vec![(
                "yt-dlp".to_string(),
                format!(
                    "{} · {ytdlp_src}",
                    deps::version(&tools.ytdlp).unwrap_or_else(|| "?".into())
                ),
            )];
            v.push((
                "deno".into(),
                match &tools.deno {
                    Some(d) => {
                        let src = if Dep::Deno.managed().as_ref() == Some(d) {
                            t("downloaded by ytmini")
                        } else {
                            t("system copy")
                        };
                        format!(
                            "{} · {src}",
                            deps::version(d)
                                .unwrap_or_else(|| "?".into())
                                .trim_start_matches("deno ")
                        )
                    }
                    None => t("not found — YouTube may fail").into(),
                },
            ));
            let player = match mpv::bundled_or_system() {
                Some(true) => t("libmpv bundled in ytmini.app"),
                Some(false) => t("system libmpv"),
                None => t("separate window only (no libmpv)"),
            };
            v.push(("player".into(), player.into()));
            let _ = tx.send(Msg::Versions(v));
            ctx.request_repaint();
        });
    }

    // ---------- background work ----------

    /// A worker thread that feeds one tab. It asks YouTube's web API directly
    /// (fast, page by page with continuation tokens) and falls back to a
    /// yt-dlp stream if that fails. Videos are read only on request and sent
    /// in small chunks so cards appear as they arrive. A yt-dlp stream closes
    /// after a minute without requests (resumed later from the same position).
    fn spawn_stream(
        &self,
        ctx: &egui::Context,
        source: Source,
        generation: u64,
        start: usize,
    ) -> Option<Sender<usize>> {
        let tools = self.tools.clone()?;
        let (req_tx, req_rx) = channel::<usize>();
        let (tx, ctx, cookies) = (self.tx.clone(), ctx.clone(), self.cookies.clone());
        let debug = std::env::var_os("YTMINI_DEBUG").is_some();
        std::thread::Builder::new()
            .name("yt-list".into())
            .spawn(move || {
                let send = |videos: Vec<Video>, first: bool, batch_done: bool, exhausted: bool| {
                    let _ = tx.send(Msg::Chunk {
                        source: source.clone(),
                        generation,
                        videos,
                        first,
                        batch_done,
                        exhausted,
                    });
                    ctx.request_repaint();
                };
                let fail = |error: String| {
                    let _ = tx.send(Msg::FetchFailed {
                        source: source.clone(),
                        generation,
                        error,
                    });
                    ctx.request_repaint();
                };
                let native = if start == 0 {
                    Some(Feed::native(&cookies, &source))
                } else {
                    None
                };
                if let Some(Err(e)) = &native {
                    let e = format!("{e:#}");
                    if e.contains(crate::innertube::SESSION_EXPIRED) {
                        return fail(e); // yt-dlp would just list signed-out results
                    }
                }
                let mut feed = if let Some(native) = native {
                    native.unwrap_or_else(|e| {
                        if debug {
                            eprintln!("native {} failed, using yt-dlp: {e:#}", source.label());
                        }
                        Feed::Ytdlp(None)
                    })
                } else {
                    Feed::Ytdlp(None)
                };
                let mut delivered = start;
                let mut first = start == 0;
                loop {
                    let idle = match feed {
                        Feed::Native { .. } => Duration::from_secs(24 * 3600),
                        Feed::Ytdlp(_) => Duration::from_secs(60),
                    };
                    let Ok(want) = req_rx.recv_timeout(idle) else {
                        return;
                    };
                    let (mut chunk, mut got, mut last_send) =
                        (Vec::new(), 0, std::time::Instant::now());
                    let mut exhausted = false;
                    while got < want {
                        match feed.next_video(&tools, &cookies, &source, delivered) {
                            Ok(Some(v)) => {
                                chunk.push(v);
                                got += 1;
                                delivered += 1;
                            }
                            Ok(None) => {
                                exhausted = true;
                                break;
                            }
                            Err(e) => {
                                if !chunk.is_empty() {
                                    send(std::mem::take(&mut chunk), first, false, false);
                                }
                                return fail(format!("{e:#}"));
                            }
                        }
                        if last_send.elapsed() >= Duration::from_millis(150)
                            && got < want
                            && !feed.buffered()
                        {
                            send(std::mem::take(&mut chunk), first, false, false);
                            last_send = std::time::Instant::now();
                        }
                    }
                    send(chunk, first, true, exhausted);
                    first = false;
                    if exhausted {
                        return;
                    }
                    feed.prefetch();
                }
            })
            .ok()?;
        req_tx.send(yt::PAGE).ok()?;
        Some(req_tx)
    }

    /// Load every sidebar tab in parallel (each on its own yt-dlp stream), so
    /// switching tabs shows fresh results right away.
    fn prefetch_tabs(&mut self, ctx: &egui::Context) {
        for t in BASE_TABS {
            self.load(ctx, t, false);
        }
    }

    /// Load the first page of `source` (if not cached, or always when `refresh`).
    fn load(&mut self, ctx: &egui::Context, source: Source, refresh: bool) {
        if source == Source::Downloads {
            let tab = self.tabs.entry(source).or_default();
            tab.videos = self
                .downloads
                .entries
                .iter()
                .rev()
                .map(|e| e.as_video())
                .collect();
            (tab.loading, tab.loaded_once, tab.exhausted, tab.error) = (false, true, true, None);
            return;
        }
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
        tab.staging.clear();
        tab.stream = None; // stops the old stream
        let generation = tab.generation;
        let stream = self.spawn_stream(ctx, source.clone(), generation, 0);
        if let Some(tab) = self.tabs.get_mut(&source) {
            tab.stream = stream;
        }
    }

    fn load_more(&mut self, ctx: &egui::Context, source: Source) {
        let tab = self.tabs.entry(source.clone()).or_default();
        if tab.loading || tab.exhausted {
            return;
        }
        tab.loading = true;
        tab.error = None;
        // ask the live stream for more; if it was closed while idle, resume
        if tab
            .stream
            .as_ref()
            .is_some_and(|s| s.send(yt::PAGE).is_ok())
        {
            return;
        }
        let (generation, start) = (tab.generation, tab.videos.len());
        let stream = self.spawn_stream(ctx, source.clone(), generation, start);
        if let Some(tab) = self.tabs.get_mut(&source) {
            tab.stream = stream;
        }
    }

    fn handle_messages(&mut self, ctx: &egui::Context) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Chunk {
                    source,
                    generation,
                    videos,
                    first,
                    batch_done,
                    exhausted,
                } => {
                    if std::env::var_os("YTMINI_DEBUG").is_some() {
                        eprintln!(
                            "[{:6.2}s] {} +{} first={first} done={batch_done}",
                            self.started.elapsed().as_secs_f32(),
                            source.label(),
                            videos.len()
                        );
                    }
                    let name = list_name(&source);
                    let tab = self.tabs.entry(source).or_default();
                    if tab.generation != generation {
                        continue; // superseded by a refresh
                    }
                    if first {
                        let seen: HashSet<String> =
                            tab.staging.iter().map(|v| v.id.clone()).collect();
                        tab.staging
                            .extend(videos.into_iter().filter(|v| !seen.contains(&v.id)));
                        // Nothing (or only an error) on screen: show cards as they come.
                        // Cached results on screen: swap in once the page is complete.
                        if tab.cached_at.is_none() || batch_done {
                            if tab.cached_at.is_some() || tab.videos.is_empty() {
                                tab.scroll_to_top = true;
                            }
                            tab.videos = tab.staging.clone();
                            tab.cached_at = None;
                        }
                        if batch_done {
                            tab.staging.clear();
                        }
                    } else {
                        let seen: HashSet<String> =
                            tab.videos.iter().map(|v| v.id.clone()).collect();
                        tab.videos
                            .extend(videos.into_iter().filter(|v| !seen.contains(&v.id)));
                    }
                    if batch_done {
                        tab.loading = false;
                        tab.loaded_once = true;
                        tab.exhausted = exhausted;
                        if exhausted {
                            tab.stream = None;
                        }
                        if let Some(name) = name {
                            cache::write_list(name, &tab.videos);
                        }
                    }
                }
                Msg::FetchFailed {
                    source,
                    generation,
                    error: e,
                } => {
                    let tab = self.tabs.entry(source).or_default();
                    if tab.generation != generation {
                        continue;
                    }
                    tab.loading = false;
                    tab.stream = None;
                    tab.staging.clear();
                    let looks_stale = [
                        "Sign in to confirm",
                        "signature",
                        "challenge",
                        "HTTP Error 403",
                        "nsig",
                    ]
                    .iter()
                    .any(|k| e.contains(k));
                    if e.contains(crate::innertube::SESSION_EXPIRED) {
                        tab.error = Some(
                            t("Your YouTube session expired. Refreshing it from your browser…")
                                .into(),
                        );
                        self.auto_refresh_session(ctx);
                        continue;
                    }
                    tab.error = Some(e);
                    let managed = self.tools.as_ref().is_some_and(|t| t.ytdlp_managed);
                    if looks_stale
                        && managed
                        && deps::check_due(Dep::YtDlp, Duration::from_secs(3600))
                    {
                        self.run_deps_job(ctx, vec![Dep::YtDlp], true);
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
                            Some(total) => tf(
                                "Downloading {} — {} / {} MB",
                                &[
                                    &dep.name(),
                                    &format!("{:.0}", done as f64 / 1e6),
                                    &format!("{:.0}", total as f64 / 1e6),
                                ],
                            ),
                            None => tf(
                                "Downloading {} — {} MB",
                                &[&dep.name(), &format!("{:.0}", done as f64 / 1e6)],
                            ),
                        },
                        deps::Progress::Verifying(dep) => {
                            tf("Verifying {} checksum…", &[&dep.name()])
                        }
                        deps::Progress::Preparing(dep) => {
                            if cfg!(target_os = "macos") {
                                tf(
                                    "Preparing {} (macOS checks new apps once — about 15 s)…",
                                    &[&dep.name()],
                                )
                            } else {
                                tf("Preparing {}…", &[&dep.name()])
                            }
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
                                self.status = Some((tf("Couldn't update tools: {}", &[&e]), true));
                            }
                        }
                    }
                }
                Msg::Versions(v) => self.deps.versions = v,
                Msg::ExternalProgress(id, pos, dur) => self.positions.update(&id, pos, dur),
                Msg::Switched(r) => {
                    if let Err(e) = r {
                        self.status = Some((tf("Couldn't switch quality: {}", &[&e]), true));
                    }
                }
                Msg::Imported(r) => {
                    self.importing = None;
                    match r {
                        Ok(()) => {
                            self.import_error = None;
                            if let Some((id, profile)) = self.import_source.take() {
                                self.settings.signin_source =
                                    Some(crate::settings::SignInSource { id, profile });
                                self.settings.save();
                            }
                            self.screen = Screen::Main;
                            self.tabs.clear(); // new account/cookies: drop everything cached
                            self.status = Some((t("Signed in — cookies imported.").into(), false));
                            self.load(ctx, self.active.clone(), true);
                            self.prefetch_tabs(ctx);
                        }
                        Err(e) => self.import_error = Some(e),
                    }
                }
                Msg::SignInCheck(r) => self.on_signin_check(ctx, r),
                Msg::Status(text, is_err) => self.status = Some((text, is_err)),
                Msg::SessionRefreshed(r) => {
                    self.session_refresh = SessionRefresh::Done(std::time::Instant::now());
                    match r {
                        Ok((label, id, profile)) => {
                            self.settings.signin_source =
                                Some(crate::settings::SignInSource { id, profile });
                            self.status =
                                Some((tf("Session refreshed from {}.", &[&label]), false));
                            self.tabs.clear();
                            self.load(ctx, self.active.clone(), true);
                            self.prefetch_tabs(ctx);
                        }
                        Err(e) => {
                            self.status = Some((
                                tf("Your YouTube session expired and couldn't be refreshed ({}). Settings → Account & updates → Sign in again.", &[&e]),
                                true,
                            ));
                        }
                    }
                }
                Msg::Download(p) => self.on_download(ctx, p),
                Msg::Prepared(id, format, r) => {
                    self.preparing.remove(&id);
                    // the player opened for this one and is waiting
                    if let Some(p) = self
                        .embedded
                        .as_mut()
                        .filter(|p| p.id == id && p.is_waiting())
                    {
                        let ok = r.as_ref().ok().filter(|_| p.format == format).cloned();
                        if std::env::var_os("YTMINI_DEBUG").is_some() {
                            eprintln!(
                                "waited resolve for {id}: {}",
                                if ok.is_some() { "ok" } else { "fallback" }
                            );
                        }
                        p.load_resolved(ok);
                        self.resolve_wait = None;
                    }
                    if let Ok(d) = r {
                        if self.prepared.len() > 30 {
                            self.prepared
                                .retain(|_, (_, _, t)| t.elapsed() < PREPARED_TTL);
                        }
                        self.prepared
                            .insert(id, (format, d, std::time::Instant::now()));
                    }
                }
                Msg::DeArrow(id, b) => {
                    self.dearrow.insert(id, Some(b));
                }
                Msg::TopicShelf(title, token, vids) => {
                    self.topic_loading.remove(&title);
                    self.topic_shelves.insert(title, (token, vids));
                }
                Msg::Watch(id, info) => {
                    if self.playing_id() == Some(id.as_str()) {
                        self.watch = Some((id, info));
                    }
                }
                Msg::Description(id, d) => {
                    if self.playing_id() == Some(id.as_str()) {
                        self.description = Some((id, d));
                        self.desc_expanded = false;
                    }
                }
                Msg::Rated(id, like) => {
                    if let Some((_, w)) = self.watch.as_mut().filter(|(w, _)| *w == id) {
                        w.like = like;
                    }
                }
                Msg::Subscribed(cid, on) => {
                    if let Some((_, w)) = self.watch.as_mut() {
                        if w.channel_id.as_deref() == Some(cid.as_str()) {
                            w.subscribed = Some(on);
                        }
                    }
                }
                Msg::Comments(id, r, more) => {
                    let c = &mut self.comments;
                    if c.id == id {
                        c.loading = false;
                        match r {
                            Ok(page) => {
                                if more {
                                    c.items.extend(page.items);
                                } else {
                                    c.items = page.items;
                                    c.count = page.count;
                                }
                                c.next = page.next;
                            }
                            Err(e) => c.error = Some(e),
                        }
                    }
                }
                Msg::Playlists(id, r) => {
                    if let Some(d) = self.save.as_mut().filter(|d| d.video.id == id) {
                        d.list = Some(r);
                    }
                }
                Msg::Song(id) => {
                    self.songs.insert(id);
                }
                Msg::Genre(id, p) => {
                    self.eq_found.insert(id, p);
                }
                Msg::Related(id, vids) => {
                    if self.playing_id() == Some(id.as_str()) {
                        self.related = (id, vids);
                        self.related_err = None;
                    }
                }
                Msg::RelatedFailed(id, e) => {
                    if self.playing_id() == Some(id.as_str()) {
                        self.related_err = Some((id, e));
                    }
                }
                Msg::Chapters(id, c) => {
                    if let Some(p) = self.embedded.as_mut().filter(|p| p.id == id) {
                        p.chapters = c;
                    }
                }
                Msg::Storyboard(id, sb) => {
                    if let Some(p) = self.embedded.as_mut().filter(|p| p.id == id) {
                        p.storyboard = Some(sb);
                    }
                }
                Msg::NowPlayingArt(id, bytes) => {
                    if self.embedded.as_ref().is_some_and(|p| p.id == id) {
                        self.np_art = Some(bytes);
                        self.np_last = None; // republish with artwork
                    }
                }
                Msg::Segments(id, segs) => {
                    if let Some(p) = self.embedded.as_mut().filter(|p| p.id == id) {
                        p.segments = segs;
                    }
                }
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

    /// First page of one home category, for its row (one fetch at a time
    /// per category; a failure leaves it empty rather than retrying forever).
    fn load_topic_shelf(&mut self, ctx: &egui::Context, title: String, token: String) {
        if !self.topic_loading.insert(title.clone()) {
            return;
        }
        let (tx, ctx, cookies) = (self.tx.clone(), ctx.clone(), self.cookies.clone());
        std::thread::spawn(move || {
            let src = Source::Topic {
                title: title.clone(),
                token: token.clone(),
            };
            let vids = crate::innertube::Pager::new(&cookies, &src)
                .and_then(|mut p| p.next_page())
                .ok()
                .flatten()
                .unwrap_or_default();
            let _ = tx.send(Msg::TopicShelf(title, token, vids));
            ctx.request_repaint();
        });
    }

    /// Start `v` with `list` (where it was clicked) as the queue. Only lists
    /// whose order means something are passed (a playlist, Watch later, the
    /// Shorts row); from a feed, Next follows YouTube's suggestions instead.
    fn play_from(&mut self, list: Vec<Video>, v: &Video) {
        self.mini = false;
        self.queue_pos = list.iter().position(|x| x.id == v.id);
        self.queue_ctx = if self.queue_pos.is_some() {
            list
        } else {
            Vec::new()
        };
        self.played.clear();
        self.play(v);
    }

    fn playing_id(&self) -> Option<&str> {
        self.embedded
            .as_ref()
            .map(|p| p.id.as_str())
            .or(self.player.as_ref().map(|p| p.id.as_str()))
    }

    /// What Next would play.
    fn peek_next(&self) -> Option<&Video> {
        self.up_next
            .front()
            .or_else(|| self.queue_pos.and_then(|i| self.queue_ctx.get(i + 1)))
            .or_else(|| self.next_suggestion())
    }

    /// YouTube's top suggestion for the playing video that we haven't just
    /// played (what the website autoplays).
    fn next_suggestion(&self) -> Option<&Video> {
        let cur = self.playing_id()?;
        if self.related.0 != cur {
            return None;
        }
        let song = self.playing_song();
        self.related.1.iter().find(|v| {
            !v.is_playlist && (!song || v.is_music) && !self.played.iter().any(|p| p.id == v.id)
        })
    }

    /// Is the playing video a song? (Then suggestions are songs too, and
    /// Back keeps it playing in the bar.)
    fn playing_song(&self) -> bool {
        self.playing_id().is_some_and(|id| self.songs.contains(id))
    }

    fn go_next(&mut self) {
        let next = if let Some(v) = self.up_next.pop_front() {
            v
        } else if let Some(v) = self
            .queue_pos
            .and_then(|i| self.queue_ctx.get(i + 1))
            .cloned()
        {
            self.queue_pos = self.queue_pos.map(|i| i + 1);
            v
        } else if let Some(v) = self.next_suggestion().cloned() {
            v
        } else {
            return;
        };
        self.remember_current();
        self.switch_to(&next);
    }

    /// Play `v` now from the Up next panel.
    fn jump_to(&mut self, v: &Video) {
        self.up_next.retain(|x| x.id != v.id);
        self.played.retain(|x| x.id != v.id);
        if let Some(i) = self.queue_ctx.iter().position(|x| x.id == v.id) {
            self.queue_pos = Some(i);
        }
        self.remember_current();
        self.switch_to(v);
    }

    fn go_prev(&mut self) {
        let Some(prev) = self.played.pop() else {
            // nothing before: restart this one
            if let Some(p) = &mut self.embedded {
                p.seek_absolute(0.0, true);
            }
            return;
        };
        // put the current one back so Next returns to it
        if let Some(cur) = self.current_video() {
            match self.queue_ctx.iter().position(|x| x.id == prev.id) {
                Some(i) if self.queue_ctx.get(i + 1).is_some_and(|x| x.id == cur.id) => {
                    self.queue_pos = Some(i)
                }
                _ => self.up_next.push_front(cur),
            }
        }
        self.switch_to(&prev);
    }

    fn current_video(&self) -> Option<Video> {
        let id = self.playing_id()?;
        self.now
            .iter()
            .chain(self.queue_ctx.iter())
            .chain(self.played.iter())
            .find(|v| v.id == id)
            .cloned()
            .map(|mut v| {
                // opened by link / id: take the title the player learned
                if v.title.is_none() {
                    v.title = self
                        .embedded
                        .as_ref()
                        .map(|p| p.title.clone())
                        .filter(|t| !t.is_empty());
                }
                v
            })
            .or_else(|| {
                self.embedded.as_ref().map(|p| Video {
                    id: p.id.clone(),
                    title: Some(p.title.clone()),
                    ..Default::default()
                })
            })
    }

    fn remember_current(&mut self) {
        if let Some(cur) = self.current_video() {
            self.played.retain(|v| v.id != cur.id);
            self.played.push(cur);
            if self.played.len() > 200 {
                self.played.remove(0);
            }
        }
    }

    /// The playing song's artist (for the EQ choice and the lookup), and
    /// the song's name.
    fn song_artist(&self) -> Option<(String, String)> {
        let p = self.embedded.as_ref()?;
        let channel = match &self.watch {
            Some((id, w)) if self.np_artist.is_empty() && *id == p.id => {
                w.channel.clone().unwrap_or_default()
            }
            _ => self.np_artist.clone(),
        };
        crate::eq::artist_and_song(&p.title, &channel)
    }

    /// What the EQ menu shows for the playing video (None: not a song).
    fn eq_view(&self) -> Option<crate::eq::View> {
        let id = self.playing_id()?;
        if !self.songs.contains(id) {
            return None;
        }
        let manual = self.song_artist().and_then(|(a, _)| {
            self.settings
                .eq_artists
                .get(&crate::eq::artist_key(&a))
                .copied()
        });
        let auto = if self.settings.eq_auto {
            self.eq_found
                .get(id)
                .map(|p| p.unwrap_or(crate::eq::Preset::Flat))
        } else {
            Some(crate::eq::Preset::Flat)
        };
        Some(crate::eq::View {
            current: manual.or(auto).unwrap_or(crate::eq::Preset::Flat),
            manual: manual.is_some(),
            auto,
        })
    }

    /// A choice from the EQ menu: remembered for the artist (Auto forgets it).
    fn pick_eq(&mut self, pick: crate::eq::Pick) {
        let Some((artist, _)) = self.song_artist() else {
            return;
        };
        let key = crate::eq::artist_key(&artist);
        match pick {
            Some(p) => {
                self.settings.eq_artists.insert(key, p);
            }
            None => {
                self.settings.eq_artists.remove(&key);
            }
        }
    }

    /// Songs: look up the genre once, and keep mpv's EQ on the preset in
    /// effect. Everything else plays flat.
    fn sync_eq(&mut self, ctx: &egui::Context) {
        let Some(id) = self.embedded.as_ref().map(|p| p.id.clone()) else {
            return;
        };
        let song = self.songs.contains(&id);
        if song && self.settings.eq_auto && !self.eq_asked.contains(&id) {
            if let Some((artist, name)) = self.song_artist() {
                self.eq_asked.insert(id.clone());
                let (tx, ctx, id) = (self.tx.clone(), ctx.clone(), id.clone());
                std::thread::spawn(move || {
                    let found = crate::eq::lookup(&artist, &name).ok();
                    if std::env::var_os("YTMINI_DEBUG").is_some() {
                        eprintln!("eq: {artist} — {name}: {found:?}");
                    }
                    let p = found
                        .as_deref()
                        .map(crate::eq::Preset::for_genres)
                        .filter(|p| *p != crate::eq::Preset::Flat);
                    let _ = tx.send(Msg::Genre(id, p));
                    ctx.request_repaint();
                });
            }
        }
        let filter = self.eq_view().and_then(|v| v.current.filter());
        if let Some(p) = &mut self.embedded {
            p.set_eq(filter);
        }
    }

    /// The embedded video is being stopped: if it's unfinished (and not a
    /// song), it becomes Home's first card; finished, it stops being one.
    fn remember_stopped(&mut self) {
        let Some(p) = self.embedded.as_ref().filter(|p| p.started) else {
            return;
        };
        let (id, pos, dur) = (p.id.clone(), p.pos, p.duration);
        self.positions.update(&id, pos, dur);
        let before = self.last_stopped.as_ref().map(|v| v.id.clone());
        if self.positions.resume_at(&id).is_some() && !self.songs.contains(&id) {
            if let Some(mut v) = self.current_video() {
                if v.duration.is_none() && dur > 0.0 {
                    v.duration = Some(dur);
                }
                self.last_stopped = Some(v);
            }
        } else if before.as_deref() == Some(id.as_str()) {
            self.last_stopped = None;
        } else {
            return;
        }
        if let Ok(d) = yt::config_dir() {
            let path = d.join("continue.json");
            match &self.last_stopped {
                Some(v) => {
                    if let Ok(b) = serde_json::to_vec(v) {
                        let _ = std::fs::write(path, b);
                    }
                }
                None => {
                    let _ = std::fs::remove_file(path);
                }
            }
        }
    }

    /// Replace the playing video, keeping fullscreen and the player view.
    fn switch_to(&mut self, v: &Video) {
        self.remember_stopped();
        if let Some(p) = self.embedded.take() {
            self.settings.volume = p.volume;
            if p.started {
                self.positions.update(&p.id, p.pos, p.duration);
            }
            self.positions.save();
            drop(p); // GL is current during update
        }
        self.play(v);
    }

    fn play(&mut self, video: &Video) {
        self.now = Some(video.clone());
        self.watch = None;
        self.eof_at = None;
        self.autoplay_cancelled = false;
        self.stop_player();
        self.np_artist = video.channel().to_string();
        self.np_art = ["hi", "sd"]
            .iter()
            .find_map(|t| cache::read_thumb(&format!("{}-{t}", video.id)));
        self.np_last = None;
        if self.np_art.is_none() {
            // not in the thumbnail cache (e.g. opened from a link): fetch one, no cookies
            let (tx, id) = (self.tx.clone(), video.id.clone());
            std::thread::spawn(move || {
                let url = format!("https://i.ytimg.com/vi/{id}/mqdefault.jpg");
                let bytes = ureq::get(&url)
                    .call()
                    .ok()
                    .and_then(|mut r| r.body_mut().read_to_vec().ok());
                if let Some(b) = bytes {
                    let _ = tx.send(Msg::NowPlayingArt(id, b));
                }
            });
        }
        // Report the view so it shows in YouTube's watch history (as the website does).
        // YTMINI_NO_HISTORY: testing against a real account without
        // touching its watch history / recommendations
        if self.settings.mark_watched
            && self.smoke.is_none()
            && std::env::var_os("YTMINI_NO_HISTORY").is_none()
        {
            let (tx, cookies, id) = (self.tx.clone(), self.cookies.clone(), video.id.clone());
            std::thread::spawn(move || {
                if let Err(e) = crate::innertube::mark_watched(&cookies, &id) {
                    let _ = tx.send(Msg::Status(
                        tf("Couldn't add to YouTube history: {}", &[&format!("{e:#}")]),
                        true,
                    ));
                }
            });
        }
        if self.settings.player == PlayerMode::Embedded && self.gpa.is_some() {
            match mpv::available() {
                Ok(()) => {
                    self.pending_play = Some(video.clone());
                    return;
                }
                Err(e) => {
                    self.status = Some((
                        tf(
                            "Embedded player unavailable ({}) — using a separate window",
                            &[&e],
                        ),
                        true,
                    ));
                }
            }
        }
        self.play_in_window(video);
    }

    /// Start a pending embedded playback. Runs in `ui`, where GL is current.
    fn start_embedded(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        let Some(video) = self.pending_play.take() else {
            return;
        };
        let (Some(tools), Some(gpa), Some(gl)) =
            (self.tools.clone(), self.gpa.clone(), frame.gl().cloned())
        else {
            return self.play_in_window(&video);
        };
        if self.surface.is_none() {
            match Surface::new(&gl, frame) {
                Ok(s) => self.surface = Some(s),
                Err(e) => {
                    self.status = Some((
                        tf(
                            "Embedded player failed ({}) — using a separate window",
                            &[&e],
                        ),
                        true,
                    ));
                    return self.play_in_window(&video);
                }
            }
        }
        self.power = yt::power_state();
        let profile = yt::profile(self.settings.quality, self.power, self.screen_cap);
        let resume = match self.link_start.take() {
            Some((id, t)) if id == video.id => Some(t),
            _ => self.positions.resume_at(&video.id),
        };
        // a download, or URLs resolved while you hovered the card
        let direct = self
            .downloads
            .get(&video.id)
            .and_then(|e| {
                let path = |p: Option<std::path::PathBuf>| p.map(|p| p.display().to_string());
                let (v, a) = (path(e.video_path()), path(e.audio_path()));
                let (video, audio) = match v {
                    Some(v) => (v, a),
                    None => (a?, None),
                };
                Some(yt::Direct {
                    video,
                    audio,
                    user_agent: None,
                    offline: true,
                    title: None,
                })
            })
            .or_else(|| self.prepared_for(&video.id, &profile.ytdl_format));
        if let Some(d) = &direct {
            if !d.offline && std::env::var_os("YTMINI_DEBUG").is_some() {
                eprintln!("playing {} from pre-resolved URLs", video.id);
            }
        }
        // nothing stored: resolve with the warm helper (much faster than
        // mpv's own one-off yt-dlp) while the player opens
        if direct.is_none() && !profile.audio_only {
            self.start_prepare(ctx, tools.clone(), video.clone());
        }
        // hovered a moment ago, or just started above: wait for it
        let wait = direct.is_none() && self.preparing.contains(&video.id);
        self.resolve_wait = wait.then(std::time::Instant::now);
        match Embedded::start(
            &tools,
            &self.cookies,
            &video,
            &profile,
            &self.settings,
            resume,
            gpa,
            ctx,
            direct,
            wait,
        ) {
            Ok(mut p) => {
                self.status = None;
                if self.mini || self.hidden {
                    p.set_background(true); // the bar / the tray plays on: audio only
                }
                if video.is_music {
                    self.songs.insert(video.id.clone());
                } else if !self.songs.contains(&video.id) {
                    let (tx, id, ctx2) = (self.tx.clone(), video.id.clone(), ctx.clone());
                    std::thread::spawn(move || {
                        if let Ok(true) = crate::innertube::is_song(&id) {
                            let _ = tx.send(Msg::Song(id));
                            ctx2.request_repaint();
                        }
                    });
                }
                {
                    let (tx, id, cookies, ctx2) = (
                        self.tx.clone(),
                        video.id.clone(),
                        self.cookies.clone(),
                        ctx.clone(),
                    );
                    {
                        let (tx, cookies, id, ctx) =
                            (tx.clone(), cookies.clone(), id.clone(), ctx2.clone());
                        std::thread::spawn(move || {
                            fetch_related(&tx, &cookies, &id);
                            ctx.request_repaint();
                        });
                    }
                    std::thread::spawn(move || {
                        if let Ok(e) = crate::innertube::player_extras(&cookies, &id) {
                            let _ = tx.send(Msg::Description(id.clone(), e.description.clone()));
                            let chapters = crate::innertube::chapters(&id)
                                .ok()
                                .filter(|c| !c.is_empty())
                                .unwrap_or_else(|| {
                                    crate::chapters::parse(&e.description, e.duration)
                                });
                            if !chapters.is_empty() {
                                let _ = tx.send(Msg::Chapters(id.clone(), chapters));
                            }
                            if let Some(sb) = crate::storyboard::from_extras(&e) {
                                let _ = tx.send(Msg::Storyboard(id, sb));
                            }
                            ctx2.request_repaint();
                        }
                    });
                }
                if self.settings.sponsorblock {
                    let (tx, id, cats, ctx2) = (
                        self.tx.clone(),
                        video.id.clone(),
                        self.settings.sb_categories.clone(),
                        ctx.clone(),
                    );
                    std::thread::spawn(move || {
                        // A failure just means no skipping; not worth a status message.
                        if let Ok(segs) = crate::sponsorblock::fetch(&id, &cats) {
                            let _ = tx.send(Msg::Segments(id, segs));
                            ctx2.request_repaint();
                        }
                    });
                }
                // the player header shows the title; the window stays "ytmini"
                self.embedded = Some(p);
            }
            Err(e) => {
                self.status = Some((
                    tf(
                        "Embedded player failed ({}) — using a separate window",
                        &[&e],
                    ),
                    true,
                ));
                self.play_in_window(&video);
            }
        }
    }

    /// Leave the player view: tear mpv down, give back VRAM, restore the window.
    fn close_embedded(&mut self, ctx: &egui::Context, frame: &eframe::Frame) {
        self.mini = false;
        self.remember_stopped();
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
        let resume = match self.link_start.take() {
            Some((id, t)) if id == video.id => Some(t),
            _ => self.positions.resume_at(&video.id),
        };
        match yt::play(tools, &self.cookies, video, &profile, resume) {
            Ok((child, jar)) => {
                self.status = None;
                self.player = Some(Player {
                    child,
                    _jar: jar,
                    title: video.title().to_string(),
                    id: video.id.clone(),
                    url: video.url(),
                    profile: profile.label,
                    format: profile.ytdl_format,
                });
            }
            Err(e) => {
                self.status = Some((tf("Couldn't start mpv: {}", &[&format!("{e:#}")]), true))
            }
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
        let (size, ppp) = ctx.input(|i| {
            (
                i.viewport().monitor_size,
                i.viewport().native_pixels_per_point,
            )
        });
        let (Some(size), Some(ppp)) = (size, ppp) else {
            return;
        };
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
                    let why = yt::last_player_error()
                        .unwrap_or_else(|| tf("mpv exited with {}", &[&status]));
                    Some((tf("Playback failed: {}", &[&why]), true))
                };
                self.player = None;
            }
            Err(e) => {
                self.status = Some((tf("Lost track of mpv: {}", &[&e]), true));
                self.player = None;
            }
        }
    }

    // ---------- screens ----------

    fn ui_missing(&mut self, ui: &mut egui::Ui, missing: &yt::Missing) {
        let mut retry = false;
        centered_column(ui, 520.0, |ui| {
            logo(ui, 64.0);
            ui.add_space(8.0);
            ui.label(
                RichText::new(t("No video player found"))
                    .size(20.0)
                    .color(text()),
            );
            ui.add_space(12.0);
            let _ = missing;
            ui.label(t("This build of ytmini doesn't include libmpv, and mpv isn't installed. Install it with:"));
            ui.add_space(8.0);
            code_box(ui, yt::install_hint());
            ui.add_space(16.0);
            if accent_button(ui, t("Check again")).clicked() {
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
            logo(ui, 64.0);
            ui.add_space(8.0);
            ui.label(RichText::new(t("Getting ready")).size(20.0).color(text()));
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
                if accent_button(ui, t("Try again")).clicked() {
                    retry = true;
                }
            }
        });
        if retry {
            self.check_tools(ui.ctx());
        }
    }

    fn ui_onboarding(&mut self, ui: &mut egui::Ui) {
        let mut cancel = false;
        let can_cancel = yt::logged_in(&self.cookies) && self.importing.is_none();
        let busy = self.importing.is_some();
        let mut google = GoogleAction::None;

        egui::ScrollArea::vertical()
            .auto_shrink(false)
            .show(ui, |ui| {
                centered_column(ui, 560.0, |ui| {
                    logo(ui, 64.0);
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new(t("Sign in to YouTube"))
                            .size(20.0)
                            .color(text()),
                    );
                    ui.add_space(14.0);
                    google = self.ui_google_signin(ui, busy);
                    ui.add_space(10.0);
                    ui.label(
                        RichText::new(
                            "ytmini copies your YouTube session from the browser; the cookies \
                             stay on this computer.",
                        )
                        .small()
                        .color(text_dim()),
                    );
                    if self.safari_blocked {
                        ui.label(
                            RichText::new(
                                "Safari: give ytmini Full Disk Access (System Settings → Privacy & \
                                 Security) to use its session.",
                            )
                            .small()
                            .color(text_dim()),
                        );
                    }
                    ui.add_space(16.0);
                    if let Some(spec) = &self.importing {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(tf(
                                "Reading your session from {}… (answer any system prompt)",
                                &[&spec],
                            ));
                        });
                    }
                    if let Some(err) = &self.import_error {
                        egui::Frame::new()
                            .fill(err_bg())
                            .corner_radius(RADIUS)
                            .inner_margin(Margin::same(12))
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                ui.label(
                                    RichText::new(t("Couldn't sign in"))
                                        .strong()
                                        .color(crate::theme::err()),
                                );
                                ui.add(
                                    egui::Label::new(RichText::new(err).size(13.0).color(text()))
                                        .wrap(),
                                );
                            });
                    }
                    if can_cancel {
                        ui.add_space(12.0);
                        if ui.button(t("← Back to videos")).clicked() {
                            cancel = true;
                        }
                    }
                });
            });

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
            .frame(egui::Frame::new().fill(sidebar()).inner_margin(Margin {
                left: 10,
                right: 10,
                top: 14,
                bottom: 10,
            }))
            .show_separator_line(false)
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                let item = |ui: &mut egui::Ui, icon: Icon, label: &str, selected: bool| -> bool {
                    nav_item(ui, icon, label, selected, collapsed).clicked()
                };
                ui.add_space(2.0);
                if collapsed {
                    ui.vertical_centered(|ui| logo(ui, 34.0));
                } else {
                    ui.horizontal(|ui| {
                        ui.add_space(4.0);
                        logo(ui, 34.0);
                        ui.label(
                            RichText::new(t("mini YouTube"))
                                .size(12.5)
                                .color(text_dim()),
                        );
                    });
                }
                ui.add_space(12.0);
                let home_of = |t: &Source| {
                    t == &self.active
                        || (*t == Source::Feed && self.active.is_home())
                        || (self.active.is_page() && self.nav_back.first() == Some(t))
                };
                for t in BASE_TABS {
                    if t == Source::WatchLater && !collapsed {
                        ui.add_space(10.0);
                        ui.label(
                            RichText::new(crate::i18n::t("Library"))
                                .small()
                                .color(text_dim()),
                        );
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
                        let (icon, tip) = if collapsed {
                            (Icon::Expand, t("Expand sidebar"))
                        } else {
                            (Icon::Collapse, t("Collapse sidebar"))
                        };
                        if nav_item(ui, icon, t("Collapse"), false, collapsed)
                            .on_hover_text(tip)
                            .clicked()
                        {
                            self.settings.sidebar_collapsed = !self.settings.sidebar_collapsed;
                        }
                    }
                    if item(ui, Icon::Gear, t("Settings"), self.show_settings) {
                        self.show_settings = !self.show_settings;
                    }
                });
            });

        // ----- top bar: search + refresh -----
        egui::Panel::top("top")
            .frame(egui::Frame::new().fill(bg()).inner_margin(Margin {
                left: 20,
                right: 16,
                top: 12,
                bottom: 8,
            }))
            .show_separator_line(false)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let w = (ui.available_width() - 60.0).clamp(160.0, 520.0);
                    let r = ui.add(
                        egui::TextEdit::singleline(&mut self.search_query)
                            .hint_text(t("Search YouTube"))
                            .desired_width(w)
                            .margin(Margin::symmetric(12, 7)),
                    );
                    if self.focus_search {
                        r.request_focus();
                        self.focus_search = false;
                    }
                    if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                        let q = self.search_query.trim();
                        if let Some(link) = crate::links::parse(q) {
                            self.pending_link = Some(link);
                            self.search_query.clear();
                        } else if !q.is_empty() {
                            search = Some(q.to_string());
                        }
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let b = egui::Button::new(RichText::new("↻").size(18.0).color(text_dim()))
                            .frame(false);
                        if ui
                            .add(b)
                            .on_hover_text(if cfg!(target_os = "macos") {
                                t("Refresh (⌘R)")
                            } else {
                                t("Refresh (Ctrl+R)")
                            })
                            .clicked()
                        {
                            refresh = true;
                        }
                        if let Some(link) = self.clip_link.clone() {
                            let x =
                                egui::Button::new(RichText::new("✕").size(11.0).color(text_dim()))
                                    .frame(false);
                            if ui.add(x).on_hover_text(t("Dismiss")).clicked() {
                                self.clip_link = None;
                            }
                            if ui
                                .add(accent_btn(&format!("▶  {}", link.verb())))
                                .on_hover_text(t("A YouTube link is on your clipboard"))
                                .clicked()
                            {
                                self.pending_link = Some(link);
                                self.clip_link = None;
                            }
                        }
                    });
                });
            });

        // ----- status bar -----
        let active_loading = self.tabs.get(&self.active).is_some_and(|t| t.loading);
        let mut stop = false;
        let has_status = active_loading
            || self.player.is_some()
            || self.status.is_some()
            || self.power.saver
            || self.power.on_battery;
        if has_status {
            egui::Panel::bottom("status")
                .frame(
                    egui::Frame::new()
                        .fill(panel())
                        .inner_margin(Margin::symmetric(16, 6)),
                )
                .show_separator_line(false)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        let cached_at = self.tabs.get(&self.active).and_then(|t| t.cached_at);
                        if active_loading {
                            ui.spinner();
                            let msg = match cached_at {
                                Some(at) => tf("Updating… (showing results from {})", &[&ago(at)]),
                                None => t("Loading…").into(),
                            };
                            ui.label(RichText::new(msg).color(text_dim()));
                        } else if let Some(p) = &self.player {
                            ui.label(RichText::new("▶").color(accent()));
                            ui.label(RichText::new(tf("Playing: {}", &[&p.title])).color(text()));
                            ui.label(RichText::new(&p.profile).color(text_dim()));
                            if ui.small_button(t("Stop")).clicked() {
                                stop = true;
                            }
                        } else if let Some((msg, is_err)) = &self.status {
                            ui.label(RichText::new(msg).color(if *is_err {
                                crate::theme::err()
                            } else {
                                text_dim()
                            }));
                        }
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            let power = if self.power.saver {
                                t("battery saver")
                            } else if self.power.on_battery {
                                t("on battery")
                            } else {
                                ""
                            };
                            if !power.is_empty() {
                                ui.label(RichText::new(power).small().color(text_dim()))
                                    .on_hover_text(t(
                                        "Auto quality lowers resolution/frame rate to save power",
                                    ));
                            }
                        });
                    });
                });
        }
        if stop {
            self.stop_player();
        }

        // ----- grid -----
        self.ui_page_header(ui);
        self.ui_filter_bar(ui);
        let mut clicked: Option<Video> = None;
        let mut open_channel: Option<Video> = None;
        let mut quick: Option<(Quick, Video)> = None;
        if self.active != self.view_shown {
            self.view_shown = self.active.clone();
            self.view_since = std::time::Instant::now();
        }
        let mut more = false;
        let mut reimport_from_empty = false;
        let mut hovered_video: Option<Video> = None;
        let mut want_branding: Vec<String> = Vec::new();
        let mut clicked_list: Vec<Video> = Vec::new();
        let mut clicked_label = String::new();
        let mut want_topics: Vec<(String, String)> = Vec::new();
        let mut open_topic: Option<Source> = None;
        let mut show_all = false;
        egui::CentralPanel::no_frame().show(ui, |ui| {
            let source = self.active.clone();
            let playing_id = self.player.as_ref().map(|p| p.id.clone());
            let thumbs = &mut self.thumbs;
            let positions = &self.positions;
            let settings = &self.settings;
            let (downloads, dl_active) = (&self.downloads, &self.dl_active);
            let topic_shelves = &self.topic_shelves;
            let feed_all = self.feed_all;
            // Home: the video you stopped partway through comes first
            let resume_card = self
                .last_stopped
                .as_ref()
                .filter(|v| source == Source::Feed && self.positions.resume_at(&v.id).is_some());
            let dearrow = settings.dearrow.then_some(&self.dearrow);
            let Some(tab) = self.tabs.get_mut(&source) else {
                return;
            };
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
                            if ui.button(t("Re-import cookies…")).clicked() {
                                reimport_from_empty = true;
                            }
                        }
                    } else if tab.loaded_once {
                        if source == Source::Feed {
                            ui.label(
                                RichText::new(t("Your feed came back empty"))
                                    .size(18.0)
                                    .color(text()),
                            );
                            ui.label(
                                RichText::new(t(
                                    "That usually means the cookies expired or were rotated.",
                                ))
                                .color(text_dim()),
                            );
                            ui.add_space(10.0);
                            if accent_button(ui, t("Re-import cookies")).clicked() {
                                reimport_from_empty = true;
                            }
                        } else {
                            ui.label(RichText::new(t("Nothing here.")).color(text_dim()));
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
            // Shorts go to their own shelf on the home feed, subscriptions
            // and search (channel/playlist pages keep them in the grid).
            let shelf_src = matches!(
                source,
                Source::Feed
                    | Source::Subscriptions
                    | Source::Music
                    | Source::Topic { .. }
                    | Source::Search(_)
            );
            // Home: each of YouTube's categories as its own row
            // Home: For you (3 rows), Shorts, then the categories; the page ends
            let home = source == Source::Feed && !feed_all;
            let topics: Vec<(String, String)> = if home {
                crate::innertube::HOME_CHIPS
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone()
            } else {
                Vec::new()
            };
            // (title, token, videos once loaded with that token)
            let topic_rows: Vec<(String, String, Option<Vec<&Video>>)> = topics
                .into_iter()
                .filter_map(|(title, token)| {
                    let vids = match topic_shelves.get(&title) {
                        Some((t, vids)) if *t == token => Some(
                            vids.iter()
                                .filter(|v| {
                                    let watched = positions.get(&v.id).is_some_and(|e| e.watched());
                                    !v.is_short && !settings.hides(&source, v, watched)
                                })
                                .collect::<Vec<_>>(),
                        ),
                        _ => None,
                    };
                    // loaded but nothing left after filters: no row
                    if vids.as_ref().is_some_and(|v| v.is_empty()) {
                        return None;
                    }
                    Some((title, token, vids))
                })
                .collect();
            let mut shown: Vec<&Video> = resume_card.into_iter().collect();
            let mut shorts: Vec<&Video> = Vec::new();
            for v in &tab.videos {
                if resume_card.is_some_and(|c| c.id == v.id) {
                    continue;
                }
                let watched = positions.get(&v.id).is_some_and(|e| e.watched());
                if settings.hides(&source, v, watched) {
                    continue;
                }
                if shelf_src && v.is_short {
                    if settings.show_shorts {
                        shorts.push(v);
                    }
                } else {
                    shown.push(v);
                }
            }
            let video_rows = shown.len().div_ceil(cols);
            let show_footer = !tab.exhausted || tab.error.is_some();

            // rows: video rows, with the Shorts shelf after the second one
            enum Row {
                Head,
                Videos(usize),
                Shelf,
                Topic(usize),
                Footer,
            }
            let mut rows: Vec<Row> = Vec::new();
            if home {
                rows.push(Row::Head);
                rows.extend((0..video_rows.min(3)).map(Row::Videos));
                if !shorts.is_empty() {
                    rows.push(Row::Shelf);
                }
                rows.extend((0..topic_rows.len()).map(Row::Topic));
            } else {
                rows.extend((0..video_rows).map(Row::Videos));
                if !shorts.is_empty() {
                    rows.insert(video_rows.min(2), Row::Shelf);
                }
                if show_footer {
                    rows.push(Row::Footer);
                }
            }
            let short_w = (card_w * 0.62).clamp(140.0, 210.0);
            let short_thumb_h = short_w * 16.0 / 9.0;
            let short_h = short_thumb_h + (card_h - thumb_h);
            const SHELF_HEAD: f32 = 34.0;
            let height = |r: &Row| match r {
                Row::Head => SHELF_HEAD - GAP * 0.5,
                Row::Shelf => SHELF_HEAD + short_h,
                Row::Topic(_) => SHELF_HEAD + card_h,
                _ => card_h,
            };
            let total: f32 = rows.iter().map(|r| height(r) + GAP).sum();

            let mut draw = |ui: &mut egui::Ui, v: &Video, card: CardInfo, variant: Variant| {
                let slot = egui::Rect::from_min_size(ui.cursor().min, card.size);
                if !v.is_playlist
                    && ui
                        .input(|i| i.pointer.hover_pos())
                        .is_some_and(|p| slot.contains(p) && ui.clip_rect().contains(p))
                {
                    hovered_video = Some(v.clone());
                }
                // DeArrow: community title/thumbnail when there is one
                let mut shown_v = std::borrow::Cow::Borrowed(v);
                let mut card_variant = variant;
                if let Some(map) = dearrow.filter(|_| !card.portrait) {
                    match map.get(&v.id) {
                        Some(Some(b)) => {
                            if let Some(t) = &b.title {
                                shown_v.to_mut().title = Some(t.clone());
                            }
                            if let Some(t) = b.thumb_time {
                                card_variant = Variant::DeArrow((t * 1000.0) as u32);
                            }
                        }
                        Some(None) => {}
                        None if !v.is_playlist => want_branding.push(v.id.clone()),
                        None => {}
                    }
                }
                match video_card(ui, thumbs, card_variant, &shown_v, card) {
                    CardAction::Open => clicked = Some(v.clone()),
                    CardAction::Channel => open_channel = Some(v.clone()),
                    CardAction::Quick(q) => quick = Some((q, v.clone())),
                    CardAction::None => {}
                }
            };
            let info = |v: &Video, size: Vec2, thumb_h: f32, portrait: bool| CardInfo {
                size,
                thumb_h,
                playing: playing_id.as_deref() == Some(v.id.as_str()),
                progress: (!v.is_playlist)
                    .then(|| positions.get(&v.id))
                    .flatten()
                    .copied(),
                m,
                in_watch_later: source == Source::WatchLater,
                in_history: source == Source::History,
                download: dl_active
                    .get(&v.id)
                    .copied()
                    .or_else(|| downloads.get(&v.id).map(|_| 1.0)),
                portrait,
            };

            let mut scroll = egui::ScrollArea::vertical().auto_shrink(false);
            if std::mem::take(&mut tab.scroll_to_top) {
                scroll = scroll.vertical_scroll_offset(0.0);
            }
            ui.add_space(GAP * 0.5);
            scroll.show_viewport(ui, |ui, viewport| {
                let origin = ui.max_rect().min;
                ui.set_min_height(total);
                let mut y = 0.0;
                let mut last_visible = 0;
                for (i, row) in rows.iter().enumerate() {
                    let h = height(row);
                    let visible = y + h >= viewport.min.y && y <= viewport.max.y;
                    if visible {
                        last_visible = i;
                        let rect = egui::Rect::from_min_size(
                            origin + Vec2::new(GAP, y),
                            Vec2::new(avail_w, h),
                        );
                        ui.scope_builder(
                            egui::UiBuilder::new()
                                .max_rect(rect)
                                .layout(Layout::left_to_right(Align::Min)),
                            |ui| {
                                ui.spacing_mut().item_spacing.x = GAP;
                                match row {
                                    Row::Videos(r) => {
                                        for &v in shown.iter().skip(r * cols).take(cols) {
                                            let c =
                                                info(v, Vec2::new(card_w, card_h), thumb_h, false);
                                            draw(ui, v, c, variant);
                                        }
                                    }
                                    Row::Shelf => {
                                        ui.vertical(|ui| {
                                            ui.spacing_mut().item_spacing.y = 0.0;
                                            ui.allocate_ui_with_layout(
                                                Vec2::new(avail_w, SHELF_HEAD),
                                                Layout::left_to_right(Align::Center),
                                                |ui| {
                                                    ui.label(
                                                        RichText::new(t("Shorts"))
                                                            .size(17.0)
                                                            .strong()
                                                            .color(text()),
                                                    );
                                                    ui.label(
                                                        RichText::new(format!("{}", shorts.len()))
                                                            .color(text_dim()),
                                                    );
                                                },
                                            );
                                            egui::ScrollArea::horizontal()
                                                .id_salt("shorts")
                                                .auto_shrink([false, true])
                                                .show(ui, |ui| {
                                                    ui.horizontal(|ui| {
                                                        ui.spacing_mut().item_spacing.x =
                                                            GAP * 0.75;
                                                        for &v in &shorts {
                                                            let c = info(
                                                                v,
                                                                Vec2::new(short_w, short_h),
                                                                short_thumb_h,
                                                                true,
                                                            );
                                                            draw(ui, v, c, Variant::Portrait);
                                                        }
                                                    });
                                                });
                                        });
                                    }
                                    Row::Head => {
                                        ui.label(
                                            RichText::new(t("For you"))
                                                .size(17.0)
                                                .strong()
                                                .color(text()),
                                        );
                                        ui.with_layout(
                                            Layout::right_to_left(Align::Center),
                                            |ui| {
                                                if ui
                                                    .add(
                                                        egui::Label::new(
                                                            RichText::new(format!(
                                                                "{}  ›",
                                                                t("Show all")
                                                            ))
                                                            .color(text_dim()),
                                                        )
                                                        .sense(Sense::click()),
                                                    )
                                                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                                                    .clicked()
                                                {
                                                    show_all = true;
                                                }
                                            },
                                        );
                                    }
                                    Row::Topic(i) => {
                                        let (title, token, vids) = &topic_rows[*i];
                                        ui.vertical(|ui| {
                                            ui.spacing_mut().item_spacing.y = 0.0;
                                            ui.allocate_ui_with_layout(
                                                Vec2::new(avail_w, SHELF_HEAD),
                                                Layout::left_to_right(Align::Center),
                                                |ui| {
                                                    let r = ui
                                                        .add(
                                                            egui::Label::new(
                                                                RichText::new(format!(
                                                                    "{title}  ›"
                                                                ))
                                                                .size(17.0)
                                                                .strong()
                                                                .color(text()),
                                                            )
                                                            .sense(Sense::click()),
                                                        )
                                                        .on_hover_cursor(
                                                            egui::CursorIcon::PointingHand,
                                                        )
                                                        .on_hover_text(t("See all"));
                                                    if r.clicked() {
                                                        open_topic = Some(Source::Topic {
                                                            title: title.clone(),
                                                            token: token.clone(),
                                                        });
                                                    }
                                                },
                                            );
                                            match vids {
                                                Some(vids) => {
                                                    egui::ScrollArea::horizontal()
                                                        .id_salt(("topic", title))
                                                        .auto_shrink([false, true])
                                                        .show(ui, |ui| {
                                                            ui.horizontal(|ui| {
                                                                ui.spacing_mut().item_spacing.x =
                                                                    GAP;
                                                                for &v in vids {
                                                                    let c = info(
                                                                        v,
                                                                        Vec2::new(card_w, card_h),
                                                                        thumb_h,
                                                                        false,
                                                                    );
                                                                    draw(ui, v, c, variant);
                                                                }
                                                            });
                                                        });
                                                }
                                                None => {
                                                    want_topics
                                                        .push((title.clone(), token.clone()));
                                                    ui.horizontal(|ui| {
                                                        ui.spacing_mut().item_spacing.x = GAP;
                                                        for _ in 0..cols {
                                                            let (r, _) = ui.allocate_exact_size(
                                                                Vec2::new(card_w, card_h),
                                                                Sense::hover(),
                                                            );
                                                            ui.painter().rect_filled(
                                                                r,
                                                                RADIUS,
                                                                card(),
                                                            );
                                                        }
                                                    });
                                                }
                                            }
                                        });
                                    }
                                    Row::Footer => {
                                        // spinner while the next page loads; retry on error
                                        ui.allocate_ui_with_layout(
                                            Vec2::new(avail_w, card_h),
                                            Layout::top_down(Align::Center),
                                            |ui| {
                                                ui.add_space(12.0);
                                                if let Some(e) =
                                                    tab.error.as_ref().filter(|_| !tab.loading)
                                                {
                                                    error_box(ui, e);
                                                    ui.add_space(8.0);
                                                    if accent_button(ui, t("Retry")).clicked() {
                                                        more = true;
                                                    }
                                                } else {
                                                    ui.spinner();
                                                }
                                            },
                                        );
                                    }
                                }
                            },
                        );
                    }
                    y += h + GAP;
                }
                // Infinite scroll: start fetching the next page when we're within
                // a couple of rows of the end, so it's usually there before you arrive.
                if !home
                    && last_visible + 3 >= rows.len()
                    && !tab.loading
                    && !tab.exhausted
                    && tab.error.is_none()
                {
                    more = true;
                }
            });
            // the list it was clicked in becomes the queue
            if let Some(c) = &clicked {
                let in_shelf = topic_rows
                    .iter()
                    .find_map(|(_, _, v)| v.as_ref().filter(|v| v.iter().any(|x| x.id == c.id)));
                let list = if c.is_short && shorts.iter().any(|v| v.id == c.id) {
                    &shorts
                } else if let Some(shelf) = in_shelf.filter(|_| !shown.iter().any(|x| x.id == c.id))
                {
                    shelf
                } else {
                    &shown
                };
                let ordered = std::ptr::eq(list, &shorts)
                    || matches!(source, Source::Playlist { .. } | Source::WatchLater);
                if ordered {
                    clicked_list = list
                        .iter()
                        .filter(|v| !v.is_playlist)
                        .map(|v| (*v).clone())
                        .collect();
                    clicked_label = if std::ptr::eq(list, &shorts) {
                        t("Shorts").to_string()
                    } else {
                        source.label()
                    };
                }
            }
        });

        // ----- apply actions -----
        if let Some(v) = clicked {
            if v.is_playlist {
                self.open_page(
                    &ctx,
                    Source::Playlist {
                        id: v.id.clone(),
                        title: v.title().to_string(),
                    },
                );
            } else {
                self.queue_label = clicked_label;
                self.play_from(clicked_list, &v);
            }
        }
        if let Some(v) = open_channel {
            if let Some(url) = v.channel_url.clone() {
                self.open_page(
                    &ctx,
                    Source::Channel {
                        url,
                        name: v.channel().to_string(),
                    },
                );
            }
        }
        if more {
            self.load_more(&ctx, self.active.clone());
        }
        for (title, token) in want_topics {
            self.load_topic_shelf(&ctx, title, token);
        }
        if show_all {
            self.feed_all = true;
            if let Some(tab) = self.tabs.get_mut(&Source::Feed) {
                tab.scroll_to_top = true;
            }
        }
        if let Some(src) = open_topic {
            self.tabs.remove(&src); // category tokens are short-lived; always fresh
            self.nav_back.push(self.active.clone());
            self.active = src.clone();
            self.load(&ctx, src, false);
        }
        match (&self.hover, hovered_video) {
            (Some((old, _)), Some(new)) if old.id == new.id => {}
            (_, Some(new)) => self.hover = Some((new, std::time::Instant::now())),
            (_, None) => self.hover = None,
        }
        self.maybe_prepare(&ctx);
        for id in want_branding {
            self.request_branding(&ctx, id);
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
            self.feed_all = false;
            self.nav_back.clear();
            self.active = t.clone();
            self.load(&ctx, t, false);
        }
        if refresh {
            self.load(&ctx, self.active.clone(), true);
        }
        if reimport_from_empty {
            self.import_error = None;
            self.safari_blocked = signin::safari_needs_access();
            self.sessions = signin::sessions();
            self.session_pick = 0;
            self.screen = Screen::Onboarding;
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum SettingsTab {
    #[default]
    Playback,
    SponsorBlock,
    Feed,
    Appearance,
    Language,
    Storage,
    Account,
}

impl SettingsTab {
    /// Watching → what you see → how it looks → disk → account/app.
    const ALL: [SettingsTab; 7] = [
        SettingsTab::Playback,
        SettingsTab::SponsorBlock,
        SettingsTab::Feed,
        SettingsTab::Appearance,
        SettingsTab::Language,
        SettingsTab::Storage,
        SettingsTab::Account,
    ];
    fn label(self) -> &'static str {
        match self {
            SettingsTab::Playback => t("Playback"),
            SettingsTab::SponsorBlock => "SponsorBlock",
            SettingsTab::Feed => t("Feed"),
            SettingsTab::Appearance => t("Appearance"),
            SettingsTab::Language => t("Language & region"),
            SettingsTab::Storage => t("Storage"),
            SettingsTab::Account => t("Account & updates"),
        }
    }
}

/// Seconds the "Up next" card waits before playing the next video.
const AUTOPLAY_WAIT: f32 = 5.0;

/// How often to check, while the window is in front, that we're still signed in.
const SESSION_PROBE: Duration = Duration::from_secs(30 * 60);

enum SessionRefresh {
    Idle,
    Running,
    /// When the last attempt finished; we don't retry within 10 minutes.
    Done(std::time::Instant),
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
        let url = if v.is_playlist {
            format!("https://www.youtube.com/playlist?list={}", v.id)
        } else {
            v.url()
        };
        match q {
            Quick::CopyLink => {
                self.clip_seen = url.clone(); // our own copy: don't offer it back
                ctx.copy_text(url);
                self.status = Some((t("Link copied.").into(), false));
            }
            Quick::OpenInBrowser => {
                if let Err(e) = open_url(&url) {
                    self.status = Some((tf("Couldn't open the browser: {}", &[&e]), true));
                }
            }
            Quick::Download => self.start_download(ctx, v),
            Quick::PlayNext | Quick::AddToQueue => {
                self.up_next.retain(|x| x.id != v.id);
                if q == Quick::PlayNext {
                    self.up_next.push_front(v.clone());
                } else {
                    self.up_next.push_back(v.clone());
                }
                self.status = Some((
                    if self.playing_id().is_some() {
                        tn(
                            "{} video in your queue",
                            "{} videos in your queue",
                            self.up_next.len() as u64,
                        )
                    } else {
                        t("Queued. It plays after the next video you start.").into()
                    },
                    false,
                ));
            }
            Quick::SaveTo => self.open_save(ctx, v.clone()),
            Quick::RemoveFromHistory => {
                if let Some(tab) = self.tabs.get_mut(&Source::History) {
                    tab.videos.retain(|x| x.id != v.id);
                }
                let (tx, cookies, id) = (self.tx.clone(), self.cookies.clone(), v.id.clone());
                std::thread::spawn(move || {
                    let msg = match crate::innertube::remove_from_history(&cookies, &id) {
                        Ok(_) => (t("Removed from your YouTube history.").to_string(), false),
                        Err(e) => (
                            tf("Couldn't remove it from history: {}", &[&format!("{e:#}")]),
                            true,
                        ),
                    };
                    let _ = tx.send(Msg::Status(msg.0, msg.1));
                });
            }
            Quick::DeleteDownload => {
                self.downloads.remove(&v.id);
                self.status = Some((t("Download deleted.").into(), false));
                self.load(ctx, Source::Downloads, true);
            }
            Quick::MuteChannel => {
                if let Some(ch) = v.channel.clone() {
                    self.status = Some((
                        tf(
                            "Hiding {} in your feeds and search (“Muted” above the feed to undo).",
                            &[&ch],
                        ),
                        false,
                    ));
                    if !self.settings.muted_channels.contains(&ch) {
                        self.settings.muted_channels.push(ch);
                    }
                }
            }
            Quick::Watched(true) => self.positions.mark_watched(&v.id, v.duration),
            Quick::Watched(false) => self.positions.forget(&v.id),
            Quick::WatchLater(add) => {
                let (tx, ctx, cookies, id, title) = (
                    self.tx.clone(),
                    ctx.clone(),
                    self.cookies.clone(),
                    v.id.clone(),
                    v.title().to_string(),
                );
                std::thread::spawn(move || {
                    let r = crate::innertube::set_watch_later(&cookies, &id, add)
                        .map(|()| {
                            if add {
                                tf("Saved to Watch later: {}", &[&title])
                            } else {
                                tf("Removed from Watch later: {}", &[&title])
                            }
                        })
                        .map_err(|e| tf("Watch later: {}", &[&format!("{e:#}")]));
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
        self.nav_back
            .push(std::mem::replace(&mut self.active, page.clone()));
        self.load(ctx, page, false);
    }

    /// Back / title / Videos-Playlists switch for channel and playlist pages.
    /// Feed filters as chips above Home, Subscriptions and search results.
    fn ui_filter_bar(&mut self, ui: &mut egui::Ui) {
        let feed = matches!(
            self.active,
            Source::Feed | Source::Subscriptions | Source::Music | Source::Topic { .. }
        );
        if !(feed || matches!(self.active, Source::Search(_))) {
            return;
        }
        let mut go: Option<Source> = None;
        egui::Panel::top("filters")
            .frame(egui::Frame::new().fill(bg()).inner_margin(Margin {
                left: 16,
                right: 16,
                top: 8,
                bottom: 2,
            }))
            .show_separator_line(false)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    // inside a category opened from Home: the way back, and its name
                    let sub_title = match &self.active {
                        Source::Topic { title, .. } => Some(title.clone()),
                        Source::Feed if self.feed_all => Some(t("For you").to_string()),
                        _ => None,
                    };
                    if let Some(title) = sub_title {
                        if topic_chip(ui, t("←  Home"), false).clicked() {
                            go = Some(Source::Feed);
                        }
                        ui.label(RichText::new(title).size(17.0).strong().color(text()));
                        ui.add_space(10.0);
                    }
                    let s = &mut self.settings;
                    if chip(ui, t("Shorts"), s.show_shorts)
                        .on_hover_text(t("Shorts in their own row"))
                        .clicked()
                    {
                        s.show_shorts = !s.show_shorts;
                    }
                    if chip(ui, t("Live"), !s.hide_live)
                        .on_hover_text(t("Live streams and premieres"))
                        .clicked()
                    {
                        s.hide_live = !s.hide_live;
                    }
                    if feed
                        && chip(ui, t("Watched"), !s.hide_watched)
                            .on_hover_text(t("Videos you've already watched"))
                            .clicked()
                    {
                        s.hide_watched = !s.hide_watched;
                    }
                    if chip(ui, t("Less clickbait"), s.dearrow)
                        .on_hover_text(t("Community titles and thumbnails (DeArrow, experimental)"))
                        .clicked()
                    {
                        s.dearrow = !s.dearrow;
                    }
                    let n = s.muted_channels.len() + s.muted_words.len();
                    let label = if n == 0 {
                        format!("{} ▾", t("Muted"))
                    } else {
                        format!("{} · {n} ▾", t("Muted"))
                    };
                    let r = chip(ui, &label, n > 0);
                    egui::Popup::menu(&r)
                        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
                        .show(|ui| {
                            ui.set_width(300.0);
                            ui.label(RichText::new(t("Hide titles containing")).color(text_dim()));
                            if ui
                                .add(
                                    egui::TextEdit::singleline(&mut self.muted_words_buf)
                                        .hint_text(t("reaction, podcast clip"))
                                        .desired_width(f32::INFINITY),
                                )
                                .changed()
                            {
                                s.muted_words = self
                                    .muted_words_buf
                                    .split(',')
                                    .map(|w| w.trim().to_string())
                                    .filter(|w| !w.is_empty())
                                    .collect();
                            }
                            ui.add_space(6.0);
                            ui.label(RichText::new(t("Hidden channels")).color(text_dim()));
                            if s.muted_channels.is_empty() {
                                ui.label(
                                    RichText::new(t(
                                        "None yet. Use “Hide this channel” in a video's ⋯ menu.",
                                    ))
                                    .small()
                                    .color(text_dim()),
                                );
                            }
                            let mut unmute = None;
                            ui.horizontal_wrapped(|ui| {
                                for (i, ch) in s.muted_channels.iter().enumerate() {
                                    if ui
                                        .button(format!("{ch}  ✕"))
                                        .on_hover_text(t("Show again"))
                                        .clicked()
                                    {
                                        unmute = Some(i);
                                    }
                                }
                            });
                            if let Some(i) = unmute {
                                s.muted_channels.remove(i);
                            }
                        });
                });
            });
        // Esc / mouse back button leave a category too
        if (matches!(self.active, Source::Topic { .. })
            || (self.active == Source::Feed && self.feed_all))
            && !ui.ctx().egui_wants_keyboard_input()
            && ui.ctx().input(|i| {
                i.key_pressed(Key::Escape) || i.pointer.button_clicked(egui::PointerButton::Extra1)
            })
        {
            go = Some(Source::Feed);
        }
        if let Some(src) = go {
            let ctx = ui.ctx().clone();
            self.feed_all = false;
            self.nav_back.clear();
            self.active = src.clone();
            self.load(&ctx, src, false);
        }
    }

    fn ui_page_header(&mut self, ui: &mut egui::Ui) {
        if !self.active.is_page() {
            return;
        }
        let ctx = ui.ctx().clone();
        let mut back = false;
        let mut switch: Option<Source> = None;
        egui::Panel::top("page")
            .frame(egui::Frame::new().fill(bg()).inner_margin(Margin {
                left: 16,
                right: 16,
                top: 12,
                bottom: 4,
            }))
            .show_separator_line(false)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    if ui
                        .add(egui::Button::new(RichText::new(t("←  Back")).size(15.0)).frame(false))
                        .clicked()
                    {
                        back = true;
                    }
                    ui.add_space(10.0);
                    let (kind, title) = match &self.active {
                        Source::Playlist { title, .. } => (t("Playlist"), title.clone()),
                        other => (t("Channel"), other.label()),
                    };
                    ui.label(RichText::new(kind).small().color(accent()));
                    ui.add(
                        egui::Label::new(RichText::new(title).size(20.0).strong().color(text()))
                            .truncate(),
                    );
                    if let Source::Channel { url, name } | Source::ChannelPlaylists { url, name } =
                        &self.active
                    {
                        let (url, name) = (url.clone(), name.clone());
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            let on_lists = matches!(self.active, Source::ChannelPlaylists { .. });
                            if tab_button(ui, t("Playlists"), on_lists).clicked() && !on_lists {
                                switch = Some(Source::ChannelPlaylists {
                                    url: url.clone(),
                                    name: name.clone(),
                                });
                            }
                            if tab_button(ui, t("Videos"), !on_lists).clicked() && on_lists {
                                switch = Some(Source::Channel { url, name });
                            }
                        });
                    }
                });
            });
        // Esc / mouse back button also go back
        let key_back = !ctx.egui_wants_keyboard_input()
            && ctx.input(|i| {
                i.key_pressed(Key::Escape) || i.pointer.button_clicked(egui::PointerButton::Extra1)
            });
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
                ui.label(RichText::new(t("Sign in with Google")).size(16.0).strong().color(text()));
                ui.label(RichText::new(t("Needs Firefox or a Chromium-based browser (Chrome, Brave, Edge…).")).color(text_dim()));
                return;
            }
            if let Some(s) = &self.signin {
                let b = s.session.browser;
                if !s.opened {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(RichText::new(tf("Checking {}…", &[&s.session.label()])).color(text()));
                    });
                    return;
                }
                ui.label(RichText::new(tf("Sign in to Google in {}", &[&b])).size(16.0).strong().color(text()));
                ui.label(
                    RichText::new(tf(
                        "We opened Google's sign-in page in {0}. Once you're signed in, ytmini picks the session up by itself — leave {0} open.",
                        &[&b],
                    ))
                    .color(text_dim()),
                );
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(RichText::new(t("Waiting for sign-in…")).color(text_dim()));
                    ui.add_space(8.0);
                    if accent_button(ui, t("I've signed in")).clicked() {
                        act = GoogleAction::CheckNow;
                    }
                    if ui.button(t("Cancel")).clicked() {
                        act = GoogleAction::Cancel;
                    }
                });
                return;
            }
            let pick = self.session_pick.min(self.sessions.len() - 1);
            let chosen = &self.sessions[pick];
            // title + button on one row; description below at full width
            ui.horizontal(|ui| {
                ui.label(RichText::new(t("Sign in with Google")).size(16.0).strong().color(text()));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.add_enabled(!busy, accent_btn(t("Sign in"))).clicked() {
                        act = GoogleAction::Start;
                    }
                });
            });
            ui.add_space(4.0);
            let mut note = tf(
                "Uses your {} session. If you're not signed in there yet, Google's sign-in page opens in it.",
                &[&chosen.browser],
            );
            if !chosen.firefox && !chosen.safari && cfg!(target_os = "macos") {
                note.push(' ');
                note.push_str(t("macOS asks for Keychain access once — click Allow."));
            }
            ui.add(egui::Label::new(RichText::new(note).size(12.5).color(text_dim())).wrap());
            if self.sessions.len() > 1 {
                ui.add_space(10.0);
                let labels: Vec<String> = self.sessions.iter().map(|s| s.label()).collect();
                crate::theme::dropdown(ui, &labels[pick], 200.0, 360.0, |ui| {
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
        self.signin = Some(SignIn {
            session,
            opened: false,
            checking: false,
            last_check: now,
            started: now,
        });
        self.spawn_signin_check(ctx);
    }

    fn spawn_signin_check(&mut self, ctx: &egui::Context) {
        let Some(s) = &mut self.signin else { return };
        s.checking = true;
        s.last_check = std::time::Instant::now();
        let (tx, ctx, session) = (self.tx.clone(), ctx.clone(), s.session.clone());
        std::thread::spawn(move || {
            let _ = tx.send(Msg::SignInCheck(
                session.has_login().map_err(|e| format!("{e:#}")),
            ));
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
            self.import_error = Some(
                t("Stopped waiting for the Google sign-in. Click Sign in to try again.").into(),
            );
            return;
        }
        if s.last_check.elapsed() >= SIGNIN_POLL {
            self.spawn_signin_check(ctx);
        }
        ctx.request_repaint_after(Duration::from_millis(500));
    }

    /// Google rotates part of the login in the browser; after a few days our
    /// copy stops signing in. Re-import from the browser we signed in with
    /// (or the first signed-in one) without bothering the user. Chromium
    /// browsers may show a Keychain prompt; "Always Allow" makes it silent.
    /// Any signed-in call (history reporting, Watch later, the periodic
    /// probe) that finds the session gone starts the refresh, not just feeds.
    fn watch_session(&mut self, ctx: &egui::Context) {
        if let Some((text, true)) = &self.status {
            if text.contains(crate::innertube::SESSION_EXPIRED) {
                self.status = Some((
                    t("Your YouTube session expired. Refreshing it from your browser…").into(),
                    false,
                ));
                self.auto_refresh_session(ctx);
            }
        }
        let focused = ctx.input(|i| i.focused);
        if focused && self.session_checked.elapsed() > SESSION_PROBE && yt::logged_in(&self.cookies)
        {
            self.session_checked = std::time::Instant::now();
            let (tx, ctx, cookies) = (self.tx.clone(), ctx.clone(), self.cookies.clone());
            std::thread::spawn(move || {
                if let Err(e) = crate::innertube::check_session(&cookies) {
                    let e = format!("{e:#}");
                    if e.contains(crate::innertube::SESSION_EXPIRED) {
                        let _ = tx.send(Msg::Status(e, true));
                        ctx.request_repaint();
                    }
                }
            });
        }
    }

    fn auto_refresh_session(&mut self, ctx: &egui::Context) {
        match self.session_refresh {
            SessionRefresh::Running => return,
            SessionRefresh::Done(t) if t.elapsed() < Duration::from_secs(600) => return,
            _ => {}
        }
        self.session_refresh = SessionRefresh::Running;
        let saved = self.settings.signin_source.clone();
        let (tx, ctx, cookies) = (self.tx.clone(), ctx.clone(), self.cookies.clone());
        std::thread::spawn(move || {
            let pick = saved
                .and_then(|s| signin::find(&s.id, &s.profile))
                .or_else(|| {
                    let all = signin::sessions();
                    // prefer browsers that don't need a Keychain prompt
                    let ok = |s: &signin::Session| s.has_login().unwrap_or(false);
                    all.iter()
                        .find(|s| (s.firefox || s.safari) && ok(s))
                        .or_else(|| all.iter().find(|s| ok(s)))
                        .cloned()
                });
            let r = match pick {
                None => Err(t("no signed-in browser found").to_string()),
                Some(s) => s
                    .export_cookies(&cookies)
                    .map(|()| (s.label(), s.id.to_string(), s.profile.clone()))
                    .map_err(|e| format!("{e:#}")),
            };
            let _ = tx.send(Msg::SessionRefreshed(r));
            ctx.request_repaint();
        });
    }

    /// Step 2: the profile is signed in → import a snapshot of its cookies.
    fn finish_signin(&mut self, ctx: &egui::Context) {
        let Some(s) = self.signin.take() else { return };
        self.importing = Some(s.session.label());
        self.import_error = None;
        self.import_source = Some((s.session.id.to_string(), s.session.profile.clone()));
        let (tx, ctx, cookies) = (self.tx.clone(), ctx.clone(), self.cookies.clone());
        std::thread::spawn(move || {
            let r = s
                .session
                .export_cookies(&cookies)
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(Msg::Imported(r));
            ctx.request_repaint();
        });
    }

    /// Beside the video: your queue (reorder, remove, clear), the rest of an
    /// ordered list you started from, then YouTube's suggestions. Returns a
    /// video to play now.
    fn ui_up_next(&mut self, ctx: &egui::Context, video: egui::Rect) -> Option<Video> {
        // Shown while the controls are (the mouse moved lately), while you
        // point at it, drag in it or have the Save window open; otherwise it
        // slides out to the right with the controls.
        let hovered = ctx
            .pointer_hover_pos()
            .zip(self.overlay_rect)
            .is_some_and(|(p, r)| r.contains(p));
        let shown = self.up_next_open
            && (self.controls_visible
                || hovered
                || self.queue_drag.is_some()
                || self.save.is_some());
        let lin = ctx.animate_bool_with_time(
            egui::Id::unique("up_next_fade"),
            shown,
            if shown { 0.25 } else { 0.45 },
        );
        let a = player::ease(lin);
        if a <= 0.0 || !video.is_positive() {
            self.overlay_rect = None;
            return None;
        }
        let mut play: Option<Video> = None;
        let w = 360.0f32.min(video.width() - 24.0);
        let slide = (1.0 - a) * 56.0;
        let rect = egui::Rect::from_min_max(
            egui::pos2(video.right() - w - 12.0 + slide, video.top() + 12.0),
            egui::pos2(
                video.right() - 12.0 + slide,
                video.bottom() - player::CONTROLS_H - 8.0,
            ),
        );
        egui::Area::new(egui::Id::unique("up_next_overlay"))
            .fixed_pos(rect.min)
            .order(egui::Order::Foreground)
            .interactable(shown)
            .show(ctx, |ui| {
                ui.multiply_opacity(a);
                ui.style_mut().visuals = egui::Visuals::dark();
                egui::Frame::new()
                    .fill(Color32::from_rgba_unmultiplied(14, 14, 16, 205))
                    .corner_radius(14)
                    .stroke(Stroke::new(1.0, Color32::from_white_alpha(18)))
                    .inner_margin(Margin::same(10))
                    .show(ui, |ui| {
                        let inner = rect.size() - Vec2::splat(20.0);
                        ui.set_min_size(inner);
                        ui.set_max_size(inner);
                        crate::theme::on_dark(|| {
                            ui.horizontal(|ui| {
                                for (tab, label) in [
                                    (PanelTab::UpNext, t("Up next")),
                                    (PanelTab::Comments, t("Description")),
                                ] {
                                    if pill(ui, label, self.panel_tab == tab).clicked() {
                                        self.panel_tab = tab;
                                    }
                                }
                            });
                            ui.add_space(6.0);
                            match self.panel_tab {
                                PanelTab::UpNext => play = self.up_next_body(ui),
                                PanelTab::Comments => self.comments_body(ui),
                            }
                        });
                    });
            });
        self.overlay_rect = Some(rect);
        if a < 1.0 {
            ctx.request_repaint();
        }
        play
    }

    /// The description card: title, channel with Subscribe, like / dislike /
    /// Save, views and date, then the description with clickable timestamps.
    /// Returns a position to seek to.
    fn ui_about_card(&mut self, ui: &mut egui::Ui, cur: &str) -> Option<f64> {
        use crate::innertube::Like;
        let ctx = ui.ctx().clone();
        let info = self
            .watch
            .as_ref()
            .filter(|(w, _)| w == cur)
            .map(|(_, i)| i.clone());
        let title = self
            .current_video()
            .map(|v| v.title().to_string())
            .unwrap_or_default();
        let mut seek = None;
        egui::Frame::new()
            .fill(Color32::from_white_alpha(14))
            .corner_radius(12)
            .inner_margin(Margin::same(12))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.add(
                    egui::Label::new(RichText::new(&title).size(15.0).strong().color(text()))
                        .wrap(),
                );
                let Some(info) = info else {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(RichText::new(t("Loading…")).color(text_dim()));
                    });
                    return;
                };
                ui.add_space(6.0);
                // channel · subscribers            [Subscribe]
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new(info.channel.clone().unwrap_or_default())
                                .size(13.5)
                                .strong()
                                .color(text()),
                        );
                        if let Some(n) = &info.subscribers {
                            ui.label(RichText::new(n).size(11.5).color(text_dim()));
                        }
                    });
                    if let (Some(cid), Some(sub)) = (info.channel_id.clone(), info.subscribed) {
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            let channel = info.channel.clone().unwrap_or_default();
                            let r =
                                pill(ui, if sub { t("Subscribed") } else { t("Subscribe") }, !sub);
                            if sub {
                                egui::Popup::menu(&r).show(|ui| {
                                    if ui.button(tf("Unsubscribe from {}", &[&channel])).clicked() {
                                        self.subscribe(&ctx, &cid, false);
                                    }
                                });
                            } else if r.clicked() {
                                self.subscribe(&ctx, &cid, true);
                            }
                        });
                    }
                });
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    if icon_pill(
                        ui,
                        Glyph::ThumbUp,
                        info.like_count.as_deref(),
                        info.like == Like::Like,
                    )
                    .on_hover_text(t("Like"))
                    .clicked()
                    {
                        self.rate(&ctx, Like::Like);
                    }
                    if icon_pill(ui, Glyph::ThumbDown, None, info.like == Like::Dislike)
                        .on_hover_text(t("Dislike"))
                        .clicked()
                    {
                        self.rate(&ctx, Like::Dislike);
                    }
                    // Save sits at the right edge, under Subscribe
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if pill(ui, &format!("＋ {}", t("Save")), false)
                            .on_hover_text(t("Save to playlist…"))
                            .clicked()
                        {
                            if let Some(v) = self.current_video() {
                                self.open_save(&ctx, v);
                            }
                        }
                    });
                });
                let meta: Vec<&str> = [info.views.as_deref(), info.date.as_deref()]
                    .into_iter()
                    .flatten()
                    .collect();
                if !meta.is_empty() {
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new(meta.join(" · "))
                            .size(12.0)
                            .strong()
                            .color(text_dim()),
                    );
                }
                let desc = self
                    .description
                    .as_ref()
                    .filter(|(d, _)| d == cur)
                    .map(|(_, d)| d.clone());
                if let Some(d) = desc.filter(|d| !d.trim().is_empty()) {
                    ui.add_space(4.0);
                    const FOLD: usize = 8;
                    let lines: Vec<&str> = d.lines().collect();
                    let long = lines.len() > FOLD + 2;
                    let shown = if long && !self.desc_expanded {
                        &lines[..FOLD]
                    } else {
                        &lines[..]
                    };
                    if let Some(t) = rich_text(ui, &shown.join("\n"), 13.0, text()) {
                        seek = Some(t);
                    }
                    if long {
                        let label = if self.desc_expanded {
                            t("Show less")
                        } else {
                            t("Show more")
                        };
                        if ui.link(RichText::new(label).strong()).clicked() {
                            self.desc_expanded = !self.desc_expanded;
                        }
                    }
                }
            });
        seek
    }

    /// Like / dislike (again: clear). Shown at once; rolled back if YouTube refuses.
    fn rate(&mut self, ctx: &egui::Context, like: crate::innertube::Like) {
        use crate::innertube::Like;
        let Some((id, info)) = self.watch.as_mut() else {
            return;
        };
        let prev = info.like;
        let new = if prev == like { Like::None } else { like };
        info.like = new;
        let (tx, cookies, id, ctx) = (
            self.tx.clone(),
            self.cookies.clone(),
            id.clone(),
            ctx.clone(),
        );
        std::thread::spawn(move || {
            if let Err(e) = crate::innertube::set_like(&cookies, &id, new) {
                let _ = tx.send(Msg::Rated(id, prev));
                let _ = tx.send(Msg::Status(
                    tf("Couldn't update YouTube: {}", &[&format!("{e:#}")]),
                    true,
                ));
                ctx.request_repaint();
            }
        });
    }

    fn subscribe(&mut self, ctx: &egui::Context, channel_id: &str, on: bool) {
        if let Some((_, w)) = self.watch.as_mut() {
            if w.channel_id.as_deref() == Some(channel_id) {
                w.subscribed = Some(on);
            }
        }
        let (tx, cookies, cid, ctx) = (
            self.tx.clone(),
            self.cookies.clone(),
            channel_id.to_string(),
            ctx.clone(),
        );
        std::thread::spawn(move || {
            if let Err(e) = crate::innertube::set_subscribed(&cookies, &cid, on) {
                let _ = tx.send(Msg::Subscribed(cid, !on));
                let _ = tx.send(Msg::Status(
                    tf("Couldn't update YouTube: {}", &[&format!("{e:#}")]),
                    true,
                ));
            }
            ctx.request_repaint();
        });
    }

    fn open_save(&mut self, ctx: &egui::Context, video: Video) {
        let (tx, cookies, id, ctx) = (
            self.tx.clone(),
            self.cookies.clone(),
            video.id.clone(),
            ctx.clone(),
        );
        self.save = Some(SaveDialog { video, list: None });
        std::thread::spawn(move || {
            let r = crate::innertube::playlists_for(&cookies, &id).map_err(|e| format!("{e:#}"));
            let _ = tx.send(Msg::Playlists(id, r));
            ctx.request_repaint();
        });
    }

    /// "Save to playlist": your playlists with a checkbox each.
    fn ui_save_dialog(&mut self, ctx: &egui::Context) {
        let Some(d) = self.save.as_mut() else { return };
        let mut open = true;
        let mut toggle: Option<(String, bool)> = None;
        egui::Window::new(t("Save to playlist"))
            .id(egui::Id::unique("save_to_playlist"))
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .order(egui::Order::Foreground)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.set_min_width(280.0);
                ui.add(
                    egui::Label::new(RichText::new(d.video.title()).color(text_dim())).truncate(),
                );
                ui.add_space(6.0);
                match &mut d.list {
                    None => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(t("Loading…"));
                        });
                    }
                    Some(Err(e)) => {
                        ui.label(
                            RichText::new(tf("Couldn't load your playlists: {}", &[e]))
                                .color(text_dim()),
                        );
                    }
                    Some(Ok(list)) => {
                        egui::ScrollArea::vertical()
                            .max_height(360.0)
                            .show(ui, |ui| {
                                for p in list.iter_mut() {
                                    let mut on = p.contains;
                                    if ui.checkbox(&mut on, &p.title).changed() {
                                        p.contains = on;
                                        toggle = Some((p.id.clone(), on));
                                    }
                                }
                            });
                    }
                }
            });
        if let Some((pid, add)) = toggle {
            let (tx, cookies, vid, ctx) = (
                self.tx.clone(),
                self.cookies.clone(),
                d.video.id.clone(),
                ctx.clone(),
            );
            std::thread::spawn(move || {
                if let Err(e) = crate::innertube::set_in_playlist(&cookies, &pid, &vid, add) {
                    let _ = tx.send(Msg::Status(
                        tf("Couldn't update YouTube: {}", &[&format!("{e:#}")]),
                        true,
                    ));
                    // show what's really there
                    let r = crate::innertube::playlists_for(&cookies, &vid)
                        .map_err(|e| format!("{e:#}"));
                    let _ = tx.send(Msg::Playlists(vid, r));
                }
                ctx.request_repaint();
            });
        }
        if !open || ctx.input(|i| i.key_pressed(Key::Escape)) {
            self.save = None;
        }
    }

    /// Start (or continue, `more`) loading the playing video's comments.
    fn load_comments(&mut self, ctx: &egui::Context, more: bool) {
        let Some(id) = self.playing_id().map(str::to_string) else {
            return;
        };
        if self.comments.id != id {
            self.comments = CommentsState {
                id: id.clone(),
                ..Default::default()
            };
        }
        if self.comments.loading {
            return;
        }
        let token = if more {
            self.comments.next.clone()
        } else {
            self.watch
                .as_ref()
                .filter(|(w, _)| *w == id)
                .and_then(|(_, i)| i.comments.clone())
        };
        let Some(token) = token else { return };
        self.comments.loading = true;
        self.comments.started = true;
        self.comments.error = None;
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let r = crate::innertube::comments(&token).map_err(|e| format!("{e:#}"));
            let _ = tx.send(Msg::Comments(id, r, more));
            ctx.request_repaint();
        });
    }

    /// The description card, then the comments (read-only, a page at a
    /// time as you scroll). Timestamps in either seek the video.
    fn comments_body(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let Some(cur) = self.playing_id().map(str::to_string) else {
            return;
        };
        if self.comments.id != cur {
            self.comments = CommentsState {
                id: cur.clone(),
                ..Default::default()
            };
        }
        let info = self
            .watch
            .as_ref()
            .filter(|(w, _)| *w == cur)
            .map(|(_, i)| i.clone());
        if !self.comments.started && info.as_ref().is_some_and(|i| i.comments.is_some()) {
            self.load_comments(&ctx, false);
        }
        let (mut more, mut retry, mut seek) = (false, false, None);
        egui::ScrollArea::vertical()
            .auto_shrink(false)
            .show(ui, |ui| {
                seek = self.ui_about_card(ui, &cur);
                ui.add_space(14.0);
                let c = &self.comments;
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(t("Comments"))
                            .size(15.0)
                            .strong()
                            .color(text()),
                    );
                    if let Some(n) = &c.count {
                        // "10 644 364 комментария": the number is enough here
                        let num: String = n.chars().take_while(|ch| !ch.is_alphabetic()).collect();
                        ui.label(RichText::new(num.trim()).color(text_dim()));
                    }
                });
                match &info {
                    None => return,
                    Some(i) if i.comments.is_none() => {
                        ui.label(RichText::new(t("Comments are turned off")).color(text_dim()));
                        return;
                    }
                    _ => {}
                }
                for cm in &c.items {
                    if let Some(t) = comment_row(ui, cm) {
                        seek = Some(t);
                    }
                }
                if c.loading {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(RichText::new(t("Loading…")).color(text_dim()));
                    });
                } else if let Some(e) = &c.error {
                    ui.label(
                        RichText::new(tf("Couldn't load comments: {}", &[e]))
                            .small()
                            .color(text_dim()),
                    );
                    if ui.link(t("Try again")).clicked() {
                        retry = true;
                    }
                } else if c.next.is_some() {
                    // the end came into view: next page
                    let r =
                        ui.allocate_response(Vec2::new(ui.available_width(), 1.0), Sense::hover());
                    if ui.is_rect_visible(r.rect) {
                        more = true;
                    }
                }
            });
        if let (Some(t), Some(p)) = (seek, self.embedded.as_mut()) {
            p.seek_absolute(t, true);
            p.poke();
        }
        if more {
            self.load_comments(&ctx, true);
        } else if retry {
            let more = !self.comments.items.is_empty();
            self.comments.error = None;
            self.load_comments(&ctx, more);
        }
    }

    fn up_next_body(&mut self, ui: &mut egui::Ui) -> Option<Video> {
        let mut play: Option<Video> = None;
        let mut retry_related = false;
        let cur = self.playing_id().map(str::to_string);
        let now_v = self
            .current_video()
            .filter(|v| self.now.as_ref().is_some_and(|n| n.id == v.id));
        // a song plays: suggest songs only
        let song_now = self.playing_song();
        let thumbs = &mut self.thumbs;
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 4.0;
                    // --- what you played before this, oldest first ---
                    if !self.played.is_empty() {
                        ui.label(RichText::new(t("Previously played")).size(15.0).strong().color(text()));
                        for v in &self.played {
                            if up_next_row(ui, thumbs, v, false).clicked {
                                play = Some(v.clone());
                            }
                        }
                        ui.add_space(10.0);
                    }
                    if let Some(now) = now_v.as_ref() {
                        let head = ui.label(RichText::new(t("Now playing")).size(15.0).strong().color(text()));
                        // open on "Now playing" with history above it
                        if self.up_next_anchor != now.id {
                            head.scroll_to_me(Some(Align::TOP));
                            self.up_next_anchor = now.id.clone();
                        }
                        up_next_row(ui, thumbs, now, true);
                        ui.add_space(10.0);
                    }
                    // --- your queue ---
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(t("Your queue")).size(15.0).strong().color(text()));
                        if !self.up_next.is_empty() {
                            ui.label(RichText::new(self.up_next.len().to_string()).color(text_dim()));
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if ui
                                    .add(egui::Button::new(RichText::new(t("Clear")).color(text_dim())).frame(false))
                                    .clicked()
                                {
                                    self.up_next.clear();
                                }
                            });
                        }
                    });
                    if self.up_next.is_empty() {
                        ui.label(
                            RichText::new(t("Empty. Add videos with “Play next” or “Add to queue” in a video's ⋯ menu, or with + below."))
                                .small()
                                .color(text_dim()),
                        );
                    }
                    // drag a row to reorder; ✕ on hover removes it
                    let mut remove: Option<usize> = None;
                    let n = self.up_next.len();
                    let mut rects = Vec::with_capacity(n);
                    for (i, v) in self.up_next.iter().enumerate() {
                        let r = up_next_row(ui, thumbs, v, false);
                        rects.push(r.rect);
                        if r.drag_started {
                            self.queue_drag = Some(i);
                        }
                        if r.clicked {
                            play = Some(v.clone());
                        }
                        if r.hovered && self.queue_drag.is_none() {
                            let b = egui::Rect::from_min_size(r.rect.right_top() + Vec2::new(-28.0, 4.0), Vec2::splat(22.0));
                            if round_btn(ui, b, "✕").on_hover_text(t("Remove")).clicked() {
                                remove = Some(i);
                            }
                        }
                    }
                    if let Some(i) = remove {
                        self.up_next.remove(i);
                    }
                    if let Some(from) = self.queue_drag.filter(|&f| f < n) {
                        let (pointer, released) =
                            ui.input(|i| (i.pointer.interact_pos(), i.pointer.any_released() || !i.pointer.any_down()));
                        if let Some(p) = pointer {
                            // drop slot 0..=n: before the first row whose middle is below the pointer
                            let to = rects.iter().position(|r| p.y < r.center().y).unwrap_or(n);
                            let painter = ui.painter();
                            painter.rect_filled(rects[from], 8, Color32::from_black_alpha(140));
                            if to != from && to != from + 1 {
                                let y = if to < n { rects[to].top() - 2.0 } else { rects[n - 1].bottom() + 2.0 };
                                painter.hline(rects[from].x_range(), y, Stroke::new(3.0, accent()));
                            }
                            ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
                            if released {
                                if let Some(item) = self.up_next.remove(from) {
                                    let to = if to > from { to - 1 } else { to };
                                    self.up_next.insert(to.min(self.up_next.len()), item);
                                }
                                self.queue_drag = None;
                            }
                        } else if released {
                            self.queue_drag = None;
                        }
                    } else {
                        self.queue_drag = None;
                    }
                    // --- the rest of an ordered list ---
                    if let Some(pos) = self.queue_pos {
                        let rest: Vec<&Video> = self.queue_ctx.iter().skip(pos + 1).take(30).collect();
                        if !rest.is_empty() {
                            ui.add_space(10.0);
                            ui.label(
                                RichText::new(tf("Next in {}", &[&self.queue_label]))
                                    .size(15.0)
                                    .strong()
                                    .color(text()),
                            );
                            for v in rest {
                                if up_next_row(ui, thumbs, v, false).clicked {
                                    play = Some(v.clone());
                                }
                            }
                        }
                    }
                    // --- YouTube's suggestions ---
                    ui.add_space(10.0);
                    ui.label(RichText::new(t("Suggestions")).size(15.0).strong().color(text()));
                    let suggestions: Vec<&Video> = if cur.as_deref() == Some(self.related.0.as_str()) {
                        self.related.1.iter().filter(|v| !v.is_playlist && (!song_now || v.is_music)).collect()
                    } else {
                        Vec::new()
                    };
                    let failed = self.related_err.as_ref().filter(|(id, _)| cur.as_deref() == Some(id.as_str()));
                    if suggestions.is_empty() {
                        if let Some((_, e)) = failed {
                            ui.label(
                                RichText::new(tf("Couldn't load suggestions: {}", &[e]))
                                    .small()
                                    .color(text_dim()),
                            );
                            if ui.link(t("Try again")).clicked() {
                                retry_related = true;
                            }
                        } else {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.label(RichText::new(t("Loading…")).color(text_dim()));
                            });
                        }
                    }
                    let mut add: Option<Video> = None;
                    for v in suggestions {
                        let r = up_next_row(ui, thumbs, v, false);
                        if r.clicked {
                            play = Some(v.clone());
                        }
                        if r.hovered {
                            let b = egui::Rect::from_min_size(r.rect.right_top() + Vec2::new(-28.0, 4.0), Vec2::splat(22.0));
                            let queued = self.up_next.iter().any(|q| q.id == v.id);
                            let resp = round_btn(ui, b, if queued { "✓" } else { "+" })
                                .on_hover_text(t("Add to queue"));
                            if resp.clicked() && !queued {
                                add = Some(v.clone());
                            }
                        }
                    }
                    if let Some(v) = add {
                        self.up_next.push_back(v);
                    }
                });
        if retry_related {
            if let Some(id) = self.playing_id().map(str::to_string) {
                self.related_err = None;
                let (tx, cookies, ctx) = (self.tx.clone(), self.cookies.clone(), ui.ctx().clone());
                std::thread::spawn(move || {
                    fetch_related(&tx, &cookies, &id);
                    ctx.request_repaint();
                });
            }
        }
        play
    }

    fn ui_player(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let fullscreen = ctx.input(|i| i.viewport().fullscreen.unwrap_or(false));
        let mut back = false;
        let mut toggle_fs = false;
        let (mut go_next, mut go_prev) = (false, false);

        // Keyboard (only when no text field / popup has focus), YouTube's keys
        if !ctx.egui_wants_keyboard_input() {
            use egui::Modifiers as M;
            let mut keys: Vec<(M, Key)> = Vec::new();
            ctx.input_mut(|i| {
                for (m, k) in [
                    (M::NONE, Key::Space),
                    (M::NONE, Key::K),
                    (M::NONE, Key::J),
                    (M::NONE, Key::L),
                    (M::NONE, Key::ArrowLeft),
                    (M::NONE, Key::ArrowRight),
                    (M::NONE, Key::ArrowUp),
                    (M::NONE, Key::ArrowDown),
                    (M::NONE, Key::M),
                    (M::NONE, Key::F),
                    (M::NONE, Key::Escape),
                    (M::SHIFT, Key::N),
                    (M::SHIFT, Key::P),
                    (M::SHIFT, Key::Comma),
                    (M::SHIFT, Key::Period),
                    (M::NONE, Key::Comma),
                    (M::NONE, Key::Period),
                    (M::NONE, Key::Home),
                    (M::NONE, Key::End),
                    (M::SHIFT, Key::C),
                    (M::NONE, Key::C),
                    (M::SHIFT, Key::Questionmark),
                    (M::NONE, Key::Questionmark),
                    (M::SHIFT, Key::Slash),
                    (M::NONE, Key::Num0),
                    (M::NONE, Key::Num1),
                    (M::NONE, Key::Num2),
                    (M::NONE, Key::Num3),
                    (M::NONE, Key::Num4),
                    (M::NONE, Key::Num5),
                    (M::NONE, Key::Num6),
                    (M::NONE, Key::Num7),
                    (M::NONE, Key::Num8),
                    (M::NONE, Key::Num9),
                ] {
                    if i.consume_key(m, k) {
                        keys.push((m, k));
                    }
                }
            });
            for (m, k) in keys {
                let shift = m == M::SHIFT;
                match k {
                    Key::F => toggle_fs = true,
                    Key::Escape if self.up_next_open => self.up_next_open = false,
                    Key::Escape if fullscreen => toggle_fs = true,
                    Key::Escape => back = true,
                    Key::N if shift => go_next = true,
                    Key::P if shift => go_prev = true,
                    _ => {}
                }
                let Some(p) = &mut self.embedded else {
                    continue;
                };
                p.poke();
                match k {
                    Key::Space | Key::K => p.toggle_pause(),
                    Key::J => p.seek_relative(-10.0),
                    Key::L => p.seek_relative(10.0),
                    Key::ArrowLeft => p.seek_relative(-5.0),
                    Key::ArrowRight => p.seek_relative(5.0),
                    Key::ArrowUp => p.set_volume(p.volume + 5.0),
                    Key::ArrowDown => p.set_volume(p.volume - 5.0),
                    Key::M => p.toggle_mute(),
                    Key::Comma if shift => p.step_speed(false),
                    Key::Period if shift => p.step_speed(true),
                    Key::Comma => p.frame_step(false),
                    Key::Period => p.frame_step(true),
                    Key::Home => p.seek_absolute(0.0, true),
                    Key::End if p.duration > 1.0 => p.seek_absolute(p.duration - 1.0, true),
                    Key::C => p.jump_chapter(!shift),
                    Key::Questionmark | Key::Slash => p.show_help = !p.show_help,
                    _ => {
                        let n = [
                            Key::Num0,
                            Key::Num1,
                            Key::Num2,
                            Key::Num3,
                            Key::Num4,
                            Key::Num5,
                            Key::Num6,
                            Key::Num7,
                            Key::Num8,
                            Key::Num9,
                        ]
                        .iter()
                        .position(|&x| x == k);
                        if let Some(n) = n {
                            if p.duration > 0.0 {
                                p.seek_absolute(p.duration * n as f64 / 10.0, true);
                            }
                        }
                    }
                }
            }
        }

        let mut quality = None;
        let mut open_in_window = None;
        // queue: titles for the buttons, autoplay countdown at the end
        let q_next = self.peek_next().map(|v| v.title().to_string());
        let q_prev = self.played.last().map(|v| v.title().to_string());
        let mut autoplay_in = None;
        match &self.embedded {
            Some(p) if p.eof && p.started => {
                let since = *self.eof_at.get_or_insert_with(std::time::Instant::now);
                if self.settings.autoplay && !self.autoplay_cancelled && q_next.is_some() {
                    let wait = if self.current_video().is_some_and(|v| v.is_short) {
                        1.5
                    } else {
                        AUTOPLAY_WAIT
                    };
                    let left = wait - since.elapsed().as_secs_f32();
                    if left <= 0.0 {
                        go_next = true;
                    } else {
                        autoplay_in = Some(left);
                    }
                }
            }
            _ => self.eof_at = None,
        }
        let mut jump: Option<Video> = None;
        let mut video_rect = egui::Rect::NOTHING;
        let song_now = self.playing_song();
        let eq_now = self.eq_view();
        let mut eq_pick = None;
        if let (Some(p), Some(surface), Some(gl)) =
            (&mut self.embedded, &mut self.surface, frame.gl())
        {
            let view = player::ViewCtx {
                gl,
                surface,
                fullscreen,
                quality: self.settings.quality,
                screen_cap: self.screen_cap,
                accent: accent(),
                prev: q_prev,
                next: q_next,
                autoplay_in,
                panel_open: Some(self.up_next_open),
                song: song_now,
                eq: eq_now,
            };
            let act = p.ui(ui, view);
            go_next |= act.next;
            go_prev |= act.prev;
            if act.toggle_panel {
                self.up_next_open = !self.up_next_open;
                self.up_next_anchor.clear();
            }
            video_rect = act.video_rect.unwrap_or(egui::Rect::NOTHING);
            self.controls_visible = act.controls_visible;
            if act.cancel_autoplay {
                self.autoplay_cancelled = true;
            }
            back |= act.back;
            eq_pick = act.eq_pick;
            toggle_fs |= act.toggle_fullscreen;
            quality = act.quality;
            if act.open_in_window {
                open_in_window = Some(Video {
                    id: p.id.clone(),
                    title: Some(p.title.clone()),
                    ..Default::default()
                });
            }
        }

        if self.embedded.is_some() {
            if let Some(v) = self.ui_up_next(&ctx, video_rect) {
                jump = Some(v);
            }
        }
        if toggle_fs {
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(!fullscreen));
        }
        if let Some(pick) = eq_pick {
            self.pick_eq(pick);
        }
        if let Some(q) = quality {
            self.settings.quality = q;
            self.apply_profile_live(&ctx);
        }
        if let Some(v) = jump.filter(|_| !back) {
            self.jump_to(&v);
        } else if go_next && !back {
            self.go_next();
        } else if go_prev && !back {
            self.go_prev();
        }
        if back && open_in_window.is_none() && self.playing_song() {
            self.collapse_player(&ctx);
        } else if back || open_in_window.is_some() {
            self.close_embedded(&ctx, frame);
        }
        if let Some(v) = open_in_window {
            self.play_in_window(&v);
        }
    }

    /// Back on a song: keep it playing (audio only) in the bar under the pages.
    fn collapse_player(&mut self, ctx: &egui::Context) {
        self.mini = true;
        if let Some(p) = &mut self.embedded {
            p.set_background(true);
        }
        if ctx.input(|i| i.viewport().fullscreen.unwrap_or(false)) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
        }
    }

    /// The music bar: what's playing, prev / play-pause / next, a seek line,
    /// open the player again, stop. Plays on into the next song.
    fn ui_mini_bar(&mut self, ui: &mut egui::Ui, frame: &eframe::Frame) {
        const H: f32 = 64.0;
        let ctx = ui.ctx().clone();
        let mut expand = false;
        let mut close = false;
        let (mut go_next, mut go_prev) = (false, false);

        // the next song starts by itself (no countdown: it's music)
        let next_title = self.peek_next().map(|v| v.title().to_string());
        let prev_any = !self.played.is_empty();
        if let Some(p) = &self.embedded {
            if p.eof && p.started {
                let since = *self.eof_at.get_or_insert_with(std::time::Instant::now);
                if self.settings.autoplay && !self.autoplay_cancelled && next_title.is_some() {
                    if since.elapsed() >= Duration::from_millis(800) {
                        go_next = true;
                    } else {
                        ctx.request_repaint_after(Duration::from_millis(100));
                    }
                }
            } else {
                self.eof_at = None;
            }
        }

        // a floating card, centred under the pages (right of the sidebar)
        let win = ctx.content_rect();
        let narrow = win.width() < 760.0;
        let side = if self.settings.sidebar_collapsed || narrow {
            60.0
        } else {
            208.0
        };
        let lane = egui::Rect::from_min_max(egui::pos2(win.left() + side, win.top()), win.max);
        let w = (lane.width() - 32.0).min(620.0);
        let card = egui::Rect::from_center_size(
            egui::pos2(lane.center().x, lane.bottom() - 16.0 - H / 2.0),
            Vec2::new(w, H),
        );
        let eqv = self.eq_view();
        let mut eq_pick = None;
        // no picture while collapsed: give the video texture's VRAM back
        if let (Some(s), Some(gl)) = (&mut self.surface, frame.gl()) {
            s.shrink(gl);
        }
        let Some(p) = &mut self.embedded else { return };
        egui::Area::new(egui::Id::unique("music_card"))
            .order(egui::Order::Foreground)
            .fixed_pos(card.min)
            .show(&ctx, |ui| {
                let (card, _) = ui.allocate_exact_size(card.size(), Sense::hover());
                let painter = ui.painter().clone();
                painter.add(
                    egui::epaint::Shadow {
                        offset: [0, 6],
                        blur: 24,
                        spread: 0,
                        color: Color32::from_black_alpha(90),
                    }
                    .as_shape(card, 16),
                );
                painter.rect(
                    card,
                    16,
                    panel(),
                    Stroke::new(1.0, card_hover()),
                    StrokeKind::Inside,
                );
                let cy = card.center().y - 2.0;
                let btn = |ui: &mut egui::Ui, rect: egui::Rect, id: &str, enabled: bool| {
                    let r = ui.interact(rect, ui.scope_id().with(id), Sense::click());
                    if enabled && r.hovered() {
                        ui.painter()
                            .circle_filled(rect.center(), rect.width() / 2.0, card_hover());
                    }
                    if enabled {
                        r.on_hover_cursor(egui::CursorIcon::PointingHand)
                    } else {
                        r
                    }
                };

                // right to left: ✕ stop, ⌃ open, ⏭ ⏯ ⏮
                let r = |x: f32, d: f32| {
                    egui::Rect::from_center_size(egui::pos2(x, cy), Vec2::splat(d))
                };
                let close_r = r(card.right() - 26.0, 32.0);
                let open_r = r(close_r.center().x - 34.0, 32.0);
                // EQ menu (songs), left of ⌃
                let eq_w = if eqv.is_some() { 92.0 } else { 0.0 };
                let eq_r = egui::Rect::from_min_max(
                    egui::pos2(open_r.left() - 6.0 - eq_w, cy - 14.0),
                    egui::pos2(open_r.left() - 6.0, cy + 14.0),
                );
                let next_r = r(eq_r.left() - 26.0, 38.0);
                let pp_r = r(next_r.center().x - 42.0, 42.0);
                let prev_r = r(pp_r.center().x - 42.0, 38.0);

                // left: square cover, ♫ title, channel (click: back to the player)
                let thumb = egui::Rect::from_min_size(
                    egui::pos2(card.left() + 10.0, cy - 22.0),
                    Vec2::splat(44.0),
                );
                painter.rect_filled(thumb, 10, thumb_bg());
                if let Some(tex) = self.thumbs.get(&p.id, Variant::Standard) {
                    // the middle square of the 16:9 frame
                    egui::Image::from_texture(egui::load::SizedTexture::from_handle(tex))
                        .uv(egui::Rect::from_min_max(
                            egui::pos2(0.21875, 0.0),
                            egui::pos2(0.78125, 1.0),
                        ))
                        .corner_radius(10)
                        .paint_at(ui, thumb);
                }
                let text_left = thumb.right() + 12.0;
                let text_right = prev_r.left() - 8.0;
                let info = egui::Rect::from_min_max(
                    egui::pos2(card.left() + 4.0, card.top() + 4.0),
                    egui::pos2(text_right, card.bottom() - 8.0),
                );
                let ir = ui.interact(info, ui.scope_id().with("music_info"), Sense::click());
                if ir
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .on_hover_text(t("Show the player"))
                    .clicked()
                {
                    expand = true;
                }
                if text_right - text_left > 30.0 {
                    let one = |s: &str, size: f32, col: Color32, width: f32| {
                        let mut job = LayoutJob::single_section(
                            s.to_string(),
                            TextFormat {
                                font_id: FontId::proportional(size),
                                color: col,
                                ..Default::default()
                            },
                        );
                        job.wrap.max_width = width.max(10.0);
                        job.wrap.max_rows = 1;
                        job.wrap.break_anywhere = true;
                        job.wrap.overflow_character = Some('…');
                        painter.layout_job(job)
                    };
                    crate::theme::paint_note(
                        &painter,
                        egui::pos2(text_left + 5.0, cy - 9.0),
                        0.55,
                        accent(),
                    );
                    let tg = one(&p.title, 13.5, text(), text_right - text_left - 15.0);
                    painter.galley(
                        egui::pos2(text_left + 15.0, cy - 9.0 - tg.size().y / 2.0),
                        tg,
                        text(),
                    );
                    let channel = match &self.watch {
                        Some((id, w)) if self.np_artist.is_empty() && *id == p.id => {
                            w.channel.clone().unwrap_or_default()
                        }
                        _ => self.np_artist.clone(),
                    };
                    let cg = one(&channel, 12.0, text_dim(), text_right - text_left);
                    painter.galley(egui::pos2(text_left, cy + 2.0), cg, text_dim());
                }

                // transport
                let dim = text_dim().gamma_multiply(0.45);
                let can_prev = prev_any || p.pos > 3.0;
                if btn(ui, prev_r, "music_prev", can_prev)
                    .on_hover_text(t("Previous"))
                    .clicked()
                {
                    go_prev = true;
                }
                player::skip_icon(
                    &painter,
                    prev_r.center(),
                    false,
                    if can_prev { text() } else { dim },
                );
                if btn(ui, pp_r, "music_pp", true)
                    .on_hover_text(t("Play / pause"))
                    .clicked()
                {
                    if p.eof {
                        p.seek_absolute(0.0, true);
                    }
                    p.toggle_pause();
                }
                player::play_pause_icon(&painter, pp_r.center(), p.paused || p.eof, text());
                let has_next = next_title.is_some();
                let nr = btn(ui, next_r, "music_next", has_next);
                let nr = match &next_title {
                    Some(tn) => nr.on_hover_text(format!("{}: {tn}", t("Next"))),
                    None => nr,
                };
                if nr.clicked() && has_next {
                    go_next = true;
                }
                player::skip_icon(
                    &painter,
                    next_r.center(),
                    true,
                    if has_next { text() } else { dim },
                );

                if let Some(v) = &eqv {
                    ui.scope_builder(
                        egui::UiBuilder::new()
                            .max_rect(eq_r)
                            .layout(Layout::right_to_left(Align::Center)),
                        |ui| {
                            crate::theme::dropdown(
                                ui,
                                &format!("EQ · {}", v.current.label()),
                                0.0,
                                440.0,
                                |ui| {
                                    if let Some(pk) = crate::eq::menu(ui, v) {
                                        eq_pick = Some(pk);
                                    }
                                },
                            );
                        },
                    );
                }
                // ⌃ open the player, ✕ stop
                if btn(ui, open_r, "music_open", true)
                    .on_hover_text(t("Show the player"))
                    .clicked()
                {
                    expand = true;
                }
                let c = open_r.center();
                painter.line(
                    vec![
                        c + Vec2::new(-5.5, 2.5),
                        c + Vec2::new(0.0, -3.0),
                        c + Vec2::new(5.5, 2.5),
                    ],
                    Stroke::new(1.8, text()),
                );
                if btn(ui, close_r, "music_close", true)
                    .on_hover_text(t("Stop"))
                    .clicked()
                {
                    close = true;
                }
                let c = close_r.center();
                for (a, b) in [
                    (Vec2::new(-4.5, -4.5), Vec2::new(4.5, 4.5)),
                    (Vec2::new(-4.5, 4.5), Vec2::new(4.5, -4.5)),
                ] {
                    painter.line_segment([c + a, c + b], Stroke::new(1.8, text()));
                }

                // progress along the bottom edge; hover shows the time, click seeks
                if p.duration > 0.0 {
                    let line = egui::Rect::from_min_max(
                        egui::pos2(card.left() + 16.0, card.bottom() - 9.0),
                        egui::pos2(card.right() - 16.0, card.bottom() - 1.0),
                    );
                    let sr = ui.interact(
                        line,
                        ui.scope_id().with("music_seek"),
                        Sense::click_and_drag(),
                    );
                    if let Some(pos) = sr
                        .interact_pointer_pos()
                        .filter(|_| sr.clicked() || sr.dragged())
                    {
                        let f = ((pos.x - line.left()) / line.width()).clamp(0.0, 1.0) as f64;
                        p.seek_absolute(f * p.duration, true);
                    }
                    let thick = if sr.hovered() || sr.dragged() {
                        4.0
                    } else {
                        2.5
                    };
                    let y = line.center().y;
                    let track = egui::Rect::from_min_max(
                        egui::pos2(line.left(), y - thick / 2.0),
                        egui::pos2(line.right(), y + thick / 2.0),
                    );
                    painter.rect_filled(track, 2, Color32::from_gray(128).gamma_multiply(0.3));
                    let f = (p.pos / p.duration).clamp(0.0, 1.0) as f32;
                    painter.rect_filled(
                        egui::Rect::from_min_size(track.min, Vec2::new(track.width() * f, thick)),
                        2,
                        accent(),
                    );
                    // the time under the pointer, in a label just above it
                    if let Some(hp) = sr.hover_pos() {
                        ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
                        let f = ((hp.x - line.left()) / line.width()).clamp(0.0, 1.0) as f64;
                        let tp = ctx.layer_painter(egui::LayerId::new(
                            egui::Order::Tooltip,
                            egui::Id::unique("music_seek_time"),
                        ));
                        let g = tp.layout_no_wrap(
                            format!(
                                "{} / {}",
                                player::fmt_time(f * p.duration),
                                player::fmt_time(p.duration)
                            ),
                            FontId::proportional(12.5),
                            text(),
                        );
                        let size = g.size() + Vec2::new(16.0, 8.0);
                        let x = (hp.x - size.x / 2.0).clamp(card.left(), card.right() - size.x);
                        let r = egui::Rect::from_min_size(
                            egui::pos2(x, card.top() - size.y - 6.0),
                            size,
                        );
                        tp.rect(
                            r,
                            8,
                            panel(),
                            Stroke::new(1.0, card_hover()),
                            StrokeKind::Inside,
                        );
                        tp.galley(r.min + Vec2::new(8.0, 4.0), g, text());
                    }
                }
                // keep the progress moving
                if !p.paused {
                    ctx.request_repaint_after(Duration::from_millis(250));
                }
            });

        if let Some(pk) = eq_pick {
            self.pick_eq(pk);
        }
        if close {
            self.close_embedded(&ctx, frame);
        } else if expand {
            self.mini = false;
            if let Some(p) = &mut self.embedded {
                p.set_background(false);
            }
        } else if go_next {
            self.go_next();
        } else if go_prev {
            self.go_prev();
        }
    }

    fn ui_settings(&mut self, ctx: &egui::Context) {
        let mut open = true;
        let mut clear = false;
        let mut reimport = false;
        let mut refresh_session = false;
        let mut check_updates = false;
        let mut clear_downloads = false;
        let before_quality = self.settings.quality;
        let tab = &mut self.settings_tab;
        egui::Window::new(t("Settings"))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .fixed_size(Vec2::new(600.0, 440.0))
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                let s = &mut self.settings;
                ui.horizontal_top(|ui| {
                    ui.vertical(|ui| {
                        ui.set_width(140.0);
                        ui.spacing_mut().item_spacing.y = 2.0;
                        for t in SettingsTab::ALL {
                            let on = *tab == t;
                            let r = ui.add_sized(
                                [140.0, 28.0],
                                egui::Button::selectable(on, RichText::new(t.label()).size(14.0).color(if on { text() } else { text_dim() })),
                            );
                            if r.clicked() {
                                *tab = t;
                            }
                        }
                    });
                    ui.separator();
                    egui::ScrollArea::vertical().id_salt(("settings", *tab as u8)).auto_shrink(false).show(ui, |ui| {
                    ui.with_layout(Layout::top_down(Align::Min), |ui| {
                    ui.set_max_width(ui.available_width() - 8.0);
                    ui.add_space(4.0);
                    match *tab {
                        SettingsTab::Playback => {
                            section(ui, t("Playback"));
                            egui::Grid::new("playback").num_columns(2).spacing([16.0, 10.0]).show(ui, |ui| {
                                ui.label(t("Player"));
                                ui.horizontal(|ui| {
                                    ui.selectable_value(&mut s.player, PlayerMode::Embedded, t("In this window"));
                                    ui.selectable_value(&mut s.player, PlayerMode::Window, t("Separate window"));
                                });
                                ui.end_row();
                                ui.label(t("Quality"));
                                crate::theme::dropdown(ui, &s.quality.label(), 220.0, 400.0, |ui| {
                                    for q in yt::Quality::choices(self.screen_cap) {
                                        ui.selectable_value(&mut s.quality, q, q.label());
                                    }
                                });
                                ui.end_row();
                            });
                            ui.checkbox(&mut s.mark_watched, t("Add videos I watch to my YouTube history"));
                            ui.checkbox(&mut s.volume_leveling, t("Level volume (quiet voices up, loud parts down)"));
                            ui.checkbox(&mut s.eq_auto, t("Auto EQ for songs (a preset for the genre)"));
                            ui.label(RichText::new(t("The genre comes from Deezer: ytmini sends it the artist and song name only. A preset you pick in the player's EQ menu is kept for that artist.")).small().color(text_dim()));
                            ui.checkbox(&mut s.prepare_on_hover, t("Start videos faster (prepare the one you're pointing at)"));
                                ui.checkbox(&mut s.autoplay, t("Autoplay the next video in the queue"));
                            ui.label(
                                RichText::new(if cfg!(target_os = "macos") {
                                    tf(
                                        "Auto never exceeds what this screen can show ({}p). On battery and in Low Power Mode it plays 1080p30. Sharp VP9 is preferred everywhere: this Mac decodes it in hardware, so it costs no more battery than blurry H.264.",
                                        &[&self.screen_cap],
                                    )
                                } else {
                                    tf(
                                        "Auto never exceeds what this screen can show ({}p). On battery and in Battery Saver it plays 1080p30. Sharp VP9 is preferred: most current PCs decode it in hardware, so it costs no more battery than blurry H.264.",
                                        &[&self.screen_cap],
                                    )
                                })
                                .small()
                                .color(text_dim()),
                            );

                        }
                        SettingsTab::SponsorBlock => {
                            section(ui, t("SponsorBlock"));
                            ui.checkbox(&mut s.sponsorblock, t("Skip sponsor segments and other filler"));
                            if s.sponsorblock {
                                ui.indent("sb", |ui| {
                                    for (id, _, [r, g, b]) in crate::sponsorblock::CATEGORIES {
                                        let mut on = s.sb_categories.iter().any(|c| c == id);
                                        ui.horizontal(|ui| {
                                            let (dot, _) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
                                            ui.painter().circle_filled(dot.center(), 4.0, Color32::from_rgb(r, g, b));
                                            if ui.checkbox(&mut on, crate::sponsorblock::label(id)).changed() {
                                                s.sb_categories.retain(|c| c != id);
                                                if on {
                                                    s.sb_categories.push(id.to_string());
                                                }
                                            }
                                        });
                                    }
                                });
                            }
                            ui.label(
                                RichText::new(t("Segments come from the SponsorBlock community (sponsor.ajay.app). Only a short hash prefix of the video id is sent, never the id itself."))
                                    .small()
                                    .color(text_dim()),
                            );

                        }
                        SettingsTab::Feed => {
                            section(ui, t("Feed"));
                            ui.checkbox(&mut s.show_shorts, t("Shorts (in their own row)"));
                            ui.checkbox(&mut s.dearrow, t("Less clickbait: community titles and thumbnails (DeArrow, experimental)"));
                            ui.checkbox(&mut s.hide_live, t("Hide live streams and premieres"));
                            ui.checkbox(&mut s.hide_watched, t("Hide videos I've watched (Home and Subscriptions)"));
                            ui.horizontal(|ui| {
                                ui.label(t("Hide titles containing"));
                                if ui
                                    .add(
                                        egui::TextEdit::singleline(&mut self.muted_words_buf)
                                            .hint_text(t("reaction, podcast clip"))
                                            .desired_width(200.0),
                                    )
                                    .changed()
                                {
                                    s.muted_words = self
                                        .muted_words_buf
                                        .split(',')
                                        .map(|w| w.trim().to_string())
                                        .filter(|w| !w.is_empty())
                                        .collect();
                                }
                            });
                            if !s.muted_channels.is_empty() {
                                ui.label(t("Hidden channels"));
                                let mut unmute = None;
                                ui.horizontal_wrapped(|ui| {
                                    for (i, ch) in s.muted_channels.iter().enumerate() {
                                        if ui.button(format!("{ch}  ✕")).on_hover_text(t("Show again")).clicked() {
                                            unmute = Some(i);
                                        }
                                    }
                                });
                                if let Some(i) = unmute {
                                    s.muted_channels.remove(i);
                                }
                            }
                            ui.label(
                                RichText::new(t("Filters apply to Home, Subscriptions and search, never to channel or playlist pages. Hide a channel from a video's ⋯ menu."))
                                    .small()
                                    .color(text_dim()),
                            );

                        }
                        SettingsTab::Appearance => {
                            section(ui, t("Layout"));
                            ui.horizontal(|ui| {
                                ui.label(t("Grid"));
                                ui.selectable_value(&mut s.density, Density::Comfortable, t("Comfortable"));
                                ui.selectable_value(&mut s.density, Density::Compact, t("Compact"));
                            });
                            ui.checkbox(&mut s.sidebar_collapsed, t("Sidebar: icons only"));
                            ui.add_space(10.0);
                            section(ui, t("Thumbnails"));
                            ui.horizontal(|ui| {
                                ui.selectable_value(&mut s.thumbs, ThumbQuality::Auto, t("Auto"));
                                ui.selectable_value(&mut s.thumbs, ThumbQuality::Standard, t("Standard · 320p"));
                                ui.selectable_value(&mut s.thumbs, ThumbQuality::High, t("High · 640p"));
                            });
                            ui.label(RichText::new(if cfg!(target_os = "macos") { t("Standard ≈ 14 KB, high ≈ 60 KB each on disk. Auto picks high on sharp (Retina) screens.") } else { t("Standard ≈ 14 KB, high ≈ 60 KB each on disk. Auto picks high on sharp (high-DPI) screens.") }).small().color(text_dim()));

                            ui.add_space(10.0);
                            if self.tray.is_some() {
                                section(ui, t("Window"));
                                if cfg!(target_os = "macos") {
                                    ui.checkbox(&mut s.close_to_tray, t("Closing the window keeps ytmini in the menu bar"));
                                    ui.label(RichText::new(t("A playing video continues as audio only. Quit from the menu-bar icon or with ⌘Q.")).small().color(text_dim()));
                                } else {
                                    ui.checkbox(&mut s.close_to_tray, t("Closing the window keeps ytmini in the system tray"));
                                    ui.label(RichText::new(t("A playing video continues as audio only. Quit from the tray icon.")).small().color(text_dim()));
                                }
                                ui.add_space(10.0);
                            }
                        }
                        SettingsTab::Language => {
                            section(ui, t("Language"));
                            let sys = crate::i18n::system();
                            let auto = if cfg!(windows) { tf("Same as Windows ({})", &[&sys.native_name()]) } else { tf("Same as macOS ({})", &[&sys.native_name()]) };
                            let shown = s.language.map_or(auto.clone(), |l| l.native_name().to_string());
                            crate::theme::dropdown(ui, &shown, 240.0, 400.0, |ui| {
                                ui.selectable_value(&mut s.language, None, auto);
                                for l in crate::i18n::Lang::ALL {
                                    ui.selectable_value(&mut s.language, Some(l), l.native_name());
                                }
                            });
                            ui.label(RichText::new(t("The interface, and the language ytmini asks YouTube to use for its own text (categories, view counts, dates). Video titles stay as their authors wrote them.")).small().color(text_dim()));
                            ui.add_space(10.0);
                            section(ui, t("Region"));
                            let sysr = crate::i18n::system_region();
                            let auto = if cfg!(windows) { tf("Same as Windows ({})", &[&crate::i18n::regions::name(&sysr)]) } else { tf("Same as macOS ({})", &[&crate::i18n::regions::name(&sysr)]) };
                            let shown = s.region.as_deref().map_or(auto.clone(), |r| format!("{} ({r})", crate::i18n::regions::name(r)));
                            crate::theme::dropdown(ui, &shown, 240.0, 360.0, |ui| {
                                ui.selectable_value(&mut s.region, None, auto);
                                for (code, name) in crate::i18n::regions::REGIONS {
                                    ui.selectable_value(&mut s.region, Some(code.to_string()), format!("{name} ({code})"));
                                }
                            });
                            ui.label(RichText::new(t("What YouTube recommends and trends for, and how it ranks search results. Separate from the language.")).small().color(text_dim()));
                        }
                        SettingsTab::Storage => {
                            section(ui, t("Downloads"));
                            ui.horizontal(|ui| {
                                ui.label(t("Quality"));
                                let label = |h: u32| if h == 0 { t("Audio only").to_string() } else { format!("{h}p") };
                                crate::theme::dropdown(ui, &label(s.download_height), 110.0, 400.0, |ui| {
                                    for h in [480, 720, 1080, 1440, 0] {
                                        ui.selectable_value(&mut s.download_height, h, label(h));
                                    }
                                });
                                ui.label(t("Keep at most"));
                                ui.add(egui::DragValue::new(&mut s.download_cap_gb).range(1..=500).suffix(" GB"));
                            });
                            ui.horizontal(|ui| {
                                ui.label(
                                    RichText::new(format!(
                                        "{} · {}",
                                        tf("{} used", &[&crate::downloads::fmt_bytes(self.downloads.total_bytes())]),
                                        tn("{} video", "{} videos", self.downloads.entries.len() as u64)
                                    ))
                                    .color(text_dim()),
                                );
                                if ui.button(if cfg!(windows) { t("Show in Explorer") } else { t("Show in Finder") }).clicked() {
                                    if let Ok(d) = crate::downloads::dir() {
                                        let _ = open_url(&d.display().to_string());
                                    }
                                }
                                if !self.downloads.entries.is_empty() && ui.button(t("Delete all")).clicked() {
                                    self.downloads.clear();
                                    clear_downloads = true;
                                }
                            });
                            ui.label(
                                RichText::new(t("Saved to Movies/ytmini. When full, the oldest videos you've already watched are deleted first. Hardware-friendly codecs only, so offline playback is easy on the battery."))
                                    .small()
                                    .color(text_dim()),
                            );

                            ui.add_space(10.0);
                            section(ui, t("Disk cache"));
                            egui::Grid::new("cache").num_columns(2).spacing([16.0, 10.0]).show(ui, |ui| {
                                ui.label(t("Size limit"));
                                ui.add(
                                    egui::Slider::new(&mut s.cache_mb, 32..=4096)
                                        .logarithmic(true)
                                        .suffix(" MB")
                                        .text(""),
                                );
                                ui.end_row();
                                ui.label(t("Keep for"));
                                ui.add(egui::Slider::new(&mut s.cache_days, 1..=365).logarithmic(true).suffix(format!(" {}", t("days"))));
                                ui.end_row();
                                ui.label(t("In use"));
                                ui.horizontal(|ui| {
                                    let used = self.cache_usage.map_or("…".into(), |b| format!("{:.1} MB", b as f64 / 1_048_576.0));
                                    ui.label(used);
                                    if ui.small_button(t("Clear")).clicked() {
                                        clear = true;
                                    }
                                });
                                ui.end_row();
                            });
                            ui.label(
                                RichText::new(tf(
                                    "Recommended: {} MB, {} days — about 4,000 high-quality or 18,000 standard thumbnails, plus your last feed/subscriptions for instant startup.",
                                    &[&crate::settings::DEFAULT_CACHE_MB, &crate::settings::DEFAULT_CACHE_DAYS],
                                ))
                                .small()
                                .color(text_dim()),
                            );

                            ui.add_space(10.0);
                            section(ui, t("Crash logs"));
                            ui.horizontal(|ui| {
                                let n = crate::crashlog::count();
                                ui.label(
                                    RichText::new(if n == 0 { t("No crashes so far").to_string() } else { tn("{} crash log", "{} crash logs", n as u64) })
                                        .color(text_dim()),
                                );
                                if ui.button(t("Open logs folder")).clicked() {
                                    if let Some(d) = crate::crashlog::dir() {
                                        let _ = open_url(&d.display().to_string());
                                    }
                                }
                            });
                            ui.label(
                                RichText::new(t("If ytmini crashes, the date, time and technical details are saved here (no cookies or links). Attach the file when you report a problem."))
                                    .small()
                                    .color(text_dim()),
                            );

                            ui.add_space(10.0);
                            section(ui, t("Fast video lookup"));
                            let (n, last) = crate::native::logged_failures();
                            ui.label(
                                RichText::new(if n == 0 {
                                    t("No failures so far").to_string()
                                } else {
                                    tn("{} failure logged", "{} failures logged", n as u64)
                                })
                                .color(text_dim()),
                            );
                            if let Some(last) = last {
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(tf("Latest: {}", &[&last])).small().color(text_dim()),
                                    )
                                    .wrap(),
                                );
                            }
                            ui.label(
                                RichText::new(t("ytmini looks up videos itself, which is much faster than yt-dlp. When that fails and yt-dlp has to step in, the time and reason are saved to native-lookup.log in the logs folder above (no cookies or links)."))
                                    .small()
                                    .color(text_dim()),
                            );
                        }
                        SettingsTab::Account => {
                            section(ui, t("YouTube account"));
                            ui.horizontal(|ui| {
                                if let Some(src) = &s.signin_source {
                                    let label = signin::find(&src.id, &src.profile).map(|s| s.label());
                                    if let Some(label) = label {
                                        if ui.button(t("Refresh session")).on_hover_text(format!("Re-import from {label}")).clicked() {
                                            refresh_session = true;
                                        }
                                    }
                                }
                                if ui.button(t("Sign in again…")).clicked() {
                                    reimport = true;
                                }
                            });
                            ui.add_space(10.0);
                            section(ui, t("Components"));
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
                                    ui.label(RichText::new(self.deps.activity.clone().unwrap_or_else(|| t("Checking…").into())).small());
                                } else if managed && ui.button(t("Check for yt-dlp update")).clicked() {
                                    check_updates = true;
                                }
                            });
                        }
                    }
                    });
                    });
                });
            });
        if !open || reimport || refresh_session {
            self.show_settings = false;
        }
        if refresh_session {
            let src = self.settings.signin_source.clone();
            if let Some(s) = src.and_then(|src| signin::find(&src.id, &src.profile)) {
                self.import_error = None;
                self.safari_blocked = signin::safari_needs_access();
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
        if clear_downloads {
            self.load(ctx, Source::Downloads, true);
        }
        if clear {
            cache::clear();
            self.thumbs.clear();
            self.cache_usage = Some(0);
        }
        if reimport {
            self.import_error = None;
            self.safari_blocked = signin::safari_needs_access();
            self.sessions = signin::sessions();
            self.session_pick = 0;
            self.screen = Screen::Onboarding;
        }
    }
}

impl App {
    /// Links from the Services menu, and a fresh look at the clipboard
    /// whenever the window comes to the front.
    fn poll_links(&mut self, ctx: &egui::Context) {
        for text in crate::links::take() {
            match crate::links::find(&text) {
                Some(link) => {
                    self.pending_link = Some(link);
                    if self.hidden {
                        self.show_from_tray(ctx);
                    } else {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                    }
                }
                None => {
                    self.status = Some((
                        t("That isn't a YouTube video, playlist or channel link").into(),
                        true,
                    ))
                }
            }
        }
        let focused = ctx.input(|i| i.focused);
        if focused && !self.was_focused {
            if let Some(text) = crate::links::clipboard_text().filter(|t| t.len() < 4096) {
                if text != self.clip_seen {
                    self.clip_link = crate::links::find(&text);
                    self.clip_seen = text;
                }
            }
        }
        self.was_focused = focused;
    }

    /// Queue a DeArrow lookup (one worker, in order of appearance on screen).
    fn request_branding(&mut self, ctx: &egui::Context, id: String) {
        if self.dearrow.contains_key(&id) {
            return;
        }
        self.dearrow.insert(id.clone(), None);
        let tx = self.dearrow_tx.get_or_insert_with(|| {
            let (req_tx, req_rx) = std::sync::mpsc::channel::<String>();
            let (tx, ctx) = (self.tx.clone(), ctx.clone());
            std::thread::spawn(move || {
                for id in req_rx {
                    let b = crate::dearrow::fetch(&id).unwrap_or_default();
                    let _ = tx.send(Msg::DeArrow(id, b));
                    ctx.request_repaint();
                }
            });
            req_tx
        });
        let _ = tx.send(id);
    }

    /// Fresh pre-resolved URLs for `id` in `format`, if we have them.
    fn prepared_for(&self, id: &str, format: &str) -> Option<yt::Direct> {
        self.prepared
            .get(id)
            .filter(|(f, d, t)| f == format && t.elapsed() < PREPARED_TTL && d.fresh())
            .map(|(_, d, _)| d.clone())
    }

    /// Hovered a card for a moment (or a link is on the clipboard): resolve
    /// its stream in the background, one at a time. In Low Power Mode it
    /// waits for a longer hover, so passing over cards costs nothing.
    fn maybe_prepare(&mut self, ctx: &egui::Context) {
        // two at a time: moving from one card to the next doesn't queue behind it
        if !self.settings.prepare_on_hover || self.preparing.len() >= 2 {
            return;
        }
        let dwell = if self.power.saver {
            HOVER_PREPARE * 2 + HOVER_PREPARE / 3
        } else {
            HOVER_PREPARE
        };
        let Some(tools) = self.tools.clone() else {
            return;
        };
        let candidate = match &self.hover {
            Some((v, since)) if since.elapsed() >= dwell => Some(v.clone()),
            Some((_, since)) => {
                ctx.request_repaint_after(dwell.saturating_sub(since.elapsed()));
                None
            }
            None => match &self.clip_link {
                Some(crate::links::Link::Video { id, .. }) => Some(Video {
                    id: id.clone(),
                    ..Default::default()
                }),
                _ => None,
            },
        };
        let Some(video) = candidate else { return };
        self.start_prepare(ctx, tools, video);
    }

    /// Resolve `video`'s stream in the background (unless it's already
    /// resolved, running, or downloaded).
    fn start_prepare(&mut self, ctx: &egui::Context, tools: Tools, video: Video) {
        let profile = yt::profile(self.settings.quality, self.power, self.screen_cap);
        if profile.audio_only
            || self.preparing.contains(&video.id)
            || self.downloads.get(&video.id).is_some()
            || self.prepared_for(&video.id, &profile.ytdl_format).is_some()
        {
            return;
        }
        if std::env::var_os("YTMINI_DEBUG").is_some() {
            eprintln!("resolving {} ahead", video.id);
        }
        self.preparing.insert(video.id.clone());
        let (tx, ctx, cookies, format) = (
            self.tx.clone(),
            ctx.clone(),
            self.cookies.clone(),
            profile.ytdl_format,
        );
        std::thread::spawn(move || {
            let r = crate::resolver::resolve(&tools, &cookies, &video, &format)
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(Msg::Prepared(video.id, format, r));
            ctx.request_repaint();
        });
    }

    /// Stored stream URLs only work from the IP they were resolved on: drop
    /// them when the route changes (Wi-Fi switch, VPN, new network).
    fn watch_network(&mut self) {
        if self.net_checked.elapsed() < NET_CHECK {
            return;
        }
        self.net_checked = std::time::Instant::now();
        crate::resolver::reap_idle();
        let now = yt::route_fingerprint();
        if now != self.net_route {
            if self.net_route.is_some() && !self.prepared.is_empty() {
                if std::env::var_os("YTMINI_DEBUG").is_some() {
                    eprintln!(
                        "network changed: dropping {} stored resolves",
                        self.prepared.len()
                    );
                }
                self.prepared.clear();
            }
            if self.net_route.is_some() {
                crate::resolver::reset();
            }
            self.net_route = now;
        }
    }

    /// While a video plays, resolve the one Next / autoplay would start, so
    /// it opens at once. Waits a little into playback (longer in Low Power
    /// Mode) so it doesn't compete with the start of this one.
    fn prepare_next(&mut self, ctx: &egui::Context) {
        if !self.settings.prepare_on_hover || self.preparing.len() >= 3 {
            return;
        }
        let after = if self.power.saver { 20.0 } else { 5.0 };
        // ...but never later than the last 30 s (short videos)
        let due = |p: &Embedded| p.pos >= after || (p.duration > 0.0 && p.duration - p.pos < 30.0);
        if !self
            .embedded
            .as_ref()
            .is_some_and(|p| p.started && !p.local && due(p))
        {
            return;
        }
        let (Some(next), Some(tools)) = (self.peek_next().cloned(), self.tools.clone()) else {
            return;
        };
        self.start_prepare(ctx, tools, next);
    }

    fn start_download(&mut self, ctx: &egui::Context, v: &Video) {
        let Some(tools) = self.tools.clone() else {
            return;
        };
        if self.dl_active.contains_key(&v.id) || self.downloads.get(&v.id).is_some() {
            return;
        }
        let cap = self.settings.download_cap_gb * 1_000_000_000;
        let positions = &self.positions;
        let evicted =
            self.downloads
                .evict(cap, |id| positions.get(id).is_some_and(|e| e.watched()), "");
        if self.downloads.total_bytes() >= cap {
            self.status = Some((
                tf(
                    "Downloads are full ({} of {}). Delete some, or raise the limit in Settings.",
                    &[
                        &crate::downloads::fmt_bytes(self.downloads.total_bytes()),
                        &crate::downloads::fmt_bytes(cap),
                    ],
                ),
                true,
            ));
            return;
        }
        if evicted > 0 {
            self.status = Some((
                tn(
                    "Made room: deleted {} watched download.",
                    "Made room: deleted {} watched downloads.",
                    evicted as u64,
                ),
                false,
            ));
        }
        self.dl_active.insert(v.id.clone(), 0.0);
        let (tx, ctx, cookies, video, height) = (
            self.tx.clone(),
            ctx.clone(),
            self.cookies.clone(),
            v.clone(),
            self.settings.download_height,
        );
        std::thread::spawn(move || {
            use crate::downloads::{download, Progress};
            let mut last = 0.0f32;
            let r = download(&tools, &cookies, &video, height, |p| {
                if p - last >= 0.01 {
                    last = p;
                    let _ = tx.send(Msg::Download(Progress::Running(video.id.clone(), p)));
                    ctx.request_repaint();
                }
            });
            let _ = tx.send(Msg::Download(Progress::Done(
                video.id.clone(),
                r.map_err(|e| format!("{e:#}")),
            )));
            ctx.request_repaint();
        });
    }

    fn on_download(&mut self, ctx: &egui::Context, p: crate::downloads::Progress) {
        use crate::downloads::Progress;
        match p {
            Progress::Running(id, f) => {
                if let Some(x) = self.dl_active.get_mut(&id) {
                    *x = f.min(0.99);
                }
            }
            Progress::Done(id, r) => {
                self.dl_active.remove(&id);
                match r {
                    Ok(e) => {
                        self.status = Some((
                            tf(
                                "Downloaded “{}” ({}).",
                                &[&e.title, &crate::downloads::fmt_bytes(e.bytes)],
                            ),
                            false,
                        ));
                        self.downloads.add(e);
                        let cap = self.settings.download_cap_gb * 1_000_000_000;
                        let positions = &self.positions;
                        self.downloads.evict(
                            cap,
                            |id| positions.get(id).is_some_and(|e| e.watched()),
                            &id,
                        );
                        self.load(ctx, Source::Downloads, true);
                    }
                    Err(e) => self.status = Some((tf("Download failed: {}", &[&e]), true)),
                }
            }
        }
    }

    /// Media keys / Control Center → player, and player state → Now Playing.
    fn sync_now_playing(&mut self) {
        use crate::nowplaying::{self as np, Cmd};
        for c in np::take() {
            match c {
                Cmd::Next => {
                    self.go_next();
                    continue;
                }
                Cmd::Prev => {
                    self.go_prev();
                    continue;
                }
                _ => {}
            }
            let Some(p) = &mut self.embedded else {
                continue;
            };
            match c {
                Cmd::Toggle => p.toggle_pause(),
                Cmd::Play if p.paused || p.eof => p.toggle_pause(),
                Cmd::Pause if !p.paused => p.toggle_pause(),
                Cmd::Seek(t) => p.seek_absolute(t, true),
                Cmd::Skip(d) => p.seek_relative(d),
                _ => {}
            }
        }
        let Some(p) = &self.embedded else {
            if self.np_last.take().is_some() {
                np::publish(None, None);
            }
            return;
        };
        if !p.started {
            return;
        }
        let info = np::Info {
            id: p.id.clone(),
            title: p.title.clone(),
            artist: self.np_artist.clone(),
            duration: p.duration,
            pos: p.pos,
            playing: !p.paused && !p.eof,
            rate: p.speed,
        };
        let due = match &self.np_last {
            None => true,
            Some((old, at)) => info.differs(old, at.elapsed().as_secs_f64()),
        };
        if due {
            np::publish(Some(&info), self.np_art.as_deref());
            self.np_last = Some((info, std::time::Instant::now()));
        }
    }

    fn open_link(&mut self, ctx: &egui::Context, frame: &eframe::Frame, link: crate::links::Link) {
        use crate::links::Link;
        self.clip_link = None;
        match link {
            Link::Video { id, start } => {
                if let Some(t) = start {
                    self.link_start = Some((id.clone(), t));
                }
                self.play_from(
                    Vec::new(),
                    &Video {
                        id,
                        ..Default::default()
                    },
                );
            }
            Link::Playlist(id) => {
                self.close_embedded(ctx, frame);
                self.open_page(
                    ctx,
                    Source::Playlist {
                        id,
                        title: t("Playlist").into(),
                    },
                );
            }
            Link::Channel { url, name } => {
                self.close_embedded(ctx, frame);
                self.open_page(ctx, Source::Channel { url, name });
            }
        }
    }

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
            p.set_background(self.mini);
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
        let Some(mut sm) = self.smoke.take() else {
            return;
        };
        ctx.request_repaint_after(Duration::from_millis(100));
        let t = sm.since.elapsed().as_secs_f64();
        let started = self
            .embedded
            .as_ref()
            .is_some_and(|p| p.started && !p.buffering);
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
                sm.log(
                    pos > 50.0,
                    &format!("second playback resumed where we left off ({pos:.1}s)"),
                );
                self.settings.quality = yt::Quality::Max(480);
                self.apply_profile_live(ctx);
                next(&mut sm);
            }
            4 if started && t > 4.0 => {
                let h = self
                    .embedded
                    .as_ref()
                    .and_then(Embedded::height)
                    .unwrap_or(0.0);
                sm.log(
                    h > 0.0 && h <= 480.0,
                    &format!("live switch to 480p (height {h})"),
                );
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
                sm.log(
                    pos > sm.mark + 3.0,
                    &format!("keeps playing while hidden ({:.1}s → {pos:.1}s)", sm.mark),
                );
                sm.log(
                    h.is_none(),
                    &format!("audio only while hidden (video height {h:?})"),
                );
                self.show_from_tray(ctx);
                next(&mut sm);
            }
            6 if t > 3.0 => {
                let h = self
                    .embedded
                    .as_ref()
                    .and_then(Embedded::height)
                    .unwrap_or(0.0);
                sm.log(
                    h > 0.0,
                    &format!("video back after showing window (height {h})"),
                );
                eprintln!(
                    "[smoke] quitting during playback; {}",
                    if sm.failed {
                        "SOME FAILED"
                    } else {
                        "ALL PASSED"
                    }
                );
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
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let d = now.saturating_sub(unix);
    match d {
        0..=89 => t("just now").into(),
        90..=5399 => tn("{} minute ago", "{} minutes ago", d / 60),
        5400..=129_599 => tn("{} hour ago", "{} hours ago", d / 3600),
        _ => tn("{} day ago", "{} days ago", d / 86_400),
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
        self.poll_links(ctx);
        self.watch_session(ctx);
        self.sync_now_playing();
        // Minimized or in the tray, eframe runs no ui pass, but autoplay and
        // starting the next video happen there (the player needs GL): ask
        // for passes while something is due.
        let due = self.pending_play.is_some()
            || self.resolve_wait.is_some()
            || !self.play_seq.is_empty()
            || self.embedded.as_ref().is_some_and(|p| {
                p.eof && p.started && self.settings.autoplay && !self.autoplay_cancelled
            });
        if due
            && ctx.input(|i| {
                !i.viewport().visible().unwrap_or(true) || i.viewport().minimized == Some(true)
            })
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::RequestPaintWhileHidden);
            ctx.request_repaint_after(Duration::from_millis(200));
        }
        self.sync_eq(ctx);
        crate::awake::set(match &self.embedded {
            Some(p) if p.started && !p.paused && !p.eof => {
                if !self.hidden && !self.mini && p.has_video() {
                    crate::awake::Need::Display
                } else {
                    crate::awake::Need::System
                }
            }
            _ => crate::awake::Need::Nothing,
        });
        self.watch_network();
        // our own stream lookup failed and yt-dlp stepped in: say so once a session
        let (fails, last) = crate::native::failures();
        if fails > self.native_seen {
            if self.native_seen == 0 {
                let why = last.unwrap_or_default();
                let why: String = why.chars().take(140).collect();
                self.status = Some((
                    tf(
                        "Fast video lookup failed ({}). yt-dlp took over; details are in Settings → Storage.",
                        &[&why],
                    ),
                    true,
                ));
            }
            self.native_seen = fails;
        }
        if let Some(e) = &mut self.embedded {
            e.poll_events();
            if std::mem::take(&mut e.resolved_failed) {
                crate::native::report_failure(&e.id);
                // probably a new network: the other stored URLs won't work either
                self.prepared.clear();
            }
            if std::mem::take(&mut e.title_changed) {
                self.np_last = None; // republish Now Playing with the real title
            }
        }
        // (the smoke test changes settings; never persist those)
        if self.smoke.is_none() && self.settings != self.saved_settings {
            if (self.settings.language, &self.settings.region)
                != (self.saved_settings.language, &self.saved_settings.region)
            {
                // YouTube answers in the new language/region: refetch everything
                crate::i18n::set(self.settings.language);
                crate::i18n::set_region(self.settings.region.clone());
                crate::innertube::HOME_CHIPS
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clear();
                self.topic_shelves.clear();
                self.dearrow.clear();
                for tab in self.tabs.values_mut() {
                    tab.loaded_once = false;
                }
                self.load(ctx, self.active.clone(), true);
            }
            if self.settings.volume_leveling != self.saved_settings.volume_leveling {
                if let Some(p) = &mut self.embedded {
                    p.set_leveling(self.settings.volume_leveling);
                }
            }
            if (self.settings.cache_mb, self.settings.cache_days)
                != (self.saved_settings.cache_mb, self.saved_settings.cache_days)
            {
                self.limits
                    .set(self.settings.cache_mb, self.settings.cache_days);
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
        if self.pending_link.is_some()
            && self.tools.is_some()
            && matches!(self.screen, Screen::Main)
        {
            if let Some(link) = self.pending_link.take() {
                self.open_link(&ctx, frame, link);
            }
        }
        if self.pending_play.is_some() {
            self.start_embedded(&ctx, frame);
        }
        // a waited-for resolve is taking too long: let mpv's yt-dlp go ahead
        if let Some(t) = self.resolve_wait {
            match self.embedded.as_mut().filter(|p| p.is_waiting()) {
                Some(p) if t.elapsed() >= RESOLVE_WAIT_MAX => {
                    p.load_resolved(None);
                    self.resolve_wait = None;
                }
                Some(_) => ctx.request_repaint_after(RESOLVE_WAIT_MAX.saturating_sub(t.elapsed())),
                None => self.resolve_wait = None,
            }
        }
        self.prepare_next(&ctx);
        // (after 6 s, like a person picking a video: start-up work is done)
        if !self.play_seq.is_empty()
            && self.tools.is_some()
            && ctx.input(|i| i.time) > 6.0
            && self.pending_play.is_none()
            && self
                .play_seq_wait
                .is_none_or(|t| std::time::Instant::now() >= t)
            && self
                .embedded
                .as_ref()
                .is_none_or(|p| p.started && p.pos > 4.0)
        {
            let step = self.play_seq.pop().unwrap_or_default();
            if step == "back" {
                // as if Back were clicked
                if self.playing_song() {
                    self.collapse_player(&ctx);
                } else {
                    self.close_embedded(&ctx, frame);
                }
            } else if step == "tray" {
                self.hide_to_tray(&ctx);
            } else if let Some(secs) = step.strip_prefix("wait:").and_then(|t| t.parse().ok()) {
                self.play_seq_wait =
                    Some(std::time::Instant::now() + Duration::from_secs_f64(secs));
            } else if step == "expand" {
                self.mini = false;
                if let Some(p) = &mut self.embedded {
                    p.set_background(false);
                }
            } else if step == "pause" {
                if let Some(p) = &mut self.embedded {
                    p.toggle_pause();
                }
            } else if let Some(t) = step.strip_prefix("seek:").and_then(|t| t.parse().ok()) {
                if let Some(p) = &mut self.embedded {
                    p.seek_absolute(t, true);
                }
            } else if step == "minimize" {
                ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
            } else if step == "end" {
                if let Some(p) = self.embedded.as_mut().filter(|p| p.duration > 5.0) {
                    p.seek_absolute(p.duration - 2.0, true);
                }
            } else if let Some(path) = step.strip_prefix("shot:") {
                let path = std::path::PathBuf::from(path);
                ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(
                    egui::ScreenshotCallback::new(move |img| {
                        let [w, h] = img.size;
                        let rgba: Vec<u8> = img.pixels.iter().flat_map(|c| c.to_array()).collect();
                        if let Some(buf) = image::RgbaImage::from_raw(w as u32, h as u32, rgba) {
                            let _ = buf.save(&path);
                        }
                    }),
                ));
            } else if let Some(v) = Video::from_url_or_id(&step) {
                if std::env::var_os("YTMINI_DEBUG").is_some() {
                    eprintln!("play_seq: {}", v.id);
                }
                self.play(&v);
                ctx.request_repaint();
            }
        }
        if !self.play_seq.is_empty() {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
        let show_player = self.embedded.is_some() && !self.mini;
        let grid_in = ctx.animate_bool_with_time(egui::Id::unique("grid_in"), !show_player, 0.25);
        egui::Frame::new().fill(bg()).show(ui, |ui| {
            ui.set_min_size(ui.available_size());
            match &self.screen {
                Screen::MissingTools(m) => {
                    let m = m.clone();
                    self.ui_missing(ui, &m);
                }
                Screen::Setup => self.ui_setup(ui),
                Screen::Onboarding => self.ui_onboarding(ui),
                Screen::Main if show_player => self.ui_player(ui, frame),
                Screen::Main => {
                    if self.mini && self.embedded.is_some() {
                        self.ui_mini_bar(ui, frame);
                    }
                    // back from the player: the grid fades in
                    ui.multiply_opacity(player::ease(grid_in));
                    self.ui_main(ui);
                }
            }
        });
        if self.show_settings {
            self.ui_settings(&ctx);
        }
        self.ui_save_dialog(&ctx);
    }

    fn on_exit(&mut self, gl: Option<&eframe::glow::Context>) {
        self.remember_stopped();
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

// ---------- list sources ----------

/// Where a tab's videos come from: YouTube's API, or yt-dlp as the fallback.
enum Feed {
    Native {
        pager: Box<crate::innertube::Pager>,
        buf: std::collections::VecDeque<Video>,
        ended: bool,
    },
    /// Opened lazily (and reopened after idling) at the current position.
    Ytdlp(Option<yt::Stream>),
}

impl Feed {
    /// Fetch the first page natively; errors mean "use yt-dlp".
    fn native(cookies: &std::path::Path, source: &Source) -> anyhow::Result<Feed> {
        if std::env::var_os("YTMINI_NO_NATIVE").is_some() {
            anyhow::bail!("disabled by YTMINI_NO_NATIVE");
        }
        let mut pager = crate::innertube::Pager::new(cookies, source)?;
        let page = pager.next_page()?.unwrap_or_default();
        Ok(Feed::Native {
            pager: Box::new(pager),
            buf: page.into(),
            ended: false,
        })
    }

    /// Videos already in hand (no waiting for the next one).
    fn buffered(&self) -> bool {
        matches!(self, Feed::Native { buf, .. } if !buf.is_empty())
    }

    fn next_video(
        &mut self,
        tools: &Tools,
        cookies: &std::path::Path,
        source: &Source,
        delivered: usize,
    ) -> anyhow::Result<Option<Video>> {
        loop {
            match self {
                Feed::Native { pager, buf, ended } => {
                    if let Some(v) = buf.pop_front() {
                        return Ok(Some(v));
                    }
                    if *ended {
                        return Ok(None);
                    }
                    match pager.next_page() {
                        Ok(Some(page)) if !page.is_empty() => buf.extend(page),
                        Ok(_) => *ended = true,
                        // a later page failed: carry on with yt-dlp from here
                        Err(_) => *self = Feed::Ytdlp(None),
                    }
                }
                Feed::Ytdlp(stream) => {
                    if stream.is_none() {
                        *stream = Some(yt::Stream::open(tools, cookies, source, delivered)?);
                    }
                    return stream.as_mut().map_or(Ok(None), |s| s.next_video());
                }
            }
        }
    }

    /// Native: fetch the next page while the user is still looking at this one.
    fn prefetch(&mut self) {
        if let Feed::Native { pager, buf, ended } = self {
            if buf.len() < yt::PAGE && !*ended {
                match pager.next_page() {
                    Ok(Some(page)) if !page.is_empty() => buf.extend(page),
                    Ok(_) => *ended = true,
                    Err(_) => {} // retried on the next request
                }
            }
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
    in_history: bool,
    /// None: not downloaded; Some(1.0): downloaded; less: in progress.
    download: Option<f32>,
    /// A Short: 9:16 cover, no duration badge.
    portrait: bool,
}

#[derive(Clone, Copy)]
enum Glyph {
    ThumbUp,
    ThumbDown,
    Bubble,
}

/// Small vector icons (the UI font has no emoji).
fn paint_glyph(p: &egui::Painter, r: egui::Rect, icon: Glyph, color: Color32) {
    let st = Stroke::new(1.5, color);
    match icon {
        Glyph::ThumbUp | Glyph::ThumbDown => {
            // drawn as "up", mirrored vertically for "down"
            let down = matches!(icon, Glyph::ThumbDown);
            let f = |x: f32, y: f32| {
                let y = if down { 1.0 - y } else { y };
                egui::pos2(r.left() + x * r.width(), r.top() + y * r.height())
            };
            p.line_segment([f(0.08, 0.45), f(0.08, 0.95)], Stroke::new(2.5, color));
            let pts = vec![
                f(0.25, 0.45),
                f(0.45, 0.08),
                f(0.55, 0.05),
                f(0.6, 0.15),
                f(0.55, 0.38),
                f(0.88, 0.38),
                f(0.95, 0.48),
                f(0.85, 0.92),
                f(0.25, 0.92),
            ];
            p.add(egui::Shape::closed_line(pts, st));
        }
        Glyph::Bubble => {
            let b = egui::Rect::from_min_max(
                r.min + Vec2::new(r.width() * 0.05, r.height() * 0.1),
                r.max - Vec2::new(r.width() * 0.05, r.height() * 0.3),
            );
            p.rect_stroke(b, 3, st, egui::StrokeKind::Middle);
            p.line_segment(
                [
                    egui::pos2(b.left() + b.width() * 0.3, b.bottom()),
                    egui::pos2(b.left() + b.width() * 0.2, r.bottom()),
                ],
                st,
            );
        }
    }
}

/// A pill with an icon and optional text.
fn icon_pill(ui: &mut egui::Ui, icon: Glyph, label: Option<&str>, strong: bool) -> egui::Response {
    let (fill, color) = if strong {
        (accent(), on_accent())
    } else {
        (card_hover(), text())
    };
    let galley = label.map(|l| {
        ui.painter()
            .layout_no_wrap(l.to_string(), FontId::proportional(13.0), color)
    });
    let w = 12.0 + 15.0 + galley.as_ref().map_or(0.0, |g| 6.0 + g.size().x) + 12.0;
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, 28.0), Sense::click());
    let fill = if resp.hovered() {
        fill.gamma_multiply(1.15)
    } else {
        fill
    };
    ui.painter().rect_filled(rect, 14, fill);
    let ir = egui::Rect::from_center_size(
        egui::pos2(rect.left() + 12.0 + 7.5, rect.center().y),
        Vec2::splat(14.0),
    );
    paint_glyph(ui.painter(), ir, icon, color);
    if let Some(g) = galley {
        let pos = egui::pos2(ir.right() + 6.0, rect.center().y - g.size().y / 2.0);
        ui.painter().galley(pos, g, color);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A rounded, borderless button; `strong`: filled with the accent colour.
/// Drawn by hand so it never changes size on hover (egui's buttons grow).
fn pill(ui: &mut egui::Ui, label: &str, strong: bool) -> egui::Response {
    let (fill, color) = if strong {
        (accent(), on_accent())
    } else {
        (card_hover(), text())
    };
    let g = ui
        .painter()
        .layout_no_wrap(label.to_string(), FontId::proportional(13.0), color);
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(g.size().x + 24.0, 28.0), Sense::click());
    let hover = ui
        .ctx()
        .animate_bool_with_time(resp.id, resp.hovered(), 0.12);
    let fill = fill.lerp_to_gamma(Color32::WHITE, 0.08 * hover);
    ui.painter().rect_filled(rect, 14, fill);
    ui.painter()
        .galley(rect.center() - g.size() / 2.0, g, color);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// One comment: author, when, text, likes and replies. Returns a clicked
/// timestamp.
fn comment_row(ui: &mut egui::Ui, c: &crate::innertube::Comment) -> Option<f64> {
    ui.add_space(6.0);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        let name = RichText::new(&c.author).size(12.5).strong();
        ui.label(if c.creator {
            name.color(accent())
        } else {
            name.color(text())
        });
        if c.verified {
            ui.label(RichText::new("✔").size(11.0).color(text_dim()));
        }
        ui.label(RichText::new(&c.published).size(11.5).color(text_dim()));
        if c.pinned {
            ui.label(RichText::new(t("Pinned")).size(11.5).color(text_dim()));
        }
    });
    let seek = rich_text(ui, &c.text, 13.0, text());
    if !c.likes.is_empty() || !c.replies.is_empty() {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            for (icon, n) in [(Glyph::ThumbUp, &c.likes), (Glyph::Bubble, &c.replies)] {
                if n.is_empty() {
                    continue;
                }
                let (r, _) = ui.allocate_exact_size(Vec2::splat(12.0), Sense::hover());
                paint_glyph(ui.painter(), r, icon, text_dim());
                ui.label(RichText::new(n.as_str()).size(11.5).color(text_dim()));
                ui.add_space(8.0);
            }
        });
    }
    ui.add_space(4.0);
    seek
}

/// Text with clickable timestamps (returned when clicked) and links (opened
/// in the browser).
fn rich_text(ui: &mut egui::Ui, s: &str, size: f32, color: Color32) -> Option<f64> {
    use crate::textlinks::{segments, Seg};
    let mut seek = None;
    for line in s.lines() {
        if line.trim().is_empty() {
            ui.add_space(size * 0.6);
            continue;
        }
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            for seg in segments(line) {
                match seg {
                    Seg::Text(t) => {
                        ui.label(RichText::new(t).size(size).color(color));
                    }
                    Seg::Time(t, secs) => {
                        if ui
                            .link(RichText::new(t).size(size).color(accent()))
                            .clicked()
                        {
                            seek = Some(secs);
                        }
                    }
                    Seg::Url(u) => {
                        let shown = u
                            .trim_start_matches("https://")
                            .trim_start_matches("http://");
                        let r = ui
                            .link(RichText::new(shown).size(size).color(accent()))
                            .on_hover_text(u);
                        if r.clicked() {
                            let _ = open_url(u);
                        }
                    }
                }
            }
        });
    }
    seek
}

/// A small round icon button floating over a row (queue / suggestions).
fn round_btn(ui: &mut egui::Ui, rect: egui::Rect, sym: &str) -> egui::Response {
    let resp = ui.interact(
        rect,
        ui.scope_id()
            .with(("round_btn", rect.min.x as i32, rect.min.y as i32)),
        Sense::click(),
    );
    let alpha = if resp.hovered() { 235 } else { 175 };
    let p = ui.painter();
    p.circle_filled(
        rect.center(),
        rect.width() / 2.0,
        Color32::from_black_alpha(alpha),
    );
    p.text(
        rect.center(),
        Align2::CENTER_CENTER,
        sym,
        FontId::proportional(12.0),
        if resp.hovered() {
            Color32::WHITE
        } else {
            Color32::from_gray(220)
        },
    );
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Load suggestions for `id`, retrying a few times (network blips, a session
/// refresh in progress) before reporting the error.
fn fetch_related(tx: &Sender<Msg>, cookies: &std::path::Path, id: &str) {
    let mut last = String::new();
    for wait in [0, 2, 5, 10] {
        std::thread::sleep(Duration::from_secs(wait));
        match crate::innertube::watch_next(cookies, id) {
            Ok((r, info)) if !r.is_empty() => {
                let _ = tx.send(Msg::Watch(id.to_string(), info));
                let _ = tx.send(Msg::Related(id.to_string(), r));
                return;
            }
            Ok(_) => last = t("YouTube returned no suggestions").to_string(),
            Err(e) => last = format!("{e:#}"),
        }
    }
    let _ = tx.send(Msg::RelatedFailed(id.to_string(), last));
}

struct RowResp {
    clicked: bool,
    hovered: bool,
    rect: egui::Rect,
    drag_started: bool,
}

/// A compact video row for the Up next panel: thumbnail, title, channel.
fn up_next_row(ui: &mut egui::Ui, thumbs: &mut Thumbs, v: &Video, current: bool) -> RowResp {
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, 64.0), Sense::click_and_drag());
    let hovered = ui.rect_contains_pointer(rect);
    let drag_started = resp.drag_started();
    if !ui.is_rect_visible(rect) {
        return RowResp {
            clicked: false,
            hovered: false,
            rect,
            drag_started,
        };
    }
    let painter = ui.painter_at(rect);
    if current {
        painter.rect_filled(rect, 8, accent().gamma_multiply(0.22));
    } else if hovered {
        painter.rect_filled(rect, 8, card_hover());
    }
    let thumb = egui::Rect::from_min_size(rect.min + Vec2::new(2.0, 2.0), Vec2::new(107.0, 60.0));
    painter.rect_filled(thumb, 6, thumb_bg());
    if let Some(tex) = thumbs.get(&v.id, Variant::Standard) {
        egui::Image::from_texture(egui::load::SizedTexture::from_handle(tex))
            .corner_radius(6)
            .paint_at(ui, thumb);
    }
    let dur = v.duration_label();
    if !dur.is_empty() && dur != "LIVE" {
        let g = painter.layout_no_wrap(dur, FontId::proportional(10.5), Color32::WHITE);
        let note = if v.is_music { 11.0 } else { 0.0 };
        let b = egui::Rect::from_min_size(
            thumb.right_bottom() - g.size() - Vec2::new(7.0 + note, 5.0),
            g.size() + Vec2::new(4.0 + note, 2.0),
        );
        painter.rect_filled(b, 3, Color32::from_black_alpha(190));
        if note > 0.0 {
            crate::theme::paint_note(
                &painter,
                egui::pos2(b.left() + 7.0, b.center().y + 0.5),
                0.45,
                Color32::WHITE,
            );
        }
        painter.galley(b.min + Vec2::new(2.0 + note, 1.0), g, Color32::WHITE);
    }
    let tx = thumb.right() + 8.0;
    let tw = rect.right() - tx - 4.0;
    let mut job = LayoutJob::single_section(
        v.title().to_string(),
        TextFormat {
            font_id: FontId::proportional(13.0),
            color: text(),
            ..Default::default()
        },
    );
    job.wrap.max_width = tw;
    job.wrap.max_rows = 2;
    job.wrap.overflow_character = Some('…');
    let g = painter.layout_job(job);
    painter.galley(egui::pos2(tx, rect.top() + 3.0), g, text());
    let mut cj = LayoutJob::single_section(
        v.channel().to_string(),
        TextFormat {
            font_id: FontId::proportional(11.5),
            color: text_dim(),
            ..Default::default()
        },
    );
    cj.wrap.max_width = tw;
    cj.wrap.max_rows = 1;
    cj.wrap.overflow_character = Some('…');
    let cg = painter.layout_job(cj);
    painter.galley(egui::pos2(tx, rect.bottom() - 18.0), cg, text_dim());
    RowResp {
        clicked: resp
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .clicked(),
        hovered,
        rect,
        drag_started,
    }
}

fn video_card(
    ui: &mut egui::Ui,
    thumbs: &mut Thumbs,
    variant: Variant,
    v: &Video,
    info: CardInfo,
) -> CardAction {
    let CardInfo {
        size,
        thumb_h,
        playing,
        progress,
        m,
        in_watch_later,
        in_history,
        download,
        portrait,
    } = info;
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    if !ui.is_rect_visible(rect) {
        return CardAction::None;
    }
    let ctx = ui.ctx().clone();
    let hover =
        ctx.animate_bool_with_time(resp.id, resp.hovered() || resp.context_menu_opened(), 0.12);
    let painter = ui.painter_at(rect.expand(2.0));
    let r = CornerRadius::same(RADIUS);
    painter.rect_filled(rect, r, lerp_color(card(), card_hover(), hover));

    // thumbnail (fades in when it arrives; already-cached ones show at once)
    let thumb = egui::Rect::from_min_size(rect.min, Vec2::new(rect.width(), thumb_h));
    let tr = CornerRadius {
        nw: RADIUS,
        ne: RADIUS,
        sw: 0,
        se: 0,
    };
    painter.rect_filled(thumb, tr, thumb_bg());
    let thumb_video = if v.is_playlist {
        v.thumb_id.as_deref()
    } else {
        Some(v.id.as_str())
    };
    let watched = progress.is_some_and(|p| p.watched());
    let tex = thumb_video.and_then(|id| thumbs.get(id, variant));
    let shown = ctx.animate_bool_with_time(resp.id.with("thumb"), tex.is_some(), 0.25);
    if let Some(tex) = tex {
        let base = if watched {
            Color32::from_gray(140)
        } else {
            Color32::WHITE
        };
        // cover: crop a landscape fallback to fill a portrait slot (and vice versa)
        let [tw, th] = tex.size().map(|x| x as f32);
        let (want, have) = (thumb.width() / thumb.height(), tw / th.max(1.0));
        let uv = if (want - have).abs() < 0.02 {
            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0))
        } else if have > want {
            let w = want / have;
            egui::Rect::from_min_max(
                egui::pos2(0.5 - w / 2.0, 0.0),
                egui::pos2(0.5 + w / 2.0, 1.0),
            )
        } else {
            let h = have / want;
            egui::Rect::from_min_max(
                egui::pos2(0.0, 0.5 - h / 2.0),
                egui::pos2(1.0, 0.5 + h / 2.0),
            )
        };
        egui::Image::from_texture(egui::load::SizedTexture::from_handle(tex))
            .uv(uv)
            .corner_radius(tr)
            .tint(base.gamma_multiply(shown))
            .paint_at(ui, thumb);
    }

    // watch progress: thin accent bar along the bottom of the thumbnail
    if let Some(f) = progress.and_then(|p| p.fraction()).filter(|f| *f > 0.01) {
        let track = egui::Rect::from_min_max(
            egui::pos2(thumb.left(), thumb.bottom() - 3.0),
            thumb.right_bottom(),
        );
        painter.rect_filled(track, 0, Color32::from_white_alpha(70));
        let done = egui::Rect::from_min_max(
            track.min,
            egui::pos2(track.left() + track.width() * f, track.bottom()),
        );
        painter.rect_filled(done, 0, accent());
    }

    // duration / playlist badge
    let dur = if v.is_playlist {
        t("PLAYLIST").to_string()
    } else if watched {
        format!("✓ {}", v.duration_label())
    } else {
        v.duration_label()
    };
    let live = dur == "LIVE";
    let no_badge = dur.trim_start_matches("✓ ").is_empty();
    let shown_dur = if live { t("LIVE").to_string() } else { dur };
    let g = painter.layout_no_wrap(shown_dur, FontId::proportional(11.5), Color32::WHITE);
    // songs: a ♫ before the duration, as on YouTube
    let note = if v.is_music && !live && !v.is_playlist {
        14.0
    } else {
        0.0
    };
    let badge = egui::Rect::from_min_size(
        thumb.right_bottom() - g.size() - Vec2::new(12.0 + note, 10.0),
        g.size() + Vec2::new(8.0 + note, 4.0),
    );
    if (!portrait || live) && !no_badge {
        painter.rect_filled(
            badge,
            4,
            if live {
                Color32::from_rgb(0xE5, 0x2D, 0x27)
            } else {
                Color32::from_black_alpha(190)
            },
        );
        if note > 0.0 {
            crate::theme::paint_note(
                &painter,
                egui::pos2(badge.left() + 10.0, badge.center().y + 0.5),
                0.55,
                Color32::WHITE,
            );
        }
        painter.galley(badge.min + Vec2::new(4.0 + note, 2.0), g, Color32::WHITE);
    }

    // title (max 2 lines)
    let text_w = rect.width() - 2.0 * m.pad;
    let mut job = LayoutJob::single_section(
        v.title().to_string(),
        TextFormat {
            font_id: FontId::proportional(m.title),
            color: text(),
            ..Default::default()
        },
    );
    job.wrap.max_width = text_w;
    job.wrap.max_rows = 2;
    job.wrap.break_anywhere = false;
    job.wrap.overflow_character = Some('…');
    let title = painter.layout_job(job);
    let title_pos = egui::pos2(rect.min.x + m.pad, thumb.max.y + m.pad);
    painter.galley(title_pos, title, text());

    // channel · views
    let views: String = [v.views_label(), v.age.as_deref().map(crate::i18n::age)]
        .into_iter()
        .flatten()
        .map(|s| format!("  ·  {s}"))
        .collect();
    let views_g = painter.layout_no_wrap(views, FontId::proportional(m.meta), text_dim());
    let mut cjob = LayoutJob::single_section(
        v.channel().to_string(),
        TextFormat {
            font_id: FontId::proportional(m.meta),
            color: text_dim(),
            ..Default::default()
        },
    );
    cjob.wrap.max_width = (text_w - views_g.size().x).max(40.0);
    cjob.wrap.max_rows = 1;
    cjob.wrap.overflow_character = Some('…');
    let chan = painter.layout_job(cjob);
    let chan_rect = egui::Rect::from_min_size(
        egui::pos2(title_pos.x, rect.max.y - m.pad - chan.size().y),
        chan.size(),
    );
    // The channel name is its own link (when we know the channel's URL).
    let chan_resp = (v.channel_url.is_some() && !v.channel().is_empty()).then(|| {
        ui.interact(
            chan_rect.expand(2.0),
            resp.id.with("channel"),
            Sense::click(),
        )
    });
    let chan_hover = chan_resp.as_ref().is_some_and(|r| r.hovered());
    painter.galley(
        chan_rect.min,
        chan,
        if chan_hover { text() } else { text_dim() },
    );
    painter.galley(
        egui::pos2(chan_rect.right(), chan_rect.top()),
        views_g,
        text_dim(),
    );
    if chan_hover {
        painter.line_segment(
            [chan_rect.left_bottom(), chan_rect.right_bottom()],
            Stroke::new(1.0, text_dim()),
        );
    }

    // outline when playing
    if playing {
        painter.rect_stroke(rect, r, Stroke::new(2.0, accent()), StrokeKind::Outside);
    }

    // downloaded: small badge top-left; downloading: progress along the bottom
    match download {
        Some(p) if p >= 1.0 => {
            let c = thumb.left_top() + Vec2::new(16.0, 16.0);
            painter.circle_filled(c, 10.0, Color32::from_black_alpha(170));
            paint_icon(&painter, Icon::Download, c, Color32::WHITE);
        }
        Some(p) => {
            let bar = egui::Rect::from_min_max(
                thumb.left_bottom() - Vec2::new(0.0, 4.0),
                egui::Pos2::new(thumb.left() + thumb.width() * p.max(0.02), thumb.bottom()),
            );
            painter.rect_filled(bar, 0, accent());
        }
        None => {}
    }

    // ⋯ menu button on hover, top-right of the thumbnail
    let mut action = CardAction::None;
    let menu_rect = egui::Rect::from_center_size(
        thumb.right_top() + Vec2::new(-20.0, 20.0),
        Vec2::splat(28.0),
    );
    let menu_resp = ui.interact(menu_rect, resp.id.with("menu"), Sense::click());
    let menu_open = egui::Popup::is_id_open(&ctx, egui::Popup::default_response_id(&menu_resp));
    if hover > 0.0 || menu_open {
        let a = if menu_open { 1.0 } else { hover };
        let fill = Color32::from_black_alpha(if menu_resp.hovered() { 210 } else { 150 })
            .gamma_multiply(a);
        painter.circle_filled(menu_rect.center(), 13.0, fill);
        painter.text(
            menu_rect.center(),
            Align2::CENTER_CENTER,
            "⋯",
            FontId::proportional(16.0),
            Color32::WHITE.gamma_multiply(a),
        );
    }
    let menu = |ui: &mut egui::Ui, action: &mut CardAction| {
        ui.set_min_width(190.0);
        if !v.is_playlist {
            if ui.button(t("Play next")).clicked() {
                *action = CardAction::Quick(Quick::PlayNext);
            }
            if ui.button(t("Add to queue")).clicked() {
                *action = CardAction::Quick(Quick::AddToQueue);
            }
            ui.separator();
            if ui.button(t("Save to playlist…")).clicked() {
                *action = CardAction::Quick(Quick::SaveTo);
            }
            if in_watch_later {
                if ui.button(t("Remove from Watch later")).clicked() {
                    *action = CardAction::Quick(Quick::WatchLater(false));
                }
            } else if ui.button(t("Save to Watch later")).clicked() {
                *action = CardAction::Quick(Quick::WatchLater(true));
            }
            let (label, to) = if watched {
                (t("Mark as unwatched"), false)
            } else {
                (t("Mark as watched"), true)
            };
            if ui.button(label).clicked() {
                *action = CardAction::Quick(Quick::Watched(to));
            }
            if in_history && ui.button(t("Remove from watch history")).clicked() {
                *action = CardAction::Quick(Quick::RemoveFromHistory);
            }
            ui.separator();
        }
        if v.channel_url.is_some() && ui.button(t("Go to channel")).clicked() {
            *action = CardAction::Channel;
        }
        if !v.is_playlist {
            match download {
                None => {
                    if ui.button(t("Download")).clicked() {
                        *action = CardAction::Quick(Quick::Download);
                    }
                }
                Some(p) if p < 1.0 => {
                    ui.add_enabled(
                        false,
                        egui::Button::new(tf("Downloading… {}%", &[&format!("{:.0}", p * 100.0)])),
                    );
                }
                Some(_) => {
                    if ui.button(t("Delete download")).clicked() {
                        *action = CardAction::Quick(Quick::DeleteDownload);
                    }
                }
            }
        }
        if v.channel.is_some() && !v.is_playlist && ui.button(t("Hide this channel")).clicked() {
            *action = CardAction::Quick(Quick::MuteChannel);
        }
        if ui.button(t("Copy link")).clicked() {
            *action = CardAction::Quick(Quick::CopyLink);
        }
        if ui.button(t("Open in browser")).clicked() {
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
    let resp = resp
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_ui(|ui| {
            ui.set_max_width(340.0);
            ui.label(RichText::new(v.title()).strong().color(text()));
            let mut meta = v.channel().to_string();
            for extra in [v.views_label(), v.age.as_deref().map(crate::i18n::age)]
                .into_iter()
                .flatten()
            {
                meta = if meta.is_empty() {
                    extra
                } else {
                    format!("{meta} · {extra}")
                };
            }
            if let Some(p) = progress.filter(|p| p.dur > 0.0 && !p.watched()) {
                meta.push_str(" · ");
                meta.push_str(&tf("{} left", &[&fmt_secs(p.dur - p.pos)]));
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
            painter.rect_filled(
                th,
                CornerRadius {
                    nw: RADIUS,
                    ne: RADIUS,
                    sw: 0,
                    se: 0,
                },
                bar,
            );
            let l1 = egui::Rect::from_min_size(
                min + Vec2::new(10.0, thumb_h + 12.0),
                Vec2::new(size.x * 0.8, 10.0),
            );
            let l2 = egui::Rect::from_min_size(
                min + Vec2::new(10.0, thumb_h + 30.0),
                Vec2::new(size.x * 0.45, 9.0),
            );
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

/// A category pill (YouTube's style: solid when selected).
fn topic_chip(ui: &mut egui::Ui, label: &str, on: bool) -> egui::Response {
    ui.add(
        egui::Button::new(
            RichText::new(label)
                .size(13.0)
                .color(if on { bg() } else { text() }),
        )
        .fill(if on { text() } else { card_hover() })
        .stroke(Stroke::NONE)
        .corner_radius(8)
        .min_size(Vec2::new(0.0, 26.0)),
    )
    .on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A toggle pill: filled when on.
fn chip(ui: &mut egui::Ui, label: &str, on: bool) -> egui::Response {
    let text = RichText::new(label)
        .size(13.0)
        .color(if on { text() } else { text_dim() });
    ui.add(
        egui::Button::new(text)
            .fill(if on {
                card_hover()
            } else {
                Color32::TRANSPARENT
            })
            .stroke(Stroke::new(
                1.0,
                if on {
                    Color32::TRANSPARENT
                } else {
                    card_hover()
                },
            ))
            .corner_radius(14)
            .min_size(Vec2::new(0.0, 26.0)),
    )
    .on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn tab_button(ui: &mut egui::Ui, label: &str, selected: bool) -> egui::Response {
    let text = RichText::new(label)
        .size(15.0)
        .color(if selected { text() } else { text_dim() });
    let btn = egui::Button::new(text)
        .fill(if selected {
            card_hover()
        } else {
            Color32::TRANSPARENT
        })
        .stroke(Stroke::NONE)
        .corner_radius(8)
        .min_size(Vec2::new(0.0, 30.0));
    let resp = ui.add(btn);
    if selected {
        let r = resp.rect;
        ui.painter().line_segment(
            [
                egui::pos2(r.left() + 10.0, r.bottom() + 3.0),
                egui::pos2(r.right() - 10.0, r.bottom() + 3.0),
            ],
            Stroke::new(2.0, accent()),
        );
    }
    resp
}

#[derive(Clone, Copy)]
enum Icon {
    Download,
    Music,
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
        Source::Music => Icon::Music,
        Source::WatchLater => Icon::Later,
        Source::History => Icon::History,
        Source::Playlists => Icon::Lists,
        Source::Downloads => Icon::Download,
        _ => Icon::Search,
    }
}

/// Small line icons drawn with the painter (no icon font needed). `c` = centre, ~16 pt box.
fn paint_icon(p: &egui::Painter, icon: Icon, c: egui::Pos2, color: Color32) {
    use egui::pos2;
    let st = Stroke::new(1.5, color);
    let v = |x: f32, y: f32| c + Vec2::new(x, y);
    match icon {
        Icon::Music => crate::theme::paint_note(p, c, 1.0, color),
        Icon::Home => {
            p.line(vec![v(-7.0, -0.5), v(0.0, -7.0), v(7.0, -0.5)], st);
            p.line(
                vec![v(-5.0, -2.0), v(-5.0, 6.5), v(5.0, 6.5), v(5.0, -2.0)],
                st,
            );
            p.line(
                vec![v(-1.8, 6.5), v(-1.8, 2.0), v(1.8, 2.0), v(1.8, 6.5)],
                st,
            );
        }
        Icon::Subs => {
            p.rect_stroke(
                egui::Rect::from_center_size(c + Vec2::new(0.0, 1.0), Vec2::new(15.0, 11.0)),
                2.5,
                st,
                StrokeKind::Middle,
            );
            p.line_segment([v(-4.5, -7.0), v(4.5, -7.0)], st);
            p.add(egui::Shape::convex_polygon(
                vec![v(-2.0, -2.0), v(3.5, 1.0), v(-2.0, 4.0)],
                color,
                Stroke::NONE,
            ));
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
            p.add(egui::Shape::convex_polygon(
                vec![
                    tip + Vec2::new(-3.5, -1.0),
                    tip + Vec2::new(2.5, -2.5),
                    tip + Vec2::new(0.5, 3.0),
                ],
                color,
                Stroke::NONE,
            ));
            p.line(vec![v(0.0, -3.5), v(0.0, 0.0), v(2.5, 2.0)], st);
        }
        Icon::Download => {
            p.line_segment([v(0.0, -7.0), v(0.0, 3.0)], st);
            p.line_segment([v(-4.0, -1.0), v(0.0, 3.0)], st);
            p.line_segment([v(4.0, -1.0), v(0.0, 3.0)], st);
            p.line_segment([v(-7.0, 7.0), v(7.0, 7.0)], st);
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
            let s = if matches!(icon, Icon::Collapse) {
                1.0
            } else {
                -1.0
            };
            for dx in [-2.5, 2.5] {
                p.line(
                    vec![
                        pos2(c.x + dx + 2.5 * s, c.y - 5.0),
                        pos2(c.x + dx - 2.5 * s, c.y),
                        pos2(c.x + dx + 2.5 * s, c.y + 5.0),
                    ],
                    st,
                );
            }
        }
    }
}

/// A sidebar row: icon + label, rounded highlight when selected (macOS source-list style).
fn nav_item(
    ui: &mut egui::Ui,
    icon: Icon,
    label: &str,
    selected: bool,
    collapsed: bool,
) -> egui::Response {
    let (rect, resp) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 30.0), Sense::click());
    let hover = ui
        .ctx()
        .animate_bool_with_time(resp.id, resp.hovered(), 0.12);
    // macOS source-list style: neutral highlight, accent-coloured icon
    let sel = if crate::theme::pal().dark {
        Color32::from_white_alpha(22)
    } else {
        Color32::from_black_alpha(18)
    };
    let fill = if selected {
        sel
    } else {
        sel.gamma_multiply(0.6 * hover)
    };
    ui.painter().rect_filled(rect, 6, fill);
    let icon_color = if selected { accent() } else { text_dim() };
    let icon_x = if collapsed {
        rect.center().x
    } else {
        rect.left() + 18.0
    };
    paint_icon(
        ui.painter(),
        icon,
        egui::pos2(icon_x, rect.center().y),
        icon_color,
    );
    if !collapsed {
        let g = ui.painter().layout(
            label.to_string(),
            FontId::proportional(14.0),
            text(),
            rect.width() - 46.0,
        );
        let g = if g.rows.len() > 1 {
            let mut job = LayoutJob::single_section(
                label.to_string(),
                TextFormat {
                    font_id: FontId::proportional(14.0),
                    color: text(),
                    ..Default::default()
                },
            );
            job.wrap.max_width = rect.width() - 46.0;
            job.wrap.max_rows = 1;
            job.wrap.overflow_character = Some('…');
            ui.painter().layout_job(job)
        } else {
            g
        };
        ui.painter().galley(
            egui::pos2(rect.left() + 36.0, rect.center().y - g.size().y / 2.0),
            g,
            text(),
        );
    }
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    if collapsed {
        resp.on_hover_text(label)
    } else {
        resp
    }
}

/// The "mYT" badge: same look as the app icon (coral rounded square, white lettering).
fn logo(ui: &mut egui::Ui, h: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(h), Sense::hover());
    let p = ui.painter();
    let top = Color32::from_rgb(0xFF, 0x7A, 0x5C);
    let bottom = Color32::from_rgb(0xE8, 0x3A, 0x4E);
    // vertical gradient in a rounded square: rounded fill + gradient mesh inset by the radius
    let r = h * 0.225;
    p.rect_filled(rect, r, lerp_color(top, bottom, 0.5));
    let mut mesh = egui::Mesh::default();
    let inner = rect.shrink2(Vec2::new(0.0, r));
    let i = mesh.vertices.len() as u32;
    mesh.colored_vertex(inner.left_top(), top);
    mesh.colored_vertex(inner.right_top(), top);
    mesh.colored_vertex(inner.left_bottom(), bottom);
    mesh.colored_vertex(inner.right_bottom(), bottom);
    mesh.add_triangle(i, i + 1, i + 2);
    mesh.add_triangle(i + 1, i + 3, i + 2);
    p.rect_filled(
        egui::Rect::from_min_max(rect.min, egui::pos2(rect.max.x, inner.min.y)),
        CornerRadius {
            nw: r as u8,
            ne: r as u8,
            sw: 0,
            se: 0,
        },
        top,
    );
    p.rect_filled(
        egui::Rect::from_min_max(egui::pos2(rect.min.x, inner.max.y), rect.max),
        CornerRadius {
            nw: 0,
            ne: 0,
            sw: r as u8,
            se: r as u8,
        },
        bottom,
    );
    p.add(mesh);
    let mut job = LayoutJob::default();
    let fmt = |c: Color32| TextFormat {
        font_id: FontId::proportional(h * 0.36),
        color: c,
        ..Default::default()
    };
    job.append("m", 0.0, fmt(Color32::from_white_alpha(225)));
    job.append("YT", 0.0, fmt(Color32::WHITE));
    let g = p.layout_job(job);
    let pos = rect.center() - g.size() / 2.0;
    p.galley(pos, g.clone(), Color32::WHITE);
    p.galley(pos + Vec2::new(h * 0.012, 0.0), g, Color32::WHITE); // faux-bold
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
            ui.add(
                egui::Label::new(RichText::new(err).size(13.0).color(crate::theme::err())).wrap(),
            );
        });
}

fn code_box(ui: &mut egui::Ui, text: &str) {
    egui::Frame::new()
        .fill(card())
        .corner_radius(8)
        .inner_margin(Margin::same(10))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(text).monospace().color(crate::theme::text()));
                if ui.small_button(t("Copy")).clicked() {
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
