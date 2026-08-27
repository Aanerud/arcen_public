//! The frozen transaction driver.
//!
//! Section 3 of ADR 0010. [`OutputTransaction::acquire`] runs admission, then
//! preflight, then bind, then verify; a returned transaction has passed
//! verification and has not been committed.
//!
//! # State
//!
//! [`OutputTransactionState`] tracks the driver's own state machine and is
//! deliberately separate from
//! [`is_armed`](OutputProvider::is_armed), which tracks the provider's
//! teardown obligation. The driver never asserts that a committed binding is
//! disarmed.
//!
//! - A commit failure rolls back eagerly and the transaction ends in
//!   [`OutputTransactionState::RolledBack`], so a host's armed-drop guard
//!   finds an already-resolved transaction. When that rollback *itself*
//!   fails, the obligation is still outstanding, so the transaction stays in
//!   [`OutputTransactionState::Bound`] and [`OutputTransaction::rollback`]
//!   may be retried. Both failures are returned as one typed value.
//! - [`OutputTransaction::rollback`] is idempotent: it is a no-op once the
//!   transaction is already rolled back, and it still runs on a committed
//!   transaction, which is how a session-end teardown releases a provider
//!   that stays armed after commit.
//! - [`OutputTransaction::commit`] is a no-op on an already resolved
//!   transaction: it never re-applies a committed binding and never re-arms a
//!   rolled-back one. [`OutputTransaction::into_committed`] remains the only
//!   way to reach the committed resources, and it succeeds only in
//!   [`OutputTransactionState::Committed`].

use core::fmt;

use super::{
    OutputCapabilities, OutputContext, OutputProvider, OutputStage, OutputTransactionError, admits,
};

/// Where the driver's state machine is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OutputTransactionState {
    /// Bound and verified, not yet committed.
    Bound,
    /// Committed to the session.
    Committed,
    /// Rolled back. The driver holds no further obligation.
    RolledBack,
}

/// An acquired, verified output transaction.
///
/// This type never rolls back in `Drop`; the host keeps its own armed-drop
/// guard, because that policy needs the host's runtime knowledge.
#[must_use = "an acquired output transaction is armed until it is committed or rolled back"]
pub struct OutputTransaction<P: OutputProvider> {
    provider: P,
    binding: P::Binding,
    state: OutputTransactionState,
}

impl<P: OutputProvider> fmt::Debug for OutputTransaction<P> {
    // Neither the provider nor its binding is required to be `Debug`: a
    // binding owns operating-system handles and child processes, and a
    // provider may own driver state. Only the driver's own observable facts
    // are rendered.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OutputTransaction")
            .field("state", &self.state)
            .field("armed", &self.is_armed())
            .field("capabilities", &self.capabilities())
            .finish_non_exhaustive()
    }
}

impl<P: OutputProvider> OutputTransaction<P> {
    /// Runs admission, preflight, bind, and verify.
    ///
    /// The capability gate runs first, before any provider code, so a plan
    /// this provider cannot serve is refused without a single
    /// operating-system call.
    ///
    /// # Errors
    ///
    /// - [`OutputTransactionError::Admission`] when the shared capability
    ///   gate refuses the plan. No provider method other than
    ///   [`capabilities`](OutputProvider::capabilities) and
    ///   [`demand`](OutputProvider::demand) has run.
    /// - [`OutputTransactionError::Operation`] at
    ///   [`OutputStage::Preflight`], [`OutputStage::Bind`], or
    ///   [`OutputStage::Verify`] when that stage failed and nothing was left
    ///   behind.
    /// - [`OutputTransactionError::Rollback`] at [`OutputStage::Bind`] when
    ///   the provider's own undo failed, or at [`OutputStage::Verify`] when
    ///   the driver's rollback failed. Both failures survive.
    pub async fn acquire(
        mut provider: P,
        plan: &P::Plan,
        context: &OutputContext,
    ) -> Result<Self, OutputTransactionError<P::Error>> {
        let capabilities = provider.capabilities();
        let demand = provider.demand(plan);
        admits(&capabilities, &demand).map_err(OutputTransactionError::Admission)?;

        let prepared = provider.preflight(plan, context).map_err(|source| {
            OutputTransactionError::Operation {
                stage: OutputStage::Preflight,
                source,
            }
        })?;

        let mut binding = match provider.bind(prepared).await {
            Ok(binding) => binding,
            // `bind` produces the binding, so on failure there is nothing for
            // the driver to roll back. The provider reports its own undo.
            Err(failure) => {
                return Err(match failure.rollback {
                    None => OutputTransactionError::Operation {
                        stage: OutputStage::Bind,
                        source: failure.source,
                    },
                    Some(rollback) => OutputTransactionError::Rollback {
                        stage: OutputStage::Bind,
                        source: failure.source,
                        rollback,
                    },
                });
            }
        };

        if let Err(source) = provider.verify(&mut binding).await {
            return Err(match provider.rollback(&mut binding).await {
                Ok(()) => OutputTransactionError::Operation {
                    stage: OutputStage::Verify,
                    source,
                },
                Err(rollback) => OutputTransactionError::Rollback {
                    stage: OutputStage::Verify,
                    source,
                    rollback,
                },
            });
        }

        Ok(Self {
            provider,
            binding,
            state: OutputTransactionState::Bound,
        })
    }

