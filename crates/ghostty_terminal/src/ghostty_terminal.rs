//! A terminal backed by the embedded Ghostty runtime (libghostty).
//!
//! Ghostty renders with its own Metal renderer into an IOSurface that it
//! assigns as the `contents` of a layer on an offscreen `NSView`. We composite
//! that IOSurface into GPUI's scene, so the terminal looks exactly like the
//! Ghostty app while GPUI overlays (menus, the command palette) still draw on
//! top of it. Keyboard input goes through [`input_view`], mouse input through
//! regular GPUI events.

#![cfg(target_os = "macos")]

mod claude_status;
mod columns;
mod graphics;
mod input_view;
mod runtime;
mod sheets;
mod tab_sessions;
mod terminal_column;
mod winman;
mod worktree_picker;

pub use columns::TerminalColumns;
pub use terminal_column::{
    ClaudeState, PickWorktree, TerminalColumn, TerminalColumnEvent, TerminalTab, worktree_split,
};

use std::{
    collections::HashMap,
    ffi::{CString, c_void},
    ops::Range,
    path::PathBuf,
    ptr,
    sync::OnceLock,
};

use anyhow::{Result, anyhow};
use cocoa::{
    base::{id, nil},
    foundation::{NSPoint, NSRect, NSSize, NSString, NSUInteger},
};
use core_foundation::base::TCFType;
use core_video::pixel_buffer::{CVPixelBuffer, CVPixelBufferRef};
use futures::{StreamExt as _, channel::mpsc};
use ghostty_embed as ffi;
use gpui::{
    App, Bounds, Context, CursorStyle, DispatchPhase, Entity, EventEmitter, FocusHandle, Focusable,
    Hitbox, HitboxBehavior, InteractiveElement, IntoElement, Modifiers, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement, Pixels, Render, ScrollDelta,
    ScrollWheelEvent, SharedString, Styled, Task, WeakEntity, Window, actions, canvas, div, px,
    size,
};
use objc::{
    class, msg_send,
    runtime::{Class, Object, Sel},
    sel, sel_impl,
};
use parking_lot::Mutex;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use ui::prelude::*;
use workspace::{
    Workspace,
    item::{Item, ItemEvent, TabContentParams},
};

use input_view::{InputState, InputView};
use runtime::{GhosttyRuntime, SurfaceEvent, SurfaceShared};

actions!(
    ghostty_terminal,
    [
        /// Opens a new Ghostty terminal in the active pane.
        NewGhosttyTerminal,
        /// Fullscreen for the side that has the keyboard (terminal or editor).
        ToggleFullscreen,
        /// Moves the keyboard to the terminal column.
        FocusTerminal,
        /// Moves the keyboard to the editor.
        FocusEditor,
        /// Opens the Ghostty config file in the editor.
        OpenConfig,
        /// Reloads the Ghostty config files.
        ReloadConfig,
    ]
);

fn column_of(workspace: &Workspace) -> Option<Entity<TerminalColumn>> {
    workspace
        .leading_column()?
        .clone()
        .downcast::<TerminalColumn>()
        .ok()
}

/// Moves the keyboard to the workspace's active editor (or pane).
pub fn focus_editor(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    if let Some(column) = column_of(workspace) {
        let layout = column.update(cx, |column, cx| column.prepare_side(false, cx));
        workspace.set_leading_column_layout(layout, cx);
    }
    let focus_handle = match workspace.active_item(cx) {
        Some(item) => item.item_focus_handle(cx),
        None => workspace.active_pane().focus_handle(cx),
    };
    window.focus(&focus_handle, cx);
}

/// winman's terminal width (800 pt, 650 at a 50 % width factor) for every
/// terminal column.
pub fn set_terminal_width(width: f32, cx: &mut App) {
    for column in TerminalColumns::all(cx) {
        column.update(cx, |column, cx| column.set_column_width(px(width), cx));
    }
}

/// winman's q+f: fullscreen for the side that has the keyboard.
pub fn toggle_fullscreen(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if let Some(column) = column_of(workspace) {
        let layout = column.update(cx, |column, cx| column.toggle_fullscreen(window, cx));
        workspace.set_leading_column_layout(layout, cx);
    }
}

/// Moves the keyboard to the terminal column. In fullscreen the column is
/// laid out first, since a hidden column cannot take focus.
pub fn focus_terminal(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    if let Some(column) = column_of(workspace) {
        let layout = column.update(cx, |column, cx| column.prepare_side(true, cx));
        workspace.set_leading_column_layout(layout, cx);
        window.focus(&column.focus_handle(cx), cx);
    }
}

