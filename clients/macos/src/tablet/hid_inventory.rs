//! Read-only HID inventory from the macOS I/O Registry.
//!
//! Enumerates every `IOHIDDevice` service and copies three published
//! properties: `VendorID`, `ProductID` and `ReportDescriptor`. Nothing is
//! opened, seized or listened to, so this needs no Input Monitoring grant and
//! cannot disturb a device's driver. Classification happens elsewhere, in
//! shared code; this module only gathers what the operating system already
//! publishes.

use super::presence::HidDeviceFacts;

/// Upper bound on registry entries examined, so a pathological registry
/// cannot make a presence probe unbounded.
const MAX_HID_SERVICES: usize = 256;

/// Reads the HID inventory. `None` when the registry could not be queried.
#[cfg(target_os = "macos")]
#[must_use]
pub fn hid_devices() -> Option<Vec<HidDeviceFacts>> {
    ffi::hid_devices()
}

/// Non-macOS builds have no registry to read.
#[cfg(not(target_os = "macos"))]
#[must_use]
pub fn hid_devices() -> Option<Vec<HidDeviceFacts>> {
    None
}

#[cfg(target_os = "macos")]
#[allow(non_camel_case_types, unsafe_code)]
mod ffi {
    use super::{HidDeviceFacts, MAX_HID_SERVICES};
    use std::ffi::{c_char, c_void};

    type CFTypeRef = *const c_void;
    type CFStringRef = *const c_void;
    type CFAllocatorRef = *const c_void;
    type CFMutableDictionaryRef = *mut c_void;
    type CFIndex = isize;
    type CFTypeID = usize;
    type io_object_t = u32;
    type io_iterator_t = u32;
    type kern_return_t = i32;
    type mach_port_t = u32;

    const KERN_SUCCESS: kern_return_t = 0;
    /// `kIOMainPortDefault` (formerly `kIOMasterPortDefault`) is `MACH_PORT_NULL`.
    const IO_MAIN_PORT_DEFAULT: mach_port_t = 0;
    const CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
    const CF_NUMBER_SINT64_TYPE: CFIndex = 4;

    #[link(name = "IOKit", kind = "framework")]
    extern "C" {
        fn IOServiceMatching(name: *const c_char) -> CFMutableDictionaryRef;
        fn IOServiceGetMatchingServices(
            main_port: mach_port_t,
            matching: CFMutableDictionaryRef,
            existing: *mut io_iterator_t,
        ) -> kern_return_t;
        fn IOIteratorNext(iterator: io_iterator_t) -> io_object_t;
        fn IOObjectRelease(object: io_object_t) -> kern_return_t;
        fn IORegistryEntryCreateCFProperty(
            entry: io_object_t,
            key: CFStringRef,
            allocator: CFAllocatorRef,
            options: u32,
        ) -> CFTypeRef;
    }

