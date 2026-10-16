//! HID report descriptors from the macOS I/O Registry.
//!
//! macOS publishes every HID interface's report descriptor as the
//! `ReportDescriptor` property of its `IOHIDDevice` service. Reading the
//! registry opens nothing, sends no request to the device and needs no
//! privilege, so the helper can examine every attached device this way without
//! touching keyboards and mice at all. A USB request to the device itself is
//! reserved for an interface the registry left undescribed on a device the
//! registry already shows to be a digitizer (see [`fallback_plan`]).
//!
//! Each `IOHIDDevice` is tied back to its USB device by `LocationID` (the USB
//! location id, which libusb's macOS backend splits into bus number and port
//! path, see [`location_id`]) plus vendor and product id, and to its interface
//! by the `bInterfaceNumber` of its `IOUSBHostInterface` ancestor.

use std::collections::BTreeMap;

/// Upper bound on registry entries examined per scan.
const MAX_HID_SERVICES: usize = 256;

/// One `IOHIDDevice` as the registry describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryHid {
    pub vendor_id: u16,
    pub product_id: u16,
    pub location_id: Option<u32>,
    pub interface: Option<u8>,
    pub report_descriptor: Vec<u8>,
}

/// Reconstructs the macOS USB `locationID` from libusb's bus number and port
/// path: the bus in the top byte, then one nibble per hub port from bit 20
/// down. `None` when a port does not fit a nibble or the path is too deep.
#[must_use]
pub fn location_id(bus: u8, ports: &[u8]) -> Option<u32> {
    if ports.is_empty() || ports.len() > 6 {
        return None;
    }
    let mut location = u32::from(bus) << 24;
    for (depth, &port) in ports.iter().enumerate() {
        if port == 0 || port > 0x0F {
            return None;
        }
        let shift = 20 - 4 * u32::try_from(depth).ok()?;
        location |= u32::from(port) << shift;
    }
    Some(location)
}

/// The registry cannot say which descriptor belongs to the device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistryAmbiguity {
    /// Two registry entries for the same interface carry different
    /// descriptors.
    ConflictingInterface(u8),
}

/// Report descriptors the registry publishes for one USB device, keyed by
/// interface number.
///
/// `location` is `Some` exactly when two attached devices share the vendor
/// and product id; then only entries with a known, equal `LocationID` count,
/// so one tablet's descriptors can never authorize its twin. With `None` the
/// identity is unique and matches on its own. Entries without an interface
/// number are ignored. Two entries for one interface must agree byte for
/// byte; otherwise the device is ambiguous and refused.
///
/// # Errors
///
/// Returns [`RegistryAmbiguity`] for conflicting entries.
pub fn descriptors_for(
    entries: &[RegistryHid],
    vendor_id: u16,
    product_id: u16,
    location: Option<u32>,
) -> Result<BTreeMap<u8, Vec<u8>>, RegistryAmbiguity> {
    let mut found: BTreeMap<u8, Vec<u8>> = BTreeMap::new();
    for entry in entries {
        if entry.vendor_id != vendor_id || entry.product_id != product_id {
            continue;
        }
        if let Some(wanted) = location {
            if entry.location_id != Some(wanted) {
                continue;
            }
        }
        let Some(interface) = entry.interface else {
            continue;
        };
        match found.get(&interface) {
            Some(existing) if existing != &entry.report_descriptor => {
                return Err(RegistryAmbiguity::ConflictingInterface(interface));
            }
            Some(_) => {}
            None => {
                found.insert(interface, entry.report_descriptor.clone());
            }
        }
    }
    Ok(found)
}

/// One HID interface of a candidate device, from its configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HidInterface {
    pub number: u8,
    pub subclass: u8,
    pub protocol: u8,
}

/// Which interfaces may be read with a USB `GET_DESCRIPTOR(Report)` request.
///
/// Only when the registry already shows the device to be a digitizer
/// (`registry_declares_digitizer`) and only for HID interfaces the registry
/// left undescribed. A device with a boot-keyboard interface is never asked:
/// its missing descriptor makes it inadmissible instead.
///
/// # Errors
///
/// Returns a reason when a fallback read would be needed but is not allowed.
pub fn fallback_plan(
    interfaces: &[HidInterface],
    described: &BTreeMap<u8, Vec<u8>>,
    registry_declares_digitizer: bool,
) -> Result<Vec<u8>, FallbackRefusal> {
    let missing: Vec<u8> = interfaces
        .iter()
        .map(|interface| interface.number)
        .filter(|number| !described.contains_key(number))
        .collect();
    if missing.is_empty() {
        return Ok(missing);
    }
    if !registry_declares_digitizer {
        return Err(FallbackRefusal::NotADigitizer);
    }
    // Not a forwarding rule (a tablet's express-key keyboard is bridged with
    // it): this only keeps the probe from ever sending a request to anything
    // that could be someone's keyboard.
    if interfaces.iter().any(|interface| {
        arcen_usb_bridge::is_boot_keyboard_interface(interface.subclass, interface.protocol)
    }) {
        return Err(FallbackRefusal::BootKeyboard);
    }
    Ok(missing)
}

