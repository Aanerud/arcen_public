// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The virtual host controller a bridged device is attached to.
//!
//! Two kernels mechanisms can play a USB device to the host's own drivers:
//!
//! - **`vhci-hcd`** (primary): the in-tree USB/IP virtual host controller,
//!   shipped and signed by the distribution (on Rocky Linux / RHEL in
//!   `kernel-modules-internal`). The helper attaches one end of a socketpair
//!   to a free port and speaks USB/IP on the other ([`super::usbip`]).
//! - **`usb-vhci`** (lab fallback): the out-of-tree `/dev/usb-vhci` ioctl
//!   controller ([`super::vhci`]). It needs a self-built, self-signed module
//!   and is never installed by Arcen.
//!
//! Both are served through [`VirtualHcd`], so the URB lane to the Deck, the
//! descriptor guard and the seat handling do not depend on which one runs.

use super::kmod::vhci_hcd_facts;
use super::usbip::UsbipHcd;
use super::vhci::{FetchedWork, PortState, VhciController, VhciError};
use arcen_usb_bridge::{SetupPacket, TransferDirection, TransferKind, UrbStatus, UsbSpeed};
use std::path::Path;
use std::time::Duration;

use super::kmod::VHCI_HCD_SYSFS;
/// The platform device name of the in-tree controller.
pub(crate) const VHCI_HCD_CONTROLLER: &str = "vhci_hcd.0";
const USB_VHCI_DEVICE: &str = "/dev/usb-vhci";
const MODPROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Which virtual host controller serves a bridge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HcdBackendKind {
    /// The distribution's in-tree USB/IP `vhci-hcd`.
    VhciHcd,
    /// The out-of-tree `usb-vhci` character device (lab fallback).
    UsbVhci,
}

impl HcdBackendKind {
    /// Stable label for logs and the helper's argument.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::VhciHcd => "vhci-hcd",
            Self::UsbVhci => "usb-vhci",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "vhci-hcd" => Some(Self::VhciHcd),
            "usb-vhci" => Some(Self::UsbVhci),
            _ => None,
        }
    }
}

