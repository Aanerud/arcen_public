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
//! [`dispatch::TabletEventDispatcher`] then folds a drained sample batch
//! into an edge-preserving, motion-coalesced output ready to send, and
//! `ui/app.rs` owns the [`runtime::TabletRuntime`] RAII guard, negotiates
//! typed pen with the host over `arcen_protocol::messages::PenEventMsg`,
//! maps window points through the current video `image_rect`, and
//! suppresses the egui mouse-emulation duplicates while a real pen has
//! authority.
//!
//! Every file in this module except `monitor.rs` and `hid_inventory.rs`
//! forbids `unsafe_code` individually (`#![forbid(unsafe_code)]` cannot be declared here at the
//! `tablet` module level without also — unoverridably — forbidding the
//! legitimate, safety-commented AppKit and I/O Registry FFI those two need).

pub mod dispatch;
pub mod hid_inventory;
pub mod mapper;
#[cfg(target_os = "macos")]
pub mod monitor;
pub mod presence;
pub mod probe;
pub mod runtime;
pub mod sample;

pub use dispatch::{TabletDispatchOverflow, TabletEventDispatcher};
pub use mapper::{TabletMapper, ViewSize};
pub use presence::{
    attached_pen_tablets, note_pen_observed, tablet_presence, typed_pen_capability,
};
pub use probe::TabletCapabilityProbe;
pub use runtime::TabletRuntime;
pub use sample::{
    BoundedSampleQueue, NativeTabletPoint, NativeTabletProximity, NativeTabletTool,
    RawTabletSample, TabletButtonMask,
};

#[cfg(target_os = "macos")]
pub use monitor::{TabletEventMonitor, TabletSampleHandle};
