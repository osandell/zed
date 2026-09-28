//! Winman page tinting shared by the tab bar and the workspace's bottom strip.
//!
//! The palette and blend math are kept verbatim with the Ghostty fork
//! (`WinmanPageMonitor.accents` and `ThemedTabPalette` in `ZedTabBar.swift`) so
//! the terminal and the editor show the exact same color for a given winman
//! page. Only the active (key) window picks up the page tint; inactive windows
//! stay on the theme's neutral background.

use gpui::{App, FocusHandle, Global, Hsla, Rgba, SharedString, WeakFocusHandle, Window, rgb};

/// The active winman "page" (0-based), pushed from the winman daemon over Zed's
/// CLI datagram socket. `None` = unknown.
#[derive(Default)]
pub struct WinmanPage(Option<usize>);

impl Global for WinmanPage {}

/// Whether this app is the frontmost one, set from NSWorkspace activations.
/// `window.is_window_active()` alone is not enough: a Zed window can stay key
/// while another app (the Ghostty fork) holds the front, and then both apps
/// painted the page tint at once. `None` = not heard yet, treated as frontmost.
#[derive(Default)]
struct WinmanAppFront(Option<bool>);

impl Global for WinmanAppFront {}

/// Record whether Zed is the frontmost app and redraw on a change, so only the
/// app the user is actually in shows the page color.
pub fn set_winman_app_front(front: bool, cx: &mut App) {
    if cx.try_global::<WinmanAppFront>().and_then(|f| f.0) == Some(front) {
        return;
    }
    cx.set_global(WinmanAppFront(Some(front)));
    cx.refresh_windows();
}

/// Focus handles of the terminal columns sharing the window with the editor.
/// The window tints only the half holding the keyboard, so a glance at the
/// bars tells whether keys go to the terminal or the editor.
#[derive(Default)]
struct WinmanTerminalFocus(Vec<WeakFocusHandle>);

impl Global for WinmanTerminalFocus {}

/// Register a terminal column's focus handle; while focus is inside it the
/// editor's bars stay neutral and the terminal's take the page tint.
pub fn register_winman_terminal_focus(handle: &FocusHandle, cx: &mut App) {
    let handles = &mut cx.default_global::<WinmanTerminalFocus>().0;
    handles.retain(|handle| handle.upgrade().is_some());
    handles.push(handle.downgrade());
}

/// Whether the keyboard is in a terminal column of `window`.
pub fn winman_terminal_focused(window: &Window, cx: &App) -> bool {
    cx.try_global::<WinmanTerminalFocus>().is_some_and(|focus| {
        focus
            .0
            .iter()
            .filter_map(WeakFocusHandle::upgrade)
            .any(|handle| handle.contains_focused(window, cx))
    })
}

/// Base the bar tints from when the window is active, before the page accent is
/// blended in. One per appearance, matching `lightBars.barActive` and
/// `darkBars.barActive` in the Ghostty fork — a single light base left the bars
/// glowing pale against a dark editor while the terminal went dark blue/green.
const WINMAN_BAR_ACTIVE_LIGHT: u32 = 0xd5dce1;
const WINMAN_BAR_ACTIVE_DARK: u32 = 0x3c3836;

/// Relative luminance of `color`, 0 (black) to 1 (white).
///
/// The appearance is read off the neutral background we were handed rather than
/// from the theme, which is what the Ghostty fork does too (`bars(for:)` switches
/// on the terminal background's luminance). It also means a theme that is dark
/// without saying so still gets the dark base.
fn luminance(color: Hsla) -> f32 {
    let rgba: Rgba = color.into();
    0.2126 * rgba.r + 0.7152 * rgba.g + 0.0722 * rgba.b
}

/// The active-window base for the appearance implied by `neutral`.
fn bar_active_base(neutral: Hsla) -> u32 {
    if luminance(neutral) < 0.5 {
        WINMAN_BAR_ACTIVE_DARK
    } else {
        WINMAN_BAR_ACTIVE_LIGHT
    }
}

