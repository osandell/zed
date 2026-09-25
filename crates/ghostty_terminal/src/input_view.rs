//! A zero-sized native view that owns keyboard input for one Ghostty surface.
//!
//! GPUI draws the terminal, but Ghostty's key encoding needs the raw `NSEvent`
//! (key codes, side-specific modifiers, `characters(byApplyingModifiers:)`) and
//! its own `NSTextInputClient` for dead keys and IMEs. So while the terminal has
//! GPUI focus this view is made first responder, and it ports the key handling
//! of Ghostty's `SurfaceView_AppKit.swift`. Like in the Ghostty app, the
//! terminal gets every key; Zed's keybindings are not consulted, only
//! Ghostty's own and the menu's.

use std::{
    ffi::{CString, c_void},
    ptr,
    sync::OnceLock,
};

use cocoa::{
    base::{BOOL, NO, YES, id, nil},
    foundation::{NSPoint, NSRect, NSSize, NSUInteger},
};
use ghostty_embed as ffi;
use objc::{
    class,
    declare::ClassDecl,
    msg_send,
    runtime::{Class, Object, Protocol, Sel},
    sel, sel_impl,
};

use crate::{NSRange, ns_string, ns_string_to_string};

const STATE_IVAR: &str = "zedGhosttyInputState";

const NS_EVENT_MODIFIER_FLAG_CAPS_LOCK: u64 = 1 << 16;
const NS_EVENT_MODIFIER_FLAG_SHIFT: u64 = 1 << 17;
const NS_EVENT_MODIFIER_FLAG_CONTROL: u64 = 1 << 18;
const NS_EVENT_MODIFIER_FLAG_OPTION: u64 = 1 << 19;
const NS_EVENT_MODIFIER_FLAG_COMMAND: u64 = 1 << 20;
const NX_DEVICE_RIGHT_SHIFT_KEY_MASK: u64 = 0x0000_0004;
const NX_DEVICE_RIGHT_CONTROL_KEY_MASK: u64 = 0x0000_2000;
const NX_DEVICE_RIGHT_ALT_KEY_MASK: u64 = 0x0000_0040;
const NX_DEVICE_RIGHT_COMMAND_KEY_MASK: u64 = 0x0000_0010;
const NS_EVENT_TYPE_KEY_DOWN: u64 = 10;
const NS_EVENT_TYPE_KEY_UP: u64 = 11;

/// Per-surface keyboard state, owned by the terminal view and referenced by
/// the native view's ivar.
pub(crate) struct InputState {
    pub surface: ffi::ghostty_surface_t,
    pub gpui_view: id,
    /// The terminal's bounds in window coordinates (points, top-left origin),
    /// used to place the IME candidate window.
    pub terminal_origin: (f64, f64),
    pub cell_size: (f64, f64),
    marked_text: String,
    key_text_accumulator: Option<Vec<String>>,
    last_perform_key_timestamp: Option<f64>,
}

impl InputState {
    pub fn new(surface: ffi::ghostty_surface_t, gpui_view: id) -> Self {
        Self {
            surface,
            gpui_view,
            terminal_origin: (0., 0.),
            cell_size: (0., 0.),
            marked_text: String::new(),
            key_text_accumulator: None,
            last_perform_key_timestamp: None,
        }
    }
}

pub(crate) fn ghostty_mods(flags: u64) -> ffi::ghostty_input_mods_e {
    let mut mods = ffi::GHOSTTY_MODS_NONE;
    if flags & NS_EVENT_MODIFIER_FLAG_SHIFT != 0 {
        mods |= ffi::GHOSTTY_MODS_SHIFT;
    }
    if flags & NS_EVENT_MODIFIER_FLAG_CONTROL != 0 {
        mods |= ffi::GHOSTTY_MODS_CTRL;
    }
    if flags & NS_EVENT_MODIFIER_FLAG_OPTION != 0 {
        mods |= ffi::GHOSTTY_MODS_ALT;
    }
    if flags & NS_EVENT_MODIFIER_FLAG_COMMAND != 0 {
        mods |= ffi::GHOSTTY_MODS_SUPER;
    }
    if flags & NS_EVENT_MODIFIER_FLAG_CAPS_LOCK != 0 {
        mods |= ffi::GHOSTTY_MODS_CAPS;
    }
    if flags & NX_DEVICE_RIGHT_SHIFT_KEY_MASK != 0 {
        mods |= ffi::GHOSTTY_MODS_SHIFT_RIGHT;
    }
    if flags & NX_DEVICE_RIGHT_CONTROL_KEY_MASK != 0 {
        mods |= ffi::GHOSTTY_MODS_CTRL_RIGHT;
    }
    if flags & NX_DEVICE_RIGHT_ALT_KEY_MASK != 0 {
        mods |= ffi::GHOSTTY_MODS_ALT_RIGHT;
    }
    if flags & NX_DEVICE_RIGHT_COMMAND_KEY_MASK != 0 {
        mods |= ffi::GHOSTTY_MODS_SUPER_RIGHT;
    }
    mods
}

