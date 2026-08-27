use arcen_telemetry::{classify_ip_literal, InterfaceKind, NetworkScope, NetworkSnapshot};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AdapterFacts {
    pub(crate) interface_type: u32,
    pub(crate) operational: bool,
    pub(crate) link_bps: u64,
    pub(crate) mtu: u32,
}

const IF_TYPE_ETHERNET_CSMACD: u32 = 6;
const IF_TYPE_PPP: u32 = 23;
const IF_TYPE_SOFTWARE_LOOPBACK: u32 = 24;
const IF_TYPE_IEEE80211: u32 = 71;
const IF_TYPE_TUNNEL: u32 = 131;

pub(crate) fn classify_adapter(interface_type: u32) -> InterfaceKind {
    match interface_type {
        IF_TYPE_ETHERNET_CSMACD => InterfaceKind::Ethernet,
        IF_TYPE_IEEE80211 => InterfaceKind::Wifi,
        IF_TYPE_PPP | IF_TYPE_TUNNEL => InterfaceKind::Vpn,
        IF_TYPE_SOFTWARE_LOOPBACK => InterfaceKind::Loopback,
        _ => InterfaceKind::Other,
    }
}

pub(crate) fn select_adapter(facts: &[AdapterFacts]) -> Option<AdapterFacts> {
    facts
        .iter()
        .copied()
        .filter(|facts| facts.operational)
        .min_by_key(|facts| match classify_adapter(facts.interface_type) {
            InterfaceKind::Ethernet => 0,
            InterfaceKind::Wifi => 1,
            InterfaceKind::Vpn => 2,
            InterfaceKind::Other => 3,
            InterfaceKind::Cellular => 4,
            InterfaceKind::Loopback => 5,
        })
}

pub(crate) fn snapshot(peer_addr: &str) -> Option<NetworkSnapshot> {
    let peer_ip = peer_addr
        .parse::<std::net::SocketAddr>()
        .map(|address| address.ip().to_string())
        .unwrap_or_else(|_| peer_addr.trim_matches(['[', ']']).to_owned());
    let scope = classify_ip_literal(&peer_ip).unwrap_or(NetworkScope::Wan);
    platform_adapters().and_then(|facts| {
        let selected = select_adapter(&facts)?;
        let link_mbps = u32::try_from(selected.link_bps / 1_000_000)
            .ok()
            .filter(|value| *value != 0);
        NetworkSnapshot::new(
            classify_adapter(selected.interface_type),
            link_mbps,
            None::<String>,
            None,
            scope,
            Some(selected.mtu),
        )
        .ok()
    })
}

