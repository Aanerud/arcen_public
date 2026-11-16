// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The local path to a Pier: which interface the traffic leaves through,
//! whether it is Wi-Fi or Ethernet, its MTU and link speed (the macOS
//! Deck's `netinfo` facts, from `GetAdaptersAddresses`).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, ToSocketAddrs, UdpSocket};

use arcen_deck_core::network::{NetworkIdentityPermission, NetworkProbeResult, OutgoingInterface};
use arcen_protocol::messages::{
    ClientNetworkSnapshotError, ClientNetworkSnapshotMsg, NetworkInterfaceKind, NetworkScopeMsg,
};

const IF_TYPE_ETHERNET_CSMACD: u32 = 6;
const IF_TYPE_PPP: u32 = 23;
const IF_TYPE_SOFTWARE_LOOPBACK: u32 = 24;
const IF_TYPE_PROP_VIRTUAL: u32 = 53;
const IF_TYPE_IEEE80211: u32 = 71;
const IF_TYPE_TUNNEL: u32 = 131;
const IF_TYPE_WWANPP: u32 = 243;
const IF_TYPE_WWANPP2: u32 = 244;

/// One adapter's facts for the interface that owns an address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterFacts {
    /// The name Windows shows (`Wi-Fi`, `Ethernet 2`).
    pub friendly_name: String,
    /// The IANA `ifType`.
    pub interface_type: u32,
    /// The IP MTU.
    pub mtu: Option<u32>,
    /// The slower of the transmit and receive link speeds, bits per second.
    pub link_bps: Option<u64>,
}

/// The interface kind of an IANA `ifType` as Windows reports it.
#[must_use]
pub const fn classify_interface_type(interface_type: u32) -> NetworkInterfaceKind {
    match interface_type {
        IF_TYPE_ETHERNET_CSMACD => NetworkInterfaceKind::Ethernet,
        IF_TYPE_IEEE80211 => NetworkInterfaceKind::Wifi,
        IF_TYPE_PPP | IF_TYPE_TUNNEL | IF_TYPE_PROP_VIRTUAL => NetworkInterfaceKind::Vpn,
        IF_TYPE_SOFTWARE_LOOPBACK => NetworkInterfaceKind::Loopback,
        IF_TYPE_WWANPP | IF_TYPE_WWANPP2 => NetworkInterfaceKind::Cellular,
        _ => NetworkInterfaceKind::Other,
    }
}

/// The connection test's label (`Wi-Fi (Wi-Fi)`, `Ethernet (Ethernet 2)`):
/// the macOS Deck's words for the kind, then the interface's own name.
#[must_use]
pub fn describe(facts: &AdapterFacts) -> String {
    let kind = match classify_interface_type(facts.interface_type) {
        NetworkInterfaceKind::Ethernet => "Ethernet",
        NetworkInterfaceKind::Wifi => "Wi-Fi",
        NetworkInterfaceKind::Cellular => "Cellular",
        NetworkInterfaceKind::Vpn => "VPN",
        NetworkInterfaceKind::Loopback => "Loopback",
        NetworkInterfaceKind::Other => "Other",
    };
    format!("{kind} ({})", facts.friendly_name)
}

/// The hello's network snapshot for traffic to `peer` over `facts`.
#[must_use]
pub fn snapshot(facts: Option<&AdapterFacts>, peer: IpAddr) -> Option<ClientNetworkSnapshotMsg> {
    let scope = match arcen_telemetry::classify_ip(peer) {
        arcen_telemetry::NetworkScope::Lan => NetworkScopeMsg::Lan,
        arcen_telemetry::NetworkScope::Wan => NetworkScopeMsg::Wan,
    };
    let kind = facts.map_or(NetworkInterfaceKind::Other, |facts| {
        classify_interface_type(facts.interface_type)
    });
    let link_mbps = facts
        .and_then(|facts| facts.link_bps)
        .and_then(|bps| u32::try_from(bps / 1_000_000).ok())
        .filter(|mbps| *mbps != 0);
    let mtu = facts.and_then(|facts| facts.mtu);
    match ClientNetworkSnapshotMsg::new(kind, scope, link_mbps, None, mtu, None) {
        // A loopback adapter reports an MTU of u32::MAX; keep the rest.
        Err(ClientNetworkSnapshotError::InvalidMtu) => {
            ClientNetworkSnapshotMsg::new(kind, scope, link_mbps, None, None, None).ok()
        }
        result => result.ok(),
    }
}

