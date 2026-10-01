//! The Linux terminal: libghostty-vt (Ghostty's terminal core) parses the
//! program's output and keeps the screen; this file owns the pty, feeds it,
//! encodes keys and mouse with Ghostty's encoders and draws the cells with
//! GPUI. Everything above it (the column, tabs, sessions, winman's socket)
//! is the same code as on macOS.

use std::{
    ffi::c_void,
    io::{Read as _, Write as _},
    path::PathBuf,
    ptr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use anyhow::{Context as _, Result, anyhow};
use futures::{StreamExt as _, channel::mpsc};
use ghostty_vt_sys as vt;
use gpui::{
    App, Bounds, ClipboardItem, Context, CursorStyle, DismissEvent, DispatchPhase, Entity,
    EventEmitter, ExternalPaths, FocusHandle, Focusable, Font, FontStyle, FontWeight, Hitbox,
    HitboxBehavior, Hsla, InteractiveElement, IntoElement, Keystroke, KeystrokeEvent, Modifiers,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement, Pixels,
    Render, ScrollDelta, ScrollWheelEvent, SharedString, StrikethroughStyle, Styled, Subscription,
    Task, TextRun, UnderlineStyle, WeakEntity, Window, anchored, canvas, deferred, div, fill,
    point, px, size,
};
use parking_lot::Mutex;
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use ui::{ContextMenu, ContextMenuEntry, prelude::*};
use workspace::item::{Item, ItemEvent, TabContentParams};

use crate::remote_session::{self, RemoteState};
use crate::{GhosttyTerminalEvent, TerminalOptions, ffi, runtime, winman};

/// There is no native window handle to hand out on Linux (see `sheets`).
pub type NativeWindow = ();

pub(crate) fn gpui_native_window(_window: &Window) -> Result<NativeWindow> {
    Err(anyhow!("no native window on Linux"))
}

/// The window's frame, already top-left based. Wayland does not tell a
/// client where its window is, so the origin is the window's own.
pub(crate) fn native_window_frame(window: &Window) -> Option<(f64, f64, f64, f64)> {
    let bounds = window.bounds();
    Some((
        f64::from(bounds.origin.x),
        f64::from(bounds.origin.y),
        f64::from(bounds.size.width),
        f64::from(bounds.size.height),
    ))
}

/// `None`: `native_window_frame` is top-left based already.
pub(crate) fn primary_screen_height() -> Option<f64> {
    None
}

enum VtEvent {
    Output,
    Title(String),
    Pwd(String),
    Clipboard(String),
    Exited,
}

/// What libghostty-vt's callbacks reach: the pty to answer on and the
/// terminal's event stream. Lives as long as the terminal handle.
struct Effects {
    writer: Arc<Mutex<Box<dyn std::io::Write + Send>>>,
    events: mpsc::UnboundedSender<VtEvent>,
}

/// One libghostty-vt terminal with its render state and encoders. Not
/// thread-safe on its own; always used under the mutex in `Shared`.
struct Vt {
    terminal: vt::GhosttyTerminal,
    render: vt::GhosttyRenderState,
    rows: vt::GhosttyRenderStateRowIterator,
    cells: vt::GhosttyRenderStateRowCells,
    keys: vt::GhosttyKeyEncoder,
    key_event: vt::GhosttyKeyEvent,
    mouse: vt::GhosttyMouseEncoder,
    mouse_event: vt::GhosttyMouseEvent,
    effects: *mut Effects,
}

// SAFETY: the handles are only touched with the `Shared::vt` mutex held.
unsafe impl Send for Vt {}

impl Drop for Vt {
    fn drop(&mut self) {
        unsafe {
            vt::ghostty_mouse_event_free(self.mouse_event);
            vt::ghostty_mouse_encoder_free(self.mouse);
            vt::ghostty_key_event_free(self.key_event);
            vt::ghostty_key_encoder_free(self.keys);
            vt::ghostty_render_state_row_cells_free(self.cells);
            vt::ghostty_render_state_row_iterator_free(self.rows);
            vt::ghostty_render_state_free(self.render);
            vt::ghostty_terminal_free(self.terminal);
            drop(Box::from_raw(self.effects));
        }
    }
}

fn check(result: vt::GhosttyResult, what: &str) -> Result<()> {
    if result == vt::GHOSTTY_SUCCESS {
        Ok(())
    } else {
        Err(anyhow!("libghostty-vt: {what} failed ({result})"))
    }
}

unsafe fn ghostty_string(string: vt::GhosttyString) -> String {
    if string.ptr.is_null() || string.len == 0 {
        return String::new();
    }
    let bytes = unsafe { std::slice::from_raw_parts(string.ptr, string.len) };
    String::from_utf8_lossy(bytes).into_owned()
}

extern "C" fn on_write_pty(_terminal: vt::GhosttyTerminal, userdata: *mut c_void, data: *const u8, len: usize) {
    let effects = unsafe { &*(userdata as *const Effects) };
    let bytes = unsafe { std::slice::from_raw_parts(data, len) };
    if let Err(error) = effects.writer.lock().write_all(bytes) {
        log::warn!("terminal reply: {error}");
    }
}

extern "C" fn on_title_changed(terminal: vt::GhosttyTerminal, userdata: *mut c_void) {
    let effects = unsafe { &*(userdata as *const Effects) };
    let mut title: vt::GhosttyString = unsafe { std::mem::zeroed() };
    let result = unsafe {
        vt::ghostty_terminal_get(terminal, vt::GHOSTTY_TERMINAL_DATA_TITLE, &mut title as *mut _ as *mut c_void)
    };
    if result == vt::GHOSTTY_SUCCESS {
        effects.events.unbounded_send(VtEvent::Title(unsafe { ghostty_string(title) })).ok();
    }
}

extern "C" fn on_pwd_changed(terminal: vt::GhosttyTerminal, userdata: *mut c_void) {
    let effects = unsafe { &*(userdata as *const Effects) };
    let mut pwd: vt::GhosttyString = unsafe { std::mem::zeroed() };
    let result = unsafe {
        vt::ghostty_terminal_get(terminal, vt::GHOSTTY_TERMINAL_DATA_PWD, &mut pwd as *mut _ as *mut c_void)
    };
    if result != vt::GHOSTTY_SUCCESS {
        return;
    }
    let pwd = unsafe { ghostty_string(pwd) };
    // OSC 7 is `file://host/path`; keep the path.
    let path = match pwd.strip_prefix("file://") {
        Some(rest) => rest.find('/').map(|slash| rest[slash..].to_string()).unwrap_or_default(),
        None => pwd,
    };
    if !path.is_empty() {
        effects.events.unbounded_send(VtEvent::Pwd(path)).ok();
    }
}

extern "C" fn on_color_scheme(
    _terminal: vt::GhosttyTerminal,
    _userdata: *mut c_void,
    out: *mut vt::GhosttyColorScheme,
) -> bool {
    unsafe {
        *out = if runtime::is_dark() {
            vt::GHOSTTY_COLOR_SCHEME_DARK
        } else {
            vt::GHOSTTY_COLOR_SCHEME_LIGHT
        };
    }
    true
}

extern "C" fn on_clipboard_write(
    _terminal: vt::GhosttyTerminal,
    userdata: *mut c_void,
    write: *const vt::GhosttyClipboardWrite,
) {
    let effects = unsafe { &*(userdata as *const Effects) };
    let write = unsafe { &*write };
    let contents = if write.contents.is_null() {
        &[][..]
    } else {
        unsafe { std::slice::from_raw_parts(write.contents, write.contents_len) }
    };
    // Ghostty's default clipboard-write = allow: plain text goes through.
    let text = contents.iter().find_map(|content| {
        let mime = unsafe { ghostty_string(content.mime) };
        (mime.is_empty() || mime.starts_with("text/plain")).then(|| unsafe { ghostty_string(content.data) })
    });
    let result = match text {
        Some(text) => {
            effects.events.unbounded_send(VtEvent::Clipboard(text)).ok();
            vt::GHOSTTY_CLIPBOARD_WRITE_RESULT_SUCCESS
        }
        None => vt::GHOSTTY_CLIPBOARD_WRITE_RESULT_UNSUPPORTED,
    };
    let mut reply: vt::GhosttyClipboardWriteReply = unsafe { std::mem::zeroed() };
    reply.size = std::mem::size_of::<vt::GhosttyClipboardWriteReply>();
    reply.result = result;
    if let Some(answer) = write.reply {
        unsafe { answer(write, &reply) };
    }
}

impl Vt {
    fn new(columns: u16, rows: u16, effects: Effects) -> Result<Self> {
        let effects = Box::into_raw(Box::new(effects));
        unsafe {
            let mut terminal = ptr::null_mut();
            if let Err(error) = check(vt::ghostty_terminal_new(ptr::null(), &mut terminal, columns, rows), "terminal_new") {
                drop(Box::from_raw(effects));
                return Err(error);
            }
            let mut this = Self {
                terminal,
                render: ptr::null_mut(),
                rows: ptr::null_mut(),
                cells: ptr::null_mut(),
                keys: ptr::null_mut(),
                key_event: ptr::null_mut(),
                mouse: ptr::null_mut(),
                mouse_event: ptr::null_mut(),
                effects,
            };
            check(vt::ghostty_render_state_new(ptr::null(), &mut this.render), "render_state_new")?;
            check(vt::ghostty_render_state_row_iterator_new(ptr::null(), &mut this.rows), "row_iterator_new")?;
            check(vt::ghostty_render_state_row_cells_new(ptr::null(), &mut this.cells), "row_cells_new")?;
            check(vt::ghostty_key_encoder_new(ptr::null(), &mut this.keys), "key_encoder_new")?;
            check(vt::ghostty_key_event_new(ptr::null(), &mut this.key_event), "key_event_new")?;
            check(vt::ghostty_mouse_encoder_new(ptr::null(), &mut this.mouse), "mouse_encoder_new")?;
            check(vt::ghostty_mouse_event_new(ptr::null(), &mut this.mouse_event), "mouse_event_new")?;

            let set = |option, value: *const c_void| vt::ghostty_terminal_set(terminal, option, value);
            set(vt::GHOSTTY_TERMINAL_OPT_USERDATA, effects as *const c_void);
            set(vt::GHOSTTY_TERMINAL_OPT_WRITE_PTY, on_write_pty as *const c_void);
            set(vt::GHOSTTY_TERMINAL_OPT_TITLE_CHANGED, on_title_changed as *const c_void);
            set(vt::GHOSTTY_TERMINAL_OPT_PWD_CHANGED, on_pwd_changed as *const c_void);
            set(vt::GHOSTTY_TERMINAL_OPT_COLOR_SCHEME, on_color_scheme as *const c_void);
            set(vt::GHOSTTY_TERMINAL_OPT_CLIPBOARD_WRITE, on_clipboard_write as *const c_void);
            let lines: usize = runtime::config().scrollback_lines.unwrap_or(10_000);
            set(vt::GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_LINES, &lines as *const usize as *const c_void);
            Ok(this)
        }
    }

    fn write(&mut self, bytes: &[u8]) {
        unsafe { vt::ghostty_terminal_vt_write(self.terminal, bytes.as_ptr(), bytes.len()) };
    }

    fn get<T>(&self, data: vt::GhosttyTerminalData, out: &mut T) -> bool {
        unsafe { vt::ghostty_terminal_get(self.terminal, data, out as *mut T as *mut c_void) == vt::GHOSTTY_SUCCESS }
    }

    fn set_colors(&mut self, background: Rgb, foreground: Rgb, cursor: Option<Rgb>, palette: &[(u8, Rgb)]) {
        unsafe {
            let set = |option, value: *const c_void| vt::ghostty_terminal_set(self.terminal, option, value);
            let background = background.to_vt();
            let foreground = foreground.to_vt();
            set(vt::GHOSTTY_TERMINAL_OPT_COLOR_BACKGROUND, &background as *const _ as *const c_void);
            set(vt::GHOSTTY_TERMINAL_OPT_COLOR_FOREGROUND, &foreground as *const _ as *const c_void);
            match cursor {
                Some(cursor) => {
                    let cursor = cursor.to_vt();
                    set(vt::GHOSTTY_TERMINAL_OPT_COLOR_CURSOR, &cursor as *const _ as *const c_void)
                }
                None => set(vt::GHOSTTY_TERMINAL_OPT_COLOR_CURSOR, ptr::null()),
            };
            if !palette.is_empty() {
                let mut colors = [vt::GhosttyColorRgb { r: 0, g: 0, b: 0 }; 256];
                if !self.get(vt::GHOSTTY_TERMINAL_DATA_COLOR_PALETTE_DEFAULT, &mut colors) {
                    self.get(vt::GHOSTTY_TERMINAL_DATA_COLOR_PALETTE, &mut colors);
                }
                for (index, color) in palette {
                    colors[*index as usize] = color.to_vt();
                }
                set(vt::GHOSTTY_TERMINAL_OPT_COLOR_PALETTE, &colors as *const _ as *const c_void);
            }
        }
    }

    fn resize(&mut self, columns: u16, rows: u16, cell_width: u32, cell_height: u32) {
        unsafe { vt::ghostty_terminal_resize(self.terminal, columns, rows, cell_width, cell_height) };
    }

    fn scroll(&mut self, tag: vt::GhosttyTerminalScrollViewportTag, delta: isize) {
        let mut behavior: vt::GhosttyTerminalScrollViewport = unsafe { std::mem::zeroed() };
        behavior.tag = tag;
        behavior.value.delta = delta;
        unsafe { vt::ghostty_terminal_scroll_viewport(self.terminal, behavior) };
    }

    fn mouse_tracking(&self) -> bool {
        let mut tracking = false;
        self.get(vt::GHOSTTY_TERMINAL_DATA_MOUSE_TRACKING, &mut tracking) && tracking
    }

    fn alternate_screen(&self) -> bool {
        let mut screen: vt::GhosttyTerminalScreen = vt::GHOSTTY_TERMINAL_SCREEN_PRIMARY;
        self.get(vt::GHOSTTY_TERMINAL_DATA_ACTIVE_SCREEN, &mut screen);
        screen == vt::GHOSTTY_TERMINAL_SCREEN_ALTERNATE
    }

    /// Encodes a key with the terminal's current modes (cursor keys, kitty
    /// keyboard flags, ...).
    fn encode_key(&mut self, key: vt::GhosttyKey, mods: vt::GhosttyMods, text: Option<&str>, unshifted: u32) -> Vec<u8> {
        unsafe {
            vt::ghostty_key_encoder_setopt_from_terminal(self.keys, self.terminal);
            vt::ghostty_key_event_set_action(self.key_event, vt::GHOSTTY_KEY_ACTION_PRESS);
            vt::ghostty_key_event_set_key(self.key_event, key);
            vt::ghostty_key_event_set_mods(self.key_event, mods);
            vt::ghostty_key_event_set_consumed_mods(self.key_event, 0);
            vt::ghostty_key_event_set_unshifted_codepoint(self.key_event, unshifted);
            match text {
                Some(text) => vt::ghostty_key_event_set_utf8(self.key_event, text.as_ptr() as *const _, text.len()),
                None => vt::ghostty_key_event_set_utf8(self.key_event, ptr::null(), 0),
            }
            let mut buffer = [0u8; 128];
            let mut written = 0usize;
            let result = vt::ghostty_key_encoder_encode(
                self.keys,
                self.key_event,
                buffer.as_mut_ptr() as *mut _,
                buffer.len(),
                &mut written,
            );
            if result == vt::GHOSTTY_SUCCESS {
                buffer[..written].to_vec()
            } else {
                Vec::new()
            }
        }
    }

    fn encode_mouse(
        &mut self,
        action: vt::GhosttyMouseAction,
        button: Option<vt::GhosttyMouseButton>,
        mods: vt::GhosttyMods,
        position: (f32, f32),
        metrics: &Metrics,
        any_pressed: bool,
    ) -> Vec<u8> {
        unsafe {
            vt::ghostty_mouse_encoder_setopt_from_terminal(self.mouse, self.terminal);
            let mut size: vt::GhosttyMouseEncoderSize = std::mem::zeroed();
            size.size = std::mem::size_of::<vt::GhosttyMouseEncoderSize>();
            size.screen_width = metrics.surface_width;
            size.screen_height = metrics.surface_height;
            size.cell_width = metrics.cell_width_px;
            size.cell_height = metrics.cell_height_px;
            size.padding_left = metrics.padding_left_px;
            size.padding_top = metrics.padding_top_px;
            vt::ghostty_mouse_encoder_setopt(self.mouse, vt::GHOSTTY_MOUSE_ENCODER_OPT_SIZE, &size as *const _ as *const c_void);
            vt::ghostty_mouse_encoder_setopt(
                self.mouse,
                vt::GHOSTTY_MOUSE_ENCODER_OPT_ANY_BUTTON_PRESSED,
                &any_pressed as *const bool as *const c_void,
            );
            vt::ghostty_mouse_event_set_action(self.mouse_event, action);
            match button {
                Some(button) => vt::ghostty_mouse_event_set_button(self.mouse_event, button),
                None => vt::ghostty_mouse_event_clear_button(self.mouse_event),
            }
            vt::ghostty_mouse_event_set_mods(self.mouse_event, mods);
            vt::ghostty_mouse_event_set_position(self.mouse_event, vt::GhosttyMousePosition { x: position.0, y: position.1 });
            let mut buffer = [0u8; 64];
            let mut written = 0usize;
            let result = vt::ghostty_mouse_encoder_encode(
                self.mouse,
                self.mouse_event,
                buffer.as_mut_ptr() as *mut _,
                buffer.len(),
                &mut written,
            );
            if result == vt::GHOSTTY_SUCCESS {
                buffer[..written].to_vec()
            } else {
                Vec::new()
            }
        }
    }

    fn grid_ref(&self, tag: vt::GhosttyPointTag, x: u16, y: u32) -> Option<vt::GhosttyGridRef> {
        unsafe {
            let mut point: vt::GhosttyPoint = std::mem::zeroed();
            point.tag = tag;
            point.value.coordinate = vt::GhosttyPointCoordinate { x, y };
            let mut grid_ref: vt::GhosttyGridRef = std::mem::zeroed();
            grid_ref.size = std::mem::size_of::<vt::GhosttyGridRef>();
            (vt::ghostty_terminal_grid_ref(self.terminal, point, &mut grid_ref) == vt::GHOSTTY_SUCCESS).then_some(grid_ref)
        }
    }

    fn set_selection(&mut self, from: (u16, u32), to: (u16, u32)) {
        let (Some(start), Some(end)) = (
            self.grid_ref(vt::GHOSTTY_POINT_TAG_VIEWPORT, from.0, from.1),
            self.grid_ref(vt::GHOSTTY_POINT_TAG_VIEWPORT, to.0, to.1),
        ) else {
            return;
        };
        unsafe {
            let mut selection: vt::GhosttySelection = std::mem::zeroed();
            selection.size = std::mem::size_of::<vt::GhosttySelection>();
            selection.start = start;
            selection.end = end;
            vt::ghostty_terminal_set(self.terminal, vt::GHOSTTY_TERMINAL_OPT_SELECTION, &selection as *const _ as *const c_void);
        }
    }

    fn clear_selection(&mut self) {
        unsafe { vt::ghostty_terminal_set(self.terminal, vt::GHOSTTY_TERMINAL_OPT_SELECTION, ptr::null()) };
    }

    fn selection_text(&self) -> Option<String> {
        unsafe {
            let mut selection: vt::GhosttySelection = std::mem::zeroed();
            selection.size = std::mem::size_of::<vt::GhosttySelection>();
            if !self.get(vt::GHOSTTY_TERMINAL_DATA_SELECTION, &mut selection) {
                return None;
            }
            let mut options: vt::GhosttyTerminalSelectionFormatOptions = std::mem::zeroed();
            options.size = std::mem::size_of::<vt::GhosttyTerminalSelectionFormatOptions>();
            options.emit = vt::GHOSTTY_FORMATTER_FORMAT_PLAIN;
            options.unwrap = true;
            options.trim = true;
            options.selection = &selection;
            let mut buffer = vec![0u8; 1 << 20];
            let mut written = 0usize;
            let result = vt::ghostty_terminal_selection_format_buf(
                self.terminal,
                options,
                buffer.as_mut_ptr(),
                buffer.len(),
                &mut written,
            );
            (result == vt::GHOSTTY_SUCCESS && written > 0).then(|| String::from_utf8_lossy(&buffer[..written]).into_owned())
        }
    }

    /// The whole screen and scrollback as plain text.
    fn plain_text(&self) -> Option<String> {
        unsafe {
            let mut options: vt::GhosttyFormatterTerminalOptions = std::mem::zeroed();
            options.size = std::mem::size_of::<vt::GhosttyFormatterTerminalOptions>();
            options.emit = vt::GHOSTTY_FORMATTER_FORMAT_PLAIN;
            options.trim = true;
            let mut formatter = ptr::null_mut();
            if vt::ghostty_formatter_terminal_new(ptr::null(), &mut formatter, self.terminal, options) != vt::GHOSTTY_SUCCESS {
                return None;
            }
            let mut needed = 0usize;
            vt::ghostty_formatter_format_buf(formatter, ptr::null_mut(), 0, &mut needed);
            let mut buffer = vec![0u8; needed.max(1)];
            let mut written = 0usize;
            let result = vt::ghostty_formatter_format_buf(formatter, buffer.as_mut_ptr(), buffer.len(), &mut written);
            vt::ghostty_formatter_free(formatter);
            (result == vt::GHOSTTY_SUCCESS).then(|| String::from_utf8_lossy(&buffer[..written]).into_owned())
        }
    }

    /// Snapshots the screen for drawing.
    fn frame(&mut self, colors_fallback: (Rgb, Rgb)) -> Frame {
        let mut frame = Frame::default();
        unsafe {
            if vt::ghostty_render_state_update(self.render, self.terminal) != vt::GHOSTTY_SUCCESS {
                return frame;
            }
            let mut colors: vt::GhosttyRenderStateColors = std::mem::zeroed();
            colors.size = std::mem::size_of::<vt::GhosttyRenderStateColors>();
            let have_colors = vt::ghostty_render_state_get(
                self.render,
                vt::GHOSTTY_RENDER_STATE_DATA_COLORS,
                &mut colors as *mut _ as *mut c_void,
            ) == vt::GHOSTTY_SUCCESS;
            let (background, foreground) = if have_colors {
                (Rgb::from_vt(colors.background), Rgb::from_vt(colors.foreground))
            } else {
                colors_fallback
            };
            frame.background = background;
            frame.foreground = foreground;
            frame.cursor_color = (have_colors && colors.cursor_has_value).then(|| Rgb::from_vt(colors.cursor));

            let mut cursor: vt::GhosttyRenderStateCursor = std::mem::zeroed();
            cursor.size = std::mem::size_of::<vt::GhosttyRenderStateCursor>();
            if vt::ghostty_render_state_get(self.render, vt::GHOSTTY_RENDER_STATE_DATA_CURSOR, &mut cursor as *mut _ as *mut c_void)
                == vt::GHOSTTY_SUCCESS
                && cursor.visible
                && cursor.viewport_has_value
            {
                frame.cursor = Some(CursorFrame {
                    column: cursor.viewport_x,
                    row: cursor.viewport_y,
                    style: cursor.visual_style,
                });
            }

            if vt::ghostty_render_state_get(self.render, vt::GHOSTTY_RENDER_STATE_DATA_ROW_ITERATOR, &mut self.rows as *mut _ as *mut c_void)
                != vt::GHOSTTY_SUCCESS
            {
                return frame;
            }
            while vt::ghostty_render_state_row_iterator_next(self.rows) {
                let mut row = RowFrame::default();
                let mut raw_row: vt::GhosttyRow = 0;
                if vt::ghostty_render_state_row_get(self.rows, vt::GHOSTTY_RENDER_STATE_ROW_DATA_RAW, &mut raw_row as *mut _ as *mut c_void)
                    == vt::GHOSTTY_SUCCESS
                {
                    let mut wrap = false;
                    vt::ghostty_row_get(raw_row, vt::GHOSTTY_ROW_DATA_WRAP, &mut wrap as *mut bool as *mut c_void);
                    row.wrapped = wrap;
                }
                let mut selection: vt::GhosttyRenderStateRowSelection = std::mem::zeroed();
                selection.size = std::mem::size_of::<vt::GhosttyRenderStateRowSelection>();
                if vt::ghostty_render_state_row_get(
                    self.rows,
                    vt::GHOSTTY_RENDER_STATE_ROW_DATA_SELECTION,
                    &mut selection as *mut _ as *mut c_void,
                ) == vt::GHOSTTY_SUCCESS
                {
                    row.selection = Some((selection.start_x, selection.end_x));
                }
                if vt::ghostty_render_state_row_get(self.rows, vt::GHOSTTY_RENDER_STATE_ROW_DATA_CELLS, &mut self.cells as *mut _ as *mut c_void)
                    != vt::GHOSTTY_SUCCESS
                {
                    frame.rows.push(row);
                    continue;
                }
                while vt::ghostty_render_state_row_cells_next(self.cells) {
                    row.cells.push(self.cell());
                }
                frame.rows.push(row);
            }
            vt::ghostty_render_state_clean(self.render);
        }
        frame
    }

    unsafe fn cell(&self) -> CellFrame {
        unsafe {
            let get = |data, out: *mut c_void| vt::ghostty_render_state_row_cells_get(self.cells, data, out) == vt::GHOSTTY_SUCCESS;
            let mut cell = CellFrame::default();
            let mut raw: vt::GhosttyCell = 0;
            if get(vt::GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_RAW, &mut raw as *mut _ as *mut c_void) {
                let mut wide: vt::GhosttyCellWide = vt::GHOSTTY_CELL_WIDE_NARROW;
                vt::ghostty_cell_get(raw, vt::GHOSTTY_CELL_DATA_WIDE, &mut wide as *mut _ as *mut c_void);
                cell.wide = wide == vt::GHOSTTY_CELL_WIDE_WIDE;
                cell.spacer = wide == vt::GHOSTTY_CELL_WIDE_SPACER_TAIL || wide == vt::GHOSTTY_CELL_WIDE_SPACER_HEAD;
            }
            let mut length: u32 = 0;
            get(vt::GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_GRAPHEMES_LEN, &mut length as *mut _ as *mut c_void);
            if length > 0 {
                let mut codepoints = vec![0u32; length as usize];
                if get(vt::GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_GRAPHEMES_BUF, codepoints.as_mut_ptr() as *mut c_void) {
                    cell.text = codepoints.iter().filter_map(|&codepoint| char::from_u32(codepoint)).collect();
                }
            }
            let mut styled = false;
            get(vt::GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_HAS_STYLING, &mut styled as *mut _ as *mut c_void);
            if styled {
                let mut style: vt::GhosttyStyle = std::mem::zeroed();
                style.size = std::mem::size_of::<vt::GhosttyStyle>();
                if get(vt::GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_STYLE, &mut style as *mut _ as *mut c_void) {
                    cell.bold = style.bold;
                    cell.italic = style.italic;
                    cell.faint = style.faint;
                    cell.inverse = style.inverse;
                    cell.invisible = style.invisible;
                    cell.strikethrough = style.strikethrough;
                    cell.underline = style.underline != 0;
                    cell.undercurl = style.underline == 3;
                }
                let mut color = vt::GhosttyColorRgb { r: 0, g: 0, b: 0 };
                if get(vt::GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_FG_COLOR, &mut color as *mut _ as *mut c_void) {
                    cell.foreground = Some(Rgb::from_vt(color));
                }
                if get(vt::GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_BG_COLOR, &mut color as *mut _ as *mut c_void) {
                    cell.background = Some(Rgb::from_vt(color));
                }
            } else {
                // Unstyled cells can still carry a background (e.g. erased
                // with a colour set).
                let mut color = vt::GhosttyColorRgb { r: 0, g: 0, b: 0 };
                if get(vt::GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_BG_COLOR, &mut color as *mut _ as *mut c_void) {
                    cell.background = Some(Rgb::from_vt(color));
                }
            }
            cell
        }
    }
}

#[derive(Clone, Copy, Default, PartialEq, Debug)]
struct Rgb(u8, u8, u8);

impl Rgb {
    fn from_vt(color: vt::GhosttyColorRgb) -> Self {
        Rgb(color.r, color.g, color.b)
    }

    fn to_vt(self) -> vt::GhosttyColorRgb {
        vt::GhosttyColorRgb { r: self.0, g: self.1, b: self.2 }
    }

    fn from_rgba(color: gpui::Rgba) -> Self {
        let channel = |value: f32| (value.clamp(0., 1.) * 255.).round() as u8;
        Rgb(channel(color.r), channel(color.g), channel(color.b))
    }

    fn hsla(self, alpha: f32) -> Hsla {
        let mut color: Hsla = gpui::Rgba {
            r: self.0 as f32 / 255.,
            g: self.1 as f32 / 255.,
            b: self.2 as f32 / 255.,
            a: 1.,
        }
        .into();
        color.a = alpha;
        color
    }
}

#[derive(Default, Clone)]
struct CellFrame {
    text: String,
    foreground: Option<Rgb>,
    background: Option<Rgb>,
    bold: bool,
    italic: bool,
    faint: bool,
    inverse: bool,
    invisible: bool,
    strikethrough: bool,
    underline: bool,
    undercurl: bool,
    wide: bool,
    spacer: bool,
}

#[derive(Default)]
struct RowFrame {
    cells: Vec<CellFrame>,
    selection: Option<(u16, u16)>,
    wrapped: bool,
}

impl RowFrame {
    fn text(&self) -> String {
        self.cells
            .iter()
            .filter(|cell| !cell.spacer)
            .map(|cell| if cell.text.is_empty() { " " } else { cell.text.as_str() })
            .collect::<String>()
            .trim_end()
            .to_string()
    }
}

struct CursorFrame {
    column: u16,
    row: u16,
    style: vt::GhosttyRenderStateCursorVisualStyle,
}

#[derive(Default)]
struct Frame {
    rows: Vec<RowFrame>,
    background: Rgb,
    foreground: Rgb,
    cursor: Option<CursorFrame>,
    cursor_color: Option<Rgb>,
}

/// Cell geometry of the last layout, in points and device pixels.
#[derive(Clone, Copy, Default, PartialEq)]
struct Metrics {
    columns: u16,
    rows: u16,
    cell_width: f32,
    cell_height: f32,
    padding_left: f32,
    padding_top: f32,
    cell_width_px: u32,
    cell_height_px: u32,
    padding_left_px: u32,
    padding_top_px: u32,
    surface_width: u32,
    surface_height: u32,
}

/// The pty, the child, and the terminal state the reader thread feeds.
struct Shared {
    vt: Mutex<Vt>,
    writer: Arc<Mutex<Box<dyn std::io::Write + Send>>>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    child: Mutex<Box<dyn Child + Send + Sync>>,
    child_pid: Option<u32>,
    /// Coalesces repaint requests from the reader thread.
    output_pending: AtomicBool,
}

impl Shared {
    fn send(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        if let Err(error) = self.writer.lock().write_all(bytes) {
            log::warn!("terminal input: {error}");
        }
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        // Hang up the child, as Ghostty does when a surface closes.
        if let Err(error) = self.child.lock().kill() {
            log::debug!("terminal child: {error}");
        }
    }
}

/// The shell to start: the config's `command`, else $SHELL, both as a login
/// shell with Ghostty's shell integration injected (bash and zsh).
fn shell_command(options: &TerminalOptions, config: &runtime::Config) -> CommandBuilder {
    let resources = runtime::resources_dir();
    let mut builder = if let Some(command) = options.command.as_ref() {
        let mut builder = CommandBuilder::new("/bin/sh");
        builder.args(["-c", command]);
        builder
    } else {
        let shell = config
            .command
            .clone()
            .or_else(|| std::env::var("SHELL").ok())
            .unwrap_or_else(|| "/bin/bash".into());
        let mut parts = shell.split_whitespace();
        let program = parts.next().unwrap_or("/bin/bash").to_string();
        let mut args: Vec<String> = parts.map(str::to_string).collect();
        let name = program.rsplit('/').next().unwrap_or(&program).to_string();
        let integration = resources.as_ref().map(|dir| dir.join("shell-integration"));
        let mut builder = CommandBuilder::new(&program);
        match (name.as_str(), integration) {
            ("bash", Some(integration)) => {
                // Ghostty's bash injection: POSIX mode reads $ENV, which
                // sources the integration and then the normal startup files.
                builder.env("ENV", integration.join("bash/ghostty.bash"));
                builder.env("GHOSTTY_BASH_INJECT", "1");
                if let Ok(env) = std::env::var("ENV") {
                    builder.env("GHOSTTY_BASH_ENV", env);
                }
                args.insert(0, "--posix".into());
            }
            ("zsh", Some(integration)) => {
                if let Ok(zdotdir) = std::env::var("ZDOTDIR") {
                    builder.env("GHOSTTY_ZSH_ZDOTDIR", zdotdir);
                }
                builder.env("ZDOTDIR", integration.join("zsh"));
            }
            _ => {}
        }
        builder.args(args);
        builder
    };
    if let Some(directory) = options.working_directory.as_ref() {
        builder.cwd(directory);
    } else {
        builder.cwd(paths::home_dir());
    }
    builder.env("TERM", "xterm-ghostty");
    builder.env("COLORTERM", "truecolor");
    builder.env("TERM_PROGRAM", "ghostty");
    if let Some(resources) = resources.as_ref() {
        builder.env("GHOSTTY_RESOURCES_DIR", resources);
        if let Some(share) = resources.parent() {
            builder.env("TERMINFO", share.join("terminfo"));
        }
        builder.env("GHOSTTY_SHELL_FEATURES", "cursor,title,sudo");
    }
    for (name, value) in &config.env {
        builder.env(name, value);
    }
    builder
}

pub struct GhosttyTerminal {
    focus_handle: FocusHandle,
    shared: Arc<Shared>,
    title: SharedString,
    working_directory: Option<PathBuf>,
    reported_directory: Option<PathBuf>,
    metrics: Option<Metrics>,
    /// Where the terminal was last painted, in window coordinates.
    painted_bounds: Option<Bounds<Pixels>>,
    /// Text and soft wraps of the last frame's rows, for winman's hints.
    row_text: Vec<(String, bool)>,
    config_generation: Option<u64>,
    skin_colors: Option<Option<(Rgb, Rgb)>>,
    pressed_buttons: u8,
    /// Start of a selection drag, in viewport cells.
    selecting: Option<(u16, u32)>,
    context_menu: Option<(Entity<ContextMenu>, gpui::Point<Pixels>, Subscription)>,
    remote: RemoteState,
    _remote_task: Option<Task<()>>,
    _remote_watch: Task<()>,
    _event_task: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<GhosttyTerminalEvent> for GhosttyTerminal {}

impl GhosttyTerminal {
    pub fn open(options: TerminalOptions, window: &mut Window, cx: &mut App) -> Result<Entity<Self>> {
        let mut options = options;
        if options.working_directory.is_none()
            && let Some((parent, _context)) = options.inherit_from.as_ref()
            && let Some(parent) = parent.upgrade()
        {
            let parent = parent.read(cx);
            options.working_directory = parent
                .reported_directory
                .clone()
                .or_else(|| parent.working_directory.clone());
        }
        let config = runtime::config();
        let pty = native_pty_system()
            .openpty(PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 })
            .map_err(|error| anyhow!("{error}"))
            .context("opening a pty")?;
        let child = pty
            .slave
            .spawn_command(shell_command(&options, &config))
            .map_err(|error| anyhow!("{error}"))
            .context("starting the shell")?;
        drop(pty.slave);
        let child_pid = child.process_id();
        let mut reader = pty.master.try_clone_reader().map_err(|error| anyhow!("{error}"))?;
        let writer: Arc<Mutex<Box<dyn std::io::Write + Send>>> =
            Arc::new(Mutex::new(pty.master.take_writer().map_err(|error| anyhow!("{error}"))?));

        let (events_tx, events_rx) = mpsc::unbounded();
        let vt = Vt::new(80, 24, Effects { writer: writer.clone(), events: events_tx.clone() })?;
        let shared = Arc::new(Shared {
            vt: Mutex::new(vt),
            writer,
            master: Mutex::new(pty.master),
            child: Mutex::new(child),
            child_pid,
            output_pending: AtomicBool::new(false),
        });

        // The pty's output, fed to the terminal on its own thread.
        {
            let shared = Arc::downgrade(&shared);
            let events = events_tx.clone();
            std::thread::Builder::new()
                .name("ghostty-pty".into())
                .spawn(move || {
                    let mut buffer = vec![0u8; 64 * 1024];
                    loop {
                        let read = match reader.read(&mut buffer) {
                            Ok(0) | Err(_) => break,
                            Ok(read) => read,
                        };
                        let Some(shared) = shared.upgrade() else {
                            return;
                        };
                        shared.vt.lock().write(&buffer[..read]);
                        if !shared.output_pending.swap(true, Ordering::AcqRel) {
                            events.unbounded_send(VtEvent::Output).ok();
                        }
                    }
                    events.unbounded_send(VtEvent::Exited).ok();
                })
                .context("starting the pty reader")?;
        }

        if let Some(input) = options.initial_input.as_ref() {
            shared.send(input.as_bytes());
        }

        let working_directory = options.working_directory.clone();
        Ok(cx.new(|cx| Self::new(shared, events_rx, working_directory, window, cx)))
    }

    fn new(
        shared: Arc<Shared>,
        mut events_rx: mpsc::UnboundedReceiver<VtEvent>,
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
                        this.handle_vt_event(event, cx);
                    }
                });
                if result.is_err() {
                    break;
                }
            }
        });

        let focus_handle = cx.focus_handle();
        let intercept = {
            let entity = cx.entity().downgrade();
            let focus_handle = focus_handle.clone();
            cx.intercept_keystrokes(move |event: &KeystrokeEvent, window, cx| {
                if !focus_handle.is_focused(window) {
                    return;
                }
                // Keys go to the terminal, never to Zed's keymap, as on macOS.
                let handled = entity
                    .update(cx, |this, cx| this.key_down(&event.keystroke, window, cx))
                    .unwrap_or(false);
                if handled {
                    cx.stop_propagation();
                }
            })
        };
        let subscriptions = vec![
            cx.on_focus(&focus_handle, window, |_this, _window, cx| {
                cx.emit(GhosttyTerminalEvent::Focused);
                cx.notify();
            }),
            cx.on_blur(&focus_handle, window, |_this, _window, cx| cx.notify()),
            intercept,
        ];

        Self {
            focus_handle,
            shared,
            title: "Terminal".into(),
            working_directory,
            reported_directory: None,
            metrics: None,
            painted_bounds: None,
            row_text: Vec::new(),
            config_generation: None,
            skin_colors: None,
            pressed_buttons: 0,
            selecting: None,
            context_menu: None,
            remote: RemoteState::Local,
            _remote_task: None,
            _remote_watch: Self::watch_remote(cx),
            _event_task: event_task,
            _subscriptions: subscriptions,
        }
    }

    fn handle_vt_event(&mut self, event: VtEvent, cx: &mut Context<Self>) {
        match event {
            VtEvent::Output => {
                self.shared.output_pending.store(false, Ordering::Release);
                cx.notify();
            }
            VtEvent::Title(title) => {
                self.title = if title.is_empty() { "Terminal".into() } else { title.into() };
                cx.emit(GhosttyTerminalEvent::TitleChanged);
                cx.notify();
            }
            VtEvent::Pwd(pwd) => {
                self.working_directory = Some(PathBuf::from(&pwd));
                self.reported_directory = Some(PathBuf::from(pwd));
                cx.emit(GhosttyTerminalEvent::PwdChanged);
            }
            VtEvent::Clipboard(text) => cx.write_to_clipboard(ClipboardItem::new_string(text)),
            VtEvent::Exited => cx.emit(GhosttyTerminalEvent::CloseRequested { process_alive: false }),
        }
    }

    /// Runs a Ghostty keybinding action, e.g. `new_split:right`.
    pub fn binding_action(&self, action: &str) {
        log::warn!("binding action without a context: {action}");
    }

    fn run_action(&mut self, action: &str, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let (name, parameter) = action.split_once(':').unwrap_or((action, ""));
        match name {
            "new_tab" => cx.emit(GhosttyTerminalEvent::NewTab),
            "new_split" => cx.emit(GhosttyTerminalEvent::NewSplit(match parameter {
                "left" => ffi::GHOSTTY_SPLIT_DIRECTION_LEFT,
                "up" => ffi::GHOSTTY_SPLIT_DIRECTION_UP,
                "down" => ffi::GHOSTTY_SPLIT_DIRECTION_DOWN,
                _ => ffi::GHOSTTY_SPLIT_DIRECTION_RIGHT,
            })),
            "goto_split" => cx.emit(GhosttyTerminalEvent::GotoSplit(match parameter {
                "previous" => ffi::GHOSTTY_GOTO_SPLIT_PREVIOUS,
                "up" | "top" => ffi::GHOSTTY_GOTO_SPLIT_UP,
                "left" => ffi::GHOSTTY_GOTO_SPLIT_LEFT,
                "down" | "bottom" => ffi::GHOSTTY_GOTO_SPLIT_DOWN,
                "right" => ffi::GHOSTTY_GOTO_SPLIT_RIGHT,
                _ => ffi::GHOSTTY_GOTO_SPLIT_NEXT,
            })),
            "resize_split" => {
                let (direction, amount) = parameter.split_once(',').unwrap_or((parameter, "10"));
                cx.emit(GhosttyTerminalEvent::ResizeSplit {
                    direction: match direction {
                        "up" => ffi::GHOSTTY_RESIZE_SPLIT_UP,
                        "down" => ffi::GHOSTTY_RESIZE_SPLIT_DOWN,
                        "left" => ffi::GHOSTTY_RESIZE_SPLIT_LEFT,
                        _ => ffi::GHOSTTY_RESIZE_SPLIT_RIGHT,
                    },
                    amount: amount.parse().unwrap_or(10),
                })
            }
            "equalize_splits" => cx.emit(GhosttyTerminalEvent::EqualizeSplits),
            "toggle_split_zoom" => cx.emit(GhosttyTerminalEvent::ToggleSplitZoom),
            "toggle_command_palette" => cx.emit(GhosttyTerminalEvent::ToggleCommandPalette),
            "previous_tab" => cx.emit(GhosttyTerminalEvent::GotoTab(ffi::GHOSTTY_GOTO_TAB_PREVIOUS)),
            "next_tab" => cx.emit(GhosttyTerminalEvent::GotoTab(ffi::GHOSTTY_GOTO_TAB_NEXT)),
            "last_tab" => cx.emit(GhosttyTerminalEvent::GotoTab(ffi::GHOSTTY_GOTO_TAB_LAST)),
            "goto_tab" => {
                if let Ok(tab) = parameter.parse::<i32>() {
                    cx.emit(GhosttyTerminalEvent::GotoTab(tab));
                }
            }
            "close_surface" => cx.emit(GhosttyTerminalEvent::CloseRequested { process_alive: self.needs_confirm_quit() }),
            "close_tab" => cx.emit(GhosttyTerminalEvent::CloseTab(match parameter {
                "other" => ffi::GHOSTTY_ACTION_CLOSE_TAB_MODE_OTHER,
                "right" => ffi::GHOSTTY_ACTION_CLOSE_TAB_MODE_RIGHT,
                _ => ffi::GHOSTTY_ACTION_CLOSE_TAB_MODE_THIS,
            })),
            "copy_to_clipboard" => {
                let text = self.shared.vt.lock().selection_text();
                match text {
                    Some(text) => cx.write_to_clipboard(ClipboardItem::new_string(text)),
                    None => return false,
                }
            }
            "paste_from_clipboard" | "paste_from_selection" => {
                if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                    self.paste(&text);
                }
            }
            "select_all" => {
                let rows = self.metrics.map_or(0, |metrics| metrics.rows as u32);
                let columns = self.metrics.map_or(0, |metrics| metrics.columns);
                self.shared.vt.lock().set_selection((0, 0), (columns.saturating_sub(1), rows.saturating_sub(1)));
                cx.notify();
            }
            "clear_screen" => {
                self.shared.send(b"\x0c");
            }
            "scroll_to_top" => self.shared.vt.lock().scroll(vt::GHOSTTY_SCROLL_VIEWPORT_TOP, 0),
            "scroll_to_bottom" => self.shared.vt.lock().scroll(vt::GHOSTTY_SCROLL_VIEWPORT_BOTTOM, 0),
            "scroll_page_up" | "scroll_page_down" | "scroll_page_lines" => {
                let rows = self.metrics.map_or(24, |metrics| metrics.rows as isize);
                let delta = match name {
                    "scroll_page_up" => -rows,
                    "scroll_page_down" => rows,
                    _ => parameter.parse().unwrap_or(0),
                };
                self.shared.vt.lock().scroll(vt::GHOSTTY_SCROLL_VIEWPORT_DELTA, delta);
                cx.notify();
            }
            "text" => self.shared.send(&unescape(parameter)),
            "esc" => {
                let mut bytes = vec![0x1b];
                bytes.extend_from_slice(parameter.as_bytes());
                self.shared.send(&bytes);
            }
            "csi" => {
                let mut bytes = b"\x1b[".to_vec();
                bytes.extend_from_slice(parameter.as_bytes());
                self.shared.send(&bytes);
            }
            "reload_config" => {
                runtime::reload_config();
                cx.notify();
            }
            "open_config" => runtime::open_config(cx),
            "ignore" | "unbind" => {}
            other => {
                log::info!("Ghostty action not supported on Linux: {other}");
                return false;
            }
        }
        let _ = window;
        true
    }

    /// Sends text to the terminal as if it had been pasted.
    pub fn input_text(&self, text: &str) {
        self.paste(text);
    }

    fn paste(&self, text: &str) {
        // Bracketed paste is the program's choice (mode 2004); without it
        // newlines go through as typed returns.
        let bracketed = {
            let vt = self.shared.vt.lock();
            let mut mode: vt::GhosttyTerminalModeConfig = unsafe { std::mem::zeroed() };
            // DEC private mode 2004 (ghostty_mode_new is a header-only inline).
            mode.mode = 2004;
            vt.get(vt::GHOSTTY_TERMINAL_DATA_MODE, &mut mode) && mode.value
        };
        if bracketed {
            let mut bytes = b"\x1b[200~".to_vec();
            bytes.extend_from_slice(text.replace("\x1b[201~", "").as_bytes());
            bytes.extend_from_slice(b"\x1b[201~");
            self.shared.send(&bytes);
        } else {
            self.shared.send(text.replace("\r\n", "\r").replace('\n', "\r").as_bytes());
        }
    }

    pub fn reported_directory(&self) -> Option<&PathBuf> {
        self.reported_directory.as_ref()
    }

    pub fn working_directory(&self) -> Option<&PathBuf> {
        self.working_directory.as_ref()
    }

    /// Rows, cell width, cell height (points) and columns of the screen.
    pub fn grid(&self) -> Option<(u32, f64, f64, u32)> {
        let metrics = self.metrics?;
        (metrics.columns > 0 && metrics.rows > 0).then(|| {
            (
                metrics.rows as u32,
                metrics.cell_width as f64,
                metrics.cell_height as f64,
                metrics.columns as u32,
            )
        })
    }

    /// One screen row's text and the position of its first cell, relative to
    /// the terminal's top-left (points).
    pub fn read_viewport_row(&self, row: u32, _columns: u32) -> Option<(String, f64, f64)> {
        let metrics = self.metrics?;
        let (text, _) = self.row_text.get(row as usize)?;
        Some((
            text.clone(),
            metrics.padding_left as f64,
            (metrics.padding_top + (row as f32 + 1.) * metrics.cell_height) as f64,
        ))
    }

    /// Whether `row` continues on the next one (a soft wrap, no newline).
    pub fn is_soft_wrapped(&self, row: u32, rows: u32, _columns: u32) -> bool {
        row + 1 < rows && self.row_text.get(row as usize).is_some_and(|(_, wrapped)| *wrapped)
    }

    /// The whole screen and scrollback, the last `max_lines` lines of it, and
    /// whether that cut anything.
    pub fn scrollback(&self, max_lines: usize) -> Option<(String, bool)> {
        let text = self.shared.vt.lock().plain_text()?;
        let lines: Vec<&str> = text.split('\n').collect();
        let truncated = lines.len() > max_lines;
        let tail = &lines[lines.len().saturating_sub(max_lines)..];
        Some((tail.join("\n"), truncated))
    }

    pub fn bounds(&self) -> Option<Bounds<Pixels>> {
        self.painted_bounds
    }

    pub fn title(&self) -> &SharedString {
        &self.title
    }

    /// The pid of the process in the terminal's foreground (e.g. `claude`).
    pub fn foreground_pid(&self) -> Option<u32> {
        let leader = self.shared.master.lock().process_group_leader()?;
        (leader > 0).then_some(leader as u32)
    }

    pub fn needs_confirm_quit(&self) -> bool {
        match (self.foreground_pid(), self.shared.child_pid) {
            (Some(foreground), Some(shell)) => foreground != shell,
            _ => false,
        }
    }

    /// GPUI only paints what is shown, so there is nothing to pause.
    pub fn set_visible(&self, _visible: bool) {}

    pub fn set_color_scheme(&self, dark: bool) {
        runtime::set_color_scheme(dark);
    }

    /// Presses and releases a key, given as a macOS virtual key code (the
    /// shared code's vocabulary).
    pub fn press_key(&self, key_code: u32, mods: ffi::ghostty_input_mods_e) {
        let key = match key_code {
            0x24 => vt::GHOSTTY_KEY_ENTER,
            0x35 => vt::GHOSTTY_KEY_ESCAPE,
            0x30 => vt::GHOSTTY_KEY_TAB,
            0x33 => vt::GHOSTTY_KEY_BACKSPACE,
            0x20 => vt::GHOSTTY_KEY_U,
            0x7e => vt::GHOSTTY_KEY_ARROW_UP,
            0x7d => vt::GHOSTTY_KEY_ARROW_DOWN,
            0x7b => vt::GHOSTTY_KEY_ARROW_LEFT,
            0x7c => vt::GHOSTTY_KEY_ARROW_RIGHT,
            other => {
                log::warn!("press_key: no Linux key for macOS key code {other:#x}");
                return;
            }
        };
        let mut vt_mods: vt::GhosttyMods = 0;
        if mods & ffi::GHOSTTY_MODS_SHIFT != 0 {
            vt_mods |= vt::GHOSTTY_MODS_SHIFT as vt::GhosttyMods;
        }
        if mods & ffi::GHOSTTY_MODS_CTRL != 0 {
            vt_mods |= vt::GHOSTTY_MODS_CTRL as vt::GhosttyMods;
        }
        if mods & ffi::GHOSTTY_MODS_ALT != 0 {
            vt_mods |= vt::GHOSTTY_MODS_ALT as vt::GhosttyMods;
        }
        let unshifted = if key == vt::GHOSTTY_KEY_U { 'u' as u32 } else { 0 };
        let bytes = self.shared.vt.lock().encode_key(key, vt_mods, None, unshifted);
        self.shared.send(&bytes);
    }

    /// A key press, after Ghostty's keybindings (the macOS defaults plus the
    /// config's `keybind` lines). Returns whether the terminal used it.
    fn key_down(&mut self, keystroke: &Keystroke, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if let Some(action) = keybinding(keystroke) {
            if self.run_action(&action, window, cx) {
                cx.notify();
                return true;
            }
        }
        let (key, unshifted) = ghostty_key(&keystroke.key);
        let mods = vt_mods(&keystroke.modifiers);
        // Text the key types, unless a modifier makes it a control sequence.
        let text = keystroke
            .key_char
            .as_deref()
            .filter(|_| !keystroke.modifiers.control && !keystroke.modifiers.platform);
        if key == vt::GHOSTTY_KEY_UNIDENTIFIED && text.is_none() {
            return false;
        }
        let bytes = {
            let mut vt = self.shared.vt.lock();
            let bytes = vt.encode_key(key, mods, text, unshifted);
            if !bytes.is_empty() {
                vt.clear_selection();
                vt.scroll(vt::GHOSTTY_SCROLL_VIEWPORT_BOTTOM, 0);
            }
            bytes
        };
        if bytes.is_empty() {
            return false;
        }
        self.shared.send(&bytes);
        cx.notify();
        true
    }

    fn cell_at(&self, position: gpui::Point<Pixels>) -> Option<(u16, u32)> {
        let metrics = self.metrics?;
        let bounds = self.painted_bounds?;
        let local = position - bounds.origin;
        let column = ((f32::from(local.x) - metrics.padding_left) / metrics.cell_width).floor();
        let row = ((f32::from(local.y) - metrics.padding_top) / metrics.cell_height).floor();
        Some((
            column.clamp(0., metrics.columns.saturating_sub(1) as f32) as u16,
            row.clamp(0., metrics.rows.saturating_sub(1) as f32) as u32,
        ))
    }

    fn surface_position(&self, position: gpui::Point<Pixels>, window: &Window) -> (f32, f32) {
        let origin = self.painted_bounds.map_or(point(px(0.), px(0.)), |bounds| bounds.origin);
        let local = position - origin;
        let scale = window.scale_factor();
        (f32::from(local.x) * scale, f32::from(local.y) * scale)
    }

    fn mouse_button(
        &mut self,
        pressed: bool,
        button: MouseButton,
        position: gpui::Point<Pixels>,
        modifiers: Modifiers,
        window: &Window,
    ) -> bool {
        let (vt_button, bit) = match button {
            MouseButton::Left => (vt::GHOSTTY_MOUSE_BUTTON_LEFT, 1),
            MouseButton::Right => (vt::GHOSTTY_MOUSE_BUTTON_RIGHT, 2),
            MouseButton::Middle => (vt::GHOSTTY_MOUSE_BUTTON_MIDDLE, 4),
            _ => return false,
        };
        if pressed {
            self.pressed_buttons |= bit;
        } else if self.pressed_buttons & bit == 0 {
            return false;
        } else {
            self.pressed_buttons &= !bit;
        }
        let Some(metrics) = self.metrics else {
            return false;
        };
        let tracking = self.shared.vt.lock().mouse_tracking();
        // Shift overrides the program's mouse reporting, as in Ghostty.
        if tracking && !modifiers.shift {
            let surface = self.surface_position(position, window);
            let action = if pressed { vt::GHOSTTY_MOUSE_ACTION_PRESS } else { vt::GHOSTTY_MOUSE_ACTION_RELEASE };
            let bytes = self.shared.vt.lock().encode_mouse(
                action,
                Some(vt_button),
                vt_mods(&modifiers),
                surface,
                &metrics,
                self.pressed_buttons != 0,
            );
            self.shared.send(&bytes);
            return true;
        }
        if button == MouseButton::Left {
            if pressed {
                self.selecting = self.cell_at(position);
                self.shared.vt.lock().clear_selection();
            } else {
                self.selecting = None;
            }
        }
        false
    }

    fn mouse_moved(&mut self, position: gpui::Point<Pixels>, modifiers: Modifiers, window: &Window, cx: &mut Context<Self>) {
        let Some(metrics) = self.metrics else {
            return;
        };
        if let Some(start) = self.selecting
            && let Some(end) = self.cell_at(position)
        {
            self.shared.vt.lock().set_selection(start, end);
            cx.notify();
            return;
        }
        let tracking = self.shared.vt.lock().mouse_tracking();
        if tracking && !modifiers.shift {
            let surface = self.surface_position(position, window);
            let button = match self.pressed_buttons {
                bits if bits & 1 != 0 => Some(vt::GHOSTTY_MOUSE_BUTTON_LEFT),
                bits if bits & 2 != 0 => Some(vt::GHOSTTY_MOUSE_BUTTON_RIGHT),
                bits if bits & 4 != 0 => Some(vt::GHOSTTY_MOUSE_BUTTON_MIDDLE),
                _ => None,
            };
            let bytes = self.shared.vt.lock().encode_mouse(
                vt::GHOSTTY_MOUSE_ACTION_MOTION,
                button,
                vt_mods(&modifiers),
                surface,
                &metrics,
                self.pressed_buttons != 0,
            );
            self.shared.send(&bytes);
        }
    }

    fn scroll(&mut self, event: &ScrollWheelEvent, window: &Window, cx: &mut Context<Self>) {
        let Some(metrics) = self.metrics else {
            return;
        };
        let lines = match event.delta {
            ScrollDelta::Pixels(delta) => f32::from(delta.y) / metrics.cell_height,
            ScrollDelta::Lines(delta) => delta.y * 3.,
        };
        let lines = lines.round() as isize;
        if lines == 0 {
            return;
        }
        let mut vt = self.shared.vt.lock();
        if vt.mouse_tracking() {
            let surface = self.surface_position(event.position, window);
            let button = if lines > 0 { vt::GHOSTTY_MOUSE_BUTTON_FOUR } else { vt::GHOSTTY_MOUSE_BUTTON_FIVE };
            let mut bytes = Vec::new();
            for _ in 0..lines.unsigned_abs().min(10) {
                bytes.extend(vt.encode_mouse(
                    vt::GHOSTTY_MOUSE_ACTION_PRESS,
                    Some(button),
                    vt_mods(&event.modifiers),
                    surface,
                    &metrics,
                    false,
                ));
            }
            drop(vt);
            self.shared.send(&bytes);
        } else if vt.alternate_screen() {
            // Ghostty's mouse-scroll on the alternate screen: arrow keys.
            let key = if lines > 0 { vt::GHOSTTY_KEY_ARROW_UP } else { vt::GHOSTTY_KEY_ARROW_DOWN };
            let mut bytes = Vec::new();
            for _ in 0..lines.unsigned_abs() {
                bytes.extend(vt.encode_key(key, 0, None, 0));
            }
            drop(vt);
            self.shared.send(&bytes);
        } else {
            vt.scroll(vt::GHOSTTY_SCROLL_VIEWPORT_DELTA, -lines);
            drop(vt);
            cx.notify();
        }
    }

    /// Lays out the grid for `bounds` and resizes the terminal and pty when
    /// it changes.
    fn layout(&mut self, bounds: Bounds<Pixels>, window: &mut Window, cx: &App) -> (Font, Pixels, Metrics) {
        let config = runtime::config();
        let font = gpui::font(SharedString::from(
            config.font_family.clone().unwrap_or_else(|| "monospace".to_string()),
        ));
        let font_size = px(config.font_size);
        let text_system = window.text_system();
        let font_id = text_system.resolve_font(&font);
        let cell_width = text_system
            .advance(font_id, font_size, 'm')
            .map(|advance| f32::from(advance.width))
            .unwrap_or(config.font_size * 0.6);
        let ascent = f32::from(text_system.ascent(font_id, font_size));
        let descent = f32::from(text_system.descent(font_id, font_size)).abs();
        let scale = window.scale_factor();
        // Whole device pixels per cell, as Ghostty's grid.
        let cell_width = (cell_width * scale).round() / scale;
        let cell_height = ((ascent + descent) * scale).ceil() / scale;
        let (padding_left, padding_right) = config.padding_x;
        let (padding_top, padding_bottom) = config.padding_y;
        let width = f32::from(bounds.size.width) - padding_left - padding_right;
        let height = f32::from(bounds.size.height) - padding_top - padding_bottom;
        let columns = ((width / cell_width).floor() as i32).clamp(1, u16::MAX as i32) as u16;
        let rows = ((height / cell_height).floor() as i32).clamp(1, u16::MAX as i32) as u16;
        let metrics = Metrics {
            columns,
            rows,
            cell_width,
            cell_height,
            padding_left,
            padding_top,
            cell_width_px: (cell_width * scale).round() as u32,
            cell_height_px: (cell_height * scale).round() as u32,
            padding_left_px: (padding_left * scale).round() as u32,
            padding_top_px: (padding_top * scale).round() as u32,
            surface_width: (f32::from(bounds.size.width) * scale).round() as u32,
            surface_height: (f32::from(bounds.size.height) * scale).round() as u32,
        };
        if self.metrics.is_none_or(|old| old.columns != columns || old.rows != rows || old.cell_width_px != metrics.cell_width_px || old.cell_height_px != metrics.cell_height_px) {
            self.shared.vt.lock().resize(columns, rows, metrics.cell_width_px, metrics.cell_height_px);
            let size = PtySize { rows, cols: columns, pixel_width: metrics.cell_width_px as u16 * columns, pixel_height: metrics.cell_height_px as u16 * rows };
            if let Err(error) = self.shared.master.lock().resize(size) {
                log::warn!("pty resize: {error}");
            }
        }
        self.metrics = Some(metrics);
        self.painted_bounds = Some(bounds);
        self.apply_colors(cx);
        (font, font_size, metrics)
    }

    /// Pushes the config's (or the winman skin's) colours into the terminal
    /// when either changed.
    fn apply_colors(&mut self, cx: &App) {
        let skin = ui::has_winman_skin("terminal_panel", cx).then(|| {
            let colors = cx.theme().colors();
            (Rgb::from_rgba(colors.editor_background.into()), Rgb::from_rgba(colors.text.into()))
        });
        let generation = runtime::generation();
        if self.config_generation == Some(generation) && self.skin_colors == Some(skin) {
            return;
        }
        self.config_generation = Some(generation);
        self.skin_colors = Some(skin);
        let config = runtime::config();
        let colors = runtime::terminal_colors();
        let (background, foreground) = skin.unwrap_or((Rgb::from_rgba(colors.background), Rgb::from_rgba(colors.foreground)));
        let palette: Vec<(u8, Rgb)> = config.palette.iter().map(|(index, color)| (*index, Rgb::from_rgba(*color))).collect();
        self.shared
            .vt
            .lock()
            .set_colors(background, foreground, config.cursor_color.map(Rgb::from_rgba), &palette);
    }

    fn paint_frame(&mut self, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
        let (font, font_size, metrics) = self.layout(bounds, window, cx);
        let colors = runtime::terminal_colors();
        let frame = self
            .shared
            .vt
            .lock()
            .frame((Rgb::from_rgba(colors.background), Rgb::from_rgba(colors.foreground)));
        self.row_text = frame.rows.iter().map(|row| (row.text(), row.wrapped)).collect();
        let config = runtime::config();
        let selection_background = config.selection_background.map(Rgb::from_rgba);
        let selection_foreground = config.selection_foreground.map(Rgb::from_rgba);

        window.paint_quad(fill(bounds, frame.background.hsla(1.)));
        let origin = point(
            bounds.origin.x + px(metrics.padding_left),
            bounds.origin.y + px(metrics.padding_top),
        );
        let cell_width = px(metrics.cell_width);
        let line_height = px(metrics.cell_height);
        let focused = self.focus_handle.is_focused(window);

        for (row_index, row) in frame.rows.iter().enumerate() {
            let y = origin.y + line_height * row_index as f32;
            let selected = |column: usize| {
                row.selection
                    .is_some_and(|(start, end)| column >= start as usize && column <= end as usize)
            };
            // Backgrounds, merged per run of equal colour.
            let mut column = 0usize;
            while column < row.cells.len() {
                let cell = &row.cells[column];
                let (_, background) = cell_colors(cell, &frame, selected(column), selection_background, selection_foreground);
                let start = column;
                column += 1;
                while column < row.cells.len() {
                    let (_, next) = cell_colors(&row.cells[column], &frame, selected(column), selection_background, selection_foreground);
                    if next != background {
                        break;
                    }
                    column += 1;
                }
                if let Some(background) = background {
                    window.paint_quad(fill(
                        Bounds::new(
                            point((origin.x + cell_width * start as f32).floor(), y),
                            size((cell_width * (column - start) as f32).ceil(), line_height),
                        ),
                        background.hsla(1.),
                    ));
                }
            }

            // Text, shaped in runs pinned to the grid. Wide characters are
            // shaped alone so they span their two cells.
            let mut column = 0usize;
            while column < row.cells.len() {
                let cell = &row.cells[column];
                if cell.spacer || cell.text.is_empty() || cell.invisible {
                    column += 1;
                    continue;
                }
                let start = column;
                let mut text = String::new();
                let mut runs: Vec<TextRun> = Vec::new();
                let alone = cell.wide || !cell.text.is_ascii();
                loop {
                    let cell = &row.cells[column];
                    let (foreground, _) = cell_colors(cell, &frame, selected(column), selection_background, selection_foreground);
                    let run = TextRun {
                        len: cell.text.len(),
                        font: Font {
                            weight: if cell.bold { FontWeight::BOLD } else { FontWeight::NORMAL },
                            style: if cell.italic { FontStyle::Italic } else { FontStyle::Normal },
                            ..font.clone()
                        },
                        color: foreground.hsla(if cell.faint { 0.5 } else { 1. }),
                        background_color: None,
                        underline: cell.underline.then(|| UnderlineStyle {
                            color: Some(foreground.hsla(1.)),
                            thickness: px(1.),
                            wavy: cell.undercurl,
                        }),
                        strikethrough: cell.strikethrough.then(|| StrikethroughStyle {
                            color: Some(foreground.hsla(1.)),
                            thickness: px(1.),
                        }),
                    };
                    text.push_str(&cell.text);
                    match runs.last_mut() {
                        Some(last) if last.font == run.font && last.color == run.color && last.underline == run.underline && last.strikethrough == run.strikethrough => {
                            last.len += run.len
                        }
                        _ => runs.push(run),
                    }
                    column += 1;
                    if alone || column >= row.cells.len() {
                        break;
                    }
                    let next = &row.cells[column];
                    if next.spacer || next.text.is_empty() || next.invisible || next.wide || !next.text.is_ascii() {
                        break;
                    }
                }
                let position = point(origin.x + cell_width * start as f32, y);
                let force_width = (!alone).then_some(cell_width);
                let shaped = window.text_system().shape_line(text.into(), font_size, &runs, force_width);
                if let Err(error) = shaped.paint(position, line_height, gpui::TextAlign::Left, None, window, cx) {
                    log::debug!("terminal text: {error}");
                }
            }
        }

        if let Some(cursor) = frame.cursor.as_ref() {
            let color = frame.cursor_color.unwrap_or(frame.foreground).hsla(1.);
            let cell_origin = point(
                origin.x + cell_width * cursor.column as f32,
                origin.y + line_height * cursor.row as f32,
            );
            let wide = frame
                .rows
                .get(cursor.row as usize)
                .and_then(|row| row.cells.get(cursor.column as usize))
                .is_some_and(|cell| cell.wide);
            let width = if wide { cell_width * 2. } else { cell_width };
            let rect = |width: Pixels, height: Pixels, y_offset: Pixels| {
                Bounds::new(point(cell_origin.x, cell_origin.y + y_offset), size(width, height))
            };
            match cursor.style {
                vt::GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_BAR => window.paint_quad(fill(rect(px(2.), line_height, px(0.)), color)),
                vt::GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_UNDERLINE => {
                    window.paint_quad(fill(rect(width, px(2.), line_height - px(2.)), color))
                }
                _ if !focused || cursor.style == vt::GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_BLOCK_HOLLOW => {
                    window.paint_quad(gpui::outline(rect(width, line_height, px(0.)), color, gpui::BorderStyle::Solid))
                }
                _ => {
                    window.paint_quad(fill(rect(width, line_height, px(0.)), color));
                    // The character under a block cursor, in the background colour.
                    if let Some(cell) = frame
                        .rows
                        .get(cursor.row as usize)
                        .and_then(|row| row.cells.get(cursor.column as usize))
                        .filter(|cell| !cell.text.is_empty())
                    {
                        let run = TextRun {
                            len: cell.text.len(),
                            font: Font {
                                weight: if cell.bold { FontWeight::BOLD } else { FontWeight::NORMAL },
                                ..font.clone()
                            },
                            color: config.cursor_text.map(Rgb::from_rgba).unwrap_or(frame.background).hsla(1.),
                            background_color: None,
                            underline: None,
                            strikethrough: None,
                        };
                        let shaped = window.text_system().shape_line(cell.text.clone().into(), font_size, &[run], None);
                        if let Err(error) = shaped.paint(cell_origin, line_height, gpui::TextAlign::Left, None, window, cx) {
                            log::debug!("terminal cursor text: {error}");
                        }
                    }
                }
            }
        }
    }
}

