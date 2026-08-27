//! AppKit local event monitor for typed tablet capture (macOS only).
//!
//! This is the only file in `clients/macos/src/tablet/` that touches AppKit
//! or contains `unsafe` code. Everything it produces funnels through the
//! pure types in [`super::sample`] so the rest of the module (mapper, probe)
//! never needs `unsafe` or a live `NSApplication` to be tested.
//!
//! Design constraints from the Wacom local-termination plan:
//! - **Main-thread/AppKit lifecycle**: constructing and dropping the monitor
//!   requires proof of the main thread via `MainThreadMarker`
//!   (`objc2_foundation`). AppKit event monitors must be installed/removed
//!   from the main thread; the marker makes that a compile-time-checked
//!   precondition rather than a runtime assumption.
//! - **Bounded delivery**: the monitor callback runs synchronously on the
//!   main thread as part of AppKit's event dispatch, so it must never block
//!   or grow unbounded memory. Every decoded sample goes through
//!   [`super::sample::BoundedSampleQueue`], which drops the oldest sample
//!   and counts the drop instead of blocking or growing without limit.
//! - **No coordinate/sample logging**: only lifecycle events (installed,
//!   removed, queue-full drop *counts*) are logged; raw coordinates,
//!   pressure, tilt, rotation, or button values are never passed to
//!   `tracing`.
//! - **FFI safety**: every `unsafe` block below has a `SAFETY` comment
//!   justifying it individually; getter calls on a live `&NSEvent` (e.g.
//!   `pressure()`, `tilt()`, `buttonMask()`) are safe Objective-C message
//!   sends already covered by `objc2-app-kit`'s bindings and require no
//!   additional `unsafe` beyond dereferencing the event pointer itself.

use std::ptr::NonNull;
use std::sync::{Arc, Mutex};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_app_kit::{NSEvent, NSEventMask, NSEventSubtype, NSEventType};
use objc2_foundation::MainThreadMarker;

use super::sample::{
    BoundedSampleQueue, NativeTabletPoint, NativeTabletProximity, NativeTabletTool,
    RawTabletSample, TabletButtonMask,
};

/// Bounded queue depth. Tablet point events can arrive well over 100 Hz;
/// 256 gives the consumer several frames of slack at typical UI poll rates
/// without letting an unresponsive consumer grow memory without bound.
const DEFAULT_QUEUE_CAPACITY: usize = 256;

struct Shared {
    queue: Mutex<BoundedSampleQueue<RawTabletSample>>,
}

/// A `Send + Sync` handle to the bounded sample queue, obtainable from the
/// main-thread-only [`TabletEventMonitor`] and usable from any thread to
/// drain samples. Kept separate from the monitor itself so a consumer does
/// not need to be main-thread-bound just to read queued samples.
#[derive(Clone)]
pub struct TabletSampleHandle {
    shared: Arc<Shared>,
}

impl TabletSampleHandle {
    /// Drain every currently queued sample, oldest first. Never blocks the
    /// AppKit callback for longer than the lock hold time of a `Vec` drain.
    pub fn drain(&self) -> Vec<RawTabletSample> {
        self.shared
            .queue
            .lock()
            .map(|mut queue| queue.drain())
            .unwrap_or_default()
    }

    /// Number of samples dropped so far because the bounded queue was full
    /// and the consumer had not drained it in time.
    #[must_use]
    pub fn dropped_count(&self) -> u64 {
        self.shared
            .queue
            .lock()
            .map(|queue| queue.dropped_count())
            .unwrap_or_default()
    }
}

/// Main-thread-owned RAII guard around an AppKit local tablet event monitor.
///
/// Construction requires a [`MainThreadMarker`], proving the caller is on
/// AppKit's main thread — the only thread `NSEvent` local monitors may be
/// installed or removed from. `MainThreadMarker` is itself `!Send`, so this
/// guard cannot be moved to another thread by accident; it must be created,
/// held, and dropped on the main thread, matching AppKit's lifecycle
/// requirement without relying on a runtime check alone.
pub struct TabletEventMonitor {
    monitor: Retained<AnyObject>,
    _mtm: MainThreadMarker,
}

