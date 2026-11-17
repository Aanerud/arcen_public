// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! A held-button drag across Match My Layout's windows.
//!
//! A multi-window session shows each host monitor in its own fullscreen
//! window, one per Deck display. While a mouse button is held the platform
//! keeps delivering the pointer to the window the press started in, even
//! after the cursor has physically left it (`AppKit`'s mouse-down window,
//! Win32 `SetCapture`), so that window reports positions outside its own
//! picture. Clamping them into its monitor pins the remote cursor to the
//! edge: a remote window dragged towards the next screen stops there.
//!
//! [`PointerDragCapture`] is the one rule every Deck uses instead:
//!
//! - the first press latches the window it happened in as the drag's
//!   *origin*; until the last button is released only the origin's samples
//!   move the one remote pointer, and samples from every other window are
//!   [`DragRoute::Ignore`]d (never two windows dispatching in one frame);
//! - each origin sample is placed on the Deck's own desktop (the physical
//!   display arrangement, [`PictureSurface`]) and dispatched to the monitor
//!   whose window is under the cursor, at that monitor's own picture
//!   fraction; a point in no window (a gap, a letterbox bar, beyond the
//!   desktop) lands on the nearest picture's edge rather than stopping;
//! - the release lands where the drag ended, then the capture ends.
//!
//! The Deck's arrangement decides, not the host's: a host that laid its
//! monitors out differently (a guest or foreign desktop, a size it did not
//! honour) still receives the pointer on the monitor the user's hand is
//! over. Only when a Deck cannot yet say where its windows are does the
//! host's applied layout ([`arcen_media::resolve_pointer_crossing`]) decide.

use arcen_input::deck_pen::ImageRect;
use arcen_media::{MonitorDesktopRect, SessionMonitorId};

/// Where one monitor is shown on the Deck, in the Deck's own desktop
/// coordinates: one space for every display, top-left origin, y down
/// (global points on macOS, virtual-screen pixels on Windows).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PictureSurface {
    /// The monitor this window shows.
    pub monitor: SessionMonitorId,
    /// The window's content area (its display, when fullscreen).
    pub window: ImageRect,
    /// The monitor's picture inside it (the fitted video).
    pub picture: ImageRect,
}

/// The geometry a drag is resolved against.
#[derive(Debug, Clone, Copy, Default)]
pub struct DragGeometry<'a> {
    /// Every window the Deck can place now.
    pub surfaces: &'a [PictureSurface],
    /// The host's applied layout, the fallback while the origin window
    /// cannot be placed.
    pub host: &'a [(SessionMonitorId, MonitorDesktopRect)],
}

/// Where one pointer sample goes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DragRoute {
    /// To `monitor`, at `fraction` of its picture (each in `0.0..=1.0`).
    To {
        monitor: SessionMonitorId,
        fraction: (f64, f64),
    },
    /// Nowhere: another window owns the pointer during a drag, or the
    /// sample is not a position.
    Ignore,
}

/// The session's one pointer drag: which window holds it and which
/// buttons are down. Buttons are wire buttons (1 left, 2 middle, 3 right).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PointerDragCapture {
    origin: Option<SessionMonitorId>,
    held: u8,
}

impl PointerDragCapture {
    /// The window holding the drag, while a button is held.
    #[must_use]
    pub const fn origin(&self) -> Option<SessionMonitorId> {
        self.origin
    }

    /// The wire buttons held now, as a mask of [`Self::button_bit`]s.
    #[must_use]
    pub const fn held(&self) -> u8 {
        self.held
    }

    /// The bit of wire button `button` in a held mask (`0` for a button
    /// outside 1..=7).
    #[must_use]
    pub fn button_bit(button: u8) -> u8 {
        if (1..=7).contains(&button) {
            1 << button
        } else {
            0
        }
    }

    /// Forgets the drag (focus loss, a new layout, the session's end).
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// A pointer move in `source`'s window at `unclamped`, its picture
    /// fraction, which may lie outside `0.0..=1.0` during a drag.
    #[must_use]
    pub fn motion(
        &mut self,
        geometry: &DragGeometry<'_>,
        source: SessionMonitorId,
        unclamped: (f64, f64),
    ) -> DragRoute {
        self.route(geometry, source, unclamped, self.held, false)
    }