/// Probes the path to `host:port`.
#[must_use]
pub fn probe(host: &str, port: u16) -> NetworkProbeResult {
    let peer = (host, port)
        .to_socket_addrs()
        .ok()
        .and_then(|mut addresses| addresses.next());
    let Some(peer) = peer else {
        return NetworkProbeResult {
            snapshot: None,
            permission: NetworkIdentityPermission::NotRequested,
        };
    };
    let facts = source_address(peer.ip()).and_then(adapter_for_address);
    NetworkProbeResult {
        snapshot: snapshot(facts.as_ref(), peer.ip()),
        permission: NetworkIdentityPermission::NotRequested,
    }
}

/// The interface traffic to `peer` leaves through, and its MTU.
#[must_use]
pub fn outgoing_interface(peer: IpAddr) -> OutgoingInterface {
    match source_address(peer).and_then(adapter_for_address) {
        Some(facts) => OutgoingInterface {
            description: Some(describe(&facts)),
            mtu: facts.mtu,
        },
        None => OutgoingInterface::default(),
    }
}

/// The local address the routing table picks for `peer` (a connected UDP
/// socket sends nothing).
fn source_address(peer: IpAddr) -> Option<IpAddr> {
    let bind = match peer {
        IpAddr::V4(_) => "0.0.0.0:0",
        IpAddr::V6(_) => "[::]:0",
    };
    let socket = UdpSocket::bind(bind).ok()?;
    socket.connect((peer, 9)).ok()?;
    socket.local_addr().ok().map(|address| address.ip())
}

