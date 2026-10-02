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
    HighlightStyle, Hsla, KeyDownEvent, PathBuilder, Pixels, Point, ScrollStrategy, SharedString,
    StyledText, Subscription, Task, UniformListScrollHandle, WeakEntity, Window, canvas, div, fill,
    linear_color_stop, linear_gradient, point, prelude::*, px, relative, rgb, rgba, size,
    uniform_list,
};
use git::{GitHostingProviderRegistry, GitRemote, parse_git_remote_url};
use language::{HighlightId, LanguageRegistry, Rope};
use settings::Settings as _;
use crate::commit_tooltip::CommitAvatar;
use theme::ActiveTheme as _;
use theme_settings::ThemeSettings;
use time::{OffsetDateTime, UtcOffset};
use ui::Tooltip;
use util::ResultExt as _;
use workspace::{MultiWorkspace, MultiWorkspaceEvent, Workspace};

const FETCH_INTERVAL: Duration = Duration::from_secs(5 * 60);
const REFRESH_INTERVAL: Duration = Duration::from_secs(2);
const LOG_LIMIT: &str = "3000";
const DETAIL_CACHE_SIZE: usize = 64;
const MAX_HIGHLIGHTED_LINES: usize = 20_000;
const MAX_LINE_LENGTH: usize = 1_000;

const COMMIT_ROW_HEIGHT: f32 = 28.;
const SIDEBAR_WIDTH: f32 = 330.;
const MIN_COMMIT_LIST_WIDTH: f32 = 240.;
const MIN_DIFF_WIDTH: f32 = 320.;

/// The commit list's width as last dragged, shared by every view so it
/// survives closing and reopening. `None` until first dragged: a share of
/// the window then.
static COMMIT_LIST_WIDTH: Mutex<Option<Pixels>> = Mutex::new(None);

struct DraggedSplit;
const AVATAR_SIZE: f32 = 18.;
const LANE_WIDTH: f32 = 14.;
/// Lanes beyond this are clipped rather than pushing the messages away.
const MAX_VISIBLE_LANES: usize = 16;
/// With more remote branches than this (a fork of a big project mirrors
/// thousands), the graph only follows the ones local branches track.
const MAX_REMOTE_BRANCHES: usize = 300;
const DIFF_ROW_HEIGHT: f32 = 20.;
const TREE_ROW_HEIGHT: f32 = 24.;

/// When each repository was last fetched, across views, so the schedule in
/// `poll` does not fetch again right after the fetch every open makes.
static LAST_FETCH: LazyLock<Mutex<HashMap<PathBuf, Instant>>> =
    LazyLock::new(|| Mutex::new(HashMap::default()));

/// How often every workspace's repository is checked and, when a ref moved,
/// loaded again into `REPOSITORY_CACHE`.
const PREWARM_INTERVAL: Duration = Duration::from_secs(30);

/// A repository as the view last saw it, or as `prewarm` loaded it, so the
/// view comes up with its history at once instead of waiting on git.
#[derive(Clone)]
struct CachedRepository {
    commits: Arc<Vec<CommitRow>>,
    refs: Refs,
    signature: String,
    /// The newest commit's detail, the one the view selects when it opens.
    head_detail: Option<(SharedString, Arc<CommitDetail>)>,
}

static REPOSITORY_CACHE: LazyLock<Mutex<HashMap<PathBuf, CachedRepository>>> =
    LazyLock::new(|| Mutex::new(HashMap::default()));

fn cached_repository(root: &Path) -> Option<CachedRepository> {
    REPOSITORY_CACHE.lock().ok()?.get(root).cloned()
}

/// Stores a freshly loaded history, keeping the head detail when the newest
/// commit is still the same one.
fn store_repository(root: &Path, commits: Arc<Vec<CommitRow>>, refs: Refs, signature: String) {
    let Ok(mut cache) = REPOSITORY_CACHE.lock() else {
        return;
    };
    let head = commits.first().map(|commit| commit.sha.clone());
    let head_detail = cache
        .get(root)
        .and_then(|cached| cached.head_detail.clone())
        .filter(|(sha, _)| Some(sha) == head.as_ref());
    cache.insert(
        root.to_path_buf(),
        CachedRepository {
            commits,
            refs,
            signature,
            head_detail,
        },
    );
}

fn store_head_detail(root: &Path, sha: &SharedString, detail: &Arc<CommitDetail>) {
    let Ok(mut cache) = REPOSITORY_CACHE.lock() else {
        return;
    };
    if let Some(cached) = cache.get_mut(root)
        && cached
            .commits
            .first()
            .is_some_and(|commit| &commit.sha == sha)
    {
        cached.head_detail = Some((sha.clone(), detail.clone()));
    }
}

/// Keeps every workspace's history in `REPOSITORY_CACHE` in the unified
/// window, where winman opens the view: first a few seconds after start, then
/// every `PREWARM_INTERVAL`, loading a repository again only when a ref moved.
pub fn init(cx: &mut App) {
    cx.spawn(async move |cx| {
        cx.background_executor().timer(Duration::from_secs(5)).await;
        loop {
            let roots = cx.update(|cx| {
                if !workspace::unified_window_enabled(cx) {
                    return Vec::new();
                }
                let mut roots: Vec<(PathBuf, Arc<LanguageRegistry>)> = Vec::new();
                for window in cx.windows() {
                    let Some(window) = window.downcast::<MultiWorkspace>() else {
                        continue;
                    };
                    let Ok(multi_workspace) = window.read(cx) else {
                        continue;
                    };
                    for workspace in multi_workspace.workspaces() {
                        if let Some(active) = active_root(workspace, cx)
                            && !roots.iter().any(|(root, _)| *root == active.0)
                        {
                            roots.push(active);
                        }
                    }
                }
                roots
            });
            for (root, languages) in roots {
                prewarm(root, languages, cx).await.log_err();
            }
            cx.background_executor().timer(PREWARM_INTERVAL).await;
        }
    })
    .detach();
}

