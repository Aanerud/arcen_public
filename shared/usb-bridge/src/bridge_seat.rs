// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! A bridged USB device belongs to exactly one session, never to the
//! console's.
//!
//! A bridged device appears under one port of a virtual host controller
//! (USB/IP `vhci_hcd` first, the out-of-tree `usb_vhci_hcd` as a lab
//! fallback). Before the port is connected, the host adapter installs a
//! volatile udev rule for that port's subtree, so the device is never
//! unclaimed, not even for one udev event:
//!
//! - **Native Wayland desktop:** the rule puts the subtree on the desktop's
//!   own seat, whose compositor takes it through logind.
//! - **Dedicated Xorg desktop:** the subtree stays on `seat0` (a `seat0`
//!   Xorg hot-adds only `seat0` devices), and its input nodes are tagged for
//!   Xorg (`ID_INPUT.tags`) with [`XORG_BRIDGE_TAG`] and the desktop's own
//!   tag ([`x11_session_tag`]). A system-wide Xorg class
//!   ([`xorg_bridge_ignore_conf`], installed by the host installer) makes
//!   every X server ignore [`XORG_BRIDGE_TAG`], and the desktop's own
//!   configuration ends by ignoring every bridge again and re-admitting only
//!   its own tag ([`xorg_session_bridge_class`]), so
//!   the desktop hot-adds the tablet the moment it connects and no other X
//!   server ever opens it. `LIBINPUT_IGNORE_DEVICE` keeps every libinput
//!   compositor (a Wayland login screen or console session) off it.
//!
//! Every rule strips `uaccess`, so no logind ACL ever reaches the device.
//! This module owns the portable parts: names, the port's sysfs path prefix,
//! the rule text, which device paths it assigns, the fail-closed check of
//! the udev database, the Xorg classes, and the check that every running X
//! server honours them. Writing files and running `udevadm` stay in the
//! adapter.

use std::fmt::{Display, Formatter, Write as _};

/// Directory volatile udev rules are written to.
pub const RULE_DIRECTORY: &str = "/run/udev/rules.d";
/// File name prefix of every bridge rule (sorts before `73-seat-late.rules`,
/// which turns `ID_SEAT` into a tag and applies `uaccess`).
pub const RULE_FILE_PREFIX: &str = "72-arcen-usb-bridge-";
/// File name suffix of every bridge rule.
pub const RULE_FILE_SUFFIX: &str = ".rules";
/// Name prefix of a bridge for a dedicated Xorg desktop
/// (`arcen-x11-<display>-<16 hex>`; not a seat).
pub const X11_SESSION_PREFIX: &str = "arcen-x11-";
/// Seat name prefix of a native Wayland desktop seat (ADR 0015).
pub const NATIVE_SEAT_PREFIX: &str = "seat-arcen-pen-";
/// Most input event nodes one bridged device may present to a desktop.
pub const MAX_BRIDGED_INPUT_NODES: usize = 8;
/// The Xorg tag every bridged input node carries; every X server ignores it.
pub const XORG_BRIDGE_TAG: &str = "arcen_bridge";
/// Where the host installer puts [`xorg_bridge_ignore_conf`].
pub const XORG_BRIDGE_IGNORE_PATH: &str = "/etc/X11/xorg.conf.d/90-arcen-bridged-usb.conf";
/// Identifier of the system-wide class that ignores bridged devices.
pub const XORG_BRIDGE_IGNORE_CLASS: &str = "arcen-bridged-usb-ignore";
/// Identifier of a dedicated desktop's class re-admitting its own bridge.
pub const XORG_SESSION_BRIDGE_CLASS: &str = "arcen-bridged-usb-session";
/// Identifier of a dedicated desktop's class ignoring every bridge again
/// after its input isolation classes (which admit by device name and VID,
/// both of which a bridged device's string descriptors could imitate).
pub const XORG_SESSION_BRIDGE_IGNORE_CLASS: &str = "arcen-bridged-usb-session-ignore";
/// Highest display number of a dedicated Xorg desktop.
pub const MAX_X11_DISPLAY: u16 = 99;
/// Longest seat name accepted.
const MAX_SEAT_NAME: usize = 64;

/// Why a seat, port or node fact was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BridgeSeatError {
    /// The name is not an Arcen bridge or native seat name.
    SeatName,
    /// The device path prefix is not exactly one virtual host controller port.
    PortPrefix,
    /// An input node fact is malformed or outside the port.
    Node,
    /// More nodes than [`MAX_BRIDGED_INPUT_NODES`].
    TooManyNodes,
}

impl Display for BridgeSeatError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::SeatName => "not an Arcen bridge seat name",
            Self::PortPrefix => "not a virtual host controller port path",
            Self::Node => "bridged input node outside its port or malformed",
            Self::TooManyNodes => "too many bridged input nodes",
        })
    }
}

impl std::error::Error for BridgeSeatError {}

/// Whom a bridged device belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BridgeSeat {
    /// One dedicated Xorg desktop (display `:display`): the device stays on
    /// `seat0`, tagged for that desktop's X server only. `id` (16 lowercase
    /// hex digits) names this bridge's rule.
    X11Session { display: u16, id: String },
    /// A native Wayland desktop's own seat (`seat-arcen-pen-<id>`), whose
    /// compositor takes the devices through logind.
    NativeSeat { seat: String },
}

impl BridgeSeat {
    /// A bridge for the dedicated Xorg desktop on `:display`, named from 8
    /// random bytes.
    ///
    /// # Errors
    ///
    /// [`BridgeSeatError::SeatName`] for a display outside 1–99.
    pub fn x11_session(display: u16, random: [u8; 8]) -> Result<Self, BridgeSeatError> {
        if !(1..=MAX_X11_DISPLAY).contains(&display) {
            return Err(BridgeSeatError::SeatName);
        }
        let mut id = String::with_capacity(16);
        for byte in random {
            let _ = write!(id, "{byte:02x}");
        }
        Ok(Self::X11Session { display, id })
    }

    /// Parses a name (`arcen-x11-<display>-<16 hex>` or
    /// `seat-arcen-pen-<id>`).
    ///
    /// # Errors
    ///
    /// [`BridgeSeatError::SeatName`] for anything else.
    pub fn parse(name: &str) -> Result<Self, BridgeSeatError> {
        if let Some(rest) = name.strip_prefix(X11_SESSION_PREFIX) {
            let (display, id) = rest.split_once('-').ok_or(BridgeSeatError::SeatName)?;
            let display_ok = !display.is_empty()
                && display.len() <= 2
                && !display.starts_with('0')
                && display.bytes().all(|byte| byte.is_ascii_digit());
            let id_ok = id.len() == 16
                && id
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
            return match display.parse::<u16>() {
                Ok(display) if display_ok && id_ok && display <= MAX_X11_DISPLAY => {
                    Ok(Self::X11Session {
                        display,
                        id: id.to_owned(),
                    })
                }
                _ => Err(BridgeSeatError::SeatName),
            };
        }
        if let Some(id) = name.strip_prefix(NATIVE_SEAT_PREFIX) {
            if !id.is_empty()
                && name.len() <= MAX_SEAT_NAME
                && id
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
            {
                return Ok(Self::NativeSeat {
                    seat: name.to_owned(),
                });
            }
        }
        Err(BridgeSeatError::SeatName)
    }

