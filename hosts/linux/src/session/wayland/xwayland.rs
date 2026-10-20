//! Xwayland inside a headless session: finding the displays the compositor
//! locked, proving X11 clients are served, and removing the session's own
//! X11 sockets and lock files when the compositor could not.
//!
//! Mutter owns its Xwayland: it picks the displays, writes the lock files,
//! binds the sockets, creates the cookie and exports `DISPLAY`. Arcen only
//! reads those facts, connects once as a client, and, after a compositor
//! that died without its own shutdown (SIGKILL), removes exactly the
//! artefacts of the displays that compositor locked.
//!
//! `/tmp` and `/tmp/.X11-unix` are shared, root-owned sticky directories in
//! which any user creates names. Nothing here acts on a path there: every
//! operation goes through a descriptor of the directory, proven root-owned
//! and sticky, with `fstatat(AT_SYMLINK_NOFOLLOW)` before `unlinkat`, and an
//! entry is removed only when it is a socket or regular file owned by the
//! session user, named for one of the session's displays, and (for a lock)
//! still holding the session compositor's pid. Run as the session user, the
//! kernel's sticky-bit rule adds that nobody else's entry can be removed
//! anyway.

use std::io::Read as _;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::time::Duration;

use arcen_session::wayland_x11::{
    glamor_route_from_maps, is_mutter_xauthority, parse_lock_name, parse_x_lock,
    parse_xauthority_cookie, parse_xwayland_cmdline, removable_lock, removable_socket,
    session_displays, x11_lock_name, x11_socket_name, ArtifactType, GlamorRoute, LockFact,
    SessionDisplays, X11_LOCK_DIR, X11_UNIX_DIR,
};
use rustix::fs::{AtFlags, FileType, Mode, OFlags};

use crate::logging::target;

const POLL: Duration = Duration::from_millis(50);
/// Largest lock file read: `%10d\n` is eleven bytes.
const MAX_LOCK_BYTES: u64 = 64;
/// Largest authority file read; Mutter's holds one 16-byte cookie.
const MAX_XAUTHORITY_BYTES: u64 = 64 * 1024;
const STICKY: u32 = 0o1000;

/// The directories X11 artefacts live in and who must own them.
#[derive(Debug, Clone)]
pub struct X11Dirs {
    /// Holds `.X<n>-lock`.
    pub lock_dir: PathBuf,
    /// Holds `X<n>` sockets.
    pub unix_dir: PathBuf,
    /// Owner both directories must have (root in production).
    pub owner: u32,
    /// Whether both must be sticky (always in production).
    pub sticky: bool,
}

impl X11Dirs {
    /// `/tmp` and `/tmp/.X11-unix`, root-owned and sticky.
    #[must_use]
    pub fn production() -> Self {
        Self {
            lock_dir: PathBuf::from(X11_LOCK_DIR),
            unix_dir: PathBuf::from(X11_UNIX_DIR),
            owner: 0,
            sticky: true,
        }
    }

    /// A descriptor of `dir`, when it is a real directory (not a symlink)
    /// with the expected owner and sticky bit.
    fn open_trusted(&self, dir: &Path) -> Option<OwnedFd> {
        let fd = rustix::fs::open(
            dir,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .ok()?;
        let stat = rustix::fs::fstat(&fd).ok()?;
        let trusted = FileType::from_raw_mode(stat.st_mode) == FileType::Directory
            && stat.st_uid == self.owner
            && (!self.sticky || stat.st_mode & STICKY != 0);
        trusted.then_some(fd)
    }
}

/// One directory entry, never followed.
#[derive(Debug, Clone, Copy)]
struct Entry {
    kind: ArtifactType,
    owner: u32,
    inode: u64,
    device: u64,
}

fn entry_at(dir: &OwnedFd, name: &str) -> Option<Entry> {
    let stat = rustix::fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW).ok()?;
    Some(Entry {
        kind: match FileType::from_raw_mode(stat.st_mode) {
            FileType::Socket => ArtifactType::Socket,
            FileType::RegularFile => ArtifactType::Regular,
            _ => ArtifactType::Other,
        },
        owner: stat.st_uid,
        inode: stat.st_ino,
        device: stat.st_dev,
    })
}

