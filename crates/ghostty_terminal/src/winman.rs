//! The interfaces winman had with the Ghostty fork, served in-process.
//!
//! - The control socket (`WinmanControlServer.swift`): winman addresses a
//!   terminal "window" by its title, the worktree's `~` path; here that is the
//!   worktree's terminal column in the one window.
//! - The mailbox (`WinmanTextInjector.swift`, `WinmanSurfaceReader.swift`):
//!   winman-gui types into, and reads, the active worktree's terminal.
//! - Reports to winman-gui (`WinmanClaudeReporter.swift`,
//!   `WinmanTabStripReporter.swift`) and the daemon (`WinmanEditorFollow.swift`).
//!
//! A Zed started with `--user-data-dir` (a test instance) keeps all of this in
//! its data directory, so it never answers for, or talks over, the real app.

use std::{
    collections::HashMap,
    io::{Read as _, Write as _},
    os::unix::net::{UnixListener, UnixStream},
    path::{Path, PathBuf},
    sync::{LazyLock, mpsc as std_mpsc},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use futures::{StreamExt as _, channel::mpsc};
use gpui::{App, AppContext as _, AsyncApp, Entity, Global, TaskExt as _};
use parking_lot::Mutex;
use util::paths::PathExt as _;

use crate::{
    ClaudeState, GhosttyTerminal, PickWorktree, TerminalColumn, TerminalColumnEvent,
    TerminalColumns, TerminalOptions, claude_status::is_claude,
};
use ghostty_embed as ffi;

const HEARTBEAT: Duration = Duration::from_secs(4);
const MAILBOX_MAX_AGE_MS: i64 = 10_000;
const MAX_SCROLLBACK_LINES: usize = 20_000;
pub(crate) const KEY_CODE_RETURN: u32 = 0x24;
const KEY_CODE_ESCAPE: u32 = 0x35;
const KEY_CODE_TAB: u32 = 0x30;

fn test_dir() -> Option<&'static PathBuf> {
    paths::custom_data_dir()
}

fn runtime_path(real: &str, name: &str) -> PathBuf {
    match test_dir() {
        Some(dir) => dir.join(name),
        None => PathBuf::from(real),
    }
}

fn control_socket_path() -> PathBuf {
    runtime_path(
        "/tmp/ghostty-winman-control.sock",
        "ghostty-winman-control.sock",
    )
}

fn gui_socket_path() -> PathBuf {
    runtime_path("/tmp/winman-gui.sock", "winman-gui.sock")
}

fn daemon_socket_path() -> PathBuf {
    runtime_path("/tmp/winman.sock", "winman.sock")
}

fn mailbox_path() -> PathBuf {
    runtime_path(
        "/tmp/winman-ghostty-inject.json",
        "winman-ghostty-inject.json",
    )
}

fn ack_path() -> PathBuf {
    runtime_path(
        "/tmp/winman-ghostty-inject.ack",
        "winman-ghostty-inject.ack",
    )
}

fn read_response_path() -> PathBuf {
    runtime_path("/tmp/winman-ghostty-read.json", "winman-ghostty-read.json")
}

