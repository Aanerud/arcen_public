// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The server side of the in-tree USB/IP virtual host controller.
//!
//! The helper reserves one free USB 2 port of `vhci_hcd.0`, creates an
//! `AF_UNIX`/`SOCK_STREAM` socketpair, and writes
//! `"<port> <sockfd> <devid> <speed>"` to the controller's sysfs `attach`. The
//! kernel takes its own reference to that socket end; the helper keeps the
//! other end and plays the device on it with the USB/IP protocol
//! ([`arcen_usb_bridge::decode_command`], [`arcen_usb_bridge::UsbipServerState`]).
//! Detach is the sysfs `detach` file; closing the socket (a crashed helper)
//! makes the kernel disconnect the port by itself.
//!
//! Ports are shared by every user of the host's single controller (other
//! Piers, an operator's `usbip attach`). A port is reserved with a host-wide
//! lock file and re-checked free in the kernel's `status` table after the lock
//! is taken.

use super::backend::{
    status_to_errno, HcdBackendKind, HcdEvent, HcdUrb, VirtualHcd, VHCI_HCD_CONTROLLER,
};
use super::kmod::VHCI_HCD_SYSFS;
use arcen_usb_bridge::{
    decode_command, encode_ret_unlink, port_devpath_prefix, UnlinkAction, UrbStatus, UsbSpeed,
    UsbipCommand, UsbipServerState, VhciStatus, MAX_TRANSFER_BYTES, USBIP_HEADER_BYTES,
};
use nix::fcntl::{Flock, FlockArg};
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

const LOCK_DIR: &str = "/run/arcen-usb-bridge";
/// The kernel's `usb_device_speed` values for `attach`.
const SPEED_LOW: u32 = 1;
const SPEED_FULL: u32 = 2;
const SPEED_HIGH: u32 = 3;
/// `devid` the kernel echoes in every command; any value identifies the
/// remote device to it, and only one device lives on a port.
const DEVICE_ID: u32 = 0x0001_0001;
/// A PDU the kernel started writing arrives whole well within this time.
const PDU_READ_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_SYSFS_BYTES: u64 = 64 * 1024;
const MIN_INTERRUPT_CADENCE: Duration = Duration::from_millis(1);
const MAX_INTERRUPT_CADENCE: Duration = Duration::from_millis(8);

/// One reserved `vhci_hcd` port and, once attached, the device on it.
pub(crate) struct UsbipHcd {
    sysfs: PathBuf,
    port: u32,
    speed: UsbSpeed,
    prefix: String,
    stream: Option<UnixStream>,
    state: UsbipServerState,
    /// `CMD_UNLINK` sequence numbers awaiting their `RET_UNLINK`, by target.
    unlinks: BTreeMap<u32, u32>,
    /// Held for the reservation's lifetime; released on drop.
    _lock: Flock<File>,
}

impl std::fmt::Debug for UsbipHcd {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("UsbipHcd")
            .field("port", &self.port)
            .field("prefix", &self.prefix)
            .field("attached", &self.stream.is_some())
            .finish_non_exhaustive()
    }
}

fn read_sysfs(path: &Path) -> Result<String, String> {
    let mut text = String::new();
    File::open(path)
        .and_then(|file| file.take(MAX_SYSFS_BYTES).read_to_string(&mut text))
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    Ok(text)
}

fn write_sysfs(path: &Path, value: &str) -> Result<(), String> {
    OpenOptions::new()
        .write(true)
        .open(path)
        .and_then(|mut file| file.write_all(value.as_bytes()))
        .map_err(|error| format!("write {}: {error}", path.display()))
}

/// The bus number of the controller's USB 2 root hub (`usb<N>` with speed
/// 480 Mbit/s or lower).
fn high_speed_bus(sysfs: &Path) -> Result<u32, String> {
    let entries =
        std::fs::read_dir(sysfs).map_err(|error| format!("read {}: {error}", sysfs.display()))?;
    for entry in entries.flatten().take(256) {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(bus) = name
            .strip_prefix("usb")
            .and_then(|bus| bus.parse::<u32>().ok())
        else {
            continue;
        };
        let speed = read_sysfs(&entry.path().join("speed")).unwrap_or_default();
        if speed.trim().parse::<u32>().is_ok_and(|mbit| mbit <= 480) {
            return Ok(bus);
        }
    }
    Err(format!("{} has no USB 2 root hub", sysfs.display()))
}