fn event_modifier_flags(mods: ffi::ghostty_input_mods_e) -> u64 {
    let mut flags = 0;
    if mods & ffi::GHOSTTY_MODS_SHIFT != 0 {
        flags |= NS_EVENT_MODIFIER_FLAG_SHIFT;
    }
    if mods & ffi::GHOSTTY_MODS_CTRL != 0 {
        flags |= NS_EVENT_MODIFIER_FLAG_CONTROL;
    }
    if mods & ffi::GHOSTTY_MODS_ALT != 0 {
        flags |= NS_EVENT_MODIFIER_FLAG_OPTION;
    }
    if mods & ffi::GHOSTTY_MODS_SUPER != 0 {
        flags |= NS_EVENT_MODIFIER_FLAG_COMMAND;
    }
    flags
}

static CLASS: OnceLock<usize> = OnceLock::new();

fn input_view_class() -> *const Class {
    *CLASS.get_or_init(|| unsafe {
        let mut decl = ClassDecl::new("ZedGhosttyInputView", class!(NSView))
            .expect("ZedGhosttyInputView registered twice");
        decl.add_ivar::<*mut c_void>(STATE_IVAR);
        decl.add_protocol(Protocol::get("NSTextInputClient").expect("NSTextInputClient"));

        decl.add_method(
            sel!(acceptsFirstResponder),
            accepts_first_responder as extern "C" fn(&Object, Sel) -> BOOL,
        );
        decl.add_method(
            sel!(hitTest:),
            hit_test as extern "C" fn(&Object, Sel, NSPoint) -> id,
        );
        decl.add_method(sel!(keyDown:), key_down as extern "C" fn(&Object, Sel, id));
        decl.add_method(sel!(keyUp:), key_up as extern "C" fn(&Object, Sel, id));
        decl.add_method(
            sel!(flagsChanged:),
            flags_changed as extern "C" fn(&Object, Sel, id),
        );
        decl.add_method(
            sel!(performKeyEquivalent:),
            perform_key_equivalent as extern "C" fn(&Object, Sel, id) -> BOOL,
        );

        decl.add_method(
            sel!(hasMarkedText),
            has_marked_text as extern "C" fn(&Object, Sel) -> BOOL,
        );
        decl.add_method(
            sel!(markedRange),
            marked_range as extern "C" fn(&Object, Sel) -> NSRange,
        );
        decl.add_method(
            sel!(selectedRange),
            selected_range as extern "C" fn(&Object, Sel) -> NSRange,
        );
        decl.add_method(
            sel!(setMarkedText:selectedRange:replacementRange:),
            set_marked_text as extern "C" fn(&Object, Sel, id, NSRange, NSRange),
        );
        decl.add_method(sel!(unmarkText), unmark_text as extern "C" fn(&Object, Sel));
        decl.add_method(
            sel!(validAttributesForMarkedText),
            valid_attributes_for_marked_text as extern "C" fn(&Object, Sel) -> id,
        );
        decl.add_method(
            sel!(attributedSubstringForProposedRange:actualRange:),
            attributed_substring_for_proposed_range
                as extern "C" fn(&Object, Sel, NSRange, *mut c_void) -> id,
        );
        decl.add_method(
            sel!(insertText:replacementRange:),
            insert_text as extern "C" fn(&Object, Sel, id, NSRange),
        );
        decl.add_method(
            sel!(characterIndexForPoint:),
            character_index_for_point as extern "C" fn(&Object, Sel, NSPoint) -> NSUInteger,
        );
        decl.add_method(
            sel!(firstRectForCharacterRange:actualRange:),
            first_rect_for_character_range
                as extern "C" fn(&Object, Sel, NSRange, *mut c_void) -> NSRect,
        );
        decl.add_method(
            sel!(doCommandBySelector:),
            do_command_by_selector as extern "C" fn(&Object, Sel, Sel),
        );

        decl.register() as *const Class as usize
    }) as *const Class
}

