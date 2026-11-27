// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Whether another remote-access product's client is attached, from the
//! evidence a Pier can read (ADR 0025 §13): the half of
//! [`super::RemoteAttachment`] that is not Arcen's own.
//!
//! A Linux Pier has three sources, and every one must be proved empty
//! before a session counts as having no remote client:
//!
//! 1. another product's client input: a keyboard or pointer made in user
//!    space (`console_session::another_products_client_input`);
//! 2. a remote-access server from [`REMOTE_ACCESS_SERVERS`]: a server
//!    process holding any established TCP connection (IPv4 or IPv6,
//!    loopback included), whichever desktop it serves; a relay-based tool, merely running, makes the answer
//!    unknown (nothing on the host says whether anyone is connected);
//! 3. a screen-cast or remote-desktop session of the desktop's own
//!    compositor.
//!
//! A source that cannot be read makes the answer unknown, never "nobody".
//! A tool that is not in the list and makes no input device is not seen:
//! the list is the supported set, and it grows by adding a name here.

use std::collections::BTreeSet;

use super::{RemoteAttachment, RemoteClient};
use crate::console_session::ConsoleOrigin;

/// What tells that someone is connected through a server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionSignal {
    /// A server: a viewer may be connected whenever one of its processes
    /// holds any established TCP connection, inbound or outbound (an
    /// inetd-started server, a reverse connection to a listening viewer,
    /// a tunnel over loopback). This errs towards refusal on purpose.
    EstablishedConnection,
    /// A relay-based tool: it keeps outbound connections to its own
    /// service whether or not anyone is connected, and nothing on the host
    /// tells the two apart. While one runs, whether a client is attached
    /// cannot be told.
    None,
}

/// A remote-access server process a Pier looks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemoteAccessServer {
    /// The process's executable name, or its name as the kernel keeps it.
    pub name: &'static str,
    /// What it is, for logs and documentation.
    pub kind: &'static str,
    /// What tells that someone is connected through it.
    pub signal: SessionSignal,
}

const fn server(name: &'static str, kind: &'static str) -> RemoteAccessServer {
    RemoteAccessServer {
        name,
        kind,
        signal: SessionSignal::EstablishedConnection,
    }
}

const fn relayed(name: &'static str) -> RemoteAccessServer {
    RemoteAccessServer {
        name,
        kind: "remote-support client",
        signal: SessionSignal::None,
    }
}

/// The remote-access servers the Linux Pier recognises. Another product
/// that serves the console through virtual input devices is recognised by
/// those devices instead (source 1).
pub const REMOTE_ACCESS_SERVERS: &[RemoteAccessServer] = &[
    server("x11vnc", "VNC server"),
    server("x0vncserver", "VNC server"),
    server("x0tigervncserver", "VNC server"),
    server("Xvnc", "VNC server"),
    server("Xtigervnc", "VNC server"),
    server("vncserver", "VNC server"),
    server("gnome-remote-desktop-daemon", "GNOME Remote Desktop"),
    server("grd", "GNOME Remote Desktop"),
    server("xrdp", "RDP server"),
    server("xrdp-sesman", "RDP server"),
    server("nxserver", "NX server"),
    server("nxserver.bin", "NX server"),
    server("nxnode", "NX server"),
    server("nxnode.bin", "NX server"),
    server("nxd", "NX server"),
    relayed("anydesk"),
    relayed("teamviewerd"),
    relayed("TeamViewer"),
    relayed("rustdesk"),
];

/// The kernel's limit on a process name (`/proc/<pid>/comm`), in bytes.
const COMM_BYTES: usize = 15;

/// Whether the kernel's name `comm` is `name`, whole or cut at
/// [`COMM_BYTES`].
fn comm_is(comm: &str, name: &str) -> bool {
    comm == name || (comm.len() == COMM_BYTES && name.len() > COMM_BYTES && name.starts_with(comm))
}

