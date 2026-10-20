//! A real pen in a headless Wayland session through a private udev seat
//! (lab).
//!
//! A headless GNOME compositor normally opens no input devices, and no
//! released Mutter offers a stylus on its emulated-input (EIS) seat. With
//! GNOME/mutter!5224 (headless libinput, open upstream) plus an Arcen seat
//! selector, a headless compositor runs libinput on one named udev seat. The
//! Pier then gives the session a virtual tablet on that seat, and the
//! compositor sees it as a real tablet: pressure, tilt and buttons reach
//! applications through `zwp_tablet_v2` and Xwayland, exactly as a USB
//! tablet would.
//!
//! Every desktop gets its own tablet on its own seat ([`DesktopPen`]): a
//! unique device name, a seat named after the desktop, and
//! a udev rule that matches only that virtual device. No seat0 consumer (a
//! display manager, a console X server) and no other desktop ever sees it,
//! and only the desktop's own user may open its event node, for as long as
//! the desktop lives. This module is the pure part: names, the udev rule,
//! the access plan and the raster.

/// Every desktop's seat starts with this; never `seat0`.
pub const SEAT_PREFIX: &str = "seat-arcen-pen-";

/// Every desktop's tablet name starts with this.
pub const DEVICE_NAME_PREFIX: &str = "Arcen Seat Pen ";

/// USB-style vendor id of Arcen's virtual input devices.
pub const VENDOR: u16 = 0xa2ce;

/// Product id of the seat pen (distinct from the X11 session devices).
pub const PRODUCT: u16 = 0x0010;

/// `BUS_VIRTUAL`: the bus a uinput device reports. A physical (USB,
/// Bluetooth) device can copy a name but not this bus and a virtual devpath.
pub const BUS_VIRTUAL: u16 = 0x06;

/// Where every uinput device lives in sysfs.
pub const VIRTUAL_INPUT_DEVPATH: &str = "/devices/virtual/input/";

/// The environment variable that names the udev seat a lab-patched headless
/// Mutter runs libinput on; unset, it runs none (stock behaviour).
pub const COMPOSITOR_SEAT_ENV: &str = "ARCEN_LAB_HEADLESS_INPUT_SEAT";

/// The tablet's absolute raster on both axes. The compositor maps the whole
/// raster onto the stage, so the raster sets precision, not geometry.
pub const RASTER: u32 = 32_768;

/// Raster units per millimetre that the device reports. libinput refuses
/// tablets without a resolution.
pub const RESOLUTION_PER_MM: i32 = 100;

/// Where the Pier installs the rules. `/run` is volatile, so a reboot always
/// removes them.
pub const UDEV_RULE_DIR: &str = "/run/udev/rules.d";

/// Every seat-pen rule file name starts with this (the single process-wide
/// rule of the first lab build used `72-arcen-lab-seat-pen.rules`).
pub const UDEV_RULE_PREFIX: &str = "72-arcen-lab-seat-pen";

const ID_MIN: usize = 8;
const ID_MAX: usize = 32;

/// Whether `name` is a file in [`UDEV_RULE_DIR`] that a seat pen installed,
/// so a Pier start sweeps it (the Pier that wrote it is gone).
#[must_use]
pub fn is_rule_file(name: &str) -> bool {
    name.strip_prefix(UDEV_RULE_PREFIX)
        .and_then(|rest| rest.strip_suffix(".rules"))
        .is_some_and(|middle| middle.is_empty() || middle.strip_prefix('-').is_some_and(valid_id))
}

/// Whether `seat` is a desktop seat-pen seat ([`DesktopPen::seat`]).
#[must_use]
pub fn is_desktop_seat(seat: &str) -> bool {
    seat.strip_prefix(SEAT_PREFIX).is_some_and(valid_id)
}

fn valid_id(id: &str) -> bool {
    (ID_MIN..=ID_MAX).contains(&id.len())
        && id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// What udev knows about an input device, as far as the seat-pen rule looks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputDeviceFacts<'a> {
    pub devpath: &'a str,
    pub bustype: u16,
    pub vendor: u16,
    pub product: u16,
    pub name: &'a str,
}

/// One desktop's tablet: its seat, identity and udev rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopPen {
    id: String,
}

impl DesktopPen {
    /// A desktop's pen, from a random lowercase hex id of 8 to 32 digits;
    /// `None` for anything else.
    #[must_use]
    pub fn new(id: &str) -> Option<Self> {
        valid_id(id).then(|| Self { id: id.to_owned() })
    }

