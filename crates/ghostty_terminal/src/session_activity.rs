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
