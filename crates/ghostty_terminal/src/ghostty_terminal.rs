//! A terminal backed by Ghostty, in a column beside the editor.
//!
//! macOS embeds the whole of libghostty (`terminal_mac.rs`, Ghostty's own Metal
//! renderer). Linux embeds libghostty-vt, Ghostty's terminal core, and draws
//! its cells with GPUI (`terminal_linux.rs`). Each platform file provides the
//! same `GhosttyTerminal` and the same `runtime`, `sheets` and `ffi` modules,
//! so the column, tabs, sessions and winman's socket are shared.

#![cfg(any(target_os = "macos", target_os = "linux"))]

mod claude_status;
mod close_confirm;
mod columns;
mod command_palette;
mod graphics;
pub mod lf_view;
mod remote_session;
mod session_activity;
mod tab_sessions;
mod terminal_column;
mod winman;
mod worktree_picker;

#[cfg(target_os = "macos")]
mod input_view;
#[cfg(target_os = "macos")]
mod runtime;
#[cfg(target_os = "macos")]
mod sheets;
#[cfg(target_os = "macos")]
use ghostty_embed as ffi;

#[cfg(target_os = "linux")]
#[path = "linux_runtime.rs"]
mod runtime;
#[cfg(target_os = "linux")]
#[path = "linux_sheets.rs"]
mod sheets;
#[cfg(target_os = "linux")]
#[path = "linux_ffi.rs"]
mod ffi;

pub use columns::TerminalColumns;
pub use terminal_column::{
    ClaudeState, PickWorktree, TerminalColumn, TerminalColumnEvent, TerminalTab, worktree_split,
};

use gpui::{App, AppContext as _, Context, Entity, Focusable as _, Window, actions, px};
use std::path::PathBuf;
use gpui::WeakEntity;
use workspace::Workspace;

#[cfg(target_os = "macos")]
#[path = "terminal_mac.rs"]
mod terminal;
#[cfg(target_os = "linux")]
#[path = "terminal_linux.rs"]
mod terminal;
pub use terminal::*;

actions!(
    ghostty_terminal,
    [
        /// Opens a new Ghostty terminal in the active pane.
        NewGhosttyTerminal,
        /// Fullscreen for the side that has the keyboard (terminal or editor).
        ToggleFullscreen,
        /// Moves the keyboard to the terminal column.
        FocusTerminal,
        /// Moves the keyboard to the editor.
        FocusEditor,
        /// Opens the Ghostty config file in the editor.
        OpenConfig,
        /// Reloads the Ghostty config files.
        ReloadConfig,
    ]
);

fn column_of(workspace: &Workspace) -> Option<Entity<TerminalColumn>> {
    workspace
        .leading_column()?
        .clone()
        .downcast::<TerminalColumn>()
        .ok()
}

/// Moves the keyboard to the workspace's active editor (or pane).
pub fn focus_editor(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    if let Some(column) = column_of(workspace) {
        let layout = column.update(cx, |column, cx| column.prepare_side(false, cx));
        workspace.set_leading_column_layout(layout, cx);
    }
    let focus_handle = match workspace.active_item(cx) {
        Some(item) => item.item_focus_handle(cx),
        None => workspace.active_pane().focus_handle(cx),
    };
    window.focus(&focus_handle, cx);
}

/// Shows `target` in the unified window without moving the keyboard out of the
/// terminal. `MultiWorkspace::activate` focuses the new workspace's pane, which
/// is right for a worktree switch and wrong when the editor only follows the
/// terminal tab's worktree: p+q/w to a tab in another worktree then left the
/// keyboard, and winman's virtual keys, on the editor.
pub fn show_workspace_keeping_terminal_focus(
    multi_workspace: &mut workspace::MultiWorkspace,
    target: Entity<Workspace>,
    window: &mut Window,
    cx: &mut Context<workspace::MultiWorkspace>,
) {
    if multi_workspace.workspace() == &target {
        return;
    }
    let terminal = ui::winman_terminal_focused(window, cx)
        .then(|| TerminalColumns::current(cx))
        .flatten();
    multi_workspace.activate(target, None, window, cx);
    if let Some(column) = terminal {
        window.focus(&column.focus_handle(cx), cx);
    }
}

/// winman's terminal width (800 pt, 650 at a 50 % width factor) for every
/// terminal column.
pub fn set_terminal_width(width: f32, cx: &mut App) {
    for column in TerminalColumns::all(cx) {
        column.update(cx, |column, cx| column.set_column_width(px(width), cx));
    }
}

/// winman's q+f: fullscreen for the side that has the keyboard.
pub fn toggle_fullscreen(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if let Some(column) = column_of(workspace) {
        let layout = column.update(cx, |column, cx| column.toggle_fullscreen(window, cx));
        workspace.set_leading_column_layout(layout, cx);
    }
}