pub fn init(cx: &mut App) {
    // Zed and the terminal are one app with one window holding every
    // workspace.
    cx.set_global(workspace::UnifiedWindow);
    winman::init(cx);

    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else {
            return;
        };
        if workspace.project().read(cx).is_local() {
            let handle = cx.entity().downgrade();
            let project = workspace.project().clone();
            let column =
                cx.new(|cx| TerminalColumn::new(handle.clone(), Some(project), window, cx));
            TerminalColumns::register(handle, column.clone(), cx);
            winman::watch_column(&column, cx);
            workspace.set_leading_column(Some(column.into()), cx);
            cx.on_release(|_, cx| {
                // `on_release` has no handle to the released entity; drop every
                // registration whose workspace is gone.
                if let Some(columns) = cx.try_global::<TerminalColumns>() {
                    let gone: Vec<_> = columns
                        .owners()
                        .filter(|owner| owner.upgrade().is_none())
                        .collect();
                    for owner in gone {
                        TerminalColumns::unregister(&owner, cx);
                    }
                }
            })
            .detach();
        }
        workspace.register_action(|workspace, _: &ToggleFullscreen, window, cx| {
            toggle_fullscreen(workspace, window, cx);
        });
        workspace.register_action(|workspace, _: &FocusTerminal, window, cx| {
            focus_terminal(workspace, window, cx);
        });
        workspace.register_action(|workspace, _: &FocusEditor, window, cx| {
            focus_editor(workspace, window, cx);
        });
        workspace.register_action(|_, _: &OpenConfig, _, cx| runtime::open_config(cx));
        workspace.register_action(|_, _: &ReloadConfig, _, _| runtime::reload_config());
    })
    .detach();

    // Pair the current terminal with whichever editor the window shows, and
    // let only that column draw.
    cx.observe_new(|_: &mut workspace::MultiWorkspace, window, cx| {
        let Some(window) = window else {
            return;
        };
        cx.subscribe_in(
            &cx.entity(),
            window,
            |multi_workspace, _, event, window, cx| {
                if matches!(
                    event,
                    workspace::MultiWorkspaceEvent::ActiveWorkspaceChanged { .. }
                        | workspace::MultiWorkspaceEvent::WorkspaceAdded(_)
                        | workspace::MultiWorkspaceEvent::WorkspaceRemoved(_)
                ) {
                    columns::sync(multi_workspace, window, cx);
                }
            },
        )
        .detach();
    })
    .detach();

    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &NewGhosttyTerminal, window, cx| {
            let working_directory = workspace
                .project()
                .read(cx)
                .visible_worktrees(cx)
                .next()
                .map(|worktree| worktree.read(cx).abs_path().to_path_buf());
            let options = TerminalOptions {
                working_directory,
                ..Default::default()
            };
            match GhosttyTerminal::open(options, window, cx) {
                Ok(terminal) => {
                    workspace.add_item_to_active_pane(Box::new(terminal), None, true, window, cx);
                }
                Err(error) => {
                    log::error!("failed to open a Ghostty terminal: {error:#}");
                    workspace.show_error(&error, cx);
                }
            }
        });
    })
    .detach();
}

#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub(crate) struct NSRange {
    pub location: NSUInteger,
    pub length: NSUInteger,
}

impl NSRange {
    fn invalid() -> Self {
        Self {
            location: cocoa::foundation::NSNotFound as NSUInteger,
            length: 0,
        }
    }
}

impl From<Range<usize>> for NSRange {
    fn from(range: Range<usize>) -> Self {
        NSRange {
            location: range.start as NSUInteger,
            length: range.len() as NSUInteger,
        }
    }
}

unsafe impl objc::Encode for NSRange {
    fn encode() -> objc::Encoding {
        let encoding = format!(
            "{{_NSRange={}{}}}",
            NSUInteger::encode().as_str(),
            NSUInteger::encode().as_str()
        );
        unsafe { objc::Encoding::from_str(&encoding) }
    }
}

#[allow(
    clippy::disallowed_methods,
    reason = "NSString::alloc is autoreleased right away"
)]
pub(crate) unsafe fn ns_string(string: &str) -> id {
    use cocoa::foundation::NSAutoreleasePool as _;
    unsafe { NSString::alloc(nil).init_str(string).autorelease() }
}

pub(crate) unsafe fn ns_string_to_string(string: id) -> Option<String> {
    if string == nil {
        return None;
    }
    unsafe {
        let bytes = NSString::UTF8String(string);
        if bytes.is_null() {
            return None;
        }
        Some(
            std::ffi::CStr::from_ptr(bytes)
                .to_string_lossy()
                .into_owned(),
        )
    }
}

/// Layers whose `contents` we watch, keyed by layer address.
static LAYER_LISTENERS: OnceLock<Mutex<HashMap<usize, mpsc::UnboundedSender<SurfaceEvent>>>> =
    OnceLock::new();

fn layer_listeners() -> &'static Mutex<HashMap<usize, mpsc::UnboundedSender<SurfaceEvent>>> {
    LAYER_LISTENERS.get_or_init(Default::default)
}

/// Ghostty presents a frame by assigning a new IOSurface to its layer's
/// `contents`. Overriding `setContents:` on its layer class tells us when to
/// repaint, without polling.
fn watch_layer_contents(layer: id) {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| unsafe {
        extern "C" fn set_contents(this: &Object, _: Sel, contents: id) {
            unsafe {
                let _: () = msg_send![super(this, class!(CALayer)), setContents: contents];
            }
            let key = this as *const Object as usize;
            if let Some(listener) = layer_listeners().lock().get(&key) {
                listener.unbounded_send(SurfaceEvent::Frame).ok();
            }
        }
        let layer_class: *const Class = msg_send![layer, class];
        let added = objc::runtime::class_addMethod(
            layer_class as *mut Class,
            sel!(setContents:),
            std::mem::transmute::<extern "C" fn(&Object, Sel, id), objc::runtime::Imp>(
                set_contents,
            ),
            c"v@:@".as_ptr(),
        );
        if added == objc::runtime::NO {
            log::error!("could not watch Ghostty's layer contents; the terminal will not repaint");
        }
    });
}

