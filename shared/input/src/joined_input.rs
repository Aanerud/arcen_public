// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Arcen's typed input devices on a joined console desktop (ADR 0025): the
//! proof that each one reaches that desktop's display server alone, and
//! what a session does when the proof lapses.
//!
//! A Linux Pier injects through kernel virtual devices
//! ([`crate::virtual_device`]): an absolute keyboard and pointer, a
//! relative pointer and a tablet-tool pen. A dedicated Arcen desktop admits
//! them by name and every other Arcen desktop ignores them. A joined
//! console's display server is not one Arcen starts: it admits any device on
//! its seat, and so does any other display server on that seat (a login
//! screen, another account's desktop, another product's). So each device is
//! used only while [`prove_joined_device_confinement`] holds for it: the
//! device exists, its only kernel handlers with a node are `evdev`,
//! `mousedev` (which also feeds the multiplexed mouse) and `joydev`, the
//! joined desktop's display server
//! has its event node open (the device is admitted, not merely created),
//! no foreign reader has any of its nodes open, and the joined session is
//! the one in front of the seat. [`classify_reader`] decides who is
//! foreign: the threat is input reaching another session or another display
//! server, not a system service outside every session. Another display
//! server (even in the background: brought forward, it would read the
//! device), a process of another login session (another account's, a login
//! screen's), another account's process and a known input bridge are
//! foreign; the input broker (logind, and its descriptor store), system
//! services such as `acpid`, `thd`, `gpm` or `brltty`, and the joined
//! session's and account's own processes are not. A reader that cannot be
//! attributed is foreign.
//!
//! What the proof cannot see: it reads who has a node open now, from the
//! process table. Someone already allowed to open the devices (root, the
//! `input` group, an ACL such as `uaccess` grants) can read them unseen:
//! through a descriptor in flight over `SCM_RIGHTS`, an `io_uring` fixed
//! file, or a thread's own descriptor table. That is the access they have
//! to the physical keyboard too; the proof keeps input from other sessions
//! and display servers, not from the host's administrators. The same
//! people can stop a joined session (fail closed) by holding a node open
//! from a process the proof counts as foreign, or as unattributable
//! (another namespace), as can a display server or input remapper an
//! administrator runs. Anyone else cannot: the platform adapter counts
//! only a stable, readable open of the device itself (on Linux, matched by
//! device number with the descriptor's own record, never an `O_PATH`
//! handle or a name), and a process that cannot open a node cannot hold
//! one.
//!
//! [`JoinedInputGuard`] turns the proofs into what the session does
//! (fail safe). Keyboard and pointers are essential: a lapse stops their
//! input at once (nothing is injected, not even releases, while the devices
//! could reach someone else); when the proof holds again within
//! [`JOINED_INPUT_GRACE`] held keys and buttons are released first, then
//! input resumes; otherwise the attachment ends with a reason the Deck
//! shows. The pen is optional: a lapse withdraws it for the rest of the
//! attachment and the Deck is told, while the session goes on.
//!
//! Policy only: a platform adapter gathers [`JoinedDeviceFacts`] from the
//! operating system and acts on the decisions.

use crate::virtual_device::VirtualInputRole;
use arcen_protocol::messages::{TabletModeMsg, TabletModeReason, TabletModeResultMsg};
use arcen_usb_bridge::JoinedPenRefusal;
use std::fmt::{Display, Formatter};
use std::time::{Duration, Instant};

/// Highest uid of a system account (the `login.defs` default
/// `SYS_UID_MAX`): such an account's process outside any login session is a
/// system service.
pub const SYSTEM_UID_MAX: u32 = 999;

/// Whether `uid` is a system account's: up to [`SYSTEM_UID_MAX`], systemd's
/// dynamic service users (61184-65519) and `nobody` (65534), the ranges
/// systemd reserves for services (its `UIDS-GIDS` allocation). Accounts
/// people log in with are above `SYSTEM_UID_MAX` and outside them.
#[must_use]
pub const fn is_system_uid(uid: u32) -> bool {
    uid <= SYSTEM_UID_MAX || matches!(uid, 61_184..=65_519 | 65_534)
}

/// Longest process name kept (the kernel's `comm` is 15 bytes).
const MAX_COMM_CHARS: usize = 15;

/// Display servers and compositors, by process name: any of them that is
/// not the joined desktop's own is a foreign reader.
pub const DISPLAY_SERVERS: &[&str] = &[
    "Xorg",
    "X",
    "Xwayland",
    "gnome-shell",
    "mutter",
    "kwin_wayland",
    "kwin_x11",
    "weston",
    "sway",
    "Hyprland",
    "labwc",
    "cage",
];

/// Input remappers that grab devices and send what they read out again
/// through a virtual device of their own, which every session on the seat
/// receives; by process name (the kernel's 15-byte `comm`). One holding
/// Arcen's device is a foreign reader even as a system service: whatever
/// Arcen injects would reach every session. The list names well-behaved
/// software an administrator installed, not an adversary: a system service
/// already runs as a system account, and a process of another account is
/// foreign whatever its name.
pub const INPUT_BRIDGES: &[&str] = &[
    "keyd",
    "input-remapper-",
    "evsieve",
    "kmonad",
    "kanata",
    "udevmon",
    "intercept",
    "interception",
    "evremap",
    "xremap",
];

/// What a platform adapter knows about one process that has a node open.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReaderFacts {
    pub pid: u32,
    /// Its name (`comm`).
    pub comm: String,
    /// Its real uid; `None` when it could not be read.
    pub uid: Option<u32>,
    /// The login session it belongs to; `None` for none (a system service,
    /// a user's own service manager).
    pub session: Option<String>,
    /// The account whose own service manager it runs under (systemd's
    /// `user@<uid>.service`, delegated to that account), when it is in no
    /// session: part of that account's login, never a system service, even
    /// for a system account such as a login screen's.
    pub user_manager: Option<u32>,
    /// Its descriptors could be read: what it has open is known.
    pub readable: bool,
    /// It runs in the host's own mount and network namespaces (pid 1's).
    pub host_namespaces: bool,
    /// It is the joined desktop's display server (identified by the
    /// adapter, by the socket it listens on).
    pub joined_server: bool,
}

/// The joined desktop a reader is judged against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinedIdentity {
    /// Its login session.
    pub session: String,
    /// Its account.
    pub uid: u32,
}

/// Why a reader cannot be attributed to anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unattributable {
    /// Its open descriptors could not be read.
    Unreadable,
    /// It runs in a mount or network namespace of its own.
    Namespaced,
}

