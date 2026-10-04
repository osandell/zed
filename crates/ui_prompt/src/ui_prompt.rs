use gpui::{
    App, Entity, EventEmitter, FocusHandle, Focusable, PromptButton, PromptHandle, PromptLevel,
    PromptResponse, RenderablePromptHandle, SharedString, TextStyleRefinement, WeakEntity, Window,
    div, prelude::*,
};
use std::cell::RefCell;
use markdown::{Markdown, MarkdownElement, MarkdownStyle};
use settings::{Settings, SettingsStore};
use theme_settings::ThemeSettings;
use ui::{FluentBuilder, TintColor, prelude::*};
use workspace::WorkspaceSettings;

pub fn init(cx: &mut App) {
    process_settings(cx);

    cx.observe_global::<SettingsStore>(process_settings)
        .detach();
}

/// Picks the prompt renderer again, for a change `init` could not see coming:
/// the unified window is switched on after this crate is initialized.
pub fn refresh(cx: &mut App) {
    process_settings(cx);
}

fn process_settings(cx: &mut App) {
    let settings = WorkspaceSettings::get_global(cx);
    // With arcoscope, prompts are drawn like its own panels rather than as system
    // alerts, so a confirm looks the same whichever of the two raised it.
    if workspace::unified_window_enabled(cx) {
        cx.set_prompt_builder(zed_prompt_renderer);
    } else if settings.use_system_prompts
        && cfg!(not(any(target_os = "linux", target_os = "freebsd")))
    {
        cx.reset_prompt_builder();
    } else {
        cx.set_prompt_builder(zed_prompt_renderer);
    }
}

/// Use this function in conjunction with [App::set_prompt_builder] to force
/// GPUI to use the internal prompt system.
fn zed_prompt_renderer(
    level: PromptLevel,
    message: &str,
    detail: Option<&str>,
    actions: &[PromptButton],
    handle: PromptHandle,
    window: &mut Window,
    cx: &mut App,
) -> RenderablePromptHandle {
    let renderer = cx.new({
        |cx| ZedPromptRenderer {
            _level: level,
            arcoscope: workspace::unified_window_enabled(cx),
            message_text: SharedString::new(message),
            detail_text: detail
                .filter(|text| !text.is_empty())
                .map(SharedString::new),
            message: cx.new(|cx| Markdown::new(SharedString::new(message), None, None, cx)),
            actions: actions.iter().map(|a| a.label().to_string()).collect(),
            focus: cx.focus_handle(),
            active_action_id: 0,
            answered: false,
            detail: detail
                .filter(|text| !text.is_empty())
                .map(|text| cx.new(|cx| Markdown::new(SharedString::new(text), None, None, cx))),
        }
    });

    OPEN_PROMPTS.with_borrow_mut(|open| {
        open.retain(|prompt| prompt.upgrade().is_some());
        open.push(renderer.downgrade());
    });
    handle.with_view(renderer, window, cx)
}

thread_local! {
    /// Prompts drawn by `zed_prompt_renderer`, newest last, so arcoscope can answer
    /// one by voice. A prompt drops out when its view is released.
    static OPEN_PROMPTS: RefCell<Vec<WeakEntity<ZedPromptRenderer>>> = const { RefCell::new(Vec::new()) };
}

fn newest_prompt(cx: &App) -> Option<Entity<ZedPromptRenderer>> {
    OPEN_PROMPTS.with_borrow_mut(|open| {
        open.retain(|prompt| prompt.upgrade().is_some());
        open.iter().rev().find_map(|prompt| prompt.upgrade())
    })
    .filter(|prompt| !prompt.read(cx).answered)
}

/// The newest open prompt's message and button labels.
pub fn open_prompt(cx: &App) -> Option<(String, Vec<String>)> {
    newest_prompt(cx).map(|prompt| {
        let prompt = prompt.read(cx);
        (prompt.message_text.to_string(), prompt.actions.clone())
    })
}

/// Press the button of the newest open prompt whose label is `label`, ignoring
/// case. Returns the label pressed.
pub fn answer_open_prompt(label: &str, cx: &mut App) -> Option<String> {
    let prompt = newest_prompt(cx)?;
    let wanted = label.trim().to_lowercase();
    let ix = prompt
        .read(cx)
        .actions
        .iter()
        .position(|action| action.to_lowercase() == wanted)?;
    prompt.update(cx, |prompt, cx| {
        prompt.answered = true;
        cx.emit(PromptResponse(ix));
        Some(prompt.actions[ix].clone())
    })
}

pub struct ZedPromptRenderer {
    _level: PromptLevel,
    arcoscope: bool,
    message_text: SharedString,
    detail_text: Option<SharedString>,
    message: Entity<Markdown>,
    actions: Vec<String>,
    focus: FocusHandle,
    active_action_id: usize,
    /// Set once answered by voice, so a second phrase cannot press it again
    /// before the view is released.
    answered: bool,
    detail: Option<Entity<Markdown>>,
}

impl ZedPromptRenderer {
    fn confirm(&mut self, _: &menu::Confirm, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(PromptResponse(self.active_action_id));
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = self
            .actions
            .iter()
            .position(|a| a == "Cancel" || a == "Avbryt")
        {
            cx.emit(PromptResponse(ix));
        }
    }

    fn select_first(
        &mut self,
        _: &menu::SelectFirst,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.active_action_id = self.actions.len().saturating_sub(1);
        cx.notify();
    }