    /// A desktop's pen from 8 random bytes.
    #[must_use]
    pub fn from_random(bytes: [u8; 8]) -> Self {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut id = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            id.push(char::from(HEX[usize::from(byte >> 4)]));
            id.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
        Self { id }
    }

    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The udev seat (`ID_SEAT`) this desktop's tablet belongs to and its
    /// compositor runs libinput on.
    #[must_use]
    pub fn seat(&self) -> String {
        format!("{SEAT_PREFIX}{}", self.id)
    }

    /// The tablet's device name.
    #[must_use]
    pub fn device_name(&self) -> String {
        format!("{DEVICE_NAME_PREFIX}{}", self.id)
    }

    /// Where this desktop's rule is installed.
    #[must_use]
    pub fn rule_path(&self) -> String {
        format!("{UDEV_RULE_DIR}/{UDEV_RULE_PREFIX}-{}.rules", self.id)
    }

    /// The udev rule that puts this desktop's tablet, and nothing else, on
    /// [`Self::seat`]: a virtual device (bus 0006 under
    /// [`VIRTUAL_INPUT_DEVPATH`]) with this desktop's name: a random 64-bit
    /// id no other desktop and no physical device carries.
    /// It runs before systemd's `73-seat-late.rules` turns `ID_SEAT` into a
    /// tag. [`Self::assigns`] is its model.
    #[must_use]
    pub fn udev_rule(&self) -> String {
        let seat = self.seat();
        let name = self.device_name();
        let matches = |attr: &str| {
            format!(
                "{attr}{{id/bustype}}==\"{BUS_VIRTUAL:04x}\", {attr}{{id/vendor}}==\"{VENDOR:04x}\", \
                 {attr}{{id/product}}==\"{PRODUCT:04x}\", {attr}{{name}}==\"{name}\""
            )
        };
        format!(
            "# Arcen (lab): desktop {id}'s pen belongs to {seat}, never seat0.\n\
             ACTION==\"remove\", GOTO=\"arcen_seat_pen_end\"\n\
             SUBSYSTEM!=\"input\", GOTO=\"arcen_seat_pen_end\"\n\
             DEVPATH!=\"{VIRTUAL_INPUT_DEVPATH}*\", GOTO=\"arcen_seat_pen_end\"\n\
             KERNEL==\"input[0-9]*\", {own}, ENV{{ID_SEAT}}=\"{seat}\"\n\
             KERNEL!=\"input[0-9]*\", {parent}, ENV{{ID_SEAT}}=\"{seat}\"\n\
             LABEL=\"arcen_seat_pen_end\"\n",
            id = self.id,
            own = matches("ATTR"),
            parent = matches("ATTRS"),
        )
    }

    /// Whether [`Self::udev_rule`] assigns `device` to this desktop's seat.
    #[must_use]
    pub fn assigns(&self, device: &InputDeviceFacts<'_>) -> bool {
        device.devpath.starts_with(VIRTUAL_INPUT_DEVPATH)
            && device.bustype == BUS_VIRTUAL
            && device.vendor == VENDOR
            && device.product == PRODUCT
            && device.name == self.device_name()
    }

    /// The `setfacl` argument lists that give `uid`, and only `uid`, access
    /// to this desktop's event `node`: every extended entry is cleared first.
    #[must_use]
    pub fn grant_plan(node: &str, uid: u32) -> [Vec<String>; 2] {
        [
            Self::revoke_plan(node),
            vec!["-m".into(), format!("u:{uid}:rw"), node.into()],
        ]
    }

    /// The `setfacl` arguments that take every grant back from `node`.
    #[must_use]
    pub fn revoke_plan(node: &str) -> Vec<String> {
        vec!["-b".into(), node.into()]
    }

    /// The udev rule of a [native seat](NativeSeat): the compositor GPU's
    /// card node and the `vkms` card join this desktop's seat, then
    /// [`Self::udev_rule`] puts the desktop's tablet there too.
    #[must_use]
    pub fn native_udev_rule(&self, seat: &NativeSeat) -> String {
        let id = &self.id;
        let target = self.seat();
        let gpu = &seat.gpu_id_path;
        format!(
            "# Arcen (lab): desktop {id} runs a native compositor on {target}.\n\
             ACTION!=\"remove\", SUBSYSTEM==\"drm\", KERNEL==\"card[0-9]*\", ENV{{ID_PATH}}==\"{gpu}\", ENV{{ID_SEAT}}=\"{target}\"\n\
             ACTION!=\"remove\", SUBSYSTEM==\"drm\", KERNEL==\"card[0-9]*\", DEVPATH==\"{VKMS_DEVPATH_GLOB}\", ENV{{ID_SEAT}}=\"{target}\"\n\
             {pen}",
            pen = self.udev_rule(),
        )
    }

