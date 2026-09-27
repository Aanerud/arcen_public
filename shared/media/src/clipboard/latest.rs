//! A one-slot clipboard handover where the newest copy wins.
//!
//! A clipboard is not a queue. Someone who copies three things in a row before
//! the network drains any of them wants the third pasted, not the first, and
//! certainly not all three in order. Yet the obvious implementation — a bounded
//! channel — gives exactly the wrong answer under pressure: when it is full it
//! rejects the *newest* item, so the peer receives the oldest copies and the
//! one the user actually wants is the one thrown away.
//!
//! macOS had that bug with a comment claiming the opposite, and the claim was
//! not merely optimistic. It said a dropped offer would be picked up by the
//! next poll, but reading the pasteboard consumes its change counter, so there
//! was no next poll: the copy was gone until the user copied again.
//!
//! This slot holds at most one payload and replaces it. Nothing queues, so
//! nothing can be stale, and the failure mode under load is "you get the
//! latest" rather than "you get the oldest and lose the latest".
//!
//! # Why it is not async
//!
//! `arcen-media` must not pull a runtime into a program that only wanted
//! clipboard policy, so this is a plain mutex with no waker. Callers that need
//! to block already have a cadence to poll on — a pasteboard has to be polled
//! regardless, because it publishes no change notification.

/// A clipboard payload waiting to be handed over, and its sequence number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardSlotItem<T> {
    /// Ordering, so a late arrival cannot displace a newer one.
    pub sequence: u64,
    /// What was copied.
    pub payload: T,
}

/// Holds the most recent clipboard payload, and only that one.
#[derive(Debug)]
pub struct LatestClipboard<T> {
    item: Option<ClipboardSlotItem<T>>,
    /// The highest sequence ever accepted, retained after `take`.
    ///
    /// Kept separately from the item so that taking the payload does not reopen
    /// the door to an older one. A retransmitted or reordered offer arriving
    /// after its successor was delivered would otherwise overwrite the user's
    /// current clipboard with a previous copy.
    latest: u64,
    /// Payloads displaced before anyone read them.
    replaced: u64,
}

impl<T> Default for LatestClipboard<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> LatestClipboard<T> {
    /// Creates an empty slot.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            item: None,
            latest: 0,
            replaced: 0,
        }
    }

    /// Offers a payload, displacing any older one.
    ///
    /// Returns whether it was accepted. An offer at or below the highest
    /// sequence already seen is refused, because it is older than something
    /// this slot has already handed on.
    ///
    /// Whatever it displaces is dropped immediately. Scrubbing is the
    /// payload's own responsibility through `Drop` — `ClipboardTransfer`
    /// zeroizes its bytes there — so this slot does not need to know whether a
    /// payload is sensitive, and cannot forget to clear one that is.
    pub fn offer(&mut self, sequence: u64, payload: T) -> bool {
        if sequence <= self.latest {
            return false;
        }
        self.latest = sequence;
        if self.item.take().is_some() {
            self.replaced += 1;
        }
        self.item = Some(ClipboardSlotItem { sequence, payload });
        true
    }

    /// Removes and returns the pending payload, if any.
    pub fn take(&mut self) -> Option<ClipboardSlotItem<T>> {
        self.item.take()
    }

    /// Whether a payload is waiting.
    #[must_use]
    pub const fn is_pending(&self) -> bool {
        self.item.is_some()
    }

    /// How many payloads were displaced before anyone read them.
    ///
    /// A rising count means the consumer is behind. That is not an error — it
    /// is the design working — but it is worth reporting, because a clipboard
    /// that is always behind is a clipboard someone will describe as broken.
    #[must_use]
    pub const fn replaced(&self) -> u64 {
        self.replaced
    }

    /// Drops any pending payload.
    ///
    /// For session teardown: a payload still waiting when a session ends must
    /// not survive into the next one.
    pub fn clear(&mut self) {
        self.item = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_newest_copy_wins() {
        // The whole point. A bounded queue would have kept "first" and
        // rejected "third"; the user wants "third".
        let mut slot = LatestClipboard::new();
        assert!(slot.offer(1, b"first".to_vec()));
        assert!(slot.offer(2, b"second".to_vec()));
        assert!(slot.offer(3, b"third".to_vec()));

        let taken = slot.take().expect("a payload");
        assert_eq!(taken.payload, b"third".to_vec());
        assert_eq!(taken.sequence, 3);
        assert!(!slot.is_pending(), "only one payload is ever held");
        assert_eq!(slot.replaced(), 2, "two were displaced unread");
    }

    #[test]
    fn an_older_offer_cannot_displace_a_newer_one() {
        let mut slot = LatestClipboard::new();
        assert!(slot.offer(5, b"current".to_vec()));
        assert!(
            !slot.offer(4, b"stale".to_vec()),
            "a reordered arrival must be refused",
        );
        assert_eq!(slot.take().expect("a payload").payload, b"current".to_vec());
    }

    #[test]
    fn taking_does_not_reopen_the_door_to_an_older_copy() {
        // The reason `latest` outlives the item. Without it, an offer that was
        // delayed in flight would overwrite the user's clipboard with a copy
        // they made before the one they already have.
        let mut slot = LatestClipboard::new();
        assert!(slot.offer(9, b"delivered".to_vec()));
        slot.take().expect("a payload");
        assert!(!slot.offer(8, b"arrived late".to_vec()));
        assert!(!slot.offer(9, b"duplicate".to_vec()));
        assert!(slot.offer(10, b"genuinely newer".to_vec()));
    }

    #[test]
    fn a_displaced_payload_is_dropped_at_once() {
        // Scrubbing belongs to the payload's own `Drop`, so what this has to
        // guarantee is that a displaced payload is released immediately rather
        // than lingering in the slot where nothing will clear it.
        struct Tracked(std::rc::Rc<std::cell::Cell<u32>>);
        impl Drop for Tracked {
            fn drop(&mut self) {
                self.0.set(self.0.get() + 1);
            }
        }

        let drops = std::rc::Rc::new(std::cell::Cell::new(0));
        let mut slot = LatestClipboard::new();
        slot.offer(1, Tracked(std::rc::Rc::clone(&drops)));
        assert_eq!(drops.get(), 0);
        slot.offer(2, Tracked(std::rc::Rc::clone(&drops)));
        assert_eq!(drops.get(), 1, "the displaced payload is dropped at once");

        slot.clear();
        assert_eq!(drops.get(), 2, "teardown leaves nothing behind");
        assert!(!slot.is_pending());
    }

    #[test]
    fn an_empty_slot_yields_nothing() {
        let mut slot: LatestClipboard<Vec<u8>> = LatestClipboard::new();
        assert!(slot.take().is_none());
        assert!(!slot.is_pending());
        assert_eq!(slot.replaced(), 0);
    }
}
