//! A compositor-neutral bridge between one desktop selection and the client.
//!
//! Wayland compositors do not let a client watch or own the clipboard the way
//! X11 does. They offer a smaller set of actions:
//!
//! - owner changes, with the names (MIME types) the new owner offers;
//! - a read of one name;
//! - taking ownership with a list of names;
//! - transfer requests, where a reader asks for one of our names.
//!
//! Mutter's `RemoteDesktop` clipboard is that model, and so are
//! `ext-data-control` and the desktop portal. This state machine makes every
//! decision those actions need: what to read, what to ignore, what to offer,
//! what to serve, what to count. A platform adapter only performs the I/O it
//! is told to and reports back. It holds no clock and does no I/O, so the
//! rules are tested here and are the same for every compositor.
//!
//! The rules match the dedicated-Xorg child:
//!
//! - Only changes after the bridge started are carried. The owner that existed
//!   when the client attached is a baseline and is not sent.
//! - Our own ownership is never read back.
//! - The newest change wins and cancels an older read.
//! - A payload above the policy maximum is refused and counted, never
//!   truncated, and never ends the bridge.
//! - Sequences are nonzero and increase within one bridge.
//! - Payload bytes are scrubbed when they are dropped.

use std::collections::HashSet;
use std::fmt::{Debug, Formatter};
use std::ops::Deref;
use std::sync::Arc;
use std::time::Duration;

use zeroize::Zeroize;

use super::formats::{
    ClipboardPayloadError, HostFormat, choose_host_format, decode_host_payload, offered_names,
    render_for_host, validate_payload,
};
use super::{ClipboardFlow, ClipboardKind, ClipboardNegotiation};

/// How long a selection read or write may make no progress before the
/// adapter abandons it. This is the same bound as X11 INCR and the wire
/// reassembly.
pub const SELECTION_PROGRESS_TIMEOUT: Duration = Duration::from_secs(5);

/// The most transfer requests a bridge serves at once. More are refused
/// rather than queued: each one holds a copy of the payload in flight.
pub const MAX_CONCURRENT_HOST_WRITES: usize = 2;

/// Bytes that are zeroed when the last reference drops.
#[derive(Clone, PartialEq, Eq)]
pub struct ScrubbedBytes(Vec<u8>);

impl ScrubbedBytes {
    /// Takes ownership of `bytes`.
    #[must_use]
    pub const fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }
}

impl Deref for ScrubbedBytes {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.0
    }
}

impl Debug for ScrubbedBytes {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "ScrubbedBytes({} bytes)", self.0.len())
    }
}

impl Drop for ScrubbedBytes {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

/// What the adapter must do after an owner change.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OwnerDecision {
    /// A read in flight that must be abandoned first (close its descriptor).
    /// The compositor serves one read at a time.
    pub cancel: Option<u64>,
    /// A read to start: its token and the name exactly as the owner offered it.
    pub read: Option<(u64, String)>,
}

/// How a read ended.
#[derive(Debug, PartialEq, Eq)]
pub enum ReadOutcome {
    /// The owner wrote everything, within the read limit.
    Complete(Vec<u8>),
    /// The owner wrote more than [`SelectionBridge::read_limit`].
    Overflow,
    /// No progress within [`SELECTION_PROGRESS_TIMEOUT`].
    TimedOut,
    /// The compositor refused the read or the descriptor failed.
    Failed,
}

/// A host clipboard item ready for the client.
pub struct HostItem {
    /// Nonzero, increasing within one bridge.
    pub sequence: u64,
    /// Wire kind.
    pub kind: ClipboardKind,
    bytes: Vec<u8>,
}

impl HostItem {
    /// Takes the payload. The caller now owns scrubbing it.
    #[must_use]
    pub fn take_bytes(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.bytes)
    }

    /// Payload length.
    #[must_use]
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Whether the payload is empty (only after [`Self::take_bytes`]).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}

impl Debug for HostItem {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HostItem")
            .field("sequence", &self.sequence)
            .field("kind", &self.kind)
            .field("bytes", &self.bytes.len())
            .finish()
    }
}

impl Drop for HostItem {
    fn drop(&mut self) {
        self.bytes.zeroize();
    }
}

/// What to answer a transfer request with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferDecision {
    /// Write these bytes to the requestor, then report
    /// [`SelectionBridge::transfer_finished`].
    Write {
        /// The compositor's request serial.
        serial: u32,
        /// The rendered payload.
        bytes: Arc<ScrubbedBytes>,
    },
    /// Answer the request as failed.
    Refuse {
        /// The compositor's request serial.
        serial: u32,
        /// Stable reason token.
        reason: &'static str,
    },
}

