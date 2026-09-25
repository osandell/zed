//! The terminal column on the left of each workspace: the Ghostty fork's
//! in-window tab bar (`ZedTabBar.swift`), each tab a split tree of Ghostty
//! surfaces, and the bottom strip. Sizes, colors and rules follow the fork so
//! the column looks exactly like the Ghostty window winman used to place there.

use std::path::{Path, PathBuf};

use gpui::{
    AnyElement, App, Axis, Bounds, ClickEvent, Context, Entity, EntityId, FocusHandle, Focusable,
    Hsla, InteractiveElement, IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, ParentElement, Pixels, PromptLevel, Render, Rgba, SharedString,
    StatefulInteractiveElement, Styled, Subscription, WeakEntity, Window, canvas, div, img,
    prelude::FluentBuilder, px, rgb,
};
use project::Project;
use util::paths::PathExt as _;
use workspace::{LeadingColumnLayout, Workspace};

use crate::{
    GhosttyTerminal, GhosttyTerminalEvent, InheritContext, TerminalOptions,
    graphics::{self, Bitmap, SymbolWeight, darken, lighten, mix},
    runtime,
    worktree_picker::{self, WorktreeEntry, WorktreePicker},
};
use ghostty_embed as ffi;

const BAR_HEIGHT: f32 = 40.;
const TITLE_ROW_HEIGHT: f32 = 24.;
const WORKTREE_ROW_HEIGHT: f32 = 16.;
const CLOSE_BUTTON_WIDTH: f32 = 32.;
const MAX_TAB_WIDTH: f32 = 336.;
const MIN_TAB_WIDTH: f32 = 86.;
const NEW_TAB_BUTTON_WIDTH: f32 = 36.;
const BOTTOM_BAND_HEIGHT: f32 = 10.;

const LAMP_WORKING: u32 = 0xfe8019;
const LAMP_QUESTION: u32 = 0xc678dd;
const LAMP_DONE: u32 = 0xb8bb26;
const LAMP_IDLE: u32 = 0x928374;
const LAMP_BACKGROUND: u32 = 0x83a598;
const LAMP_BLOCKED: u32 = 0xfb4934;

/// Frames per gear turn; the gear turns once per 4 s.
const GEAR_FRAMES: u32 = 120;

const KEY_CODE_U: u32 = 0x20;
const KEY_CODE_RETURN: u32 = 0x24;

/// What `pick_worktree` did, the fork's `pick-worktree` replies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PickWorktree {
    Opened,
    Stepped,
    ClaudeTab,
    NoWorktrees,
    Unavailable,
}

pub enum TerminalColumnEvent {
    /// The worktree picker `cd`d a tab; the editor side follows it.
    WorktreeChosen(PathBuf),
    /// Tabs were added, removed, selected or changed their lamps: the winman
    /// bar's tab strips and the editor follow need a look.
    TabsChanged,
}

impl gpui::EventEmitter<TerminalColumnEvent> for TerminalColumn {}

/// What the Claude session in a tab is doing, from its hook state file.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ClaudeState {
    #[default]
    Absent,
    Working,
    Question,
    Done,
    Background,
}

enum SplitNode {
    Leaf(Entity<GhosttyTerminal>),
    Split {
        axis: Axis,
        /// Fraction of the space taken by `first`.
        ratio: f32,
        first: Box<SplitNode>,
        second: Box<SplitNode>,
    },
}

impl SplitNode {
    fn leaves(&self) -> Vec<Entity<GhosttyTerminal>> {
        let mut leaves = Vec::new();
        self.collect_leaves(&mut leaves);
        leaves
    }

    fn collect_leaves(&self, leaves: &mut Vec<Entity<GhosttyTerminal>>) {
        match self {
            SplitNode::Leaf(terminal) => leaves.push(terminal.clone()),
            SplitNode::Split { first, second, .. } => {
                first.collect_leaves(leaves);
                second.collect_leaves(leaves);
            }
        }
    }

    fn leaf_count(&self) -> usize {
        match self {
            SplitNode::Leaf(_) => 1,
            SplitNode::Split { first, second, .. } => first.leaf_count() + second.leaf_count(),
        }
    }

    fn contains(&self, id: EntityId) -> bool {
        match self {
            SplitNode::Leaf(terminal) => terminal.entity_id() == id,
            SplitNode::Split { first, second, .. } => first.contains(id) || second.contains(id),
        }
    }

    /// Replaces the leaf `target` with a split holding it and `new`.
    fn split(
        &mut self,
        target: EntityId,
        new: Entity<GhosttyTerminal>,
        axis: Axis,
        new_first: bool,
    ) -> bool {
        match self {
            SplitNode::Leaf(terminal) if terminal.entity_id() == target => {
                let existing = SplitNode::Leaf(terminal.clone());
                let added = SplitNode::Leaf(new);
                let (first, second) = if new_first {
                    (added, existing)
                } else {
                    (existing, added)
                };
                *self = SplitNode::Split {
                    axis,
                    ratio: 0.5,
                    first: Box::new(first),
                    second: Box::new(second),
                };
                true
            }
            SplitNode::Leaf(_) => false,
            SplitNode::Split { first, second, .. } => {
                if first.contains(target) {
                    first.split(target, new, axis, new_first)
                } else {
                    second.split(target, new, axis, new_first)
                }
            }
        }
    }

    /// Removes the leaf `target`; its sibling takes the parent's place.
    /// Returns `None` when the tree becomes empty.
    fn remove(self, target: EntityId) -> Option<SplitNode> {
        match self {
            SplitNode::Leaf(terminal) if terminal.entity_id() == target => None,
            SplitNode::Leaf(terminal) => Some(SplitNode::Leaf(terminal)),
            SplitNode::Split {
                axis,
                ratio,
                first,
                second,
            } => match (first.remove(target), second.remove(target)) {
                (Some(first), Some(second)) => Some(SplitNode::Split {
                    axis,
                    ratio,
                    first: Box::new(first),
                    second: Box::new(second),
                }),
                (Some(only), None) | (None, Some(only)) => Some(only),
                (None, None) => None,
            },
        }
    }

    fn equalize(&mut self) {
        if let SplitNode::Split {
            axis,
            ratio,
            first,
            second,
        } = self
        {
            let weight = |node: &SplitNode| match node {
                SplitNode::Split { axis: inner, .. } if inner == axis => node.leaf_count(),
                _ => 1,
            };
            let first_weight = weight(first) as f32;
            let second_weight = weight(second) as f32;
            *ratio = first_weight / (first_weight + second_weight);
            first.equalize();
            second.equalize();
        }
    }

    /// Moves the nearest divider on `axis` that has `target` on the side
    /// given by `target_first`, by `delta` (a fraction of that split).
    fn resize(&mut self, target: EntityId, axis: Axis, grow_first: bool, delta: f32) -> bool {
        let SplitNode::Split {
            axis: split_axis,
            ratio,
            first,
            second,
        } = self
        else {
            return false;
        };
        let in_first = first.contains(target);
        let child = if in_first {
            first.as_mut()
        } else {
            second.as_mut()
        };
        if child.resize(target, axis, grow_first, delta) {
            return true;
        }
        if *split_axis != axis || !(in_first || second.contains(target)) {
            return false;
        }
        let change = if grow_first { delta } else { -delta };
        *ratio = (*ratio + change).clamp(0.05, 0.95);
        true
    }

    fn ratio_at_mut(&mut self, path: &[bool]) -> Option<(&mut f32, Axis)> {
        match self {
            SplitNode::Leaf(_) => None,
            SplitNode::Split {
                axis,
                ratio,
                first,
                second,
            } => match path.split_first() {
                None => Some((ratio, *axis)),
                Some((go_second, rest)) => {
                    if *go_second {
                        second.ratio_at_mut(rest)
                    } else {
                        first.ratio_at_mut(rest)
                    }
                }
            },
        }
    }
}

pub struct TerminalTab {
    id: u64,
    tree: SplitNode,
    focused: Option<WeakEntity<GhosttyTerminal>>,
    zoomed: Option<EntityId>,
    pub claude_title: Option<SharedString>,
    pub claude_session: Option<String>,
    pub claude_state: ClaudeState,
    pub claude_present: bool,
    pub blocked: bool,
    pub blocked_note: String,
    pub worktree: Option<String>,
    pub worktree_path: Option<PathBuf>,
}

impl TerminalTab {
    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn terminals(&self) -> Vec<Entity<GhosttyTerminal>> {
        self.tree.leaves()
    }

