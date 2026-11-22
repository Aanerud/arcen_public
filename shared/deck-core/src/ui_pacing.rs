// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! How often a Deck's UI asks for its next pass, and how often what it
//! shows may change.
//!
//! egui shortens every delayed repaint by its predicted frame time
//! (`InputState::predicted_dt`, which eframe leaves at its 1/60 s default):
//! `request_repaint_after(16 ms)` becomes an immediate repaint. With display
//! sync on, the next drawable caps that at the refresh rate; with display
//! sync off (a multi-window session), nothing does, and the UI spins at
//! hundreds of passes a second on the main thread.

use std::time::{Duration, Instant};

/// The longest refresh period a pacer honours. A display slower than 20 Hz,
/// or a period read wrong, must not leave the UI that slow.
pub const MAX_PACED_PERIOD: Duration = Duration::from_millis(50);

/// How often Stream Statistics' text may change: the macOS Deck's idle UI
/// pass. A panel whose text changes on every pass redraws its overlay on
/// every pass (the Windows Deck presented it with every video frame, +2 ms
/// to the picture's deadline).
pub const STATS_PANEL_REFRESH: Duration = Duration::from_millis(250);

/// When a window's next UI pass may begin: never sooner than one refresh of
/// the display under it after the previous pass began.
///
/// egui asks for two more passes after every pass that carried input (its
/// "settle" passes), so a pointer moving at 55 events a second asked a
/// window whose presents do not wait for the display for 165 passes a
/// second. Every pass in excess of the refresh is never seen; on macOS it
/// blocks the main thread in `nextDrawable`, where input cannot be
/// dispatched. A pass held back here is merged with whatever asks for one
/// before it is due: input that arrives meanwhile is in that pass, at most
/// one refresh later, as with display sync on.
#[derive(Debug, Clone, Default)]
pub struct PassPacer {
    last_pass_at: Option<Instant>,
    held: bool,
    released: bool,
}

impl PassPacer {
    /// Whether a pass asked for at `now` may begin now. When it may, it is
    /// recorded as begun; otherwise it is held and [`Self::held_until`]
    /// says when it is due. Without a known `period` every pass runs.
    pub fn admit(&mut self, now: Instant, period: Option<Duration>) -> bool {
        let due = match (self.last_pass_at, Self::paced(period)) {
            (Some(last), Some(period)) if !self.released => {
                now.saturating_duration_since(last) >= period
            }
            _ => true,
        };
        if due {
            self.last_pass_at = Some(now);
            self.held = false;
            self.released = false;
        } else {
            self.held = true;
        }
        due
    }

    /// When the held pass is due, if one is held.
    #[must_use]
    pub fn held_until(&self, period: Option<Duration>) -> Option<Instant> {
        if !self.held {
            return None;
        }
        match (self.last_pass_at, Self::paced(period)) {
            (Some(last), Some(period)) if !self.released => Some(last + period),
            _ => self.last_pass_at,
        }
    }

    /// Whether a held pass is due at `now` (the caller then runs it through
    /// [`Self::admit`]).
    #[must_use]
    pub fn held_due(&self, now: Instant, period: Option<Duration>) -> bool {
        self.held_until(period).is_some_and(|until| now >= until)
    }

    /// The next pass runs at once whatever the pacing: the window changed
    /// size or scale (a stale picture would stretch), or a decoded picture
    /// waits for the pass to be shown.
    pub const fn release(&mut self) {
        self.released = true;
    }

    fn paced(period: Option<Duration>) -> Option<Duration> {
        period
            .filter(|period| !period.is_zero())
            .map(|period| period.min(MAX_PACED_PERIOD))
    }
}

/// What one window event asks of the window's next UI pass, given whether
/// the UI (egui-winit's `EventResponse::repaint`) wants one.
///
/// egui-winit answers `repaint: true` for the window's redraw event itself.
/// A shell that asks for a redraw on that answer asks for one redraw per
/// redraw: the pacer holds almost all of them, and the event loop never
/// sleeps (the Windows Deck kept a core busy on an idle Home, 12,000 to
/// 23,000 redraws a second).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepaintRequest {
    /// No further pass: the UI wants none, or the event is the redraw the
    /// pass runs in, or a scale change whose resize is still to come.
    None,
    /// A pass is due now. It reaches the window through its pending pass
    /// time and its [`PassPacer`], never as an immediate redraw, so it is
    /// paced like every other pass.
    Due,
}

/// Which window event a [`RepaintRequest`] answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PassEvent {
    /// The redraw the shell runs its pass in.
    Redraw,
    /// The window's scale changed. The window has not been resized for it
    /// yet (winit resizes it after this event, on Windows inside the same
    /// `WM_DPICHANGED`): a pass now would lay out the new scale in the old
    /// size, and use up the pacer's release. The resize that follows
    /// releases the pacer and runs the pass.
    ScaleChange,
    /// Any other event: input, resize, focus, occlusion.
    Other,
}