async fn prewarm(
    root: PathBuf,
    languages: Arc<LanguageRegistry>,
    cx: &mut gpui::AsyncApp,
) -> Result<()> {
    let cached = cached_repository(&root);
    let signature = cx
        .background_spawn({
            let root = root.clone();
            async move { ref_signature(&root).await }
        })
        .await?;
    let up_to_date = cached.as_ref().is_some_and(|cached| {
        cached.signature == signature && (cached.head_detail.is_some() || cached.commits.is_empty())
    });
    if up_to_date {
        return Ok(());
    }
    if cached
        .as_ref()
        .is_none_or(|cached| cached.signature != signature)
    {
        let (commits, refs, signature) = cx
            .background_spawn({
                let root = root.clone();
                async move { load_repository(&root).await }
            })
            .await?;
        store_repository(&root, Arc::new(commits), refs, signature);
    }
    let head = cached_repository(&root)
        .filter(|cached| cached.head_detail.is_none())
        .and_then(|cached| cached.commits.first().map(|commit| commit.sha.clone()));
    if let Some(sha) = head {
        let detail = load_detail(root.clone(), sha.clone(), Some(languages), cx).await?;
        store_head_detail(&root, &sha, &Arc::new(detail));
    }
    Ok(())
}

/// The git view's colours. Amiga is winman's palette (`Theme.swift`, gruvbox)
/// and `PixelStyle.bevel` on its tab block colour, precomputed; Mist and
/// gruvbox-dark are the vector skins', with flat rounded panels instead of
/// bevels.
struct Palette {
    /// Flat rounded panels (the vector skins) instead of bevels and scanlines.
    vector: bool,
    /// The border round the whole view under a vector skin: the skin's window
    /// frame line.
    frame: u32,
    outline: u32,
    text: u32,
    text_bright: u32,
    text_selected: u32,
    dim: u32,
    stale: u32,
    red: u32,
    green: u32,
    yellow: u32,
    blue: u32,
    aqua: u32,

    background_top: u32,
    background_bottom: u32,

    raised_top: u32,
    raised_bottom: u32,
    raised_light: u32,
    raised_dark: u32,

    lit_top: u32,
    lit_bottom: u32,
    lit_light: u32,
    lit_dark: u32,

    sunken: u32,
    sunken_dark: u32,
    sunken_light: u32,
    code: u32,
    hunk: u32,

    screen_top: u32,
    screen_bottom: u32,
    screen_dark: u32,
    screen_subject: u32,

    added_background: u32,
    removed_background: u32,
    added_number: u32,
    removed_number: u32,
    scanline: u32,

    /// Chip text, top, bottom and top-left light for a green, blue, yellow and
    /// red label.
    chip_green: (u32, u32, u32, u32),
    chip_blue: (u32, u32, u32, u32),
    chip_yellow: (u32, u32, u32, u32),
    chip_red: (u32, u32, u32, u32),

    lanes: [u32; 7],
}

const AMIGA: Palette = Palette {
    vector: false,
    frame: 0x0a0a0c,
    outline: 0x0a0a0c,
    text: 0xbdae93,
    text_bright: 0xebdbb2,
    text_selected: 0xd5c4a1,
    dim: 0x928374,
    stale: 0x665c54,
    red: 0xfb4934,
    green: 0xb8bb26,
    yellow: 0xfabd2f,
    blue: 0x83a598,
    aqua: 0x8ec07c,

    background_top: 0x2f2f2f,
    background_bottom: 0x171717,

    raised_top: 0x474747,
    raised_bottom: 0x1f1f1f,
    raised_light: 0x727272,
    raised_dark: 0x151515,

    lit_top: 0x5c768c,
    lit_bottom: 0x20374b,
    lit_light: 0x778d9f,
    lit_dark: 0x101c27,

    sunken: 0x181818,
    sunken_dark: 0x0b0b0b,
    sunken_light: 0x4a4a4a,
    code: 0x1b1b1b,
    hunk: 0x232323,

    screen_top: 0x0c1a12,
    screen_bottom: 0x07110b,
    screen_dark: 0x050505,
    screen_subject: 0xb8f0c8,

    added_background: 0xb8bb2621,
    removed_background: 0xfb493421,
    added_number: 0x7c7f2a,
    removed_number: 0x8a3a30,
    scanline: 0x00000047,

    chip_green: (0xb8bb26, 0x4b5220, 0x262a0e, 0x7d8540),
    chip_blue: (0x83a598, 0x3f5a70, 0x1c2e3d, 0x6d8aa0),
    chip_yellow: (0xfabd2f, 0x5a4a18, 0x2c230a, 0x8d7a3e),
    chip_red: (0xfb4934, 0x5e2219, 0x2d0f0b, 0x93503f),

    lanes: [
        0xfe8019, 0x8ec07c, 0xfabd2f, 0x83a598, 0xfb4934, 0xb8bb26, 0xd3869b,
    ],
};

