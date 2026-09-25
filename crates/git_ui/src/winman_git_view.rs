//! winman's git view: a read-only history browser in the Amiga look of
//! winman's bar, taking the whole window in place of the workspace. winman
//! toggles it (lcmd+p) with `zed://winman/git-view`. It always shows the
//! repository of the window's active workspace, so a worktree switch in
//! winman switches the view too; there is no repository picker of its own.

use std::{
    collections::HashMap,
    ops::Range,
    path::{Path, PathBuf},
    sync::{Arc, LazyLock, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result};
use gpui::{
    AnyElement, App, Bounds, ClickEvent, Context, Entity, FocusHandle, Focusable, Font,
    HighlightStyle, Hsla, KeyDownEvent, ScrollStrategy, SharedString, StyledText, Subscription,
    Task, UniformListScrollHandle, WeakEntity, Window, canvas, div, fill, linear_color_stop,
    linear_gradient, point, prelude::*, px, relative, rgb, rgba, size, uniform_list,
};
use language::{HighlightId, LanguageRegistry, Rope};
use settings::Settings as _;
use theme::ActiveTheme as _;
use theme_settings::ThemeSettings;
use time::{OffsetDateTime, UtcOffset};
use util::ResultExt as _;
use workspace::{MultiWorkspace, MultiWorkspaceEvent, Workspace};

const FETCH_INTERVAL: Duration = Duration::from_secs(5 * 60);
const REFRESH_INTERVAL: Duration = Duration::from_secs(2);
const LOG_LIMIT: &str = "3000";
const DETAIL_CACHE_SIZE: usize = 64;
const MAX_HIGHLIGHTED_LINES: usize = 20_000;
const MAX_LINE_LENGTH: usize = 1_000;

const COMMIT_ROW_HEIGHT: f32 = 28.;
const DIFF_ROW_HEIGHT: f32 = 20.;
const TREE_ROW_HEIGHT: f32 = 24.;

/// When each repository was last fetched, across views, so toggling the view
/// does not fetch on every open.
static LAST_FETCH: LazyLock<Mutex<HashMap<PathBuf, Instant>>> =
    LazyLock::new(|| Mutex::new(HashMap::default()));

// winman's palette (`Theme.swift`, gruvbox) and `PixelStyle.bevel` on its tab
// block colour, precomputed.
mod palette {
    pub const OUTLINE: u32 = 0x0a0a0c;
    pub const TEXT: u32 = 0xbdae93;
    pub const TEXT_BRIGHT: u32 = 0xebdbb2;
    pub const TEXT_SELECTED: u32 = 0xd5c4a1;
    pub const DIM: u32 = 0x928374;
    pub const STALE: u32 = 0x665c54;
    pub const RED: u32 = 0xfb4934;
    pub const GREEN: u32 = 0xb8bb26;
    pub const YELLOW: u32 = 0xfabd2f;
    pub const BLUE: u32 = 0x83a598;
    pub const AQUA: u32 = 0x8ec07c;
    pub const ORANGE: u32 = 0xfe8019;

    pub const BACKGROUND_TOP: u32 = 0x2f2f2f;
    pub const BACKGROUND_BOTTOM: u32 = 0x171717;

    pub const RAISED_TOP: u32 = 0x474747;
    pub const RAISED_BOTTOM: u32 = 0x1f1f1f;
    pub const RAISED_LIGHT: u32 = 0x727272;
    pub const RAISED_DARK: u32 = 0x151515;

    pub const LIT_TOP: u32 = 0x5c768c;
    pub const LIT_BOTTOM: u32 = 0x20374b;
    pub const LIT_LIGHT: u32 = 0x778d9f;
    pub const LIT_DARK: u32 = 0x101c27;

    pub const SUNKEN: u32 = 0x181818;
    pub const SUNKEN_DARK: u32 = 0x0b0b0b;
    pub const SUNKEN_LIGHT: u32 = 0x4a4a4a;
    pub const CODE: u32 = 0x1b1b1b;
    pub const HUNK: u32 = 0x232323;

    pub const SCREEN_TOP: u32 = 0x0c1a12;
    pub const SCREEN_BOTTOM: u32 = 0x07110b;
    pub const SCREEN_DARK: u32 = 0x050505;

    pub const ADDED_BACKGROUND: u32 = 0xb8bb2621;
    pub const REMOVED_BACKGROUND: u32 = 0xfb493421;
    pub const ADDED_NUMBER: u32 = 0x7c7f2a;
    pub const REMOVED_NUMBER: u32 = 0x8a3a30;
    pub const SCANLINE: u32 = 0x00000047;
}

fn color(hex: u32) -> Hsla {
    rgb(hex).into()
}

fn color_alpha(hex: u32) -> Hsla {
    rgba(hex).into()
}

/// Whether `multi_workspace` shows the git view.
pub fn is_open(multi_workspace: &MultiWorkspace) -> bool {
    multi_workspace
        .full_overlay()
        .is_some_and(|overlay| overlay.clone().downcast::<WinmanGitView>().is_ok())
}

/// Opens the git view over the window, or closes it when it is up.
pub fn toggle(
    multi_workspace: &mut MultiWorkspace,
    window: &mut Window,
    cx: &mut Context<MultiWorkspace>,
) {
    if is_open(multi_workspace) {
        close(multi_workspace, window, cx);
    } else {
        open(multi_workspace, window, cx);
    }
}

fn open(
    multi_workspace: &mut MultiWorkspace,
    window: &mut Window,
    cx: &mut Context<MultiWorkspace>,
) {
    let previous_focus = window.focused(cx);
    let handle = cx.entity().downgrade();
    // Read here: the view cannot read the multi-workspace while it is being
    // updated, which it is until this returns.
    let active = active_root(multi_workspace.workspace(), cx);
    let view = cx.new(|cx| WinmanGitView::new(handle, previous_focus, active, window, cx));
    let focus_handle = view.focus_handle(cx);
    multi_workspace.set_full_overlay(Some(view.into()), window, cx);
    window.focus(&focus_handle, cx);
}

fn close(
    multi_workspace: &mut MultiWorkspace,
    window: &mut Window,
    cx: &mut Context<MultiWorkspace>,
) {
    let view = multi_workspace
        .full_overlay()
        .and_then(|overlay| overlay.clone().downcast::<WinmanGitView>().ok());
    let current_root = active_root(multi_workspace.workspace(), cx).map(|(root, _)| root);
    // The keyboard goes back where it was, unless winman switched worktree
    // meanwhile: that focus belongs to a workspace no longer shown.
    let previous_focus = view.and_then(|view| {
        let view = view.read(cx);
        (view.opened_root == current_root)
            .then(|| view.previous_focus.clone())
            .flatten()
    });
    multi_workspace.set_full_overlay(None, window, cx);
    match previous_focus {
        Some(focus_handle) => window.focus(&focus_handle, cx),
        None => {
            let workspace = multi_workspace.workspace().clone();
            let focus_handle = workspace.read(cx).active_pane().focus_handle(cx);
            window.focus(&focus_handle, cx);
        }
    }
}

/// Gives the git view the keyboard if it is up. Returns whether it was.
pub fn focus_if_open(multi_workspace: &MultiWorkspace, window: &mut Window, cx: &mut App) -> bool {
    let Some(view) = multi_workspace
        .full_overlay()
        .and_then(|overlay| overlay.clone().downcast::<WinmanGitView>().ok())
    else {
        return false;
    };
    let focus_handle = view.focus_handle(cx);
    window.focus(&focus_handle, cx);
    true
}

fn active_root(
    workspace: &Entity<Workspace>,
    cx: &App,
) -> Option<(PathBuf, Arc<LanguageRegistry>)> {
    let workspace = workspace.read(cx);
    let root = workspace.root_paths(cx).into_iter().next()?;
    let languages = workspace.project().read(cx).languages().clone();
    Some((root.to_path_buf(), languages))
}

/// The name winman shows for a worktree: the project, `winman-mac` for
/// `…/winman-mac/worktrees/main`, else the directory itself.
fn project_name(root: &Path) -> String {
    let name = |path: &Path| {
        path.file_name()
            .map(|name| name.to_string_lossy().into_owned())
    };
    let parent = root.parent();
    match parent.and_then(name).as_deref() {
        Some("worktrees") => parent.and_then(Path::parent).and_then(name),
        _ => name(root),
    }
    .unwrap_or_default()
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RefKind {
    Head,
    Branch,
    Remote,
    Tag,
}

#[derive(Clone)]
struct RefLabel {
    name: SharedString,
    kind: RefKind,
}

#[derive(Clone)]
struct CommitRow {
    sha: SharedString,
    short_sha: SharedString,
    author: SharedString,
    timestamp: i64,
    refs: Vec<RefLabel>,
    prefix: Option<SharedString>,
    subject: SharedString,
}

#[derive(Clone)]
struct RefEntry {
    name: SharedString,
    target: SharedString,
    current: bool,
    track: Option<SharedString>,
}

#[derive(Clone, Default)]
struct Refs {
    head_branch: Option<SharedString>,
    head_track: Option<SharedString>,
    head_upstream: Option<SharedString>,
    worktrees: Vec<RefEntry>,
    branches: Vec<RefEntry>,
    remotes: Vec<RefEntry>,
    tags: Vec<RefEntry>,
    stashes: Vec<RefEntry>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FileStatus {
    Modified,
    Added,
    Deleted,
    Renamed,
}

impl FileStatus {
    fn letter(self) -> &'static str {
        match self {
            FileStatus::Modified => "M",
            FileStatus::Added => "A",
            FileStatus::Deleted => "D",
            FileStatus::Renamed => "R",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LineKind {
    Hunk,
    Context,
    Added,
    Removed,
}

struct DiffLine {
    kind: LineKind,
    old_number: Option<u32>,
    new_number: Option<u32>,
    text: SharedString,
    highlights: Vec<(Range<usize>, HighlightId)>,
}

struct FileDiff {
    path: String,
    old_path: Option<String>,
    status: FileStatus,
    binary: bool,
    added: usize,
    removed: usize,
    lines: Vec<DiffLine>,
}

struct CommitDetail {
    author: SharedString,
    timestamp: i64,
    subject: SharedString,
    files: Vec<FileDiff>,
}

struct TreeRow {
    depth: usize,
    name: SharedString,
    file_index: Option<usize>,
}

enum FetchState {
    Idle,
    Running,
    Failed,
}

pub struct WinmanGitView {
    multi_workspace: WeakEntity<MultiWorkspace>,
    previous_focus: Option<FocusHandle>,
    opened_root: Option<PathBuf>,
    focus_handle: FocusHandle,
    root: Option<PathBuf>,
    languages: Option<Arc<LanguageRegistry>>,
    commits: Arc<Vec<CommitRow>>,
    refs: Refs,
    ref_signature: Option<String>,
    error: Option<SharedString>,
    loading: bool,
    selected_commit: usize,
    selected_file: usize,
    details: HashMap<SharedString, Arc<CommitDetail>>,
    detail_order: Vec<SharedString>,
    tree: Vec<TreeRow>,
    tree_sha: Option<SharedString>,
    /// Ref sections folded shut, by label. Long ones start folded.
    collapsed: HashMap<&'static str, bool>,
    fetch_state: FetchState,
    commit_scroll: UniformListScrollHandle,
    diff_scroll: UniformListScrollHandle,
    load_task: Option<Task<()>>,
    detail_task: Option<Task<()>>,
    fetch_task: Option<Task<()>>,
    _poll_task: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl Focusable for WinmanGitView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl WinmanGitView {
    fn new(
        multi_workspace: WeakEntity<MultiWorkspace>,
        previous_focus: Option<FocusHandle>,
        active: Option<(PathBuf, Arc<LanguageRegistry>)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        let mut subscriptions = Vec::new();
        if let Some(multi_workspace) = multi_workspace.upgrade() {
            subscriptions.push(cx.subscribe_in(
                &multi_workspace,
                window,
                |this, _, event: &MultiWorkspaceEvent, _window, cx| {
                    if let MultiWorkspaceEvent::ActiveWorkspaceChanged { .. } = event {
                        this.follow_active_workspace(cx);
                    }
                },
            ));
        }
        // winman moves the keyboard to the terminal or the editor of the
        // worktree it switches to. While the view is up it owns the keyboard,
        // so take it back, unless a modal (the command palette, say) took it.
        subscriptions.push(
            cx.on_focus_out(&focus_handle, window, |this, _, window, cx| {
                let Some(multi_workspace) = this.multi_workspace.upgrade() else {
                    return;
                };
                let workspace = multi_workspace.read(cx).workspace().clone();
                if workspace.update(cx, |workspace, cx| workspace.has_active_modal(window, cx)) {
                    return;
                }
                cx.defer_in(window, |this, window, cx| {
                    let still_open = this
                        .multi_workspace
                        .upgrade()
                        .is_some_and(|multi_workspace| is_open(multi_workspace.read(cx)));
                    if still_open && !this.focus_handle.contains_focused(window, cx) {
                        window.focus(&this.focus_handle, cx);
                    }
                });
            }),
        );

        let poll_task = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(REFRESH_INTERVAL).await;
                let Ok(()) = this.update(cx, |this, cx| this.poll(cx)) else {
                    break;
                };
            }
        });

        let mut this = Self {
            multi_workspace,
            previous_focus,
            opened_root: active.as_ref().map(|(root, _)| root.clone()),
            focus_handle,
            root: None,
            languages: None,
            commits: Arc::new(Vec::new()),
            refs: Refs::default(),
            ref_signature: None,
            error: None,
            loading: false,
            selected_commit: 0,
            selected_file: 0,
            details: HashMap::default(),
            detail_order: Vec::new(),
            tree: Vec::new(),
            tree_sha: None,
            collapsed: HashMap::default(),
            fetch_state: FetchState::Idle,
            commit_scroll: UniformListScrollHandle::new(),
            diff_scroll: UniformListScrollHandle::new(),
            load_task: None,
            detail_task: None,
            fetch_task: None,
            _poll_task: poll_task,
            _subscriptions: subscriptions,
        };
        this.show_root(active, cx);
        this
    }

    fn follow_active_workspace(&mut self, cx: &mut Context<Self>) {
        let Some(multi_workspace) = self.multi_workspace.upgrade() else {
            return;
        };
        let workspace = multi_workspace.read(cx).workspace().clone();
        let active = active_root(&workspace, cx);
        self.show_root(active, cx);
    }

    fn show_root(
        &mut self,
        active: Option<(PathBuf, Arc<LanguageRegistry>)>,
        cx: &mut Context<Self>,
    ) {
        let root = active.as_ref().map(|(root, _)| root.clone());
        if root == self.root {
            return;
        }
        self.root = root;
        self.languages = active.map(|(_, languages)| languages);
        self.commits = Arc::new(Vec::new());
        self.refs = Refs::default();
        self.ref_signature = None;
        self.error = None;
        self.selected_commit = 0;
        self.selected_file = 0;
        self.details.clear();
        self.detail_order.clear();
        self.tree.clear();
        self.tree_sha = None;
        self.detail_task = None;
        self.fetch_task = None;
        self.fetch_state = FetchState::Idle;
        self.commit_scroll.scroll_to_item(0, ScrollStrategy::Top);
        self.diff_scroll.scroll_to_item(0, ScrollStrategy::Top);
        self.reload(cx);
        self.maybe_fetch(cx);
        cx.notify();
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.root.clone() else {
            self.error = Some("Ingen worktree i fokus".into());
            return;
        };
        self.loading = true;
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn({
                    let root = root.clone();
                    async move { load_repository(&root).await }
                })
                .await;
            this.update(cx, |this, cx| {
                if this.root.as_deref() != Some(root.as_path()) {
                    return;
                }
                this.loading = false;
                match result {
                    Ok((commits, refs, signature)) => {
                        let selected_sha = this
                            .commits
                            .get(this.selected_commit)
                            .map(|commit| commit.sha.clone());
                        this.commits = Arc::new(commits);
                        this.refs = refs;
                        this.ref_signature = Some(signature);
                        this.error = None;
                        this.selected_commit = selected_sha
                            .and_then(|sha| this.commits.iter().position(|c| c.sha == sha))
                            .unwrap_or(0);
                        this.load_selected_detail(cx);
                    }
                    Err(error) => {
                        log::warn!("winman git view: {error:#}");
                        this.error = Some(format!("{error:#}").into());
                    }
                }
                cx.notify();
            })
            .log_err();
        }));
    }

    /// Reloads when HEAD or any ref moved (a commit in the terminal, a fetch,
    /// a checkout), and fetches on schedule.
    fn poll(&mut self, cx: &mut Context<Self>) {
        self.follow_active_workspace(cx);
        self.maybe_fetch(cx);
        let Some(root) = self.root.clone() else {
            return;
        };
        if self.loading {
            return;
        }
        let known = self.ref_signature.clone();
        cx.spawn(async move |this, cx| {
            let signature = cx
                .background_spawn({
                    let root = root.clone();
                    async move { ref_signature(&root).await }
                })
                .await;
            let Some(signature) = signature.log_err() else {
                return;
            };
            if Some(&signature) != known.as_ref() {
                this.update(cx, |this, cx| {
                    if this.root.as_deref() == Some(root.as_path()) && !this.loading {
                        this.reload(cx);
                    }
                })
                .log_err();
            }
        })
        .detach();
    }

    fn maybe_fetch(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.root.clone() else {
            return;
        };
        if self.fetch_task.is_some() {
            return;
        }
        let due = LAST_FETCH
            .lock()
            .map(|last| {
                last.get(&root)
                    .is_none_or(|at| at.elapsed() >= FETCH_INTERVAL)
            })
            .unwrap_or(true);
        if !due {
            return;
        }
        if let Ok(mut last) = LAST_FETCH.lock() {
            last.insert(root.clone(), Instant::now());
        }
        self.fetch_state = FetchState::Running;
        cx.notify();
        self.fetch_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn({
                    let root = root.clone();
                    async move { git(&root, &["fetch", "--all", "--prune", "--quiet"]).await }
                })
                .await;
            this.update(cx, |this, cx| {
                if this.root.as_deref() != Some(root.as_path()) {
                    return;
                }
                this.fetch_task = None;
                this.fetch_state = match result {
                    Ok(_) => FetchState::Idle,
                    Err(error) => {
                        log::warn!("winman git view: fetch failed: {error:#}");
                        FetchState::Failed
                    }
                };
                this.reload(cx);
                cx.notify();
            })
            .log_err();
        }));
    }

    fn select_commit(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.commits.is_empty() {
            return;
        }
        let index = index.min(self.commits.len() - 1);
        if index != self.selected_commit {
            self.selected_commit = index;
            self.selected_file = 0;
            self.diff_scroll.scroll_to_item(0, ScrollStrategy::Top);
        }
        self.commit_scroll
            .scroll_to_item(index, ScrollStrategy::Nearest);
        self.load_selected_detail(cx);
        cx.notify();
    }

    fn select_sha(&mut self, sha: &str, cx: &mut Context<Self>) {
        if let Some(index) = self.commits.iter().position(|commit| commit.sha == sha) {
            self.select_commit(index, cx);
            self.commit_scroll
                .scroll_to_item(index, ScrollStrategy::Center);
        }
    }

    fn select_file(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(detail) = self.selected_detail() else {
            return;
        };
        if detail.files.is_empty() {
            return;
        }
        self.selected_file = index.min(detail.files.len() - 1);
        self.diff_scroll.scroll_to_item(0, ScrollStrategy::Top);
        cx.notify();
    }

    fn selected_detail(&self) -> Option<Arc<CommitDetail>> {
        let commit = self.commits.get(self.selected_commit)?;
        self.details.get(&commit.sha).cloned()
    }

    fn load_selected_detail(&mut self, cx: &mut Context<Self>) {
        let Some(commit) = self.commits.get(self.selected_commit) else {
            return;
        };
        let sha = commit.sha.clone();
        if self.details.contains_key(&sha) {
            self.rebuild_tree(cx);
            return;
        }
        let (Some(root), languages) = (self.root.clone(), self.languages.clone()) else {
            return;
        };
        self.detail_task = Some(cx.spawn(async move |this, cx| {
            // Holding an arrow key walks the list faster than git can show
            // each commit; only load where the selection comes to rest.
            cx.background_executor()
                .timer(Duration::from_millis(40))
                .await;
            let detail = load_detail(root, sha.clone(), languages, cx).await;
            this.update(cx, |this, cx| {
                match detail {
                    Ok(detail) => {
                        this.details.insert(sha.clone(), Arc::new(detail));
                        this.detail_order.push(sha);
                        if this.detail_order.len() > DETAIL_CACHE_SIZE {
                            let evicted = this.detail_order.remove(0);
                            this.details.remove(&evicted);
                        }
                        this.rebuild_tree(cx);
                    }
                    Err(error) => log::warn!("winman git view: {error:#}"),
                }
                cx.notify();
            })
            .log_err();
        }));
    }

    fn rebuild_tree(&mut self, _cx: &mut Context<Self>) {
        let Some(commit) = self.commits.get(self.selected_commit) else {
            return;
        };
        if self.tree_sha.as_ref() == Some(&commit.sha) {
            return;
        }
        let Some(detail) = self.details.get(&commit.sha) else {
            return;
        };
        self.tree_sha = Some(commit.sha.clone());
        self.tree = build_tree(detail);
    }

    fn scroll_diff_by(&mut self, rows: f32, cx: &mut Context<Self>) {
        let handle = self.diff_scroll.0.borrow().base_handle.clone();
        let offset = handle.offset();
        let max = handle.max_offset();
        let y = (offset.y - px(rows * DIFF_ROW_HEIGHT)).clamp(-max.y, px(0.));
        handle.set_offset(point(offset.x, y));
        cx.notify();
    }

    fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let keystroke = &event.keystroke;
        let modifiers = &keystroke.modifiers;
        if modifiers.platform || modifiers.control || modifiers.alt {
            return;
        }
        let page = 20;
        match (keystroke.key.as_str(), modifiers.shift) {
            ("down" | "j", false) => self.select_commit(self.selected_commit + 1, cx),
            ("up" | "k", false) => self.select_commit(self.selected_commit.saturating_sub(1), cx),
            ("pagedown", _) => self.select_commit(self.selected_commit + page, cx),
            ("pageup", _) => self.select_commit(self.selected_commit.saturating_sub(page), cx),
            ("home", _) => self.select_commit(0, cx),
            ("end", _) => self.select_commit(usize::MAX, cx),
            ("tab" | "]", false) | ("down" | "j", true) => {
                self.select_file(self.selected_file + 1, cx)
            }
            ("tab", true) | ("[", false) | ("up" | "k", true) => {
                self.select_file(self.selected_file.saturating_sub(1), cx)
            }
            ("space", false) => self.scroll_diff_by(20., cx),
            ("space", true) => self.scroll_diff_by(-20., cx),
            _ => return,
        }
        cx.stop_propagation();
    }

    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(multi_workspace) = self.multi_workspace.upgrade() else {
            return;
        };
        // Deferred: closing drops this view, which is being updated right now.
        window.defer(cx, move |window, cx| {
            multi_workspace.update(cx, |multi_workspace, cx| {
                if is_open(multi_workspace) {
                    close(multi_workspace, window, cx);
                }
            });
        });
    }
}

