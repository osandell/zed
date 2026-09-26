//! "Konvertera till remote-session": move the Claude Code session running in a
//! terminal to machinehead and keep going there, in the same tab.
//!
//! The work is done by the `remotework` CLI (github.com/osandell/remotework), so
//! the editor only has to know which claude process the tab runs. `remotework move
//! --pid <pid>` mirrors the repo on machinehead, stops the local claude, copies the
//! session and starts `claude --resume` in tmux there. When that returns, the tab's
//! shell prompt is back and the tab types `remotework attach <name>` into itself, so
//! the terminal the session left is the one that shows it again. A pylon in the
//! bottom-left corner marks the tab as remote for as long as that ssh runs.

use std::{path::PathBuf, process::Command};

use gpui::SharedString;

use crate::{claude_status, winman};

/// What the pylon overlay shows for one terminal.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) enum RemoteState {
    #[default]
    Local,
    /// `remotework move` is running.
    Moving,
    /// Attached to this tmux session on machinehead.
    Remote(SharedString),
    /// The move failed; the message stays until the next attempt or a few seconds.
    Failed(SharedString),
}

fn remotework_bin() -> PathBuf {
    paths::home_dir().join(".local/bin/remotework")
}

/// A PATH that finds what remotework shells out to (ssh, git, rsync, lsof). An app
/// started from the Dock gets launchd's minimal PATH, not the login shell's.
const PATH: &str = "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin";

/// The claude process at or above `foreground` (the foreground is claude itself,
/// or one of its tools when a command is running).
pub(crate) fn claude_pid_under(foreground: i32) -> Option<i32> {
    let mut pid = foreground;
    for _ in 0..=8 {
        if claude_status::is_claude(pid) {
            return Some(pid);
        }
        match winman::parent_pid(pid) {
            Some(parent) if parent > 1 => pid = parent,
            _ => return None,
        }
    }
    None
}

/// Blocking: run it on the background executor. Returns the tmux session name, or
/// the CLI's own message (it is written for a person: which session, and why not).
pub(crate) fn move_session(claude_pid: i32) -> Result<String, String> {
    let bin = remotework_bin();
    if !bin.exists() {
        return Err(format!(
            "remotework saknas ({}): klona osandell/remotework och kör `just install`",
            bin.display()
        ));
    }
    let output = Command::new(&bin)
        .args(["move", "--pid", &claude_pid.to_string(), "--no-attach"])
        .env("PATH", PATH)
        .output()
        .map_err(|error| format!("kunde inte starta remotework: {error}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        let message = stderr
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("remotework misslyckades")
            .trim_start_matches("remotework: ")
            .to_string();
        return Err(message);
    }
    stdout
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .map(|line| line.trim().to_string())
        .ok_or_else(|| "remotework gav inget sessionsnamn".to_string())
}

/// The command the tab types into its own shell once the local claude has exited.
pub(crate) fn attach_command(name: &str) -> String {
    format!("{} attach {}", remotework_bin().display(), name)
}

/// Full argv of `pid` (KERN_PROCARGS2: argc, exec path, padding, then argv).
fn process_argv(pid: i32) -> Option<Vec<String>> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    let mut size: libc::size_t = 0;
    let status = unsafe {
        libc::sysctl(mib.as_mut_ptr(), 3, std::ptr::null_mut(), &mut size, std::ptr::null_mut(), 0)
    };
    if status != 0 || size == 0 {
        return None;
    }
    let mut buffer = vec![0u8; size];
    let status = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            buffer.as_mut_ptr() as *mut libc::c_void,
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if status != 0 || size < 4 {
        return None;
    }
    buffer.truncate(size);
    let argc = i32::from_ne_bytes(buffer[..4].try_into().ok()?) as usize;
    let mut rest = buffer[4..].split(|&b| b == 0).filter(|part| !part.is_empty());
    rest.next()?; // exec path
    Some(
        rest.take(argc)
            .map(|part| String::from_utf8_lossy(part).into_owned())
            .collect(),
    )
}

/// The remotework session `pid` is attached to, when `pid` is the
/// `ssh … tmux -L remotework attach -t =<name>` that `remotework attach` runs.
/// Recognising the process (rather than remembering what we typed) keeps the pylon
/// right however the tab got there: the menu, a restored tab, or typed by hand.
pub(crate) fn attached_name(pid: i32) -> Option<String> {
    let argv = process_argv(pid)?;
    if argv.first()?.rsplit('/').next()? != "ssh" {
        return None;
    }
    let marker = "tmux -L remotework attach -t =";
    argv.iter().find_map(|arg| {
        let start = arg.find(marker)? + marker.len();
        let name: String = arg[start..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
            .collect();
        (!name.is_empty()).then_some(name)
    })
}