    /// The focused surface first, then the rest of the splits.
    pub fn terminals_focused_first(&self) -> Vec<Entity<GhosttyTerminal>> {
        let mut terminals = self.tree.leaves();
        if let Some(focused) = self.focused_terminal()
            && let Some(position) = terminals.iter().position(|terminal| terminal == &focused)
        {
            let focused = terminals.remove(position);
            terminals.insert(0, focused);
        }
        terminals
    }

    pub fn focused_terminal(&self) -> Option<Entity<GhosttyTerminal>> {
        self.focused
            .as_ref()
            .and_then(|focused| focused.upgrade())
            .filter(|focused| self.tree.contains(focused.entity_id()))
            .or_else(|| self.tree.leaves().into_iter().next())
    }
}

/// The last `worktrees` component of `path` splits it into the project
/// container and the worktree name (the fork's `WorktreeLayout.split`).
pub fn worktree_split(path: &Path) -> Option<(PathBuf, String, PathBuf)> {
    let components: Vec<_> = path.components().collect();
    let index = components
        .iter()
        .rposition(|component| component.as_os_str() == "worktrees")?;
    let name = components
        .get(index + 1)?
        .as_os_str()
        .to_string_lossy()
        .into_owned();
    let container: PathBuf = components[..index].iter().collect();
    let worktree = container.join("worktrees").join(&name);
    Some((container, name, worktree))
}

pub struct TerminalColumn {
    /// The workspace this column belongs to (its worktree).
    workspace: WeakEntity<Workspace>,
    /// The workspace showing this column right now, which is another
    /// worktree's while the editor follows the work elsewhere.
    displayed_in: WeakEntity<Workspace>,
    /// The workspace's worktree root (winman's window identity).
    workspace_path: Option<PathBuf>,
    /// `workspace_path` with `~`, shown as the tab title (the fork's
    /// `titleOverride`).
    title_path: SharedString,
    worktrees_dir: Option<PathBuf>,
    tabs: Vec<TerminalTab>,
    selected: usize,
    next_tab_id: u64,
    focus_handle: FocusHandle,
    bar_width: f32,
    /// Split bounds from the last paint, keyed by the path to the split.
    split_bounds: Vec<(u64, Vec<bool>, Bounds<Pixels>)>,
    dragging_divider: Option<(u64, Vec<bool>)>,
    worktree_picker: Option<WorktreePicker>,
    /// winman's fullscreen for this worktree: the side with the keyboard takes
    /// the whole width.
    fullscreen: bool,
    /// Whether the keyboard is in the terminal column (else the editor).
    terminal_side: bool,
    /// Whether this column's workspace is the one the window shows.
    workspace_active: bool,
    column_width: Pixels,
    subscriptions: Vec<(EntityId, Subscription)>,
    _window_subscriptions: Vec<Subscription>,
}

