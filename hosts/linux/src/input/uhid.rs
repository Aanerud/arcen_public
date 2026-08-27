use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::io::AsRawFd;

// /dev/uhid kernel structs.  Layout from <linux/uhid.h>.
// We only use UHID_CREATE2 and UHID_INPUT2.

const UHID_CREATE2: u32 = 11;
const UHID_INPUT2: u32 = 12;

const UHID_MAX_REPORT_SIZE: usize = 4096;
const UHID_MAX_NAME_LEN: usize = 128;
const UHID_MAX_RD_SIZE: usize = 4096;

// uhid_create2_req layout (248 bytes on x86_64 / aarch64):
//   u8  name[128]
//   u8  phys[64]
//   u8  uniq[64]
//   u16 rd_size       (report descriptor size)
//   u16 bus           (BUS_USB = 3)
//   u32 vendor
//   u32 product
//   u32 version
//   u32 country
//   u8  rd_data[4096]
//
// struct uhid_event:
//   u32 type
//   union { uhid_create2_req create2; uhid_input2_req input2; ... }

#[repr(C, packed)]
struct UhidCreate2Req {
    name: [u8; UHID_MAX_NAME_LEN],
    phys: [u8; 64],
    uniq: [u8; 64],
    rd_size: u16,
    bus: u16,
    vendor: u32,
    product: u32,
    version: u32,
    country: u32,
    rd_data: [u8; UHID_MAX_RD_SIZE],
}

#[repr(C, packed)]
struct UhidInput2Req {
    size: u16,
    data: [u8; UHID_MAX_REPORT_SIZE],
}

// uhid_event is a u32 type followed by a union of the request types.
// We just write the raw bytes — the kernel only reads `type` + the active union member.
#[repr(C, packed)]
struct UhidEventCreate2 {
    event_type: u32,
    req: UhidCreate2Req,
}

#[repr(C, packed)]
struct UhidEventInput2 {
    event_type: u32,
    size: u16,
    data: [u8; UHID_MAX_REPORT_SIZE],
}

const BUS_USB: u16 = 3;

/// A virtual HID device backed by /dev/uhid.  Dropping it removes the device.
pub struct UhidDevice {
    file: File,
    device_id: u8,
}

// SEC-raw-hid: deliberate, minimal `Debug` — never derive/delegate to
// `File`'s `Debug`, which (on Linux) resolves and prints the underlying
// path/fd via `/proc/self/fd`. That's not needed for diagnostics here and
// would leak more than callers (e.g. test failure output, `unwrap_err`
// panics) should print for a kernel-facing HID handle. Only the harmless,
// already-public `device_id` is shown; the file handle stays opaque.
impl std::fmt::Debug for UhidDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UhidDevice")
            .field("device_id", &self.device_id)
            .finish_non_exhaustive()
    }
}

