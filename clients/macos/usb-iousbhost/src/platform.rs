// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The Objective-C side of the backend: I/O Registry reads, `DeviceCapture`,
//! configuration, interfaces, pipes, asynchronous transfers and release.
//!
//! This is the only module of the crate allowed to use `unsafe`; every block
//! says why it is sound. Decisions live in the portable modules
//! ([`crate::matching`], [`crate::lifecycle`], [`crate::status`],
//! [`crate::marker`]); this module only carries them out.
//!
//! Threads and queues:
//!
//! - One serial dispatch queue per capture services every asynchronous
//!   completion of the device, its interfaces and its pipes. Completion
//!   handlers only copy their buffer, mark the transfer finished and hand the
//!   outcome to the shared engine's sink; they never touch the device.
//! - `IOUSBHost` delivers interest messages (requesting-close, termination) on
//!   its own internal queue. The handler records the ending and wakes the
//!   release thread; it never destroys anything.
//! - A dedicated release thread per capture aborts, drains and destroys. Apple
//!   documents that `destroy` from a completion handler deadlocks, so nothing
//!   on the I/O queue ever releases.

use crate::handback::{
    HANDBACK_POLL, Handback, HandbackTimeline, HandbackVerdict, REENUMERATION_WAIT,
    TERMINATION_WAIT,
};
use crate::lifecycle::{DestroyMode, Lifecycle, Phase, ReleasePlan, pipe_may_abort};
use crate::marker::{CaptureMarker, MarkerRead, MarkerStore, RecoveryPlan, plan_recovery};
use crate::matching::{
    CaptureTarget, ControlRoute, DEVICE_DESCRIPTOR_LENGTH, RegistryHid, RegistryUsbDevice,
    captured_facts, configuration_total_length, control_route, hid_descriptors_for,
    missing_interfaces, pipes_for, select_target, speed_from_registry,
};
use crate::status::{CaptureError, CaptureStage, IoReturn, transfer_status};
use arcen_usb_bridge::{
    BackendRefusal, BackendStatus, CapturedDeviceFacts, CapturedUsbDevice, ControlOperation,
    EndSignal, EndpointAddress, InterfaceReportDescriptor, ParsedConfiguration, PendingCompletion,
    ReleaseOutcome, ReleaseReason, UrbId,
};
use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchQueueAttr, DispatchRetained};
use objc2::AllocAnyThread;
use objc2::rc::Retained;
use objc2_core_foundation::{CFData, CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_foundation::{NSError, NSMutableData};
use objc2_io_kit::{
    IOIteratorNext, IOObjectConformsTo, IOObjectRelease, IORegistryEntryCreateCFProperty,
    IORegistryEntryGetChildIterator, IORegistryEntryGetRegistryEntryID,
    IORegistryEntrySearchCFProperty, IOServiceGetMatchingServices, IOServiceMatching,
    IOUSBDeviceRequest, io_iterator_t, io_object_t, kIORegistryIterateParents,
    kIORegistryIterateRecursively, kIOServicePlane,
};
use objc2_io_usb_host::{
    IOUSBHostAbortOption, IOUSBHostDevice, IOUSBHostInterface, IOUSBHostObject,
    IOUSBHostObjectDestroyOptions, IOUSBHostObjectInitOptions, IOUSBHostPipe,
};
use std::collections::BTreeMap;
use std::ffi::{CStr, c_char, c_void};
use std::fmt::{Display, Formatter};
use std::ptr::NonNull;
use std::sync::mpsc::{Receiver, Sender, SyncSender};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// `kIOMainPortDefault` is `MACH_PORT_NULL`.
const MAIN_PORT_DEFAULT: u32 = 0;
const KERN_SUCCESS: i32 = 0;
/// Upper bound on registry entries examined per scan.
const MAX_SERVICES: usize = 256;
/// How long `configureWithValue` may take to publish the interface services.
const INTERFACE_WAIT: Duration = Duration::from_secs(3);
const INTERFACE_POLL: Duration = Duration::from_millis(20);
/// How long a release waits for aborted transfers to report before it
/// destroys the device anyway (`destroy` completes whatever is left).
const RELEASE_DRAIN: Duration = Duration::from_millis(1_500);
/// How long [`IoUsbHostDevice::release`] waits for the release thread: the
/// drain, then a held hand-back (termination, hold, re-enumeration; see
/// [`crate::handback`]), with room to spare.
const RELEASE_REPLY: Duration = Duration::from_secs(20);
const IO_QUEUE_LABEL: &str = "tech.arcen.usb-iousbhost.io";

/// The registry could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryError(String);

impl Display for RegistryError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "read the I/O Registry: {}", self.0)
    }
}

impl std::error::Error for RegistryError {}

/// One owned `io_object_t` reference, released on drop.
struct IoObject(io_object_t);

impl IoObject {
    fn new(raw: io_object_t) -> Option<Self> {
        (raw != 0).then_some(Self(raw))
    }

    const fn raw(&self) -> io_object_t {
        self.0
    }
}

impl Drop for IoObject {
    fn drop(&mut self) {
        // The reference was returned to us by an I/O Kit lookup or iterator
        // and is released exactly once, here.
        let _ = IOObjectRelease(self.0);
    }
}

/// An I/O Kit name argument (`io_name_t`, a C string the callee only reads).
fn io_name(name: &CStr) -> *mut [c_char; 128] {
    name.as_ptr().cast_mut().cast()
}

fn conforms_to(entry: &IoObject, class: &CStr) -> bool {
    // SAFETY: `entry` holds a live reference, and `class` is a NUL-terminated
    // string that `IOObjectConformsTo` only reads.
    unsafe { IOObjectConformsTo(entry.raw(), io_name(class)) }
}

fn property(entry: &IoObject, key: &'static str) -> Option<CFRetained<CFType>> {
    let key = CFString::from_static_str(key);
    // SAFETY: `entry` holds a live reference and `key` is a valid CFString;
    // a null allocator is `kCFAllocatorDefault`. The result follows the Create
    // rule, which `CFRetained` takes over.
    unsafe { IORegistryEntryCreateCFProperty(entry.raw(), Some(&key), None, 0) }
}

fn number_property(entry: &IoObject, key: &'static str) -> Option<i64> {
    property(entry, key)?.downcast::<CFNumber>().ok()?.as_i64()
}

fn string_property(entry: &IoObject, key: &'static str) -> Option<String> {
    Some(
        property(entry, key)?
            .downcast::<CFString>()
            .ok()?
            .to_string(),
    )
}

fn data_property(entry: &IoObject, key: &'static str) -> Option<Vec<u8>> {
    Some(property(entry, key)?.downcast::<CFData>().ok()?.to_vec())
}

/// Searches `entry` and its ancestors in the service plane for `key`.
fn inherited_number(entry: &IoObject, key: &'static str) -> Option<i64> {
    let key = CFString::from_static_str(key);
    // SAFETY: as in `property`; the plane name is a NUL-terminated string the
    // call only reads.
    let value = unsafe {
        IORegistryEntrySearchCFProperty(
            entry.raw(),
            io_name(kIOServicePlane),
            Some(&key),
            None,
            kIORegistryIterateRecursively | kIORegistryIterateParents,
        )
    }?;
    value.downcast::<CFNumber>().ok()?.as_i64()
}

