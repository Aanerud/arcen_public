//! Typed tablet/pen capture surface for Arcen Deck.
//!
//! This module is the local-termination capture boundary for any pen tablet:
//! AppKit delivers typed `NSEventTypeTabletPoint` /
//! `NSEventTypeTabletProximity` events (produced by whichever tablet driver is
//! installed on this Mac), a bounded main-thread monitor turns them into small
//! native samples, and a pure mapper turns those samples into
//! [`arcen_input::PenEvent`] through the shared `arcen_input::pen_axes`
//! normalisation. No vendor SDK, vendor id or vendor report parsing is used;
//! tablet presence comes from HID report descriptors ([`presence`]).
//!
//! The monitor hands each sample to `ui::pen_input::PenInput` as AppKit
//! dispatches it, so a pen sample is mapped, classified by the shared
//! [`arcen_input::PenForwardingDispatcher`] and sent to the input lane at
//! once instead of waiting for the next UI pass. `ui/app.rs` owns the
//! [`runtime::TabletRuntime`] RAII guard, negotiates typed pen with the host,
//! publishes the current video `image_rect` and routing once per pass, and
//! suppresses the egui mouse-emulation duplicates while a real pen has
//! authority.
//!
//! Every file in this module except `monitor.rs` and `hid_inventory.rs`
//! forbids `unsafe_code` individually (`#![forbid(unsafe_code)]` cannot be declared here at the
//! `tablet` module level without also — unoverridably — forbidding the
//! legitimate, safety-commented AppKit and I/O Registry FFI those two need).

pub mod hid_inventory;
pub mod mapper;
#[cfg(target_os = "macos")]
pub mod monitor;
pub mod presence;
pub mod probe;
pub mod runtime;
pub mod sample;

pub use mapper::{TabletMapper, ViewSize};
pub use presence::{
    attached_pen_tablets, note_pen_observed, tablet_presence, typed_pen_capability,
};
pub use probe::TabletCapabilityProbe;
pub use runtime::{MouseCoalescing, TabletRuntime};
pub use sample::{
    BoundedSampleQueue, DispatchAgeStats, DispatchAgeSummary, NativeTabletPoint,
    NativeTabletProximity, NativeTabletTool, RawTabletSample, TabletButtonMask, TabletMonitorEvent,
};

#[cfg(target_os = "macos")]
pub use monitor::{TabletEventMonitor, TabletSampleHandle};
