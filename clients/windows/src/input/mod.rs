// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Input capture.
//!
//! The session window's procedure hands every message to [`WindowInput`]
//! first ([`attach`] subclasses the window), before winit and egui see it.
//! Keys, the mouse and the pen leave from here the moment Windows delivers
//! them, as [`InputEvent`]s to the session's [`InputSink`]; nothing waits
//! for a UI pass.
//!
//! Everything this module decides is shared: the PC keyboard policy, the
//! Deck's chords, who has the keyboard and the hook's routing
//! (`arcen_input::keyboard`), committed text (`arcen_input::text`), pen
//! axes (`arcen_input::deck_pen`) and the wheel (`arcen_input::deck_wheel`).
//! What is here only reads Windows and acts on what comes back.

// Win32 input calls; each block states what it relies on.
#![allow(unsafe_code)]

mod cursor;
mod hook;
mod keyboard;
mod led;
mod mouse;
mod pen;
mod subclass;
mod touchpad;

use std::time::Instant;

use arcen_deck_core::input::overlay_pointer::{
    HeldBy, OverlayPointerRegions, PointerMessage, PointerRoute, keys_to_host, over_ui,
    route_pointer,
};
use arcen_input::deck_pen::ImageRect;
use arcen_input::keyboard::{
    CaptureChange, ClickRoute, DeckShortcut, HostKeyEdge, KeyboardCapture, KeyboardCapturePolicy,
    KeyboardOwner, KeyboardStatus, PcKeyboardSettings, SendKey,
};
use arcen_input::text::{HostTextTruth, TypingMode};
use arcen_input::touchpad::TouchpadOutput;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};

pub use arcen_input::deck_events::{
    DeckInputEvent as InputEvent, DeckInputSink as InputSink, HostInputFacts, LedCursor,
};
pub use cursor::HostCursorImage;
#[cfg(feature = "dev-tools")]
pub use hook::LAB_KEY_MARK;
pub use subclass::{attach, detach, with_input};

/// The session window's input: one per window, owned by its subclass.
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent window facts: active, focused, bridged pen"
)]
pub struct WindowInput {
    hwnd: HWND,
    sink: Box<dyn InputSink>,
    keyboard: keyboard::Keyboard,
    mouse: mouse::Mouse,
    pen: pen::Pen,
    /// `DirectManipulation` for the touchpad while a session is on screen
    /// (`None`: unavailable, the touchpad stays a wheel).
    touchpad: Option<touchpad::Touchpad>,
    cursor: cursor::Cursor,
    led: led::Led,
    image: Option<ImageRect>,
    active: bool,
    focused: bool,
    /// Who has the keyboard (shared): the host, or this PC after
    /// Ctrl+Alt+Home until the chord again or a click into the picture.
    capture: KeyboardCapture,
    /// Buttons whose press gave the keyboard back to the host (bit per
    /// left, middle, right): neither it nor its release reaches the host.
    consumed_buttons: u8,
    /// Where the Deck's own UI takes the pointer over the session (the
    /// fullscreen connection bar, a modal Help window), published by the
    /// shell every pass.
    overlay: OverlayPointerRegions,
    /// The last pointer move was on the Deck's UI.
    ui_pointer: bool,
    /// Alt+F4 under a modal window: F4 is held, and the Alt press it came
    /// with (whose release the default procedure would take for the menu,
    /// and which is never seeded into the host's modifiers when the modal
    /// window closes before it is let go).
    modal_keys: ModalKeys,
    /// Buttons whose press went to the Deck's UI (bit per left, middle,
    /// right), released there too.
    ui_buttons: u8,
    /// This input decides the window's input method (a session is on screen
    /// and the connection bar does not have the keyboard); otherwise winit
    /// and egui do, which switch it on only for a focused text field.
    owns_input_method: bool,
    epoch: Instant,
    host_cursor: Option<arcen_input::cursor::DeckCursor>,
    bridged_pen: bool,
    cursor_scale: f32,
}

impl std::fmt::Debug for WindowInput {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WindowInput")
            .field("active", &self.active)
            .field("focused", &self.focused)
            .field("grabbed", &self.grab_active())
            .finish_non_exhaustive()
    }
}

impl WindowInput {
    /// Input for `hwnd`, sending to `sink`. Inactive until
    /// [`Self::set_session`] says a session is on screen.
    #[must_use]
    pub fn new(hwnd: HWND, sink: Box<dyn InputSink>, policy: KeyboardCapturePolicy) -> Self {
        Self {
            hwnd,
            sink,
            keyboard: keyboard::Keyboard::default(),
            mouse: mouse::Mouse::default(),
            pen: pen::Pen::default(),
            touchpad: None,
            cursor: cursor::Cursor::default(),
            led: led::Led::default(),
            image: None,
            active: false,
            focused: false,
            capture: KeyboardCapture::new(policy),
            consumed_buttons: 0,
            overlay: OverlayPointerRegions::default(),
            ui_pointer: false,
            modal_keys: ModalKeys::default(),
            ui_buttons: 0,
            owns_input_method: false,
            epoch: Instant::now(),
            host_cursor: None,
            bridged_pen: false,
            cursor_scale: 1.0,
        }
    }

    /// Reads whether the window has the keyboard focus now (it may have had
    /// it before the input was attached, so no `WM_SETFOCUS` comes).
    pub(crate) fn sync_focus(&mut self) {
        // SAFETY: a plain read of this thread's focus window.
        let focused =
            unsafe { windows::Win32::UI::Input::KeyboardAndMouse::GetFocus() } == self.hwnd;
        if focused != self.focused {
            self.focused = focused;
            self.capture.set_focused(self.keyboard_focus());
            if focused {
                forget_released_dismiss_keys();
            }
            if self.keyboard_focus() {
                self.keyboard.focus_gained(self.modal_keys.alt_chord);
            } else if !focused {
                // As on WM_KILLFOCUS: Alt+F4 is let go elsewhere (a held Esc
                // or Enter stays the Deck's in the window focus went to).
                self.modal_keys = ModalKeys::default();
                self.release_all("focus_out");
            }
            self.apply_grab();
        }
    }

    /// A session is on screen (with the host's facts and this connection's
    /// keyboard settings), or not. Ending one releases everything it held.
    pub fn set_session(&mut self, session: Option<(HostInputFacts, PcKeyboardSettings)>) {
        if let Some((facts, settings)) = session {
            let edges = self.keyboard.set_host(facts, settings);
            self.send_keys(edges);
            self.mouse.set_precise_scroll(facts.precise_scroll);
            if !self.active {
                self.capture.set_session(true);
            }
            self.active = true;
            if self.touchpad.is_none() {
                self.touchpad = touchpad::Touchpad::new(self.hwnd)
                    .inspect_err(|error| {
                        tracing::info!(
                            target: arcen_deck_core::log_target::INPUT,
                            %error,
                            "touchpad: DirectManipulation unavailable; two-finger scroll stays a wheel"
                        );
                    })
                    .ok();
            }
        } else {
            self.close_touchpad();
            self.release_all("session_teardown");
            self.keyboard.set_host_keymap(None);
            self.led.reset();
            self.sink.led_cursor(None);
            self.active = false;
            self.capture.set_session(false);
            self.consumed_buttons = 0;
            let _ = self.keyboard.set_released(false);
        }
        self.apply_grab();
    }

    /// The session's pen-button mapping (its host's layout, this Deck's
    /// pen settings).
    pub fn set_pen_buttons(&mut self, map: arcen_input::pen_buttons::PenButtonMap) {
        self.pen.set_button_map(map);
    }

    /// Where the video is drawn in the client area, in client pixels.
    pub fn set_image_rect(&mut self, image: Option<ImageRect>) {
        if image.is_none() {
            let sink = &self.sink;
            self.mouse.release_buttons(|event| sink.input(event));
        }
        self.image = image;
    }

    /// The host's keymap notice (`host_keymap`): what its layout types at
    /// each key position, so a key it would type differently goes as text
    /// (the macOS Deck's `on_host_keymap`).
    pub fn set_host_keymap(&mut self, keymap: &arcen_protocol::messages::HostKeymapMsg) {
        self.keyboard.set_host_keymap(Some(keymap));
    }

