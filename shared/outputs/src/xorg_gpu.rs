//! Which GPU a dedicated NVIDIA Xorg server is told to drive.
//!
//! An NVIDIA `Device` section names its GPU with `BusID "PCI:<bus>:<device>:<function>"`.
//! A stale or wrong value is fatal and quiet: the driver logs
//! `(EE) No devices detected.`, Xorg reports `no screens found`, and every
//! session on the host fails before a pixel exists. The value is a fact about
//! one machine — it changes between hosts, between hypervisor slots, and when a
//! GPU moves — so it cannot ship in a template.
//!
//! This module owns the portable half of resolving it. A Linux adapter lists
//! PCI functions (sysfs `vendor`, `class`, and bound driver) and hands them
//! here as [`GpuCandidate`]s; [`decide_xorg_gpu`] says whether the template's
//! `BusID` already names a usable NVIDIA GPU, which GPU to name instead, or
//! that there is none; [`apply_bus_id`] rewrites exactly that one line.
//!
//! Address forms differ and are easy to confuse: sysfs and `lspci` print
//! `DDDD:BB:DD.F` in **hexadecimal**, while Xorg's `BusID` is **decimal** and
//! carries a non-zero domain as `PCI:<bus>@<domain>:<device>:<function>`.
//! `0000:00:10.0` is therefore `PCI:0:16:0`.
//!
//! No I/O happens here.

use core::fmt;

/// PCI vendor ID of NVIDIA Corporation.
pub const NVIDIA_PCI_VENDOR: u16 = 0x10de;

/// PCI base class for display controllers.
const DISPLAY_BASE_CLASS: u8 = 0x03;
/// Display subclasses a GPU an X server can drive is published under:
/// VGA-compatible (`0x0300`) and 3D controller (`0x0302`, every headless
/// data-centre and vGPU board).
const DISPLAY_SUBCLASSES: [u8; 2] = [0x00, 0x02];

/// Kernel driver name an NVIDIA Xorg server needs bound to the GPU.
const NVIDIA_KERNEL_DRIVER: &str = "nvidia";

/// The address of one PCI function.
///
/// Ordering is numeric by domain, bus, device, then function, which is the
/// order a host enumerates them in and the order [`decide_xorg_gpu`] uses to
/// choose deterministically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PciAddress {
    pub domain: u32,
    pub bus: u8,
    pub device: u8,
    pub function: u8,
}

/// Why a PCI address string was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PciAddressError {
    /// Not in the expected `DDDD:BB:DD.F` (sysfs) or `PCI:B[@D]:D:F` (Xorg) shape.
    Malformed(String),
    /// A field is outside its PCI range (device 0..=31, function 0..=7).
    OutOfRange(String),
}

impl fmt::Display for PciAddressError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed(text) => write!(f, "malformed PCI address {text:?}"),
            Self::OutOfRange(text) => write!(f, "PCI address {text:?} is out of range"),
        }
    }
}

impl std::error::Error for PciAddressError {}

impl PciAddress {
    const MAX_DEVICE: u8 = 31;
    const MAX_FUNCTION: u8 = 7;

    /// Builds an address, refusing a device or function PCI cannot express.
    ///
    /// # Errors
    ///
    /// [`PciAddressError::OutOfRange`] when `device > 31` or `function > 7`.
    pub fn new(domain: u32, bus: u8, device: u8, function: u8) -> Result<Self, PciAddressError> {
        let address = Self {
            domain,
            bus,
            device,
            function,
        };
        if device > Self::MAX_DEVICE || function > Self::MAX_FUNCTION {
            return Err(PciAddressError::OutOfRange(address.to_string()));
        }
        Ok(address)
    }

    /// Parses the hexadecimal form sysfs and `lspci -D` print, `DDDD:BB:DD.F`.
    /// The domain may be omitted (`BB:DD.F`, plain `lspci`), meaning domain 0.
    ///
    /// # Errors
    ///
    /// [`PciAddressError`] for anything else, including surrounding text.
    pub fn parse_sysfs(text: &str) -> Result<Self, PciAddressError> {
        let malformed = || PciAddressError::Malformed(text.to_owned());
        let (head, function) = text.rsplit_once('.').ok_or_else(malformed)?;
        let mut fields = head.rsplit(':');
        let device = fields.next().ok_or_else(malformed)?;
        let bus = fields.next().ok_or_else(malformed)?;
        let domain = fields.next();
        if fields.next().is_some() {
            return Err(malformed());
        }
        let hex = |field: &str, max_digits: usize| {
            if field.is_empty()
                || field.len() > max_digits
                || !field.bytes().all(|byte| byte.is_ascii_hexdigit())
            {
                return Err(malformed());
            }
            u32::from_str_radix(field, 16).map_err(|_| malformed())
        };
        let domain = domain.map_or(Ok(0), |field| hex(field, 8))?;
        let bus = hex(bus, 2)?;
        let device = hex(device, 2)?;
        let function = hex(function, 1)?;
        Self::from_wide(domain, bus, device, function)
            .map_err(|_| PciAddressError::OutOfRange(text.to_owned()))
    }

