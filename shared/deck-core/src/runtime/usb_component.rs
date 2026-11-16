// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The Deck's half of the Arcen USB component link, over any local byte
//! stream (macOS: the privileged helper's Unix socket, ADR 0011; Windows:
//! the `ArcenUsbComponent` service's named pipe, ADR 0024).
//!
//! The component captures the tablet; the Deck forwards the *same*
//! `arcen-protocol` URB frames it exchanges with the Pier, wrapped in the
//! shared [`arcen_usb_bridge::component_ipc`] frames, so this link adds no
//! second wire format. A platform adapter only opens the stream and proves
//! who is at the other end (macOS: a root peer; Windows: the service
//! process running as `SYSTEM`); everything after that is here.

use std::time::Duration;

use arcen_input::PenEvent;
use arcen_protocol::messages::{UsbHardDeviceMsg, UsbHidReportDescriptorMsg};
use arcen_protocol::{UsbUrbSubmitHeader, encode_usb_urb_cancel, encode_usb_urb_submit};
use arcen_usb_bridge::component_ipc::{
    self, TAG_CANCEL, TAG_COMPLETE, TAG_ERROR, TAG_HELLO, TAG_INFO, TAG_SUBMIT,
};
use arcen_usb_bridge::{AttachmentGeneration, UrbId};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::runtime::usb::{BoxFuture, UsbSubmitResult, UsbTabletResponder};

/// How long [`ComponentClient::handshake`] waits for the info frame after
/// the hello. A current component writes it immediately; an older one never
/// does, and then this is the only cost.
pub const INFO_WAIT: Duration = Duration::from_millis(300);

/// What the log says when the component did not name its backend.
pub const BACKEND_UNREPORTED: &str = "unreported (older helper: libusb)";

/// One connected component session: the tablet stays captured until it is
/// dropped or [`ComponentClient::shutdown`].
pub struct ComponentClient<S> {
    stream: S,
    /// How messages name the other end ("privileged USB helper", "Arcen USB
    /// component").
    peer: &'static str,
    device: UsbHardDeviceMsg,
    /// Bytes received that do not yet form a whole frame.
    ///
    /// This lives in the struct, not on the stack of a future, because
    /// [`Self::next_completion`] is polled inside a `tokio::select!`. A future
    /// dropped mid-frame must not take received bytes with it.
    rx: Vec<u8>,
    /// The capture backend the component named in its info frame.
    backend: Option<String>,
}

impl<S> std::fmt::Debug for ComponentClient<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ComponentClient")
            .field("peer", &self.peer)
            .field("device", &(self.device.vendor_id, self.device.product_id))
            .field("buffered", &self.rx.len())
            .field("backend", &self.backend)
            .finish_non_exhaustive()
    }
}

