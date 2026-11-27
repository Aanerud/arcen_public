// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! A change of account on the console (ADR 0025 §13): the Linux adapter.
//!
//! Every decision is `arcen_session::session_handover`'s. This module reads
//! what the system says (logind's sessions, the boot id, another product's
//! client, the account's GNOME session manager) and, when a confirmed
//! change of account asks for it, requests GNOME's ordinary logout of
//! exactly that session and reads logind back until the session is gone.
//!
//! The logout is `org.gnome.SessionManager.Logout` with mode 1 ("no
//! confirmation"), which honours inhibitors. Nothing here forces a logout
//! (mode 2), terminates a logind session or signals a process: an
//! application that objects stops the change of account.

use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};
use std::time::Duration;

use arcen_session::console_session::holder::{console_holder, ConsoleHolder};
use arcen_session::console_session::{ConsoleSessionFact, SessionRead};
use arcen_session::direct_reconnect::AccountPrincipal;
use arcen_session::session_handover::logout::{
    LogoutOutcome, SessionExitProof, SessionPresence, LOGOUT_COMPLETION_TIMEOUT,
};
use arcen_session::session_handover::remote_access::{self, Evidence, ForeignEvidence};
use arcen_session::session_handover::{
    BootId, ConsoleOccupant, DeskState, NativePrincipal, NativeSessionId, NativeSessionIdentity,
    RemoteAttachment, RemoteClient, SessionFacts,
};
use tokio::process::Command;

use super::console;
use super::lifecycle::SessionRegistry;
use crate::logging::target;

const LOGINCTL: &str = "/usr/bin/loginctl";
const GDBUS: &str = "/usr/bin/gdbus";
const BOOT_ID: &str = "/proc/sys/kernel/random/boot_id";
const SESSION_MANAGER: &str = "org.gnome.SessionManager";
const SESSION_MANAGER_PATH: &str = "/org/gnome/SessionManager";
/// `Logout` mode 1: no confirmation dialog, inhibitors honoured. Mode 2
/// would ignore them and is never used.
const LOGOUT_NO_CONFIRMATION: &str = "1";
/// `IsInhibited` flag 1: logout.
const INHIBIT_LOGOUT: &str = "1";
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(500);
/// How long logind is read back after a logout request GNOME did not
/// acknowledge.
const UNACKNOWLEDGED_READ_BACK: Duration = Duration::from_secs(10);
/// How long the display manager may take to show its login screen again
/// after the confirmed session ended.
const LOGIN_ROUTE_WAIT: Duration = Duration::from_secs(30);

/// What the console looked like for one sign-in.
pub(crate) struct Reading {
    /// The occupant, for the shared decision.
    pub(crate) occupant: ConsoleOccupant,
    /// The session the occupant names, when it names one.
    pub(crate) session: Option<ConsoleSessionFact>,
    /// logind answered completely, and no single session could be named:
    /// the existing console decision speaks for the sign-in.
    pub(crate) several: bool,
    /// Why whether a remote client is attached could not be told.
    pub(crate) unknown: Vec<String>,
}

/// This boot's id.
pub(crate) fn boot_id() -> Option<BootId> {
    let text = std::fs::read_to_string(BOOT_ID).ok()?;
    BootId::new(text.trim()).ok()
}

/// The account `uid`/`name` as the shared contract names it.
pub(crate) fn principal(uid: u32, name: &str) -> Option<NativePrincipal> {
    NativePrincipal::from_account(AccountPrincipal {
        name,
        id: u64::from(uid),
    })
    .ok()
}

/// The session's generation in this boot: logind's creation time
/// (`TimestampMonotonic`), which differs whenever an id is used again.
async fn generation(session_id: &str) -> Option<u64> {
    if !console::safe_session_id(session_id) {
        return None;
    }
    let mut show = Command::new(LOGINCTL);
    show.args([
        "show-session",
        session_id,
        "--property=TimestampMonotonic",
        "--value",
    ]);
    let output = console::bounded_output_within(show, COMMAND_TIMEOUT).await?;
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .ok()
        .filter(|value| *value != 0)
}

