// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS

use crate::{
    CapabilityAvailability, CursorMode, CursorModeNegotiation, TabletMode, TabletModeNegotiation,
};

/// Human-readable result of cursor-authority negotiation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CursorModeNegotiationReason {
    /// The requested authority is active.
    #[default]
    Accepted,
    /// The client asked the host to draw the cursor, but the host cannot.
    HostCursorUnavailable,
    /// The automatic cursor: the host sends no cursor images, so it draws
    /// the pointer into the video and the Deck shows none
    /// ([`resolve_automatic_cursor_mode`]).
    NoCursorImages,
}

impl CursorModeNegotiationReason {
    /// Stable product-facing explanation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "",
            Self::HostCursorUnavailable => {
                "host cursor mode is unavailable; client-rendered cursor remains active"
            }
            Self::NoCursorImages => {
                "automatic cursor: host sends no cursor images, so it draws the pointer into the video"
            }
        }
    }
}

/// Cursor-authority result plus a stable explanation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedCursorMode {
    /// Requested cursor authority.
    pub requested: CursorMode,
    /// Cursor authority that can truthfully be active.
    pub active: CursorMode,
    /// Whether the request was accepted exactly.
    pub accepted: bool,
    /// Why the request was not accepted.
    pub reason: CursorModeNegotiationReason,
}

impl From<ResolvedCursorMode> for CursorModeNegotiation {
    fn from(resolved: ResolvedCursorMode) -> Self {
        Self {
            requested: resolved.requested,
            active: resolved.active,
            accepted: resolved.accepted,
        }
    }
}

/// The automatic cursor, the one decision for a Deck that leaves the
/// pointer to Arcen (`cursor_automatic_v1`): when the host sends cursor
/// images, the Deck draws the pointer, which moves at the speed of the hand
/// and has the host's shape; when it sends none, the host draws the pointer
/// into the video if it can, so the shape is right, and the Deck shows none.
/// A host that can do neither leaves the Deck's arrow.
///
/// `requested` is the Deck's stored request, which only hosts older than the
/// automatic cursor serve; it is echoed and does not change the outcome.
/// Every automatic result is `accepted`: the Deck asked for exactly this
/// rule.
#[must_use]
pub const fn resolve_automatic_cursor_mode(
    requested: CursorMode,
    host_cursor: CapabilityAvailability,
    host_sends_cursor_images: bool,
) -> ResolvedCursorMode {
    let (active, reason) = match (host_sends_cursor_images, host_cursor) {
        (true, _) => (CursorMode::Local, CursorModeNegotiationReason::Accepted),
        (false, CapabilityAvailability::Available) => (
            CursorMode::Host,
            CursorModeNegotiationReason::NoCursorImages,
        ),
        (false, CapabilityAvailability::Unavailable | CapabilityAvailability::Unknown) => (
            CursorMode::Local,
            CursorModeNegotiationReason::HostCursorUnavailable,
        ),
    };
    ResolvedCursorMode {
        requested,
        active,
        accepted: true,
        reason,
    }
}

/// Human-readable result of tablet-mode negotiation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TabletModeNegotiationReason {
    /// The requested mode is active.
    #[default]
    Accepted,
    /// The client did not advertise local tablet termination.
    ClientLocalTerminationUnavailable,
    /// The host cannot inject local tablet/pen events.
    HostLocalTerminationUnavailable,
    /// Native USB tablet bridging is unavailable.
    NativeTabletUnavailable,
    /// Mouse compatibility mode could not be applied.
    MouseCompatibilityUnavailable,
}

impl TabletModeNegotiationReason {
    /// Stable product-facing explanation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "",
            Self::ClientLocalTerminationUnavailable => {
                "local tablet termination unavailable: client did not advertise detected tablet/input-v3 pen support"
            }
            Self::HostLocalTerminationUnavailable => {
                "local tablet termination unavailable: host pen backend did not initialize"
            }
            Self::NativeTabletUnavailable => {
                "Native tablet (USB bridged) is unavailable on this host: it needs a USB virtualization backend and a host driver for the tablet. Use Tablet support instead; it needs no host driver and works over any network."
            }
            Self::MouseCompatibilityUnavailable => {
                "mouse compatibility mode negotiation failed unexpectedly"
            }
        }
    }
}

/// Tablet-mode result plus a stable explanation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedTabletMode {
    /// Requested tablet mode.
    pub requested: TabletMode,
    /// Tablet mode that can truthfully be active.
    pub active: TabletMode,
    /// Whether the request was accepted exactly.
    pub accepted: bool,
    /// Why the request was not accepted.
    pub reason: TabletModeNegotiationReason,
    /// Whether the Deck should tell the operator that changing this requires a reconnect.
    pub reconnect_required: bool,
}

impl From<ResolvedTabletMode> for TabletModeNegotiation {
    fn from(resolved: ResolvedTabletMode) -> Self {
        Self {
            requested: resolved.requested,
            active: resolved.active,
            accepted: resolved.accepted,
        }
    }
}

