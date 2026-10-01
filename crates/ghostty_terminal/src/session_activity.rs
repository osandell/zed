//! What winman knows about a Claude session: what it is about and what it is
//! doing. winman-gui's `SessionActivityMonitor` has Haiku summarize every Claude
//! tab's transcript and writes one file per session to
//! `~/.config/winman/session-activity/<session>.json`:
//!
//! ```json
//! {"session": "...", "topic": "...", "line": "...", "now": "...", "status": "..."}
//! ```
//!
//! `topic` comes from the session's first prompt and does not change; `now` is
//! what is going on at the moment, or what Claude's last reply wants once the
//! turn is over. Drawn as the band under the terminal (`render_session_band`).

use gpui::SharedString;
use serde::Deserialize;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionInfo {
    pub topic: Option<SharedString>,
    pub now: Option<SharedString>,
}

impl SessionInfo {
    pub fn is_empty(&self) -> bool {
        self.topic.is_none() && self.now.is_none()
    }
}

#[derive(Deserialize)]
struct Entry {
    topic: Option<String>,
    now: Option<String>,
}

/// The session's file, or None when winman has not written one (yet).
pub fn read(session: &str) -> Option<SessionInfo> {
    let path = paths::home_dir()
        .join(".config/winman/session-activity")
        .join(format!("{session}.json"));
    let data = std::fs::read(path).ok()?;
    let entry: Entry = serde_json::from_slice(&data).ok()?;
    let clean = |s: Option<String>| {
        s.map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .map(SharedString::from)
    };
    let info = SessionInfo {
        topic: clean(entry.topic),
        now: clean(entry.now),
    };
    (!info.is_empty()).then_some(info)
}

/// Where a forked Claude session came from. `zed-tabs fork` writes one record
/// per fork to `~/.claude/forks/<token>.json` and removes it once the fork has
/// handed back, which is also when the fork's tab closes.
#[derive(Clone, Debug, PartialEq)]
pub struct ForkOrigin {
    pub parent_session: String,
    pub parent_worktree: std::path::PathBuf,
    /// The parent's project root, the folder winman opens as a workspace.
    pub parent_root: std::path::PathBuf,
    pub parent_title: Option<String>,
    /// The parent handed the work over and closed itself (`fork --handoff`):
    /// there is no session to go back to.
    pub handed_off: bool,
}

impl ForkOrigin {
    pub fn workspace_name(&self) -> String {
        self.parent_root
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
}

#[derive(Deserialize)]
struct ForkRecord {
    parent_session: String,
    parent_worktree: String,
    fork_session: String,
    #[serde(default)]
    handoff: bool,
}

/// The parent's title is read from this much of its transcript's tail.
const PARENT_TITLE_TAIL_BYTES: u64 = 256 * 1024;

pub fn fork_origin(session: &str) -> Option<ForkOrigin> {
    let claude = paths::home_dir().join(".claude");
    let record = std::fs::read_dir(claude.join("forks"))
        .ok()?
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
        .filter_map(|entry| std::fs::read(entry.path()).ok())
        .filter_map(|data| serde_json::from_slice::<ForkRecord>(&data).ok())
        .find(|record| record.fork_session == session)?;
    let parent_worktree = std::path::PathBuf::from(&record.parent_worktree);
    let parent_root = match record.parent_worktree.split_once("/worktrees/") {
        Some((root, _)) => std::path::PathBuf::from(root),
        None => parent_worktree.clone(),
    };
    let transcript_name = format!("{}.jsonl", record.parent_session);
    let parent_title = std::fs::read_dir(claude.join("projects"))
        .ok()?
        .flatten()
        .map(|entry| entry.path().join(&transcript_name))
        .find(|path| path.is_file())
        .and_then(|path| {
            let size = std::fs::metadata(&path).ok()?.len();
            let titles = crate::claude_status::newest_titles(
                &path.to_string_lossy(),
                size.saturating_sub(PARENT_TITLE_TAIL_BYTES),
            );
            titles.custom.or(titles.generated)
        });
    Some(ForkOrigin {
        parent_session: record.parent_session,
        parent_worktree,
        parent_root,
        parent_title,
        handed_off: record.handoff,
    })
}
