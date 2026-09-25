//! Ghostty's command palette (`toggle_command_palette`), as a Zed picker: the
//! config's `command-palette-entry` commands, run as binding actions on the
//! terminal that asked, and a "Focus:" entry for every terminal tab.

use std::sync::{Arc, atomic::AtomicBool};

use fuzzy::StringMatchCandidate;
use gpui::{
    App, AppContext as _, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    Task, WeakEntity, Window,
};
use picker::{Picker, PickerDelegate};
use ui::{ListItem, ListItemSpacing, prelude::*};
use util::{ResultExt as _, paths::PathExt as _};

use crate::{GhosttyTerminal, TerminalColumn, TerminalColumns, runtime};

/// Actions the Ghostty app leaves out because they make no sense on macOS.
const UNSUPPORTED_ACTION_KEYS: [&str; 3] = [
    "toggle_tab_overview",
    "toggle_window_decorations",
    "show_gtk_inspector",
];

enum Entry {
    Command {
        title: String,
        description: String,
        action: String,
    },
    Focus {
        title: String,
        subtitle: Option<String>,
        column: WeakEntity<TerminalColumn>,
        tab_id: u64,
    },
}

impl Entry {
    fn title(&self) -> &str {
        match self {
            Entry::Command { title, .. } | Entry::Focus { title, .. } => title,
        }
    }
}

pub struct GhosttyCommandPalette {
    picker: Entity<Picker<GhosttyCommandPaletteDelegate>>,
}

impl GhosttyCommandPalette {
    /// A palette for `terminal`, drawn by its terminal column.
    pub fn new(
        terminal: WeakEntity<GhosttyTerminal>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let entries = entries(cx);
        let palette = cx.entity().downgrade();
        let delegate = GhosttyCommandPaletteDelegate {
            palette,
            terminal,
            entries,
            matches: Vec::new(),
            selected_index: 0,
        };
        let picker = cx.new(|cx| {
            let mut picker = Picker::uniform_list(delegate, window, cx);
            picker.refresh(window, cx);
            picker
        });
        Self { picker }
    }
}

fn entries(cx: &App) -> Vec<Entry> {
    let mut entries: Vec<Entry> = runtime::command_palette_entries()
        .into_iter()
        .filter(|command| !UNSUPPORTED_ACTION_KEYS.contains(&command.action_key.as_str()))
        .map(|command| Entry::Command {
            title: command.title,
            description: command.description,
            action: command.action,
        })
        .collect();
    for column in TerminalColumns::all(cx) {
        let column_ref = column.read(cx);
        let column_path = column_ref
            .workspace_path()
            .map(|path| path.compact().to_string_lossy().into_owned());
        for tab in column_ref.tabs() {
            let title = tab
                .claude_title
                .as_ref()
                .map(|title| title.to_string())
                .or_else(|| column_path.clone())
                .unwrap_or_else(|| "Untitled".into());
            let pwd = tab
                .focused_terminal()
                .and_then(|terminal| terminal.read(cx).reported_directory().cloned())
                .map(|pwd| pwd.compact().to_string_lossy().into_owned());
            entries.push(Entry::Focus {
                subtitle: pwd.filter(|pwd| !title.contains(pwd.as_str())),
                title: format!("Focus: {title}"),
                column: column.downgrade(),
                tab_id: tab.id(),
            });
        }
    }
    entries
}

impl Render for GhosttyCommandPalette {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex().w(rems(34.)).child(self.picker.clone())
    }
}

impl Focusable for GhosttyCommandPalette {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl EventEmitter<DismissEvent> for GhosttyCommandPalette {}

struct GhosttyCommandPaletteDelegate {
    palette: WeakEntity<GhosttyCommandPalette>,
    terminal: WeakEntity<GhosttyTerminal>,
    entries: Vec<Entry>,
    /// Indices into `entries`, best first.
    matches: Vec<usize>,
    selected_index: usize,
}

impl PickerDelegate for GhosttyCommandPaletteDelegate {
    type ListItem = ListItem;

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Execute a command…".into()
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(&mut self, index: usize, _: &mut Window, _: &mut Context<Picker<Self>>) {
        self.selected_index = index;
    }

    fn update_matches(
        &mut self,
        query: String,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        if query.is_empty() {
            self.matches = (0..self.entries.len()).collect();
            self.selected_index = 0;
            return Task::ready(());
        }
        let candidates: Vec<StringMatchCandidate> = self
            .entries
            .iter()
            .enumerate()
            .map(|(index, entry)| StringMatchCandidate::new(index, entry.title()))
            .collect();
        let executor = cx.background_executor().clone();
        cx.spawn(async move |picker, cx| {
            let cancel = AtomicBool::new(false);
            let matches = fuzzy::match_strings(
                &candidates,
                &query,
                false,
                true,
                candidates.len(),
                &cancel,
                executor,
            )
            .await;
            picker
                .update(cx, |picker, _| {
                    let delegate = &mut picker.delegate;
                    delegate.matches = matches
                        .into_iter()
                        .map(|found| found.candidate_id)
                        .collect();
                    delegate.selected_index = 0;
                })
                .log_err();
        })
    }

    fn confirm(&mut self, _: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(entry) = self
            .matches
            .get(self.selected_index)
            .and_then(|index| self.entries.get(*index))
        else {
            return;
        };
        match entry {
            Entry::Command { action, .. } => {
                if let Some(terminal) = self.terminal.upgrade() {
                    terminal.read(cx).binding_action(action);
                }
            }
            Entry::Focus { column, tab_id, .. } => {
                if let Some(column) = column.upgrade() {
                    let tab_id = *tab_id;
                    TerminalColumns::set_current(&column, cx);
                    column.update(cx, |column, cx| {
                        if let Some(index) = column.tabs().iter().position(|tab| tab.id() == tab_id)
                        {
                            column.select_tab(index, window, cx);
                        }
                    });
                }
            }
        }
        self.dismissed(window, cx);
    }

    fn dismissed(&mut self, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.palette
            .update(cx, |_, cx| cx.emit(DismissEvent))
            .log_err();
    }

    fn render_match(
        &self,
        index: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let entry = self.entries.get(*self.matches.get(index)?)?;
        let (title, detail) = match entry {
            Entry::Command {
                title, description, ..
            } => (
                title.clone(),
                (!description.is_empty()).then(|| description.clone()),
            ),
            Entry::Focus {
                title, subtitle, ..
            } => (title.clone(), subtitle.clone()),
        };
        Some(
            ListItem::new(index)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(
                    v_flex()
                        .child(Label::new(title))
                        .children(detail.map(|detail| {
                            Label::new(detail)
                                .size(LabelSize::Small)
                                .color(Color::Muted)
                        })),
                ),
        )
    }
}