/// Matches a cursor request to proven host cursor capability and explains downgrades.
#[must_use]
pub const fn resolve_cursor_mode(
    requested: CursorMode,
    host_cursor: CapabilityAvailability,
) -> ResolvedCursorMode {
    match (requested, host_cursor) {
        (CursorMode::Host, CapabilityAvailability::Available) => ResolvedCursorMode {
            requested,
            active: CursorMode::Host,
            accepted: true,
            reason: CursorModeNegotiationReason::Accepted,
        },
        (CursorMode::Host, _) => ResolvedCursorMode {
            requested,
            active: CursorMode::Local,
            accepted: false,
            reason: CursorModeNegotiationReason::HostCursorUnavailable,
        },
        (CursorMode::Local, _) => ResolvedCursorMode {
            requested,
            active: CursorMode::Local,
            accepted: true,
            reason: CursorModeNegotiationReason::Accepted,
        },
    }
}

/// Matches a tablet-mode request to proven endpoint capability truth and explains downgrades.
#[must_use]
pub const fn resolve_tablet_mode(
    requested: TabletMode,
    client_local_termination: CapabilityAvailability,
    host_local_termination: CapabilityAvailability,
    client_usb_tablet_bridge: CapabilityAvailability,
    host_usb_tablet_bridge: CapabilityAvailability,
) -> ResolvedTabletMode {
    let local_termination =
        super::mutual_capability(client_local_termination, host_local_termination);
    let usb_tablet_bridge =
        super::mutual_capability(client_usb_tablet_bridge, host_usb_tablet_bridge);
    let negotiation = super::negotiate_tablet_mode(requested, local_termination, usb_tablet_bridge);
    let reason = if negotiation.accepted {
        TabletModeNegotiationReason::Accepted
    } else {
        match requested {
            TabletMode::LocalTermination
                if !matches!(client_local_termination, CapabilityAvailability::Available) =>
            {
                TabletModeNegotiationReason::ClientLocalTerminationUnavailable
            }
            TabletMode::LocalTermination => {
                TabletModeNegotiationReason::HostLocalTerminationUnavailable
            }
            TabletMode::UsbTabletBridge => TabletModeNegotiationReason::NativeTabletUnavailable,
            TabletMode::DisabledMouseCompat => {
                TabletModeNegotiationReason::MouseCompatibilityUnavailable
            }
        }
    };
    ResolvedTabletMode {
        requested,
        active: negotiation.active,
        accepted: negotiation.accepted,
        reason,
        reconnect_required: matches!(requested, TabletMode::UsbTabletBridge)
            && !negotiation.accepted,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_automatic_cursor_is_the_decks_with_images_and_the_hosts_without() {
        // The stored request is echoed and never changes the outcome.
        for requested in [CursorMode::Local, CursorMode::Host] {
            for host_cursor in [
                CapabilityAvailability::Available,
                CapabilityAvailability::Unavailable,
                CapabilityAvailability::Unknown,
            ] {
                let images = resolve_automatic_cursor_mode(requested, host_cursor, true);
                assert_eq!(images.requested, requested);
                assert_eq!(images.active, CursorMode::Local, "{host_cursor:?}");
                assert_eq!(images.reason, CursorModeNegotiationReason::Accepted);
                assert!(images.accepted);
            }
            let without =
                resolve_automatic_cursor_mode(requested, CapabilityAvailability::Available, false);
            assert_eq!(without.requested, requested);
            assert_eq!(without.active, CursorMode::Host);
            assert!(without.accepted);
            assert_eq!(without.reason, CursorModeNegotiationReason::NoCursorImages);
        }
        // The reason travels as a bounded string (`CursorModeReason`).
        assert!(CursorModeNegotiationReason::NoCursorImages.as_str().len() <= 160);
        for host_cursor in [
            CapabilityAvailability::Unavailable,
            CapabilityAvailability::Unknown,
        ] {
            let arrow = resolve_automatic_cursor_mode(CursorMode::Local, host_cursor, false);
            assert_eq!(arrow.active, CursorMode::Local);
            assert_eq!(
                arrow.reason,
                CursorModeNegotiationReason::HostCursorUnavailable
            );
        }
    }

    #[test]
    fn cursor_result_downgrades_unavailable_host_cursor() {
        let resolved = resolve_cursor_mode(CursorMode::Host, CapabilityAvailability::Unavailable);
        assert_eq!(resolved.active, CursorMode::Local);
        assert!(!resolved.accepted);
        assert_eq!(
            resolved.reason,
            CursorModeNegotiationReason::HostCursorUnavailable
        );
    }

    #[test]
    fn native_tablet_never_substitutes_local_termination() {
        let resolved = resolve_tablet_mode(
            TabletMode::UsbTabletBridge,
            CapabilityAvailability::Available,
            CapabilityAvailability::Available,
            CapabilityAvailability::Available,
            CapabilityAvailability::Unavailable,
        );
        assert_eq!(resolved.active, TabletMode::DisabledMouseCompat);
        assert!(!resolved.accepted);
        assert!(resolved.reconnect_required);
        assert_eq!(
            resolved.reason,
            TabletModeNegotiationReason::NativeTabletUnavailable
        );
    }
}
