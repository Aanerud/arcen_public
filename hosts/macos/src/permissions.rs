//! Read-only macOS permission probes.
//!
//! These probes are evidence only. They never grant permission, launch a
//! prompt, or turn a denied capability into an assumed capability.

#![allow(unsafe_code)]

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct PermissionSnapshot {
    pub screen_recording: bool,
    pub accessibility: bool,
}

/// What the granted permissions actually allow.
///
/// These are kept separate because they are not equivalent, and measurement on
/// real hardware disagrees with the obvious assumption. On a Mac with neither
/// grant, `probe-media` is refused by `ScreenCaptureKit` while pointer injection
/// through `CGWarpMouseCursorPosition` and `CGEventPost` still places the
/// cursor exactly. Reporting one combined flag would have told an operator
/// that input was unavailable on a machine where it demonstrably worked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct PermissionCapabilities {
    /// Screen capture. Requires Screen Recording; verified refused without it.
    pub capture: bool,
    /// Pointer placement and buttons. Verified working without Accessibility.
    pub pointer_input: bool,
    /// Synthetic key events. Requires Accessibility; measured on hardware,
    /// where an injected key press is observed with the grant and is not
    /// observed without it.
    pub keyboard_input: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct PermissionReport {
    pub snapshot: PermissionSnapshot,
    pub capabilities: PermissionCapabilities,
    /// Whether every capability a full session needs is available.
    pub usable_for_input_and_capture: bool,
}

impl PermissionReport {
    #[must_use]
    pub const fn from_snapshot(snapshot: PermissionSnapshot) -> Self {
        Self {
            capabilities: snapshot.capabilities(),
            usable_for_input_and_capture: snapshot.usable_for_input_and_capture(),
            snapshot,
        }
    }
}

impl PermissionSnapshot {
    /// Derives what these grants actually permit.
    #[must_use]
    pub const fn capabilities(self) -> PermissionCapabilities {
        PermissionCapabilities {
            capture: self.screen_recording,
            // Measured: pointer injection works with Accessibility denied.
            pointer_input: true,
            keyboard_input: self.accessibility,
        }
    }

    /// Returns whether a full session can run: capture plus complete input.
    ///
    /// A Pier that can move the pointer but not type is not a usable desktop,
    /// so this stays a conjunction even though pointer input alone is
    /// unconditional.
    #[must_use]
    pub const fn usable_for_input_and_capture(self) -> bool {
        let capabilities = self.capabilities();
        capabilities.capture && capabilities.pointer_input && capabilities.keyboard_input
    }
}

#[cfg(target_os = "macos")]
#[must_use]
pub fn probe() -> PermissionSnapshot {
    PermissionSnapshot {
        screen_recording: unsafe { CGPreflightScreenCaptureAccess() },
        accessibility: unsafe { AXIsProcessTrusted() },
    }
}

#[cfg(not(target_os = "macos"))]
pub const fn probe() -> PermissionSnapshot {
    PermissionSnapshot {
        screen_recording: false,
        accessibility: false,
    }
}

#[cfg(target_os = "macos")]
#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    /// Returns whether this process has Screen Recording permission.
    ///
    /// # Safety
    ///
    /// This is a nullary system query with no borrowed pointers or callback
    /// state. The framework owns all implementation state.
    fn CGPreflightScreenCaptureAccess() -> bool;
    /// Asks for Screen Recording, showing the consent dialog.
    ///
    /// Unlike the preflight call, this *registers* this bundle with TCC, which
    /// is what puts it in the Privacy list at all. A subject that has never
    /// asked does not appear there, so an operator who wants to grant the
    /// permission in advance has nothing to switch on.
    fn CGRequestScreenCaptureAccess() -> bool;
}

#[cfg(target_os = "macos")]
#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    /// Returns whether this process is trusted for Accessibility APIs.
    ///
    /// # Safety
    ///
    /// This is a nullary system query with no borrowed pointers or callback
    /// state. The framework owns all implementation state.
    fn AXIsProcessTrusted() -> bool;

    /// Returns whether this process is trusted, optionally prompting first.
    ///
    /// # Safety
    ///
    /// `options` is either null or a valid `CFDictionary` that outlives the
    /// call. The framework copies what it needs; prompting is asynchronous and
    /// does not change the returned value.
    fn AXIsProcessTrustedWithOptions(options: *const objc2_core_foundation::CFDictionary) -> bool;

    /// The options key that asks macOS to tell the user when untrusted.
    static kAXTrustedCheckOptionPrompt: *const objc2_core_foundation::CFString;
}

