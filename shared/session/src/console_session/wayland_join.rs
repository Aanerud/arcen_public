// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Joining the user's own GNOME/Mutter Wayland session (ADR 0025 §9,
//! GAP-042).
//!
//! A sign-in whose account holds a Wayland session at the workstation is
//! the same case as one holding an X11 session: Arcen joins that session and
//! never builds a second desktop beside it. This module holds the decisions
//! of that case that do not need the operating system:
//!
//! - [`WaylandTarget`]: the account's own Wayland console session the
//!   console decision chose (`ConsoleDecision::JoinWayland`), or the one a
//!   host that creates its own Wayland desktops found before creating one
//!   ([`decide_before_creating`]);
//! - [`CompositorEvidence`] and [`prove_compositor`]: what the Pier read,
//!   read-only, about that session's runtime directory, its user bus and the
//!   `org.gnome.Mutter.*` names on it, and whether that proves the
//!   compositor is this session's, served by this account;
//! - [`decide_join`]: join, take the lock screen first, or refuse — and,
//!   while a Pier has no Wayland console adapter
//!   ([`WaylandJoinAdapter::NotYetAvailable`]), the typed
//!   [`WaylandJoin::AdapterUnavailable`] outcome that keeps the visible
//!   `console_wayland_session` refusal and carries the evidence.
//!
//! The Pier's adapter reads logind, the session's processes, the bus
//! socket's owner and peer credentials, and the bus's own report of each
//! name's owner; it starts no bus and no compositor, and it acts on nothing.
//! One Deck is attached to a workstation at a time: a target is the one
//! session a sign-in may join, never a second attachment.

use super::{ConsoleOrigin, ConsoleRefusal, ConsoleSessionFact};

/// Mutter's display configuration: the physical monitors of the session.
pub const MUTTER_DISPLAY_CONFIG: &str = "org.gnome.Mutter.DisplayConfig";
/// Mutter's screen cast: `RecordMonitor` of the session's own monitors.
pub const MUTTER_SCREEN_CAST: &str = "org.gnome.Mutter.ScreenCast";
/// Mutter's remote desktop: input into the session.
pub const MUTTER_REMOTE_DESKTOP: &str = "org.gnome.Mutter.RemoteDesktop";
/// Every name a joined session's compositor must own, one connection for
/// all of them.
pub const MUTTER_NAMES: [&str; 3] = [
    MUTTER_DISPLAY_CONFIG,
    MUTTER_SCREEN_CAST,
    MUTTER_REMOTE_DESKTOP,
];

/// The user bus socket's name in the user's runtime directory.
pub const USER_BUS_SOCKET: &str = "bus";

/// The account's own Wayland session at the workstation, as the console
/// decision found it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaylandTarget {
    /// logind's session id.
    pub session: String,
    /// The session's owner: the authenticated account.
    pub uid: u32,
    /// The process that opened the session (logind `Leader=`): a session
    /// id logind reused for a later session has another leader.
    pub leader: u32,
    /// It waits behind the login screen or another session (logind would
    /// bring it forward before a join).
    pub activate: bool,
    /// Who started it.
    pub origin: ConsoleOrigin,
    /// Its screen is locked (logind `LockedHint`).
    pub locked: bool,
    /// logind's `Desktop=` (evidence only; GDM may leave it empty).
    pub desktop: String,
}

impl WaylandTarget {
    /// The target for `fact`, the account's own Wayland console session.
    #[must_use]
    pub fn of(fact: &ConsoleSessionFact, activate: bool) -> Self {
        Self {
            session: fact.id.clone(),
            uid: fact.uid,
            leader: fact.leader,
            activate,
            origin: fact.origin(),
            locked: fact.locked,
            desktop: fact.desktop.clone(),
        }
    }

    /// The runtime directory logind gives the session's user, where the
    /// user bus lives.
    #[must_use]
    pub fn runtime_dir(&self) -> String {
        format!("/run/user/{}", self.uid)
    }

    /// The user bus socket in [`Self::runtime_dir`].
    #[must_use]
    pub fn bus_socket(&self) -> String {
        format!("{}/{USER_BUS_SOCKET}", self.runtime_dir())
    }
}

/// What a host that creates its own Wayland desktops (no Xorg server: the
/// headless or native-seat provider) does with a sign-in before it creates
/// one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BeforeCreating {
    /// logind proved the account has no graphical session at the
    /// workstation: the Pier creates its own desktop.
    Create,
    /// The account's own Wayland session at the workstation is the session
    /// to join, never a second desktop beside it.
    JoinWayland(WaylandTarget),
    /// The account's own session at the workstation is one this host's Pier
    /// cannot serve (an X11 session on a host without an Xorg provider):
    /// refused, never a second desktop.
    Refuse(ConsoleRefusal),
}

/// Decides what a sign-in of `uid` does, on a host that creates its own
/// Wayland desktops, before it creates one: the account's own graphical
/// session on the console seat (in front first, then any) is the session
/// it holds. Another account's session, a login screen, an SSH or
/// background session and Arcen's own desktops (`TTY=arcen`, seatless or on
/// their own seat) are not the account's workstation session.
#[must_use]
pub fn decide_before_creating(sessions: &[ConsoleSessionFact], uid: u32) -> BeforeCreating {
    let Some(mine) = sessions
        .iter()
        .filter(|session| session.uid == uid && session.is_console_desktop())
        .min_by_key(|session| (!session.active, session.state != "online"))
    else {
        return BeforeCreating::Create;
    };
    if mine.is_wayland() {
        BeforeCreating::JoinWayland(WaylandTarget::of(mine, !mine.active))
    } else {
        BeforeCreating::Refuse(ConsoleRefusal::SessionUnreachable)
    }
}

/// [`decide_before_creating`] from a read of logind's sessions: a read
/// that is not complete refuses ([`ConsoleRefusal::SessionsUnreadable`]);
/// only a complete one proves the account has no session to join.
#[must_use]
pub fn decide_before_creating_read(read: &super::SessionRead, uid: u32) -> BeforeCreating {
    match read {
        Ok(sessions) => decide_before_creating(sessions, uid),
        Err(_) => BeforeCreating::Refuse(ConsoleRefusal::SessionsUnreadable),
    }
}

