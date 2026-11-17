//! Keep the screen on while a video plays, and the computer awake while
//! anything plays (audio continues when the window is in the tray).
//! mpv would do this itself, but not when embedded through libmpv's render
//! API: it has no window of its own then.

/// What playback needs right now.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Need {
    #[default]
    Nothing,
    /// Audio only / window hidden: don't let the computer sleep.
    System,
    /// A visible video: the display stays on too.
    Display,
}

/// Tell the OS (cheap when nothing changed). Call from the UI thread: on
/// Windows the request belongs to the calling thread.
pub fn set(need: Need) {
    use std::sync::Mutex;
    static LAST: Mutex<Need> = Mutex::new(Need::Nothing);
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    if *last == need {
        return;
    }
    *last = need;
    imp::set(need);
}

#[cfg(windows)]
mod imp {
    use super::Need;

    const ES_CONTINUOUS: u32 = 0x8000_0000;
    const ES_SYSTEM_REQUIRED: u32 = 0x0000_0001;
    const ES_DISPLAY_REQUIRED: u32 = 0x0000_0002;

    #[link(name = "kernel32")]
    extern "system" {
        fn SetThreadExecutionState(flags: u32) -> u32;
    }

    pub fn set(need: Need) {
        let flags = match need {
            Need::Nothing => ES_CONTINUOUS,
            Need::System => ES_CONTINUOUS | ES_SYSTEM_REQUIRED,
            Need::Display => ES_CONTINUOUS | ES_SYSTEM_REQUIRED | ES_DISPLAY_REQUIRED,
        };
        unsafe {
            SetThreadExecutionState(flags);
        }
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use super::Need;
    use std::ffi::{c_char, c_void, CStr};
    use std::sync::Mutex;

    type CFStringRef = *const c_void;
    const UTF8: u32 = 0x0800_0100;
    const ASSERTION_ON: u32 = 255;

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFStringCreateWithCString(
            alloc: *const c_void,
            s: *const c_char,
            encoding: u32,
        ) -> CFStringRef;
        fn CFRelease(cf: *const c_void);
    }

    #[link(name = "IOKit", kind = "framework")]
    extern "C" {
        fn IOPMAssertionCreateWithName(
            kind: CFStringRef,
            level: u32,
            name: CFStringRef,
            id: *mut u32,
        ) -> i32;
        fn IOPMAssertionRelease(id: u32) -> i32;
    }

    /// The assertion we hold, if any.
    static HELD: Mutex<Option<u32>> = Mutex::new(None);

    fn assert(kind: &CStr) -> Option<u32> {
        unsafe {
            let kind = CFStringCreateWithCString(std::ptr::null(), kind.as_ptr(), UTF8);
            let name =
                CFStringCreateWithCString(std::ptr::null(), c"ytmini is playing".as_ptr(), UTF8);
            let mut id = 0;
            let ok = !kind.is_null()
                && !name.is_null()
                && IOPMAssertionCreateWithName(kind, ASSERTION_ON, name, &mut id) == 0;
            if !kind.is_null() {
                CFRelease(kind);
            }
            if !name.is_null() {
                CFRelease(name);
            }
            ok.then_some(id)
        }
    }

    pub fn set(need: Need) {
        let mut held = HELD.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(id) = held.take() {
            unsafe {
                IOPMAssertionRelease(id);
            }
        }
        *held = match need {
            Need::Nothing => None,
            Need::System => assert(c"PreventUserIdleSystemSleep"),
            Need::Display => assert(c"PreventUserIdleDisplaySleep"),
        };
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod imp {
    pub fn set(_: super::Need) {}
}
