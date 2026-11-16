// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! A precision touchpad's two-finger scroll and pinch, from a manipulation
//! engine's status and content transform (Windows `DirectManipulation` on a
//! viewport nothing draws), to what a Mac Deck sends for its trackpad.
//!
//! The fingers' scroll goes as a phased precise scroll, `Began … Ended`;
//! the system's inertia after the fingers lift goes as a second
//! `Began … Ended` train, as macOS momentum does. A pinch goes as
//! [`GestureSample::Magnify`] steps. A scroll may turn into a pinch, never
//! the reverse: once two fingers pinch, their drift is the pinch's centre,
//! not a scroll. Rotation, smart zoom and multi-finger swipes are not
//! touchpad gestures an application receives on Windows.

use arcen_protocol::messages::ScrollPhaseMsg;

use crate::ScrollPhase;
use crate::gestures::GestureSample;

/// The engine's state of the manipulation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManipulationStatus {
    /// Being set up.
    Building,
    /// Ready for contacts, none yet.
    Enabled,
    /// Off.
    Disabled,
    /// Fingers are moving it.
    Running,
    /// The fingers lifted; the system's inertia moves it.
    Inertia,
    /// Finished; waiting for the next contact.
    Ready,
    /// Interrupted (another manipulation took the contacts).
    Suspended,
}

/// The content's transform: scale and translation in the window's pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ContentTransform {
    /// 1 is unscaled.
    pub scale: f64,
    /// Right.
    pub x: f64,
    /// Down.
    pub y: f64,
}

impl ContentTransform {
    /// No scale, no translation.
    pub const IDENTITY: Self = Self {
        scale: 1.0,
        x: 0.0,
        y: 0.0,
    };
}

/// What to send.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TouchpadOutput {
    /// A precise scroll of `dx`/`dy` window pixels, signed as a wheel's
    /// (`deck_wheel`): vertical positive up, horizontal positive right. The
    /// content follows the fingers as the user's touchpad settings make it,
    /// so a scroll here is the wheel Windows itself would have produced.
    Scroll {
        dx: f64,
        dy: f64,
        phase: ScrollPhase,
    },
    /// A gesture step for a host that takes gestures.
    Gesture(GestureSample),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Train {
    /// Nothing sent yet in this stage.
    Idle,
    /// A scroll train is open (`Began` sent).
    Scrolling,
    /// A magnify gesture is open.
    Pinching,
}

/// One touchpad manipulation at a time, as status changes and content
/// updates arrive.
#[derive(Debug, Clone, Copy)]
pub struct TouchpadTracker {
    status: ManipulationStatus,
    last: ContentTransform,
    train: Train,
    /// The fingers pinched during this manipulation (inertia then sends
    /// nothing).
    pinched: bool,
}

impl Default for TouchpadTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// Scale changes smaller than this are a pan's rounding, not a pinch.
const SCALE_EPSILON: f64 = 1e-6;

