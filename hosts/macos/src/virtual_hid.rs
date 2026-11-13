// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Whether this host can present an input device macOS did not get from
//! hardware.
//!
//! Native Tablet sends a Deck's physical pen tablet across the wire and expects
//! the host to make it appear locally, so that the operator's own tablet driver and
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

/// Product id, under the "no vendor" id the virtual keyboard and pointer use,
/// for the probe's pen digitizer.
const PEN_PRODUCT_ID: u32 = 0xA2C2;

/// What macOS made of a virtual pen digitizer.
///
/// Gathered by [`probe_pen`] from the events the HID system produced, not
/// from what was sent, because the question is what an application receives.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct PenHidEvidence {
    /// Why nothing could be measured, when nothing could.
    pub refusal: Option<String>,
    /// Tablet proximity events seen while the probe pen was in range.
    pub proximity_events: u32,
    /// `-[NSEvent capabilityMask]` of the last proximity event: what macOS
    /// claims the pen carries.
    pub capability_mask: i64,
    /// Pointer events with the tablet-point subtype: the events `AppKit`
    /// hands an application, whose `tilt` and `rotation` it reads.
    pub pointer_events: u32,
    /// Largest |tilt|, normalized, carried by those pointer events.
    pub pointer_max_tilt: f64,
    /// Largest rotation, degrees, carried by those pointer events.
    pub pointer_max_rotation: f64,
    /// Standalone tablet-point events (`kCGEventTabletPointer`). The window
    /// server posts these beside the pointer events; an `AppKit` window was
    /// measured not to receive them.
    pub tablet_point_events: u32,
    /// Largest |tilt|, normalized, in the standalone events.
    pub tablet_point_max_tilt: f64,
    /// Largest rotation, degrees, in the standalone events.
    pub tablet_point_max_rotation: f64,
    /// The largest tilt the probe pen sent, normalized as `CoreGraphics`
    /// defines it (1.0 is 90 degrees), to compare the two above against.
    pub sent_max_tilt: f64,
    /// The largest rotation the probe pen sent, degrees.
    pub sent_max_rotation: f64,
}

impl PenHidEvidence {
    /// Whether the measurement saw the device at all.
    #[must_use]
    pub const fn observed(&self) -> bool {
        self.refusal.is_none() && self.proximity_events > 0 && self.pointer_events > 0
    }

    /// Whether the tablet surface reaches applications: the probe pen reported
    /// tilt and rotation on every sample, so pointer events without them mean
    /// the HID system dropped them on the way.
    #[must_use]
    pub fn carries_tablet_surface(&self) -> bool {
        self.observed() && self.pointer_max_tilt > 0.0 && self.pointer_max_rotation > 0.0
    }
}

