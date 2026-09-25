use std::{
    ffi::{CStr, CString, c_char, c_void},
    ptr,
    sync::OnceLock,
};

use anyhow::{Result, anyhow};
use cocoa::{
    appkit::{NSPasteboard, NSPasteboardTypeString},
    base::{id, nil},
    foundation::NSString,
};
use futures::{StreamExt as _, channel::mpsc};
use ghostty_embed as ffi;
use gpui::{App, Global};
use objc::{msg_send, sel, sel_impl};
use parking_lot::Mutex;

/// Something the embedded Ghostty runtime told one surface.
///
/// Callbacks can arrive on Ghostty's own threads, so they are forwarded over a
/// channel and handled on the foreground thread by the owning terminal view.
#[derive(Debug)]
pub(crate) enum SurfaceEvent {
    Frame,
    Title(String),
    Pwd(String),
    MouseShape(ffi::ghostty_action_mouse_shape_e),
    CellSize { width: u32, height: u32 },
    Close,
}

/// Heap-pinned per-surface state whose address is Ghostty's surface userdata.
pub(crate) struct SurfaceShared {
    pub surface: Mutex<ffi::ghostty_surface_t>,
    pub events: mpsc::UnboundedSender<SurfaceEvent>,
}

// Ghostty surfaces are safe to reference from its callback threads; all calls
// that mutate a surface are made on the foreground thread.
unsafe impl Send for SurfaceShared {}
unsafe impl Sync for SurfaceShared {}

impl SurfaceShared {
    pub fn send(&self, event: SurfaceEvent) {
        self.events.unbounded_send(event).ok();
    }
}

struct AppHandle(ffi::ghostty_app_t);

// The app handle is only ticked from the foreground thread; the wakeup
// callback merely needs to know that a tick is due.
unsafe impl Send for AppHandle {}
unsafe impl Sync for AppHandle {}

static APP: OnceLock<AppHandle> = OnceLock::new();
static WAKEUP: OnceLock<mpsc::UnboundedSender<()>> = OnceLock::new();

pub struct GhosttyRuntime {
    app: ffi::ghostty_app_t,
}

impl Global for GhosttyRuntime {}

impl GhosttyRuntime {
    pub fn app(&self) -> ffi::ghostty_app_t {
        self.app
    }

    pub fn global(cx: &mut App) -> Result<&GhosttyRuntime> {
        if !cx.has_global::<GhosttyRuntime>() {
            let runtime = Self::new(cx)?;
            cx.set_global(runtime);
        }
        Ok(cx.global::<GhosttyRuntime>())
    }

    fn new(cx: &mut App) -> Result<Self> {
        ensure_resources_dir();
        unsafe {
            let mut argv: [*mut c_char; 1] = [ptr::null_mut()];
            if ffi::ghostty_init(0, argv.as_mut_ptr()) != 0 {
                return Err(anyhow!("ghostty_init failed"));
            }

            let config = ffi::ghostty_config_new();
            ffi::ghostty_config_load_default_files(config);
            ffi::ghostty_config_load_recursive_files(config);
            ffi::ghostty_config_finalize(config);
            for index in 0..ffi::ghostty_config_diagnostics_count(config) {
                let diagnostic = ffi::ghostty_config_get_diagnostic(config, index);
                if !diagnostic.message.is_null() {
                    log::warn!(
                        "ghostty config: {}",
                        CStr::from_ptr(diagnostic.message).to_string_lossy()
                    );
                }
            }

            let (wakeup_tx, mut wakeup_rx) = mpsc::unbounded::<()>();
            WAKEUP
                .set(wakeup_tx)
                .map_err(|_| anyhow!("ghostty runtime initialized twice"))?;

            let runtime_config = ffi::ghostty_runtime_config_s {
                userdata: ptr::null_mut(),
                supports_selection_clipboard: false,
                wakeup_cb: Some(wakeup_cb),
                action_cb: Some(action_cb),
                read_clipboard_cb: Some(read_clipboard_cb),
                confirm_read_clipboard_cb: Some(confirm_read_clipboard_cb),
                write_clipboard_cb: Some(write_clipboard_cb),
                close_surface_cb: Some(close_surface_cb),
            };
            let app = ffi::ghostty_app_new(&runtime_config, config);
            if app.is_null() {
                return Err(anyhow!("ghostty_app_new failed"));
            }
            APP.set(AppHandle(app))
                .map_err(|_| anyhow!("ghostty runtime initialized twice"))?;

            cx.spawn(async move |_| {
                while wakeup_rx.next().await.is_some() {
                    // Coalesce bursts of wakeups into a single tick.
                    while wakeup_rx.try_recv().is_ok() {}
                    if let Some(app) = APP.get() {
                        ffi::ghostty_app_tick(app.0);
                    }
                }
            })
            .detach();

            Ok(Self { app })
        }
    }
}

