//! Claude lamps and titles for terminal tabs, the fork's `ClaudeTabStatus`.
//!
//! Every 1.5 s the foreground thread collects each tab's foreground pids
//! (focused split first) and whether the tab is the one being looked at. The
//! disk and kernel work runs in the background: find the pid that is
//! `claude`, read the state file its hooks write
//! (`~/.claude/tab-state/<pid>.json`, see `~/.claude/hooks/ghostty-tab-state.sh`),
//! the session's AI title from its transcript, and winman's background jobs.

use std::{
    collections::{HashMap, HashSet},
    io::{Read as _, Seek as _, SeekFrom},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, UNIX_EPOCH},
};

use gpui::{AnyWindowHandle, App, AppContext as _, Global, WeakEntity};
use parking_lot::Mutex;

use crate::{ClaudeState, TerminalColumn, tab_sessions};

const POLL_INTERVAL: Duration = Duration::from_millis(1500);
const FIRST_POLL: Duration = Duration::from_millis(500);
const TITLE_TAIL_BYTES: u64 = 64 * 1024;

/// What a tab's Claude hooks last reported.
#[derive(Clone, Debug, Default)]
pub struct Report {
    pub state: String,
    pub ts: f64,
    pub transcript: String,
    /// Whether `transcript` exists on disk. A session that never got a prompt
    /// has none, and `claude --resume` cannot pick it up.
    pub transcript_exists: bool,
    pub session: String,
    pub worktree: String,
    pub worktree_path: String,
}

#[derive(Clone, Debug)]
pub struct ProbeResult {
    /// The Claude pid, or `None` when the tab runs no Claude.
    pub pid: Option<i32>,
    pub title: Option<String>,
    /// The title is the session's own name (`claude --name`, `/rename`), not
    /// Claude's generated one.
    pub title_is_custom: bool,
    pub state: ClaudeState,
    pub report: Option<Report>,
}

struct Probe {
    /// Foreground pid and terminal title of each terminal in the tab.
    candidates: Vec<(i32, String)>,
    focused: bool,
}

struct TranscriptTitle {
    transcript: String,
    title: Option<String>,
    custom: bool,
    scanned_size: u64,
}

#[derive(Default)]
struct ClaudeTabIo {
    background_sessions: HashSet<String>,
    background_jobs_stamp: Option<f64>,
    sessions: HashMap<i32, TranscriptTitle>,
    /// The `done` timestamp the user has seen, per Claude pid.
    acknowledged: HashMap<i32, f64>,
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
}

fn modification_time(path: &Path) -> Option<f64> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    Some(modified.duration_since(UNIX_EPOCH).ok()?.as_secs_f64())
}

/// `KERN_PROCARGS2`: argc, then the exec path, NUL padding, then argv[0].
fn proc_args(pid: i32) -> Option<(String, String)> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    let mut size: libc::size_t = 0;
    let status = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
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
    if status != 0 {
        return None;
    }
    buffer.truncate(size);
    let int_size = std::mem::size_of::<i32>();
    if buffer.len() <= int_size {
        return None;
    }
    let mut index = int_size;
    let exec_start = index;
    while index < buffer.len() && buffer[index] != 0 {
        index += 1;
    }
    let exec_path = String::from_utf8_lossy(&buffer[exec_start..index]).into_owned();
    while index < buffer.len() && buffer[index] == 0 {
        index += 1;
    }
    let argv_start = index;
    while index < buffer.len() && buffer[index] != 0 {
        index += 1;
    }
    let argv0 = String::from_utf8_lossy(&buffer[argv_start..index]).into_owned();
    Some((exec_path, argv0))
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn is_named(pid: i32, names: &[&str]) -> bool {
    proc_args(pid).is_some_and(|(exec_path, argv0)| {
        names.contains(&file_name(&exec_path)) || names.contains(&file_name(&argv0))
    })
}

pub fn is_claude(pid: i32) -> bool {
    is_named(pid, &["claude"])
}

/// Claude, or Codex. Codex's hooks (`~/.codex/hooks.json`) write the same
/// state file with no session or transcript, so it gets the lamps but never a
/// `claude --resume` on restore.
fn is_agent(pid: i32) -> bool {
    is_named(pid, &["claude", "codex"])
}

const IDLE_TITLE_GRACE: f64 = 5.;

fn report_age(report: &Report) -> f64 {
    let now = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs_f64())
        .unwrap_or_default();
    now - report.ts
}

