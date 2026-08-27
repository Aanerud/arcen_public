//! Host-local glue between the Linux session launcher and the shared output
//! provider lifecycle.
//!
//! The lifecycle itself -- the trait, the transaction driver, the capability
//! gate, the stage vocabulary, and the typed errors -- lives in
//! `arcen-outputs` and is frozen by
//! [ADR 0010](../../../../docs/adr/0010-shared-output-provider-lifecycle.md).
//! What stays here is exactly what the shared crate must never learn: the
//! dedicated-Xorg capability promises, the translation from a Linux topology
//! plan into the shared semantic demand, and the host-shaped evidence a
//! committed dedicated Xorg produces.
//!
//! The protocol and the topology planner still know nothing about any of it.

use std::path::{Path, PathBuf};

use arcen_media::{Rotation, MAX_MULTI_MONITOR_COUNT};
use arcen_outputs::{OutputCapabilities, OutputDemand, OutputSurface, RollbackGuarantee};

use crate::display::topology::LinuxTopologyPlan;

/// What one dedicated-Xorg attempt applies.
///
/// `None` is the single-head legacy path, which derives its geometry from the
/// shipped Xorg template and the assigned GPU head. `Some` is a committed
/// 1..=4 region multi-head topology.
pub(crate) type DedicatedXorgPlan = Option<LinuxTopologyPlan>;

/// What the dedicated-Xorg provider promises about the desktop it produces.
///
/// This replaces the host-local `OutputProviderCapabilities::DEDICATED_XORG`
/// constant. Every promise is semantic, and each one is falsifiable:
///
/// - `DedicatedPhysical`: the session owns a private Xorg server on its own
///   GPU head. It never mutates the console topology.
/// - `exact_modes`: the multi-head renderer emits exact NVIDIA MetaMode
///   tokens and [`super::randr_verify::verify_applied_topology`] refuses any
///   applied geometry, rotation, primary flag, or screen bound that differs
///   from the plan. Nothing is nearest-matched.
/// - `persistent_dedicated_desktop`: the Xorg child lives for the whole
///   session, not for one call.
/// - `headless_capable`: the template's `ConnectedMonitor` option forces each
///   planned head to present as connected with no monitor attached.
/// - `per_region_rotation`: rotation is rendered as a MetaMode rotation token
///   and verified back out of `xrandr --query`.
/// - `ExactRestore`: rollback terminates the private Xorg process tree and
///   removes the session's runtime artifacts. It creates no console-visible
///   state to restore, which is the [`OutputSurface::DedicatedPhysical`] form
///   of the guarantee.
///
/// `signed_desktop_coordinates` and `fractional_scale` stay false on purpose.
/// The Linux planner translates every plan so the bounding raster starts at a
/// non-negative origin, because an Xorg `Virtual` framebuffer cannot express a
/// negative one; and `Scale120` is carried for region-scoped input mapping,
/// not applied as desktop scaling by this provider.
pub(crate) const DEDICATED_XORG_CAPABILITIES: OutputCapabilities = {
    let mut capabilities = match OutputCapabilities::new(
        1,
        MAX_MULTI_MONITOR_COUNT,
        OutputSurface::DedicatedPhysical,
        RollbackGuarantee::ExactRestore,
    ) {
        Ok(capabilities) => capabilities,
        Err(_) => panic!("dedicated Xorg serves 1..=MAX_MULTI_MONITOR_COUNT regions"),
    };
    capabilities.exact_modes = true;
    capabilities.persistent_dedicated_desktop = true;
    capabilities.headless_capable = true;
    capabilities.per_region_rotation = true;
    capabilities
};

/// Translates a dedicated-Xorg plan into the shared semantic vocabulary the
/// admission gate reads.
///
/// The gate now runs in the driver, before any provider code, so the region
/// count is refused without touching the filesystem or the display. That is
/// the one behavioural move: the old `supports_region_count` check lived
/// inside `dry_run`.
pub(crate) fn dedicated_xorg_demand(plan: &DedicatedXorgPlan) -> OutputDemand {
    let mut demand = OutputDemand::new(plan.as_ref().map_or(1, |plan| plan.monitors.len()));
    // Every dedicated head is forced connected through `ConnectedMonitor`, so
    // the session is always served with no monitor physically attached.
    demand.headless = true;
    // The Xorg child holds the desktop for the whole session.
    demand.persistent_desktop = true;
    demand.exact_modes = plan.is_some();
    demand.negative_coordinates = plan.as_ref().is_some_and(|plan| {
        plan.monitors
            .iter()
            .any(|monitor| monitor.x < 0 || monitor.y < 0)
    });
    demand.rotation = plan.as_ref().is_some_and(|plan| {
        plan.monitors
            .iter()
            .any(|monitor| monitor.rotation != Rotation::Degrees0)
    });
    demand
}

