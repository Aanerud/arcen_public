//! A keyboard and pointer macOS treats as hardware.
//!
//! `CGEvent` posting is enough for an application in a logged-in session, and
//! it is what the Pier uses there. It does not reach the login window, and it
//! is refused wherever the system will only take input it believes a person
//! typed. A virtual HID device is that input: an `IOHIDUserDevice` whose
//! reports the HID system handles exactly as a USB keyboard's.
//!
//! Two conditions gate it, and both were measured on the lab Mac rather than
//! read. The process creating the device must carry
//! `com.apple.developer.hid.virtual.device`, which only the `pier.arcen.tech`
//! signature does; and the app responsible for that process must hold
//! Accessibility, which the Agent Helper does. So the helper starts the Pier's
//! own executable as a child — `arcen-pier-macos hid-injector` — and feeds it
//! reports on its standard input. The child owns the device and nothing else:
//! no network, no session, no policy. What is held, and which key is which,
//! stays in the helper and in `arcen_input::hid_reports`.
//!
//! The same child owns an absolute pointer. After sign-in the Pier moves the
//! pointer with `CGEvent`, which it can place exactly; at the login window it
//! must not. Measured on the lab: the first `CGEventCreateMouseEvent` a root
//! agent made there never returned — `SkyLight` blocked on a lock inside its
//! own event-source lookup — and it took the whole session down with it. So the
//! login window's pointer is a device too, as it is in other remote desktops
//! that serve that screen.
//!
//! If the helper goes away the child's input closes, and it releases every key
//! and button before exiting. A device that outlives the session holding
//! something down is the failure this is built to rule out.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, ExitCode, Stdio};
use std::time::Duration;

use arcen_input::hid_reports::{
    HELPER_READY, HelperFrame, KEYBOARD_DESCRIPTOR, KeyboardState, POINTER_DESCRIPTOR,
    PointerState, VirtualDeviceKind,
};
/// How long a new device is given to be attached before it is used.
const DEVICE_SETTLE: Duration = Duration::from_millis(300);
/// How long the helper waits for the child to say it is ready.
const READY_TIMEOUT: Duration = Duration::from_secs(3);
/// USB-IF's code for "no vendor"; the device claims to be nobody's product.
const VENDOR_ID: u32 = 0x1209;
/// Product id within that space for Arcen's keyboard.
const KEYBOARD_PRODUCT_ID: u32 = 0xA2C0;
/// Product id within that space for Arcen's pointer.
const POINTER_PRODUCT_ID: u32 = 0xA2C1;