fn tab_strips_dir() -> PathBuf {
    match test_dir() {
        Some(dir) => dir.join("ghostty-tab-strips"),
        None => paths::home_dir().join(".config/winman/ghostty-tab-strips"),
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}

/// A worktree path as winman writes it: `~` expanded, no trailing slash.
fn normalize(path: &str) -> PathBuf {
    let expanded = match path.strip_prefix("~/") {
        Some(rest) => paths::home_dir().join(rest),
        None if path == "~" => paths::home_dir().clone(),
        None => PathBuf::from(path),
    };
    let text = expanded.to_string_lossy();
    if text.len() > 1 && text.ends_with('/') {
        PathBuf::from(text.trim_end_matches('/'))
    } else {
        expanded
    }
}

/// winman's `BarView.workspaceSlug`, from a terminal's directory.
fn slug_for_directory(directory: Option<&Path>) -> String {
    let Some(directory) = directory else {
        return "unknown".into();
    };
    let text = directory.to_string_lossy();
    if text.is_empty() {
        return "unknown".into();
    }
    let home = paths::home_dir().to_string_lossy().into_owned();
    let relative = text
        .strip_prefix(&format!("{home}/"))
        .or_else(|| text.strip_prefix("~/"))
        .unwrap_or(&text);
    let mapped: String = relative
        .chars()
        .map(|character| match character {
            '/' | '.' | ':' | ' ' => '-',
            other => other,
        })
        .collect();
    let trimmed = mapped.trim_matches('-');
    if trimmed.is_empty() {
        "default".into()
    } else {
        trimmed.into()
    }
}

/// Fire-and-forget: one line to a winman socket, off the foreground thread.
fn send_line(path: PathBuf, line: String, cx: &App) {
    cx.background_spawn(async move {
        if let Ok(mut stream) = UnixStream::connect(&path) {
            let line = if line.ends_with('\n') {
                line
            } else {
                format!("{line}\n")
            };
            stream.write_all(line.as_bytes()).ok();
        }
    })
    .detach();
}

/// `show-editor` has no newline and the daemon reads it whole; returns whether
/// winman took it.
fn send_to_daemon(request: String) -> bool {
    UnixStream::connect(daemon_socket_path())
        .and_then(|mut stream| stream.write_all(request.as_bytes()))
        .is_ok()
}

/// A request to the daemon that is answered on the same connection
/// (`workspace-list`). Blocking: call it off the foreground thread.
fn daemon_request(request: &str) -> Option<String> {
    let mut stream = UnixStream::connect(daemon_socket_path()).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    stream.write_all(request.as_bytes()).ok()?;
    stream.shutdown(std::net::Shutdown::Write).ok()?;
    let mut reply = String::new();
    stream.read_to_string(&mut reply).ok()?;
    Some(reply)
}

/// What winman has to be told so its bar shows `worktree`'s workspace:
/// `set-active-worktree <workspace> <worktree>`, or nothing when that is what
/// it already shows or no workspace owns the path.
fn winman_follow_request(workspaces: &str, worktree: &Path) -> Option<String> {
    let workspaces: Vec<serde_json::Value> = serde_json::from_str(workspaces).ok()?;
    let worktree = worktree.to_string_lossy();
    let workspace = workspaces.iter().find(|workspace| {
        workspace["worktrees"]
            .as_array()
            .is_some_and(|worktrees| worktrees.iter().any(|path| path.as_str() == Some(&worktree)))
    })?;
    if workspace["active"].as_bool() == Some(true)
        && workspace["active_worktree"].as_str() == Some(&worktree)
    {
        return None;
    }
    let workspace_index = workspace["index"].as_u64()?;
    let worktree_index = workspace["worktrees"]
        .as_array()?
        .iter()
        .position(|path| path.as_str() == Some(&worktree))?;
    Some(format!(
        "set-active-worktree {workspace_index} {worktree_index}"
    ))
}

/// Zed showed `column` on its own (a tab focused over the control socket, the
/// fork button, the command palette): winman's bar and editor would still be
/// on the old workspace. Asks winman to follow. Only for switches winman did
/// not ask for: winman answers with `focus-window` for the same column, and
/// that path does not come back here, so there is no loop. Nothing is sent when
/// winman already shows the worktree, so the echo is a no-op even so.
pub(crate) fn follow_in_winman(column: &Entity<TerminalColumn>, cx: &App) {
    let Some(worktree) = column.read(cx).workspace_path().cloned() else {
        return;
    };
    cx.background_spawn(async move {
        let Some(workspaces) = daemon_request("workspace-list") else {
            return;
        };
        if let Some(request) = winman_follow_request(&workspaces, &worktree) {
            if !send_to_daemon(request.clone()) {
                log::warn!("winman did not take {request}");
            }
        }
    })
    .detach();
}

#[derive(Default)]
struct Reports {
    last_focused_tab: Option<(String, Instant)>,
    last_blocked: Option<(String, Instant)>,
    last_tab_strips: Option<String>,
    /// The worktree last shown in the editor, per terminal column path.
    last_shown_editor: HashMap<PathBuf, PathBuf>,
}

impl Global for Reports {}

pub fn init(cx: &mut App) {
    cx.set_global(Reports::default());
    start_opener(cx);
    start_control_server(cx);
    start_mailbox(cx);

    cx.spawn(async move |cx| {
        loop {
            cx.background_executor()
                .timer(Duration::from_millis(1500))
                .await;
            cx.update(|cx| {
                report_focused_tab(cx);
                report_blocked_tabs(cx);
            });
        }
    })
    .detach();

    // Quitting: nothing is blocked any more and the tab strips are gone.
    cx.on_app_quit(|_| {
        crate::tab_sessions::freeze();
        let pid = std::process::id();
        let strips = tab_strips_dir().join(format!("{pid}.json"));
        let gui = gui_socket_path();
        async move {
            if let Ok(mut stream) = UnixStream::connect(&gui) {
                stream
                    .write_all(format!("blocked-tabs {pid}\n").as_bytes())
                    .ok();
            }
            std::fs::remove_file(strips).ok();
            if let Ok(mut stream) = UnixStream::connect(&gui) {
                stream.write_all(b"tab-strips-changed\n").ok();
            }
        }
    })
    .detach();
}

#[derive(Default)]
struct ReportedSide(Option<bool>);

impl Global for ReportedSide {}

/// `focus-side <terminal|editor>` to the daemon: the front app is always Zed
/// Dev now, so winman learns from this which half has the keyboard (its
/// virtual keys and `active_app` follow it).
pub fn report_side(terminal: bool, cx: &mut App) {
    let reported = cx.default_global::<ReportedSide>();
    if reported.0 == Some(terminal) {
        return;
    }
    reported.0 = Some(terminal);
    let request = format!(
        "focus-side {}",
        if terminal { "terminal" } else { "editor" }
    );
    cx.background_spawn(async move {
        send_to_daemon(request);
    })
    .detach();
}

/// Hooks a new column up to the reports that follow it.
pub fn watch_column(column: &Entity<TerminalColumn>, cx: &mut App) {
    cx.subscribe(column, |column, event, cx| match event {
        TerminalColumnEvent::WorktreeChosen(path) => {
            if let Some(own) = column.read(cx).workspace_path().cloned() {
                cx.default_global::<Reports>()
                    .last_shown_editor
                    .insert(own, path.clone());
            }
            show_editor(path.clone(), cx);
        }
        TerminalColumnEvent::TabsChanged => {
            publish_tab_strips(cx);
            report_focused_tab(cx);
            report_blocked_tabs(cx);
            follow_editor(cx);
        }
    })
    .detach();
}

/// The editor column follows the worktree the shown terminal's tab works in
/// (`WinmanEditorFollow.sync`).
pub fn follow_editor(cx: &mut App) {
    let Some(column) = TerminalColumns::current(cx) else {
        return;
    };
    let column = column.read(cx);
    let Some(own) = column.workspace_path().cloned() else {
        return;
    };
    let Some(target) = column
        .tabs()
        .get(column.selected_index())
        .and_then(|tab| tab.worktree_path.clone())
    else {
        return;
    };
    let reports = cx.default_global::<Reports>();
    if reports.last_shown_editor.get(&own) == Some(&target) {
        return;
    }
    reports.last_shown_editor.insert(own, target.clone());
    show_editor(target, cx);
}

/// Asks winman to show `path` in the editor column; without winman, shows it
/// directly.
fn show_editor(path: PathBuf, cx: &mut App) {
    let request = format!("show-editor {}", path.display());
    let task = cx.background_spawn(async move { send_to_daemon(request) });
    cx.spawn(async move |cx| {
        if !task.await {
            cx.update(|cx| show_editor_locally(&path, cx));
        }
    })
    .detach();
}

fn show_editor_locally(path: &Path, cx: &mut App) {
    let Some(window) = workspace::unified_window_handle(cx) else {
        return;
    };
    window
        .update(cx, |multi_workspace, window, cx| {
            let target = multi_workspace.workspaces().find(|workspace| {
                workspace
                    .read(cx)
                    .visible_worktrees(cx)
                    .any(|worktree| worktree.read(cx).abs_path().as_ref() == path)
            });
            if let Some(target) = target.cloned() {
                crate::show_workspace_keeping_terminal_focus(multi_workspace, target, window, cx);
            }
        })
        .ok();
}

/// The shown column's focused terminal, when the keyboard is in it.
fn focused_terminal(cx: &mut App) -> Option<Entity<GhosttyTerminal>> {
    let window = workspace::unified_window_handle(cx)?;
    let column = TerminalColumns::current(cx)?;
    window
        .update(cx, |_, window, cx| {
            if !window.is_window_active() {
                return None;
            }
            let column = column.read(cx);
            if !column.focus_handle_ref().contains_focused(window, cx) {
                return None;
            }
            column
                .tabs()
                .get(column.selected_index())?
                .focused_terminal()
        })
        .ok()
        .flatten()
}

/// `focused-tab <slug> <0|1> <pid>` (`WinmanClaudeReporter.refresh`).
fn report_focused_tab(cx: &mut App) {
    let Some(terminal) = focused_terminal(cx) else {
        return;
    };
    let terminal = terminal.read(cx);
    let pid = terminal.foreground_pid().unwrap_or(0);
    let running = pid > 0 && is_claude(pid as i32);
    let slug = slug_for_directory(terminal.reported_directory().map(PathBuf::as_path));
    let line = format!("focused-tab {slug} {} {pid}", running as u8);
    let reports = cx.default_global::<Reports>();
    if reports
        .last_focused_tab
        .as_ref()
        .is_some_and(|(last, at)| *last == line && at.elapsed() < HEARTBEAT)
    {
        return;
    }
    reports.last_focused_tab = Some((line.clone(), Instant::now()));
    send_line(gui_socket_path(), line, cx);
}

/// `blocked-tabs <pid> <slug>...`
fn report_blocked_tabs(cx: &mut App) {
    let mut slugs: Vec<String> = Vec::new();
    for column in TerminalColumns::all(cx) {
        for tab in column.read(cx).tabs().iter().filter(|tab| tab.blocked) {
            let directory = tab
                .focused_terminal()
                .and_then(|terminal| terminal.read(cx).reported_directory().cloned());
            let slug = slug_for_directory(directory.as_deref());
            if slug != "unknown" && !slugs.contains(&slug) {
                slugs.push(slug);
            }
        }
    }
    let mut line = format!("blocked-tabs {}", std::process::id());
    for slug in &slugs {
        line.push(' ');
        line.push_str(slug);
    }
    let reports = cx.default_global::<Reports>();
    if reports
        .last_blocked
        .as_ref()
        .is_some_and(|(last, at)| *last == line && at.elapsed() < HEARTBEAT)
    {
        return;
    }
    reports.last_blocked = Some((line.clone(), Instant::now()));
    send_line(gui_socket_path(), line, cx);
}

/// `busy`: a shell tab whose foreground process runs a command (`r`), not
/// one waiting at its prompt (`s`).
fn tab_token(state: ClaudeState, claude_present: bool, busy: bool, blocked: bool) -> String {
    let mut token = match state {
        ClaudeState::Working => "w",
        ClaudeState::Question => "q",
        ClaudeState::Background => "j",
        ClaudeState::Done => "d",
        ClaudeState::Absent if claude_present => "c",
        ClaudeState::Absent if busy => "r",
        ClaudeState::Absent => "s",
    }
    .to_string();
    if blocked {
        token.push('X');
    }
    token
}

/// `~/.config/winman/ghostty-tab-strips/<pid>.json`, the miniature tab rows
/// the winman bar draws, and a `tab-strips-changed` nudge when it changed.
pub fn publish_tab_strips(cx: &mut App) {
    let windows: Vec<serde_json::Value> = TerminalColumns::all(cx)
        .iter()
        .filter_map(|column| {
            let column = column.read(cx);
            let path = column
                .workspace_path()?
                .compact()
                .to_string_lossy()
                .into_owned();
            let tabs: Vec<String> = column
                .tabs()
                .iter()
                .map(|tab| tab_token(tab.claude_state, tab.claude_present, tab.busy, tab.blocked))
                .collect();
            let active = if column.tabs().is_empty() {
                -1
            } else {
                column.selected_index() as i64
            };
            Some(serde_json::json!({ "active": active, "path": path, "tabs": tabs }))
        })
        .collect();
    let pid = std::process::id();
    let document = serde_json::json!({ "pid": pid, "windows": windows });
    let Ok(json) = serde_json::to_string(&document) else {
        return;
    };
    let reports = cx.default_global::<Reports>();
    if reports.last_tab_strips.as_ref() == Some(&json) {
        return;
    }
    reports.last_tab_strips = Some(json.clone());
    let path = tab_strips_dir().join(format!("{pid}.json"));
    let gui = gui_socket_path();
    cx.background_spawn(async move {
        if write_atomically(&path, json.as_bytes()).is_ok()
            && let Ok(mut stream) = UnixStream::connect(&gui)
        {
            stream.write_all(b"tab-strips-changed\n").ok();
        }
    })
    .detach();
}

fn write_atomically(path: &Path, data: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Unique per write, so overlapping writes of one file cannot rename each
    // other's temporary away.
    static WRITES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let serial = WRITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let temporary = PathBuf::from(format!(
        "{}.{}.{serial}.tmp",
        path.display(),
        std::process::id()
    ));
    std::fs::write(&temporary, data)?;
    std::fs::rename(&temporary, path)
}

// ---------------------------------------------------------------------------
// The control socket
// ---------------------------------------------------------------------------

struct ControlRequest {
    line: String,
    reply: std_mpsc::Sender<String>,
}

fn start_control_server(cx: &mut App) {
    let path = control_socket_path();
    // Another live instance answers already: leave it the socket.
    if UnixStream::connect(&path).is_ok() {
        log::warn!(
            "{} is served by another process; not taking it",
            path.display()
        );
        return;
    }
    std::fs::remove_file(&path).ok();
    let listener = match UnixListener::bind(&path) {
        Ok(listener) => listener,
        Err(error) => {
            log::error!("could not bind {}: {error}", path.display());
            return;
        }
    };
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).ok();
    }

    let (requests_tx, mut requests_rx) = mpsc::unbounded::<ControlRequest>();
    let spawned = std::thread::Builder::new()
        .name("winman-control".into())
        .spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else {
                    continue;
                };
                let mut buffer = vec![0u8; 8192];
                let Ok(length) = stream.read(&mut buffer) else {
                    continue;
                };
                let line = String::from_utf8_lossy(&buffer[..length])
                    .trim()
                    .to_string();
                let (reply_tx, reply_rx) = std_mpsc::channel();
                if requests_tx
                    .unbounded_send(ControlRequest {
                        line,
                        reply: reply_tx,
                    })
                    .is_err()
                {
                    break;
                }
                let reply = reply_rx
                    .recv_timeout(Duration::from_secs(10))
                    .unwrap_or_else(|_| "error timeout".into());
                stream.write_all(format!("{reply}\n").as_bytes()).ok();
            }
        });
    if let Err(error) = spawned {
        log::error!("could not start the winman control thread: {error}");
        return;
    }

    cx.spawn(async move |cx| {
        while let Some(request) = requests_rx.next().await {
            let reply = handle_control(&request.line, cx).await;
            request.reply.send(reply).ok();
        }
    })
    .detach();
}

