// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Host cursor watcher for the Linux X11 session.
//!
//! Opens a dedicated X11 connection, registers for `XFixes` cursor-change
//! notifications on the root window, and reads the desktop's actual cursor
//! image (`XFixesGetCursorImageAndName`) whenever it changes. Every image is
//! handed to the shared [`arcen_input::cursor::HostCursor`], which gives it a
//! stable id, ignores repeats and rate-limits new images; the result is
//! published as [`CursorFacts`] on a `tokio` watch channel. The input lane
//! worker sends a Deck that declared `cursor_image_v1` the image itself and
//! any other Deck the named shape of the cursor's X name
//! ([`arcen_input::cursor::shape_for_x_cursor_name`]). Only active while
//! [`CursorMode::Local`](arcen_protocol::messages::CursorMode::Local) is
//! negotiated — the cursor is already embedded in the video stream when
//! host-cursor mode is active.
//!
//! For a Deck that declared `cursor_position_v1` the same thread also reads
//! where the X pointer is (`QueryPointer`) and hands every change to the
//! attachment's [`DesktopPointer`] as a [`DesktopPointerSample`]: every 8 ms
//! while the pointer moves and every 33 ms while it rests. That source is
//! still needed for an application's warp (core warps generate no input
//! events), for a tablet the user remapped, and for host-side devices; it is
//! not the pen's: while a USB-bridged pen leads (ADR 0023) its positions
//! come from its own evdev node and the X pointer is not read at all. For
//! that pen the thread also proves the desktop's mapping from the
//! `xf86-input-wacom` devices of the pen's node (tablet area, rotation,
//! transformation matrix, absolute mode; [`x11_wacom_mapping`]) once a
//! second. It reads only; the input lane worker decides what the Deck hears
//! (`arcen_input::host_pointer`), so the echo of the Deck's own input is
//! never reported. This works for every preset, including Grading's
//! depth-30 XShm capture, whose video cannot carry the cursor.
//!
//! The watcher runs in its own OS thread (not a Tokio task) so the blocking
//! wait never ties up a Tokio worker thread. Changes that arrive together
//! (a theme's animated cursor, several windows switching at once) are
//! coalesced into one read. The thread exits when every receiver is gone
//! (the attachment ended) or the X11 connection drops.

use std::os::fd::AsFd;
use std::sync::Arc;
use std::time::{Duration, Instant};

use arcen_input::cursor::{CursorFacts, HostCursor, HostCursorOutcome, HostCursorSample};
use arcen_input::host_pointer::DesktopPointerSample;
use arcen_input::tablet_pointer::{x11_wacom_mapping, PenMapping, X11WacomTool};
use arcen_media::cursor::CursorImage;
use arcen_protocol::messages::CursorShapeKind;
use tokio::sync::watch;

use crate::media::desktop_pointer::DesktopPointer;
use arcen_input::tablet_pointer::PenDevice;

/// How long the watcher sleeps between checks that its attachment still
/// listens when the desktop's cursor does not change.
const IDLE_CHECK: Duration = Duration::from_millis(500);

/// Pointer read interval while the pointer moves (about 120 Hz).
const POINTER_MOVING_INTERVAL: Duration = Duration::from_millis(8);
/// Pointer read interval while it rests.
const POINTER_RESTING_INTERVAL: Duration = Duration::from_millis(33);
/// How long the pointer counts as moving after its last change.
const POINTER_MOVING_FOR: Duration = Duration::from_millis(500);
/// How often the root window's size is read again (a RandR resize).
const ROOT_SIZE_INTERVAL: Duration = Duration::from_secs(1);

/// What a watcher publishes.
pub struct Watcher {
    /// The desktop's current cursor.
    pub facts: watch::Receiver<Option<CursorFacts>>,
}