/// What a host that creates its own Wayland desktops does with a sign-in
/// before it creates one (ADR 0025: never build a private desktop for an
/// account that holds one).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreatingStep {
    /// logind proved the account holds no session at the workstation:
    /// create its desktop.
    Create,
    /// The account's own Wayland session is proven and this Pier can join
    /// it: the sign-in joins it as a console session instead (S4b).
    JoinConsole,
    /// Refused, never a desktop beside the account's own.
    Refuse(ConsoleRefusal),
}

/// The step before creating a desktop.
///
/// | Before creating | Outcome |
/// | --- | --- |
/// | logind's sessions read completely, none is the account's console desktop (nothing, a login screen, another account, SSH or background sessions, Arcen's own desktops, a session on another seat, a closing one) | [`CreatingStep::Create`] |
/// | logind unreadable | refused `console_sessions_unreadable` |
/// | the account's own Wayland console session, proven, the adapter available ([`WaylandJoin::Join`] or [`WaylandJoin::HandleLock`]) | [`CreatingStep::JoinConsole`] |
/// | the same, not proven, or without the adapter | `join`'s refusal (`console_session_unreachable`, `console_wayland_session`) |
/// | the account's own X11 console session | refused `console_session_unreachable` |
///
/// `join` is the answer of [`decide_join`] for a
/// [`BeforeCreating::JoinWayland`] target, `None` when none was asked (then
/// refused: never a desktop beside it).
#[must_use]
pub fn creating_step(decision: &BeforeCreating, join: Option<&WaylandJoin>) -> CreatingStep {
    match decision {
        BeforeCreating::Create => CreatingStep::Create,
        BeforeCreating::Refuse(refusal) => CreatingStep::Refuse(*refusal),
        BeforeCreating::JoinWayland(_) => match join.map(WaylandJoin::refusal) {
            Some(None) => CreatingStep::JoinConsole,
            Some(Some(refusal)) => CreatingStep::Refuse(refusal),
            None => CreatingStep::Refuse(WaylandJoin::REFUSAL_WITHOUT_ADAPTER),
        },
    }
}

/// A file system object, as the adapter read it without following links.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PathFact {
    pub kind: PathKind,
    pub owner: u32,
    /// The permission bits.
    pub mode: u32,
}

/// What kind of object a [`PathFact`] describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathKind {
    Directory,
    Socket,
    Other,
}

/// The connection owning a well-known name, as the bus reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameOwner {
    /// The owner's unique connection name (`:1.42`).
    pub unique: String,
    /// The owner's uid (`GetConnectionCredentials` `UnixUserID`).
    pub uid: Option<u32>,
    /// The owner's process (`ProcessID`).
    pub pid: Option<u32>,
}

/// One well-known name on the session's bus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NameFact {
    Owned(NameOwner),
    /// The bus answered: nobody owns it.
    Unowned,
    /// The bus could not be asked, or did not answer.
    Unreadable,
}

/// The compositor process, as the kernel reports it (`/proc`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessFact {
    pub pid: u32,
    /// Its real uid.
    pub uid: u32,
    /// Its unified cgroup (`/user.slice/user-1000.slice/…`).
    pub cgroup: String,
    /// `XDG_SESSION_ID` in its environment, the session Mutter takes.
    pub session_id: Option<String>,
}

/// What the Pier read, read-only, about a [`WaylandTarget`]'s runtime
/// directory, user bus and compositor. `None` is a fact that could not be
/// read.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CompositorEvidence {
    /// The session as logind describes it after everything below was read
    /// (a replaced or reused session id shows here).
    pub session_after: Option<ConsoleSessionFact>,
    /// `XDG_RUNTIME_DIR` as the session's own processes have it.
    pub runtime_dir: Option<String>,
    /// That directory.
    pub runtime_dir_fact: Option<PathFact>,
    /// The bus socket in it.
    pub bus_socket: Option<PathFact>,
    /// The uid of the process serving the bus socket, from the kernel
    /// (`SO_PEERCRED`), not from the bus.
    pub bus_peer_uid: Option<u32>,
    /// Each of [`MUTTER_NAMES`] and its owner.
    pub names: Vec<(String, NameFact)>,
    /// The process owning them.
    pub compositor: Option<ProcessFact>,
    /// The owners asked again after the process was read: a compositor
    /// that restarted, or a recycled process id, shows here.
    pub names_after: Vec<(String, NameFact)>,
    /// The session logind names as the user's display session (`Display=`
    /// of the user), the one Mutter takes without `XDG_SESSION_ID`.
    pub user_display: Option<String>,
}

/// What [`prove_compositor`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompositorProof {
    /// The session's compositor owns every Mutter name on the account's own
    /// bus: the process `pid`.
    Verified { pid: u32 },
    /// A fact could not be read; nothing is proven.
    Unreadable(EvidenceGap),
    /// A fact contradicts the session: never joined.
    Mismatch(CompositorMismatch),
}

impl CompositorProof {
    /// Stable token for logs.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Verified { .. } => "verified",
            Self::Unreadable(gap) => gap.token(),
            Self::Mismatch(mismatch) => mismatch.token(),
        }
    }
}

/// The fact [`CompositorProof::Unreadable`] is missing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvidenceGap {
    Session,
    RuntimeDir,
    BusSocket,
    BusPeer,
    BusNames,
    Compositor,
}

impl EvidenceGap {
    /// Stable token for logs.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Session => "unreadable_session",
            Self::RuntimeDir => "unreadable_runtime_dir",
            Self::BusSocket => "unreadable_bus_socket",
            Self::BusPeer => "unreadable_bus_peer",
            Self::BusNames => "unreadable_bus_names",
            Self::Compositor => "unreadable_compositor",
        }
    }
}

