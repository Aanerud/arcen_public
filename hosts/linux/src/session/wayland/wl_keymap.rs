//! The compositor's keymap, read as an ordinary Wayland client.
//!
//! A `wl_keyboard` receives the compositor's keymap when it is created and
//! again whenever the keymap changes (an input-source switch, a new layout
//! list). The session agent keeps one such client open on its own
//! compositor socket, so it always knows the keymap Mutter types keysyms
//! with. The client creates no surface and never takes focus; it binds
//! `wl_seat`, asks for its keyboard once the seat says it has one, and
//! reads.
//!
//! The order matters: Mutter 49.4 crashes (`seat_get_keyboard` →
//! `send_keymap` → `mtk_anonymous_file_open_fd` on a keymap it has not
//! made yet) when a client asks a headless seat for its keyboard before the
//! seat advertises the keyboard capability. Seen in the lab on a fresh
//! headless session; so the keyboard is requested only after a
//! `capabilities` event names it, and released if the seat loses it.
//!
//! The Wayland wire protocol needed here is five messages, so it is spoken
//! directly: a little-endian header (object id, size and opcode), 32-bit
//! arguments, length-prefixed padded strings, and file descriptors in the
//! socket's ancillary data.

use std::collections::VecDeque;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::FileExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::Arc;

use nix::sys::socket::{recvmsg, ControlMessageOwned, MsgFlags};

use super::text::SessionText;
use crate::logging::target;

const DISPLAY_ID: u32 = 1;
const REGISTRY_ID: u32 = 2;
const SYNC_ID: u32 = 3;
const SEAT_ID: u32 = 4;
const KEYBOARD_ID: u32 = 5;

/// `wl_seat.capability.keyboard`.
const SEAT_CAPABILITY_KEYBOARD: u32 = 2;
/// `wl_keyboard.release` (since version 3).
const KEYBOARD_RELEASE_VERSION: u32 = 3;

/// `wl_keyboard.keymap_format.xkb_v1`.
const KEYMAP_FORMAT_XKB_V1: u32 = 1;
/// The largest keymap accepted (Mutter's are 50–100 KiB).
const MAX_KEYMAP_BYTES: usize = 4 * 1024 * 1024;
/// The highest `wl_seat` version this client speaks (`get_keyboard` exists
/// since 1, `keymap` since 1).
const SEAT_VERSION: u32 = 5;

/// Why the keymap client stopped.
#[derive(Debug, thiserror::Error)]
pub enum KeymapClientError {
    /// The socket failed.
    #[error("Wayland socket: {0}")]
    Io(#[from] std::io::Error),
    /// The compositor sent something unreadable.
    #[error("Wayland protocol: {0}")]
    Protocol(&'static str),
    /// The compositor reported a protocol error.
    #[error("the compositor reported a Wayland error: {0}")]
    Compositor(String),
    /// The compositor has no seat.
    #[error("the compositor offers no wl_seat")]
    NoSeat,
}

/// One message read from the compositor.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Event {
    pub(super) object: u32,
    pub(super) opcode: u16,
    pub(super) args: Vec<u8>,
}

pub(super) fn request(object: u32, opcode: u16, args: &[u8]) -> Vec<u8> {
    let size = u32::try_from(8 + args.len()).unwrap_or(u32::MAX);
    let mut message = Vec::with_capacity(8 + args.len());
    message.extend_from_slice(&object.to_le_bytes());
    message.extend_from_slice(&((size << 16) | u32::from(opcode)).to_le_bytes());
    message.extend_from_slice(args);
    message
}

pub(super) fn put_string(args: &mut Vec<u8>, value: &str) {
    let length = u32::try_from(value.len() + 1).unwrap_or(u32::MAX);
    args.extend_from_slice(&length.to_le_bytes());
    args.extend_from_slice(value.as_bytes());
    args.push(0);
    while args.len() & 3 != 0 {
        args.push(0);
    }
}

/// Reads 32-bit words and strings from an event's arguments.
pub(super) struct Args<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Args<'a> {
    pub(super) const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    pub(super) fn uint(&mut self) -> Result<u32, KeymapClientError> {
        let word = self
            .bytes
            .get(self.at..self.at + 4)
            .ok_or(KeymapClientError::Protocol("short argument"))?;
        self.at += 4;
        Ok(u32::from_le_bytes([word[0], word[1], word[2], word[3]]))
    }

