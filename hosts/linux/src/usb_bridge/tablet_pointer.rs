// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The Linux Native tablet pen adapter: where a USB-bridged pen points, on
//! an Xorg desktop and a native-seat Wayland desktop alike (ADR 0023).
//!
//! This reader opens the bridged pen's own evdev node read-only, never
//! grabbed (the X server or the compositor keeps it), turns its events into
//! shared [`PenSample`]s ([`EvdevPenFrames`]) the moment each report
//! arrives, and hands them to the desktop's [`DesktopPointer`], where the
//! shared lead decides whether the pen leads. It belongs to one attachment:
//! it stops when the attachment drops it or when the node goes away with
//! the bridge. The node it opens must be the device it verified (opened
//! with `O_NOFOLLOW`, same device number as sysfs gives the verified node,
//! which still carries its seat's bridge rule), and a reader that is told
//! to stop publishes nothing more.

use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::fs::FileTypeExt;
use std::os::unix::fs::OpenOptionsExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use arcen_input::tablet_pointer::{
    AxisRange, EvdevPenFrames, PenSample, ABS_X, ABS_Y, TABLET_TOOL_CODES,
};
use arcen_usb_bridge::bridge_seat::{BridgedInputNode, BridgedNodeRole};
use arcen_usb_bridge::BridgeSeat;
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};

use crate::media::desktop_pointer::DesktopPointer;
use arcen_input::tablet_pointer::PenDevice;

/// How often the reader checks whether its attachment still wants it.
const POLL_INTERVAL_MS: u16 = 250;

/// The running reader; dropping it stops the reader within
/// [`POLL_INTERVAL_MS`].
#[derive(Debug)]
pub struct TabletPointerReader {
    stop: Arc<AtomicBool>,
}

