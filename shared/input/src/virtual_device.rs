//! Session-scoped identity of a host's virtual input devices.
//!
//! A Linux Pier injects into its dedicated X server through kernel virtual
//! devices. The kernel and udev give every such device to whichever X server or
//! compositor runs on the same seat. Without an identity that names the
//! session, a dedicated X server also adds the console's own keyboard and mice
//! and another session's virtual devices. Each device name therefore carries
//! the session token, and the session's X server is configured to admit only
//! devices that carry its token.
//!
//! This module owns the naming, the X server input classes that match it,
//! and the libinput quirks that tune it. It performs no device or file
//! operations.

use std::error::Error;
use std::fmt::{Display, Formatter};

/// USB-style vendor identifier every Arcen virtual input device reports.
pub const VIRTUAL_INPUT_VENDOR_ID: u16 = 0xA2CE;

/// Longest device name the Linux uinput interface accepts, in bytes.
pub const MAX_DEVICE_NAME_BYTES: usize = 79;

/// X server input class that ignores every device it matches.
pub const FOREIGN_INPUT_CLASS: &str = "arcen-ignore-foreign-input";

/// X server input class that re-admits this session's own devices.
pub const SESSION_INPUT_CLASS: &str = "arcen-session-input";

/// File name of the libinput quirks for Arcen's virtual devices. It sorts
/// after the distribution's own quirks files (`10-` to `50-`), so its values
/// win over theirs, and before an administrator's local overrides.
pub const LIBINPUT_QUIRKS_FILE_NAME: &str = "90-arcen-virtual-input.quirks";

/// Device pressure at and above which libinput puts the virtual tablet pen
/// in contact, set by [`libinput_quirks`] (`AttrPressureRange`).
pub const PRESSURE_CONTACT_UPPER: i32 = 2;

/// Device pressure at and below which libinput takes the virtual tablet pen
/// out of contact, set by [`libinput_quirks`].
pub const PRESSURE_CONTACT_LOWER: i32 = 1;

/// libinput quirks for Arcen's virtual devices.
///
/// libinput smooths a tablet tool's position and tilt by averaging its last
/// four samples unless a quirk turns that off (`AttrTabletSmoothing`, libinput
/// 1.19 and later). The virtual pen replays samples the client's own tablet
/// stack already filtered, so averaging them again only delays the pen by
/// about one and a half samples against the mouse, which libinput never
/// smooths.
///
/// A tablet with a pressure axis gets its contact from pressure, not from
/// `BTN_TOUCH`: libinput enters contact at 5 % of the range and leaves it at
/// 1 % unless `AttrPressureRange` sets other thresholds (`evdev-tablet.c`,
/// libinput 1.19 to 1.31). The client already decided contact, so the
/// thresholds sit at the bottom of the range ([`PRESSURE_CONTACT_UPPER`],
/// [`PRESSURE_CONTACT_LOWER`]) and the host holds a touching sample at or above
/// the upper one ([`contact_pressure`]).
#[must_use]
pub fn libinput_quirks() -> String {
    format!(
        concat!(
            "# Arcen virtual tablet pen: samples arrive already filtered by the\n",
            "# client's tablet driver; libinput must not average them again, and\n",
            "# the client decided contact, so the pressure thresholds are minimal.\n",
            "[Arcen Virtual Tablet Pen]\n",
            "MatchUdevType=tablet\n",
            "MatchVendor=0x{vendor:04X}\n",
            "MatchProduct=0x{product:04X}\n",
            "AttrTabletSmoothing=0\n",
            "AttrPressureRange={upper}:{lower}\n"
        ),
        vendor = VIRTUAL_INPUT_VENDOR_ID,
        product = VirtualInputRole::TabletPen.product_id(),
        upper = PRESSURE_CONTACT_UPPER,
        lower = PRESSURE_CONTACT_LOWER,
    )
}

/// The smallest pressure, on a `0..=maximum` axis, that libinput's default
/// contact threshold (5 % of the range, `axis_range_percentage`) admits, with
/// one unit of margin. For a virtual tablet whose libinput reads no Arcen
/// quirk (a compositor's own libinput).
#[must_use]
pub const fn libinput_default_contact_pressure(maximum: i32) -> i32 {
    // ceil(5 % of maximum) + 1, without overflow for any non-negative i32.
    let threshold = maximum / 20 + (maximum % 20 != 0) as i32 + 1;
    if threshold > maximum {
        maximum
    } else {
        threshold
    }
}

/// The pressure a virtual tablet reports for one sample: `0` out of contact,
/// so libinput never invents a contact, and at least `contact_floor` in
/// contact, so a touch the client reported with little or no pressure (a tap,
/// the start of a light stroke) still crosses libinput's contact threshold.
#[must_use]
pub const fn contact_pressure(pressure: i32, touching: bool, contact_floor: i32) -> i32 {
    if !touching {
        0
    } else if pressure < contact_floor {
        contact_floor
    } else {
        pressure
    }
}