// ---------------------------------------------------------------------------
// git
// ---------------------------------------------------------------------------

async fn git(root: &Path, args: &[&str]) -> Result<String> {
    let output = util::command::new_command("git")
        .current_dir(root)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .await
        .with_context(|| format!("running git {}", args.join(" ")))?;
    anyhow::ensure!(
        output.status.success(),
        "git {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

async fn ref_signature(root: &Path) -> Result<String> {
    // `show-ref` fails when there are no refs at all, an empty repository.
    let refs = git(root, &["show-ref", "--head"]).await.unwrap_or_default();
    let head = git(root, &["symbolic-ref", "-q", "HEAD"])
        .await
        .unwrap_or_default();
    Ok(format!("{head}\n{refs}"))
}

async fn load_repository(root: &Path) -> Result<(Vec<CommitRow>, Refs, String)> {
    let signature = ref_signature(root).await?;
    let remotes: Vec<String> = git(root, &["remote"])
        .await?
        .lines()
        .map(str::to_string)
        .collect();
    let log = git(
        root,
        &[
            "log",
            "-n",
            LOG_LIMIT,
            "--no-color",
            "--format=%H%x1f%h%x1f%an%x1f%at%x1f%D%x1f%s%x1e",
            "HEAD",
        ],
    );
    let (log, refs) = futures::join!(log, load_refs(root, &remotes));
    // A repository without commits has no HEAD to log.
    let commits = log
        .unwrap_or_default()
        .split('\x1e')
        .filter_map(|record| parse_commit(record.trim_start_matches('\n'), &remotes))
        .collect();
    Ok((commits, refs?, signature))
}

fn parse_commit(record: &str, remotes: &[String]) -> Option<CommitRow> {
    let mut fields = record.split('\x1f');
    let sha = fields.next()?.trim();
    if sha.is_empty() {
        return None;
    }
    let short_sha = fields.next()?;
    let author = fields.next()?;
    let timestamp = fields.next()?.parse().unwrap_or(0);
    let decorations = fields.next()?;
    let subject = fields.next().unwrap_or_default();

    let mut refs = Vec::new();
    for decoration in decorations.split(", ").filter(|d| !d.is_empty()) {
        if let Some(branch) = decoration.strip_prefix("HEAD -> ") {
            refs.push(RefLabel {
                name: branch.to_string().into(),
                kind: RefKind::Head,
            });
        } else if let Some(tag) = decoration.strip_prefix("tag: ") {
            refs.push(RefLabel {
                name: tag.to_string().into(),
                kind: RefKind::Tag,
            });
        } else if decoration == "HEAD" || decoration.ends_with("/HEAD") {
            continue;
        } else if remotes
            .iter()
            .any(|remote| decoration.starts_with(&format!("{remote}/")))
        {
            refs.push(RefLabel {
                name: decoration.to_string().into(),
                kind: RefKind::Remote,
            });
        } else {
            refs.push(RefLabel {
                name: decoration.to_string().into(),
                kind: RefKind::Branch,
            });
        }
    }

    let (prefix, subject) = match subject.split_once(": ") {
        Some((prefix, rest)) if prefix.chars().count() <= 32 && !prefix.contains('`') => {
            (Some(SharedString::from(format!("{prefix}:"))), rest)
        }
        _ => (None, subject),
    };
    // `%h` grows with the repository (ten digits in Zed's); a fixed seven
    // keeps the column narrow.
    let short_sha = short_sha.get(..7).unwrap_or(short_sha);
    Some(CommitRow {
        sha: sha.to_string().into(),
        short_sha: short_sha.to_string().into(),
        author: author.to_string().into(),
        timestamp,
        refs,
        prefix,
        subject: subject.to_string().into(),
    })
}

fn format_track(track: &str) -> Option<SharedString> {
    // `ahead 1, behind 139` as winman's bar writes it: `139↓ 1↑`.
    let mut ahead = None;
    let mut behind = None;
    for part in track.split(", ") {
        if let Some(count) = part.strip_prefix("ahead ") {
            ahead = Some(count);
        } else if let Some(count) = part.strip_prefix("behind ") {
            behind = Some(count);
        }
    }
    let text = match (behind, ahead) {
        (Some(behind), Some(ahead)) => format!("{behind}↓ {ahead}↑"),
        (Some(behind), None) => format!("{behind}↓"),
        (None, Some(ahead)) => format!("{ahead}↑"),
        (None, None) => return None,
    };
    Some(text.into())
}

async fn load_refs(root: &Path, remotes: &[String]) -> Result<Refs> {
    let mut refs = Refs::default();
    let listing = git(
        root,
        &[
            "for-each-ref",
            "--sort=-committerdate",
            "--format=%(refname)%1f%(HEAD)%1f%(objectname)%1f%(upstream:track,nobracket)%1f%(upstream:short)",
            "refs/heads",
            "refs/remotes",
            "refs/tags",
        ],
    )
    .await?;
    for line in listing.lines() {
        let mut fields = line.split('\x1f');
        let (Some(name), Some(head), Some(target)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let track = fields.next().unwrap_or_default();
        let upstream = fields.next().unwrap_or_default();
        let current = head == "*";
        let entry = |short: &str| RefEntry {
            name: short.to_string().into(),
            target: target.to_string().into(),
            current,
            track: format_track(track),
        };
        if let Some(branch) = name.strip_prefix("refs/heads/") {
            if current {
                refs.head_branch = Some(branch.to_string().into());
                refs.head_track = format_track(track);
                refs.head_upstream = (!upstream.is_empty()).then(|| upstream.to_string().into());
            }
            refs.branches.push(entry(branch));
        } else if let Some(remote) = name.strip_prefix("refs/remotes/") {
            if !remote.ends_with("/HEAD") {
                refs.remotes.push(entry(remote));
            }
        } else if let Some(tag) = name.strip_prefix("refs/tags/") {
            refs.tags.push(entry(tag));
        }
    }
    // Branches in a stable order with the current one first; remotes grouped
    // by remote in `git remote` order.
    refs.branches
        .sort_by_key(|entry| std::cmp::Reverse(entry.current));
    refs.remotes.sort_by_key(|entry| {
        remotes
            .iter()
            .position(|remote| entry.name.starts_with(&format!("{remote}/")))
            .unwrap_or(usize::MAX)
    });

    let canonical_root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let worktrees = git(root, &["worktree", "list", "--porcelain"])
        .await
        .unwrap_or_default();
    let mut path: Option<PathBuf> = None;
    let mut target = String::new();
    let flush = |path: &mut Option<PathBuf>, target: &str, refs: &mut Refs| {
        if let Some(path) = path.take() {
            let canonical = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.to_string_lossy().into_owned());
            refs.worktrees.push(RefEntry {
                name: name.into(),
                target: target.to_string().into(),
                current: canonical == canonical_root,
                track: None,
            });
        }
    };
    for line in worktrees.lines() {
        if let Some(worktree) = line.strip_prefix("worktree ") {
            flush(&mut path, &target, &mut refs);
            path = Some(PathBuf::from(worktree));
            target.clear();
        } else if let Some(head) = line.strip_prefix("HEAD ") {
            target = head.to_string();
        }
    }
    flush(&mut path, &target, &mut refs);

    let stashes = git(root, &["stash", "list", "--format=%H%x1f%gd%x1f%s"])
        .await
        .unwrap_or_default();
    for line in stashes.lines() {
        let mut fields = line.split('\x1f');
        let (Some(target), Some(_name), Some(subject)) =
            (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        refs.stashes.push(RefEntry {
            name: subject.to_string().into(),
            target: target.to_string().into(),
            current: false,
            track: None,
        });
    }
    Ok(refs)
}

async fn load_detail(
    root: PathBuf,
    sha: SharedString,
    languages: Option<Arc<LanguageRegistry>>,
    cx: &mut gpui::AsyncApp,
) -> Result<CommitDetail> {
    let output = cx
        .background_spawn(async move {
            git(
                &root,
                &[
                    "show",
                    "--no-color",
                    "--no-ext-diff",
                    "--diff-merges=first-parent",
                    "-M",
                    "-U3",
                    "--format=%an%x1f%at%x1f%s%x1e",
                    sha.as_ref(),
                ],
            )
            .await
        })
        .await?;
    let (header, patch) = output.split_once('\x1e').unwrap_or((&output, ""));
    let mut fields = header.split('\x1f');
    let author = fields.next().unwrap_or_default().to_string();
    let timestamp = fields.next().and_then(|t| t.parse().ok()).unwrap_or(0);
    let subject = fields.next().unwrap_or_default().to_string();
    let mut files = parse_patch(patch);

    if let Some(languages) = languages {
        let mut budget = MAX_HIGHLIGHTED_LINES;
        for file in &mut files {
            if file.binary || file.lines.is_empty() || file.lines.len() > budget {
                continue;
            }
            budget -= file.lines.len();
            let Some(language) = languages
                .load_language_for_file_path(Path::new(&file.path))
                .await
                .ok()
            else {
                continue;
            };
            let lines = std::mem::take(&mut file.lines);
            file.lines = cx
                .background_spawn(async move { highlight_lines(lines, &language) })
                .await;
        }
    }

    Ok(CommitDetail {
        author: author.into(),
        timestamp,
        subject: subject.into(),
        files,
    })
}

fn expand_tabs(text: &str) -> String {
    let text = text.replace('\t', "    ");
    if text.len() <= MAX_LINE_LENGTH {
        return text;
    }
    let mut end = MAX_LINE_LENGTH;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

fn parse_hunk_header(line: &str) -> Option<(u32, u32)> {
    // `@@ -12,5 +12,7 @@ context`
    let rest = line.strip_prefix("@@ -")?;
    let (old, rest) = rest.split_once(' ')?;
    let new = rest.strip_prefix('+')?.split(' ').next()?;
    let start = |range: &str| range.split(',').next()?.parse::<u32>().ok();
    Some((start(old)?, start(new)?))
}

fn parse_patch(patch: &str) -> Vec<FileDiff> {
    let mut files: Vec<FileDiff> = Vec::new();
    let mut in_hunk = false;
    let mut old_number = 0;
    let mut new_number = 0;
    for line in patch.lines() {
        if let Some(paths) = line.strip_prefix("diff --git ") {
            in_hunk = false;
            let path = paths
                .rsplit_once(" b/")
                .map(|(_, path)| path)
                .unwrap_or(paths)
                .to_string();
            files.push(FileDiff {
                path,
                old_path: None,
                status: FileStatus::Modified,
                binary: false,
                added: 0,
                removed: 0,
                lines: Vec::new(),
            });
            continue;
        }
        let Some(file) = files.last_mut() else {
            continue;
        };
        if let Some((old, new)) = line
            .starts_with("@@")
            .then(|| parse_hunk_header(line))
            .flatten()
        {
            in_hunk = true;
            old_number = old;
            new_number = new;
            file.lines.push(DiffLine {
                kind: LineKind::Hunk,
                old_number: None,
                new_number: None,
                text: expand_tabs(line).into(),
                highlights: Vec::new(),
            });
            continue;
        }
        if !in_hunk {
            if line.starts_with("new file mode") {
                file.status = FileStatus::Added;
            } else if line.starts_with("deleted file mode") {
                file.status = FileStatus::Deleted;
            } else if let Some(from) = line.strip_prefix("rename from ") {
                file.status = FileStatus::Renamed;
                file.old_path = Some(from.to_string());
            } else if let Some(to) = line.strip_prefix("rename to ") {
                file.path = to.to_string();
            } else if line.starts_with("Binary files") {
                file.binary = true;
            } else if let Some(path) = line.strip_prefix("+++ b/") {
                file.path = path.to_string();
            }
            continue;
        }
        let (kind, text) = match line.as_bytes().first() {
            Some(b'+') => (LineKind::Added, &line[1..]),
            Some(b'-') => (LineKind::Removed, &line[1..]),
            Some(b' ') => (LineKind::Context, &line[1..]),
            Some(b'\\') => continue,
            None => (LineKind::Context, ""),
            _ => continue,
        };
        let (old, new) = match kind {
            LineKind::Added => {
                file.added += 1;
                new_number += 1;
                (None, Some(new_number - 1))
            }
            LineKind::Removed => {
                file.removed += 1;
                old_number += 1;
                (Some(old_number - 1), None)
            }
            _ => {
                old_number += 1;
                new_number += 1;
                (Some(old_number - 1), Some(new_number - 1))
            }
        };
        file.lines.push(DiffLine {
            kind,
            old_number: old,
            new_number: new,
            text: expand_tabs(text).into(),
            highlights: Vec::new(),
        });
    }
    files
}

/// Highlights a file's diff lines. The old and the new side are each parsed
/// as one text made of their lines, so constructs that span lines inside a
/// hunk highlight right; the gaps between hunks are simply left out.
fn highlight_lines(mut lines: Vec<DiffLine>, language: &Arc<language::Language>) -> Vec<DiffLine> {
    for side in [LineKind::Removed, LineKind::Added] {
        let mut text = String::new();
        let mut spans: Vec<(usize, Range<usize>)> = Vec::new();
        for (index, line) in lines.iter().enumerate() {
            let on_side = line.kind == LineKind::Context || line.kind == side;
            if !on_side {
                continue;
            }
            let start = text.len();
            text.push_str(&line.text);
            spans.push((index, start..text.len()));
            text.push('\n');
        }
        if spans.is_empty() {
            continue;
        }
        let rope = Rope::from(text.as_str());
        let highlights = language.highlight_text(&rope, 0..text.len());
        for (range, id) in highlights {
            let first = spans.partition_point(|(_, span)| span.end <= range.start);
            for (index, span) in spans.iter().skip(first) {
                if span.start >= range.end {
                    break;
                }
                let line = &mut lines[*index];
                // Context lines are on both sides; take their highlights once.
                if line.kind == LineKind::Context && side == LineKind::Added {
                    break;
                }
                let start = range.start.max(span.start) - span.start;
                let end = range.end.min(span.end) - span.start;
                if start < end {
                    line.highlights.push((start..end, id));
                }
            }
        }
    }
    lines
}

fn build_tree(detail: &CommitDetail) -> Vec<TreeRow> {
    let mut order: Vec<usize> = (0..detail.files.len()).collect();
    order.sort_by(|a, b| detail.files[*a].path.cmp(&detail.files[*b].path));
    let mut rows = Vec::new();
    let mut open: Vec<&str> = Vec::new();
    for index in order {
        let path = &detail.files[index].path;
        let mut components: Vec<&str> = path.split('/').collect();
        let file_name = components.pop().unwrap_or(path);
        let shared = open
            .iter()
            .zip(&components)
            .take_while(|(a, b)| a == b)
            .count();
        open.truncate(shared);
        for component in &components[shared..] {
            rows.push(TreeRow {
                depth: open.len(),
                name: component.to_string().into(),
                file_index: None,
            });
            open.push(component);
        }
        rows.push(TreeRow {
            depth: open.len(),
            name: file_name.to_string().into(),
            file_index: Some(index),
        });
    }
    rows
}

// ---------------------------------------------------------------------------
// time
// ---------------------------------------------------------------------------

const MONTHS: [&str; 12] = [
    "jan", "feb", "mar", "apr", "maj", "jun", "jul", "aug", "sep", "okt", "nov", "dec",
];

fn local(timestamp: i64) -> Option<OffsetDateTime> {
    let offset = UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC);
    Some(
        OffsetDateTime::from_unix_timestamp(timestamp)
            .ok()?
            .to_offset(offset),
    )
}

fn format_clock(timestamp: i64) -> String {
    local(timestamp)
        .map(|at| format!("{:02}:{:02}", at.hour(), at.minute()))
        .unwrap_or_default()
}

fn format_short_time(timestamp: i64) -> String {
    let (Some(at), Some(now)) = (
        local(timestamp),
        local(OffsetDateTime::now_utc().unix_timestamp()),
    ) else {
        return String::new();
    };
    let month = MONTHS[(u8::from(at.month()) as usize).saturating_sub(1).min(11)];
    let days = (now.date() - at.date()).whole_days();
    match days {
        0 => format!("{:02}:{:02}", at.hour(), at.minute()),
        1 => format!("igår {:02}:{:02}", at.hour(), at.minute()),
        _ if at.year() == now.year() => {
            format!("{} {} {:02}:{:02}", at.day(), month, at.hour(), at.minute())
        }
        _ => format!("{} {} {}", at.day(), month, at.year()),
    }
}

fn format_long_time(timestamp: i64) -> String {
    let Some(at) = local(timestamp) else {
        return String::new();
    };
    let month = MONTHS[(u8::from(at.month()) as usize).saturating_sub(1).min(11)];
    format!(
        "{} {} {} {:02}:{:02}",
        at.day(),
        month,
        at.year(),
        at.hour(),
        at.minute()
    )
}

// ---------------------------------------------------------------------------
// Amiga chrome
// ---------------------------------------------------------------------------

/// The four one-pixel edges of a bevel inside a `relative` element's border:
/// `light` along the top and left, `dark` along the bottom and right.
fn bevel_edges(element: gpui::Div, light: Hsla, dark: Hsla) -> gpui::Div {
    element
        .child(div().absolute().top_0().left_0().right_0().h_px().bg(light))
        .child(
            div()
                .absolute()
                .top_0()
                .left_0()
                .bottom_0()
                .w_px()
                .bg(light),
        )
        .child(
            div()
                .absolute()
                .bottom_0()
                .left_0()
                .right_0()
                .h_px()
                .bg(dark),
        )
        .child(
            div()
                .absolute()
                .top_0()
                .right_0()
                .bottom_0()
                .w_px()
                .bg(dark),
        )
}

fn gradient(top: u32, bottom: u32) -> gpui::Background {
    linear_gradient(
        180.,
        linear_color_stop(color(top), 0.),
        linear_color_stop(color(bottom), 1.),
    )
}

/// `PixelStyle.bevel` raised on winman's tab block colour: a bar tab.
fn raised(element: gpui::Div) -> gpui::Div {
    element
        .relative()
        .border_1()
        .border_color(color(palette::OUTLINE))
        .bg(gradient(palette::RAISED_TOP, palette::RAISED_BOTTOM))
}

fn raised_edges(element: gpui::Div) -> gpui::Div {
    bevel_edges(
        element,
        color(palette::RAISED_LIGHT),
        color(palette::RAISED_DARK),
    )
}

/// The active bar tab: raised in winman's accent blue.
fn lit(element: gpui::Div) -> gpui::Div {
    bevel_edges(
        element
            .relative()
            .border_1()
            .border_color(color(palette::OUTLINE))
            .bg(gradient(palette::LIT_TOP, palette::LIT_BOTTOM)),
        color(palette::LIT_LIGHT),
        color(palette::LIT_DARK),
    )
}

fn sunken(element: gpui::Div) -> gpui::Div {
    element
        .relative()
        .border_1()
        .border_color(color(palette::OUTLINE))
        .bg(color(palette::SUNKEN))
}

fn sunken_edges(element: gpui::Div) -> gpui::Div {
    bevel_edges(
        element,
        color(palette::SUNKEN_DARK),
        color(palette::SUNKEN_LIGHT),
    )
}

/// The message screen of winman's bar: a sunken phosphor face with
/// scanlines every third pixel.
fn screen(element: gpui::Div) -> gpui::Div {
    bevel_edges(
        element
            .relative()
            .overflow_hidden()
            .border_1()
            .border_color(color(palette::OUTLINE))
            .bg(gradient(palette::SCREEN_TOP, palette::SCREEN_BOTTOM))
            .text_color(color(palette::AQUA)),
        color(palette::SCREEN_DARK),
        color(palette::SUNKEN_LIGHT),
    )
    .child(
        canvas(
            |_, _, _| (),
            |bounds: Bounds<gpui::Pixels>, _, window, _| {
                let mut y = bounds.top() + px(1.);
                while y < bounds.bottom() {
                    window.paint_quad(fill(
                        Bounds::new(point(bounds.left(), y), size(bounds.size.width, px(1.))),
                        color_alpha(palette::SCANLINE),
                    ));
                    y += px(3.);
                }
            },
        )
        .absolute()
        .inset_0(),
    )
}

/// A small raised label in a gruvbox colour: a ref on a commit, a file status.
fn chip(
    text: impl Into<SharedString>,
    foreground: u32,
    top: u32,
    bottom: u32,
    light: u32,
) -> gpui::Div {
    div()
        .relative()
        .flex_none()
        .px(px(5.))
        .h(px(17.))
        .flex()
        .items_center()
        .text_size(px(11.5))
        .border_1()
        .border_color(color(palette::OUTLINE))
        .bg(gradient(top, bottom))
        .text_color(color(foreground))
        .child(
            div()
                .absolute()
                .top_0()
                .left_0()
                .right_0()
                .h_px()
                .bg(color(light)),
        )
        .child(
            div()
                .absolute()
                .top_0()
                .left_0()
                .bottom_0()
                .w_px()
                .bg(color(light)),
        )
        .child(text.into())
}

fn ref_chip(label: &RefLabel) -> gpui::Div {
    match label.kind {
        RefKind::Head => chip(
            format!("✓ {}", label.name),
            palette::GREEN,
            0x4b5220,
            0x262a0e,
            0x7d8540,
        ),
        RefKind::Branch => chip(
            label.name.clone(),
            palette::GREEN,
            0x4b5220,
            0x262a0e,
            0x7d8540,
        ),
        RefKind::Remote => chip(
            label.name.clone(),
            palette::BLUE,
            0x3f5a70,
            0x1c2e3d,
            0x6d8aa0,
        ),
        RefKind::Tag => chip(
            label.name.clone(),
            palette::YELLOW,
            0x5a4a18,
            0x2c230a,
            0x8d7a3e,
        ),
    }
}

fn status_chip(status: FileStatus) -> gpui::Div {
    let (foreground, top, bottom, light) = match status {
        FileStatus::Modified | FileStatus::Renamed => {
            (palette::YELLOW, 0x5a4a18, 0x2c230a, 0x8d7a3e)
        }
        FileStatus::Added => (palette::GREEN, 0x4b5220, 0x262a0e, 0x7d8540),
        FileStatus::Deleted => (palette::RED, 0x5e2219, 0x2d0f0b, 0x93503f),
    };
    chip(status.letter(), foreground, top, bottom, light)
        .h(px(15.))
        .text_size(px(10.5))
}

fn title_bar(id: &'static str) -> gpui::Stateful<gpui::Div> {
    raised_edges(
        raised(div())
            .flex_none()
            .h(px(30.))
            .px(px(10.))
            .flex()
            .items_center()
            .gap(px(10.))
            .text_color(color(palette::TEXT)),
    )
    .id(id)
}

// ---------------------------------------------------------------------------
// rendering
// ---------------------------------------------------------------------------

impl WinmanGitView {
    fn render_commit_row(&self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(commit) = self.commits.get(index) else {
            return div().into_any_element();
        };
        let selected = index == self.selected_commit;
        let last = index + 1 == self.commits.len();
        let dim = if selected {
            palette::TEXT_SELECTED
        } else {
            palette::DIM
        };
        let graph = div()
            .relative()
            .flex_none()
            .w(px(34.))
            .h_full()
            .child(
                div()
                    .absolute()
                    .left(px(16.))
                    .w(px(2.))
                    .top(if index == 0 { px(13.) } else { px(0.) })
                    .when(last, |this| this.h(px(13.)))
                    .when(!last, |this| this.bottom_0())
                    .bg(color(palette::ORANGE)),
            )
            .child(
                div()
                    .absolute()
                    .left(px(12.))
                    .top(px(8.))
                    .size(px(10.))
                    .border_2()
                    .border_color(color(palette::ORANGE))
                    .bg(if selected {
                        color(palette::ORANGE)
                    } else {
                        color(palette::SUNKEN)
                    }),
            );
        let message = div()
            .flex_1()
            .min_w_0()
            .flex()
            .items_center()
            .gap(px(6.))
            .overflow_hidden()
            .children(commit.refs.iter().map(ref_chip))
            .child(
                div()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .when_some(commit.prefix.clone(), |this, prefix| {
                        this.child(
                            gpui::StyledText::new(format!("{prefix} {}", commit.subject))
                                .with_highlights([(
                                    0..prefix.len(),
                                    HighlightStyle {
                                        color: Some(color(palette::TEXT_BRIGHT)),
                                        ..Default::default()
                                    },
                                )]),
                        )
                    })
                    .when(commit.prefix.is_none(), |this| {
                        this.child(commit.subject.clone())
                    }),
            );
        let row = div()
            .id(("commit", index))
            .size_full()
            .flex()
            .items_center()
            .pr(px(10.))
            .text_color(color(if selected {
                palette::TEXT_BRIGHT
            } else {
                palette::TEXT
            }))
            .child(graph)
            .child(message)
            .child(
                div()
                    .flex_none()
                    .w(px(74.))
                    .pl(px(8.))
                    .whitespace_nowrap()
                    .text_color(color(dim))
                    .child(commit.short_sha.clone()),
            )
            .child(
                div()
                    .flex_none()
                    .w(px(100.))
                    .flex()
                    .justify_end()
                    .whitespace_nowrap()
                    .text_color(color(dim))
                    .child(format_short_time(commit.timestamp)),
            )
            .on_click(
                cx.listener(move |this, _: &ClickEvent, _, cx| this.select_commit(index, cx)),
            );
        // Every row the same height, the uniform list's measure: the selected
        // one's bevel border goes inside it.
        let container = if selected { lit(div()) } else { div() };
        container
            .h(px(COMMIT_ROW_HEIGHT))
            .w_full()
            .child(row)
            .into_any_element()
    }

    fn render_diff_row(&self, detail: &CommitDetail, index: usize, cx: &App) -> AnyElement {
        let Some(line) = detail
            .files
            .get(self.selected_file)
            .and_then(|file| file.lines.get(index))
        else {
            return div().into_any_element();
        };
        let number = |number: Option<u32>, hex: u32| {
            div()
                .flex_none()
                .w(px(44.))
                .pr(px(8.))
                .flex()
                .justify_end()
                .text_size(px(11.5))
                .text_color(color(hex))
                .child(number.map(|n| n.to_string()).unwrap_or_default())
        };
        let (background, sign, sign_color, number_color) = match line.kind {
            LineKind::Hunk => {
                return div()
                    .h(px(DIFF_ROW_HEIGHT))
                    .w_full()
                    .pl(px(106.))
                    .flex()
                    .items_center()
                    .bg(color(palette::HUNK))
                    .border_t_1()
                    .border_color(color(palette::SUNKEN_DARK))
                    .text_size(px(12.))
                    .text_color(color(palette::BLUE))
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .child(line.text.clone())
                    .into_any_element();
            }
            LineKind::Added => (
                Some(palette::ADDED_BACKGROUND),
                "+",
                palette::GREEN,
                palette::ADDED_NUMBER,
            ),
            LineKind::Removed => (
                Some(palette::REMOVED_BACKGROUND),
                "-",
                palette::RED,
                palette::REMOVED_NUMBER,
            ),
            LineKind::Context => (None, " ", palette::DIM, palette::STALE),
        };
        let syntax = cx.theme().syntax();
        let highlights: Vec<(Range<usize>, HighlightStyle)> = line
            .highlights
            .iter()
            .filter_map(|(range, id)| Some((range.clone(), *syntax.get(*id)?)))
            .collect();
        div()
            .h(px(DIFF_ROW_HEIGHT))
            .w_full()
            .flex()
            .items_center()
            .when_some(background, |this, background| {
                this.bg(color_alpha(background))
            })
            .child(number(line.old_number, number_color))
            .child(number(line.new_number, number_color))
            .child(
                div()
                    .flex_none()
                    .w(px(18.))
                    .text_color(color(sign_color))
                    .child(sign),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .text_color(color(palette::TEXT))
                    .child(StyledText::new(line.text.clone()).with_highlights(highlights)),
            )
            .into_any_element()
    }

    fn render_commits(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let repo_name = self.root.as_deref().map(project_name).unwrap_or_default();
        let branch = self
            .refs
            .head_branch
            .clone()
            .unwrap_or_else(|| "HEAD".into());
        let body: AnyElement = if let Some(error) = &self.error {
            div()
                .p(px(12.))
                .text_color(color(palette::RED))
                .child(error.clone())
                .into_any_element()
        } else if self.commits.is_empty() {
            div()
                .p(px(12.))
                .text_color(color(palette::DIM))
                .child(if self.loading {
                    "Läser historiken…"
                } else {
                    "Inga commits"
                })
                .into_any_element()
        } else {
            uniform_list(
                "winman-git-commits",
                self.commits.len(),
                cx.processor(|this, range: Range<usize>, _window, cx| {
                    range
                        .map(|index| this.render_commit_row(index, cx))
                        .collect::<Vec<_>>()
                }),
            )
            .size_full()
            .track_scroll(&self.commit_scroll)
            .into_any_element()
        };
        let _ = window;
        let fetch_text = match self.fetch_state {
            FetchState::Running => "● fetchar…".to_string(),
            FetchState::Failed => {
                format!("● fetch misslyckades {}", self.fetch_clock(false))
            }
            FetchState::Idle => format!(
                "● auto-fetch {} · nästa {} · read-only",
                self.fetch_clock(false),
                self.fetch_clock(true)
            ),
        };
        let mut upstream = branch.to_string();
        if let Some(track) = &self.refs.head_track {
            upstream.push_str(&format!(" {track}"));
        }
        if let Some(remote) = &self.refs.head_upstream {
            upstream.push_str(&format!(" · {remote}"));
        }
        div()
            .flex()
            .flex_col()
            .gap(px(4.))
            .h_full()
            .w(relative(0.36))
            .flex_none()
            .child(
                title_bar("winman-git-commits-title")
                    .child(repo_name)
                    .child(div().text_color(color(palette::DIM)).child("›"))
                    .child(branch)
                    .child(
                        div()
                            .ml_auto()
                            .text_color(color(palette::DIM))
                            .child(format!("{} commits", self.commits.len())),
                    ),
            )
            .child(sunken_edges(
                sunken(div())
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .child(body),
            ))
            .child(
                screen(div())
                    .flex_none()
                    .h(px(30.))
                    .px(px(10.))
                    .flex()
                    .items_center()
                    .text_size(px(12.))
                    .whitespace_nowrap()
                    .child(upstream)
                    .child(div().ml_auto().child(fetch_text)),
            )
    }

    fn fetch_clock(&self, next: bool) -> String {
        let Some(root) = &self.root else {
            return String::new();
        };
        let Some(at) = LAST_FETCH
            .lock()
            .ok()
            .and_then(|last| last.get(root).copied())
        else {
            return "--:--".into();
        };
        let now = OffsetDateTime::now_utc().unix_timestamp();
        let elapsed = at.elapsed().as_secs() as i64;
        let timestamp = if next {
            now - elapsed + FETCH_INTERVAL.as_secs() as i64
        } else {
            now - elapsed
        };
        format_clock(timestamp)
    }

    fn render_diff(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let commit = self.commits.get(self.selected_commit);
        let detail = self.selected_detail();
        let file = detail
            .as_ref()
            .and_then(|detail| detail.files.get(self.selected_file));

        let header = screen(div())
            .flex_none()
            .h(px(44.))
            .px(px(14.))
            .flex()
            .items_center()
            .gap(px(14.))
            .whitespace_nowrap()
            .when_some(commit, |this, commit| {
                let (author, timestamp, subject) = match &detail {
                    Some(detail) => (
                        detail.author.clone(),
                        detail.timestamp,
                        detail.subject.clone(),
                    ),
                    None => (
                        commit.author.clone(),
                        commit.timestamp,
                        match &commit.prefix {
                            Some(prefix) => format!("{prefix} {}", commit.subject).into(),
                            None => commit.subject.clone(),
                        },
                    ),
                };
                this.child(author)
                    .child(div().opacity(0.7).child(commit.short_sha.clone()))
                    .child(div().opacity(0.7).child(format_long_time(timestamp)))
                    .child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .text_color(color(0xb8f0c8))
                            .child(subject),
                    )
            });

        let file_bar = title_bar("winman-git-file-title")
            .text_size(px(13.))
            .when_some(file, |this, file| {
                let (directory, name) = match file.path.rsplit_once('/') {
                    Some((directory, name)) => (format!("{directory}/"), name.to_string()),
                    None => (String::new(), file.path.clone()),
                };
                this.child(status_chip(file.status))
                    .child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .flex()
                            .when_some(file.old_path.clone(), |this, old_path| {
                                this.child(
                                    div()
                                        .text_color(color(palette::DIM))
                                        .child(format!("{old_path} → ")),
                                )
                            })
                            .child(div().text_color(color(palette::DIM)).child(directory))
                            .child(div().text_color(color(palette::TEXT_BRIGHT)).child(name)),
                    )
                    .child(
                        div()
                            .ml_auto()
                            .flex_none()
                            .flex()
                            .gap(px(6.))
                            .child(
                                div()
                                    .text_color(color(palette::GREEN))
                                    .child(format!("+{}", file.added)),
                            )
                            .child(
                                div()
                                    .text_color(color(palette::RED))
                                    .child(format!("-{}", file.removed)),
                            )
                            .child(div().text_color(color(palette::DIM)).child(format!(
                                "· {}/{}",
                                self.selected_file + 1,
                                detail.as_ref().map_or(0, |detail| detail.files.len())
                            ))),
                    )
            });

        let body: AnyElement = match (&detail, file) {
            (Some(detail), Some(file)) if file.binary => {
                let _ = detail;
                div()
                    .p(px(12.))
                    .text_color(color(palette::DIM))
                    .child("Binärfil")
                    .into_any_element()
            }
            (Some(detail), Some(file)) => {
                let line_count = file.lines.len();
                let detail = detail.clone();
                uniform_list(
                    "winman-git-diff",
                    line_count,
                    cx.processor(move |this, range: Range<usize>, _window, cx| {
                        range
                            .map(|index| this.render_diff_row(&detail, index, cx))
                            .collect::<Vec<_>>()
                    }),
                )
                .size_full()
                .track_scroll(&self.diff_scroll)
                .into_any_element()
            }
            (Some(_), None) => div()
                .p(px(12.))
                .text_color(color(palette::DIM))
                .child("Inga ändrade filer")
                .into_any_element(),
            (None, _) => div().into_any_element(),
        };

        div()
            .flex()
            .flex_col()
            .gap(px(4.))
            .h_full()
            .flex_1()
            .min_w_0()
            .child(header)
            .child(file_bar)
            .child(sunken_edges(
                sunken(div())
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .bg(color(palette::CODE))
                    .child(body),
            ))
    }

    fn render_tree_row(&self, row: &TreeRow, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let detail = self.selected_detail();
        let indent = px(10. + row.depth as f32 * 14.);
        let base = div()
            .id(("tree", index))
            .size_full()
            .pl(indent)
            .pr(px(10.))
            .flex()
            .items_center()
            .gap(px(7.))
            .whitespace_nowrap()
            .overflow_hidden();
        match row.file_index {
            None => div()
                .h(px(TREE_ROW_HEIGHT))
                .flex_none()
                .child(
                    base.text_color(color(palette::TEXT))
                        .child(
                            div()
                                .text_size(px(10.))
                                .text_color(color(palette::DIM))
                                .child("▾"),
                        )
                        .child(div().text_color(color(palette::BLUE)).child("\u{f07b}"))
                        .child(row.name.clone()),
                )
                .into_any_element(),
            Some(file_index) => {
                let selected = file_index == self.selected_file;
                let status = detail
                    .as_ref()
                    .and_then(|detail| detail.files.get(file_index))
                    .map(|file| file.status)
                    .unwrap_or(FileStatus::Modified);
                let row = base
                    .text_color(color(if selected {
                        palette::TEXT_BRIGHT
                    } else {
                        palette::TEXT
                    }))
                    .child(status_chip(status))
                    .child(row.name.clone())
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.select_file(file_index, cx)
                    }));
                let container = if selected { lit(div()) } else { div() };
                container
                    .h(px(TREE_ROW_HEIGHT))
                    .flex_none()
                    .child(row)
                    .into_any_element()
            }
        }
    }

    fn render_ref_row(
        &self,
        id: (&'static str, usize),
        glyph: &'static str,
        glyph_color: u32,
        entry: &RefEntry,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let target = entry.target.clone();
        let row = div()
            .id(id)
            .size_full()
            .px(px(10.))
            .flex()
            .items_center()
            .gap(px(7.))
            .whitespace_nowrap()
            .overflow_hidden()
            .text_color(color(if entry.current {
                palette::TEXT_BRIGHT
            } else {
                palette::TEXT
            }))
            .child(
                div()
                    .flex_none()
                    .w(px(12.))
                    .flex()
                    .justify_center()
                    .text_color(color(if entry.current {
                        palette::GREEN
                    } else {
                        glyph_color
                    }))
                    .child(if entry.current { "✓" } else { glyph }),
            )
            .child(
                div()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .child(entry.name.clone()),
            )
            .when_some(entry.track.clone(), |this, track| {
                this.child(
                    div()
                        .ml_auto()
                        .flex_none()
                        .text_size(px(12.))
                        .text_color(color(palette::DIM))
                        .child(track),
                )
            })
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.select_sha(&target, cx)));
        let container = if entry.current { lit(div()) } else { div() };
        container
            .h(px(TREE_ROW_HEIGHT))
            .flex_none()
            .child(row)
            .into_any_element()
    }

    fn section_collapsed(&self, label: &'static str, count: usize) -> bool {
        const FOLDED_FROM: usize = 12;
        self.collapsed
            .get(label)
            .copied()
            .unwrap_or(count >= FOLDED_FROM && label != "Branches" && label != "Worktrees")
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let file_count = self
            .selected_detail()
            .map_or(0, |detail| detail.files.len());
        let tree_rows: Vec<AnyElement> = self
            .tree
            .iter()
            .enumerate()
            .map(|(index, row)| self.render_tree_row(row, index, cx))
            .collect();

        let section =
            |label: &'static str, count: usize, collapsed: bool, cx: &mut Context<Self>| {
                div()
                    .id(label)
                    .flex_none()
                    .px(px(10.))
                    .pt(px(8.))
                    .pb(px(3.))
                    .flex()
                    .gap(px(6.))
                    .text_size(px(12.))
                    .text_color(color(palette::DIM))
                    .cursor_pointer()
                    .child(if collapsed { "▸" } else { "▾" })
                    .child(label)
                    .when(collapsed, |this| this.child(format!("({count})")))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        let collapsed = this.section_collapsed(label, count);
                        this.collapsed.insert(label, !collapsed);
                        cx.notify();
                    }))
            };
        let mut refs: Vec<AnyElement> = Vec::new();
        let groups: [(
            &'static str,
            &'static str,
            &'static str,
            u32,
            &Vec<RefEntry>,
        ); 5] = [
            (
                "Worktrees",
                "worktree",
                "\u{f07b}",
                palette::BLUE,
                &self.refs.worktrees,
            ),
            (
                "Branches",
                "branch",
                "\u{e0a0}",
                palette::DIM,
                &self.refs.branches,
            ),
            (
                "Remotes",
                "remote",
                "\u{e0a0}",
                palette::BLUE,
                &self.refs.remotes,
            ),
            ("Tags", "tag", "◆", palette::YELLOW, &self.refs.tags),
            ("Stashes", "stash", "▤", palette::DIM, &self.refs.stashes),
        ];
        for (label, id, glyph, glyph_color, entries) in groups {
            if entries.is_empty() {
                continue;
            }
            let collapsed = self.section_collapsed(label, entries.len());
            refs.push(section(label, entries.len(), collapsed, cx).into_any_element());
            if collapsed {
                continue;
            }
            for (index, entry) in entries.iter().enumerate() {
                refs.push(self.render_ref_row((id, index), glyph, glyph_color, entry, cx));
            }
        }

        let close_button = raised_edges(
            raised(div())
                .size(px(20.))
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(12.))
                .text_color(color(palette::TEXT)),
        )
        .id("winman-git-close")
        .cursor_pointer()
        .child("✕")
        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| this.close(window, cx)));

        div()
            .flex()
            .flex_col()
            .gap(px(4.))
            .h_full()
            .w(px(330.))
            .flex_none()
            .child(
                title_bar("winman-git-files-title")
                    .pr(px(5.))
                    .child("Filer")
                    .child(
                        div()
                            .text_color(color(palette::DIM))
                            .child(file_count.to_string()),
                    )
                    .child(div().ml_auto().child(close_button)),
            )
            .child(sunken_edges(
                sunken(div())
                    .h(relative(0.38))
                    .flex_none()
                    .overflow_hidden()
                    .child(
                        div()
                            .id("winman-git-tree")
                            .size_full()
                            .py(px(3.))
                            .flex()
                            .flex_col()
                            .overflow_y_scroll()
                            .children(tree_rows),
                    ),
            ))
            .child(title_bar("winman-git-refs-title").child("Refs"))
            .child(sunken_edges(
                sunken(div()).flex_1().min_h_0().overflow_hidden().child(
                    div()
                        .id("winman-git-refs")
                        .size_full()
                        .pb(px(6.))
                        .flex()
                        .flex_col()
                        .overflow_y_scroll()
                        .children(refs),
                ),
            ))
    }
}

