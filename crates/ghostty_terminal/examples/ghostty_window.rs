//! Opens a window with an embedded Ghostty terminal, types a command into it
//! and saves both Ghostty's own frame and the composited window as PNGs.
//!
//! Usage: cargo run -p ghostty_terminal --example ghostty_window -- <out-dir>

#[cfg(target_os = "macos")]
fn main() {
    use std::{path::PathBuf, time::Duration};

    use ghostty_terminal::GhosttyTerminal;
    use gpui::{App, Bounds, Focusable as _, WindowBounds, WindowOptions, px, size};
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    let out_dir = PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| ".".into()));

    gpui_platform::application().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(900.), px(560.)), cx);
        let mut window_number = 0i64;
        let window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    // A floating panel stays visible (and so keeps rendering)
                    // without taking focus from whatever the user is doing.
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
                    let terminal = GhosttyTerminal::open(Default::default(), window, cx).expect("open terminal");
                    window.focus(&terminal.focus_handle(cx), cx);
                    terminal
                },
            )
            .expect("open window");
        println!("window number {window_number}");

        cx.spawn(async move |cx| {
            cx.background_executor().timer(Duration::from_secs(3)).await;
            window
                .update(cx, |terminal, _, _| {
                    terminal.input_text("echo \"hello from $TERM in $(tput cols)x$(tput lines)\"; printf '\\e[1;31mred\\e[0m \\e[1;32mgreen\\e[0m åäö →\\n'")
                })
                .ok();
            cx.background_executor().timer(Duration::from_millis(500)).await;
            // Real key events through the window, like AppKit delivers them.
            // (character, US ANSI key code)
            let keys: &[(&str, u16)] = &[
                ("\r", 36),
                ("e", 14), ("c", 8), ("h", 4), ("o", 31), (" ", 49),
                ("t", 17), ("y", 16), ("p", 35), ("e", 14), ("d", 2),
                ("\r", 36),
            ];
            for (characters, key_code) in keys {
                unsafe { send_key(window_number, characters, *key_code) };
                cx.background_executor().timer(Duration::from_millis(30)).await;
            }
            cx.background_executor().timer(Duration::from_secs(2)).await;
            window
                .update(cx, |terminal, _, _| {
                    let (width, height, rgba) = terminal.frame_rgba().expect("no ghostty frame");
                    let image = image::RgbaImage::from_raw(width, height, rgba).expect("image");
                    image.save(out_dir.join("ghostty-frame.png")).expect("save frame");
                    println!("ghostty frame {width}x{height}");
                })
                .ok();
            let status = std::process::Command::new("screencapture")
                .args(["-x", "-o", "-l", &window_number.to_string()])
                .arg(out_dir.join("window.png"))
                .status();
            println!("screencapture: {status:?}");
            cx.update(|cx| cx.quit());
        })
        .detach();
    });
}

#[cfg(target_os = "macos")]
#[allow(
    clippy::disallowed_methods,
    reason = "NSString::alloc is autoreleased right away"
)]
unsafe fn send_key(window_number: i64, characters: &str, key_code: u16) {
    use cocoa::{
        base::{id, nil},
        foundation::{NSAutoreleasePool as _, NSPoint, NSString as _},
    };
    use objc::{class, msg_send, sel, sel_impl};
    unsafe {
        let application: id = msg_send![class!(NSApplication), sharedApplication];
        let window: id = msg_send![application, windowWithWindowNumber: window_number];
        let characters = cocoa::foundation::NSString::alloc(nil)
            .init_str(characters)
            .autorelease();
        for event_type in [10u64, 11u64] {
            let event: id = msg_send![class!(NSEvent),
                keyEventWithType: event_type
                location: NSPoint::new(0., 0.)
                modifierFlags: 0u64
                timestamp: 0f64
                windowNumber: window_number
                context: nil
                characters: characters
                charactersIgnoringModifiers: characters
                isARepeat: false
                keyCode: key_code
            ];
            let _: () = msg_send![window, sendEvent: event];
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {}