// Mist's skin colours (`winman.json`) and Solarized accents, which read on its
// light panels as they do in its terminal.
const MIST: Palette = Palette {
    vector: true,
    frame: 0xb5bab9,
    outline: 0xd0d1cb,
    text: 0x3e5667,
    text_bright: 0x1f3342,
    text_selected: 0x1f3342,
    dim: 0x7d8b93,
    stale: 0xa7aeb0,
    red: 0xdc322f,
    green: 0x859900,
    yellow: 0xb58900,
    blue: 0x268bd2,
    aqua: 0x2aa198,

    background_top: 0xe4e3de,
    background_bottom: 0xe4e3de,

    raised_top: 0xefede7,
    raised_bottom: 0xefede7,
    raised_light: 0xd6d6d0,
    raised_dark: 0xd6d6d0,

    lit_top: 0x9ec5e5,
    lit_bottom: 0x9ec5e5,
    lit_light: 0x7b99b2,
    lit_dark: 0x7b99b2,

    sunken: 0xf4f2eb,
    sunken_dark: 0xd0d1cb,
    sunken_light: 0xd0d1cb,
    code: 0xf7f6f1,
    hunk: 0xebe9e2,

    screen_top: 0xefede7,
    screen_bottom: 0xefede7,
    screen_dark: 0xd6d6d0,
    screen_subject: 0x1f3342,

    added_background: 0x85990024,
    removed_background: 0xdc322f1c,
    added_number: 0x8a9a4a,
    removed_number: 0xc0786c,
    scanline: 0x00000000,

    chip_green: (0x5b6a00, 0xe6ebd2, 0xe6ebd2, 0xc4cf98),
    chip_blue: (0x1d6aa3, 0xdce9f3, 0xdce9f3, 0xa9c8e0),
    chip_yellow: (0x876600, 0xf2e8c8, 0xf2e8c8, 0xdcc98a),
    chip_red: (0xb02a27, 0xf5dcd6, 0xf5dcd6, 0xe0aaa0),

    lanes: [
        0xcb4b16, 0x2aa198, 0xb58900, 0x268bd2, 0xdc322f, 0x859900, 0x6c71c4,
    ],
};

// gruvbox-dark's skin colours (`winman.json`, winman's `MistPalette.gruvboxDark`)
// and Gruvbox's bright accents. The selected row is drawn like the bar's active
// tab: the chassis tinted 30 % toward the blue collection, a ring at 75 %.
const GRUVBOX_DARK: Palette = Palette {
    vector: true,
    frame: 0x3c3836,
    outline: 0x3a3733,
    text: 0xebdbb2,
    text_bright: 0xfbf1c7,
    text_selected: 0xd5c4a1,
    dim: 0xa89984,
    stale: 0x7c6f64,
    red: 0xfb4934,
    green: 0xb8bb26,
    yellow: 0xfabd2f,
    blue: 0x83a598,
    aqua: 0x8ec07c,

    background_top: 0x202323,
    background_bottom: 0x202323,

    raised_top: 0x252827,
    raised_bottom: 0x252827,
    raised_light: 0x3a3733,
    raised_dark: 0x3a3733,

    lit_top: 0x334248,
    lit_bottom: 0x334248,
    lit_light: 0x4e7081,
    lit_dark: 0x4e7081,

    sunken: 0x1d2021,
    sunken_dark: 0x3e3a35,
    sunken_light: 0x3e3a35,
    code: 0x1b1e1f,
    hunk: 0x282828,

    screen_top: 0x1b1e1f,
    screen_bottom: 0x1b1e1f,
    screen_dark: 0x3e3a35,
    screen_subject: 0xfbf1c7,

    added_background: 0xb8bb2621,
    removed_background: 0xfb493421,
    added_number: 0x7c7f2a,
    removed_number: 0x8a3a30,
    scanline: 0x00000000,

    chip_green: (0xb8bb26, 0x373a23, 0x373a23, 0x555824),
    chip_blue: (0x83a598, 0x2f3635, 0x2f3635, 0x43504c),
    chip_yellow: (0xfabd2f, 0x413a25, 0x413a25, 0x6c5927),
    chip_red: (0xfb4934, 0x412926, 0x412926, 0x6d3029),

    lanes: [
        0xfe8019, 0x8ec07c, 0xfabd2f, 0x83a598, 0xfb4934, 0xb8bb26, 0xd3869b,
    ],
};

thread_local! {
    /// The palette this frame draws with; set at the top of `render`, so the
    /// rows a list renders later in the same frame agree with it.
    static ACTIVE_PALETTE: std::cell::Cell<&'static Palette> =
        const { std::cell::Cell::new(&AMIGA) };
}

fn palette() -> &'static Palette {
    ACTIVE_PALETTE.with(|palette| palette.get())
}

/// The palette for winman's current look: a vector skin's (Mist or
/// gruvbox-dark) when one frames the editor, Amiga otherwise.
fn select_palette(cx: &App) -> &'static Palette {
    if !ui::has_winman_skin("editor_window", cx) {
        return &AMIGA;
    }
    match ui::winman_bar_theme(cx) {
        Some("gruvbox-dark") => &GRUVBOX_DARK,
        _ => &MIST,
    }
}

