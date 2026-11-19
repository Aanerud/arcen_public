// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The Deck's canvas (ADR 0028): the shared half every Deck runs.
//!
//! A Deck that streams through the canvas keeps one persistent eight-bit
//! canvas per monitor and applies the Pier's ordered canvas records to it.
//! Everything here is OS-free; a platform supplies only a GPU executor
//! ([`crate::present::VideoPresenterSink::execute`]) and the decoders it
//! already has ([`crate::decode::VideoDecoderFactory`]).
//!
//! - [`CanvasRequest`]: what the Deck advertises (`AuthResponse.canvas_v1`).
//!   Built from the sink's [`crate::present::VideoPresenterSink::CANVAS_EXECUTOR`]
//!   and the user's switch, so a Deck whose sink has no executor never asks.
//! - [`DeckCanvas`]: one connection's canvases. It admits each record
//!   ([`admit`]: the hello's per-monitor state, then region-frame admission),
//!   validates it with the shared state owner
//!   ([`arcen_media::canvas::CanvasState`], which keeps fences and
//!   generations), decodes tiles and surface access units (one decoder per
//!   surface, at most [`DECK_SURFACES_PER_MONITOR`] per monitor) and hands
//!   the work to the sink, which applies it before it presents the canvas.
//!   It keeps flow control (`canvas_ack`), recovery (`canvas_reset_request`),
//!   the canvas digest and the statistics.
//!
//! The cache budget is [`DEFAULT_CACHE_MIB`] (as a remote-desktop graphics
//! client's large cache), never below [`MIN_CACHE_MIB`] (its thin-client
//! cache), with one slot per [`CACHE_SLOT_BYTES`].

use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

use arcen_media::canvas::{
    CanvasDigest, CanvasOp, CanvasState, CanvasWork, TileDecoder, canvas_digest,
};
use arcen_media::refine::tile::{TileCodes, TileRect};
use arcen_media::video::{DecodeLatencyPolicy, PresentTiming};
use arcen_media::{MonitorRoute, RegionFrameRoster, TopologyGeneration};
use arcen_protocol::canvas::{
    CANVAS_PREFIX_BYTES, CanvasCapability, CanvasControlMsg, CanvasRecord, CanvasScope,
    CanvasStateMsg, CanvasStatusMsg, CanvasTerms, MAX_CACHE_BYTES, MAX_CACHE_SLOTS,
    MAX_CANVAS_SIDE,
};
use arcen_protocol::wire::{BitDepth, ColorMatrix, ColorRange};
use arcen_protocol::{ChromaSubsampling, FrameType, VideoCodec, VideoHeader};

use crate::decode::{NativeDecodedVideoFrame, SessionColor, VideoDecoderBackend};
use crate::present::{
    CanvasExecution, CanvasExecutorFacts, CanvasOpInput, CanvasOutcome, CanvasReadback,
    VideoPresenterSink,
};

/// Default cache budget in MiB (O3): 25,600 slots of [`CACHE_SLOT_BYTES`].
pub const DEFAULT_CACHE_MIB: u32 = 100;
/// The smallest cache budget in MiB a Deck advertises: 4,096 slots.
pub const MIN_CACHE_MIB: u32 = 16;
/// Cache bytes per slot.
pub const CACHE_SLOT_BYTES: u64 = 4096;
/// Surfaces (H.264 decoders) per monitor this Deck runs at most.
pub const DECK_SURFACES_PER_MONITOR: u8 = 2;
/// A monitor that keeps receiving records is acknowledged at least this
/// often, whether or not a frame was presented (at least 4 Hz).
pub const ACK_INTERVAL: Duration = Duration::from_millis(250);
/// A reset request is repeated at most this often while the canvas stays
/// lost.
pub const RESET_REQUEST_RETRY: Duration = Duration::from_secs(2);
/// The canvas digest is read back at most this often per monitor, at the
/// next fence that is a multiple of
/// [`arcen_media::canvas::CANVAS_DIGEST_EVERY_FENCES`].
pub const DIGEST_INTERVAL: Duration = Duration::from_secs(10);
/// The surface decode time percentile covers this window.
pub const DECODE_WINDOW: Duration = Duration::from_secs(1);
/// The statistics' tile/surface/copy shares cover two windows of this.
pub const STATS_WINDOW: Duration = Duration::from_secs(5);

const MIB: u64 = 1024 * 1024;
/// Bytes a decoded tile holds per pixel until the sink runs (three `u16`
/// planes).
const TILE_CODE_BYTES_PER_PIXEL: u64 = 6;
/// Decoded tile bytes the Deck holds before it hands a record's work to the
/// sink in parts (about eleven million pixels of tiles).
pub const PART_TILE_BYTES: u64 = 64 * MIB;

/// The cache budget a Deck advertises for a configured size in MiB:
/// at least [`MIN_CACHE_MIB`], at most the protocol's bound.
#[must_use]
pub fn cache_budget_bytes(configured_mib: u32) -> u64 {
    (u64::from(configured_mib) * MIB).clamp(u64::from(MIN_CACHE_MIB) * MIB, MAX_CACHE_BYTES)
}

/// Cache slots for a budget: one per [`CACHE_SLOT_BYTES`], within the
/// protocol's bound.
#[must_use]
pub fn cache_slots(budget_bytes: u64) -> u32 {
    u32::try_from(budget_bytes / CACHE_SLOT_BYTES)
        .unwrap_or(u32::MAX)
        .min(MAX_CACHE_SLOTS)
}

/// Why this Deck does or does not ask for the canvas.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum CanvasRequest {
    /// This Deck cannot hold a canvas: its sink has no canvas executor, or
    /// this build cannot decode tiles. Nothing is advertised or shown.
    #[default]
    Unavailable,
    /// The user switched it off (Settings, or this connection).
    Off,
    /// Advertised as `canvas_v1`.
    Requested(CanvasCapability),
}