fn report_for(pid: i32) -> Option<Report> {
    let path = home().join(".claude/tab-state").join(format!("{pid}.json"));
    let data = std::fs::read(path).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&data).ok()?;
    let string = |key: &str| {
        value
            .get(key)
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string()
    };
    let transcript = string("transcript");
    let transcript_exists = !transcript.is_empty() && Path::new(&transcript).is_file();
    Some(Report {
        state: value.get("state")?.as_str()?.to_string(),
        ts: value
            .get("ts")
            .and_then(|ts| ts.as_f64())
            .unwrap_or_default(),
        transcript,
        transcript_exists,
        session: string("session"),
        worktree: string("worktree"),
        worktree_path: string("worktreePath"),
    })
}

#[derive(Default)]
pub(crate) struct Titles {
    /// The newest `custom-title`: the name given with `claude --name` or
    /// `/rename`. A forked session gets one for its own task, while the
    /// `ai-title` records it copied from its parent still name the parent's.
    pub custom: Option<String>,
    /// The newest `ai-title`, Claude's generated title.
    pub generated: Option<String>,
}

/// The newest title records in a transcript from byte `start` on.
pub(crate) fn newest_titles(path: &str, start: u64) -> Titles {
    let mut titles = Titles::default();
    let Ok(mut file) = std::fs::File::open(path) else {
        return titles;
    };
    let mut data = Vec::new();
    if file.seek(SeekFrom::Start(start)).is_err() || file.read_to_end(&mut data).is_err() {
        return titles;
    }
    let text = String::from_utf8_lossy(&data);
    for line in text.split('\n').rev() {
        if titles.custom.is_some() {
            break;
        }
        let (key, slot) = if line.contains("\"custom-title\"") {
            ("customTitle", &mut titles.custom)
        } else if titles.generated.is_none() && line.contains("\"ai-title\"") {
            ("aiTitle", &mut titles.generated)
        } else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if let Some(title) = value.get(key).and_then(|title| title.as_str())
            && !title.is_empty()
        {
            *slot = Some(title.to_string());
        }
    }
    titles
}

impl ClaudeTabIo {
    fn run(&mut self, probes: &[Probe]) -> Vec<ProbeResult> {
        self.read_background_sessions();
        let mut live = HashSet::new();
        let results = probes
            .iter()
            .map(|probe| {
                let Some((pid, terminal_title)) = probe
                    .candidates
                    .iter()
                    .find(|(pid, _)| is_agent(*pid))
                    .map(|(pid, title)| (*pid, title.as_str()))
                else {
                    return ProbeResult {
                        pid: None,
                        title: None,
                        title_is_custom: false,
                        state: ClaudeState::Absent,
                        report: None,
                    };
                };
                live.insert(pid);
                let report = report_for(pid);
                let job_running = report
                    .as_ref()
                    .is_some_and(|report| self.background_sessions.contains(&report.session));
                let (title, title_is_custom) = self
                    .title(
                        pid,
                        report.as_ref().map(|report| report.transcript.as_str()),
                    )
                    .unzip();
                let state = self.state(
                    report.as_ref(),
                    pid,
                    terminal_title,
                    probe.focused,
                    job_running,
                );
                ProbeResult {
                    pid: Some(pid),
                    title,
                    title_is_custom: title_is_custom.unwrap_or(false),
                    state,
                    report,
                }
            })
            .collect();
        self.sessions.retain(|pid, _| live.contains(pid));
        self.acknowledged.retain(|pid, _| live.contains(pid));
        results
    }

    fn title(&mut self, pid: i32, transcript: Option<&str>) -> Option<(String, bool)> {
        let Some(transcript) = transcript.filter(|transcript| !transcript.is_empty()) else {
            return self
                .sessions
                .get(&pid)
                .and_then(|session| Some((session.title.clone()?, session.custom)));
        };
        if self
            .sessions
            .get(&pid)
            .is_none_or(|session| session.transcript != transcript)
        {
            self.sessions.insert(
                pid,
                TranscriptTitle {
                    transcript: transcript.to_string(),
                    title: None,
                    custom: false,
                    scanned_size: 0,
                },
            );
        }
        let session = self.sessions.get_mut(&pid)?;
        let size = std::fs::metadata(transcript)
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        if size != session.scanned_size {
            // The first look reads the whole file: a name is only re-recorded
            // now and then, and the tail alone could miss it and show the
            // generated title instead.
            let start = if session.scanned_size == 0 {
                0
            } else {
                size.saturating_sub(TITLE_TAIL_BYTES)
            };
            session.scanned_size = size;
            let found = newest_titles(transcript, start);
            if let Some(name) = found.custom {
                session.title = Some(name);
                session.custom = true;
            } else if let Some(generated) = found.generated
                && !session.custom
            {
                session.title = Some(generated);
            }
        }
        Some((session.title.clone()?, session.custom))
    }