impl TabletEventMonitor {
    /// Install the local tablet-point/tablet-proximity event monitor.
    ///
    /// Returns `None` if AppKit refuses to install the monitor (rare; e.g.
    /// no run loop yet). Returns the monitor guard plus a cloneable
    /// [`TabletSampleHandle`] for draining decoded samples.
    #[must_use]
    pub fn start(mtm: MainThreadMarker) -> Option<(Self, TabletSampleHandle)> {
        Self::start_with_capacity(mtm, DEFAULT_QUEUE_CAPACITY)
    }

    #[must_use]
    pub fn start_with_capacity(
        mtm: MainThreadMarker,
        capacity: usize,
    ) -> Option<(Self, TabletSampleHandle)> {
        let shared = Arc::new(Shared {
            queue: Mutex::new(BoundedSampleQueue::new(capacity)),
        });
        let callback_shared = Arc::clone(&shared);

        // The handler block runs synchronously on the main thread inside
        // AppKit's event dispatch for every matched event. It must return the
        // event unchanged (`Some`/passthrough) because this is a local
        // *observer*, not a filter: swallowing tablet events here would also
        // suppress the mouse-emulation duplicates AppKit generates alongside
        // them. Suppressing those duplicates while a pen owns input is done
        // downstream in `ui/app.rs`, not by filtering here.
        let handler = RcBlock::new(move |event: NonNull<NSEvent>| -> *mut NSEvent {
            // SAFETY: `addLocalMonitorForEventsMatchingMask:handler:`
            // guarantees the block is invoked synchronously, on the main
            // thread, with a live, non-null `NSEvent` valid for the duration
            // of the call. We only borrow it (`as_ref`) to read scalar
            // fields through safe `objc2-app-kit` getters; we never retain,
            // store, or use the pointer after this closure returns, and we
            // return the same pointer unchanged so AppKit continues normal
            // dispatch.
            let event_ref = unsafe { event.as_ref() };
            // `catch_unwind` guarantees a panic decoding/queuing one event
            // can never unwind across this Objective-C block boundary and
            // into AppKit's dispatch machinery — it is caught, logged, and
            // the sample is dropped instead, so a single bad event degrades
            // capture rather than aborting or corrupting the process.
            let outcome =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| decode_event(event_ref)));
            match outcome {
                Ok(Some(sample)) => {
                    // A poisoned mutex (panic while a sample was queued) still
                    // has valid queue contents; recovering here keeps capture
                    // alive instead of permanently losing tablet input over one
                    // panicking consumer.
                    let mut queue = callback_shared
                        .queue
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    queue.push(sample);
                }
                Ok(None) => {}
                Err(_) => {
                    tracing::error!(
                        target: crate::logging::target::INPUT,
                        "AppKit tablet event decode panicked; sample dropped",
                    );
                }
            }
            event.as_ptr()
        });

        // SAFETY: `handler` is an `RcBlock` whose closure only touches
        // `'static` data (`Arc<Shared>`) plus its by-value `NonNull<NSEvent>`
        // argument; it performs no long-lived borrow of anything with a
        // shorter lifetime. `addLocalMonitorForEventsMatchingMask_handler`'s
        // safety contract is that the handler must return a valid pointer or
        // null, which it does (the unmodified input event pointer).
        //
        // The mask includes both standalone tablet events (NSEventTypeTabletPoint,
        // NSEventTypeTabletProximity) AND every mouse/drag event type.  Wacom and
        // other tablet vendors on macOS normally deliver pen data as mouse events
        // with a TabletPoint or TabletProximity *subtype*, not as standalone tablet
        // event types. Listening only to the standalone types misses essentially
        // all real-world pen motion and proximity events from an attached Wacom.
        let tablet_mouse_mask = NSEventMask::LeftMouseDown
            | NSEventMask::LeftMouseUp
            | NSEventMask::RightMouseDown
            | NSEventMask::RightMouseUp
            | NSEventMask::MouseMoved
            | NSEventMask::LeftMouseDragged
            | NSEventMask::RightMouseDragged
            | NSEventMask::OtherMouseDown
            | NSEventMask::OtherMouseUp
            | NSEventMask::OtherMouseDragged;
        let monitor = unsafe {
            NSEvent::addLocalMonitorForEventsMatchingMask_handler(
                NSEventMask::TabletPoint | NSEventMask::TabletProximity | tablet_mouse_mask,
                &handler,
            )
        };

        let monitor = monitor?;
        tracing::info!(
            target: crate::logging::target::INPUT,
            "AppKit tablet event monitor installed",
        );
        Some((Self { monitor, _mtm: mtm }, TabletSampleHandle { shared }))
    }
}

