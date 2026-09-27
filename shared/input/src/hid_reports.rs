//! HID reports for a host that presents input as hardware.
//!
//! A host that injects through a virtual HID device — `IOHIDUserDevice` on
//! macOS, `uhid` on Linux, the Virtual HID Framework on Windows — needs the
//! same three things whatever the operating system: a report descriptor that
//! says what the device is, the state of what is held, and the bytes that state
//! becomes. Those are defined by the USB HID specification, not by any
//! platform, so they live here once. A platform adapter maps its own key codes
//! onto HID usages and hands the bytes to its OS.
//!
//! The *helper* that owns the device is also shared in shape, though not in
//! code. Every platform isolates device creation in a small process of its
//! own — the identity allowed to create devices is rarely the one that should
//! run a session — and every such helper speaks [`HelperFrame`]s on its
//! standard input: one line saying [`HELPER_READY`] or why not, then frames,
//! and when its input closes it sends each device it has driven its
//! [released](VirtualDeviceKind::released) report before it exits. A device it
//! never drove holds nothing and is left alone. A helper
//! that outlives its session holding a key down is the failure the last rule
//! exists to prevent.
//!
//! The keyboard is the boot-protocol layout every operating system accepts: one
//! modifier byte, one reserved byte, six key slots. A seventh simultaneous key
//! is refused rather than dropped silently, so a caller can count it.
//!
//! The pointer is absolute, the shape of a USB tablet rather than a mouse: a
//! mouse reports motion, which the receiving system accelerates, so a remote
//! pointer driven through one lands near where the client drew it rather than
//! on it. An absolute device reports *where*, so it lands exactly, and needs
//! no knowledge of where the cursor already is.

/// The line a helper prints once its devices exist.
pub const HELPER_READY: &str = "ready";

/// Longest report a helper frame carries.
pub const MAX_REPORT_LEN: usize = 64;

/// Which virtual device a frame is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum VirtualDeviceKind {
    /// A boot keyboard ([`KEYBOARD_DESCRIPTOR`]).
    Keyboard = 1,
    /// An absolute pointer ([`POINTER_DESCRIPTOR`]).
    Pointer = 2,
}

impl VirtualDeviceKind {
    /// Every kind, so a helper can release all of them.
    pub const ALL: [Self; 2] = [Self::Keyboard, Self::Pointer];

    /// The kind a frame's first byte names.
    #[must_use]
    pub const fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            1 => Some(Self::Keyboard),
            2 => Some(Self::Pointer),
            _ => None,
        }
    }

    /// The report that lets go of everything `last` was holding.
    ///
    /// A keyboard releases to all zeroes. A pointer keeps its position and
    /// releases only its buttons: an absolute report of zeroes would also throw
    /// the cursor into the top-left corner of the desktop, which is not a
    /// release but a move nobody asked for. A `last` that is not a whole report
    /// releases to the device's zero report.
    #[must_use]
    pub fn released(self, last: &[u8]) -> Vec<u8> {
        let mut report = vec![0; self.report_len()];
        if self == Self::Pointer && last.len() == POINTER_REPORT_LEN {
            report[1..5].copy_from_slice(&last[1..5]);
        }
        report
    }

    /// How long this device's reports are.
    #[must_use]
    pub const fn report_len(self) -> usize {
        match self {
            Self::Keyboard => KEYBOARD_REPORT_LEN,
            Self::Pointer => POINTER_REPORT_LEN,
        }
    }
}

/// One report on its way to a helper: `[kind][length][report bytes]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelperFrame {
    /// The device.
    pub kind: VirtualDeviceKind,
    /// The report.
    pub report: Vec<u8>,
}

/// A frame could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelperFrameError {
    /// The first byte names no device.
    UnknownDevice(u8),
    /// The length is not this device's report length.
    WrongLength {
        /// The device.
        kind: VirtualDeviceKind,
        /// The length the frame claimed.
        length: usize,
    },
}

impl std::fmt::Display for HelperFrameError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownDevice(byte) => write!(formatter, "frame names unknown device {byte}"),
            Self::WrongLength { kind, length } => write!(
                formatter,
                "{kind:?} reports are {} bytes, frame carried {length}",
                kind.report_len()
            ),
        }
    }
}

impl std::error::Error for HelperFrameError {}

impl HelperFrame {
    /// Encodes the frame.
    #[must_use]
    pub fn encode(kind: VirtualDeviceKind, report: &[u8]) -> Vec<u8> {
        let length = report.len().min(MAX_REPORT_LEN);
        let mut frame = Vec::with_capacity(2 + length);
        frame.push(kind as u8);
        frame.push(u8::try_from(length).unwrap_or(u8::MAX));
        frame.extend_from_slice(&report[..length]);
        frame
    }

