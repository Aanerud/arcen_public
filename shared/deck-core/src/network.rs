// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Network facts the Deck reports in its hello and its connection test.

use arcen_protocol::messages::ClientNetworkSnapshotMsg;

/// The outcome of probing the path to a Pier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkProbeResult {
    /// Interface kind, scope, link speed and MTU of the path, when known.
    pub snapshot: Option<ClientNetworkSnapshotMsg>,
    /// Whether the platform needed (and had) permission to name the
    /// network.
    pub permission: NetworkIdentityPermission,
}

/// Whether naming the local network needs a permission the Deck lacks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkIdentityPermission {
    /// No permission was needed or asked for.
    NotRequested,
    /// The platform withheld the network's identity.
    Unavailable,
}

/// The interface traffic to a Pier leaves through, for the connection
/// test's path step.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OutgoingInterface {
    /// For example `Wi-Fi (en0)` or `Ethernet (Ethernet 2)`.
    pub description: Option<String>,
    /// The interface MTU.
    pub mtu: Option<u32>,
}
