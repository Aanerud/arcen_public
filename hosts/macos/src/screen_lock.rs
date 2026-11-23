// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Locking the user's session when the Deck leaves (ADR 0025: disconnect
//! locks, never ends).
//!
//! macOS publishes no call that locks the screen. The system's own "Lock
//! Screen" (Control-Command-Q, the Apple menu) is `SACLockScreenImmediate` in
//! the private `login` framework, found here with `dlsym` and refused visibly
//! when absent, like the other private symbols this host relies on
//! (`SLSDisplayGetPotentialHeadroom`, `CGVirtualDisplay`). Whether the
//! screen is locked is read back from `CGSessionCopyCurrentDictionary`'s
//! `CGSSessionScreenIsLocked`, never assumed from the call returning.
//!
//! Nothing here unlocks: a Deck that comes back finds the lock screen and
//! signs in to it, as at the desk. macOS offers no public unlock after a
//! password check, which is what the Linux Pier does through logind.

use std::time::{Duration, Instant};

/// Why the session was not locked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockError {
    /// This macOS has no `SACLockScreenImmediate`.
    Unavailable,
    /// The call returned and the screen did not report itself locked in
    /// time.
    NotConfirmed,
}

impl std::fmt::Display for LockError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable => formatter.write_str("this macOS offers no screen lock call"),
            Self::NotConfirmed => {
                formatter.write_str("the screen did not report itself locked in time")
            }
        }
    }
}

impl std::error::Error for LockError {}

/// How long a lock is given to show before it is reported as failed.
pub const LOCK_CONFIRM_TIMEOUT: Duration = Duration::from_secs(5);

/// Whether this GUI session's screen is locked, or `None` when the window
/// server does not say (no GUI session).
#[must_use]
pub fn screen_is_locked() -> Option<bool> {
    native::screen_is_locked()
}

/// Whether this GUI session holds the console with its screen locked: its
/// lock screen is what the console's keyboard types into. `false` for a
/// session in the background (another account switched in), whose lock
/// state is no business of keys that reach the console. `None` when the
/// window server does not say.
#[must_use]
pub fn locked_on_console() -> Option<bool> {
    native::locked_on_console()
}

/// Whether this GUI session is the one on the console, as the window
/// server records it (`kCGSessionOnConsoleKey`). `false` for a session in
/// the background (another account switched in); `None` when the window
/// server does not say (no GUI session, or one that has ended).
#[must_use]
pub fn session_on_console() -> Option<bool> {
    native::session_on_console()
}

/// This process's security session id: the audit session the window
/// server's login session runs in, as the public `SessionGetInfo` reports it
/// for `callerSecuritySession` (Security framework, `AuthSession.h`). Unique
/// among the sessions of one boot, shared by every process in the login
/// session: an agent launchd restarts in the user's `gui/<uid>` domain gets
/// the same one (that domain's `asid`). `None` when the security server does
/// not answer.
///
/// Not `CGSessionCopyCurrentDictionary`'s `kCGSSessionIDKey`: that key is
/// not in the dictionary (measured on macOS 26: the dictionary carries
/// `kCGSSessionAuditIDKey` and `kSCSecuritySessionID`, both this same
/// number, and no `kCGSSessionIDKey`), so the attachment record was never
/// written.
#[must_use]
pub fn login_session_id() -> Option<u64> {
    native::login_session_id()
}

