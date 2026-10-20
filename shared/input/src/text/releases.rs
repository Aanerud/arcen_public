//! Releases an input method swallowed.
//!
//! While a client's input method composes, its platform may deliver no key
//! releases at all (winit on macOS sends none outside its ground state). A
//! key pressed before a dead key and released during the composition then
//! stays held on the host, which repeats it. The client records each press
//! it actually sent (the host's key, the modifiers, and the platform's own
//! key code), and asks the keyboard's physical state about the keys still
//! held. A key found up is released only once it has stayed up for a grace
//! period, so a release still waiting in the client's event queue arrives
//! first and is never sent twice.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

/// How long a key must be physically up, with its release not delivered,
/// before the client releases it itself.
pub const LOST_RELEASE_GRACE: Duration = Duration::from_millis(80);

/// A key press the client sent to the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SentPress {
    /// The host key sent.
    pub key: u32,
    /// The wire modifiers sent with it.
    pub modifiers: u32,
    /// The platform's key code for the physical key, if it has one; a key
    /// without one is never released here.
    pub key_code: Option<u16>,
}

#[derive(Debug, Clone, Copy)]
struct Held {
    press: SentPress,
    up_since: Option<Instant>,
}

/// The presses a client sent and has not released.
#[derive(Debug, Clone)]
pub struct ReleaseRecovery {
    held: BTreeMap<u32, Held>,
    grace: Duration,
    press_bits: u32,
}

impl ReleaseRecovery {
    /// A tracker that waits `grace`. `press_bits` are wire modifier bits
    /// that describe the key itself (a keypad key) and are sent with its
    /// release as they were with its press; the other bits are the current
    /// modifiers.
    #[must_use]
    pub const fn new(grace: Duration, press_bits: u32) -> Self {
        Self {
            held: BTreeMap::new(),
            grace,
            press_bits,
        }
    }

    /// The client sent `press`.
    pub fn pressed(&mut self, press: SentPress) {
        self.held.insert(
            press.key,
            Held {
                press,
                up_since: None,
            },
        );
    }

    /// The client sent the release of `key`.
    pub fn released(&mut self, key: u32) {
        self.held.remove(&key);
    }

    /// Forgets every press (the client released everything another way).
    pub fn clear(&mut self) {
        self.held.clear();
    }

    /// Whether a key was found up and waits out its grace: the caller
    /// should look again after [`Self::grace`].
    #[must_use]
    pub fn waiting(&self) -> bool {
        self.held.values().any(|held| held.up_since.is_some())
    }

    /// The grace period.
    #[must_use]
    pub const fn grace(&self) -> Duration {
        self.grace
    }

    /// The releases to send now, as `(key, modifiers)`, for keys that have
    /// been physically up for the whole grace period. `is_down` reads a key
    /// code's physical state; `modifiers` are the current wire modifiers.
    pub fn lost(
        &mut self,
        now: Instant,
        modifiers: u32,
        is_down: impl Fn(u16) -> bool,
    ) -> Vec<(u32, u32)> {
        let mut lost = Vec::new();
        for held in self.held.values_mut() {
            let Some(code) = held.press.key_code else {
                continue;
            };
            if is_down(code) {
                held.up_since = None;
                continue;
            }
            let since = *held.up_since.get_or_insert(now);
            if now.saturating_duration_since(since) >= self.grace {
                lost.push((
                    held.press.key,
                    (modifiers & !self.press_bits) | (held.press.modifiers & self.press_bits),
                ));
            }
        }
        for (key, _) in &lost {
            self.held.remove(key);
        }
        lost
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEYPAD: u32 = 1 << 4;
    const SHIFT: u32 = 1;

    fn press(key: u32, modifiers: u32, key_code: u16) -> SentPress {
        SentPress {
            key,
            modifiers,
            key_code: Some(key_code),
        }
    }

    #[test]
    fn a_key_up_for_the_grace_period_is_released_as_it_was_pressed() {
        let start = Instant::now();
        let mut recovery = ReleaseRecovery::new(LOST_RELEASE_GRACE, KEYPAD);
        // Keypad 1 (key code 83), pressed with Shift.
        recovery.pressed(press(0x31, KEYPAD | SHIFT, 83));
        assert!(recovery.lost(start, SHIFT, |_| true).is_empty());
        // Up: not yet, its release may still be queued.
        assert!(recovery.lost(start, 0, |_| false).is_empty());
        assert!(recovery.waiting());
        assert!(
            recovery
                .lost(start + LOST_RELEASE_GRACE / 2, 0, |_| false)
                .is_empty()
        );
        // Still up after the grace: released as keypad 1, with the current
        // modifiers (Shift no longer held).
        assert_eq!(
            recovery.lost(start + LOST_RELEASE_GRACE, 0, |_| false),
            [(0x31, KEYPAD)]
        );
        assert!(!recovery.waiting());
        assert!(
            recovery
                .lost(start + LOST_RELEASE_GRACE * 4, 0, |_| false)
                .is_empty()
        );
    }

    #[test]
    fn a_release_delivered_within_the_grace_is_never_sent_twice() {
        let start = Instant::now();
        let mut recovery = ReleaseRecovery::new(LOST_RELEASE_GRACE, KEYPAD);
        recovery.pressed(press(0x0100_0005, KEYPAD, 76));
        // The keyboard reports the key up before its release event arrives.
        assert!(recovery.lost(start, 0, |_| false).is_empty());
        recovery.released(0x0100_0005);
        assert!(
            recovery
                .lost(start + LOST_RELEASE_GRACE * 2, 0, |_| false)
                .is_empty()
        );
    }

    #[test]
    fn a_key_pressed_again_restarts_its_grace() {
        let start = Instant::now();
        let mut recovery = ReleaseRecovery::new(LOST_RELEASE_GRACE, KEYPAD);
        recovery.pressed(press(0x2A, KEYPAD, 67));
        assert!(recovery.lost(start, 0, |_| false).is_empty());
        assert!(
            recovery
                .lost(start + LOST_RELEASE_GRACE / 2, 0, |code| code == 67)
                .is_empty()
        );
        assert!(
            recovery
                .lost(start + LOST_RELEASE_GRACE, 0, |_| false)
                .is_empty()
        );
        assert_eq!(
            recovery.lost(start + LOST_RELEASE_GRACE * 2, 0, |_| false),
            [(0x2A, KEYPAD)]
        );
    }

    #[test]
    fn a_key_without_a_key_code_is_left_alone() {
        let mut recovery = ReleaseRecovery::new(LOST_RELEASE_GRACE, KEYPAD);
        recovery.pressed(SentPress {
            key: 0x41,
            modifiers: 0,
            key_code: None,
        });
        let later = Instant::now() + LOST_RELEASE_GRACE * 2;
        assert!(recovery.lost(Instant::now(), 0, |_| false).is_empty());
        assert!(recovery.lost(later, 0, |_| false).is_empty());
    }
}
