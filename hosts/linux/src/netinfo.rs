//! Linux network-path probing for the shared observability network schema.
//!
//! Bounded `/sys/class/net` and `/proc/net/route` reads only: no packet
//! capture, no DNS/hostname resolution, and no raw SSID disclosure — Wi-Fi
//! identity is never populated by this probe (default-omit policy; see
//! `arcen_telemetry::NetworkSnapshot`, which only accepts an SSID attached to
//! a Wi-Fi interface and never receives one here).

use std::fs;
use std::io::Read;
use std::path::Path;

use arcen_telemetry::{
    classify_ip_literal, FieldValue, InterfaceKind, NetworkScope, NetworkSnapshot, StructuredFields,
};

/// Bound on any single sysfs/procfs read. These are all tiny virtual files;
/// anything larger indicates a hostile or corrupt mount and is truncated
/// rather than read in full.
const MAX_PROBE_READ_BYTES: usize = 8192;

/// `ARPHRD_ETHER` from `linux/if_arp.h`, as reported by `.../type`.
const ARPHRD_ETHER: &str = "1";
/// `ARPHRD_LOOPBACK` from `linux/if_arp.h`.
const ARPHRD_LOOPBACK: &str = "772";

/// Facts read for one `/sys/class/net/<name>` interface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InterfaceFacts {
    pub(crate) name: String,
    pub(crate) kind: InterfaceKind,
    pub(crate) operational: bool,
    pub(crate) link_mbps: Option<u32>,
    pub(crate) mtu: Option<u32>,
}

fn read_bounded(path: &Path) -> Option<String> {
    let mut file = fs::File::open(path).ok()?;
    let mut buffer = vec![0u8; MAX_PROBE_READ_BYTES];
    let read = file.read(&mut buffer).ok()?;
    buffer.truncate(read);
    String::from_utf8(buffer)
        .ok()
        .map(|value| value.trim().to_owned())
}

/// Classifies one interface from its name and sysfs facts: `wireless`/
/// `phy80211` marks Wi-Fi, well-known tunnel name prefixes mark VPN, `lo`
/// (and `ARPHRD_LOOPBACK`) marks loopback, and `ARPHRD_ETHER` marks Ethernet.
/// Everything else is `Other` rather than guessed.
fn classify_interface(sys_class_net: &Path, name: &str) -> InterfaceKind {
    if name == "lo" {
        return InterfaceKind::Loopback;
    }
    let interface_dir = sys_class_net.join(name);
    if interface_dir.join("wireless").is_dir() || interface_dir.join("phy80211").is_dir() {
        return InterfaceKind::Wifi;
    }
    if ["tun", "tap", "wg", "ppp"]
        .iter()
        .any(|prefix| name.starts_with(prefix))
    {
        return InterfaceKind::Vpn;
    }
    match read_bounded(&interface_dir.join("type")).as_deref() {
        Some(ARPHRD_ETHER) => InterfaceKind::Ethernet,
        Some(ARPHRD_LOOPBACK) => InterfaceKind::Loopback,
        _ => InterfaceKind::Other,
    }
}

fn interface_facts(sys_class_net: &Path, name: &str) -> InterfaceFacts {
    let interface_dir = sys_class_net.join(name);
    let operational = read_bounded(&interface_dir.join("operstate"))
        .is_some_and(|state| state.eq_ignore_ascii_case("up"));
    // `speed` reads as Mbps but only resolves for interfaces that are up and
    // driver-reporting; ENODEV/EINVAL (e.g. on a down or virtual interface)
    // are absent rather than a misleading zero.
    let link_mbps = read_bounded(&interface_dir.join("speed"))
        .and_then(|value| value.parse::<i64>().ok())
        .and_then(|value| u32::try_from(value).ok())
        .filter(|value| *value != 0);
    let mtu = read_bounded(&interface_dir.join("mtu")).and_then(|value| value.parse::<u32>().ok());
    InterfaceFacts {
        name: name.to_owned(),
        kind: classify_interface(sys_class_net, name),
        operational,
        link_mbps,
        mtu,
    }
}

fn list_interfaces(sys_class_net: &Path) -> Vec<String> {
    fs::read_dir(sys_class_net)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect()
}

/// Resolves the interface whose kernel `ifindex` (`/sys/class/net/<name>/
/// ifindex`) matches a peer's `SocketAddrV6` zone/scope id. On Linux, a
/// numeric IPv6 scope id *is* the ifindex, so this is an exact, bounded
/// lookup rather than a guess — used only to disambiguate a link-local
/// peer address, which the route table cannot resolve on its own (the same
/// `fe80::/10` prefix is valid, and typically present, on every interface).
fn interface_by_ifindex(sys_class_net: &Path, ifindex: u32) -> Option<String> {
    list_interfaces(sys_class_net).into_iter().find(|name| {
        read_bounded(&sys_class_net.join(name).join("ifindex"))
            .and_then(|value| value.parse::<u32>().ok())
            == Some(ifindex)
    })
}