/// Start the cursor watcher thread. Returns a receiver of the desktop's
/// current cursor; with `pointer`, the thread also reads the X pointer into
/// it. The thread exits when every cursor receiver is dropped.
///
/// Returns `None` only if the thread cannot be spawned. An X11 connection
/// or `XFixes` failure ends the thread with a log line; the receiver then
/// never reports a cursor and the Deck keeps its own arrow.
///
/// `display` is the DISPLAY string (e.g. `":0"`), `xauthority` the path to
/// the Xauthority file for that display (may be `None` for sessions that do
/// not require one).
pub fn spawn(
    display: String,
    xauthority: Option<String>,
    pointer: Option<Arc<DesktopPointer>>,
) -> Option<Watcher> {
    let (tx, rx) = watch::channel(None);
    std::thread::Builder::new()
        .name("arcen-cursor-watcher".to_string())
        .spawn(move || run_watcher(display, xauthority, tx, pointer))
        .ok()?;
    Some(Watcher { facts: rx })
}

fn run_watcher(
    display: String,
    xauthority: Option<String>,
    tx: watch::Sender<Option<CursorFacts>>,
    pointer: Option<Arc<DesktopPointer>>,
) {
    // Point the X11 library at the correct display and authority file.
    // SAFETY: setenv is not thread-safe in multi-threaded programs, but this
    // watcher thread is spawned before any other thread touches DISPLAY or
    // XAUTHORITY for this thread's environment (each std::thread has its own
    // address-space-visible env; we only write before connecting, and the
    // values don't change after that). The x11rb connection reads them once
    // on `x11rb::connect`.
    if let Some(ref xauth) = xauthority {
        // SAFETY: single writer, thread-local-lifetime env only read by this
        // thread's connect call immediately below.
        unsafe {
            std::env::set_var("XAUTHORITY", xauth);
        }
    }

    let (conn, screen_num) = match x11rb::connect(Some(&display)) {
        Ok(pair) => pair,
        Err(error) => {
            // Rebind to avoid the tracing macro resolving `display` as
            // `tracing::field::display` (the function) rather than the
            // local String parameter.
            let x11_disp = display.as_str();
            tracing::warn!(
                target: crate::logging::target::SESSION,
                %error,
                x11_display = x11_disp,
                "cursor watcher: failed to connect to X11 display; \
                 cursor streaming unavailable for this session"
            );
            return;
        }
    };

    let mut pointer = pointer.map(PointerReader::new);
    if let Err(error) = init_and_watch(&conn, screen_num, &tx, pointer.as_mut()) {
        tracing::debug!(
            target: crate::logging::target::SESSION,
            %error,
            "cursor watcher exited"
        );
    }
}

/// What one read of the X cursor produced.
struct XCursorRead {
    serial: u32,
    sample: Option<HostCursorSample>,
    name: String,
}

/// One pointer read: root position and, when read again, the root size.
type PointerRead = (i32, i32, Option<(u32, u32)>);

/// Reads the X pointer on its own schedule into the desktop's pointer, and
/// proves a bridged pen's mapping.
struct PointerReader {
    pointer: Arc<DesktopPointer>,
    last: Option<(i32, i32)>,
    moved_at: Option<Instant>,
    next_read: Instant,
    root_size: Option<(u32, u32)>,
    root_size_at: Option<Instant>,
    /// The pen device whose mapping was last read, and when.
    mapping_read: Option<(PenDevice, Instant)>,
    xinput: Option<XInputAtoms>,
    failed: bool,
}

impl PointerReader {
    fn new(pointer: Arc<DesktopPointer>) -> Self {
        Self {
            pointer,
            last: None,
            moved_at: None,
            next_read: Instant::now(),
            root_size: None,
            root_size_at: None,
            mapping_read: None,
            xinput: None,
            failed: false,
        }
    }