/// The contradiction [`CompositorProof::Mismatch`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompositorMismatch {
    /// logind now describes another session under this id (another owner
    /// or leader: a reused id), or it is no longer a Wayland console
    /// session.
    SessionReplaced,
    /// The session's `XDG_RUNTIME_DIR` is not logind's runtime directory of
    /// the account.
    RuntimeDirNotTheUsers,
    /// That directory is not a directory owned by the account alone.
    RuntimeDirNotPrivate,
    /// The bus socket is not a socket owned by the account.
    BusSocketNotTheUsers,
    /// The kernel says another account serves the bus socket.
    BusServedByAnotherUser,
    /// A Mutter name has no owner: not a GNOME/Mutter session, or its
    /// compositor is not running.
    NotMutter,
    /// The Mutter names are owned by different connections.
    NamesSplit,
    /// The names' owner is not the account (by the bus or the kernel).
    OwnerNotTheUser,
    /// The names' owner is not this session's compositor: outside the
    /// account's slice, or Mutter's session is another one.
    OwnerOutsideSession,
    /// The names changed owner while they were read (the compositor
    /// restarted, or its process id was reused).
    OwnerChanged,
}

impl CompositorMismatch {
    /// Stable token for logs.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::SessionReplaced => "session_replaced",
            Self::RuntimeDirNotTheUsers => "runtime_dir_not_the_users",
            Self::RuntimeDirNotPrivate => "runtime_dir_not_private",
            Self::BusSocketNotTheUsers => "bus_socket_not_the_users",
            Self::BusServedByAnotherUser => "bus_served_by_another_user",
            Self::NotMutter => "not_mutter",
            Self::NamesSplit => "names_split",
            Self::OwnerNotTheUser => "owner_not_the_user",
            Self::OwnerOutsideSession => "owner_outside_session",
            Self::OwnerChanged => "owner_changed",
        }
    }
}

/// Whether `evidence` proves that the compositor serving `target` is that
/// session's, run by its account, on its account's own bus.
///
/// Every fact must be read and agree: the session is still the same
/// (owner, leader, Wayland on the console seat); its `XDG_RUNTIME_DIR` is
/// logind's runtime directory of the account, a directory the account alone
/// owns, holding a socket the account owns and the kernel says the account
/// serves; one connection owns every Mutter name, the bus and the kernel
/// agree it is the account's process, in the account's slice, and that
/// Mutter's session is this one (its own scope, its `XDG_SESSION_ID`, or,
/// without one, the account's display session); and the names did not move
/// while they were read.
#[must_use]
pub fn prove_compositor(target: &WaylandTarget, evidence: &CompositorEvidence) -> CompositorProof {
    use CompositorMismatch as Mismatch;
    use CompositorProof::{Mismatch as Contradicts, Unreadable};
    let uid = target.uid;
    let Some(session) = &evidence.session_after else {
        return Unreadable(EvidenceGap::Session);
    };
    if session.id != target.session
        || session.uid != uid
        || session.leader != target.leader
        || !session.is_console_desktop()
        || !session.is_wayland()
    {
        return Contradicts(Mismatch::SessionReplaced);
    }
    let Some(runtime_dir) = &evidence.runtime_dir else {
        return Unreadable(EvidenceGap::RuntimeDir);
    };
    if *runtime_dir != target.runtime_dir() {
        return Contradicts(Mismatch::RuntimeDirNotTheUsers);
    }
    let Some(directory) = evidence.runtime_dir_fact else {
        return Unreadable(EvidenceGap::RuntimeDir);
    };
    if directory.kind != PathKind::Directory
        || directory.owner != uid
        || directory.mode & 0o077 != 0
    {
        return Contradicts(Mismatch::RuntimeDirNotPrivate);
    }
    let Some(socket) = evidence.bus_socket else {
        return Unreadable(EvidenceGap::BusSocket);
    };
    if socket.kind != PathKind::Socket || socket.owner != uid {
        return Contradicts(Mismatch::BusSocketNotTheUsers);
    }
    match evidence.bus_peer_uid {
        None => return Unreadable(EvidenceGap::BusPeer),
        Some(peer) if peer != uid => return Contradicts(Mismatch::BusServedByAnotherUser),
        Some(_) => {}
    }
    let owner = match single_owner(&evidence.names) {
        Ok(owner) => owner,
        Err(proof) => return proof,
    };
    let (Some(owner_uid), Some(pid)) = (owner.uid, owner.pid) else {
        return Unreadable(EvidenceGap::BusNames);
    };
    if owner_uid != uid {
        return Contradicts(Mismatch::OwnerNotTheUser);
    }
    let Some(process) = &evidence.compositor else {
        return Unreadable(EvidenceGap::Compositor);
    };
    if process.pid != pid {
        return Contradicts(Mismatch::OwnerChanged);
    }
    if process.uid != uid {
        return Contradicts(Mismatch::OwnerNotTheUser);
    }
    if !compositor_of_session(
        process,
        uid,
        &target.session,
        evidence.user_display.as_deref(),
    ) {
        return Contradicts(Mismatch::OwnerOutsideSession);
    }
    match single_owner(&evidence.names_after) {
        Ok(after) if *after == *owner => CompositorProof::Verified { pid },
        Ok(_) | Err(Contradicts(_)) => Contradicts(Mismatch::OwnerChanged),
        Err(proof) => proof,
    }
}

/// The one connection owning every Mutter name in `names`.
fn single_owner(names: &[(String, NameFact)]) -> Result<&NameOwner, CompositorProof> {
    let mut owner: Option<&NameOwner> = None;
    for name in MUTTER_NAMES {
        let fact = names
            .iter()
            .find_map(|(known, fact)| (known == name).then_some(fact));
        match fact {
            None | Some(NameFact::Unreadable) => {
                return Err(CompositorProof::Unreadable(EvidenceGap::BusNames));
            }
            Some(NameFact::Unowned) => {
                return Err(CompositorProof::Mismatch(CompositorMismatch::NotMutter));
            }
            Some(NameFact::Owned(this)) => match owner {
                Some(first) if first != this => {
                    return Err(CompositorProof::Mismatch(CompositorMismatch::NamesSplit));
                }
                Some(_) => {}
                None => owner = Some(this),
            },
        }
    }
    owner.ok_or(CompositorProof::Unreadable(EvidenceGap::BusNames))
}

