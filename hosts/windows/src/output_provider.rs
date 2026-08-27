//! Host-local glue between the Windows display stack and the shared output
//! provider lifecycle.
//!
//! The lifecycle itself -- the trait, the transaction driver, the capability
//! gate, the stage vocabulary, and the typed errors -- lives in
//! `arcen-outputs` and is frozen by
//! [ADR 0010](../../../docs/adr/0010-shared-output-provider-lifecycle.md).
//! What stays here is exactly what the shared crate must never learn: the
//! physical and IddCx capability promises, the translation from a Windows
//! topology plan into the shared semantic demand, the host-shaped evidence a
//! bound topology produces, and the two reconciliation points between a
//! synchronous Windows display stack and an asynchronous shared trait.
//!
//! # Why this host still runs the lifecycle synchronously
//!
//! Every Windows transition is a blocking CCD/NVAPI/IddCx call and the whole
//! transaction is driven from inside `tokio::task::spawn_blocking`, while
//! [`MultiDisplayLease`](crate::display::MultiDisplayLease)'s armed-drop guard
//! must roll back from `Drop`, which cannot await. The providers therefore
//! satisfy the trait's future-returning shape with [`core::future::ready`],
//! and [`block_on`] drives those already-complete futures to their value on
//! the calling thread. No runtime is created, no dependency is added to the
//! shared crate, and no blocking call moves off the thread it ran on before.

use core::future::Future;

use arcen_media::{Rotation, MAX_MULTI_MONITOR_COUNT};
use arcen_outputs::{
    OutputCapabilities, OutputDemand, OutputStage, OutputSurface, OutputTransactionError,
    RollbackGuarantee,
};

use crate::display::DisplayReport;
use crate::logging::DISPLAY;
use crate::multi_monitor_topology::WindowsTopologyPlan;

/// What the physical CCD/NVAPI provider promises about the desktop it
/// produces.
///
/// This replaces the host-local `OutputProviderCapabilities` literal that used
/// to live inline in the provider. Every promise is semantic and falsifiable:
///
/// - `SharedPhysical`: the provider mutates the one console desktop the
///   logged-on user also sees. That surface is why admission demands at least
///   [`RollbackGuarantee::SafePrimary`] of it.
/// - `exact_modes`: NVAPI applies exact timings and
///   `verify_multi_display_plan` refuses any applied geometry, refresh,
///   rotation, or primary flag that differs from the plan. Nothing is
///   nearest-matched.
/// - `signed_desktop_coordinates`: the Windows virtual desktop is signed, and
///   the planner routinely places secondary regions at a negative origin.
/// - `persistent_dedicated_desktop`: the applied topology holds for the whole
///   session rather than for one call.
/// - `per_region_rotation`: rotation is applied through the CCD path and read
///   back out of the applied topology.
/// - `ExactRestore`: `bind` writes an out-of-process recovery journal holding
///   the full original path/mode arrays plus a stable topology snapshot
///   *before* the first mutation, and arms a watchdog. Rollback restores that
///   exact topology; if this process dies first, the watchdog restores it.
///
/// `headless_capable` stays false: this provider drives outputs that Windows
/// already enumerates, so it cannot serve a plan that requires no monitor to
/// be attached. That is precisely what the IddCx provider is for.
/// `fractional_scale` stays false because `Scale120` is carried for
/// region-scoped input mapping, not applied as desktop scaling here.
pub(crate) const PHYSICAL_OUTPUT_CAPABILITIES: OutputCapabilities = {
    let mut capabilities = match OutputCapabilities::new(
        1,
        MAX_MULTI_MONITOR_COUNT,
        OutputSurface::SharedPhysical,
        RollbackGuarantee::ExactRestore,
    ) {
        Ok(capabilities) => capabilities,
        Err(_) => panic!("the physical provider serves 1..=MAX_MULTI_MONITOR_COUNT regions"),
    };
    capabilities.exact_modes = true;
    capabilities.signed_desktop_coordinates = true;
    capabilities.persistent_dedicated_desktop = true;
    capabilities.per_region_rotation = true;
    capabilities
};