async fn handle_control(line: &str, cx: &mut AsyncApp) -> String {
    let fields: Vec<&str> = line.split('\t').collect();
    let verb = fields.first().copied().unwrap_or_default();
    let argument = |index: usize| fields.get(index).copied().filter(|value| !value.is_empty());
    match verb {
        "ping" => format!("pong\t{}", std::process::id()),
        // The half last reported with `focus-side`. That report is sent on a change
        // only, so a restarted daemon asks here instead of waiting for the next one.
        "focus-side" => cx.update(|cx| {
            match cx
                .try_global::<ReportedSide>()
                .and_then(|reported| reported.0)
            {
                Some(true) => "side\tterminal",
                Some(false) => "side\teditor",
                None => "side\tunknown",
            }
            .to_string()
        }),
        // Ghostty's reload_config, for a config edit made outside the app.
        "reload-config" => cx.update(|_| {
            crate::runtime::reload_config();
            "reloaded".to_string()
        }),
        "list-windows" => cx.update(|cx| {
            let mut reply = "windows".to_string();
            for column in TerminalColumns::all(cx) {
                if let Some(path) = column.read(cx).workspace_path() {
                    reply.push('\t');
                    reply.push_str(&path.compact().to_string_lossy());
                }
            }
            reply
        }),
        "new-window" => {
            let (Some(directory), Some(title)) = (argument(1), argument(2)) else {
                return "error missing-args".into();
            };
            new_window(directory, title, cx)
        }
        // `open-worktrees <dir> <title> [<dir> <title> ...]`: every worktree
        // winman wants, in one message; the ones already open are left as they
        // are. Replies `opened <created> <existing>`.
        "open-worktrees" => {
            let pairs: Vec<(PathBuf, PathBuf)> = fields[1..]
                .chunks_exact(2)
                .filter(|pair| !pair[0].is_empty() && !pair[1].is_empty())
                .map(|pair| (normalize(pair[0]), normalize(pair[1])))
                .collect();
            cx.update(|cx| {
                let mut created = 0;
                let mut existing = 0;
                for (directory, title) in pairs {
                    match open_worktree(directory, title, cx) {
                        "created" => created += 1,
                        "exists" => existing += 1,
                        error => return error.to_string(),
                    }
                }
                format!("opened\t{created}\t{existing}")
            })
        }
        "focus-window" | "raise-window" | "raise-window-soon" => {
            let Some(title) = argument(1) else {
                return "error missing-args".into();
            };
            let focus = verb == "focus-window";
            cx.update(|cx| {
                let Some(column) = TerminalColumns::column_for_path(&normalize(title), cx) else {
                    return "no-window".into();
                };
                show_terminal(&column, focus, cx);
                if focus { "focused" } else { "raised" }.into()
            })
        }
        "close-window" => {
            let Some(title) = argument(1) else {
                return "error missing-args".into();
            };
            cx.update(|cx| {
                if close_worktrees(|path| path == normalize(title), cx) > 0 {
                    "closed".into()
                } else {
                    "no-window".into()
                }
            })
        }
        "close-unmapped" => {
            let keep: Vec<PathBuf> = fields[1..]
                .iter()
                .filter(|title| !title.is_empty())
                .map(|title| normalize(title))
                .collect();
            if keep.is_empty() {
                return "error no-valid-titles".into();
            }
            cx.update(|cx| {
                format!(
                    "closed\t{}",
                    close_worktrees(|path| !keep.contains(&path), cx)
                )
            })
        }
        // winman places the one window itself; the terminal column's frame
        // follows from the layout.
        "set-frame" => "framed".into(),
        "focus-claude" => {
            let Some(pid) = argument(1).and_then(|pid| pid.parse::<i32>().ok()) else {
                return "error missing-args".into();
            };
            cx.update(|cx| match find_claude(pid, cx) {
                Some((column, tab_id, terminal)) => {
                    show_terminal(&column, true, cx);
                    focus_tab_terminal(&column, tab_id, &terminal, cx);
                    "focused".into()
                }
                None => "no-tab".into(),
            })
        }
        "type-claude" => {
            let Some(pid) = argument(1).and_then(|pid| pid.parse::<i32>().ok()) else {
                return "error missing-args".into();
            };
            let text = fields
                .get(2..)
                .map(|rest| rest.join("\t"))
                .unwrap_or_default();
            let terminal = cx.update(|cx| find_claude(pid, cx).map(|(_, _, terminal)| terminal));
            let Some(terminal) = terminal else {
                return "no-tab".into();
            };
            cx.update(|cx| terminal.read(cx).input_text(&text));
            cx.background_executor()
                .timer(Duration::from_millis(80))
                .await;
            cx.update(|cx| {
                terminal
                    .read(cx)
                    .press_key(KEY_CODE_RETURN, ffi::GHOSTTY_MODS_NONE)
            });
            "typed".into()
        }
        "pick-worktree" => {
            let Some(title) = argument(1) else {
                return "error missing-args".into();
            };
            cx.update(|cx| pick_worktree(&normalize(title), cx))
        }
        // winman's tab hints (hold 3): the window frame in top-left screen
        // coordinates, the tab row's centre and each tab's left edge, relative
        // to the window, like the AX read of the Ghostty window gave it.
        "tab-hints" => {
            let Some(title) = argument(1) else {
                return "error missing-args".into();
            };
            cx.update(|cx| tab_hints(&normalize(title), cx))
        }
        "select-tab" => {
            let (Some(title), Some(index)) = (
                argument(1),
                argument(2).and_then(|index| index.parse::<usize>().ok()),
            ) else {
                return "error missing-args".into();
            };
            cx.update(|cx| {
                let Some(column) = TerminalColumns::column_for_path(&normalize(title), cx) else {
                    return "no-window".into();
                };
                let Some(tab_id) = column.read(cx).tabs().get(index).map(|tab| tab.id()) else {
                    return "no-tab".into();
                };
                show_terminal(&column, true, cx);
                let Some(terminal) = column
                    .read(cx)
                    .tabs()
                    .get(index)
                    .and_then(|tab| tab.focused_terminal())
                else {
                    return "no-tab".into();
                };
                focus_tab_terminal(&column, tab_id, &terminal, cx);
                "selected".into()
            })
        }
        // `new-tab <title> <dir> <input>`: a tab in that worktree's column,
        // started in <dir> with <input> typed into its shell, neither selected
        // nor focused. Replies `tab <id>` for `focus-tab`.
        "new-tab" => {
            let (Some(title), Some(directory), Some(_)) = (argument(1), argument(2), argument(3))
            else {
                return "error missing-args".into();
            };
            let input = fields[3..].join("\t");
            let input = input.trim_end();
            // Typed ahead line by line, a newline inside a quoted argument ends
            // the line for line editors that evaluate each line themselves
            // (ble.sh runs `'a<NL>b'` as two commands, both unterminated).
            // Bracketed paste is how a terminal hands over multi-line text as
            // one edit, so send it the way a paste would arrive.
            let input = if input.contains('\n') {
                format!("\x1b[200~{input}\x1b[201~\n")
            } else {
                format!("{input}\n")
            };
            cx.update(|cx| {
                let Some(column) = TerminalColumns::column_for_path(&normalize(title), cx) else {
                    return "no-window".into();
                };
                let Some(window) = workspace::unified_window_handle(cx) else {
                    return "no-window".into();
                };
                let options = TerminalOptions {
                    working_directory: Some(normalize(directory)),
                    initial_input: Some(input),
                    ..Default::default()
                };
                window
                    .update(cx, |_, window, cx| {
                        column.update(cx, |column, cx| {
                            column.new_background_tab(options, window, cx)
                        })
                    })
                    .ok()
                    .flatten()
                    .map(|id| format!("tab\t{id}"))
                    .unwrap_or_else(|| "error open-failed".into())
            })
        }
        // `focus-tab <title> <id | claude-session>`: show that column and give
        // the tab the keyboard. A tab id only holds while the app runs (it
        // counts from 0 again after a restart), so a caller that keeps it on
        // disk passes the Claude session id instead, which the restored tab
        // carries over.
        "focus-tab" => {
            let (Some(title), Some(target)) = (argument(1), argument(2)) else {
                return "error missing-args".into();
            };
            let tab_id = target.parse::<u64>().ok();
            cx.update(|cx| {
                let Some(column) = TerminalColumns::column_for_path(&normalize(title), cx) else {
                    return "no-window".into();
                };
                let Some((tab_id, terminal)) = column
                    .read(cx)
                    .tabs()
                    .iter()
                    .find(|tab| match tab_id {
                        Some(id) => tab.id() == id,
                        None => tab.claude_session.as_deref() == Some(target),
                    })
                    .and_then(|tab| Some((tab.id(), tab.focused_terminal()?)))
                else {
                    return "no-tab".into();
                };
                show_terminal(&column, true, cx);
                focus_tab_terminal(&column, tab_id, &terminal, cx);
                follow_in_winman(&column, cx);
                "focused".into()
            })
        }
        // `list-tabs [<scope>]`: every tab in the scope's columns as one JSON
        // array, for a caller that picks tabs by what they are about (voice).
        "list-tabs" => {
            let scope = argument(1).unwrap_or("-").to_string();
            cx.update(|cx| {
                let tabs: Vec<serde_json::Value> = columns_in_scope(&scope, cx)
                    .iter()
                    .flat_map(|column| describe_tabs(column, cx))
                    .collect();
                serde_json::to_string(&tabs).unwrap_or_else(|_| "[]".into())
            })
        }
        // `close-tabs <scope> <selector> [force]`: <selector> is a tab id, a
        // Claude session id, `shell` (every tab without Claude) or `claude`.
        // Without `force` a tab with a running process (a Claude, a build) is
        // left open and counted as busy. Replies `closed <n> busy <m>`.
        "close-tabs" => {
            let (Some(scope), Some(selector)) = (argument(1), argument(2)) else {
                return "error missing-args".into();
            };
            let force = argument(3) == Some("force");
            cx.update(|cx| close_tabs(scope, selector, force, cx))
        }
        "close-worktree-picker" => cx.update(|cx| {
            let Some(window) = workspace::unified_window_handle(cx) else {
                return "none".into();
            };
            let closed = TerminalColumns::all(cx).into_iter().any(|column| {
                window
                    .update(cx, |_, window, cx| {
                        column.update(cx, |column, cx| column.close_worktree_picker(window, cx))
                    })
                    .unwrap_or(false)
            });
            if closed { "closed" } else { "none" }.into()
        }),
        // `prompt`: the newest open prompt as JSON `{"message", "actions"}`, or
        // `none`. `prompt-answer <label>`: press its button labelled <label>
        // (any case), for answering it by voice. Replies `answered <label>`,
        // `no-match` or `none`.
        "prompt" => cx.update(|cx| match ui_prompt::open_prompt(cx) {
            Some((message, actions)) => {
                serde_json::json!({ "message": message, "actions": actions }).to_string()
            }
            None => "none".into(),
        }),
        "prompt-answer" => {
            let Some(label) = argument(1).map(str::to_string) else {
                return "error missing-args".into();
            };
            cx.update(|cx| {
                if ui_prompt::open_prompt(cx).is_none() {
                    return "none".into();
                }
                match ui_prompt::answer_open_prompt(&label, cx) {
                    Some(pressed) => format!("answered {pressed}"),
                    None => "no-match".into(),
                }
            })
        }
        "quit" => {
            cx.update(|cx| cx.defer(|cx| cx.quit()));
            "quitting".into()
        }
        "debug-order-out-others" | "debug-order-in-all" => "ordered 0".into(),
        _ => "error unknown-command".into(),
    }
}