    /// The name passed to the helper and logged: the logind seat of a native
    /// desktop, or the X11 bridge's own name (which is not a seat).
    #[must_use]
    pub fn seat_name(&self) -> String {
        match self {
            Self::X11Session { display, id } => format!("{X11_SESSION_PREFIX}{display}-{id}"),
            Self::NativeSeat { seat } => seat.clone(),
        }
    }

    /// The identifier the rule file and its labels are named by.
    #[must_use]
    pub fn rule_id(&self) -> String {
        match self {
            Self::X11Session { display, id } => format!("x11-{display}-{id}"),
            Self::NativeSeat { seat } => format!(
                "pen-{}",
                seat.strip_prefix(NATIVE_SEAT_PREFIX).unwrap_or(seat)
            ),
        }
    }

    /// The rule's file name inside [`RULE_DIRECTORY`].
    #[must_use]
    pub fn rule_file_name(&self) -> String {
        format!("{RULE_FILE_PREFIX}{}{RULE_FILE_SUFFIX}", self.rule_id())
    }
}

/// The Xorg tag of the dedicated desktop on `:display` (`arcen_bridge_<N>`).
#[must_use]
pub fn x11_session_tag(display: u16) -> String {
    format!("{XORG_BRIDGE_TAG}_{display}")
}

/// The first line of every bridge rule's text, by which an uninstaller
/// recognizes a rule Arcen wrote.
pub const RULE_HEADER: &str = "# Arcen: bridged USB port ";

/// Whether `text` is a bridge rule Arcen wrote.
#[must_use]
pub fn is_bridge_rule_text(text: &str) -> bool {
    text.lines().any(|line| line.starts_with(RULE_HEADER))
}

/// The port a bridge rule's text names (from its header line).
#[must_use]
pub fn rule_port(text: &str) -> Option<&str> {
    text.lines().find_map(|line| {
        let rest = line.strip_prefix(RULE_HEADER)?;
        let (port, _) = rest.split_once(" belongs to ")?;
        validate_port_prefix(port).is_ok().then_some(port)
    })
}

/// Whether `name` is a bridge rule file (for the start-up sweep).
#[must_use]
pub fn is_rule_file(name: &str) -> bool {
    name.strip_prefix(RULE_FILE_PREFIX)
        .and_then(|rest| rest.strip_suffix(RULE_FILE_SUFFIX))
        .is_some_and(|id| {
            !id.is_empty()
                && id
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        })
}

/// The sysfs device path of one virtual host controller port:
/// `/devices/platform/vhci_hcd.<C>/usb<B>/<B>-<P>` (USB/IP) or
/// `/devices/platform/usb_vhci_hcd.<C>/usb<B>/<B>-<P>` (lab fallback).
///
/// # Errors
///
/// [`BridgeSeatError::PortPrefix`] for anything that is not exactly one such
/// root-hub port; a physical controller is never accepted.
pub fn validate_port_prefix(prefix: &str) -> Result<(), BridgeSeatError> {
    let mut parts = prefix.split('/');
    let ok = (|| {
        if !parts.next()?.is_empty() || parts.next()? != "devices" || parts.next()? != "platform" {
            return None;
        }
        let controller = parts.next()?;
        let index = controller
            .strip_prefix("vhci_hcd.")
            .or_else(|| controller.strip_prefix("usb_vhci_hcd."))?;
        let bus = parts.next()?.strip_prefix("usb")?;
        let (port_bus, port) = parts.next()?.split_once('-')?;
        let digits = |value: &str| {
            !value.is_empty() && value.len() <= 3 && value.bytes().all(|byte| byte.is_ascii_digit())
        };
        (parts.next().is_none()
            && digits(index)
            && digits(bus)
            && port_bus == bus
            && digits(port)
            && !port.starts_with('0'))
        .then_some(())
    })();
    ok.ok_or(BridgeSeatError::PortPrefix)
}