/// The pid a lock file of `uid` holds, read through the directory
/// descriptor without following a symlink and only from the same regular
/// file `entry` described.
fn lock_pid_at(dir: &OwnedFd, name: &str, entry: Entry) -> Option<u32> {
    let fd = rustix::fs::openat(
        dir,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC | OFlags::NOCTTY,
        Mode::empty(),
    )
    .ok()?;
    let stat = rustix::fs::fstat(&fd).ok()?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile
        || stat.st_ino != entry.inode
        || stat.st_dev != entry.device
        || stat.st_uid != entry.owner
    {
        return None;
    }
    let mut contents = Vec::with_capacity(16);
    std::fs::File::from(fd)
        .take(MAX_LOCK_BYTES)
        .read_to_end(&mut contents)
        .ok()?;
    parse_x_lock(&contents)
}

/// Every `.X<n>-lock` in `dirs.lock_dir`; the pid is read only from lock
/// files owned by `uid`.
fn lock_facts(dirs: &X11Dirs, uid: u32) -> Vec<LockFact> {
    let Some(dir) = dirs.open_trusted(&dirs.lock_dir) else {
        return Vec::new();
    };
    let Ok(names) = std::fs::read_dir(&dirs.lock_dir) else {
        return Vec::new();
    };
    names
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter_map(|name| Some((parse_lock_name(&name)?, name)))
        .filter_map(|(display, name)| {
            let entry = entry_at(&dir, &name)?;
            let pid = (entry.kind == ArtifactType::Regular && entry.owner == uid)
                .then(|| lock_pid_at(&dir, &name, entry))
                .flatten();
            Some(LockFact {
                display,
                owner_uid: entry.owner,
                pid,
            })
        })
        .collect()
}

/// The two displays the compositor `compositor_pid` locked. Mutter locks
/// them before it creates its Wayland socket, so they exist once the
/// compositor is reachable; the wait only covers a slow file system.
pub async fn discover(
    dirs: &X11Dirs,
    uid: u32,
    compositor_pid: u32,
    timeout: Duration,
) -> Option<SessionDisplays> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(displays) = session_displays(&lock_facts(dirs, uid), uid, compositor_pid) {
            return Some(displays);
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(POLL).await;
    }
}

/// Mutter's authority file in the private runtime directory: the one
/// regular `.mutter-Xwaylandauth.*` file of `uid` there, never followed.
#[must_use]
pub fn mutter_xauthority(runtime_dir: &Path, uid: u32) -> Option<PathBuf> {
    use std::os::unix::fs::MetadataExt;
    let mut found = std::fs::read_dir(runtime_dir)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| is_mutter_xauthority(path, runtime_dir))
        .filter(|path| {
            std::fs::symlink_metadata(path)
                .is_ok_and(|metadata| metadata.is_file() && metadata.uid() == uid)
        });
    let first = found.next()?;
    found.next().is_none().then_some(first)
}

/// Whether `/tmp/.X<n>-lock` (`lock`, in the trusted directory `dir`) is a
/// regular file of `uid` (never followed) that holds `compositor_pid`.
fn session_lock_held(dir: &OwnedFd, lock: &str, uid: u32, compositor_pid: u32) -> bool {
    entry_at(dir, lock).is_some_and(|entry| {
        let pid = (entry.kind == ArtifactType::Regular && entry.owner == uid)
            .then(|| lock_pid_at(dir, lock, entry))
            .flatten();
        removable_lock(entry.kind, entry.owner, pid, uid, compositor_pid)
    })
}

/// What removing a session's X11 artefacts left.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ArtifactSweep {
    /// Entries removed (`/tmp/.X11-unix/X1`, `/tmp/.X1-lock`).
    pub removed: Vec<String>,
    /// Entries of the session that are still there.
    pub remaining: Vec<String>,
}

