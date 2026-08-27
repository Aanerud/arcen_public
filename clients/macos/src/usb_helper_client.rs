//! Deck-side client for the privileged USB helper.
//!
//! Deck no longer captures the physical device itself. It connects to the
//! root-owned helper socket and forwards the *same* `arcen-protocol` URB frames
//! it already exchanges with the host, so this boundary adds no second wire
//! format. See `docs/adr/0011-macos-privileged-usb-helper.md`.

use arcen_protocol::messages::UsbHardDeviceMsg;
use arcen_protocol::{encode_usb_urb_cancel, encode_usb_urb_submit, UsbUrbSubmitHeader};
use arcen_usb_bridge::{AttachmentGeneration, UrbId, UsbSpeed, MAX_TRANSFER_BYTES};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

/// Default rendezvous path, matching `arcen-usb-helper`'s default.
pub const DEFAULT_SOCKET: &str = "/var/run/arcen-usb-helper.sock";

const TAG_HELLO: u8 = 0x01;
const TAG_SUBMIT: u8 = 0x02;
const TAG_CANCEL: u8 = 0x03;
const TAG_COMPLETE: u8 = 0x04;
const TAG_ERROR: u8 = 0x05;

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
        Ok(Self { stream, device, rx })
    }

    #[must_use]
    pub const fn device(&self) -> UsbHardDeviceMsg {
        self.device
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
    Some(UsbHardDeviceMsg {
        vendor_id: u16::from_le_bytes([payload[0], payload[1]]),
        product_id: u16::from_le_bytes([payload[2], payload[3]]),
        bcd_device: u16::from_le_bytes([payload[4], payload[5]]),
        device_class: payload[6],
        speed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_decodes_the_measured_wacom_identity() {
        let payload = [0x6a, 0x05, 0x17, 0x03, 0x00, 0x01, 0x00, 0x01];
        let device = decode_hello(&payload).expect("decodes");
        assert_eq!(device.vendor_id, 0x056a);
        assert_eq!(device.product_id, 0x0317);
        assert_eq!(device.bcd_device, 0x0100);
        assert_eq!(device.device_class, 0);
        assert_eq!(device.speed, UsbSpeed::Full);
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
