use gpui::{IntoElement, ParentElement, SharedString, Styled, div, px, rgb};

/// Letters matched on quick-jump hint key presses, top-to-bottom over targets.
/// Kept identical to ms-mail / ms-calendar (`AppState.hintLetters`) so the
/// physical-key order is the same across all three apps.
///
/// NOTE: `o` and `y` are ⌘⇧⌃-letter chords bound in the default macOS keymap
/// (`projects::OpenRemote` / `git::UnstageAll`); keybindings dispatch before
/// `on_key_down`, so hints at those two positions won't fire unless those
/// bindings are removed from the keymap.
pub const QUICK_JUMP_HINT_KEYS: &[&str] = &[
    "n", "e", "i", "o", "m", "'", "l", "u", "y", "-", "j", "7", "z", "ä", "x", "k", "h", ",", ".",
    "å", "ö", "w", "f", "p", "s",
];

/// Badge glyph for keys whose physical keycap differs from the matched char, so
/// the badge shows what's printed on the key (matches ms-mail's
/// `hintLabelOverrides`): you press the key that emits the char, the badge shows
/// the keycap. The key right of å emits `7` and is painted `/`.
fn quick_jump_hint_label_for(key: &str) -> &str {
    match key {
        "w" => "3",
        "7" => "/",
        "z" => "9",
        "x" => "+",
        other => other,
    }
}

/// The hint letter (matched char) for the nth target, or `None` once we run out.
pub fn quick_jump_hint_key(index: usize) -> Option<SharedString> {
    QUICK_JUMP_HINT_KEYS
        .get(index)
        .map(|key| SharedString::from(*key))
}

/// The glyph to paint on the nth target's badge (the physical keycap), or `None`.
pub fn quick_jump_hint_label(index: usize) -> Option<SharedString> {
    QUICK_JUMP_HINT_KEYS
        .get(index)
        .map(|key| SharedString::from(quick_jump_hint_label_for(key)))
}

/// gpui reports the SHIFTED glyph for these keys (and drops the shift modifier),
/// while the hint table uses the unmodified char (like ms-mail's UCKeyTranslate,
/// which ignores modifiers). Fold gpui's value back to the table's char so the
/// special-character hints match. `/` is the key right of å, which ms-mail keys
/// as `7`.
fn normalize_quick_jump_key(key: &str) -> &str {
    match key {
        "*" => "'",
        "_" => "-",
        "/" => "7",
        "Ä" => "ä",
        "Å" => "å",
        "Ö" => "ö",
        ";" => ",",
        ":" => ".",
        other => other,
    }
}

/// Labels for the editor tab-bar quick-jump badges (tab 0..N, left→right),
/// driven externally by winman over the `zed://winman/...` URL channel rather
/// than the ⌘⇧⌃ modifier. The user presses physical Tab/q/w/e, which emit
/// `q w f p` on their layout (see winman's tmux-layer zed-N mapping); the badge
/// paints those same glyphs.
pub const TAB_HINT_LABELS: &[&str] = &["q", "w", "f", "p"];

/// The badge glyph for the nth editor tab, or `None` past the labelled tabs.
pub fn tab_hint_label(index: usize) -> Option<SharedString> {
    TAB_HINT_LABELS.get(index).map(|key| SharedString::from(*key))
}

/// The hint index for a pressed key, or `None` if it isn't a hint letter.
pub fn quick_jump_hint_index(key: &str) -> Option<usize> {
    let key = normalize_quick_jump_key(key);
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