    /// What the provider promises about the desktop it produced.
    pub fn capabilities(&self) -> OutputCapabilities {
        self.provider.capabilities()
    }

    /// The host-shaped evidence for this verified binding.
    pub fn evidence(&self) -> &P::Evidence {
        self.provider.evidence(&self.binding)
    }

    /// Whether the provider still holds an outstanding teardown obligation.
    pub fn is_armed(&self) -> bool {
        self.provider.is_armed(&self.binding)
    }

    /// Where the driver's state machine is.
    pub const fn state(&self) -> OutputTransactionState {
        self.state
    }

    /// Hands the verified binding to the session.
    ///
    /// A failure rolls the binding back eagerly, so the caller never has to.
    ///
    /// # Errors
    ///
    /// - [`OutputTransactionError::Operation`] at [`OutputStage::Commit`]
    ///   when commit failed and the rollback that followed succeeded. The
    ///   transaction is [`OutputTransactionState::RolledBack`].
    /// - [`OutputTransactionError::Rollback`] at [`OutputStage::Commit`] when
    ///   that rollback also failed. Both failures survive, and the
    ///   transaction stays [`OutputTransactionState::Bound`] so
    ///   [`Self::rollback`] can be retried.
    pub async fn commit(&mut self) -> Result<(), OutputTransactionError<P::Error>> {
        match self.state {
            OutputTransactionState::Committed | OutputTransactionState::RolledBack => {
                return Ok(());
            }
            OutputTransactionState::Bound => {}
        }
        let source = match self.provider.commit(&mut self.binding).await {
            Ok(()) => {
                self.state = OutputTransactionState::Committed;
                return Ok(());
            }
            Err(source) => source,
        };
        Err(match self.provider.rollback(&mut self.binding).await {
            Ok(()) => {
                self.state = OutputTransactionState::RolledBack;
                OutputTransactionError::Operation {
                    stage: OutputStage::Commit,
                    source,
                }
            }
            Err(rollback) => OutputTransactionError::Rollback {
                stage: OutputStage::Commit,
                source,
                rollback,
            },
        })
    }

    /// Releases the provider's teardown obligation.
    ///
    /// Idempotent: a no-op once the transaction is already rolled back. On a
    /// committed transaction it still runs, which is how a session-end
    /// teardown releases a provider that stays armed after commit.
    ///
    /// # Errors
    ///
    /// Returns the provider's error when the obligation could not be
    /// released. The state is unchanged, so the call may be retried.
    pub async fn rollback(&mut self) -> Result<(), P::Error> {
        if matches!(self.state, OutputTransactionState::RolledBack) {
            return Ok(());
        }
        self.provider.rollback(&mut self.binding).await?;
        self.state = OutputTransactionState::RolledBack;
        Ok(())
    }