fn lock_port(port: u32) -> Result<Option<Flock<File>>, String> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(LOCK_DIR)
        .map_err(|error| format!("create {LOCK_DIR}: {error}"))?;
    let path = Path::new(LOCK_DIR).join(format!("vhci-port-{port}.lock"));
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC)
        .open(&path)
        .map_err(|error| format!("open {}: {error}", path.display()))?;
    match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
        Ok(lock) => Ok(Some(lock)),
        Err((_, nix::errno::Errno::EWOULDBLOCK)) => Ok(None),
        Err((_, error)) => Err(format!("lock {}: {error}", path.display())),
    }
}

impl UsbipHcd {
    /// Reserves the lowest free USB 2 port of `vhci_hcd.0`.
    pub(crate) fn reserve(speed: UsbSpeed) -> Result<Self, String> {
        let sysfs = PathBuf::from(VHCI_HCD_SYSFS);
        let bus = high_speed_bus(&sysfs)?;
        let status = VhciStatus::parse(&read_sysfs(&sysfs.join("status"))?);
        for port in status.free_high_speed_ports().collect::<Vec<_>>() {
            let Some(lock) = lock_port(port)? else {
                continue;
            };
            // Re-read under the lock: another user may have attached since.
            let current = VhciStatus::parse(&read_sysfs(&sysfs.join("status"))?);
            if !current.row(port).is_some_and(|row| row.is_free()) {
                continue;
            }
            let index = current
                .high_speed_root_index(port)
                .ok_or_else(|| format!("vhci-hcd port {port} has no root-hub index"))?;
            let expected = port_devpath_prefix(VHCI_HCD_CONTROLLER, bus, index);
            let prefix = super::seat::port_prefix_for_bus(
                i32::try_from(bus).map_err(|_| "vhci-hcd bus number overflow".to_owned())?,
                u8::try_from(index).map_err(|_| "vhci-hcd port index overflow".to_owned())?,
            )
            .map_err(|error| format!("bridge port path: {error}"))?;
            if prefix != expected {
                return Err(format!(
                    "vhci-hcd port {port} resolves to {prefix}, not {expected}"
                ));
            }
            return Ok(Self {
                prefix,
                sysfs,
                port,
                speed,
                stream: None,
                state: UsbipServerState::default(),
                unlinks: BTreeMap::new(),
                _lock: lock,
            });
        }
        Err("every vhci-hcd USB 2 port is in use".to_owned())
    }

    fn read_pdu_bytes(&mut self, buffer: &mut [u8]) -> Result<(), String> {
        let stream = self
            .stream
            .as_mut()
            .ok_or_else(|| "vhci-hcd port is not attached".to_owned())?;
        stream.read_exact(buffer).map_err(|error| {
            if error.kind() == std::io::ErrorKind::UnexpectedEof {
                "vhci-hcd closed the USB/IP connection".to_owned()
            } else {
                format!("read USB/IP PDU: {error}")
            }
        })
    }

    fn send(&mut self, pdu: &[u8]) -> Result<(), String> {
        let stream = self
            .stream
            .as_mut()
            .ok_or_else(|| "vhci-hcd port is not attached".to_owned())?;
        stream
            .write_all(pdu)
            .map_err(|error| format!("write USB/IP PDU: {error}"))
    }

    /// Interrupt URB pacing from the kernel's interval (frames at low/full
    /// speed, microframes at high speed), clamped to 1–8 ms.
    fn cadence(&self, interval: i32) -> Duration {
        let interval = u64::try_from(interval).unwrap_or(0);
        let micros = match self.speed {
            UsbSpeed::High => interval.saturating_mul(125),
            UsbSpeed::Low | UsbSpeed::Full => interval.saturating_mul(1_000),
        };
        Duration::from_micros(micros).clamp(MIN_INTERRUPT_CADENCE, MAX_INTERRUPT_CADENCE)
    }
}

impl VirtualHcd for UsbipHcd {
    fn kind(&self) -> HcdBackendKind {
        HcdBackendKind::VhciHcd
    }

    fn devpath_prefix(&self) -> &str {
        &self.prefix
    }