/// The exact identity of a logind session.
pub(crate) async fn identity_of(fact: &ConsoleSessionFact) -> Option<NativeSessionIdentity> {
    Some(NativeSessionIdentity::new(
        principal(fact.uid, &fact.user)?,
        NativeSessionId::new(fact.id.clone()).ok()?,
        boot_id()?,
        generation(&fact.id).await?,
    ))
}

/// Another product's client input (source 1 of `remote_access`).
fn product_client_evidence() -> Evidence {
    match console::another_products_client_read() {
        Ok(None) => Evidence::Absent,
        Ok(Some(device)) => Evidence::Present(vec![device]),
        Err(error) => {
            tracing::warn!(target: target::SESSION, %error, "whether another product's client is attached could not be read");
            Evidence::Unreadable
        }
    }
}

/// Whether a process's own entry went away while it was read (it exited).
fn gone(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::NotFound || error.raw_os_error() == Some(3)
}

/// A server process's socket inodes and the TCP table of its network
/// namespace.
type ServerSockets = (
    std::collections::BTreeSet<u64>,
    Vec<remote_access::TcpSocket>,
);

/// One recognised server process's sockets; `Ok(None)` when it exited
/// meanwhile.
fn server_sockets(pid: u32) -> std::io::Result<Option<ServerSockets>> {
    let fds = match std::fs::read_dir(format!("/proc/{pid}/fd")) {
        Ok(fds) => fds,
        Err(error) if gone(&error) => return Ok(None),
        Err(error) => return Err(error),
    };
    let owned: std::collections::BTreeSet<u64> = fds
        .flatten()
        .filter_map(|fd| std::fs::read_link(fd.path()).ok())
        .filter_map(|link| remote_access::socket_inode(&link.to_string_lossy()))
        .collect();
    let mut sockets = Vec::new();
    for table in ["tcp", "tcp6"] {
        match std::fs::read_to_string(format!("/proc/{pid}/net/{table}")) {
            Ok(text) => sockets.extend(
                remote_access::tcp_sockets(&text)
                    .map_err(|_| std::io::Error::other("unreadable TCP table"))?,
            ),
            // A kernel without IPv6 has no tcp6 table.
            Err(error) if gone(&error) && table == "tcp6" => {}
            Err(error) if gone(&error) => return Ok(None),
            Err(error) => return Err(error),
        }
    }
    Ok(Some((owned, sockets)))
}

/// Every recognised remote-access server (source 2): a server process
/// with any established TCP connection is present; a relay-based tool that runs
/// makes the source undecidable. One unreadable process makes the whole
/// scan unreadable.
fn remote_access_evidence() -> Evidence {
    use std::collections::BTreeSet;
    use std::os::unix::ffi::OsStrExt as _;
    let entries = match std::fs::read_dir("/proc") {
        Ok(entries) => entries,
        Err(error) => {
            tracing::warn!(target: target::SESSION, %error, "the process table could not be read");
            return Evidence::Unreadable;
        }
    };
    let mut relays = BTreeSet::new();
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let comm = match std::fs::read(format!("/proc/{pid}/comm")) {
            Ok(comm) => String::from_utf8_lossy(&comm).into_owned(),
            Err(error) if gone(&error) => continue,
            Err(error) => {
                tracing::warn!(target: target::SESSION, pid, %error, "a process's name could not be read");
                return Evidence::Unreadable;
            }
        };
        let exe = std::fs::read_link(format!("/proc/{pid}/exe")).ok();
        let exe_name = exe
            .as_deref()
            .and_then(std::path::Path::file_name)
            .map(|name| {
                String::from_utf8_lossy(name.as_bytes())
                    .trim_end_matches(" (deleted)")
                    .to_owned()
            });
        let Some(server) = remote_access::remote_access_server(&comm, exe_name.as_deref()) else {
            continue;
        };
        if server.signal == remote_access::SessionSignal::None {
            relays.insert(remote_access::undecidable_reason(server));
            continue;
        }
        match server_sockets(pid) {
            Ok(Some((owned, sockets))) => {
                let connected = remote_access::established_connections(&owned, &sockets);
                if !connected.is_empty() {
                    let inodes: Vec<String> = connected.iter().map(u64::to_string).collect();
                    found.push(format!("{}[{pid}] {}", server.name, inodes.join(",")));
                }
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(target: target::SESSION, pid, server = server.name, %error, "a remote-access server's connections could not be read");
                return Evidence::Unreadable;
            }
        }
    }
    if !found.is_empty() {
        Evidence::Present(found)
    } else if !relays.is_empty() {
        Evidence::Undecidable(relays.into_iter().collect())
    } else {
        Evidence::Absent
    }
}