    /// Parses an Xorg `BusID` value: `PCI:<bus>[@<domain>]:<device>[:<function>]`,
    /// decimal, with the function defaulting to 0 as Xorg does. The `PCI`
    /// prefix is matched case-insensitively.
    ///
    /// # Errors
    ///
    /// [`PciAddressError`] for any other text, including placeholders such as
    /// `auto`.
    pub fn parse_xorg_bus_id(text: &str) -> Result<Self, PciAddressError> {
        let malformed = || PciAddressError::Malformed(text.to_owned());
        let (prefix, rest) = text.split_once(':').ok_or_else(malformed)?;
        if !prefix.eq_ignore_ascii_case("PCI") {
            return Err(malformed());
        }
        let mut fields = rest.split(':');
        let bus_field = fields.next().ok_or_else(malformed)?;
        let device = fields.next().ok_or_else(malformed)?;
        let function = fields.next();
        if fields.next().is_some() {
            return Err(malformed());
        }
        let (bus, domain) = match bus_field.split_once('@') {
            Some((bus, domain)) => (bus, Some(domain)),
            None => (bus_field, None),
        };
        let decimal = |field: &str| {
            if field.is_empty() || field.len() > 10 || !field.bytes().all(|b| b.is_ascii_digit()) {
                return Err(malformed());
            }
            field.parse::<u32>().map_err(|_| malformed())
        };
        let domain = domain.map_or(Ok(0), decimal)?;
        let bus = decimal(bus)?;
        let device = decimal(device)?;
        let function = function.map_or(Ok(0), decimal)?;
        Self::from_wide(domain, bus, device, function)
            .map_err(|_| PciAddressError::OutOfRange(text.to_owned()))
    }

    fn from_wide(
        domain: u32,
        bus: u32,
        device: u32,
        function: u32,
    ) -> Result<Self, PciAddressError> {
        let narrow = |value: u32| {
            u8::try_from(value).map_err(|_| PciAddressError::OutOfRange(String::new()))
        };
        Self::new(domain, narrow(bus)?, narrow(device)?, narrow(function)?)
    }

    /// The Xorg `BusID` value for this address, in decimal:
    /// `PCI:<bus>:<device>:<function>`, or `PCI:<bus>@<domain>:<device>:<function>`
    /// when the domain is not 0.
    #[must_use]
    pub fn xorg_bus_id(&self) -> String {
        if self.domain == 0 {
            format!("PCI:{}:{}:{}", self.bus, self.device, self.function)
        } else {
            format!(
                "PCI:{}@{}:{}:{}",
                self.bus, self.domain, self.device, self.function
            )
        }
    }
}

/// Formats in the sysfs/`lspci -D` form, `0000:00:10.0`.
impl fmt::Display for PciAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:04x}:{:02x}:{:02x}.{:x}",
            self.domain, self.bus, self.device, self.function
        )
    }
}

/// The kernel driver bound to a PCI function, as far as an X server cares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BoundDriver {
    /// The NVIDIA kernel driver: the GPU is ready for an NVIDIA X server.
    Nvidia,
    /// Nothing bound — typically the NVIDIA module is not loaded yet.
    Unbound,
    /// Some other driver, such as `vfio-pci` (the GPU is passed through to a
    /// guest) or `nouveau`. An NVIDIA X server cannot use it.
    Other(String),
}

impl BoundDriver {
    /// Classifies the name of a sysfs `driver` link target, or its absence.
    #[must_use]
    pub fn from_name(name: Option<&str>) -> Self {
        match name.map(str::trim) {
            None | Some("") => Self::Unbound,
            Some(NVIDIA_KERNEL_DRIVER) => Self::Nvidia,
            Some(other) => Self::Other(other.to_owned()),
        }
    }
}

/// One PCI function as a host reported it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuCandidate {
    pub address: PciAddress,
    /// PCI vendor ID.
    pub vendor: u16,
    /// 24-bit PCI class code, `0xCCSSPP` (class, subclass, programming interface).
    pub class: u32,
    pub driver: BoundDriver,
}

