//! The worktree picker under a shell tab, the fork's `WorktreePicker.swift`:
//! lists `<project>/worktrees/*` and `cd`s the tab into the one chosen.

use std::path::{Path, PathBuf};

use gpui::{
    AnyElement, Context, FocusHandle, InteractiveElement, IntoElement, KeyDownEvent, MouseButton,
    MouseDownEvent, ParentElement, Rgba, SharedString, Styled, div, prelude::FluentBuilder as _,
    px, rgb,
};

use crate::TerminalColumn;

const ROW_HEIGHT: f32 = 24.;
const HEADER_HEIGHT: f32 = 28.;
const FOOTER_HEIGHT: f32 = 22.;
pub const WIDTH: f32 = 380.;
const PAD: f32 = 12.;
const MAX_VISIBLE_ROWS: usize = 16;

const BACKGROUND: u32 = 0x1e1e1e;
const SELECTED_BACKGROUND: u32 = 0x504945;
const TEXT: u32 = 0xbdae93;
const SECONDARY: u32 = 0x928374;
const ACCENT: u32 = 0xfe8019;
const RED: u32 = 0xfb4934;
const FONT: &str = "Consolas Nerd Font";

#[derive(Clone, Debug)]
pub struct WorktreeEntry {
    pub name: String,
    pub path: PathBuf,
    pub branch: Option<String>,
}

/// The worktrees in `dir`, sorted, each with the branch its HEAD is on. Read
/// straight from the `.git` files rather than by running git.
pub fn list_worktrees(dir: &Path) -> Vec<WorktreeEntry> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| !name.starts_with('.'))
        .collect();
    names.sort();
    names
        .into_iter()
        .map(|name| {
            let path = dir.join(&name);
            WorktreeEntry {
                branch: branch_of(&path),
                name,
                path,
            }
        })
        .collect()
}

fn branch_of(path: &Path) -> Option<String> {
    let dot_git = path.join(".git");
    let git_dir = match std::fs::read_to_string(&dot_git) {
        Ok(pointer) => {
            let line = pointer.lines().next()?;
            let git_dir = line.strip_prefix("gitdir:")?.trim();
            PathBuf::from(git_dir)
        }
        Err(_) => dot_git,
    };
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let reference = head.trim().strip_prefix("ref:")?.trim();
    Some(
        reference
            .strip_prefix("refs/heads/")
            .unwrap_or(reference)
            .to_string(),
    )
}

pub struct WorktreePicker {
    pub tab_id: u64,
    pub entries: Vec<WorktreeEntry>,
    pub selected: usize,
    /// Opened by winman's p+3 stepper: every step applies right away and the
    /// close only hands the keyboard back.
    pub stepping: bool,
    pub focus_handle: FocusHandle,
    /// Where the panel hangs, relative to the column's top-left.
    pub x: f32,
    pub y: f32,
}

impl WorktreePicker {
    pub fn visible_rows(&self) -> usize {
        self.entries.len().min(MAX_VISIBLE_ROWS)
    }

    pub fn height(&self) -> f32 {
        HEADER_HEIGHT + self.visible_rows() as f32 * ROW_HEIGHT + FOOTER_HEIGHT
    }

    fn scroll_offset(&self) -> usize {
        if self.entries.len() <= MAX_VISIBLE_ROWS {
            return 0;
        }
        let half = MAX_VISIBLE_ROWS / 2;
        self.selected
            .saturating_sub(half)
            .min(self.entries.len() - MAX_VISIBLE_ROWS)
    }

    pub fn move_selection(&mut self, delta: isize) {
        let count = self.entries.len() as isize;
        if count == 0 {
            return;
        }
        self.selected = ((self.selected as isize + delta).rem_euclid(count)) as usize;
    }
}

fn color(value: u32) -> Rgba {
    rgb(value)
}

impl TerminalColumn {
    pub(crate) fn render_worktree_picker(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let picker = self.worktree_picker()?;
        let offset = picker.scroll_offset();
        let rows: Vec<AnyElement> = (0..picker.visible_rows())
            .map(|row| {
                let index = offset + row;
                let entry = &picker.entries[index];
                let selected = index == picker.selected;
                let mismatch = entry
                    .branch
                    .as_ref()
                    .is_some_and(|branch| *branch != entry.name);
                let marker = if selected { "\u{276F} " } else { "  " };
                let label_color = if mismatch {
                    RED
                } else if selected {
                    ACCENT
                } else {
                    TEXT
                };
                div()
                    .id(("ghostty-worktree-row", index))
                    .relative()
                    .h(px(ROW_HEIGHT))
                    .w_full()
                    .flex()
                    .items_center()
                    .when(selected, |this| {
                        this.child(
                            div()
                                .absolute()
                                .top(px(1.))
                                .bottom(px(1.))
                                .left(px(3.))
                                .right(px(3.))
                                .bg(color(SELECTED_BACKGROUND)),
                        )
                    })
                    .child(
                        div()
                            .relative()
                            .pl(px(PAD))
                            .text_size(px(13.))
                            .text_color(color(label_color))
                            .child(SharedString::from(format!("{marker}{}", entry.name))),
                    )
                    .when_some(entry.branch.clone().filter(|_| mismatch), |this, branch| {
                        this.child(
                            div()
                                .absolute()
                                .right(px(PAD))
                                .text_size(px(11.))
                                .text_color(color(RED))
                                .child(SharedString::from(format!("on {branch}"))),
                        )
                    })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                            cx.stop_propagation();
                            if let Some(picker) = this.worktree_picker_mut() {
                                picker.selected = index;
                            }
                            this.confirm_worktree_picker(window, cx);
                        }),
                    )
                    .into_any_element()
            })
            .collect();

        Some(
            gpui::deferred(
                div()
                    .id("ghostty-worktree-picker")
                    .track_focus(&picker.focus_handle)
                    .absolute()
                    .left(px(picker.x))
                    .top(px(picker.y))
                    .w(px(WIDTH))
                    .h(px(picker.height()))
                    .bg(color(BACKGROUND))
                    .border_1()
                    .border_color(color(SELECTED_BACKGROUND))
                    .shadow_lg()
                    .font_family(FONT)
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .h(px(HEADER_HEIGHT))
                            .flex()
                            .items_center()
                            .pl(px(PAD))
                            .text_size(px(12.))
                            .text_color(color(SECONDARY))
                            .child("worktree"),
                    )
                    .children(rows)
                    .child(
                        div()
                            .h(px(FOOTER_HEIGHT))
                            .flex()
                            .items_center()
                            .pl(px(PAD))
                            .text_size(px(10.))
                            .text_color(color(SECONDARY))
                            .child(
                                "\u{2191}/\u{2193} select \u{00B7} \u{23CE} cd \u{00B7} esc cancel",
                            ),
                    )
                    .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                        cx.stop_propagation();
                        match event.keystroke.key.as_str() {
                            "down" | "j" => this.step_worktree_picker(1, false, cx),
                            "up" | "k" => this.step_worktree_picker(-1, false, cx),
                            "enter" => this.confirm_worktree_picker(window, cx),
                            "escape" => {
                                this.close_worktree_picker(window, cx);
                            }
                            _ => {}
                        }
                    }))
                    .on_mouse_down_out(cx.listener(|this, _: &MouseDownEvent, window, cx| {
                        this.close_worktree_picker(window, cx);
                    })),
            )
            .with_priority(1)
            .into_any_element(),
        )
    }
}