/// Whether the rule for `prefix` assigns the device at `devpath`: the port
/// itself and everything beneath it, never a sibling port (`<B>-<P>0`).
#[must_use]
pub fn assigns(prefix: &str, devpath: &str) -> bool {
    validate_port_prefix(prefix).is_ok()
        && devpath
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// The volatile udev rule giving the port at `prefix` to `seat`.
///
/// # Errors
///
/// [`BridgeSeatError::PortPrefix`] when `prefix` is not one port.
pub fn udev_rule(prefix: &str, seat: &BridgeSeat) -> Result<String, BridgeSeatError> {
    validate_port_prefix(prefix)?;
    let seat_name = seat.seat_name();
    BridgeSeat::parse(&seat_name)?;
    let label = format!("arcen_usb_bridge_{}", seat.rule_id().replace('-', "_"));
    let (owner, apply) = match seat {
        BridgeSeat::NativeSeat { .. } => (
            format!("{seat_name}, never seat0"),
            format!(
                "ENV{{ID_SEAT}}=\"{seat_name}\", TAG-=\"uaccess\"\n\
                 SUBSYSTEM==\"input\", KERNEL==\"event*\", SYMLINK+=\"input/arcen-bridge-{}-%k\"\n",
                seat.rule_id()
            ),
        ),
        BridgeSeat::X11Session { display, .. } => (
            format!(
                "X11 display :{display} ({seat_name}), never another X server or seat0's consoles"
            ),
            format!(
                "TAG-=\"uaccess\", TAG-=\"seat\", TAG-=\"power-switch\"\n\
                 SUBSYSTEM==\"input\", ENV{{ID_INPUT.tags}}=\"{XORG_BRIDGE_TAG},{}\", ENV{{LIBINPUT_IGNORE_DEVICE}}=\"1\"\n",
                x11_session_tag(*display)
            ),
        ),
    };
    Ok(format!(
        "{RULE_HEADER}{prefix} belongs to {owner}.\n\
         ACTION==\"remove\", GOTO=\"{label}_end\"\n\
         DEVPATH==\"{prefix}\", GOTO=\"{label}_apply\"\n\
         DEVPATH==\"{prefix}/*\", GOTO=\"{label}_apply\"\n\
         GOTO=\"{label}_end\"\n\
         LABEL=\"{label}_apply\"\n\
         {apply}\
         SUBSYSTEM==\"usb\", ENV{{DEVTYPE}}==\"usb_device\", ATTR{{power/control}}=\"on\"\n\
         LABEL=\"{label}_end\"\n"
    ))
}

/// Fail-closed proof from a udev database entry (`/run/udev/data/c<M>:<m>`)
/// that the bridge rule applied to an input node: no `uaccess` tag, and
/// - for a native seat: the node is on that seat;
/// - for an X11 desktop: the node carries exactly the bridge tags for that
///   desktop, libinput is told to ignore it, it is on no other seat than
///   `seat0` (a `seat0` X server hot-adds only `seat0` devices, by
///   `ID_SEAT`), and it lacks logind's `seat` and `power-switch` tags, so
///   logind neither tracks it as a `seat0` device for a console session to
///   take nor acts on its power keys.
#[must_use]
pub fn verify_seat(udev_database: &str, seat: &BridgeSeat) -> bool {
    let has_tag = |tag: &str| {
        udev_database
            .lines()
            .any(|line| (line.starts_with("G:") || line.starts_with("Q:")) && line[2..] == *tag)
    };
    if has_tag("uaccess") {
        return false;
    }
    let seats: Vec<&str> = udev_database
        .lines()
        .filter_map(|line| line.strip_prefix("E:ID_SEAT="))
        .collect();
    match seat {
        BridgeSeat::NativeSeat { seat } => seats == [seat.as_str()],
        BridgeSeat::X11Session { display, .. } => {
            let tags = format!(
                "E:ID_INPUT.tags={XORG_BRIDGE_TAG},{}",
                x11_session_tag(*display)
            );
            let tagged: Vec<&str> = udev_database
                .lines()
                .filter(|line| line.starts_with("E:ID_INPUT.tags="))
                .collect();
            tagged == [tags.as_str()]
                && udev_database
                    .lines()
                    .any(|line| line == "E:LIBINPUT_IGNORE_DEVICE=1")
                && seats.iter().all(|name| *name == "seat0")
                && !has_tag("seat")
                && !has_tag("power-switch")
        }
    }
}

/// The system-wide Xorg class (installed at [`XORG_BRIDGE_IGNORE_PATH`]):
/// every X server ignores bridged devices. A dedicated desktop's own
/// configuration is read after the configuration directories and re-admits
/// its own bridge ([`xorg_session_bridge_class`]).
#[must_use]
pub fn xorg_bridge_ignore_conf() -> String {
    format!(
        "# Arcen: a bridged USB device (Native tablet) belongs to one dedicated Xorg desktop.\n\
         # Every other X server ignores it: the login screen, console sessions, other desktops.\n\
         # Installed by the Arcen Pier installer and removed by its uninstall.\n\
         Section \"InputClass\"\n    \
             Identifier     \"{XORG_BRIDGE_IGNORE_CLASS}\"\n    \
             MatchTag       \"{XORG_BRIDGE_TAG}\"\n    \
             Option         \"Ignore\" \"on\"\n\
         EndSection\n"
    )
}

/// Whether `text` is exactly [`xorg_bridge_ignore_conf`].
#[must_use]
pub fn is_xorg_bridge_ignore_conf(text: &str) -> bool {
    text == xorg_bridge_ignore_conf()
}

/// The classes a dedicated Xorg desktop on `:display` appends last, after its
/// input isolation classes: first every bridged device is ignored again (the
/// isolation classes re-admit Arcen's own virtual devices by name and vendor
/// id, which a bridged device's string descriptors could imitate, and a later
/// class's `Ignore` wins), then exactly this desktop's own bridge is
/// re-admitted, with the xf86-input-wacom driver (libinput is told to ignore
/// bridged nodes) and its pressure recalibration off
/// ([`XORG_BRIDGE_PRESSURE_RECALIBRATION`]).
#[must_use]
pub fn xorg_session_bridge_class(display: u16) -> String {
    format!(
        "Section \"InputClass\"\n    \
             Identifier     \"{XORG_SESSION_BRIDGE_IGNORE_CLASS}\"\n    \
             MatchTag       \"{XORG_BRIDGE_TAG}\"\n    \
             Option         \"Ignore\" \"on\"\n\
         EndSection\n\
         \n\
         Section \"InputClass\"\n    \
             Identifier     \"{XORG_SESSION_BRIDGE_CLASS}\"\n    \
             MatchTag       \"{}\"\n    \
             Driver         \"wacom\"\n    \
             Option         \"Ignore\" \"off\"\n    \
             Option         \"PressureRecalibration\" \"{XORG_BRIDGE_PRESSURE_RECALIBRATION}\"\n\
         EndSection\n",
        x11_session_tag(display)
    )
}

/// xf86-input-wacom's `PressureRecalibration` for a bridged tablet: off.
///
/// With it on (the driver's default), the first pressure a pen reports in
/// proximity becomes its zero, for worn pens that never read zero. A tablet
/// on a desk is sampled every few milliseconds, so that first sample is a
/// hover at zero. A bridged tablet's interrupt endpoint is read once per
/// network round trip (Linux `usbhid` keeps one transfer in flight), so a
/// pen put down at an ordinary speed often shows up already in contact: the
/// driver then takes the contact pressure as zero, reports a pressure near
/// nought, and clicks only once the pressure moves away from the landing. Pages
/// and applications draw nothing visible, or broken strokes. The driver's
/// manual documents this downside and the option that turns it off
/// (`wacom(4)`).
pub const XORG_BRIDGE_PRESSURE_RECALIBRATION: &str = "off";

/// The display whose bridge a rendered Xorg configuration re-admits: only
/// when the configuration ends with exactly [`xorg_session_bridge_class`] for
/// it, so no later class can re-admit a bridged device.
#[must_use]
pub fn admitted_bridge_display(config: &str) -> Option<u16> {
    let once = |identifier: &str| config.matches(&format!("\"{identifier}\"")).count() == 1;
    if !once(XORG_SESSION_BRIDGE_CLASS) || !once(XORG_SESSION_BRIDGE_IGNORE_CLASS) {
        return None;
    }
    let config = config.trim_end();
    (1..=MAX_X11_DISPLAY)
        .find(|display| config.ends_with(xorg_session_bridge_class(*display).trim_end()))
}

/// What the adapter found about one running X server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XServerFacts {
    /// Its process id (for the refusal's log line).
    pub pid: u32,
    /// The display it serves, when its command line names one.
    pub display: Option<u16>,
    /// The bridge display its `-config` file re-admits, with every other
    /// bridge ignored after its isolation classes
    /// ([`admitted_bridge_display`]).
    pub admits_bridge: Option<u16>,
    /// Its `-config` file carries a dedicated desktop's input isolation
    /// (which re-admits Arcen's virtual devices by name and vendor id, after
    /// the system-wide ignore class).
    pub ignores_foreign_input: bool,
    /// It read the configuration directories after [`xorg_bridge_ignore_conf`]
    /// was installed (started later, without `-configdir`).
    pub loaded_bridge_ignore: bool,
}

/// Why a dedicated desktop's bridge would not be confined to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum X11IsolationGap {
    /// The system-wide ignore class is not installed.
    IgnoreNotInstalled,
    /// No running X server serves the desktop's display.
    DesktopServerMissing,
    /// The desktop's server does not re-admit its bridge (it was started by
    /// an earlier Pier).
    DesktopPredatesBridgeClass,
    /// Another X server could take the device (it started before the
    /// ignore class was installed, or re-admits this desktop's tag).
    ForeignServer { pid: u32 },
}

/// Checks that a bridged device tagged for the desktop on `:display` reaches
/// that desktop's X server and no other: the desktop ends its configuration
/// with its own bridge classes; another dedicated desktop must too (its
/// isolation classes come after the system-wide ignore class and admit by
/// name and vendor id, which a bridged device could imitate), and every
/// other server must have loaded the system-wide ignore class.
///
/// # Errors
///
/// The first [`X11IsolationGap`] found.
pub fn x11_isolation_in_force(
    ignore_installed: bool,
    servers: &[XServerFacts],
    display: u16,
) -> Result<(), X11IsolationGap> {
    if !ignore_installed {
        return Err(X11IsolationGap::IgnoreNotInstalled);
    }
    let mut own = false;
    for server in servers {
        if server.display == Some(display) {
            if server.admits_bridge != Some(display) {
                return Err(X11IsolationGap::DesktopPredatesBridgeClass);
            }
            own = true;
            continue;
        }
        let confined = server.admits_bridge != Some(display)
            && if server.ignores_foreign_input {
                server.admits_bridge.is_some()
            } else {
                server.loaded_bridge_ignore
            };
        if !confined {
            return Err(X11IsolationGap::ForeignServer { pid: server.pid });
        }
    }
    if own {
        Ok(())
    } else {
        Err(X11IsolationGap::DesktopServerMissing)
    }
}

