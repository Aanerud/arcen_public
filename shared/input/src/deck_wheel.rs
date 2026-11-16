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

/// Turns wheel travel into scrolls, keeping what a notch has not yet
/// completed.
#[derive(Debug, Clone, Copy, Default)]
pub struct WheelAccumulator {
    precise_host: bool,
    remainder: [i32; 2],
}

impl WheelAccumulator {
    /// For a host that took precise scroll (`precise_scroll_v1`), or not.
    #[must_use]
    pub const fn new(precise_host: bool) -> Self {
        Self {
            precise_host,
            remainder: [0; 2],
        }
    }

    /// Whether the host took precise scroll.
    #[must_use]
    pub const fn precise_host(&self) -> bool {
        self.precise_host
    }

    /// One wheel message's travel; the scroll to send, if any.
    pub fn wheel(&mut self, axis: WheelAxis, delta: i32) -> Option<WheelScroll> {
        if delta == 0 {
            return None;
        }
        let slot = match axis {
            WheelAxis::Vertical => 1,
            WheelAxis::Horizontal => 0,
        };
        let pending = self.remainder[slot].saturating_add(delta);
        if pending == 0 {
            self.remainder[slot] = 0;
            return None;
        }
        let (value, unit) = if pending % WHEEL_DELTA == 0 {
            self.remainder[slot] = 0;
            (f64::from(pending / WHEEL_DELTA), ScrollUnitMsg::Line)
        } else if self.precise_host {
            self.remainder[slot] = 0;
            (f64::from(pending), ScrollUnitMsg::Point)
        } else {
            let notches = pending / WHEEL_DELTA;
            self.remainder[slot] = pending % WHEEL_DELTA;
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

    /// Forgets partial notches (focus loss, a new session).
    pub fn clear(&mut self) {
        self.remainder = [0; 2];
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
}
