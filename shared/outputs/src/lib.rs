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
pub mod fairness;
pub mod provider;

#[cfg(test)]
mod block_on;

pub use admission::{
    AdmissionGates, AdmissionOutcome, AdmissionRejection, AdmissionStage, CarrierMismatch,
    DegradeReason, GateClosed, RegionAdmissionPolicy, admit, admit_regions, select_carrier,
};
pub use applied::{
    AppliedRegion, AppliedRegionAssembler, OriginTranslation, OriginTranslationOverflow,
    assemble_applied_regions,
};
pub use atomic_start::{AtomicStartFailure, RollbackFailure, start_all_or_rollback};
pub use fairness::{FairRoster, RosterError, ServiceOrder};
pub use provider::{
    BindFailure, CapabilityMismatch, CapabilityRangeError, CommittedOutput, OutputCapabilities,
    OutputContext, OutputDemand, OutputProvider, OutputStage, OutputSurface, OutputTransaction,
    OutputTransactionError, OutputTransactionState, RollbackGuarantee, admits,
};