/// Resolution, in units per radian, of a virtual tablet's tilt axes whose
/// unit is one degree (`180 / π`, rounded).
///
/// The kernel defines a tilt axis' resolution in units per radian. libinput
/// reads a tilt axis that declares none as a Wacom pen's physical ±64° spread
/// over the axis' range (`adjust_tilt` in `evdev-tablet.c`, libinput 1.19 to
/// 1.31), so a `-90..=90` degree axis reached applications scaled by 64/90: a
/// 45° tilt read 32° in Chrome on Xorg and in every Wayland client. With this
/// resolution libinput converts the units itself, within 0.6 %.
pub const TILT_UNITS_PER_RADIAN: i32 = 57;

/// The tilt, in degrees, libinput reports for `value` on a tilt axis spanning
/// `min..=max` with `resolution` units per radian (`adjust_tilt`, libinput
/// 1.19 to 1.31): the physical angle when the axis declares a resolution and
/// is centred on zero, otherwise the range mapped onto ±64°.
#[must_use]
pub fn libinput_tilt_degrees(value: i32, min: i32, max: i32, resolution: i32) -> f64 {
    const WACOM_MAX_DEGREES: f64 = 64.0;
    if resolution != 0 && max > 0 && min < 0 {
        return 180.0 / std::f64::consts::PI * f64::from(value) / f64::from(resolution);
    }
    let range = f64::from(max) - f64::from(min);
    if range <= 0.0 {
        return 0.0;
    }
    ((f64::from(value) - f64::from(min)) / range * 2.0 - 1.0) * WACOM_MAX_DEGREES
}

/// Disarms libinput's forced proximity-out for a virtual tablet.
///
/// libinput treats every tablet as one that may never report leaving
/// proximity: while its pen is in proximity and not touching, 50 ms without
/// an event make libinput report it gone (`FORCED_PROXOUT_TIMEOUT`, in
/// `evdev-tablet.c` up to 1.2x and `libinput-plugin-tablet-proximity-timer.c`
/// from 1.30; seen in 1.19, 1.30 and 1.31). It stops doing so for a device
/// once that device reports a genuine release of the pen tool
/// (`BTN_TOOL_PEN`), so the prelude is always the pen, whatever tool
/// approaches first. A pen
/// held still above the client's tablet sends nothing, so before that
/// release libinput declared it gone after 50 ms, and a jump to a new point
/// arrived only as a fresh proximity-in, with no motion an application sees.
///
/// The first time a tool enters proximity on a device, a host injects one
/// genuine pen proximity-in and -out ([`Self::needs_prelude`]) before the real
/// sample. Applications see one extra, instant in/out at the first approach.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProximityTimeoutDisarm {
    done: bool,
}

impl ProximityTimeoutDisarm {
    /// Whether to inject the in/out prelude before a sample that has the tool
    /// `entering` proximity (`was_in_proximity` is the device's state before
    /// it). True at most once per device.
    pub fn needs_prelude(&mut self, was_in_proximity: bool, entering: bool) -> bool {
        if self.done || was_in_proximity || !entering {
            return false;
        }
        self.done = true;
        true
    }
}

const LOWEST_DISPLAY: u8 = 1;
const HIGHEST_DISPLAY: u8 = 99;

/// One of the virtual devices a session's input is injected through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VirtualInputRole {
    /// Keyboard plus absolute pointer and scroll wheels.
    Absolute,
    /// Relative pointer for pointer-lock motion.
    Relative,
    /// Tablet tool carrying pressure and tilt.
    TabletPen,
}

impl VirtualInputRole {
    /// Every role, in device-creation order.
    pub const ALL: [Self; 3] = [Self::Absolute, Self::Relative, Self::TabletPen];

    /// Product identifier paired with [`VIRTUAL_INPUT_VENDOR_ID`].
    #[must_use]
    pub const fn product_id(self) -> u16 {
        match self {
            Self::Absolute => 0x0001,
            Self::Relative => 0x0002,
            Self::TabletPen => 0x0003,
        }
    }

    /// Device name without the session token.
    #[must_use]
    pub const fn base_name(self) -> &'static str {
        match self {
            Self::Absolute => "Arcen Virtual Absolute Input",
            Self::Relative => "Arcen Virtual Relative Pointer",
            Self::TabletPen => "Arcen Virtual Tablet Pen",
        }
    }
}

/// Why a display cannot identify a session's virtual input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionInputIdentityError {
    /// The display is not `:N` with `N` in `1..=99`.
    InvalidDisplay(String),
}

