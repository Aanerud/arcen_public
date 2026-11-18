// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! A single-stream session's frame rate follows the Deck display that shows
//! it (`AuthResponse::display_refresh_v1`, stated at connect). When the
//! window moves to another display, or that display's refresh changes, the
//! Deck re-reports it with a `display_refresh_update`, once the window has
//! stayed on the new display for [`SETTLE`]: a window dragged across a
//! display on its way elsewhere reports nothing.
//!
//! Every Deck shell feeds [`StreamDisplayFollower::observe`] the displays it
//! reads and the display its session window is on; this decides when to
//! report and what.

use std::time::{Duration, Instant};

use arcen_protocol::messages::{
    ClientMonitor, DeckDisplayRefreshMsg, DisplayRefreshUpdateMsg, ServerHelloMsg,
};

/// How long the window must stay on a display before it is reported.
pub const SETTLE: Duration = Duration::from_millis(500);

/// Whether a session's host follows a window to another display: it said so
/// (`StreamRateMsg::follows_display_moves`) and the session is one stream.
#[must_use]
pub fn host_follows_display_moves(hello: &ServerHelloMsg, multi_monitor: bool) -> bool {
    !multi_monitor
        && hello
            .stream_rate
            .as_ref()
            .is_some_and(|rate| rate.follows_display_moves)
}

/// What the host was last told: the display that shows the session and its
/// refresh.
type Reported = (Option<u32>, Option<u32>);

/// Decides when a Deck re-reports the display that shows its session.
#[derive(Debug, Clone, Default)]
pub struct StreamDisplayFollower {
    observed: bool,
    sequence: u64,
    reported: Option<Reported>,
    candidate: Option<(Reported, Instant)>,
}

impl StreamDisplayFollower {
    /// A follower for a session that stated `connected` at connect.
    #[must_use]
    pub fn new(connected: Option<&DeckDisplayRefreshMsg>) -> Self {
        Self {
            observed: false,
            sequence: 0,
            reported: connected
                .map(|stated| (stated.stream_display_id, stated.stream_refresh_hz())),
            candidate: None,
        }
    }

    /// Whether [`Self::observe`] has run: a shell that reads the displays
    /// only when its window moves looks once at the start too, because a
    /// Deck that stated no window display (a windowed session on Windows)
    /// left the host on its primary.
    #[must_use]
    pub fn has_observed(&self) -> bool {
        self.observed
    }

    /// Whether the window is on a display the host has not been told about
    /// yet: the shell should look again within [`SETTLE`].
    #[must_use]
    pub fn is_settling(&self) -> bool {
        self.candidate.is_some()
    }