/// What the IddCx provider promises about the desktop it produces.
///
/// Same promises as [`PHYSICAL_OUTPUT_CAPABILITIES`], with two differences
/// that are the whole reason the backend exists:
///
/// - `Virtual`: the monitors are synthesised by the indirect display driver,
///   so the console topology is added to rather than mutated in place.
/// - `headless_capable`: each connector carries a synthesised EDID and
///   enumerates with nothing physically attached.
///
/// `ExactRestore` still holds: `rollback` removes the generation it applied
/// and refuses to report success while any synthesised monitor survives, so
/// the pre-session topology is exactly what remains.
///
/// The region ceiling is the driver ABI's `MAX_MONITORS` rather than the media
/// constant, so a future divergence between the two is a compile-time refusal
/// here instead of a runtime surprise inside the driver.
pub(crate) const IDDCX_OUTPUT_CAPABILITIES: OutputCapabilities = {
    let mut capabilities = match OutputCapabilities::new(
        1,
        arcen_iddcx_provider::abi::MAX_MONITORS,
        OutputSurface::Virtual,
        RollbackGuarantee::ExactRestore,
    ) {
        Ok(capabilities) => capabilities,
        Err(_) => panic!("the IddCx provider serves 1..=MAX_MONITORS regions"),
    };
    capabilities.exact_modes = true;
    capabilities.signed_desktop_coordinates = true;
    capabilities.persistent_dedicated_desktop = true;
    capabilities.headless_capable = true;
    capabilities.per_region_rotation = true;
    capabilities
};

/// Translates a Windows topology plan into the shared semantic vocabulary the
/// admission gate reads.
///
/// Both Windows backends share this translation, because both consume the same
/// planner output. The gate now runs in the driver, before any provider code,
/// so an out-of-range region count is refused without touching the recovery
/// journal, the watchdog, or the display.
///
/// `exact_modes` and `persistent_desktop` are unconditional: they are what the
/// old `validate_capabilities` demanded of every provider regardless of plan,
/// and every Windows plan really does require both. `headless` stays false
/// because the plan itself never requires an unattached output -- the backend
/// choice does -- and demanding it would make the physical provider refuse
/// every plan it serves today.
pub(crate) fn windows_output_demand(plan: &WindowsTopologyPlan) -> OutputDemand {
    let mut demand = OutputDemand::new(plan.monitors.len());
    demand.exact_modes = true;
    demand.persistent_desktop = true;
    demand.negative_coordinates = plan
        .monitors
        .iter()
        .any(|monitor| monitor.x < 0 || monitor.y < 0);
    demand.rotation = plan
        .monitors
        .iter()
        .any(|monitor| monitor.rotation != Rotation::Degrees0);
    demand
}

/// Host-shaped evidence for one bound Windows topology.
///
/// This is the Windows half of the shared `Evidence` associated type: it holds
/// exactly the pair the old `OutputProvider::report` and
/// `OutputProvider::applied_plan` accessors returned, so the session layer
/// still reads a [`DisplayReport`] and the applied plan through the
/// transaction and nowhere else. `applied_plan` is not optional here because
/// both Windows backends always have one; the old `Option` existed only
/// because the trait's default returned `None`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WindowsOutputEvidence {
    report: DisplayReport,
    applied_plan: WindowsTopologyPlan,
}

impl WindowsOutputEvidence {
    /// Builds evidence for a topology that has been bound but not yet
    /// verified. The report starts as the plan's own geometry and is replaced
    /// with the read-back report during `verify`.
    pub(crate) const fn new(report: DisplayReport, applied_plan: WindowsTopologyPlan) -> Self {
        Self {
            report,
            applied_plan,
        }
    }