    fn select_last(&mut self, _: &menu::SelectLast, _window: &mut Window, cx: &mut Context<Self>) {
        self.active_action_id = 0;
        cx.notify();
    }

    fn select_next(&mut self, _: &menu::SelectNext, _window: &mut Window, cx: &mut Context<Self>) {
        self.active_action_id = (self.active_action_id + 1) % self.actions.len();
        cx.notify();
    }

    fn select_previous(
        &mut self,
        _: &menu::SelectPrevious,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.active_action_id > 0 {
            self.active_action_id -= 1;
        } else {
            self.active_action_id = self.actions.len().saturating_sub(1);
        }
        cx.notify();
    }
}

impl ZedPromptRenderer {
    /// arcoscope's confirm panels (`RebootPanel`, `FinderDeletePanel`): square
    /// corners, a one-pixel border, flat buttons in a row with the default one
    /// on the right, and the system font.
    fn render_arcoscope(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let grey = |white: f32| gpui::hsla(0., 0., white, 1.);
        let text = grey(0.95);
        let buttons = self
            .actions
            .iter()
            .enumerate()
            .rev()
            .map(|(ix, action)| {
                let active = ix == self.active_action_id;
                div()
                    .id(ix)
                    .tab_index(ix as isize)
                    .h(px(32.))
                    .min_w(px(90.))
                    .px_4()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(grey(0.20))
                    .border_1()
                    .border_color(if active { grey(0.60) } else { grey(0.38) })
                    .hover(|style| style.bg(grey(0.25)))
                    .text_size(px(13.))
                    .text_color(text)
                    .child(action.clone())
                    .on_click(cx.listener(move |_, _, _window, cx| {
                        cx.emit(PromptResponse(ix));
                    }))
            })
            .collect::<Vec<_>>();

        let dialog = v_flex()
            .key_context("Prompt")
            .cursor_default()
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::select_first))
            .on_action(cx.listener(Self::select_last))
            .w(px(460.))
            .px(px(20.))
            .pt(px(14.))
            .pb(px(14.))
            .bg(gpui::hsla(0., 0., 0.11, 0.98))
            .border_1()
            .border_color(grey(0.30))
            .shadow_lg()
            .font_family(".SystemUIFont")
            .child(
                div()
                    .text_size(px(14.))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(text)
                    .child(self.message_text.clone()),
            )
            .children(self.detail_text.clone().map(|detail| {
                div()
                    .mt(px(10.))
                    .text_size(px(12.))
                    .text_color(grey(0.62))
                    .child(detail)
            }))
            .child(
                h_flex()
                    .mt(px(18.))
                    .justify_end()
                    .gap(px(10.))
                    .children(buttons),
            );

        div()
            .size_full()
            .occlude()
            .child(
                v_flex()
                    .size_full()
                    .absolute()
                    .top_0()
                    .left_0()
                    .items_center()
                    .justify_center()
                    .child(dialog),
            )
            .into_any_element()
    }
}

impl Render for ZedPromptRenderer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.arcoscope {
            return self.render_arcoscope(cx);
        }
        let settings = ThemeSettings::get_global(cx);

        let dialog = v_flex()
            .key_context("Prompt")
            .cursor_default()
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::select_first))
            .on_action(cx.listener(Self::select_last))
            .w_80()
            .p_4()
            .gap_4()
            .elevation_3(cx)
            .overflow_hidden()
            .font_family(settings.ui_font.family.clone())
            .child(div().w_full().child(MarkdownElement::new(
                self.message.clone(),
                markdown_style(true, window, cx),
            )))
            .children(self.detail.clone().map(|detail| {
                div().w_full().text_xs().child(MarkdownElement::new(
                    detail,
                    markdown_style(false, window, cx),
                ))
            }))
            .child(
                v_flex()
                    .gap_1()
                    .children(self.actions.iter().enumerate().map(|(ix, action)| {
                        Button::new(ix, action.clone())
                            .full_width()
                            .style(ButtonStyle::Outlined)
                            .when(ix == self.active_action_id, |s| {
                                s.style(ButtonStyle::Tinted(TintColor::Accent))
                            })
                            .tab_index(ix as isize)
                            .on_click(cx.listener(move |_, _, _window, cx| {
                                cx.emit(PromptResponse(ix));
                            }))
                    })),
            );

        div()
            .size_full()
            .occlude()
            .bg(gpui::black().opacity(0.2))
            .child(
                v_flex()
                    .size_full()
                    .absolute()
                    .top_0()
                    .left_0()
                    .items_center()
                    .justify_center()
                    .child(dialog),
            )
            .into_any_element()
    }
}

fn markdown_style(main_message: bool, window: &Window, cx: &App) -> MarkdownStyle {
    let mut base_text_style = window.text_style();
    let settings = ThemeSettings::get_global(cx);
    let font_size = settings.ui_font_size(cx).into();

    let color = if main_message {
        Color::Default.color(cx)
    } else {
        Color::Muted.color(cx)
    };

    base_text_style.refine(&TextStyleRefinement {
        font_family: Some(settings.ui_font.family.clone()),
        font_size: Some(font_size),
        color: Some(color),
        ..Default::default()
    });

    MarkdownStyle {
        base_text_style,
        selection_background_color: cx.theme().colors().element_selection_background,
        ..Default::default()
    }
}

impl EventEmitter<PromptResponse> for ZedPromptRenderer {}

impl Focusable for ZedPromptRenderer {
    fn focus_handle(&self, _: &crate::App) -> FocusHandle {
        self.focus.clone()
    }
}
