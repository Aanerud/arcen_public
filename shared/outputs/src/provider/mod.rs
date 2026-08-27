//! The frozen output provider lifecycle of ADR 0010.
//!
//! One trait, one driver, one capability vocabulary, and one error model
//! replace the three host-local output contracts that existed before it.
//! Hosts keep their plans, reports, journals, watchdogs, and runtime policy;
//! this module owns the ordering, the gate, the state machine, and the error
//! shape.
//!
//! # Ownership rules
//!
//! These are falsifiable claims an implementer must satisfy:
//!
//! - [`preflight`](OutputProvider::preflight) makes no operating-system-visible
//!   mutation. Dropping a [`Prepared`](OutputProvider::Prepared) is always
//!   safe and never requires rollback. A `Prepared` that owns a handle, a
//!   journal arm, or a child process is a defect.
//! - [`Binding`](OutputProvider::Binding) owns every operating-system-visible
//!   resource, handle, journal arm, and child process created by
//!   [`bind`](OutputProvider::bind).
//!   [`rollback`](OutputProvider::rollback) needs nothing else.
//! - The provider holds only driver-level state that outlives one attempt,
//!   for example a loaded driver or an inherited control handle. Per-attempt
//!   state belongs in the binding.
//! - [`Evidence`](OutputProvider::Evidence) is only read through the
//!   transaction, which only exists after verification succeeded.
//!
//! # Cancellation and drop
//!
//! Dropping a transition future is allowed, and it is not an undo. A provider
//! must record operating-system-visible mutation into its binding or into an
//! out-of-process journal before the first await point that can be
//! cancelled, so a later rollback still undoes it.
//! [`bind`](OutputProvider::bind) is the one stage where cancellation cannot
//! be repaired by the driver, because no binding exists yet; every provider
//! must therefore own a synchronous last-resort release that does not require
//! being polled again.
//!
//! [`OutputTransaction`] never rolls back in `Drop`: it cannot await, and
//! this crate embeds no executor, so a `Drop` rollback would either block a
//! runtime worker or silently skip. It is `#[must_use]` instead, and the host
//! keeps its own armed-drop guard. This crate applies no timeout and no
//! retry; every deadline stays with the provider.

mod capability;
mod error;
mod transaction;

use core::fmt;
use core::future::Future;

use arcen_telemetry::CorrelationId;

pub use capability::{
    CapabilityMismatch, CapabilityRangeError, OutputCapabilities, OutputDemand, OutputSurface,
    RollbackGuarantee, admits,
};
pub use error::{BindFailure, OutputStage, OutputTransactionError};
pub use transaction::{CommittedOutput, OutputTransaction, OutputTransactionState};

/// Ambient, host-independent context for one provisioning attempt.
///
/// Carries the session's correlation identifier today and grows additively.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputContext {
    session_log_id: CorrelationId,
}

impl OutputContext {
    /// Builds the context for one session's provisioning attempt.
    #[must_use]
    pub const fn new(session_log_id: CorrelationId) -> Self {
        Self { session_log_id }
    }

    /// The correlation identifier every log and telemetry event for this
    /// attempt must carry.
    #[must_use]
    pub const fn session_log_id(&self) -> &CorrelationId {
        &self.session_log_id
    }
}

/// A host output backend, driven by [`OutputTransaction`].
///
/// Providers are used through generics; object safety is not a requirement,
/// and no `dyn OutputProvider` exists. A host that picks between backends
/// wraps them in its own enum.
///
/// Every transition returns `impl Future<Output = ...> + Send` rather than
/// using `async fn`, because `async fn` in a trait desugars without a `Send`
/// bound and a host could then not drive the transaction inside a spawned
/// task. A synchronous provider satisfies the shape with
/// [`core::future::ready`] and keeps its blocking calls exactly as they are.
pub trait OutputProvider {
    /// Host-owned request. This crate never inspects it.
    type Plan;
    /// Inert result of preflight. Holds no operating-system resource.
    type Prepared;
    /// Owns every operating-system-visible resource created by
    /// [`bind`](Self::bind).
    type Binding;
    /// Host-shaped result, meaningful only after [`verify`](Self::verify)
    /// returned `Ok`.
    type Evidence;
    /// Host error. Deliberately not [`std::error::Error`], so a host whose
    /// provider errors are still `String` can migrate without a simultaneous
    /// error-type rewrite. `Sync` is not required, because the driver only
    /// moves errors, never shares them.
    type Error: fmt::Debug + fmt::Display + Send + 'static;

