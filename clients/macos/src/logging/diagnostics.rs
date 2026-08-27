//! One-shot diagnostic snapshots for support & debugging.
//!
//! These answer the first questions a sysadmin asks when a session misbehaves:
//! *what version, what OS, what transport, what did the hostname resolve to,
//! and what USB/tablet hardware is on the client?* Everything here runs off the
//! UI/hot path (USB enumeration and DNS can block for tens of ms), so the
//! connect-time snapshot is gathered on a short-lived detached thread.

use std::net::{IpAddr, ToSocketAddrs};

use tracing::{debug, info};

use super::target::{SESSION, TRANSPORT, USB};

/// Emit the process-startup banner: versions + host environment. Cheap, safe to
/// call inline on the main thread.
///
/// The licence and source fields are deliberate: Arcen is AGPL-3.0 free
/// software, and keeping the source location in the running program means a
/// user who only ever sees a built binary can still find the corresponding
/// source. The Pier carries the same notice.
pub fn log_startup_banner() {
    info!(
        target: SESSION,
        version = env!("CARGO_PKG_VERSION"),
        license = "AGPL-3.0-only",
        source = crate::SOURCE_URL,
        protocol = %format!("{:?}", crate::protocol::PROTOCOL_VERSION),
        os = %os_description(),
        arch = std::env::consts::ARCH,
        host = %host_name(),
        "Arcen client starting"
    );
}

/// Everything the client knows about an outgoing connection attempt. Owned
/// (Send + 'static) so it can be handed to the diagnostics thread.
#[derive(Clone, Debug)]
pub struct SessionContext {
    pub host: String,
    pub port: u16,
    pub scheme: &'static str,
    pub tls: bool,
    pub security_mode: String,
    pub codec: String,
    pub chroma: String,
    pub max_fps: u32,
    pub authenticated: bool,
}

impl SessionContext {
    fn endpoint(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

/// Log the connect-time context banner (transport + requested profile) and the
/// USB inventory on a detached thread so DNS/USB latency never touches the UI.
pub fn spawn_connect_diagnostics(ctx: SessionContext) {
    let spawn = std::thread::Builder::new()
        .name("diagnostics".to_string())
        .spawn(move || {
            let resolved = resolve_addrs(&ctx.host, ctx.port);
            info!(
                target: TRANSPORT,
                endpoint = %ctx.endpoint(),
                transport = "websocket",
                scheme = ctx.scheme,
                tls = ctx.tls,
                security_mode = %ctx.security_mode,
                network_scope = network_scope(&ctx.host),
                resolved = %resolved,
                nat = "unknown (no STUN/gateway yet; direct connection)",
                "Connecting to host"
            );
            info!(
                target: SESSION,
                codec = %ctx.codec,
                chroma = %ctx.chroma,
                max_fps = ctx.max_fps,
                auth = if ctx.authenticated { "password" } else { "anonymous" },
                "Requested stream profile"
            );
            log_usb_inventory();
        });
    if spawn.is_err() {
        // Extremely unlikely; fall back to inline so we never silently skip it.
        log_usb_inventory();
    }
}

/// A USB device as seen by the client, for diagnostics.
#[derive(Clone, Debug)]
pub struct UsbDeviceInfo {
    pub vendor_id: u16,
    pub product_id: u16,
    pub vendor_name: Option<String>,
    pub product: Option<String>,
    pub serial: Option<String>,
    /// Firmware/device release, decoded from the BCD `bcdDevice` field.
    pub firmware: String,
    /// True if any interface advertises the HID class (0x03).
    pub is_hid: bool,
    /// Known graphics-tablet brand, if the vendor id is recognised.
    pub tablet_brand: Option<&'static str>,
}

/// Enumerate attached USB devices. Pure-Rust via `nusb` (no libusb/C dep).
/// Returns `Err` with the enumeration error so callers can distinguish "no
/// devices attached" from "USB subsystem/permission failure".
pub fn usb_inventory() -> Result<Vec<UsbDeviceInfo>, String> {
    let devices = nusb::list_devices().map_err(|error| error.to_string())?;
    Ok(devices
        .map(|device| {
            let is_hid = device.interfaces().any(|iface| iface.class() == 0x03);
            UsbDeviceInfo {
                vendor_id: device.vendor_id(),
                product_id: device.product_id(),
                vendor_name: device.manufacturer_string().map(str::to_string),
                product: device.product_string().map(str::to_string),
                serial: device.serial_number().map(str::to_string),
                firmware: bcd_version(device.device_version()),
                is_hid,
                tablet_brand: tablet_brand(device.vendor_id()),
            }
        })
        .collect())
}

/// Log the USB inventory: a summary line plus one line per device. Tablets are
/// highlighted at INFO (a VFX operator cares which pen is attached); the rest
/// sit at DEBUG so the Info level stays scannable. An enumeration failure is a
/// WARN so it is never mistaken for "no devices attached".
pub fn log_usb_inventory() {
    let devices = match usb_inventory() {
        Ok(devices) => devices,
        Err(error) => {
            tracing::warn!(
                target: USB,
                %error,
                "USB enumeration failed (subsystem or permissions?)"
            );
            return;
        }
    };
    let tablets = devices.iter().filter(|d| d.tablet_brand.is_some()).count();
    info!(
        target: USB,
        total = devices.len(),
        tablets,
        "USB inventory enumerated"
    );
    for device in &devices {
        let id = format!("{:04x}:{:04x}", device.vendor_id, device.product_id);
        let product = device.product.as_deref().unwrap_or("?");
        let vendor = device.vendor_name.as_deref().unwrap_or("?");
        if let Some(brand) = device.tablet_brand {
            info!(
                target: USB,
                brand,
                id = %id,
                product,
                firmware = %device.firmware,
                serial = device.serial.as_deref().unwrap_or("-"),
                hid = device.is_hid,
                "Graphics tablet attached"
            );
        } else {
            debug!(
                target: USB,
                id = %id,
                vendor,
                product,
                firmware = %device.firmware,
                hid = device.is_hid,
                "USB device"
            );
        }
    }
}

/// Decode a USB `bcdDevice` (BCD) release number, e.g. `0x0110` -> `"1.10"`.
fn bcd_version(raw: u16) -> String {
    format!("{:x}.{:02x}", (raw >> 8) & 0xff, raw & 0xff)
}

/// Recognise common graphics-tablet vendor ids so pens/tablets stand out.
fn tablet_brand(vendor_id: u16) -> Option<&'static str> {
    match vendor_id {
        0x056a => Some("Wacom"),
        0x256c => Some("Huion"),
        0x28bd => Some("XP-Pen"),
        0x5543 => Some("UC-Logic"),
        0x0b57 => Some("Gaomon"),
        _ => None,
    }
}

/// Classify the destination as private LAN, public WAN, or an unresolved
/// hostname — a quick hint for "is this a local test or a real remote?".
fn network_scope(host: &str) -> &'static str {
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) => {
            if ip.is_private() || ip.is_loopback() || ip.is_link_local() {
                "LAN/private"
            } else {
                "public/WAN"
            }
        }
        Ok(IpAddr::V6(ip)) => {
            let first = ip.segments()[0];
            let link_local = (first & 0xffc0) == 0xfe80;
            let unique_local = (first & 0xfe00) == 0xfc00;
            if ip.is_loopback() || link_local || unique_local {
                "LAN/private"
            } else {
                "public/WAN"
            }
        }
        Err(_) => "hostname",
    }
}

