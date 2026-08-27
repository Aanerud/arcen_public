//! Raw FFI bindings for the IOHIDManager C API (IOKit.framework).
//!
//! IOHIDManager is a Core Foundation-based C API — no Objective-C needed.
//! We declare the minimal surface required to enumerate tablet devices and
//! capture their raw HID reports without installing a kext or driver.
//!
//! All opaque IOKit objects are CF-retained handles; callers must release them
//! via `CFRelease` when done.  The HidSession wrapper handles all lifetimes.

#![allow(
    non_camel_case_types,
    non_upper_case_globals,
    non_snake_case,
    dead_code
)]

use std::ffi::c_void;

// ── Opaque CF / IOKit types ──────────────────────────────────────────────────

pub type CFAllocatorRef = *mut c_void;
pub type CFDictionaryRef = *mut c_void;
pub type CFRunLoopRef = *mut c_void;
pub type CFStringRef = *const c_void;
pub type CFTypeRef = *mut c_void;
pub type IOOptionBits = u32;
pub type IOReturn = i32;

pub type IOHIDManagerRef = *mut c_void;
pub type IOHIDDeviceRef = *mut c_void;

// ── IOReturn constants ────────────────────────────────────────────────────────

pub const kIOReturnSuccess: IOReturn = 0;

// ── IOHIDManager option bits ──────────────────────────────────────────────────

pub const kIOHIDOptionsTypeNone: IOOptionBits = 0x00;

// ── IOHIDManager run loop mode ────────────────────────────────────────────────

pub const kCFRunLoopDefaultMode: &[u8] = b"kCFRunLoopDefaultMode\0";

// ── IOHIDDevice property keys (CFStringRef, obtained via CFStringCreateWithCString) ──

// Rather than linking kIOHIDVendorIDKey etc. as extern statics (requires IOHID
// symbols not available in all SDK configs), we use the string values directly.
pub const PROP_VENDOR_ID: &[u8] = b"VendorID\0";
pub const PROP_PRODUCT_ID: &[u8] = b"ProductID\0";
pub const PROP_REPORT_DESCRIPTOR: &[u8] = b"ReportDescriptor\0";
pub const PROP_TRANSPORT: &[u8] = b"Transport\0";
pub const PROP_PRIMARY_USAGE_PAGE: &[u8] = b"PrimaryUsagePage\0";
/// Matching-dictionary key for a device's usage page. Distinct from
/// `PROP_PRIMARY_USAGE_PAGE`, which reads the property back off a device.
pub const MATCH_DEVICE_USAGE_PAGE: &[u8] = b"DeviceUsagePage\0";
/// Matching-dictionary key for a device's USB vendor id.
pub const MATCH_VENDOR_ID: &[u8] = b"VendorID\0";
pub const PROP_MAX_INPUT_REPORT_SIZE: &[u8] = b"MaxInputReportSize\0";

// ── TCC (Input Monitoring) access types (IOHIDLib.h) ─────────────────────────
//
// SEC-raw-hid: these let us distinguish the three possible privacy-access
// states truthfully (Granted / Denied / Unknown) instead of only inferring
// "granted" indirectly from a successful device open, which cannot tell
// apart "denied" from "no matching hardware".

pub type IOHIDRequestType = u32;
pub const kIOHIDRequestTypeListenEvent: IOHIDRequestType = 0;

pub type IOHIDAccessType = isize;
pub const kIOHIDAccessTypeGranted: IOHIDAccessType = 0;
pub const kIOHIDAccessTypeDenied: IOHIDAccessType = 1;
pub const kIOHIDAccessTypeUnknown: IOHIDAccessType = 2;

// ── Callback types ────────────────────────────────────────────────────────────

pub type IOHIDDeviceCallback = unsafe extern "C" fn(
    context: *mut c_void,
    result: IOReturn,
    sender: *mut c_void,
    device: IOHIDDeviceRef,
);