pub enum GhosttyTerminalEvent {
    TitleChanged,
    PwdChanged,
    CloseRequested {
        process_alive: bool,
    },
    Focused,
    NewSplit(ffi::ghostty_action_split_direction_e),
    NewTab,
    CloseTab(ffi::ghostty_action_close_tab_mode_e),
    GotoTab(i32),
    GotoSplit(ffi::ghostty_action_goto_split_e),
    ResizeSplit {
        direction: ffi::ghostty_action_resize_split_direction_e,
        amount: u16,
    },
    EqualizeSplits,
    ToggleSplitZoom,
}

/// How to start a new terminal.
#[derive(Default)]
pub struct TerminalOptions {
    pub working_directory: Option<PathBuf>,
    /// Typed into the shell once it starts, e.g. `claude --resume ...\n`.
    pub initial_input: Option<String>,
    /// Inherit font size, working directory etc. from this terminal, the way a
    /// new Ghostty tab or split does.
    pub inherit_from: Option<(WeakEntity<GhosttyTerminal>, InheritContext)>,
}

#[derive(Clone, Copy)]
pub enum InheritContext {
    Tab,
    Split,
}

pub struct GhosttyTerminal {
    focus_handle: FocusHandle,
    surface: Surface,
    title: SharedString,
    working_directory: Option<PathBuf>,
    /// The directory the shell last reported with OSC 7.
    reported_directory: Option<PathBuf>,
    cursor_style: CursorStyle,
    /// Last bounds and scale handed to Ghostty, to only resize on change.
    geometry: Option<(Bounds<Pixels>, f32)>,
    pressed_buttons: u8,
    _event_task: Task<()>,
    _subscriptions: Vec<gpui::Subscription>,
}

impl EventEmitter<GhosttyTerminalEvent> for GhosttyTerminal {}

/// The window's frame in AppKit screen coordinates (bottom-left origin):
/// x, y, width, height.
pub(crate) fn native_window_frame(window: &Window) -> Option<(f64, f64, f64, f64)> {
    let ns_window = gpui_native_window(window).ok()?;
    let frame: NSRect = unsafe { msg_send![ns_window, frame] };
    Some((
        frame.origin.x,
        frame.origin.y,
        frame.size.width,
        frame.size.height,
    ))
}

/// The primary screen's height, which AppKit's y coordinates count up from.
pub(crate) fn primary_screen_height() -> Option<f64> {
    unsafe {
        let screens: id = msg_send![class!(NSScreen), screens];
        if screens == nil {
            return None;
        }
        let count: NSUInteger = msg_send![screens, count];
        if count == 0 {
            return None;
        }
        let primary: id = msg_send![screens, objectAtIndex: 0 as NSUInteger];
        let frame: NSRect = msg_send![primary, frame];
        Some(frame.size.height)
    }
}

pub(crate) fn gpui_native_window(window: &Window) -> Result<id> {
    let view = gpui_native_view(window)?;
    let ns_window: id = unsafe { msg_send![view, window] };
    if ns_window == nil {
        return Err(anyhow!("the GPUI view is not in a window"));
    }
    Ok(ns_window)
}

fn gpui_native_view(window: &Window) -> Result<id> {
    let handle = HasWindowHandle::window_handle(window)
        .map_err(|error| anyhow!("no native window handle: {error}"))?;
    match handle.as_raw() {
        RawWindowHandle::AppKit(handle) => Ok(handle.ns_view.as_ptr() as id),
        _ => Err(anyhow!("the Ghostty terminal needs an AppKit window")),
    }
}

/// The native resources behind one terminal, released together.
struct Surface {
    surface: ffi::ghostty_surface_t,
    shared: *mut SurfaceShared,
    /// Offscreen view Ghostty renders into; it is never part of a window.
    render_view: id,
    layer: id,
    input_view: InputView,
}