impl Drop for TabletEventMonitor {
    fn drop(&mut self) {
        // SAFETY: `self.monitor` is exactly the opaque token
        // `addLocalMonitorForEventsMatchingMask_handler` returned to this
        // guard and has not been passed to `removeMonitor` before (this
        // `Drop` impl runs at most once per guard). `removeMonitor:`
        // requires the token be "of the correct type", which it is: it is
        // the unmodified `Retained<AnyObject>` AppKit gave us.
        unsafe {
            NSEvent::removeMonitor(&self.monitor);
        }
        tracing::info!(
            target: crate::logging::target::INPUT,
            "AppKit tablet event monitor removed",
        );
    }
}

/// Decode a live `NSEvent` into a pure native sample. Handles both
/// standalone tablet events (`NSEventTypeTabletPoint`,
/// `NSEventTypeTabletProximity`) and mouse/drag events with a tablet subtype
/// (`NSEventSubtype::TabletPoint`, `NSEventSubtype::TabletProximity`).
///
/// Wacom and most other tablet drivers on macOS deliver pen input primarily
/// as mouse events with a tablet subtype, not as standalone tablet event
/// types. The standalone types may still appear in some driver configurations,
/// so both paths are handled. Every getter call here is a safe Objective-C
/// message send (`objc2-app-kit` marks them as safe `fn`s); `subtype()` is
/// `unsafe` and is guarded individually below.
fn decode_event(event: &NSEvent) -> Option<RawTabletSample> {
    let location = event.locationInWindow();
    let kind = event.r#type();

    // Determine whether this is a tablet event — either by top-level type or
    // by the subtype field carried inside a mouse event.
    let is_mouse_event = matches!(
        kind,
        NSEventType::LeftMouseDown
            | NSEventType::LeftMouseUp
            | NSEventType::RightMouseDown
            | NSEventType::RightMouseUp
            | NSEventType::MouseMoved
            | NSEventType::LeftMouseDragged
            | NSEventType::RightMouseDragged
            | NSEventType::OtherMouseDown
            | NSEventType::OtherMouseUp
            | NSEventType::OtherMouseDragged
    );

    // When the top-level type is a mouse event, read the subtype to find
    // embedded tablet data.
    // SAFETY: `event` is a live, non-null `NSEvent` valid for the duration
    // of this call (guaranteed by the `addLocalMonitorForEventsMatchingMask`
    // contract). `subtype()` is an unsafe getter because AppKit's docs only
    // guarantee it is meaningful on certain event types; we only call it
    // here after confirming the event type is one of the mouse/drag types
    // that the Wacom driver uses to carry tablet data, matching the AppKit
    // documentation for `NSEvent.subtype`.
    #[allow(unused_unsafe)]
    let effective_tablet_kind = if kind == NSEventType::TabletPoint {
        Some(NSEventType::TabletPoint)
    } else if kind == NSEventType::TabletProximity {
        Some(NSEventType::TabletProximity)
    } else if is_mouse_event {
        let subtype = unsafe { event.subtype() };
        if subtype == NSEventSubtype::TabletPoint {
            Some(NSEventType::TabletPoint)
        } else if subtype == NSEventSubtype::TabletProximity {
            Some(NSEventType::TabletProximity)
        } else {
            // Some Wacom driver/runtime combinations deliver pen motion as
            // ordinary mouse/drag events with Pen/Eraser device type but
            // without a TabletPoint subtype. Accept those as tablet-point
            // samples so motion/pressure reaches the host. Trackpad-origin
            // tablet-shaped events still fail closed because their pointing
            // device type is Unknown/Cursor (never Pen/Eraser).
            let tool =
                NativeTabletTool::from_ns_pointing_device_type(event.pointingDeviceType().0 as u64);
            if tool.is_pen_or_eraser() {
                Some(NSEventType::TabletPoint)
            } else {
                None
            }
        }
    } else {
        None
    };

    let effective_kind = match effective_tablet_kind {
        Some(kind) => kind,
        None => return None,
    };

    // Resolve the effective tool type from NSEvent.pointingDeviceType.
    //
    // Wacom on macOS delivers pen motion as mouse/drag events whose
    // pointingDeviceType and vendorID are BOTH 0 — AppKit does not populate
    // those fields reliably for tablet-subtype events on all driver versions.
    // The only thing that reliably identifies Apple trackpads (the false-
    // positive we must reject) is vendor ID 0x05AC (Apple). Every other
    // Unknown-tool tablet event that passed the effective_kind gate above
    // (subtype TabletPoint/TabletProximity, or pen-typed drag) is pen input.
    //
    // Gate logic:
    //   1. If pointingDeviceType is Pen/Eraser — always accept.
    //   2. If vendor_id is Apple (0x05AC) — always reject (trackpad).
    //   3. Otherwise (vendor_id = 0 or known tablet vendor, type = Unknown)
    //      — promote to Pen; the event already passed the effective_kind gate.
    const APPLE_VENDOR_ID: u64 = 0x05AC;
    let vendor_id = event.vendorID() as u64;
    let pointing_device_type = event.pointingDeviceType().0 as u64;
    let raw_type = NativeTabletTool::from_ns_pointing_device_type(pointing_device_type);
    let tool = if raw_type.is_pen_or_eraser() {
        raw_type
    } else if vendor_id == APPLE_VENDOR_ID {
        // Apple trackpad emitting tablet-shaped event — reject.
        tracing::debug!(
            target: crate::logging::target::INPUT,
            vendor_id,
            pointing_device_type,
            effective_kind = ?effective_kind,
            "tablet sample blocked: Apple vendor (trackpad)",
        );
        return None;
    } else {
        // Unknown tool type, non-Apple vendor (or vendor_id=0 which is normal
        // for Wacom driver on macOS). Promote to Pen — the effective_kind gate
        // already confirmed this is a tablet-subtype or standalone event.
        tracing::debug!(
            target: crate::logging::target::INPUT,
            vendor_id,
            pointing_device_type,
            effective_kind = ?effective_kind,
            "tablet sample promoted to Pen (Unknown tool, non-Apple vendor)",
        );
        NativeTabletTool::Pen
    };
    if effective_kind == NSEventType::TabletPoint {
        tracing::debug!(
            target: crate::logging::target::INPUT,
            tool = ?tool,
            device_id = event.deviceID() as u64,
            vendor_id = vendor_id,
            pointing_device_type,
            "decoded TabletPoint sample from AppKit",
        );
        Some(RawTabletSample::Point(NativeTabletPoint {
            window_x: location.x,
            window_y: location.y,
            pressure: event.pressure(),
            tilt_x: event.tilt().x as f32,
            tilt_y: event.tilt().y as f32,
            rotation_degrees: event.rotation(),
            buttons: TabletButtonMask::new(truncate_bits(event.buttonMask().0)),
            device_id: event.deviceID() as u64,
            tool,
            window_number: event.windowNumber(),
        }))
    } else {
        tracing::debug!(
            target: crate::logging::target::INPUT,
            tool = ?tool,
            entering = event.isEnteringProximity(),
            device_id = event.deviceID() as u64,
            vendor_id = vendor_id,
            pointing_device_type,
            "decoded TabletProximity sample from AppKit",
        );
        Some(RawTabletSample::Proximity(NativeTabletProximity {
            window_x: location.x,
            window_y: location.y,
            entering: event.isEnteringProximity(),
            tool,
            vendor_id,
            tablet_id: event.tabletID() as u64,
            pointing_device_id: event.pointingDeviceID() as u64,
            system_tablet_id: event.systemTabletID() as u64,
            vendor_pointing_device_type: event.vendorPointingDeviceType() as u64,
            unique_id: event.uniqueID(),
            capability_mask: event.capabilityMask() as u64,
            device_id: event.deviceID() as u64,
            window_number: event.windowNumber(),
        }))
    }
}