/// The server a process is, from its name (`comm`, cut by the kernel)
/// or its executable's file name: either one naming a server is enough
/// (an alternatives link runs a server under another executable name, and
/// a wrapper under another process name).
#[must_use]
pub fn remote_access_server(
    comm: &str,
    exe_name: Option<&str>,
) -> Option<&'static RemoteAccessServer> {
    let comm = comm.trim_end_matches('\n');
    REMOTE_ACCESS_SERVERS
        .iter()
        .find(|server| exe_name == Some(server.name) || comm_is(comm, server.name))
}

/// Why a TCP table could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TableUnreadable;

/// TCP state `ESTABLISHED`.
pub const TCP_ESTABLISHED: u8 = 0x01;

/// One row of a Linux TCP table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TcpSocket {
    /// The local port.
    pub local_port: u16,
    /// The kernel's TCP state.
    pub state: u8,
    /// The socket's inode.
    pub inode: u64,
}

/// The rows of a Linux TCP table (`/proc/<pid>/net/tcp` or `tcp6`).
///
/// # Errors
///
/// A table without its header, or a row without a local address, a state
/// or an inode.
pub fn tcp_sockets(table: &str) -> Result<Vec<TcpSocket>, TableUnreadable> {
    let mut lines = table.lines();
    if !lines
        .next()
        .is_some_and(|header| header.trim_start().starts_with("sl"))
    {
        return Err(TableUnreadable);
    }
    lines
        .filter(|row| !row.trim().is_empty())
        .map(|row| {
            let fields: Vec<&str> = row.split_whitespace().collect();
            let (Some(local), Some(state), Some(inode)) =
                (fields.get(1), fields.get(3), fields.get(9))
            else {
                return Err(TableUnreadable);
            };
            let port = local.rsplit_once(':').ok_or(TableUnreadable)?.1;
            Ok(TcpSocket {
                local_port: u16::from_str_radix(port, 16).map_err(|_| TableUnreadable)?,
                state: u8::from_str_radix(state, 16).map_err(|_| TableUnreadable)?,
                inode: inode.parse().map_err(|_| TableUnreadable)?,
            })
        })
        .collect()
}

/// The established connections among a server process's sockets
/// (`owned`), in either direction.
#[must_use]
pub fn established_connections(owned: &BTreeSet<u64>, sockets: &[TcpSocket]) -> BTreeSet<u64> {
    sockets
        .iter()
        .filter(|socket| {
            socket.state == TCP_ESTABLISHED && socket.inode != 0 && owned.contains(&socket.inode)
        })
        .map(|socket| socket.inode)
        .collect()
}

/// The inode of a descriptor that is a socket (`socket:[123]`).
#[must_use]
pub fn socket_inode(link: &str) -> Option<u64> {
    link.strip_prefix("socket:[")?
        .strip_suffix(']')?
        .parse()
        .ok()
}

/// Why a running relay-based tool leaves the attachment unknown.
#[must_use]
pub fn undecidable_reason(server: &RemoteAccessServer) -> String {
    format!(
        "cannot tell whether someone is connected through {}",
        server.name
    )
}

/// What one source showed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Evidence {
    /// Read, and nothing was there.
    Absent,
    /// Read, and these were there (a device, a server and its connections,
    /// a compositor session).
    Present(Vec<String>),
    /// Could not be read.
    Unreadable,
    /// Read, and what was there cannot tell whether a client is attached
    /// (the reasons, for the log).
    Undecidable(Vec<String>),
}

/// The three sources for one desktop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignEvidence {
    /// Another product's client input.
    pub product_client: Evidence,
    /// Remote-access servers with an established connection.
    pub remote_access: Evidence,
    /// The compositor's own screen-cast or remote-desktop sessions.
    pub compositor_sessions: Evidence,
}

impl ForeignEvidence {
    fn sources(&self) -> [&Evidence; 3] {
        [
            &self.product_client,
            &self.remote_access,
            &self.compositor_sessions,
        ]
    }