/// What a bridge did. Counts only; never content.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BridgeStats {
    /// Host items handed to the client.
    pub sent: u64,
    /// Client items the desktop now owns.
    pub received: u64,
    /// Items the policy or negotiation refused (excluding size).
    pub refused: u64,
    /// Items above the policy maximum.
    pub too_large: u64,
    /// Items that were not what their label said.
    pub malformed: u64,
    /// Owner changes with nothing Arcen carries.
    pub unsupported: u64,
    /// Reads abandoned for lack of progress.
    pub timed_out: u64,
    /// Reads the compositor refused or that failed.
    pub failed: u64,
    /// Owner changes that were our own ownership.
    pub own: u64,
    /// The owner announced when the bridge started.
    pub baseline: u64,
    /// Transfer requests served.
    pub served: u64,
    /// Transfer requests refused.
    pub refused_transfers: u64,
}

struct RemoteSelection {
    kind: ClipboardKind,
    bytes: Arc<ScrubbedBytes>,
}

struct ReadInFlight {
    token: u64,
    format: HostFormat,
}

/// One client's view of one desktop selection.
pub struct SelectionBridge {
    negotiation: ClipboardNegotiation,
    next_sequence: Option<u64>,
    next_token: u64,
    read: Option<ReadInFlight>,
    remote: Option<RemoteSelection>,
    writes: HashSet<u32>,
    stats: BridgeStats,
}

impl Debug for SelectionBridge {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SelectionBridge")
            .field("read", &self.read.as_ref().map(|read| read.token))
            .field("remote", &self.remote.as_ref().map(|remote| remote.kind))
            .field("writes", &self.writes.len())
            .field("stats", &self.stats)
            .finish_non_exhaustive()
    }
}

impl SelectionBridge {
    /// A bridge for one negotiated client.
    #[must_use]
    pub fn new(negotiation: ClipboardNegotiation) -> Self {
        Self {
            negotiation,
            next_sequence: Some(1),
            next_token: 1,
            read: None,
            remote: None,
            writes: HashSet::new(),
            stats: BridgeStats::default(),
        }
    }

    /// The most bytes a read may deliver. One byte more means
    /// [`ReadOutcome::Overflow`].
    #[must_use]
    pub const fn read_limit(&self) -> usize {
        self.negotiation.policy().max_bytes
    }

    /// The read in flight, if any.
    #[must_use]
    pub fn read_in_flight(&self) -> Option<u64> {
        self.read.as_ref().map(|read| read.token)
    }

    /// Counters so far.
    #[must_use]
    pub const fn stats(&self) -> BridgeStats {
        self.stats
    }

    /// The selection changed owner. `ours` is the compositor saying the new
    /// owner is this bridge's own ownership. `baseline` marks the owner the
    /// compositor announced when the bridge was enabled.
    pub fn owner_changed<S: AsRef<str>>(
        &mut self,
        names: &[S],
        ours: bool,
        baseline: bool,
    ) -> OwnerDecision {
        let cancel = self.read.take().map(|read| read.token);
        if ours {
            self.stats.own += 1;
            return OwnerDecision { cancel, read: None };
        }
        // Someone else owns the selection now: what we held is no longer
        // what a paste on the desktop gets.
        self.remote = None;
        if baseline {
            self.stats.baseline += 1;
            return OwnerDecision { cancel, read: None };
        }
        let negotiation = self.negotiation;
        let chosen = choose_host_format(
            |kind| negotiation.allows(ClipboardFlow::HostToClient, kind),
            names.iter().map(AsRef::as_ref),
        );
        let Some((name, format)) = chosen else {
            self.stats.unsupported += 1;
            return OwnerDecision { cancel, read: None };
        };
        let token = self.next_token;
        self.next_token = self.next_token.wrapping_add(1).max(1);
        self.read = Some(ReadInFlight { token, format });
        OwnerDecision {
            cancel,
            read: Some((token, name.to_owned())),
        }
    }

