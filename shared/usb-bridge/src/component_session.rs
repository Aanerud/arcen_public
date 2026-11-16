// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! One Deck connection to a privileged USB component, served end to end.
//!
//! The component has captured a tablet and wrapped it in a [`UsbExporter`];
//! the Deck sends URB submissions and cancellations and reads completions
//! ([`crate::component_ipc`]). Everything between the connected stream and
//! the exporter is the same on every platform, so it lives here:
//!
//! - the hello (from the facts the exporter verified) and the info frame
//!   come first, before any completion;
//! - completions funnel through **one bounded queue and one writer**, so a
//!   cancellation acknowledgement never interleaves with a worker's
//!   completion, and a Deck that stops reading stalls only its own session;
//! - a write failure ends the session at once (the framing can no longer be
//!   trusted) through the platform's `abort`, which must unblock the reader;
//! - the first valid request clears the platform's first-request deadline:
//!   an established session may be silent as long as the tablet is idle;
//! - any malformed frame ends the session;
//! - a [`SessionRevocation`] ends it from outside when the authorisation it
//!   was served under no longer holds ([`crate::component_lease`]): no
//!   further request is served and no further completion forwarded, the Deck
//!   is told why (an error frame), the stream is aborted, and in-flight
//!   transfers are cancelled by the exporter's shutdown.
//!
//! URB payloads are `arcen-protocol` frames; this crate does not depend on
//! `arcen-protocol`, so the platform supplies them through [`UrbCodec`].

use std::io::{Read, Write};
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::component_ipc::{
    ComponentHello, TAG_CANCEL, TAG_COMPLETE, TAG_ERROR, TAG_HELLO, TAG_INFO, TAG_SUBMIT,
    encode_error, encode_hello, encode_info, read_frame, write_frame,
};
use crate::{AttachmentGeneration, ExportedCompletion, UrbId, UrbSubmission, UsbExporter};

/// Completion frames queued for the writer before producers wait.
pub const OUTBOUND_QUEUE_LIMIT: usize = 256;

/// The `arcen-protocol` URB codec, supplied by the platform.
pub trait UrbCodec: Send + Sync {
    /// Decodes a `usb_urb_submit` frame into a submission and its payload.
    ///
    /// # Errors
    ///
    /// A description of the malformed frame.
    fn decode_submit<'a>(&self, frame: &'a [u8]) -> Result<(UrbSubmission, &'a [u8]), String>;

    /// Decodes a `usb_urb_cancel` frame.
    ///
    /// # Errors
    ///
    /// A description of the malformed frame.
    fn decode_cancel(&self, frame: &[u8]) -> Result<(AttachmentGeneration, UrbId), String>;

    /// Encodes a `usb_urb_complete` frame.
    ///
    /// # Errors
    ///
    /// A description of why it cannot be encoded.
    fn encode_completion(&self, completion: &ExportedCompletion) -> Result<Vec<u8>, String>;
}

/// The hello a component sends for the device an exporter verified.
#[must_use]
pub fn hello_for(exporter: &UsbExporter) -> ComponentHello {
    let facts = exporter.facts();
    ComponentHello {
        vendor_id: facts.snapshot.id.vendor_id,
        product_id: facts.snapshot.id.product_id,
        bcd_device: facts.snapshot.id.bcd_device,
        device_class: facts.snapshot.device_class,
        speed: facts.speed,
        report_descriptors: facts
            .report_descriptors
            .iter()
            .map(|entry| (entry.interface, entry.descriptor.clone()))
            .collect(),
    }
}

/// How one session ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEnd {
    /// The Deck closed the stream (or the platform aborted it).
    Closed,
    /// The Deck sent something this boundary refuses; the text is safe to
    /// send back as a [`crate::component_ipc::TAG_ERROR`].
    Refused(String),
    /// The platform revoked the session ([`SessionRevocation::revoke`]); the
    /// reason was sent to the Deck.
    Revoked(String),
}

/// What the single writer sends.
enum Outbound {
    /// A `usb_urb_complete` frame.
    Completion(Vec<u8>),
    /// The revocation notice; the writer aborts the stream after it.
    Notice(Vec<u8>),
}

type Abort = Arc<dyn Fn() + Send + Sync>;

