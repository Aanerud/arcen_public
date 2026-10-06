//! Portable classification for outbound network reachability failures.

use std::fmt::{Display, Formatter};
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

/// Endpoint scope used for user-facing reachability messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReachabilityScope {
    /// Private, link-local, or unique-local destination that is not loopback.
    LocalNetwork,
    /// Public or loopback destination.
    Other,
}

impl ReachabilityScope {
    /// Stable structured-log token.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LocalNetwork => "local_network",
            Self::Other => "other",
        }
    }
}

/// A network-layer failure class that should not wait for a QUIC timeout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReachabilityFailureClass {
    /// No route to the destination host.
    HostUnreachable,
    /// No route to the destination network.
    NetworkUnreachable,
}

impl ReachabilityFailureClass {
    /// Stable structured-log token.
    #[must_use]
    pub const fn reason_class(self, scope: ReachabilityScope) -> &'static str {
        match (self, scope) {
            (Self::HostUnreachable | Self::NetworkUnreachable, ReachabilityScope::LocalNetwork) => {
                "local_network_unreachable"
            }
            (Self::HostUnreachable, ReachabilityScope::Other) => "host_unreachable",
            (Self::NetworkUnreachable, ReachabilityScope::Other) => "network_unreachable",
        }
    }
}

/// A classified reachability failure with bounded OS evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReachabilityFailure {
    /// Local versus non-local destination scope.
    pub scope: ReachabilityScope,
    /// Host/network unreachable class.
    pub class: ReachabilityFailureClass,
    /// Platform errno/WSA code when available.
    pub os_code: Option<i32>,
}

impl Display for ReachabilityFailure {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.user_message())
    }
}

impl std::error::Error for ReachabilityFailure {}

impl ReachabilityFailure {
    /// Stable reason-class token for lifecycle logs.
    #[must_use]
    pub const fn reason_class(self) -> &'static str {
        self.class.reason_class(self.scope)
    }

    /// User-facing message, including the macOS Local Network hint only for LAN destinations.
    #[must_use]
    pub fn user_message(self) -> &'static str {
        match self.scope {
            ReachabilityScope::LocalNetwork => {
                "macOS may be blocking Arcen Deck from the local network. In System Settings › Privacy & Security › Local Network, enable Arcen Deck. The host may also be offline or unreachable."
            }
            ReachabilityScope::Other => "The host is offline or unreachable.",
        }
    }
}

/// Returns true when a destination should get a cheap UDP preflight before QUIC.
///
/// Loopback is deliberately excluded: it is local to the machine, not subject
/// to macOS Local Network privacy, and tests/dev tools often use it.
#[must_use]
pub fn should_preflight_local_network(destination: IpAddr) -> bool {
    match destination {
        IpAddr::V4(address) => is_local_ipv4(address) && !address.is_loopback(),
        IpAddr::V6(address) => is_local_ipv6(address) && !address.is_loopback(),
    }
}

/// Classifies a destination for user-facing reachability text.
#[must_use]
pub fn reachability_scope(destination: IpAddr) -> ReachabilityScope {
    if should_preflight_local_network(destination) {
        ReachabilityScope::LocalNetwork
    } else {
        ReachabilityScope::Other
    }
}

/// Maps an I/O error produced by UDP/QUIC path setup into an explicit class.
#[must_use]
pub fn classify_reachability_error(
    error: &io::Error,
    destination: SocketAddr,
) -> Option<ReachabilityFailure> {
    let class = match error.kind() {
        io::ErrorKind::HostUnreachable => ReachabilityFailureClass::HostUnreachable,
        io::ErrorKind::NetworkUnreachable => ReachabilityFailureClass::NetworkUnreachable,
        _ => return None,
    };
    Some(ReachabilityFailure {
        scope: reachability_scope(destination.ip()),
        class,
        os_code: error.raw_os_error(),
    })
}

fn is_local_ipv4(address: Ipv4Addr) -> bool {
    address.is_private() || address.is_link_local()
}

fn is_local_ipv6(address: Ipv6Addr) -> bool {
    let first = address.segments()[0];
    (first & 0xffc0) == 0xfe80 || (first & 0xfe00) == 0xfc00
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preflight_scope_includes_private_and_link_local_but_excludes_loopback_and_public() {
        for address in [
            "10.1.2.3",
            "172.16.4.5",
            "192.168.1.9",
            "169.254.7.8",
            "fd00::1",
            "fe80::abcd",
        ] {
            let parsed: IpAddr = address.parse().expect("test address");
            assert!(should_preflight_local_network(parsed), "{address}");
            assert_eq!(reachability_scope(parsed), ReachabilityScope::LocalNetwork);
        }

        for address in ["127.0.0.1", "::1", "203.0.113.10", "2001:db8::1"] {
            let parsed: IpAddr = address.parse().expect("test address");
            assert!(!should_preflight_local_network(parsed), "{address}");
            assert_eq!(reachability_scope(parsed), ReachabilityScope::Other);
        }
    }

    #[test]
    fn unreachable_errors_keep_errno_and_choose_reason_class_by_destination() {
        let local = SocketAddr::new("192.168.1.44".parse().unwrap(), 18_444);
        let public = SocketAddr::new("203.0.113.44".parse().unwrap(), 18_444);
        let local_error = io::Error::from(io::ErrorKind::HostUnreachable);
        let public_error = io::Error::from(io::ErrorKind::NetworkUnreachable);

        let local_failure = classify_reachability_error(&local_error, local).unwrap();
        assert_eq!(local_failure.reason_class(), "local_network_unreachable");
        assert!(local_failure.user_message().contains("Local Network"));

        let public_failure = classify_reachability_error(&public_error, public).unwrap();
        assert_eq!(public_failure.reason_class(), "network_unreachable");
        assert!(!public_failure.user_message().contains("Local Network"));
    }
}
