// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The one software encode loop every Pier runs (ADR 0028): Keel →
//! planner → governor → [`SurfaceEncoder`] → canvas records.
//!
//! A capture adapter (Xorg `XShm` + `XDamage`, Wayland `PipeWire`, Windows
//! WGC) hands each picture and whatever damage its OS reported to
//! [`CanvasEncodeLoop::tick`] and writes what comes back. Nothing about
//! *what* to send is decided in a platform directory.
//!
//! A Deck that advertised `canvas_v1` gets canvas records
//! ([`LoopRecord::Canvas`]). An older Deck gets the same loop's degenerate
//! path: the whole monitor is one surface encoded by the same encoder
//! ([`LoopEncoder::encode_full`]) and sent as an ordinary access unit
//! ([`LoopRecord::AccessUnit`]), gated by Keel's damage and idle cadence
//! exactly as before. There is no second software loop.

use std::time::{Duration, Instant};

use arcen_keel::{
    BgraFrame, DamageTracker, EmitMode, ExternalDamage, IdleCadence, KernelPreference,
};
use arcen_protocol::canvas::{CanvasControlMsg, CanvasRecordKind, CanvasResetReason, CanvasTerms};

use super::governor::{CanvasGovernor, CpuShare, GovernorConfig, GovernorEvent, GovernorState};
use super::planner::{CanvasPlanner, CommandMix, PlannerConfig, PlannerError};
use super::surface::{SurfaceEncodeError, SurfaceEncoder};
use super::tile::TilePacker;
use super::{CanvasDigest, CostModel};
use crate::video::StageDamage;

/// The Pier logs its mirror digest at every fence that is a multiple of
/// this, and a Deck-side harness digests the same fences, so the two can
/// be compared line for line (`canvas mirror digest` / `canvas digest`).
pub const CANVAS_DIGEST_EVERY_FENCES: u64 = 30;

/// The log fields of a mirror digest, in the form a Deck-side harness logs
/// its own (`generation= fence= covered= digest=`), so the two compare line
/// for line.
#[must_use]
pub fn digest_fields(generation: u32, digest: &CanvasDigest) -> String {
    use std::fmt::Write as _;
    let mut line = format!(
        "generation={generation} fence={} covered={} digest=",
        digest.fence, digest.covered
    );
    for byte in digest.digest {
        let _ = write!(line, "{byte:02x}");
    }
    line
}

/// One access unit of the degenerate full-frame path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FullFrameUnit {
    /// Annex-B bytes.
    pub bytes: Vec<u8>,
    /// An IDR picture.
    pub keyframe: bool,
}

/// The encoder the loop drives: canvas surfaces, and the whole monitor for
/// a Deck without `canvas_v1`. The Piers implement it with the shared
/// software H.264 session (`crate::video::SoftwareSurfaces`).
pub trait LoopEncoder: SurfaceEncoder {
    /// Full-frame path: stages `frame` (only `damage`) into the
    /// monitor-sized session and encodes it.
    ///
    /// # Errors
    ///
    /// [`SurfaceEncodeError`].
    fn encode_full(
        &mut self,
        frame: BgraFrame<'_>,
        damage: StageDamage<'_>,
        idr: bool,
    ) -> Result<Option<FullFrameUnit>, SurfaceEncodeError>;

    /// Full-frame path: a live rate (the Pier's frame-rate governor).
    fn set_full_frame_rate(&mut self, fps: u32);

    /// Full-frame path: a live bitrate (the Pier's rate controller).
    fn set_full_frame_bitrate(&mut self, bps: u64);
}

/// What one Deck gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopMode {
    /// A Deck without `canvas_v1`: full-frame access units.
    FullFrame,
    /// A Deck that advertised `canvas_v1`, under these terms.
    Canvas(CanvasLoopTerms),
}

