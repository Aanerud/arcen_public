// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The Linux facts behind the shared confinement proof for Arcen's typed
//! input devices on a joined Xorg console (`arcen_input::joined_input`):
//! each device's kernel nodes (from its sysfs entry), who has each open
//! (every process's descriptors under `/proc`), which process is the joined
//! desktop's X server (the one listening on its `/tmp/.X11-unix` socket),
//! and whether the joined session is in front of `seat0` (logind's seat
//! record). The decisions are the shared crate's; this module reads and
//! classifies only.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use arcen_input::joined_input::{
    classify_reader, prove_joined_device_confinement, DeviceNodeFacts, DeviceNodeReader,
    JoinedDeviceFacts, JoinedDeviceGap, JoinedIdentity, ReaderFacts,
};
use arcen_input::virtual_device::VirtualInputRole;

/// The multiplexed mouse (`/dev/input/mice`) every `mousedev` node feeds.
const MULTIPLEXED_MOUSE: (u64, u64) = (13, 63);
/// `/dev/psaux` (misc 10:1), the same multiplexed stream on kernels built
/// with `CONFIG_INPUT_MOUSEDEV_PSAUX` (`mousedev_open` maps the misc major
/// to the mixed device).
const PSAUX_MOUSE: (u64, u64) = (10, 1);
/// Display servers and compositors a reader is named for in the gap.
/// logind's seat record for `seat0`.
const SEAT0_RECORD: &str = "/run/systemd/seats/seat0";
/// How often a proof that waits for the X server to add the device retries.
const SETTLE_STEP: Duration = Duration::from_millis(100);

/// Which typed devices' input a joined console admits now, written by the
/// attachment's guard (`arcen_input::joined_input::JoinedInputGuard`) and
/// read on every injection. Every device starts shut.
#[derive(Debug, Default)]
pub struct JoinedInputGate {
    admitted: [AtomicBool; 3],
    dropped: AtomicU64,
    /// The joined session's virtual terminal, when it has one: every
    /// injection also checks it is in front, so a terminal switch shuts
    /// input before anything is written, not at the next poll.
    joined_vt: Option<u32>,
}

const fn gate_slot(role: VirtualInputRole) -> usize {
    match role {
        VirtualInputRole::Absolute => 0,
        VirtualInputRole::Relative => 1,
        VirtualInputRole::TabletPen => 2,
    }
}

impl JoinedInputGate {
    /// A gate, every device shut, that also requires `joined_vt` (when
    /// known) to be in front for every injection.
    #[must_use]
    pub fn new(joined_vt: Option<u32>) -> Self {
        Self {
            joined_vt,
            ..Self::default()
        }
    }

    /// Whether `role`'s input may be injected at this instant: admitted by
    /// the guard, and the joined session's terminal in front.
    #[must_use]
    pub fn admits(&self, role: VirtualInputRole) -> bool {
        self.admitted(role) && self.joined_vt.is_none_or(vt_in_front)
    }

    /// Shuts every device (the watch ended).
    pub fn shut_all(&self) {
        for role in VirtualInputRole::ALL {
            self.set(role, false);
        }
    }

    /// Opens or shuts `role`'s input.
    pub fn set(&self, role: VirtualInputRole, admitted: bool) {
        self.admitted[gate_slot(role)].store(admitted, Ordering::Release);
    }

    /// Whether `role`'s input may be injected now.
    #[must_use]
    pub fn admitted(&self, role: VirtualInputRole) -> bool {
        self.admitted[gate_slot(role)].load(Ordering::Acquire)
    }

    /// Counts one injection refused while shut.
    pub fn count_drop(&self) {
        self.dropped.fetch_add(1, Ordering::Relaxed);
    }

    /// Injections refused while shut, so far.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Mirrors what `guard` admits.
    pub fn follow(&self, guard: &arcen_input::joined_input::JoinedInputGuard) {
        for role in VirtualInputRole::ALL {
            self.set(role, guard.admitted(role));
        }
    }
}

/// One kernel node of a typed input device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceNode {
    /// The node's kernel name (`event12`).
    pub name: String,
    /// Its device number (major, minor).
    pub device: (u64, u64),
}

/// The joined desktop the typed devices must reach alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinedDesktop {
    /// Its X display number.
    pub display: u16,
    /// Its logind session.
    pub session_id: String,
    /// Its account.
    pub uid: u32,
}

/// The kernel nodes of the input device at `syspath`
/// (`/sys/devices/virtual/input/inputN`): every child with a device number.
///
/// # Errors
///
/// When the directory cannot be read.
pub fn device_nodes(syspath: &Path) -> io::Result<Vec<DeviceNode>> {
    let mut nodes = Vec::new();
    for entry in fs::read_dir(syspath)? {
        let entry = entry?;
        let Ok(dev) = fs::read_to_string(entry.path().join("dev")) else {
            continue;
        };
        let Some(device) = parse_device_number(&dev) else {
            continue;
        };
        nodes.push(DeviceNode {
            name: entry.file_name().to_string_lossy().into_owned(),
            device,
        });
    }
    nodes.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(nodes)
}

/// `major:minor` from a sysfs `dev` file.
fn parse_device_number(text: &str) -> Option<(u64, u64)> {
    let (major, minor) = text.trim().split_once(':')?;
    Some((major.parse().ok()?, minor.parse().ok()?))
}

/// What one process has open that the proof needs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ProcessFacts {
    pid: u32,
    comm: String,
    uid: Option<u32>,
    /// Character devices it has open.
    devices: Vec<(u64, u64)>,
    /// Socket inodes it has open.
    sockets: Vec<u64>,
    /// Its descriptors could be read.
    readable: bool,
    /// Where its cgroup places it in the login hierarchy.
    login: Login,
    /// It runs in the host's own mount and network namespaces (pid 1's):
    /// only such a process can be the joined desktop's X server. A process
    /// in a namespace of its own (an unprivileged user namespace) can bind
    /// a socket at the same path and take the name `Xorg`.
    host_namespaces: bool,
}

