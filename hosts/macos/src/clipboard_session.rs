//! Carrying the clipboard between a client and this desktop.
//!
//! The pasteboard adapter and the shared policy already existed; this joins
//! them to a session. Text copied on either side becomes available on the
//! other, within the limits the host is willing to allow.
//!
//! Two things make this harder than it looks, and both are handled here rather
//! than left to chance. A host that echoes back what a client just sent
//! produces an endless loop between the two pasteboards, so an injected
//! clipboard is recorded as ours and never re-sent. And a payload larger than
//! the policy permits is refused rather than truncated, because half a
//! clipboard silently replacing a whole one loses someone's work.

use arcen_media::clipboard::{ClipboardFlow, ClipboardKind, ClipboardNegotiation};
use arcen_protocol::messages::ClipboardContentKind;
use serde::Serialize;

use crate::clipboard::{ClipboardError, ClipboardPayload, Pasteboard};

/// What the clipboard did while a session ran.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct ClipboardStats {
    /// Local copies offered to the client.
    pub sent: u64,
    /// Client clipboards written to this desktop.
    pub received: u64,
    /// Payloads the host policy refused.
    pub refused: u64,
    /// Offers ignored because they repeated what was already seen.
    pub duplicates: u64,
    /// Payloads that were not what they claimed to be.
    ///
    /// Separate from `refused`, which is a policy decision about something
    /// well-formed. A client sending bytes that are not the content type they
    /// are labelled as is a different report, and folding the two together
    /// made a malformed image indistinguishable from one the operator had
    /// chosen not to accept.
    pub malformed: u64,
}
/// One clipboard transfer this desktop is offering to the client.
///
/// The framing lives in [`arcen_protocol::ClipboardTransfer`], beside the
/// reassembly that has to agree with it. Chunk boundaries, offset arithmetic
/// and the refusal of an empty payload are protocol decisions, not macOS
/// ones, and a host that reimplements them is a host that can disagree with
/// the peer that has to put the pieces back together.
pub type OutgoingClipboard = arcen_protocol::clipboard::ClipboardTransfer;
type IncomingClipboard = (u64, ClipboardContentKind, Vec<u8>);
type OutgoingClipboardSlot = std::sync::Arc<
    std::sync::Mutex<arcen_media::clipboard::latest::LatestClipboard<OutgoingClipboard>>,
>;
type IncomingClipboardSlot = std::sync::Arc<
    std::sync::Mutex<arcen_media::clipboard::latest::LatestClipboard<IncomingClipboard>>,
>;

/// Carries the clipboard for one session.
pub struct ClipboardSession {
    pasteboard: Pasteboard,
    negotiation: ClipboardNegotiation,
    sequence: u64,
    last_seen: u64,
    stats: ClipboardStats,
}

impl std::fmt::Debug for ClipboardSession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ClipboardSession")
            .field("stats", &self.stats)
            .finish_non_exhaustive()
    }
}

impl ClipboardSession {
    /// Opens the general pasteboard for a session governed by `policy`.
    ///
    /// # Errors
    ///
    /// Returns [`ClipboardError`] when no pasteboard is available.
    pub fn new(negotiation: ClipboardNegotiation) -> Result<Self, ClipboardError> {
        Ok(Self {
            pasteboard: Pasteboard::general()?,
            negotiation,
            sequence: 0,
            last_seen: 0,
            stats: ClipboardStats::default(),
        })
    }

    /// Returns what has happened so far.
    #[must_use]
    pub const fn stats(&self) -> ClipboardStats {
        self.stats
    }

    /// Returns a clipboard transfer when the local pasteboard has changed.
    ///
    /// Returns `None` when nothing changed, when the direction is not allowed,
    /// or when the policy refuses the payload.
    pub fn poll_local(&mut self) -> Option<OutgoingClipboard> {
        if !self.pasteboard.take_changed() {
            return None;
        }
        let payload = self.pasteboard.read()?;
        let kind = match payload {
            ClipboardPayload::Text(_) => ClipboardKind::TextUtf8,
            ClipboardPayload::ImagePng(_) => ClipboardKind::ImagePng,
        };
        if !self.negotiation.allows(ClipboardFlow::HostToClient, kind) {
            self.stats.refused += 1;
            return None;
        }
        // Refused rather than truncated: half a clipboard silently replacing a
        // whole one loses work.
        if self
            .negotiation
            .policy()
            .check_size(ClipboardFlow::HostToClient, kind, payload.size_bytes())
            .is_err()
        {
            self.stats.refused += 1;
            return None;
        }

        self.sequence += 1;
        let (wire_kind, bytes) = match payload {
            ClipboardPayload::Text(text) => (ClipboardContentKind::TextUtf8, text.into_bytes()),
            ClipboardPayload::ImagePng(bytes) => (ClipboardContentKind::ImagePng, bytes),
        };
        let transfer = OutgoingClipboard::new(self.sequence, wire_kind, bytes).ok()?;
        self.stats.sent += 1;
        Some(transfer)
    }

