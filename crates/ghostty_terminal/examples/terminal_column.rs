//! Shows a terminal column (tab bar, terminal, bottom strip) in a floating
//! window and saves a screenshot, for comparing against the Ghostty fork.
//!
//! Usage: cargo run -p ghostty_terminal --example terminal_column -- <out.png> [flat]

#![allow(clippy::disallowed_methods, reason = "examples block on screencapture")]

#[cfg(target_os = "macos")]
fn main() {
    use std::{path::PathBuf, time::Duration};

    use ghostty_terminal::{ClaudeState, TerminalColumn};
    use gpui::{
        App, AppContext as _, Bounds, Focusable as _, WindowBounds, WindowOptions, point, px, size,
    };
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    let out = PathBuf::from(
        std::env::args()
            .nth(1)
            .unwrap_or_else(|| "column.png".into()),
    );
    let flat = std::env::args().any(|argument| argument == "flat");
    let path = std::env::current_dir().expect("cwd");
    // Keep tab sessions out of the real `~/.config/ghostty/tab-sessions`.
    let data_dir = std::env::temp_dir().join("ghostty-terminal-column-example");
    paths::set_custom_data_dir(&data_dir.to_string_lossy());
    let restore = std::env::args().any(|argument| argument == "restore");
    if restore {
        let sessions = data_dir.join("ghostty-tab-sessions");
        std::fs::create_dir_all(&sessions).expect("sessions dir");
        let slug = path.to_string_lossy().replace('/', "-");
        let snapshot = serde_json::json!({
            "workspace": path.to_string_lossy(), "updated": 0, "selected": 1,
            "tabs": [
                {"cwd": path.to_string_lossy()},
                {"cwd": "/tmp", "blocked": true, "blockedNote": "Väntar på review"},
            ],
        });
        std::fs::write(sessions.join(format!("{slug}.json")), snapshot.to_string())
            .expect("write snapshot");
    }

    gpui_platform::application().run(move |cx: &mut App| {
        ui::set_winman_amiga(!flat, cx);
        let bounds = Bounds::new(point(px(0.), px(200.)), size(px(800.), px(500.)));
        let mut window_number = 0i64;
        let window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    kind: gpui::WindowKind::PopUp,
                    focus: false,
                    ..Default::default()
                },
                |window, cx| {
                    if let Ok(RawWindowHandle::AppKit(handle)) =
                        HasWindowHandle::window_handle(window).map(|handle| handle.as_raw())
                    {
                        unsafe {
                            use objc::{msg_send, sel, sel_impl};
                            let view = handle.ns_view.as_ptr() as cocoa::base::id;
                            let ns_window: cocoa::base::id = msg_send![view, window];
                            window_number = msg_send![ns_window, windowNumber];
                        }
                    }
                    let column = cx.new(|cx| TerminalColumn::for_path(path.clone(), window, cx));
                    window.focus(&column.focus_handle(cx), cx);
                    column
                },
            )
            .expect("open window");

        cx.spawn(async move |cx| {
            cx.background_executor().timer(Duration::from_secs(2)).await;
            window
                .update(cx, |column, window, cx| {
                    if restore {
                        let tab = column.tabs()[0].id();
                        column.select_tab(0, window, cx);
                        column.pick_worktree(tab, false, window, cx);
                        return;
                    }
                    column.new_tab_following_worktree(window, cx);
                    column.select_tab(0, window, cx);
                    let tabs = column.tabs_mut();
                    tabs[0].claude_present = true;
                    tabs[0].claude_state = ClaudeState::Working;
                    tabs[0].claude_title = Some("Zed och Ghostty samma process".into());
                    tabs[0].worktree = Some("unified-terminal".into());
                    tabs[1].worktree = Some("main".into());
                    cx.notify();
                })
                .ok();
            cx.background_executor().timer(Duration::from_secs(2)).await;
            let status = std::process::Command::new("screencapture")
                .args(["-x", "-o", "-l", &window_number.to_string()])
                .arg(&out)
                .status();
            println!("screencapture: {status:?}");
            cx.update(|cx| cx.quit());
        })
        .detach();
    });
}

#[cfg(not(target_os = "macos"))]
fn main() {}
