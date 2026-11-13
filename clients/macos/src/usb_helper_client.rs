// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Deck-side client for the privileged USB helper.
//!
//! Deck no longer captures the physical device itself. It connects to the
//! root-owned helper socket and forwards the *same* `arcen-protocol` URB frames
//! it already exchanges with the host, so this boundary adds no second wire
//! format. See `docs/adr/0011-macos-privileged-usb-helper.md`.

use arcen_protocol::messages::{UsbHardDeviceMsg, UsbHidReportDescriptorMsg};
use arcen_protocol::{encode_usb_urb_cancel, encode_usb_urb_submit, UsbUrbSubmitHeader};
use arcen_usb_bridge::{AttachmentGeneration, UrbId, UsbSpeed, MAX_TRANSFER_BYTES};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

/// Default rendezvous path, matching `arcen-usb-helper`'s default and the
/// launchd plist. Every directory on it is root's and writable by nobody
/// else (not `/var/run`, which group `daemon` can write on macOS).
pub const DEFAULT_SOCKET: &str = "/private/var/db/arcen/run/usb-helper.sock";

/// The effective uid of the process at the other end of a Unix socket.
///
/// `getpeereid` reads the credentials the kernel recorded at connect time,
/// never a pid that could be reused.
fn peer_uid(fd: std::os::fd::BorrowedFd<'_>) -> std::io::Result<u32> {
    use std::os::fd::AsRawFd;
    let mut uid: libc::uid_t = 0;
    let mut gid: libc::gid_t = 0;
    // SAFETY: both out-params are initialized locals, and the descriptor is
    // borrowed, so it stays open for the duration of the call.
    let result = unsafe { libc::getpeereid(fd.as_raw_fd(), &raw mut uid, &raw mut gid) };
    if result != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(uid)
}

/// Only root may answer as the privileged helper: anything else listening on
/// the helper's path would receive the host's URB traffic and could feed the
/// Deck a forged device.
fn check_helper_peer(uid: std::io::Result<u32>, path: &str) -> Result<(), String> {
    match uid {
        Ok(0) => Ok(()),
        Ok(uid) => Err(format!(
            "refusing the USB helper at {path}: it runs as uid {uid}, not root; \
             another process may be impersonating the helper"
        )),
        Err(error) => Err(format!(
            "refusing the USB helper at {path}: its credentials cannot be read: {error}"
        )),
    }
}

const TAG_HELLO: u8 = 0x01;
const TAG_SUBMIT: u8 = 0x02;
const TAG_CANCEL: u8 = 0x03;
const TAG_COMPLETE: u8 = 0x04;
const TAG_ERROR: u8 = 0x05;
/// Helper facts sent right after the hello (`backend=<name>`); helpers older
/// than 0.19 do not send it.
const TAG_INFO: u8 = 0x06;
/// How long [`HelperClient::connect`] waits for the info frame after the
/// hello. A current helper writes it immediately; an older one never does,
/// and then this is the only cost.
const INFO_WAIT: std::time::Duration = std::time::Duration::from_millis(300);
/// What the log says when the helper did not name its backend.
pub const BACKEND_UNREPORTED: &str = "unreported (older helper: libusb)";

const MAX_FRAME_BYTES: usize = MAX_TRANSFER_BYTES + 1024;

/// One connected privileged-helper session.
pub struct HelperClient {
    stream: UnixStream,
    device: UsbHardDeviceMsg,
    /// Bytes received from the helper that do not yet form a whole frame.
    ///
    /// This lives in the struct, not on the stack of a future, because
    /// [`Self::next_completion`] is polled inside a `tokio::select!`. A future
    /// dropped mid-frame must not take received bytes with it.
    rx: Vec<u8>,
    /// The capture backend the helper named in its info frame.
    backend: Option<String>,
}

