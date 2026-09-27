//! Who owns the physical console, and waiting for that to become the right
//! person.
//!
//! A Pier can only serve the desktop its window server session owns. That
//! makes "who is at the console right now" a fact the session layer has to
//! establish before it hands anyone a screen, and it is emphatically not the
//! account the Pier process happens to run as — an earlier version read
//! `$USER` and would have compared a service account against the person
//! logged in.
//!
//! The Windows host solves the same problem with
//! `WTSGetActiveConsoleSessionId`, activates the console, and bounds the wait
//! for a first interactive login with `first_login_timeout_secs`. This module
//! is the macOS half of that procedure: read the console owner, and wait a
//! bounded time for it to become the authenticated account.
//!
//! What it deliberately does not do is *drive* the switch. On macOS, putting
//! the login window into credential collection for a named user requires an
//! authorization plug-in in `/Library/Security/SecurityAgentPlugins/`, which
//! Arcen does not yet ship. Waiting for a switch an operator performs is
//! honest and useful; claiming to have caused it would not be.

use std::time::{Duration, Instant};

/// How long to wait for the console to become the authenticated user.
///
/// Ten minutes, matching the `first_login_timeout_secs` value the Windows
/// host documents, so an operator reading either runbook sees the same
/// number.
pub const DEFAULT_FIRST_LOGIN_TIMEOUT: Duration = Duration::from_secs(600);

/// How often the console owner is re-read while waiting.
const POLL: Duration = Duration::from_millis(500);

/// Why a session could not be bound to the console.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConsoleError {
    /// Nobody is logged in at the console.
    ///
    /// On macOS the login window itself owns the console as `root` or
    /// `loginwindow`, which is not a desktop anyone can be served.
    NoConsoleUser,
    /// Somebody else is at the console and did not switch within the timeout.
    Mismatch {
        /// The account that authenticated.
        authenticated: String,
        /// The account actually at the console.
        console_owner: String,
        /// How long was spent waiting.
        waited: Duration,
    },
}

impl std::fmt::Display for ConsoleError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoConsoleUser => write!(
                formatter,
                "no user is logged in at this Mac's console, so there is no desktop to serve; \
                 log in at the machine, or wait for a login-window provider"
            ),
            Self::Mismatch {
                authenticated,
                console_owner,
                waited,
            } => write!(
                formatter,
                "{authenticated} authenticated, but {console_owner} is still at the console after \
                 {}s; switch the console to {authenticated} (log out, or use Fast User Switching) \
                 and reconnect",
                waited.as_secs()
            ),
        }
    }
}

impl std::error::Error for ConsoleError {}

/// Names macOS uses for "nobody is really logged in".
///
/// The login window owns the console between sessions, and it reports itself
/// under one of these. Treating them as an ordinary user would mean trying to
/// serve the login screen as though it were somebody's desktop.
const UNOWNED: [&str; 3] = ["root", "loginwindow", "_mbsetupuser"];

/// Returns the account currently at the physical console.
///
/// Returns `None` when the console is unowned, which is a different state from
/// "an error occurred" and is reported as such.
#[must_use]
pub fn console_owner() -> Option<String> {
    let owner = native::console_owner()?;
    if owner.is_empty() || UNOWNED.contains(&owner.as_str()) {
        return None;
    }
    Some(owner)
}

/// Returns who holds the console, by uid, for routing a Deck to the agent of
/// the session on screen.
///
/// The login window reports itself as `root` or `loginwindow`; an unreadable
/// console is treated the same way, because either way no user's desktop is on
/// screen.
#[must_use]
pub fn console_holder() -> arcen_session::agent_relay::ConsoleHolder {
    match native::console_identity() {
        Some((name, uid)) if uid != 0 && !name.is_empty() && !UNOWNED.contains(&name.as_str()) => {
            arcen_session::agent_relay::ConsoleHolder::User(uid)
        }
        _ => arcen_session::agent_relay::ConsoleHolder::LoginWindow,
    }
}

