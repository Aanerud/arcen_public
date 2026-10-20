//! `capenc wayland-ramp-client`: the qualification's known picture.
//!
//! A minimal Wayland client that shows one full-screen ten-bit
//! `XRGB2101010` shared-memory buffer, the shared
//! [`qualification_picture_code`]: the top half a horizontal grey ramp, the
//! bottom half a white patch and a patch of three distinct channel codes
//! (which proves channel order, as a grey ramp cannot). With `pq` the surface is tagged PQ / BT.2020 through
//! `wp_color_management_v1`, so the compositor's HDR composition carries the
//! ramp's codes unchanged and a capture can prove it delivers ten bits; with
//! `sdr` the surface is untagged (sRGB), so the white patch shows where the
//! composition puts SDR white. The capture side measures both
//! (`fidelity-probe`).
//!
//! The Wayland wire protocol is spoken directly over the compositor's Unix
//! socket (a handful of requests on core, `xdg-shell` and
//! `color-management-v1` objects), without a client library: the
//! qualification needs only this picture, and the session's D-Bus interfaces
//! cannot draw one.
//!
//! ```text
//! arcen-capenc wayland-ramp-client <pq|sdr> [seconds=N]
//! ```
//!
//! Runs as the session user with the session's `XDG_RUNTIME_DIR`; prints
//! `RAMP-READY width=W height=H` on stderr once the buffer is shown, then
//! keeps the window until `seconds` pass or stdin closes.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use arcen_media::video::compositor_fidelity::qualification_picture_code;

/// `WL_SHM_FORMAT_XRGB2101010` (a DRM fourcc, `XR30`).
const SHM_XRGB2101010: u32 = u32::from_le_bytes(*b"XR30");
/// `wp_color_manager_v1.transfer_function.st2084_pq`.
const TF_ST2084_PQ: u32 = 11;
/// `wp_color_manager_v1.primaries.bt2020`.
const PRIMARIES_BT2020: u32 = 6;
/// `wp_color_manager_v1.render_intent.perceptual`.
const RENDER_INTENT_PERCEPTUAL: u32 = 0;
const DISPLAY_ID: u32 = 1;
const ROUNDTRIP_TIMEOUT: Duration = Duration::from_secs(5);

/// One outgoing request's arguments.
enum Arg<'a> {
    Uint(u32),
    Int(i32),
    Str(&'a str),
}

/// A decoded event.
struct Event {
    object: u32,
    opcode: u16,
    body: Vec<u8>,
}

impl Event {
    fn u32_at(&self, index: usize) -> u32 {
        self.body.get(index * 4..index * 4 + 4).map_or(0, |bytes| {
            u32::from_ne_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
        })
    }

    /// The string at word `index` and the word after it.
    fn string_at(&self, index: usize) -> (String, usize) {
        let length = self.u32_at(index) as usize;
        let start = (index + 1) * 4;
        let text = self
            .body
            .get(start..start + length.saturating_sub(1))
            .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
            .unwrap_or_default();
        (text, index + 1 + length.div_ceil(4))
    }
}

/// The connection and the client's object ids.
struct Client {
    socket: UnixStream,
    next_id: u32,
    pending: Vec<u8>,
}