/// Worktrees `new-window` asked to open, until their workspace shows up.
static PENDING_OPENS: LazyLock<Mutex<Vec<(PathBuf, Instant)>>> = LazyLock::new(Default::default);

/// Whether a workspace for `path` exists already or is on its way: winman opens
/// every worktree at once at startup, while Zed restores its own session.
fn worktree_open_or_opening(path: &Path, cx: &App) -> bool {
    if TerminalColumns::column_for_path(path, cx).is_some() {
        forget_pending_opens(|pending_path| pending_path == path);
        return true;
    }
    let open = workspace::unified_window_handle(cx)
        .and_then(|window| {
            window
                .read_with(cx, |multi_workspace, cx| {
                    multi_workspace.workspaces().any(|workspace| {
                        workspace
                            .read(cx)
                            .project()
                            .read(cx)
                            .visible_worktrees(cx)
                            .any(|worktree| worktree.read(cx).abs_path().as_ref() == path)
                    })
                })
                .ok()
        })
        .unwrap_or(false);
    if open {
        forget_pending_opens(|pending_path| pending_path == path);
        return true;
    }
    let mut pending = PENDING_OPENS.lock();
    pending.retain(|(_, since)| since.elapsed() < Duration::from_secs(60));
    pending.iter().any(|(pending_path, _)| pending_path == path)
}

