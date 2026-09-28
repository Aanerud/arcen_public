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
                "Native tablet (USB bridged) is unavailable on this host: it needs a USB virtualization backend and a host Wacom driver. Use Tablet support instead; it needs no host driver and works over any network."
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
    client_wacom_usb_bridge: CapabilityAvailability,
    host_wacom_usb_bridge: CapabilityAvailability,
) -> ResolvedTabletMode {
    let local_termination =
        super::mutual_capability(client_local_termination, host_local_termination);
    let wacom_usb_bridge = super::mutual_capability(client_wacom_usb_bridge, host_wacom_usb_bridge);
    let negotiation = super::negotiate_tablet_mode(requested, local_termination, wacom_usb_bridge);
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
            TabletMode::WacomUsbBridge => TabletModeNegotiationReason::NativeTabletUnavailable,
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
        reconnect_required: matches!(requested, TabletMode::WacomUsbBridge)
            && !negotiation.accepted,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            TabletMode::WacomUsbBridge,
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