impl Surface {
    fn new(
        options: &TerminalOptions,
        window: &mut Window,
        cx: &mut App,
    ) -> Result<(Self, mpsc::UnboundedReceiver<SurfaceEvent>)> {
        let app = GhosttyRuntime::global(cx)?.app();
        let inherit = options.inherit_from.as_ref().and_then(|(parent, context)| {
            let parent = parent.upgrade()?;
            let context = match context {
                InheritContext::Tab => ffi::GHOSTTY_SURFACE_CONTEXT_TAB,
                InheritContext::Split => ffi::GHOSTTY_SURFACE_CONTEXT_SPLIT,
            };
            Some((parent.read(cx).surface.surface, context))
        });
        let gpui_view = gpui_native_view(window)?;
        let scale = window.scale_factor() as f64;

        let (events_tx, events_rx) = mpsc::unbounded();
        let shared = Box::into_raw(Box::new(SurfaceShared {
            surface: Mutex::new(ptr::null_mut()),
            events: events_tx.clone(),
        }));

        let working_directory_c = options
            .working_directory
            .as_ref()
            .and_then(|path| CString::new(path.to_string_lossy().as_bytes()).ok());
        let initial_input_c = options
            .initial_input
            .as_ref()
            .and_then(|input| CString::new(input.as_str()).ok());

        let (surface, render_view, layer) = unsafe {
            let render_view: id = msg_send![class!(NSView), alloc];
            let render_view: id = msg_send![render_view,
                initWithFrame: NSRect::new(NSPoint::new(0., 0.), NSSize::new(800., 600.))];

            let mut config = match inherit {
                Some((parent, context)) => ffi::ghostty_surface_inherited_config(parent, context),
                None => ffi::ghostty_surface_config_new(),
            };
            config.platform_tag = ffi::GHOSTTY_PLATFORM_MACOS;
            config.platform.macos = ffi::ghostty_platform_macos_s {
                nsview: render_view as *mut c_void,
            };
            config.userdata = shared as *mut c_void;
            config.scale_factor = scale;
            config.context =
                inherit.map_or(ffi::GHOSTTY_SURFACE_CONTEXT_TAB, |(_, context)| context);
            if let Some(working_directory) = working_directory_c.as_ref() {
                config.working_directory = working_directory.as_ptr();
            }
            if let Some(initial_input) = initial_input_c.as_ref() {
                config.initial_input = initial_input.as_ptr();
            }

            let surface = ffi::ghostty_surface_new(app, &config);
            if surface.is_null() {
                let _: () = msg_send![render_view, release];
                drop(Box::from_raw(shared));
                return Err(anyhow!("ghostty_surface_new failed"));
            }
            *(*shared).surface.lock() = surface;

            let layer: id = msg_send![render_view, layer];
            if layer == nil {
                ffi::ghostty_surface_free(surface);
                let _: () = msg_send![render_view, release];
                drop(Box::from_raw(shared));
                return Err(anyhow!("Ghostty did not attach a layer to its view"));
            }
            watch_layer_contents(layer);
            layer_listeners().lock().insert(layer as usize, events_tx);

            if let Some(display_id) = display_id(gpui_view) {
                ffi::ghostty_surface_set_display_id(surface, display_id);
            }

            (surface, render_view, layer)
        };

        let input_view = InputView::new(InputState::new(surface, gpui_view));
        Ok((
            Self {
                surface,
                shared,
                render_view,
                layer,
                input_view,
            },
            events_rx,
        ))
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        layer_listeners().lock().remove(&(self.layer as usize));
        unsafe {
            *(*self.shared).surface.lock() = ptr::null_mut();
            ffi::ghostty_surface_free(self.surface);
            let _: () = msg_send![self.render_view, release];
            drop(Box::from_raw(self.shared));
        }
    }
}

impl GhosttyTerminal {
    pub fn open(
        options: TerminalOptions,
        window: &mut Window,
        cx: &mut App,
    ) -> Result<Entity<Self>> {
        let (surface, events) = Surface::new(&options, window, cx)?;
        Ok(cx.new(|cx| Self::new(surface, events, options.working_directory, window, cx)))
    }

    fn new(
        surface: Surface,
        mut events_rx: mpsc::UnboundedReceiver<SurfaceEvent>,
        working_directory: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let event_task = cx.spawn(async move |this: WeakEntity<Self>, cx| {
            while let Some(event) = events_rx.next().await {
                let mut events = vec![event];
                while let Ok(event) = events_rx.try_recv() {
                    events.push(event);
                }
                let result = this.update(cx, |this, cx| {
                    for event in events {
                        this.handle_surface_event(event, cx);
                    }
                });
                if result.is_err() {
                    break;
                }
            }
        });

        let focus_handle = cx.focus_handle();
        let subscriptions = vec![
            cx.on_focus(&focus_handle, window, |this, _window, cx| {
                this.set_focused(true);
                cx.emit(GhosttyTerminalEvent::Focused);
            }),
            cx.on_blur(&focus_handle, window, |this, _window, _cx| {
                this.set_focused(false);
            }),
            cx.observe_window_activation(window, |this, window, _cx| unsafe {
                ffi::ghostty_app_set_focus(
                    ffi::ghostty_surface_app(this.surface.surface),
                    window.is_window_active(),
                );
            }),
        ];

        Self {
            focus_handle,
            surface,
            title: "Terminal".into(),
            working_directory,
            reported_directory: None,
            cursor_style: CursorStyle::IBeam,
            geometry: None,
            pressed_buttons: 0,
            _event_task: event_task,
            _subscriptions: subscriptions,
        }
    }

    /// Sends text to the terminal as if it had been pasted.
    pub fn input_text(&self, text: &str) {
        if let Ok(text_c) = CString::new(text) {
            unsafe { ffi::ghostty_surface_text(self.surface.surface, text_c.as_ptr(), text.len()) };
        }
    }

    /// The last frame Ghostty presented, as tightly packed RGBA rows.
    pub fn frame_rgba(&self) -> Option<(u32, u32, Vec<u8>)> {
        unsafe extern "C" {
            fn IOSurfaceLock(surface: id, options: u32, seed: *mut u32) -> i32;
            fn IOSurfaceUnlock(surface: id, options: u32, seed: *mut u32) -> i32;
            fn IOSurfaceGetBaseAddress(surface: id) -> *const u8;
            fn IOSurfaceGetBytesPerRow(surface: id) -> usize;
            fn IOSurfaceGetWidth(surface: id) -> usize;
            fn IOSurfaceGetHeight(surface: id) -> usize;
        }
        const READ_ONLY: u32 = 1;
        unsafe {
            let io_surface: id = msg_send![self.surface.layer, contents];
            if io_surface == nil || IOSurfaceLock(io_surface, READ_ONLY, ptr::null_mut()) != 0 {
                return None;
            }
            let width = IOSurfaceGetWidth(io_surface);
            let height = IOSurfaceGetHeight(io_surface);
            let stride = IOSurfaceGetBytesPerRow(io_surface);
            let base = IOSurfaceGetBaseAddress(io_surface);
            let mut rgba = Vec::with_capacity(width * height * 4);
            for row in 0..height {
                let line = std::slice::from_raw_parts(base.add(row * stride), width * 4);
                for pixel in line.chunks_exact(4) {
                    rgba.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
                }
            }
            IOSurfaceUnlock(io_surface, READ_ONLY, ptr::null_mut());
            Some((width as u32, height as u32, rgba))
        }
    }