/// How a session is ended from outside its request loop.
#[derive(Clone)]
pub struct SessionStop {
    /// Makes any blocked read or write on the stream return (close or
    /// cancel it). Once [`Self::revocation`] is revoked it must not discard
    /// what was already written (on Windows: cancel the I/O, do not
    /// `DisconnectNamedPipe`), or the Deck never reads the notice.
    pub abort: Arc<dyn Fn() + Send + Sync>,
    /// Revokes the session when its authorisation no longer holds.
    pub revocation: Arc<SessionRevocation>,
}

impl std::fmt::Debug for SessionStop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionStop")
            .field("revocation", &self.revocation)
            .finish_non_exhaustive()
    }
}

#[derive(Default)]
struct RevocationState {
    reason: Option<String>,
    outbound: Option<SyncSender<Outbound>>,
    abort: Option<Abort>,
}

/// Ends a live session from outside: the authorisation it was served under
/// no longer holds. Shared between the session and whatever watches that
/// authorisation (session notifications, the client process, a stop).
#[derive(Default)]
pub struct SessionRevocation {
    state: Mutex<RevocationState>,
}

impl std::fmt::Debug for SessionRevocation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionRevocation")
            .field("reason", &self.state().reason)
            .finish_non_exhaustive()
    }
}

impl SessionRevocation {
    /// A revocation that has not happened.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn state(&self) -> MutexGuard<'_, RevocationState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Revokes the session with a reason the Deck is shown. The first call
    /// wins and returns `true`. A running session stops serving at once;
    /// the notice is queued for the Deck and the stream aborted after it
    /// (at once when the notice cannot be queued). The platform should
    /// still abort the stream after a short grace, in case the Deck stopped
    /// reading and the writer is blocked.
    pub fn revoke(&self, reason: &str) -> bool {
        let abort = {
            let mut state = self.state();
            if state.reason.is_some() {
                return false;
            }
            state.reason = Some(reason.to_owned());
            let queued = state.outbound.as_ref().is_some_and(|outbound| {
                outbound
                    .try_send(Outbound::Notice(encode_error(reason)))
                    .is_ok()
            });
            if queued { None } else { state.abort.clone() }
        };
        if let Some(abort) = abort {
            abort();
        }
        true
    }

    /// The reason, once revoked.
    #[must_use]
    pub fn reason(&self) -> Option<String> {
        self.state().reason.clone()
    }

    /// Whether the session was revoked.
    #[must_use]
    pub fn is_revoked(&self) -> bool {
        self.state().reason.is_some()
    }

    /// Connects a starting session; a revocation that already happened
    /// queues its notice now.
    fn attach(&self, outbound: SyncSender<Outbound>, abort: Abort) {
        let late = {
            let mut state = self.state();
            if let Some(reason) = state.reason.clone() {
                (outbound
                    .try_send(Outbound::Notice(encode_error(&reason)))
                    .is_err())
                .then(|| Arc::clone(&abort))
            } else {
                state.outbound = Some(outbound);
                state.abort = Some(abort);
                None
            }
        };
        if let Some(abort) = late {
            abort();
        }
    }

    /// Lets the writer finish: drops the session's queue handle.
    fn detach(&self) {
        let mut state = self.state();
        state.outbound = None;
        state.abort = None;
    }
}