/// A cell's text and background colour (`None`: the terminal background).
fn cell_colors(
    cell: &CellFrame,
    frame: &Frame,
    selected: bool,
    selection_background: Option<Rgb>,
    selection_foreground: Option<Rgb>,
) -> (Rgb, Option<Rgb>) {
    let mut foreground = cell.foreground.unwrap_or(frame.foreground);
    let mut background = cell.background;
    if cell.inverse {
        let swapped = background.unwrap_or(frame.background);
        background = Some(foreground);
        foreground = swapped;
    }
    if selected {
        // Ghostty's default selection: the cell's colours inverted.
        let selected_background = selection_background.unwrap_or(foreground);
        foreground = selection_foreground.unwrap_or(background.unwrap_or(frame.background));
        background = Some(selected_background);
    }
    (foreground, background)
}

fn vt_mods(modifiers: &Modifiers) -> vt::GhosttyMods {
    let mut mods: u32 = 0;
    if modifiers.shift {
        mods |= vt::GHOSTTY_MODS_SHIFT;
    }
    if modifiers.control {
        mods |= vt::GHOSTTY_MODS_CTRL;
    }
    if modifiers.alt {
        mods |= vt::GHOSTTY_MODS_ALT;
    }
    if modifiers.platform {
        mods |= vt::GHOSTTY_MODS_SUPER;
    }
    mods as vt::GhosttyMods
}

