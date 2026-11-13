// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! `IOReturn` codes in the shared vocabulary.
//!
//! A completion handler reports an `IOReturn`; the shared engine speaks
//! [`BackendStatus`]. A capture step reports an `NSError` whose code is an
//! `IOReturn` (`IOUSBHostErrorDomain`); the helper reports a
//! [`CaptureError`]. Both mappings are pure so they are tested on every
//! operating system, and they follow libusb's macOS backend where it already
//! made the same decision, so switching backends changes no wire status.

use arcen_usb_bridge::BackendStatus;
use std::fmt::{Display, Formatter};

/// The I/O Kit status type (`kern_return_t`).
pub type IoReturn = i32;

const fn io_return(code: u32) -> IoReturn {
    IoReturn::from_ne_bytes(code.to_ne_bytes())
}

/// `IOReturn` values used by this crate (`IOKit/IOReturn.h`,
/// `IOKit/usb/USB.h`, `IOKit/usb/IOUSBHostFamilyDefinitions.h`).
pub mod codes {
    use super::{IoReturn, io_return};

    pub const SUCCESS: IoReturn = 0;
    pub const ERROR: IoReturn = io_return(0xe000_02bc);
    pub const NO_MEMORY: IoReturn = io_return(0xe000_02bd);
    pub const NO_RESOURCES: IoReturn = io_return(0xe000_02be);
    pub const NO_DEVICE: IoReturn = io_return(0xe000_02c0);
    pub const NOT_PRIVILEGED: IoReturn = io_return(0xe000_02c1);
    pub const BAD_ARGUMENT: IoReturn = io_return(0xe000_02c2);
    pub const EXCLUSIVE_ACCESS: IoReturn = io_return(0xe000_02c5);
    pub const UNSUPPORTED: IoReturn = io_return(0xe000_02c7);
    pub const IO_ERROR: IoReturn = io_return(0xe000_02ca);
    pub const NOT_OPEN: IoReturn = io_return(0xe000_02cd);
    pub const BUSY: IoReturn = io_return(0xe000_02d5);
    pub const TIMEOUT: IoReturn = io_return(0xe000_02d6);
    pub const OFFLINE: IoReturn = io_return(0xe000_02d7);
    pub const NOT_ATTACHED: IoReturn = io_return(0xe000_02d9);
    pub const NOT_PERMITTED: IoReturn = io_return(0xe000_02e2);
    pub const UNDERRUN: IoReturn = io_return(0xe000_02e7);
    pub const OVERRUN: IoReturn = io_return(0xe000_02e8);
    pub const ABORTED: IoReturn = io_return(0xe000_02eb);
    pub const NOT_RESPONDING: IoReturn = io_return(0xe000_02ed);
    /// `kIOUSBPipeStalled`.
    pub const USB_PIPE_STALLED: IoReturn = io_return(0xe000_404f);
    /// `kIOUSBTransactionTimeout`.
    pub const USB_TRANSACTION_TIMEOUT: IoReturn = io_return(0xe000_4051);
    /// `kIOUSBEndpointNotFound`.
    pub const USB_ENDPOINT_NOT_FOUND: IoReturn = io_return(0xe000_4057);
    /// `kUSBHostReturnPipeStalled`.
    pub const USB_HOST_PIPE_STALLED: IoReturn = io_return(0xe000_5000);
}

/// The terminal status of one transfer completion.
///
/// A short IN transfer (`kIOReturnUnderrun`) completed: the handler's
/// byte count says how much arrived.
#[must_use]
pub const fn transfer_status(code: IoReturn) -> BackendStatus {
    match code {
        codes::SUCCESS | codes::UNDERRUN => BackendStatus::Completed,
        codes::ABORTED => BackendStatus::Cancelled,
        codes::TIMEOUT | codes::USB_TRANSACTION_TIMEOUT => BackendStatus::TimedOut,
        codes::USB_PIPE_STALLED | codes::USB_HOST_PIPE_STALLED => BackendStatus::Stall,
        codes::NO_DEVICE
        | codes::NOT_ATTACHED
        | codes::OFFLINE
        | codes::NOT_RESPONDING
        | codes::NOT_OPEN => BackendStatus::Disconnected,
        codes::OVERRUN
        | codes::BAD_ARGUMENT
        | codes::UNSUPPORTED
        | codes::USB_ENDPOINT_NOT_FOUND => BackendStatus::Protocol,
        _ => BackendStatus::Io,
    }
}

/// The capture step that failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureStage {
    /// Finding the device in the I/O Registry.
    Lookup,
    /// Writing the root-owned captured marker.
    Marker,
    /// `initWithIOService:options:DeviceCapture`.
    Capture,
    /// Reading the device and configuration descriptors.
    Descriptors,
    /// `configureWithValue:matchInterfaces:NO`.
    Configure,
    /// Waiting for and opening the `IOUSBHostInterface` services.
    Interfaces,
    /// Copying the interrupt pipes.
    Pipes,
}

impl CaptureStage {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Lookup => "find the device in the I/O Registry",
            Self::Marker => "record the capture",
            Self::Capture => "capture the device",
            Self::Descriptors => "read the captured device's descriptors",
            Self::Configure => "configure the captured device",
            Self::Interfaces => "open the captured device's interfaces",
            Self::Pipes => "open the captured device's pipes",
        }
    }
}