    pub fn reported_directory(&self) -> Option<&PathBuf> {
        self.reported_directory.as_ref()
    }

    pub fn working_directory(&self) -> Option<&PathBuf> {
        self.working_directory.as_ref()
    }

    fn read_selection(&self, selection: ffi::ghostty_selection_s) -> Option<(String, f64, f64)> {
        unsafe {
            let mut text: ffi::ghostty_text_s = std::mem::zeroed();
            if !ffi::ghostty_surface_read_text(self.surface.surface, selection, &mut text) {
                return None;
            }
            let result = (!text.text.is_null()).then(|| {
                let bytes = std::slice::from_raw_parts(text.text as *const u8, text.text_len);
                (
                    String::from_utf8_lossy(bytes).into_owned(),
                    text.tl_px_x,
                    text.tl_px_y,
                )
            });
            ffi::ghostty_surface_free_text(self.surface.surface, &mut text);
            result
        }
    }

    fn viewport_selection(from: (u32, u32), to: (u32, u32)) -> ffi::ghostty_selection_s {
        let point = |(x, y): (u32, u32)| ffi::ghostty_point_s {
            tag: ffi::GHOSTTY_POINT_VIEWPORT,
            coord: ffi::GHOSTTY_POINT_COORD_EXACT,
            x,
            y,
        };
        ffi::ghostty_selection_s {
            top_left: point(from),
            bottom_right: point(to),
            rectangle: false,
        }
    }

    /// Rows, cell width, cell height (points) and columns of the screen.
    pub fn grid(&self) -> Option<(u32, f64, f64, u32)> {
        let (_, scale) = self.geometry?;
        let size = unsafe { ffi::ghostty_surface_size(self.surface.surface) };
        let scale = scale as f64;
        (size.columns > 0 && size.rows > 0 && size.cell_width_px > 0).then(|| {
            (
                size.rows as u32,
                size.cell_width_px as f64 / scale,
                size.cell_height_px as f64 / scale,
                size.columns as u32,
            )
        })
    }

    /// One screen row's text and the position of its first cell, relative to
    /// the terminal's top-left (points).
    pub fn read_viewport_row(&self, row: u32, columns: u32) -> Option<(String, f64, f64)> {
        let (text, x, y) = self.read_selection(Self::viewport_selection(
            (0, row),
            (columns.saturating_sub(1), row),
        ))?;
        (y >= 0.).then_some((text, x, y))
    }

    /// Whether `row` continues on the next one (a soft wrap, no newline).
    pub fn is_soft_wrapped(&self, row: u32, rows: u32, columns: u32) -> bool {
        if row + 1 >= rows {
            return false;
        }
        self.read_selection(Self::viewport_selection(
            (0, row),
            (columns.saturating_sub(1), row + 1),
        ))
        .is_some_and(|(text, _, _)| !text.contains('\n'))
    }

    /// The whole screen and scrollback, the last `max_lines` lines of it, and
    /// whether that cut anything.
    pub fn scrollback(&self, max_lines: usize) -> Option<(String, bool)> {
        let point = |coord| ffi::ghostty_point_s {
            tag: ffi::GHOSTTY_POINT_SCREEN,
            coord,
            x: 0,
            y: 0,
        };
        let (text, _, _) = self.read_selection(ffi::ghostty_selection_s {
            top_left: point(ffi::GHOSTTY_POINT_COORD_TOP_LEFT),
            bottom_right: point(ffi::GHOSTTY_POINT_COORD_BOTTOM_RIGHT),
            rectangle: false,
        })?;
        let lines: Vec<&str> = text.split('\n').collect();
        let truncated = lines.len() > max_lines;
        let tail = &lines[lines.len().saturating_sub(max_lines)..];
        Some((tail.join("\n"), truncated))
    }

    /// Shows the clipboard confirmation sheet on the terminal's window.
    fn ask_clipboard(
        &mut self,
        request: sheets::ClipboardRequest,
        contents: &str,
    ) -> futures::channel::oneshot::Receiver<bool> {
        let (sender, receiver) = futures::channel::oneshot::channel();
        let gpui_view = self.surface.input_view.state().gpui_view;
        unsafe {
            let window: id = msg_send![gpui_view, window];
            if window == nil {
                sender.send(false).ok();
            } else {
                sheets::confirm_clipboard(window, request, contents, sender);
            }
        }
        receiver
    }

    /// Where the terminal was last painted, in window coordinates.
    pub fn bounds(&self) -> Option<Bounds<Pixels>> {
        self.geometry.map(|(bounds, _)| bounds)
    }

    pub fn title(&self) -> &SharedString {
        &self.title
    }

    /// The pid of the process in the terminal's foreground (e.g. `claude`).
    pub fn foreground_pid(&self) -> Option<u32> {
        let pid = unsafe { ffi::ghostty_surface_foreground_pid(self.surface.surface) };
        (pid != 0).then_some(pid as u32)
    }