/// The canvas half of a loop's configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CanvasLoopTerms {
    /// The Deck's clamped terms.
    pub terms: CanvasTerms,
    /// Session monitor id (0 for a single-monitor session).
    pub monitor_id: u16,
    /// Topology generation (0 for a single-monitor session).
    pub topology_generation: u64,
    /// Stream epoch (0 for a single-monitor session).
    pub stream_epoch: u64,
    /// `video.software.cpu_share`.
    pub cpu_share: CpuShare,
    /// Logical cores the budget is a share of.
    pub logical_cores: u32,
}

/// Everything a loop needs at start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncodeLoopConfig {
    /// Monitor width.
    pub width: u32,
    /// Monitor height.
    pub height: u32,
    /// Session rate (the tick rate).
    pub fps: u32,
    /// Idle re-send interval.
    pub keepalive: Duration,
    /// Starting link bitrate.
    pub initial_bitrate_bps: u64,
    /// Canvas or full frames.
    pub mode: LoopMode,
}

/// One record to write, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoopRecord {
    /// A full-frame access unit (Annex-B).
    AccessUnit {
        /// Bytes.
        bytes: Vec<u8>,
        /// An IDR picture (a recovery point).
        keyframe: bool,
    },
    /// A complete canvas record (frame type `0x0E`).
    Canvas {
        /// Bytes.
        bytes: Vec<u8>,
        /// A `CanvasReset` (a recovery point: what queued before it is
        /// superseded).
        reset: bool,
    },
}

impl LoopRecord {
    /// The record's bytes.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        match self {
            Self::AccessUnit { bytes, .. } | Self::Canvas { bytes, .. } => bytes,
        }
    }

    /// Whether the record is a recovery point.
    #[must_use]
    pub const fn is_recovery_point(&self) -> bool {
        match self {
            Self::AccessUnit { keyframe, .. } => *keyframe,
            Self::Canvas { reset, .. } => *reset,
        }
    }
}

/// What one tick produced.
#[derive(Debug, Default)]
pub struct LoopTick {
    /// Records in send order (empty when nothing changed).
    pub records: Vec<LoopRecord>,
    /// A degradation step the governor took this tick.
    pub event: Option<GovernorEvent>,
    /// The mirror digest at a [`CANVAS_DIGEST_EVERY_FENCES`] fence.
    pub digest: Option<(u32, CanvasDigest)>,
}

/// Why a tick failed. The loop recovers by itself: a canvas starts a new
/// generation at its next tick, a full-frame stream rebuilds its planes
/// and sends a keyframe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoopError {
    /// The canvas planner refused the tick.
    Planner(PlannerError),
    /// The encoder failed.
    Encoder(SurfaceEncodeError),
    /// The picture's size is not the loop's: build a new loop.
    Geometry,
}

impl std::fmt::Display for LoopError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Planner(error) => write!(formatter, "canvas planner: {error}"),
            Self::Encoder(error) => write!(formatter, "encoder: {error}"),
            Self::Geometry => formatter.write_str("picture size changed"),
        }
    }
}

impl std::error::Error for LoopError {}

/// Per-interval counters for the stats line.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EncodeLoopStats {
    /// Pictures handed to the loop.
    pub ticks: u64,
    /// Ticks that produced at least one record.
    pub sent_ticks: u64,
    /// Records written.
    pub records: u64,
    /// Bytes written.
    pub bytes: u64,
    /// Wall time inside [`CanvasEncodeLoop::tick`], milliseconds.
    pub tick_ms_sum: f64,
    /// Slowest tick, milliseconds.
    pub tick_ms_max: f64,
    /// Canvas commands sent.
    pub mix: CommandMix,
    /// Surface pictures encoded (full-frame pictures on that path).
    pub pictures: u64,
    /// Changed blocks still waiting at the end of the interval.
    pub pending_blocks: u32,
    /// Failed ticks.
    pub errors: u64,
}

