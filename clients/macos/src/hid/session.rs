use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;

/// Capacity of the bounded channel used to carry [`HidEvent`]s from the
/// synchronous IOHID callback thread to the async transport loop.
///
/// SEC-raw-hid: this is a *bounded* queue on purpose. The producer side runs
/// inside synchronous C callbacks (see `imp::on_report` etc.) and must never
/// block waiting for the consumer; events are pushed with `try_send` and
/// silently dropped (counted as an error event where possible) once this
/// capacity is exceeded, rather than growing an unbounded queue that a
/// misbehaving/flooding device could use to exhaust memory.
pub const HID_EVENT_CHANNEL_CAPACITY: usize = 256;

/// Events produced by the IOHIDManager background thread.
#[derive(Debug)]
pub enum HidEvent {
    DeviceAdded {
        device_id: u8,
        vendor_id: u16,
        product_id: u16,
        descriptor: Vec<u8>,
    },
    DeviceRemoved {
        device_id: u8,
    },
    Report {
        device_id: u8,
        data: Vec<u8>,
    },
    PermissionGranted,
    Error {
        device_id: Option<u8>,
        reason_class: &'static str,
    },
}

/// Vendor IDs for supported tablet manufacturers.
///
/// SEC-raw-hid: this allow-list is enforced client-side as a first line of
/// defence, but the host independently re-checks vendor IDs before ever
/// creating a `/dev/uhid` device — the client's filtering is never trusted
/// alone.
const TABLET_VENDOR_IDS: &[u16] = &[
    0x056A, // Wacom
    0x256c, // Huion
    0x28bd, // XP-Pen
    0x5543, // UC-Logic
    0x0b57, // Gaomon
];

/// Returns whether `vendor_id` belongs to a supported experimental raw-HID
/// tablet vendor.
pub(crate) fn is_supported_tablet_vendor(vendor_id: u16) -> bool {
    TABLET_VENDOR_IDS.contains(&vendor_id)
}

/// Maximum number of concurrently tracked devices per session, mirroring the
/// host-side `MAX_EXPERIMENTAL_RAW_HID_DEVICES` bound. Prevents a pathological
/// hub/hotplug storm from growing the device table without bound.
const MAX_TRACKED_DEVICES: usize = 8;

/// HID usage page for "Digitizer" collections (pen/tablet input reports).
const USAGE_PAGE_DIGITIZER: i64 = 0x0D;
/// HID usage pages `0xFF00..=0xFFFF` are reserved for vendor-defined use;
/// tablet vendors commonly expose their raw report collection this way.
const USAGE_PAGE_VENDOR_DEFINED_MIN: i64 = 0xFF00;

/// Usage pages the device-matching dictionaries ask for.
///
/// Deliberately narrower than [`is_permitted_usage_page`], which filters the
/// devices the callback is offered. `IOHIDManagerOpen` must succeed for
/// *every* matched device, so the manager should claim only the collections
/// that actually carry pen reports rather than every collection a tablet
/// happens to publish.
///
/// An Intuos5 touch L (`056a:0317`) publishes three: `0xFF0D` (vendor pen
/// data, where the reports arrive), `0xFF00` (vendor control) and `0x0001`
/// (Generic Desktop mouse). Only the first is needed.
///
/// What is *not* claimed here: this is not a fix for `kIOReturnExclusiveAccess`.
/// That failure was observed against every matching combination, including
/// this one, and then observed to clear for every combination -- including
/// plain vendor-id matching -- after the vendor driver was restarted. It is a
/// transient ownership state, not a property of any particular collection,
/// and the remedy reported for it says so.
///
/// Add a page here when a tablet is tested that needs it; a device on an
/// unlisted page is simply never offered, which the debug log makes visible.
const MATCHED_USAGE_PAGES: &[i64] = &[USAGE_PAGE_DIGITIZER, 0xFF0D];

/// Returns whether `usage_page` identifies an interface collection that is
/// safe to treat as tablet/pen input (Digitizer or vendor-defined), as
/// opposed to e.g. the plain HID mouse (Generic Desktop, usage page `0x01`)
/// interface that some of these tablets also expose on the same VID/PID.
///
/// A missing/unreadable usage page is treated as *not permitted* (fail
/// closed) rather than assumed to be safe.
pub(crate) fn is_permitted_usage_page(usage_page: Option<i64>) -> bool {
    match usage_page {
        Some(page) => page == USAGE_PAGE_DIGITIZER || page >= USAGE_PAGE_VENDOR_DEFINED_MIN,
        None => false,
    }
}

/// Fallback per-device report buffer size used when a device does not report
/// (or reports an unusable) `MaxInputReportSize` property.
const DEFAULT_REPORT_BUF_SIZE: usize = 64;
/// Smallest report buffer we will ever allocate; guards against a device
/// claiming a nonsensical size like 0 or 1.
const MIN_REPORT_BUF_SIZE: usize = 8;