/// One parsed IPv4 route row from `/proc/net/route`.
///
/// `/proc/net/route` prints `Destination`/`Mask` as the raw 32-bit value's
/// hex byte dump (little-endian on every real Linux target), so each is
/// byte-swapped back into the normal network-order integer matching
/// `Ipv4Addr`'s own representation before any prefix comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Ipv4Route {
    interface: String,
    network: u32,
    mask: u32,
    prefix_len: u32,
    metric: u32,
}

fn parse_ipv4_routes(content: &str) -> Vec<Ipv4Route> {
    content
        .lines()
        .skip(1)
        .filter_map(|line| {
            let mut columns = line.split_whitespace();
            let interface = columns.next()?;
            let destination_hex = columns.next()?;
            let _gateway = columns.next()?;
            let _flags = columns.next()?;
            let _refcnt = columns.next()?;
            let _use = columns.next()?;
            let metric_dec = columns.next()?;
            let mask_hex = columns.next()?;
            let network = u32::from_str_radix(destination_hex, 16).ok()?.swap_bytes();
            let mask = u32::from_str_radix(mask_hex, 16).ok()?.swap_bytes();
            let metric = metric_dec.parse::<u32>().ok()?;
            Some(Ipv4Route {
                interface: interface.to_owned(),
                network: network & mask,
                mask,
                prefix_len: mask.count_ones(),
                metric,
            })
        })
        .collect()
}

/// Longest-prefix-match (ties broken by lowest metric, matching real kernel
/// route selection) over every parsed IPv4 route, never just the default
/// route — required for a multi-homed host or a VPN tunnel route that is
/// more specific than the physical interface's default route.
fn best_ipv4_route(routes: &[Ipv4Route], peer: std::net::Ipv4Addr) -> Option<&Ipv4Route> {
    let peer = u32::from(peer);
    routes
        .iter()
        .filter(|route| peer & route.mask == route.network)
        .max_by_key(|route| (route.prefix_len, u32::MAX - route.metric))
}

/// One parsed IPv6 route row from `/proc/net/ipv6_route`: `dest_addr` is
/// already a plain big-endian 128-bit hex dump (unlike the IPv4 table, no
/// byte-swap is needed).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Ipv6Route {
    interface: String,
    network: u128,
    prefix_len: u32,
    metric: u32,
}

fn parse_ipv6_routes(content: &str) -> Vec<Ipv6Route> {
    content
        .lines()
        .filter_map(|line| {
            let mut columns = line.split_whitespace();
            let dest_hex = columns.next()?;
            let prefix_len_hex = columns.next()?;
            let _src_hex = columns.next()?;
            let _src_prefix_len_hex = columns.next()?;
            let _next_hop_hex = columns.next()?;
            let metric_hex = columns.next()?;
            let _refcnt_hex = columns.next()?;
            let _use_hex = columns.next()?;
            let _flags_hex = columns.next()?;
            let interface = columns.next()?;
            let dest = u128::from_str_radix(dest_hex, 16).ok()?;
            let prefix_len = u32::from_str_radix(prefix_len_hex, 16).ok()?;
            let metric = u32::from_str_radix(metric_hex, 16).ok()?;
            let mask = ipv6_prefix_mask(prefix_len);
            Some(Ipv6Route {
                interface: interface.to_owned(),
                network: dest & mask,
                prefix_len,
                metric,
            })
        })
        .collect()
}

fn ipv6_prefix_mask(prefix_len: u32) -> u128 {
    if prefix_len == 0 {
        0
    } else if prefix_len >= 128 {
        u128::MAX
    } else {
        u128::MAX << (128 - prefix_len)
    }
}

fn best_ipv6_route(routes: &[Ipv6Route], peer: std::net::Ipv6Addr) -> Option<&Ipv6Route> {
    let peer = u128::from(peer);
    routes
        .iter()
        .filter(|route| {
            let mask = ipv6_prefix_mask(route.prefix_len);
            peer & mask == route.network
        })
        .max_by_key(|route| (route.prefix_len, u32::MAX - route.metric))
}