/// What an input node of a bridged device is, from udev's `ID_INPUT_*`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BridgedNodeRole {
    /// A pen/stylus surface (`ID_INPUT_TABLET` without pad).
    Pen,
    /// Express keys, rings and strips (`ID_INPUT_TABLET_PAD`).
    Pad,
    /// A touch surface (`ID_INPUT_TOUCHPAD`/`ID_INPUT_TOUCHSCREEN`).
    Touch,
    /// Anything else the device's driver created (mouse-mode reports,
    /// keyboard-style express keys).
    Other,
}

impl BridgedNodeRole {
    /// The role from a udev database entry.
    #[must_use]
    pub fn from_udev_database(udev_database: &str) -> Self {
        let has = |key: &str| {
            udev_database
                .lines()
                .any(|line| line == format!("E:{key}=1"))
        };
        if has("ID_INPUT_TABLET_PAD") {
            Self::Pad
        } else if has("ID_INPUT_TOUCHPAD") || has("ID_INPUT_TOUCHSCREEN") {
            Self::Touch
        } else if has("ID_INPUT_TABLET") {
            Self::Pen
        } else {
            Self::Other
        }
    }
}

/// One verified input event node of a bridged device: facts, never bytes.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BridgedInputNode {
    /// `/dev/input/event<N>`.
    pub devnode: String,
    /// The node's sysfs path (under the port prefix).
    pub devpath: String,
    /// What it is.
    pub role: BridgedNodeRole,
    /// The kernel device name (device-chosen; only logged).
    pub name: String,
    /// The kernel HID driver bound to the node's device (`wacom`,
    /// `hid-generic`, …); empty when unknown.
    #[serde(default)]
    pub kernel_driver: String,
}

impl BridgedInputNode {
    /// Checks the node is an event node under `prefix`.
    ///
    /// # Errors
    ///
    /// [`BridgeSeatError::Node`] otherwise.
    pub fn validate(&self, prefix: &str) -> Result<(), BridgeSeatError> {
        let number = self
            .devnode
            .strip_prefix("/dev/input/event")
            .ok_or(BridgeSeatError::Node)?;
        let leaf = self.devpath.rsplit('/').next().unwrap_or_default();
        if number.is_empty()
            || number.len() > 4
            || !number.bytes().all(|byte| byte.is_ascii_digit())
            || leaf != format!("event{number}")
            || !assigns(prefix, &self.devpath)
            || self.devpath.contains("/../")
        {
            return Err(BridgeSeatError::Node);
        }
        Ok(())
    }
}

/// Whether a dedicated Xorg desktop can use this tablet: one of its pen
/// nodes is served by the kernel `wacom` driver, and xf86-input-wacom (the
/// driver [`xorg_session_bridge_class`] names) is installed.
#[must_use]
pub fn x11_hotplug_capable(nodes: &[BridgedInputNode], wacom_x_driver: bool) -> bool {
    wacom_x_driver
        && nodes
            .iter()
            .any(|node| node.role == BridgedNodeRole::Pen && node.kernel_driver == "wacom")
}

/// Whether a report descriptor has an input field the kernel's `hid-input`
/// could map to a key the console keyboard handler binds (text VTs, `SysRq`,
/// power). `hid-input` maps keys per field usage, whatever collection the
/// field is in, so every Input item is checked, with its usages paged as
/// Linux (5.3 and later) pages them, against an allowlist of usages that
/// map to buttons and axes only: the Digitizers and Button pages, the
/// Generic Desktop pointer and axis usages (Pointer, Mouse, X, Y, Z, Rx, Ry,
/// Rz, Slider, Dial, Wheel), and the vendor pages `0xFF00` (which
/// `hid-input` ignores) and `0xFF0D` (which it maps to `BTN_MISC` and
/// `ABS_MISC` at most). Every other page, including the vendor pages
/// `hid-input` maps to keyboard keys (`0xFF01`, `0xFF7F`), a Usage Page
/// change between a usage and its Main item, a delimiter, or a malformed
/// descriptor counts as yes. On an X11 desktop a bridged device stays on
/// `seat0`, where that handler binds it, so such interfaces are refused.
#[must_use]
pub fn declares_keyboard(descriptor: &[u8]) -> bool {
    hid_input_usages(descriptor).is_none_or(|usages| {
        usages
            .iter()
            .any(|usage| !x11_allowed_usages(usage.page, usage.low, usage.high))
    })
}

/// Whether every usage `first..=last` on `page` maps to a button or an axis.
fn x11_allowed_usages(page: u16, first: u16, last: u16) -> bool {
    const DIGITIZER: u16 = 0x0D;
    const BUTTON: u16 = 0x09;
    const GENERIC_DESKTOP: u16 = 0x01;
    let generic = |usage: u16| matches!(usage, 0x01 | 0x02 | 0x30..=0x38);
    match page {
        DIGITIZER | BUTTON | 0xFF00 | 0xFF0D => true,
        GENERIC_DESKTOP => first <= last && (first..=last).all(generic),
        _ => false,
    }
}

/// Usages a Local item declared: one usage or a range, with its page as
/// the kernel completes it, and whether it was extended (a 4-byte item
/// naming its own page, which the kernel never re-pages).
#[derive(Debug, Clone, Copy)]
struct PendingUsages {
    page: u16,
    low: u16,
    high: u16,
    extended: bool,
}

/// One Local item: a usage, or the bounds of a range. The kernel expands
/// Minimum..=Maximum when it reads the Maximum, completing each with the
/// page in effect then unless the Maximum is extended. `None` refuses the
/// descriptor.
fn local_item(
    pending: &mut Vec<PendingUsages>,
    minimum: &mut Option<u32>,
    tag: u8,
    size: usize,
    value: u32,
    page: u16,
) -> Option<()> {
    if pending.len() >= crate::hid_digitizer::MAX_LOCAL_USAGES {
        return None;
    }
    let split = |value: u32| ((value >> 16) as u16, (value & 0xFFFF) as u16);
    match tag {
        0x0 if size == 4 => {
            let (page, id) = split(value);
            pending.push(PendingUsages {
                page,
                low: id,
                high: id,
                extended: true,
            });
        }
        0x0 => {
            let id = u16::try_from(value).ok()?;
            pending.push(PendingUsages {
                page,
                low: id,
                high: id,
                extended: false,
            });
        }
        0x1 => *minimum = Some(value),
        0x2 => {
            let low = (*minimum)?;
            if low > value {
                return Some(());
            }
            if size == 4 {
                let ((low_page, low), (high_page, high)) = (split(low), split(value));
                if low_page != high_page {
                    return None;
                }
                pending.push(PendingUsages {
                    page: high_page,
                    low,
                    high,
                    extended: true,
                });
            } else {
                pending.push(PendingUsages {
                    page,
                    low: u16::try_from(low).ok()?,
                    high: u16::try_from(value).ok()?,
                    extended: false,
                });
            }
        }
        // Delimiters (alternative usages): never on a tablet.
        0xA => return None,
        _ => {}
    }
    Some(())
}