/// Clamp a device-reported `MaxInputReportSize` (if any) into a sane,
/// bounded per-device report buffer length.
///
/// SEC-raw-hid: the returned length is always within
/// `[MIN_REPORT_BUF_SIZE, arcen_protocol::wire::MAX_HID_REPORT_LEN]` so a
/// hostile or buggy device can neither cause a near-zero allocation nor force
/// an unbounded one; reports that exceed the shared wire bound are rejected
/// later by the shared protocol codec regardless.
pub(crate) fn clamp_report_buffer_len(raw_max_input_report_size: Option<i64>) -> usize {
    let requested = match raw_max_input_report_size {
        Some(value) if value > 0 => value as usize,
        _ => DEFAULT_REPORT_BUF_SIZE,
    };
    requested.clamp(
        MIN_REPORT_BUF_SIZE,
        arcen_protocol::wire::MAX_HID_REPORT_LEN,
    )
}

/// Find the smallest device id (0..=255) not already present in `used_ids`.
///
/// Returns `None` once all 256 ids are in use — this refuses new devices
/// rather than silently reusing (and thereby colliding with) an id that is
/// still active, which the previous `wrapping_add`-based allocator could do.
pub(crate) fn allocate_device_id(used_ids: &std::collections::HashSet<u8>) -> Option<u8> {
    (0u8..=u8::MAX).find(|candidate| !used_ids.contains(candidate))
}

/// Wraps a raw CoreFoundation run-loop pointer so it can be stored in a
/// shared `Arc<Mutex<..>>` and used from a thread other than the one that
/// created it, purely to call `CFRunLoopStop` on it.
///
/// # Safety
/// Apple's CoreFoundation reference documentation for `CFRunLoopStop`
/// describes it as safe to call from a thread other than the one running the
/// target run loop, specifically to force that other thread's
/// `CFRunLoopRun()` call to return. We never dereference this pointer or
/// pass it to any other CoreFoundation API from a foreign thread — it is
/// used exclusively as the argument to `CFRunLoopStop`.
struct SendableRunLoopHandle(*mut std::ffi::c_void);
// SAFETY: see the doc comment above; the pointer value itself carries no
// thread-affinity requirements for the sole operation we perform on it here.
unsafe impl Send for SendableRunLoopHandle {}

type SharedRunLoopHandle = Arc<Mutex<Option<SendableRunLoopHandle>>>;

/// Clears the published run-loop handle when the worker thread leaves
/// [`run_hid_loop`], by whichever path.
///
/// A thread's `CFRunLoop` is owned by that thread and is destroyed when it
/// exits, but `CFRunLoopGetCurrent` returns a *borrowed* reference. The
/// handle used to be published once and never cleared, so after the worker
/// returned — including the early returns taken when Input Monitoring is
/// denied or a stop request already landed — `HidSession::drop` would still
/// find it and call `CFRunLoopStop` on a destroyed run loop. That is a
/// use-after-free, and it crashed the client on disconnect with
/// `EXC_BREAKPOINT` inside `__CFCheckCFInfoPACSignature`.
///
/// Ownership is now explicit: the worker retains the run loop before
/// publishing it, and whichever side *takes* the handle out of the mutex
/// owns that retain and releases it. Both sides take under the same lock, so
/// exactly one of them can win and the pointer can never be used after the
/// release.
#[cfg(target_os = "macos")]
struct PublishedRunLoop(SharedRunLoopHandle);

#[cfg(target_os = "macos")]
impl Drop for PublishedRunLoop {
    fn drop(&mut self) {
        let taken = match self.0.lock() {
            Ok(mut guard) => guard.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        };
        if let Some(handle) = taken {
            // SAFETY: `handle.0` is the `CFRetain`ed run loop published by
            // this thread. Taking it out of the mutex transfers that retain
            // here, so releasing it exactly once is correct and no other
            // thread can still observe it.
            unsafe {
                crate::hid::iokit::CFRelease(handle.0);
            }
        }
    }
}

/// Manages the IOHIDManager lifecycle on a dedicated thread.
///
/// Dropping this struct signals the background thread to stop, forces its
/// `CFRunLoopRun()` to return (via `CFRunLoopStop`, regardless of whether a
/// device happens to be added/removed), and joins the thread so no capture
/// thread is ever leaked past the `HidSession`'s lifetime.
pub struct HidSession {
    stop: Arc<AtomicBool>,
    run_loop: SharedRunLoopHandle,
    join_handle: Option<std::thread::JoinHandle<()>>,
}