fn registry_id(entry: &IoObject) -> Option<u64> {
    let mut id = 0_u64;
    // SAFETY: `entry` holds a live reference and `id` is a valid out-pointer.
    let result = unsafe { IORegistryEntryGetRegistryEntryID(entry.raw(), &raw mut id) };
    (result == KERN_SUCCESS).then_some(id)
}

/// Every registered service of `class`, bounded.
fn matching_services(class: &CStr) -> Result<Vec<IoObject>, RegistryError> {
    // SAFETY: `class` is NUL-terminated; the dictionary comes back owned.
    let matching = unsafe { IOServiceMatching(class.as_ptr()) }
        .ok_or_else(|| RegistryError(format!("no matching dictionary for {class:?}")))?;
    // SAFETY: a `CFMutableDictionary` is a `CFDictionary`; the reference is
    // moved, not duplicated, and `IOServiceGetMatchingServices` consumes it.
    let matching = unsafe { CFRetained::cast_unchecked::<CFDictionary>(matching) };
    let mut iterator: io_iterator_t = 0;
    // SAFETY: `iterator` is a valid out-pointer; the dictionary is consumed.
    let result = unsafe {
        IOServiceGetMatchingServices(MAIN_PORT_DEFAULT, Some(matching), &raw mut iterator)
    };
    if result != KERN_SUCCESS {
        return Err(RegistryError(format!(
            "IOServiceGetMatchingServices returned {result:#x}"
        )));
    }
    Ok(drain_iterator(iterator))
}

fn drain_iterator(iterator: io_iterator_t) -> Vec<IoObject> {
    let Some(iterator) = IoObject::new(iterator) else {
        return Vec::new();
    };
    let mut entries = Vec::new();
    while entries.len() < MAX_SERVICES {
        let Some(entry) = IoObject::new(IOIteratorNext(iterator.raw())) else {
            break;
        };
        entries.push(entry);
    }
    entries
}

fn read_usb_device(entry: &IoObject) -> Option<RegistryUsbDevice> {
    Some(RegistryUsbDevice {
        registry_id: registry_id(entry)?,
        location_id: number_property(entry, "locationID")
            .and_then(|value| u32::try_from(value).ok()),
        vendor_id: u16::try_from(number_property(entry, "idVendor")?).ok()?,
        product_id: u16::try_from(number_property(entry, "idProduct")?).ok()?,
        bcd_device: number_property(entry, "bcdDevice").and_then(|value| u16::try_from(value).ok()),
        device_class: number_property(entry, "bDeviceClass")
            .and_then(|value| u8::try_from(value).ok()),
        speed: number_property(entry, "USBSpeed").and_then(speed_from_registry),
        exclusive_owner: string_property(entry, "UsbExclusiveOwner"),
    })
}

fn usb_device_services() -> Result<Vec<(IoObject, RegistryUsbDevice)>, RegistryError> {
    Ok(matching_services(c"IOUSBHostDevice")?
        .into_iter()
        .filter_map(|entry| {
            let device = read_usb_device(&entry)?;
            Some((entry, device))
        })
        .collect())
}

/// Every attached USB device as the I/O Registry describes it.
///
/// Read-only: nothing is opened and nothing is sent to any device, so it is
/// safe to call unprivileged and with keyboards and mice attached.
///
/// # Errors
///
/// Returns a [`RegistryError`] when the registry cannot be queried.
pub fn usb_devices() -> Result<Vec<RegistryUsbDevice>, RegistryError> {
    Ok(usb_device_services()?
        .into_iter()
        .map(|(_, device)| device)
        .collect())
}

/// Every `IOHIDDevice` report descriptor the registry publishes. Read-only.
///
/// # Errors
///
/// Returns a [`RegistryError`] when the registry cannot be queried.
pub fn hid_devices() -> Result<Vec<RegistryHid>, RegistryError> {
    Ok(matching_services(c"IOHIDDevice")?
        .iter()
        .filter_map(|entry| {
            Some(RegistryHid {
                vendor_id: u16::try_from(number_property(entry, "VendorID")?).ok()?,
                product_id: u16::try_from(number_property(entry, "ProductID")?).ok()?,
                location_id: number_property(entry, "LocationID")
                    .and_then(|value| u32::try_from(value).ok()),
                interface: inherited_number(entry, "bInterfaceNumber")
                    .and_then(|value| u8::try_from(value).ok()),
                report_descriptor: data_property(entry, "ReportDescriptor")?,
            })
        })
        .collect())
}

/// The report descriptors macOS published for `device`, read before capture
/// (capture terminates the HID drivers that publish them).
///
/// # Errors
///
/// Returns a reason when the registry cannot be read or disagrees with itself.
pub fn report_descriptors_for(
    device: &RegistryUsbDevice,
) -> Result<Vec<InterfaceReportDescriptor>, String> {
    hid_descriptors_for(&hid_devices().map_err(|error| error.to_string())?, device)
}

/// The `IOUSBHostInterface` children of a device, with their interface
/// numbers. They exist, unregistered, after `configureWithValue` with
/// `matchInterfaces: NO`, so they are found by walking the service plane, not
/// by matching.
fn interface_services(device: &IoObject) -> Vec<(IoObject, u8)> {
    let mut iterator: io_iterator_t = 0;
    // SAFETY: `device` holds a live reference, the plane name is only read and
    // `iterator` is a valid out-pointer.
    let result = unsafe {
        IORegistryEntryGetChildIterator(device.raw(), io_name(kIOServicePlane), &raw mut iterator)
    };
    if result != KERN_SUCCESS {
        return Vec::new();
    }
    drain_iterator(iterator)
        .into_iter()
        .filter(|child| conforms_to(child, c"IOUSBHostInterface"))
        .filter_map(|child| {
            let number = u8::try_from(number_property(&child, "bInterfaceNumber")?).ok()?;
            Some((child, number))
        })
        .collect()
}

fn io_return_of(error: Option<&Retained<NSError>>) -> Option<IoReturn> {
    error.and_then(|error| IoReturn::try_from(error.code()).ok())
}

fn framework_error(stage: CaptureStage, error: Option<&Retained<NSError>>) -> CaptureError {
    io_return_of(error).map_or_else(
        || CaptureError::new(stage, "the framework reported no error code"),
        |code| CaptureError::from_io_return(stage, code),
    )
}

/// Copies `length` bytes the framework owns.
///
/// # Safety
///
/// `pointer` must be null or point to at least `length` readable bytes that
/// stay valid for the duration of the call.
unsafe fn copy_bytes(pointer: *const u8, length: usize) -> Option<Vec<u8>> {
    if pointer.is_null() {
        return None;
    }
    // SAFETY: the caller guarantees `length` readable bytes at `pointer`.
    Some(unsafe { std::slice::from_raw_parts(pointer, length) }.to_vec())
}

/// Lets a backend report an ending to the shared engine, connected after
/// [`arcen_usb_bridge::UsbExporter::new`] has taken the device.
///
/// An ending that arrives before the connection is held and delivered when
/// it is made.
#[derive(Debug, Clone, Default)]
pub struct EndLink(Arc<Mutex<EndLinkState>>);

#[derive(Debug, Default)]
struct EndLinkState {
    signal: Option<EndSignal>,
    pending: Option<ReleaseReason>,
}