/// A pending open has to end with its workspace: left to the 60 s expiry, a
/// close followed by a reopen within that minute was answered `exists` and
/// never opened anything.
fn forget_pending_opens(matches: impl Fn(&Path) -> bool) {
    PENDING_OPENS
        .lock()
        .retain(|(pending_path, _)| !matches(pending_path));
}

/// Whether winman decides which workspaces exist. Then Zed does not restore
/// its own last session at startup: winman opens its worktrees as soon as the
/// app answers, and a restore of the same worktrees running alongside opened
/// one of them twice.
pub fn owns_workspaces(cx: &App) -> bool {
    workspace::unified_window_enabled(cx) && UnixStream::connect(daemon_socket_path()).is_ok()
}

/// Worktrees to open, one at a time. The first open creates the one window and
/// the rest join it; opened side by side, each found no window yet and made one.
struct Opener(mpsc::UnboundedSender<PathBuf>);

impl Global for Opener {}

fn start_opener(cx: &mut App) {
    let (sender, mut receiver) = mpsc::unbounded::<PathBuf>();
    cx.set_global(Opener(sender));
    cx.spawn(async move |cx| {
        while let Some(directory) = receiver.next().await {
            let first = cx.update(|cx| {
                let Some(app_state) = workspace::AppState::try_global(cx) else {
                    log::error!("could not open a worktree: no app state");
                    return None;
                };
                let has_window = workspace::unified_window_handle(cx).is_some();
                let task = workspace::open_paths(
                    &[directory],
                    app_state,
                    workspace::OpenOptions {
                        open_mode: workspace::OpenMode::Add,
                        // One workspace per worktree, like one Ghostty window
                        // per worktree: not a folder opened inside a workspace
                        // whose root contains it (e.g. an open `~/dev/aixia-projects`).
                        workspace_matching: workspace::WorkspaceMatching::None,
                        ..Default::default()
                    },
                    cx,
                );
                if has_window {
                    task.detach_and_log_err(cx);
                    None
                } else {
                    Some(task)
                }
            });
            if let Some(task) = first
                && let Err(error) = task.await
            {
                log::error!("could not open the first worktree: {error:#}");
            }
        }
    })
    .detach();
}

