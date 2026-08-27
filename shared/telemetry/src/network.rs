//! Pure network snapshot and LAN/WAN classification contracts.

use std::error::Error;
use std::fmt::{Display, Formatter};
use std::net::IpAddr;

use serde::Serialize;

/// Maximum schema-approved network identity length.
pub const MAX_NETWORK_IDENTITY_BYTES: usize = 64;
/// Minimum accepted interface MTU.
pub const MIN_NETWORK_MTU: u32 = 576;
/// Maximum accepted interface MTU, including Linux and Windows loopback.
pub const MAX_NETWORK_MTU: u32 = 65_536;

/// Active interface kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum InterfaceKind {
    /// Wired Ethernet.
    Ethernet,
    /// Wi-Fi.
    Wifi,
    /// Cellular.
    Cellular,
    /// Virtual private network.
    Vpn,
    /// Local loopback.
    Loopback,
    /// Other or unknown interface.
    Other,
}

/// Endpoint network scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum NetworkScope {
    /// Private, loopback, link-local, or unique-local addressing.
    Lan,
    /// Public addressing.
    Wan,
}

/// Validated network path snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NetworkSnapshot {
    /// Active interface kind.
    pub interface_kind: InterfaceKind,
    /// Negotiated or reported link rate.
    pub link_mbps: Option<u32>,
    /// Wi-Fi identity, when already permitted by the OS.
    pub ssid: Option<String>,
    /// Wi-Fi signal strength.
    pub rssi_dbm: Option<i32>,
    /// LAN or WAN endpoint scope.
    pub scope: NetworkScope,
    /// Interface MTU.
    pub mtu: Option<u32>,
}

impl NetworkSnapshot {
    /// Creates a bounded network snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error for zero rates, invalid MTUs or RSSI, unsafe identity
    /// text, or Wi-Fi-only facts attached to another interface kind.
    pub fn new(
        interface_kind: InterfaceKind,
        link_mbps: Option<u32>,
        ssid: Option<impl Into<String>>,
        rssi_dbm: Option<i32>,
        scope: NetworkScope,
        mtu: Option<u32>,
    ) -> Result<Self, NetworkValidationError> {
        if link_mbps == Some(0) {
            return Err(NetworkValidationError::InvalidLinkRate);
        }
        if mtu.is_some_and(|value| !(MIN_NETWORK_MTU..=MAX_NETWORK_MTU).contains(&value)) {
            return Err(NetworkValidationError::InvalidMtu);
        }
        if rssi_dbm.is_some_and(|value| !(-127..=0).contains(&value)) {
            return Err(NetworkValidationError::InvalidRssi);
        }
        let ssid = ssid.map(Into::into);
        if ssid.as_ref().is_some_and(|value| {
            value.is_empty()
                || value.len() > MAX_NETWORK_IDENTITY_BYTES
                || value.chars().any(char::is_control)
        }) {
            return Err(NetworkValidationError::InvalidIdentity);
        }
        if interface_kind != InterfaceKind::Wifi && (ssid.is_some() || rssi_dbm.is_some()) {
            return Err(NetworkValidationError::WifiFactsOnOtherInterface);
        }
        Ok(Self {
            interface_kind,
            link_mbps,
            ssid,
            rssi_dbm,
            scope,
            mtu,
        })
    }
}

/// Classifies a parsed endpoint address without probing or resolving.
#[must_use]
pub fn classify_ip(address: IpAddr) -> NetworkScope {
    match address {
        IpAddr::V4(address)
            if address.is_private()
                || address.is_loopback()
                || address.is_link_local()
                || address.is_unspecified() =>
        {
            NetworkScope::Lan
        }
        IpAddr::V6(address) => {
            let first = address.segments()[0];
            if address.is_loopback()
                || address.is_unspecified()
                || (first & 0xffc0) == 0xfe80
                || (first & 0xfe00) == 0xfc00
            {
                NetworkScope::Lan
            } else {
                NetworkScope::Wan
            }
        }
        IpAddr::V4(_) => NetworkScope::Wan,
    }
}