impl Drop for HidSession {
    fn drop(&mut self) {
        // Order matters: publish the stop request before consulting the
        // shared run-loop handle so that a worker thread which is still
        // inside its own startup critical section (see `run_hid_loop`) is
        // guaranteed to observe `stop == true` once it takes the lock, even
        // if it raced ahead of us and hasn't reached `CFRunLoopRun()` yet.
        self.stop.store(true, Ordering::SeqCst);
        let taken = match self.run_loop.lock() {
            Ok(mut guard) => guard.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        };
        if let Some(handle) = taken {
            // SAFETY: taking the handle out of the mutex transfers the
            // worker's `CFRetain` to us, so the run loop is guaranteed alive
            // for this call even if the worker thread is exiting right now.
            // `CFRunLoopStop` is documented as safe from any thread. We then
            // own the retain and release it exactly once. Borrowing instead
            // of taking (the previous behaviour) let this run after the
            // worker thread had already died, crashing on a freed run loop.
            #[cfg(target_os = "macos")]
            unsafe {
                crate::hid::iokit::CFRunLoopStop(handle.0);
                crate::hid::iokit::CFRelease(handle.0);
            }
            #[cfg(not(target_os = "macos"))]
            let _ = handle;
        }
        if let Some(join_handle) = self.join_handle.take() {
            let _ = join_handle.join();
        }
    }
}

impl HidSession {
    /// Start the HID session.  Tablet attach/detach events and raw reports are
    /// forwarded through the bounded `tx` channel.  The background CFRunLoop
    /// thread runs until the returned `HidSession` is dropped, at which point
    /// it is reliably stopped and joined (see `Drop`).
    pub fn start(tx: mpsc::Sender<HidEvent>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let run_loop: SharedRunLoopHandle = Arc::new(Mutex::new(None));
        let stop_clone = stop.clone();
        let run_loop_clone = run_loop.clone();
        let spawn_error_tx = tx.clone();
        let join_handle = std::thread::Builder::new()
            .name("arcen-hid".into())
            .spawn(move || {
                #[cfg(target_os = "macos")]
                run_hid_loop(tx, stop_clone, run_loop_clone);
                #[cfg(not(target_os = "macos"))]
                let _ = (tx, stop_clone, run_loop_clone);
            })
            .inspect_err(|_| {
                let _ = spawn_error_tx.try_send(HidEvent::Error {
                    device_id: None,
                    reason_class: "worker_start",
                });
            })
            .ok();
        Self {
            stop,
            run_loop,
            join_handle,
        }
    }
}

// ── macOS implementation ──────────────────────────────────────────────────────

#[cfg(target_os = "macos")]
mod imp {
    use super::*;
    use crate::hid::iokit::*;
    use std::ffi::c_void;

    /// Builds the device-matching array: one `{ VendorID: n }` dictionary
    /// per supported tablet vendor.
    ///
    /// The manager must be told what to match. Passing null ("everything")
    /// makes `IOHIDManagerOpen` responsible for every HID device on the Mac,
    /// including keyboards, and the system refuses that outright — measured
    /// here as `IOHIDManagerOpen` failing while a digitizer-only match
    /// succeeded. Matching the supported vendors is both narrower and what
    /// the add callback already wanted; the callback keeps its own
    /// vendor/usage-page check to reject a vendor's non-tablet interfaces.
    ///
    /// Returns null if any CoreFoundation allocation fails; callers treat
    /// that as a startup failure rather than falling back to matching
    /// everything.
    ///
    /// # Safety
    /// Calls CoreFoundation constructors. The returned array is owned by the
    /// caller and must be released.
    unsafe fn build_tablet_matching_array() -> CFTypeRef {
        let array = CFArrayCreateMutable(std::ptr::null_mut(), 0, &raw const kCFTypeArrayCallBacks);
        if array.is_null() {
            return std::ptr::null_mut();
        }
        for vendor in TABLET_VENDOR_IDS {
            for page in MATCHED_USAGE_PAGES {
                let dictionary = CFDictionaryCreateMutable(
                    std::ptr::null_mut(),
                    0,
                    &raw const kCFTypeDictionaryKeyCallBacks,
                    &raw const kCFTypeDictionaryValueCallBacks,
                );
                if dictionary.is_null() {
                    CFRelease(array);
                    return std::ptr::null_mut();
                }
                let entries = [
                    (MATCH_VENDOR_ID, i32::from(*vendor)),
                    (
                        MATCH_DEVICE_USAGE_PAGE,
                        i32::try_from(*page).unwrap_or(0x0D),
                    ),
                ];
                for (name, number) in entries {
                    let key = cf_string(name);
                    let value = CFNumberCreate(
                        std::ptr::null_mut(),
                        CF_NUMBER_SINT32_TYPE,
                        (&raw const number).cast::<c_void>(),
                    );
                    if key.is_null() || value.is_null() {
                        if !value.is_null() {
                            CFRelease(value);
                        }
                        CFRelease(dictionary);
                        CFRelease(array);
                        return std::ptr::null_mut();
                    }
                    CFDictionarySetValue(
                        dictionary,
                        key.cast::<c_void>(),
                        value.cast_const().cast::<c_void>(),
                    );
                    CFRelease(key.cast_mut());
                    CFRelease(value);
                }
                CFArrayAppendValue(array, dictionary.cast_const().cast::<c_void>());
                CFRelease(dictionary);
            }
        }
        array
    }