/// Who has one of the typed input device's nodes open, and what that means.
///
/// Allowed: the joined desktop's display server, the input broker, the
/// joined session's own processes, the joined account's own services, and
/// system services (a system account's process in no login session, such as
/// `acpid`, `thd`, `gpm`, `brltty`, `upowerd`). Foreign: another display
/// server, a process of another login session (another account's, a login
/// screen's), another account's process outside any session, and a known
/// input bridge that sends what it reads to every session. A reader that
/// cannot be attributed fails closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceNodeReader {
    /// The joined desktop's own display server.
    JoinedDisplayServer,
    /// The seat's input broker (logind, and the service manager's
    /// descriptor store that keeps logind's descriptors), which opens a
    /// session's devices on its behalf and revokes them when the session
    /// leaves the front; it reads no events.
    InputBroker,
    /// A process of the joined session itself.
    JoinedSession { pid: u32, comm: String },
    /// The joined account's own process outside any session (its service
    /// manager and services).
    JoinedAccount { pid: u32, comm: String },
    /// A system service: a system account's process in no login session.
    SystemService { pid: u32, uid: u32, comm: String },
    /// Another display server or compositor (a login screen, another
    /// account's desktop, another product's).
    OtherDisplayServer { pid: u32, comm: String },
    /// A process of another login session (another account's desktop or
    /// terminal, a login screen's helpers).
    OtherSession {
        pid: u32,
        uid: Option<u32>,
        session: String,
        comm: String,
    },
    /// Another account's process outside any session.
    OtherAccount { pid: u32, uid: u32, comm: String },
    /// A known input bridge ([`INPUT_BRIDGES`]).
    InputBridge { pid: u32, comm: String },
    /// A process that cannot be attributed.
    Unattributable {
        pid: u32,
        comm: String,
        why: Unattributable,
    },
}

impl DeviceNodeReader {
    /// Whether this reader may hold the node.
    #[must_use]
    pub const fn admitted(&self) -> bool {
        matches!(
            self,
            Self::JoinedDisplayServer
                | Self::InputBroker
                | Self::JoinedSession { .. }
                | Self::JoinedAccount { .. }
                | Self::SystemService { .. }
        )
    }

    /// An allowed reader worth reporting (not the joined server or the
    /// broker, which every joined desktop has).
    #[must_use]
    pub const fn reported(&self) -> bool {
        matches!(
            self,
            Self::JoinedSession { .. } | Self::JoinedAccount { .. } | Self::SystemService { .. }
        )
    }

    /// How an allowed reader worth reporting is logged, once per
    /// attachment: by name and account, not by process.
    #[must_use]
    pub fn report_label(&self) -> Option<String> {
        match self {
            Self::JoinedSession { comm, .. } => Some(format!("{comm} (joined session)")),
            Self::JoinedAccount { comm, .. } => Some(format!("{comm} (joined account)")),
            Self::SystemService { uid, comm, .. } => {
                Some(format!("{comm} (system service, uid {uid})"))
            }
            _ => None,
        }
    }
}

/// Classifies one reader of a joined desktop's device (see
/// [`DeviceNodeReader`]).
#[must_use]
pub fn classify_reader(reader: &ReaderFacts, joined: &JoinedIdentity) -> DeviceNodeReader {
    let pid = reader.pid;
    // A process names itself: what is shown is short and plain ASCII, so
    // no control, format or bidirectional character (Unicode `Cc`, `Cf`)
    // reaches a log line or the Deck.
    let shown = |text: &str, limit| -> String {
        text.chars()
            .take(limit)
            .map(|c| {
                if c.is_ascii_graphic() || c == ' ' {
                    c
                } else {
                    '?'
                }
            })
            .collect()
    };
    let comm = shown(&reader.comm, MAX_COMM_CHARS);
    if reader.joined_server {
        return DeviceNodeReader::JoinedDisplayServer;
    }
    let root = reader.uid == Some(0);
    if root && (reader.comm == "systemd-logind" || (pid == 1 && reader.comm == "systemd")) {
        return DeviceNodeReader::InputBroker;
    }
    if !reader.readable {
        return DeviceNodeReader::Unattributable {
            pid,
            comm,
            why: Unattributable::Unreadable,
        };
    }
    if !reader.host_namespaces {
        return DeviceNodeReader::Unattributable {
            pid,
            comm,
            why: Unattributable::Namespaced,
        };
    }
    if DISPLAY_SERVERS.contains(&reader.comm.as_str()) {
        return DeviceNodeReader::OtherDisplayServer { pid, comm };
    }
    if INPUT_BRIDGES.contains(&reader.comm.as_str()) {
        return DeviceNodeReader::InputBridge { pid, comm };
    }
    match (reader.session.as_deref(), reader.uid) {
        (Some(session), _) if session == joined.session => {
            DeviceNodeReader::JoinedSession { pid, comm }
        }
        (Some(session), uid) => DeviceNodeReader::OtherSession {
            pid,
            uid,
            session: shown(session, MAX_COMM_CHARS),
            comm,
        },
        (None, _) if reader.user_manager == Some(joined.uid) => {
            DeviceNodeReader::JoinedAccount { pid, comm }
        }
        (None, uid) if reader.user_manager.is_some() => DeviceNodeReader::OtherAccount {
            pid,
            uid: uid.or(reader.user_manager).unwrap_or_default(),
            comm,
        },
        (None, Some(uid)) if uid == joined.uid => DeviceNodeReader::JoinedAccount { pid, comm },
        (None, Some(uid)) if is_system_uid(uid) => {
            DeviceNodeReader::SystemService { pid, uid, comm }
        }
        (None, Some(uid)) => DeviceNodeReader::OtherAccount { pid, uid, comm },
        (None, None) => DeviceNodeReader::Unattributable {
            pid,
            comm,
            why: Unattributable::Unreadable,
        },
    }
}

impl Display for DeviceNodeReader {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::JoinedDisplayServer => f.write_str("the joined desktop's display server"),
            Self::InputBroker => f.write_str("the input broker"),
            Self::JoinedSession { pid, comm } => {
                write!(f, "{comm} (pid {pid}) of the joined session")
            }
            Self::JoinedAccount { pid, comm } => {
                write!(f, "{comm} (pid {pid}) of the joined account")
            }
            Self::SystemService { pid, uid, comm } => {
                write!(f, "system service {comm} (pid {pid}, uid {uid})")
            }
            Self::OtherDisplayServer { pid, comm } => {
                write!(f, "display server {comm} (pid {pid})")
            }
            Self::OtherSession {
                pid, session, comm, ..
            } => write!(f, "{comm} (pid {pid}) of session {session}"),
            Self::OtherAccount { pid, uid, comm } => write!(f, "{comm} (pid {pid}, uid {uid})"),
            Self::InputBridge { pid, comm } => write!(f, "input remapper {comm} (pid {pid})"),
            Self::Unattributable { pid, comm, why } => match why {
                Unattributable::Unreadable => write!(f, "{comm} (pid {pid}, unreadable)"),
                Unattributable::Namespaced => {
                    write!(f, "{comm} (pid {pid}, in its own namespace)")
                }
            },
        }
    }
}

