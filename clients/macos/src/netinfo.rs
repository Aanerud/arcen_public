use std::net::{IpAddr, ToSocketAddrs, UdpSocket};

use arcen_protocol::messages::{ClientNetworkSnapshotMsg, NetworkInterfaceKind, NetworkScopeMsg};
use arcen_telemetry::classify_ip;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkProbeResult {
    pub snapshot: Option<ClientNetworkSnapshotMsg>,
    pub permission: NetworkIdentityPermission,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkIdentityPermission {
    NotRequested,
    Unavailable,
}

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
    let local = source_address(peer.ip());
    let (interface, mtu, link_mbps) = local
        .and_then(interface_for_address)
        .unwrap_or_else(|| ("unknown".to_owned(), None, None));
    let kind = classify_interface_name(&interface);
    let scope = match classify_ip(peer.ip()) {
        arcen_telemetry::NetworkScope::Lan => NetworkScopeMsg::Lan,
        arcen_telemetry::NetworkScope::Wan => NetworkScopeMsg::Wan,
    };
    let snapshot = ClientNetworkSnapshotMsg::new(kind, scope, link_mbps, None, mtu, None).ok();
    NetworkProbeResult {
        snapshot,
        permission: NetworkIdentityPermission::NotRequested,
    }
}

fn source_address(peer: IpAddr) -> Option<IpAddr> {
    let bind = match peer {
        IpAddr::V4(_) => "0.0.0.0:0",
        IpAddr::V6(_) => "[::]:0",
    };
    let socket = UdpSocket::bind(bind).ok()?;
    socket.connect((peer, 9)).ok()?;
    socket.local_addr().ok().map(|address| address.ip())
}

fn classify_interface_name(name: &str) -> NetworkInterfaceKind {
    if name == "lo0" || name == "lo" {
        NetworkInterfaceKind::Loopback
    } else if name.starts_with("utun") || name.starts_with("ppp") || name.starts_with("ipsec") {
        NetworkInterfaceKind::Vpn
    } else if name == "en0" || name.starts_with("wl") {
        NetworkInterfaceKind::Wifi
    } else if name.starts_with("en") || name.starts_with("eth") {
        NetworkInterfaceKind::Ethernet
    } else if name.starts_with("pdp_ip") {
        NetworkInterfaceKind::Cellular
    } else {
        NetworkInterfaceKind::Other
    }
}

#[cfg(unix)]
fn interface_for_address(address: IpAddr) -> Option<(String, Option<u32>, Option<u32>)> {
    use std::ffi::CStr;
    use std::ptr;

    let mut interfaces: *mut libc::ifaddrs = ptr::null_mut();
    // SAFETY: getifaddrs initializes a linked list owned by freeifaddrs. Every
    // pointer is null-checked and sockaddr families select the correct layout.
    if unsafe { libc::getifaddrs(&mut interfaces) } != 0 {
        return None;
    }
    let mut cursor = interfaces;
    let mut found = None;
    while !cursor.is_null() {
        // SAFETY: cursor belongs to the live getifaddrs list.
        let item = unsafe { &*cursor };
        if !item.ifa_addr.is_null() && sockaddr_ip(item.ifa_addr) == Some(address) {
            // SAFETY: ifa_name is a NUL-terminated interface name for this item.
            let name = unsafe { CStr::from_ptr(item.ifa_name) }
                .to_str()
                .ok()
                .map(str::to_owned);
            if let Some(name) = name {
                let (mtu, link_mbps) = interface_facts(item);
                found = Some((name, mtu, link_mbps));
            }
            break;
        }
        cursor = item.ifa_next;
    }
    // SAFETY: interfaces is the original pointer returned by getifaddrs.
    unsafe { libc::freeifaddrs(interfaces) };
    found
}

#[cfg(unix)]
fn sockaddr_ip(address: *const libc::sockaddr) -> Option<IpAddr> {
    match unsafe { (*address).sa_family as i32 } {
        libc::AF_INET => {
            // SAFETY: AF_INET identifies sockaddr_in.
            let address = unsafe { &*(address.cast::<libc::sockaddr_in>()) };
            Some(IpAddr::V4(std::net::Ipv4Addr::from(
                address.sin_addr.s_addr.to_ne_bytes(),
            )))
        }
        libc::AF_INET6 => {
            // SAFETY: AF_INET6 identifies sockaddr_in6.
            let address = unsafe { &*(address.cast::<libc::sockaddr_in6>()) };
            Some(IpAddr::V6(std::net::Ipv6Addr::from(
                address.sin6_addr.s6_addr,
            )))
        }
        _ => None,
    }
}

#[cfg(not(unix))]
fn interface_for_address(_address: IpAddr) -> Option<(String, Option<u32>, Option<u32>)> {
    None
}

#[cfg(target_os = "macos")]
fn interface_facts(item: &libc::ifaddrs) -> (Option<u32>, Option<u32>) {
    if item.ifa_data.is_null() {
        return (None, None);
    }
    // SAFETY: Darwin documents ifa_data as `if_data` for interface entries.
    let data = unsafe { &*(item.ifa_data.cast::<libc::if_data>()) };
    let mtu = (data.ifi_mtu != 0).then_some(data.ifi_mtu);
    let link_mbps = u32::try_from(data.ifi_baudrate / 1_000_000)
        .ok()
        .filter(|value| *value != 0);
    (mtu, link_mbps)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn interface_facts(_item: &libc::ifaddrs) -> (Option<u32>, Option<u32>) {
    (None, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interface_classification_is_bounded() {
        assert_eq!(classify_interface_name("en0"), NetworkInterfaceKind::Wifi);
        assert_eq!(
            classify_interface_name("en7"),
            NetworkInterfaceKind::Ethernet
        );
        assert_eq!(classify_interface_name("utun3"), NetworkInterfaceKind::Vpn);
        assert_eq!(
            classify_interface_name("lo0"),
            NetworkInterfaceKind::Loopback
        );
    }

    #[test]
    fn raw_ssid_is_omitted_without_an_explicit_product_opt_in() {
        let result = probe("127.0.0.1", 18_443);
        let snapshot = result.snapshot.expect("loopback snapshot");
        assert_eq!(snapshot.ssid(), None);
        assert_eq!(result.permission, NetworkIdentityPermission::NotRequested);
    }
}