/// Removes the sockets and lock files of `displays` that the compositor
/// `displays.lock_pid` of `uid` left, and reports what remains. Safe as
/// root and as the session user (see the module documentation); a
/// directory that is not the expected root-owned sticky one is not touched
/// and its entries count as remaining when they exist.
#[must_use]
pub fn remove_session_artifacts(
    dirs: &X11Dirs,
    uid: u32,
    displays: &SessionDisplays,
) -> ArtifactSweep {
    let mut sweep = ArtifactSweep::default();
    let unix = dirs.open_trusted(&dirs.unix_dir);
    let locks = dirs.open_trusted(&dirs.lock_dir);
    for display in displays.numbers() {
        let socket = x11_socket_name(display);
        let socket_path = dirs.unix_dir.join(&socket).display().to_string();
        let lock = x11_lock_name(display);
        let lock_path = dirs.lock_dir.join(&lock).display().to_string();
        // The lock is read first: only a display it still holds for the
        // session's compositor is the session's to clean up.
        let held = locks
            .as_ref()
            .is_some_and(|dir| session_lock_held(dir, &lock, uid, displays.lock_pid));
        let mut socket_left = false;
        match &unix {
            Some(dir) => match entry_at(dir, &socket) {
                Some(entry) if removable_socket(entry.kind, entry.owner, uid, held) => {
                    if rustix::fs::unlinkat(dir, socket.as_str(), AtFlags::empty()).is_ok() {
                        sweep.removed.push(socket_path);
                    } else {
                        sweep.remaining.push(socket_path);
                        socket_left = true;
                    }
                }
                _ => {}
            },
            None => {
                if std::fs::symlink_metadata(&socket_path).is_ok() {
                    sweep.remaining.push(socket_path);
                    socket_left = held;
                }
            }
        }
        match &locks {
            Some(dir) => {
                if held {
                    // A socket that could not be removed keeps its lock, so
                    // the number is not handed out over a live path.
                    if !socket_left
                        && rustix::fs::unlinkat(dir, lock.as_str(), AtFlags::empty()).is_ok()
                    {
                        sweep.removed.push(lock_path);
                    } else {
                        sweep.remaining.push(lock_path);
                    }
                }
            }
            None => {
                if std::fs::symlink_metadata(&lock_path).is_ok() {
                    sweep.remaining.push(lock_path);
                }
            }
        }
    }
    if !sweep.removed.is_empty() || !sweep.remaining.is_empty() {
        tracing::info!(
            target: target::SESSION,
            removed = ?sweep.removed,
            remaining = ?sweep.remaining,
            "X11 sockets and lock files the session's compositor left behind"
        );
    }
    sweep
}

/// The session's X11 artefacts still present: sockets of its displays
/// owned by `uid`, and lock files still holding its compositor's pid.
#[must_use]
pub fn session_artifacts_present(
    dirs: &X11Dirs,
    uid: u32,
    displays: &SessionDisplays,
) -> Vec<String> {
    let mut present = Vec::new();
    let unix = dirs.open_trusted(&dirs.unix_dir);
    let locks = dirs.open_trusted(&dirs.lock_dir);
    for display in displays.numbers() {
        let socket = x11_socket_name(display);
        let lock = x11_lock_name(display);
        let held = locks
            .as_ref()
            .is_some_and(|dir| session_lock_held(dir, &lock, uid, displays.lock_pid));
        if let Some(entry) = unix.as_ref().and_then(|dir| entry_at(dir, &socket)) {
            if removable_socket(entry.kind, entry.owner, uid, held) {
                present.push(dirs.unix_dir.join(&socket).display().to_string());
            }
        }
        if held {
            present.push(dirs.lock_dir.join(&lock).display().to_string());
        }
    }
    present
}

/// The compositor's Xwayland child in the session scope, with what its
/// command line names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XwaylandProcess {
    /// Its pid.
    pub pid: u32,
    /// The public display it serves.
    pub display: u32,
    /// Its authority file.
    pub auth: PathBuf,
}

fn parent_pid(pid: i32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // `pid (comm) state ppid …`; comm may hold spaces and parentheses.
    let after = stat.rsplit_once(')')?.1;
    after.split_whitespace().nth(1)?.parse().ok()
}

/// Waits for the compositor's Xwayland among the session scope's
/// processes. Mutter starts it while it opens its display, shortly after
/// the Wayland socket appears.
pub async fn find_xwayland(
    scope: &Path,
    uid: u32,
    compositor_pid: u32,
    timeout: Duration,
) -> Option<XwaylandProcess> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let found = super::stack::scope_processes(scope, uid)
            .into_iter()
            .filter(|pid| parent_pid(*pid) == Some(compositor_pid))
            .find_map(|pid| {
                let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
                let args = parse_xwayland_cmdline(&cmdline)?;
                Some(XwaylandProcess {
                    pid: u32::try_from(pid).ok()?,
                    display: args.display,
                    auth: args.auth,
                })
            });
        if found.is_some() || tokio::time::Instant::now() >= deadline {
            return found;
        }
        tokio::time::sleep(POLL).await;
    }
}

