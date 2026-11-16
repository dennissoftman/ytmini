//! The eframe app: all UI state and drawing. Slow work (yt-dlp) runs on worker
//! threads and reports back over an mpsc channel.

use crate::thumbs::Thumbs;
use crate::yt::{self, Browser, Source, Tools, Video};
use eframe::egui::{
    self, text::LayoutJob, Align, Color32, CornerRadius, FontFamily, FontId, Key, Layout, Margin, RichText, Sense,
    Stroke, StrokeKind, TextFormat, Vec2,
};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    process::Child,
    sync::mpsc::{channel, Receiver, Sender},
    time::Duration,
};

// ---------- look ----------

const ACCENT: Color32 = Color32::from_rgb(0xFF, 0x5C, 0x4D); // warm coral
const BG: Color32 = Color32::from_rgb(0x0E, 0x0E, 0x12);
const PANEL: Color32 = Color32::from_rgb(0x15, 0x15, 0x1B);
const CARD: Color32 = Color32::from_rgb(0x1B, 0x1B, 0x23);
const CARD_HOVER: Color32 = Color32::from_rgb(0x26, 0x26, 0x31);
const TEXT: Color32 = Color32::from_rgb(0xEC, 0xEC, 0xF1);
const TEXT_DIM: Color32 = Color32::from_rgb(0x9A, 0x9A, 0xA8);
const ERR: Color32 = Color32::from_rgb(0xFF, 0x8A, 0x80);

const CARD_MIN_W: f32 = 250.0;
const GAP: f32 = 16.0;
const CARD_PAD: f32 = 10.0;
const TITLE_SIZE: f32 = 15.0;
const META_SIZE: f32 = 13.0;
const RADIUS: u8 = 12;

const BASE_TABS: [Source; 4] = [Source::Feed, Source::Subscriptions, Source::WatchLater, Source::History];

// ---------- state ----------

enum Screen {
    MissingTools(yt::Missing),
    Onboarding,
    Main,
}

enum Msg {
    Fetched { source: Source, generation: u64, start: usize, result: Result<Vec<Video>, String> },
    Imported(Result<(), String>),
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
}

struct Player {
    child: Child,
    title: String,
    id: String,
    /// e.g. "720p30 · battery saver"
    profile: String,
}

pub struct App {
    tools: Option<Tools>,
    cookies: PathBuf,
    screen: Screen,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,

    // onboarding
    browsers: Vec<Browser>,
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
    quality: yt::Quality,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let ctx = &cc.egui_ctx;
        setup_style(ctx);
        add_fallback_fonts(ctx);