unsafe fn surface_shared<'a>(userdata: *mut c_void) -> Option<&'a SurfaceShared> {
    unsafe { (userdata as *const SurfaceShared).as_ref() }
}

unsafe fn c_string(value: *const c_char) -> Option<String> {
    if value.is_null() {
        None
    } else {
        Some(
            unsafe { CStr::from_ptr(value) }
                .to_string_lossy()
                .into_owned(),
        )
    }
}

unsafe extern "C" fn wakeup_cb(_userdata: *mut c_void) {
    if let Some(wakeup) = WAKEUP.get() {
        wakeup.unbounded_send(()).ok();
    }
}

unsafe extern "C" fn action_cb(
    _app: ffi::ghostty_app_t,
    target: ffi::ghostty_target_s,
    action: ffi::ghostty_action_s,
) -> bool {
    if target.tag != ffi::GHOSTTY_TARGET_SURFACE {
        return false;
    }
    let shared = unsafe {
        let surface = target.target.surface;
        if surface.is_null() {
            return false;
        }
        surface_shared(ffi::ghostty_surface_userdata(surface))
    };
    let Some(shared) = shared else {
        return false;
    };

    unsafe {
        match action.tag {
            ffi::GHOSTTY_ACTION_SET_TITLE | ffi::GHOSTTY_ACTION_SET_TAB_TITLE => {
                let title = if action.tag == ffi::GHOSTTY_ACTION_SET_TITLE {
                    action.action.set_title.title
                } else {
                    action.action.set_tab_title.title
                };
                if let Some(title) = c_string(title) {
                    shared.send(SurfaceEvent::Title(title));
                }
                true
            }
            ffi::GHOSTTY_ACTION_PWD => {
                if let Some(pwd) = c_string(action.action.pwd.pwd) {
                    shared.send(SurfaceEvent::Pwd(pwd));
                }
                true
            }
            ffi::GHOSTTY_ACTION_MOUSE_SHAPE => {
                shared.send(SurfaceEvent::MouseShape(action.action.mouse_shape));
                true
            }
            ffi::GHOSTTY_ACTION_CELL_SIZE => {
                let cell_size = action.action.cell_size;
                shared.send(SurfaceEvent::CellSize {
                    width: cell_size.width,
                    height: cell_size.height,
                });
                true
            }
            ffi::GHOSTTY_ACTION_RENDER => {
                shared.send(SurfaceEvent::Frame);
                true
            }
            ffi::GHOSTTY_ACTION_CLOSE_TAB | ffi::GHOSTTY_ACTION_CLOSE_WINDOW => {
                shared.send(SurfaceEvent::Close);
                true
            }
            _ => false,
        }
    }
}

unsafe fn pasteboard_string() -> Option<String> {
    unsafe {
        let pasteboard: id = NSPasteboard::generalPasteboard(nil);
        let string: id = msg_send![pasteboard, stringForType: NSPasteboardTypeString];
        if string == nil {
            return None;
        }
        let bytes = NSString::UTF8String(string);
        c_string(bytes)
    }
}