    /// The typing setting and the host's text truth for committed text.
    pub fn set_typing(&mut self, typing: TypingMode, host: HostTextTruth) {
        self.keyboard.set_typing(typing, host);
        self.apply_input_method();
    }

    /// The window entered or left fullscreen (the policy may follow it).
    pub fn set_fullscreen(&mut self, fullscreen: bool) {
        self.capture.set_fullscreen(fullscreen);
        self.apply_grab();
    }

    /// Settings changed when the system shortcuts go to the host.
    pub fn set_capture_policy(&mut self, policy: KeyboardCapturePolicy) {
        self.capture.set_policy(policy);
        self.apply_grab();
    }

    /// The keyboard switch's chord (Settings ▸ Input ▸ Keyboard switch), for
    /// this window's keys and the keyboard hook.
    pub fn set_switch_chord(&mut self, switch: arcen_input::keyboard::KeyboardSwitchChord) {
        self.keyboard.set_switch_chord(switch);
        hook::set_switch_chord(switch);
    }

    /// The keyboard switch or the menu item: the keyboard goes to this PC (every
    /// key held on the host is released first) or back to the host.
    pub fn toggle_keyboard(&mut self) {
        let change = self.capture.toggle();
        self.keyboard_changed(change);
    }

    /// The keyboard goes to this PC (`true`: the fullscreen connection bar
    /// was pinned) or back to the host (the bar closed with Esc or a
    /// command).
    pub fn set_keyboard_released(&mut self, released: bool) {
        let change = if released {
            self.capture.release()
        } else {
            self.capture.recapture()
        };
        self.keyboard_changed(change);
    }

    /// Whether this PC has the keyboard (the host gets no key).
    #[must_use]
    pub fn keyboard_released(&self) -> bool {
        self.capture.owner() == KeyboardOwner::ThisComputer
    }

    /// What the indicator says.
    #[must_use]
    pub fn keyboard_status(&self) -> KeyboardStatus {
        self.capture.status()
    }

    /// Whether the session takes the system shortcuts now.
    #[must_use]
    pub fn grab_active(&self) -> bool {
        self.active && self.capture.takes_system_shortcuts()
    }

    fn keyboard_changed(&mut self, change: CaptureChange) {
        let released = change.owner == KeyboardOwner::ThisComputer;
        if change.release_host_keys {
            self.end_composition();
        }
        if released != self.keyboard.released() {
            let edges = self.keyboard.set_released(released);
            self.send_keys(edges);
            if released && self.mouse.unlock() {
                // The pointer is this PC's too.
                self.sink.input(InputEvent::PointerLockReleased {
                    reason: "keyboard_released",
                });
            }
            tracing::info!(
                target: arcen_deck_core::log_target::INPUT,
                released,
                "keyboard: {}",
                if released { "this PC" } else { "the host" }
            );
        }
        self.apply_grab();
    }

    /// Where the Deck's own UI takes the pointer over the session now
    /// (`arcen_deck_core::input::overlay_pointer`): the shell publishes the
    /// fullscreen top-edge strip, the open connection bar and whether a
    /// modal Help window is open every pass. A modal window takes the
    /// keyboard too: everything held on the host is released, as on a focus
    /// loss, and keys reach the Deck's UI until it closes.
    pub fn set_overlay_pointer(&mut self, regions: OverlayPointerRegions) {
        let keys_changed = keys_to_host(regions) != keys_to_host(self.overlay);
        self.overlay = regions;
        if regions == OverlayPointerRegions::default() {
            self.ui_pointer = false;
        }
        if keys_changed {
            self.capture.set_focused(self.keyboard_focus());
            if !keys_to_host(regions) {
                if self.active {
                    self.release_all("modal_window");
                }
            } else if self.focused {
                self.keyboard.focus_gained(self.modal_keys.alt_chord);
            }
            self.apply_grab();
        }
    }

    /// The window has the keyboard focus and no modal window of the Deck's
    /// takes the keys: they may reach the host.
    fn keyboard_focus(&self) -> bool {
        self.focused && keys_to_host(self.overlay)
    }

    /// Locks the pointer (relative motion) or releases it.
    pub fn set_pointer_lock(&mut self, locked: bool) {
        if locked {
            if self.active && self.keyboard_focus() {
                self.mouse.lock(self.hwnd);
            }
        } else if self.mouse.unlock() {
            self.sink
                .input(InputEvent::PointerLockReleased { reason: "menu" });
        }
    }

    /// Whether the pointer is locked.
    #[must_use]
    pub fn pointer_locked(&self) -> bool {
        self.mouse.locked()
    }

    /// The host's cursor to show over the image (`None`: the system arrow).
    ///
    /// `shown` is the shared cursor state (`DeckCursorState::current`) and
    /// `bridged_pen` whether Native tablet drives the host pointer: they
    /// decide the mark drawn while the host leads.
    pub fn set_host_cursor(
        &mut self,
        cursor: Option<HostCursorImage>,
        shown: Option<arcen_input::cursor::DeckCursor>,
        bridged_pen: bool,
    ) {
        let retired = self.cursor.set(cursor);
        self.led.set_shown(shown, bridged_pen);
        self.show_cursor_now();
        drop(retired);
        if self.led.host_leads() {
            self.sink.led_cursor(self.led.current());
        }
    }

    /// The shared cursor state's current cursor (the session's
    /// `DeckCursorState`), drawn at the last [`Self::set_cursor_scale`];
    /// `bridged_pen` whether Native tablet drives the host pointer. `None`
    /// is the system arrow.
    pub fn set_host_cursor_shown(
        &mut self,
        shown: Option<arcen_input::cursor::DeckCursor>,
        bridged_pen: bool,
    ) {
        if shown == self.host_cursor && bridged_pen == self.bridged_pen {
            return;
        }
        let image = shown
            .as_ref()
            .and_then(|cursor| HostCursorImage::from_deck_cursor(cursor, self.cursor_scale));
        self.host_cursor.clone_from(&shown);
        self.bridged_pen = bridged_pen;
        self.set_host_cursor(image, shown, bridged_pen);
    }

    /// Device pixels per host stream pixel (the video fit); the host's
    /// cursor image is redrawn at the new size when it changes.
    pub fn set_cursor_scale(&mut self, scale: f32) {
        if !scale.is_finite() || scale <= 0.0 || (scale - self.cursor_scale).abs() < 1e-3 {
            return;
        }
        self.cursor_scale = scale;
        if let Some(cursor) = &self.host_cursor {
            let retired = self
                .cursor
                .set(HostCursorImage::from_deck_cursor(cursor, scale));
            self.show_cursor_now();
            drop(retired);
        }
    }

    fn show_cursor_now(&self) {
        if self.pointer_over_image() {
            self.cursor.apply_now(self.led.host_leads());
        }
    }

    /// The host's pointer position (a `CursorPosition` the session already
    /// applied): the host leads until the Deck's own pointer moves.
    pub fn host_pointer_position(
        &mut self,
        position: arcen_protocol::cursor_position::CursorPosition,
    ) {
        if self.led.reported(position, Instant::now()) {
            self.sink.led_cursor(self.led.current());
        }
    }

    /// A `CursorPosition` record from the input lane: the host's pointer
    /// moved. Malformed and stale records are dropped.
    pub fn cursor_position_record(&mut self, bytes: &[u8]) {
        if self.led.record(bytes, Instant::now()) {
            self.sink.led_cursor(self.led.current());
        }
    }

    /// Sends Ctrl+Alt+Del to the host (the menu item).
    pub fn send_secure_attention(&mut self) {
        let edges = self.keyboard.secure_attention();
        self.send_keys(edges);
    }

    /// Connection ▸ Send Key: one key, after everything held on the host is
    /// released (whoever has the keyboard: the menu was chosen on purpose).
    pub fn send_key(&mut self, key: SendKey) {
        if !self.active {
            return;
        }
        let edges = self.keyboard.send_key(key);
        self.send_keys(edges);
    }