/// Host-shaped evidence for one bound dedicated Xorg.
///
/// This is the Linux half of the shared `Evidence` associated type, and the
/// counterpart of the Windows `DisplayReport` plus applied plan. It is only
/// reachable through an acquired transaction, so it can only be read after
/// verification succeeded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DedicatedXorgEvidence {
    x_display: String,
    gpu_head: String,
    display_number: u16,
    xauthority: PathBuf,
    pid: Option<u32>,
    applied_topology: Option<LinuxTopologyPlan>,
}

impl DedicatedXorgEvidence {
    pub(crate) fn new(
        x_display: &str,
        gpu_head: &str,
        display_number: u16,
        xauthority: &Path,
        pid: Option<u32>,
    ) -> Self {
        Self {
            x_display: x_display.to_string(),
            gpu_head: gpu_head.to_string(),
            display_number,
            xauthority: xauthority.to_path_buf(),
            pid,
            applied_topology: None,
        }
    }

    /// Records the topology `verify` proved the running server actually
    /// applied. Absent for the single-head legacy path, which has no plan.
    pub(crate) fn set_applied_topology(&mut self, plan: Option<LinuxTopologyPlan>) {
        self.applied_topology = plan;
    }

    pub(crate) fn x_display(&self) -> &str {
        &self.x_display
    }

    pub(crate) fn gpu_head(&self) -> &str {
        &self.gpu_head
    }

    pub(crate) const fn display_number(&self) -> u16 {
        self.display_number
    }

    pub(crate) fn xauthority(&self) -> &Path {
        &self.xauthority
    }

    pub(crate) const fn pid(&self) -> Option<u32> {
        self.pid
    }

    pub(crate) const fn applied_topology(&self) -> Option<&LinuxTopologyPlan> {
        self.applied_topology.as_ref()
    }

    /// How many regions the verified server is serving.
    pub(crate) fn regions(&self) -> usize {
        self.applied_topology
            .as_ref()
            .map_or(1, |plan| plan.monitors.len())
    }

    pub(crate) const fn multi_monitor(&self) -> bool {
        self.applied_topology.is_some()
    }
}

#[cfg(test)]
mod tests {
    use core::future::{ready, Future};
    use std::sync::{Arc, Mutex};

    use arcen_media::{
        LogicalPoint, LogicalRect, LogicalSize, PhysicalSize, Rotation, Scale120, SessionMonitorId,
        TopologyGeneration,
    };
    use arcen_outputs::{
        BindFailure, CapabilityMismatch, OutputCapabilities, OutputContext, OutputDemand,
        OutputProvider, OutputStage, OutputSurface, OutputTransaction, OutputTransactionError,
        OutputTransactionState, RollbackGuarantee,
    };
    use arcen_telemetry::CorrelationId;

    use super::{
        dedicated_xorg_demand, DedicatedXorgEvidence, DedicatedXorgPlan,
        DEDICATED_XORG_CAPABILITIES,
    };
    use crate::display::topology::{LinuxMonitorPlan, LinuxTopologyPlan};
    use crate::session::launcher::LauncherError;
    use arcen_protocol::messages::MonitorQualityIntentMsg;

    fn monitor(index: usize, rotation: Rotation, x: i32) -> LinuxMonitorPlan {
        LinuxMonitorPlan {
            session_monitor_id: SessionMonitorId::new(
                u16::try_from(index + 1).expect("bounded test id"),
            )
            .expect("nonzero monitor id"),
            client_display_id: format!("display-{index}"),
            head: format!("DFP-{index}"),
            x,
            y: 0,
            width: 1_920,
            height: 1_080,
            logical_rect: LogicalRect::new(
                LogicalPoint::new(0, 0),
                LogicalSize::from_pixels(1_920, 1_080).expect("logical size"),
            )
            .expect("logical rect"),
            physical_size: PhysicalSize::new(1_920, 1_080).expect("physical size"),
            scale: Scale120::new(120).expect("scale"),
            rotation,
            primary: index == 0,
            quality_intent: MonitorQualityIntentMsg::HostDefault,
            mode_token: "1920x1080".to_string(),
        }
    }