impl<S> ComponentClient<S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    /// Reads the component's hello (or its refusal) and its info frame from
    /// a stream whose peer the caller has already verified.
    ///
    /// # Errors
    ///
    /// The component refused (its own reason), went away, or sent a
    /// malformed hello.
    pub async fn handshake(mut stream: S, peer: &'static str) -> Result<Self, String> {
        let mut rx = Vec::new();
        let (tag, payload) = read_frame(&mut stream, &mut rx, peer).await?;
        match tag {
            TAG_HELLO => {}
            TAG_ERROR => {
                return Err(format!(
                    "{peer} refused: {}",
                    String::from_utf8_lossy(&payload)
                ));
            }
            other => return Err(format!("unexpected {peer} handshake tag {other:#04x}")),
        }
        let device =
            hello_device(&payload).ok_or_else(|| format!("{peer} sent a short handshake"))?;
        let backend = read_info(&mut stream, &mut rx, peer).await;
        Ok(Self {
            stream,
            peer,
            device,
            rx,
            backend,
        })
    }

    /// The device the component captured.
    #[must_use]
    pub fn device(&self) -> UsbHardDeviceMsg {
        self.device.clone()
    }

    /// The capture backend the component reported (`libusb`, `iousbhost`,
    /// `winusb`, `emulated`), or [`BACKEND_UNREPORTED`].
    #[must_use]
    pub fn backend(&self) -> &str {
        self.backend.as_deref().unwrap_or(BACKEND_UNREPORTED)
    }

    /// Forwards one URB to the component.
    ///
    /// # Errors
    ///
    /// Re-encoding failed or the component has gone away.
    pub async fn submit(
        &mut self,
        header: UsbUrbSubmitHeader,
        payload: &[u8],
    ) -> Result<(), String> {
        let frame = encode_usb_urb_submit(header, payload)
            .map_err(|error| format!("re-encode Hard USB submit for {}: {error:?}", self.peer))?;
        write_frame(&mut self.stream, TAG_SUBMIT, &frame, self.peer).await
    }

    /// Forwards one cancellation to the component; its terminal completion
    /// arrives through [`Self::next_completion`].
    ///
    /// # Errors
    ///
    /// The component has gone away.
    pub async fn cancel(
        &mut self,
        generation: AttachmentGeneration,
        urb_id: UrbId,
    ) -> Result<(), String> {
        let frame = encode_usb_urb_cancel(generation, urb_id);
        write_frame(&mut self.stream, TAG_CANCEL, &frame, self.peer).await
    }

    /// Waits for the component's next completion frame, ready to send to
    /// the Pier.
    ///
    /// **Cancellation-safe.** It is polled as a `tokio::select!` branch and
    /// dropped mid-flight constantly, so every received byte is accumulated
    /// in `self.rx` before any parsing, and the only await is a plain `read`
    /// (cancel-safe: either bytes land in `self.rx` or nothing happened). A
    /// component may write a frame's length, tag and payload separately; a
    /// `read_exact` on the stack would lose a lone length to a dropped
    /// future and then parse a length out of the middle of a frame.
    ///
    /// # Errors
    ///
    /// The component reported an error or disconnected.
    pub async fn next_completion(&mut self) -> Result<Vec<u8>, String> {
        loop {
            // Drain what is already buffered before touching the stream: a
            // single read can deliver several frames.
            if let Some((tag, payload)) = take_frame(&mut self.rx, self.peer)? {
                match tag {
                    TAG_COMPLETE => return Ok(payload),
                    TAG_ERROR => {
                        return Err(format!(
                            "{} error: {}",
                            self.peer,
                            String::from_utf8_lossy(&payload)
                        ));
                    }
                    // Arrived after the handshake wait: note it once.
                    TAG_INFO if self.backend.is_none() => {
                        self.backend = component_ipc::info_backend(&payload);
                        tracing::info!(
                            target: crate::log_target::USB,
                            backend = self.backend(),
                            "{} capture backend",
                            self.peer
                        );
                    }
                    _ => {}
                }
                continue;
            }
            fill(&mut self.stream, &mut self.rx, self.peer).await?;
        }
    }

    /// Closes the session, which makes the component release the tablet.
    pub async fn shutdown(mut self) {
        let _ = self.stream.shutdown().await;
    }
}

impl<S> UsbTabletResponder for ComponentClient<S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    fn device(&self) -> UsbHardDeviceMsg {
        Self::device(self)
    }

    fn update_pen(&mut self, _event: PenEvent) {}

    fn submit<'a>(
        &'a mut self,
        header: UsbUrbSubmitHeader,
        payload: &'a [u8],
    ) -> BoxFuture<'a, Result<UsbSubmitResult, String>> {
        Box::pin(async move {
            Self::submit(self, header, payload).await?;
            Ok(UsbSubmitResult::Pending)
        })
    }

    fn cancel(
        &mut self,
        generation: AttachmentGeneration,
        urb_id: UrbId,
    ) -> BoxFuture<'_, Result<Option<Vec<u8>>, String>> {
        Box::pin(async move {
            Self::cancel(self, generation, urb_id).await?;
            Ok(None)
        })
    }

    fn has_async_completions(&self) -> bool {
        true
    }

    fn next_completion(&mut self) -> BoxFuture<'_, Result<Vec<u8>, String>> {
        Box::pin(Self::next_completion(self))
    }

    fn shutdown(self: Box<Self>) -> BoxFuture<'static, ()> {
        Box::pin(Self::shutdown(*self))
    }
}

async fn write_frame<S: AsyncWrite + Unpin>(
    stream: &mut S,
    tag: u8,
    payload: &[u8],
    peer: &str,
) -> Result<(), String> {
    let frame = component_ipc::encode_frame(tag, payload)
        .map_err(|_| format!("Hard USB frame for the {peer} exceeds the bound"))?;
    stream
        .write_all(&frame)
        .await
        .map_err(|error| format!("write {peer} frame: {error}"))?;
    stream
        .flush()
        .await
        .map_err(|error| format!("flush {peer} frame: {error}"))
}