impl Client {
    fn connect() -> Result<Self, String> {
        let runtime = std::env::var("XDG_RUNTIME_DIR").map_err(|_| "XDG_RUNTIME_DIR is not set")?;
        let display = std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "wayland-0".to_owned());
        let path = if display.starts_with('/') {
            std::path::PathBuf::from(display)
        } else {
            std::path::Path::new(&runtime).join(display)
        };
        let socket = UnixStream::connect(&path)
            .map_err(|error| format!("connect {}: {error}", path.display()))?;
        socket
            .set_read_timeout(Some(Duration::from_millis(200)))
            .map_err(|error| error.to_string())?;
        Ok(Self {
            socket,
            next_id: 2,
            pending: Vec::new(),
        })
    }

    fn new_id(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn encode(object: u32, opcode: u16, args: &[Arg<'_>]) -> Vec<u8> {
        let mut body = Vec::new();
        for arg in args {
            match arg {
                Arg::Uint(value) => body.extend_from_slice(&value.to_ne_bytes()),
                Arg::Int(value) => body.extend_from_slice(&value.to_ne_bytes()),
                Arg::Str(text) => {
                    let length = u32::try_from(text.len() + 1).unwrap_or(u32::MAX);
                    body.extend_from_slice(&length.to_ne_bytes());
                    body.extend_from_slice(text.as_bytes());
                    body.push(0);
                    while body.len() % 4 != 0 {
                        body.push(0);
                    }
                }
            }
        }
        let size = u32::try_from(body.len() + 8).unwrap_or(u32::MAX);
        let mut message = Vec::with_capacity(body.len() + 8);
        message.extend_from_slice(&object.to_ne_bytes());
        message.extend_from_slice(&((size << 16) | u32::from(opcode)).to_ne_bytes());
        message.extend_from_slice(&body);
        message
    }

    fn send(&mut self, object: u32, opcode: u16, args: &[Arg<'_>]) -> Result<(), String> {
        self.socket
            .write_all(&Self::encode(object, opcode, args))
            .map_err(|error| format!("send: {error}"))
    }

    /// Sends a request whose arguments carry one file descriptor.
    fn send_with_fd(
        &mut self,
        object: u32,
        opcode: u16,
        args: &[Arg<'_>],
        fd: RawFd,
    ) -> Result<(), String> {
        let message = Self::encode(object, opcode, args);
        let mut iov = libc::iovec {
            iov_base: message.as_ptr() as *mut libc::c_void,
            iov_len: message.len(),
        };
        // SAFETY: CMSG_SPACE only computes a size.
        let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
        let mut control = vec![0u8; space];
        // SAFETY: an all-zero msghdr is valid; the pointers below stay alive
        // for the call.
        let mut header: libc::msghdr = unsafe { std::mem::zeroed() };
        header.msg_iov = &raw mut iov;
        header.msg_iovlen = 1;
        header.msg_control = control.as_mut_ptr().cast();
        header.msg_controllen = space as _;
        // SAFETY: `control` holds one SCM_RIGHTS message for one descriptor,
        // as CMSG_FIRSTHDR/CMSG_DATA describe; `header` points at it.
        let sent = unsafe {
            let cmsg = libc::CMSG_FIRSTHDR(&raw const header);
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as _;
            std::ptr::copy_nonoverlapping(
                (&raw const fd).cast::<u8>(),
                libc::CMSG_DATA(cmsg),
                std::mem::size_of::<RawFd>(),
            );
            libc::sendmsg(self.socket.as_raw_fd(), &raw const header, 0)
        };
        if sent < 0 || usize::try_from(sent).ok() != Some(message.len()) {
            return Err(format!("sendmsg: {}", std::io::Error::last_os_error()));
        }
        Ok(())
    }

    /// The next complete event, waiting up to the socket's read timeout.
    fn next_event(&mut self) -> Result<Option<Event>, String> {
        loop {
            if self.pending.len() >= 8 {
                let word = |at: usize| {
                    u32::from_ne_bytes([
                        self.pending[at],
                        self.pending[at + 1],
                        self.pending[at + 2],
                        self.pending[at + 3],
                    ])
                };
                let object = word(0);
                let size_opcode = word(4);
                let size = (size_opcode >> 16) as usize;
                if size < 8 {
                    return Err("malformed event".to_owned());
                }
                if self.pending.len() >= size {
                    let body = self.pending[8..size].to_vec();
                    self.pending.drain(..size);
                    return Ok(Some(Event {
                        object,
                        opcode: (size_opcode & 0xffff) as u16,
                        body,
                    }));
                }
            }
            let mut buffer = [0u8; 4096];
            match self.socket.read(&mut buffer) {
                Ok(0) => return Err("the compositor closed the connection".to_owned()),
                Ok(read) => self.pending.extend_from_slice(&buffer[..read]),
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    return Ok(None)
                }
                Err(error) => return Err(format!("read: {error}")),
            }
        }
    }
}

/// What the client has learnt from the compositor.
#[derive(Default)]
struct State {
    globals: HashMap<String, (u32, u32)>,
    done_callbacks: Vec<u32>,
    configure_serial: Option<u32>,
    size: Option<(i32, i32)>,
    description_ready: bool,
    description_failed: Option<String>,
    wm_base: u32,
    xdg_surface: u32,
    toplevel: u32,
    registry: u32,
    description: u32,
}

impl State {
    fn handle(&mut self, client: &mut Client, event: &Event) -> Result<(), String> {
        if event.object == DISPLAY_ID {
            if event.opcode == 0 {
                let (message, _) = event.string_at(2);
                return Err(format!(
                    "protocol error on object {} code {}: {message}",
                    event.u32_at(0),
                    event.u32_at(1)
                ));
            }
            return Ok(());
        }
        if event.object == self.registry && event.opcode == 0 {
            let name = event.u32_at(0);
            let (interface, next) = event.string_at(1);
            let version = event.u32_at(next);
            self.globals.insert(interface, (name, version));
        } else if self.wm_base != 0 && event.object == self.wm_base && event.opcode == 0 {
            client.send(self.wm_base, 3, &[Arg::Uint(event.u32_at(0))])?;
        } else if self.xdg_surface != 0 && event.object == self.xdg_surface && event.opcode == 0 {
            self.configure_serial = Some(event.u32_at(0));
        } else if self.toplevel != 0 && event.object == self.toplevel && event.opcode == 0 {
            #[allow(clippy::cast_possible_wrap)]
            let (width, height) = (event.u32_at(0) as i32, event.u32_at(1) as i32);
            if width > 0 && height > 0 {
                self.size = Some((width, height));
            }
        } else if self.description != 0 && event.object == self.description {
            match event.opcode {
                0 => self.description_failed = Some(event.string_at(1).0),
                _ => self.description_ready = true,
            }
        } else if event.opcode == 0 {
            // A `wl_callback.done` of a sync this client sent.
            self.done_callbacks.push(event.object);
        }
        Ok(())
    }

    fn roundtrip(&mut self, client: &mut Client) -> Result<(), String> {
        let callback = client.new_id();
        client.send(DISPLAY_ID, 0, &[Arg::Uint(callback)])?;
        let deadline = Instant::now() + ROUNDTRIP_TIMEOUT;
        while !self.done_callbacks.contains(&callback) {
            if Instant::now() > deadline {
                return Err("the compositor did not answer a roundtrip".to_owned());
            }
            if let Some(event) = client.next_event()? {
                self.handle(client, &event)?;
            }
        }
        Ok(())
    }

    fn bind(&mut self, client: &mut Client, interface: &str, version: u32) -> Result<u32, String> {
        let (name, offered) = *self
            .globals
            .get(interface)
            .ok_or_else(|| format!("the compositor offers no {interface}"))?;
        let id = client.new_id();
        client.send(
            self.registry,
            0,
            &[
                Arg::Uint(name),
                Arg::Str(interface),
                Arg::Uint(version.min(offered)),
                Arg::Uint(id),
            ],
        )?;
        Ok(id)
    }
}

/// The qualification picture as `XRGB2101010` words (red in bits 20..30).
fn ramp_picture(width: usize, height: usize) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(width * height * 4);
    for row in 0..height {
        for x in 0..width {
            let [red, green, blue] =
                qualification_picture_code(x, row, width, height).map(u32::from);
            let word = (red << 20) | (green << 10) | blue;
            bytes.extend_from_slice(&word.to_le_bytes());
        }
    }
    bytes
}

fn memfd_with(bytes: &[u8]) -> Result<OwnedFd, String> {
    // SAFETY: a NUL-terminated name; the returned descriptor is owned below.
    let fd = unsafe { libc::memfd_create(c"arcen-ramp".as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err(format!("memfd_create: {}", std::io::Error::last_os_error()));
    }
    // SAFETY: `fd` was just created and is owned by nothing else.
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    let mut file = std::fs::File::from(owned);
    file.write_all(bytes)
        .map_err(|error| format!("memfd write: {error}"))?;
    Ok(OwnedFd::from(file))
}

/// Shows the picture and holds it. Returns the process exit code.
pub fn run(args: &[String]) -> i32 {
    let pq = match args
        .iter()
        .skip_while(|argument| *argument != "wayland-ramp-client")
        .nth(1)
        .map(String::as_str)
    {
        Some("pq") => true,
        Some("sdr") => false,
        _ => {
            eprintln!("[capenc] ERROR: wayland-ramp-client needs pq or sdr");
            return 2;
        }
    };
    let seconds = args
        .iter()
        .find_map(|argument| argument.strip_prefix("seconds="))
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(30);
    match show(pq, Duration::from_secs(seconds)) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("[capenc] ERROR: wayland-ramp-client: {error}");
            3
        }
    }
}