    struct DeviceState {
        device_id: u8,
        // Heap-allocated, fixed-length for the lifetime of the device: the
        // slice's backing allocation address is stable even though the
        // owning `DeviceState` may move within `CallbackCtx::devices`'s
        // hash map, so the raw pointer registered with IOKit below stays
        // valid for as long as the entry remains in the map.
        report_buf: Box<[u8]>,
    }

    struct CallbackCtx {
        tx: mpsc::Sender<HidEvent>,
        // device pointer → DeviceState (keyed as usize to avoid Send issues)
        devices: HashMap<usize, DeviceState>,
        run_loop: CFRunLoopRef,
        stop: Arc<AtomicBool>,
        permission_reported: bool,
    }

    impl CallbackCtx {
        fn used_device_ids(&self) -> std::collections::HashSet<u8> {
            self.devices.values().map(|s| s.device_id).collect()
        }
    }

    /// Best-effort, non-blocking send: this runs inside a synchronous IOKit
    /// callback, so we must never block. Dropping events under backpressure
    /// is an accepted trade-off of the bounded-queue design (see
    /// `HID_EVENT_CHANNEL_CAPACITY`).
    fn send_event(tx: &mpsc::Sender<HidEvent>, event: HidEvent) {
        let _ = tx.try_send(event);
    }

    unsafe extern "C" fn on_device_added(
        context: *mut c_void,
        _result: IOReturn,
        _sender: *mut c_void,
        device: IOHIDDeviceRef,
    ) {
        // SAFETY: `context` is the `*mut CallbackCtx` we registered in
        // `run_hid_loop`; it stays alive for as long as the run loop can
        // invoke callbacks (see the SAFETY note on `run_hid_loop`).
        let ctx = &mut *(context as *mut CallbackCtx);

        // Read vendor/product IDs.
        //
        // SEC-209. `cf_string` uses CFStringCreateWithCString, so the two keys
        // are owned here and must be released. The two VALUES come from
        // IOHIDDeviceGetProperty, a Get-rule API: the returned CFTypeRef is
        // autoreleased and owned by IOKit, and releasing it is an over-release.
        // Only the keys are released below.
        let vid_key = cf_string(PROP_VENDOR_ID);
        let pid_key = cf_string(PROP_PRODUCT_ID);
        let vid_cf = IOHIDDeviceGetProperty(device, vid_key);
        let pid_cf = IOHIDDeviceGetProperty(device, pid_key);
        CFRelease(vid_key as CFTypeRef);
        CFRelease(pid_key as CFTypeRef);

        let vid = cf_number_i32(vid_cf).unwrap_or(0) as u16;
        let pid = cf_number_i32(pid_cf).unwrap_or(0) as u16;

        // Filter #1: only forward recognised tablet vendors. The host
        // independently re-checks this — the client filter is defence in
        // depth only, never trusted alone.
        if !is_supported_tablet_vendor(vid) {
            return;
        }

        // Filter #2: usage-page filtering. These tablets commonly expose
        // multiple logical HID interfaces on the same VID/PID — including a
        // plain Generic Desktop mouse interface — and only the
        // Digitizer/vendor-defined interface should ever be captured.
        let usage_page_key = cf_string(PROP_PRIMARY_USAGE_PAGE);
        let usage_page_cf = IOHIDDeviceGetProperty(device, usage_page_key);
        CFRelease(usage_page_key as CFTypeRef);
        let usage_page = cf_number_i32(usage_page_cf).map(i64::from);
        if !is_permitted_usage_page(usage_page) {
            return;
        }

        // Bound #1: cap the number of concurrently tracked devices.
        if ctx.devices.len() >= MAX_TRACKED_DEVICES {
            send_event(
                &ctx.tx,
                HidEvent::Error {
                    device_id: None,
                    reason_class: "device_limit_exceeded",
                },
            );
            return;
        }

        // Bound #2: allocate a checked device id, refusing to proceed (rather
        // than reusing/colliding an id still in use) once the id space (256)
        // is exhausted.
        let device_id = match allocate_device_id(&ctx.used_device_ids()) {
            Some(id) => id,
            None => {
                send_event(
                    &ctx.tx,
                    HidEvent::Error {
                        device_id: None,
                        reason_class: "device_id_space_exhausted",
                    },
                );
                return;
            }
        };

        // Open the device to receive input reports.
        if IOHIDDeviceOpen(device, kIOHIDOptionsTypeNone) != kIOReturnSuccess {
            send_event(
                &ctx.tx,
                HidEvent::Error {
                    device_id: None,
                    reason_class: "open_failed",
                },
            );
            return;
        }
        if !ctx.permission_reported {
            ctx.permission_reported = true;
            send_event(&ctx.tx, HidEvent::PermissionGranted);
        }

        // Read the report descriptor.
        //
        // SEC-209. Same Get rule as above: `desc_key` is owned and released,
        // `desc_cf` is borrowed from IOKit and must not be.
        let desc_key = cf_string(PROP_REPORT_DESCRIPTOR);
        let desc_cf = IOHIDDeviceGetProperty(device, desc_key);
        CFRelease(desc_key as CFTypeRef);
        let descriptor = cf_data_to_vec(desc_cf).unwrap_or_default();
        if descriptor.is_empty() {
            send_event(
                &ctx.tx,
                HidEvent::Error {
                    device_id: None,
                    reason_class: "descriptor_read",
                },
            );
            let _ = IOHIDDeviceClose(device, kIOHIDOptionsTypeNone);
            return;
        }
        // Bound #3: never forward an oversize descriptor claim. The shared
        // wire codec enforces this too, but rejecting locally avoids ever
        // constructing/transmitting a frame we know will be dropped, and
        // protects against a pathological/hostile device inflating memory.
        if descriptor.len() > arcen_protocol::wire::MAX_HID_DESCRIPTOR_LEN {
            send_event(
                &ctx.tx,
                HidEvent::Error {
                    device_id: None,
                    reason_class: "descriptor_too_large",
                },
            );
            let _ = IOHIDDeviceClose(device, kIOHIDOptionsTypeNone);
            return;
        }

        // Per-device report buffer, sized from the device's own
        // MaxInputReportSize property (clamped to a safe bounded range)
        // instead of one hardcoded constant for every vendor/device.
        let max_report_key = cf_string(PROP_MAX_INPUT_REPORT_SIZE);
        let max_report_cf = IOHIDDeviceGetProperty(device, max_report_key);
        CFRelease(max_report_key as CFTypeRef);
        let report_buf_len = clamp_report_buffer_len(cf_number_i32(max_report_cf).map(i64::from));

        // Allocate a stable report buffer on the heap and register the callback.
        let report_buf: Box<[u8]> = vec![0u8; report_buf_len].into_boxed_slice();
        let buf_ptr = report_buf.as_ptr() as *mut u8;

        let state = DeviceState {
            device_id,
            report_buf,
        };
        ctx.devices.insert(device as usize, state);

        IOHIDDeviceRegisterInputReportCallback(
            device,
            buf_ptr,
            report_buf_len as isize,
            on_report,
            context,
        );

        send_event(
            &ctx.tx,
            HidEvent::DeviceAdded {
                device_id,
                vendor_id: vid,
                product_id: pid,
                descriptor,
            },
        );
    }

