//! Whether this host can present an input device macOS did not get from
//! hardware.
//!
//! Native Tablet sends a Deck's physical Wacom across the wire and expects the
//! host to make it appear locally, so that the operator's own Wacom driver and
//! every pressure-aware application see a real tablet. On macOS the route for
//! that is `IOHIDUserDevice`: public SDK API, in IOKit's public module map,
//! available since 10.15, needing no DriverKit extension, no system extension,
//! no approval dialog and no reboot.
//!
//! It needs one entitlement. The header is explicit, at
//! `IOKit.framework/Headers/hidsystem/IOHIDUserDevice.h`:
//!
//! > In order to create the device, the entitlement
//! > `"com.apple.developer.hid.virtual.device"` is required to validate the
//! > source of the device.
//!
//! It also needs **Accessibility**. The kernel's `IOHIDResourceDeviceUserClient`
//! refuses a signed, entitled, notarized process whose responsible app has no
//! Accessibility grant, logging only `failed MACF`, and macOS then raises
//! "Accessibility Access (Events)" for that app. Measured on macOS 26.5.2: the
//! same bundle was refused over SSH (attributed to `sshd-keygen-wrapper`) and
//! worked once launched as its own app with the grant. A probe run from a
//! terminal therefore answers for the terminal, not for the Pier.
//!
//! This module does not assume the answer. It asks.
//!
//! # Why it asks instead of reading a build flag
//!
//! An entitlement is granted to a signing identity, not compiled in, so the
//! same binary is authorised on one machine and refused on another. A constant
//! would therefore be wrong half the time, and wrong in the expensive
//! direction: a host that advertises Native Tablet it cannot deliver takes the
//! Deck's tablet away from its owner and gives back nothing.
//!
//! The previous code hardcoded the refusal and named
//! `IOUSBHostControllerInterface` and `com.apple.developer.usb.host-controller-interface`
//! as the reason. Both were wrong — that is a synthetic USB host controller,
//! a different and more heavily gated thing — so the host was refusing for a
//! reason that would never become true.
//!
//! # Why asking is safe
//!
//! Requesting an entitlement the signature does not carry usually means AMFI
//! terminates the process before `main`. This one does not: creation returns
//! null and the process continues. Measured on two Macs with an unentitled
//! probe, as an ordinary user and under `sudo`:
//!
//! ```text
//! uid=501 create=0x0 errno=0
//! uid=0   create=0x0 errno=0
//! ```
//!
//! Two things follow. Root is not a substitute, so there is no version of this
//! that ships before Apple answers. And the probe costs nothing worse than a
//! null, so the host can ask at startup and report the real reason rather than
//! a guess.

/// What this host can do about presenting a virtual input device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VirtualHidSupport {
    /// A virtual device was created and torn down, so the real one will work.
    Available,
    /// The API is present but refused this process.
    ///
    /// Overwhelmingly this is the missing entitlement, but the API returns a
    /// bare null with no error, so the host says what it observed and names
    /// the likely cause rather than asserting one it cannot see.
    Refused,
}

impl VirtualHidSupport {
    /// Whether Native Tablet may be advertised to a Deck.
    #[must_use]
    pub const fn is_available(&self) -> bool {
        matches!(self, Self::Available)
    }

    /// Why Native Tablet is unavailable, in words a Deck can show its user.
    #[must_use]
    pub const fn refusal(&self) -> Option<&'static str> {
        match self {
            Self::Available => None,
            Self::Refused => Some(
                "this host cannot create a virtual HID device; it needs both the \
                 com.apple.developer.hid.virtual.device entitlement in its signature \
                 and Accessibility approval for the app that started it",
            ),
        }
    }
}

/// The smallest descriptor macOS will accept, used only to ask the question.
///
/// A digitizer collection with one button, padded to a byte. It is deliberately
/// not the tablet descriptor: this probe exists to learn whether creation is
/// permitted, and a device that briefly appears to the whole system should be
/// as uninteresting as possible. The real descriptor is built per attachment
/// from the bridged device's own report descriptor.
const PROBE_DESCRIPTOR: &[u8] = &[
    0x05, 0x0D, // Usage Page (Digitizers)
    0x09, 0x02, // Usage (Pen)
    0xA1, 0x01, // Collection (Application)
    0x85, 0x01, //   Report ID (1)
    0x09, 0x20, //   Usage (Stylus)
    0xA1, 0x00, //   Collection (Physical)
    0x09, 0x42, //     Usage (Tip Switch)
    0x15, 0x00, //     Logical Minimum (0)
    0x25, 0x01, //     Logical Maximum (1)
    0x75, 0x01, //     Report Size (1)
    0x95, 0x01, //     Report Count (1)
    0x81, 0x02, //     Input (Data,Var,Abs)
    0x95, 0x07, //     Report Count (7)
    0x81, 0x03, //     Input (Cnst,Var,Abs) — padding to a byte
    0xC0, //   End Collection
    0xC0, // End Collection
];