/// The native input view for one terminal, added as a zero-sized subview of
/// the GPUI view.
pub(crate) struct InputView {
    view: id,
    state: *mut InputState,
}

impl InputView {
    pub fn new(state: InputState) -> Self {
        let gpui_view = state.gpui_view;
        let state = Box::into_raw(Box::new(state));
        unsafe {
            let view: id = msg_send![input_view_class(), alloc];
            let view: id = msg_send![view, initWithFrame: NSRect::new(NSPoint::new(0., 0.), NSSize::new(0., 0.))];
            (*view).set_ivar::<*mut c_void>(STATE_IVAR, state as *mut c_void);
            let _: () = msg_send![gpui_view, addSubview: view];
            Self { view, state }
        }
    }

    pub fn state(&self) -> &mut InputState {
        unsafe { &mut *self.state }
    }

    pub fn is_first_responder(&self) -> bool {
        unsafe {
            let window: id = msg_send![self.view, window];
            if window == nil {
                return false;
            }
            let first_responder: id = msg_send![window, firstResponder];
            first_responder == self.view
        }
    }

    pub fn make_first_responder(&self) {
        unsafe {
            let window: id = msg_send![self.view, window];
            if window != nil && !self.is_first_responder() {
                let _: BOOL = msg_send![window, makeFirstResponder: self.view];
            }
        }
    }

    /// Hands keyboard input back to the GPUI view, if we still hold it.
    pub fn resign_first_responder(&self) {
        unsafe {
            let window: id = msg_send![self.view, window];
            if window != nil && self.is_first_responder() {
                let _: BOOL = msg_send![window, makeFirstResponder: self.state().gpui_view];
            }
        }
    }
}

impl Drop for InputView {
    fn drop(&mut self) {
        self.resign_first_responder();
        unsafe {
            (*self.view).set_ivar::<*mut c_void>(STATE_IVAR, ptr::null_mut());
            let _: () = msg_send![self.view, removeFromSuperview];
            let _: () = msg_send![self.view, release];
            drop(Box::from_raw(self.state));
        }
    }
}

unsafe fn state_of<'a>(this: &Object) -> Option<&'a mut InputState> {
    unsafe {
        let state: *mut c_void = *this.get_ivar(STATE_IVAR);
        (state as *mut InputState).as_mut()
    }
}

extern "C" fn accepts_first_responder(_: &Object, _: Sel) -> BOOL {
    YES
}

// Mouse events belong to the GPUI view underneath.
extern "C" fn hit_test(_: &Object, _: Sel, _: NSPoint) -> id {
    nil
}

unsafe fn event_characters(event: id, modifier_flags: Option<u64>) -> Option<String> {
    unsafe {
        let characters: id = match modifier_flags {
            Some(flags) => msg_send![event, charactersByApplyingModifiers: flags],
            None => msg_send![event, characters],
        };
        ns_string_to_string(characters)
    }
}

unsafe fn modifier_flags(event: id) -> u64 {
    unsafe { msg_send![event, modifierFlags] }
}

/// Port of `NSEvent.ghosttyKeyEvent`.
unsafe fn ghostty_key_event(
    event: id,
    action: ffi::ghostty_input_action_e,
    translation_flags: Option<u64>,
) -> ffi::ghostty_input_key_s {
    unsafe {
        let key_code: u16 = msg_send![event, keyCode];
        let flags = modifier_flags(event);
        let event_type: u64 = msg_send![event, type];
        let mut unshifted_codepoint = 0;
        if (event_type == NS_EVENT_TYPE_KEY_DOWN || event_type == NS_EVENT_TYPE_KEY_UP)
            && let Some(first) =
                event_characters(event, Some(0)).and_then(|chars| chars.chars().next())
        {
            unshifted_codepoint = first as u32;
        }
        // macOS offers no way to know which modifiers produced the text. Like
        // Ghostty, assume control and command never contribute to it.
        let consumed_flags = translation_flags.unwrap_or(flags)
            & !(NS_EVENT_MODIFIER_FLAG_CONTROL | NS_EVENT_MODIFIER_FLAG_COMMAND);
        ffi::ghostty_input_key_s {
            action,
            mods: ghostty_mods(flags),
            consumed_mods: ghostty_mods(consumed_flags),
            keycode: key_code as u32,
            text: ptr::null(),
            unshifted_codepoint,
            composing: false,
        }
    }
}

