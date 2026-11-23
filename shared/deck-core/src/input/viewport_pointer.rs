// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! One further Match My Layout window's pointer, event by event.
//!
//! Every pointer event a window receives reaches the wire, in the order it
//! happened and at its own position. Sampling the pointer once per UI pass
//! instead loses whatever happened between two samples: a press and its
//! release, or the moves of a drag. A window whose passes are paced (a still
//! host monitor repaints about once a second) then drops almost all of its
//! input and leaves buttons held on the host.
//!
//! [`ViewportPointer`] decides, for each event, where the window puts the
//! pointer and which buttons it holds:
//!
//! - a press inside the picture belongs to the window until it is released;
//!   while one is held, positions are unclamped (outside `0.0..=1.0` when the
//!   pointer has left the picture), so the drag can continue into another
//!   window ([`super::pointer_drag`]) and the release lands where it ended;
//! - a press that starts outside the picture (a letterbox bar) is ignored as
//!   a whole gesture: dragging it into the picture and releasing it there
//!   sends nothing;
//! - a free pointer counts only while it is over the picture, clamped to it;
//!   once it leaves the window, the window has no say in the pointer.

/// Where a window shows its picture, in the window's own coordinates (the
/// coordinates its pointer events use): the same value the window draws
/// its picture from ([`crate::present::fit`]).
pub use crate::present::fit::PictureRect;

/// One pointer event of a window, in its own coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ViewportPointerEvent {
    Moved {
        x: f32,
        y: f32,
    },
    /// `button`: 0 primary, 1 middle, 2 secondary (wire button minus one).
    Button {
        x: f32,
        y: f32,
        button: usize,
        pressed: bool,
    },
    /// The pointer left the window.
    Left,
}

/// What one event means for the host pointer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ViewportPointerSample {
    /// Where the pointer is, as a fraction of this window's picture:
    /// unclamped while a press that began in the picture is held (or is
    /// being released), clamped otherwise. `None` when this window has no
    /// say in the pointer.
    pub fraction: Option<(f64, f64)>,
    /// The buttons this window holds down on the host after the event
    /// (primary, middle, secondary).
    pub held: [bool; 3],
}

/// One window's pointer state between its events.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ViewportPointer {
    /// Buttons pressed inside the picture and not yet released.
    accepted: [bool; 3],
    /// Buttons pressed outside the picture and not yet released.
    ignored: [bool; 3],
}

impl ViewportPointer {
    /// The buttons this window holds on the host.
    #[must_use]
    pub const fn held(&self) -> [bool; 3] {
        self.accepted
    }

    /// Applies one event; `picture` is where the window shows its picture
    /// now (`None` before it has shown one). `None` for a button the wire
    /// has no code for, or for leaving the window while a button is held
    /// (the platform keeps delivering that drag here).
    pub fn apply(
        &mut self,
        picture: Option<PictureRect>,
        event: ViewportPointerEvent,
    ) -> Option<ViewportPointerSample> {
        let (x, y, releasing_accepted) = match event {
            ViewportPointerEvent::Moved { x, y } => (x, y, false),
            ViewportPointerEvent::Button {
                x,
                y,
                button,
                pressed,
            } => {
                if button >= self.accepted.len() {
                    return None;
                }
                if pressed {
                    if picture.is_some_and(|picture| picture.contains(x, y)) {
                        if !self.ignored[button] {
                            self.accepted[button] = true;
                        }
                    } else if !self.accepted[button] {
                        self.ignored[button] = true;
                    }
                    (x, y, false)
                } else {
                    let releasing = self.accepted[button];
                    self.accepted[button] = false;
                    self.ignored[button] = false;
                    (x, y, releasing)
                }
            }
            ViewportPointerEvent::Left => {
                if self.accepted.contains(&true) {
                    return None;
                }
                return Some(ViewportPointerSample {
                    fraction: None,
                    held: self.accepted,
                });
            }
        };
        let in_use = releasing_accepted || self.accepted.contains(&true);
        Some(ViewportPointerSample {
            fraction: picture.and_then(|picture| locate(picture, x, y, in_use)),
            held: self.accepted,
        })
    }
}

