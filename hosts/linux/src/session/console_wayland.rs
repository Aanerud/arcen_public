// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The account's own GNOME/Mutter Wayland session at the workstation: the
//! Linux adapter of `arcen_session::console_session::wayland_join` (ADR 0025
//! §9, GAP-042).
//!
//! Every decision is the shared module's. This adapter only reads, and
//! never acts: the session's `XDG_RUNTIME_DIR` from its own processes, the
//! runtime directory and its `bus` socket without following links, the
//! kernel's peer credentials of the process serving that socket, the
//! `org.gnome.Mutter.*` names' owner and its credentials as the user's own
//! bus reports them, and the owner process from `/proc`. It starts no bus
//! and no compositor, calls no Mutter method and changes nothing in the
//! session.
//!
//! The Pier is root, and a user's message bus does not accept root (a
//! `dbus-daemon` user bus refuses it at authentication). So, once the
//! kernel has said the account serves the socket, the bus is asked by a
//! short-lived `busctl` running as the account with no supplementary groups
//! and an empty environment: it can learn nothing the account could not.
//! What the bus says is the account's own word; whatever it names is
//! checked against the kernel (`/proc`) before the shared proof trusts it.
//! The `busctl` child dies with the Pier (`PR_SET_PDEATHSIG`), but runs as
//! the account in the Pier's service cgroup and is not protected from the
//! account's own debugger for its few milliseconds: it holds nothing the
//! account does not. A proven compositor is the account's process, never
//! more trusted than the account (S4b acts on it only as the account).

use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};
use std::path::Path;
use std::time::Duration;

use arcen_session::console_session::wayland_join::{
    self as model, CompositorEvidence, NameFact, NameOwner, PathFact, PathKind, ProcessFact,
    WaylandJoin, WaylandJoinAdapter, WaylandTarget, MUTTER_NAMES,
};
use tokio::process::Command;

use crate::logging::target;

const BUSCTL: &str = "/usr/bin/busctl";
const LOGINCTL: &str = "/usr/bin/loginctl";
/// What one read of the bus or logind may take.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(3);
/// What `busctl` itself waits for a reply.
const BUS_REPLY_TIMEOUT: &str = "--timeout=2";
/// A reply of these calls is a few hundred bytes; more is not an answer.
const MAX_REPLY_BYTES: usize = 64 * 1024;
/// `GetCurrentState` lists every mode of every monitor.
const MAX_DISPLAY_STATE_BYTES: usize = 2 * 1024 * 1024;
const MAX_STATUS_BYTES: u64 = 64 * 1024;
const MAX_ENVIRON_BYTES: u64 = 256 * 1024;

/// A Pier built with the Wayland adapter joins a proven Wayland console
/// session (S4b, `session/wayland/join.rs`); one without it refuses
/// visibly.
pub(crate) const ADAPTER: WaylandJoinAdapter = if super::wayland::WAYLAND_ADAPTER_BUILT {
    WaylandJoinAdapter::Available
} else {
    WaylandJoinAdapter::NotYetAvailable
};

/// Identifies `target`, the account's own Wayland session, proves its
/// compositor read-only and decides what the sign-in does, logging the
/// evidence. `gid` is the account's primary group, for the bus queries.
pub(crate) async fn identify(target: &WaylandTarget, gid: u32) -> WaylandJoin {
    let evidence = gather(target, gid).await;
    let proof = model::prove_compositor(target, &evidence);
    let join = model::decide_join(target, proof, ADAPTER);
    let compositor = match proof {
        model::CompositorProof::Verified { pid } => pid,
        _ => 0,
    };
    tracing::info!(
        target: target::SESSION,
        session_id = %target.session,
        uid = target.uid,
        leader = target.leader,
        origin = target.origin.token(),
        desktop = ?target.desktop,
        locked = target.locked,
        activate = target.activate,
        runtime_dir_is_logind = evidence.runtime_dir.as_deref() == Some(target.runtime_dir().as_str()),
        bus_peer_uid = ?evidence.bus_peer_uid,
        compositor_pid = compositor,
        proof = proof.token(),
        outcome = join.token(),
        "the account's Wayland console session is identified"
    );
    join
}

