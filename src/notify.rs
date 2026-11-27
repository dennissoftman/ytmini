//! System notifications (only used to announce an update).
//!
//! - macOS: the UserNotifications framework. The first one asks for
//!   permission; clicking it shows ytmini (`on_click`). Only from the app
//!   bundle: the framework throws without a bundle identifier.
//! - Windows: a balloon on ytmini's notification-area icon, which Windows
//!   10/11 shows as a toast. Needs the tray icon.
//! - Linux: none (the in-app banner covers it).

#[cfg(target_os = "macos")]
mod imp {
    use block2::RcBlock;
    use objc2::rc::Retained;
    use objc2::runtime::{Bool, NSObject, ProtocolObject};
    use objc2::{define_class, msg_send, AnyThread};
    use objc2_foundation::{NSError, NSObjectProtocol, NSString};
    use objc2_user_notifications::*;
    use std::sync::OnceLock;

    type Callback = Box<dyn Fn() + Send + Sync>;
    static ON_CLICK: OnceLock<Callback> = OnceLock::new();

    define_class!(
        // SAFETY: NSObject has no subclassing requirements; no Drop impl.
        #[unsafe(super(NSObject))]
        #[name = "YtminiNotifyDelegate"]
        struct Delegate;

        unsafe impl NSObjectProtocol for Delegate {}

        unsafe impl UNUserNotificationCenterDelegate for Delegate {
            #[unsafe(method(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:))]
            fn did_receive(
                &self,
                _center: &UNUserNotificationCenter,
                _response: &UNNotificationResponse,
                done: &block2::DynBlock<dyn Fn()>,
            ) {
                if let Some(f) = ON_CLICK.get() {
                    f();
                }
                done.call(());
            }
        }
    );

    impl Delegate {
        fn new() -> Retained<Self> {
            let this = Self::alloc().set_ivars(());
            // SAFETY: plain NSObject init.
            unsafe { msg_send![super(this), init] }
        }
    }

    fn bundled() -> bool {
        std::env::current_exe()
            .map(|e| e.to_string_lossy().contains(".app/Contents/MacOS/"))
            .unwrap_or(false)
    }

    pub fn init(on_click: impl Fn() + Send + Sync + 'static) {
        if !bundled() || ON_CLICK.set(Box::new(on_click)).is_err() {
            return;
        }
        let center = UNUserNotificationCenter::currentNotificationCenter();
        let delegate = Delegate::new();
        center.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
        std::mem::forget(delegate); // the center holds it weakly; it lives for the whole run
    }

    pub fn show(title: &str, body: &str) {
        if !bundled() {
            return;
        }
        let (title, body) = (title.to_string(), body.to_string());
        let center = UNUserNotificationCenter::currentNotificationCenter();
        let opts = UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound;
        let handler = RcBlock::new(move |granted: Bool, _e: *mut NSError| {
            if !granted.as_bool() {
                return;
            }
            let content = UNMutableNotificationContent::new();
            content.setTitle(&NSString::from_str(&title));
            content.setBody(&NSString::from_str(&body));
            let req = UNNotificationRequest::requestWithIdentifier_content_trigger(
                &NSString::from_str("ytmini-update"),
                &content,
                None,
            );
            UNUserNotificationCenter::currentNotificationCenter()
                .addNotificationRequest_withCompletionHandler(&req, None);
        });
        center.requestAuthorizationWithOptions_completionHandler(opts, &handler);
    }
}

#[cfg(windows)]
mod imp {
    pub fn init(_on_click: impl Fn() + Send + Sync + 'static) {}

    /// A balloon on the tray icon (tray-icon numbers its icons from 1 and
    /// we have one; a few ids are tried in case it was ever recreated).
    pub fn show_on(hwnd: windows_sys::Win32::Foundation::HWND, title: &str, body: &str) {
        use windows_sys::Win32::UI::Shell::{
            Shell_NotifyIconW, NIF_INFO, NIIF_INFO, NIM_MODIFY, NOTIFYICONDATAW,
        };
        fn fill(dst: &mut [u16], s: &str) {
            let n = dst.len() - 1;
            for (d, c) in dst.iter_mut().zip(s.encode_utf16().take(n)) {
                *d = c;
            }
        }
        for id in 1..=8 {
            // SAFETY: all-zero is a valid NOTIFYICONDATAW.
            let mut nid: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
            nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
            nid.hWnd = hwnd;
            nid.uID = id;
            nid.uFlags = NIF_INFO;
            nid.dwInfoFlags = NIIF_INFO;
            fill(&mut nid.szInfoTitle, title);
            fill(&mut nid.szInfo, body);
            // SAFETY: nid is fully initialised and outlives the call.
            if unsafe { Shell_NotifyIconW(NIM_MODIFY, &nid) } != 0 {
                return;
            }
        }
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
mod imp {
    pub fn init(_on_click: impl Fn() + Send + Sync + 'static) {}
}

/// macOS without a menu-bar icon; elsewhere notifications go through the tray.
pub fn show_without_tray(_title: &str, _body: &str) {
    #[cfg(target_os = "macos")]
    imp::show(_title, _body);
}

pub use imp::*;
