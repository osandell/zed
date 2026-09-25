//! Which terminal column the window shows.
//!
//! Every workspace owns a terminal column for its worktree, but the column the
//! window shows is not always the active workspace's own. winman's editor
//! follows the work: a Claude session in the `main` worktree, or a shell that
//! `cd`s, moves the editor to another worktree while the terminal stays
//! exactly where the user is. So the window pairs the *current terminal* (set
//! when winman focuses a terminal) with the active workspace's editor, the way
//! winman used to place a Ghostty window beside a Zed window of another path.

use std::path::Path;

use gpui::{App, Context, Entity, Global, WeakEntity, Window};
use workspace::{MultiWorkspace, Workspace};

use crate::TerminalColumn;

#[derive(Default)]
pub struct TerminalColumns {
    /// Each column with the workspace that owns it. Columns are kept alive
    /// here while no workspace shows them.
    columns: Vec<(WeakEntity<Workspace>, Entity<TerminalColumn>)>,
    current: Option<WeakEntity<TerminalColumn>>,
}

impl Global for TerminalColumns {}

impl TerminalColumns {
    pub fn register(owner: WeakEntity<Workspace>, column: Entity<TerminalColumn>, cx: &mut App) {
        cx.default_global::<TerminalColumns>()
            .columns
            .push((owner, column));
    }

    pub fn owners(&self) -> impl Iterator<Item = WeakEntity<Workspace>> + '_ {
        self.columns.iter().map(|(owner, _)| owner.clone())
    }

    pub fn unregister(owner: &WeakEntity<Workspace>, cx: &mut App) {
        let columns = cx.default_global::<TerminalColumns>();
        columns.columns.retain(|(existing, _)| existing != owner);
    }

    pub fn column_for_workspace(
        workspace: &WeakEntity<Workspace>,
        cx: &App,
    ) -> Option<Entity<TerminalColumn>> {
        cx.try_global::<TerminalColumns>()?
            .columns
            .iter()
            .find(|(owner, _)| owner == workspace)
            .map(|(_, column)| column.clone())
    }

    /// The column of the worktree at `path` (winman's window title).
    pub fn column_for_path(path: &Path, cx: &App) -> Option<Entity<TerminalColumn>> {
        cx.try_global::<TerminalColumns>()?
            .columns
            .iter()
            .find(|(_, column)| {
                column
                    .read(cx)
                    .workspace_path()
                    .is_some_and(|own| own == path)
            })
            .map(|(_, column)| column.clone())
    }

    pub fn all(cx: &App) -> Vec<Entity<TerminalColumn>> {
        cx.try_global::<TerminalColumns>()
            .map(|columns| {
                columns
                    .columns
                    .iter()
                    .map(|(_, column)| column.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn current(cx: &App) -> Option<Entity<TerminalColumn>> {
        cx.try_global::<TerminalColumns>()?
            .current
            .as_ref()?
            .upgrade()
    }

    /// Shows `column` beside whatever editor the window shows.
    pub fn set_current(column: &Entity<TerminalColumn>, cx: &mut App) {
        cx.default_global::<TerminalColumns>().current = Some(column.downgrade());
        if let Some(window) = workspace::unified_window_handle(cx) {
            window
                .update(cx, |multi_workspace, window, cx| {
                    sync(multi_workspace, window, cx)
                })
                .ok();
        }
    }
}

/// Puts the current terminal next to the active workspace, hides every other
/// column and lets only the shown one draw.
pub fn sync(
    multi_workspace: &mut MultiWorkspace,
    window: &mut Window,
    cx: &mut Context<MultiWorkspace>,
) {
    let active = multi_workspace.workspace().clone();
    let owned = TerminalColumns::column_for_workspace(&active.downgrade(), cx);
    let shown = TerminalColumns::current(cx)
        .filter(|column| {
            // A column whose workspace has closed is gone with it.
            cx.try_global::<TerminalColumns>().is_some_and(|columns| {
                columns
                    .columns
                    .iter()
                    .any(|(_, existing)| existing == column)
            })
        })
        .or(owned);

    let workspaces: Vec<Entity<Workspace>> = multi_workspace.workspaces().cloned().collect();
    for workspace in workspaces {
        let is_active = workspace == active;
        let column = is_active.then(|| shown.clone()).flatten();
        workspace.update(cx, |workspace, cx| {
            workspace.set_leading_column(column.map(Into::into), cx);
            workspace.set_shown_in_window(is_active, window, cx);
        });
    }
    for column in TerminalColumns::all(cx) {
        let is_shown = shown.as_ref() == Some(&column);
        column.update(cx, |column, cx| {
            column.set_workspace_active(is_shown, cx);
            if is_shown {
                column.set_displayed_in(active.downgrade());
            }
        });
    }
    if let Some(shown) = shown {
        let layout = shown.read(cx).layout();
        active.update(cx, |workspace, cx| {
            workspace.set_leading_column_layout(layout, cx)
        });
    }
}
