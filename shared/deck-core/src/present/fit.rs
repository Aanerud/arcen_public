// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Where a session window shows the remote picture, as one value that both
//! drawing and input read.
//!
//! A Deck window lays out its picture in its own top-left, y-down
//! coordinates (egui's, and the coordinates its pointer events use): a
//! [`PictureRect`]. Everything that has to agree about the picture is
//! derived from that one rectangle:
//!
//! - drawing: [`PictureRect::layer_frame`] places a native video layer in its
//!   view's own coordinate space, which is y-down for a flipped view (winit's
//!   `NSView`, every Win32 client area) and y-up for an unflipped `NSView`;
//! - input: [`PictureRect::fraction`] and [`PictureRect::clamped_fraction`]
//!   turn a pointer position into the fraction of the picture the host maps
//!   to its desktop.
//!
//! The picture itself comes from [`PictureRect::aspect_fit`]: the stream's
//! aspect ratio, centred in the space the window gives the video, every
//! edge on a physical pixel. A window that converts the rectangle for one
//! purpose and not the other shows the picture somewhere its pointer is
//! not: the macOS Deck once placed its Metal layer as if its view were
//! unflipped, so in a window the picture sat one session toolbar higher
//! than where clicks landed.

/// A rectangle in a window's top-left, y-down coordinates (points).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PictureRect {
    pub left: f32,
    pub top: f32,
    pub width: f32,
    pub height: f32,
}

/// Which way a native view's own y axis points.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewYAxis {
    /// y grows downwards from the top edge: a flipped `NSView` (winit's
    /// content view, whose backing layer `AppKit` geometry-flips to match), a
    /// Win32 client area.
    Down,
    /// y grows upwards from the bottom edge: an unflipped `NSView` and its
    /// backing layer.
    Up,
}

/// A native layer's frame in its view's own coordinates (points).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LayerFrame {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl PictureRect {
    #[must_use]
    pub fn right(self) -> f32 {
        self.left + self.width
    }

    #[must_use]
    pub fn bottom(self) -> f32 {
        self.top + self.height
    }

    /// Whether `(x, y)` lies on the picture, edges included.
    #[must_use]
    pub fn contains(self, x: f32, y: f32) -> bool {
        x >= self.left && x <= self.right() && y >= self.top && y <= self.bottom()
    }

    /// Fits a `source` picture (only its aspect ratio counts) into
    /// `container`, centred, each edge snapped to a physical pixel at
    /// `pixels_per_point`. Without a usable source or container the picture
    /// is the whole container.
    #[must_use]
    pub fn aspect_fit(container: Self, source: Option<[usize; 2]>, pixels_per_point: f32) -> Self {
        let Some([w, h]) = source else {
            return container;
        };
        if w == 0 || h == 0 || container.width <= 0.0 || container.height <= 0.0 {
            return container;
        }
        let snap = |v: f32| (v * pixels_per_point).round() / pixels_per_point;
        #[allow(
            clippy::cast_precision_loss,
            reason = "picture sizes are far below 2^24"
        )]
        let aspect = w as f32 / h as f32;
        let fit_h = (container.width / aspect).min(container.height);
        let fit_w = fit_h * aspect;
        let cx = container.left + container.width / 2.0;
        let cy = container.top + container.height / 2.0;
        let left = snap(cx - fit_w / 2.0);
        let top = snap(cy - fit_h / 2.0);
        let right = snap(cx + fit_w / 2.0);
        let bottom = snap(cy + fit_h / 2.0);
        Self {
            left,
            top,
            width: right - left,
            height: bottom - top,
        }
    }

    /// Where a layer showing this picture goes in a view `view_height`
    /// points tall whose y axis points `y_axis`: it covers exactly the
    /// points [`Self::fraction`] maps into `0.0..=1.0`.
    #[must_use]
    pub fn layer_frame(self, view_height: f64, y_axis: ViewYAxis) -> LayerFrame {
        let y = match y_axis {
            ViewYAxis::Down => f64::from(self.top),
            ViewYAxis::Up => view_height - f64::from(self.bottom()),
        };
        LayerFrame {
            x: f64::from(self.left),
            y,
            width: f64::from(self.width),
            height: f64::from(self.height),
        }
    }

    /// `(x, y)` as a fraction of the picture, unclamped (outside
    /// `0.0..=1.0` off the picture); `None` for an empty picture.
    #[must_use]
    pub fn fraction(self, x: f32, y: f32) -> Option<(f64, f64)> {
        if self.width <= 0.0 || self.height <= 0.0 {
            return None;
        }
        let fraction = (
            f64::from((x - self.left) / self.width),
            f64::from((y - self.top) / self.height),
        );
        (fraction.0.is_finite() && fraction.1.is_finite()).then_some(fraction)
    }

    /// [`Self::fraction`] clamped into the picture.
    #[must_use]
    pub fn clamped_fraction(self, x: f32, y: f32) -> Option<(f64, f64)> {
        if self.width <= 0.0 || self.height <= 0.0 {
            return None;
        }
        Some((
            f64::from(((x - self.left) / self.width).clamp(0.0, 1.0)),
            f64::from(((y - self.top) / self.height).clamp(0.0, 1.0)),
        ))
    }
}