/// Reads everything [`model::prove_compositor`] needs about `target`, in
/// the order that lets a change show: the session is read again last.
pub(crate) async fn gather(target: &WaylandTarget, gid: u32) -> CompositorEvidence {
    let mut evidence = CompositorEvidence {
        runtime_dir: super::console::session_environ(
            &target.session,
            target.uid,
            "XDG_RUNTIME_DIR",
        ),
        runtime_dir_fact: path_fact(Path::new(&target.runtime_dir())),
        bus_socket: path_fact(Path::new(&target.bus_socket())),
        bus_peer_uid: bus_peer_uid(Path::new(&target.bus_socket()), target.uid).await,
        ..CompositorEvidence::default()
    };
    // The bus is asked only when the kernel says the account serves it.
    if evidence.bus_peer_uid == Some(target.uid) {
        let bus = UserBus::new(&target.bus_socket(), target.uid, gid);
        evidence.names = bus.mutter_names().await;
        evidence.compositor = single_pid(&evidence.names).and_then(process_fact);
        evidence.names_after = bus.mutter_names().await;
    }
    evidence.user_display = user_display(target.uid).await;
    evidence.session_after = super::console::session_fact(&target.session).await;
    evidence
}

/// `path` without following a link.
fn path_fact(path: &Path) -> Option<PathFact> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    let file_type = metadata.file_type();
    let kind = if file_type.is_dir() {
        PathKind::Directory
    } else if file_type.is_socket() {
        PathKind::Socket
    } else {
        PathKind::Other
    };
    Some(PathFact {
        kind,
        owner: metadata.uid(),
        mode: metadata.mode() & 0o7777,
    })
}

/// The uid of the process serving `socket`, from the kernel (`SO_PEERCRED`),
/// checked against the account `uid`. Nothing is written to the socket.
///
/// The account owns the directory and could swap the socket for a link
/// between a check and the connect, so the socket is opened without
/// following links (`O_PATH | O_NOFOLLOW`), must be a socket, and root
/// connects through that very file (`/proc/self/fd/<n>`): never to
/// whatever a link names.
async fn bus_peer_uid(socket: &Path, uid: u32) -> Option<u32> {
    use super::wayland::socket_proof::{verify_peer, SocketProofError};
    use std::os::fd::AsRawFd as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_PATH | nix::libc::O_NOFOLLOW)
        .open(socket)
        .ok()?;
    if !file.metadata().ok()?.file_type().is_socket() {
        return None;
    }
    let pinned = format!("/proc/self/fd/{}", file.as_raw_fd());
    let stream = tokio::time::timeout(COMMAND_TIMEOUT, tokio::net::UnixStream::connect(pinned))
        .await
        .ok()?
        .ok()?;
    drop(file);
    match verify_peer(&stream, uid, None) {
        Ok(()) => Some(uid),
        Err(SocketProofError::ForeignPeer { uid, .. }) => uid,
        Err(_) => None,
    }
}

/// The account's own bus, asked through `busctl` running as the account.
struct UserBus {
    address: String,
    uid: u32,
    gid: u32,
}

impl UserBus {
    fn new(socket: &str, uid: u32, gid: u32) -> Self {
        Self {
            address: format!("--address=unix:path={socket}"),
            uid,
            gid,
        }
    }

    /// Calls a method of the bus itself; its JSON reply, or `None`.
    async fn call(&self, method: &str, argument: &str) -> Option<serde_json::Value> {
        self.call_on(
            [
                "org.freedesktop.DBus",
                "/org/freedesktop/DBus",
                "org.freedesktop.DBus",
                method,
                "s",
                argument,
            ],
            MAX_REPLY_BYTES,
        )
        .await
    }

    /// Mutter's `DisplayConfig.GetCurrentState` (read-only); its JSON
    /// reply, or `None`.
    async fn display_state(&self) -> Option<serde_json::Value> {
        self.call_on(
            [
                model::MUTTER_DISPLAY_CONFIG,
                "/org/gnome/Mutter/DisplayConfig",
                model::MUTTER_DISPLAY_CONFIG,
                "GetCurrentState",
            ],
            MAX_DISPLAY_STATE_BYTES,
        )
        .await
    }