/// GPUI's key name as Ghostty's key and its unshifted codepoint.
fn ghostty_key(key: &str) -> (vt::GhosttyKey, u32) {
    let unshifted = |key: &str| key.chars().next().filter(|_| key.chars().count() == 1).map_or(0, |c| c as u32);
    let letter = |c: char| -> Option<vt::GhosttyKey> {
        c.is_ascii_lowercase().then(|| vt::GHOSTTY_KEY_A + (c as u32 - 'a' as u32) as vt::GhosttyKey)
    };
    let digit = |c: char| -> Option<vt::GhosttyKey> {
        c.is_ascii_digit().then(|| vt::GHOSTTY_KEY_DIGIT_0 + (c as u32 - '0' as u32) as vt::GhosttyKey)
    };
    let mut chars = key.chars();
    if let (Some(c), None) = (chars.next(), chars.next())
        && let Some(found) = letter(c).or_else(|| digit(c))
    {
        return (found, c as u32);
    }
    let found = match key {
        "enter" => vt::GHOSTTY_KEY_ENTER,
        "escape" => vt::GHOSTTY_KEY_ESCAPE,
        "tab" => vt::GHOSTTY_KEY_TAB,
        "backspace" => vt::GHOSTTY_KEY_BACKSPACE,
        "space" => vt::GHOSTTY_KEY_SPACE,
        "delete" => vt::GHOSTTY_KEY_DELETE,
        "insert" => vt::GHOSTTY_KEY_INSERT,
        "home" => vt::GHOSTTY_KEY_HOME,
        "end" => vt::GHOSTTY_KEY_END,
        "pageup" => vt::GHOSTTY_KEY_PAGE_UP,
        "pagedown" => vt::GHOSTTY_KEY_PAGE_DOWN,
        "up" => vt::GHOSTTY_KEY_ARROW_UP,
        "down" => vt::GHOSTTY_KEY_ARROW_DOWN,
        "left" => vt::GHOSTTY_KEY_ARROW_LEFT,
        "right" => vt::GHOSTTY_KEY_ARROW_RIGHT,
        "-" => vt::GHOSTTY_KEY_MINUS,
        "=" => vt::GHOSTTY_KEY_EQUAL,
        "[" => vt::GHOSTTY_KEY_BRACKET_LEFT,
        "]" => vt::GHOSTTY_KEY_BRACKET_RIGHT,
        ";" => vt::GHOSTTY_KEY_SEMICOLON,
        "'" => vt::GHOSTTY_KEY_QUOTE,
        "," => vt::GHOSTTY_KEY_COMMA,
        "." => vt::GHOSTTY_KEY_PERIOD,
        "/" => vt::GHOSTTY_KEY_SLASH,
        "\\" => vt::GHOSTTY_KEY_BACKSLASH,
        "`" => vt::GHOSTTY_KEY_BACKQUOTE,
        other => {
            let function = other
                .strip_prefix('f')
                .and_then(|number| number.parse::<u32>().ok())
                .filter(|number| (1..=25).contains(number));
            match function {
                Some(number) => vt::GHOSTTY_KEY_F1 + (number - 1) as vt::GhosttyKey,
                None => return (vt::GHOSTTY_KEY_UNIDENTIFIED, unshifted(other)),
            }
        }
    };
    (found, unshifted(key))
}

