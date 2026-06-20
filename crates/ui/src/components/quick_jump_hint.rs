use gpui::{IntoElement, ParentElement, SharedString, Styled, div, px, rgb};

/// Letters painted on quick-jump hint badges, in the order they're assigned to
/// the visible targets (top to bottom). Front-loaded with home-row keys for
/// fast one-handed jumps while ⌘⇧⌃ is held.
///
/// Deliberately excludes `o` and `y`: those are the only ⌘⇧⌃-letter chords bound
/// in the default macOS keymap (`projects::OpenRemote` / `git::UnstageAll`), and
/// keybindings dispatch before `on_key_down`, so a hint on either would never
/// reach our handler.
pub const QUICK_JUMP_HINT_KEYS: &[&str] = &[
    "n", "e", "i", "m", "l", "u", "k", "h", "w", "f", "p", "s", "a", "b", "c", "d", "g", "j", "q",
    "r", "t", "v", "x", "z",
];

/// The hint letter for the nth target, or `None` once we run out of letters.
pub fn quick_jump_hint_key(index: usize) -> Option<SharedString> {
    QUICK_JUMP_HINT_KEYS
        .get(index)
        .map(|key| SharedString::from(*key))
}

/// The hint index for a pressed key, or `None` if it isn't a hint letter.
pub fn quick_jump_hint_index(key: &str) -> Option<usize> {
    QUICK_JUMP_HINT_KEYS.iter().position(|candidate| *candidate == key)
}

/// A small Gruvbox-red badge with a dark glyph, overlaid at the leading edge of
/// a list row. Matches the ms-calendar quick-jump hints. The parent row must be
/// positioned (`relative`) for the absolute placement to anchor correctly.
pub fn quick_jump_hint_badge(letter: SharedString) -> impl IntoElement {
    div()
        .absolute()
        .left(px(2.))
        .top_0()
        .bottom_0()
        .flex()
        .items_center()
        .child(
            div()
                .px(px(4.))
                .rounded_sm()
                .bg(rgb(0xfb4934))
                .text_color(rgb(0x1d2021))
                .text_size(px(12.))
                .line_height(px(16.))
                .child(letter),
        )
}
