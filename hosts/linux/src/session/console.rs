// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Joining the user's console session (ADR 0025): the Linux adapter.
//!
//! Every decision is `arcen_session::console_session`'s. This module only
//! reads what the system says (logind's sessions, a session's processes and
//! X authority, evdev and udev, RandR) and does what the decisions ask:
//! complete a remote-first sign-in through GDM's login screen, keep the
//! desk's monitors blank and its input disabled while a Deck is attached,
//! and hand the console back locked. Nothing here starts an X server or a
//! desktop, and nothing ends the user's session.

use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use arcen_session::console_session::{
    self as model, greeter_token, ConsoleNotice, ConsoleRefusal, ConsoleSessionFact,
    ForeignService, InputDeviceFact,
};
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use x11rb::connection::Connection as _;
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::rust_connection::{DefaultStream, RustConnection};

use crate::logging::target;

const LOGINCTL: &str = "/usr/bin/loginctl";
const BUSCTL: &str = "/usr/bin/busctl";
const COMMAND_TIMEOUT: Duration = Duration::from_secs(3);
/// What a console decision reads of every logind session.
const SESSION_PROPERTIES: &str =
    "Id,User,Name,Seat,VTNr,TTY,Class,Type,State,Active,Remote,Service,LockedHint,Leader";
const MAX_ENVIRON_BYTES: u64 = 256 * 1024;
const MAX_XAUTHORITY_BYTES: u64 = 64 * 1024;
/// Where the launcher records what it changed on the console, so a Pier
/// that lost its launcher can still hand the console back.
pub(crate) const PRIVACY_JOURNAL: &str = "/run/arcen/console-privacy.json";
/// How long a remote-first sign-in may take from token to a running
/// session of the user.
const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(30);
/// How long, after a remote-first sign-in, GDM may take to finish the login
/// screen that took it (it ends a few seconds after the session starts).
const SIGN_IN_SETTLE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the signed-in session must stay in front, with that login
/// screen gone, before it counts as settled (GDM puts a new login screen
/// in front about a second after the old one ends when it loses the race).
const SIGN_IN_SETTLE: Duration = Duration::from_secs(2);
/// A display manager that keeps its login screen: the session counts as
/// settled after this long in front.
const SIGN_IN_KEEPS_GREETER: Duration = Duration::from_secs(8);
/// How often the signed-in session is brought back in front of a new login
/// screen before the sign-in gives up.
const SIGN_IN_BRING_FORWARD_ATTEMPTS: u32 = 3;
/// How long a remote-first sign-in waits for the login screen's shell
/// (GDM restarted, or a login screen brought to its terminal).
const LOGIN_SCREEN_READY: Duration = Duration::from_secs(15);
/// How often a sign-in brings a login screen that lost its terminal switch
/// to its terminal.
const LOGIN_SCREEN_BRING_ATTEMPTS: u32 = 2;
/// A login screen's shell takes the sign-in token only once its login
/// dialog listens; one that started a moment ago may not yet. While the
/// token is unspent it is given again after this long.
const TOKEN_RESEND_AFTER: Duration = Duration::from_secs(5);
const TOKEN_RESENDS: u32 = 3;
/// How often the Pier looks for a login screen left without its terminal.
const LOGIN_SCREEN_WATCH_TICK: Duration = Duration::from_secs(2);
/// How long a joined session's X display may take to answer (a session the
/// login screen just started).
const DISPLAY_TIMEOUT: Duration = Duration::from_secs(15);
/// How long handing back waits for the lock screen before the monitors
/// come back regardless.
const LOCK_SCREEN_TIMEOUT: Duration = Duration::from_secs(4);

fn safe_session_id(session_id: &str) -> bool {
    !session_id.is_empty()
        && session_id.len() <= 64
        && session_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

async fn bounded_output(mut command: Command) -> Option<std::process::Output> {
    command
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null());
    match tokio::time::timeout(COMMAND_TIMEOUT, command.output()).await {
        Ok(Ok(output)) if output.status.success() => Some(output),
        _ => None,
    }
}

/// Every logind session, as facts. A session logind cannot describe is
/// left out; an unreadable session list is an empty one.
pub(crate) async fn read_sessions() -> Vec<ConsoleSessionFact> {
    let mut list = Command::new(LOGINCTL);
    list.args(["list-sessions", "--no-legend", "--no-pager"]);
    let Some(output) = bounded_output(list).await else {
        return Vec::new();
    };
    let ids: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .filter(|id| safe_session_id(id))
        .map(str::to_owned)
        .collect();
    let mut facts = Vec::with_capacity(ids.len());
    for id in ids {
        if let Some(fact) = session_fact(&id).await {
            facts.push(fact);
        }
    }
    facts
}

pub(crate) async fn session_fact(session_id: &str) -> Option<ConsoleSessionFact> {
    if !safe_session_id(session_id) {
        return None;
    }
    let mut show = Command::new(LOGINCTL);
    show.args(["show-session", session_id]);
    for property in SESSION_PROPERTIES.split(',') {
        show.args(["-p", property]);
    }
    let output = bounded_output(show).await?;
    ConsoleSessionFact::from_properties(&String::from_utf8_lossy(&output.stdout))
}

/// The system service a cgroup path (`/proc/<pid>/cgroup`) puts a process
/// in: the first `<name>.service` below `system.slice`, or `None` (a user
/// session's scope, the user's own manager, the root slice).
fn system_service_unit(cgroup: &str) -> Option<String> {
    cgroup.lines().find_map(|line| {
        // cgroup v2 (`0::/path`) or the systemd hierarchy of v1.
        let path = line
            .strip_prefix("0::")
            .or_else(|| line.split_once(":name=systemd:").map(|(_, path)| path))?;
        let mut components = path.split('/').skip_while(|c| *c != "system.slice").skip(1);
        components
            .find(|component| component.ends_with(".service"))
            .map(str::to_owned)
    })
}

fn parent_pid(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, after) = stat.rsplit_once(')')?;
    after.split_whitespace().nth(1)?.parse().ok()
}

fn process_name(pid: u32) -> Option<String> {
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .ok()
        .map(|comm| comm.trim().to_owned())
}

/// The system service a process started by systemd belongs to: its
/// `INVOCATION_ID` (set by systemd for every service process) matched
/// against systemd's own record of each unit's current invocation
/// (`/run/systemd/units/invocation:<unit>`).
fn invoked_unit(pid: u32) -> Option<String> {
    let invocation = environ_value(pid, "INVOCATION_ID")?;
    if invocation.len() != 32 || !invocation.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    std::fs::read_dir("/run/systemd/units")
        .ok()?
        .flatten()
        .find_map(|entry| {
            let name = entry
                .file_name()
                .to_str()?
                .strip_prefix("invocation:")?
                .to_owned();
            let target = std::fs::read_link(entry.path()).ok()?;
            (name.ends_with(".service") && target.to_str() == Some(invocation.as_str()))
                .then_some(name)
        })
}

/// What the host says started `fact`'s session: its PAM service, and the
/// system service of its leader or of the leader's nearest ancestor in
/// one, with that process's name. Read only to name another
/// remote-desktop service in a refusal and in the Pier's log.
pub(crate) fn foreign_service(fact: &ConsoleSessionFact) -> ForeignService {
    let mut pid = fact.leader;
    let mut starter = None;
    for _ in 0..16 {
        if pid <= 1 {
            break;
        }
        if let Some(unit) = std::fs::read_to_string(format!("/proc/{pid}/cgroup"))
            .ok()
            .as_deref()
            .and_then(system_service_unit)
        {
            starter = Some((unit, process_name(pid)));
            break;
        }
        let Some(parent) = parent_pid(pid) else {
            break;
        };
        if parent == 1 {
            // Started by systemd itself (a service with `PAMName=`): logind
            // moved it into the session's scope, and its unit is the one
            // whose invocation it belongs to.
            starter = invoked_unit(pid).map(|unit| (unit, process_name(pid)));
            break;
        }
        pid = parent;
    }
    let (unit, process) = starter.map_or((None, None), |(unit, process)| (Some(unit), process));
    ForeignService::new(&fact.service, unit.as_deref(), process.as_deref())
}

/// The notice for `refusal` of a sign-in of `uid`: when another
/// remote-desktop service holds the workstation, it names that service as
/// the host reports it, and logs what it found.
pub(crate) fn notice_for(
    refusal: ConsoleRefusal,
    sessions: &[ConsoleSessionFact],
    uid: u32,
) -> ConsoleNotice {
    if refusal != ConsoleRefusal::OtherUserOtherProduct {
        return refusal.into();
    }
    let Some(holder) = model::other_product_holder(sessions, uid) else {
        return refusal.into();
    };
    let service = foreign_service(holder);
    tracing::warn!(
        target: target::SESSION,
        reason_class = refusal.token(),
        holder_session = %holder.id,
        holder_uid = holder.uid,
        holder_vt = holder.vtnr,
        holder_leader = holder.leader,
        pam_service = service.pam_service().unwrap_or("unknown"),
        unit = service.unit().unwrap_or("unknown"),
        process = service.process().unwrap_or("unknown"),
        "another remote-desktop service holds this workstation"
    );
    ConsoleNotice::naming(refusal, Some(service))
}

/// One process logind assigns to a session.
struct SessionProcess {
    pid: u32,
    uid: u32,
    comm: String,
}

/// The processes of `uid` in `session_id`'s own scope
/// (`/user.slice/user-<uid>.slice/session-<id>.scope`), read from `/proc`:
/// the session's processes, not anything a user started elsewhere (a user
/// can name a scope of their own manager `session-<id>.scope`, but not
/// place it there).
fn session_processes(session_id: &str, uid: u32) -> Vec<SessionProcess> {
    let scope = format!("/user.slice/user-{uid}.slice/session-{session_id}.scope");
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut processes = Vec::new();
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        let base = entry.path();
        let Ok(cgroup) = std::fs::read_to_string(base.join("cgroup")) else {
            continue;
        };
        if !cgroup.lines().any(|line| line.ends_with(&scope)) {
            continue;
        }
        let Ok(metadata) = std::fs::metadata(&base) else {
            continue;
        };
        if metadata.uid() != uid {
            continue;
        }
        let comm = std::fs::read_to_string(base.join("comm"))
            .map(|comm| comm.trim().to_owned())
            .unwrap_or_default();
        processes.push(SessionProcess {
            pid,
            uid: metadata.uid(),
            comm,
        });
    }
    processes.sort_by_key(|process| process.pid);
    processes
}

/// `XDG_SESSION_DESKTOP` as the user's processes in the session's scope
/// have it (GDM 40 does not give logind the session's desktop).
pub(crate) fn session_environ_desktop(session_id: &str, uid: u32) -> Option<String> {
    session_processes(session_id, uid)
        .iter()
        .find_map(|process| environ_value(process.pid, "XDG_SESSION_DESKTOP"))
}

fn environ_value(pid: u32, key: &str) -> Option<String> {
    use std::io::Read as _;
    let file = std::fs::File::open(format!("/proc/{pid}/environ")).ok()?;
    let mut bytes = Vec::new();
    file.take(MAX_ENVIRON_BYTES).read_to_end(&mut bytes).ok()?;
    let prefix = format!("{key}=");
    bytes
        .split(|byte| *byte == 0)
        .filter_map(|entry| std::str::from_utf8(entry).ok())
        .find_map(|entry| entry.strip_prefix(&prefix).map(str::to_owned))
}

/// A joined session's X display and the authority to open it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SessionDisplay {
    pub(crate) display: String,
    pub(crate) number: u32,
    pub(crate) xauthority: PathBuf,
    /// The session's user: the authority file and the X server answering on
    /// the socket are proved against it when the display is opened.
    /// `None` only in a journal an older launcher wrote: the file's owner
    /// stands in for it then.
    #[serde(default)]
    pub(crate) uid: Option<u32>,
}

fn socket_path(number: u32) -> PathBuf {
    PathBuf::from(format!("/tmp/.X11-unix/X{number}"))
}

fn trusted_xauthority(path: &Path, uid: u32) -> bool {
    path.is_absolute()
        && std::fs::symlink_metadata(path).is_ok_and(|metadata| {
            model::xauthority_trustworthy(
                metadata.file_type().is_file(),
                metadata.uid(),
                metadata.mode(),
                uid,
            )
        })
}

/// The display of `session_id` as its own processes (owned by `uid`) see
/// it: `DISPLAY` and `XAUTHORITY` (or `~/.Xauthority`, which desktops
/// started outside the display manager use), checked for a local socket and an authority file only the user
/// or root can write.
pub(crate) fn find_session_display(
    session_id: &str,
    uid: u32,
    home: &Path,
) -> Option<SessionDisplay> {
    for process in session_processes(session_id, uid) {
        let Some(display) = environ_value(process.pid, "DISPLAY") else {
            continue;
        };
        let Some(number) = model::local_display_number(&display) else {
            continue;
        };
        let xauthority = environ_value(process.pid, "XAUTHORITY")
            .map_or_else(|| home.join(".Xauthority"), PathBuf::from);
        let socket = std::fs::symlink_metadata(socket_path(number))
            .is_ok_and(|metadata| metadata.file_type().is_socket());
        if socket && trusted_xauthority(&xauthority, uid) {
            return Some(SessionDisplay {
                display: format!(":{number}"),
                number,
                xauthority,
                uid: Some(uid),
            });
        }
    }
    None
}

/// Waits for the session's display to exist and answer.
pub(crate) async fn wait_for_session_display(
    session_id: &str,
    uid: u32,
    home: &Path,
) -> Option<(SessionDisplay, u8)> {
    let deadline = tokio::time::Instant::now() + DISPLAY_TIMEOUT;
    loop {
        if let Some(display) = find_session_display(session_id, uid, home) {
            if let Ok(connection) = connect(&display) {
                let depth = connection
                    .setup()
                    .roots
                    .first()
                    .map_or(24, |screen| screen.root_depth);
                return Some((display, depth));
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Opens the session's X display as root with the session's own cookie.
///
/// The authority file is opened without following a link and proved on the
/// opened file (a regular file of the user's or root's, writable by nobody
/// else); the cookie is sent only to an X server the shared
/// `x_server_peer_trustworthy` admits, proved by the socket's peer
/// credentials: the session's processes name the file and the display, and
/// anyone may listen on a display number nobody serves.
pub(crate) fn connect(display: &SessionDisplay) -> Result<RustConnection, String> {
    connect_reporting(display, |_| {})
}

/// [`connect`], handing `on_socket` the X socket before the handshake, so a
/// caller that gives up waiting can shut it down and wake a thread blocked
/// on an X server that accepted the connection but never answers.
pub(crate) fn connect_reporting(
    display: &SessionDisplay,
    on_socket: impl FnOnce(&std::os::unix::net::UnixStream),
) -> Result<RustConnection, String> {
    use std::io::Read as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    // Non-blocking: a FIFO put where the authority file was would block
    // the open itself, before the file type is checked (a regular file's
    // reads ignore the flag).
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC | nix::libc::O_NONBLOCK)
        .open(&display.xauthority)
        .map_err(|error| format!("open X authority: {error}"))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("X authority: {error}"))?;
    let session_uid = display.uid.unwrap_or_else(|| metadata.uid());
    if !model::xauthority_trustworthy(
        metadata.file_type().is_file(),
        metadata.uid(),
        metadata.mode(),
        session_uid,
    ) {
        return Err("the X authority file is not the session's".to_owned());
    }
    let mut cookie = Vec::new();
    (&mut file)
        .take(MAX_XAUTHORITY_BYTES)
        .read_to_end(&mut cookie)
        .map_err(|error| format!("read X authority: {error}"))?;
    let cookie = model::mit_magic_cookie(&cookie, display.number);
    let stream = connect_without_waiting(&socket_path(display.number))
        .map_err(|error| format!("connect X socket: {error}"))?;
    let peer = nix::sys::socket::getsockopt(&stream, nix::sys::socket::sockopt::PeerCredentials)
        .map_err(|error| format!("X socket peer: {error}"))?;
    if !model::x_server_peer_trustworthy(peer.uid(), metadata.uid(), session_uid) {
        return Err(format!(
            "the X socket :{} is not served by the session's X server",
            display.number
        ));
    }
    on_socket(&stream);
    let (stream, _) =
        DefaultStream::from_unix_stream(stream).map_err(|error| format!("X stream: {error}"))?;
    let (name, data) = match cookie {
        Some(data) => (b"MIT-MAGIC-COOKIE-1".to_vec(), data),
        None => (Vec::new(), Vec::new()),
    };
    RustConnection::connect_to_stream_with_auth_info(stream, 0, name, data)
        .map_err(|error| format!("X setup: {error}"))
}

/// Connects to a local socket without waiting for a listener that does not
/// accept: a full backlog (a squatter that listens and never accepts) is
/// an error at once, never a thread blocked in `connect` beyond any bound.
fn connect_without_waiting(path: &Path) -> std::io::Result<std::os::unix::net::UnixStream> {
    use nix::sys::socket::{connect, socket, AddressFamily, SockFlag, SockType, UnixAddr};
    use std::os::fd::AsRawFd as _;
    let fd = socket(
        AddressFamily::Unix,
        SockType::Stream,
        SockFlag::SOCK_NONBLOCK | SockFlag::SOCK_CLOEXEC,
        None,
    )?;
    let address = UnixAddr::new(path)?;
    match connect(fd.as_raw_fd(), &address) {
        Ok(()) => {}
        Err(nix::errno::Errno::EAGAIN) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "the X socket accepts no connection",
            ))
        }
        Err(error) => return Err(error.into()),
    }
    let stream = std::os::unix::net::UnixStream::from(fd);
    stream.set_nonblocking(false)?;
    Ok(stream)
}

