//! Research-only comparison of complete-region and bounded patch framing.
//!
//! This crate is outside the workspace default members. It models pre-codec
//! BGRA copies and framing bytes; it does not define or enable a live wire
//! format.

#![forbid(unsafe_code)]

mod model;

use std::error::Error;
use std::fmt::{Display, Formatter};

use arcen_keel::scenario::{Scenario, ScenarioKind};
use arcen_keel::{ActivityHint, BgraFrame, CadenceRecommendation, KeelError, KernelPreference};
use arcen_media::{
    RegionActivityDiagnostics, RegionActivityError, RegionActivityGrid, RegionActivityOwner,
    RegionContractError, RegionGeneration, RegionId,
};

use model::{FrameDisposition, FramingModel, ModelFrameStats};

/// Bytes per pixel in the BGRA research corpus.
pub const BGRA_BYTES_PER_PIXEL: usize = 4;
/// Capture ticks between metadata/full-picture keepalives in the harness.
pub const KEEPALIVE_TICKS: u64 = 60;
/// Maximum interval between full patch snapshots at a nominal 60 Hz.
pub const RECOVERY_KEYFRAME_TICKS: u64 = 120;
/// Maximum independently compositable rectangles in one patch frame.
pub const MAX_PATCHES_PER_FRAME: usize = 64;
/// Conceptual common frame metadata used only for byte accounting.
pub const FRAME_HEADER_BYTES: usize = 32;
/// Conceptual per-patch metadata used only for byte accounting.
pub const PATCH_DESCRIPTOR_BYTES: usize = 24;
/// Patch candidates at or above 80% of a full snapshot fall back to full.
pub const PATCH_FALLBACK_BASIS_POINTS: u16 = 8_000;
/// Fixed report width; Criterion uses the larger 1792x1168 corpus geometry.
pub const REPORT_WIDTH: usize = 640;
/// Fixed report height.
pub const REPORT_HEIGHT: usize = 360;
/// Fixed report duration at a nominal 60 capture ticks per second.
pub const REPORT_TICKS: u64 = 180;

/// Framing/copy model compared by the research harness.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelKind {
    /// Current complete-region input and complete-picture carrier accounting.
    FullPicture,
    /// Complete-picture carrier backed by coalesced full-width dirty bands.
    DirtyRows,
    /// Complete-picture carrier backed by coalesced dirty rectangles.
    DirtyRects,
    /// Bounded raw BGRA patches with full-snapshot fallback and recovery.
    BoundedPatches,
}

impl ModelKind {
    /// Every model in stable report order.
    pub const ALL: [Self; 4] = [
        Self::FullPicture,
        Self::DirtyRows,
        Self::DirtyRects,
        Self::BoundedPatches,
    ];

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::FullPicture => "full-picture",
            Self::DirtyRows => "dirty-rows",
            Self::DirtyRects => "dirty-rects",
            Self::BoundedPatches => "bounded-patches",
        }
    }
}

/// Receiver behavior for one emitted research frame.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DeliveryMode {
    /// Apply patches in descriptor order.
    #[default]
    InOrder,
    /// Apply patches in reverse order to prove spatial independence.
    ReversePatches,
    /// Account for sender work but drop the complete emitted frame.
    DropFrame,
}

/// Result of delivering one modeled frame.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeliveryStatus {
    NotEmitted,
    Applied,
    ReorderedApplied,
    Dropped,
    RejectedSequenceGap,
}

/// Semantic frame role after bounded-patch fallback is considered.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FrameKind {
    Keyframe,
    Delta,
    Keepalive,
}

/// Counts of `RegionActivityGrid` cadence recommendations.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CadenceMetrics {
    pub immediate: u64,
    pub keepalive: u64,
    pub responsive: u64,
    pub smooth: u64,
    pub continuous: u64,
}

impl CadenceMetrics {
    fn record(&mut self, cadence: CadenceRecommendation) {
        let counter = match cadence {
            CadenceRecommendation::Immediate => &mut self.immediate,
            CadenceRecommendation::Keepalive => &mut self.keepalive,
            CadenceRecommendation::Responsive => &mut self.responsive,
            CadenceRecommendation::Smooth => &mut self.smooth,
            CadenceRecommendation::Continuous => &mut self.continuous,
        };
        *counter = counter.saturating_add(1);
    }
}

