// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Who gets the pointer when a Deck draws its own UI over a session: the
//! host, the Deck's UI, or both.
//!
//! A session window sends pointer input to the host before its UI toolkit
//! sees it (input never waits for the UI pass). Fullscreen draws nothing
//! over the video, so the Deck's own controls appear only on demand: the
//! connection bar drops down when the pointer reaches the top edge above it
//! (the *reveal* strip), and while it is open the pointer over it (or, with
//! one of its menus open, anywhere) belongs to the Deck. A platform adapter
//! publishes these regions in window pixels once per UI pass and asks
//! [`route_pointer`] for every pointer message.

use arcen_input::deck_pen::ImageRect;

/// The parts of a session window where the Deck's own UI takes the
/// pointer, in window pixels (top-left origin).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct OverlayPointerRegions {
    /// Moves here reach the host and the Deck's UI (the UI decides to open
    /// the connection bar); presses and the wheel stay the host's.
    pub reveal: Option<ImageRect>,
    /// The pointer here is the Deck's UI only (the open connection bar).
    pub owned: Option<ImageRect>,
    /// The Deck's UI has the whole window (a menu of the bar is open).
    pub whole_window: bool,
}

/// The kind of pointer message to route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerMessage {
    Move,
    Press,
    Release,
    Wheel,
}

/// Which side a held button's press went to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeldBy {
    Host,
    Ui,
}

/// Where one pointer message goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PointerRoute {
    /// The host receives it.
    pub host: bool,
    /// The Deck's UI receives it.
    pub ui: bool,
}

impl PointerRoute {
    const HOST: Self = Self {
        host: true,
        ui: false,
    };
    const UI: Self = Self {
        host: false,
        ui: true,
    };
    const BOTH: Self = Self {
        host: true,
        ui: true,
    };
}

fn contains(rect: Option<ImageRect>, x: f64, y: f64) -> bool {
    rect.is_some_and(|rect| {
        x >= rect.left && y >= rect.top && x < rect.left + rect.width && y < rect.top + rect.height
    })
}

/// Whether window point (`x`, `y`) is on the Deck's UI: the top-edge strip,
/// the open bar, or anywhere while one of its menus is open. A platform
/// adapter keeps this for the last move and passes it to [`route_pointer`]
/// as `ui_had_pointer`.
#[must_use]
pub fn over_ui(regions: OverlayPointerRegions, x: f64, y: f64) -> bool {
    regions.whole_window || contains(regions.owned, x, y) || contains(regions.reveal, x, y)
}

/// Routes one pointer `message` at window point (`x`, `y`).
///
/// `held`: which side a button press still held went to; until its
/// release, that side keeps the pointer (a drag in the picture may cross the
/// bar, a press on the bar may be released off it). `ui_had_pointer`: the
/// last move was on the UI; the first move away from it goes there too, so
/// the UI sees the pointer leave and closes the bar.
#[must_use]
pub fn route_pointer(
    regions: OverlayPointerRegions,
    message: PointerMessage,
    x: f64,
    y: f64,
    held: Option<HeldBy>,
    ui_had_pointer: bool,
) -> PointerRoute {
    match held {
        Some(HeldBy::Host) => return PointerRoute::HOST,
        Some(HeldBy::Ui) => return PointerRoute::UI,
        None => {}
    }
    if regions.whole_window || contains(regions.owned, x, y) {
        return PointerRoute::UI;
    }
    if message == PointerMessage::Move && (ui_had_pointer || contains(regions.reveal, x, y)) {
        return PointerRoute::BOTH;
    }
    PointerRoute::HOST
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn rect(left: f64, top: f64, width: f64, height: f64) -> ImageRect {
        ImageRect {
            left,
            top,
            width,
            height,
        }
    }

    const REVEAL: OverlayPointerRegions = OverlayPointerRegions {
        reveal: Some(ImageRect {
            left: 720.0,
            top: 0.0,
            width: 360.0,
            height: 5.0,
        }),
        owned: None,
        whole_window: false,
    };

    #[test]
    fn the_top_edge_strip_shows_the_ui_the_pointer_and_keeps_the_host_in_charge() {
        let at = |message| route_pointer(REVEAL, message, 900.0, 1.0, None, false);
        assert_eq!(at(PointerMessage::Move), PointerRoute::BOTH);
        assert_eq!(at(PointerMessage::Press), PointerRoute::HOST);
        assert_eq!(at(PointerMessage::Wheel), PointerRoute::HOST);
        // Beside or below the strip: the host's only.
        for (x, y) in [(500.0, 0.0), (900.0, 5.0), (1080.0, 0.0)] {
            assert_eq!(
                route_pointer(REVEAL, PointerMessage::Move, x, y, None, false),
                PointerRoute::HOST,
                "({x}, {y})"
            );
        }
    }

    #[test]
    fn the_open_bar_owns_the_pointer_over_it_and_a_menu_owns_the_window() {
        let open = OverlayPointerRegions {
            owned: Some(rect(700.0, 0.0, 400.0, 30.0)),
            ..REVEAL
        };
        for message in [
            PointerMessage::Move,
            PointerMessage::Press,
            PointerMessage::Release,
            PointerMessage::Wheel,
        ] {
            assert_eq!(
                route_pointer(open, message, 750.0, 20.0, None, true),
                PointerRoute::UI
            );
        }
        let menu = OverlayPointerRegions {
            whole_window: true,
            ..open
        };
        assert_eq!(
            route_pointer(menu, PointerMessage::Press, 100.0, 900.0, None, true),
            PointerRoute::UI
        );
    }

    #[test]
    fn the_ui_has_the_pointer_in_any_of_its_regions() {
        assert!(over_ui(REVEAL, 900.0, 0.0));
        assert!(!over_ui(REVEAL, 900.0, 400.0));
        let open = OverlayPointerRegions {
            owned: Some(rect(700.0, 0.0, 400.0, 30.0)),
            ..REVEAL
        };
        assert!(over_ui(open, 750.0, 20.0));
        assert!(over_ui(
            OverlayPointerRegions {
                whole_window: true,
                ..REVEAL
            },
            10.0,
            900.0
        ));
    }

    #[test]
    fn leaving_the_ui_is_shown_to_it_once_and_a_held_button_stays_with_its_side() {
        // The first move away from the UI reaches both; later moves the host.
        assert_eq!(
            route_pointer(REVEAL, PointerMessage::Move, 900.0, 400.0, None, true),
            PointerRoute::BOTH
        );
        assert_eq!(
            route_pointer(REVEAL, PointerMessage::Move, 900.0, 410.0, None, false),
            PointerRoute::HOST
        );
        // A press on the bar dragged off it: moves and the release stay the UI's.
        for message in [PointerMessage::Move, PointerMessage::Release] {
            assert_eq!(
                route_pointer(REVEAL, message, 900.0, 600.0, Some(HeldBy::Ui), false),
                PointerRoute::UI
            );
        }
        // A drag that began in the picture keeps going to the host, bar or not.
        let menu = OverlayPointerRegions {
            whole_window: true,
            ..REVEAL
        };
        for message in [PointerMessage::Move, PointerMessage::Release] {
            assert_eq!(
                route_pointer(menu, message, 900.0, 1.0, Some(HeldBy::Host), false),
                PointerRoute::HOST
            );
        }
    }
}
