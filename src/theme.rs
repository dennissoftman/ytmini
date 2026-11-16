//! Colours and fonts. Follows the system: light/dark mode (live) and, on
//! macOS, the accent colour from System Settings → Appearance, with the
//! system font (SF Pro) when it's there.

use eframe::egui::{self, Color32, CornerRadius, FontFamily, FontId, Stroke, Vec2};
use std::cell::Cell;

#[derive(Clone, Copy, PartialEq)]
pub struct Palette {
    pub dark: bool,
    pub accent: Color32,
    /// Text/icons drawn on top of `accent`.
    pub on_accent: Color32,
    pub bg: Color32,
    pub sidebar: Color32,
    pub panel: Color32,
    pub card: Color32,
    pub card_hover: Color32,
    pub thumb_bg: Color32,
    pub text: Color32,
    pub text_dim: Color32,
    pub err: Color32,
    pub err_bg: Color32,
}

// Close to macOS's own window, sidebar and control colours.
const DARK: Palette = Palette {
    dark: true,
    accent: Color32::from_rgb(0x0A, 0x84, 0xFF),
    on_accent: Color32::WHITE,
    bg: Color32::from_rgb(0x1E, 0x1E, 0x20),
    sidebar: Color32::from_rgb(0x28, 0x28, 0x2B),
    panel: Color32::from_rgb(0x1E, 0x1E, 0x20),
    card: Color32::from_rgb(0x26, 0x26, 0x29),
    card_hover: Color32::from_rgb(0x32, 0x32, 0x36),
    thumb_bg: Color32::from_rgb(0x30, 0x30, 0x34),
    text: Color32::from_rgb(0xF2, 0xF2, 0xF5),
    text_dim: Color32::from_rgb(0x98, 0x98, 0x9F),
    err: Color32::from_rgb(0xFF, 0x6B, 0x63),
    err_bg: Color32::from_rgb(0x3A, 0x1D, 0x1D),
};

const LIGHT: Palette = Palette {
    dark: false,
    accent: Color32::from_rgb(0x00, 0x7A, 0xFF),
    on_accent: Color32::WHITE,
    bg: Color32::from_rgb(0xFF, 0xFF, 0xFF),
    sidebar: Color32::from_rgb(0xEE, 0xEE, 0xF0),
    panel: Color32::from_rgb(0xFF, 0xFF, 0xFF),
    card: Color32::from_rgb(0xF4, 0xF4, 0xF6),
    card_hover: Color32::from_rgb(0xE7, 0xE7, 0xEB),
    thumb_bg: Color32::from_rgb(0xDD, 0xDD, 0xE1),
    text: Color32::from_rgb(0x1D, 0x1D, 0x1F),
    text_dim: Color32::from_rgb(0x6E, 0x6E, 0x73),
    err: Color32::from_rgb(0xD7, 0x00, 0x15),
    err_bg: Color32::from_rgb(0xFD, 0xEB, 0xEC),
};

thread_local! {
    static PAL: Cell<Palette> = const { Cell::new(DARK) };
    /// System accent (light, dark), None = default blue.
    static SYS_ACCENT: Cell<Option<(Color32, Color32)>> = const { Cell::new(None) };
}

pub fn pal() -> Palette {
    PAL.with(|p| p.get())
}