impl HelperClient {
    /// Connects to the helper and completes the identity handshake.
    ///
    /// # Errors
    ///
    /// Returns a message when the helper is not running, the socket is not
    /// reachable by this user, or the handshake frame is malformed.
    pub async fn connect(path: &str) -> Result<Self, String> {
        let mut stream = UnixStream::connect(path).await.map_err(|error| {
            format!(
                "connect privileged USB helper at {path}: {error}; \
                 start it with `sudo arcen-usb-helper`"
            )
        })?;
        {
            use std::os::fd::AsFd;
            if let Err(refusal) = check_helper_peer(peer_uid(stream.as_fd()), path) {
                tracing::error!(target: crate::logging::target::USB, "{refusal}");
                return Err(refusal);
            }
        }
        let mut rx = Vec::new();
        let (tag, payload) = read_frame(&mut stream, &mut rx).await?;
        match tag {
            TAG_HELLO => {}
            TAG_ERROR => {
                return Err(format!(
                    "privileged USB helper refused: {}",
                    String::from_utf8_lossy(&payload)
                ))
            }
            other => return Err(format!("unexpected helper handshake tag {other:#04x}")),
        }
        let device = decode_hello(&payload)
            .ok_or_else(|| "privileged USB helper sent a short handshake".to_owned())?;
        let backend = read_info(&mut stream, &mut rx).await;
        Ok(Self {
            stream,
            device,
            rx,
            backend,
        })
    }

    #[must_use]
    pub fn device(&self) -> UsbHardDeviceMsg {
        self.device.clone()
    }

    /// The capture backend the helper reported (`libusb` or `iousbhost`),
    /// or [`BACKEND_UNREPORTED`].
    #[must_use]
    pub fn backend(&self) -> &str {
        self.backend.as_deref().unwrap_or(BACKEND_UNREPORTED)
    }

    /// Forwards one URB to the helper.
    ///
    /// # Errors
    ///
    /// Returns a message when re-encoding fails or the helper has gone away.
    pub async fn submit(
        &mut self,
        header: UsbUrbSubmitHeader,
        payload: &[u8],
    ) -> Result<(), String> {
        let frame = encode_usb_urb_submit(header, payload)
            .map_err(|error| format!("re-encode Hard USB submit for helper: {error:?}"))?;
        write_frame(&mut self.stream, TAG_SUBMIT, &frame).await
    }

    /// Forwards one cancellation to the helper.
    ///
    /// # Errors
    ///
    /// Returns a message when re-encoding fails or the helper has gone away.
    pub async fn cancel(
        &mut self,
        generation: AttachmentGeneration,
        urb_id: UrbId,
    ) -> Result<(), String> {
        let frame = encode_usb_urb_cancel(generation, urb_id);
        write_frame(&mut self.stream, TAG_CANCEL, &frame).await
    }

    /// Waits for the helper's next completion frame, ready to send to the host.
    ///
    /// **Cancellation-safe.** This is polled as a `tokio::select!` branch
    /// alongside the WebSocket and several timers, so it is dropped mid-flight
    /// constantly. Every received byte is therefore accumulated in `self.rx`
    /// before any parsing, and the only await is a plain `read`, which tokio
    /// documents as cancel-safe: either bytes land in `self.rx` or nothing
    /// happened.
    ///
    /// The previous implementation awaited two `read_exact` calls on the
    /// stack. `read_exact` is explicitly *not* cancel-safe, and the helper
    /// writes each frame as three separate writes, so a reader can legitimately
    /// observe the 4-byte length alone. Losing those 4 bytes to a dropped
    /// future made the next call parse a length out of the middle of a frame
    /// and kill the bridge with a bogus oversized-frame error.
    ///
    /// # Errors
    ///
    /// Returns a message when the helper reports an error or disconnects.
    pub async fn next_completion(&mut self) -> Result<Vec<u8>, String> {
        loop {
            // Drain what is already buffered before touching the socket: a
            // single read can deliver several frames.
            if let Some((tag, payload)) = take_frame(&mut self.rx)? {
                match tag {
                    TAG_COMPLETE => return Ok(payload),
                    TAG_ERROR => {
                        return Err(format!(
                            "privileged USB helper error: {}",
                            String::from_utf8_lossy(&payload)
                        ))
                    }
                    // Arrived after the connect wait: note it once.
                    TAG_INFO if self.backend.is_none() => {
                        self.backend = info_backend(&payload);
                        tracing::info!(
                            target: crate::logging::target::USB,
                            backend = self.backend(),
                            "privileged USB helper capture backend"
                        );
                        continue;
                    }
                    _ => continue,
                }
            }
            fill(&mut self.stream, &mut self.rx).await?;
        }
    }