    pub fn needs_confirm_quit(&self) -> bool {
        unsafe { ffi::ghostty_surface_needs_confirm_quit(self.surface.surface) }
    }

    /// Whether Ghostty should draw frames: hidden terminals (other tabs,
    /// inactive workspaces) are occluded so they stop rendering.
    pub fn set_visible(&self, visible: bool) {
        unsafe { ffi::ghostty_surface_set_occlusion(self.surface.surface, visible) };
    }

    pub fn set_color_scheme(&self, dark: bool) {
        let scheme = if dark {
            ffi::GHOSTTY_COLOR_SCHEME_DARK
        } else {
            ffi::GHOSTTY_COLOR_SCHEME_LIGHT
        };
        unsafe { ffi::ghostty_surface_set_color_scheme(self.surface.surface, scheme) };
    }

    /// Presses and releases a key, given as a macOS virtual key code.
    pub fn press_key(&self, key_code: u32, mods: ffi::ghostty_input_mods_e) {
        for action in [ffi::GHOSTTY_ACTION_PRESS, ffi::GHOSTTY_ACTION_RELEASE] {
            let key_event = ffi::ghostty_input_key_s {
                action,
                mods,
                consumed_mods: ffi::GHOSTTY_MODS_NONE,
                keycode: key_code,
                text: ptr::null(),
                unshifted_codepoint: 0,
                composing: false,
            };
            unsafe { ffi::ghostty_surface_key(self.surface.surface, key_event) };
        }
    }

    fn set_focused(&mut self, focused: bool) {
        if focused {
            self.surface.input_view.make_first_responder();
        } else {
            self.surface.input_view.resign_first_responder();
        }
        unsafe { ffi::ghostty_surface_set_focus(self.surface.surface, focused) };
    }

    fn handle_surface_event(&mut self, event: SurfaceEvent, cx: &mut Context<Self>) {
        match event {
            SurfaceEvent::Frame => cx.notify(),
            SurfaceEvent::Title(title) => {
                self.title = title.into();
                cx.emit(GhosttyTerminalEvent::TitleChanged);
                cx.notify();
            }
            SurfaceEvent::Pwd(pwd) => {
                self.working_directory = Some(PathBuf::from(&pwd));
                self.reported_directory = Some(PathBuf::from(pwd));
                cx.emit(GhosttyTerminalEvent::PwdChanged);
            }
            SurfaceEvent::MouseShape(shape) => {
                self.cursor_style = cursor_style(shape);
                cx.notify();
            }
            SurfaceEvent::CellSize { width, height } => {
                if let Some((_, scale)) = self.geometry {
                    let scale = scale as f64;
                    self.surface.input_view.state().cell_size =
                        (width as f64 / scale, height as f64 / scale);
                }
            }
            SurfaceEvent::Close { process_alive } => {
                cx.emit(GhosttyTerminalEvent::CloseRequested { process_alive })
            }
            SurfaceEvent::NewSplit(direction) => cx.emit(GhosttyTerminalEvent::NewSplit(direction)),
            SurfaceEvent::NewTab => cx.emit(GhosttyTerminalEvent::NewTab),
            SurfaceEvent::CloseTab(mode) => cx.emit(GhosttyTerminalEvent::CloseTab(mode)),
            SurfaceEvent::GotoTab(tab) => cx.emit(GhosttyTerminalEvent::GotoTab(tab)),
            SurfaceEvent::GotoSplit(direction) => {
                cx.emit(GhosttyTerminalEvent::GotoSplit(direction))
            }
            SurfaceEvent::ResizeSplit { direction, amount } => {
                cx.emit(GhosttyTerminalEvent::ResizeSplit { direction, amount })
            }
            SurfaceEvent::EqualizeSplits => cx.emit(GhosttyTerminalEvent::EqualizeSplits),
            SurfaceEvent::ToggleSplitZoom => cx.emit(GhosttyTerminalEvent::ToggleSplitZoom),
            SurfaceEvent::ReloadConfig { soft } => {
                runtime::reload_surface_config(self.surface.surface, soft)
            }
            SurfaceEvent::ConfirmClipboardRead {
                text,
                state,
                request,
            } => {
                let kind = if request == ffi::GHOSTTY_CLIPBOARD_REQUEST_OSC_52_READ {
                    sheets::ClipboardRequest::Read
                } else {
                    sheets::ClipboardRequest::Paste
                };
                let surface = self.surface.surface;
                let answer = self.ask_clipboard(kind, &text);
                cx.spawn(async move |this, cx| {
                    let confirmed = answer.await.unwrap_or(false);
                    // The surface lives as long as the terminal does.
                    if this.upgrade().is_some() {
                        let pasted = if confirmed { text.as_str() } else { "" };
                        cx.update(|_| runtime::complete_clipboard(surface, pasted, state, true));
                    }
                })
                .detach();
            }
            SurfaceEvent::ConfirmClipboardWrite { text } => {
                let answer = self.ask_clipboard(sheets::ClipboardRequest::Write, &text);
                cx.spawn(async move |_, _| {
                    if answer.await.unwrap_or(false) {
                        runtime::write_pasteboard(&text);
                    }
                })
                .detach();
            }
        }
    }