impl EncodeLoopStats {
    /// The stats line every adapter logs once a second.
    #[must_use]
    pub fn line(&self, canvas: bool, state: Option<GovernorState>) -> String {
        use std::fmt::Write as _;
        #[allow(clippy::cast_precision_loss)]
        let mean = if self.ticks == 0 {
            0.0
        } else {
            self.tick_ms_sum / self.ticks as f64
        };
        let mut line = format!(
            "software_loop={} ticks={} sent={} records={} bytes={} kbps={} tick_ms_mean={mean:.2} tick_ms_max={:.2} pictures={} errors={}",
            if canvas { "canvas" } else { "full_frame" },
            self.ticks,
            self.sent_ticks,
            self.records,
            self.bytes,
            self.bytes.saturating_mul(8) / 1000,
            self.tick_ms_max,
            self.pictures,
            self.errors,
        );
        if canvas {
            let mix = self.mix;
            let _ = write!(
                line,
                " fill={} copy={} tile={} cache_store={} cache_paint={} surface_open={} surface_paint={} pending_blocks={}",
                mix.fill,
                mix.copy,
                mix.tile,
                mix.cache_store,
                mix.cache_paint,
                mix.surface_open,
                mix.surface_paint,
                self.pending_blocks,
            );
            if let Some(state) = state {
                let _ = write!(
                    line,
                    " budget_ms={:.2} surface_fps={} quality_step={} scale={} coalescing={} link_bps={}",
                    state.budget.as_secs_f64() * 1000.0,
                    state.surface_fps,
                    state.quality_step,
                    state.scale.token(),
                    state.coalescing,
                    state.link_bps,
                );
            }
        }
        line
    }
}

/// The full-frame half: Keel's tracker and idle cadence over the
/// monitor-sized session (what `SoftwareEncodeLoop` and the Xorg loop did).
#[derive(Debug)]
struct FullFrame {
    tracker: Option<DamageTracker>,
    cadence: IdleCadence,
    staged_any: bool,
    idr: bool,
    last_emit: Option<Instant>,
}

/// The canvas half: one planner (one monitor) and the session's governor.
#[derive(Debug)]
struct Canvas {
    planner: CanvasPlanner,
    governor: CanvasGovernor,
    monitor_id: u16,
    last_sent: Option<Instant>,
    keepalive: Duration,
}

#[derive(Debug)]
enum Inner {
    Full(Box<FullFrame>),
    Canvas(Box<Canvas>),
}

/// The one software encode loop.
pub struct CanvasEncodeLoop<E> {
    encoder: E,
    packer: Box<dyn TilePacker + Send>,
    inner: Inner,
    width: u32,
    height: u32,
    keepalive: Duration,
    started: Instant,
    stats: EncodeLoopStats,
}

impl<E: std::fmt::Debug> std::fmt::Debug for CanvasEncodeLoop<E> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CanvasEncodeLoop")
            .field("encoder", &self.encoder)
            .field("inner", &self.inner)
            .field("width", &self.width)
            .field("height", &self.height)
            .finish_non_exhaustive()
    }
}

impl<E: LoopEncoder> CanvasEncodeLoop<E> {
    /// A loop over `encoder`; `packer` packs canvas tiles (unused on the
    /// full-frame path).
    ///
    /// # Errors
    ///
    /// [`LoopError::Planner`] when the monitor does not fit the Deck's
    /// canvas.
    pub fn new(
        config: EncodeLoopConfig,
        encoder: E,
        packer: Box<dyn TilePacker + Send>,
        now: Instant,
    ) -> Result<Self, LoopError> {
        let inner = match config.mode {
            LoopMode::FullFrame => Inner::Full(Box::new(FullFrame {
                // Without a tracker every picture is converted in full,
                // which is what a degraded tracker would report anyway.
                tracker: DamageTracker::new(
                    config.width as usize,
                    config.height as usize,
                    KernelPreference::Auto,
                )
                .ok(),
                cadence: IdleCadence::new(config.keepalive),
                staged_any: false,
                idr: true,
                last_emit: None,
            })),
            LoopMode::Canvas(canvas) => {
                let planner = CanvasPlanner::new(
                    canvas.monitor_id,
                    canvas.topology_generation,
                    canvas.stream_epoch,
                    config.width,
                    config.height,
                    canvas.terms,
                    PlannerConfig {
                        keepalive: config.keepalive,
                        ..PlannerConfig::default()
                    },
                )
                .map_err(LoopError::Planner)?;
                let governor = CanvasGovernor::new(
                    GovernorConfig::new(
                        config.fps.max(1),
                        canvas.logical_cores.max(1),
                        canvas.cpu_share,
                        config.initial_bitrate_bps,
                    ),
                    CostModel::default(),
                );
                Inner::Canvas(Box::new(Canvas {
                    planner,
                    governor,
                    monitor_id: canvas.monitor_id,
                    last_sent: None,
                    keepalive: config.keepalive,
                }))
            }
        };
        Ok(Self {
            encoder,
            packer,
            inner,
            width: config.width,
            height: config.height,
            keepalive: config.keepalive,
            started: now,
            stats: EncodeLoopStats::default(),
        })
    }