    /// Checks a frame's two-byte header, returning the device and how many
    /// report bytes follow.
    ///
    /// # Errors
    ///
    /// Returns [`HelperFrameError`] for an unknown device or a length that is
    /// not that device's report length, which a helper treats as a broken
    /// peer and stops.
    pub fn header(header: [u8; 2]) -> Result<(VirtualDeviceKind, usize), HelperFrameError> {
        let kind = VirtualDeviceKind::from_byte(header[0])
            .ok_or(HelperFrameError::UnknownDevice(header[0]))?;
        let length = usize::from(header[1]);
        if length != kind.report_len() {
            return Err(HelperFrameError::WrongLength { kind, length });
        }
        Ok((kind, length))
    }
}

/// Report descriptor for a boot keyboard with no report id.
pub const KEYBOARD_DESCRIPTOR: &[u8] = &[
    0x05, 0x01, // Usage Page (Generic Desktop)
    0x09, 0x06, // Usage (Keyboard)
    0xA1, 0x01, // Collection (Application)
    0x05, 0x07, //   Usage Page (Keyboard/Keypad)
    0x19, 0xE0, //   Usage Minimum (Left Control)
    0x29, 0xE7, //   Usage Maximum (Right GUI)
    0x15, 0x00, //   Logical Minimum (0)
    0x25, 0x01, //   Logical Maximum (1)
    0x75, 0x01, //   Report Size (1)
    0x95, 0x08, //   Report Count (8)
    0x81, 0x02, //   Input (Data, Variable, Absolute) — modifiers
    0x95, 0x01, //   Report Count (1)
    0x75, 0x08, //   Report Size (8)
    0x81, 0x01, //   Input (Constant) — reserved
    0x95, 0x06, //   Report Count (6)
    0x75, 0x08, //   Report Size (8)
    0x15, 0x00, //   Logical Minimum (0)
    0x26, 0xFF, 0x00, // Logical Maximum (255)
    0x05, 0x07, //   Usage Page (Keyboard/Keypad)
    0x19, 0x00, //   Usage Minimum (0)
    0x29, 0xFF, //   Usage Maximum (255)
    0x81, 0x00, //   Input (Data, Array) — key slots
    0xC0, // End Collection
];

/// Length of one keyboard input report.
pub const KEYBOARD_REPORT_LEN: usize = 8;

/// First HID usage that is a modifier (Left Control).
const FIRST_MODIFIER_USAGE: u8 = 0xE0;
/// Last HID usage that is a modifier (Right GUI).
const LAST_MODIFIER_USAGE: u8 = 0xE7;
/// Key slots in a boot keyboard report.
const KEY_SLOTS: usize = 6;

/// A key could not be added to the report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyboardReportError {
    /// Six non-modifier keys are already held.
    RolloverExceeded,
    /// Usage zero is "no event", not a key.
    ReservedUsage,
}

impl std::fmt::Display for KeyboardReportError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::RolloverExceeded => "six keys are already held; a boot keyboard carries no more",
            Self::ReservedUsage => "HID usage 0 is not a key",
        })
    }
}

impl std::error::Error for KeyboardReportError {}

/// What a virtual keyboard is holding.
///
/// Keys are kept in the order they were pressed, which is the order a real
/// keyboard reports them and what makes auto-repeat pick the newest key.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeyboardState {
    modifiers: u8,
    keys: Vec<u8>,
}

impl KeyboardState {
    /// An idle keyboard.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a press or release of a HID keyboard usage.
    ///
    /// Returns whether the report changed. Releasing a key that is not held,
    /// or pressing one that already is, changes nothing and is not an error:
    /// a remote peer's repeated or lost transitions must converge, not fail.
    ///
    /// # Errors
    ///
    /// Returns [`KeyboardReportError`] for usage zero, or for a seventh
    /// non-modifier key.
    pub fn set(&mut self, usage: u8, pressed: bool) -> Result<bool, KeyboardReportError> {
        if usage == 0 {
            return Err(KeyboardReportError::ReservedUsage);
        }
        if (FIRST_MODIFIER_USAGE..=LAST_MODIFIER_USAGE).contains(&usage) {
            let bit = 1_u8 << (usage - FIRST_MODIFIER_USAGE);
            let before = self.modifiers;
            if pressed {
                self.modifiers |= bit;
            } else {
                self.modifiers &= !bit;
            }
            return Ok(before != self.modifiers);
        }
        let held = self.keys.iter().position(|&key| key == usage);
        match (pressed, held) {
            (true, Some(_)) | (false, None) => Ok(false),
            (true, None) => {
                if self.keys.len() >= KEY_SLOTS {
                    return Err(KeyboardReportError::RolloverExceeded);
                }
                self.keys.push(usage);
                Ok(true)
            }
            (false, Some(index)) => {
                self.keys.remove(index);
                Ok(true)
            }
        }
    }