    /// Reads the pointer when its time has come; returns when to read next.
    fn poll(
        &mut self,
        conn: &x11rb::rust_connection::RustConnection,
        root: u32,
        now: Instant,
    ) -> Option<Instant> {
        use x11rb::protocol::xproto::ConnectionExt as _;
        if self.failed || !self.pointer.is_followed() {
            return None;
        }
        if now < self.next_read {
            return Some(self.next_read);
        }
        self.check_pen_mapping(conn, now);
        // The pen's own positions lead: the X pointer only trails them.
        if self.pointer.pen_leads() {
            self.next_read = now + POINTER_MOVING_INTERVAL;
            return Some(self.next_read);
        }
        let read = || -> Result<PointerRead, Box<dyn std::error::Error>> {
            let reply = conn.query_pointer(root)?.reply()?;
            let size = if self
                .root_size_at
                .is_none_or(|at| now.saturating_duration_since(at) >= ROOT_SIZE_INTERVAL)
            {
                let geometry = conn.get_geometry(root)?.reply()?;
                Some((u32::from(geometry.width), u32::from(geometry.height)))
            } else {
                None
            };
            Ok((i32::from(reply.root_x), i32::from(reply.root_y), size))
        };
        match read() {
            Ok((x, y, size)) => {
                if let Some(size) = size {
                    self.root_size = Some(size);
                    self.root_size_at = Some(now);
                    self.pointer.set_desktop_size(size.0, size.1);
                }
                if self.last != Some((x, y)) {
                    self.last = Some((x, y));
                    self.moved_at = Some(now);
                    if let Some((root_width, root_height)) = self.root_size {
                        self.pointer.platform(DesktopPointerSample {
                            x,
                            y,
                            desktop_width: root_width,
                            desktop_height: root_height,
                            at: now,
                        });
                    }
                }
            }
            Err(error) => {
                self.failed = true;
                tracing::warn!(
                    target: crate::logging::target::SESSION,
                    %error,
                    "cursor watcher: the X pointer cannot be read; the Deck keeps drawing its own pointer"
                );
                return None;
            }
        }
        let moving = self
            .moved_at
            .is_some_and(|at| now.saturating_duration_since(at) < POINTER_MOVING_FOR);
        self.next_read = now
            + if moving {
                POINTER_MOVING_INTERVAL
            } else {
                POINTER_RESTING_INTERVAL
            };
        Some(self.next_read)
    }

    /// Reads how this X server maps the bridged pen, when a pen is read and
    /// its mapping was not read in the last second.
    fn check_pen_mapping(&mut self, conn: &x11rb::rust_connection::RustConnection, now: Instant) {
        let Some(device) = self.pointer.pen_device() else {
            self.mapping_read = None;
            return;
        };
        if self.mapping_read.is_some_and(|(read, at)| {
            read == device && now.saturating_duration_since(at) < ROOT_SIZE_INTERVAL
        }) {
            return;
        }
        self.mapping_read = Some((device, now));
        let mapping = match self.wacom_tools(conn, device) {
            Ok(tools) => x11_wacom_mapping(&tools, device.x_range, device.y_range),
            Err(error) => {
                tracing::debug!(
                    target: crate::logging::target::SESSION,
                    %error,
                    "cursor watcher: the pen's X devices cannot be read; the X pointer leads"
                );
                PenMapping::Unknown
            }
        };
        if mapping != self.pointer.mapping() {
            tracing::info!(
                target: crate::logging::target::SESSION,
                ?mapping,
                "cursor watcher: the bridged pen's mapping on this desktop"
            );
            self.pointer.set_mapping(mapping);
        }
    }