/// Serves one connected, already verified Deck until it goes away or
/// `revocation` ends it.
///
/// `writer` moves to the writer thread. `cleared_deadline` runs once, after
/// the first valid request. `stop.abort` must make any blocked read or write
/// on the stream return (close or cancel it); it runs on a write failure,
/// after a revocation notice, and when the session ends. The exporter is shut down
/// before this returns (cancelling in-flight transfers); dropping the
/// caller's last reference releases the device.
///
/// # Errors
///
/// A message when the hello or info frame cannot be written; the session
/// never started.
pub fn serve_session<W: Write + Send + 'static>(
    exporter: &Arc<UsbExporter>,
    codec: &Arc<dyn UrbCodec>,
    backend: &str,
    reader: &mut impl Read,
    mut writer: W,
    cleared_deadline: impl FnOnce(),
    stop: &SessionStop,
) -> Result<SessionEnd, String> {
    let SessionStop { abort, revocation } = stop;
    if let Some(reason) = revocation.reason() {
        exporter.shutdown();
        let _ = write_frame(&mut writer, TAG_ERROR, &encode_error(&reason));
        abort();
        return Ok(SessionEnd::Revoked(reason));
    }
    let hello = encode_hello(&hello_for(exporter))?;
    write_frame(&mut writer, TAG_HELLO, &hello)
        .map_err(|error| format!("send the handshake: {error}"))?;
    write_frame(&mut writer, TAG_INFO, &encode_info(backend))
        .map_err(|error| format!("send the component info: {error}"))?;

    let (outbound, queued) = std::sync::mpsc::sync_channel::<Outbound>(OUTBOUND_QUEUE_LIMIT);
    let writer_abort = Arc::clone(abort);
    let pump = std::thread::spawn(move || pump(&queued, writer, &*writer_abort));
    revocation.attach(outbound.clone(), Arc::clone(abort));

    let drain_exporter = Arc::clone(exporter);
    let drain_codec = Arc::clone(codec);
    let drain_outbound = outbound.clone();
    let drain_revocation = Arc::clone(revocation);
    let drain = std::thread::spawn(move || {
        while let Ok(completion) = drain_exporter.next_completion() {
            if drain_revocation.is_revoked() {
                break;
            }
            let Ok(frame) = drain_codec.encode_completion(&completion) else {
                break;
            };
            if drain_outbound.send(Outbound::Completion(frame)).is_err() {
                break;
            }
        }
    });

    let end = run_requests(
        exporter,
        &**codec,
        reader,
        &outbound,
        cleared_deadline,
        revocation,
    );
    drop(outbound);
    exporter.shutdown();
    abort();
    let _ = drain.join();
    revocation.detach();
    let _ = pump.join();
    Ok(revocation.reason().map_or(end, SessionEnd::Revoked))
}

fn pump(queued: &Receiver<Outbound>, mut writer: impl Write, abort: &(dyn Fn() + Send + Sync)) {
    while let Ok(item) = queued.recv() {
        let (tag, frame, last) = match item {
            Outbound::Completion(frame) => (TAG_COMPLETE, frame, false),
            Outbound::Notice(reason) => (TAG_ERROR, reason, true),
        };
        if write_frame(&mut writer, tag, &frame).is_err() || last {
            abort();
            break;
        }
    }
}