impl EndLink {
    /// Connects the engine's [`EndSignal`]; an ending already seen is
    /// delivered now.
    pub fn connect(&self, signal: EndSignal) {
        let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(reason) = state.pending.take() {
            signal.signal(reason);
        }
        state.signal = Some(signal);
    }

    fn signal(&self, reason: ReleaseReason) {
        let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        match &state.signal {
            Some(signal) => signal.signal(reason),
            None => {
                state.pending.get_or_insert(reason);
            }
        }
    }
}

/// Transfers started on the device and not yet finished.
#[derive(Default)]
struct InFlight {
    entries: Mutex<BTreeMap<UrbId, Pending>>,
    drained: Condvar,
}

/// What carries a transfer, and what a cancellation may abort.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Channel {
    /// The pipe of this endpoint.
    Pipe(u8),
    /// Control requests on the default pipe, through this object.
    Control(ControlRoute),
    /// A synchronous call on a helper thread; it cannot be aborted.
    Synchronous,
}

#[derive(Debug, Clone, Copy)]
struct Pending {
    channel: Channel,
    cancelled: bool,
}

impl InFlight {
    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<UrbId, Pending>> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Marks `urb` cancelled and says which channel may now be aborted.
    fn cancel(&self, urb: UrbId) -> Option<Channel> {
        let mut entries = self.lock();
        let channel = entries.get_mut(&urb).map(|pending| {
            pending.cancelled = true;
            pending.channel
        })?;
        let on_channel: Vec<bool> = entries
            .values()
            .filter(|pending| pending.channel == channel)
            .map(|pending| pending.cancelled)
            .collect();
        pipe_may_abort(&on_channel).then_some(channel)
    }

    fn wait_drained(&self, limit: Duration) -> bool {
        let deadline = Instant::now() + limit;
        let mut entries = self.lock();
        while !entries.is_empty() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            entries = self
                .drained
                .wait_timeout(entries, remaining)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        true
    }
}

/// Removes its transfer from [`InFlight`] when the transfer finishes, or when
/// the framework drops a completion handler it never called.
struct InFlightGuard {
    table: Arc<InFlight>,
    urb: UrbId,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.table.lock().remove(&self.urb);
        self.table.drained.notify_all();
    }
}

/// A transfer between admission and its terminal completion. Shared between
/// the submitting thread (which completes it if the enqueue fails) and the
/// completion handler (which completes it otherwise); whoever takes it first
/// completes it, exactly once.
type Slot = Arc<Mutex<Option<(PendingCompletion, InFlightGuard)>>>;

fn take(slot: &Slot) -> Option<(PendingCompletion, InFlightGuard)> {
    slot.lock().unwrap_or_else(PoisonError::into_inner).take()
}

fn finish(slot: &Slot, status: BackendStatus, data: Vec<u8>) {
    if let Some((completion, guard)) = take(slot) {
        // The transfer stops counting as pending before the engine hears of it.
        drop(guard);
        completion.complete(status, data);
    }
}

/// One open interface and the pipes of its current alternate setting.
struct OpenInterface {
    number: u8,
    alternate: u8,
    object: Retained<IOUSBHostInterface>,
    pipes: Vec<(u8, Retained<IOUSBHostPipe>)>,
}

/// Every Objective-C object one capture owns.
struct Objects {
    device: Retained<IOUSBHostDevice>,
    interfaces: Vec<OpenInterface>,
}

impl Objects {
    /// The object a control request on `route` goes through. An interface
    /// that is not open (it always is while transfers flow) falls back to the
    /// device.
    fn control_object(&self, route: ControlRoute) -> &IOUSBHostObject {
        match route {
            ControlRoute::Interface(number) => self
                .interfaces
                .iter()
                .find(|open| open.number == number)
                .map_or(&**self.device, |open| &**open.object),
            ControlRoute::Device => &self.device,
        }
    }

    fn pipe(&self, endpoint: u8) -> Option<&Retained<IOUSBHostPipe>> {
        self.interfaces
            .iter()
            .flat_map(|interface| interface.pipes.iter())
            .find(|(address, _)| *address == endpoint)
            .map(|(_, pipe)| pipe)
    }
}

/// The state of one capture, shared by the device handle, its release thread
/// and its transfer helper threads.
struct Shared {
    objects: Mutex<Objects>,
    lifecycle: Arc<Mutex<Lifecycle>>,
    in_flight: Arc<InFlight>,
    outcome: Mutex<Option<ReleaseOutcome>>,
    end: EndLink,
    markers: MarkerStore,
    configuration: ParsedConfiguration,
    /// The registry record of the captured device, to recognise it going
    /// away and coming back during the hand-back.
    record: RegistryUsbDevice,
    /// Kept alive for as long as the device can call it.
    _interest: RcBlock<dyn Fn(NonNull<IOUSBHostObject>, u32, *mut c_void)>,
    queue: DispatchRetained<DispatchQueue>,
}

// SAFETY: `Shared` is `!Send`/`!Sync` only because of the Objective-C objects
// it holds (`IOUSBHost*`, the interest block, the dispatch queue). Their
// reference counting is atomic. `IOUSBHost` objects are not bound to a thread:
// Apple's design has completions arrive on a client-chosen queue while the
// client submits, aborts and destroys from its own threads, and requires that
// `destroy` is *not* called from that queue. Every use of the objects here is
// serialised by the `objects` mutex, except the completion handlers, which run
// on the I/O queue and touch only their own buffers. The interest block
// captures only `Send + Sync` state and is called by the framework, never by
// us.
unsafe impl Send for Shared {}
// SAFETY: see `Send` above; shared access is through mutexes.
unsafe impl Sync for Shared {}

