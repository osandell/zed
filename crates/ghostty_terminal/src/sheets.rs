//! Native sheets the Ghostty fork shows with `NSAlert`.

use std::cell::RefCell;

use block::ConcreteBlock;
use cocoa::{
    base::id,
    foundation::{NSPoint, NSRect, NSSize},
};
use futures::channel::oneshot;
use objc::{class, msg_send, sel, sel_impl};

use crate::{ns_string, ns_string_to_string};

const NS_ALERT_FIRST_BUTTON_RETURN: i64 = 1000;

/// "What is blocking this tab?" with a text field prefilled with `note`.
/// Sends the trimmed text on OK, `None` on Cancel.
///
/// # Safety
///
/// `window` must be a live `NSWindow`; call on the main thread.
pub unsafe fn ask_blocked_note(window: id, note: &str, reply: oneshot::Sender<Option<String>>) {
    unsafe {
        let alert: id = msg_send![class!(NSAlert), alloc];
        let alert: id = msg_send![alert, init];
        let _: () = msg_send![alert, setMessageText: ns_string("What is blocking this tab?")];
        let _: () = msg_send![alert, setInformativeText:
            ns_string("Shown when you hover the red dot. Leave blank to clear the note.")];

        let field: id = msg_send![class!(NSTextField), alloc];
        let field: id = msg_send![field, initWithFrame:
            NSRect::new(NSPoint::new(0., 0.), NSSize::new(320., 24.))];
        let _: () = msg_send![field, setStringValue: ns_string(note)];
        let _: () = msg_send![field, setPlaceholderString: ns_string("Waiting on ...")];
        let _: () = msg_send![alert, setAccessoryView: field];

        let _: id = msg_send![alert, addButtonWithTitle: ns_string("OK")];
        let _: id = msg_send![alert, addButtonWithTitle: ns_string("Cancel")];
        let alert_window: id = msg_send![alert, window];
        let _: () = msg_send![alert_window, setInitialFirstResponder: field];

        let reply = RefCell::new(Some(reply));
        let handler = ConcreteBlock::new(move |response: i64| {
            let answer = (response == NS_ALERT_FIRST_BUTTON_RETURN).then(|| {
                let value: id = msg_send![field, stringValue];
                ns_string_to_string(value)
                    .unwrap_or_default()
                    .trim()
                    .to_string()
            });
            if let Some(reply) = reply.borrow_mut().take() {
                reply.send(answer).ok();
            }
            let _: () = msg_send![field, release];
            let _: () = msg_send![alert, release];
        });
        let handler = handler.copy();
        let _: () = msg_send![alert, beginSheetModalForWindow: window completionHandler: &*handler];
    }
}