/// The desktop's own screen-cast and remote-desktop sessions (source 3):
/// Mutter's session objects on the account's bus. A bus where Mutter's
/// services have no owner has none.
async fn compositor_evidence(fact: &ConsoleSessionFact) -> Evidence {
    let Some(bus) = user_bus(fact).await else {
        return Evidence::Unreadable;
    };
    let mut found = Vec::new();
    for (service, path) in [
        (
            "org.gnome.Mutter.RemoteDesktop",
            "/org/gnome/Mutter/RemoteDesktop/Session",
        ),
        (
            "org.gnome.Mutter.ScreenCast",
            "/org/gnome/Mutter/ScreenCast/Session",
        ),
    ] {
        let owned = call(
            &bus,
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus.NameHasOwner",
            &[service],
        )
        .await
        .as_deref()
        .and_then(parse_bool);
        match owned {
            Some(false) => continue,
            Some(true) => {}
            None => return Evidence::Unreadable,
        }
        let Some(nodes) = introspect(&bus, service, path).await else {
            return Evidence::Unreadable;
        };
        found.extend(nodes.into_iter().map(|node| format!("{service}/{node}")));
    }
    if found.is_empty() {
        Evidence::Absent
    } else {
        Evidence::Present(found)
    }
}

/// The child nodes of `path` on the account's bus.
async fn introspect(bus: &UserBus, service: &str, path: &str) -> Option<Vec<String>> {
    let mut gdbus = Command::new(GDBUS);
    gdbus
        .env_clear()
        .env("DBUS_SESSION_BUS_ADDRESS", &bus.address)
        .env("PATH", "/usr/bin:/bin")
        .uid(bus.uid)
        .gid(bus.gid)
        .args([
            "introspect",
            "--session",
            "--dest",
            service,
            "--object-path",
            path,
        ]);
    let output = console::bounded_output_within(gdbus, COMMAND_TIMEOUT).await?;
    Some(child_nodes(&String::from_utf8_lossy(&output.stdout)))
}

/// The child nodes `gdbus introspect` prints (`node u1 {`), without the
/// object itself (`node /path {`).
fn child_nodes(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| line.trim().strip_prefix("node "))
        .filter_map(|rest| rest.split_whitespace().next())
        .filter(|name| !name.starts_with('/'))
        .map(str::to_owned)
        .collect()
}

/// Whether a remote client is attached to the desktop `fact`: an Arcen
/// Deck the registry holds, or another product's client by every source
/// the shared `remote_access` rule names. The foreign evidence is folded
/// into the registry's attachment epoch, so an offer made before any
/// change of it is stale.
#[allow(
    clippy::disallowed_methods,
    reason = "plain file reads that finish; nothing is left running when the caller stops waiting"
)]
async fn attachment_of(
    registry: &SessionRegistry,
    fact: &ConsoleSessionFact,
) -> (RemoteAttachment, Vec<String>) {
    let local =
        tokio::task::spawn_blocking(|| (product_client_evidence(), remote_access_evidence())).await;
    let (product_client, remote_access) =
        local.unwrap_or((Evidence::Unreadable, Evidence::Unreadable));
    let evidence = ForeignEvidence {
        product_client,
        remote_access,
        compositor_sessions: compositor_evidence(fact).await,
    };
    registry.observe_foreign(&evidence.fingerprint());
    if registry.arcen_attached(fact.uid) {
        return (RemoteAttachment::Attached(RemoteClient::Arcen), Vec::new());
    }
    let attachment = evidence.attachment(fact.origin());
    match attachment {
        RemoteAttachment::Attached(_) => tracing::info!(
            target: target::SESSION,
            session_id = %fact.id,
            evidence = %evidence.fingerprint(),
            "a remote client of another product is attached to the console session"
        ),
        RemoteAttachment::Unknown => tracing::info!(
            target: target::SESSION,
            session_id = %fact.id,
            reason = %evidence.unknown_reasons().join("; "),
            "whether a remote client is attached to the console session cannot be told"
        ),
        RemoteAttachment::ProvedDetached => {}
    }
    (attachment, evidence.unknown_reasons())
}