/// Where Xwayland's glamor renders, from the libraries it mapped.
#[must_use]
pub fn glamor(pid: u32) -> GlamorRoute {
    std::fs::read_to_string(format!("/proc/{pid}/maps"))
        .map_or(GlamorRoute::Unknown, |maps| glamor_route_from_maps(&maps))
}

/// What the X11 client proof established.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct X11ClientProof {
    /// The server accepted the session cookie and answered.
    pub served: bool,
    /// The compositor manages the screen (`_NET_SUPPORTING_WM_CHECK`).
    pub window_manager: bool,
}

/// Connects once to the public display as an X11 client with the session's
/// cookie and waits for the window manager. The socket must be a socket of
/// `uid` in the trusted `/tmp/.X11-unix`; the cookie must come from
/// Mutter's authority file inside `runtime_dir`. Bounded by `timeout`: a
/// server that never answers is disconnected, not waited for.
///
/// # Errors
///
/// Why no client could be served.
pub async fn prove_client(
    dirs: &X11Dirs,
    uid: u32,
    runtime_dir: &Path,
    display: u32,
    auth: &Path,
    timeout: Duration,
) -> Result<X11ClientProof, String> {
    if !is_mutter_xauthority(auth, runtime_dir) {
        return Err(format!(
            "the X authority {} is not Mutter's in the session's runtime directory",
            auth.display()
        ));
    }
    let cookie = read_cookie(auth, uid, display)?;
    let unix = dirs.open_trusted(&dirs.unix_dir).ok_or_else(|| {
        format!(
            "{} is not a root-owned sticky directory",
            dirs.unix_dir.display()
        )
    })?;
    let socket = x11_socket_name(display);
    match entry_at(&unix, &socket) {
        Some(entry) if entry.kind == ArtifactType::Socket && entry.owner == uid => {}
        _ => return Err(format!("no X11 socket of this session for :{display}")),
    }
    let path = dirs.unix_dir.join(socket);
    let stream = std::os::unix::net::UnixStream::connect(&path)
        .map_err(|error| format!("connecting to {}: {error}", path.display()))?;
    let unblock = stream
        .try_clone()
        .map_err(|error| format!("X11 socket: {error}"))?;
    let deadline = std::time::Instant::now() + timeout;
    // Supervised: on timeout the socket is shut down below, which ends the
    // blocked client call and its thread.
    #[allow(clippy::disallowed_methods)]
    let task = tokio::task::spawn_blocking(move || client_proof(stream, cookie, deadline));
    match tokio::time::timeout(timeout + Duration::from_millis(500), task).await {
        Ok(Ok(result)) => result,
        Ok(Err(error)) => Err(format!("X11 client proof failed: {error}")),
        Err(_) => {
            // Wakes the blocked client so its thread ends.
            let _ = unblock.shutdown(std::net::Shutdown::Both);
            Err(format!(
                "display :{display} did not answer within {timeout:?}"
            ))
        }
    }
}

fn read_cookie(auth: &Path, uid: u32, display: u32) -> Result<Vec<u8>, String> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC)
        .open(auth)
        .map_err(|error| format!("reading {}: {error}", auth.display()))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("reading {}: {error}", auth.display()))?;
    if !metadata.is_file() || metadata.uid() != uid {
        return Err(format!("{} is not the session's own file", auth.display()));
    }
    let mut contents = Vec::new();
    file.take(MAX_XAUTHORITY_BYTES)
        .read_to_end(&mut contents)
        .map_err(|error| format!("reading {}: {error}", auth.display()))?;
    parse_xauthority_cookie(&contents, display)
        .ok_or_else(|| format!("{} holds no MIT-MAGIC-COOKIE-1", auth.display()))
}