    async fn call_on<const N: usize>(
        &self,
        call: [&str; N],
        max_bytes: usize,
    ) -> Option<serde_json::Value> {
        let mut command = Command::new(BUSCTL);
        command
            .args([
                self.address.as_str(),
                "--json=short",
                "--no-pager",
                BUS_REPLY_TIMEOUT,
                "call",
            ])
            .args(call)
            .env_clear()
            .current_dir("/")
            // As root, setting the uid also drops every supplementary group.
            .uid(self.uid)
            .gid(self.gid)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        // SAFETY: runs after fork, after the credential change, before exec;
        // only prctl/getppid.
        unsafe {
            command.pre_exec(super::identity::arm_parent_death_signal);
        }
        let output = tokio::time::timeout(COMMAND_TIMEOUT, command.output())
            .await
            .ok()?
            .ok()?;
        if !output.status.success() || output.stdout.len() > max_bytes {
            return None;
        }
        serde_json::from_slice(&output.stdout).ok()
    }

    /// Each of [`MUTTER_NAMES`] and its owner.
    async fn mutter_names(&self) -> Vec<(String, NameFact)> {
        let mut facts = Vec::with_capacity(MUTTER_NAMES.len());
        for name in MUTTER_NAMES {
            facts.push((name.to_owned(), self.name_fact(name).await));
        }
        facts
    }

    async fn name_fact(&self, name: &str) -> NameFact {
        match self
            .call("NameHasOwner", name)
            .await
            .as_ref()
            .and_then(reply_bool)
        {
            None => return NameFact::Unreadable,
            Some(false) => return NameFact::Unowned,
            Some(true) => {}
        }
        let Some(unique) = self
            .call("GetNameOwner", name)
            .await
            .as_ref()
            .and_then(reply_string)
        else {
            return NameFact::Unreadable;
        };
        if !unique.starts_with(':') {
            return NameFact::Unreadable;
        }
        let credentials = self.call("GetConnectionCredentials", &unique).await;
        let (uid, pid) = credentials.as_ref().map_or((None, None), reply_credentials);
        NameFact::Owned(NameOwner { unique, uid, pid })
    }
}

/// Whether `target`'s session shows on a monitor at the desk, read from its
/// compositor (`DisplayConfig.GetCurrentState`, as the account) before
/// anything of the session is brought forward or unlocked; `None` when it
/// could not be read.
pub(crate) async fn desk_shows_session(target: &WaylandTarget, gid: u32) -> Option<bool> {
    let bus = UserBus::new(&target.bus_socket(), target.uid, gid);
    let state = bus.display_state().await?;
    let monitors = display_state_monitors(&state)?;
    Some(arcen_session::console_session::wayland_attach::monitors_at_desk(&monitors))
}

/// The monitors of a `GetCurrentState` reply (`busctl --json=short`):
/// each connector, whether the compositor made it, and whether a logical
/// monitor places it.
fn display_state_monitors(
    reply: &serde_json::Value,
) -> Option<Vec<arcen_session::console_session::wayland_attach::JoinMonitor>> {
    use arcen_session::console_session::wayland_attach::{is_virtual_product, JoinMonitor};
    let data = reply.get("data")?.as_array()?;
    let monitors = data.get(1)?.as_array()?;
    let logical = data.get(2)?.as_array()?;
    let placed = |connector: &str| {
        logical.iter().find_map(|monitor| {
            let entry = monitor.as_array()?;
            let x = i32::try_from(entry.first()?.as_i64()?).ok()?;
            let y = i32::try_from(entry.get(1)?.as_i64()?).ok()?;
            entry
                .get(5)?
                .as_array()?
                .iter()
                .any(|spec| spec.get(0).and_then(serde_json::Value::as_str) == Some(connector))
                .then_some((x, y))
        })
    };
    monitors
        .iter()
        .map(|monitor| {
            let spec = monitor.as_array()?.first()?.as_array()?;
            let connector = spec.first()?.as_str()?;
            let product = spec.get(2)?.as_str()?;
            Some(JoinMonitor {
                connector: connector.to_owned(),
                virtual_monitor: is_virtual_product(product),
                mode: None,
                placement: placed(connector),
                scale: 1.0,
            })
        })
        .collect()
}

/// `{"type":"b","data":[true]}`.
fn reply_bool(reply: &serde_json::Value) -> Option<bool> {
    (reply.get("type")?.as_str()? == "b")
        .then(|| reply.get("data")?.get(0)?.as_bool())
        .flatten()
}

