// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The layout of another product's desktop when a Deck attaches (ADR 0025
//! §8, amended 2026-11-25).
//!
//! A remote-desktop product that owns the console lays its desktop out for
//! its own client at each connect and leaves that layout when the client
//! goes. A Deck that attaches later found the desktop in the last client's
//! layout and saw each of its monitors fitted, letterboxed when the shapes
//! differ. Arcen now does what that product does: while nobody uses the
//! desktop through it, the desktop takes the Deck's monitors on attach, the
//! layout it had is journalled, and the Deck's layout stays when the Deck
//! leaves (the session is still locked, never ended).
//!
//! Arcen never changes the desktop while the other product's client is
//! attached, nor when it cannot tell whether one is. Then, and when the
//! administrator keeps the other product's layout
//! (`platform.session.guest_layout = "keep"`), the desktop is shown as it
//! is and each Deck monitor is fitted to the output in its place.
//!
//! The decision is [`guest_layout_policy`]; the platform adapter gathers
//! the facts (who owns the console, whether that product's client is
//! attached, the setting) and lays the desktop out or reads it.

use serde::{Deserialize, Serialize};

use super::ConsoleOrigin;

/// What the administrator chose for another product's desktop
/// (`platform.session.guest_layout`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GuestLayoutSetting {
    /// Lay the desktop out to the Deck's monitors on attach, while the
    /// other product has no client attached. The default.
    #[default]
    Deck,
    /// Never change the other product's layout: each Deck monitor shows
    /// the output in its place, fitted.
    Keep,
}

impl GuestLayoutSetting {
    /// Every setting, in configuration order.
    pub const ALL: [Self; 2] = [Self::Deck, Self::Keep];

    /// The configuration token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Deck => "deck",
            Self::Keep => "keep",
        }
    }

    /// Parses [`Self::token`].
    #[must_use]
    pub fn from_token(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|setting| setting.token() == value)
    }
}

/// Whether the other product's own client is attached to its desktop now,
/// as far as the host can tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForeignClient {
    /// Its client is attached (on Linux, input devices it made in user
    /// space: [`super::another_products_client_input`]).
    Attached,
    /// The host proved no client is attached.
    Absent,
    /// The check could not run or could not be completed.
    Unknown,
}

impl ForeignClient {
    /// Stable token for logs.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Attached => "attached",
            Self::Absent => "absent",
            Self::Unknown => "unknown",
        }
    }
}

/// Why another product's desktop is shown as it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShownBecause {
    /// The administrator keeps the other product's layout.
    SettingKeep,
    /// The other product's client is attached.
    ClientAttached,
    /// Whether its client is attached is not known.
    ClientUnknown,
}

impl ShownBecause {
    /// Stable token for logs.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::SettingKeep => "guest_layout_keep",
            Self::ClientAttached => "foreign_client_attached",
            Self::ClientUnknown => "foreign_client_unknown",
        }
    }
}

/// What happens to the layout of another product's desktop when a Deck
/// attaches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuestLayoutPolicy {
    /// Read, never changed: each Deck monitor shows the output in its
    /// place, fitted.
    Shown(ShownBecause),
    /// Laid out to the Deck's monitors (the layout it had journalled
    /// first); the Deck's layout stays when the Deck leaves.
    ApplyOnAttach,
}

impl GuestLayoutPolicy {
    /// Stable token for logs.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Shown(because) => because.token(),
            Self::ApplyOnAttach => "apply_on_attach",
        }
    }
}

/// What happens to a layout Arcen laid on a console when the Deck leaves
/// (the hand-back, a resume hold that ends, or the Pier's recovery after a
/// launcher that did not hand it back).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LayoutDeparture {
    /// The console's own layout, journalled before, is put back (ADR 0025
    /// §6). The default, and what a journal without the field means.
    #[default]
    RestoreOwn,
    /// The Deck's layout stays and the journal is kept only as a record:
    /// another product lays its desktop out again for its own client, and
    /// a layout put back under a client that is connecting would fight it.
    /// Such a layout is never scaled either (`arcen_outputs`
    /// `HeadScaling::NativeOnly`): the product keeps a head's transform
    /// when it lays the desktop out again.
    LeaveDecks,
}

/// [`LayoutDeparture`] for a console started by `origin`.
#[must_use]
pub const fn layout_departure(origin: ConsoleOrigin) -> LayoutDeparture {
    match origin {
        ConsoleOrigin::OtherProduct => LayoutDeparture::LeaveDecks,
        ConsoleOrigin::DisplayManager | ConsoleOrigin::Arcen => LayoutDeparture::RestoreOwn,
    }
}