    unsafe extern "C" fn on_device_removed(
        context: *mut c_void,
        _result: IOReturn,
        _sender: *mut c_void,
        device: IOHIDDeviceRef,
    ) {
        // SAFETY: see `on_device_added`.
        let ctx = &mut *(context as *mut CallbackCtx);
        if let Some(state) = ctx.devices.remove(&(device as usize)) {
            let _ = IOHIDDeviceClose(device, kIOHIDOptionsTypeNone);
            send_event(
                &ctx.tx,
                HidEvent::DeviceRemoved {
                    device_id: state.device_id,
                },
            );
        }
        // Best-effort extra stop path: if a Drop-triggered stop request
        // arrived while we were between callbacks, honour it here too. The
        // authoritative stop mechanism is `HidSession::drop`, which stops the
        // run loop directly via the shared run-loop handle even if no
        // device add/remove event ever fires.
        if ctx.stop.load(Ordering::SeqCst) {
            CFRunLoopStop(ctx.run_loop);
        }
    }

    unsafe extern "C" fn on_report(
        context: *mut c_void,
        _result: IOReturn,
        _sender: *mut c_void,
        _report_type: u32,
        _report_id: u32,
        report: *const u8,
        report_len: isize,
    ) {
        if report_len <= 0 {
            return;
        }
        let len = report_len as usize;
        // Bound: defence in depth against a device somehow reporting a
        // length beyond what we registered (IOKit should never do this
        // since we handed it a `report_buf_len`-sized buffer, but the shared
        // wire bound is the authoritative ceiling regardless).
        if len > arcen_protocol::wire::MAX_HID_REPORT_LEN {
            return;
        }
        // SAFETY: see `on_device_added`.
        let ctx = &mut *(context as *mut CallbackCtx);
        // SAFETY: IOKit guarantees `report` points at `len` valid bytes for
        // the duration of this callback invocation.
        let data = std::slice::from_raw_parts(report, len).to_vec();

        // We don't get the IOHIDDeviceRef here directly in the callback we
        // registered, so we scan the map. With O(1) tablets this is fine.
        for state in ctx.devices.values() {
            let buf_ptr = state.report_buf.as_ptr();
            if report == buf_ptr {
                send_event(
                    &ctx.tx,
                    HidEvent::Report {
                        device_id: state.device_id,
                        data,
                    },
                );
                break;
            }
        }
    }