    /// Takes the committed output, or hands the transaction back unchanged.
    ///
    /// # Errors
    ///
    /// Returns the transaction itself, still unresolved, in every state other
    /// than [`OutputTransactionState::Committed`], so the caller must resolve
    /// it.
    pub fn into_committed(self) -> Result<CommittedOutput<P>, Self> {
        match self.state {
            OutputTransactionState::Committed => Ok(CommittedOutput {
                provider: self.provider,
                binding: self.binding,
            }),
            OutputTransactionState::Bound | OutputTransactionState::RolledBack => Err(self),
        }
    }
}

/// A committed output and the provider that owns it.
#[must_use = "a committed output still owns the provider's binding"]
pub struct CommittedOutput<P: OutputProvider> {
    provider: P,
    binding: P::Binding,
}

impl<P: OutputProvider> fmt::Debug for CommittedOutput<P> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CommittedOutput")
            .field("armed", &self.is_armed())
            .field("capabilities", &self.capabilities())
            .finish_non_exhaustive()
    }
}

impl<P: OutputProvider> CommittedOutput<P> {
    /// What the provider promises about the committed desktop.
    pub fn capabilities(&self) -> OutputCapabilities {
        self.provider.capabilities()
    }

    /// The host-shaped evidence for the committed binding.
    pub fn evidence(&self) -> &P::Evidence {
        self.provider.evidence(&self.binding)
    }

    /// Whether the provider still holds a teardown obligation the host must
    /// honour at session end.
    pub fn is_armed(&self) -> bool {
        self.provider.is_armed(&self.binding)
    }

    /// Splits the committed output into the provider and its binding, so the
    /// host can take ownership of the resources it planned for.
    pub fn into_parts(self) -> (P, P::Binding) {
        (self.provider, self.binding)
    }
}

#[cfg(test)]
mod tests {
    use core::future::{Future, ready};
    use std::sync::{Arc, Mutex};

    use arcen_telemetry::CorrelationId;