/// Ghostty's macOS default keybindings (winman's kanata sends the Mac's
/// chords on Linux too), as `trigger=action`.
const DEFAULT_KEYBINDS: &[(&str, &str)] = &[
    ("super+c", "copy_to_clipboard"),
    ("super+v", "paste_from_clipboard"),
    ("super+shift+v", "paste_from_selection"),
    ("super+a", "select_all"),
    ("super+k", "clear_screen"),
    ("super+t", "new_tab"),
    ("super+w", "close_surface"),
    ("super+alt+w", "close_tab"),
    ("super+d", "new_split:right"),
    ("super+shift+d", "new_split:down"),
    ("super+[", "goto_split:previous"),
    ("super+]", "goto_split:next"),
    ("super+alt+up", "goto_split:up"),
    ("super+alt+down", "goto_split:down"),
    ("super+alt+left", "goto_split:left"),
    ("super+alt+right", "goto_split:right"),
    ("super+ctrl+up", "resize_split:up,10"),
    ("super+ctrl+down", "resize_split:down,10"),
    ("super+ctrl+left", "resize_split:left,10"),
    ("super+ctrl+right", "resize_split:right,10"),
    ("super+ctrl+=", "equalize_splits"),
    ("super+shift+enter", "toggle_split_zoom"),
    ("super+shift+[", "previous_tab"),
    ("super+shift+]", "next_tab"),
    ("ctrl+tab", "next_tab"),
    ("ctrl+shift+tab", "previous_tab"),
    ("super+1", "goto_tab:1"),
    ("super+2", "goto_tab:2"),
    ("super+3", "goto_tab:3"),
    ("super+4", "goto_tab:4"),
    ("super+5", "goto_tab:5"),
    ("super+6", "goto_tab:6"),
    ("super+7", "goto_tab:7"),
    ("super+8", "goto_tab:8"),
    ("super+9", "last_tab"),
    ("super+shift+p", "toggle_command_palette"),
    ("super+,", "open_config"),
    ("super+shift+,", "reload_config"),
    ("super+home", "scroll_to_top"),
    ("super+end", "scroll_to_bottom"),
    ("super+pageup", "scroll_page_up"),
    ("super+pagedown", "scroll_page_down"),
    ("super+left", "text:\\x01"),
    ("super+right", "text:\\x05"),
    ("super+backspace", "text:\\x15"),
    ("alt+left", "esc:b"),
    ("alt+right", "esc:f"),
];