/// Why the helper will not send a `GET_DESCRIPTOR(Report)` request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FallbackRefusal {
    /// The registry does not show the device to be a digitizer.
    NotADigitizer,
    /// The device has a boot-keyboard interface.
    BootKeyboard,
}

/// Reads every `IOHIDDevice` with a report descriptor. `None` when the
/// registry could not be queried.
#[cfg(target_os = "macos")]
#[must_use]
pub fn hid_devices() -> Option<Vec<RegistryHid>> {
    ffi::hid_devices()
}

/// There is no I/O Registry off macOS.
#[cfg(not(target_os = "macos"))]
#[must_use]
pub fn hid_devices() -> Option<Vec<RegistryHid>> {
    None
}

#[cfg(target_os = "macos")]
#[allow(non_camel_case_types, unsafe_code)]
mod ffi {
    use super::{RegistryHid, MAX_HID_SERVICES};
    use std::ffi::{c_char, c_void, CStr};

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
    /// `kIOMainPortDefault` is `MACH_PORT_NULL`.
    const IO_MAIN_PORT_DEFAULT: mach_port_t = 0;
    const CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
    const CF_NUMBER_SINT64_TYPE: CFIndex = 4;
    /// `kIORegistryIterateRecursively | kIORegistryIterateParents`.
    const ITERATE_PARENTS_RECURSIVELY: u32 = 0x1 | 0x2;

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
        fn IORegistryEntrySearchCFProperty(
            entry: io_object_t,
            plane: *const c_char,
            key: CFStringRef,
            allocator: CFAllocatorRef,
            options: u32,
        ) -> CFTypeRef;
    }

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
            // would call `CFRelease(NULL)`, which traps.
            if object.is_null() {
                None
            } else {
                Some(Self(object))
            }
        }
    }

    impl Drop for Owned {
        fn drop(&mut self) {
            // SAFETY: a non-null Create-rule object this wrapper owns one
            // reference to, released exactly once.
            unsafe { CFRelease(self.0) }
        }
    }

    /// Owned I/O Kit object, released on drop.
    struct IoObject(io_object_t);

    impl Drop for IoObject {
        fn drop(&mut self) {
            if self.0 != 0 {
                // SAFETY: a nonzero object this wrapper owns.
                unsafe {
                    IOObjectRelease(self.0);
                }
            }
        }
    }

    fn key(name: &CStr) -> Option<Owned> {
        // SAFETY: `name` is NUL-terminated; null is handled by `Owned::new`.
        Owned::new(unsafe {
            CFStringCreateWithCString(std::ptr::null(), name.as_ptr(), CF_STRING_ENCODING_UTF8)
        })
    }

    fn own_property(entry: io_object_t, key: &Owned) -> Option<Owned> {
        // SAFETY: `entry` and `key` are live for the call; the result follows
        // the Create rule and is owned by `Owned`.
        Owned::new(unsafe { IORegistryEntryCreateCFProperty(entry, key.0, std::ptr::null(), 0) })
    }

    fn ancestor_property(entry: io_object_t, key: &Owned) -> Option<Owned> {
        // SAFETY: as above; the plane name is a NUL-terminated literal and the
        // search returns a Create-rule object or null.
        Owned::new(unsafe {
            IORegistryEntrySearchCFProperty(
                entry,
                c"IOService".as_ptr(),
                key.0,
                std::ptr::null(),
                ITERATE_PARENTS_RECURSIVELY,
            )
        })
    }

    fn number(value: Option<Owned>) -> Option<i64> {
        let value = value?;
        // SAFETY: `value.0` is a live CF object.
        if unsafe { CFGetTypeID(value.0) != CFNumberGetTypeID() } {
            return None;
        }
        let mut out: i64 = 0;
        // SAFETY: `value.0` is a CFNumber and `out` a valid i64 destination.
        let ok = unsafe {
            CFNumberGetValue(
                value.0,
                CF_NUMBER_SINT64_TYPE,
                std::ptr::addr_of_mut!(out).cast::<c_void>(),
            )
        };
        ok.then_some(out)
    }

    fn data(value: Option<Owned>) -> Option<Vec<u8>> {
        let value = value?;
        // SAFETY: `value.0` is a live CF object.
        if unsafe { CFGetTypeID(value.0) != CFDataGetTypeID() } {
            return None;
        }
        // SAFETY: `value.0` is a CFData.
        let length = usize::try_from(unsafe { CFDataGetLength(value.0) }).ok()?;
        if length == 0 || length > arcen_usb_bridge::MAX_HID_REPORT_DESCRIPTOR_BYTES {
            return None;
        }
        // SAFETY: `value` keeps the CFData alive for the copy.
        let bytes = unsafe { CFDataGetBytePtr(value.0) };
        if bytes.is_null() {
            return None;
        }
        // SAFETY: `bytes` points at `length` readable bytes.
        Some(unsafe { std::slice::from_raw_parts(bytes, length) }.to_vec())
    }

    pub(super) fn hid_devices() -> Option<Vec<RegistryHid>> {
        let vendor_key = key(c"VendorID")?;
        let product_key = key(c"ProductID")?;
        let location_key = key(c"LocationID")?;
        let interface_key = key(c"bInterfaceNumber")?;
        let descriptor_key = key(c"ReportDescriptor")?;
        // SAFETY: valid class name; the dictionary is consumed below.
        let matching = unsafe { IOServiceMatching(c"IOHIDDevice".as_ptr()) };
        if matching.is_null() {
            return None;
        }
        let mut iterator: io_iterator_t = 0;
        // SAFETY: consumes `matching`; `iterator` is a valid out-pointer.
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
            let Some(report_descriptor) = data(own_property(entry.0, &descriptor_key)) else {
                continue;
            };
            devices.push(RegistryHid {
                vendor_id: number(own_property(entry.0, &vendor_key))
                    .and_then(|value| u16::try_from(value).ok())
                    .unwrap_or(0),
                product_id: number(own_property(entry.0, &product_key))
                    .and_then(|value| u16::try_from(value).ok())
                    .unwrap_or(0),
                location_id: number(own_property(entry.0, &location_key))
                    .and_then(|value| u32::try_from(value).ok()),
                interface: number(ancestor_property(entry.0, &interface_key))
                    .and_then(|value| u8::try_from(value).ok()),
                report_descriptor,
            });
        }
        Some(devices)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcen_usb_bridge::descriptor_fixtures::{
        BOOT_KEYBOARD, INTUOS5_STYLE_MOUSE_INTERFACE, INTUOS5_STYLE_PEN_INTERFACE,
        INTUOS5_STYLE_VENDOR_CONTROL_INTERFACE,
    };

    fn entry(location: u32, interface: u8, descriptor: &[u8]) -> RegistryHid {
        RegistryHid {
            vendor_id: 0x056a,
            product_id: 0x0317,
            location_id: Some(location),
            interface: Some(interface),
            report_descriptor: descriptor.to_vec(),
        }
    }

    #[test]
    fn location_ids_follow_the_macos_nibble_layout() {
        assert_eq!(location_id(0x01, &[1]), Some(0x0110_0000));
        assert_eq!(location_id(0x14, &[2, 3]), Some(0x1423_0000));
        assert_eq!(location_id(0x02, &[1, 2, 3, 4, 5, 6]), Some(0x0212_3456));
        assert_eq!(location_id(0x02, &[]), None);
        assert_eq!(location_id(0x02, &[16]), None);
        assert_eq!(location_id(0x02, &[0]), None);
        assert_eq!(location_id(0x02, &[1, 1, 1, 1, 1, 1, 1]), None);
    }

    #[test]
    fn an_intuos5_style_tablet_is_described_entirely_by_the_registry() {
        let entries = [
            entry(0x0110_0000, 0, INTUOS5_STYLE_PEN_INTERFACE),
            entry(0x0110_0000, 1, INTUOS5_STYLE_VENDOR_CONTROL_INTERFACE),
            entry(0x0110_0000, 2, INTUOS5_STYLE_MOUSE_INTERFACE),
            // The same model on another port must not be mixed in.
            entry(0x0120_0000, 0, BOOT_KEYBOARD),
        ];
        let found = descriptors_for(&entries, 0x056a, 0x0317, Some(0x0110_0000)).unwrap();
        assert_eq!(found.len(), 3);
        let interfaces = [
            HidInterface {
                number: 0,
                subclass: 0,
                protocol: 0,
            },
            HidInterface {
                number: 1,
                subclass: 0,
                protocol: 0,
            },
            HidInterface {
                number: 2,
                subclass: 1,
                protocol: 2,
            },
        ];
        assert_eq!(fallback_plan(&interfaces, &found, true), Ok(Vec::new()));
        let profile = arcen_usb_bridge::classify_pen_tablet(found.values().map(Vec::as_slice))
            .expect("Intuos5-style tablet classifies");
        assert_eq!(
            profile.evidence,
            arcen_usb_bridge::PenTabletEvidence::VendorDigitizerPen
        );
        assert_eq!(profile.capabilities.pressure_levels(), None);
    }

    #[test]
    fn a_lone_vendor_device_is_not_a_tablet_but_one_with_express_keys_is() {
        let lone = [entry(
            0x0110_0000,
            0,
            INTUOS5_STYLE_VENDOR_CONTROL_INTERFACE,
        )];
        let found = descriptors_for(&lone, 0x056a, 0x0317, Some(0x0110_0000)).unwrap();
        assert!(arcen_usb_bridge::classify_pen_tablet(found.values().map(Vec::as_slice)).is_err());

        let with_keyboard = [
            entry(0x0110_0000, 0, INTUOS5_STYLE_PEN_INTERFACE),
            entry(0x0110_0000, 1, BOOT_KEYBOARD),
        ];
        let found = descriptors_for(&with_keyboard, 0x056a, 0x0317, Some(0x0110_0000)).unwrap();
        let claims: Vec<arcen_usb_bridge::InterfaceReportDescriptor> = found
            .into_iter()
            .map(
                |(interface, descriptor)| arcen_usb_bridge::InterfaceReportDescriptor {
                    interface,
                    descriptor,
                },
            )
            .collect();
        assert!(arcen_usb_bridge::authorize_hard_usb_tablet(0, &claims).is_ok());
    }

    #[test]
    fn identity_alone_matches_only_when_no_location_is_needed() {
        let mut unlocated = entry(0, 0, INTUOS5_STYLE_PEN_INTERFACE);
        unlocated.location_id = None;
        // A unique identity: no location needed, the entry matches.
        assert_eq!(
            descriptors_for(std::slice::from_ref(&unlocated), 0x056a, 0x0317, None)
                .unwrap()
                .len(),
            1
        );
        assert!(
            descriptors_for(std::slice::from_ref(&unlocated), 0x1234, 0x0317, None)
                .unwrap()
                .is_empty()
        );
    }

    /// The reviewed bypass: with two identical devices attached, an entry
    /// whose location is unknown used to match either of them, so one
    /// device's pen descriptor could authorize capturing the other.
    #[test]
    fn twins_need_a_known_equal_location_on_both_sides() {
        let mut unlocated = entry(0, 0, INTUOS5_STYLE_PEN_INTERFACE);
        unlocated.location_id = None;
        let other_port = entry(0x0120_0000, 0, INTUOS5_STYLE_PEN_INTERFACE);
        let found =
            descriptors_for(&[unlocated, other_port], 0x056a, 0x0317, Some(0x0110_0000)).unwrap();
        assert!(found.is_empty(), "neither entry is provably this device");
    }

    #[test]
    fn conflicting_entries_for_one_interface_are_refused() {
        let entries = [
            entry(0x0110_0000, 0, INTUOS5_STYLE_PEN_INTERFACE),
            entry(0x0110_0000, 0, BOOT_KEYBOARD),
        ];
        assert_eq!(
            descriptors_for(&entries, 0x056a, 0x0317, Some(0x0110_0000)),
            Err(RegistryAmbiguity::ConflictingInterface(0))
        );
        // Identical duplicates are not a conflict.
        let duplicates = [
            entry(0x0110_0000, 0, INTUOS5_STYLE_PEN_INTERFACE),
            entry(0x0110_0000, 0, INTUOS5_STYLE_PEN_INTERFACE),
        ];
        assert_eq!(
            descriptors_for(&duplicates, 0x056a, 0x0317, None)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn usb_requests_are_reserved_for_digitizers_without_a_boot_keyboard() {
        let interfaces = [
            HidInterface {
                number: 0,
                subclass: 0,
                protocol: 0,
            },
            HidInterface {
                number: 1,
                subclass: 0,
                protocol: 0,
            },
        ];
        let mut described = BTreeMap::new();
        described.insert(0, INTUOS5_STYLE_PEN_INTERFACE.to_vec());
        assert_eq!(fallback_plan(&interfaces, &described, true), Ok(vec![1]));
        assert_eq!(
            fallback_plan(&interfaces, &described, false),
            Err(FallbackRefusal::NotADigitizer)
        );
        let with_keyboard = [
            HidInterface {
                number: 0,
                subclass: 0,
                protocol: 0,
            },
            HidInterface {
                number: 1,
                subclass: 1,
                protocol: 1,
            },
        ];
        assert_eq!(
            fallback_plan(&with_keyboard, &described, true),
            Err(FallbackRefusal::BootKeyboard)
        );
        // Nothing missing: no request at all, whatever the device is.
        assert_eq!(
            fallback_plan(&interfaces[..1], &described, false),
            Ok(Vec::new())
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_live_registry_is_readable_and_bounded() {
        let devices = hid_devices().expect("the I/O Registry is readable");
        assert!(devices.len() <= MAX_HID_SERVICES);
        for device in devices {
            assert!(!device.report_descriptor.is_empty());
        }
    }
}
