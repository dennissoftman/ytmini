//! Minimal libmpv binding (client API 2.x + OpenGL render API).
//!
//! libmpv is loaded at runtime with `dlopen` rather than linked at build time:
//! - the app still starts (and falls back to the separate-window player) when
//!   libmpv is missing or an mpv upgrade breaks the ABI;
//! - no build.rs / linker search paths per OS. We look in the usual places:
//!   Homebrew (`/opt/homebrew/lib`, `/usr/local/lib`), the system library path,
//!   and next to the executable (for a future self-contained `.app`).
//!
//! Only the handful of functions ytmini needs are bound. Struct layouts and
//! constants are copied from mpv's `client.h` / `render.h` / `render_gl.h`.

use std::{
    ffi::{c_char, c_int, c_void, CStr, CString},
    sync::OnceLock,
};

// ---- constants (client.h / render.h) ----
pub const FORMAT_STRING: c_int = 1;
pub const FORMAT_FLAG: c_int = 3;
pub const FORMAT_INT64: c_int = 4;
pub const FORMAT_DOUBLE: c_int = 5;

pub const EVENT_NONE: c_int = 0;
pub const EVENT_SHUTDOWN: c_int = 1;
pub const EVENT_LOG_MESSAGE: c_int = 2;
pub const EVENT_START_FILE: c_int = 6;
pub const EVENT_END_FILE: c_int = 7;
pub const EVENT_PLAYBACK_RESTART: c_int = 21;
pub const EVENT_PROPERTY_CHANGE: c_int = 22;

pub const END_FILE_REASON_EOF: c_int = 0;
pub const END_FILE_REASON_ERROR: c_int = 4;

const RENDER_PARAM_INVALID: c_int = 0;
const RENDER_PARAM_API_TYPE: c_int = 1;
const RENDER_PARAM_OPENGL_INIT_PARAMS: c_int = 2;
const RENDER_PARAM_OPENGL_FBO: c_int = 3;
const RENDER_PARAM_FLIP_Y: c_int = 4;
pub const RENDER_UPDATE_FRAME: u64 = 1;

// ---- C structs ----
#[repr(C)]
pub struct Event {
    pub event_id: c_int,
    pub error: c_int,
    pub reply_userdata: u64,
    pub data: *mut c_void,
}

#[repr(C)]
pub struct EventProperty {
    pub name: *const c_char,
    pub format: c_int,
    pub data: *mut c_void,
}

#[repr(C)]
pub struct EventEndFile {
    pub reason: c_int,
    pub error: c_int,
    pub playlist_entry_id: i64,
    pub playlist_insert_id: i64,
    pub playlist_insert_num_entries: c_int,
}

#[repr(C)]
pub struct EventLogMessage {
    pub prefix: *const c_char,
    pub level: *const c_char,
    pub text: *const c_char,
    pub log_level: c_int,
}

#[repr(C)]
struct RenderParam {
    kind: c_int,
    data: *mut c_void,
}

#[repr(C)]
struct OpenGlInitParams {
    get_proc_address: Option<unsafe extern "C" fn(ctx: *mut c_void, name: *const c_char) -> *mut c_void>,
    get_proc_address_ctx: *mut c_void,
}

#[repr(C)]
struct OpenGlFbo {
    fbo: c_int,
    w: c_int,
    h: c_int,
    internal_format: c_int,
}

#[repr(C)]
pub struct Handle {
    _private: [u8; 0],
}
#[repr(C)]
pub struct RenderCtx {
    _private: [u8; 0],
}

type Callback = unsafe extern "C" fn(*mut c_void);