/// A trigger in canonical form: modifiers in a fixed order, then the key in
/// GPUI's naming.
fn canonical_trigger(trigger: &str) -> String {
    let mut ctrl = false;
    let mut alt = false;
    let mut shift = false;
    let mut logo = false;
    let mut key = String::new();
    for part in trigger.split('+') {
        match part.trim().to_lowercase().as_str() {
            "ctrl" | "control" => ctrl = true,
            "alt" | "opt" | "option" => alt = true,
            "shift" => shift = true,
            "super" | "cmd" | "command" => logo = true,
            other => {
                key = match other {
                    "page_up" => "pageup",
                    "page_down" => "pagedown",
                    "arrow_up" => "up",
                    "arrow_down" => "down",
                    "arrow_left" => "left",
                    "arrow_right" => "right",
                    "equal" => "=",
                    "minus" => "-",
                    "comma" => ",",
                    "period" => ".",
                    "bracket_left" => "[",
                    "bracket_right" => "]",
                    "return" => "enter",
                    other => other,
                }
                .to_string()
            }
        }
    }
    format!("{}{}{}{}{key}", if ctrl { "ctrl+" } else { "" }, if alt { "alt+" } else { "" }, if shift { "shift+" } else { "" }, if logo { "super+" } else { "" })
}