impl Shared {
    fn objects(&self) -> std::sync::MutexGuard<'_, Objects> {
        self.objects.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The objects, for starting a transfer; `None` once the capture stopped
    /// admitting transfers (a release may be destroying them).
    fn objects_for_transfer(&self) -> Option<std::sync::MutexGuard<'_, Objects>> {
        // Refuse before waiting for the objects: a release holds them for
        // the whole hand-back.
        if !self.lifecycle().admits_transfers() {
            return None;
        }
        let objects = self.objects();
        self.lifecycle().admits_transfers().then_some(objects)
    }

    fn lifecycle(&self) -> std::sync::MutexGuard<'_, Lifecycle> {
        self.lifecycle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Records a transfer the device is about to start, or refuses it when
    /// the capture no longer admits transfers.
    fn admit(&self, completion: PendingCompletion, channel: Channel) -> Option<Slot> {
        if !self.lifecycle().admits_transfers() {
            completion.refuse(BackendRefusal::Disconnected);
            return None;
        }
        let urb = completion.urb();
        self.in_flight.lock().insert(
            urb,
            Pending {
                channel,
                cancelled: false,
            },
        );
        let guard = InFlightGuard {
            table: Arc::clone(&self.in_flight),
            urb,
        };
        Some(Arc::new(Mutex::new(Some((completion, guard)))))
    }

    /// Aborts, drains and destroys — once. Returns `None` when a release had
    /// already started.
    ///
    /// Never called on the I/O queue.
    fn release(&self, requested: ReleaseReason) -> Option<ReleaseOutcome> {
        let plan = self.lifecycle().begin_release(requested)?;
        eprintln!(
            "arcen-usb-iousbhost: releasing device, reason={}, destroy={:?}",
            plan.reason, plan.destroy
        );
        let objects = self.objects();
        // Asynchronous aborts: the aborted completions run on the I/O queue,
        // and this thread must never wait for that queue while it holds
        // anything a completion handler needs. The bounded drain below waits
        // for them instead.
        for interface in &objects.interfaces {
            for (_, pipe) in &interface.pipes {
                // SAFETY: a live pipe of this capture; aborting is valid in any
                // state and only fails for an already-terminated pipe.
                let _ = unsafe { pipe.abortWithOption_error(IOUSBHostAbortOption::Asynchronous) };
            }
        }
        // SAFETY: as above, for control requests on the default pipe, which
        // go through the device or one of its interfaces.
        let _ = unsafe {
            objects
                .device
                .abortDeviceRequestsWithOption_error(IOUSBHostAbortOption::Asynchronous)
        };
        for interface in &objects.interfaces {
            // SAFETY: as above.
            let _ = unsafe {
                interface
                    .object
                    .abortDeviceRequestsWithOption_error(IOUSBHostAbortOption::Asynchronous)
            };
        }
        // Not holding the objects while waiting: a submission that passed
        // admission just before the release began must be able to take them,
        // see the release under way and finish its transfer.
        drop(objects);
        if !self.in_flight.wait_drained(RELEASE_DRAIN) {
            eprintln!("arcen-usb-iousbhost: transfers still pending; destroying anyway");
        }
        let objects = self.objects();
        let handback = Handback::for_plan(plan, objects.interfaces.len());
        if handback == Handback::HeldReset {
            self.held_reset(&objects);
        } else {
            destroy_interfaces(&objects.interfaces);
            destroy_device(&objects.device, plan);
            if handback == Handback::PlainReset {
                observe_reenumeration(HandbackTimeline::new(&self.record), Instant::now());
            }
        }
        drop(objects);
        if let Err(error) = self.markers.clear() {
            eprintln!(
                "arcen-usb-iousbhost: device released, but the capture marker {} could not be \
                 removed: {error}",
                self.markers.path().display()
            );
        }
        self.lifecycle().finish_release();
        eprintln!(
            "arcen-usb-iousbhost: device released, reason={}",
            plan.reason
        );
        let outcome = ReleaseOutcome::Restored;
        *self.outcome.lock().unwrap_or_else(PoisonError::into_inner) = Some(outcome.clone());
        Some(outcome)
    }

    /// Resets the device with its interfaces still open, so the old device
    /// cannot finish terminating and the port stays empty; lets the
    /// interfaces go [`crate::handback::HANDBACK_HOLD`] after the old device
    /// was seen terminated. The timeline goes to the log.
    ///
    /// `destroy` runs on its own thread: should the framework wait for the
    /// termination it starts, this thread still lets the interfaces go.
    fn held_reset(&self, objects: &Objects) {
        let mut timeline = HandbackTimeline::new(&self.record);
        eprintln!(
            "arcen-usb-iousbhost: hand-back: resetting {:04x}:{:04x} registry={:#x} with {} \
             interface(s) held open",
            self.record.vendor_id,
            self.record.product_id,
            self.record.registry_id,
            objects.interfaces.len()
        );
        let device = HeldDevice(objects.device.clone());
        let (destroyed, destroy_done) = std::sync::mpsc::sync_channel(1);
        let start = Instant::now();
        let spawned = std::thread::Builder::new()
            .name("arcen-usb-iousbhost-destroy".to_owned())
            .spawn(move || {
                let device = device.into_inner();
                // SAFETY: the captured device, destroyed once, never on the
                // I/O queue. With `DeviceCapture` this resets the device and
                // re-registers its drivers.
                unsafe { device.destroy() };
                let _ = destroyed.send(());
            });
        if let Err(error) = spawned {
            // Without a second thread the interfaces cannot stay held while
            // `destroy` runs here, so this falls back to the plain reset:
            // interfaces first, then the device, as before the hold existed.
            eprintln!(
                "arcen-usb-iousbhost: hand-back: destroy thread: {error}; plain reset without \
                 a hold"
            );
            destroy_interfaces(&objects.interfaces);
            // SAFETY: as above, on the release thread, after its interfaces.
            unsafe { objects.device.destroy() };
            observe_reenumeration(timeline, start);
            return;
        }

        poll_registry(&mut timeline, start, TERMINATION_WAIT, |timeline| {
            timeline.terminated().is_some()
        });
        if let Some(until) = timeline.hold_until() {
            eprintln!(
                "arcen-usb-iousbhost: hand-back: old device terminated at {}; holding the port \
                 empty until +{}ms",
                offset(timeline.terminated()),
                until.as_millis()
            );
            poll_registry(&mut timeline, start, until, |timeline| {
                timeline.reenumerated().is_some()
            });
        } else {
            eprintln!(
                "arcen-usb-iousbhost: hand-back: old device still registered after +{}ms; \
                 letting the interfaces go",
                start.elapsed().as_millis()
            );
        }
        destroy_interfaces(&objects.interfaces);
        timeline.note_hold_released(start.elapsed());
        if timeline.came_back_during_hold() {
            eprintln!(
                "arcen-usb-iousbhost: hand-back: warning: the device re-enumerated at {} while \
                 its interfaces were held; the kernel did not wait for them",
                offset(timeline.reenumerated().map(|(at, _)| at))
            );
        }
        eprintln!(
            "arcen-usb-iousbhost: hand-back: interfaces released at +{}ms",
            start.elapsed().as_millis()
        );
        if let Err(std::sync::mpsc::RecvTimeoutError::Timeout) =
            destroy_done.recv_timeout(TERMINATION_WAIT)
        {
            eprintln!(
                "arcen-usb-iousbhost: hand-back: warning: destroy has not returned after +{}ms",
                start.elapsed().as_millis()
            );
        }
        if timeline.terminated().is_none() {
            let deadline = start.elapsed() + TERMINATION_WAIT;
            poll_registry(&mut timeline, start, deadline, |timeline| {
                timeline.terminated().is_some()
            });
        }
        if timeline.terminated().is_none() {
            eprintln!(
                "arcen-usb-iousbhost: hand-back: the old device did not go away; resetting it again"
            );
            let report = self.reset_again();
            eprintln!("arcen-usb-iousbhost: hand-back: second reset: {report:?}");
        }
        observe_reenumeration(timeline, start);
    }

    /// Resets the old device through a fresh capture, as crash recovery does.
    fn reset_again(&self) -> RecoveryReport {
        let registry_id = self.record.registry_id;
        match usb_device_services() {
            Ok(services) => services
                .iter()
                .find(|(_, device)| device.registry_id == registry_id)
                .map_or_else(
                    || RecoveryReport::Discarded {
                        reason: "the device went away".to_owned(),
                    },
                    |(service, _)| reset_device(service, registry_id),
                ),
            Err(error) => RecoveryReport::Failed {
                detail: error.to_string(),
            },
        }
    }

    fn stored_outcome(&self) -> ReleaseOutcome {
        self.outcome
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .unwrap_or_else(|| ReleaseOutcome::Failed("the release did not run".to_owned()))
    }
}

enum Command {
    /// The operating system ended the capture.
    Ended(ReleaseReason),
    Release {
        reason: ReleaseReason,
        reply: SyncSender<ReleaseOutcome>,
    },
}