    /// Sends a tapped Alt+F4 to the host (Quit was not held long enough).
    pub fn tap_on_host(&mut self, qt_key: u32) {
        let edges = self.keyboard.tap(qt_key);
        self.send_keys(edges);
    }

    /// Whether the Deck still holds a key it took for a chord (Alt+F4's F4,
    /// for hold-to-quit).
    #[must_use]
    pub fn holds_deck_key(&self, qt_key: u32) -> bool {
        self.keyboard.holds_deck_key(qt_key)
            || (qt_key == arcen_input::keyboard::pc::KEY_F4 && self.modal_keys.f4)
    }

    /// Release Stuck Keys: every key and button held on the host, a pen in
    /// range and the pointer lock, and the host resets its modifiers (what
    /// F9 did before it went to the host).
    pub fn release_modifiers(&mut self) {
        if !self.active {
            return;
        }
        let reason = "release_stuck_keys";
        self.release_all(reason);
        self.sink.input(InputEvent::Reset { reason });
    }

    /// One window message; `Some` when it was handled and must not reach
    /// the default procedure.
    pub fn handle(&mut self, msg: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
        use windows::Win32::UI::WindowsAndMessaging::{
            WM_KEYDOWN, WM_KILLFOCUS, WM_SETCURSOR, WM_SETFOCUS, WM_SYSKEYDOWN,
        };
        match msg {
            WM_SETFOCUS => {
                self.focused = true;
                forget_released_dismiss_keys();
                self.capture.set_focused(self.keyboard_focus());
                if self.keyboard_focus() {
                    self.keyboard.focus_gained(self.modal_keys.alt_chord);
                }
                self.apply_grab();
                return None;
            }
            WM_KILLFOCUS => {
                self.focused = false;
                self.modal_keys = ModalKeys::default();
                self.capture.set_focused(false);
                self.release_all("focus_out");
                self.apply_grab();
                return None;
            }
            WM_SETCURSOR => {
                // Over the Deck's own UI the system's arrow shows.
                return self.cursor.set_cursor(
                    self.hwnd,
                    lparam,
                    self.pointer_over_image() && !self.ui_pointer,
                    self.led.host_leads(),
                );
            }
            _ => {}
        }
        if !self.active {
            return None;
        }
        if msg == hook::WM_HOOK_KEY && (!self.keyboard_focus() || self.keyboard_released()) {
            // Taken by the hook while the window was in front, posted, and
            // only now handled after focus left (whose release went first),
            // a modal window opened or the keyboard went to this PC: a press
            // sent now would stay held on the host.
            return Some(LRESULT(0));
        }
        if let Some(result) = self.modal_key(msg, wparam, lparam) {
            return Some(result);
        }
        let keys_to_host = keys_to_host(self.overlay);
        if keys_to_host
            && self.keyboard_released()
            && self.keyboard.released_key(msg, wparam, lparam)
        {
            // Ctrl+Alt+Home again: the keyboard goes back to the host.
            self.toggle_keyboard();
            self.sink
                .input(InputEvent::Shortcut(DeckShortcut::ReleaseKeyboard));
            return Some(LRESULT(0));
        }
        if matches!(msg, WM_KEYDOWN | WM_SYSKEYDOWN) {
            // egui may have switched the input method since the last change
            // this input saw (a text field in the connection bar).
            self.apply_input_method();
        }
        if keys_to_host
            && !self.keyboard_released()
            && let Some(result) = self.handle_keyboard(msg, wparam, lparam)
        {
            return Some(result);
        }
        if keys_to_host {
            // Under a modal window the touchpad and the pen are the system's
            // pointer (a pen tap is a click), which the UI takes below.
            if let Some(result) = self.handle_touchpad(msg, wparam) {
                return Some(result);
            }
            if let Some(result) = self.handle_pen(msg, wparam) {
                return Some(result);
            }
        }
        self.handle_mouse(msg, wparam, lparam)
    }

    /// The keys a modal window takes from every session window.
    ///
    /// Alt+F4: never the default procedure's (it would close the window at
    /// once, ending the session without the hold), but the Deck's quit
    /// chord, as in a session: let go soon it closes the modal window, held
    /// it quits (`shell::launcher::drive_quit_hold`).
    ///
    /// Esc or Enter: closes the modal window in front, from whichever
    /// session window has the keyboard (`InputEvent::DismissModal`).
    ///
    /// A key taken here keeps its repeats, characters and release, even
    /// after the modal window closed: the host never saw the press, so a
    /// repeat must not reach it as a fresh one. A held Esc or Enter is the
    /// session's, not one window's ([`dismiss_key_held`]): it stays the
    /// Deck's in whichever session window has the keyboard until it is let
    /// go. A new press after the release is the host's again, also one after
    /// a release outside the Deck (a press that is no repeat, or the key
    /// found up when a session window gains focus).
    fn modal_key(&mut self, msg: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
        use windows::Win32::UI::Input::KeyboardAndMouse::{VK_ESCAPE, VK_RETURN};
        use windows::Win32::UI::WindowsAndMessaging::{
            WM_CHAR, WM_KEYDOWN, WM_KEYUP, WM_SYSCHAR, WM_SYSKEYDOWN, WM_SYSKEYUP,
        };
        // Once the window has closed the hook may take a held key's repeat
        // or release (Alt+Esc): it comes posted, Virtual-Key and flags packed.
        let hooked = msg == hook::WM_HOOK_KEY;
        let (msg, key) = if hooked {
            #[allow(clippy::cast_possible_truncation, reason = "packed by the hook")]
            let (vk, flags) = (wparam.0 as u16, (wparam.0 >> 16) as u32);
            let up = flags & hook::LLKHF_UP != 0;
            (if up { WM_KEYUP } else { WM_KEYDOWN }, vk)
        } else {
            (msg, u16::try_from(wparam.0).ok()?)
        };
        let mut held = dismiss_key_held(key);
        let down = matches!(msg, WM_KEYDOWN | WM_SYSKEYDOWN);
        // Bit 30: the key was down before this message (a repeat). The
        // hook's copy does not say; it is taken as one.
        let repeat = hooked || lparam.0 & (1 << 30) != 0;
        if held == Some(true) && down && !repeat {
            // Let go outside the Deck: this is a new press.
            set_dismiss_key_held(key, false);
            held = Some(false);
        }
        match (msg, held) {
            // A repeat of a dismissing key, or its release, with Alt down
            // meanwhile too.
            (WM_KEYDOWN | WM_SYSKEYDOWN, Some(true)) => {}
            (WM_KEYUP | WM_SYSKEYUP, Some(true)) => set_dismiss_key_held(key, false),
            (WM_KEYDOWN, Some(false)) if !hooked && !keys_to_host(self.overlay) => {
                set_dismiss_key_held(key, true);
                self.sink.input(InputEvent::DismissModal);
            }
            // The characters the system makes of them (Esc, carriage return).
            (WM_CHAR | WM_SYSCHAR, _)
                if (key == 0x1B && dismiss_key_held(VK_ESCAPE.0) == Some(true))
                    || (key == 0x0D && dismiss_key_held(VK_RETURN.0) == Some(true)) => {}
            _ if hooked => return None,
            _ => return self.modal_quit_chord(msg, key),
        }
        Some(LRESULT(0))
    }

    /// Alt+F4 under a modal window (`modal_key`).
    fn modal_quit_chord(&mut self, msg: u32, key: u16) -> Option<LRESULT> {
        use windows::Win32::UI::Input::KeyboardAndMouse::{VK_F4, VK_MENU};
        use windows::Win32::UI::WindowsAndMessaging::{
            WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN, WM_SYSKEYUP,
        };
        match msg {
            // Its repeats after the modal window closed (a quick Alt+F4 in
            // another session window): still the Deck's, never a fresh F4.
            WM_SYSKEYDOWN | WM_KEYDOWN if key == VK_F4.0 && self.modal_keys.f4 => {}
            WM_SYSKEYDOWN if key == VK_F4.0 && !keys_to_host(self.overlay) => {
                self.modal_keys.alt_chord = true;
                if !std::mem::replace(&mut self.modal_keys.f4, true) {
                    self.sink.input(InputEvent::Shortcut(DeckShortcut::Quit));
                }
            }
            WM_SYSKEYUP | WM_KEYUP if key == VK_F4.0 && self.modal_keys.f4 => {
                self.modal_keys.f4 = false;
            }
            WM_SYSKEYUP | WM_KEYUP if key == VK_MENU.0 && self.modal_keys.alt_chord => {
                self.modal_keys.alt_chord = false;
            }
            _ => return None,
        }
        Some(LRESULT(0))
    }

