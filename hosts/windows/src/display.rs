use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::logging::DISPLAY;
use crate::nvapi::AdapterLuid;

const MIN_WIDTH: u32 = 320;
const MIN_HEIGHT: u32 = 240;
const MAX_WIDTH: u32 = 16_384;
const MAX_HEIGHT: u32 = 8_640;
const RESTORE_ATTEMPTS: usize = 3;
const RESTORE_RETRY_DELAY: Duration = Duration::from_millis(100);
const MAX_FALLBACK_CANDIDATES: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DisplaySize {
    pub width: u32,
    pub height: u32,
}

impl DisplaySize {
    pub fn validate(width: u32, height: u32) -> Result<Self, String> {
        if width < MIN_WIDTH || height < MIN_HEIGHT {
            return Err(format!(
                "client display {width}x{height} is below {MIN_WIDTH}x{MIN_HEIGHT}"
            ));
        }
        if width > MAX_WIDTH || height > MAX_HEIGHT {
            return Err(format!(
                "client display {width}x{height} exceeds {MAX_WIDTH}x{MAX_HEIGHT}"
            ));
        }
        if width % 2 != 0 {
            return Err(format!(
                "client display width {width} must be even for the native encoder"
            ));
        }
        Ok(Self { width, height })
    }
}

impl std::fmt::Display for DisplaySize {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}x{}", self.width, self.height)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DisplayRequest {
    pub size: DisplaySize,
    pub refresh_hz: u32,
    pub width_mm: f32,
    pub height_mm: f32,
    pub scale: f32,
    pub product_id: u16,
    pub serial: u32,
    /// Advertise HDR10 in the synthesised EDID.
    ///
    /// The first link of the chain, and the only one Arcen controls: Windows
    /// offers Advanced Color only where the sink claims it, composites in FP16
    /// scRGB only where Advanced Color is on, and a wide capture format
    /// carries information only where DWM composited wide. Without this the
    /// whole HDR path runs correctly over an 8-bit desktop.
    pub hdr10: bool,
}

impl DisplayRequest {
    pub fn new(width: u32, height: u32) -> Result<Self, String> {
        Ok(Self {
            size: DisplaySize::validate(width, height)?,
            refresh_hz: 60,
            width_mm: 0.0,
            height_mm: 0.0,
            scale: 1.0,
            product_id: 0x0001,
            serial: 0,
            // SDR unless a session asks for HDR. An EDID claiming HDR10 on a
            // host that never streams it would make Windows offer Advanced
            // Color for nothing, and change how every ordinary desktop is
            // composited.
            hdr10: false,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DesktopRect {
    pub left: i32,
    pub top: i32,
    pub width: i32,
    pub height: i32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OutputSelector {
    GlobalIndex(u32),
    Adapter { name: String, output_index: u32 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedOutput {
    pub global_index: u32,
    pub adapter_name: String,
    pub adapter_output_index: u32,
    pub device_name: String,
    /// PCI vendor of the owning adapter (0x10de = NVIDIA); 0 off-Windows.
    pub vendor_id: u32,
    pub desktop_rect: DesktopRect,
}

#[cfg(windows)]
pub fn resolve_output_selector(selector: &OutputSelector) -> Result<ResolvedOutput, String> {
    windows_backend::resolve_output_selector(selector)
}

/// Every desktop-attached output DXGI reports, in global index order.
///
/// The selected output alone is not enough to diagnose a bad pick: when a
/// positional `GlobalIndex` selector lands on the wrong adapter there is no way
/// to tell what else was available. Callers log this next to their choice.
#[cfg(windows)]
pub fn enumerate_outputs() -> Result<Vec<ResolvedOutput>, String> {
    windows_backend::enumerate_outputs()
}

#[cfg(not(windows))]
pub fn enumerate_outputs() -> Result<Vec<ResolvedOutput>, String> {
    Ok(Vec::new())
}

/// NVIDIA's PCI vendor id. NVENC lives only on these adapters.
pub const NVIDIA_VENDOR_ID: u32 = 0x10de;

/// Re-pick an encode-capable output when a *positional* selector landed on an
/// adapter that cannot encode.
///
/// `output_index` is a global ordinal across every attached output, so it moves
/// when the set of attached displays changes — opening a hypervisor console, or
/// another agent plugging a virtual monitor, is enough to slide index 0 from the
/// GPU onto an emulated adapter. That silently costs NVENC, and with it the
/// exact-display policy that the client's display modes depend on.
///
/// Only applies when the operator did not name an adapter: an explicit
/// `Adapter { .. }` selector is an instruction, not a guess. Returns `None` when
/// the current pick is already fine or nothing better exists.
pub fn prefer_encode_capable_output<'a>(
    selector: &OutputSelector,
    selected: &ResolvedOutput,
    outputs: &'a [ResolvedOutput],
    requested: DisplaySize,
) -> Option<&'a ResolvedOutput> {
    if !matches!(selector, OutputSelector::GlobalIndex(_)) {
        return None;
    }
    let presents_requested = |output: &ResolvedOutput| {
        output.desktop_rect.width.unsigned_abs() == requested.width
            && output.desktop_rect.height.unsigned_abs() == requested.height
    };
    if selected.vendor_id == NVIDIA_VENDOR_ID && presents_requested(selected) {
        return None;
    }
    outputs
        .iter()
        .find(|output| output.vendor_id == NVIDIA_VENDOR_ID && presents_requested(output))
        .or_else(|| {
            (selected.vendor_id != NVIDIA_VENDOR_ID)
                .then(|| {
                    outputs
                        .iter()
                        .find(|output| output.vendor_id == NVIDIA_VENDOR_ID)
                })
                .flatten()
        })
}

#[cfg(not(windows))]
pub fn resolve_output_selector(selector: &OutputSelector) -> Result<ResolvedOutput, String> {
    match selector {
        OutputSelector::GlobalIndex(global_index) => Ok(ResolvedOutput {
            global_index: *global_index,
            adapter_name: "non-windows".to_string(),
            adapter_output_index: *global_index,
            device_name: "non-windows".to_string(),
            vendor_id: 0,
            desktop_rect: DesktopRect {
                left: 0,
                top: 0,
                width: 0,
                height: 0,
            },
        }),
        OutputSelector::Adapter { .. } => {
            Err("adapter-name output selection is available only on Windows".to_string())
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct DisplayScaleReport {
    pub client_display_id: String,
    pub session_monitor_id: u16,
    pub device_name: String,
    pub requested_scale_percent: u16,
    pub effective_dpi_x: u32,
    pub effective_dpi_y: u32,
    pub effective_scale_percent: u16,
    pub matches_requested: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DisplayReport {
    pub requested: DisplaySize,
    pub applied: DisplaySize,
    pub original: DisplaySize,
    pub original_refresh_hz: u32,
    pub applied_refresh_hz: u32,
    pub exact: bool,
    pub changed: bool,
    /// The live backend has an owned NVAPI snapshot and active exact timing, so
    /// an in-session retarget can be attempted and rolled back safely.
    pub retarget_capable: bool,
    pub backend: &'static str,
    pub restore_backend: &'static str,
    pub device_name: String,
    pub capture_output_index: u32,
    pub desktop_rect: DesktopRect,
    pub effective_scale_reports: Vec<DisplayScaleReport>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DisplayTarget {
    device_name: String,
    vendor_id: u32,
    adapter_luid: AdapterLuid,
    adapter_output_index: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ModeState {
    size: DisplaySize,
    refresh_hz: u32,
    output_index: u32,
    desktop_rect: DesktopRect,
    active_outputs: u32,
}

impl ModeState {
    fn is_settled_at(self, size: DisplaySize) -> bool {
        self.size == size
            && self.desktop_rect.width == size.width as i32
            && self.desktop_rect.height == size.height as i32
    }

    /// The exact-mirroring contract: the target output is the ONLY active
    /// output and it sits at (0, 0) — which is the definition of the Windows
    /// GDI primary.
    fn is_isolated_primary_at(self, size: DisplaySize) -> bool {
        self.is_settled_at(size)
            && self.desktop_rect.left == 0
            && self.desktop_rect.top == 0
            && self.active_outputs == 1
    }
}

/// How faithfully the session display must mirror the client display.
///
/// The per-encoder product rule: direct-NVENC hosts recreate the client
/// display exactly — the requested mode on the configured output, isolated as
/// the only (primary) output — or the session is refused. Software-encoder
/// and paravirtualized hosts cannot promise exactness (software limits and
/// VMware tool granularity), so they keep the
/// negotiated path: closest safe driver mode, honestly reported.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisplayPolicy {
    /// Exact client mode + topology isolation (only output, primary) — or
    /// refuse. No fallback modes.
    ExactIsolated,
    /// The pre-existing negotiation ladder: exact, then safe driver
    /// fallbacks, then the healthy current mode. Topology is left as-is.
    Negotiated,
    /// The negotiated ladder restricted to complete H.264 macroblocks.
    ///
    /// Media Foundation cannot encode partial 16x16 macroblocks. Filtering
    /// before candidate ranking prevents a fixed-mode driver from selecting an
    /// otherwise-close mode that capenc must reject after display mutation.
    NegotiatedMacroblock16,
}

impl DisplayPolicy {
    fn accepts_size(self, size: DisplaySize) -> bool {
        match self {
            Self::ExactIsolated | Self::Negotiated => true,
            Self::NegotiatedMacroblock16 => size.width % 16 == 0 && size.height % 16 == 0,
        }
    }
}

trait DisplayBackend {
    type Snapshot;

    fn select_target(&mut self, selector: &OutputSelector) -> Result<DisplayTarget, String>;
    fn prepare_target(&mut self, _target: &DisplayTarget) -> Result<(), String> {
        Ok(())
    }
    fn snapshot(&mut self, target: &DisplayTarget) -> Result<Self::Snapshot, String>;
    fn current(&mut self, target: &DisplayTarget) -> Result<ModeState, String>;
    fn supported_sizes(&mut self, target: &DisplayTarget) -> Result<Vec<DisplaySize>, String>;
    fn requires_contract_refresh(
        &self,
        _target: &DisplayTarget,
        _size: DisplaySize,
    ) -> Result<bool, String> {
        Ok(false)
    }
    fn prepare_exact_retarget(
        &mut self,
        _target: &DisplayTarget,
        _size: DisplaySize,
    ) -> Result<(), String> {
        Ok(())
    }
    fn test_mode(&mut self, target: &DisplayTarget, size: DisplaySize) -> Result<(), String>;
    fn apply_mode(
        &mut self,
        target: &DisplayTarget,
        size: DisplaySize,
    ) -> Result<ModeState, String>;
    /// Make the target the only active output, positioned at (0, 0) (the GDI
    /// primary). Only invoked under `DisplayPolicy::ExactIsolated`.
    fn isolate_topology(&mut self, target: &DisplayTarget) -> Result<ModeState, String>;
    fn restore(
        &mut self,
        target: &DisplayTarget,
        snapshot: &Self::Snapshot,
    ) -> Result<ModeState, String>;

    fn arm_recovery(
        &mut self,
        _target: &DisplayTarget,
        _snapshot: &Self::Snapshot,
    ) -> Result<(), String> {
        Ok(())
    }

    fn mark_mutation_started(&mut self) -> Result<(), String> {
        Ok(())
    }

    fn disarm_recovery(&mut self) -> Result<(), String> {
        Ok(())
    }

    fn applied_backend(&self, exact: bool) -> &'static str {
        if exact {
            "change-display-settings-ex-temporary"
        } else {
            "change-display-settings-ex-temporary-fallback"
        }
    }

    fn restore_backend(&self) -> &'static str {
        "set-display-config-plus-exact-devmode"
    }
}

struct DisplayTransaction<B: DisplayBackend> {
    backend: B,
    target: DisplayTarget,
    snapshot: Option<B::Snapshot>,
    original: ModeState,
    report: DisplayReport,
    restore_state: Option<Arc<Mutex<RestoreJournal>>>,
}

#[derive(Debug)]
struct RetargetError {
    message: String,
    mutation_attempted: bool,
}

impl RetargetError {
    fn before_mutation(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            mutation_attempted: false,
        }
    }

    fn after_mutation(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            mutation_attempted: true,
        }
    }
}

#[derive(Debug)]
enum ArmedApplyError {
    RecoveryNotReady(String),
    MutationFailed(String),
}

impl<B: DisplayBackend> DisplayTransaction<B> {
    #[cfg(test)]
    fn acquire(backend: B, output_index: u32, requested: DisplaySize) -> Result<Self, String> {
        Self::acquire_observed(
            backend,
            &OutputSelector::GlobalIndex(output_index),
            requested,
            DisplayPolicy::Negotiated,
            None,
        )
    }

    #[cfg(test)]
    fn acquire_isolated(
        backend: B,
        output_index: u32,
        requested: DisplaySize,
    ) -> Result<Self, String> {
        Self::acquire_observed(
            backend,
            &OutputSelector::GlobalIndex(output_index),
            requested,
            DisplayPolicy::ExactIsolated,
            None,
        )
    }

    fn acquire_observed(
        mut backend: B,
        selector: &OutputSelector,
        requested: DisplaySize,
        policy: DisplayPolicy,
        restore_state: Option<Arc<Mutex<RestoreJournal>>>,
    ) -> Result<Self, String> {
        let target = backend.select_target(selector)?;
        backend.prepare_target(&target)?;
        let snapshot = backend.snapshot(&target)?;
        let original = backend.current(&target)?;
        let contract_refresh_required = backend.requires_contract_refresh(&target, requested)?;
        let mut transaction = Self {
            backend,
            target: target.clone(),
            snapshot: Some(snapshot),
            original,
            report: report(
                requested,
                original,
                original,
                original.size == requested,
                false,
                "display-transaction-pending",
                "set-display-config-plus-exact-devmode",
                &target,
            ),
            restore_state,
        };

        let already_satisfied = match policy {
            DisplayPolicy::ExactIsolated => original.is_isolated_primary_at(requested),
            DisplayPolicy::Negotiated | DisplayPolicy::NegotiatedMacroblock16 => {
                policy.accepts_size(original.size) && original.is_settled_at(requested)
            }
        };
        if already_satisfied && !contract_refresh_required {
            transaction.snapshot = None;
            transaction.report = report(
                requested,
                original,
                original,
                true,
                false,
                "unchanged",
                "none",
                &target,
            );
            return Ok(transaction);
        }

        // ExactIsolated with a matching mode but an un-isolated topology:
        // skip the mode apply and only isolate under the armed journal.
        if policy == DisplayPolicy::ExactIsolated
            && original.is_settled_at(requested)
            && !contract_refresh_required
        {
            let (failure, attempted) = match transaction.isolate_armed() {
                Ok(isolated) if isolated.is_isolated_primary_at(requested) => {
                    let restore_backend = transaction.backend.restore_backend();
                    transaction.report = report(
                        requested,
                        original,
                        isolated,
                        true,
                        true,
                        "set-display-config-topology-isolation",
                        restore_backend,
                        &target,
                    );
                    transaction.record_restore_active();
                    return Ok(transaction);
                }
                Ok(isolated) => (isolation_mismatch(isolated, requested), true),
                Err(ArmedApplyError::MutationFailed(error)) => (error, true),
                Err(ArmedApplyError::RecoveryNotReady(error)) => {
                    transaction.snapshot = None;
                    transaction.record_restore_success();
                    return Err(format!(
                        "display recovery watchdog was not ready; no display mutation was \
                         attempted: {error}"
                    ));
                }
            };
            return transaction.refuse_strict(requested, failure, attempted);
        }

        let (exact_failure, exact_apply_attempted) =
            match transaction.backend.test_mode(&target, requested) {
                Ok(()) => match transaction.apply_mode_armed(requested) {
                    Ok(applied) if applied.is_settled_at(requested) => match policy {
                        DisplayPolicy::Negotiated | DisplayPolicy::NegotiatedMacroblock16 => {
                            let backend = transaction.backend.applied_backend(true);
                            let restore_backend = transaction.backend.restore_backend();
                            transaction.report = report(
                                requested,
                                original,
                                applied,
                                true,
                                true,
                                backend,
                                restore_backend,
                                &target,
                            );
                            transaction.record_restore_active();
                            return Ok(transaction);
                        }
                        DisplayPolicy::ExactIsolated => {
                            match transaction.backend.isolate_topology(&target) {
                                Ok(isolated) if isolated.is_isolated_primary_at(requested) => {
                                    let backend = transaction.backend.applied_backend(true);
                                    let restore_backend = transaction.backend.restore_backend();
                                    transaction.report = report(
                                        requested,
                                        original,
                                        isolated,
                                        true,
                                        true,
                                        backend,
                                        restore_backend,
                                        &target,
                                    );
                                    transaction.record_restore_active();
                                    return Ok(transaction);
                                }
                                Ok(isolated) => (isolation_mismatch(isolated, requested), true),
                                Err(error) => (error, true),
                            }
                        }
                    },
                    Ok(applied) => (
                        format!(
                            "driver settled at {} with rect {:?} instead of requested {requested}",
                            applied.size, applied.desktop_rect
                        ),
                        true,
                    ),
                    Err(ArmedApplyError::MutationFailed(error)) => (error, true),
                    Err(ArmedApplyError::RecoveryNotReady(error)) => {
                        transaction.snapshot = None;
                        transaction.record_restore_success();
                        return Err(format!(
                            "display recovery watchdog was not ready; no display mutation was \
                             attempted: {error}"
                        ));
                    }
                },
                Err(error) => (error, false),
            };

        if exact_apply_attempted {
            transaction
                .restore_keeping_armed()
                .map_err(|restore_error| {
                    format!(
                        "exact display apply failed ({exact_failure}); rollback also failed: \
                         {restore_error}"
                    )
                })?;
        }

        // STRICT policy: exact + isolated or refuse — no fallback negotiation.
        if policy == DisplayPolicy::ExactIsolated {
            transaction.disarm_after_rollback()?;
            transaction.snapshot = None;
            transaction.record_restore_success();
            return Err(format!(
                "this host cannot present the exact client display {requested} on {}: \
                 {exact_failure}",
                transaction.target.device_name
            ));
        }

        let supported = match transaction.backend.supported_sizes(&target) {
            Ok(supported) => supported,
            Err(error) => {
                transaction.disarm_after_rollback()?;
                transaction.snapshot = None;
                transaction.record_restore_success();
                return Err(error);
            }
        };
        let candidates =
            fallback_candidates_matching(requested, &supported, |size| policy.accepts_size(size));
        let mut rejected = vec![format!("exact {requested}: {exact_failure}")];
        for fallback in candidates {
            tracing::warn!(
                target: DISPLAY,
                requested = %requested,
                fallback = %fallback,
                reason = %exact_failure,
                "exact client display mode rejected; testing safe driver fallback"
            );
            if let Err(error) = transaction.backend.test_mode(&target, fallback) {
                rejected.push(format!("test {fallback}: {error}"));
                continue;
            }
            let applied = match transaction.apply_mode_armed(fallback) {
                Ok(applied) => applied,
                Err(ArmedApplyError::MutationFailed(error)) => {
                    transaction.restore_keeping_armed().map_err(|restore| {
                        format!(
                            "fallback display apply failed for {fallback} ({error}); rollback also \
                             failed: {restore}"
                        )
                    })?;
                    rejected.push(format!("apply {fallback}: {error}"));
                    continue;
                }
                Err(ArmedApplyError::RecoveryNotReady(error)) => {
                    transaction.snapshot = None;
                    transaction.record_restore_success();
                    return Err(format!(
                        "display recovery watchdog was not ready; no display mutation was \
                         attempted: {error}"
                    ));
                }
            };
            if applied.is_settled_at(fallback) {
                transaction.report = report(
                    requested,
                    original,
                    applied,
                    false,
                    true,
                    transaction.backend.applied_backend(false),
                    transaction.backend.restore_backend(),
                    &target,
                );
                transaction.record_restore_active();
                return Ok(transaction);
            }
            let mismatch = format!(
                "settle {fallback}: driver reached {} with rect {:?}",
                applied.size, applied.desktop_rect
            );
            transaction
                .restore_keeping_armed()
                .map_err(|restore| format!("{mismatch}; rollback also failed: {restore}"))?;
            rejected.push(mismatch);
        }

        let current = transaction.backend.current(&target)?;
        if current == original
            && current.is_settled_at(original.size)
            && policy.accepts_size(current.size)
            && DisplaySize::validate(current.size.width, current.size.height).is_ok()
        {
            transaction.disarm_after_rollback()?;
            transaction.snapshot = None;
            transaction.record_restore_success();
            transaction.report = report(
                requested,
                original,
                current,
                false,
                false,
                "unchanged-current-mode-fallback",
                "none",
                &target,
            );
            tracing::warn!(
                target: DISPLAY,
                requested = %requested,
                current = %current.size,
                rejected = ?rejected,
                "requested and driver fallback modes were unavailable; continuing on healthy current display"
            );
            Ok(transaction)
        } else {
            let error = format!(
                "display mode negotiation exhausted [{}] and current state is unsafe: {}",
                rejected.join("; "),
                restore_mismatch(current, original)
            );
            transaction.disarm_after_rollback()?;
            transaction.snapshot = None;
            transaction.record_restore_success();
            Err(error)
        }
    }

    fn report(&self) -> &DisplayReport {
        &self.report
    }

    fn retarget_exact(&mut self, requested: DisplaySize) -> Result<(), String> {
        if self.report.applied == requested {
            return Ok(());
        }
        if !self.report.retarget_capable {
            return Err(
                "live display backend did not prove NVAPI retarget and rollback capability"
                    .to_string(),
            );
        }
        let previous = self.report.clone();
        match self.retarget_exact_once(requested) {
            Ok(()) => Ok(()),
            Err(error) if !error.mutation_attempted => Err(error.message),
            Err(error) => match self.retarget_exact_once(previous.applied) {
                Ok(()) => {
                    self.report = previous;
                    Err(format!(
                        "{}; previous display mode was restored",
                        error.message
                    ))
                }
                Err(rollback_error) => Err(format!(
                    "{}; restoring the previous display mode also failed: {}",
                    error.message, rollback_error.message
                )),
            },
        }
    }

    fn retarget_exact_once(&mut self, requested: DisplaySize) -> Result<(), RetargetError> {
        if self.snapshot.is_none() {
            let current = self
                .backend
                .current(&self.target)
                .map_err(RetargetError::before_mutation)?;
            if current != self.original {
                return Err(RetargetError::before_mutation(format!(
                    "cannot arm display retarget without the original topology: {}",
                    restore_mismatch(current, self.original)
                )));
            }
            self.snapshot = Some(
                self.backend
                    .snapshot(&self.target)
                    .map_err(RetargetError::before_mutation)?,
            );
        }
        self.backend
            .prepare_exact_retarget(&self.target, requested)
            .map_err(RetargetError::before_mutation)?;
        self.backend
            .test_mode(&self.target, requested)
            .map_err(RetargetError::before_mutation)?;
        let applied = self
            .apply_mode_armed(requested)
            .map_err(|error| match error {
                ArmedApplyError::RecoveryNotReady(error) => RetargetError::before_mutation(
                    format!("display recovery was not ready for display retarget: {error}"),
                ),
                ArmedApplyError::MutationFailed(error) => {
                    RetargetError::after_mutation(format!("display retarget failed: {error}"))
                }
            })?;
        let retarget_failure = if !applied.is_settled_at(requested) {
            Some(format!(
                "display retarget settled at {} with rect {:?}, expected {requested}",
                applied.size, applied.desktop_rect
            ))
        } else {
            match self.backend.isolate_topology(&self.target) {
                Ok(isolated) if isolated.is_isolated_primary_at(requested) => {
                    self.report = report(
                        requested,
                        self.original,
                        isolated,
                        true,
                        isolated != self.original,
                        self.backend.applied_backend(true),
                        self.backend.restore_backend(),
                        &self.target,
                    );
                    self.record_restore_active();
                    return Ok(());
                }
                Ok(isolated) => Some(isolation_mismatch(isolated, requested)),
                Err(error) => Some(error),
            }
        };
        let Some(retarget_failure) = retarget_failure else {
            return Err(RetargetError::after_mutation(
                "media fallback retarget reached an invalid success state",
            ));
        };
        Err(RetargetError::after_mutation(format!(
            "display retarget failed: {retarget_failure}"
        )))
    }

    fn arm_for_mutation(&mut self) -> Result<(), ArmedApplyError> {
        let snapshot = self.snapshot.as_ref().ok_or_else(|| {
            ArmedApplyError::RecoveryNotReady(
                "display apply requested without a recovery snapshot".to_string(),
            )
        })?;
        self.backend
            .arm_recovery(&self.target, snapshot)
            .map_err(ArmedApplyError::RecoveryNotReady)?;
        if let Err(error) = self.backend.mark_mutation_started() {
            let disarm = self.backend.disarm_recovery();
            return Err(ArmedApplyError::RecoveryNotReady(match disarm {
                Ok(()) => error,
                Err(disarm_error) => {
                    format!("{error}; failed to disarm unmutated recovery: {disarm_error}")
                }
            }));
        }
        Ok(())
    }

    fn apply_mode_armed(&mut self, size: DisplaySize) -> Result<ModeState, ArmedApplyError> {
        self.arm_for_mutation()?;
        self.backend
            .apply_mode(&self.target, size)
            .map_err(ArmedApplyError::MutationFailed)
    }

    fn isolate_armed(&mut self) -> Result<ModeState, ArmedApplyError> {
        self.arm_for_mutation()?;
        self.backend
            .isolate_topology(&self.target)
            .map_err(ArmedApplyError::MutationFailed)
    }

    fn refuse_strict(
        mut self,
        requested: DisplaySize,
        failure: String,
        mutation_attempted: bool,
    ) -> Result<Self, String> {
        if mutation_attempted {
            self.restore_keeping_armed().map_err(|restore_error| {
                format!(
                    "exact display apply failed ({failure}); rollback also failed: \
                     {restore_error}"
                )
            })?;
        }
        self.disarm_after_rollback()?;
        self.snapshot = None;
        self.record_restore_success();
        Err(format!(
            "this host cannot present the exact client display {requested} on {}: {failure}",
            self.target.device_name
        ))
    }

    #[cfg(test)]
    fn observe_restore(&mut self, restore_state: Arc<Mutex<RestoreJournal>>) {
        self.restore_state = Some(restore_state);
    }

    fn restore_keeping_armed(&mut self) -> Result<ModeState, String> {
        let result = {
            let Some(snapshot) = self.snapshot.as_ref() else {
                return Err("display restore requested without an armed snapshot".to_string());
            };
            restore_with_retry(&mut self.backend, &self.target, snapshot, self.original)
        };
        if let Err(error) = &result {
            self.record_restore_failure(error);
            self.snapshot = None;
        };
        result
    }

    pub(super) fn restore(&mut self) -> Result<(), String> {
        if self.snapshot.is_none() {
            return Ok(());
        }
        self.restore_keeping_armed()?;
        if let Err(error) = self.backend.disarm_recovery() {
            self.record_restore_failure(&error);
            self.snapshot = None;
            return Err(error);
        }
        self.snapshot = None;
        self.record_restore_success();
        Ok(())
    }

    fn disarm_after_rollback(&mut self) -> Result<(), String> {
        if let Err(error) = self.backend.disarm_recovery() {
            self.record_restore_failure(&error);
            self.snapshot = None;
            return Err(format!(
                "display returned to its original mode but recovery journal disarm failed: {error}"
            ));
        }
        Ok(())
    }

    fn record_restore_success(&self) {
        let Some(state) = self.restore_state.as_ref() else {
            return;
        };
        lock_restore_state(state).state = RestoreState::Clean;
    }

    fn record_restore_active(&self) {
        let Some(state) = self.restore_state.as_ref() else {
            return;
        };
        lock_restore_state(state).state = RestoreState::Active {
            device_name: self.report.device_name.clone(),
            original: self.report.original,
        };
    }

    fn record_restore_failure(&self, error: &str) {
        let Some(state) = self.restore_state.as_ref() else {
            return;
        };
        let mut journal = lock_restore_state(state);
        journal.state = RestoreState::Failed {
            device_name: self.report.device_name.clone(),
            original: self.report.original,
            error: error.to_string(),
        };
        journal.last_failure = Some(error.to_string());
    }
}

fn isolation_mismatch(state: ModeState, requested: DisplaySize) -> String {
    format!(
        "topology isolation settled at {} rect {:?} with {} active outputs instead of \
         {requested} as the only primary output",
        state.size, state.desktop_rect, state.active_outputs
    )
}

fn restore_mismatch(restored: ModeState, original: ModeState) -> String {
    format!(
        "restore settled at {} @ {}Hz output {} rect {:?} instead of original \
         {} @ {}Hz output {} rect {:?}",
        restored.size,
        restored.refresh_hz,
        restored.output_index,
        restored.desktop_rect,
        original.size,
        original.refresh_hz,
        original.output_index,
        original.desktop_rect
    )
}

impl<B: DisplayBackend> Drop for DisplayTransaction<B> {
    fn drop(&mut self) {
        if self.snapshot.is_none() {
            return;
        }
        match self.restore() {
            Ok(()) => tracing::info!(
                target: DISPLAY,
                device = %self.report.device_name,
                restored = %self.report.original,
                backend = self.report.restore_backend,
                "display restored by Drop cleanup"
            ),
            Err(error) => tracing::error!(
                target: DISPLAY,
                device = %self.report.device_name,
                original = %self.report.original,
                %error,
                "display Drop cleanup exhausted bounded restore retries"
            ),
        }
    }
}

fn restore_with_retry<B: DisplayBackend>(
    backend: &mut B,
    target: &DisplayTarget,
    snapshot: &B::Snapshot,
    original: ModeState,
) -> Result<ModeState, String> {
    let mut last_error = String::new();
    for attempt in 1..=RESTORE_ATTEMPTS {
        match backend.restore(target, snapshot) {
            Ok(mode) if mode == original => return Ok(mode),
            Ok(mode) => {
                last_error = format!(
                    "attempt {attempt}/{RESTORE_ATTEMPTS}: {}",
                    restore_mismatch(mode, original)
                );
                if attempt < RESTORE_ATTEMPTS {
                    std::thread::sleep(RESTORE_RETRY_DELAY);
                }
            }
            Err(error) => {
                last_error = format!("attempt {attempt}/{RESTORE_ATTEMPTS}: {error}");
                if attempt < RESTORE_ATTEMPTS {
                    std::thread::sleep(RESTORE_RETRY_DELAY);
                }
            }
        }
    }
    Err(last_error)
}

fn report(
    requested: DisplaySize,
    original: ModeState,
    applied: ModeState,
    exact: bool,
    changed: bool,
    backend: &'static str,
    restore_backend: &'static str,
    target: &DisplayTarget,
) -> DisplayReport {
    let retarget_capable = exact
        && backend.starts_with("nvidia-nvapi-")
        && restore_backend == "nvapi-purge-plus-set-display-config-exact";
    DisplayReport {
        requested,
        applied: applied.size,
        original: original.size,
        original_refresh_hz: original.refresh_hz,
        applied_refresh_hz: applied.refresh_hz,
        exact,
        changed,
        retarget_capable,
        backend,
        restore_backend,
        device_name: target.device_name.clone(),
        capture_output_index: applied.output_index,
        desktop_rect: applied.desktop_rect,
        effective_scale_reports: Vec::new(),
    }
}

#[cfg(test)]
fn choose_fallback(
    requested: DisplaySize,
    original: DisplaySize,
    supported: &[DisplaySize],
) -> Option<DisplaySize> {
    fallback_candidates(requested, supported)
        .into_iter()
        .next()
        .or_else(|| {
            DisplaySize::validate(original.width, original.height)
                .ok()
                .filter(|original| *original != requested)
        })
}

#[cfg(test)]
fn complete_topology_bytes_match(
    expected_paths: &[u8],
    expected_modes: &[u8],
    actual_paths: &[u8],
    actual_modes: &[u8],
) -> bool {
    expected_paths == actual_paths && expected_modes == actual_modes
}

fn require_stable_recovery_schema(version: u32, has_stable_topology: bool) -> Result<(), String> {
    if version < 4 || !has_stable_topology {
        Err(format!(
            "legacy display recovery journal v{version} lacks stable output identities; run \
             `arcen-pier migrate-display-journal --journal <PATH>` from the elevated exact local \
             console before restore"
        ))
    } else {
        Ok(())
    }
}

#[cfg(test)]
fn fallback_candidates(requested: DisplaySize, supported: &[DisplaySize]) -> Vec<DisplaySize> {
    fallback_candidates_matching(requested, supported, |_| true)
}

fn fallback_candidates_matching(
    requested: DisplaySize,
    supported: &[DisplaySize],
    accepts: impl Fn(DisplaySize) -> bool,
) -> Vec<DisplaySize> {
    let mut candidates: Vec<DisplaySize> = supported
        .iter()
        .copied()
        .filter(|size| {
            *size != requested
                && accepts(*size)
                && DisplaySize::validate(size.width, size.height).is_ok()
        })
        .collect();
    candidates.sort_by_key(|size| (size.width, size.height));
    candidates.dedup();

    let mut ordered = candidates;
    ordered.sort_by(|left, right| {
        ranked_fallback_score(requested, *left)
            .total_cmp(&ranked_fallback_score(requested, *right))
            .then_with(|| {
                (right.width as u64 * right.height as u64)
                    .cmp(&(left.width as u64 * left.height as u64))
            })
    });
    ordered.truncate(MAX_FALLBACK_CANDIDATES);
    ordered
}

/// How much worse a mode larger than the request is treated, on the
/// [`fallback_score`] scale.
///
/// A mode that fits inside the request can be presented pixel-for-pixel, so it
/// is preferred while it stays competitive. Excluding larger modes outright was
/// worse: on a fixed-mode host (QEMU stdvga, measured 2026-08-03) a 1792x1120
/// request scored 1920x1200 at 0.138 and 1280x800 at 0.673, then served the
/// 1280x800 — a desktop less than half the area — while the display was already
/// running 1920x1200.
///
/// The value is calibrated so both known cases stay right, and each is pinned
/// by a test below:
///
/// - it must be under 0.535, so 1920x1200 still beats 1280x800 above; and
/// - it must be over 0.284, so a 3600x2338 request keeps preferring the fitting
///   2560x1600 over the larger, wronger-shaped 3840x2160.
///
/// On the score's own scale 0.40 is roughly a 1.5x area difference: a larger
/// mode has to be clearly closer to the request, not merely closer.
const FALLBACK_EXCEEDS_REQUEST_PENALTY: f64 = 0.40;

fn exceeds_request(requested: DisplaySize, candidate: DisplaySize) -> bool {
    candidate.width > requested.width || candidate.height > requested.height
}

fn ranked_fallback_score(requested: DisplaySize, candidate: DisplaySize) -> f64 {
    let penalty = if exceeds_request(requested, candidate) {
        FALLBACK_EXCEEDS_REQUEST_PENALTY
    } else {
        0.0
    };
    fallback_score(requested, candidate) + penalty
}

fn fallback_score(requested: DisplaySize, candidate: DisplaySize) -> f64 {
    let requested_aspect = requested.width as f64 / requested.height as f64;
    let candidate_aspect = candidate.width as f64 / candidate.height as f64;
    let aspect_error = (candidate_aspect / requested_aspect).ln().abs();
    let area_ratio = (candidate.width as f64 * candidate.height as f64)
        / (requested.width as f64 * requested.height as f64);
    aspect_error * 4.0 + area_ratio.ln().abs()
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum RestoreState {
    Clean,
    Active {
        device_name: String,
        original: DisplaySize,
    },
    Failed {
        device_name: String,
        original: DisplaySize,
        error: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RestoreJournal {
    state: RestoreState,
    last_failure: Option<String>,
}

fn lock_restore_state(
    state: &Arc<Mutex<RestoreJournal>>,
) -> std::sync::MutexGuard<'_, RestoreJournal> {
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[derive(Clone)]
pub struct DisplayManager {
    session_slot: Arc<Semaphore>,
    restore_state: Arc<Mutex<RestoreJournal>>,
}

impl Default for DisplayManager {
    fn default() -> Self {
        Self {
            session_slot: Arc::new(Semaphore::new(1)),
            restore_state: Arc::new(Mutex::new(RestoreJournal {
                state: RestoreState::Clean,
                last_failure: None,
            })),
        }
    }
}

impl DisplayManager {
    pub fn acquire(
        &self,
        selector: OutputSelector,
        request: DisplayRequest,
        policy: DisplayPolicy,
        session_log_id: arcen_telemetry::CorrelationId,
    ) -> Result<DisplayLease, String> {
        self.acquire_with_deskside(selector, request, policy, session_log_id, None)
    }

    pub fn acquire_with_deskside(
        &self,
        selector: OutputSelector,
        request: DisplayRequest,
        policy: DisplayPolicy,
        session_log_id: arcen_telemetry::CorrelationId,
        deskside: Option<crate::recovery::DesksideRecoveryEntry>,
    ) -> Result<DisplayLease, String> {
        let dpi_awareness = crate::input::initialize_process_dpi_awareness();
        tracing::debug!(
            target: DISPLAY,
            dpi_awareness,
            "display transaction initialized process DPI awareness before DXGI enumeration"
        );
        recover_pending_journal(&crate::recovery::default_path())?;
        let permit = self
            .session_slot
            .clone()
            .try_acquire_owned()
            .map_err(|_| "shared Windows display is already in use".to_string())?;

        if let RestoreState::Failed {
            device_name,
            original,
            error,
        } = &lock_restore_state(&self.restore_state).state
        {
            return Err(format!(
                "previous display restore failed for {device_name} to {original}: {error}; \
                 refusing another mode transaction"
            ));
        }

        let requested = request.size;
        let requested_scale_percent = requested_scale_percent_from_request(request)?;
        let mut inner = DisplayTransaction::acquire_observed(
            NativeBackend::new(request, session_log_id, deskside),
            &selector,
            requested,
            policy,
            Some(Arc::clone(&self.restore_state)),
        )?;
        let report = inner.report().clone();
        if !report.changed {
            lock_restore_state(&self.restore_state).state = RestoreState::Clean;
        }
        #[cfg(windows)]
        if inner.target.vendor_id == 0x10de {
            if let Err(error) = windows_backend::apply_single_display_scale(
                &report.device_name,
                report.desktop_rect,
                requested_scale_percent,
            ) {
                let restore = inner.restore();
                return Err(match restore {
                    Ok(()) => format!("apply requested Windows UI scale: {error}"),
                    Err(restore) => format!(
                        "apply requested Windows UI scale: {error}; display rollback failed: \
                         {restore}"
                    ),
                });
            }
        }
        // Apply and verify the UI scale Windows actually resolved, not just
        // the mode.
        //
        // The multi-display path has done this since it shipped; the
        // single-display path applied the mode and synthesized EDID and never
        // looked at the result. A lab probe inside an active console measured
        // 250% effective DPI on a stream that asked for a point-sized 1:1
        // desktop, and nothing in the host log said so — the defect was
        // invisible here while being obvious on screen.
        //
        // NVIDIA GRID can ignore the EDID-derived recommendation entirely, so
        // the guarded Windows device-info path above applies the exact
        // Settings-equivalent step and fails closed if it cannot be verified.
        #[cfg(windows)]
        windows_backend::log_single_display_effective_scale(
            &report.device_name,
            report.desktop_rect,
            requested_scale_percent,
        );
        #[cfg(not(windows))]
        let _ = requested_scale_percent;
        tracing::info!(
            target: DISPLAY,
            requested = %report.requested,
            applied = %report.applied,
            original = %report.original,
            exact = report.exact,
            changed = report.changed,
            backend = report.backend,
            restore_backend = report.restore_backend,
            device = %report.device_name,
            policy = ?policy,
            requested_selector = ?selector,
            capture_output_index = report.capture_output_index,
            rect_left = report.desktop_rect.left,
            rect_top = report.desktop_rect.top,
            rect_width = report.desktop_rect.width,
            rect_height = report.desktop_rect.height,
            "authenticated display lease ready before capture"
        );
        Ok(DisplayLease {
            inner,
            #[cfg(windows)]
            headless: None,
            _permit: permit,
        })
    }

    pub fn acquire_multi(
        &self,
        plan: &crate::multi_monitor_topology::WindowsTopologyPlan,
        iddcx: Option<crate::config::WindowsIddCxConfig>,
        session_log_id: arcen_telemetry::CorrelationId,
    ) -> Result<MultiDisplayLease, String> {
        let dpi_awareness = crate::input::initialize_process_dpi_awareness();
        tracing::debug!(
            target: DISPLAY,
            dpi_awareness,
            "multi-display transaction initialized process DPI awareness"
        );
        recover_pending_journal(&crate::recovery::default_path())?;
        let permit = self
            .session_slot
            .clone()
            .try_acquire_owned()
            .map_err(|_| "shared Windows display is already in use".to_string())?;
        #[cfg(windows)]
        let inner = {
            let context = arcen_outputs::OutputContext::new(session_log_id);
            match iddcx {
                Some(config) => MultiDisplayTransaction::IddCx(
                    crate::output_provider::block_on(arcen_outputs::OutputTransaction::acquire(
                        windows_backend::IddCxOutputProvider::new(config)?,
                        plan,
                        &context,
                    ))
                    .map_err(|error| {
                        crate::output_provider::multi_display_provision_error(&error)
                    })?,
                ),
                None => MultiDisplayTransaction::Physical(
                    crate::output_provider::block_on(arcen_outputs::OutputTransaction::acquire(
                        windows_backend::PhysicalOutputProvider::new(None),
                        plan,
                        &context,
                    ))
                    .map_err(|error| {
                        crate::output_provider::multi_display_provision_error(&error)
                    })?,
                ),
            }
        };
        #[cfg(not(windows))]
        let inner = {
            let _ = (plan, iddcx, session_log_id);
            return Err("multi-display transactions are only available on Windows".to_string());
        };
        Ok(MultiDisplayLease {
            inner,
            _permit: permit,
        })
    }

    pub fn prepare_nvidia_headless_multi(
        &self,
        adapter_name: &str,
        contracts: Vec<crate::nvapi_headless::HeadlessDisplayContract>,
        session_log_id: arcen_telemetry::CorrelationId,
    ) -> Result<HeadlessPlanningLease, String> {
        let dpi_awareness = crate::input::initialize_process_dpi_awareness();
        tracing::debug!(
            target: DISPLAY,
            dpi_awareness,
            "NVIDIA headless planning initialized process DPI awareness"
        );
        recover_pending_journal(&crate::recovery::default_path())?;
        let permit = self
            .session_slot
            .clone()
            .try_acquire_owned()
            .map_err(|_| "shared Windows display is already in use".to_string())?;
        #[cfg(windows)]
        let preparation = windows_backend::provision_nvidia_headless_outputs(
            adapter_name,
            &contracts,
            &session_log_id,
        )?;
        #[cfg(not(windows))]
        let preparation = {
            let _ = (adapter_name, contracts, session_log_id);
            return Err("NVIDIA headless provisioning is only available on Windows".to_string());
        };
        Ok(HeadlessPlanningLease {
            preparation,
            adapter_name: adapter_name.to_string(),
            restore_state: Arc::clone(&self.restore_state),
            _permit: permit,
        })
    }
}

fn ensure_recovery_journal_clear(pending: bool, path: &std::path::Path) -> Result<(), String> {
    if pending {
        Err(format!(
            "display recovery journal {path:?} is pending; run \
             `arcen-pier restore-display` before starting another session"
        ))
    } else {
        Ok(())
    }
}

/// Clear a pending recovery journal by performing the restore it describes,
/// rather than refusing every future session until somebody runs a command on
/// the machine.
///
/// The journal is armed before the display is mutated and removed once it has
/// been put back. A session that ends without that happening -- a crash, or
/// simply the user signing out, which kills the agent and the watchdog together
/// because both live in that session -- leaves it behind.
///
/// Refusing on that is a trap. This is a remote access product: the operator
/// told to "run `arcen-pier restore-display`" generally cannot reach the machine
/// to run it, because the thing that would have let them reach it is what just
/// refused. And the journal is not an obstacle to recovery, it is the recipe for
/// it, so holding it while declining to act on it helps nobody.
///
/// This runs in the per-session agent, inside the signed-in user's session, so
/// it has the desktop the restore needs. If the restore fails the journal is
/// deliberately left in place and the original message is returned unchanged,
/// so a genuinely stuck host still stops rather than looping over a broken
/// display.
#[cfg(windows)]
fn recover_pending_journal(path: &std::path::Path) -> Result<(), String> {
    if !path.exists() {
        return Ok(());
    }
    tracing::warn!(
        target: DISPLAY,
        journal = %path.display(),
        "display recovery journal was left pending; restoring before this session"
    );
    match windows_backend::restore_from_path(path) {
        Ok(outcome) if !path.exists() => {
            tracing::info!(
                target: DISPLAY,
                journal = %path.display(),
                ?outcome,
                "restored the display from the pending recovery journal"
            );
            Ok(())
        }
        Ok(outcome) => {
            tracing::error!(
                target: DISPLAY,
                journal = %path.display(),
                ?outcome,
                "display restore reported success but did not clear the journal"
            );
            quarantine_unrestorable_journal(path, "restore_did_not_clear_journal")
        }
        Err(error) => {
            tracing::error!(
                target: DISPLAY,
                journal = %path.display(),
                %error,
                "could not restore the display from the pending recovery journal"
            );
            quarantine_unrestorable_journal(path, "restore_failed")
        }
    }
}

/// Move a journal that cannot be applied out of the way, so it stops blocking
/// every future session, and continue.
///
/// A journal describes how to put the display back. When it cannot be applied
/// -- measured on a vGPU host as `connected stable display id 0x82061080 has
/// ambiguous all-path bindings`, after a sign-out recreated the console session
/// under it -- the state it describes is one that can no longer be reached.
/// Refusing for ever then protects nothing and costs everything: the host
/// serves nobody, and the documented remedy is out of reach.
///
/// That remedy really is out of reach, which is why this does not simply tell
/// the operator to run it. `arcen-pier restore-display` needs an interactive
/// desktop; over SSH on the same host it fails with `QueryDisplayConfig
/// returned Win32 error 5`. On a machine whose only interactive route in is the
/// product that just refused, there is no way to run it at all.
///
/// The record is renamed rather than deleted. It is the only description of the
/// display the stranded session changed, so it stays on disk for a later manual
/// restore or a support bundle; what it stops doing is gating new sessions. If
/// even the rename fails the original refusal stands, because arming a fresh
/// journal over one still in place would overwrite exactly that record.
#[cfg(windows)]
fn quarantine_unrestorable_journal(
    path: &std::path::Path,
    reason: &'static str,
) -> Result<(), String> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    let aside = path.with_file_name(format!(
        "{}.unrestorable.{stamp}.json",
        path.file_stem()
            .and_then(std::ffi::OsStr::to_str)
            .unwrap_or("display-recovery")
    ));
    if let Err(error) = std::fs::rename(path, &aside) {
        tracing::error!(
            target: DISPLAY,
            journal = %path.display(),
            %error,
            "could not set the unrestorable display journal aside; refusing the session"
        );
        return ensure_recovery_journal_clear(true, path);
    }
    tracing::warn!(
        target: DISPLAY,
        journal = %path.display(),
        quarantined = %aside.display(),
        reason,
        "display journal could not be applied and was set aside; continuing without it"
    );
    Ok(())
}

#[cfg(not(windows))]
fn recover_pending_journal(_path: &std::path::Path) -> Result<(), String> {
    Ok(())
}

pub struct DisplayLease {
    inner: DisplayTransaction<NativeBackend>,
    #[cfg(windows)]
    headless: Option<windows_backend::HeadlessOutputPreparation>,
    _permit: OwnedSemaphorePermit,
}

pub struct HeadlessPlanningLease {
    #[cfg(windows)]
    preparation: Option<windows_backend::HeadlessOutputPreparation>,
    #[cfg(not(windows))]
    preparation: Option<()>,
    adapter_name: String,
    restore_state: Arc<Mutex<RestoreJournal>>,
    _permit: OwnedSemaphorePermit,
}

impl HeadlessPlanningLease {
    pub fn acquire_single(
        mut self,
        selector: OutputSelector,
        request: DisplayRequest,
        policy: DisplayPolicy,
        session_log_id: arcen_telemetry::CorrelationId,
        deskside: Option<crate::recovery::DesksideRecoveryEntry>,
    ) -> Result<DisplayLease, String> {
        #[cfg(windows)]
        let mut headless = None;
        let selector = if self.preparation.is_some() {
            OutputSelector::Adapter {
                name: self.adapter_name.clone(),
                // Reconciliation leaves exactly one requested head on this
                // adapter for the single-display path, so its live DXGI
                // ordinal is necessarily zero regardless of which stale head
                // the pre-provision configuration named.
                output_index: 0,
            }
        } else {
            selector
        };
        #[cfg(windows)]
        if let Some(preparation) = self.preparation.take() {
            // NVAPI assigns exactly the requested heads, but GRID can leave an
            // emptied connector as an active CCD path. Isolate the requested
            // target while the headless recovery journal is still armed, so
            // Windows Advanced Color and capture both see the same one-display
            // topology. The normal display transaction below owns in-session
            // mode changes; the lease retains this pre-provision journal and
            // restores it after the normal transaction at teardown.
            let mut isolator =
                NativeBackend::new(request, session_log_id.clone(), deskside.clone());
            let target = isolator.select_target(&selector)?;
            let isolated = isolator.isolate_topology(&target)?;
            if !isolated.is_isolated_primary_at(request.size) {
                return Err(format!(
                    "NVIDIA headless single-display topology did not isolate the requested \
                     display: mode={} rect={:?} active_outputs={}",
                    isolated.size, isolated.desktop_rect, isolated.active_outputs
                ));
            }
            headless = Some(preparation);
            tracing::info!(
                target: DISPLAY,
                device = target.device_name,
                requested = %request.size,
                "single requested NVIDIA head isolated with pre-session EDID rollback armed"
            );
        }
        #[cfg(not(windows))]
        {
            let _ = (&selector, request, policy, &session_log_id, &deskside);
            return Err("NVIDIA headless provisioning is only available on Windows".to_string());
        }
        let inner = DisplayTransaction::acquire_observed(
            NativeBackend::new(request, session_log_id, deskside),
            &selector,
            request.size,
            policy,
            Some(Arc::clone(&self.restore_state)),
        )?;
        Ok(DisplayLease {
            inner,
            #[cfg(windows)]
            headless,
            _permit: self._permit,
        })
    }

    pub fn acquire(
        mut self,
        plan: &crate::multi_monitor_topology::WindowsTopologyPlan,
        session_log_id: arcen_telemetry::CorrelationId,
    ) -> Result<MultiDisplayLease, String> {
        #[cfg(windows)]
        let inner = {
            let context = arcen_outputs::OutputContext::new(session_log_id);
            MultiDisplayTransaction::Physical(
                crate::output_provider::block_on(arcen_outputs::OutputTransaction::acquire(
                    windows_backend::PhysicalOutputProvider::new(self.preparation.take()),
                    plan,
                    &context,
                ))
                .map_err(|error| crate::output_provider::multi_display_provision_error(&error))?,
            )
        };
        #[cfg(not(windows))]
        let inner = {
            let _ = (plan, session_log_id);
            return Err("NVIDIA headless provisioning is only available on Windows".to_string());
        };
        Ok(MultiDisplayLease {
            inner,
            _permit: self._permit,
        })
    }
}

pub struct MultiDisplayLease {
    #[cfg(windows)]
    inner: MultiDisplayTransaction,
    #[cfg(not(windows))]
    inner: (),
    _permit: OwnedSemaphorePermit,
}

#[cfg(windows)]
enum MultiDisplayTransaction {
    Physical(arcen_outputs::OutputTransaction<windows_backend::PhysicalOutputProvider>),
    IddCx(arcen_outputs::OutputTransaction<windows_backend::IddCxOutputProvider>),
}

impl MultiDisplayLease {
    pub fn report(&self) -> &DisplayReport {
        #[cfg(windows)]
        {
            match &self.inner {
                MultiDisplayTransaction::Physical(inner) => inner.evidence().report(),
                MultiDisplayTransaction::IddCx(inner) => inner.evidence().report(),
            }
        }
        #[cfg(not(windows))]
        {
            unreachable!("multi-display lease cannot be constructed off Windows")
        }
    }

    pub fn restore(&mut self) -> Result<(), String> {
        #[cfg(windows)]
        {
            match &mut self.inner {
                MultiDisplayTransaction::Physical(inner) => {
                    crate::output_provider::block_on(inner.rollback())
                }
                MultiDisplayTransaction::IddCx(inner) => {
                    crate::output_provider::block_on(inner.rollback())
                }
            }
        }
        #[cfg(not(windows))]
        {
            unreachable!("multi-display lease cannot be constructed off Windows")
        }
    }

    pub fn commit(&mut self) -> Result<(), String> {
        #[cfg(windows)]
        {
            let result = match &mut self.inner {
                MultiDisplayTransaction::Physical(inner) => {
                    crate::output_provider::block_on(inner.commit())
                }
                MultiDisplayTransaction::IddCx(inner) => {
                    crate::output_provider::block_on(inner.commit())
                }
            };
            result.map_err(|error| crate::output_provider::multi_display_provision_error(&error))
        }
        #[cfg(not(windows))]
        {
            unreachable!("multi-display lease cannot be constructed off Windows")
        }
    }

    pub fn applied_plan(&self) -> Option<&crate::multi_monitor_topology::WindowsTopologyPlan> {
        #[cfg(windows)]
        {
            match &self.inner {
                MultiDisplayTransaction::Physical(inner) => Some(inner.evidence().applied_plan()),
                MultiDisplayTransaction::IddCx(inner) => Some(inner.evidence().applied_plan()),
            }
        }
        #[cfg(not(windows))]
        {
            None
        }
    }
}

impl Drop for MultiDisplayLease {
    fn drop(&mut self) {
        #[cfg(windows)]
        if match &self.inner {
            MultiDisplayTransaction::Physical(inner) => inner.is_armed(),
            MultiDisplayTransaction::IddCx(inner) => inner.is_armed(),
        } {
            let restore = if tokio::runtime::Handle::try_current().is_ok_and(|handle| {
                handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread
            }) {
                tokio::task::block_in_place(|| self.restore())
            } else {
                self.restore()
            };
            if let Err(error) = restore {
                tracing::error!(
                    target: DISPLAY,
                    %error,
                    "multi-display lease Drop cleanup failed"
                );
            }
        }
    }
}

impl DisplayLease {
    pub fn report(&self) -> &DisplayReport {
        self.inner.report()
    }

    pub fn restore(&mut self) -> Result<(), String> {
        let display_restore = self.inner.restore();
        #[cfg(windows)]
        let headless_restore = self
            .headless
            .as_mut()
            .map_or(Ok(()), windows_backend::HeadlessOutputPreparation::restore);
        #[cfg(not(windows))]
        let headless_restore: Result<(), String> = Ok(());
        match (display_restore, headless_restore) {
            (Ok(()), Ok(())) => {}
            (Err(error), Ok(())) | (Ok(()), Err(error)) => return Err(error),
            (Err(display_error), Err(headless_error)) => {
                return Err(format!(
                    "{display_error}; NVIDIA headless rollback failed: {headless_error}"
                ));
            }
        }
        tracing::info!(
            target: DISPLAY,
            device = %self.inner.report.device_name,
            restored = %self.inner.report.original,
            backend = self.inner.report.restore_backend,
            "display restored after capture and input shutdown"
        );
        Ok(())
    }

    pub fn retarget_exact(&mut self, requested: DisplaySize) -> Result<(), String> {
        self.inner.retarget_exact(requested)?;
        tracing::info!(
            target: DISPLAY,
            requested = %requested,
            applied = %self.inner.report.applied,
            device = %self.inner.report.device_name,
            "retargeted active display transaction"
        );
        Ok(())
    }
}

impl Drop for DisplayLease {
    fn drop(&mut self) {
        #[cfg(windows)]
        let headless_armed = self
            .headless
            .as_ref()
            .is_some_and(windows_backend::HeadlessOutputPreparation::is_armed);
        #[cfg(not(windows))]
        let headless_armed = false;
        if self.inner.snapshot.is_none() && !headless_armed {
            return;
        }
        let restore = if tokio::runtime::Handle::try_current().is_ok_and(|handle| {
            handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread
        }) {
            tokio::task::block_in_place(|| self.restore())
        } else {
            self.restore()
        };
        if let Err(error) = restore {
            tracing::error!(
                target: DISPLAY,
                device = %self.inner.report.device_name,
                original = %self.inner.report.original,
                %error,
                "display lease Drop cleanup exhausted bounded restore retries"
            );
        }
    }
}

#[cfg(windows)]
struct NativeBackend {
    request: DisplayRequest,
    dxgi: windows_backend::SessionDxgiEnumerator,
    nvapi: Option<crate::nvapi::Nvapi>,
    nvapi_snapshot: Option<crate::nvapi::ExactModeSnapshot>,
    nvapi_active: Option<crate::nvapi::ActiveExactMode>,
    nvapi_failed: bool,
    pending_nvapi: bool,
    pending_vmware: bool,
    vmware_active: bool,
    vmware_failed: bool,
    recovery_armed: bool,
    deskside: Option<crate::recovery::DesksideRecoveryEntry>,
    journal_path: std::path::PathBuf,
    session_log_id: arcen_telemetry::CorrelationId,
}

#[cfg(windows)]
impl NativeBackend {
    fn new(
        request: DisplayRequest,
        session_log_id: arcen_telemetry::CorrelationId,
        deskside: Option<crate::recovery::DesksideRecoveryEntry>,
    ) -> Self {
        Self {
            request,
            dxgi: windows_backend::SessionDxgiEnumerator::default(),
            nvapi: None,
            nvapi_snapshot: None,
            nvapi_active: None,
            nvapi_failed: false,
            pending_nvapi: false,
            pending_vmware: false,
            vmware_active: false,
            vmware_failed: false,
            recovery_armed: false,
            deskside,
            journal_path: crate::recovery::default_path(),
            session_log_id,
        }
    }

    fn desired_edid(&self, size: DisplaySize) -> Result<Vec<u8>, String> {
        let request = crate::edid::EdidRequest {
            width: size.width,
            height: size.height,
            refresh_hz: self.request.refresh_hz.max(1),
            width_mm: self.request.width_mm,
            height_mm: self.request.height_mm,
            scale: self.request.scale,
            product_id: self.request.product_id,
            serial: self.request.serial,
        };
        if self.request.hdr10 {
            Ok(crate::edid::generate_hdr10(request)?.to_vec())
        } else {
            Ok(crate::edid::generate(request)?.to_vec())
        }
    }
}

#[cfg(windows)]
fn nvapi_exact_available(
    vendor_id: u32,
    failed: bool,
    driver_present: bool,
    snapshot_present: bool,
) -> bool {
    vendor_id == 0x10de && !failed && driver_present && snapshot_present
}

#[cfg(windows)]
fn require_nvapi_exact_retarget(
    vendor_id: u32,
    failed: bool,
    driver_present: bool,
    snapshot_present: bool,
) -> Result<(), String> {
    if vendor_id != 0x10de
        || nvapi_exact_available(vendor_id, failed, driver_present, snapshot_present)
    {
        return Ok(());
    }
    Err(
        "NVIDIA media fallback retarget requires the owned NVAPI exact-resolution backend"
            .to_string(),
    )
}

#[cfg(windows)]
fn take_nvapi_active_after_stage<T>(
    active: &mut Option<T>,
    recovery_armed: bool,
    read_stage: impl FnOnce() -> Result<crate::nvapi::CleanupStage, String>,
) -> Result<Option<(T, crate::nvapi::CleanupStage)>, String> {
    if active.is_none() {
        return Ok(None);
    }
    let cleanup_stage = if recovery_armed {
        read_stage()?
    } else {
        crate::nvapi::CleanupStage::Pending
    };
    Ok(active.take().map(|active| (active, cleanup_stage)))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LegacySourceEvidence {
    width: u32,
    height: u32,
    x: i32,
    y: i32,
}

const EFFECTIVE_SCALE_MATCH_TOLERANCE_PERCENT: u16 = 5;

fn requested_scale_percent(scale: arcen_media::Scale120) -> u16 {
    let units = u32::from(scale.get());
    u16::try_from((units * 100 + 60) / 120).unwrap_or(u16::MAX)
}

/// The client's requested presentation scale as the plain ratio
/// [`crate::edid::EdidRequest::scale`] expects (`1.0` = 100%, `2.0` = 200%).
///
/// This is how requested scale is represented in synthesized EDID metadata. A
/// synthesized EDID has no "scale" field; its physical size implies DPI.
/// NVIDIA single-display sessions additionally apply and verify the matching
/// Windows display-config scale step because GRID can ignore the EDID-derived
/// recommendation.
/// [`crate::edid::physical_size_mm`] already encodes that relationship
/// (`width * 25.4 / (96 * scale)`, i.e. an implied `96 * scale` DPI) — it was
/// simply never given a real scale on any multi-monitor path, so every
/// synthesized display declared itself to be exactly 96 DPI and Windows
/// dutifully recommended 100%.
///
/// Returns `None` for a non-finite or non-positive ratio so callers keep the
/// historical `1.0` rather than emitting a nonsensical physical size.
pub(crate) fn edid_scale_ratio(scale: arcen_media::Scale120) -> Option<f32> {
    let ratio = f64::from(scale.get()) / 120.0;
    if !ratio.is_finite() || ratio <= 0.0 {
        return None;
    }
    Some(ratio as f32)
}

fn effective_scale_percent_from_dpi(dpi_x: u32, dpi_y: u32) -> Option<u16> {
    if dpi_x == 0 || dpi_y == 0 {
        return None;
    }
    let average_dpi = (u64::from(dpi_x) + u64::from(dpi_y)) / 2;
    u16::try_from((average_dpi * 100 + 48) / 96).ok()
}

fn display_scale_matches_requested(requested_percent: u16, effective_percent: u16) -> bool {
    requested_percent.abs_diff(effective_percent) <= EFFECTIVE_SCALE_MATCH_TOLERANCE_PERCENT
}

fn requested_scale_percent_from_request(request: DisplayRequest) -> Result<u16, String> {
    let ratio = if request.width_mm > 0.0 && request.height_mm > 0.0 {
        let horizontal = request.size.width as f32 * 25.4 / (96.0 * request.width_mm);
        let vertical = request.size.height as f32 * 25.4 / (96.0 * request.height_mm);
        if !horizontal.is_finite() || !vertical.is_finite() || horizontal <= 0.0 || vertical <= 0.0
        {
            return Err("client display physical size implies an invalid scale".to_string());
        }
        if (horizontal - vertical).abs() > 0.05 {
            return Err(format!(
                "client display physical size implies non-square scaling ({horizontal:.3}x vs \
                 {vertical:.3}x)"
            ));
        }
        (horizontal + vertical) / 2.0
    } else {
        request.scale
    };
    let percent = (f64::from(ratio) * 100.0).round();
    if !percent.is_finite() || !(1.0..=1000.0).contains(&percent) {
        return Err("client display scale percentage is outside 1..=1000".to_string());
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Ok(percent as u16)
}

#[cfg(windows)]
mod windows_backend {
    use super::{
        display_scale_matches_requested, effective_scale_percent_from_dpi, nvapi_exact_available,
        report, requested_scale_percent, require_nvapi_exact_retarget,
        take_nvapi_active_after_stage, DesktopRect, DisplayBackend, DisplayReport,
        DisplayScaleReport, DisplaySize, DisplayTarget, ModeState, NativeBackend, OutputSelector,
    };
    use crate::logging::DISPLAY;
    use crate::nvapi;
    use crate::nvapi::{AdapterLuid, NvapiDriver};
    use core::future::Future;
    use std::collections::BTreeSet;
    use std::os::windows::process::CommandExt;
    use std::time::{Duration, Instant};
    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::Devices::Display::{
        DisplayConfigGetDeviceInfo, DisplayConfigSetDeviceInfo, GetDisplayConfigBufferSizes,
        QueryDisplayConfig, SetDisplayConfig, DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
        DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME, DISPLAYCONFIG_DEVICE_INFO_HEADER,
        DISPLAYCONFIG_DEVICE_INFO_TYPE, DISPLAYCONFIG_MODE_INFO,
        DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE, DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_ROTATION,
        DISPLAYCONFIG_ROTATION_IDENTITY, DISPLAYCONFIG_ROTATION_ROTATE180,
        DISPLAYCONFIG_ROTATION_ROTATE270, DISPLAYCONFIG_ROTATION_ROTATE90,
        DISPLAYCONFIG_SOURCE_DEVICE_NAME, DISPLAYCONFIG_TARGET_DEVICE_NAME, QDC_ALL_PATHS,
        QDC_ONLY_ACTIVE_PATHS, QDC_VIRTUAL_MODE_AWARE, QUERY_DISPLAY_CONFIG_FLAGS,
        SDC_ALLOW_CHANGES, SDC_APPLY, SDC_USE_SUPPLIED_DISPLAY_CONFIG, SDC_VIRTUAL_MODE_AWARE,
        SET_DISPLAY_CONFIG_FLAGS,
    };
    use windows::Win32::Foundation::{
        CloseHandle, DuplicateHandle, BOOL, DUPLICATE_HANDLE_OPTIONS, ERROR_INSUFFICIENT_BUFFER,
        ERROR_SUCCESS, HANDLE, HWND, POINT, POINTL, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT,
        WIN32_ERROR,
    };
    use windows::Win32::Globalization::{CompareStringOrdinal, CSTR_EQUAL};
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter, IDXGIFactory1};
    use windows::Win32::Graphics::Gdi::{
        ChangeDisplaySettingsExW, EnumDisplaySettingsExW, MonitorFromPoint, CDS_FULLSCREEN,
        CDS_TEST, CDS_TYPE, DEVMODEW, DEVMODE_FIELD_FLAGS, DISP_CHANGE, DISP_CHANGE_SUCCESSFUL,
        DM_PELSHEIGHT, DM_PELSWIDTH, ENUM_CURRENT_SETTINGS, ENUM_DISPLAY_SETTINGS_FLAGS,
        ENUM_DISPLAY_SETTINGS_MODE, MONITOR_DEFAULTTONEAREST,
    };
    use windows::Win32::Security::{
        CheckTokenMembership, CreateWellKnownSid, WinBuiltinAdministratorsSid, PSID,
        SECURITY_ATTRIBUTES,
    };
    use windows::Win32::System::RemoteDesktop::{
        ProcessIdToSessionId, WTSClientProtocolType, WTSFreeMemory, WTSGetActiveConsoleSessionId,
        WTSQuerySessionInformationW, WTS_CURRENT_SERVER_HANDLE, WTS_CURRENT_SESSION,
    };
    use windows::Win32::System::Threading::{
        CreateEventW, CreateProcessW, DeleteProcThreadAttributeList, GetCurrentProcess,
        GetCurrentProcessId, InitializeProcThreadAttributeList, TerminateProcess,
        UpdateProcThreadAttribute, WaitForMultipleObjects, WaitForSingleObject,
        CREATE_BREAKAWAY_FROM_JOB, CREATE_NO_WINDOW, EXTENDED_STARTUPINFO_PRESENT,
        LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION,
        PROCESS_SYNCHRONIZE, PROC_THREAD_ATTRIBUTE_HANDLE_LIST, STARTUPINFOEXW,
    };
    use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};

    const MODE_SETTLE_TIMEOUT: Duration = Duration::from_secs(3);
    const VMWARE_MODE_SETTLE_TIMEOUT: Duration = Duration::from_secs(10);
    const MODE_SETTLE_POLL: Duration = Duration::from_millis(50);
    /// DISPLAYCONFIG_PATH_ACTIVE — the SDK macro is not exported by windows-rs.
    const PATH_ACTIVE_FLAG: u32 = 0x1;
    /// All virtual-mode-aware 16-bit mode indices (clone group + source mode
    /// idx, desktop image + target mode idx) set to their INVALID (0xffff)
    /// sentinels; also DISPLAYCONFIG_PATH_MODE_IDX_INVALID for the non-virtual
    /// u32 interpretation, so it is correct for both encodings.
    const PATH_MODE_INDICES_INVALID: u32 = u32::MAX;
    const TARGET_READY_TIMEOUT: Duration = Duration::from_secs(5);
    const TARGET_READY_POLL: Duration = Duration::from_millis(100);
    const VMWARE_RESOLUTION_TIMEOUT: Duration = Duration::from_secs(5);
    const VMWARE_VENDOR_ID: u32 = 0x15ad;
    const TOPOLOGY_QUERY_ATTEMPTS: usize = 4;
    const MAX_ENUMERATED_MODES: u32 = 4096;
    const WATCHDOG_READY_TIMEOUT_MS: u32 = 5_000;

    struct OwnedHandle(HANDLE);

    impl OwnedHandle {
        fn raw(&self) -> HANDLE {
            self.0
        }
    }

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            if !self.0.is_invalid() {
                // SAFETY: this wrapper uniquely owns a valid Windows handle.
                unsafe {
                    let _ = CloseHandle(self.0);
                }
            }
        }
    }

    struct AttributeList {
        raw: LPPROC_THREAD_ATTRIBUTE_LIST,
        _storage: Vec<usize>,
    }

    impl Drop for AttributeList {
        fn drop(&mut self) {
            // SAFETY: raw was initialized successfully and storage remains alive.
            unsafe {
                DeleteProcThreadAttributeList(self.raw);
            }
        }
    }

    pub(super) struct WindowsSnapshot {
        topology: TopologySnapshot,
        mode: DEVMODEW,
        original: ModeState,
        nvapi: Option<nvapi::ExactModeSnapshot>,
    }

    pub(super) struct TopologySnapshot {
        pub(super) paths: Vec<DISPLAYCONFIG_PATH_INFO>,
        pub(super) modes: Vec<DISPLAYCONFIG_MODE_INFO>,
    }

    pub(super) struct PhysicalOutputProvider {
        headless: Option<HeadlessOutputPreparation>,
    }

    impl PhysicalOutputProvider {
        pub(super) const fn new(headless: Option<HeadlessOutputPreparation>) -> Self {
            Self { headless }
        }
    }

    pub(super) struct HeadlessOutputPreparation {
        original: Option<TopologySnapshot>,
        original_stable: Option<crate::recovery::StableTopologySnapshot>,
        journal_path: std::path::PathBuf,
        entries: Vec<crate::nvapi_headless::HeadlessEdidRecovery>,
        armed: bool,
    }

    impl HeadlessOutputPreparation {
        pub(super) fn restore(&mut self) -> Result<(), String> {
            if !self.armed {
                return Ok(());
            }
            restore_from_path(&self.journal_path)?;
            self.armed = false;
            Ok(())
        }

        pub(super) const fn is_armed(&self) -> bool {
            self.armed
        }

        fn into_parts(
            mut self,
        ) -> (
            TopologySnapshot,
            crate::recovery::StableTopologySnapshot,
            std::path::PathBuf,
            Vec<crate::nvapi_headless::HeadlessEdidRecovery>,
        ) {
            self.armed = false;
            (
                self.original.take().expect("headless original topology"),
                self.original_stable
                    .take()
                    .expect("headless stable topology"),
                std::mem::take(&mut self.journal_path),
                std::mem::take(&mut self.entries),
            )
        }
    }

    impl Drop for HeadlessOutputPreparation {
        fn drop(&mut self) {
            if !self.armed {
                return;
            }
            if let Err(error) = self.restore() {
                tracing::error!(
                    target: DISPLAY,
                    %error,
                    journal = %self.journal_path.display(),
                    "NVIDIA headless planning rollback failed; recovery journal remains"
                );
            }
        }
    }

    pub(super) struct PreparedPhysicalOutput {
        plan: crate::multi_monitor_topology::WindowsTopologyPlan,
        session_log_id: arcen_telemetry::CorrelationId,
    }

    pub(super) struct PhysicalOutputBinding {
        evidence: crate::output_provider::WindowsOutputEvidence,
        original: TopologySnapshot,
        working: TopologySnapshot,
        original_stable: crate::recovery::StableTopologySnapshot,
        journal_path: std::path::PathBuf,
        recovery_armed: bool,
        nvapi_driver: Option<nvapi::Nvapi>,
        nvapi_modes: Vec<(nvapi::ExactModeSnapshot, nvapi::ActiveExactMode)>,
        headless_entries: Vec<crate::nvapi_headless::HeadlessEdidRecovery>,
        applied_dpi_scales: Vec<AppliedDpiScale>,
    }

    #[derive(Clone, Copy)]
    struct DisplaySourceKey {
        adapter_id: windows::Win32::Foundation::LUID,
        source_id: u32,
    }

    struct AppliedDpiScale {
        source: DisplaySourceKey,
        previous_relative: i32,
        previous_percent: u16,
        rect: DesktopRect,
        device_name: String,
    }

    fn arm_physical_recovery(
        session_log_id: &arcen_telemetry::CorrelationId,
        headless_entries: Vec<crate::nvapi_headless::HeadlessEdidRecovery>,
    ) -> Result<
        (
            TopologySnapshot,
            crate::recovery::StableTopologySnapshot,
            std::path::PathBuf,
        ),
        String,
    > {
        let journal_path = crate::recovery::default_path();
        if journal_path.exists() {
            return Err(format!(
                "refusing display mutation while recovery journal {journal_path:?} exists"
            ));
        }
        let original = query_active_topology()?;
        let original_stable = capture_stable_topology(&original)?;
        let selected_path_index = original_stable
            .paths
            .iter()
            .position(|identity| {
                matches!(
                    identity.binding,
                    crate::recovery::StableOutputBackend::WindowsNative
                )
            })
            .or_else(|| {
                original.paths.iter().position(|path| {
                    source_gdi_name(path)
                        .and_then(|device| current_devmode(&device))
                        .is_ok_and(|mode| {
                            // SAFETY: current_devmode returns the active display
                            // variant, whose union contains dmPosition.
                            let position = unsafe { mode.Anonymous1.Anonymous2.dmPosition };
                            position.x == 0 && position.y == 0
                        })
                })
            })
            .ok_or_else(|| {
                "multi-display recovery requires one active safe output in the original topology"
                    .to_string()
            })?;
        let selected_path = original
            .paths
            .get(selected_path_index)
            .ok_or_else(|| "safe recovery path index is outside the topology".to_string())?;
        let selected_device = source_gdi_name(selected_path)?;
        let selected_mode = current_devmode(&selected_device)?;
        let journal = crate::recovery::DisplayRecoveryJournal::new(
            selected_device,
            selected_mode.dmPelsWidth,
            selected_mode.dmPelsHeight,
            selected_mode.dmDisplayFrequency.max(1),
            as_bytes(&original.paths),
            as_bytes(&original.modes),
            value_bytes(&selected_mode),
            None,
        )
        .with_stable_topology(original_stable.clone())
        .with_selected_path_index(selected_path_index)
        .with_headless_nvapi_edids(headless_entries);
        crate::recovery::write_atomic(&journal_path, &journal)?;
        if let Err(error) = spawn_recovery_watchdog(
            &journal_path,
            session_log_id,
            crate::recovery::WatchdogResource::Display,
        ) {
            let remove = crate::recovery::remove(&journal_path);
            return Err(match remove {
                Ok(()) => error,
                Err(remove_error) => format!("{error}; {remove_error}"),
            });
        }
        crate::recovery::mark_mutation_started(&journal_path)?;
        Ok((original, original_stable, journal_path))
    }

    pub(super) fn provision_nvidia_headless_outputs(
        adapter_name: &str,
        contracts: &[crate::nvapi_headless::HeadlessDisplayContract],
        session_log_id: &arcen_telemetry::CorrelationId,
    ) -> Result<Option<HeadlessOutputPreparation>, String> {
        let prepared = crate::nvapi_headless::prepare_provisioning(adapter_name, contracts)?;
        if prepared.is_empty() {
            return Ok(None);
        }
        let entries = prepared.recovery_entries();
        let (original, original_stable, journal_path) =
            arm_physical_recovery(session_log_id, entries.clone())?;
        if let Err(error) = crate::nvapi_headless::apply_provisioning(&prepared) {
            let rollback = restore_from_path(&journal_path);
            return Err(match rollback {
                Ok(_) => format!("provision NVIDIA headless outputs: {error}"),
                Err(rollback_error) => format!(
                    "provision NVIDIA headless outputs: {error}; rollback failed: {rollback_error}"
                ),
            });
        }
        tracing::info!(
            target: DISPLAY,
            adapter = adapter_name,
            requested_outputs = contracts.len(),
            changed_edids = entries.len(),
            "NVIDIA headless outputs provisioned before topology planning"
        );
        Ok(Some(HeadlessOutputPreparation {
            original: Some(original),
            original_stable: Some(original_stable),
            journal_path,
            entries,
            armed: true,
        }))
    }

    impl arcen_outputs::OutputProvider for PhysicalOutputProvider {
        type Plan = crate::multi_monitor_topology::WindowsTopologyPlan;
        type Prepared = PreparedPhysicalOutput;
        type Binding = PhysicalOutputBinding;
        type Evidence = crate::output_provider::WindowsOutputEvidence;
        type Error = String;

        fn capabilities(&self) -> arcen_outputs::OutputCapabilities {
            crate::output_provider::PHYSICAL_OUTPUT_CAPABILITIES
        }

        fn demand(&self, plan: &Self::Plan) -> arcen_outputs::OutputDemand {
            crate::output_provider::windows_output_demand(plan)
        }

        fn preflight(
            &mut self,
            plan: &Self::Plan,
            context: &arcen_outputs::OutputContext,
        ) -> Result<Self::Prepared, Self::Error> {
            let journal_path = crate::recovery::default_path();
            if self.headless.is_none() && journal_path.exists() {
                return Err(format!(
                    "refusing multi-display mutation while recovery journal {journal_path:?} exists"
                ));
            }
            if let Some(headless) = self.headless.as_ref() {
                if headless.journal_path != journal_path || !journal_path.exists() {
                    return Err(
                        "NVIDIA headless planning lease lost its recovery journal".to_string()
                    );
                }
            }
            let topology = query_active_topology()?;
            for monitor in &plan.monitors {
                if !topology.paths.iter().any(|path| {
                    path.targetInfo.adapterId.LowPart == monitor.adapter_luid.low_part
                        && path.targetInfo.adapterId.HighPart == monitor.adapter_luid.high_part
                        && path.targetInfo.id == monitor.target_id
                }) {
                    return Err(format!(
                        "dry-run could not bind planned physical output {}",
                        monitor.device_name
                    ));
                }
            }
            Ok(PreparedPhysicalOutput {
                plan: plan.clone(),
                session_log_id: context.session_log_id().clone(),
            })
        }

        fn bind(
            &mut self,
            prepared: Self::Prepared,
        ) -> impl Future<Output = Result<Self::Binding, arcen_outputs::BindFailure<Self::Error>>> + Send
        {
            core::future::ready(self.bind_blocking(prepared))
        }

        fn verify(
            &mut self,
            binding: &mut Self::Binding,
        ) -> impl Future<Output = Result<(), Self::Error>> + Send {
            core::future::ready(Self::verify_blocking(binding))
        }

        fn commit(
            &mut self,
            binding: &mut Self::Binding,
        ) -> impl Future<Output = Result<(), Self::Error>> + Send {
            core::future::ready(binding.commit())
        }

        fn rollback(
            &mut self,
            binding: &mut Self::Binding,
        ) -> impl Future<Output = Result<(), Self::Error>> + Send {
            core::future::ready(binding.restore())
        }

        fn evidence<'a>(&'a self, binding: &'a Self::Binding) -> &'a Self::Evidence {
            &binding.evidence
        }

        fn is_armed(&self, binding: &Self::Binding) -> bool {
            binding.recovery_armed
        }
    }

    impl PhysicalOutputProvider {
        /// The whole of `bind`, kept synchronous because every CCD, NVAPI, and
        /// recovery-journal call in it blocks.
        ///
        /// The journal is written and the watchdog armed *before* the first
        /// operating-system-visible mutation, which is what makes dropping the
        /// returned future safe: an interrupted bind still leaves the
        /// out-of-process journal that the next session's
        /// `recover_pending_journal` replays.
        fn bind_blocking(
            &mut self,
            prepared: PreparedPhysicalOutput,
        ) -> Result<PhysicalOutputBinding, arcen_outputs::BindFailure<String>> {
            let plan = prepared.plan;
            let session_log_id = &prepared.session_log_id;
            let working = query_active_topology().map_err(arcen_outputs::BindFailure::new)?;
            let (original, original_stable, journal_path, headless_entries) =
                if let Some(headless) = self.headless.take() {
                    headless.into_parts()
                } else {
                    let (original, original_stable, journal_path) =
                        arm_physical_recovery(session_log_id, Vec::new())
                            .map_err(arcen_outputs::BindFailure::new)?;
                    (original, original_stable, journal_path, Vec::new())
                };

            let report =
                empty_multi_display_report(&plan).map_err(arcen_outputs::BindFailure::new)?;
            let mut binding = PhysicalOutputBinding {
                evidence: crate::output_provider::WindowsOutputEvidence::new(report, plan.clone()),
                original,
                working,
                original_stable,
                journal_path,
                recovery_armed: true,
                nvapi_driver: None,
                nvapi_modes: Vec::new(),
                headless_entries,
                applied_dpi_scales: Vec::new(),
            };
            let prepare_modes = if binding.headless_entries.is_empty() {
                binding.prepare_exact_modes(&plan)
            } else {
                // Headless provisioning already wrote the final per-monitor
                // EDIDs before topology planning. Re-running apply_exact here
                // used to replace them with an SDR EDID and custom timing,
                // which dropped Windows Advanced Color during the session.
                Ok(())
            };
            if let Err(error) = prepare_modes.and_then(|()| {
                binding.apply_headless_nvapi_topology(&plan)?;
                binding.apply(&plan)
            }) {
                return Err(arcen_outputs::BindFailure {
                    source: error,
                    rollback: binding.restore().err(),
                });
            }
            Ok(binding)
        }

        fn verify_blocking(binding: &mut PhysicalOutputBinding) -> Result<(), String> {
            wait_for_multi_display_plan(binding.plan(), MODE_SETTLE_TIMEOUT)?;
            binding.apply_requested_dpi_scales()?;
            let effective_scale_reports = effective_scale_reports(binding.plan());
            let report = multi_display_report(binding.plan(), effective_scale_reports)?;
            binding.evidence.set_report(report);
            let plan = binding.plan();
            log_effective_scale_reports(&binding.evidence.report().effective_scale_reports);
            tracing::info!(
                target: DISPLAY,
                monitors = plan.monitors.len(),
                desktop_x = plan.desktop_x,
                desktop_y = plan.desktop_y,
                desktop_width = plan.desktop_width,
                desktop_height = plan.desktop_height,
                "exact physical output-provider topology bound and verified"
            );
            Ok(())
        }
    }

    pub(super) struct IddCxOutputProvider {
        config: crate::config::WindowsIddCxConfig,
        control: std::sync::Arc<crate::iddcx::NativeControl>,
    }

    pub(super) struct PreparedIddCxOutput {
        plan: crate::multi_monitor_topology::WindowsTopologyPlan,
        request: arcen_iddcx_provider::abi::ApplyRequest,
    }

    pub(super) struct IddCxOutputBinding {
        evidence: crate::output_provider::WindowsOutputEvidence,
        generation: u32,
        monitor_count: usize,
        render_adapter: arcen_iddcx_provider::abi::AdapterLuid,
        recovery_armed: bool,
    }

    impl IddCxOutputProvider {
        pub(super) fn new(config: crate::config::WindowsIddCxConfig) -> Result<Self, String> {
            if !config.enabled {
                return Err("IddCx output provider was selected while disabled".to_string());
            }
            Ok(Self {
                control: crate::iddcx::inherited_control(true)?,
                config,
            })
        }

        fn wait_for_applied_plan(
            &self,
            plan: &crate::multi_monitor_topology::WindowsTopologyPlan,
        ) -> Result<crate::multi_monitor_topology::WindowsTopologyPlan, String> {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut last_error = None;
            while Instant::now() < deadline {
                match crate::iddcx::rebind_applied_plan(&self.config, plan) {
                    Ok(plan) => return Ok(plan),
                    Err(error) => last_error = Some(error),
                }
                std::thread::sleep(TARGET_READY_POLL);
            }
            Err(last_error.unwrap_or_else(|| {
                "IddCx monitors did not enumerate before the settle deadline".to_string()
            }))
        }

        fn wait_for_swapchains(&self, binding: &IddCxOutputBinding) -> Result<(), String> {
            let deadline = Instant::now() + Duration::from_secs(10);
            let required = arcen_iddcx_provider::abi::BINDING_SWAPCHAIN_READY
                | arcen_iddcx_provider::abi::BINDING_RENDER_ADAPTER_MATCHED;
            let mut last = "no status response".to_string();
            while Instant::now() < deadline {
                let status = self.control.status()?;
                if self
                    .validate_bindings(binding, &status, required, true)
                    .is_ok()
                {
                    return Ok(());
                }
                last = format!(
                    "generation={} monitors={} bindings={:?}",
                    status.generation,
                    status.monitor_count,
                    &status.bindings[..binding.monitor_count.min(status.bindings.len())]
                );
                std::thread::sleep(TARGET_READY_POLL);
            }
            Err(format!(
                "IddCx swapchain/render-affinity verification timed out ({last})"
            ))
        }

        fn validate_bindings(
            &self,
            binding: &IddCxOutputBinding,
            status: &arcen_iddcx_provider::abi::TopologyResponse,
            required_flags: u32,
            require_render_adapter: bool,
        ) -> Result<(), String> {
            if status.generation != binding.generation
                || status.monitor_count as usize != binding.monitor_count
            {
                return Err("IddCx status generation or monitor count does not match".to_string());
            }
            for (index, monitor) in binding.evidence.applied_plan().monitors.iter().enumerate() {
                let driver = &status.bindings[index];
                if driver.connector_index != index as u32
                    || driver.state != arcen_iddcx_provider::abi::BINDING_PRESENT
                    || driver.flags & required_flags != required_flags
                    || driver.os_adapter.low_part != monitor.adapter_luid.low_part
                    || driver.os_adapter.high_part != monitor.adapter_luid.high_part
                    || driver.os_target_id != monitor.target_id
                {
                    return Err(format!(
                        "IddCx connector {index} driver/Windows binding does not match the enumerated output"
                    ));
                }
                if require_render_adapter && driver.actual_render_adapter != binding.render_adapter
                {
                    return Err(format!(
                        "IddCx connector {index} rendered on an unexpected adapter"
                    ));
                }
            }
            Ok(())
        }

        fn remove_binding(&self, binding: &mut IddCxOutputBinding) -> Result<(), String> {
            if !binding.recovery_armed {
                return Ok(());
            }
            let response = self.control.remove(binding.generation)?;
            if response.monitor_count != 0 {
                return Err(format!(
                    "IddCx remove left {} active monitors",
                    response.monitor_count
                ));
            }
            binding.recovery_armed = false;
            Ok(())
        }
    }

    impl arcen_outputs::OutputProvider for IddCxOutputProvider {
        type Plan = crate::multi_monitor_topology::WindowsTopologyPlan;
        type Prepared = PreparedIddCxOutput;
        type Binding = IddCxOutputBinding;
        type Evidence = crate::output_provider::WindowsOutputEvidence;
        type Error = String;

        fn capabilities(&self) -> arcen_outputs::OutputCapabilities {
            crate::output_provider::IDDCX_OUTPUT_CAPABILITIES
        }

        fn demand(&self, plan: &Self::Plan) -> arcen_outputs::OutputDemand {
            crate::output_provider::windows_output_demand(plan)
        }

        fn preflight(
            &mut self,
            plan: &Self::Plan,
            _context: &arcen_outputs::OutputContext,
        ) -> Result<Self::Prepared, Self::Error> {
            Ok(PreparedIddCxOutput {
                request: crate::iddcx::topology_request(plan)?,
                plan: plan.clone(),
            })
        }

        fn bind(
            &mut self,
            prepared: Self::Prepared,
        ) -> impl Future<Output = Result<Self::Binding, arcen_outputs::BindFailure<Self::Error>>> + Send
        {
            core::future::ready(self.bind_blocking(prepared))
        }

        fn verify(
            &mut self,
            binding: &mut Self::Binding,
        ) -> impl Future<Output = Result<(), Self::Error>> + Send {
            core::future::ready(self.verify_blocking(binding))
        }

        fn commit(
            &mut self,
            binding: &mut Self::Binding,
        ) -> impl Future<Output = Result<(), Self::Error>> + Send {
            core::future::ready(self.commit_blocking(binding))
        }

        fn rollback(
            &mut self,
            binding: &mut Self::Binding,
        ) -> impl Future<Output = Result<(), Self::Error>> + Send {
            core::future::ready(self.remove_binding(binding))
        }

        fn evidence<'a>(&'a self, binding: &'a Self::Binding) -> &'a Self::Evidence {
            &binding.evidence
        }

        fn is_armed(&self, binding: &Self::Binding) -> bool {
            binding.recovery_armed
        }
    }

    impl IddCxOutputProvider {
        fn bind_blocking(
            &mut self,
            prepared: PreparedIddCxOutput,
        ) -> Result<IddCxOutputBinding, arcen_outputs::BindFailure<String>> {
            let generation = prepared.request.generation;
            let monitor_count = prepared.plan.monitors.len();
            let render_adapter = prepared.request.render_adapter;
            let report = empty_multi_display_report(&prepared.plan)
                .map_err(arcen_outputs::BindFailure::new)?;
            self.control
                .apply(&prepared.request)
                .map_err(arcen_outputs::BindFailure::new)?;
            let mut binding = IddCxOutputBinding {
                evidence: crate::output_provider::WindowsOutputEvidence::new(report, prepared.plan),
                generation,
                monitor_count,
                render_adapter,
                recovery_armed: true,
            };
            match self.wait_for_applied_plan(binding.evidence.applied_plan()) {
                Ok(plan) => binding.evidence.set_applied_plan(plan),
                Err(error) => {
                    // The generation is already applied, so removing it is the
                    // provider's own undo and the driver must learn whether it
                    // succeeded.
                    return Err(arcen_outputs::BindFailure {
                        source: error,
                        rollback: self.remove_binding(&mut binding).err(),
                    });
                }
            }
            Ok(binding)
        }

        fn verify_blocking(&self, binding: &mut IddCxOutputBinding) -> Result<(), String> {
            wait_for_multi_display_plan(binding.evidence.applied_plan(), Duration::from_secs(10))?;
            let status = self.control.status()?;
            self.validate_bindings(binding, &status, 0, false)?;
            let effective_scale_reports = effective_scale_reports(binding.evidence.applied_plan());
            let report =
                multi_display_report(binding.evidence.applied_plan(), effective_scale_reports)?;
            binding.evidence.set_report(report);
            log_effective_scale_reports(&binding.evidence.report().effective_scale_reports);
            tracing::info!(
                target: DISPLAY,
                generation = binding.generation,
                monitors = binding.monitor_count,
                "exact IddCx output-provider topology bound and verified"
            );
            Ok(())
        }

        fn commit_blocking(&self, binding: &IddCxOutputBinding) -> Result<(), String> {
            self.wait_for_swapchains(binding)?;
            tracing::info!(
                target: DISPLAY,
                generation = binding.generation,
                monitors = binding.monitor_count,
                "IddCx swapchains and exact render-adapter affinity verified"
            );
            Ok(())
        }
    }

    impl PhysicalOutputBinding {
        fn plan(&self) -> &crate::multi_monitor_topology::WindowsTopologyPlan {
            self.evidence.applied_plan()
        }

        fn prepare_exact_modes(
            &mut self,
            plan: &crate::multi_monitor_topology::WindowsTopologyPlan,
        ) -> Result<(), String> {
            // Only a plan that placed some output at a mode it does not
            // enumerate needs a synthesized timing, and synthesizing one is
            // an NVIDIA-only capability. A fixed-mode-only plan is applied by
            // the vendor-neutral CCD `apply` below at modes the outputs
            // already advertise, so requiring NVAPI here would fail every
            // host without an NVIDIA driver for no gain.
            if !plan.requires_custom_timing {
                tracing::debug!(
                    target: DISPLAY,
                    monitors = plan.monitors.len(),
                    "multi-display plan uses enumerated modes only; skipping NVAPI exact timings"
                );
                return Ok(());
            }
            let mut driver =
                nvapi::Nvapi::load().map_err(|error| format!("load NVAPI: {error}"))?;
            // Bind every boot-local Windows target to its stable NVAPI display
            // id before the first topology mutation. Activating an earlier
            // head can renumber later \\.\DISPLAY names, so resolving and
            // mutating one monitor at a time can accidentally bind two planned
            // monitors to the same NVIDIA head.
            let snapshots = plan
                .monitors
                .iter()
                .map(|monitor| {
                    nvapi::snapshot(
                        &mut driver,
                        &monitor.device_name,
                        monitor.adapter_luid,
                        monitor.mode_width,
                        monitor.mode_height,
                        monitor.refresh_hz,
                    )
                    .map_err(|error| {
                        format!("snapshot exact mode for {}: {error}", monitor.device_name)
                    })
                })
                .collect::<Result<Vec<_>, String>>()?;
            for (monitor, snapshot) in plan.monitors.iter().zip(snapshots) {
                let snapshot = match nvapi::retarget_snapshot(
                    &mut driver,
                    &snapshot,
                    monitor.mode_width,
                    monitor.mode_height,
                    monitor.refresh_hz,
                ) {
                    Ok(snapshot) => snapshot,
                    Err(error) => {
                        self.nvapi_driver = Some(driver);
                        let cleanup = self.restore_nvapi_modes();
                        return Err(match cleanup {
                            Ok(()) => {
                                format!("refresh exact mode for {}: {error}", monitor.device_name)
                            }
                            Err(cleanup_error) => format!(
                                "refresh exact mode for {}: {error}; cleanup failed: {cleanup_error}",
                                monitor.device_name
                            ),
                        });
                    }
                };
                tracing::debug!(
                    target: DISPLAY,
                    device = monitor.device_name,
                    display_id = format_args!("{:#010x}", snapshot.mapping.display_id),
                    output_id = format_args!("{:#010x}", snapshot.mapping.output_id),
                    "bound planned Windows output to stable NVAPI head before mutation"
                );
                let edid = crate::edid::generate(crate::edid::EdidRequest {
                    width: monitor.mode_width,
                    height: monitor.mode_height,
                    refresh_hz: monitor.refresh_hz,
                    width_mm: 0.0,
                    height_mm: 0.0,
                    // Carry the client's requested per-monitor scale. Passing
                    // `1.0` here declared every synthesized display to be
                    // exactly 96 DPI, so Windows recommended 100% no matter
                    // what the client asked for -- measured on pier-windows.example.internal as
                    // 200% requested and 100% applied.
                    scale: super::edid_scale_ratio(monitor.scale).unwrap_or(1.0),
                    product_id: u16::try_from(monitor.target_id).unwrap_or(0),
                    serial: monitor.target_id,
                })
                .map_err(|error| {
                    format!("generate exact EDID for {}: {error}", monitor.device_name)
                })?;
                match nvapi::apply_exact(
                    &mut driver,
                    &snapshot,
                    &edid,
                    monitor.mode_width,
                    monitor.mode_height,
                    monitor.refresh_hz,
                    |_| Ok(()),
                ) {
                    Ok(active) => self.nvapi_modes.push((snapshot, active)),
                    Err(error) => {
                        if let Some(active) = error.active {
                            self.nvapi_modes.push((snapshot, active));
                        }
                        self.nvapi_driver = Some(driver);
                        let cleanup = self.restore_nvapi_modes();
                        return Err(match cleanup {
                            Ok(()) => format!(
                                "apply exact mode for {}: {}",
                                monitor.device_name, error.message
                            ),
                            Err(cleanup_error) => format!(
                                "apply exact mode for {}: {}; cleanup failed: {cleanup_error}",
                                monitor.device_name, error.message
                            ),
                        });
                    }
                }
            }
            self.nvapi_driver = Some(driver);
            Ok(())
        }

        fn apply_headless_nvapi_topology(
            &mut self,
            plan: &crate::multi_monitor_topology::WindowsTopologyPlan,
        ) -> Result<bool, String> {
            if self.headless_entries.is_empty() {
                return Ok(false);
            }
            if plan.monitors.len()
                > usize::from(crate::multi_monitor_gate::MAX_NVIDIA_HEADLESS_MONITORS)
            {
                return Err(
                    "three-or-more NVIDIA headless displays are disabled: hardware validation \
                     reproduced an uninterruptible Windows SetDisplayConfig call after NVAPI \
                     activation; configure max_monitors=2 until DISPLAY-381 is resolved"
                        .to_string(),
                );
            }
            let display_ids = if self.nvapi_modes.is_empty() {
                let mut driver =
                    nvapi::Nvapi::load().map_err(|error| format!("load NVAPI: {error}"))?;
                let display_ids = plan
                    .monitors
                    .iter()
                    .map(|monitor| {
                        driver
                            .map_display(&monitor.device_name, monitor.adapter_luid)
                            .map(|mapping| mapping.display_id)
                    })
                    .collect::<Result<Vec<_>, String>>()?;
                self.nvapi_driver = Some(driver);
                display_ids
            } else if self.nvapi_modes.len() == plan.monitors.len() {
                self.nvapi_modes
                    .iter()
                    .map(|(snapshot, _)| snapshot.mapping.display_id)
                    .collect::<Vec<_>>()
            } else {
                return Err(format!(
                    "NVAPI prepared {} exact outputs for a {}-monitor headless topology",
                    self.nvapi_modes.len(),
                    plan.monitors.len()
                ));
            };
            let driver = self
                .nvapi_driver
                .as_mut()
                .ok_or_else(|| "headless NVIDIA topology has no NVAPI driver".to_string())?;
            driver
                .activate_extended_displays(&display_ids)
                .map_err(|error| format!("activate NVIDIA extended topology: {error}"))?;
            let deadline = Instant::now() + MODE_SETTLE_TIMEOUT;
            loop {
                let error = match query_active_topology() {
                    Ok(topology)
                        if plan.monitors.iter().all(|monitor| {
                            topology.paths.iter().any(|path| {
                                monitor.adapter_luid.low_part == path.targetInfo.adapterId.LowPart
                                    && monitor.adapter_luid.high_part
                                        == path.targetInfo.adapterId.HighPart
                                    && monitor.target_id == path.targetInfo.id
                            })
                        }) =>
                    {
                        self.working = topology;
                        break;
                    }
                    Ok(_) => "Windows has not activated every planned NVIDIA path".to_string(),
                    Err(error) => error,
                };
                if Instant::now() >= deadline {
                    return Err(format!(
                        "NVIDIA extended topology did not expose every requested Windows path: {}",
                        error
                    ));
                }
                std::thread::sleep(MODE_SETTLE_POLL);
            }
            tracing::info!(
                target: DISPLAY,
                monitors = plan.monitors.len(),
                desktop_x = plan.desktop_x,
                desktop_y = plan.desktop_y,
                desktop_width = plan.desktop_width,
                desktop_height = plan.desktop_height,
                "NVIDIA headless paths activated atomically before Windows geometry apply"
            );
            Ok(false)
        }

        fn apply(
            &mut self,
            plan: &crate::multi_monitor_topology::WindowsTopologyPlan,
        ) -> Result<(), String> {
            let mut paths = self.working.paths.clone();
            let mut modes = self.working.modes.clone();
            let mut matched = std::collections::BTreeSet::new();

            for path in &mut paths {
                let planned = plan.monitors.iter().find(|monitor| {
                    monitor.adapter_luid.low_part == path.targetInfo.adapterId.LowPart
                        && monitor.adapter_luid.high_part == path.targetInfo.adapterId.HighPart
                        && monitor.target_id == path.targetInfo.id
                });
                let Some(monitor) = planned else {
                    path.flags &= !PATH_ACTIVE_FLAG;
                    path.sourceInfo.Anonymous.modeInfoIdx = PATH_MODE_INDICES_INVALID;
                    path.targetInfo.Anonymous.modeInfoIdx = PATH_MODE_INDICES_INVALID;
                    continue;
                };
                matched.insert(monitor.session_monitor_id.get());
                let source_mode_index = virtual_source_mode_index(unsafe {
                    path.sourceInfo.Anonymous.Anonymous._bitfield
                })
                .ok_or_else(|| {
                    format!(
                        "selected output {} has no virtual source mode",
                        monitor.device_name
                    )
                })?;
                let source_mode = modes.get_mut(source_mode_index).ok_or_else(|| {
                    format!(
                        "selected output {} source mode index is outside the topology",
                        monitor.device_name
                    )
                })?;
                if source_mode.infoType != DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE {
                    return Err(format!(
                        "selected output {} mode is not a source mode",
                        monitor.device_name
                    ));
                }
                source_mode.Anonymous.sourceMode.width = monitor.width;
                source_mode.Anonymous.sourceMode.height = monitor.height;
                source_mode.Anonymous.sourceMode.position = POINTL {
                    x: monitor.x,
                    y: monitor.y,
                };
                path.targetInfo.rotation = rotation_to_display_config(monitor.rotation);
                path.flags |= PATH_ACTIVE_FLAG;
            }
            if matched.len() != plan.monitors.len() {
                return Err(format!(
                    "only {} of {} requested outputs exist in the active topology",
                    matched.len(),
                    plan.monitors.len()
                ));
            }
            let flags = SET_DISPLAY_CONFIG_FLAGS(
                SDC_APPLY.0
                    | SDC_USE_SUPPLIED_DISPLAY_CONFIG.0
                    | SDC_ALLOW_CHANGES.0
                    | SDC_VIRTUAL_MODE_AWARE.0,
            );
            let rc = unsafe { SetDisplayConfig(Some(&paths), Some(&modes), flags) };
            if rc != 0 {
                return Err(format!(
                    "SetDisplayConfig multi-monitor apply returned {rc}"
                ));
            }
            Ok(())
        }

        fn commit(&mut self) -> Result<(), String> {
            if !self.recovery_armed {
                return Ok(());
            }
            crate::recovery::remove(&self.journal_path)?;
            self.recovery_armed = false;
            self.nvapi_modes.clear();
            self.nvapi_driver = None;
            self.headless_entries.clear();
            self.applied_dpi_scales.clear();
            tracing::info!(
                target: DISPLAY,
                "verified physical output-provider topology committed for the persistent dedicated Windows desktop"
            );
            Ok(())
        }

        fn restore(&mut self) -> Result<(), String> {
            if !self.recovery_armed {
                return Ok(());
            }
            self.restore_applied_dpi_scales();
            if self.headless_entries.is_empty() {
                if let Err(error) = self.restore_nvapi_modes() {
                    tracing::warn!(
                        target: DISPLAY,
                        %error,
                        "one or more temporary NVAPI modes could not be removed before display rollback"
                    );
                }
            } else {
                // Headless outputs are temporary by construction. Calling
                // restore_exact here first asks NVAPI to reinstate a topology
                // containing paths that Windows is concurrently tearing down;
                // the V100D driver can block inside that call indefinitely.
                // Purging the journalled EDIDs invalidates the temporary modes,
                // then the original CCD snapshot below restores the desktop.
                crate::nvapi_headless::restore_recovery_entries(&self.headless_entries)
                    .map_err(|error| format!("restore NVIDIA headless EDIDs: {error}"))?;
                self.nvapi_modes.clear();
                self.nvapi_driver = None;
            }
            let flags = SET_DISPLAY_CONFIG_FLAGS(
                SDC_APPLY.0
                    | SDC_USE_SUPPLIED_DISPLAY_CONFIG.0
                    | SDC_VIRTUAL_MODE_AWARE.0
                    | if self.headless_entries.is_empty() {
                        0
                    } else {
                        SDC_ALLOW_CHANGES.0
                    },
            );
            let initial_rc = unsafe {
                SetDisplayConfig(
                    Some(&self.original.paths),
                    Some(&self.original.modes),
                    flags,
                )
            };
            if initial_rc != 0 {
                return Err(format!(
                    "SetDisplayConfig pre-cleanup restore returned {initial_rc}"
                ));
            }
            // Bounded by wall-clock, not by a fixed attempt count. Three
            // attempts with a 50ms poll only ever spent ~100ms waiting for a
            // topology that ADR 0008 measured taking ~1s to converge for a
            // single target, so a correct restore was routinely reported as a
            // failure -- which then left the journal armed, because the
            // journal is only removed on the success path below. The apply
            // direction already waits `MODE_SETTLE_TIMEOUT` through
            // `wait_for_multi_display_plan`; the rollback direction gets the
            // same budget.
            let started = Instant::now();
            let deadline = started + MODE_SETTLE_TIMEOUT;
            let mut attempts = 0_u32;
            let last_error = loop {
                attempts += 1;
                let rc = unsafe {
                    SetDisplayConfig(
                        Some(&self.original.paths),
                        Some(&self.original.modes),
                        flags,
                    )
                };
                let error = if rc == 0 {
                    match verify_restored_topology(&self.original, &self.original_stable) {
                        Ok(()) => {
                            crate::recovery::remove(&self.journal_path)?;
                            self.recovery_armed = false;
                            self.headless_entries.clear();
                            tracing::info!(
                                target: DISPLAY,
                                attempts,
                                elapsed_ms = u64::try_from(started.elapsed().as_millis())
                                    .unwrap_or(u64::MAX),
                                "original full display topology restored"
                            );
                            return Ok(());
                        }
                        Err(error) => error,
                    }
                } else {
                    format!("SetDisplayConfig restore returned {rc}")
                };
                if Instant::now() >= deadline {
                    break error;
                }
                std::thread::sleep(MODE_SETTLE_POLL);
            };
            tracing::warn!(
                target: DISPLAY,
                attempts,
                elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                headless_entries = self.headless_entries.len(),
                error = %last_error,
                "display topology restore exhausted its settle budget; recovery journal stays armed"
            );
            Err(last_error)
        }

        fn restore_nvapi_modes(&mut self) -> Result<(), String> {
            let Some(driver) = self.nvapi_driver.as_mut() else {
                self.nvapi_modes.clear();
                return Ok(());
            };
            let mut errors = Vec::new();
            while let Some((snapshot, active)) = self.nvapi_modes.pop() {
                if let Err(error) = nvapi::restore_exact(driver, &snapshot, Some(&active)) {
                    errors.push(error);
                }
            }
            if errors.is_empty() {
                Ok(())
            } else {
                Err(errors.join("; "))
            }
        }

        fn apply_requested_dpi_scales(&mut self) -> Result<(), String> {
            let monitors = self.plan().monitors.clone();
            let mut applied = Vec::new();
            for monitor in monitors {
                match apply_monitor_dpi_scale(&monitor) {
                    Ok(Some(scale)) => applied.push(scale),
                    Ok(None) => {}
                    Err(error) => {
                        for scale in applied.iter().rev() {
                            let _ = restore_applied_dpi_scale(scale);
                        }
                        return Err(error);
                    }
                }
            }
            self.applied_dpi_scales = applied;
            Ok(())
        }

        fn restore_applied_dpi_scales(&mut self) {
            for scale in self.applied_dpi_scales.drain(..).rev() {
                if let Err(error) = restore_applied_dpi_scale(&scale) {
                    tracing::warn!(
                        target: DISPLAY,
                        device = %scale.device_name,
                        %error,
                        "prior Windows UI scale could not be restored before topology rollback"
                    );
                }
            }
        }
    }

    fn rotation_to_display_config(rotation: arcen_media::Rotation) -> DISPLAYCONFIG_ROTATION {
        match rotation {
            arcen_media::Rotation::Degrees0 => DISPLAYCONFIG_ROTATION_IDENTITY,
            arcen_media::Rotation::Degrees90 => DISPLAYCONFIG_ROTATION_ROTATE90,
            arcen_media::Rotation::Degrees180 => DISPLAYCONFIG_ROTATION_ROTATE180,
            arcen_media::Rotation::Degrees270 => DISPLAYCONFIG_ROTATION_ROTATE270,
        }
    }

    fn empty_multi_display_report(
        plan: &crate::multi_monitor_topology::WindowsTopologyPlan,
    ) -> Result<DisplayReport, String> {
        let primary = plan.primary();
        Ok(report(
            DisplaySize {
                width: primary.width,
                height: primary.height,
            },
            ModeState {
                size: DisplaySize {
                    width: primary.width,
                    height: primary.height,
                },
                refresh_hz: primary.refresh_hz,
                output_index: primary.global_index,
                desktop_rect: DesktopRect {
                    left: primary.x,
                    top: primary.y,
                    width: i32::try_from(primary.width)
                        .map_err(|_| "primary width exceeds i32".to_string())?,
                    height: i32::try_from(primary.height)
                        .map_err(|_| "primary height exceeds i32".to_string())?,
                },
                active_outputs: u32::try_from(plan.monitors.len()).unwrap_or(u32::MAX),
            },
            ModeState {
                size: DisplaySize {
                    width: primary.width,
                    height: primary.height,
                },
                refresh_hz: primary.refresh_hz,
                output_index: primary.global_index,
                desktop_rect: DesktopRect {
                    left: primary.x,
                    top: primary.y,
                    width: i32::try_from(primary.width)
                        .map_err(|_| "primary width exceeds i32".to_string())?,
                    height: i32::try_from(primary.height)
                        .map_err(|_| "primary height exceeds i32".to_string())?,
                },
                active_outputs: u32::try_from(plan.monitors.len()).unwrap_or(u32::MAX),
            },
            true,
            true,
            "set-display-config-multi-monitor",
            "set-display-config-full-topology",
            &DisplayTarget {
                device_name: primary.device_name.clone(),
                vendor_id: 0x10de,
                adapter_luid: primary.adapter_luid,
                adapter_output_index: primary.adapter_output_index,
            },
        ))
    }

    fn multi_display_report(
        plan: &crate::multi_monitor_topology::WindowsTopologyPlan,
        effective_scale_reports: Vec<DisplayScaleReport>,
    ) -> Result<DisplayReport, String> {
        let mut report = empty_multi_display_report(plan)?;
        report.effective_scale_reports = effective_scale_reports;
        Ok(report)
    }

    fn monitor_center_point(
        monitor: &crate::multi_monitor_topology::WindowsMonitorPlan,
    ) -> Result<POINT, String> {
        let half_width = i32::try_from(monitor.width / 2)
            .map_err(|_| format!("monitor {} width exceeds i32", monitor.device_name))?;
        let half_height = i32::try_from(monitor.height / 2)
            .map_err(|_| format!("monitor {} height exceeds i32", monitor.device_name))?;
        Ok(POINT {
            x: monitor
                .x
                .checked_add(half_width)
                .ok_or_else(|| format!("monitor {} center x exceeds i32", monitor.device_name))?,
            y: monitor
                .y
                .checked_add(half_height)
                .ok_or_else(|| format!("monitor {} center y exceeds i32", monitor.device_name))?,
        })
    }

    const WINDOWS_DPI_SCALE_STEPS: [u16; 12] =
        [100, 125, 150, 175, 200, 225, 250, 300, 350, 400, 450, 500];
    const DISPLAYCONFIG_DEVICE_INFO_GET_DPI_SCALE: DISPLAYCONFIG_DEVICE_INFO_TYPE =
        DISPLAYCONFIG_DEVICE_INFO_TYPE(-3);
    const DISPLAYCONFIG_DEVICE_INFO_SET_DPI_SCALE: DISPLAYCONFIG_DEVICE_INFO_TYPE =
        DISPLAYCONFIG_DEVICE_INFO_TYPE(-4);

    #[repr(C)]
    struct DisplayConfigSourceDpiScaleGet {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER,
        min_scale_relative: i32,
        current_scale_relative: i32,
        max_scale_relative: i32,
    }

    #[repr(C)]
    struct DisplayConfigSourceDpiScaleSet {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER,
        scale_relative: i32,
    }

    const _: () = assert!(std::mem::size_of::<DisplayConfigSourceDpiScaleGet>() == 32);
    const _: () = assert!(std::mem::size_of::<DisplayConfigSourceDpiScaleSet>() == 24);

    fn effective_scale_at(rect: DesktopRect) -> Result<(u32, u32, u16), String> {
        let point = POINT {
            x: rect.left.saturating_add(rect.width / 2),
            y: rect.top.saturating_add(rect.height / 2),
        };
        let hmonitor = unsafe { MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST) };
        if hmonitor.is_invalid() {
            return Err(format!(
                "no monitor exists at applied display center {},{}",
                point.x, point.y
            ));
        }
        let mut dpi_x = 0_u32;
        let mut dpi_y = 0_u32;
        unsafe { GetDpiForMonitor(hmonitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) }
            .map_err(|error| format!("GetDpiForMonitor failed: {error}"))?;
        let percent = effective_scale_percent_from_dpi(dpi_x, dpi_y)
            .ok_or_else(|| "GetDpiForMonitor returned zero DPI".to_string())?;
        Ok((dpi_x, dpi_y, percent))
    }

    fn active_source_path(device_name: &str) -> Result<DISPLAYCONFIG_PATH_INFO, String> {
        query_active_topology()?
            .paths
            .into_iter()
            .find_map(|path| {
                source_gdi_name(&path)
                    .ok()
                    .filter(|name| name.eq_ignore_ascii_case(device_name))
                    .map(|_| path)
            })
            .ok_or_else(|| format!("active display source {device_name} was not found"))
    }

    fn display_source_key(path: &DISPLAYCONFIG_PATH_INFO) -> DisplaySourceKey {
        DisplaySourceKey {
            adapter_id: path.sourceInfo.adapterId,
            source_id: path.sourceInfo.id,
        }
    }

    fn source_dpi_scale(
        source: DisplaySourceKey,
    ) -> Result<DisplayConfigSourceDpiScaleGet, String> {
        let mut request = DisplayConfigSourceDpiScaleGet {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_GET_DPI_SCALE,
                size: std::mem::size_of::<DisplayConfigSourceDpiScaleGet>() as u32,
                adapterId: source.adapter_id,
                id: source.source_id,
            },
            min_scale_relative: 0,
            current_scale_relative: 0,
            max_scale_relative: 0,
        };
        // SAFETY: the pointer originates from the complete repr(C) packet, not
        // its header subobject. The packet sizes are compile-time pinned above,
        // and Windows reads exactly `header.size` bytes synchronously.
        let status = unsafe { DisplayConfigGetDeviceInfo(std::ptr::addr_of_mut!(request).cast()) };
        if status != 0 {
            return Err(format!(
                "DisplayConfigGetDeviceInfo(DPI scale) returned {status} for source {}",
                source.source_id
            ));
        }
        let relative_limit =
            i32::try_from(WINDOWS_DPI_SCALE_STEPS.len() - 1).expect("scale-step count fits i32");
        if request.min_scale_relative > request.current_scale_relative
            || request.current_scale_relative > request.max_scale_relative
            || request.min_scale_relative < -relative_limit
            || request.max_scale_relative > relative_limit
        {
            return Err(format!(
                "Windows returned invalid DPI scale bounds {}..={} with current {}",
                request.min_scale_relative,
                request.max_scale_relative,
                request.current_scale_relative
            ));
        }
        Ok(request)
    }

    fn set_source_dpi_scale(source: DisplaySourceKey, scale_relative: i32) -> Result<(), String> {
        let mut request = DisplayConfigSourceDpiScaleSet {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_SET_DPI_SCALE,
                size: std::mem::size_of::<DisplayConfigSourceDpiScaleSet>() as u32,
                adapterId: source.adapter_id,
                id: source.source_id,
            },
            scale_relative,
        };
        // SAFETY: same whole-packet provenance and fixed layout contract as
        // `source_dpi_scale`; Windows consumes the packet synchronously.
        let status = unsafe { DisplayConfigSetDeviceInfo(std::ptr::addr_of_mut!(request).cast()) };
        if status != 0 {
            return Err(format!(
                "DisplayConfigSetDeviceInfo(DPI scale {scale_relative}) returned {status} for \
                 source {}",
                source.source_id
            ));
        }
        Ok(())
    }

    fn dpi_scale_step(percent: u16) -> Option<i32> {
        WINDOWS_DPI_SCALE_STEPS
            .iter()
            .position(|candidate| candidate.abs_diff(percent) <= 5)
            .and_then(|index| i32::try_from(index).ok())
    }

    pub(super) fn requested_relative_scale(
        current_percent: u16,
        requested_percent: u16,
        current_relative: i32,
        minimum_relative: i32,
        maximum_relative: i32,
    ) -> Result<i32, String> {
        let current_step = dpi_scale_step(current_percent).ok_or_else(|| {
            format!("Windows effective scale {current_percent}% is not a supported DPI step")
        })?;
        let requested_step = dpi_scale_step(requested_percent).ok_or_else(|| {
            format!("requested scale {requested_percent}% is not a supported Windows DPI step")
        })?;
        let recommended_step = current_step
            .checked_sub(current_relative)
            .ok_or_else(|| "Windows recommended DPI step overflowed".to_string())?;
        let scale_step_count =
            i32::try_from(WINDOWS_DPI_SCALE_STEPS.len()).expect("scale-step count fits i32");
        if !(0..scale_step_count).contains(&recommended_step) {
            return Err(format!(
                "Windows recommended DPI step {recommended_step} is outside the supported table"
            ));
        }
        let requested_relative = requested_step
            .checked_sub(recommended_step)
            .ok_or_else(|| "requested Windows DPI step overflowed".to_string())?;
        if requested_relative < minimum_relative || requested_relative > maximum_relative {
            return Err(format!(
                "requested scale {requested_percent}% requires relative step \
                 {requested_relative}, outside Windows range \
                 {minimum_relative}..={maximum_relative}"
            ));
        }
        Ok(requested_relative)
    }

    fn wait_for_effective_scale(rect: DesktopRect, requested_percent: u16) -> Result<(), String> {
        let deadline = Instant::now() + MODE_SETTLE_TIMEOUT;
        loop {
            let last = match effective_scale_at(rect) {
                Ok((_, _, effective))
                    if display_scale_matches_requested(requested_percent, effective) =>
                {
                    return Ok(());
                }
                Ok((_, _, effective)) => format!("effective scale is {effective}%"),
                Err(error) => error,
            };
            if Instant::now() >= deadline {
                return Err(format!(
                    "Windows did not settle at requested scale {requested_percent}% within {}ms \
                     ({last})",
                    MODE_SETTLE_TIMEOUT.as_millis()
                ));
            }
            std::thread::sleep(MODE_SETTLE_POLL);
        }
    }

    pub(super) fn apply_single_display_scale(
        device_name: &str,
        rect: DesktopRect,
        requested_percent: u16,
    ) -> Result<(), String> {
        let (_, _, current_percent) = effective_scale_at(rect)?;
        if display_scale_matches_requested(requested_percent, current_percent) {
            return Ok(());
        }
        let path = active_source_path(device_name)?;
        let source = display_source_key(&path);
        let current = source_dpi_scale(source)?;
        let requested_relative = requested_relative_scale(
            current_percent,
            requested_percent,
            current.current_scale_relative,
            current.min_scale_relative,
            current.max_scale_relative,
        )?;
        set_source_dpi_scale(source, requested_relative)?;

        if let Err(error) = wait_for_effective_scale(rect, requested_percent) {
            let rollback = set_source_dpi_scale(source, current.current_scale_relative)
                .and_then(|()| wait_for_effective_scale(rect, current_percent));
            return Err(match rollback {
                Ok(()) => format!("{error}; prior scale was restored and verified"),
                Err(rollback) => {
                    format!("{error}; restoring the prior scale failed: {rollback}")
                }
            });
        }
        tracing::info!(
            target: DISPLAY,
            device = %device_name,
            previous_scale_percent = current_percent,
            requested_scale_percent = requested_percent,
            relative_scale = requested_relative,
            "Windows UI scale applied and verified for single-display session"
        );
        Ok(())
    }

    fn apply_monitor_dpi_scale(
        monitor: &crate::multi_monitor_topology::WindowsMonitorPlan,
    ) -> Result<Option<AppliedDpiScale>, String> {
        let rect = DesktopRect {
            left: monitor.x,
            top: monitor.y,
            width: i32::try_from(monitor.width)
                .map_err(|_| format!("monitor {} width exceeds i32", monitor.device_name))?,
            height: i32::try_from(monitor.height)
                .map_err(|_| format!("monitor {} height exceeds i32", monitor.device_name))?,
        };
        let requested_percent = requested_scale_percent(monitor.scale);
        let (_, _, current_percent) = effective_scale_at(rect)?;
        if display_scale_matches_requested(requested_percent, current_percent) {
            return Ok(None);
        }
        let path = active_source_path(&monitor.device_name)?;
        let source = display_source_key(&path);
        let current = source_dpi_scale(source)?;
        let requested_relative = requested_relative_scale(
            current_percent,
            requested_percent,
            current.current_scale_relative,
            current.min_scale_relative,
            current.max_scale_relative,
        )?;
        set_source_dpi_scale(source, requested_relative)?;
        if let Err(error) = wait_for_effective_scale(rect, requested_percent) {
            let rollback = set_source_dpi_scale(source, current.current_scale_relative)
                .and_then(|()| wait_for_effective_scale(rect, current_percent));
            return Err(match rollback {
                Ok(()) => format!(
                    "apply Windows UI scale for {}: {error}; prior scale was restored",
                    monitor.device_name
                ),
                Err(rollback) => format!(
                    "apply Windows UI scale for {}: {error}; restoring prior scale failed: {rollback}",
                    monitor.device_name
                ),
            });
        }
        tracing::info!(
            target: DISPLAY,
            device = %monitor.device_name,
            previous_scale_percent = current_percent,
            requested_scale_percent = requested_percent,
            relative_scale = requested_relative,
            "Windows UI scale applied and verified for multi-display session"
        );
        Ok(Some(AppliedDpiScale {
            source,
            previous_relative: current.current_scale_relative,
            previous_percent: current_percent,
            rect,
            device_name: monitor.device_name.clone(),
        }))
    }

    fn restore_applied_dpi_scale(scale: &AppliedDpiScale) -> Result<(), String> {
        set_source_dpi_scale(scale.source, scale.previous_relative)?;
        wait_for_effective_scale(scale.rect, scale.previous_percent)
    }

    /// Report the effective UI scale Windows resolved for a single-display
    /// session, warning when it disagrees with what the client asked for.
    ///
    /// The multi-display sibling is `effective_scale_reports`; this exists
    /// because the single-display lease had no equivalent and a 250% desktop
    /// went unremarked in the host log while being unmissable on screen.
    ///
    /// Deliberately non-fatal in every direction. A probe that cannot resolve
    /// the monitor logs and returns: an unreadable DPI is not a reason to
    /// refuse a session that is otherwise working, and this is a diagnostic,
    /// not a gate.
    pub(super) fn log_single_display_effective_scale(
        device_name: &str,
        rect: DesktopRect,
        requested_scale_percent: u16,
    ) {
        let (dpi_x, dpi_y, effective_scale_percent) = match effective_scale_at(rect) {
            Ok(report) => report,
            Err(error) => {
                tracing::warn!(
                    target: DISPLAY,
                    device = %device_name,
                    %error,
                    "single-display effective scale could not be read"
                );
                return;
            }
        };
        let matches_requested =
            display_scale_matches_requested(requested_scale_percent, effective_scale_percent);
        tracing::info!(
            target: DISPLAY,
            device = %device_name,
            requested_scale_percent,
            effective_dpi_x = dpi_x,
            effective_dpi_y = dpi_y,
            effective_scale_percent,
            matches_requested,
            "Windows effective display scale resolved for single-display session"
        );
        if !matches_requested {
            tracing::warn!(
                target: DISPLAY,
                device = %device_name,
                requested_scale_percent,
                effective_scale_percent,
                effective_dpi_x = dpi_x,
                effective_dpi_y = dpi_y,
                "Windows resolved a different UI scale than the client requested; the desktop \
                 will look larger or smaller than the stream contract implies"
            );
        }
    }

    fn effective_scale_report(
        monitor: &crate::multi_monitor_topology::WindowsMonitorPlan,
    ) -> Result<DisplayScaleReport, String> {
        let point = monitor_center_point(monitor)?;
        let hmonitor = unsafe { MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST) };
        if hmonitor.is_invalid() {
            return Err(format!(
                "MonitorFromPoint could not resolve {} at {},{}",
                monitor.device_name, point.x, point.y
            ));
        }
        let mut dpi_x = 0_u32;
        let mut dpi_y = 0_u32;
        // GetDpiForMonitor(MDT_EFFECTIVE_DPI) is the documented verification
        // oracle for the per-monitor UI scale. The single-display NVIDIA path
        // applies the requested Settings-equivalent step through guarded
        // DisplayConfig device-info packets, then proves the result here.
        unsafe { GetDpiForMonitor(hmonitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) }.map_err(
            |error| format!("GetDpiForMonitor({}) failed: {error}", monitor.device_name),
        )?;
        let effective_scale_percent =
            effective_scale_percent_from_dpi(dpi_x, dpi_y).ok_or_else(|| {
                format!(
                    "GetDpiForMonitor({}) returned zero DPI",
                    monitor.device_name
                )
            })?;
        let requested_scale_percent = requested_scale_percent(monitor.scale);
        Ok(DisplayScaleReport {
            client_display_id: monitor.client_display_id.clone(),
            session_monitor_id: monitor.session_monitor_id.get(),
            device_name: monitor.device_name.clone(),
            requested_scale_percent,
            effective_dpi_x: dpi_x,
            effective_dpi_y: dpi_y,
            effective_scale_percent,
            matches_requested: display_scale_matches_requested(
                requested_scale_percent,
                effective_scale_percent,
            ),
        })
    }

    pub(super) fn effective_scale_reports(
        plan: &crate::multi_monitor_topology::WindowsTopologyPlan,
    ) -> Vec<DisplayScaleReport> {
        plan.monitors
            .iter()
            .filter_map(|monitor| match effective_scale_report(monitor) {
                Ok(report) => Some(report),
                Err(error) => {
                    tracing::warn!(
                        target: DISPLAY,
                        client_display_id = %monitor.client_display_id,
                        session_monitor_id = monitor.session_monitor_id.get(),
                        device = %monitor.device_name,
                        %error,
                        "Windows effective display scale could not be read for applied monitor"
                    );
                    None
                }
            })
            .collect()
    }

    fn log_effective_scale_reports(reports: &[DisplayScaleReport]) {
        for report in reports {
            tracing::info!(
                target: DISPLAY,
                client_display_id = %report.client_display_id,
                session_monitor_id = report.session_monitor_id,
                device = %report.device_name,
                requested_scale_percent = report.requested_scale_percent,
                effective_dpi_x = report.effective_dpi_x,
                effective_dpi_y = report.effective_dpi_y,
                effective_scale_percent = report.effective_scale_percent,
                matches_requested = report.matches_requested,
                "Windows effective display scale resolved for applied monitor"
            );
            if !report.matches_requested {
                tracing::warn!(
                    target: DISPLAY,
                    client_display_id = %report.client_display_id,
                    session_monitor_id = report.session_monitor_id,
                    device = %report.device_name,
                    requested_scale_percent = report.requested_scale_percent,
                    effective_scale_percent = report.effective_scale_percent,
                    effective_dpi_x = report.effective_dpi_x,
                    effective_dpi_y = report.effective_dpi_y,
                    "Windows effective display scale differs from requested client scale; continuing fail-loud so the session remains usable"
                );
            }
        }
    }

    fn wait_for_multi_display_plan(
        plan: &crate::multi_monitor_topology::WindowsTopologyPlan,
        timeout: Duration,
    ) -> Result<(), String> {
        let deadline = Instant::now() + timeout;
        loop {
            match verify_multi_display_plan(plan) {
                Ok(()) => return Ok(()),
                Err(error) if Instant::now() < deadline => {
                    let _ = error;
                    std::thread::sleep(MODE_SETTLE_POLL);
                }
                Err(error) => return Err(error),
            }
        }
    }

    fn verify_multi_display_plan(
        plan: &crate::multi_monitor_topology::WindowsTopologyPlan,
    ) -> Result<(), String> {
        let allowed_adapters = plan
            .monitors
            .iter()
            .map(|monitor| monitor.adapter_name.clone())
            .collect::<Vec<_>>();
        let inventory = crate::gpu_probe::physical_output_inventory(&allowed_adapters)?;
        if inventory.len() != plan.monitors.len() {
            return Err(format!(
                "applied topology exposes {} capture-capable outputs instead of {}",
                inventory.len(),
                plan.monitors.len()
            ));
        }
        for monitor in &plan.monitors {
            let output = inventory
                .outputs()
                .iter()
                .find(|output| {
                    output.adapter_luid == monitor.adapter_luid
                        && output.target_id == monitor.target_id
                })
                .ok_or_else(|| {
                    format!(
                        "applied output {} is missing after topology commit",
                        monitor.device_name
                    )
                })?;
            if (
                output.current_x,
                output.current_y,
                output.current_width,
                output.current_height,
            ) != (monitor.x, monitor.y, monitor.width, monitor.height)
            {
                return Err(format!(
                    "applied output {} geometry is {}x{}@{},{} instead of {}x{}@{},{}",
                    monitor.device_name,
                    output.current_width,
                    output.current_height,
                    output.current_x,
                    output.current_y,
                    monitor.width,
                    monitor.height,
                    monitor.x,
                    monitor.y
                ));
            }
        }
        let topology = query_active_topology()?;
        for monitor in &plan.monitors {
            let path = topology
                .paths
                .iter()
                .find(|path| {
                    path.targetInfo.adapterId.LowPart == monitor.adapter_luid.low_part
                        && path.targetInfo.adapterId.HighPart == monitor.adapter_luid.high_part
                        && path.targetInfo.id == monitor.target_id
                })
                .ok_or_else(|| format!("applied path {} is missing", monitor.device_name))?;
            if path.targetInfo.rotation != rotation_to_display_config(monitor.rotation) {
                return Err(format!(
                    "applied output {} rotation does not match {:?}",
                    monitor.device_name, monitor.rotation
                ));
            }
        }
        Ok(())
    }

    fn verify_restored_topology(
        original: &TopologySnapshot,
        stable: &crate::recovery::StableTopologySnapshot,
    ) -> Result<(), String> {
        let current = query_active_topology()?;
        let current_stable = capture_stable_topology(&current)?;
        if complete_topology_with_stable_identities(
            &original.paths,
            &original.modes,
            stable,
            &current.paths,
            &current.modes,
            &current_stable,
        )? {
            Ok(())
        } else {
            Err("restored display topology does not match the original snapshot".to_string())
        }
    }

    impl DisplayBackend for NativeBackend {
        type Snapshot = WindowsSnapshot;

        fn select_target(&mut self, selector: &OutputSelector) -> Result<DisplayTarget, String> {
            let outputs = self.dxgi.outputs()?;
            let output = select_dxgi_output(selector, &outputs)?;
            Ok(DisplayTarget {
                device_name: output.device_name.clone(),
                vendor_id: output.vendor_id,
                adapter_luid: output.adapter_luid,
                adapter_output_index: output.adapter_output_index,
            })
        }

        fn prepare_target(&mut self, target: &DisplayTarget) -> Result<(), String> {
            let protocol = current_session_protocol_type()?;
            if protocol != 0 {
                return Err(remote_session_display_error(protocol));
            }
            wait_for_target_ready(&mut self.dxgi, target, TARGET_READY_TIMEOUT)
        }

        fn snapshot(&mut self, target: &DisplayTarget) -> Result<Self::Snapshot, String> {
            let topology = query_active_topology()?;
            let mode = current_devmode(&target.device_name)?;
            let original = mode_state(&mut self.dxgi, target, &mode)?;
            let nvapi = if target.vendor_id == 0x10de {
                match nvapi::Nvapi::load().and_then(|mut driver| {
                    let snapshot = nvapi::snapshot(
                        &mut driver,
                        &target.device_name,
                        target.adapter_luid,
                        self.request.size.width,
                        self.request.size.height,
                        self.request.refresh_hz.max(1),
                    )?;
                    self.nvapi = Some(driver);
                    Ok(snapshot)
                }) {
                    Ok(snapshot) => {
                        tracing::info!(
                            target: DISPLAY,
                            device = %target.device_name,
                            display_id = format_args!("0x{:08x}", snapshot.mapping.display_id),
                            output_id = format_args!("0x{:08x}", snapshot.mapping.output_id),
                            head = snapshot.mapping.head,
                            luid_high = format_args!("0x{:08x}", target.adapter_luid.high_part as u32),
                            luid_low = format_args!("0x{:08x}", target.adapter_luid.low_part),
                            "NVAPI exact-resolution capability mapped to selected NVIDIA output"
                        );
                        self.nvapi_snapshot = Some(snapshot.clone());
                        Some(snapshot)
                    }
                    Err(error) => {
                        tracing::warn!(
                            target: DISPLAY,
                            device = %target.device_name,
                            %error,
                            "NVAPI exact-resolution capability unavailable; Windows mode fallback remains enabled"
                        );
                        self.nvapi_failed = true;
                        None
                    }
                }
            } else {
                tracing::info!(
                    target: DISPLAY,
                    device = %target.device_name,
                    vendor_id = format_args!("0x{:04x}", target.vendor_id),
                    "selected output is not NVIDIA; skipping NVAPI exact-resolution backend"
                );
                self.nvapi_failed = true;
                None
            };
            Ok(WindowsSnapshot {
                topology,
                mode,
                original,
                nvapi,
            })
        }

        fn current(&mut self, target: &DisplayTarget) -> Result<ModeState, String> {
            current_mode(&mut self.dxgi, target)
        }

        fn supported_sizes(&mut self, target: &DisplayTarget) -> Result<Vec<DisplaySize>, String> {
            supported_sizes(&target.device_name)
        }

        fn requires_contract_refresh(
            &self,
            target: &DisplayTarget,
            size: DisplaySize,
        ) -> Result<bool, String> {
            if !nvapi_exact_available(
                target.vendor_id,
                self.nvapi_failed,
                self.nvapi.is_some(),
                self.nvapi_snapshot.is_some(),
            ) {
                return Ok(false);
            }
            let desired = self.desired_edid(size)?;
            // The link in the HDR chain that Arcen controls, recorded where it
            // is made. Windows offers Advanced Color only where the sink
            // claims HDR10, so an EDID that did not go on explains every
            // downstream measurement that comes back SDR.
            tracing::debug!(
                target: DISPLAY,
                hdr10 = self.request.hdr10,
                edid_bytes = desired.len(),
                "synthesised display EDID"
            );
            let current = self
                .nvapi_snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.original_edid.as_deref());
            Ok(current != Some(desired.as_slice()))
        }

        fn prepare_exact_retarget(
            &mut self,
            target: &DisplayTarget,
            size: DisplaySize,
        ) -> Result<(), String> {
            let recovering_failed_apply = self.nvapi_failed && self.nvapi_active.is_some();
            require_nvapi_exact_retarget(
                target.vendor_id,
                self.nvapi_failed && !recovering_failed_apply,
                self.nvapi.is_some(),
                self.nvapi_snapshot.is_some(),
            )?;
            if target.vendor_id != 0x10de {
                return Ok(());
            }
            let driver = self
                .nvapi
                .as_mut()
                .ok_or_else(|| "NVAPI retarget lost its driver".to_string())?;
            let snapshot = self
                .nvapi_snapshot
                .clone()
                .ok_or_else(|| "NVAPI retarget lost its original snapshot".to_string())?;

            let active =
                take_nvapi_active_after_stage(&mut self.nvapi_active, self.recovery_armed, || {
                    crate::recovery::nvapi_cleanup_stage(&self.journal_path)
                })?;
            if let Some((active, cleanup_stage)) = active {
                let recovery_armed = self.recovery_armed;
                let journal_path = self.journal_path.clone();
                if let Err(error) = nvapi::restore_exact_staged(
                    driver,
                    &snapshot,
                    Some(&active),
                    cleanup_stage,
                    |stage| {
                        if recovery_armed {
                            crate::recovery::mark_nvapi_cleanup_stage(&journal_path, stage)
                        } else {
                            Ok(())
                        }
                    },
                ) {
                    self.nvapi_active = Some(active);
                    return Err(format!(
                        "clean previous NVAPI exact timing before media retarget: {error}"
                    ));
                }
                self.dxgi.invalidate();
                self.nvapi_failed = false;
            }

            let retarget = nvapi::retarget_snapshot(
                driver,
                &snapshot,
                size.width,
                size.height,
                self.request.refresh_hz.max(1),
            )?;
            if self.recovery_armed {
                crate::recovery::rearm_nvapi(
                    &self.journal_path,
                    nvapi::recovery_data(
                        target.device_name.clone(),
                        &retarget,
                        size.width,
                        size.height,
                        self.request.refresh_hz.max(1),
                    ),
                )?;
            }
            self.nvapi_snapshot = Some(retarget);
            Ok(())
        }

        fn test_mode(&mut self, target: &DisplayTarget, size: DisplaySize) -> Result<(), String> {
            if target.vendor_id == VMWARE_VENDOR_ID
                && !self.vmware_failed
                && vmware_resolution_tool().is_some()
                && vmware_resolution_supported(&mut self.dxgi, target)
            {
                self.pending_nvapi = false;
                self.pending_vmware = true;
                return Ok(());
            }
            self.pending_vmware = false;
            if nvapi_exact_available(
                target.vendor_id,
                self.nvapi_failed,
                self.nvapi.is_some(),
                self.nvapi_snapshot.is_some(),
            ) {
                self.desired_edid(size)?;
                self.pending_nvapi = true;
                return Ok(());
            }
            self.pending_nvapi = false;
            change_mode(&target.device_name, size, CDS_TEST)
        }

        fn apply_mode(
            &mut self,
            target: &DisplayTarget,
            size: DisplaySize,
        ) -> Result<ModeState, String> {
            if self.pending_vmware {
                self.pending_vmware = false;
                let exit_code = match apply_vmware_resolution(size) {
                    Ok(exit_code) => exit_code,
                    Err(error) => {
                        self.vmware_failed = true;
                        return Err(error);
                    }
                };
                self.dxgi.invalidate();
                match wait_for_mode(&mut self.dxgi, target, size, VMWARE_MODE_SETTLE_TIMEOUT) {
                    Ok(mode) => {
                        if exit_code != 0 {
                            tracing::warn!(
                                target: DISPLAY,
                                %size,
                                exit_code,
                                "VMwareResolutionSet returned failure but the requested mode settled successfully"
                            );
                        }
                        self.vmware_active = true;
                        return Ok(mode);
                    }
                    Err(settle_error) => {
                        self.vmware_failed = true;
                        return Err(format!(
                            "VMwareResolutionSet exited {exit_code}; {settle_error}"
                        ));
                    }
                }
            }
            if self.pending_nvapi {
                self.pending_nvapi = false;
                let edid = self.desired_edid(size)?;
                let snapshot = self
                    .nvapi_snapshot
                    .as_ref()
                    .ok_or_else(|| "NVAPI exact apply lost its snapshot".to_string())?;
                let journal_path = self.journal_path.clone();
                let driver = self
                    .nvapi
                    .as_mut()
                    .ok_or_else(|| "NVAPI exact apply lost its driver".to_string())?;
                match nvapi::apply_exact(
                    driver,
                    snapshot,
                    &edid,
                    size.width,
                    size.height,
                    self.request.refresh_hz.max(1),
                    |active| crate::recovery::mark_nvapi_ownership(&journal_path, active),
                ) {
                    Ok(active) => {
                        let save_error = active.save_error.clone();
                        self.nvapi_active = Some(active);
                        if let Some(error) = save_error.as_deref() {
                            tracing::warn!(
                                target: DISPLAY,
                                device = %target.device_name,
                                display_id = format_args!("0x{:08x}", snapshot.mapping.display_id),
                                %error,
                                "NVAPI driver rejected persistent custom timing; continuing with temporary trial timing"
                            );
                        }
                    }
                    Err(error) => {
                        let topology_commit_failed = error.topology_commit_failed;
                        let nvapi_error = error.message;
                        self.nvapi_active = error.active;
                        self.dxgi.invalidate();
                        if topology_commit_failed && self.nvapi_active.is_some() {
                            let fallback = change_mode(&target.device_name, size, CDS_FULLSCREEN)
                                .and_then(|()| {
                                    self.dxgi.invalidate();
                                    wait_for_mode(&mut self.dxgi, target, size, MODE_SETTLE_TIMEOUT)
                                });
                            match fallback {
                                Ok(mode) => {
                                    tracing::warn!(
                                        target: DISPLAY,
                                        %size,
                                        %nvapi_error,
                                        "NVAPI topology commit rejected retarget; applied checkpointed custom timing through Windows display mode API"
                                    );
                                    return Ok(mode);
                                }
                                Err(fallback_error) => {
                                    self.nvapi_failed = true;
                                    return Err(format!(
                                        "{nvapi_error}; ChangeDisplaySettingsEx fallback failed: {fallback_error}"
                                    ));
                                }
                            }
                        }
                        self.nvapi_failed = true;
                        return Err(nvapi_error);
                    }
                }
                self.dxgi.invalidate();
                return wait_for_mode(&mut self.dxgi, target, size, MODE_SETTLE_TIMEOUT);
            }
            change_mode(&target.device_name, size, CDS_FULLSCREEN)?;
            self.dxgi.invalidate();
            wait_for_mode(&mut self.dxgi, target, size, MODE_SETTLE_TIMEOUT)
        }

        fn isolate_topology(&mut self, target: &DisplayTarget) -> Result<ModeState, String> {
            let size = current_mode(&mut self.dxgi, target)?.size;
            isolate_session_output(&mut self.dxgi, target, size)
        }

        fn restore(
            &mut self,
            target: &DisplayTarget,
            snapshot: &Self::Snapshot,
        ) -> Result<ModeState, String> {
            let mut errors = Vec::new();
            let mut nvapi_errors = Vec::new();
            // Nothing to revert unless an NVAPI mutation actually happened:
            // isolate-only sessions snapshot NVAPI state but never touch it.
            if self.nvapi_active.is_some() {
                if let (Some(driver), Some(nvapi_snapshot)) =
                    (self.nvapi.as_mut(), snapshot.nvapi.as_ref())
                {
                    let cleanup_stage = if self.recovery_armed {
                        crate::recovery::nvapi_cleanup_stage(&self.journal_path)
                    } else {
                        Ok(nvapi::CleanupStage::Pending)
                    };
                    match cleanup_stage {
                        Ok(cleanup_stage) => {
                            let recovery_armed = self.recovery_armed;
                            let journal_path = self.journal_path.clone();
                            if let Err(error) = nvapi::restore_exact_staged(
                                driver,
                                nvapi_snapshot,
                                self.nvapi_active.as_ref(),
                                cleanup_stage,
                                |stage| {
                                    if recovery_armed {
                                        crate::recovery::mark_nvapi_cleanup_stage(
                                            &journal_path,
                                            stage,
                                        )
                                    } else {
                                        Ok(())
                                    }
                                },
                            ) {
                                nvapi_errors.push(error);
                            } else {
                                self.nvapi_active = None;
                            }
                        }
                        Err(error) => nvapi_errors.push(format!(
                            "read NVAPI cleanup stage from recovery journal: {error}"
                        )),
                    }
                    self.nvapi_failed = true;
                }
            } else if snapshot.nvapi.is_some() {
                tracing::debug!(
                    target: DISPLAY,
                    device = %target.device_name,
                    "no NVAPI mutation to revert; skipping NVAPI restore stage"
                );
            }
            let flags = SET_DISPLAY_CONFIG_FLAGS(
                SDC_APPLY.0 | SDC_USE_SUPPLIED_DISPLAY_CONFIG.0 | SDC_VIRTUAL_MODE_AWARE.0,
            );
            // SAFETY: both slices contain initialized values returned by QueryDisplayConfig
            // and remain valid for the duration of this synchronous call.
            let topology_rc = unsafe {
                SetDisplayConfig(
                    Some(&snapshot.topology.paths),
                    Some(&snapshot.topology.modes),
                    flags,
                )
            };
            if topology_rc != 0 {
                errors.push(format!("SetDisplayConfig restore returned {topology_rc}"));
            }

            let device = wide(&target.device_name);
            // SAFETY: the device string is null-terminated and snapshot.mode is the exact,
            // initialized DEVMODE returned for this device before the transaction.
            let mode_rc = unsafe {
                ChangeDisplaySettingsExW(
                    PCWSTR(device.as_ptr()),
                    Some(&snapshot.mode),
                    HWND::default(),
                    CDS_TYPE(0),
                    None,
                )
            };
            if mode_rc != DISP_CHANGE_SUCCESSFUL {
                errors.push(format!(
                    "ChangeDisplaySettingsExW exact-mode restore returned {}",
                    display_change_name(mode_rc)
                ));
            }

            self.dxgi.invalidate();
            let restored = match wait_for_mode(
                &mut self.dxgi,
                target,
                snapshot.original.size,
                MODE_SETTLE_TIMEOUT,
            ) {
                Ok(restored) => restored,
                Err(error) => {
                    errors.push(error);
                    errors.extend(nvapi_errors);
                    return Err(errors.join("; "));
                }
            };
            if restored != snapshot.original {
                errors.push(format!(
                    "restored mode/config {restored:?} differs from exact original {:?}",
                    snapshot.original
                ));
            }
            if errors.is_empty() {
                if !nvapi_errors.is_empty() {
                    return Err(format!(
                        "mandatory NVAPI timing/EDID cleanup failed: {}",
                        nvapi_errors.join("; ")
                    ));
                }
                self.vmware_active = false;
                Ok(restored)
            } else {
                errors.extend(nvapi_errors);
                Err(errors.join("; "))
            }
        }

        fn applied_backend(&self, exact: bool) -> &'static str {
            if self.vmware_active {
                "vmware-tools-resolution-set"
            } else if exact
                && self
                    .nvapi_active
                    .as_ref()
                    .is_some_and(|active| active.ownership == nvapi::TimingOwnership::SavedByUs)
            {
                "nvidia-nvapi-edid-saved-custom-timing"
            } else if exact && self.nvapi_active.is_some() {
                "nvidia-nvapi-edid-trial-custom-timing"
            } else if exact {
                "change-display-settings-ex-temporary"
            } else {
                "change-display-settings-ex-temporary-fallback"
            }
        }

        fn restore_backend(&self) -> &'static str {
            if self.nvapi_snapshot.is_some() {
                "nvapi-purge-plus-set-display-config-exact"
            } else {
                "set-display-config-plus-exact-devmode"
            }
        }

        fn arm_recovery(
            &mut self,
            target: &DisplayTarget,
            snapshot: &Self::Snapshot,
        ) -> Result<(), String> {
            if self.recovery_armed {
                if self.journal_path.exists() {
                    return Ok(());
                }
                return Err(format!(
                    "display recovery was armed but journal {:?} disappeared",
                    self.journal_path
                ));
            }
            if self.journal_path.exists() {
                return Err(format!(
                    "refusing display mutation while recovery journal {:?} exists",
                    self.journal_path
                ));
            }
            let nvapi = snapshot.nvapi.as_ref().map(|nvapi_snapshot| {
                nvapi::recovery_data(
                    target.device_name.clone(),
                    nvapi_snapshot,
                    self.request.size.width,
                    self.request.size.height,
                    self.request.refresh_hz.max(1),
                )
            });
            let stable_topology = capture_stable_topology(&snapshot.topology)?;
            let selected_path_index = snapshot
                .topology
                .paths
                .iter()
                .enumerate()
                .filter_map(|(index, path)| {
                    source_gdi_name(path)
                        .ok()
                        .filter(|name| name.eq_ignore_ascii_case(&target.device_name))
                        .map(|_| index)
                })
                .collect::<Vec<_>>();
            let [selected_path_index] = selected_path_index.as_slice() else {
                return Err(
                    "selected display does not map uniquely to the captured stable topology"
                        .to_string(),
                );
            };
            let journal = crate::recovery::DisplayRecoveryJournal::new(
                target.device_name.clone(),
                snapshot.original.size.width,
                snapshot.original.size.height,
                snapshot.original.refresh_hz,
                as_bytes(&snapshot.topology.paths),
                as_bytes(&snapshot.topology.modes),
                value_bytes(&snapshot.mode),
                nvapi,
            )
            .with_deskside(self.deskside.clone())
            .with_stable_topology(stable_topology)
            .with_selected_path_index(*selected_path_index);
            crate::recovery::write_atomic(&self.journal_path, &journal)?;
            if let Err(error) = spawn_recovery_watchdog(
                &self.journal_path,
                &self.session_log_id,
                crate::recovery::WatchdogResource::Display,
            ) {
                let remove = crate::recovery::remove(&self.journal_path);
                return Err(match remove {
                    Ok(()) => error,
                    Err(remove_error) => {
                        format!("{error}; failed to remove unarmed journal: {remove_error}")
                    }
                });
            }
            self.recovery_armed = true;
            tracing::info!(
                target: DISPLAY,
                journal = %self.journal_path.display(),
                device = %target.device_name,
                original = %snapshot.original.size,
                nvapi = snapshot.nvapi.is_some(),
                "persistent display recovery armed before mutation"
            );
            Ok(())
        }

        fn disarm_recovery(&mut self) -> Result<(), String> {
            crate::recovery::remove(&self.journal_path)?;
            self.recovery_armed = false;
            tracing::info!(
                target: DISPLAY,
                journal = %self.journal_path.display(),
                "persistent display recovery disarmed after verified restore"
            );
            Ok(())
        }

        fn mark_mutation_started(&mut self) -> Result<(), String> {
            crate::recovery::mark_mutation_started(&self.journal_path)
        }
    }

    pub(super) fn as_bytes<T>(values: &[T]) -> &[u8] {
        // SAFETY: this borrows the initialized object representation without
        // changing lifetime or alignment; the bytes are copied into JSON before
        // the source slice can be mutated or dropped.
        unsafe {
            std::slice::from_raw_parts(values.as_ptr().cast::<u8>(), std::mem::size_of_val(values))
        }
    }

    fn value_bytes<T>(value: &T) -> &[u8] {
        as_bytes(std::slice::from_ref(value))
    }

    fn same_luid(
        left: windows::Win32::Foundation::LUID,
        right: windows::Win32::Foundation::LUID,
    ) -> bool {
        left.LowPart == right.LowPart && left.HighPart == right.HighPart
    }

    fn normalized_topology_paths(
        paths: &[DISPLAYCONFIG_PATH_INFO],
        modes: &[DISPLAYCONFIG_MODE_INFO],
    ) -> Result<Vec<Vec<u8>>, String> {
        let mut normalized = Vec::with_capacity(paths.len());
        for path in paths {
            let source_mode = modes
                .iter()
                .find(|mode| {
                    mode.infoType == DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE
                        && mode.id == path.sourceInfo.id
                        && same_luid(mode.adapterId, path.sourceInfo.adapterId)
                })
                .ok_or_else(|| {
                    format!(
                        "active source {} has no matching source mode",
                        path.sourceInfo.id
                    )
                })?;
            let target_mode = modes
                .iter()
                .find(|mode| {
                    mode.infoType
                        == windows::Win32::Devices::Display::DISPLAYCONFIG_MODE_INFO_TYPE_TARGET
                        && mode.id == path.targetInfo.id
                        && same_luid(mode.adapterId, path.targetInfo.adapterId)
                })
                .ok_or_else(|| {
                    format!(
                        "active target {} has no matching target mode",
                        path.targetInfo.id
                    )
                })?;

            // SAFETY: paths came from QDC_VIRTUAL_MODE_AWARE, so both anonymous
            // unions carry their documented packed bitfields.
            let source_bits = unsafe { path.sourceInfo.Anonymous.Anonymous._bitfield };
            let target_bits = unsafe { path.targetInfo.Anonymous.Anonymous._bitfield };
            let clone_group_id = source_bits & 0xFFFF;
            let desktop_mode_index = target_bits & 0xFFFF;
            let desktop_mode = if desktop_mode_index == 0xFFFF {
                None
            } else {
                let mode = modes.get(desktop_mode_index as usize).ok_or_else(|| {
                    format!(
                        "desktop image mode index {desktop_mode_index} is outside {} modes",
                        modes.len()
                    )
                })?;
                if mode.infoType
                    != windows::Win32::Devices::Display::DISPLAYCONFIG_MODE_INFO_TYPE_DESKTOP_IMAGE
                {
                    return Err(format!(
                        "mode {desktop_mode_index} is not a desktop image mode"
                    ));
                }
                Some(*mode)
            };

            let mut path = *path;
            path.sourceInfo.adapterId = Default::default();
            path.sourceInfo.id = 0;
            path.sourceInfo.Anonymous.modeInfoIdx = clone_group_id;
            path.targetInfo.adapterId = Default::default();
            path.targetInfo.id = 0;
            path.targetInfo.Anonymous.modeInfoIdx = 0;

            let mut source_mode = *source_mode;
            source_mode.adapterId = Default::default();
            source_mode.id = 0;
            let mut target_mode = *target_mode;
            target_mode.adapterId = Default::default();
            target_mode.id = 0;
            let desktop_mode = desktop_mode.map(|mut mode| {
                mode.adapterId = Default::default();
                mode.id = 0;
                mode
            });

            let mut evidence = Vec::new();
            evidence.extend_from_slice(value_bytes(&path));
            evidence.extend_from_slice(value_bytes(&source_mode));
            evidence.extend_from_slice(value_bytes(&target_mode));
            if let Some(desktop_mode) = desktop_mode {
                evidence.push(1);
                evidence.extend_from_slice(value_bytes(&desktop_mode));
            } else {
                evidence.push(0);
            }
            normalized.push(evidence);
        }
        Ok(normalized)
    }

    fn normalized_topology(
        paths: &[DISPLAYCONFIG_PATH_INFO],
        modes: &[DISPLAYCONFIG_MODE_INFO],
    ) -> Result<Vec<Vec<u8>>, String> {
        let mut normalized = normalized_topology_paths(paths, modes)?;
        normalized.sort();
        Ok(normalized)
    }

    pub(super) fn complete_topology_semantically_matches(
        expected_paths: &[DISPLAYCONFIG_PATH_INFO],
        expected_modes: &[DISPLAYCONFIG_MODE_INFO],
        actual_paths: &[DISPLAYCONFIG_PATH_INFO],
        actual_modes: &[DISPLAYCONFIG_MODE_INFO],
    ) -> Result<bool, String> {
        Ok(normalized_topology(expected_paths, expected_modes)?
            == normalized_topology(actual_paths, actual_modes)?)
    }

    pub(super) fn complete_topology_with_stable_identities(
        expected_paths: &[DISPLAYCONFIG_PATH_INFO],
        expected_modes: &[DISPLAYCONFIG_MODE_INFO],
        expected_identities: &crate::recovery::StableTopologySnapshot,
        actual_paths: &[DISPLAYCONFIG_PATH_INFO],
        actual_modes: &[DISPLAYCONFIG_MODE_INFO],
        actual_identities: &crate::recovery::StableTopologySnapshot,
    ) -> Result<bool, String> {
        if expected_paths.len() != expected_identities.paths.len()
            || actual_paths.len() != actual_identities.paths.len()
        {
            return Err(
                "stable output identity count does not match active topology path count"
                    .to_string(),
            );
        }
        let expected = normalized_topology_paths(expected_paths, expected_modes)?;
        let actual = normalized_topology_paths(actual_paths, actual_modes)?;
        let mut expected = expected_identities
            .paths
            .iter()
            .cloned()
            .zip(expected)
            .collect::<Vec<_>>();
        let mut actual = actual_identities
            .paths
            .iter()
            .cloned()
            .zip(actual)
            .collect::<Vec<_>>();
        expected.sort();
        actual.sort();
        Ok(expected == actual)
    }

    fn mode_identity_index(
        modes: &[DISPLAYCONFIG_MODE_INFO],
        expected_adapter: windows::Win32::Foundation::LUID,
        expected_id: u32,
        info_type: windows::Win32::Devices::Display::DISPLAYCONFIG_MODE_INFO_TYPE,
    ) -> Result<usize, String> {
        let mut matches = modes.iter().enumerate().filter(|(_, mode)| {
            mode.infoType == info_type
                && mode.id == expected_id
                && same_luid(mode.adapterId, expected_adapter)
        });
        let (index, _) = matches
            .next()
            .ok_or_else(|| format!("journal topology mode {expected_id} is absent"))?;
        if matches.next().is_some() {
            return Err(format!("journal topology mode {expected_id} is ambiguous"));
        }
        Ok(index)
    }

    pub(super) fn rebind_topology_boot_identifiers(
        expected_paths: &[DISPLAYCONFIG_PATH_INFO],
        expected_modes: &[DISPLAYCONFIG_MODE_INFO],
        current_paths: &[DISPLAYCONFIG_PATH_INFO],
        current_path_indexes: &[usize],
    ) -> Result<(Vec<DISPLAYCONFIG_PATH_INFO>, Vec<DISPLAYCONFIG_MODE_INFO>), String> {
        if expected_paths.len() != current_path_indexes.len() {
            return Err("stable topology path mapping count is incomplete".to_string());
        }
        let mut paths = expected_paths.to_vec();
        let mut modes = expected_modes.to_vec();
        for (index, path) in paths.iter_mut().enumerate() {
            let current = current_paths
                .get(current_path_indexes[index])
                .ok_or_else(|| "stable topology path mapping is outside inventory".to_string())?;
            let expected_source_adapter = path.sourceInfo.adapterId;
            let expected_source_id = path.sourceInfo.id;
            let expected_target_adapter = path.targetInfo.adapterId;
            let expected_target_id = path.targetInfo.id;
            let source_mode_index = mode_identity_index(
                expected_modes,
                expected_source_adapter,
                expected_source_id,
                DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE,
            )?;
            let target_mode_index = mode_identity_index(
                expected_modes,
                expected_target_adapter,
                expected_target_id,
                windows::Win32::Devices::Display::DISPLAYCONFIG_MODE_INFO_TYPE_TARGET,
            )?;
            let source_mode = &mut modes[source_mode_index];
            if (!same_luid(source_mode.adapterId, expected_source_adapter)
                || source_mode.id != expected_source_id)
                && (!same_luid(source_mode.adapterId, current.sourceInfo.adapterId)
                    || source_mode.id != current.sourceInfo.id)
            {
                return Err("clone source maps inconsistently in current topology".to_string());
            }
            source_mode.adapterId = current.sourceInfo.adapterId;
            source_mode.id = current.sourceInfo.id;
            modes[target_mode_index].adapterId = current.targetInfo.adapterId;
            modes[target_mode_index].id = current.targetInfo.id;
            // SAFETY: both paths were queried with QDC_VIRTUAL_MODE_AWARE.
            let target_bits = unsafe { path.targetInfo.Anonymous.Anonymous._bitfield };
            let desktop_index = (target_bits & 0xFFFF) as usize;
            if desktop_index != 0xFFFF {
                let desktop = modes.get_mut(desktop_index).ok_or_else(|| {
                    format!("journal desktop-image mode index {desktop_index} is outside modes")
                })?;
                if desktop.infoType
                    != windows::Win32::Devices::Display::DISPLAYCONFIG_MODE_INFO_TYPE_DESKTOP_IMAGE
                {
                    return Err(format!(
                        "journal mode {desktop_index} is not a desktop-image mode"
                    ));
                }
                desktop.adapterId = current.targetInfo.adapterId;
                desktop.id = current.targetInfo.id;
            }
            path.sourceInfo.adapterId = current.sourceInfo.adapterId;
            path.sourceInfo.id = current.sourceInfo.id;
            path.targetInfo.adapterId = current.targetInfo.adapterId;
            path.targetInfo.id = current.targetInfo.id;
        }
        if !complete_topology_semantically_matches(expected_paths, expected_modes, &paths, &modes)?
        {
            return Err("boot-identifier reconstruction changed topology semantics".to_string());
        }
        Ok((paths, modes))
    }

    pub(super) fn reconstruct_current_topology(
        expected_paths: &[DISPLAYCONFIG_PATH_INFO],
        expected_modes: &[DISPLAYCONFIG_MODE_INFO],
        stable: &crate::recovery::StableTopologySnapshot,
        selected_path_index: usize,
    ) -> Result<
        (
            Vec<DISPLAYCONFIG_PATH_INFO>,
            Vec<DISPLAYCONFIG_MODE_INFO>,
            String,
        ),
        String,
    > {
        if expected_paths.len() != stable.paths.len() || selected_path_index >= expected_paths.len()
        {
            return Err("stable recovery authority is incomplete".to_string());
        }
        let needs_nvapi = stable.paths.iter().any(|identity| {
            matches!(
                identity.binding,
                crate::recovery::StableOutputBackend::Nvidia { .. }
            )
        });
        let mut nvapi_driver = needs_nvapi
            .then(nvapi::Nvapi::load)
            .transpose()
            .map_err(|error| format!("load NVAPI for stable topology reconstruction: {error}"))?;
        let current = query_all_topology()?;
        let inventory = crate::gpu_probe::probe()
            .map_err(|error| format!("inventory Windows-native recovery outputs: {error}"))?;
        if let Some(error) = inventory.topology_error {
            return Err(format!(
                "Windows-native recovery output inventory failed: {error}"
            ));
        }
        let mut used = std::collections::BTreeSet::new();
        let mut mappings = Vec::with_capacity(stable.paths.len());
        for (expected_index, expected) in stable.paths.iter().enumerate() {
            let _ = legacy_source_evidence(&expected_paths[expected_index], expected_modes)?;
            let mut candidates = Vec::new();
            for (path_index, path) in current.paths.iter().enumerate() {
                let Ok(monitor_path) = target_monitor_device_path(path) else {
                    continue;
                };
                for adapter in &inventory.adapters {
                    for output in &adapter.outputs {
                        if output.monitor_device_path.as_deref() != Some(monitor_path.as_str()) {
                            continue;
                        }
                        let current_mapping = match expected.binding {
                            crate::recovery::StableOutputBackend::WindowsNative => {
                                if adapter.vendor_id == 0x10de {
                                    continue;
                                }
                                None
                            }
                            crate::recovery::StableOutputBackend::Nvidia { .. } => {
                                if adapter.vendor_id != 0x10de {
                                    continue;
                                }
                                let mut dxgi = SessionDxgiEnumerator::default();
                                let dxgi_output = dxgi.output_by_device(&output.device_name)?;
                                let Some(driver) = nvapi_driver.as_mut() else {
                                    return Err(
                                        "NVIDIA topology reconstruction lacks NVAPI driver"
                                            .to_string(),
                                    );
                                };
                                match driver
                                    .map_display(&output.device_name, dxgi_output.adapter_luid)
                                {
                                    Ok(mapping) => Some(mapping),
                                    Err(_) => continue,
                                }
                            }
                        };
                        let identity = stable_output_identity(adapter, output, current_mapping)?;
                        if expected.immutable_binding_matches(&identity)
                            && path.flags & PATH_ACTIVE_FLAG != 0
                        {
                            candidates.push(path_index);
                        }
                    }
                }
            }
            candidates.sort_unstable_by_key(|index| {
                let path = &current.paths[*index];
                (
                    path.sourceInfo.adapterId.HighPart,
                    path.sourceInfo.adapterId.LowPart,
                    path.sourceInfo.id,
                    path.targetInfo.adapterId.HighPart,
                    path.targetInfo.adapterId.LowPart,
                    path.targetInfo.id,
                )
            });
            candidates.dedup_by_key(|index| {
                let path = &current.paths[*index];
                (
                    path.sourceInfo.adapterId.HighPart,
                    path.sourceInfo.adapterId.LowPart,
                    path.sourceInfo.id,
                    path.targetInfo.adapterId.HighPart,
                    path.targetInfo.adapterId.LowPart,
                    path.targetInfo.id,
                )
            });
            let [path_index] = candidates.as_slice() else {
                return Err(format!(
                    "stable output is absent or ambiguous in current all-path inventory \
                     (backend {:?}, {} distinct boot bindings)",
                    expected.binding,
                    candidates.len(),
                ));
            };
            if !used.insert(*path_index) {
                return Err("stable outputs map to the same current path".to_string());
            }
            mappings.push(*path_index);
        }
        let selected_device = source_gdi_name(&current.paths[mappings[selected_path_index]])?;
        let (paths, modes) = rebind_topology_boot_identifiers(
            expected_paths,
            expected_modes,
            &current.paths,
            &mappings,
        )?;
        Ok((paths, modes, selected_device))
    }

    fn reconstruct_current_topology_with_settle(
        expected_paths: &[DISPLAYCONFIG_PATH_INFO],
        expected_modes: &[DISPLAYCONFIG_MODE_INFO],
        stable: &crate::recovery::StableTopologySnapshot,
        selected_path_index: usize,
    ) -> Result<
        (
            Vec<DISPLAYCONFIG_PATH_INFO>,
            Vec<DISPLAYCONFIG_MODE_INFO>,
            String,
        ),
        String,
    > {
        let deadline = Instant::now() + MODE_SETTLE_TIMEOUT;
        loop {
            match reconstruct_current_topology(
                expected_paths,
                expected_modes,
                stable,
                selected_path_index,
            ) {
                Ok(reconstructed) => return Ok(reconstructed),
                Err(error) if Instant::now() < deadline => {
                    let _ = error;
                    std::thread::sleep(MODE_SETTLE_POLL);
                }
                Err(error) => return Err(error),
            }
        }
    }

    fn reconstruct_headless_original_geometry(
        expected_paths: &[DISPLAYCONFIG_PATH_INFO],
        expected_modes: &[DISPLAYCONFIG_MODE_INFO],
        stable: &crate::recovery::StableTopologySnapshot,
        selected_path_index: usize,
    ) -> Result<
        (
            Vec<DISPLAYCONFIG_PATH_INFO>,
            Vec<DISPLAYCONFIG_MODE_INFO>,
            String,
        ),
        String,
    > {
        if expected_paths.len() != stable.paths.len() || selected_path_index >= stable.paths.len() {
            return Err("headless recovery authority is incomplete".to_string());
        }
        let mut current = query_active_topology()?;
        let inventory = crate::gpu_probe::probe()
            .map_err(|error| format!("inventory headless recovery outputs: {error}"))?;
        if let Some(error) = inventory.topology_error {
            return Err(format!(
                "headless recovery output inventory failed: {error}"
            ));
        }
        let mut nvapi_driver = stable
            .paths
            .iter()
            .any(|identity| {
                matches!(
                    identity.binding,
                    crate::recovery::StableOutputBackend::Nvidia { .. }
                )
            })
            .then(nvapi::Nvapi::load)
            .transpose()
            .map_err(|error| format!("load NVAPI for headless recovery: {error}"))?;
        let mut selected_device = None;
        let mut used = std::collections::BTreeSet::new();
        for (expected_index, expected) in stable.paths.iter().enumerate() {
            let mut matches = Vec::new();
            for (path_index, path) in current.paths.iter().enumerate() {
                let Ok(monitor_path) = target_monitor_device_path(path) else {
                    continue;
                };
                for adapter in &inventory.adapters {
                    for output in &adapter.outputs {
                        if output.monitor_device_path.as_deref() != Some(monitor_path.as_str()) {
                            continue;
                        }
                        let current_mapping = match expected.binding {
                            crate::recovery::StableOutputBackend::WindowsNative => {
                                if adapter.vendor_id == 0x10de {
                                    continue;
                                }
                                None
                            }
                            crate::recovery::StableOutputBackend::Nvidia { .. } => {
                                if adapter.vendor_id != 0x10de {
                                    continue;
                                }
                                let mut dxgi = SessionDxgiEnumerator::default();
                                let dxgi_output = dxgi.output_by_device(&output.device_name)?;
                                let Some(driver) = nvapi_driver.as_mut() else {
                                    return Err(
                                        "headless recovery lacks its NVAPI driver".to_string()
                                    );
                                };
                                match driver
                                    .map_display(&output.device_name, dxgi_output.adapter_luid)
                                {
                                    Ok(mapping) => Some(mapping),
                                    Err(_) => continue,
                                }
                            }
                        };
                        let identity = stable_output_identity(adapter, output, current_mapping)?;
                        if expected.immutable_binding_matches(&identity) {
                            matches.push((path_index, output.device_name.clone()));
                        }
                    }
                }
            }
            matches.sort_by_key(|(path_index, _)| *path_index);
            matches.dedup_by_key(|(path_index, _)| *path_index);
            let [(path_index, device_name)] = matches.as_slice() else {
                return Err(format!(
                    "headless recovery identity {:?} resolved to {} active paths",
                    expected.binding,
                    matches.len()
                ));
            };
            if !used.insert(*path_index) {
                return Err("headless recovery mapped two outputs to one path".to_string());
            }
            let expected_geometry =
                legacy_source_evidence(&expected_paths[expected_index], expected_modes)?;
            // SAFETY: paths came from QDC_VIRTUAL_MODE_AWARE, so the anonymous
            // union carries the documented clone-group/source-mode bitfield.
            let source_index = virtual_source_mode_index(unsafe {
                current.paths[*path_index]
                    .sourceInfo
                    .Anonymous
                    .Anonymous
                    ._bitfield
            })
            .ok_or_else(|| "headless recovery path lacks a source mode".to_string())?;
            let source = current
                .modes
                .get_mut(source_index)
                .ok_or_else(|| "headless recovery source mode index is invalid".to_string())?;
            if source.infoType != DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE {
                return Err("headless recovery mode is not a source mode".to_string());
            }
            source.Anonymous.sourceMode.width = expected_geometry.width;
            source.Anonymous.sourceMode.height = expected_geometry.height;
            source.Anonymous.sourceMode.position = POINTL {
                x: expected_geometry.x,
                y: expected_geometry.y,
            };
            current.paths[*path_index].targetInfo.rotation =
                expected_paths[expected_index].targetInfo.rotation;
            if expected_index == selected_path_index {
                selected_device = Some(device_name.clone());
            }
        }
        Ok((
            current.paths,
            current.modes,
            selected_device.ok_or_else(|| "headless recovery lost selected output".to_string())?,
        ))
    }

    fn verify_headless_original_geometry(
        expected_paths: &[DISPLAYCONFIG_PATH_INFO],
        expected_modes: &[DISPLAYCONFIG_MODE_INFO],
        stable: &crate::recovery::StableTopologySnapshot,
    ) -> Result<(), String> {
        let current = query_active_topology()?;
        if current.paths.len() != expected_paths.len() || expected_paths.len() != stable.paths.len()
        {
            return Err(format!(
                "headless restore has {} active paths; expected {}",
                current.paths.len(),
                expected_paths.len()
            ));
        }
        let inventory = crate::gpu_probe::probe()
            .map_err(|error| format!("inventory restored headless outputs: {error}"))?;
        if let Some(error) = inventory.topology_error {
            return Err(format!(
                "restored headless output inventory failed: {error}"
            ));
        }
        let mut nvapi_driver = stable
            .paths
            .iter()
            .any(|identity| {
                matches!(
                    identity.binding,
                    crate::recovery::StableOutputBackend::Nvidia { .. }
                )
            })
            .then(nvapi::Nvapi::load)
            .transpose()
            .map_err(|error| format!("load NVAPI for headless verification: {error}"))?;
        let mut used = std::collections::BTreeSet::new();
        for (expected_index, expected) in stable.paths.iter().enumerate() {
            let mut matches = Vec::new();
            for (path_index, path) in current.paths.iter().enumerate() {
                let Ok(monitor_path) = target_monitor_device_path(path) else {
                    continue;
                };
                for adapter in &inventory.adapters {
                    for output in &adapter.outputs {
                        if output.monitor_device_path.as_deref() != Some(monitor_path.as_str()) {
                            continue;
                        }
                        let current_mapping = match expected.binding {
                            crate::recovery::StableOutputBackend::WindowsNative => {
                                if adapter.vendor_id == 0x10de {
                                    continue;
                                }
                                None
                            }
                            crate::recovery::StableOutputBackend::Nvidia { .. } => {
                                if adapter.vendor_id != 0x10de {
                                    continue;
                                }
                                let mut dxgi = SessionDxgiEnumerator::default();
                                let dxgi_output = dxgi.output_by_device(&output.device_name)?;
                                let Some(driver) = nvapi_driver.as_mut() else {
                                    return Err(
                                        "headless verification lacks its NVAPI driver".to_string()
                                    );
                                };
                                match driver
                                    .map_display(&output.device_name, dxgi_output.adapter_luid)
                                {
                                    Ok(mapping) => Some(mapping),
                                    Err(_) => continue,
                                }
                            }
                        };
                        let identity = stable_output_identity(adapter, output, current_mapping)?;
                        if expected.immutable_binding_matches(&identity) {
                            matches.push(path_index);
                        }
                    }
                }
            }
            matches.sort_unstable();
            matches.dedup();
            let [path_index] = matches.as_slice() else {
                return Err(format!(
                    "restored headless identity {:?} resolved to {} active paths",
                    expected.binding,
                    matches.len()
                ));
            };
            if !used.insert(*path_index) {
                return Err("restored headless outputs share one active path".to_string());
            }
            let expected_geometry =
                legacy_source_evidence(&expected_paths[expected_index], expected_modes)?;
            let actual_geometry =
                legacy_source_evidence(&current.paths[*path_index], &current.modes)?;
            if (
                actual_geometry.width,
                actual_geometry.height,
                actual_geometry.x,
                actual_geometry.y,
            ) != (
                expected_geometry.width,
                expected_geometry.height,
                expected_geometry.x,
                expected_geometry.y,
            ) || current.paths[*path_index].targetInfo.rotation
                != expected_paths[expected_index].targetInfo.rotation
            {
                return Err(format!(
                    "restored headless output {:?} is {}x{}@{},{} rotation {:?}; expected \
                     {}x{}@{},{} rotation {:?}",
                    expected.binding,
                    actual_geometry.width,
                    actual_geometry.height,
                    actual_geometry.x,
                    actual_geometry.y,
                    current.paths[*path_index].targetInfo.rotation,
                    expected_geometry.width,
                    expected_geometry.height,
                    expected_geometry.x,
                    expected_geometry.y,
                    expected_paths[expected_index].targetInfo.rotation,
                ));
            }
        }
        Ok(())
    }

    fn decode_values<T: Default + Clone>(
        bytes: &[u8],
        maximum_count: usize,
        label: &str,
    ) -> Result<Vec<T>, String> {
        let element_size = std::mem::size_of::<T>();
        if element_size == 0 || bytes.len() % element_size != 0 {
            return Err(format!(
                "{label} byte length {} is not a multiple of ABI size {element_size}",
                bytes.len()
            ));
        }
        let count = bytes.len() / element_size;
        if count == 0 || count > maximum_count {
            return Err(format!(
                "{label} count {count} is outside 1..={maximum_count}"
            ));
        }
        let mut values = vec![T::default(); count];
        // SAFETY: values is fully initialized; its writable object representation
        // has exactly bytes.len() bytes. Windows display structs are C-compatible
        // integer/union POD values, and the restored bytes were captured from the
        // same executable ABI before mutation.
        unsafe {
            std::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                values.as_mut_ptr().cast::<u8>(),
                bytes.len(),
            );
        }
        Ok(values)
    }

    fn decode_value<T: Default + Clone>(bytes: &[u8], label: &str) -> Result<T, String> {
        let mut values = decode_values(bytes, 1, label)?;
        Ok(values.remove(0))
    }

    pub(super) fn spawn_recovery_watchdog(
        path: &std::path::Path,
        session_log_id: &arcen_telemetry::CorrelationId,
        resource: crate::recovery::WatchdogResource,
    ) -> Result<(), String> {
        #[cfg(debug_assertions)]
        let executable = std::env::var_os("ARCEN_DISPLAY_WATCHDOG_EXE")
            .map(std::path::PathBuf::from)
            .map(Ok)
            .unwrap_or_else(|| std::env::current_exe())
            .map_err(|error| format!("resolve display recovery watchdog executable: {error}"))?;
        #[cfg(not(debug_assertions))]
        let executable = std::env::current_exe()
            .map_err(|error| format!("resolve display recovery watchdog executable: {error}"))?;
        let executable = executable
            .to_str()
            .ok_or_else(|| "display watchdog executable path is not valid UTF-8".to_string())?;
        let journal = path
            .to_str()
            .ok_or_else(|| "display recovery journal path is not valid UTF-8".to_string())?;

        // SAFETY: GetCurrentProcess returns the current process pseudo-handle.
        let current_process = unsafe { GetCurrentProcess() };
        let mut inherited_parent = HANDLE::default();
        // SAFETY: source and target identify this process, and the output pointer is valid. The
        // duplicate is inheritable and grants only the access required for liveness waiting.
        unsafe {
            DuplicateHandle(
                current_process,
                current_process,
                current_process,
                &mut inherited_parent,
                PROCESS_QUERY_LIMITED_INFORMATION.0 | PROCESS_SYNCHRONIZE.0,
                true,
                DUPLICATE_HANDLE_OPTIONS(0),
            )
        }
        .map_err(|error| format!("duplicate parent process handle for watchdog: {error}"))?;
        let inherited_parent = OwnedHandle(inherited_parent);

        let security = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            bInheritHandle: true.into(),
            ..Default::default()
        };
        // SAFETY: security remains valid for the duration of CreateEventW.
        let ready_event = unsafe { CreateEventW(Some(&security), true, false, PCWSTR::null()) }
            .map(OwnedHandle)
            .map_err(|error| format!("create watchdog readiness event: {error}"))?;

        let mut attribute_bytes = 0usize;
        // SAFETY: the documented sizing call accepts a null list and writes the required size.
        let _ = unsafe {
            InitializeProcThreadAttributeList(
                LPPROC_THREAD_ATTRIBUTE_LIST(std::ptr::null_mut()),
                1,
                0,
                &mut attribute_bytes,
            )
        };
        if attribute_bytes == 0 {
            return Err(format!(
                "size watchdog process attribute list: {}",
                std::io::Error::last_os_error()
            ));
        }
        let attribute_words = attribute_bytes.div_ceil(std::mem::size_of::<usize>());
        let mut storage = vec![0usize; attribute_words];
        let raw_attribute_list =
            LPPROC_THREAD_ATTRIBUTE_LIST(storage.as_mut_ptr().cast::<std::ffi::c_void>());
        // SAFETY: storage has the size returned by the sizing call and remains alive until the
        // initialized list is deleted.
        unsafe {
            InitializeProcThreadAttributeList(raw_attribute_list, 1, 0, &mut attribute_bytes)
        }
        .map_err(|error| format!("initialize watchdog process attribute list: {error}"))?;
        let attribute_list = AttributeList {
            raw: raw_attribute_list,
            _storage: storage,
        };
        let inherited_handles = [inherited_parent.raw(), ready_event.raw()];
        // SAFETY: the list is initialized and inherited_handles remains alive through process
        // creation. This attribute restricts inheritance to exactly these two handles.
        unsafe {
            UpdateProcThreadAttribute(
                attribute_list.raw,
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                Some(inherited_handles.as_ptr().cast()),
                std::mem::size_of_val(&inherited_handles),
                None,
                None,
            )
        }
        .map_err(|error| format!("set watchdog inherited-handle list: {error}"))?;

        let command_line = format!(
            "{} restore-watchdog --resource {} --parent-handle {} --ready-handle {} --journal {} \
             --session-log-id {}",
            quote_windows_argument(executable),
            resource.as_arg(),
            inherited_parent.raw().0 as isize,
            ready_event.raw().0 as isize,
            quote_windows_argument(journal),
            quote_windows_argument(session_log_id.as_str()),
        );
        let executable_wide: Vec<u16> = executable.encode_utf16().chain([0]).collect();
        let startup = STARTUPINFOEXW {
            StartupInfo: windows::Win32::System::Threading::STARTUPINFOW {
                cb: std::mem::size_of::<STARTUPINFOEXW>() as u32,
                ..Default::default()
            },
            lpAttributeList: attribute_list.raw,
        };
        let create_watchdog = |flags| {
            let mut command_line_wide: Vec<u16> = command_line.encode_utf16().chain([0]).collect();
            let mut process_info = PROCESS_INFORMATION::default();
            // SAFETY: all pointers reference initialized storage that outlives the call. The
            // command line is mutable and NUL-terminated as required by CreateProcessW.
            unsafe {
                CreateProcessW(
                    PCWSTR(executable_wide.as_ptr()),
                    PWSTR(command_line_wide.as_mut_ptr()),
                    None,
                    None,
                    true,
                    flags,
                    None,
                    PCWSTR::null(),
                    &startup.StartupInfo,
                    &mut process_info,
                )
            }?;
            Ok::<_, windows::core::Error>(process_info)
        };
        let base_flags = EXTENDED_STARTUPINFO_PRESENT | CREATE_NO_WINDOW;
        let process_info = match create_watchdog(base_flags | CREATE_BREAKAWAY_FROM_JOB) {
            Ok(process_info) => process_info,
            Err(error) if error.code().0 as u32 == 0x8007_0005 => {
                tracing::warn!(
                    target: DISPLAY,
                    "watchdog could not break away from the parent job; using normal child process"
                );
                create_watchdog(base_flags)
                    .map_err(|error| format!("start display recovery watchdog: {error}"))?
            }
            Err(error) => return Err(format!("start display recovery watchdog: {error}")),
        };
        let watchdog_process = OwnedHandle(process_info.hProcess);
        let _watchdog_thread = OwnedHandle(process_info.hThread);

        // SAFETY: both handles are valid and remain owned through the wait.
        let readiness = unsafe {
            WaitForMultipleObjects(
                &[ready_event.raw(), watchdog_process.raw()],
                false,
                WATCHDOG_READY_TIMEOUT_MS,
            )
        };
        if readiness == WAIT_OBJECT_0
            // SAFETY: watchdog_process is a valid synchronizable process handle.
            && unsafe { WaitForSingleObject(watchdog_process.raw(), 0) } == WAIT_TIMEOUT
        {
            return Ok(());
        }

        // SAFETY: the process handle is valid. Termination prevents a late acknowledgment from
        // racing journal cleanup after the parent has refused to mutate the display.
        unsafe {
            let _ = TerminateProcess(watchdog_process.raw(), 1);
            let _ = WaitForSingleObject(watchdog_process.raw(), 1_000);
        }
        if readiness == WAIT_TIMEOUT {
            Err("display recovery watchdog readiness timed out".to_string())
        } else if readiness.0 == WAIT_OBJECT_0.0 + 1 {
            Err("display recovery watchdog exited before readiness".to_string())
        } else if readiness == WAIT_FAILED {
            Err(format!(
                "wait for display recovery watchdog readiness: {}",
                std::io::Error::last_os_error()
            ))
        } else {
            Err(format!(
                "unexpected display recovery watchdog readiness result {}",
                readiness.0
            ))
        }
    }

    fn quote_windows_argument(argument: &str) -> String {
        let mut quoted = String::from("\"");
        let mut backslashes = 0usize;
        for character in argument.chars() {
            if character == '\\' {
                backslashes += 1;
            } else if character == '"' {
                quoted.extend(std::iter::repeat('\\').take(backslashes * 2 + 1));
                quoted.push('"');
                backslashes = 0;
            } else {
                quoted.extend(std::iter::repeat('\\').take(backslashes));
                quoted.push(character);
                backslashes = 0;
            }
        }
        quoted.extend(std::iter::repeat('\\').take(backslashes * 2));
        quoted.push('"');
        quoted
    }

    pub(super) fn query_active_topology() -> Result<TopologySnapshot, String> {
        let flags = QUERY_DISPLAY_CONFIG_FLAGS(QDC_ONLY_ACTIVE_PATHS.0 | QDC_VIRTUAL_MODE_AWARE.0);
        query_topology(flags)
    }

    fn query_all_topology() -> Result<TopologySnapshot, String> {
        let flags = QUERY_DISPLAY_CONFIG_FLAGS(QDC_ALL_PATHS.0 | QDC_VIRTUAL_MODE_AWARE.0);
        query_topology(flags)
    }

    fn query_topology(flags: QUERY_DISPLAY_CONFIG_FLAGS) -> Result<TopologySnapshot, String> {
        for _ in 0..TOPOLOGY_QUERY_ATTEMPTS {
            let mut path_count = 0u32;
            let mut mode_count = 0u32;
            // SAFETY: valid writable count pointers are supplied.
            let sizes =
                unsafe { GetDisplayConfigBufferSizes(flags, &mut path_count, &mut mode_count) };
            win32_ok(sizes, "GetDisplayConfigBufferSizes")?;

            let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); path_count as usize];
            let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); mode_count as usize];
            // SAFETY: vectors are initialized and sized to the counts returned immediately
            // above; the API receives their writable storage and updated counts.
            let query = unsafe {
                QueryDisplayConfig(
                    flags,
                    &mut path_count,
                    paths.as_mut_ptr(),
                    &mut mode_count,
                    modes.as_mut_ptr(),
                    None,
                )
            };
            if query == ERROR_INSUFFICIENT_BUFFER {
                continue;
            }
            win32_ok(query, "QueryDisplayConfig")?;
            paths.truncate(path_count as usize);
            modes.truncate(mode_count as usize);
            return Ok(TopologySnapshot { paths, modes });
        }
        Err("display topology changed repeatedly while snapshotting".to_string())
    }

    fn stable_output_identity(
        adapter: &crate::gpu_probe::AdapterCapability,
        output: &crate::gpu_probe::OutputCapability,
        nvapi_mapping: Option<nvapi::DisplayMapping>,
    ) -> Result<crate::recovery::StableOutputIdentity, String> {
        Ok(crate::recovery::StableOutputIdentity {
            adapter_stable_id: adapter.stable_id.clone(),
            monitor_device_path: output.monitor_device_path.clone().ok_or_else(|| {
                format!(
                    "output {} lacks a stable monitor device path",
                    output.device_name
                )
            })?,
            adapter_output_index: output.adapter_output_index,
            output_technology: output
                .output_technology
                .ok_or_else(|| format!("output {} lacks output technology", output.device_name))?,
            connector_instance: output
                .connector_instance
                .ok_or_else(|| format!("output {} lacks connector identity", output.device_name))?,
            edid_manufacture_id: output.edid_manufacture_id.unwrap_or(0),
            edid_product_code_id: output.edid_product_code_id.unwrap_or(0),
            edid_sha256: output.deskside_edid_sha256.clone(),
            binding: match nvapi_mapping {
                Some(mapping) => crate::recovery::StableOutputBackend::Nvidia {
                    nvapi_display_id: mapping.display_id,
                    nvapi_output_id: mapping.output_id,
                    nvapi_head: mapping.head,
                },
                None => crate::recovery::StableOutputBackend::WindowsNative,
            },
        })
    }

    pub(super) fn capture_stable_topology(
        topology: &TopologySnapshot,
    ) -> Result<crate::recovery::StableTopologySnapshot, String> {
        let inventory = crate::gpu_probe::probe()
            .map_err(|error| format!("inventory stable display identities: {error}"))?;
        if let Some(error) = inventory.topology_error {
            return Err(format!(
                "inventory stable display identities reported topology failure: {error}"
            ));
        }
        let mut nvapi_driver = None;
        let mut identities = Vec::with_capacity(topology.paths.len());
        for path in &topology.paths {
            let device_name = source_gdi_name(path)?;
            let expected_monitor_path = target_monitor_device_path(path)?;
            let mut matches = inventory.adapters.iter().flat_map(|adapter| {
                adapter
                    .outputs
                    .iter()
                    .filter(|output| {
                        output.attached_to_desktop
                            && output.ccd_active == Some(true)
                            && output.monitor_device_path.as_deref()
                                == Some(expected_monitor_path.as_str())
                    })
                    .map(move |output| (adapter, output))
            });
            let Some((adapter, output)) = matches.next() else {
                return Err(format!(
                    "active path {device_name} has no stable output identity"
                ));
            };
            if matches.next().is_some() {
                return Err(format!(
                    "active path {device_name} has ambiguous stable output identities"
                ));
            }
            let monitor_device_path = output.monitor_device_path.clone().ok_or_else(|| {
                format!("active path {device_name} lacks a stable monitor device path")
            })?;
            let nvapi_mapping = if adapter.vendor_id == 0x10de {
                if nvapi_driver.is_none() {
                    nvapi_driver = Some(nvapi::Nvapi::load().map_err(|error| {
                        format!("load NVAPI for stable display identities: {error}")
                    })?);
                }
                let driver = nvapi_driver.as_mut().expect("NVAPI initialized above");
                let mapping = driver.map_display(
                    &device_name,
                    AdapterLuid {
                        low_part: path.sourceInfo.adapterId.LowPart,
                        high_part: path.sourceInfo.adapterId.HighPart,
                    },
                )?;
                Some(mapping)
            } else {
                None
            };
            let identity = stable_output_identity(adapter, output, nvapi_mapping)?;
            if identity.monitor_device_path != monitor_device_path {
                return Err(format!(
                    "active path {device_name} monitor identity changed during capture"
                ));
            }
            identities.push(identity);
        }
        Ok(crate::recovery::StableTopologySnapshot { paths: identities })
    }

    fn win32_ok(code: WIN32_ERROR, operation: &str) -> Result<(), String> {
        if code == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(format!("{operation} returned Win32 error {}", code.0))
        }
    }

    fn current_devmode(device_name: &str) -> Result<DEVMODEW, String> {
        let device = wide(device_name);
        let mut mode = DEVMODEW {
            dmSize: std::mem::size_of::<DEVMODEW>() as u16,
            ..DEVMODEW::default()
        };
        // SAFETY: the device string is null-terminated and mode is a valid writable DEVMODEW.
        let found = unsafe {
            EnumDisplaySettingsExW(
                PCWSTR(device.as_ptr()),
                ENUM_CURRENT_SETTINGS,
                &mut mode,
                ENUM_DISPLAY_SETTINGS_FLAGS(0),
            )
        };
        if found.as_bool() {
            Ok(mode)
        } else {
            Err(format!(
                "EnumDisplaySettingsExW current mode failed for {device_name}"
            ))
        }
    }

    fn supported_sizes(device_name: &str) -> Result<Vec<DisplaySize>, String> {
        let device = wide(device_name);
        let mut sizes = BTreeSet::new();
        for index in 0..MAX_ENUMERATED_MODES {
            let mut mode = DEVMODEW {
                dmSize: std::mem::size_of::<DEVMODEW>() as u16,
                ..DEVMODEW::default()
            };
            // SAFETY: the device string is null-terminated and mode is writable.
            let found = unsafe {
                EnumDisplaySettingsExW(
                    PCWSTR(device.as_ptr()),
                    ENUM_DISPLAY_SETTINGS_MODE(index),
                    &mut mode,
                    ENUM_DISPLAY_SETTINGS_FLAGS(0),
                )
            };
            if !found.as_bool() {
                break;
            }
            sizes.insert((mode.dmPelsWidth, mode.dmPelsHeight));
        }
        if sizes.is_empty() {
            return Err(format!("no display modes enumerated for {device_name}"));
        }
        Ok(sizes
            .into_iter()
            .map(|(width, height)| DisplaySize { width, height })
            .collect())
    }

    fn change_mode(device_name: &str, size: DisplaySize, flags: CDS_TYPE) -> Result<(), String> {
        let device = wide(device_name);
        let mode = DEVMODEW {
            dmSize: std::mem::size_of::<DEVMODEW>() as u16,
            dmFields: DEVMODE_FIELD_FLAGS(DM_PELSWIDTH.0 | DM_PELSHEIGHT.0),
            dmPelsWidth: size.width,
            dmPelsHeight: size.height,
            ..DEVMODEW::default()
        };
        // SAFETY: the device string is null-terminated and mode is initialized with the
        // required dmSize/dmFields members for a synchronous modeset.
        let result = unsafe {
            ChangeDisplaySettingsExW(
                PCWSTR(device.as_ptr()),
                Some(&mode),
                HWND::default(),
                flags,
                None,
            )
        };
        if result == DISP_CHANGE_SUCCESSFUL {
            Ok(())
        } else {
            Err(format!(
                "ChangeDisplaySettingsExW({size}, flags=0x{:x}) returned {}",
                flags.0,
                display_change_name(result)
            ))
        }
    }

    fn current_mode(
        dxgi: &mut SessionDxgiEnumerator,
        target: &DisplayTarget,
    ) -> Result<ModeState, String> {
        let mode = current_devmode(&target.device_name)?;
        mode_state(dxgi, target, &mode)
    }

    fn mode_state(
        dxgi: &mut SessionDxgiEnumerator,
        target: &DisplayTarget,
        mode: &DEVMODEW,
    ) -> Result<ModeState, String> {
        let outputs = dxgi.outputs()?;
        let active_outputs = outputs.len() as u32;
        let output = outputs
            .into_iter()
            .find(|output| output.device_name.eq_ignore_ascii_case(&target.device_name))
            .ok_or_else(|| {
                format!(
                    "DXGI device {} disappeared during display transaction",
                    target.device_name
                )
            })?;
        Ok(ModeState {
            size: DisplaySize {
                width: mode.dmPelsWidth,
                height: mode.dmPelsHeight,
            },
            refresh_hz: mode.dmDisplayFrequency,
            output_index: output.output_index,
            desktop_rect: output.desktop_rect,
            active_outputs,
        })
    }

    /// Extract the virtual-mode-aware source mode index (high 16 bits of the
    /// union bitfield; the low 16 bits are the clone group id).
    fn virtual_source_mode_index(bitfield: u32) -> Option<usize> {
        let index = bitfield >> 16;
        (index != 0xFFFF).then_some(index as usize)
    }

    fn source_gdi_name(path: &DISPLAYCONFIG_PATH_INFO) -> Result<String, String> {
        let mut request = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
                size: std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
                adapterId: path.sourceInfo.adapterId,
                id: path.sourceInfo.id,
            },
            ..Default::default()
        };
        // SAFETY: the request packet is initialized with the correct type/size
        // header and remains valid for this synchronous call.
        let rc = unsafe { DisplayConfigGetDeviceInfo(&mut request.header) };
        if rc != 0 {
            return Err(format!(
                "DisplayConfigGetDeviceInfo(source name) returned {rc} for source {}",
                path.sourceInfo.id
            ));
        }
        Ok(utf16(&request.viewGdiDeviceName))
    }

    fn target_monitor_device_path(path: &DISPLAYCONFIG_PATH_INFO) -> Result<String, String> {
        let mut request = DISPLAYCONFIG_TARGET_DEVICE_NAME {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
                size: std::mem::size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32,
                adapterId: path.targetInfo.adapterId,
                id: path.targetInfo.id,
            },
            ..Default::default()
        };
        // SAFETY: the request packet is initialized with the correct type/size
        // header and remains valid for this synchronous call.
        let rc = unsafe { DisplayConfigGetDeviceInfo(&mut request.header) };
        if rc != 0 {
            return Err(format!(
                "DisplayConfigGetDeviceInfo(target name) returned {rc} for target {}",
                path.targetInfo.id
            ));
        }
        let value = utf16(&request.monitorDevicePath);
        if value.is_empty() {
            return Err(format!(
                "active target {} has no stable monitor device path",
                path.targetInfo.id
            ));
        }
        Ok(value)
    }

    /// Deactivate every path except the session output's and move the session
    /// source to (0, 0) — the GDI primary position — so the host desktop is
    /// exactly the one client display. The change is intentionally NOT saved
    /// to the persistence database; restore reapplies the full snapshot.
    fn isolate_session_output(
        dxgi: &mut SessionDxgiEnumerator,
        target: &DisplayTarget,
        expected: DisplaySize,
    ) -> Result<ModeState, String> {
        // A fresh query, not the pre-mutation snapshot: the NVAPI custom-
        // timing apply has already changed the mode tables.
        let topology = query_active_topology()?;
        let mut paths = topology.paths;
        let mut modes = topology.modes;

        let mut session_indices = Vec::new();
        for (index, path) in paths.iter().enumerate() {
            if source_gdi_name(path)?.eq_ignore_ascii_case(&target.device_name) {
                session_indices.push(index);
            }
        }
        let [session_index] = session_indices.as_slice() else {
            return Err(format!(
                "topology isolation found {} active paths for {} instead of exactly one",
                session_indices.len(),
                target.device_name
            ));
        };
        let session_index = *session_index;

        let source_mode_index = {
            let session = &paths[session_index];
            // SAFETY: the paths were queried with QDC_VIRTUAL_MODE_AWARE, so
            // the union carries the bitfield encoding.
            virtual_source_mode_index(unsafe { session.sourceInfo.Anonymous.Anonymous._bitfield })
                .ok_or_else(|| {
                format!(
                    "active session path for {} has no source mode",
                    target.device_name
                )
            })?
        };
        let source_mode = modes.get_mut(source_mode_index).ok_or_else(|| {
            format!(
                "session source mode index {source_mode_index} is outside the {} queried modes",
                target.device_name
            )
        })?;
        if source_mode.infoType != DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE {
            return Err(format!(
                "session mode entry {source_mode_index} is not a source mode"
            ));
        }
        source_mode.Anonymous.sourceMode.position = POINTL { x: 0, y: 0 };

        for (index, path) in paths.iter_mut().enumerate() {
            if index == session_index {
                continue;
            }
            path.flags &= !PATH_ACTIVE_FLAG;
            path.sourceInfo.Anonymous.modeInfoIdx = PATH_MODE_INDICES_INVALID;
            path.targetInfo.Anonymous.modeInfoIdx = PATH_MODE_INDICES_INVALID;
        }

        let flags = SET_DISPLAY_CONFIG_FLAGS(
            SDC_APPLY.0
                | SDC_USE_SUPPLIED_DISPLAY_CONFIG.0
                | SDC_ALLOW_CHANGES.0
                | SDC_VIRTUAL_MODE_AWARE.0,
        );
        // SAFETY: both slices hold initialized values from QueryDisplayConfig,
        // mutated only through the checked accessors above, and remain valid
        // for this synchronous call.
        let rc = unsafe { SetDisplayConfig(Some(&paths), Some(&modes), flags) };
        if rc != 0 {
            return Err(format!(
                "SetDisplayConfig topology isolation returned {rc} for {}",
                target.device_name
            ));
        }
        dxgi.invalidate();
        wait_for_isolated(dxgi, target, expected, MODE_SETTLE_TIMEOUT)
    }

    fn wait_for_isolated(
        dxgi: &mut SessionDxgiEnumerator,
        target: &DisplayTarget,
        expected: DisplaySize,
        timeout: Duration,
    ) -> Result<ModeState, String> {
        let deadline = Instant::now() + timeout;
        loop {
            let last = match current_mode(dxgi, target) {
                Ok(mode) if mode.is_isolated_primary_at(expected) => return Ok(mode),
                Ok(mode) => format!(
                    "mode={} rect={:?} active_outputs={}",
                    mode.size, mode.desktop_rect, mode.active_outputs
                ),
                Err(error) => error,
            };
            if Instant::now() >= deadline {
                return Err(format!(
                    "topology did not settle isolated at {expected} within {}ms ({last})",
                    timeout.as_millis()
                ));
            }
            std::thread::sleep(MODE_SETTLE_POLL);
        }
    }

    fn wait_for_mode(
        dxgi: &mut SessionDxgiEnumerator,
        target: &DisplayTarget,
        expected: DisplaySize,
        timeout: Duration,
    ) -> Result<ModeState, String> {
        let deadline = Instant::now() + timeout;
        loop {
            let last = match current_mode(dxgi, target) {
                Ok(mode)
                    if mode.size == expected
                        && mode.desktop_rect.width == expected.width as i32
                        && mode.desktop_rect.height == expected.height as i32 =>
                {
                    return Ok(mode);
                }
                Ok(mode) => format!(
                    "mode={} rect={}x{} output={}",
                    mode.size, mode.desktop_rect.width, mode.desktop_rect.height, mode.output_index
                ),
                Err(error) => error,
            };
            if Instant::now() >= deadline {
                return Err(format!(
                    "display did not settle at {expected} within {}ms ({last})",
                    timeout.as_millis()
                ));
            }
            std::thread::sleep(MODE_SETTLE_POLL);
        }
    }

    fn wait_for_target_ready(
        dxgi: &mut SessionDxgiEnumerator,
        target: &DisplayTarget,
        timeout: Duration,
    ) -> Result<(), String> {
        let deadline = Instant::now() + timeout;
        let mut previous = None;
        loop {
            let last = match current_mode(dxgi, target) {
                Ok(current) if current.is_settled_at(current.size) => {
                    if previous.is_some_and(|sample: ModeState| {
                        sample.size == current.size
                            && sample.output_index == current.output_index
                            && sample.desktop_rect == current.desktop_rect
                    }) {
                        return Ok(());
                    }
                    previous = Some(current);
                    format!("waiting for a second stable sample at {}", current.size)
                }
                Ok(current) => {
                    previous = None;
                    format!(
                        "mode={} rect={}x{}",
                        current.size, current.desktop_rect.width, current.desktop_rect.height
                    )
                }
                Err(error) => {
                    previous = None;
                    error
                }
            };
            if Instant::now() >= deadline {
                return Err(format!(
                    "selected display {} was not ready within {}ms ({last})",
                    target.device_name,
                    timeout.as_millis()
                ));
            }
            std::thread::sleep(TARGET_READY_POLL);
        }
    }

    fn current_session_protocol_type() -> Result<u16, String> {
        let mut buffer = PWSTR::null();
        let mut bytes = 0u32;
        // SAFETY: output pointers are valid and WTS allocates the returned buffer.
        unsafe {
            WTSQuerySessionInformationW(
                WTS_CURRENT_SERVER_HANDLE,
                WTS_CURRENT_SESSION,
                WTSClientProtocolType,
                &mut buffer,
                &mut bytes,
            )
        }
        .map_err(|error| format!("query current WTS client protocol: {error}"))?;
        if buffer.is_null() || bytes < std::mem::size_of::<u16>() as u32 {
            if !buffer.is_null() {
                // SAFETY: WTS allocated the buffer and it is freed exactly once.
                unsafe { WTSFreeMemory(buffer.0.cast()) };
            }
            return Err("current WTS session did not report a client protocol type".to_string());
        }
        // SAFETY: the returned buffer contains at least one u16.
        let protocol = unsafe { *buffer.as_ptr().cast::<u16>() };
        // SAFETY: WTS allocated the buffer and it is freed exactly once.
        unsafe { WTSFreeMemory(buffer.0.cast()) };
        Ok(protocol)
    }

    pub(super) fn require_legacy_migration_context() -> Result<(), String> {
        if current_session_protocol_type()? != 0 {
            return Err(
                "legacy display-journal migration requires the local console protocol".to_string(),
            );
        }
        let mut session_id = 0_u32;
        unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session_id) }
            .map_err(|error| format!("resolve migration process session: {error}"))?;
        if session_id == 0 || session_id != unsafe { WTSGetActiveConsoleSessionId() } {
            return Err(
                "legacy display-journal migration requires the exact active console session"
                    .to_string(),
            );
        }

        let mut administrators = vec![0_u8; 128];
        let mut bytes = administrators.len() as u32;
        unsafe {
            CreateWellKnownSid(
                WinBuiltinAdministratorsSid,
                None,
                PSID(administrators.as_mut_ptr().cast()),
                &mut bytes,
            )
        }
        .map_err(|error| format!("construct Administrators SID: {error}"))?;
        let mut member = BOOL(0);
        unsafe {
            CheckTokenMembership(None, PSID(administrators.as_mut_ptr().cast()), &mut member)
        }
        .map_err(|error| format!("check elevated administrator token: {error}"))?;
        if !member.as_bool() {
            return Err(
                "legacy display-journal migration requires an elevated local administrator"
                    .to_string(),
            );
        }
        Ok(())
    }

    fn remote_session_display_error(protocol: u16) -> String {
        format!(
            "authenticated Windows session uses remote display protocol {protocol}; disconnect \
             RDP and connect Arcen Deck to the physical console session"
        )
    }

    fn vmware_resolution_tool() -> Option<std::path::PathBuf> {
        let path = std::path::PathBuf::from(std::env::var_os("ProgramFiles")?)
            .join("VMware")
            .join("VMware Tools")
            .join("VMwareResolutionSet.exe");
        path.is_file().then_some(path)
    }

    fn vmware_resolution_supported(
        dxgi: &mut SessionDxgiEnumerator,
        target: &DisplayTarget,
    ) -> bool {
        if target.adapter_output_index != 0 {
            return false;
        }
        dxgi.outputs().is_ok_and(|outputs| {
            outputs
                .iter()
                .filter(|output| output.adapter_luid == target.adapter_luid)
                .count()
                == 1
        })
    }

    fn apply_vmware_resolution(size: DisplaySize) -> Result<i32, String> {
        let path = vmware_resolution_tool()
            .ok_or_else(|| "VMwareResolutionSet.exe is not installed".to_string())?;
        let args = vmware_resolution_args(size);
        let mut child = std::process::Command::new(&path)
            .args(&args)
            .creation_flags(CREATE_NO_WINDOW.0)
            .spawn()
            .map_err(|error| format!("start {}: {error}", path.display()))?;
        let deadline = Instant::now() + VMWARE_RESOLUTION_TIMEOUT;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => return Ok(status.code().unwrap_or(-1)),
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(25));
                }
                Ok(None) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!(
                        "{} timed out applying {size} after {}ms",
                        path.display(),
                        VMWARE_RESOLUTION_TIMEOUT.as_millis()
                    ));
                }
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!(
                        "wait for {} applying {size}: {error}",
                        path.display()
                    ));
                }
            }
        }
    }

    fn vmware_resolution_args(size: DisplaySize) -> Vec<String> {
        vec![
            "0".to_string(),
            "1".to_string(),
            ",".to_string(),
            "0".to_string(),
            "0".to_string(),
            size.width.to_string(),
            size.height.to_string(),
        ]
    }

    struct DxgiOutput {
        device_name: String,
        output_index: u32,
        adapter_name: String,
        adapter_output_index: u32,
        desktop_rect: DesktopRect,
        vendor_id: u32,
        adapter_luid: AdapterLuid,
    }

    struct FactoryCache<F> {
        factory: Option<F>,
        creation_count: u32,
    }

    impl<F> Default for FactoryCache<F> {
        fn default() -> Self {
            Self {
                factory: None,
                creation_count: 0,
            }
        }
    }

    impl<F> FactoryCache<F> {
        fn get_or_try_init<E>(
            &mut self,
            is_current: impl FnOnce(&F) -> bool,
            create: impl FnOnce() -> Result<F, E>,
        ) -> Result<&F, E> {
            if self
                .factory
                .as_ref()
                .is_none_or(|factory| !is_current(factory))
            {
                self.factory = Some(create()?);
                self.creation_count += 1;
            }
            Ok(self.factory.as_ref().expect("factory initialized"))
        }

        fn invalidate(&mut self) {
            self.factory = None;
        }

        #[cfg(test)]
        fn creation_count(&self) -> u32 {
            self.creation_count
        }
    }

    /// One DXGI factory cache for the lifetime of an authenticated display
    /// lease. Readiness and settle polls reuse it while `IsCurrent` remains
    /// true; successful topology mutations explicitly invalidate it.
    #[derive(Default)]
    pub(super) struct SessionDxgiEnumerator {
        factory: FactoryCache<IDXGIFactory1>,
    }

    impl SessionDxgiEnumerator {
        fn outputs(&mut self) -> Result<Vec<DxgiOutput>, String> {
            let factory = self.factory.get_or_try_init(
                |factory| unsafe { factory.IsCurrent().as_bool() },
                || unsafe {
                    CreateDXGIFactory1().map_err(|error| format!("CreateDXGIFactory1: {error}"))
                },
            )?;
            enumerate_dxgi_outputs(factory)
        }

        fn output_by_device(&mut self, device_name: &str) -> Result<DxgiOutput, String> {
            self.outputs()?
                .into_iter()
                .find(|output| output.device_name.eq_ignore_ascii_case(device_name))
                .ok_or_else(|| {
                    format!("DXGI device {device_name} disappeared during display transaction")
                })
        }

        fn invalidate(&mut self) {
            self.factory.invalidate();
        }
    }

    pub(super) fn resolve_output_selector(
        selector: &super::OutputSelector,
    ) -> Result<super::ResolvedOutput, String> {
        let outputs = SessionDxgiEnumerator::default().outputs()?;
        resolve_output_from_outputs(selector, &outputs)
    }

    pub(super) fn enumerate_outputs() -> Result<Vec<super::ResolvedOutput>, String> {
        Ok(SessionDxgiEnumerator::default()
            .outputs()?
            .iter()
            .map(|output| super::ResolvedOutput {
                global_index: output.output_index,
                adapter_name: output.adapter_name.clone(),
                adapter_output_index: output.adapter_output_index,
                device_name: output.device_name.clone(),
                vendor_id: output.vendor_id,
                desktop_rect: output.desktop_rect,
            })
            .collect())
    }

    fn resolve_output_from_outputs(
        selector: &super::OutputSelector,
        outputs: &[DxgiOutput],
    ) -> Result<super::ResolvedOutput, String> {
        let selected = select_dxgi_output(selector, outputs)?;
        Ok(super::ResolvedOutput {
            global_index: selected.output_index,
            adapter_name: selected.adapter_name.clone(),
            adapter_output_index: selected.adapter_output_index,
            device_name: selected.device_name.clone(),
            vendor_id: selected.vendor_id,
            desktop_rect: selected.desktop_rect,
        })
    }

    fn select_dxgi_output<'a>(
        selector: &super::OutputSelector,
        outputs: &'a [DxgiOutput],
    ) -> Result<&'a DxgiOutput, String> {
        match selector {
            super::OutputSelector::GlobalIndex(index) => outputs
                .iter()
                .find(|output| output.output_index == *index)
                .ok_or_else(|| selection_error(selector, outputs)),
            super::OutputSelector::Adapter { name, output_index } => {
                let on_adapter = outputs
                    .iter()
                    .filter(|output| windows_eq_ignore_case(&output.adapter_name, name))
                    .collect::<Vec<_>>();
                let exact = on_adapter
                    .iter()
                    .filter(|output| output.adapter_output_index == *output_index)
                    .collect::<Vec<_>>();
                if exact.len() == 1 {
                    return Ok(exact[0]);
                }
                // The adapter-local index is resolved by the broker, which runs
                // as LocalSystem in session 0, and used by the session agent,
                // which runs in the interactive session. Windows does not
                // guarantee the two enumerate the same DXGI outputs: on a
                // multi-GPU host after a cold boot the broker saw two outputs on
                // the NVIDIA adapter and froze index 1, while the agent saw one
                // at index 0 and the session died with "could not uniquely
                // resolve adapter ... output 1". A first install therefore could
                // not stream at all until someone hand-edited pier.json.
                //
                // When the adapter has exactly one output here, that output is
                // unambiguous whatever it was numbered elsewhere, so use it. The
                // reason the positional selector was frozen in the first place —
                // stopping display mutation falling back to *global* output 0,
                // which may belong to a different GPU — still holds: this only
                // ever resolves within the adapter that was chosen.
                if on_adapter.len() == 1 {
                    tracing::warn!(
                        target: crate::logging::DISPLAY,
                        adapter = %name,
                        requested_output = *output_index,
                        resolved_output = on_adapter[0].adapter_output_index,
                        device = %on_adapter[0].device_name,
                        "adapter-local output index is not present in this session; \
                         using the adapter's only output"
                    );
                    return Ok(on_adapter[0]);
                }
                Err(selection_error(selector, outputs))
            }
        }
    }

    fn windows_eq_ignore_case(left: &str, right: &str) -> bool {
        let left = left.encode_utf16().collect::<Vec<_>>();
        let right = right.encode_utf16().collect::<Vec<_>>();
        // SAFETY: both UTF-16 slices remain live for the synchronous comparison.
        unsafe { CompareStringOrdinal(&left, &right, BOOL(1)) == CSTR_EQUAL }
    }

    fn selection_error(selector: &super::OutputSelector, outputs: &[DxgiOutput]) -> String {
        let requested = match selector {
            super::OutputSelector::GlobalIndex(index) => format!("global output {index}"),
            super::OutputSelector::Adapter { name, output_index } => {
                format!("adapter {name:?} output {output_index}")
            }
        };
        let available = outputs
            .iter()
            .map(|output| {
                format!(
                    "global={} adapter={:?} adapter_output={} device={} rect={}x{}@{},{}",
                    output.output_index,
                    output.adapter_name,
                    output.adapter_output_index,
                    output.device_name,
                    output.desktop_rect.width,
                    output.desktop_rect.height,
                    output.desktop_rect.left,
                    output.desktop_rect.top
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        format!("DXGI could not uniquely resolve {requested}; available outputs: [{available}]")
    }

    #[cfg(test)]
    mod selector_tests {
        use super::*;

        fn output(global: u32, adapter: &str, adapter_output: u32, device: &str) -> DxgiOutput {
            DxgiOutput {
                device_name: device.to_string(),
                output_index: global,
                adapter_name: adapter.to_string(),
                adapter_output_index: adapter_output,
                desktop_rect: DesktopRect {
                    left: 0,
                    top: 0,
                    width: 1920,
                    height: 1080,
                },
                vendor_id: 0x10de,
                adapter_luid: AdapterLuid::default(),
            }
        }

        #[test]
        fn a_single_adapter_output_resolves_even_if_the_index_differs() {
            // The broker resolves this index in session 0; the agent uses it in
            // the interactive session, which need not enumerate the same
            // outputs. A cold-booted multi-GPU host froze index 1 and then saw
            // only index 0, and the session died rather than streaming.
            let outputs = vec![
                output(0, "Microsoft Basic Render Driver", 0, r"\\.\DISPLAY1"),
                output(1, "NVIDIA GRID V100D-16Q", 0, r"\\.\DISPLAY3"),
            ];
            let selector = super::super::OutputSelector::Adapter {
                name: "NVIDIA GRID V100D-16Q".to_string(),
                output_index: 1,
            };
            let selected =
                resolve_output_from_outputs(&selector, &outputs).expect("single output resolves");
            assert_eq!(selected.device_name, r"\\.\DISPLAY3");
            assert_eq!(selected.adapter_name, "NVIDIA GRID V100D-16Q");
        }

        #[test]
        fn a_missing_index_never_falls_through_to_another_adapter() {
            // The guarantee the frozen selector exists to provide. Falling back
            // to the adapter's own only output is safe; falling back across
            // adapters would hand capture to the wrong GPU.
            let outputs = vec![
                output(0, "Microsoft Basic Render Driver", 0, r"\\.\DISPLAY1"),
                output(1, "NVIDIA GRID V100D-16Q", 0, r"\\.\DISPLAY3"),
                output(2, "NVIDIA GRID V100D-16Q", 1, r"\\.\DISPLAY4"),
            ];
            // Two outputs on the adapter and no index 7: ambiguous, so refuse.
            let selector = super::super::OutputSelector::Adapter {
                name: "NVIDIA GRID V100D-16Q".to_string(),
                output_index: 7,
            };
            let error = resolve_output_from_outputs(&selector, &outputs)
                .expect_err("ambiguous selection must fail");
            assert!(error.contains("could not uniquely resolve"));

            // An adapter that is not present at all must never resolve either.
            let absent = super::super::OutputSelector::Adapter {
                name: "AMD Radeon Pro W6800".to_string(),
                output_index: 0,
            };
            assert!(resolve_output_from_outputs(&absent, &outputs).is_err());
        }

        #[test]
        fn an_exact_index_match_still_wins_over_the_fallback() {
            let outputs = vec![
                output(0, "NVIDIA GRID V100D-16Q", 0, r"\\.\DISPLAY3"),
                output(1, "NVIDIA GRID V100D-16Q", 1, r"\\.\DISPLAY4"),
            ];
            let selector = super::super::OutputSelector::Adapter {
                name: "NVIDIA GRID V100D-16Q".to_string(),
                output_index: 1,
            };
            let selected = resolve_output_from_outputs(&selector, &outputs).expect("exact match");
            assert_eq!(selected.device_name, r"\\.\DISPLAY4");
        }

        #[test]
        fn adapter_selector_resolves_case_insensitively() {
            let outputs = vec![
                output(0, "Microsoft Basic Render Driver", 0, r"\\.\DISPLAY1"),
                output(1, "NVIDIA GRID V100D-16Q", 0, r"\\.\DISPLAY6"),
            ];
            let selector = super::super::OutputSelector::Adapter {
                name: "nvidia grid v100d-16q".to_string(),
                output_index: 0,
            };
            let selected = resolve_output_from_outputs(&selector, &outputs).expect("selector");
            assert_eq!(selected.global_index, 1);
        }

        #[test]
        fn selection_error_lists_available_adapters() {
            let outputs = vec![output(2, "NVIDIA GRID V100D-16Q", 0, r"\\.\DISPLAY6")];
            let error = selection_error(
                &super::super::OutputSelector::Adapter {
                    name: "missing".to_string(),
                    output_index: 0,
                },
                &outputs,
            );
            assert!(error.contains("NVIDIA GRID V100D-16Q"));
            assert!(error.contains("global=2"));
        }

        #[test]
        fn adapter_comparison_uses_windows_unicode_case_rules() {
            assert!(windows_eq_ignore_case("NVIDIA ÅDAPTER", "nvidia ådapter"));
        }

        #[test]
        fn duplicate_adapter_identity_fails_closed() {
            let outputs = vec![
                output(1, "NVIDIA GRID V100D-16Q", 0, r"\\.\DISPLAY6"),
                output(2, "NVIDIA GRID V100D-16Q", 0, r"\\.\DISPLAY7"),
            ];
            let error = resolve_output_from_outputs(
                &super::super::OutputSelector::Adapter {
                    name: "NVIDIA GRID V100D-16Q".to_string(),
                    output_index: 0,
                },
                &outputs,
            )
            .unwrap_err();
            assert!(error.contains("could not uniquely resolve"));
        }

        #[test]
        fn session_factory_cache_reuses_and_bounds_recreation() {
            #[derive(Debug)]
            struct FakeFactory {
                generation: u32,
                current: bool,
            }

            let mut cache = FactoryCache::default();
            for _ in 0..10 {
                let factory = cache
                    .get_or_try_init(
                        |factory: &FakeFactory| factory.current,
                        || {
                            Ok::<_, ()>(FakeFactory {
                                generation: 1,
                                current: true,
                            })
                        },
                    )
                    .expect("initial factory");
                assert_eq!(factory.generation, 1);
            }
            assert_eq!(cache.creation_count(), 1);

            cache.factory.as_mut().expect("cached factory").current = false;
            let refreshed = cache
                .get_or_try_init(
                    |factory| factory.current,
                    || {
                        Ok::<_, ()>(FakeFactory {
                            generation: 2,
                            current: true,
                        })
                    },
                )
                .expect("stale refresh");
            assert_eq!(refreshed.generation, 2);

            cache.invalidate();
            let post_topology = cache
                .get_or_try_init(
                    |factory| factory.current,
                    || {
                        Ok::<_, ()>(FakeFactory {
                            generation: 3,
                            current: true,
                        })
                    },
                )
                .expect("topology refresh");
            assert_eq!(post_topology.generation, 3);
            assert_eq!(cache.creation_count(), 3);
        }
    }

    fn enumerate_dxgi_outputs(factory: &IDXGIFactory1) -> Result<Vec<DxgiOutput>, String> {
        // SAFETY: windows-rs owns all returned COM interfaces and releases them on drop.
        unsafe {
            let mut outputs = Vec::new();
            let mut adapter_index = 0u32;
            loop {
                let adapter: IDXGIAdapter = match factory.EnumAdapters(adapter_index) {
                    Ok(adapter) => adapter,
                    Err(_) => break,
                };
                adapter_index += 1;
                let adapter_desc = adapter
                    .GetDesc()
                    .map_err(|error| format!("IDXGIAdapter::GetDesc: {error}"))?;
                let adapter_name = utf16(&adapter_desc.Description);
                let mut local_output = 0u32;
                loop {
                    let adapter_output_index = local_output;
                    let output = match adapter.EnumOutputs(adapter_output_index) {
                        Ok(output) => output,
                        Err(_) => break,
                    };
                    local_output += 1;
                    let desc = output
                        .GetDesc()
                        .map_err(|error| format!("IDXGIOutput::GetDesc: {error}"))?;
                    if !desc.AttachedToDesktop.as_bool() {
                        continue;
                    }
                    let rect = desc.DesktopCoordinates;
                    outputs.push(DxgiOutput {
                        device_name: utf16(&desc.DeviceName),
                        output_index: outputs.len() as u32,
                        adapter_name: adapter_name.clone(),
                        adapter_output_index,
                        vendor_id: adapter_desc.VendorId,
                        adapter_luid: {
                            let luid = adapter_desc.AdapterLuid;
                            AdapterLuid {
                                low_part: luid.LowPart,
                                high_part: luid.HighPart,
                            }
                        },
                        desktop_rect: DesktopRect {
                            left: rect.left,
                            top: rect.top,
                            width: rect.right.saturating_sub(rect.left),
                            height: rect.bottom.saturating_sub(rect.top),
                        },
                    });
                }
            }
            Ok(outputs)
        }
    }

    fn display_change_name(change: DISP_CHANGE) -> String {
        match change.0 {
            0 => "DISP_CHANGE_SUCCESSFUL(0)".to_string(),
            1 => "DISP_CHANGE_RESTART(1)".to_string(),
            -1 => "DISP_CHANGE_FAILED(-1)".to_string(),
            -2 => "DISP_CHANGE_BADMODE(-2)".to_string(),
            -3 => "DISP_CHANGE_NOTUPDATED(-3)".to_string(),
            -4 => "DISP_CHANGE_BADFLAGS(-4)".to_string(),
            -5 => "DISP_CHANGE_BADPARAM(-5)".to_string(),
            -6 => "DISP_CHANGE_BADDUALVIEW(-6)".to_string(),
            other => format!("code {other}"),
        }
    }

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn utf16(value: &[u16]) -> String {
        let end = value
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(value.len());
        String::from_utf16_lossy(&value[..end])
    }

    /// Outcome of one standalone (journal-based) display restore attempt,
    /// carrying just enough safe, non-identifying detail for the caller
    /// (`restore-display` CLI or the crash watchdog) to emit the correct
    /// lifecycle event.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) enum RestoreOutcome {
        /// The journal existed but nothing had been mutated; nothing to
        /// restore or report.
        AlreadyClean,
        /// The display was restored and fully verified.
        Restored {
            restore_backend: &'static str,
            width: u32,
            height: u32,
        },
    }

    fn journal_restore_backend_name(has_nvapi: bool, has_headless_edids: bool) -> &'static str {
        if has_headless_edids {
            "nvapi-headless-edid-plus-set-display-config-exact"
        } else if has_nvapi {
            "nvapi-purge-plus-set-display-config-exact"
        } else {
            "set-display-config-plus-exact-devmode"
        }
    }

    pub(super) fn migrate_legacy_from_path(path: &std::path::Path) -> Result<(), String> {
        require_legacy_migration_context()?;
        let journal = crate::recovery::legacy_windows_migration_evidence(path)?;
        let paths: Vec<DISPLAYCONFIG_PATH_INFO> =
            decode_values(&journal.topology_paths()?, 128, "DISPLAYCONFIG_PATH_INFO")?;
        let modes: Vec<DISPLAYCONFIG_MODE_INFO> =
            decode_values(&journal.topology_modes()?, 512, "DISPLAYCONFIG_MODE_INFO")?;
        let current_all = query_all_topology()?;
        let inventory = crate::gpu_probe::probe()
            .map_err(|error| format!("inventory legacy migration outputs: {error}"))?;
        if let Some(error) = inventory.topology_error {
            return Err(format!(
                "legacy migration all-path inventory failed: {error}"
            ));
        }
        let mut stable = Vec::with_capacity(paths.len());
        let mut selected_path_index = None;

        for (legacy_index, path_info) in paths.iter().enumerate() {
            let evidence = legacy_source_evidence(path_info, &modes)?;
            let mut candidates = Vec::new();
            for current_path in &current_all.paths {
                let Ok(current_evidence) = legacy_source_evidence(current_path, &current_all.modes)
                else {
                    continue;
                };
                if (
                    current_evidence.width,
                    current_evidence.height,
                    current_evidence.x,
                    current_evidence.y,
                ) != (evidence.width, evidence.height, evidence.x, evidence.y)
                {
                    continue;
                }
                let monitor_path = target_monitor_device_path(current_path)?;
                for adapter in &inventory.adapters {
                    if adapter.vendor_id == 0x10de {
                        continue;
                    }
                    for output in &adapter.outputs {
                        if output.monitor_device_path.as_deref() == Some(monitor_path.as_str()) {
                            candidates.push((adapter, output, current_path));
                        }
                    }
                }
            }
            let [(adapter, output, current_path)] = candidates.as_slice() else {
                return Err(format!(
                    "legacy non-NVIDIA path {}x{}+{},{} cannot be mapped unambiguously to an \
                     immutable connected output; journal preserved for manual recovery",
                    evidence.width, evidence.height, evidence.x, evidence.y
                ));
            };
            stable.push(stable_output_identity(adapter, output, None)?);
            if source_gdi_name(current_path)?.eq_ignore_ascii_case(&journal.device_name) {
                if selected_path_index.replace(legacy_index).is_some() {
                    return Err(
                        "legacy selected Windows-native output is ambiguous; journal preserved"
                            .to_string(),
                    );
                }
            }
        }
        let selected_path_index = selected_path_index.ok_or_else(|| {
            "legacy selected Windows-native output cannot be proven from current all-path \
             inventory; journal preserved for manual recovery"
                .to_string()
        })?;

        crate::recovery::upgrade_legacy_stable_topology(
            path,
            crate::recovery::StableTopologySnapshot { paths: stable },
            selected_path_index,
        )
    }

    pub(super) fn restore_from_path(path: &std::path::Path) -> Result<RestoreOutcome, String> {
        let journal = crate::recovery::read(path)?;
        super::require_stable_recovery_schema(journal.version, journal.stable_topology.is_some())?;
        let mut errors = Vec::new();
        let mut nvapi_errors = Vec::new();
        let mut nvapi_restored = false;
        let mut recovery_device_name = journal.device_name.clone();

        let paths: Vec<DISPLAYCONFIG_PATH_INFO> =
            decode_values(&journal.topology_paths()?, 128, "DISPLAYCONFIG_PATH_INFO")?;
        let modes: Vec<DISPLAYCONFIG_MODE_INFO> =
            decode_values(&journal.topology_modes()?, 512, "DISPLAYCONFIG_MODE_INFO")?;
        let stable_topology = journal.stable_topology.as_ref().ok_or_else(|| {
            format!(
                "display recovery journal version {} lacks complete stable output identities; \
                 automatic topology-changing recovery is unsafe",
                journal.version
            )
        })?;
        if stable_topology.paths.len() != paths.len() {
            return Err(format!(
                "stable topology has {} paths but the journaled Windows topology has {}",
                stable_topology.paths.len(),
                paths.len()
            ));
        }
        let mode: DEVMODEW = decode_value(&journal.devmode()?, "DEVMODEW")?;
        if mode.dmSize as usize != std::mem::size_of::<DEVMODEW>() {
            return Err(format!(
                "journal DEVMODEW dmSize {} does not match current ABI size {}",
                mode.dmSize,
                std::mem::size_of::<DEVMODEW>()
            ));
        }
        if !journal.mutation_started {
            crate::recovery::remove(path)?;
            tracing::info!(
                target: DISPLAY,
                journal = %path.display(),
                "discarded fully validated unmutated display recovery journal"
            );
            return Ok(RestoreOutcome::AlreadyClean);
        }
        if !journal.headless_nvapi_edids.is_empty() {
            crate::nvapi_headless::restore_recovery_entries(&journal.headless_nvapi_edids)
                .map_err(|error| format!("restore NVIDIA headless EDIDs: {error}"))?;
        }
        let mut reconstructed = if journal.nvapi.is_none() {
            let (paths, modes, selected_device) = if journal.headless_nvapi_edids.is_empty() {
                reconstruct_current_topology_with_settle(
                    &paths,
                    &modes,
                    stable_topology,
                    journal.selected_path_index,
                )?
            } else {
                reconstruct_headless_original_geometry(
                    &paths,
                    &modes,
                    stable_topology,
                    journal.selected_path_index,
                )?
            };
            recovery_device_name = selected_device;
            Some((paths, modes))
        } else {
            None
        };

        if let Some(recovery) = journal.nvapi.as_ref() {
            let recovery = recovery.clone();
            let display_id = recovery.display_id.ok_or_else(|| {
                "NVAPI recovery lacks an authoritative stable display id".to_string()
            })?;
            let mut identity_matches = stable_topology
                .paths
                .iter()
                .filter(|identity| identity.nvapi_display_id() == Some(display_id));
            let selected_identity = identity_matches.next().ok_or_else(|| {
                format!("stable topology has no output for NVAPI display id 0x{display_id:08x}")
            })?;
            if identity_matches.next().is_some() {
                return Err(format!(
                    "stable topology repeats NVAPI display id 0x{display_id:08x}"
                ));
            }

            let mut driver = nvapi::Nvapi::load()
                .map_err(|error| format!("load NVAPI for authoritative recovery: {error}"))?;
            let mapping = driver
                .map_recovery_display(
                    &recovery.device_name,
                    recovery.adapter_luid,
                    &recovery.original_config,
                    recovery.display_id,
                )
                .map_err(|error| format!("map authoritative stable NVAPI output: {error}"))?;
            let inventory = crate::gpu_probe::probe()
                .map_err(|error| format!("inventory authoritative recovery output: {error}"))?;
            if let Some(error) = inventory.topology_error {
                return Err(format!(
                    "inventory authoritative recovery output reported topology failure: {error}"
                ));
            }
            let injected_edid_proven = matches!(
                recovery.edid_write_stage,
                nvapi::EdidWriteStage::Attempted | nvapi::EdidWriteStage::Verified
            ) && recovery.intended_edid_sha256.is_some();
            let mut current_matches = Vec::new();
            for adapter in &inventory.adapters {
                for output in &adapter.outputs {
                    if !adapter
                        .stable_id
                        .eq_ignore_ascii_case(&selected_identity.adapter_stable_id)
                        || output.adapter_output_index != selected_identity.adapter_output_index
                    {
                        continue;
                    }
                    let mut candidate_dxgi = SessionDxgiEnumerator::default();
                    let candidate_output = candidate_dxgi.output_by_device(&output.device_name)?;
                    let candidate_mapping = match driver
                        .map_display(&output.device_name, candidate_output.adapter_luid)
                    {
                        Ok(mapping) => mapping,
                        Err(_) => continue,
                    };
                    if candidate_mapping != mapping {
                        continue;
                    }
                    let current_identity =
                        stable_output_identity(adapter, output, Some(candidate_mapping))?;
                    if selected_identity.immutable_binding_matches(&current_identity)
                        && (injected_edid_proven || selected_identity == &current_identity)
                    {
                        current_matches.push((adapter, output));
                    }
                }
            }
            let [(_adapter, current_output)] = current_matches.as_slice() else {
                if current_matches.is_empty() {
                    return Err("authoritative stable recovery output is absent".to_string());
                }
                return Err("authoritative stable recovery output is ambiguous".to_string());
            };
            let current_output = *current_output;
            let mut dxgi = SessionDxgiEnumerator::default();
            let output = dxgi.output_by_device(&current_output.device_name)?;
            if output.adapter_luid != mapping.adapter_luid
                || output.adapter_output_index != selected_identity.adapter_output_index
            {
                return Err(
                    "stable NVAPI identity and current DXGI output binding disagree".to_string(),
                );
            }
            let rebound_target = DisplayTarget {
                device_name: output.device_name,
                vendor_id: output.vendor_id,
                adapter_luid: output.adapter_luid,
                adapter_output_index: output.adapter_output_index,
            };
            recovery_device_name = rebound_target.device_name.clone();
            let all_paths = query_all_topology()?;
            let mut boot_paths = std::collections::BTreeMap::new();
            for identity in &stable_topology.paths {
                let Some(stable_display_id) = identity.nvapi_display_id() else {
                    continue;
                };
                let (stable_output_id, stable_head) =
                    identity.nvapi_output_binding().ok_or_else(|| {
                        format!(
                            "stable NVIDIA display id 0x{stable_display_id:08x} lacks output binding"
                        )
                    })?;
                let mut output_matches = Vec::new();
                for adapter in &inventory.adapters {
                    if !adapter
                        .stable_id
                        .eq_ignore_ascii_case(&identity.adapter_stable_id)
                    {
                        continue;
                    }
                    for output in &adapter.outputs {
                        if output.adapter_output_index != identity.adapter_output_index
                            || output.monitor_device_path.is_none()
                        {
                            continue;
                        }
                        let mut candidate_dxgi = SessionDxgiEnumerator::default();
                        let candidate_output =
                            candidate_dxgi.output_by_device(&output.device_name)?;
                        let candidate_mapping = match driver
                            .map_display(&output.device_name, candidate_output.adapter_luid)
                        {
                            Ok(mapping)
                                if mapping.display_id == stable_display_id
                                    && mapping.output_id == stable_output_id
                                    && mapping.head == stable_head =>
                            {
                                mapping
                            }
                            _ => continue,
                        };
                        output_matches.push((output.device_name.as_str(), candidate_mapping));
                    }
                }
                let [(device_name, _stable_mapping)] = output_matches.as_slice() else {
                    if output_matches.is_empty() {
                        return Err(format!(
                            "connected stable display id 0x{stable_display_id:08x} has no current output"
                        ));
                    }
                    return Err(format!(
                        "connected stable display id 0x{stable_display_id:08x} is ambiguous"
                    ));
                };
                let mut path_matches = all_paths.paths.iter().filter(|path_info| {
                    source_gdi_name(path_info)
                        .is_ok_and(|name| name.eq_ignore_ascii_case(*device_name))
                });
                let Some(path_info) = path_matches.next() else {
                    return Err(format!(
                        "connected stable display id 0x{stable_display_id:08x} has no current all-path binding"
                    ));
                };
                if path_matches.next().is_some() {
                    return Err(format!(
                        "connected stable display id 0x{stable_display_id:08x} has ambiguous all-path bindings"
                    ));
                }
                boot_paths.insert(
                    stable_display_id,
                    nvapi::BootPathBinding {
                        source_id: path_info.sourceInfo.id,
                        target_id: path_info.targetInfo.id,
                    },
                );
            }
            match nvapi::restore_recovery_staged_with_topology_fallback(
                &mut driver,
                &recovery,
                Some(&boot_paths),
                |stage| crate::recovery::mark_nvapi_cleanup_stage(path, stage),
                |nvapi_error| {
                    let device = wide(&rebound_target.device_name);
                    // SAFETY: the stable-identity rebound DXGI name is
                    // NUL-terminated and mode is the ABI-validated journaled mode.
                    let status = unsafe {
                        ChangeDisplaySettingsExW(
                            PCWSTR(device.as_ptr()),
                            Some(&mode),
                            HWND::default(),
                            CDS_TYPE(0),
                            None,
                        )
                    };
                    if status != DISP_CHANGE_SUCCESSFUL {
                        return Err(format!(
                            "restore NVAPI topology: {nvapi_error}; rebound exact-mode \
                             fallback returned {}",
                            display_change_name(status)
                        ));
                    }
                    let mut verify = SessionDxgiEnumerator::default();
                    wait_for_mode(
                        &mut verify,
                        &rebound_target,
                        DisplaySize {
                            width: journal.original_width,
                            height: journal.original_height,
                        },
                        MODE_SETTLE_TIMEOUT,
                    )
                    .map(|_| ())
                    .map_err(|error| {
                        format!(
                            "restore NVAPI topology: {nvapi_error}; rebound exact-mode \
                             fallback did not settle: {error}"
                        )
                    })
                },
            ) {
                Ok(()) => {
                    nvapi_restored = true;
                    tracing::info!(
                        target: DISPLAY,
                        stable_display_id = format_args!("0x{display_id:08x}"),
                        rebound_device = %recovery_device_name,
                        "standalone recovery restored authoritative stable NVAPI output"
                    );
                }
                Err(error) => nvapi_errors.push(format!("NVAPI recovery: {error}")),
            }
        }

        if journal.nvapi.is_some() && !nvapi_restored {
            return Err(nvapi_errors.join("; "));
        }
        if reconstructed.is_none() {
            let (rebuilt_paths, rebuilt_modes, selected_device) =
                if journal.headless_nvapi_edids.is_empty() {
                    reconstruct_current_topology_with_settle(
                        &paths,
                        &modes,
                        stable_topology,
                        journal.selected_path_index,
                    )?
                } else {
                    reconstruct_headless_original_geometry(
                        &paths,
                        &modes,
                        stable_topology,
                        journal.selected_path_index,
                    )?
                };
            recovery_device_name = selected_device;
            reconstructed = Some((rebuilt_paths, rebuilt_modes));
        }
        let (reconstructed_paths, reconstructed_modes) =
            reconstructed.expect("topology reconstructed above");

        let flags = SET_DISPLAY_CONFIG_FLAGS(
            SDC_APPLY.0
                | SDC_USE_SUPPLIED_DISPLAY_CONFIG.0
                | SDC_VIRTUAL_MODE_AWARE.0
                | if journal.headless_nvapi_edids.is_empty() {
                    0
                } else {
                    SDC_ALLOW_CHANGES.0
                },
        );
        // SAFETY: every boot-local adapter/source/target identity was rebound
        // from the current all-path inventory before this call.
        let topology_rc = unsafe {
            SetDisplayConfig(
                Some(&reconstructed_paths),
                Some(&reconstructed_modes),
                flags,
            )
        };
        if topology_rc != 0 {
            errors.push(format!(
                "standalone reconstructed SetDisplayConfig restore returned {topology_rc}"
            ));
        }

        let device = wide(&recovery_device_name);
        // SAFETY: the device name is NUL-terminated and mode is the initialized,
        // ABI-validated pre-transaction DEVMODEW from the journal.
        let mode_rc = unsafe {
            ChangeDisplaySettingsExW(
                PCWSTR(device.as_ptr()),
                Some(&mode),
                HWND::default(),
                CDS_TYPE(0),
                None,
            )
        };
        if mode_rc != DISP_CHANGE_SUCCESSFUL {
            errors.push(format!(
                "standalone exact-mode restore returned {}",
                display_change_name(mode_rc)
            ));
        }

        if journal.headless_nvapi_edids.is_empty() {
            match query_active_topology() {
                Ok(current) => {
                    let current_stable = capture_stable_topology(&current);
                    match current_stable.and_then(|current_stable| {
                        complete_topology_with_stable_identities(
                            &paths,
                            &modes,
                            stable_topology,
                            &current.paths,
                            &current.modes,
                            &current_stable,
                        )
                    }) {
                        Ok(true) => {}
                        Ok(false) => errors.push(
                            "standalone restore did not reproduce the complete journaled topology \
                             (stable output identities, active source/target paths, clone groups, \
                             desktop images, positions, primary, or modes)"
                                .to_string(),
                        ),
                        Err(error) => errors.push(format!(
                            "normalize complete topology after standalone restore: {error}"
                        )),
                    }
                }
                Err(error) => errors.push(format!(
                    "query complete topology after standalone restore: {error}"
                )),
            }
        } else if let Err(error) =
            verify_headless_original_geometry(&paths, &modes, stable_topology)
        {
            errors.push(error);
        }

        let mut dxgi = SessionDxgiEnumerator::default();
        match dxgi.output_by_device(&recovery_device_name) {
            Ok(output) => {
                let target = DisplayTarget {
                    device_name: output.device_name,
                    vendor_id: output.vendor_id,
                    adapter_luid: output.adapter_luid,
                    adapter_output_index: output.adapter_output_index,
                };
                let expected = DisplaySize {
                    width: journal.original_width,
                    height: journal.original_height,
                };
                match wait_for_mode(&mut dxgi, &target, expected, MODE_SETTLE_TIMEOUT) {
                    Ok(restored)
                        if restored.size == expected
                            && restored.refresh_hz == journal.original_refresh_hz =>
                    {
                        if restored.active_outputs != paths.len() as u32 {
                            errors.push(format!(
                                "standalone restore left {} active outputs but the journal \
                                 topology has {} active paths",
                                restored.active_outputs,
                                paths.len()
                            ));
                        }
                    }
                    Ok(restored) => errors.push(format!(
                        "standalone restore settled at {} @ {}Hz, expected {} @ {}Hz",
                        restored.size, restored.refresh_hz, expected, journal.original_refresh_hz
                    )),
                    Err(error) => errors.push(error),
                }
            }
            Err(error) => errors.push(error),
        }

        if errors.is_empty() && nvapi_errors.is_empty() {
            let restore_backend = journal_restore_backend_name(
                journal.nvapi.is_some(),
                !journal.headless_nvapi_edids.is_empty(),
            );
            let width = journal.original_width;
            let height = journal.original_height;
            crate::recovery::remove(path)?;
            tracing::info!(
                target: DISPLAY,
                journal = %path.display(),
                device = %journal.device_name,
                restored = %format!("{}x{}", journal.original_width, journal.original_height),
                refresh_hz = journal.original_refresh_hz,
                "standalone display recovery completed and journal removed"
            );
            Ok(RestoreOutcome::Restored {
                restore_backend,
                width,
                height,
            })
        } else {
            errors.extend(nvapi_errors);
            Err(errors.join("; "))
        }
    }

    fn legacy_source_evidence(
        path: &DISPLAYCONFIG_PATH_INFO,
        modes: &[DISPLAYCONFIG_MODE_INFO],
    ) -> Result<super::LegacySourceEvidence, String> {
        let source_mode_index =
            virtual_source_mode_index(unsafe { path.sourceInfo.Anonymous.Anonymous._bitfield })
                .ok_or_else(|| {
                    format!(
                        "legacy recovery source {} has no virtual-aware source mode",
                        path.sourceInfo.id
                    )
                })?;
        let mode = modes.get(source_mode_index).ok_or_else(|| {
            format!(
                "legacy recovery source mode {source_mode_index} is outside {} modes",
                modes.len()
            )
        })?;
        if mode.infoType != DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE {
            return Err(format!(
                "legacy recovery mode {source_mode_index} is not a source mode"
            ));
        }
        // SAFETY: the mode was checked against the SOURCE discriminator.
        let source_mode = unsafe { mode.Anonymous.sourceMode };
        Ok(super::LegacySourceEvidence {
            width: source_mode.width,
            height: source_mode.height,
            x: source_mode.position.x,
            y: source_mode.position.y,
        })
    }

    pub(super) fn watchdog(
        parent_handle_value: isize,
        ready_handle_value: isize,
        path: &std::path::Path,
    ) -> Result<RestoreOutcome, String> {
        match crate::recovery::wait_for_watchdog_parent(
            parent_handle_value,
            ready_handle_value,
            path,
        )? {
            crate::recovery::WatchdogWait::Disarmed => {
                return Ok(RestoreOutcome::AlreadyClean);
            }
            crate::recovery::WatchdogWait::ParentExited => {}
        }
        if path.exists() {
            tracing::warn!(
                target: DISPLAY,
                journal = %path.display(),
                "host exited with armed display recovery; watchdog is processing recovery now"
            );
            let mut last_error = String::new();
            for attempt in 1..=super::RESTORE_ATTEMPTS {
                match restore_from_path(path) {
                    Ok(outcome) => return Ok(outcome),
                    Err(error) => {
                        last_error = error;
                        tracing::warn!(
                            target: DISPLAY,
                            attempt,
                            error = %last_error,
                            "watchdog display recovery attempt failed"
                        );
                        if attempt < super::RESTORE_ATTEMPTS {
                            std::thread::sleep(Duration::from_millis(500));
                        }
                    }
                }
            }
            Err(format!(
                "watchdog display recovery exhausted {} attempts: {last_error}",
                super::RESTORE_ATTEMPTS
            ))
        } else {
            Ok(RestoreOutcome::AlreadyClean)
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::os::windows::io::AsRawHandle;
        use windows::Win32::Foundation::DUPLICATE_SAME_ACCESS;

        #[test]
        fn windows_argument_quoting_preserves_spaces_quotes_and_trailing_slashes() {
            assert_eq!(quote_windows_argument("plain"), "\"plain\"");
            assert_eq!(
                quote_windows_argument(r#"C:\Program Files\a"b\"#),
                "\"C:\\Program Files\\a\\\"b\\\\\""
            );
        }

        #[test]
        fn vmware_resolution_command_uses_single_monitor_topology_syntax() {
            assert_eq!(
                vmware_resolution_args(DisplaySize {
                    width: 1800,
                    height: 1168,
                }),
                ["0", "1", ",", "0", "0", "1800", "1168"]
            );
        }

        #[test]
        fn remote_session_error_requires_console_handoff() {
            let error = remote_session_display_error(2);
            assert!(error.contains("remote display protocol 2"));
            assert!(error.contains("disconnect RDP"));
            assert!(error.contains("physical console"));
        }

        #[test]
        fn exited_parent_cannot_acknowledge_watchdog_readiness() {
            let mut child = std::process::Command::new("cmd.exe")
                .args(["/C", "exit", "0"])
                .spawn()
                .unwrap();
            child.wait().unwrap();

            // SAFETY: GetCurrentProcess returns this process's pseudo-handle.
            let current = unsafe { GetCurrentProcess() };
            let child_handle = HANDLE(child.as_raw_handle());
            let mut parent_duplicate = HANDLE::default();
            // SAFETY: child_handle remains valid while child is alive, and the output pointer is
            // writable. The duplicate is independently owned by watchdog.
            unsafe {
                DuplicateHandle(
                    current,
                    child_handle,
                    current,
                    &mut parent_duplicate,
                    0,
                    true,
                    DUPLICATE_SAME_ACCESS,
                )
            }
            .unwrap();
            // SAFETY: null security attributes create a valid manual-reset event.
            let ready = unsafe { CreateEventW(None, true, false, PCWSTR::null()) }.unwrap();
            let mut ready_observer = HANDLE::default();
            // SAFETY: ready is valid, and ready_observer receives an independently owned duplicate.
            unsafe {
                DuplicateHandle(
                    current,
                    ready,
                    current,
                    &mut ready_observer,
                    0,
                    false,
                    DUPLICATE_SAME_ACCESS,
                )
            }
            .unwrap();
            let ready_observer = OwnedHandle(ready_observer);

            let error = watchdog(
                parent_duplicate.0 as isize,
                ready.0 as isize,
                std::path::Path::new(r"C:\does-not-exist\display-recovery.json"),
            )
            .unwrap_err();

            assert!(error.contains("parent exited before readiness"));
            // SAFETY: ready_observer is a valid event handle.
            assert_eq!(
                unsafe { WaitForSingleObject(ready_observer.raw(), 0) },
                WAIT_TIMEOUT,
                "an exited parent must not receive a ready acknowledgment"
            );
        }
    }
}

#[cfg(windows)]
pub fn restore_from_journal(
    path: Option<std::path::PathBuf>,
    emitter: &crate::LifecycleEmitter,
) -> Result<(), String> {
    crate::input::initialize_process_dpi_awareness();
    let path = path.unwrap_or_else(crate::recovery::default_path);
    let correlation_id = crate::eventlog::random_correlation_id();
    match windows_backend::restore_from_path(&path) {
        Ok(outcome) => {
            emit_cli_restore_outcome(emitter, correlation_id, outcome);
            Ok(())
        }
        Err(error) => {
            emit_display_restore_failed(emitter, correlation_id, "standalone_restore");
            Err(error)
        }
    }
}

#[cfg(not(windows))]
pub fn restore_from_journal(
    _path: Option<std::path::PathBuf>,
    _emitter: &crate::LifecycleEmitter,
) -> Result<(), String> {
    Err("display recovery is only available on Windows".to_string())
}

#[cfg(windows)]
pub fn migrate_legacy_journal(
    path: Option<std::path::PathBuf>,
    emitter: &crate::LifecycleEmitter,
) -> Result<(), String> {
    crate::input::initialize_process_dpi_awareness();
    let path = path.unwrap_or_else(crate::recovery::default_path);
    windows_backend::migrate_legacy_from_path(&path)?;
    restore_from_journal(Some(path), emitter)
}

#[cfg(not(windows))]
pub fn migrate_legacy_journal(
    _path: Option<std::path::PathBuf>,
    _emitter: &crate::LifecycleEmitter,
) -> Result<(), String> {
    Err("legacy display recovery migration is only available on Windows".to_string())
}

#[cfg(windows)]
pub fn run_restore_watchdog(
    parent_handle: isize,
    ready_handle: isize,
    path: std::path::PathBuf,
    correlation_id: arcen_telemetry::CorrelationId,
    emitter: &crate::LifecycleEmitter,
) -> Result<(), String> {
    crate::input::initialize_process_dpi_awareness();
    match windows_backend::watchdog(parent_handle, ready_handle, &path) {
        Ok(outcome) => {
            emit_watchdog_restore_outcome(emitter, correlation_id, outcome);
            Ok(())
        }

        Err(error) => {
            emit_display_restore_failed(emitter, correlation_id, "watchdog_restore");
            Err(error)
        }
    }
}

#[cfg(not(windows))]
pub fn run_restore_watchdog(
    _parent_handle: isize,
    _ready_handle: isize,
    _path: std::path::PathBuf,
    _correlation_id: arcen_telemetry::CorrelationId,
    _emitter: &crate::LifecycleEmitter,
) -> Result<(), String> {
    Err("display recovery watchdog is only available on Windows".to_string())
}

#[cfg(windows)]
pub(crate) fn spawn_timezone_recovery_watchdog(
    path: &std::path::Path,
    owner: &str,
) -> Result<(), String> {
    let correlation_id = arcen_telemetry::CorrelationId::parse_uuid(owner)
        .map_err(|error| format!("timezone watchdog correlation id: {error}"))?;
    windows_backend::spawn_recovery_watchdog(
        path,
        &correlation_id,
        crate::recovery::WatchdogResource::Timezone,
    )
}

#[cfg(windows)]
pub(crate) fn spawn_nvapi_headless_recovery_watchdog(
    path: &std::path::Path,
    owner: &str,
) -> Result<(), String> {
    let correlation_id = arcen_telemetry::CorrelationId::parse_uuid(owner)
        .map_err(|error| format!("NVAPI headless watchdog correlation id: {error}"))?;
    windows_backend::spawn_recovery_watchdog(
        path,
        &correlation_id,
        crate::recovery::WatchdogResource::NvapiHeadless,
    )
}

#[cfg(not(windows))]
pub(crate) fn spawn_nvapi_headless_recovery_watchdog(
    _path: &std::path::Path,
    _owner: &str,
) -> Result<(), String> {
    Err("NVAPI headless recovery watchdog is only available on Windows".to_string())
}

#[cfg(windows)]
pub(crate) fn require_nvapi_headless_probe_context() -> Result<(), String> {
    crate::input::initialize_process_dpi_awareness();
    windows_backend::require_legacy_migration_context().map_err(|error| {
        error.replace(
            "legacy display-journal migration",
            "NVAPI headless activation probe",
        )
    })
}

#[cfg(not(windows))]
pub(crate) fn require_nvapi_headless_probe_context() -> Result<(), String> {
    Err("NVAPI headless activation probe is only available on Windows".to_string())
}

#[cfg(not(windows))]
pub(crate) fn spawn_timezone_recovery_watchdog(
    _path: &std::path::Path,
    _owner: &str,
) -> Result<(), String> {
    Err("timezone recovery watchdog is only available on Windows".to_string())
}

/// Emits `DISPLAY_RESTORED` (1201) for an explicit `restore-display` CLI
/// invocation. An already-clean journal emits nothing.
#[cfg(windows)]
fn emit_cli_restore_outcome(
    emitter: &crate::LifecycleEmitter,
    correlation_id: arcen_telemetry::CorrelationId,
    outcome: windows_backend::RestoreOutcome,
) {
    use arcen_telemetry::{FieldValue, LifecycleEventKind, StructuredFields};
    match outcome {
        windows_backend::RestoreOutcome::AlreadyClean => {}
        windows_backend::RestoreOutcome::Restored {
            restore_backend,
            width,
            height,
        } => {
            let mut fields = StructuredFields::default();
            let _ = fields.insert(
                "restore_backend",
                FieldValue::String(restore_backend.to_string()),
            );
            let _ = fields.insert("changed", FieldValue::Boolean(true));
            let _ = fields.insert("width", FieldValue::Integer(i64::from(width)));
            let _ = fields.insert("height", FieldValue::Integer(i64::from(height)));
            crate::emit_lifecycle_event(
                emitter,
                LifecycleEventKind::DisplayRestored,
                correlation_id,
                fields,
            );
        }
    }
}

/// Emits `WATCHDOG_RESTORE` (1204) once the crash watchdog has restored
/// display state, whether or not the secondary NVAPI cleanup step degraded.
/// An already-clean journal (nothing to restore) emits nothing.
#[cfg(windows)]
fn emit_watchdog_restore_outcome(
    emitter: &crate::LifecycleEmitter,
    correlation_id: arcen_telemetry::CorrelationId,
    outcome: windows_backend::RestoreOutcome,
) {
    use arcen_telemetry::{FieldValue, LifecycleEventKind, StructuredFields};
    let restore_backend = match outcome {
        windows_backend::RestoreOutcome::AlreadyClean => return,
        windows_backend::RestoreOutcome::Restored {
            restore_backend, ..
        } => restore_backend,
    };
    let mut fields = StructuredFields::default();
    let _ = fields.insert(
        "restore_backend",
        FieldValue::String(restore_backend.to_string()),
    );
    let _ = fields.insert(
        "reason_class",
        FieldValue::String("crash_watchdog_restore".to_string()),
    );
    let _ = fields.insert("journal_pending", FieldValue::Boolean(false));
    crate::emit_lifecycle_event(
        emitter,
        LifecycleEventKind::WatchdogRestore,
        correlation_id,
        fields,
    );
}

/// Emits `DISPLAY_RESTORE_FAILED` (1203) for either a standalone
/// `restore-display` CLI failure or a watchdog restore failure. The journal
/// is never removed on this path, so `journal_pending` is always `true`.
#[cfg(windows)]
fn emit_display_restore_failed(
    emitter: &crate::LifecycleEmitter,
    correlation_id: arcen_telemetry::CorrelationId,
    stage: &'static str,
) {
    use arcen_telemetry::{FieldValue, LifecycleEventKind, StructuredFields};
    let mut fields = StructuredFields::default();
    let _ = fields.insert(
        "restore_backend",
        FieldValue::String("standalone-recovery".to_string()),
    );
    let _ = fields.insert("stage", FieldValue::String(stage.to_string()));
    let _ = fields.insert(
        "reason_class",
        FieldValue::String("restore_verification_failed".to_string()),
    );
    let _ = fields.insert("journal_pending", FieldValue::Boolean(true));
    crate::emit_lifecycle_event(
        emitter,
        LifecycleEventKind::DisplayRestoreFailed,
        correlation_id,
        fields,
    );
}

#[cfg(all(windows, debug_assertions))]
pub fn run_live_watchdog_crash_test() -> Result<(), String> {
    if std::env::var("ARCEN_LIVE_WATCHDOG_TEST").as_deref() != Ok("1") {
        return Err(
            "set ARCEN_LIVE_WATCHDOG_TEST=1 to authorize the crash-recovery test".to_string(),
        );
    }
    crate::input::initialize_process_dpi_awareness();
    let mut request = DisplayRequest::new(3600, 2338)?;
    request.refresh_hz = 60;
    request.width_mm = 344.0;
    request.height_mm = 223.0;
    request.scale = 2.0;
    request.product_id = 0x3600;
    request.serial = 0x2338;
    let manager = DisplayManager::default();
    let display = manager.acquire(
        OutputSelector::GlobalIndex(0),
        request,
        DisplayPolicy::ExactIsolated,
        crate::eventlog::random_correlation_id(),
    )?;
    if !display.report().exact {
        return Err(format!(
            "crash-recovery test requires exact 3600x2338 mode, got {}",
            display.report().applied
        ));
    }
    if !crate::recovery::default_path().exists() {
        return Err("crash-recovery test did not arm its recovery journal".to_string());
    }
    // SAFETY: this test-only path deliberately terminates its own process to
    // prove that the independently armed watchdog performs crash recovery.
    unsafe {
        windows::Win32::System::Threading::TerminateProcess(
            windows::Win32::System::Threading::GetCurrentProcess(),
            137,
        )
    }
    .map_err(|error| format!("force-terminate crash-recovery test process: {error}"))?;
    Err("forced process termination unexpectedly returned".to_string())
}

#[cfg(not(windows))]
struct NativeBackend {
    request: DisplayRequest,
}

#[cfg(not(windows))]
impl NativeBackend {
    fn new(
        request: DisplayRequest,
        _session_log_id: arcen_telemetry::CorrelationId,
        _deskside: Option<crate::recovery::DesksideRecoveryEntry>,
    ) -> Self {
        Self { request }
    }
}

#[cfg(not(windows))]
impl DisplayBackend for NativeBackend {
    type Snapshot = ();

    fn select_target(&mut self, _selector: &OutputSelector) -> Result<DisplayTarget, String> {
        Err("native Windows display management is unavailable on this platform".to_string())
    }

    fn snapshot(&mut self, _target: &DisplayTarget) -> Result<Self::Snapshot, String> {
        unreachable!()
    }

    fn current(&mut self, _target: &DisplayTarget) -> Result<ModeState, String> {
        unreachable!()
    }

    fn supported_sizes(&mut self, _target: &DisplayTarget) -> Result<Vec<DisplaySize>, String> {
        unreachable!()
    }

    fn test_mode(&mut self, _target: &DisplayTarget, _size: DisplaySize) -> Result<(), String> {
        unreachable!()
    }

    fn apply_mode(
        &mut self,
        _target: &DisplayTarget,
        _size: DisplaySize,
    ) -> Result<ModeState, String> {
        unreachable!()
    }

    fn isolate_topology(&mut self, _target: &DisplayTarget) -> Result<ModeState, String> {
        unreachable!()
    }

    fn restore(
        &mut self,
        _target: &DisplayTarget,
        _snapshot: &Self::Snapshot,
    ) -> Result<ModeState, String> {
        unreachable!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    /// `Scale120` is the plan's canonical scale; `EdidRequest::scale` wants a
    /// plain ratio. A wrong conversion here is invisible in logs and shows up
    /// only as a remote desktop that is the wrong physical size.
    #[test]
    fn scale120_converts_to_the_plain_ratio_edid_expects() {
        let ratio = |units| {
            edid_scale_ratio(arcen_media::Scale120::new(units).expect("valid scale")).unwrap()
        };
        assert!((ratio(120) - 1.0).abs() < f32::EPSILON, "120/120 is 100%");
        assert!((ratio(240) - 2.0).abs() < f32::EPSILON, "240/120 is 200%");
        assert!((ratio(180) - 1.5).abs() < f32::EPSILON, "180/120 is 150%");
        assert!((ratio(300) - 2.5).abs() < f32::EPSILON, "300/120 is 250%");
    }

    #[test]
    fn effective_dpi_converts_to_windows_scale_percent() {
        assert_eq!(effective_scale_percent_from_dpi(96, 96), Some(100));
        assert_eq!(effective_scale_percent_from_dpi(120, 120), Some(125));
        assert_eq!(effective_scale_percent_from_dpi(144, 144), Some(150));
        assert_eq!(effective_scale_percent_from_dpi(192, 192), Some(200));
        assert_eq!(effective_scale_percent_from_dpi(0, 96), None);
    }

    #[test]
    fn requested_scale_percent_uses_shared_scale120_domain() {
        assert_eq!(
            requested_scale_percent(arcen_media::scale120_from_scale(1.0).expect("1x")),
            100
        );
        assert_eq!(
            requested_scale_percent(arcen_media::scale120_from_scale(1.25).expect("1.25x")),
            125
        );
        assert_eq!(
            requested_scale_percent(arcen_media::scale120_from_scale(2.0).expect("2x")),
            200
        );
    }

    #[test]
    fn requested_scale_percent_uses_the_synthesized_physical_size() {
        let mut request = DisplayRequest::new(3008, 1692).unwrap();
        request.scale = 2.0;
        request.width_mm = 3008.0 * 25.4 / 96.0;
        request.height_mm = 1692.0 * 25.4 / 96.0;
        assert_eq!(requested_scale_percent_from_request(request).unwrap(), 100);

        request.size = DisplaySize::validate(6016, 3384).unwrap();
        assert_eq!(requested_scale_percent_from_request(request).unwrap(), 200);
    }

    #[test]
    fn requested_scale_percent_rejects_non_square_physical_metadata() {
        let mut request = DisplayRequest::new(3008, 1692).unwrap();
        request.width_mm = 3008.0 * 25.4 / 96.0;
        request.height_mm = 100.0;
        assert!(requested_scale_percent_from_request(request)
            .unwrap_err()
            .contains("non-square"));
    }

    #[cfg(windows)]
    #[test]
    fn relative_dpi_scale_maps_measured_250_percent_to_requested_100_percent() {
        assert_eq!(
            windows_backend::requested_relative_scale(250, 100, 0, -6, 0).unwrap(),
            -6
        );
        assert_eq!(
            windows_backend::requested_relative_scale(100, 200, -6, -6, 0).unwrap(),
            -2
        );
        assert!(
            windows_backend::requested_relative_scale(250, 500, 0, -6, 0)
                .unwrap_err()
                .contains("outside Windows range")
        );
        assert!(
            windows_backend::requested_relative_scale(100, 100, 10, -11, 11)
                .unwrap_err()
                .contains("outside the supported table")
        );
    }

    #[test]
    fn scale_match_policy_warns_only_beyond_tolerance() {
        assert!(display_scale_matches_requested(200, 205));
        assert!(display_scale_matches_requested(200, 195));
        assert!(!display_scale_matches_requested(200, 100));
        assert!(!display_scale_matches_requested(150, 100));
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "requires elevated interactive console and pier-windows.example.internal V100D vGPU"]
    fn native_nvidia_headless_three_output_transaction_restores_exact_baseline() {
        use arcen_media::{
            Monitor, MonitorIdentity, RequestedMonitor, RequestedMonitorTopology, Rotation,
            TopologyGeneration,
        };

        const ADAPTER: &str = "NVIDIA GRID V100D-16Q";
        require_nvapi_headless_probe_context().expect("elevated local console");
        crate::logging::init(
            arcen_telemetry::OperationalProfile::Debug,
            crate::logging::COMPONENT_DIAGNOSTIC,
            None,
            false,
        )
        .expect("lab test logging");
        let baseline_nvapi = crate::nvapi_inventory::inventory().expect("baseline NVAPI");
        let baseline_edids = baseline_nvapi
            .gpus
            .iter()
            .find(|gpu| gpu.full_name.as_deref() == Some("GRID V100D-16Q"))
            .expect("V100 inventory")
            .displays
            .iter()
            .map(|display| {
                (
                    display.display_id,
                    display.flags.connected,
                    display.flags.active,
                    display.edid.sha256.clone(),
                )
            })
            .collect::<Vec<_>>();
        let baseline =
            crate::gpu_probe::physical_output_inventory(&[ADAPTER.to_string()]).expect("baseline");
        assert_eq!(baseline.len(), 1, "lab starts with one active V100 output");

        let manager = DisplayManager::default();
        let owner = crate::eventlog::random_correlation_id();
        // Deliberately not 100%. At 100% a display that carries no scale at
        // all still lands on 96 DPI, so the trivial case passes even when the
        // requested scale is being discarded -- which is exactly how the
        // pier-windows.example.internal defect survived. 200% is the scale the field report
        // asked for and did not get.
        const REQUESTED_SCALE_PERCENT: u16 = 200;
        let requested_scale = arcen_media::Scale120::new(240).expect("scale");
        let contracts = (0..3)
            .map(|index| crate::nvapi_headless::HeadlessDisplayContract {
                width: 2560,
                height: 1440,
                refresh_hz: 60,
                width_mm: 0.0,
                height_mm: 0.0,
                scale: requested_scale,
                product_id: 0x0001,
                serial: index,
                hdr10: false,
                primary: index == 0,
                preferred_output_index: None,
            })
            .collect();
        let planning = manager
            .prepare_nvidia_headless_multi(ADAPTER, contracts, owner.clone())
            .expect("provision three V100 outputs");
        let inventory =
            crate::gpu_probe::physical_output_inventory(&[ADAPTER.to_string()]).expect("expanded");
        assert_eq!(inventory.len(), 3);
        let requested = RequestedMonitorTopology::new(
            (0..3)
                .map(|index| {
                    RequestedMonitor::new(
                        Monitor {
                            identity: MonitorIdentity {
                                id: format!("smoke-{index}"),
                                name: format!("Smoke {index}"),
                                ..MonitorIdentity::default()
                            },
                            // Logical origin, so it advances by the logical
                            // width (1280) rather than the physical one.
                            x: index * 1280,
                            y: 0,
                            // 2560x1440, deliberately, and not the 1920x1080
                            // this test used to use. Windows caps a display's
                            // recommended scale by how small it would leave
                            // the logical desktop: at 1920x1080 a 200%
                            // request is clamped to 150%, because that would
                            // leave 960x540. Measured directly on this host.
                            // 2560x1440 at 200% leaves 1280x720 -- exactly
                            // the logical size Windows was willing to hand
                            // back at that clamp -- so 200% is genuinely
                            // reachable here and the assertion below tests
                            // the fix rather than Windows' clamp. Three of
                            // them also stay inside the 8192px desktop bound,
                            // which three 4K panels do not.
                            width_px: 2560,
                            height_px: 1440,
                            scale: 2.0,
                            refresh_hz: 60,
                            rotation: Rotation::Degrees0,
                            primary: index == 0,
                            width_mm: 0.0,
                            height_mm: 0.0,
                        },
                        1280,
                        720,
                    )
                    .expect("monitor")
                })
                .collect(),
        )
        .expect("requested topology");
        let plan = crate::multi_monitor_topology::plan_topology(
            &requested,
            TopologyGeneration::new(1).expect("generation"),
            &inventory,
        )
        .expect("three-output plan");
        let mut lease = planning.acquire(&plan, owner).expect("physical provider");
        let applied_plan = lease.applied_plan().expect("applied plan");
        assert_eq!(applied_plan.monitors.len(), 3);

        // The whole point of the fix: the scale the client asked for has to
        // survive into the synthesized EDID and come back out of Windows.
        // `effective_scale_report` reads it live via GetDpiForMonitor, so this
        // is Windows' own answer, not Arcen's bookkeeping.
        let scale_reports = windows_backend::effective_scale_reports(applied_plan);
        assert_eq!(
            scale_reports.len(),
            3,
            "every applied monitor must report an effective scale",
        );
        for report in &scale_reports {
            assert_eq!(
                report.requested_scale_percent, REQUESTED_SCALE_PERCENT,
                "the plan must carry the requested scale for {}",
                report.device_name,
            );
            assert!(
                report.matches_requested,
                "Windows resolved {}% on {} for a requested {}% (effective DPI {}x{}); the \
                 requested scale is not reaching Windows",
                report.effective_scale_percent,
                report.device_name,
                report.requested_scale_percent,
                report.effective_dpi_x,
                report.effective_dpi_y,
            );
        }

        lease
            .restore()
            .expect("combined topology and EDID rollback");
        assert!(!crate::recovery::default_path().exists());
        let restored =
            crate::gpu_probe::physical_output_inventory(&[ADAPTER.to_string()]).expect("restored");
        assert_eq!(restored.len(), baseline.len());
        let restored_nvapi = crate::nvapi_inventory::inventory().expect("restored NVAPI");
        let restored_edids = restored_nvapi
            .gpus
            .iter()
            .find(|gpu| gpu.full_name.as_deref() == Some("GRID V100D-16Q"))
            .expect("restored V100 inventory")
            .displays
            .iter()
            .map(|display| {
                (
                    display.display_id,
                    display.flags.connected,
                    display.flags.active,
                    display.edid.sha256.clone(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(restored_edids, baseline_edids);
    }

    fn attached_output(global: u32, adapter: &str, vendor_id: u32) -> ResolvedOutput {
        ResolvedOutput {
            global_index: global,
            adapter_name: adapter.to_string(),
            adapter_output_index: 0,
            device_name: format!(r"\\.\DISPLAY{}", global + 1),
            vendor_id,
            desktop_rect: DesktopRect {
                left: 0,
                top: 0,
                width: 1280,
                height: 800,
            },
        }
    }

    const EMULATED_VENDOR_ID: u32 = 0x1414;

    #[test]
    fn positional_index_on_an_emulated_adapter_moves_to_the_gpu() {
        // Attaching another display (a hypervisor console, another agent's
        // virtual monitor) slides global index 0 off the GPU. Silently losing
        // NVENC that way also loses the exact-display policy the client's
        // display modes depend on.
        let outputs = [
            attached_output(0, "Microsoft Basic Render Driver", EMULATED_VENDOR_ID),
            attached_output(1, "NVIDIA GRID V100D-16Q", NVIDIA_VENDOR_ID),
        ];
        let preferred = prefer_encode_capable_output(
            &OutputSelector::GlobalIndex(0),
            &outputs[0],
            &outputs,
            DisplaySize {
                width: 1280,
                height: 800,
            },
        )
        .expect("an attached NVIDIA output must be preferred");
        assert_eq!(preferred.adapter_name, "NVIDIA GRID V100D-16Q");
        assert_eq!(preferred.global_index, 1);
    }

    #[test]
    fn an_explicitly_named_adapter_is_never_second_guessed() {
        let outputs = [
            attached_output(0, "Microsoft Basic Render Driver", EMULATED_VENDOR_ID),
            attached_output(1, "NVIDIA GRID V100D-16Q", NVIDIA_VENDOR_ID),
        ];
        assert!(
            prefer_encode_capable_output(
                &OutputSelector::Adapter {
                    name: "Microsoft Basic Render Driver".to_string(),
                    output_index: 0,
                },
                &outputs[0],
                &outputs,
                DisplaySize {
                    width: 1280,
                    height: 800,
                },
            )
            .is_none(),
            "an operator instruction must be obeyed, not corrected"
        );
    }

    #[test]
    fn an_output_already_on_the_gpu_is_left_alone() {
        let outputs = [
            attached_output(0, "NVIDIA GRID V100D-16Q", NVIDIA_VENDOR_ID),
            attached_output(1, "NVIDIA GRID RTX6000-8Q", NVIDIA_VENDOR_ID),
        ];
        assert!(
            prefer_encode_capable_output(
                &OutputSelector::GlobalIndex(0),
                &outputs[0],
                &outputs,
                DisplaySize {
                    width: 1280,
                    height: 800,
                },
            )
            .is_none(),
            "no move when the selected output can already encode"
        );
    }

    #[test]
    fn a_host_with_no_gpu_output_keeps_what_it_has() {
        let outputs = [attached_output(
            0,
            "Microsoft Basic Render Driver",
            EMULATED_VENDOR_ID,
        )];
        assert!(
            prefer_encode_capable_output(
                &OutputSelector::GlobalIndex(0),
                &outputs[0],
                &outputs,
                DisplaySize {
                    width: 1280,
                    height: 800,
                },
            )
            .is_none(),
            "nothing better exists; the session must still run"
        );
    }

    #[test]
    fn automatic_selection_prefers_the_nvidia_output_matching_the_client_size() {
        let mut first = attached_output(0, "NVIDIA GRID RTX6000-8Q", NVIDIA_VENDOR_ID);
        first.desktop_rect.width = 1800;
        first.desktop_rect.height = 1130;
        let mut second = attached_output(1, "NVIDIA GRID V100D-16Q", NVIDIA_VENDOR_ID);
        second.desktop_rect.width = 3008;
        second.desktop_rect.height = 1692;
        let outputs = [first, second];
        let preferred = prefer_encode_capable_output(
            &OutputSelector::GlobalIndex(0),
            &outputs[0],
            &outputs,
            DisplaySize {
                width: 3008,
                height: 1692,
            },
        )
        .expect("the NVIDIA output already presenting the requested size should win");
        assert_eq!(preferred.adapter_name, "NVIDIA GRID V100D-16Q");
    }

    #[test]
    fn complete_topology_verification_rejects_any_path_or_mode_change() {
        let paths = [1_u8, 2, 3, 4, 5, 6];
        let modes = [7_u8, 8, 9, 10];
        assert!(complete_topology_bytes_match(
            &paths, &modes, &paths, &modes
        ));

        let mut changed_identity_or_path = paths;
        changed_identity_or_path[2] ^= 1;
        assert!(!complete_topology_bytes_match(
            &paths,
            &modes,
            &changed_identity_or_path,
            &modes
        ));

        let mut changed_position_or_mode = modes;
        changed_position_or_mode[3] ^= 1;
        assert!(!complete_topology_bytes_match(
            &paths,
            &modes,
            &paths,
            &changed_position_or_mode
        ));
        assert!(!complete_topology_bytes_match(
            &paths,
            &modes,
            &paths[..paths.len() - 1],
            &modes
        ));
    }

    #[test]
    fn legacy_schema_is_rejected_before_unmutated_journal_cleanup() {
        let error = require_stable_recovery_schema(3, false).unwrap_err();
        assert!(error.contains("migrate-display-journal"));
        assert!(require_stable_recovery_schema(4, true).is_ok());
        assert!(require_stable_recovery_schema(4, false).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn native_topology_is_exact_or_unavailable_fail_closed() {
        let topology = match windows_backend::query_active_topology() {
            Ok(topology) => topology,
            Err(error) => {
                assert!(!error.is_empty(), "native topology error must be explicit");
                return;
            }
        };
        assert!(complete_topology_bytes_match(
            windows_backend::as_bytes(&topology.paths),
            windows_backend::as_bytes(&topology.modes),
            windows_backend::as_bytes(&topology.paths),
            windows_backend::as_bytes(&topology.modes),
        ));

        let mut changed_paths = topology.paths.clone();
        changed_paths[0].sourceInfo.id ^= 1;
        assert!(!complete_topology_bytes_match(
            windows_backend::as_bytes(&topology.paths),
            windows_backend::as_bytes(&topology.modes),
            windows_backend::as_bytes(&changed_paths),
            windows_backend::as_bytes(&topology.modes),
        ));
    }

    #[cfg(windows)]
    #[test]
    fn native_semantic_topology_ignores_only_boot_local_identifiers() {
        let topology = match windows_backend::query_active_topology() {
            Ok(topology) => topology,
            Err(error) => {
                assert!(!error.is_empty(), "native topology error must be explicit");
                return;
            }
        };
        let mut rebound_paths = topology.paths.clone();
        let mut rebound_modes = topology.modes.clone();
        for (index, (original, rebound)) in topology
            .paths
            .iter()
            .zip(rebound_paths.iter_mut())
            .enumerate()
        {
            let new_luid = windows::Win32::Foundation::LUID {
                LowPart: 10_000 + index as u32,
                HighPart: 0,
            };
            for mode in &mut rebound_modes {
                let same_source = mode.infoType
                    == windows::Win32::Devices::Display::DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE
                    && mode.id == original.sourceInfo.id
                    && mode.adapterId.LowPart == original.sourceInfo.adapterId.LowPart
                    && mode.adapterId.HighPart == original.sourceInfo.adapterId.HighPart;
                let same_target = mode.infoType
                    == windows::Win32::Devices::Display::DISPLAYCONFIG_MODE_INFO_TYPE_TARGET
                    && mode.id == original.targetInfo.id
                    && mode.adapterId.LowPart == original.targetInfo.adapterId.LowPart
                    && mode.adapterId.HighPart == original.targetInfo.adapterId.HighPart;
                if same_source || same_target {
                    mode.adapterId = new_luid;
                    mode.id = mode.id.wrapping_add(100);
                }
            }
            rebound.sourceInfo.adapterId = new_luid;
            rebound.sourceInfo.id = rebound.sourceInfo.id.wrapping_add(100);
            rebound.targetInfo.adapterId = new_luid;
            rebound.targetInfo.id = rebound.targetInfo.id.wrapping_add(100);
        }
        assert!(windows_backend::complete_topology_semantically_matches(
            &topology.paths,
            &topology.modes,
            &rebound_paths,
            &rebound_modes,
        )
        .unwrap());
        let current_indexes = (0..rebound_paths.len()).collect::<Vec<_>>();
        let (rebuilt_paths, rebuilt_modes) = windows_backend::rebind_topology_boot_identifiers(
            &topology.paths,
            &topology.modes,
            &rebound_paths,
            &current_indexes,
        )
        .unwrap();
        assert!(windows_backend::complete_topology_semantically_matches(
            &topology.paths,
            &topology.modes,
            &rebuilt_paths,
            &rebuilt_modes,
        )
        .unwrap());
        assert!(rebuilt_paths
            .iter()
            .zip(&rebound_paths)
            .all(
                |(rebuilt, current)| rebuilt.sourceInfo.id == current.sourceInfo.id
                    && rebuilt.targetInfo.id == current.targetInfo.id
                    && rebuilt.sourceInfo.adapterId.LowPart == current.sourceInfo.adapterId.LowPart
                    && rebuilt.sourceInfo.adapterId.HighPart
                        == current.sourceInfo.adapterId.HighPart
            ));
        let stable = crate::recovery::StableTopologySnapshot {
            paths: (0..topology.paths.len())
                .map(|index| crate::recovery::StableOutputIdentity {
                    adapter_stable_id: format!("adapter-{index}"),
                    monitor_device_path: format!("monitor-{index}"),
                    adapter_output_index: index as u32,
                    output_technology: 4,
                    connector_instance: index as u32,
                    edid_manufacture_id: index as u16,
                    edid_product_code_id: index as u16,
                    edid_sha256: Some(format!("{index:064x}")),
                    binding: crate::recovery::StableOutputBackend::Nvidia {
                        nvapi_display_id: index as u32 + 1,
                        nvapi_output_id: 1_u32 << index,
                        nvapi_head: index as u32,
                    },
                })
                .collect(),
        };
        assert!(windows_backend::complete_topology_with_stable_identities(
            &topology.paths,
            &topology.modes,
            &stable,
            &rebound_paths,
            &rebound_modes,
            &stable,
        )
        .unwrap());
        let mut wrong_identity = stable.clone();
        wrong_identity.paths[0]
            .monitor_device_path
            .push_str("-wrong");
        assert!(!windows_backend::complete_topology_with_stable_identities(
            &topology.paths,
            &topology.modes,
            &stable,
            &rebound_paths,
            &rebound_modes,
            &wrong_identity,
        )
        .unwrap());

        let mut changed_clone_paths = rebound_paths.clone();
        // SAFETY: the path came from QDC_VIRTUAL_MODE_AWARE.
        unsafe {
            let bits = changed_clone_paths[0]
                .sourceInfo
                .Anonymous
                .Anonymous
                ._bitfield;
            changed_clone_paths[0]
                .sourceInfo
                .Anonymous
                .Anonymous
                ._bitfield = (bits & 0xFFFF_0000) | ((bits.wrapping_add(1)) & 0xFFFF);
        }
        assert!(!windows_backend::complete_topology_semantically_matches(
            &topology.paths,
            &topology.modes,
            &changed_clone_paths,
            &rebound_modes,
        )
        .unwrap());

        let mut changed_desktop_modes = rebound_modes.clone();
        if let Some(desktop_mode) = changed_desktop_modes.iter_mut().find(|mode| {
            mode.infoType
                == windows::Win32::Devices::Display::DISPLAYCONFIG_MODE_INFO_TYPE_DESKTOP_IMAGE
        }) {
            // SAFETY: the mode was selected by its DESKTOP_IMAGE discriminator;
            // mutate one byte of its desktop-image payload.
            unsafe {
                let payload = (desktop_mode
                    as *mut windows::Win32::Devices::Display::DISPLAYCONFIG_MODE_INFO)
                    .cast::<u8>()
                    .add(16);
                *payload ^= 1;
            }
            assert!(!windows_backend::complete_topology_semantically_matches(
                &topology.paths,
                &topology.modes,
                &rebound_paths,
                &changed_desktop_modes,
            )
            .unwrap());
        }

        let source_mode = rebound_modes
            .iter_mut()
            .find(|mode| {
                mode.infoType
                    == windows::Win32::Devices::Display::DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE
            })
            .expect("active topology has a source mode");
        // SAFETY: the mode was selected by its SOURCE discriminator.
        unsafe {
            source_mode.Anonymous.sourceMode.position.x += 1;
        }
        assert!(!windows_backend::complete_topology_semantically_matches(
            &topology.paths,
            &topology.modes,
            &rebound_paths,
            &rebound_modes,
        )
        .unwrap());
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "requires an elevated exact local-console session"]
    fn native_legacy_migration_context_guard_accepts_interactive_console_admin() {
        windows_backend::require_legacy_migration_context()
            .expect("interactive native harness has exact elevated console context");
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "requires an elevated exact local-console session"]
    fn native_stable_bindings_select_only_the_host_display_backends() {
        let topology =
            windows_backend::query_active_topology().expect("query active Windows topology");
        let stable =
            windows_backend::capture_stable_topology(&topology).expect("capture stable bindings");
        let has_nvidia = stable.paths.iter().any(|identity| {
            matches!(
                &identity.binding,
                crate::recovery::StableOutputBackend::Nvidia { .. }
            )
        });
        let inventory = crate::gpu_probe::probe().expect("probe native adapters");
        let inventory_has_nvidia = inventory.adapters.iter().any(|adapter| {
            adapter.vendor_id == 0x10de
                && adapter
                    .outputs
                    .iter()
                    .any(|output| output.attached_to_desktop)
        });
        assert_eq!(has_nvidia, inventory_has_nvidia);
        if !inventory_has_nvidia {
            assert!(stable.paths.iter().all(|identity| {
                matches!(
                    &identity.binding,
                    crate::recovery::StableOutputBackend::WindowsNative
                )
            }));
        }
        let (rebuilt_paths, rebuilt_modes, selected_device) =
            windows_backend::reconstruct_current_topology(
                &topology.paths,
                &topology.modes,
                &stable,
                0,
            )
            .expect("rebind active journal semantics through current all-path inventory");
        assert!(!selected_device.is_empty());
        assert!(windows_backend::complete_topology_semantically_matches(
            &topology.paths,
            &topology.modes,
            &rebuilt_paths,
            &rebuilt_modes,
        )
        .unwrap());
    }

    #[test]
    fn stale_recovery_journal_blocks_a_new_display_owner() {
        let path = std::path::Path::new(r"C:\ProgramData\Arcen\runtime\display-recovery.json");
        let error = ensure_recovery_journal_clear(true, path).unwrap_err();
        assert!(error.contains("display recovery journal"));
        assert!(error.contains("restore-display"));
        assert!(ensure_recovery_journal_clear(false, path).is_ok());
    }

    #[cfg(windows)]
    #[test]
    fn an_absent_journal_needs_no_recovery_and_touches_nothing() {
        // The ordinary path: no journal, so a session starts without the
        // recovery code doing any display work at all.
        let directory = std::env::temp_dir().join(format!(
            "arcen-journal-absent-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&directory).expect("scratch directory");
        let path = directory.join("display-recovery.json");
        assert!(!path.exists());
        assert!(recover_pending_journal(&path).is_ok());
        assert!(
            !path.exists(),
            "recovery must not create a journal where there was none"
        );
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[cfg(windows)]
    #[test]
    fn an_unreadable_journal_is_set_aside_rather_than_blocking_for_ever() {
        // A journal that cannot be applied must stop gating sessions, because
        // the documented remedy needs an interactive desktop and is therefore
        // unreachable on exactly the hosts this product exists to reach. It
        // must still be kept: it is the only description of what the stranded
        // session changed.
        let directory = std::env::temp_dir().join(format!(
            "arcen-journal-bad-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&directory).expect("scratch directory");
        let path = directory.join("display-recovery.json");
        std::fs::write(&path, b"{ not a journal").expect("write scratch journal");

        recover_pending_journal(&path).expect("an unapplicable journal must not block");
        assert!(!path.exists(), "the journal must stop gating new sessions");

        let kept = std::fs::read_dir(&directory)
            .expect("scratch directory")
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains("unrestorable"))
            .count();
        assert_eq!(kept, 1, "the record must be preserved, not deleted");
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[cfg(windows)]
    #[test]
    fn standalone_restore_lifecycle_emitters_never_panic_on_a_disabled_emitter() {
        let emitter = crate::LifecycleEmitter::disabled();
        let correlation_id = crate::eventlog::random_correlation_id();
        for outcome in [
            windows_backend::RestoreOutcome::AlreadyClean,
            windows_backend::RestoreOutcome::Restored {
                restore_backend: "set-display-config-plus-exact-devmode",
                width: 1920,
                height: 1080,
            },
        ] {
            emit_cli_restore_outcome(&emitter, correlation_id.clone(), outcome);
            emit_watchdog_restore_outcome(&emitter, correlation_id.clone(), outcome);
        }
        emit_display_restore_failed(&emitter, correlation_id.clone(), "standalone_restore");
        emit_display_restore_failed(&emitter, correlation_id, "watchdog_restore");
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum Call {
        Select(u32),
        Snapshot,
        Current,
        Supported,
        Test(DisplaySize),
        PrepareRetarget(DisplaySize),
        Arm,
        Apply(DisplaySize),
        Isolate,
        Restore,
        Disarm,
    }

    struct FakeBackend {
        calls: Arc<Mutex<Vec<Call>>>,
        current: ModeState,
        supported: Vec<DisplaySize>,
        tests: VecDeque<Result<(), String>>,
        retarget_preparations: VecDeque<Result<(), String>>,
        applies: VecDeque<Result<ModeState, String>>,
        isolates: VecDeque<Result<ModeState, String>>,
        restores: VecDeque<Result<ModeState, String>>,
        recovery_armed: bool,
        arm_error: Option<String>,
        contract_refresh_required: bool,
    }

    impl FakeBackend {
        // The fake models the mirroring host: two active outputs (Microsoft
        // Basic DISPLAY1 + the NVIDIA session output) before isolation.
        fn new(current: DisplaySize) -> Self {
            let state = mode(current, 0);
            Self {
                calls: Arc::new(Mutex::new(Vec::new())),
                current: state,
                supported: vec![current],
                tests: VecDeque::new(),
                retarget_preparations: VecDeque::new(),
                applies: VecDeque::new(),
                isolates: VecDeque::new(),
                restores: VecDeque::from([Ok(state)]),
                recovery_armed: false,
                arm_error: None,
                contract_refresh_required: false,
            }
        }

        fn record(&self, call: Call) {
            self.calls.lock().unwrap().push(call);
        }
    }

    impl DisplayBackend for FakeBackend {
        type Snapshot = ModeState;

        fn select_target(&mut self, selector: &OutputSelector) -> Result<DisplayTarget, String> {
            let output_index = match selector {
                OutputSelector::GlobalIndex(index) => *index,
                OutputSelector::Adapter { output_index, .. } => *output_index,
            };
            self.record(Call::Select(output_index));
            Ok(DisplayTarget {
                device_name: r"\\.\DISPLAY6".to_string(),
                vendor_id: 0x10de,
                adapter_luid: AdapterLuid::default(),
                adapter_output_index: 0,
            })
        }

        fn snapshot(&mut self, _target: &DisplayTarget) -> Result<Self::Snapshot, String> {
            self.record(Call::Snapshot);
            Ok(self.current)
        }

        fn current(&mut self, _target: &DisplayTarget) -> Result<ModeState, String> {
            self.record(Call::Current);
            Ok(self.current)
        }

        fn supported_sizes(&mut self, _target: &DisplayTarget) -> Result<Vec<DisplaySize>, String> {
            self.record(Call::Supported);
            Ok(self.supported.clone())
        }

        fn requires_contract_refresh(
            &self,
            _target: &DisplayTarget,
            _size: DisplaySize,
        ) -> Result<bool, String> {
            Ok(self.contract_refresh_required)
        }

        fn prepare_exact_retarget(
            &mut self,
            _target: &DisplayTarget,
            size: DisplaySize,
        ) -> Result<(), String> {
            self.record(Call::PrepareRetarget(size));
            self.retarget_preparations.pop_front().unwrap_or(Ok(()))
        }

        fn test_mode(&mut self, _target: &DisplayTarget, size: DisplaySize) -> Result<(), String> {
            self.record(Call::Test(size));
            self.tests.pop_front().unwrap_or(Ok(()))
        }

        fn apply_mode(
            &mut self,
            _target: &DisplayTarget,
            size: DisplaySize,
        ) -> Result<ModeState, String> {
            self.record(Call::Apply(size));
            let result = self.applies.pop_front().unwrap_or(Ok(mode(size, 0)));
            if let Ok(applied) = result {
                self.current = applied;
            }
            result
        }

        fn isolate_topology(&mut self, _target: &DisplayTarget) -> Result<ModeState, String> {
            self.record(Call::Isolate);
            let result = self
                .isolates
                .pop_front()
                .unwrap_or(Ok(isolated(self.current.size)));
            if let Ok(state) = result {
                self.current = state;
            }
            result
        }

        fn restore(
            &mut self,
            _target: &DisplayTarget,
            _snapshot: &Self::Snapshot,
        ) -> Result<ModeState, String> {
            self.record(Call::Restore);
            let result = self.restores.pop_front().unwrap_or(Ok(self.current));
            if let Ok(restored) = result {
                self.current = restored;
            }
            result
        }

        fn arm_recovery(
            &mut self,
            _target: &DisplayTarget,
            _snapshot: &Self::Snapshot,
        ) -> Result<(), String> {
            if !self.recovery_armed {
                self.record(Call::Arm);
                if let Some(error) = self.arm_error.take() {
                    return Err(error);
                }
                self.recovery_armed = true;
            }
            Ok(())
        }

        fn disarm_recovery(&mut self) -> Result<(), String> {
            if self.recovery_armed {
                self.record(Call::Disarm);
                self.recovery_armed = false;
            }
            Ok(())
        }
    }

    fn size(width: u32, height: u32) -> DisplaySize {
        DisplaySize { width, height }
    }

    fn mode(size: DisplaySize, output_index: u32) -> ModeState {
        ModeState {
            size,
            refresh_hz: 60,
            output_index,
            desktop_rect: DesktopRect {
                left: 0,
                top: 0,
                width: size.width as i32,
                height: size.height as i32,
            },
            active_outputs: 2,
        }
    }

    fn isolated(size: DisplaySize) -> ModeState {
        ModeState {
            active_outputs: 1,
            output_index: 0,
            ..mode(size, 0)
        }
    }

    #[test]
    fn validates_encoder_safe_client_dimensions() {
        assert_eq!(DisplaySize::validate(3600, 2338).unwrap(), size(3600, 2338));
        assert!(DisplaySize::validate(3599, 2338).is_err());
        assert_eq!(DisplaySize::validate(3600, 2337).unwrap(), size(3600, 2337));
        assert!(DisplaySize::validate(318, 240).is_err());
        assert!(DisplaySize::validate(320, 239).is_err());
        assert!(DisplaySize::validate(16_386, 1080).is_err());
        assert!(DisplaySize::validate(3840, 8642).is_err());
    }

    #[test]
    fn fallback_never_selects_an_encoder_unsafe_mode() {
        assert_eq!(
            choose_fallback(
                size(1920, 1080),
                size(321, 241),
                &[size(641, 481), size(20_000, 10_000)]
            ),
            None
        );
    }

    #[test]
    fn unchanged_mode_is_a_noop_and_does_not_restore() {
        let backend = FakeBackend::new(size(1920, 1080));
        let calls = Arc::clone(&backend.calls);
        let transaction = DisplayTransaction::acquire(backend, 3, size(1920, 1080)).unwrap();
        assert!(!transaction.report.changed);
        assert_eq!(transaction.report.capture_output_index, 0);
        drop(transaction);
        assert_eq!(
            *calls.lock().unwrap(),
            vec![Call::Select(3), Call::Snapshot, Call::Current]
        );
    }

    #[test]
    fn media_fallback_retargets_an_unchanged_exact_lease_transactionally() {
        let original = isolated(size(1920, 1080));
        let fallback = size(1920, 1072);
        let mut backend = FakeBackend::new(original.size);
        backend.current = original;
        backend.applies.push_back(Ok(isolated(fallback)));
        backend.restores = VecDeque::from([Ok(original)]);
        let calls = Arc::clone(&backend.calls);
        let mut transaction =
            DisplayTransaction::acquire_isolated(backend, 0, original.size).unwrap();
        transaction.report.retarget_capable = true;

        transaction.retarget_exact(fallback).unwrap();
        assert_eq!(transaction.report().applied, fallback);
        assert_eq!(transaction.report().desktop_rect.width, 1920);
        assert_eq!(transaction.report().desktop_rect.height, 1072);
        transaction.restore().unwrap();

        let calls = calls.lock().unwrap();
        assert!(calls.contains(&Call::Snapshot));
        assert!(calls.contains(&Call::PrepareRetarget(fallback)));
        assert!(calls.contains(&Call::Test(fallback)));
        assert!(calls.contains(&Call::Arm));
        assert!(calls.contains(&Call::Apply(fallback)));
        assert!(calls.contains(&Call::Isolate));
        assert!(calls.contains(&Call::Restore));
    }

    #[test]
    fn failed_exact_retarget_reapplies_previous_session_mode() {
        let original = isolated(size(1920, 1080));
        let requested = size(1120, 760);
        let mut backend = FakeBackend::new(original.size);
        backend.current = original;
        backend
            .applies
            .push_back(Err("NVAPI topology commit rejected".to_string()));
        backend.applies.push_back(Ok(original));
        let calls = Arc::clone(&backend.calls);
        let mut transaction =
            DisplayTransaction::acquire_isolated(backend, 0, original.size).unwrap();
        transaction.report.retarget_capable = true;

        let error = transaction.retarget_exact(requested).unwrap_err();

        assert!(error.contains("previous display mode was restored"));
        assert_eq!(transaction.report().applied, original.size);
        let calls = calls.lock().unwrap();
        assert!(calls.contains(&Call::Apply(requested)));
        assert!(calls.contains(&Call::Apply(original.size)));
    }

    #[cfg(windows)]
    #[test]
    fn nvidia_exact_backend_accepts_a_retargeted_media_geometry() {
        assert!(nvapi_exact_available(0x10de, false, true, true));
        assert!(!nvapi_exact_available(0x10de, true, true, true));
        assert!(!nvapi_exact_available(0x8086, false, true, true));
        assert!(require_nvapi_exact_retarget(0x10de, false, true, true).is_ok());
        assert!(require_nvapi_exact_retarget(0x10de, true, true, true).is_err());
        assert!(require_nvapi_exact_retarget(0x8086, false, false, false).is_ok());
    }

    #[cfg(windows)]
    #[test]
    fn nvapi_stage_read_failure_retains_active_timing_for_recovery() {
        let mut active = Some(7_u32);
        let error = take_nvapi_active_after_stage(&mut active, true, || {
            Err("injected journal read failure".to_string())
        })
        .unwrap_err();
        assert_eq!(error, "injected journal read failure");
        assert_eq!(active, Some(7));

        let taken = take_nvapi_active_after_stage(&mut active, true, || {
            Ok(crate::nvapi::CleanupStage::Pending)
        })
        .unwrap();
        assert_eq!(taken, Some((7, crate::nvapi::CleanupStage::Pending)));
        assert_eq!(active, None);
    }

    #[test]
    fn failed_exact_retarget_preparation_never_probes_cds_mode() {
        let original = isolated(size(1920, 1080));
        let fallback = size(1920, 1072);
        let mut backend = FakeBackend::new(original.size);
        backend.current = original;
        backend
            .retarget_preparations
            .push_back(Err("NVAPI exact retarget unavailable".to_string()));
        let calls = Arc::clone(&backend.calls);
        let mut transaction =
            DisplayTransaction::acquire_isolated(backend, 0, original.size).unwrap();
        transaction.report.retarget_capable = true;

        let error = transaction.retarget_exact(fallback).unwrap_err();
        assert!(error.contains("NVAPI exact retarget unavailable"));
        let calls = calls.lock().unwrap();
        assert!(calls.contains(&Call::PrepareRetarget(fallback)));
        assert!(!calls.contains(&Call::Test(fallback)));
        assert!(!calls.contains(&Call::Apply(fallback)));
    }

    #[test]
    fn matching_devmode_with_unsettled_geometry_is_not_a_noop() {
        let mut backend = FakeBackend::new(size(1920, 1080));
        backend.current.desktop_rect.width = 1680;
        backend.applies.push_back(Ok(mode(size(1920, 1080), 0)));
        let calls = Arc::clone(&backend.calls);

        let transaction = DisplayTransaction::acquire(backend, 0, size(1920, 1080)).unwrap();

        assert!(transaction.report.changed);
        assert!(transaction.report.exact);
        assert!(calls
            .lock()
            .unwrap()
            .contains(&Call::Apply(size(1920, 1080))));
    }

    #[test]
    fn exact_apply_and_restore_order_is_transactional() {
        let mut backend = FakeBackend::new(size(1680, 1050));
        backend.applies.push_back(Ok(mode(size(1920, 1080), 2)));
        let calls = Arc::clone(&backend.calls);
        let mut transaction = DisplayTransaction::acquire(backend, 1, size(1920, 1080)).unwrap();
        assert!(transaction.report.exact);
        assert_eq!(transaction.report.capture_output_index, 2);
        transaction.restore().unwrap();
        drop(transaction);
        assert_eq!(
            *calls.lock().unwrap(),
            vec![
                Call::Select(1),
                Call::Snapshot,
                Call::Current,
                Call::Test(size(1920, 1080)),
                Call::Arm,
                Call::Apply(size(1920, 1080)),
                Call::Restore,
                Call::Disarm,
            ]
        );
    }

    #[test]
    fn isolated_policy_runs_exact_apply_then_isolation_transactionally() {
        let mut backend = FakeBackend::new(size(1680, 1050));
        backend.applies.push_back(Ok(mode(size(1920, 1080), 2)));
        let calls = Arc::clone(&backend.calls);
        let mut transaction =
            DisplayTransaction::acquire_isolated(backend, 1, size(1920, 1080)).unwrap();
        assert!(transaction.report.exact);
        assert_eq!(
            transaction.report.capture_output_index, 0,
            "after isolation the session output is the only (first) desktop output"
        );
        assert_eq!(
            transaction.report.desktop_rect,
            DesktopRect {
                left: 0,
                top: 0,
                width: 1920,
                height: 1080
            }
        );
        transaction.restore().unwrap();
        drop(transaction);
        assert_eq!(
            *calls.lock().unwrap(),
            vec![
                Call::Select(1),
                Call::Snapshot,
                Call::Current,
                Call::Test(size(1920, 1080)),
                Call::Arm,
                Call::Apply(size(1920, 1080)),
                Call::Isolate,
                Call::Restore,
                Call::Disarm,
            ]
        );
    }

    #[test]
    fn isolated_policy_with_matching_size_still_isolates_a_multi_output_desktop() {
        let backend = FakeBackend::new(size(1920, 1080));
        let calls = Arc::clone(&backend.calls);
        let mut transaction =
            DisplayTransaction::acquire_isolated(backend, 0, size(1920, 1080)).unwrap();
        assert!(transaction.report.changed);
        assert!(transaction.report.exact);
        assert_eq!(
            transaction.report.backend,
            "set-display-config-topology-isolation"
        );
        transaction.restore().unwrap();
        drop(transaction);
        assert_eq!(
            *calls.lock().unwrap(),
            vec![
                Call::Select(0),
                Call::Snapshot,
                Call::Current,
                Call::Arm,
                Call::Isolate,
                Call::Restore,
                Call::Disarm,
            ],
            "a matching mode on a multi-output desktop must still isolate the session output"
        );
    }

    #[test]
    fn isolated_policy_refreshes_a_changed_contract_even_when_geometry_matches() {
        let mut backend = FakeBackend::new(size(1920, 1080));
        backend.contract_refresh_required = true;
        let calls = Arc::clone(&backend.calls);
        let mut transaction =
            DisplayTransaction::acquire_isolated(backend, 0, size(1920, 1080)).unwrap();
        assert!(transaction.report.changed);
        transaction.restore().unwrap();
        drop(transaction);
        assert_eq!(
            *calls.lock().unwrap(),
            vec![
                Call::Select(0),
                Call::Snapshot,
                Call::Current,
                Call::Test(size(1920, 1080)),
                Call::Arm,
                Call::Apply(size(1920, 1080)),
                Call::Isolate,
                Call::Restore,
                Call::Disarm,
            ],
            "a changed EDID contract must not take the matching-mode isolation shortcut"
        );
    }

    #[test]
    fn isolated_policy_noop_requires_an_already_isolated_topology() {
        let mut backend = FakeBackend::new(size(1920, 1080));
        backend.current = isolated(size(1920, 1080));
        let calls = Arc::clone(&backend.calls);
        let transaction =
            DisplayTransaction::acquire_isolated(backend, 3, size(1920, 1080)).unwrap();
        assert!(!transaction.report.changed);
        drop(transaction);
        assert_eq!(
            *calls.lock().unwrap(),
            vec![Call::Select(3), Call::Snapshot, Call::Current]
        );
    }

    #[test]
    fn isolated_policy_refreshes_a_changed_contract_on_an_already_isolated_desktop() {
        let mut backend = FakeBackend::new(size(1920, 1080));
        backend.current = isolated(size(1920, 1080));
        backend.restores = VecDeque::from([Ok(backend.current)]);
        backend.contract_refresh_required = true;
        let calls = Arc::clone(&backend.calls);
        let mut transaction =
            DisplayTransaction::acquire_isolated(backend, 3, size(1920, 1080)).unwrap();
        assert!(transaction.report.changed);
        transaction.restore().unwrap();
        drop(transaction);
        assert_eq!(
            *calls.lock().unwrap(),
            vec![
                Call::Select(3),
                Call::Snapshot,
                Call::Current,
                Call::Test(size(1920, 1080)),
                Call::Arm,
                Call::Apply(size(1920, 1080)),
                Call::Isolate,
                Call::Restore,
                Call::Disarm,
            ],
            "a changed EDID contract must not take the fully-satisfied no-op shortcut"
        );
    }

    #[test]
    fn isolated_policy_refuses_rejected_exact_mode_without_fallback() {
        let mut backend = FakeBackend::new(size(1680, 1050));
        backend.tests.push_back(Err("BADMODE".to_string()));
        let calls = Arc::clone(&backend.calls);

        let error = match DisplayTransaction::acquire_isolated(backend, 0, size(3600, 2338)) {
            Ok(_) => panic!("a rejected exact mode must refuse the strict session"),
            Err(error) => error,
        };

        assert!(error.contains("cannot present the exact client display"));
        assert!(error.contains("3600x2338"));
        assert!(error.contains("BADMODE"));
        let locked_calls = calls.lock().unwrap();
        assert!(
            !locked_calls.iter().any(|call| matches!(
                call,
                Call::Apply(_) | Call::Isolate | Call::Restore | Call::Supported
            )),
            "strict refusal must not mutate, flicker, or negotiate fallback modes"
        );
    }

    #[test]
    fn isolated_policy_rolls_back_and_refuses_on_isolation_failure() {
        let mut backend = FakeBackend::new(size(1680, 1050));
        backend.applies.push_back(Ok(mode(size(1920, 1080), 0)));
        backend.isolates.push_back(Err(
            "SetDisplayConfig topology isolation returned -1".to_string()
        ));
        let calls = Arc::clone(&backend.calls);
        let restore_state = Arc::new(Mutex::new(RestoreJournal {
            state: RestoreState::Clean,
            last_failure: None,
        }));

        let error = match DisplayTransaction::acquire_observed(
            backend,
            &OutputSelector::GlobalIndex(0),
            size(1920, 1080),
            DisplayPolicy::ExactIsolated,
            Some(Arc::clone(&restore_state)),
        ) {
            Ok(_) => panic!("a failed topology isolation must refuse the session"),
            Err(error) => error,
        };

        assert!(error.contains("cannot present the exact client display"));
        assert!(error.contains("topology isolation"));
        let locked_calls = calls.lock().unwrap();
        let isolate = locked_calls
            .iter()
            .position(|call| *call == Call::Isolate)
            .unwrap();
        let rollback = locked_calls
            .iter()
            .position(|call| *call == Call::Restore)
            .unwrap();
        assert!(isolate < rollback);
        assert!(locked_calls.contains(&Call::Disarm));
        assert_eq!(
            lock_restore_state(&restore_state).state,
            RestoreState::Clean
        );
    }

    #[test]
    fn isolated_policy_refuses_isolation_that_settles_with_extra_outputs() {
        let mut backend = FakeBackend::new(size(1680, 1050));
        backend.applies.push_back(Ok(mode(size(1920, 1080), 0)));
        // Isolation "succeeds" but a second output is still active.
        backend.isolates.push_back(Ok(mode(size(1920, 1080), 0)));
        let calls = Arc::clone(&backend.calls);

        let error = match DisplayTransaction::acquire_isolated(backend, 0, size(1920, 1080)) {
            Ok(_) => panic!("isolation settling with extra outputs must refuse the session"),
            Err(error) => error,
        };

        assert!(error.contains("cannot present the exact client display"));
        assert!(error.contains("2 active outputs"));
        assert!(calls.lock().unwrap().contains(&Call::Restore));
    }

    #[test]
    fn restore_retries_until_all_outputs_are_reenabled() {
        let mut backend = FakeBackend::new(size(1680, 1050));
        // First restore settles the mode but leaves the topology isolated
        // (only one active output); the second re-enables everything.
        backend.restores = VecDeque::from([
            Ok(isolated(size(1680, 1050))),
            Ok(mode(size(1680, 1050), 0)),
        ]);
        let calls = Arc::clone(&backend.calls);
        let mut transaction = DisplayTransaction::acquire(backend, 0, size(1920, 1080)).unwrap();

        transaction.restore().unwrap();

        assert_eq!(
            calls
                .lock()
                .unwrap()
                .iter()
                .filter(|call| **call == Call::Restore)
                .count(),
            2,
            "a restore that leaves outputs disabled must be retried"
        );
    }

    #[test]
    fn rejected_exact_mode_uses_closest_fitting_driver_mode() {
        let mut backend = FakeBackend::new(size(1680, 1050));
        backend.supported = vec![
            size(1920, 1080),
            size(2560, 1440),
            size(2560, 1600),
            size(3600, 2338),
            size(3840, 2160),
        ];
        backend.tests.push_back(Err("BADMODE".to_string()));
        backend.tests.push_back(Ok(()));
        backend.applies.push_back(Ok(mode(size(2560, 1600), 0)));
        backend.restores =
            VecDeque::from([Ok(mode(size(1680, 1050), 0)), Ok(mode(size(1680, 1050), 0))]);
        let calls = Arc::clone(&backend.calls);

        let transaction = DisplayTransaction::acquire(backend, 0, size(3600, 2338)).unwrap();
        assert!(!transaction.report.exact);
        assert_eq!(transaction.report.applied, size(2560, 1600));
        assert_eq!(
            transaction.report.backend,
            "change-display-settings-ex-temporary-fallback"
        );
        assert_eq!(
            calls
                .lock()
                .unwrap()
                .iter()
                .filter(|call| **call == Call::Test(size(3600, 2338)))
                .count(),
            1,
            "a mode rejected by CDS_TEST must not be retried as its own fallback"
        );
        assert!(
            !calls.lock().unwrap().contains(&Call::Restore),
            "CDS_TEST rejection cannot have changed the mode and must not flicker the topology"
        );
        drop(transaction);
    }

    #[test]
    fn failed_apply_rolls_back_before_attempting_fallback() {
        let mut backend = FakeBackend::new(size(1680, 1050));
        backend.supported = vec![size(2560, 1600)];
        backend.tests.push_back(Ok(()));
        backend.tests.push_back(Ok(()));
        backend.applies.push_back(Err("apply failed".to_string()));
        backend.applies.push_back(Ok(mode(size(2560, 1600), 0)));
        backend.restores =
            VecDeque::from([Ok(mode(size(1680, 1050), 0)), Ok(mode(size(1680, 1050), 0))]);
        let calls = Arc::clone(&backend.calls);

        let transaction = DisplayTransaction::acquire(backend, 0, size(3600, 2338)).unwrap();
        let locked_calls = calls.lock().unwrap();
        let first_apply = locked_calls
            .iter()
            .position(|call| matches!(call, Call::Apply(_)))
            .unwrap();
        let rollback = locked_calls
            .iter()
            .position(|call| matches!(call, Call::Restore))
            .unwrap();
        let second_apply = locked_calls
            .iter()
            .rposition(|call| matches!(call, Call::Apply(_)))
            .unwrap();
        assert!(first_apply < rollback && rollback < second_apply);
        drop(locked_calls);
        drop(transaction);
        let locked_calls = calls.lock().unwrap();
        assert_eq!(
            locked_calls
                .iter()
                .filter(|call| **call == Call::Arm)
                .count(),
            1
        );
        assert_eq!(
            locked_calls
                .iter()
                .filter(|call| **call == Call::Disarm)
                .count(),
            1
        );
    }

    #[test]
    fn failed_apply_with_failed_rollback_is_not_retried_by_drop() {
        let mut backend = FakeBackend::new(size(1680, 1050));
        backend.tests.push_back(Ok(()));
        backend.applies.push_back(Err("apply failed".to_string()));
        backend.restores = VecDeque::from([
            Err("display busy".to_string()),
            Err("display busy".to_string()),
            Err("display busy".to_string()),
        ]);
        let calls = Arc::clone(&backend.calls);
        let restore_state = Arc::new(Mutex::new(RestoreJournal {
            state: RestoreState::Clean,
            last_failure: None,
        }));

        let error = match DisplayTransaction::acquire_observed(
            backend,
            &OutputSelector::GlobalIndex(0),
            size(1920, 1080),
            DisplayPolicy::Negotiated,
            Some(Arc::clone(&restore_state)),
        ) {
            Ok(_) => panic!("failed rollback must fail acquisition"),
            Err(error) => error,
        };

        assert!(error.contains("rollback also failed"));
        assert_eq!(
            calls
                .lock()
                .unwrap()
                .iter()
                .filter(|call| **call == Call::Restore)
                .count(),
            RESTORE_ATTEMPTS
        );
        assert!(matches!(
            &lock_restore_state(&restore_state).state,
            RestoreState::Failed { error, .. } if error.contains("attempt 3/3")
        ));
    }

    #[test]
    fn rejected_fallback_test_continues_on_healthy_current_display() {
        let mut backend = FakeBackend::new(size(1680, 1050));
        backend.supported = vec![size(2560, 1600)];
        backend.tests.push_back(Err("exact BADMODE".to_string()));
        backend.tests.push_back(Err("fallback BADMODE".to_string()));
        let calls = Arc::clone(&backend.calls);

        let transaction =
            DisplayTransaction::acquire(backend, 0, size(3600, 2338)).expect("healthy fallback");

        assert_eq!(transaction.report.applied, size(1680, 1050));
        assert!(!transaction.report.changed);
        assert_eq!(
            transaction.report.backend,
            "unchanged-current-mode-fallback"
        );
        assert!(!calls.lock().unwrap().contains(&Call::Restore));
    }

    #[test]
    fn rejected_driver_fallback_advances_to_the_next_candidate() {
        let mut backend = FakeBackend::new(size(1680, 1050));
        backend.supported = vec![size(2560, 1600), size(1920, 1080)];
        backend.tests.push_back(Err("exact BADMODE".to_string()));
        backend.tests.push_back(Err("closest BADMODE".to_string()));
        backend.tests.push_back(Ok(()));
        let ordered = fallback_candidates(size(3600, 2338), &backend.supported);
        backend.applies.push_back(Ok(mode(ordered[1], 0)));
        let calls = Arc::clone(&backend.calls);

        let transaction =
            DisplayTransaction::acquire(backend, 0, size(3600, 2338)).expect("second fallback");

        assert_eq!(transaction.report.applied, ordered[1]);
        assert!(calls.lock().unwrap().contains(&Call::Test(ordered[0])));
        assert!(calls.lock().unwrap().contains(&Call::Test(ordered[1])));
    }

    /// Measured on pier-windows-software.example.internal (QEMU stdvga, no NVENC) 2026-08-03: a
    /// 1792x1120 request served 1280x800 while the display was already running
    /// 1920x1200 and offered it. Both modes are 16-aligned, so the MF filter
    /// was not the cause — the old hard "must fit inside the request" rule
    /// discarded 1920x1200 (score 0.138) and left 1280x800 (score 0.673), a
    /// desktop less than half the area.
    #[test]
    fn a_slightly_larger_mode_beats_a_much_smaller_one() {
        let requested = size(1792, 1120);
        let supported = [
            size(640, 480),
            size(1024, 768),
            size(1152, 864),
            size(1280, 720),
            size(1280, 800),
            size(1280, 960),
            size(1280, 1024),
            size(1600, 1200),
            size(1920, 1200),
            size(2560, 1440),
            size(2560, 1600),
            size(3840, 2160),
        ];

        let ordered = fallback_candidates_matching(requested, &supported, |size| {
            DisplayPolicy::NegotiatedMacroblock16.accepts_size(size)
        });

        assert_eq!(
            ordered.first().copied(),
            Some(size(1920, 1200)),
            "the mode the display was already in must win, not 1280x800"
        );
    }

    /// The pixel-exact preference still has to hold: a mode that fits is worth
    /// keeping while it stays competitive, because it can be presented 1:1.
    #[test]
    fn a_fitting_mode_still_wins_when_it_is_competitive() {
        let requested = size(1920, 1200);
        // 1680x1050 fits and matches the aspect exactly; 2560x1600 also matches
        // the aspect but is far larger.
        let supported = [size(1680, 1050), size(2560, 1600)];

        let ordered = fallback_candidates_matching(requested, &supported, |_| true);

        assert_eq!(ordered.first().copied(), Some(size(1680, 1050)));

        // The same guard the penalty is calibrated against: a 3600x2338 request
        // must keep the fitting 2560x1600 rather than the larger 3840x2160,
        // whose shape is further from what was asked for.
        let ordered = fallback_candidates_matching(
            size(3600, 2338),
            &[size(2560, 1600), size(3840, 2160)],
            |_| true,
        );
        assert_eq!(ordered.first().copied(), Some(size(2560, 1600)));
    }

    #[test]
    fn exceeding_the_request_is_penalised_but_not_forbidden() {
        let requested = size(1792, 1120);
        assert!(exceeds_request(requested, size(1920, 1200)));
        assert!(!exceeds_request(requested, size(1280, 800)));
        // The penalty is exactly the documented constant, so the ordering above
        // is explainable rather than a tuned magic number.
        let bare = fallback_score(requested, size(1920, 1200));
        let ranked = ranked_fallback_score(requested, size(1920, 1200));
        assert!((ranked - bare - FALLBACK_EXCEEDS_REQUEST_PENALTY).abs() < 1e-9);
        assert!(ranked < ranked_fallback_score(requested, size(1280, 800)));
    }

    #[test]
    fn macroblock_policy_filters_unaligned_modes_before_fallback_ranking() {
        let requested = size(1792, 1168);
        let aligned = size(1280, 960);
        let mut backend = FakeBackend::new(size(1024, 768));
        backend.supported = vec![size(1680, 1050), size(1600, 900), aligned];
        backend.tests.push_back(Err("exact BADMODE".to_string()));
        backend.tests.push_back(Ok(()));
        backend.applies.push_back(Ok(mode(aligned, 0)));
        let calls = Arc::clone(&backend.calls);

        let transaction = DisplayTransaction::acquire_observed(
            backend,
            &OutputSelector::GlobalIndex(0),
            requested,
            DisplayPolicy::NegotiatedMacroblock16,
            None,
        )
        .expect("aligned driver fallback");

        assert_eq!(transaction.report.requested, requested);
        assert_eq!(transaction.report.applied, aligned);
        assert!(!transaction.report.exact);
        let calls = calls.lock().unwrap();
        assert!(calls.contains(&Call::Test(requested)));
        assert!(calls.contains(&Call::Test(aligned)));
        assert!(!calls.contains(&Call::Test(size(1680, 1050))));
        assert!(!calls.contains(&Call::Test(size(1600, 900))));
    }

    #[test]
    fn macroblock_policy_refuses_an_unaligned_current_mode_without_safe_candidates() {
        let requested = size(1792, 1168);
        let mut backend = FakeBackend::new(size(1680, 1050));
        backend.supported = vec![size(1600, 900)];
        backend.tests.push_back(Err("exact BADMODE".to_string()));
        let calls = Arc::clone(&backend.calls);

        let error = match DisplayTransaction::acquire_observed(
            backend,
            &OutputSelector::GlobalIndex(0),
            requested,
            DisplayPolicy::NegotiatedMacroblock16,
            None,
        ) {
            Ok(_) => panic!("unaligned current mode must not satisfy MF"),
            Err(error) => error,
        };

        assert!(error.contains("current state is unsafe"));
        let calls = calls.lock().unwrap();
        assert!(calls.contains(&Call::Test(requested)));
        assert!(!calls.iter().any(|call| matches!(call, Call::Apply(_))));
        assert!(!calls.contains(&Call::Restore));
    }

    #[test]
    fn watchdog_readiness_failure_blocks_all_display_mutation() {
        let mut backend = FakeBackend::new(size(1680, 1050));
        backend.arm_error = Some("watchdog exited before readiness".to_string());
        let calls = Arc::clone(&backend.calls);

        let error = match DisplayTransaction::acquire(backend, 0, size(3600, 2338)) {
            Ok(_) => panic!("acquisition must fail when recovery readiness fails"),
            Err(error) => error,
        };

        assert!(error.contains("watchdog exited before readiness"));
        assert!(calls.lock().unwrap().contains(&Call::Arm));
        assert!(
            !calls
                .lock()
                .unwrap()
                .iter()
                .any(|call| matches!(call, Call::Apply(_) | Call::Restore)),
            "display mutation cannot begin before watchdog readiness"
        );
    }

    #[test]
    fn explicit_restore_retries_bounded_failures_and_disarms_drop() {
        let mut backend = FakeBackend::new(size(1680, 1050));
        backend.restores = VecDeque::from([Err("busy".to_string()), Ok(mode(size(1680, 1050), 0))]);
        let calls = Arc::clone(&backend.calls);
        let mut transaction = DisplayTransaction::acquire(backend, 0, size(1920, 1080)).unwrap();
        transaction.restore().unwrap();
        drop(transaction);
        assert_eq!(
            calls
                .lock()
                .unwrap()
                .iter()
                .filter(|call| **call == Call::Restore)
                .count(),
            2
        );
    }

    #[test]
    fn restore_retries_a_successful_call_that_settles_to_the_wrong_geometry() {
        let mut backend = FakeBackend::new(size(1680, 1050));
        let mut wrong = mode(size(1680, 1050), 0);
        wrong.desktop_rect.width = 1920;
        backend.restores = VecDeque::from([Ok(wrong), Ok(mode(size(1680, 1050), 0))]);
        let calls = Arc::clone(&backend.calls);
        let mut transaction = DisplayTransaction::acquire(backend, 0, size(1920, 1080)).unwrap();

        transaction.restore().unwrap();

        assert_eq!(
            calls
                .lock()
                .unwrap()
                .iter()
                .filter(|call| **call == Call::Restore)
                .count(),
            2
        );
    }

    #[test]
    fn restore_journal_is_active_until_exact_restore_succeeds() {
        let backend = FakeBackend::new(size(1680, 1050));
        let restore_state = Arc::new(Mutex::new(RestoreJournal {
            state: RestoreState::Clean,
            last_failure: None,
        }));
        let mut transaction = DisplayTransaction::acquire_observed(
            backend,
            &OutputSelector::GlobalIndex(0),
            size(1920, 1080),
            DisplayPolicy::Negotiated,
            Some(Arc::clone(&restore_state)),
        )
        .unwrap();
        assert!(matches!(
            &lock_restore_state(&restore_state).state,
            RestoreState::Active {
                original,
                device_name
            } if *original == size(1680, 1050) && device_name == r"\\.\DISPLAY6"
        ));

        transaction.restore().unwrap();

        assert_eq!(
            lock_restore_state(&restore_state).state,
            RestoreState::Clean
        );
    }

    #[test]
    fn unwind_runs_drop_restore() {
        let backend = FakeBackend::new(size(1680, 1050));
        let calls = Arc::clone(&backend.calls);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _transaction = DisplayTransaction::acquire(backend, 0, size(1920, 1080)).unwrap();
            panic!("simulated session panic");
        }));
        assert!(result.is_err());
        assert!(calls
            .lock()
            .unwrap()
            .ends_with(&[Call::Restore, Call::Disarm]));
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "mutates the active Windows display; requires explicit environment opt-in"]
    fn live_native_display_round_trip() {
        assert_eq!(
            std::env::var("ARCEN_LIVE_DISPLAY_TEST").as_deref(),
            Ok("1"),
            "set ARCEN_LIVE_DISPLAY_TEST=1 to authorize live display mutation"
        );
        crate::logging::init(
            arcen_telemetry::OperationalProfile::Debug,
            crate::logging::COMPONENT_DIAGNOSTIC,
            None,
            false,
        )
        .expect("live test logging");
        let parse = |name: &str, default: u32| {
            std::env::var(name)
                .ok()
                .map(|value| value.parse::<u32>().unwrap())
                .unwrap_or(default)
        };
        let width = parse("ARCEN_LIVE_DISPLAY_WIDTH", 3600);
        let height = parse("ARCEN_LIVE_DISPLAY_HEIGHT", 2338);
        let output = parse("ARCEN_LIVE_DISPLAY_OUTPUT", 0);
        let hold_ms = parse("ARCEN_LIVE_DISPLAY_HOLD_MS", 2_000);
        let mut request = DisplayRequest::new(width, height).unwrap();
        request.refresh_hz = parse("ARCEN_LIVE_DISPLAY_REFRESH", 60);
        request.width_mm = 344.0;
        request.height_mm = 223.0;
        request.scale = 2.0;
        request.product_id = 0x3600;
        request.serial = 0x2338;

        let manager = DisplayManager::default();
        let mut lease = manager
            .acquire(
                OutputSelector::GlobalIndex(output),
                request,
                DisplayPolicy::ExactIsolated,
                crate::eventlog::random_correlation_id(),
            )
            .unwrap();
        println!("live display report: {:?}", lease.report());
        std::thread::sleep(Duration::from_millis(hold_ms.into()));
        lease.restore().unwrap();

        assert!(
            !crate::recovery::default_path().exists(),
            "verified restore must remove the recovery journal"
        );
    }

    // The helper is `cfg(all(windows, debug_assertions))`, so this caller must
    // carry the same gate or `cargo test --release` fails to compile.
    #[cfg(all(windows, debug_assertions))]
    #[test]
    #[ignore = "aborts after live display mutation to exercise the external recovery watchdog"]
    fn live_watchdog_crash_restore() {
        crate::logging::init(
            arcen_telemetry::OperationalProfile::Debug,
            crate::logging::COMPONENT_DIAGNOSTIC,
            None,
            false,
        )
        .expect("live test logging");
        run_live_watchdog_crash_test().unwrap();
    }

    #[test]
    fn failed_restore_is_retained_after_bounded_retries() {
        let mut backend = FakeBackend::new(size(1680, 1050));
        backend.restores = VecDeque::from([
            Err("display busy".to_string()),
            Err("display busy".to_string()),
            Err("display busy".to_string()),
        ]);
        let calls = Arc::clone(&backend.calls);
        let restore_state = Arc::new(Mutex::new(RestoreJournal {
            state: RestoreState::Clean,
            last_failure: None,
        }));
        let mut transaction = DisplayTransaction::acquire(backend, 0, size(1920, 1080)).unwrap();
        transaction.observe_restore(Arc::clone(&restore_state));

        assert!(transaction.restore().is_err());
        drop(transaction);
        assert_eq!(
            calls
                .lock()
                .unwrap()
                .iter()
                .filter(|call| **call == Call::Restore)
                .count(),
            RESTORE_ATTEMPTS
        );
        assert!(matches!(
            &lock_restore_state(&restore_state).state,
            RestoreState::Failed { error, .. } if error.contains("attempt 3/3")
        ));
        assert!(lock_restore_state(&restore_state)
            .last_failure
            .as_deref()
            .is_some_and(|error| error.contains("attempt 3/3")));
    }

    #[test]
    fn drop_records_exhausted_restore_in_the_journal() {
        let mut backend = FakeBackend::new(size(1680, 1050));
        backend.restores = VecDeque::from([
            Err("display busy".to_string()),
            Err("display busy".to_string()),
            Err("display busy".to_string()),
        ]);
        let restore_state = Arc::new(Mutex::new(RestoreJournal {
            state: RestoreState::Clean,
            last_failure: None,
        }));
        let transaction = DisplayTransaction::acquire_observed(
            backend,
            &OutputSelector::GlobalIndex(0),
            size(1920, 1080),
            DisplayPolicy::Negotiated,
            Some(Arc::clone(&restore_state)),
        )
        .unwrap();

        drop(transaction);

        assert!(matches!(
            &lock_restore_state(&restore_state).state,
            RestoreState::Failed { error, .. } if error.contains("attempt 3/3")
        ));
    }
}