impl TerminalColumn {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        project: Option<Entity<Project>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        let mut window_subscriptions = vec![
            cx.observe_window_activation(window, |_, _, cx| cx.notify()),
            cx.observe_window_appearance(window, |this, window, cx| {
                this.sync_color_scheme(window, cx);
            }),
            cx.on_focus(&focus_handle, window, |this, window, cx| {
                this.focus_selected(window, cx);
            }),
            // Fullscreen shows whichever side holds the keyboard.
            cx.on_focus_in(&focus_handle, window, |this, _window, cx| {
                this.set_terminal_side(true, cx);
            }),
            cx.on_focus_out(&focus_handle, window, |this, _, _window, cx| {
                this.set_terminal_side(false, cx);
            }),
        ];
        // Start the shells as soon as the project has its root rather than
        // when the column is first shown, so every workspace's terminals (and
        // their resumed Claude sessions) run from launch, like the Ghostty
        // windows winman opened up front.
        if let Some(project) = project.as_ref() {
            window_subscriptions.push(cx.subscribe_in(
                project,
                window,
                |this, _, event, window, cx| {
                    if matches!(event, project::Event::WorktreeAdded(_)) {
                        this.ensure_started(window, cx);
                    }
                },
            ));
        }
        cx.defer_in(window, |this, window, cx| this.ensure_started(window, cx));
        crate::claude_status::ClaudeTabStatus::register(
            window.window_handle(),
            cx.entity().downgrade(),
            cx,
        );
        Self {
            displayed_in: workspace.clone(),
            workspace,
            workspace_path: None,
            title_path: "👻".into(),
            worktrees_dir: None,
            tabs: Vec::new(),
            selected: 0,
            next_tab_id: 0,
            focus_handle,
            bar_width: MAX_TAB_WIDTH + NEW_TAB_BUTTON_WIDTH,
            split_bounds: Vec::new(),
            dragging_divider: None,
            worktree_picker: None,
            fullscreen: false,
            terminal_side: false,
            workspace_active: true,
            column_width: px(800.),
            subscriptions: Vec::new(),
            _window_subscriptions: window_subscriptions,
        }
    }

    /// A column for `path` outside any workspace (examples and tests).
    pub fn for_path(path: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut this = Self::new(WeakEntity::new_invalid(), None, window, cx);
        this.title_path = path.compact().to_string_lossy().into_owned().into();
        this.worktrees_dir =
            worktree_split(&path).map(|(container, _, _)| container.join("worktrees"));
        this.workspace_path = Some(path);
        this
    }

    pub fn focus_handle_ref(&self) -> &FocusHandle {
        &self.focus_handle
    }

    /// Selects the tab and gives `terminal` (one of its splits) the keyboard.
    pub fn focus_terminal_in_tab(
        &mut self,
        tab_id: u64,
        terminal: &Entity<GhosttyTerminal>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.tabs.iter().position(|tab| tab.id == tab_id) else {
            return;
        };
        self.tabs[index].focused = Some(terminal.downgrade());
        self.select_tab(index, window, cx);
    }

    pub fn tabs(&self) -> &[TerminalTab] {
        &self.tabs
    }

    pub fn tabs_mut(&mut self) -> &mut [TerminalTab] {
        &mut self.tabs
    }

    pub fn selected_index(&self) -> usize {
        self.selected
    }

    pub fn workspace_path(&self) -> Option<&PathBuf> {
        self.workspace_path.as_ref()
    }

    pub fn workspace(&self) -> &WeakEntity<Workspace> {
        &self.workspace
    }

    fn sync_color_scheme(&self, window: &Window, _cx: &mut Context<Self>) {
        let dark = matches!(
            window.appearance(),
            gpui::WindowAppearance::Dark | gpui::WindowAppearance::VibrantDark
        );
        runtime::set_color_scheme(dark);
        for tab in &self.tabs {
            for terminal in tab.terminals() {
                terminal.read(_cx).set_color_scheme(dark);
            }
        }
    }

    /// Picks up the workspace's root path once the project has one, and opens
    /// the first tab.
    fn ensure_started(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.workspace_path.is_some() && !self.tabs.is_empty() {
            return;
        }
        if self.workspace_path.is_none() {
            let Some(workspace) = self.workspace.upgrade() else {
                return;
            };
            let Some(root) = workspace
                .read(cx)
                .project()
                .read(cx)
                .visible_worktrees(cx)
                .next()
                .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
            else {
                return;
            };
            self.title_path = root.compact().to_string_lossy().into_owned().into();
            self.worktrees_dir =
                worktree_split(&root).map(|(container, _, _)| container.join("worktrees"));
            self.workspace_path = Some(root);
        }
        if self.tabs.is_empty() {
            cx.defer_in(window, move |this, window, cx| {
                if this.tabs.is_empty() {
                    this.open_initial_tabs(window, cx);
                    this.sync_color_scheme(window, cx);
                }
            });
        }
    }

    /// The workspace's saved tabs, each resuming its Claude session, or a
    /// single fresh tab.
    fn open_initial_tabs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let snapshot = self
            .workspace_path
            .as_deref()
            .and_then(crate::tab_sessions::take_restore);
        if let Some(snapshot) = snapshot {
            for saved in &snapshot.tabs {
                let options = TerminalOptions {
                    working_directory: Some(PathBuf::from(&saved.cwd)),
                    initial_input: saved.initial_input(),
                    ..Default::default()
                };
                if let Some(id) = self.new_tab(options, window, cx)
                    && let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == id)
                {
                    tab.blocked = saved.blocked.unwrap_or(false);
                    tab.blocked_note = saved.blocked_note.clone().unwrap_or_default();
                }
            }
            if !self.tabs.is_empty() {
                self.select_tab(snapshot.selected.min(self.tabs.len() - 1), window, cx);
                return;
            }
        }
        let working_directory = self.workspace_path.clone();
        self.new_tab(
            TerminalOptions {
                working_directory,
                ..Default::default()
            },
            window,
            cx,
        );
    }

    fn subscribe(
        &mut self,
        terminal: &Entity<GhosttyTerminal>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let subscription = cx.subscribe_in(terminal, window, Self::handle_terminal_event);
        self.subscriptions
            .push((terminal.entity_id(), subscription));
    }

    fn open_terminal(
        &mut self,
        options: TerminalOptions,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<GhosttyTerminal>> {
        match GhosttyTerminal::open(options, window, cx) {
            Ok(terminal) => {
                self.subscribe(&terminal, window, cx);
                Some(terminal)
            }
            Err(error) => {
                log::error!("failed to open a Ghostty terminal: {error:#}");
                None
            }
        }
    }

    /// Opens a tab after the others and selects it.
    pub fn new_tab(
        &mut self,
        options: TerminalOptions,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<u64> {
        let terminal = self.open_terminal(options, window, cx)?;
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        self.tabs.push(TerminalTab {
            id,
            tree: SplitNode::Leaf(terminal.clone()),
            focused: Some(terminal.downgrade()),
            zoomed: None,
            claude_title: None,
            claude_session: None,
            claude_state: ClaudeState::Absent,
            claude_present: false,
            blocked: false,
            blocked_note: String::new(),
            worktree: None,
            worktree_path: None,
        });
        self.select_tab(self.tabs.len() - 1, window, cx);
        Some(id)
    }

    /// The fork's `zedNewTab(followingWorktree: true)`: a new tab starts in the
    /// worktree the current tab's Claude session works in, else it inherits
    /// the current terminal's directory like a Ghostty tab.
    pub fn new_tab_following_worktree(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let current = self.tabs.get(self.selected);
        let claude_worktree = current
            .filter(|tab| tab.claude_present)
            .and_then(|tab| tab.worktree_path.clone())
            .filter(|path| path.is_dir());
        let inherit_from = current
            .and_then(|tab| tab.focused_terminal())
            .map(|terminal| (terminal.downgrade(), InheritContext::Tab));
        let options = TerminalOptions {
            working_directory: claude_worktree.or_else(|| {
                inherit_from
                    .is_none()
                    .then(|| self.workspace_path.clone())
                    .flatten()
            }),
            inherit_from,
            ..Default::default()
        };
        self.new_tab(options, window, cx);
    }

    pub fn select_tab(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(index) else {
            return;
        };
        let visible: Vec<EntityId> = tab
            .terminals()
            .iter()
            .map(|terminal| terminal.entity_id())
            .collect();
        for (tab_index, tab) in self.tabs.iter().enumerate() {
            for terminal in tab.terminals() {
                let is_visible = self.workspace_active
                    && tab_index == index
                    && visible.contains(&terminal.entity_id());
                terminal.read(cx).set_visible(is_visible);
            }
        }
        self.selected = index;
        self.focus_selected(window, cx);
        cx.emit(TerminalColumnEvent::TabsChanged);
        cx.notify();
    }

    fn focus_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(terminal) = self
            .tabs
            .get(self.selected)
            .and_then(|tab| tab.focused_terminal())
        {
            window.focus(&terminal.focus_handle(cx), cx);
        }
    }

    fn tab_index_of(&self, terminal: EntityId) -> Option<usize> {
        self.tabs.iter().position(|tab| tab.tree.contains(terminal))
    }

    /// The fork's `zedCloseTab`: asks first when a process is running, and
    /// never leaves the column without a tab.
    pub fn close_tab(
        &mut self,
        index: usize,
        confirm: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.tabs.get(index) else {
            return;
        };
        let needs_confirm = confirm
            && tab
                .terminals()
                .iter()
                .any(|terminal| terminal.read(cx).needs_confirm_quit());
        if needs_confirm {
            let tab_id = tab.id;
            let answer = window.prompt(
                PromptLevel::Warning,
                "Close Tab?",
                Some("The terminal still has a running process. If you close the tab the process will be killed."),
                &["Close", "Cancel"],
                cx,
            );
            cx.spawn_in(window, async move |this, cx| {
                if answer.await.ok() == Some(0) {
                    this.update_in(cx, |this, window, cx| {
                        if let Some(index) = this.tabs.iter().position(|tab| tab.id == tab_id) {
                            this.close_tab(index, false, window, cx);
                        }
                    })
                    .ok();
                }
            })
            .detach();
            return;
        }

        if self.tabs.len() == 1 {
            let working_directory = self.workspace_path.clone();
            self.new_tab(
                TerminalOptions {
                    working_directory,
                    ..Default::default()
                },
                window,
                cx,
            );
        }
        let Some(index) = self
            .tabs
            .iter()
            .position(|candidate| candidate.id == self.tabs[index].id)
        else {
            return;
        };
        let was_selected = index == self.selected;
        let removed = self.tabs.remove(index);
        self.drop_terminals(&removed.terminals());
        cx.emit(TerminalColumnEvent::TabsChanged);
        if self.tabs.is_empty() {
            cx.notify();
            return;
        }
        if was_selected {
            self.select_tab(index.min(self.tabs.len() - 1), window, cx);
        } else if index < self.selected {
            self.selected -= 1;
        }
        cx.notify();
    }

    fn drop_terminals(&mut self, terminals: &[Entity<GhosttyTerminal>]) {
        let ids: Vec<EntityId> = terminals
            .iter()
            .map(|terminal| terminal.entity_id())
            .collect();
        // Dropping the subscription and the last handle frees the surface,
        // which closes its pty and hangs up the shell.
        self.subscriptions.retain(|(id, _)| !ids.contains(id));
    }

    fn close_terminal(
        &mut self,
        terminal: EntityId,
        process_alive: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.tab_index_of(terminal) else {
            return;
        };
        if process_alive {
            let answer = window.prompt(
                PromptLevel::Warning,
                "Close Terminal?",
                Some("The terminal still has a running process. If you close the terminal the process will be killed."),
                &["Close", "Cancel"],
                cx,
            );
            cx.spawn_in(window, async move |this, cx| {
                if answer.await.ok() == Some(0) {
                    this.update_in(cx, |this, window, cx| {
                        this.close_terminal(terminal, false, window, cx)
                    })
                    .ok();
                }
            })
            .detach();
            return;
        }
        if self.tabs[index].tree.leaf_count() == 1 {
            self.close_tab(index, false, window, cx);
            return;
        }
        let tab = &mut self.tabs[index];
        let leaves = tab.tree.leaves();
        let placeholder = SplitNode::Leaf(leaves[0].clone());
        let tree = std::mem::replace(&mut tab.tree, placeholder);
        if let Some(tree) = tree.remove(terminal) {
            tab.tree = tree;
        }
        if tab.zoomed == Some(terminal) {
            tab.zoomed = None;
        }
        if let Some(removed) = leaves.iter().find(|leaf| leaf.entity_id() == terminal) {
            self.drop_terminals(std::slice::from_ref(removed));
        }
        if index == self.selected {
            self.focus_selected(window, cx);
        }
        cx.notify();
    }

    fn new_split(
        &mut self,
        terminal: &Entity<GhosttyTerminal>,
        direction: ffi::ghostty_action_split_direction_e,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.tab_index_of(terminal.entity_id()) else {
            return;
        };
        let (axis, new_first) = match direction {
            ffi::GHOSTTY_SPLIT_DIRECTION_LEFT => (Axis::Horizontal, true),
            ffi::GHOSTTY_SPLIT_DIRECTION_UP => (Axis::Vertical, true),
            ffi::GHOSTTY_SPLIT_DIRECTION_DOWN => (Axis::Vertical, false),
            _ => (Axis::Horizontal, false),
        };
        let options = TerminalOptions {
            inherit_from: Some((terminal.downgrade(), InheritContext::Split)),
            ..Default::default()
        };
        let Some(new_terminal) = self.open_terminal(options, window, cx) else {
            return;
        };
        let tab = &mut self.tabs[index];
        tab.zoomed = None;
        tab.tree
            .split(terminal.entity_id(), new_terminal.clone(), axis, new_first);
        tab.focused = Some(new_terminal.downgrade());
        new_terminal.read(cx).set_visible(index == self.selected);
        if index == self.selected {
            window.focus(&new_terminal.focus_handle(cx), cx);
        }
        cx.notify();
    }

    fn goto_split(
        &mut self,
        terminal: &Entity<GhosttyTerminal>,
        direction: ffi::ghostty_action_goto_split_e,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.tab_index_of(terminal.entity_id()) else {
            return;
        };
        let leaves = self.tabs[index].tree.leaves();
        let Some(position) = leaves.iter().position(|leaf| leaf == terminal) else {
            return;
        };
        let target = match direction {
            ffi::GHOSTTY_GOTO_SPLIT_PREVIOUS => {
                Some(leaves[(position + leaves.len() - 1) % leaves.len()].clone())
            }
            ffi::GHOSTTY_GOTO_SPLIT_NEXT => Some(leaves[(position + 1) % leaves.len()].clone()),
            _ => {
                let Some(from) = terminal.read(cx).bounds() else {
                    return;
                };
                let from_center = from.center();
                leaves
                    .iter()
                    .filter(|leaf| *leaf != terminal)
                    .filter_map(|leaf| Some((leaf, leaf.read(cx).bounds()?)))
                    .filter(|(_, bounds)| match direction {
                        ffi::GHOSTTY_GOTO_SPLIT_LEFT => bounds.right() <= from.left(),
                        ffi::GHOSTTY_GOTO_SPLIT_RIGHT => bounds.left() >= from.right(),
                        ffi::GHOSTTY_GOTO_SPLIT_UP => bounds.bottom() <= from.top(),
                        _ => bounds.top() >= from.bottom(),
                    })
                    .min_by(|(_, a), (_, b)| {
                        let distance = |bounds: &Bounds<Pixels>| {
                            let center = bounds.center();
                            let dx = f32::from(center.x - from_center.x);
                            let dy = f32::from(center.y - from_center.y);
                            dx * dx + dy * dy
                        };
                        distance(a).total_cmp(&distance(b))
                    })
                    .map(|(leaf, _)| leaf.clone())
            }
        };
        if let Some(target) = target {
            self.tabs[index].focused = Some(target.downgrade());
            window.focus(&target.focus_handle(cx), cx);
            cx.notify();
        }
    }

    fn goto_tab(&mut self, tab: i32, window: &mut Window, cx: &mut Context<Self>) {
        let count = self.tabs.len();
        if count == 0 {
            return;
        }
        let index = match tab {
            ffi::GHOSTTY_GOTO_TAB_PREVIOUS => (self.selected + count - 1) % count,
            ffi::GHOSTTY_GOTO_TAB_NEXT => (self.selected + 1) % count,
            ffi::GHOSTTY_GOTO_TAB_LAST => count - 1,
            number => (number.max(1) as usize).min(count) - 1,
        };
        self.select_tab(index, window, cx);
    }

    fn handle_terminal_event(
        &mut self,
        terminal: &Entity<GhosttyTerminal>,
        event: &GhosttyTerminalEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            GhosttyTerminalEvent::Focused => {
                if let Some(index) = self.tab_index_of(terminal.entity_id()) {
                    self.tabs[index].focused = Some(terminal.downgrade());
                    cx.notify();
                }
            }
            GhosttyTerminalEvent::CloseRequested { process_alive } => {
                self.close_terminal(terminal.entity_id(), *process_alive, window, cx)
            }
            GhosttyTerminalEvent::NewTab => self.new_tab_following_worktree(window, cx),
            GhosttyTerminalEvent::NewSplit(direction) => {
                self.new_split(terminal, *direction, window, cx)
            }
            GhosttyTerminalEvent::CloseTab(mode) => {
                let Some(index) = self.tab_index_of(terminal.entity_id()) else {
                    return;
                };
                let keep = self.tabs[index].id;
                let doomed: Vec<u64> = match *mode {
                    ffi::GHOSTTY_ACTION_CLOSE_TAB_MODE_OTHER => self
                        .tabs
                        .iter()
                        .map(|tab| tab.id)
                        .filter(|id| *id != keep)
                        .collect(),
                    ffi::GHOSTTY_ACTION_CLOSE_TAB_MODE_RIGHT => {
                        self.tabs[index + 1..].iter().map(|tab| tab.id).collect()
                    }
                    _ => vec![keep],
                };
                for id in doomed {
                    if let Some(index) = self.tabs.iter().position(|tab| tab.id == id) {
                        self.close_tab(index, true, window, cx);
                    }
                }
            }
            GhosttyTerminalEvent::GotoTab(tab) => self.goto_tab(*tab, window, cx),
            GhosttyTerminalEvent::GotoSplit(direction) => {
                self.goto_split(terminal, *direction, window, cx)
            }
            GhosttyTerminalEvent::ResizeSplit { direction, amount } => {
                let Some(index) = self.tab_index_of(terminal.entity_id()) else {
                    return;
                };
                let (axis, grow_first) = match *direction {
                    ffi::GHOSTTY_RESIZE_SPLIT_LEFT => (Axis::Horizontal, false),
                    ffi::GHOSTTY_RESIZE_SPLIT_RIGHT => (Axis::Horizontal, true),
                    ffi::GHOSTTY_RESIZE_SPLIT_UP => (Axis::Vertical, false),
                    _ => (Axis::Vertical, true),
                };
                // Ghostty's amount is in points; relate it to the tab's area.
                let extent = self
                    .split_bounds
                    .iter()
                    .find(|(tab, path, _)| *tab == self.tabs[index].id && path.is_empty())
                    .map(|(_, _, bounds)| match axis {
                        Axis::Horizontal => f32::from(bounds.size.width),
                        Axis::Vertical => f32::from(bounds.size.height),
                    })
                    .unwrap_or(800.)
                    .max(1.);
                self.tabs[index].tree.resize(
                    terminal.entity_id(),
                    axis,
                    grow_first,
                    *amount as f32 / extent,
                );
                cx.notify();
            }
            GhosttyTerminalEvent::EqualizeSplits => {
                if let Some(index) = self.tab_index_of(terminal.entity_id()) {
                    self.tabs[index].tree.equalize();
                    cx.notify();
                }
            }
            GhosttyTerminalEvent::ToggleSplitZoom => {
                if let Some(index) = self.tab_index_of(terminal.entity_id()) {
                    let tab = &mut self.tabs[index];
                    if tab.tree.leaf_count() > 1 {
                        tab.zoomed = match tab.zoomed {
                            Some(_) => None,
                            None => Some(terminal.entity_id()),
                        };
                        cx.notify();
                    }
                }
            }
            GhosttyTerminalEvent::PwdChanged => {
                if let Some(index) = self.tab_index_of(terminal.entity_id()) {
                    self.refresh_shell_worktree(index, cx);
                }
            }
            GhosttyTerminalEvent::TitleChanged => {}
        }
    }

    /// The worktree line of a shell tab follows its focused terminal's
    /// directory (OSC 7), falling back to the workspace's own worktree.
    fn refresh_shell_worktree(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.tabs.get(index).is_some_and(|tab| !tab.claude_present) {
            self.apply_cwd_worktree(index, cx);
        }
    }

    /// The fork's `applyCwdWorktree`: the tab's directory (OSC 7) in the
    /// `<project>/worktrees/<name>` layout, else the workspace's own worktree.
    fn apply_cwd_worktree(&mut self, index: usize, cx: &mut Context<Self>) {
        let workspace_worktree = self.workspace_path.as_deref().and_then(worktree_split);
        let Some(tab) = self.tabs.get(index) else {
            return;
        };
        let pwd = tab
            .focused_terminal()
            .and_then(|terminal| terminal.read(cx).reported_directory().cloned());
        let split = match pwd {
            Some(pwd) => worktree_split(&pwd),
            None => workspace_worktree,
        };
        let (worktree, worktree_path) = match split {
            Some((_, name, path)) => (Some(name), Some(path)),
            None => (None, None),
        };
        self.set_worktree(index, worktree, worktree_path, cx);
    }

    fn set_worktree(
        &mut self,
        index: usize,
        worktree: Option<String>,
        worktree_path: Option<PathBuf>,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.tabs.get_mut(index) else {
            return;
        };
        if tab.worktree != worktree || tab.worktree_path != worktree_path {
            tab.worktree = worktree;
            tab.worktree_path = worktree_path;
            cx.notify();
        }
    }

    /// Per tab: its id, the foreground pids of its splits (focused first) and
    /// whether the user is looking at it (the terminal has focus in the active
    /// window and this tab is the selected one).
    pub(crate) fn claude_probes(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<(u64, Vec<i32>, bool)> {
        let column_focused =
            window.is_window_active() && self.focus_handle.contains_focused(window, cx);
        self.tabs
            .iter()
            .enumerate()
            .map(|(index, tab)| {
                let pids = tab
                    .terminals_focused_first()
                    .iter()
                    .filter_map(|terminal| terminal.read(cx).foreground_pid())
                    .map(|pid| pid as i32)
                    .collect();
                (tab.id, pids, column_focused && index == self.selected)
            })
            .collect()
    }

    /// The fork's `ClaudeTabStatus.apply`.
    pub(crate) fn apply_claude_results(
        &mut self,
        results: Vec<(u64, crate::claude_status::ProbeResult)>,
        cx: &mut Context<Self>,
    ) {
        // After every poll, like the fork: the strips, and the editor follow.
        cx.emit(TerminalColumnEvent::TabsChanged);
        for (tab_id, result) in results {
            let Some(index) = self.tabs.iter().position(|tab| tab.id == tab_id) else {
                continue;
            };
            let tab = &mut self.tabs[index];
            if result.pid.is_none() {
                let changed = tab.claude_title.is_some()
                    || tab.claude_state != ClaudeState::Absent
                    || tab.claude_present;
                tab.claude_title = None;
                tab.claude_state = ClaudeState::Absent;
                tab.claude_present = false;
                tab.claude_session = None;
                if changed {
                    cx.notify();
                }
                self.apply_cwd_worktree(index, cx);
                continue;
            }
            let title: Option<SharedString> = result.title.map(Into::into);
            if !tab.claude_present || tab.claude_title != title || tab.claude_state != result.state
            {
                tab.claude_present = true;
                tab.claude_title = title;
                tab.claude_state = result.state;
                cx.notify();
            }
            tab.claude_session = result
                .report
                .as_ref()
                .map(|report| report.session.clone())
                .filter(|session| !session.is_empty());
            match result.report.filter(|report| !report.worktree.is_empty()) {
                Some(report) => {
                    let path = (!report.worktree_path.is_empty())
                        .then(|| PathBuf::from(&report.worktree_path));
                    self.set_worktree(index, Some(report.worktree), path, cx);
                }
                None => self.apply_cwd_worktree(index, cx),
            }
        }
    }

    /// The blocked lamp's note, asked for in a sheet like the fork's alert.
    pub fn prompt_blocked_note(
        &mut self,
        tab_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.tabs.iter().find(|tab| tab.id == tab_id) else {
            return;
        };
        let Ok(ns_window) = crate::gpui_native_window(window) else {
            return;
        };
        let (sender, receiver) = futures::channel::oneshot::channel();
        unsafe { crate::sheets::ask_blocked_note(ns_window, &tab.blocked_note, sender) };
        cx.spawn(async move |this, cx| {
            if let Ok(Some(note)) = receiver.await {
                this.update(cx, |this, cx| {
                    if let Some(tab) = this.tabs.iter_mut().find(|tab| tab.id == tab_id) {
                        tab.blocked_note = note;
                        cx.notify();
                    }
                })
                .ok();
            }
        })
        .detach();
    }

    /// Occludes the terminals while another workspace is shown, so they stop
    /// drawing frames nobody sees.
    pub fn set_workspace_active(&mut self, active: bool, cx: &mut Context<Self>) {
        if self.workspace_active == active {
            return;
        }
        self.workspace_active = active;
        for (index, tab) in self.tabs.iter().enumerate() {
            for terminal in tab.terminals() {
                terminal
                    .read(cx)
                    .set_visible(active && index == self.selected);
            }
        }
    }

    pub fn is_fullscreen(&self) -> bool {
        self.fullscreen
    }

    /// The layout the workspace should use for this column's state.
    pub fn layout(&self) -> LeadingColumnLayout {
        match (self.fullscreen, self.terminal_side) {
            (false, _) => LeadingColumnLayout::Beside(self.column_width),
            (true, true) => LeadingColumnLayout::Full,
            (true, false) => LeadingColumnLayout::Hidden,
        }
    }

    /// winman's width factor: the column is 800 pt, 650 at 50 %.
    pub fn set_column_width(&mut self, width: Pixels, cx: &mut Context<Self>) {
        self.column_width = width;
        self.push_layout(cx);
    }

    /// q+f: fullscreen for the side you are on, which keeps the keyboard.
    /// Returns the new layout for the caller to apply when it is updating the
    /// workspace itself.
    pub fn toggle_fullscreen(
        &mut self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> LeadingColumnLayout {
        self.terminal_side = self.focus_handle.contains_focused(window, cx);
        self.fullscreen = !self.fullscreen;
        cx.notify();
        self.layout()
    }

    /// Records which side is about to get the keyboard, for callers that move
    /// it themselves while updating the workspace (so the hidden side is laid
    /// out before it is focused).
    pub fn prepare_side(&mut self, terminal_side: bool, cx: &mut App) -> LeadingColumnLayout {
        self.terminal_side = terminal_side;
        crate::winman::report_side(terminal_side, cx);
        self.layout()
    }

    fn set_terminal_side(&mut self, terminal_side: bool, cx: &mut Context<Self>) {
        if self.terminal_side != terminal_side {
            self.terminal_side = terminal_side;
            self.push_layout(cx);
        }
        crate::winman::report_side(terminal_side, cx);
    }

    pub fn set_displayed_in(&mut self, workspace: WeakEntity<Workspace>) {
        self.displayed_in = workspace;
    }

    fn push_layout(&self, cx: &mut Context<Self>) {
        let layout = self.layout();
        self.displayed_in
            .update(cx, |workspace, cx| {
                workspace.set_leading_column_layout(layout, cx)
            })
            .ok();
    }

    pub(crate) fn worktree_picker(&self) -> Option<&WorktreePicker> {
        self.worktree_picker.as_ref()
    }

    pub(crate) fn worktree_picker_mut(&mut self) -> Option<&mut WorktreePicker> {
        self.worktree_picker.as_mut()
    }

    /// Opens the worktree picker under the tab, or steps it when it is open
    /// already (winman's p+3 stepper, `pick-worktree` on the control socket).
    pub fn pick_worktree(
        &mut self,
        tab_id: u64,
        stepping: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> PickWorktree {
        if self.worktree_picker.is_some() {
            self.step_worktree_picker(1, true, cx);
            return PickWorktree::Stepped;
        }
        let Some(index) = self.tabs.iter().position(|tab| tab.id == tab_id) else {
            return PickWorktree::Unavailable;
        };
        if self.tabs[index].claude_present {
            return PickWorktree::ClaudeTab;
        }
        let Some(dir) = self.worktrees_dir.clone() else {
            return PickWorktree::NoWorktrees;
        };
        let entries = worktree_picker::list_worktrees(&dir);
        if entries.is_empty() {
            return PickWorktree::NoWorktrees;
        }
        let current = self.tabs[index].worktree.clone();
        let selected = entries
            .iter()
            .position(|entry| Some(&entry.name) == current.as_ref())
            .unwrap_or(0);
        // Under the tab, left-aligned, kept inside the column.
        let x = (index as f32 * self.tab_width())
            .min(self.bar_width - worktree_picker::WIDTH)
            .max(0.);
        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle, cx);
        self.worktree_picker = Some(WorktreePicker {
            tab_id,
            entries,
            selected,
            stepping,
            focus_handle,
            x,
            y: 1. + BAR_HEIGHT,
        });
        cx.notify();
        PickWorktree::Opened
    }

    pub(crate) fn step_worktree_picker(
        &mut self,
        delta: isize,
        apply: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(picker) = self.worktree_picker.as_mut() else {
            return;
        };
        picker.move_selection(delta);
        let tab_id = picker.tab_id;
        let entry = picker.entries.get(picker.selected).cloned();
        if apply && let Some(entry) = entry {
            self.apply_worktree(tab_id, &entry, cx);
        }
        cx.notify();
    }

    pub(crate) fn confirm_worktree_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(picker) = self.worktree_picker.take() else {
            return;
        };
        if !picker.stepping
            && let Some(entry) = picker.entries.get(picker.selected)
        {
            self.apply_worktree(picker.tab_id, entry, cx);
        }
        self.return_keyboard(picker.tab_id, window, cx);
    }

    pub fn close_worktree_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(picker) = self.worktree_picker.take() else {
            return false;
        };
        self.return_keyboard(picker.tab_id, window, cx);
        true
    }

    fn return_keyboard(&mut self, tab_id: u64, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = self.tabs.iter().position(|tab| tab.id == tab_id) {
            self.select_tab(index, window, cx);
        }
        cx.notify();
    }

    /// Types the `cd` into the tab. Ctrl-U and Return go through Ghostty's key
    /// encoder: the text path is a paste, which strips Ctrl-U and, under
    /// bracketed paste, would leave the newline unsubmitted.
    fn apply_worktree(&mut self, tab_id: u64, entry: &WorktreeEntry, cx: &mut Context<Self>) {
        let Some(terminal) = self
            .tabs
            .iter()
            .find(|tab| tab.id == tab_id)
            .and_then(|tab| tab.focused_terminal())
        else {
            return;
        };
        let path = entry.path.to_string_lossy().replace('\'', "'\\''");
        let terminal = terminal.read(cx);
        terminal.press_key(KEY_CODE_U, ffi::GHOSTTY_MODS_CTRL);
        terminal.input_text(&format!("cd '{path}' && clear"));
        terminal.press_key(KEY_CODE_RETURN, ffi::GHOSTTY_MODS_NONE);
        cx.emit(TerminalColumnEvent::WorktreeChosen(entry.path.clone()));
    }

    pub fn toggle_blocked(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        if let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == tab_id) {
            tab.blocked = !tab.blocked;
            if !tab.blocked {
                tab.blocked_note.clear();
            }
            cx.emit(TerminalColumnEvent::TabsChanged);
            cx.notify();
        }
    }
}

