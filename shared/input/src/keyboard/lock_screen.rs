// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Keys a host posts into its own locked session's password field, and the
//! user activity a person's keys would have made.
//!
//! A macOS lock screen that has gone idle checks a submitted password only
//! once the power manager reports a person active, which hardware input
//! does and posted input does not: it waits five seconds and fails the
//! password as incorrect, however right its characters are. It also shows
//! its password field, focused, only for a person's first keystroke; a
//! posted keystroke shows it without focus and the keys after it go
//! nowhere. So a host that posts keys declares that activity itself
//! ([`LockScreenActivity`]):
//!
//! - **After a submit.** A Return or Enter pressed at the locked console is
//!   posted first and the activity is declared [`LOCK_SCREEN_SUBMIT_DELAY`]
//!   later, while the lock screen waits for it, so the password it already
//!   holds is checked.
//! - **Before the first key into a hidden field.** A key pressed at the
//!   locked console while no field holds secure input (the password field
//!   is not up) is held, with everything the Deck sends after it, until the
//!   declared activity has brought the field up, focused
//!   ([`LOCK_SCREEN_WAKE_SETTLE`]), at most once per
//!   [`LOCK_SCREEN_WAKE_INTERVAL`]. A field that is up (a click put it
//!   there) is left alone: waking it again took its focus away.
//! - **Nowhere else.** Keys on the desktop, and in a desktop's own password
//!   fields, are posted as before; a release never waits.
//!
//! The hold is [`InputHold`]: an ordered queue a host's session loop drains
//! from a timer, so nothing waits on the thread that also sends the picture.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// How long after a posted submit the activity is declared: the lock screen
/// starts waiting for it as the key arrives, and waits five seconds.
pub const LOCK_SCREEN_SUBMIT_DELAY: Duration = Duration::from_millis(250);

/// How long a key into a hidden field is held after the activity is declared:
/// the lock screen raised and focused its field within 0.1 s and animates
/// for a while after.
pub const LOCK_SCREEN_WAKE_SETTLE: Duration = Duration::from_millis(1500);

/// The least time between two wakes before keys.
pub const LOCK_SCREEN_WAKE_INTERVAL: Duration = Duration::from_secs(4);

/// What the host reads when a key is pressed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LockScreenFacts {
    /// The host's own session is on the console and its screen is locked.
    pub locked_on_console: bool,
    /// A field holds secure input (on macOS, Secure Event Input): the
    /// password field is up.
    pub secure_input: bool,
}

/// What the host does around one posted key edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockScreenStep {
    /// Post the key.
    Post,
    /// Declare user activity now, and hold this key and everything after it
    /// ([`InputHold`]) for [`LOCK_SCREEN_WAKE_SETTLE`].
    WakeThenHold,
    /// Post the key, then declare user activity
    /// [`LOCK_SCREEN_SUBMIT_DELAY`] later.
    PostThenDeclare,
}

/// The per-session state: feed every posted key edge, in order.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LockScreenActivity {
    last_wake: Option<Instant>,
}

impl LockScreenActivity {
    /// What to do around a key edge at `now`. `submits`: the key is Return
    /// or Enter. `facts` is read only for a press.
    pub fn key(
        &mut self,
        pressed: bool,
        submits: bool,
        now: Instant,
        facts: impl FnOnce() -> LockScreenFacts,
    ) -> LockScreenStep {
        if !pressed {
            return LockScreenStep::Post;
        }
        let facts = facts();
        if !facts.locked_on_console {
            self.last_wake = None;
            return LockScreenStep::Post;
        }
        if submits {
            return LockScreenStep::PostThenDeclare;
        }
        let woke_lately = self
            .last_wake
            .is_some_and(|woke| now.saturating_duration_since(woke) < LOCK_SCREEN_WAKE_INTERVAL);
        if facts.secure_input || woke_lately {
            return LockScreenStep::Post;
        }
        self.last_wake = Some(now);
        LockScreenStep::WakeThenHold
    }
}

/// Input held back, in arrival order, until a deadline: the key that woke a
/// lock screen and everything the Deck sent after it. A host feeds every
/// message through [`InputHold::defer`], drains [`InputHold::next_due`]
/// when [`InputHold::due`] passes (a timer in its session loop, never a
/// sleep), and [`InputHold::clear`]s it when the session ends: nothing held
/// was posted, so nothing held can be left down.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputHold<T> {
    until: Option<Instant>,
    queue: VecDeque<T>,
}

impl<T> Default for InputHold<T> {
    fn default() -> Self {
        Self {
            until: None,
            queue: VecDeque::new(),
        }
    }
}

impl<T> InputHold<T> {
    /// Holds `first`, ahead of anything still held, until at least `until`.
    /// While held input is being drained, the item that holds again is the
    /// one that was due next, so it stays first.
    pub fn hold(&mut self, until: Instant, first: T) {
        self.until = Some(self.until.map_or(until, |held| held.max(until)));
        self.queue.push_front(first);
    }

    /// `item` when nothing is held (apply it now); otherwise it is held
    /// behind what is, and `None`.
    pub fn defer(&mut self, item: T) -> Option<T> {
        if self.until.is_none() {
            return Some(item);
        }
        self.queue.push_back(item);
        None
    }

    /// When held input is due, if any is held.
    #[must_use]
    pub const fn due(&self) -> Option<Instant> {
        self.until
    }