pub type IOHIDReportCallback = unsafe extern "C" fn(
    context: *mut c_void,
    result: IOReturn,
    sender: *mut c_void,
    report_type: u32,
    report_id: u32,
    report: *const u8,
    report_len: isize,
);

// ── CoreFoundation symbols ───────────────────────────────────────────────────

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    pub fn CFRunLoopGetCurrent() -> CFRunLoopRef;
    pub fn CFRunLoopRun();
    pub fn CFRunLoopStop(rl: CFRunLoopRef);
    pub fn CFRelease(cf: CFTypeRef);
    pub fn CFRetain(cf: CFTypeRef) -> CFTypeRef;

    pub fn CFStringCreateWithCString(
        alloc: CFAllocatorRef,
        c_str: *const u8,
        encoding: u32,
    ) -> CFStringRef;

    pub fn CFNumberGetValue(number: CFTypeRef, the_type: i32, value_ptr: *mut c_void) -> bool;

    pub fn CFDataGetLength(data: CFTypeRef) -> isize;
    pub fn CFDataGetBytePtr(data: CFTypeRef) -> *const u8;

    /// Builds the device-matching dictionary handed to
    /// `IOHIDManagerSetDeviceMatching`. See its use in `session.rs`: the
    /// manager must be told to match *only* digitizers, because
    /// `IOHIDManagerOpen` has to succeed for every matched device and the
    /// system refuses a manager that matches everything.
    pub fn CFDictionaryCreateMutable(
        alloc: CFAllocatorRef,
        capacity: isize,
        key_callbacks: *const c_void,
        value_callbacks: *const c_void,
    ) -> CFTypeRef;
    pub fn CFDictionarySetValue(dict: CFTypeRef, key: *const c_void, value: *const c_void);
    pub fn CFNumberCreate(
        alloc: CFAllocatorRef,
        the_type: i32,
        value_ptr: *const c_void,
    ) -> CFTypeRef;

    pub static kCFTypeDictionaryKeyCallBacks: c_void;
    pub static kCFTypeDictionaryValueCallBacks: c_void;

    pub fn CFArrayCreateMutable(
        alloc: CFAllocatorRef,
        capacity: isize,
        callbacks: *const c_void,
    ) -> CFTypeRef;
    pub fn CFArrayAppendValue(array: CFTypeRef, value: *const c_void);
    pub static kCFTypeArrayCallBacks: c_void;
}

/// `kIOReturnExclusiveAccess`. The device is already open exclusively by
/// another owner — on macOS a Wacom tablet is held by Wacom's own
/// `TabletDriver`, so a HID *listener* can never open it, with or without
/// `kIOHIDOptionsTypeSeizeDevice`. Measured on this hardware. Reporting this
/// as a permission problem sends the reader to grant Input Monitoring they
/// already have.
pub const IO_RETURN_EXCLUSIVE_ACCESS: i32 = 0xe000_02c5_u32 as i32;

/// `kIOReturnNotPermitted`, the genuine "not allowed" answer.
pub const IO_RETURN_NOT_PERMITTED: i32 = 0xe000_02e2_u32 as i32;

/// `kCFNumberSInt32Type`, the `CFNumberCreate` type tag for the 32-bit
/// integers IOKit matching dictionaries use.
pub const CF_NUMBER_SINT32_TYPE: i32 = 3;