/// The kernel input handler behind one node of the typed input device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceNodeHandler {
    /// `evdev` (`eventN`): what display servers read.
    Event,
    /// `mousedev` (`mouseN`), which also feeds the multiplexed mouse every
    /// mouse handler shares.
    Mouse,
    /// `joydev` (`jsN`): it takes any device with an absolute X axis that
    /// it does not recognise as a mouse or tablet, the absolute keyboard and
    /// pointer included (seen on the Xorg lab host).
    Joystick,
    /// Any other handler: not expected for a typed device.
    Other,
}

impl DeviceNodeHandler {
    /// The handler a node's kernel name (`event12`, `mouse3`) belongs to.
    #[must_use]
    pub fn from_node_name(name: &str) -> Self {
        let numbered = |prefix: &str| {
            name.strip_prefix(prefix).is_some_and(|number| {
                !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit())
            })
        };
        if numbered("event") {
            Self::Event
        } else if numbered("mouse") {
            Self::Mouse
        } else if numbered("js") {
            Self::Joystick
        } else {
            Self::Other
        }
    }
}

/// One node of the typed input device and who has it open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceNodeFacts {
    /// The node's kernel name (`event12`).
    pub name: String,
    pub handler: DeviceNodeHandler,
    pub readers: Vec<DeviceNodeReader>,
}

impl DeviceNodeFacts {
    /// A node named `name` with `readers`.
    #[must_use]
    pub fn new(name: &str, readers: Vec<DeviceNodeReader>) -> Self {
        Self {
            name: name.to_owned(),
            handler: DeviceNodeHandler::from_node_name(name),
            readers,
        }
    }
}

/// What a platform adapter knows about its typed input device on a joined
/// desktop.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct JoinedDeviceFacts {
    /// The typed input device exists.
    pub device: bool,
    /// The joined desktop's display server was identified.
    pub joined_server: bool,
    /// The joined session is the one in front of its seat.
    pub joined_in_front: bool,
    /// Every node of the device.
    pub nodes: Vec<DeviceNodeFacts>,
    /// Who has the multiplexed mouse open, which a `mousedev` node feeds.
    pub multiplexed_mouse_readers: Vec<DeviceNodeReader>,
}

/// Why the typed input device is not proved to reach the joined desktop
/// alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JoinedDeviceGap {
    /// There is no typed input device (it could not be created, or was
    /// released).
    NoDevice,
    /// The device has no node the facts name.
    NoNodes,
    /// A handler other than `evdev` or `mousedev` took the device.
    UnknownHandler { node: String },
    /// A reader other than the joined desktop's display server or the input
    /// broker has a node open.
    ForeignReader {
        node: String,
        reader: DeviceNodeReader,
    },
    /// The joined desktop's display server could not be identified.
    NoJoinedServer,
    /// The joined session is not in front of its seat.
    NotInFront,
    /// The joined desktop's display server has not opened the event node:
    /// it has not admitted the device (yet).
    NotAdmitted,
}

impl JoinedDeviceGap {
    /// What this gap means for the host-decoded Native tablet route
    /// ([`arcen_usb_bridge::native_tablet_route`]).
    #[must_use]
    pub const fn pen_refusal(&self) -> JoinedPenRefusal {
        match self {
            Self::NoDevice => JoinedPenRefusal::NoDevice,
            Self::NoNodes | Self::UnknownHandler { .. } | Self::ForeignReader { .. } => {
                JoinedPenRefusal::NotConfined
            }
            Self::NoJoinedServer | Self::NotInFront | Self::NotAdmitted => {
                JoinedPenRefusal::NotReady
            }
        }
    }

    /// Whether the device is proved to reach another reader (as opposed to
    /// not being ready for the joined desktop yet).
    #[must_use]
    pub const fn reaches_another_reader(&self) -> bool {
        matches!(
            self,
            Self::NoNodes | Self::UnknownHandler { .. } | Self::ForeignReader { .. }
        )
    }

    /// Whether waiting may close the gap (the display server is still
    /// adding the device).
    #[must_use]
    pub const fn may_settle(&self) -> bool {
        matches!(self, Self::NotAdmitted)
    }
}

impl Display for JoinedDeviceGap {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoDevice => f.write_str("no typed input device exists"),
            Self::NoNodes => f.write_str("the typed input device has no input node"),
            Self::UnknownHandler { node } => {
                write!(f, "an unexpected kernel handler took the device ({node})")
            }
            Self::ForeignReader { node, reader } => {
                write!(f, "{reader} has the device's node {node} open")
            }
            Self::NoJoinedServer => {
                f.write_str("the joined desktop's display server was not identified")
            }
            Self::NotInFront => f.write_str("the joined session is not in front of its seat"),
            Self::NotAdmitted => {
                f.write_str("the joined desktop's display server has not opened the device")
            }
        }
    }
}

impl std::error::Error for JoinedDeviceGap {}

/// Proves from `facts` that the typed input device reaches the joined
/// desktop's display server and nothing else (see the module).
///
/// # Errors
///
/// The first gap, confinement before readiness.
pub fn prove_joined_device_confinement(facts: &JoinedDeviceFacts) -> Result<(), JoinedDeviceGap> {
    if !facts.device {
        return Err(JoinedDeviceGap::NoDevice);
    }
    if facts.nodes.is_empty() {
        return Err(JoinedDeviceGap::NoNodes);
    }
    for node in &facts.nodes {
        if node.handler == DeviceNodeHandler::Other {
            return Err(JoinedDeviceGap::UnknownHandler {
                node: node.name.clone(),
            });
        }
        if let Some(reader) = node.readers.iter().find(|reader| !reader.admitted()) {
            return Err(JoinedDeviceGap::ForeignReader {
                node: node.name.clone(),
                reader: reader.clone(),
            });
        }
        if node.handler == DeviceNodeHandler::Mouse {
            if let Some(reader) = facts
                .multiplexed_mouse_readers
                .iter()
                .find(|reader| !reader.admitted())
            {
                return Err(JoinedDeviceGap::ForeignReader {
                    node: "mice".to_owned(),
                    reader: reader.clone(),
                });
            }
        }
    }
    if !facts.joined_server {
        return Err(JoinedDeviceGap::NoJoinedServer);
    }
    if !facts.joined_in_front {
        return Err(JoinedDeviceGap::NotInFront);
    }
    let admitted = facts.nodes.iter().any(|node| {
        node.handler == DeviceNodeHandler::Event
            && node
                .readers
                .contains(&DeviceNodeReader::JoinedDisplayServer)
    });
    if admitted {
        Ok(())
    } else {
        Err(JoinedDeviceGap::NotAdmitted)
    }
}