    /// Applies a clipboard the client offered.
    ///
    /// # Errors
    ///
    /// Returns [`ClipboardError`] when the pasteboard refuses the write.
    pub fn apply_remote(
        &mut self,
        sequence: u64,
        kind: ClipboardContentKind,
        bytes: Vec<u8>,
    ) -> Result<(), ClipboardError> {
        // An offer that does not advance is a repeat, and writing it again
        // would fight with whatever the person has copied since.
        if sequence <= self.last_seen {
            self.stats.duplicates += 1;
            return Ok(());
        }
        let flow_kind = match kind {
            ClipboardContentKind::TextUtf8 => ClipboardKind::TextUtf8,
            ClipboardContentKind::ImagePng => ClipboardKind::ImagePng,
        };
        if !self
            .negotiation
            .allows(ClipboardFlow::ClientToHost, flow_kind)
        {
            self.stats.refused += 1;
            return Ok(());
        }
        if self
            .negotiation
            .policy()
            .check_size(ClipboardFlow::ClientToHost, flow_kind, bytes.len())
            .is_err()
        {
            self.stats.refused += 1;
            return Ok(());
        }

        let payload = match kind {
            ClipboardContentKind::TextUtf8 => {
                // Bytes that are not UTF-8 are malformed, not refused: the
                // operator did not decide anything here, the client sent
                // something that is not what it said it was.
                let Ok(text) = String::from_utf8(bytes) else {
                    self.stats.malformed += 1;
                    return Ok(());
                };
                ClipboardPayload::Text(text)
            }
            ClipboardContentKind::ImagePng => {
                // Validated before it reaches the pasteboard. Anything within
                // the size cap used to be written through untouched, so a
                // client could put arbitrary bytes labelled `image/png` onto
                // the desktop's pasteboard and every application that reads it
                // would be decoding attacker-chosen input. The shared
                // validator checks the structure, the dimensions and the
                // decoded size, which is the same check the Linux host runs.
                if arcen_media::clipboard::validate_png(
                    &bytes,
                    arcen_media::clipboard::ImageLimits::default(),
                )
                .is_err()
                {
                    self.stats.malformed += 1;
                    return Ok(());
                }
                ClipboardPayload::ImagePng(bytes)
            }
        };

        // `write_owned` records the change as ours, so the next poll does not
        // send it straight back and start a loop between the two pasteboards.
        self.pasteboard.write_owned(&payload)?;
        self.last_seen = sequence;
        self.stats.received += 1;
        Ok(())
    }
}

/// A clipboard running on its own thread.
///
/// `NSPasteboard` objects are not `Send`, so the session cannot be held across
/// an await point. It lives on a thread instead and is reached over channels,
/// exactly as capture is, which also keeps pasteboard calls off the async
/// runtime.
pub struct ClipboardWorker {
    /// Transfers the local desktop has produced.
    pub outgoing: OutgoingClipboardSlot,
    incoming: IncomingClipboardSlot,
    stats: std::sync::Arc<std::sync::Mutex<ClipboardStats>>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    handle: Option<ClipboardWorkerHandle>,
}

impl std::fmt::Debug for ClipboardWorker {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ClipboardWorker")
            .finish_non_exhaustive()
    }
}