// ── IOKit symbols ─────────────────────────────────────────────────────────────

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    pub fn IOHIDManagerCreate(allocator: CFAllocatorRef, options: IOOptionBits) -> IOHIDManagerRef;

    pub fn IOHIDManagerOpen(manager: IOHIDManagerRef, options: IOOptionBits) -> IOReturn;

    pub fn IOHIDManagerClose(manager: IOHIDManagerRef, options: IOOptionBits) -> IOReturn;

    pub fn IOHIDManagerSetDeviceMatching(manager: IOHIDManagerRef, matching: CFDictionaryRef);
    /// Matches any of an array of dictionaries, which is how one manager
    /// can cover several tablet vendors at once.
    pub fn IOHIDManagerSetDeviceMatchingMultiple(manager: IOHIDManagerRef, matching: CFTypeRef);

    pub fn IOHIDManagerRegisterDeviceMatchingCallback(
        manager: IOHIDManagerRef,
        callback: IOHIDDeviceCallback,
        context: *mut c_void,
    );

    pub fn IOHIDManagerRegisterDeviceRemovalCallback(
        manager: IOHIDManagerRef,
        callback: IOHIDDeviceCallback,
        context: *mut c_void,
    );

    pub fn IOHIDManagerScheduleWithRunLoop(
        manager: IOHIDManagerRef,
        run_loop: CFRunLoopRef,
        mode: CFStringRef,
    );

    pub fn IOHIDDeviceOpen(device: IOHIDDeviceRef, options: IOOptionBits) -> IOReturn;

    pub fn IOHIDDeviceClose(device: IOHIDDeviceRef, options: IOOptionBits) -> IOReturn;

    pub fn IOHIDDeviceGetProperty(device: IOHIDDeviceRef, key: CFStringRef) -> CFTypeRef;

    pub fn IOHIDDeviceRegisterInputReportCallback(
        device: IOHIDDeviceRef,
        report: *mut u8,
        report_len: isize,
        callback: IOHIDReportCallback,
        context: *mut c_void,
    );

    /// Synchronously returns the current TCC (Input Monitoring) access state
    /// for this process, without prompting the user.
    pub fn IOHIDCheckAccess(request_type: IOHIDRequestType) -> IOHIDAccessType;

    /// Asks the OS to prompt the user for Input Monitoring access if the
    /// current state is not yet determined. Does not block on the user's
    /// answer; the result must be observed indirectly (e.g. via a
    /// subsequent `IOHIDCheckAccess` call or device-open behaviour).
    pub fn IOHIDRequestAccess(request_type: IOHIDRequestType) -> bool;
}

// ── Inline helpers ────────────────────────────────────────────────────────────

pub const kCFStringEncodingUTF8: u32 = 0x0800_0100;
pub const kCFNumberSInt32Type: i32 = 3;

/// Build a CFStringRef from a NUL-terminated byte string literal (e.g. `b"VendorID\0"`).
///
/// # Safety
/// `bytes` must be a NUL-terminated ASCII/UTF-8 string.  The returned
/// CFStringRef must be released by the caller via `CFRelease`.
pub unsafe fn cf_string(bytes: &[u8]) -> CFStringRef {
    CFStringCreateWithCString(std::ptr::null_mut(), bytes.as_ptr(), kCFStringEncodingUTF8)
}

/// Read an i32 from a CFNumber.  Returns `None` if the ref is null or not a number.
///
/// # Safety
/// `cf_number` must be a valid CFNumberRef or null.
pub unsafe fn cf_number_i32(cf_number: CFTypeRef) -> Option<i32> {
    if cf_number.is_null() {
        return None;
    }
    let mut value: i32 = 0;
    if CFNumberGetValue(
        cf_number,
        kCFNumberSInt32Type,
        &raw mut value as *mut c_void,
    ) {
        Some(value)
    } else {
        None
    }
}

/// Copy bytes out of a CFDataRef into a Vec.  Returns `None` if null.
///
/// # Safety
/// `cf_data` must be a valid CFDataRef or null.
pub unsafe fn cf_data_to_vec(cf_data: CFTypeRef) -> Option<Vec<u8>> {
    if cf_data.is_null() {
        return None;
    }
    let len = CFDataGetLength(cf_data) as usize;
    let ptr = CFDataGetBytePtr(cf_data);
    let slice = std::slice::from_raw_parts(ptr, len);
    Some(slice.to_vec())
}