    /// The read-back display report for the bound topology.
    pub(crate) const fn report(&self) -> &DisplayReport {
        &self.report
    }

    /// The topology the provider actually applied. For IddCx this is the
    /// rebound plan carrying the adapter LUIDs and target identifiers Windows
    /// assigned, which differ from the requested plan.
    pub(crate) const fn applied_plan(&self) -> &WindowsTopologyPlan {
        &self.applied_plan
    }

    /// Records the report `verify` read back out of the live topology.
    pub(crate) fn set_report(&mut self, report: DisplayReport) {
        self.report = report;
    }

    /// Records the topology the operating system actually enumerated, which
    /// supersedes the requested one.
    pub(crate) fn set_applied_plan(&mut self, plan: WindowsTopologyPlan) {
        self.applied_plan = plan;
    }
}

/// Drives an already-complete provider future to its value on the calling
/// thread.
///
/// Every Windows provider transition returns [`core::future::ready`], so the
/// first poll always yields [`Poll::Ready`](core::task::Poll::Ready) and this
/// never parks. The park is kept as the honest total implementation of the
/// contract rather than an `unreachable!`, so a provider that later grows a
/// genuinely pending await still completes instead of panicking. The waker
/// unparks this thread, so such a future would still make progress.
///
/// This exists so the Windows host can keep `acquire_multi` synchronous -- it
/// runs inside `spawn_blocking`, and `MultiDisplayLease::drop` must roll back
/// without an executor -- while implementing the shared asynchronous trait.
pub(crate) fn block_on<F: Future>(future: F) -> F::Output {
    use core::pin::pin;
    use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

    fn clone(data: *const ()) -> RawWaker {
        RawWaker::new(data, &VTABLE)
    }
    fn wake(data: *const ()) {
        wake_by_ref(data);
    }
    fn wake_by_ref(data: *const ()) {
        // SAFETY: `data` is the `&std::thread::Thread` handed to
        // `RawWaker::new` below, which outlives every waker clone because this
        // function does not return until the future completes.
        unsafe { &*data.cast::<std::thread::Thread>() }.unpark();
    }
    const fn drop_waker(_data: *const ()) {}
    static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop_waker);

    let thread = std::thread::current();
    let raw = RawWaker::new(std::ptr::from_ref(&thread).cast(), &VTABLE);
    // SAFETY: the vtable only unparks the thread the pointer refers to, which
    // is alive for the whole call.
    let waker = unsafe { Waker::from_raw(raw) };
    let mut context = Context::from_waker(&waker);
    let mut future = pin!(future);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => return output,
            Poll::Pending => std::thread::park(),
        }
    }
}