/// `{"type":"s","data":[":1.42"]}`.
fn reply_string(reply: &serde_json::Value) -> Option<String> {
    (reply.get("type")?.as_str()? == "s")
        .then(|| Some(reply.get("data")?.get(0)?.as_str()?.to_owned()))
        .flatten()
}

/// `UnixUserID` and `ProcessID` of a `GetConnectionCredentials` reply.
fn reply_credentials(reply: &serde_json::Value) -> (Option<u32>, Option<u32>) {
    let Some(map) = (reply.get("type").and_then(serde_json::Value::as_str) == Some("a{sv}"))
        .then(|| reply.get("data")?.get(0))
        .flatten()
    else {
        return (None, None);
    };
    let field = |key: &str| -> Option<u32> {
        let value = map.get(key)?;
        (value.get("type")?.as_str()? == "u")
            .then(|| u32::try_from(value.get("data")?.as_u64()?).ok())
            .flatten()
    };
    (field("UnixUserID"), field("ProcessID"))
}

/// The one process the names' owner reports, when there is one.
fn single_pid(names: &[(String, NameFact)]) -> Option<u32> {
    let mut pids = names.iter().map(|(_, fact)| match fact {
        NameFact::Owned(owner) => owner.pid,
        NameFact::Unowned | NameFact::Unreadable => None,
    });
    let first = pids.next()??;
    pids.all(|pid| pid == Some(first)).then_some(first)
}

/// The process `pid` as the kernel reports it. Every file is read through
/// one handle on its `/proc` directory: a process id reused while it is
/// read gives errors, never another process's facts.
fn process_fact(pid: u32) -> Option<ProcessFact> {
    use std::io::Read as _;
    use std::os::fd::AsRawFd as _;
    let directory = std::fs::File::open(format!("/proc/{pid}")).ok()?;
    let read = |name: &str, limit: u64| -> Option<Vec<u8>> {
        let mut bytes = Vec::new();
        std::fs::File::open(format!("/proc/self/fd/{}/{name}", directory.as_raw_fd()))
            .ok()?
            .take(limit)
            .read_to_end(&mut bytes)
            .ok()?;
        Some(bytes)
    };
    let status = String::from_utf8(read("status", MAX_STATUS_BYTES)?).ok()?;
    let cgroup = String::from_utf8(read("cgroup", MAX_STATUS_BYTES)?).ok()?;
    let environ = read("environ", MAX_ENVIRON_BYTES).unwrap_or_default();
    Some(ProcessFact {
        pid,
        uid: process_uid(&status)?,
        cgroup: unified_cgroup(&cgroup)?,
        session_id: environ_entry(&environ, "XDG_SESSION_ID"),
    })
}

/// The uid of a `/proc/<pid>/status` whose real, effective, saved and file
/// system uids are all that one.
fn process_uid(status: &str) -> Option<u32> {
    let uids: Vec<u32> = status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))?
        .split_whitespace()
        .map(str::parse)
        .collect::<Result<_, _>>()
        .ok()?;
    match uids.as_slice() {
        [real, rest @ ..] if rest.len() == 3 && rest.iter().all(|uid| uid == real) => Some(*real),
        _ => None,
    }
}

/// `key` in a NUL-separated environment.
fn environ_entry(environ: &[u8], key: &str) -> Option<String> {
    let prefix = format!("{key}=");
    environ
        .split(|byte| *byte == 0)
        .filter_map(|entry| std::str::from_utf8(entry).ok())
        .find_map(|entry| entry.strip_prefix(&prefix).map(str::to_owned))
}

/// The unified (cgroup v2) path of a `/proc/<pid>/cgroup`.
fn unified_cgroup(cgroup: &str) -> Option<String> {
    cgroup
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .map(str::to_owned)
}