    /// The rule that takes the `vkms` card off this desktop's seat at
    /// teardown. logind reads the *current* `seat` and `master-of-seat`
    /// tags, so dropping both removes the device, and with its last master
    /// the seat (and any greeter a display manager put there), without the
    /// card ever joining `seat0`.
    #[must_use]
    pub fn native_release_rule(&self) -> String {
        let target = self.seat();
        format!(
            "# Arcen (lab): releasing {target}.\n\
             ACTION!=\"remove\", SUBSYSTEM==\"drm\", KERNEL==\"card[0-9]*\", DEVPATH==\"{VKMS_DEVPATH_GLOB}\", ENV{{ID_SEAT}}=\"{target}\", TAG-=\"master-of-seat\", TAG-=\"seat\"\n"
        )
    }
}

/// `DEVPATH` of the in-tree `vkms` driver's card nodes (`/devices/platform/`
/// on older kernels, `/devices/faux/` since 6.12).
pub const VKMS_DEVPATH_GLOB: &str = "*/vkms/*";

/// Marks the volatile override of the distribution's `61-mutter.rules` a
/// native seat installs (see [`mutter_rules_without_vkms_ignore`]).
pub const MUTTER_RULES_OVERRIDE_HEADER: &str =
    "# Arcen (lab) native seat: the distribution's 61-mutter.rules without its vkms ignore lines.";

/// The file name of Mutter's own udev rules, in `/usr/lib/udev/rules.d` and,
/// for the override, in [`UDEV_RULE_DIR`].
pub const MUTTER_RULES_FILE: &str = "61-mutter.rules";

/// A desktop whose compositor runs natively (not headless) on its own seat
/// (`wayland.session_mode`, see `arcen_session::native_seat`).
///
/// Stock Mutter keeps libinput only in native mode, and native mode needs a
/// logind session on a seat and at least one KMS device with a connector on
/// that seat (`meta-monitor-manager-native.c`, "No GPUs with outputs
/// found"). A vGPU has no connectors, so the seat gets two cards: the
/// in-tree `vkms` card as its display head and the compositor GPU's card
/// node for rendering. Arcen's outputs are `RecordVirtual` monitors; the
/// `vkms` monitor is switched off by the output placement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeSeat {
    gpu_id_path: String,
}

impl NativeSeat {
    /// A native seat rendering on the card at udev `ID_PATH` `pci-…`
    /// (`domain:bus:device.function`); `None` for anything else.
    #[must_use]
    pub fn new(gpu_id_path: &str) -> Option<Self> {
        valid_pci_id_path(gpu_id_path).then(|| Self {
            gpu_id_path: gpu_id_path.to_owned(),
        })
    }

    #[must_use]
    pub fn gpu_id_path(&self) -> &str {
        &self.gpu_id_path
    }
}