/// Whether `process` is the compositor of the account's session `session`:
/// in the account's own slice, and either in the session's own scope or
/// (GNOME under the user's systemd manager) with `XDG_SESSION_ID` naming
/// the session, or, without one, while the session is the account's display
/// session (the one Mutter takes then).
fn compositor_of_session(
    process: &ProcessFact,
    uid: u32,
    session: &str,
    user_display: Option<&str>,
) -> bool {
    let slice = format!("/user.slice/user-{uid}.slice/");
    let cgroup = process.cgroup.trim();
    let normal = cgroup
        .split('/')
        .skip(1)
        .all(|part| !part.is_empty() && part != "." && part != "..");
    if !normal || !cgroup.starts_with(&slice) {
        return false;
    }
    let scope = format!("{slice}session-{session}.scope");
    if cgroup == scope {
        return true;
    }
    match process.session_id.as_deref() {
        Some(id) => id == session,
        None => user_display == Some(session),
    }
}

/// Whether this Pier can carry out a Wayland console join (S4b supplies
/// the media and input adapter).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaylandJoinAdapter {
    /// The Pier joins a proven Wayland session.
    Available,
    /// The Pier cannot join one yet: a target is identified and proven,
    /// and the sign-in is refused visibly.
    NotYetAvailable,
}

/// What a sign-in does with the account's own Wayland session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaylandJoin {
    /// Join the session through its own compositor, process `compositor`.
    /// `activate` brings it forward first.
    Join {
        session: String,
        activate: bool,
        compositor: u32,
    },
    /// The session is proven but locked: GNOME stops screen sharing while
    /// it is locked, so a join takes the lock screen first (or waits for
    /// the desk to unlock it). The session is never ended or replaced.
    /// `activate` brings it forward first.
    HandleLock {
        session: String,
        activate: bool,
        compositor: u32,
    },
    /// Refused: a compositor not proven to be this session's is never
    /// joined.
    Refuse {
        refusal: ConsoleRefusal,
        proof: CompositorProof,
    },
    /// The target is identified, and what its proof said is known, but this
    /// Pier has no Wayland console adapter yet: refused visibly with
    /// [`Self::REFUSAL_WITHOUT_ADAPTER`].
    AdapterUnavailable {
        session: String,
        proof: CompositorProof,
        locked: bool,
    },
}

impl WaylandJoin {
    /// The refusal a Pier without the adapter shows: the same code as
    /// before the target was identified.
    pub const REFUSAL_WITHOUT_ADAPTER: ConsoleRefusal = ConsoleRefusal::WaylandSession;

    /// The refusal the Deck is shown, or `None` when the join goes ahead.
    #[must_use]
    pub const fn refusal(&self) -> Option<ConsoleRefusal> {
        match self {
            Self::Join { .. } | Self::HandleLock { .. } => None,
            Self::Refuse { refusal, .. } => Some(*refusal),
            Self::AdapterUnavailable { .. } => Some(Self::REFUSAL_WITHOUT_ADAPTER),
        }
    }

    /// Stable token for logs.
    #[must_use]
    pub const fn token(&self) -> &'static str {
        match self {
            Self::Join { .. } => "join",
            Self::HandleLock { .. } => "handle_lock",
            Self::Refuse { .. } => "refuse",
            Self::AdapterUnavailable { .. } => "identified_adapter_unavailable",
        }
    }
}