/// Takes one whole frame out of `buffer`, or reports that more bytes are
/// needed. Kept apart from any I/O so split and coalesced delivery can be
/// tested directly.
fn take_frame(buffer: &mut Vec<u8>, peer: &str) -> Result<Option<(u8, Vec<u8>)>, String> {
    component_ipc::take_frame(buffer).map_err(|error| match error {
        component_ipc::FrameError::Empty => format!("{peer} sent an empty frame"),
        other => other.to_string(),
    })
}

/// Reads once, appending to `buffer`. `read` is cancel-safe and `buffer` is
/// owned by the caller, so a dropped future cannot lose bytes.
async fn fill<S: AsyncRead + Unpin>(
    stream: &mut S,
    buffer: &mut Vec<u8>,
    peer: &str,
) -> Result<(), String> {
    let mut chunk = [0_u8; 4096];
    let read = stream
        .read(&mut chunk)
        .await
        .map_err(|error| format!("read {peer} frame: {error}"))?;
    if read == 0 {
        return Err(format!("{peer} closed the connection"));
    }
    buffer.extend_from_slice(&chunk[..read]);
    Ok(())
}

/// Reads exactly one frame, for the handshake.
async fn read_frame<S: AsyncRead + Unpin>(
    stream: &mut S,
    buffer: &mut Vec<u8>,
    peer: &str,
) -> Result<(u8, Vec<u8>), String> {
    loop {
        if let Some(frame) = take_frame(buffer, peer)? {
            return Ok(frame);
        }
        fill(stream, buffer, peer).await?;
    }
}

/// Takes the info frame that follows the hello, waiting at most
/// [`INFO_WAIT`]. Any other frame stays buffered for
/// [`ComponentClient::next_completion`]; a read failure here is left for it
/// to report.
async fn read_info<S: AsyncRead + Unpin>(
    stream: &mut S,
    buffer: &mut Vec<u8>,
    peer: &str,
) -> Option<String> {
    let wait = async {
        loop {
            match component_ipc::whole_frame_tag(buffer) {
                Some(TAG_INFO) => {
                    return take_frame(buffer, peer)
                        .ok()
                        .flatten()
                        .and_then(|(_, payload)| component_ipc::info_backend(&payload));
                }
                Some(_) => return None,
                // A header that can never complete (empty or oversized) is
                // left for `next_completion` to refuse; stop reading here.
                None if buffer.len() >= 4
                    && component_ipc::take_frame(&mut buffer[..4].to_vec()).is_err() =>
                {
                    return None;
                }
                None => {}
            }
            if fill(stream, buffer, peer).await.is_err() {
                return None;
            }
        }
    };
    tokio::time::timeout(INFO_WAIT, wait).await.ok().flatten()
}