    /// Whether anything is held.
    #[must_use]
    pub fn is_idle(&self) -> bool {
        self.modifiers == 0 && self.keys.is_empty()
    }

    /// Releases everything.
    pub fn clear(&mut self) {
        self.modifiers = 0;
        self.keys.clear();
    }

    /// The input report for this state.
    #[must_use]
    pub fn report(&self) -> [u8; KEYBOARD_REPORT_LEN] {
        let mut report = [0_u8; KEYBOARD_REPORT_LEN];
        report[0] = self.modifiers;
        for (slot, key) in report[2..].iter_mut().zip(&self.keys) {
            *slot = *key;
        }
        report
    }
}

/// Report descriptor for an absolute pointer with no report id: five buttons,
/// X and Y over `0..=POINTER_AXIS_MAX`, a vertical wheel and a horizontal pan.
pub const POINTER_DESCRIPTOR: &[u8] = &[
    0x05, 0x01, // Usage Page (Generic Desktop)
    0x09, 0x02, // Usage (Mouse)
    0xA1, 0x01, // Collection (Application)
    0x09, 0x01, //   Usage (Pointer)
    0xA1, 0x00, //   Collection (Physical)
    0x05, 0x09, //     Usage Page (Button)
    0x19, 0x01, //     Usage Minimum (1)
    0x29, 0x05, //     Usage Maximum (5)
    0x15, 0x00, //     Logical Minimum (0)
    0x25, 0x01, //     Logical Maximum (1)
    0x95, 0x05, //     Report Count (5)
    0x75, 0x01, //     Report Size (1)
    0x81, 0x02, //     Input (Data, Variable, Absolute) — buttons
    0x95, 0x01, //     Report Count (1)
    0x75, 0x03, //     Report Size (3)
    0x81, 0x01, //     Input (Constant) — padding
    0x05, 0x01, //     Usage Page (Generic Desktop)
    0x09, 0x30, //     Usage (X)
    0x09, 0x31, //     Usage (Y)
    0x15, 0x00, //     Logical Minimum (0)
    0x26, 0xFF, 0x7F, // Logical Maximum (32767)
    0x75, 0x10, //     Report Size (16)
    0x95, 0x02, //     Report Count (2)
    0x81, 0x02, //     Input (Data, Variable, Absolute) — position
    0x09, 0x38, //     Usage (Wheel)
    0x15, 0x81, //     Logical Minimum (-127)
    0x25, 0x7F, //     Logical Maximum (127)
    0x75, 0x08, //     Report Size (8)
    0x95, 0x01, //     Report Count (1)
    0x81, 0x06, //     Input (Data, Variable, Relative) — wheel
    0x05, 0x0C, //     Usage Page (Consumer)
    0x0A, 0x38, 0x02, // Usage (AC Pan)
    0x15, 0x81, //     Logical Minimum (-127)
    0x25, 0x7F, //     Logical Maximum (127)
    0x75, 0x08, //     Report Size (8)
    0x95, 0x01, //     Report Count (1)
    0x81, 0x06, //     Input (Data, Variable, Relative) — pan
    0xC0, //   End Collection
    0xC0, // End Collection
];

/// Length of one pointer input report: buttons, X, Y, wheel, pan.
pub const POINTER_REPORT_LEN: usize = 7;

/// The largest value either pointer axis reports.
pub const POINTER_AXIS_MAX: u16 = 0x7FFF;

/// What an absolute pointer is holding, and where it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PointerState {
    buttons: u8,
    x: u16,
    y: u16,
}

impl Default for PointerState {
    fn default() -> Self {
        Self::new()
    }
}

