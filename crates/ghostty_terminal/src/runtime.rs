use std::{
    ffi::{CStr, CString, c_char, c_void},
    ptr,
    sync::{Arc, OnceLock},
};

use anyhow::{Result, anyhow};
use cocoa::{
    appkit::{NSPasteboard, NSPasteboardTypeString},
    base::{id, nil},
    foundation::NSString,
};
use futures::{StreamExt as _, channel::mpsc};
use ghostty_embed as ffi;
use gpui::{App, Global, TaskExt as _};
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
    CellSize {
        width: u32,
        height: u32,
    },
    /// The surface asked to be closed: its process exited or the user closed
    /// it. `process_alive` means closing kills a running process.
    Close {
        process_alive: bool,
    },
    NewSplit(ffi::ghostty_action_split_direction_e),
    NewTab,
    CloseTab(ffi::ghostty_action_close_tab_mode_e),
    /// A 1-based tab index, or one of the `GHOSTTY_GOTO_TAB_*` values.
    GotoTab(i32),
    GotoSplit(ffi::ghostty_action_goto_split_e),
    ResizeSplit {
        direction: ffi::ghostty_action_resize_split_direction_e,
        amount: u16,
    },
    EqualizeSplits,
    ToggleSplitZoom,
    /// Ghostty wants the user to confirm a paste it considers unsafe, or an
    /// application reading the clipboard (OSC 52). `state` is Ghostty's handle
    /// for the request, completed with `complete_clipboard`.
    ConfirmClipboardRead {
        text: String,
        state: usize,
        request: ffi::ghostty_clipboard_request_e,
    },
    /// An application wants to write the clipboard (OSC 52) and the config
    /// asks for confirmation.
    ConfirmClipboardWrite {
        text: String,
    },
    ReloadConfig {
        soft: bool,
    },
}

/// Something the embedded Ghostty runtime told the app as a whole.
enum AppEvent {
    ConfigChanged,
    ReloadConfig { soft: bool },
    OpenConfig,
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

struct ConfigHandle(ffi::ghostty_config_t);

// A finalized config is never mutated; Ghostty only reads it, from any thread.
unsafe impl Send for ConfigHandle {}
unsafe impl Sync for ConfigHandle {}

impl Drop for ConfigHandle {
    fn drop(&mut self) {
        unsafe { ffi::ghostty_config_free(self.0) };
    }
}

static APP: OnceLock<AppHandle> = OnceLock::new();
static WAKEUP: OnceLock<mpsc::UnboundedSender<()>> = OnceLock::new();
static APP_EVENTS: OnceLock<mpsc::UnboundedSender<AppEvent>> = OnceLock::new();
/// The app's current config, kept to read values such as the terminal colors.
/// Shared so a config handed to Ghostty stays alive while Ghostty reads it,
/// even if a `CONFIG_CHANGE` replaces it meanwhile.
static CONFIG: Mutex<Option<Arc<ConfigHandle>>> = Mutex::new(None);

fn current_config() -> Option<Arc<ConfigHandle>> {
    CONFIG.lock().clone()
}

fn replace_config(config: ffi::ghostty_config_t) {
    let previous = CONFIG.lock().replace(Arc::new(ConfigHandle(config)));
    drop(previous);
}

/// Ghostty asks for a reload after a config edit (hard) and when the
/// light/dark appearance flips (soft: re-apply the current config so the
/// conditional theme resolves again). Like the Ghostty app, the applied config
/// comes back through `CONFIG_CHANGE`.
fn reload_app_config(soft: bool) {
    let Some(app) = APP.get() else {
        return;
    };
    if soft {
        if let Some(config) = current_config() {
            unsafe { ffi::ghostty_app_update_config(app.0, config.0) };
        }
    } else {
        let config = ConfigHandle(load_config());
        unsafe { ffi::ghostty_app_update_config(app.0, config.0) };
    }
}

/// Reloads the config files, like Ghostty's `reload_config` binding.
pub fn reload_config() {
    reload_app_config(false);
}

/// The Ghostty config file, created if it does not exist yet.
pub fn config_file_path() -> Option<std::path::PathBuf> {
    let path = unsafe { ffi::ghostty_config_open_path() };
    if path.ptr.is_null() {
        return None;
    }
    let bytes = unsafe { std::slice::from_raw_parts(path.ptr as *const u8, path.len) };
    let result = std::path::PathBuf::from(String::from_utf8_lossy(bytes).into_owned());
    unsafe { ffi::ghostty_string_free(path) };
    Some(result)
}

/// Opens the Ghostty config in the editor beside the terminal (the Ghostty
/// app opened it in the default text editor).
pub fn open_config(cx: &mut App) {
    let Some(path) = config_file_path() else {
        log::warn!("Ghostty has no config file to open");
        return;
    };
    let Some(app_state) = workspace::AppState::try_global(cx) else {
        return;
    };
    workspace::open_paths(&[path], app_state, workspace::OpenOptions::default(), cx)
        .detach_and_log_err(cx);
}

pub(crate) fn reload_surface_config(surface: ffi::ghostty_surface_t, soft: bool) {
    if soft {
        if let Some(config) = current_config() {
            unsafe { ffi::ghostty_surface_update_config(surface, config.0) };
        }
    } else {
        let config = ConfigHandle(load_config());
        unsafe { ffi::ghostty_surface_update_config(surface, config.0) };
    }
}

/// The terminal's configured colors, which the tab bar derives its palette from.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TerminalColors {
    pub background: gpui::Rgba,
    pub foreground: gpui::Rgba,
}