/// AppKit's `buttonMask` is an `NSUInteger` (64-bit) but only ever sets the
/// three documented low bits (`PenTip`/`PenLowerSide`/`PenUpperSide`) for
/// tablet events; truncating to `u16` cannot lose any of those bits.
fn truncate_bits(raw: usize) -> u16 {
    (raw & 0xFFFF) as u16
}

/// USB vendor IDs of known tablet manufacturers.
///
/// A `NSTabletProximity` event with `NSPointingDeviceTypeUnknown` (0) is
/// promoted to `Pen` when its vendor ID appears in this list.  This admits
/// proximity events that arrive before the Wacom driver finishes registering
/// the tool type with AppKit on hot-plug — the vendor ID is set by the OS HID
/// layer (not the driver) and is reliable from the very first event.  Mac
/// trackpads, which also emit Unknown-typed tablet events, carry Apple's vendor
/// ID (`0x05AC`) and therefore remain blocked.
///
/// Mirrors `crate::hid::session::TABLET_VENDOR_IDS`; kept local so this
/// module has no compile-time dependency on the experimental raw-HID path.
///
/// Test-only: the typed-pen path identifies tablets from the AppKit event
/// subtype rather than the vendor ID, so nothing in the shipped build reads
/// this. It is retained because the mapping is the documented reference for
/// which vendors the raw-HID path recognises.
#[cfg(test)]
const TYPED_TABLET_VENDOR_IDS: &[u16] = &[
    0x056A, // Wacom
    0x256c, // Huion
    0x28bd, // XP-Pen
    0x5543, // UC-Logic
    0x0b57, // Gaomon
];