/// Loads `vhci-hcd` when it is installed but not loaded.
fn load_vhci_hcd() -> bool {
    if Path::new(VHCI_HCD_SYSFS).is_dir() {
        return true;
    }
    let Ok(mut child) = std::process::Command::new("modprobe")
        .args(["-q", "vhci-hcd"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    else {
        return false;
    };
    let deadline = std::time::Instant::now() + MODPROBE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
    Path::new(VHCI_HCD_SYSFS).is_dir()
}

fn usb_vhci_device_present() -> bool {
    use std::os::unix::fs::FileTypeExt;
    std::fs::metadata(USB_VHCI_DEVICE).is_ok_and(|metadata| metadata.file_type().is_char_device())
}

/// Chooses the controller: the in-tree `vhci-hcd` when it is loaded or
/// loadable, else the lab `/dev/usb-vhci`, else a refusal that names the
/// package to install.
///
/// # Errors
///
/// The operator-facing reason no controller is usable.
pub fn select_backend() -> Result<HcdBackendKind, String> {
    if load_vhci_hcd() {
        return Ok(HcdBackendKind::VhciHcd);
    }
    if usb_vhci_device_present() {
        return Ok(HcdBackendKind::UsbVhci);
    }
    Err(arcen_usb_bridge::missing_vhci_hcd_reason(&vhci_hcd_facts()))
}

/// Loads `vhci-hcd` once, at Pier start, before anything is served (it may
/// block on `modprobe` for up to [`MODPROBE_TIMEOUT`]). Connections then use
/// the cheap [`available_backend`].
pub fn prepare() -> Result<HcdBackendKind, String> {
    select_backend()
}

/// The controller that is usable now, from sysfs and `/dev` alone: never
/// loads a module, never sleeps, so it is safe on an async task.
///
/// # Errors
///
/// The operator-facing reason no controller is usable.
pub fn available_backend() -> Result<HcdBackendKind, String> {
    if Path::new(VHCI_HCD_SYSFS).is_dir() {
        return Ok(HcdBackendKind::VhciHcd);
    }
    if usb_vhci_device_present() {
        return Ok(HcdBackendKind::UsbVhci);
    }
    Err(arcen_usb_bridge::missing_vhci_hcd_reason(&vhci_hcd_facts()))
}

/// One URB the host's USB core submitted to the virtual device.
#[derive(Debug, Clone)]
pub(crate) struct HcdUrb {
    /// Backend handle; answered with [`VirtualHcd::giveback`].
    pub handle: u64,
    pub kind: TransferKind,
    /// Endpoint address including the direction bit.
    pub endpoint: u8,
    pub setup: Option<SetupPacket>,
    /// Requested transfer length, bounded by `MAX_TRANSFER_BYTES`.
    pub length: usize,
    /// OUT payload (empty for IN).
    pub out: Vec<u8>,
    /// How long to hold an interrupt URB before forwarding it, so a device
    /// that answers at once cannot drive a busy loop.
    pub cadence: Duration,
}

impl HcdUrb {
    pub(crate) const fn direction(&self) -> TransferDirection {
        if self.endpoint & 0x80 == 0 {
            TransferDirection::Out
        } else {
            TransferDirection::In
        }
    }
}

/// Work the virtual host controller hands to the bridge.
#[derive(Debug, Clone)]
pub(crate) enum HcdEvent {
    Submit(HcdUrb),
    /// The host cancelled the URB with this handle; answer with
    /// [`VirtualHcd::acknowledge_cancel`].
    Cancel {
        handle: u64,
    },
}

/// A virtual USB host controller port with one bridged device.
pub(crate) trait VirtualHcd {
    /// Which controller this is.
    fn kind(&self) -> HcdBackendKind;
    /// Sysfs devpath every node of the device will live under, valid before
    /// [`VirtualHcd::attach`] so the seat rule can exist first.
    fn devpath_prefix(&self) -> &str;
    /// Connects the device; the kernel starts enumerating it.
    fn attach(&mut self) -> Result<(), String>;
    /// Waits up to `timeout` for work; `None` when there was none.
    fn next_event(&mut self, timeout: Duration) -> Result<Option<HcdEvent>, String>;
    /// Answers one URB. `out_actual` is the byte count an OUT transfer
    /// consumed; `data` is an IN transfer's payload.
    fn giveback(
        &mut self,
        handle: u64,
        status: UrbStatus,
        out_actual: usize,
        data: &[u8],
    ) -> Result<(), String>;
    /// Confirms a cancellation reported by [`HcdEvent::Cancel`].
    fn acknowledge_cancel(&mut self, handle: u64) -> Result<(), String>;
    /// Disconnects the device. Idempotent.
    fn detach(&mut self) -> Result<(), String>;
}

/// Opens the chosen controller and reserves one port for `speed`.
pub(crate) fn open(
    kind: HcdBackendKind,
    speed: UsbSpeed,
) -> Result<Box<dyn VirtualHcd + Send>, String> {
    match kind {
        HcdBackendKind::VhciHcd => Ok(Box::new(UsbipHcd::reserve(speed)?)),
        HcdBackendKind::UsbVhci => Ok(Box::new(UsbVhciHcd::open(speed)?)),
    }
}

/// The negative Linux errno a host USB core expects for `status`.
pub(crate) const fn status_to_errno(status: UrbStatus) -> i32 {
    match status {
        UrbStatus::Success => 0,
        UrbStatus::Cancelled => -nix::libc::ECONNRESET,
        UrbStatus::TimedOut => -nix::libc::ETIMEDOUT,
        UrbStatus::Stall => -nix::libc::EPIPE,
        UrbStatus::Disconnected => -nix::libc::ENODEV,
        UrbStatus::Protocol => -nix::libc::EPROTO,
        UrbStatus::Io => -nix::libc::EIO,
    }
}

/// The out-of-tree `usb-vhci` controller (lab fallback), one port.
struct UsbVhciHcd {
    controller: VhciController,
    speed: UsbSpeed,
    prefix: String,
    connected: bool,
}

const USB_VHCI_PORT: u8 = 1;

impl UsbVhciHcd {
    fn open(speed: UsbSpeed) -> Result<Self, String> {
        let controller = VhciController::open_and_register(1).map_err(|error| error.to_string())?;
        let prefix = super::seat::port_prefix_for_bus(controller.bus_number(), USB_VHCI_PORT)
            .map_err(|error| format!("bridge port path: {error}"))?;
        Ok(Self {
            controller,
            speed,
            prefix,
            connected: false,
        })
    }

    fn port_state(&mut self, state: PortState) -> Result<(), VhciError> {
        if state.reset_requested() {
            self.controller.complete_reset(state.index)
        } else if state.resume_requested() {
            self.controller.complete_resume(state.index)
        } else {
            Ok(())
        }
    }
}

impl VirtualHcd for UsbVhciHcd {
    fn kind(&self) -> HcdBackendKind {
        HcdBackendKind::UsbVhci
    }

    fn devpath_prefix(&self) -> &str {
        &self.prefix
    }

    fn attach(&mut self) -> Result<(), String> {
        self.controller
            .connect(USB_VHCI_PORT, self.speed)
            .map_err(|error| error.to_string())?;
        self.connected = true;
        Ok(())
    }

    fn next_event(&mut self, timeout: Duration) -> Result<Option<HcdEvent>, String> {
        match self.controller.fetch_work(timeout) {
            Ok(FetchedWork::Port(state)) => {
                self.port_state(state).map_err(|error| error.to_string())?;
                Ok(None)
            }
            Ok(FetchedWork::Cancel { handle }) => Ok(Some(HcdEvent::Cancel { handle })),
            Ok(FetchedWork::Urb(urb)) => {
                let kind = urb.transfer_kind().map_err(|error| error.to_string())?;
                let out = if urb.direction() == TransferDirection::Out && urb.buffer_length > 0 {
                    self.controller
                        .fetch_out_data(urb.handle, urb.buffer_length)
                        .map_err(|error| error.to_string())?
                } else {
                    Vec::new()
                };
                Ok(Some(HcdEvent::Submit(HcdUrb {
                    handle: urb.handle,
                    kind,
                    endpoint: urb.endpoint(),
                    setup: (kind == TransferKind::Control).then_some(SetupPacket {
                        request_type: urb.setup.request_type,
                        request: urb.setup.request,
                        value: urb.setup.value,
                        index: urb.setup.index,
                        length: urb.setup.length,
                    }),
                    length: urb.buffer_length,
                    out,
                    cadence: urb.interrupt_cadence(),
                })))
            }
            Err(VhciError::TimedOut) => Ok(None),
            Err(error) => Err(error.to_string()),
        }
    }

    fn giveback(
        &mut self,
        handle: u64,
        status: UrbStatus,
        _out_actual: usize,
        data: &[u8],
    ) -> Result<(), String> {
        self.controller
            .giveback(handle, status_to_errno(status), data)
            .map_err(|error| error.to_string())
    }

    fn acknowledge_cancel(&mut self, handle: u64) -> Result<(), String> {
        self.controller
            .giveback(handle, -nix::libc::ECONNRESET, &[])
            .map_err(|error| error.to_string())
    }

    fn detach(&mut self) -> Result<(), String> {
        if !self.connected {
            return Ok(());
        }
        self.connected = false;
        self.controller
            .disconnect(USB_VHCI_PORT)
            .map_err(|error| error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_labels_round_trip() {
        for kind in [HcdBackendKind::VhciHcd, HcdBackendKind::UsbVhci] {
            assert_eq!(HcdBackendKind::parse(kind.label()), Some(kind));
        }
        assert_eq!(HcdBackendKind::parse("dummy_hcd"), None);
    }
}