    /// Another product's client is attached when any source shows one; no
    /// client is proved only when every source was read and showed none;
    /// otherwise it is unknown. On a desktop another product started
    /// (`origin`), that product's own client input is the product using its
    /// desktop ([`RemoteClient::OwningProduct`]); every other source, and
    /// that input on any other desktop, is [`RemoteClient::Foreign`].
    #[must_use]
    pub fn attachment(&self, origin: ConsoleOrigin) -> RemoteAttachment {
        let sources = self.sources();
        if origin == ConsoleOrigin::OtherProduct
            && matches!(self.product_client, Evidence::Present(_))
        {
            RemoteAttachment::Attached(RemoteClient::OwningProduct)
        } else if sources
            .iter()
            .any(|source| matches!(source, Evidence::Present(_)))
        {
            RemoteAttachment::Attached(RemoteClient::Foreign)
        } else if sources
            .iter()
            .any(|source| matches!(source, Evidence::Unreadable | Evidence::Undecidable(_)))
        {
            RemoteAttachment::Unknown
        } else {
            RemoteAttachment::ProvedDetached
        }
    }

    /// Why the attachment is unknown, for the log: the reasons each
    /// undecidable source gave, and each source that could not be read.
    #[must_use]
    pub fn unknown_reasons(&self) -> Vec<String> {
        let names = [
            "another product's client input",
            "remote-access servers",
            "the compositor's sessions",
        ];
        self.sources()
            .iter()
            .zip(names)
            .flat_map(|(source, name)| match source {
                Evidence::Unreadable => vec![format!("{name} could not be read")],
                Evidence::Undecidable(reasons) => reasons.clone(),
                Evidence::Absent | Evidence::Present(_) => Vec::new(),
            })
            .collect()
    }