/// Every usage range declared for an Input item, in any collection at any
/// depth, paged as Linux's `hid_parser_main` pages it: a short usage takes
/// the Usage Page in effect when it is declared, and at the Main item
/// `hid_concatenate_last_usage_page` re-pages the pending short usages, from
/// the last back to the first already on the Main item's page, to that page.
/// `None` for a malformed descriptor, a delimiter, or a Usage Page change
/// (an item or a Pop) between a short usage and its Main item (the fail-safe:
/// real tablets never do it).
fn hid_input_usages(descriptor: &[u8]) -> Option<Vec<PendingUsages>> {
    if descriptor.is_empty()
        || descriptor.len() > crate::hid_digitizer::MAX_HID_REPORT_DESCRIPTOR_BYTES
    {
        return None;
    }
    let mut ranges = Vec::new();
    let mut page: u16 = 0;
    let mut pages: Vec<u16> = Vec::new();
    let mut pending: Vec<PendingUsages> = Vec::new();
    let mut minimum: Option<u32> = None;
    let mut at = 0;
    while at < descriptor.len() {
        let prefix = descriptor[at];
        if prefix == 0xFE {
            // Long item: skip it (no long items are defined).
            let size = usize::from(*descriptor.get(at + 1)?);
            at = at.checked_add(3 + size)?;
            continue;
        }
        let size = match prefix & 0x03 {
            3 => 4,
            size => usize::from(size),
        };
        let data = descriptor.get(at + 1..at + 1 + size)?;
        at += 1 + size;
        let value = data
            .iter()
            .rev()
            .fold(0_u32, |value, byte| (value << 8) | u32::from(*byte));
        let short_pending = || pending.iter().any(|usage| !usage.extended);
        match (prefix >> 2) & 0x03 {
            // Main.
            0 => {
                for usage in pending.iter_mut().rev() {
                    if usage.extended {
                        continue;
                    }
                    if usage.page == page {
                        break;
                    }
                    usage.page = page;
                }
                if prefix >> 4 == 0x8 {
                    ranges.append(&mut pending);
                }
                pending.clear();
                minimum = None;
            }
            // Global.
            1 => match prefix >> 4 {
                0x0 => {
                    let next = u16::try_from(value).ok()?;
                    if next != page && short_pending() {
                        return None;
                    }
                    page = next;
                }
                0xA => {
                    if pages.len() >= crate::hid_digitizer::MAX_GLOBAL_STACK_DEPTH {
                        return None;
                    }
                    pages.push(page);
                }
                0xB => {
                    let next = pages.pop()?;
                    if next != page && short_pending() {
                        return None;
                    }
                    page = next;
                }
                _ => {}
            },
            // Local.
            2 => local_item(&mut pending, &mut minimum, prefix >> 4, size, value, page)?,
            _ => return None,
        }
    }
    Some(ranges)
}