// ---- function table ----
#[allow(clippy::type_complexity)] // it's an FFI table; aliases wouldn't make it clearer
struct Lib {
    _lib: libloading::Library,
    client_api_version: unsafe extern "C" fn() -> u64,
    error_string: unsafe extern "C" fn(c_int) -> *const c_char,
    create: unsafe extern "C" fn() -> *mut Handle,
    initialize: unsafe extern "C" fn(*mut Handle) -> c_int,
    terminate_destroy: unsafe extern "C" fn(*mut Handle),
    set_option_string: unsafe extern "C" fn(*mut Handle, *const c_char, *const c_char) -> c_int,
    set_property: unsafe extern "C" fn(*mut Handle, *const c_char, c_int, *mut c_void) -> c_int,
    get_property: unsafe extern "C" fn(*mut Handle, *const c_char, c_int, *mut c_void) -> c_int,
    command: unsafe extern "C" fn(*mut Handle, *mut *const c_char) -> c_int,
    observe_property: unsafe extern "C" fn(*mut Handle, u64, *const c_char, c_int) -> c_int,
    request_log_messages: unsafe extern "C" fn(*mut Handle, *const c_char) -> c_int,
    wait_event: unsafe extern "C" fn(*mut Handle, f64) -> *mut Event,
    set_wakeup_callback: unsafe extern "C" fn(*mut Handle, Option<Callback>, *mut c_void),
    render_context_create: unsafe extern "C" fn(*mut *mut RenderCtx, *mut Handle, *mut RenderParam) -> c_int,
    render_context_set_update_callback: unsafe extern "C" fn(*mut RenderCtx, Option<Callback>, *mut c_void),
    render_context_update: unsafe extern "C" fn(*mut RenderCtx) -> u64,
    render_context_render: unsafe extern "C" fn(*mut RenderCtx, *mut RenderParam) -> c_int,
    render_context_free: unsafe extern "C" fn(*mut RenderCtx),
}

fn candidates() -> Vec<std::path::PathBuf> {
    let mut v = Vec::new();
    if let Some(dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf())) {
        // .app/Contents/MacOS/../Frameworks, or next to the exe
        v.push(dir.join("../Frameworks/libmpv.2.dylib"));
        v.push(dir.join("libmpv.2.dylib"));
        v.push(dir.join("libmpv-2.dll"));
    }
    let names: &[&str] = if cfg!(target_os = "macos") {
        &["/opt/homebrew/lib/libmpv.2.dylib", "/usr/local/lib/libmpv.2.dylib", "/opt/local/lib/libmpv.2.dylib", "libmpv.2.dylib"]
    } else if cfg!(windows) {
        &["libmpv-2.dll", "mpv-2.dll", "mpv-1.dll"]
    } else {
        &["libmpv.so.2", "libmpv.so"]
    };
    v.extend(names.iter().map(std::path::PathBuf::from));
    v
}

fn load() -> Result<Lib, String> {
    let mut errors = Vec::new();
    for path in candidates() {
        if path.is_absolute() && !path.exists() {
            continue;
        }
        // SAFETY: loading libmpv runs its (trusted) initializers only.
        match unsafe { libloading::Library::new(&path) } {
            Ok(lib) => return bind(lib).map_err(|e| format!("{}: {e}", path.display())),
            Err(e) => errors.push(format!("{}: {e}", path.display())),
        }
    }
    Err(format!("libmpv not found ({})", errors.last().cloned().unwrap_or_else(|| "no candidates".into())))
}

fn bind(lib: libloading::Library) -> Result<Lib, String> {
    macro_rules! sym {
        ($name:literal) => {
            // SAFETY: the signature matches mpv's client.h/render.h for this symbol.
            *unsafe { lib.get(concat!($name, "\0").as_bytes()) }.map_err(|e| format!("{}: {e}", $name))?
        };
    }
    let l = Lib {
        client_api_version: sym!("mpv_client_api_version"),
        error_string: sym!("mpv_error_string"),
        create: sym!("mpv_create"),
        initialize: sym!("mpv_initialize"),
        terminate_destroy: sym!("mpv_terminate_destroy"),
        set_option_string: sym!("mpv_set_option_string"),
        set_property: sym!("mpv_set_property"),
        get_property: sym!("mpv_get_property"),
        command: sym!("mpv_command"),
        observe_property: sym!("mpv_observe_property"),
        request_log_messages: sym!("mpv_request_log_messages"),
        wait_event: sym!("mpv_wait_event"),
        set_wakeup_callback: sym!("mpv_set_wakeup_callback"),
        render_context_create: sym!("mpv_render_context_create"),
        render_context_set_update_callback: sym!("mpv_render_context_set_update_callback"),
        render_context_update: sym!("mpv_render_context_update"),
        render_context_render: sym!("mpv_render_context_render"),
        render_context_free: sym!("mpv_render_context_free"),
        _lib: lib,
    };
    // SAFETY: plain getter.
    let ver = unsafe { (l.client_api_version)() };
    if ver >> 16 != 2 {
        return Err(format!("unsupported libmpv client API {}.{} (need 2.x)", ver >> 16, ver & 0xffff));
    }
    Ok(l)
}