/// Deterministic byte, copy, allocation-growth, and cadence accounting.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ModelMetrics {
    pub capture_ticks: u64,
    pub emitted_frames: u64,
    pub keyframes: u64,
    pub delta_frames: u64,
    pub keepalives: u64,
    pub carrier_bytes: u64,
    pub source_copy_bytes: u64,
    pub source_copy_operations: u64,
    pub compositor_copy_bytes: u64,
    pub compositor_copy_operations: u64,
    pub patches: u64,
    pub peak_patch_count: usize,
    pub full_frame_fallbacks: u64,
    pub dropped_frames: u64,
    pub rejected_sequence_gaps: u64,
    /// Unexpected `Vec` capacity growth after model construction.
    pub allocation_growths: u64,
    pub cadence: CadenceMetrics,
}

impl ModelMetrics {
    fn record_frame(&mut self, stats: ModelFrameStats) {
        self.emitted_frames = self.emitted_frames.saturating_add(1);
        match stats.frame_kind {
            FrameKind::Keyframe => self.keyframes = self.keyframes.saturating_add(1),
            FrameKind::Delta => self.delta_frames = self.delta_frames.saturating_add(1),
            FrameKind::Keepalive => self.keepalives = self.keepalives.saturating_add(1),
        }
        self.carrier_bytes = self.carrier_bytes.saturating_add(stats.carrier_bytes);
        self.source_copy_bytes = self
            .source_copy_bytes
            .saturating_add(stats.source_copy_bytes);
        self.source_copy_operations = self
            .source_copy_operations
            .saturating_add(stats.source_copy_operations);
        self.compositor_copy_bytes = self
            .compositor_copy_bytes
            .saturating_add(stats.compositor_copy_bytes);
        self.compositor_copy_operations = self
            .compositor_copy_operations
            .saturating_add(stats.compositor_copy_operations);
        self.patches = self.patches.saturating_add(usize_to_u64(stats.patch_count));
        self.peak_patch_count = self.peak_patch_count.max(stats.patch_count);
        self.allocation_growths = self
            .allocation_growths
            .saturating_add(stats.allocation_growths);
        if stats.full_frame_fallback {
            self.full_frame_fallbacks = self.full_frame_fallbacks.saturating_add(1);
        }
        match stats.delivery {
            DeliveryStatus::Dropped => {
                self.dropped_frames = self.dropped_frames.saturating_add(1);
            }
            DeliveryStatus::RejectedSequenceGap => {
                self.rejected_sequence_gaps = self.rejected_sequence_gaps.saturating_add(1);
            }
            DeliveryStatus::NotEmitted
            | DeliveryStatus::Applied
            | DeliveryStatus::ReorderedApplied => {}
        }
    }
}

/// Per-tick controls used by correctness and recovery research.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StepOptions {
    pub activity_hint: ActivityHint,
    pub force_keyframe: bool,
    pub delivery: DeliveryMode,
}

/// Outcome of one capture tick.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StepOutcome {
    pub diagnostics: RegionActivityDiagnostics,
    pub frame_kind: Option<FrameKind>,
    pub delivery: DeliveryStatus,
    pub patch_count: usize,
    pub full_frame_fallback: bool,
}

/// Fixed deterministic scenario configuration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScenarioConfig {
    pub width: usize,
    pub height: usize,
    pub ticks: u64,
    pub kind: ScenarioKind,
    pub seed: u64,
}

impl ScenarioConfig {
    #[must_use]
    pub const fn report(kind: ScenarioKind) -> Self {
        Self {
            width: REPORT_WIDTH,
            height: REPORT_HEIGHT,
            ticks: REPORT_TICKS,
            kind,
            seed: 42,
        }
    }
}

/// Complete deterministic result for one scenario/model pair.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScenarioReport {
    pub config: ScenarioConfig,
    pub model: ModelKind,
    pub metrics: ModelMetrics,
    pub reconstruction_mismatches: u64,
}