fn release_thread(shared: &Shared, commands: &Receiver<Command>) {
    while let Ok(command) = commands.recv() {
        match command {
            // A requesting-close is honoured at once: the seizing client is
            // waiting. An unplug frees what the capture holds.
            Command::Ended(reason) => {
                let _ = shared.release(reason);
            }
            Command::Release { reason, reply } => {
                let outcome = shared
                    .release(reason)
                    .unwrap_or_else(|| shared.stored_outcome());
                let _ = reply.send(outcome);
            }
        }
        if shared.lifecycle().phase() == Phase::Released {
            break;
        }
    }
}

fn interest_block(
    lifecycle: Arc<Mutex<Lifecycle>>,
    end: EndLink,
    commands: Sender<Command>,
) -> RcBlock<dyn Fn(NonNull<IOUSBHostObject>, u32, *mut c_void)> {
    RcBlock::new(
        move |_object: NonNull<IOUSBHostObject>, message: u32, _argument: *mut c_void| {
            let reason = lifecycle
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .note_interest(message);
            if let Some(reason) = reason {
                eprintln!(
                    "arcen-usb-iousbhost: interest message {message:#010x}, ending with {reason}"
                );
                end.signal(reason);
                let _ = commands.send(Command::Ended(reason));
            }
        },
    )
}

fn completion_block(
    slot: Slot,
    buffer: Option<Retained<NSMutableData>>,
    returns_data: bool,
) -> RcBlock<dyn Fn(IoReturn, usize)> {
    RcBlock::new(move |status: IoReturn, transferred: usize| {
        let status = transfer_status(status);
        let data = match (&buffer, returns_data, status) {
            (Some(buffer), true, BackendStatus::Completed) => {
                let mut bytes = buffer.to_vec();
                bytes.truncate(transferred);
                bytes
            }
            _ => Vec::new(),
        };
        finish(&slot, status, data);
    })
}

/// One device captured with `IOUSBHost` `DeviceCapture`, presented to the
/// shared engine.
///
/// Dropping it releases the device exactly as [`CapturedUsbDevice::release`]
/// with [`ReleaseReason::SessionEnded`] does.
pub struct IoUsbHostDevice {
    facts: CapturedDeviceFacts,
    shared: Arc<Shared>,
    commands: Sender<Command>,
    released: bool,
}

impl std::fmt::Debug for IoUsbHostDevice {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("IoUsbHostDevice")
            .field("id", &self.facts.snapshot.id)
            .field("released", &self.released)
            .finish_non_exhaustive()
    }
}

impl IoUsbHostDevice {
    /// The link through which an ending the operating system reports reaches
    /// the shared engine: `link.connect(exporter.end_signal())`.
    #[must_use]
    pub fn end_link(&self) -> EndLink {
        self.shared.end.clone()
    }

    fn release_once(&mut self, reason: ReleaseReason) -> ReleaseOutcome {
        if self.released {
            return self.shared.stored_outcome();
        }
        self.released = true;
        let (reply, answer) = std::sync::mpsc::sync_channel(1);
        if self
            .commands
            .send(Command::Release { reason, reply })
            .is_err()
        {
            // The release thread finished after the operating system ended the
            // capture.
            return self.shared.stored_outcome();
        }
        match answer.recv_timeout(RELEASE_REPLY) {
            Ok(outcome) => outcome,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => self.shared.stored_outcome(),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => ReleaseOutcome::Failed(
                "the release did not finish in time; the device stays captured until the helper \
                 exits"
                    .to_owned(),
            ),
        }
    }

    /// Runs one synchronous framework call off the submitting thread.
    fn run_detached(
        &self,
        slot: Slot,
        work: impl FnOnce(&Shared) -> BackendStatus + Send + 'static,
    ) {
        let shared = Arc::clone(&self.shared);
        std::thread::spawn(move || {
            let status = work(&shared);
            finish(&slot, status, Vec::new());
        });
    }

    fn enqueue_pipe(
        &self,
        endpoint: EndpointAddress,
        buffer: &Retained<NSMutableData>,
        slot: &Slot,
        returns_data: bool,
    ) {
        let block = completion_block(Arc::clone(slot), Some(buffer.clone()), returns_data);
        let Some(objects) = self.shared.objects_for_transfer() else {
            finish(slot, BackendStatus::Disconnected, Vec::new());
            return;
        };
        let Some(pipe) = objects.pipe(endpoint.0) else {
            drop(objects);
            if let Some((completion, guard)) = take(slot) {
                drop(guard);
                completion.refuse(BackendRefusal::UnknownTarget);
            }
            return;
        };
        let mut error = None;
        // SAFETY: a live pipe of this capture, under the objects lock. The
        // buffer stays alive in the block; the framework copies the block
        // and calls it once on the I/O queue. Interrupt pipes require a zero
        // completion timeout: an interrupt IN stays pending until the device
        // reports or the transfer is aborted (the shared hold rule).
        let enqueued = unsafe {
            pipe.enqueueIORequestWithData_completionTimeout_error_completionHandler(
                Some(buffer),
                0.0,
                Some(&mut error),
                RcBlock::as_ptr(&block),
            )
        };
        drop(objects);
        if !enqueued {
            let status = io_return_of(error.as_ref()).map_or(BackendStatus::Io, transfer_status);
            finish(slot, status, Vec::new());
        }
    }
}

impl CapturedUsbDevice for IoUsbHostDevice {
    fn facts(&self) -> &CapturedDeviceFacts {
        &self.facts
    }

    fn submit_control(
        &self,
        operation: ControlOperation,
        timeout: Duration,
        completion: PendingCompletion,
    ) {
        match operation {
            ControlOperation::Transfer {
                setup,
                data,
                in_length,
            } => {
                let is_in = setup.request_type & 0x80 != 0;
                let length = if is_in { in_length } else { data.len() };
                let Ok(w_length) = u16::try_from(length) else {
                    completion.refuse(BackendRefusal::Unsupported);
                    return;
                };
                let route =
                    control_route(&self.shared.configuration, setup.request_type, setup.index);
                let Some(slot) = self.shared.admit(completion, Channel::Control(route)) else {
                    return;
                };
                let buffer = if is_in {
                    NSMutableData::dataWithLength(length)
                } else if data.is_empty() {
                    None
                } else {
                    Some(NSMutableData::with_bytes(&data))
                };
                if length > 0 && buffer.is_none() {
                    finish(&slot, BackendStatus::Io, Vec::new());
                    return;
                }
                let request = IOUSBDeviceRequest {
                    bmRequestType: setup.request_type,
                    bRequest: setup.request,
                    wValue: setup.value,
                    wIndex: setup.index,
                    wLength: w_length,
                };
                let block = completion_block(Arc::clone(&slot), buffer.clone(), is_in);
                let Some(objects) = self.shared.objects_for_transfer() else {
                    finish(&slot, BackendStatus::Disconnected, Vec::new());
                    return;
                };
                let mut error = None;
                // SAFETY: a live device or interface object of this capture,
                // under the objects lock. The buffer is kept alive by the
                // block, which the framework copies and calls once on the I/O
                // queue.
                let enqueued = unsafe {
                    objects
                        .control_object(route)
                        .enqueueDeviceRequest_data_completionTimeout_error_completionHandler(
                            request,
                            buffer.as_deref(),
                            timeout.as_secs_f64(),
                            Some(&mut error),
                            RcBlock::as_ptr(&block),
                        )
                };
                drop(objects);
                if !enqueued {
                    let status =
                        io_return_of(error.as_ref()).map_or(BackendStatus::Io, transfer_status);
                    finish(&slot, status, Vec::new());
                }
            }
            ControlOperation::SelectAlternateSetting {
                interface,
                alternate,
            } => {
                let Some(slot) = self.shared.admit(completion, Channel::Synchronous) else {
                    return;
                };
                self.run_detached(slot, move |shared| {
                    select_alternate(shared, interface.0, alternate.0)
                });
            }
            ControlOperation::ClearHalt { endpoint } => {
                let Some(slot) = self.shared.admit(completion, Channel::Synchronous) else {
                    return;
                };
                self.run_detached(slot, move |shared| {
                    let Some(objects) = shared.objects_for_transfer() else {
                        return BackendStatus::Disconnected;
                    };
                    let Some(pipe) = objects.pipe(endpoint.0) else {
                        return BackendStatus::Protocol;
                    };
                    // SAFETY: a live pipe of this capture, under the objects
                    // lock, on a helper thread rather than the I/O queue.
                    match unsafe { pipe.clearStallWithError() } {
                        Ok(()) => BackendStatus::Completed,
                        Err(error) => {
                            io_return_of(Some(&error)).map_or(BackendStatus::Io, transfer_status)
                        }
                    }
                });
            }
        }
    }