/// Maps a macOS virtual key code onto a HID keyboard usage.
///
/// The inverse of the table the HID system uses to produce virtual key codes,
/// so a key the `CGEvent` path would post as code `n` arrives here as the usage
/// that the system turns back into `n`. Returns `None` for keys a keyboard does
/// not carry, such as Fn.
#[must_use]
pub const fn macos_key_to_hid_usage(code: u16) -> Option<u8> {
    Some(match code {
        0x00 => 0x04, // A
        0x0B => 0x05, // B
        0x08 => 0x06, // C
        0x02 => 0x07, // D
        0x0E => 0x08, // E
        0x03 => 0x09, // F
        0x05 => 0x0A, // G
        0x04 => 0x0B, // H
        0x22 => 0x0C, // I
        0x26 => 0x0D, // J
        0x28 => 0x0E, // K
        0x25 => 0x0F, // L
        0x2E => 0x10, // M
        0x2D => 0x11, // N
        0x1F => 0x12, // O
        0x23 => 0x13, // P
        0x0C => 0x14, // Q
        0x0F => 0x15, // R
        0x01 => 0x16, // S
        0x11 => 0x17, // T
        0x20 => 0x18, // U
        0x09 => 0x19, // V
        0x0D => 0x1A, // W
        0x07 => 0x1B, // X
        0x10 => 0x1C, // Y
        0x06 => 0x1D, // Z
        0x12 => 0x1E, // 1
        0x13 => 0x1F, // 2
        0x14 => 0x20, // 3
        0x15 => 0x21, // 4
        0x17 => 0x22, // 5
        0x16 => 0x23, // 6
        0x1A => 0x24, // 7
        0x1C => 0x25, // 8
        0x19 => 0x26, // 9
        0x1D => 0x27, // 0
        0x24 => 0x28, // Return
        0x35 => 0x29, // Escape
        0x33 => 0x2A, // Delete (backspace)
        0x30 => 0x2B, // Tab
        0x31 => 0x2C, // Space
        0x1B => 0x2D, // -
        0x18 => 0x2E, // =
        0x21 => 0x2F, // [
        0x1E => 0x30, // ]
        0x2A => 0x31, // backslash
        0x29 => 0x33, // ;
        0x27 => 0x34, // '
        0x32 => 0x35, // `
        0x2B => 0x36, // ,
        0x2F => 0x37, // .
        0x2C => 0x38, // /
        0x39 => 0x39, // Caps Lock
        0x7A => 0x3A, // F1
        0x78 => 0x3B, // F2
        0x63 => 0x3C, // F3
        0x76 => 0x3D, // F4
        0x60 => 0x3E, // F5
        0x61 => 0x3F, // F6
        0x62 => 0x40, // F7
        0x64 => 0x41, // F8
        0x65 => 0x42, // F9
        0x6D => 0x43, // F10
        0x67 => 0x44, // F11
        0x6F => 0x45, // F12
        0x72 => 0x49, // Help / Insert
        0x73 => 0x4A, // Home
        0x74 => 0x4B, // Page Up
        0x75 => 0x4C, // Forward Delete
        0x77 => 0x4D, // End
        0x79 => 0x4E, // Page Down
        0x7C => 0x4F, // Right
        0x7B => 0x50, // Left
        0x7D => 0x51, // Down
        0x7E => 0x52, // Up
        0x47 => 0x53, // Keypad Clear
        0x4B => 0x54, // Keypad /
        0x43 => 0x55, // Keypad *
        0x4E => 0x56, // Keypad -
        0x45 => 0x57, // Keypad +
        0x4C => 0x58, // Keypad Enter
        0x53 => 0x59, // Keypad 1
        0x54 => 0x5A, // Keypad 2
        0x55 => 0x5B, // Keypad 3
        0x56 => 0x5C, // Keypad 4
        0x57 => 0x5D, // Keypad 5
        0x58 => 0x5E, // Keypad 6
        0x59 => 0x5F, // Keypad 7
        0x5B => 0x60, // Keypad 8
        0x5C => 0x61, // Keypad 9
        0x52 => 0x62, // Keypad 0
        0x41 => 0x63, // Keypad .
        0x0A => 0x64, // ISO section
        0x51 => 0x67, // Keypad =
        0x69 => 0x68, // F13
        0x6B => 0x69, // F14
        0x71 => 0x6A, // F15
        0x6A => 0x6B, // F16
        0x40 => 0x6C, // F17
        0x4F => 0x6D, // F18
        0x50 => 0x6E, // F19
        0x5A => 0x6F, // F20
        0x4A => 0x7F, // Mute
        0x48 => 0x80, // Volume Up
        0x49 => 0x81, // Volume Down
        0x3B => 0xE0, // Control
        0x38 => 0xE1, // Shift
        0x3A => 0xE2, // Option
        0x37 => 0xE3, // Command
        0x3E => 0xE4, // Right Control
        0x3C => 0xE5, // Right Shift
        0x3D => 0xE6, // Right Option
        0x36 => 0xE7, // Right Command
        _ => return None,
    })
}

/// Why the virtual keyboard could not be used.
#[derive(Debug)]
pub enum VirtualKeyboardError {
    /// The child could not be started or talked to.
    Io(std::io::Error),
    /// The child started and said it could not create the device.
    Refused(String),
    /// The child did not say anything in time.
    NotReady,
    /// A seventh key was pressed while six were held. The key is refused and
    /// the device is fine.
    Rollover,
}

impl std::fmt::Display for VirtualKeyboardError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "virtual keyboard: {error}"),
            Self::Refused(reason) => write!(formatter, "virtual keyboard refused: {reason}"),
            Self::NotReady => formatter.write_str("virtual keyboard did not start in time"),
            Self::Rollover => formatter.write_str("virtual keyboard already holds six keys"),
        }
    }
}

impl std::error::Error for VirtualKeyboardError {}

impl From<std::io::Error> for VirtualKeyboardError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

/// The helper's handle on the injector child and its two devices.
pub struct VirtualHid {
    child: Child,
    input: Option<ChildStdin>,
    state: KeyboardState,
    pointer: PointerState,
    /// Whether this handle has ever placed the pointer. A pointer it never
    /// drove is not released: its default position is the middle of the
    /// desktop, and an absolute report of it would move a cursor that
    /// `CGEvent` has been placing somewhere else.
    pointer_driven: bool,
}

