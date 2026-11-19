// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The canvas CPU-budget governor: one budget per Pier tick, shared by
//! every monitor, spent in a fixed order and degraded in a fixed order.
//!
//! **Budget.** `B = tick × logical cores × cpu_share` ([`CpuShare`],
//! `video.software.cpu_share`, 50 % of the logical cores by default).
//!
//! **Spending order** (first wins; [`CanvasGovernor::allocate`]):
//!
//! 1. Mandatory work, always, even over budget: a reset and its repaint,
//!    the IDR of a new surface, damage near the latest input, keepalives.
//! 2. Content: copies, fills and cache paints (never deferred, they are
//!    cheaper than anything they replace), then exact tiles and one-shot
//!    image paints, oldest first.
//! 3. Motion surfaces, at the governed surface rate.
//! 4. Settle refinement (lossy blocks rebuilt exactly) with what is left,
//!    only on a monitor without surface work this tick.
//!
//! Across monitors the order within each tier is the monitor with input
//! focus first, then the monitor with the most motion.
//!
//! **Degradation order** when surfaces do not fit:
//!
//! - (a) the surface rate steps down through the session rate's even
//!   divisors (the shared [`HostRateGovernor`], judged once a second on
//!   surface pictures wanted against painted);
//! - (b) at its floor, the surface quality steps down (lower bitrate,
//!   higher minimum QP; [`QUALITY_STEPS`]);
//! - (c) then surfaces are encoded smaller ([`SurfaceScale`], design O4);
//! - (d) content itself spreads over more ticks only when it alone
//!   exceeds the budget (tiles are coalesced, oldest first).
//!
//! Recovery runs in reverse after the governor's probing holds: scale,
//! then quality, then the rate.
//!
//! **Flow control** ([`CanvasFlow`], ADR 0028 §7). At most
//! [`MAX_UNACKED_OPTIONAL_FRAMES`] canvas frames per monitor that carry
//! optional work (tiers 3 and 4) may be unacknowledged by `canvas_ack`, and
//! none while the Deck holds more unapplied bytes than the link carries in
//! [`UNPROCESSED_LINK_WINDOW`]. Mandatory work and content never wait for
//! an acknowledgement.

use super::cost::CostModel;
use super::surface::{SurfaceQuality, SurfaceScale};
use crate::video::{
    GOVERNOR_FIRST_HOLD_SECS, GOVERNOR_MAX_HOLD_SECS, GOVERNOR_MIN_FPS, GOVERNOR_SHORT_WINDOWS,
    GOVERNOR_WINDOWS, GovernorReason, HostRateGovernor,
};
use arcen_keel::WorkKind;
use arcen_protocol::canvas::CanvasControlMsg;
use std::collections::{BTreeMap, VecDeque};
use std::time::Duration;

/// Canvas frames carrying optional work a monitor may have unacknowledged
/// (ADR 0028 §7).
pub const MAX_UNACKED_OPTIONAL_FRAMES: usize = 2;

/// Default share of the logical cores a software Pier spends (design O2).
pub const DEFAULT_CPU_SHARE_PERCENT: u8 = 50;

/// Unapplied Deck bytes above which optional work stops: what the link
/// carries in this window, and at least [`MIN_UNPROCESSED_LIMIT_BYTES`].
pub const UNPROCESSED_LINK_WINDOW: Duration = Duration::from_millis(250);

/// Floor of the unapplied-bytes limit.
pub const MIN_UNPROCESSED_LIMIT_BYTES: u64 = 256 * 1024;

/// Quality steps below full quality (degradation step b).
pub const QUALITY_STEPS: u8 = 2;

const QUALITY_BITRATE_PERCENT: [u64; QUALITY_STEPS as usize + 1] = [100, 75, 50];
const QUALITY_MIN_QP_RAISE: [u8; QUALITY_STEPS as usize + 1] = [0, 4, 8];
const HOLD_PERCENT: u32 = 95;

const fn held_floor(fps: u32) -> u32 {
    fps.saturating_mul(HOLD_PERCENT).div_ceil(100)
}

/// The share of the logical cores the software pipeline may spend
/// (`video.software.cpu_share`, a whole percentage, 1 to 100).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpuShare {
    percent: u8,
}

impl Default for CpuShare {
    fn default() -> Self {
        Self {
            percent: DEFAULT_CPU_SHARE_PERCENT,
        }
    }
}

impl CpuShare {
    /// A share of `percent` (1 to 100).
    #[must_use]
    pub const fn from_percent(percent: u8) -> Option<Self> {
        if percent == 0 || percent > 100 {
            None
        } else {
            Some(Self { percent })
        }
    }

    /// The administrator's setting, or the default when it is absent.
    /// Configuration loading refuses an out-of-range value; one that
    /// reaches here anyway keeps the default.
    #[must_use]
    pub fn from_setting(percent: Option<u8>) -> Self {
        percent.and_then(Self::from_percent).unwrap_or_default()
    }

    /// The percentage.
    #[must_use]
    pub const fn percent(self) -> u8 {
        self.percent
    }