    fn plan(regions: usize) -> LinuxTopologyPlan {
        rotated_plan(regions, Rotation::Degrees0)
    }

    fn rotated_plan(regions: usize, rotation: Rotation) -> LinuxTopologyPlan {
        LinuxTopologyPlan {
            generation: TopologyGeneration::new(1).expect("generation"),
            virtual_width: u32::try_from(regions).expect("bounded regions") * 1_920,
            virtual_height: 1_080,
            monitors: (0..regions)
                .map(|index| {
                    monitor(
                        index,
                        rotation,
                        i32::try_from(index).expect("bounded index") * 1_920,
                    )
                })
                .collect(),
        }
    }

    fn context() -> OutputContext {
        OutputContext::new(CorrelationId::new("linux-output-parity".to_string()).expect("id"))
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Stage {
        Preflight,
        Bind,
        Verify,
        Commit,
    }

    /// A dedicated-Xorg-shaped provider that owns no operating-system
    /// resource, so the stage attribution, rollback ordering, and evidence
    /// rules the real provider relies on can be proved on any development
    /// machine.
    struct FakeXorgProvider {
        calls: Arc<Mutex<Vec<&'static str>>>,
        fail: Option<Stage>,
        fail_rollback: bool,
        bind_undo_fails: bool,
    }

    struct FakeXorgBinding {
        evidence: DedicatedXorgEvidence,
        armed: bool,
    }

    impl FakeXorgProvider {
        fn new(calls: &Arc<Mutex<Vec<&'static str>>>, fail: Option<Stage>) -> Self {
            Self {
                calls: Arc::clone(calls),
                fail,
                fail_rollback: false,
                bind_undo_fails: false,
            }
        }

        fn call(&self, name: &'static str) {
            self.calls.lock().expect("calls").push(name);
        }
    }

    impl OutputProvider for FakeXorgProvider {
        type Plan = DedicatedXorgPlan;
        type Prepared = DedicatedXorgPlan;
        type Binding = FakeXorgBinding;
        type Evidence = DedicatedXorgEvidence;
        type Error = LauncherError;

        fn capabilities(&self) -> OutputCapabilities {
            DEDICATED_XORG_CAPABILITIES
        }

        fn demand(&self, plan: &Self::Plan) -> OutputDemand {
            dedicated_xorg_demand(plan)
        }

        fn preflight(
            &mut self,
            plan: &Self::Plan,
            _context: &OutputContext,
        ) -> Result<Self::Prepared, Self::Error> {
            self.call("preflight");
            if self.fail == Some(Stage::Preflight) {
                return Err(LauncherError::XorgConfig);
            }
            Ok(plan.clone())
        }

        fn bind(
            &mut self,
            prepared: Self::Prepared,
        ) -> impl Future<Output = Result<Self::Binding, BindFailure<Self::Error>>> + Send {
            self.call("bind");
            let failed = self.fail == Some(Stage::Bind);
            let undo_failed = self.bind_undo_fails;
            ready(if failed {
                Err(BindFailure {
                    source: LauncherError::XorgStart,
                    rollback: undo_failed.then_some(LauncherError::XorgConfig),
                })
            } else {
                let mut evidence = DedicatedXorgEvidence::new(
                    ":7",
                    "DFP-0",
                    7,
                    std::path::Path::new("/run/arcen/session/Xauthority"),
                    Some(4_242),
                );
                evidence.set_applied_topology(prepared);
                Ok(FakeXorgBinding {
                    evidence,
                    armed: true,
                })
            })
        }

        fn verify(
            &mut self,
            _binding: &mut Self::Binding,
        ) -> impl Future<Output = Result<(), Self::Error>> + Send {
            self.call("verify");
            ready(if self.fail == Some(Stage::Verify) {
                Err(LauncherError::XorgStart)
            } else {
                Ok(())
            })
        }

        fn commit(
            &mut self,
            _binding: &mut Self::Binding,
        ) -> impl Future<Output = Result<(), Self::Error>> + Send {
            self.call("commit");
            ready(if self.fail == Some(Stage::Commit) {
                Err(LauncherError::XorgStart)
            } else {
                Ok(())
            })
        }

        fn rollback(
            &mut self,
            binding: &mut Self::Binding,
        ) -> impl Future<Output = Result<(), Self::Error>> + Send {
            self.call("rollback");
            ready(if self.fail_rollback {
                Err(LauncherError::XorgConfig)
            } else {
                binding.armed = false;
                Ok(())
            })
        }

        fn evidence<'a>(&'a self, binding: &'a Self::Binding) -> &'a Self::Evidence {
            &binding.evidence
        }

        fn is_armed(&self, binding: &Self::Binding) -> bool {
            binding.armed
        }
    }

    #[test]
    fn dedicated_xorg_capabilities_replace_the_host_local_constant() {
        let capabilities = DEDICATED_XORG_CAPABILITIES;
        assert_eq!(capabilities.min_regions(), 1);
        assert_eq!(capabilities.max_regions(), MAX_REGIONS);
        for regions in 1..=MAX_REGIONS {
            assert!(capabilities.serves_region_count(regions));
        }
        assert!(!capabilities.serves_region_count(0));
        assert!(!capabilities.serves_region_count(MAX_REGIONS + 1));
        assert_eq!(capabilities.surface, OutputSurface::DedicatedPhysical);
        assert_eq!(capabilities.rollback, RollbackGuarantee::ExactRestore);
        assert!(capabilities.headless_capable);
        assert!(capabilities.exact_modes);
        assert!(capabilities.persistent_dedicated_desktop);
        assert!(capabilities.per_region_rotation);
        assert!(!capabilities.signed_desktop_coordinates);
        assert!(!capabilities.fractional_scale);
    }

    const MAX_REGIONS: usize = arcen_media::MAX_MULTI_MONITOR_COUNT;

    #[test]
    fn every_admissible_dedicated_xorg_plan_is_still_admitted() {
        for regions in 1..=MAX_REGIONS {
            assert_eq!(
                arcen_outputs::admits(
                    &DEDICATED_XORG_CAPABILITIES,
                    &dedicated_xorg_demand(&Some(plan(regions))),
                ),
                Ok(())
            );
        }
        assert_eq!(
            arcen_outputs::admits(&DEDICATED_XORG_CAPABILITIES, &dedicated_xorg_demand(&None)),
            Ok(())
        );
        assert_eq!(
            arcen_outputs::admits(
                &DEDICATED_XORG_CAPABILITIES,
                &dedicated_xorg_demand(&Some(rotated_plan(2, Rotation::Degrees270))),
            ),
            Ok(())
        );
    }

    #[tokio::test]
    async fn capability_gate_refuses_zero_and_five_regions_before_any_provider_code() {
        for regions in [0, MAX_REGIONS + 1] {
            let calls = Arc::new(Mutex::new(Vec::new()));
            let error = OutputTransaction::acquire(
                FakeXorgProvider::new(&calls, None),
                &Some(plan(regions)),
                &context(),
            )
            .await
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
            assert!(calls.lock().expect("calls").is_empty());
        }
    }

    #[tokio::test]
    async fn capability_gate_refuses_a_negative_origin_the_virtual_framebuffer_cannot_express() {
        let mut refused = plan(2);
        refused.monitors[1].x = -1_920;
        let calls = Arc::new(Mutex::new(Vec::new()));
        let error = OutputTransaction::acquire(
            FakeXorgProvider::new(&calls, None),
            &Some(refused),
            &context(),
        )
        .await
        .expect_err("must refuse");
        assert!(matches!(
            error,
            OutputTransactionError::Admission(CapabilityMismatch::SignedCoordinatesUnsupported)
        ));
        assert!(calls.lock().expect("calls").is_empty());
    }

    #[tokio::test]
    async fn lifecycle_commits_only_after_verification_and_exposes_evidence() {
        for regions in 1..=MAX_REGIONS {
            let calls = Arc::new(Mutex::new(Vec::new()));
            let mut transaction = OutputTransaction::acquire(
                FakeXorgProvider::new(&calls, None),
                &Some(plan(regions)),
                &context(),
            )
            .await
            .expect("acquired");
            assert_eq!(transaction.state(), OutputTransactionState::Bound);
            assert!(transaction.is_armed());
            assert_eq!(transaction.evidence().regions(), regions);
            assert!(transaction.evidence().multi_monitor());
            assert_eq!(transaction.evidence().x_display(), ":7");
            assert_eq!(transaction.evidence().display_number(), 7);
            assert_eq!(transaction.evidence().pid(), Some(4_242));
            transaction.commit().await.expect("commit");
            assert_eq!(transaction.state(), OutputTransactionState::Committed);
            assert_eq!(
                *calls.lock().expect("calls"),
                ["preflight", "bind", "verify", "commit"]
            );
            let committed = transaction.into_committed().expect("committed");
            assert_eq!(committed.evidence().regions(), regions);
            assert!(committed.is_armed());
        }
    }

    #[tokio::test]
    async fn single_head_evidence_reports_one_region_and_no_topology() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut transaction =
            OutputTransaction::acquire(FakeXorgProvider::new(&calls, None), &None, &context())
                .await
                .expect("acquired");
        assert_eq!(transaction.evidence().regions(), 1);
        assert!(!transaction.evidence().multi_monitor());
        assert_eq!(transaction.evidence().applied_topology(), None);
        assert_eq!(transaction.evidence().gpu_head(), "DFP-0");
        assert_eq!(
            transaction.evidence().xauthority(),
            std::path::Path::new("/run/arcen/session/Xauthority")
        );
        transaction.rollback().await.expect("rollback");
    }