    /// Whether this loop sends canvas records.
    #[must_use]
    pub const fn is_canvas(&self) -> bool {
        matches!(self.inner, Inner::Canvas(_))
    }

    /// The encoder (live session details for logs).
    pub const fn encoder(&self) -> &E {
        &self.encoder
    }

    /// The encoder, mutably.
    pub const fn encoder_mut(&mut self) -> &mut E {
        &mut self.encoder
    }

    /// The governor's position (canvas only), for READY and Stats.
    #[must_use]
    pub fn governor_state(&self) -> Option<GovernorState> {
        match &self.inner {
            Inner::Canvas(canvas) => Some(canvas.governor.state()),
            Inner::Full(_) => None,
        }
    }

    /// Whether the adapter should capture a picture now: something
    /// changed (`damage_pending`; pass `true` for a source without damage
    /// reports), work is still waiting, a keyframe or reset is due, or the
    /// keepalive is.
    #[must_use]
    pub fn wants_frame(&self, now: Instant, damage_pending: bool) -> bool {
        match &self.inner {
            Inner::Full(full) => {
                !full.staged_any
                    || damage_pending
                    || full.idr
                    || full
                        .last_emit
                        .is_none_or(|at| now.saturating_duration_since(at) >= self.keepalive)
            }
            Inner::Canvas(canvas) => {
                damage_pending
                    || canvas.planner.reset_pending()
                    || canvas.planner.pending_blocks() > 0
                    || canvas
                        .last_sent
                        .is_none_or(|at| now.saturating_duration_since(at) >= canvas.keepalive)
            }
        }
    }

    /// Everything must be sent again: a keyframe on the full-frame path; a
    /// new canvas generation (reason `recovery`): the Pier dropped
    /// records, or the Deck asked for a full picture.
    pub fn request_recovery(&mut self) {
        match &mut self.inner {
            Inner::Full(full) => full.idr = true,
            Inner::Canvas(canvas) => canvas.planner.request_reset(CanvasResetReason::Recovery),
        }
    }

    /// A Deck's canvas control message (already validated by the Pier).
    pub fn on_control(&mut self, message: &CanvasControlMsg) {
        if let Inner::Canvas(canvas) = &mut self.inner
            && let Some(monitor) = canvas.governor.on_control(message)
            && monitor == canvas.monitor_id
        {
            canvas
                .planner
                .request_reset(CanvasResetReason::ResetRequest);
        }
    }

    /// Local input acted at `(x, y)` (monitor pixels): its neighbourhood
    /// is never deferred.
    pub fn note_input(&mut self, x: u32, y: u32, now: Instant) {
        if let Inner::Canvas(canvas) = &mut self.inner {
            let at = now.saturating_duration_since(self.started);
            canvas.planner.note_input(x, y, at);
            canvas.planner.set_focus(true);
        }
    }