/// Decides what a sign-in does with `target`, the account's own Wayland
/// session, from what [`prove_compositor`] found and whether this Pier can
/// join one.
///
/// With the adapter: a proven session is joined, a proven locked session
/// has its lock screen handled first, and anything not proven is refused
/// `console_session_unreachable`. Without it, every case is
/// [`WaylandJoin::AdapterUnavailable`].
#[must_use]
pub fn decide_join(
    target: &WaylandTarget,
    proof: CompositorProof,
    adapter: WaylandJoinAdapter,
) -> WaylandJoin {
    if adapter == WaylandJoinAdapter::NotYetAvailable {
        return WaylandJoin::AdapterUnavailable {
            session: target.session.clone(),
            proof,
            locked: target.locked,
        };
    }
    match proof {
        CompositorProof::Verified { pid } if target.locked => WaylandJoin::HandleLock {
            session: target.session.clone(),
            activate: target.activate,
            compositor: pid,
        },
        CompositorProof::Verified { pid } => WaylandJoin::Join {
            session: target.session.clone(),
            activate: target.activate,
            compositor: pid,
        },
        CompositorProof::Unreadable(_) | CompositorProof::Mismatch(_) => WaylandJoin::Refuse {
            refusal: ConsoleRefusal::SessionUnreachable,
            proof,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::console_session::{CONSOLE_SEAT, ConsoleDecision, ConsoleHost, decide_on};

    const ANA: u32 = 1001;
    const BO: u32 = 1002;

    /// One change to healthy evidence.
    type Edit = fn(&mut CompositorEvidence);

    fn session(id: &str, uid: u32, class: &str, kind: &str, active: bool) -> ConsoleSessionFact {
        ConsoleSessionFact {
            id: id.to_owned(),
            uid,
            user: format!("u{uid}"),
            seat: CONSOLE_SEAT.to_owned(),
            vtnr: 2,
            class: class.to_owned(),
            session_type: kind.to_owned(),
            state: if active { "active" } else { "online" }.to_owned(),
            active,
            service: "gdm-password".to_owned(),
            leader: 4000 + uid,
            desktop: "gnome".to_owned(),
            ..ConsoleSessionFact::default()
        }
    }

    fn wayland(id: &str, uid: u32, active: bool) -> ConsoleSessionFact {
        session(id, uid, "user", "wayland", active)
    }

    fn wayland_greeter() -> ConsoleSessionFact {
        let mut greeter = session("c1", 42, "greeter", "wayland", true);
        greeter.service = "gdm-launch-environment".to_owned();
        greeter.vtnr = 1;
        greeter
    }

    fn owner(unique: &str, uid: u32, pid: u32) -> NameFact {
        NameFact::Owned(NameOwner {
            unique: unique.to_owned(),
            uid: Some(uid),
            pid: Some(pid),
        })
    }

    fn names(fact: &NameFact) -> Vec<(String, NameFact)> {
        MUTTER_NAMES
            .iter()
            .map(|name| ((*name).to_owned(), fact.clone()))
            .collect()
    }

    /// What the Pier reads of a healthy GNOME 49 session: gnome-shell under
    /// the user's systemd manager, `XDG_SESSION_ID` naming the session.
    fn evidence(fact: &ConsoleSessionFact) -> CompositorEvidence {
        let uid = fact.uid;
        let shell = owner(":1.7", uid, 2345);
        CompositorEvidence {
            session_after: Some(fact.clone()),
            runtime_dir: Some(format!("/run/user/{uid}")),
            runtime_dir_fact: Some(PathFact {
                kind: PathKind::Directory,
                owner: uid,
                mode: 0o700,
            }),
            bus_socket: Some(PathFact {
                kind: PathKind::Socket,
                owner: uid,
                mode: 0o666,
            }),
            bus_peer_uid: Some(uid),
            names: names(&shell),
            compositor: Some(ProcessFact {
                pid: 2345,
                uid,
                cgroup: format!(
                    "/user.slice/user-{uid}.slice/user@{uid}.service/session.slice/org.gnome.Shell@wayland.service"
                ),
                session_id: Some(fact.id.clone()),
            }),
            names_after: names(&shell),
            user_display: Some(fact.id.clone()),
        }
    }

    fn target(fact: &ConsoleSessionFact) -> WaylandTarget {
        WaylandTarget::of(fact, !fact.active)
    }

    #[test]
    fn the_accounts_own_wayland_session_is_the_target_not_a_refusal() {
        let mine = wayland("7", ANA, true);
        assert_eq!(
            decide_on(std::slice::from_ref(&mine), ANA, ConsoleHost::default()),
            ConsoleDecision::JoinWayland(WaylandTarget::of(&mine, false))
        );
        // In the background (behind the login screen): brought forward.
        let behind = wayland("8", ANA, false);
        let mut greeter = wayland_greeter();
        greeter.session_type = "x11".to_owned();
        assert_eq!(
            decide_on(&[greeter, behind.clone()], ANA, ConsoleHost::default()),
            ConsoleDecision::JoinWayland(WaylandTarget::of(&behind, true))
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One row per case of the table.
    fn the_wayland_decision_table() {
        let host = ConsoleHost::default();
        let no_dm = ConsoleHost {
            display_manager: false,
        };
        let mut locked = wayland("7", ANA, true);
        locked.locked = true;
        let mut background_class = session("9", ANA, "background", "unspecified", true);
        background_class.seat = String::new();
        let mut ssh = session("10", ANA, "user", "tty", true);
        ssh.seat = String::new();
        let mut arcen_desktop = wayland("11", ANA, true);
        arcen_desktop.seat = String::new();
        arcen_desktop.tty = crate::console_session::ARCEN_TTY.to_owned();
        let mut manager = session("12", ANA, "manager", "unspecified", true);
        manager.seat = String::new();
        let mut manager_early = manager.clone();
        manager_early.id = "13".to_owned();
        manager_early.class = "manager-early".to_owned();
        let mut x11_greeter = wayland_greeter();
        x11_greeter.session_type = "x11".to_owned();
        let mut locked_behind = wayland("8", ANA, false);
        locked_behind.locked = true;
        let mut other_product = wayland("7", BO, false);
        other_product.service = "xrdp-sesman".to_owned();
        let mut own_outside_dm = wayland("8", ANA, true);
        own_outside_dm.service = "login".to_owned();
        let rows: Vec<(&str, Vec<ConsoleSessionFact>, ConsoleHost, ConsoleDecision)> = vec![
            (
                "own locked Wayland session behind an X11 login screen: brought forward, locked",
                vec![x11_greeter.clone(), locked_behind.clone()],
                host,
                ConsoleDecision::JoinWayland(WaylandTarget::of(&locked_behind, true)),
            ),
            (
                "own Wayland session in front, an X11 one of the account behind it",
                vec![
                    wayland("6", ANA, true),
                    session("7", ANA, "user", "x11", false),
                ],
                host,
                ConsoleDecision::JoinWayland(WaylandTarget::of(&wayland("6", ANA, true), false)),
            ),
            (
                "own X11 session in front, a Wayland one of the account behind it",
                vec![
                    wayland("6", ANA, false),
                    session("7", ANA, "user", "x11", true),
                ],
                host,
                ConsoleDecision::Attach {
                    session: "7".to_owned(),
                    activate: false,
                    origin: ConsoleOrigin::DisplayManager,
                },
            ),
            (
                "another account's display-manager session in the background beside the account's own",
                vec![
                    x11_greeter.clone(),
                    wayland("7", BO, false),
                    wayland("8", ANA, false),
                ],
                host,
                ConsoleDecision::JoinWayland(WaylandTarget::of(&wayland("8", ANA, false), true)),
            ),
            (
                "another account's session of another product, in the background",
                vec![
                    x11_greeter.clone(),
                    other_product.clone(),
                    wayland("8", ANA, false),
                ],
                host,
                ConsoleDecision::Refuse(ConsoleRefusal::OtherUserOtherProduct),
            ),
            (
                "another account in the background and only a Wayland login screen",
                vec![wayland_greeter(), wayland("7", BO, false)],
                host,
                ConsoleDecision::Refuse(ConsoleRefusal::WaylandSession),
            ),
            (
                "another account in the background and an X11 login screen",
                vec![x11_greeter.clone(), wayland("7", BO, false)],
                host,
                ConsoleDecision::SignInThroughGreeter {
                    greeter: "c1".to_owned(),
                },
            ),
            (
                "the account's user managers are not a desktop",
                vec![manager.clone(), manager_early.clone()],
                no_dm,
                ConsoleDecision::StartLoginSession { vt: 7 },
            ),
            (
                "own Wayland session without a display manager: joined, never a second login session",
                vec![own_outside_dm.clone(), manager.clone()],
                no_dm,
                ConsoleDecision::JoinWayland(WaylandTarget::of(&own_outside_dm, false)),
            ),
            (
                "own active Wayland session",
                vec![wayland("7", ANA, true)],
                host,
                ConsoleDecision::JoinWayland(target(&wayland("7", ANA, true))),
            ),
            (
                "own locked Wayland session: the same target, locked",
                vec![locked.clone()],
                host,
                ConsoleDecision::JoinWayland(target(&locked)),
            ),
            (
                "another account's active Wayland session",
                vec![wayland("7", BO, true)],
                host,
                ConsoleDecision::Refuse(ConsoleRefusal::OtherUser),
            ),
            (
                "another account's Wayland session beside the account's own in the background",
                vec![wayland("7", BO, true), wayland("8", ANA, false)],
                host,
                ConsoleDecision::Refuse(ConsoleRefusal::OtherUser),
            ),
            (
                "only a Wayland login screen",
                vec![wayland_greeter()],
                host,
                ConsoleDecision::Refuse(ConsoleRefusal::WaylandSession),
            ),
            (
                "own X11 session is joined as before, preferred to a Wayland one",
                vec![
                    wayland("6", ANA, false),
                    session("7", ANA, "user", "x11", false),
                ],
                host,
                ConsoleDecision::Attach {
                    session: "7".to_owned(),
                    activate: true,
                    origin: ConsoleOrigin::DisplayManager,
                },
            ),
            (
                "background, SSH and Arcen desktops are not the workstation session",
                vec![background_class.clone(), ssh.clone(), arcen_desktop.clone()],
                host,
                ConsoleDecision::Refuse(ConsoleRefusal::NoConsoleSession),
            ),
            (
                "genuinely absent without a display manager: the X11 login session is created",
                vec![background_class, ssh, arcen_desktop],
                no_dm,
                ConsoleDecision::StartLoginSession { vt: 7 },
            ),
        ];
        for (row, sessions, host, expected) in rows {
            assert_eq!(decide_on(&sessions, ANA, host), expected, "{row}");
        }
    }

    #[test]
    fn a_host_that_creates_wayland_desktops_never_creates_one_beside_the_accounts_own() {
        let mine = wayland("7", ANA, false);
        assert_eq!(
            decide_before_creating(&[wayland_greeter(), mine.clone()], ANA),
            BeforeCreating::JoinWayland(WaylandTarget::of(&mine, true))
        );
        assert_eq!(
            decide_before_creating(&[session("7", ANA, "user", "x11", true)], ANA),
            BeforeCreating::Refuse(ConsoleRefusal::SessionUnreachable)
        );
        // Nothing of the account's own: its desktop is created, as before,
        // whoever else holds the console.
        let mut arcen_desktop = wayland("11", ANA, true);
        arcen_desktop.seat = String::new();
        arcen_desktop.tty = crate::console_session::ARCEN_TTY.to_owned();
        let mut native_seat = wayland("12", ANA, true);
        native_seat.seat = "seat-arcen-1".to_owned();
        let mut closing = wayland("13", ANA, false);
        closing.state = "closing".to_owned();
        for (row, sessions) in [
            ("nothing", vec![]),
            ("a login screen", vec![wayland_greeter()]),
            ("another account", vec![wayland("7", BO, true)]),
            (
                "Arcen's own desktops and a closing session",
                vec![arcen_desktop, native_seat, closing],
            ),
        ] {
            assert_eq!(
                decide_before_creating(&sessions, ANA),
                BeforeCreating::Create,
                "{row}"
            );
        }
    }

    #[test]
    fn a_wayland_creating_pier_creates_only_when_the_account_proved_to_hold_nothing() {
        use crate::console_session::SessionsUnreadable;
        let step_with = |read: &crate::console_session::SessionRead,
                         proof: CompositorProof,
                         adapter: WaylandJoinAdapter| {
            let decision = decide_before_creating_read(read, ANA);
            let join = match &decision {
                BeforeCreating::JoinWayland(target) => Some(decide_join(target, proof, adapter)),
                _ => None,
            };
            creating_step(&decision, join.as_ref())
        };
        let refusal = |read: &crate::console_session::SessionRead| match step_with(
            read,
            CompositorProof::Verified { pid: 2345 },
            WaylandJoinAdapter::NotYetAvailable,
        ) {
            CreatingStep::Create => None,
            CreatingStep::Refuse(refusal) => Some(refusal),
            CreatingStep::JoinConsole => panic!("no join without the adapter"),
        };
        let mut arcen_desktop = wayland("11", ANA, true);
        arcen_desktop.seat = String::new();
        arcen_desktop.tty = crate::console_session::ARCEN_TTY.to_owned();
        let mut native_seat = arcen_desktop.clone();
        native_seat.id = "12".to_owned();
        native_seat.seat = "seat-arcen-1".to_owned();
        let mut other_seat = wayland("13", ANA, true);
        other_seat.seat = "seat1".to_owned();
        let mut closing = wayland("14", ANA, false);
        closing.state = "closing".to_owned();
        let mut ssh = session("15", ANA, "user", "tty", true);
        ssh.seat = String::new();
        let mut locked_behind = wayland("7", ANA, false);
        locked_behind.locked = true;
        let rows: Vec<(
            &str,
            crate::console_session::SessionRead,
            Option<ConsoleRefusal>,
        )> = vec![
            ("nothing", Ok(vec![]), None),
            ("only a login screen", Ok(vec![wayland_greeter()]), None),
            (
                "another account at the desk",
                Ok(vec![wayland("7", BO, true)]),
                None,
            ),
            (
                "Arcen's own seatless and native-seat desktops",
                Ok(vec![arcen_desktop, native_seat]),
                None,
            ),
            ("a session on another seat", Ok(vec![other_seat]), None),
            (
                "a closing session and an SSH session",
                Ok(vec![closing, ssh]),
                None,
            ),
            (
                "logind unreadable",
                Err(SessionsUnreadable::ListFailed),
                Some(ConsoleRefusal::SessionsUnreadable),
            ),
            (
                "logind partly readable",
                Err(SessionsUnreadable::SessionUndescribed),
                Some(ConsoleRefusal::SessionsUnreadable),
            ),
            (
                "the account's Wayland session in front",
                Ok(vec![wayland("7", ANA, true)]),
                Some(ConsoleRefusal::WaylandSession),
            ),
            (
                "the account's Wayland session locked behind a login screen",
                Ok(vec![wayland_greeter(), locked_behind]),
                Some(ConsoleRefusal::WaylandSession),
            ),
            (
                "the account's X11 session",
                Ok(vec![session("7", ANA, "user", "x11", true)]),
                Some(ConsoleRefusal::SessionUnreachable),
            ),
        ];
        for (row, read, expected) in rows {
            assert_eq!(refusal(&read), expected, "{row}");
        }
        // With the adapter, the account's own proven session is joined as a
        // console session, in front or locked; never a desktop beside it.
        let mut locked = wayland("7", ANA, true);
        locked.locked = true;
        for session in [wayland("7", ANA, true), locked] {
            let read = Ok(vec![session]);
            assert_eq!(
                step_with(
                    &read,
                    CompositorProof::Verified { pid: 2345 },
                    WaylandJoinAdapter::Available
                ),
                CreatingStep::JoinConsole
            );
            assert_eq!(
                step_with(
                    &read,
                    CompositorProof::Mismatch(CompositorMismatch::NotMutter),
                    WaylandJoinAdapter::Available
                ),
                CreatingStep::Refuse(ConsoleRefusal::SessionUnreachable)
            );
        }
        // No join asked: refused.
        let target =
            BeforeCreating::JoinWayland(WaylandTarget::of(&wayland("7", ANA, true), false));
        assert_eq!(
            creating_step(&target, None),
            CreatingStep::Refuse(ConsoleRefusal::WaylandSession)
        );
    }

    #[test]
    fn a_healthy_session_is_proven_in_the_users_manager_or_its_own_scope() {
        let mine = wayland("7", ANA, true);
        let proof = prove_compositor(&target(&mine), &evidence(&mine));
        assert_eq!(proof, CompositorProof::Verified { pid: 2345 });
        // Without XDG_SESSION_ID, Mutter takes the user's display session.
        let mut no_env = evidence(&mine);
        no_env.compositor.as_mut().unwrap().session_id = None;
        assert_eq!(
            prove_compositor(&target(&mine), &no_env),
            CompositorProof::Verified { pid: 2345 }
        );
        // A compositor in the session's own scope (older GNOME).
        let mut scoped = evidence(&mine);
        let process = scoped.compositor.as_mut().unwrap();
        process.cgroup = format!("/user.slice/user-{ANA}.slice/session-7.scope");
        process.session_id = None;
        scoped.user_display = None;
        assert_eq!(
            prove_compositor(&target(&mine), &scoped),
            CompositorProof::Verified { pid: 2345 }
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One row per case of the table.
    fn ownership_and_peer_mismatches_are_never_proven() {
        let mine = wayland("7", ANA, true);
        let t = target(&mine);
        let cases: [(&str, Edit, CompositorMismatch); 13] = [
            (
                "another runtime directory",
                |e| e.runtime_dir = Some("/run/user/1002".into()),
                CompositorMismatch::RuntimeDirNotTheUsers,
            ),
            (
                "a runtime directory others can enter",
                |e| e.runtime_dir_fact.as_mut().unwrap().mode = 0o755,
                CompositorMismatch::RuntimeDirNotPrivate,
            ),
            (
                "a runtime directory of another owner",
                |e| e.runtime_dir_fact.as_mut().unwrap().owner = BO,
                CompositorMismatch::RuntimeDirNotPrivate,
            ),
            (
                "a bus socket of another owner",
                |e| e.bus_socket.as_mut().unwrap().owner = BO,
                CompositorMismatch::BusSocketNotTheUsers,
            ),
            (
                "a bus path that is not a socket",
                |e| e.bus_socket.as_mut().unwrap().kind = PathKind::Other,
                CompositorMismatch::BusSocketNotTheUsers,
            ),
            (
                "a bus served by another account (kernel peer credentials)",
                |e| e.bus_peer_uid = Some(BO),
                CompositorMismatch::BusServedByAnotherUser,
            ),
            (
                "a Mutter name nobody owns",
                |e| e.names[1].1 = NameFact::Unowned,
                CompositorMismatch::NotMutter,
            ),
            (
                "Mutter names on two connections",
                |e| e.names[2].1 = owner(":1.99", ANA, 2345),
                CompositorMismatch::NamesSplit,
            ),
            (
                "names owned by another account, as the bus says",
                |e| {
                    let other = owner(":1.7", BO, 2345);
                    e.names = names(&other);
                    e.names_after = names(&other);
                },
                CompositorMismatch::OwnerNotTheUser,
            ),
            (
                "the owner process runs as another account, as the kernel says",
                |e| e.compositor.as_mut().unwrap().uid = BO,
                CompositorMismatch::OwnerNotTheUser,
            ),
            (
                "the owner process is in another account's slice",
                |e| {
                    e.compositor.as_mut().unwrap().cgroup =
                        "/user.slice/user-10010.slice/user@10010.service/x.service".into();
                },
                CompositorMismatch::OwnerOutsideSession,
            ),
            (
                "Mutter took another session of the account",
                |e| e.compositor.as_mut().unwrap().session_id = Some("8".into()),
                CompositorMismatch::OwnerOutsideSession,
            ),
            (
                "the names moved while they were read",
                |e| e.names_after = names(&owner(":1.8", ANA, 2399)),
                CompositorMismatch::OwnerChanged,
            ),
        ];
        for (row, edit, expected) in cases {
            let mut e = evidence(&mine);
            edit(&mut e);
            assert_eq!(
                prove_compositor(&t, &e),
                CompositorProof::Mismatch(expected),
                "{row}"
            );
        }
        // A system-service cgroup that only ends like the account's slice.
        let mut e = evidence(&mine);
        e.compositor.as_mut().unwrap().cgroup =
            format!("/system.slice/user.slice/user-{ANA}.slice/session-7.scope");
        assert_eq!(
            prove_compositor(&t, &e),
            CompositorProof::Mismatch(CompositorMismatch::OwnerOutsideSession)
        );
        // Without XDG_SESSION_ID, while another session is the display one.
        let mut e = evidence(&mine);
        e.compositor.as_mut().unwrap().session_id = None;
        e.user_display = Some("8".into());
        assert_eq!(
            prove_compositor(&t, &e),
            CompositorProof::Mismatch(CompositorMismatch::OwnerOutsideSession)
        );
        // The bus's pid for the owner is not the process that was read.
        let mut e = evidence(&mine);
        e.compositor.as_mut().unwrap().pid = 2346;
        assert_eq!(
            prove_compositor(&t, &e),
            CompositorProof::Mismatch(CompositorMismatch::OwnerChanged)
        );
    }

    #[test]
    fn a_reused_or_replaced_session_id_is_never_proven() {
        let mine = wayland("7", ANA, true);
        let t = target(&mine);
        let mut reused = mine.clone();
        reused.leader = 9999;
        let mut other_owner = mine.clone();
        other_owner.uid = BO;
        let mut now_x11 = mine.clone();
        now_x11.session_type = "x11".into();
        let mut closing = mine.clone();
        closing.state = "closing".into();
        for (row, after) in [
            ("another leader under the same id", reused),
            ("another owner under the same id", other_owner),
            ("no longer Wayland", now_x11),
            ("closing", closing),
        ] {
            let mut e = evidence(&mine);
            e.session_after = Some(after);
            assert_eq!(
                prove_compositor(&t, &e),
                CompositorProof::Mismatch(CompositorMismatch::SessionReplaced),
                "{row}"
            );
        }
    }

    #[test]
    fn unreadable_facts_prove_nothing() {
        let mine = wayland("7", ANA, true);
        let t = target(&mine);
        let cases: [(Edit, EvidenceGap); 9] = [
            (|e| e.session_after = None, EvidenceGap::Session),
            (|e| e.runtime_dir = None, EvidenceGap::RuntimeDir),
            (|e| e.runtime_dir_fact = None, EvidenceGap::RuntimeDir),
            (|e| e.bus_socket = None, EvidenceGap::BusSocket),
            (|e| e.bus_peer_uid = None, EvidenceGap::BusPeer),
            (
                |e| e.names[0].1 = NameFact::Unreadable,
                EvidenceGap::BusNames,
            ),
            (|e| e.names.clear(), EvidenceGap::BusNames),
            (|e| e.compositor = None, EvidenceGap::Compositor),
            (
                |e| e.names_after[2].1 = NameFact::Unreadable,
                EvidenceGap::BusNames,
            ),
        ];
        for (edit, gap) in cases {
            let mut e = evidence(&mine);
            edit(&mut e);
            assert_eq!(
                prove_compositor(&t, &e),
                CompositorProof::Unreadable(gap),
                "{}",
                gap.token()
            );
        }
        assert_eq!(
            prove_compositor(&t, &CompositorEvidence::default()),
            CompositorProof::Unreadable(EvidenceGap::Session)
        );
    }

    #[test]
    fn with_the_adapter_a_proven_session_is_joined_and_a_locked_one_takes_its_lock_screen_first() {
        let mine = wayland("7", ANA, false);
        let verified = CompositorProof::Verified { pid: 2345 };
        assert_eq!(
            decide_join(&target(&mine), verified, WaylandJoinAdapter::Available),
            WaylandJoin::Join {
                session: "7".into(),
                activate: true,
                compositor: 2345
            }
        );
        let mut locked = wayland("7", ANA, true);
        locked.locked = true;
        let join = decide_join(&target(&locked), verified, WaylandJoinAdapter::Available);
        assert_eq!(
            join,
            WaylandJoin::HandleLock {
                session: "7".into(),
                activate: false,
                compositor: 2345
            }
        );
        assert_eq!(join.refusal(), None);
        let unproven = CompositorProof::Mismatch(CompositorMismatch::BusServedByAnotherUser);
        let join = decide_join(&target(&locked), unproven, WaylandJoinAdapter::Available);
        assert_eq!(
            join,
            WaylandJoin::Refuse {
                refusal: ConsoleRefusal::SessionUnreachable,
                proof: unproven
            }
        );
        assert_eq!(join.refusal(), Some(ConsoleRefusal::SessionUnreachable));
    }

    #[test]
    fn without_the_adapter_every_target_is_identified_and_refused_as_before() {
        let mut locked = wayland("7", ANA, true);
        locked.locked = true;
        for (fact, proof) in [
            (
                wayland("7", ANA, true),
                CompositorProof::Verified { pid: 1 },
            ),
            (locked, CompositorProof::Verified { pid: 1 }),
            (
                wayland("7", ANA, true),
                CompositorProof::Unreadable(EvidenceGap::BusNames),
            ),
        ] {
            let join = decide_join(&target(&fact), proof, WaylandJoinAdapter::NotYetAvailable);
            assert_eq!(
                join,
                WaylandJoin::AdapterUnavailable {
                    session: "7".into(),
                    proof,
                    locked: fact.locked
                }
            );
            assert_eq!(join.refusal(), Some(ConsoleRefusal::WaylandSession));
            assert_eq!(join.token(), "identified_adapter_unavailable");
        }
    }

    #[test]
    fn the_target_names_logind_runtime_directory_and_bus() {
        let t = target(&wayland("7", ANA, true));
        assert_eq!(t.runtime_dir(), "/run/user/1001");
        assert_eq!(t.bus_socket(), "/run/user/1001/bus");
        assert_eq!(t.leader, 4000 + ANA);
        assert_eq!(t.desktop, "gnome");
    }
}