/// Moves the keyboard to the terminal column. In fullscreen the column is
/// laid out first, since a hidden column cannot take focus.
pub fn focus_terminal(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    if let Some(column) = column_of(workspace) {
        let layout = column.update(cx, |column, cx| column.prepare_side(true, cx));
        workspace.set_leading_column_layout(layout, cx);
        window.focus(&column.focus_handle(cx), cx);
    }
}

pub use winman::owns_workspaces;

pub fn init(cx: &mut App) {
    // Zed and the terminal are one app with one window holding every
    // workspace.
    cx.set_global(workspace::UnifiedWindow);
    winman::init(cx);

    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else {
            return;
        };
        if workspace.project().read(cx).is_local() {
            let handle = cx.entity().downgrade();
            let project = workspace.project().clone();
            let column =
                cx.new(|cx| TerminalColumn::new(handle.clone(), Some(project), window, cx));
            TerminalColumns::register(handle, column.clone(), cx);
            winman::watch_column(&column, cx);
            workspace.set_leading_column(Some(column.into()), cx);
            cx.on_release(|_, cx| {
                // `on_release` has no handle to the released entity; drop every
                // registration whose workspace is gone.
                if let Some(columns) = cx.try_global::<TerminalColumns>() {
                    let gone: Vec<_> = columns
                        .owners()
                        .filter(|owner| owner.upgrade().is_none())
                        .collect();
                    for owner in gone {
                        TerminalColumns::unregister(&owner, cx);
                    }
                }
            })
            .detach();
        }
        workspace.register_action(|workspace, _: &ToggleFullscreen, window, cx| {
            toggle_fullscreen(workspace, window, cx);
        });
        workspace.register_action(|workspace, _: &FocusTerminal, window, cx| {
            focus_terminal(workspace, window, cx);
        });
        workspace.register_action(|workspace, _: &FocusEditor, window, cx| {
            focus_editor(workspace, window, cx);
        });
        workspace.register_action(|_, _: &OpenConfig, _, cx| runtime::open_config(cx));
        workspace.register_action(|_, _: &ReloadConfig, _, _| runtime::reload_config());
    })
    .detach();

    // Pair the current terminal with whichever editor the window shows, and
    // let only that column draw.
    cx.observe_new(|_: &mut workspace::MultiWorkspace, window, cx| {
        let Some(window) = window else {
            return;
        };
        cx.subscribe_in(
            &cx.entity(),
            window,
            |multi_workspace, _, event, window, cx| {
                if matches!(
                    event,
                    workspace::MultiWorkspaceEvent::ActiveWorkspaceChanged { .. }
                        | workspace::MultiWorkspaceEvent::WorkspaceAdded(_)
                        | workspace::MultiWorkspaceEvent::WorkspaceRemoved(_)
                ) {
                    columns::sync(multi_workspace, window, cx);
                }
            },
        )
        .detach();
    })
    .detach();

    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &NewGhosttyTerminal, window, cx| {
            let working_directory = workspace
                .project()
                .read(cx)
                .visible_worktrees(cx)
                .next()
                .map(|worktree| worktree.read(cx).abs_path().to_path_buf());
            let options = TerminalOptions {
                working_directory,
                ..Default::default()
            };
            match GhosttyTerminal::open(options, window, cx) {
                Ok(terminal) => {
                    workspace.add_item_to_active_pane(Box::new(terminal), None, true, window, cx);
                }
                Err(error) => {
                    log::error!("failed to open a Ghostty terminal: {error:#}");
                    workspace.show_error(&error, cx);
                }
            }
        });
    })
    .detach();
}

pub enum GhosttyTerminalEvent {
    TitleChanged,
    PwdChanged,
    CloseRequested {
        process_alive: bool,
    },
    Focused,
    NewSplit(ffi::ghostty_action_split_direction_e),
    NewTab,
    CloseTab(ffi::ghostty_action_close_tab_mode_e),
    GotoTab(i32),
    GotoSplit(ffi::ghostty_action_goto_split_e),
    ResizeSplit {
        direction: ffi::ghostty_action_resize_split_direction_e,
        amount: u16,
    },
    EqualizeSplits,
    ToggleSplitZoom,
    ToggleCommandPalette,
}

/// How to start a new terminal.
#[derive(Default)]
pub struct TerminalOptions {
    pub working_directory: Option<PathBuf>,
    /// Run instead of the configured shell; the terminal closes when it exits.
    pub command: Option<String>,
    /// Typed into the shell once it starts, e.g. `claude --resume ...\n`.
    pub initial_input: Option<String>,
    /// Inherit font size, working directory etc. from this terminal, the way a
    /// new Ghostty tab or split does.
    pub inherit_from: Option<(WeakEntity<GhosttyTerminal>, InheritContext)>,
}

#[derive(Clone, Copy)]
pub enum InheritContext {
    Tab,
    Split,
}