    fn handle_keyboard(&mut self, msg: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
        // A hooked key drains the window's queued keys before it: the Esc or
        // Enter a modal window holds stays the Deck's there too.
        self.keyboard.set_modal_held(&held_dismiss_keys());
        let outcome = self.keyboard.handle(self.hwnd, msg, wparam, lparam);
        for vk in self.keyboard.take_modal_released() {
            set_dismiss_key_held(vk, false);
        }
        let outcome = outcome?;
        for action in outcome.events {
            if action == InputEvent::Shortcut(DeckShortcut::ReleaseKeyboard) {
                // At once, not on the next UI pass: no further key may
                // reach the host.
                self.toggle_keyboard();
            }
            self.sink.input(action);
        }
        // Reading the queue may have delivered a focus change this window
        // could not take while busy (a nested PeekMessage).
        self.sync_focus();
        outcome.result
    }

    fn handle_touchpad(&mut self, msg: u32, wparam: WPARAM) -> Option<LRESULT> {
        use windows::Win32::UI::WindowsAndMessaging::{DM_POINTERHITTEST, WM_TIMER};
        let image = self.image?;
        match msg {
            DM_POINTERHITTEST => {
                // Only a contact over the picture is the host's.
                if !mouse::Mouse::over(self.hwnd, image) && !self.mouse.locked() {
                    return None;
                }
                let touchpad = self.touchpad.as_mut()?;
                touchpad.hit_test(self.hwnd, wparam).then_some(LRESULT(0))
            }
            WM_TIMER if wparam.0 == touchpad::TIMER_ID => {
                let outputs = self.touchpad.as_mut()?.tick(self.hwnd);
                let scale = f64::from(self.cursor_scale);
                for output in outputs {
                    match output {
                        TouchpadOutput::Scroll { dx, dy, phase } => {
                            // Window pixels to the host's, as the macOS Deck
                            // scales a trackpad's points.
                            let sink = &self.sink;
                            self.mouse.touchpad_scroll(
                                self.hwnd,
                                image,
                                (dx / scale, dy / scale),
                                phase,
                                |event| sink.input(event),
                            );
                        }
                        TouchpadOutput::Gesture(sample) => {
                            self.sink.input(InputEvent::Gesture(sample));
                        }
                    }
                }
                Some(LRESULT(0))
            }
            _ => None,
        }
    }

    fn handle_pen(&mut self, msg: u32, wparam: WPARAM) -> Option<LRESULT> {
        let image = self.image?;
        let epoch = self.epoch;
        let sink = &self.sink;
        self.pen
            .handle(self.hwnd, msg, wparam, image, epoch, |event| {
                sink.input(InputEvent::Pen(event));
            })
    }

    fn handle_mouse(&mut self, msg: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
        use windows::Win32::UI::WindowsAndMessaging::{
            WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEMOVE,
            WM_RBUTTONDOWN, WM_RBUTTONUP,
        };
        // A release always reaches the button its press went to; the
        // release of a click that gave the keyboard back goes nowhere,
        // wherever the pointer is now.
        let release = matches!(msg, WM_LBUTTONUP | WM_MBUTTONUP | WM_RBUTTONUP);
        if let Some((bit, false)) = ui_button(msg)
            && self.consumed_buttons & bit != 0
        {
            self.consumed_buttons &= !bit;
            if self.consumed_buttons == 0 {
                // SAFETY: gives up the capture the consumed press took.
                let _ = unsafe { windows::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture() };
            }
            return Some(LRESULT(0));
        }
        let image = match self.image {
            Some(image) => image,
            None if release => ImageRect {
                left: 0.0,
                top: 0.0,
                width: 0.0,
                height: 0.0,
            },
            None => return None,
        };
        if let Some((route, on_ui)) = self.overlay_route(msg, lparam) {
            if msg == WM_MOUSEMOVE {
                self.ui_pointer = on_ui;
            }
            if !route.host {
                if let Some((bit, pressed)) = ui_button(msg) {
                    if pressed {
                        self.ui_buttons |= bit;
                    } else {
                        self.ui_buttons &= !bit;
                    }
                }
                // The Deck's own UI (winit, egui) takes it.
                return None;
            }
            if route.ui {
                // The host takes it, and the UI sees where the pointer is.
                let _ = self.send_mouse(msg, wparam, lparam, image);
                return None;
            }
        }
        if let Some((bit, true)) = ui_button(msg)
            && matches!(msg, WM_LBUTTONDOWN | WM_MBUTTONDOWN | WM_RBUTTONDOWN)
            && mouse::Mouse::over(self.hwnd, image)
            && self.capture.click_in_picture() == ClickRoute::Consume
        {
            // A click into the picture gives the keyboard back to the host
            // and is not sent: it must not paint or press anything.
            self.consumed_buttons |= bit;
            // SAFETY: the window's own handle on its thread: its release
            // comes here even outside the window.
            let _ = unsafe { windows::Win32::UI::Input::KeyboardAndMouse::SetCapture(self.hwnd) };
            self.keyboard_changed(CaptureChange {
                owner: KeyboardOwner::Host,
                release_host_keys: false,
            });
            return Some(LRESULT(0));
        }
        self.send_mouse(msg, wparam, lparam, image)
    }

    /// A mouse message to the host (unless the pen owns the pointer).
    fn send_mouse(
        &mut self,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
        image: ImageRect,
    ) -> Option<LRESULT> {
        use windows::Win32::UI::WindowsAndMessaging::{WM_MBUTTONDBLCLK, WM_MOUSEFIRST};
        if (WM_MOUSEFIRST..=WM_MBUTTONDBLCLK).contains(&msg) {
            // While the pen is in range it owns the pointer, and a click its
            // tablet driver performs for a pen button is the pen's.
            let sink = &self.sink;
            if let Some(result) = self.pen.mouse_message(msg, self.epoch, |event| {
                sink.input(InputEvent::Pen(event));
            }) {
                return Some(result);
            }
        }
        let sink = &self.sink;
        let led = &mut self.led;
        self.mouse
            .handle(self.hwnd, msg, wparam, lparam, image, |event| {
                if matches!(
                    event,
                    InputEvent::PointerMove { .. } | InputEvent::PointerRelative { .. }
                ) && led.client_moved(Instant::now())
                {
                    sink.led_cursor(None);
                }
                sink.input(event);
            })
    }

    /// Where a pointer message goes while the Deck's UI takes part of the
    /// session window (`None`: no overlay, or not a routed message).
    /// With the route: whether the pointer is on the UI now.
    fn overlay_route(&self, msg: u32, lparam: LPARAM) -> Option<(PointerRoute, bool)> {
        use windows::Win32::UI::WindowsAndMessaging::{
            WM_LBUTTONDBLCLK, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDBLCLK, WM_MBUTTONDOWN,
            WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_RBUTTONDBLCLK,
            WM_RBUTTONDOWN, WM_RBUTTONUP,
        };
        if self.overlay == OverlayPointerRegions::default()
            && !self.ui_pointer
            && self.ui_buttons == 0
        {
            return None;
        }
        if self.mouse.locked() {
            return None;
        }
        let message = match msg {
            WM_MOUSEMOVE => PointerMessage::Move,
            WM_LBUTTONDOWN | WM_LBUTTONDBLCLK | WM_MBUTTONDOWN | WM_MBUTTONDBLCLK
            | WM_RBUTTONDOWN | WM_RBUTTONDBLCLK => PointerMessage::Press,
            WM_LBUTTONUP | WM_MBUTTONUP | WM_RBUTTONUP => PointerMessage::Release,
            WM_MOUSEWHEEL | WM_MOUSEHWHEEL => PointerMessage::Wheel,
            _ => return None,
        };
        let (x, y) = mouse::Mouse::message_point(self.hwnd, msg, lparam)?;
        // A click the pen took from its tablet driver is the host's too:
        // its release must reach the pen over the Deck's own UI.
        let held = if self.mouse.buttons_held() || self.pen.holds_driver_click() {
            Some(HeldBy::Host)
        } else if self.ui_buttons != 0 {
            Some(HeldBy::Ui)
        } else {
            None
        };
        let route = route_pointer(self.overlay, message, x, y, held, self.ui_pointer);
        Some((route, over_ui(self.overlay, x, y)))
    }