    /// The link bitrate (the Pier's rate controller).
    pub fn set_link_bitrate(&mut self, bps: u64) {
        match &mut self.inner {
            Inner::Full(_) => self.encoder.set_full_frame_bitrate(bps),
            Inner::Canvas(canvas) => canvas.governor.set_link_bps(bps),
        }
    }

    /// A live session rate from the Pier's frame-rate governor. The
    /// full-frame path applies it; a canvas paces its surfaces with its
    /// own governor and keeps ticking at the session rate, so it returns
    /// `false` (the adapter keeps its tick interval).
    pub fn set_frame_rate(&mut self, fps: u32) -> bool {
        match &mut self.inner {
            Inner::Full(_) => {
                self.encoder.set_full_frame_rate(fps);
                true
            }
            Inner::Canvas(_) => false,
        }
    }

    /// Takes and resets the interval counters.
    pub fn take_stats(&mut self) -> EncodeLoopStats {
        let pending = match &self.inner {
            Inner::Canvas(canvas) => canvas.planner.pending_blocks(),
            Inner::Full(_) => 0,
        };
        let mut stats = std::mem::take(&mut self.stats);
        stats.pending_blocks = pending;
        stats
    }

    /// The canvas mirror's digest now (canvas only), and its generation.
    #[must_use]
    pub fn mirror_digest(&self) -> Option<(u32, CanvasDigest)> {
        match &self.inner {
            Inner::Canvas(canvas) => canvas
                .planner
                .mirror()
                .digest()
                .map(|digest| (canvas.planner.generation(), digest)),
            Inner::Full(_) => None,
        }
    }

    /// Runs one tick over the monitor's current picture. `os_damage`, when
    /// the capture reports it, narrows Keel's hashing and supplies move
    /// hints.
    ///
    /// # Errors
    ///
    /// [`LoopError`]; the loop recovers at its next tick, except after
    /// [`LoopError::Geometry`].
    pub fn tick(
        &mut self,
        frame: BgraFrame<'_>,
        os_damage: Option<&ExternalDamage>,
        now: Instant,
    ) -> Result<LoopTick, LoopError> {
        let grid = frame.grid();
        if grid.width() != self.width as usize || grid.height() != self.height as usize {
            return Err(LoopError::Geometry);
        }
        let started = Instant::now();
        let result = match &mut self.inner {
            Inner::Full(full) => tick_full(
                full,
                &mut self.encoder,
                frame,
                os_damage,
                now,
                &mut self.stats,
            ),
            Inner::Canvas(canvas) => tick_canvas(
                canvas,
                &mut self.encoder,
                self.packer.as_mut(),
                frame,
                os_damage,
                now.saturating_duration_since(self.started),
                now,
                &mut self.stats,
            ),
        };
        let ms = started.elapsed().as_secs_f64() * 1000.0;
        self.stats.ticks += 1;
        self.stats.tick_ms_sum += ms;
        self.stats.tick_ms_max = self.stats.tick_ms_max.max(ms);
        match &result {
            Ok(tick) if !tick.records.is_empty() => {
                self.stats.sent_ticks += 1;
                self.stats.records += tick.records.len() as u64;
                self.stats.bytes += tick
                    .records
                    .iter()
                    .map(|record| record.bytes().len() as u64)
                    .sum::<u64>();
            }
            Ok(_) => {}
            Err(_) => self.stats.errors += 1,
        }
        result
    }
}