/// Decodes the component's hello: an eight-byte identity followed by the HID
/// report descriptors that made the device admissible. An older helper sends
/// only the identity; the Pier then refuses Hard USB with a clear reason
/// rather than guessing from the vendor id. A malformed trailer is refused.
#[must_use]
pub fn hello_device(payload: &[u8]) -> Option<UsbHardDeviceMsg> {
    let hello = component_ipc::decode_hello(payload)?;
    Some(UsbHardDeviceMsg {
        vendor_id: hello.vendor_id,
        product_id: hello.product_id,
        bcd_device: hello.bcd_device,
        device_class: hello.device_class,
        speed: hello.speed,
        hid_report_descriptors: hello
            .report_descriptors
            .iter()
            .map(|(interface, bytes)| UsbHidReportDescriptorMsg::new(*interface, bytes))
            .collect(),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use arcen_usb_bridge::UsbSpeed;
    use arcen_usb_bridge::component_ipc::MAX_FRAME_BYTES;
    use tokio::io::{DuplexStream, duplex};

    const PEER: &str = "test component";
    const IDENTITY: [u8; 8] = [0x6a, 0x05, 0x17, 0x03, 0x00, 0x01, 0x00, 0x01];

    /// One wire frame, the way the components write it.
    fn wire(tag: u8, payload: &[u8]) -> Vec<u8> {
        component_ipc::encode_frame(tag, payload).expect("fits")
    }

    #[test]
    fn an_older_helpers_identity_only_hello_still_decodes() {
        let device = hello_device(&IDENTITY).expect("decodes");
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
        let mut payload = IDENTITY.to_vec();
        payload.push(3);
        for (interface, descriptor) in [
            (0_u8, INTUOS5_STYLE_PEN_INTERFACE),
            (1, INTUOS5_STYLE_VENDOR_CONTROL_INTERFACE),
            (2, INTUOS5_STYLE_MOUSE_INTERFACE),
        ] {
            payload.push(interface);
            payload.extend_from_slice(&u16::try_from(descriptor.len()).unwrap().to_le_bytes());
            payload.extend_from_slice(descriptor);
        }
        let device = hello_device(&payload).expect("decodes");
        let descriptors = device.report_descriptors().expect("valid descriptors");
        assert_eq!(descriptors.len(), 3);
        assert!(
            arcen_usb_bridge::authorize_hard_usb_tablet(device.device_class, &descriptors).is_ok()
        );
    }

    #[test]
    fn a_short_or_unknown_speed_hello_is_refused() {
        assert!(hello_device(&[1, 2, 3]).is_none());
        let mut unknown = IDENTITY;
        unknown[7] = 0xff;
        assert!(hello_device(&unknown).is_none());
    }

    async fn connected(
        sent: &[u8],
    ) -> (Result<ComponentClient<DuplexStream>, String>, DuplexStream) {
        let (mut component, deck) = duplex(64 * 1024);
        component.write_all(sent).await.expect("write");
        (ComponentClient::handshake(deck, PEER).await, component)
    }

    #[tokio::test]
    async fn the_handshake_reads_the_hello_then_the_info_and_keeps_later_frames() {
        let mut sent = wire(TAG_HELLO, &IDENTITY);
        sent.extend(wire(TAG_INFO, b"backend=winusb"));
        sent.extend(wire(TAG_COMPLETE, b"urb"));
        let (client, _component) = connected(&sent).await;
        let mut client = client.expect("handshake");
        assert_eq!(client.device().product_id, 0x0317);
        assert_eq!(client.backend(), "winusb");
        assert_eq!(client.next_completion().await.expect("completion"), b"urb");
    }

    #[tokio::test]
    async fn a_refusal_carries_the_components_own_reason() {
        let (client, _component) =
            connected(&wire(TAG_ERROR, b"no prepared tablet is attached")).await;
        let refusal = client.expect_err("refused");
        assert_eq!(
            refusal,
            "test component refused: no prepared tablet is attached"
        );
    }

    #[tokio::test]
    async fn a_hostile_header_after_the_hello_stops_the_info_wait_at_once() {
        let mut sent = wire(TAG_HELLO, &IDENTITY);
        sent.extend(u32::try_from(MAX_FRAME_BYTES + 1).unwrap().to_le_bytes());
        let started = std::time::Instant::now();
        let (client, _component) = connected(&sent).await;
        let mut client = client.expect("handshake");
        assert!(started.elapsed() < INFO_WAIT);
        assert_eq!(client.backend(), BACKEND_UNREPORTED);
        assert!(
            client.next_completion().await.is_err(),
            "left for next_completion"
        );
    }

    #[tokio::test]
    async fn an_older_helper_without_info_costs_only_the_wait() {
        let started = std::time::Instant::now();
        let (client, _component) = connected(&wire(TAG_HELLO, &IDENTITY)).await;
        assert_eq!(client.expect("handshake").backend(), BACKEND_UNREPORTED);
        assert!(started.elapsed() < INFO_WAIT * 3);
    }

    #[tokio::test]
    async fn a_late_info_frame_is_noted_and_skipped_by_the_completion_reader() {
        let (client, mut component) = connected(&wire(TAG_HELLO, &IDENTITY)).await;
        let mut client = client.expect("handshake");
        let mut sent = wire(TAG_INFO, b"backend=iousbhost");
        sent.extend(wire(TAG_COMPLETE, b"urb"));
        component.write_all(&sent).await.expect("write");
        assert_eq!(client.next_completion().await.expect("completion"), b"urb");
        assert_eq!(client.backend(), "iousbhost");
    }

    #[tokio::test]
    async fn a_completion_split_across_dropped_reads_arrives_intact() {
        let (client, mut component) = connected(&wire(TAG_HELLO, &IDENTITY)).await;
        let mut client = client.expect("handshake");
        let whole = wire(TAG_COMPLETE, b"pen-report");
        for byte in &whole[..whole.len() - 1] {
            component.write_all(&[*byte]).await.expect("write");
            // A select! branch that loses the race drops the future.
            let pending =
                tokio::time::timeout(Duration::from_millis(5), client.next_completion()).await;
            assert!(pending.is_err(), "not a whole frame yet");
        }
        component
            .write_all(&whole[whole.len() - 1..])
            .await
            .expect("write");
        assert_eq!(
            client.next_completion().await.expect("completion"),
            b"pen-report"
        );
    }

    #[tokio::test]
    async fn submits_and_cancels_reach_the_component_as_tagged_frames() {
        use arcen_usb_bridge::{EndpointAddress, TransferKind};
        use std::num::{NonZeroU32, NonZeroU64};
        let (client, mut component) = connected(&wire(TAG_HELLO, &IDENTITY)).await;
        let mut client: Box<dyn UsbTabletResponder> = Box::new(client.expect("handshake"));
        let generation = AttachmentGeneration::new(NonZeroU64::MIN);
        let urb_id = UrbId::new(NonZeroU32::MIN);
        let header = UsbUrbSubmitHeader {
            generation,
            urb_id,
            endpoint: EndpointAddress(0x81),
            transfer_kind: TransferKind::Interrupt,
            timeout_ms: 1_000,
            declared_length: 10,
            setup: None,
        };
        assert!(matches!(
            client.submit(header, &[]).await.expect("submit"),
            UsbSubmitResult::Pending
        ));
        assert!(
            client
                .cancel(generation, urb_id)
                .await
                .expect("cancel")
                .is_none()
        );
        assert!(client.has_async_completions());
        client.shutdown().await;
        let mut received = Vec::new();
        component.read_to_end(&mut received).await.expect("read");
        let (tag, submit) = component_ipc::take_frame(&mut received).unwrap().unwrap();
        assert_eq!(tag, TAG_SUBMIT);
        assert_eq!(submit, encode_usb_urb_submit(header, &[]).unwrap());
        let (tag, cancel) = component_ipc::take_frame(&mut received).unwrap().unwrap();
        assert_eq!(tag, TAG_CANCEL);
        assert_eq!(cancel, encode_usb_urb_cancel(generation, urb_id));
        assert!(received.is_empty(), "shutdown closes the stream");
    }

    #[tokio::test]
    async fn an_error_frame_or_a_closed_stream_ends_the_bridge_with_a_reason() {
        let (client, mut component) = connected(&wire(TAG_HELLO, &IDENTITY)).await;
        let mut client = client.expect("handshake");
        component
            .write_all(&wire(TAG_ERROR, b"tablet unplugged"))
            .await
            .expect("write");
        assert_eq!(
            client.next_completion().await.unwrap_err(),
            "test component error: tablet unplugged"
        );
        drop(component);
        assert_eq!(
            client.next_completion().await.unwrap_err(),
            "test component closed the connection"
        );
    }

    #[test]
    fn partial_frames_consume_nothing_and_coalesced_frames_come_one_at_a_time() {
        let whole = wire(TAG_COMPLETE, b"abcdefgh");
        for split in 0..whole.len() {
            let mut buffer = whole[..split].to_vec();
            assert!(take_frame(&mut buffer, PEER).unwrap().is_none());
            assert_eq!(
                buffer,
                whole[..split],
                "a partial frame must not be consumed"
            );
        }
        let mut buffer = wire(TAG_COMPLETE, b"one");
        buffer.extend(wire(TAG_ERROR, b"two"));
        let mut seen = Vec::new();
        while let Some(frame) = take_frame(&mut buffer, PEER).unwrap() {
            seen.push(frame);
        }
        assert_eq!(
            seen,
            vec![
                (TAG_COMPLETE, b"one".to_vec()),
                (TAG_ERROR, b"two".to_vec())
            ]
        );
        let mut empty = 0_u32.to_le_bytes().to_vec();
        assert_eq!(
            take_frame(&mut empty, PEER).unwrap_err(),
            "test component sent an empty frame"
        );
        let mut oversized = u32::try_from(MAX_FRAME_BYTES + 1)
            .unwrap()
            .to_le_bytes()
            .to_vec();
        assert!(take_frame(&mut oversized, PEER).is_err());
    }
}