    /// Closes the session, which makes the helper release the device.
    pub async fn shutdown(mut self) {
        let _ = self.stream.shutdown().await;
    }
}

async fn write_frame(stream: &mut UnixStream, tag: u8, payload: &[u8]) -> Result<(), String> {
    let len = payload.len().saturating_add(1);
    if len > MAX_FRAME_BYTES {
        return Err("Hard USB helper frame exceeds bound".to_owned());
    }
    let header = u32::try_from(len).map_err(|_| "helper frame length overflow".to_owned())?;
    stream
        .write_all(&header.to_le_bytes())
        .await
        .map_err(|error| format!("write helper frame: {error}"))?;
    stream
        .write_all(&[tag])
        .await
        .map_err(|error| format!("write helper tag: {error}"))?;
    stream
        .write_all(payload)
        .await
        .map_err(|error| format!("write helper payload: {error}"))?;
    stream
        .flush()
        .await
        .map_err(|error| format!("flush helper frame: {error}"))
}

/// Takes one whole frame out of `buffer`, or reports that more bytes are needed.
///
/// Kept separate from any I/O so it can be tested directly against split and
/// coalesced delivery, which is the failure this framing exists to survive.
fn take_frame(buffer: &mut Vec<u8>) -> Result<Option<(u8, Vec<u8>)>, String> {
    if buffer.len() < 4 {
        return Ok(None);
    }
    let len = u32::from_le_bytes([buffer[0], buffer[1], buffer[2], buffer[3]]) as usize;
    if len == 0 {
        return Err("helper sent an empty frame".to_owned());
    }
    if len > MAX_FRAME_BYTES {
        return Err(format!("helper frame of {len} bytes exceeds bound"));
    }
    // The length covers the tag byte, so a whole frame is 4 + len on the wire.
    if buffer.len() < 4 + len {
        return Ok(None);
    }
    let tag = buffer[4];
    let payload = buffer[5..4 + len].to_vec();
    buffer.drain(..4 + len);
    Ok(Some((tag, payload)))
}

/// Reads once from the helper, appending to `buffer`.
///
/// `read` is cancel-safe and `buffer` is owned by the caller, so a dropped
/// future cannot lose bytes.
async fn fill(stream: &mut UnixStream, buffer: &mut Vec<u8>) -> Result<(), String> {
    let mut chunk = [0_u8; 4096];
    let read = stream
        .read(&mut chunk)
        .await
        .map_err(|error| format!("read helper frame: {error}"))?;
    if read == 0 {
        return Err("privileged USB helper closed the connection".to_owned());
    }
    buffer.extend_from_slice(&chunk[..read]);
    Ok(())
}

/// Blocking read of exactly one frame, for the handshake.
async fn read_frame(
    stream: &mut UnixStream,
    buffer: &mut Vec<u8>,
) -> Result<(u8, Vec<u8>), String> {
    loop {
        if let Some(frame) = take_frame(buffer)? {
            return Ok(frame);
        }
        fill(stream, buffer).await?;
    }
}

/// Whether `buffer` starts with a whole frame, and its tag if so.
fn whole_frame_tag(buffer: &[u8]) -> Option<u8> {
    let header = buffer.get(..4)?;
    let len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
    (len > 0 && buffer.len() >= 4 + len).then(|| buffer[4])
}

