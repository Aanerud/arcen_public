//! The native-seat pen proof: a Wayland client of the desktop's compositor
//! that watches `zwp_tablet_manager_v2` (see [`arcen_session::native_seat`]).
//!
//! It binds the seat and the tablet manager, asks for the seat's tablets,
//! and records every tablet name. Mutter announces a tool only to the client
//! whose surface the tool focuses (`meta-wayland-tablet-tool.c`,
//! `meta_wayland_tablet_tool_set_focus` → `ensure_resource`), so the client
//! also maps one fullscreen surface (a plain shared-memory buffer) for the
//! pen to land on. Once the desktop's own tablet is there and the surface is
//! mapped it
//! calls `bring_pen`, which makes the pen come into proximity, and calls it
//! again every [`BRING_AGAIN`] until a tool is announced (a shell grab, such
//! as GNOME Shell's overview at start, holds tablet events back). The
//! compositor announces the tool (type and capabilities) to every client of
//! the tablet seat, with no surface or focus needed. Nothing is drawn and
//! no input is taken beyond the pen's own proximity over that surface.
//!
//! The wire protocol is spoken directly, like [`super::wl_keymap`], whose
//! message helpers it shares.

use std::collections::{BTreeSet, VecDeque};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

use arcen_session::native_seat::TabletObservation;

use super::wl_keymap::{put_string, request, Args, Connection, KeymapClientError};

const DISPLAY_ID: u32 = 1;
const REGISTRY_ID: u32 = 2;
const SYNC_ID: u32 = 3;
// Client object ids, in the order the client creates them: libwayland
// refuses a new id that skips one ("invalid arguments").
const SEAT_ID: u32 = 4;
const MANAGER_ID: u32 = 5;
const COMPOSITOR_ID: u32 = 6;
const SHM_ID: u32 = 7;
const WM_BASE_ID: u32 = 8;
const SURFACE_ID: u32 = 9;
const XDG_SURFACE_ID: u32 = 10;
const TOPLEVEL_ID: u32 = 11;
const TABLET_SEAT_ID: u32 = 12;
const POOL_ID: u32 = 13;
const BUFFER_ID: u32 = 14;

/// `wl_shm.format` xrgb8888.
const SHM_FORMAT_XRGB8888: u32 = 1;
/// The surface's size when the compositor leaves it to the client.
const FALLBACK_SIZE: (u32, u32) = (1920, 1080);

const TABLET_SEAT_TABLET_ADDED: u16 = 0;
const TABLET_SEAT_TOOL_ADDED: u16 = 1;
const TABLET_NAME: u16 = 0;
const TABLET_DONE: u16 = 3;
const TOOL_TYPE: u16 = 0;
const TOOL_CAPABILITY: u16 = 3;
const TOOL_DONE: u16 = 4;

/// How often the pen is brought again while no tool was announced.
const BRING_AGAIN: Duration = Duration::from_millis(1500);