fn client_proof(
    stream: std::os::unix::net::UnixStream,
    cookie: Vec<u8>,
    deadline: std::time::Instant,
) -> Result<X11ClientProof, String> {
    use x11rb::connection::Connection as _;
    use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _};
    use x11rb::rust_connection::{DefaultStream, RustConnection};

    let (stream, _peer) =
        DefaultStream::from_unix_stream(stream).map_err(|error| format!("X11 stream: {error}"))?;
    let connection = RustConnection::connect_to_stream_with_auth_info(
        stream,
        0,
        arcen_session::wayland_x11::MIT_MAGIC_COOKIE.to_vec(),
        cookie,
    )
    .map_err(|error| format!("X11 setup refused: {error}"))?;
    let root = connection.setup().roots.first().map(|screen| screen.root);
    let Some(root) = root else {
        return Err("the X server reports no screen".to_owned());
    };
    let atom = connection
        .intern_atom(true, b"_NET_SUPPORTING_WM_CHECK")
        .map_err(|error| error.to_string())?
        .reply()
        .map_err(|error| error.to_string())?
        .atom;
    loop {
        let managed = atom != 0
            && connection
                .get_property(false, root, atom, AtomEnum::WINDOW, 0, 1)
                .map_err(|error| error.to_string())?
                .reply()
                .map_err(|error| error.to_string())?
                .value32()
                .and_then(|mut values| values.next())
                .is_some_and(|window| window != 0);
        if managed || std::time::Instant::now() >= deadline {
            return Ok(X11ClientProof {
                served: true,
                window_manager: managed,
            });
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// One output a pointer proof drives, in compositor logical pixels.
#[derive(Debug, Clone)]
pub struct PointerTarget {
    /// The session's input socket.
    pub input_socket: PathBuf,
    /// Output connector the coordinates are for.
    pub connector: String,
    /// Output generation.
    pub generation: u64,
    /// Output origin and size.
    pub rect: (i32, i32, u32, u32),
}

/// What injecting absolute pointer motion did to the X server's pointer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PointerProof {
    /// Root coordinates the motion aimed at.
    pub expected: Vec<(i32, i32)>,
    /// Root coordinates the X server reported after each.
    pub observed: Vec<(i32, i32)>,
}

impl PointerProof {
    /// Every observed position is within two pixels of its target.
    #[must_use]
    pub fn followed(&self) -> bool {
        self.expected.len() == self.observed.len()
            && !self.expected.is_empty()
            && self
                .expected
                .iter()
                .zip(&self.observed)
                .all(|(want, got)| (want.0 - got.0).abs() <= 2 && (want.1 - got.1).abs() <= 2)
    }
}

/// Proves that pointer motion injected through the session's input socket
/// moves the X server's pointer: an X11 client maps an override-redirect
/// window over the output (so the pointer is over an X11 surface), the
/// motion goes to two points of the output, and the X server's own pointer
/// position (`QueryPointer` on the root) must follow. Blocking; bounded by
/// `timeout` per step.
///
/// # Errors
///
/// Why the proof could not be made (not whether the pointer followed: see
/// [`PointerProof::followed`]).
pub fn prove_pointer(
    dirs: &X11Dirs,
    uid: u32,
    runtime_dir: &Path,
    display: u32,
    auth: &Path,
    target: &PointerTarget,
    timeout: Duration,
) -> Result<PointerProof, String> {
    use std::io::{BufRead as _, Write as _};
    use x11rb::connection::Connection as _;
    use x11rb::protocol::xproto::{ConnectionExt as _, CreateWindowAux, EventMask, WindowClass};
    use x11rb::rust_connection::{DefaultStream, RustConnection};

    if !is_mutter_xauthority(auth, runtime_dir) {
        return Err("the X authority is not Mutter's in the runtime directory".to_owned());
    }
    let cookie = read_cookie(auth, uid, display)?;
    let unix = dirs
        .open_trusted(&dirs.unix_dir)
        .ok_or_else(|| format!("{} is not trusted", dirs.unix_dir.display()))?;
    let socket = x11_socket_name(display);
    match entry_at(&unix, &socket) {
        Some(entry) if entry.kind == ArtifactType::Socket && entry.owner == uid => {}
        _ => return Err(format!("no X11 socket of this session for :{display}")),
    }
    let stream = std::os::unix::net::UnixStream::connect(dirs.unix_dir.join(socket))
        .map_err(|error| format!("X11 socket: {error}"))?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|error| error.to_string())?;
    let (stream, _) =
        DefaultStream::from_unix_stream(stream).map_err(|error| format!("X11 stream: {error}"))?;
    let connection = RustConnection::connect_to_stream_with_auth_info(
        stream,
        0,
        arcen_session::wayland_x11::MIT_MAGIC_COOKIE.to_vec(),
        cookie,
    )
    .map_err(|error| format!("X11 setup refused: {error}"))?;
    let screen = connection
        .setup()
        .roots
        .first()
        .ok_or("the X server reports no screen")?
        .clone();
    let (x, y, width, height) = target.rect;
    let window = connection
        .generate_id()
        .map_err(|error| error.to_string())?;
    let x11_error = |error: &dyn std::fmt::Display| error.to_string();
    connection
        .create_window(
            screen.root_depth,
            window,
            screen.root,
            i16::try_from(x).unwrap_or(0),
            i16::try_from(y).unwrap_or(0),
            u16::try_from(width).unwrap_or(u16::MAX),
            u16::try_from(height).unwrap_or(u16::MAX),
            0,
            WindowClass::INPUT_OUTPUT,
            0,
            &CreateWindowAux::new()
                .override_redirect(1)
                .background_pixel(screen.black_pixel)
                .event_mask(EventMask::POINTER_MOTION | EventMask::ENTER_WINDOW),
        )
        .map_err(|error| x11_error(&error))?;
    connection
        .map_window(window)
        .map_err(|error| x11_error(&error))?;
    connection.flush().map_err(|error| x11_error(&error))?;
    std::thread::sleep(Duration::from_millis(500));

    let input = std::os::unix::net::UnixStream::connect(&target.input_socket)
        .map_err(|error| format!("input socket: {error}"))?;
    input
        .set_read_timeout(Some(timeout))
        .map_err(|error| error.to_string())?;
    let mut replies =
        std::io::BufReader::new(input.try_clone().map_err(|error| error.to_string())?);
    let mut writer = input;
    let mut proof = PointerProof {
        expected: Vec::new(),
        observed: Vec::new(),
    };
    for (fx, fy) in [(0.3_f64, 0.4_f64), (0.7, 0.6)] {
        let px = (f64::from(width) * fx).round();
        let py = (f64::from(height) * fy).round();
        let mut line = serde_json::to_vec(&super::InputRequest::new(
            super::InputCommand::PointerAbsolute {
                output: target.connector.clone(),
                generation: target.generation,
                x: px,
                y: py,
            },
            timeout,
        ))
        .map_err(|error| error.to_string())?;
        line.push(b'\n');
        writer
            .write_all(&line)
            .map_err(|error| format!("input socket write: {error}"))?;
        let mut reply = String::new();
        replies
            .read_line(&mut reply)
            .map_err(|error| format!("input socket read: {error}"))?;
        let accepted = serde_json::from_str::<super::InputReply>(&reply)
            .map_err(|error| format!("input reply: {error}"))?;
        if !accepted.ok {
            return Err(format!("motion refused: {:?}", accepted.error));
        }
        std::thread::sleep(Duration::from_millis(250));
        let pointer = connection
            .query_pointer(screen.root)
            .map_err(|error| x11_error(&error))?
            .reply()
            .map_err(|error| x11_error(&error))?;
        #[allow(clippy::cast_possible_truncation)] // Whole stream pixels.
        proof.expected.push((x + px as i32, y + py as i32));
        proof
            .observed
            .push((i32::from(pointer.root_x), i32::from(pointer.root_y)));
    }
    let _ = connection.destroy_window(window);
    let _ = connection.flush();
    Ok(proof)
}