fn load_config() -> ffi::ghostty_config_t {
    unsafe {
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
        config
    }
}

unsafe fn read_config_color(config: ffi::ghostty_config_t, key: &str) -> Option<gpui::Rgba> {
    let mut color = ffi::ghostty_config_color_s { r: 0, g: 0, b: 0 };
    let found = unsafe {
        ffi::ghostty_config_get(
            config,
            &mut color as *mut _ as *mut c_void,
            key.as_ptr() as *const c_char,
            key.len(),
        )
    };
    found.then(|| gpui::Rgba {
        r: color.r as f32 / 255.,
        g: color.g as f32 / 255.,
        b: color.b as f32 / 255.,
        a: 1.,
    })
}

/// The configured terminal colors, following the light/dark theme in effect.
pub fn terminal_colors() -> TerminalColors {
    let config = CONFIG.lock();
    let read = |key, default: u32| {
        config
            .as_ref()
            .and_then(|config| unsafe { read_config_color(config.0, key) })
            .unwrap_or_else(|| gpui::rgb(default))
    };
    TerminalColors {
        background: read("background", 0x282c33),
        foreground: read("foreground", 0xdce0e5),
    }
}

/// A color config value, e.g. `split-divider-color`, if it is set.
pub fn config_color(key: &str) -> Option<gpui::Rgba> {
    let config = CONFIG.lock();
    let config = config.as_ref()?;
    unsafe { read_config_color(config.0, key) }
}

/// A floating-point config value, e.g. `unfocused-split-opacity`.
pub fn config_f64(key: &str) -> Option<f64> {
    let config = CONFIG.lock();
    let config = config.as_ref()?;
    let mut value: f64 = 0.;
    let found = unsafe {
        ffi::ghostty_config_get(
            config.0,
            &mut value as *mut f64 as *mut c_void,
            key.as_ptr() as *const c_char,
            key.len(),
        )
    };
    found.then_some(value)
}