    /// A stable description of everything seen, compared between an offer
    /// and its confirmation: any change (a client that came, went or was
    /// replaced) makes the offer stale.
    #[must_use]
    pub fn fingerprint(&self) -> String {
        self.sources()
            .iter()
            .map(|source| match source {
                Evidence::Absent => "-".to_owned(),
                Evidence::Unreadable => "?".to_owned(),
                Evidence::Undecidable(items) => format!("?{}", items.join(",")),
                Evidence::Present(items) => {
                    let mut items = items.clone();
                    items.sort();
                    items.join(",")
                }
            })
            .collect::<Vec<_>>()
            .join("|")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TABLE: &str = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n   0: 00000000:170C 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 41001 1 0000000000000000 100 0 0 10 0\n   1: 0100007F:170C 0100007F:D2B4 01 00000000:00000000 00:00000000 00000000  1000        0 41002 1 0000000000000000 20 4 30 10 -1\n   2: 0100007F:D2B5 5DB8D822:01BB 01 00000000:00000000 00:00000000 00000000  1000        0 41003 1 0000000000000000 20 4 30 10 -1\n";

    fn evidence(product: Evidence, servers: Evidence, compositor: Evidence) -> ForeignEvidence {
        ForeignEvidence {
            product_client: product,
            remote_access: servers,
            compositor_sessions: compositor,
        }
    }

    #[test]
    fn servers_are_matched_by_executable_or_by_the_kernels_name() {
        let name = |comm: &str, exe: Option<&str>| remote_access_server(comm, exe).map(|s| s.name);
        assert_eq!(name("x11vnc", Some("x11vnc")), Some("x11vnc"));
        assert_eq!(name("x11vnc", None), Some("x11vnc"));
        assert_eq!(
            name("gnome-remote-de", None),
            Some("gnome-remote-desktop-daemon")
        );
        // An alternatives link: the process is named x0vncserver, the
        // executable is TigerVNC's.
        assert!(name("x0vncserver", Some("x0tigervncserver")).is_some());
        assert_eq!(name("x0tigervncserve", None), Some("x0tigervncserver"));
        assert_eq!(name("Xtigervnc", Some("Xtigervnc")), Some("Xtigervnc"));
        assert_eq!(
            name("nxserver.bin", Some("nxserver.bin")),
            Some("nxserver.bin")
        );
        assert_eq!(name("nxnode.bin", Some("nxnode.bin")), Some("nxnode.bin"));
        // A wrapper whose executable is an interpreter is found by its name.
        assert_eq!(name("x11vnc", Some("python3.9")), Some("x11vnc"));
        assert_eq!(name("gnome-shell", Some("gnome-shell")), None);
        assert_eq!(name("x11", None), None);
    }

    #[test]
    fn relay_tools_have_no_session_signal() {
        for relayed in ["anydesk", "teamviewerd", "TeamViewer", "rustdesk"] {
            let server = remote_access_server(relayed, None).unwrap();
            assert_eq!(server.signal, SessionSignal::None, "{relayed}");
            assert!(undecidable_reason(server).contains(relayed));
        }
        assert_eq!(
            remote_access_server("x11vnc", None).unwrap().signal,
            SessionSignal::EstablishedConnection
        );
    }

    fn row(slot: u32, local: &str, remote: &str, state: &str, inode: u64) -> String {
        format!(
            "   {slot}: {local} {remote} {state} 00000000:00000000 00:00000000 00000000  1000        0 {inode} 1 0000000000000000 20 4 30 10 -1\n"
        )
    }

    const HEADER: &str = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n";

    #[test]
    fn any_established_connection_of_a_server_counts() {
        let sockets = tcp_sockets(TABLE).unwrap();
        assert_eq!(sockets.len(), 3);
        let owned = BTreeSet::from([41001, 41002, 41003]);
        // The accepted viewer and the outbound connection; never the listener.
        assert_eq!(
            established_connections(&owned, &sockets),
            BTreeSet::from([41002, 41003])
        );
        // Another process's connections are not this server's.
        assert!(established_connections(&BTreeSet::from([41001]), &sockets).is_empty());
        assert_eq!(tcp_sockets("  sl  local_address\n"), Ok(Vec::new()));
        assert_eq!(tcp_sockets(""), Err(TableUnreadable));
        assert_eq!(tcp_sockets("  sl\n 0: x y 01\n"), Err(TableUnreadable));
        assert_eq!(socket_inode("socket:[41002]"), Some(41002));
        assert_eq!(socket_inode("pipe:[41002]"), None);
    }

    #[test]
    fn an_inetd_started_server_with_no_listener_of_its_own_counts() {
        // `x11vnc -inetd`: the viewer's socket was accepted by xinetd or
        // systemd and handed over; the server holds no listener.
        let table = format!(
            "{HEADER}{}",
            row(0, "0A00020F:170C", "0A000263:C350", "01", 52001)
        );
        let sockets = tcp_sockets(&table).unwrap();
        assert_eq!(
            established_connections(&BTreeSet::from([52001]), &sockets),
            BTreeSet::from([52001])
        );
    }

    #[test]
    fn a_reverse_connection_from_an_ephemeral_port_counts() {
        // `x11vnc -connect viewer:5500`: the server dials out to a
        // listening viewer from an ephemeral local port.
        let table = format!(
            "{HEADER}{}{}",
            row(0, "00000000:170C", "00000000:0000", "0A", 53000),
            row(1, "0A00020F:D431", "0A000263:157C", "01", 53001)
        );
        let sockets = tcp_sockets(&table).unwrap();
        assert_eq!(
            established_connections(&BTreeSet::from([53000, 53001]), &sockets),
            BTreeSet::from([53001])
        );
    }

    #[test]
    fn a_loopback_connection_counts_in_ipv4_and_ipv6() {
        // A viewer tunnelled over SSH reaches the server on loopback.
        let v4 = format!(
            "{HEADER}{}",
            row(0, "0100007F:170C", "0100007F:A1B2", "01", 54001)
        );
        let v6 = format!(
            "{HEADER}{}",
            row(
                0,
                "00000000000000000000000001000000:170C",
                "00000000000000000000000001000000:A1B3",
                "01",
                54002
            )
        );
        let mut sockets = tcp_sockets(&v4).unwrap();
        sockets.extend(tcp_sockets(&v6).unwrap());
        assert_eq!(
            established_connections(&BTreeSet::from([54001, 54002]), &sockets),
            BTreeSet::from([54001, 54002])
        );
    }

    #[test]
    fn no_client_is_proved_only_when_every_source_was_read_empty() {
        use Evidence::{Absent, Present, Undecidable, Unreadable};
        let vnc = || Present(vec!["x11vnc[42] 1".to_owned()]);
        assert_eq!(
            evidence(Absent, Absent, Absent).attachment(ConsoleOrigin::DisplayManager),
            RemoteAttachment::ProvedDetached
        );
        assert_eq!(
            evidence(Absent, vnc(), Absent).attachment(ConsoleOrigin::DisplayManager),
            RemoteAttachment::Attached(RemoteClient::Foreign)
        );
        assert_eq!(
            evidence(Unreadable, vnc(), Absent).attachment(ConsoleOrigin::DisplayManager),
            RemoteAttachment::Attached(RemoteClient::Foreign)
        );
        let relay = evidence(
            Absent,
            Undecidable(vec!["cannot tell … teamviewerd".into()]),
            Absent,
        );
        assert_eq!(
            relay.attachment(ConsoleOrigin::DisplayManager),
            RemoteAttachment::Unknown
        );
        assert_eq!(
            relay.unknown_reasons(),
            vec!["cannot tell … teamviewerd".to_owned()]
        );
        for unreadable in [
            evidence(Unreadable, Absent, Absent),
            evidence(Absent, Unreadable, Absent),
            evidence(Absent, Absent, Unreadable),
        ] {
            assert_eq!(
                unreadable.attachment(ConsoleOrigin::DisplayManager),
                RemoteAttachment::Unknown
            );
            assert_eq!(unreadable.unknown_reasons().len(), 1);
        }
    }

    #[test]
    fn the_owning_products_client_is_only_its_input_on_its_own_desktop() {
        use Evidence::{Absent, Present, Undecidable, Unreadable};
        let device = || Present(vec!["event7 (virtual keyboard)".to_owned()]);
        let vnc = || Present(vec!["x11vnc[42] 1".to_owned()]);
        assert_eq!(
            evidence(device(), Absent, Absent).attachment(ConsoleOrigin::OtherProduct),
            RemoteAttachment::Attached(RemoteClient::OwningProduct)
        );
        assert_eq!(
            evidence(device(), vnc(), Unreadable).attachment(ConsoleOrigin::OtherProduct),
            RemoteAttachment::Attached(RemoteClient::OwningProduct)
        );
        assert_eq!(
            evidence(device(), Absent, Absent).attachment(ConsoleOrigin::DisplayManager),
            RemoteAttachment::Attached(RemoteClient::Foreign)
        );
        // Only the other remote tools: a foreign client, never the owning
        // product's, even on its desktop.
        assert_eq!(
            evidence(Absent, vnc(), Absent).attachment(ConsoleOrigin::OtherProduct),
            RemoteAttachment::Attached(RemoteClient::Foreign)
        );
        assert_eq!(
            evidence(Unreadable, Absent, Absent).attachment(ConsoleOrigin::OtherProduct),
            RemoteAttachment::Unknown
        );
        assert_eq!(
            evidence(Absent, Undecidable(vec!["relay".into()]), Absent)
                .attachment(ConsoleOrigin::OtherProduct),
            RemoteAttachment::Unknown
        );
    }

    #[test]
    fn the_fingerprint_changes_with_any_client_and_not_with_order() {
        use Evidence::{Absent, Present, Unreadable};
        let a = evidence(Absent, Present(vec!["b".into(), "a".into()]), Absent);
        let b = evidence(Absent, Present(vec!["a".into(), "b".into()]), Absent);
        assert_eq!(a.fingerprint(), b.fingerprint());
        assert_ne!(
            a.fingerprint(),
            evidence(Absent, Absent, Absent).fingerprint()
        );
        assert_ne!(
            evidence(Absent, Absent, Absent).fingerprint(),
            evidence(Absent, Absent, Unreadable).fingerprint()
        );
    }
}
