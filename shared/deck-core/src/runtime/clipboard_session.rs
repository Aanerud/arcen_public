// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The session's bounded clipboard state: one owned payload each way and
//! the host's policy (moved from the macOS Deck, ADR 0024 step 4). The
//! platform clipboard adapter (`crate::clipboard::PlatformClipboard`)
//! reads and writes the operating system's clipboard around it.

use std::fmt::{Debug, Formatter};
use std::sync::{Arc, Mutex};

use arcen_media::clipboard::{ClipboardContent, ClipboardDirection, ClipboardPolicy};
use arcen_protocol::messages::{
    ClipboardContentKind, ClipboardContentMsg, ClipboardDirectionMsg, ClipboardPolicyMsg,
};
use tokio::sync::Notify;
use zeroize::Zeroize;

/// One owned normalized clipboard payload.
pub struct ClipboardItem {
    pub sequence: u64,
    pub kind: ClipboardContentKind,
    pub bytes: Vec<u8>,
    pub truncated: bool,
}

impl ClipboardItem {
    #[must_use]
    pub fn new(
        sequence: u64,
        kind: ClipboardContentKind,
        bytes: Vec<u8>,
        truncated: bool,
    ) -> Option<Self> {
        if sequence == 0
            || bytes.is_empty()
            || u32::try_from(bytes.len()).is_err()
            || (truncated && kind != ClipboardContentKind::TextUtf8)
        {
            return None;
        }
        Some(Self {
            sequence,
            kind,
            bytes,
            truncated,
        })
    }

    pub fn scrub(&mut self) {
        self.bytes.zeroize();
        self.bytes.clear();
    }
}

impl Debug for ClipboardItem {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ClipboardItem")
            .field("sequence", &self.sequence)
            .field("kind", &self.kind)
            .field("bytes", &self.bytes.len())
            .field("truncated", &self.truncated)
            .finish()
    }
}

impl Drop for ClipboardItem {
    fn drop(&mut self) {
        self.bytes.zeroize();
    }
}

#[derive(Debug, Default)]
struct ClipboardSlots {
    policy: Option<ClipboardPolicyMsg>,
    inbound: Option<ClipboardItem>,
    outbound: Option<ClipboardItem>,
}

impl Drop for ClipboardSlots {
    fn drop(&mut self) {
        clear_item(&mut self.inbound);
        clear_item(&mut self.outbound);
    }
}

#[derive(Debug)]
struct ClipboardSessionInner {
    slots: Mutex<ClipboardSlots>,
    outbound_ready: Notify,
}

/// Capacity-one inbound and outbound clipboard mailboxes for one transport generation.
#[derive(Clone)]
pub struct ClipboardSession {
    inner: Arc<ClipboardSessionInner>,
}