/// Writes a root-only journal (`0600`, atomically renamed into place) in a
/// directory only root may write; the console privacy and layout journals
/// both live under `/run/arcen`.
pub(crate) fn write_root_journal(path: &Path, text: &[u8]) -> Result<(), String> {
    use std::io::Write as _;
    use std::os::unix::fs::DirBuilderExt as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    // `/run` is emptied at boot; the tmpfiles rule recreates the
    // directory, and a host without it gets it here.
    let dir = path.parent().ok_or("journal has no directory")?;
    match std::fs::DirBuilder::new().mode(0o755).create(dir) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.to_string()),
    }
    if std::fs::symlink_metadata(dir)
        .map(|metadata| !metadata.is_dir() || metadata.uid() != 0 || metadata.mode() & 0o022 != 0)
        .unwrap_or(true)
    {
        return Err("its directory is not root's alone".to_owned());
    }
    let temporary = path.with_extension("json.new");
    let _ = std::fs::remove_file(&temporary);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(&temporary)
        .map_err(|error| error.to_string())?;
    file.write_all(text)
        .and_then(|()| file.sync_all())
        .map_err(|error| error.to_string())?;
    std::fs::rename(&temporary, path).map_err(|error| error.to_string())
}

/// Reads a journal [`write_root_journal`] wrote: a root-owned regular file
/// of at most 4 MiB, never through a link.
pub(crate) fn read_root_journal(path: &Path) -> Option<Vec<u8>> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if !metadata.file_type().is_file() || metadata.uid() != 0 || metadata.len() > 4 << 20 {
        return None;
    }
    std::fs::read(path).ok()
}

/// The gamma ramps of one CRTC before Arcen blanked it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SavedGamma {
    crtc: u32,
    red: Vec<u16>,
    green: Vec<u16>,
    blue: Vec<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PrivacyJournal {
    session_id: String,
    display: SessionDisplay,
    gamma: Vec<SavedGamma>,
    /// The desk's input devices (`/dev/input/event*`) the session's X
    /// server was told to ignore (XInput `Device Enabled` 0) because the
    /// session could not be secured: unlike a grab, that outlives the
    /// launcher and the Pier, and only a recovery that secured the session
    /// enables them again.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    x_disabled: Vec<String>,
}

impl PrivacyJournal {
    fn write(&self) -> Result<(), String> {
        let text = serde_json::to_vec(self).map_err(|error| error.to_string())?;
        write_root_journal(Path::new(PRIVACY_JOURNAL), &text)
            .map_err(|error| format!("console privacy journal: {error}"))
    }

    fn read() -> Option<Self> {
        serde_json::from_slice(&read_root_journal(Path::new(PRIVACY_JOURNAL))?).ok()
    }

    fn remove() {
        let _ = std::fs::remove_file(PRIVACY_JOURNAL);
    }
}

/// The desk's monitors, blanked by zero gamma ramps on every CRTC. NvFBC
/// and XShm read the framebuffer before the ramps, so the Deck's picture
/// is unaffected (lab-proven, ADR 0025).
struct BlankedOutputs {
    connection: RustConnection,
    root: u32,
    journal: PrivacyJournal,
}

impl BlankedOutputs {
    fn blank(display: &SessionDisplay, session_id: &str) -> Result<Self, String> {
        let connection = connect(display)?;
        let root = connection
            .setup()
            .roots
            .first()
            .map(|screen| screen.root)
            .ok_or("X server has no screen")?;
        // A journal an earlier attachment left (never handed back) holds
        // the monitors' own ramps: they are kept, not replaced by the zero
        // ramps they show now.
        let inherited = PrivacyJournal::read().filter(|previous| previous.display == *display);
        let inherited_journal = inherited.is_some();
        let (gamma, x_disabled) = inherited
            .map(|previous| (previous.gamma, previous.x_disabled))
            .unwrap_or_default();
        let mut blanked = Self {
            connection,
            root,
            journal: PrivacyJournal {
                session_id: session_id.to_owned(),
                display: display.clone(),
                gamma,
                x_disabled,
            },
        };
        if let Err(error) = blanked.enforce() {
            // Half blank is not private: give back what this attachment
            // took. A journal it inherited belongs to a console that was
            // never secured: it stays (blank, devices disabled) for the
            // Pier's recovery.
            if !inherited_journal {
                restore_gamma(&blanked.connection, &blanked.journal.gamma);
                PrivacyJournal::remove();
            }
            return Err(error);
        }
        Ok(blanked)
    }

    fn crtcs(&self) -> Result<Vec<u32>, String> {
        Ok(self
            .connection
            .randr_get_screen_resources_current(self.root)
            .map_err(|error| error.to_string())?
            .reply()
            .map_err(|error| error.to_string())?
            .crtcs)
    }

    /// Blanks every CRTC that is not blank, saving (and journalling) the
    /// ramps of any CRTC seen for the first time, and proves the result.
    /// Returns how many CRTCs it had to blank.
    fn enforce(&mut self) -> Result<usize, String> {
        let mut changed = 0;
        for crtc in self.crtcs()? {
            let current = self
                .connection
                .randr_get_crtc_gamma(crtc)
                .map_err(|error| error.to_string())?
                .reply()
                .map_err(|error| error.to_string())?;
            let blank = current.red.iter().all(|v| *v == 0)
                && current.green.iter().all(|v| *v == 0)
                && current.blue.iter().all(|v| *v == 0);
            if !self.journal.gamma.iter().any(|saved| saved.crtc == crtc) {
                // A ramp that is already zero was blanked by an earlier
                // attachment that never handed back: restore to linear.
                let saved = if blank {
                    linear_ramp(crtc, current.red.len())
                } else {
                    SavedGamma {
                        crtc,
                        red: current.red.clone(),
                        green: current.green.clone(),
                        blue: current.blue.clone(),
                    }
                };
                self.journal.gamma.push(saved);
                self.journal.write()?;
            }
            if blank || current.red.is_empty() {
                continue;
            }
            let zeros = vec![0_u16; current.red.len()];
            self.connection
                .randr_set_crtc_gamma(crtc, &zeros, &zeros, &zeros)
                .map_err(|error| error.to_string())?
                .check()
                .map_err(|error| error.to_string())?;
            let proof = self
                .connection
                .randr_get_crtc_gamma(crtc)
                .map_err(|error| error.to_string())?
                .reply()
                .map_err(|error| error.to_string())?;
            if proof
                .red
                .iter()
                .chain(&proof.green)
                .chain(&proof.blue)
                .any(|v| *v != 0)
            {
                return Err(format!("CRTC {crtc} did not take a blank gamma ramp"));
            }
            changed += 1;
        }
        Ok(changed)
    }

    fn restore(self) {
        restore_gamma(&self.connection, &self.journal.gamma);
        // Devices that stay disabled keep their journal: the Pier's
        // recovery tries them again.
        if enable_x_devices(&self.connection, &self.journal.x_disabled) {
            PrivacyJournal::remove();
        }
    }

    /// Tells the session's X server to ignore the desk's devices `nodes`
    /// and journals them, so the desk stays unusable after this process
    /// is gone (until a recovery secures the session).
    fn disable_x_devices(&mut self, nodes: &[String]) -> Result<usize, String> {
        let changed = set_x_devices_enabled(&self.connection, nodes, false)?;
        let mut added = 0;
        for node in changed {
            if !self.journal.x_disabled.contains(&node) {
                self.journal.x_disabled.push(node);
                added += 1;
            }
        }
        if added > 0 {
            self.journal.write()?;
        }
        Ok(added)
    }
}

/// Sets XInput's `Device Enabled` on every slave device of the X server
/// whose `Device Node` is one of `nodes`, proves it, and returns the
/// nodes it set.
fn set_x_devices_enabled(
    connection: &RustConnection,
    nodes: &[String],
    enabled: bool,
) -> Result<Vec<String>, String> {
    use x11rb::protocol::xinput::{
        ConnectionExt as _, DeviceType, XIChangePropertyAux, XIGetPropertyItems,
    };
    use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, PropMode};
    if nodes.is_empty() {
        return Ok(Vec::new());
    }
    let error = |error: &dyn std::fmt::Display| error.to_string();
    connection
        .xinput_xi_query_version(2, 0)
        .map_err(|e| error(&e))?
        .reply()
        .map_err(|e| error(&e))?;
    let atom = |name: &[u8]| -> Result<u32, String> {
        Ok(connection
            .intern_atom(true, name)
            .map_err(|e| error(&e))?
            .reply()
            .map_err(|e| error(&e))?
            .atom)
    };
    let node_atom = atom(b"Device Node")?;
    let enabled_atom = atom(b"Device Enabled")?;
    if node_atom == 0 || enabled_atom == 0 {
        return Err("the X server names no input device properties".to_owned());
    }
    let read8 = |device: u16, property: u32| -> Option<Vec<u8>> {
        let reply = connection
            .xinput_xi_get_property(device, false, property, AtomEnum::ANY.into(), 0, 64)
            .ok()?
            .reply()
            .ok()?;
        match reply.items {
            XIGetPropertyItems::Data8(bytes) => Some(bytes),
            _ => None,
        }
    };
    let devices = connection
        .xinput_xi_query_device(0_u16)
        .map_err(|e| error(&e))?
        .reply()
        .map_err(|e| error(&e))?;
    let value = u8::from(enabled);
    let mut changed = Vec::new();
    for info in devices.infos {
        if ![
            DeviceType::SLAVE_POINTER,
            DeviceType::SLAVE_KEYBOARD,
            DeviceType::FLOATING_SLAVE,
        ]
        .contains(&info.type_)
        {
            continue;
        }
        let Some(node) = read8(info.deviceid, node_atom) else {
            continue;
        };
        let node = String::from_utf8_lossy(node.split(|b| *b == 0).next().unwrap_or_default())
            .into_owned();
        if !nodes.contains(&node) {
            continue;
        }
        connection
            .xinput_xi_change_property(
                info.deviceid,
                PropMode::REPLACE,
                enabled_atom,
                AtomEnum::INTEGER.into(),
                1,
                &XIChangePropertyAux::Data8(vec![value]),
            )
            .map_err(|e| error(&e))?
            .check()
            .map_err(|e| error(&e))?;
        if read8(info.deviceid, enabled_atom).and_then(|v| v.first().copied()) != Some(value) {
            return Err(format!(
                "X input device {node} did not take Device Enabled {value}"
            ));
        }
        changed.push(node);
    }
    let _ = connection.flush();
    Ok(changed)
}

/// Enables the journalled desk devices again; `false` when the X server
/// refused (they stay disabled, and journalled).
fn enable_x_devices(connection: &RustConnection, nodes: &[String]) -> bool {
    match set_x_devices_enabled(connection, nodes, true) {
        Ok(enabled) => {
            if !enabled.is_empty() {
                tracing::info!(
                    target: target::INPUT,
                    devices = enabled.len(),
                    "the desk's input devices are enabled in the session's X server again"
                );
            }
            true
        }
        Err(error) => {
            tracing::error!(
                target: target::INPUT,
                %error,
                "the desk's input devices could not be enabled in the session's X server again"
            );
            false
        }
    }
}

/// Every evdev node a person at the desk could drive the desktop with
/// ([`model::disables_while_attached`]).
fn desk_device_nodes() -> Vec<String> {
    let Ok(entries) = std::fs::read_dir("/dev/input") else {
        return Vec::new();
    };
    let mut nodes: Vec<String> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_str()?;
            (name.starts_with("event")
                && input_device_fact(name).is_some_and(model::disables_while_attached))
            .then(|| entry.path().to_string_lossy().into_owned())
        })
        .collect();
    nodes.sort();
    nodes
}

fn linear_ramp(crtc: u32, size: usize) -> SavedGamma {
    let step = |index: usize| -> u16 {
        if size <= 1 {
            return u16::MAX;
        }
        u16::try_from(index * usize::from(u16::MAX) / (size - 1)).unwrap_or(u16::MAX)
    };
    let ramp: Vec<u16> = (0..size).map(step).collect();
    SavedGamma {
        crtc,
        red: ramp.clone(),
        green: ramp.clone(),
        blue: ramp,
    }
}

fn restore_gamma(connection: &RustConnection, saved: &[SavedGamma]) {
    for ramp in saved {
        let result = connection
            .randr_set_crtc_gamma(ramp.crtc, &ramp.red, &ramp.green, &ramp.blue)
            .map_err(|error| error.to_string())
            .and_then(|cookie| cookie.check().map_err(|error| error.to_string()));
        if let Err(error) = result {
            tracing::error!(
                target: target::SESSION,
                crtc = ramp.crtc,
                %error,
                "a console monitor's own gamma could not be restored"
            );
        }
    }
    let _ = connection.flush();
}

/// The desk's own keyboards, mice, touch and tablets, grabbed (EVIOCGRAB)
/// so their events reach nobody. The kernel releases a grab when its
/// descriptor closes, so a launcher that dies gives the desk its input back
/// by itself.
#[derive(Default)]
struct GrabbedInput {
    devices: Vec<(PathBuf, u64, evdev::Device)>,
}

/// udev's classes of a keyboard or a pointer, as opposed to a device with
/// a few keys, buttons or switches (`ID_INPUT_KEY`, `ID_INPUT_SWITCH`).
const KEYBOARD_OR_POINTER_CLASSES: [&str; 7] = [
    "ID_INPUT_KEYBOARD",
    "ID_INPUT_MOUSE",
    "ID_INPUT_TOUCHPAD",
    "ID_INPUT_TABLET",
    "ID_INPUT_TOUCHSCREEN",
    "ID_INPUT_POINTINGSTICK",
    "ID_INPUT_TRACKBALL",
];

fn input_device_fact(event: &str) -> Option<InputDeviceFact> {
    let class = Path::new("/sys/class/input").join(event);
    let device = std::fs::canonicalize(class.join("device")).ok()?;
    let dev = std::fs::read_to_string(class.join("dev")).ok()?;
    let udev =
        std::fs::read_to_string(format!("/run/udev/data/c{}", dev.trim())).unwrap_or_default();
    let bustype = std::fs::read_to_string(class.join("device/id/bustype")).unwrap_or_default();
    Some(device_fact_from(&device, &udev, &bustype))
}

