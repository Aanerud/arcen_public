//! Typed tablet/pen capture surface for Arcen Deck.
//!
//! This module is the local-termination capture boundary described in the
//! Wacom local-termination plan: AppKit delivers typed `NSEventTypeTabletPoint`
//! / `NSEventTypeTabletProximity` events (produced by the vendor tablet driver
//! already installed on this Mac), a bounded main-thread monitor turns them
//! into small native samples, and a pure mapper turns those samples into
//! [`arcen_input::PenEvent`]. No Wacom SDK or vendor report parsing is used;
//! the existing raw-HID path under `crate::hid` is untouched by this module.
//!
//! [`dispatch::TabletEventDispatcher`] then folds a drained sample batch
//! into an edge-preserving, motion-coalesced output ready to send, and
//! `ui/app.rs` owns the [`runtime::TabletRuntime`] RAII guard, negotiates
//! typed pen with the host over `arcen_protocol::messages::PenEventMsg`,
//! maps window points through the current video `image_rect`, and
//! suppresses the egui mouse-emulation duplicates while a real pen has
//! authority.
//!
//! Every file in this module except `monitor.rs` forbids `unsafe_code`
//! individually (`#![forbid(unsafe_code)]` cannot be declared here at the
//! `tablet` module level without also — unoverridably — forbidding the
//! legitimate, safety-commented AppKit FFI `monitor.rs` needs).

pub mod dispatch;
pub mod mapper;
#[cfg(target_os = "macos")]
pub mod monitor;
pub mod probe;
pub mod runtime;
pub mod sample;

pub use dispatch::{TabletDispatchOverflow, TabletEventDispatcher};
pub use mapper::{TabletMapper, ViewSize};
pub use probe::{wacom_usb_presence, TabletCapabilityProbe};
pub use runtime::TabletRuntime;
pub use sample::{
    BoundedSampleQueue, NativeTabletPoint, NativeTabletProximity, NativeTabletTool,
    RawTabletSample, TabletButtonMask,
};

#[cfg(target_os = "macos")]
pub use monitor::{TabletEventMonitor, TabletSampleHandle};