/// Asks macOS what a generic HID pen digitizer delivers to applications.
///
/// Creates `arcen_input::hid_digitizer::PEN_DESCRIPTOR` as a virtual device,
/// puts the pointer on the main display (macOS maps an absolute digitizer
/// onto whichever display the pointer is on), hovers the pen there with tilt
/// and rotation, and reads back what the HID system posted through a
/// listen-only event tap. Then it takes the pen out of range and puts the
/// pointer back.
///
/// It only ever hovers. A tip on the desktop is a click on whatever is under
/// it, and hover is enough to see whether the surface survives: tilt and
/// rotation are carried in range, touching or not.
///
/// Measured on macOS 26.7.2 with the entitlement: proximity arrives with a
/// capability mask claiming pressure, tilt and rotation; the pointer events
/// an application receives carry none of them; the window server's separate
/// tablet-point events carry tilt and rotation mis-scaled (raw / 32767 and
/// raw / 64) and never pressure, and an `AppKit` window did not receive them.
/// That is why Basic Tablet stays on `CGEvent`. The probe exists so the
/// answer can be re-asked of each macOS.
///
/// Needs what the injector needs — the entitlement in the signature and
/// Accessibility for the app responsible for the process — plus permission
/// to observe events. The device lives until the process exits.
#[cfg(target_os = "macos")]
#[must_use]
pub fn probe_pen() -> PenHidEvidence {
    use arcen_input::hid_digitizer::{PenReport, PenSample};

    let device = match crate::input::virtual_keyboard::native::VirtualDevice::new(
        arcen_input::hid_digitizer::PEN_DESCRIPTOR,
        PEN_PRODUCT_ID,
        "Arcen Pen",
    ) {
        Ok(device) => device,
        Err(reason) => {
            return PenHidEvidence {
                refusal: Some(reason),
                ..PenHidEvidence::default()
            };
        }
    };
    // The HID system attaches a device asynchronously; a report sent at once
    // is dropped, as it is for the keyboard.
    std::thread::sleep(std::time::Duration::from_millis(500));

    let display = objc2_core_graphics::CGDisplayBounds(objc2_core_graphics::CGMainDisplayID());
    let centre = (
        display.origin.x + display.size.width / 2.0,
        display.origin.y + display.size.height / 2.0,
    );
    let original = crate::input::current_pointer_position();

    let observed = std::sync::Mutex::new(PenHidEvidence::default());
    let (ready_sender, ready) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        let observed = &observed;
        scope.spawn(move || tap::observe(observed, &ready_sender));
        if ready.recv().unwrap_or(false) {
            crate::input::warp_pointer(centre);
            std::thread::sleep(std::time::Duration::from_millis(50));
            let mut last = PenReport::default();
            let (mut sent_tilt, mut sent_rotation) = (0.0_f32, 0.0_f32);
            for step in 0..12_u8 {
                let progress = f32::from(step) / 11.0;
                let sample = PenSample {
                    tool: arcen_input::PenTool::Tip,
                    in_proximity: true,
                    touching: false,
                    buttons: 0,
                    x: 0.5 + f64::from(progress) * 0.02,
                    y: 0.5,
                    pressure: 0.0,
                    tilt_x_degrees: 10.0 + 40.0 * progress,
                    tilt_y_degrees: -20.0,
                    rotation_degrees: 30.0 + 200.0 * progress,
                };
                sent_tilt = sent_tilt.max(sample.tilt_x_degrees.abs());
                sent_rotation = sent_rotation.max(sample.rotation_degrees);
                let report = PenReport::from_sample(&sample);
                if device.send(&report.bytes()).is_err() {
                    break;
                }
                last = report;
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            if let Ok(mut evidence) = observed.lock() {
                evidence.sent_max_tilt = arcen_input::pen_axes::tilt_degrees_to_unit(sent_tilt);
                evidence.sent_max_rotation = f64::from(sent_rotation);
            }
            let _ = device.send(&last.released().bytes());
            std::thread::sleep(std::time::Duration::from_millis(100));
            if let Some(position) = original {
                crate::input::warp_pointer(position);
            }
        } else if let Ok(mut evidence) = observed.lock() {
            evidence.refusal = Some(
                "could not observe events: an event tap needs Input Monitoring \
                 (or Accessibility) for the app that started this probe"
                    .to_owned(),
            );
        }
    });
    observed.into_inner().unwrap_or_default()
}

/// Off macOS there is nothing to ask.
#[cfg(not(target_os = "macos"))]
#[must_use]
pub fn probe_pen() -> PenHidEvidence {
    PenHidEvidence {
        refusal: Some("virtual HID is a macOS API".to_owned()),
        ..PenHidEvidence::default()
    }
}

/// A listen-only event tap that records tablet events into [`PenHidEvidence`].
#[cfg(target_os = "macos")]
mod tap {
    #![allow(unsafe_code)]

    use std::ffi::c_void;
    use std::sync::Mutex;

    use super::PenHidEvidence;

    type CGEventRef = *mut c_void;
    type CFMachPortRef = *mut c_void;
    type CFRunLoopSourceRef = *mut c_void;
    type CFRunLoopRef = *mut c_void;
    type CFStringRef = *const c_void;
    type TapCallback =
        unsafe extern "C" fn(*mut c_void, u32, CGEventRef, *mut c_void) -> CGEventRef;

    const SESSION_EVENT_TAP: u32 = 1;
    const HEAD_INSERT: u32 = 0;
    const LISTEN_ONLY: u32 = 1;
    const MOUSE_MOVED: u32 = 5;
    const LEFT_MOUSE_DRAGGED: u32 = 6;
    const TABLET_POINTER: u32 = 23;
    const TABLET_PROXIMITY: u32 = 24;
    /// `kCGMouseEventSubtype`, and its tablet-point value.
    const SUBTYPE_FIELD: u32 = 7;
    const SUBTYPE_TABLET_POINT: i64 = 1;
    const TILT_X: u32 = 20;
    const TILT_Y: u32 = 21;
    const ROTATION: u32 = 22;
    const CAPABILITY_MASK: u32 = 36;
    /// How long the tap listens: the probe's samples take about a third of it.
    const LISTEN_SECONDS: f64 = 1.5;