/// The inodes of the sockets listening as X display `display`
/// (`/tmp/.X11-unix/X<n>`, abstract or not), from `/proc/net/unix`.
fn listening_x_sockets(net_unix: &str, display: u16) -> Vec<u64> {
    let path = format!("/tmp/.X11-unix/X{display}");
    let abstract_path = format!("@{path}");
    net_unix
        .lines()
        .skip(1)
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let (flags, inode, name) = (fields.get(3)?, fields.get(6)?, fields.get(7)?);
            let listening = u32::from_str_radix(flags, 16).ok()? & 0x0001_0000 != 0;
            (listening && (*name == path || *name == abstract_path))
                .then(|| inode.parse().ok())
                .flatten()
        })
        .collect()
}

/// Whether logind's seat record (`ACTIVE=<session>`) names `session_id`.
fn session_in_front(seat_record: &str, session_id: &str) -> bool {
    !session_id.is_empty()
        && seat_record
            .lines()
            .any(|line| line.strip_prefix("ACTIVE=") == Some(session_id))
}

/// Classifies every reader of every node, from what each process has open.
fn facts_from(
    processes: &[ProcessFacts],
    x_sockets: &[u64],
    nodes: &[DeviceNode],
    in_front: bool,
    identity: &JoinedIdentity,
) -> JoinedDeviceFacts {
    let servers: Vec<u32> = processes
        .iter()
        .filter(|process| process.host_namespaces)
        .filter(|process| matches!(process.comm.as_str(), "Xorg" | "X"))
        .filter(|process| {
            process
                .sockets
                .iter()
                .any(|inode| x_sockets.contains(inode))
        })
        .map(|process| process.pid)
        .collect();
    // Exactly one X server listens as the joined display; anything else
    // identifies nobody.
    let joined = match servers.as_slice() {
        [pid] => Some(*pid),
        _ => None,
    };
    let classify = |process: &ProcessFacts| {
        classify_reader(
            &ReaderFacts {
                pid: process.pid,
                comm: process.comm.clone(),
                uid: process.uid,
                session: process.login.session.clone(),
                user_manager: process.login.user_manager,
                readable: process.readable,
                host_namespaces: process.host_namespaces,
                joined_server: Some(process.pid) == joined,
            },
            identity,
        )
    };
    let readers_of = |device: (u64, u64)| -> Vec<DeviceNodeReader> {
        processes
            .iter()
            .filter(|process| process.devices.contains(&device))
            .map(classify)
            .collect()
    };
    JoinedDeviceFacts {
        device: true,
        joined_server: joined.is_some(),
        joined_in_front: in_front,
        nodes: nodes
            .iter()
            .map(|node| DeviceNodeFacts::new(&node.name, readers_of(node.device)))
            .collect(),
        multiplexed_mouse_readers: readers_of(MULTIPLEXED_MOUSE)
            .into_iter()
            .chain(readers_of(PSAUX_MOUSE))
            .collect(),
    }
}

/// Every process under `proc` with the character devices in `wanted` and
/// the sockets it has open. A process that ends while it is read is
/// skipped.
fn scan_processes(proc: &Path, wanted: &[(u64, u64)]) -> Vec<ProcessFacts> {
    let Ok(entries) = fs::read_dir(proc) else {
        return Vec::new();
    };
    let namespaces = |pid: &str| {
        ["mnt", "net"].map(|kind| fs::read_link(proc.join(pid).join("ns").join(kind)).ok())
    };
    let host = namespaces("1");
    let mut processes = Vec::new();
    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let path = entry.path();
        let mut process = ProcessFacts {
            pid,
            ..ProcessFacts::default()
        };
        let fds = match fs::read_dir(path.join("fd")) {
            Ok(fds) => fds,
            // Ended while it was read.
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            // A live process whose descriptors cannot be read could hold
            // anything: it counts as holding every node (fail closed).
            Err(_) => {
                process.devices = wanted.to_vec();
                process.comm = read_comm(&path);
                processes.push(process);
                continue;
            }
        };
        process.readable = true;
        for fd in fds.flatten() {
            match descriptor(&fd.path(), wanted) {
                Descriptor::Socket(inode) => process.sockets.push(inode),
                Descriptor::CharDevice(device) => process.devices.push(device),
                Descriptor::Other => {}
            }
        }
        process.comm = read_comm(&path);
        let own = namespaces(&pid.to_string());
        process.host_namespaces = host.iter().all(Option::is_some) && own == host;
        process.login = fs::read_to_string(path.join("cgroup"))
            .map(|cgroup| login_of(&cgroup))
            .unwrap_or_default();
        process.uid = fs::read_to_string(path.join("status"))
            .ok()
            .and_then(|status| {
                status
                    .lines()
                    .find_map(|line| line.strip_prefix("Uid:"))?
                    .split_whitespace()
                    .next()?
                    .parse()
                    .ok()
            });
        processes.push(process);
    }
    processes
}

/// What one open descriptor is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Descriptor {
    /// A socket, by inode.
    Socket(u64),
    /// A stable, non-`O_PATH` open of one of the wanted character devices,
    /// by its device number (`st_rdev`).
    CharDevice((u64, u64)),
    /// Anything else: a file, a pipe, another device, an `O_PATH` handle
    /// (which reads nothing), a descriptor closed or replaced while it was
    /// read, or one whose identity cannot be read.
    Other,
}

/// `O_PATH` in `/proc/<pid>/fdinfo/<n>`'s octal `flags`.
const O_PATH_FLAG: u32 = 0o10_000_000;

/// How often a descriptor whose `fdinfo` and `statx` disagree is read
/// again before it is taken for an unstable handle (not a reader).
const DESCRIPTOR_READS: usize = 3;

/// One `/proc/<pid>/fdinfo/<n>` record: what the open file is, as the
/// kernel prints it in one read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FdInfo {
    flags: u32,
    mnt_id: Option<u64>,
    ino: Option<u64>,
}