    /// A button edge in `source`'s window at `unclamped`.
    #[must_use]
    pub fn button(
        &mut self,
        geometry: &DragGeometry<'_>,
        source: SessionMonitorId,
        unclamped: (f64, f64),
        button: u8,
        pressed: bool,
    ) -> DragRoute {
        let bit = Self::button_bit(button);
        let held = if pressed {
            self.held | bit
        } else {
            self.held & !bit
        };
        self.route(geometry, source, unclamped, held, pressed)
    }

    /// A pointer sample in `source`'s window at `unclamped`, with `held`
    /// the mask of buttons down in that window once it is applied (see
    /// [`Self::button_bit`]); for a Deck that reads each window's button
    /// state rather than edges.
    #[must_use]
    pub fn sample(
        &mut self,
        geometry: &DragGeometry<'_>,
        source: SessionMonitorId,
        unclamped: (f64, f64),
        held: u8,
    ) -> DragRoute {
        self.route(geometry, source, unclamped, held, held != 0)
    }

    /// `press`: `source` reports a button pressed in its own window.
    fn route(
        &mut self,
        geometry: &DragGeometry<'_>,
        source: SessionMonitorId,
        unclamped: (f64, f64),
        held: u8,
        press: bool,
    ) -> DragRoute {
        if !unclamped.0.is_finite() || !unclamped.1.is_finite() {
            return DragRoute::Ignore;
        }
        match self.origin {
            Some(origin) if origin != source => {
                // Another window owns the pointer. A press here means its
                // release was lost (the platform delivers every press to the
                // window holding the capture): this window takes over rather
                // than leaving every other window dead.
                if !press {
                    return DragRoute::Ignore;
                }
                self.origin = Some(source);
                self.held = held;
                to_source(source, unclamped)
            }
            Some(origin) => {
                let route = resolve(geometry, origin, unclamped);
                self.held = held;
                if held == 0 {
                    self.origin = None;
                }
                route
            }
            None => {
                if held != 0 {
                    self.origin = Some(source);
                    self.held = held;
                }
                to_source(source, unclamped)
            }
        }
    }
}

fn clamp_fraction((x, y): (f64, f64)) -> (f64, f64) {
    (x.clamp(0.0, 1.0), y.clamp(0.0, 1.0))
}

fn to_source(source: SessionMonitorId, unclamped: (f64, f64)) -> DragRoute {
    DragRoute::To {
        monitor: source,
        fraction: clamp_fraction(unclamped),
    }
}

fn usable(rect: &ImageRect) -> bool {
    [rect.left, rect.top, rect.width, rect.height]
        .iter()
        .all(|value| value.is_finite())
        && rect.width > 0.0
        && rect.height > 0.0
}

fn contains(rect: &ImageRect, (x, y): (f64, f64)) -> bool {
    x >= rect.left && y >= rect.top && x < rect.left + rect.width && y < rect.top + rect.height
}

fn distance_squared(rect: &ImageRect, (x, y): (f64, f64)) -> f64 {
    let dx = (rect.left - x).max(0.0).max(x - (rect.left + rect.width));
    let dy = (rect.top - y).max(0.0).max(y - (rect.top + rect.height));
    dx * dx + dy * dy
}

fn picture_fraction(picture: &ImageRect, (x, y): (f64, f64)) -> (f64, f64) {
    clamp_fraction((
        (x - picture.left) / picture.width,
        (y - picture.top) / picture.height,
    ))
}