impl LayerFrame {
    /// The window point (top-left, y-down) where the user sees the point
    /// `(dx, dy)` of this layer, measured from its visible top-left corner.
    #[must_use]
    pub fn window_point(self, view_height: f64, y_axis: ViewYAxis, dx: f64, dy: f64) -> (f64, f64) {
        let top = match y_axis {
            ViewYAxis::Down => self.y,
            ViewYAxis::Up => view_height - self.y - self.height,
        };
        (self.x + dx, top + dy)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A windowed macOS session: 1120 × 760 points of content, the 28-pt
    /// session toolbar on top, the video below it.
    const VIEW_W: f32 = 1120.0;
    const VIEW_H: f32 = 760.0;
    const TOOLBAR_H: f32 = 28.0;

    fn below_toolbar() -> PictureRect {
        PictureRect {
            left: 0.0,
            top: TOOLBAR_H,
            width: VIEW_W,
            height: VIEW_H - TOOLBAR_H,
        }
    }

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-4
    }

    #[test]
    fn the_layer_sits_below_the_toolbar_in_a_flipped_and_an_unflipped_view() {
        let picture = PictureRect::aspect_fit(below_toolbar(), Some([1800, 1168]), 2.0);
        assert!(picture.top >= TOOLBAR_H, "{picture:?}");
        let view_h = f64::from(VIEW_H);

        let flipped = picture.layer_frame(view_h, ViewYAxis::Down);
        assert!(approx(flipped.y, f64::from(picture.top)), "{flipped:?}");
        let unflipped = picture.layer_frame(view_h, ViewYAxis::Up);
        assert!(
            approx(unflipped.y, view_h - f64::from(picture.bottom())),
            "{unflipped:?}"
        );

        for (frame, axis) in [(flipped, ViewYAxis::Down), (unflipped, ViewYAxis::Up)] {
            let (x, y) = frame.window_point(view_h, axis, 0.0, 0.0);
            assert!(approx(x, f64::from(picture.left)) && approx(y, f64::from(picture.top)));
            assert!(
                y >= f64::from(TOOLBAR_H),
                "{axis:?}: top edge under the toolbar"
            );
            let (_, bottom) = frame.window_point(view_h, axis, frame.width, frame.height);
            assert!(approx(bottom, f64::from(picture.bottom())), "{axis:?}");
        }
    }