/// Asks macOS whether this process may create a virtual HID device.
///
/// Creates one and immediately tears it down. Nothing is published to the user:
/// the device exists for the duration of this call and carries no reports.
#[cfg(target_os = "macos")]
#[must_use]
pub fn probe() -> VirtualHidSupport {
    use objc2_core_foundation::CFDictionary;
    use objc2_foundation::{NSData, NSDictionary, NSString};

    // SAFETY: `IOHIDUserDeviceCreateWithProperties` and `IOHIDUserDeviceCancel`
    // are public IOKit symbols, declared in the public module map at
    // `hidsystem/IOHIDUserDevice.h` and exported from `IOKit.tbd`. The
    // signatures below match that header: a nullable allocator, a non-null
    // property dictionary, options, returning a nullable retained reference.
    unsafe extern "C" {
        fn IOHIDUserDeviceCreateWithProperties(
            allocator: *const std::ffi::c_void,
            properties: *const CFDictionary,
            options: u32,
        ) -> *mut std::ffi::c_void;
    }

    // `kIOHIDReportDescriptorKey` is `"ReportDescriptor"`, spelled out rather
    // than linked because IOKit exports it as a C macro, not a symbol.
    let properties = NSDictionary::from_slices(
        &[&*NSString::from_str("ReportDescriptor")],
        &[&*NSData::with_bytes(PROBE_DESCRIPTOR)],
    );

    // SAFETY: `NSDictionary` is toll-free bridged with `CFDictionary`, which is
    // the documented bridge rather than a guess, and `properties` outlives the
    // call. A null allocator selects the default one, as the header documents.
    // The returned pointer is owned here: null means refusal, non-null must be
    // cancelled and released.
    let device = unsafe {
        let bridged = objc2::rc::Retained::as_ptr(&properties).cast::<CFDictionary>();
        IOHIDUserDeviceCreateWithProperties(std::ptr::null(), bridged, 0)
    };

    if device.is_null() {
        return VirtualHidSupport::Refused;
    }

    // SAFETY: `device` is non-null and was returned by the create call above,
    // so it is a valid `IOHIDUserDeviceRef` this scope owns. Released, not
    // cancelled: `IOHIDUserDeviceCancel` is for a device that was scheduled on
    // a queue and activated, and on one that never was it aborts the process
    // ("Unschedule failed queue") — measured the first time this probe ran
    // with the entitlement present. Releasing the last reference closes the
    // user client, which removes the kernel device.
    //
    // Release goes through `crate::input::release_core_foundation` rather than
    // a second `CFRelease` declaration. The crate already declares that symbol
    // once, and two declarations of one symbol that disagree about their
    // signature is undefined behaviour rather than an inconsistency.
    unsafe {
        crate::input::release_core_foundation(device.cast_const());
    }
    VirtualHidSupport::Available
}

/// Every other operating system refuses, because this is a macOS API.
///
/// Present so the rest of the host compiles and behaves identically off macOS,
/// rather than making every call site carry a `cfg`.
#[cfg(not(target_os = "macos"))]
#[must_use]
pub fn probe() -> VirtualHidSupport {
    VirtualHidSupport::Refused
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refusal_explains_itself() {
        // A Deck shows this to somebody whose tablet stopped working, so it
        // has to name the cause rather than say "unavailable".
        let refusal = VirtualHidSupport::Refused.refusal().expect("a reason");
        assert!(
            refusal.contains("com.apple.developer.hid.virtual.device"),
            "the entitlement has to be named: {refusal}",
        );
        assert!(!VirtualHidSupport::Refused.is_available());
    }

    #[test]
    fn availability_carries_no_excuse() {
        assert!(VirtualHidSupport::Available.is_available());
        assert_eq!(VirtualHidSupport::Available.refusal(), None);
    }

    #[test]
    fn the_probe_descriptor_is_a_well_formed_digitizer() {
        // A malformed descriptor is refused by macOS for its own reasons, which
        // would read as a missing entitlement and send somebody looking in the
        // wrong place entirely.
        assert_eq!(
            &PROBE_DESCRIPTOR[..4],
            &[0x05, 0x0D, 0x09, 0x02],
            "must open as a digitizer pen usage",
        );
        assert_eq!(
            PROBE_DESCRIPTOR.last(),
            Some(&0xC0),
            "collections must be closed",
        );
        let opens = PROBE_DESCRIPTOR
            .windows(2)
            .filter(|pair| pair[0] == 0xA1)
            .count();
        let closes = PROBE_DESCRIPTOR
            .iter()
            .fold(0_usize, |total, &byte| total + usize::from(byte == 0xC0));
        assert_eq!(
            opens, closes,
            "every collection must be closed exactly once"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn probing_does_not_kill_this_process() {
        // The point of the test: asking for an entitlement the signature does
        // not carry normally means AMFI terminates the process before `main`.
        // This call does not, which is what lets the host report a real reason
        // instead of refusing on principle. If that ever changes, this test
        // stops the whole suite rather than one session in the field.
        let support = probe();
        assert!(
            matches!(
                support,
                VirtualHidSupport::Available | VirtualHidSupport::Refused
            ),
            "the probe must answer rather than abort",
        );
    }
}