/// Fraction of the (dark) page accent blended into the light base. Kept
/// moderate so tab-label text stays readable (matches `pageTintAmount`).
const WINMAN_PAGE_TINT_AMOUNT: f32 = 0.30;

/// Dark accent color for a winman page index, or `None` if out of range.
/// Mirrors `pageAccents` in winman's `BarView.swift` and
/// `WinmanPageMonitor.accents` in the Ghostty fork.
fn winman_page_accent(page: usize) -> Option<u32> {
    Some(match page {
        0 => 0xb55512, // ö  dark orange
        1 => 0x3a5f2a, // p  green
        2 => 0xa84a78, // b  pink
        3 => 0x2e4f6b, // t  blue
        4 => 0x8a7a1e, // g  yellow
        _ => return None,
    })
}

/// Channel-wise lerp of two packed `0xRRGGBB` colors: `amount` of `accent`
/// blended into `base` (matches `ThemedTabPalette.tint`).
fn tint(base: u32, accent: u32, amount: f32) -> Hsla {
    let channel = |shift: u32| {
        let b = ((base >> shift) & 0xff) as f32;
        let a = ((accent >> shift) & 0xff) as f32;
        ((b + (a - b) * amount).round() as u32) & 0xff
    };
    rgb((channel(16) << 16) | (channel(8) << 8) | channel(0)).into()
}

/// Background for the editor's tab bar / bottom strip.
///
/// Bars stay neutral when another app or a terminal column holds focus.
pub fn winman_bar_background(window: &Window, neutral: Hsla, cx: &App) -> Hsla {
    if !editor_holds_winman_focus(window, cx) {
        return neutral;
    }
    winman_page_tint(neutral, cx)
}

/// Whether the editor side of this window is where the user is: the key window
/// of the frontmost app, with no terminal column holding focus. The editor's
/// bars carry the page colour only then; the terminal colours its own.
fn editor_holds_winman_focus(window: &Window, cx: &App) -> bool {
    let app_front = cx
        .try_global::<WinmanAppFront>()
        .and_then(|f| f.0)
        .unwrap_or(true);
    window.is_window_active() && app_front && !winman_terminal_focused(window, cx)
}

/// The active base for the appearance implied by `neutral`, tinted toward the
/// current page's accent.
pub fn winman_page_tint(neutral: Hsla, cx: &App) -> Hsla {
    let base = bar_active_base(neutral);
    match cx
        .try_global::<WinmanPage>()
        .and_then(|page| page.0)
        .and_then(winman_page_accent)
    {
        Some(accent) => tint(base, accent, WINMAN_PAGE_TINT_AMOUNT),
        None => rgb(base).into(),
    }
}

/// Update the active winman page and redraw every window so the tint follows
/// it. No-op when the page is unchanged.
pub fn set_winman_page(page: usize, cx: &mut App) {
    if cx.try_global::<WinmanPage>().map(|page| page.0) == Some(Some(page)) {
        return;
    }
    cx.set_global(WinmanPage(Some(page)));
    cx.refresh_windows();
}

#[derive(Default)]
pub struct WinmanTheme {
    snapshot: Option<ThemeSnapshot>,
    missing_theme: Option<SharedString>,
}

impl Global for WinmanTheme {}

#[derive(Clone, Debug, PartialEq)]
struct ThemeSnapshot {
    name: String,
    binding: Option<crate::winman_skin::ThemeBinding>,
}

#[derive(serde::Deserialize)]
struct ThemeBindings {
    #[serde(default = "follow_themes_by_default")]
    enabled: bool,
    #[serde(default)]
    themes: std::collections::BTreeMap<String, crate::winman_skin::ThemeBinding>,
}

fn follow_themes_by_default() -> bool {
    true
}

pub fn winman_amiga(cx: &App) -> bool {
    cx.try_global::<WinmanTheme>()
        .and_then(|state| state.snapshot.as_ref())
        .and_then(|snapshot| snapshot.binding.as_ref())
        .is_some_and(|binding| binding.chrome.as_deref() == Some("amiga") && binding.skin.is_none())
}