/// Watches the compositor at `socket` until the tablet `pen_name` and a pen
/// tool are announced, or `budget` runs out; `bring_pen` is called when the
/// tablet is there and again every [`BRING_AGAIN`] until a tool is
/// announced. What was seen is returned either way.
///
/// # Errors
///
/// When the socket fails before anything was seen, or the compositor
/// reports a protocol error or offers no tablet manager.
pub fn observe(
    socket: &Path,
    pen_name: &str,
    budget: Duration,
    mut bring_pen: impl FnMut(),
) -> Result<TabletObservation, KeymapClientError> {
    let stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(Duration::from_millis(200)))?;
    let mut connection = Connection {
        stream,
        buffer: Vec::new(),
        fds: VecDeque::new(),
    };
    let deadline = Instant::now() + budget;
    connection.send(&request(DISPLAY_ID, 1, &REGISTRY_ID.to_le_bytes()))?;
    connection.send(&request(DISPLAY_ID, 0, &SYNC_ID.to_le_bytes()))?;
    let mut seat = None;
    let mut manager = None;
    let mut compositor = None;
    let mut shm = None;
    let mut wm_base = None;
    let mut synced = false;
    while !synced {
        for event in next(&mut connection, deadline)? {
            match (event.object, event.opcode) {
                (DISPLAY_ID, 0) => return Err(compositor_error(&event.args)),
                (REGISTRY_ID, 0) => {
                    let mut args = Args::new(&event.args);
                    let name = args.uint()?;
                    let interface = args.string()?;
                    match interface.as_str() {
                        "wl_seat" if seat.is_none() => seat = Some(name),
                        "zwp_tablet_manager_v2" => manager = Some(name),
                        "wl_compositor" => compositor = Some(name),
                        "wl_shm" => shm = Some(name),
                        "xdg_wm_base" => wm_base = Some(name),
                        _ => {}
                    }
                }
                (SYNC_ID, 0) => synced = true,
                _ => {}
            }
        }
    }
    let seat = seat.ok_or(KeymapClientError::NoSeat)?;
    let manager = manager.ok_or(KeymapClientError::Protocol("no zwp_tablet_manager_v2"))?;
    let compositor = compositor.ok_or(KeymapClientError::Protocol("no wl_compositor"))?;
    let shm = shm.ok_or(KeymapClientError::Protocol("no wl_shm"))?;
    let wm_base = wm_base.ok_or(KeymapClientError::Protocol("no xdg_wm_base"))?;
    for (name, interface, id) in [
        (seat, "wl_seat", SEAT_ID),
        (manager, "zwp_tablet_manager_v2", MANAGER_ID),
        (compositor, "wl_compositor", COMPOSITOR_ID),
        (shm, "wl_shm", SHM_ID),
        (wm_base, "xdg_wm_base", WM_BASE_ID),
    ] {
        let mut bind = Vec::new();
        bind.extend_from_slice(&name.to_le_bytes());
        put_string(&mut bind, interface);
        bind.extend_from_slice(&1_u32.to_le_bytes());
        bind.extend_from_slice(&id.to_le_bytes());
        connection.send(&request(REGISTRY_ID, 0, &bind))?;
    }
    // One fullscreen toplevel, committed without a buffer until configured.
    connection.send(&request(COMPOSITOR_ID, 0, &SURFACE_ID.to_le_bytes()))?;
    connection.send(&request(
        WM_BASE_ID,
        2,
        &words(&[XDG_SURFACE_ID, SURFACE_ID]),
    ))?;
    connection.send(&request(XDG_SURFACE_ID, 1, &TOPLEVEL_ID.to_le_bytes()))?;
    connection.send(&request(TOPLEVEL_ID, 11, &0_u32.to_le_bytes()))?;
    connection.send(&request(SURFACE_ID, 6, &[]))?;
    let mut get_seat = Vec::new();
    get_seat.extend_from_slice(&TABLET_SEAT_ID.to_le_bytes());
    get_seat.extend_from_slice(&SEAT_ID.to_le_bytes());
    connection.send(&request(MANAGER_ID, 0, &get_seat))?;

    let mut observation = TabletObservation::default();
    let mut tablets = BTreeSet::new();
    let mut tools = BTreeSet::new();
    let mut pen_tool_done = false;
    let mut brought: Option<Instant> = None;
    let mut size = (0, 0);
    let mut mapped = false;
    let mut tablet_seen = false;
    // Kept open until the client is done: the compositor reads the pixels.
    let mut _buffer_memory = None;
    while Instant::now() < deadline && !pen_tool_done {
        if brought.is_none() && tablet_seen && mapped {
            bring_pen();
            brought = Some(Instant::now());
        }
        if brought.is_some_and(|at| at.elapsed() >= BRING_AGAIN) && tools.is_empty() {
            bring_pen();
            brought = Some(Instant::now());
        }
        let events = match next(&mut connection, deadline.min(Instant::now() + BRING_AGAIN)) {
            Ok(events) => events,
            Err(KeymapClientError::Io(error)) if is_timeout(&error) => {
                if Instant::now() >= deadline {
                    break;
                }
                continue;
            }
            Err(error) => return Err(error),
        };
        for event in events {
            let mut args = Args::new(&event.args);
            match (event.object, event.opcode) {
                (DISPLAY_ID, 0) => return Err(compositor_error(&event.args)),
                (WM_BASE_ID, 0) => {
                    let serial = args.uint()?;
                    connection.send(&request(WM_BASE_ID, 3, &serial.to_le_bytes()))?;
                }
                (TOPLEVEL_ID, 0) => {
                    size = (args.uint()?, args.uint()?);
                }
                (XDG_SURFACE_ID, 0) => {
                    let serial = args.uint()?;
                    connection.send(&request(XDG_SURFACE_ID, 4, &serial.to_le_bytes()))?;
                    if !mapped {
                        let (width, height) = if size.0 > 0 && size.1 > 0 {
                            size
                        } else {
                            FALLBACK_SIZE
                        };
                        _buffer_memory = Some(map_buffer(&mut connection, width, height)?);
                        mapped = true;
                    }
                }
                (TABLET_SEAT_ID, TABLET_SEAT_TABLET_ADDED) => {
                    tablets.insert(args.uint()?);
                }
                (TABLET_SEAT_ID, TABLET_SEAT_TOOL_ADDED) => {
                    tools.insert(args.uint()?);
                }
                (object, TABLET_NAME) if tablets.contains(&object) => {
                    observation.tablets.push(args.string()?);
                }
                (object, TABLET_DONE) if tablets.contains(&object) => {
                    tablet_seen |= observation.tablets.iter().any(|name| name == pen_name);
                }
                (object, TOOL_TYPE) if tools.contains(&object) => {
                    observation.tool_types.push(args.uint()?);
                }
                (object, TOOL_CAPABILITY) if tools.contains(&object) => {
                    observation.tool_capabilities.push(args.uint()?);
                }
                (object, TOOL_DONE) if tools.contains(&object) => {
                    pen_tool_done = observation
                        .tool_types
                        .contains(&arcen_session::native_seat::TABLET_TOOL_TYPE_PEN);
                }
                _ => {}
            }
        }
    }
    Ok(observation)
}