fn parse_fdinfo(text: &str) -> Option<FdInfo> {
    let field = |name: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(name))
            .map(str::trim)
    };
    Some(FdInfo {
        flags: u32::from_str_radix(field("flags:")?, 8).ok()?,
        mnt_id: field("mnt_id:").and_then(|id| id.parse().ok()),
        ino: field("ino:").and_then(|ino| ino.parse().ok()),
    })
}

/// Identifies the descriptor at `/proc/<pid>/fd/<n>` by the identity of
/// what it has open, never by the path text of its link (which the
/// process controls, from any mount namespace). It counts as one of the
/// `wanted` devices only when:
///
/// - `statx` through the descriptor (`AT_STATX_DONT_SYNC`: a stalled
///   network mount is answered from the cache) finds a character device
///   whose `st_rdev` is wanted, so the same device bind-mounted,
///   hard-linked or made again with `mknod` elsewhere is the same device,
///   and a file merely named `/dev/input/…` is not; a `statx` failure is
///   no input node;
/// - and the descriptor's `fdinfo`, read before, is the same open file
///   (its inode and mount match the `statx`) and not `O_PATH`. The two
///   lookups are separate: a process that swaps the descriptor between
///   them (an `O_PATH` handle on the node, which needs no read permission,
///   opened, closed or `dup2`'d over in a loop) never matches both, and
///   after [`DESCRIPTOR_READS`] attempts that do not agree it is an
///   unstable handle, not a reader. A real reader holds a stable
///   descriptor. On a kernel without `ino` in `fdinfo` or `STATX_MNT_ID`
///   (older than the supported 5.14) the missing field is not compared, so
///   such a handle may refuse a session there (fail closed), never admit
///   one.
fn descriptor(link: &Path, wanted: &[(u64, u64)]) -> Descriptor {
    use rustix::fs::{statx, AtFlags, StatxFlags, CWD};
    let Ok(target) = fs::read_link(link) else {
        return Descriptor::Other;
    };
    if let Some(inode) = target
        .to_str()
        .and_then(|target| target.strip_prefix("socket:["))
        .and_then(|rest| rest.strip_suffix(']'))
        .and_then(|inode| inode.parse::<u64>().ok())
    {
        return Descriptor::Socket(inode);
    }
    let info_path = match (link.file_name(), link.parent().and_then(Path::parent)) {
        (Some(fd), Some(process)) => process.join("fdinfo").join(fd),
        _ => return Descriptor::Other,
    };
    for _ in 0..DESCRIPTOR_READS {
        let info = fs::read_to_string(&info_path)
            .ok()
            .and_then(|text| parse_fdinfo(&text));
        let Ok(stat) = statx(
            CWD,
            link,
            AtFlags::STATX_DONT_SYNC,
            StatxFlags::TYPE | StatxFlags::INO | StatxFlags::MNT_ID,
        ) else {
            return Descriptor::Other;
        };
        let device = (
            u64::from(stat.stx_rdev_major),
            u64::from(stat.stx_rdev_minor),
        );
        if u32::from(stat.stx_mode) & 0o170_000 != 0o020_000 || !wanted.contains(&device) {
            return Descriptor::Other;
        }
        let Some(info) = info else {
            // Closed or replaced between the two reads: read again.
            continue;
        };
        let same_ino = info
            .ino
            .is_none_or(|ino| stat.stx_mask & StatxFlags::INO.bits() == 0 || ino == stat.stx_ino);
        let same_mount = info.mnt_id.is_none_or(|mnt_id| {
            stat.stx_mask & StatxFlags::MNT_ID.bits() == 0 || mnt_id == stat.stx_mnt_id
        });
        if !(same_ino && same_mount) {
            continue;
        }
        return if info.flags & O_PATH_FLAG != 0 {
            Descriptor::Other
        } else {
            Descriptor::CharDevice(device)
        };
    }
    Descriptor::Other
}

/// Where a process's cgroup places it in the login hierarchy.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Login {
    /// The logind session (`user.slice/user-<uid>.slice/session-<id>.scope`).
    session: Option<String>,
    /// The account whose slice holds it outside a session (its
    /// `user@<uid>.service`, delegated to it).
    user_manager: Option<u32>,
}

/// Places a process from its `/proc/<pid>/cgroup` record (the unified
/// `0::` hierarchy, or systemd's own on a legacy host). Only root creates
/// the `user-<uid>.slice` and `session-<id>.scope` levels; what an account
/// creates in its delegated `user@<uid>.service` stays below it, so a path
/// is read by its first levels only.
fn login_of(cgroup: &str) -> Login {
    let Some(path) = cgroup
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .filter(|path| *path != "/")
        .or_else(|| {
            cgroup
                .lines()
                .find_map(|line| line.split_once(":name=systemd:").map(|(_, path)| path))
        })
    else {
        return Login::default();
    };
    let mut levels = path.trim_start_matches('/').split('/');
    if levels.next() != Some("user.slice") {
        return Login::default();
    }
    let Some(uid) = levels
        .next()
        .and_then(|slice| slice.strip_prefix("user-")?.strip_suffix(".slice"))
        .and_then(|uid| uid.parse::<u32>().ok())
    else {
        return Login::default();
    };
    let session = levels.next().and_then(|scope| {
        let id = scope.strip_prefix("session-")?.strip_suffix(".scope")?;
        (!id.is_empty() && id.bytes().all(|byte| byte.is_ascii_alphanumeric()))
            .then(|| id.to_owned())
    });
    match session {
        Some(session) => Login {
            session: Some(session),
            user_manager: None,
        },
        None => Login {
            session: None,
            user_manager: Some(uid),
        },
    }
}

fn read_comm(process: &Path) -> String {
    let comm = fs::read(process.join("comm")).unwrap_or_default();
    String::from_utf8_lossy(comm.strip_suffix(b"\n").unwrap_or(&comm)).into_owned()
}

/// The account of logind session `session_id`, from its record (tests:
/// production takes it from the session's own metadata).
#[cfg(test)]
pub(crate) fn session_uid(session_id: &str) -> Option<u32> {
    if session_id.is_empty() || !session_id.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
        return None;
    }
    fs::read_to_string(format!("/run/systemd/sessions/{session_id}"))
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("UID="))?
        .trim()
        .parse()
        .ok()
}