impl ClipboardSession {
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(ClipboardSessionInner {
                slots: Mutex::new(ClipboardSlots::default()),
                outbound_ready: Notify::new(),
            }),
        }
    }

    pub fn set_policy(&self, policy: Option<ClipboardPolicyMsg>) {
        let mut slots = self.lock_slots();
        slots.policy = policy;
        if policy.is_none() {
            clear_item(&mut slots.inbound);
            clear_item(&mut slots.outbound);
        }
    }

    #[must_use]
    pub fn policy(&self) -> Option<ClipboardPolicyMsg> {
        self.lock_slots().policy
    }

    /// Replaces the pending outbound item only when the sequence is newer.
    #[must_use]
    pub fn queue_outbound(&self, mut item: ClipboardItem) -> bool {
        let mut slots = self.lock_slots();
        if slots.policy.is_none()
            || slots
                .outbound
                .as_ref()
                .is_some_and(|pending| pending.sequence >= item.sequence)
        {
            item.scrub();
            return false;
        }
        clear_item(&mut slots.outbound);
        slots.outbound = Some(item);
        drop(slots);
        self.inner.outbound_ready.notify_one();
        true
    }

    #[must_use]
    pub fn take_outbound(&self) -> Option<ClipboardItem> {
        self.lock_slots().outbound.take()
    }

    pub async fn outbound_notified(&self) {
        self.inner.outbound_ready.notified().await;
    }

    /// Replaces the completed inbound item only when the sequence is newer.
    #[must_use]
    pub fn queue_inbound(&self, mut item: ClipboardItem) -> bool {
        let mut slots = self.lock_slots();
        if slots.policy.is_none()
            || slots
                .inbound
                .as_ref()
                .is_some_and(|pending| pending.sequence >= item.sequence)
        {
            item.scrub();
            return false;
        }
        clear_item(&mut slots.inbound);
        slots.inbound = Some(item);
        true
    }

    #[must_use]
    pub fn take_inbound(&self) -> Option<ClipboardItem> {
        self.lock_slots().inbound.take()
    }

    pub fn clear(&self) {
        let mut slots = self.lock_slots();
        slots.policy = None;
        clear_item(&mut slots.inbound);
        clear_item(&mut slots.outbound);
        drop(slots);
        self.inner.outbound_ready.notify_one();
    }

    fn lock_slots(&self) -> std::sync::MutexGuard<'_, ClipboardSlots> {
        self.inner
            .slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Default for ClipboardSession {
    fn default() -> Self {
        Self::new()
    }
}

impl Debug for ClipboardSession {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        let slots = self.lock_slots();
        formatter
            .debug_struct("ClipboardSession")
            .field("policy", &slots.policy)
            .field("inbound", &slots.inbound.as_ref().map(|item| item.sequence))
            .field(
                "outbound",
                &slots.outbound.as_ref().map(|item| item.sequence),
            )
            .finish()
    }
}

fn clear_item(slot: &mut Option<ClipboardItem>) {
    if let Some(mut item) = slot.take() {
        item.scrub();
    }
}

/// Maps a wire policy to the shared host-authoritative policy.
#[must_use]
pub fn media_policy(policy: ClipboardPolicyMsg) -> Option<ClipboardPolicy> {
    let direction = match policy.direction {
        ClipboardDirectionMsg::Both => ClipboardDirection::Both,
        ClipboardDirectionMsg::ClientToHost => ClipboardDirection::ClientToHost,
        ClipboardDirectionMsg::HostToClient => ClipboardDirection::HostToClient,
        ClipboardDirectionMsg::Disabled => ClipboardDirection::Disabled,
    };
    let content = match policy.content {
        ClipboardContentMsg::All => ClipboardContent::All,
        ClipboardContentMsg::Text => ClipboardContent::Text,
        ClipboardContentMsg::Image => ClipboardContent::Image,
    };
    ClipboardPolicy::new(direction, content, usize::try_from(policy.max_bytes).ok()?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcen_protocol::messages::CLIPBOARD_PROTOCOL_VERSION;

    fn policy() -> ClipboardPolicyMsg {
        ClipboardPolicyMsg {
            protocol_version: CLIPBOARD_PROTOCOL_VERSION,
            direction: ClipboardDirectionMsg::Both,
            content: ClipboardContentMsg::All,
            max_bytes: 1024,
        }
    }

    fn text(sequence: u64, bytes: &[u8]) -> ClipboardItem {
        ClipboardItem::new(
            sequence,
            ClipboardContentKind::TextUtf8,
            bytes.to_vec(),
            false,
        )
        .expect("item")
    }

    #[test]
    fn latest_slots_replace_and_disabled_state_scrubs() {
        let session = ClipboardSession::new();
        session.set_policy(Some(policy()));
        assert!(session.queue_outbound(text(1, b"one")));
        assert!(!session.queue_outbound(text(1, b"stale")));
        assert!(session.queue_outbound(text(2, b"two")));
        assert_eq!(session.take_outbound().expect("latest").bytes, b"two");

        session.set_policy(None);
        assert!(!session.queue_outbound(text(3, b"off")));
        assert!(session.take_outbound().is_none());
    }
}