impl Focusable for TerminalColumn {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

/// The fork's `ThemedTabPalette`, derived from the terminal's colors.
struct Palette {
    line: Rgba,
    /// The bar's background: key-dependent and page-tinted.
    bar_color: Rgba,
    active_background: Rgba,
    inactive_background: Rgba,
    hover: Rgba,
    active_text: Rgba,
    inactive_text: Rgba,
}

fn luminance(color: Rgba) -> f32 {
    0.2126 * color.r + 0.7152 * color.g + 0.0722 * color.b
}

fn to_rgba(color: Hsla) -> Rgba {
    color.into()
}

impl Palette {
    fn new(window_is_key: bool, cx: &App) -> Self {
        let colors = runtime::terminal_colors();
        let background = colors.background;
        let foreground = colors.foreground;
        let dark = luminance(background) < 0.5;
        let (line, bar) = if dark {
            (rgb(0x665c54), rgb(0x32302f))
        } else {
            (rgb(0x94a0a1), rgb(0xeee8d5))
        };
        // Same base and page tint as the editor's bars (`ui::winman`), chosen by
        // the terminal background's luminance like the fork does.
        let bar_color = if window_is_key {
            to_rgba(ui::winman_bar_background(true, Hsla::from(background), cx))
        } else {
            bar
        };
        Self {
            line,
            bar_color,
            active_background: background,
            inactive_background: bar,
            hover: mix(background, foreground, 0.12),
            active_text: foreground,
            inactive_text: mix(background, foreground, 0.55),
        }
    }
}

fn with_opacity(color: Rgba, opacity: f32) -> Rgba {
    Rgba {
        a: color.a * opacity,
        ..color
    }
}

fn bitmap_element(bitmap: Option<Bitmap>) -> Option<AnyElement> {
    let bitmap = bitmap?;
    Some(
        img(bitmap.image)
            .w(px(bitmap.width))
            .h(px(bitmap.height))
            .flex_none()
            .into_any_element(),
    )
}

/// A 1-point-per-pixel dithered image positioned absolutely.
fn ramp_at(
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    top: Rgba,
    bottom: Rgba,
    levels: u32,
    scale: f32,
) -> Option<AnyElement> {
    let bitmap = graphics::dithered_gradient(
        width.floor() as u32,
        height.floor() as u32,
        top,
        bottom,
        levels,
        scale,
    )?;
    Some(
        div()
            .absolute()
            .left(px(x))
            .top(px(y))
            .child(img(bitmap.image).w(px(bitmap.width)).h(px(bitmap.height)))
            .into_any_element(),
    )
}

fn fill_at(x: f32, y: f32, width: f32, height: f32, color: Rgba) -> AnyElement {
    div()
        .absolute()
        .left(px(x))
        .top(px(y))
        .w(px(width))
        .h(px(height))
        .bg(color)
        .into_any_element()
}

impl TerminalColumn {
    /// The tab row's vertical centre and each tab's left edge, relative to
    /// the column's top-left (the window's, since the column sits at its left).
    pub fn tab_positions(&self) -> (f32, Vec<f32>) {
        let width = self.tab_width();
        let xs = (0..self.tabs.len())
            .map(|index| index as f32 * width)
            .collect();
        (1. + BAR_HEIGHT / 2., xs)
    }