/// Reads who holds the console for a sign-in of `requester_uid`.
pub(crate) async fn read(registry: &SessionRegistry, requester_uid: u32) -> Reading {
    let sessions: SessionRead = console::read_sessions().await;
    let Ok(sessions) = sessions else {
        return Reading {
            occupant: ConsoleOccupant::Unknown,
            session: None,
            several: false,
            unknown: Vec::new(),
        };
    };
    let fact = match console_holder(&sessions, requester_uid) {
        ConsoleHolder::Nobody => {
            return Reading {
                occupant: ConsoleOccupant::Nobody,
                session: None,
                several: false,
                unknown: Vec::new(),
            }
        }
        ConsoleHolder::Several => {
            return Reading {
                occupant: ConsoleOccupant::Unknown,
                session: None,
                several: true,
                unknown: Vec::new(),
            }
        }
        ConsoleHolder::Session(fact) => fact.clone(),
    };
    let Some(identity) = identity_of(&fact).await else {
        tracing::warn!(target: target::SESSION, session_id = %fact.id, "the console session's identity (boot, generation) could not be read");
        return Reading {
            occupant: ConsoleOccupant::Unknown,
            session: Some(fact),
            several: false,
            unknown: Vec::new(),
        };
    };
    let (attachment, unknown) = attachment_of(registry, &fact).await;
    // Read after the attachment: an attach or detach in between makes the
    // next reading's epoch differ.
    let epoch = registry.attachment_epoch();
    let desk = if fact.locked {
        DeskState::Locked
    } else {
        DeskState::Unlocked
    };
    Reading {
        occupant: ConsoleOccupant::Session(SessionFacts {
            identity,
            attachment,
            epoch,
            desk,
        }),
        session: Some(fact),
        several: false,
        unknown,
    }
}

/// The account's session bus, proved to be its own.
struct UserBus {
    uid: u32,
    gid: u32,
    address: String,
}

/// The session bus of the account that holds `fact`, when that account
/// has exactly this one graphical session (the bus is per account, so its
/// session manager is this session's) and its bus socket is its own.
async fn user_bus(fact: &ConsoleSessionFact) -> Option<UserBus> {
    let sessions = console::read_sessions().await.ok()?;
    let graphical = sessions
        .iter()
        .filter(|s| s.uid == fact.uid && matches!(s.session_type.as_str(), "x11" | "wayland"))
        .filter(|s| s.state != "closing")
        .count();
    if graphical != 1 || !sessions.iter().any(|s| s.id == fact.id) {
        return None;
    }
    let path = format!("/run/user/{}/bus", fact.uid);
    let metadata = std::fs::symlink_metadata(&path).ok()?;
    if !metadata.file_type().is_socket() || metadata.uid() != fact.uid {
        return None;
    }
    let user = nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(fact.uid))
        .ok()
        .flatten()?;
    if user.uid.is_root() {
        return None;
    }
    Some(UserBus {
        uid: fact.uid,
        gid: user.gid.as_raw(),
        address: format!("unix:path={path}"),
    })
}