impl Drop for TabletPointerReader {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

/// Starts reading the pen node of the bridged device at port `prefix` on
/// `seat` (once its nodes are stable and verified) into `pointer`.
///
/// # Errors
///
/// When the reader thread cannot be started.
pub fn spawn(
    prefix: String,
    seat: BridgeSeat,
    discovery_timeout: Duration,
    pointer: Arc<DesktopPointer>,
) -> std::io::Result<TabletPointerReader> {
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    std::thread::Builder::new()
        .name("arcen-tablet-pointer".to_owned())
        .spawn(move || run(&prefix, &seat, discovery_timeout, &pointer, &thread_stop))?;
    Ok(TabletPointerReader { stop })
}

fn run(
    prefix: &str,
    seat: &BridgeSeat,
    discovery_timeout: Duration,
    pointer: &DesktopPointer,
    stop: &AtomicBool,
) {
    let cancelled = || stop.load(Ordering::Acquire);
    let nodes = match super::seat::discover_nodes_until(prefix, seat, discovery_timeout, &cancelled)
    {
        Ok(nodes) => nodes,
        Err(_) if cancelled() => return,
        Err(reason) => {
            tracing::warn!(
                target: "arcen::usb",
                %reason,
                "bridged pen position unavailable: its nodes did not verify"
            );
            return;
        }
    };
    let Some(pen) = nodes.iter().find(|node| node.role == BridgedNodeRole::Pen) else {
        tracing::info!(
            target: "arcen::usb",
            "the bridged device has no pen node; no pen position is reported"
        );
        return;
    };
    let (mut device, mut frames, device_number) = match open(pen, prefix, seat) {
        Ok(opened) => opened,
        Err(error) => {
            tracing::warn!(
                target: "arcen::usb",
                devnode = %pen.devnode,
                %error,
                "bridged pen position unavailable"
            );
            return;
        }
    };
    tracing::info!(
        target: "arcen::usb",
        devnode = %pen.devnode,
        name = %pen.name,
        "reporting the bridged pen's position from its own node"
    );
    let (x_range, y_range) = frames.ranges();
    pointer.note_pen_device(PenDevice {
        device_number,
        x_range,
        y_range,
    });
    let mut updates = 0_u64;
    let publish = |sample: PenSample, updates: &mut u64| {
        // A reader whose attachment moved on publishes nothing more.
        if !stop.load(Ordering::Acquire) {
            *updates += 1;
            pointer.pen(sample);
        }
    };
    while !stop.load(Ordering::Acquire) {
        let ready = {
            let mut fds = [PollFd::new(device.as_fd(), PollFlags::POLLIN)];
            match poll(&mut fds, PollTimeout::from(POLL_INTERVAL_MS)) {
                Ok(count) => count > 0,
                Err(nix::errno::Errno::EINTR) => false,
                Err(error) => {
                    tracing::debug!(target: "arcen::usb", %error, "bridged pen poll ended");
                    break;
                }
            }
        };
        if !ready {
            continue;
        }
        // Collected first: a resync reads the device between two events.
        let fetched = device.fetch_events().map(|events| {
            events
                .map(|event| (event.event_type().0, event.code(), event.value()))
                .collect::<Vec<_>>()
        });
        let at = Instant::now();
        match fetched {
            Ok(events) => {
                for (kind, code, value) in events {
                    if let Some(sample) = frames.apply(kind, code, value, at) {
                        publish(sample, &mut updates);
                    }
                    if frames.needs_resync() {
                        match resync(&device, &mut frames, at) {
                            Ok(Some(sample)) => publish(sample, &mut updates),
                            Ok(None) => {}
                            Err(error) => {
                                tracing::debug!(target: "arcen::usb", %error, "bridged pen state unreadable");
                            }
                        }
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => {
                // ENODEV: the bridge ended and the node went with it.
                tracing::debug!(target: "arcen::usb", %error, "bridged pen node closed");
                break;
            }
        }
    }
    // The cursor stays where the pen left it; the platform's pointer leads
    // again from its next change.
    pointer.pen_ended(device_number);
    tracing::info!(
        target: "arcen::usb",
        devnode = %pen.devnode,
        updates,
        "bridged pen position reader ended"
    );
}

/// Opens the verified pen node and proves the open file is that device:
/// a character device whose number is the one sysfs gives the verified
/// node, which still carries its seat's bridge rule.
fn open(
    node: &BridgedInputNode,
    prefix: &str,
    seat: &BridgeSeat,
) -> std::io::Result<(evdev::raw_stream::RawDevice, EvdevPenFrames, (u64, u64))> {
    let refused =
        |reason: String| std::io::Error::new(std::io::ErrorKind::PermissionDenied, reason);
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NONBLOCK | nix::libc::O_CLOEXEC | nix::libc::O_NOFOLLOW)
        .open(&node.devnode)?;
    let metadata = file.metadata()?;
    let expected = super::seat::node_device_number(node).map_err(refused)?;
    let opened = {
        use std::os::unix::fs::MetadataExt;
        let rdev = metadata.rdev();
        (nix::sys::stat::major(rdev), nix::sys::stat::minor(rdev))
    };
    if !metadata.file_type().is_char_device() || opened != expected {
        return Err(refused(format!(
            "{} is not the verified pen device (opened {}:{}, verified {}:{})",
            node.devnode, opened.0, opened.1, expected.0, expected.1
        )));
    }
    super::seat::verify_node(node, prefix, seat).map_err(refused)?;
    let device = evdev::raw_stream::RawDevice::from_fd(OwnedFd::from(file))?;
    let ((x_range, x), (y_range, y)) = axes(&device)?;
    let mut frames = EvdevPenFrames::new(x_range, y_range, x, y).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "the pen node's axes are empty",
        )
    })?;
    for code in tools_down(&device)? {
        frames.set_tool_down(code);
    }
    Ok((device, frames, expected))
}

/// Reads the device's state again after the kernel dropped events.
fn resync(
    device: &evdev::raw_stream::RawDevice,
    frames: &mut EvdevPenFrames,
    at: Instant,
) -> std::io::Result<Option<PenSample>> {
    let ((_, x), (_, y)) = axes(device)?;
    Ok(frames.resync(tools_down(device)?, x, y, at))
}

type Axis = (AxisRange, i32);

/// The X and Y axes' ranges and current values (`EVIOCGABS`).
fn axes(device: &evdev::raw_stream::RawDevice) -> std::io::Result<(Axis, Axis)> {
    let mut x = None;
    let mut y = None;
    for (axis, info) in device.get_absinfo()? {
        let range = AxisRange {
            minimum: info.minimum(),
            maximum: info.maximum(),
        };
        if axis.0 == ABS_X {
            x = Some((range, info.value()));
        } else if axis.0 == ABS_Y {
            y = Some((range, info.value()));
        }
    }
    x.zip(y).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "the pen node has no X/Y axes",
        )
    })
}

/// The tablet tools that are down (`EVIOCGKEY`).
fn tools_down(device: &evdev::raw_stream::RawDevice) -> std::io::Result<Vec<u16>> {
    let keys = device.get_key_state()?;
    Ok(TABLET_TOOL_CODES
        .into_iter()
        .filter(|code| keys.contains(evdev::KeyCode::new(*code)))
        .collect())
}