#[allow(clippy::too_many_lines)] // One linear client.
fn show(pq: bool, hold: Duration) -> Result<(), String> {
    let mut client = Client::connect()?;
    let mut state = State {
        registry: 2,
        ..State::default()
    };
    client.next_id = 3;
    client.send(DISPLAY_ID, 1, &[Arg::Uint(state.registry)])?;
    state.roundtrip(&mut client)?;
    let compositor = state.bind(&mut client, "wl_compositor", 4)?;
    let shm = state.bind(&mut client, "wl_shm", 1)?;
    state.wm_base = state.bind(&mut client, "xdg_wm_base", 1)?;
    let colour = if pq {
        Some(state.bind(&mut client, "wp_color_manager_v1", 1)?)
    } else {
        None
    };

    let surface = client.new_id();
    client.send(compositor, 0, &[Arg::Uint(surface)])?;
    state.xdg_surface = client.new_id();
    client.send(
        state.wm_base,
        2,
        &[Arg::Uint(state.xdg_surface), Arg::Uint(surface)],
    )?;
    state.toplevel = client.new_id();
    client.send(state.xdg_surface, 1, &[Arg::Uint(state.toplevel)])?;
    client.send(state.toplevel, 2, &[Arg::Str("arcen-ramp")])?;
    // set_fullscreen(output: null).
    client.send(state.toplevel, 11, &[Arg::Uint(0)])?;
    client.send(surface, 6, &[])?;
    let deadline = Instant::now() + ROUNDTRIP_TIMEOUT;
    while state.configure_serial.is_none() {
        if Instant::now() > deadline {
            return Err("the compositor never configured the window".to_owned());
        }
        state.roundtrip(&mut client)?;
    }
    let (width, height) = state
        .size
        .ok_or("the compositor configured the window without a size")?;
    let serial = state.configure_serial.unwrap_or_default();
    client.send(state.xdg_surface, 4, &[Arg::Uint(serial)])?;

    if let Some(manager) = colour {
        let creator = client.new_id();
        client.send(manager, 5, &[Arg::Uint(creator)])?;
        client.send(creator, 1, &[Arg::Uint(TF_ST2084_PQ)])?;
        client.send(creator, 3, &[Arg::Uint(PRIMARIES_BT2020)])?;
        state.description = client.new_id();
        client.send(creator, 0, &[Arg::Uint(state.description)])?;
        while !state.description_ready {
            if let Some(reason) = &state.description_failed {
                return Err(format!("the PQ image description failed: {reason}"));
            }
            state.roundtrip(&mut client)?;
        }
        let managed = client.new_id();
        client.send(manager, 2, &[Arg::Uint(managed), Arg::Uint(surface)])?;
        client.send(
            managed,
            1,
            &[
                Arg::Uint(state.description),
                Arg::Uint(RENDER_INTENT_PERCEPTUAL),
            ],
        )?;
    }

    let (columns, rows) = (
        usize::try_from(width).map_err(|_| "negative width")?,
        usize::try_from(height).map_err(|_| "negative height")?,
    );
    let picture = ramp_picture(columns, rows);
    let size = i32::try_from(picture.len()).map_err(|_| "picture too large")?;
    let memory = memfd_with(&picture)?;
    let pool = client.new_id();
    client.send_with_fd(
        shm,
        0,
        &[Arg::Uint(pool), Arg::Int(size)],
        memory.as_raw_fd(),
    )?;
    let buffer = client.new_id();
    client.send(
        pool,
        0,
        &[
            Arg::Uint(buffer),
            Arg::Int(0),
            Arg::Int(width),
            Arg::Int(height),
            Arg::Int(width * 4),
            Arg::Uint(SHM_XRGB2101010),
        ],
    )?;
    client.send(surface, 1, &[Arg::Uint(buffer), Arg::Int(0), Arg::Int(0)])?;
    client.send(
        surface,
        9,
        &[Arg::Int(0), Arg::Int(0), Arg::Int(width), Arg::Int(height)],
    )?;
    client.send(surface, 6, &[])?;
    state.roundtrip(&mut client)?;
    eprintln!(
        "RAMP-READY width={width} height={height} tagged={}",
        if pq { "pq-bt2020" } else { "untagged-srgb" }
    );

    let started = Instant::now();
    let stdin_closed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let closed = std::sync::Arc::clone(&stdin_closed);
        std::thread::spawn(move || {
            let mut sink = [0u8; 64];
            while matches!(std::io::stdin().read(&mut sink), Ok(read) if read > 0) {}
            closed.store(true, std::sync::atomic::Ordering::Relaxed);
        });
    }
    while started.elapsed() < hold && !stdin_closed.load(std::sync::atomic::Ordering::Relaxed) {
        if let Some(event) = client.next_event()? {
            state.handle(&mut client, &event)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcen_media::video::compositor_fidelity::{expected_ramp_code, CHANNEL_PROBE_CODES};

    #[test]
    fn requests_are_encoded_with_size_opcode_and_padded_strings() {
        let message = Client::encode(7, 2, &[Arg::Str("arcen"), Arg::Uint(3)]);
        // header 8 + string length 4 + "arcen\0" padded to 8 + uint 4.
        assert_eq!(message.len(), 24);
        assert_eq!(u32::from_ne_bytes(message[0..4].try_into().unwrap()), 7);
        assert_eq!(
            u32::from_ne_bytes(message[4..8].try_into().unwrap()),
            (24 << 16) | 2
        );
        assert_eq!(u32::from_ne_bytes(message[8..12].try_into().unwrap()), 6);
        assert_eq!(&message[12..18], b"arcen\0");
        assert_eq!(u32::from_ne_bytes(message[20..24].try_into().unwrap()), 3);
    }

    #[test]
    fn the_picture_is_the_measured_ramp_over_a_white_patch() {
        let picture = ramp_picture(1920, 4);
        let word = |row: usize, x: usize| {
            let at = (row * 1920 + x) * 4;
            u32::from_le_bytes(picture[at..at + 4].try_into().unwrap())
        };
        assert_eq!(word(0, 0) & 0x3ff, 0);
        assert_eq!(word(0, 1919) & 0x3ff, 1023);
        assert_eq!(word(1, 960) >> 20, u32::from(expected_ramp_code(960, 1920)));
        assert_eq!(word(3, 5), (1023 << 20) | (1023 << 10) | 1023);
        let [red, green, blue] = CHANNEL_PROBE_CODES.map(u32::from);
        assert_eq!(word(3, 1900), (red << 20) | (green << 10) | blue);
        assert_eq!(SHM_XRGB2101010, 0x3033_5258);
    }

    #[test]
    fn strings_in_events_are_read_with_their_padding() {
        let mut body = Vec::new();
        body.extend_from_slice(&5u32.to_ne_bytes());
        // The length counts the terminating NUL; the padding does not.
        body.extend_from_slice(&7u32.to_ne_bytes());
        body.extend_from_slice(b"wl_shm\0\0");
        body.extend_from_slice(&1u32.to_ne_bytes());
        let event = Event {
            object: 2,
            opcode: 0,
            body,
        };
        let (text, next) = event.string_at(1);
        assert_eq!(text, "wl_shm");
        assert_eq!(event.u32_at(next), 1);
    }
}