    use super::{OutputTransaction, OutputTransactionState};
    use crate::block_on::block_on;
    use crate::provider::{
        BindFailure, CapabilityMismatch, OutputCapabilities, OutputContext, OutputDemand,
        OutputProvider, OutputStage, OutputSurface, OutputTransactionError, RollbackGuarantee,
    };

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Stage {
        Preflight,
        Bind,
        Verify,
        Commit,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct Plan {
        regions: usize,
        headless: bool,
    }

    impl Plan {
        const fn regions(regions: usize) -> Self {
            Self {
                regions,
                headless: false,
            }
        }
    }

    #[derive(Debug, Default, PartialEq, Eq)]
    struct Evidence {
        applied_regions: usize,
    }

    #[derive(Debug)]
    struct Binding {
        armed: bool,
    }

    /// Call log shared with the test body, so the exact stage order is still
    /// observable after a failure consumed the provider.
    #[derive(Debug, Clone, Default)]
    struct CallLog(Arc<Mutex<Vec<&'static str>>>);

    impl CallLog {
        fn push(&self, name: &'static str) {
            self.0.lock().expect("call log").push(name);
        }

        fn calls(&self) -> Vec<&'static str> {
            self.0.lock().expect("call log").clone()
        }
    }

    #[allow(clippy::struct_excessive_bools)]
    #[derive(Debug)]
    struct Fake {
        calls: CallLog,
        fail: Option<Stage>,
        fail_rollback: bool,
        bind_undo_fails: bool,
        headless_capable: bool,
        commit_disarms: bool,
        evidence: Evidence,
    }

    impl Fake {
        fn new() -> Self {
            Self {
                calls: CallLog::default(),
                fail: None,
                fail_rollback: false,
                bind_undo_fails: false,
                headless_capable: true,
                commit_disarms: true,
                evidence: Evidence::default(),
            }
        }

        fn failing(stage: Stage) -> Self {
            Self {
                fail: Some(stage),
                ..Self::new()
            }
        }

        fn failing_with_failed_rollback(stage: Stage) -> Self {
            Self {
                fail: Some(stage),
                fail_rollback: true,
                ..Self::new()
            }
        }

        fn call(&mut self, name: &'static str) {
            self.calls.push(name);
        }

        fn fails(&self, stage: Stage) -> bool {
            self.fail == Some(stage)
        }
    }

    impl OutputProvider for Fake {
        type Plan = Plan;
        type Prepared = usize;
        type Binding = Binding;
        type Evidence = Evidence;
        type Error = &'static str;

        fn capabilities(&self) -> OutputCapabilities {
            let mut capabilities = OutputCapabilities::new(
                1,
                4,
                OutputSurface::DedicatedPhysical,
                RollbackGuarantee::ExactRestore,
            )
            .expect("valid region range");
            capabilities.exact_modes = true;
            capabilities.signed_desktop_coordinates = true;
            capabilities.persistent_dedicated_desktop = true;
            capabilities.headless_capable = self.headless_capable;
            capabilities.per_region_rotation = true;
            capabilities
        }

        fn demand(&self, plan: &Self::Plan) -> OutputDemand {
            OutputDemand {
                headless: plan.headless,
                ..OutputDemand::new(plan.regions)
            }
        }

        fn preflight(
            &mut self,
            plan: &Self::Plan,
            _context: &OutputContext,
        ) -> Result<Self::Prepared, Self::Error> {
            self.call("preflight");
            if self.fails(Stage::Preflight) {
                return Err("preflight failed");
            }
            Ok(plan.regions)
        }

        fn bind(
            &mut self,
            prepared: Self::Prepared,
        ) -> impl Future<Output = Result<Self::Binding, BindFailure<Self::Error>>> + Send {
            self.call("bind");
            if self.fails(Stage::Bind) {
                let failure = if self.bind_undo_fails {
                    BindFailure::with_rollback("bind failed", "bind undo failed")
                } else {
                    BindFailure::new("bind failed")
                };
                return ready(Err(failure));
            }
            self.evidence.applied_regions = prepared;
            ready(Ok(Binding { armed: true }))
        }

        fn verify(
            &mut self,
            _binding: &mut Self::Binding,
        ) -> impl Future<Output = Result<(), Self::Error>> + Send {
            self.call("verify");
            ready(if self.fails(Stage::Verify) {
                Err("verify failed")
            } else {
                Ok(())
            })
        }

        fn commit(
            &mut self,
            binding: &mut Self::Binding,
        ) -> impl Future<Output = Result<(), Self::Error>> + Send {
            self.call("commit");
            if self.fails(Stage::Commit) {
                return ready(Err("commit failed"));
            }
            if self.commit_disarms {
                binding.armed = false;
            }
            ready(Ok(()))
        }

        fn rollback(
            &mut self,
            binding: &mut Self::Binding,
        ) -> impl Future<Output = Result<(), Self::Error>> + Send {
            self.call("rollback");
            if self.fail_rollback {
                return ready(Err("rollback failed"));
            }
            binding.armed = false;
            self.evidence.applied_regions = 0;
            ready(Ok(()))
        }

        fn evidence<'a>(&'a self, _binding: &'a Self::Binding) -> &'a Self::Evidence {
            &self.evidence
        }

        fn is_armed(&self, binding: &Self::Binding) -> bool {
            binding.armed
        }
    }

    fn context() -> OutputContext {
        OutputContext::new(CorrelationId::new("output-transaction-test").expect("correlation id"))
    }

    fn acquire(
        provider: Fake,
        plan: Plan,
    ) -> Result<OutputTransaction<Fake>, OutputTransactionError<&'static str>> {
        block_on(OutputTransaction::acquire(provider, &plan, &context()))
    }

    fn acquire_logged(
        provider: Fake,
        plan: Plan,
    ) -> (
        CallLog,
        Result<OutputTransaction<Fake>, OutputTransactionError<&'static str>>,
    ) {
        let log = provider.calls.clone();
        let result = acquire(provider, plan);
        (log, result)
    }