/// The facts [`guest_layout_policy`] decides from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuestLayoutFacts {
    /// Who started the console session.
    pub origin: ConsoleOrigin,
    /// Whether the other product's client is attached.
    pub foreign_client: ForeignClient,
    /// `platform.session.guest_layout`.
    pub setting: GuestLayoutSetting,
}

/// The layout policy for a console another product owns; `None` for a
/// console that is not another product's (the display manager's, or
/// Arcen's own login session), which Arcen lays out for the Deck and puts
/// back when it leaves (ADR 0025 §6).
///
/// Only a proved absent client lets Arcen change the desktop; an attached
/// client or an unknown answer shows it as it is.
#[must_use]
pub const fn guest_layout_policy(facts: GuestLayoutFacts) -> Option<GuestLayoutPolicy> {
    if !matches!(facts.origin, ConsoleOrigin::OtherProduct) {
        return None;
    }
    Some(match (facts.foreign_client, facts.setting) {
        (ForeignClient::Attached, _) => GuestLayoutPolicy::Shown(ShownBecause::ClientAttached),
        (ForeignClient::Unknown, _) => GuestLayoutPolicy::Shown(ShownBecause::ClientUnknown),
        (ForeignClient::Absent, GuestLayoutSetting::Keep) => {
            GuestLayoutPolicy::Shown(ShownBecause::SettingKeep)
        }
        (ForeignClient::Absent, GuestLayoutSetting::Deck) => GuestLayoutPolicy::ApplyOnAttach,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(
        origin: ConsoleOrigin,
        foreign_client: ForeignClient,
        setting: GuestLayoutSetting,
    ) -> GuestLayoutFacts {
        GuestLayoutFacts {
            origin,
            foreign_client,
            setting,
        }
    }

    #[test]
    fn another_products_idle_desktop_takes_the_decks_layout_by_default() {
        let policy = guest_layout_policy(facts(
            ConsoleOrigin::OtherProduct,
            ForeignClient::Absent,
            GuestLayoutSetting::default(),
        ));
        assert_eq!(policy, Some(GuestLayoutPolicy::ApplyOnAttach));
    }

    #[test]
    fn only_another_products_desktop_keeps_the_decks_layout_when_the_deck_leaves() {
        assert_eq!(
            layout_departure(ConsoleOrigin::OtherProduct),
            LayoutDeparture::LeaveDecks
        );
        for origin in [ConsoleOrigin::DisplayManager, ConsoleOrigin::Arcen] {
            assert_eq!(layout_departure(origin), LayoutDeparture::RestoreOwn);
        }
    }

    #[test]
    fn an_attached_or_unknown_client_keeps_the_desktop_as_it_is_whatever_the_setting() {
        for setting in GuestLayoutSetting::ALL {
            assert_eq!(
                guest_layout_policy(facts(
                    ConsoleOrigin::OtherProduct,
                    ForeignClient::Attached,
                    setting
                )),
                Some(GuestLayoutPolicy::Shown(ShownBecause::ClientAttached))
            );
            assert_eq!(
                guest_layout_policy(facts(
                    ConsoleOrigin::OtherProduct,
                    ForeignClient::Unknown,
                    setting
                )),
                Some(GuestLayoutPolicy::Shown(ShownBecause::ClientUnknown))
            );
        }
    }

    #[test]
    fn the_administrator_can_keep_the_other_products_layout() {
        assert_eq!(
            guest_layout_policy(facts(
                ConsoleOrigin::OtherProduct,
                ForeignClient::Absent,
                GuestLayoutSetting::Keep
            )),
            Some(GuestLayoutPolicy::Shown(ShownBecause::SettingKeep))
        );
    }

    #[test]
    fn a_console_that_is_not_another_products_has_no_guest_policy() {
        for origin in [ConsoleOrigin::DisplayManager, ConsoleOrigin::Arcen] {
            for client in [
                ForeignClient::Attached,
                ForeignClient::Absent,
                ForeignClient::Unknown,
            ] {
                assert_eq!(
                    guest_layout_policy(facts(origin, client, GuestLayoutSetting::Deck)),
                    None
                );
            }
        }
    }

    #[test]
    fn the_setting_reads_and_writes_its_tokens() {
        for setting in GuestLayoutSetting::ALL {
            assert_eq!(
                GuestLayoutSetting::from_token(setting.token()),
                Some(setting)
            );
            let json = serde_json::to_string(&setting).unwrap();
            assert_eq!(json, format!("\"{}\"", setting.token()));
            assert_eq!(
                serde_json::from_str::<GuestLayoutSetting>(&json).unwrap(),
                setting
            );
        }
        assert_eq!(GuestLayoutSetting::from_token("apply"), None);
    }
}