fn lane_color(index: usize) -> Hsla {
    color(palette().lanes[index % palette().lanes.len()])
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

/// Closes the git view if it is up, leaving the keyboard alone: winman sends
/// this right before it moves the keyboard itself, over another socket, so
/// handing it back to where it was could land after winman's move and undo it.
pub fn close_if_open(
    multi_workspace: &mut MultiWorkspace,
    window: &mut Window,
    cx: &mut Context<MultiWorkspace>,
) {
    if is_open(multi_workspace) {
        multi_workspace.set_full_overlay(None, window, cx);
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
    email: SharedString,
    /// The newest commit by the same author: avatar lookups are keyed by
    /// commit, so every row of one author shares a single lookup.
    avatar_sha: SharedString,
    timestamp: i64,
    refs: Vec<RefLabel>,
    prefix: Option<SharedString>,
    subject: SharedString,
    body: SharedString,
    parents: Vec<SharedString>,
    graph: GraphRow,
}

impl CommitRow {
    fn full_subject(&self) -> SharedString {
        match &self.prefix {
            Some(prefix) => format!("{prefix} {}", self.subject).into(),
            None => self.subject.clone(),
        }
    }

    fn full_message(&self) -> SharedString {
        if self.body.is_empty() {
            self.full_subject()
        } else {
            format!("{}\n\n{}", self.full_subject(), self.body).into()
        }
    }
}

/// A line in one half of a commit row, from lane `from` at the half's top to
/// lane `to` at its bottom.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct GraphEdge {
    from: usize,
    to: usize,
    color: usize,
}

/// A commit's slice of the branch graph: its node, the lines coming in from
/// the row above (ending at the node's middle) and those going out below.
#[derive(Clone, Default, Debug)]
struct GraphRow {
    lane: usize,
    color: usize,
    top: Vec<GraphEdge>,
    bottom: Vec<GraphEdge>,
}

impl GraphRow {
    fn width(&self) -> usize {
        self.top
            .iter()
            .chain(&self.bottom)
            .map(|edge| edge.from.max(edge.to))
            .chain([self.lane])
            .max()
            .unwrap_or(0)
            + 1
    }
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
    /// The remote the hosting provider (and so the avatars) comes from,
    /// picked the way the git panel picks it: `upstream`, else `origin`.
    remote_url: Option<SharedString>,
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
    /// The selected commit's whole message in place of its diff.
    show_message: bool,
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

        // Every open fetches straight away. The history on screen does not wait
        // for it: it comes from the cache, and the fetch reloads it when done.
        if let Some((root, _)) = &active
            && let Ok(mut last) = LAST_FETCH.lock()
        {
            last.remove(root);
        }

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
            show_message: false,
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
        match self.root.as_deref().and_then(cached_repository) {
            Some(cached) => {
                self.commits = cached.commits;
                self.refs = cached.refs;
                self.ref_signature = Some(cached.signature);
                if let Some((sha, detail)) = cached.head_detail {
                    self.details.insert(sha.clone(), detail);
                    self.detail_order.push(sha);
                }
                self.load_selected_detail(cx);
                // The cache may be up to a prewarm interval old.
                self.check_for_changes(cx);
            }
            None => self.reload(cx),
        }
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
                        let commits = Arc::new(commits);
                        store_repository(&root, commits.clone(), refs.clone(), signature.clone());
                        this.commits = commits;
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
        self.check_for_changes(cx);
    }

    /// Reloads when the refs differ from the ones the history was loaded with.
    fn check_for_changes(&mut self, cx: &mut Context<Self>) {
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
                        let detail = Arc::new(detail);
                        if let Some(root) = this.root.as_deref() {
                            store_head_detail(root, &sha, &detail);
                        }
                        this.details.insert(sha.clone(), detail);
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

    fn toggle_message(&mut self, cx: &mut Context<Self>) {
        self.show_message = !self.show_message;
        cx.notify();
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
            ("m", false) => self.toggle_message(cx),
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
    let remote_branches: Vec<String> = git(
        root,
        &["for-each-ref", "--format=%(refname)", "refs/remotes"],
    )
    .await?
    .lines()
    .filter(|name| !name.ends_with("/HEAD"))
    .map(str::to_string)
    .collect();
    let mut log_args = vec![
        "log",
        "-n",
        LOG_LIMIT,
        "--no-color",
        "--date-order",
        "--format=%H%x1f%h%x1f%an%x1f%ae%x1f%at%x1f%D%x1f%P%x1f%s%x1f%b%x1e",
        "HEAD",
        "--branches",
    ];
    let upstreams;
    if remote_branches.len() <= MAX_REMOTE_BRANCHES {
        log_args.push("--remotes");
    } else {
        upstreams = git(
            root,
            &["for-each-ref", "--format=%(upstream)", "refs/heads"],
        )
        .await?;
        log_args.extend(
            upstreams
                .lines()
                .filter(|upstream| remote_branches.iter().any(|name| name == upstream)),
        );
    }
    log_args.push("--");
    let log = git(root, &log_args);
    let (log, refs) = futures::join!(log, load_refs(root, &remotes));
    // A repository without commits has no HEAD to log.
    let mut commits: Vec<CommitRow> = log
        .unwrap_or_default()
        .split('\x1e')
        .filter_map(|record| parse_commit(record.trim_start_matches('\n'), &remotes))
        .collect();
    let mut newest_by_email: HashMap<SharedString, SharedString> = HashMap::default();
    for commit in &mut commits {
        commit.avatar_sha = newest_by_email
            .entry(commit.email.clone())
            .or_insert_with(|| commit.sha.clone())
            .clone();
    }
    layout_graph(&mut commits);
    Ok((commits, refs?, signature))
}

/// Assigns every commit a lane, newest first: a commit takes the leftmost
/// lane waiting for it (the others bend into it: where a branch forked off),
/// its first parent always continues in that lane and colour so a line stays
/// straight, and further parents (merged branches) get a lane of their own
/// unless one is already waiting for them. Lanes are reused once free, never
/// shifted.
fn layout_graph(commits: &mut [CommitRow]) {
    let mut lanes: Vec<Option<(SharedString, usize)>> = Vec::new();
    let mut next_color = 0;
    let mut new_color = || {
        let color = next_color;
        next_color += 1;
        color
    };
    fn free_lane(lanes: &mut Vec<Option<(SharedString, usize)>>) -> usize {
        match lanes.iter().position(Option::is_none) {
            Some(index) => index,
            None => {
                lanes.push(None);
                lanes.len() - 1
            }
        }
    }
    fn waiting_for(lanes: &[Option<(SharedString, usize)>], sha: &SharedString) -> Option<usize> {
        lanes
            .iter()
            .position(|lane| lane.as_ref().is_some_and(|(waiting, _)| waiting == sha))
    }

    for commit in commits.iter_mut() {
        let mut row = GraphRow::default();
        let (lane, color) = match waiting_for(&lanes, &commit.sha) {
            Some(index) => (
                index,
                lanes
                    .get(index)
                    .and_then(|lane| lane.as_ref())
                    .map_or(0, |(_, color)| *color),
            ),
            None => (free_lane(&mut lanes), new_color()),
        };
        row.lane = lane;
        row.color = color;

        for (index, entry) in lanes.iter_mut().enumerate() {
            let Some((sha, lane_color)) = entry else {
                continue;
            };
            if *sha == commit.sha {
                row.top.push(GraphEdge {
                    from: index,
                    to: lane,
                    color: *lane_color,
                });
                *entry = None;
            } else {
                let edge = GraphEdge {
                    from: index,
                    to: index,
                    color: *lane_color,
                };
                row.top.push(edge);
                row.bottom.push(edge);
            }
        }

        for (position, parent) in commit.parents.iter().enumerate() {
            let waiting = if position == 0 {
                None
            } else {
                waiting_for(&lanes, parent)
            };
            let (target, target_color) = match waiting {
                Some(index) => (
                    index,
                    lanes
                        .get(index)
                        .and_then(|lane| lane.as_ref())
                        .map_or(0, |(_, color)| *color),
                ),
                None => {
                    let (index, lane_color) = if position == 0 {
                        (lane, color)
                    } else {
                        (free_lane(&mut lanes), new_color())
                    };
                    if let Some(entry) = lanes.get_mut(index) {
                        *entry = Some((parent.clone(), lane_color));
                    }
                    (index, lane_color)
                }
            };
            row.bottom.push(GraphEdge {
                from: lane,
                to: target,
                color: target_color,
            });
        }

        while lanes.last().is_some_and(Option::is_none) {
            lanes.pop();
        }
        commit.graph = row;
    }
}

fn parse_commit(record: &str, remotes: &[String]) -> Option<CommitRow> {
    let mut fields = record.split('\x1f');
    let sha = fields.next()?.trim();
    if sha.is_empty() {
        return None;
    }
    let short_sha = fields.next()?;
    let author = fields.next()?;
    let email = fields.next()?;
    let timestamp = fields.next()?.parse().unwrap_or(0);
    let decorations = fields.next()?;
    let parents = fields
        .next()?
        .split_whitespace()
        .map(|parent| SharedString::from(parent.to_string()))
        .collect();
    let subject = fields.next().unwrap_or_default();
    let body = fields.next().unwrap_or_default().trim();

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
        email: email.to_string().into(),
        avatar_sha: sha.to_string().into(),
        timestamp,
        refs,
        prefix,
        subject: subject.to_string().into(),
        body: body.to_string().into(),
        parents,
        graph: GraphRow::default(),
    })
}