        let (tx, rx) = channel();
        let cookies = yt::cookie_file().unwrap_or_else(|_| PathBuf::from("cookies.txt"));
        let mut app = App {
            tools: None,
            cookies,
            screen: Screen::Onboarding,
            tx,
            rx,
            browsers: yt::detect_browsers(),
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
            thumbs: Thumbs::new(ctx),
            quality: yt::Quality::Auto,
        };
        app.check_tools(ctx);
        app
    }

    fn check_tools(&mut self, ctx: &egui::Context) {
        match Tools::locate() {
            Ok(t) => {
                if t.deno.is_none() {
                    self.status = Some(("deno not found — YouTube may fail. Install it: brew install deno".into(), true));
                }
                self.tools = Some(t);
                if yt::logged_in(&self.cookies) {
                    self.screen = Screen::Main;
                    self.load(ctx, Source::Feed, false);
                } else {
                    self.screen = Screen::Onboarding;
                }
            }
            Err(m) => self.screen = Screen::MissingTools(m),
        }
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
                        }
                        Err(e) => tab.error = Some(e),
                    }
                }
                Msg::Imported(r) => {
                    self.importing = None;
                    match r {
                        Ok(()) => {
                            self.import_error = None;
                            self.screen = Screen::Main;
                            self.tabs.clear(); // new account/cookies: drop everything cached
                            self.status = Some(("Signed in — cookies imported.".into(), false));
                            self.load(ctx, self.active.clone(), true);
                        }
                        Err(e) => self.import_error = Some(e),
                    }
                }
            }
        }
    }

    // ---------- player ----------

    fn play(&mut self, video: &Video) {
        self.stop_player();
        let Some(tools) = &self.tools else { return };
        let profile = yt::profile(self.quality, yt::power_state());
        match yt::play(tools, &self.cookies, video, &profile) {
            Ok(child) => {
                self.status = None;
                self.player = Some(Player {
                    child,
                    title: video.title().to_string(),
                    id: video.id.clone(),
                    profile: profile.label,
                });
            }
            Err(e) => self.status = Some((format!("Couldn't start mpv: {e:#}"), true)),
        }
    }

    fn stop_player(&mut self) {
        if let Some(mut p) = self.player.take() {
            let _ = p.child.kill();
            let _ = p.child.wait(); // reap — no zombies
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
            ui.label(RichText::new("ytmini").size(34.0).strong().color(ACCENT));
            ui.add_space(8.0);
            ui.label(RichText::new("A couple of tools are missing").size(20.0).color(TEXT));
            ui.add_space(12.0);
            let names: Vec<&str> =
                [("yt-dlp", missing.ytdlp), ("mpv", missing.mpv)].iter().filter(|x| x.1).map(|x| x.0).collect();
            ui.label(format!("ytmini needs {} to list and play videos. Install with:", names.join(" and ")));
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

    fn ui_onboarding(&mut self, ui: &mut egui::Ui) {
        let mut chosen: Option<String> = None;
        let mut cancel = false;
        let can_cancel = yt::logged_in(&self.cookies) && self.importing.is_none();
        let busy = self.importing.is_some();
        let browsers = self.browsers.clone();

        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            centered_column(ui, 560.0, |ui| {
                ui.label(RichText::new("ytmini").size(34.0).strong().color(ACCENT));
                ui.add_space(4.0);
                ui.label(RichText::new("Sign in with your browser").size(20.0).color(TEXT));
                ui.add_space(6.0);
                ui.label(
                    RichText::new(
                        "ytmini borrows your YouTube login from a browser you're already signed in to. \
                         Cookies are stored only on this computer.",
                    )
                    .color(TEXT_DIM),
                );
                ui.add_space(18.0);

                if browsers.is_empty() {
                    ui.label(RichText::new("No browsers detected — type a yt-dlp spec below.").color(TEXT_DIM));
                }
                for b in &browsers {
                    egui::Frame::new()
                        .fill(CARD)
                        .corner_radius(RADIUS)
                        .inner_margin(Margin::symmetric(14, 10))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.horizontal(|ui| {
                                ui.vertical(|ui| {
                                    ui.label(RichText::new(b.name).size(16.0).strong().color(TEXT));
                                    ui.add(
                                        egui::Label::new(RichText::new(yt::browser_note(b.id)).size(12.5).color(TEXT_DIM))
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
                ui.label(RichText::new("Or a yt-dlp browser spec").color(TEXT_DIM));
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
                    ui.label(RichText::new(yt::browser_note(&self.custom_spec)).size(12.5).color(TEXT_DIM));
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
                        .fill(Color32::from_rgb(0x33, 0x18, 0x1A))
                        .corner_radius(RADIUS)
                        .inner_margin(Margin::same(12))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.label(RichText::new("Import failed").strong().color(ERR));
                            ui.add(egui::Label::new(RichText::new(err).monospace().size(12.0).color(TEXT)).wrap());
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
        let mut reimport = false;

        // ----- top bar -----
        egui::Panel::top("top")
            .frame(egui::Frame::new().fill(PANEL).inner_margin(Margin::symmetric(16, 10)))
            .show_separator_line(false)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("ytmini").size(20.0).strong().color(ACCENT));
                    ui.add_space(14.0);
                    let mut tabs: Vec<Source> = BASE_TABS.to_vec();
                    if let Some(s) = &self.last_search {
                        tabs.push(s.clone());
                    }
                    for t in tabs {
                        if tab_button(ui, t.label(), t == self.active).clicked() {
                            switch_to = Some(t);
                        }
                    }

                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.menu_button(RichText::new("☰").size(18.0), |ui| {
                            ui.label(RichText::new("Playback quality").small().color(TEXT_DIM));
                            for q in yt::Quality::ALL {
                                if ui.radio_value(&mut self.quality, q, q.label()).clicked() {
                                    ui.close();
                                }
                            }
                            ui.separator();
                            if ui.button("Re-import cookies…").clicked() {
                                reimport = true;
                                ui.close();
                            }
                        })
                        .response
                        .on_hover_text("Settings");
                        if ui.add(egui::Button::new(RichText::new("↻").size(19.0)).frame(false)).on_hover_text("Refresh (⌘R)").clicked() {
                            refresh = true;
                        }
                        ui.add_space(6.0);
                        let r = ui.add(
                            egui::TextEdit::singleline(&mut self.search_query)
                                .hint_text("Search YouTube")
                                .desired_width(260.0)
                                .margin(Margin::symmetric(10, 6)),
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
                    });
                });
            });

        // ----- status bar -----
        let active_loading = self.tabs.get(&self.active).is_some_and(|t| t.loading);
        let mut stop = false;
        egui::Panel::bottom("status")
            .frame(egui::Frame::new().fill(PANEL).inner_margin(Margin::symmetric(16, 6)))
            .show_separator_line(false)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    if active_loading {
                        ui.spinner();
                        ui.label(RichText::new("Loading…").color(TEXT_DIM));
                    } else if let Some(p) = &self.player {
                        ui.label(RichText::new("▶").color(ACCENT));
                        ui.label(RichText::new(format!("Playing: {}", p.title)).color(TEXT));
                        ui.label(RichText::new(&p.profile).color(TEXT_DIM));
                        if ui.small_button("Stop").clicked() {
                            stop = true;
                        }
                    } else if let Some((msg, is_err)) = &self.status {
                        ui.label(RichText::new(msg).color(if *is_err { ERR } else { TEXT_DIM }));
                    } else {
                        let n = self.tabs.get(&self.active).map_or(0, |t| t.videos.len());
                        ui.label(RichText::new(format!("{n} videos")).color(TEXT_DIM));
                    }
                });
            });
        if stop {
            self.stop_player();
        }

        // ----- grid -----
        let mut clicked: Option<Video> = None;
        let mut more = false;
        let mut reimport_from_empty = false;
        egui::CentralPanel::no_frame().show(ui, |ui| {
            let source = self.active.clone();
            let playing_id = self.player.as_ref().map(|p| p.id.clone());
            let thumbs = &mut self.thumbs;
            let Some(tab) = self.tabs.get_mut(&source) else { return };
            let avail_w = ui.available_width() - 2.0 * GAP;

            if tab.videos.is_empty() {
                centered_column(ui, 520.0, |ui| {
                    if tab.loading {
                        ui.spinner();
                    } else if let Some(e) = &tab.error {
                        error_box(ui, e);
                        if source == Source::Feed {
                            ui.add_space(8.0);
                            if ui.button("Re-import cookies…").clicked() {
                                reimport_from_empty = true;
                            }
                        }
                    } else if tab.loaded_once {
                        if source == Source::Feed {
                            ui.label(RichText::new("Your feed came back empty").size(18.0).color(TEXT));
                            ui.label(
                                RichText::new("That usually means the cookies expired or were rotated.")
                                    .color(TEXT_DIM),
                            );
                            ui.add_space(10.0);
                            if accent_button(ui, "Re-import cookies").clicked() {
                                reimport_from_empty = true;
                            }
                        } else {
                            ui.label(RichText::new("Nothing here.").color(TEXT_DIM));
                        }
                    }
                });
                return;
            }

            let cols = (((avail_w + GAP) / (CARD_MIN_W + GAP)).floor() as usize).max(1);
            let card_w = (avail_w - GAP * (cols as f32 - 1.0)) / cols as f32;
            let thumb_h = card_w * 9.0 / 16.0;
            let title_h = ui.fonts_mut(|f| f.row_height(&FontId::proportional(TITLE_SIZE)));
            let meta_h = ui.fonts_mut(|f| f.row_height(&FontId::proportional(META_SIZE)));
            let card_h = thumb_h + CARD_PAD + title_h * 2.0 + 4.0 + meta_h + CARD_PAD;
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
                for row in range {
                    ui.horizontal(|ui| {
                        ui.add_space(GAP);
                        ui.spacing_mut().item_spacing.x = GAP;
                        if row < video_rows {
                            for v in tab.videos.iter().skip(row * cols).take(cols) {
                                let playing = playing_id.as_deref() == Some(v.id.as_str());
                                if video_card(ui, thumbs, v, Vec2::new(card_w, card_h), thumb_h, playing).clicked() {
                                    clicked = Some(v.clone());
                                }
                            }
                        } else {
                            // footer row: load more / error
                            ui.allocate_ui_with_layout(
                                Vec2::new(avail_w, card_h),
                                Layout::top_down(Align::Center),
                                |ui| {
                                    ui.add_space(12.0);
                                    if tab.loading {
                                        ui.spinner();
                                    } else {
                                        if let Some(e) = &tab.error {
                                            error_box(ui, e);
                                            ui.add_space(8.0);
                                        }
                                        if !tab.exhausted && accent_button(ui, "Load more").clicked() {
                                            more = true;
                                        }
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
            self.play(&v);
        }
        if more {
            self.load_more(&ctx, self.active.clone());
        }
        if let Some(q) = search {
            let s = Source::Search(q);
            self.tabs.remove(&s);
            if let Some(old) = self.last_search.replace(s.clone()) {
                if old != s {
                    self.tabs.remove(&old);
                }
            }
            self.active = s.clone();
            self.load(&ctx, s, true);
        }
        if let Some(t) = switch_to {
            self.active = t.clone();
            self.load(&ctx, t, false);
        }
        if refresh {
            self.load(&ctx, self.active.clone(), true);
        }
        if reimport || reimport_from_empty {
            self.import_error = None;
            self.browsers = yt::detect_browsers();
            self.screen = Screen::Onboarding;
        }
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.handle_messages(ctx);
        self.thumbs.poll(ctx);
        self.poll_player();
        if self.player.is_some() {
            // Nobody else wakes us when mpv exits; check twice a second.
            ctx.request_repaint_after(Duration::from_millis(500));
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::Frame::new().fill(BG).show(ui, |ui| {
            ui.set_min_size(ui.available_size());
            match &self.screen {
                Screen::MissingTools(m) => {
                    let m = m.clone();
                    self.ui_missing(ui, &m);
                }
                Screen::Onboarding => self.ui_onboarding(ui),
                Screen::Main => self.ui_main(ui),
            }
        });
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.stop_player();
    }
}

// ---------- widgets ----------

fn video_card(ui: &mut egui::Ui, thumbs: &mut Thumbs, v: &Video, size: Vec2, thumb_h: f32, playing: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    if !ui.is_rect_visible(rect) {
        return resp;
    }
    let hover = ui.ctx().animate_bool(resp.id, resp.hovered());
    let painter = ui.painter_at(rect.expand(2.0));
    let r = CornerRadius::same(RADIUS);
    painter.rect_filled(rect, r, lerp_color(CARD, CARD_HOVER, hover));

    // thumbnail
    let thumb = egui::Rect::from_min_size(rect.min, Vec2::new(rect.width(), thumb_h));
    let tr = CornerRadius { nw: RADIUS, ne: RADIUS, sw: 0, se: 0 };
    painter.rect_filled(thumb, tr, Color32::from_rgb(0x24, 0x24, 0x2D));
    if let Some(tex) = thumbs.get(&v.thumbnail()) {
        egui::Image::from_texture(egui::load::SizedTexture::from_handle(tex))
            .corner_radius(tr)
            .paint_at(ui, thumb);
    }

    // duration badge
    let dur = v.duration_label();
    let live = dur == "LIVE";
    let g = painter.layout_no_wrap(dur, FontId::proportional(12.0), Color32::WHITE);
    let badge = egui::Rect::from_min_size(
        thumb.right_bottom() - g.size() - Vec2::new(14.0, 10.0),
        g.size() + Vec2::new(8.0, 4.0),
    );
    painter.rect_filled(badge, 5, if live { ACCENT } else { Color32::from_black_alpha(200) });
    painter.galley(badge.min + Vec2::new(4.0, 2.0), g, Color32::WHITE);

    // title (max 2 lines) + channel
    let text_w = rect.width() - 2.0 * CARD_PAD;
    let mut job = LayoutJob::single_section(
        v.title().to_string(),
        TextFormat { font_id: FontId::proportional(TITLE_SIZE), color: TEXT, ..Default::default() },
    );
    job.wrap.max_width = text_w;
    job.wrap.max_rows = 2;
    job.wrap.break_anywhere = false;
    job.wrap.overflow_character = Some('…');
    let title = painter.layout_job(job);
    let title_pos = egui::pos2(rect.min.x + CARD_PAD, thumb.max.y + CARD_PAD);
    painter.galley(title_pos, title, TEXT);

    let mut cjob = LayoutJob::single_section(
        v.channel().to_string(),
        TextFormat { font_id: FontId::proportional(META_SIZE), color: TEXT_DIM, ..Default::default() },
    );
    cjob.wrap.max_width = text_w;
    cjob.wrap.max_rows = 1;
    cjob.wrap.overflow_character = Some('…');
    let chan = painter.layout_job(cjob);
    painter.galley(egui::pos2(title_pos.x, rect.max.y - CARD_PAD - chan.size().y), chan, TEXT_DIM);

    // outline: accent when playing, faint accent on hover
    if playing {
        painter.rect_stroke(rect, r, Stroke::new(2.0, ACCENT), StrokeKind::Outside);
    } else if hover > 0.0 {
        painter.rect_stroke(rect, r, Stroke::new(1.0, ACCENT.gamma_multiply(0.5 * hover)), StrokeKind::Outside);
    }

    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn tab_button(ui: &mut egui::Ui, label: &str, selected: bool) -> egui::Response {
    let text = RichText::new(label).size(15.0).color(if selected { TEXT } else { TEXT_DIM });
    let btn = egui::Button::new(text)
        .fill(if selected { CARD_HOVER } else { Color32::TRANSPARENT })
        .stroke(Stroke::NONE)
        .corner_radius(8)
        .min_size(Vec2::new(0.0, 30.0));
    let resp = ui.add(btn);
    if selected {
        let r = resp.rect;
        ui.painter().line_segment(
            [egui::pos2(r.left() + 10.0, r.bottom() + 3.0), egui::pos2(r.right() - 10.0, r.bottom() + 3.0)],
            Stroke::new(2.0, ACCENT),
        );
    }
    resp
}

fn accent_btn(label: &str) -> egui::Button<'_> {
    egui::Button::new(RichText::new(label).strong().color(Color32::WHITE))
        .fill(ACCENT)
        .corner_radius(8)
        .min_size(Vec2::new(72.0, 30.0))
}

fn accent_button(ui: &mut egui::Ui, label: &str) -> egui::Response {
    ui.add(accent_btn(label))
}

fn error_box(ui: &mut egui::Ui, err: &str) {
    egui::Frame::new()
        .fill(Color32::from_rgb(0x33, 0x18, 0x1A))
        .corner_radius(RADIUS)
        .inner_margin(Margin::same(12))
        .show(ui, |ui| {
            ui.add(egui::Label::new(RichText::new(err).size(13.0).color(ERR)).wrap());
        });
}

fn code_box(ui: &mut egui::Ui, text: &str) {
    egui::Frame::new().fill(CARD).corner_radius(8).inner_margin(Margin::same(10)).show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label(RichText::new(text).monospace().color(TEXT));
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

// ---------- style ----------

fn setup_style(ctx: &egui::Context) {
    ctx.set_theme(egui::Theme::Dark);
    ctx.all_styles_mut(|style| {
        use egui::TextStyle::*;
        style.text_styles = [
            (Heading, FontId::new(22.0, FontFamily::Proportional)),
            (Body, FontId::new(15.0, FontFamily::Proportional)),
            (Button, FontId::new(15.0, FontFamily::Proportional)),
            (Small, FontId::new(12.0, FontFamily::Proportional)),
            (Monospace, FontId::new(13.0, FontFamily::Monospace)),
        ]
        .into();
        style.spacing.item_spacing = Vec2::new(8.0, 6.0);
        style.spacing.button_padding = Vec2::new(10.0, 5.0);

        let v = &mut style.visuals;
        *v = egui::Visuals::dark();
        v.panel_fill = BG;
        v.window_fill = PANEL;
        v.extreme_bg_color = Color32::from_rgb(0x0A, 0x0A, 0x0D);
        v.override_text_color = None;
        v.hyperlink_color = ACCENT;
        v.selection.bg_fill = ACCENT.gamma_multiply(0.45);
        v.selection.stroke = Stroke::new(1.0, ACCENT);
        v.window_corner_radius = CornerRadius::same(RADIUS);
        v.menu_corner_radius = CornerRadius::same(10);
        for w in [
            &mut v.widgets.noninteractive,
            &mut v.widgets.inactive,
            &mut v.widgets.hovered,
            &mut v.widgets.active,
            &mut v.widgets.open,
        ] {
            w.corner_radius = CornerRadius::same(8);
        }
        v.widgets.noninteractive.fg_stroke.color = TEXT;
        v.widgets.inactive.fg_stroke.color = TEXT;
        v.widgets.inactive.weak_bg_fill = CARD;
        v.widgets.inactive.bg_fill = CARD;
        v.widgets.hovered.weak_bg_fill = CARD_HOVER;
        v.widgets.hovered.bg_fill = CARD_HOVER;
        v.widgets.hovered.bg_stroke = Stroke::new(1.0, ACCENT.gamma_multiply(0.6));
        v.widgets.active.bg_stroke = Stroke::new(1.0, ACCENT);
        v.text_cursor.stroke = Stroke::new(2.0, ACCENT);
    });
}

/// egui's built-in fonts cover Latin/Cyrillic/Greek. Titles also come in CJK,
/// Arabic, Hindi… so add a broad system font as a fallback if we can find one.
fn add_fallback_fonts(ctx: &egui::Context) {
    let candidates: &[&str] = if cfg!(target_os = "macos") {
        &["/System/Library/Fonts/Supplemental/Arial Unicode.ttf"]
    } else if cfg!(windows) {
        &["C:\\Windows\\Fonts\\arialuni.ttf", "C:\\Windows\\Fonts\\seguisym.ttf"]
    } else {
        &[
            "/usr/share/fonts/truetype/droid/DroidSansFallbackFull.ttf",
            "/usr/share/fonts/google-droid-sans-fonts/DroidSansFallbackFull.ttf",
            "/usr/share/fonts/truetype/arphic/uming.ttc",
        ]
    };
    for path in candidates {
        if let Ok(bytes) = std::fs::read(path) {
            use egui::epaint::text::{FontInsert, FontPriority, InsertFontFamily};
            let fam = |family| InsertFontFamily { family, priority: FontPriority::Lowest };
            ctx.add_font(FontInsert::new(
                "system-fallback",
                egui::FontData::from_owned(bytes),
                vec![fam(FontFamily::Proportional), fam(FontFamily::Monospace)],
            ));
            break;
        }
    }
}
