// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Keeping the Mac awake while it serves, and waking its displays when a Deck
//! arrives.
//!
//! The Linux Pier holds a logind `sleep:idle` inhibitor and the Windows Pier
//! `SetThreadExecutionState` for the life of a desktop; this is the macOS
//! counterpart through `IOKit` power assertions. A session's own input already
//! counts as user activity, but a Deck that only watches (a build log, a
//! video) sends none, and an idle Mac would then sleep under it.

/// Tells the power manager a person is using the Mac, which wakes sleeping
/// displays: a sleeping display is not an active one, and the agent would
/// otherwise find nothing to serve (`inventory is empty`). Returns whether the
/// power manager took it.
#[must_use]
pub fn declare_user_activity() -> bool {
    native::declare_user_activity()
}

/// An `IOPMAssertion` that keeps the Mac from idle sleep while it is held;
/// released on drop. The display may still sleep by the user's settings
/// (a remote session is not looking at it); the machine does not.
#[derive(Debug)]
pub struct SleepAssertion {
    id: Option<u32>,
}

impl SleepAssertion {
    /// Prevents idle sleep until dropped. Holds nothing when the power manager
    /// refuses, which is logged by the caller as a degraded session, not a
    /// refused one.
    #[must_use]
    pub fn hold(reason: &str) -> Self {
        Self {
            id: native::prevent_idle_sleep(reason),
        }
    }

    /// Whether the power manager granted the assertion.
    #[must_use]
    pub const fn is_held(&self) -> bool {
        self.id.is_some()
    }
}

impl Drop for SleepAssertion {
    fn drop(&mut self) {
        if let Some(id) = self.id.take() {
            native::release(id);
        }
    }
}

#[cfg(target_os = "macos")]
mod native {
    // IOKit power-management entry points, each with its SAFETY note below.
    #![allow(unsafe_code)]

    use std::ffi::{c_char, c_void};

    type CFTypeRef = *const c_void;

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFStringCreateWithCString(
            allocator: CFTypeRef,
            text: *const c_char,
            encoding: u32,
        ) -> CFTypeRef;
        fn CFRelease(value: CFTypeRef);
    }

    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IOPMAssertionDeclareUserActivity(
            name: CFTypeRef,
            user_type: u32,
            assertion: *mut u32,
        ) -> i32;
        fn IOPMAssertionCreateWithName(
            assertion_type: CFTypeRef,
            level: u32,
            name: CFTypeRef,
            assertion: *mut u32,
        ) -> i32;
        fn IOPMAssertionRelease(assertion: u32) -> i32;
    }

    const UTF8: u32 = 0x0800_0100;
    /// `kIOPMUserActiveLocal`.
    const USER_ACTIVE_LOCAL: u32 = 0;
    /// `kIOPMAssertionLevelOn`.
    const LEVEL_ON: u32 = 255;

    /// An owned Core Foundation string, released on drop.
    struct CfString(CFTypeRef);

    impl CfString {
        fn new(text: &str) -> Option<Self> {
            let text = std::ffi::CString::new(text).ok()?;
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

    pub(super) fn declare_user_activity() -> bool {
        let Some(name) = CfString::new("Arcen Deck connecting") else {
            return false;
        };
        let mut assertion = 0_u32;
        // SAFETY: a live name and a writable id. A declared activity lasts
        // as long as the user's display-sleep setting and then lapses by
        // itself, so the id is not kept.
        let status = unsafe {
            IOPMAssertionDeclareUserActivity(name.0, USER_ACTIVE_LOCAL, &raw mut assertion)
        };
        status == 0
    }

    pub(super) fn prevent_idle_sleep(reason: &str) -> Option<u32> {
        // `kIOPMAssertPreventUserIdleSystemSleep`.
        let kind = CfString::new("PreventUserIdleSystemSleep")?;
        let name = CfString::new(reason)?;
        let mut assertion = 0_u32;
        // SAFETY: live strings and a writable id; the id is released once,
        // by `release`, when the holder drops.
        let status =
            unsafe { IOPMAssertionCreateWithName(kind.0, LEVEL_ON, name.0, &raw mut assertion) };
        (status == 0).then_some(assertion)
    }

    pub(super) fn release(assertion: u32) {
        // SAFETY: an id `prevent_idle_sleep` created and nobody released.
        let _ = unsafe { IOPMAssertionRelease(assertion) };
    }
}

#[cfg(not(target_os = "macos"))]
mod native {
    pub(super) fn declare_user_activity() -> bool {
        false
    }

    pub(super) fn prevent_idle_sleep(_reason: &str) -> Option<u32> {
        None
    }

    pub(super) fn release(_assertion: u32) {}
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn a_held_assertion_is_granted_and_given_back() {
        // Holds idle sleep off for the length of the test only.
        let assertion = SleepAssertion::hold("Arcen Pier test");
        assert!(assertion.is_held());
        drop(assertion);
    }
}