    /// Run the IOHIDManager capture loop on the current thread until stopped.
    ///
    /// # Safety (thread lifetime / callback validity)
    /// `ctx` (the `CallbackCtx` boxed below) is only ever touched by
    /// callbacks dispatched through `CFRunLoopRun()` on *this* thread — IOKit
    /// never invokes these callbacks concurrently or from another thread —
    /// so `ctx` is guaranteed to outlive every invocation of
    /// `on_device_added`/`on_device_removed`/`on_report` and is only dropped
    /// after `CFRunLoopRun()` has returned control back to this function.
    #[allow(non_upper_case_globals)] // matching Apple's kIOHID* naming convention
    pub(super) fn run_hid_loop(
        tx: mpsc::Sender<HidEvent>,
        stop: Arc<AtomicBool>,
        run_loop_handle: SharedRunLoopHandle,
    ) {
        unsafe {
            let run_loop = CFRunLoopGetCurrent();

            // Retain before publishing, and install a guard that clears the
            // published handle on every exit path from this function. See
            // `PublishedRunLoop`: without this the handle outlived the thread
            // that owned the run loop.
            CFRetain(run_loop as CFTypeRef);
            let _published = PublishedRunLoop(run_loop_handle.clone());

            // Publish the run-loop handle before doing anything else so that
            // `HidSession::drop` can reliably stop us even if it races ahead
            // of the rest of this setup (see `SendableRunLoopHandle`'s SAFETY
            // note and `HidSession::drop`).
            {
                let mut guard = match run_loop_handle.lock() {
                    Ok(guard) => guard,
                    Err(poisoned) => poisoned.into_inner(),
                };
                *guard = Some(SendableRunLoopHandle(run_loop as *mut std::ffi::c_void));
                if stop.load(Ordering::SeqCst) {
                    // A stop request already landed before we published the
                    // handle above (the caller observed `None` and is about
                    // to (or already did) fall through to `join()`); return
                    // immediately without ever creating the IOHIDManager.
                    return;
                }
            }

            // TCC (Input Monitoring) access check. We distinguish the three
            // possible states truthfully instead of only inferring
            // "granted" from a successful device open:
            //   - Granted: proceed, and report it immediately.
            //   - Denied:  fail closed — never create the manager or touch
            //              any device, and report the denial explicitly.
            //   - Unknown: prompt the user via IOHIDRequestAccess and fall
            //              back to the existing open-result heuristic below
            //              (we cannot synchronously know the user's answer).
            // Whether TCC already reported Input Monitoring as granted when
            // this process started. macOS decides a process's actual access
            // when it first uses the API, so a grant made while the app is
            // running is visible to `IOHIDCheckAccess` but does not apply to
            // this process: the open still fails until it is restarted.
            // Distinguishing the two matters -- calling that a denial sends
            // whoever reads the log to re-grant a permission that is already
            // granted.
            let mut access_already_granted = false;
            match IOHIDCheckAccess(kIOHIDRequestTypeListenEvent) {
                kIOHIDAccessTypeDenied => {
                    send_event(
                        &tx,
                        HidEvent::Error {
                            device_id: None,
                            reason_class: "permission_denied",
                        },
                    );
                    return;
                }
                kIOHIDAccessTypeGranted => {
                    access_already_granted = true;
                    send_event(&tx, HidEvent::PermissionGranted);
                }
                _ => {
                    // Unknown/not-yet-determined: ask the OS to prompt the
                    // user. This does not block; the eventual answer is
                    // observed indirectly via subsequent device-open
                    // success/failure.
                    IOHIDRequestAccess(kIOHIDRequestTypeListenEvent);
                }
            }

            let mode_str = cf_string(kCFRunLoopDefaultMode);

            let manager = IOHIDManagerCreate(std::ptr::null_mut(), kIOHIDOptionsTypeNone);
            if manager.is_null() {
                return;
            }

            // Match digitizers only.
            //
            // This used to pass null ("match everything") and filter by
            // vendor and usage page in the add callback. That is too late:
            // `IOHIDManagerOpen` must succeed for *every* matched device, and
            // the system refuses a manager matching every HID device on the
            // Mac -- including keyboards -- however Input Monitoring is set.
            // Measured directly on this hardware with Input Monitoring
            // granted: matching all devices and matching the tablet's vendor
            // id both fail `IOHIDManagerOpen` with 0xe00002c5, while matching
            // `DeviceUsagePage == Digitizer` succeeds.
            //
            // Matching the vendor id fails too because a tablet publishes
            // more than the pen: an Intuos also exposes keyboard-like
            // ExpressKey collections, and those drag the same refusal back
            // in. Narrowing to the digitizer collection is both what the
            // callback filter already wanted and the only form the system
            // will open. The callback keeps its own vendor/usage-page check,
            // which is still needed to reject non-tablet digitizers.
            let matching = build_tablet_matching_array();
            if matching.is_null() {
                send_event(
                    &tx,
                    HidEvent::Error {
                        device_id: None,
                        reason_class: "matching_dictionary",
                    },
                );
                CFRelease(manager as CFTypeRef);
                CFRelease(mode_str as CFTypeRef);
                return;
            }
            IOHIDManagerSetDeviceMatchingMultiple(manager, matching);
            CFRelease(matching);

            let mut ctx = Box::new(CallbackCtx {
                tx,
                devices: HashMap::new(),
                run_loop,
                stop,
                permission_reported: false,
            });
            let ctx_ptr = &raw mut *ctx as *mut c_void;

            IOHIDManagerRegisterDeviceMatchingCallback(manager, on_device_added, ctx_ptr);
            IOHIDManagerRegisterDeviceRemovalCallback(manager, on_device_removed, ctx_ptr);
            IOHIDManagerScheduleWithRunLoop(manager, run_loop, mode_str);

            let open_result = IOHIDManagerOpen(manager, kIOHIDOptionsTypeNone);
            if open_result != kIOReturnSuccess {
                // Report what the OS actually said. This used to collapse
                // every failure into "open_failed", which the error emitter
                // then reported as an Input Monitoring denial -- so a tablet
                // held by its own vendor driver looked exactly like a missing
                // permission, and cost an evening of granting a permission
                // that was already granted.
                //
                // `kIOReturnExclusiveAccess` is transient: it was measured
                // against every matching combination on one occasion and
                // against none of them after the vendor driver was restarted,
                // with the driver running in both cases. So it means "someone
                // holds it right now", not "this device cannot be captured".
                let reason_class = match open_result {
                    IO_RETURN_EXCLUSIVE_ACCESS => "exclusive_access",
                    IO_RETURN_NOT_PERMITTED => "not_permitted",
                    _ if access_already_granted => "open_failed_despite_grant",
                    _ => "open_failed",
                };
                send_event(
                    &ctx.tx,
                    HidEvent::Error {
                        device_id: None,
                        reason_class,
                    },
                );
                CFRelease(manager as CFTypeRef);
                CFRelease(mode_str as CFTypeRef);
                return;
            }

            // Run until the session is dropped: `HidSession::drop` calls
            // `CFRunLoopStop` directly via `run_loop_handle` (reliable even
            // if no device add/remove event ever fires), and
            // `on_device_removed` also re-checks `stop` as a secondary path.
            CFRunLoopRun();

            IOHIDManagerClose(manager, kIOHIDOptionsTypeNone);
            CFRelease(manager as CFTypeRef);
            CFRelease(mode_str as CFTypeRef);
            drop(ctx);
        }
    }
}