    /// The `xf86-input-wacom` stylus and eraser devices this X server made
    /// of the pen node `device`, as read from their properties. The cursor
    /// (puck) device on the same node is relative by default and is never a
    /// pen, so it is not judged (the shared lead never lets a puck lead).
    fn wacom_tools(
        &mut self,
        conn: &x11rb::rust_connection::RustConnection,
        device: PenDevice,
    ) -> Result<Vec<X11WacomTool>, Box<dyn std::error::Error>> {
        use x11rb::protocol::xinput::{ConnectionExt as _, DeviceClassData, ValuatorMode};
        let atoms = match self.xinput {
            Some(atoms) => atoms,
            None => {
                let atoms = XInputAtoms::intern(conn)?;
                self.xinput = Some(atoms);
                atoms
            }
        };
        let mut tools = Vec::new();
        let devices = conn
            .xinput_xi_query_device(x11rb::protocol::xinput::Device::ALL)?
            .reply()?;
        for info in devices.infos {
            let Some(node) =
                property(conn, info.deviceid, atoms.device_node)?.and_then(|value| match value {
                    x11rb::protocol::xinput::XIGetPropertyItems::Data8(bytes) => Some(bytes),
                    _ => None,
                })
            else {
                continue;
            };
            let pen_tool = ints(property(conn, info.deviceid, atoms.tool_type)?)
                .and_then(|values| values.first().copied())
                .and_then(|atom| u32::try_from(atom).ok())
                .is_some_and(|atom| atom == atoms.stylus || atom == atoms.eraser);
            if !pen_tool || !same_device(&node, device.device_number) {
                continue;
            }
            let tablet_area = ints(property(conn, info.deviceid, atoms.tablet_area)?)
                .and_then(|area| <[i64; 4]>::try_from(area.as_slice()).ok());
            // An 8-bit property in xf86-input-wacom.
            let rotation = ints(property(conn, info.deviceid, atoms.rotation)?)
                .and_then(|values| values.first().copied());
            let transform =
                property(conn, info.deviceid, atoms.transform)?.and_then(|value| match value {
                    x11rb::protocol::xinput::XIGetPropertyItems::Data32(words) => {
                        <[u32; 9]>::try_from(words.as_slice())
                            .ok()
                            .map(|words| words.map(f32::from_bits))
                    }
                    _ => None,
                });
            let absolute = info.classes.iter().any(|class| {
                matches!(
                    &class.data,
                    DeviceClassData::Valuator(valuator)
                        if valuator.number == 0 && valuator.mode == ValuatorMode::ABSOLUTE
                )
            });
            tools.push(X11WacomTool {
                tablet_area,
                rotation,
                transform,
                absolute,
            });
        }
        Ok(tools)
    }
}

/// The XInput properties a pen's mapping is read from.
#[derive(Debug, Clone, Copy)]
struct XInputAtoms {
    device_node: u32,
    tool_type: u32,
    stylus: u32,
    eraser: u32,
    tablet_area: u32,
    rotation: u32,
    transform: u32,
}

impl XInputAtoms {
    fn intern(
        conn: &x11rb::rust_connection::RustConnection,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        use x11rb::protocol::xinput::ConnectionExt as _;
        use x11rb::protocol::xproto::ConnectionExt as _;
        conn.xinput_xi_query_version(2, 2)?.reply()?;
        let atom = |name: &str| -> Result<u32, Box<dyn std::error::Error>> {
            Ok(conn.intern_atom(false, name.as_bytes())?.reply()?.atom)
        };
        Ok(Self {
            device_node: atom("Device Node")?,
            tool_type: atom("Wacom Tool Type")?,
            stylus: atom("STYLUS")?,
            eraser: atom("ERASER")?,
            tablet_area: atom("Wacom Tablet Area")?,
            rotation: atom("Wacom Rotation")?,
            transform: atom("Coordinate Transformation Matrix")?,
        })
    }
}

/// One XInput device property, `None` when the device does not have it.
fn property(
    conn: &x11rb::rust_connection::RustConnection,
    device: u16,
    atom: u32,
) -> Result<Option<x11rb::protocol::xinput::XIGetPropertyItems>, Box<dyn std::error::Error>> {
    use x11rb::protocol::xinput::ConnectionExt as _;
    let reply = conn
        .xinput_xi_get_property(
            device,
            false,
            atom,
            u32::from(x11rb::protocol::xproto::AtomEnum::ANY),
            0,
            16,
        )?
        .reply()?;
    Ok((reply.type_ != x11rb::NONE).then_some(reply.items))
}

/// An integer property's values, whatever its format (8, 16 or 32 bits,
/// signed as the X server stores `XA_INTEGER`).
fn ints(items: Option<x11rb::protocol::xinput::XIGetPropertyItems>) -> Option<Vec<i64>> {
    use x11rb::protocol::xinput::XIGetPropertyItems;
    match items? {
        XIGetPropertyItems::Data8(bytes) => Some(
            bytes
                .into_iter()
                .map(|byte| i64::from(i8::from_ne_bytes([byte])))
                .collect(),
        ),
        XIGetPropertyItems::Data16(halves) => Some(
            halves
                .into_iter()
                .map(|half| i64::from(i16::from_ne_bytes(half.to_ne_bytes())))
                .collect(),
        ),
        XIGetPropertyItems::Data32(words) => Some(
            words
                .into_iter()
                .map(|word| i64::from(i32::from_ne_bytes(word.to_ne_bytes())))
                .collect(),
        ),
        XIGetPropertyItems::InvalidValue(_) => None,
    }
}