    fn sync_geometry(&mut self, bounds: Bounds<Pixels>, scale: f32) {
        self.surface.input_view.state().terminal_origin =
            (f64::from(bounds.origin.x), f64::from(bounds.origin.y));
        if self.geometry == Some((bounds, scale)) {
            return;
        }
        let size_changed = self.geometry.is_none_or(|(old_bounds, old_scale)| {
            old_bounds.size != bounds.size || old_scale != scale
        });
        self.geometry = Some((bounds, scale));
        if !size_changed {
            return;
        }

        let width = f64::from(bounds.size.width);
        let height = f64::from(bounds.size.height);
        let scale = scale as f64;
        unsafe {
            let frame = NSRect::new(NSPoint::new(0., 0.), NSSize::new(width, height));
            let _: () = msg_send![self.surface.render_view, setFrame: frame];
            // The view never joins a window, so AppKit does not keep its
            // layer in sync. Ghostty sizes its frames from the layer.
            let _: () = msg_send![self.surface.layer, setBounds: frame];
            let _: () = msg_send![self.surface.layer, setContentsScale: scale];
            ffi::ghostty_surface_set_content_scale(self.surface.surface, scale, scale);
            ffi::ghostty_surface_set_size(
                self.surface.surface,
                (width * scale).round() as u32,
                (height * scale).round() as u32,
            );
        }
    }

    /// The IOSurface Ghostty last presented, wrapped for GPUI.
    fn current_frame(&self) -> Option<CVPixelBuffer> {
        unsafe extern "C" {
            fn CVPixelBufferCreateWithIOSurface(
                allocator: *const c_void,
                surface: *const c_void,
                attributes: *const c_void,
                pixel_buffer_out: *mut CVPixelBufferRef,
            ) -> i32;
        }
        unsafe {
            let contents: id = msg_send![self.surface.layer, contents];
            if contents == nil {
                return None;
            }
            let mut pixel_buffer: CVPixelBufferRef = ptr::null_mut();
            let status = CVPixelBufferCreateWithIOSurface(
                ptr::null(),
                contents as *const c_void,
                ptr::null(),
                &mut pixel_buffer,
            );
            if status != 0 || pixel_buffer.is_null() {
                return None;
            }
            Some(CVPixelBuffer::wrap_under_create_rule(pixel_buffer))
        }
    }

    fn mouse_position(&self, position: gpui::Point<Pixels>, modifiers: Modifiers) {
        let Some((bounds, _)) = self.geometry else {
            return;
        };
        let local = position - bounds.origin;
        unsafe {
            ffi::ghostty_surface_mouse_pos(
                self.surface.surface,
                f64::from(local.x),
                f64::from(local.y),
                gpui_mods(modifiers),
            );
        }
    }

    fn mouse_button(
        &mut self,
        pressed: bool,
        button: MouseButton,
        position: gpui::Point<Pixels>,
        modifiers: Modifiers,
    ) -> bool {
        let (ghostty_button, bit) = match button {
            MouseButton::Left => (ffi::GHOSTTY_MOUSE_LEFT, 1),
            MouseButton::Right => (ffi::GHOSTTY_MOUSE_RIGHT, 2),
            MouseButton::Middle => (ffi::GHOSTTY_MOUSE_MIDDLE, 4),
            _ => return false,
        };
        if pressed {
            self.pressed_buttons |= bit;
        } else if self.pressed_buttons & bit == 0 {
            return false;
        } else {
            self.pressed_buttons &= !bit;
        }
        self.mouse_position(position, modifiers);
        unsafe {
            ffi::ghostty_surface_mouse_button(
                self.surface.surface,
                if pressed {
                    ffi::GHOSTTY_MOUSE_PRESS
                } else {
                    ffi::GHOSTTY_MOUSE_RELEASE
                },
                ghostty_button,
                gpui_mods(modifiers),
            )
        }
    }

    fn scroll(&self, event: &ScrollWheelEvent) {
        let (x, y, precise) = match event.delta {
            // Ghostty doubles precise deltas; it "feels better".
            ScrollDelta::Pixels(delta) => (f64::from(delta.x) * 2., f64::from(delta.y) * 2., true),
            ScrollDelta::Lines(delta) => (delta.x as f64, delta.y as f64, false),
        };
        self.mouse_position(event.position, event.modifiers);
        unsafe {
            ffi::ghostty_surface_mouse_scroll(self.surface.surface, x, y, precise as i32);
        }
    }
}

unsafe fn display_id(gpui_view: id) -> Option<u32> {
    unsafe {
        let window: id = msg_send![gpui_view, window];
        if window == nil {
            return None;
        }
        let screen: id = msg_send![window, screen];
        if screen == nil {
            return None;
        }
        let description: id = msg_send![screen, deviceDescription];
        let number: id = msg_send![description, objectForKey: ns_string("NSScreenNumber")];
        if number == nil {
            return None;
        }
        Some(msg_send![number, unsignedIntValue])
    }
}

fn gpui_mods(modifiers: Modifiers) -> ffi::ghostty_input_mods_e {
    let mut mods = ffi::GHOSTTY_MODS_NONE;
    if modifiers.shift {
        mods |= ffi::GHOSTTY_MODS_SHIFT;
    }
    if modifiers.control {
        mods |= ffi::GHOSTTY_MODS_CTRL;
    }
    if modifiers.alt {
        mods |= ffi::GHOSTTY_MODS_ALT;
    }
    if modifiers.platform {
        mods |= ffi::GHOSTTY_MODS_SUPER;
    }
    mods
}

