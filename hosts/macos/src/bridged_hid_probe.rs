// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! `probe-bridged-hid` (lab builds only): what the HID system sees of a
//! bridged tablet (ADR 0022 §10).
//!
//! Lists every HID device whose `LocationID` carries Arcen's bridged prefix
//! ([`arcen_usb_bridge::is_bridged_location_id`]) with the properties a vendor
//! driver matches on. With `--mode-switch` it opens the vendor pen interface
//! (usage page `0xFF0D`) the way Wacom's driver does and sets feature report
//! 2 to `[2, 2]`, then reads it back — which crosses the whole set/get report
//! path to the Deck's device. With `--count SECONDS` it counts the input
//! reports each device delivers and their intervals.
//!
//! It reads; it injects nothing.

#![allow(unsafe_code)]

use std::ffi::c_void;
use std::time::{Duration, Instant};

use objc2_foundation::{NSData, NSString};

type CFTypeRef = *const c_void;

const K_CF_NUMBER_SINT64: i32 = 4;
const K_IO_HID_REPORT_TYPE_FEATURE: u32 = 2;
const K_IO_HID_OPTIONS_NONE: u32 = 0;

// SAFETY: signatures match `IOKit/hid/IOHIDManager.h`, `IOHIDDevice.h` and
// `CoreFoundation` (`CFSet.h`, `CFNumber.h`, `CFRunLoop.h`, `CFBase.h`).
#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOHIDManagerCreate(allocator: *const c_void, options: u32) -> *mut c_void;
    fn IOHIDManagerSetDeviceMatching(manager: *mut c_void, matching: *const c_void);
    fn IOHIDManagerCopyDevices(manager: *mut c_void) -> *const c_void;
    fn IOHIDDeviceGetProperty(device: *mut c_void, key: *const c_void) -> CFTypeRef;
    fn IOHIDDeviceOpen(device: *mut c_void, options: u32) -> i32;
    fn IOHIDDeviceClose(device: *mut c_void, options: u32) -> i32;
    fn IOHIDDeviceSetReport(
        device: *mut c_void,
        report_type: u32,
        report_id: isize,
        report: *const u8,
        length: isize,
    ) -> i32;
    fn IOHIDDeviceGetReport(
        device: *mut c_void,
        report_type: u32,
        report_id: isize,
        report: *mut u8,
        length: *mut isize,
    ) -> i32;
    fn IOHIDDeviceRegisterInputReportCallback(
        device: *mut c_void,
        report: *mut u8,
        length: isize,
        callback: extern "C" fn(*mut c_void, i32, *mut c_void, u32, u32, *mut u8, isize),
        context: *mut c_void,
    );
    fn IOHIDDeviceScheduleWithRunLoop(device: *mut c_void, run_loop: *mut c_void, mode: CFTypeRef);
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFRunLoopDefaultMode: CFTypeRef;
    fn CFSetGetCount(set: *const c_void) -> isize;
    fn CFSetGetValues(set: *const c_void, values: *mut *const c_void);
    fn CFGetTypeID(object: CFTypeRef) -> usize;
    fn CFNumberGetTypeID() -> usize;
    fn CFStringGetTypeID() -> usize;
    fn CFDataGetTypeID() -> usize;
    fn CFNumberGetValue(number: CFTypeRef, kind: i32, value: *mut c_void) -> u8;
    fn CFRunLoopGetCurrent() -> *mut c_void;
    fn CFRunLoopRunInMode(mode: CFTypeRef, seconds: f64, return_after_source: u8) -> i32;
    fn CFRelease(object: CFTypeRef);
}

/// Input reports one device delivered.
struct Counter {
    times: Vec<Instant>,
    last: Vec<u8>,
    buffer: Vec<u8>,
}

extern "C" fn on_report(
    context: *mut c_void,
    _result: i32,
    _sender: *mut c_void,
    _report_type: u32,
    _report_id: u32,
    report: *mut u8,
    length: isize,
) {
    // SAFETY: `context` is the `Counter` registered with this callback, which
    // outlives the run loop that calls it; `report` holds `length` bytes.
    unsafe {
        let counter = &mut *context.cast::<Counter>();
        counter.times.push(Instant::now());
        let length = usize::try_from(length).unwrap_or(0);
        counter.last = std::slice::from_raw_parts(report, length).to_vec();
    }
}