/// Selects the effective route's owning interface for the actual peer
/// address, covering multi-homed hosts, a more-specific VPN route, and
/// IPv6, via bounded reads of the kernel's own route tables (no `ip route
/// get`/other subprocess, no unbounded parsing). Falls back to `None` (never
/// a guess) when the peer's address family cannot be parsed or no route
/// matches, leaving `select_best`'s priority fallback as the final resort.
fn route_interface_for_peer(
    proc_net_route: &Path,
    proc_net_ipv6_route: &Path,
    peer_ip: &str,
) -> Option<String> {
    if let Ok(peer) = peer_ip.parse::<std::net::Ipv4Addr>() {
        let content = read_bounded(proc_net_route)?;
        let routes = parse_ipv4_routes(&content);
        return best_ipv4_route(&routes, peer).map(|route| route.interface.clone());
    }
    if let Ok(peer) = peer_ip.parse::<std::net::Ipv6Addr>() {
        let content = read_bounded(proc_net_ipv6_route)?;
        let routes = parse_ipv6_routes(&content);
        return best_ipv6_route(&routes, peer).map(|route| route.interface.clone());
    }
    None
}

fn priority(kind: InterfaceKind) -> u8 {
    match kind {
        InterfaceKind::Ethernet => 0,
        InterfaceKind::Wifi => 1,
        InterfaceKind::Vpn => 2,
        InterfaceKind::Other => 3,
        InterfaceKind::Cellular => 4,
        InterfaceKind::Loopback => 5,
    }
}

/// Picks the best operational, non-loopback interface when no default route
/// is available (e.g. IPv6-only or an unreadable route table).
pub(crate) fn select_best(facts: &[InterfaceFacts]) -> Option<&InterfaceFacts> {
    facts
        .iter()
        .filter(|facts| facts.operational && facts.kind != InterfaceKind::Loopback)
        .min_by_key(|facts| priority(facts.kind))
}

/// Probes the live host for the interface facts backing one peer connection.
///
/// `scope_id` is the peer's `SocketAddrV6` zone id when the connection is
/// IPv6 (`0` / absent otherwise): a link-local peer address is ambiguous
/// across interfaces without it, so when present and the peer address is
/// link-local it takes priority over the route-table lookup below.
pub(crate) fn snapshot(peer_addr: &str, scope_id: Option<u32>) -> Option<NetworkSnapshot> {
    snapshot_at(
        Path::new("/sys/class/net"),
        Path::new("/proc/net/route"),
        Path::new("/proc/net/ipv6_route"),
        peer_addr,
        scope_id,
    )
}

fn snapshot_at(
    sys_class_net: &Path,
    proc_net_route: &Path,
    proc_net_ipv6_route: &Path,
    peer_addr: &str,
    scope_id: Option<u32>,
) -> Option<NetworkSnapshot> {
    let peer_ip = peer_addr
        .parse::<std::net::SocketAddr>()
        .map(|address| address.ip().to_string())
        .unwrap_or_else(|_| peer_addr.trim_matches(['[', ']']).to_owned());
    let scope = classify_ip_literal(&peer_ip).unwrap_or(NetworkScope::Wan);

    let names = list_interfaces(sys_class_net);
    if names.is_empty() {
        return None;
    }
    let facts: Vec<InterfaceFacts> = names
        .iter()
        .map(|name| interface_facts(sys_class_net, name))
        .collect();

    let is_link_local_v6 = peer_ip
        .parse::<std::net::Ipv6Addr>()
        .is_ok_and(|address| address.segments()[0] & 0xffc0 == 0xfe80);
    let has_nonzero_scope_id = scope_id.is_some_and(|id| id != 0);
    let scoped_choice = if is_link_local_v6 {
        scope_id
            .filter(|id| *id != 0)
            .and_then(|ifindex| interface_by_ifindex(sys_class_net, ifindex))
            .and_then(|name| {
                facts
                    .iter()
                    .find(|candidate| candidate.name == name && candidate.operational)
            })
    } else {
        None
    };

    let chosen = if is_link_local_v6 && has_nonzero_scope_id {
        // Re-review finding #4: a link-local IPv6 peer with a nonzero
        // scope id must resolve strictly via that exact interface index.
        // The scope id is the only signal disambiguating which interface
        // this specific peer is reachable on; falling back to the
        // route table or "best" interface when it fails to resolve could
        // silently report facts for the wrong interface, so this returns
        // unavailable (`None`) instead of guessing.
        scoped_choice?
    } else {
        scoped_choice
            .or_else(|| {
                route_interface_for_peer(proc_net_route, proc_net_ipv6_route, &peer_ip).and_then(
                    |name| {
                        facts
                            .iter()
                            .find(|candidate| candidate.name == name && candidate.operational)
                    },
                )
            })
            .or_else(|| select_best(&facts))?
    };

    // Never disclose the raw SSID or RSSI: this probe never supplies Wi-Fi
    // identity/signal facts even when `chosen.kind == InterfaceKind::Wifi`.
    NetworkSnapshot::new(
        chosen.kind,
        chosen.link_mbps,
        None::<String>,
        None,
        scope,
        chosen.mtu,
    )
    .ok()
}