impl Render for WinmanGitView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let font: Font = ThemeSettings::get_global(cx).buffer_font.clone();
        div()
            .id("winman-git-view")
            .key_context("WinmanGitView")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::handle_key_down))
            .size_full()
            .p(px(5.))
            .flex()
            .gap(px(5.))
            .font(font)
            .text_size(px(13.))
            .text_color(color(palette::TEXT))
            .bg(gradient(
                palette::BACKGROUND_TOP,
                palette::BACKGROUND_BOTTOM,
            ))
            .child(self.render_commits(window, cx))
            .child(self.render_diff(cx))
            .child(self.render_sidebar(cx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_patch() {
        let patch = "diff --git a/src/a.rs b/src/a.rs\n\
index 1..2 100644\n\
--- a/src/a.rs\n\
+++ b/src/a.rs\n\
@@ -10,3 +10,4 @@ fn main()\n \
 keep\n\
-old\n\
+new\n\
+-- not a header\n \
 tail\n\
diff --git a/b.txt b/b.txt\n\
new file mode 100644\n\
--- /dev/null\n\
+++ b/b.txt\n\
@@ -0,0 +1 @@\n\
+hello\n";
        let files = parse_patch(patch);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path, "src/a.rs");
        assert_eq!((files[0].added, files[0].removed), (2, 1));
        let numbers: Vec<_> = files[0]
            .lines
            .iter()
            .map(|line| (line.old_number, line.new_number))
            .collect();
        assert_eq!(
            numbers,
            vec![
                (None, None),
                (Some(10), Some(10)),
                (Some(11), None),
                (None, Some(11)),
                (None, Some(12)),
                (Some(12), Some(13)),
            ]
        );
        assert!(files[1].status == FileStatus::Added);
        assert_eq!(files[1].lines.len(), 2);
    }

    #[test]
    fn parses_decorations() {
        let remotes = vec!["origin".to_string()];
        let commit = parse_commit(
            "abc\x1fab\x1fOlof\x1f100\x1fHEAD -> main, origin/main, origin/HEAD, tag: v1\x1fBaren: gröna skärmen",
            &remotes,
        )
        .expect("commit");
        let kinds: Vec<_> = commit
            .refs
            .iter()
            .map(|r| (r.name.to_string(), r.kind))
            .collect();
        assert!(
            kinds
                == vec![
                    ("main".to_string(), RefKind::Head),
                    ("origin/main".to_string(), RefKind::Remote),
                    ("v1".to_string(), RefKind::Tag),
                ]
        );
        assert_eq!(commit.prefix.as_deref(), Some("Baren:"));
        assert_eq!(commit.subject.as_ref(), "gröna skärmen");
    }

    #[test]
    fn names_the_project() {
        assert_eq!(
            project_name(Path::new("/dev/winman-mac/worktrees/main")),
            "winman-mac"
        );
        assert_eq!(project_name(Path::new("/dev/goals")), "goals");
    }

    #[test]
    fn formats_tracking() {
        assert_eq!(
            format_track("ahead 1, behind 139").as_deref(),
            Some("139↓ 1↑")
        );
        assert_eq!(format_track("").as_deref(), None);
    }
}
