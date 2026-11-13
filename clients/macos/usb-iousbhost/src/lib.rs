// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The `IOUSBHost` capture backend for Arcen Native tablet bridging on macOS
//! (ADR 0019 §2–§4).
//!
//! A root process captures one USB device with `IOUSBHost` `DeviceCapture`,
//! configures it without letting the HID drivers re-match, opens its
//! interfaces and interrupt pipes, and presents the result to the shared
//! exporter engine as an [`arcen_usb_bridge::CapturedUsbDevice`]. The
//! engine — admission, cancellation, locally answered requests, release
//! accounting — stays in `arcen-usb-bridge`; this crate is only the adapter.
//!
//! The decisions that do not need the operating system are portable and
//! tested everywhere:
//!
//! - [`matching`]: which I/O Registry entry is the device to capture (by
//!   registry ID, falling back to `locationID`), what its descriptors say, and
//!   which interfaces and pipes it must have.
//! - [`lifecycle`]: the capture state machine — release exactly once, a
//!   requesting-close surrenders instead of resetting, nothing is destroyed
//!   twice — and which pipe may be aborted for a cancellation.
//! - [`status`]: `IOReturn` codes mapped to the shared transfer vocabulary and
//!   to capture failures.
//! - [`marker`]: the root-owned "captured" marker the helper writes before a
//!   capture, and the recovery decision it makes when it restarts after a
//!   crash.
//!
//! The Objective-C calls live in one module that exists only on macOS, and it
//! is the only place in this crate allowed to use `unsafe`.

#![deny(unsafe_code)]
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used))]

pub mod handback;
pub mod lifecycle;
pub mod marker;
pub mod matching;
pub mod status;

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
mod platform;

pub use handback::{
    HANDBACK_HOLD, Handback, HandbackTimeline, HandbackVerdict, MIN_DETACHED, REENUMERATION_WAIT,
    TERMINATION_WAIT,
};
pub use lifecycle::{
    DestroyMode, InterestAction, Lifecycle, LifecycleError, Phase, ReleasePlan, pipe_may_abort,
};
pub use marker::{
    CaptureMarker, DEFAULT_MARKER_DIRECTORY, DEFAULT_MARKER_PATH, MARKER_DIRECTORY_MODE,
    MARKER_FILE_NAME, MarkerRead, MarkerStore, RecoveryPlan, plan_recovery,
};
pub use matching::{
    CaptureTarget, ControlRoute, LookupError, MatchedBy, RegistryHid, RegistryUsbDevice,
    captured_facts, configuration_total_length, control_route, hid_descriptors_for,
    missing_interfaces, pipes_for, select_target, speed_from_registry,
};
pub use status::{CaptureError, CaptureStage, transfer_status};

#[cfg(target_os = "macos")]
pub use platform::{
    EndLink, IoUsbHostDevice, RecoveryReport, RegistryError, boot_session_uuid, capture,
    hid_devices, recover, report_descriptors_for, usb_devices,
};