/// `NETWORK_PATH_ACTIVE`/`NETWORK_PATH_RESTORED` fields for one snapshot.
pub(crate) fn lifecycle_fields(snapshot: &NetworkSnapshot) -> StructuredFields {
    let mut fields = StructuredFields::default();
    let _ = fields.insert(
        "interface_kind",
        FieldValue::String(interface_kind_name(snapshot.interface_kind).to_owned()),
    );
    let _ = fields.insert(
        "scope",
        FieldValue::String(network_scope_name(snapshot.scope).to_owned()),
    );
    if let Some(link_mbps) = snapshot.link_mbps {
        let _ = fields.insert("link_mbps", FieldValue::Integer(i64::from(link_mbps)));
    }
    if let Some(mtu) = snapshot.mtu {
        let _ = fields.insert("mtu", FieldValue::Integer(i64::from(mtu)));
    }
    fields
}

/// `NETWORK_PATH_CHANGED` fields between two snapshots.
pub(crate) fn changed_fields(old: &NetworkSnapshot, new: &NetworkSnapshot) -> StructuredFields {
    let mut fields = StructuredFields::default();
    let _ = fields.insert(
        "old_kind",
        FieldValue::String(interface_kind_name(old.interface_kind).to_owned()),
    );
    let _ = fields.insert(
        "new_kind",
        FieldValue::String(interface_kind_name(new.interface_kind).to_owned()),
    );
    if let Some(link_mbps) = old.link_mbps {
        let _ = fields.insert("old_mbps", FieldValue::Integer(i64::from(link_mbps)));
    }
    if let Some(link_mbps) = new.link_mbps {
        let _ = fields.insert("new_mbps", FieldValue::Integer(i64::from(link_mbps)));
    }
    let _ = fields.insert(
        "reason_class",
        FieldValue::String("adapter_snapshot_changed".to_owned()),
    );
    fields
}

/// `NETWORK_PATH_LOST` fields for one snapshot.
pub(crate) fn lost_fields(snapshot: &NetworkSnapshot) -> StructuredFields {
    let mut fields = StructuredFields::default();
    let _ = fields.insert(
        "interface_kind",
        FieldValue::String(interface_kind_name(snapshot.interface_kind).to_owned()),
    );
    fields
}

/// `NETWORK_PATH_RESTORED` fields for one snapshot plus the outage duration.
pub(crate) fn restored_fields(snapshot: &NetworkSnapshot, gap_ms: u64) -> StructuredFields {
    let mut fields = lost_fields(snapshot);
    let _ = fields.insert(
        "gap_ms",
        FieldValue::Integer(i64::try_from(gap_ms).unwrap_or(i64::MAX)),
    );
    fields
}

fn interface_kind_name(kind: InterfaceKind) -> &'static str {
    match kind {
        InterfaceKind::Ethernet => "ethernet",
        InterfaceKind::Wifi => "wifi",
        InterfaceKind::Cellular => "cellular",
        InterfaceKind::Vpn => "vpn",
        InterfaceKind::Loopback => "loopback",
        InterfaceKind::Other => "other",
    }
}

