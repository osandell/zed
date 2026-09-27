//! Runs a stand-in `claude` (a symlink to `sleep`) in a terminal column, feeds
//! it hook state and a transcript title, and screenshots the lamps.
//!
//! Usage: cargo run -p ghostty_terminal --example claude_status -- <scratch-dir>

#![allow(clippy::disallowed_methods, reason = "examples block on screencapture")]

#[cfg(target_os = "macos")]
fn main() {
    use std::{path::PathBuf, time::Duration};

    use ghostty_terminal::TerminalColumn;
    use gpui::{App, AppContext as _, Bounds, WindowBounds, WindowOptions, point, px, size};
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    let scratch = PathBuf::from(std::env::args().nth(1).expect("scratch dir"));
    paths::set_custom_data_dir(&scratch.join("zed-data").to_string_lossy());
    let fake_claude = scratch.join("claude");
    std::fs::remove_file(&fake_claude).ok();
    std::os::unix::fs::symlink("/bin/sleep", &fake_claude).expect("symlink");
    let transcript = scratch.join("transcript.jsonl");
    std::fs::write(
        &transcript,
        "{\"type\":\"user\"}\n{\"type\":\"ai-title\",\"aiTitle\":\"Testtitel från transcript\"}\n",
    )
    .expect("transcript");
    let path = std::env::current_dir().expect("cwd");

    gpui_platform::application().run(move |cx: &mut App| {
        ui::set_winman_amiga(true, cx);
        let bounds = Bounds::new(point(px(0.), px(200.)), size(px(800.), px(300.)));
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
                    cx.new(|cx| TerminalColumn::for_path(path.clone(), window, cx))
                },
            )
            .expect("open window");

        let screenshot = move |name: &str| {
            std::process::Command::new("screencapture")
                .args(["-x", "-o", "-l", &window_number.to_string()])
                .arg(scratch.join(name))
                .status()
                .ok();
        };
        let fake_claude = fake_claude.clone();
        cx.spawn(async move |cx| {
            let executor = cx.background_executor().clone();
            let timer = |ms| executor.timer(Duration::from_millis(ms));
            timer(2000).await;
            window
                .update(cx, |column, _, cx| {
                    let terminal = column.tabs()[0].terminals()[0].clone();
                    terminal
                        .read(cx)
                        .input_text(&format!("{} 30", fake_claude.display()));
                    terminal.read(cx).press_key(0x24, 0);
                })
                .ok();
            timer(1000).await;
            let pid = window
                .update(cx, |column, _, cx| {
                    column.tabs()[0].terminals()[0].read(cx).foreground_pid()
                })
                .ok()
                .flatten()
                .expect("foreground pid");
            println!("fake claude pid {pid}");
            let state_file = PathBuf::from(std::env::var("HOME").unwrap())
                .join(".claude/tab-state")
                .join(format!("{pid}.json"));
            let write_state = |state: &str| {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs_f64();
                let json = serde_json::json!({
                    "state": state, "ts": now, "session": "test-session",
                    "transcript": transcript.to_string_lossy(),
                    "worktree": "feature__x", "worktreePath": "/tmp/p/worktrees/feature__x",
                });
                std::fs::write(&state_file, json.to_string()).unwrap();
            };
            write_state("working");
            timer(2500).await;
            screenshot("claude-working.png");
            write_state("done");
            timer(2500).await;
            screenshot("claude-done.png");
            std::fs::remove_file(&state_file).ok();
            timer(2000).await;
            screenshot("claude-idle.png");
            cx.update(|cx| cx.quit());
        })
        .detach();
    });
}

#[cfg(not(target_os = "macos"))]
fn main() {}