/// How long keyboard and pointer input may stay unproved before the
/// attachment ends. Long enough for a login screen that flashes up and goes
/// (a display manager's race after a sign-in) to leave; short enough that a
/// Deck is not left typing into nothing.
pub const JOINED_INPUT_GRACE: Duration = Duration::from_secs(5);

/// How often the full proof is repeated while a Deck is attached.
pub const JOINED_INPUT_RECHECK: Duration = Duration::from_secs(2);

/// Whether a session cannot go on without `role`: keyboard and pointers
/// are essential, the pen is not.
#[must_use]
pub const fn essential(role: VirtualInputRole) -> bool {
    !matches!(role, VirtualInputRole::TabletPen)
}

const fn slot(role: VirtualInputRole) -> usize {
    match role {
        VirtualInputRole::Absolute => 0,
        VirtualInputRole::Relative => 1,
        VirtualInputRole::TabletPen => 2,
    }
}

/// What a session does after one observation ([`JoinedInputGuard`]).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct JoinedInputStep {
    /// Release every held key, button and pen state before injecting again:
    /// keyboard and pointer input resumes after a lapse, and the releases
    /// now reach the joined desktop alone.
    pub release_held: bool,
    /// Keyboard or pointer input stopped now, and why.
    pub lapsed: Option<(VirtualInputRole, JoinedDeviceGap)>,
    /// The pen was withdrawn now, and why; the Deck is told.
    pub pen_withdrawn: Option<JoinedDeviceGap>,
    /// Keyboard or pointer input stayed unproved past
    /// [`JOINED_INPUT_GRACE`]: the attachment ends, and why.
    pub end: Option<JoinedDeviceGap>,
}

/// Why a joined console's input was not admitted at the start: an
/// essential device is not proved to reach the joined desktop alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinedInputRefusal {
    pub role: VirtualInputRole,
    pub gap: JoinedDeviceGap,
}

impl JoinedInputRefusal {
    /// The reason a Deck is shown when its attachment is refused or ended
    /// for this.
    #[must_use]
    pub fn deck_reason(&self) -> String {
        deck_reason(&self.gap)
    }
}

/// Longest reason a Deck is shown when its attachment ends: it rides in a
/// close frame (125-byte control payload, 2 for the code).
pub const MAX_END_REASON_BYTES: usize = 120;

/// The reason a Deck is shown when keyboard and pointer input cannot be
/// kept to the joined desktop: what reads Arcen's devices and what the
/// host's administrator does about it (at most [`MAX_END_REASON_BYTES`]).
#[must_use]
pub fn deck_reason(gap: &JoinedDeviceGap) -> String {
    let text = match gap {
        JoinedDeviceGap::ForeignReader { reader, .. } => match reader {
            DeviceNodeReader::OtherSession {
                pid, session, comm, ..
            } => format!(
                "session input stopped: {comm} (pid {pid}) in host session {session} reads Arcen's \
                 input; sign it out"
            ),
            DeviceNodeReader::OtherDisplayServer { pid, comm } => format!(
                "session input stopped: display server {comm} (pid {pid}) on the host reads \
                 Arcen's input; stop it"
            ),
            DeviceNodeReader::OtherAccount { pid, uid, comm } => format!(
                "session input stopped: {comm} (pid {pid}, uid {uid}) on the host reads Arcen's \
                 input; stop it"
            ),
            DeviceNodeReader::InputBridge { pid, comm } => format!(
                "session input stopped: {comm} (pid {pid}) resends Arcen's input to every session; \
                 exclude device a2ce:*"
            ),
            DeviceNodeReader::Unattributable {
                pid,
                comm,
                why: Unattributable::Namespaced,
            } => format!(
                "session input stopped: {comm} (pid {pid}) reads Arcen's input from its own \
                 namespace; stop it"
            ),
            DeviceNodeReader::Unattributable { pid, comm, .. } => format!(
                "session input stopped: the host cannot inspect {comm} (pid {pid}), which reads \
                 Arcen's input; stop it"
            ),
            _ => {
                "session input stopped: another program on the host reads Arcen's input".to_owned()
            }
        },
        JoinedDeviceGap::UnknownHandler { node } => format!(
            "session input stopped: an unexpected kernel handler ({node}) took Arcen's input device"
        ),
        JoinedDeviceGap::NoDevice | JoinedDeviceGap::NoNodes => {
            "session input stopped: Arcen's input devices could not be read on the host".to_owned()
        }
        JoinedDeviceGap::NoJoinedServer
        | JoinedDeviceGap::NotInFront
        | JoinedDeviceGap::NotAdmitted => "session input stopped: this desktop left the host's \
                                           monitors or no longer takes Arcen's input"
            .to_owned(),
    };
    bounded(text, MAX_END_REASON_BYTES)
}

fn bounded(mut text: String, limit: usize) -> String {
    if text.len() > limit {
        let mut cut = limit;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
    }
    text
}

/// What a Deck whose typed pen was withdrawn (`gap`) is told, from the
/// tablet-mode result it was sent (`result`): Tablet support stops and the
/// pen works as a mouse, the session goes on. `None` when the Deck was not
/// in Tablet support (Native tablet ends through its own attachment).
#[must_use]
pub fn pen_withdrawn_result(
    result: &TabletModeResultMsg,
    gap: &JoinedDeviceGap,
) -> Option<TabletModeResultMsg> {
    if result.active != TabletModeMsg::LocalTermination || !result.accepted {
        return None;
    }
    let why = match gap {
        JoinedDeviceGap::ForeignReader { reader, .. } => {
            format!("{reader} on the host also reads Arcen's pen device")
        }
        gap if gap.reaches_another_reader() => {
            "another program on the host also reads Arcen's pen device".to_owned()
        }
        _ => {
            "this desktop left the host's monitors or no longer takes Arcen's pen device".to_owned()
        }
    };
    Some(TabletModeResultMsg {
        active: TabletModeMsg::DisabledMouseCompat,
        accepted: false,
        reconnect_required: false,
        reason: TabletModeReason::try_from(format!(
            "Tablet support stopped: {why}. The pen works as a mouse."
        ))
        .unwrap_or_default(),
        ..result.clone()
    })
}

/// The admission of a joined console's typed input devices over one
/// attachment (see the module): which devices may be injected into now, and
/// what to do when a proof lapses or holds again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinedInputGuard {
    present: [bool; 3],
    admitted: [bool; 3],
    pen_withdrawn: bool,
    lapsed_since: Option<Instant>,
    ended: bool,
}