    /// CPU time per tick of `tick` on `logical_cores` cores.
    #[must_use]
    pub fn budget(self, tick: Duration, logical_cores: u32) -> Duration {
        tick.saturating_mul(logical_cores.max(1)) * u32::from(self.percent) / 100
    }
}

/// A monitor's flow-control window (ADR 0028 §7).
#[derive(Debug, Clone, Default)]
pub struct CanvasFlow {
    generation: u32,
    optional: VecDeque<u64>,
    unprocessed_bytes: u64,
    acked_fence: u64,
}

impl CanvasFlow {
    /// A frame went out. A new generation (a reset) starts a fresh window.
    pub fn sent(&mut self, generation: u32, fence: u64, optional: bool) {
        if generation != self.generation {
            *self = Self {
                generation,
                ..Self::default()
            };
        }
        if optional && fence > self.acked_fence {
            self.optional.push_back(fence);
        }
    }

    /// A `canvas_ack`: frames up to `fence` are presented and the Deck
    /// holds `unprocessed_bytes`. An acknowledgement of another generation
    /// is ignored.
    pub fn ack(&mut self, generation: u32, fence: u64, unprocessed_bytes: u64) {
        if generation != self.generation {
            return;
        }
        self.acked_fence = self.acked_fence.max(fence);
        while self
            .optional
            .front()
            .is_some_and(|sent| *sent <= self.acked_fence)
        {
            self.optional.pop_front();
        }
        self.unprocessed_bytes = unprocessed_bytes;
    }

    /// Frames with optional work not yet acknowledged.
    #[must_use]
    pub fn unacked_optional(&self) -> usize {
        self.optional.len()
    }

    /// Whether the next frame may carry optional work.
    #[must_use]
    pub fn optional_allowed(&self, unprocessed_limit: u64) -> bool {
        self.optional.len() < MAX_UNACKED_OPTIONAL_FRAMES
            && self.unprocessed_bytes <= unprocessed_limit
    }
}

/// What a governor is set up with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GovernorConfig {
    /// The session's tick rate (the served rate).
    pub session_fps: u32,
    /// Logical cores on the host.
    pub logical_cores: u32,
    /// The share of them the pipeline may spend.
    pub cpu_share: CpuShare,
    /// The lowest surface rate (degradation step a).
    pub floor_fps: u32,
    /// Bitrate surfaces share before rate control reports one.
    pub initial_bitrate_bps: u64,
    /// Feed measured work into the cost model; a simulation turns it off
    /// to keep its cost model fixed.
    pub learn: bool,
}

impl GovernorConfig {
    /// A governor for a session at `session_fps` on `logical_cores` cores.
    #[must_use]
    pub const fn new(
        session_fps: u32,
        logical_cores: u32,
        cpu_share: CpuShare,
        initial_bitrate_bps: u64,
    ) -> Self {
        Self {
            session_fps,
            logical_cores,
            cpu_share,
            floor_fps: GOVERNOR_MIN_FPS,
            initial_bitrate_bps,
            learn: true,
        }
    }

    /// The tick.
    #[must_use]
    pub fn tick(&self) -> Duration {
        Duration::from_secs(1) / self.session_fps.max(1)
    }

    /// The per-tick CPU budget.
    #[must_use]
    pub fn budget(&self) -> Duration {
        self.cpu_share.budget(self.tick(), self.logical_cores)
    }
}

/// One surface's wish for this tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SurfaceDemand {
    /// Surface id.
    pub surface: u8,
    /// Predicted encode cost.
    pub cost: Duration,
    /// Coded pixels.
    pub coded_pixels: u64,
}

/// What one monitor wants this tick, predicted by the cost model.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MonitorDemand {
    /// Monitor.
    pub monitor_id: u16,
    /// The monitor has input focus.
    pub focus: bool,
    /// Keel work already done this tick (hashing, classification).
    pub sunk: Duration,
    /// Tier 1.
    pub mandatory: Duration,
    /// Tier 2: copies, fills and cache paints (never deferred).
    pub structural: Duration,
    /// Tier 2: tiles and one-shot paints (deferrable, oldest first).
    pub content: Duration,
    /// Tier 3.
    pub surfaces: Vec<SurfaceDemand>,
    /// Tier 4.
    pub refine: Duration,
    /// `Motion` blocks (the split across monitors).
    pub motion_blocks: u32,
    /// Coded pixels of pictures that paint without a surface grant (new
    /// surfaces' IDRs, one-shot pictures), for the bitrate split.
    pub picture_pixels: u64,
    /// The monitor's area in pixels (0: not stated). Pictures share the
    /// link by their part of every monitor's area at most, the bits per
    /// pixel a full-frame stream of the same monitors would get, so a
    /// window's surface does not spend the whole link on its region.
    pub canvas_pixels: u64,
}

/// What one monitor may spend this tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorGrant {
    /// Monitor.
    pub monitor_id: u16,
    /// Budget for deferrable content (tiles, one-shot paints).
    pub content: Duration,
    /// Surfaces that may paint (besides IDRs of new surfaces, which always
    /// paint).
    pub surfaces: Vec<u8>,
    /// Budget for refinement.
    pub refine: Duration,
    /// Rate control of this monitor's pictures this tick: its share of the
    /// link by coded pixels, which the planner splits among its pictures
    /// the same way.
    pub quality: SurfaceQuality,
    /// The scale surfaces are encoded at.
    pub scale: SurfaceScale,
}