impl CanvasRequest {
    /// The request for a Deck whose canvases are executed by `P`: only a
    /// sink that states a canvas executor
    /// ([`VideoPresenterSink::CANVAS_EXECUTOR`]), in a build that decodes
    /// tiles (`runtime`), with the user's switch on.
    #[must_use]
    pub fn for_sink<P: VideoPresenterSink>(enabled: bool, cache_mib: u32) -> Self {
        Self::for_executor(
            P::CANVAS_EXECUTOR,
            cfg!(feature = "runtime"),
            enabled,
            cache_mib,
        )
    }

    /// [`Self::for_sink`] from its facts.
    #[must_use]
    pub fn for_executor(
        executor: Option<CanvasExecutorFacts>,
        decodes_tiles: bool,
        enabled: bool,
        cache_mib: u32,
    ) -> Self {
        let Some(executor) = executor.filter(|_| decodes_tiles) else {
            return Self::Unavailable;
        };
        if !enabled {
            return Self::Off;
        }
        let cache_bytes = cache_budget_bytes(cache_mib);
        Self::Requested(CanvasCapability {
            max_canvas_px: executor.max_canvas_px.min(MAX_CANVAS_SIDE),
            max_surfaces_per_monitor: DECK_SURFACES_PER_MONITOR,
            cache_bytes,
            cache_slots: cache_slots(cache_bytes),
            tile_codecs: vec![arcen_protocol::refine::CODEC_DELTA_ZSTD1_TOKEN.to_owned()],
        })
    }

    /// What `AuthResponse.canvas_v1` carries.
    #[must_use]
    pub const fn capability(&self) -> Option<&CanvasCapability> {
        match self {
            Self::Requested(capability) => Some(capability),
            Self::Unavailable | Self::Off => None,
        }
    }
}

/// The monitor a canvas record names, read from its prefix without decoding
/// it (the record is decoded and checked in full by [`DeckCanvas::apply`]).
#[must_use]
pub fn record_monitor(bytes: &[u8]) -> Option<u16> {
    if bytes.len() < CANVAS_PREFIX_BYTES || bytes.first() != Some(&(FrameType::Canvas as u8)) {
        return None;
    }
    Some(u16::from_be_bytes([bytes[4], bytes[5]]))
}

/// Whether a canvas record is a reset (kind 1), read from its prefix.
#[must_use]
pub fn record_is_reset(bytes: &[u8]) -> bool {
    bytes.len() >= CANVAS_PREFIX_BYTES
        && bytes.first() == Some(&(FrameType::Canvas as u8))
        && bytes[2] == arcen_protocol::canvas::CanvasRecordKind::Reset.wire()
}

/// Why a record was not admitted. Admission never breaks a canvas: a record
/// of a replaced topology or stream is stale, not wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanvasAdmissionError {
    /// The hello did not list the monitor `on`.
    MonitorOff,
    /// A single-monitor session takes monitor 0 with a zero topology
    /// generation and stream epoch, like its legacy video records.
    NotLegacyPrimary,
    /// A record of a multi-monitor topology arrived before the Deck
    /// committed its roster: it cannot be admitted yet, and the canvas it
    /// would have started is lost.
    RosterPending,
    /// Region-frame admission refused the record's topology generation or
    /// monitor.
    Region(arcen_media::RegionFrameAdmissionError),
    /// The record names another stream epoch than the monitor's.
    StaleStreamEpoch,
}

/// Admits a record's scope: the monitor is `on` in the hello, then
/// region-frame admission. Without a committed multi-monitor roster the
/// session is single-monitor: monitor 0, topology generation 0, stream epoch
/// 0, as its legacy video records. With one, the committed topology
/// generation, a monitor of the roster and that monitor's stream epoch.
///
/// # Errors
///
/// [`CanvasAdmissionError`].
pub fn admit(
    scope: &CanvasScope,
    status: &CanvasStatusMsg,
    roster: Option<&RegionFrameRoster>,
) -> Result<(), CanvasAdmissionError> {
    if !status.is_on_for(scope.monitor_id) {
        return Err(CanvasAdmissionError::MonitorOff);
    }
    let Some(roster) = roster else {
        return if scope.monitor_id == 0 && scope.topology_generation == 0 && scope.stream_epoch == 0
        {
            Ok(())
        } else if scope.monitor_id != 0 && scope.topology_generation != 0 {
            Err(CanvasAdmissionError::RosterPending)
        } else {
            Err(CanvasAdmissionError::NotLegacyPrimary)
        };
    };
    let route = MonitorRoute::from_wire_monitor_id(scope.monitor_id);
    let generation = TopologyGeneration::new(scope.topology_generation).map_err(|_| {
        CanvasAdmissionError::Region(arcen_media::RegionFrameAdmissionError::StaleGeneration {
            expected: roster.generation().get(),
            actual: 0,
        })
    })?;
    roster
        .admit_route(generation, route)
        .map_err(CanvasAdmissionError::Region)?;
    match roster.plan(route) {
        Some(plan) if plan.stream_epoch.get() != scope.stream_epoch => {
            Err(CanvasAdmissionError::StaleStreamEpoch)
        }
        _ => Ok(()),
    }
}

/// A tile's codes as opaque BGRA8 rows, `None` when a code exceeds eight
/// bits (a tile the state owner accepted never does).
#[must_use]
pub fn tile_bgra(codes: &TileCodes) -> Option<Vec<u8>> {
    let pixels = codes.width() as usize * codes.height() as usize;
    let (r, g, b) = (codes.plane(0), codes.plane(1), codes.plane(2));
    let mut bgra = Vec::with_capacity(pixels * 4);
    for at in 0..pixels {
        bgra.extend_from_slice(&[
            u8::try_from(*b.get(at)?).ok()?,
            u8::try_from(*g.get(at)?).ok()?,
            u8::try_from(*r.get(at)?).ok()?,
            255,
        ]);
    }
    Some(bgra)
}

