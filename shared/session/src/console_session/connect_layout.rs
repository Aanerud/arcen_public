// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Whoever connects gets their own Deck's layout (ADR 0025 §6, amended
//! 2026-11-25).
//!
//! A Deck that matches its layout (Match My Layout) is laid out to its own
//! monitors on every connection, whatever an earlier connection left: a
//! fresh sign-in, a sign-in that finds the console held for an earlier
//! Deck, the sign-in hand-over, a console Arcen owns and one another
//! product owns. Native modes only on another product's desktop (§8), and
//! never while that product's client is attached.
//!
//! [`console_layout_ask`] decides which layout a joined console is asked
//! to take for the connecting Deck. Nothing an earlier connection left
//! decides it:
//!
//! - A sign-in never reattaches a console held for an earlier Deck: the
//!   same account's sign-in takes the held session over
//!   ([`crate::direct_reconnect::fresh_sign_in`]), which hands the console
//!   back ([`super::detached_hold::after_deck_left`]), and the sign-in joins
//!   it afresh with its own layout.
//! - A resume continues the attachment it resumes: its grant is bound to
//!   that attachment's whole layout request (each Pier's resume
//!   `TopologyBinding`), so a Deck whose layout changed signs in afresh.
//! - A layout journal only records what to put back; it never decides what
//!   is laid.

use super::ConsoleOrigin;

/// What the connecting Deck asks of the console's layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeckLayout {
    /// Match My Layout with several monitors: a committed plan of the
    /// console's own outputs.
    Monitors,
    /// Match My Layout with one display.
    MatchOneDisplay,
    /// One display that does not match a layout (Primary Display Only,
    /// windowed, an older Deck).
    OneDisplay,
    /// Nothing the console could take (no display facts, or a plan for
    /// outputs that are not the console's).
    Nothing,
}

/// The layout a joined console is asked to take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsoleLayoutAsk {
    /// Every monitor of the Deck's plan.
    Monitors,
    /// The one served Deck display, at its size.
    OneDisplay,
    /// None: the console is served as one picture at its own size (§6),
    /// or another product's desktop as it shows it (§8).
    Keep,
}

impl ConsoleLayoutAsk {
    /// Stable token for logs.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Monitors => "monitors",
            Self::OneDisplay => "one_display",
            Self::Keep => "keep",
        }
    }
}

/// What a joined console started by `origin` is asked to take for `deck`.
/// `forced_outputs`: the console runs the installer's forced virtual
/// outputs (a headless console whose heads are Arcen's own).
///
/// A plan of several monitors is always asked for. One display is asked
/// for where the console is laid out for one display at all: Arcen's
/// forced heads (any one-display Deck, §6), and another product's desktop
/// for a Deck that matches its layout, whose previous client left that
/// product's layout (or an earlier Deck's) behind (§8). A console at a
/// desk otherwise keeps its own layout for a one-display Deck, as it has
/// after every earlier Deck.
#[must_use]
pub const fn console_layout_ask(
    origin: ConsoleOrigin,
    deck: DeckLayout,
    forced_outputs: bool,
) -> ConsoleLayoutAsk {
    match (deck, origin) {
        (DeckLayout::Monitors, _) => ConsoleLayoutAsk::Monitors,
        (DeckLayout::MatchOneDisplay, ConsoleOrigin::OtherProduct) => ConsoleLayoutAsk::OneDisplay,
        (DeckLayout::OneDisplay, ConsoleOrigin::OtherProduct) | (DeckLayout::Nothing, _) => {
            ConsoleLayoutAsk::Keep
        }
        (
            DeckLayout::MatchOneDisplay | DeckLayout::OneDisplay,
            ConsoleOrigin::DisplayManager | ConsoleOrigin::Arcen,
        ) => {
            if forced_outputs {
                ConsoleLayoutAsk::OneDisplay
            } else {
                ConsoleLayoutAsk::Keep
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DESK_OWNED: [ConsoleOrigin; 2] = [ConsoleOrigin::DisplayManager, ConsoleOrigin::Arcen];

    #[test]
    fn several_monitors_are_always_laid_out_on_every_console() {
        for origin in [
            ConsoleOrigin::DisplayManager,
            ConsoleOrigin::Arcen,
            ConsoleOrigin::OtherProduct,
        ] {
            for forced in [false, true] {
                assert_eq!(
                    console_layout_ask(origin, DeckLayout::Monitors, forced),
                    ConsoleLayoutAsk::Monitors
                );
            }
        }
    }

    #[test]
    fn another_products_desktop_takes_a_one_display_decks_layout_only_when_it_matches_its_layout() {
        for forced in [false, true] {
            assert_eq!(
                console_layout_ask(
                    ConsoleOrigin::OtherProduct,
                    DeckLayout::MatchOneDisplay,
                    forced
                ),
                ConsoleLayoutAsk::OneDisplay
            );
            assert_eq!(
                console_layout_ask(ConsoleOrigin::OtherProduct, DeckLayout::OneDisplay, forced),
                ConsoleLayoutAsk::Keep
            );
        }
    }

    #[test]
    fn a_desk_console_keeps_its_own_layout_for_one_display_unless_its_heads_are_arcens() {
        for origin in DESK_OWNED {
            for deck in [DeckLayout::MatchOneDisplay, DeckLayout::OneDisplay] {
                assert_eq!(
                    console_layout_ask(origin, deck, false),
                    ConsoleLayoutAsk::Keep
                );
                assert_eq!(
                    console_layout_ask(origin, deck, true),
                    ConsoleLayoutAsk::OneDisplay
                );
            }
            assert_eq!(
                console_layout_ask(origin, DeckLayout::Nothing, true),
                ConsoleLayoutAsk::Keep
            );
        }
    }

    #[test]
    fn a_sign_in_never_finds_a_console_held_for_an_earlier_deck() {
        use super::super::detached_hold::{AfterDeckLeft, after_deck_left};
        use crate::direct_reconnect::{
            AccountPrincipal, FreshSignIn, SessionAttachment, fresh_sign_in,
        };
        let account = AccountPrincipal {
            name: "alice",
            id: 1000,
        };
        assert_eq!(
            fresh_sign_in(account, account, SessionAttachment::Detached),
            FreshSignIn::TakeOverDetached,
            "the same account's sign-in takes a held session over"
        );
        assert_eq!(
            after_deck_left(true, false),
            AfterDeckLeft::LeaveConsole,
            "and the console it held is handed back, so the sign-in joins it afresh"
        );
    }
}