    fn tab_width(&self) -> f32 {
        let count = self.tabs.len().max(1) as f32;
        let available = (self.bar_width - NEW_TAB_BUTTON_WIDTH).max(0.);
        (available / count).clamp(MIN_TAB_WIDTH, MAX_TAB_WIDTH)
    }

    fn render_icon(
        &self,
        tab: &TerminalTab,
        amiga: bool,
        scale: f32,
        window: &mut Window,
    ) -> Option<AnyElement> {
        let spinning = |window: &mut Window| {
            // Keep animating while a gear turns.
            window.request_animation_frame();
            graphics::quantize_phase(graphics::gear_phase(), GEAR_FRAMES)
        };
        if amiga {
            match tab.claude_state {
                ClaudeState::Working => {
                    let phase = spinning(window);
                    return bitmap_element(graphics::pixel_gear(rgb(LAMP_WORKING), phase, scale));
                }
                ClaudeState::Absent if !tab.blocked && tab.claude_present => {
                    return bitmap_element(graphics::pixel_gear(rgb(LAMP_IDLE), 0., scale));
                }
                ClaudeState::Done | ClaudeState::Absent if tab.blocked => {
                    return Some(
                        div()
                            .size(px(16.))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .children(bitmap_element(graphics::pixel_no_entry(scale)))
                            .into_any_element(),
                    );
                }
                _ => {}
            }
        }
        match tab.claude_state {
            ClaudeState::Working => {
                let phase = spinning(window);
                bitmap_element(graphics::sf_symbol(
                    "gearshape.fill",
                    13. * 0.8,
                    SymbolWeight::Semibold,
                    rgb(LAMP_WORKING),
                    Some(13.),
                    phase,
                    scale,
                ))
            }
            ClaudeState::Question => bitmap_element(graphics::sf_symbol(
                "questionmark",
                11.,
                SymbolWeight::Bold,
                rgb(LAMP_QUESTION),
                None,
                0.,
                scale,
            )),
            ClaudeState::Background => Some(
                div()
                    .size(px(8.))
                    .flex_none()
                    .bg(rgb(LAMP_BACKGROUND))
                    .into_any_element(),
            ),
            ClaudeState::Done if !tab.blocked => bitmap_element(graphics::sf_symbol(
                "checkmark",
                11.,
                SymbolWeight::Bold,
                rgb(LAMP_DONE),
                None,
                0.,
                scale,
            )),
            ClaudeState::Done | ClaudeState::Absent if tab.blocked => Some(
                div()
                    .size(px(16.))
                    .flex_none()
                    .relative()
                    .flex()
                    .items_center()
                    .justify_center()
                    .children(bitmap_element(graphics::prohibited_mark(
                        rgb(LAMP_BLOCKED),
                        10.,
                        scale,
                    )))
                    .when(!tab.blocked_note.is_empty(), |this| {
                        this.child(
                            div()
                                .absolute()
                                .top(px(1.))
                                .left(px(1.))
                                .size(px(14.))
                                .border_1()
                                .border_color(rgb(LAMP_BLOCKED)),
                        )
                    })
                    .into_any_element(),
            ),
            ClaudeState::Absent if tab.claude_present => bitmap_element(graphics::sf_symbol(
                "gearshape.fill",
                11.,
                SymbolWeight::Semibold,
                rgb(LAMP_IDLE),
                None,
                0.,
                scale,
            )),
            _ => None,
        }
    }