/// Queues `directory` unless its workspace exists or is on its way.
fn open_worktree(directory: PathBuf, title: PathBuf, cx: &mut App) -> &'static str {
    if worktree_open_or_opening(&title, cx) || worktree_open_or_opening(&directory, cx) {
        return "exists";
    }
    let Some(opener) = cx.try_global::<Opener>() else {
        return "error unavailable";
    };
    PENDING_OPENS
        .lock()
        .push((directory.clone(), Instant::now()));
    if opener.0.unbounded_send(directory).is_err() {
        return "error unavailable";
    }
    "created"
}

/// The fork's `new-window`: idempotent; opens the worktree's workspace in the
/// background (the editor side is winman's to show).
fn new_window(directory: &str, title: &str, cx: &mut AsyncApp) -> String {
    let path = normalize(title);
    let directory = normalize(directory);
    cx.update(|cx| open_worktree(directory, path, cx).into())
}

/// Gives the keyboard to the tab running Claude session `session` in
/// `worktree`'s column. Returns false when that column or tab is not there.
fn focus_session_tab(worktree: &Path, session: &str, cx: &mut App) -> bool {
    let Some(column) = TerminalColumns::column_for_path(worktree, cx) else {
        return false;
    };
    let Some((tab_id, terminal)) = column
        .read(cx)
        .tabs()
        .iter()
        .find(|tab| tab.claude_session.as_deref() == Some(session))
        .and_then(|tab| Some((tab.id(), tab.focused_terminal()?)))
    else {
        return false;
    };
    show_terminal(&column, true, cx);
    focus_tab_terminal(&column, tab_id, &terminal, cx);
    follow_in_winman(&column, cx);
    true
}

/// The session band's fork button: back to the parent session's tab. Its
/// workspace may have been closed since the fork; then winman opens it, and
/// the tab (restored with its session) is focused once its column is up.
pub(crate) fn focus_session(worktree: PathBuf, root: PathBuf, session: String, cx: &mut App) {
    if focus_session_tab(&worktree, &session, cx) {
        return;
    }
    send_to_daemon(format!("workspace-open {}", root.display()));
    cx.spawn(async move |cx| {
        for _ in 0..40 {
            cx.background_executor()
                .timer(Duration::from_millis(500))
                .await;
            if cx.update(|cx| focus_session_tab(&worktree, &session, cx)) {
                return;
            }
        }
        log::warn!(
            "fork parent {session} did not show up in {}",
            worktree.display()
        );
    })
    .detach();
}

/// Shows `column` in the window; with `focus`, also gives it the keyboard
/// and makes the app active.
fn show_terminal(column: &Entity<TerminalColumn>, focus: bool, cx: &mut App) {
    TerminalColumns::set_current(column, cx);
    if !focus {
        return;
    }
    let Some(window) = workspace::unified_window_handle(cx) else {
        return;
    };
    window
        .update(cx, |multi_workspace, window, cx| {
            window.activate_window();
            multi_workspace.workspace().update(cx, |workspace, cx| {
                crate::focus_terminal(workspace, window, cx);
            });
        })
        .ok();
    cx.activate(true);
}

fn focus_tab_terminal(
    column: &Entity<TerminalColumn>,
    tab_id: u64,
    terminal: &Entity<GhosttyTerminal>,
    cx: &mut App,
) {
    let Some(window) = workspace::unified_window_handle(cx) else {
        return;
    };
    window
        .update(cx, |_, window, cx| {
            column.update(cx, |column, cx| {
                column.focus_terminal_in_tab(tab_id, terminal, window, cx);
            });
        })
        .ok();
}

/// The tab whose split runs `pid`, or one of its descendants (winman knows
/// the Claude pid; the terminal's foreground may be a child of it).
fn find_claude(
    pid: i32,
    cx: &App,
) -> Option<(Entity<TerminalColumn>, u64, Entity<GhosttyTerminal>)> {
    for column in TerminalColumns::all(cx) {
        for tab in column.read(cx).tabs() {
            for terminal in tab.terminals() {
                let Some(foreground) = terminal.read(cx).foreground_pid() else {
                    continue;
                };
                if descends_from(foreground as i32, pid) {
                    return Some((column.clone(), tab.id(), terminal));
                }
            }
        }
    }
    None
}

pub(crate) fn parent_pid(pid: i32) -> Option<i32> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            size,
        )
    };
    (written == size).then_some(info.pbi_ppid as i32)
}

fn descends_from(mut pid: i32, ancestor: i32) -> bool {
    for _ in 0..=8 {
        if pid == ancestor {
            return true;
        }
        match parent_pid(pid) {
            Some(parent) if parent > 1 => pid = parent,
            _ => return false,
        }
    }
    false
}

fn tab_hints(path: &Path, cx: &mut App) -> String {
    let Some(column) = TerminalColumns::column_for_path(path, cx) else {
        return "no-window".into();
    };
    let Some(window) = workspace::unified_window_handle(cx) else {
        return "no-window".into();
    };
    let frame = window
        .update(cx, |_, window, _| crate::native_window_frame(window))
        .ok()
        .flatten();
    let Some((x, y, width, height)) = frame else {
        return "no-window".into();
    };
    // AppKit's origin is the bottom-left of the primary screen; winman's is its
    // top-left.
    let top = crate::primary_screen_height().map_or(y, |screen| screen - (y + height));
    let (row_center, xs) = column.read(cx).tab_positions();
    let mut reply = format!("tabs\t{x}\t{top}\t{width}\t{height}\t{row_center}");
    for tab_x in xs {
        reply.push_str(&format!("\t{tab_x}"));
    }
    reply
}

fn pick_worktree(path: &Path, cx: &mut App) -> String {
    let Some(column) = TerminalColumns::column_for_path(path, cx) else {
        return "no-window".into();
    };
    let Some(window) = workspace::unified_window_handle(cx) else {
        return "no-window".into();
    };
    let result = window
        .update(cx, |_, window, cx| {
            column.update(cx, |column, cx| {
                let tab = column
                    .tabs()
                    .get(column.selected_index())
                    .map(|tab| tab.id())?;
                Some(column.pick_worktree(tab, true, window, cx))
            })
        })
        .ok()
        .flatten();
    match result {
        Some(PickWorktree::Opened) => "picking",
        Some(PickWorktree::Stepped) => "stepping",
        Some(PickWorktree::ClaudeTab) => "claude-tab",
        Some(PickWorktree::NoWorktrees) => "no-worktrees",
        Some(PickWorktree::Unavailable) | None => "no-window",
    }
    .into()
}