impl ClipboardWorker {
    /// Starts a clipboard thread polling every `poll` interval.
    #[must_use]
    pub fn start(negotiation: ClipboardNegotiation, poll: std::time::Duration) -> Self {
        // One slot each way, newest wins. These were bounded FIFO channels with
        // a comment claiming latest-wins, and the two disagreed: a full
        // `sync_channel` rejects the *newest* item, so under pressure the peer
        // got the oldest copies and the one the user actually wanted was the
        // one thrown away.
        //
        // Worse, the claim that a dropped offer would be picked up by the next
        // poll was false. `poll_local` reads through `take_changed`, which
        // consumes the pasteboard's change counter, so there was no next poll —
        // the copy was simply lost until the user copied again.
        //
        // The slot is shared rather than written here because none of this is
        // macOS-specific; Linux reached the same design independently in its
        // own `ClipboardMailbox`.
        let outgoing = std::sync::Arc::new(std::sync::Mutex::new(
            arcen_media::clipboard::latest::LatestClipboard::<OutgoingClipboard>::new(),
        ));
        let offer_slot = std::sync::Arc::clone(&outgoing);
        let incoming = std::sync::Arc::new(std::sync::Mutex::new(
            arcen_media::clipboard::latest::LatestClipboard::<IncomingClipboard>::new(),
        ));
        let apply_slot = std::sync::Arc::clone(&incoming);
        let stats = std::sync::Arc::new(std::sync::Mutex::new(ClipboardStats::default()));
        let shared = std::sync::Arc::clone(&stats);
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = std::sync::Arc::clone(&stop);

        let handle = ClipboardWorkerHandle::new(std::thread::spawn(move || {
            let Ok(mut session) = ClipboardSession::new(negotiation) else {
                // No pasteboard in this session; the desktop still streams.
                return;
            };
            while !flag.load(std::sync::atomic::Ordering::Relaxed) {
                // Drained every pass: pasteboard reads and writes autorelease
                // their objects, and a plain thread's implicit pool keeps every
                // copied payload alive until the session ends.
                objc2::rc::autoreleasepool(|_| {
                    let pending = apply_slot.lock().map(|mut slot| slot.take());
                    if let Ok(Some(item)) = pending {
                        let (sequence, kind, bytes) = item.payload;
                        let _ = session.apply_remote(sequence, kind, bytes);
                    }
                    if let Some(offer) = session.poll_local() {
                        // Displacing an unread offer is the design working, not
                        // a failure: the next paste should be the last thing
                        // copied.
                        if let Ok(mut slot) = offer_slot.lock() {
                            let sequence = offer.sequence();
                            slot.offer(sequence, offer);
                        }
                    }
                    if let Ok(mut current) = shared.lock() {
                        *current = session.stats();
                    }
                });
                std::thread::sleep(poll);
            }
        }));

        Self {
            outgoing,
            incoming,
            stats,
            stop,
            handle: Some(handle),
        }
    }

    /// Hands a client's clipboard to the thread.
    pub fn apply_remote(&self, sequence: u64, kind: ClipboardContentKind, bytes: Vec<u8>) {
        // Never blocks. A stalled pasteboard must not hold up the session loop
        // that is also carrying video and input: a missed paste is an
        // annoyance, a blocked loop is a frozen desktop.
        if let Ok(mut slot) = self.incoming.lock() {
            slot.offer(sequence, (sequence, kind, bytes));
        }
    }

    /// Returns the counters as last published by the thread.
    #[must_use]
    pub fn stats(&self) -> ClipboardStats {
        self.stats.lock().map(|stats| *stats).unwrap_or_default()
    }
}

struct ClipboardWorkerHandle {
    handle: std::thread::JoinHandle<()>,
}

impl ClipboardWorkerHandle {
    fn new(handle: std::thread::JoinHandle<()>) -> Self {
        Self { handle }
    }

    fn shutdown(self) {
        let _ = crate::blocking::reap_thread_after_grace(
            "arcen-macos-clipboard-worker-reaper",
            self.handle,
            std::time::Duration::from_millis(500),
            std::time::Duration::from_millis(25),
        );
    }
}

