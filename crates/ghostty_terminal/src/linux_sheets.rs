//! The macOS sheets (`sheets.rs`) have no Linux counterpart yet: there is no
//! native window to attach them to (`gpui_native_window` fails), so these
//! keep the shared code compiling and are never reached.

use futures::channel::oneshot;

use crate::NativeWindow;

/// # Safety
///
/// Always safe on Linux; `unsafe` matches the macOS signature.
pub unsafe fn ask_blocked_note(_window: NativeWindow, _note: &str, reply: oneshot::Sender<Option<String>>) {
    reply.send(None).ok();
}

/// # Safety
///
/// Always safe on Linux; `unsafe` matches the macOS signature.
pub unsafe fn remind_full_disk_access(_window: NativeWindow) {}
