// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! A Deck's wheel to `mouse_scroll` deltas.
//!
//! A wheel reports travel in 120ths of a notch (Windows `WHEEL_DELTA`; the
//! same unit Linux's high-resolution wheel uses). A notched wheel moves 120
//! at a time and goes as whole lines, as every host has always understood.
//! A high-resolution wheel or a precision touchpad moves less, and goes as
//! that travel (`ScrollUnitMsg::Point`, which both the Windows and the Linux
//! Pier inject in the same 120ths) to a host that took precise scroll;
//! otherwise it is gathered into whole notches. The sign is the wheel's:
//! vertical positive away from the user (up), horizontal positive right.
//! A platform adapter turns its own sign into this one before it calls in.
//!
//! Travel that does not yet make a notch is kept, not rounded away:
//! [`WheelAccumulator::wheel`] takes whole 120ths per message (Windows
//! `WM_MOUSEWHEEL`), [`WheelAccumulator::travel`] fractional ones (a
//! trackpad's points or a precision touchpad's step for a host without
//! precise scroll), and [`WheelAccumulator::notches`] a wheel that reports
//! lines (a macOS smooth wheel's fraction of a line). All three draw on the
//! same remainder.
//!
//! A wheel's lines always go as whole lines, on every host: a Pier reads
//! `Point` as its own unit of continuous travel (the macOS Pier posts it as
//! pixels), so a fraction of a line sent as `Point` would scroll a
//! different distance from the same line sent whole.
//!
//! # Remainder policy
//!
//! - **Owned by one window.** A Deck keeps one accumulator per session
//!   window (root and every monitor window), so a partial notch never
//!   leaks into another window's count.
//! - **Conserved across a reversal.** Travel back cancels travel forward
//!   exactly, as the Linux kernel's high-resolution wheel accumulator does
//!   (`hidinput_handle_scroll`): a finger's back-and-forth jitter nets out
//!   instead of scrolling, and a slow drift through it still arrives. The
//!   cost is bounded: the first notch after a reversal may take up to one
//!   extra notch of travel.
//! - **Cleared** ([`WheelAccumulator::clear`], or a new accumulator) when
//!   the Deck loses focus, when the window's picture goes away or its
//!   multi-window session is torn down, and on every new or resumed
//!   connection: travel gathered before then belongs to a scroll that has
//!   ended. A resize or a new fit keeps it: travel is the device's, not
//!   the picture's.
//! - **Never poisoned.** Zero, NaN and infinite travel are refused and
//!   leave the remainder untouched.
//! - **A precise host keeps nothing of `wheel` and `travel`** (beyond a
//!   thousandth of a 120th, too little to send). All of it goes at once as
//!   `Point`; only whole notches go as lines. `notches` keeps its fraction
//!   of a line for every host.

use arcen_protocol::messages::ScrollUnitMsg;

use crate::ScrollPhase;

/// Travel of one wheel notch.
pub const WHEEL_DELTA: i32 = 120;

/// One wheel axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WheelAxis {
    /// Up and down (`WM_MOUSEWHEEL`).
    Vertical,
    /// Left and right (`WM_MOUSEHWHEEL`).
    Horizontal,
}

/// One scroll to send: `dx`/`dy` in `unit`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WheelScroll {
    /// Horizontal delta.
    pub dx: f64,
    /// Vertical delta.
    pub dy: f64,
    /// What the deltas measure.
    pub unit: ScrollUnitMsg,
    /// Where a touchpad's scroll is in its gesture
    /// ([`ScrollPhase::None`] for a wheel).
    pub phase: ScrollPhase,
}

/// Travel within this much (a thousandth of a 120th) of a whole number of
/// notches is that number: sums of fractional travel, and lines a platform
/// reports in `f32` (2.3 + 0.7 lines is 359.99999 of travel), land a little
/// off the value they stand for. Whole 120ths are never this close to
/// anything but themselves, so the integer path is unaffected.
const WHOLE_TRAVEL_TOLERANCE: f64 = 1e-3;

impl WheelScroll {
    /// A `Line` scroll's whole notches as the wire's integers (`None` for
    /// `Point` travel or a count out of range).
    #[must_use]
    pub fn whole_lines(&self) -> Option<(i32, i32)> {
        if self.unit != ScrollUnitMsg::Line {
            return None;
        }
        Some((whole_i32(self.dx)?, whole_i32(self.dy)?))
    }
}