/// One device's facts from its sysfs path (`/sys/class/input/eventN/device`
/// resolved), its udev database record (`/run/udev/data/c<major>:<minor>`)
/// and its `id/bustype` (hexadecimal).
fn device_fact_from(device: &Path, udev: &str, bustype: &str) -> InputDeviceFact {
    let property = |key: &str| {
        udev.lines()
            .filter_map(|line| line.strip_prefix("E:"))
            .find_map(|line| line.strip_prefix(key)?.strip_prefix('='))
            .map(str::to_owned)
    };
    let set = |key: &&str| property(key).as_deref() == Some("1");
    let keyboard_or_pointer = KEYBOARD_OR_POINTER_CLASSES.iter().any(set);
    InputDeviceFact {
        virtual_device: device.starts_with("/sys/devices/virtual"),
        console_seat: property("ID_SEAT").is_none_or(|seat| seat == model::CONSOLE_SEAT),
        // Only Arcen's own udev rule marks a bridged device (a tag and
        // libinput ignoring it); a device's own strings (model, serial) are
        // never trusted for this.
        bridged: property("ID_INPUT.tags").is_some_and(|tags| {
            tags.split(',')
                .any(|tag| tag == arcen_usb_bridge::bridge_seat::XORG_BRIDGE_TAG)
        }) && property("LIBINPUT_IGNORE_DEVICE").as_deref() == Some("1"),
        drives_desktop: keyboard_or_pointer || set(&"ID_INPUT_KEY"),
        keyboard_or_pointer,
        // Unreadable reads as bus 0: never the platform bus, so it cannot
        // excuse a device.
        bus: u16::from_str_radix(bustype.trim(), 16).unwrap_or(0),
    }
}

/// Whether `event`'s device carries Arcen's virtual input vendor (Arcen's
/// own injectors).
fn arcen_input_device(event: &str) -> bool {
    std::fs::read_to_string(
        Path::new("/sys/class/input")
            .join(event)
            .join("device/id/vendor"),
    )
    .ok()
    .and_then(|vendor| u16::from_str_radix(vendor.trim(), 16).ok())
        == Some(arcen_input::virtual_device::VIRTUAL_INPUT_VENDOR_ID)
}

/// The kernel's interfaces for input devices made in user space, by their
/// node names and fixed misc-device numbers (`UINPUT_MINOR`, `UHID_MINOR`).
const USER_SPACE_INPUT_NODES: [(&str, u32, u32); 2] = [("uinput", 10, 223), ("uhid", 10, 239)];

/// A process holding a user-space input interface open.
#[derive(Debug, Clone, PartialEq, Eq)]
struct InputCreator {
    pid: u32,
    comm: String,
    /// `/proc/<pid>/exe` as the link reads (`… (deleted)` after an update
    /// replaced the file); `None` when unreadable.
    exe: Option<String>,
}

/// An executable path without the kernel's ` (deleted)` suffix.
fn executable_path(link: &str) -> &str {
    link.strip_suffix(" (deleted)").unwrap_or(link)
}

/// Whether `creator` is one of Arcen's own processes: the Pier and every
/// helper it starts run this one executable (also after an update replaced
/// the file under a running session). Read from the links' text only, so a
/// holder's executable on a hung network mount cannot stall the check.
fn arcen_creator(creator: &InputCreator, own_exe: Option<&str>) -> bool {
    matches!(
        (creator.exe.as_deref(), own_exe),
        (Some(theirs), Some(ours)) if executable_path(theirs) == executable_path(ours)
    )
}

/// Who holds the user-space input interfaces open, from the processes
/// found holding one (`None`: they could not all be read).
fn creators_from(
    holders: Option<&[InputCreator]>,
    own_exe: Option<&str>,
) -> model::UserSpaceInputCreators {
    match holders {
        None => model::UserSpaceInputCreators::Unknown,
        Some(holders) if holders.iter().all(|holder| arcen_creator(holder, own_exe)) => {
            model::UserSpaceInputCreators::ArcenOnly
        }
        Some(_) => model::UserSpaceInputCreators::Other,
    }
}

/// A device number's major and minor (glibc's `gnu_dev_major`/`minor`).
const fn device_major_minor(rdev: u64) -> (u32, u32) {
    #[allow(
        clippy::cast_possible_truncation,
        reason = "glibc's encoding keeps both numbers within 32 bits"
    )]
    let major = (((rdev >> 32) & 0xffff_f000) | ((rdev >> 8) & 0x0000_0fff)) as u32;
    #[allow(
        clippy::cast_possible_truncation,
        reason = "glibc's encoding keeps both numbers within 32 bits"
    )]
    let minor = (((rdev >> 12) & 0xffff_ff00) | (rdev & 0x0000_00ff)) as u32;
    (major, minor)
}

/// Whether descriptor `fd` (a `/proc/<pid>/fd/<n>` link reading `target`)
/// is open on uinput or uhid: by the node's name, then proved by its device
/// number (`rdev`, of the opened file), so a node elsewhere (a chroot) still
/// counts and nothing else is ever stat'ed.
fn holds_user_space_input(
    fd: &Path,
    target: &Path,
    rdev: &dyn Fn(&Path) -> std::io::Result<u64>,
) -> std::io::Result<bool> {
    let Some(name) = target.file_name().and_then(|name| name.to_str()) else {
        return Ok(false);
    };
    let Some(&(_, major, minor)) = USER_SPACE_INPUT_NODES
        .iter()
        .find(|(node, _, _)| *node == name)
    else {
        return Ok(false);
    };
    Ok(device_major_minor(rdev(fd)?) == (major, minor))
}

/// Every process under `proc_root` holding `/dev/uinput` or `/dev/uhid`
/// open; `None` when a process's descriptors could not be read (a process
/// or descriptor that ended meanwhile is skipped).
fn user_space_input_holders(
    proc_root: &Path,
    rdev: &dyn Fn(&Path) -> std::io::Result<u64>,
) -> Option<Vec<InputCreator>> {
    let gone = |error: &std::io::Error| error.kind() == std::io::ErrorKind::NotFound;
    let mut holders = Vec::new();
    for entry in std::fs::read_dir(proc_root).ok()?.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let base = entry.path();
        let descriptors = match std::fs::read_dir(base.join("fd")) {
            Ok(descriptors) => descriptors,
            Err(error) if gone(&error) => continue,
            Err(_) => return None,
        };
        let mut holds = false;
        for descriptor in descriptors {
            let fd = match descriptor {
                Ok(descriptor) => descriptor.path(),
                Err(error) if gone(&error) => continue,
                Err(_) => return None,
            };
            match std::fs::read_link(&fd)
                .and_then(|target| holds_user_space_input(&fd, &target, rdev))
            {
                Ok(true) => {
                    holds = true;
                    break;
                }
                Ok(false) => {}
                Err(error) if gone(&error) => {}
                Err(_) => return None,
            }
        }
        if !holds {
            continue;
        }
        let exe = std::fs::read_link(base.join("exe"))
            .ok()
            .map(|link| link.to_string_lossy().into_owned());
        let comm = std::fs::read_to_string(base.join("comm"))
            .map(|comm| comm.trim().to_owned())
            .unwrap_or_default();
        holders.push(InputCreator { pid, comm, exe });
    }
    holders.sort_by_key(|holder| holder.pid);
    Some(holders)
}

/// A short account of the user-space input creators outside Arcen, for
/// the log (`pcomm[pid]`, or that they could not be read).
fn outside_creators(holders: Option<&[InputCreator]>, own_exe: Option<&str>) -> String {
    let Some(holders) = holders else {
        return "user-space input creators unreadable".to_owned();
    };
    let names: Vec<String> = holders
        .iter()
        .filter(|holder| !arcen_creator(holder, own_exe))
        .map(|holder| format!("{}[{}]", holder.comm, holder.pid))
        .collect();
    format!("created through uinput/uhid by {}", names.join(", "))
}

/// The name of a keyboard or pointer of another remote-desktop product's
/// connected client, if one exists (shared `another_products_client_input`):
/// a parentless keyboard or pointer that is not Arcen's, not a firmware
/// device, while a process outside Arcen holds `/dev/uinput` or `/dev/uhid`.
/// Only asked about a desktop another product owns: there, that product's
/// client is using it now.
fn another_products_client_now() -> Option<String> {
    let entries = std::fs::read_dir("/sys/class/input").ok()?;
    let mut creators: Option<(Option<Vec<InputCreator>>, Option<String>)> = None;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(event) = name.to_str().filter(|name| name.starts_with("event")) else {
            continue;
        };
        let Some(fact) = input_device_fact(event) else {
            continue;
        };
        let arcen = arcen_input_device(event);
        // Who creates input in user space is read only once a device could
        // be another product's at all (a host with none never scans /proc).
        if !model::remote_client_input_candidate(fact, arcen) {
            continue;
        }
        let (holders, own_exe) = creators.get_or_insert_with(|| {
            (
                user_space_input_holders(Path::new("/proc"), &|fd| {
                    std::fs::metadata(fd).map(|metadata| metadata.rdev())
                }),
                std::fs::read_link("/proc/self/exe")
                    .ok()
                    .map(|link| link.to_string_lossy().into_owned()),
            )
        });
        let who = creators_from(holders.as_deref(), own_exe.as_deref());
        if model::another_products_client_input(fact, arcen, who) {
            let device_name = std::fs::read_to_string(entry.path().join("device/name"))
                .map(|name| name.trim().to_owned())
                .unwrap_or_default();
            return Some(format!(
                "{event} ({device_name}; bus {:#06x}; {})",
                fact.bus,
                outside_creators(holders.as_deref(), own_exe.as_deref())
            ));
        }
    }
    None
}

/// [`another_products_client_now`] off the async runtime (it reads sysfs,
/// udev and, once a device could be another product's, every process's
/// descriptors). A check that could not run counts as a client: one remote
/// product at a time is kept when in doubt.
#[allow(
    clippy::disallowed_methods,
    reason = "plain file reads that finish; nothing is left running when the caller stops waiting"
)]
pub(crate) async fn another_products_client() -> Option<String> {
    tokio::task::spawn_blocking(another_products_client_now)
        .await
        .unwrap_or_else(|error| Some(format!("the check could not run: {error}")))
}

impl GrabbedInput {
    /// Grabs every device of the desk not grabbed yet; returns how many it
    /// grabbed now.
    fn grab_new(&mut self) -> Result<usize, String> {
        let entries = std::fs::read_dir("/dev/input").map_err(|error| error.to_string())?;
        let mut grabbed = 0;
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str().filter(|name| name.starts_with("event")) else {
                continue;
            };
            let path = entry.path();
            let Ok(metadata) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if !metadata.file_type().is_char_device() {
                continue;
            }
            let rdev = metadata.rdev();
            if self
                .devices
                .iter()
                .any(|(known, known_rdev, _)| *known == path && *known_rdev == rdev)
            {
                continue;
            }
            if !input_device_fact(name).is_some_and(model::disables_while_attached) {
                continue;
            }
            let mut device = evdev::Device::open(&path)
                .map_err(|error| format!("open {}: {error}", path.display()))?;
            device
                .grab()
                .map_err(|error| format!("grab {}: {error}", path.display()))?;
            tracing::info!(
                target: target::INPUT,
                device = %path.display(),
                name = device.name().unwrap_or("unnamed"),
                "the desk's input device is disabled while a Deck is attached"
            );
            self.devices.push((path, rdev, device));
            grabbed += 1;
        }
        // A device that went away (unplugged) is forgotten.
        self.devices.retain(|(path, rdev, _)| {
            std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.rdev() == *rdev)
        });
        Ok(grabbed)
    }

    fn release(&mut self) {
        for (_, _, mut device) in self.devices.drain(..) {
            let _ = device.ungrab();
        }
    }
}

impl Drop for GrabbedInput {
    fn drop(&mut self) {
        self.release();
    }
}

/// The console made private for an attached Deck: desk input disabled,
/// desk monitors blank.
#[derive(Default)]
pub(crate) struct ConsolePrivacy {
    input: GrabbedInput,
    outputs: Option<BlankedOutputs>,
    /// A session was brought to the desk's monitors and not yet handed
    /// back secure.
    brought_forward: bool,
    /// The Deck's layout on the console's outputs (ADR 0025 §6), restored
    /// before the monitors are shown again.
    layout: Option<super::console_layout::AppliedLayout>,
    /// The layout is kept through a hand-back while a Deck is held for its
    /// resume ([`Self::hold_layout`]).
    layout_held: bool,
    /// The session agent's audio lease (the desk's speakers silent while a
    /// Deck is attached), given back with the desk ([`DetachStep::RestoreAudio`]).
    ///
    /// [`DetachStep::RestoreAudio`]: arcen_session::console_session::DetachStep::RestoreAudio
    audio: Option<super::agent::ConsoleAudioControl>,
    /// The agent holds the lease now.
    audio_held: bool,
    /// The administrator turned desk privacy off (`privacy.desk = "off"`):
    /// the joined session's desk has its monitors and input while the Deck
    /// is attached ([`Self::show_desk`]). Anything that makes the desk
    /// private again (a hand-back, a resume, a failure) ends it.
    desk_live: bool,
}

impl ConsolePrivacy {
    /// For an administrator who turned desk privacy off: once the session
    /// is joined (and after each resume), the desk gets its monitors (their
    /// own ramps, exactly as journalled) and input back. The session stays
    /// "armed" so the Deck's departure still locks it before anything else
    /// (`DETACH_STEPS`); a session that will not lock is made private again.
    pub(crate) fn show_desk(&mut self) {
        if let Some(outputs) = self.outputs.as_ref() {
            restore_gamma(&outputs.connection, &outputs.journal.gamma);
        }
        self.input.release();
        self.desk_live = true;
        tracing::warn!(
            target: target::SESSION,
            "desk privacy is off (privacy.desk): the desk's monitors and input stay live while the Deck is attached"
        );
    }

    /// Ends [`Self::show_desk`]: the next [`Self::enforce`] blanks the
    /// monitors and disables the desk's input again.
    fn end_live_desk(&mut self) {
        self.desk_live = false;
    }

    /// Disables the desk's input (before a remote-first sign-in, so nobody
    /// at the desk can use the session the login screen is about to
    /// start).
    pub(crate) fn disable_input(&mut self) -> Result<(), ConsoleRefusal> {
        self.desk_live = false;
        self.input.grab_new().map(drop).map_err(|error| {
            tracing::error!(target: target::SESSION, %error, "the desk's input could not be disabled");
            ConsoleRefusal::PrivacyUnavailable
        })
    }

    /// Makes the console private: input disabled, monitors blank. A Deck
    /// is attached again: the next hand-back's failed lock starts its own
    /// grace ([`model::UNLOCKED_GRACE`]).
    pub(crate) fn arm(
        &mut self,
        display: &SessionDisplay,
        session_id: &str,
    ) -> Result<(), ConsoleRefusal> {
        self.disable_input()?;
        self.blank(display, session_id)?;
        UnlockedSince::clear();
        Ok(())
    }

    /// Blanks the desk's monitors (proving them blank again when they
    /// already are).
    pub(crate) fn blank(
        &mut self,
        display: &SessionDisplay,
        session_id: &str,
    ) -> Result<(), ConsoleRefusal> {
        self.desk_live = false;
        if let Some(outputs) = self.outputs.as_mut() {
            // Already blank: prove it again (a VT switch, a monitor that
            // came back).
            outputs.enforce().map_err(|error| {
                tracing::error!(target: target::SESSION, %error, "the desk's monitors could not be kept blank");
                ConsoleRefusal::PrivacyUnavailable
            })?;
        } else {
            let outputs = BlankedOutputs::blank(display, session_id).map_err(|error| {
                tracing::error!(target: target::SESSION, %error, "the desk's monitors could not be blanked");
                ConsoleRefusal::PrivacyUnavailable
            })?;
            self.outputs = Some(outputs);
        }
        Ok(())
    }

    /// Keeps it private: a monitor turned on, a colour daemon that reset a
    /// ramp, a keyboard plugged in.
    pub(crate) fn enforce(&mut self) {
        if self.desk_live {
            return;
        }
        if let Some(outputs) = self.outputs.as_mut() {
            match outputs.enforce() {
                Ok(0) => {}
                Ok(count) => tracing::warn!(
                    target: target::SESSION,
                    crtcs = count,
                    "a desk monitor came back on while a Deck is attached; blanked again"
                ),
                Err(error) => tracing::error!(
                    target: target::SESSION,
                    %error,
                    "the desk's monitors could not be kept blank"
                ),
            }
        }
        if let Err(error) = self.input.grab_new() {
            tracing::error!(target: target::SESSION, %error, "a new desk input device could not be disabled");
        }
    }