struct MessageTooltip {
    message: SharedString,
}

impl Render for MessageTooltip {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let font: Font = ThemeSettings::get_global(cx).buffer_font.clone();
        ui::tooltip_container(cx, |this, _| {
            this.child(
                div()
                    .max_w(px(560.))
                    .font(font)
                    .text_size(px(12.))
                    .child(self.message.clone()),
            )
        })
    }
}

fn message_tooltip(message: SharedString) -> impl Fn(&mut Window, &mut App) -> gpui::AnyView {
    move |_, cx| {
        cx.new(|_| MessageTooltip {
            message: message.clone(),
        })
        .into()
    }
}

fn initials(name: &str) -> String {
    name.split_whitespace()
        .filter_map(|word| word.chars().next())
        .take(2)
        .flat_map(char::to_uppercase)
        .collect()
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
    let remote = ["upstream", "origin"]
        .into_iter()
        .find(|name| remotes.iter().any(|remote| remote == name))
        .or_else(|| remotes.first().map(String::as_str));
    if let Some(remote) = remote {
        refs.remote_url = git(root, &["remote", "get-url", remote])
            .await
            .log_err()
            .map(|url| url.trim().to_string().into());
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
                    "--format=%an%x1f%at%x1e",
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
    if palette().vector {
        return element;
    }
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
        .when(palette().vector, |this| this.rounded(px(6.)))
        .border_1()
        .border_color(color(palette().outline))
        .bg(gradient(palette().raised_top, palette().raised_bottom))
}

fn raised_edges(element: gpui::Div) -> gpui::Div {
    bevel_edges(
        element,
        color(palette().raised_light),
        color(palette().raised_dark),
    )
}

/// The active bar tab: raised in winman's accent blue.
fn lit(element: gpui::Div) -> gpui::Div {
    bevel_edges(
        element
            .relative()
            .when(palette().vector, |this| {
                this.rounded(px(4.))
                    .border_color(color(palette().lit_light))
            })
            .border_1()
            .when(!palette().vector, |this| {
                this.border_color(color(palette().outline))
            })
            .bg(gradient(palette().lit_top, palette().lit_bottom)),
        color(palette().lit_light),
        color(palette().lit_dark),
    )
}

fn sunken(element: gpui::Div) -> gpui::Div {
    element
        .relative()
        .when(palette().vector, |this| this.rounded(px(6.)))
        .border_1()
        .border_color(color(palette().outline))
        .bg(color(palette().sunken))
}

fn sunken_edges(element: gpui::Div) -> gpui::Div {
    bevel_edges(
        element,
        color(palette().sunken_dark),
        color(palette().sunken_light),
    )
}

/// The message screen of winman's bar: a sunken phosphor face with
/// scanlines every third pixel.
fn screen(element: gpui::Div) -> gpui::Div {
    bevel_edges(
        element
            .relative()
            .overflow_hidden()
            .when(palette().vector, |this| this.rounded(px(6.)))
            .border_1()
            .border_color(color(palette().outline))
            .bg(gradient(palette().screen_top, palette().screen_bottom))
            .text_color(color(if palette().vector {
                palette().text
            } else {
                palette().aqua
            })),
        color(palette().screen_dark),
        color(palette().sunken_light),
    )
    .when(!palette().vector, |this| {
        this.child(
            canvas(
                |_, _, _| (),
                |bounds: Bounds<gpui::Pixels>, _, window, _| {
                    let mut y = bounds.top() + px(1.);
                    while y < bounds.bottom() {
                        window.paint_quad(fill(
                            Bounds::new(point(bounds.left(), y), size(bounds.size.width, px(1.))),
                            color_alpha(palette().scanline),
                        ));
                        y += px(3.);
                    }
                },
            )
            .absolute()
            .inset_0(),
        )
    })
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
        .border_color(color(if palette().vector {
            light
        } else {
            palette().outline
        }))
        .when(palette().vector, |this| this.rounded(px(4.)))
        .bg(gradient(top, bottom))
        .text_color(color(foreground))
        .when(!palette().vector, |this| {
            this.child(
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
        })
        .child(text.into())
}

fn ref_chip(label: &RefLabel) -> gpui::Div {
    let (text, (foreground, top, bottom, light)): (SharedString, _) = match label.kind {
        RefKind::Head => (format!("✓ {}", label.name).into(), palette().chip_green),
        RefKind::Branch => (label.name.clone(), palette().chip_green),
        RefKind::Remote => (label.name.clone(), palette().chip_blue),
        RefKind::Tag => (label.name.clone(), palette().chip_yellow),
    };
    chip(text, foreground, top, bottom, light)
}

fn status_chip(status: FileStatus) -> gpui::Div {
    let (foreground, top, bottom, light) = match status {
        FileStatus::Modified | FileStatus::Renamed => palette().chip_yellow,
        FileStatus::Added => palette().chip_green,
        FileStatus::Deleted => palette().chip_red,
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
            .text_color(color(palette().text)),
    )
    .id(id)
}

// ---------------------------------------------------------------------------
// rendering
// ---------------------------------------------------------------------------

/// The horizontal centre of a graph lane within the graph column.
fn lane_x(lane: usize) -> f32 {
    17. + lane as f32 * LANE_WIDTH
}

/// Draws a row's graph lines. A line joining the node leaves or enters it
/// sideways and bends into the vertical, so branches fork out and merge back
/// like Fork draws them.
fn paint_graph_row(row: &GraphRow, bounds: Bounds<Pixels>, window: &mut Window) {
    let at = |x: f32, y: f32| -> Point<Pixels> {
        point(bounds.origin.x + px(x), bounds.origin.y + px(y))
    };
    let top = 0.;
    let middle = COMMIT_ROW_HEIGHT / 2.;
    let bottom = bounds.size.height.as_f32();
    let halves = row
        .top
        .iter()
        .map(|edge| (edge, top, middle, true))
        .chain(row.bottom.iter().map(|edge| (edge, middle, bottom, false)));
    for (edge, start_y, end_y, incoming) in halves {
        let from = lane_x(edge.from);
        let to = lane_x(edge.to);
        let mut path = PathBuilder::stroke(px(2.));
        path.move_to(at(from, start_y));
        if edge.from == edge.to {
            path.line_to(at(to, end_y));
        } else if incoming {
            path.curve_to(at(to, end_y), at(from, end_y));
        } else {
            path.curve_to(at(to, end_y), at(to, start_y));
        }
        if let Some(path) = path.build().log_err() {
            window.paint_path(path, lane_color(edge.color));
        }
    }
}

impl WinmanGitView {
    fn render_commit_row(
        &self,
        index: usize,
        lanes: usize,
        remote: Option<&GitRemote>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(commit) = self.commits.get(index) else {
            return div().into_any_element();
        };
        let selected = index == self.selected_commit;
        let dim = if selected {
            palette().text_selected
        } else {
            palette().dim
        };
        let graph_row = commit.graph.clone();
        let node_color = lane_color(graph_row.color);
        let node_left = lane_x(graph_row.lane) - 5.;
        let graph = div()
            .relative()
            .flex_none()
            .w(px(lane_x(lanes - 1) + 17.))
            .h_full()
            .overflow_hidden()
            .child(
                canvas(
                    |_, _, _| {},
                    move |bounds, _, window, _| paint_graph_row(&graph_row, bounds, window),
                )
                .absolute()
                .size_full(),
            )
            .child(
                div()
                    .absolute()
                    .left(px(node_left))
                    .top(px(COMMIT_ROW_HEIGHT / 2. - 5.))
                    .size(px(10.))
                    .border_2()
                    .border_color(node_color)
                    .bg(if selected {
                        node_color
                    } else {
                        color(palette().sunken)
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
                    .id(("message", index))
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .tooltip(message_tooltip(commit.full_message()))
                    .when_some(commit.prefix.clone(), |this, prefix| {
                        this.child(
                            gpui::StyledText::new(format!("{prefix} {}", commit.subject))
                                .with_highlights([(
                                    0..prefix.len(),
                                    HighlightStyle {
                                        color: Some(color(palette().text_bright)),
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
                palette().text_bright
            } else {
                palette().text
            }))
            .child(graph)
            .child(self.render_avatar(commit, index, remote, window, cx))
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

    fn render_avatar(
        &self,
        commit: &CommitRow,
        index: usize,
        remote: Option<&GitRemote>,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        let email = (!commit.email.is_empty()).then(|| commit.email.clone());
        let avatar = CommitAvatar::new(&commit.avatar_sha, email, remote).avatar(window, cx);
        let picture: AnyElement = match avatar {
            Some(avatar) => avatar
                .size(px(AVATAR_SIZE))
                .border_color(color(palette().frame))
                .into_any_element(),
            None => div()
                .size(px(AVATAR_SIZE))
                .rounded_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(color(palette().raised_bottom))
                .border_1()
                .border_color(color(palette().outline))
                .text_color(color(palette().dim))
                .text_size(px(9.))
                .child(initials(&commit.author))
                .into_any_element(),
        };
        let author = commit.author.clone();
        div()
            .id(("avatar", index))
            .flex_none()
            .mr(px(8.))
            .child(picture)
            .tooltip(Tooltip::text(author))
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
                    .bg(color(palette().hunk))
                    .border_t_1()
                    .border_color(color(palette().sunken_dark))
                    .text_size(px(12.))
                    .text_color(color(palette().blue))
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .child(line.text.clone())
                    .into_any_element();
            }
            LineKind::Added => (
                Some(palette().added_background),
                "+",
                palette().green,
                palette().added_number,
            ),
            LineKind::Removed => (
                Some(palette().removed_background),
                "-",
                palette().red,
                palette().removed_number,
            ),
            LineKind::Context => (None, " ", palette().dim, palette().stale),
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
                    .text_color(color(palette().text))
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
                .text_color(color(palette().red))
                .child(error.clone())
                .into_any_element()
        } else if self.commits.is_empty() {
            div()
                .p(px(12.))
                .text_color(color(palette().dim))
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
                cx.processor(|this, range: Range<usize>, window, cx| {
                    let remote = this.refs.remote_url.as_ref().and_then(|url| {
                        let registry = GitHostingProviderRegistry::default_global(cx);
                        let (host, parsed) = parse_git_remote_url(registry, url)?;
                        Some(GitRemote {
                            host,
                            owner: parsed.owner.into(),
                            repo: parsed.repo.into(),
                        })
                    });
                    let lanes = this
                        .commits
                        .iter()
                        .map(|commit| commit.graph.width())
                        .max()
                        .unwrap_or(1)
                        .clamp(1, MAX_VISIBLE_LANES);
                    range
                        .map(|index| {
                            this.render_commit_row(index, lanes, remote.as_ref(), window, cx)
                        })
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
        let width = COMMIT_LIST_WIDTH.lock().ok().and_then(|width| *width);
        div()
            .relative()
            .flex()
            .flex_col()
            .gap(px(4.))
            .h_full()
            .map(|this| match width {
                Some(width) => this.w(width),
                None => this.w(relative(0.36)),
            })
            .flex_none()
            .child(
                div()
                    .id("winman-git-split")
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .right(px(-6.))
                    .w(px(6.))
                    .cursor_col_resize()
                    .on_click(|event: &ClickEvent, _, cx| {
                        if event.click_count() >= 2
                            && let Ok(mut width) = COMMIT_LIST_WIDTH.lock()
                        {
                            *width = None;
                            cx.stop_propagation();
                        }
                    })
                    .on_drag(DraggedSplit, |_, _, _, cx| cx.new(|_| gpui::Empty)),
            )
            .child(
                title_bar("winman-git-commits-title")
                    .child(repo_name)
                    .child(div().text_color(color(palette().dim)).child("›"))
                    .child(branch)
                    .child(
                        div()
                            .ml_auto()
                            .text_color(color(palette().dim))
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
                let (author, timestamp) = match &detail {
                    Some(detail) => (detail.author.clone(), detail.timestamp),
                    None => (commit.author.clone(), commit.timestamp),
                };
                let (foreground, top, bottom, light) = palette().chip_blue;
                this.child(author)
                    .child(div().opacity(0.7).child(commit.short_sha.clone()))
                    .child(div().opacity(0.7).child(format_long_time(timestamp)))
                    .child(
                        div()
                            .id("winman-git-subject")
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .text_color(color(palette().screen_subject))
                            .tooltip(message_tooltip(commit.full_message()))
                            .child(commit.full_subject()),
                    )
                    .child(
                        chip(
                            if self.show_message {
                                "Diff"
                            } else {
                                "Meddelande"
                            },
                            foreground,
                            top,
                            bottom,
                            light,
                        )
                        .id("winman-git-message-toggle")
                        .cursor_pointer()
                        .tooltip(Tooltip::text("Växla mellan diff och hela meddelandet (m)"))
                        .on_click(
                            cx.listener(|this, _: &ClickEvent, _, cx| this.toggle_message(cx)),
                        ),
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
                                        .text_color(color(palette().dim))
                                        .child(format!("{old_path} → ")),
                                )
                            })
                            .child(div().text_color(color(palette().dim)).child(directory))
                            .child(div().text_color(color(palette().text_bright)).child(name)),
                    )
                    .child(
                        div()
                            .ml_auto()
                            .flex_none()
                            .flex()
                            .gap(px(6.))
                            .child(
                                div()
                                    .text_color(color(palette().green))
                                    .child(format!("+{}", file.added)),
                            )
                            .child(
                                div()
                                    .text_color(color(palette().red))
                                    .child(format!("-{}", file.removed)),
                            )
                            .child(div().text_color(color(palette().dim)).child(format!(
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
                    .text_color(color(palette().dim))
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
                .text_color(color(palette().dim))
                .child("Inga ändrade filer")
                .into_any_element(),
            (None, _) => div().into_any_element(),
        };

        if self.show_message {
            let message = div()
                .id("winman-git-message")
                .size_full()
                .overflow_y_scroll()
                .p(px(14.))
                .flex()
                .flex_col()
                .gap(px(12.))
                .when_some(commit, |this, commit| {
                    this.child(
                        div()
                            .text_color(color(palette().text_bright))
                            .child(commit.full_subject()),
                    )
                    .when(!commit.body.is_empty(), |this| {
                        this.child(
                            div()
                                .text_color(color(palette().text))
                                .child(commit.body.clone()),
                        )
                    })
                });
            return div()
                .flex()
                .flex_col()
                .gap(px(4.))
                .h_full()
                .flex_1()
                .min_w_0()
                .child(header)
                .child(sunken_edges(
                    sunken(div())
                        .flex_1()
                        .min_h_0()
                        .overflow_hidden()
                        .bg(color(palette().code))
                        .child(message),
                ));
        }

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
                    .bg(color(palette().code))
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
                    base.text_color(color(palette().text))
                        .child(
                            div()
                                .text_size(px(10.))
                                .text_color(color(palette().dim))
                                .child("▾"),
                        )
                        .child(div().text_color(color(palette().blue)).child("\u{f07b}"))
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
                        palette().text_bright
                    } else {
                        palette().text
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
                palette().text_bright
            } else {
                palette().text
            }))
            .child(
                div()
                    .flex_none()
                    .w(px(12.))
                    .flex()
                    .justify_center()
                    .text_color(color(if entry.current {
                        palette().green
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
                        .text_color(color(palette().dim))
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
                    .text_color(color(palette().dim))
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
                palette().blue,
                &self.refs.worktrees,
            ),
            (
                "Branches",
                "branch",
                "\u{e0a0}",
                palette().dim,
                &self.refs.branches,
            ),
            (
                "Remotes",
                "remote",
                "\u{e0a0}",
                palette().blue,
                &self.refs.remotes,
            ),
            ("Tags", "tag", "◆", palette().yellow, &self.refs.tags),
            ("Stashes", "stash", "▤", palette().dim, &self.refs.stashes),
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
                .text_color(color(palette().text)),
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
            .w(px(SIDEBAR_WIDTH))
            .flex_none()
            .child(
                title_bar("winman-git-files-title")
                    .pr(px(5.))
                    .child("Filer")
                    .child(
                        div()
                            .text_color(color(palette().dim))
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
        // The vector skins (Mist, gruvbox-dark) get flat rounded panels in
        // their own colours; every other theme the Amiga look.
        ACTIVE_PALETTE.with(|palette| palette.set(select_palette(cx)));
        div()
            .id("winman-git-view")
            .key_context("WinmanGitView")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::handle_key_down))
            .size_full()
            .p(px(if palette().vector { 8. } else { 5. }))
            .flex()
            .gap(px(if palette().vector { 6. } else { 5. }))
            .when(palette().vector, |this| {
                this.rounded(px(9.))
                    .border_1()
                    .border_color(color(palette().frame))
            })
            .font(font)
            .text_size(px(13.))
            .text_color(color(palette().text))
            .bg(gradient(
                palette().background_top,
                palette().background_bottom,
            ))
            .on_drag_move::<DraggedSplit>(cx.listener(
                |_, event: &gpui::DragMoveEvent<DraggedSplit>, _, cx| {
                    let bounds = event.bounds;
                    let padding = px(if palette().vector { 8. } else { 5. });
                    let max = bounds.size.width - px(SIDEBAR_WIDTH + MIN_DIFF_WIDTH);
                    let dragged = (event.event.position.x - bounds.left() - padding)
                        .min(max)
                        .max(px(MIN_COMMIT_LIST_WIDTH));
                    if let Ok(mut width) = COMMIT_LIST_WIDTH.lock() {
                        *width = Some(dragged);
                    }
                    cx.notify();
                },
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
            "abc\x1fab\x1fOlof\x1folof@example.com\x1f100\x1fHEAD -> main, origin/main, origin/HEAD, tag: v1\x1fdef 123\x1fBaren: gröna skärmen",
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
        assert_eq!(commit.parents, vec!["def", "123"]);
        assert_eq!(commit.prefix.as_deref(), Some("Baren:"));
        assert_eq!(commit.subject.as_ref(), "gröna skärmen");
    }

    #[test]
    fn lays_out_a_merged_branch() {
        // m merges b into a; b and a both come from r.
        let commit = |sha: &str, parents: &[&str]| {
            parse_commit(
                &format!(
                    "{sha}\x1f{sha}\x1fOlof\x1f\x1f0\x1f\x1f{}\x1fs",
                    parents.join(" ")
                ),
                &[],
            )
            .expect("commit")
        };
        let mut commits = vec![
            commit("m", &["a", "b"]),
            commit("b", &["r"]),
            commit("a", &["r"]),
            commit("r", &[]),
        ];
        layout_graph(&mut commits);
        let lanes: Vec<_> = commits.iter().map(|c| c.graph.lane).collect();
        assert_eq!(lanes, vec![0, 1, 0, 0]);
        let edge = |from, to, color| GraphEdge { from, to, color };
        assert_eq!(commits[0].graph.bottom, vec![edge(0, 0, 0), edge(0, 1, 1)]);
        assert_eq!(commits[1].graph.top, vec![edge(0, 0, 0), edge(1, 1, 1)]);
        assert_eq!(commits[2].graph.bottom, vec![edge(1, 1, 1), edge(0, 0, 0)]);
        // Both lanes wait for r; b's bends into a's where it forked off.
        assert_eq!(commits[3].graph.lane, 0);
        assert_eq!(commits[3].graph.top, vec![edge(0, 0, 0), edge(1, 0, 1)]);
        assert!(commits[3].graph.bottom.is_empty());
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