impl GpuCandidate {
    /// Builds a candidate from the raw text of a sysfs PCI device directory:
    /// its name (`0000:00:10.0`), `vendor` (`0x10de`), `class` (`0x030000`),
    /// and the file name of its `driver` link, if it has one.
    ///
    /// Returns `None` for anything malformed; a host skips such entries.
    #[must_use]
    pub fn from_sysfs(name: &str, vendor: &str, class: &str, driver: Option<&str>) -> Option<Self> {
        let address = PciAddress::parse_sysfs(name.trim()).ok()?;
        let vendor = parse_sysfs_hex(vendor)?;
        let class = parse_sysfs_hex(class)?;
        Some(Self {
            address,
            vendor: u16::try_from(vendor).ok()?,
            class: (class <= 0x00ff_ffff).then_some(class)?,
            driver: BoundDriver::from_name(driver),
        })
    }

    /// True for an NVIDIA VGA or 3D display controller — the only kind of
    /// PCI function an NVIDIA Xorg `Device` section can name.
    #[must_use]
    pub fn is_nvidia_display(&self) -> bool {
        let base = (self.class >> 16) & 0xff;
        let sub = (self.class >> 8) & 0xff;
        self.vendor == NVIDIA_PCI_VENDOR
            && base == u32::from(DISPLAY_BASE_CLASS)
            && DISPLAY_SUBCLASSES.iter().any(|&s| u32::from(s) == sub)
    }

    /// An NVIDIA display controller not claimed by another kernel driver.
    #[must_use]
    pub fn is_usable_nvidia_gpu(&self) -> bool {
        self.is_nvidia_display() && !matches!(self.driver, BoundDriver::Other(_))
    }
}

fn parse_sysfs_hex(text: &str) -> Option<u32> {
    let text = text.trim();
    let digits = text
        .strip_prefix("0x")
        .or_else(|| text.strip_prefix("0X"))
        .unwrap_or(text);
    if digits.is_empty() || digits.len() > 8 {
        return None;
    }
    u32::from_str_radix(digits, 16).ok()
}

/// Why a template cannot be resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum XorgTemplateError {
    /// The template must have exactly one `Device` section with `Driver "nvidia"`.
    NvidiaDeviceSections(usize),
    /// The NVIDIA `Device` section names more than one `BusID`.
    DuplicateBusId,
    /// A `BusID` line whose value is not one quoted string.
    MalformedBusIdLine,
}

impl fmt::Display for XorgTemplateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NvidiaDeviceSections(count) => write!(
                f,
                "expected exactly one Device section with Driver \"nvidia\", found {count}"
            ),
            Self::DuplicateBusId => {
                f.write_str("the NVIDIA Device section has more than one BusID line")
            }
            Self::MalformedBusIdLine => {
                f.write_str("the NVIDIA Device section has a BusID line without a quoted value")
            }
        }
    }
}

impl std::error::Error for XorgTemplateError {}

/// What to do about a template's NVIDIA `BusID`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum XorgGpuDecision {
    /// The template already names a usable NVIDIA GPU. `bus_id` is the value
    /// exactly as written.
    Keep { bus_id: String, address: PciAddress },
    /// The template's `BusID` is missing, a placeholder, or names no usable
    /// NVIDIA GPU on this host; it must become `bus_id`.
    Set {
        /// The value the template had, if any line was present.
        previous: Option<String>,
        bus_id: String,
        address: PciAddress,
        /// Every other usable NVIDIA GPU, in preference order, so a caller can
        /// tell the operator what was not chosen.
        others: Vec<PciAddress>,
    },
    /// The host has no usable NVIDIA display GPU. Nothing should change.
    NoNvidiaGpu { previous: Option<String> },
}

/// Decides which GPU the template's NVIDIA `Device` section must name.
///
/// A `BusID` that parses and names a usable candidate is kept, even when it is
/// not the first one: that is how an operator picks a GPU. Otherwise the choice
/// is the first usable NVIDIA display GPU, preferring one the NVIDIA kernel
/// driver already owns, then the lowest PCI address. A GPU bound to another
/// driver (`vfio-pci`, `nouveau`) and non-NVIDIA display devices (QXL,
/// virtio-gpu, a BMC's VGA) are never chosen.
///
/// # Errors
///
/// [`XorgTemplateError`] when the template does not have exactly one NVIDIA
/// `Device` section or its `BusID` lines are ambiguous.
pub fn decide_xorg_gpu(
    template: &str,
    candidates: &[GpuCandidate],
) -> Result<XorgGpuDecision, XorgTemplateError> {
    let previous = locate_nvidia_device(template)?.bus_id;
    let mut usable: Vec<&GpuCandidate> = candidates
        .iter()
        .filter(|candidate| candidate.is_usable_nvidia_gpu())
        .collect();
    usable.sort_by_key(|candidate| (candidate.driver != BoundDriver::Nvidia, candidate.address));
    usable.dedup_by_key(|candidate| candidate.address);

    let Some(chosen) = usable.first() else {
        return Ok(XorgGpuDecision::NoNvidiaGpu { previous });
    };
    if let Some(current) = previous.as_deref() {
        if let Ok(address) = PciAddress::parse_xorg_bus_id(current.trim()) {
            if usable.iter().any(|candidate| candidate.address == address) {
                return Ok(XorgGpuDecision::Keep {
                    bus_id: current.to_owned(),
                    address,
                });
            }
        }
    }
    Ok(XorgGpuDecision::Set {
        previous,
        bus_id: chosen.address.xorg_bus_id(),
        address: chosen.address,
        others: usable[1..]
            .iter()
            .map(|candidate| candidate.address)
            .collect(),
    })
}