/// Port of `NSEvent.ghosttyCharacters`: control characters are encoded by
/// Ghostty itself, and function keys arrive as private-use codepoints.
unsafe fn ghostty_characters(event: id) -> Option<String> {
    unsafe {
        let characters = event_characters(event, None)?;
        let mut chars = characters.chars();
        if let (Some(scalar), None) = (chars.next(), chars.next()) {
            if (scalar as u32) < 0x20 {
                let flags = modifier_flags(event) & !NS_EVENT_MODIFIER_FLAG_CONTROL;
                return event_characters(event, Some(flags));
            }
            if (0xF700..=0xF8FF).contains(&(scalar as u32)) {
                return None;
            }
        }
        Some(characters)
    }
}

fn should_suppress_composing_control_input(text: Option<&str>, composing: bool) -> bool {
    let Some(text) = text else {
        return false;
    };
    if !composing {
        return false;
    }
    let mut chars = text.chars();
    matches!((chars.next(), chars.next()), (Some(scalar), None) if (scalar as u32) < 0x20)
}

unsafe fn key_action(
    state: &InputState,
    action: ffi::ghostty_input_action_e,
    event: id,
    translation_event: Option<id>,
    text: Option<&str>,
    composing: bool,
) -> bool {
    unsafe {
        let translation_flags = translation_event.map(|event| modifier_flags(event));
        let mut key_event = ghostty_key_event(event, action, translation_flags);
        key_event.composing = composing;
        // Only encode UTF-8 when it isn't a single control character; Ghostty
        // encodes those itself (otherwise ctrl+enter misbehaves).
        if let Some(text) = text
            && text.as_bytes().first().is_some_and(|byte| *byte >= 0x20)
            && let Ok(text) = CString::new(text)
        {
            key_event.text = text.as_ptr();
            return ffi::ghostty_surface_key(state.surface, key_event);
        }
        ffi::ghostty_surface_key(state.surface, key_event)
    }
}

unsafe fn committed_preedit_text_action(
    state: &InputState,
    action: ffi::ghostty_input_action_e,
    text: &str,
) -> bool {
    let Ok(text) = CString::new(text) else {
        return false;
    };
    let key_event = ffi::ghostty_input_key_s {
        action,
        mods: ffi::GHOSTTY_MODS_NONE,
        consumed_mods: ffi::GHOSTTY_MODS_NONE,
        keycode: 0,
        text: text.as_ptr(),
        unshifted_codepoint: 0,
        composing: false,
    };
    unsafe { ffi::ghostty_surface_key(state.surface, key_event) }
}

unsafe fn should_replay_committed_preedit_key(event: id) -> bool {
    unsafe {
        let key_code: u16 = msg_send![event, keyCode];
        match key_code {
            // Arrow down, right and up.
            0x7D | 0x7C | 0x7E => true,
            // Plain left arrow is not replayed: AppKit already leaves the caret in
            // place after Korean IMEs commit preedit text.
            0x7B => {
                modifier_flags(event)
                    & (NS_EVENT_MODIFIER_FLAG_SHIFT
                        | NS_EVENT_MODIFIER_FLAG_CONTROL
                        | NS_EVENT_MODIFIER_FLAG_OPTION
                        | NS_EVENT_MODIFIER_FLAG_COMMAND)
                    != 0
            }
            _ => false,
        }
    }
}

fn sync_preedit(state: &InputState, clear_if_needed: bool) {
    unsafe {
        if !state.marked_text.is_empty() {
            if let Ok(text) = CString::new(state.marked_text.as_str()) {
                ffi::ghostty_surface_preedit(state.surface, text.as_ptr(), state.marked_text.len());
            }
        } else if clear_if_needed {
            ffi::ghostty_surface_preedit(state.surface, ptr::null(), 0);
        }
    }
}