    fn submit_interrupt_in(
        &self,
        endpoint: EndpointAddress,
        length: usize,
        completion: PendingCompletion,
    ) {
        let Some(slot) = self.shared.admit(completion, Channel::Pipe(endpoint.0)) else {
            return;
        };
        let Some(buffer) = NSMutableData::dataWithLength(length) else {
            finish(&slot, BackendStatus::Io, Vec::new());
            return;
        };
        self.enqueue_pipe(endpoint, &buffer, &slot, true);
    }

    /// Interrupt pipes take no completion timeout in `IOUSBHost`; an OUT
    /// transfer the device never accepts ends when it is cancelled or the
    /// device is released.
    fn submit_interrupt_out(
        &self,
        endpoint: EndpointAddress,
        data: Vec<u8>,
        _timeout: Duration,
        completion: PendingCompletion,
    ) {
        let Some(slot) = self.shared.admit(completion, Channel::Pipe(endpoint.0)) else {
            return;
        };
        self.enqueue_pipe(endpoint, &NSMutableData::with_bytes(&data), &slot, false);
    }

    /// `IOUSBHost` aborts a whole pipe (or every control request of one
    /// object), not one transfer, so it is aborted only once everything
    /// pending on it was cancelled
    /// ([`pipe_may_abort`]); otherwise the transfer is left to finish and the
    /// engine discards its late completion.
    fn cancel(&self, urb: UrbId) {
        let Some(channel) = self.shared.in_flight.cancel(urb) else {
            return;
        };
        let objects = self.shared.objects();
        let _ = match channel {
            // SAFETY: a live pipe of this capture, under the objects lock;
            // asynchronous, so this thread never waits on the I/O queue.
            Channel::Pipe(endpoint) => objects.pipe(endpoint).map(|pipe| unsafe {
                pipe.abortWithOption_error(IOUSBHostAbortOption::Asynchronous)
            }),
            // SAFETY: as above, for the default pipe through the object that
            // carried the request.
            Channel::Control(route) => Some(unsafe {
                objects
                    .control_object(route)
                    .abortDeviceRequestsWithOption_error(IOUSBHostAbortOption::Asynchronous)
            }),
            Channel::Synchronous => None,
        };
    }

    fn release(mut self: Box<Self>, reason: ReleaseReason) -> ReleaseOutcome {
        self.release_once(reason)
    }
}

impl Drop for IoUsbHostDevice {
    fn drop(&mut self) {
        if !self.released {
            let _ = self.release_once(ReleaseReason::SessionEnded);
        }
    }
}

fn select_alternate(shared: &Shared, interface: u8, alternate: u8) -> BackendStatus {
    let Some(mut objects) = shared.objects_for_transfer() else {
        return BackendStatus::Disconnected;
    };
    let Some(open) = objects
        .interfaces
        .iter_mut()
        .find(|open| open.number == interface)
    else {
        return BackendStatus::Protocol;
    };
    // SAFETY: a live interface of this capture, under the objects lock, on a
    // helper thread rather than the I/O queue. Pending transfers on its pipes
    // are aborted by the framework when the setting changes.
    if let Err(error) = unsafe {
        open.object
            .selectAlternateSetting_error(usize::from(alternate))
    } {
        return io_return_of(Some(&error)).map_or(BackendStatus::Io, transfer_status);
    }
    open.alternate = alternate;
    let mut pipes = Vec::new();
    for address in pipes_for(&shared.configuration, interface, alternate) {
        // SAFETY: as above; the address comes from the parsed configuration
        // of this very interface and setting.
        match unsafe {
            open.object
                .copyPipeWithAddress_error(usize::from(address.0))
        } {
            Ok(pipe) => pipes.push((address.0, pipe)),
            Err(error) => {
                open.pipes.clear();
                return io_return_of(Some(&error)).map_or(BackendStatus::Io, transfer_status);
            }
        }
    }
    open.pipes = pipes;
    BackendStatus::Completed
}

/// Reads the descriptors the captured device reports about itself.
fn read_descriptors(device: &IOUSBHostDevice) -> Result<(Vec<u8>, Vec<u8>), CaptureError> {
    let stage = CaptureStage::Descriptors;
    // SAFETY: the framework returns a pointer to the device descriptor it
    // caches for this live object (or null); 18 bytes are copied at once.
    let device_descriptor = unsafe {
        copy_bytes(
            device.deviceDescriptor().cast::<u8>(),
            DEVICE_DESCRIPTOR_LENGTH,
        )
    }
    .ok_or_else(|| CaptureError::new(stage, "no device descriptor"))?;
    // SAFETY: as above, for the current configuration descriptor, which may
    // be null for an unconfigured device.
    let mut configuration = unsafe { device.configurationDescriptor() }.cast::<u8>();
    if configuration.is_null() {
        let mut error = None;
        // SAFETY: the live device; index 0 is the first configuration.
        configuration =
            unsafe { device.configurationDescriptorWithIndex_error(0, Some(&mut error)) }
                .cast::<u8>();
        if configuration.is_null() {
            return Err(framework_error(stage, error.as_ref()));
        }
    }
    // SAFETY: a configuration descriptor starts with its 9-byte header; only
    // its first 4 bytes are read to learn the total length.
    let header = unsafe { copy_bytes(configuration, 4) }
        .ok_or_else(|| CaptureError::new(stage, "no configuration descriptor"))?;
    let total =
        configuration_total_length(&header).map_err(|reason| CaptureError::new(stage, reason))?;
    // SAFETY: the framework's descriptor is `wTotalLength` bytes long, and
    // that length was bounded above.
    let configuration = unsafe { copy_bytes(configuration, total) }
        .ok_or_else(|| CaptureError::new(stage, "no configuration descriptor"))?;
    Ok((device_descriptor, configuration))
}