/// Names `address` as the NVIDIA `Device` section's `BusID`.
///
/// An existing `BusID` line keeps its indentation, keyword spelling, spacing,
/// and trailing comment; only the quoted value changes. Without one, a line is
/// inserted directly after the section's `Driver "nvidia"` line, indented and
/// aligned like it. Every other byte, including other `Device` sections and
/// line endings, is unchanged.
///
/// # Errors
///
/// [`XorgTemplateError`] as for [`decide_xorg_gpu`].
pub fn apply_bus_id(template: &str, address: PciAddress) -> Result<String, XorgTemplateError> {
    let device = locate_nvidia_device(template)?;
    let bus_id = address.xorg_bus_id();
    let mut out = String::with_capacity(template.len() + bus_id.len() + 32);
    for (index, line) in template.split_inclusive('\n').enumerate() {
        match device.bus_id_line {
            Some(target) if target == index => {
                let (open, close) =
                    quoted_value_span(line).ok_or(XorgTemplateError::MalformedBusIdLine)?;
                out.push_str(&line[..=open]);
                out.push_str(&bus_id);
                out.push_str(&line[close..]);
            }
            None if index == device.driver_line => {
                out.push_str(line);
                if !line.ends_with('\n') {
                    out.push('\n');
                }
                out.push_str(&inserted_bus_id_line(line, &bus_id));
            }
            _ => out.push_str(line),
        }
    }
    Ok(out)
}

/// Decides and, for [`XorgGpuDecision::Set`], applies — the in-memory form a
/// host uses before rendering a session configuration.
///
/// # Errors
///
/// [`XorgTemplateError`] as for [`decide_xorg_gpu`].
pub fn resolve_xorg_gpu(
    template: &str,
    candidates: &[GpuCandidate],
) -> Result<(String, XorgGpuDecision), XorgTemplateError> {
    let decision = decide_xorg_gpu(template, candidates)?;
    let resolved = match &decision {
        XorgGpuDecision::Set { address, .. } => apply_bus_id(template, *address)?,
        XorgGpuDecision::Keep { .. } | XorgGpuDecision::NoNvidiaGpu { .. } => template.to_owned(),
    };
    Ok((resolved, decision))
}

/// Where the single NVIDIA `Device` section's relevant lines are, by index in
/// `template.split_inclusive('\n')`.
struct NvidiaDeviceLines {
    driver_line: usize,
    bus_id_line: Option<usize>,
    bus_id: Option<String>,
}

/// A `BusID` line's index and its quoted value, if it had one.
type BusIdLine = (usize, Option<String>);

#[derive(Default)]
struct DeviceSection {
    nvidia_driver_line: Option<usize>,
    bus_id_lines: Vec<BusIdLine>,
}