impl JoinedInputGuard {
    /// Admits a session's devices from their first proofs. Every essential
    /// device must be proved; a pen that is not is withdrawn from the start
    /// (the session goes on without one).
    ///
    /// # Errors
    ///
    /// The first essential device that is not proved: the attachment is
    /// refused rather than injecting into a device someone else could read.
    pub fn start(
        proofs: &[(VirtualInputRole, Result<(), JoinedDeviceGap>)],
    ) -> Result<(Self, Option<JoinedDeviceGap>), JoinedInputRefusal> {
        let mut guard = Self {
            present: [false; 3],
            admitted: [false; 3],
            pen_withdrawn: false,
            lapsed_since: None,
            ended: false,
        };
        let mut pen_gap = None;
        for (role, proof) in proofs {
            guard.present[slot(*role)] = true;
            match proof {
                Ok(()) => guard.admitted[slot(*role)] = true,
                Err(gap) if essential(*role) => {
                    return Err(JoinedInputRefusal {
                        role: *role,
                        gap: gap.clone(),
                    });
                }
                Err(gap) => {
                    guard.pen_withdrawn = true;
                    pen_gap = Some(gap.clone());
                }
            }
        }
        if !essential_present(guard.present) {
            return Err(JoinedInputRefusal {
                role: VirtualInputRole::Absolute,
                gap: JoinedDeviceGap::NoDevice,
            });
        }
        Ok((guard, pen_gap))
    }

    /// Whether input for `role` may be injected now.
    #[must_use]
    pub const fn admitted(&self, role: VirtualInputRole) -> bool {
        !self.ended && self.admitted[slot(role)]
    }

    /// Whether the attachment has ended for its input.
    #[must_use]
    pub const fn ended(&self) -> bool {
        self.ended
    }

    /// One full recheck: a proof for every device the session has.
    pub fn observe(
        &mut self,
        proofs: &[(VirtualInputRole, Result<(), JoinedDeviceGap>)],
        now: Instant,
    ) -> JoinedInputStep {
        let mut step = JoinedInputStep::default();
        if self.ended {
            return step;
        }
        let mut essential_gap: Option<(VirtualInputRole, JoinedDeviceGap)> = None;
        for (role, proof) in proofs {
            let index = slot(*role);
            if !self.present[index] {
                continue;
            }
            if essential(*role) {
                self.admitted[index] = proof.is_ok();
                if let (Err(gap), None) = (proof, essential_gap.as_ref()) {
                    essential_gap = Some((*role, gap.clone()));
                }
            } else if let Err(gap) = proof {
                if !self.pen_withdrawn {
                    self.pen_withdrawn = true;
                    step.pen_withdrawn = Some(gap.clone());
                }
                self.admitted[index] = false;
            } else {
                self.admitted[index] = !self.pen_withdrawn;
            }
        }
        match essential_gap {
            None => {
                if self.lapsed_since.take().is_some() {
                    step.release_held = true;
                }
            }
            Some((role, gap)) => {
                let since = *self.lapsed_since.get_or_insert_with(|| {
                    step.lapsed = Some((role, gap.clone()));
                    now
                });
                if now.saturating_duration_since(since) >= JOINED_INPUT_GRACE {
                    self.ended = true;
                    self.admitted = [false; 3];
                    step.end = Some(gap);
                }
            }
        }
        step
    }

    /// A quick check between full proofs: whether the joined session is
    /// still in front. Leaving the front lapses every device at once; being
    /// in front proves nothing on its own (the next full proof decides).
    pub fn observe_in_front(&mut self, in_front: bool, now: Instant) -> JoinedInputStep {
        if in_front || self.ended {
            return JoinedInputStep::default();
        }
        let proofs: Vec<_> = [
            VirtualInputRole::Absolute,
            VirtualInputRole::Relative,
            VirtualInputRole::TabletPen,
        ]
        .into_iter()
        .filter(|role| self.present[slot(*role)])
        .map(|role| (role, Err(JoinedDeviceGap::NotInFront)))
        .collect();
        self.observe(&proofs, now)
    }
}