impl RepaintRequest {
    /// The request of a window event.
    #[must_use]
    pub const fn for_window_event(event: PassEvent, ui_wants_repaint: bool) -> Self {
        match event {
            PassEvent::Other if ui_wants_repaint => Self::Due,
            PassEvent::Other | PassEvent::Redraw | PassEvent::ScaleChange => Self::None,
        }
    }

    /// The window's pending pass time with this request made at `now`: the
    /// earlier of the two.
    #[must_use]
    pub fn merged(self, pending: Option<Instant>, now: Instant) -> Option<Instant> {
        match self {
            Self::None => pending,
            Self::Due => Some(pending.map_or(now, |at| at.min(now))),
        }
    }
}

/// A value shown on screen that is rebuilt at most once per `interval`, so
/// what draws it changes no more often than that (Stream Statistics' text).
#[derive(Debug, Clone)]
pub struct PacedValue<T> {
    interval: Duration,
    value: Option<(Instant, T)>,
}

impl<T> PacedValue<T> {
    #[must_use]
    pub const fn new(interval: Duration) -> Self {
        Self {
            interval,
            value: None,
        }
    }

    /// Whether the value is missing or older than the interval at `now`.
    #[must_use]
    pub fn is_due(&self, now: Instant) -> bool {
        self.value
            .as_ref()
            .is_none_or(|(at, _)| now.saturating_duration_since(*at) >= self.interval)
    }

    /// Stores a value built at `now`.
    pub fn set(&mut self, now: Instant, value: T) {
        self.value = Some((now, value));
    }

    /// The value last stored.
    #[must_use]
    pub fn value(&self) -> Option<&T> {
        self.value.as_ref().map(|(_, value)| value)
    }

    /// The value, rebuilt with `build` when it is due.
    pub fn get_or_refresh(&mut self, now: Instant, build: impl FnOnce() -> T) -> &T {
        if self.is_due(now) {
            self.value = None;
        }
        &self.value.get_or_insert_with(|| (now, build())).1
    }

    /// Forget the value: the next read rebuilds it (a session ended).
    pub fn clear(&mut self) {
        self.value = None;
    }
}