/// The session logind names as the account's display session
/// (`loginctl show-user <uid> -p Display`), the one Mutter takes when it is
/// not told which.
async fn user_display(uid: u32) -> Option<String> {
    let mut show = Command::new(LOGINCTL);
    show.args(["show-user", &uid.to_string(), "-p", "Display", "--value"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(COMMAND_TIMEOUT, show.output())
        .await
        .ok()?
        .ok()?;
    if !output.status.success() {
        return None;
    }
    display_value(&String::from_utf8_lossy(&output.stdout))
}

fn display_value(text: &str) -> Option<String> {
    let value = text.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

/// What a host whose Pier creates its own Wayland desktops does with a
/// sign-in before it creates one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BeforeCreatingOutcome {
    /// logind proved the account holds no session at the workstation.
    Create,
    /// The account's own Wayland session is proven: join it as a console
    /// session (S4b).
    JoinConsole,
    /// Refused, with the notice the Deck is shown.
    Refuse(arcen_session::console_session::ConsoleNotice),
}

/// The step before a desktop is created (shared
/// `wayland_join::creating_step`): the account's own Wayland session is
/// identified and proven read-only first.
pub(crate) async fn before_creating(uid: u32, gid: u32) -> BeforeCreatingOutcome {
    let sessions = super::console::read_sessions().await;
    before_creating_from(&sessions, uid, gid).await
}

async fn before_creating_from(
    sessions: &arcen_session::console_session::SessionRead,
    uid: u32,
    gid: u32,
) -> BeforeCreatingOutcome {
    use model::{BeforeCreating, CreatingStep};
    let decision = model::decide_before_creating_read(sessions, uid);
    let join = match &decision {
        BeforeCreating::JoinWayland(target) => Some(identify(target, gid).await),
        BeforeCreating::Create | BeforeCreating::Refuse(_) => None,
    };
    let session_id = match &decision {
        BeforeCreating::JoinWayland(target) => target.session.as_str(),
        BeforeCreating::Create | BeforeCreating::Refuse(_) => "",
    };
    match model::creating_step(&decision, join.as_ref()) {
        CreatingStep::Create => BeforeCreatingOutcome::Create,
        CreatingStep::JoinConsole => {
            tracing::info!(
                target: target::SESSION,
                uid,
                session_id,
                "the account holds a proven Wayland session at the workstation: it is joined, no private desktop is created"
            );
            BeforeCreatingOutcome::JoinConsole
        }
        CreatingStep::Refuse(refusal) => {
            tracing::warn!(
                target: target::SESSION,
                uid,
                reason_class = refusal.token(),
                session_id,
                logind = sessions.as_ref().err().map_or("read", |unreadable| unreadable.token()),
                "no private desktop is created: the account holds a session at the workstation, or logind could not say whether it does"
            );
            BeforeCreatingOutcome::Refuse(super::console::notice_for(refusal, sessions, uid))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json(text: &str) -> serde_json::Value {
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn busctl_replies_parse_as_the_bus_sends_them() {
        // Captured from busctl (systemd 257) on Enterprise Linux 10.
        assert_eq!(
            reply_bool(&json(r#"{"type":"b","data":[true]}"#)),
            Some(true)
        );
        assert_eq!(
            reply_bool(&json(r#"{"type":"b","data":[false]}"#)),
            Some(false)
        );
        assert_eq!(reply_bool(&json(r#"{"type":"s","data":["true"]}"#)), None);
        assert_eq!(
            reply_string(&json(r#"{"type":"s","data":[":1.1"]}"#)).as_deref(),
            Some(":1.1")
        );
        assert_eq!(
            reply_string(&json(r#"{"type":"as","data":[[":1.1"]]}"#)),
            None
        );
        let credentials = json(
            r#"{"type":"a{sv}","data":[{"UnixUserID":{"type":"u","data":1001},"ProcessID":{"type":"u","data":979757},"UnixGroupIDs":{"type":"au","data":[1001]},"ProcessFD":{"type":"h","data":{"fd":5,"path":"anon_inode:[pidfd]"}}}]}"#,
        );
        assert_eq!(reply_credentials(&credentials), (Some(1001), Some(979_757)));
        // A bus that leaves a field out, or types it otherwise, proves nothing.
        assert_eq!(
            reply_credentials(&json(
                r#"{"type":"a{sv}","data":[{"UnixUserID":{"type":"s","data":"1001"}}]}"#
            )),
            (None, None)
        );
        assert_eq!(
            reply_credentials(&json(
                r#"{"type":"a{sv}","data":[{"ProcessID":{"type":"u","data":4294967296}}]}"#
            )),
            (None, None)
        );
    }

    #[test]
    fn proc_status_and_cgroup_give_the_real_uid_and_unified_path() {
        let status =
            "Name:\tgnome-shell\nUid:\t1001\t1001\t1001\t1001\nGid:\t1001\t1001\t1001\t1001\n";
        assert_eq!(process_uid(status), Some(1001));
        // A process whose effective, saved or file system uid differs (a
        // set-uid program, or one changing identity) is not the account's.
        assert_eq!(process_uid("Uid:\t1001\t0\t1001\t1001\n"), None);
        assert_eq!(process_uid("Uid:\t1001\t1001\t0\t1001\n"), None);
        assert_eq!(process_uid("Uid:\t1001\t1001\t1001\n"), None);
        assert_eq!(process_uid("Name:\tx\n"), None);
        assert_eq!(
            environ_entry(
                b"A=1\0XDG_SESSION_ID=7\0XDG_SESSION_ID_X=8\0",
                "XDG_SESSION_ID"
            )
            .as_deref(),
            Some("7")
        );
        assert_eq!(environ_entry(b"A=1\0", "XDG_SESSION_ID"), None);
        let v2 = "0::/user.slice/user-1001.slice/user@1001.service/session.slice/org.gnome.Shell@wayland.service\n";
        assert_eq!(
            unified_cgroup(v2).as_deref(),
            Some("/user.slice/user-1001.slice/user@1001.service/session.slice/org.gnome.Shell@wayland.service")
        );
        let hybrid = "12:pids:/user.slice\n1:name=systemd:/user.slice/user-1001.slice/session-2.scope\n0::/user.slice/user-1001.slice/session-2.scope\n";
        assert_eq!(
            unified_cgroup(hybrid).as_deref(),
            Some("/user.slice/user-1001.slice/session-2.scope")
        );
        assert_eq!(unified_cgroup("1:name=systemd:/x\n"), None);
    }

    #[test]
    fn one_owner_process_is_found_only_when_every_name_agrees() {
        let owned = |pid: Option<u32>| {
            NameFact::Owned(NameOwner {
                unique: ":1.7".into(),
                uid: Some(1001),
                pid,
            })
        };
        let names = |facts: [NameFact; 3]| -> Vec<(String, NameFact)> {
            MUTTER_NAMES
                .iter()
                .zip(facts)
                .map(|(name, fact)| ((*name).to_owned(), fact))
                .collect()
        };
        assert_eq!(
            single_pid(&names([owned(Some(9)), owned(Some(9)), owned(Some(9))])),
            Some(9)
        );
        assert_eq!(
            single_pid(&names([owned(Some(9)), owned(Some(8)), owned(Some(9))])),
            None
        );
        assert_eq!(
            single_pid(&names([owned(Some(9)), NameFact::Unowned, owned(Some(9))])),
            None
        );
        assert_eq!(
            single_pid(&names([owned(None), owned(None), owned(None)])),
            None
        );
        assert_eq!(single_pid(&[]), None);
    }

    #[test]
    fn an_empty_display_is_none() {
        assert_eq!(display_value("7\n").as_deref(), Some("7"));
        assert_eq!(display_value("\n"), None);
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = std::env::current_exe().unwrap().with_file_name(format!(
            "arcen-console-wayland-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }

    #[tokio::test]
    async fn path_facts_never_follow_a_link_and_the_kernel_names_the_peer() {
        let dir = scratch("paths");
        let socket = dir.join("bus");
        let _listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let uid = nix::unistd::Uid::effective().as_raw();
        let directory = path_fact(&dir).unwrap();
        assert_eq!(
            (directory.kind, directory.owner, directory.mode),
            (PathKind::Directory, uid, 0o700)
        );
        assert_eq!(path_fact(&socket).unwrap().kind, PathKind::Socket);
        // A link to the socket is a link, not a socket, and root never
        // connects through it.
        let link = dir.join("link");
        std::os::unix::fs::symlink(&socket, &link).unwrap();
        assert_eq!(path_fact(&link).unwrap().kind, PathKind::Other);
        assert_eq!(bus_peer_uid(&link, uid).await, None);
        assert_eq!(bus_peer_uid(&dir, uid).await, None);
        assert_eq!(path_fact(&dir.join("missing")), None);
        assert_eq!(bus_peer_uid(&socket, uid).await, Some(uid));
        // Expecting another account, the kernel's answer is still this one.
        assert_eq!(bus_peer_uid(&socket, uid.wrapping_add(1)).await, Some(uid));
        assert_eq!(bus_peer_uid(&dir.join("missing"), uid).await, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn this_process_is_read_from_proc() {
        let fact = process_fact(std::process::id()).unwrap();
        assert_eq!(fact.uid, nix::unistd::Uid::current().as_raw());
        assert!(fact.cgroup.starts_with('/'), "{}", fact.cgroup);
        assert_eq!(process_fact(u32::MAX), None);
    }

    /// Lab proof (root, `dbus-daemon` and python3-gi): a user's bus served
    /// by `nobody`, with the three Mutter names owned by one connection, is
    /// read as that account and every fact comes back. Run with
    /// `--ignored live_user_bus`.
    #[tokio::test]
    #[ignore = "lab: needs root, dbus-daemon and python3-gi"]
    async fn live_user_bus_is_read_as_its_account() {
        use std::os::unix::fs::PermissionsExt as _;
        assert!(nix::unistd::Uid::effective().is_root(), "run as root");
        let nobody = nix::unistd::User::from_name("nobody").unwrap().unwrap();
        let (uid, gid) = (nobody.uid.as_raw(), nobody.gid.as_raw());
        let dir = std::path::PathBuf::from(format!("/run/arcen-s4a-live-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        nix::unistd::chown(&dir, Some(nobody.uid), Some(nobody.gid)).unwrap();
        let socket = dir.join("bus");
        let address = format!("unix:path={}", socket.display());
        let mut daemon = Command::new("dbus-daemon")
            .args([
                "--session",
                "--nofork",
                "--nopidfile",
                &format!("--address={address}"),
            ])
            .env_clear()
            .current_dir("/")
            .uid(uid)
            .gid(gid)
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        for _ in 0..50 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let script = format!(
            "from gi.repository import Gio, GLib\n\
             c = Gio.DBusConnection.new_for_address_sync('{address}', \
             Gio.DBusConnectionFlags.AUTHENTICATION_CLIENT | Gio.DBusConnectionFlags.MESSAGE_BUS_CONNECTION, None, None)\n\
             for n in {names:?}:\n    c.call_sync('org.freedesktop.DBus', '/org/freedesktop/DBus', 'org.freedesktop.DBus', 'RequestName', GLib.Variant('(su)', (n, 4)), None, 0, -1, None)\n\
             GLib.MainLoop().run()\n",
            names = MUTTER_NAMES
        );
        let mut owner = Command::new("python3")
            .args(["-c", &script])
            .env_clear()
            .current_dir("/")
            .uid(uid)
            .gid(gid)
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let owner_pid = owner.id().unwrap();
        let bus = UserBus::new(&socket.display().to_string(), uid, gid);
        let mut names = Vec::new();
        for _ in 0..50 {
            names = bus.mutter_names().await;
            if names
                .iter()
                .all(|(_, fact)| matches!(fact, NameFact::Owned(_)))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let peer = bus_peer_uid(&socket, uid).await;
        let socket_fact = path_fact(&socket);
        let process = single_pid(&names).and_then(process_fact);
        println!("live: peer={peer:?} socket={socket_fact:?} names={names:?} process={process:?}");
        let _ = owner.kill().await;
        let _ = daemon.kill().await;
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(peer, Some(uid));
        assert_eq!(
            socket_fact.map(|fact| (fact.kind, fact.owner)),
            Some((PathKind::Socket, uid))
        );
        assert_eq!(single_pid(&names), Some(owner_pid), "{names:?}");
        for (_, fact) in &names {
            assert!(
                matches!(fact, NameFact::Owned(owner) if owner.uid == Some(uid)),
                "{fact:?}"
            );
        }
        assert_eq!(process.map(|process| process.uid), Some(uid));
    }

    /// Lab evidence: what this adapter enumerates on the host it runs on.
    /// Read-only. Run with `--ignored --nocapture live_enumeration`.
    #[tokio::test]
    #[ignore = "lab: reads the host's logind sessions"]
    async fn live_enumeration_of_this_host() {
        use arcen_session::console_session::{decide_on, wayland_join::decide_before_creating};
        let sessions = super::super::console::read_sessions()
            .await
            .expect("logind's sessions");
        for session in &sessions {
            println!("live: session {session:?}");
        }
        let uids: std::collections::BTreeSet<u32> = sessions.iter().map(|s| s.uid).collect();
        for uid in uids {
            let decision = decide_on(&sessions, uid, super::super::login_session::console_host());
            println!(
                "live: uid {uid}: console {decision:?}; before creating {:?}; display {:?}",
                decide_before_creating(&sessions, uid),
                user_display(uid).await
            );
            if let arcen_session::console_session::ConsoleDecision::JoinWayland(target) = decision {
                let gid = nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid))
                    .ok()
                    .flatten()
                    .map_or(uid, |user| user.gid.as_raw());
                println!("live: uid {uid}: {:?}", identify(&target, gid).await);
            }
        }
    }

    fn console_desktop(
        id: &str,
        uid: u32,
        kind: &str,
    ) -> arcen_session::console_session::ConsoleSessionFact {
        arcen_session::console_session::ConsoleSessionFact {
            id: id.to_owned(),
            uid,
            user: format!("u{uid}"),
            seat: "seat0".to_owned(),
            vtnr: 2,
            tty: "tty2".to_owned(),
            class: "user".to_owned(),
            session_type: kind.to_owned(),
            state: "active".to_owned(),
            active: true,
            service: "gdm-password".to_owned(),
            ..arcen_session::console_session::ConsoleSessionFact::default()
        }
    }

    #[tokio::test]
    async fn a_desktop_is_created_only_when_logind_proved_the_account_holds_none() {
        use arcen_session::console_session::{ConsoleRefusal, SessionsUnreadable};
        const ACCOUNT: u32 = 1001;
        let refusal = |read| async move {
            match before_creating_from(&read, ACCOUNT, ACCOUNT).await {
                BeforeCreatingOutcome::Refuse(notice) => Some(notice.token()),
                BeforeCreatingOutcome::Create | BeforeCreatingOutcome::JoinConsole => None,
            }
        };
        // logind unreadable: never a desktop.
        assert_eq!(
            refusal(Err(SessionsUnreadable::ListFailed)).await,
            Some(ConsoleRefusal::SessionsUnreadable.token())
        );
        // The account's own X11 session at the workstation.
        assert_eq!(
            refusal(Ok(vec![console_desktop("7", ACCOUNT, "x11")])).await,
            Some(ConsoleRefusal::SessionUnreachable.token())
        );
        // Proven absence: nothing of the account's at the workstation.
        let mut arcen_desktop = console_desktop("11", ACCOUNT, "wayland");
        arcen_desktop.seat = String::new();
        arcen_desktop.tty = arcen_session::console_session::ARCEN_TTY.to_owned();
        assert_eq!(refusal(Ok(Vec::new())).await, None);
        assert_eq!(
            refusal(Ok(vec![console_desktop("7", 1002, "x11"), arcen_desktop])).await,
            None
        );
    }

    #[test]
    fn the_desk_is_read_from_the_compositors_display_state() {
        // Captured shape of `busctl --json=short ... GetCurrentState`
        // (Mutter 49.4), shortened: one headless virtual monitor.
        let headless = json(
            r#"{"type":"ua((ssss)a(siiddada{sv})a{sv})a(iiduba(ssss)a{sv})a{sv}","data":[2,[[["Meta-0","MetaVendor","MetaVirtualMonitor","0x00"],[["1920x1080@60.000",1920,1080,60.0,1.0,[1.0,2.0],{}]],{}]],[[0,0,1.0,0,true,[["Meta-0","MetaVendor","MetaVirtualMonitor","0x00"]],{}]],{}]}"#,
        );
        let monitors = display_state_monitors(&headless).expect("monitors");
        assert_eq!(monitors.len(), 1);
        assert!(monitors[0].virtual_monitor);
        assert_eq!(monitors[0].placement, Some((0, 0)));
        assert!(!arcen_session::console_session::wayland_attach::monitors_at_desk(&monitors));
        // A physical monitor in a logical monitor shows at the desk; one
        // that is disabled does not.
        let desk = json(
            r#"{"type":"x","data":[3,[[["DP-1","DEL","DELL U2720Q","123"],[],{}],[["HDMI-1","GSM","LG","9"],[],{}]],[[0,0,1.0,0,true,[["DP-1","DEL","DELL U2720Q","123"]],{}]],{}]}"#,
        );
        let monitors = display_state_monitors(&desk).expect("monitors");
        assert_eq!(monitors[1].placement, None);
        assert!(arcen_session::console_session::wayland_attach::monitors_at_desk(&monitors));
        assert_eq!(
            display_state_monitors(&json(r#"{"type":"x","data":[1]}"#)),
            None
        );
    }
}