    fn calls(transaction: &OutputTransaction<Fake>) -> Vec<&'static str> {
        transaction.provider.calls.calls()
    }

    #[test]
    fn acquire_runs_admission_preflight_bind_then_verify() {
        for regions in 1..=4 {
            let transaction =
                acquire(Fake::new(), Plan::regions(regions)).expect("acquire succeeds");
            assert_eq!(calls(&transaction), ["preflight", "bind", "verify"]);
            assert_eq!(transaction.state(), OutputTransactionState::Bound);
            assert!(transaction.is_armed());
            assert_eq!(transaction.evidence().applied_regions, regions);
            assert_eq!(transaction.capabilities().max_regions(), 4);
        }
    }

    #[test]
    fn admission_refusal_runs_no_provider_stage() {
        for regions in [0, 5] {
            let (log, result) = acquire_logged(Fake::new(), Plan::regions(regions));
            let error = result.expect_err("must refuse");
            assert_eq!(
                error,
                OutputTransactionError::Admission(CapabilityMismatch::RegionCount {
                    requested: regions,
                    min: 1,
                    max: 4,
                })
            );
            assert_eq!(error.stage(), OutputStage::Admission);
            assert_eq!(error.failure(), None);
            assert_eq!(error.rollback(), None);
            assert!(log.calls().is_empty());
        }
    }

    #[test]
    fn admission_refuses_an_unsupported_capability_before_preflight() {
        let provider = Fake {
            headless_capable: false,
            ..Fake::new()
        };
        let (log, result) = acquire_logged(
            provider,
            Plan {
                regions: 2,
                headless: true,
            },
        );
        assert_eq!(
            result.expect_err("must refuse"),
            OutputTransactionError::Admission(CapabilityMismatch::HeadlessUnsupported)
        );
        assert!(log.calls().is_empty());
    }

    #[test]
    fn a_preflight_failure_never_rolls_back_because_nothing_is_bound() {
        let (log, result) = acquire_logged(Fake::failing(Stage::Preflight), Plan::regions(2));
        let error = result.expect_err("preflight must fail");
        assert_eq!(
            error,
            OutputTransactionError::Operation {
                stage: OutputStage::Preflight,
                source: "preflight failed",
            }
        );
        assert!(!error.rollback_failed());
        assert_eq!(log.calls(), ["preflight"]);
    }

    #[test]
    fn a_bind_failure_reports_the_providers_own_undo() {
        let (log, result) = acquire_logged(Fake::failing(Stage::Bind), Plan::regions(2));
        assert_eq!(
            result.expect_err("bind must fail"),
            OutputTransactionError::Operation {
                stage: OutputStage::Bind,
                source: "bind failed",
            }
        );
        assert_eq!(
            log.calls(),
            ["preflight", "bind"],
            "the driver never rolls back a binding that was never produced"
        );

        let provider = Fake {
            bind_undo_fails: true,
            ..Fake::failing(Stage::Bind)
        };
        let (log, result) = acquire_logged(provider, Plan::regions(2));
        assert_eq!(
            result.expect_err("bind must fail"),
            OutputTransactionError::Rollback {
                stage: OutputStage::Bind,
                source: "bind failed",
                rollback: "bind undo failed",
            }
        );
        assert_eq!(log.calls(), ["preflight", "bind"]);
    }

    #[test]
    fn a_verify_failure_rolls_the_binding_back_and_preserves_both_failures() {
        let (log, result) = acquire_logged(Fake::failing(Stage::Verify), Plan::regions(2));
        assert_eq!(
            result.expect_err("verify must fail"),
            OutputTransactionError::Operation {
                stage: OutputStage::Verify,
                source: "verify failed",
            }
        );
        assert_eq!(log.calls(), ["preflight", "bind", "verify", "rollback"]);

        let (log, result) = acquire_logged(
            Fake::failing_with_failed_rollback(Stage::Verify),
            Plan::regions(2),
        );
        assert_eq!(
            result.expect_err("verify must fail"),
            OutputTransactionError::Rollback {
                stage: OutputStage::Verify,
                source: "verify failed",
                rollback: "rollback failed",
            }
        );
        assert_eq!(log.calls(), ["preflight", "bind", "verify", "rollback"]);
    }

    #[test]
    fn every_failing_stage_is_attributed_and_ordered() {
        let cases = [
            (
                Stage::Preflight,
                OutputStage::Preflight,
                vec!["preflight"],
                "preflight failed",
            ),
            (
                Stage::Bind,
                OutputStage::Bind,
                vec!["preflight", "bind"],
                "bind failed",
            ),
            (
                Stage::Verify,
                OutputStage::Verify,
                vec!["preflight", "bind", "verify", "rollback"],
                "verify failed",
            ),
        ];
        for (failing, expected_stage, expected_calls, expected_source) in cases {
            let (log, result) = acquire_logged(Fake::failing(failing), Plan::regions(2));
            let error = result.expect_err("stage must fail");
            assert_eq!(error.stage(), expected_stage);
            assert_eq!(error.failure(), Some(&expected_source));
            assert_eq!(log.calls(), expected_calls);
        }

        let (log, result) = acquire_logged(Fake::new(), Plan::regions(5));
        assert_eq!(
            result
                .expect_err("admission must refuse five regions")
                .stage(),
            OutputStage::Admission
        );
        assert!(
            log.calls().is_empty(),
            "admission runs before every provider stage"
        );
    }

    #[test]
    fn a_failed_rollback_is_attributed_to_the_stage_that_failed_first() {
        let cases = [
            (
                Stage::Verify,
                OutputStage::Verify,
                "verify failed",
                vec!["preflight", "bind", "verify", "rollback"],
            ),
            (
                Stage::Bind,
                OutputStage::Bind,
                "bind failed",
                vec!["preflight", "bind"],
            ),
        ];
        for (failing, expected_stage, expected_source, expected_calls) in cases {
            let provider = Fake {
                bind_undo_fails: true,
                ..Fake::failing_with_failed_rollback(failing)
            };
            let (log, result) = acquire_logged(provider, Plan::regions(2));
            let error = result.expect_err("stage must fail");
            assert_eq!(error.stage(), expected_stage);
            assert_eq!(error.failure(), Some(&expected_source));
            assert!(error.rollback_failed());
            assert_eq!(log.calls(), expected_calls);
        }
    }

    #[test]
    fn commit_transitions_to_committed_and_yields_the_committed_output() {
        let mut transaction = acquire(Fake::new(), Plan::regions(3)).expect("acquire succeeds");
        block_on(transaction.commit()).expect("commit succeeds");
        assert_eq!(transaction.state(), OutputTransactionState::Committed);
        assert!(!transaction.is_armed());
        assert_eq!(
            calls(&transaction),
            ["preflight", "bind", "verify", "commit"]
        );
        let committed = transaction.into_committed().expect("committed");
        assert_eq!(committed.evidence().applied_regions, 3);
        assert!(!committed.is_armed());
        assert_eq!(committed.capabilities().min_regions(), 1);
        let (provider, binding) = committed.into_parts();
        assert_eq!(
            provider.calls.calls(),
            ["preflight", "bind", "verify", "commit"]
        );
        assert!(!binding.armed);
    }

    #[test]
    fn a_provider_may_stay_armed_after_a_successful_commit() {
        let provider = Fake {
            commit_disarms: false,
            ..Fake::new()
        };
        let mut transaction = acquire(provider, Plan::regions(1)).expect("acquire succeeds");
        block_on(transaction.commit()).expect("commit succeeds");
        assert_eq!(transaction.state(), OutputTransactionState::Committed);
        assert!(
            transaction.is_armed(),
            "the driver must never assert that commit disarms"
        );

        block_on(transaction.rollback()).expect("session-end teardown succeeds");
        assert_eq!(transaction.state(), OutputTransactionState::RolledBack);
        assert!(!transaction.is_armed());
        assert_eq!(
            calls(&transaction),
            ["preflight", "bind", "verify", "commit", "rollback"]
        );
    }

    #[test]
    fn a_commit_failure_rolls_back_eagerly_and_ends_rolled_back() {
        let mut transaction =
            acquire(Fake::failing(Stage::Commit), Plan::regions(2)).expect("acquire succeeds");
        let error = block_on(transaction.commit()).expect_err("commit must fail");
        assert_eq!(
            error,
            OutputTransactionError::Operation {
                stage: OutputStage::Commit,
                source: "commit failed",
            }
        );
        assert_eq!(transaction.state(), OutputTransactionState::RolledBack);
        assert!(!transaction.is_armed());
        assert_eq!(
            calls(&transaction),
            ["preflight", "bind", "verify", "commit", "rollback"]
        );
        let transaction = transaction
            .into_committed()
            .expect_err("a rolled-back transaction is never committed");
        assert_eq!(transaction.state(), OutputTransactionState::RolledBack);
    }

    #[test]
    fn a_commit_failure_whose_rollback_also_fails_keeps_both_and_stays_retryable() {
        let mut transaction = acquire(
            Fake::failing_with_failed_rollback(Stage::Commit),
            Plan::regions(2),
        )
        .expect("acquire succeeds");
        let error = block_on(transaction.commit()).expect_err("commit must fail");
        assert_eq!(
            error,
            OutputTransactionError::Rollback {
                stage: OutputStage::Commit,
                source: "commit failed",
                rollback: "rollback failed",
            }
        );
        assert_eq!(
            transaction.state(),
            OutputTransactionState::Bound,
            "an outstanding obligation must stay resolvable"
        );
        assert!(transaction.is_armed());
        assert_eq!(
            block_on(transaction.rollback()).expect_err("rollback still fails"),
            "rollback failed"
        );
        assert_eq!(transaction.state(), OutputTransactionState::Bound);
    }

    #[test]
    fn rollback_is_idempotent_and_commit_never_re_arms_a_resolved_transaction() {
        let mut transaction = acquire(Fake::new(), Plan::regions(2)).expect("acquire succeeds");
        block_on(transaction.rollback()).expect("rollback succeeds");
        assert_eq!(transaction.state(), OutputTransactionState::RolledBack);
        assert!(!transaction.is_armed());

        block_on(transaction.rollback()).expect("rollback is idempotent");
        block_on(transaction.commit()).expect("commit on a resolved transaction is a no-op");
        assert_eq!(transaction.state(), OutputTransactionState::RolledBack);
        assert_eq!(
            calls(&transaction),
            ["preflight", "bind", "verify", "rollback"],
            "a resolved transaction never calls the provider again"
        );
        assert!(transaction.into_committed().is_err());
    }

    #[test]
    fn commit_is_idempotent_once_committed() {
        let mut transaction = acquire(Fake::new(), Plan::regions(2)).expect("acquire succeeds");
        block_on(transaction.commit()).expect("commit succeeds");
        block_on(transaction.commit()).expect("commit is idempotent");
        assert_eq!(
            calls(&transaction),
            ["preflight", "bind", "verify", "commit"]
        );
        assert_eq!(transaction.state(), OutputTransactionState::Committed);
    }

    #[test]
    fn into_committed_hands_an_unresolved_transaction_back_still_armed() {
        let transaction = acquire(Fake::new(), Plan::regions(2)).expect("acquire succeeds");
        let transaction = transaction
            .into_committed()
            .expect_err("a bound transaction is not committed");
        assert_eq!(transaction.state(), OutputTransactionState::Bound);
        assert!(transaction.is_armed());
    }

    #[test]
    fn transaction_futures_are_send() {
        fn assert_send<T: Send>(_value: T) {}

        let plan = Plan::regions(2);
        let context = context();
        assert_send(OutputTransaction::acquire(Fake::new(), &plan, &context));

        let mut transaction = acquire(Fake::new(), plan).expect("acquire succeeds");
        assert_send(transaction.commit());
        assert_send(transaction.rollback());
    }

    #[test]
    fn debug_renders_driver_state_without_the_binding() {
        let transaction = acquire(Fake::new(), Plan::regions(2)).expect("acquire succeeds");
        let rendered = format!("{transaction:?}");
        assert!(rendered.contains("OutputTransaction"), "{rendered}");
        assert!(rendered.contains("Bound"), "{rendered}");
        assert!(rendered.contains("armed: true"), "{rendered}");
    }
}
