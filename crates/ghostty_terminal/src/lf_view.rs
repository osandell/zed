//! winman's file manager (p+2): lf in a Ghostty terminal over the whole
//! window, in place of the kitty window it used to run in. winman toggles it
//! with `zed://winman/lf` and closes it with `zed://winman/lf-close` before
//! it shows a workspace or moves the keyboard, e.g. after lf's `c` opened one.
//!
//! lf keeps running while the view is hidden, like the kitty window did, so it
//! comes back where it was. Quitting lf closes the view; the next toggle
//! starts a new one.

use gpui::{
    App, Context, Entity, FocusHandle, Focusable, Global, Subscription, WeakEntity, Window, div,
    prelude::*,
};
use workspace::{MultiWorkspace, Workspace};

use crate::{GhosttyTerminal, GhosttyTerminalEvent, TerminalOptions};

const LF: &str = "/opt/homebrew/bin/lf";

pub struct LfView {
    terminal: Entity<GhosttyTerminal>,
    multi_workspace: WeakEntity<MultiWorkspace>,
    /// Where the keyboard was when the view opened, and in which workspace.
    previous_focus: Option<(WeakEntity<Workspace>, FocusHandle)>,
    _subscription: Subscription,
}

/// The one lf that runs, kept while its view is hidden.
#[derive(Default)]
struct RunningLf(Option<Entity<LfView>>);

impl Global for RunningLf {}

/// Starts lf through a login shell, so lf and the commands in lfrc get the
/// same environment as in a terminal tab.
fn start_lf(window: &mut Window, cx: &mut App) -> anyhow::Result<Entity<GhosttyTerminal>> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());
    let options = TerminalOptions {
        working_directory: Some(paths::home_dir().clone()),
        command: Some(format!("{shell} -l -c 'exec {LF}'")),
        ..Default::default()
    };
    GhosttyTerminal::open(options, window, cx)
}

impl LfView {
    fn new(
        terminal: Entity<GhosttyTerminal>,
        multi_workspace: WeakEntity<MultiWorkspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscription = cx.subscribe_in(&terminal, window, |this, _, event, window, cx| {
            if let GhosttyTerminalEvent::CloseRequested { .. } = event {
                this.quit(window, cx);
            }
        });
        Self {
            terminal,
            multi_workspace,
            previous_focus: None,
            _subscription: subscription,
        }
    }

    fn quit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let this = cx.entity();
        let running = cx.default_global::<RunningLf>();
        if running.0.as_ref() == Some(&this) {
            running.0 = None;
        }
        // Deferred: the multi-workspace may be the one being updated when the
        // terminal reports its close, e.g. while it renders the view.
        let multi_workspace = self.multi_workspace.clone();
        window.defer(cx, move |window, cx| {
            multi_workspace
                .update(cx, |multi_workspace, cx| {
                    if shows(multi_workspace, &this) {
                        close(multi_workspace, window, cx);
                    }
                })
                .ok();
        });
    }
}

impl Focusable for LfView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.terminal.focus_handle(cx)
    }
}

impl Render for LfView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("winman-lf-view")
            .size_full()
            .child(self.terminal.clone())
    }
}

fn shows(multi_workspace: &MultiWorkspace, view: &Entity<LfView>) -> bool {
    multi_workspace
        .full_overlay()
        .and_then(|overlay| overlay.clone().downcast::<LfView>().ok())
        .is_some_and(|shown| &shown == view)
}

/// Whether `multi_workspace` shows the lf view.
pub fn is_open(multi_workspace: &MultiWorkspace) -> bool {
    multi_workspace
        .full_overlay()
        .is_some_and(|overlay| overlay.clone().downcast::<LfView>().is_ok())
}

/// Opens the lf view over the window, or closes it when it is up.
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
    let handle = cx.entity().downgrade();
    let running = cx
        .default_global::<RunningLf>()
        .0
        .clone()
        .filter(|view| view.read(cx).multi_workspace == handle);
    let view = match running {
        Some(view) => view,
        None => {
            let terminal = match start_lf(window, cx) {
                Ok(terminal) => terminal,
                Err(error) => {
                    log::error!("could not start lf: {error:#}");
                    return;
                }
            };
            let view = cx.new(|cx| LfView::new(terminal, handle, window, cx));
            cx.set_global(RunningLf(Some(view.clone())));
            view
        }
    };
    // Another full-window view (the git view) is replaced, and its focus
    // goes with it.
    let previous_focus = multi_workspace
        .full_overlay()
        .is_none()
        .then(|| window.focused(cx))
        .flatten()
        .map(|focus| (multi_workspace.workspace().downgrade(), focus));
    view.update(cx, |view, cx| {
        view.previous_focus = previous_focus;
        view.terminal.read(cx).set_visible(true);
    });
    let focus_handle = view.focus_handle(cx);
    multi_workspace.set_full_overlay(Some(view.into()), window, cx);
    window.focus(&focus_handle, cx);
}

fn close(
    multi_workspace: &mut MultiWorkspace,
    window: &mut Window,
    cx: &mut Context<MultiWorkspace>,
) {
    let previous_focus = hide(multi_workspace, window, cx);
    // The keyboard goes back where it was, unless winman switched workspace
    // meanwhile: that focus belongs to a workspace no longer shown.
    let current = multi_workspace.workspace().clone();
    let focus_handle = previous_focus
        .filter(|(workspace, _)| workspace == &current.downgrade())
        .map(|(_, focus)| focus)
        .unwrap_or_else(|| current.read(cx).active_pane().focus_handle(cx));
    window.focus(&focus_handle, cx);
}

/// Takes the view down and returns where the keyboard was when it opened.
fn hide(
    multi_workspace: &mut MultiWorkspace,
    window: &mut Window,
    cx: &mut Context<MultiWorkspace>,
) -> Option<(WeakEntity<Workspace>, FocusHandle)> {
    let view = multi_workspace
        .full_overlay()
        .and_then(|overlay| overlay.clone().downcast::<LfView>().ok())?;
    multi_workspace.set_full_overlay(None, window, cx);
    view.update(cx, |view, cx| {
        view.terminal.read(cx).set_visible(false);
        view.previous_focus.take()
    })
}

/// Closes the lf view if it is up, leaving the keyboard alone: winman sends
/// this right before it moves the keyboard itself.
pub fn close_if_open(
    multi_workspace: &mut MultiWorkspace,
    window: &mut Window,
    cx: &mut Context<MultiWorkspace>,
) {
    hide(multi_workspace, window, cx);
}

/// Gives the lf view the keyboard if it is up. Returns whether it was.
pub fn focus_if_open(multi_workspace: &MultiWorkspace, window: &mut Window, cx: &mut App) -> bool {
    let Some(view) = multi_workspace
        .full_overlay()
        .and_then(|overlay| overlay.clone().downcast::<LfView>().ok())
    else {
        return false;
    };
    let focus_handle = view.focus_handle(cx);
    window.focus(&focus_handle, cx);
    true
}