unsafe fn current_keyboard_layout_id() -> Option<String> {
    unsafe extern "C" {
        fn TISCopyCurrentKeyboardInputSource() -> *mut c_void;
        fn TISGetInputSourceProperty(source: *mut c_void, key: *const c_void) -> *const c_void;
        static kTISPropertyInputSourceID: *const c_void;
        fn CFRelease(value: *const c_void);
    }
    unsafe {
        let source = TISCopyCurrentKeyboardInputSource();
        if source.is_null() {
            return None;
        }
        let identifier = TISGetInputSourceProperty(source, kTISPropertyInputSourceID);
        let identifier = ns_string_to_string(identifier as id);
        CFRelease(source);
        identifier
    }
}

extern "C" fn key_down(this: &Object, _: Sel, event: id) {
    unsafe {
        let Some(state) = state_of(this) else {
            return;
        };
        handle_key_down(this, state, event);
    }
}

unsafe fn handle_key_down(this: &Object, state: &mut InputState, event: id) {
    unsafe {
        let flags = modifier_flags(event);

        // Translate the mods (maybe) to handle configs such as option-as-alt.
        let translation_mods_ghostty = event_modifier_flags(
            ffi::ghostty_surface_key_translation_mods(state.surface, ghostty_mods(flags)),
        );
        // Hidden bits in the event matter for some dead keys, so only the four
        // modifiers are replaced.
        let mut translation_flags = flags;
        for flag in [
            NS_EVENT_MODIFIER_FLAG_SHIFT,
            NS_EVENT_MODIFIER_FLAG_CONTROL,
            NS_EVENT_MODIFIER_FLAG_OPTION,
            NS_EVENT_MODIFIER_FLAG_COMMAND,
        ] {
            if translation_mods_ghostty & flag != 0 {
                translation_flags |= flag;
            } else {
                translation_flags &= !flag;
            }
        }

        // The original event must be reused when nothing changed, otherwise
        // input methods such as Korean break.
        let translation_event: id = if translation_flags == flags {
            event
        } else {
            let characters = event_characters(event, Some(translation_flags)).unwrap_or_default();
            let characters_ignoring_modifiers: id = msg_send![event, charactersIgnoringModifiers];
            let location: NSPoint = msg_send![event, locationInWindow];
            let timestamp: f64 = msg_send![event, timestamp];
            let window_number: i64 = msg_send![event, windowNumber];
            let is_repeat: BOOL = msg_send![event, isARepeat];
            let key_code: u16 = msg_send![event, keyCode];
            let event_type: u64 = msg_send![event, type];
            let translated: id = msg_send![class!(NSEvent),
                keyEventWithType: event_type
                location: location
                modifierFlags: translation_flags
                timestamp: timestamp
                windowNumber: window_number
                context: nil
                characters: ns_string(&characters)
                charactersIgnoringModifiers: characters_ignoring_modifiers
                isARepeat: is_repeat
                keyCode: key_code
            ];
            if translated == nil { event } else { translated }
        };

        let is_repeat: BOOL = msg_send![event, isARepeat];
        let action = if is_repeat == YES {
            ffi::GHOSTTY_ACTION_REPEAT
        } else {
            ffi::GHOSTTY_ACTION_PRESS
        };

        // While this is set, insertText accumulates instead of sending, so that
        // complex input (Korean, dead keys) is resolved by interpretKeyEvents.
        state.key_text_accumulator = Some(Vec::new());
        let marked_text_before = !state.marked_text.is_empty();
        let keyboard_id_before = if marked_text_before {
            None
        } else {
            current_keyboard_layout_id()
        };
        state.last_perform_key_timestamp = None;

        let events: id = msg_send![class!(NSArray), arrayWithObject: translation_event];
        let _: () = msg_send![this, interpretKeyEvents: events];

        let accumulated = state.key_text_accumulator.take().unwrap_or_default();

        // An input method that switched the keyboard layout grabbed the key.
        if !marked_text_before && keyboard_id_before != current_keyboard_layout_id() {
            return;
        }

        sync_preedit(state, marked_text_before);

        let composing = !state.marked_text.is_empty() || marked_text_before;

        if marked_text_before && !accumulated.is_empty() {
            for text in &accumulated {
                if should_suppress_composing_control_input(Some(text), composing) {
                    continue;
                }
                committed_preedit_text_action(state, action, text);
            }
            if should_replay_committed_preedit_key(translation_event) {
                key_action(state, action, event, Some(translation_event), None, false);
            }
            return;
        }

        if !accumulated.is_empty() {
            for text in &accumulated {
                if should_suppress_composing_control_input(Some(text), composing) {
                    continue;
                }
                key_action(
                    state,
                    action,
                    event,
                    Some(translation_event),
                    Some(text),
                    false,
                );
            }
        } else {
            let characters = event_characters(event, None);
            if should_suppress_composing_control_input(characters.as_deref(), composing) {
                return;
            }
            let text = ghostty_characters(translation_event);
            key_action(
                state,
                action,
                event,
                Some(translation_event),
                text.as_deref(),
                composing,
            );
        }
    }
}