fn run_requests(
    exporter: &UsbExporter,
    codec: &dyn UrbCodec,
    reader: &mut impl Read,
    outbound: &SyncSender<Outbound>,
    cleared_deadline: impl FnOnce(),
    revocation: &SessionRevocation,
) -> SessionEnd {
    let mut cleared_deadline = Some(cleared_deadline);
    loop {
        let Ok((tag, payload)) = read_frame(reader) else {
            return SessionEnd::Closed;
        };
        if let Some(reason) = revocation.reason() {
            return SessionEnd::Revoked(reason);
        }
        let outcome = match tag {
            TAG_SUBMIT => codec
                .decode_submit(&payload)
                .and_then(|(submission, body)| {
                    exporter
                        .submit(submission, body)
                        .map_err(|error| error.to_string())
                }),
            TAG_CANCEL => codec.decode_cancel(&payload).and_then(|(generation, urb)| {
                let cancelled = exporter
                    .cancel(generation, urb)
                    .map_err(|error| error.to_string())?;
                let frame = codec.encode_completion(&cancelled)?;
                // The writer is gone: the session is ending anyway.
                let _ = outbound.send(Outbound::Completion(frame));
                Ok(())
            }),
            other => Err(format!("unexpected frame tag {other:#04x}")),
        };
        if let Err(refusal) = outcome {
            return SessionEnd::Refused(refusal);
        }
        if let Some(clear) = cleared_deadline.take() {
            clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::component_ipc::{decode_hello, info_backend, take_frame};
    use crate::{
        EndpointAddress, SetupPacket, SyntheticDevice, TransferKind, UrbStatus, UsbDeviceId,
    };
    use std::num::{NonZeroU32, NonZeroU64};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    /// A test codec: submit = generation u64, urb u32, endpoint u8, kind u8,
    /// length u32, then the 8 setup bytes for control; cancel = generation,
    /// urb; completion = urb u32, status u8, data.
    struct TestCodec;

    fn generation(frame: &[u8]) -> Result<AttachmentGeneration, String> {
        let raw = u64::from_le_bytes(frame.get(..8).ok_or("short")?.try_into().map_err(|_| "x")?);
        Ok(AttachmentGeneration::new(
            NonZeroU64::new(raw).ok_or("zero")?,
        ))
    }

    fn urb_id(frame: &[u8]) -> Result<UrbId, String> {
        let raw = u32::from_le_bytes(
            frame
                .get(8..12)
                .ok_or("short")?
                .try_into()
                .map_err(|_| "x")?,
        );
        Ok(UrbId::new(NonZeroU32::new(raw).ok_or("zero")?))
    }

    impl UrbCodec for TestCodec {
        fn decode_submit<'a>(&self, frame: &'a [u8]) -> Result<(UrbSubmission, &'a [u8]), String> {
            let endpoint = EndpointAddress(*frame.get(12).ok_or("short")?);
            let control = *frame.get(13).ok_or("short")? == 0;
            let length = u32::from_le_bytes(
                frame
                    .get(14..18)
                    .ok_or("short")?
                    .try_into()
                    .map_err(|_| "x")?,
            );
            let (setup, body) = if control {
                let raw = frame.get(18..26).ok_or("short")?;
                (
                    Some(SetupPacket {
                        request_type: raw[0],
                        request: raw[1],
                        value: u16::from_le_bytes([raw[2], raw[3]]),
                        index: u16::from_le_bytes([raw[4], raw[5]]),
                        length: u16::from_le_bytes([raw[6], raw[7]]),
                    }),
                    &frame[26..],
                )
            } else {
                (None, &frame[18..])
            };
            Ok((
                UrbSubmission {
                    generation: generation(frame)?,
                    urb_id: urb_id(frame)?,
                    endpoint,
                    transfer_kind: if control {
                        TransferKind::Control
                    } else {
                        TransferKind::Interrupt
                    },
                    timeout_ms: 1000,
                    declared_length: length,
                    setup,
                },
                body,
            ))
        }

        fn decode_cancel(&self, frame: &[u8]) -> Result<(AttachmentGeneration, UrbId), String> {
            Ok((generation(frame)?, urb_id(frame)?))
        }

        fn encode_completion(&self, completion: &ExportedCompletion) -> Result<Vec<u8>, String> {
            let mut out = completion.urb_id.get().to_le_bytes().to_vec();
            out.push(match completion.status {
                UrbStatus::Success => 0,
                UrbStatus::Cancelled => 1,
                _ => 2,
            });
            out.extend_from_slice(&completion.data);
            Ok(out)
        }
    }

    fn submit(urb: u32, endpoint: u8, length: u32, setup: Option<[u8; 8]>) -> Vec<u8> {
        let mut frame = 7_u64.to_le_bytes().to_vec();
        frame.extend_from_slice(&urb.to_le_bytes());
        frame.push(endpoint);
        frame.push(u8::from(setup.is_none()));
        frame.extend_from_slice(&length.to_le_bytes());
        if let Some(setup) = setup {
            frame.extend_from_slice(&setup);
        }
        crate::component_ipc::encode_frame(TAG_SUBMIT, &frame).expect("frame")
    }

    fn cancel(urb: u32) -> Vec<u8> {
        let mut frame = 7_u64.to_le_bytes().to_vec();
        frame.extend_from_slice(&urb.to_le_bytes());
        crate::component_ipc::encode_frame(TAG_CANCEL, &frame).expect("frame")
    }

    /// The Deck's side of the stream, collecting what the component writes.
    #[derive(Clone, Default)]
    struct Collected(Arc<Mutex<Vec<u8>>>);

    impl Write for Collected {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("lock").extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn frames(collected: &Collected) -> Vec<(u8, Vec<u8>)> {
        let mut buffer = collected.0.lock().expect("lock").clone();
        let mut out = Vec::new();
        while let Some(frame) = take_frame(&mut buffer).expect("valid") {
            out.push(frame);
        }
        out
    }

    /// The Deck's requests, then end of stream after the component had time
    /// to answer (a real Deck keeps the stream open while it waits).
    struct Requests {
        bytes: Vec<u8>,
        at: usize,
    }

    impl Read for Requests {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.at == self.bytes.len() {
                std::thread::sleep(std::time::Duration::from_millis(300));
                return Ok(0);
            }
            let n = buf.len().min(self.bytes.len() - self.at);
            buf[..n].copy_from_slice(&self.bytes[self.at..self.at + n]);
            self.at += n;
            Ok(n)
        }
    }

    fn served(requests: &[Vec<u8>]) -> (SessionEnd, Vec<(u8, Vec<u8>)>, usize, bool) {
        let (device, _handle) = SyntheticDevice::capture().expect("synthetic tablet");
        let exporter = Arc::new(UsbExporter::new(Box::new(device)).expect("admitted"));
        let collected = Collected::default();
        let mut input = Requests {
            bytes: requests.concat(),
            at: 0,
        };
        let aborts = Arc::new(AtomicUsize::new(0));
        let cleared = AtomicBool::new(false);
        let counter = Arc::clone(&aborts);
        let codec: Arc<dyn UrbCodec> = Arc::new(TestCodec);
        let abort: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            counter.fetch_add(1, Ordering::AcqRel);
        });
        let end = serve_session(
            &exporter,
            &codec,
            "synthetic",
            &mut input,
            collected.clone(),
            || cleared.store(true, Ordering::Release),
            &SessionStop {
                abort,
                revocation: Arc::new(SessionRevocation::new()),
            },
        )
        .expect("session starts");
        (
            end,
            frames(&collected),
            aborts.load(Ordering::Acquire),
            cleared.load(Ordering::Acquire),
        )
    }

    #[test]
    fn the_hello_and_info_come_first_and_describe_the_verified_device() {
        let (end, frames, aborts, cleared) = served(&[]);
        assert_eq!(end, SessionEnd::Closed);
        assert!(aborts >= 1, "the stream is closed when the session ends");
        assert!(!cleared, "no request, no cleared deadline");
        assert_eq!(frames[0].0, TAG_HELLO);
        let hello = decode_hello(&frames[0].1).expect("hello decodes");
        let (device, _) = SyntheticDevice::capture().expect("synthetic tablet");
        let facts = crate::CapturedUsbDevice::facts(&device).clone();
        assert_eq!(
            UsbDeviceId {
                vendor_id: hello.vendor_id,
                product_id: hello.product_id,
                bcd_device: hello.bcd_device,
            },
            facts.snapshot.id
        );
        assert!(!hello.report_descriptors.is_empty());
        assert_eq!(frames[1].0, TAG_INFO);
        assert_eq!(info_backend(&frames[1].1).as_deref(), Some("synthetic"));
    }

    #[test]
    fn a_control_request_and_a_cancelled_interrupt_are_answered_in_order() {
        // GET_DESCRIPTOR(Device), 18 bytes.
        let get_device = [0x80, 6, 0x00, 0x01, 0, 0, 18, 0];
        let (end, frames, _, cleared) = served(&[
            submit(1, 0x00, 18, Some(get_device)),
            submit(2, 0x81, 64, None),
            cancel(2),
        ]);
        assert_eq!(end, SessionEnd::Closed);
        assert!(cleared);
        let completions: Vec<_> = frames
            .iter()
            .filter(|(tag, _)| *tag == TAG_COMPLETE)
            .collect();
        let device_descriptor = completions
            .iter()
            .find(|(_, body)| body[..4] == 1_u32.to_le_bytes())
            .expect("the control request completed");
        assert_eq!(device_descriptor.1[4], 0, "success");
        assert_eq!(device_descriptor.1[5..].len(), 18);
        assert_eq!(device_descriptor.1[6], 1, "a device descriptor");
        let cancelled: Vec<_> = completions
            .iter()
            .filter(|(_, body)| body[..4] == 2_u32.to_le_bytes())
            .collect();
        assert_eq!(cancelled.len(), 1, "exactly one terminal completion");
        assert_eq!(cancelled[0].1[4], 1, "cancelled");
    }

    #[test]
    fn a_malformed_or_unexpected_frame_ends_the_session_with_a_reason() {
        let (end, _, _, cleared) =
            served(&[crate::component_ipc::encode_frame(0x7f, b"?").expect("frame")]);
        assert_eq!(
            end,
            SessionEnd::Refused("unexpected frame tag 0x7f".to_owned())
        );
        assert!(!cleared);
        let (end, _, _, _) =
            served(&[crate::component_ipc::encode_frame(TAG_SUBMIT, b"short").expect("frame")]);
        assert!(matches!(end, SessionEnd::Refused(_)));
    }

    #[test]
    fn a_failing_writer_aborts_the_session() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("gone"))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let (device, _handle) = SyntheticDevice::capture().expect("synthetic tablet");
        let exporter = Arc::new(UsbExporter::new(Box::new(device)).expect("admitted"));
        let codec: Arc<dyn UrbCodec> = Arc::new(TestCodec);
        let abort: Arc<dyn Fn() + Send + Sync> = Arc::new(|| {});
        let outcome = serve_session(
            &exporter,
            &codec,
            "synthetic",
            &mut [].as_slice(),
            Broken,
            || {},
            &SessionStop {
                abort,
                revocation: Arc::new(SessionRevocation::new()),
            },
        );
        assert!(outcome.is_err(), "no hello, no session");
    }

    /// Requests, then a stream that stays open until the session aborts it.
    struct OpenRequests {
        bytes: Vec<u8>,
        at: usize,
        aborted: Arc<AtomicBool>,
    }

    impl Read for OpenRequests {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.at == self.bytes.len() {
                for _ in 0..500 {
                    if self.aborted.load(Ordering::Acquire) {
                        return Ok(0);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                return Ok(0);
            }
            let n = buf.len().min(self.bytes.len() - self.at);
            buf[..n].copy_from_slice(&self.bytes[self.at..self.at + n]);
            self.at += n;
            Ok(n)
        }
    }

    /// Serves `requests` on a stream that stays open; `during` runs on
    /// another thread with the revocation once the session is up.
    fn served_until_revoked(
        requests: &[Vec<u8>],
        revocation: &Arc<SessionRevocation>,
        during: impl FnOnce(Arc<SessionRevocation>) + Send + 'static,
    ) -> (SessionEnd, Vec<(u8, Vec<u8>)>, bool) {
        let (device, _handle) = SyntheticDevice::capture().expect("synthetic tablet");
        let exporter = Arc::new(UsbExporter::new(Box::new(device)).expect("admitted"));
        let collected = Collected::default();
        let aborted = Arc::new(AtomicBool::new(false));
        let mut input = OpenRequests {
            bytes: requests.concat(),
            at: 0,
            aborted: Arc::clone(&aborted),
        };
        let flag = Arc::clone(&aborted);
        let abort: Arc<dyn Fn() + Send + Sync> =
            Arc::new(move || flag.store(true, Ordering::Release));
        let codec: Arc<dyn UrbCodec> = Arc::new(TestCodec);
        let watcher = {
            let revocation = Arc::clone(revocation);
            std::thread::spawn(move || during(revocation))
        };
        let end = serve_session(
            &exporter,
            &codec,
            "synthetic",
            &mut input,
            collected.clone(),
            || {},
            &SessionStop {
                abort,
                revocation: Arc::clone(revocation),
            },
        )
        .expect("session starts");
        let _ = watcher.join();
        (end, frames(&collected), aborted.load(Ordering::Acquire))
    }

    const REASON: &str = "the tablet was released: another session (2) now has this PC's console";

    #[test]
    fn a_revocation_ends_a_live_session_tells_the_deck_and_aborts_the_stream() {
        let get_device = [0x80, 6, 0x00, 0x01, 0, 0, 18, 0];
        let revocation = Arc::new(SessionRevocation::new());
        let (end, frames, aborted) = served_until_revoked(
            // A control request that completes, and an interrupt IN that
            // stays in flight until the revocation cancels it.
            &[
                submit(1, 0x00, 18, Some(get_device)),
                submit(2, 0x81, 64, None),
            ],
            &revocation,
            |revocation| {
                std::thread::sleep(std::time::Duration::from_millis(200));
                assert!(revocation.revoke(REASON));
                assert!(!revocation.revoke("second"), "the first reason wins");
            },
        );
        assert_eq!(end, SessionEnd::Revoked(REASON.to_owned()));
        assert!(aborted, "the stream is aborted after the notice");
        let notice = frames
            .iter()
            .position(|(tag, _)| *tag == TAG_ERROR)
            .expect("the Deck is told why");
        assert_eq!(frames[notice].1, encode_error(REASON));
        assert_eq!(
            notice,
            frames.len() - 1,
            "nothing is forwarded after the notice"
        );
        assert!(
            frames
                .iter()
                .any(|(tag, body)| *tag == TAG_COMPLETE && body[..4] == 1_u32.to_le_bytes()),
            "requests before the revocation were served"
        );
    }

    #[test]
    fn a_request_after_the_revocation_is_not_served() {
        let get_device = [0x80, 6, 0x00, 0x01, 0, 0, 18, 0];
        let revocation = Arc::new(SessionRevocation::new());
        revocation.revoke(REASON);
        let (end, frames, aborted) = served_until_revoked(
            &[submit(1, 0x00, 18, Some(get_device))],
            &revocation,
            |_| {},
        );
        assert_eq!(end, SessionEnd::Revoked(REASON.to_owned()));
        assert!(aborted);
        assert_eq!(
            frames,
            vec![(TAG_ERROR, encode_error(REASON))],
            "a session revoked before it started sends no hello and serves nothing"
        );
    }

    /// Delivers the first request, then the second only once the session
    /// was revoked, then stays open until aborted.
    struct RequestsAcrossRevocation {
        first: Vec<u8>,
        second: Vec<u8>,
        stage: u8,
        revocation: Arc<SessionRevocation>,
        aborted: Arc<AtomicBool>,
    }

    impl Read for RequestsAcrossRevocation {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.first.is_empty() && self.stage == 0 {
                self.stage = 1;
            }
            if self.stage == 1 && self.second.is_empty() {
                self.stage = 2;
            }
            let pending = match self.stage {
                0 => &mut self.first,
                1 => {
                    for _ in 0..500 {
                        if self.revocation.is_revoked() {
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    &mut self.second
                }
                _ => {
                    for _ in 0..500 {
                        if self.aborted.load(Ordering::Acquire) {
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    return Ok(0);
                }
            };
            let n = buf.len().min(pending.len());
            buf[..n].copy_from_slice(&pending[..n]);
            pending.drain(..n);
            Ok(n)
        }
    }

    #[test]
    fn a_request_arriving_after_a_live_revocation_is_not_served() {
        let get_device = [0x80, 6, 0x00, 0x01, 0, 0, 18, 0];
        let (device, _handle) = SyntheticDevice::capture().expect("synthetic tablet");
        let exporter = Arc::new(UsbExporter::new(Box::new(device)).expect("admitted"));
        let collected = Collected::default();
        let revocation = Arc::new(SessionRevocation::new());
        let aborted = Arc::new(AtomicBool::new(false));
        let mut input = RequestsAcrossRevocation {
            first: submit(1, 0x00, 18, Some(get_device)),
            second: submit(2, 0x00, 18, Some(get_device)),
            stage: 0,
            revocation: Arc::clone(&revocation),
            aborted: Arc::clone(&aborted),
        };
        let flag = Arc::clone(&aborted);
        let codec: Arc<dyn UrbCodec> = Arc::new(TestCodec);
        let revoker = {
            let revocation = Arc::clone(&revocation);
            // Revokes once the first request was served (its deadline cleared).
            move || {
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    revocation.revoke(REASON);
                });
            }
        };
        let end = serve_session(
            &exporter,
            &codec,
            "synthetic",
            &mut input,
            collected.clone(),
            revoker,
            &SessionStop {
                abort: Arc::new(move || flag.store(true, Ordering::Release)),
                revocation: Arc::clone(&revocation),
            },
        )
        .expect("session starts");
        assert_eq!(end, SessionEnd::Revoked(REASON.to_owned()));
        let frames = frames(&collected);
        let completed = |urb: u32| {
            frames
                .iter()
                .any(|(tag, body)| *tag == TAG_COMPLETE && body[..4] == urb.to_le_bytes())
        };
        assert!(completed(1), "the request before the revocation was served");
        assert!(!completed(2), "the request after it was not");
        assert_eq!(frames.last(), Some(&(TAG_ERROR, encode_error(REASON))));
    }

    #[test]
    fn a_revocation_that_cannot_queue_its_notice_aborts_at_once() {
        let revocation = SessionRevocation::new();
        let (outbound, _queued) = std::sync::mpsc::sync_channel::<Outbound>(0);
        let aborted = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&aborted);
        revocation.attach(
            outbound,
            Arc::new(move || flag.store(true, Ordering::Release)),
        );
        assert!(revocation.revoke(REASON));
        assert!(
            aborted.load(Ordering::Acquire),
            "a full queue cannot delay the end"
        );
        assert_eq!(revocation.reason().as_deref(), Some(REASON));
    }
}