    /// What this provider promises about the desktop it produces.
    fn capabilities(&self) -> OutputCapabilities;

    /// Translates a host plan into the shared semantic vocabulary the
    /// admission gate reads.
    fn demand(&self, plan: &Self::Plan) -> OutputDemand;

    /// Preflights the plan without mutating anything operating-system-visible.
    ///
    /// Synchronous by design: preflight must never need an executor. A
    /// provider that needs asynchronous probing does it in
    /// [`bind`](Self::bind), which already owns the failure and cleanup path.
    ///
    /// # Errors
    ///
    /// Returns the host error describing why the plan cannot be prepared.
    fn preflight(
        &mut self,
        plan: &Self::Plan,
        context: &OutputContext,
    ) -> Result<Self::Prepared, Self::Error>;

    /// Creates the binding that owns every resource this attempt makes.
    ///
    /// # Errors
    ///
    /// Returns a [`BindFailure`] carrying the primary failure and, when the
    /// provider had already mutated something, the outcome of its own undo.
    fn bind(
        &mut self,
        prepared: Self::Prepared,
    ) -> impl Future<Output = Result<Self::Binding, BindFailure<Self::Error>>> + Send;

    /// Confirms the applied state matches the plan.
    ///
    /// # Errors
    ///
    /// Returns the host error describing the mismatch. The driver rolls the
    /// binding back.
    fn verify(
        &mut self,
        binding: &mut Self::Binding,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;

    /// Hands the verified binding to the session.
    ///
    /// A provider is not required to disarm here: a virtual provider whose
    /// monitors must still be removed at session end stays armed after a
    /// successful commit, and that is correct.
    ///
    /// # Errors
    ///
    /// Returns the host error describing why the binding cannot be handed
    /// over. The driver rolls the binding back.
    fn commit(
        &mut self,
        binding: &mut Self::Binding,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;

    /// Releases the teardown obligation this binding holds.
    ///
    /// Returning `Ok(())` is a claim. For
    /// [`OutputSurface::SharedPhysical`] it claims the host has at least one
    /// active, usable output, either the exact pre-bind topology or a
    /// verified safe-primary topology. For
    /// [`OutputSurface::DedicatedPhysical`] and [`OutputSurface::Virtual`] it
    /// claims every resource the provider created has been released and the
    /// console topology was not disturbed. A provider that cannot prove its
    /// claim returns `Err` rather than `Ok`. This is the ADR 0009
    /// non-headless invariant expressed as a postcondition on one function.
    ///
    /// Rollback must be idempotent and safe to call repeatedly.
    ///
    /// # Errors
    ///
    /// Returns the host error describing why the obligation could not be
    /// released. The obligation still stands.
    fn rollback(
        &mut self,
        binding: &mut Self::Binding,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;

    /// The host-shaped evidence for this binding.
    ///
    /// Borrowing from both halves lets a provider store evidence in either
    /// one.
    fn evidence<'a>(&'a self, binding: &'a Self::Binding) -> &'a Self::Evidence;

    /// Whether the provider still holds an outstanding teardown obligation
    /// whose omission would leave the host in a state the operator did not
    /// choose.
    ///
    /// This becomes true at the first thing [`bind`](Self::bind) creates that
    /// outlives a crash, whether that is an operating-system-visible mutation
    /// or a recovery artifact armed before the mutation. It stays true until
    /// rollback completes, or until commit explicitly releases the
    /// obligation.
    fn is_armed(&self, binding: &Self::Binding) -> bool;
}
