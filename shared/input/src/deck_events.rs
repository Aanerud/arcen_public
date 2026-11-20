// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! What a Deck's input adapter hands its session, in delivery order.
//!
//! A platform adapter reads its window system (Win32 messages on Windows)
//! through the shared keyboard, text, wheel and pen policies and emits
//! [`DeckInputEvent`]s to one [`DeckInputSink`] per session window, so
//! per-monitor routing is the sink's: each viewport's sink knows its
//! monitor. The session controller, not the adapter, turns events into wire
//! messages.

use arcen_protocol::cursor_position::CursorPosition;
use arcen_protocol::messages::HostPlatform;

use crate::PenEvent;
use crate::deck_wheel::WheelScroll;
use crate::gestures::GestureSample;
use crate::host_pointer::LedPointerMark;
use crate::keyboard::{DeckShortcut, HostKeyEdge};

/// What the session sends, in the order the system delivered it.
#[derive(Debug, Clone, PartialEq)]
pub enum DeckInputEvent {
    /// Host key edges (`key_event`).
    Keys(Vec<HostKeyEdge>),
    /// Release everything and reset the host's modifiers
    /// (`key_reset_modifiers`), after the releases already sent.
    Reset {
        /// Why.
        reason: &'static str,
    },
    /// Committed text (`text_commit`).
    Text(String),
    /// The input method's composition to show over the video; `None` hides
    /// it.
    Preedit(Option<String>),
    /// A chord the Deck keeps (Disconnect, Paste as Keystrokes, …).
    Shortcut(DeckShortcut),
    /// Esc or Enter pressed in a session window while a modal window of
    /// the Deck has the keyboard: the Deck closes the one in front. The key,
    /// its repeats and its release stay the Deck's.
    DismissModal,
    /// The pointer moved to a normalized position in the image.
    PointerMove {
        /// `0..=1` across the image.
        x: f64,
        /// `0..=1` down the image.
        y: f64,
    },
    /// Relative motion while the pointer is locked, in Deck pixels.
    PointerRelative {
        /// Right.
        dx: i32,
        /// Down.
        dy: i32,
    },
    /// A button edge at a normalized position (1 left, 2 middle, 3 right).
    Button {
        /// Wire button.
        button: u8,
        /// Pressed or released.
        pressed: bool,
        /// `0..=1` across the image.
        x: f64,
        /// `0..=1` down the image.
        y: f64,
    },
    /// A wheel scroll at a normalized position.
    Scroll {
        /// The deltas and their unit.
        scroll: WheelScroll,
        /// `0..=1` across the image.
        x: f64,
        /// `0..=1` down the image.
        y: f64,
    },
    /// A typed pen sample (Tablet support).
    Pen(PenEvent),
    /// A trackpad gesture step (pinch), sent only to a host that advertised
    /// gestures, as the macOS Deck does.
    Gesture(GestureSample),
    /// The pointer lock ended (F9, focus loss, a click outside), with why.
    PointerLockReleased {
        /// Why, as the macOS Deck logs it (`panic_f9`, `focus_out`).
        reason: &'static str,
    },
}

/// What the host said in its hello, for the keyboard policy and the wheel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent capability facts from one hello"
)]
pub struct HostInputFacts {
    /// The host's platform.
    pub platform: HostPlatform,
    /// `input_capabilities.extended_keys`.
    pub extended_keys: bool,
    /// `input_capabilities.precise_scroll`.
    pub precise_scroll: bool,
    /// The host types `text_commit`.
    pub text_commit: bool,
    /// The host types characters beyond its layout.
    pub unicode_text: bool,
}

/// Where the session's input goes. Called on the window's thread, from its
/// window procedure: send without blocking.
pub trait DeckInputSink {
    /// One event.
    fn input(&self, event: DeckInputEvent);

    /// What to draw over the video for a host-led pointer (the host's image,
    /// the arrow, or the pen dot at `position`), or `None` to hide it while
    /// the Deck's own pointer leads.
    fn led_cursor(&self, _led: Option<LedCursor>) {}
}

/// What the shell draws over the video while the host leads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LedCursor {
    /// The host's cursor image, the arrow, or the pen dot.
    pub mark: LedPointerMark,
    /// The position, in the host's stream pixels (the presenter maps it into
    /// the window with its video fit).
    pub position: CursorPosition,
}