/// Returns `true` when `vendor_id` identifies a known tablet manufacturer.
///
/// `NSEvent.vendorID` is an `NSUInteger` (64-bit) but only the low 16 bits
/// carry the USB vendor ID; the upper bits are always zero in practice.
#[cfg(test)]
fn is_known_tablet_vendor(vendor_id: u64) -> bool {
    TYPED_TABLET_VENDOR_IDS.contains(&(vendor_id as u16))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constructing_off_the_main_thread_is_reported_as_none() {
        // Rust's test harness runs each `#[test]` on a worker thread, not
        // AppKit's main thread, so `MainThreadMarker::new()` deterministically
        // returns `None` here — this exercises exactly the "no marker
        // available" branch every non-main-thread caller must handle, without
        // needing a live `NSApplication`.
        assert!(MainThreadMarker::new().is_none());
    }

    #[test]
    fn truncate_bits_keeps_the_three_documented_pen_bits() {
        assert_eq!(truncate_bits(0), 0);
        assert_eq!(truncate_bits(0b111), 0b111);
        assert_eq!(truncate_bits(usize::MAX), 0xFFFF);
    }

    #[test]
    fn source_filter_rejects_trackpad_and_cursor_types() {
        assert!(!NativeTabletTool::Unknown.is_pen_or_eraser());
        assert!(!NativeTabletTool::Cursor.is_pen_or_eraser());
        assert!(NativeTabletTool::Pen.is_pen_or_eraser());
        assert!(NativeTabletTool::Eraser.is_pen_or_eraser());
    }

    // --- vendor-gated Unknown bypass ---

    #[test]
    fn known_tablet_vendor_accepts_wacom_huion_xppen_and_rejects_apple_and_unknown() {
        // Known tablet vendors — must be accepted.
        assert!(is_known_tablet_vendor(0x056A)); // Wacom
        assert!(is_known_tablet_vendor(0x256c)); // Huion
        assert!(is_known_tablet_vendor(0x28bd)); // XP-Pen
        assert!(is_known_tablet_vendor(0x5543)); // UC-Logic
        assert!(is_known_tablet_vendor(0x0b57)); // Gaomon

        // Apple trackpad vendor must stay blocked regardless of event shape.
        assert!(!is_known_tablet_vendor(0x05AC));
        // Zero / all-ones must not accidentally match.
        assert!(!is_known_tablet_vendor(0x0000));
        assert!(!is_known_tablet_vendor(0xFFFF));
    }

    #[test]
    fn vendor_id_check_uses_only_low_16_bits() {
        // NSEvent.vendorID is NSUInteger; upper 48 bits must not affect the match.
        assert!(is_known_tablet_vendor(0x0000_0000_0000_056A)); // Wacom, clean
        assert!(is_known_tablet_vendor(0xDEAD_BEEF_CAFE_056A)); // Wacom, dirty high bits
        assert!(!is_known_tablet_vendor(0xDEAD_BEEF_CAFE_05AC)); // Apple, dirty high bits
    }
}
