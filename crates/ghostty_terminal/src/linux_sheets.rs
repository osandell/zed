//! The macOS sheets (`sheets.rs`) on Linux. A text question has no GPUI
//! counterpart, so it is asked with zenity (a GTK dialog), off the main
//! thread; the answer comes back on the same channel as the NSAlert's.

use futures::channel::oneshot;

use crate::NativeWindow;

/// "What is blocking this tab?" with a text field prefilled with `note`.
/// Sends the trimmed text on OK, `None` on Cancel.
///
/// # Safety
///
/// Always safe on Linux; `unsafe` matches the macOS signature.
pub unsafe fn ask_blocked_note(_window: NativeWindow, note: &str, reply: oneshot::Sender<Option<String>>) {
    let note = note.to_string();
    std::thread::spawn(move || {
        let output = std::process::Command::new("zenity")
            .args([
                "--entry",
                "--title=Ghostty",
                "--text=What is blocking this tab?\nShown when you hover the red dot. Leave blank to clear the note.",
                &format!("--entry-text={note}"),
            ])
            .output();
        let answer = match output {
            Ok(output) if output.status.success() => {
                Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
            }
            Ok(_) => None,
            Err(error) => {
                log::warn!("blocked note: zenity: {error}");
                None
            }
        };
        reply.send(answer).ok();
    });
}

/// The macOS Full Disk Access reminder. Linux has no such permission, so
/// there is nothing to remind about.
///
/// # Safety
///
/// Always safe on Linux; `unsafe` matches the macOS signature.
pub unsafe fn remind_full_disk_access(_window: NativeWindow) {}