/// Captures the classified device.
///
/// The marker is written first, so a crash at any later point is recovered
/// by the next helper start ([`recover`]). Then: `DeviceCapture` (which
/// terminates the HID drivers), the descriptors read back from the device,
/// `configureWithValue` with `matchInterfaces: NO` (so they do not come back),
/// the `IOUSBHostInterface` services awaited and opened, their interrupt pipes
/// copied — all on one serial I/O queue. Any failure releases whatever was
/// taken (the device is reset and its drivers re-match) and removes the
/// marker.
///
/// `report_descriptors` are the descriptors classification read from the
/// registry before capture; the shared engine re-checks them together with
/// the descriptors read here.
///
/// # Errors
///
/// Returns a [`CaptureError`] naming the failed step.
pub fn capture(
    target: &CaptureTarget,
    report_descriptors: Vec<InterfaceReportDescriptor>,
    markers: &MarkerStore,
) -> Result<IoUsbHostDevice, CaptureError> {
    let services = usb_device_services()
        .map_err(|error| CaptureError::new(CaptureStage::Lookup, error.to_string()))?;
    let devices: Vec<RegistryUsbDevice> =
        services.iter().map(|(_, device)| device.clone()).collect();
    let (found, matched_by) = select_target(&devices, target)
        .map_err(|error| CaptureError::new(CaptureStage::Lookup, error.to_string()))?;
    let found = found.clone();
    let Some((service, _)) = services
        .iter()
        .find(|(_, device)| device.registry_id == found.registry_id)
    else {
        return Err(CaptureError::new(
            CaptureStage::Lookup,
            "the device disappeared",
        ));
    };
    let target = CaptureTarget::of(&found);
    eprintln!(
        "arcen-usb-iousbhost: capturing {:04x}:{:04x} registry={:#x} location={:?} by {matched_by:?}",
        found.vendor_id, found.product_id, found.registry_id, found.location_id
    );
    let boot_session = boot_session_uuid().ok_or_else(|| {
        CaptureError::new(CaptureStage::Marker, "cannot read kern.bootsessionuuid")
    })?;
    markers
        .write(&CaptureMarker::for_target(
            &target,
            std::process::id(),
            &boot_session,
        ))
        .map_err(|error| CaptureError::new(CaptureStage::Marker, error.to_string()))?;

    let queue = DispatchQueue::new(IO_QUEUE_LABEL, DispatchQueueAttr::SERIAL);
    let lifecycle = Arc::new(Mutex::new(Lifecycle::new()));
    let end = EndLink::default();
    let (commands, receiver) = std::sync::mpsc::channel();
    let interest = interest_block(Arc::clone(&lifecycle), end.clone(), commands.clone());
    let mut error = None;
    // SAFETY: `service` is a live IOUSBHostDevice registry entry; the queue is
    // serial and outlives the object (kept in `Shared`); the interest block is
    // valid and kept alive in `Shared` too. The object takes its own reference
    // to the service.
    let device = unsafe {
        IOUSBHostDevice::initWithIOService_options_queue_error_interestHandler(
            IOUSBHostDevice::alloc(),
            service.raw(),
            IOUSBHostObjectInitOptions::DeviceCapture,
            Some(&queue),
            Some(&mut error),
            RcBlock::as_ptr(&interest),
        )
    };
    let Some(device) = device else {
        let _ = markers.clear();
        return Err(framework_error(CaptureStage::Capture, error.as_ref()));
    };
    let mut shared = Shared {
        objects: Mutex::new(Objects {
            device,
            interfaces: Vec::new(),
        }),
        lifecycle,
        in_flight: Arc::new(InFlight::default()),
        outcome: Mutex::new(None),
        end,
        markers: markers.clone(),
        record: found.clone(),
        configuration: arcen_usb_bridge::ParsedConfiguration {
            configuration_value: 0,
            interfaces: Vec::new(),
        },
        _interest: interest,
        queue,
    };
    match set_up(&mut shared, service, &found, &target, report_descriptors) {
        Ok(facts) => {
            let shared = Arc::new(shared);
            let thread_shared = Arc::clone(&shared);
            std::thread::Builder::new()
                .name("arcen-usb-iousbhost-release".to_owned())
                .spawn(move || release_thread(&thread_shared, &receiver))
                .map_err(|error| {
                    let _ = shared.release(ReleaseReason::Error);
                    CaptureError::new(CaptureStage::Capture, format!("release thread: {error}"))
                })?;
            Ok(IoUsbHostDevice {
                facts,
                shared,
                commands,
                released: false,
            })
        }
        Err(error) => {
            let _ = shared.release(ReleaseReason::Error);
            Err(error)
        }
    }
}

fn set_up(
    shared: &mut Shared,
    service: &IoObject,
    found: &RegistryUsbDevice,
    target: &CaptureTarget,
    report_descriptors: Vec<InterfaceReportDescriptor>,
) -> Result<CapturedDeviceFacts, CaptureError> {
    let (device_descriptor, configuration) = read_descriptors(&shared.objects().device)?;
    let facts = captured_facts(
        &device_descriptor,
        &configuration,
        found.speed,
        target,
        report_descriptors,
    )
    .map_err(|reason| CaptureError::new(CaptureStage::Descriptors, reason))?;
    let parsed = facts.snapshot.configuration.clone();
    {
        let objects = shared.objects();
        // SAFETY: the live, captured device; not on the I/O queue. `NO` keeps
        // the HID drivers from matching the interfaces while captured.
        unsafe {
            objects.device.configureWithValue_matchInterfaces_error(
                usize::from(parsed.configuration_value),
                false,
            )
        }
        .map_err(|error| framework_error(CaptureStage::Configure, Some(&error)))?;
    }

    let deadline = Instant::now() + INTERFACE_WAIT;
    let children = loop {
        let children = interface_services(service);
        let numbers: Vec<u8> = children.iter().map(|(_, number)| *number).collect();
        let missing = missing_interfaces(&parsed, &numbers);
        if missing.is_empty() {
            break children;
        }
        if Instant::now() >= deadline {
            return Err(CaptureError::new(
                CaptureStage::Interfaces,
                format!("interfaces {missing:?} did not appear"),
            ));
        }
        std::thread::sleep(INTERFACE_POLL);
    };

    let queue = shared.queue.clone();
    let mut opened: Vec<OpenInterface> = Vec::new();
    for (child, number) in &children {
        if opened.iter().any(|open| open.number == *number) {
            continue;
        }
        let mut error = None;
        // SAFETY: a live IOUSBHostInterface entry of the captured device; the
        // same serial I/O queue as the device; no interest handler.
        let object = unsafe {
            IOUSBHostInterface::initWithIOService_options_queue_error_interestHandler(
                IOUSBHostInterface::alloc(),
                child.raw(),
                IOUSBHostObjectInitOptions::empty(),
                Some(&queue),
                Some(&mut error),
                std::ptr::null_mut(),
            )
        };
        let Some(object) = object else {
            destroy_interfaces(&opened);
            return Err(framework_error(CaptureStage::Interfaces, error.as_ref()));
        };
        // SAFETY: the framework's interface descriptor for this live object;
        // byte 3 is `bAlternateSetting`.
        let alternate = unsafe {
            copy_bytes(
                object
                    .interfaceDescriptor()
                    .as_ptr()
                    .cast::<u8>()
                    .cast_const(),
                4,
            )
        }
        .map_or(0, |bytes| bytes[3]);
        let mut pipes = Vec::new();
        for address in pipes_for(&parsed, *number, alternate) {
            // SAFETY: the live interface just opened; the address comes from
            // the parsed configuration of this interface and setting.
            match unsafe { object.copyPipeWithAddress_error(usize::from(address.0)) } {
                Ok(pipe) => pipes.push((address.0, pipe)),
                Err(error) => {
                    // SAFETY: destroying the interface opened just above.
                    unsafe { object.destroy() };
                    destroy_interfaces(&opened);
                    return Err(framework_error(CaptureStage::Pipes, Some(&error)));
                }
            }
        }
        opened.push(OpenInterface {
            number: *number,
            alternate,
            object,
            pipes,
        });
    }
    shared.objects().interfaces = opened;
    shared.configuration = parsed;
    shared
        .lifecycle()
        .activate()
        .map_err(|error| CaptureError::new(CaptureStage::Capture, error.to_string()))?;
    Ok(facts)
}