/// Follows the system light/dark appearance, which picks the `theme =
/// light:...,dark:...` variant.
pub fn set_color_scheme(dark: bool) {
    let Some(app) = APP.get() else {
        return;
    };
    let scheme = if dark {
        ffi::GHOSTTY_COLOR_SCHEME_DARK
    } else {
        ffi::GHOSTTY_COLOR_SCHEME_LIGHT
    };
    unsafe { ffi::ghostty_app_set_color_scheme(app.0, scheme) };
}

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

            let config = load_config();

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
            replace_config(config);

            let (app_events_tx, mut app_events_rx) = mpsc::unbounded::<AppEvent>();
            APP_EVENTS.set(app_events_tx).ok();
            cx.spawn(async move |cx| {
                while let Some(event) = app_events_rx.next().await {
                    match event {
                        AppEvent::ConfigChanged => {}
                        AppEvent::ReloadConfig { soft } => reload_app_config(soft),
                        AppEvent::OpenConfig => {
                            cx.update(|cx| open_config(cx));
                            continue;
                        }
                    }
                    cx.update(|cx| cx.refresh_windows());
                }
            })
            .detach();

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
    if action.tag == ffi::GHOSTTY_ACTION_CONFIG_CHANGE {
        // The applied app config after a reload or an appearance flip. Surface
        // targets carry that surface's own config; the tab bar reads the app's,
        // like the Ghostty app. The payload is only valid during the callback.
        if target.tag == ffi::GHOSTTY_TARGET_APP {
            replace_config(unsafe {
                ffi::ghostty_config_clone(action.action.config_change.config)
            });
            if let Some(events) = APP_EVENTS.get() {
                events.unbounded_send(AppEvent::ConfigChanged).ok();
            }
        }
        return true;
    }
    if action.tag == ffi::GHOSTTY_ACTION_OPEN_CONFIG {
        if let Some(events) = APP_EVENTS.get() {
            events.unbounded_send(AppEvent::OpenConfig).ok();
        }
        return true;
    }
    if action.tag == ffi::GHOSTTY_ACTION_RELOAD_CONFIG {
        let soft = unsafe { action.action.reload_config.soft };
        if target.tag == ffi::GHOSTTY_TARGET_APP {
            if let Some(events) = APP_EVENTS.get() {
                events.unbounded_send(AppEvent::ReloadConfig { soft }).ok();
            }
        } else if let Some(shared) = unsafe {
            let surface = target.target.surface;
            (!surface.is_null())
                .then(|| surface_shared(ffi::ghostty_surface_userdata(surface)))
                .flatten()
        } {
            shared.send(SurfaceEvent::ReloadConfig { soft });
        }
        return true;
    }
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
            ffi::GHOSTTY_ACTION_CLOSE_TAB => {
                shared.send(SurfaceEvent::CloseTab(action.action.close_tab_mode));
                true
            }
            ffi::GHOSTTY_ACTION_CLOSE_WINDOW => {
                shared.send(SurfaceEvent::Close {
                    process_alive: false,
                });
                true
            }
            ffi::GHOSTTY_ACTION_NEW_TAB | ffi::GHOSTTY_ACTION_NEW_WINDOW => {
                shared.send(SurfaceEvent::NewTab);
                true
            }
            ffi::GHOSTTY_ACTION_NEW_SPLIT => {
                shared.send(SurfaceEvent::NewSplit(action.action.new_split));
                true
            }
            ffi::GHOSTTY_ACTION_GOTO_TAB => {
                shared.send(SurfaceEvent::GotoTab(action.action.goto_tab));
                true
            }
            ffi::GHOSTTY_ACTION_GOTO_SPLIT => {
                shared.send(SurfaceEvent::GotoSplit(action.action.goto_split));
                true
            }
            ffi::GHOSTTY_ACTION_RESIZE_SPLIT => {
                let resize = action.action.resize_split;
                shared.send(SurfaceEvent::ResizeSplit {
                    direction: resize.direction,
                    amount: resize.amount,
                });
                true
            }
            ffi::GHOSTTY_ACTION_EQUALIZE_SPLITS => {
                shared.send(SurfaceEvent::EqualizeSplits);
                true
            }
            ffi::GHOSTTY_ACTION_TOGGLE_SPLIT_ZOOM => {
                shared.send(SurfaceEvent::ToggleSplitZoom);
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
    request: ffi::ghostty_clipboard_request_e,
) {
    let Some(shared) = (unsafe { surface_shared(userdata) }) else {
        return;
    };
    let text = unsafe { c_string(text) }.unwrap_or_default();
    shared.send(SurfaceEvent::ConfirmClipboardRead {
        text,
        state: state as usize,
        request,
    });
}

/// Answers a clipboard request Ghostty asked the user to confirm: `text` is
/// what gets pasted (empty when declined).
pub(crate) fn complete_clipboard(
    surface: ffi::ghostty_surface_t,
    text: &str,
    state: usize,
    confirmed: bool,
) {
    let Ok(text) = CString::new(text) else {
        return;
    };
    unsafe {
        ffi::ghostty_surface_complete_clipboard_request(
            surface,
            text.as_ptr(),
            state as *mut c_void,
            confirmed,
        )
    };
}

pub(crate) fn write_pasteboard(text: &str) {
    unsafe {
        let pasteboard: id = NSPasteboard::generalPasteboard(nil);
        let _: i64 = msg_send![pasteboard, clearContents];
        let string = crate::ns_string(text);
        let _: bool = msg_send![pasteboard, setString: string forType: NSPasteboardTypeString];
    }
}

unsafe extern "C" fn write_clipboard_cb(
    userdata: *mut c_void,
    clipboard: ffi::ghostty_clipboard_e,
    content: *const ffi::ghostty_clipboard_content_s,
    content_len: usize,
    confirm: bool,
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
    if confirm && let Some(shared) = unsafe { surface_shared(userdata) } {
        shared.send(SurfaceEvent::ConfirmClipboardWrite { text });
        return;
    }
    write_pasteboard(&text);
}

unsafe extern "C" fn close_surface_cb(userdata: *mut c_void, process_alive: bool) {
    if let Some(shared) = unsafe { surface_shared(userdata) } {
        shared.send(SurfaceEvent::Close { process_alive });
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