    /// Lays the console out for the Deck (its monitors already blank),
    /// then blanks any CRTC the layout lit at once.
    pub(crate) async fn apply_layout(
        &mut self,
        display: &SessionDisplay,
        session_id: &str,
        uid: u32,
        requests: &[arcen_outputs::console_layout::ConsoleHeadRequest],
    ) -> Result<(), super::console_layout::LayoutFailure> {
        self.layout_held = false;
        if let Some(previous) = self.layout.take() {
            previous.restore().await;
            self.enforce();
        }
        let work = async {
            super::console_layout::wait_for_desktop_settled(uid).await;
            let mut applied = super::console_layout::apply(display, session_id, requests).await?;
            let settled = applied.settle().await;
            if settled.is_ok() {
                applied.note_settled_windows();
            }
            match settled {
                Ok(0) => {}
                Ok(times) => tracing::info!(
                    target: target::SESSION,
                    times,
                    "the console's layout was applied again while the desktop settled"
                ),
                Err(failure) => {
                    applied.restore().await;
                    return Err(failure);
                }
            }
            Ok(applied)
        };
        let applied = self.enforcing_while(work).await;
        // A head the layout switched on (or a restore switched back on)
        // starts with its own ramps.
        self.enforce();
        let applied = applied?;
        tracing::debug!(target: target::SESSION, heads = applied.plan().heads.len(), "console layout held");
        self.layout = Some(applied);
        Ok(())
    }

    /// Runs `work` while keeping the desk private: the monitors are proved
    /// blank (and the desk's input grabbed) every 250 ms until it is done.
    async fn enforcing_while<T>(&mut self, work: impl std::future::Future<Output = T>) -> T {
        tokio::pin!(work);
        let mut tick = tokio::time::interval(std::time::Duration::from_millis(250));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                result = &mut work => return result,
                _ = tick.tick() => self.enforce(),
            }
        }
    }

    /// For a returning Deck: the layout held through its absence is proved
    /// (and applied again if the desktop changed it), or laid out afresh
    /// when none is held; before the session is unlocked.
    pub(crate) async fn refresh_layout(
        &mut self,
        display: &SessionDisplay,
        session_id: &str,
        uid: u32,
        requests: &[arcen_outputs::console_layout::ConsoleHeadRequest],
    ) -> Result<(), super::console_layout::LayoutFailure> {
        self.layout_held = false;
        let Some(layout) = self.layout.take() else {
            return self.apply_layout(display, session_id, uid, requests).await;
        };
        let settled = self.enforcing_while(layout.settle()).await;
        self.enforce();
        match settled {
            Ok(_) => {
                self.layout = Some(layout);
                Ok(())
            }
            Err(failure) => {
                self.enforcing_while(layout.restore()).await;
                self.enforce();
                Err(failure)
            }
        }
    }

    /// Keeps the Deck's layout through the hand-back of a Deck that is held
    /// for its resume (the console stays in it, locked, until the hold ends
    /// and the launcher leaves, or the Deck returns).
    pub(crate) fn hold_layout(&mut self) {
        self.layout_held = self.layout.is_some();
    }

    /// Ends a resume hold: the next hand-back restores the console's layout.
    pub(crate) fn release_layout_hold(&mut self) {
        self.layout_held = false;
    }

    /// Puts the console's own layout back (journalled; a failure leaves the
    /// journal for the Pier's recovery). A layout held for a resume stays.
    pub(crate) async fn restore_layout(&mut self) {
        if self.layout_held {
            return;
        }
        if let Some(layout) = self.layout.take() {
            layout.restore().await;
        }
    }

    pub(crate) fn restore_outputs(&mut self) {
        if let Some(outputs) = self.outputs.take() {
            outputs.restore();
        }
    }

    pub(crate) fn release_input(&mut self) {
        self.input.release();
    }

    /// The session agent's audio lease, held from the agent's start.
    pub(crate) fn hold_audio_with(&mut self, control: Option<super::agent::ConsoleAudioControl>) {
        self.audio_held = control.is_some();
        self.audio = control;
    }

    /// Gives the desk its sound back: the agent ends its lease, so the
    /// default sink and mutes are the desk's own again.
    pub(crate) async fn restore_audio(&mut self) {
        if !self.audio_held {
            return;
        }
        let Some(control) = self.audio.as_mut() else {
            return;
        };
        self.audio_held = false;
        if control.release().await {
            tracing::info!(
                target: target::AUDIO,
                "the desk's sound is given back: Arcen's audio lease ended"
            );
        } else {
            tracing::error!(
                target: target::AUDIO,
                "the session agent did not confirm giving the desk its sound back; it is restored when the agent ends"
            );
        }
    }

    /// Silences the desk again for a returning Deck. Whether the agent
    /// holds the lease with the evidence the operator's policy requires (a
    /// console without an audio link has nothing to take).
    pub(crate) async fn retake_audio(&mut self) -> bool {
        let Some(control) = self.audio.as_mut() else {
            return true;
        };
        if self.audio_held {
            return true;
        }
        self.audio_held = control.hold().await;
        self.audio_held
    }

    pub(crate) fn is_armed(&self) -> bool {
        self.outputs.is_some()
    }

    /// Whether any desk input device is still grabbed (an attachment that
    /// failed between disabling the input and blanking the monitors).
    pub(crate) fn holds_input(&self) -> bool {
        !self.input.devices.is_empty()
    }

    /// Whether the console must still be handed back before the launcher
    /// may leave: blank, holding the desk's input, or showing a session
    /// this launcher brought forward.
    pub(crate) fn needs_hand_back(&self) -> bool {
        self.is_armed() || self.holds_input() || self.brought_forward || self.layout.is_some()
    }

    /// Records that a session is about to be (or was) put on the desk's
    /// monitors by this launcher: it is handed back before the launcher
    /// leaves.
    pub(crate) fn mark_brought_forward(&mut self) {
        self.brought_forward = true;
    }

    pub(crate) fn is_brought_forward(&self) -> bool {
        self.brought_forward
    }

    /// For a session that cannot be secured: the session's X server is
    /// told to ignore the desk's devices (grabbed now, and any other the
    /// desk has), journalled, so the desk stays unusable however this
    /// launcher ends. Only a recovery that secured the session undoes it.
    pub(crate) fn keep_desk_disabled_beyond_launcher(&mut self) {
        let Some(outputs) = self.outputs.as_mut() else {
            tracing::error!(
                target: target::SESSION,
                "the session's display is unknown; the desk's input stays disabled only while this launcher runs"
            );
            return;
        };
        let mut nodes: Vec<String> = self
            .input
            .devices
            .iter()
            .map(|(path, _, _)| path.to_string_lossy().into_owned())
            .collect();
        for node in desk_device_nodes() {
            if !nodes.contains(&node) {
                nodes.push(node);
            }
        }
        match outputs.disable_x_devices(&nodes) {
            Ok(0) => {}
            Ok(count) => tracing::warn!(
                target: target::SESSION,
                devices = count,
                "the session could not be secured; the desk's input is disabled in its X server until it is"
            ),
            Err(error) => tracing::error!(
                target: target::SESSION,
                %error,
                "the desk's input could not be disabled in the session's X server"
            ),
        }
    }
}

impl Drop for ConsolePrivacy {
    /// The kernel would release the grabs anyway. The monitors stay blank
    /// and journalled: only [`hand_back`] or the Pier's [`recover_console`]
    /// show them again, after the session is locked.
    fn drop(&mut self) {
        self.release_input();
    }
}