/// Whether an input device's `capabilities/key` bitmap (sysfs: hexadecimal
/// words of `word_bits` bits, most significant first) holds a key the
/// kernel's console keyboard handler binds: any code below `BTN_MISC`
/// (`0x100`) or a braille dot (`0x1f1`–`0x1fa`), as `kbd_match` tests. A
/// bitmap that does not parse counts as holding one.
#[must_use]
pub fn key_bitmap_reaches_keyboard_handler(bitmap: &str, word_bits: u32) -> bool {
    let words: Option<Vec<u64>> = bitmap
        .split_whitespace()
        .rev()
        .map(|word| u64::from_str_radix(word, 16).ok())
        .collect();
    let Some(words) = words else {
        return true;
    };
    if words.is_empty() || !matches!(word_bits, 32 | 64) {
        return true;
    }
    let set = |code: u32| {
        words
            .get((code / word_bits) as usize)
            .is_some_and(|word| word >> (code % word_bits) & 1 == 1)
    };
    (1..0x100).any(set) || (0x1f1..=0x1fa).any(set)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PORT: &str = "/devices/platform/vhci_hcd.0/usb3/3-1";

    fn x11() -> BridgeSeat {
        BridgeSeat::x11_session(58, [0xde, 0xad, 0xbe, 0xef, 0, 1, 2, 3]).unwrap()
    }

    #[test]
    fn names_round_trip_and_reject_injection() {
        let seat = x11();
        assert_eq!(seat.seat_name(), "arcen-x11-58-deadbeef00010203");
        assert_eq!(BridgeSeat::parse(&seat.seat_name()), Ok(seat.clone()));
        assert_eq!(
            seat.rule_file_name(),
            "72-arcen-usb-bridge-x11-58-deadbeef00010203.rules"
        );
        assert!(is_rule_file(&seat.rule_file_name()));
        let native = BridgeSeat::parse("seat-arcen-pen-7f3a").unwrap();
        assert_eq!(
            native.rule_file_name(),
            "72-arcen-usb-bridge-pen-7f3a.rules"
        );
        for display in [0, 100, 600] {
            assert!(BridgeSeat::x11_session(display, [0; 8]).is_err());
        }
        for bad in [
            "seat0",
            "arcen-x11-58-DEADBEEF00010203",
            "arcen-x11-58-deadbeef",
            "arcen-x11-058-deadbeef00010203",
            "arcen-x11-0-deadbeef00010203",
            "arcen-x11-100-deadbeef00010203",
            "arcen-x11--deadbeef00010203",
            "arcen-x11-58-deadbeef00010203\"",
            "seat-arcen-usb-deadbeef00010203",
            "seat-arcen-pen-",
            "seat-arcen-pen-a\"b",
            "seat-arcen-pen-a/b",
            "seat-arcen-pen-a\nENV{X}=\"y\"",
        ] {
            assert!(BridgeSeat::parse(bad).is_err(), "{bad}");
        }
        assert!(!is_rule_file("72-arcen-usb-bridge-.rules"));
        assert!(!is_rule_file("72-arcen-usb-bridge-x.rules.tmp"));
        assert!(!is_rule_file("61-mutter.rules"));
    }

    #[test]
    fn only_virtual_controller_ports_are_prefixes() {
        for good in [
            PORT,
            "/devices/platform/vhci_hcd.0/usb4/4-8",
            "/devices/platform/usb_vhci_hcd.2/usb7/7-1",
        ] {
            assert_eq!(validate_port_prefix(good), Ok(()), "{good}");
        }
        for bad in [
            "/devices/pci0000:00/0000:00:14.0/usb1/1-1",
            "/devices/platform/vhci_hcd.0/usb3",
            "/devices/platform/vhci_hcd.0/usb3/4-1",
            "/devices/platform/vhci_hcd.0/usb3/3-01",
            "/devices/platform/vhci_hcd.0/usb3/3-1/3-1:1.0",
            "/devices/platform/vhci_hcd.0/usb3/3-1*",
            "/devices/platform/vhci_hcd.x/usb3/3-1",
            "devices/platform/vhci_hcd.0/usb3/3-1",
        ] {
            assert!(validate_port_prefix(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_rule_assigns_its_own_port_subtree_and_nothing_else() {
        assert!(assigns(PORT, PORT));
        assert!(assigns(
            PORT,
            "/devices/platform/vhci_hcd.0/usb3/3-1/3-1:1.0/0003:056A:0317.0001/input/input9/event9"
        ));
        assert!(!assigns(PORT, "/devices/platform/vhci_hcd.0/usb3/3-10"));
        assert!(!assigns(
            PORT,
            "/devices/platform/vhci_hcd.0/usb3/3-2/3-2:1.0"
        ));
        assert!(!assigns(
            PORT,
            "/devices/pci0000:00/0000:00:14.0/usb1/1-1/1-1:1.0"
        ));
        assert!(!assigns(
            PORT,
            "/devices/virtual/misc/uhid/0003:056A:0029.0006"
        ));
        assert!(!assigns(
            "/devices/pci0000:00/usb1/1-1",
            "/devices/pci0000:00/usb1/1-1"
        ));
    }

    fn assert_common_rule_shape(rule: &str) {
        assert!(rule.contains("ACTION==\"remove\", GOTO="));
        assert!(rule.contains(&format!("DEVPATH==\"{PORT}\", GOTO=")));
        assert!(rule.contains(&format!("DEVPATH==\"{PORT}/*\", GOTO=")));
        assert!(rule.contains("TAG-=\"uaccess\""));
        assert!(rule.contains("ATTR{power/control}=\"on\""));
        assert!(is_bridge_rule_text(&format!(
            "# arcen-owner-pid: 7\n{rule}"
        )));
        assert_eq!(rule_port(rule), Some(PORT));
        // Every GOTO has its LABEL.
        for label in ["_apply\"", "_end\""] {
            assert_eq!(rule.matches(label).count(), 3, "{label}");
        }
    }

    #[test]
    fn the_x11_rule_tags_the_nodes_for_one_desktop_and_keeps_them_on_seat0() {
        let rule = udev_rule(PORT, &x11()).unwrap();
        assert_common_rule_shape(&rule);
        assert!(rule.contains(
            "SUBSYSTEM==\"input\", ENV{ID_INPUT.tags}=\"arcen_bridge,arcen_bridge_58\", ENV{LIBINPUT_IGNORE_DEVICE}=\"1\""
        ));
        assert!(rule.contains("belongs to X11 display :58 (arcen-x11-58-deadbeef00010203)"));
        assert!(rule.contains("TAG-=\"uaccess\", TAG-=\"seat\", TAG-=\"power-switch\"\n"));
        // A seat0 X server hot-adds only seat0 devices: no seat is set.
        assert!(!rule.contains("ID_SEAT"));
        assert!(!rule.contains("SYMLINK"));
        assert!(udev_rule("/devices/pci0000:00/usb1/1-1", &x11()).is_err());
    }

    #[test]
    fn the_native_seat_rule_is_unchanged() {
        let native = BridgeSeat::parse("seat-arcen-pen-7f3a").unwrap();
        let rule = udev_rule(PORT, &native).unwrap();
        assert_common_rule_shape(&rule);
        assert!(rule.contains("ENV{ID_SEAT}=\"seat-arcen-pen-7f3a\", TAG-=\"uaccess\""));
        assert!(rule.contains(
            "SUBSYSTEM==\"input\", KERNEL==\"event*\", SYMLINK+=\"input/arcen-bridge-pen-7f3a-%k\""
        ));
        assert!(rule.contains("belongs to seat-arcen-pen-7f3a, never seat0."));
        assert!(!rule.contains("ID_INPUT.tags"));
        assert_eq!(
            rule_port("# Arcen: bridged USB port /devices/pci0/usb1/1-1 belongs to x\n"),
            None
        );
        assert!(!is_bridge_rule_text("ENV{ID_SEAT}=\"seat-arcen-pen-x\"\n"));
    }

    #[test]
    fn the_udev_database_must_prove_the_rule_applied() {
        let native = BridgeSeat::parse("seat-arcen-pen-7f3a").unwrap();
        let on_seat = "E:ID_INPUT=1\nE:ID_SEAT=seat-arcen-pen-7f3a\nG:seat-arcen-pen-7f3a\n";
        assert!(verify_seat(on_seat, &native));
        assert!(!verify_seat(&format!("{on_seat}G:uaccess\n"), &native));
        assert!(!verify_seat(
            &format!("{on_seat}E:ID_SEAT=seat0\n"),
            &native
        ));
        assert!(!verify_seat("E:ID_INPUT=1\n", &native));

        let seat = x11();
        let good = "E:ID_INPUT=1\nE:ID_INPUT_TABLET=1\nE:ID_INPUT.tags=arcen_bridge,arcen_bridge_58\nE:LIBINPUT_IGNORE_DEVICE=1\nG:systemd\n";
        assert!(verify_seat(good, &seat));
        assert!(verify_seat(&format!("{good}E:ID_SEAT=seat0\n"), &seat));
        assert!(!verify_seat(&format!("{good}G:uaccess\n"), &seat));
        assert!(!verify_seat(&format!("{good}Q:uaccess\n"), &seat));
        // logind's seat tag would let a console session take the node.
        assert!(!verify_seat(&format!("{good}G:seat\n"), &seat));
        assert!(!verify_seat(&format!("{good}Q:seat\n"), &seat));
        // Nor logind's power-switch tag (power keys acting on the host).
        assert!(!verify_seat(&format!("{good}G:power-switch\n"), &seat));
        assert!(!verify_seat(
            &format!("{good}E:ID_SEAT=seat-arcen-pen-x\n"),
            &seat
        ));
        assert!(!verify_seat(
            &good.replace("arcen_bridge_58", "arcen_bridge_59"),
            &seat
        ));
        assert!(!verify_seat(&good.replace("arcen_bridge,", ""), &seat));
        assert!(!verify_seat(
            &good.replace("E:LIBINPUT_IGNORE_DEVICE=1\n", ""),
            &seat
        ));
        assert!(!verify_seat(
            &good.replace("E:ID_INPUT.tags=arcen_bridge,arcen_bridge_58\n", ""),
            &seat
        ));
        assert!(!verify_seat("", &seat));
        assert!(!verify_seat(on_seat, &seat));
        assert!(!verify_seat(good, &native));
    }

    #[test]
    fn the_xorg_classes_ignore_every_bridge_and_readmit_only_the_desktops_own() {
        let ignore = xorg_bridge_ignore_conf();
        assert!(ignore.contains("MatchTag       \"arcen_bridge\"\n"));
        assert!(ignore.contains("Option         \"Ignore\" \"on\"\n"));
        assert!(is_xorg_bridge_ignore_conf(&ignore));
        assert!(!is_xorg_bridge_ignore_conf(
            &ignore.replace("\"on\"", "\"off\"")
        ));
        assert!(XORG_BRIDGE_IGNORE_PATH.starts_with("/etc/X11/xorg.conf.d/"));

        let class = xorg_session_bridge_class(58);
        // Every bridge is ignored again first, then only this desktop's own
        // is re-admitted (a later class's Ignore wins).
        let ignore_all = class.find("MatchTag       \"arcen_bridge\"\n").unwrap();
        let own = class.find("MatchTag       \"arcen_bridge_58\"\n").unwrap();
        assert!(ignore_all < own);
        assert!(class[ignore_all..own].contains("Option         \"Ignore\" \"on\"\n"));
        assert!(class[own..].contains("Driver         \"wacom\"\n"));
        assert!(class[own..].contains("Option         \"Ignore\" \"off\"\n"));
        // A pen sampled once per round trip often lands already in contact:
        // its contact pressure must not become the zero.
        assert!(class[own..].contains("Option         \"PressureRecalibration\" \"off\"\n"));
        let config = format!("Section \"ServerFlags\"\nEndSection\n\n{class}");
        assert_eq!(admitted_bridge_display(&config), Some(58));
        // A class after them could re-admit a bridged device: not admitted.
        let followed = format!("{config}\nSection \"InputClass\"\nEndSection\n");
        assert_eq!(admitted_bridge_display(&followed), None);
        let doubled = format!("{class}{class}");
        assert_eq!(admitted_bridge_display(&doubled), None);
        assert_eq!(
            admitted_bridge_display("Section \"ServerFlags\"\nEndSection\n"),
            None
        );
        let widened = config.replace("\"arcen_bridge_58\"", "\"arcen_bridge\"");
        assert_eq!(admitted_bridge_display(&widened), None);
        let extended = config.replace("    Driver", "    MatchProduct   \"x\"\n    Driver");
        assert_eq!(admitted_bridge_display(&extended), None);
    }

    fn server(pid: u32, display: Option<u16>) -> XServerFacts {
        XServerFacts {
            pid,
            display,
            admits_bridge: None,
            ignores_foreign_input: false,
            loaded_bridge_ignore: false,
        }
    }

    #[test]
    fn a_bridge_needs_its_desktop_to_admit_it_and_every_other_server_to_ignore_it() {
        let desktop = XServerFacts {
            admits_bridge: Some(58),
            ignores_foreign_input: true,
            ..server(10, Some(58))
        };
        let greeter = XServerFacts {
            loaded_bridge_ignore: true,
            ..server(20, Some(0))
        };
        let other_desktop = XServerFacts {
            admits_bridge: Some(59),
            ignores_foreign_input: true,
            ..server(30, Some(59))
        };
        // Another desktop without the final bridge classes (an earlier Pier)
        // re-admits by name and vendor id after the system-wide ignore
        // class: a bridged device imitating its virtual devices would reach
        // it, whenever it started.
        let old_other_desktop = XServerFacts {
            admits_bridge: None,
            loaded_bridge_ignore: true,
            ..other_desktop.clone()
        };
        assert_eq!(
            x11_isolation_in_force(true, &[desktop.clone(), old_other_desktop], 58),
            Err(X11IsolationGap::ForeignServer { pid: 30 })
        );
        let ok = [desktop.clone(), greeter.clone(), other_desktop.clone()];
        assert_eq!(x11_isolation_in_force(true, &ok, 58), Ok(()));
        assert_eq!(
            x11_isolation_in_force(false, &ok, 58),
            Err(X11IsolationGap::IgnoreNotInstalled)
        );
        // A login screen started before the ignore class was installed.
        let stale_greeter = server(20, Some(0));
        assert_eq!(
            x11_isolation_in_force(true, &[desktop.clone(), stale_greeter], 58),
            Err(X11IsolationGap::ForeignServer { pid: 20 })
        );
        // A server that re-admits this desktop's tag is never acceptable.
        let squatter = XServerFacts {
            admits_bridge: Some(58),
            loaded_bridge_ignore: true,
            ..server(40, None)
        };
        assert_eq!(
            x11_isolation_in_force(true, &[desktop.clone(), squatter], 58),
            Err(X11IsolationGap::ForeignServer { pid: 40 })
        );
        let old_desktop = XServerFacts {
            admits_bridge: None,
            ..desktop.clone()
        };
        assert_eq!(
            x11_isolation_in_force(true, &[old_desktop, greeter.clone()], 58),
            Err(X11IsolationGap::DesktopPredatesBridgeClass)
        );
        assert_eq!(
            x11_isolation_in_force(true, &[greeter], 58),
            Err(X11IsolationGap::DesktopServerMissing)
        );
    }

    fn node(index: u32, role: BridgedNodeRole, driver: &str) -> BridgedInputNode {
        BridgedInputNode {
            devnode: format!("/dev/input/event{index}"),
            devpath: format!("{PORT}/3-1:1.0/0003:056A:0317.0001/input/input{index}/event{index}"),
            role,
            name: "Wacom Intuos5 touch L Pen".to_owned(),
            kernel_driver: driver.to_owned(),
        }
    }

    #[test]
    fn an_x11_desktop_takes_a_tablet_whose_pen_the_kernel_wacom_driver_serves() {
        let pen = node(9, BridgedNodeRole::Pen, "wacom");
        let pad = node(10, BridgedNodeRole::Pad, "wacom");
        assert!(x11_hotplug_capable(&[pen.clone(), pad.clone()], true));
        assert!(!x11_hotplug_capable(&[pen], false));
        assert!(!x11_hotplug_capable(&[pad], true));
        assert!(!x11_hotplug_capable(
            &[node(9, BridgedNodeRole::Pen, "hid-generic")],
            true
        ));
    }

    /// The kernel's console keyboard handler binds any device with
    /// keyboard keys; a bridged tablet on an X11 desktop stays on seat0, so
    /// keyboard-capable interfaces are refused there.
    #[test]
    fn keyboard_interfaces_and_keyboard_keys_are_recognized() {
        use crate::descriptor_fixtures::BOOT_KEYBOARD;
        assert!(declares_keyboard(BOOT_KEYBOARD));
        for descriptor in [
            crate::INTUOS5_PEN_REPORT_DESCRIPTOR,
            crate::INTUOS5_TOUCH_REPORT_DESCRIPTOR,
            crate::INTUOS5_MOUSE_REPORT_DESCRIPTOR,
        ] {
            assert!(!declares_keyboard(descriptor));
        }
        assert!(declares_keyboard(&[0xff]));
        // hid-input maps keys per field, whatever the collection: keyboard
        // usages inside a Digitizer collection, Consumer usages, Generic
        // Desktop System Control (power, sleep) and nested Keyboard
        // collections are all refused; buttons and axes are not.
        let digitizer_with = |page: u8, usages: &[u8]| {
            let mut descriptor = vec![0x05, 0x0D, 0x09, 0x02, 0xA1, 0x01, 0x05, page];
            descriptor.extend_from_slice(usages);
            descriptor.extend_from_slice(&[0x75, 0x01, 0x95, 0x01, 0x81, 0x02, 0xC0]);
            descriptor
        };
        assert!(declares_keyboard(&digitizer_with(0x07, &[0x09, 0x46])));
        assert!(declares_keyboard(&digitizer_with(
            0x07,
            &[0x19, 0x00, 0x29, 0xFF]
        )));
        assert!(declares_keyboard(&digitizer_with(0x0C, &[0x09, 0xE9])));
        assert!(declares_keyboard(&digitizer_with(0x01, &[0x09, 0x81])));
        assert!(declares_keyboard(&digitizer_with(
            0x01,
            &[0x19, 0x30, 0x29, 0x81]
        )));
        assert!(!declares_keyboard(&digitizer_with(
            0x09,
            &[0x19, 0x01, 0x29, 0x10]
        )));
        assert!(!declares_keyboard(&digitizer_with(
            0x01,
            &[0x09, 0x30, 0x09, 0x31, 0x09, 0x38]
        )));
        assert!(!declares_keyboard(&digitizer_with(
            0x0D,
            &[0x09, 0x42, 0x09, 0x30]
        )));
        let vendor = [
            0x06, 0x00, 0xFF, 0x09, 0x01, 0xA1, 0x01, 0x09, 0x01, 0x75, 0x08, 0x95, 0x01, 0x81,
            0x02, 0xC0,
        ];
        assert!(!declares_keyboard(&vendor));
        // An extended usage names its own page.
        let extended = [
            0x05, 0x0D, 0x09, 0x02, 0xA1, 0x01, 0x0B, 0x46, 0x00, 0x07, 0x00, 0x75, 0x01, 0x95,
            0x01, 0x81, 0x02, 0xC0,
        ];
        assert!(declares_keyboard(&extended));
        // A Keyboard collection nested in a Digitizer application.
        let nested = [
            0x05, 0x0D, 0x09, 0x02, 0xA1, 0x01, 0x05, 0x01, 0x09, 0x06, 0xA1, 0x02, 0x05, 0x07,
            0x09, 0x04, 0x75, 0x01, 0x95, 0x01, 0x81, 0x02, 0xC0, 0xC0,
        ];
        assert!(declares_keyboard(&nested));
        // Push/pop restores the page.
        let pushed = [
            0x05, 0x0D, 0x09, 0x02, 0xA1, 0x01, 0xA4, 0x05, 0x09, 0xB4, 0x09, 0x46, 0x75, 0x01,
            0x95, 0x01, 0x81, 0x02, 0xC0,
        ];
        assert!(!declares_keyboard(&pushed));
        assert!(declares_keyboard(&[]));
        assert!(declares_keyboard(&[0x05]));
        // The Intuos5's pen, pad and touch nodes on Rocky 9 (`B: KEY=`):
        // buttons only, no console keyboard handler.
        for buttons in [
            "1cdf 1f0000 0 0 0 0",
            "800 1ff 0 0 0 0",
            "e520 0 0 0 0 0",
            "0",
        ] {
            assert!(
                !key_bitmap_reaches_keyboard_handler(buttons, 64),
                "{buttons}"
            );
        }
        // KEY_ESC (1), KEY_SYSRQ (99), KEY_BRL_DOT1 (0x1f1).
        assert!(key_bitmap_reaches_keyboard_handler("2", 64));
        assert!(key_bitmap_reaches_keyboard_handler("0 800000000", 64));
        assert!(key_bitmap_reaches_keyboard_handler(
            "2000000000000 0 0 0 0 0 0 0",
            64
        ));
        assert!(key_bitmap_reaches_keyboard_handler("2", 32));
        assert!(key_bitmap_reaches_keyboard_handler("zz", 64));
        assert!(key_bitmap_reaches_keyboard_handler("", 64));
    }

    /// Linux pages usages at each Main item (`hid_concatenate_last_usage_page`)
    /// and maps some vendor pages to keyboard keys: the check follows both.
    #[test]
    fn usages_are_paged_as_linux_pages_them_and_only_ignored_vendor_pages_pass() {
        // Linux re-pages short usages to the page in effect at the Main
        // item: Button 0x46 declared, then Usage Page Keyboard, is
        // KEY_SYSRQ. Refused (re-paged, and the fail-safe).
        let repaged = [
            0x05, 0x09, 0x09, 0x46, 0x05, 0x07, 0x75, 0x01, 0x95, 0x01, 0x81, 0x02,
        ];
        assert!(declares_keyboard(&repaged));
        let repaged_range = [0x05, 0x09, 0x19, 0x00, 0x29, 0xFF, 0x05, 0x07, 0x81, 0x02];
        assert!(declares_keyboard(&repaged_range));
        // The re-paging itself, without the fail-safe: a page changed by
        // a Pop counts too.
        let popped = [
            0x05, 0x07, 0xA4, 0x05, 0x09, 0x09, 0x46, 0xB4, 0x75, 0x01, 0x95, 0x01, 0x81, 0x02,
        ];
        assert!(declares_keyboard(&popped));
        assert_eq!(
            hid_input_usages(&[
                0x05, 0x09, 0x09, 0x46, 0x0B, 0x01, 0x00, 0x0D, 0x00, 0x81, 0x02
            ])
            .map(|usages| usages
                .iter()
                .map(|usage| (usage.page, usage.low))
                .collect::<Vec<_>>()),
            Some(vec![(0x09, 0x46), (0x0D, 0x01)]),
            "an extended usage keeps its page, a short one its declared page"
        );
        // Vendor pages hid-input maps to keyboard keys are refused; the
        // ones it ignores or maps to BTN_MISC/ABS_MISC are not.
        for vendor_page in [[0x01, 0xFF], [0x7F, 0xFF], [0x31, 0xFF], [0xFF, 0x00]] {
            let descriptor = [
                0x06,
                vendor_page[0],
                vendor_page[1],
                0x09,
                0x01,
                0x75,
                0x01,
                0x95,
                0x01,
                0x81,
                0x02,
            ];
            assert!(declares_keyboard(&descriptor), "{vendor_page:02x?}");
        }
        for vendor_page in [[0x00, 0xFF], [0x0D, 0xFF]] {
            let descriptor = [
                0x06,
                vendor_page[0],
                vendor_page[1],
                0x09,
                0x01,
                0x75,
                0x01,
                0x95,
                0x01,
                0x81,
                0x02,
            ];
            assert!(!declares_keyboard(&descriptor), "{vendor_page:02x?}");
        }
        // The Intuos5 declares no usage page between a usage and its Main
        // item (the fail-safe would refuse it otherwise).
        for descriptor in [
            crate::INTUOS5_PEN_REPORT_DESCRIPTOR,
            crate::INTUOS5_TOUCH_REPORT_DESCRIPTOR,
            crate::INTUOS5_MOUSE_REPORT_DESCRIPTOR,
        ] {
            assert!(hid_input_usages(descriptor).is_some());
        }
    }

    #[test]
    fn nodes_must_be_event_nodes_under_their_port() {
        assert_eq!(node(9, BridgedNodeRole::Pen, "").validate(PORT), Ok(()));
        let mut outside = node(11, BridgedNodeRole::Pen, "");
        outside.devpath =
            "/devices/platform/vhci_hcd.0/usb3/3-2/3-2:1.0/input/input11/event11".into();
        assert!(outside.validate(PORT).is_err());
        let mut mismatch = node(12, BridgedNodeRole::Pen, "");
        mismatch.devnode = "/dev/input/event13".into();
        assert!(mismatch.validate(PORT).is_err());
        let mut not_event = node(14, BridgedNodeRole::Pen, "");
        not_event.devnode = "/dev/hidraw0".into();
        assert!(not_event.validate(PORT).is_err());
    }

    #[test]
    fn roles_come_from_udev() {
        assert_eq!(
            BridgedNodeRole::from_udev_database("E:ID_INPUT=1\nE:ID_INPUT_TABLET=1\n"),
            BridgedNodeRole::Pen
        );
        assert_eq!(
            BridgedNodeRole::from_udev_database("E:ID_INPUT_TABLET=1\nE:ID_INPUT_TABLET_PAD=1\n"),
            BridgedNodeRole::Pad
        );
        assert_eq!(
            BridgedNodeRole::from_udev_database("E:ID_INPUT_TOUCHPAD=1\n"),
            BridgedNodeRole::Touch
        );
        // A Wacom "Finger" node is tagged as both.
        assert_eq!(
            BridgedNodeRole::from_udev_database("E:ID_INPUT_TABLET=1\nE:ID_INPUT_TOUCHPAD=1\n"),
            BridgedNodeRole::Touch
        );
        assert_eq!(
            BridgedNodeRole::from_udev_database("E:ID_INPUT_MOUSE=1\n"),
            BridgedNodeRole::Other
        );
    }
}