fn network_scope_name(scope: NetworkScope) -> &'static str {
    match scope {
        NetworkScope::Lan => "lan",
        NetworkScope::Wan => "wan",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static FIXTURE_ID: AtomicU64 = AtomicU64::new(0);

    struct Fixture {
        root: std::path::PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let id = FIXTURE_ID.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir()
                .join(format!("arcen-linux-netinfo-{}-{id}", std::process::id()));
            fs::create_dir_all(&root).unwrap();
            Self { root }
        }

        fn sys_class_net(&self) -> std::path::PathBuf {
            self.root.join("sys-class-net")
        }

        fn proc_net_route(&self) -> std::path::PathBuf {
            self.root.join("proc-net-route")
        }

        fn proc_net_ipv6_route(&self) -> std::path::PathBuf {
            self.root.join("proc-net-ipv6-route")
        }

        fn add_interface(&self, name: &str, operstate: &str, type_value: &str, mtu: &str) {
            let dir = self.sys_class_net().join(name);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("operstate"), operstate).unwrap();
            fs::write(dir.join("type"), type_value).unwrap();
            fs::write(dir.join("mtu"), mtu).unwrap();
        }

        fn mark_wireless(&self, name: &str) {
            fs::create_dir_all(self.sys_class_net().join(name).join("wireless")).unwrap();
        }

        fn set_speed(&self, name: &str, speed: &str) {
            fs::write(self.sys_class_net().join(name).join("speed"), speed).unwrap();
        }

        fn set_ifindex(&self, name: &str, ifindex: u32) {
            fs::write(
                self.sys_class_net().join(name).join("ifindex"),
                ifindex.to_string(),
            )
            .unwrap();
        }

        fn write_route(&self, lines: &[&str]) {
            let mut content = String::from("Iface\tDestination\tGateway\tFlags\n");
            for line in lines {
                content.push_str(line);
                content.push('\n');
            }
            fs::write(self.proc_net_route(), content).unwrap();
        }

        /// Writes a full 11-column `/proc/net/route` table: each entry is
        /// `(interface, destination_hex, mask_hex, metric)`, matching the
        /// real kernel format `best_ipv4_route` parses (byte-swapped hex,
        /// decimal metric).
        fn write_route_full(&self, routes: &[(&str, &str, &str, &str)]) {
            let mut content = String::from(
                "Iface\tDestination\tGateway\tFlags\tRefCnt\tUse\tMetric\tMask\tMTU\tWindow\tIRTT\n",
            );
            for (interface, destination, mask, metric) in routes {
                content.push_str(&format!(
                    "{interface}\t{destination}\t00000000\t0001\t0\t0\t{metric}\t{mask}\t0\t0\t0\n"
                ));
            }
            fs::write(self.proc_net_route(), content).unwrap();
        }

        /// Writes a `/proc/net/ipv6_route` table: each entry is
        /// `(dest_addr_hex32, prefix_len_hex, metric_hex, interface)`.
        fn write_ipv6_route(&self, routes: &[(&str, &str, &str, &str)]) {
            let mut content = String::new();
            for (dest, prefix_len, metric, interface) in routes {
                content.push_str(&format!(
                    "{dest} {prefix_len} 00000000000000000000000000000000 00 \
                     00000000000000000000000000000000 {metric} 00000001 00000000 00000001 \
                     {interface}\n"
                ));
            }
            fs::write(self.proc_net_ipv6_route(), content).unwrap();
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn interface_kinds_are_normalized() {
        let fixture = Fixture::new();
        fixture.add_interface("eth0", "up", "1", "1500");
        fixture.add_interface("wlan0", "up", "1", "1500");
        fixture.mark_wireless("wlan0");
        fixture.add_interface("wg0", "up", "65534", "1420");
        fixture.add_interface("lo", "unknown", "772", "65536");
        fixture.add_interface("mystery0", "up", "512", "1500");

        assert_eq!(
            classify_interface(&fixture.sys_class_net(), "eth0"),
            InterfaceKind::Ethernet
        );
        assert_eq!(
            classify_interface(&fixture.sys_class_net(), "wlan0"),
            InterfaceKind::Wifi
        );
        assert_eq!(
            classify_interface(&fixture.sys_class_net(), "wg0"),
            InterfaceKind::Vpn
        );
        assert_eq!(
            classify_interface(&fixture.sys_class_net(), "lo"),
            InterfaceKind::Loopback
        );
        assert_eq!(
            classify_interface(&fixture.sys_class_net(), "mystery0"),
            InterfaceKind::Other
        );
    }

    #[test]
    fn default_route_selects_the_routed_interface_over_priority_fallback() {
        let fixture = Fixture::new();
        fixture.add_interface("eth0", "down", "1", "1500");
        fixture.add_interface("wlan0", "up", "1", "1500");
        fixture.mark_wireless("wlan0");
        fixture.set_speed("wlan0", "866");
        fixture.write_route_full(&[
            ("wlan0", "00000000", "00000000", "3"),
            ("wlan0", "0002A8C0", "00FFFFFF", "1"),
        ]);

        let snapshot = snapshot_at(
            &fixture.sys_class_net(),
            &fixture.proc_net_route(),
            &fixture.proc_net_ipv6_route(),
            "203.0.113.9:12345",
            None,
        )
        .expect("snapshot resolves");
        assert_eq!(snapshot.interface_kind, InterfaceKind::Wifi);
        assert_eq!(snapshot.link_mbps, Some(866));
        assert_eq!(snapshot.scope, NetworkScope::Wan);
        assert_eq!(snapshot.ssid, None, "SSID must never be disclosed");
        assert_eq!(snapshot.rssi_dbm, None, "RSSI must never be disclosed");
    }

    #[test]
    fn priority_fallback_prefers_ethernet_over_wifi_when_route_is_unreadable() {
        let fixture = Fixture::new();
        fixture.add_interface("wlan0", "up", "1", "1500");
        fixture.mark_wireless("wlan0");
        fixture.add_interface("eth0", "up", "1", "1500");
        // No route file written: falls back to priority selection.

        let snapshot = snapshot_at(
            &fixture.sys_class_net(),
            &fixture.proc_net_route(),
            &fixture.proc_net_ipv6_route(),
            "192.168.1.20:5000",
            None,
        )
        .expect("snapshot resolves via fallback");
        assert_eq!(snapshot.interface_kind, InterfaceKind::Ethernet);
        assert_eq!(snapshot.scope, NetworkScope::Lan);
    }

    #[test]
    fn no_operational_interfaces_yields_no_snapshot() {
        let fixture = Fixture::new();
        fixture.add_interface("eth0", "down", "1", "1500");
        fixture.add_interface("lo", "unknown", "772", "65536");

        assert!(snapshot_at(
            &fixture.sys_class_net(),
            &fixture.proc_net_route(),
            &fixture.proc_net_ipv6_route(),
            "10.0.0.5:1",
            None,
        )
        .is_none());
    }

    #[test]
    fn multi_homed_ipv4_prefers_the_longest_matching_prefix_over_the_default_route() {
        // eth0 owns the default route; eth1 has a more-specific route to
        // the peer's subnet (192.168.50.0/24). The longest-prefix match
        // must win over the default route, proving multi-homed routing
        // does not simply follow the default-route interface.
        let fixture = Fixture::new();
        fixture.add_interface("eth0", "up", "1", "1500");
        fixture.add_interface("eth1", "up", "1", "1500");
        fixture.set_speed("eth0", "1000");
        fixture.set_speed("eth1", "100");
        fixture.write_route_full(&[
            ("eth0", "00000000", "00000000", "100"),
            // 192.168.50.0/24 -> host order 0xC0A83200, byte-swapped "0032A8C0".
            ("eth1", "0032A8C0", "00FFFFFF", "0"),
        ]);

        let snapshot = snapshot_at(
            &fixture.sys_class_net(),
            &fixture.proc_net_route(),
            &fixture.proc_net_ipv6_route(),
            "192.168.50.7:1",
            None,
        )
        .expect("snapshot resolves");
        assert_eq!(
            snapshot.link_mbps,
            Some(100),
            "the /24 route via eth1 must win over eth0's default route"
        );
    }

    #[test]
    fn vpn_route_wins_over_default_route_when_more_specific() {
        // wg0 has a more-specific route (10.8.0.0/24) to the peer than
        // eth0's default route: the VPN tunnel must be selected.
        let fixture = Fixture::new();
        fixture.add_interface("eth0", "up", "1", "1500");
        fixture.add_interface("wg0", "up", "65534", "1420");
        fixture.write_route_full(&[
            ("eth0", "00000000", "00000000", "100"),
            // 10.8.0.0/24 -> host order 0x0A080000, byte-swapped "0000080A".
            ("wg0", "0000080A", "00FFFFFF", "0"),
        ]);

        let snapshot = snapshot_at(
            &fixture.sys_class_net(),
            &fixture.proc_net_route(),
            &fixture.proc_net_ipv6_route(),
            "10.8.0.5:51820",
            None,
        )
        .expect("snapshot resolves");
        assert_eq!(snapshot.interface_kind, InterfaceKind::Vpn);
        assert_eq!(snapshot.mtu, Some(1420));
    }

    #[test]
    fn metric_tiebreaks_equal_prefix_length_routes() {
        // Two routes to the exact same /24 subnet via two different
        // interfaces: the lower-metric (more preferred) route must win.
        let fixture = Fixture::new();
        fixture.add_interface("eth0", "up", "1", "1500");
        fixture.add_interface("eth1", "up", "1", "1500");
        fixture.set_speed("eth0", "1000");
        fixture.set_speed("eth1", "100");
        fixture.write_route_full(&[
            ("eth0", "0032A8C0", "00FFFFFF", "50"),
            ("eth1", "0032A8C0", "00FFFFFF", "600"),
        ]);

        let snapshot = snapshot_at(
            &fixture.sys_class_net(),
            &fixture.proc_net_route(),
            &fixture.proc_net_ipv6_route(),
            "192.168.50.7:1",
            None,
        )
        .expect("snapshot resolves");
        assert_eq!(
            snapshot.link_mbps,
            Some(1000),
            "eth0's lower metric (50 < 600) must win the tiebreak"
        );
    }

    #[test]
    fn ipv6_peer_resolves_via_the_more_specific_ipv6_route() {
        // fd00::/8 default-ish route via eth0, plus a more specific
        // fd00:1::/32 route via wg0 that must win for a peer inside it.
        let fixture = Fixture::new();
        fixture.add_interface("eth0", "up", "1", "1500");
        fixture.add_interface("wg0", "up", "65534", "1420");
        fixture.write_ipv6_route(&[
            ("00000000000000000000000000000000", "00", "00000064", "eth0"),
            ("fd000001000000000000000000000000", "20", "00000000", "wg0"),
        ]);

        let snapshot = snapshot_at(
            &fixture.sys_class_net(),
            &fixture.proc_net_route(),
            &fixture.proc_net_ipv6_route(),
            "[fd00:1::5]:1",
            None,
        )
        .expect("snapshot resolves via IPv6 route");
        assert_eq!(snapshot.interface_kind, InterfaceKind::Vpn);
        assert_eq!(snapshot.mtu, Some(1420));
    }

    #[test]
    fn link_local_ipv6_peer_resolves_via_scope_id_over_the_route_table() {
        // Both interfaces are otherwise route-table-indistinguishable for a
        // link-local peer (the same `fe80::/10` prefix is valid on every
        // interface), so only the `SocketAddrV6` zone/scope id — matched
        // against each interface's kernel `ifindex` — can disambiguate
        // which one the peer actually reached us on. `eth0` owns the
        // (wrong, ambiguous) default route; `wg0`'s ifindex is the real
        // scope id and must win despite having no route entry at all.
        let fixture = Fixture::new();
        fixture.add_interface("eth0", "up", "1", "1500");
        fixture.add_interface("wg0", "up", "65534", "1420");
        fixture.set_ifindex("eth0", 2);
        fixture.set_ifindex("wg0", 7);
        fixture.write_ipv6_route(&[("00000000000000000000000000000000", "00", "00000064", "eth0")]);

        let snapshot = snapshot_at(
            &fixture.sys_class_net(),
            &fixture.proc_net_route(),
            &fixture.proc_net_ipv6_route(),
            "[fe80::1]:1",
            Some(7),
        )
        .expect("snapshot resolves via scope id");
        assert_eq!(
            snapshot.interface_kind,
            InterfaceKind::Vpn,
            "wg0 (ifindex 7, matching the peer's scope id) must win over eth0's default route"
        );
        assert_eq!(snapshot.mtu, Some(1420));
    }

    #[test]
    fn scope_id_is_ignored_for_a_non_link_local_peer() {
        // A global/WAN peer address is unambiguous without a zone id: an
        // incidentally-present scope id (e.g. stale from a prior link-local
        // hop) must not override the ordinary route-table resolution.
        let fixture = Fixture::new();
        fixture.add_interface("eth0", "up", "1", "1500");
        fixture.add_interface("wg0", "up", "65534", "1420");
        fixture.set_ifindex("wg0", 7);
        fixture.write_ipv6_route(&[("00000000000000000000000000000000", "00", "00000064", "eth0")]);

        let snapshot = snapshot_at(
            &fixture.sys_class_net(),
            &fixture.proc_net_route(),
            &fixture.proc_net_ipv6_route(),
            "[2001:db8::1]:1",
            Some(7),
        )
        .expect("snapshot resolves via route table");
        assert_eq!(
            snapshot.interface_kind,
            InterfaceKind::Ethernet,
            "a non-link-local peer must resolve via the route table, not the incidental scope id"
        );
    }

    #[test]
    fn unresolvable_nonzero_scope_id_returns_unavailable_never_a_fallback() {
        // Re-review finding #4: a scope id that matches no interface's
        // ifindex (e.g. an interface that has since been removed) must
        // return unavailable/`None` for a link-local peer, never fall back
        // to the route table or "best" interface — that fallback could
        // silently report facts for the wrong interface, since the scope
        // id was the only signal that could disambiguate which interface
        // this specific peer actually reached us on.
        let fixture = Fixture::new();
        fixture.add_interface("eth0", "up", "1", "1500");
        fixture.set_ifindex("eth0", 2);
        fixture.write_ipv6_route(&[("00000000000000000000000000000000", "00", "00000064", "eth0")]);

        let snapshot = snapshot_at(
            &fixture.sys_class_net(),
            &fixture.proc_net_route(),
            &fixture.proc_net_ipv6_route(),
            "[fe80::1]:1",
            Some(99),
        );
        assert!(
            snapshot.is_none(),
            "an unresolved nonzero scope id must yield unavailable, never a route-table/best-\
             interface guess"
        );
    }

    #[test]
    fn absent_scope_id_still_falls_back_to_the_route_table_for_a_link_local_peer() {
        // The exact-ifindex-only requirement is scoped to a *nonzero*
        // scope id specifically: when no scope id is present at all (e.g.
        // an older peer, or a transport that cannot report one), the
        // ordinary route-table/best-interface fallback remains available,
        // since there was never a disambiguating signal to trust or
        // distrust in the first place.
        let fixture = Fixture::new();
        fixture.add_interface("eth0", "up", "1", "1500");
        fixture.set_ifindex("eth0", 2);
        fixture.write_ipv6_route(&[("00000000000000000000000000000000", "00", "00000064", "eth0")]);

        let snapshot = snapshot_at(
            &fixture.sys_class_net(),
            &fixture.proc_net_route(),
            &fixture.proc_net_ipv6_route(),
            "[fe80::1]:1",
            None,
        )
        .expect("no scope id must still fall back to the route table");
        assert_eq!(snapshot.interface_kind, InterfaceKind::Ethernet);
    }

    #[test]
    fn interface_by_ifindex_finds_the_matching_interface_and_bounds_its_read() {
        let fixture = Fixture::new();
        fixture.add_interface("eth0", "up", "1", "1500");
        fixture.add_interface("wg0", "up", "65534", "1420");
        fixture.set_ifindex("eth0", 2);
        fixture.set_ifindex("wg0", 7);

        assert_eq!(
            interface_by_ifindex(&fixture.sys_class_net(), 7),
            Some("wg0".to_owned())
        );
        assert_eq!(interface_by_ifindex(&fixture.sys_class_net(), 42), None);
    }

    #[test]
    fn lifecycle_field_helpers_stay_bounded_and_ssid_free() {
        let snapshot = NetworkSnapshot::new(
            InterfaceKind::Wifi,
            Some(866),
            None::<String>,
            None,
            NetworkScope::Lan,
            Some(1_500),
        )
        .unwrap();
        let fields = lifecycle_fields(&snapshot);
        let map = fields.as_map();
        assert_eq!(
            map.get("interface_kind"),
            Some(&FieldValue::String("wifi".to_owned()))
        );
        assert!(!map.contains_key("ssid"));
        assert!(!map.contains_key("rssi_dbm"));
    }

    /// Finding #5: `NETWORK_PATH_CHANGED` (1701) must report both the old
    /// and new interface kind and link rate so an operator can see exactly
    /// what changed, without ever carrying SSID/RSSI.
    #[test]
    fn changed_fields_reports_old_and_new_kind_and_mbps() {
        let old = NetworkSnapshot::new(
            InterfaceKind::Wifi,
            Some(300),
            None::<String>,
            None,
            NetworkScope::Lan,
            Some(1_500),
        )
        .unwrap();
        let new = NetworkSnapshot::new(
            InterfaceKind::Ethernet,
            Some(1_000),
            None::<String>,
            None,
            NetworkScope::Lan,
            Some(1_500),
        )
        .unwrap();

        let fields = changed_fields(&old, &new);
        let map = fields.as_map();
        assert_eq!(
            map.get("old_kind"),
            Some(&FieldValue::String("wifi".to_owned()))
        );
        assert_eq!(
            map.get("new_kind"),
            Some(&FieldValue::String("ethernet".to_owned()))
        );
        assert_eq!(map.get("old_mbps"), Some(&FieldValue::Integer(300)));
        assert_eq!(map.get("new_mbps"), Some(&FieldValue::Integer(1_000)));
        assert!(!map.contains_key("ssid"));
        assert!(!map.contains_key("rssi_dbm"));
    }

    /// Finding #5: `NETWORK_PATH_LOST` (1702) must report the interface
    /// kind that was lost, bounded to that one field.
    #[test]
    fn lost_fields_reports_interface_kind() {
        let snapshot = NetworkSnapshot::new(
            InterfaceKind::Vpn,
            Some(500),
            None::<String>,
            None,
            NetworkScope::Wan,
            Some(1_400),
        )
        .unwrap();

        let fields = lost_fields(&snapshot);
        let map = fields.as_map();
        assert_eq!(
            map.get("interface_kind"),
            Some(&FieldValue::String("vpn".to_owned()))
        );
        assert!(!map.contains_key("ssid"));
        assert!(!map.contains_key("rssi_dbm"));
    }

    /// Finding #5: `NETWORK_PATH_RESTORED` (1703) must include everything
    /// `lost_fields` does plus the measured outage duration `gap_ms`, so a
    /// restore's dashboard shows both what came back and how long it was
    /// down.
    #[test]
    fn restored_fields_includes_gap_ms() {
        let snapshot = NetworkSnapshot::new(
            InterfaceKind::Ethernet,
            Some(1_000),
            None::<String>,
            None,
            NetworkScope::Lan,
            Some(1_500),
        )
        .unwrap();

        let fields = restored_fields(&snapshot, 4_200);
        let map = fields.as_map();
        assert_eq!(
            map.get("interface_kind"),
            Some(&FieldValue::String("ethernet".to_owned()))
        );
        assert_eq!(map.get("gap_ms"), Some(&FieldValue::Integer(4_200)));
    }
}