    #[test]
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    fn a_click_on_a_shown_pixel_lands_on_that_pixel_in_either_view_at_either_scale() {
        // An 1800 x 1168 desktop, windowed on a Retina display and on a 1x display.
        let host = [1800_usize, 1168];
        let view_h = f64::from(VIEW_H);
        for ppp in [2.0_f32, 1.0] {
            let picture = PictureRect::aspect_fit(below_toolbar(), Some(host), ppp);
            for axis in [ViewYAxis::Down, ViewYAxis::Up] {
                let frame = picture.layer_frame(view_h, axis);
                let (sx, sy) = (frame.width / host[0] as f64, frame.height / host[1] as f64);
                for (hx, hy) in [(300.0, 300.0), (900.0, 900.0), (1500.0, 600.0), (0.0, 0.0)] {
                    // Where the layer shows host pixel (hx, hy) ...
                    let (wx, wy) = frame.window_point(view_h, axis, hx * sx, hy * sy);
                    // ... is where a click maps back to.
                    let (fx, fy) = picture.fraction(wx as f32, wy as f32).unwrap();
                    let (px, py) = (fx * host[0] as f64, fy * host[1] as f64);
                    assert!(
                        (px - hx).abs() <= 0.5 && (py - hy).abs() <= 0.5,
                        "ppp {ppp} {axis:?}: ({hx}, {hy}) came back as ({px}, {py})"
                    );
                }
            }
        }
    }

    #[test]
    fn a_y_up_frame_in_a_y_down_view_is_drawn_one_toolbar_too_high() {
        // The 0.20.8 macOS Deck fault, stated as maths.
        let picture = below_toolbar();
        let view_h = f64::from(VIEW_H);
        let wrong = picture.layer_frame(view_h, ViewYAxis::Up);
        let (_, shown_top) = wrong.window_point(view_h, ViewYAxis::Down, 0.0, 0.0);
        assert!(approx(
            f64::from(picture.top) - shown_top,
            f64::from(TOOLBAR_H)
        ));
        // A picture filling the whole view (fullscreen, no toolbar) hid it.
        let full = PictureRect {
            left: 0.0,
            top: 0.0,
            width: VIEW_W,
            height: VIEW_H,
        };
        assert_eq!(
            full.layer_frame(view_h, ViewYAxis::Up),
            full.layer_frame(view_h, ViewYAxis::Down)
        );
    }

    #[test]
    fn aspect_fit_centres_snaps_and_falls_back_to_the_container() {
        let container = PictureRect {
            left: 0.0,
            top: 0.0,
            width: 1000.0,
            height: 1000.0,
        };
        let fit = PictureRect::aspect_fit(container, Some([1920, 1080]), 1.0);
        assert!((fit.width - 1000.0).abs() < 0.01);
        assert!((fit.height - 562.0).abs() <= 1.0);
        assert!((fit.top - (1000.0 - fit.height) / 2.0).abs() <= 1.0);
        let odd = PictureRect::aspect_fit(below_toolbar(), Some([1801, 1167]), 2.0);
        for edge in [odd.left, odd.top, odd.right(), odd.bottom()] {
            assert!((edge * 2.0 - (edge * 2.0).round()).abs() < 1e-3, "{odd:?}");
        }
        assert_eq!(PictureRect::aspect_fit(container, None, 2.0), container);
        assert_eq!(
            PictureRect::aspect_fit(container, Some([0, 10]), 2.0),
            container
        );
        let empty = PictureRect {
            width: 0.0,
            ..container
        };
        assert_eq!(PictureRect::aspect_fit(empty, Some([16, 9]), 2.0), empty);
        assert_eq!(empty.fraction(1.0, 1.0), None);
        assert_eq!(empty.clamped_fraction(1.0, 1.0), None);
    }

    #[test]
    fn a_pointer_over_the_toolbar_is_off_the_picture() {
        let picture = below_toolbar();
        assert!(picture.fraction(560.0, 0.0).unwrap().1 < 0.0);
        assert!(picture.clamped_fraction(560.0, 0.0).unwrap().1.abs() < f64::EPSILON);
        assert!(!picture.contains(560.0, 10.0));
        assert!(picture.contains(560.0, TOOLBAR_H));
    }
}