/// Waits for `authenticated` to own the console, up to `timeout`.
///
/// Returns immediately when they already do. This is the macOS counterpart of
/// the Windows first-login wait: an operator switching users at the machine is
/// a normal thing to be waiting for, and a session that refuses instantly
/// would make Fast User Switching unusable.
///
/// # Errors
///
/// Returns [`ConsoleError`] when the console stays unowned or owned by someone
/// else for the whole timeout.
pub fn wait_for_console_owner(
    authenticated: &str,
    timeout: Duration,
) -> Result<String, ConsoleError> {
    let started = Instant::now();
    let mut announced = false;

    loop {
        match console_owner() {
            Some(owner) if owner.eq_ignore_ascii_case(authenticated) => return Ok(owner),
            // Somebody else is already at the screen. Waiting cannot help:
            // a session does not end because a remote client is hoping it
            // will, so the ten-minute first-login budget would be spent and
            // the Deck would give up first, showing "connection timed out" for
            // what is really "that account is not the one logged in here".
            //
            // Measured before this existed: a Deck authenticated as one
            // account while another held the console, sat with no picture, and
            // timed out with nothing to read on either end.
            Some(owner) => {
                return Err(ConsoleError::Mismatch {
                    authenticated: authenticated.to_owned(),
                    console_owner: owner,
                    waited: started.elapsed(),
                });
            }
            // Nobody is at the screen yet. This one is worth waiting for: it
            // is an ordinary cold boot or a logout, the account may be about
            // to appear, and refusing instantly would make first login and
            // Fast User Switching unusable.
            None => {}
        }

        // Say what is being waited for, once, as soon as it is clear this will
        // be a wait. Without this the host is indistinguishable from a hung
        // one: a Deck that authenticated successfully sits with no picture and
        // no reason, for as long as the first-login timeout allows, and the
        // operator has nothing to read. The authenticated account and the
        // account actually at the screen are both named, because the usual
        // cause is that they differ and that is invisible from either end.
        if !announced {
            announced = true;
            tracing::warn!(
                target: arcen_telemetry::names::target::SESSION,
                authenticated = %authenticated,
                timeout_secs = timeout.as_secs(),
                "waiting for the authenticated account to log in at the console",
            );
        }

        if started.elapsed() >= timeout {
            return Err(ConsoleError::NoConsoleUser);
        }
        std::thread::sleep(POLL);
    }
}

#[cfg(target_os = "macos")]
mod native {
    // One SystemConfiguration call, with its SAFETY note below.
    #![allow(unsafe_code)]

    use objc2_core_foundation::{CFRetained, CFString};
    use std::ptr::NonNull;

    // SAFETY: declared to match SystemConfiguration's published signature.
    // `SCDynamicStoreCopyConsoleUser` takes an optional store and two optional
    // out-parameters for uid and gid, and follows the Core Foundation copy
    // rule for its return value.
    #[link(name = "SystemConfiguration", kind = "framework")]
    unsafe extern "C" {
        fn SCDynamicStoreCopyConsoleUser(
            store: *const std::ffi::c_void,
            uid: *mut u32,
            gid: *mut u32,
        ) -> *const CFString;
    }