    fn attach(&mut self) -> Result<(), String> {
        if self.stream.is_some() {
            return Ok(());
        }
        let (ours, kernel) =
            UnixStream::pair().map_err(|error| format!("create socketpair: {error}"))?;
        let speed = match self.speed {
            UsbSpeed::Low => SPEED_LOW,
            UsbSpeed::Full => SPEED_FULL,
            UsbSpeed::High => SPEED_HIGH,
        };
        // The kernel resolves the descriptor in this process's table and
        // keeps its own reference; ours is closed when `kernel` drops.
        write_sysfs(
            &self.sysfs.join("attach"),
            &format!("{} {} {DEVICE_ID} {speed}", self.port, kernel.as_raw_fd()),
        )?;
        drop(kernel);
        ours.set_read_timeout(Some(PDU_READ_TIMEOUT))
            .and_then(|()| ours.set_write_timeout(Some(PDU_READ_TIMEOUT)))
            .map_err(|error| format!("configure USB/IP socket: {error}"))?;
        self.stream = Some(ours);
        Ok(())
    }

    fn next_event(&mut self, timeout: Duration) -> Result<Option<HcdEvent>, String> {
        let ready = {
            let stream = self
                .stream
                .as_ref()
                .ok_or_else(|| "vhci-hcd port is not attached".to_owned())?;
            let millis = u16::try_from(timeout.as_millis()).unwrap_or(u16::MAX);
            let mut fds = [PollFd::new(stream.as_fd(), PollFlags::POLLIN)];
            match poll(&mut fds, PollTimeout::from(millis)) {
                Ok(count) => count > 0,
                Err(nix::errno::Errno::EINTR) => false,
                Err(error) => return Err(format!("poll USB/IP socket: {error}")),
            }
        };
        if !ready {
            return Ok(None);
        }
        let mut header = [0_u8; USBIP_HEADER_BYTES];
        self.read_pdu_bytes(&mut header)?;
        match decode_command(&header).map_err(|error| error.to_string())? {
            UsbipCommand::Submit(submit) => {
                let mut out = vec![0_u8; submit.out_payload_len().min(MAX_TRANSFER_BYTES)];
                if !out.is_empty() {
                    self.read_pdu_bytes(&mut out)?;
                }
                self.state
                    .submit(&submit)
                    .map_err(|error| error.to_string())?;
                let kind = submit.transfer_kind();
                Ok(Some(HcdEvent::Submit(HcdUrb {
                    handle: u64::from(submit.seqnum),
                    kind,
                    endpoint: submit.endpoint_address,
                    setup: (kind == arcen_usb_bridge::TransferKind::Control)
                        .then_some(submit.setup),
                    length: submit.length,
                    out,
                    cadence: self.cadence(submit.interval),
                })))
            }
            UsbipCommand::Unlink { seqnum, target } => match self.state.unlink(target) {
                UnlinkAction::Cancel => {
                    self.unlinks.insert(target, seqnum);
                    Ok(Some(HcdEvent::Cancel {
                        handle: u64::from(target),
                    }))
                }
                UnlinkAction::AlreadyDone => {
                    self.send(&encode_ret_unlink(seqnum, 0))?;
                    Ok(None)
                }
            },
        }
    }

    fn giveback(
        &mut self,
        handle: u64,
        status: UrbStatus,
        out_actual: usize,
        data: &[u8],
    ) -> Result<(), String> {
        let seqnum = u32::try_from(handle).map_err(|_| "USB/IP handle overflow".to_owned())?;
        let errno = status_to_errno(status);
        let pdu = match self.state.reply(seqnum, errno, out_actual, data) {
            Ok(pdu) => pdu,
            // A reply longer than the kernel's buffer would make the kernel
            // drop the connection: fail the URB instead.
            Err(_) => self
                .state
                .reply(seqnum, -nix::libc::EOVERFLOW, 0, &[])
                .map_err(|error| error.to_string())?,
        };
        match pdu {
            Some(pdu) => self.send(&pdu),
            None => Ok(()),
        }
    }

    fn acknowledge_cancel(&mut self, handle: u64) -> Result<(), String> {
        let target = u32::try_from(handle).map_err(|_| "USB/IP handle overflow".to_owned())?;
        match self.unlinks.remove(&target) {
            Some(seqnum) => self.send(&encode_ret_unlink(seqnum, -nix::libc::ECONNRESET)),
            None => Ok(()),
        }
    }

    fn detach(&mut self) -> Result<(), String> {
        let Some(stream) = self.stream.take() else {
            return Ok(());
        };
        let result = write_sysfs(&self.sysfs.join("detach"), &self.port.to_string());
        let _ = stream.shutdown(std::net::Shutdown::Both);
        result
    }
}