/// The columns a tab verb acts on: `-` is the column on screen, `*` every
/// column, and a path the column of that worktree or every column under that
/// project root (a winman workspace).
fn columns_in_scope(scope: &str, cx: &App) -> Vec<Entity<TerminalColumn>> {
    match scope {
        "-" => TerminalColumns::current(cx).into_iter().collect(),
        "*" => TerminalColumns::all(cx),
        path => {
            let root = normalize(path);
            // `~/dev/aixia-projects` is a workspace of its own as well as the
            // parent of others: an exact match means only that column.
            if let Some(column) = TerminalColumns::column_for_path(&root, cx) {
                return vec![column];
            }
            TerminalColumns::all(cx)
                .into_iter()
                .filter(|column| {
                    column
                        .read(cx)
                        .workspace_path()
                        .is_some_and(|own| own.starts_with(&root))
                })
                .collect()
        }
    }
}

fn claude_state_name(state: ClaudeState) -> &'static str {
    match state {
        ClaudeState::Absent => "absent",
        ClaudeState::Working => "working",
        ClaudeState::Question => "question",
        ClaudeState::Done => "done",
        ClaudeState::Background => "background",
    }
}

fn describe_tabs(column: &Entity<TerminalColumn>, cx: &App) -> Vec<serde_json::Value> {
    let current = TerminalColumns::current(cx).is_some_and(|current| &current == column);
    let column = column.read(cx);
    let window = column
        .workspace_path()
        .map(|path| path.compact().to_string_lossy().into_owned());
    column
        .tabs()
        .iter()
        .enumerate()
        .map(|(index, tab)| {
            let terminal = tab.focused_terminal();
            let terminal = terminal.as_ref().map(|terminal| terminal.read(cx));
            let busy = tab
                .terminals()
                .iter()
                .any(|terminal| terminal.read(cx).needs_confirm_quit());
            let info = tab.session_info.as_ref();
            serde_json::json!({
                "window": window,
                "index": index,
                "id": tab.id(),
                "selected": index == column.selected_index(),
                "on_screen": current,
                "kind": if tab.claude_present { "claude" } else { "shell" },
                "busy": busy,
                "state": claude_state_name(tab.claude_state),
                "session": tab.claude_session,
                "claude_title": tab.claude_title.as_ref().map(|title| title.to_string()),
                // Same choice as the session band: a session's own name over
                // winman's topic, which a fork shares with its parent.
                "topic": tab
                    .claude_title
                    .as_ref()
                    .filter(|_| tab.claude_title_custom)
                    .or(info.and_then(|info| info.topic.as_ref()))
                    .map(|topic| topic.to_string()),
                "fork_of": tab
                    .fork_origin
                    .as_ref()
                    .and_then(|(_, origin)| origin.as_ref())
                    .map(|origin| origin.parent_session.clone()),
                "now": info.and_then(|info| info.now.as_ref()).map(|now| now.to_string()),
                "title": terminal.map(|terminal| terminal.title().to_string()),
                "cwd": terminal
                    .and_then(|terminal| terminal.reported_directory())
                    .map(|path| path.compact().to_string_lossy().into_owned()),
                "worktree": tab.worktree,
                "pid": terminal.and_then(|terminal| terminal.foreground_pid()),
            })
        })
        .collect()
}

fn close_tabs(scope: &str, selector: &str, force: bool, cx: &mut App) -> String {
    let Some(window) = workspace::unified_window_handle(cx) else {
        return "no-window".into();
    };
    let tab_id = selector.parse::<u64>().ok();
    let mut closed = 0;
    let mut busy = 0;
    for column in columns_in_scope(scope, cx) {
        let targets: Vec<(u64, bool)> = column
            .read(cx)
            .tabs()
            .iter()
            .filter(|tab| match (selector, tab_id) {
                (_, Some(id)) => tab.id() == id,
                ("shell", None) => !tab.claude_present,
                ("claude", None) => tab.claude_present,
                (session, None) => tab.claude_session.as_deref() == Some(session),
            })
            .map(|tab| {
                let running = tab
                    .terminals()
                    .iter()
                    .any(|terminal| terminal.read(cx).needs_confirm_quit());
                (tab.id(), tab.claude_present || running)
            })
            .collect();
        for (tab_id, running) in targets {
            if running && !force {
                busy += 1;
                continue;
            }
            window
                .update(cx, |_, window, cx| {
                    column.update(cx, |column, cx| {
                        if let Some(index) = column.tabs().iter().position(|tab| tab.id() == tab_id)
                        {
                            column.close_tab(index, false, window, cx);
                        }
                    })
                })
                .ok();
            closed += 1;
        }
    }
    format!("closed\t{closed}\tbusy\t{busy}")
}