impl Display for SessionInputIdentityError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidDisplay(display) => write!(
                formatter,
                "display {display:?} is not a dedicated X display :{LOWEST_DISPLAY}..:{HIGHEST_DISPLAY}"
            ),
        }
    }
}

impl Error for SessionInputIdentityError {}

/// The identity shared by one session's virtual devices and its X server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SessionInputIdentity {
    display: u8,
}

impl SessionInputIdentity {
    /// Identity of the session whose dedicated X server runs on `display`.
    ///
    /// # Errors
    ///
    /// Returns an error unless `display` is exactly `:N` with `N` in
    /// `1..=99` and no leading zero, screen suffix or host part.
    pub fn for_x_display(display: &str) -> Result<Self, SessionInputIdentityError> {
        let invalid = || SessionInputIdentityError::InvalidDisplay(display.to_owned());
        let digits = display.strip_prefix(':').ok_or_else(invalid)?;
        if digits.is_empty()
            || digits.len() > 2
            || digits.starts_with('0')
            || !digits.bytes().all(|byte| byte.is_ascii_digit())
        {
            return Err(invalid());
        }
        let number = digits.parse::<u8>().map_err(|_| invalid())?;
        if !(LOWEST_DISPLAY..=HIGHEST_DISPLAY).contains(&number) {
            return Err(invalid());
        }
        Ok(Self { display: number })
    }

    /// The dedicated X display number.
    #[must_use]
    pub const fn display_number(self) -> u8 {
        self.display
    }

    /// Token every device of this session carries in its name.
    ///
    /// The closing parenthesis keeps `:6` from matching `:61`.
    #[must_use]
    pub fn session_token(self) -> String {
        format!("(session :{})", self.display)
    }

    /// Full device name for `role`, within [`MAX_DEVICE_NAME_BYTES`].
    #[must_use]
    pub fn device_name(self, role: VirtualInputRole) -> String {
        format!("{} {}", role.base_name(), self.session_token())
    }