/// Why a capture failed. Whatever it took was released before this is
/// returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureError {
    pub stage: CaptureStage,
    /// The `IOReturn` the framework reported, when it reported one.
    pub code: Option<IoReturn>,
    pub detail: String,
}

impl CaptureError {
    /// A failure the framework reported with an `IOReturn`.
    #[must_use]
    pub fn from_io_return(stage: CaptureStage, code: IoReturn) -> Self {
        let detail = match code {
            codes::NOT_PRIVILEGED | codes::NOT_PERMITTED => {
                "not privileged; the helper must run with root privilege".to_owned()
            }
            codes::EXCLUSIVE_ACCESS | codes::BUSY => {
                "another process holds the device exclusively".to_owned()
            }
            codes::NO_DEVICE | codes::NOT_ATTACHED | codes::OFFLINE => {
                "the device is no longer attached".to_owned()
            }
            codes::NO_MEMORY | codes::NO_RESOURCES => "the system is out of resources".to_owned(),
            other => format!("IOReturn {:#010x}", u32::from_ne_bytes(other.to_ne_bytes())),
        };
        Self {
            stage,
            code: Some(code),
            detail,
        }
    }

    /// A failure with no `IOReturn` (a refused descriptor, a timeout).
    #[must_use]
    pub fn new(stage: CaptureStage, detail: impl Into<String>) -> Self {
        Self {
            stage,
            code: None,
            detail: detail.into(),
        }
    }

    /// Whether the failure means the process lacks the privilege to capture.
    #[must_use]
    pub const fn is_privilege(&self) -> bool {
        matches!(
            self.code,
            Some(codes::NOT_PRIVILEGED | codes::NOT_PERMITTED)
        )
    }
}

impl Display for CaptureError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.stage.as_str(), self.detail)
    }
}

impl std::error::Error for CaptureError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_return_constants_match_the_sdk_headers() {
        assert_eq!(
            u32::from_ne_bytes(codes::ABORTED.to_ne_bytes()),
            0xe000_02eb
        );
        assert_eq!(
            u32::from_ne_bytes(codes::NOT_PRIVILEGED.to_ne_bytes()),
            0xe000_02c1
        );
        assert_eq!(
            u32::from_ne_bytes(codes::USB_PIPE_STALLED.to_ne_bytes()),
            0xe000_404f
        );
        assert_eq!(
            u32::from_ne_bytes(codes::USB_HOST_PIPE_STALLED.to_ne_bytes()),
            0xe000_5000
        );
    }

    #[test]
    fn transfer_outcomes_map_like_libusbs_macos_backend() {
        let table = [
            (codes::SUCCESS, BackendStatus::Completed),
            (codes::UNDERRUN, BackendStatus::Completed),
            (codes::ABORTED, BackendStatus::Cancelled),
            (codes::TIMEOUT, BackendStatus::TimedOut),
            (codes::USB_TRANSACTION_TIMEOUT, BackendStatus::TimedOut),
            (codes::USB_PIPE_STALLED, BackendStatus::Stall),
            (codes::USB_HOST_PIPE_STALLED, BackendStatus::Stall),
            (codes::NO_DEVICE, BackendStatus::Disconnected),
            (codes::NOT_ATTACHED, BackendStatus::Disconnected),
            (codes::OFFLINE, BackendStatus::Disconnected),
            (codes::NOT_RESPONDING, BackendStatus::Disconnected),
            (codes::NOT_OPEN, BackendStatus::Disconnected),
            (codes::OVERRUN, BackendStatus::Protocol),
            (codes::BAD_ARGUMENT, BackendStatus::Protocol),
            (codes::UNSUPPORTED, BackendStatus::Protocol),
            (codes::USB_ENDPOINT_NOT_FOUND, BackendStatus::Protocol),
            (codes::ERROR, BackendStatus::Io),
            (codes::IO_ERROR, BackendStatus::Io),
            (codes::NO_MEMORY, BackendStatus::Io),
            (codes::EXCLUSIVE_ACCESS, BackendStatus::Io),
            (0x1234, BackendStatus::Io),
        ];
        for (code, status) in table {
            assert_eq!(transfer_status(code), status, "{code:#x}");
        }
    }

    #[test]
    fn capture_failures_name_the_step_and_the_cause() {
        let privilege = CaptureError::from_io_return(CaptureStage::Capture, codes::NOT_PRIVILEGED);
        assert!(privilege.is_privilege());
        assert_eq!(
            privilege.to_string(),
            "capture the device: not privileged; the helper must run with root privilege"
        );
        let busy = CaptureError::from_io_return(CaptureStage::Capture, codes::EXCLUSIVE_ACCESS);
        assert!(!busy.is_privilege());
        assert!(busy.to_string().contains("another process"));
        let gone = CaptureError::from_io_return(CaptureStage::Configure, codes::NO_DEVICE);
        assert_eq!(
            gone.to_string(),
            "configure the captured device: the device is no longer attached"
        );
        let other = CaptureError::from_io_return(CaptureStage::Pipes, codes::IO_ERROR);
        assert_eq!(
            other.to_string(),
            "open the captured device's pipes: IOReturn 0xe00002ca"
        );
        let timed = CaptureError::new(CaptureStage::Interfaces, "interface 1 never appeared");
        assert_eq!(timed.code, None);
        assert!(timed.to_string().ends_with("interface 1 never appeared"));
    }
}