fn locate_nvidia_device(template: &str) -> Result<NvidiaDeviceLines, XorgTemplateError> {
    let mut sections: Vec<DeviceSection> = Vec::new();
    let mut current: Option<DeviceSection> = None;
    // Depth of nested `SubSection`s inside the current section.
    let mut nested = 0usize;
    for (index, line) in template.split_inclusive('\n').enumerate() {
        let Some((keyword, value)) = directive(line) else {
            continue;
        };
        if keyword_is(keyword, "Section") {
            sections.extend(current.take());
            if value
                .as_deref()
                .is_some_and(|name| keyword_is(name, "Device"))
            {
                current = Some(DeviceSection::default());
            }
            nested = 0;
        } else if keyword_is(keyword, "SubSection") {
            nested += 1;
        } else if keyword_is(keyword, "EndSubSection") {
            nested = nested.saturating_sub(1);
        } else if keyword_is(keyword, "EndSection") {
            sections.extend(current.take());
        } else if let Some(section) = current.as_mut().filter(|_| nested == 0) {
            if keyword_is(keyword, "Driver")
                && value
                    .as_deref()
                    .is_some_and(|driver| driver.eq_ignore_ascii_case("nvidia"))
            {
                section.nvidia_driver_line = Some(index);
            } else if keyword_is(keyword, "BusID") {
                section.bus_id_lines.push((index, value));
            }
        }
    }
    sections.extend(current);
    let nvidia: Vec<(usize, &[BusIdLine])> = sections
        .iter()
        .filter_map(|section| {
            section
                .nvidia_driver_line
                .map(|line| (line, section.bus_id_lines.as_slice()))
        })
        .collect();
    let [(driver_line, bus_id_lines)] = nvidia.as_slice() else {
        return Err(XorgTemplateError::NvidiaDeviceSections(nvidia.len()));
    };
    let driver_line = *driver_line;
    match bus_id_lines {
        [] => Ok(NvidiaDeviceLines {
            driver_line,
            bus_id_line: None,
            bus_id: None,
        }),
        [(line, Some(value))] => Ok(NvidiaDeviceLines {
            driver_line,
            bus_id_line: Some(*line),
            bus_id: Some(value.clone()),
        }),
        [(_, None)] => Err(XorgTemplateError::MalformedBusIdLine),
        _ => Err(XorgTemplateError::DuplicateBusId),
    }
}

/// Splits a configuration line into its keyword and first quoted value,
/// ignoring comments. `None` for blank and comment-only lines.
fn directive(line: &str) -> Option<(&str, Option<String>)> {
    let code = strip_comment(line).trim();
    if code.is_empty() {
        return None;
    }
    let keyword_end = code
        .find(|c: char| c.is_whitespace() || c == '"')
        .unwrap_or(code.len());
    let keyword = &code[..keyword_end];
    let value = quoted_value_span(code).map(|(open, close)| code[open + 1..close].to_owned());
    Some((keyword, value))
}

/// The line up to a `#` that is not inside a quoted string.
fn strip_comment(line: &str) -> &str {
    let mut quoted = false;
    for (index, c) in line.char_indices() {
        match c {
            '"' => quoted = !quoted,
            '#' if !quoted => return &line[..index],
            _ => {}
        }
    }
    line
}

/// Byte offsets of the first quoted value's opening and closing quotes,
/// outside any comment.
fn quoted_value_span(line: &str) -> Option<(usize, usize)> {
    let code = strip_comment(line);
    let open = code.find('"')?;
    let close = open + 1 + code[open + 1..].find('"')?;
    Some((open, close))
}

/// Xorg compares keywords ignoring case and underscores.
fn keyword_is(found: &str, expected: &str) -> bool {
    let mut found = found.chars().filter(|&c| c != '_');
    let mut expected = expected.chars();
    loop {
        match (found.next(), expected.next()) {
            (None, None) => return true,
            (Some(a), Some(b)) if a.eq_ignore_ascii_case(&b) => {}
            _ => return false,
        }
    }
}