/// Whether the X device's "Device Node" (a path, NUL-terminated) is the
/// character device `device_number`. The session's X server names the path,
/// so only one under `/dev/input/` without `..` is looked at, and it is only
/// `stat`ed, never opened.
fn same_device(node: &[u8], device_number: (u64, u64)) -> bool {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let path = node.split(|byte| *byte == 0).next().unwrap_or_default();
    if !path.starts_with(b"/dev/input/") || path.windows(2).any(|pair| pair == b"..") {
        return false;
    }
    std::fs::metadata(std::ffi::OsStr::from_bytes(path)).is_ok_and(|metadata| {
        metadata.file_type().is_char_device()
            && (
                nix::sys::stat::major(metadata.rdev()),
                nix::sys::stat::minor(metadata.rdev()),
            ) == device_number
    })
}

fn init_and_watch(
    conn: &x11rb::rust_connection::RustConnection,
    screen_num: usize,
    tx: &watch::Sender<Option<CursorFacts>>,
    mut pointer: Option<&mut PointerReader>,
) -> Result<(), Box<dyn std::error::Error>> {
    use x11rb::connection::Connection as _;
    use x11rb::protocol::xfixes::{ConnectionExt as _, CursorNotifyMask};

    // Initialise XFixes — required before any other XFixes call.
    conn.xfixes_query_version(5, 0)?.reply()?;

    let root = conn.setup().roots[screen_num].root;

    // Register for cursor-change notifications on the root window.
    conn.xfixes_select_cursor_input(root, CursorNotifyMask::DISPLAY_CURSOR)?
        .check()?;

    let mut host = HostCursor::default();
    let mut last_serial = None;
    let mut refused_logged = false;
    // The initial cursor is read and sent at once, so the Deck starts with
    // the right one even if it never changes during the session.
    let mut read_now = true;
    loop {
        if tx.is_closed() {
            return Ok(());
        }
        if read_now {
            read_now = false;
            let read = read_cursor(conn)?;
            if last_serial != Some(read.serial) {
                last_serial = Some(read.serial);
                match read.sample {
                    Some(sample) => {
                        let outcome = host.offer(sample, Instant::now());
                        publish(&host, tx, outcome, &read.name, &mut refused_logged);
                    }
                    None if !refused_logged => {
                        refused_logged = true;
                        tracing::warn!(
                            target: crate::logging::target::SESSION,
                            name = %read.name,
                            "cursor watcher: an X cursor image is out of range; the Deck keeps the previous cursor"
                        );
                    }
                    None => {}
                }
            }
        }
        if let Some(retry_at) = host.retry_at() {
            if Instant::now() >= retry_at {
                let outcome = host.poll(Instant::now());
                publish(&host, tx, outcome, "", &mut refused_logged);
                continue;
            }
        }
        // Drain what x11rb already buffered before waiting on the socket.
        let mut changed = false;
        while let Some(event) = conn.poll_for_event()? {
            if matches!(event, x11rb::protocol::Event::XfixesCursorNotify(_)) {
                changed = true;
            }
        }
        if changed {
            read_now = true;
            continue;
        }
        let pointer_due = pointer
            .as_deref_mut()
            .and_then(|reader| reader.poll(conn, root, Instant::now()));
        let wait = host
            .retry_at()
            .into_iter()
            .chain(pointer_due)
            .map(|at| at.saturating_duration_since(Instant::now()))
            .fold(IDLE_CHECK, Duration::min);
        let timeout = u16::try_from(wait.as_millis()).unwrap_or(u16::MAX).max(1);
        let mut fds = [nix::poll::PollFd::new(
            conn.stream().as_fd(),
            nix::poll::PollFlags::POLLIN,
        )];
        match nix::poll::poll(&mut fds, timeout) {
            Ok(_) | Err(nix::errno::Errno::EINTR) => {}
            Err(error) => return Err(error.into()),
        }
    }
}