fn whole_i32(value: f64) -> Option<i32> {
    let value = value.round();
    if !(f64::from(i32::MIN)..=f64::from(i32::MAX)).contains(&value) {
        return None;
    }
    #[allow(clippy::cast_possible_truncation, reason = "whole and range-checked")]
    Some(value as i32)
}

/// Turns wheel travel into scrolls, keeping what a notch has not yet
/// completed (see the module's remainder policy).
#[derive(Debug, Clone, Copy, Default)]
pub struct WheelAccumulator {
    precise_host: bool,
    /// Travel not yet sent, per axis (horizontal, vertical), in 120ths.
    remainder: [f64; 2],
}

impl WheelAccumulator {
    /// For a host that took precise scroll (`precise_scroll_v1`), or not.
    #[must_use]
    pub const fn new(precise_host: bool) -> Self {
        Self {
            precise_host,
            remainder: [0.0; 2],
        }
    }

    /// Whether the host took precise scroll.
    #[must_use]
    pub const fn precise_host(&self) -> bool {
        self.precise_host
    }

    /// Travel on `axis` not yet sent, in 120ths (for a precise host, only
    /// what [`Self::notches`] keeps).
    #[must_use]
    pub const fn remainder(&self, axis: WheelAxis) -> f64 {
        self.remainder[slot(axis)]
    }

    /// One wheel message's travel; the scroll to send, if any.
    pub fn wheel(&mut self, axis: WheelAxis, delta: i32) -> Option<WheelScroll> {
        if delta == 0 {
            return None;
        }
        self.gather(axis, f64::from(delta), self.precise_host)
    }

    /// Fractional travel on `axis`, in 120ths of a notch, signed as
    /// [`Self::wheel`]; the scroll to send, if any. Zero, NaN and infinite
    /// travel are refused and change nothing.
    pub fn travel(&mut self, axis: WheelAxis, delta: f64) -> Option<WheelScroll> {
        if !delta.is_finite() || delta == 0.0 {
            return None;
        }
        self.gather(axis, delta, self.precise_host)
    }

    /// A wheel's travel reported in lines, in 120ths (lines × 120), signed
    /// as [`Self::wheel`]: whole lines to every host, the fraction kept
    /// until it completes one. Zero, NaN and infinite travel are refused and
    /// change nothing.
    pub fn notches(&mut self, axis: WheelAxis, delta: f64) -> Option<WheelScroll> {
        if !delta.is_finite() || delta == 0.0 {
            return None;
        }
        self.gather(axis, delta, false)
    }

    /// Fractional travel on both axes at once (one trackpad frame), as one
    /// scroll. Where one axis completes whole notches and the other goes as
    /// travel (only a precise host), both go as travel.
    pub fn travel_xy(&mut self, dx: f64, dy: f64) -> Option<WheelScroll> {
        let horizontal = self.travel(WheelAxis::Horizontal, dx);
        let vertical = self.travel(WheelAxis::Vertical, dy);
        combine(horizontal, vertical)
    }

    /// [`Self::notches`] on both axes at once, as one scroll of whole lines.
    pub fn notches_xy(&mut self, dx: f64, dy: f64) -> Option<WheelScroll> {
        let horizontal = self.notches(WheelAxis::Horizontal, dx);
        let vertical = self.notches(WheelAxis::Vertical, dy);
        combine(horizontal, vertical)
    }

    /// Forgets partial notches (focus loss, a new session).
    pub fn clear(&mut self) {
        self.remainder = [0.0; 2];
    }