/// Where an origin sample lands: the window under it on the Deck's desktop
/// (the nearest picture when it is in none), or the host's layout while
/// the origin cannot be placed.
fn resolve(
    geometry: &DragGeometry<'_>,
    origin: SessionMonitorId,
    unclamped: (f64, f64),
) -> DragRoute {
    let placed: Vec<&PictureSurface> = geometry
        .surfaces
        .iter()
        .filter(|surface| usable(&surface.picture))
        .collect();
    let Some(from) = placed.iter().find(|surface| surface.monitor == origin) else {
        return match arcen_media::resolve_pointer_crossing(geometry.host, origin, unclamped) {
            Some((monitor, fraction)) => DragRoute::To { monitor, fraction },
            None => to_source(origin, unclamped),
        };
    };
    let point = (
        from.picture.left + unclamped.0 * from.picture.width,
        from.picture.top + unclamped.1 * from.picture.height,
    );
    // The window under the cursor owns it, even over its letterbox bars;
    // the origin wins a shared edge.
    let under = std::iter::once(*from)
        .chain(placed.iter().copied())
        .find(|surface| usable(&surface.window) && contains(&surface.window, point));
    let target = under.unwrap_or_else(|| {
        // In no window: a gap between displays, or past the desktop's edge.
        let mut nearest = *from;
        let mut best = distance_squared(&from.picture, point);
        for surface in &placed {
            let distance = distance_squared(&surface.picture, point);
            if distance < best {
                best = distance;
                nearest = *surface;
            }
        }
        nearest
    });
    DragRoute::To {
        monitor: target.monitor,
        fraction: picture_fraction(&target.picture, point),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(value: u16) -> SessionMonitorId {
        SessionMonitorId::new(value).unwrap()
    }

    fn rect(left: f64, top: f64, width: f64, height: f64) -> ImageRect {
        ImageRect {
            left,
            top,
            width,
            height,
        }
    }

    /// A window that fills its display with the picture.
    fn full(monitor: u16, area: ImageRect) -> PictureSurface {
        PictureSurface {
            monitor: id(monitor),
            window: area,
            picture: area,
        }
    }

    fn geometry(surfaces: &[PictureSurface]) -> DragGeometry<'_> {
        DragGeometry {
            surfaces,
            host: &[],
        }
    }

    fn to(monitor: u16, fraction: (f64, f64)) -> DragRoute {
        DragRoute::To {
            monitor: id(monitor),
            fraction,
        }
    }

    fn assert_route(actual: DragRoute, monitor: u16, (x, y): (f64, f64)) {
        let DragRoute::To {
            monitor: got,
            fraction,
        } = actual
        else {
            panic!("expected a route to {monitor}, got {actual:?}");
        };
        assert_eq!(got, id(monitor), "{actual:?}");
        assert!(
            (fraction.0 - x).abs() < 1e-9 && (fraction.1 - y).abs() < 1e-9,
            "{actual:?} != ({x}, {y})"
        );
    }

    /// The owner's Mac: a 2560×1440 main display (a secondary window) and
    /// a 1728×1117 built-in display (the root window) to its right, tops
    /// aligned.
    fn main_and_builtin() -> [PictureSurface; 2] {
        [
            full(1, rect(0.0, 0.0, 2560.0, 1440.0)),
            full(2, rect(2560.0, 0.0, 1728.0, 1117.0)),
        ]
    }

    #[test]
    fn a_drag_started_on_a_secondary_window_crosses_into_the_root_one_and_back() {
        let surfaces = main_and_builtin();
        let geometry = geometry(&surfaces);
        let mut drag = PointerDragCapture::default();
        assert_route(
            drag.button(&geometry, id(1), (0.5, 0.5), 1, true),
            1,
            (0.5, 0.5),
        );
        assert_eq!(drag.origin(), Some(id(1)));
        // 2560 + 864 = the built-in display's middle, in the main window's
        // own (unclamped) fraction.
        let route = drag.motion(&geometry, id(1), (3424.0 / 2560.0, 558.5 / 1440.0));
        assert_route(route, 2, (0.5, 0.5));
        let back = drag.motion(&geometry, id(1), (0.25, 0.25));
        assert_route(back, 1, (0.25, 0.25));
        assert_eq!(drag.origin(), Some(id(1)));
    }

    #[test]
    fn the_release_lands_on_the_monitor_the_drag_ended_over() {
        let surfaces = main_and_builtin();
        let geometry = geometry(&surfaces);
        let mut drag = PointerDragCapture::default();
        let _ = drag.button(&geometry, id(2), (0.5, 0.5), 1, true);
        // From the built-in window, 1280 points left of it: the main
        // display's middle.
        let release = drag.button(
            &geometry,
            id(2),
            (-1280.0 / 1728.0, 720.0 / 1117.0),
            1,
            false,
        );
        assert_route(release, 1, (0.5, 0.5));
        assert_eq!(drag.origin(), None);
        // Free again: each window points into its own monitor.
        assert_route(drag.motion(&geometry, id(1), (0.1, 0.1)), 1, (0.1, 0.1));
    }

    #[test]
    fn only_the_origin_window_moves_the_pointer_while_a_button_is_held() {
        let surfaces = main_and_builtin();
        let geometry = geometry(&surfaces);
        let mut drag = PointerDragCapture::default();
        let _ = drag.button(&geometry, id(1), (0.9, 0.5), 1, true);
        let crossed = drag.motion(&geometry, id(1), (1.1, 0.2));
        assert!(matches!(crossed, DragRoute::To { monitor, .. } if monitor == id(2)));
        // The built-in window sees the pointer too in the same frame: it
        // must not dispatch a second position.
        assert_eq!(drag.motion(&geometry, id(2), (0.2, 0.3)), DragRoute::Ignore);
        assert_eq!(
            drag.sample(&geometry, id(2), (0.2, 0.3), 0),
            DragRoute::Ignore
        );
        assert_eq!(
            drag.button(&geometry, id(2), (0.2, 0.3), 1, false),
            DragRoute::Ignore
        );
        assert_eq!(drag.origin(), Some(id(1)));
    }

    #[test]
    fn displays_left_right_above_and_below_are_followed() {
        let centre = full(1, rect(0.0, 0.0, 1000.0, 1000.0));
        for (neighbour, point, expected) in [
            (
                rect(-500.0, 0.0, 500.0, 1000.0),
                (-250.0, 500.0),
                (0.5, 0.5),
            ),
            (
                rect(1000.0, 0.0, 500.0, 1000.0),
                (1250.0, 500.0),
                (0.5, 0.5),
            ),
            (
                rect(0.0, -400.0, 1000.0, 400.0),
                (500.0, -100.0),
                (0.5, 0.75),
            ),
            (
                rect(250.0, 1000.0, 500.0, 400.0),
                (500.0, 1100.0),
                (0.5, 0.25),
            ),
        ] {
            let surfaces = [centre, full(2, neighbour)];
            let geometry = geometry(&surfaces);
            let mut drag = PointerDragCapture::default();
            let _ = drag.button(&geometry, id(1), (0.5, 0.5), 1, true);
            let route = drag.motion(&geometry, id(1), (point.0 / 1000.0, point.1 / 1000.0));
            assert_route(route, 2, expected);
        }
    }

    #[test]
    fn a_gap_or_the_desktop_edge_clamps_to_the_nearest_picture_instead_of_stopping() {
        // A 100-point gap between the displays, and a smaller second one.
        let surfaces = [
            full(1, rect(0.0, 0.0, 1000.0, 1000.0)),
            full(2, rect(1100.0, 200.0, 500.0, 500.0)),
        ];
        let geometry = geometry(&surfaces);
        let mut drag = PointerDragCapture::default();
        let _ = drag.button(&geometry, id(1), (0.5, 0.5), 1, true);
        // In the gap, nearer the second display: its left edge.
        assert_route(drag.motion(&geometry, id(1), (1.08, 0.45)), 2, (0.0, 0.5));
        // In the gap, nearer the origin: the origin's right edge.
        assert_route(drag.motion(&geometry, id(1), (1.02, 0.45)), 1, (1.0, 0.45));
        // Far past the second display: its far edge, not nowhere.
        assert_route(drag.motion(&geometry, id(1), (5.0, 0.45)), 2, (1.0, 0.5));
    }

    #[test]
    fn a_letterboxed_window_owns_its_bars_and_clamps_into_its_picture() {
        let surfaces = [
            full(1, rect(0.0, 0.0, 1000.0, 1000.0)),
            PictureSurface {
                monitor: id(2),
                window: rect(1000.0, 0.0, 1000.0, 1000.0),
                picture: rect(1000.0, 250.0, 1000.0, 500.0),
            },
        ];
        let geometry = geometry(&surfaces);
        let mut drag = PointerDragCapture::default();
        let _ = drag.button(&geometry, id(1), (0.5, 0.5), 1, true);
        // On the second display's top bar: its picture's top edge.
        assert_route(drag.motion(&geometry, id(1), (1.5, 0.1)), 2, (0.5, 0.0));
    }

    #[test]
    fn a_rotated_portrait_display_is_followed_in_its_own_orientation() {
        // A display turned to portrait: 1080 wide, 1920 tall, to the right,
        // vertically centred on a 1920×1080 landscape one.
        let surfaces = [
            full(1, rect(0.0, 0.0, 1920.0, 1080.0)),
            full(2, rect(1920.0, -420.0, 1080.0, 1920.0)),
        ];
        let geometry = geometry(&surfaces);
        let mut drag = PointerDragCapture::default();
        let _ = drag.button(&geometry, id(1), (0.5, 0.0), 1, true);
        // Above the landscape display but beside the portrait one's top.
        let route = drag.motion(&geometry, id(1), (2460.0 / 1920.0, -300.0 / 1080.0));
        assert_route(route, 2, (0.5, 120.0 / 1920.0));
    }

    #[test]
    fn without_deck_geometry_the_host_layout_decides() {
        let host = [
            (
                id(1),
                MonitorDesktopRect {
                    x: 0,
                    y: 0,
                    width_px: 1920,
                    height_px: 1080,
                },
            ),
            (
                id(2),
                MonitorDesktopRect {
                    x: -1920,
                    y: 0,
                    width_px: 1920,
                    height_px: 1080,
                },
            ),
        ];
        let geometry = DragGeometry {
            surfaces: &[],
            host: &host,
        };
        let mut drag = PointerDragCapture::default();
        let _ = drag.button(&geometry, id(1), (0.1, 0.5), 1, true);
        let route = drag.motion(&geometry, id(1), (-0.5, 0.5));
        assert!(matches!(route, DragRoute::To { monitor, .. } if monitor == id(2)));
        // A point the host layout does not hold: the origin's edge.
        assert_route(drag.motion(&geometry, id(1), (0.5, 5.0)), 1, (0.5, 1.0));
    }

    #[test]
    fn a_free_pointer_stays_in_its_window_and_is_clamped_there() {
        let surfaces = main_and_builtin();
        let geometry = geometry(&surfaces);
        let mut drag = PointerDragCapture::default();
        assert_eq!(
            drag.motion(&geometry, id(2), (-0.2, 1.3)),
            to(2, (0.0, 1.0))
        );
        assert_eq!(
            drag.motion(&geometry, id(1), (f64::NAN, 0.5)),
            DragRoute::Ignore
        );
        assert_eq!(drag.origin(), None);
    }

    #[test]
    fn the_capture_lasts_until_the_last_button_and_a_lost_release_is_recovered() {
        let surfaces = main_and_builtin();
        let geometry = geometry(&surfaces);
        let mut drag = PointerDragCapture::default();
        let _ = drag.button(&geometry, id(1), (0.5, 0.5), 1, true);
        let _ = drag.button(&geometry, id(1), (0.5, 0.5), 3, true);
        let _ = drag.button(&geometry, id(1), (0.5, 0.5), 1, false);
        assert_eq!(drag.origin(), Some(id(1)));
        let _ = drag.button(&geometry, id(1), (0.5, 0.5), 3, false);
        assert_eq!(drag.origin(), None);

        // The origin's release never arrived: a press of the same button in
        // another window takes the pointer instead of leaving it dead, for
        // an edge-reading Deck and a state-reading one alike.
        let left = PointerDragCapture::button_bit(1);
        let _ = drag.button(&geometry, id(1), (0.5, 0.5), 1, true);
        assert_eq!(
            drag.button(&geometry, id(2), (0.3, 0.3), 1, false),
            DragRoute::Ignore
        );
        assert_eq!(
            drag.button(&geometry, id(2), (0.3, 0.3), 1, true),
            to(2, (0.3, 0.3))
        );
        assert_eq!(drag.origin(), Some(id(2)));
        let _ = drag.sample(&geometry, id(1), (0.6, 0.6), left);
        assert_eq!(drag.origin(), Some(id(1)));
        drag.reset();
        assert_eq!(drag.origin(), None);
        assert_eq!(drag.held(), 0);
    }
}
