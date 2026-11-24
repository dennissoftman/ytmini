//! One ytmini at a time on Windows: launching it again (Start menu, desktop
//! shortcut) while it runs, maybe hidden in the tray, brings that window back
//! instead of starting a second copy with a second tray icon. macOS does this
//! itself; on Linux closing the window quits, so there's nothing to come back to.
//!
//! A named event per config folder: the first instance creates it and waits on
//! it; a later one finds it, signals it and exits. Test runs and other config
//! folders (`YTMINI_CONFIG_DIR`) get their own, so they never hand over to the
//! app you're using.

/// `true`: carry on starting. `false`: another ytmini was asked to show its
/// window; exit.
pub fn claim() -> bool {
    let test_run = std::env::args().any(|a| a == "--smoke-test")
        || std::env::var_os("YTMINI_PLAY_SEQ").is_some()
        || std::env::var_os("YTMINI_NO_HISTORY").is_some();
    if test_run {
        return true;
    }
    imp::claim()
}

/// Call `show` (on another thread) whenever a later launch asks for the window.
pub fn on_show(show: impl Fn() + Send + 'static) {
    imp::on_show(show);
}

#[cfg(windows)]
mod imp {
    use sha2::{Digest, Sha256};
    use std::sync::OnceLock;
    use windows_sys::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Threading::{
        CreateEventW, SetEvent, WaitForSingleObject, INFINITE,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{AllowSetForegroundWindow, ASFW_ANY};

    /// Our event (a HANDLE, kept as an integer so it can cross threads).
    static EVENT: OnceLock<usize> = OnceLock::new();

    fn name() -> Vec<u16> {
        let dir = crate::yt::config_dir().unwrap_or_default();
        let key = dir.to_string_lossy().to_lowercase();
        let h = Sha256::digest(key.as_bytes());
        // "Local\": this Windows session only
        format!("Local\\ytmini-{}", hex(&h[..8]))
            .encode_utf16()
            .chain([0])
            .collect()
    }

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    pub fn claim() -> bool {
        let name = name();
        // auto-reset, not signalled
        let h = unsafe { CreateEventW(std::ptr::null(), 0, 0, name.as_ptr()) };
        if h.is_null() {
            return true; // can't tell: better two copies than none
        }
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            // We were just launched, so we may hand the foreground on.
            unsafe {
                AllowSetForegroundWindow(ASFW_ANY);
                SetEvent(h);
            }
            return false;
        }
        let _ = EVENT.set(h as usize);
        true
    }

    pub fn on_show(show: impl Fn() + Send + 'static) {
        let Some(&h) = EVENT.get() else {
            return;
        };
        let _ = std::thread::Builder::new()
            .name("single-instance".into())
            .spawn(move || loop {
                if unsafe { WaitForSingleObject(h as _, INFINITE) } != WAIT_OBJECT_0 {
                    return;
                }
                show();
            });
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn claim() -> bool {
        true
    }

    pub fn on_show(_show: impl Fn() + Send + 'static) {}
}