    /// Where this window and its picture are on the Deck's desktop, in
    /// screen pixels (`None` until the picture is placed): what a held
    /// drag that leaves another window is resolved against.
    #[must_use]
    pub fn screen_placement(&self) -> Option<(ImageRect, ImageRect)> {
        mouse::screen_placement(self.hwnd, self.image?)
    }

    fn pointer_over_image(&self) -> bool {
        self.active
            && self
                .image
                .is_some_and(|image| mouse::Mouse::over(self.hwnd, image))
    }

    /// Everything held on the host is released: keys (the Mac rule on
    /// focus loss), buttons, a pen in range, the pointer lock.
    fn release_all(&mut self, reason: &'static str) {
        self.end_composition();
        let edges = self.keyboard.release_all();
        self.send_keys(edges);
        let sink = &self.sink;
        self.mouse.release_buttons(|event| sink.input(event));
        self.ui_buttons = 0;
        self.consumed_buttons = 0;
        if let Some(event) = self.pen.leave(self.epoch) {
            self.sink.input(InputEvent::Pen(event));
        }
        if self.mouse.unlock() {
            self.sink.input(InputEvent::PointerLockReleased { reason });
        }
    }

    fn close_touchpad(&mut self) {
        if let Some(mut touchpad) = self.touchpad.take() {
            touchpad.close(self.hwnd);
        }
    }

    fn apply_grab(&mut self) {
        let grabbed = self.grab_active();
        let edges = self.keyboard.set_grab(grabbed);
        self.send_keys(edges);
        if self.active && self.keyboard_focus() && !self.keyboard_released() {
            hook::install(self.hwnd, grabbed, self.keyboard.held_keys());
        } else {
            hook::uninstall(self.hwnd);
        }
        self.apply_input_method();
    }

    /// Switches the window's input method on while the session takes
    /// committed text and off while it does not (Typing = Keys, or a host
    /// without `text_commit`): the macOS Deck's rule, so keys then reach the
    /// host by position instead of composing into nothing. Outside a session
    /// and while the connection bar has the keyboard, egui decides (winit
    /// keeps it off unless a text field is focused), so this hands it back
    /// switched off, as egui last set it. The window's real association is
    /// read each time: egui may have switched it behind this input's back.
    fn apply_input_method(&mut self) {
        use windows::Win32::UI::Input::Ime::{
            HIMC, IACE_CHILDREN, IACE_DEFAULT, ImmAssociateContextEx, ImmGetContext,
            ImmReleaseContext,
        };
        let owns = self.active && keys_to_host(self.overlay) && !self.keyboard_released();
        if !owns && !self.owns_input_method {
            return;
        }
        // SAFETY: the window's own handle on its thread; a context it
        // returns is released at once.
        let actual = unsafe {
            let context = ImmGetContext(self.hwnd);
            let on = !context.is_invalid();
            if on {
                let _ = ImmReleaseContext(self.hwnd, context);
            }
            on
        };
        let set = input_method_step(owns, self.keyboard.text_active(), actual);
        self.owns_input_method = owns;
        if set == Some(false) {
            self.end_composition();
        }
        if let Some(on) = set {
            // SAFETY: as above; a null context with IACE_DEFAULT restores the
            // window's default context, with IACE_CHILDREN removes it
            // (winit's `set_ime_allowed`).
            let flags = if on { IACE_DEFAULT } else { IACE_CHILDREN };
            let done = unsafe { ImmAssociateContextEx(self.hwnd, HIMC::default(), flags) };
            tracing::info!(
                target: arcen_deck_core::log_target::INPUT,
                on,
                ok = done.as_bool(),
                "ime: session window input method"
            );
        }
    }

    /// Cancels a composition in progress (focus loss, a reset, the
    /// connection bar, the input method switched off) and hides the Deck's
    /// line for it. The hiding is sent from here: the input method's own
    /// messages for the cancel arrive while this input is busy.
    fn end_composition(&mut self) {
        use windows::Win32::UI::Input::Ime::{
            CPS_CANCEL, ImmGetContext, ImmNotifyIME, ImmReleaseContext, NI_COMPOSITIONSTR,
        };
        if !self.keyboard.composing() {
            return;
        }
        // SAFETY: the window's own handle on its thread; the context is
        // released at once.
        unsafe {
            let context = ImmGetContext(self.hwnd);
            if !context.is_invalid() {
                let _ = ImmNotifyIME(context, NI_COMPOSITIONSTR, CPS_CANCEL, 0);
                let _ = ImmReleaseContext(self.hwnd, context);
            }
        }
        for event in self.keyboard.end_composition() {
            self.sink.input(event);
        }
    }

    fn send_keys(&self, edges: Vec<HostKeyEdge>) {
        if !edges.is_empty() {
            self.sink.input(InputEvent::Keys(edges));
        }
    }
}

/// Alt+F4 taken under a modal window (`WindowInput::modal_key`), held now
/// in this window.
#[derive(Debug, Clone, Copy, Default)]
struct ModalKeys {
    /// Alt+F4's F4.
    f4: bool,
    /// The Alt that came with it.
    alt_chord: bool,
}

thread_local! {
    /// The Esc and Enter that dismissed a modal window and are held now:
    /// one record for every session window (all are on the UI thread), so
    /// a key held while the keyboard moves to another session window stays
    /// the Deck's there.
    static DISMISS_HELD: std::cell::Cell<[bool; 2]> = const { std::cell::Cell::new([false; 2]) };
}

/// `DISMISS_HELD`'s slot for a Virtual-Key (`None`: no dismissing key).
fn dismiss_slot(vk: u16) -> Option<usize> {
    use windows::Win32::UI::Input::KeyboardAndMouse::{VK_ESCAPE, VK_RETURN};
    [VK_ESCAPE.0, VK_RETURN.0].iter().position(|key| *key == vk)
}

/// Whether `vk`, a dismissing key, is held for a modal window (`None`: not
/// a dismissing key).
fn dismiss_key_held(vk: u16) -> Option<bool> {
    let slot = dismiss_slot(vk)?;
    Some(DISMISS_HELD.with(|held| held.get()[slot]))
}

fn set_dismiss_key_held(vk: u16, on: bool) {
    if let Some(slot) = dismiss_slot(vk) {
        DISMISS_HELD.with(|held| {
            let mut keys = held.get();
            keys[slot] = on;
            held.set(keys);
        });
    }
}

/// The Virtual-Keys of the dismissing keys held now.
fn held_dismiss_keys() -> Vec<u16> {
    use windows::Win32::UI::Input::KeyboardAndMouse::{VK_ESCAPE, VK_RETURN};
    [VK_ESCAPE.0, VK_RETURN.0]
        .into_iter()
        .filter(|vk| dismiss_key_held(*vk) == Some(true))
        .collect()
}

/// A session window gained focus: a dismissing key let go outside the
/// Deck meanwhile is not held any more (the focus brings this thread's key
/// state up to date).
fn forget_released_dismiss_keys() {
    use windows::Win32::UI::Input::KeyboardAndMouse::GetKeyState;
    for vk in held_dismiss_keys() {
        // SAFETY: a plain read of this thread's key state.
        if unsafe { GetKeyState(i32::from(vk)) } >= 0 {
            set_dismiss_key_held(vk, false);
        }
    }
}