fn keybinding(keystroke: &Keystroke) -> Option<String> {
    let modifiers = &keystroke.modifiers;
    if !modifiers.control && !modifiers.alt && !modifiers.platform {
        return None;
    }
    let pressed = format!(
        "{}{}{}{}{}",
        if modifiers.control { "ctrl+" } else { "" },
        if modifiers.alt { "alt+" } else { "" },
        if modifiers.shift { "shift+" } else { "" },
        if modifiers.platform { "super+" } else { "" },
        keystroke.key
    );
    let config = runtime::config();
    // The config's keybinds win over the defaults, later lines over earlier.
    config
        .keybinds
        .iter()
        .rev()
        .map(|(trigger, action)| (canonical_trigger(trigger), action.clone()))
        .chain(DEFAULT_KEYBINDS.iter().map(|(trigger, action)| (canonical_trigger(trigger), action.to_string())))
        .find(|(trigger, _)| *trigger == pressed)
        .map(|(_, action)| action)
}

/// Ghostty's `text:` escapes (`\x01`, `\n`, `\\`).
fn unescape(text: &str) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            let mut buffer = [0u8; 4];
            bytes.extend_from_slice(c.encode_utf8(&mut buffer).as_bytes());
            continue;
        }
        match chars.next() {
            Some('x') => {
                let hex: String = chars.by_ref().take(2).collect();
                if let Ok(byte) = u8::from_str_radix(&hex, 16) {
                    bytes.push(byte);
                }
            }
            Some('n') => bytes.push(b'\n'),
            Some('r') => bytes.push(b'\r'),
            Some('t') => bytes.push(b'\t'),
            Some('e') => bytes.push(0x1b),
            Some(other) => {
                let mut buffer = [0u8; 4];
                bytes.extend_from_slice(other.encode_utf8(&mut buffer).as_bytes());
            }
            None => bytes.push(b'\\'),
        }
    }
    bytes
}

