use gpui::{Pixels, Window, px};

pub const MACOS_SDK_26_OR_LATER: bool = cfg!(macos_sdk_26_or_later);

// Borderless-window patch: the native traffic lights are removed (see the
// gpui/mac square-corners/no-shadow patch), so there's nothing to reserve
// space for. Use a normal left inset instead of the traffic-light gap.
pub const TRAFFIC_LIGHT_PADDING: f32 = 8.;

/// Returns the platform-appropriate title bar height.
///
/// On Windows, this returns a fixed height of 32px.
/// On other platforms, it scales with the window's rem size (1.75x) with a minimum of 34px.
#[cfg(not(target_os = "windows"))]
pub fn platform_title_bar_height(window: &Window) -> Pixels {
    (1.75 * window.rem_size()).max(px(34.))
}

#[cfg(target_os = "windows")]
pub fn platform_title_bar_height(_window: &Window) -> Pixels {
    // todo(windows) instead of hard coded size report the actual size to the Windows platform API
    px(32.)
}
