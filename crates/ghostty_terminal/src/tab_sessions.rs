//! Terminal tabs survive a restart, the fork's `ClaudeTabSessions`: each
//! workspace's tabs (directory, Claude session, blocked flag) are kept in
//! `~/.config/ghostty/tab-sessions/<slug>.json`, the same files the Ghostty
//! app wrote, and a tab that ran Claude comes back resuming its session.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::LazyLock,
};

use gpui::{AppContext as _, Context};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::TerminalColumn;

const RESUME_FLAGS: &str = "--dangerously-skip-permissions";

// Fields in alphabetical order: the Ghostty app wrote them with sorted keys.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SavedTab {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked: Option<bool>,
    #[serde(
        rename = "blockedNote",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub blocked_note: Option<String>,
    pub cwd: String,
    /// The remotework tmux session this tab was attached to (the Claude session
    /// runs on machinehead, not here). Restores as an attach, never a local resume:
    /// resuming locally would run the same session in two places.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

impl SavedTab {
    /// What to type into the new tab's shell to pick the session back up.
    pub fn initial_input(&self) -> Option<String> {
        if let Some(name) = self.remote.as_ref().filter(|name| !name.is_empty()) {
            return Some(format!("{}\n", crate::remote_session::attach_command(name)));
        }
        self.session
            .as_ref()
            .filter(|session| !session.is_empty())
            .map(|session| format!("claude --resume {session} {RESUME_FLAGS}\n"))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snapshot {
    pub selected: usize,
    pub tabs: Vec<SavedTab>,
    pub updated: f64,
    pub workspace: String,
}

#[derive(Default)]
struct State {
    restored: HashSet<String>,
    last_written: HashMap<String, String>,
}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(Default::default);

fn directory() -> PathBuf {
    // A Zed started with `--user-data-dir` (a test instance) keeps its own
    // sessions instead of overwriting the ones the real app restores.
    if let Some(custom) = paths::custom_data_dir() {
        return custom.join("ghostty-tab-sessions");
    }
    paths::home_dir().join(".config/ghostty/tab-sessions")
}

fn file_for(workspace: &Path) -> PathBuf {
    let slug = workspace.to_string_lossy().replace('/', "-");
    directory().join(format!("{slug}.json"))
}

/// The saved tabs of `workspace`, once per workspace per run. Also marks the
/// workspace as restored, which is what lets `save` write it from now on.
pub fn take_restore(workspace: &Path) -> Option<Snapshot> {
    let key = workspace.to_string_lossy().into_owned();
    if !STATE.lock().restored.insert(key) {
        return None;
    }
    let data = std::fs::read(file_for(workspace)).ok()?;
    let snapshot: Snapshot = serde_json::from_slice(&data)
        .inspect_err(|error| {
            log::warn!(
                "unreadable tab sessions for {}: {error}",
                workspace.display()
            )
        })
        .ok()?;
    (!snapshot.tabs.is_empty()).then_some(snapshot)
}

pub fn save(column: &TerminalColumn, cx: &mut Context<TerminalColumn>) {
    let Some(workspace) = column.workspace_path() else {
        return;
    };
    let key = workspace.to_string_lossy().into_owned();
    if !STATE.lock().restored.contains(&key) {
        return;
    }
    let mut tabs = Vec::new();
    let mut selected = 0;
    for (index, tab) in column.tabs().iter().enumerate() {
        // The directory the shell last reported (OSC 7), or the one it was
        // started in until it has: a freshly restored tab must not drop out of
        // the file before its shell has spoken.
        let cwd = tab
            .focused_terminal()
            .and_then(|terminal| terminal.read(cx).working_directory().cloned());
        let Some(cwd) = cwd
            .map(|cwd| cwd.to_string_lossy().into_owned())
            .filter(|cwd| !cwd.is_empty())
        else {
            continue;
        };
        if index == column.selected_index() {
            selected = tabs.len();
        }
        let remote = tab
            .terminals()
            .into_iter()
            .find_map(|terminal| terminal.read(cx).remote_session_name());
        tabs.push(SavedTab {
            blocked: Some(tab.blocked),
            blocked_note: (!tab.blocked_note.is_empty()).then(|| tab.blocked_note.clone()),
            cwd,
            remote: remote.clone(),
            session: if remote.is_some() { None } else { tab.claude_session.clone() },
            title: tab.claude_title.as_ref().map(|title| title.to_string()),
        });
    }
    if tabs.is_empty() {
        return;
    }
    let mut snapshot = Snapshot {
        selected: selected.min(tabs.len() - 1),
        tabs,
        updated: 0.,
        workspace: key.clone(),
    };
    // Compare without the timestamp, so an unchanged column is not rewritten
    // every poll.
    let Ok(stable) = serde_json::to_string_pretty(&snapshot) else {
        return;
    };
    {
        let mut state = STATE.lock();
        if state.last_written.get(&key) == Some(&stable) {
            return;
        }
        state.last_written.insert(key, stable);
    }
    snapshot.updated = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .unwrap_or_default();
    let path = file_for(workspace);
    cx.background_spawn(async move {
        if let Err(error) = write_atomically(&path, &snapshot) {
            log::warn!(
                "could not save tab sessions to {}: {error:#}",
                path.display()
            );
        }
    })
    .detach();
}

fn write_atomically(path: &Path, snapshot: &Snapshot) -> anyhow::Result<()> {
    let data = serde_json::to_vec_pretty(snapshot)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Unique per write: two saves of one workspace can overlap, and a shared
    // temporary name let one rename the other's file away.
    static WRITES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let serial = WRITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let temporary = path.with_extension(format!("json.{}.{serial}.tmp", std::process::id()));
    std::fs::write(&temporary, data)?;
    std::fs::rename(&temporary, path)?;
    Ok(())
}