/// Whether the session streams through the canvas, as the Deck knows it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum CanvasSessionState {
    /// The host has not answered yet.
    #[default]
    Pending,
    /// The host sends canvas records for at least one monitor.
    On,
    /// The host sends full frames; the stable reason token.
    Off(String),
}

/// The newest canvas digest the Deck computed from its own canvas.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CanvasDigestReport {
    /// Monitor.
    pub monitor_id: u16,
    /// Generation.
    pub generation: u32,
    /// Refine's canonical digest over the canvas's exact blocks.
    pub digest: CanvasDigest,
}

/// The canvas statistics of one connection.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CanvasStats {
    /// What this Deck asked for.
    pub request: CanvasRequestState,
    /// What the host answered.
    pub session: CanvasSessionState,
    /// Records applied to a canvas.
    pub records_applied: u64,
    /// Records whose canvas was handed to the presenter.
    pub records_presented: u64,
    /// Resets applied.
    pub resets: u64,
    /// Records the state owner refused (each breaks the canvas).
    pub refusals: u64,
    /// Records dropped before the state owner: not admitted, for a monitor
    /// not `on`, or while the canvas waits for a reset.
    pub dropped: u64,
    /// `canvas_reset_request`s sent.
    pub reset_requests: u64,
    /// Canvas pixels written by lossless tiles, fills and cache paints over
    /// the last [`STATS_WINDOW`]s.
    pub tile_pixels: u64,
    /// Canvas pixels written by surface paints over the same windows.
    pub surface_pixels: u64,
    /// Canvas pixels moved by copies over the same windows.
    pub copy_pixels: u64,
    /// Surfaces open across monitors.
    pub open_surfaces: usize,
    /// Worst monitor's 95th-percentile surface decode time over the last
    /// [`DECODE_WINDOW`], in milliseconds.
    pub decode_ms_p95: u32,
    /// Why canvas work was refused by this Deck's sink, if it was.
    pub sink_refused: Option<String>,
    /// The newest digest.
    pub last_digest: Option<CanvasDigestReport>,
}

/// [`CanvasRequest`] without the capability, for the statistics.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CanvasRequestState {
    /// Not offered by this Deck.
    #[default]
    Unavailable,
    /// Off by the user's choice.
    Off,
    /// Asked for.
    Requested,
}

impl From<&CanvasRequest> for CanvasRequestState {
    fn from(request: &CanvasRequest) -> Self {
        match request {
            CanvasRequest::Unavailable => Self::Unavailable,
            CanvasRequest::Off => Self::Off,
            CanvasRequest::Requested(_) => Self::Requested,
        }
    }
}

fn percent(part: u64, total: u64) -> u64 {
    (part.saturating_mul(100) + total / 2)
        .checked_div(total)
        .unwrap_or(0)
}

impl CanvasStats {
    /// The Stream Statistics line: "Screen updates: canvas (tiles x%,
    /// surface y%, copy z%)", or why the Deck receives full frames. `None`
    /// for a Deck that cannot hold a canvas.
    #[must_use]
    pub fn line(&self) -> Option<String> {
        match self.request {
            CanvasRequestState::Unavailable => return None,
            CanvasRequestState::Off => {
                return Some("Screen updates: full frames (canvas off in Settings)".to_owned());
            }
            CanvasRequestState::Requested => {}
        }
        if let Some(reason) = &self.sink_refused {
            return Some(format!(
                "Screen updates: canvas refused by this Deck ({reason})"
            ));
        }
        Some(match &self.session {
            CanvasSessionState::Pending => {
                "Screen updates: canvas (waiting for the host)".to_owned()
            }
            CanvasSessionState::Off(reason) => format!(
                "Screen updates: full frames (canvas off: {})",
                reason.replace('_', " ")
            ),
            CanvasSessionState::On => {
                let total = self.tile_pixels + self.surface_pixels + self.copy_pixels;
                if total == 0 {
                    "Screen updates: canvas".to_owned()
                } else {
                    format!(
                        "Screen updates: canvas (tiles {}%, surface {}%, copy {}%)",
                        percent(self.tile_pixels, total),
                        percent(self.surface_pixels, total),
                        percent(self.copy_pixels, total),
                    )
                }
            }
        })
    }
}

/// What the Deck needs to apply one record besides the record itself.
pub struct CanvasEnv<'e, D: VideoDecoderBackend, P> {
    /// Now.
    pub now: Instant,
    /// Creates a surface decoder (the Deck's `VideoDecoderFactory`).
    pub decoders: &'e dyn Fn() -> D,
    /// Decodes tile payloads (`delta-zstd1` at depth 8).
    pub tiles: &'e mut dyn TileDecoder,
    /// The sink that executes and presents this monitor's canvas; `None`
    /// when the monitor has no presenter yet.
    pub sink: Option<&'e mut P>,
    /// When to show the canvas.
    pub timing: PresentTiming,
}

impl<D: VideoDecoderBackend, P> std::fmt::Debug for CanvasEnv<'_, D, P> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CanvasEnv")
            .field("now", &self.now)
            .field("sink", &self.sink.is_some())
            .field("timing", &self.timing)
            .finish_non_exhaustive()
    }
}

/// What became of one record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanvasApplied {
    /// Applied, and the canvas was handed to the presenter.
    Presented,
    /// Applied; the canvas is not shown now.
    Applied,
    /// Not applied: the canvas is not on, the record was not admitted, or
    /// the monitor's canvas waits for a reset.
    Dropped,
    /// Refused or failed: the canvas is lost and a reset is requested.
    Lost(&'static str),
    /// The sink has no canvas executor: canvas records are ignored for the
    /// rest of the connection.
    SinkRefused,
}