/// Failure while constructing or driving the research harness.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum HarnessError {
    RegionContract(RegionContractError),
    RegionActivity(RegionActivityError),
    Frame(KeelError),
    GeometryOverflow,
    TickRegression { previous: u64, received: u64 },
}

impl Display for HarnessError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RegionContract(error) => Display::fmt(error, formatter),
            Self::RegionActivity(error) => Display::fmt(error, formatter),
            Self::Frame(error) => Display::fmt(error, formatter),
            Self::GeometryOverflow => formatter.write_str("research frame geometry overflowed"),
            Self::TickRegression { previous, received } => write!(
                formatter,
                "capture tick regressed from {previous} to {received}"
            ),
        }
    }
}

impl Error for HarnessError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::RegionContract(error) => Some(error),
            Self::RegionActivity(error) => Some(error),
            Self::Frame(error) => Some(error),
            Self::GeometryOverflow | Self::TickRegression { .. } => None,
        }
    }
}

impl From<RegionContractError> for HarnessError {
    fn from(error: RegionContractError) -> Self {
        Self::RegionContract(error)
    }
}

impl From<RegionActivityError> for HarnessError {
    fn from(error: RegionActivityError) -> Self {
        Self::RegionActivity(error)
    }
}

impl From<KeelError> for HarnessError {
    fn from(error: KeelError) -> Self {
        Self::Frame(error)
    }
}

/// Region-owned activity map plus one research framing model and receiver.
#[derive(Debug)]
pub struct RegionPatchHarness {
    width: usize,
    height: usize,
    stride: usize,
    owner: RegionActivityOwner,
    activity: RegionActivityGrid,
    model: FramingModel,
    metrics: ModelMetrics,
    next_sequence: u64,
    last_tick: Option<u64>,
    last_emit_tick: Option<u64>,
    last_keyframe_tick: Option<u64>,
}

impl RegionPatchHarness {
    /// Constructs fixed-geometry state and preallocates every hot-path buffer.
    ///
    /// # Errors
    ///
    /// Returns checked region or frame-geometry errors.
    pub fn new(kind: ModelKind, width: usize, height: usize) -> Result<Self, HarnessError> {
        let owner = RegionActivityOwner::new(RegionGeneration::new(1)?, RegionId::new(1)?);
        let stride = width
            .checked_mul(BGRA_BYTES_PER_PIXEL)
            .ok_or(HarnessError::GeometryOverflow)?;
        let frame_bytes = stride
            .checked_mul(height)
            .ok_or(HarnessError::GeometryOverflow)?;
        let activity = RegionActivityGrid::new(owner, width, height, KernelPreference::Xxh3)?;
        let model = FramingModel::new(kind, activity.grid(), frame_bytes);
        Ok(Self {
            width,
            height,
            stride,
            owner,
            activity,
            model,
            metrics: ModelMetrics::default(),
            next_sequence: 1,
            last_tick: None,
            last_emit_tick: None,
            last_keyframe_tick: None,
        })
    }

    /// Processes one captured BGRA frame.
    ///
    /// Dirty frames emit immediately, clean frames emit only at the bounded
    /// keepalive, and every model receives the same cadence decision.
    ///
    /// # Errors
    ///
    /// Returns frame validation, activity ownership, or tick-order errors.
    pub fn step(
        &mut self,
        pixels: &[u8],
        stride: usize,
        tick: u64,
        options: StepOptions,
    ) -> Result<StepOutcome, HarnessError> {
        if self.last_tick.is_some_and(|previous| tick <= previous) {
            return Err(HarnessError::TickRegression {
                previous: self.last_tick.unwrap_or_default(),
                received: tick,
            });
        }
        let frame = BgraFrame::new(pixels, self.width, self.height, stride)?;
        let diagnostics =
            self.activity
                .update_with_hint(self.owner, frame, options.activity_hint)?;
        self.metrics.capture_ticks = self.metrics.capture_ticks.saturating_add(1);
        self.metrics.cadence.record(diagnostics.activity.cadence);

        let disposition = self.disposition(tick, diagnostics, options.force_keyframe);
        let Some(frame_kind) = disposition.frame_kind() else {
            self.last_tick = Some(tick);
            return Ok(StepOutcome {
                diagnostics,
                frame_kind: None,
                delivery: DeliveryStatus::NotEmitted,
                patch_count: 0,
                full_frame_fallback: false,
            });
        };

        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.saturating_add(1);
        let map = self.activity.damage_map();
        let stats = self
            .model
            .process(frame, map, frame_kind, sequence, options.delivery);
        self.metrics.record_frame(stats);
        self.last_emit_tick = Some(tick);
        if stats.frame_kind == FrameKind::Keyframe {
            self.last_keyframe_tick = Some(tick);
        }
        self.last_tick = Some(tick);

        Ok(StepOutcome {
            diagnostics,
            frame_kind: Some(stats.frame_kind),
            delivery: stats.delivery,
            patch_count: stats.patch_count,
            full_frame_fallback: stats.full_frame_fallback,
        })
    }