/// The UI-held button bit a button message is about, and whether it is a
/// press.
fn ui_button(msg: u32) -> Option<(u8, bool)> {
    use windows::Win32::UI::WindowsAndMessaging::{
        WM_LBUTTONDBLCLK, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDBLCLK, WM_MBUTTONDOWN,
        WM_MBUTTONUP, WM_RBUTTONDBLCLK, WM_RBUTTONDOWN, WM_RBUTTONUP,
    };
    Some(match msg {
        WM_LBUTTONDOWN | WM_LBUTTONDBLCLK => (1, true),
        WM_LBUTTONUP => (1, false),
        WM_MBUTTONDOWN | WM_MBUTTONDBLCLK => (2, true),
        WM_MBUTTONUP => (2, false),
        WM_RBUTTONDOWN | WM_RBUTTONDBLCLK => (4, true),
        WM_RBUTTONUP => (4, false),
        _ => return None,
    })
}

/// What to set the session window's input method to now (`None`: leave
/// it). `owns`: a session is on screen and the connection bar does not have
/// the keyboard (otherwise this input is handing it back); `composes`: the
/// session takes committed text; `actual`: the window has an input context
/// now. Handing it back switches it off when it is on, as egui last set it.
fn input_method_step(owns: bool, composes: bool, actual: bool) -> Option<bool> {
    if owns {
        (actual != composes).then_some(composes)
    } else {
        actual.then_some(false)
    }
}

#[cfg(test)]
mod input_method_tests {
    use super::input_method_step;

    #[test]
    fn the_input_method_follows_committed_text_and_is_handed_back_off() {
        // A session in Automatic typing: on (winit created the window
        // without one), then left alone.
        assert_eq!(input_method_step(true, true, false), Some(true));
        assert_eq!(input_method_step(true, true, true), None);
        // Typing = Keys: off, so keys reach the host by position.
        assert_eq!(input_method_step(true, false, true), Some(false));
        assert_eq!(input_method_step(true, false, false), None);
        // The connection bar takes the keyboard or the session ends: handed
        // back off (the caller asks only when this input owned it).
        assert_eq!(input_method_step(false, true, true), Some(false));
        assert_eq!(input_method_step(false, false, false), None);
    }
}

#[cfg(test)]
mod modal_tests {
    use std::sync::{Arc, Mutex};