struct MonitorCanvas<D> {
    state: CanvasState,
    surfaces: Vec<SurfaceSlot<D>>,
    /// Why the canvas was lost; `Some` until the next reset applies.
    lost: Option<&'static str>,
    last_reset_request: Option<Instant>,
    /// Newest fence applied in the current generation.
    applied_fence: u64,
    presented_since_ack: bool,
    records_since_ack: bool,
    last_ack: Option<Instant>,
    decode_ms: VecDeque<(Instant, f64)>,
    last_digest_request: Option<Instant>,
    pending_digest: Option<(u32, u64, CanvasState)>,
}

impl<D> MonitorCanvas<D> {
    fn new(monitor_id: u16, terms: CanvasTerms) -> Self {
        Self {
            state: CanvasState::new(monitor_id, terms),
            surfaces: (0..terms.max_surfaces_per_monitor)
                .map(|_| SurfaceSlot::Closed)
                .collect(),
            lost: None,
            last_reset_request: None,
            applied_fence: 0,
            presented_since_ack: false,
            records_since_ack: false,
            last_ack: None,
            decode_ms: VecDeque::new(),
            last_digest_request: None,
            pending_digest: None,
        }
    }

    fn generation(&self) -> u32 {
        self.state.scope().map_or(0, |scope| scope.generation.get())
    }

    fn lose(&mut self, reason: &'static str) {
        self.state.mark_broken();
        self.lost = Some(reason);
        self.pending_digest = None;
        for surface in &mut self.surfaces {
            *surface = SurfaceSlot::Closed;
        }
    }

    fn decode_p95(&mut self, now: Instant) -> u32 {
        while self
            .decode_ms
            .front()
            .is_some_and(|(at, _)| now.saturating_duration_since(*at) > DECODE_WINDOW)
        {
            self.decode_ms.pop_front();
        }
        let mut samples: Vec<f64> = self.decode_ms.iter().map(|(_, ms)| *ms).collect();
        if samples.is_empty() {
            return 0;
        }
        samples.sort_by(f64::total_cmp);
        let rank = (samples.len() * 95).div_ceil(100).max(1) - 1;
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "milliseconds of one decode, rounded up and clamped"
        )]
        let ms = samples[rank].ceil().clamp(0.0, f64::from(u32::MAX)) as u32;
        ms
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct PixelShares {
    tiles: u64,
    surface: u64,
    copy: u64,
}

/// One connection's canvases: admission, validation, decode, execution,
/// flow control, recovery, digests and statistics (see the module docs).
pub struct DeckCanvas<D: VideoDecoderBackend> {
    terms: Option<CanvasTerms>,
    status: Option<CanvasStatusMsg>,
    roster: Option<RegionFrameRoster>,
    session_color: SessionColor,
    latency: DecodeLatencyPolicy,
    monitors: BTreeMap<u16, MonitorCanvas<D>>,
    stats: CanvasStats,
    sink_refused: bool,
    part_tile_bytes: u64,
    window_started: Option<Instant>,
    current: PixelShares,
    previous: PixelShares,
}

impl<D: VideoDecoderBackend> std::fmt::Debug for DeckCanvas<D> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DeckCanvas")
            .field("terms", &self.terms)
            .field("monitors", &self.monitors.keys().collect::<Vec<_>>())
            .field("stats", &self.stats)
            .finish_non_exhaustive()
    }
}

impl<D: VideoDecoderBackend> DeckCanvas<D> {
    /// The canvases of a connection that sent `request`, before its hello.
    #[must_use]
    pub fn new(request: &CanvasRequest) -> Self {
        Self {
            terms: request.capability().and_then(CanvasCapability::terms),
            status: None,
            roster: None,
            session_color: SessionColor::default(),
            latency: DecodeLatencyPolicy::default(),
            monitors: BTreeMap::new(),
            stats: CanvasStats {
                request: request.into(),
                ..CanvasStats::default()
            },
            sink_refused: false,
            part_tile_bytes: PART_TILE_BYTES,
            window_started: None,
            current: PixelShares::default(),
            previous: PixelShares::default(),
        }
    }

    /// The host's answer (`ServerHelloMsg.canvas`), checked against what
    /// this Deck offered (`CanvasStatusMsg::validate`); an invalid answer
    /// reads as off. A new hello starts every canvas over.
    pub fn on_hello(&mut self, status: Option<&CanvasStatusMsg>) {
        self.monitors.clear();
        self.status = None;
        let Some(terms) = self.terms else {
            self.stats.session = CanvasSessionState::Off("not_requested".to_owned());
            return;
        };
        let Some(status) = status else {
            self.stats.session = CanvasSessionState::Off("not_offered".to_owned());
            return;
        };
        if let Err(error) = status.validate(&terms) {
            tracing::warn!(
                target: crate::log_target::VIDEO,
                %error,
                "the host's canvas status is invalid; canvas is off for this connection"
            );
            self.stats.session = CanvasSessionState::Off("invalid_status".to_owned());
            return;
        }
        self.stats.session = if status.state == CanvasStateMsg::On {
            CanvasSessionState::On
        } else {
            CanvasSessionState::Off(status.reason.clone().unwrap_or_else(|| "off".to_owned()))
        };
        tracing::info!(
            target: crate::log_target::VIDEO,
            state = ?status.state,
            reason = status.reason.as_deref().unwrap_or(""),
            monitors = status.monitors.len(),
            cache_bytes = status.cache_bytes.unwrap_or(0),
            surfaces = status.max_surfaces_per_monitor.unwrap_or(0),
            "canvas negotiated",
        );
        self.status = Some(status.clone());
    }

