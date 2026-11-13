// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The bridged tablet's HID devices, owned by the entitled injector child
//! (ADR 0022 §2, §9).
//!
//! `arcen-pier-macos hid-injector --bridged-usb` is the same child the Agent
//! Helper already starts for its virtual keyboard: the Pier's own executable,
//! the only signature carrying `com.apple.developer.hid.virtual.device`. In
//! this mode it creates one `IOHIDUserDevice` per HID interface the session
//! planned ([`arcen_usb_bridge::HidHostAdapter`]), hands every input report it
//! is given to its device, and passes the host's get/set report calls back
//! to the session, which turns them into USB control transfers on the bridged
//! device. It owns the devices and nothing else: the plan, the policy and the
//! network stay in the session.
//!
//! When its input closes — the session ended, the agent exited or crashed —
//! it cancels every device and exits; the kernel removes a device with the
//! process that created it in any case.

use std::collections::HashMap;
use std::io::Write;
use std::process::ExitCode;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use arcen_usb_bridge::hid_device_ipc::HidDeviceFrame;
use arcen_usb_bridge::{BridgedHidDevice, HID_HOST_CONTROL_DEADLINE, MAX_INTERFACES};

/// How long a get/set report call waits for the session's answer.
pub const REPORT_CALL_TIMEOUT: Duration = HID_HOST_CONTROL_DEADLINE;
/// `RequestTimeout` advertised on every device, in microseconds.
pub const REQUEST_TIMEOUT_MICROS: u32 = 3_000_000;
/// The HID system attaches a new device asynchronously; a report sent the
/// instant it is activated can be dropped (measured for the keyboard).
const DEVICE_SETTLE: Duration = Duration::from_millis(300);

/// A host report call waiting for the session's answer.
type Waiters = Arc<Mutex<HashMap<u32, SyncSender<(bool, Vec<u8>)>>>>;

/// Where the child's frames to the session go; shared by the main loop and
/// the devices' report callbacks.
#[derive(Clone)]
pub struct Uplink {
    output: Arc<Mutex<Box<dyn Write + Send>>>,
    waiters: Waiters,
    next_token: Arc<AtomicU32>,
}

impl std::fmt::Debug for Uplink {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Uplink").finish_non_exhaustive()
    }
}

/// One host report call, as a device callback sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReportCall {
    Get {
        index: u8,
        kind: u8,
        report_id: u8,
        length: u16,
    },
    Set {
        index: u8,
        kind: u8,
        report_id: u8,
        data: Vec<u8>,
    },
}

impl Uplink {
    /// An uplink writing frames to `output`.
    #[must_use]
    pub fn new(output: Box<dyn Write + Send>) -> Self {
        Self {
            output: Arc::new(Mutex::new(output)),
            waiters: Arc::new(Mutex::new(HashMap::new())),
            next_token: Arc::new(AtomicU32::new(1)),
        }
    }

    /// Sends one frame.
    ///
    /// # Errors
    ///
    /// The write error when the session has gone.
    pub fn send(&self, frame: &HidDeviceFrame) -> std::io::Result<()> {
        let mut output = self.output.lock().unwrap_or_else(PoisonError::into_inner);
        frame.write_to(&mut *output)
    }

