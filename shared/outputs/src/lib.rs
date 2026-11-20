// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Arcen shared output lifecycle primitives.
//!
//! This crate owns the host-independent half of "give this session the
//! outputs it asked for, atomically, and never leave the machine in a state
//! the operator did not choose". It is the single implementation of four
//! policies that Linux and Windows hosts previously wrote twice, in two
//! incompatible shapes:
//!
//! - [`provider`]: the frozen output provider lifecycle of
//!   [ADR 0010](../../../docs/adr/0010-shared-output-provider-lifecycle.md) —
//!   `Plan`/`Prepared`/`Binding`/`Evidence`/`Error`, a capability gate that
//!   runs before any provider code, typed stage attribution, and a
//!   transaction driver whose rollback failures are never flattened into a
//!   formatted string.
//! - [`atomic_start`]: the generic all-or-reverse-rollback start policy that
//!   both hosts hand-wrote as `spawn_all_or_rollback`.
//! - [`fairness`]: the validated per-region roster plus the deterministic
//!   round-robin service order and close-one-clear-all teardown policy that
//!   both hosts hand-wrote in their video multiplexers.
//! - [`admission`]: the gate ordering, degrade attribution, and carrier
//!   intersection policy that both hosts hand-wrote in their multi-monitor
//!   admission gates, plus the frozen multi-region order (offer, carrier,
//!   request conversion, planning, media policy) behind
//!   [`admission::RegionAdmissionPolicy`].
//! - [`applied`]: the planned-region/resolved-media/negotiated-budget join,
//!   the applied origin translation, and the "publish the negotiated bitrate
//!   budget verbatim" rule that both hosts hand-wrote when assembling their
//!   applied multi-region capability.
//! - [`display_lease`]: what a session leaves behind — when its display
//!   changes are returned, which displays are removed or restored (never a
//!   display Arcen did not create), and which windows move back to a
//!   display the local user can see.
//! - [`virtual_topology`]: placement of a requested topology onto virtual
//!   outputs a compositor creates on demand (no head inventory).
//! - [`ui_scale`]: the desktop UI scale for one virtual output, decided from
//!   the client display's facts among the scales the compositor supports.
//! - [`windows_topology`] and [`windows_recovery`]: the Windows Pier's
//!   physical multi-monitor planner (requested topology onto a probed output
//!   inventory, keyed by adapter LUID and CCD target id) and the versioned
//!   full-topology recovery model. The Pier keeps only the CCD, NVAPI and
//!   `IddCx` calls that enumerate and apply.
//! - [`windows_advanced_colour`]: when a Windows session display's colour
//!   state (SDR, WCG or HDR composition and the user's HDR toggle) admits
//!   capture, which HDR toggle to change, and what to restore. The Pier
//!   keeps only the `DisplayConfig` calls that read and set it.
//! - [`xorg_gpu`]: which PCI GPU a dedicated NVIDIA Xorg `Device` section
//!   names (`BusID`), decided from host-reported PCI facts, so a template
//!   never carries one machine's GPU address to another.
//!
//! # Dependency and runtime boundary
//!
//! The crate depends on exactly `arcen-media` (for
//! [`arcen_media::MAX_MULTI_MONITOR_COUNT`] and the shared region value
//! objects) and `arcen-telemetry` (for
//! [`arcen_telemetry::CorrelationId`]). It embeds no executor, no timer, no
//! thread, no task, and no I/O; every transition is an ordinary
//! [`core::future::Future`] the host drives on its own runtime. It never
//! names a host queue, child process, display handle, protocol message, or
//! network type: everything host-shaped enters through an associated type or
//! a caller-supplied closure.
//!
//! Because the crate carries no dev-dependency either, its own tests drive
//! futures with an in-crate `block_on` helper rather than a runtime attribute
//! macro.

#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used))]

pub mod admission;
pub mod applied;
pub mod atomic_start;
pub mod console_layout;
pub mod display_lease;
pub mod edid;
pub mod fairness;
pub mod nvidia_heads;
pub mod provider;
pub mod ui_scale;
pub mod virtual_topology;
pub mod windows_advanced_colour;
pub mod windows_multi_monitor;
pub mod windows_recovery;
pub mod windows_topology;
pub mod xorg_gpu;
pub mod xorg_screen;

#[cfg(test)]
mod block_on;

pub use admission::{
    AdmissionGates, AdmissionOutcome, AdmissionRejection, AdmissionStage, CarrierMismatch,
    DegradeReason, GateClosed, OfferEncoder, OfferPolicy, OfferWithheld, RegionAdmissionPolicy,
    admit, admit_regions, offer_max_monitors, select_carrier,
};
pub use applied::{
    AppliedRegion, AppliedRegionAssembler, OriginTranslation, OriginTranslationOverflow,
    assemble_applied_regions,
};
pub use atomic_start::{AtomicStartFailure, RollbackFailure, start_all_or_rollback};
pub use edid::{
    DetailedTiming, EDID_HDR10_LEN, EDID_MANUFACTURER_ID, EdidRequest,
    HDR10_MAX_TMDS_CHARACTER_RATE_MHZ, MONITOR_NAME, cvt_reduced_blanking, generate,
    generate_hdr10, generate_named, hdr10_link_fits, hdr10_refresh_hz, validate,
};
pub use fairness::{FairRoster, RosterError, ServiceOrder};
pub use nvidia_heads::{
    NvidiaXorgOutput, choose_nvidia_xorg_heads, is_nvidia_dfp_head_token, nvidia_dfp_head_index,
    parse_nvidia_xorg_outputs, rank_nvidia_xorg_heads,
};
pub use provider::{
    BindFailure, CapabilityMismatch, CapabilityRangeError, CommittedOutput, OutputCapabilities,
    OutputContext, OutputDemand, OutputProvider, OutputStage, OutputSurface, OutputTransaction,
    OutputTransactionError, OutputTransactionState, RollbackGuarantee, admits,
};
pub use ui_scale::{
    OutputScaleChoice, OutputScaleFacts, ScaleBasis, WINDOWS_UI_SCALE_PERCENTS, WindowsUiScale,
    choose_output_scale,
};
pub use virtual_topology::{
    VirtualOutputLimits, VirtualOutputPlan, VirtualTopologyError, VirtualTopologyPlan,
    plan_virtual_topology,
};
pub use windows_multi_monitor::{
    AdminHeadlessMode, WindowsCcdModePlan, WindowsCcdOutputKind, WindowsMultiMonitorAdapter,
    WindowsMultiMonitorPolicy, WindowsMultiMonitorRefusal, WindowsMultiMonitorSelection,
    WindowsMultiMonitorSelectionReason, resolve_windows_multi_monitor_adapter,
    windows_ccd_mode_plan,
};
pub use xorg_gpu::{
    BoundDriver, GpuCandidate, NVIDIA_PCI_VENDOR, PciAddress, PciAddressError, XorgGpuDecision,
    XorgTemplateError, apply_bus_id, decide_xorg_gpu, resolve_xorg_gpu,
};
pub use xorg_screen::{RenderedScreen, SessionScreen, raised_virtual, with_session_screen};