    /// Whether canvas records are expected for at least one monitor.
    #[must_use]
    pub fn is_on(&self) -> bool {
        self.status
            .as_ref()
            .is_some_and(|status| status.state == CanvasStateMsg::On)
    }

    /// The committed multi-monitor roster records are admitted against
    /// (`None`: a single-monitor session).
    pub fn set_roster(&mut self, roster: Option<RegionFrameRoster>) {
        self.roster = roster;
    }

    /// Whether a roster is committed.
    #[must_use]
    pub const fn has_roster(&self) -> bool {
        self.roster.is_some()
    }

    /// The session's colour, applied to every surface decoder.
    pub fn set_session_color(&mut self, color: SessionColor) {
        self.session_color = color;
        for monitor in self.monitors.values_mut() {
            for slot in &mut monitor.surfaces {
                if let SurfaceSlot::Decoding(decoder) = slot {
                    decoder.set_session_color(color);
                }
            }
        }
    }

    /// The served pipeline's decode latency policy for new surface decoders.
    pub const fn set_decode_latency_policy(&mut self, policy: DecodeLatencyPolicy) {
        self.latency = policy;
    }

    /// The monitor's canvas generation and newest applied fence.
    #[must_use]
    pub fn position(&self, monitor_id: u16) -> Option<(u32, u64)> {
        let monitor = self.monitors.get(&monitor_id)?;
        monitor
            .state
            .scope()
            .map(|scope| (scope.generation.get(), monitor.applied_fence))
    }

    /// The monitor's canvas size, once a reset created it.
    #[must_use]
    pub fn size(&self, monitor_id: u16) -> Option<(u32, u32)> {
        let grid = self.monitors.get(&monitor_id)?.state.grid()?;
        Some((grid.width, grid.height))
    }

    /// Open surface decoders of a monitor.
    #[must_use]
    pub fn open_surfaces(&self, monitor_id: u16) -> usize {
        self.monitors.get(&monitor_id).map_or(0, |monitor| {
            monitor
                .surfaces
                .iter()
                .filter(|slot| slot.is_open())
                .count()
        })
    }

    /// Sets the decoded tile bytes after which a record goes to the sink in
    /// parts (tests use a small value).
    #[cfg(test)]
    pub(crate) const fn set_part_tile_bytes(&mut self, bytes: u64) {
        self.part_tile_bytes = bytes;
    }

    /// The monitors with a canvas.
    #[must_use]
    pub fn monitor_ids(&self) -> Vec<u16> {
        self.monitors.keys().copied().collect()
    }

    /// When [`Self::take_control_messages`] next owes the Pier something
    /// without a new record: a reset request for a lost canvas, or an ack
    /// for records applied but not acknowledged. `None` when nothing is owed.
    #[must_use]
    pub fn next_due(&self, now: Instant) -> Option<Instant> {
        if self.sink_refused || !self.is_on() {
            return None;
        }
        self.monitors
            .values()
            .filter_map(|monitor| {
                if monitor.lost.is_some() {
                    Some(
                        monitor
                            .last_reset_request
                            .map_or(now, |at| at + RESET_REQUEST_RETRY),
                    )
                } else if monitor.state.scope().is_some()
                    && (monitor.records_since_ack || monitor.presented_since_ack)
                {
                    Some(monitor.last_ack.map_or(now, |at| at + ACK_INTERVAL))
                } else {
                    None
                }
            })
            .min()
    }

    /// Whether the monitor's canvas is lost and waits for a reset.
    #[must_use]
    pub fn is_lost(&self, monitor_id: u16) -> bool {
        self.monitors
            .get(&monitor_id)
            .is_some_and(|monitor| monitor.lost.is_some())
    }

    /// The statistics.
    #[must_use]
    pub fn stats(&self) -> CanvasStats {
        let mut stats = self.stats.clone();
        stats.tile_pixels = self.previous.tiles + self.current.tiles;
        stats.surface_pixels = self.previous.surface + self.current.surface;
        stats.copy_pixels = self.previous.copy + self.current.copy;
        stats.open_surfaces = self
            .monitors
            .values()
            .map(|monitor| {
                monitor
                    .surfaces
                    .iter()
                    .filter(|slot| slot.is_open())
                    .count()
            })
            .sum();
        stats
    }

    /// The Deck dropped queued canvas records of `monitor_id` (its inbox
    /// overflowed): the canvas is lost until the next reset.
    pub fn records_lost(&mut self, monitor_id: u16) {
        if let Some(monitor) = self.monitors.get_mut(&monitor_id) {
            monitor.lose("queue_overflow");
        } else if let Some(terms) = self.terms {
            let mut monitor = MonitorCanvas::new(monitor_id, terms);
            monitor.lose("queue_overflow");
            self.monitors.insert(monitor_id, monitor);
        }
    }

    fn roll_window(&mut self, now: Instant) {
        let started = *self.window_started.get_or_insert(now);
        if now.saturating_duration_since(started) >= STATS_WINDOW {
            self.previous = self.current;
            self.current = PixelShares::default();
            self.window_started = Some(now);
        }
    }