    pub(super) fn string(&mut self) -> Result<String, KeymapClientError> {
        let length = self.uint()? as usize;
        let padded = length.div_ceil(4) * 4;
        let bytes = self
            .bytes
            .get(self.at..self.at + padded)
            .ok_or(KeymapClientError::Protocol("short string"))?;
        self.at += padded;
        let text = bytes.get(..length.saturating_sub(1)).unwrap_or_default();
        Ok(String::from_utf8_lossy(text).into_owned())
    }
}

/// Splits complete messages off the front of `buffer`.
pub(super) fn parse_events(buffer: &mut Vec<u8>) -> Result<Vec<Event>, KeymapClientError> {
    let mut events = Vec::new();
    loop {
        if buffer.len() < 8 {
            break;
        }
        let object = u32::from_le_bytes([buffer[0], buffer[1], buffer[2], buffer[3]]);
        let word = u32::from_le_bytes([buffer[4], buffer[5], buffer[6], buffer[7]]);
        let size = (word >> 16) as usize;
        if size < 8 || size & 3 != 0 {
            return Err(KeymapClientError::Protocol("bad message size"));
        }
        if buffer.len() < size {
            break;
        }
        let opcode = u16::try_from(word & 0xffff).unwrap_or(u16::MAX);
        events.push(Event {
            object,
            opcode,
            args: buffer[8..size].to_vec(),
        });
        buffer.drain(..size);
    }
    Ok(events)
}

pub(super) struct Connection {
    pub(super) stream: UnixStream,
    pub(super) buffer: Vec<u8>,
    pub(super) fds: VecDeque<OwnedFd>,
}

impl Connection {
    pub(super) fn send(&mut self, message: &[u8]) -> Result<(), KeymapClientError> {
        use std::io::Write;
        self.stream.write_all(message)?;
        Ok(())
    }

    /// Reads at least one more chunk, keeping any file descriptors that
    /// came with it.
    fn fill(&mut self) -> Result<(), KeymapClientError> {
        let mut data = [0_u8; 4096];
        let mut control = nix::cmsg_space!([RawFd; 28]);
        let mut iov = [std::io::IoSliceMut::new(&mut data)];
        let message = recvmsg::<()>(
            self.stream.as_raw_fd(),
            &mut iov,
            Some(&mut control),
            MsgFlags::MSG_CMSG_CLOEXEC,
        )
        .map_err(std::io::Error::from)?;
        let read = message.bytes;
        for cmsg in message.cmsgs().map_err(std::io::Error::from)? {
            if let ControlMessageOwned::ScmRights(fds) = cmsg {
                for fd in fds {
                    // SAFETY: the kernel just installed `fd` in this process
                    // for this message; nothing else owns it.
                    self.fds.push_back(unsafe { OwnedFd::from_raw_fd(fd) });
                }
            }
        }
        if read == 0 {
            return Err(KeymapClientError::Io(
                std::io::ErrorKind::UnexpectedEof.into(),
            ));
        }
        self.buffer.extend_from_slice(&data[..read]);
        Ok(())
    }

    pub(super) fn next_events(&mut self) -> Result<Vec<Event>, KeymapClientError> {
        loop {
            let events = parse_events(&mut self.buffer)?;
            if !events.is_empty() {
                return Ok(events);
            }
            self.fill()?;
        }
    }
}