    #[tokio::test]
    async fn every_stage_failure_keeps_its_own_attribution() {
        for (stage, expected, calls_expected) in [
            (Stage::Preflight, OutputStage::Preflight, vec!["preflight"]),
            (Stage::Bind, OutputStage::Bind, vec!["preflight", "bind"]),
            (
                Stage::Verify,
                OutputStage::Verify,
                vec!["preflight", "bind", "verify", "rollback"],
            ),
        ] {
            let calls = Arc::new(Mutex::new(Vec::new()));
            let error = OutputTransaction::acquire(
                FakeXorgProvider::new(&calls, Some(stage)),
                &Some(plan(2)),
                &context(),
            )
            .await
            .expect_err("must fail");
            assert_eq!(error.stage(), expected);
            assert!(!error.rollback_failed());
            assert!(error.failure().is_some());
            assert_eq!(*calls.lock().expect("calls"), calls_expected);
        }
    }

    #[tokio::test]
    async fn a_commit_failure_rolls_back_and_reports_the_commit_stage() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut transaction = OutputTransaction::acquire(
            FakeXorgProvider::new(&calls, Some(Stage::Commit)),
            &Some(plan(2)),
            &context(),
        )
        .await
        .expect("acquired");
        let error = transaction.commit().await.expect_err("commit must fail");
        assert_eq!(error.stage(), OutputStage::Commit);
        assert!(!error.rollback_failed());
        assert_eq!(transaction.state(), OutputTransactionState::RolledBack);
        assert!(!transaction.is_armed());
        assert_eq!(
            *calls.lock().expect("calls"),
            ["preflight", "bind", "verify", "commit", "rollback"]
        );
    }

    #[tokio::test]
    async fn both_failures_survive_when_the_rollback_also_fails() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut provider = FakeXorgProvider::new(&calls, Some(Stage::Verify));
        provider.fail_rollback = true;
        let error = OutputTransaction::acquire(provider, &Some(plan(2)), &context())
            .await
            .expect_err("must fail");
        assert_eq!(error.stage(), OutputStage::Verify);
        assert!(error.rollback_failed());
        assert!(matches!(error.failure(), Some(LauncherError::XorgStart)));
        assert!(matches!(error.rollback(), Some(LauncherError::XorgConfig)));

        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut provider = FakeXorgProvider::new(&calls, Some(Stage::Commit));
        provider.fail_rollback = true;
        let mut transaction = OutputTransaction::acquire(provider, &Some(plan(2)), &context())
            .await
            .expect("acquired");
        let error = transaction.commit().await.expect_err("commit must fail");
        assert_eq!(error.stage(), OutputStage::Commit);
        assert!(error.rollback_failed());
        // The obligation is still outstanding, so the transaction stays bound
        // and the host may retry the rollback.
        assert_eq!(transaction.state(), OutputTransactionState::Bound);
        assert!(transaction.is_armed());
    }

    #[tokio::test]
    async fn a_bind_that_cannot_undo_itself_reports_both_failures_without_a_driver_rollback() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut provider = FakeXorgProvider::new(&calls, Some(Stage::Bind));
        provider.bind_undo_fails = true;
        let error = OutputTransaction::acquire(provider, &Some(plan(2)), &context())
            .await
            .expect_err("must fail");
        assert_eq!(error.stage(), OutputStage::Bind);
        assert!(error.rollback_failed());
        // `bind` produces the binding, so the driver has nothing to roll back
        // and must not call the provider's rollback.
        assert_eq!(*calls.lock().expect("calls"), ["preflight", "bind"]);
    }
}