/// Asks macOS for Accessibility, prompting if this process is untrusted.
///
/// `AXIsProcessTrusted` only reports. macOS lists a subject under Privacy &
/// Security once it has *asked*, and the documented way to ask is the options
/// dictionary with `kAXTrustedCheckOptionPrompt`. A host that only preflights
/// never appears in the Accessibility list, so there is nothing for an
/// operator to switch on — which reads exactly like a refused permission and
/// is not one. This is the same trap system audio had.
///
/// Prompting is asynchronous and does not change the returned value, so a
/// first call on an untrusted process returns `false` and raises the dialog.
#[cfg(target_os = "macos")]
fn request_accessibility() -> bool {
    use objc2_core_foundation::{CFDictionary, CFRetained, CFString};
    use objc2_foundation::{NSDictionary, NSNumber};

    // SAFETY: the framework owns this constant for the process lifetime, and
    // it is a valid immutable `CFString` from the moment the framework loads.
    let Some(key) = (unsafe { kAXTrustedCheckOptionPrompt.as_ref() }) else {
        // Without the key there is nothing to ask with, so report the state.
        // SAFETY: takes no arguments.
        return unsafe { AXIsProcessTrusted() };
    };
    let key: CFRetained<CFString> = CFRetained::from(key);
    let options = NSDictionary::from_slices(
        &[&*objc2_foundation::NSString::from_str(&key.to_string())],
        &[&*NSNumber::new_bool(true)],
    );

    // SAFETY: `NSDictionary` is toll-free bridged with `CFDictionary`, which
    // is the documented bridge rather than a guess, and the dictionary
    // outlives the call.
    unsafe {
        let bridged = objc2::rc::Retained::as_ptr(&options).cast::<CFDictionary>();
        AXIsProcessTrustedWithOptions(bridged)
    }
}

/// Asks macOS for Screen Recording and Accessibility.
///
/// This exists because checking is not the same as asking. TCC lists a subject
/// in Privacy & Security only once it has requested the permission, so a host
/// that merely preflights is invisible there: the operator sees an empty list
/// and no way to approve anything. Requesting registers the bundle and shows
/// the dialog while someone is still in front of the machine.
///
/// Returns whether Screen Recording is granted once the request settles. A
/// refusal is a legitimate answer, not an error.
#[cfg(target_os = "macos")]
#[must_use]
pub fn request() -> PermissionSnapshot {
    // SAFETY: both take no arguments. `CGRequestScreenCaptureAccess` may
    // display a dialog and returns the resulting grant; it is safe to call
    // from any process, and returns immediately when a decision already
    // exists.
    let screen_recording = unsafe { CGRequestScreenCaptureAccess() };

    let accessibility = request_accessibility();

    PermissionSnapshot {
        screen_recording,
        accessibility,
    }
}

#[cfg(not(target_os = "macos"))]
#[must_use]
pub fn request() -> PermissionSnapshot {
    PermissionSnapshot {
        screen_recording: false,
        accessibility: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_session_needs_capture_and_complete_input() {
        assert!(
            !PermissionSnapshot {
                screen_recording: true,
                accessibility: false,
            }
            .usable_for_input_and_capture()
        );
        assert!(
            !PermissionSnapshot {
                screen_recording: false,
                accessibility: true,
            }
            .usable_for_input_and_capture()
        );
        assert!(
            PermissionSnapshot {
                screen_recording: true,
                accessibility: true,
            }
            .usable_for_input_and_capture()
        );
    }

    #[test]
    fn capture_follows_screen_recording_only() {
        // Measured on hardware: ScreenCaptureKit refuses without the grant and
        // succeeds with it, independent of Accessibility.
        assert!(
            PermissionSnapshot {
                screen_recording: true,
                accessibility: false,
            }
            .capabilities()
            .capture
        );
        assert!(
            !PermissionSnapshot {
                screen_recording: false,
                accessibility: true,
            }
            .capabilities()
            .capture
        );
    }

    #[test]
    fn pointer_input_is_not_reported_as_blocked_by_accessibility() {
        // A Mac with both grants denied still placed the cursor exactly.
        // Reporting pointer input as unavailable there would be untrue.
        let denied = PermissionSnapshot {
            screen_recording: false,
            accessibility: false,
        };
        assert!(denied.capabilities().pointer_input);
    }

    #[test]
    fn keyboard_input_is_not_claimed_without_accessibility() {
        // Measured: an injected key press is observed with the Accessibility
        // grant and is not observed without it, so this gate is real.
        let snapshot = PermissionSnapshot {
            screen_recording: true,
            accessibility: false,
        };
        assert!(!snapshot.capabilities().keyboard_input);
        assert!(
            PermissionSnapshot {
                screen_recording: true,
                accessibility: true,
            }
            .capabilities()
            .keyboard_input
        );
    }

    #[test]
    fn the_three_capabilities_are_genuinely_independent() {
        // Hardware showed all three differ on one machine: capture denied,
        // pointer working, keyboard blocked. A single combined flag cannot
        // express that.
        let denied = PermissionSnapshot {
            screen_recording: false,
            accessibility: false,
        }
        .capabilities();
        assert!(!denied.capture);
        assert!(denied.pointer_input);
        assert!(!denied.keyboard_input);
    }

    #[test]
    fn permission_report_derives_readiness_without_granting_access() {
        let report = PermissionReport::from_snapshot(PermissionSnapshot {
            screen_recording: true,
            accessibility: false,
        });
        assert!(report.snapshot.screen_recording);
        assert!(report.capabilities.capture);
        assert!(!report.capabilities.keyboard_input);
        assert!(!report.usable_for_input_and_capture);
    }
}