fn read_keymap(fd: OwnedFd, size: u32) -> Result<String, KeymapClientError> {
    let size = size as usize;
    if size == 0 || size > MAX_KEYMAP_BYTES {
        return Err(KeymapClientError::Protocol("keymap size"));
    }
    // Positional reads: a compositor may hand every client the same open
    // file, whose offset must not move under them.
    let file = std::fs::File::from(fd);
    let mut bytes = vec![0_u8; size];
    let mut filled = 0;
    while filled < size {
        let read = file.read_at(&mut bytes[filled..], filled as u64)?;
        if read == 0 {
            break;
        }
        filled += read;
    }
    bytes.truncate(filled);
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    Ok(String::from_utf8_lossy(&bytes[..end]).into_owned())
}

/// Connects to the compositor at `socket`, then records every keymap its
/// keyboard is sent into `text` until the connection ends.
///
/// # Errors
///
/// [`KeymapClientError`] when the connection fails or ends.
pub fn follow_keymap(socket: &Path, text: &SessionText) -> Result<(), KeymapClientError> {
    follow_keymap_on(UnixStream::connect(socket)?, text)
}

fn follow_keymap_on(stream: UnixStream, text: &SessionText) -> Result<(), KeymapClientError> {
    let mut connection = Connection {
        stream,
        buffer: Vec::new(),
        fds: VecDeque::new(),
    };
    connection.send(&request(DISPLAY_ID, 1, &REGISTRY_ID.to_le_bytes()))?;
    connection.send(&request(DISPLAY_ID, 0, &SYNC_ID.to_le_bytes()))?;
    let mut seat = None;
    let mut synced = false;
    loop {
        for event in connection.next_events()? {
            match (event.object, event.opcode) {
                (DISPLAY_ID, 0) => {
                    let mut args = Args::new(&event.args);
                    let _object = args.uint()?;
                    let _code = args.uint()?;
                    return Err(KeymapClientError::Compositor(args.string()?));
                }
                (REGISTRY_ID, 0) => {
                    let mut args = Args::new(&event.args);
                    let name = args.uint()?;
                    let interface = args.string()?;
                    let version = args.uint()?;
                    if interface == "wl_seat" && seat.is_none() {
                        seat = Some((name, version.min(SEAT_VERSION)));
                    }
                }
                (SYNC_ID, 0) => synced = true,
                _ => {}
            }
        }
        if synced {
            break;
        }
    }
    let (name, version) = seat.ok_or(KeymapClientError::NoSeat)?;
    let mut bind = Vec::new();
    bind.extend_from_slice(&name.to_le_bytes());
    put_string(&mut bind, "wl_seat");
    bind.extend_from_slice(&version.to_le_bytes());
    bind.extend_from_slice(&SEAT_ID.to_le_bytes());
    connection.send(&request(REGISTRY_ID, 0, &bind))?;
    // Keyboard object ids: a released keyboard's id is not reused before
    // the compositor confirms it with `delete_id`, so each request takes a
    // fresh one.
    let mut keyboard: Option<u32> = None;
    let mut next_keyboard_id = KEYBOARD_ID;
    loop {
        for event in connection.next_events()? {
            match (event.object, event.opcode) {
                (SEAT_ID, 0) => {
                    let capabilities = Args::new(&event.args).uint()?;
                    let has_keyboard = capabilities & SEAT_CAPABILITY_KEYBOARD != 0;
                    match (has_keyboard, keyboard) {
                        (true, None) => {
                            connection.send(&request(
                                SEAT_ID,
                                1,
                                &next_keyboard_id.to_le_bytes(),
                            ))?;
                            keyboard = Some(next_keyboard_id);
                            next_keyboard_id += 1;
                            tracing::info!(
                                target: target::INPUT,
                                seat_version = version,
                                "reading the compositor's keymap as a Wayland client"
                            );
                        }
                        (false, Some(id)) => {
                            if version >= KEYBOARD_RELEASE_VERSION {
                                connection.send(&request(id, 0, &[]))?;
                            }
                            keyboard = None;
                        }
                        _ => {}
                    }
                }
                (DISPLAY_ID, 0) => {
                    let mut args = Args::new(&event.args);
                    let _object = args.uint()?;
                    let _code = args.uint()?;
                    return Err(KeymapClientError::Compositor(args.string()?));
                }
                (object, 0) if Some(object) == keyboard => {
                    let mut args = Args::new(&event.args);
                    let format = args.uint()?;
                    let size = args.uint()?;
                    let fd = connection
                        .fds
                        .pop_front()
                        .ok_or(KeymapClientError::Protocol("keymap without a file"))?;
                    if format != KEYMAP_FORMAT_XKB_V1 {
                        tracing::warn!(target: target::INPUT, format, "the compositor's keymap is not XKB; committed text stays unverified");
                        continue;
                    }
                    let keymap = read_keymap(fd, size)?;
                    record_keymap(text, &keymap);
                }
                _ => {}
            }
        }
    }
}