impl MonitorGrant {
    /// A grant that spends nothing optional (a monitor the governor has
    /// not seen).
    #[must_use]
    pub fn mandatory_only(monitor_id: u16, scale: SurfaceScale) -> Self {
        Self {
            monitor_id,
            content: Duration::ZERO,
            surfaces: Vec::new(),
            refine: Duration::ZERO,
            quality: SurfaceQuality {
                bitrate_bps: 0,
                min_qp_raise: 0,
                fps: 0,
            },
            scale,
        }
    }
}

/// What the planner and the Pier report after a tick, per monitor.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TickOutcome {
    /// Monitor.
    pub monitor_id: u16,
    /// Canvas generation of the frames sent.
    pub generation: u32,
    /// Newest fence sent (0: none).
    pub fence: u64,
    /// The newest frame carried optional work.
    pub carried_optional: bool,
    /// Measured work: kind, pixels, time.
    pub measured: Vec<(WorkKind, u64, Duration)>,
}

/// A degradation step taken or undone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DegradationStep {
    /// (a) The surface rate.
    SurfaceFps {
        /// New rate.
        fps: u32,
        /// Previous rate.
        previous: u32,
    },
    /// (b) Surface quality step (0 = full).
    Quality {
        /// New step.
        step: u8,
        /// Previous step.
        previous: u8,
    },
    /// (c) Surface scale.
    Scale {
        /// New scale.
        scale: SurfaceScale,
        /// Previous scale.
        previous: SurfaceScale,
    },
}

/// A step and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GovernorEvent {
    /// The step.
    pub step: DegradationStep,
    /// Why ([`GovernorReason::Shortfall`] on budget-denied surface
    /// pictures, [`GovernorReason::Probe`] after a hold).
    pub reason: GovernorReason,
}

/// The governor's position, for READY, hello truth and Stats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GovernorState {
    /// Per-tick CPU budget.
    pub budget: Duration,
    /// Session rate.
    pub session_fps: u32,
    /// Surface rate (step a).
    pub surface_fps: u32,
    /// Quality step (b).
    pub quality_step: u8,
    /// Surface scale (c).
    pub scale: SurfaceScale,
    /// Content was spread over more ticks in the latest tick (d).
    pub coalescing: bool,
    /// Bitrate surfaces share.
    pub link_bps: u64,
}

/// Steps (b) and (c), judged like [`HostRateGovernor`]: three short seconds
/// of five step down, a hold probes one step back, a failed probe doubles
/// the hold.
#[derive(Debug, Clone)]
struct Ladder {
    quality: u8,
    scale: SurfaceScale,
    judged: u32,
    short: u32,
    hold_secs: u32,
    held_for: u32,
    probing: bool,
}

impl Ladder {
    const fn new() -> Self {
        Self {
            quality: 0,
            scale: SurfaceScale::Full,
            judged: 0,
            short: 0,
            hold_secs: GOVERNOR_FIRST_HOLD_SECS,
            held_for: 0,
            probing: false,
        }
    }

    fn clear(&self) -> bool {
        self.quality == 0 && self.scale == SurfaceScale::Full
    }

    fn escalate(&mut self) -> Option<DegradationStep> {
        if self.quality < QUALITY_STEPS {
            let previous = self.quality;
            self.quality += 1;
            return Some(DegradationStep::Quality {
                step: self.quality,
                previous,
            });
        }
        let previous = self.scale;
        self.scale = previous.smaller()?;
        Some(DegradationStep::Scale {
            scale: self.scale,
            previous,
        })
    }

    fn relax(&mut self) -> Option<DegradationStep> {
        if let Some(scale) = self.scale.larger() {
            let previous = self.scale;
            self.scale = scale;
            return Some(DegradationStep::Scale { scale, previous });
        }
        let previous = self.quality;
        self.quality = previous.checked_sub(1)?;
        Some(DegradationStep::Quality {
            step: self.quality,
            previous,
        })
    }

    fn reset_window(&mut self) {
        self.judged = 0;
        self.short = 0;
    }