    /// Asks the session to perform a host report call and waits for the
    /// answer: `Some(data)` on success, `None` when the device refused, the
    /// session failed it, or [`REPORT_CALL_TIMEOUT`] passed.
    #[must_use]
    pub fn call(&self, call: ReportCall) -> Option<Vec<u8>> {
        let token = self.next_token.fetch_add(1, Ordering::Relaxed);
        let (sender, answer) = sync_channel(1);
        self.waiters
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(token, sender);
        let frame = match call {
            ReportCall::Get {
                index,
                kind,
                report_id,
                length,
            } => HidDeviceFrame::GetReport {
                token,
                index,
                kind,
                report_id,
                length,
            },
            ReportCall::Set {
                index,
                kind,
                report_id,
                data,
            } => HidDeviceFrame::SetReport {
                token,
                index,
                kind,
                report_id,
                data,
            },
        };
        let answer = if self.send(&frame).is_ok() {
            answer.recv_timeout(REPORT_CALL_TIMEOUT).ok()
        } else {
            None
        };
        self.waiters
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&token);
        answer.and_then(|(ok, data)| ok.then_some(data))
    }

    /// Delivers the session's answer to a waiting call; a late or unknown
    /// token is dropped.
    pub fn answer(&self, token: u32, ok: bool, data: Vec<u8>) {
        if let Some(waiter) = self
            .waiters
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&token)
        {
            let _ = waiter.try_send((ok, data));
        }
    }
}

/// What owns the published devices; a fake in tests.
pub trait DeviceSet {
    /// Hands one input report to device `index`.
    ///
    /// # Errors
    ///
    /// Why the system refused it.
    fn report(&self, index: u8, report: &[u8]) -> Result<(), String>;
}

/// Reads the plan from `input`: `Create` frames, then `Activate`.
///
/// # Errors
///
/// Why the plan is out of contract.
pub fn read_plan(input: &mut impl std::io::Read) -> Result<Vec<BridgedHidDevice>, String> {
    let mut plan: Vec<BridgedHidDevice> = Vec::new();
    loop {
        match HidDeviceFrame::read_from(input).map_err(|error| error.to_string())? {
            Some(HidDeviceFrame::Create(device)) => {
                if usize::from(device.index) != plan.len() || plan.len() >= MAX_INTERFACES {
                    return Err("devices must be described once each, in order".to_owned());
                }
                plan.push(device);
            }
            Some(HidDeviceFrame::Activate) if !plan.is_empty() => return Ok(plan),
            Some(_) => return Err("unexpected frame before activation".to_owned()),
            None => return Err("the session closed before activation".to_owned()),
        }
    }
}

/// Serves `devices` until the session closes `input`: reports go to their
/// device, answers to their waiting call. Returns why it stopped.
///
/// # Errors
///
/// A frame out of contract or a report the system refused.
pub fn serve(
    input: &mut impl std::io::Read,
    devices: &impl DeviceSet,
    count: usize,
    uplink: &Uplink,
) -> Result<(), String> {
    loop {
        match HidDeviceFrame::read_from(input).map_err(|error| error.to_string())? {
            None => return Ok(()),
            Some(HidDeviceFrame::Report { index, report }) if usize::from(index) < count => {
                devices.report(index, &report)?;
            }
            Some(HidDeviceFrame::ReportReply { token, ok, data }) => uplink.answer(token, ok, data),
            Some(_) => return Err("unexpected frame from the session".to_owned()),
        }
    }
}

