//! Winman page tinting shared by the tab bar and the workspace's bottom strip.
//!
//! The palette and blend math are kept verbatim with the Ghostty fork
//! (`WinmanPageMonitor.accents` and `ThemedTabPalette` in `ZedTabBar.swift`) so
//! the terminal and the editor show the exact same color for a given winman
//! page. Only the active (key) window picks up the page tint; inactive windows
//! stay on the theme's neutral background.

use gpui::{App, Global, Hsla, rgb};

/// The active winman "page" (0-based), pushed from the winman daemon over Zed's
/// CLI datagram socket. `None` = unknown.
#[derive(Default)]
pub struct WinmanPage(Option<usize>);

impl Global for WinmanPage {}

/// Light base the bar tints from when the window is active, before the page
/// accent is blended in (matches `barActiveHex` in the Ghostty fork).
const WINMAN_BAR_ACTIVE: u32 = 0xd5dce1;

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

/// Background for the tab bar / bottom strip given the window's active state.
///
/// Inactive windows stay on `neutral` (the theme's tab-bar background); the
/// active window shows the light base, tinted toward the current page's accent.
pub fn winman_bar_background(window_active: bool, neutral: Hsla, cx: &App) -> Hsla {
    if !window_active {
        return neutral;
    }
    match cx
        .try_global::<WinmanPage>()
        .and_then(|page| page.0)
        .and_then(winman_page_accent)
    {
        Some(accent) => tint(WINMAN_BAR_ACTIVE, accent, WINMAN_PAGE_TINT_AMOUNT),
        None => rgb(WINMAN_BAR_ACTIVE).into(),
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