    #[must_use]
    pub const fn metrics(&self) -> ModelMetrics {
        self.metrics
    }

    /// Tight BGRA reconstruction held by the modeled full frame or patch receiver.
    #[must_use]
    pub fn reconstructed(&self) -> &[u8] {
        self.model.reconstructed()
    }

    /// Compares active source pixels with the tight reconstructed frame.
    #[must_use]
    pub fn reconstruction_matches(&self, pixels: &[u8], stride: usize) -> bool {
        if stride < self.stride {
            return false;
        }
        let Some(required) = stride.checked_mul(self.height) else {
            return false;
        };
        if pixels.len() < required {
            return false;
        }
        pixels
            .chunks(stride)
            .take(self.height)
            .zip(self.reconstructed().chunks_exact(self.stride))
            .all(|(source, reconstructed)| source.get(..self.stride) == Some(reconstructed))
    }

    fn disposition(
        &self,
        tick: u64,
        diagnostics: RegionActivityDiagnostics,
        force_keyframe: bool,
    ) -> FrameDisposition {
        let recovery_due = self
            .last_keyframe_tick
            .is_some_and(|last| tick.saturating_sub(last) >= RECOVERY_KEYFRAME_TICKS);
        if diagnostics.activity.baseline_refresh || force_keyframe || recovery_due {
            return FrameDisposition::Emit(FrameKind::Keyframe);
        }
        if !diagnostics.activity.summary.is_clean() {
            return FrameDisposition::Emit(FrameKind::Delta);
        }
        if self
            .last_emit_tick
            .is_some_and(|last| tick.saturating_sub(last) >= KEEPALIVE_TICKS)
        {
            FrameDisposition::Emit(FrameKind::Keepalive)
        } else {
            FrameDisposition::Suppress
        }
    }
}

/// Runs a complete deterministic scenario and checks reconstruction every tick.
///
/// # Errors
///
/// Returns any harness construction or frame-processing error.
pub fn run_scenario(
    config: ScenarioConfig,
    model: ModelKind,
) -> Result<ScenarioReport, HarnessError> {
    let scenario = Scenario::new(config.width, config.height, config.kind, config.seed);
    let frame_bytes = scenario
        .stride()
        .checked_mul(config.height)
        .ok_or(HarnessError::GeometryOverflow)?;
    let mut pixels = Vec::with_capacity(frame_bytes);
    let mut harness = RegionPatchHarness::new(model, config.width, config.height)?;
    let mut reconstruction_mismatches = 0u64;

    for tick in 0..config.ticks {
        scenario.render(tick, &mut pixels);
        let activity_hint = if config.kind == ScenarioKind::Scroll {
            ActivityHint::Scroll
        } else {
            ActivityHint::None
        };
        harness.step(
            &pixels,
            scenario.stride(),
            tick,
            StepOptions {
                activity_hint,
                ..StepOptions::default()
            },
        )?;
        if !harness.reconstruction_matches(&pixels, scenario.stride()) {
            reconstruction_mismatches = reconstruction_mismatches.saturating_add(1);
        }
    }

    Ok(ScenarioReport {
        config,
        model,
        metrics: harness.metrics(),
        reconstruction_mismatches,
    })
}

#[must_use]
pub(crate) fn usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}