    // SAFETY: signatures match `CGEvent.h`, `CGEventTypes.h`, `CFMachPort.h`
    // and `CFRunLoop.h`; every reference is an opaque pointer. None of these
    // symbols is declared anywhere else in this crate.
    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        fn CGEventTapCreate(
            tap: u32,
            place: u32,
            options: u32,
            events_of_interest: u64,
            callback: TapCallback,
            user_info: *mut c_void,
        ) -> CFMachPortRef;
        fn CGEventTapEnable(tap: CFMachPortRef, enable: bool);
        fn CGEventGetIntegerValueField(event: CGEventRef, field: u32) -> i64;
        fn CGEventGetDoubleValueField(event: CGEventRef, field: u32) -> f64;
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        static kCFRunLoopDefaultMode: CFStringRef;
        fn CFMachPortCreateRunLoopSource(
            allocator: *const c_void,
            port: CFMachPortRef,
            order: isize,
        ) -> CFRunLoopSourceRef;
        fn CFMachPortInvalidate(port: CFMachPortRef);
        fn CFRunLoopGetCurrent() -> CFRunLoopRef;
        fn CFRunLoopAddSource(
            run_loop: CFRunLoopRef,
            source: CFRunLoopSourceRef,
            mode: CFStringRef,
        );
        fn CFRunLoopRunInMode(mode: CFStringRef, seconds: f64, return_after_source: u8) -> i32;
    }

    unsafe extern "C" fn record(
        _proxy: *mut c_void,
        event_type: u32,
        event: CGEventRef,
        user_info: *mut c_void,
    ) -> CGEventRef {
        // SAFETY: `user_info` is the `Mutex` `observe` passed in, which
        // outlives the run loop this callback runs on.
        let observed = unsafe { &*user_info.cast::<Mutex<PenHidEvidence>>() };
        let Ok(mut evidence) = observed.lock() else {
            return event;
        };
        // SAFETY: `event` is live for the duration of the callback, and these
        // only read its fields.
        unsafe {
            if event_type == TABLET_PROXIMITY {
                evidence.proximity_events += 1;
                evidence.capability_mask = CGEventGetIntegerValueField(event, CAPABILITY_MASK);
                return event;
            }
            let tilt = CGEventGetDoubleValueField(event, TILT_X)
                .abs()
                .max(CGEventGetDoubleValueField(event, TILT_Y).abs());
            let rotation = CGEventGetDoubleValueField(event, ROTATION);
            if event_type == TABLET_POINTER {
                evidence.tablet_point_events += 1;
                evidence.tablet_point_max_tilt = evidence.tablet_point_max_tilt.max(tilt);
                evidence.tablet_point_max_rotation =
                    evidence.tablet_point_max_rotation.max(rotation);
            } else if CGEventGetIntegerValueField(event, SUBTYPE_FIELD) == SUBTYPE_TABLET_POINT {
                evidence.pointer_events += 1;
                evidence.pointer_max_tilt = evidence.pointer_max_tilt.max(tilt);
                evidence.pointer_max_rotation = evidence.pointer_max_rotation.max(rotation);
            }
        }
        event
    }

    /// Listens for [`LISTEN_SECONDS`], telling `ready` whether it could.
    pub(super) fn observe(observed: &Mutex<PenHidEvidence>, ready: &std::sync::mpsc::Sender<bool>) {
        let mask = [
            MOUSE_MOVED,
            LEFT_MOUSE_DRAGGED,
            TABLET_POINTER,
            TABLET_PROXIMITY,
        ]
        .iter()
        .fold(0_u64, |mask, kind| mask | 1 << kind);
        // SAFETY: a listen-only tap with a callback that only reads; the
        // user-info pointer is valid for as long as this function runs, which
        // is as long as the run loop that calls back.
        let port = unsafe {
            CGEventTapCreate(
                SESSION_EVENT_TAP,
                HEAD_INSERT,
                LISTEN_ONLY,
                mask,
                record,
                std::ptr::from_ref(observed).cast_mut().cast(),
            )
        };
        if port.is_null() {
            let _ = ready.send(false);
            return;
        }
        // SAFETY: `port` is a live tap; the source and the run loop are this
        // thread's, and both references are released before returning.
        unsafe {
            let source = CFMachPortCreateRunLoopSource(std::ptr::null(), port, 0);
            CFRunLoopAddSource(CFRunLoopGetCurrent(), source, kCFRunLoopDefaultMode);
            CGEventTapEnable(port, true);
            let _ = ready.send(true);
            CFRunLoopRunInMode(kCFRunLoopDefaultMode, LISTEN_SECONDS, 0);
            CGEventTapEnable(port, false);
            CFMachPortInvalidate(port);
            crate::input::release_core_foundation(source.cast_const());
            crate::input::release_core_foundation(port.cast_const());
        }
    }
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