fn cursor_style(shape: ffi::ghostty_action_mouse_shape_e) -> CursorStyle {
    match shape {
        ffi::GHOSTTY_MOUSE_SHAPE_TEXT => CursorStyle::IBeam,
        ffi::GHOSTTY_MOUSE_SHAPE_VERTICAL_TEXT => CursorStyle::IBeamCursorForVerticalLayout,
        ffi::GHOSTTY_MOUSE_SHAPE_POINTER => CursorStyle::PointingHand,
        ffi::GHOSTTY_MOUSE_SHAPE_CROSSHAIR => CursorStyle::Crosshair,
        ffi::GHOSTTY_MOUSE_SHAPE_GRAB => CursorStyle::OpenHand,
        ffi::GHOSTTY_MOUSE_SHAPE_GRABBING => CursorStyle::ClosedHand,
        ffi::GHOSTTY_MOUSE_SHAPE_NOT_ALLOWED | ffi::GHOSTTY_MOUSE_SHAPE_NO_DROP => {
            CursorStyle::OperationNotAllowed
        }
        ffi::GHOSTTY_MOUSE_SHAPE_COL_RESIZE | ffi::GHOSTTY_MOUSE_SHAPE_EW_RESIZE => {
            CursorStyle::ResizeLeftRight
        }
        ffi::GHOSTTY_MOUSE_SHAPE_ROW_RESIZE | ffi::GHOSTTY_MOUSE_SHAPE_NS_RESIZE => {
            CursorStyle::ResizeUpDown
        }
        ffi::GHOSTTY_MOUSE_SHAPE_CONTEXT_MENU => CursorStyle::ContextualMenu,
        ffi::GHOSTTY_MOUSE_SHAPE_COPY => CursorStyle::DragCopy,
        _ => CursorStyle::Arrow,
    }
}

impl Focusable for GhosttyTerminal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for GhosttyTerminal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        let cursor_style = self.cursor_style;
        div()
            .id("ghostty-terminal")
            .key_context("GhosttyTerminal")
            .track_focus(&self.focus_handle)
            .size_full()
            .child(
                canvas(
                    |bounds, window, _cx| window.insert_hitbox(bounds, HitboxBehavior::Normal),
                    move |bounds, hitbox: Hitbox, window, cx| {
                        let frame = entity.update(cx, |this, _cx| {
                            this.sync_geometry(bounds, window.scale_factor());
                            this.current_frame()
                        });
                        if let Some(frame) = frame {
                            let scale = window.scale_factor();
                            // Paint at the frame's own size, pinned top-left like
                            // Ghostty's layer, so a stale frame during a resize is
                            // not stretched.
                            let frame_size = size(
                                px(frame.get_width() as f32 / scale),
                                px(frame.get_height() as f32 / scale),
                            );
                            window.paint_surface(Bounds::new(bounds.origin, frame_size), frame);
                        }
                        window.set_cursor_style(cursor_style, &hitbox);

                        window.on_mouse_event({
                            let entity = entity.clone();
                            let hitbox = hitbox.clone();
                            move |event: &MouseDownEvent, phase, window, cx| {
                                if phase != DispatchPhase::Bubble || !hitbox.is_hovered(window) {
                                    return;
                                }
                                entity.update(cx, |this, cx| {
                                    window.focus(&this.focus_handle, cx);
                                    this.mouse_button(
                                        true,
                                        event.button,
                                        event.position,
                                        event.modifiers,
                                    );
                                });
                                cx.stop_propagation();
                            }
                        });
                        window.on_mouse_event({
                            let entity = entity.clone();
                            move |event: &MouseUpEvent, phase, _window, cx| {
                                if phase != DispatchPhase::Bubble {
                                    return;
                                }
                                entity.update(cx, |this, _cx| {
                                    this.mouse_button(
                                        false,
                                        event.button,
                                        event.position,
                                        event.modifiers,
                                    );
                                });
                            }
                        });
                        window.on_mouse_event({
                            let entity = entity.clone();
                            let hitbox = hitbox.clone();
                            move |event: &MouseMoveEvent, phase, window, cx| {
                                if phase != DispatchPhase::Bubble {
                                    return;
                                }
                                let dragging = entity.read(cx).pressed_buttons != 0;
                                if !dragging && !hitbox.is_hovered(window) {
                                    return;
                                }
                                entity
                                    .read(cx)
                                    .mouse_position(event.position, event.modifiers);
                            }
                        });
                        window.on_mouse_event({
                            let entity = entity.clone();
                            move |event: &ScrollWheelEvent, phase, window, cx| {
                                if phase != DispatchPhase::Bubble || !hitbox.is_hovered(window) {
                                    return;
                                }
                                entity.read(cx).scroll(event);
                                cx.stop_propagation();
                            }
                        });
                    },
                )
                .size_full(),
            )
    }
}

impl Item for GhosttyTerminal {
    type Event = GhosttyTerminalEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        self.title.clone()
    }

    fn tab_content(
        &self,
        params: TabContentParams,
        _window: &Window,
        cx: &App,
    ) -> gpui::AnyElement {
        Label::new(self.tab_content_text(params.detail.unwrap_or_default(), cx))
            .color(params.text_color())
            .into_any_element()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Terminal))
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        match event {
            GhosttyTerminalEvent::TitleChanged => f(ItemEvent::UpdateTab),
            GhosttyTerminalEvent::CloseRequested { .. } => f(ItemEvent::CloseItem),
            _ => {}
        }
    }
}