    fn render_amiga_tab_face(
        &self,
        active: bool,
        hovering: bool,
        width: f32,
        palette: &Palette,
        accent: Rgba,
        scale: f32,
    ) -> Vec<AnyElement> {
        let w = width.floor();
        let h = BAR_HEIGHT;
        let bar = palette.bar_color;
        let mut elements = Vec::new();
        if active {
            let terminal = palette.active_background;
            let face = lighten(terminal, 0.09);
            elements.extend(ramp_at(
                0.,
                0.,
                w,
                h,
                lighten(face, 0.03),
                terminal,
                3,
                scale,
            ));
            elements.push(fill_at(0., 0., w, 2., accent));
            elements.push(fill_at(0., 2., w, 1., lighten(accent, 0.45)));
            elements.push(fill_at(0., 0., 1., h, darken(bar, 0.55)));
            elements.push(fill_at(w - 1., 0., 1., h, darken(bar, 0.55)));
        } else {
            let base = if hovering { lighten(bar, 0.06) } else { bar };
            elements.extend(ramp_at(
                0.,
                0.,
                w,
                h - 1.,
                lighten(base, 0.03),
                darken(base, 0.06),
                3,
                scale,
            ));
            elements.push(fill_at(0., 0., w, 1., lighten(base, 0.10)));
            elements.push(fill_at(w - 2., 3., 1., h - 7., darken(bar, 0.45)));
            elements.push(fill_at(w - 1., 3., 1., h - 7., lighten(bar, 0.10)));
        }
        elements
    }