extern "C" fn key_up(this: &Object, _: Sel, event: id) {
    unsafe {
        let Some(state) = state_of(this) else {
            return;
        };
        let _: () = msg_send![state.gpui_view, keyUp: event];
        key_action(state, ffi::GHOSTTY_ACTION_RELEASE, event, None, None, false);
    }
}

extern "C" fn flags_changed(this: &Object, _: Sel, event: id) {
    unsafe {
        let Some(state) = state_of(this) else {
            return;
        };
        // GPUI tracks modifiers for hover effects and modifier-only bindings.
        let _: () = msg_send![state.gpui_view, flagsChanged: event];

        let key_code: u16 = msg_send![event, keyCode];
        let modifier = match key_code {
            0x39 => ffi::GHOSTTY_MODS_CAPS,
            0x38 | 0x3C => ffi::GHOSTTY_MODS_SHIFT,
            0x3B | 0x3E => ffi::GHOSTTY_MODS_CTRL,
            0x3A | 0x3D => ffi::GHOSTTY_MODS_ALT,
            0x37 | 0x36 => ffi::GHOSTTY_MODS_SUPER,
            _ => return,
        };
        if !state.marked_text.is_empty() {
            return;
        }
        let flags = modifier_flags(event);
        let mods = ghostty_mods(flags);
        let mut action = ffi::GHOSTTY_ACTION_RELEASE;
        if mods & modifier != 0 {
            // A press only counts if the modifier on the side of this key is the
            // one held; otherwise it is the release of this side.
            let side_pressed = match key_code {
                0x3C => flags & NX_DEVICE_RIGHT_SHIFT_KEY_MASK != 0,
                0x3E => flags & NX_DEVICE_RIGHT_CONTROL_KEY_MASK != 0,
                0x3D => flags & NX_DEVICE_RIGHT_ALT_KEY_MASK != 0,
                0x36 => flags & NX_DEVICE_RIGHT_COMMAND_KEY_MASK != 0,
                _ => true,
            };
            if side_pressed {
                action = ffi::GHOSTTY_ACTION_PRESS;
            }
        }
        key_action(state, action, event, None, None, false);
    }
}