    /// X server input classes that admit only this session's devices.
    ///
    /// The first class ignores every input device: the console keyboard and
    /// mice, other sessions' virtual devices, and anything hot-plugged later.
    /// The second re-admits devices that carry both this session's vendor
    /// identifier and its token. The X server applies the last matching
    /// class that sets `Ignore`, so the order matters.
    #[must_use]
    pub fn xorg_input_classes(self) -> String {
        format!(
            concat!(
                "Section \"InputClass\"\n",
                "    Identifier     \"{foreign}\"\n",
                "    Option         \"Ignore\" \"on\"\n",
                "EndSection\n",
                "\n",
                "Section \"InputClass\"\n",
                "    Identifier     \"{session}\"\n",
                "    MatchUSBID     \"{vendor:04x}:*\"\n",
                "    MatchProduct   \"{token}\"\n",
                "    Option         \"Ignore\" \"off\"\n",
                "EndSection\n"
            ),
            foreign = FOREIGN_INPUT_CLASS,
            session = SESSION_INPUT_CLASS,
            vendor = VIRTUAL_INPUT_VENDOR_ID,
            token = self.session_token(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn libinput_quirks_turn_off_smoothing_only_for_the_virtual_pen() {
        let quirks = libinput_quirks();
        assert!(LIBINPUT_QUIRKS_FILE_NAME.ends_with(".quirks"));
        assert!(quirks.contains("MatchUdevType=tablet\n"));
        assert!(quirks.contains("MatchVendor=0xA2CE\n"));
        assert!(quirks.contains("MatchProduct=0x0003\n"));
        assert!(quirks.contains("AttrTabletSmoothing=0\n"));
        assert!(quirks.contains("AttrPressureRange=2:1\n"));
        // One section: a parse error in any quirks file makes libinput drop
        // every quirk, so the format stays minimal.
        assert_eq!(quirks.matches('[').count(), 1);
        for line in quirks.lines() {
            assert!(
                line.is_empty()
                    || line.starts_with('#')
                    || (line.starts_with('[') && line.ends_with(']'))
                    || line
                        .split_once('=')
                        .is_some_and(|(k, v)| !k.is_empty() && !v.is_empty()),
                "{line}"
            );
        }
    }

    #[test]
    fn a_touch_always_crosses_libinputs_contact_threshold_and_a_hover_never_does() {
        // Quirked thresholds: the upper one must lie above the lower one.
        assert!(PRESSURE_CONTACT_LOWER < PRESSURE_CONTACT_UPPER);
        // A zero-pressure tip down and the first, lightest samples of a stroke.
        for floor in [
            PRESSURE_CONTACT_UPPER,
            libinput_default_contact_pressure(65_535),
        ] {
            for pressure in [0, 1, floor - 1] {
                assert_eq!(contact_pressure(pressure, true, floor), floor);
            }
            assert_eq!(contact_pressure(40_000, true, floor), 40_000);
            assert_eq!(contact_pressure(40_000, false, floor), 0);
            assert_eq!(contact_pressure(0, false, floor), 0);
        }
        // libinput 1.31: hi = (max - min) * 5 / 100.0 + min, truncated.
        let libinput_hi = (65_535.0_f64 * 5.0 / 100.0) as i32;
        assert!(libinput_default_contact_pressure(65_535) > libinput_hi);
        assert_eq!(libinput_default_contact_pressure(65_535), 3_278);
        assert_eq!(libinput_default_contact_pressure(10), 2);
    }

    #[test]
    fn a_degree_tilt_axis_reaches_libinput_clients_as_degrees() {
        // Without a resolution libinput spreads -90..=90 over ±64°: 45° read 32°.
        let unresolved = libinput_tilt_degrees(45, -90, 90, 0);
        assert!((unresolved - 32.0).abs() < 1e-9, "{unresolved}");
        for degrees in [-90, -45, -17, -1, 0, 1, 23, 45, 89, 90] {
            let reported = libinput_tilt_degrees(degrees, -90, 90, TILT_UNITS_PER_RADIAN);
            assert!(
                (reported - f64::from(degrees)).abs() <= 0.006 * f64::from(degrees).abs() + 1e-9,
                "{degrees}° reported as {reported}°"
            );
            assert_eq!(reported.round() as i32, degrees, "{degrees}°");
        }
    }

    #[test]
    fn the_proximity_timeout_is_disarmed_once_at_the_first_approach() {
        let mut disarm = ProximityTimeoutDisarm::default();
        assert!(!disarm.needs_prelude(false, false), "not entering");
        assert!(disarm.needs_prelude(false, true));
        assert!(!disarm.needs_prelude(true, true), "already in proximity");
        assert!(!disarm.needs_prelude(false, true), "once per device");
    }

    #[test]
    fn accepts_only_dedicated_displays() {
        for valid in [":1", ":9", ":10", ":58", ":99"] {
            assert!(
                SessionInputIdentity::for_x_display(valid).is_ok(),
                "{valid}"
            );
        }
        for invalid in [
            "", ":", ":0", ":00", ":05", ":100", ":1.0", "host:1", "1", ": 1", ":-1", ":+1", ":a",
            ":1a",
        ] {
            assert!(
                SessionInputIdentity::for_x_display(invalid).is_err(),
                "{invalid}"
            );
        }
    }

    #[test]
    fn device_names_carry_a_token_no_other_display_matches() {
        let six = SessionInputIdentity::for_x_display(":6").unwrap();
        let sixty_one = SessionInputIdentity::for_x_display(":61").unwrap();
        for role in VirtualInputRole::ALL {
            let name = sixty_one.device_name(role);
            assert!(name.starts_with(role.base_name()));
            assert!(name.contains(&sixty_one.session_token()));
            assert!(!name.contains(&six.session_token()));
            assert!(!six.device_name(role).contains(&sixty_one.session_token()));
        }
    }

    #[test]
    fn every_device_name_fits_uinput() {
        let widest = SessionInputIdentity::for_x_display(":99").unwrap();
        for role in VirtualInputRole::ALL {
            let name = widest.device_name(role);
            assert!(name.len() <= MAX_DEVICE_NAME_BYTES, "{name}");
            assert!(name.is_ascii());
            assert!(!name.contains('"'));
        }
    }

    #[test]
    fn product_identifiers_are_unchanged() {
        assert_eq!(
            SessionInputIdentity::for_x_display(":61")
                .unwrap()
                .display_number(),
            61
        );
        assert_eq!(VIRTUAL_INPUT_VENDOR_ID, 0xA2CE);
        assert_eq!(VirtualInputRole::Absolute.product_id(), 1);
        assert_eq!(VirtualInputRole::Relative.product_id(), 2);
        assert_eq!(VirtualInputRole::TabletPen.product_id(), 3);
    }

    #[test]
    fn input_classes_ignore_everything_then_admit_the_session() {
        let identity = SessionInputIdentity::for_x_display(":58").unwrap();
        let classes = identity.xorg_input_classes();
        let foreign = classes.find(FOREIGN_INPUT_CLASS).unwrap();
        let session = classes.find(SESSION_INPUT_CLASS).unwrap();
        assert!(foreign < session);
        assert_eq!(classes.matches("Section \"InputClass\"").count(), 2);
        assert_eq!(classes.matches("EndSection").count(), 2);
        assert!(classes.contains("MatchUSBID     \"a2ce:*\""));
        assert!(classes.contains("MatchProduct   \"(session :58)\""));
        let (foreign_section, session_section) = classes.split_at(session);
        assert!(foreign_section.contains("\"Ignore\" \"on\""));
        assert!(!foreign_section.contains("Match"));
        assert!(session_section.contains("\"Ignore\" \"off\""));
    }
}