fn publish(
    host: &HostCursor,
    tx: &watch::Sender<Option<CursorFacts>>,
    outcome: HostCursorOutcome,
    name: &str,
    refused_logged: &mut bool,
) {
    match outcome {
        HostCursorOutcome::Changed => {
            let current = host.current().cloned();
            tracing::debug!(
                target: crate::logging::target::SESSION,
                name,
                cursor = current.as_ref().map(CursorFacts::summary).as_deref(),
                images = host.registered_images(),
                "cursor watcher: cursor changed"
            );
            tx.send_replace(current);
        }
        HostCursorOutcome::Deferred => {
            tracing::trace!(
                target: crate::logging::target::SESSION,
                deferred = host.deferred(),
                "cursor watcher: new cursor images are rate-limited"
            );
        }
        HostCursorOutcome::ImageRefused(error) if !*refused_logged => {
            *refused_logged = true;
            tracing::warn!(
                target: crate::logging::target::SESSION,
                %error,
                "cursor watcher: an X cursor image could not be fitted to the wire; the Deck keeps the previous cursor"
            );
        }
        HostCursorOutcome::ImageRefused(_) | HostCursorOutcome::Unchanged => {}
    }
}

/// Reads the current cursor image and name from the X server.
fn read_cursor(
    conn: &x11rb::rust_connection::RustConnection,
) -> Result<XCursorRead, Box<dyn std::error::Error>> {
    use x11rb::protocol::xfixes::ConnectionExt as _;

    let reply = conn.xfixes_get_cursor_image_and_name()?.reply()?;
    let name = String::from_utf8_lossy(&reply.name).into_owned();
    let fallback = arcen_input::cursor::shape_for_x_cursor_name(&name);
    // A fully transparent image is how X clients hide the pointer (the
    // blank cursor a uinput tablet shows in proximity, a video player's
    // idle hide); a blank name says the same.
    let all_transparent = !reply.cursor_image.iter().any(|px| (px >> 24) != 0);
    let sample = if all_transparent || fallback == CursorShapeKind::Hidden {
        Some(HostCursorSample::Hidden)
    } else {
        CursorImage::from_premultiplied_argb32(
            u32::from(reply.width),
            u32::from(reply.height),
            (i64::from(reply.xhot), i64::from(reply.yhot)),
            &reply.cursor_image,
        )
        .ok()
        .map(|image| HostCursorSample::Image { image, fallback })
    };
    tracing::trace!(
        target: crate::logging::target::SESSION,
        %name,
        serial = reply.cursor_serial,
        width = reply.width,
        height = reply.height,
        all_transparent,
        "cursor watcher: read cursor"
    );
    Ok(XCursorRead {
        serial: reply.cursor_serial,
        sample,
        name,
    })
}

#[cfg(test)]
mod tests {
    use x11rb::protocol::xinput::XIGetPropertyItems;

    use super::{ints, same_device};

    /// xf86-input-wacom stores "Wacom Rotation" as an 8-bit integer and the
    /// tablet area as 32-bit ones; both must be read, or a rotated tablet
    /// would pass as the default mapping.
    #[test]
    fn wacom_integer_properties_are_read_in_every_format() {
        assert_eq!(
            ints(Some(XIGetPropertyItems::Data8(vec![2]))),
            Some(vec![2])
        );
        assert_eq!(
            ints(Some(XIGetPropertyItems::Data8(vec![0xff]))),
            Some(vec![-1])
        );
        assert_eq!(
            ints(Some(XIGetPropertyItems::Data16(vec![0xfffe]))),
            Some(vec![-2])
        );
        assert_eq!(
            ints(Some(XIGetPropertyItems::Data32(vec![0, 65_024, u32::MAX]))),
            Some(vec![0, 65_024, -1])
        );
        assert_eq!(ints(None), None);
        assert_eq!(ints(Some(XIGetPropertyItems::InvalidValue(3))), None);
    }

    #[test]
    fn a_device_node_is_looked_at_only_under_dev_input() {
        // `/dev/null` is a character device (1, 3), but not an input node.
        assert!(!same_device(b"/dev/null", (1, 3)));
        assert!(!same_device(b"/dev/input/../null", (1, 3)));
        assert!(!same_device(b"/dev/input/\0", (1, 3)), "a directory");
        assert!(!same_device(b"/dev/input/arcen-nonexistent", (13, 70)));
    }
}