/// Resolve `host:port` to a comma-separated list of IPs (what the OS would dial).
fn resolve_addrs(host: &str, port: u16) -> String {
    match (host, port).to_socket_addrs() {
        Ok(addrs) => {
            let list: Vec<String> = addrs.map(|addr| addr.ip().to_string()).collect();
            if list.is_empty() {
                "unresolved".to_string()
            } else {
                list.join(",")
            }
        }
        Err(error) => format!("resolve-error: {error}"),
    }
}

#[cfg(target_os = "macos")]
fn os_description() -> String {
    objc2_foundation::NSProcessInfo::processInfo()
        .operatingSystemVersionString()
        .to_string()
}

#[cfg(not(target_os = "macos"))]
fn os_description() -> String {
    format!("{} ({})", std::env::consts::OS, std::env::consts::ARCH)
}

#[cfg(target_os = "macos")]
fn host_name() -> String {
    objc2_foundation::NSProcessInfo::processInfo()
        .hostName()
        .to_string()
}

#[cfg(not(target_os = "macos"))]
fn host_name() -> String {
    std::env::var("HOSTNAME").unwrap_or_else(|_| "unknown".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bcd_decodes_release_number() {
        assert_eq!(bcd_version(0x0110), "1.10");
        assert_eq!(bcd_version(0x0203), "2.03");
    }

    #[test]
    fn tablet_vendor_ids_are_recognised() {
        assert_eq!(tablet_brand(0x056a), Some("Wacom"));
        assert_eq!(tablet_brand(0x1234), None);
    }

    #[test]
    fn network_scope_classifies_addresses() {
        assert_eq!(network_scope("192.168.1.10"), "LAN/private");
        assert_eq!(network_scope("10.0.0.5"), "LAN/private");
        assert_eq!(network_scope("8.8.8.8"), "public/WAN");
        assert_eq!(network_scope("example.com"), "hostname");
    }
}