    fn gather(&mut self, axis: WheelAxis, delta: f64, as_travel: bool) -> Option<WheelScroll> {
        let slot = slot(axis);
        // The integer path saturated at i32; travel never needs more.
        let pending =
            (self.remainder[slot] + delta).clamp(f64::from(i32::MIN), f64::from(i32::MAX));
        if pending.abs() <= WHOLE_TRAVEL_TOLERANCE {
            // Too little to be anything yet; kept, so tiny steps still add up.
            self.remainder[slot] = pending;
            return None;
        }
        let notch = f64::from(WHEEL_DELTA);
        let nearest = (pending / notch).round();
        let (value, unit) = if (pending - nearest * notch).abs() <= WHOLE_TRAVEL_TOLERANCE {
            self.remainder[slot] = 0.0;
            (nearest, ScrollUnitMsg::Line)
        } else if as_travel {
            self.remainder[slot] = 0.0;
            (pending, ScrollUnitMsg::Point)
        } else {
            let notches = (pending / notch).trunc();
            self.remainder[slot] = pending - notches * notch;
            if notches == 0.0 {
                return None;
            }
            (notches, ScrollUnitMsg::Line)
        };
        let (dx, dy) = match axis {
            WheelAxis::Vertical => (0.0, value),
            WheelAxis::Horizontal => (value, 0.0),
        };
        Some(WheelScroll {
            dx,
            dy,
            unit,
            phase: ScrollPhase::None,
        })
    }
}

const fn slot(axis: WheelAxis) -> usize {
    match axis {
        WheelAxis::Vertical => 1,
        WheelAxis::Horizontal => 0,
    }
}

/// One axis's scroll each as one scroll: travel for both when only one is
/// travel (a precise host).
fn combine(horizontal: Option<WheelScroll>, vertical: Option<WheelScroll>) -> Option<WheelScroll> {
    match (horizontal, vertical) {
        (Some(horizontal), Some(vertical)) if horizontal.unit == vertical.unit => {
            Some(WheelScroll {
                dy: vertical.dy,
                ..horizontal
            })
        }
        (Some(horizontal), Some(vertical)) => Some(WheelScroll {
            dx: as_travel(horizontal).0,
            dy: as_travel(vertical).1,
            unit: ScrollUnitMsg::Point,
            phase: ScrollPhase::None,
        }),
        (one, None) | (None, one) => one,
    }
}