    fn render_tab(
        &self,
        index: usize,
        tab: &TerminalTab,
        palette: &Palette,
        amiga: bool,
        accent: Rgba,
        width: f32,
        scale: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let active = index == self.selected;
        let group: SharedString = format!("ghostty-tab-{}", tab.id).into();
        let text_color = if active {
            palette.active_text
        } else {
            palette.inactive_text
        };
        let title_color = if amiga {
            rgb(if active { 0xe0d0ae } else { 0xbdae93 })
        } else {
            with_opacity(text_color, if active { 0.72 } else { 0.6 })
        };
        let worktree_color = if amiga {
            rgb(if active { 0xa89984 } else { 0x7c6f64 })
        } else {
            with_opacity(text_color, if active { 0.62 } else { 0.5 })
        };
        let can_flag_blocked = matches!(tab.claude_state, ClaudeState::Done | ClaudeState::Absent);
        let tab_id = tab.id;
        let title: SharedString = tab
            .claude_title
            .clone()
            .unwrap_or_else(|| self.title_path.clone());
        let interactive_worktree = !tab.claude_present && self.worktrees_dir.is_some();

        let icon = self.render_icon(tab, amiga, scale, window).map(|icon| {
            let blocked_lamp =
                tab.blocked && matches!(tab.claude_state, ClaudeState::Done | ClaudeState::Absent);
            if blocked_lamp {
                let tooltip = if tab.blocked_note.is_empty() {
                    SharedString::from("Blocked. Click to say what by.")
                } else {
                    SharedString::from(tab.blocked_note.clone())
                };
                div()
                    .id(("ghostty-blocked-lamp", tab.id))
                    .flex_none()
                    .child(icon)
                    .tooltip(ui::Tooltip::text(tooltip))
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        cx.stop_propagation();
                        this.prompt_blocked_note(tab_id, window, cx);
                    }))
                    .into_any_element()
            } else {
                icon
            }
        });
        let title_row = div()
            .h(px(TITLE_ROW_HEIGHT))
            .w_full()
            .flex()
            .items_center()
            .gap(px(6.))
            .pr(px(4.))
            .children(icon)
            .child(
                div()
                    .id(("ghostty-tab-title", tab.id))
                    .min_w_0()
                    .text_size(px(11.))
                    .text_color(title_color)
                    .truncate()
                    .child(title)
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        cx.stop_propagation();
                        if active && can_flag_blocked {
                            this.toggle_blocked(tab_id, cx);
                        } else {
                            this.select_tab(index, window, cx);
                        }
                    })),
            );

        let worktree_row = match tab.worktree.clone().filter(|name| !name.is_empty()) {
            Some(name) => {
                let hover_group: SharedString = format!("ghostty-worktree-{}", tab.id).into();
                div()
                    .id(("ghostty-tab-worktree", tab.id))
                    .group(hover_group.clone())
                    .h(px(WORKTREE_ROW_HEIGHT))
                    .w_full()
                    .flex()
                    .items_center()
                    .gap(px(3.))
                    .relative()
                    .top(px(-2.))
                    .when(interactive_worktree, |this| {
                        this.hover(|style| style.bg(palette.hover))
                    })
                    .child(
                        div()
                            .min_w_0()
                            .text_size(px(9.))
                            .text_color(worktree_color)
                            .truncate()
                            .child(name),
                    )
                    .when(interactive_worktree, |this| {
                        let chevron = |opacity: f32| {
                            bitmap_element(graphics::sf_symbol(
                                "chevron.down",
                                6.,
                                SymbolWeight::Semibold,
                                with_opacity(text_color, 1.),
                                None,
                                0.,
                                scale,
                            ))
                            .map(|element| div().opacity(opacity).child(element))
                        };
                        this.children(chevron(0.35).map(|element| {
                            element.group_hover(hover_group.clone(), |style| style.opacity(0.85))
                        }))
                    })
                    .tooltip(ui::Tooltip::text(if interactive_worktree {
                        "This shell's worktree. Click to switch."
                    } else {
                        "The worktree Claude is working in"
                    }))
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        cx.stop_propagation();
                        if interactive_worktree && active {
                            this.pick_worktree(tab_id, false, window, cx);
                        } else {
                            this.select_tab(index, window, cx);
                        }
                    }))
                    .into_any_element()
            }
            None => div().h(px(WORKTREE_ROW_HEIGHT)).into_any_element(),
        };

        let close_button = div()
            .id(("ghostty-tab-close", tab.id))
            .w(px(CLOSE_BUTTON_WIDTH))
            .h(px(BAR_HEIGHT))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .hover(|style| style.bg(palette.hover))
            .children(bitmap_element(graphics::sf_symbol(
                "xmark",
                10.,
                SymbolWeight::Bold,
                text_color,
                None,
                0.,
                scale,
            )))
            .when(!active, |this| {
                this.invisible()
                    .group_hover(group.clone(), |style| style.visible())
            })
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                cx.stop_propagation();
                if let Some(index) = this.tabs.iter().position(|tab| tab.id == tab_id) {
                    this.close_tab(index, true, window, cx);
                }
            }));

        let face: Vec<AnyElement> = if amiga {
            let normal = self.render_amiga_tab_face(active, false, width, palette, accent, scale);
            if active {
                normal
            } else {
                // Two faces, the hovered one shown by the group hover.
                let hovered =
                    self.render_amiga_tab_face(active, true, width, palette, accent, scale);
                vec![
                    div()
                        .absolute()
                        .size_full()
                        .children(normal)
                        .into_any_element(),
                    div()
                        .absolute()
                        .size_full()
                        .invisible()
                        .group_hover(group.clone(), |style| style.visible())
                        .children(hovered)
                        .into_any_element(),
                ]
            }
        } else {
            Vec::new()
        };

        div()
            .id(("ghostty-tab", tab.id))
            .group(group)
            .relative()
            .w(px(width))
            .h(px(BAR_HEIGHT))
            .flex_none()
            .flex()
            .when(!amiga, |this| {
                if active {
                    this.bg(palette.active_background)
                } else {
                    this.bg(palette.inactive_background)
                        .hover(|style| style.bg(palette.hover))
                }
            })
            .children(face)
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .pl(px(10.))
                    .pr(px(4.))
                    .flex()
                    .flex_col()
                    .child(title_row)
                    .child(worktree_row),
            )
            .child(close_button)
            .when(!amiga, |this| {
                this.child(fill_at(width - 1., 0., 1., BAR_HEIGHT, palette.line))
                    .when(!active, |this| {
                        this.child(fill_at(0., BAR_HEIGHT - 1., width, 1., palette.line))
                    })
            })
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.select_tab(index, window, cx);
            }))
            .into_any_element()
    }

    fn render_tab_bar(
        &self,
        palette: &Palette,
        amiga: bool,
        scale: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let accent = if amiga {
            to_rgba(ui::winman_amiga_accent(cx))
        } else {
            palette.bar_color
        };
        let tab_width = self.tab_width();
        let tabs: Vec<AnyElement> = self
            .tabs
            .iter()
            .enumerate()
            .map(|(index, tab)| {
                self.render_tab(
                    index, tab, palette, amiga, accent, tab_width, scale, window, cx,
                )
            })
            .collect();
        let bar_width = self.bar_width;
        let bar = palette.bar_color;
        let entity = cx.entity();

        let background: Vec<AnyElement> = if amiga {
            let mut elements = Vec::new();
            elements.extend(ramp_at(
                0.,
                0.,
                bar_width,
                BAR_HEIGHT,
                lighten(bar, 0.04),
                darken(bar, 0.10),
                4,
                scale,
            ));
            elements.push(fill_at(
                0.,
                BAR_HEIGHT - 1.,
                bar_width,
                1.,
                darken(bar, 0.5),
            ));
            elements
        } else {
            vec![
                fill_at(0., 0., bar_width, BAR_HEIGHT, bar),
                fill_at(0., BAR_HEIGHT - 1., bar_width, 1., palette.line),
            ]
        };

        div()
            .relative()
            .w_full()
            .h(px(BAR_HEIGHT))
            .flex_none()
            .overflow_hidden()
            .child(
                canvas(
                    move |bounds, _window, cx| {
                        let width = f32::from(bounds.size.width).floor();
                        entity.update(cx, |this, cx| {
                            if this.bar_width != width {
                                this.bar_width = width;
                                cx.notify();
                            }
                        });
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .children(background)
            .child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full()
                    .flex()
                    .child(
                        div()
                            .id("ghostty-tabs-scroll")
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .flex()
                            .overflow_x_scroll()
                            .children(tabs),
                    )
                    .child(
                        div()
                            .id("ghostty-new-tab")
                            .w(px(NEW_TAB_BUTTON_WIDTH))
                            .h(px(BAR_HEIGHT))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .children(bitmap_element(graphics::sf_symbol(
                                "plus",
                                12.,
                                SymbolWeight::Medium,
                                palette.inactive_text,
                                None,
                                0.,
                                scale,
                            )))
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.new_tab_following_worktree(window, cx);
                            })),
                    ),
            )
            .into_any_element()
    }

    fn render_bottom_strip(&self, palette: &Palette, amiga: bool, scale: f32) -> AnyElement {
        let bar = palette.bar_color;
        let width = self.bar_width;
        if amiga {
            div()
                .relative()
                .w_full()
                .h(px(BOTTOM_BAND_HEIGHT + 1.))
                .flex_none()
                .overflow_hidden()
                .child(fill_at(0., 0., width, 1., darken(bar, 0.5)))
                .child(fill_at(0., 1., width, 1., lighten(bar, 0.08)))
                .children(ramp_at(
                    0.,
                    2.,
                    width,
                    BOTTOM_BAND_HEIGHT - 1.,
                    lighten(bar, 0.02),
                    darken(bar, 0.10),
                    3,
                    scale,
                ))
                .into_any_element()
        } else {
            div()
                .w_full()
                .flex_none()
                .flex()
                .flex_col()
                .child(div().w_full().h(px(1.)).bg(palette.line))
                .child(div().w_full().h(px(BOTTOM_BAND_HEIGHT)).bg(bar))
                .into_any_element()
        }
    }

    fn render_split(
        &self,
        tab: &TerminalTab,
        node: &SplitNode,
        path: Vec<bool>,
        leaf_count: usize,
        focused: Option<EntityId>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match node {
            SplitNode::Leaf(terminal) => {
                let dim = leaf_count > 1 && Some(terminal.entity_id()) != focused;
                let overlay_opacity =
                    1. - runtime::config_f64("unfocused-split-opacity").unwrap_or(0.85) as f32;
                let fill = runtime::config_color("unfocused-split-fill")
                    .unwrap_or_else(|| runtime::terminal_colors().background);
                div()
                    .relative()
                    .size_full()
                    .child(terminal.clone())
                    .when(dim && overlay_opacity > 0., |this| {
                        this.child(
                            div()
                                .absolute()
                                .top_0()
                                .left_0()
                                .size_full()
                                .bg(with_opacity(fill, overlay_opacity)),
                        )
                    })
                    .into_any_element()
            }
            SplitNode::Split {
                axis,
                ratio,
                first,
                second,
            } => {
                let mut first_path = path.clone();
                first_path.push(false);
                let mut second_path = path.clone();
                second_path.push(true);
                let first_element =
                    self.render_split(tab, first, first_path, leaf_count, focused, cx);
                let second_element =
                    self.render_split(tab, second, second_path, leaf_count, focused, cx);
                let divider_color =
                    runtime::config_color("split-divider-color").unwrap_or_else(|| {
                        let background = runtime::terminal_colors().background;
                        if luminance(background) >= 0.5 {
                            darken(background, 0.08)
                        } else {
                            darken(background, 0.4)
                        }
                    });
                let tab_id = tab.id;
                let entity = cx.entity();
                let record_path = path.clone();
                let drag_path = path.clone();
                let horizontal = *axis == Axis::Horizontal;
                let divider = div()
                    .id(SharedString::from(format!(
                        "ghostty-divider-{tab_id}-{path:?}"
                    )))
                    .relative()
                    .flex_none()
                    .bg(divider_color)
                    .when(horizontal, |this| this.w(px(1.)).h_full())
                    .when(!horizontal, |this| this.h(px(1.)).w_full())
                    // A wider invisible handle to grab, like Ghostty's splitter.
                    .child(
                        div()
                            .id(SharedString::from(format!(
                                "ghostty-divider-handle-{tab_id}-{path:?}"
                            )))
                            .absolute()
                            .when(horizontal, |this| {
                                this.top_0()
                                    .bottom_0()
                                    .left(px(-3.))
                                    .w(px(7.))
                                    .cursor_col_resize()
                            })
                            .when(!horizontal, |this| {
                                this.left_0()
                                    .right_0()
                                    .top(px(-3.))
                                    .h(px(7.))
                                    .cursor_row_resize()
                            })
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                                    cx.stop_propagation();
                                    this.dragging_divider = Some((tab_id, drag_path.clone()));
                                }),
                            ),
                    );
                div()
                    .size_full()
                    .flex()
                    .when(!horizontal, |this| this.flex_col())
                    .child(
                        canvas(
                            move |bounds, _window, cx| {
                                entity.update(cx, |this, _cx| {
                                    this.split_bounds.retain(|(tab, path, _)| {
                                        !(*tab == tab_id && *path == record_path)
                                    });
                                    this.split_bounds
                                        .push((tab_id, record_path.clone(), bounds));
                                });
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .size_full(),
                    )
                    .child(
                        div()
                            .when(horizontal, |this| this.h_full().w(gpui::relative(*ratio)))
                            .when(!horizontal, |this| this.w_full().h(gpui::relative(*ratio)))
                            .flex_none()
                            .overflow_hidden()
                            .child(first_element),
                    )
                    .child(divider)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .min_h_0()
                            .overflow_hidden()
                            .child(second_element),
                    )
                    .relative()
                    .into_any_element()
            }
        }
    }

    fn drag_divider(&mut self, position: gpui::Point<Pixels>, cx: &mut Context<Self>) {
        let Some((tab_id, path)) = self.dragging_divider.clone() else {
            return;
        };
        let Some(bounds) = self
            .split_bounds
            .iter()
            .find(|(tab, split_path, _)| *tab == tab_id && *split_path == path)
            .map(|(_, _, bounds)| *bounds)
        else {
            return;
        };
        let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == tab_id) else {
            return;
        };
        if let Some((ratio, axis)) = tab.tree.ratio_at_mut(&path) {
            let fraction = match axis {
                Axis::Horizontal => {
                    f32::from(position.x - bounds.origin.x) / f32::from(bounds.size.width)
                }
                Axis::Vertical => {
                    f32::from(position.y - bounds.origin.y) / f32::from(bounds.size.height)
                }
            };
            *ratio = fraction.clamp(0.05, 0.95);
            cx.notify();
        }
    }
}