fn destroy_interfaces(opened: &[OpenInterface]) {
    for open in opened {
        // SAFETY: interfaces this capture opened; not on the I/O queue.
        unsafe { open.object.destroy() };
    }
}

/// Destroys the device as `plan` says, without a hold.
fn destroy_device(device: &IOUSBHostDevice, plan: ReleasePlan) {
    match plan.destroy {
        // SAFETY: not on the I/O queue. With `DeviceCapture` this resets the
        // device and re-registers its drivers.
        DestroyMode::ResetAndRematch => unsafe { device.destroy() },
        // SAFETY: as above; surrender skips the reset so the client that
        // requested the device gets it as it is.
        DestroyMode::Surrender => unsafe {
            device.destroyWithOptions(IOUSBHostObjectDestroyOptions::DeviceSurrender);
        },
    }
}

/// The captured device, moved to the thread that destroys it.
struct HeldDevice(Retained<IOUSBHostDevice>);

impl HeldDevice {
    // Taking `self` whole keeps the closure from capturing only the field.
    fn into_inner(self) -> Retained<IOUSBHostDevice> {
        self.0
    }
}

// SAFETY: as for `Shared`: `IOUSBHostDevice` is not bound to a thread, its
// reference count is atomic, and this thread only calls `destroy` (which the
// framework documents as callable once from any thread but the I/O queue;
// repeated calls have no effect).
unsafe impl Send for HeldDevice {}

/// Waits, bounded, for the device to come back, then logs the timeline
/// and what it amounted to.
fn observe_reenumeration(mut timeline: HandbackTimeline, start: Instant) {
    let deadline = start.elapsed() + REENUMERATION_WAIT;
    poll_registry(&mut timeline, start, deadline, |timeline| {
        timeline.reenumerated().is_some()
    });
    eprintln!("arcen-usb-iousbhost: hand-back: {timeline}");
    let verdict = timeline.verdict();
    let warning = match verdict {
        HandbackVerdict::Reenumerated { held: true, .. } => "",
        _ => "warning: ",
    };
    eprintln!("arcen-usb-iousbhost: hand-back: {warning}{verdict}");
}

/// Reads the registry into `timeline` every [`HANDBACK_POLL`] until `done`
/// or until `deadline` (an offset from `start`) has passed.
fn poll_registry(
    timeline: &mut HandbackTimeline,
    start: Instant,
    deadline: Duration,
    done: impl Fn(&HandbackTimeline) -> bool,
) {
    loop {
        if let Ok(devices) = usb_devices() {
            timeline.observe(start.elapsed(), &devices);
        }
        if done(timeline) || start.elapsed() >= deadline {
            return;
        }
        std::thread::sleep(HANDBACK_POLL);
    }
}

fn offset(at: Option<Duration>) -> String {
    at.map_or_else(|| "-".to_owned(), |at| format!("+{}ms", at.as_millis()))
}

/// `kern.bootsessionuuid`: a new value at every boot. `None` when it cannot
/// be read.
#[must_use]
pub fn boot_session_uuid() -> Option<String> {
    let mut buffer = [0_u8; 64];
    let mut length = buffer.len();
    // SAFETY: the name is a NUL-terminated C string; `buffer` and `length`
    // describe a writable buffer the call fills and shortens; no new value is
    // set (null pointer, zero length).
    let result = unsafe {
        libc::sysctlbyname(
            c"kern.bootsessionuuid".as_ptr(),
            buffer.as_mut_ptr().cast(),
            &raw mut length,
            std::ptr::null_mut(),
            0,
        )
    };
    if result != 0 {
        return None;
    }
    let text = std::str::from_utf8(&buffer[..length.min(buffer.len())])
        .ok()?
        .trim_end_matches('\0');
    (!text.is_empty()).then(|| text.to_owned())
}

/// What a helper starting up did about the captured marker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryReport {
    /// No capture was in progress.
    Nothing,
    /// The marker was removed and no device touched.
    Discarded { reason: String },
    /// The device was captured again and destroyed: it was reset and its
    /// drivers re-matched.
    Restored { registry_id: u64 },
    /// Recovery could not finish; `detail` says what is left.
    Failed { detail: String },
}

/// Recovers from a helper that stopped while it held a device.
///
/// Call once at daemon start, before serving. See [`crate::marker`].
#[must_use]
pub fn recover(markers: &MarkerStore) -> RecoveryReport {
    let read = markers.read();
    if read == MarkerRead::Absent {
        return RecoveryReport::Nothing;
    }
    let services = match usb_device_services() {
        Ok(services) => services,
        Err(error) => {
            // Keep the marker: the next start may read the registry.
            return RecoveryReport::Failed {
                detail: error.to_string(),
            };
        }
    };
    let devices: Vec<RegistryUsbDevice> =
        services.iter().map(|(_, device)| device.clone()).collect();
    let report = match plan_recovery(&read, boot_session_uuid().as_deref(), &devices) {
        RecoveryPlan::Nothing => return RecoveryReport::Nothing,
        RecoveryPlan::Discard { reason } => RecoveryReport::Discarded { reason },
        RecoveryPlan::ResetDevice { registry_id } => {
            match services
                .iter()
                .find(|(_, device)| device.registry_id == registry_id)
            {
                Some((service, _)) => reset_device(service, registry_id),
                None => RecoveryReport::Discarded {
                    reason: "the device disappeared during recovery".to_owned(),
                },
            }
        }
    };
    // Whatever happened, retrying at every start cannot do better: unplugging
    // the device restores it.
    if let Err(error) = markers.clear() {
        return RecoveryReport::Failed {
            detail: format!("{report:?}; the marker could not be removed: {error}"),
        };
    }
    report
}

fn reset_device(service: &IoObject, registry_id: u64) -> RecoveryReport {
    let mut error = None;
    // SAFETY: a live IOUSBHostDevice registry entry; with no queue the
    // framework creates its own; no interest handler.
    let device = unsafe {
        IOUSBHostDevice::initWithIOService_options_queue_error_interestHandler(
            IOUSBHostDevice::alloc(),
            service.raw(),
            IOUSBHostObjectInitOptions::DeviceCapture,
            None,
            Some(&mut error),
            std::ptr::null_mut(),
        )
    };
    let Some(device) = device else {
        return RecoveryReport::Failed {
            detail: framework_error(CaptureStage::Capture, error.as_ref()).to_string(),
        };
    };
    // SAFETY: the object just created, on this thread (no completion handler
    // exists). `destroy` after `DeviceCapture` resets the device and lets its
    // drivers re-match.
    unsafe { device.destroy() };
    RecoveryReport::Restored { registry_id }
}