unsafe fn complete_clipboard_request(
    shared: &SurfaceShared,
    text: &str,
    state: *mut c_void,
    confirmed: bool,
) {
    let surface = *shared.surface.lock();
    if surface.is_null() {
        return;
    }
    let Ok(text) = CString::new(text) else {
        return;
    };
    unsafe {
        ffi::ghostty_surface_complete_clipboard_request(surface, text.as_ptr(), state, confirmed)
    };
}

unsafe extern "C" fn read_clipboard_cb(
    userdata: *mut c_void,
    clipboard: ffi::ghostty_clipboard_e,
    state: *mut c_void,
) -> bool {
    if clipboard != ffi::GHOSTTY_CLIPBOARD_STANDARD {
        return false;
    }
    let Some(shared) = (unsafe { surface_shared(userdata) }) else {
        return false;
    };
    let text = unsafe { pasteboard_string() }.unwrap_or_default();
    unsafe { complete_clipboard_request(shared, &text, state, false) };
    true
}

unsafe extern "C" fn confirm_read_clipboard_cb(
    userdata: *mut c_void,
    text: *const c_char,
    state: *mut c_void,
    _request: ffi::ghostty_clipboard_request_e,
) {
    // Ghostty asks for confirmation before pasting text it considers unsafe
    // (e.g. containing newlines). The Ghostty app shows a dialog; until the
    // unified window has one, the paste goes through as the user asked.
    let Some(shared) = (unsafe { surface_shared(userdata) }) else {
        return;
    };
    let text = unsafe { c_string(text) }.unwrap_or_default();
    unsafe { complete_clipboard_request(shared, &text, state, true) };
}

unsafe extern "C" fn write_clipboard_cb(
    _userdata: *mut c_void,
    clipboard: ffi::ghostty_clipboard_e,
    content: *const ffi::ghostty_clipboard_content_s,
    content_len: usize,
    _confirm: bool,
) {
    if clipboard != ffi::GHOSTTY_CLIPBOARD_STANDARD || content.is_null() {
        return;
    }
    let contents = unsafe { std::slice::from_raw_parts(content, content_len) };
    let text = contents.iter().find_map(|content| unsafe {
        let mime = c_string(content.mime)?;
        (mime == "text/plain")
            .then(|| c_string(content.data))
            .flatten()
    });
    let Some(text) = text else {
        return;
    };
    unsafe {
        let pasteboard: id = NSPasteboard::generalPasteboard(nil);
        let _: i64 = msg_send![pasteboard, clearContents];
        let string = crate::ns_string(&text);
        let _: bool = msg_send![pasteboard, setString: string forType: NSPasteboardTypeString];
    }
}

unsafe extern "C" fn close_surface_cb(userdata: *mut c_void, _process_alive: bool) {
    if let Some(shared) = unsafe { surface_shared(userdata) } {
        shared.send(SurfaceEvent::Close);
    }
}

/// Ghostty finds its resources (terminfo, shell integration, themes) next to
/// the executable inside an app bundle, which the bundled Zed provides. An
/// unbundled dev build borrows them from an installed Ghostty instead.
fn ensure_resources_dir() {
    if std::env::var_os("GHOSTTY_RESOURCES_DIR").is_some() {
        return;
    }
    let bundled = std::env::current_exe().ok().and_then(|exe| {
        let resources = exe.parent()?.parent()?.join("Resources");
        resources
            .join("terminfo/78/xterm-ghostty")
            .exists()
            .then(|| resources.join("ghostty"))
    });
    if bundled.is_some() {
        return;
    }
    let fallback = ["/Applications/Ghostty Dev.app", "/Applications/Ghostty.app"]
        .iter()
        .map(|app| std::path::Path::new(app).join("Contents/Resources/ghostty"))
        .find(|dir| dir.exists());
    if let Some(dir) = fallback {
        // SAFETY: called on the main thread before Ghostty starts any threads
        // of its own; nothing else in Zed reads this variable.
        unsafe { std::env::set_var("GHOSTTY_RESOURCES_DIR", dir) };
    } else {
        log::warn!("no Ghostty resources found; shell integration and terminfo are unavailable");
    }
}