    /// The next held item once it is due at `now`; the hold ends with the
    /// last one.
    pub fn next_due(&mut self, now: Instant) -> Option<T> {
        if self.until.is_none_or(|until| now < until) {
            return None;
        }
        let item = self.queue.pop_front();
        if self.queue.is_empty() {
            self.until = None;
        }
        item
    }

    /// Drops everything held (the session ended); returns how many items.
    pub fn clear(&mut self) -> usize {
        self.until = None;
        let held = self.queue.len();
        self.queue.clear();
        held
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIELD_UP: LockScreenFacts = LockScreenFacts {
        locked_on_console: true,
        secure_input: true,
    };
    const FIELD_HIDDEN: LockScreenFacts = LockScreenFacts {
        locked_on_console: true,
        secure_input: false,
    };
    const DESKTOP: LockScreenFacts = LockScreenFacts {
        locked_on_console: false,
        secure_input: false,
    };

    #[test]
    fn a_submitted_password_is_followed_by_user_activity() {
        let mut lock = LockScreenActivity::default();
        let now = Instant::now();
        assert_eq!(
            lock.key(true, false, now, || FIELD_UP),
            LockScreenStep::Post
        );
        assert_eq!(
            lock.key(true, true, now, || FIELD_UP),
            LockScreenStep::PostThenDeclare
        );
        assert_eq!(
            lock.key(false, true, now, || panic!("a release reads nothing")),
            LockScreenStep::Post
        );
    }

    #[test]
    fn the_first_key_into_a_hidden_field_wakes_it_once() {
        let mut lock = LockScreenActivity::default();
        let now = Instant::now();
        assert_eq!(
            lock.key(true, false, now, || FIELD_HIDDEN),
            LockScreenStep::WakeThenHold
        );
        // Keys queued behind the wake, and a field slow to report itself.
        assert_eq!(
            lock.key(true, false, now + Duration::from_secs(2), || FIELD_HIDDEN),
            LockScreenStep::Post
        );
        assert_eq!(
            lock.key(true, false, now + LOCK_SCREEN_WAKE_INTERVAL, || {
                FIELD_HIDDEN
            }),
            LockScreenStep::WakeThenHold
        );
    }

    #[test]
    fn the_desktop_and_its_own_password_fields_are_left_alone() {
        let mut lock = LockScreenActivity::default();
        let now = Instant::now();
        let secure_desktop = LockScreenFacts {
            locked_on_console: false,
            secure_input: true,
        };
        for facts in [DESKTOP, secure_desktop] {
            assert_eq!(lock.key(true, false, now, || facts), LockScreenStep::Post);
            assert_eq!(lock.key(true, true, now, || facts), LockScreenStep::Post);
        }
    }

    #[test]
    fn leaving_the_lock_screen_forgets_the_last_wake() {
        let mut lock = LockScreenActivity::default();
        let now = Instant::now();
        lock.key(true, false, now, || FIELD_HIDDEN);
        lock.key(true, false, now, || DESKTOP);
        assert_eq!(
            lock.key(true, false, now, || FIELD_HIDDEN),
            LockScreenStep::WakeThenHold
        );
    }

    #[test]
    fn held_input_leaves_in_order_once_due_and_not_before() {
        let start = Instant::now();
        let due = start + LOCK_SCREEN_WAKE_SETTLE;
        let mut hold = InputHold::default();
        assert_eq!(hold.defer("shift down"), Some("shift down"));
        hold.hold(due, "c down");
        for item in ["c up", "shift up", "zero down", "zero up"] {
            assert_eq!(hold.defer(item), None);
        }
        assert_eq!(hold.due(), Some(due));
        assert_eq!(hold.next_due(start + LOCK_SCREEN_WAKE_SETTLE / 2), None);
        let drained: Vec<_> = std::iter::from_fn(|| hold.next_due(due)).collect();
        assert_eq!(
            drained,
            ["c down", "c up", "shift up", "zero down", "zero up"]
        );
        assert_eq!(hold.due(), None);
        assert_eq!(hold.defer("after"), Some("after"));
    }

    #[test]
    fn an_item_that_holds_again_while_draining_stays_ahead_of_the_rest() {
        let start = Instant::now();
        let mut hold = InputHold::default();
        hold.hold(start, "a");
        hold.defer("b");
        hold.defer("c");
        assert_eq!(hold.next_due(start), Some("a"));
        // Applying "a" woke the lock screen again: it is held first, and the
        // rest waits for the new deadline.
        let later = start + LOCK_SCREEN_WAKE_SETTLE;
        hold.hold(later, "a again");
        assert_eq!(hold.defer("d"), None);
        assert_eq!(hold.next_due(start), None);
        let drained: Vec<_> = std::iter::from_fn(|| hold.next_due(later)).collect();
        assert_eq!(drained, ["a again", "b", "c", "d"]);
    }

    #[test]
    fn a_session_that_ends_while_holding_posts_nothing_it_held() {
        let start = Instant::now();
        let mut hold = InputHold::default();
        hold.hold(start + LOCK_SCREEN_WAKE_SETTLE, "c down");
        hold.defer("c up");
        assert_eq!(hold.clear(), 2);
        assert_eq!(hold.due(), None);
        assert_eq!(hold.next_due(start + LOCK_SCREEN_WAKE_INTERVAL), None);
        assert_eq!(hold.defer("next session"), Some("next session"));
    }
}
