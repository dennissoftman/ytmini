//! Menu-bar (macOS) / notification-area (Windows) icon, so closing the window
//! keeps ytmini running in the background. Not built on Linux: tray support
//! there needs GTK + appindicator system libraries; closing just quits.

use eframe::egui;
use std::sync::mpsc::Sender;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(any(target_os = "macos", windows)), allow(dead_code))]
pub enum TrayEvent {
    Show,
    PlayPause,
    Quit,
}

#[cfg(any(target_os = "macos", windows))]
mod imp {
    use super::*;
    use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
    use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

    pub struct Tray {
        _icon: TrayIcon,
        play_pause: MenuItem,
        last_label: Option<&'static str>,
    }

    impl Tray {
        /// Must be called on the main thread after the event loop started
        /// (i.e. from `App::new`).
        pub fn new(ctx: &egui::Context, tx: Sender<TrayEvent>) -> Result<Tray, String> {
            let show = MenuItem::new(crate::i18n::t("Show ytmini"), true, None);
            let play_pause = MenuItem::new(crate::i18n::t("Pause"), false, None);
            let quit = MenuItem::new(crate::i18n::t("Quit ytmini"), true, None);
            let menu =
                Menu::with_items(&[&show, &play_pause, &PredefinedMenuItem::separator(), &quit])
                    .map_err(|e| e.to_string())?;
            let (show_id, pp_id, quit_id) = (
                show.id().clone(),
                play_pause.id().clone(),
                quit.id().clone(),
            );
            let ctx = ctx.clone();
            // Called from the OS event thread; wake the app so `logic` runs even
            // while the window is hidden.
            MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
                let ev = if e.id == show_id {
                    TrayEvent::Show
                } else if e.id == pp_id {
                    TrayEvent::PlayPause
                } else if e.id == quit_id {
                    TrayEvent::Quit
                } else {
                    return;
                };
                let _ = tx.send(ev);
                ctx.request_repaint();
            }));
            let (rgba, size) = icon_rgba();
            let icon = Icon::from_rgba(rgba, size, size).map_err(|e| e.to_string())?;
            let builder = TrayIconBuilder::new();
            // macOS: template image, tinted by the system for light/dark menu bars.
            #[cfg(target_os = "macos")]
            let builder = builder.with_icon_templated(icon);
            #[cfg(not(target_os = "macos"))]
            let builder = builder.with_icon(icon);
            let icon = builder
                .with_tooltip("ytmini")
                .with_menu(Box::new(menu))
                .build()
                .map_err(|e| e.to_string())?;
            Ok(Tray {
                _icon: icon,
                play_pause,
                last_label: None,
            })
        }

        /// Reflect player state in the menu (`None` = nothing playing).
        pub fn set_playing(&mut self, paused: Option<bool>) {
            let label = match paused {
                None => crate::i18n::t("Play / pause"),
                Some(true) => crate::i18n::t("Play"),
                Some(false) => crate::i18n::t("Pause"),
            };
            if self.last_label != Some(label) {
                self.play_pause.set_text(label);
                self.play_pause.set_enabled(paused.is_some());
                self.last_label = Some(label);
            }
        }
    }

    /// 22pt menu-bar glyph at 2× (44 px): rounded-rect outline with a play
    /// triangle, black on transparent (template image).
    fn icon_rgba() -> (Vec<u8>, u32) {
        const N: u32 = 44;
        const SS: u32 = 4;
        let (x0, y0, x1, y1, r, stroke) = (4.0f32, 9.0f32, 40.0f32, 35.0f32, 7.0f32, 3.0f32);
        let rr = |x: f32, y: f32, inset: f32| {
            let (ax, ay, bx, by, rad) = (
                x0 + inset,
                y0 + inset,
                x1 - inset,
                y1 - inset,
                (r - inset).max(0.0),
            );
            if x < ax || x > bx || y < ay || y > by {
                return false;
            }
            let qx = (x - x.clamp(ax + rad, bx - rad)).abs();
            let qy = (y - y.clamp(ay + rad, by - rad)).abs();
            qx * qx + qy * qy <= rad * rad
        };
        let tri = [(18.5f32, 15.5f32), (18.5f32, 28.5f32), (29.0f32, 22.0f32)];
        let in_tri = |x: f32, y: f32| {
            let e =
                |a: (f32, f32), b: (f32, f32)| (b.0 - a.0) * (y - a.1) - (b.1 - a.1) * (x - a.0);
            let (d1, d2, d3) = (e(tri[0], tri[1]), e(tri[1], tri[2]), e(tri[2], tri[0]));
            (d1 >= 0.0 && d2 >= 0.0 && d3 >= 0.0) || (d1 <= 0.0 && d2 <= 0.0 && d3 <= 0.0)
        };
        let mut out = Vec::with_capacity((N * N * 4) as usize);
        for py in 0..N {
            for px in 0..N {
                let mut cov = 0u32;
                for sy in 0..SS {
                    for sx in 0..SS {
                        let x = px as f32 + (sx as f32 + 0.5) / SS as f32;
                        let y = py as f32 + (sy as f32 + 0.5) / SS as f32;
                        let ring = rr(x, y, 0.0) && !rr(x, y, stroke);
                        if ring || in_tri(x, y) {
                            cov += 1;
                        }
                    }
                }
                out.extend_from_slice(&[0, 0, 0, (cov * 255 / (SS * SS)) as u8]);
            }
        }
        (out, N)
    }

    /// Hide/show the Dock icon (the app lives in the menu bar while hidden).
    pub fn set_dock_visible(visible: bool) {
        #[cfg(target_os = "macos")]
        {
            use objc2::MainThreadMarker;
            use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
            let Some(mtm) = MainThreadMarker::new() else {
                return;
            };
            let app = NSApplication::sharedApplication(mtm);
            app.setActivationPolicy(if visible {
                NSApplicationActivationPolicy::Regular
            } else {
                NSApplicationActivationPolicy::Accessory
            });
            if visible {
                #[allow(deprecated)] // `activate()` is macOS 14+; this works on 11+
                app.activateIgnoringOtherApps(true);
            }
        }
        #[cfg(not(target_os = "macos"))]
        let _ = visible;
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
mod imp {
    use super::*;

    pub struct Tray;

    impl Tray {
        pub fn new(_ctx: &egui::Context, _tx: Sender<TrayEvent>) -> Result<Tray, String> {
            Err("no tray support on this platform".into())
        }
        pub fn set_playing(&mut self, _paused: Option<bool>) {}
    }

    pub fn set_dock_visible(_visible: bool) {}
}

pub use imp::{set_dock_visible, Tray};