    /// A read ended. Returns the item for the client, or `None` when it was
    /// stale, refused, malformed, too large or failed (counted).
    pub fn read_finished(&mut self, token: u64, outcome: ReadOutcome) -> Option<HostItem> {
        let Some(read) = self.read.take_if(|read| read.token == token) else {
            if let ReadOutcome::Complete(mut bytes) = outcome {
                bytes.zeroize();
            }
            return None;
        };
        let bytes = match outcome {
            ReadOutcome::Complete(bytes) => bytes,
            ReadOutcome::Overflow => {
                self.stats.too_large += 1;
                return None;
            }
            ReadOutcome::TimedOut => {
                self.stats.timed_out += 1;
                return None;
            }
            ReadOutcome::Failed => {
                self.stats.failed += 1;
                return None;
            }
        };
        match decode_host_payload(self.negotiation.policy(), read.format, bytes) {
            Ok((kind, bytes)) => {
                if !self.negotiation.allows(ClipboardFlow::HostToClient, kind) {
                    let mut bytes = bytes;
                    bytes.zeroize();
                    self.stats.refused += 1;
                    return None;
                }
                let sequence = self.next_sequence?;
                self.next_sequence = sequence.checked_add(1);
                self.stats.sent += 1;
                Some(HostItem {
                    sequence,
                    kind,
                    bytes,
                })
            }
            Err(error) => {
                self.count(error);
                None
            }
        }
    }