/// Takes the info frame that follows the hello, waiting at most
/// [`INFO_WAIT`]. Any other frame stays buffered for
/// [`HelperClient::next_completion`]; a read failure here is left for it to
/// report.
async fn read_info(stream: &mut UnixStream, buffer: &mut Vec<u8>) -> Option<String> {
    let wait = async {
        loop {
            match whole_frame_tag(buffer) {
                Some(TAG_INFO) => {
                    return take_frame(buffer)
                        .ok()
                        .flatten()
                        .and_then(|(_, payload)| info_backend(&payload));
                }
                Some(_) => return None,
                None => {}
            }
            if fill(stream, buffer).await.is_err() {
                return None;
            }
        }
    };
    tokio::time::timeout(INFO_WAIT, wait).await.ok().flatten()
}

/// The `backend=<name>` line of an info payload, if it names one plainly.
fn info_backend(payload: &[u8]) -> Option<String> {
    std::str::from_utf8(payload)
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("backend="))
        .filter(|name| {
            !name.is_empty()
                && name.len() <= 32
                && name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
        .map(str::to_owned)
}

/// Decodes the helper's handshake: an eight-byte identity followed by the HID
/// report descriptors that made the device admissible (count `u8`, then per
/// descriptor interface `u8`, length `u16` LE, bytes). An older helper sends
/// only the identity; the host then refuses Hard USB with a clear reason
/// rather than guessing from the vendor id. A malformed trailer is refused.
fn decode_hello(payload: &[u8]) -> Option<UsbHardDeviceMsg> {
    if payload.len() < 8 {
        return None;
    }
    let speed = match payload[7] {
        0 => UsbSpeed::Low,
        1 => UsbSpeed::Full,
        2 => UsbSpeed::High,
        _ => return None,
    };
    let mut hid_report_descriptors = Vec::new();
    if let Some((&count, mut rest)) = payload[8..].split_first() {
        if usize::from(count) > arcen_usb_bridge::MAX_INTERFACES {
            return None;
        }
        for _ in 0..count {
            let (&interface, after) = rest.split_first()?;
            let length = usize::from(u16::from_le_bytes([*after.first()?, *after.get(1)?]));
            if length > arcen_usb_bridge::MAX_HID_REPORT_DESCRIPTOR_BYTES {
                return None;
            }
            let bytes = after.get(2..2 + length)?;
            hid_report_descriptors.push(UsbHidReportDescriptorMsg::new(interface, bytes));
            rest = &after[2 + length..];
        }
        if !rest.is_empty() {
            return None;
        }
    }
    Some(UsbHardDeviceMsg {
        vendor_id: u16::from_le_bytes([payload[0], payload[1]]),
        product_id: u16::from_le_bytes([payload[2], payload[3]]),
        bcd_device: u16::from_le_bytes([payload[4], payload[5]]),
        device_class: payload[6],
        speed,
        hid_report_descriptors,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_socket_matches_the_helpers_launchd_plist() {
        let plist = include_str!("../../../packaging/macos/tech.arcen.deck.usbhelper.plist");
        assert!(
            plist.contains(&format!("<string>{DEFAULT_SOCKET}</string>")),
            "the plist must start the helper on {DEFAULT_SOCKET}"
        );
        assert!(!DEFAULT_SOCKET.starts_with("/var/"));
        assert!(!DEFAULT_SOCKET.starts_with("/private/var/run"));
    }

    #[test]
    fn only_a_root_peer_is_accepted_as_the_helper() {
        assert_eq!(check_helper_peer(Ok(0), DEFAULT_SOCKET), Ok(()));
        let refusal = check_helper_peer(Ok(501), DEFAULT_SOCKET).unwrap_err();
        assert!(refusal.contains("uid 501, not root"), "{refusal}");
        let unreadable = check_helper_peer(
            Err(std::io::Error::from_raw_os_error(libc::ENOTCONN)),
            DEFAULT_SOCKET,
        )
        .unwrap_err();
        assert!(unreadable.contains("cannot be read"), "{unreadable}");
    }

    /// This test process's own uid, as a socket peer sees it.
    fn own_uid() -> u32 {
        use std::os::fd::AsFd;
        let (deck, _impostor) = std::os::unix::net::UnixStream::pair().expect("socket pair");
        peer_uid(deck.as_fd()).expect("peer credentials")
    }

    #[test]
    fn a_helper_peer_that_is_not_root_is_refused() {
        let uid = own_uid();
        if uid != 0 {
            assert!(check_helper_peer(Ok(uid), DEFAULT_SOCKET).is_err());
        }
    }

    #[tokio::test]
    async fn connect_refuses_a_listener_that_is_not_root_before_reading_anything() {
        // A short path: `sun_path` holds 104 bytes.
        let home = std::fs::canonicalize(std::env::var_os("HOME").expect("HOME")).expect("home");
        let directory = home.join(format!(".arcen-hc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir(&directory).expect("scratch directory");
        let path = directory.join("h.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).expect("bind");
        // An impostor that would answer with a valid hello.
        let impostor = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let _ = std::io::Write::write_all(&mut stream, &[TAG_HELLO, 0, 0, 0, 0]);
        });
        let outcome = HelperClient::connect(path.to_str().expect("utf-8")).await;
        let _ = impostor.join();
        let _ = std::fs::remove_dir_all(&directory);
        if own_uid() != 0 {
            let refusal = outcome.err().expect("a non-root helper is refused");
            assert!(refusal.contains("not root"), "{refusal}");
        }
    }

    #[test]
    fn an_older_helpers_identity_only_hello_still_decodes() {
        let payload = [0x6a, 0x05, 0x17, 0x03, 0x00, 0x01, 0x00, 0x01];
        let device = decode_hello(&payload).expect("decodes");
        assert_eq!(device.vendor_id, 0x056a);
        assert_eq!(device.product_id, 0x0317);
        assert_eq!(device.bcd_device, 0x0100);
        assert_eq!(device.device_class, 0);
        assert_eq!(device.speed, UsbSpeed::Full);
        assert!(device.hid_report_descriptors.is_empty());
    }

    #[test]
    fn an_intuos5_style_hello_is_a_bridgeable_tablet() {
        use arcen_usb_bridge::descriptor_fixtures::{
            INTUOS5_STYLE_MOUSE_INTERFACE, INTUOS5_STYLE_PEN_INTERFACE,
            INTUOS5_STYLE_VENDOR_CONTROL_INTERFACE,
        };
        let mut payload = vec![0x6a, 0x05, 0x17, 0x03, 0x00, 0x01, 0x00, 0x01, 3];
        for (interface, descriptor) in [
            (0_u8, INTUOS5_STYLE_PEN_INTERFACE),
            (1, INTUOS5_STYLE_VENDOR_CONTROL_INTERFACE),
            (2, INTUOS5_STYLE_MOUSE_INTERFACE),
        ] {
            payload.push(interface);
            payload.extend_from_slice(&u16::try_from(descriptor.len()).unwrap().to_le_bytes());
            payload.extend_from_slice(descriptor);
        }
        let device = decode_hello(&payload).expect("decodes");
        let descriptors = device.report_descriptors().expect("valid descriptors");
        assert_eq!(descriptors.len(), 3);
        let profile =
            arcen_usb_bridge::authorize_hard_usb_tablet(device.device_class, &descriptors)
                .expect("an Intuos5-style tablet is bridgeable");
        assert_eq!(profile.capabilities.pressure_levels(), None);
    }

    #[test]
    fn hello_carries_the_report_descriptors_of_any_vendors_tablet() {
        let pen = arcen_usb_bridge::SYNTHETIC_TABLET_REPORT_DESCRIPTOR;
        let mut payload = vec![0x34, 0x12, 0x01, 0x00, 0x00, 0x01, 0x00, 0x02, 1, 0];
        payload.extend_from_slice(&u16::try_from(pen.len()).unwrap().to_le_bytes());
        payload.extend_from_slice(pen);
        let device = decode_hello(&payload).expect("decodes");
        assert_eq!(device.vendor_id, 0x1234);
        let descriptors = device.report_descriptors().expect("valid descriptors");
        assert_eq!(descriptors.len(), 1);
        assert_eq!(descriptors[0].descriptor, pen);
        assert!(arcen_usb_bridge::authorize_hard_usb_tablet(0, &descriptors).is_ok());

        let mut trailing = payload.clone();
        trailing.push(0);
        assert!(decode_hello(&trailing).is_none());
        payload.pop();
        assert!(decode_hello(&payload).is_none());
    }

    #[test]
    fn short_hello_is_refused() {
        assert!(decode_hello(&[1, 2, 3]).is_none());
    }

    #[test]
    fn unknown_speed_code_is_refused_rather_than_guessed() {
        let payload = [0x6a, 0x05, 0x17, 0x03, 0x00, 0x01, 0x00, 0xff];
        assert!(decode_hello(&payload).is_none());
    }

    #[test]
    fn the_info_frame_names_a_plain_backend_only() {
        assert_eq!(
            info_backend(b"backend=iousbhost").as_deref(),
            Some("iousbhost")
        );
        assert_eq!(
            info_backend(b"x=1\nbackend=libusb").as_deref(),
            Some("libusb")
        );
        assert_eq!(info_backend(b"backend="), None);
        assert_eq!(info_backend(b"backend=a b"), None);
        assert_eq!(info_backend(&[0xff, 0xfe]), None);
    }

    #[tokio::test]
    async fn the_backend_is_read_after_the_hello_and_other_frames_stay_queued() {
        let (mut helper, mut deck) = UnixStream::pair().expect("pair");
        let mut sent = wire(TAG_INFO, b"backend=iousbhost");
        sent.extend_from_slice(&wire(TAG_COMPLETE, b"urb"));
        helper.write_all(&sent).await.expect("write");
        let mut buffer = Vec::new();
        assert_eq!(
            read_info(&mut deck, &mut buffer).await.as_deref(),
            Some("iousbhost")
        );
        assert_eq!(
            take_frame(&mut buffer).expect("valid"),
            Some((TAG_COMPLETE, b"urb".to_vec()))
        );
    }

    #[tokio::test]
    async fn an_older_helper_without_info_costs_only_the_wait() {
        let (_helper, mut deck) = UnixStream::pair().expect("pair");
        let mut buffer = Vec::new();
        let started = std::time::Instant::now();
        assert_eq!(read_info(&mut deck, &mut buffer).await, None);
        assert!(started.elapsed() < INFO_WAIT * 3);

        let (mut helper, mut deck) = UnixStream::pair().expect("pair");
        helper
            .write_all(&wire(TAG_COMPLETE, b"urb"))
            .await
            .expect("write");
        let mut buffer = Vec::new();
        assert_eq!(read_info(&mut deck, &mut buffer).await, None);
        assert_eq!(
            take_frame(&mut buffer).expect("valid"),
            Some((TAG_COMPLETE, b"urb".to_vec())),
            "a frame that is not the info frame must stay for next_completion"
        );
    }

    #[tokio::test]
    async fn a_late_info_frame_is_noted_and_skipped_by_the_completion_reader() {
        let (mut helper, deck) = UnixStream::pair().expect("pair");
        let mut client = HelperClient {
            stream: deck,
            device: decode_hello(&[0x6a, 0x05, 0x17, 0x03, 0x00, 0x01, 0x00, 0x01]).expect("hello"),
            rx: Vec::new(),
            backend: None,
        };
        assert_eq!(client.backend(), BACKEND_UNREPORTED);
        let mut sent = wire(TAG_INFO, b"backend=iousbhost");
        sent.extend_from_slice(&wire(TAG_COMPLETE, b"urb"));
        helper.write_all(&sent).await.expect("write");
        assert_eq!(client.next_completion().await.expect("completion"), b"urb");
        assert_eq!(client.backend(), "iousbhost");
    }

    /// Builds one wire frame the way `arcen-usb-helper` writes it.
    fn wire(tag: u8, payload: &[u8]) -> Vec<u8> {
        let mut frame = u32::try_from(payload.len() + 1)
            .expect("fits")
            .to_le_bytes()
            .to_vec();
        frame.push(tag);
        frame.extend_from_slice(payload);
        frame
    }

    #[test]
    fn a_whole_frame_is_taken_and_consumed() {
        let mut buffer = wire(TAG_COMPLETE, b"abc");
        let (tag, payload) = take_frame(&mut buffer).expect("valid").expect("complete");
        assert_eq!(tag, TAG_COMPLETE);
        assert_eq!(payload, b"abc");
        assert!(
            buffer.is_empty(),
            "a consumed frame must leave nothing behind"
        );
    }

    /// The exact shape that used to kill the bridge. The helper writes the
    /// length, the tag and the payload as three separate writes, so a reader
    /// can see the 4-byte length on its own. Partial input must be reported as
    /// "not yet", never consumed.
    #[test]
    fn a_partial_frame_consumes_nothing() {
        let whole = wire(TAG_COMPLETE, b"abcdefgh");
        for split in 0..whole.len() {
            let mut buffer = whole[..split].to_vec();
            let before = buffer.clone();
            assert!(
                take_frame(&mut buffer).expect("valid").is_none(),
                "{split} bytes is not a whole frame"
            );
            assert_eq!(buffer, before, "a partial frame must not be consumed");
        }
    }

    /// Feeding the buffer one byte at a time is the worst case a cancelled
    /// future can produce, and it must still yield the identical frame.
    #[test]
    fn a_frame_split_across_every_byte_still_arrives_intact() {
        let whole = wire(TAG_COMPLETE, b"pen-report");
        let mut buffer = Vec::new();
        let mut got = None;
        for byte in &whole {
            buffer.push(*byte);
            if let Some(frame) = take_frame(&mut buffer).expect("valid") {
                got = Some(frame);
            }
        }
        let (tag, payload) = got.expect("the frame arrives once its last byte does");
        assert_eq!(tag, TAG_COMPLETE);
        assert_eq!(payload, b"pen-report");
        assert!(buffer.is_empty());
    }

    /// One read can deliver several frames; none may be dropped.
    #[test]
    fn coalesced_frames_are_returned_one_at_a_time() {
        let mut buffer = wire(TAG_COMPLETE, b"one");
        buffer.extend(wire(TAG_ERROR, b"two"));
        buffer.extend(wire(TAG_COMPLETE, b"three"));
        let mut seen = Vec::new();
        while let Some((tag, payload)) = take_frame(&mut buffer).expect("valid") {
            seen.push((tag, payload));
        }
        assert_eq!(
            seen,
            vec![
                (TAG_COMPLETE, b"one".to_vec()),
                (TAG_ERROR, b"two".to_vec()),
                (TAG_COMPLETE, b"three".to_vec()),
            ]
        );
        assert!(buffer.is_empty());
    }

    #[test]
    fn an_empty_frame_is_refused() {
        let mut buffer = 0_u32.to_le_bytes().to_vec();
        assert!(take_frame(&mut buffer).is_err());
    }

    /// The bound is checked from the length alone, before any allocation, so a
    /// hostile length cannot make Deck reserve memory for it.
    #[test]
    fn an_oversized_length_is_refused_before_its_body_arrives() {
        let mut buffer = u32::try_from(MAX_FRAME_BYTES + 1)
            .expect("fits")
            .to_le_bytes()
            .to_vec();
        assert!(take_frame(&mut buffer).is_err());
    }
}