fn lib() -> Result<&'static Lib, String> {
    static LIB: OnceLock<Result<Lib, String>> = OnceLock::new();
    LIB.get_or_init(load).as_ref().map_err(Clone::clone)
}

/// Is an embeddable libmpv available? (Loads it on first call.)
pub fn available() -> Result<(), String> {
    lib().map(|_| ())
}

fn cstr(s: &str) -> CString {
    CString::new(s.replace('\0', "")).unwrap_or_default()
}

fn check(code: c_int, what: &str) -> Result<(), String> {
    if code >= 0 {
        return Ok(());
    }
    let msg = lib()
        // SAFETY: mpv_error_string returns a static string.
        .map(|l| unsafe { CStr::from_ptr((l.error_string)(code)) }.to_string_lossy().into_owned())
        .unwrap_or_default();
    Err(format!("{what}: {msg}"))
}

// ---------------------------------------------------------------------------
// Safe-ish wrapper
// ---------------------------------------------------------------------------

/// An mpv core. Not `Sync`; used from the UI thread, except that
/// [`Mpv::destroy_in_background`] may hand it to another thread for shutdown.
pub struct Mpv {
    h: *mut Handle,
}

// SAFETY: libmpv's client API is thread-safe; we only move the handle between
// threads for shutdown, never use it concurrently.
unsafe impl Send for Mpv {}

impl Mpv {
    /// Create a core, set `options` (before init), initialize.
    pub fn new(options: &[(&str, String)]) -> Result<Mpv, String> {
        let l = lib()?;
        // SAFETY: mpv_create has no preconditions.
        let h = unsafe { (l.create)() };
        if h.is_null() {
            return Err("mpv_create failed".into());
        }
        let mpv = Mpv { h };
        for (k, v) in options {
            let (k, v) = (cstr(k), cstr(v));
            // SAFETY: valid handle and C strings.
            check(unsafe { (l.set_option_string)(h, k.as_ptr(), v.as_ptr()) }, &format!("option {k:?}"))?;
        }
        // SAFETY: valid, not yet initialized handle.
        check(unsafe { (l.initialize)(h) }, "mpv_initialize")?;
        Ok(mpv)
    }