fn valid_pci_id_path(path: &str) -> bool {
    let Some(address) = path.strip_prefix("pci-") else {
        return false;
    };
    let hex = |part: &str, len: usize| {
        part.len() == len && part.bytes().all(|byte| byte.is_ascii_hexdigit())
    };
    let mut parts = address.split(':');
    let (Some(domain), Some(bus), Some(rest), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    let Some((device, function)) = rest.split_once('.') else {
        return false;
    };
    hex(domain, 4) && hex(bus, 2) && hex(device, 2) && function.len() == 1 && hex(function, 1)
}

/// The distribution's `61-mutter.rules` without the lines that tag `vkms`
/// cards `mutter-device-ignore`, headed by
/// [`MUTTER_RULES_OVERRIDE_HEADER`]. The tag is sticky (`TAG-=` only clears
/// the current tags, and Mutter reads all tags), so a native seat whose
/// head is `vkms` needs this override in [`UDEV_RULE_DIR`] for as long as
/// it lives.
#[must_use]
pub fn mutter_rules_without_vkms_ignore(distribution: &str) -> String {
    let mut out = String::with_capacity(distribution.len() + 128);
    out.push_str(MUTTER_RULES_OVERRIDE_HEADER);
    out.push('\n');
    for line in distribution.lines() {
        if line.contains("vkms") && line.contains("mutter-device-ignore") {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Whether `text` is an override [`mutter_rules_without_vkms_ignore`]
/// wrote, which a Pier start may remove.
#[must_use]
pub fn is_mutter_rules_override(text: &str) -> bool {
    text.starts_with(MUTTER_RULES_OVERRIDE_HEADER)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pen(id: &str) -> DesktopPen {
        DesktopPen::new(id).unwrap()
    }

    fn device(pen_name: &str) -> InputDeviceFacts<'_> {
        InputDeviceFacts {
            devpath: "/devices/virtual/input/input41",
            bustype: BUS_VIRTUAL,
            vendor: VENDOR,
            product: PRODUCT,
            name: pen_name,
        }
    }

    #[test]
    fn two_desktops_get_two_seats_two_devices_and_two_rules() {
        let a = pen("0123456789abcdef");
        let b = pen("fedcba9876543210");
        assert_ne!(a.seat(), b.seat());
        assert_ne!(a.device_name(), b.device_name());
        assert_ne!(a.rule_path(), b.rule_path());
        let (a_name, b_name) = (a.device_name(), b.device_name());
        assert!(a.assigns(&device(&a_name)));
        assert!(b.assigns(&device(&b_name)));
        assert!(!a.assigns(&device(&b_name)), "A never takes B's pen");
        assert!(!b.assigns(&device(&a_name)), "B never takes A's pen");
        let other_product = InputDeviceFacts {
            product: 0x0001,
            ..device(&a_name)
        };
        assert!(!a.assigns(&other_product));
        assert!(!a.udev_rule().contains(b.id()));
        assert!(!b.udev_rule().contains(a.id()));
    }

    #[test]
    fn a_physical_device_with_the_desktops_name_is_not_assigned() {
        let a = pen("0123456789abcdef");
        let name = a.device_name();
        let usb = InputDeviceFacts {
            devpath: "/devices/pci0000:00/0000:00:14.0/usb1/1-1/input/input9",
            bustype: 0x03,
            ..device(&name)
        };
        assert!(!a.assigns(&usb));
        let usb_bus_only = InputDeviceFacts {
            bustype: 0x03,
            ..device(&name)
        };
        assert!(!a.assigns(&usb_bus_only));
        let usb_path_only = InputDeviceFacts {
            devpath: usb.devpath,
            ..device(&name)
        };
        assert!(!a.assigns(&usb_path_only));
    }

    #[test]
    fn the_rule_names_its_own_seat_device_bus_and_devpath_and_never_seat0() {
        let a = pen("0123456789abcdef");
        let rule = a.udev_rule();
        assert!(
            rule.contains("ENV{ID_SEAT}=\"seat-arcen-pen-0123456789abcdef\""),
            "{rule}"
        );
        assert!(rule.contains("ATTR{name}==\"Arcen Seat Pen 0123456789abcdef\""));
        assert!(rule.contains("ATTRS{name}==\"Arcen Seat Pen 0123456789abcdef\""));
        assert!(rule.contains("ATTR{id/bustype}==\"0006\""));
        assert!(rule.contains("ATTRS{id/bustype}==\"0006\""));
        assert!(rule.contains("DEVPATH!=\"/devices/virtual/input/*\""));
        assert!(rule.contains("ATTRS{id/vendor}==\"a2ce\""));
        assert!(rule.contains("ATTRS{id/product}==\"0010\""));
        assert!(!rule.contains("seat0\""), "{rule}");
        assert!(rule.starts_with('#'));
        assert!(a.rule_path().starts_with("/run/"));
        assert!(is_rule_file(
            a.rule_path().rsplit('/').next().unwrap_or_default()
        ));
    }

    #[test]
    fn each_desktops_user_alone_is_granted_its_own_node() {
        let [clear, grant] = DesktopPen::grant_plan("/dev/input/event7", 1001);
        assert_eq!(clear, ["-b", "/dev/input/event7"]);
        assert_eq!(grant, ["-m", "u:1001:rw", "/dev/input/event7"]);
        let [_, other] = DesktopPen::grant_plan("/dev/input/event8", 1002);
        assert!(
            !other
                .iter()
                .any(|arg| arg.contains("1001") || arg.contains("event7"))
        );
        assert_eq!(
            DesktopPen::revoke_plan("/dev/input/event7"),
            ["-b", "/dev/input/event7"]
        );
    }

    #[test]
    fn ids_seats_and_rule_files_are_validated() {
        for refused in [
            "",
            "0123456",
            "0123456789ABCDEF",
            "0123456789abcdeg",
            "../../etc",
        ] {
            assert!(DesktopPen::new(refused).is_none(), "{refused}");
        }
        let random = DesktopPen::from_random([0, 1, 2, 3, 0xfc, 0xfd, 0xfe, 0xff]);
        assert_eq!(random.id(), "00010203fcfdfeff");
        assert!(is_desktop_seat(&random.seat()));
        for refused in [
            "seat0",
            "seat-arcen-pen",
            "seat-arcen-pen-",
            "seat-arcen-pen-xyz12345",
            "seat-arcen-pen-0123456789abcdef;rm",
        ] {
            assert!(!is_desktop_seat(refused), "{refused}");
        }
        assert!(is_rule_file("72-arcen-lab-seat-pen.rules"));
        assert!(is_rule_file("72-arcen-lab-seat-pen-0123456789abcdef.rules"));
        for refused in [
            "72-arcen-lab-seat-pen-x.rules",
            "72-arcen-lab-seat-pen-0123456789abcdef.rules.bak",
            "70-uaccess.rules",
            "72-arcen-lab-seat-penX.rules",
        ] {
            assert!(!is_rule_file(refused), "{refused}");
        }
    }

    #[test]
    fn a_native_seat_takes_the_gpu_card_vkms_and_its_own_pen_only() {
        let a = pen("0123456789abcdef");
        let native = NativeSeat::new("pci-0000:00:10.0").unwrap();
        let rule = a.native_udev_rule(&native);
        assert!(rule.contains(
            "SUBSYSTEM==\"drm\", KERNEL==\"card[0-9]*\", ENV{ID_PATH}==\"pci-0000:00:10.0\", ENV{ID_SEAT}=\"seat-arcen-pen-0123456789abcdef\""
        ), "{rule}");
        assert!(
            rule.contains(
                "DEVPATH==\"*/vkms/*\", ENV{ID_SEAT}=\"seat-arcen-pen-0123456789abcdef\""
            )
        );
        assert!(rule.contains(&a.udev_rule()), "the pen block is unchanged");
        assert!(!rule.contains("seat0\""));
        assert!(!rule.contains("renderD"), "render nodes never join a seat");
        let release = a.native_release_rule();
        assert!(release.contains("TAG-=\"master-of-seat\", TAG-=\"seat\""));
        assert!(release.contains("ENV{ID_SEAT}=\"seat-arcen-pen-0123456789abcdef\""));
        assert!(!release.contains("ID_PATH"), "only vkms is released by tag");
    }

    #[test]
    fn native_seat_gpus_are_pci_id_paths_only() {
        assert!(NativeSeat::new("pci-0000:00:10.0").is_some());
        assert!(NativeSeat::new("pci-0000:3b:00.1").is_some());
        for refused in [
            "",
            "0000:00:10.0",
            "pci-0000:00:10",
            "pci-0000:00:10.0\"",
            "pci-0000:00:10.0, ENV{ID_SEAT}=\"seat0\"",
            "platform-vkms",
            "pci-00:10.0",
        ] {
            assert!(NativeSeat::new(refused).is_none(), "{refused}");
        }
    }

    #[test]
    fn the_mutter_override_drops_only_the_vkms_ignore_lines() {
        let distribution = "DRIVER==\"tegra\", SUBSYSTEM==\"platform\", TAG+=\"mutter-device-disable-atomic-kms\"\n\
                            ENV{ID_PATH}==\"platform-vkms\", TAG+=\"mutter-device-ignore\"\n\
                            ENV{DEVPATH}==\"/devices/faux/vkms/drm/card[0-9]*\", TAG+=\"mutter-device-ignore\"\n\
                            ATTRS{vendor}==\"0x8086\", TAG+=\"mutter-device-disable-kms-modifiers\"\n";
        let out = mutter_rules_without_vkms_ignore(distribution);
        assert!(is_mutter_rules_override(&out));
        assert!(
            !out.lines().skip(1).any(|line| line.contains("vkms")),
            "{out}"
        );
        assert!(out.contains("tegra"));
        assert!(out.contains("0x8086"));
        assert!(!is_mutter_rules_override(distribution));
    }

    #[test]
    fn the_raster_has_a_resolution() {
        assert!(RASTER > 0);
        assert!(RESOLUTION_PER_MM > 0);
    }
}