/// Keep terminal status icons in step with WinMan's non-flat themes.
pub fn winman_pixel_art(cx: &App) -> bool {
    cx.try_global::<WinmanTheme>()
        .and_then(|state| state.snapshot.as_ref())
        .is_some_and(|snapshot| snapshot.name != "flat" && snapshot.binding.is_some())
}

/// Explicit theme selection for standalone terminal previews.
pub fn set_winman_amiga(amiga: bool, cx: &mut App) {
    let settings = if amiga {
        r#"{"barTheme":"amiga"}"#
    } else {
        r#"{"barTheme":"flat"}"#
    };
    match theme_snapshot(Some(settings), None) {
        Ok(snapshot) => apply_winman_theme(snapshot, cx),
        Err(error) => log::error!("WinMan theme configuration: {error}"),
    }
}

fn read_optional_file(path: &std::path::Path) -> Result<Option<String>, String> {
    match std::fs::read_to_string(path) {
        Ok(content) => Ok(Some(content)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("{}: {error}", path.display())),
    }
}

fn theme_snapshot(
    settings: Option<&str>,
    overrides: Option<&str>,
) -> Result<ThemeSnapshot, String> {
    let mut bindings: ThemeBindings = serde_json::from_str(include_str!(
        "../../../assets/images/window-skins/winman.json"
    ))
    .map_err(|error| error.to_string())?;
    if let Some(overrides) = overrides {
        let overrides: ThemeBindings =
            serde_json::from_str(overrides).map_err(|error| error.to_string())?;
        bindings.enabled = overrides.enabled;
        bindings.themes.extend(overrides.themes);
    }
    let settings: serde_json::Value = settings
        .map(serde_json::from_str)
        .transpose()
        .map_err(|error| error.to_string())?
        .unwrap_or_default();
    let name = settings
        .get("barTheme")
        .and_then(|value| value.as_str())
        .unwrap_or("flat")
        .to_owned();
    let binding = if bindings.enabled {
        bindings.themes.remove(&name)
    } else {
        None
    };
    Ok(ThemeSnapshot { name, binding })
}

fn read_winman_theme() -> Result<ThemeSnapshot, String> {
    let home = std::env::var_os("HOME").ok_or("HOME is not set")?;
    let root = std::path::Path::new(&home).join(".config");
    let settings = read_optional_file(&root.join("winman/gui-settings.json"))?;
    let overrides = read_optional_file(&root.join("zed/winman-themes.json"))?;
    theme_snapshot(settings.as_deref(), overrides.as_deref())
}

fn apply_winman_theme(snapshot: ThemeSnapshot, cx: &mut App) {
    if cx
        .try_global::<WinmanTheme>()
        .and_then(|state| state.snapshot.as_ref())
        == Some(&snapshot)
    {
        return;
    }
    let name = snapshot
        .binding
        .as_ref()
        .and_then(|binding| binding.theme.as_ref())
        .map(|name| SharedString::from(name.clone()));
    if theme_settings::set_external_theme(name.clone(), cx) {
        crate::winman_skin::set_bitmap_skin(snapshot.binding.as_ref(), cx);
    } else {
        // Keep the user's ordinary theme when a mapping names an uninstalled one.
        // Do not record this snapshot: a later registry load should retry it.
        if cx
            .try_global::<WinmanTheme>()
            .and_then(|state| state.missing_theme.as_ref())
            != name.as_ref()
        {
            log::warn!(
                "WinMan theme {:?} is not installed; using the configured Zed theme",
                name
            );
        }
        theme_settings::set_external_theme(None, cx);
        crate::winman_skin::set_bitmap_skin(None, cx);
        cx.set_global(WinmanTheme {
            snapshot: None,
            missing_theme: name,
        });
        return;
    }
    cx.set_global(WinmanTheme {
        snapshot: Some(snapshot),
        missing_theme: None,
    });
    cx.refresh_windows();
}

pub fn start_winman_theme_watch(cx: &mut App) {
    cx.spawn(async move |cx| {
        let mut previous_error = None;
        loop {
            match cx
                .background_executor()
                .spawn(async { read_winman_theme() })
                .await
            {
                Ok(snapshot) => {
                    previous_error = None;
                    cx.update(|cx| apply_winman_theme(snapshot, cx));
                }
                Err(error) => {
                    if previous_error.as_ref() != Some(&error) {
                        log::error!("WinMan theme configuration: {error}");
                        previous_error = Some(error);
                    }
                }
            }
            cx.background_executor()
                .timer(std::time::Duration::from_millis(1500))
                .await;
        }
    })
    .detach();
}