    /// The client sent an item. Returns the names to take ownership with, or
    /// why the item does not cross (counted). The bytes are kept, scrubbed,
    /// until another owner takes the selection or the bridge is cleared.
    ///
    /// # Errors
    ///
    /// [`ClipboardPayloadError`] when the negotiation, policy or content
    /// refuses the item.
    pub fn remote_item(
        &mut self,
        kind: ClipboardKind,
        bytes: Vec<u8>,
    ) -> Result<Vec<&'static str>, ClipboardPayloadError> {
        let bytes = ScrubbedBytes::new(bytes);
        let checked = if self.negotiation.allows(ClipboardFlow::ClientToHost, kind) {
            validate_payload(
                self.negotiation.policy(),
                ClipboardFlow::ClientToHost,
                kind,
                &bytes,
            )
        } else {
            Err(ClipboardPayloadError::Refused(
                super::ClipboardPolicyError::ContentNotAllowed,
            ))
        };
        if let Err(error) = checked {
            self.count(error);
            return Err(error);
        }
        let names = offered_names(kind, &bytes);
        self.remote = Some(RemoteSelection {
            kind,
            bytes: Arc::new(bytes),
        });
        self.stats.received += 1;
        Ok(names)
    }

    /// A reader asked for one of our names.
    pub fn transfer_requested(&mut self, name: &str, serial: u32) -> TransferDecision {
        let decision = match self.remote.as_ref() {
            None => TransferDecision::Refuse {
                serial,
                reason: "not_owner",
            },
            Some(_) if self.writes.len() >= MAX_CONCURRENT_HOST_WRITES => {
                TransferDecision::Refuse {
                    serial,
                    reason: "busy",
                }
            }
            Some(remote) => match render_for_host(name, remote.kind, &remote.bytes) {
                Some(std::borrow::Cow::Borrowed(_)) => TransferDecision::Write {
                    serial,
                    bytes: Arc::clone(&remote.bytes),
                },
                Some(std::borrow::Cow::Owned(rendered)) => TransferDecision::Write {
                    serial,
                    bytes: Arc::new(ScrubbedBytes::new(rendered)),
                },
                None => TransferDecision::Refuse {
                    serial,
                    reason: "unsupported_name",
                },
            },
        };
        match decision {
            TransferDecision::Write { serial, .. } => {
                self.writes.insert(serial);
            }
            TransferDecision::Refuse { .. } => self.stats.refused_transfers += 1,
        }
        decision
    }

    /// A write the bridge asked for ended.
    pub fn transfer_finished(&mut self, serial: u32, ok: bool) {
        if self.writes.remove(&serial) {
            if ok {
                self.stats.served += 1;
            } else {
                self.stats.refused_transfers += 1;
            }
        }
    }

    /// The client went away or the clipboard was disabled. Drops what the
    /// bridge held and returns a read to abandon.
    pub fn clear(&mut self) -> Option<u64> {
        self.remote = None;
        self.writes.clear();
        self.read.take().map(|read| read.token)
    }

    const fn count(&mut self, error: ClipboardPayloadError) {
        match error {
            ClipboardPayloadError::Refused(super::ClipboardPolicyError::TooLarge { .. }) => {
                self.stats.too_large += 1;
            }
            ClipboardPayloadError::Refused(_) => self.stats.refused += 1,
            ClipboardPayloadError::Malformed | ClipboardPayloadError::Empty => {
                self.stats.malformed += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clipboard::formats::{IMAGE_PNG, TEXT_PLAIN_UTF8, UTF8_STRING, X11_STRING};
    use crate::clipboard::{
        ClipboardContent, ClipboardDirection, ClipboardDirections, ClipboardPolicy,
        ClipboardRequest,
    };

    fn bridge_with(policy: ClipboardPolicy, request: ClipboardRequest) -> SelectionBridge {
        SelectionBridge::new(ClipboardNegotiation::resolve(policy, true, request).unwrap())
    }

    fn everything() -> ClipboardRequest {
        ClipboardRequest {
            protocol_version: arcen_protocol::messages::CLIPBOARD_PROTOCOL_VERSION,
            text: ClipboardDirections::both(),
            image: ClipboardDirections::both(),
        }
    }

    fn bridge() -> SelectionBridge {
        bridge_with(
            ClipboardPolicy::new(ClipboardDirection::Both, ClipboardContent::All, 16).unwrap(),
            everything(),
        )
    }

    #[test]
    fn the_owner_present_at_attach_is_not_sent() {
        let mut bridge = bridge();
        let decision = bridge.owner_changed(&[TEXT_PLAIN_UTF8], false, true);
        assert_eq!(decision, OwnerDecision::default());
        assert_eq!(bridge.stats().baseline, 1);
    }

    #[test]
    fn a_host_copy_is_read_once_and_sent_with_increasing_sequences() {
        let mut bridge = bridge();
        let decision = bridge.owner_changed(&["text/html", TEXT_PLAIN_UTF8], false, false);
        let (token, name) = decision.read.unwrap();
        assert_eq!(name, TEXT_PLAIN_UTF8);
        let item = bridge
            .read_finished(token, ReadOutcome::Complete(b"hi".to_vec()))
            .unwrap();
        assert_eq!(
            (item.sequence, item.kind, item.len()),
            (1, ClipboardKind::TextUtf8, 2)
        );
        let (token, _) = bridge
            .owner_changed(&[UTF8_STRING], false, false)
            .read
            .unwrap();
        let item = bridge
            .read_finished(token, ReadOutcome::Complete(b"again".to_vec()))
            .unwrap();
        assert_eq!(item.sequence, 2);
        assert_eq!(bridge.stats().sent, 2);
    }

    #[test]
    fn our_own_ownership_is_never_read_back() {
        let mut bridge = bridge();
        let names = bridge
            .remote_item(ClipboardKind::TextUtf8, b"deck".to_vec())
            .unwrap();
        assert!(names.contains(&TEXT_PLAIN_UTF8));
        assert_eq!(
            bridge.owner_changed(&names, true, false),
            OwnerDecision::default()
        );
        // Still ours: a paste is served.
        assert!(matches!(
            bridge.transfer_requested(TEXT_PLAIN_UTF8, 7),
            TransferDecision::Write { serial: 7, .. }
        ));
    }

    #[test]
    fn the_newest_change_cancels_an_older_read() {
        let mut bridge = bridge();
        let (first, _) = bridge
            .owner_changed(&[TEXT_PLAIN_UTF8], false, false)
            .read
            .unwrap();
        let decision = bridge.owner_changed(&[IMAGE_PNG, UTF8_STRING], false, false);
        assert_eq!(decision.cancel, Some(first));
        let (second, name) = decision.read.unwrap();
        assert_eq!(name, UTF8_STRING);
        // The cancelled read's late result is dropped.
        assert!(
            bridge
                .read_finished(first, ReadOutcome::Complete(b"old".to_vec()))
                .is_none()
        );
        assert!(
            bridge
                .read_finished(second, ReadOutcome::Complete(b"new".to_vec()))
                .is_some()
        );
    }

    #[test]
    fn an_oversized_host_copy_is_refused_and_the_bridge_goes_on() {
        let mut bridge = bridge();
        let (token, _) = bridge
            .owner_changed(&[TEXT_PLAIN_UTF8], false, false)
            .read
            .unwrap();
        assert!(bridge.read_finished(token, ReadOutcome::Overflow).is_none());
        let (token, _) = bridge
            .owner_changed(&[TEXT_PLAIN_UTF8], false, false)
            .read
            .unwrap();
        assert!(
            bridge
                .read_finished(token, ReadOutcome::Complete(vec![b'a'; 17]))
                .is_none()
        );
        assert_eq!(bridge.stats().too_large, 2);
        let (token, _) = bridge
            .owner_changed(&[TEXT_PLAIN_UTF8], false, false)
            .read
            .unwrap();
        assert!(
            bridge
                .read_finished(token, ReadOutcome::Complete(b"fits".to_vec()))
                .is_some()
        );
        assert_eq!(bridge.read_limit(), 16);
    }

    #[test]
    fn a_policy_that_keeps_text_on_the_host_reads_nothing() {
        let mut bridge = bridge_with(
            ClipboardPolicy::new(ClipboardDirection::ClientToHost, ClipboardContent::All, 16)
                .unwrap(),
            everything(),
        );
        assert_eq!(
            bridge.owner_changed(&[TEXT_PLAIN_UTF8], false, false),
            OwnerDecision::default()
        );
        assert_eq!(bridge.stats().unsupported, 1);
    }

    #[test]
    fn a_client_item_is_checked_before_the_desktop_owns_it() {
        let mut bridge = bridge_with(
            ClipboardPolicy::new(ClipboardDirection::Both, ClipboardContent::Text, 16).unwrap(),
            everything(),
        );
        assert!(
            bridge
                .remote_item(ClipboardKind::ImagePng, b"png?".to_vec())
                .is_err()
        );
        assert!(
            bridge
                .remote_item(ClipboardKind::TextUtf8, vec![0xff])
                .is_err()
        );
        assert!(
            bridge
                .remote_item(ClipboardKind::TextUtf8, vec![b'a'; 17])
                .is_err()
        );
        let stats = bridge.stats();
        assert_eq!((stats.refused, stats.malformed, stats.too_large), (1, 1, 1));
        assert!(matches!(
            bridge.transfer_requested(TEXT_PLAIN_UTF8, 1),
            TransferDecision::Refuse {
                reason: "not_owner",
                ..
            }
        ));
    }

    #[test]
    fn transfers_render_latin1_bound_concurrency_and_end_with_a_new_owner() {
        let mut bridge = bridge();
        bridge
            .remote_item(ClipboardKind::TextUtf8, "café".as_bytes().to_vec())
            .unwrap();
        let TransferDecision::Write { bytes, .. } = bridge.transfer_requested(X11_STRING, 1) else {
            panic!("latin-1 served");
        };
        assert_eq!(&bytes[..], b"caf\xe9");
        assert!(matches!(
            bridge.transfer_requested(TEXT_PLAIN_UTF8, 2),
            TransferDecision::Write { .. }
        ));
        assert!(matches!(
            bridge.transfer_requested(TEXT_PLAIN_UTF8, 3),
            TransferDecision::Refuse { reason: "busy", .. }
        ));
        bridge.transfer_finished(1, true);
        assert!(matches!(
            bridge.transfer_requested(IMAGE_PNG, 4),
            TransferDecision::Refuse {
                reason: "unsupported_name",
                ..
            }
        ));
        let _ = bridge.owner_changed(&["text/html"], false, false);
        bridge.transfer_finished(2, true);
        assert!(matches!(
            bridge.transfer_requested(TEXT_PLAIN_UTF8, 5),
            TransferDecision::Refuse {
                reason: "not_owner",
                ..
            }
        ));
        assert_eq!(bridge.stats().served, 2);
    }

    #[test]
    fn clearing_drops_what_was_held_and_names_the_read_to_abandon() {
        let mut bridge = bridge();
        bridge
            .remote_item(ClipboardKind::TextUtf8, b"x".to_vec())
            .unwrap();
        let (token, _) = bridge
            .owner_changed(&[TEXT_PLAIN_UTF8], false, false)
            .read
            .unwrap();
        assert_eq!(bridge.clear(), Some(token));
        assert!(bridge.read_in_flight().is_none());
        assert!(matches!(
            bridge.transfer_requested(TEXT_PLAIN_UTF8, 1),
            TransferDecision::Refuse { .. }
        ));
    }

    #[test]
    fn timeouts_and_failures_are_counted_apart() {
        let mut bridge = bridge();
        let (token, _) = bridge
            .owner_changed(&[TEXT_PLAIN_UTF8], false, false)
            .read
            .unwrap();
        assert!(bridge.read_finished(token, ReadOutcome::TimedOut).is_none());
        let (token, _) = bridge
            .owner_changed(&[TEXT_PLAIN_UTF8], false, false)
            .read
            .unwrap();
        assert!(bridge.read_finished(token, ReadOutcome::Failed).is_none());
        let stats = bridge.stats();
        assert_eq!((stats.timed_out, stats.failed), (1, 1));
    }
}