/// Runs the child in bridged mode on standard input and output.
#[must_use]
pub fn run_bridged_injector() -> ExitCode {
    let uplink = Uplink::new(Box::new(std::io::stdout()));
    let mut input = std::io::stdin().lock();
    let plan = match read_plan(&mut input) {
        Ok(plan) => plan,
        Err(reason) => {
            eprintln!("hid-injector: {reason}");
            return ExitCode::FAILURE;
        }
    };
    let devices = match native::Devices::create(&plan, &uplink) {
        Ok(devices) => devices,
        Err(reason) => {
            let _ = uplink.send(&HidDeviceFrame::Refused { reason });
            return ExitCode::FAILURE;
        }
    };
    std::thread::sleep(DEVICE_SETTLE);
    let ready = HidDeviceFrame::Ready {
        devices: u8::try_from(plan.len()).unwrap_or(u8::MAX),
    };
    if uplink.send(&ready).is_err() {
        return ExitCode::FAILURE;
    }
    let outcome = serve(&mut input, &devices, plan.len(), &uplink);
    drop(devices);
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(reason) => {
            eprintln!("hid-injector: {reason}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(target_os = "macos")]
mod native {
    #![allow(unsafe_code)]

    use std::ffi::c_void;
    use std::ptr::NonNull;

    use block2::RcBlock;
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2_core_foundation::CFDictionary;
    use objc2_foundation::{NSData, NSDictionary, NSNumber, NSString};

    use super::{REQUEST_TIMEOUT_MICROS, ReportCall, Uplink};
    use arcen_usb_bridge::BridgedHidDevice;

    const K_IO_RETURN_SUCCESS: i32 = 0;
    #[allow(clippy::cast_possible_wrap)]
    const K_IO_RETURN_ERROR: i32 = 0xE000_02BC_u32 as i32;
    #[allow(clippy::cast_possible_wrap)]
    const K_IO_RETURN_BAD_ARGUMENT: i32 = 0xE000_02C2_u32 as i32;

    type SetReportBlock = RcBlock<dyn Fn(u32, u32, *const u8, isize) -> i32>;
    type GetReportBlock = RcBlock<dyn Fn(u32, u32, *mut u8, *mut isize) -> i32>;

    // SAFETY: signatures match `IOKit/hidsystem/IOHIDUserDevice.h` (macOS
    // 10.15+), `<dispatch/queue.h>` and `CFBase.h`; references are opaque
    // pointers, `IOHIDReportType` is a C enum (32-bit) and `CFIndex` is
    // `isize`.
    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IOHIDUserDeviceCreateWithProperties(
            allocator: *const c_void,
            properties: *const CFDictionary,
            options: u32,
        ) -> *mut c_void;
        fn IOHIDUserDeviceRegisterGetReportBlock(
            device: *mut c_void,
            block: *const block2::Block<dyn Fn(u32, u32, *mut u8, *mut isize) -> i32>,
        );
        fn IOHIDUserDeviceRegisterSetReportBlock(
            device: *mut c_void,
            block: *const block2::Block<dyn Fn(u32, u32, *const u8, isize) -> i32>,
        );
        fn IOHIDUserDeviceSetDispatchQueue(device: *mut c_void, queue: *mut c_void);
        fn IOHIDUserDeviceActivate(device: *mut c_void);
        fn IOHIDUserDeviceCancel(device: *mut c_void);
        fn IOHIDUserDeviceHandleReportWithTimeStamp(
            device: *mut c_void,
            timestamp: u64,
            report: *const u8,
            length: isize,
        ) -> i32;
    }
    unsafe extern "C" {
        fn dispatch_queue_create(
            label: *const std::ffi::c_char,
            attr: *const c_void,
        ) -> *mut c_void;
        fn mach_absolute_time() -> u64;
        fn CFRelease(object: *const c_void);
    }

    struct Device {
        handle: NonNull<c_void>,
        // Kept alive for the device's lifetime; IOKit holds its own copy, but
        // nothing here relies on that.
        _get: GetReportBlock,
        _set: SetReportBlock,
    }

    /// The published devices, cancelled and released on drop.
    pub(super) struct Devices {
        devices: Vec<Device>,
    }

    // SAFETY: an `IOHIDUserDeviceRef` may be used from any thread; reports are
    // handed over from the main loop only, and the callbacks run on each
    // device's own serial queue.
    unsafe impl Send for Devices {}

    impl Devices {
        pub(super) fn create(plan: &[BridgedHidDevice], uplink: &Uplink) -> Result<Self, String> {
            let mut devices = Self {
                devices: Vec::with_capacity(plan.len()),
            };
            for spec in plan {
                let handle = create_one(spec)?;
                let (get, set) = callbacks(spec.index, uplink);
                // SAFETY: the device is live and not yet activated, which is
                // when blocks and a queue may be set. Each device gets its own
                // serial queue, so a waiting report call holds up only its own
                // device.
                unsafe {
                    IOHIDUserDeviceRegisterGetReportBlock(handle.as_ptr(), RcBlock::as_ptr(&get));
                    IOHIDUserDeviceRegisterSetReportBlock(handle.as_ptr(), RcBlock::as_ptr(&set));
                    let queue = dispatch_queue_create(
                        c"tech.arcen.pier.bridged-hid".as_ptr(),
                        std::ptr::null(),
                    );
                    IOHIDUserDeviceSetDispatchQueue(handle.as_ptr(), queue);
                    IOHIDUserDeviceActivate(handle.as_ptr());
                }
                devices.devices.push(Device {
                    handle,
                    _get: get,
                    _set: set,
                });
            }
            Ok(devices)
        }
    }

    impl super::DeviceSet for Devices {
        fn report(&self, index: u8, report: &[u8]) -> Result<(), String> {
            let device = self
                .devices
                .get(usize::from(index))
                .ok_or_else(|| format!("no bridged device {index}"))?;
            // SAFETY: the device is live and activated; `report` is valid for
            // its length during the call.
            let status = unsafe {
                IOHIDUserDeviceHandleReportWithTimeStamp(
                    device.handle.as_ptr(),
                    mach_absolute_time(),
                    report.as_ptr(),
                    isize::try_from(report.len()).unwrap_or(isize::MAX),
                )
            };
            if status == K_IO_RETURN_SUCCESS {
                Ok(())
            } else {
                Err(format!("report refused with IOReturn {status:#x}"))
            }
        }
    }

    impl Drop for Devices {
        fn drop(&mut self) {
            for device in self.devices.drain(..) {
                // SAFETY: each handle was created, activated and is released
                // exactly once here.
                unsafe {
                    IOHIDUserDeviceCancel(device.handle.as_ptr());
                    CFRelease(device.handle.as_ptr());
                }
            }
        }
    }

    fn create_one(spec: &BridgedHidDevice) -> Result<NonNull<c_void>, String> {
        let descriptor = NSData::with_bytes(&spec.report_descriptor);
        let numbers: Vec<(&str, Retained<NSNumber>)> = [
            ("VendorID", u32::from(spec.vendor_id)),
            ("ProductID", u32::from(spec.product_id)),
            ("VersionNumber", u32::from(spec.version)),
            ("CountryCode", u32::from(spec.country_code)),
            ("PrimaryUsagePage", u32::from(spec.primary_usage_page)),
            ("PrimaryUsage", u32::from(spec.primary_usage)),
            ("LocationID", spec.location_id),
            ("RequestTimeout", REQUEST_TIMEOUT_MICROS),
        ]
        .into_iter()
        .map(|(key, value)| (key, NSNumber::new_u32(value)))
        .collect();
        let mut strings: Vec<(&str, Retained<NSString>)> = vec![
            ("Transport", NSString::from_str("USB")),
            ("Product", NSString::from_str(&spec.product)),
            ("SerialNumber", NSString::from_str(&spec.serial)),
        ];
        if !spec.manufacturer.is_empty() {
            strings.push(("Manufacturer", NSString::from_str(&spec.manufacturer)));
        }
        let mut keys: Vec<Retained<NSString>> = vec![NSString::from_str("ReportDescriptor")];
        let mut value_refs: Vec<&AnyObject> = vec![descriptor.as_ref()];
        for (key, value) in &numbers {
            keys.push(NSString::from_str(key));
            value_refs.push(value.as_ref());
        }
        for (key, value) in &strings {
            keys.push(NSString::from_str(key));
            value_refs.push(value.as_ref());
        }
        let key_refs: Vec<&NSString> = keys.iter().map(|key| &**key).collect();
        let properties = NSDictionary::from_slices(&key_refs, &value_refs);
        // SAFETY: `NSDictionary` is toll-free bridged to `CFDictionary` and
        // outlives the call; a null allocator selects the default.
        let device = unsafe {
            IOHIDUserDeviceCreateWithProperties(
                std::ptr::null(),
                Retained::as_ptr(&properties).cast::<CFDictionary>(),
                0,
            )
        };
        NonNull::new(device).ok_or_else(|| {
            crate::virtual_hid::VirtualHidSupport::Refused
                .refusal()
                .unwrap_or("virtual HID refused")
                .to_owned()
        })
    }

    /// `IOHIDReportType` (input 0, output 1, feature 2) as the HID class
    /// request's report type byte (1, 2, 3).
    fn request_kind(report_type: u32) -> Option<u8> {
        (report_type <= 2).then(|| u8::try_from(report_type + 1).unwrap_or(0))
    }

    fn callbacks(index: u8, uplink: &Uplink) -> (GetReportBlock, SetReportBlock) {
        let get_uplink = uplink.clone();
        let get = RcBlock::new(
            move |report_type: u32, report_id: u32, report: *mut u8, length: *mut isize| -> i32 {
                if report.is_null() || length.is_null() {
                    return K_IO_RETURN_BAD_ARGUMENT;
                }
                // SAFETY: IOKit passes a valid length pointer for the call.
                let capacity = unsafe { *length };
                let (Some(kind), Ok(report_id), Ok(capacity)) = (
                    request_kind(report_type),
                    u8::try_from(report_id),
                    u16::try_from(capacity.clamp(0, 0xFFFF)),
                ) else {
                    return K_IO_RETURN_BAD_ARGUMENT;
                };
                let Some(data) = get_uplink.call(ReportCall::Get {
                    index,
                    kind,
                    report_id,
                    length: capacity,
                }) else {
                    return K_IO_RETURN_ERROR;
                };
                let copied = data.len().min(usize::from(capacity));
                // SAFETY: `report` holds `capacity` bytes for the call, and
                // `copied` never exceeds it.
                unsafe {
                    std::ptr::copy_nonoverlapping(data.as_ptr(), report, copied);
                    *length = isize::try_from(copied).unwrap_or(0);
                }
                K_IO_RETURN_SUCCESS
            },
        );
        let set_uplink = uplink.clone();
        let set = RcBlock::new(
            move |report_type: u32, report_id: u32, report: *const u8, length: isize| -> i32 {
                let (Some(kind), Ok(report_id), Ok(length)) = (
                    request_kind(report_type),
                    u8::try_from(report_id),
                    usize::try_from(length),
                ) else {
                    return K_IO_RETURN_BAD_ARGUMENT;
                };
                if length > arcen_usb_bridge::MAX_TRANSFER_BYTES || (report.is_null() && length > 0)
                {
                    return K_IO_RETURN_BAD_ARGUMENT;
                }
                let data = if length == 0 {
                    Vec::new()
                } else {
                    // SAFETY: IOKit passes `length` readable bytes at `report`
                    // for the call.
                    unsafe { std::slice::from_raw_parts(report, length) }.to_vec()
                };
                if set_uplink
                    .call(ReportCall::Set {
                        index,
                        kind,
                        report_id,
                        data,
                    })
                    .is_some()
                {
                    K_IO_RETURN_SUCCESS
                } else {
                    K_IO_RETURN_ERROR
                }
            },
        );
        (get, set)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use arcen_usb_bridge::EndpointAddress;
    use std::sync::mpsc::channel;

    fn device(index: u8) -> BridgedHidDevice {
        BridgedHidDevice {
            index,
            interface: index,
            report_descriptor: arcen_usb_bridge::INTUOS5_PEN_REPORT_DESCRIPTOR.to_vec(),
            vendor_id: 0x056a,
            product_id: 0x0317,
            version: 0x0100,
            country_code: 0,
            primary_usage_page: 0xff0d,
            primary_usage: 1,
            location_id: 0xA2C0_0100,
            manufacturer: String::new(),
            product: "Intuos5 touch L".to_owned(),
            serial: "arcen-1-0".to_owned(),
            input_endpoint: EndpointAddress(0x81),
            input_length: 16,
        }
    }

    fn frames(frames: &[HidDeviceFrame]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for frame in frames {
            frame.write_to(&mut bytes).unwrap();
        }
        bytes
    }

    #[derive(Default)]
    struct Recorded(Mutex<Vec<(u8, Vec<u8>)>>);

    impl DeviceSet for Recorded {
        fn report(&self, index: u8, report: &[u8]) -> Result<(), String> {
            self.0.lock().unwrap().push((index, report.to_vec()));
            Ok(())
        }
    }

    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);

    impl Write for Sink {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn the_plan_must_arrive_in_order_and_be_activated() {
        let plan = frames(&[
            HidDeviceFrame::Create(device(0)),
            HidDeviceFrame::Create(device(1)),
            HidDeviceFrame::Activate,
        ]);
        assert_eq!(read_plan(&mut plan.as_slice()).unwrap().len(), 2);
        for broken in [
            frames(&[HidDeviceFrame::Create(device(1)), HidDeviceFrame::Activate]),
            frames(&[HidDeviceFrame::Activate]),
            frames(&[
                HidDeviceFrame::Create(device(0)),
                HidDeviceFrame::Report {
                    index: 0,
                    report: vec![1],
                },
            ]),
            frames(&[HidDeviceFrame::Create(device(0))]),
        ] {
            assert!(read_plan(&mut broken.as_slice()).is_err());
        }
    }

    #[test]
    fn reports_reach_their_device_and_a_stray_frame_ends_the_child() {
        let devices = Recorded::default();
        let uplink = Uplink::new(Box::new(Sink::default()));
        let input = frames(&[
            HidDeviceFrame::Report {
                index: 1,
                report: vec![2, 3],
            },
            HidDeviceFrame::Report {
                index: 0,
                report: vec![4],
            },
        ]);
        serve(&mut input.as_slice(), &devices, 2, &uplink).unwrap();
        assert_eq!(
            *devices.0.lock().unwrap(),
            vec![(1, vec![2, 3]), (0, vec![4])]
        );
        let unknown = frames(&[HidDeviceFrame::Report {
            index: 2,
            report: vec![1],
        }]);
        assert!(serve(&mut unknown.as_slice(), &devices, 2, &uplink).is_err());
        let stray = frames(&[HidDeviceFrame::Activate]);
        assert!(serve(&mut stray.as_slice(), &devices, 2, &uplink).is_err());
    }

    #[test]
    fn a_report_call_waits_for_its_answer_and_gives_up_on_silence() {
        let sink = Sink::default();
        let uplink = Uplink::new(Box::new(sink.clone()));
        let (done, result) = channel();
        let caller = uplink.clone();
        std::thread::spawn(move || {
            let _ = done.send(caller.call(ReportCall::Set {
                index: 0,
                kind: 3,
                report_id: 2,
                data: vec![2, 2],
            }));
        });
        // The call is on the wire; answer it as the session would.
        let token = loop {
            let bytes = sink.0.lock().unwrap().clone();
            if let Ok(Some(HidDeviceFrame::SetReport { token, data, .. })) =
                HidDeviceFrame::read_from(&mut bytes.as_slice())
            {
                assert_eq!(data, vec![2, 2]);
                break token;
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        uplink.answer(token, true, Vec::new());
        assert_eq!(
            result.recv_timeout(Duration::from_secs(2)).unwrap(),
            Some(Vec::new())
        );
        // A refusal fails the call; an answer nobody waits for is dropped.
        let (done, result) = channel();
        let caller = uplink.clone();
        std::thread::spawn(move || {
            let _ = done.send(caller.call(ReportCall::Get {
                index: 0,
                kind: 3,
                report_id: 2,
                length: 2,
            }));
        });
        std::thread::sleep(Duration::from_millis(50));
        uplink.answer(token + 1, false, Vec::new());
        assert_eq!(result.recv_timeout(Duration::from_secs(2)).unwrap(), None);
        uplink.answer(9_999, true, vec![1]);
    }
}