impl GhosttyTerminal {
    pub(crate) fn deploy_context_menu(
        &mut self,
        position: gpui::Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let claude = self
            .foreground_pid()
            .and_then(|pid| remote_session::claude_pid_under(pid as i32));
        let busy = self.remote == RemoteState::Moving;
        let entity = cx.entity().downgrade();
        let context_menu = ContextMenu::build(window, cx, move |menu, _, _| {
            let entry = ContextMenuEntry::new(if busy {
                "Flyttar till machinehead…"
            } else {
                "Konvertera till remote-session"
            })
            .icon(IconName::Pylon)
            .disabled(claude.is_none() || busy)
            .handler({
                let entity = entity.clone();
                move |window, cx| {
                    if let Some(pid) = claude {
                        entity
                            .update(cx, |this, cx| this.convert_to_remote(pid, window, cx))
                            .ok();
                    }
                }
            });
            let menu = menu.item(entry);
            if claude.is_none() && !busy {
                menu.label("Ingen Claude-session körs i den här fliken")
            } else {
                menu
            }
        });
        window.focus(&context_menu.focus_handle(cx), cx);
        let subscription = cx.subscribe_in(&context_menu, window, |this, _, _: &DismissEvent, window, cx| {
            if this
                .context_menu
                .as_ref()
                .is_some_and(|(menu, _, _)| menu.focus_handle(cx).contains_focused(window, cx))
            {
                window.focus(&this.focus_handle, cx);
            }
            this.context_menu.take();
            cx.notify();
        });
        self.context_menu = Some((context_menu, position, subscription));
        cx.notify();
    }