impl Drop for UsbipHcd {
    fn drop(&mut self) {
        let _ = self.detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interrupt_cadence_follows_the_bus_interval_within_bounds() {
        let (mut hcd, _kernel) = connected(UsbSpeed::Full);
        assert_eq!(hcd.cadence(4), Duration::from_millis(4));
        assert_eq!(hcd.cadence(0), MIN_INTERRUPT_CADENCE);
        assert_eq!(hcd.cadence(255), MAX_INTERRUPT_CADENCE);
        hcd.speed = UsbSpeed::High;
        assert_eq!(hcd.cadence(16), Duration::from_millis(2));
        assert_eq!(hcd.cadence(-1), MIN_INTERRUPT_CADENCE);
    }

    fn lock_for_test() -> Flock<File> {
        let path = std::env::temp_dir().join(format!(
            "arcen-usbip-test-{}-{}.lock",
            std::process::id(),
            LOCK_SERIAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let file = File::create(&path).expect("lock file");
        let _ = std::fs::remove_file(&path);
        Flock::lock(file, FlockArg::LockExclusiveNonblock)
            .map_err(|(_, error)| error)
            .expect("lock a private file")
    }

    static LOCK_SERIAL: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

    /// A server on one end of a socketpair, a fake kernel on the other.
    fn connected(speed: UsbSpeed) -> (UsbipHcd, UnixStream) {
        let (ours, kernel) = UnixStream::pair().expect("socketpair");
        ours.set_read_timeout(Some(PDU_READ_TIMEOUT))
            .expect("timeout");
        let hcd = UsbipHcd {
            sysfs: PathBuf::new(),
            port: 0,
            speed,
            prefix: String::new(),
            stream: Some(ours),
            state: UsbipServerState::default(),
            unlinks: BTreeMap::new(),
            _lock: lock_for_test(),
        };
        (hcd, kernel)
    }

    fn cmd_submit(seqnum: u32, direction: u32, ep: u32, length: u32, setup: [u8; 8]) -> Vec<u8> {
        let mut pdu = vec![0_u8; USBIP_HEADER_BYTES];
        pdu[0..4].copy_from_slice(&1_u32.to_be_bytes());
        pdu[4..8].copy_from_slice(&seqnum.to_be_bytes());
        pdu[12..16].copy_from_slice(&direction.to_be_bytes());
        pdu[16..20].copy_from_slice(&ep.to_be_bytes());
        pdu[24..28].copy_from_slice(&length.to_be_bytes());
        pdu[36..40].copy_from_slice(&4_u32.to_be_bytes());
        pdu[40..48].copy_from_slice(&setup);
        pdu
    }

    fn cmd_unlink(seqnum: u32, target: u32) -> Vec<u8> {
        let mut pdu = vec![0_u8; USBIP_HEADER_BYTES];
        pdu[0..4].copy_from_slice(&2_u32.to_be_bytes());
        pdu[4..8].copy_from_slice(&seqnum.to_be_bytes());
        pdu[20..24].copy_from_slice(&target.to_be_bytes());
        pdu
    }

    fn read_reply(kernel: &mut UnixStream, payload: usize) -> (u32, u32, i32, u32, Vec<u8>) {
        let mut header = [0_u8; USBIP_HEADER_BYTES];
        kernel.read_exact(&mut header).expect("reply header");
        let word = |offset: usize| {
            u32::from_be_bytes([
                header[offset],
                header[offset + 1],
                header[offset + 2],
                header[offset + 3],
            ])
        };
        let mut data = vec![0_u8; payload];
        kernel.read_exact(&mut data).expect("reply payload");
        #[allow(clippy::cast_possible_wrap)]
        (word(0), word(4), word(20) as i32, word(24), data)
    }

    #[test]
    fn a_control_in_and_an_interrupt_out_round_trip_over_the_socket() {
        let (mut hcd, mut kernel) = connected(UsbSpeed::Full);
        kernel
            .write_all(&cmd_submit(1, 1, 0, 18, [0x80, 6, 0, 1, 0, 0, 18, 0]))
            .expect("submit");
        let Some(HcdEvent::Submit(urb)) = hcd.next_event(Duration::from_secs(2)).expect("event")
        else {
            panic!("expected a submit");
        };
        assert_eq!(urb.kind, arcen_usb_bridge::TransferKind::Control);
        assert_eq!(urb.endpoint, 0x80);
        assert_eq!(urb.length, 18);
        hcd.giveback(urb.handle, UrbStatus::Success, 0, &[0x12, 0x01])
            .expect("giveback");
        assert_eq!(read_reply(&mut kernel, 2), (3, 1, 0, 2, vec![0x12, 0x01]));

        let mut out = cmd_submit(2, 0, 2, 4, [0; 8]);
        out.extend_from_slice(&[9, 8, 7, 6]);
        kernel.write_all(&out).expect("submit out");
        let Some(HcdEvent::Submit(urb)) = hcd.next_event(Duration::from_secs(2)).expect("event")
        else {
            panic!("expected a submit");
        };
        assert_eq!(urb.kind, arcen_usb_bridge::TransferKind::Interrupt);
        assert_eq!(urb.out, vec![9, 8, 7, 6]);
        assert_eq!(urb.cadence, Duration::from_millis(4));
        hcd.giveback(urb.handle, UrbStatus::Stall, 0, &[])
            .expect("giveback");
        assert_eq!(
            read_reply(&mut kernel, 0),
            (3, 2, -nix::libc::EPIPE, 0, Vec::new())
        );
        assert!(hcd
            .next_event(Duration::from_millis(10))
            .expect("idle")
            .is_none());
    }

    #[test]
    fn an_unlink_cancels_once_and_the_late_completion_is_dropped() {
        let (mut hcd, mut kernel) = connected(UsbSpeed::High);
        kernel
            .write_all(&cmd_submit(5, 1, 1, 8, [0; 8]))
            .expect("submit");
        let Some(HcdEvent::Submit(urb)) = hcd.next_event(Duration::from_secs(2)).expect("event")
        else {
            panic!("expected a submit");
        };
        kernel.write_all(&cmd_unlink(6, 5)).expect("unlink");
        let Some(HcdEvent::Cancel { handle }) =
            hcd.next_event(Duration::from_secs(2)).expect("event")
        else {
            panic!("expected a cancel");
        };
        assert_eq!(handle, urb.handle);
        hcd.acknowledge_cancel(handle).expect("acknowledge");
        assert_eq!(
            read_reply(&mut kernel, 0),
            (4, 6, -nix::libc::ECONNRESET, 0, Vec::new())
        );
        // The Deck's completion raced the unlink: nothing reaches the kernel.
        hcd.giveback(handle, UrbStatus::Success, 0, &[1; 8])
            .expect("dropped");
        // An unlink of an URB already answered gets status 0 by itself.
        kernel.write_all(&cmd_unlink(7, 5)).expect("late unlink");
        assert!(hcd
            .next_event(Duration::from_secs(2))
            .expect("event")
            .is_none());
        assert_eq!(read_reply(&mut kernel, 0), (4, 7, 0, 0, Vec::new()));
    }

    #[test]
    fn an_oversized_reply_fails_the_urb_instead_of_the_connection() {
        let (mut hcd, mut kernel) = connected(UsbSpeed::Full);
        kernel
            .write_all(&cmd_submit(9, 1, 1, 4, [0; 8]))
            .expect("submit");
        let Some(HcdEvent::Submit(urb)) = hcd.next_event(Duration::from_secs(2)).expect("event")
        else {
            panic!("expected a submit");
        };
        hcd.giveback(urb.handle, UrbStatus::Success, 0, &[0; 5])
            .expect("giveback");
        assert_eq!(
            read_reply(&mut kernel, 0),
            (3, 9, -nix::libc::EOVERFLOW, 0, Vec::new())
        );
    }

    #[test]
    fn a_closed_or_hostile_kernel_end_is_an_error() {
        let (mut hcd, mut kernel) = connected(UsbSpeed::Full);
        let mut iso = cmd_submit(1, 1, 1, 8, [0; 8]);
        iso[32..36].copy_from_slice(&3_u32.to_be_bytes());
        kernel.write_all(&iso).expect("iso submit");
        assert!(hcd.next_event(Duration::from_secs(2)).is_err());
        let (mut hcd, kernel) = connected(UsbSpeed::Full);
        drop(kernel);
        assert!(hcd
            .next_event(Duration::from_secs(2))
            .expect_err("closed")
            .contains("closed"));
    }
}