/// Renders a typed transaction failure into the `String` the Windows session
/// boundary still returns, after logging the parts a flattened string loses.
///
/// The old driver flattened a double failure into
/// `"{error}; output-provider rollback also failed: {rollback}"` and threw the
/// stage away. The shared driver keeps the stage and both errors as data;
/// this renders them for the boundary and emits the stage, the primary
/// failure, and any rollback failure as separate structured fields so an
/// operator can tell a clean rollback from a desktop that is still mutated.
pub(crate) fn multi_display_provision_error(error: &OutputTransactionError<String>) -> String {
    let stage = match error.stage() {
        OutputStage::Admission => "admission",
        OutputStage::Preflight => "preflight",
        OutputStage::Bind => "bind",
        OutputStage::Verify => "verify",
        OutputStage::Commit => "commit",
        _ => "unknown",
    };
    if let Some(rollback) = error.rollback() {
        tracing::error!(
            target: DISPLAY,
            stage,
            failure = error.failure().map(String::as_str).unwrap_or_default(),
            rollback_failure = %rollback,
            "multi-display output transaction failed and its rollback also failed; \
             the console topology may still be mutated"
        );
    } else {
        tracing::warn!(
            target: DISPLAY,
            stage,
            failure = error.failure().map(String::as_str).unwrap_or_default(),
            rolled_back = error.stage() != OutputStage::Admission
                && error.stage() != OutputStage::Preflight,
            "multi-display output transaction failed"
        );
    }
    error.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcen_media::{Rotation, SessionMonitorId, TopologyGeneration};
    use arcen_outputs::{
        BindFailure, CapabilityMismatch, OutputContext, OutputProvider, OutputTransaction,
        OutputTransactionState,
    };
    use arcen_telemetry::CorrelationId;
    use std::sync::{Arc, Mutex};

    use crate::display::{DesktopRect, DisplaySize};
    use crate::multi_monitor_topology::WindowsMonitorPlan;
    use crate::nvapi::AdapterLuid;

    const MAX_REGIONS: usize = MAX_MULTI_MONITOR_COUNT;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum FailureStage {
        Preflight,
        Bind,
        Verify,
        Commit,
    }

    /// A provider shaped exactly like the real Windows ones: every transition
    /// is synchronous and satisfies the trait through `core::future::ready`,
    /// and every stage is recorded so ordering can be asserted.
    struct FakeWindowsProvider {
        calls: Arc<Mutex<Vec<&'static str>>>,
        capabilities: OutputCapabilities,
        fail_stage: Option<FailureStage>,
        fail_rollback: bool,
    }

    struct FakeWindowsBinding {
        evidence: WindowsOutputEvidence,
        armed: bool,
    }

    impl FakeWindowsProvider {
        fn new(calls: &Arc<Mutex<Vec<&'static str>>>, fail_stage: Option<FailureStage>) -> Self {
            Self {
                calls: Arc::clone(calls),
                capabilities: PHYSICAL_OUTPUT_CAPABILITIES,
                fail_stage,
                fail_rollback: false,
            }
        }

        fn failing_rollback(
            calls: &Arc<Mutex<Vec<&'static str>>>,
            fail_stage: Option<FailureStage>,
        ) -> Self {
            let mut provider = Self::new(calls, fail_stage);
            provider.fail_rollback = true;
            provider
        }

        fn with_capabilities(
            calls: &Arc<Mutex<Vec<&'static str>>>,
            capabilities: OutputCapabilities,
        ) -> Self {
            let mut provider = Self::new(calls, None);
            provider.capabilities = capabilities;
            provider
        }

        fn record(&self, stage: &'static str) {
            self.calls.lock().expect("calls").push(stage);
        }
    }

    impl OutputProvider for FakeWindowsProvider {
        type Plan = WindowsTopologyPlan;
        type Prepared = WindowsTopologyPlan;
        type Binding = FakeWindowsBinding;
        type Evidence = WindowsOutputEvidence;
        type Error = String;

        fn capabilities(&self) -> OutputCapabilities {
            self.capabilities
        }

        fn demand(&self, plan: &Self::Plan) -> OutputDemand {
            windows_output_demand(plan)
        }

        fn preflight(
            &mut self,
            plan: &Self::Plan,
            _context: &OutputContext,
        ) -> Result<Self::Prepared, Self::Error> {
            self.record("preflight");
            if self.fail_stage == Some(FailureStage::Preflight) {
                return Err("preflight failed".to_string());
            }
            Ok(plan.clone())
        }

        fn bind(
            &mut self,
            prepared: Self::Prepared,
        ) -> impl Future<Output = Result<Self::Binding, BindFailure<Self::Error>>> + Send {
            self.record("bind");
            let result = if self.fail_stage == Some(FailureStage::Bind) {
                Err(BindFailure {
                    source: "bind failed".to_string(),
                    rollback: self
                        .fail_rollback
                        .then(|| "bind rollback failed".to_string()),
                })
            } else {
                Ok(FakeWindowsBinding {
                    evidence: WindowsOutputEvidence::new(report("planned"), prepared),
                    armed: true,
                })
            };
            core::future::ready(result)
        }

        fn verify(
            &mut self,
            binding: &mut Self::Binding,
        ) -> impl Future<Output = Result<(), Self::Error>> + Send {
            self.record("verify");
            let result = if self.fail_stage == Some(FailureStage::Verify) {
                Err("verify failed".to_string())
            } else {
                binding.evidence.set_report(report("applied"));
                Ok(())
            };
            core::future::ready(result)
        }

        fn commit(
            &mut self,
            binding: &mut Self::Binding,
        ) -> impl Future<Output = Result<(), Self::Error>> + Send {
            self.record("commit");
            let result = if self.fail_stage == Some(FailureStage::Commit) {
                Err("commit failed".to_string())
            } else {
                binding.armed = false;
                Ok(())
            };
            core::future::ready(result)
        }

        fn rollback(
            &mut self,
            binding: &mut Self::Binding,
        ) -> impl Future<Output = Result<(), Self::Error>> + Send {
            self.record("rollback");
            let result = if self.fail_rollback {
                Err("rollback failed".to_string())
            } else {
                binding.armed = false;
                Ok(())
            };
            core::future::ready(result)
        }

        fn evidence<'a>(&'a self, binding: &'a Self::Binding) -> &'a Self::Evidence {
            &binding.evidence
        }

        fn is_armed(&self, binding: &Self::Binding) -> bool {
            binding.armed
        }
    }

    fn report(device_name: &str) -> DisplayReport {
        let size = DisplaySize {
            width: 1_920,
            height: 1_080,
        };
        DisplayReport {
            requested: size,
            applied: size,
            original: size,
            original_refresh_hz: 60,
            applied_refresh_hz: 60,
            exact: true,
            changed: true,
            retarget_capable: false,
            backend: "fake",
            restore_backend: "fake",
            device_name: device_name.to_string(),
            capture_output_index: 0,
            desktop_rect: DesktopRect {
                left: 0,
                top: 0,
                width: 1_920,
                height: 1_080,
            },
            effective_scale_reports: Vec::new(),
        }
    }

    fn plan(regions: usize) -> WindowsTopologyPlan {
        plan_with(regions, false, Rotation::Degrees0)
    }

    fn plan_with(regions: usize, signed: bool, rotation: Rotation) -> WindowsTopologyPlan {
        let monitors = (0..regions)
            .map(|index| WindowsMonitorPlan {
                session_monitor_id: SessionMonitorId::new(
                    u16::try_from(index + 1).expect("bounded test id"),
                )
                .expect("nonzero monitor id"),
                client_display_id: format!("display-{index}"),
                adapter_luid: AdapterLuid {
                    low_part: 1,
                    high_part: 0,
                },
                target_id: u32::try_from(index).expect("bounded target"),
                adapter_output_index: u32::try_from(index).expect("bounded output"),
                adapter_name: "fake".to_string(),
                global_index: u32::try_from(index).expect("bounded global index"),
                device_name: format!(r"\\.\DISPLAY{}", index + 1),
                x: if signed && index == 1 { -1_920 } else { 0 },
                y: 0,
                width: 1_920,
                height: 1_080,
                mode_width: 1_920,
                mode_height: 1_080,
                logical_rect: arcen_media::LogicalRect::new(
                    arcen_media::LogicalPoint::new(0, 0),
                    arcen_media::LogicalSize::from_pixels(1_920, 1_080).expect("logical size"),
                )
                .expect("logical rect"),
                scale: arcen_media::Scale120::new(120).expect("scale"),
                refresh_hz: 60,
                rotation: if index == 0 {
                    rotation
                } else {
                    Rotation::Degrees0
                },
                primary: index == 0,
            })
            .collect();
        WindowsTopologyPlan {
            generation: TopologyGeneration::new(1).expect("generation"),
            desktop_x: if signed { -1_920 } else { 0 },
            desktop_y: 0,
            desktop_width: u32::try_from(regions).expect("bounded regions") * 1_920,
            desktop_height: 1_080,
            monitors,
            requires_custom_timing: false,
        }
    }

    fn context() -> OutputContext {
        OutputContext::new(CorrelationId::new("windows-output-test").expect("correlation"))
    }

    #[test]
    fn both_windows_backends_promise_what_the_old_capability_literals_promised() {
        for (capabilities, max_regions, surface, headless) in [
            (
                PHYSICAL_OUTPUT_CAPABILITIES,
                MAX_MULTI_MONITOR_COUNT,
                OutputSurface::SharedPhysical,
                false,
            ),
            (
                IDDCX_OUTPUT_CAPABILITIES,
                arcen_iddcx_provider::abi::MAX_MONITORS,
                OutputSurface::Virtual,
                true,
            ),
        ] {
            assert_eq!(capabilities.min_regions(), 1);
            assert_eq!(capabilities.max_regions(), max_regions);
            assert_eq!(capabilities.surface, surface);
            assert!(capabilities.exact_modes);
            assert!(capabilities.signed_desktop_coordinates);
            assert!(capabilities.persistent_dedicated_desktop);
            assert!(capabilities.per_region_rotation);
            assert!(!capabilities.fractional_scale);
            assert_eq!(capabilities.rollback, RollbackGuarantee::ExactRestore);
            assert_eq!(
                capabilities.headless_capable, headless,
                "only synthesised EDIDs make a backend headless-capable; a provider \
                 that drives already-enumerated outputs cannot promise it"
            );
        }
    }

    #[test]
    fn a_shared_physical_desktop_is_only_admitted_with_a_restore_guarantee() {
        assert_eq!(
            OutputCapabilities::required_rollback(OutputSurface::SharedPhysical),
            RollbackGuarantee::SafePrimary
        );
        let mut unsafe_physical = PHYSICAL_OUTPUT_CAPABILITIES;
        unsafe_physical.rollback = RollbackGuarantee::None;
        let calls = Arc::new(Mutex::new(Vec::new()));
        let error = block_on(OutputTransaction::acquire(
            FakeWindowsProvider::with_capabilities(&calls, unsafe_physical),
            &plan(2),
            &context(),
        ))
        .expect_err("a non-restoring shared desktop must be refused");
        assert!(matches!(
            error,
            OutputTransactionError::Admission(CapabilityMismatch::RollbackGuaranteeInsufficient {
                required: RollbackGuarantee::SafePrimary,
                provided: RollbackGuarantee::None,
            })
        ));
        assert!(calls.lock().expect("calls").is_empty());
    }

    #[test]
    fn every_plan_the_old_gate_admitted_is_still_admitted() {
        for regions in 1..=MAX_REGIONS {
            for signed in [false, true] {
                for rotation in [Rotation::Degrees0, Rotation::Degrees90] {
                    let demand = windows_output_demand(&plan_with(regions, signed, rotation));
                    arcen_outputs::admits(&PHYSICAL_OUTPUT_CAPABILITIES, &demand)
                        .expect("physical provider still admits every planner output");
                    arcen_outputs::admits(&IDDCX_OUTPUT_CAPABILITIES, &demand)
                        .expect("IddCx provider still admits every planner output");
                }
            }
        }
    }

    #[test]
    fn the_demand_reports_exactly_what_the_plan_requires() {
        let simple = windows_output_demand(&plan(2));
        assert_eq!(simple.regions, 2);
        assert!(simple.exact_modes);
        assert!(simple.persistent_desktop);
        assert!(!simple.negative_coordinates);
        assert!(!simple.rotation);
        assert!(!simple.headless);
        assert!(!simple.fractional_scale);

        let signed = windows_output_demand(&plan_with(2, true, Rotation::Degrees270));
        assert!(signed.negative_coordinates);
        assert!(signed.rotation);
    }

    #[test]
    fn capability_gate_refuses_zero_or_more_than_four_regions_before_any_provider_call() {
        for regions in [0, MAX_REGIONS + 1] {
            let calls = Arc::new(Mutex::new(Vec::new()));
            let error = block_on(OutputTransaction::acquire(
                FakeWindowsProvider::new(&calls, None),
                &plan(regions),
                &context(),
            ))
            .expect_err("must refuse");
            assert_eq!(error.stage(), OutputStage::Admission);
            assert!(matches!(
                error,
                OutputTransactionError::Admission(CapabilityMismatch::RegionCount {
                    requested,
                    min: 1,
                    max: MAX_REGIONS,
                }) if requested == regions
            ));
            assert!(
                calls.lock().expect("calls").is_empty(),
                "no recovery journal, watchdog, or CCD call may run for a refused plan"
            );
        }
    }

    #[test]
    fn a_provider_without_signed_coordinates_refuses_a_negative_origin() {
        let mut unsigned = PHYSICAL_OUTPUT_CAPABILITIES;
        unsigned.signed_desktop_coordinates = false;
        let calls = Arc::new(Mutex::new(Vec::new()));
        let error = block_on(OutputTransaction::acquire(
            FakeWindowsProvider::with_capabilities(&calls, unsigned),
            &plan_with(2, true, Rotation::Degrees0),
            &context(),
        ))
        .expect_err("must refuse");
        assert!(matches!(
            error,
            OutputTransactionError::Admission(CapabilityMismatch::SignedCoordinatesUnsupported)
        ));
        assert!(calls.lock().expect("calls").is_empty());
    }

    #[test]
    fn lifecycle_commits_only_after_verification_and_exposes_report_and_applied_plan() {
        for regions in 1..=MAX_REGIONS {
            let calls = Arc::new(Mutex::new(Vec::new()));
            let mut transaction = block_on(OutputTransaction::acquire(
                FakeWindowsProvider::new(&calls, None),
                &plan(regions),
                &context(),
            ))
            .expect("acquire");
            assert_eq!(transaction.state(), OutputTransactionState::Bound);
            assert!(transaction.is_armed());
            assert_eq!(transaction.evidence().report().device_name, "applied");
            assert_eq!(
                transaction.evidence().applied_plan().monitors.len(),
                regions
            );
            block_on(transaction.commit()).expect("commit");
            assert_eq!(transaction.state(), OutputTransactionState::Committed);
            assert!(!transaction.is_armed());
            assert_eq!(
                *calls.lock().expect("calls"),
                ["preflight", "bind", "verify", "commit"]
            );
            let committed = transaction.into_committed().unwrap_or_else(|_| {
                panic!("a committed transaction must yield its provider and binding")
            });
            assert_eq!(committed.evidence().report().device_name, "applied");
        }
    }

    #[test]
    fn each_stage_failure_keeps_its_own_attribution() {
        for (stage, expected_stage, expected_calls) in [
            (
                FailureStage::Preflight,
                OutputStage::Preflight,
                vec!["preflight"],
            ),
            (
                FailureStage::Bind,
                OutputStage::Bind,
                vec!["preflight", "bind"],
            ),
            (
                FailureStage::Verify,
                OutputStage::Verify,
                vec!["preflight", "bind", "verify", "rollback"],
            ),
        ] {
            let calls = Arc::new(Mutex::new(Vec::new()));
            let error = block_on(OutputTransaction::acquire(
                FakeWindowsProvider::new(&calls, Some(stage)),
                &plan(2),
                &context(),
            ))
            .expect_err("must fail");
            assert_eq!(error.stage(), expected_stage);
            assert!(error.rollback().is_none());
            assert!(!error.rollback_failed());
            assert_eq!(*calls.lock().expect("calls"), expected_calls);
        }
    }

    #[test]
    fn a_verification_failure_rolls_the_bound_topology_back() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let error = block_on(OutputTransaction::acquire(
            FakeWindowsProvider::new(&calls, Some(FailureStage::Verify)),
            &plan(2),
            &context(),
        ))
        .expect_err("must fail");
        assert_eq!(error.stage(), OutputStage::Verify);
        assert_eq!(error.failure().map(String::as_str), Some("verify failed"));
        assert_eq!(
            *calls.lock().expect("calls"),
            ["preflight", "bind", "verify", "rollback"]
        );
    }

    #[test]
    fn a_commit_failure_rolls_back_eagerly_and_disarms() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut transaction = block_on(OutputTransaction::acquire(
            FakeWindowsProvider::new(&calls, Some(FailureStage::Commit)),
            &plan(2),
            &context(),
        ))
        .expect("acquire");
        let error = block_on(transaction.commit()).expect_err("commit must fail");
        assert_eq!(error.stage(), OutputStage::Commit);
        assert_eq!(error.failure().map(String::as_str), Some("commit failed"));
        assert!(error.rollback().is_none());
        assert_eq!(transaction.state(), OutputTransactionState::RolledBack);
        assert!(!transaction.is_armed());
        assert_eq!(
            *calls.lock().expect("calls"),
            ["preflight", "bind", "verify", "commit", "rollback"]
        );
    }

    #[test]
    fn a_double_failure_keeps_both_errors_and_leaves_the_topology_armed() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut transaction = block_on(OutputTransaction::acquire(
            FakeWindowsProvider::failing_rollback(&calls, Some(FailureStage::Commit)),
            &plan(2),
            &context(),
        ))
        .expect("acquire");
        let error = block_on(transaction.commit()).expect_err("commit must fail");
        assert_eq!(error.stage(), OutputStage::Commit);
        assert_eq!(error.failure().map(String::as_str), Some("commit failed"));
        assert_eq!(
            error.rollback().map(String::as_str),
            Some("rollback failed")
        );
        assert!(error.rollback_failed());
        assert_eq!(
            transaction.state(),
            OutputTransactionState::Bound,
            "a topology whose rollback failed is still mutated and must stay armed"
        );
        assert!(transaction.is_armed());
        let rendered = multi_display_provision_error(&error);
        assert!(rendered.contains("commit failed"), "{rendered}");
        assert!(rendered.contains("rollback failed"), "{rendered}");
    }

    #[test]
    fn a_bind_rollback_failure_is_reported_without_a_second_driver_rollback() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let error = block_on(OutputTransaction::acquire(
            FakeWindowsProvider::failing_rollback(&calls, Some(FailureStage::Bind)),
            &plan(2),
            &context(),
        ))
        .expect_err("must fail");
        assert_eq!(error.stage(), OutputStage::Bind);
        assert_eq!(error.failure().map(String::as_str), Some("bind failed"));
        assert_eq!(
            error.rollback().map(String::as_str),
            Some("bind rollback failed")
        );
        assert_eq!(
            *calls.lock().expect("calls"),
            ["preflight", "bind"],
            "the driver cannot roll back a binding that was never produced"
        );
    }

    #[test]
    fn rollback_is_idempotent_so_the_armed_drop_guard_cannot_double_restore() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut transaction = block_on(OutputTransaction::acquire(
            FakeWindowsProvider::new(&calls, None),
            &plan(2),
            &context(),
        ))
        .expect("acquire");
        block_on(transaction.rollback()).expect("rollback");
        assert!(!transaction.is_armed());
        block_on(transaction.rollback()).expect("second rollback is a no-op");
        assert_eq!(
            *calls.lock().expect("calls"),
            ["preflight", "bind", "verify", "rollback"]
        );
    }

    #[test]
    fn block_on_returns_the_value_of_a_ready_future() {
        assert_eq!(block_on(core::future::ready(7_u32)), 7);
    }
}