impl TouchpadTracker {
    /// Waiting for the first contact.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            status: ManipulationStatus::Ready,
            last: ContentTransform::IDENTITY,
            train: Train::Idle,
            pinched: false,
        }
    }

    /// The manipulation's status changed. When it stops moving the caller
    /// resets the content to [`ContentTransform::IDENTITY`].
    pub fn status(&mut self, current: ManipulationStatus) -> Vec<TouchpadOutput> {
        let previous = self.status;
        self.status = current;
        let mut out = Vec::new();
        match current {
            ManipulationStatus::Running if previous != ManipulationStatus::Running => {
                if previous == ManipulationStatus::Inertia {
                    // New fingers caught the inertia: its train ends.
                    self.close(ScrollPhase::Ended, &mut out);
                    self.pinched = false;
                } else {
                    self.last = ContentTransform::IDENTITY;
                    self.pinched = false;
                }
            }
            ManipulationStatus::Inertia => {
                // The fingers lifted; momentum, if any, is a new train.
                self.close(ScrollPhase::Ended, &mut out);
            }
            // An engine may go back to Enabled rather than Ready.
            ManipulationStatus::Ready
            | ManipulationStatus::Enabled
            | ManipulationStatus::Building => {
                self.close(ScrollPhase::Ended, &mut out);
                self.last = ContentTransform::IDENTITY;
                self.pinched = false;
            }
            ManipulationStatus::Suspended | ManipulationStatus::Disabled => {
                self.close(ScrollPhase::Cancelled, &mut out);
                self.last = ContentTransform::IDENTITY;
                self.pinched = false;
            }
            ManipulationStatus::Running => {}
        }
        out
    }

    /// The content moved to `now`.
    pub fn content(&mut self, now: ContentTransform) -> Vec<TouchpadOutput> {
        let mut out = Vec::new();
        let moving = matches!(
            self.status,
            ManipulationStatus::Running | ManipulationStatus::Inertia
        );
        if !moving || !now.scale.is_finite() || !now.x.is_finite() || !now.y.is_finite() {
            return out;
        }
        let last = self.last;
        self.last = now;
        let scaled = last.scale > 0.0 && (now.scale / last.scale - 1.0).abs() > SCALE_EPSILON;
        let inertia = self.status == ManipulationStatus::Inertia;
        if scaled && !inertia {
            if self.train == Train::Scrolling {
                self.close(ScrollPhase::Ended, &mut out);
            }
            let phase = if self.train == Train::Pinching {
                ScrollPhaseMsg::Changed
            } else {
                ScrollPhaseMsg::Began
            };
            self.train = Train::Pinching;
            self.pinched = true;
            out.push(TouchpadOutput::Gesture(GestureSample::Magnify {
                scale_delta: now.scale / last.scale - 1.0,
                phase,
            }));
            return out;
        }
        if self.pinched {
            return out;
        }
        let (dx, dy) = (-(now.x - last.x), now.y - last.y);
        if dx == 0.0 && dy == 0.0 {
            return out;
        }
        let phase = if self.train == Train::Scrolling {
            ScrollPhase::Changed
        } else {
            ScrollPhase::Began
        };
        self.train = Train::Scrolling;
        out.push(TouchpadOutput::Scroll { dx, dy, phase });
        out
    }

    fn close(&mut self, phase: ScrollPhase, out: &mut Vec<TouchpadOutput>) {
        match self.train {
            Train::Idle => {}
            Train::Scrolling => out.push(TouchpadOutput::Scroll {
                dx: 0.0,
                dy: 0.0,
                phase,
            }),
            Train::Pinching => out.push(TouchpadOutput::Gesture(GestureSample::Magnify {
                scale_delta: 0.0,
                phase: phase.into(),
            })),
        }
        self.train = Train::Idle;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(scale: f64, x: f64, y: f64) -> ContentTransform {
        ContentTransform { scale, x, y }
    }

    fn scroll(dx: f64, dy: f64, phase: ScrollPhase) -> TouchpadOutput {
        TouchpadOutput::Scroll { dx, dy, phase }
    }

    #[test]
    fn a_two_finger_scroll_and_its_inertia_are_two_trains() {
        let mut pad = TouchpadTracker::new();
        assert!(pad.status(ManipulationStatus::Running).is_empty());
        // Fingers move down: the content follows, as a wheel turned up.
        assert_eq!(
            pad.content(at(1.0, 0.0, 10.0)),
            [scroll(0.0, 10.0, ScrollPhase::Began)]
        );
        assert_eq!(
            pad.content(at(1.0, -4.0, 15.0)),
            [scroll(4.0, 5.0, ScrollPhase::Changed)]
        );
        assert_eq!(
            pad.status(ManipulationStatus::Inertia),
            [scroll(0.0, 0.0, ScrollPhase::Ended)]
        );
        assert_eq!(
            pad.content(at(1.0, -4.0, 18.0)),
            [scroll(0.0, 3.0, ScrollPhase::Began)]
        );
        assert_eq!(
            pad.content(at(1.0, -4.0, 19.0)),
            [scroll(0.0, 1.0, ScrollPhase::Changed)]
        );
        assert_eq!(
            pad.status(ManipulationStatus::Ready),
            [scroll(0.0, 0.0, ScrollPhase::Ended)]
        );
        // The caller's reset to identity while Ready sends nothing.
        assert!(pad.content(ContentTransform::IDENTITY).is_empty());
        // The next manipulation starts from identity.
        pad.status(ManipulationStatus::Running);
        assert_eq!(
            pad.content(at(1.0, 0.0, -2.0)),
            [scroll(0.0, -2.0, ScrollPhase::Began)]
        );
    }

    #[test]
    fn a_scroll_may_become_a_pinch_never_the_reverse() {
        let mut pad = TouchpadTracker::new();
        pad.status(ManipulationStatus::Running);
        pad.content(at(1.0, 0.0, 3.0));
        let out = pad.content(at(1.1, 1.0, 3.0));
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], scroll(0.0, 0.0, ScrollPhase::Ended));
        let TouchpadOutput::Gesture(GestureSample::Magnify { scale_delta, phase }) = out[1] else {
            panic!("{out:?}");
        };
        assert!((scale_delta - 0.1).abs() < 1e-9);
        assert_eq!(phase, ScrollPhaseMsg::Began);
        let out = pad.content(at(1.21, 2.0, 5.0));
        let TouchpadOutput::Gesture(GestureSample::Magnify { scale_delta, phase }) = out[0] else {
            panic!("{out:?}");
        };
        assert!((scale_delta - 0.1).abs() < 1e-9);
        assert_eq!(phase, ScrollPhaseMsg::Changed);
        // Drift at a steady scale is the pinch's centre, not a scroll.
        assert!(pad.content(at(1.21, 9.0, 9.0)).is_empty());
        assert_eq!(
            pad.status(ManipulationStatus::Inertia),
            [TouchpadOutput::Gesture(GestureSample::Magnify {
                scale_delta: 0.0,
                phase: ScrollPhaseMsg::Ended
            })]
        );
        // A pinch has no momentum.
        assert!(pad.content(at(1.3, 12.0, 9.0)).is_empty());
        assert!(pad.status(ManipulationStatus::Ready).is_empty());
    }

    #[test]
    fn an_interrupted_scroll_is_cancelled_and_nothing_moves_while_ready() {
        let mut pad = TouchpadTracker::new();
        assert!(pad.content(at(1.0, 0.0, 5.0)).is_empty());
        pad.status(ManipulationStatus::Running);
        pad.content(at(1.0, 0.0, 5.0));
        assert_eq!(
            pad.status(ManipulationStatus::Suspended),
            [scroll(0.0, 0.0, ScrollPhase::Cancelled)]
        );
        assert!(pad.content(at(1.0, 0.0, 9.0)).is_empty());
        assert!(pad.content(at(f64::NAN, 0.0, 9.0)).is_empty());
    }

    #[test]
    fn fingers_that_catch_the_inertia_end_its_train() {
        let mut pad = TouchpadTracker::new();
        pad.status(ManipulationStatus::Running);
        pad.content(at(1.0, 0.0, 5.0));
        pad.status(ManipulationStatus::Inertia);
        pad.content(at(1.0, 0.0, 8.0));
        assert_eq!(
            pad.status(ManipulationStatus::Running),
            [scroll(0.0, 0.0, ScrollPhase::Ended)]
        );
        // The transform continues from where the inertia left it.
        assert_eq!(
            pad.content(at(1.0, 0.0, 10.0)),
            [scroll(0.0, 2.0, ScrollPhase::Began)]
        );
    }
}