/// Locks this GUI session's screen and waits, up to `timeout`, until it
/// reports itself locked. A screen that already is succeeds at once.
///
/// # Errors
///
/// [`LockError`] when there is nothing to call or the lock did not show.
pub fn lock_and_confirm(timeout: Duration) -> Result<(), LockError> {
    if screen_is_locked() == Some(true) {
        return Ok(());
    }
    native::lock_screen()?;
    let deadline = Instant::now() + timeout;
    loop {
        if screen_is_locked() == Some(true) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(LockError::NotConfirmed);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(target_os = "macos")]
mod native {
    // A handful of C entry points, each with its SAFETY note below.
    #![allow(unsafe_code)]

    use super::LockError;
    use std::ffi::{c_char, c_void};

    type CFTypeRef = *const c_void;

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFDictionaryGetValue(dictionary: CFTypeRef, key: CFTypeRef) -> CFTypeRef;
        fn CFStringCreateWithCString(
            allocator: CFTypeRef,
            text: *const c_char,
            encoding: u32,
        ) -> CFTypeRef;
        fn CFGetTypeID(value: CFTypeRef) -> usize;
        fn CFBooleanGetTypeID() -> usize;
        fn CFBooleanGetValue(value: CFTypeRef) -> u8;
        fn CFRelease(value: CFTypeRef);
    }

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGSessionCopyCurrentDictionary() -> CFTypeRef;
    }

    #[link(name = "Security", kind = "framework")]
    unsafe extern "C" {
        fn SessionGetInfo(session: u32, session_id: *mut u32, attributes: *mut u32) -> i32;
    }

    unsafe extern "C" {
        fn dlopen(path: *const c_char, mode: i32) -> *mut c_void;
        fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    }

    const UTF8: u32 = 0x0800_0100;
    const RTLD_LAZY: i32 = 0x1;
    /// `callerSecuritySession`: the session the calling process is in.
    const CALLER_SECURITY_SESSION: u32 = u32::MAX;
    /// `noSecuritySession`, never a valid session id.
    const NO_SECURITY_SESSION: u32 = 0;

    /// An owned Core Foundation string, released on drop.
    struct CfString(CFTypeRef);

    impl CfString {
        fn new(text: &std::ffi::CStr) -> Option<Self> {
            // SAFETY: a NUL-terminated UTF-8 string and the default allocator;
            // the result follows the create rule and is released on drop.
            let string =
                unsafe { CFStringCreateWithCString(std::ptr::null(), text.as_ptr(), UTF8) };
            (!string.is_null()).then_some(Self(string))
        }
    }

    impl Drop for CfString {
        fn drop(&mut self) {
            // SAFETY: owned since creation, released once.
            unsafe { CFRelease(self.0) };
        }
    }

    /// Reads boolean keys of this session's dictionary (an absent or
    /// non-boolean value reads `false`); `None` outside a GUI session or
    /// when a key string cannot be made.
    fn session_flags<const N: usize>(keys: [&std::ffi::CStr; N]) -> Option<[bool; N]> {
        // SAFETY: no arguments; the result follows the copy rule (released
        // below) and is null outside a GUI session.
        let session = unsafe { CGSessionCopyCurrentDictionary() };
        if session.is_null() {
            return None;
        }
        let mut flags = Some([false; N]);
        for (index, name) in keys.into_iter().enumerate() {
            let Some(key) = CfString::new(name) else {
                flags = None;
                break;
            };
            // SAFETY: `session` is a live dictionary and `key` a live string;
            // the value is borrowed from the dictionary, read before it is
            // released, and checked to be a boolean before it is read as one.
            let set = unsafe {
                let value = CFDictionaryGetValue(session, key.0);
                !value.is_null()
                    && CFGetTypeID(value) == CFBooleanGetTypeID()
                    && CFBooleanGetValue(value) != 0
            };
            if let Some(flags) = flags.as_mut() {
                flags[index] = set;
            }
        }
        // SAFETY: owned under the copy rule, released once.
        unsafe { CFRelease(session) };
        flags
    }

    pub(super) fn screen_is_locked() -> Option<bool> {
        session_flags([c"CGSSessionScreenIsLocked"]).map(|[locked]| locked)
    }

    pub(super) fn locked_on_console() -> Option<bool> {
        // `kCGSessionOnConsoleKey` is `CFSTR("kCGSSessionOnConsoleKey")`
        // (`CGSession.h`).
        session_flags([c"CGSSessionScreenIsLocked", c"kCGSSessionOnConsoleKey"])
            .map(|[locked, on_console]| locked && on_console)
    }

    pub(super) fn session_on_console() -> Option<bool> {
        session_flags([c"kCGSSessionOnConsoleKey"]).map(|[on_console]| on_console)
    }

    pub(super) fn login_session_id() -> Option<u64> {
        let mut id = NO_SECURITY_SESSION;
        // SAFETY: `OSStatus SessionGetInfo(SecuritySessionId, SecuritySessionId
        // *, SessionAttributeBits *)`, both `UInt32`; it writes the caller's
        // session id into `id` and nothing through the null attributes
        // pointer, which the header declares nullable.
        let status =
            unsafe { SessionGetInfo(CALLER_SECURITY_SESSION, &raw mut id, std::ptr::null_mut()) };
        (status == 0 && id != NO_SECURITY_SESSION && id != CALLER_SECURITY_SESSION)
            .then_some(u64::from(id))
    }

    pub(super) fn lock_screen() -> Result<(), LockError> {
        // SAFETY: a NUL-terminated path; the private framework sits in the
        // shared cache, and `dlopen` returns null when it is absent.
        let framework = unsafe {
            dlopen(
                c"/System/Library/PrivateFrameworks/login.framework/Versions/Current/login"
                    .as_ptr(),
                RTLD_LAZY,
            )
        };
        if framework.is_null() {
            return Err(LockError::Unavailable);
        }
        // SAFETY: a live handle and a NUL-terminated name.
        let address = unsafe { dlsym(framework, c"SACLockScreenImmediate".as_ptr()) };
        if address.is_null() {
            return Err(LockError::Unavailable);
        }
        // SAFETY: `int SACLockScreenImmediate(void)`, what the system's
        // Lock Screen command calls. Its result is not documented, so the
        // lock is proved by reading the session back instead.
        let lock: extern "C" fn() -> i32 = unsafe { std::mem::transmute(address) };
        let _ = lock();
        Ok(())
    }
}

#[cfg(not(target_os = "macos"))]
mod native {
    use super::LockError;

    pub(super) fn screen_is_locked() -> Option<bool> {
        None
    }

    pub(super) fn locked_on_console() -> Option<bool> {
        None
    }

    pub(super) fn session_on_console() -> Option<bool> {
        None
    }

    pub(super) fn login_session_id() -> Option<u64> {
        None
    }

    pub(super) fn lock_screen() -> Result<(), LockError> {
        Err(LockError::Unavailable)
    }
}