impl std::fmt::Debug for VirtualHid {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("VirtualHid")
            .field("pid", &self.child.id())
            .field("state", &self.state)
            .field("pointer", &self.pointer)
            .finish_non_exhaustive()
    }
}

impl VirtualHid {
    /// Starts `program hid-injector` and waits for its devices.
    ///
    /// # Errors
    ///
    /// Returns [`VirtualKeyboardError`] when the child cannot start, refuses,
    /// or says nothing within the readiness budget.
    pub fn spawn(program: &Path) -> Result<Self, VirtualKeyboardError> {
        let mut child = Command::new(program)
            .arg("hid-injector")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;
        let input = child.stdin.take().ok_or(VirtualKeyboardError::NotReady)?;
        let mut output = child.stdout.take().ok_or(VirtualKeyboardError::NotReady)?;
        let (sender, answer) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut line = Vec::new();
            let mut byte = [0_u8; 1];
            while output.read(&mut byte).is_ok_and(|read| read == 1) && byte[0] != b'\n' {
                line.push(byte[0]);
            }
            let _ = sender.send(String::from_utf8_lossy(&line).into_owned());
        });
        match answer.recv_timeout(READY_TIMEOUT) {
            Ok(line) if line == HELPER_READY => Ok(Self {
                child,
                input: Some(input),
                state: KeyboardState::new(),
                pointer: PointerState::new(),
                pointer_driven: false,
            }),
            Ok(line) => {
                let _ = child.kill();
                let _ = child.wait();
                Err(VirtualKeyboardError::Refused(line))
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                Err(VirtualKeyboardError::NotReady)
            }
        }
    }

    /// Presses or releases one HID usage and sends the resulting report.
    ///
    /// # Errors
    ///
    /// Returns the write error when the child has gone, and the rollover error
    /// for a seventh held key.
    pub fn set(&mut self, usage: u8, pressed: bool) -> Result<(), VirtualKeyboardError> {
        let changed = self
            .state
            .set(usage, pressed)
            .map_err(|_| VirtualKeyboardError::Rollover)?;
        if changed {
            self.send()?;
        }
        Ok(())
    }

    /// Places the pointer at a normalized desktop position.
    ///
    /// # Errors
    ///
    /// Returns the write error when the child has gone.
    pub fn move_pointer(&mut self, x: f64, y: f64) -> Result<(), VirtualKeyboardError> {
        self.pointer.move_to(x, y);
        self.pointer_driven = true;
        self.send_pointer(0, 0)
    }

    /// Presses or releases a pointer button, numbered as on the wire.
    ///
    /// # Errors
    ///
    /// Returns the write error when the child has gone.
    pub fn pointer_button(
        &mut self,
        button: u8,
        pressed: bool,
    ) -> Result<(), VirtualKeyboardError> {
        self.pointer_driven = true;
        if self.pointer.set_button(button, pressed) {
            self.send_pointer(0, 0)?;
        }
        Ok(())
    }

    /// Scrolls by whole detents; positive `wheel` is up, positive `pan` right.
    ///
    /// # Errors
    ///
    /// Returns the write error when the child has gone.
    pub fn scroll(&mut self, wheel: i8, pan: i8) -> Result<(), VirtualKeyboardError> {
        if wheel == 0 && pan == 0 {
            return Ok(());
        }
        self.pointer_driven = true;
        self.send_pointer(wheel, pan)
    }

    /// Whether a pointer button is held.
    #[must_use]
    pub const fn has_held_buttons(&self) -> bool {
        self.pointer.has_buttons()
    }

    /// Releases every key and pointer button.
    ///
    /// # Errors
    ///
    /// Returns the first write error when the child has gone.
    pub fn release_all(&mut self) -> Result<(), VirtualKeyboardError> {
        self.state.clear();
        let keys = self.send();
        if !self.pointer_driven {
            return keys;
        }
        self.pointer.clear_buttons();
        let buttons = self.send_pointer(0, 0);
        keys.and(buttons)
    }

    fn send(&mut self) -> Result<(), VirtualKeyboardError> {
        let report = self.state.report();
        self.write(VirtualDeviceKind::Keyboard, &report)
    }

    fn send_pointer(&mut self, wheel: i8, pan: i8) -> Result<(), VirtualKeyboardError> {
        let report = self.pointer.report(wheel, pan);
        self.write(VirtualDeviceKind::Pointer, &report)
    }

    fn write(
        &mut self,
        kind: VirtualDeviceKind,
        report: &[u8],
    ) -> Result<(), VirtualKeyboardError> {
        let input = self.input.as_mut().ok_or(VirtualKeyboardError::NotReady)?;
        input.write_all(&HelperFrame::encode(kind, report))?;
        input.flush()?;
        Ok(())
    }
}