    fn state(
        &mut self,
        report: Option<&Report>,
        pid: i32,
        terminal_title: &str,
        focused: bool,
        job_running: bool,
    ) -> ClaudeState {
        let Some(report) = report else {
            return ClaudeState::Absent;
        };
        match report.state.as_str() {
            // Claude Code runs no Stop hook when a turn is interrupted (Esc),
            // so the report stays on "working" forever. Its terminal title
            // does not lie: a spinner while a turn runs, tool calls included,
            // and "✳" once it is idle. The grace period covers the moment
            // between the prompt hook and the spinner appearing.
            "working"
                if terminal_title.starts_with('✳') && report_age(report) > IDLE_TITLE_GRACE =>
            {
                if job_running {
                    ClaudeState::Background
                } else {
                    ClaudeState::Absent
                }
            }
            "working" => ClaudeState::Working,
            "question" => ClaudeState::Question,
            "done" => {
                if focused {
                    self.acknowledged.insert(pid, report.ts);
                }
                let unread = self.acknowledged.get(&pid).copied().unwrap_or(0.) < report.ts;
                if job_running {
                    ClaudeState::Background
                } else if unread {
                    ClaudeState::Done
                } else {
                    ClaudeState::Absent
                }
            }
            _ if job_running => ClaudeState::Background,
            _ => ClaudeState::Absent,
        }
    }

    fn read_background_sessions(&mut self) {
        let path = home().join(".config/winman/claude-status.json");
        let Some(modified) = modification_time(&path) else {
            self.background_sessions.clear();
            self.background_jobs_stamp = None;
            return;
        };
        if self.background_jobs_stamp == Some(modified) {
            return;
        }
        self.background_jobs_stamp = Some(modified);
        self.background_sessions = std::fs::read(&path)
            .ok()
            .and_then(|data| serde_json::from_slice::<serde_json::Value>(&data).ok())
            .and_then(|value| {
                let jobs = value.get("backgroundJobs")?.as_array()?.clone();
                Some(
                    jobs.iter()
                        .filter_map(|job| job.get("sessionId")?.as_str().map(str::to_string))
                        .filter(|session| !session.is_empty())
                        .collect(),
                )
            })
            .unwrap_or_default();
    }
}

/// Every terminal column, so the poll can reach them all.
#[derive(Default)]
pub struct ClaudeTabStatus {
    columns: Vec<(AnyWindowHandle, WeakEntity<TerminalColumn>)>,
    io: Arc<Mutex<ClaudeTabIo>>,
    started: bool,
}

impl Global for ClaudeTabStatus {}

impl ClaudeTabStatus {
    /// Mark the `done` of Claude `pid` as seen, as focusing its tab does.
    pub fn acknowledge(pid: i32, cx: &App) {
        let Some(status) = cx.try_global::<ClaudeTabStatus>() else {
            return;
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or_default();
        status.io.lock().acknowledged.insert(pid, now);
    }

    pub fn register(window: AnyWindowHandle, column: WeakEntity<TerminalColumn>, cx: &mut App) {
        let status = cx.default_global::<ClaudeTabStatus>();
        status.columns.push((window, column));
        if !status.started {
            status.started = true;
            cx.spawn(async move |cx| {
                cx.background_executor().timer(FIRST_POLL).await;
                loop {
                    Self::poll(cx).await;
                    cx.background_executor().timer(POLL_INTERVAL).await;
                }
            })
            .detach();
        }
    }

    async fn poll(cx: &mut gpui::AsyncApp) {
        let (columns, io) = cx.update(|cx| {
            let status = cx.default_global::<ClaudeTabStatus>();
            status
                .columns
                .retain(|(_, column)| column.upgrade().is_some());
            (status.columns.clone(), status.io.clone())
        });

        // Foreground: which pids each tab shows, and whether it is being read.
        let mut targets = Vec::new();
        let mut probes = Vec::new();
        for (window, column) in &columns {
            let collected = window.update(cx, |_, window, cx| {
                column.update(cx, |column, cx| column.claude_probes(window, cx))
            });
            if let Ok(Ok(tab_probes)) = collected {
                for (tab_id, candidates, focused) in tab_probes {
                    targets.push((column.clone(), tab_id));
                    probes.push(Probe {
                        candidates,
                        focused,
                    });
                }
            }
        }

        let results = cx
            .background_spawn(async move { io.lock().run(&probes) })
            .await;

        let mut by_column: Vec<(WeakEntity<TerminalColumn>, Vec<(u64, ProbeResult)>)> = Vec::new();
        for ((column, tab_id), result) in targets.into_iter().zip(results) {
            match by_column
                .iter_mut()
                .find(|(existing, _)| *existing == column)
            {
                Some((_, entries)) => entries.push((tab_id, result)),
                None => by_column.push((column, vec![(tab_id, result)])),
            }
        }
        for (column, entries) in by_column {
            column
                .update(cx, |column, cx| {
                    column.apply_claude_results(entries, cx);
                    tab_sessions::save(column, cx);
                })
                .ok();
        }
    }
}