extern "C" fn perform_key_equivalent(this: &Object, _: Sel, event: id) -> BOOL {
    unsafe {
        let Some(state) = state_of(this) else {
            return NO;
        };
        let event_type: u64 = msg_send![event, type];
        if event_type != NS_EVENT_TYPE_KEY_DOWN {
            return NO;
        }

        // Ghostty keybindings (cmd+c, cmd+v, cmd+k, ...) take the key before
        // the menu does.
        let mut key_event = ghostty_key_event(event, ffi::GHOSTTY_ACTION_PRESS, None);
        let characters = event_characters(event, None).unwrap_or_default();
        let is_binding = CString::new(characters)
            .map(|characters| {
                key_event.text = characters.as_ptr();
                let mut flags: ffi::ghostty_binding_flags_e = 0;
                ffi::ghostty_surface_key_is_binding(state.surface, key_event, &mut flags)
            })
            .unwrap_or(false);
        if is_binding {
            handle_key_down(this, state, event);
            return YES;
        }

        let characters_ignoring_modifiers: id = msg_send![event, charactersIgnoringModifiers];
        let characters_ignoring_modifiers =
            ns_string_to_string(characters_ignoring_modifiers).unwrap_or_default();
        let flags = modifier_flags(event);
        let equivalent = match characters_ignoring_modifiers.as_str() {
            // Pass ctrl+return through; AppKit would otherwise eat it.
            "\r" if flags & NS_EVENT_MODIFIER_FLAG_CONTROL != 0 => "\r".to_string(),
            "\r" => return NO,
            // ctrl+/ is sent as ctrl+_ like other terminals do.
            "/" if flags & NS_EVENT_MODIFIER_FLAG_CONTROL != 0
                && flags
                    & (NS_EVENT_MODIFIER_FLAG_SHIFT
                        | NS_EVENT_MODIFIER_FLAG_COMMAND
                        | NS_EVENT_MODIFIER_FLAG_OPTION)
                    == 0 =>
            {
                "_".to_string()
            }
            "/" => return NO,
            _ => {
                let timestamp: f64 = msg_send![event, timestamp];
                if timestamp == 0. {
                    return NO;
                }
                if flags & (NS_EVENT_MODIFIER_FLAG_COMMAND | NS_EVENT_MODIFIER_FLAG_CONTROL) == 0 {
                    state.last_perform_key_timestamp = None;
                    return NO;
                }
                // AppKit sends some cmd/ctrl keys here twice without a keyDown.
                // The second time we treat it as a key down.
                if state.last_perform_key_timestamp.take() == Some(timestamp) {
                    event_characters(event, None).unwrap_or_default()
                } else {
                    state.last_perform_key_timestamp = Some(timestamp);
                    return NO;
                }
            }
        };

        let location: NSPoint = msg_send![event, locationInWindow];
        let timestamp: f64 = msg_send![event, timestamp];
        let window_number: i64 = msg_send![event, windowNumber];
        let key_code: u16 = msg_send![event, keyCode];
        let final_event: id = msg_send![class!(NSEvent),
            keyEventWithType: NS_EVENT_TYPE_KEY_DOWN
            location: location
            modifierFlags: flags
            timestamp: timestamp
            windowNumber: window_number
            context: nil
            characters: ns_string(&equivalent)
            charactersIgnoringModifiers: ns_string(&equivalent)
            isARepeat: NO
            keyCode: key_code
        ];
        if final_event == nil {
            return NO;
        }
        handle_key_down(this, state, final_event);
        YES
    }
}

extern "C" fn has_marked_text(this: &Object, _: Sel) -> BOOL {
    match unsafe { state_of(this) } {
        Some(state) if !state.marked_text.is_empty() => YES,
        _ => NO,
    }
}

fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

extern "C" fn marked_range(this: &Object, _: Sel) -> NSRange {
    match unsafe { state_of(this) } {
        Some(state) if !state.marked_text.is_empty() => {
            NSRange::from(0..utf16_len(&state.marked_text))
        }
        _ => NSRange::invalid(),
    }
}

extern "C" fn selected_range(this: &Object, _: Sel) -> NSRange {
    let Some(state) = (unsafe { state_of(this) }) else {
        return NSRange::invalid();
    };
    unsafe {
        let mut text: ffi::ghostty_text_s = std::mem::zeroed();
        if !ffi::ghostty_surface_read_selection(state.surface, &mut text) {
            return NSRange::invalid();
        }
        let range = NSRange {
            location: text.offset_start as NSUInteger,
            length: text.offset_len as NSUInteger,
        };
        ffi::ghostty_surface_free_text(state.surface, &mut text);
        range
    }
}

unsafe fn string_from_text_input(text: id) -> Option<String> {
    unsafe {
        let is_attributed: BOOL = msg_send![text, isKindOfClass: class!(NSAttributedString)];
        let text: id = if is_attributed == YES {
            msg_send![text, string]
        } else {
            text
        };
        ns_string_to_string(text)
    }
}

extern "C" fn set_marked_text(this: &Object, _: Sel, text: id, _: NSRange, _: NSRange) {
    let Some(state) = (unsafe { state_of(this) }) else {
        return;
    };
    state.marked_text = unsafe { string_from_text_input(text) }.unwrap_or_default();
    // Inside keyDown the preedit is synced once interpretKeyEvents returns;
    // otherwise (e.g. the keyboard layout changed) sync right away.
    if state.key_text_accumulator.is_none() {
        sync_preedit(state, true);
    }
}