const fn essential_present(present: [bool; 3]) -> bool {
    present[slot(VirtualInputRole::Absolute)]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn confined() -> JoinedDeviceFacts {
        JoinedDeviceFacts {
            device: true,
            joined_server: true,
            joined_in_front: true,
            nodes: vec![
                DeviceNodeFacts::new(
                    "event17",
                    vec![
                        DeviceNodeReader::InputBroker,
                        DeviceNodeReader::JoinedDisplayServer,
                    ],
                ),
                DeviceNodeFacts::new("mouse4", Vec::new()),
            ],
            multiplexed_mouse_readers: Vec::new(),
        }
    }

    #[test]
    fn node_names_name_their_handler() {
        assert_eq!(
            DeviceNodeHandler::from_node_name("event0"),
            DeviceNodeHandler::Event
        );
        assert_eq!(
            DeviceNodeHandler::from_node_name("mouse12"),
            DeviceNodeHandler::Mouse
        );
        assert_eq!(
            DeviceNodeHandler::from_node_name("js0"),
            DeviceNodeHandler::Joystick
        );
        for other in ["ts0", "js", "event", "mouse", "event1a", "input5", "mice"] {
            assert_eq!(
                DeviceNodeHandler::from_node_name(other),
                DeviceNodeHandler::Other,
                "{other}"
            );
        }
    }

    #[test]
    fn a_pen_device_only_the_joined_server_and_the_broker_hold_is_confined() {
        assert_eq!(prove_joined_device_confinement(&confined()), Ok(()));
        // The broker alone, or nobody, is not the desktop admitting it.
        let mut facts = confined();
        facts.nodes[0].readers = vec![DeviceNodeReader::InputBroker];
        assert_eq!(
            prove_joined_device_confinement(&facts),
            Err(JoinedDeviceGap::NotAdmitted)
        );
        assert!(JoinedDeviceGap::NotAdmitted.may_settle());
    }

    #[test]
    fn another_display_server_or_process_on_any_node_is_a_foreign_reader() {
        // A login screen in the background holds the node: brought forward,
        // it would read the pen.
        let mut greeter = confined();
        greeter.nodes[0]
            .readers
            .push(DeviceNodeReader::OtherDisplayServer {
                pid: 900,
                comm: "Xorg".to_owned(),
            });
        let gap = prove_joined_device_confinement(&greeter).unwrap_err();
        assert_eq!(
            gap,
            JoinedDeviceGap::ForeignReader {
                node: "event17".to_owned(),
                reader: DeviceNodeReader::OtherDisplayServer {
                    pid: 900,
                    comm: "Xorg".to_owned(),
                },
            }
        );
        assert_eq!(gap.pen_refusal(), JoinedPenRefusal::NotConfined);
        assert!(gap.reaches_another_reader());
        assert!(!gap.may_settle());
        let mut reader = confined();
        reader.nodes[1].readers = vec![DeviceNodeReader::OtherAccount {
            pid: 31,
            uid: 1001,
            comm: "evtest".to_owned(),
        }];
        assert!(matches!(
            prove_joined_device_confinement(&reader),
            Err(JoinedDeviceGap::ForeignReader { node, .. }) if node == "mouse4"
        ));
        // The multiplexed mouse a mousedev node feeds counts too.
        let mut mice = confined();
        mice.multiplexed_mouse_readers = vec![DeviceNodeReader::InputBridge {
            pid: 7,
            comm: "keyd".to_owned(),
        }];
        assert!(matches!(
            prove_joined_device_confinement(&mice),
            Err(JoinedDeviceGap::ForeignReader { node, .. }) if node == "mice"
        ));
        // ...only when the device has a mousedev node.
        mice.nodes.pop();
        assert_eq!(prove_joined_device_confinement(&mice), Ok(()));
        // Foreign readers are reported before readiness.
        greeter.joined_in_front = false;
        assert!(matches!(
            prove_joined_device_confinement(&greeter),
            Err(JoinedDeviceGap::ForeignReader { .. })
        ));
    }

    #[test]
    fn no_device_no_nodes_an_unknown_handler_or_an_unready_desktop_is_refused() {
        let mut facts = confined();
        facts.device = false;
        assert_eq!(
            prove_joined_device_confinement(&facts),
            Err(JoinedDeviceGap::NoDevice)
        );
        let mut facts = confined();
        facts.nodes.clear();
        assert_eq!(
            prove_joined_device_confinement(&facts),
            Err(JoinedDeviceGap::NoNodes)
        );
        let mut facts = confined();
        facts.nodes.push(DeviceNodeFacts::new("ts0", Vec::new()));
        assert_eq!(
            prove_joined_device_confinement(&facts),
            Err(JoinedDeviceGap::UnknownHandler {
                node: "ts0".to_owned()
            })
        );
        let mut facts = confined();
        facts.joined_server = false;
        assert_eq!(
            prove_joined_device_confinement(&facts),
            Err(JoinedDeviceGap::NoJoinedServer)
        );
        let mut facts = confined();
        facts.joined_in_front = false;
        assert_eq!(
            prove_joined_device_confinement(&facts),
            Err(JoinedDeviceGap::NotInFront)
        );
        assert_eq!(
            JoinedDeviceGap::NotInFront.pen_refusal(),
            JoinedPenRefusal::NotReady
        );
        assert!(!JoinedDeviceGap::NotInFront.reaches_another_reader());
    }

    fn joined() -> JoinedIdentity {
        JoinedIdentity {
            session: "35175".to_owned(),
            uid: 88_601_122,
        }
    }

    fn reader(pid: u32, comm: &str, uid: u32, session: Option<&str>) -> ReaderFacts {
        ReaderFacts {
            pid,
            comm: comm.to_owned(),
            uid: Some(uid),
            session: session.map(str::to_owned),
            user_manager: None,
            readable: true,
            host_namespaces: true,
            joined_server: false,
        }
    }

    /// System daemons that open every input device are system services:
    /// allowed and reported, never a reason to refuse.
    #[test]
    fn system_daemons_reading_every_device_are_allowed_and_reported() {
        for (comm, uid) in [
            ("acpid", 0),
            ("thd", 0),
            ("gpm", 0),
            ("brltty", 0),
            ("brltty", 985),
            ("upowerd", 0),
            ("iio-sensor-pro", 993),
            ("dynamic-svc", 61_200),
            ("nobody-svc", 65_534),
        ] {
            let classified = classify_reader(&reader(700, comm, uid, None), &joined());
            assert_eq!(
                classified,
                DeviceNodeReader::SystemService {
                    pid: 700,
                    uid,
                    comm: comm.to_owned()
                },
                "{comm}"
            );
            assert!(classified.admitted() && classified.reported(), "{comm}");
            assert_eq!(
                classified.report_label(),
                Some(format!("{comm} (system service, uid {uid})"))
            );
        }
        // The joined session's own processes and the joined account's
        // services are its own.
        let own = classify_reader(
            &reader(800, "gsd-media-keys", 88_601_122, Some("35175")),
            &joined(),
        );
        assert!(matches!(
            own,
            DeviceNodeReader::JoinedSession { pid: 800, .. }
        ));
        let service = classify_reader(
            &ReaderFacts {
                user_manager: Some(88_601_122),
                ..reader(801, "input-helper", 88_601_122, None)
            },
            &joined(),
        );
        assert!(matches!(
            service,
            DeviceNodeReader::JoinedAccount { pid: 801, .. }
        ));
        assert!(own.admitted() && service.admitted());
        // Even root inside the joined session.
        let root = classify_reader(&reader(802, "gdm-x-session", 0, Some("35175")), &joined());
        assert!(root.admitted());
        // The broker and the joined server.
        assert_eq!(
            classify_reader(&reader(1860, "systemd-logind", 0, None), &joined()),
            DeviceNodeReader::InputBroker
        );
        assert_eq!(
            classify_reader(&reader(1, "systemd", 0, None), &joined()),
            DeviceNodeReader::InputBroker
        );
        let server = ReaderFacts {
            joined_server: true,
            ..reader(2_987_884, "Xorg", 88_601_122, Some("35175"))
        };
        assert_eq!(
            classify_reader(&server, &joined()),
            DeviceNodeReader::JoinedDisplayServer
        );
        // A device read by them all is proved.
        let facts = JoinedDeviceFacts {
            device: true,
            joined_server: true,
            joined_in_front: true,
            nodes: vec![DeviceNodeFacts::new(
                "event6",
                ["acpid", "thd", "gpm", "brltty"]
                    .into_iter()
                    .map(|comm| classify_reader(&reader(9, comm, 0, None), &joined()))
                    .chain([DeviceNodeReader::JoinedDisplayServer])
                    .collect(),
            )],
            multiplexed_mouse_readers: vec![classify_reader(&reader(9, "gpm", 0, None), &joined())],
        };
        assert_eq!(prove_joined_device_confinement(&facts), Ok(()));
    }

    /// Another account's session, a login screen, another account's process
    /// outside any session, an input bridge and an unattributable process
    /// are foreign, and the Deck is told which and what to do.
    #[test]
    fn other_sessions_display_servers_and_bridges_are_foreign_and_named() {
        let second_user = classify_reader(&reader(4100, "evtest", 1001, Some("7")), &joined());
        assert_eq!(
            second_user,
            DeviceNodeReader::OtherSession {
                pid: 4100,
                uid: Some(1001),
                session: "7".to_owned(),
                comm: "evtest".to_owned()
            }
        );
        // The same account in another session is another session too.
        let same_account =
            classify_reader(&reader(4101, "evtest", 88_601_122, Some("9")), &joined());
        assert!(matches!(
            same_account,
            DeviceNodeReader::OtherSession { .. }
        ));
        // A login screen: its compositor and its helpers.
        let greeter = classify_reader(&reader(912, "gnome-shell", 42, Some("c1")), &joined());
        assert!(matches!(
            greeter,
            DeviceNodeReader::OtherDisplayServer { pid: 912, .. }
        ));
        let greeter_helper =
            classify_reader(&reader(913, "gsd-media-keys", 42, Some("c1")), &joined());
        assert!(matches!(
            greeter_helper,
            DeviceNodeReader::OtherSession { .. }
        ));
        // A root process of another session (a greeter's worker).
        let worker = classify_reader(&reader(914, "gdm-session-wor", 0, Some("c1")), &joined());
        assert!(matches!(worker, DeviceNodeReader::OtherSession { .. }));
        // A login screen's helpers under its account's own service manager
        // are that login's, not system services, though the account is a
        // system account.
        let greeter_service = classify_reader(
            &ReaderFacts {
                user_manager: Some(42),
                ..reader(915, "gsd-media-keys", 42, None)
            },
            &joined(),
        );
        assert_eq!(
            greeter_service,
            DeviceNodeReader::OtherAccount {
                pid: 915,
                uid: 42,
                comm: "gsd-media-keys".to_owned()
            }
        );
        // Whatever uid it runs as under another account's manager.
        let setuid = classify_reader(
            &ReaderFacts {
                user_manager: Some(1001),
                ..reader(916, "evtest", 0, None)
            },
            &joined(),
        );
        assert!(!setuid.admitted());
        let other_account = classify_reader(&reader(4200, "evtest", 1001, None), &joined());
        // A domain account's job outside a session (cron), whatever its uid.
        for uid in [60_000, 65_520, 65_535, 200_000] {
            assert!(
                !classify_reader(&reader(4201, "job", uid, None), &joined()).admitted(),
                "{uid}"
            );
        }
        assert!(matches!(
            other_account,
            DeviceNodeReader::OtherAccount { .. }
        ));
        let bridge = classify_reader(&reader(600, "keyd", 0, None), &joined());
        assert!(matches!(bridge, DeviceNodeReader::InputBridge { .. }));
        let hidden = classify_reader(
            &ReaderFacts {
                readable: false,
                ..reader(610, "unknown", 0, None)
            },
            &joined(),
        );
        assert!(matches!(
            hidden,
            DeviceNodeReader::Unattributable {
                why: Unattributable::Unreadable,
                ..
            }
        ));
        let namespaced = classify_reader(
            &ReaderFacts {
                host_namespaces: false,
                ..reader(620, "Xorg", 1001, None)
            },
            &joined(),
        );
        assert!(matches!(
            namespaced,
            DeviceNodeReader::Unattributable {
                why: Unattributable::Namespaced,
                ..
            }
        ));
        for (reader, expected) in [
            (second_user, "evtest (pid 4100) in host session 7"),
            (greeter, "display server gnome-shell (pid 912)"),
            (other_account, "evtest (pid 4200, uid 1001)"),
            (bridge, "keyd (pid 600) resends"),
            (hidden, "cannot inspect unknown (pid 610)"),
            (
                namespaced,
                "Xorg (pid 620) reads Arcen's input from its own namespace",
            ),
        ] {
            assert!(!reader.admitted(), "{reader:?}");
            let reason = deck_reason(&JoinedDeviceGap::ForeignReader {
                node: "event6".to_owned(),
                reader,
            });
            assert!(reason.contains(expected), "{reason}");
            assert!(reason.len() <= MAX_END_REASON_BYTES, "{reason}");
        }
        // The longest names still fit, with what to do.
        let long = deck_reason(&JoinedDeviceGap::ForeignReader {
            node: "event6".to_owned(),
            reader: DeviceNodeReader::OtherSession {
                pid: 4_194_304,
                uid: Some(4_294_967_294),
                session: "c4294967295".to_owned(),
                comm: "abcdefghijklmno".to_owned(),
            },
        });
        assert!(long.len() <= MAX_END_REASON_BYTES, "{long}");
        assert!(long.ends_with("sign it out"), "{long}");
        // A process names itself: control, format and bidirectional
        // characters are not shown.
        let spoofed = classify_reader(
            &reader(
                4301,
                "evil\u{202e}gpj\u{200b}x\u{2066}",
                1001,
                Some("7\u{061c}"),
            ),
            &joined(),
        );
        assert_eq!(
            spoofed,
            DeviceNodeReader::OtherSession {
                pid: 4301,
                uid: Some(1001),
                session: "7?".to_owned(),
                comm: "evil?gpj?x?".to_owned()
            }
        );
        let named = classify_reader(
            &reader(4300, "ev\ntest\u{1b}[2J-and-more", 1001, Some("7")),
            &joined(),
        );
        assert_eq!(
            named,
            DeviceNodeReader::OtherSession {
                pid: 4300,
                uid: Some(1001),
                session: "7".to_owned(),
                comm: "ev?test?[2J-and".to_owned()
            }
        );
    }

    fn proofs(
        absolute: Result<(), JoinedDeviceGap>,
        relative: Result<(), JoinedDeviceGap>,
        pen: Option<Result<(), JoinedDeviceGap>>,
    ) -> Vec<(VirtualInputRole, Result<(), JoinedDeviceGap>)> {
        let mut proofs = vec![
            (VirtualInputRole::Absolute, absolute),
            (VirtualInputRole::Relative, relative),
        ];
        if let Some(pen) = pen {
            proofs.push((VirtualInputRole::TabletPen, pen));
        }
        proofs
    }

    fn foreign() -> JoinedDeviceGap {
        JoinedDeviceGap::ForeignReader {
            node: "event5".to_owned(),
            reader: DeviceNodeReader::OtherDisplayServer {
                pid: 900,
                comm: "Xorg".to_owned(),
            },
        }
    }

    /// The normal case: every device proved, everything admitted, nothing
    /// to do on later proofs.
    #[test]
    fn proved_devices_are_admitted_and_stay_so() {
        let (mut guard, pen_gap) =
            JoinedInputGuard::start(&proofs(Ok(()), Ok(()), Some(Ok(())))).unwrap();
        assert_eq!(pen_gap, None);
        for role in VirtualInputRole::ALL {
            assert!(guard.admitted(role), "{role:?}");
        }
        let now = Instant::now();
        assert_eq!(
            guard.observe(&proofs(Ok(()), Ok(()), Some(Ok(()))), now),
            JoinedInputStep::default()
        );
        assert_eq!(
            guard.observe_in_front(true, now),
            JoinedInputStep::default()
        );
        assert!(guard.admitted(VirtualInputRole::Absolute));
    }

    /// An essential device not proved at the start refuses the attachment;
    /// an unproved pen is only withdrawn.
    #[test]
    fn an_unproved_keyboard_refuses_the_start_and_an_unproved_pen_is_withdrawn() {
        let refusal = JoinedInputGuard::start(&proofs(Err(foreign()), Ok(()), None)).unwrap_err();
        assert_eq!(refusal.role, VirtualInputRole::Absolute);
        assert!(
            refusal
                .deck_reason()
                .contains("display server Xorg (pid 900) on the host reads Arcen's input; stop it"),
            "{}",
            refusal.deck_reason()
        );
        let refusal =
            JoinedInputGuard::start(&proofs(Ok(()), Err(JoinedDeviceGap::NotAdmitted), None))
                .unwrap_err();
        assert_eq!(refusal.role, VirtualInputRole::Relative);
        assert!(refusal.deck_reason().contains("left the host's monitors"));
        assert!(JoinedInputGuard::start(&[]).is_err(), "no keyboard at all");
        let (guard, pen_gap) =
            JoinedInputGuard::start(&proofs(Ok(()), Ok(()), Some(Err(foreign())))).unwrap();
        assert_eq!(pen_gap, Some(foreign()));
        assert!(guard.admitted(VirtualInputRole::Absolute));
        assert!(!guard.admitted(VirtualInputRole::TabletPen));
    }

    /// A lapse stops keyboard and pointer input at once; proved again within
    /// the grace, held input is released first and input resumes.
    #[test]
    fn a_short_lapse_stops_input_then_releases_held_input_and_resumes() {
        let (mut guard, _) = JoinedInputGuard::start(&proofs(Ok(()), Ok(()), None)).unwrap();
        let start = Instant::now();
        let step = guard.observe(&proofs(Err(foreign()), Ok(()), None), start);
        assert_eq!(step.lapsed, Some((VirtualInputRole::Absolute, foreign())));
        assert_eq!(step.end, None);
        assert!(!guard.admitted(VirtualInputRole::Absolute));
        assert!(guard.admitted(VirtualInputRole::Relative));
        // Still lapsed: no new lapse, no end yet.
        let step = guard.observe(
            &proofs(Err(foreign()), Ok(()), None),
            start + Duration::from_secs(2),
        );
        assert_eq!(step, JoinedInputStep::default());
        let step = guard.observe(
            &proofs(Ok(()), Ok(()), None),
            start + Duration::from_secs(4),
        );
        assert!(step.release_held);
        assert!(guard.admitted(VirtualInputRole::Absolute));
        assert!(!guard.ended());
    }

    /// Unproved past the grace, the attachment ends and nothing is admitted
    /// again.
    #[test]
    fn a_lapse_past_the_grace_ends_the_attachment_for_good() {
        let (mut guard, _) =
            JoinedInputGuard::start(&proofs(Ok(()), Ok(()), Some(Ok(())))).unwrap();
        let start = Instant::now();
        let step = guard.observe_in_front(false, start);
        assert_eq!(
            step.lapsed,
            Some((VirtualInputRole::Absolute, JoinedDeviceGap::NotInFront))
        );
        assert_eq!(step.pen_withdrawn, Some(JoinedDeviceGap::NotInFront));
        for role in VirtualInputRole::ALL {
            assert!(!guard.admitted(role), "{role:?}");
        }
        let step = guard.observe_in_front(false, start + JOINED_INPUT_GRACE);
        assert_eq!(step.end, Some(JoinedDeviceGap::NotInFront));
        assert!(guard.ended());
        assert_eq!(
            guard.observe(
                &proofs(Ok(()), Ok(()), Some(Ok(()))),
                start + JOINED_INPUT_GRACE * 2
            ),
            JoinedInputStep::default()
        );
        assert!(!guard.admitted(VirtualInputRole::Absolute));
    }

    /// Every end reason fits a close frame; a Deck in Tablet support is told
    /// its pen became a mouse in words its consistency check accepts, and a
    /// Deck in another mode gets nothing from this.
    #[test]
    fn the_deck_is_told_in_words_that_fit_and_that_it_accepts() {
        for gap in [
            foreign(),
            JoinedDeviceGap::NotInFront,
            JoinedDeviceGap::NotAdmitted,
        ] {
            assert!(deck_reason(&gap).len() <= MAX_END_REASON_BYTES, "{gap:?}");
        }
        let typed = TabletModeResultMsg {
            requested: TabletModeMsg::LocalTermination,
            active: TabletModeMsg::LocalTermination,
            accepted: true,
            ..TabletModeResultMsg::default()
        };
        let withdrawn = pen_withdrawn_result(&typed, &foreign()).unwrap();
        assert_eq!(withdrawn.requested, TabletModeMsg::LocalTermination);
        assert_eq!(withdrawn.active, TabletModeMsg::DisabledMouseCompat);
        assert!(!withdrawn.accepted && !withdrawn.reconnect_required);
        assert_eq!(
            String::from(withdrawn.reason),
            "Tablet support stopped: display server Xorg (pid 900) on the host also reads \
             Arcen's pen device. The pen works as a mouse."
        );
        let native = TabletModeResultMsg {
            requested: TabletModeMsg::UsbTabletBridge,
            active: TabletModeMsg::UsbTabletBridge,
            accepted: true,
            ..TabletModeResultMsg::default()
        };
        assert_eq!(pen_withdrawn_result(&native, &foreign()), None);
        let refused = TabletModeResultMsg {
            active: TabletModeMsg::DisabledMouseCompat,
            accepted: false,
            ..typed
        };
        assert_eq!(pen_withdrawn_result(&refused, &foreign()), None);
    }

    /// The pen's lapse withdraws it for the rest of the attachment and
    /// never ends the session.
    #[test]
    fn a_pen_lapse_withdraws_the_pen_for_good_and_the_session_goes_on() {
        let (mut guard, _) =
            JoinedInputGuard::start(&proofs(Ok(()), Ok(()), Some(Ok(())))).unwrap();
        let start = Instant::now();
        let step = guard.observe(&proofs(Ok(()), Ok(()), Some(Err(foreign()))), start);
        assert_eq!(step.pen_withdrawn, Some(foreign()));
        assert_eq!(step.end, None);
        assert_eq!(step.lapsed, None);
        assert!(!guard.admitted(VirtualInputRole::TabletPen));
        let step = guard.observe(
            &proofs(Ok(()), Ok(()), Some(Ok(()))),
            start + JOINED_INPUT_GRACE * 3,
        );
        assert_eq!(step, JoinedInputStep::default());
        assert!(
            !guard.admitted(VirtualInputRole::TabletPen),
            "withdrawn for good"
        );
        assert!(guard.admitted(VirtualInputRole::Absolute));
        assert!(!essential(VirtualInputRole::TabletPen));
    }
}