impl Drop for VirtualHid {
    fn drop(&mut self) {
        let _ = self.release_all();
        // Closing input is the child's signal to release and exit on its own.
        // It is given a moment to do so, then made to.
        drop(self.input.take());
        let deadline = std::time::Instant::now() + Duration::from_millis(500);
        while std::time::Instant::now() < deadline {
            if self.child.try_wait().is_ok_and(|status| status.is_some()) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Runs the injector child: creates the devices, then turns every frame read
/// from standard input into input, speaking the shared helper contract in
/// [`arcen_input::hid_reports`].
///
/// Prints [`HELPER_READY`] once both devices exist, or one line saying why
/// they do not.
#[must_use]
pub fn run_injector() -> ExitCode {
    let devices =
        native::VirtualDevice::new(KEYBOARD_DESCRIPTOR, KEYBOARD_PRODUCT_ID, "Arcen Keyboard")
            .and_then(|keyboard| {
                native::VirtualDevice::new(POINTER_DESCRIPTOR, POINTER_PRODUCT_ID, "Arcen Pointer")
                    .map(|pointer| (keyboard, pointer))
            });
    let (keyboard, pointer) = match devices {
        Ok(devices) => devices,
        Err(reason) => {
            println!("{reason}");
            return ExitCode::FAILURE;
        }
    };
    let device = |kind: VirtualDeviceKind| match kind {
        VirtualDeviceKind::Keyboard => &keyboard,
        VirtualDeviceKind::Pointer => &pointer,
    };
    let mut last: std::collections::HashMap<VirtualDeviceKind, Vec<u8>> =
        std::collections::HashMap::new();
    // The HID system attaches a new device asynchronously. A report sent the
    // instant the device is activated is dropped: measured on the lab, the
    // first letter of a probe phrase never arrived and every later one did.
    std::thread::sleep(DEVICE_SETTLE);
    println!("{HELPER_READY}");
    let _ = std::io::stdout().flush();

    let mut stdin = std::io::stdin().lock();
    let mut report = [0_u8; arcen_input::hid_reports::MAX_REPORT_LEN];
    let exit = loop {
        let mut header = [0_u8; 2];
        // The helper closed its end, or died. Either way nobody is left to
        // release what is held, so this does.
        if stdin.read_exact(&mut header).is_err() {
            break ExitCode::SUCCESS;
        }
        let (kind, length) = match HelperFrame::header(header) {
            Ok(parsed) => parsed,
            Err(error) => {
                eprintln!("hid-injector: {error}");
                break ExitCode::FAILURE;
            }
        };
        if stdin.read_exact(&mut report[..length]).is_err() {
            break ExitCode::SUCCESS;
        }
        if let Err(reason) = device(kind).send(&report[..length]) {
            eprintln!("hid-injector: {reason}");
            break ExitCode::FAILURE;
        }
        last.insert(kind, report[..length].to_vec());
    };
    // Only devices that were used. A device never sent a report holds nothing,
    // and releasing a pointer nobody placed would move the cursor.
    for kind in VirtualDeviceKind::ALL {
        if let Some(held) = last.get(&kind) {
            let _ = device(kind).send(&kind.released(held));
        }
    }
    // A moment for the release to be delivered before the device goes away
    // with the process.
    std::thread::sleep(Duration::from_millis(50));
    exit
}

#[cfg(target_os = "macos")]
mod native {
    #![allow(unsafe_code)]

    use std::ffi::c_void;

    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2_core_foundation::CFDictionary;
    use objc2_foundation::{NSData, NSDictionary, NSNumber, NSString};

    // SAFETY: signatures match `IOKit/hidsystem/IOHIDUserDevice.h` and
    // `<dispatch/queue.h>`; every reference is an opaque pointer.
    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IOHIDUserDeviceCreateWithProperties(
            allocator: *const c_void,
            properties: *const CFDictionary,
            options: u32,
        ) -> *mut c_void;
        fn IOHIDUserDeviceSetDispatchQueue(device: *mut c_void, queue: *mut c_void);
        fn IOHIDUserDeviceActivate(device: *mut c_void);
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
    }

    /// A live virtual HID device, owned by this process for its lifetime.
    pub(super) struct VirtualDevice {
        device: *mut c_void,
    }

    impl VirtualDevice {
        pub(super) fn new(descriptor: &[u8], product_id: u32, name: &str) -> Result<Self, String> {
            let number = |value: u32| -> Retained<NSNumber> { NSNumber::new_u32(value) };
            let descriptor = NSData::with_bytes(descriptor);
            let vendor = number(super::VENDOR_ID);
            let product_id = number(product_id);
            let product = NSString::from_str(name);
            let keys = [
                &*NSString::from_str("ReportDescriptor"),
                &*NSString::from_str("VendorID"),
                &*NSString::from_str("ProductID"),
                &*NSString::from_str("Product"),
            ];
            let values: [&AnyObject; 4] = [
                descriptor.as_ref(),
                vendor.as_ref(),
                product_id.as_ref(),
                product.as_ref(),
            ];
            let properties = NSDictionary::from_slices(&keys, &values);
            // SAFETY: `NSDictionary` is toll-free bridged to `CFDictionary`
            // and outlives the call; a null allocator selects the default.
            let device = unsafe {
                IOHIDUserDeviceCreateWithProperties(
                    std::ptr::null(),
                    Retained::as_ptr(&properties).cast::<CFDictionary>(),
                    0,
                )
            };
            if device.is_null() {
                return Err(crate::virtual_hid::VirtualHidSupport::Refused
                    .refusal()
                    .unwrap_or("virtual HID refused")
                    .to_owned());
            }
            // SAFETY: a static label and a null attribute make a serial queue,
            // which lives for the process. The device is non-null and not yet
            // activated, which is when a queue may be set.
            unsafe {
                let queue =
                    dispatch_queue_create(c"tech.arcen.pier.hid".as_ptr(), std::ptr::null());
                IOHIDUserDeviceSetDispatchQueue(device, queue);
                IOHIDUserDeviceActivate(device);
            }
            Ok(Self { device })
        }

        pub(super) fn send(&self, report: &[u8]) -> Result<(), String> {
            // SAFETY: the device is live and activated; `report` is valid for
            // its length for the duration of the call.
            let status = unsafe {
                IOHIDUserDeviceHandleReportWithTimeStamp(
                    self.device,
                    mach_absolute_time(),
                    report.as_ptr(),
                    isize::try_from(report.len()).unwrap_or(isize::MAX),
                )
            };
            if status == 0 {
                Ok(())
            } else {
                Err(format!("report refused with IOReturn {status:#x}"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_mapped_macos_key_has_a_distinct_usage() {
        let mut seen = std::collections::BTreeMap::new();
        for code in 0_u16..=0x7F {
            if let Some(usage) = macos_key_to_hid_usage(code) {
                assert_ne!(usage, 0, "code {code:#x} maps to the reserved usage");
                if let Some(previous) = seen.insert(usage, code) {
                    panic!("usage {usage:#x} is claimed by {previous:#x} and {code:#x}");
                }
            }
        }
        assert!(seen.len() > 100, "the table covers a full keyboard");
    }

    #[test]
    fn the_keymap_and_the_usage_table_agree_on_common_keys() {
        use crate::input::keymap::{MOD_CTRL, MOD_SHIFT, qt_key_to_macos};
        let usage = |qt: u32, modifiers: u32| {
            qt_key_to_macos(qt, modifiers).and_then(macos_key_to_hid_usage)
        };
        assert_eq!(usage(0x41, 0), Some(0x04), "A");
        assert_eq!(usage(0x5A, 0), Some(0x1D), "Z");
        assert_eq!(usage(0x30, 0), Some(0x27), "0");
        assert_eq!(usage(0x0100_0004, 0), Some(0x28), "Return");
        assert_eq!(usage(0x0100_0020, MOD_SHIFT), Some(0xE1), "Shift");
        assert_eq!(usage(0x0100_0021, MOD_CTRL), Some(0xE0), "Control");
    }

    #[test]
    fn fn_is_not_a_keyboard_usage() {
        assert_eq!(macos_key_to_hid_usage(0x3F), None);
    }
}