fn record_keymap(text: &SessionText, keymap: &str) {
    match super::xkb::index_keymap(keymap) {
        Ok(index) => {
            let groups = index.group_count();
            let generation = text.set_keymap(keymap, index);
            let facts = text.facts();
            tracing::info!(
                target: target::INPUT,
                generation,
                groups,
                layouts = ?facts.layouts,
                "compositor keymap read; committed text is checked against it"
            );
        }
        Err(error) => {
            tracing::warn!(target: target::INPUT, %error, "compositor keymap could not be indexed; committed text stays unverified");
        }
    }
}

/// Follows the compositor's keymap on a thread of its own for the life of
/// the session (a blocking reader of one Unix socket).
pub fn spawn(socket: std::path::PathBuf, text: Arc<SessionText>) {
    let spawned = std::thread::Builder::new()
        .name("arcen-keymap".to_owned())
        .spawn(move || {
            if !super::xkb::available() {
                tracing::warn!(target: target::INPUT, "libxkbcommon is not installed; committed text cannot be checked against the session's layout");
                return;
            }
            match follow_keymap(&socket, &text) {
                Ok(()) => {}
                Err(error) => tracing::info!(
                    target: target::INPUT,
                    %error,
                    "stopped following the compositor's keymap"
                ),
            }
        });
    if let Err(error) = spawned {
        tracing::warn!(target: target::INPUT, %error, "keymap reader thread could not start");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_encode_header_and_padded_strings() {
        let mut args = Vec::new();
        args.extend_from_slice(&7_u32.to_le_bytes());
        put_string(&mut args, "wl_seat");
        assert_eq!(args.len(), 4 + 4 + 8);
        let message = request(2, 0, &args);
        assert_eq!(&message[..4], &2_u32.to_le_bytes());
        assert_eq!(
            u32::from_le_bytes([message[4], message[5], message[6], message[7]]),
            (24 << 16)
        );
        let mut reader = Args::new(&args);
        assert_eq!(reader.uint().unwrap(), 7);
        assert_eq!(reader.string().unwrap(), "wl_seat");
    }

    #[test]
    fn events_are_split_only_when_complete() {
        let mut args = Vec::new();
        args.extend_from_slice(&3_u32.to_le_bytes());
        put_string(&mut args, "wl_seat");
        args.extend_from_slice(&8_u32.to_le_bytes());
        let mut buffer = request(REGISTRY_ID, 0, &args);
        buffer.extend_from_slice(&request(SYNC_ID, 0, &1_u32.to_le_bytes())[..6]);
        let events = parse_events(&mut buffer).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(buffer.len(), 6);
        let mut global = Args::new(&events[0].args);
        assert_eq!(global.uint().unwrap(), 3);
        assert_eq!(global.string().unwrap(), "wl_seat");
        assert_eq!(global.uint().unwrap(), 8);
        let mut bad = vec![1, 0, 0, 0, 0, 0, 5, 0];
        assert!(parse_events(&mut bad).is_err());
    }

    /// A fake compositor: one `wl_seat` global, a sync, then the seat's
    /// capabilities as given, then a keymap for whatever keyboard the
    /// client asked for. Returns the requests the client sent.
    fn fake_compositor(
        server: UnixStream,
        capabilities: &'static [u32],
        keymap: &'static str,
    ) -> std::thread::JoinHandle<Vec<(u32, u16)>> {
        use nix::sys::socket::{sendmsg, ControlMessage};
        use std::io::Write;
        std::thread::spawn(move || {
            let mut server = server;
            let mut received = Vec::new();
            let mut buffer = Vec::new();
            let read_requests = |server: &mut UnixStream, buffer: &mut Vec<u8>| {
                use std::io::Read;
                let mut chunk = [0_u8; 1024];
                server
                    .set_read_timeout(Some(std::time::Duration::from_millis(300)))
                    .unwrap();
                if let Ok(read) = server.read(&mut chunk) {
                    buffer.extend_from_slice(&chunk[..read]);
                }
                parse_events(buffer).unwrap()
            };
            // get_registry + sync
            let mut requests = Vec::new();
            while requests.len() < 2 {
                requests.extend(read_requests(&mut server, &mut buffer));
            }
            received.extend(requests.iter().map(|event| (event.object, event.opcode)));
            let mut global = Vec::new();
            global.extend_from_slice(&9_u32.to_le_bytes());
            put_string(&mut global, "wl_seat");
            global.extend_from_slice(&8_u32.to_le_bytes());
            server.write_all(&request(REGISTRY_ID, 0, &global)).unwrap();
            server
                .write_all(&request(SYNC_ID, 0, &1_u32.to_le_bytes()))
                .unwrap();
            for &caps in capabilities {
                server
                    .write_all(&request(SEAT_ID, 0, &caps.to_le_bytes()))
                    .unwrap();
                for event in read_requests(&mut server, &mut buffer) {
                    received.push((event.object, event.opcode));
                    if event.object == SEAT_ID && event.opcode == 1 {
                        let keyboard = Args::new(&event.args).uint().unwrap();
                        let memfd = nix::sys::memfd::memfd_create(
                            c"keymap",
                            nix::sys::memfd::MemFdCreateFlag::empty(),
                        )
                        .unwrap();
                        let mut file = std::fs::File::from(memfd);
                        file.write_all(keymap.as_bytes()).unwrap();
                        file.write_all(&[0]).unwrap();
                        let mut args = Vec::new();
                        args.extend_from_slice(&KEYMAP_FORMAT_XKB_V1.to_le_bytes());
                        args.extend_from_slice(
                            &u32::try_from(keymap.len() + 1).unwrap().to_le_bytes(),
                        );
                        let message = request(keyboard, 0, &args);
                        let fds = [file.as_raw_fd()];
                        sendmsg::<()>(
                            server.as_raw_fd(),
                            &[std::io::IoSlice::new(&message)],
                            &[ControlMessage::ScmRights(&fds)],
                            MsgFlags::empty(),
                            None,
                        )
                        .unwrap();
                    }
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(300));
            received
        })
    }

    #[test]
    fn the_keyboard_is_asked_for_only_once_the_seat_has_one() {
        let (client, server) = UnixStream::pair().unwrap();
        let compositor = fake_compositor(
            server,
            &[0, SEAT_CAPABILITY_KEYBOARD],
            super::super::xkb::tests::KEYMAP,
        );
        let text = Arc::new(SessionText::default());
        let following = std::thread::spawn({
            let text = Arc::clone(&text);
            move || follow_keymap_on(client, &text)
        });
        let received = compositor.join().unwrap();
        // get_registry, sync, bind, and get_keyboard only after the seat
        // named its keyboard.
        assert_eq!(
            received,
            [
                (DISPLAY_ID, 1),
                (DISPLAY_ID, 0),
                (REGISTRY_ID, 0),
                (SEAT_ID, 1)
            ]
        );
        assert!(
            following.join().unwrap().is_err(),
            "the compositor went away"
        );
        if super::super::xkb::available() {
            let facts = text.facts();
            assert!(facts.keymap_known);
            assert_eq!(facts.layouts, ["no", "us"]);
        }
    }
}