    fn l(&self) -> &'static Lib {
        lib().expect("libmpv loaded (Mpv exists)")
    }

    pub fn command(&self, args: &[&str]) -> Result<(), String> {
        let owned: Vec<CString> = args.iter().map(|a| cstr(a)).collect();
        let mut ptrs: Vec<*const c_char> = owned.iter().map(|c| c.as_ptr()).collect();
        ptrs.push(std::ptr::null());
        // SAFETY: NULL-terminated array of valid C strings that outlive the call.
        check(unsafe { (self.l().command)(self.h, ptrs.as_mut_ptr()) }, args.first().copied().unwrap_or("command"))
    }

    pub fn set_str(&self, name: &str, value: &str) -> Result<(), String> {
        let (n, v) = (cstr(name), cstr(value));
        let mut p = v.as_ptr();
        // SAFETY: MPV_FORMAT_STRING takes a pointer to a char*.
        check(unsafe { (self.l().set_property)(self.h, n.as_ptr(), FORMAT_STRING, (&raw mut p).cast()) }, name)
    }

    pub fn set_f64(&self, name: &str, mut value: f64) -> Result<(), String> {
        let n = cstr(name);
        // SAFETY: MPV_FORMAT_DOUBLE takes a pointer to a double.
        check(unsafe { (self.l().set_property)(self.h, n.as_ptr(), FORMAT_DOUBLE, (&raw mut value).cast()) }, name)
    }

    pub fn set_flag(&self, name: &str, value: bool) -> Result<(), String> {
        let n = cstr(name);
        let mut v: c_int = value.into();
        // SAFETY: MPV_FORMAT_FLAG takes a pointer to an int.
        check(unsafe { (self.l().set_property)(self.h, n.as_ptr(), FORMAT_FLAG, (&raw mut v).cast()) }, name)
    }

    pub fn get_f64(&self, name: &str) -> Option<f64> {
        let n = cstr(name);
        let mut v = 0.0f64;
        // SAFETY: MPV_FORMAT_DOUBLE writes a double.
        let r = unsafe { (self.l().get_property)(self.h, n.as_ptr(), FORMAT_DOUBLE, (&raw mut v).cast()) };
        (r >= 0).then_some(v)
    }

    pub fn observe(&self, id: u64, name: &str, format: c_int) {
        let n = cstr(name);
        // SAFETY: valid handle and name.
        unsafe { (self.l().observe_property)(self.h, id, n.as_ptr(), format) };
    }

    pub fn request_log_messages(&self, level: &str) {
        let lv = cstr(level);
        // SAFETY: valid handle and level string.
        unsafe { (self.l().request_log_messages)(self.h, lv.as_ptr()) };
    }

    /// Next pending event, or None. The returned pointer is valid until the next call.
    pub fn poll_event(&self) -> Option<&Event> {
        // SAFETY: timeout 0 = non-blocking; mpv owns the returned event.
        let e = unsafe { &*(self.l().wait_event)(self.h, 0.0) };
        (e.event_id != EVENT_NONE).then_some(e)
    }

    /// `cb(data)` is called from mpv's threads whenever events are pending.
    ///
    /// # Safety
    /// `data` must stay valid until the core is destroyed.
    pub unsafe fn set_wakeup_callback(&self, cb: Callback, data: *mut c_void) {
        (self.l().set_wakeup_callback)(self.h, Some(cb), data);
    }

    /// Shut the core down on a background thread (it can block briefly while
    /// mpv stops yt-dlp/network). `keep_alive` is dropped only afterwards, so
    /// callback data stays valid while mpv may still call it.
    pub fn destroy_in_background<T: Send + 'static>(self, keep_alive: T) {
        std::thread::spawn(move || {
            drop(self);
            drop(keep_alive);
        });
    }
}

impl Drop for Mpv {
    fn drop(&mut self) {
        // SAFETY: handle is valid and dropped exactly once; any render context
        // must already be freed (Render holds a borrow-free raw ptr, freed first).
        unsafe { (self.l().terminate_destroy)(self.h) };
    }
}

pub fn prop_name(p: &EventProperty) -> &str {
    // SAFETY: mpv gives a valid NUL-terminated name.
    unsafe { CStr::from_ptr(p.name) }.to_str().unwrap_or("")
}

pub fn prop_f64(p: &EventProperty) -> Option<f64> {
    // SAFETY: format says what data points to.
    match p.format {
        FORMAT_DOUBLE if !p.data.is_null() => Some(unsafe { *(p.data as *const f64) }),
        FORMAT_INT64 if !p.data.is_null() => Some(unsafe { *(p.data as *const i64) } as f64),
        _ => None,
    }
}

pub fn prop_string(p: &EventProperty) -> Option<String> {
    // SAFETY: MPV_FORMAT_STRING data is a char**.
    (p.format == FORMAT_STRING && !p.data.is_null()).then(|| c_to_string(unsafe { *(p.data as *const *const c_char) }))
}

pub fn prop_flag(p: &EventProperty) -> Option<bool> {
    // SAFETY: MPV_FORMAT_FLAG data is an int.
    (p.format == FORMAT_FLAG && !p.data.is_null()).then(|| unsafe { *(p.data as *const c_int) } != 0)
}