/// Locks a console session and waits, bounded, for its lock screen.
pub(crate) async fn lock_session(session_id: &str) -> bool {
    if !safe_session_id(session_id) {
        return false;
    }
    let mut lock = Command::new(LOGINCTL);
    lock.args(["lock-session", session_id]);
    if bounded_output(lock).await.is_none() {
        tracing::error!(target: target::SESSION, session_id, "the console session could not be locked");
        return false;
    }
    let deadline = tokio::time::Instant::now() + LOCK_SCREEN_TIMEOUT;
    while tokio::time::Instant::now() < deadline {
        if session_fact(session_id)
            .await
            .is_some_and(|fact| fact.locked)
        {
            tracing::info!(target: target::SESSION, session_id, "console session locked");
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    tracing::warn!(
        target: target::SESSION,
        session_id,
        "the console session did not report its lock screen in time"
    );
    false
}

/// Brings a session waiting behind the login screen to the foreground.
pub(crate) async fn activate_session(session_id: &str) -> bool {
    if !safe_session_id(session_id) {
        return false;
    }
    let mut activate = Command::new(BUSCTL);
    activate.args([
        "call",
        "org.freedesktop.login1",
        "/org/freedesktop/login1",
        "org.freedesktop.login1.Manager",
        "ActivateSessionOnSeat",
        "ss",
        session_id,
        model::CONSOLE_SEAT,
    ]);
    if bounded_output(activate).await.is_none() {
        return false;
    }
    for _ in 0..25 {
        if session_fact(session_id)
            .await
            .is_some_and(|fact| fact.active)
        {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    false
}

/// Asks GDM for a login screen on a new virtual terminal ("switch
/// user"): the session moves to the background, where only its own
/// password brings it back.
async fn switch_to_login_screen() -> bool {
    let mut switch = Command::new(BUSCTL);
    switch.args([
        "call",
        "org.gnome.DisplayManager",
        "/org/gnome/DisplayManager/LocalDisplayFactory",
        "org.gnome.DisplayManager.LocalDisplayFactory",
        "CreateTransientDisplay",
    ]);
    bounded_output(switch).await.is_some()
}

/// What logind says about a session, owned (see [`model::SessionLookup`]).
pub(crate) enum SessionState {
    Known {
        fact: Box<ConsoleSessionFact>,
        display_alive: bool,
    },
    Unknown,
    Failed,
}

impl SessionState {
    pub(crate) fn lookup(&self) -> model::SessionLookup<'_> {
        match self {
            Self::Known {
                fact,
                display_alive,
            } => model::SessionLookup::Known {
                fact,
                display_alive: *display_alive,
            },
            Self::Unknown => model::SessionLookup::Unknown,
            Self::Failed => model::SessionLookup::Failed,
        }
    }

    fn hand_back(&self) -> model::UnlockedHandBack {
        model::unlocked_hand_back(self.lookup())
    }

    fn hand_back_after(&self, unlocked_for: Duration) -> model::UnlockedHandBack {
        model::unlocked_hand_back_after(self.lookup(), unlocked_for)
    }
}

/// Where logind keeps a state file for every session it knows.
const LOGIND_SESSIONS: &str = "/run/systemd/sessions";

/// Asks logind about `session_id`; a session it cannot describe is gone
/// only when its state file is gone too (a failed or slow `loginctl` proves
/// nothing).
pub(crate) async fn session_state(session_id: &str) -> SessionState {
    if let Some(fact) = session_fact(session_id).await {
        let display_alive = if fact.origin() == model::ConsoleOrigin::Arcen {
            session_display_alive(session_id).await
        } else {
            true
        };
        return SessionState::Known {
            fact: Box::new(fact),
            display_alive,
        };
    }
    if !safe_session_id(session_id) {
        return SessionState::Failed;
    }
    match std::fs::symlink_metadata(Path::new(LOGIND_SESSIONS).join(session_id)) {
        Err(error)
            if error.kind() == std::io::ErrorKind::NotFound
                && Path::new(LOGIND_SESSIONS).is_dir() =>
        {
            SessionState::Unknown
        }
        _ => SessionState::Failed,
    }
}

/// Whether the X server of `session_id` (logind `Display=`) still accepts
/// connections; `true` whenever that cannot be ruled out.
async fn session_display_alive(session_id: &str) -> bool {
    let mut show = Command::new(LOGINCTL);
    show.args(["show-session", session_id, "-p", "Display", "--value"]);
    let Some(output) = bounded_output(show).await else {
        return true;
    };
    let display = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let Some(number) = model::local_display_number(&display) else {
        return true;
    };
    let path = format!("/tmp/.X11-unix/X{number}");
    if std::os::unix::net::UnixStream::connect(&path).is_ok() {
        return true;
    }
    {
        use std::os::linux::net::SocketAddrExt as _;
        if let Ok(address) = std::os::unix::net::SocketAddr::from_abstract_name(path.as_bytes()) {
            if std::os::unix::net::UnixStream::connect_addr(&address).is_ok() {
                return true;
            }
        }
    }
    false
}

/// Whether `session_id` may be shown at the desk again: locked, gone, or
/// another product's session no longer in the foreground. A session that
/// will not lock is sent behind a login screen; a display manager's stays
/// private there until it locks (a key chord would switch to it), and
/// Arcen's own login session, and one logind could not describe, are never
/// given back unlocked ([`model::UnlockedHandBack::KeepPrivate`]).
pub(crate) async fn secure_session(session_id: &str) -> bool {
    if lock_session(session_id).await {
        UnlockedSince::clear();
        return true;
    }
    let unlocked_for = UnlockedSince::of(session_id);
    let secured = secure_unlocked(session_id, unlocked_for).await;
    if secured {
        UnlockedSince::clear();
    }
    secured
}

/// When Arcen first failed to lock a console session (root-only, survives
/// launchers and Pier restarts), for [`model::UNLOCKED_GRACE`].
#[derive(Debug, Serialize, Deserialize)]
struct UnlockedSince {
    session_id: String,
    since: u64,
}

impl UnlockedSince {
    const PATH: &'static str = "/run/arcen/console-unlocked.json";

    fn now() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs())
    }

    /// How long `session_id` has stayed unlocked; starts the clock now
    /// for a session not seen before.
    fn of(session_id: &str) -> Duration {
        let now = Self::now();
        let recorded = read_root_journal(Path::new(Self::PATH))
            .and_then(|text| serde_json::from_slice::<Self>(&text).ok())
            .filter(|record| record.session_id == session_id && record.since <= now);
        if let Some(record) = recorded {
            return Duration::from_secs(now - record.since);
        }
        let record = Self {
            session_id: session_id.to_owned(),
            since: now,
        };
        if let Ok(text) = serde_json::to_vec(&record) {
            let _ = write_root_journal(Path::new(Self::PATH), &text);
        }
        Duration::ZERO
    }

    fn clear() {
        let _ = std::fs::remove_file(Self::PATH);
    }
}

async fn secure_unlocked(session_id: &str, unlocked_for: Duration) -> bool {
    match session_state(session_id)
        .await
        .hand_back_after(unlocked_for)
    {
        model::UnlockedHandBack::Gone | model::UnlockedHandBack::InBackground => return true,
        model::UnlockedHandBack::KeepPrivate => return false,
        model::UnlockedHandBack::BehindLoginScreen => {}
    }
    if switch_to_login_screen().await {
        for _ in 0..20 {
            match session_state(session_id)
                .await
                .hand_back_after(unlocked_for)
            {
                model::UnlockedHandBack::Gone | model::UnlockedHandBack::InBackground => {
                    tracing::warn!(
                        target: target::SESSION,
                        session_id,
                        "the console session did not lock; a login screen covers it instead"
                    );
                    return true;
                }
                model::UnlockedHandBack::KeepPrivate => {
                    tracing::warn!(
                        target: target::SESSION,
                        session_id,
                        "the console session did not lock; the login screen is in front and the session stays private behind it until it locks (at most a minute)"
                    );
                    return false;
                }
                model::UnlockedHandBack::BehindLoginScreen => {}
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
    false
}

/// Whether `session_id` is Arcen's own login session, which a host without
/// a display manager cannot lock: kept private by design, not a failure.
pub(crate) async fn kept_private_by_design(session_id: &str) -> bool {
    session_fact(session_id)
        .await
        .is_some_and(|fact| fact.origin() == model::ConsoleOrigin::Arcen)
}

/// Ends a session nobody has seen yet (Arcen's own login session that
/// could not be made private): its processes are stopped by logind.
pub(crate) async fn terminate_session(session_id: &str) -> bool {
    if !safe_session_id(session_id) {
        return false;
    }
    let mut terminate = Command::new(LOGINCTL);
    terminate.args(["terminate-session", session_id]);
    if bounded_output(terminate).await.is_none() {
        return false;
    }
    for _ in 0..50 {
        if matches!(
            session_state(session_id).await.hand_back(),
            model::UnlockedHandBack::Gone
        ) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    false
}

/// [`secure_session`], keeping the desk private every second meanwhile
/// (securing can take seconds; a keyboard plugged in then is grabbed, a
/// monitor turned on blanked).
async fn secure_keeping_private(privacy: &mut ConsolePrivacy, session_id: &str) -> bool {
    let secure = secure_session(session_id);
    tokio::pin!(secure);
    let mut tick = tokio::time::interval(UNUSABLE_TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            secured = &mut secure => return secured,
            _ = tick.tick() => privacy.enforce(),
        }
    }
}

/// Hands the console back in [`model::DETACH_STEPS`] order; the session
/// lives on. Nothing is shown or given back before the session is secure
/// ([`secure_session`]): if it is not, the desk stays private and `false`
/// is returned so the caller tries again.
pub(crate) async fn hand_back(privacy: &mut ConsolePrivacy, session_id: &str) -> bool {
    // A desk shown while attached (privacy.desk = "off") is made private
    // while the session locks, as any other: a session that will not lock
    // is never left open at the desk.
    privacy.end_live_desk();
    for step in model::DETACH_STEPS {
        match step {
            model::DetachStep::LockSession => {
                if !secure_keeping_private(privacy, session_id).await {
                    if kept_private_by_design(session_id).await {
                        tracing::info!(
                            target: target::SESSION,
                            session_id,
                            "the login session stays private: without a display manager it has no lock screen, so its monitors stay blank and the desk's input disabled until its account signs in through Arcen again or signs out"
                        );
                    } else {
                        tracing::error!(
                            target: target::SESSION,
                            session_id,
                            "the console session could not be locked; the desk stays blank and its input disabled"
                        );
                    }
                    // From the first failure on, also beyond this launcher:
                    // a grab ends with the process, however it ends.
                    privacy.keep_desk_disabled_beyond_launcher();
                    return false;
                }
            }
            // `lock_session` already waited for the lock screen.
            model::DetachStep::AwaitLockScreen => {}
            // The console's own layout comes back first, while the monitors
            // are still blank; then their own ramps.
            model::DetachStep::RestoreOutputs => {
                privacy.restore_layout().await;
                privacy.restore_outputs();
            }
            model::DetachStep::RestoreAudio => privacy.restore_audio().await,
            model::DetachStep::ReleaseInput => privacy.release_input(),
        }
    }
    privacy.brought_forward = false;
    tracing::info!(
        target: target::SESSION,
        session_id,
        "the console is handed back to the desk locked; the session continues"
    );
    true
}

/// Hands the console back after a launcher that did not: called by the
/// Pier when a console launcher has exited, at service start, and every
/// [`RECOVERY_RETRY`] while a console stays unsecured
/// ([`guard_unsecured_console`]). Restores the journalled monitors' own
/// ramps and the desk's devices once their session is secure; while it
/// cannot be, the desk is kept unusable: its monitors blank again and its
/// devices (newly plugged ones too) disabled in the session's X server.
/// Returns whether nothing is left to recover.
pub(crate) async fn recover_console() -> bool {
    let _recovery = RECOVERY.lock().await;
    recover(false).await
}

/// One recovery at a time, and none while a console launcher is handed
/// the gate ([`console_gate`] waits for a recovery in progress).
static RECOVERY: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Callers hold [`RECOVERY`].
async fn recover(retry: bool) -> bool {
    recover_with(&mut LiveRecovery, retry).await
}

/// What recovering a console left by a launcher that did not hand it back
/// does to the console ([`recover_with`]); the live one is
/// [`LiveRecovery`].
trait ConsoleRecovery {
    fn read_journal(&mut self) -> Option<PrivacyJournal>;
    /// The session of a layout journal with no privacy journal beside it.
    fn layout_session(&mut self) -> Option<String>;
    /// Blanks the journalled monitors again and disables the desk's
    /// devices; never restores anything.
    fn keep_desk_unusable(&mut self, journal: &mut PrivacyJournal);
    /// Locks the session (proved), keeping the desk unusable meanwhile.
    async fn secure(&mut self, session_id: &str, journal: Option<&mut PrivacyJournal>) -> bool;
    async fn kept_private_by_design(&mut self, session_id: &str) -> bool;
    /// Restores the console's own layout (and windows).
    async fn restore_layout(&mut self);
    /// Gives the monitors their own ramps and the desk its devices;
    /// `false` when a device stayed disabled.
    fn give_back(&mut self, journal: &PrivacyJournal) -> bool;
    fn remove_journal(&mut self);
}

/// Recovers a console a launcher left private or laid out: the session is
/// secured (locked) before anything is given back, and while it cannot be,
/// the desk stays unusable and the journal stays. Returns whether nothing
/// is left to recover.
async fn recover_with(host: &mut impl ConsoleRecovery, retry: bool) -> bool {
    let Some(mut journal) = host.read_journal() else {
        // Monitors already shown again but the layout not restored (the
        // launcher died between the two): the session is secured first.
        if let Some(session_id) = host.layout_session() {
            if host.secure(&session_id, None).await {
                host.restore_layout().await;
            }
        }
        return true;
    };
    // Unusable first: securing the session can take seconds, and nothing
    // grabs the desk's devices while no launcher runs.
    host.keep_desk_unusable(&mut journal);
    if !retry {
        tracing::warn!(
            target: target::SESSION,
            session_id = %journal.session_id,
            "a console was left private by a launcher that did not hand it back; locking it and restoring its monitors"
        );
    }
    let session_id = journal.session_id.clone();
    if !host.secure(&session_id, Some(&mut journal)).await {
        if !retry {
            if host.kept_private_by_design(&session_id).await {
                tracing::info!(
                    target: target::SESSION,
                    session_id = %session_id,
                    "the login session stays private (no lock screen without a display manager) until its account signs in through Arcen again or signs out"
                );
            } else {
                tracing::error!(
                    target: target::SESSION,
                    session_id = %session_id,
                    "the console session could not be locked; its monitors stay blank and the desk's input disabled until it is (retried every 10 s; or sign the session out)"
                );
            }
        }
        host.keep_desk_unusable(&mut journal);
        return false;
    }
    // The layout while the monitors are still blank (ADR 0025 §6).
    host.restore_layout().await;
    if !host.give_back(&journal) {
        return false;
    }
    host.remove_journal();
    tracing::info!(
        target: target::SESSION,
        session_id = %session_id,
        "the console left private is secured and handed back to the desk"
    );
    true
}

/// The console itself: X, logind, the root journals.
struct LiveRecovery;

impl ConsoleRecovery for LiveRecovery {
    fn read_journal(&mut self) -> Option<PrivacyJournal> {
        PrivacyJournal::read()
    }

    fn layout_session(&mut self) -> Option<String> {
        super::console_layout::journalled_session()
    }

    fn keep_desk_unusable(&mut self, journal: &mut PrivacyJournal) {
        keep_desk_unusable(journal);
    }

    async fn secure(&mut self, session_id: &str, journal: Option<&mut PrivacyJournal>) -> bool {
        let Some(journal) = journal else {
            return secure_session(session_id).await;
        };
        let secure = secure_session(session_id);
        tokio::pin!(secure);
        let mut tick = tokio::time::interval(UNUSABLE_TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tick.tick().await;
        loop {
            tokio::select! {
                secured = &mut secure => break secured,
                _ = tick.tick() => keep_desk_unusable(journal),
            }
        }
    }

    async fn kept_private_by_design(&mut self, session_id: &str) -> bool {
        kept_private_by_design(session_id).await
    }

    async fn restore_layout(&mut self) {
        super::console_layout::recover().await;
    }

    fn give_back(&mut self, journal: &PrivacyJournal) -> bool {
        match connect(&journal.display) {
            Ok(connection) => {
                restore_gamma(&connection, &journal.gamma);
                enable_x_devices(&connection, &journal.x_disabled)
            }
            Err(error) => {
                tracing::warn!(
                    target: target::SESSION,
                    %error,
                    "the console's display is gone; nothing to restore"
                );
                true
            }
        }
    }

    fn remove_journal(&mut self) {
        PrivacyJournal::remove();
    }
}

/// Blanks the journalled display again and disables the desk's devices in
/// its X server, journalling what it changed; never restores anything.
fn keep_desk_unusable(journal: &mut PrivacyJournal) {
    let Ok(connection) = connect(&journal.display) else {
        return;
    };
    let mut changed = false;
    if let Some(root) = connection.setup().roots.first().map(|screen| screen.root) {
        let crtcs = connection
            .randr_get_screen_resources_current(root)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .map(|reply| reply.crtcs)
            .unwrap_or_default();
        for crtc in crtcs {
            let Some(current) = connection
                .randr_get_crtc_gamma(crtc)
                .ok()
                .and_then(|cookie| cookie.reply().ok())
            else {
                continue;
            };
            let blank = current
                .red
                .iter()
                .chain(&current.green)
                .chain(&current.blue)
                .all(|v| *v == 0);
            if !journal.gamma.iter().any(|saved| saved.crtc == crtc) {
                journal.gamma.push(if blank {
                    linear_ramp(crtc, current.red.len())
                } else {
                    SavedGamma {
                        crtc,
                        red: current.red.clone(),
                        green: current.green.clone(),
                        blue: current.blue.clone(),
                    }
                });
                changed = true;
            }
            if !blank && !current.red.is_empty() {
                let zeros = vec![0_u16; current.red.len()];
                let _ = connection
                    .randr_set_crtc_gamma(crtc, &zeros, &zeros, &zeros)
                    .map(|cookie| cookie.check());
                tracing::warn!(
                    target: target::SESSION,
                    crtc,
                    "a monitor of the unsecured console came back on; blanked again"
                );
            }
        }
    }
    match set_x_devices_enabled(&connection, &desk_device_nodes(), false) {
        Ok(nodes) => {
            for node in nodes {
                if !journal.x_disabled.contains(&node) {
                    tracing::warn!(
                        target: target::SESSION,
                        device = %node,
                        "the desk's input device is disabled in the unsecured console's X server"
                    );
                    journal.x_disabled.push(node);
                    changed = true;
                }
            }
        }
        Err(error) => tracing::error!(
            target: target::SESSION,
            %error,
            "the desk's input could not be disabled in the unsecured console's X server"
        ),
    }
    if changed {
        if let Err(error) = journal.write() {
            tracing::error!(target: target::SESSION, %error, "the console privacy journal could not be updated");
        }
    }
}

/// How often the Pier retries a console it could not secure.
const RECOVERY_RETRY: Duration = Duration::from_secs(10);
/// How often, meanwhile, it keeps the desk unusable (as a launcher's
/// `enforce` does): a re-plugged keyboard, a monitor turned on.
const UNUSABLE_TICK: Duration = Duration::from_secs(1);

/// One console launcher, or one recovery, at a time: a launcher holds it
/// from its open request to the end of its own clean-up, so a recovery
/// never touches a console a launcher is using.
static CONSOLE_GATE: std::sync::LazyLock<std::sync::Arc<tokio::sync::Mutex<()>>> =
    std::sync::LazyLock::new(|| std::sync::Arc::new(tokio::sync::Mutex::new(())));

/// The launcher process that holds [`CONSOLE_GATE`] (0: none, or not
/// spawned yet).
static GATE_HOLDER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
/// Its start time (`/proc/<pid>/stat` field 22), so a reused pid is not
/// taken for it.
static GATE_HOLDER_START: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A process's state letter and start time from `/proc/<pid>/stat`.
fn process_state(pid: u32) -> Option<(String, u64)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, after) = stat.rsplit_once(')')?;
    let fields: Vec<&str> = after.split_whitespace().collect();
    // After the command: state is field 3, start time field 22.
    Some((fields.first()?.to_string(), fields.get(19)?.parse().ok()?))
}

/// [`CONSOLE_GATE`], held by a console launcher for its whole life.
pub(crate) struct ConsoleGate {
    _guard: tokio::sync::OwnedMutexGuard<()>,
}

impl ConsoleGate {
    /// Names the launcher process holding the gate: once it has exited,
    /// the Pier's retries no longer wait for its clean-up.
    pub(crate) fn held_by(&self, pid: Option<u32>) {
        let start = pid.and_then(process_state).map_or(0, |(_, start)| start);
        GATE_HOLDER_START.store(start, std::sync::atomic::Ordering::SeqCst);
        GATE_HOLDER.store(pid.unwrap_or(0), std::sync::atomic::Ordering::SeqCst);
    }
}

impl Drop for ConsoleGate {
    fn drop(&mut self) {
        GATE_HOLDER.store(0, std::sync::atomic::Ordering::SeqCst);
    }
}

pub(crate) async fn console_gate() -> ConsoleGate {
    let guard = std::sync::Arc::clone(&CONSOLE_GATE).lock_owned().await;
    GATE_HOLDER.store(0, std::sync::atomic::Ordering::SeqCst);
    // A recovery the Pier started beside a launcher that had died finishes
    // before the next launcher touches the console.
    drop(RECOVERY.lock().await);
    ConsoleGate { _guard: guard }
}

/// Whether the launcher holding the gate has exited (gone, or a zombie
/// its broker has not reaped yet): its grabs are gone with it.
fn gate_holder_exited() -> bool {
    let pid = GATE_HOLDER.load(std::sync::atomic::Ordering::SeqCst);
    if pid == 0 {
        return false;
    }
    let start = GATE_HOLDER_START.load(std::sync::atomic::Ordering::SeqCst);
    process_state(pid).is_none_or(|(state, started)| {
        state == "Z" || state == "X" || (start != 0 && started != start)
    })
}

/// The Pier's watch over a console left unsecured: while its journal
/// stays and no console launcher runs, the session is tried again and
/// the desk kept unusable.
pub(crate) async fn guard_unsecured_console() {
    let mut tick = tokio::time::interval(UNUSABLE_TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let retry_every =
        u32::try_from(RECOVERY_RETRY.as_secs() / UNUSABLE_TICK.as_secs()).unwrap_or(10);
    let mut ticks: u32 = 0;
    loop {
        tick.tick().await;
        if PrivacyJournal::read().is_none() {
            ticks = 0;
            continue;
        }
        // Only while no live launcher holds the console: one that died
        // (killed, crashed) left the desk with its grabs gone, and its
        // clean-up waits for the next sign-in.
        // RECOVERY first: a launcher handed the gate after this check
        // still waits for it (`console_gate`).
        let Ok(_recovery) = RECOVERY.try_lock() else {
            continue;
        };
        let gate = std::sync::Arc::clone(&CONSOLE_GATE).try_lock_owned();
        if gate.is_err() && !gate_holder_exited() {
            continue;
        }
        ticks = ticks.wrapping_add(1);
        if ticks % retry_every == 1 || retry_every <= 1 {
            recover(true).await;
        } else if let Some(mut journal) = PrivacyJournal::read() {
            keep_desk_unusable(&mut journal);
        }
        drop(gate);
    }
}

/// The Pier's watch over the console's login screen: one that GDM left
/// waiting for its terminal after a session ended (no X server, no shell,
/// nothing in front of the seat) is brought there, so the person at the
/// desk gets the login screen back. Only while no console launcher runs
/// (a sign-in brings it there itself).
pub(crate) async fn guard_login_screen() {
    let mut tick = tokio::time::interval(LOGIN_SCREEN_WATCH_TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut watch = model::login_screen::StalledLoginScreens::default();
    loop {
        tick.tick().await;
        let Ok(gate) = std::sync::Arc::clone(&CONSOLE_GATE).try_lock_owned() else {
            continue;
        };
        let sessions = read_sessions().await;
        let mut without_shell = Vec::new();
        for greeter in sessions.iter().filter(|fact| fact.is_console_greeter()) {
            if session_processes(&greeter.id, greeter.uid).is_empty() {
                without_shell.push(greeter.id.clone());
            }
        }
        let foreground_vt = foreground_vt();
        if let Some(id) = watch.observe(
            &sessions,
            &without_shell,
            foreground_vt,
            std::time::Instant::now(),
        ) {
            let activated = activate_session(&id).await;
            // A display manager fault: recorded at every profile.
            tracing::error!(
                target: target::SESSION,
                greeter_session = %id,
                activated,
                attempt = watch.attempts(&id),
                "the login screen was left waiting for its terminal after a session ended (GDM's switch was lost); it is brought there"
            );
        }
        drop(gate);
    }
}

/// The virtual terminal in front (`/sys/class/tty/tty0/active`), 0 unknown.
fn foreground_vt() -> u32 {
    std::fs::read_to_string("/sys/class/tty/tty0/active")
        .ok()
        .and_then(|active| active.trim().strip_prefix("tty")?.parse().ok())
        .unwrap_or(0)
}

/// Ends the login screen session that took a remote-first sign-in when GDM
/// has finished it but processes it left behind keep it (an `ibus-daemon`
/// of a crashed greeter shell): they keep the display manager's account
/// manager running, which lets the next login screen lose its terminal
/// switch when this session ends.
pub(crate) async fn end_finished_login_screen(greeter_session: &str) {
    let Some(fact) = session_fact(greeter_session).await else {
        return;
    };
    let leader_alive = fact.leader != 0 && process_state(fact.leader).is_some();
    if !model::login_screen::finished_but_lingering(&fact, leader_alive) {
        return;
    }
    let leftovers = session_processes(greeter_session, fact.uid);
    let mut terminate = Command::new(LOGINCTL);
    terminate.args(["terminate-session", greeter_session]);
    let ended = bounded_output(terminate).await.is_some();
    tracing::info!(
        target: target::SESSION,
        greeter_session,
        leftovers = leftovers.len(),
        processes = %leftovers.iter().map(|process| process.comm.as_str()).collect::<Vec<_>>().join(","),
        ended,
        "the finished login screen's leftover processes are ended"
    );
}

/// SSSD's configuration as SSSD loads it: `sssd.conf` and every
/// `conf.d/*.conf` snippet (root's, bounded), for the domain logon check.
pub(crate) fn sssd_files() -> Vec<model::domain_logon::SssdFile> {
    use std::io::Read as _;
    let read = |path: &Path| -> Option<String> {
        let metadata = std::fs::symlink_metadata(path).ok()?;
        if !metadata.file_type().is_file() || metadata.uid() != 0 || metadata.len() > 256 * 1024 {
            return None;
        }
        let mut text = String::new();
        std::fs::File::open(path)
            .ok()?
            .take(256 * 1024)
            .read_to_string(&mut text)
            .ok()?;
        Some(text)
    };
    let mut files = Vec::new();
    let protected = |path: &Path| {
        std::fs::symlink_metadata(path).is_ok_and(|metadata| {
            model::domain_logon::sssd_reads_snippet(
                metadata.file_type().is_file(),
                metadata.uid(),
                metadata.mode(),
            )
        })
    };
    if let Some(text) = read(Path::new("/etc/sssd/sssd.conf")) {
        files.push(model::domain_logon::SssdFile {
            name: "sssd.conf".to_owned(),
            text,
        });
    }
    if let Ok(entries) = std::fs::read_dir("/etc/sssd/conf.d") {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || !name.ends_with(".conf") || !protected(&entry.path()) {
                continue;
            }
            if let Some(text) = read(&entry.path()) {
                files.push(model::domain_logon::SssdFile { name, text });
            }
        }
    }
    files
}

/// Whether `username` resolves through SSSD (a domain account).
async fn domain_account(username: &str) -> bool {
    let mut getent = Command::new("/usr/bin/getent");
    getent.args(["-s", "sss", "passwd", username]);
    bounded_output(getent).await.is_some()
}

/// Why a domain account's remote-first sign-in through the login screen
/// would be refused by the domain's Group Policy before GDM even starts
/// it: the login screen's service is not weighed as an interactive logon
/// (the installer's SSSD snippet is missing), or the administrator took it
/// out of the interactive set.
pub(crate) async fn domain_refusal(username: &str, service: &str) -> Option<ConsoleRefusal> {
    use model::domain_logon::{domain_logons, DomainLogon};
    let logons = domain_logons(&sssd_files(), service);
    let blocking = logons.iter().find(|(_, logon)| match logon {
        DomainLogon::NotGpo | DomainLogon::Interactive => false,
        // The administrator's own map decides; only a deny is known now.
        DomainLogon::MappedByAdministrator { map } => map == "ad_gpo_map_deny",
        DomainLogon::RemovedByAdministrator | DomainLogon::Unmapped { .. } => true,
    })?;
    if !domain_account(username).await {
        return None;
    }
    let (domain, logon) = blocking;
    tracing::error!(
        target: target::SESSION,
        pam_service = service,
        %domain,
        decision = ?logon,
        "SSSD's Active Directory Group Policy access control does not weigh this remote-first sign-in as an interactive logon (rerun the installer, which maps it; or the administrator removed it)"
    );
    Some(match logon {
        DomainLogon::Unmapped { .. } => ConsoleRefusal::SignInUnavailable,
        _ => ConsoleRefusal::AccountDenied,
    })
}

/// Whether the login screen can take a remote sign-in on this host: Arcen's
/// PAM stack for the hook and its module are installed.
fn sign_in_components_installed() -> bool {
    let stack = std::fs::read_to_string(format!("/etc/pam.d/{}", greeter_token::PAM_SERVICE))
        .is_ok_and(|text| greeter_token::pam_stack_is_arcen(&text));
    let module = [
        "/usr/lib64/security",
        "/usr/lib/security",
        "/usr/lib/x86_64-linux-gnu/security",
    ]
    .iter()
    .any(|dir| Path::new(dir).join("pam_arcen_greeter.so").is_file());
    stack && module
}

/// Writes the token's record into GDM's runtime directory (root `0700`).
fn write_record(
    token: &str,
    record: &greeter_token::GreeterTokenRecord,
) -> Result<PathBuf, String> {
    use std::io::Write as _;
    use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _};
    let dir = Path::new(greeter_token::RECORD_DIR);
    if std::fs::symlink_metadata(dir.parent().ok_or("record directory has no parent")?)
        .map(|metadata| !metadata.is_dir() || metadata.uid() != 0)
        .unwrap_or(true)
    {
        return Err("GDM's runtime directory is missing or not root's".to_owned());
    }
    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(format!("record directory: {error}")),
    }
    let metadata = std::fs::symlink_metadata(dir).map_err(|error| error.to_string())?;
    if !greeter_token::record_dir_trustworthy(metadata.is_dir(), metadata.uid(), 0, metadata.mode())
    {
        // Ours, but loosened: tighten it rather than refuse a sign-in.
        if metadata.is_dir() && metadata.uid() == 0 {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
                .map_err(|error| error.to_string())?;
        } else {
            return Err("record directory is not root's".to_owned());
        }
    }
    let name = greeter_token::record_name(token).ok_or("token is malformed")?;
    let path = dir.join(name);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(&path)
        .map_err(|error| format!("record: {error}"))?;
    file.write_all(record.encode().as_bytes())
        .map_err(|error| format!("record: {error}"))?;
    Ok(path)
}

/// The login screen's processes that are its own (not the display
/// manager's worker, which runs as root): its display server and shell.
async fn login_screen_processes(greeter_session: &str) -> Option<Vec<SessionProcess>> {
    let uid = session_fact(greeter_session)
        .await
        .filter(|fact| fact.class == "greeter")
        .map(|fact| fact.uid)?;
    Some(session_processes(greeter_session, uid))
}

fn shell_pids(processes: &[SessionProcess]) -> Vec<u32> {
    processes
        .iter()
        .filter(|process| process.comm == "gnome-shell")
        .map(|process| process.pid)
        .collect()
}

/// Waits (bounded) until the login screen a remote-first sign-in goes
/// through runs its shell, and returns that login screen. One that lost
/// GDM's terminal switch while nothing is in front of the seat
/// ([`model::login_screen`]) is brought to its terminal, which lets GDM
/// start its X server and shell; a login screen GDM replaced meanwhile is
/// followed.
pub(crate) async fn ready_login_screen(greeter_session: &str) -> Result<String, ConsoleRefusal> {
    use model::login_screen::LoginScreenStep;
    let deadline = tokio::time::Instant::now() + LOGIN_SCREEN_READY;
    let mut greeter = greeter_session.to_owned();
    let mut brought = 0_u32;
    loop {
        let sessions = read_sessions().await;
        let processes = login_screen_processes(&greeter).await.unwrap_or_default();
        let shell_running = !shell_pids(&processes).is_empty();
        match model::login_screen::step(&sessions, &greeter, shell_running) {
            LoginScreenStep::Ready => {
                if brought > 0 {
                    tracing::info!(
                        target: target::SESSION,
                        greeter_session = %greeter,
                        "the login screen runs after it was brought to its terminal"
                    );
                }
                return Ok(greeter);
            }
            LoginScreenStep::BringToTerminal(id) if brought < LOGIN_SCREEN_BRING_ATTEMPTS => {
                brought += 1;
                let activated = activate_session(&id).await;
                // A display manager fault: recorded at every profile.
                tracing::error!(
                    target: target::SESSION,
                    greeter_session = %id,
                    activated,
                    "the login screen was waiting for its terminal with nothing in front (GDM's switch was lost while the previous session ended); it is brought there"
                );
            }
            LoginScreenStep::Gone => {
                if let Some(next) = sessions.iter().find(|fact| fact.is_console_greeter()) {
                    next.id.clone_into(&mut greeter);
                    continue;
                }
            }
            LoginScreenStep::BringToTerminal(_) | LoginScreenStep::Wait => {}
        }
        if tokio::time::Instant::now() >= deadline {
            tracing::error!(
                target: target::SESSION,
                greeter_session = %greeter,
                brought,
                processes = processes.len(),
                "the login screen's shell is not running"
            );
            return Err(ConsoleRefusal::SignInUnavailable);
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Gives the sign-in token to the login screen's own bus connections only
/// (never broadcast): the processes of gnome-shell in the login screen's
/// session. Owns the credential name until the sign-in completes: the
/// login screen checks the sender against the name's owner.
struct TokenSender {
    connection: zbus::Connection,
    greeter_session: String,
}

impl TokenSender {
    async fn new(greeter_session: &str) -> Result<Self, String> {
        use zbus::fdo::{RequestNameFlags, RequestNameReply};
        let connection = zbus::Connection::system()
            .await
            .map_err(|error| format!("system bus: {error}"))?;
        let reply = connection
            .request_name_with_flags(greeter_token::BUS_NAME, RequestNameFlags::DoNotQueue.into())
            .await
            .map_err(|error| format!("credential name: {error}"))?;
        if reply != RequestNameReply::PrimaryOwner {
            return Err("another service owns the login screen's credential name".to_owned());
        }
        // The login screen's proxy follows the name's owner: give it a
        // moment to see the new owner before the signal arrives.
        tokio::time::sleep(Duration::from_millis(500)).await;
        Ok(Self {
            connection,
            greeter_session: greeter_session.to_owned(),
        })
    }

    /// Sends `token` to every shell connection of the login screen; how
    /// many took it. `unspent`, when given, is checked again just before
    /// each signal: a shell that receives the token while its conversation
    /// already runs restarts it, which must never happen once the PAM
    /// module has spent the record.
    async fn send(&self, token: &str, unspent: Option<&Path>) -> Result<usize, String> {
        use zbus::fdo::DBusProxy;
        let shells = shell_pids(
            &login_screen_processes(&self.greeter_session)
                .await
                .ok_or("the login screen's session is gone")?,
        );
        if shells.is_empty() {
            return Err("the login screen's shell is not running".to_owned());
        }
        let proxy = DBusProxy::new(&self.connection)
            .await
            .map_err(|error| error.to_string())?;
        let mut targets = Vec::new();
        for name in proxy
            .list_names()
            .await
            .map_err(|error| error.to_string())?
        {
            if !name.as_str().starts_with(':') {
                continue;
            }
            if let Ok(pid) = proxy.get_connection_unix_process_id(name.as_ref()).await {
                if shells.contains(&pid) {
                    targets.push(name);
                }
            }
        }
        for target in &targets {
            if unspent.is_some_and(|record| !record.exists()) {
                break;
            }
            self.connection
                .emit_signal(
                    Some(target.as_ref()),
                    greeter_token::OBJECT_PATH,
                    greeter_token::INTERFACE,
                    greeter_token::SIGNAL,
                    &(token,),
                )
                .await
                .map_err(|error| format!("credential signal: {error}"))?;
        }
        Ok(targets.len())
    }
}

impl Drop for TokenSender {
    /// Keeps owning the name until a late sign-in could still complete.
    fn drop(&mut self) {
        let connection = self.connection.clone();
        tokio::spawn(async move {
            tokio::time::sleep(SIGN_IN_TIMEOUT).await;
            let _ = connection.release_name(greeter_token::BUS_NAME).await;
        });
    }
}

/// Completes a remote-first sign-in through GDM's login screen and returns
/// the session it started (or, when the user already had one GDM started,
/// resumed). The caller proved the user's password with its own PAM
/// service and account checks first.
pub(crate) async fn sign_in_through_greeter(
    greeter_session: &str,
    username: &str,
    uid: u32,
) -> Result<String, ConsoleRefusal> {
    if !sign_in_components_installed() {
        tracing::error!(
            target: target::SESSION,
            pam_service = greeter_token::PAM_SERVICE,
            "the login screen's remote sign-in is not installed (rerun the installer)"
        );
        return Err(ConsoleRefusal::SignInUnavailable);
    }
    if let Some(refusal) = domain_refusal(username, greeter_token::PAM_SERVICE).await {
        return Err(refusal);
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    let record = greeter_token::GreeterTokenRecord::issue(username, uid, now)
        .ok_or(ConsoleRefusal::SignInUnavailable)?;
    let mut bytes = [0_u8; greeter_token::TOKEN_BYTES];
    getrandom::getrandom(&mut bytes).map_err(|_| ConsoleRefusal::SignInUnavailable)?;
    let token = greeter_token::token_text(&bytes);
    bytes.fill(0);
    let record_path = write_record(&token, &record).map_err(|error| {
        tracing::error!(target: target::SESSION, %error, "the one-time sign-in record could not be written");
        ConsoleRefusal::SignInUnavailable
    })?;
    struct Spend(PathBuf);
    impl Drop for Spend {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let _spend = Spend(record_path.clone());
    let sender = TokenSender::new(greeter_session).await.map_err(|error| {
        tracing::error!(target: target::SESSION, %error, "the login screen could not be given the sign-in");
        ConsoleRefusal::SignInUnavailable
    })?;
    let sent = sender.send(&token, None).await;
    let mut sent_at = tokio::time::Instant::now();
    let mut resends = 0_u32;
    match sent {
        // A shell that has not joined the system bus yet: given again below.
        Ok(0) => {
            tracing::warn!(target: target::SESSION, "no login-screen connection took the sign-in token yet");
        }
        Ok(connections) => tracing::info!(
            target: target::SESSION,
            username,
            greeter_session,
            connections,
            "remote-first sign-in handed to the login screen (one-time token)"
        ),
        Err(error) => {
            tracing::error!(target: target::SESSION, %error, "the login screen could not be given the sign-in");
            return Err(ConsoleRefusal::SignInUnavailable);
        }
    }
    let deadline = tokio::time::Instant::now() + SIGN_IN_TIMEOUT;
    let mut spent_logged = false;
    let mut brought_forward = 0_u32;
    while tokio::time::Instant::now() < deadline {
        if !spent_logged && !record_path.exists() {
            spent_logged = true;
            tracing::info!(target: target::SESSION, "the login screen spent the sign-in token");
        }
        // A shell that started a moment ago may not have listened yet: the
        // token goes again while nothing has spent it.
        if !spent_logged
            && resends < TOKEN_RESENDS
            && sent_at.elapsed() >= TOKEN_RESEND_AFTER
            && record_path.exists()
        {
            resends += 1;
            sent_at = tokio::time::Instant::now();
            let sent = sender.send(&token, Some(&record_path)).await;
            tracing::warn!(
                target: target::SESSION,
                resends,
                connections = sent.as_ref().map_or(0, |count| *count),
                "the login screen had not taken the sign-in token; it is given again"
            );
        }
        let sessions = read_sessions().await;
        if let Some(session) = sessions
            .iter()
            .find(|session| session.uid == uid && session.is_console_desktop() && session.active)
        {
            tracing::info!(
                target: target::SESSION,
                session_id = %session.id,
                service = %session.service,
                "GDM started the user's console session"
            );
            return Ok(session.id.clone());
        }
        // GDM put a new login screen in front of the session before it was
        // ever shown: bring it forward (the desk's input is already off).
        if let model::SignInSettlement::BringForward(id) =
            model::settle_sign_in(&sessions, uid, greeter_session)
        {
            if brought_forward < SIGN_IN_BRING_FORWARD_ATTEMPTS {
                brought_forward += 1;
                let activated = activate_session(&id).await;
                tracing::warn!(
                    target: target::SESSION,
                    session_id = %id,
                    activated,
                    "the display manager put a new login screen in front of the signed-in session; it is brought forward"
                );
                if activated {
                    return Ok(id);
                }
                continue;
            }
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let token_spent = !record_path.exists();
    tracing::error!(
        target: target::SESSION,
        token_spent,
        "the login screen did not start the session in time"
    );
    if token_spent {
        // GDM may still finish the sign-in: a session that appears late is
        // locked, never left open at the desk.
        let late = tokio::time::Instant::now() + SIGN_IN_TIMEOUT;
        while tokio::time::Instant::now() < late {
            if let Some(session) = read_sessions()
                .await
                .into_iter()
                .find(|session| session.uid == uid && session.is_console_desktop())
            {
                let locked = secure_session(&session.id).await;
                tracing::warn!(
                    target: target::SESSION,
                    session_id = %session.id,
                    locked,
                    "the late console session is locked"
                );
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
    Err(ConsoleRefusal::SignInFailed)
}

/// Waits, with the desk already private (kept so on every tick), until GDM
/// has finished the login screen that took a remote-first sign-in, and
/// brings `session` forward again when GDM put a new login screen in front
/// of it. Gives up at `until` at the latest. Returns whether `session` is
/// in the foreground.
pub(crate) async fn settle_signed_in_session(
    greeter_session: &str,
    uid: u32,
    session: &str,
    privacy: &mut ConsolePrivacy,
    until: tokio::time::Instant,
) -> bool {
    let deadline = (tokio::time::Instant::now() + SIGN_IN_SETTLE_TIMEOUT).min(until);
    let mut settled_since: Option<tokio::time::Instant> = None;
    let mut active_since: Option<tokio::time::Instant> = None;
    let mut brought_forward = 0_u32;
    loop {
        let sessions = read_sessions().await;
        let now = tokio::time::Instant::now();
        let mine_active = sessions
            .iter()
            .any(|fact| fact.id == session && fact.active);
        active_since = if mine_active {
            active_since.or(Some(now))
        } else {
            None
        };
        match model::settle_sign_in(&sessions, uid, greeter_session) {
            model::SignInSettlement::Settled(id) if id == session => {
                let since = *settled_since.get_or_insert(now);
                if now.duration_since(since) >= SIGN_IN_SETTLE {
                    tracing::info!(
                        target: target::SESSION,
                        session_id = %session,
                        brought_forward,
                        "the login screen finished; the signed-in session stays in front"
                    );
                    return true;
                }
            }
            model::SignInSettlement::BringForward(id)
                if id == session && brought_forward < SIGN_IN_BRING_FORWARD_ATTEMPTS =>
            {
                settled_since = None;
                brought_forward += 1;
                let activated = activate_session(session).await;
                tracing::warn!(
                    target: target::SESSION,
                    session_id = %session,
                    activated,
                    "the display manager put a new login screen in front of the signed-in session; it is brought forward"
                );
            }
            _ => {
                settled_since = None;
                // A display manager that keeps its login screen after the
                // sign-in: the session has been in front long enough.
                if active_since
                    .is_some_and(|since| now.duration_since(since) >= SIGN_IN_KEEPS_GREETER)
                {
                    return true;
                }
            }
        }
        if now >= deadline {
            tracing::warn!(
                target: target::SESSION,
                session_id = %session,
                in_front = mine_active,
                brought_forward,
                "the login screen had not settled after the sign-in"
            );
            return mine_active;
        }
        // A VT switch, or the session's own colour settings once it is in
        // front, may undo the blanking; new desk devices are taken too.
        privacy.enforce();
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// What a returning Deck's attachment does to the console, as steps a
/// test can stand in for.
pub(crate) trait ConsoleDesk {
    async fn sessions(&mut self) -> Vec<ConsoleSessionFact>;
    fn disable_input(&mut self) -> Result<(), ConsoleRefusal>;
    async fn activate(&mut self, session_id: &str) -> bool;
    /// Blanks the desk's monitors only.
    fn blank(&mut self) -> Result<(), ConsoleRefusal>;
    /// Blanks the desk's monitors and keeps its input disabled.
    fn arm(&mut self) -> Result<(), ConsoleRefusal>;
    async fn unlock(&mut self, session_id: &str);
    async fn hand_back(&mut self, session_id: &str) -> bool;
}

/// Makes the console private again for a returning Deck (ADR 0025), or
/// says why it may not (the reason the returning Deck is shown).
///
/// The console is decided afresh (`decide_reattach`): only this session
/// of `uid`, still the one the decision attaches, is touched. When it has
/// to be brought forward from behind a login screen, the desk's input is
/// disabled before it is shown, and its monitors are blanked before it is
/// shown where the X server keeps the ramps of an inactive VT (Xorg
/// re-applies them when the VT comes back), and in any case as soon as it
/// is shown. Any failure after that hands the console back: locked (or
/// covered by a login screen) before the desk gets its monitors and input
/// back.
pub(crate) async fn reattach<D: ConsoleDesk>(
    desk: &mut D,
    session_id: &str,
    uid: u32,
    host: model::ConsoleHost,
) -> Result<(), ConsoleRefusal> {
    let sessions = desk.sessions().await;
    let activate = match model::decide_reattach(&sessions, uid, session_id, host) {
        Ok(activate) => activate,
        Err(refusal) => {
            tracing::warn!(
                target: target::SESSION,
                session_id,
                reason_class = refusal.token(),
                "the console changed while the Deck was away; the returning Deck is not attached"
            );
            return Err(refusal);
        }
    };
    if activate {
        if let Err(refusal) = desk.disable_input() {
            tracing::error!(
                target: target::SESSION,
                reason_class = refusal.token(),
                "the desk's input could not be disabled; the session is not brought forward"
            );
            desk.hand_back(session_id).await;
            return Err(refusal);
        }
        // Best effort: blank on the inactive VT, before it is shown (the
        // input is already disabled, and that part is never best effort).
        let blanked_first = desk.blank().is_ok();
        if !blanked_first {
            tracing::warn!(
                target: target::SESSION,
                "the console's monitors could not be blanked behind the login screen; they are blanked as soon as the session is in front"
            );
        }
        if !desk.activate(session_id).await {
            tracing::error!(
                target: target::SESSION,
                session_id,
                "the console session could not be brought forward for the returning Deck"
            );
            desk.hand_back(session_id).await;
            return Err(ConsoleRefusal::SessionUnreachable);
        }
    }
    if let Err(refusal) = desk.arm() {
        tracing::error!(
            target: target::SESSION,
            reason_class = refusal.token(),
            brought_forward = activate,
            "the console could not be made private again; it is handed back locked"
        );
        desk.hand_back(session_id).await;
        return Err(refusal);
    }
    // Only now, with the desk private, may the session open.
    desk.unlock(session_id).await;
    Ok(())
}

/// The real console for [`reattach`].
pub(crate) struct LiveDesk<'a> {
    pub(crate) privacy: &'a mut ConsolePrivacy,
    pub(crate) display: &'a SessionDisplay,
    pub(crate) session_id: &'a str,
    /// The Deck's layout, laid out (or proved) before the session opens.
    pub(crate) layout: Option<(&'a [arcen_outputs::console_layout::ConsoleHeadRequest], u32)>,
    /// Set when the layout could not be laid out again: the session was
    /// not unlocked and the console is handed back.
    pub(crate) layout_failed: bool,
}

impl ConsoleDesk for LiveDesk<'_> {
    async fn sessions(&mut self) -> Vec<ConsoleSessionFact> {
        read_sessions().await
    }

    fn disable_input(&mut self) -> Result<(), ConsoleRefusal> {
        self.privacy.disable_input()
    }

    async fn activate(&mut self, session_id: &str) -> bool {
        // From here the session may be on the desk's monitors: the
        // launcher does not leave before it is handed back.
        self.privacy.mark_brought_forward();
        activate_session(session_id).await
    }

    fn blank(&mut self) -> Result<(), ConsoleRefusal> {
        self.privacy.blank(self.display, self.session_id)
    }

    fn arm(&mut self) -> Result<(), ConsoleRefusal> {
        self.privacy.arm(self.display, self.session_id)
    }

    async fn unlock(&mut self, session_id: &str) {
        if let Some((requests, uid)) = self.layout {
            if let Err(failure) = self
                .privacy
                .refresh_layout(self.display, self.session_id, uid, requests)
                .await
            {
                tracing::error!(
                    target: target::SESSION,
                    reason_class = failure.token,
                    detail = %failure.detail,
                    "the console could not take the returning Deck's layout again; it is handed back"
                );
                self.layout_failed = true;
                hand_back(self.privacy, session_id).await;
                return;
            }
        }
        if let Err(error) = super::launcher::unlock_logind_session_id(session_id).await {
            tracing::warn!(target: target::SESSION, %error, "the rejoined console session could not be unlocked");
        }
    }

    async fn hand_back(&mut self, session_id: &str) -> bool {
        hand_back(self.privacy, session_id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A console left behind by a launcher that died (killed, crashed):
    /// records what recovery does to it, in order.
    struct DeadLauncherConsole {
        journal: Option<PrivacyJournal>,
        layout_session: Option<String>,
        locks: bool,
        devices_back: bool,
        log: Vec<&'static str>,
    }

    impl DeadLauncherConsole {
        fn new(locks: bool) -> Self {
            Self {
                journal: Some(PrivacyJournal {
                    session_id: "c7".to_owned(),
                    display: SessionDisplay {
                        display: ":1".to_owned(),
                        number: 1,
                        xauthority: PathBuf::from("/run/user/1000/gdm/Xauthority"),
                        uid: Some(1000),
                    },
                    gamma: Vec::new(),
                    x_disabled: Vec::new(),
                }),
                layout_session: Some("c7".to_owned()),
                locks,
                devices_back: true,
                log: Vec::new(),
            }
        }

        fn recover(&mut self) -> bool {
            tokio::runtime::Builder::new_current_thread()
                .build()
                .expect("runtime")
                .block_on(recover_with(self, false))
        }
    }

    impl ConsoleRecovery for DeadLauncherConsole {
        fn read_journal(&mut self) -> Option<PrivacyJournal> {
            self.journal.clone()
        }
        fn layout_session(&mut self) -> Option<String> {
            self.layout_session.clone()
        }
        fn keep_desk_unusable(&mut self, _journal: &mut PrivacyJournal) {
            self.log.push("unusable");
        }
        async fn secure(
            &mut self,
            _session_id: &str,
            _journal: Option<&mut PrivacyJournal>,
        ) -> bool {
            self.log.push("lock");
            self.locks
        }
        async fn kept_private_by_design(&mut self, _session_id: &str) -> bool {
            false
        }
        async fn restore_layout(&mut self) {
            self.log.push("layout");
        }
        fn give_back(&mut self, _journal: &PrivacyJournal) -> bool {
            self.log.push("give_back");
            self.devices_back
        }
        fn remove_journal(&mut self) {
            self.log.push("journal_removed");
            self.journal = None;
        }
    }

    #[test]
    fn a_console_a_dead_launcher_left_is_locked_before_anything_is_given_back() {
        let mut console = DeadLauncherConsole::new(true);
        assert!(console.recover());
        assert_eq!(
            console.log,
            ["unusable", "lock", "layout", "give_back", "journal_removed"]
        );
    }

    #[test]
    fn a_console_that_will_not_lock_stays_unusable_and_journalled() {
        let mut console = DeadLauncherConsole::new(false);
        assert!(!console.recover());
        assert_eq!(console.log, ["unusable", "lock", "unusable"]);
        assert!(console.journal.is_some());
        // A device that stays disabled keeps the journal too.
        let mut console = DeadLauncherConsole::new(true);
        console.devices_back = false;
        assert!(!console.recover());
        assert!(console.journal.is_some());
    }

    #[test]
    fn a_layout_left_without_privacy_is_restored_only_once_locked() {
        let mut console = DeadLauncherConsole::new(false);
        console.journal = None;
        assert!(console.recover());
        assert_eq!(console.log, ["lock"]);
        let mut console = DeadLauncherConsole::new(true);
        console.journal = None;
        assert!(console.recover());
        assert_eq!(console.log, ["lock", "layout"]);
    }

    /// A listener that never accepts fills its backlog; the next connect
    /// fails at once instead of blocking the root Pier's thread.
    #[test]
    fn connecting_to_a_listener_that_never_accepts_fails_at_once() {
        use nix::sys::socket::{
            bind, listen, socket, AddressFamily, Backlog, SockFlag, SockType, UnixAddr,
        };
        let path = std::env::temp_dir().join(format!("arcen-x-squat-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = socket(
            AddressFamily::Unix,
            SockType::Stream,
            SockFlag::SOCK_CLOEXEC,
            None,
        )
        .unwrap();
        bind(
            std::os::fd::AsRawFd::as_raw_fd(&listener),
            &UnixAddr::new(&path).unwrap(),
        )
        .unwrap();
        listen(&listener, Backlog::new(0).unwrap()).unwrap();
        let started = std::time::Instant::now();
        let mut held = Vec::new();
        let refused = loop {
            match connect_without_waiting(&path) {
                Ok(stream) => held.push(stream),
                Err(error) => break error,
            }
            assert!(held.len() < 64, "the backlog never filled");
        };
        assert_eq!(refused.kind(), std::io::ErrorKind::WouldBlock, "{refused}");
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn the_desk_gets_its_sound_back_once_and_a_returning_deck_silences_it_again() {
        let mut privacy = ConsolePrivacy::default();
        privacy.restore_audio().await;
        assert!(
            privacy.retake_audio().await,
            "no audio link: nothing to take"
        );
        // A second release or hold would be answered `unexpected`.
        let (_agent, control) = super::super::agent::ConsoleAudioControl::stand_in("released held");
        privacy.hold_audio_with(Some(control));
        privacy.restore_audio().await;
        privacy.restore_audio().await;
        assert!(privacy.retake_audio().await);
        assert!(privacy.retake_audio().await);
    }

    #[tokio::test]
    async fn a_desk_that_cannot_be_silenced_again_refuses_the_returning_deck() {
        let (_agent, control) =
            super::super::agent::ConsoleAudioControl::stand_in("released refused");
        let mut privacy = ConsolePrivacy::default();
        privacy.hold_audio_with(Some(control));
        privacy.restore_audio().await;
        assert!(!privacy.retake_audio().await);
    }

    const TIM: u32 = 1001;

    #[test]
    fn the_starting_system_service_is_read_from_the_cgroup() {
        assert_eq!(
            system_service_unit("0::/system.slice/labrdp.service\n").as_deref(),
            Some("labrdp.service")
        );
        assert_eq!(
            system_service_unit("0::/system.slice/labrdp.service/session\n").as_deref(),
            Some("labrdp.service")
        );
        assert_eq!(
            system_service_unit("12:cpu:/\n1:name=systemd:/system.slice/vendor-agent@1.service\n")
                .as_deref(),
            Some("vendor-agent@1.service")
        );
        for none in [
            "0::/user.slice/user-1001.slice/session-7.scope\n",
            "0::/user.slice/user-1001.slice/user@1001.service/app.slice/x.service\n",
            "0::/\n",
            "",
        ] {
            assert_eq!(system_service_unit(none), None, "{none}");
        }
    }

    fn fact(text: &str) -> ConsoleSessionFact {
        ConsoleSessionFact::from_properties(text).expect("fact")
    }

    fn held(active: bool) -> ConsoleSessionFact {
        fact(&format!(
            "Id=7\nUser={TIM}\nSeat=seat0\nVTNr=7\nTTY=tty7\nClass=user\nType=x11\nState={}\nActive={}\nService=gdm-password\n",
            if active { "active" } else { "online" },
            if active { "yes" } else { "no" }
        ))
    }

    fn greeter() -> ConsoleSessionFact {
        fact("Id=c3\nUser=42\nSeat=seat0\nVTNr=1\nClass=greeter\nType=x11\nState=active\nActive=yes\nService=gdm-launch-environment\n")
    }

    #[derive(Default)]
    struct FakeDesk {
        sessions: Vec<ConsoleSessionFact>,
        input_fails: bool,
        activate_fails: bool,
        arm_fails: bool,
        calls: Vec<&'static str>,
    }

    impl ConsoleDesk for FakeDesk {
        async fn sessions(&mut self) -> Vec<ConsoleSessionFact> {
            self.calls.push("sessions");
            self.sessions.clone()
        }
        fn disable_input(&mut self) -> Result<(), ConsoleRefusal> {
            self.calls.push("disable_input");
            if self.input_fails {
                Err(ConsoleRefusal::PrivacyUnavailable)
            } else {
                Ok(())
            }
        }
        async fn activate(&mut self, _: &str) -> bool {
            self.calls.push("activate");
            !self.activate_fails
        }
        fn blank(&mut self) -> Result<(), ConsoleRefusal> {
            self.calls.push("blank");
            Ok(())
        }
        fn arm(&mut self) -> Result<(), ConsoleRefusal> {
            self.calls.push("arm");
            if self.arm_fails {
                Err(ConsoleRefusal::PrivacyUnavailable)
            } else {
                Ok(())
            }
        }
        async fn unlock(&mut self, _: &str) {
            self.calls.push("unlock");
        }
        async fn hand_back(&mut self, _: &str) -> bool {
            self.calls.push("hand_back");
            true
        }
    }

    fn run(desk: &mut FakeDesk) -> Result<(), ConsoleRefusal> {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime")
            .block_on(reattach(desk, "7", TIM, model::ConsoleHost::default()))
    }

    #[test]
    fn a_session_behind_the_login_screen_comes_forward_only_with_the_desk_disabled() {
        let mut desk = FakeDesk {
            sessions: vec![greeter(), held(false)],
            ..FakeDesk::default()
        };
        assert_eq!(run(&mut desk), Ok(()));
        assert_eq!(
            desk.calls,
            [
                "sessions",
                "disable_input",
                "blank",
                "activate",
                "arm",
                "unlock"
            ],
            "blanked behind the login screen, then proved blank once in front"
        );
    }

    #[test]
    fn a_session_already_in_front_is_made_private_then_unlocked() {
        let mut desk = FakeDesk {
            sessions: vec![held(true)],
            ..FakeDesk::default()
        };
        assert_eq!(run(&mut desk), Ok(()));
        assert_eq!(desk.calls, ["sessions", "arm", "unlock"]);
    }

    #[test]
    fn a_failure_after_bringing_the_session_forward_hands_it_back_locked() {
        let mut desk = FakeDesk {
            sessions: vec![greeter(), held(false)],
            arm_fails: true,
            ..FakeDesk::default()
        };
        assert!(run(&mut desk).is_err());
        assert_eq!(
            desk.calls,
            [
                "sessions",
                "disable_input",
                "blank",
                "activate",
                "arm",
                "hand_back"
            ],
            "never unlocked, and handed back (locked) before the desk gets anything back"
        );
        let mut desk = FakeDesk {
            sessions: vec![greeter(), held(false)],
            activate_fails: true,
            ..FakeDesk::default()
        };
        assert_eq!(run(&mut desk), Err(ConsoleRefusal::SessionUnreachable));
        assert_eq!(
            desk.calls,
            [
                "sessions",
                "disable_input",
                "blank",
                "activate",
                "hand_back"
            ]
        );
    }

    #[test]
    fn without_the_desk_input_disabled_nothing_is_brought_forward() {
        let mut desk = FakeDesk {
            sessions: vec![greeter(), held(false)],
            input_fails: true,
            ..FakeDesk::default()
        };
        assert!(run(&mut desk).is_err());
        assert!(!desk.calls.contains(&"activate"));
        assert!(!desk.calls.contains(&"unlock"));
    }

    #[test]
    fn a_console_that_changed_while_the_deck_was_away_is_not_touched() {
        // Another account is at the desk now.
        let other = fact("Id=9\nUser=2002\nSeat=seat0\nVTNr=2\nClass=user\nType=x11\nState=active\nActive=yes\nService=gdm-password\n");
        let mut desk = FakeDesk {
            sessions: vec![other, held(false)],
            ..FakeDesk::default()
        };
        assert_eq!(run(&mut desk), Err(ConsoleRefusal::OtherUser));
        assert_eq!(desk.calls, ["sessions"]);
        // The held session is gone; the account signed in again elsewhere.
        let mut fresh = held(true);
        fresh.id = "8".into();
        let mut desk = FakeDesk {
            sessions: vec![fresh],
            ..FakeDesk::default()
        };
        assert_eq!(run(&mut desk), Err(ConsoleRefusal::SessionChanged));
        assert_eq!(desk.calls, ["sessions"]);
    }

    #[test]
    fn every_property_a_console_decision_reads_is_requested_from_logind() {
        // `from_properties` only sees what `loginctl show-session -p` prints:
        // without `TTY`, Arcen's own dedicated desktops would look like
        // another product's console session.
        let requested: Vec<&str> = SESSION_PROPERTIES.split(',').collect();
        for property in [
            "TTY",
            "Service",
            "Seat",
            "Class",
            "Type",
            "Active",
            "LockedHint",
            "Leader",
        ] {
            assert!(requested.contains(&property), "{property} is not requested");
        }
    }

    /// The field workstation's `HP WMI hotkeys` (`event3`), as udev and
    /// sysfs show it: the hp-wmi driver registers it without a parent, on
    /// `BUS_HOST`, with a few keys and switches.
    const HP_WMI_HOTKEYS_UDEV: &str = "I:15847733\nE:ID_INPUT=1\nE:ID_INPUT_KEY=1\n\
        E:ID_INPUT_SWITCH=1\nE:NVME_HOST_IFACE=none\nE:LIBINPUT_DEVICE_GROUP=19/0/0:wmi\n\
        G:power-switch\nQ:power-switch\nV:1\n";

    /// A remote product's client keyboard made through uinput while its
    /// client is connected (no parent, libevdev's default bus 0 and vendor 0).
    const PRODUCT_KEYBOARD_UDEV: &str = "I:4011223344\nE:ID_INPUT=1\nE:ID_INPUT_KEY=1\n\
        E:ID_INPUT_KEYBOARD=1\nE:XKBMODEL=pc105\nE:XKBLAYOUT=no\nG:seat\nG:uaccess\n\
        Q:seat\nQ:uaccess\nV:1\n";

    #[test]
    fn a_firmware_hotkey_device_reads_as_a_platform_device_with_a_few_keys() {
        let fact = device_fact_from(
            Path::new("/sys/devices/virtual/input/input6"),
            HP_WMI_HOTKEYS_UDEV,
            "0019\n",
        );
        assert!(fact.virtual_device && fact.console_seat && fact.drives_desktop);
        assert!(!fact.keyboard_or_pointer);
        assert_eq!(fact.bus, model::BUS_HOST);
        assert!(!model::remote_client_input_candidate(fact, false));
        assert!(!model::another_products_client_input(
            fact,
            false,
            model::UserSpaceInputCreators::Unknown
        ));
    }

    #[test]
    fn a_uinput_keyboard_and_pointer_of_another_product_are_candidates() {
        let keyboard = device_fact_from(
            Path::new("/sys/devices/virtual/input/input213"),
            PRODUCT_KEYBOARD_UDEV,
            "0000\n",
        );
        assert!(keyboard.keyboard_or_pointer && keyboard.virtual_device);
        assert_eq!(keyboard.bus, 0);
        assert!(model::remote_client_input_candidate(keyboard, false));
        // Arcen's own (its vendor) never is.
        assert!(!model::remote_client_input_candidate(keyboard, true));
        let mouse = device_fact_from(
            Path::new("/sys/devices/virtual/input/input214"),
            "E:ID_INPUT=1\nE:ID_INPUT_MOUSE=1\nE:ID_INPUT_TOUCHSCREEN=1\n",
            "0006",
        );
        assert!(model::remote_client_input_candidate(mouse, false));
        // An unreadable bus never excuses a device.
        let unread = device_fact_from(
            Path::new("/sys/devices/virtual/input/input215"),
            PRODUCT_KEYBOARD_UDEV,
            "",
        );
        assert!(model::remote_client_input_candidate(unread, false));
    }

    #[test]
    fn a_bridged_tablet_and_the_desks_own_keyboard_are_not_candidates() {
        let bridged = device_fact_from(
            Path::new("/sys/devices/virtual/misc/uhid/0003:056A:0317.0009/input/input30"),
            "E:ID_INPUT=1\nE:ID_INPUT_TABLET=1\nE:ID_INPUT.tags=arcen_bridge,arcen_bridge_1\n\
             E:LIBINPUT_IGNORE_DEVICE=1\n",
            "0003",
        );
        assert!(bridged.bridged);
        assert!(!model::remote_client_input_candidate(bridged, false));
        let desk = device_fact_from(
            Path::new("/sys/devices/platform/i8042/serio0/input/input1"),
            "E:ID_INPUT=1\nE:ID_INPUT_KEY=1\nE:ID_INPUT_KEYBOARD=1\n",
            "0011",
        );
        assert!(!desk.virtual_device);
        assert!(!model::remote_client_input_candidate(desk, false));
        assert!(model::disables_while_attached(desk));
    }

    #[test]
    fn a_bluetooth_keyboard_at_the_desk_is_not_a_candidate() {
        // BlueZ makes a Bluetooth LE keyboard through uhid (bluetoothd holds
        // /dev/uhid), so it has no parent device either.
        let keyboard = device_fact_from(
            Path::new("/sys/devices/virtual/misc/uhid/0005:046D:B35B.0004/input/input21"),
            "E:ID_INPUT=1\nE:ID_INPUT_KEY=1\nE:ID_INPUT_KEYBOARD=1\nE:ID_BUS=bluetooth\n",
            "0005\n",
        );
        assert!(keyboard.virtual_device && keyboard.keyboard_or_pointer);
        assert_eq!(keyboard.bus, model::BUS_BLUETOOTH);
        assert!(!model::remote_client_input_candidate(keyboard, false));
    }

    fn creator(pid: u32, exe: Option<&str>) -> InputCreator {
        InputCreator {
            pid,
            comm: format!("p{pid}"),
            exe: exe.map(str::to_owned),
        }
    }

    #[test]
    fn only_arcen_holding_uinput_rules_out_another_products_devices() {
        use model::UserSpaceInputCreators::{ArcenOnly, Other, Unknown};
        const OURS: &str = "/opt/arcen/bin/arcen-pier";
        assert_eq!(creators_from(Some(&[]), Some(OURS)), ArcenOnly);
        let arcen = [
            creator(10, Some(OURS)),
            // A helper of a session that outlived an update of the file.
            creator(11, Some("/opt/arcen/bin/arcen-pier (deleted)")),
        ];
        assert_eq!(creators_from(Some(&arcen), Some(OURS)), ArcenOnly);
        assert_eq!(
            creators_from(Some(&arcen), Some("/opt/arcen/bin/arcen-pier (deleted)")),
            ArcenOnly
        );
        let mut both = arcen.to_vec();
        both.push(creator(20, Some("/usr/lib/remote-product/server")));
        assert_eq!(creators_from(Some(&both), Some(OURS)), Other);
        assert_eq!(
            outside_creators(Some(&both), Some(OURS)),
            "created through uinput/uhid by p20[20]"
        );
        // An unreadable executable is not Arcen's, nor is anyone when this
        // Pier's own could not be read.
        assert_eq!(creators_from(Some(&[creator(21, None)]), Some(OURS)), Other);
        assert_eq!(creators_from(Some(&arcen), None), Other);
        assert_eq!(creators_from(None, Some(OURS)), Unknown);
    }

    #[test]
    fn device_numbers_decode_like_glibc() {
        // makedev(10, 223) and makedev(10, 239): uinput and uhid.
        assert_eq!(device_major_minor(0x0adf), (10, 223));
        assert_eq!(device_major_minor(0x0aef), (10, 239));
        // A large major and minor use the high bits (glibc's makedev).
        let (major, minor) = (0x0001_2345_u64, 0x0006_789a_u64);
        let rdev = ((major & 0x0fff) << 8)
            | ((major & !0x0fff) << 32)
            | (minor & 0xff)
            | ((minor & !0xff) << 12);
        assert_eq!(device_major_minor(rdev), (0x0001_2345, 0x0006_789a));
        // The host's own node, where there is one.
        if let Ok(node) = std::fs::metadata("/dev/uinput") {
            assert_eq!(device_major_minor(node.rdev()), (10, 223));
        }
    }

    #[test]
    fn uinput_and_uhid_holders_are_read_from_proc() {
        use std::os::unix::fs::symlink;
        let root = std::env::temp_dir().join(format!(
            "arcen-console-input-creators-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let process = |pid: u32, exe: &str, fds: &[&str]| {
            let base = root.join(pid.to_string());
            std::fs::create_dir_all(base.join("fd")).unwrap();
            symlink(exe, base.join("exe")).unwrap();
            std::fs::write(base.join("comm"), format!("p{pid}\n")).unwrap();
            for (fd, target) in fds.iter().enumerate() {
                symlink(target, base.join("fd").join(fd.to_string())).unwrap();
            }
        };
        process(
            100,
            "/usr/lib/remote-product/server",
            &["/dev/null", "/dev/uinput"],
        );
        process(
            200,
            "/opt/arcen/bin/arcen-pier",
            &["/dev/uinput", "/dev/uhid"],
        );
        process(300, "/usr/bin/bash", &["/dev/null", "socket:[1]"]);
        // Through a chroot, and a node named uinput that is something else.
        process(400, "/usr/lib/remote-product/hid", &["/srv/jail/dev/uhid"]);
        process(500, "/usr/bin/impostor", &["/home/u/uinput"]);
        // A process that ended between listing and reading.
        std::fs::create_dir_all(root.join("600")).unwrap();
        // The fixture's descriptors stand in for device nodes: their device
        // numbers come from the link text.
        let rdev = |fd: &Path| -> std::io::Result<u64> {
            let target = std::fs::read_link(fd)?;
            Ok(match target.to_str() {
                Some("/dev/uinput") => 0x0adf,
                Some("/dev/uhid" | "/srv/jail/dev/uhid") => 0x0aef,
                _ => 0x0103,
            })
        };

        let holders = user_space_input_holders(&root, &rdev).unwrap();
        assert_eq!(
            holders.iter().map(|holder| holder.pid).collect::<Vec<_>>(),
            [100, 200, 400]
        );
        assert_eq!(
            holders[0].exe.as_deref(),
            Some("/usr/lib/remote-product/server")
        );
        const OURS: Option<&str> = Some("/opt/arcen/bin/arcen-pier");
        assert_eq!(
            creators_from(Some(&holders), OURS),
            model::UserSpaceInputCreators::Other
        );
        assert_eq!(
            creators_from(Some(&holders[1..2]), OURS),
            model::UserSpaceInputCreators::ArcenOnly
        );
        // A descriptor that cannot be read makes the answer unknown.
        let unreadable = |_: &Path| -> std::io::Result<u64> {
            Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
        };
        assert!(user_space_input_holders(&root, &unreadable).is_none());
        std::fs::remove_dir_all(&root).unwrap();
    }
}