/// The adapter that owns `address`.
#[allow(
    clippy::cast_ptr_alignment,
    reason = "socket addresses are read unaligned"
)]
fn adapter_for_address(address: IpAddr) -> Option<AdapterFacts> {
    use windows::Win32::Foundation::{ERROR_BUFFER_OVERFLOW, ERROR_SUCCESS};
    use windows::Win32::NetworkManagement::IpHelper::{
        GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER, GAA_FLAG_SKIP_MULTICAST,
        GetAdaptersAddresses, IP_ADAPTER_ADDRESSES_LH,
    };
    use windows::Win32::Networking::WinSock::{
        AF_INET, AF_INET6, AF_UNSPEC, SOCKADDR_IN, SOCKADDR_IN6,
    };

    let flags = GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER;
    let mut length = 16_u32 * 1024;
    let mut storage: Vec<u64> = Vec::new();
    let mut status = ERROR_BUFFER_OVERFLOW.0;
    for _ in 0..4 {
        let words = usize::try_from(length).ok()?.div_ceil(8);
        storage.clear();
        storage.resize(words, 0);
        // SAFETY: `storage` is 8-byte aligned (as the pointer-bearing
        // records need) and writable for `length` bytes; `length` is a live
        // in/out parameter.
        status = unsafe {
            GetAdaptersAddresses(
                u32::from(AF_UNSPEC.0),
                flags,
                None,
                Some(storage.as_mut_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>()),
                &raw mut length,
            )
        };
        if status != ERROR_BUFFER_OVERFLOW.0 {
            break;
        }
    }
    if status != ERROR_SUCCESS.0 {
        return None;
    }
    let mut adapter = storage.as_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>();
    while !adapter.is_null() {
        // SAFETY: the list lives in `storage`; every `Next` pointer is either
        // null or another record in the same buffer.
        let record = unsafe { &*adapter };
        let mut unicast = record.FirstUnicastAddress;
        while !unicast.is_null() {
            // SAFETY: as above, a record of this adapter's unicast list.
            let entry = unsafe { &*unicast };
            let sockaddr = entry.Address.lpSockaddr;
            if !sockaddr.is_null() {
                // SAFETY: a non-null socket address starts with its family.
                let family = unsafe { (*sockaddr).sa_family };
                let ip = if family == AF_INET {
                    // SAFETY: AF_INET identifies a SOCKADDR_IN; read
                    // unaligned because the type only promises a SOCKADDR.
                    let v4 = unsafe { sockaddr.cast::<SOCKADDR_IN>().read_unaligned() };
                    // SAFETY: reading the plain u32 view of the address union.
                    Some(IpAddr::V4(Ipv4Addr::from(u32::from_be(unsafe {
                        v4.sin_addr.S_un.S_addr
                    }))))
                } else if family == AF_INET6 {
                    // SAFETY: AF_INET6 identifies a SOCKADDR_IN6.
                    let v6 = unsafe { sockaddr.cast::<SOCKADDR_IN6>().read_unaligned() };
                    // SAFETY: reading the byte view of the address union.
                    Some(IpAddr::V6(Ipv6Addr::from(unsafe { v6.sin6_addr.u.Byte })))
                } else {
                    None
                };
                if ip == Some(address) {
                    // SAFETY: FriendlyName is a NUL-terminated wide string
                    // inside the same buffer.
                    let friendly_name =
                        unsafe { record.FriendlyName.to_string() }.unwrap_or_default();
                    let speed = record.TransmitLinkSpeed.min(record.ReceiveLinkSpeed);
                    return Some(AdapterFacts {
                        friendly_name,
                        interface_type: record.IfType,
                        mtu: (record.Mtu != 0 && record.Mtu != u32::MAX).then_some(record.Mtu),
                        link_bps: (speed != 0 && speed != u64::MAX).then_some(speed),
                    });
                }
            }
            unicast = entry.Next;
        }
        adapter = record.Next;
    }
    None
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;

    fn facts(interface_type: u32) -> AdapterFacts {
        AdapterFacts {
            friendly_name: "Ethernet 2".to_owned(),
            interface_type,
            mtu: Some(1500),
            link_bps: Some(10_000_000_000),
        }
    }

    #[test]
    fn interface_types_classify_like_the_mac_names_do() {
        assert_eq!(classify_interface_type(6), NetworkInterfaceKind::Ethernet);
        assert_eq!(classify_interface_type(71), NetworkInterfaceKind::Wifi);
        assert_eq!(classify_interface_type(53), NetworkInterfaceKind::Vpn);
        assert_eq!(classify_interface_type(24), NetworkInterfaceKind::Loopback);
        assert_eq!(classify_interface_type(243), NetworkInterfaceKind::Cellular);
        assert_eq!(classify_interface_type(1), NetworkInterfaceKind::Other);
        assert_eq!(describe(&facts(6)), "Ethernet (Ethernet 2)");
        assert_eq!(
            describe(&AdapterFacts {
                friendly_name: "Wi-Fi".to_owned(),
                ..facts(71)
            }),
            "Wi-Fi (Wi-Fi)"
        );
    }

    #[test]
    fn the_snapshot_carries_kind_scope_speed_and_mtu() {
        let lan = snapshot(Some(&facts(6)), "192.168.1.10".parse().expect("ip")).expect("snapshot");
        assert_eq!(lan.interface_kind(), NetworkInterfaceKind::Ethernet);
        assert_eq!(lan.scope(), NetworkScopeMsg::Lan);
        assert_eq!(lan.link_mbps(), Some(10_000));
        assert_eq!(lan.mtu(), Some(1500));
        assert_eq!(lan.ssid(), None);

        let loopback = AdapterFacts {
            mtu: Some(u32::MAX),
            link_bps: None,
            ..facts(24)
        };
        let snap = snapshot(Some(&loopback), "203.0.113.7".parse().expect("ip")).expect("snapshot");
        assert_eq!(snap.mtu(), None);
        assert_eq!(snap.scope(), NetworkScopeMsg::Wan);
    }

    #[test]
    fn loopback_probe_names_the_loopback_adapter() {
        let result = probe("127.0.0.1", 18_443);
        assert_eq!(result.permission, NetworkIdentityPermission::NotRequested);
        let snapshot = result.snapshot.expect("loopback snapshot");
        assert_eq!(snapshot.ssid(), None);
    }
}