    /// The update to send now, if any: the window has been on
    /// `window_display` (an id from `displays`) for [`SETTLE`], and that
    /// display or its refresh differs from what the host was last told.
    /// `displays` is the shell's own reading, primary first.
    pub fn observe(
        &mut self,
        displays: &[ClientMonitor],
        window_display: Option<u32>,
        now: Instant,
    ) -> Option<DisplayRefreshUpdateMsg> {
        self.observed = true;
        let refresh = crate::connect_plan::deck_display_refresh(displays, window_display)?;
        refresh.stream_display_id?;
        let current = (refresh.stream_display_id, refresh.stream_refresh_hz());
        if self.reported == Some(current) {
            self.candidate = None;
            return None;
        }
        match self.candidate {
            Some((candidate, since)) if candidate == current => {
                if now.saturating_duration_since(since) < SETTLE {
                    return None;
                }
            }
            _ => {
                self.candidate = Some((current, now));
                return None;
            }
        }
        self.candidate = None;
        self.reported = Some(current);
        self.sequence += 1;
        Some(DisplayRefreshUpdateMsg::new(self.sequence, refresh))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcen_protocol::messages::{DISPLAY_REFRESH_UPDATE, StreamRateMsg};

    fn display(id: u32, refresh_hz: u32, is_primary: bool) -> ClientMonitor {
        ClientMonitor {
            id,
            x: 0,
            y: 0,
            width_px: 1920,
            height_px: 1080,
            scale: 1.0,
            refresh_hz,
            is_primary,
            name: format!("display {id}"),
            width_mm: 0.0,
            height_mm: 0.0,
            ..ClientMonitor::default()
        }
    }

    #[test]
    fn a_window_that_settles_on_another_display_is_reported_once() {
        let displays = [display(1, 120, true), display(2, 60, false)];
        let connected = crate::connect_plan::deck_display_refresh(&displays, Some(1));
        let mut follower = StreamDisplayFollower::new(connected.as_ref());
        let start = Instant::now();
        assert_eq!(follower.observe(&displays, Some(1), start), None);
        assert_eq!(follower.observe(&displays, Some(2), start), None);
        assert!(follower.is_settling());
        assert_eq!(
            follower.observe(&displays, Some(2), start + SETTLE / 2),
            None
        );
        let update = follower
            .observe(&displays, Some(2), start + SETTLE)
            .expect("settled on display 2");
        assert_eq!(update.msg_type, DISPLAY_REFRESH_UPDATE);
        assert_eq!(update.sequence, 1);
        assert_eq!(update.refresh.stream_display_id, Some(2));
        assert_eq!(update.refresh.stream_refresh_hz(), Some(60));
        assert_eq!(
            follower.observe(&displays, Some(2), start + SETTLE * 4),
            None
        );
        // Back to the first display: a new sequence.
        assert_eq!(
            follower.observe(&displays, Some(1), start + SETTLE * 5),
            None
        );
        let back = follower
            .observe(&displays, Some(1), start + SETTLE * 6)
            .expect("settled back");
        assert_eq!(back.sequence, 2);
        assert_eq!(back.refresh.stream_refresh_hz(), Some(120));
    }

    #[test]
    fn a_window_dragged_across_a_display_reports_nothing_for_it() {
        let displays = [
            display(1, 60, true),
            display(2, 144, false),
            display(3, 60, false),
        ];
        let connected = crate::connect_plan::deck_display_refresh(&displays, Some(1));
        let mut follower = StreamDisplayFollower::new(connected.as_ref());
        let start = Instant::now();
        assert_eq!(follower.observe(&displays, Some(2), start), None);
        assert_eq!(
            follower.observe(&displays, Some(3), start + SETTLE / 2),
            None
        );
        let update = follower
            .observe(&displays, Some(3), start + SETTLE * 2)
            .expect("settled on display 3");
        assert_eq!(update.refresh.stream_display_id, Some(3));
    }

    #[test]
    fn a_changed_refresh_on_the_same_display_is_reported() {
        let mut displays = [display(1, 120, true)];
        let connected = crate::connect_plan::deck_display_refresh(&displays, Some(1));
        let mut follower = StreamDisplayFollower::new(connected.as_ref());
        let start = Instant::now();
        displays[0].refresh_hz = 60;
        assert_eq!(follower.observe(&displays, Some(1), start), None);
        let update = follower
            .observe(&displays, Some(1), start + SETTLE)
            .expect("refresh changed");
        assert_eq!(update.refresh.stream_refresh_hz(), Some(60));
    }

    #[test]
    fn a_session_that_stated_no_window_display_reports_the_real_one() {
        let displays = [display(1, 144, true), display(2, 60, false)];
        let connected = crate::connect_plan::deck_display_refresh(&displays, None);
        let mut follower = StreamDisplayFollower::new(connected.as_ref());
        assert!(!follower.has_observed());
        let start = Instant::now();
        assert_eq!(follower.observe(&displays, Some(2), start), None);
        assert!(follower.has_observed());
        let update = follower
            .observe(&displays, Some(2), start + SETTLE)
            .expect("the window's own display");
        assert_eq!(update.refresh.stream_refresh_hz(), Some(60));
    }

    #[test]
    fn an_unknown_window_display_reports_nothing() {
        let displays = [display(1, 120, true), display(2, 60, false)];
        let mut follower = StreamDisplayFollower::new(None);
        let start = Instant::now();
        assert_eq!(follower.observe(&displays, None, start), None);
        assert_eq!(follower.observe(&displays, Some(9), start + SETTLE), None);
    }

    #[test]
    fn only_a_single_stream_on_a_host_that_follows_is_followed() {
        let mut hello: ServerHelloMsg =
            serde_json::from_str(r#"{"type":"server_hello"}"#).expect("hello");
        assert!(!host_follows_display_moves(&hello, false));
        hello.stream_rate = Some(StreamRateMsg {
            fps: 60,
            display_refresh_hz: Some(60),
            basis: "display".to_owned(),
            follows_display_moves: true,
        });
        assert!(host_follows_display_moves(&hello, false));
        assert!(!host_follows_display_moves(&hello, true));
    }
}