/// Where the facts are read from; the system's, or a test tree.
#[derive(Debug, Clone)]
struct Roots {
    proc: PathBuf,
    seat0: PathBuf,
}

impl Roots {
    fn system() -> Self {
        Self {
            proc: PathBuf::from("/proc"),
            seat0: PathBuf::from(SEAT0_RECORD),
        }
    }
}

fn devices_facts_in<K: Copy>(
    roots: &Roots,
    devices: &[(K, Vec<DeviceNode>)],
    joined: &JoinedDesktop,
) -> Vec<(K, JoinedDeviceFacts)> {
    let mut wanted: Vec<(u64, u64)> = devices
        .iter()
        .flat_map(|(_, nodes)| nodes.iter().map(|node| node.device))
        .collect();
    wanted.extend([MULTIPLEXED_MOUSE, PSAUX_MOUSE]);
    let processes = scan_processes(&roots.proc, &wanted);
    let x_sockets = fs::read_to_string(roots.proc.join("net/unix"))
        .map(|text| listening_x_sockets(&text, joined.display))
        .unwrap_or_default();
    let in_front = fs::read_to_string(&roots.seat0)
        .is_ok_and(|record| session_in_front(&record, &joined.session_id));
    let identity = JoinedIdentity {
        session: joined.session_id.clone(),
        uid: joined.uid,
    };
    devices
        .iter()
        .map(|(key, nodes)| {
            (
                *key,
                facts_from(&processes, &x_sockets, nodes, in_front, &identity),
            )
        })
        .collect()
}

/// The virtual terminal logind gave `session_id` (`VTNR=` in its session
/// record); `None` for a session without one or an id that is not plain.
#[must_use]
pub fn session_vt(session_id: &str) -> Option<u32> {
    if session_id.is_empty() || !session_id.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
        return None;
    }
    let record = fs::read_to_string(Path::new("/run/systemd/sessions").join(session_id)).ok()?;
    record
        .lines()
        .find_map(|line| line.strip_prefix("VTNR="))?
        .trim()
        .parse()
        .ok()
        .filter(|vt| *vt > 0)
}

/// The virtual terminal in front (`/sys/class/tty/tty0/active`, `ttyN`).
fn foreground_vt(active: &str) -> Option<u32> {
    active.trim().strip_prefix("tty")?.parse().ok()
}

/// Whether `vt` is the virtual terminal in front now. Fails closed: an
/// unreadable answer is "no".
#[must_use]
pub fn vt_in_front(vt: u32) -> bool {
    fs::read_to_string("/sys/class/tty/tty0/active")
        .ok()
        .and_then(|active| foreground_vt(&active))
        == Some(vt)
}

/// Whether `joined`'s session is the one in front of `seat0` now (one read
/// of logind's seat record).
#[must_use]
pub fn joined_in_front(joined: &JoinedDesktop) -> bool {
    fs::read_to_string(SEAT0_RECORD)
        .is_ok_and(|record| session_in_front(&record, &joined.session_id))
}

/// The facts of the typed device with `nodes` on `joined`'s desktop, as the
/// system shows them now. Blocking: it reads every process's descriptors.
#[must_use]
pub fn device_facts(nodes: &[DeviceNode], joined: &JoinedDesktop) -> JoinedDeviceFacts {
    devices_facts_in(&Roots::system(), &[((), nodes.to_vec())], joined)
        .pop()
        .map(|((), facts)| facts)
        .unwrap_or_default()
}

/// The facts of several typed devices (keyed, with their nodes) from one
/// reading of the system. Blocking.
#[must_use]
pub fn devices_facts<K: Copy>(
    devices: &[(K, Vec<DeviceNode>)],
    joined: &JoinedDesktop,
) -> Vec<(K, JoinedDeviceFacts)> {
    devices_facts_in(&Roots::system(), devices, joined)
}

/// One typed device's proof, read from the system.
#[derive(Debug, Clone)]
pub struct DeviceProof<K> {
    pub key: K,
    pub proof: Result<(), JoinedDeviceGap>,
    /// Who reads which node, for the log.
    pub readers: String,
    /// The allowed readers worth reporting ([`reported_readers`]).
    pub reported: BTreeSet<String>,
}

/// Proves each typed device reaches `joined`'s X server alone, waiting up
/// to `settle` while one is still being added. Blocking.
#[must_use]
pub fn prove_devices<K: Copy>(
    devices: &[(K, Vec<DeviceNode>)],
    joined: &JoinedDesktop,
    settle: Duration,
) -> Vec<DeviceProof<K>> {
    let deadline = Instant::now() + settle;
    loop {
        let proved: Vec<_> = devices_facts(devices, joined)
            .into_iter()
            .map(|(key, facts)| DeviceProof {
                key,
                proof: prove_joined_device_confinement(&facts),
                readers: readers_summary(&facts),
                reported: reported_readers(&facts),
            })
            .collect();
        let settling = proved.iter().any(|proved| {
            proved
                .proof
                .as_ref()
                .is_err_and(JoinedDeviceGap::may_settle)
        });
        if !settling || Instant::now() >= deadline {
            return proved;
        }
        std::thread::sleep(SETTLE_STEP);
    }
}

/// Proves one typed device reaches `joined`'s X server alone, waiting
/// up to `settle` for that server to add a device it has not opened yet.
/// Blocking.
///
/// # Errors
///
/// The gap, with the facts it was read from.
pub fn prove_confined(
    nodes: &[DeviceNode],
    joined: &JoinedDesktop,
    settle: Duration,
) -> Result<JoinedDeviceFacts, Box<(JoinedDeviceGap, JoinedDeviceFacts)>> {
    let deadline = Instant::now() + settle;
    loop {
        let facts = device_facts(nodes, joined);
        match prove_joined_device_confinement(&facts) {
            Ok(()) => return Ok(facts),
            Err(gap) if gap.may_settle() && Instant::now() < deadline => {
                std::thread::sleep(SETTLE_STEP);
            }
            Err(gap) => return Err(Box::new((gap, facts))),
        }
    }
}