/// Closes the workspaces whose worktree path matches, without asking.
fn close_worktrees(matches: impl Fn(PathBuf) -> bool, cx: &mut App) -> usize {
    let Some(window) = workspace::unified_window_handle(cx) else {
        return 0;
    };
    window
        .update(cx, |multi_workspace, window, cx| {
            let doomed: Vec<_> = multi_workspace
                .workspaces()
                .filter(|workspace| {
                    workspace
                        .read(cx)
                        .visible_worktrees(cx)
                        .next()
                        .is_some_and(|worktree| matches(worktree.read(cx).abs_path().to_path_buf()))
                })
                .cloned()
                .collect();
            forget_pending_opens(|path| matches(path.to_path_buf()));
            for workspace in &doomed {
                multi_workspace
                    .close_workspace(workspace, window, cx)
                    .detach_and_log_err(cx);
            }
            doomed.len()
        })
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// The mailbox
// ---------------------------------------------------------------------------

enum Mail {
    Text(String),
    Key(u32),
    ReadViewport,
    ReadScrollback(usize),
}

static LAST_SEQ: LazyLock<Mutex<i64>> = LazyLock::new(|| Mutex::new(0));

fn read_mail() -> Option<(i64, Mail)> {
    let data = std::fs::read(mailbox_path()).ok()?;
    let root: serde_json::Value = serde_json::from_slice(&data).ok()?;
    let seq = root.get("seq")?.as_i64()?;
    if let Some(target) = root.get("read").and_then(|value| value.as_str()) {
        return match target {
            "viewport" => Some((seq, Mail::ReadViewport)),
            "scrollback" => {
                let asked = root
                    .get("maxLines")
                    .and_then(|value| value.as_u64())
                    .unwrap_or(0);
                Some((
                    seq,
                    Mail::ReadScrollback((asked as usize).clamp(1, MAX_SCROLLBACK_LINES)),
                ))
            }
            _ => None,
        };
    }
    if let Some(name) = root.get("key").and_then(|value| value.as_str()) {
        let key_code = match name {
            "enter" => KEY_CODE_RETURN,
            "escape" => KEY_CODE_ESCAPE,
            "tab" => KEY_CODE_TAB,
            _ => return None,
        };
        return Some((seq, Mail::Key(key_code)));
    }
    let text = root.get("text")?.as_str()?;
    (!text.is_empty()).then(|| (seq, Mail::Text(text.to_string())))
}

fn start_mailbox(cx: &mut App) {
    let path = mailbox_path();
    *LAST_SEQ.lock() = read_mail().map(|(seq, _)| seq).unwrap_or(0);
    // Created empty so winman-gui has a file to replace.
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .ok();
    let (changes_tx, mut changes_rx) = mpsc::unbounded::<()>();
    let spawned = std::thread::Builder::new()
        .name("winman-mailbox".into())
        .spawn(move || watch_file(&path, changes_tx));
    if let Err(error) = spawned {
        log::error!("could not start the winman mailbox watcher: {error}");
        return;
    }
    cx.spawn(async move |cx| {
        while changes_rx.next().await.is_some() {
            cx.update(|cx| handle_mail(cx));
        }
    })
    .detach();
}

/// Reports every write to `path` through kqueue, like the fork's dispatch
/// source: immediate, where FSEvents would batch. A replaced file (winman-gui
/// writes atomically) is opened again after 50 ms; a missing one every 2 s.
fn watch_file(path: &Path, changes: mpsc::UnboundedSender<()>) {
    let Ok(path_c) = std::ffi::CString::new(path.to_string_lossy().as_bytes()) else {
        return;
    };
    loop {
        let fd = unsafe { libc::open(path_c.as_ptr(), libc::O_EVTONLY) };
        if fd < 0 {
            std::thread::sleep(Duration::from_secs(2));
            continue;
        }
        let queue = unsafe { libc::kqueue() };
        if queue < 0 {
            unsafe { libc::close(fd) };
            return;
        }
        let change = libc::kevent {
            ident: fd as usize,
            filter: libc::EVFILT_VNODE,
            flags: libc::EV_ADD | libc::EV_CLEAR,
            fflags: libc::NOTE_WRITE
                | libc::NOTE_EXTEND
                | libc::NOTE_DELETE
                | libc::NOTE_RENAME
                | libc::NOTE_REVOKE,
            data: 0,
            udata: std::ptr::null_mut(),
        };
        let mut replaced = false;
        loop {
            let mut event: libc::kevent = unsafe { std::mem::zeroed() };
            let count = unsafe { libc::kevent(queue, &change, 1, &mut event, 1, std::ptr::null()) };
            if count < 0 {
                break;
            }
            if count == 0 {
                continue;
            }
            if changes.unbounded_send(()).is_err() {
                unsafe {
                    libc::close(queue);
                    libc::close(fd);
                }
                return;
            }
            if event.fflags & (libc::NOTE_DELETE | libc::NOTE_RENAME | libc::NOTE_REVOKE) != 0 {
                replaced = true;
                break;
            }
        }
        unsafe {
            libc::close(queue);
            libc::close(fd);
        }
        if replaced {
            std::thread::sleep(Duration::from_millis(50));
            // The new file may already hold a message.
            if changes.unbounded_send(()).is_err() {
                return;
            }
        }
    }
}

/// The terminal winman means: the focused split of the terminal column of the
/// worktree winman has active, else the one the window shows.
fn mail_target(cx: &App) -> Option<Entity<GhosttyTerminal>> {
    let column = if test_dir().is_none() {
        workspace::read_winman_active_path()
            .and_then(|path| TerminalColumns::column_for_path(&normalize(&path), cx))
            .or_else(|| TerminalColumns::current(cx))
    } else {
        TerminalColumns::current(cx)
    }?;
    let column = column.read(cx);
    column
        .tabs()
        .get(column.selected_index())?
        .focused_terminal()
}

fn handle_mail(cx: &mut App) {
    let Some((seq, mail)) = read_mail() else {
        return;
    };
    {
        let mut last = LAST_SEQ.lock();
        if seq <= *last || (now_ms() - seq).abs() >= MAILBOX_MAX_AGE_MS {
            return;
        }
        *last = seq;
    }
    let Some(terminal) = mail_target(cx) else {
        return;
    };
    let response = match mail {
        Mail::Text(text) => {
            terminal.read(cx).input_text(&text);
            None
        }
        Mail::Key(key_code) => {
            terminal
                .read(cx)
                .press_key(key_code, ffi::GHOSTTY_MODS_NONE);
            None
        }
        Mail::ReadViewport => match viewport(&terminal, seq, cx) {
            Some(payload) => Some(payload),
            None => return,
        },
        Mail::ReadScrollback(max_lines) => match terminal.read(cx).scrollback(max_lines) {
            Some((text, truncated)) => Some(serde_json::json!({
                "seq": seq, "answeredAt": now_ms(), "text": text, "truncated": truncated,
            })),
            None => return,
        },
    };
    cx.background_spawn(async move {
        if let Some(response) = response
            && let Ok(data) = serde_json::to_vec(&response)
        {
            write_atomically(&read_response_path(), &data).ok();
        }
        write_atomically(&ack_path(), format!("{seq}\n").as_bytes()).ok();
    })
    .detach();
}

/// The fork's `WinmanSurfaceReader.viewport`: every row of the screen with its
/// window position, so winman can draw hints over words.
fn viewport(
    terminal: &Entity<GhosttyTerminal>,
    seq: i64,
    cx: &mut App,
) -> Option<serde_json::Value> {
    let window = workspace::unified_window_handle(cx)?;
    let frame = window
        .update(cx, |_, window, _| crate::native_window_frame(window))
        .ok()
        .flatten()?;
    let terminal = terminal.read(cx);
    let bounds = terminal.bounds()?;
    let (rows, cell_width, cell_height, columns) = terminal.grid()?;
    let view_left = f64::from(bounds.origin.x);
    let view_top = f64::from(bounds.origin.y);
    let mut lines = Vec::new();
    let mut column0 = None;
    for row in 0..rows {
        let Some((text, x, y)) = terminal.read_viewport_row(row, columns) else {
            continue;
        };
        column0.get_or_insert(view_left + x);
        lines.push(serde_json::json!({
            "y": view_top + y - cell_height / 2.,
            "text": text,
            "wrapped": terminal.is_soft_wrapped(row, rows, columns),
        }));
    }
    let x0 = column0?;
    Some(serde_json::json!({
        "seq": seq,
        "answeredAt": now_ms(),
        "win": { "x": frame.0, "y": frame.1, "w": frame.2, "h": frame.3 },
        "x0": x0,
        "cellW": cell_width,
        "cellH": cell_height,
        "cols": columns,
        "lines": lines,
    }))
}