    // The experimental raw-HID module declares some of these with looser
    // pointer types; these follow the CoreFoundation headers (`CFNumberType`
    // is a `CFIndex`).
    #[allow(clashing_extern_declarations)]
    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFStringCreateWithCString(
            allocator: CFAllocatorRef,
            string: *const c_char,
            encoding: u32,
        ) -> CFStringRef;
        fn CFRelease(object: CFTypeRef);
        fn CFGetTypeID(object: CFTypeRef) -> CFTypeID;
        fn CFDataGetTypeID() -> CFTypeID;
        fn CFNumberGetTypeID() -> CFTypeID;
        fn CFDataGetLength(data: CFTypeRef) -> CFIndex;
        fn CFDataGetBytePtr(data: CFTypeRef) -> *const u8;
        fn CFNumberGetValue(number: CFTypeRef, number_type: CFIndex, value: *mut c_void) -> bool;
    }

    /// Owned CF object, released on drop.
    struct Owned(CFTypeRef);

    impl Owned {
        fn new(object: CFTypeRef) -> Option<Self> {
            // Never construct `Self(null)`, even transiently: dropping it
            // calls `CFRelease(NULL)`, which traps. (`bool::then_some` would
            // build and drop one eagerly.)
            if object.is_null() {
                None
            } else {
                Some(Self(object))
            }
        }
    }

    impl Drop for Owned {
        fn drop(&mut self) {
            // SAFETY: `self.0` is a non-null CF object this wrapper owns one
            // reference to (a Create-rule return value), released exactly once.
            unsafe { CFRelease(self.0) }
        }
    }

    /// Owned I/O Kit object, released on drop.
    struct IoObject(io_object_t);

    impl Drop for IoObject {
        fn drop(&mut self) {
            if self.0 != 0 {
                // SAFETY: a nonzero object returned by `IOIteratorNext` or
                // `IOServiceGetMatchingServices` that this wrapper owns.
                unsafe {
                    IOObjectRelease(self.0);
                }
            }
        }
    }

    fn key(name: &std::ffi::CStr) -> Option<Owned> {
        // SAFETY: `name` is a valid NUL-terminated C string; a null return is
        // handled by `Owned::new`.
        Owned::new(unsafe {
            CFStringCreateWithCString(std::ptr::null(), name.as_ptr(), CF_STRING_ENCODING_UTF8)
        })
    }

    fn property(entry: io_object_t, key: &Owned) -> Option<Owned> {
        // SAFETY: `entry` is a live registry entry for the duration of this
        // call and `key` a live CFString; the returned object follows the
        // Create rule and is owned by `Owned`.
        Owned::new(unsafe { IORegistryEntryCreateCFProperty(entry, key.0, std::ptr::null(), 0) })
    }

    fn number(entry: io_object_t, key: &Owned) -> Option<u16> {
        let value = property(entry, key)?;
        // SAFETY: `value.0` is a live CF object.
        if unsafe { CFGetTypeID(value.0) != CFNumberGetTypeID() } {
            return None;
        }
        let mut out: i64 = 0;
        // SAFETY: `value.0` is a CFNumber (checked above) and `out` is a
        // valid, aligned i64 destination for `kCFNumberSInt64Type`.
        let ok = unsafe {
            CFNumberGetValue(
                value.0,
                CF_NUMBER_SINT64_TYPE,
                std::ptr::addr_of_mut!(out).cast::<c_void>(),
            )
        };
        if ok {
            u16::try_from(out).ok()
        } else {
            None
        }
    }

    fn data(entry: io_object_t, key: &Owned) -> Option<Vec<u8>> {
        let value = property(entry, key)?;
        // SAFETY: `value.0` is a live CF object.
        if unsafe { CFGetTypeID(value.0) != CFDataGetTypeID() } {
            return None;
        }
        // SAFETY: `value.0` is a CFData (checked above).
        let length = usize::try_from(unsafe { CFDataGetLength(value.0) }).ok()?;
        if length == 0 || length > arcen_usb_bridge::MAX_HID_REPORT_DESCRIPTOR_BYTES {
            return None;
        }
        // SAFETY: `value.0` is a CFData of `length` bytes that stays alive
        // (owned by `value`) for the duration of the copy.
        let bytes = unsafe { CFDataGetBytePtr(value.0) };
        if bytes.is_null() {
            return None;
        }
        // SAFETY: as above; `bytes` points at `length` readable bytes.
        Some(unsafe { std::slice::from_raw_parts(bytes, length) }.to_vec())
    }

    pub(super) fn hid_devices() -> Option<Vec<HidDeviceFacts>> {
        let vendor_key = key(c"VendorID")?;
        let product_key = key(c"ProductID")?;
        let descriptor_key = key(c"ReportDescriptor")?;
        // SAFETY: a valid NUL-terminated class name. The returned dictionary
        // is consumed by `IOServiceGetMatchingServices` below, which releases
        // it whether or not it succeeds.
        let matching = unsafe { IOServiceMatching(c"IOHIDDevice".as_ptr()) };
        if matching.is_null() {
            return None;
        }
        let mut iterator: io_iterator_t = 0;
        // SAFETY: `matching` is a fresh dictionary whose reference this call
        // consumes; `iterator` is a valid out-pointer.
        let result = unsafe {
            IOServiceGetMatchingServices(
                IO_MAIN_PORT_DEFAULT,
                matching,
                std::ptr::addr_of_mut!(iterator),
            )
        };
        if result != KERN_SUCCESS {
            return None;
        }
        let iterator = IoObject(iterator);
        let mut devices = Vec::new();
        for _ in 0..MAX_HID_SERVICES {
            // SAFETY: `iterator.0` is the live iterator owned above.
            let entry = IoObject(unsafe { IOIteratorNext(iterator.0) });
            if entry.0 == 0 {
                break;
            }
            let Some(report_descriptor) = data(entry.0, &descriptor_key) else {
                continue;
            };
            devices.push(HidDeviceFacts {
                vendor_id: number(entry.0, &vendor_key).unwrap_or(0),
                product_id: number(entry.0, &product_key).unwrap_or(0),
                report_descriptor,
            });
        }
        Some(devices)
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    #[test]
    fn reading_the_registry_returns_bounded_descriptors() {
        let devices = super::hid_devices().expect("the I/O Registry is readable");
        assert!(devices.len() <= super::MAX_HID_SERVICES);
        for device in devices {
            assert!(!device.report_descriptor.is_empty());
            assert!(
                device.report_descriptor.len() <= arcen_usb_bridge::MAX_HID_REPORT_DESCRIPTOR_BYTES
            );
        }
    }
}