fn property(device: *mut c_void, key: &str) -> CFTypeRef {
    let key = NSString::from_str(key);
    // SAFETY: `device` is a live device from the manager's set; the key is a
    // toll-free bridged string alive for the call. The result is not owned.
    unsafe { IOHIDDeviceGetProperty(device, objc2::rc::Retained::as_ptr(&key).cast()) }
}

fn number(device: *mut c_void, key: &str) -> Option<i64> {
    let value = property(device, key);
    let mut out = 0_i64;
    // SAFETY: the type is checked before the number is read into an i64.
    let ok = unsafe {
        !value.is_null()
            && CFGetTypeID(value) == CFNumberGetTypeID()
            && CFNumberGetValue(value, K_CF_NUMBER_SINT64, (&raw mut out).cast()) != 0
    };
    ok.then_some(out)
}

fn text(device: *mut c_void, key: &str) -> Option<String> {
    let value = property(device, key);
    // SAFETY: a `CFString` is toll-free bridged to `NSString`; the type is
    // checked and the reference is only borrowed for the copy.
    unsafe {
        (!value.is_null() && CFGetTypeID(value) == CFStringGetTypeID())
            .then(|| (*value.cast::<NSString>()).to_string())
    }
}

fn data_length(device: *mut c_void, key: &str) -> Option<usize> {
    let value = property(device, key);
    // SAFETY: as for `text`, with `CFData`/`NSData`.
    unsafe {
        (!value.is_null() && CFGetTypeID(value) == CFDataGetTypeID())
            .then(|| (*value.cast::<NSData>()).length())
    }
}