    /// Admits, validates, decodes and executes one canvas record.
    #[allow(
        clippy::too_many_lines,
        reason = "one record's whole path, in order, reads best in one place"
    )]
    pub fn apply<P>(&mut self, bytes: &[u8], env: &mut CanvasEnv<'_, D, P>) -> CanvasApplied
    where
        P: VideoPresenterSink<NativeFrame = D::NativeFrame>,
    {
        self.roll_window(env.now);
        let (Some(terms), Some(status)) = (self.terms, self.status.as_ref()) else {
            self.stats.dropped += 1;
            return CanvasApplied::Dropped;
        };
        if self.sink_refused {
            self.stats.dropped += 1;
            return CanvasApplied::SinkRefused;
        }
        let record = match CanvasRecord::decode(bytes) {
            Ok(record) => record,
            Err(error) => {
                // A record that does not decode belongs to no generation;
                // the canvas it names (if it is on) cannot stay exact.
                self.stats.dropped += 1;
                tracing::warn!(
                    target: crate::log_target::VIDEO,
                    error = error.token(),
                    "malformed canvas record",
                );
                if let Some(monitor_id) = record_monitor(bytes).filter(|id| status.is_on_for(*id)) {
                    self.monitors
                        .entry(monitor_id)
                        .or_insert_with(|| MonitorCanvas::new(monitor_id, terms))
                        .lose("malformed");
                    return CanvasApplied::Lost("malformed");
                }
                return CanvasApplied::Dropped;
            }
        };
        let scope = *record.scope();
        if let Err(refusal) = admit(&scope, status, self.roster.as_ref()) {
            self.stats.dropped += 1;
            if refusal == CanvasAdmissionError::RosterPending {
                // The record cannot wait for the roster: its canvas is lost
                // and asks for a reset, which a committed roster admits.
                self.monitors
                    .entry(scope.monitor_id)
                    .or_insert_with(|| MonitorCanvas::new(scope.monitor_id, terms))
                    .lose("roster_pending");
                return CanvasApplied::Lost("roster_pending");
            }
            tracing::debug!(
                target: crate::log_target::VIDEO,
                monitor_id = scope.monitor_id,
                ?refusal,
                "canvas record not admitted",
            );
            return CanvasApplied::Dropped;
        }
        let monitor = self
            .monitors
            .entry(scope.monitor_id)
            .or_insert_with(|| MonitorCanvas::new(scope.monitor_id, terms));
        monitor.records_since_ack = true;
        let is_reset = matches!(record, CanvasRecord::Reset(_));
        if monitor.lost.is_some() && !is_reset {
            self.stats.dropped += 1;
            return CanvasApplied::Dropped;
        }
        if env.sink.is_none() {
            monitor.lose("no_presenter");
            return CanvasApplied::Lost("no_presenter");
        }
        let work = match monitor.state.apply(&record) {
            Ok(work) => work,
            Err(refusal) => {
                self.stats.refusals += 1;
                let reason = refusal.token();
                tracing::warn!(
                    target: crate::log_target::VIDEO,
                    monitor_id = scope.monitor_id,
                    reason,
                    "canvas record refused; asking the host for a reset",
                );
                monitor.lose(reason);
                return CanvasApplied::Lost(reason);
            }
        };
        // Read back only at the fences the Pier logs its mirror's digest at
        // (`CANVAS_DIGEST_EVERY_FENCES`), so a lab compares the two.
        let readback = !is_reset
            && work.fence % arcen_media::canvas::CANVAS_DIGEST_EVERY_FENCES == 0
            && monitor
                .last_digest_request
                .is_none_or(|at| env.now.saturating_duration_since(at) >= DIGEST_INTERVAL);
        let Some(sink) = env.sink.as_deref_mut() else {
            monitor.lose("no_presenter");
            return CanvasApplied::Lost("no_presenter");
        };
        let mut execution = Execution {
            now: env.now,
            decoders: env.decoders,
            tiles: &mut *env.tiles,
            sink,
            timing: env.timing,
            color: self.session_color,
            latency: self.latency,
            readback,
            part_tile_bytes: self.part_tile_bytes,
        };
        let outcome = match execute_record(monitor, &work, &mut execution) {
            Ok(outcome) => outcome,
            Err(reason) => {
                tracing::warn!(
                    target: crate::log_target::VIDEO,
                    monitor_id = scope.monitor_id,
                    reason,
                    "canvas record did not decode; asking the host for a reset",
                );
                monitor.lose(reason);
                return CanvasApplied::Lost(reason);
            }
        };
        match outcome {
            CanvasOutcome::Presented | CanvasOutcome::Applied => {
                if is_reset {
                    monitor.lost = None;
                    monitor.last_reset_request = None;
                    monitor.pending_digest = None;
                    self.stats.resets += 1;
                }
                monitor.applied_fence = work.fence;
                if readback {
                    monitor.last_digest_request = Some(env.now);
                    monitor.pending_digest =
                        Some((work.generation.get(), work.fence, monitor.state.clone()));
                }
                self.stats.records_applied += 1;
                let shares = shares_of(&work);
                self.current.tiles += shares.tiles;
                self.current.surface += shares.surface;
                self.current.copy += shares.copy;
                if outcome == CanvasOutcome::Presented {
                    monitor.presented_since_ack = true;
                    self.stats.records_presented += 1;
                    CanvasApplied::Presented
                } else {
                    CanvasApplied::Applied
                }
            }
            CanvasOutcome::Unsupported => {
                // A Deck whose sink has no executor never advertises; this
                // is the wrong sink. Never ask for resets it cannot apply.
                monitor.lose("no_executor");
                self.sink_refused = true;
                self.stats.sink_refused = Some("no canvas executor".to_owned());
                tracing::error!(
                    target: crate::log_target::VIDEO,
                    monitor_id = scope.monitor_id,
                    "canvas work reached a presenter without a canvas executor; \
                     canvas records are ignored for this connection",
                );
                CanvasApplied::SinkRefused
            }
            CanvasOutcome::Failed(failure) => {
                tracing::warn!(
                    target: crate::log_target::VIDEO,
                    monitor_id = scope.monitor_id,
                    stage = ?failure.stage,
                    detail = failure.detail,
                    "canvas executor failed; asking the host for a reset",
                );
                monitor.lose("executor_failed");
                CanvasApplied::Lost("executor_failed")
            }
        }
    }

    /// Takes a canvas the sink read back and computes its digest against the
    /// state at that fence. Returns the digest when it matched a request.
    pub fn on_readback(&mut self, readback: &CanvasReadback) -> Option<CanvasDigest> {
        let monitor = self.monitors.get_mut(&readback.monitor_id)?;
        let (generation, fence, state) = monitor.pending_digest.take()?;
        if generation != readback.generation.get() || fence != readback.fence {
            return None;
        }
        let digest = canvas_digest(&state, &readback.bgra, readback.stride)?;
        tracing::info!(
            target: crate::log_target::VIDEO,
            monitor_id = readback.monitor_id,
            generation,
            fence,
            digest = %hex(&digest.digest),
            covered_blocks = digest.covered,
            "canvas digest",
        );
        self.stats.last_digest = Some(CanvasDigestReport {
            monitor_id: readback.monitor_id,
            generation,
            digest,
        });
        Some(digest)
    }

    /// The control messages due after a batch: a `canvas_ack` for every
    /// monitor that presented a frame, or that received records and was not
    /// acknowledged for [`ACK_INTERVAL`]; a `canvas_reset_request` for every
    /// lost canvas whose presenter exists (`has_sink`), at most every
    /// [`RESET_REQUEST_RETRY`]. `unprocessed` is the canvas bytes the Deck
    /// holds for a monitor and has not applied.
    pub fn take_control_messages(
        &mut self,
        now: Instant,
        has_sink: impl Fn(u16) -> bool,
        unprocessed: impl Fn(u16) -> u64,
    ) -> Vec<CanvasControlMsg> {
        let mut messages = Vec::new();
        if self.sink_refused || !self.is_on() {
            return messages;
        }
        let mut worst_p95 = 0;
        for (monitor_id, monitor) in &mut self.monitors {
            let p95 = monitor.decode_p95(now);
            worst_p95 = worst_p95.max(p95);
            if let Some(reason) = monitor.lost {
                let due = monitor
                    .last_reset_request
                    .is_none_or(|at| now.saturating_duration_since(at) >= RESET_REQUEST_RETRY);
                if due && has_sink(*monitor_id) {
                    monitor.last_reset_request = Some(now);
                    self.stats.reset_requests += 1;
                    tracing::info!(
                        target: crate::log_target::VIDEO,
                        monitor_id = *monitor_id,
                        generation = monitor.generation(),
                        reason,
                        "canvas reset requested",
                    );
                    messages.push(CanvasControlMsg::CanvasResetRequest {
                        monitor_id: *monitor_id,
                        generation: monitor.generation(),
                        reason: reason.to_owned(),
                    });
                }
                continue;
            }
            if monitor.state.scope().is_none() {
                continue;
            }
            let overdue = monitor.records_since_ack
                && monitor
                    .last_ack
                    .is_none_or(|at| now.saturating_duration_since(at) >= ACK_INTERVAL);
            if monitor.presented_since_ack || overdue {
                monitor.presented_since_ack = false;
                monitor.records_since_ack = false;
                monitor.last_ack = Some(now);
                messages.push(CanvasControlMsg::CanvasAck {
                    monitor_id: *monitor_id,
                    generation: monitor.generation(),
                    fence: monitor.applied_fence,
                    unprocessed_bytes: unprocessed(*monitor_id),
                    decode_ms_p95: p95,
                });
            }
        }
        self.stats.decode_ms_p95 = worst_p95;
        messages
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

fn area(rect: TileRect) -> u64 {
    u64::from(rect.width) * u64::from(rect.height)
}

fn shares_of(work: &CanvasWork<'_>) -> PixelShares {
    let mut shares = PixelShares::default();
    for op in &work.ops {
        match op {
            CanvasOp::Fill { rect, .. }
            | CanvasOp::Tile { rect, .. }
            | CanvasOp::CachePaint { rect, .. } => shares.tiles += area(*rect),
            CanvasOp::Copy { src, .. } => shares.copy += area(*src),
            CanvasOp::SurfacePaint { blocks, .. } => {
                shares.surface += blocks.iter().map(|span| area(*span)).sum::<u64>();
            }
            CanvasOp::Reset { .. }
            | CanvasOp::CacheStore { .. }
            | CanvasOp::CacheEvict { .. }
            | CanvasOp::SurfaceOpen { .. }
            | CanvasOp::SurfaceClose { .. } => {}
        }
    }
    shares
}

/// The header a surface access unit is decoded under: H.264 Constrained
/// Baseline 4:2:0, eight bits, limited-range BT.709 (the Software
/// contract's stream), on the record's monitor and wire clock.
fn surface_header(monitor_id: u16, timestamp_ms: u32, idr: bool) -> VideoHeader {
    VideoHeader {
        frame_type: FrameType::VideoH264,
        codec: VideoCodec::H264,
        chroma: ChromaSubsampling::Yuv420,
        flags: VideoHeader::encode_flags(
            idr,
            BitDepth::Eight,
            ColorRange::Limited,
            ColorMatrix::Bt709,
        ),
        timestamp_ms,
        monitor_id,
        topology_generation: 0,
        stream_epoch: 0,
    }
}

/// A surface slot: closed, opened (its decoder is created at its first
/// paint, so opening costs nothing), or decoding.
enum SurfaceSlot<D> {
    Closed,
    Opened,
    Decoding(D),
}

impl<D> SurfaceSlot<D> {
    const fn is_open(&self) -> bool {
        !matches!(self, Self::Closed)
    }
}

/// Decodes what one operation needs, keeping the surface decoders'
/// lifecycle with the state's (a reset or close drops a decoder, an open
/// discards the old one, the first paint after it creates a fresh one).
#[allow(
    clippy::too_many_arguments,
    reason = "the operation, the monitor's surfaces and the decoders"
)]
fn prepare_input<D>(
    monitor: &mut MonitorCanvas<D>,
    work: &CanvasWork<'_>,
    op: &CanvasOp<'_>,
    now: Instant,
    decoders: &dyn Fn() -> D,
    tiles: &mut dyn TileDecoder,
    color: SessionColor,
    latency: DecodeLatencyPolicy,
) -> Result<CanvasOpInput<D::NativeFrame>, &'static str>
where
    D: VideoDecoderBackend,
{
    Ok(match op {
        CanvasOp::Reset { .. } => {
            for surface in &mut monitor.surfaces {
                *surface = SurfaceSlot::Closed;
            }
            CanvasOpInput::None
        }
        CanvasOp::Tile { rect, payload } => CanvasOpInput::Tile(
            tiles
                .decode(payload, rect.width, rect.height)
                .ok()
                .filter(|codes| codes.width() == rect.width && codes.height() == rect.height)
                .ok_or("tile_decode")?,
        ),
        CanvasOp::SurfaceOpen { surface, .. } => {
            *monitor
                .surfaces
                .get_mut(usize::from(*surface))
                .ok_or("surface_limit")? = SurfaceSlot::Opened;
            CanvasOpInput::None
        }
        CanvasOp::SurfaceClose { surface } => {
            if let Some(slot) = monitor.surfaces.get_mut(usize::from(*surface)) {
                *slot = SurfaceSlot::Closed;
            }
            CanvasOpInput::None
        }
        CanvasOp::SurfacePaint {
            surface,
            idr,
            access_unit,
            ..
        } => {
            let slot = monitor
                .surfaces
                .get_mut(usize::from(*surface))
                .ok_or("surface_closed")?;
            if matches!(slot, SurfaceSlot::Opened) {
                let mut decoder = decoders();
                decoder.set_session_color(color);
                decoder.set_decode_latency_policy(latency);
                let _ = decoder.set_prefer_native_frames(true);
                *slot = SurfaceSlot::Decoding(decoder);
            }
            let SurfaceSlot::Decoding(decoder) = slot else {
                return Err("surface_closed");
            };
            let header = surface_header(work.monitor_id, work.timestamp_ms, *idr);
            let started = Instant::now();
            let picture = decoder
                .decode(&header, access_unit)
                .map_err(|_| "surface_decode")?;
            let elapsed = started.elapsed().as_secs_f64() * 1_000.0;
            monitor.decode_ms.push_back((now, elapsed));
            CanvasOpInput::Surface(match picture {
                None => None,
                Some(frame) => Some(native(frame.native)?),
            })
        }
        CanvasOp::Fill { .. }
        | CanvasOp::Copy { .. }
        | CanvasOp::CacheStore { .. }
        | CanvasOp::CachePaint { .. }
        | CanvasOp::CacheEvict { .. } => CanvasOpInput::None,
    })
}