/// Parses and classifies an IP literal. Hostnames remain unavailable because
/// this pure contract never performs DNS.
#[must_use]
pub fn classify_ip_literal(value: &str) -> Option<NetworkScope> {
    value.parse().ok().map(classify_ip)
}

/// Invalid network snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkValidationError {
    /// Link rate was zero.
    InvalidLinkRate,
    /// MTU was outside the supported network range.
    InvalidMtu,
    /// RSSI was outside the dBm range.
    InvalidRssi,
    /// Identity was empty, oversized, or contained controls.
    InvalidIdentity,
    /// SSID or RSSI was attached to a non-Wi-Fi interface.
    WifiFactsOnOtherInterface,
}

impl Display for NetworkValidationError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::InvalidLinkRate => "network link rate must be nonzero when available",
            Self::InvalidMtu => "network MTU is outside 576..=65536",
            Self::InvalidRssi => "network RSSI is outside -127..=0 dBm",
            Self::InvalidIdentity => "network identity is empty, oversized, or contains controls",
            Self::WifiFactsOnOtherInterface => {
                "Wi-Fi identity or RSSI was attached to another interface kind"
            }
        })
    }
}

impl Error for NetworkValidationError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifier_covers_ipv4_ipv6_and_unresolved_names() {
        assert_eq!(classify_ip_literal("192.168.1.10"), Some(NetworkScope::Lan));
        assert_eq!(classify_ip_literal("10.0.0.5"), Some(NetworkScope::Lan));
        assert_eq!(classify_ip_literal("169.254.1.1"), Some(NetworkScope::Lan));
        assert_eq!(classify_ip_literal("0.0.0.0"), Some(NetworkScope::Lan));
        assert_eq!(classify_ip_literal("8.8.8.8"), Some(NetworkScope::Wan));
        assert_eq!(classify_ip_literal("::1"), Some(NetworkScope::Lan));
        assert_eq!(classify_ip_literal("::"), Some(NetworkScope::Lan));
        assert_eq!(classify_ip_literal("fd00::1"), Some(NetworkScope::Lan));
        assert_eq!(
            classify_ip_literal("2001:4860:4860::8888"),
            Some(NetworkScope::Wan)
        );
        assert_eq!(classify_ip_literal("example.com"), None);
    }

    #[test]
    fn network_snapshot_enforces_bounds_and_interface_semantics() {
        assert!(
            NetworkSnapshot::new(
                InterfaceKind::Wifi,
                Some(866),
                Some("studio"),
                Some(-61),
                NetworkScope::Wan,
                Some(1_500),
            )
            .is_ok()
        );
        assert_eq!(
            NetworkSnapshot::new(
                InterfaceKind::Ethernet,
                Some(1_000),
                Some("not-wifi"),
                None,
                NetworkScope::Lan,
                Some(1_500),
            ),
            Err(NetworkValidationError::WifiFactsOnOtherInterface)
        );
        assert_eq!(
            NetworkSnapshot::new(
                InterfaceKind::Wifi,
                Some(0),
                None::<String>,
                None,
                NetworkScope::Lan,
                Some(1_500),
            ),
            Err(NetworkValidationError::InvalidLinkRate)
        );
        assert!(
            NetworkSnapshot::new(
                InterfaceKind::Loopback,
                None,
                None::<String>,
                None,
                NetworkScope::Lan,
                Some(MAX_NETWORK_MTU),
            )
            .is_ok()
        );
        assert_eq!(
            NetworkSnapshot::new(
                InterfaceKind::Loopback,
                None,
                None::<String>,
                None,
                NetworkScope::Lan,
                Some(MAX_NETWORK_MTU + 1),
            ),
            Err(NetworkValidationError::InvalidMtu)
        );
    }
}
