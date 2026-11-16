// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The Deck's clipboard controller, behind [`PlatformClipboard`].
//!
//! Every Deck's clipboard controller (the macOS Deck's former own copy,
//! with the pasteboard calls replaced by the seam): one controller per
//! negotiated transport generation, a fresh echo token per generation, at
//! most one poll every 250 ms, the latest host item validated (policy size,
//! UTF-8, PNG limits) and written with Arcen's origin marker, and local
//! changes sent unless they carry this generation's marker. The controller
//! runs on the clipboard's own thread (macOS main thread, the Windows
//! clipboard window's thread).

use std::time::{Duration, Instant};

use arcen_media::clipboard::{
    ClipboardDirection, ClipboardFlow, ClipboardKind, ClipboardPolicy, EchoMarker, EchoSuppressor,
    EchoToken, ImageLimits, validate_png,
};
use arcen_protocol::messages::ClipboardContentKind;

use crate::DeckServices;
use crate::clipboard::PlatformClipboard;
use crate::runtime::clipboard_session::{ClipboardItem, ClipboardSession, media_policy};

/// How often the local clipboard is polled for changes.
pub const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// How many polls one change is looked at again while another process
/// holds the clipboard (about two seconds) before it is given up.
pub const BLOCKED_READ_RETRIES: u8 = 8;

const fn media_kind(kind: ClipboardContentKind) -> ClipboardKind {
    match kind {
        ClipboardContentKind::TextUtf8 => ClipboardKind::TextUtf8,
        ClipboardContentKind::ImagePng => ClipboardKind::ImagePng,
    }
}

/// What a local clipboard change offers the Pier.
struct Observation {
    marker: Option<EchoMarker>,
    kind: ClipboardContentKind,
    bytes: Vec<u8>,
    truncated: bool,
}

/// Clipboard-thread controller that polls only for an enabled, negotiated
/// transport generation.
pub struct ClipboardController {
    generation: Option<u64>,
    session: Option<ClipboardSession>,
    next_sequence: u64,
    last_poll: Option<Instant>,
    suppressor: Option<EchoSuppressor>,
    clipboard: Option<Box<dyn PlatformClipboard>>,
    last_token: u64,
    blocked_retries: u8,
}

impl std::fmt::Debug for ClipboardController {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ClipboardController")
            .field("generation", &self.generation)
            .field("next_sequence", &self.next_sequence)
            .field("open", &self.clipboard.is_some())
            .finish_non_exhaustive()
    }
}

impl Default for ClipboardController {
    fn default() -> Self {
        Self::new()
    }
}