fn tick_full<E: LoopEncoder>(
    full: &mut FullFrame,
    encoder: &mut E,
    frame: BgraFrame<'_>,
    os_damage: Option<&ExternalDamage>,
    now: Instant,
    stats: &mut EncodeLoopStats,
) -> Result<LoopTick, LoopError> {
    let summary = full.tracker.as_mut().map(|tracker| match os_damage {
        Some(damage) => tracker.update_within(frame, damage.damage_map()),
        None => tracker.update(frame),
    });
    let (changed, whole) = match summary {
        Some(Ok(summary)) if !full.staged_any || summary.is_full_damage() => (true, true),
        Some(Ok(summary)) if summary.is_clean() => (false, false),
        Some(Ok(_)) => (true, false),
        Some(Err(_)) | None => (true, true),
    };
    if changed {
        full.cadence.note_frame();
    }
    let since = full
        .last_emit
        .map_or(Duration::MAX, |at| now.saturating_duration_since(at));
    let Some(mode) = full.cadence.decision(full.idr, since) else {
        if changed {
            // Keel took the change as its baseline but nothing staged it:
            // the next encode rebuilds the planes in full.
            if let Some(tracker) = full.tracker.as_mut() {
                tracker.reset();
            }
            full.staged_any = false;
        }
        return Ok(LoopTick::default());
    };
    let damage = match (changed, whole, full.tracker.as_ref()) {
        (false, _, _) => StageDamage::Clean,
        (true, false, Some(tracker)) => StageDamage::Rows(tracker.damage_map()),
        (true, _, _) => StageDamage::Full,
    };
    let idr = full.idr || matches!(mode, EmitMode::FirstFrame | EmitMode::Idr);
    match encoder.encode_full(frame, damage, idr) {
        Ok(Some(unit)) => {
            full.staged_any = true;
            full.idr = false;
            full.cadence.on_submitted();
            full.last_emit = Some(now);
            stats.pictures += 1;
            Ok(LoopTick {
                records: vec![LoopRecord::AccessUnit {
                    keyframe: unit.keyframe,
                    bytes: unit.bytes,
                }],
                ..LoopTick::default()
            })
        }
        Ok(None) => {
            full.staged_any = true;
            Ok(LoopTick::default())
        }
        Err(error) => {
            // The planes may be partly written: rebuild them in full and
            // start the chain again.
            if let Some(tracker) = full.tracker.as_mut() {
                tracker.reset();
            }
            full.staged_any = false;
            full.idr = true;
            Err(LoopError::Encoder(error))
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn tick_canvas<E: LoopEncoder>(
    canvas: &mut Canvas,
    encoder: &mut E,
    packer: &mut (dyn TilePacker + Send),
    frame: BgraFrame<'_>,
    os_damage: Option<&ExternalDamage>,
    at: Duration,
    now: Instant,
    stats: &mut EncodeLoopStats,
) -> Result<LoopTick, LoopError> {
    let demand = canvas
        .planner
        .observe(
            frame,
            os_damage,
            at,
            canvas.governor.costs(),
            canvas.governor.scale(),
        )
        .map_err(LoopError::Planner)?;
    let (grants, event) = canvas.governor.allocate(std::slice::from_ref(&demand));
    let grant = grants
        .into_iter()
        .find(|grant| grant.monitor_id == canvas.monitor_id)
        .ok_or(LoopError::Planner(PlannerError::NotObserved))?;
    let timestamp_ms = u32::try_from(at.as_millis() % (1u128 << 32)).unwrap_or(0);
    let planned = canvas
        .planner
        .plan(frame, &grant, timestamp_ms, encoder, packer)
        .map_err(LoopError::Planner)?;
    canvas
        .governor
        .complete(std::slice::from_ref(&planned.outcome(canvas.monitor_id)));
    stats.mix.add(planned.mix);
    stats.pictures += u64::from(planned.surfaces_painted);
    if !planned.records.is_empty() {
        canvas.last_sent = Some(now);
    }
    let digest = (planned.fence != 0 && planned.fence % CANVAS_DIGEST_EVERY_FENCES == 0)
        .then(|| canvas.planner.mirror().digest())
        .flatten()
        .map(|digest| (canvas.planner.generation(), digest));
    let records = planned
        .records
        .into_iter()
        .map(|bytes| {
            let reset = bytes.get(2).copied() == Some(CanvasRecordKind::Reset.wire());
            LoopRecord::Canvas { bytes, reset }
        })
        .collect();
    Ok(LoopTick {
        records,
        event,
        digest,
    })
}