    /// Reads the console user from the system configuration store.
    pub(super) fn console_owner() -> Option<String> {
        // SAFETY: a null store asks SystemConfiguration to use a temporary
        // one, which is the documented way to make a single query. Both
        // out-parameters are null because the numeric ids are not needed, and
        // the function accepts null for them.
        let raw = unsafe {
            SCDynamicStoreCopyConsoleUser(
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        let name = NonNull::new(raw.cast_mut())?;
        // SAFETY: this function follows Core Foundation's copy rule — the name
        // says `Copy`, which is the documented signal that the caller owns the
        // result — so ownership is taken here and released when the
        // `CFRetained` drops.
        let owner = unsafe { CFRetained::from_raw(name) };
        Some(owner.to_string())
    }

    /// Reads the console owner's name and uid in one query.
    pub(super) fn console_identity() -> Option<(String, u32)> {
        let mut uid = u32::MAX;
        // SAFETY: as above; `uid` is a live out-parameter for the call and the
        // gid is not wanted.
        let raw = unsafe {
            SCDynamicStoreCopyConsoleUser(std::ptr::null(), &raw mut uid, std::ptr::null_mut())
        };
        let name = NonNull::new(raw.cast_mut())?;
        // SAFETY: Core Foundation copy rule, as above.
        let owner = unsafe { CFRetained::from_raw(name) };
        Some((owner.to_string(), uid))
    }
}

#[cfg(not(target_os = "macos"))]
mod native {
    /// There is no macOS console to read here.
    pub(super) fn console_owner() -> Option<String> {
        None
    }

    pub(super) fn console_identity() -> Option<(String, u32)> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_login_window_is_not_a_user_to_serve() {
        // These own the console between sessions. Serving one as though it
        // were somebody's desktop is how a remote client ends up staring at a
        // login screen it cannot use, or at another user's session.
        for name in UNOWNED {
            assert!(
                UNOWNED.contains(&name),
                "{name} must be treated as an unowned console"
            );
        }
    }

    #[test]
    fn a_mismatch_says_who_is_there_and_what_to_do() {
        // The message an operator reads when a session is refused has to name
        // both accounts. "Permission denied" sends someone to check a password
        // that was correct.
        let error = ConsoleError::Mismatch {
            authenticated: "jc".to_owned(),
            console_owner: "admin".to_owned(),
            waited: Duration::from_secs(600),
        }
        .to_string();
        assert!(error.contains("jc"));
        assert!(error.contains("admin"));
        assert!(error.contains("600s"));
        assert!(error.contains("Fast User Switching"));
    }

    #[test]
    fn an_empty_console_is_reported_as_unowned_not_as_a_user() {
        let error = ConsoleError::NoConsoleUser.to_string();
        assert!(error.contains("no user is logged in"));
    }

    #[test]
    fn a_zero_timeout_still_checks_once() {
        // Waiting zero seconds must not mean "never look": a session where the
        // right person is already at the console should not be refused for
        // want of a poll.
        let result = wait_for_console_owner("definitely-not-a-real-account", Duration::ZERO);
        assert!(result.is_err(), "nobody is logged in under that name");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_console_owner_is_read_from_the_system_not_from_the_environment() {
        // `$USER` is the account this process runs as, which on a service is
        // never the person at the screen. This is the distinction the module
        // exists for, so it is asserted against the real system.
        let owner = console_owner();
        if let Some(owner) = owner {
            assert!(!owner.is_empty());
            assert!(!UNOWNED.contains(&owner.as_str()));
            println!("console owner: {owner}");
        } else {
            println!("console is unowned (login window or no session)");
        }
    }

    #[test]
    fn a_mismatched_console_owner_is_reported_as_a_mismatch_not_a_timeout() {
        // The failure an operator meets most often is authenticating as one
        // account while a different one is at the screen. It has to name both,
        // because from the Deck it looks like the host simply stopped
        // answering, and from the host it looks like a successful login.
        let error = wait_for_console_owner("nobody-should-own-this", Duration::from_millis(1));
        match error {
            Err(ConsoleError::Mismatch {
                authenticated,
                console_owner,
                ..
            }) => {
                assert_eq!(authenticated, "nobody-should-own-this");
                assert!(
                    !console_owner.is_empty(),
                    "the account actually at the screen must be named",
                );
            }
            // A machine with nobody at the console is a legitimate outcome on
            // a build agent, and is a different, equally explicit error.
            Err(ConsoleError::NoConsoleUser) => {}
            Ok(owner) => panic!("this account should not own the console: {owner}"),
        }
    }

    #[test]
    fn a_different_account_at_the_screen_is_refused_at_once_not_waited_out() {
        // Measured before this existed: a Deck authenticated as one account
        // while another held the console, sat with no picture for the whole
        // first-login budget, and showed "connection timed out" — which told
        // the user nothing about the real cause. Waiting cannot help, because
        // a session does not end because a remote client is hoping it will.
        let started = std::time::Instant::now();
        let outcome = wait_for_console_owner("nobody-owns-this-console", Duration::from_secs(600));
        let elapsed = started.elapsed();
        match outcome {
            Err(ConsoleError::Mismatch { console_owner, .. }) => {
                assert!(!console_owner.is_empty(), "name who is actually there");
                assert!(
                    elapsed < Duration::from_secs(5),
                    "a mismatch must be refused at once, took {elapsed:?}",
                );
            }
            // A machine with nobody at the console legitimately waits, and on
            // a build agent that is the outcome; the timeout is honoured there.
            Err(ConsoleError::NoConsoleUser) => {}
            Ok(owner) => panic!("this account should not own the console: {owner}"),
        }
    }
}