impl ClipboardController {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            generation: None,
            session: None,
            next_sequence: 1,
            last_poll: None,
            suppressor: None,
            clipboard: None,
            last_token: 0,
            blocked_retries: 0,
        }
    }

    /// Binds the controller to `session` for transport `generation`. A new
    /// generation starts with a fresh echo token and sequence numbers.
    pub fn attach(&mut self, generation: u64, session: ClipboardSession) {
        if self.generation == Some(generation) {
            self.session = Some(session);
            return;
        }
        self.detach();
        let mut token = [0u8; 16];
        if getrandom::getrandom(&mut token).is_err() {
            return;
        }
        self.generation = Some(generation);
        self.session = Some(session);
        self.suppressor = Some(EchoSuppressor::new(EchoToken(token)));
    }

    /// Unbinds the controller and drops the platform clipboard.
    pub fn detach(&mut self) {
        if let Some(session) = self.session.take() {
            session.clear();
        }
        self.generation = None;
        self.next_sequence = 1;
        self.last_poll = None;
        self.suppressor = None;
        self.clipboard = None;
    }

    /// Applies the latest inbound item and polls at most once every
    /// [`POLL_INTERVAL`]. Call on the clipboard's thread; the platform
    /// clipboard is opened through `services` on first use.
    pub fn sync(&mut self, now: Instant, services: &dyn DeckServices) {
        self.sync_with(now, || services.open_clipboard());
    }

    /// [`Self::sync`] with an explicit opener (tests, tools).
    pub fn sync_with(
        &mut self,
        now: Instant,
        open: impl FnOnce() -> Option<Box<dyn PlatformClipboard>>,
    ) {
        let Some(session) = self.session.clone() else {
            return;
        };
        let Some(wire_policy) = session.policy() else {
            self.clipboard = None;
            return;
        };
        let Some(policy) = media_policy(wire_policy) else {
            return;
        };
        if matches!(policy.direction, ClipboardDirection::Disabled) {
            return;
        }
        if self.clipboard.is_none() {
            let Some(mut clipboard) = open() else {
                return;
            };
            // What was on the clipboard before the session is not sent.
            self.last_token = clipboard.change_token();
            self.clipboard = Some(clipboard);
        }
        self.apply_inbound(&session, policy);
        if self
            .last_poll
            .is_some_and(|last| now.saturating_duration_since(last) < POLL_INTERVAL)
        {
            return;
        }
        self.last_poll = Some(now);
        self.poll_outbound(&session, policy);
    }

    fn apply_inbound(&mut self, session: &ClipboardSession, policy: ClipboardPolicy) {
        let Some(mut item) = session.take_inbound() else {
            return;
        };
        if policy
            .check_size(
                ClipboardFlow::HostToClient,
                media_kind(item.kind),
                item.bytes.len(),
            )
            .is_err()
        {
            item.scrub();
            return;
        }
        let valid = match item.kind {
            ClipboardContentKind::TextUtf8 => std::str::from_utf8(&item.bytes).is_ok(),
            ClipboardContentKind::ImagePng => validate_png(
                &item.bytes,
                ImageLimits {
                    max_encoded_bytes: policy.max_bytes,
                    ..ImageLimits::default()
                },
            )
            .is_ok(),
        };
        if !valid {
            item.scrub();
            return;
        }
        let Some(marker) = self
            .suppressor
            .as_mut()
            .and_then(|suppressor| suppressor.mark_injected(item.sequence))
        else {
            item.scrub();
            return;
        };
        if let Some(clipboard) = self.clipboard.as_mut()
            && clipboard.write(&marker.encode(), item.kind, &item.bytes)
        {
            // Our own write is not a local change to send back.
            self.last_token = clipboard.change_token();
        }
        item.scrub();
    }

    fn poll_outbound(&mut self, session: &ClipboardSession, policy: ClipboardPolicy) {
        if !policy.allows(ClipboardFlow::ClientToHost, ClipboardKind::TextUtf8)
            && !policy.allows(ClipboardFlow::ClientToHost, ClipboardKind::ImagePng)
        {
            return;
        }
        let Some(observation) = self.observe(policy) else {
            return;
        };
        if observation.marker.is_some_and(|marker| {
            self.suppressor
                .is_some_and(|suppressor| suppressor.should_suppress(marker))
        }) {
            return;
        }
        let sequence = self.next_sequence;
        let Some(next) = sequence.checked_add(1) else {
            self.detach();
            return;
        };
        self.next_sequence = next;
        if let Some(item) = ClipboardItem::new(
            sequence,
            observation.kind,
            observation.bytes,
            observation.truncated,
        ) {
            let _ = session.queue_outbound(item);
        }
    }

    /// The local clipboard's contents when they changed since the last
    /// look: text first, then an image (PNG, or the platform's native image
    /// converted), as the macOS Deck reads its pasteboard.
    fn observe(&mut self, policy: ClipboardPolicy) -> Option<Observation> {
        let clipboard = self.clipboard.as_mut()?;
        let token = clipboard.change_token();
        if token == self.last_token {
            return None;
        }
        let previous = std::mem::replace(&mut self.last_token, token);
        let _ = clipboard.take_read_blocked();
        let observation = Self::read_change(clipboard.as_mut(), policy);
        if clipboard.take_read_blocked() {
            // Never send a half-read change (a missing marker would echo):
            // look again at the next poll, for a while.
            if self.blocked_retries < BLOCKED_READ_RETRIES {
                self.blocked_retries += 1;
                self.last_token = previous;
            } else {
                self.blocked_retries = 0;
            }
            return None;
        }
        self.blocked_retries = 0;
        observation
    }

    fn read_change(
        clipboard: &mut dyn PlatformClipboard,
        policy: ClipboardPolicy,
    ) -> Option<Observation> {
        let marker = clipboard
            .read_origin_marker()
            .and_then(|bytes| EchoMarker::decode(&bytes));
        if policy.allows(ClipboardFlow::ClientToHost, ClipboardKind::TextUtf8)
            && let Some(text) = clipboard.read_text()
        {
            let prepared = policy.prepare_text(ClipboardFlow::ClientToHost, &text);
            if !prepared.text.is_empty() {
                return Some(Observation {
                    marker,
                    kind: ClipboardContentKind::TextUtf8,
                    bytes: prepared.text.as_bytes().to_vec(),
                    truncated: prepared.truncated,
                });
            }
        }
        if !policy.allows(ClipboardFlow::ClientToHost, ClipboardKind::ImagePng) {
            return None;
        }
        let bytes = clipboard.read_png(policy.max_bytes)?;
        if bytes.is_empty() || bytes.len() > policy.max_bytes {
            return None;
        }
        validate_png(
            &bytes,
            ImageLimits {
                max_encoded_bytes: policy.max_bytes,
                ..ImageLimits::default()
            },
        )
        .ok()?;
        Some(Observation {
            marker,
            kind: ClipboardContentKind::ImagePng,
            bytes,
            truncated: false,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use std::cell::RefCell;
    use std::rc::Rc;

    use arcen_protocol::messages::{
        CLIPBOARD_PROTOCOL_VERSION, ClipboardContentMsg, ClipboardDirectionMsg, ClipboardPolicyMsg,
    };
    use zeroize::Zeroizing;

    use super::*;

    #[derive(Default)]
    struct FakeState {
        token: u64,
        marker: Option<Vec<u8>>,
        text: Option<String>,
        png: Option<Vec<u8>>,
        writes: Vec<(ClipboardContentKind, Vec<u8>)>,
        blocked_reads: u32,
        read_blocked: bool,
    }

    impl FakeState {
        fn copy_text(&mut self, text: &str) {
            self.token += 1;
            self.marker = None;
            self.text = Some(text.to_owned());
            self.png = None;
        }
    }

    #[derive(Clone, Default)]
    struct Fake(Rc<RefCell<FakeState>>);

    impl PlatformClipboard for Fake {
        fn change_token(&mut self) -> u64 {
            self.0.borrow().token
        }

        fn read_origin_marker(&mut self) -> Option<Vec<u8>> {
            self.0.borrow().marker.clone()
        }

        fn read_text(&mut self) -> Option<Zeroizing<String>> {
            let mut state = self.0.borrow_mut();
            if state.blocked_reads > 0 {
                state.blocked_reads -= 1;
                state.read_blocked = true;
                return None;
            }
            state.text.clone().map(Zeroizing::new)
        }

        fn take_read_blocked(&mut self) -> bool {
            std::mem::take(&mut self.0.borrow_mut().read_blocked)
        }

        fn read_png(&mut self, max_encoded_bytes: usize) -> Option<Vec<u8>> {
            self.0
                .borrow()
                .png
                .clone()
                .filter(|png| png.len() <= max_encoded_bytes)
        }

        fn write(
            &mut self,
            origin_marker: &[u8],
            kind: ClipboardContentKind,
            bytes: &[u8],
        ) -> bool {
            let mut state = self.0.borrow_mut();
            state.token += 1;
            state.marker = Some(origin_marker.to_vec());
            state.text = (kind == ClipboardContentKind::TextUtf8)
                .then(|| String::from_utf8(bytes.to_vec()).expect("utf-8"));
            state.png = (kind == ClipboardContentKind::ImagePng).then(|| bytes.to_vec());
            state.writes.push((kind, bytes.to_vec()));
            true
        }
    }

    fn policy(direction: ClipboardDirectionMsg, max_bytes: u32) -> ClipboardPolicyMsg {
        ClipboardPolicyMsg {
            protocol_version: CLIPBOARD_PROTOCOL_VERSION,
            direction,
            content: ClipboardContentMsg::All,
            max_bytes,
        }
    }

    fn attached(
        fake: &Fake,
        direction: ClipboardDirectionMsg,
    ) -> (ClipboardController, ClipboardSession, Instant) {
        let session = ClipboardSession::new();
        session.set_policy(Some(policy(direction, 1024)));
        let mut controller = ClipboardController::new();
        controller.attach(1, session.clone());
        let start = Instant::now();
        let opener = fake.clone();
        controller.sync_with(start, move || Some(Box::new(opener)));
        (controller, session, start)
    }

    fn sync(controller: &mut ClipboardController, fake: &Fake, at: Instant) {
        let opener = fake.clone();
        controller.sync_with(at, move || Some(Box::new(opener)));
    }

    #[test]
    fn contents_from_before_the_session_are_not_sent() {
        let fake = Fake::default();
        fake.0.borrow_mut().copy_text("before");
        let (mut controller, session, start) = attached(&fake, ClipboardDirectionMsg::Both);
        sync(&mut controller, &fake, start + POLL_INTERVAL);
        assert!(session.take_outbound().is_none());
    }

    #[test]
    fn a_local_copy_is_sent_once_with_rising_sequences() {
        let fake = Fake::default();
        let (mut controller, session, start) = attached(&fake, ClipboardDirectionMsg::Both);
        fake.0.borrow_mut().copy_text("first");
        sync(&mut controller, &fake, start + POLL_INTERVAL);
        let first = session.take_outbound().expect("first");
        assert_eq!(
            (first.sequence, first.bytes.as_slice()),
            (1, b"first".as_slice())
        );
        sync(&mut controller, &fake, start + POLL_INTERVAL * 2);
        assert!(session.take_outbound().is_none(), "unchanged clipboard");
        fake.0.borrow_mut().copy_text("second");
        sync(&mut controller, &fake, start + POLL_INTERVAL * 3);
        assert_eq!(session.take_outbound().expect("second").sequence, 2);
    }

    #[test]
    fn polls_are_rate_limited() {
        let fake = Fake::default();
        let (mut controller, session, start) = attached(&fake, ClipboardDirectionMsg::Both);
        fake.0.borrow_mut().copy_text("soon");
        sync(&mut controller, &fake, start + Duration::from_millis(100));
        assert!(session.take_outbound().is_none());
        sync(&mut controller, &fake, start + POLL_INTERVAL);
        assert!(session.take_outbound().is_some());
    }

    #[test]
    fn host_items_are_written_with_the_marker_and_never_echoed() {
        let fake = Fake::default();
        let (mut controller, session, start) = attached(&fake, ClipboardDirectionMsg::Both);
        assert!(
            session.queue_inbound(
                ClipboardItem::new(
                    9,
                    ClipboardContentKind::TextUtf8,
                    b"from host".to_vec(),
                    false
                )
                .expect("item")
            )
        );
        sync(&mut controller, &fake, start + POLL_INTERVAL);
        assert_eq!(
            fake.0.borrow().writes,
            vec![(ClipboardContentKind::TextUtf8, b"from host".to_vec())]
        );
        let marker = EchoMarker::decode(fake.0.borrow().marker.as_deref().expect("marker"))
            .expect("decodes");
        assert_eq!(marker.sequence, 9);
        // Even a clipboard that reports our write as a change does not echo.
        fake.0.borrow_mut().token += 1;
        sync(&mut controller, &fake, start + POLL_INTERVAL * 2);
        assert!(session.take_outbound().is_none());
    }

    #[test]
    fn another_generations_marker_is_sent() {
        let fake = Fake::default();
        let (mut controller, session, start) = attached(&fake, ClipboardDirectionMsg::Both);
        let earlier = EchoMarker {
            token: EchoToken([1; 16]),
            sequence: 1,
        };
        {
            let mut clipboard = fake.0.borrow_mut();
            clipboard.copy_text("copied in an earlier session");
            clipboard.marker = Some(earlier.encode().to_vec());
        }
        sync(&mut controller, &fake, start + POLL_INTERVAL);
        assert!(session.take_outbound().is_some());
    }

    #[test]
    fn a_change_read_while_another_process_holds_the_clipboard_is_read_again() {
        let fake = Fake::default();
        let (mut controller, session, start) = attached(&fake, ClipboardDirectionMsg::Both);
        {
            let mut clipboard = fake.0.borrow_mut();
            clipboard.copy_text("busy");
            clipboard.blocked_reads = 2;
        }
        sync(&mut controller, &fake, start + POLL_INTERVAL);
        sync(&mut controller, &fake, start + POLL_INTERVAL * 2);
        assert!(session.take_outbound().is_none());
        sync(&mut controller, &fake, start + POLL_INTERVAL * 3);
        assert_eq!(session.take_outbound().expect("read again").bytes, b"busy");
    }

    #[test]
    fn a_clipboard_that_stays_held_is_given_up() {
        let fake = Fake::default();
        let (mut controller, session, start) = attached(&fake, ClipboardDirectionMsg::Both);
        {
            let mut clipboard = fake.0.borrow_mut();
            clipboard.copy_text("held");
            clipboard.blocked_reads = u32::MAX;
        }
        let polls = u32::from(BLOCKED_READ_RETRIES) + 3;
        for poll in 1..=polls {
            sync(&mut controller, &fake, start + POLL_INTERVAL * poll);
        }
        assert!(session.take_outbound().is_none());
        assert_eq!(controller.blocked_retries, 0, "the change was given up");
        fake.0.borrow_mut().blocked_reads = 0;
        sync(&mut controller, &fake, start + POLL_INTERVAL * (polls + 1));
        assert!(session.take_outbound().is_none(), "no new change");
    }

    #[test]
    fn invalid_or_oversize_host_items_are_dropped() {
        let fake = Fake::default();
        let (mut controller, session, start) = attached(&fake, ClipboardDirectionMsg::Both);
        assert!(
            session.queue_inbound(
                ClipboardItem::new(1, ClipboardContentKind::TextUtf8, vec![0xff, 0xfe], false)
                    .expect("item")
            )
        );
        sync(&mut controller, &fake, start + POLL_INTERVAL);
        assert!(
            session.queue_inbound(
                ClipboardItem::new(
                    2,
                    ClipboardContentKind::ImagePng,
                    b"not a png".to_vec(),
                    false
                )
                .expect("item")
            )
        );
        sync(&mut controller, &fake, start + POLL_INTERVAL * 2);
        assert!(
            session.queue_inbound(
                ClipboardItem::new(3, ClipboardContentKind::TextUtf8, vec![b'a'; 2048], false)
                    .expect("item")
            )
        );
        sync(&mut controller, &fake, start + POLL_INTERVAL * 3);
        assert!(fake.0.borrow().writes.is_empty());
    }

    #[test]
    fn a_host_to_client_policy_never_reads_the_local_clipboard() {
        let fake = Fake::default();
        let (mut controller, session, start) = attached(&fake, ClipboardDirectionMsg::HostToClient);
        fake.0.borrow_mut().copy_text("secret");
        sync(&mut controller, &fake, start + POLL_INTERVAL);
        assert!(session.take_outbound().is_none());
    }

    #[test]
    fn no_policy_closes_the_platform_clipboard_and_detach_clears() {
        let fake = Fake::default();
        let (mut controller, session, start) = attached(&fake, ClipboardDirectionMsg::Both);
        assert!(controller.clipboard.is_some());
        session.set_policy(None);
        sync(&mut controller, &fake, start + POLL_INTERVAL);
        assert!(controller.clipboard.is_none());
        controller.detach();
        assert_eq!(controller.next_sequence, 1);
        assert!(controller.generation.is_none());
    }
}