pub fn c_to_string(p: *const c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    // SAFETY: mpv gives valid NUL-terminated strings.
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

// ---------------------------------------------------------------------------
// OpenGL render context
// ---------------------------------------------------------------------------

pub type GetProcAddress = std::sync::Arc<dyn Fn(&CStr) -> *const c_void + Send + Sync>;

/// mpv's OpenGL renderer. Must be created, used and freed on the thread with
/// the GL context current, and freed before its [`Mpv`] is destroyed.
pub struct Render {
    ctx: *mut RenderCtx,
    // Boxed so the pointer handed to mpv stays put.
    _gpa: Box<GetProcAddress>,
}

unsafe extern "C" fn gpa_trampoline(ctx: *mut c_void, name: *const c_char) -> *mut c_void {
    // SAFETY: ctx is the Box<GetProcAddress> we passed in; name is a C string.
    let f = unsafe { &*(ctx as *const GetProcAddress) };
    f(unsafe { CStr::from_ptr(name) }) as *mut c_void
}

impl Render {
    pub fn new(mpv: &Mpv, gpa: GetProcAddress) -> Result<Render, String> {
        let l = lib()?;
        let mut gpa = Box::new(gpa);
        let mut init = OpenGlInitParams {
            get_proc_address: Some(gpa_trampoline),
            get_proc_address_ctx: (&raw mut *gpa).cast(),
        };
        let api = c"opengl";
        let mut params = [
            RenderParam { kind: RENDER_PARAM_API_TYPE, data: api.as_ptr() as *mut c_void },
            RenderParam { kind: RENDER_PARAM_OPENGL_INIT_PARAMS, data: (&raw mut init).cast() },
            RenderParam { kind: RENDER_PARAM_INVALID, data: std::ptr::null_mut() },
        ];
        let mut ctx = std::ptr::null_mut();
        // SAFETY: params are valid for the call; GL context is current (caller contract).
        check(unsafe { (l.render_context_create)(&mut ctx, mpv.h, params.as_mut_ptr()) }, "mpv_render_context_create")?;
        Ok(Render { ctx, _gpa: gpa })
    }

    /// `cb(data)` is called from any thread when a new frame is ready.
    ///
    /// # Safety
    /// `data` must stay valid until this Render is dropped.
    pub unsafe fn set_update_callback(&self, cb: Callback, data: *mut c_void) {
        (lib().expect("loaded").render_context_set_update_callback)(self.ctx, Some(cb), data);
    }

    /// Ask mpv what needs doing; returns `RENDER_UPDATE_*` flags.
    pub fn update(&self) -> u64 {
        // SAFETY: valid render context.
        unsafe { (lib().expect("loaded").render_context_update)(self.ctx) }
    }

    /// Draw the current video frame into `fbo` (size `w`×`h` pixels).
    /// No Y flip: egui's texture coordinates already match mpv's FBO output.
    pub fn render(&self, fbo: u32, w: i32, h: i32) -> Result<(), String> {
        let mut fbo = OpenGlFbo { fbo: fbo as c_int, w, h, internal_format: 0 };
        let mut flip: c_int = 0;
        let mut params = [
            RenderParam { kind: RENDER_PARAM_OPENGL_FBO, data: (&raw mut fbo).cast() },
            RenderParam { kind: RENDER_PARAM_FLIP_Y, data: (&raw mut flip).cast() },
            RenderParam { kind: RENDER_PARAM_INVALID, data: std::ptr::null_mut() },
        ];
        // SAFETY: valid ctx and params; GL context current (caller contract).
        check(unsafe { (lib().expect("loaded").render_context_render)(self.ctx, params.as_mut_ptr()) }, "render")
    }
}

impl Drop for Render {
    fn drop(&mut self) {
        // SAFETY: valid ctx, freed once, GL context current (caller contract).
        unsafe { (lib().expect("loaded").render_context_free)(self.ctx) };
    }
}