/// A `BusID` line shaped like the `Driver` line it follows: same indentation,
/// value in the same column, same line ending.
fn inserted_bus_id_line(driver_line: &str, bus_id: &str) -> String {
    const KEYWORD: &str = "BusID";
    let indent_len = driver_line.len() - driver_line.trim_start().len();
    let indent = &driver_line[..indent_len];
    let value_column = quoted_value_span(driver_line).map_or(0, |(open, _)| open);
    let padding = value_column
        .saturating_sub(indent_len + KEYWORD.len())
        .max(1);
    let ending = if driver_line.ends_with("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    format!(
        "{indent}{KEYWORD}{}\"{bus_id}\"{ending}",
        " ".repeat(padding)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn address(text: &str) -> PciAddress {
        PciAddress::parse_sysfs(text).unwrap()
    }

    fn gpu(name: &str, vendor: &str, class: &str, driver: Option<&str>) -> GpuCandidate {
        GpuCandidate::from_sysfs(name, vendor, class, driver).unwrap()
    }

    fn nvidia(name: &str) -> GpuCandidate {
        gpu(name, "0x10de\n", "0x030000\n", Some("nvidia"))
    }

    const STOCK_DEVICE: &str = "Section \"Device\"\n    Identifier     \"Device0\"\n    Driver         \"nvidia\"\n    BusID          \"PCI:6:16:0\"\n    Option         \"ConnectedMonitor\" \"DFP-0\"\nEndSection\n";

    const UNSET_DEVICE: &str = "Section \"Device\"\n    Identifier     \"Device0\"\n    Driver         \"nvidia\"\n    # BusID is filled in by the installer.\n    Option         \"ConnectedMonitor\" \"DFP-0\"\nEndSection\n";

    #[test]
    fn sysfs_addresses_are_hexadecimal() {
        let parsed = address("0000:00:10.0");
        assert_eq!(
            parsed,
            PciAddress {
                domain: 0,
                bus: 0,
                device: 16,
                function: 0
            }
        );
        assert_eq!(parsed.to_string(), "0000:00:10.0");
        assert_eq!(address("0000:3b:1f.7").bus, 0x3b);
        assert_eq!(address("0a:00.1"), PciAddress::new(0, 10, 0, 1).unwrap());
        assert_eq!(address("10000:e1:00.0").domain, 0x10000);
    }

    #[test]
    fn malformed_sysfs_addresses_are_refused() {
        for text in [
            "",
            "0000:00:10",
            "0000:00.10.0",
            "0000:00:10.8",
            "0000:00:20.0",
            "0000:000:10.0",
            "0000:00:10.0 ",
            "x000:00:10.0",
            "1:0000:00:10.0",
            "0000:00:10.",
        ] {
            assert!(PciAddress::parse_sysfs(text).is_err(), "{text:?} accepted");
        }
    }

    #[test]
    fn xorg_bus_ids_are_decimal_with_an_optional_domain() {
        assert_eq!(address("0000:00:10.0").xorg_bus_id(), "PCI:0:16:0");
        assert_eq!(address("0000:06:10.0").xorg_bus_id(), "PCI:6:16:0");
        assert_eq!(address("0000:af:00.1").xorg_bus_id(), "PCI:175:0:1");
        assert_eq!(address("0002:3b:00.0").xorg_bus_id(), "PCI:59@2:0:0");
        for text in [
            "0000:00:10.0",
            "0000:af:1f.7",
            "0002:3b:00.0",
            "10000:e1:00.0",
        ] {
            let address = address(text);
            assert_eq!(
                PciAddress::parse_xorg_bus_id(&address.xorg_bus_id()),
                Ok(address)
            );
        }
    }

    #[test]
    fn xorg_bus_id_parsing_follows_xorg_and_rejects_garbage() {
        assert_eq!(
            PciAddress::parse_xorg_bus_id("pci:0:16"),
            Ok(address("0000:00:10.0"))
        );
        assert_eq!(
            PciAddress::parse_xorg_bus_id("PCI:59@2:0:0"),
            Ok(address("0002:3b:00.0"))
        );
        for text in [
            "auto",
            "",
            "PCI:",
            "PCI:6",
            "PCI:6:16:0:0",
            "PCI:x:16:0",
            "PCI:0x6:16:0",
            "PCI:256:0:0",
            "PCI:0:32:0",
            "PCI:0:0:8",
            "PCI:@1:0:0",
            "AGP:6:16:0",
            "PCI:-1:0:0",
        ] {
            assert!(
                PciAddress::parse_xorg_bus_id(text).is_err(),
                "{text:?} accepted"
            );
        }
    }

    #[test]
    fn candidates_parse_sysfs_text_and_classify_display_controllers() {
        assert!(nvidia("0000:00:10.0").is_nvidia_display());
        assert!(gpu("0000:3b:00.0", "0x10de", "0x030200", None).is_usable_nvidia_gpu());
        // NVIDIA HDMI audio function and an NVIDIA bridge are not GPUs.
        assert!(!gpu("0000:01:00.1", "0x10de", "0x040300", None).is_nvidia_display());
        assert!(!gpu("0000:00:00.0", "0x10de", "0x060000", None).is_nvidia_display());
        // "Other display controller" is not a subclass an X server drives.
        assert!(!gpu("0000:01:00.0", "0x10de", "0x038000", None).is_nvidia_display());
        // QXL and virtio VGA.
        assert!(!gpu("0000:00:02.0", "0x1b36", "0x030000", None).is_nvidia_display());
        assert!(!gpu("0000:00:01.0", "0x1af4", "0x030000", None).is_nvidia_display());
        // Passed through to a guest.
        let vfio = gpu("0000:3b:00.0", "0x10de", "0x030200", Some("vfio-pci"));
        assert!(vfio.is_nvidia_display() && !vfio.is_usable_nvidia_gpu());
        assert!(GpuCandidate::from_sysfs("bogus", "0x10de", "0x030000", None).is_none());
        assert!(GpuCandidate::from_sysfs("0000:00:10.0", "0x110de", "0x030000", None).is_none());
        assert!(GpuCandidate::from_sysfs("0000:00:10.0", "0x10de", "0x1030000", None).is_none());
    }

    #[test]
    fn a_stale_lab_bus_id_is_replaced_by_the_hosts_gpu() {
        let candidates = [
            gpu("0000:00:01.0", "0x1234", "0x030000", None),
            nvidia("0000:00:10.0"),
        ];
        let decision = decide_xorg_gpu(STOCK_DEVICE, &candidates).unwrap();
        assert_eq!(
            decision,
            XorgGpuDecision::Set {
                previous: Some("PCI:6:16:0".to_owned()),
                bus_id: "PCI:0:16:0".to_owned(),
                address: address("0000:00:10.0"),
                others: Vec::new(),
            }
        );
        let (resolved, _) = resolve_xorg_gpu(STOCK_DEVICE, &candidates).unwrap();
        assert_eq!(
            resolved,
            STOCK_DEVICE.replace("\"PCI:6:16:0\"", "\"PCI:0:16:0\"")
        );
    }

    #[test]
    fn a_bus_id_naming_a_present_nvidia_gpu_is_kept_as_written() {
        let template = STOCK_DEVICE.replace("PCI:6:16:0", "pci:0:16");
        let candidates = [nvidia("0000:00:10.0")];
        assert_eq!(
            decide_xorg_gpu(&template, &candidates).unwrap(),
            XorgGpuDecision::Keep {
                bus_id: "pci:0:16".to_owned(),
                address: address("0000:00:10.0"),
            }
        );
        let (resolved, _) = resolve_xorg_gpu(&template, &candidates).unwrap();
        assert_eq!(resolved, template);
    }

    #[test]
    fn an_operators_choice_of_a_second_gpu_is_kept() {
        let template = STOCK_DEVICE.replace("PCI:6:16:0", "PCI:175:0:0");
        let candidates = [nvidia("0000:3b:00.0"), nvidia("0000:af:00.0")];
        assert!(matches!(
            decide_xorg_gpu(&template, &candidates).unwrap(),
            XorgGpuDecision::Keep { address, .. } if address == super::PciAddress::new(0, 0xaf, 0, 0).unwrap()
        ));
    }

    #[test]
    fn a_missing_or_placeholder_bus_id_is_set() {
        let candidates = [nvidia("0000:00:10.0")];
        assert!(matches!(
            decide_xorg_gpu(UNSET_DEVICE, &candidates).unwrap(),
            XorgGpuDecision::Set { previous: None, ref bus_id, .. } if bus_id == "PCI:0:16:0"
        ));
        let placeholder = STOCK_DEVICE.replace("PCI:6:16:0", "auto");
        assert!(matches!(
            decide_xorg_gpu(&placeholder, &candidates).unwrap(),
            XorgGpuDecision::Set { previous: Some(ref previous), .. } if previous == "auto"
        ));
        // A BusID naming a non-NVIDIA device is stale too.
        let qxl = STOCK_DEVICE.replace("PCI:6:16:0", "PCI:0:2:0");
        let with_qxl = [
            gpu("0000:00:02.0", "0x1b36", "0x030000", None),
            nvidia("0000:00:10.0"),
        ];
        assert!(matches!(
            decide_xorg_gpu(&qxl, &with_qxl).unwrap(),
            XorgGpuDecision::Set { ref bus_id, .. } if bus_id == "PCI:0:16:0"
        ));
    }

    #[test]
    fn several_gpus_choose_deterministically_and_report_the_rest() {
        let candidates = [
            nvidia("0000:af:00.0"),
            gpu("0000:5e:00.0", "0x10de", "0x030200", None),
            nvidia("0000:3b:00.0"),
            gpu("0000:18:00.0", "0x10de", "0x030200", Some("vfio-pci")),
            gpu("0000:03:00.0", "0x1a03", "0x030000", Some("ast")),
            nvidia("0000:3b:00.0"),
        ];
        let expected = XorgGpuDecision::Set {
            previous: None,
            bus_id: "PCI:59:0:0".to_owned(),
            address: address("0000:3b:00.0"),
            // Driver-bound first, then the unbound one; vfio and ASPEED never.
            others: vec![address("0000:af:00.0"), address("0000:5e:00.0")],
        };
        assert_eq!(
            decide_xorg_gpu(UNSET_DEVICE, &candidates).unwrap(),
            expected
        );
        let mut reversed = candidates.to_vec();
        reversed.reverse();
        assert_eq!(decide_xorg_gpu(UNSET_DEVICE, &reversed).unwrap(), expected);
    }

    #[test]
    fn no_nvidia_gpu_changes_nothing() {
        let candidates = [
            gpu("0000:00:02.0", "0x1b36", "0x030000", Some("qxl")),
            gpu("0000:00:01.0", "0x1af4", "0x030000", Some("virtio-pci")),
            gpu("0000:3b:00.0", "0x10de", "0x030200", Some("vfio-pci")),
        ];
        assert_eq!(
            decide_xorg_gpu(STOCK_DEVICE, &candidates).unwrap(),
            XorgGpuDecision::NoNvidiaGpu {
                previous: Some("PCI:6:16:0".to_owned())
            }
        );
        let (resolved, _) = resolve_xorg_gpu(STOCK_DEVICE, &candidates).unwrap();
        assert_eq!(resolved, STOCK_DEVICE);
        assert_eq!(
            decide_xorg_gpu(UNSET_DEVICE, &[]).unwrap(),
            XorgGpuDecision::NoNvidiaGpu { previous: None }
        );
    }

    #[test]
    fn replacing_keeps_every_other_byte() {
        let template = "Section \"Device\"\r\n\tIdentifier \"Device0\"\r\n\tDriver \"nvidia\"\r\n\tBus_ID  \"PCI:6:16:0\"   # lab GPU\r\nEndSection\r\n";
        let resolved = apply_bus_id(template, address("0000:00:10.0")).unwrap();
        assert_eq!(
            resolved,
            "Section \"Device\"\r\n\tIdentifier \"Device0\"\r\n\tDriver \"nvidia\"\r\n\tBus_ID  \"PCI:0:16:0\"   # lab GPU\r\nEndSection\r\n"
        );
    }

    #[test]
    fn inserting_aligns_with_the_driver_line() {
        let resolved = apply_bus_id(UNSET_DEVICE, address("0002:3b:00.0")).unwrap();
        assert_eq!(
            resolved,
            UNSET_DEVICE.replace(
                "    Driver         \"nvidia\"\n",
                "    Driver         \"nvidia\"\n    BusID          \"PCI:59@2:0:0\"\n"
            )
        );
        let unterminated = "Section \"Device\"\n  Driver \"nvidia\"";
        assert_eq!(
            apply_bus_id(unterminated, address("0000:00:10.0")).unwrap(),
            "Section \"Device\"\n  Driver \"nvidia\"\n  BusID  \"PCI:0:16:0\"\n"
        );
    }

    #[test]
    fn only_the_nvidia_device_section_is_touched() {
        let template = "# BusID \"PCI:9:9:9\" in a comment\nSection \"Device\"\n    Identifier \"Intel\"\n    Driver     \"modesetting\"\n    BusID      \"PCI:0:2:0\"\nEndSection\n\nSection \"Device\"\n    Identifier \"Device0\"\n    Driver     \"nvidia\"\n    BusID      \"PCI:6:16:0\"\nEndSection\nSection \"Screen\"\n    Identifier \"Screen0\"\n    SubSection \"Display\"\n        Depth 30\n    EndSubSection\nEndSection\n";
        let candidates = [
            gpu("0000:00:02.0", "0x8086", "0x030000", Some("i915")),
            nvidia("0000:01:00.0"),
        ];
        let (resolved, decision) = resolve_xorg_gpu(template, &candidates).unwrap();
        assert!(
            matches!(decision, XorgGpuDecision::Set { previous: Some(ref p), .. } if p == "PCI:6:16:0")
        );
        assert_eq!(
            resolved,
            template.replace("\"PCI:6:16:0\"", "\"PCI:1:0:0\"")
        );
    }

    #[test]
    fn templates_without_exactly_one_nvidia_device_are_refused() {
        let none = "Section \"Device\"\n    Driver \"modesetting\"\nEndSection\n";
        assert_eq!(
            decide_xorg_gpu(none, &[]),
            Err(XorgTemplateError::NvidiaDeviceSections(0))
        );
        let two = format!("{STOCK_DEVICE}{UNSET_DEVICE}");
        assert_eq!(
            apply_bus_id(&two, address("0000:00:10.0")),
            Err(XorgTemplateError::NvidiaDeviceSections(2))
        );
        let commented = "Section \"Device\"\n    # Driver \"nvidia\"\nEndSection\n";
        assert_eq!(
            decide_xorg_gpu(commented, &[]),
            Err(XorgTemplateError::NvidiaDeviceSections(0))
        );
        let duplicate = STOCK_DEVICE.replace(
            "    Option ",
            "    BusID          \"PCI:0:16:0\"\n    Option ",
        );
        assert_eq!(
            decide_xorg_gpu(&duplicate, &[]),
            Err(XorgTemplateError::DuplicateBusId)
        );
        let bare = STOCK_DEVICE.replace("\"PCI:6:16:0\"", "PCI:6:16:0");
        assert_eq!(
            decide_xorg_gpu(&bare, &[]),
            Err(XorgTemplateError::MalformedBusIdLine)
        );
    }
}