/// A scroll's travel in 120ths.
fn as_travel(scroll: WheelScroll) -> (f64, f64) {
    match scroll.unit {
        ScrollUnitMsg::Line => {
            let notch = f64::from(WHEEL_DELTA);
            (scroll.dx * notch, scroll.dy * notch)
        }
        ScrollUnitMsg::Point => (scroll.dx, scroll.dy),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn lines(dx: f64, dy: f64) -> WheelScroll {
        WheelScroll {
            dx,
            dy,
            unit: ScrollUnitMsg::Line,
            phase: ScrollPhase::None,
        }
    }

    #[test]
    fn a_notched_wheel_scrolls_whole_lines_on_every_host() {
        for precise in [false, true] {
            let mut wheel = WheelAccumulator::new(precise);
            assert_eq!(wheel.wheel(WheelAxis::Vertical, 120), Some(lines(0.0, 1.0)));
            assert_eq!(
                wheel.wheel(WheelAxis::Vertical, -240),
                Some(lines(0.0, -2.0))
            );
            assert_eq!(
                wheel.wheel(WheelAxis::Horizontal, 120),
                Some(lines(1.0, 0.0))
            );
        }
    }

    #[test]
    fn a_fine_wheel_is_precise_where_the_host_takes_it() {
        let mut wheel = WheelAccumulator::new(true);
        assert_eq!(
            wheel.wheel(WheelAxis::Vertical, 30),
            Some(WheelScroll {
                dx: 0.0,
                dy: 30.0,
                unit: ScrollUnitMsg::Point,
                phase: ScrollPhase::None,
            })
        );
    }

    #[test]
    fn a_fine_wheel_gathers_notches_for_other_hosts() {
        let mut wheel = WheelAccumulator::new(false);
        assert_eq!(wheel.wheel(WheelAxis::Vertical, 40), None);
        assert_eq!(wheel.wheel(WheelAxis::Vertical, 40), None);
        assert_eq!(wheel.wheel(WheelAxis::Vertical, 40), Some(lines(0.0, 1.0)));
        assert_eq!(wheel.wheel(WheelAxis::Vertical, -100), None);
        wheel.clear();
        assert_eq!(wheel.wheel(WheelAxis::Vertical, -100), None);
        assert_eq!(
            wheel.wheel(WheelAxis::Vertical, -50),
            Some(lines(0.0, -1.0))
        );
        assert_eq!(wheel.wheel(WheelAxis::Horizontal, 0), None);
        // Travel that cancels a partial notch sends nothing.
        assert_eq!(wheel.wheel(WheelAxis::Vertical, 60), None);
        assert_eq!(wheel.wheel(WheelAxis::Vertical, -90), None);
    }

    const fn points(dx: f64, dy: f64) -> WheelScroll {
        WheelScroll {
            dx,
            dy,
            unit: ScrollUnitMsg::Point,
            phase: ScrollPhase::None,
        }
    }

    /// The integer path as it was before fractional travel existed.
    fn integer_reference(
        precise: bool,
        remainder: &mut [i32; 2],
        axis: WheelAxis,
        delta: i32,
    ) -> Option<WheelScroll> {
        if delta == 0 {
            return None;
        }
        let slot = slot(axis);
        let pending = remainder[slot].saturating_add(delta);
        if pending == 0 {
            remainder[slot] = 0;
            return None;
        }
        let (value, unit) = if pending % WHEEL_DELTA == 0 {
            remainder[slot] = 0;
            (f64::from(pending / WHEEL_DELTA), ScrollUnitMsg::Line)
        } else if precise {
            remainder[slot] = 0;
            (f64::from(pending), ScrollUnitMsg::Point)
        } else {
            let notches = pending / WHEEL_DELTA;
            remainder[slot] = pending % WHEEL_DELTA;
            if notches == 0 {
                return None;
            }
            (f64::from(notches), ScrollUnitMsg::Line)
        };
        let (dx, dy) = match axis {
            WheelAxis::Vertical => (0.0, value),
            WheelAxis::Horizontal => (value, 0.0),
        };
        Some(WheelScroll {
            dx,
            dy,
            unit,
            phase: ScrollPhase::None,
        })
    }

    #[test]
    fn the_integer_path_is_unchanged() {
        let mut seed = 0x2545_f491_u32;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed
        };
        for precise in [false, true] {
            let mut wheel = WheelAccumulator::new(precise);
            let mut reference = [0; 2];
            for step in 0..20_000 {
                let raw = next();
                let delta = match step % 7 {
                    0 => i32::MAX,
                    1 => i32::MIN,
                    2 => 0,
                    _ => (raw % 721).cast_signed() - 360,
                };
                let axis = if raw & 1 == 0 {
                    WheelAxis::Vertical
                } else {
                    WheelAxis::Horizontal
                };
                assert_eq!(
                    wheel.wheel(axis, delta),
                    integer_reference(precise, &mut reference, axis, delta),
                    "step {step}: {delta}"
                );
            }
        }
    }

    #[test]
    fn tiny_alternating_travel_sends_nothing_and_leaves_nothing() {
        let mut wheel = WheelAccumulator::new(false);
        for _ in 0..10_000 {
            assert_eq!(wheel.travel(WheelAxis::Vertical, 0.3), None);
            assert_eq!(wheel.travel(WheelAxis::Vertical, -0.3), None);
        }
        assert_eq!(wheel.remainder(WheelAxis::Vertical), 0.0);
        // To a precise host it all goes, both ways, as the travel it is.
        let mut wheel = WheelAccumulator::new(true);
        assert_eq!(
            wheel.travel(WheelAxis::Vertical, 0.3),
            Some(points(0.0, 0.3))
        );
        assert_eq!(
            wheel.travel(WheelAxis::Vertical, -0.3),
            Some(points(0.0, -0.3))
        );
    }

    #[test]
    fn sub_unit_travel_crosses_a_notch_and_is_conserved_exactly() {
        let mut wheel = WheelAccumulator::new(false);
        // A trackpad's tenth-of-a-point steps: 1,200 of them are one notch.
        let mut sent = Vec::new();
        for _ in 0..1_200 {
            sent.extend(wheel.travel(WheelAxis::Vertical, 0.1));
        }
        assert_eq!(sent, vec![lines(0.0, 1.0)]);
        assert_eq!(wheel.remainder(WheelAxis::Vertical), 0.0);
        // Steps that each round to nothing still add up.
        let mut sent = Vec::new();
        for _ in 0..400 {
            sent.extend(wheel.travel(WheelAxis::Horizontal, -0.45));
        }
        assert_eq!(sent, vec![lines(-1.0, 0.0)]);
        assert!((wheel.remainder(WheelAxis::Horizontal) + 60.0).abs() < 1e-9);
        // Steps too small to be anything on their own still add up.
        let mut wheel = WheelAccumulator::new(false);
        for _ in 0..1_000 {
            assert_eq!(wheel.travel(WheelAxis::Vertical, 5e-4), None);
        }
        assert!((wheel.remainder(WheelAxis::Vertical) - 0.5).abs() < 1e-9);
        // 2.5 notches in one step: two now, the half kept.
        let mut wheel = WheelAccumulator::new(false);
        assert_eq!(
            wheel.travel(WheelAxis::Vertical, 300.0),
            Some(lines(0.0, 2.0))
        );
        assert_eq!(wheel.remainder(WheelAxis::Vertical), 60.0);
        assert_eq!(
            wheel.travel(WheelAxis::Vertical, 60.0),
            Some(lines(0.0, 1.0))
        );
        assert_eq!(wheel.remainder(WheelAxis::Vertical), 0.0);
    }

    #[test]
    fn a_reversal_cancels_travel_rather_than_restarting() {
        let mut wheel = WheelAccumulator::new(false);
        assert_eq!(wheel.travel(WheelAxis::Vertical, 100.0), None);
        // Back 30: 70 forward remain, nothing is sent the other way.
        assert_eq!(wheel.travel(WheelAxis::Vertical, -30.0), None);
        assert_eq!(wheel.remainder(WheelAxis::Vertical), 70.0);
        // Forward again: 70 + 50 is the notch.
        assert_eq!(
            wheel.travel(WheelAxis::Vertical, 50.0),
            Some(lines(0.0, 1.0))
        );
        // A full reversal from a partial notch: 100 forward, then 220 back
        // is exactly one notch back, nothing left over.
        assert_eq!(wheel.travel(WheelAxis::Vertical, 100.0), None);
        assert_eq!(
            wheel.travel(WheelAxis::Vertical, -220.0),
            Some(lines(0.0, -1.0))
        );
        assert_eq!(wheel.remainder(WheelAxis::Vertical), 0.0);
        // The remainder takes the sign of the travel left.
        assert_eq!(wheel.travel(WheelAxis::Vertical, -10.5), None);
        assert_eq!(wheel.remainder(WheelAxis::Vertical), -10.5);
    }

    #[test]
    fn each_window_keeps_its_own_remainder() {
        let mut root = WheelAccumulator::new(false);
        let mut monitor = WheelAccumulator::new(false);
        assert_eq!(root.travel(WheelAxis::Vertical, 119.0), None);
        // The other window's own 50 is not pushed over by root's 119.
        assert_eq!(monitor.travel(WheelAxis::Vertical, 50.0), None);
        assert_eq!(root.remainder(WheelAxis::Vertical), 119.0);
        assert_eq!(monitor.remainder(WheelAxis::Vertical), 50.0);
        // The axes of one window are independent too.
        assert_eq!(monitor.travel(WheelAxis::Horizontal, 100.0), None);
        assert_eq!(
            monitor.travel(WheelAxis::Vertical, 70.0),
            Some(lines(0.0, 1.0))
        );
        assert_eq!(monitor.remainder(WheelAxis::Horizontal), 100.0);
    }

    #[test]
    fn clearing_forgets_partial_notches_on_both_axes() {
        let mut wheel = WheelAccumulator::new(false);
        assert_eq!(wheel.travel(WheelAxis::Vertical, 110.0), None);
        assert_eq!(wheel.travel(WheelAxis::Horizontal, -0.5), None);
        // Focus lost, the picture gone, a new connection.
        wheel.clear();
        assert_eq!(wheel.remainder(WheelAxis::Vertical), 0.0);
        assert_eq!(wheel.remainder(WheelAxis::Horizontal), 0.0);
        assert_eq!(wheel.travel(WheelAxis::Vertical, 10.0), None);
    }

    #[test]
    fn a_precise_host_gets_fractional_travel_as_it_is() {
        let mut wheel = WheelAccumulator::new(true);
        assert_eq!(
            wheel.travel(WheelAxis::Vertical, 0.25),
            Some(points(0.0, 0.25))
        );
        assert_eq!(wheel.remainder(WheelAxis::Vertical), 0.0);
        // A smooth wheel's half line is 60 of travel, not lost and not a
        // line.
        assert_eq!(
            wheel.travel(WheelAxis::Horizontal, 60.0),
            Some(points(60.0, 0.0))
        );
        // Whole notches stay lines.
        assert_eq!(
            wheel.travel(WheelAxis::Vertical, -240.0),
            Some(lines(0.0, -2.0))
        );
        // Both axes in one scroll: travel when either is fractional.
        assert_eq!(wheel.travel_xy(120.0, 30.0), Some(points(120.0, 30.0)));
        assert_eq!(wheel.travel_xy(0.0, 120.0), Some(lines(0.0, 1.0)));
    }

    #[test]
    fn both_axes_go_as_one_scroll() {
        let mut wheel = WheelAccumulator::new(false);
        assert_eq!(wheel.travel_xy(60.0, -90.0), None);
        assert_eq!(wheel.travel_xy(60.0, -30.0), Some(lines(1.0, -1.0)));
        assert_eq!(wheel.travel_xy(130.0, 5.0), Some(lines(1.0, 0.0)));
        assert_eq!(wheel.remainder(WheelAxis::Horizontal), 10.0);
        assert_eq!(wheel.remainder(WheelAxis::Vertical), 5.0);
        assert_eq!(lines(2.0, -3.0).whole_lines(), Some((2, -3)));
        assert_eq!(points(2.0, 0.0).whole_lines(), None);
    }

    #[test]
    fn zero_nan_and_infinite_travel_are_refused_and_change_nothing() {
        for precise in [false, true] {
            let mut wheel = WheelAccumulator::new(precise);
            if !precise {
                assert_eq!(wheel.travel(WheelAxis::Vertical, 50.0), None);
            }
            let before = wheel.remainder(WheelAxis::Vertical);
            for bad in [0.0, -0.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
                assert_eq!(wheel.travel(WheelAxis::Vertical, bad), None, "{bad}");
                assert_eq!(wheel.travel_xy(bad, bad), None, "{bad}");
            }
            assert_eq!(wheel.remainder(WheelAxis::Vertical), before);
            assert_eq!(wheel.remainder(WheelAxis::Horizontal), 0.0);
            // A bad axis does not stop a good one.
            assert!(wheel.travel_xy(f64::NAN, 120.0).is_some());
        }
    }

    #[test]
    fn a_wheel_s_lines_go_as_whole_lines_on_every_host() {
        for precise in [false, true] {
            let mut wheel = WheelAccumulator::new(precise);
            // A quarter line four times is one line, never `Point` travel.
            for _ in 0..3 {
                assert_eq!(wheel.notches(WheelAxis::Vertical, 30.0), None, "{precise}");
            }
            assert_eq!(
                wheel.notches(WheelAxis::Vertical, 30.0),
                Some(lines(0.0, 1.0)),
                "{precise}"
            );
            // 2.3 lines: two now, the 0.3 (36) kept until it completes one.
            assert_eq!(
                wheel.notches(WheelAxis::Vertical, 276.0),
                Some(lines(0.0, 2.0)),
                "{precise}"
            );
            assert!((wheel.remainder(WheelAxis::Vertical) - 36.0).abs() < 1e-9);
            assert_eq!(
                wheel.notches_xy(-120.0, 84.0),
                Some(lines(-1.0, 1.0)),
                "{precise}"
            );
            assert_eq!(wheel.remainder(WheelAxis::Vertical), 0.0);
            // Lines a platform reports in f32 still add up to whole lines.
            let line = |value: f32| f64::from(value) * f64::from(WHEEL_DELTA);
            assert_eq!(
                wheel.notches(WheelAxis::Vertical, line(2.3)),
                Some(lines(0.0, 2.0))
            );
            assert_eq!(
                wheel.notches(WheelAxis::Vertical, line(0.7)),
                Some(lines(0.0, 1.0)),
                "{precise}"
            );
            assert_eq!(wheel.remainder(WheelAxis::Vertical), 0.0);
            for bad in [0.0, f64::NAN, f64::INFINITY] {
                assert_eq!(wheel.notches(WheelAxis::Vertical, bad), None);
                assert_eq!(wheel.notches_xy(bad, bad), None);
            }
        }
    }
}