/// The delay to hand `egui::Context::request_repaint_after` so the next pass
/// runs `interval` from now: egui subtracts `predicted_dt` (seconds) again.
#[must_use]
pub fn egui_repaint_delay(interval: Duration, predicted_dt: f32) -> Duration {
    let predicted = Duration::try_from_secs_f32(predicted_dt).unwrap_or(Duration::ZERO);
    interval.saturating_add(predicted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_poll_interval_survives_eguis_predicted_frame_time() {
        // egui's default prediction: 1/60 s. Without compensation a 16 ms
        // poll reads as 16 - 16.67 ms = an immediate repaint.
        let predicted = 1.0 / 60.0;
        let delay = egui_repaint_delay(Duration::from_millis(16), predicted);
        let after_egui = delay.saturating_sub(Duration::from_secs_f32(predicted));
        assert_eq!(after_egui, Duration::from_millis(16));
    }

    const REFRESH_120: Duration = Duration::from_micros(8_333);

    #[test]
    fn only_an_event_other_than_the_redraw_or_a_scale_change_asks_for_a_pass() {
        for (event, wants, request) in [
            (PassEvent::Redraw, true, RepaintRequest::None),
            (PassEvent::Redraw, false, RepaintRequest::None),
            // The resize winit sends after it runs the pass.
            (PassEvent::ScaleChange, true, RepaintRequest::None),
            (PassEvent::ScaleChange, false, RepaintRequest::None),
            (PassEvent::Other, false, RepaintRequest::None),
            (PassEvent::Other, true, RepaintRequest::Due),
        ] {
            assert_eq!(
                RepaintRequest::for_window_event(event, wants),
                request,
                "{event:?} {wants}"
            );
        }
        let now = Instant::now();
        let earlier = now.checked_sub(Duration::from_millis(5)).unwrap_or(now);
        let later = now + Duration::from_millis(5);
        assert_eq!(RepaintRequest::None.merged(Some(later), now), Some(later));
        assert_eq!(RepaintRequest::None.merged(None, now), None);
        assert_eq!(RepaintRequest::Due.merged(None, now), Some(now));
        assert_eq!(RepaintRequest::Due.merged(Some(later), now), Some(now));
        assert_eq!(
            RepaintRequest::Due.merged(Some(earlier), now),
            Some(earlier)
        );
    }

    /// The Windows Deck's loop on an idle Home: one input, then egui wants
    /// nothing more but answers `repaint: true` for every redraw. The loop
    /// must go back to sleep (it spun at 12,000+ redraws a second).
    #[test]
    fn an_idle_window_stops_redrawing_after_its_pass() {
        let start = Instant::now();
        let mut pacer = PassPacer::default();
        let mut pending =
            RepaintRequest::for_window_event(PassEvent::Other, true).merged(None, start);
        let mut redraw_queued = false;
        let mut redraws = 0;
        let mut now = start;
        while now < start + Duration::from_secs(1) {
            if redraw_queued {
                redraw_queued = false;
                redraws += 1;
                if !pacer.admit(now, Some(REFRESH_120))
                    && let Some(due) = pacer.held_until(Some(REFRESH_120))
                {
                    pending = RepaintRequest::Due.merged(pending, due);
                }
                pending =
                    RepaintRequest::for_window_event(PassEvent::Redraw, true).merged(pending, now);
            } else if pending.is_some_and(|at| at <= now) {
                // `about_to_wait`: a due pass becomes a redraw.
                pending = None;
                redraw_queued = true;
            }
            now += Duration::from_micros(100);
        }
        assert_eq!(redraws, 1);
        assert_eq!(pending, None);
    }

    /// Live, Match My Layout, mouse over the root window: 55 events a
    /// second asked for 165 passes (each input pass and egui's two settle
    /// passes) on a 120 Hz display.
    #[test]
    fn a_pointer_with_egui_settle_passes_never_exceeds_the_refresh() {
        let start = Instant::now();
        let mut pacer = PassPacer::default();
        let mut passes = 0;
        let mut now = start;
        let step = Duration::from_micros(500);
        let mut pending = 0_u32;
        let mut next_event = start;
        while now < start + Duration::from_secs(1) {
            if now >= next_event {
                // An input pass asks for itself and two settle passes.
                pending = pending.max(3);
                next_event += Duration::from_micros(18_182);
            }
            if pending > 0 && pacer.admit(now, Some(REFRESH_120)) {
                passes += 1;
                pending -= 1;
            }
            now += step;
        }
        assert!(passes <= 121, "{passes} passes in one second");
        // Every event still got a pass of its own.
        assert!(passes >= 110, "{passes} passes in one second");
    }

    #[test]
    fn input_waits_at_most_one_refresh_and_a_held_pass_is_due_then() {
        let start = Instant::now();
        let mut pacer = PassPacer::default();
        assert!(pacer.admit(start, Some(REFRESH_120)));
        let early = start + Duration::from_millis(2);
        assert!(!pacer.admit(early, Some(REFRESH_120)));
        assert_eq!(
            pacer.held_until(Some(REFRESH_120)),
            Some(start + REFRESH_120)
        );
        assert!(!pacer.held_due(start + Duration::from_millis(8), Some(REFRESH_120)));
        assert!(pacer.held_due(start + REFRESH_120, Some(REFRESH_120)));
        assert!(pacer.admit(start + REFRESH_120, Some(REFRESH_120)));
        assert_eq!(pacer.held_until(Some(REFRESH_120)), None);
    }

    #[test]
    fn an_unknown_refresh_paces_nothing_and_a_slow_one_is_bounded() {
        let start = Instant::now();
        let mut pacer = PassPacer::default();
        assert!(pacer.admit(start, None));
        assert!(pacer.admit(start, None));
        assert!(pacer.admit(start, Some(Duration::ZERO)));
        assert!(!pacer.admit(start, Some(Duration::from_secs(1))));
        assert_eq!(
            pacer.held_until(Some(Duration::from_secs(1))),
            Some(start + MAX_PACED_PERIOD)
        );
    }

    #[test]
    fn a_resize_lets_the_next_pass_run_at_once() {
        let start = Instant::now();
        let mut pacer = PassPacer::default();
        assert!(pacer.admit(start, Some(REFRESH_120)));
        assert!(!pacer.admit(start, Some(REFRESH_120)));
        pacer.release();
        assert!(pacer.held_due(start, Some(REFRESH_120)));
        assert!(pacer.admit(start, Some(REFRESH_120)));
        // Pacing resumes after it.
        assert!(!pacer.admit(start + Duration::from_millis(1), Some(REFRESH_120)));
    }

    #[test]
    fn stats_text_changes_at_most_four_times_a_second() {
        let start = Instant::now();
        let mut text = PacedValue::new(STATS_PANEL_REFRESH);
        let mut builds = 0;
        let mut now = start;
        // A pass per video frame at 120 fps for one second.
        while now < start + Duration::from_secs(1) {
            text.get_or_refresh(now, || {
                builds += 1;
                builds
            });
            now += REFRESH_120;
        }
        assert_eq!(builds, 4);
        text.clear();
        assert_eq!(*text.get_or_refresh(now, || 99), 99);
    }

    #[test]
    fn a_nonsensical_prediction_adds_nothing() {
        assert_eq!(
            egui_repaint_delay(Duration::from_millis(250), f32::NAN),
            Duration::from_millis(250)
        );
        assert_eq!(
            egui_repaint_delay(Duration::from_millis(250), -1.0),
            Duration::from_millis(250)
        );
    }
}