/// One `gdbus call` on the account's own bus, run as the account (never
/// as root: the bus is the account's).
async fn call(
    bus: &UserBus,
    destination: &str,
    path: &str,
    method: &str,
    args: &[&str],
) -> Option<String> {
    let mut gdbus = Command::new(GDBUS);
    gdbus
        .env_clear()
        .env("DBUS_SESSION_BUS_ADDRESS", &bus.address)
        .env("PATH", "/usr/bin:/bin")
        .uid(bus.uid)
        .gid(bus.gid)
        .args([
            "call",
            "--session",
            "--dest",
            destination,
            "--object-path",
            path,
            "--method",
            method,
        ])
        .args(args);
    let output = console::bounded_output_within(gdbus, COMMAND_TIMEOUT).await?;
    Some(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// The account's GNOME session manager, when one answers on its bus and
/// runs as the account.
async fn session_manager(fact: &ConsoleSessionFact) -> Option<UserBus> {
    let bus = user_bus(fact).await?;
    let pid = call(
        &bus,
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus.GetConnectionUnixProcessID",
        &[SESSION_MANAGER],
    )
    .await
    .and_then(|reply| parse_uint(&reply))?;
    let owner = std::fs::metadata(format!("/proc/{pid}")).ok()?.uid();
    (owner == fact.uid).then_some(bus)
}

/// `(uint32 1234,)` → 1234.
fn parse_uint(reply: &str) -> Option<u32> {
    reply
        .trim_start_matches('(')
        .trim_end_matches(')')
        .trim_end_matches(',')
        .trim()
        .trim_start_matches("uint32")
        .trim()
        .parse()
        .ok()
}

/// `(true,)` / `(false,)`.
fn parse_bool(reply: &str) -> Option<bool> {
    match reply.trim() {
        "(true,)" => Some(true),
        "(false,)" => Some(false),
        _ => None,
    }
}

/// Whether this Pier can sign the session `fact` out gracefully: the
/// account's GNOME session manager answers on its own bus.
pub(crate) async fn graceful_logout_route(fact: &ConsoleSessionFact) -> bool {
    session_manager(fact).await.is_some()
}

/// Whether the session `target` (still `fact` on logind) exists now.
async fn presence(target: &NativeSessionIdentity) -> SessionPresence {
    let Ok(sessions) = console::read_sessions().await else {
        return SessionPresence::Unknown;
    };
    let Some(listed) = sessions.iter().find(|s| s.id == target.session().as_str()) else {
        return SessionPresence::Gone;
    };
    listed_presence(
        &listed.state,
        generation(&listed.id).await,
        target.generation(),
    )
}

/// What a logind listing of the target's id says about the target: its
/// generation (`None` when it could not be read) and state.
fn listed_presence(state: &str, generation: Option<u64>, target: u64) -> SessionPresence {
    match generation {
        // logind keeps a logged-out session listed as `closing` while its
        // last processes end: the desktop is gone (as `decide_on` reads it).
        Some(generation) if generation == target && state == "closing" => SessionPresence::Gone,
        Some(generation) if generation == target => SessionPresence::Present,
        // The id belongs to a newer session: the confirmed one is gone.
        Some(_) => SessionPresence::Gone,
        // A session that ended between the list and the read is not proof.
        None => SessionPresence::Unknown,
    }
}

/// Asks GNOME to sign `target` out, honouring inhibitors, and reads logind
/// back until exactly that session is gone or
/// [`LOGOUT_COMPLETION_TIMEOUT`] passes. Never forces, terminates or kills.
/// `requested` runs just before the request, never when an inhibitor
/// already stopped it.
pub(crate) async fn sign_out(
    target: &NativeSessionIdentity,
    fact: &ConsoleSessionFact,
    requested: impl FnOnce(),
) -> LogoutOutcome {
    if fact.id != target.session().as_str() {
        return LogoutOutcome::Failed;
    }
    let Some(bus) = session_manager(fact).await else {
        tracing::warn!(target: target::SESSION, session_id = %fact.id, "the account's GNOME session manager does not answer; nothing was asked");
        return LogoutOutcome::Failed;
    };
    match call(
        &bus,
        SESSION_MANAGER,
        SESSION_MANAGER_PATH,
        "org.gnome.SessionManager.IsInhibited",
        &[INHIBIT_LOGOUT],
    )
    .await
    .as_deref()
    .and_then(parse_bool)
    {
        Some(false) => {}
        Some(true) => {
            tracing::info!(target: target::SESSION, session_id = %fact.id, "an application inhibits logout; the session is not asked to end");
            return LogoutOutcome::Vetoed;
        }
        None => return LogoutOutcome::Failed,
    }
    requested();
    let asked = call(
        &bus,
        SESSION_MANAGER,
        SESSION_MANAGER_PATH,
        "org.gnome.SessionManager.Logout",
        &[LOGOUT_NO_CONFIRMATION],
    )
    .await;
    if asked.is_none() {
        // Refused (locked down, not running) or unanswered: read back
        // anyway, briefly, in case it was taken.
        tracing::warn!(target: target::SESSION, session_id = %fact.id, "GNOME did not acknowledge the logout request");
    }
    let deadline = tokio::time::Instant::now()
        + if asked.is_some() {
            LOGOUT_COMPLETION_TIMEOUT
        } else {
            UNACKNOWLEDGED_READ_BACK
        };
    let mut objected = false;
    loop {
        if presence(target).await == SessionPresence::Gone {
            if let Some(proof) = SessionExitProof::read_back(target, SessionPresence::Gone) {
                return LogoutOutcome::Completed(proof);
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return if asked.is_some() {
                LogoutOutcome::TimedOut
            } else {
                LogoutOutcome::Failed
            };
        }
        if !objected {
            let inhibited = call(
                &bus,
                SESSION_MANAGER,
                SESSION_MANAGER_PATH,
                "org.gnome.SessionManager.IsInhibited",
                &[INHIBIT_LOGOUT],
            )
            .await
            .as_deref()
            .and_then(parse_bool);
            if inhibited == Some(true) {
                objected = true;
                tracing::warn!(target: target::SESSION, session_id = %fact.id, "an application objected to the logout after it was asked; GNOME's own dialog decides");
            }
        }
        tokio::time::sleep(POLL).await;
    }
}

/// After a confirmed session ended, waits (bounded) until the console's
/// own login can take the requester: GDM brings a new login screen up a
/// moment after the session ends. Whatever the console decides then is the
/// existing path's to say.
pub(crate) async fn wait_for_login_route(uid: u32) {
    use arcen_session::console_session::{decide_on_read, ConsoleDecision, ConsoleRefusal};
    let deadline = tokio::time::Instant::now() + LOGIN_ROUTE_WAIT;
    loop {
        let read = console::read_sessions().await;
        let decision = decide_on_read(&read, uid, super::login_session::console_host_for_sign_in());
        if !matches!(
            decision,
            ConsoleDecision::Refuse(ConsoleRefusal::NoConsoleSession)
        ) || tokio::time::Instant::now() >= deadline
        {
            return;
        }
        tokio::time::sleep(POLL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gdbus_replies_are_read_exactly() {
        assert_eq!(parse_uint("(uint32 1701586,)"), Some(1_701_586));
        assert_eq!(parse_uint("(true,)"), None);
        assert_eq!(parse_bool("(true,)"), Some(true));
        assert_eq!(parse_bool("(false,)"), Some(false));
        assert_eq!(parse_bool("()"), None);
    }

    #[test]
    fn a_closing_target_is_gone_and_a_reused_id_is_not_the_target() {
        assert_eq!(
            listed_presence("closing", Some(7), 7),
            SessionPresence::Gone
        );
        assert_eq!(
            listed_presence("active", Some(7), 7),
            SessionPresence::Present
        );
        assert_eq!(
            listed_presence("online", Some(7), 7),
            SessionPresence::Present
        );
        assert_eq!(listed_presence("active", Some(8), 7), SessionPresence::Gone);
        assert_eq!(
            listed_presence("closing", None, 7),
            SessionPresence::Unknown
        );
    }

    #[test]
    fn only_child_session_nodes_are_counted() {
        let text = "node /org/gnome/Mutter/RemoteDesktop/Session {\n  interface org.freedesktop.DBus.Peer {\n  };\n  node u12 {\n  };\n};\n";
        assert_eq!(child_nodes(text), vec!["u12".to_owned()]);
        assert!(child_nodes("node /org/gnome/Mutter/ScreenCast/Session {\n};\n").is_empty());
    }
}