impl UhidDevice {
    /// Create a virtual HID device that mirrors the physical tablet.
    /// `descriptor` must be the verbatim USB HID report descriptor.
    ///
    /// # Errors
    ///
    /// Rejects an empty or oversize descriptor before ever opening
    /// `/dev/uhid` — a truncated descriptor would silently corrupt the HID
    /// item stream the kernel then parses, so this bound must reject rather
    /// than clamp.
    pub fn create(
        name: &str,
        vendor_id: u16,
        product_id: u16,
        descriptor: &[u8],
        device_id: u8,
    ) -> io::Result<Self> {
        if descriptor.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "HID report descriptor must not be empty",
            ));
        }
        if descriptor.len() > UHID_MAX_RD_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "HID report descriptor of {} bytes exceeds the {} byte bound",
                    descriptor.len(),
                    UHID_MAX_RD_SIZE
                ),
            ));
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/uhid")?;

        let desc_len = descriptor.len();
        let mut req = UhidCreate2Req {
            name: [0u8; UHID_MAX_NAME_LEN],
            phys: [0u8; 64],
            uniq: [0u8; 64],
            rd_size: desc_len as u16,
            bus: BUS_USB,
            vendor: vendor_id as u32,
            product: product_id as u32,
            version: 0,
            country: 0,
            rd_data: [0u8; UHID_MAX_RD_SIZE],
        };

        let name_bytes = name.as_bytes();
        let copy_len = name_bytes.len().min(UHID_MAX_NAME_LEN - 1);
        req.name[..copy_len].copy_from_slice(&name_bytes[..copy_len]);
        req.rd_data[..desc_len].copy_from_slice(descriptor);

        let event = UhidEventCreate2 {
            event_type: UHID_CREATE2,
            req,
        };
        // SAFETY: `UhidEventCreate2` is `#[repr(C, packed)]` and every field
        // is a plain integer/byte-array, so reinterpreting it as a byte slice
        // of exactly `size_of::<UhidEventCreate2>()` bytes is well-defined;
        // the slice does not outlive `event`, which is not mutated for the
        // duration of the borrow.
        let bytes = unsafe {
            std::slice::from_raw_parts(
                &event as *const UhidEventCreate2 as *const u8,
                std::mem::size_of::<UhidEventCreate2>(),
            )
        };
        (&file).write_all(bytes)?;

        Ok(UhidDevice { file, device_id })
    }

    pub fn device_id(&self) -> u8 {
        self.device_id
    }

    /// Inject a raw HID input report into the virtual device.
    ///
    /// # Errors
    ///
    /// Rejects an oversize report rather than truncating it, since a
    /// truncated report would silently misrepresent the physical device's
    /// state to whatever reads the virtual device.
    pub fn write_report(&self, report: &[u8]) -> io::Result<()> {
        if report.len() > UHID_MAX_REPORT_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "HID report of {} bytes exceeds the {} byte bound",
                    report.len(),
                    UHID_MAX_REPORT_SIZE
                ),
            ));
        }
        let report_len = report.len();
        let mut event = UhidEventInput2 {
            event_type: UHID_INPUT2,
            size: report_len as u16,
            data: [0u8; UHID_MAX_REPORT_SIZE],
        };
        event.data[..report_len].copy_from_slice(report);

        // SAFETY: `UhidEventInput2` is `#[repr(C, packed)]` and every field
        // is a plain integer/byte-array, so reinterpreting it as a byte slice
        // of exactly `size_of::<UhidEventInput2>()` bytes is well-defined;
        // the slice does not outlive `event`, which is not mutated for the
        // duration of the borrow.
        let bytes = unsafe {
            std::slice::from_raw_parts(
                &event as *const UhidEventInput2 as *const u8,
                std::mem::size_of::<UhidEventInput2>(),
            )
        };
        (&self.file).write_all(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rejection must happen before `/dev/uhid` is ever opened, so this test
    /// runs without root or a `uhid` kernel module.
    #[test]
    fn create_rejects_empty_descriptor_without_opening_dev_uhid() {
        let err = UhidDevice::create("arcen-test", 0x056A, 0x0001, &[], 0).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    /// A hostile or buggy peer's oversize descriptor claim must be rejected
    /// outright rather than silently truncated into a corrupt HID item
    /// stream, and this must happen before `/dev/uhid` is opened.
    #[test]
    fn create_rejects_oversize_descriptor_without_opening_dev_uhid() {
        let oversize = vec![0xAAu8; UHID_MAX_RD_SIZE + 1];
        let err = UhidDevice::create("arcen-test", 0x056A, 0x0001, &oversize, 0).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    /// A hostile or buggy peer's oversize report must be rejected outright
    /// rather than silently truncated, and this must happen before any bytes
    /// reach the kernel-facing `/dev/uhid` write.
    #[test]
    fn write_report_rejects_oversize_report_without_writing() {
        // /dev/null is a stand-in file handle: the bound check must reject
        // before `write_all` is ever reached, so no real uhid device is
        // required to prove the rejection.
        let file = OpenOptions::new()
            .write(true)
            .open("/dev/null")
            .expect("/dev/null must be writable in test environments");
        let dev = UhidDevice { file, device_id: 0 };
        let oversize = vec![0xAAu8; UHID_MAX_REPORT_SIZE + 1];
        let err = dev.write_report(&oversize).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    /// A report exactly at the bound must still be accepted (and not
    /// mistaken for the oversize-rejection case above).
    #[test]
    fn write_report_accepts_report_exactly_at_bound() {
        let file = OpenOptions::new()
            .write(true)
            .open("/dev/null")
            .expect("/dev/null must be writable in test environments");
        let dev = UhidDevice { file, device_id: 0 };
        let at_bound = vec![0xAAu8; UHID_MAX_REPORT_SIZE];
        assert!(dev.write_report(&at_bound).is_ok());
    }
}

impl Drop for UhidDevice {
    fn drop(&mut self) {
        // Writing UHID_DESTROY to /dev/uhid removes the virtual device from the kernel.
        const UHID_DESTROY: u32 = 1;
        let event_type = UHID_DESTROY;
        let _ = (&self.file).write_all(unsafe {
            std::slice::from_raw_parts(
                &event_type as *const u32 as *const u8,
                std::mem::size_of::<u32>(),
            )
        });
    }
}