#[cfg(target_os = "macos")]
use imp::run_hid_loop;

#[cfg(not(target_os = "macos"))]
fn run_hid_loop(
    _tx: mpsc::Sender<HidEvent>,
    _stop: Arc<AtomicBool>,
    _run_loop: SharedRunLoopHandle,
) {
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vendor_allow_list_matches_only_known_tablet_vendors() {
        assert!(is_supported_tablet_vendor(0x056A)); // Wacom
        assert!(is_supported_tablet_vendor(0x256c)); // Huion
        assert!(is_supported_tablet_vendor(0x28bd)); // XP-Pen
        assert!(is_supported_tablet_vendor(0x5543)); // UC-Logic
        assert!(is_supported_tablet_vendor(0x0b57)); // Gaomon
        assert!(!is_supported_tablet_vendor(0x05ac)); // Apple, must not match
        assert!(!is_supported_tablet_vendor(0x0000));
    }

    #[test]
    fn usage_page_filter_excludes_generic_desktop_mouse_interface() {
        // The Generic Desktop usage page (0x01) is what a plain HID mouse
        // interface reports — several supported tablets expose exactly this
        // interface alongside their real digitizer/vendor interfaces, and it
        // must never be captured.
        assert!(!is_permitted_usage_page(Some(0x01)));
        assert!(!is_permitted_usage_page(None));
        assert!(is_permitted_usage_page(Some(0x0D))); // Digitizer
        assert!(is_permitted_usage_page(Some(0xFF00))); // vendor-defined
        assert!(is_permitted_usage_page(Some(0xFFFF))); // vendor-defined
    }

    #[test]
    fn report_buffer_len_is_clamped_within_bounds() {
        assert_eq!(clamp_report_buffer_len(None), DEFAULT_REPORT_BUF_SIZE);
        assert_eq!(clamp_report_buffer_len(Some(0)), DEFAULT_REPORT_BUF_SIZE);
        assert_eq!(clamp_report_buffer_len(Some(-5)), DEFAULT_REPORT_BUF_SIZE);
        assert_eq!(clamp_report_buffer_len(Some(1)), MIN_REPORT_BUF_SIZE);
        assert_eq!(
            clamp_report_buffer_len(Some(1_000_000)),
            arcen_protocol::wire::MAX_HID_REPORT_LEN
        );
        assert_eq!(clamp_report_buffer_len(Some(128)), 128);
    }

    /// Starting and dropping a session must not crash, whatever the worker
    /// did.
    ///
    /// The worker publishes its `CFRunLoopRef` for `Drop` to stop, but a
    /// thread's run loop dies with the thread and `CFRunLoopGetCurrent`
    /// returns only a borrowed reference. The handle used to be published
    /// once and never cleared, so once the worker returned — which it does
    /// immediately when Input Monitoring is not granted, the common case on
    /// a fresh machine — `Drop` called `CFRunLoopStop` on a destroyed run
    /// loop. That crashed the client on disconnect with `EXC_BREAKPOINT`
    /// inside `__CFCheckCFInfoPACSignature`, reported from a pier-linux.example.internal
    /// session on 2026-08-12.
    ///
    /// Honest scope: this exercises the exact lifecycle that crashed, but it
    /// is a smoke test, not a proof. Reverting the fix does not reliably make
    /// it fail, because a use-after-free only faults once the freed memory is
    /// actually reused — CoreFoundation may keep the run loop's storage valid
    /// for a while after the thread exits. The fix is sound because the
    /// retain makes the pointer provably live for the call, not because this
    /// test goes red without it.
    #[cfg(target_os = "macos")]
    #[test]
    fn dropping_a_session_after_its_worker_exits_does_not_touch_a_dead_run_loop() {
        let (tx, _rx) = mpsc::channel(super::HID_EVENT_CHANNEL_CAPACITY);
        let session = HidSession::start(tx);

        // Give the worker time to publish its handle and then return: with
        // Input Monitoring denied or undecided it exits almost immediately,
        // and with it granted it parks in `CFRunLoopRun` — both orderings are
        // valid here, and both must survive the drop below.
        std::thread::sleep(std::time::Duration::from_millis(400));

        drop(session);
    }

    /// Dropping twice in quick succession, and dropping a session whose
    /// worker never started, must also be safe: the handle is owned by
    /// whichever side takes it, so there is nothing left to release twice.
    #[cfg(target_os = "macos")]
    #[test]
    fn dropping_an_immediately_stopped_session_is_safe() {
        let (tx, _rx) = mpsc::channel(super::HID_EVENT_CHANNEL_CAPACITY);
        drop(HidSession::start(tx));
    }

    /// The manager should claim only the collections that carry pen reports.
    ///
    /// Measured collections on an Intuos5 touch L: `0xFF0D` carries the pen
    /// data, `0xFF00` is a vendor control collection that carries none, and
    /// `0x0001` is a Generic Desktop mouse. Claiming a collection the session
    /// never reads is a liability, since `IOHIDManagerOpen` must succeed for
    /// every matched device.
    ///
    /// This is explicitly *not* asserting that `0xFF00` is unopenable -- it
    /// was measured open once the vendor driver was restarted. It asserts
    /// only that we do not ask for what we do not read.
    #[test]
    fn matched_usage_pages_exclude_collections_that_cannot_be_opened() {
        assert!(
            MATCHED_USAGE_PAGES.contains(&USAGE_PAGE_DIGITIZER),
            "digitizer collections carry pen reports on standards-compliant tablets",
        );
        assert!(
            MATCHED_USAGE_PAGES.contains(&0xFF0D),
            "0xFF0D is where a Wacom tablet's pen reports actually arrive",
        );
        assert!(
            !MATCHED_USAGE_PAGES.contains(&USAGE_PAGE_VENDOR_DEFINED_MIN),
            "0xFF00 is a vendor control collection carrying no pen reports; \
             the session never reads it, so it must not be claimed",
        );

        // The callback filter stays deliberately broader: it judges devices it
        // is offered, and must still reject a matched non-tablet collection.
        for page in MATCHED_USAGE_PAGES {
            assert!(
                is_permitted_usage_page(Some(*page)),
                "matching must never ask for a page the callback then rejects: {page:#x}",
            );
        }
    }

    #[test]
    fn device_id_allocator_skips_used_ids_and_refuses_when_exhausted() {
        let mut used = std::collections::HashSet::new();
        assert_eq!(allocate_device_id(&used), Some(0));
        used.insert(0);
        used.insert(1);
        assert_eq!(allocate_device_id(&used), Some(2));

        let full: std::collections::HashSet<u8> = (0u8..=u8::MAX).collect();
        assert_eq!(allocate_device_id(&full), None);
    }
}