/// Runs the probe. Exit status 0 when at least one bridged device was found
/// (and, when asked, the mode switch round-tripped and reports arrived).
#[must_use]
#[allow(
    clippy::too_many_lines,
    clippy::cast_precision_loss,
    reason = "one linear lab report; counts and spans are far below 2^52"
)]
pub fn run(arguments: &[String]) -> std::process::ExitCode {
    let mode_switch = arguments.iter().any(|argument| argument == "--mode-switch");
    let count = arguments
        .iter()
        .position(|argument| argument == "--count")
        .and_then(|index| arguments.get(index + 1))
        .and_then(|value| value.parse::<f64>().ok())
        .unwrap_or(0.0);
    // SAFETY: a null allocator selects the default; a null matching
    // dictionary matches every HID device. The set is released below.
    let (manager, set) = unsafe {
        let manager = IOHIDManagerCreate(std::ptr::null(), 0);
        IOHIDManagerSetDeviceMatching(manager, std::ptr::null());
        (manager, IOHIDManagerCopyDevices(manager))
    };
    if set.is_null() {
        println!("bridged_hid devices=0 (no HID devices visible)");
        return std::process::ExitCode::FAILURE;
    }
    // SAFETY: `set` is a live CFSet of IOHIDDeviceRefs; the buffer has room
    // for every value.
    let devices: Vec<*mut c_void> = unsafe {
        let count = usize::try_from(CFSetGetCount(set)).unwrap_or(0);
        let mut values = vec![std::ptr::null(); count];
        CFSetGetValues(set, values.as_mut_ptr());
        values.into_iter().map(<*const c_void>::cast_mut).collect()
    };
    let mut bridged: Vec<(*mut c_void, i64, i64)> = devices
        .into_iter()
        .filter_map(|device| {
            let location = number(device, "LocationID")?;
            arcen_usb_bridge::is_bridged_location_id(u32::try_from(location).ok()?).then(|| {
                (
                    device,
                    location,
                    number(device, "PrimaryUsagePage").unwrap_or(0),
                )
            })
        })
        .collect();
    bridged.sort_by_key(|(_, location, _)| *location);
    println!("bridged_hid devices={}", bridged.len());
    for (device, location, _) in &bridged {
        println!(
            "device location={location:#010x} vendor={:#06x} product={:#06x} version={:#06x} \
             transport={:?} manufacturer={:?} product_name={:?} serial={:?} \
             usage_page={:#06x} usage={:#04x} report_descriptor_bytes={} max_input_report={}",
            number(*device, "VendorID").unwrap_or(-1),
            number(*device, "ProductID").unwrap_or(-1),
            number(*device, "VersionNumber").unwrap_or(-1),
            text(*device, "Transport").unwrap_or_default(),
            text(*device, "Manufacturer").unwrap_or_default(),
            text(*device, "Product").unwrap_or_default(),
            text(*device, "SerialNumber").unwrap_or_default(),
            number(*device, "PrimaryUsagePage").unwrap_or(-1),
            number(*device, "PrimaryUsage").unwrap_or(-1),
            data_length(*device, "ReportDescriptor").unwrap_or(0),
            number(*device, "MaxInputReportSize").unwrap_or(-1),
        );
    }
    let mut ok = !bridged.is_empty();
    let mut counters: Vec<Box<Counter>> = Vec::new();
    let mut opened = Vec::new();
    for (device, location, usage_page) in &bridged {
        // SAFETY: a live device; opened without seizing it.
        let status = unsafe { IOHIDDeviceOpen(*device, K_IO_HID_OPTIONS_NONE) };
        println!("open location={location:#010x} status={status:#x}");
        if status != 0 {
            continue;
        }
        opened.push(*device);
        if mode_switch && *usage_page == 0xff0d {
            let request = [2_u8, 2];
            // SAFETY: a live, opened device; `request` is valid for its length.
            let set = unsafe {
                IOHIDDeviceSetReport(
                    *device,
                    K_IO_HID_REPORT_TYPE_FEATURE,
                    2,
                    request.as_ptr(),
                    2,
                )
            };
            let mut answer = [0_u8; 2];
            let mut length: isize = 2;
            let started = Instant::now();
            // SAFETY: as above; `answer` holds `length` bytes.
            let get = unsafe {
                IOHIDDeviceGetReport(
                    *device,
                    K_IO_HID_REPORT_TYPE_FEATURE,
                    2,
                    answer.as_mut_ptr(),
                    &raw mut length,
                )
            };
            println!(
                "mode_switch set_report={set:#x} get_report={get:#x} answer={:?} round_trip_ms={:.1}",
                &answer[..usize::try_from(length).unwrap_or(0).min(2)],
                started.elapsed().as_secs_f64() * 1000.0
            );
            ok &= set == 0 && get == 0 && answer == request;
        }
        if count > 0.0 {
            let size = usize::try_from(number(*device, "MaxInputReportSize").unwrap_or(64))
                .unwrap_or(64)
                .clamp(1, 16 * 1024);
            let mut counter = Box::new(Counter {
                times: Vec::new(),
                last: Vec::new(),
                buffer: vec![0; size],
            });
            // SAFETY: the buffer and context live in `counters` until the
            // devices are closed below; the run loop is this thread's.
            unsafe {
                IOHIDDeviceRegisterInputReportCallback(
                    *device,
                    counter.buffer.as_mut_ptr(),
                    isize::try_from(size).unwrap_or(64),
                    on_report,
                    (&raw mut *counter).cast(),
                );
                IOHIDDeviceScheduleWithRunLoop(
                    *device,
                    CFRunLoopGetCurrent(),
                    kCFRunLoopDefaultMode,
                );
            }
            counters.push(counter);
        }
    }
    if count > 0.0 {
        let until = Instant::now() + Duration::from_secs_f64(count);
        while Instant::now() < until {
            // SAFETY: runs this thread's run loop, which delivers the
            // callbacks registered above.
            unsafe { CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.25, 0) };
        }
        let mut any = false;
        for counter in &counters {
            let intervals: Vec<f64> = counter
                .times
                .windows(2)
                .map(|pair| pair[1].duration_since(pair[0]).as_secs_f64() * 1000.0)
                .collect();
            let mean = if intervals.is_empty() {
                0.0
            } else {
                intervals.iter().sum::<f64>() / intervals.len() as f64
            };
            let max = intervals.iter().copied().fold(0.0_f64, f64::max);
            let span = counter
                .times
                .first()
                .zip(counter.times.last())
                .map_or(0.0, |(first, last)| {
                    last.duration_since(*first).as_secs_f64()
                });
            println!(
                "reports count={} span_s={span:.2} rate_hz={:.1} mean_interval_ms={mean:.2} \
                 max_interval_ms={max:.2} last={:02x?}",
                counter.times.len(),
                if span > 0.0 {
                    (counter.times.len() - 1) as f64 / span
                } else {
                    0.0
                },
                counter.last
            );
            any |= !counter.times.is_empty();
        }
        ok &= any;
    }
    for device in opened {
        // SAFETY: each was opened above and is closed once.
        unsafe { IOHIDDeviceClose(device, K_IO_HID_OPTIONS_NONE) };
    }
    drop(counters);
    // SAFETY: both were created/copied above and are released once.
    unsafe {
        CFRelease(set);
        CFRelease(manager);
    }
    if ok {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::FAILURE
    }
}
