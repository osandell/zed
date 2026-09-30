//! The "close this tab?" question, hung directly under the tab being closed
//! instead of centred in the window, so the pointer does not have to travel.

use gpui::{
    AnyElement, Context, FocusHandle, InteractiveElement, IntoElement, KeyDownEvent,
    MouseDownEvent, ParentElement, Styled, div, px,
};
use ui::{
    ActiveTheme as _, Button, ButtonCommon as _, ButtonStyle, Clickable as _, StyledExt as _,
};

use crate::TerminalColumn;

pub const WIDTH: f32 = 300.;

pub struct CloseConfirm {
    pub tab_id: u64,
    pub focus_handle: FocusHandle,
    /// Where the panel hangs, relative to the column's top-left.
    pub x: f32,
    pub y: f32,
}

impl TerminalColumn {
    pub(crate) fn render_close_confirm(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let confirm = self.close_confirm()?;
        let colors = cx.theme().colors();
        Some(
            gpui::deferred(
                div()
                    .id("ghostty-close-confirm")
                    .track_focus(&confirm.focus_handle)
                    .absolute()
                    .left(px(confirm.x))
                    .top(px(confirm.y))
                    .w(px(WIDTH))
                    .elevation_3(cx)
                    .p(px(12.))
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .child(
                        div()
                            .text_size(px(13.))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(colors.text)
                            .child("Stänga fliken?"),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(colors.text_muted)
                            .child(
                                "Terminalen kör fortfarande en process. Stänger du fliken avslutas den.",
                            ),
                    )
                    .child(
                        div()
                            .pt(px(6.))
                            .flex()
                            .justify_end()
                            .gap(px(6.))
                            .child(Button::new("ghostty-close-confirm-cancel", "Avbryt").on_click(
                                cx.listener(|this, _, window, cx| {
                                    this.answer_close_confirm(false, window, cx)
                                }),
                            ))
                            .child(
                                Button::new("ghostty-close-confirm-close", "Stäng")
                                    .style(ButtonStyle::Filled)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.answer_close_confirm(true, window, cx)
                                    })),
                            ),
                    )
                    // Enter closes, as the default button of the alert it
                    // replaces did; Esc keeps the tab.
                    .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                        cx.stop_propagation();
                        match event.keystroke.key.as_str() {
                            "enter" => this.answer_close_confirm(true, window, cx),
                            "escape" => this.answer_close_confirm(false, window, cx),
                            _ => {}
                        }
                    }))
                    .on_mouse_down_out(cx.listener(|this, _: &MouseDownEvent, window, cx| {
                        this.answer_close_confirm(false, window, cx)
                    })),
            )
            .with_priority(1)
            .into_any_element(),
        )
    }
}