fn words(values: &[u32]) -> Vec<u8> {
    values
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}

/// Creates a `width`×`height` shared-memory buffer, attaches it to the
/// surface and commits; returns the memory, which must outlive the client.
fn map_buffer(
    connection: &mut Connection,
    width: u32,
    height: u32,
) -> Result<std::os::fd::OwnedFd, KeymapClientError> {
    use std::os::fd::AsFd;
    let stride = width.saturating_mul(4);
    let size = stride.saturating_mul(height);
    let memory = rustix::fs::memfd_create("arcen-tablet-proof", rustix::fs::MemfdFlags::CLOEXEC)
        .map_err(std::io::Error::from)?;
    rustix::fs::ftruncate(&memory, u64::from(size)).map_err(std::io::Error::from)?;
    let mut pool = POOL_ID.to_le_bytes().to_vec();
    pool.extend_from_slice(&size.to_le_bytes());
    send_with_fd(connection, &request(SHM_ID, 0, &pool), memory.as_fd())?;
    connection.send(&request(
        POOL_ID,
        0,
        &words(&[BUFFER_ID, 0, width, height, stride, SHM_FORMAT_XRGB8888]),
    ))?;
    connection.send(&request(SURFACE_ID, 1, &words(&[BUFFER_ID, 0, 0])))?;
    connection.send(&request(SURFACE_ID, 2, &words(&[0, 0, width, height])))?;
    connection.send(&request(SURFACE_ID, 6, &[]))?;
    Ok(memory)
}

/// Sends `message` with `fd` in its ancillary data (`wl_shm.create_pool`:
/// the file descriptor argument travels out of band).
fn send_with_fd(
    connection: &mut Connection,
    message: &[u8],
    fd: std::os::fd::BorrowedFd<'_>,
) -> Result<(), KeymapClientError> {
    use std::os::fd::AsRawFd;
    let fds = [fd.as_raw_fd()];
    let control = [nix::sys::socket::ControlMessage::ScmRights(&fds)];
    let iov = [std::io::IoSlice::new(message)];
    let sent = nix::sys::socket::sendmsg::<()>(
        connection.stream.as_raw_fd(),
        &iov,
        &control,
        nix::sys::socket::MsgFlags::empty(),
        None,
    )
    .map_err(std::io::Error::from)?;
    if sent == message.len() {
        Ok(())
    } else {
        Err(KeymapClientError::Protocol("short write"))
    }
}

/// The next events, waiting at most until `deadline`.
fn next(
    connection: &mut Connection,
    deadline: Instant,
) -> Result<Vec<super::wl_keymap::Event>, KeymapClientError> {
    loop {
        match connection.next_events() {
            Ok(events) => return Ok(events),
            Err(KeymapClientError::Io(error)) if is_timeout(&error) => {
                if Instant::now() >= deadline {
                    return Err(KeymapClientError::Io(error));
                }
            }
            Err(error) => return Err(error),
        }
    }
}

fn is_timeout(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    )
}