/// What one record's execution needs besides the monitor and the work.
struct Execution<'x, D: VideoDecoderBackend, P> {
    now: Instant,
    decoders: &'x dyn Fn() -> D,
    tiles: &'x mut dyn TileDecoder,
    sink: &'x mut P,
    timing: PresentTiming,
    color: SessionColor,
    latency: DecodeLatencyPolicy,
    readback: bool,
    part_tile_bytes: u64,
}

/// Decodes the record's inputs operation by operation and hands them to the
/// sink. A record whose decoded tiles exceed `part_tile_bytes` goes to the
/// sink in parts, each applied without presenting; the last part presents
/// (the record is applied whole before the canvas is shown). The Deck so
/// never holds more than one part's decoded tiles, whatever the record's
/// payloads expand to.
fn execute_record<D, P>(
    monitor: &mut MonitorCanvas<D>,
    work: &CanvasWork<'_>,
    x: &mut Execution<'_, D, P>,
) -> Result<CanvasOutcome, &'static str>
where
    D: VideoDecoderBackend,
    P: VideoPresenterSink<NativeFrame = D::NativeFrame>,
{
    let part = |ops| CanvasWork {
        monitor_id: work.monitor_id,
        generation: work.generation,
        fence: work.fence,
        timestamp_ms: work.timestamp_ms,
        ops,
    };
    let mut ops = Vec::new();
    let mut inputs = Vec::new();
    let mut tile_bytes = 0_u64;
    let count = work.ops.len();
    for (index, op) in work.ops.iter().enumerate() {
        let input = prepare_input(
            monitor, work, op, x.now, x.decoders, x.tiles, x.color, x.latency,
        )?;
        if let CanvasOpInput::Tile(codes) = &input {
            tile_bytes +=
                u64::from(codes.width()) * u64::from(codes.height()) * TILE_CODE_BYTES_PER_PIXEL;
        }
        ops.push(op.clone());
        inputs.push(input);
        if tile_bytes >= x.part_tile_bytes && index + 1 < count {
            let outcome = x.sink.execute(&CanvasExecution {
                work: &part(std::mem::take(&mut ops)),
                inputs: &std::mem::take(&mut inputs),
                arrival: x.now,
                timing: x.timing,
                present: false,
                readback: false,
            });
            if !matches!(outcome, CanvasOutcome::Presented | CanvasOutcome::Applied) {
                return Ok(outcome);
            }
            tile_bytes = 0;
        }
    }
    Ok(x.sink.execute(&CanvasExecution {
        work: &part(ops),
        inputs: &inputs,
        arrival: x.now,
        timing: x.timing,
        present: true,
        readback: x.readback,
    }))
}

fn native<N>(
    picture: Option<NativeDecodedVideoFrame<N>>,
) -> Result<NativeDecodedVideoFrame<N>, &'static str> {
    picture.ok_or("surface_not_native")
}

#[cfg(test)]
mod tests;