    fn convert_to_remote(&mut self, claude_pid: i32, _window: &mut Window, cx: &mut Context<Self>) {
        self.remote = RemoteState::Moving;
        cx.notify();
        self._remote_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { remote_session::move_session(claude_pid) })
                .await;
            this.update(cx, |this, cx| match result {
                Ok(name) => {
                    this.input_text(&remote_session::attach_command(&name));
                    this.press_key(winman::KEY_CODE_RETURN, ffi::GHOSTTY_MODS_NONE);
                    this.remote = RemoteState::Remote(name.into());
                    cx.notify();
                }
                Err(message) => {
                    this.remote = RemoteState::Failed(message.into());
                    cx.notify();
                    this._remote_task = Some(cx.spawn(async move |this, cx| {
                        cx.background_executor().timer(std::time::Duration::from_secs(12)).await;
                        this.update(cx, |this, cx| {
                            if matches!(this.remote, RemoteState::Failed(_)) {
                                this.remote = RemoteState::Local;
                                cx.notify();
                            }
                        })
                        .ok();
                    }));
                }
            })
            .ok();
        }));
    }

    fn watch_remote(cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(std::time::Duration::from_secs(3)).await;
                let Ok(foreground) = this.update(cx, |this, _| this.foreground_pid()) else {
                    return;
                };
                let attached = match foreground {
                    Some(pid) => cx.background_spawn(async move { remote_session::attached_name(pid as i32) }).await,
                    None => None,
                };
                let alive = this.update(cx, |this, cx| {
                    let next = match (&this.remote, attached) {
                        (RemoteState::Moving | RemoteState::Failed(_), _) => return,
                        (_, Some(name)) => RemoteState::Remote(name.into()),
                        (_, None) => RemoteState::Local,
                    };
                    if next != this.remote {
                        this.remote = next;
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    return;
                }
            }
        })
    }

    pub(crate) fn remote_session_name(&self) -> Option<String> {
        match &self.remote {
            RemoteState::Remote(name) => Some(name.to_string()),
            _ => None,
        }
    }

    fn render_remote_badge(&self, cx: &App) -> Option<impl IntoElement> {
        let (label, color) = match &self.remote {
            RemoteState::Local => return None,
            RemoteState::Moving => (SharedString::from("flyttar till machinehead…"), Color::Muted),
            RemoteState::Remote(name) => (name.clone(), Color::Accent),
            RemoteState::Failed(message) => (message.clone(), Color::Error),
        };
        Some(
            h_flex()
                .absolute()
                .left(px(6.))
                .bottom(px(4.))
                .gap_1()
                .px_1p5()
                .py_0p5()
                .rounded_sm()
                .bg(cx.theme().colors().elevated_surface_background.opacity(0.9))
                .child(Icon::new(IconName::Pylon).size(IconSize::Small).color(color))
                .child(Label::new(label).size(LabelSize::XSmall).color(color)),
        )
    }
}

impl Focusable for GhosttyTerminal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

/// Backslash-escapes the characters Ghostty.app escapes in dropped paths.
fn shell_escape(text: &str) -> String {
    const ESCAPED: &str = "\\ ()[]{}<>\"'`!#$&;|*?\t";
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        if ESCAPED.contains(character) {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

impl Render for GhosttyTerminal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        let badge = self.render_remote_badge(cx);
        let menu = self
            .context_menu
            .as_ref()
            .map(|(menu, position, _)| deferred(anchored().position(*position).child(menu.clone())).with_priority(1));
        div()
            .id("ghostty-terminal")
            .relative()
            .key_context("GhosttyTerminal")
            .track_focus(&self.focus_handle)
            .size_full()
            .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                let text = paths
                    .paths()
                    .iter()
                    .map(|path| shell_escape(&path.to_string_lossy()))
                    .collect::<Vec<_>>()
                    .join(" ");
                if !text.is_empty() {
                    window.focus(&this.focus_handle, cx);
                    this.input_text(&text);
                }
            }))
            .child(
                canvas(
                    |bounds, window, _cx| window.insert_hitbox(bounds, HitboxBehavior::Normal),
                    move |bounds, hitbox: Hitbox, window, cx| {
                        entity.update(cx, |this, cx| this.paint_frame(bounds, window, cx));
                        window.set_cursor_style(CursorStyle::IBeam, &hitbox);

                        window.on_mouse_event({
                            let entity = entity.clone();
                            let hitbox = hitbox.clone();
                            move |event: &MouseDownEvent, phase, window, cx| {
                                if phase != DispatchPhase::Bubble || !hitbox.is_hovered(window) {
                                    return;
                                }
                                entity.update(cx, |this, cx| {
                                    window.focus(&this.focus_handle, cx);
                                    let consumed = this.mouse_button(true, event.button, event.position, event.modifiers, window);
                                    if event.button == MouseButton::Right && !consumed {
                                        this.deploy_context_menu(event.position, window, cx);
                                    }
                                    cx.notify();
                                });
                                cx.stop_propagation();
                            }
                        });
                        window.on_mouse_event({
                            let entity = entity.clone();
                            move |event: &MouseUpEvent, phase, window, cx| {
                                if phase != DispatchPhase::Bubble {
                                    return;
                                }
                                entity.update(cx, |this, cx| {
                                    this.mouse_button(false, event.button, event.position, event.modifiers, window);
                                    cx.notify();
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
                                entity.update(cx, |this, cx| this.mouse_moved(event.position, event.modifiers, window, cx));
                            }
                        });
                        window.on_mouse_event({
                            let entity = entity.clone();
                            move |event: &ScrollWheelEvent, phase, window, cx| {
                                if phase != DispatchPhase::Bubble || !hitbox.is_hovered(window) {
                                    return;
                                }
                                entity.update(cx, |this, cx| this.scroll(event, window, cx));
                                cx.stop_propagation();
                            }
                        });
                    },
                )
                .size_full(),
            )
            .children(badge)
            .children(menu)
    }
}

impl Item for GhosttyTerminal {
    type Event = GhosttyTerminalEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        self.title.clone()
    }

    fn tab_content(&self, params: TabContentParams, _window: &Window, cx: &App) -> gpui::AnyElement {
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