#[cfg(test)]
mod theme_binding_tests {
    use super::*;

    #[test]
    fn maps_arbitrary_future_names_without_rust_changes() {
        let snapshot = theme_snapshot(
            Some(r#"{"barTheme":"future-theme"}"#),
            Some(r#"{"themes":{"future-theme":{"theme":"Future Zed"}}}"#),
        )
        .expect("valid mapping");
        assert_eq!(
            snapshot
                .binding
                .and_then(|binding| binding.theme)
                .as_deref(),
            Some("Future Zed")
        );
    }

    #[test]
    fn disabled_or_unmapped_settings_restore_the_configured_theme() {
        assert!(
            theme_snapshot(
                Some(r#"{"barTheme":"dreamweb"}"#),
                Some(r#"{"enabled":false}"#)
            )
            .expect("valid config")
            .binding
            .is_none()
        );
        assert!(
            theme_snapshot(Some(r#"{"barTheme":"unmapped"}"#), None)
                .expect("valid config")
                .binding
                .is_none()
        );
    }

    #[test]
    fn invalid_configuration_does_not_silently_switch_theme() {
        assert!(theme_snapshot(Some("{"), None).is_err());
        assert!(theme_snapshot(None, Some("{")).is_err());
    }
}

fn mix(a: Hsla, b: Hsla, t: f32) -> Hsla {
    let a: Rgba = a.into();
    let b: Rgba = b.into();
    Rgba {
        r: a.r + (b.r - a.r) * t,
        g: a.g + (b.g - a.g) * t,
        b: a.b + (b.b - a.b) * t,
        a: 1.0,
    }
    .into()
}

/// `color` blended `amount` toward white.
pub fn winman_lighten(color: Hsla, amount: f32) -> Hsla {
    mix(color, gpui::white(), amount)
}

/// `color` blended `amount` toward black.
pub fn winman_darken(color: Hsla, amount: f32) -> Hsla {
    mix(color, gpui::black(), amount)
}

/// The current winman page, or `None` when winman has not said. Skinned
/// chrome picks its per-collection bitmap with it (`winman_skin_surface_variant`).
pub fn winman_page(cx: &App) -> Option<usize> {
    cx.try_global::<WinmanPage>().and_then(|page| page.0)
}

/// `winman_page` for the editor's skinned chrome (its selected tab and the
/// workspace's bottom strip), on the same terms as `winman_bar_background`:
/// `None` while a terminal column or another app holds focus, so the editor
/// then draws its neutral bitmaps.
pub fn winman_bar_page(window: &Window, cx: &App) -> Option<usize> {
    editor_holds_winman_focus(window, cx)
        .then(|| winman_page(cx))
        .flatten()
}

/// The accent line along the top of the active Amiga tab: the current winman
/// page's colour, brightened, so the tab you are in carries the colour of the
/// bar's current cell. Blue when winman has not said.
pub fn winman_amiga_accent(cx: &App) -> Hsla {
    match cx
        .try_global::<WinmanPage>()
        .and_then(|page| page.0)
        .and_then(winman_page_accent)
    {
        Some(accent) => winman_lighten(rgb(accent).into(), 0.25),
        None => rgb(0x5aa0e6).into(),
    }
}

/// Amiga tab title colours, the Ghostty fork's: a light beige on the active
/// tab, a readable muted beige on the rest.
pub fn winman_amiga_text(selected: bool) -> Hsla {
    rgb(if selected { 0xe0d0ae } else { 0xbdae93 }).into()
}

/// The second line of an Amiga tab (the Ghostty fork's worktree line): a dimmer
/// beige under the title.
pub fn winman_amiga_subtext(selected: bool) -> Hsla {
    rgb(if selected { 0xa89984 } else { 0x7c6f64 }).into()
}