/// The allowed readers worth reporting (system services, the joined
/// session's and account's own processes), by name and account.
#[must_use]
pub fn reported_readers(facts: &JoinedDeviceFacts) -> BTreeSet<String> {
    facts
        .nodes
        .iter()
        .flat_map(|node| &node.readers)
        .chain(&facts.multiplexed_mouse_readers)
        .filter_map(DeviceNodeReader::report_label)
        .collect()
}

/// A one-line summary of who reads which node, for the log.
#[must_use]
pub fn readers_summary(facts: &JoinedDeviceFacts) -> String {
    let mut by_node: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for node in &facts.nodes {
        by_node.insert(
            &node.name,
            node.readers.iter().map(ToString::to_string).collect(),
        );
    }
    by_node
        .into_iter()
        .map(|(node, readers)| format!("{node}: [{}]", readers.join(", ")))
        .collect::<Vec<_>>()
        .join("; ")
}

/// Lab helpers for the ignored native tests.
#[cfg(test)]
pub(crate) mod lab_seat {
    /// A volatile udev rule that puts every device named for `display` on a
    /// seat nobody runs, removed (and udev reloaded) on drop.
    pub(crate) struct LabSeatRule(std::path::PathBuf);

    impl LabSeatRule {
        pub(crate) fn install(display: &str) -> Self {
            let path = std::path::PathBuf::from(format!(
                "/run/udev/rules.d/72-arcen-i1-lab-{}-{}.rules",
                std::process::id(),
                display.trim_start_matches(':')
            ));
            std::fs::create_dir_all("/run/udev/rules.d").expect("rules directory");
            std::fs::write(
                &path,
                format!(
                    "SUBSYSTEM==\"input\", ATTRS{{name}}==\"Arcen Virtual *(session {display})\", \
                     ENV{{ID_SEAT}}=\"seat-arcen-i1lab\", ENV{{LIBINPUT_IGNORE_DEVICE}}=\"1\", \
                     TAG-=\"uaccess\", TAG-=\"seat\"\n"
                ),
            )
            .expect("write rule");
            let reloaded = std::process::Command::new("udevadm")
                .args(["control", "--reload"])
                .status()
                .expect("udevadm");
            assert!(reloaded.success());
            Self(path)
        }
    }

    impl Drop for LabSeatRule {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
            let _ = std::process::Command::new("udevadm")
                .args(["control", "--reload"])
                .status();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn process(pid: u32, comm: &str, devices: &[(u64, u64)], sockets: &[u64]) -> ProcessFacts {
        ProcessFacts {
            pid,
            comm: comm.to_owned(),
            uid: Some(0),
            devices: devices.to_vec(),
            sockets: sockets.to_vec(),
            readable: true,
            login: Login::default(),
            host_namespaces: true,
        }
    }

    fn identity() -> JoinedIdentity {
        JoinedIdentity {
            session: "35175".to_owned(),
            uid: 1000,
        }
    }

    fn in_session(mut process: ProcessFacts, uid: u32, session: &str) -> ProcessFacts {
        process.uid = Some(uid);
        process.login.session = Some(session.to_owned());
        process
    }

    fn nodes() -> Vec<DeviceNode> {
        vec![
            DeviceNode {
                name: "event21".to_owned(),
                device: (13, 85),
            },
            DeviceNode {
                name: "mouse5".to_owned(),
                device: (13, 37),
            },
        ]
    }

    #[test]
    fn device_numbers_parse_from_sysfs() {
        assert_eq!(parse_device_number("13:85\n"), Some((13, 85)));
        assert_eq!(parse_device_number("13"), None);
        assert_eq!(parse_device_number("a:1"), None);
    }