    /// One second at the rate floor.
    fn observe(&mut self, painted: u32, source: Option<u32>, asked: u32) -> Option<GovernorEvent> {
        if !self.clear() && !self.probing {
            self.held_for = self.held_for.saturating_add(1);
            if self.held_for >= self.hold_secs {
                self.held_for = 0;
                self.probing = true;
                self.reset_window();
                return self.relax().map(|step| GovernorEvent {
                    step,
                    reason: GovernorReason::Probe,
                });
            }
        }
        let floor = held_floor(asked.max(1));
        let source = source?;
        if source < floor {
            return None;
        }
        self.judged += 1;
        if painted < floor {
            self.short += 1;
        }
        if self.short >= GOVERNOR_SHORT_WINDOWS {
            if self.probing {
                self.hold_secs = self.hold_secs.saturating_mul(2).min(GOVERNOR_MAX_HOLD_SECS);
            }
            self.probing = false;
            self.held_for = 0;
            self.reset_window();
            return self.escalate().map(|step| GovernorEvent {
                step,
                reason: GovernorReason::Shortfall {
                    encoded_fps: painted,
                    source_fps: source,
                },
            });
        }
        if self.judged >= GOVERNOR_WINDOWS {
            if self.probing {
                self.probing = false;
                self.hold_secs = GOVERNOR_FIRST_HOLD_SECS;
            }
            self.reset_window();
        }
        None
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct SecondCounts {
    ticks: u32,
    /// Due ticks on which surface pictures were wanted and the flow
    /// window allowed them.
    wanted: u32,
    /// Of those, ticks on which every wanted surface painted.
    painted: u32,
}

/// The canvas CPU-budget governor of one session (every monitor).
#[derive(Debug, Clone)]
pub struct CanvasGovernor {
    config: GovernorConfig,
    budget: Duration,
    costs: CostModel,
    rate: HostRateGovernor,
    ladder: Ladder,
    phase: u32,
    second: SecondCounts,
    flows: BTreeMap<u16, CanvasFlow>,
    link_bps: u64,
    coalescing: bool,
    credit: i128,
}

fn nanos(duration: Duration) -> i128 {
    i128::try_from(duration.as_nanos()).unwrap_or(i128::MAX)
}

fn duration(nanos: i128) -> Duration {
    Duration::from_nanos(u64::try_from(nanos.max(0)).unwrap_or(u64::MAX))
}

impl CanvasGovernor {
    /// A governor over `costs`.
    #[must_use]
    pub fn new(config: GovernorConfig, costs: CostModel) -> Self {
        let session_fps = config.session_fps.max(1);
        Self {
            budget: config.budget(),
            rate: HostRateGovernor::new(session_fps, config.floor_fps),
            ladder: Ladder::new(),
            phase: session_fps,
            second: SecondCounts::default(),
            flows: BTreeMap::new(),
            link_bps: config.initial_bitrate_bps,
            coalescing: false,
            credit: 0,
            config,
            costs,
        }
    }

    /// The cost model the planner predicts with.
    #[must_use]
    pub const fn costs(&self) -> &CostModel {
        &self.costs
    }

    /// The scale surfaces are encoded at now.
    #[must_use]
    pub const fn scale(&self) -> SurfaceScale {
        self.ladder.scale
    }

    /// The governor's position.
    #[must_use]
    pub fn state(&self) -> GovernorState {
        GovernorState {
            budget: self.budget,
            session_fps: self.config.session_fps,
            surface_fps: self.rate.fps(),
            quality_step: self.ladder.quality,
            scale: self.ladder.scale,
            coalescing: self.coalescing,
            link_bps: self.link_bps,
        }
    }

    /// The flow window of a monitor.
    #[must_use]
    pub fn flow(&self, monitor_id: u16) -> Option<&CanvasFlow> {
        self.flows.get(&monitor_id)
    }

    /// Rate control's current target (`RateController::target_bps`):
    /// surfaces share it.
    pub fn set_link_bps(&mut self, bps: u64) {
        self.link_bps = bps;
    }

    /// Unapplied Deck bytes above which optional work stops.
    #[must_use]
    pub fn unprocessed_limit(&self) -> u64 {
        let window = u128::from(self.link_bps) * UNPROCESSED_LINK_WINDOW.as_millis() / 8_000;
        u64::try_from(window)
            .unwrap_or(u64::MAX)
            .max(MIN_UNPROCESSED_LIMIT_BYTES)
    }

    /// A Deck's canvas control message. Returns the monitor whose canvas
    /// must be reset (`canvas_reset_request`); invalid messages are
    /// ignored.
    pub fn on_control(&mut self, message: &CanvasControlMsg) -> Option<u16> {
        message.validate().ok()?;
        match message {
            CanvasControlMsg::CanvasAck {
                monitor_id,
                generation,
                fence,
                unprocessed_bytes,
                ..
            } => {
                if let Some(flow) = self.flows.get_mut(monitor_id) {
                    flow.ack(*generation, *fence, *unprocessed_bytes);
                }
                None
            }
            CanvasControlMsg::CanvasResetRequest { monitor_id, .. } => Some(*monitor_id),
        }
    }

    fn optional_allowed(&self, monitor_id: u16) -> bool {
        let limit = self.unprocessed_limit();
        self.flows
            .get(&monitor_id)
            .is_none_or(|flow| flow.optional_allowed(limit))
    }

    /// A monitor's share of the link: its pictures' coded pixels among
    /// every monitor's this tick (the whole link for a monitor that paints
    /// none, whose bitrate is never used).
    fn quality(&self, coded_pixels: u64, total_pixels: u64) -> SurfaceQuality {
        let step = usize::from(self.ladder.quality);
        let link = u128::from(self.link_bps) * u128::from(QUALITY_BITRATE_PERCENT[step]) / 100;
        let share = if coded_pixels == 0 {
            link
        } else {
            link * u128::from(coded_pixels) / u128::from(total_pixels.max(1))
        };
        SurfaceQuality {
            bitrate_bps: u64::try_from(share).unwrap_or(u64::MAX),
            min_qp_raise: QUALITY_MIN_QP_RAISE[step],
            fps: self.rate.fps(),
        }
    }

    /// Splits this tick's budget over every monitor's demand (one grant per
    /// demand, in the same order) and counts the tick.
    pub fn allocate(
        &mut self,
        demands: &[MonitorDemand],
    ) -> (Vec<MonitorGrant>, Option<GovernorEvent>) {
        let session_fps = self.config.session_fps.max(1);
        self.phase = self.phase.saturating_add(self.rate.fps());
        let surfaces_due = self.phase >= session_fps;
        if surfaces_due {
            self.phase -= session_fps;
        }

        // CPU credit: every tick adds the budget; a picture due every Nth
        // tick may spend what N ticks saved, and mandatory work over the
        // budget is a debt the next ticks repay.
        let fixed = demands.iter().fold(Duration::ZERO, |sum, demand| {
            sum + demand.sunk + demand.mandatory + demand.structural
        });
        self.credit = (self.credit + nanos(self.budget)).min(self.credit_cap()) - nanos(fixed);

        let mut order: Vec<usize> = (0..demands.len()).collect();
        order.sort_by_key(|&index| {
            let demand = &demands[index];
            (
                !demand.focus,
                std::cmp::Reverse(demand.motion_blocks),
                demand.monitor_id,
            )
        });

        let mut grants: Vec<MonitorGrant> = demands
            .iter()
            .map(|demand| MonitorGrant::mandatory_only(demand.monitor_id, self.ladder.scale))
            .collect();

        self.coalescing = false;
        for &index in &order {
            let wanted = nanos(demands[index].content);
            let granted = wanted.min(self.credit.max(0));
            if granted < wanted {
                self.coalescing = true;
            }
            grants[index].content = duration(granted);
            self.credit -= granted;
        }

        let mut wanted = false;
        let mut all_painted = true;
        let mut reserved = 0_i128;
        let mut painted_pixels: Vec<u64> =
            demands.iter().map(|demand| demand.picture_pixels).collect();
        for &index in &order {
            let demand = &demands[index];
            if demand.surfaces.is_empty() || !self.optional_allowed(demand.monitor_id) {
                continue;
            }
            wanted = true;
            for surface in &demand.surfaces {
                let cost = nanos(surface.cost);
                if !surfaces_due {
                    reserved += cost;
                } else if cost <= self.credit {
                    self.credit -= cost;
                    grants[index].surfaces.push(surface.surface);
                    painted_pixels[index] += surface.coded_pixels;
                } else {
                    all_painted = false;
                    reserved += cost;
                }
            }
        }
        let canvas_pixels: u64 = demands.iter().map(|demand| demand.canvas_pixels).sum();
        let total_pixels: u64 = painted_pixels.iter().sum::<u64>().max(canvas_pixels);
        for (grant, pixels) in grants.iter_mut().zip(&painted_pixels) {
            grant.quality = self.quality(*pixels, total_pixels);
        }

        // Refinement takes only credit no surface is saving for.
        for &index in &order {
            let demand = &demands[index];
            if !demand.surfaces.is_empty() || !self.optional_allowed(demand.monitor_id) {
                continue;
            }
            let granted = nanos(demand.refine).min((self.credit - reserved).max(0));
            grants[index].refine = duration(granted);
            self.credit -= granted;
        }

        // Only due ticks count: a picture that waits for its due tick is
        // one source picture, not one per tick it waited.
        self.second.ticks += 1;
        if wanted && surfaces_due {
            self.second.wanted += 1;
            if all_painted {
                self.second.painted += 1;
            }
        }
        let event = if self.second.ticks >= session_fps {
            self.judge_second()
        } else {
            None
        };
        (grants, event)
    }

    /// Credit never exceeds what the slowest surface rate saves between
    /// two pictures.
    fn credit_cap(&self) -> i128 {
        let ticks = self
            .config
            .session_fps
            .max(1)
            .div_ceil(self.config.floor_fps.max(1));
        nanos(self.budget) * i128::from(ticks.max(1))
    }

    /// One second of surface evidence, judged against the rate the
    /// surfaces were asked for: due ticks that had surface pictures to
    /// paint (the source, as far as the rate lets it show: 24 fps content
    /// under a 30 fps surface rate is never judged as a slow host) against
    /// those on which every wanted surface painted.
    fn judge_second(&mut self) -> Option<GovernorEvent> {
        let counts = std::mem::take(&mut self.second);
        let asked = self.rate.fps();
        let session_fps = self.config.session_fps.max(1);
        let source_fps = (counts.wanted > 0).then_some(counts.wanted);
        if self.ladder.clear() {
            if let Some(step) = self.rate.observe(counts.painted, source_fps, asked) {
                self.ladder.reset_window();
                self.phase = session_fps;
                return Some(GovernorEvent {
                    step: DegradationStep::SurfaceFps {
                        fps: step.fps,
                        previous: step.previous_fps,
                    },
                    reason: step.reason,
                });
            }
        }
        if self.rate.fps() > self.config.floor_fps.clamp(1, session_fps) {
            return None;
        }
        self.ladder.observe(counts.painted, source_fps, asked)
    }

    /// Records what each monitor sent: flow windows and measured work.
    pub fn complete(&mut self, outcomes: &[TickOutcome]) {
        for outcome in outcomes {
            if outcome.fence > 0 || outcome.generation > 0 {
                self.flows.entry(outcome.monitor_id).or_default().sent(
                    outcome.generation,
                    outcome.fence,
                    outcome.carried_optional,
                );
            }
            if self.config.learn {
                for &(kind, pixels, elapsed) in &outcome.measured {
                    self.costs.record(kind, pixels, elapsed);
                }
            }
        }
    }

    /// Forgets a monitor (it left the topology).
    pub fn remove_monitor(&mut self, monitor_id: u16) {
        self.flows.remove(&monitor_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MBPS: u64 = 1_000_000;

    fn ms(value: f64) -> Duration {
        Duration::from_secs_f64(value / 1_000.0)
    }

    /// 60 ticks a second, one core at 50 %: 8.33 ms a tick.
    fn governor() -> CanvasGovernor {
        let mut config = GovernorConfig::new(60, 1, CpuShare::default(), 10 * MBPS);
        config.learn = false;
        CanvasGovernor::new(config, CostModel::default())
    }

    /// One monitor playing video whose picture costs `full_ms` at source
    /// size (scaled pictures cost their area).
    fn video(monitor_id: u16, full_ms: f64, scale: SurfaceScale, motion: u32) -> MonitorDemand {
        let (numerator, denominator) = scale.ratio();
        let factor = (f64::from(numerator) / f64::from(denominator)).powi(2);
        MonitorDemand {
            monitor_id,
            surfaces: vec![SurfaceDemand {
                surface: 0,
                cost: ms(full_ms * factor),
                coded_pixels: 640 * 360,
            }],
            motion_blocks: motion,
            ..MonitorDemand::default()
        }
    }

    fn steps(
        governor: &mut CanvasGovernor,
        seconds: u32,
        demand: impl Fn(&CanvasGovernor) -> MonitorDemand,
    ) -> Vec<DegradationStep> {
        let mut events = Vec::new();
        for _ in 0..seconds * 60 {
            let demand = demand(governor);
            let (grants, event) = governor.allocate(std::slice::from_ref(&demand));
            assert_eq!(grants.len(), 1);
            events.extend(event.map(|event| event.step));
        }
        events
    }

    #[test]
    fn the_cpu_share_defaults_to_half_the_logical_cores() {
        assert_eq!(CpuShare::default().percent(), 50);
        assert_eq!(CpuShare::from_setting(None), CpuShare::default());
        assert_eq!(CpuShare::from_setting(Some(0)), CpuShare::default());
        assert_eq!(CpuShare::from_setting(Some(101)), CpuShare::default());
        assert_eq!(CpuShare::from_setting(Some(25)).percent(), 25);
        let tick = Duration::from_millis(16);
        assert_eq!(
            CpuShare::default().budget(tick, 8),
            Duration::from_millis(64)
        );
        assert_eq!(
            GovernorConfig::new(60, 4, CpuShare::default(), 0).budget(),
            Duration::from_secs(1) / 60 * 2
        );
    }

    #[test]
    fn step_a_the_surface_rate_steps_down_first_to_an_even_divisor() {
        // 30 ms pictures on 8.33 ms ticks: about 16 a second fit.
        let mut governor = governor();
        let events = steps(&mut governor, 10, |governor| {
            video(1, 30.0, governor.scale(), 100)
        });
        assert_eq!(
            events,
            vec![DegradationStep::SurfaceFps {
                fps: 15,
                previous: 60
            }]
        );
        assert_eq!(governor.state().surface_fps, 15);
        // Held at 15, every due picture paints: no quality or scale step,
        // even after the probe back up fails.
        let events = steps(&mut governor, 60, |governor| {
            video(1, 30.0, governor.scale(), 100)
        });
        assert!(
            events
                .iter()
                .all(|step| matches!(step, DegradationStep::SurfaceFps { .. })),
            "{events:?}"
        );
        assert_eq!(governor.state().quality_step, 0);
        assert_eq!(governor.state().scale, SurfaceScale::Full);
    }

    #[test]
    fn steps_b_and_c_quality_then_scale_only_at_the_rate_floor() {
        // 120 ms pictures: nothing fits even at 10 a second until the
        // surface is encoded at half size.
        let mut governor = governor();
        let events = steps(&mut governor, 30, |governor| {
            video(1, 120.0, governor.scale(), 100)
        });
        assert_eq!(
            events,
            vec![
                DegradationStep::SurfaceFps {
                    fps: 10,
                    previous: 60
                },
                DegradationStep::Quality {
                    step: 1,
                    previous: 0
                },
                DegradationStep::Quality {
                    step: 2,
                    previous: 1
                },
                DegradationStep::Scale {
                    scale: SurfaceScale::TwoThirds,
                    previous: SurfaceScale::Full
                },
                DegradationStep::Scale {
                    scale: SurfaceScale::Half,
                    previous: SurfaceScale::TwoThirds
                },
            ]
        );
        let state = governor.state();
        assert_eq!(
            (state.surface_fps, state.quality_step, state.scale),
            (10, 2, SurfaceScale::Half)
        );
        // Quality step 2 halves the surfaces' bitrate and raises their QP.
        let (grants, _) = governor.allocate(&[video(1, 120.0, SurfaceScale::Half, 100)]);
        let painted = (0..6)
            .map(|_| {
                governor
                    .allocate(&[video(1, 120.0, SurfaceScale::Half, 100)])
                    .0
            })
            .chain(std::iter::once(grants))
            .find(|grants| !grants[0].surfaces.is_empty())
            .expect("half-size pictures paint at 10 a second");
        assert_eq!(painted[0].quality.bitrate_bps, 5 * MBPS);
        assert_eq!(painted[0].quality.min_qp_raise, 8);
        assert_eq!(painted[0].scale, SurfaceScale::Half);
    }

    #[test]
    fn recovery_runs_in_reverse_after_the_probing_holds() {
        let mut governor = governor();
        steps(&mut governor, 40, |governor| {
            video(1, 120.0, governor.scale(), 100)
        });
        let events = steps(&mut governor, 400, |governor| {
            video(1, 2.0, governor.scale(), 100)
        });
        let kinds: Vec<&str> = events
            .iter()
            .map(|step| match step {
                DegradationStep::Scale { .. } => "scale",
                DegradationStep::Quality { .. } => "quality",
                DegradationStep::SurfaceFps { .. } => "fps",
            })
            .collect();
        assert_eq!(
            &kinds[..4],
            &["scale", "scale", "quality", "quality"],
            "{events:?}"
        );
        assert!(kinds[4..].iter().all(|kind| *kind == "fps"), "{events:?}");
        let state = governor.state();
        assert_eq!(
            (state.surface_fps, state.quality_step, state.scale),
            (60, 0, SurfaceScale::Full)
        );
    }

    #[test]
    fn step_d_tiles_coalesce_only_when_content_alone_exceeds_the_budget() {
        let mut governor = governor();
        // Content within budget: granted whole; the surface takes the rest.
        let mut demand = video(1, 4.0, SurfaceScale::Full, 100);
        demand.content = ms(3.0);
        let (grants, _) = governor.allocate(std::slice::from_ref(&demand));
        assert_eq!(grants[0].content, ms(3.0));
        assert_eq!(grants[0].surfaces, vec![0]);
        assert!(!governor.state().coalescing);
        // Content over budget: spread over ticks, surfaces get nothing.
        demand.content = ms(20.0);
        for _ in 0..30 {
            let (grants, _) = governor.allocate(std::slice::from_ref(&demand));
            assert!(grants[0].content < ms(20.0));
            assert!(grants[0].surfaces.is_empty());
            assert!(governor.state().coalescing);
        }
    }

    #[test]
    fn mandatory_work_is_never_refused_and_is_repaid_by_later_ticks() {
        let mut governor = governor();
        let mut demand = video(1, 4.0, SurfaceScale::Full, 100);
        demand.mandatory = ms(40.0);
        let (grants, _) = governor.allocate(std::slice::from_ref(&demand));
        // A mandatory overrun leaves no budget this tick...
        assert!(grants[0].surfaces.is_empty());
        demand.mandatory = Duration::ZERO;
        // ...and is repaid within five ticks.
        let first = (1..=8)
            .find(|_| {
                !governor.allocate(std::slice::from_ref(&demand)).0[0]
                    .surfaces
                    .is_empty()
            })
            .expect("surfaces resume");
        assert_eq!(first, 5);
    }

    #[test]
    fn at_most_two_unacknowledged_frames_carry_optional_work() {
        let mut governor = governor();
        let demand = video(1, 1.0, SurfaceScale::Full, 100);
        let fence = std::cell::Cell::new(0_u64);
        let send = |governor: &mut CanvasGovernor| {
            let (grants, _) = governor.allocate(std::slice::from_ref(&demand));
            let optional = !grants[0].surfaces.is_empty();
            fence.set(fence.get() + 1);
            governor.complete(&[TickOutcome {
                monitor_id: 1,
                generation: 1,
                fence: fence.get(),
                carried_optional: optional,
                measured: Vec::new(),
            }]);
            optional
        };
        assert!(send(&mut governor));
        assert!(send(&mut governor));
        assert_eq!(governor.flow(1).map(CanvasFlow::unacked_optional), Some(2));
        assert!(!send(&mut governor), "a third would be unacknowledged");
        assert!(!send(&mut governor));
        // An acknowledgement of the first frame opens one place.
        let ack = |fence, unprocessed_bytes| CanvasControlMsg::CanvasAck {
            monitor_id: 1,
            generation: 1,
            fence,
            unprocessed_bytes,
            decode_ms_p95: 3,
        };
        assert_eq!(governor.on_control(&ack(1, 0)), None);
        assert!(send(&mut governor));
        assert!(!send(&mut governor));
        // Everything presented, but the Deck holds more than the link
        // carries in 250 ms: still no optional work.
        governor.on_control(&ack(fence.get(), 10 * MBPS));
        assert!(!send(&mut governor));
        governor.on_control(&ack(fence.get(), 0));
        assert!(send(&mut governor));
        // An acknowledgement of an older generation changes nothing, and a
        // reset request names its monitor.
        assert_eq!(
            governor.on_control(&CanvasControlMsg::CanvasResetRequest {
                monitor_id: 1,
                generation: 1,
                reason: "decode".into(),
            }),
            Some(1)
        );
    }

    #[test]
    fn flow_denials_are_not_judged_as_a_slow_host() {
        let mut governor = governor();
        let demand = video(1, 1.0, SurfaceScale::Full, 100);
        for _ in 0..600 {
            let (_, event) = governor.allocate(std::slice::from_ref(&demand));
            assert!(event.is_none());
            // Frames go out with optional work and are never acknowledged.
            governor.complete(&[TickOutcome {
                monitor_id: 1,
                generation: 1,
                fence: 1,
                carried_optional: true,
                measured: Vec::new(),
            }]);
        }
        assert_eq!(governor.state().surface_fps, 60);
    }

    #[test]
    fn idr_and_one_shot_pictures_get_their_share_of_the_link() {
        let mut governor = governor();
        let picture = |monitor_id, picture_pixels| MonitorDemand {
            monitor_id,
            picture_pixels,
            ..MonitorDemand::default()
        };
        let (grants, _) = governor.allocate(&[picture(1, 640 * 360)]);
        assert_eq!(grants[0].quality.bitrate_bps, 10 * MBPS);
        let (grants, _) = governor.allocate(&[picture(1, 640 * 360), picture(2, 640 * 360)]);
        assert_eq!(grants[0].quality.bitrate_bps, 5 * MBPS);
        assert_eq!(grants[1].quality.bitrate_bps, 5 * MBPS);
        // A monitor that paints nothing never gets 0 bps.
        let (grants, _) = governor.allocate(&[picture(1, 640 * 360), picture(2, 0)]);
        assert_eq!(grants[0].quality.bitrate_bps, 10 * MBPS);
        assert!(grants[1].quality.bitrate_bps > 0);
        // A picture on a monitor of stated area gets that area's share:
        // a ninth of a 1080p monitor, a ninth of the link.
        let (grants, _) = governor.allocate(&[MonitorDemand {
            canvas_pixels: 1920 * 1080,
            ..picture(1, 640 * 360)
        }]);
        assert_eq!(grants[0].quality.bitrate_bps, 10 * MBPS / 9);
    }

    #[test]
    fn video_slower_than_the_surface_rate_is_not_judged_as_a_slow_host() {
        // 60 fps pictures of 14 ms: about 35 a second fit, so the rate
        // steps to 30.
        let mut governor = governor();
        let events = steps(&mut governor, 10, |governor| {
            video(1, 14.0, governor.scale(), 100)
        });
        assert_eq!(
            events,
            vec![DegradationStep::SurfaceFps {
                fps: 30,
                previous: 60
            }]
        );
        // Then cheap 24 fps video: a frame waits (pending) for the next due
        // tick and paints once. Nothing steps down again.
        let mut pending = false;
        for tick in 0_u32..120 * 60 {
            if tick * 24 / 60 != (tick + 1) * 24 / 60 {
                pending = true;
            }
            let mut demand = video(1, 1.0, governor.scale(), 100);
            if !pending {
                demand.surfaces.clear();
            }
            let (grants, event) = governor.allocate(std::slice::from_ref(&demand));
            if !grants[0].surfaces.is_empty() {
                pending = false;
            }
            if let Some(event) = event {
                assert!(
                    matches!(event.step, DegradationStep::SurfaceFps { fps, .. } if fps >= 30),
                    "{event:?}"
                );
            }
        }
        assert_eq!(governor.state().quality_step, 0);
    }

    #[test]
    fn one_budget_is_split_by_focus_then_motion() {
        let mut governor = governor();
        // Each surface fits alone, not both.
        let a = video(1, 6.0, SurfaceScale::Full, 50);
        let mut b = video(2, 6.0, SurfaceScale::Full, 400);
        let idle = MonitorDemand {
            monitor_id: 3,
            refine: ms(1.0),
            ..MonitorDemand::default()
        };
        let (grants, _) = governor.allocate(&[a.clone(), b.clone(), idle.clone()]);
        assert!(grants[0].surfaces.is_empty());
        assert_eq!(grants[1].surfaces, vec![0], "more motion first");
        assert_eq!(
            grants[2].refine,
            Duration::ZERO,
            "credit is saved for the waiting surface"
        );
        let mut a = a;
        a.focus = true;
        b.focus = false;
        let (grants, _) = governor.allocate(&[a, b.clone(), idle.clone()]);
        assert_eq!(grants[0].surfaces, vec![0], "focus first");
        assert!(grants[1].surfaces.is_empty());
        // With every surface painted, an idle monitor refines with the rest.
        let mut governor = self::governor();
        let (grants, _) = governor.allocate(&[b, idle]);
        assert_eq!(grants[0].surfaces, vec![0]);
        assert_eq!(grants[1].refine, ms(1.0));
    }
}