pub(crate) fn lifecycle_fields(snapshot: &NetworkSnapshot) -> arcen_telemetry::StructuredFields {
    use arcen_telemetry::{FieldValue, StructuredFields};

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

pub(crate) fn changed_fields(
    old: &NetworkSnapshot,
    new: &NetworkSnapshot,
) -> arcen_telemetry::StructuredFields {
    use arcen_telemetry::{FieldValue, StructuredFields};

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

pub(crate) fn lost_fields(snapshot: &NetworkSnapshot) -> arcen_telemetry::StructuredFields {
    let mut fields = arcen_telemetry::StructuredFields::default();
    let _ = fields.insert(
        "interface_kind",
        arcen_telemetry::FieldValue::String(
            interface_kind_name(snapshot.interface_kind).to_owned(),
        ),
    );
    fields
}

pub(crate) fn restored_fields(
    snapshot: &NetworkSnapshot,
    gap_ms: u64,
) -> arcen_telemetry::StructuredFields {
    let mut fields = lost_fields(snapshot);
    let _ = fields.insert(
        "gap_ms",
        arcen_telemetry::FieldValue::Integer(i64::try_from(gap_ms).unwrap_or(i64::MAX)),
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

#[cfg(windows)]
fn platform_adapters() -> Option<Vec<AdapterFacts>> {
    use windows::Win32::Foundation::{ERROR_BUFFER_OVERFLOW, NO_ERROR};
    use windows::Win32::NetworkManagement::IpHelper::{
        GetAdaptersAddresses, GAA_FLAG_INCLUDE_PREFIX, IP_ADAPTER_ADDRESSES_LH,
    };
    use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;
    use windows::Win32::Networking::WinSock::AF_UNSPEC;

    let mut length = 0u32;
    // SAFETY: the documented sizing call supplies no output buffer and a valid
    // length out-parameter.
    let sizing = unsafe {
        GetAdaptersAddresses(
            AF_UNSPEC.0 as u32,
            GAA_FLAG_INCLUDE_PREFIX,
            None,
            None,
            &mut length,
        )
    };
    if sizing != ERROR_BUFFER_OVERFLOW.0 || length == 0 {
        return None;
    }
    let length = usize::try_from(length).ok()?;
    let records = length.div_ceil(std::mem::size_of::<IP_ADAPTER_ADDRESSES_LH>());
    let mut storage: Vec<std::mem::MaybeUninit<IP_ADAPTER_ADDRESSES_LH>> =
        Vec::with_capacity(records);
    storage.resize_with(records, std::mem::MaybeUninit::uninit);
    let head = storage.as_mut_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>();
    let mut api_length = u32::try_from(length).ok()?;
    // SAFETY: `storage` is aligned for pointer-bearing adapter records and
    // writable for at least the exact byte count returned by the sizing call;
    // `head` remains valid while the API-owned linked list is traversed below.
    let status = unsafe {
        GetAdaptersAddresses(
            AF_UNSPEC.0 as u32,
            GAA_FLAG_INCLUDE_PREFIX,
            None,
            Some(head),
            &mut api_length,
        )
    };
    if status != NO_ERROR.0 {
        return None;
    }
    let mut adapters = Vec::new();
    let mut current = head;
    while !current.is_null() {
        // SAFETY: each node belongs to the API-populated linked list in
        // `bytes`; traversal stops at the documented null terminator.
        let adapter = unsafe { &*current };
        adapters.push(AdapterFacts {
            interface_type: adapter.IfType,
            operational: adapter.OperStatus == IfOperStatusUp,
            link_bps: adapter.TransmitLinkSpeed,
            mtu: adapter.Mtu,
        });
        current = adapter.Next;
    }
    Some(adapters)
}

#[cfg(not(windows))]
fn platform_adapters() -> Option<Vec<AdapterFacts>> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interface_kinds_are_normalized() {
        assert_eq!(
            classify_adapter(IF_TYPE_ETHERNET_CSMACD),
            InterfaceKind::Ethernet
        );
        assert_eq!(classify_adapter(IF_TYPE_IEEE80211), InterfaceKind::Wifi);
        assert_eq!(classify_adapter(IF_TYPE_PPP), InterfaceKind::Vpn);
        assert_eq!(classify_adapter(IF_TYPE_TUNNEL), InterfaceKind::Vpn);
        assert_eq!(
            classify_adapter(IF_TYPE_SOFTWARE_LOOPBACK),
            InterfaceKind::Loopback
        );
        assert_eq!(classify_adapter(999), InterfaceKind::Other);
    }

    #[test]
    fn active_physical_adapter_wins_without_ssid_disclosure() {
        let selected = select_adapter(&[
            AdapterFacts {
                interface_type: IF_TYPE_SOFTWARE_LOOPBACK,
                operational: true,
                link_bps: 10_000_000_000,
                mtu: 65_536,
            },
            AdapterFacts {
                interface_type: IF_TYPE_IEEE80211,
                operational: true,
                link_bps: 866_000_000,
                mtu: 1_500,
            },
            AdapterFacts {
                interface_type: IF_TYPE_ETHERNET_CSMACD,
                operational: false,
                link_bps: 1_000_000_000,
                mtu: 1_500,
            },
        ])
        .expect("active adapter");
        assert_eq!(
            classify_adapter(selected.interface_type),
            InterfaceKind::Wifi
        );
    }
}