    /// A reader is matched by the device it holds, not the path it opened
    /// it through: the same character device reached through a symlink
    /// outside `/dev` (as a bind mount, hard link or second `mknod` node
    /// would be) is found and classified exactly as the one opened at its
    /// `/dev` path. `/dev/null` (1:3) stands in for an input node.
    #[test]
    fn a_reader_holding_the_device_through_another_path_is_found() {
        let root = std::env::temp_dir().join(format!("arcen-fd-identity-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let (proc, scratch) = (root.join("proc"), root.join("scratch"));
        fs::create_dir_all(&scratch).unwrap();
        std::os::unix::fs::symlink("/dev/null", scratch.join("alias")).unwrap();
        fs::write(scratch.join("file"), b"").unwrap();
        for (pid, fds) in [
            ("100", vec![("3", PathBuf::from("/dev/null"))]),
            ("101", vec![("3", scratch.join("alias"))]),
            ("102", vec![("3", scratch.join("file"))]),
        ] {
            let fd_dir = proc.join(pid).join("fd");
            fs::create_dir_all(&fd_dir).unwrap();
            fs::create_dir_all(proc.join(pid).join("fdinfo")).unwrap();
            for (fd, target) in fds {
                fs::write(
                    proc.join(pid).join("fdinfo").join(fd),
                    fdinfo_of(&target, 0o2_000_000),
                )
                .unwrap();
                std::os::unix::fs::symlink(target, fd_dir.join(fd)).unwrap();
            }
        }
        let null = (1, 3);
        let processes = scan_processes(&proc, &[null]);
        let held = |pid: u32| {
            processes
                .iter()
                .find(|process| process.pid == pid)
                .expect("scanned")
                .devices
                .clone()
        };
        assert_eq!(held(100), [null], "at its /dev path");
        assert_eq!(held(101), [null], "through another path");
        assert!(held(102).is_empty(), "a plain file is no device");
        let classified = |pid: u32| {
            let mut process = processes
                .iter()
                .find(|process| process.pid == pid)
                .expect("scanned")
                .clone();
            process.uid = Some(1001);
            process.login.session = Some("7".to_owned());
            process.host_namespaces = true;
            let node = [DeviceNode {
                name: "event21".to_owned(),
                device: null,
            }];
            facts_from(&[process], &[], &node, true, &identity()).nodes[0]
                .readers
                .clone()
        };
        assert!(matches!(
            classified(101).as_slice(),
            [DeviceNodeReader::OtherSession { pid: 101, .. }]
        ));
        assert!(matches!(
            classified(100).as_slice(),
            [DeviceNodeReader::OtherSession { pid: 100, .. }]
        ));
        assert_eq!(
            descriptor(&proc.join("101/fd/3"), &[null]),
            Descriptor::CharDevice(null)
        );
        assert_eq!(
            descriptor(&proc.join("102/fd/3"), &[null]),
            Descriptor::Other
        );
        assert_eq!(
            descriptor(&proc.join("101/fd/9"), &[null]),
            Descriptor::Other
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_joined_server_is_the_one_listening_as_its_display() {
        let net_unix = "Num       RefCount Protocol Flags    Type St Inode Path\n\
            ffff0001: 00000002 00000000 00010000 0001 01 22710188 @/tmp/.X11-unix/X1\n\
            ffff0002: 00000002 00000000 00010000 0001 01 22710189 /tmp/.X11-unix/X1\n\
            ffff0003: 00000003 00000000 00000000 0001 03 22710249 @/tmp/.X11-unix/X1\n\
            ffff0004: 00000002 00000000 00010000 0001 01 55 /tmp/.X11-unix/X10\n\
            ffff0005: 00000002 00000000 00010000 0001 01 56 /run/other\n";
        assert_eq!(listening_x_sockets(net_unix, 1), [22_710_188, 22_710_189]);
        assert_eq!(listening_x_sockets(net_unix, 10), [55]);
        assert!(listening_x_sockets(net_unix, 2).is_empty());
    }

    #[test]
    fn the_terminal_in_front_is_read_from_sysfs() {
        assert_eq!(foreground_vt("tty7\n"), Some(7));
        assert_eq!(foreground_vt("tty"), None);
        assert_eq!(foreground_vt("console"), None);
        assert_eq!(session_vt("../etc"), None);
        assert_eq!(session_vt(""), None);
        let gate = JoinedInputGate::new(None);
        gate.set(VirtualInputRole::Absolute, true);
        assert!(gate.admits(VirtualInputRole::Absolute));
        assert!(!gate.admits(VirtualInputRole::Relative));
        gate.shut_all();
        assert!(!gate.admits(VirtualInputRole::Absolute));
    }

    /// A process in a namespace of its own (an unprivileged user namespace
    /// with its own `/tmp/.X11-unix`, named `Xorg`) is never the joined X
    /// server, and cannot make two candidates of the real one.
    #[test]
    fn an_x_server_outside_the_hosts_namespaces_is_never_the_joined_one() {
        let joined = process(400, "Xorg", &[(13, 85)], &[22_710_188]);
        let mut impostor = process(5000, "Xorg", &[], &[22_710_188]);
        impostor.host_namespaces = false;
        let facts = facts_from(
            &[joined, impostor],
            &[22_710_188],
            &nodes(),
            true,
            &identity(),
        );
        assert!(facts.joined_server);
        assert_eq!(prove_joined_device_confinement(&facts), Ok(()));
    }

    #[test]
    fn the_session_in_front_is_loginds_active_one() {
        let record = "# This is private data. Do not parse.\nIS_SEAT0=1\nACTIVE=35175\n\
                      ACTIVE_UID=1000\nSESSIONS=35175 c2\n";
        assert!(session_in_front(record, "35175"));
        assert!(!session_in_front(record, "c2"));
        assert!(!session_in_front(record, "3517"));
        assert!(!session_in_front(record, ""));
    }

    /// The joined X server and logind hold the event node: confined. The
    /// login screen's X server holding it too, or any other process, is a
    /// foreign reader; an X server of another display is never the joined
    /// one.
    #[test]
    fn readers_are_classified_against_the_joined_server() {
        let joined = process(400, "Xorg", &[(13, 85)], &[22_710_188]);
        let logind = process(1, "systemd-logind", &[(13, 85)], &[]);
        let facts = facts_from(
            &[joined.clone(), logind.clone()],
            &[22_710_188],
            &nodes(),
            true,
            &identity(),
        );
        assert_eq!(
            facts.nodes[0].readers,
            [
                DeviceNodeReader::JoinedDisplayServer,
                DeviceNodeReader::InputBroker
            ]
        );
        assert!(facts.nodes[1].readers.is_empty());
        assert_eq!(prove_joined_device_confinement(&facts), Ok(()));

        let greeter = process(900, "Xorg", &[(13, 85)], &[777]);
        let facts = facts_from(
            &[joined.clone(), logind.clone(), greeter],
            &[22_710_188],
            &nodes(),
            true,
            &identity(),
        );
        assert!(matches!(
            prove_joined_device_confinement(&facts),
            Err(JoinedDeviceGap::ForeignReader {
                reader: DeviceNodeReader::OtherDisplayServer { pid: 900, .. },
                ..
            })
        ));

        // logind's descriptor store keeps a copy in pid 1 (seen on the
        // Xorg lab host for a device the console's X server took): the
        // broker, not a reader. A user's service manager, "systemd" too,
        // is not pid 1: another account's.
        let store = process(1, "systemd", &[(13, 85)], &[]);
        let facts = facts_from(
            &[joined.clone(), logind.clone(), store],
            &[22_710_188],
            &nodes(),
            true,
            &identity(),
        );
        assert_eq!(prove_joined_device_confinement(&facts), Ok(()));
        let mut user_manager = process(4242, "systemd", &[(13, 85)], &[]);
        user_manager.uid = Some(1001);
        user_manager.login.user_manager = Some(1001);
        let facts = facts_from(
            &[joined.clone(), user_manager],
            &[22_710_188],
            &nodes(),
            true,
            &identity(),
        );
        assert!(matches!(
            prove_joined_device_confinement(&facts),
            Err(JoinedDeviceGap::ForeignReader {
                reader: DeviceNodeReader::OtherAccount {
                    pid: 4242,
                    uid: 1001,
                    ..
                },
                ..
            })
        ));

        // A logind impostor not running as root is just another account's
        // process.
        let mut impostor = process(31, "systemd-logind", &[(13, 37)], &[]);
        impostor.uid = Some(1001);
        let facts = facts_from(
            &[joined.clone(), impostor],
            &[22_710_188],
            &nodes(),
            true,
            &identity(),
        );
        assert!(matches!(
            prove_joined_device_confinement(&facts),
            Err(JoinedDeviceGap::ForeignReader { node, reader: DeviceNodeReader::OtherAccount { pid: 31, .. } })
                if node == "mouse5"
        ));

        // The multiplexed mouse's readers count for a mousedev node, through
        // /dev/input/mice or /dev/psaux: gpm, a system service, may read
        // it; a reader in another session may not.
        for shared in [MULTIPLEXED_MOUSE, PSAUX_MOUSE] {
            let gpm = process(55, "gpm", &[shared], &[]);
            let facts = facts_from(
                &[joined.clone(), gpm],
                &[22_710_188],
                &nodes(),
                true,
                &identity(),
            );
            assert!(matches!(
                facts.multiplexed_mouse_readers.as_slice(),
                [DeviceNodeReader::SystemService {
                    pid: 55,
                    uid: 0,
                    ..
                }]
            ));
            assert_eq!(prove_joined_device_confinement(&facts), Ok(()));
            let console = in_session(process(56, "evtest", &[shared], &[]), 1001, "7");
            let facts = facts_from(
                &[joined.clone(), console],
                &[22_710_188],
                &nodes(),
                true,
                &identity(),
            );
            assert!(matches!(
                prove_joined_device_confinement(&facts),
                Err(JoinedDeviceGap::ForeignReader { node, .. }) if node == "mice"
            ));
        }

        // The joined session's own processes may read; a login screen's may
        // not.
        let own = in_session(
            process(70, "gsd-media-keys", &[(13, 85)], &[]),
            1000,
            "35175",
        );
        let facts = facts_from(
            &[joined.clone(), own],
            &[22_710_188],
            &nodes(),
            true,
            &identity(),
        );
        assert_eq!(prove_joined_device_confinement(&facts), Ok(()));
        let greeter = in_session(process(71, "gsd-media-keys", &[(13, 85)], &[]), 42, "c1");
        let facts = facts_from(
            &[joined.clone(), greeter],
            &[22_710_188],
            &nodes(),
            true,
            &identity(),
        );
        assert!(matches!(
            prove_joined_device_confinement(&facts),
            Err(JoinedDeviceGap::ForeignReader {
                reader: DeviceNodeReader::OtherSession { pid: 71, .. },
                ..
            })
        ));
        // A process whose descriptors could not be read holds everything
        // and is attributed to nothing.
        let mut hidden = process(72, "acpid", &[(13, 85)], &[]);
        hidden.readable = false;
        let facts = facts_from(
            &[joined.clone(), hidden],
            &[22_710_188],
            &nodes(),
            true,
            &identity(),
        );
        assert!(matches!(
            prove_joined_device_confinement(&facts),
            Err(JoinedDeviceGap::ForeignReader {
                reader: DeviceNodeReader::Unattributable { pid: 72, .. },
                ..
            })
        ));

        // No server listening as the display, or two, identifies nobody:
        // an X server holding the node is then another display's.
        let facts = facts_from(
            std::slice::from_ref(&joined),
            &[1],
            &nodes(),
            true,
            &identity(),
        );
        assert_eq!(
            facts.nodes[0].readers,
            [DeviceNodeReader::OtherDisplayServer {
                pid: 400,
                comm: "Xorg".to_owned()
            }]
        );
        let idle = process(400, "Xorg", &[], &[22_710_188]);
        let facts = facts_from(&[idle], &[1], &nodes(), true, &identity());
        assert_eq!(
            prove_joined_device_confinement(&facts),
            Err(JoinedDeviceGap::NoJoinedServer)
        );
        let twin = process(401, "Xorg", &[], &[22_710_188]);
        let facts = facts_from(
            &[joined.clone(), twin],
            &[22_710_188],
            &nodes(),
            true,
            &identity(),
        );
        assert!(!facts.joined_server);
        // Not in front.
        let facts = facts_from(&[joined], &[22_710_188], &nodes(), false, &identity());
        assert_eq!(
            prove_joined_device_confinement(&facts),
            Err(JoinedDeviceGap::NotInFront)
        );
        // The server has not added the device yet.
        let idle = process(400, "Xorg", &[], &[22_710_188]);
        let facts = facts_from(&[idle], &[22_710_188], &nodes(), true, &identity());
        assert_eq!(
            prove_joined_device_confinement(&facts),
            Err(JoinedDeviceGap::NotAdmitted)
        );
    }

    /// Only root creates the slice and scope levels a login is read from;
    /// what an account nests in its own manager stays that account's.
    #[test]
    fn a_process_s_login_is_read_from_its_cgroup() {
        let session = |session: &str| Login {
            session: Some(session.to_owned()),
            user_manager: None,
        };
        let manager = |uid| Login {
            session: None,
            user_manager: Some(uid),
        };
        assert_eq!(
            login_of("0::/user.slice/user-1000.slice/session-35175.scope\n"),
            session("35175")
        );
        assert_eq!(
            login_of("0::/user.slice/user-42.slice/session-c1.scope\n"),
            session("c1")
        );
        assert_eq!(
            login_of("0::/user.slice/user-42.slice/user@42.service/session.slice/gsd.service\n"),
            manager(42)
        );
        // An account naming a cgroup of its own after the joined session.
        assert_eq!(
            login_of(
                "0::/user.slice/user-1001.slice/user@1001.service/app.slice/session-35175.scope\n"
            ),
            manager(1001)
        );
        assert_eq!(
            login_of("0::/system.slice/acpid.service\n"),
            Login::default()
        );
        assert_eq!(login_of("0::/init.scope\n"), Login::default());
        assert_eq!(login_of(""), Login::default());
        assert_eq!(
            login_of("0::/user.slice/user-x.slice/session-1.scope\n"),
            Login::default()
        );
        // A legacy (hybrid) host: systemd's own hierarchy.
        assert_eq!(
            login_of(
                "12:cpu:/user.slice\n1:name=systemd:/user.slice/user-1000.slice/session-7.scope\n0::/\n"
            ),
            session("7")
        );
    }

    /// Path text never makes a reader: a descriptor whose link says
    /// `/dev/input/…` but that cannot be stat'ed (a FUSE file in a user's
    /// own namespace answers root with `EACCES`; here the name is too long
    /// to resolve) or that is another device is no reader of any node, and
    /// an `O_PATH` handle on the device itself reads nothing.
    #[test]
    fn path_text_alone_never_holds_a_node() {
        use std::os::fd::AsRawFd;
        let root = std::env::temp_dir().join(format!("arcen-fd-text-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let fd_dir = root.join("proc/200/fd");
        fs::create_dir_all(&fd_dir).unwrap();
        let unresolvable = format!("/dev/input/event21{}", "x".repeat(300));
        std::os::unix::fs::symlink(&unresolvable, fd_dir.join("3")).unwrap();
        std::os::unix::fs::symlink("/dev/zero", fd_dir.join("4")).unwrap();
        let wanted = [(13, 85), (1, 3)];
        let processes = scan_processes(&root.join("proc"), &wanted);
        let reader = processes
            .iter()
            .find(|process| process.pid == 200)
            .expect("scanned");
        assert!(reader.readable);
        assert!(reader.devices.is_empty(), "{:?}", reader.devices);
        assert_eq!(descriptor(&fd_dir.join("3"), &wanted), Descriptor::Other);
        // /dev/zero (1:5) is no wanted node.
        assert_eq!(descriptor(&fd_dir.join("4"), &wanted), Descriptor::Other);

        let handle = rustix::fs::open(
            "/dev/null",
            rustix::fs::OFlags::PATH | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )
        .unwrap();
        let opened = fs::File::open("/dev/null").unwrap();
        let own = |fd: i32| PathBuf::from(format!("/proc/self/fd/{fd}"));
        assert_eq!(
            descriptor(&own(handle.as_raw_fd()), &[(1, 3)]),
            Descriptor::Other
        );
        assert_eq!(
            descriptor(&own(opened.as_raw_fd()), &[(1, 3)]),
            Descriptor::CharDevice((1, 3))
        );
        fs::remove_dir_all(&root).unwrap();
    }

    /// An `fdinfo` record for an open of `target`, with `flags` (octal).
    fn fdinfo_of(target: &Path, flags: u32) -> String {
        let stat = rustix::fs::statx(
            rustix::fs::CWD,
            target,
            rustix::fs::AtFlags::empty(),
            rustix::fs::StatxFlags::INO | rustix::fs::StatxFlags::MNT_ID,
        )
        .unwrap();
        format!(
            "pos:\t0\nflags:\t{flags:07o}\nmnt_id:\t{}\nino:\t{}\n",
            stat.stx_mnt_id, stat.stx_ino
        )
    }

    /// `statx` and `fdinfo` are two lookups: a descriptor counts as a
    /// reader only when both describe the same non-`O_PATH` open of the
    /// device. An `O_PATH` handle swapped in and out (which needs no read
    /// permission on the node) shows up as `fdinfo` saying `O_PATH`,
    /// missing, or naming another file while `statx` sees the device; none
    /// of them is a reader.
    #[test]
    fn a_descriptor_whose_fdinfo_and_statx_disagree_is_no_reader() {
        let root = std::env::temp_dir().join(format!("arcen-fd-race-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let process = root.join("proc/300");
        fs::create_dir_all(process.join("fd")).unwrap();
        fs::create_dir_all(process.join("fdinfo")).unwrap();
        fs::write(root.join("file"), b"").unwrap();
        let null = Path::new("/dev/null");
        let cases = [
            ("3", Some(fdinfo_of(null, 0o2_100_000)), true),
            ("4", Some(fdinfo_of(null, O_PATH_FLAG | 0o2_000_000)), false),
            ("5", None, false),
            ("6", Some(fdinfo_of(&root.join("file"), 0)), false),
            ("7", Some(fdinfo_of(Path::new("/dev/zero"), 0)), false),
            ("8", Some("pos:\t0\n".to_owned()), false),
        ];
        for (fd, info, _) in &cases {
            std::os::unix::fs::symlink(null, process.join("fd").join(fd)).unwrap();
            if let Some(info) = info {
                fs::write(process.join("fdinfo").join(fd), info).unwrap();
            }
        }
        for (fd, _, reader) in &cases {
            let found = descriptor(&process.join("fd").join(fd), &[(1, 3)]);
            let expected = if *reader {
                Descriptor::CharDevice((1, 3))
            } else {
                Descriptor::Other
            };
            assert_eq!(found, expected, "fd {fd}");
        }
        let scanned = scan_processes(&root.join("proc"), &[(1, 3)]);
        assert_eq!(scanned[0].devices, [(1, 3)], "only the stable open counts");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_device_s_nodes_are_its_children_with_a_device_number() {
        let root = std::env::temp_dir().join(format!("arcen-pen-nodes-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for (name, dev) in [
            ("event21", Some("13:85\n")),
            ("mouse5", Some("13:37\n")),
            ("capabilities", None),
            ("power", None),
        ] {
            fs::create_dir_all(root.join(name)).unwrap();
            if let Some(dev) = dev {
                fs::write(root.join(name).join("dev"), dev).unwrap();
            }
        }
        fs::write(
            root.join("name"),
            "Arcen Virtual Tablet Pen (session :88)\n",
        )
        .unwrap();
        assert_eq!(device_nodes(&root).unwrap(), nodes());
        fs::remove_dir_all(&root).unwrap();
        assert!(device_nodes(&root).is_err());
    }

    #[test]
    fn the_summary_names_every_node_and_reader() {
        let joined = process(400, "Xorg", &[(13, 85)], &[9]);
        let facts = facts_from(&[joined], &[9], &nodes(), true, &identity());
        assert_eq!(
            readers_summary(&facts),
            "event21: [the joined desktop's display server]; mouse5: []"
        );
    }
}