    use arcen_deck_core::input::overlay_pointer::OverlayPointerRegions;
    use arcen_input::deck_pen::ImageRect;
    use arcen_input::keyboard::pc::KEY_F4;
    use arcen_input::keyboard::{DeckShortcut, KeyboardCapturePolicy, PcKeyboardSettings};
    use arcen_protocol::messages::HostPlatform;
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, WINDOW_EX_STYLE, WM_KEYDOWN, WM_KEYUP, WM_KILLFOCUS,
        WM_LBUTTONDOWN, WM_LBUTTONUP, WM_SETFOCUS, WM_SYSKEYDOWN, WM_SYSKEYUP, WS_POPUP,
    };
    use windows::core::w;

    use super::{HostInputFacts, InputEvent, InputSink, WindowInput};

    #[derive(Clone, Default)]
    struct Sent(Arc<Mutex<Vec<InputEvent>>>);

    impl InputSink for Sent {
        fn input(&self, event: InputEvent) {
            self.0.lock().unwrap().push(event);
        }
    }

    impl Sent {
        fn take(&self) -> Vec<InputEvent> {
            std::mem::take(&mut *self.0.lock().unwrap())
        }
    }

    fn at(x: i32, y: i32) -> LPARAM {
        LPARAM(((y << 16) | (x & 0xffff)) as isize)
    }

    /// A press and release of the left button at (200, 150), and Esc;
    /// whether the input kept each message from the UI.
    fn click_and_esc(input: &mut WindowInput) -> Vec<bool> {
        [
            (WM_LBUTTONDOWN, WPARAM(1), at(200, 150)),
            (WM_LBUTTONUP, WPARAM(0), at(200, 150)),
            (WM_KEYDOWN, WPARAM(0x1B), LPARAM(0x0001_0001)),
            (WM_KEYUP, WPARAM(0x1B), LPARAM(0xC001_0001_u32 as isize)),
        ]
        .into_iter()
        .map(|(msg, wparam, lparam)| input.handle(msg, wparam, lparam).is_some())
        .collect()
    }

    /// A session window's input on a hidden popup (destroyed when the
    /// returned guard drops), its picture filling the window.
    struct TestWindow(windows::Win32::Foundation::HWND);

    impl Drop for TestWindow {
        fn drop(&mut self) {
            // SAFETY: the window `session_input` created, on this thread.
            let _ = unsafe { DestroyWindow(self.0) };
        }
    }

    fn session_input() -> (TestWindow, WindowInput, Sent) {
        // SAFETY: a plain hidden popup of the system's static class, on this
        // thread, destroyed by the guard.
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("STATIC"),
                w!("modal test"),
                WS_POPUP,
                0,
                0,
                400,
                300,
                None,
                None,
                None,
                None,
            )
        }
        .expect("a test window");
        let sent = Sent::default();
        let mut input = WindowInput::new(hwnd, Box::new(sent.clone()), KeyboardCapturePolicy::Host);
        let facts = HostInputFacts {
            platform: HostPlatform::Windows,
            extended_keys: true,
            precise_scroll: true,
            text_commit: true,
            unicode_text: true,
        };
        input.set_session(Some((facts, PcKeyboardSettings::default())));
        input.set_image_rect(Some(ImageRect {
            left: 0.0,
            top: 0.0,
            width: 400.0,
            height: 300.0,
        }));
        let _ = sent.take();
        (TestWindow(hwnd), input, sent)
    }

    const MODAL: OverlayPointerRegions = OverlayPointerRegions {
        reveal: None,
        owned: None,
        whole_window: false,
        modal: true,
    };

    #[test]
    fn a_modal_help_window_gets_the_click_esc_and_alt_f4_and_the_host_gets_none() {
        let (_window, mut input, sent) = session_input();
        let button = |events: &[InputEvent]| {
            events
                .iter()
                .filter(|event| matches!(event, InputEvent::Button { .. }))
                .count()
        };
        // A windowed session without a Help window: the click is the host's.
        let kept = click_and_esc(&mut input);
        assert!(kept[0] && kept[1], "{kept:?}");
        assert_eq!(button(&sent.take()), 2);
        // About is open: the click on its OK button reaches the UI, Esc
        // closes it; nothing reaches the host.
        input.set_overlay_pointer(OverlayPointerRegions {
            modal: true,
            ..OverlayPointerRegions::default()
        });
        let _ = sent.take();
        assert_eq!(click_and_esc(&mut input), vec![false, false, true, true]);
        assert_eq!(sent.take(), vec![InputEvent::DismissModal]);
        // Alt+F4 there is the Deck's quit chord, never the default
        // procedure's close: taken (with its repeat and releases), the quit
        // hold told once, F4 held until let go.
        let alt_f4 = LPARAM(0x203E_0001);
        for (msg, wparam, lparam) in [
            (WM_SYSKEYDOWN, WPARAM(0x73), alt_f4),
            (WM_SYSKEYDOWN, WPARAM(0x73), LPARAM(0x603E_0001)),
        ] {
            assert!(input.handle(msg, wparam, lparam).is_some());
        }
        assert_eq!(sent.take(), vec![InputEvent::Shortcut(DeckShortcut::Quit)]);
        assert!(input.holds_deck_key(KEY_F4));
        assert!(
            input
                .handle(WM_SYSKEYUP, WPARAM(0x73), LPARAM(0xE03E_0001_u32 as isize))
                .is_some()
        );
        assert!(!input.holds_deck_key(KEY_F4));
        assert!(
            input
                .handle(WM_KEYUP, WPARAM(0x12), LPARAM(0xC038_0001_u32 as isize))
                .is_some()
        );
        assert_eq!(sent.take(), Vec::new());
        // Held while the window closes (a quick Alt+F4 in another session
        // window closed it): its repeats and release stay the Deck's, never
        // a fresh F4 the host would keep down.
        assert!(input.handle(WM_SYSKEYDOWN, WPARAM(0x73), alt_f4).is_some());
        input.set_overlay_pointer(OverlayPointerRegions::default());
        let _ = sent.take();
        assert!(
            input
                .handle(WM_SYSKEYDOWN, WPARAM(0x73), LPARAM(0x603E_0001))
                .is_some()
        );
        assert!(input.holds_deck_key(KEY_F4));
        assert!(
            input
                .handle(WM_SYSKEYUP, WPARAM(0x73), LPARAM(0xE03E_0001_u32 as isize))
                .is_some()
        );
        assert!(!input.holds_deck_key(KEY_F4));
        assert_eq!(sent.take(), Vec::new());
        // Closed: the host has the click again.
        input.set_overlay_pointer(OverlayPointerRegions::default());
        let _ = sent.take();
        let kept = click_and_esc(&mut input);
        assert!(kept[0] && kept[1], "{kept:?}");
        assert_eq!(button(&sent.take()), 2);
        input.set_session(None);
    }

    #[test]
    fn esc_or_enter_dismisses_help_and_keeps_its_repeats_and_release_from_the_host() {
        use windows::Win32::UI::WindowsAndMessaging::WM_CHAR;
        const KEY_RETURN: u32 = 0x0100_0004;
        let (_window, mut input, sent) = session_input();
        // The window has the keyboard, as in a session in front.
        let _ = input.handle(WM_SETFOCUS, WPARAM(0), LPARAM(0));
        input.set_overlay_pointer(MODAL);
        let _ = sent.take();
        let enter_down = LPARAM(0x001C_0001);
        let enter_repeat = LPARAM(0x401C_0001);
        let enter_up = LPARAM(0xC01C_0001_u32 as isize);
        // Enter under About: taken, and About is told to close.
        assert!(input.handle(WM_KEYDOWN, WPARAM(0x0D), enter_down).is_some());
        assert_eq!(sent.take(), vec![InputEvent::DismissModal]);
        // It closed; Enter is still held: its repeats, their carriage
        // returns and its release are the Deck's, never a fresh Enter.
        input.set_overlay_pointer(OverlayPointerRegions::default());
        let _ = sent.take();
        for (msg, wparam, lparam) in [
            (WM_KEYDOWN, WPARAM(0x0D), enter_repeat),
            (WM_CHAR, WPARAM(0x0D), enter_repeat),
            (WM_KEYDOWN, WPARAM(0x0D), enter_repeat),
            (WM_KEYUP, WPARAM(0x0D), enter_up),
        ] {
            assert!(input.handle(msg, wparam, lparam).is_some(), "{msg:#x}");
        }
        assert_eq!(sent.take(), Vec::new());
        // Held on with Alt pressed meanwhile (the host's now: the test
        // window's keyboard would read its focus as lost, so Alt is not sent
        // here): its repeats and release come as system keys, and are still
        // the Deck's.
        input.set_overlay_pointer(MODAL);
        assert!(input.handle(WM_KEYDOWN, WPARAM(0x0D), enter_down).is_some());
        input.set_overlay_pointer(OverlayPointerRegions::default());
        let _ = sent.take();
        for (msg, lparam) in [
            (WM_SYSKEYDOWN, LPARAM(0x601C_0001)),
            (WM_SYSKEYUP, LPARAM(0xE01C_0001_u32 as isize)),
        ] {
            assert!(
                input.handle(msg, WPARAM(0x0D), lparam).is_some(),
                "{msg:#x}"
            );
        }
        let enter_edges = sent
            .take()
            .into_iter()
            .filter_map(|event| match event {
                InputEvent::Keys(edges) => Some(edges),
                _ => None,
            })
            .flatten()
            .filter(|edge| edge.qt_key == KEY_RETURN)
            .count();
        assert_eq!(enter_edges, 0);
        // A new press after the release is the host's.
        let _ = input.handle(WM_KEYDOWN, WPARAM(0x0D), enter_down);
        let _ = input.handle(WM_KEYUP, WPARAM(0x0D), enter_up);
        let edges: Vec<_> = sent
            .take()
            .into_iter()
            .filter_map(|event| match event {
                InputEvent::Keys(edges) => Some(edges),
                _ => None,
            })
            .flatten()
            .collect();
        assert!(
            edges
                .iter()
                .any(|edge| edge.qt_key == KEY_RETURN && edge.pressed),
            "{edges:?}"
        );
        input.set_session(None);
        // Esc the same way, in a session of its own.
        let (_window, mut input, sent) = session_input();
        let _ = input.handle(WM_SETFOCUS, WPARAM(0), LPARAM(0));
        input.set_overlay_pointer(MODAL);
        let _ = sent.take();
        assert!(
            input
                .handle(WM_KEYDOWN, WPARAM(0x1B), LPARAM(0x0001_0001))
                .is_some()
        );
        assert_eq!(sent.take(), vec![InputEvent::DismissModal]);
        input.set_overlay_pointer(OverlayPointerRegions::default());
        let _ = sent.take();
        assert!(
            input
                .handle(WM_KEYDOWN, WPARAM(0x1B), LPARAM(0x4001_0001))
                .is_some()
        );
        // Its repeat and release taken by the hook (Alt+Esc) and posted.
        for flags in [0_usize, 0x80] {
            let packed = WPARAM(0x1B | (flags << 16));
            assert!(
                input
                    .handle(super::hook::WM_HOOK_KEY, packed, LPARAM(0x01))
                    .is_some()
            );
        }
        assert_eq!(sent.take(), Vec::new());
        // Released: a new Esc is the host's.
        let _ = input.handle(WM_KEYDOWN, WPARAM(0x1B), LPARAM(0x0001_0001));
        let _ = input.handle(WM_KEYUP, WPARAM(0x1B), LPARAM(0xC001_0001_u32 as isize));
        let host = sent.take();
        assert!(
            host.iter().any(|event| matches!(
                event,
                InputEvent::Keys(edges)
                    if edges.iter().any(|edge| edge.qt_key == 0x0100_0000 && edge.pressed)
            )),
            "{host:?}"
        );
        // Focus lost while Esc is still held for Help, seen without a
        // WM_KILLFOCUS (the focus read after a key): it is let go elsewhere,
        // and the next Esc in the window is the host's again.
        let _ = input.handle(WM_SETFOCUS, WPARAM(0), LPARAM(0));
        input.set_overlay_pointer(MODAL);
        assert!(
            input
                .handle(WM_KEYDOWN, WPARAM(0x1B), LPARAM(0x0001_0001))
                .is_some()
        );
        input.set_overlay_pointer(OverlayPointerRegions::default());
        // The hidden test window never has the system's focus.
        input.sync_focus();
        let _ = input.handle(WM_SETFOCUS, WPARAM(0), LPARAM(0));
        let _ = sent.take();
        let _ = input.handle(WM_KEYDOWN, WPARAM(0x1B), LPARAM(0x0001_0001));
        let host = sent.take();
        assert!(
            host.iter().any(|event| matches!(
                event,
                InputEvent::Keys(edges)
                    if edges.iter().any(|edge| edge.qt_key == 0x0100_0000 && edge.pressed)
            )),
            "{host:?}"
        );
        input.set_session(None);
    }

    #[test]
    fn a_held_dismiss_key_drained_before_a_hooked_key_stays_the_decks() {
        use windows::Win32::System::SystemInformation::GetTickCount;
        use windows::Win32::UI::WindowsAndMessaging::{
            MSG, PM_NOREMOVE, PeekMessageW, PostMessageW, WM_KEYFIRST, WM_KEYLAST,
        };
        const KEY_RETURN: u32 = 0x0100_0004;
        let (window, mut input, sent) = session_input();
        let _ = input.handle(WM_SETFOCUS, WPARAM(0), LPARAM(0));
        input.set_overlay_pointer(MODAL);
        assert!(
            input
                .handle(WM_KEYDOWN, WPARAM(0x0D), LPARAM(0x001C_0001))
                .is_some()
        );
        input.set_overlay_pointer(OverlayPointerRegions::default());
        let _ = sent.take();
        // Enter's repeat and release are still queued when the hook posts a
        // key it took (the Windows key): the keyboard drains them first.
        // SAFETY: posting to the test window on this thread.
        unsafe {
            PostMessageW(window.0, WM_KEYDOWN, WPARAM(0x0D), LPARAM(0x401C_0001)).unwrap();
            PostMessageW(
                window.0,
                WM_KEYUP,
                WPARAM(0x0D),
                LPARAM(0xC01C_0001_u32 as isize),
            )
            .unwrap();
        }
        // SAFETY: a plain read of the tick count.
        let later = u64::from(unsafe { GetTickCount() }.wrapping_add(1000));
        #[allow(clippy::cast_possible_wrap, reason = "packed like the hook packs it")]
        let packed = LPARAM(((later << 32) | 0x5B) as isize);
        let _ = input.handle(
            super::hook::WM_HOOK_KEY,
            WPARAM(0x5B | (0x01 << 16)),
            packed,
        );
        let mut queued = MSG::default();
        // SAFETY: a valid out-parameter; nothing is removed.
        let left = unsafe {
            PeekMessageW(
                &raw mut queued,
                window.0,
                WM_KEYFIRST,
                WM_KEYLAST,
                PM_NOREMOVE,
            )
        }
        .as_bool();
        assert!(!left, "drained: {:#x}", queued.message);
        let host = sent.take();
        assert!(
            !host.iter().any(|event| matches!(
                event,
                InputEvent::Keys(edges) if edges.iter().any(|edge| edge.qt_key == KEY_RETURN)
            )),
            "{host:?}"
        );
        input.set_session(None);
    }

    /// Sets this thread's key state for Alt (what `GetKeyState` reads when
    /// the keyboard reseeds held modifiers).
    fn hold_alt(held: bool) {
        hold_keys(&[0x12, 0xA4], held);
    }

    /// Sets this thread's key state for `keys` (what `GetKeyState` reads).
    fn hold_keys(keys: &[usize], held: bool) {
        use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyboardState, SetKeyboardState};
        let mut state = [0_u8; 256];
        // SAFETY: this thread's own key state, read and written whole.
        unsafe {
            let _ = GetKeyboardState(&mut state);
            for vk in keys {
                state[*vk] = if held { 0x80 } else { 0 };
            }
            let _ = SetKeyboardState(&state);
        }
    }

    /// The host's Return edges among `events`.
    fn return_edges(events: Vec<InputEvent>) -> Vec<bool> {
        events
            .into_iter()
            .filter_map(|event| match event {
                InputEvent::Keys(edges) => Some(edges),
                _ => None,
            })
            .flatten()
            .filter(|edge| edge.qt_key == 0x0100_0004)
            .map(|edge| edge.pressed)
            .collect()
    }

    #[test]
    fn enter_held_from_help_into_another_session_window_stays_the_decks_until_let_go() {
        let enter_down = LPARAM(0x001C_0001);
        let enter_repeat = LPARAM(0x401C_0001);
        let enter_up = LPARAM(0xC01C_0001_u32 as isize);
        // Two session windows of one session, on this thread (Match My
        // Layout's root and a secondary window).
        let (_root_window, mut root, root_sent) = session_input();
        let (_other_window, mut other, other_sent) = session_input();
        let _ = root.handle(WM_SETFOCUS, WPARAM(0), LPARAM(0));
        root.set_overlay_pointer(MODAL);
        other.set_overlay_pointer(MODAL);
        // Enter dismisses About in the root window, and is held on.
        hold_keys(&[0x0D], true);
        assert!(root.handle(WM_KEYDOWN, WPARAM(0x0D), enter_down).is_some());
        assert_eq!(root_sent.take(), vec![InputEvent::DismissModal]);
        root.set_overlay_pointer(OverlayPointerRegions::default());
        other.set_overlay_pointer(OverlayPointerRegions::default());
        // The keyboard moves to the other window, Enter still down: its
        // repeats and release there are the Deck's too.
        let _ = root.handle(WM_KILLFOCUS, WPARAM(0), LPARAM(0));
        let _ = other.handle(WM_SETFOCUS, WPARAM(0), LPARAM(0));
        let _ = other_sent.take();
        assert!(
            other
                .handle(WM_KEYDOWN, WPARAM(0x0D), enter_repeat)
                .is_some()
        );
        assert!(other.handle(WM_KEYUP, WPARAM(0x0D), enter_up).is_some());
        hold_keys(&[0x0D], false);
        assert_eq!(return_edges(other_sent.take()), Vec::<bool>::new());
        // Pressed again after the release: the host's.
        let _ = other.handle(WM_SETFOCUS, WPARAM(0), LPARAM(0));
        let _ = other.handle(WM_KEYDOWN, WPARAM(0x0D), enter_down);
        assert!(return_edges(other_sent.take()).contains(&true));
        root.set_session(None);
        other.set_session(None);
    }

    #[test]
    fn enter_let_go_outside_the_deck_after_dismissing_help_is_the_hosts_when_pressed_again() {
        let enter_down = LPARAM(0x001C_0001);
        let enter_up = LPARAM(0xC01C_0001_u32 as isize);
        let (_window, mut input, sent) = session_input();
        // Twice: the focus comes back with Enter seen up (it was let go in
        // another program), and with Enter still read as down (a key state
        // not yet brought up to date): either way the next press, no
        // repeat, is the host's.
        for stale in [false, true] {
            let _ = input.handle(WM_SETFOCUS, WPARAM(0), LPARAM(0));
            input.set_overlay_pointer(MODAL);
            hold_keys(&[0x0D], true);
            assert!(input.handle(WM_KEYDOWN, WPARAM(0x0D), enter_down).is_some());
            input.set_overlay_pointer(OverlayPointerRegions::default());
            let _ = input.handle(WM_KILLFOCUS, WPARAM(0), LPARAM(0));
            hold_keys(&[0x0D], stale);
            let _ = input.handle(WM_SETFOCUS, WPARAM(0), LPARAM(0));
            hold_keys(&[0x0D], false);
            let _ = sent.take();
            let _ = input.handle(WM_KEYDOWN, WPARAM(0x0D), enter_down);
            let _ = input.handle(WM_KEYUP, WPARAM(0x0D), enter_up);
            assert!(return_edges(sent.take()).contains(&true), "stale {stale}");
        }
        input.set_session(None);
    }

    #[test]
    fn alt_let_go_after_a_quick_alt_f4_closed_help_never_reaches_the_host() {
        use arcen_protocol::keys::{KEY_ALT, MOD_ALT};
        let (_window, mut input, sent) = session_input();
        // The window has the keyboard, as in a session in front.
        let _ = input.handle(WM_SETFOCUS, WPARAM(0), LPARAM(0));
        input.set_overlay_pointer(MODAL);
        let _ = sent.take();
        // Alt, then F4, under About; F4 let go first closes it ...
        hold_alt(true);
        let _ = input.handle(WM_SYSKEYDOWN, WPARAM(0x12), LPARAM(0x2038_0001));
        assert!(
            input
                .handle(WM_SYSKEYDOWN, WPARAM(0x73), LPARAM(0x203E_0001))
                .is_some()
        );
        assert!(
            input
                .handle(WM_SYSKEYUP, WPARAM(0x73), LPARAM(0xE03E_0001_u32 as isize))
                .is_some()
        );
        input.set_overlay_pointer(OverlayPointerRegions::default());
        // ... and Alt is let go only then.
        let _ = input.handle(WM_SYSKEYUP, WPARAM(0x12), LPARAM(0xC038_0001_u32 as isize));
        hold_alt(false);
        // The next letter reaches the host as a plain letter.
        let _ = sent.take();
        let _ = input.handle(WM_KEYDOWN, WPARAM(0x41), LPARAM(0x001E_0001));
        let _ = input.handle(WM_KEYUP, WPARAM(0x41), LPARAM(0xC01E_0001_u32 as isize));
        let edges: Vec<_> = sent
            .take()
            .into_iter()
            .filter_map(|event| match event {
                InputEvent::Keys(edges) => Some(edges),
                _ => None,
            })
            .flatten()
            .collect();
        assert!(
            edges.iter().any(|edge| edge.qt_key == 0x41 && edge.pressed),
            "{edges:?}"
        );
        assert!(
            edges
                .iter()
                .all(|edge| edge.qt_key != KEY_ALT && edge.modifiers & MOD_ALT == 0),
            "{edges:?}"
        );
        input.set_session(None);
    }
}