/// The failure a served client reports when the screen is unmanaged.
pub const NO_WINDOW_MANAGER: &str = "the X11 screen has no window manager";

/// Proves the compositor's Xwayland serves X11 clients of this session:
/// its displays were locked by the compositor, its Xwayland child names the
/// public one and Mutter's cookie in the runtime directory, and a client
/// with that cookie is served on a managed screen. Every failure is a
/// visible [`super::ReadyX11::failed`], never a failed session: the desktop
/// does not depend on its optional X server.
pub async fn session_readiness(
    scope: &Path,
    uid: u32,
    compositor_pid: Option<u32>,
    runtime_dir: &Path,
    displays: Option<SessionDisplays>,
    timeout: Duration,
) -> super::ReadyX11 {
    let Some(displays) = displays else {
        return super::ReadyX11::failed("the compositor locked no pair of X11 displays", None);
    };
    let Some(compositor_pid) = compositor_pid else {
        return super::ReadyX11::failed("the compositor is not running", Some(displays));
    };
    let deadline = tokio::time::Instant::now() + timeout;
    let Some(xwayland) = find_xwayland(scope, uid, compositor_pid, timeout).await else {
        return super::ReadyX11::failed(
            format!("the compositor started no Xwayland within {timeout:?}"),
            Some(displays),
        );
    };
    if xwayland.display != displays.public {
        return super::ReadyX11::failed(
            format!(
                "Xwayland serves :{} but the compositor locked :{}",
                xwayland.display, displays.public
            ),
            Some(displays),
        );
    }
    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
    let glamor = glamor(xwayland.pid);
    match prove_client(
        &X11Dirs::production(),
        uid,
        runtime_dir,
        displays.public,
        &xwayland.auth,
        remaining.max(Duration::from_secs(1)),
    )
    .await
    {
        Ok(proof) if proof.served && proof.window_manager => super::ReadyX11 {
            ready: true,
            failure: None,
            displays: Some(displays),
            xauthority: Some(xwayland.auth),
            glamor,
        },
        Ok(_) => super::ReadyX11 {
            glamor,
            ..super::ReadyX11::failed(NO_WINDOW_MANAGER, Some(displays))
        },
        Err(error) => super::ReadyX11 {
            glamor,
            ..super::ReadyX11::failed(error, Some(displays))
        },
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn own_uid() -> u32 {
        nix::unistd::Uid::effective().as_raw()
    }

    /// A private fixture standing in for `/tmp` and `/tmp/.X11-unix`, under
    /// the test run's `TMPDIR`.
    fn fixture(name: &str) -> X11Dirs {
        let root = std::env::temp_dir().join(format!("xwayland-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let unix = root.join(".X11-unix");
        std::fs::create_dir_all(&unix).expect("fixture");
        for dir in [&root, &unix] {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o1777)).expect("sticky");
        }
        X11Dirs {
            lock_dir: root,
            unix_dir: unix,
            owner: own_uid(),
            sticky: true,
        }
    }

    #[test]
    fn mutter_xauthority_is_the_one_own_regular_file() {
        let runtime = std::env::temp_dir().join(format!("xwayland-auth-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&runtime);
        std::fs::create_dir_all(&runtime).expect("fixture");
        assert_eq!(mutter_xauthority(&runtime, own_uid()), None);
        let target = runtime.join("elsewhere");
        std::fs::write(&target, b"cookie").expect("target");
        std::os::unix::fs::symlink(&target, runtime.join(".mutter-Xwaylandauth.LINK1"))
            .expect("link");
        assert_eq!(mutter_xauthority(&runtime, own_uid()), None);
        let auth = runtime.join(".mutter-Xwaylandauth.ABC123");
        std::fs::write(&auth, b"cookie").expect("auth");
        assert_eq!(mutter_xauthority(&runtime, own_uid()), Some(auth));
        assert_eq!(mutter_xauthority(&runtime, own_uid().wrapping_add(1)), None);
        std::fs::write(runtime.join(".mutter-Xwaylandauth.DEF456"), b"cookie").expect("second");
        assert_eq!(mutter_xauthority(&runtime, own_uid()), None);
        let _ = std::fs::remove_dir_all(&runtime);
    }

    fn bind_socket(path: &Path) -> std::os::unix::net::UnixListener {
        std::os::unix::net::UnixListener::bind(path).expect("bind")
    }

    #[test]
    fn locks_of_the_compositor_name_the_session_displays() {
        let dirs = fixture("discover");
        let uid = own_uid();
        std::fs::write(dirs.lock_dir.join(".X3-lock"), b"       777\n").expect("lock");
        std::fs::write(dirs.lock_dir.join(".X5-lock"), b"       777\n").expect("lock");
        std::fs::write(dirs.lock_dir.join(".X4-lock"), b"       778\n").expect("lock");
        std::fs::write(dirs.lock_dir.join(".X9-lock"), b"garbage").expect("lock");
        std::os::unix::fs::symlink(
            dirs.lock_dir.join(".X3-lock"),
            dirs.lock_dir.join(".X6-lock"),
        )
        .expect("symlink");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("runtime");
        let found = runtime.block_on(discover(&dirs, uid, 777, Duration::from_millis(200)));
        assert_eq!(
            found,
            Some(SessionDisplays {
                public: 3,
                private: 5,
                lock_pid: 777
            })
        );
        assert_eq!(
            runtime.block_on(discover(&dirs, uid, 779, Duration::from_millis(100))),
            None
        );
        let _ = std::fs::remove_dir_all(&dirs.lock_dir);
    }

    #[test]
    fn the_sweep_removes_only_the_sessions_own_artefacts() {
        let dirs = fixture("sweep");
        let uid = own_uid();
        let displays = SessionDisplays {
            public: 1,
            private: 2,
            lock_pid: 900,
        };
        let _one = bind_socket(&dirs.unix_dir.join("X1"));
        let _three = bind_socket(&dirs.unix_dir.join("X3"));
        // `X2` is a symlink to a socket: never followed, never removed.
        std::os::unix::fs::symlink(dirs.unix_dir.join("X3"), dirs.unix_dir.join("X2"))
            .expect("symlink");
        std::fs::write(dirs.lock_dir.join(".X1-lock"), b"       900\n").expect("lock");
        // Another compositor's lock for one of our numbers stays.
        std::fs::write(dirs.lock_dir.join(".X2-lock"), b"       901\n").expect("lock");
        std::fs::write(dirs.lock_dir.join(".X3-lock"), b"       900\n").expect("lock");

        assert_eq!(
            session_artifacts_present(&dirs, uid, &displays),
            vec![
                dirs.unix_dir.join("X1").display().to_string(),
                dirs.lock_dir.join(".X1-lock").display().to_string(),
            ]
        );
        let sweep = remove_session_artifacts(&dirs, uid, &displays);
        assert_eq!(
            sweep.removed,
            vec![
                dirs.unix_dir.join("X1").display().to_string(),
                dirs.lock_dir.join(".X1-lock").display().to_string(),
            ]
        );
        assert!(sweep.remaining.is_empty());
        assert!(!dirs.unix_dir.join("X1").exists());
        assert!(std::fs::symlink_metadata(dirs.unix_dir.join("X2")).is_ok());
        assert!(dirs.unix_dir.join("X3").exists());
        assert!(dirs.lock_dir.join(".X2-lock").exists());
        assert!(dirs.lock_dir.join(".X3-lock").exists());
        assert!(session_artifacts_present(&dirs, uid, &displays).is_empty());
        let _ = std::fs::remove_dir_all(&dirs.lock_dir);
    }

    #[test]
    fn a_freed_display_taken_by_another_desktop_keeps_its_socket() {
        let dirs = fixture("freed");
        let uid = own_uid();
        let displays = SessionDisplays {
            public: 1,
            private: 2,
            lock_pid: 900,
        };
        // The same user's next desktop took :1 (its lock names its own
        // compositor) and is binding :2 before writing its lock.
        let _one = bind_socket(&dirs.unix_dir.join("X1"));
        std::fs::write(dirs.lock_dir.join(".X1-lock"), b"       901\n").expect("lock");
        let _two = bind_socket(&dirs.unix_dir.join("X2"));

        assert!(session_artifacts_present(&dirs, uid, &displays).is_empty());
        let sweep = remove_session_artifacts(&dirs, uid, &displays);
        assert_eq!(sweep, ArtifactSweep::default());
        assert!(dirs.unix_dir.join("X1").exists());
        assert!(dirs.unix_dir.join("X2").exists());
        assert!(dirs.lock_dir.join(".X1-lock").exists());
        let _ = std::fs::remove_dir_all(&dirs.lock_dir);
    }

    #[test]
    fn an_untrusted_directory_is_never_swept() {
        let dirs = fixture("untrusted");
        let uid = own_uid();
        let displays = SessionDisplays {
            public: 1,
            private: 2,
            lock_pid: 900,
        };
        let _one = bind_socket(&dirs.unix_dir.join("X1"));
        std::fs::write(dirs.lock_dir.join(".X1-lock"), b"       900\n").expect("lock");
        std::fs::set_permissions(&dirs.unix_dir, std::fs::Permissions::from_mode(0o777))
            .expect("not sticky");
        let wrong_owner = X11Dirs {
            owner: uid.wrapping_add(1),
            ..dirs.clone()
        };
        let sweep = remove_session_artifacts(&wrong_owner, uid, &displays);
        assert!(sweep.removed.is_empty());
        assert_eq!(sweep.remaining.len(), 2);
        // The socket cannot be removed, so its lock is kept with it.
        let sweep = remove_session_artifacts(&dirs, uid, &displays);
        assert!(sweep.removed.is_empty());
        assert_eq!(
            sweep.remaining,
            vec![
                dirs.unix_dir.join("X1").display().to_string(),
                dirs.lock_dir.join(".X1-lock").display().to_string(),
            ]
        );
        assert!(dirs.unix_dir.join("X1").exists());
        assert!(dirs.lock_dir.join(".X1-lock").exists());
        let _ = std::fs::remove_dir_all(&dirs.lock_dir);
    }

    #[test]
    fn the_parent_pid_survives_a_command_name_with_parentheses() {
        let own = i32::try_from(std::process::id()).expect("pid");
        assert_eq!(parent_pid(own), Some(std::os::unix::process::parent_id()));
    }
}
