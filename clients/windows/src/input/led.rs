// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The host-led cursor: where the host's pointer is, when it, not the
//! Deck's own mouse, moved last (a Native-tablet pen, a host application
//! warping the pointer).
//!
//! The decisions are shared (`arcen_input::host_pointer`: last mover wins,
//! [`led_pointer_mark`]); this keeps them per window and tells the shell
//! what to draw in present's `CursorLayer`. The Deck never warps its own
//! pointer (ADR 0014 Amendment 21): while the host leads, the system cursor
//! is hidden over the image and the mark is drawn at the host's position.

use std::time::Instant;

use arcen_input::cursor::DeckCursor;
use arcen_input::deck_events::LedCursor;
use arcen_input::host_pointer::{
    DeckHostPointer, LedPointerMark, PointerAuthority, led_pointer_mark,
};
use arcen_protocol::cursor_position::CursorPosition;

#[derive(Debug, Default)]
pub(super) struct Led {
    records: DeckHostPointer,
    authority: PointerAuthority,
    shown: Option<DeckCursor>,
    bridged_pen: bool,
    /// The sequence of the last position the session handed over.
    last_reported: Option<u64>,
}

impl Led {
    /// A `CursorPosition` record; whether what the shell draws changed.
    pub(super) fn record(&mut self, bytes: &[u8], at: Instant) -> bool {
        match self.records.apply(bytes) {
            Ok(Some(position)) => self.reported(position, at),
            Ok(None) | Err(_) => false,
        }
    }

    /// The host's pointer moved (a record already applied by the session);
    /// whether what the shell draws changed.
    /// A position at or before the last one is not news (the shell may hand
    /// the same value again), so it never takes the lead back.
    pub(super) fn reported(&mut self, position: CursorPosition, at: Instant) -> bool {
        if self
            .last_reported
            .is_some_and(|last| position.sequence <= last)
        {
            return false;
        }
        self.last_reported = Some(position.sequence);
        self.authority.host_reported(position, at)
    }

    /// The Deck's own mouse moved: it leads again. Whether that changed
    /// what is drawn.
    pub(super) fn client_moved(&mut self, at: Instant) -> bool {
        let before = self.authority.host_position();
        self.authority.client_moved(at);
        before.is_some()
    }

    pub(super) fn set_shown(&mut self, shown: Option<DeckCursor>, bridged_pen: bool) {
        self.shown = shown;
        self.bridged_pen = bridged_pen;
    }

    /// What to draw now; `None` while the Deck's own pointer leads.
    pub(super) fn current(&self) -> Option<LedCursor> {
        let position = self.authority.host_position()?;
        let shown = self.shown.clone().unwrap_or(DeckCursor::Default);
        let mark = led_pointer_mark(&shown, self.bridged_pen);
        (mark != LedPointerMark::Nothing).then_some(LedCursor { mark, position })
    }

    /// Whether the host leads (the system cursor hides over the image).
    pub(super) fn host_leads(&self) -> bool {
        self.authority.host_position().is_some()
    }

    /// A new session or lane.
    pub(super) fn reset(&mut self) {
        self.records.reset_lane();
        self.authority.reset();
        self.last_reported = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(sequence: u64) -> CursorPosition {
        CursorPosition {
            sequence,
            on_stream: true,
            output: 0,
            generation: 1,
            monitor_id: 0,
            x: 10,
            y: 20,
            space_width: 100,
            space_height: 100,
        }
    }

    #[test]
    fn a_position_handed_over_again_never_takes_the_lead_back() {
        let mut led = Led::default();
        let start = Instant::now();
        assert!(led.reported(at(1), start));
        assert!(led.client_moved(start));
        let rested = start + std::time::Duration::from_secs(1);
        assert!(!led.reported(at(1), rested), "the same record again");
        assert!(!led.host_leads());
        assert!(led.reported(at(2), rested), "a newer one does");
    }
}