fn locate(picture: PictureRect, x: f32, y: f32, in_use: bool) -> Option<(f64, f64)> {
    if !in_use && !picture.contains(x, y) {
        return None;
    }
    let (fx, fy) = picture.fraction(x, y)?;
    if in_use {
        Some((fx, fy))
    } else {
        Some((fx.clamp(0.0, 1.0), fy.clamp(0.0, 1.0)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 1000x500 picture with letterbox bars above and below it.
    const PICTURE: PictureRect = PictureRect {
        left: 0.0,
        top: 200.0,
        width: 1000.0,
        height: 500.0,
    };

    fn moved(x: f32, y: f32) -> ViewportPointerEvent {
        ViewportPointerEvent::Moved { x, y }
    }

    fn button(x: f32, y: f32, button: usize, pressed: bool) -> ViewportPointerEvent {
        ViewportPointerEvent::Button {
            x,
            y,
            button,
            pressed,
        }
    }

    fn close(actual: Option<(f64, f64)>, expected: (f64, f64)) -> bool {
        actual.is_some_and(|(x, y)| (x - expected.0).abs() < 1e-6 && (y - expected.1).abs() < 1e-6)
    }

    /// Live (Match My Layout, still host monitor): the window painted about
    /// once a second and 4-7 of 120 hover moves reached the host. Every move
    /// is a sample of its own.
    #[test]
    fn every_move_over_the_picture_is_a_sample_at_its_own_position() {
        let mut pointer = ViewportPointer::default();
        let samples: Vec<_> = (0..120u16)
            .map(|step| pointer.apply(Some(PICTURE), moved(f32::from(step) * 5.0, 450.0)))
            .collect();
        assert_eq!(samples.len(), 120);
        for (step, sample) in (0..120u16).zip(&samples) {
            let sample = sample.expect("a move over the picture is a sample");
            assert!(close(sample.fraction, (f64::from(step) * 0.005, 0.5)));
            assert_eq!(sample.held, [false; 3]);
        }
    }

    /// Live: a title-bar drag sent its press and never its release, because
    /// both happened between two samples; the button stayed held on the host
    /// for 20 s.
    #[test]
    fn a_press_and_its_release_both_reach_the_host_in_order() {
        let mut pointer = ViewportPointer::default();
        let press = pointer
            .apply(Some(PICTURE), button(500.0, 450.0, 0, true))
            .unwrap();
        assert_eq!(press.held, [true, false, false]);
        let drag = pointer.apply(Some(PICTURE), moved(600.0, 500.0)).unwrap();
        assert_eq!(drag.held, [true, false, false]);
        let release = pointer
            .apply(Some(PICTURE), button(600.0, 500.0, 0, false))
            .unwrap();
        assert_eq!(release.held, [false; 3]);
        assert!(close(release.fraction, (0.6, 0.6)));
    }

    #[test]
    fn a_drag_that_leaves_the_picture_stays_unclamped_and_is_released_where_it_ended() {
        let mut pointer = ViewportPointer::default();
        pointer.apply(Some(PICTURE), button(500.0, 450.0, 0, true));
        // Leaving the window does not end a held drag: the platform keeps
        // delivering it here.
        assert_eq!(
            pointer.apply(Some(PICTURE), ViewportPointerEvent::Left),
            None
        );
        let outside = pointer.apply(Some(PICTURE), moved(1500.0, 450.0)).unwrap();
        assert!(close(outside.fraction, (1.5, 0.5)));
        assert_eq!(outside.held, [true, false, false]);
        let release = pointer
            .apply(Some(PICTURE), button(1500.0, 100.0, 0, false))
            .unwrap();
        assert!(close(release.fraction, (1.5, -0.2)));
        assert_eq!(release.held, [false; 3]);
    }

    #[test]
    fn a_letterbox_press_is_ignored_as_a_whole_gesture() {
        let mut pointer = ViewportPointer::default();
        let press = pointer
            .apply(Some(PICTURE), button(500.0, 100.0, 0, true))
            .unwrap();
        assert_eq!(press.fraction, None, "no remote edge click");
        assert_eq!(press.held, [false; 3]);
        let into_picture = pointer.apply(Some(PICTURE), moved(500.0, 450.0)).unwrap();
        assert_eq!(into_picture.held, [false; 3]);
        let release = pointer
            .apply(Some(PICTURE), button(500.0, 450.0, 0, false))
            .unwrap();
        assert_eq!(release.held, [false; 3]);
        // A free hover over the picture: no release is synthesised.
        assert!(close(release.fraction, (0.5, 0.5)));
        let release_outside = {
            let mut pointer = ViewportPointer::default();
            pointer.apply(Some(PICTURE), button(500.0, 100.0, 0, true));
            pointer
                .apply(Some(PICTURE), button(500.0, 100.0, 0, false))
                .unwrap()
        };
        assert_eq!(release_outside.fraction, None);
    }

    #[test]
    fn a_letterbox_press_of_another_button_keeps_a_held_drag() {
        let mut pointer = ViewportPointer::default();
        pointer.apply(Some(PICTURE), button(500.0, 450.0, 0, true));
        let right = pointer
            .apply(Some(PICTURE), button(500.0, 100.0, 2, true))
            .unwrap();
        assert_eq!(right.held, [true, false, false]);
        assert!(close(right.fraction, (0.5, -0.2)));
        let release_left = pointer
            .apply(Some(PICTURE), button(500.0, 100.0, 0, false))
            .unwrap();
        assert_eq!(release_left.held, [false; 3]);
        assert!(close(release_left.fraction, (0.5, -0.2)));
        let release_right = pointer
            .apply(Some(PICTURE), button(500.0, 100.0, 2, false))
            .unwrap();
        assert_eq!(release_right.fraction, None);
    }

    #[test]
    fn a_free_pointer_is_clamped_to_the_picture_and_gone_once_it_leaves() {
        let mut pointer = ViewportPointer::default();
        let edge = pointer.apply(Some(PICTURE), moved(1000.0, 700.0)).unwrap();
        assert!(close(edge.fraction, (1.0, 1.0)));
        let bar = pointer.apply(Some(PICTURE), moved(500.0, 100.0)).unwrap();
        assert_eq!(bar.fraction, None);
        let left = pointer
            .apply(Some(PICTURE), ViewportPointerEvent::Left)
            .unwrap();
        assert_eq!(left.fraction, None);
    }

    #[test]
    fn nothing_is_placed_before_the_window_shows_a_picture() {
        let mut pointer = ViewportPointer::default();
        let press = pointer.apply(None, button(10.0, 10.0, 0, true)).unwrap();
        assert_eq!(press.fraction, None);
        assert_eq!(press.held, [false; 3]);
        let empty = PictureRect {
            width: 0.0,
            ..PICTURE
        };
        assert_eq!(
            pointer
                .apply(Some(empty), moved(0.0, 200.0))
                .unwrap()
                .fraction,
            None
        );
    }

    #[test]
    fn a_button_without_a_wire_code_is_skipped() {
        let mut pointer = ViewportPointer::default();
        assert_eq!(
            pointer.apply(Some(PICTURE), button(1.0, 300.0, 3, true)),
            None
        );
        assert_eq!(pointer.held(), [false; 3]);
    }
}
