// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Input capture (owner: `input` stream).
//!
//! The session window's procedure hands every message to [`WindowInput`]
//! first ([`attach`] subclasses the window), before winit and egui see it.
//! Keys, the mouse and the pen leave from here the moment Windows delivers
//! them, as [`InputEvent`]s to the session's [`InputSink`]; nothing waits
//! for a UI pass.
//!
//! Everything this module decides is shared: the PC keyboard policy, the
//! Deck's chords and the keyboard grab's routing
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
    HeldBy, OverlayPointerRegions, PointerMessage, PointerRoute, over_ui, route_pointer,
};
use arcen_input::deck_pen::ImageRect;
use arcen_input::keyboard::{HostKeyEdge, KeyboardGrab, PcKeyboardSettings};
use arcen_input::text::{HostTextTruth, TypingMode};
use arcen_input::touchpad::TouchpadOutput;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};

pub use arcen_input::deck_events::{
    DeckInputEvent as InputEvent, DeckInputSink as InputSink, HostInputFacts, LedCursor,
};
pub use cursor::HostCursorImage;
pub use hook::LAB_KEY_MARK;
pub use subclass::{attach, detach, with_input};

/// The session window's input: one per window, owned by its subclass.
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent window facts: active, focused, fullscreen, bridged pen"
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
    fullscreen: bool,
    grab: KeyboardGrab,
    grab_toggle: Option<bool>,
    /// The connection bar has the keyboard (Ctrl+Alt+Home).
    connection_bar: bool,
    /// Where the Deck's own UI takes the pointer over the session (the
    /// fullscreen connection bar), published by the shell every pass.
    overlay: OverlayPointerRegions,
    /// The last pointer move was on the Deck's UI.
    ui_pointer: bool,
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
    pub fn new(hwnd: HWND, sink: Box<dyn InputSink>, grab: KeyboardGrab) -> Self {
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
            fullscreen: false,
            grab,
            grab_toggle: None,
            connection_bar: false,
            overlay: OverlayPointerRegions::default(),
            ui_pointer: false,
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
            if focused {
                self.keyboard.focus_gained();
            } else {
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
            self.grab_toggle = None;
            self.connection_bar = false;
        }
        self.apply_grab();
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

    /// The window entered or left fullscreen (the default grab follows it).
    pub fn set_fullscreen(&mut self, fullscreen: bool) {
        self.fullscreen = fullscreen;
        self.apply_grab();
    }

    /// The Keyboard Grab menu item: turns the grab the other way for this
    /// session.
    pub fn toggle_grab(&mut self) {
        self.grab_toggle = Some(!self.grab_active());
        self.apply_grab();
    }

    /// Whether the session takes the system shortcuts now.
    #[must_use]
    pub fn grab_active(&self) -> bool {
        self.active
            && !self.connection_bar
            && self
                .grab
                .active(self.focused, self.fullscreen, self.grab_toggle)
    }

    /// The connection bar took the keyboard or gave it back: the shell calls
    /// this when it shows the bar for [`arcen_input::keyboard::DeckShortcut::ShowConnectionBar`]
    /// (Ctrl+Alt+Home) and when it hides it. While the bar has it, keys
    /// reach the Deck, not the host, and the keyboard grab lets go; a click
    /// into the picture gives the keyboard back too.
    pub fn set_connection_bar(&mut self, shown: bool) {
        if shown == self.connection_bar {
            return;
        }
        self.connection_bar = shown;
        if shown {
            self.end_composition();
            // The chord's own Control and Alt went down on the host.
            let edges = self.keyboard.release_all();
            self.send_keys(edges);
        } else {
            self.keyboard.focus_gained();
        }
        self.apply_grab();
    }

    /// Whether the connection bar has the keyboard.
    #[must_use]
    pub const fn connection_bar(&self) -> bool {
        self.connection_bar
    }

    /// Where the Deck's own UI takes the pointer over the session now
    /// (`arcen_deck_core::input::overlay_pointer`): the shell publishes the
    /// fullscreen top-edge strip and the open connection bar every pass.
    pub fn set_overlay_pointer(&mut self, regions: OverlayPointerRegions) {
        self.overlay = regions;
        if regions == OverlayPointerRegions::default() {
            self.ui_pointer = false;
        }
    }

    /// Locks the pointer (relative motion) or releases it.
    pub fn set_pointer_lock(&mut self, locked: bool) {
        if locked {
            if self.active && self.focused {
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
    }

    /// Releases every key and button held on the host (Release Modifiers).
    pub fn release_modifiers(&mut self) {
        let edges = self.keyboard.release_all();
        self.send_keys(edges);
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
                self.keyboard.focus_gained();
                self.apply_grab();
                return None;
            }
            WM_KILLFOCUS => {
                self.focused = false;
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
        if matches!(msg, WM_KEYDOWN | WM_SYSKEYDOWN) {
            // egui may have switched the input method since the last change
            // this input saw (a text field in the connection bar).
            self.apply_input_method();
        }
        if !self.connection_bar
            && let Some(result) = self.handle_keyboard(msg, wparam, lparam)
        {
            return Some(result);
        }
        if let Some(result) = self.handle_touchpad(msg, wparam) {
            return Some(result);
        }
        if let Some(result) = self.handle_pen(msg, wparam) {
            return Some(result);
        }
        self.handle_mouse(msg, wparam, lparam)
    }

    fn handle_keyboard(&mut self, msg: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
        let outcome = self.keyboard.handle(self.hwnd, msg, wparam, lparam)?;
        for action in outcome.events {
            if let InputEvent::Reset { reason } = action {
                self.release_all(reason);
                self.sink.input(InputEvent::Reset { reason });
            } else {
                self.sink.input(action);
            }
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
        // A release always reaches the button its press went to.
        let release = matches!(msg, WM_LBUTTONUP | WM_MBUTTONUP | WM_RBUTTONUP);
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
        if self.connection_bar
            && matches!(msg, WM_LBUTTONDOWN | WM_MBUTTONDOWN | WM_RBUTTONDOWN)
            && mouse::Mouse::over(self.hwnd, image)
        {
            // A click into the picture gives the keyboard back to the host.
            self.set_connection_bar(false);
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
        use windows::Win32::UI::WindowsAndMessaging::{
            WM_LBUTTONUP, WM_MBUTTONUP, WM_MOUSEFIRST, WM_MOUSEWHEEL, WM_RBUTTONUP,
        };
        let release = matches!(msg, WM_LBUTTONUP | WM_MBUTTONUP | WM_RBUTTONUP);
        if self.pen.in_proximity() && !release && (WM_MOUSEFIRST..WM_MOUSEWHEEL).contains(&msg) {
            // The pen owns the pointer while it is in range.
            return Some(LRESULT(0));
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
        let held = if self.mouse.buttons_held() {
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
        if self.active && self.focused && !self.connection_bar {
            hook::install(self.hwnd, grabbed, self.keyboard.held_keys());
        } else {
            hook::uninstall();
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
        let owns = self.active && !self.connection_bar;
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