macro_rules! getters {
    ($($name:ident),*) => { $(#[inline] pub fn $name() -> Color32 { pal().$name })* };
}
getters!(
    accent, on_accent, bg, sidebar, panel, card, card_hover, thumb_bg, text, text_dim, err, err_bg
);

/// macOS accent colour (`AppleAccentColor`): (light-mode, dark-mode) variants.
fn system_accent() -> Option<(Color32, Color32)> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let out = std::process::Command::new("defaults")
        .args(["read", "-g", "AppleAccentColor"])
        .output()
        .ok()?;
    let n: i32 = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?; // absent = multicolour (blue)
    let rgb = |r, g, b| Color32::from_rgb(r, g, b);
    Some(match n {
        -1 => (rgb(0x8C, 0x8C, 0x8C), rgb(0x98, 0x98, 0x98)), // graphite
        0 => (rgb(0xFF, 0x38, 0x3C), rgb(0xFF, 0x45, 0x3A)),  // red
        1 => (rgb(0xF7, 0x82, 0x1B), rgb(0xFF, 0x9F, 0x0A)),  // orange
        2 => (rgb(0xE0, 0xA8, 0x00), rgb(0xFF, 0xD6, 0x0A)),  // yellow
        3 => (rgb(0x28, 0xA7, 0x45), rgb(0x30, 0xD1, 0x58)),  // green
        5 => (rgb(0x95, 0x3D, 0x96), rgb(0xBF, 0x5A, 0xF2)),  // purple
        6 => (rgb(0xF7, 0x4F, 0x9E), rgb(0xFF, 0x37, 0x5F)),  // pink
        _ => return None,                                     // blue
    })
}

/// Call once at startup.
pub fn init(ctx: &egui::Context) {
    SYS_ACCENT.with(|a| a.set(system_accent()));
    add_fonts(ctx);
    // YTMINI_THEME=light|dark overrides the system (for testing).
    let pref = match std::env::var("YTMINI_THEME").as_deref() {
        Ok("light") => egui::ThemePreference::Light,
        Ok("dark") => egui::ThemePreference::Dark,
        _ => egui::ThemePreference::System,
    };
    ctx.options_mut(|o| o.theme_preference = pref);
    PAL.with(|p| {
        p.set(Palette {
            dark: !ctx.theme().default_visuals().dark_mode,
            ..DARK
        })
    }); // force first apply
    update(ctx);
}

/// Call every frame: picks up light/dark switches.
pub fn update(ctx: &egui::Context) {
    let dark = ctx.theme() == egui::Theme::Dark;
    if pal().dark == dark {
        return;
    }
    let mut p = if dark { DARK } else { LIGHT };
    if let Some((l, d)) = SYS_ACCENT.with(|a| a.get()) {
        p.accent = if dark { d } else { l };
        if p.accent == Color32::from_rgb(0xFF, 0xD6, 0x0A)
            || p.accent == Color32::from_rgb(0xE0, 0xA8, 0x00)
        {
            p.on_accent = Color32::BLACK; // yellow
        }
    }
    PAL.with(|c| c.set(p));
    apply_style(ctx, p);
}

pub const RADIUS: u8 = 10;

fn apply_style(ctx: &egui::Context, p: Palette) {
    ctx.all_styles_mut(|style| {
        use egui::TextStyle::*;
        style.text_styles = [
            (Heading, FontId::new(22.0, FontFamily::Proportional)),
            (Body, FontId::new(14.0, FontFamily::Proportional)),
            (Button, FontId::new(14.0, FontFamily::Proportional)),
            (Small, FontId::new(11.5, FontFamily::Proportional)),
            (Monospace, FontId::new(12.5, FontFamily::Monospace)),
        ]
        .into();
        style.spacing.item_spacing = Vec2::new(8.0, 6.0);
        style.spacing.button_padding = Vec2::new(10.0, 4.0);
        style.spacing.interact_size.y = 24.0;

        let v = &mut style.visuals;
        *v = if p.dark {
            egui::Visuals::dark()
        } else {
            egui::Visuals::light()
        };
        v.panel_fill = p.bg;
        v.window_fill = p.panel;
        v.window_stroke = Stroke::new(1.0, p.card_hover);
        v.extreme_bg_color = if p.dark {
            Color32::from_rgb(0x2A, 0x2A, 0x2D)
        } else {
            Color32::from_rgb(0xEE, 0xEE, 0xF0)
        }; // text fields
        v.faint_bg_color = p.card;
        v.override_text_color = None;
        v.hyperlink_color = p.accent;
        v.selection.bg_fill = p.accent.gamma_multiply(0.45);
        v.selection.stroke = Stroke::new(1.0, p.accent);
        v.window_corner_radius = CornerRadius::same(12);
        v.menu_corner_radius = CornerRadius::same(8);
        for w in [
            &mut v.widgets.noninteractive,
            &mut v.widgets.inactive,
            &mut v.widgets.hovered,
            &mut v.widgets.active,
            &mut v.widgets.open,
        ] {
            w.corner_radius = CornerRadius::same(6);
        }
        v.widgets.noninteractive.fg_stroke.color = p.text;
        v.widgets.noninteractive.bg_stroke.color = p.card_hover;
        v.widgets.inactive.fg_stroke.color = p.text;
        v.widgets.inactive.weak_bg_fill = p.card_hover;
        v.widgets.inactive.bg_fill = p.card_hover;
        v.widgets.hovered.weak_bg_fill = p.card_hover;
        v.widgets.hovered.bg_fill = p.card_hover;
        v.widgets.hovered.fg_stroke.color = p.text;
        v.widgets.hovered.bg_stroke = Stroke::new(1.0, p.accent.gamma_multiply(0.6));
        v.widgets.active.bg_stroke = Stroke::new(1.0, p.accent);
        v.widgets.open.weak_bg_fill = p.card_hover;
        v.text_cursor.stroke = Stroke::new(2.0, p.accent);
    });
}

/// SF Pro (macOS system font) as the main UI font when present, plus a broad
/// Unicode font as the last fallback: titles come in CJK, Arabic, Hindi…
fn add_fonts(ctx: &egui::Context) {
    use egui::epaint::text::{FontInsert, FontPriority, InsertFontFamily};
    let add = |name: &str, path: &str, priority: FontPriority, mono: bool| -> bool {
        let Ok(bytes) = std::fs::read(path) else {
            return false;
        };
        let mut fams = vec![InsertFontFamily {
            family: FontFamily::Proportional,
            priority,
        }];
        if mono {
            fams.push(InsertFontFamily {
                family: FontFamily::Monospace,
                priority: FontPriority::Lowest,
            });
        }
        ctx.add_font(FontInsert::new(
            name,
            egui::FontData::from_owned(bytes),
            fams,
        ));
        true
    };
    if cfg!(target_os = "macos") {
        add(
            "sf-pro",
            "/System/Library/Fonts/SFNS.ttf",
            FontPriority::Highest,
            false,
        );
    }
    let fallbacks: &[&str] = if cfg!(target_os = "macos") {
        &["/System/Library/Fonts/Supplemental/Arial Unicode.ttf"]
    } else if cfg!(windows) {
        &[
            "C:\\Windows\\Fonts\\arialuni.ttf",
            "C:\\Windows\\Fonts\\seguisym.ttf",
        ]
    } else {
        &[
            "/usr/share/fonts/truetype/droid/DroidSansFallbackFull.ttf",
            "/usr/share/fonts/google-droid-sans-fonts/DroidSansFallbackFull.ttf",
            "/usr/share/fonts/truetype/arphic/uming.ttc",
        ]
    };
    for path in fallbacks {
        if add("system-fallback", path, FontPriority::Lowest, true) {
            break;
        }
    }
}