impl Render for TerminalColumn {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_started(window, cx);
        let palette = Palette::new(window.is_window_active(), cx);
        let amiga = ui::winman_amiga(cx);
        let scale = window.scale_factor();

        let content = self.tabs.get(self.selected).map(|tab| {
            let focused = tab.focused_terminal().map(|terminal| terminal.entity_id());
            match tab.zoomed.and_then(|zoomed| {
                tab.tree
                    .leaves()
                    .into_iter()
                    .find(|leaf| leaf.entity_id() == zoomed)
            }) {
                Some(zoomed) => div().size_full().child(zoomed).into_any_element(),
                None => {
                    let leaf_count = tab.tree.leaf_count();
                    self.render_split(tab, &tab.tree, Vec::new(), leaf_count, focused, cx)
                }
            }
        });

        let tab_bar = self.render_tab_bar(&palette, amiga, scale, window, cx);
        let bottom_strip = self.render_bottom_strip(&palette, amiga, scale);

        div()
            .id("ghostty-terminal-column")
            .key_context("GhosttyTerminalColumn")
            .track_focus(&self.focus_handle)
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(palette.active_background)
            .text_color(palette.active_text)
            .font_family(".SystemUIFont")
            .child(div().w_full().h(px(1.)).flex_none().bg(palette.line))
            .child(tab_bar)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .overflow_hidden()
                    .children(content),
            )
            .child(bottom_strip)
            .children(self.render_worktree_picker(cx))
            .child(fill_at(self.bar_width - 1., 0., 1., 10000., palette.line))
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _window, cx| {
                if this.dragging_divider.is_some() {
                    this.drag_divider(event.position, cx);
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _window, _cx| {
                    this.dragging_divider = None;
                }),
            )
    }
}