impl PointerState {
    /// A pointer in the middle of the desktop with nothing held.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            buttons: 0,
            x: POINTER_AXIS_MAX / 2,
            y: POINTER_AXIS_MAX / 2,
        }
    }

    /// Places the pointer at a normalized position, `0.0..=1.0` across the
    /// desktop on each axis. Positions outside it are clamped to its edge and
    /// a non-finite one leaves that axis where it was, so a bad sample can
    /// never throw the cursor somewhere it was not asked to go.
    pub fn move_to(&mut self, x: f64, y: f64) {
        if let Some(x) = axis(x) {
            self.x = x;
        }
        if let Some(y) = axis(y) {
            self.y = y;
        }
    }

    /// Presses or releases a button, numbered as on the wire: 1 left, 2
    /// middle, 3 right, 4 back, 5 forward. Returns whether anything changed;
    /// a button this device does not carry changes nothing.
    pub fn set_button(&mut self, button: u8, pressed: bool) -> bool {
        let bit = match button {
            1 => 0,
            3 => 1,
            2 => 2,
            4 => 3,
            5 => 4,
            _ => return false,
        };
        let before = self.buttons;
        if pressed {
            self.buttons |= 1 << bit;
        } else {
            self.buttons &= !(1 << bit);
        }
        before != self.buttons
    }

    /// Whether any button is held.
    #[must_use]
    pub const fn has_buttons(&self) -> bool {
        self.buttons != 0
    }

    /// Releases every button, keeping the position.
    pub fn clear_buttons(&mut self) {
        self.buttons = 0;
    }

    /// The report for this state, carrying one wheel and pan movement in
    /// detents. Positive wheel scrolls up and positive pan scrolls right, as
    /// the HID specification defines them.
    #[must_use]
    pub fn report(&self, wheel: i8, pan: i8) -> [u8; POINTER_REPORT_LEN] {
        let [x_low, x_high] = self.x.to_le_bytes();
        let [y_low, y_high] = self.y.to_le_bytes();
        [
            self.buttons,
            x_low,
            x_high,
            y_low,
            y_high,
            wheel.to_le_bytes()[0],
            pan.to_le_bytes()[0],
        ]
    }
}