extern "C" fn unmark_text(this: &Object, _: Sel) {
    let Some(state) = (unsafe { state_of(this) }) else {
        return;
    };
    if !state.marked_text.is_empty() {
        state.marked_text.clear();
        sync_preedit(state, true);
    }
}

extern "C" fn valid_attributes_for_marked_text(_: &Object, _: Sel) -> id {
    unsafe { msg_send![class!(NSArray), array] }
}

extern "C" fn attributed_substring_for_proposed_range(
    this: &Object,
    _: Sel,
    range: NSRange,
    _: *mut c_void,
) -> id {
    let Some(state) = (unsafe { state_of(this) }) else {
        return nil;
    };
    if range.length == 0 {
        return nil;
    }
    unsafe {
        let mut text: ffi::ghostty_text_s = std::mem::zeroed();
        if !ffi::ghostty_surface_read_selection(state.surface, &mut text) {
            return nil;
        }
        let bytes = std::slice::from_raw_parts(text.text as *const u8, text.text_len);
        let string = String::from_utf8_lossy(bytes).into_owned();
        ffi::ghostty_surface_free_text(state.surface, &mut text);
        let attributed: id = msg_send![class!(NSAttributedString), alloc];
        let attributed: id = msg_send![attributed, initWithString: ns_string(&string)];
        msg_send![attributed, autorelease]
    }
}

extern "C" fn insert_text(this: &Object, _: Sel, text: id, _: NSRange) {
    unsafe {
        // AppKit may call this outside of a key event (e.g. the emoji picker is
        // dismissed); Ghostty ignores those too.
        let application: id = msg_send![class!(NSApplication), sharedApplication];
        let current_event: id = msg_send![application, currentEvent];
        if current_event == nil {
            return;
        }
        let Some(state) = state_of(this) else {
            return;
        };
        let Some(characters) = string_from_text_input(text) else {
            return;
        };
        unmark_text(this, sel!(unmarkText));
        if let Some(accumulator) = state.key_text_accumulator.as_mut() {
            accumulator.push(characters);
            return;
        }
        if let Ok(characters_c) = CString::new(characters.as_str()) {
            ffi::ghostty_surface_text(state.surface, characters_c.as_ptr(), characters.len());
        }
    }
}

extern "C" fn character_index_for_point(_: &Object, _: Sel, _: NSPoint) -> NSUInteger {
    0
}

extern "C" fn first_rect_for_character_range(
    this: &Object,
    _: Sel,
    range: NSRange,
    _: *mut c_void,
) -> NSRect {
    let zero = NSRect::new(NSPoint::new(0., 0.), NSSize::new(0., 0.));
    let Some(state) = (unsafe { state_of(this) }) else {
        return zero;
    };
    unsafe {
        let (cell_width, cell_height) = state.cell_size;
        let mut x = 0.;
        let mut y = 0.;
        let mut width = cell_width;
        let mut height = cell_height;
        ffi::ghostty_surface_ime_point(state.surface, &mut x, &mut y, &mut width, &mut height);
        if range.length == 0 && width > 0. {
            width = 0.;
            x += cell_width * (range.location + range.length) as f64;
        }

        let window: id = msg_send![this, window];
        if window == nil {
            return zero;
        }
        let content_view: id = msg_send![window, contentView];
        let content_frame: NSRect = msg_send![content_view, frame];
        let (origin_x, origin_y) = state.terminal_origin;
        // Ghostty reports the point from the terminal's top-left; AppKit window
        // coordinates start at the bottom-left.
        let window_rect = NSRect::new(
            NSPoint::new(origin_x + x, content_frame.size.height - (origin_y + y)),
            NSSize::new(width, height.max(cell_height)),
        );
        msg_send![window, convertRectToScreen: window_rect]
    }
}

extern "C" fn do_command_by_selector(_: &Object, _: Sel, _: Sel) {
    // Swallow commands such as `insertNewline:` that the text system derives
    // from keys we encode ourselves; this also suppresses the system beep.
}