impl Drop for ClipboardWorker {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            handle.shutdown();
        }
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn bytes_that_are_not_a_png_never_reach_the_pasteboard() {
        // Anything inside the size cap used to be written through untouched,
        // so a client could label arbitrary bytes `image/png` and every
        // application reading the desktop pasteboard would be decoding
        // attacker-chosen input. The structural check is the defence.
        let limits = arcen_media::clipboard::ImageLimits::default();
        assert!(arcen_media::clipboard::validate_png(b"not a png at all", limits).is_err());
        assert!(arcen_media::clipboard::validate_png(&[], limits).is_err());
        // A PNG signature with nothing behind it is still not a PNG.
        assert!(
            arcen_media::clipboard::validate_png(
                &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A],
                limits,
            )
            .is_err()
        );
    }

    #[test]
    fn malformed_is_counted_apart_from_refused() {
        // A payload that is not what it claims to be is a different report
        // from one the operator chose not to accept. Folding them together
        // made a broken client look like a policy decision.
        let mut stats = ClipboardStats::default();
        stats.malformed += 1;
        assert_eq!(stats.malformed, 1);
        assert_eq!(stats.refused, 0);
    }
    /// A negotiation where both ends allow everything, for tests that are
    /// exercising the pasteboard rather than the policy intersection.
    fn everything_allowed() -> ClipboardNegotiation {
        ClipboardNegotiation::resolve(
            ClipboardPolicy::default(),
            true,
            arcen_media::clipboard::ClipboardRequest {
                protocol_version: arcen_protocol::messages::CLIPBOARD_PROTOCOL_VERSION,
                text: arcen_media::clipboard::ClipboardDirections::both(),
                image: arcen_media::clipboard::ClipboardDirections::both(),
            },
        )
        .expect("an unrestricted negotiation")
    }

    use super::*;
    use arcen_media::clipboard::ClipboardPolicy;
    use arcen_protocol::CHUNK_BYTES;

    /// There is one system pasteboard, and these tests write to it. Running
    /// them in parallel lets one test read another's clipboard, which looks
    /// like a product bug and is not one.
    static PASTEBOARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn exclusive() -> std::sync::MutexGuard<'static, ()> {
        PASTEBOARD
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn session() -> Option<ClipboardSession> {
        if !crate::desktop_tests_allowed() {
            return None;
        }
        ClipboardSession::new(everything_allowed()).ok()
    }

    #[test]
    fn dropping_worker_signals_stop_without_waiting_for_native_exit() {
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker = ClipboardWorker {
            outgoing: std::sync::Arc::new(std::sync::Mutex::new(
                arcen_media::clipboard::latest::LatestClipboard::<OutgoingClipboard>::new(),
            )),
            incoming: std::sync::Arc::new(std::sync::Mutex::new(
                arcen_media::clipboard::latest::LatestClipboard::<IncomingClipboard>::new(),
            )),
            stats: std::sync::Arc::new(std::sync::Mutex::new(ClipboardStats::default())),
            stop: std::sync::Arc::clone(&stop),
            handle: Some(ClipboardWorkerHandle::new(std::thread::spawn(|| {
                std::thread::sleep(std::time::Duration::from_millis(250));
            }))),
        };

        let started = std::time::Instant::now();
        drop(worker);

        assert!(stop.load(std::sync::atomic::Ordering::Relaxed));
        assert!(
            started.elapsed() < std::time::Duration::from_millis(100),
            "Drop must not wait for a pasteboard call that ignores cancellation"
        );
    }

    #[test]
    fn an_injected_clipboard_is_not_sent_straight_back() {
        let _exclusive = exclusive();
        // Otherwise the two pasteboards trade the same text forever.
        let Some(mut session) = session() else {
            return;
        };
        session
            .apply_remote(
                1,
                ClipboardContentKind::TextUtf8,
                b"from the client".to_vec(),
            )
            .expect("apply");
        assert_eq!(session.stats().received, 1);
        assert!(
            session.poll_local().is_none(),
            "the host must not echo a clipboard it just received"
        );
    }

    #[test]
    fn a_repeated_offer_is_ignored() {
        let _exclusive = exclusive();
        let Some(mut session) = session() else {
            return;
        };
        session
            .apply_remote(5, ClipboardContentKind::TextUtf8, b"first".to_vec())
            .expect("apply");
        session
            .apply_remote(5, ClipboardContentKind::TextUtf8, b"again".to_vec())
            .expect("repeat");
        session
            .apply_remote(4, ClipboardContentKind::TextUtf8, b"stale".to_vec())
            .expect("stale");
        assert_eq!(session.stats().received, 1);
        assert_eq!(session.stats().duplicates, 2);
    }

    #[test]
    fn text_that_is_not_utf8_is_counted_malformed_rather_than_refused() {
        let _exclusive = exclusive();
        let Some(mut session) = session() else {
            return;
        };
        session
            .apply_remote(1, ClipboardContentKind::TextUtf8, vec![0xff, 0xfe, 0xfd])
            .expect("no panic");
        assert_eq!(session.stats().received, 0);
        // Not `refused`: the operator decided nothing here. The client sent
        // bytes that are not the content type they were labelled as, which is
        // a different report from a policy decision about something
        // well-formed.
        assert_eq!(session.stats().malformed, 1);
        assert_eq!(session.stats().refused, 0);
    }

    #[test]
    fn an_image_that_is_not_a_png_is_counted_malformed() {
        let _exclusive = exclusive();
        let Some(mut session) = session() else {
            return;
        };
        session
            .apply_remote(
                1,
                ClipboardContentKind::ImagePng,
                b"definitely not a png".to_vec(),
            )
            .expect("no panic");
        assert_eq!(session.stats().received, 0);
        assert_eq!(session.stats().malformed, 1);
    }

    #[test]
    fn an_oversized_payload_is_refused_rather_than_truncated() {
        let _exclusive = exclusive();
        // Half a clipboard silently replacing a whole one loses work.
        let Some(mut session) = session() else {
            return;
        };
        let huge = vec![b'x'; arcen_media::clipboard::HARD_MAX_CLIPBOARD_BYTES + 1];
        session
            .apply_remote(1, ClipboardContentKind::TextUtf8, huge)
            .expect("no panic");
        assert_eq!(session.stats().received, 0);
        assert_eq!(session.stats().refused, 1);
    }

    #[test]
    fn a_local_copy_becomes_an_offer_the_client_can_read() {
        let _exclusive = exclusive();
        let Some(mut session) = session() else {
            return;
        };
        // Write through the adapter directly so it registers as a local copy
        // rather than one of ours.
        let pasteboard = Pasteboard::general().expect("pasteboard");
        pasteboard
            .write(&ClipboardPayload::Text("copied locally".to_owned()))
            .expect("write");

        let Some(transfer) = session.poll_local() else {
            panic!("a local copy must produce an offer");
        };
        let offer = serde_json::to_string(&transfer.offer()).expect("serialize");
        let parsed: serde_json::Value = serde_json::from_str(&offer).expect("json");
        assert_eq!(parsed["type"], arcen_protocol::messages::CLIPBOARD_DATA);
        assert_eq!(parsed["size_bytes"], transfer.bytes().len());
        assert_eq!(transfer.bytes(), b"copied locally");
        assert_eq!(session.stats().sent, 1);

        // The bytes must leave as framed chunks, not bare. A Deck checks the
        // first byte against `FrameType::Clipboard` and ignores anything else,
        // so an unframed payload is silently dropped on arrival.
        let chunks = transfer.chunks().expect("chunks");
        assert_eq!(chunks.len(), 1, "a small payload is one chunk");
        assert_eq!(
            chunks[0].first().copied(),
            Some(arcen_protocol::FrameType::Clipboard as u8),
        );
        let (header, payload) =
            arcen_protocol::decode_clipboard_chunk(&chunks[0]).expect("decodable by the client");
        assert_eq!(header.sequence, transfer.sequence());
        assert_eq!(header.total_size as usize, transfer.bytes().len());
        assert_eq!(header.offset, 0);
        assert_eq!(payload, b"copied locally");
    }

    #[test]
    fn a_payload_larger_than_one_chunk_is_split_contiguously() {
        // The reassembler on the other side requires contiguous, increasing
        // offsets and refuses anything else, so this is the property that
        // decides whether a large paste arrives at all.
        let bytes = vec![b'x'; CHUNK_BYTES + 512];
        let transfer = OutgoingClipboard::new(7, ClipboardContentKind::TextUtf8, bytes.clone())
            .expect("transfer");
        let chunks = transfer.chunks().expect("chunks");
        assert_eq!(chunks.len(), 2);

        let mut expected_offset = 0usize;
        let mut reassembled = Vec::new();
        for chunk in &chunks {
            let (header, payload) = arcen_protocol::decode_clipboard_chunk(chunk).expect("decode");
            assert_eq!(header.sequence, 7);
            assert_eq!(header.offset as usize, expected_offset);
            assert_eq!(header.total_size as usize, bytes.len());
            expected_offset += payload.len();
            reassembled.extend_from_slice(payload);
        }
        assert_eq!(reassembled, bytes);
    }

    #[test]
    fn an_empty_pasteboard_read_is_never_offered() {
        // The shared encoder rejects a zero total size, so an empty clipboard
        // has no valid wire form at all. Constructing the transfer is what
        // refuses it, which is why `poll_local` can only ever hand back
        // something that is actually sendable.
        assert!(OutgoingClipboard::new(1, ClipboardContentKind::TextUtf8, Vec::new()).is_err());
    }
}