fn axis(value: f64) -> Option<u16> {
    if !value.is_finite() {
        return None;
    }
    let scaled = (value.clamp(0.0, 1.0) * f64::from(POINTER_AXIS_MAX)).round();
    // The clamp keeps the value inside the axis, so the cast is exact.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Some(scaled as u16)
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: u8 = 0x04;
    const B: u8 = 0x05;
    const LEFT_SHIFT: u8 = 0xE1;
    const LEFT_GUI: u8 = 0xE3;

    #[test]
    fn an_idle_keyboard_reports_nothing_held() {
        let state = KeyboardState::new();
        assert!(state.is_idle());
        assert_eq!(state.report(), [0; KEYBOARD_REPORT_LEN]);
    }

    #[test]
    fn modifiers_are_bits_and_keys_are_slots_in_press_order() {
        let mut state = KeyboardState::new();
        assert_eq!(state.set(LEFT_SHIFT, true), Ok(true));
        assert_eq!(state.set(B, true), Ok(true));
        assert_eq!(state.set(A, true), Ok(true));
        assert_eq!(state.report(), [0x02, 0, B, A, 0, 0, 0, 0]);
        assert_eq!(state.set(LEFT_GUI, true), Ok(true));
        assert_eq!(state.report()[0], 0x0A);
        assert_eq!(state.set(B, false), Ok(true));
        assert_eq!(state.report(), [0x0A, 0, A, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn repeated_and_lost_transitions_converge() {
        let mut state = KeyboardState::new();
        assert_eq!(state.set(A, false), Ok(false), "releasing an unheld key");
        assert_eq!(state.set(A, true), Ok(true));
        assert_eq!(state.set(A, true), Ok(false), "a repeat is not a new key");
        assert_eq!(state.set(LEFT_SHIFT, false), Ok(false));
        assert_eq!(state.report(), [0, 0, A, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn a_seventh_key_is_refused_not_dropped() {
        let mut state = KeyboardState::new();
        for usage in 0x04..0x0A {
            assert_eq!(state.set(usage, true), Ok(true));
        }
        assert_eq!(
            state.set(0x0A, true),
            Err(KeyboardReportError::RolloverExceeded)
        );
        // Modifiers still fit: they are bits, not slots.
        assert_eq!(state.set(LEFT_SHIFT, true), Ok(true));
        assert_eq!(state.set(0, true), Err(KeyboardReportError::ReservedUsage));
    }

    #[test]
    fn clearing_releases_everything() {
        let mut state = KeyboardState::new();
        state.set(LEFT_SHIFT, true).expect("shift");
        state.set(A, true).expect("a");
        state.clear();
        assert!(state.is_idle());
        assert_eq!(state.report(), [0; KEYBOARD_REPORT_LEN]);
    }

    #[test]
    fn a_helper_frame_round_trips_and_refuses_what_it_cannot_carry() {
        let frame = HelperFrame::encode(VirtualDeviceKind::Keyboard, &[0x02, 0, A, 0, 0, 0, 0, 0]);
        assert_eq!(frame[..2], [1, 8]);
        assert_eq!(
            HelperFrame::header([frame[0], frame[1]]),
            Ok((VirtualDeviceKind::Keyboard, 8))
        );
        assert_eq!(
            HelperFrame::header([9, 8]),
            Err(HelperFrameError::UnknownDevice(9))
        );
        assert!(matches!(
            HelperFrame::header([1, 7]),
            Err(HelperFrameError::WrongLength { length: 7, .. })
        ));
        for kind in VirtualDeviceKind::ALL {
            assert_eq!(kind.released(&[]).len(), kind.report_len());
            assert!(kind.released(&[]).iter().all(|&byte| byte == 0));
        }
    }

    #[test]
    fn the_descriptor_declares_an_eight_byte_boot_keyboard() {
        assert_eq!(&KEYBOARD_DESCRIPTOR[..4], &[0x05, 0x01, 0x09, 0x06]);
        assert_eq!(KEYBOARD_DESCRIPTOR.last(), Some(&0xC0));
        // 8 modifier bits + 8 reserved bits + 6 slots of 8 bits = 64 bits.
        assert_eq!(KEYBOARD_REPORT_LEN * 8, 8 + 8 + 6 * 8);
    }

    #[test]
    fn the_pointer_descriptor_declares_a_seven_byte_absolute_pointer() {
        assert_eq!(&POINTER_DESCRIPTOR[..4], &[0x05, 0x01, 0x09, 0x02]);
        assert_eq!(
            &POINTER_DESCRIPTOR[POINTER_DESCRIPTOR.len() - 2..],
            &[0xC0, 0xC0]
        );
        // 5 buttons + 3 padding + two 16-bit axes + wheel + pan.
        assert_eq!(POINTER_REPORT_LEN * 8, 5 + 3 + 2 * 16 + 8 + 8);
        assert_eq!(
            HelperFrame::header([2, 7]),
            Ok((VirtualDeviceKind::Pointer, 7))
        );
    }

    #[test]
    fn a_pointer_reports_where_it_is_exactly_and_clamps_to_the_desktop() {
        let mut pointer = PointerState::new();
        pointer.move_to(0.0, 1.0);
        assert_eq!(pointer.report(0, 0), [0, 0, 0, 0xFF, 0x7F, 0, 0]);
        pointer.move_to(0.5, 0.25);
        let report = pointer.report(0, 0);
        assert_eq!(u16::from_le_bytes([report[1], report[2]]), 16_384);
        assert_eq!(u16::from_le_bytes([report[3], report[4]]), 8_192);
        pointer.move_to(-3.0, 7.0);
        assert_eq!(pointer.report(0, 0)[1..5], [0, 0, 0xFF, 0x7F]);
        pointer.move_to(f64::NAN, 0.0);
        assert_eq!(
            pointer.report(0, 0)[1..5],
            [0, 0, 0, 0],
            "NaN leaves X alone"
        );
    }

    #[test]
    fn pointer_buttons_follow_wire_numbering_and_scroll_is_signed() {
        let mut pointer = PointerState::new();
        assert!(pointer.set_button(1, true));
        assert!(!pointer.set_button(1, true), "a repeat changes nothing");
        assert!(pointer.set_button(3, true));
        assert!(pointer.set_button(2, true));
        assert_eq!(pointer.report(0, 0)[0], 0b111);
        assert!(!pointer.set_button(9, true), "no ninth button to press");
        assert_eq!(pointer.report(-1, 2)[5..], [0xFF, 2]);
        pointer.clear_buttons();
        assert!(!pointer.has_buttons());
    }

    #[test]
    fn a_released_pointer_keeps_its_place_and_lets_go() {
        let mut pointer = PointerState::new();
        pointer.move_to(0.75, 0.5);
        pointer.set_button(1, true);
        let held = pointer.report(3, 0);
        let released = VirtualDeviceKind::Pointer.released(&held);
        assert_eq!(released[0], 0, "no button");
        assert_eq!(released[1..5], held[1..5], "same place");
        assert_eq!(released[5..], [0, 0], "no scroll replayed");
        assert!(
            VirtualDeviceKind::Keyboard
                .released(&[2, 0, 4, 0, 0, 0, 0, 0])
                .iter()
                .all(|&byte| byte == 0)
        );
    }
}