fn compositor_error(args: &[u8]) -> KeymapClientError {
    let mut args = Args::new(args);
    let message = args
        .uint()
        .and_then(|_object| args.uint())
        .and_then(|_code| args.string())
        .unwrap_or_else(|_| "unreadable error".to_owned());
    KeymapClientError::Compositor(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    fn event(object: u32, opcode: u16, args: &[u8]) -> Vec<u8> {
        request(object, opcode, args)
    }

    fn string_args(value: &str) -> Vec<u8> {
        let mut args = Vec::new();
        put_string(&mut args, value);
        args
    }

    fn global(name: u32, interface: &str) -> Vec<u8> {
        let mut args = name.to_le_bytes().to_vec();
        put_string(&mut args, interface);
        args.extend_from_slice(&1_u32.to_le_bytes());
        event(REGISTRY_ID, 0, &args)
    }

    #[test]
    fn the_pen_is_brought_only_after_the_desktops_tablet_and_its_tool_is_recorded() {
        let dir = std::env::temp_dir().join(format!("arcen-tablet-proof-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("wayland-test");
        let _ = std::fs::remove_file(&socket);
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let (brought_tx, brought_rx) = std::sync::mpsc::channel::<()>();
        let server = std::thread::spawn(move || {
            let (mut client, _) = listener.accept().unwrap();
            let mut scratch = [0_u8; 4096];
            let _ = client.read(&mut scratch).unwrap();
            let mut first = Vec::new();
            first.extend(global(7, "wl_seat"));
            first.extend(global(9, "zwp_tablet_manager_v2"));
            first.extend(global(10, "wl_compositor"));
            first.extend(global(11, "wl_shm"));
            first.extend(global(12, "xdg_wm_base"));
            first.extend(event(SYNC_ID, 0, &0_u32.to_le_bytes()));
            client.write_all(&first).unwrap();
            let _ = client.read(&mut scratch).unwrap();
            let tablet = 0xff00_0001_u32;
            let mut tablets = Vec::new();
            tablets.extend(event(
                TABLET_SEAT_ID,
                TABLET_SEAT_TABLET_ADDED,
                &tablet.to_le_bytes(),
            ));
            tablets.extend(event(
                tablet,
                TABLET_NAME,
                &string_args("Arcen Seat Pen 0123456789abcdef"),
            ));
            tablets.extend(event(tablet, TABLET_DONE, &[]));
            // Not yet mapped: no pen until the surface is configured.
            client.write_all(&tablets).unwrap();
            assert!(
                brought_rx.recv_timeout(Duration::from_millis(400)).is_err(),
                "the pen waits for the surface"
            );
            let mut tablets = Vec::new();
            let mut toplevel = words(&[2560, 1440, 0]);
            toplevel.truncate(12);
            tablets.extend(event(TOPLEVEL_ID, 0, &toplevel));
            tablets.extend(event(XDG_SURFACE_ID, 0, &7_u32.to_le_bytes()));
            client.write_all(&tablets).unwrap();
            brought_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("the pen is brought after the tablet");
            let tool = 0xff00_0002_u32;
            let mut tools = Vec::new();
            tools.extend(event(
                TABLET_SEAT_ID,
                TABLET_SEAT_TOOL_ADDED,
                &tool.to_le_bytes(),
            ));
            tools.extend(event(tool, TOOL_TYPE, &0x140_u32.to_le_bytes()));
            tools.extend(event(tool, TOOL_CAPABILITY, &1_u32.to_le_bytes()));
            tools.extend(event(tool, TOOL_CAPABILITY, &2_u32.to_le_bytes()));
            tools.extend(event(tool, TOOL_DONE, &[]));
            client.write_all(&tools).unwrap();
            std::thread::sleep(Duration::from_millis(300));
        });
        let observation = observe(
            &socket,
            "Arcen Seat Pen 0123456789abcdef",
            Duration::from_secs(5),
            move || {
                let _ = brought_tx.send(());
            },
        )
        .unwrap();
        server.join().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(observation.tablets, ["Arcen Seat Pen 0123456789abcdef"]);
        assert_eq!(observation.tool_types, [0x140]);
        assert_eq!(observation.tool_capabilities, [1, 2]);
        let proof = arcen_session::native_seat::NativeSeatProof::from_observation(
            &observation,
            "Arcen Seat Pen 0123456789abcdef",
            arcen_session::polkit_remote::Admission::Admit,
            None,
            false,
        );
        assert!(proof.proven());
    }
}
