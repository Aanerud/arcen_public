//! QUIC multi-monitor carrier benchmark foundation — a diagnostic-only
//! comparison of two candidate carrier shapes described in
//! `docs/adr/0009-multi-monitor-foundation.md`'s performance gate:
//!
//! - **Carrier A**: all monitor frames multiplexed over one reliable
//!   unidirectional QUIC stream, interleaved by a bounded, weighted
//!   round-robin scheduler (not a naive full-drain of monitor 0).
//! - **Carrier B**: one reliable unidirectional QUIC stream per monitor,
//!   opened with the existing [`super::monitor`] preface foundation
//!   (`open_monitor_stream`/`accept_monitor_stream`) and the test-only
//!   `monitor_carrier_transport_config`.
//!
//! This module is a **measurement tool, not a product carrier**. It does not
//! select Carrier A or Carrier B as a default, does not change any live
//! transport config, and its localhost, single-process timings are
//! explicitly **not** a glass-to-glass measurement (see the non-claims below
//! and in `docs/architecture/transport.md`). Real hardware validation
//! (pier-linux.example.internal / an actual Deck client) is required before any production
//! carrier selection, per ADR 0009's performance gate.
//!
//! ## Non-claims
//!
//! - **Not glass-to-glass.** Latency here is measured from a shared
//!   in-process `Instant` epoch at frame construction to the moment the
//!   shared per-monitor consumer stage ([`carrier_receive_consume_one`])
//!   observes the decoded frame *after* applying any configured
//!   `receiver_delay` — real capture/encode/decode/present time is not
//!   included, and there is no cross-machine clock skew because both
//!   "ends" of the loopback run in one process.
//! - **Localhost/single-process only.** No real network path, congestion,
//!   or loss is exercised; Quinn's congestion controller reacts to whatever
//!   the local loopback interface reports, which is not representative of a
//!   WAN/LAN production path.
//! - **Carrier B streams share one connection's congestion controller.**
//!   Independent per-monitor streams are not independent bandwidth; QUIC's
//!   congestion control operates per-connection, not per-stream.
//! - **No recovery/reconnect modeling.** `recovery_failures` is always
//!   reported as `0`; this foundation does not implement or measure any
//!   reconnect path.
//! - **`receiver_delay` is a carrier-neutral, per-monitor, post-demux
//!   consumer/validation delay.** Both carriers apply it identically, on
//!   an independent, parallel path per monitor, via the exact same shared
//!   consumer function ([`carrier_receive_consume_one`]): Carrier A's
//!   single shared reader only demuxes frames by `monitor_id` into a
//!   bounded per-monitor channel and never itself sleeps (see
//!   [`carrier_a_receive_all`]); Carrier B's per-monitor reader only
//!   accepts the stream and parses its identity before likewise handing
//!   frames to the same shared consumer (see [`carrier_b_receive_one`]).
//!   Both carriers' receive pipelines are therefore structurally identical
//!   from immediately after transport framing onward, and every recorded
//!   latency metric reflects frame-send to post-consume observation —
//!   *after* the configured `receiver_delay`, not before it — for both
//!   carriers alike. The same configured `receiver_delay` therefore
//!   produces a comparable predicted completion floor *and* a comparable
//!   recorded latency for both carriers — bounded by the single busiest
//!   monitor's own frame count ([`max_per_monitor_offered_frame_count`]),
//!   never a sum across every monitor — instead of the fully-serial-for-A,
//!   fully-parallel-for-B asymmetry an earlier revision of this module
//!   had.
//! - **No unverified numeric thresholds.** This module records
//!   measurements; it does not encode or enforce any pass/fail throughput,
//!   latency, or fairness threshold. See ADR 0009.
//! - **`fairness_index` is normalized against offered load, not raw
//!   volume.** It is Jain's fairness index over each monitor's *delivery
//!   ratio* (delivered/sent bytes for that run), so a fully successful run
//!   reports it near `1.0` under `one-active-rest-idle` despite that
//!   pattern's intentionally uneven per-monitor byte volume. The raw,
//!   un-normalized volume spread is reported separately as
//!   `delivered_bytes_max_min_spread_ratio` and is expected to differ by
//!   pattern — it is not itself a fairness signal.
//! - **`Workload::Duration` is wall-clock paced; `Workload::Frames` is
//!   not.** A `Duration` workload's producers sleep against
//!   [`BENCH_TICK_INTERVAL`] so the run's production phase takes
//!   approximately the requested wall-clock duration, spanning the full
//!   requested interval end-to-end — including one final pacing wait after
//!   the last tick, so the run does not finish one whole tick interval
//!   short of the requested duration (see [`tick_pacing_for`]); a `Frames`
//!   workload remains fully unpaced and deterministic, bounded only by
//!   [`MAX_BENCH_TOTAL_BYTES`] and the run's own completion deadline.
//!   Neither carrier's *elapsed* metric is a throughput claim for a paced
//!   `Duration` run — it mostly reflects the requested duration itself,
//!   not delivery speed.
//! - **Every run is bounded by a predicted completion deadline, not just a
//!   config-time cap.** [`predicted_completion_deadline`] computes a
//!   generous but finite outer deadline (production/transfer budget, plus
//!   any `receiver_delay` floor, plus a small fixed drain allowance); if a
//!   run does not finish within it, `run_carrier_a`/`run_carrier_b` return
//!   [`BenchRunError::CompletionTimeout`] instead of hanging. This is a
//!   stall/backpressure safety net, not a latency or throughput claim, and
//!   [`BenchConfig::validate`] already rejects `receiver_delay`/workload
//!   combinations whose own arithmetic would guarantee an unreasonably
//!   long floor (see [`MAX_BENCH_RECEIVER_DELAY_FLOOR`]) before a run ever
//!   starts.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fmt::{Display, Formatter};
use std::future::Future;
use std::num::NonZeroU16;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use quinn::{Connection, RecvStream, SendStream, VarInt};
use tokio::sync::{Mutex, Notify, mpsc};
use tokio_util::task::AbortOnDropHandle;

use super::error::QuicTransportError;
use super::monitor::{
    MEDIA_PLAN_FINGERPRINT_BYTES, MonitorStreamIdentity, MonitorStreamPrefaceError,
    accept_monitor_stream, open_monitor_stream,
};

// ---------------------------------------------------------------------------
// Tunable constants — bounds for this diagnostic tool only. None of these
// values are a product performance requirement or a frozen baseline; see
// ADR 0009's "no unverified numeric thresholds" performance gate.
// ---------------------------------------------------------------------------

/// Only 2 or 4 monitors are accepted, matching the product's Match My
/// Layout release hardware targets (4x1080p60, 2x4K60) rather than the
/// full 1..=4 admission range other foundations accept.
pub const ALLOWED_MONITOR_COUNTS: [u8; 2] = [2, 4];

/// Minimum accepted explicit frame-count workload.
pub const MIN_BENCH_FRAMES: u64 = 1;
/// Maximum accepted explicit frame-count workload (memory/time safety cap).
pub const MAX_BENCH_FRAMES: u64 = 2_000_000;

/// Minimum accepted duration workload — fixed at [`BENCH_TICK_INTERVAL`]
/// itself (2ms): [`resolve_tick_count`] converts a duration to a tick count
/// via integer division by `BENCH_TICK_INTERVAL`, so anything shorter than
/// one full tick interval cannot be represented as even a single
/// deterministic tick, and durations that are not an exact multiple of
/// `BENCH_TICK_INTERVAL` are truncated down to the nearest whole tick
/// (e.g. a 5ms duration resolves to 2 ticks, not 2.5) — this module's
/// smallest representable resolution is `BENCH_TICK_INTERVAL` itself.
pub const MIN_BENCH_DURATION: Duration = BENCH_TICK_INTERVAL;
/// Maximum accepted duration workload (10 minutes; keeps a runaway CLI
/// invocation bounded).
pub const MAX_BENCH_DURATION: Duration = Duration::from_secs(600);

/// Minimum accepted payload size in bytes.
pub const MIN_BENCH_PAYLOAD_BYTES: usize = 1;
/// Maximum accepted payload size in bytes (8 MiB; generous headroom above a
/// single compressed video frame, while still bounding allocation).
pub const MAX_BENCH_PAYLOAD_BYTES: usize = 8 * 1024 * 1024;

/// Maximum accepted deterministic artificial receiver delay per frame.
pub const MAX_RECEIVER_DELAY: Duration = Duration::from_millis(500);

/// Safety cap on `monitors * frames * encoded_frame_bytes(payload_bytes)`
/// (see [`encoded_frame_bytes`] — the fixed [`BENCH_FRAME_HEADER_BYTES`]
/// envelope plus the payload, i.e. the actual bytes each frame puts on the
/// wire, not the payload alone) for an explicit frame-count workload,
/// independent of the individual per-field bounds above.
pub const MAX_BENCH_TOTAL_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// Bounded per-monitor queue capacity inside the Carrier A scheduler.
/// Producers apply backpressure (bounded retry, never a silent drop) when a
/// queue is at capacity, so this is a scheduling window, not a loss budget.
pub(crate) const BENCH_SCHEDULER_QUEUE_CAPACITY: usize = 8;

/// Bounded per-monitor consumer channel capacity inside Carrier A's demux
/// receiver (see [`carrier_a_receive_all`]) — the receive-side analog of
/// [`BENCH_SCHEDULER_QUEUE_CAPACITY`], and the queue depth through which
/// Carrier A applies `receiver_delay` independently per monitor, matching
/// Carrier B's independent per-monitor stream tasks. The single shared
/// demux reader applies ordinary bounded backpressure (never a silent
/// drop) if a monitor's own consumer falls behind.
pub(crate) const BENCH_RECEIVER_QUEUE_CAPACITY: usize = 8;

/// Scheduler weight given to the single "active" monitor in the
/// one-active-rest-idle pattern.
pub(crate) const ACTIVE_MONITOR_SCHEDULER_WEIGHT: u32 = 4;
/// Scheduler weight given to each "idle" monitor in the one-active-rest-idle
/// pattern.
pub(crate) const IDLE_MONITOR_SCHEDULER_WEIGHT: u32 = 1;
/// One idle monitor produces a frame every this-many scheduling ticks in the
/// one-active-rest-idle pattern; the active monitor produces on every tick.
pub(crate) const IDLE_DUTY_CYCLE_TICKS: u64 = 10;

/// Fixed tick interval used to convert a `Workload::Duration` into a
/// deterministic scheduling-tick count before a run starts, *and* to pace
/// that same run's actual production loop to real wall-clock time (see
/// [`tick_pacing_for`]): each producer sleeps until `epoch + tick *
/// BENCH_TICK_INTERVAL` before considering tick `tick`, so a `Duration`
/// workload's production phase takes approximately the requested duration
/// itself, not however long it happens to take to blast the resolved tick
/// count as fast as the connection allows. `Workload::Frames` is never
/// paced by this constant — it remains deterministic and unpaced, bounded
/// only by [`MAX_BENCH_TOTAL_BYTES`] and this run's own completion deadline
/// (see [`predicted_completion_deadline`]).
pub const BENCH_TICK_INTERVAL: Duration = Duration::from_millis(2);

/// Small, fixed allowance added atop this run's predicted completion floor
/// when computing its outer completion deadline (see
/// [`predicted_completion_deadline`]): headroom for stream `finish()`,
/// task-join, and ordinary OS scheduling jitter right at the end of a run.
/// This is not a throughput or latency claim, only deadline sizing.
pub const BENCH_COMPLETION_DRAIN_ALLOWANCE: Duration = Duration::from_secs(5);

/// Deliberately pessimistic assumed minimum transfer throughput (bytes per
/// second), used only to size this run's own outer completion deadline
/// (see [`predicted_completion_deadline`]) generously enough that
/// legitimate localhost data-transfer time — already bounded by
/// [`MAX_BENCH_TOTAL_BYTES`] — can never trip that deadline on its own.
/// This is not a throughput floor or guarantee this module measures or
/// claims anywhere else; it exists purely so the deadline is a safety net
/// against runaway `receiver_delay`/backpressure stalls, not against
/// ordinary transfer time.
pub const MIN_ASSUMED_TRANSFER_BYTES_PER_SEC: u64 = 8 * 1024 * 1024;

/// Sane cap on the predicted receiver-delay-driven completion floor
/// (`max_per_monitor_offered_frame_count(config) * receiver_delay` — the
/// same carrier-neutral, per-monitor bound
/// [`predicted_completion_deadline`] itself uses, since `receiver_delay` is
/// applied on an independent, parallel path per monitor by both carriers):
/// a `receiver_delay` config that would already, by simple arithmetic, force
/// a run's completion floor past this bound is rejected up front by
/// [`BenchConfig::validate`], rather than left to actually run for
/// potentially hours or days before anyone notices.
pub const MAX_BENCH_RECEIVER_DELAY_FLOOR: Duration = Duration::from_secs(60);

/// Overall deadline for accepting and parsing one Carrier B monitor stream
/// preface.
pub(crate) const MONITOR_STREAM_ACCEPT_TIMEOUT: Duration = Duration::from_secs(10);

/// Application-level QUIC error code `run_carrier_a`/`run_carrier_b` signal
/// on their own ephemeral loopback stream(s)/connections when explicitly
/// resetting/closing them on a failed or timed-out run (see this module's
/// "Structured task ownership" section). Not a wire-compatibility concern —
/// this module's connections are always ephemeral, tool-owned loopback
/// connections built fresh per run (see the module's own doc comment),
/// never a shared or reused product connection with its own close-code
/// contract, so any single fixed value is sufficient here.
const BENCH_RUN_FAILURE_ERROR_CODE: u32 = 1;

/// Fixed magic identifying one benchmark frame envelope.
pub(crate) const BENCH_FRAME_MAGIC: [u8; 4] = *b"ABFR";
/// Current benchmark frame envelope wire version.
pub(crate) const BENCH_FRAME_VERSION: u16 = 1;

// magic(4) + version(2) + monitor_id(2) + sequence(8) + send_nanos(8) + payload_len(4)
const BENCH_FRAME_HEADER_BYTES: usize = 4 + 2 + 2 + 8 + 8 + 4;

/// Computes the exact number of bytes one encoded benchmark frame puts on
/// the wire: the fixed [`BENCH_FRAME_HEADER_BYTES`] envelope plus
/// `payload_bytes`. Every byte-based safety cap or budget in this module —
/// the total-bytes safety cap ([`MAX_BENCH_TOTAL_BYTES`],
/// [`BenchConfigError::TotalBytesExceedsCap`]) and the completion
/// deadline's data-transfer budget ([`predicted_completion_deadline`]) —
/// must use this figure, not `payload_bytes` alone: `payload_bytes` alone
/// under-counts the true wire bytes both carriers actually transfer per
/// frame by `BENCH_FRAME_HEADER_BYTES`, and at large offered frame counts
/// (this module allows up to [`MAX_BENCH_FRAMES`] per monitor) that gap is
/// not negligible — most visibly with a small configured `payload_bytes`,
/// where the fixed header can be a large fraction, or even a multiple, of
/// the payload itself.
///
/// This is deliberately distinct from the per-monitor/aggregate
/// `sent_bytes`/`delivered_bytes` metrics this module reports, which
/// intentionally measure only application **payload** bytes (see
/// [`PerMonitorValidator::record`] and [`BenchFrame`]'s own doc comment)
/// — a meaningful, and differently named, figure for comparing carriers'
/// actual data throughput, not this function's wire-safety-math role.
///
/// Uses saturating addition (rather than the unchecked/wrapping arithmetic
/// used for values already known to be in-range elsewhere in this module):
/// `payload_bytes` is a `usize` that, in principle, callers other than
/// [`BenchConfig::validate`] could pass unvalidated, so overflow here
/// safely saturates to `u64::MAX` instead of panicking or silently
/// wrapping to a small, wrong value that would defeat the very safety caps
/// this function feeds.
#[must_use]
pub fn encoded_frame_bytes(payload_bytes: usize) -> u64 {
    u64::try_from(payload_bytes)
        .unwrap_or(u64::MAX)
        .saturating_add(BENCH_FRAME_HEADER_BYTES as u64)
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Which carrier(s) a run should exercise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CarrierKind {
    /// All monitor frames multiplexed over one reliable stream.
    A,
    /// One reliable stream per monitor.
    B,
}

impl Display for CarrierKind {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::A => "carrier_a",
            Self::B => "carrier_b",
        })
    }
}

/// Which carrier(s) to run for one invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CarrierSelection {
    /// Run both carriers, each on its own fresh connection, for comparison.
    Both,
    /// Run only one named carrier.
    Only(CarrierKind),
}

/// Synthetic per-monitor activity shape for one run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivePattern {
    /// Every monitor produces a frame every scheduling tick.
    AllActive,
    /// Exactly one monitor (the first) produces every tick; the rest
    /// produce only every [`IDLE_DUTY_CYCLE_TICKS`]th tick.
    OneActiveRestIdle,
}

impl Display for ActivePattern {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::AllActive => "all-active",
            Self::OneActiveRestIdle => "one-active-rest-idle",
        })
    }
}

impl FromStr for ActivePattern {
    type Err = CliArgError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "all-active" => Ok(Self::AllActive),
            "one-active-rest-idle" => Ok(Self::OneActiveRestIdle),
            _ => Err(CliArgError::InvalidValue {
                arg: "--pattern",
                value: value.to_owned(),
            }),
        }
    }
}

/// How long one carrier run should keep producing frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Workload {
    /// An exact, deterministic number of scheduling ticks.
    Frames(u64),
    /// A wall-clock soft cap, deterministically converted to a tick count
    /// via [`BENCH_TICK_INTERVAL`] before the run starts.
    Duration(Duration),
}

/// Validated configuration for one Carrier A vs Carrier B comparison run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BenchConfig {
    /// Monitor count; must be exactly 2 or 4.
    pub monitors: u8,
    /// Frame-count or duration workload.
    pub workload: Workload,
    /// Per-frame application payload size in bytes.
    pub payload_bytes: usize,
    /// Synthetic per-monitor activity pattern.
    pub pattern: ActivePattern,
    /// Deterministic artificial per-monitor, post-demux consumer/validation
    /// delay applied *before* each frame is recorded/timestamped
    /// (`Duration::ZERO` disables it). This is carrier-neutral: both
    /// carriers hand frames to the exact same shared consumer function
    /// ([`carrier_receive_consume_one`]) on an independent, parallel path
    /// per monitor (Carrier A via [`carrier_a_receive_all`]'s demux
    /// reader; Carrier B via [`carrier_b_receive_one`]'s per-monitor
    /// reader), so the same value produces both a comparable predicted
    /// completion floor *and* a comparable recorded latency for both
    /// carriers, rather than a serial cost for one and a parallel cost for
    /// the other, or a delay whose cost is invisible in recorded latency.
    pub receiver_delay: Duration,
    /// Which carrier(s) to run.
    pub carriers: CarrierSelection,
}

/// Fail-closed [`BenchConfig`] validation rejection reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BenchConfigError {
    /// `monitors` was not exactly 2 or 4.
    InvalidMonitorCount(u8),
    /// The explicit frame count was outside `MIN_BENCH_FRAMES..=MAX_BENCH_FRAMES`.
    FrameCountOutOfRange(u64),
    /// The duration workload was outside `MIN_BENCH_DURATION..=MAX_BENCH_DURATION`.
    DurationOutOfRange(Duration),
    /// `payload_bytes` was outside `MIN_BENCH_PAYLOAD_BYTES..=MAX_BENCH_PAYLOAD_BYTES`.
    PayloadBytesOutOfRange(usize),
    /// `receiver_delay` exceeded [`MAX_RECEIVER_DELAY`].
    ReceiverDelayOutOfRange(Duration),
    /// `offered_frame_count(config) * encoded_frame_bytes(payload_bytes)`
    /// exceeded [`MAX_BENCH_TOTAL_BYTES`] — the exact, pattern-expanded
    /// offered frame count (see [`offered_frame_count`]) times the actual
    /// per-frame *wire* size (the fixed header plus `payload_bytes`, see
    /// [`encoded_frame_bytes`]), not `payload_bytes` alone.
    TotalBytesExceedsCap(u64),
    /// The predicted receiver-delay-driven completion floor
    /// (`max_per_monitor_offered_frame_count(config) * receiver_delay`, the
    /// same carrier-neutral, per-monitor bound
    /// [`predicted_completion_deadline`] itself uses) exceeded
    /// [`MAX_BENCH_RECEIVER_DELAY_FLOOR`] — this config's own arithmetic
    /// already guarantees a run could not complete in a sane amount of
    /// time.
    ReceiverDelayFloorExceedsCap(Duration),
}

impl Display for BenchConfigError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidMonitorCount(value) => {
                write!(formatter, "monitors must be 2 or 4, got {value}")
            }
            Self::FrameCountOutOfRange(value) => write!(
                formatter,
                "frame count {value} out of range {MIN_BENCH_FRAMES}..={MAX_BENCH_FRAMES}"
            ),
            Self::DurationOutOfRange(value) => write!(
                formatter,
                "duration {value:?} out of range {MIN_BENCH_DURATION:?}..={MAX_BENCH_DURATION:?}"
            ),
            Self::PayloadBytesOutOfRange(value) => write!(
                formatter,
                "payload bytes {value} out of range {MIN_BENCH_PAYLOAD_BYTES}..={MAX_BENCH_PAYLOAD_BYTES}"
            ),
            Self::ReceiverDelayOutOfRange(value) => write!(
                formatter,
                "receiver delay {value:?} exceeds maximum {MAX_RECEIVER_DELAY:?}"
            ),
            Self::TotalBytesExceedsCap(value) => write!(
                formatter,
                "total bytes {value} exceeds safety cap {MAX_BENCH_TOTAL_BYTES}"
            ),
            Self::ReceiverDelayFloorExceedsCap(value) => write!(
                formatter,
                "predicted receiver-delay completion floor {value:?} exceeds safety cap \
                 {MAX_BENCH_RECEIVER_DELAY_FLOOR:?}"
            ),
        }
    }
}

impl std::error::Error for BenchConfigError {}

impl BenchConfig {
    /// Validates every field against this module's fixed bounds.
    ///
    /// # Errors
    ///
    /// Returns the first out-of-range field found, checked in field order.
    /// The total-bytes safety cap and the receiver-delay completion-floor
    /// cap are both checked last, against the *exact, pattern-expanded
    /// offered load* computed by [`offered_frame_count`] — for a
    /// `Workload::Duration`, that already accounts for the
    /// [`resolve_tick_count`]-resolved frame count (the same deterministic
    /// conversion the run itself uses), and for
    /// [`ActivePattern::OneActiveRestIdle`] it counts only the frames each
    /// monitor will actually produce (the active monitor every tick, idle
    /// monitors every [`IDLE_DUTY_CYCLE_TICKS`]th tick) rather than
    /// conservatively assuming every monitor is fully active. So a
    /// duration long enough to expand past [`MAX_BENCH_TOTAL_BYTES`], or a
    /// `receiver_delay` whose serialized floor across that many frames
    /// would exceed [`MAX_BENCH_RECEIVER_DELAY_FLOOR`], is rejected here
    /// rather than accepted and only failing (or silently running for
    /// hours/days, or to a multi-TiB offered load) once the run itself
    /// starts — and a `one-active-rest-idle` configuration whose true
    /// offered load is well within both caps is not wrongly rejected by an
    /// over-counted approximation of that load.
    pub fn validate(&self) -> Result<(), BenchConfigError> {
        if !ALLOWED_MONITOR_COUNTS.contains(&self.monitors) {
            return Err(BenchConfigError::InvalidMonitorCount(self.monitors));
        }
        match self.workload {
            Workload::Frames(frames) => {
                if !(MIN_BENCH_FRAMES..=MAX_BENCH_FRAMES).contains(&frames) {
                    return Err(BenchConfigError::FrameCountOutOfRange(frames));
                }
            }
            Workload::Duration(duration) => {
                if duration < MIN_BENCH_DURATION || duration > MAX_BENCH_DURATION {
                    return Err(BenchConfigError::DurationOutOfRange(duration));
                }
            }
        }
        if !(MIN_BENCH_PAYLOAD_BYTES..=MAX_BENCH_PAYLOAD_BYTES).contains(&self.payload_bytes) {
            return Err(BenchConfigError::PayloadBytesOutOfRange(self.payload_bytes));
        }
        if self.receiver_delay > MAX_RECEIVER_DELAY {
            return Err(BenchConfigError::ReceiverDelayOutOfRange(
                self.receiver_delay,
            ));
        }
        // Compute the exact, pattern-expanded offered frame count before
        // checking either bound below — see `offered_frame_count`'s own
        // documentation for why this must be pattern-aware rather than a
        // simple `monitors * effective_frames` approximation. A `Duration`
        // workload is otherwise unbounded in offered load: at
        // `MAX_BENCH_DURATION` (600s) and `BENCH_TICK_INTERVAL` (2ms), the
        // resolved tick count alone is 300,000 — multiplied by 4 monitors
        // (all-active) and the 8 MiB payload ceiling, that is over 9 TiB of
        // offered load, which must fail validation up front rather than
        // only manifesting as a runaway run.
        let offered_frames = offered_frame_count(self);
        // Use the *encoded* (wire) frame size, not `payload_bytes` alone —
        // see `encoded_frame_bytes`'s own doc comment for why the fixed
        // header must be counted here: a config with a small
        // `payload_bytes` and a very large offered frame count could
        // otherwise pass this cap on payload bytes alone while its true
        // wire byte total (what actually crosses the cap's own safety
        // rationale) exceeds it.
        let total = offered_frames.saturating_mul(encoded_frame_bytes(self.payload_bytes));
        if total > MAX_BENCH_TOTAL_BYTES {
            return Err(BenchConfigError::TotalBytesExceedsCap(total));
        }
        // A nonzero `receiver_delay` is a per-monitor, post-demux
        // consumer/validation delay applied on independent, parallel paths
        // by both carriers (see `carrier_a_receive_all`'s per-monitor
        // demux consumer and `carrier_b_receive_one`'s per-monitor stream
        // task) — so the true worst-case serialized completion floor is
        // bounded by the single busiest monitor's own frame count
        // (`max_per_monitor_offered_frame_count`), not the sum across every
        // monitor: with every monitor's consumer running concurrently, the
        // run is only as slow as its slowest (busiest) monitor, never the
        // total of every monitor's delay added together. Even so, that
        // floor alone can reach hours or days for a large enough
        // configuration (e.g. `MAX_BENCH_DURATION` at `BENCH_TICK_INTERVAL`
        // resolves to 300,000 ticks for the busiest monitor; at
        // `receiver_delay`'s own 500ms maximum that is over 41 hours).
        // Reject that combination up front rather than starting a run
        // nobody can wait out.
        let max_per_monitor_frames = max_per_monitor_offered_frame_count(self);
        let max_per_monitor_frames_u32 = u32::try_from(max_per_monitor_frames).unwrap_or(u32::MAX);
        let receiver_delay_floor = self
            .receiver_delay
            .saturating_mul(max_per_monitor_frames_u32);
        if receiver_delay_floor > MAX_BENCH_RECEIVER_DELAY_FLOOR {
            return Err(BenchConfigError::ReceiverDelayFloorExceedsCap(
                receiver_delay_floor,
            ));
        }
        Ok(())
    }
}

/// Deterministically resolves a [`Workload`] into a scheduling-tick count.
#[must_use]
pub fn resolve_tick_count(workload: Workload) -> u64 {
    match workload {
        Workload::Frames(frames) => frames,
        Workload::Duration(duration) => {
            let ticks = duration.as_nanos() / BENCH_TICK_INTERVAL.as_nanos().max(1);
            u64::try_from(ticks).unwrap_or(u64::MAX).max(1)
        }
    }
}

/// Computes the exact, pattern-expanded total frame count this
/// configuration's run will actually produce across every monitor — the
/// single authoritative "offered load" figure. This is the one function
/// [`BenchConfig::validate`] (total-bytes cap, receiver-delay completion
/// floor) and [`predicted_completion_deadline`] (transfer budget,
/// receiver-delay floor) both use, so every cap and the deadline are all
/// checked against the same real load a run will actually produce, not an
/// approximation of it.
///
/// This mirrors [`produces_at_tick`]'s own per-monitor duty cycle exactly,
/// rather than conservatively assuming every monitor is fully active: for
/// [`ActivePattern::AllActive`] that is `monitors * effective_frames`, but
/// for [`ActivePattern::OneActiveRestIdle`] only the first monitor produces
/// every tick — the remaining `monitors - 1` monitors each produce only
/// once every [`IDLE_DUTY_CYCLE_TICKS`]th tick, matching
/// `produces_at_tick`'s own `tick % IDLE_DUTY_CYCLE_TICKS == 0` duty cycle
/// exactly. Treating `one-active-rest-idle` as if every monitor were fully
/// active (a previous, simpler bound) over-counts the true offered load by
/// roughly `IDLE_DUTY_CYCLE_TICKS`-fold for the idle monitors, and can
/// wrongly reject an otherwise perfectly acceptable configuration — or
/// wrongly accept one whose `all-active` load would in fact exceed a cap,
/// since over- and under-counting are both a correctness bug, not a safe
/// simplification, once caps are meant to reflect the actual offered load.
#[must_use]
pub fn offered_frame_count(config: &BenchConfig) -> u64 {
    let effective_frames = resolve_tick_count(config.workload);
    let monitors = u64::from(config.monitors);
    match config.pattern {
        ActivePattern::AllActive => monitors.saturating_mul(effective_frames),
        ActivePattern::OneActiveRestIdle => {
            // Monitor 0 (0-based) is the single always-active monitor; the
            // remaining `monitors - 1` monitors each produce once every
            // `IDLE_DUTY_CYCLE_TICKS`th tick (ticks `0, 10, 20, ...` within
            // `0..effective_frames`), exactly matching `produces_at_tick`.
            let idle_monitors = monitors.saturating_sub(1);
            let idle_frames_per_monitor = effective_frames.div_ceil(IDLE_DUTY_CYCLE_TICKS);
            effective_frames.saturating_add(idle_monitors.saturating_mul(idle_frames_per_monitor))
        }
    }
}

/// Computes the maximum number of frames any single monitor will produce
/// across this run — the correct bound for the receiver-delay completion
/// floor now that `receiver_delay` is a per-monitor, post-demux
/// consumer/validation delay applied on independent, parallel per-monitor
/// paths by **both** carriers (Carrier A's demux consumer, see
/// [`carrier_a_receive_all`]; Carrier B's per-monitor stream task, see
/// [`carrier_b_receive_one`]): the run is only as slow as its single
/// busiest monitor's own serialized `receiver_delay` cost, never the sum of
/// every monitor's cost added together — that sum would only be correct
/// for one fully-serial consumer processing every monitor's frames one
/// after another, which is no longer either carrier's actual runtime model
/// (nor, for Carrier B's always-independent per-monitor streams, ever was).
///
/// For both [`ActivePattern::AllActive`] (every monitor produces every
/// tick) and [`ActivePattern::OneActiveRestIdle`] (the one active monitor
/// produces every tick; every other monitor produces less often), the
/// busiest monitor always produces exactly `resolve_tick_count(config.
/// workload)` frames — the fully active monitor, under either pattern — so
/// this is exactly that resolved tick count, not a separate
/// pattern-branching formula the way [`offered_frame_count`] (a *sum*
/// across monitors, correctly pattern-aware) needs.
#[must_use]
pub fn max_per_monitor_offered_frame_count(config: &BenchConfig) -> u64 {
    resolve_tick_count(config.workload)
}

/// Returns the tick-pacing interval a run's producers should sleep against
/// for `workload`, or `None` if ticks should fire back-to-back (unpaced).
///
/// `Workload::Duration` is paced at [`BENCH_TICK_INTERVAL`] so its
/// production phase takes approximately the requested wall-clock duration;
/// `Workload::Frames` remains deterministic and unpaced (bounded only by
/// [`MAX_BENCH_TOTAL_BYTES`] and this run's own completion deadline, see
/// [`predicted_completion_deadline`]), matching its "exact frame count, no
/// timing claim" contract.
#[must_use]
pub fn tick_pacing_for(workload: Workload) -> Option<Duration> {
    match workload {
        Workload::Frames(_) => None,
        Workload::Duration(_) => Some(BENCH_TICK_INTERVAL),
    }
}

/// Computes this run's predicted outer completion deadline.
///
/// The deadline is the larger of two budgets, plus two additive terms:
///
/// - Budget (a): this workload's own paced production-time budget —
///   `Duration::ZERO` for an unpaced `Workload::Frames`, or the requested
///   duration itself for a paced `Workload::Duration`.
/// - Budget (b): a deliberately pessimistic data-transfer budget derived
///   from [`MIN_ASSUMED_TRANSFER_BYTES_PER_SEC`] applied to the run's total
///   *encoded* (wire) bytes — see [`encoded_frame_bytes`] — not payload
///   bytes alone, so a small configured `payload_bytes` does not
///   under-budget the fixed per-frame header's own real transfer cost at
///   a large offered frame count. Budgets (a) and (b) are
///   not summed together, because production and transfer proceed
///   concurrently in the common case, not sequentially.
/// - Additive term: the receiver-delay-driven completion floor
///   ([`max_per_monitor_offered_frame_count`] times `receiver_delay` — the
///   same carrier-neutral, per-monitor bound
///   [`BenchConfig::validate`] itself caps via
///   [`MAX_BENCH_RECEIVER_DELAY_FLOOR`]), which is additive on top of the
///   budgets above since it is genuinely serialized on top of ordinary
///   frame delivery, on whichever monitor's own consumer path is busiest.
/// - Additive term: a small fixed [`BENCH_COMPLETION_DRAIN_ALLOWANCE`] for
///   stream-finish/task-join jitter at the very end of a run.
///
/// This does not itself bound a config — [`BenchConfig::validate`] already
/// rejects pathological `receiver_delay`/frame-count/monitor combinations
/// up front — it is the actual runtime deadline `run_carrier_a`/
/// `run_carrier_b` enforce so an unexpected stall (e.g. a scheduler or
/// backpressure bug) surfaces as a typed
/// [`BenchRunError::CompletionTimeout`] instead of hanging indefinitely.
#[must_use]
pub fn predicted_completion_deadline(config: &BenchConfig) -> Duration {
    let offered_frames = offered_frame_count(config);

    let production_budget = match config.workload {
        Workload::Frames(_) => Duration::ZERO,
        Workload::Duration(duration) => duration,
    };

    let total_bytes = offered_frames.saturating_mul(encoded_frame_bytes(config.payload_bytes));
    let transfer_budget_nanos = total_bytes
        .saturating_mul(1_000_000_000)
        .checked_div(MIN_ASSUMED_TRANSFER_BYTES_PER_SEC.max(1))
        .unwrap_or(u64::MAX);
    let transfer_budget = Duration::from_nanos(transfer_budget_nanos);

    let max_per_monitor_frames = max_per_monitor_offered_frame_count(config);
    let max_per_monitor_frames_u32 = u32::try_from(max_per_monitor_frames).unwrap_or(u32::MAX);
    let receiver_delay_floor = config
        .receiver_delay
        .saturating_mul(max_per_monitor_frames_u32);

    production_budget
        .max(transfer_budget)
        .saturating_add(receiver_delay_floor)
        .saturating_add(BENCH_COMPLETION_DRAIN_ALLOWANCE)
}

/// Returns `true` if `monitor_index` (0-based, in monitor-id order) should
/// produce a frame at `tick` (0-based) under `pattern`.
#[must_use]
pub fn produces_at_tick(pattern: ActivePattern, monitor_index: usize, tick: u64) -> bool {
    match pattern {
        ActivePattern::AllActive => true,
        ActivePattern::OneActiveRestIdle => monitor_index == 0 || tick % IDLE_DUTY_CYCLE_TICKS == 0,
    }
}

/// Returns the Carrier A scheduler weight for `monitor_index` (0-based)
/// under `pattern`.
#[must_use]
pub fn scheduler_weight_for(pattern: ActivePattern, monitor_index: usize) -> u32 {
    match pattern {
        ActivePattern::AllActive => 1,
        ActivePattern::OneActiveRestIdle => {
            if monitor_index == 0 {
                ACTIVE_MONITOR_SCHEDULER_WEIGHT
            } else {
                IDLE_MONITOR_SCHEDULER_WEIGHT
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Frame envelope
// ---------------------------------------------------------------------------

/// One benchmark frame envelope: monitor id, per-monitor sequence, a
/// shared-epoch send timestamp, and a deterministic payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BenchFrame {
    /// Session-scoped monitor id (matches [`MonitorStreamIdentity::session_monitor_id`]
    /// for Carrier B; an assigned 1-based index for Carrier A).
    pub monitor_id: u16,
    /// Zero-based sequence number, scoped to this monitor.
    pub sequence: u64,
    /// Nanoseconds since a shared in-process [`Instant`] epoch captured once
    /// per run, before any sender or receiver task starts.
    pub send_nanos: u64,
    /// Deterministically generated application payload.
    pub payload: Vec<u8>,
}

/// Fail-closed benchmark frame decode rejection reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameCodecError {
    /// Bad magic, bad version, or a frame shorter than the fixed header.
    Malformed,
    /// The declared payload length did not match the remaining bytes.
    LengthMismatch,
    /// The declared payload length exceeded [`MAX_BENCH_PAYLOAD_BYTES`].
    OversizedPayload,
}

impl Display for FrameCodecError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::Malformed => "benchmark frame was malformed (bad magic, version, or header)",
            Self::LengthMismatch => "benchmark frame payload length did not match its declaration",
            Self::OversizedPayload => "benchmark frame payload length exceeded the safety cap",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for FrameCodecError {}

impl BenchFrame {
    /// Encodes this frame as a bounded binary envelope.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut buffer = Vec::with_capacity(BENCH_FRAME_HEADER_BYTES + self.payload.len());
        buffer.extend_from_slice(&BENCH_FRAME_MAGIC);
        buffer.extend_from_slice(&BENCH_FRAME_VERSION.to_be_bytes());
        buffer.extend_from_slice(&self.monitor_id.to_be_bytes());
        buffer.extend_from_slice(&self.sequence.to_be_bytes());
        buffer.extend_from_slice(&self.send_nanos.to_be_bytes());
        let payload_len = u32::try_from(self.payload.len()).unwrap_or(u32::MAX);
        buffer.extend_from_slice(&payload_len.to_be_bytes());
        buffer.extend_from_slice(&self.payload);
        buffer
    }

    /// Decodes a bounded binary envelope produced by [`Self::encode`].
    ///
    /// # Errors
    ///
    /// Returns [`FrameCodecError::Malformed`] for a bad magic, bad version,
    /// or short header; [`FrameCodecError::OversizedPayload`] if the
    /// declared payload length exceeds [`MAX_BENCH_PAYLOAD_BYTES`]; or
    /// [`FrameCodecError::LengthMismatch`] if the declared length does not
    /// match the remaining bytes.
    pub fn decode(bytes: &[u8]) -> Result<Self, FrameCodecError> {
        if bytes.len() < BENCH_FRAME_HEADER_BYTES {
            return Err(FrameCodecError::Malformed);
        }
        if bytes[0..4] != BENCH_FRAME_MAGIC {
            return Err(FrameCodecError::Malformed);
        }
        let version = u16::from_be_bytes([bytes[4], bytes[5]]);
        if version != BENCH_FRAME_VERSION {
            return Err(FrameCodecError::Malformed);
        }
        let monitor_id = u16::from_be_bytes([bytes[6], bytes[7]]);
        let sequence = u64::from_be_bytes(
            bytes[8..16]
                .try_into()
                .map_err(|_| FrameCodecError::Malformed)?,
        );
        let send_nanos = u64::from_be_bytes(
            bytes[16..24]
                .try_into()
                .map_err(|_| FrameCodecError::Malformed)?,
        );
        let payload_len = u32::from_be_bytes(
            bytes[24..28]
                .try_into()
                .map_err(|_| FrameCodecError::Malformed)?,
        ) as usize;
        if payload_len > MAX_BENCH_PAYLOAD_BYTES {
            return Err(FrameCodecError::OversizedPayload);
        }
        if bytes.len() != BENCH_FRAME_HEADER_BYTES + payload_len {
            return Err(FrameCodecError::LengthMismatch);
        }
        Ok(Self {
            monitor_id,
            sequence,
            send_nanos,
            payload: bytes[BENCH_FRAME_HEADER_BYTES..].to_vec(),
        })
    }
}

/// Deterministically generates an `len`-byte payload from `(monitor_id,
/// sequence)` using a fixed splitmix64 stream, so a receiver can regenerate
/// and byte-compare the expected payload without a shared side channel.
#[must_use]
pub fn deterministic_payload(monitor_id: u16, sequence: u64, len: usize) -> Vec<u8> {
    let mut state = (u64::from(monitor_id) << 48) ^ sequence.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let mut buffer = Vec::with_capacity(len);
    while buffer.len() < len {
        state = splitmix64_next(state);
        buffer.extend_from_slice(&state.to_le_bytes());
    }
    buffer.truncate(len);
    buffer
}

fn splitmix64_next(state: u64) -> u64 {
    let state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

// ---------------------------------------------------------------------------
// Carrier A scheduler
// ---------------------------------------------------------------------------

/// Fail-closed scheduler rejection reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchedulerError {
    /// The named monitor's bounded queue is at capacity.
    QueueFull,
    /// The named monitor was not registered with this scheduler.
    UnknownMonitor,
}

impl Display for SchedulerError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::QueueFull => "carrier A scheduler queue is at capacity",
            Self::UnknownMonitor => "carrier A scheduler has no queue for this monitor id",
        })
    }
}

impl std::error::Error for SchedulerError {}

#[derive(Debug, Clone, Copy)]
struct SchedulerEntry {
    monitor_id: u16,
    weight: i64,
    current_weight: i64,
}

/// A pure, synchronous, bounded weighted round-robin scheduler modeling
/// Carrier A's planned multiplexing of several monitors' frames onto one
/// reliable stream. Selection uses the same smooth weighted round-robin
/// algorithm as nginx's upstream load balancer: every pick raises each
/// entry's `current_weight` by its fixed weight, then selects (and
/// discounts by the total weight) the entry with the highest current
/// weight. This spreads a high-weight monitor's extra turns evenly across
/// the rotation instead of bursting them consecutively, and it deliberately
/// does not simply drain monitor 0 to exhaustion before considering any
/// other monitor.
#[derive(Debug)]
pub struct WeightedRoundRobinScheduler {
    entries: Vec<SchedulerEntry>,
    queues: HashMap<u16, VecDeque<BenchFrame>>,
    capacity: usize,
}

impl WeightedRoundRobinScheduler {
    /// Creates a scheduler with one bounded queue per `(monitor_id, weight)`
    /// pair. A `weight` of `0` is treated as `1` so every registered
    /// monitor always gets scheduling turns.
    #[must_use]
    pub fn new(weights: &[(u16, u32)], capacity: usize) -> Self {
        let entries = weights
            .iter()
            .map(|&(monitor_id, weight)| SchedulerEntry {
                monitor_id,
                weight: i64::from(weight.max(1)),
                current_weight: 0,
            })
            .collect();
        let queues = weights
            .iter()
            .map(|&(monitor_id, _)| (monitor_id, VecDeque::with_capacity(capacity)))
            .collect();
        Self {
            entries,
            queues,
            capacity,
        }
    }

    /// Attempts to enqueue `frame` onto `monitor_id`'s bounded queue.
    ///
    /// # Errors
    ///
    /// Returns the frame back alongside [`SchedulerError::QueueFull`] if
    /// the queue is at capacity, or [`SchedulerError::UnknownMonitor`] if
    /// `monitor_id` was not registered at construction. Callers must retry
    /// (never drop) on `QueueFull` to preserve reliable delivery.
    pub fn try_enqueue(
        &mut self,
        monitor_id: u16,
        frame: BenchFrame,
    ) -> Result<(), (SchedulerError, BenchFrame)> {
        let Some(queue) = self.queues.get_mut(&monitor_id) else {
            return Err((SchedulerError::UnknownMonitor, frame));
        };
        if queue.len() >= self.capacity {
            return Err((SchedulerError::QueueFull, frame));
        }
        queue.push_back(frame);
        Ok(())
    }

    /// Selects and dequeues the next frame to write, or `None` if every
    /// queue is currently empty.
    ///
    /// This never returns `None` while at least one queue is non-empty:
    /// the winner search below only ever compares entries whose queue
    /// currently holds a frame, so a winner always exists in exactly one
    /// pass whenever any queue has data (see the regression test with
    /// weights `[4, 1]` where only the weight-`1` monitor's queue is
    /// populated — searching for the winner over *every* registered
    /// entry, rather than only the active ones, could exhaust this
    /// method's selection attempts on a high-weight monitor whose queue
    /// happened to be empty and incorrectly return `None`).
    pub fn pop_next(&mut self) -> Option<BenchFrame> {
        if self.queues.values().all(VecDeque::is_empty) {
            return None;
        }
        let total: i64 = self.entries.iter().map(|entry| entry.weight).sum();
        for entry in &mut self.entries {
            entry.current_weight += entry.weight;
        }
        // First-index-wins on ties (not `Iterator::max_by_key`, which
        // resolves ties to the *last* equally maximal element): this
        // keeps the smooth weighted round-robin trace deterministic and
        // matches this module's documented, hand-verified pick sequences.
        // Restricting the search to entries whose queue currently has a
        // frame ready ("active" queues) is what guarantees a winner is
        // found in this single pass whenever any queue is non-empty.
        let mut winner_index: Option<usize> = None;
        for (index, entry) in self.entries.iter().enumerate() {
            let queue_has_frame = self
                .queues
                .get(&entry.monitor_id)
                .is_some_and(|queue| !queue.is_empty());
            if !queue_has_frame {
                continue;
            }
            winner_index = match winner_index {
                None => Some(index),
                Some(current_best)
                    if entry.current_weight > self.entries[current_best].current_weight =>
                {
                    Some(index)
                }
                Some(current_best) => Some(current_best),
            };
        }
        // At least one queue is non-empty (checked above), so the loop
        // above always finds at least one active entry. The `?` here is
        // unreachable in practice; it returns `None` rather than
        // panicking so this invariant can never turn into a release-mode
        // panic even if the reasoning above were ever wrong.
        let winner_index = winner_index?;
        self.entries[winner_index].current_weight -= total;
        let monitor_id = self.entries[winner_index].monitor_id;
        self.queues
            .get_mut(&monitor_id)
            .and_then(VecDeque::pop_front)
    }

    /// Returns `true` if every bounded queue is currently empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.queues.values().all(VecDeque::is_empty)
    }
}

// ---------------------------------------------------------------------------
// Per-monitor validation and metrics
// ---------------------------------------------------------------------------

/// Accumulates delivery validation and latency/gap measurements for one
/// monitor's received frames.
#[derive(Debug)]
pub struct PerMonitorValidator {
    monitor_id: u16,
    expected_sequence: u64,
    delivered_frames: u64,
    delivered_bytes: u64,
    first_frame_at: Option<Duration>,
    last_frame_at: Option<Duration>,
    /// The first delivered frame's own latency, captured at record time —
    /// deliberately kept separate from `latencies_nanos` (which is later
    /// sorted in [`Self::finish`] to compute percentiles) so it always
    /// reflects the chronologically first frame, never the post-sort
    /// minimum.
    first_frame_latency_nanos: Option<u64>,
    latencies_nanos: Vec<u64>,
    max_inter_arrival_gap: Duration,
    ordering_failures: u64,
    payload_failures: u64,
    monitor_id_mismatches: u64,
}

impl PerMonitorValidator {
    /// Creates a validator expecting sequence numbers starting at `0` for
    /// `monitor_id` (the monitor identity accepted for this stream — for
    /// Carrier B, the parsed `MonitorStreamIdentity::session_monitor_id`;
    /// for Carrier A, the assigned per-monitor id).
    #[must_use]
    pub fn new(monitor_id: u16) -> Self {
        Self {
            monitor_id,
            expected_sequence: 0,
            delivered_frames: 0,
            delivered_bytes: 0,
            first_frame_at: None,
            last_frame_at: None,
            first_frame_latency_nanos: None,
            latencies_nanos: Vec::new(),
            max_inter_arrival_gap: Duration::ZERO,
            ordering_failures: 0,
            payload_failures: 0,
            monitor_id_mismatches: 0,
        }
    }

    /// Records one received frame at `receive_elapsed` (time since the
    /// shared run epoch — for both carriers, captured by
    /// [`carrier_receive_consume_one`] *after* applying any configured
    /// `receiver_delay`, so `latency`/gap measurements below include that
    /// delay's cost at the same pipeline point for both carriers).
    /// Validates payload correctness (regenerated and byte-compared, not
    /// just length) and strict per-monitor ordering. A single ordering
    /// mismatch is counted once and resets the expected next sequence to
    /// the observed value, so it never cascades into every subsequent
    /// frame also being flagged.
    ///
    /// A frame whose `monitor_id` does not match the monitor identity this
    /// validator was created for is a distinct, always-enforced rejection
    /// (not a debug-only assertion, which would compile out entirely in
    /// the release builds this benchmark is meant to run in): it is
    /// counted in `monitor_id_mismatches` and otherwise discarded —
    /// neither its payload/ordering state nor its latency/gap
    /// measurements are folded into this monitor's series, since it never
    /// legitimately belongs to this monitor's per-stream identity (see
    /// Carrier B's `open_monitor_stream`/`accept_monitor_stream` preface).
    pub fn record(&mut self, frame: &BenchFrame, receive_elapsed: Duration) {
        if frame.monitor_id != self.monitor_id {
            self.monitor_id_mismatches += 1;
            return;
        }

        let expected_payload =
            deterministic_payload(frame.monitor_id, frame.sequence, frame.payload.len());
        if frame.payload != expected_payload {
            self.payload_failures += 1;
        }
        if frame.sequence != self.expected_sequence {
            self.ordering_failures += 1;
        }
        self.expected_sequence = frame.sequence + 1;

        let send_elapsed = Duration::from_nanos(frame.send_nanos);
        let latency = receive_elapsed.saturating_sub(send_elapsed);
        let latency_nanos = u64::try_from(latency.as_nanos()).unwrap_or(u64::MAX);
        if self.first_frame_latency_nanos.is_none() {
            self.first_frame_latency_nanos = Some(latency_nanos);
        }
        self.latencies_nanos.push(latency_nanos);

        if let Some(last) = self.last_frame_at {
            let gap = receive_elapsed.saturating_sub(last);
            if gap > self.max_inter_arrival_gap {
                self.max_inter_arrival_gap = gap;
            }
        }
        if self.first_frame_at.is_none() {
            self.first_frame_at = Some(receive_elapsed);
        }
        self.last_frame_at = Some(receive_elapsed);
        self.delivered_frames += 1;
        self.delivered_bytes += frame.payload.len() as u64;
    }

    /// Consumes this validator into final per-monitor metrics.
    #[must_use]
    pub fn finish(mut self, sent_frames: u64, sent_bytes: u64) -> PerMonitorMetrics {
        self.latencies_nanos.sort_unstable();
        let elapsed = match (self.first_frame_at, self.last_frame_at) {
            (Some(first), Some(last)) if self.delivered_frames >= 2 => last.saturating_sub(first),
            _ => Duration::ZERO,
        };
        // `delivered_bytes` is bounded well under 2^52 by `MAX_BENCH_TOTAL_BYTES`,
        // so this diagnostic-only throughput calculation cannot lose meaningful
        // precision converting to `f64`.
        #[allow(clippy::cast_precision_loss)]
        let throughput_bytes_per_sec = if elapsed.as_secs_f64() > 0.0 {
            self.delivered_bytes as f64 / elapsed.as_secs_f64()
        } else {
            0.0
        };
        PerMonitorMetrics {
            monitor_id: self.monitor_id,
            sent_frames,
            sent_bytes,
            delivered_frames: self.delivered_frames,
            delivered_bytes: self.delivered_bytes,
            elapsed,
            throughput_bytes_per_sec,
            first_frame_latency_nanos: self.first_frame_latency_nanos,
            p50_latency_nanos: percentile(&self.latencies_nanos, 50.0),
            p95_latency_nanos: percentile(&self.latencies_nanos, 95.0),
            p99_latency_nanos: percentile(&self.latencies_nanos, 99.0),
            max_inter_arrival_gap_nanos: u64::try_from(self.max_inter_arrival_gap.as_nanos())
                .unwrap_or(u64::MAX),
            ordering_failures: self.ordering_failures,
            payload_failures: self.payload_failures,
            monitor_id_mismatches: self.monitor_id_mismatches,
            completion_failures: sent_frames.saturating_sub(self.delivered_frames),
            recovery_failures: 0,
        }
    }
}

/// Nearest-rank percentile over an ascending-sorted slice: the smallest
/// element such that at least `pct` percent of the data is less than or
/// equal to it (the standard nearest-rank definition). Formally, for a
/// 1-based rank `n = ceil((pct / 100.0) * len)`, clamped to `1..=len`, this
/// returns `sorted_ascending[n - 1]`. `pct` is in `0.0..=100.0`. Returns
/// `None` for an empty slice.
///
/// Rank/index arithmetic here is bounded by `sorted_ascending.len()`, which
/// this diagnostic keeps well under `f64`'s 52-bit mantissa (bounded by
/// `MAX_BENCH_FRAMES`), so the `f64` round-trip cannot silently misselect
/// an index.
#[must_use]
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
pub fn percentile(sorted_ascending: &[u64], pct: f64) -> Option<u64> {
    if sorted_ascending.is_empty() {
        return None;
    }
    let len = sorted_ascending.len();
    let rank = ((pct / 100.0) * len as f64).ceil();
    let rank = rank.clamp(1.0, len as f64) as usize;
    sorted_ascending.get(rank - 1).copied()
}

/// Jain's fairness index over non-negative values: `(sum x)^2 / (n * sum
/// x^2)`, in `0.0..=1.0` where `1.0` is perfectly fair. Returns `1.0` for an
/// all-zero or empty input (vacuously fair: no unfair advantage observed).
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn jains_fairness_index(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 1.0;
    }
    let sum: f64 = values.iter().sum();
    let sum_sq: f64 = values.iter().map(|value| value * value).sum();
    if sum_sq == 0.0 {
        return 1.0;
    }
    (sum * sum) / (values.len() as f64 * sum_sq)
}

/// Final measurements for one monitor within one carrier run.
#[derive(Debug, Clone, Copy)]
pub struct PerMonitorMetrics {
    /// This monitor's id.
    pub monitor_id: u16,
    /// Frames the sender produced for this monitor.
    pub sent_frames: u64,
    /// Application **payload** bytes the sender produced for this monitor
    /// — deliberately excludes the fixed per-frame wire envelope header
    /// (magic, version, monitor id, sequence, send timestamp, payload
    /// length; see [`encoded_frame_bytes`]), which is a separate figure
    /// used only internally for this module's byte-based safety caps and
    /// completion-deadline transfer budget, not reported as its own
    /// metric here.
    pub sent_bytes: u64,
    /// Frames the receiver validated for this monitor.
    pub delivered_frames: u64,
    /// Application **payload** bytes the receiver validated for this
    /// monitor — see `sent_bytes`'s own doc comment: payload only, not
    /// the wire envelope header.
    pub delivered_bytes: u64,
    /// Wall-clock span between this monitor's first and last delivered
    /// frame (`Duration::ZERO` if fewer than two frames were delivered).
    pub elapsed: Duration,
    /// `delivered_bytes / elapsed`, or `0.0` if `elapsed` is zero. A
    /// **payload**-bytes-per-second figure (see `delivered_bytes`'s own
    /// doc comment), not a wire-bytes-per-second one.
    pub throughput_bytes_per_sec: f64,
    /// Latency of the first delivered frame, in nanoseconds.
    pub first_frame_latency_nanos: Option<u64>,
    /// 50th-percentile receive latency, in nanoseconds.
    pub p50_latency_nanos: Option<u64>,
    /// 95th-percentile receive latency, in nanoseconds.
    pub p95_latency_nanos: Option<u64>,
    /// 99th-percentile receive latency, in nanoseconds.
    pub p99_latency_nanos: Option<u64>,
    /// Largest gap observed between two consecutive delivered frames, in
    /// nanoseconds (a starvation/head-of-line indicator).
    pub max_inter_arrival_gap_nanos: u64,
    /// Count of frames delivered out of the expected strict sequence.
    pub ordering_failures: u64,
    /// Count of frames whose payload did not match its deterministic
    /// expected content.
    pub payload_failures: u64,
    /// Count of frames received on this monitor's accepted stream/series
    /// whose envelope `monitor_id` did not match this monitor's accepted
    /// identity. Rejected, not counted as delivered — see
    /// [`PerMonitorValidator::record`].
    pub monitor_id_mismatches: u64,
    /// `sent_frames - delivered_frames` (frames the sender produced that
    /// never validated as delivered).
    pub completion_failures: u64,
    /// Always `0`; this foundation implements no reconnect/recovery path.
    pub recovery_failures: u64,
}

impl PerMonitorMetrics {
    /// Renders this metric set as one indented JSON object (no trailing
    /// comma, caller indents/joins as needed).
    #[must_use]
    pub fn to_json(&self, indent: usize) -> String {
        let pad = " ".repeat(indent);
        let pad_inner = " ".repeat(indent + 2);
        format!(
            "{pad}{{\n\
             {pad_inner}\"monitor_id\": {},\n\
             {pad_inner}\"sent_frames\": {},\n\
             {pad_inner}\"sent_bytes\": {},\n\
             {pad_inner}\"delivered_frames\": {},\n\
             {pad_inner}\"delivered_bytes\": {},\n\
             {pad_inner}\"elapsed_secs\": {:.6},\n\
             {pad_inner}\"throughput_bytes_per_sec\": {:.3},\n\
             {pad_inner}\"first_frame_latency_nanos\": {},\n\
             {pad_inner}\"p50_latency_nanos\": {},\n\
             {pad_inner}\"p95_latency_nanos\": {},\n\
             {pad_inner}\"p99_latency_nanos\": {},\n\
             {pad_inner}\"max_inter_arrival_gap_nanos\": {},\n\
             {pad_inner}\"ordering_failures\": {},\n\
             {pad_inner}\"payload_failures\": {},\n\
             {pad_inner}\"monitor_id_mismatches\": {},\n\
             {pad_inner}\"completion_failures\": {},\n\
             {pad_inner}\"recovery_failures\": {}\n\
             {pad}}}",
            self.monitor_id,
            self.sent_frames,
            self.sent_bytes,
            self.delivered_frames,
            self.delivered_bytes,
            self.elapsed.as_secs_f64(),
            self.throughput_bytes_per_sec,
            json_opt_u64(self.first_frame_latency_nanos),
            json_opt_u64(self.p50_latency_nanos),
            json_opt_u64(self.p95_latency_nanos),
            json_opt_u64(self.p99_latency_nanos),
            self.max_inter_arrival_gap_nanos,
            self.ordering_failures,
            self.payload_failures,
            self.monitor_id_mismatches,
            self.completion_failures,
            self.recovery_failures,
        )
    }
}

fn json_opt_u64(value: Option<u64>) -> String {
    value.map_or_else(|| "null".to_owned(), |value| value.to_string())
}

/// Aggregate measurements across every monitor in one carrier run.
#[derive(Debug, Clone, Copy)]
pub struct AggregateMetrics {
    /// Total frames sent across all monitors.
    pub total_sent_frames: u64,
    /// Total application **payload** bytes sent across all monitors — see
    /// [`PerMonitorMetrics::sent_bytes`]'s own doc comment: payload only,
    /// not the wire envelope header (see [`encoded_frame_bytes`]).
    pub total_sent_bytes: u64,
    /// Total frames delivered across all monitors.
    pub total_delivered_frames: u64,
    /// Total application **payload** bytes delivered across all monitors
    /// — see [`PerMonitorMetrics::delivered_bytes`]'s own doc comment:
    /// payload only, not the wire envelope header.
    pub total_delivered_bytes: u64,
    /// Wall-clock span of the entire carrier run.
    pub elapsed: Duration,
    /// `total_delivered_bytes / elapsed`, or `0.0` if `elapsed` is zero. A
    /// **payload**-bytes-per-second figure, not a wire-bytes-per-second
    /// one — see `total_delivered_bytes`'s own doc comment.
    pub aggregate_throughput_bytes_per_sec: f64,
    /// Jain's fairness index over each monitor's **delivery ratio**
    /// (`delivered_bytes / sent_bytes`, or `1.0` for a monitor that had
    /// nothing to send in this run) — normalized against what each monitor
    /// was actually offered, not raw delivered-byte volume. This
    /// deliberately factors out the synthetic workload's own per-monitor
    /// imbalance: `one-active-rest-idle` intentionally sends far fewer
    /// bytes to the idle monitors, so a fully successful run reports
    /// fairness close to `1.0` under either active pattern; only a
    /// genuinely uneven *delivery outcome* relative to what was offered
    /// lowers this value. See `delivered_bytes_max_min_spread_ratio` for
    /// the separate, un-normalized raw-volume spread, which *is* expected
    /// to differ by pattern.
    pub fairness_index: f64,
    /// Ratio of the maximum to the minimum per-monitor **delivered byte**
    /// count (raw volume, not normalized against what was sent/offered).
    /// `1.0` if every monitor delivered the same number of bytes;
    /// `f64::INFINITY` if any monitor delivered zero bytes while another
    /// delivered more than zero. This reflects the synthetic workload's
    /// own per-monitor volume shape (e.g. intentionally large under
    /// `one-active-rest-idle`), not carrier fairness — see
    /// `fairness_index` for the delivery-ratio-normalized measure of that.
    pub delivered_bytes_max_min_spread_ratio: f64,
    /// Sum of every monitor's `completion_failures`.
    pub total_completion_failures: u64,
    /// Sum of every monitor's `monitor_id_mismatches`.
    pub total_monitor_id_mismatches: u64,
    /// Sum of every monitor's `recovery_failures` (always `0`).
    pub total_recovery_failures: u64,
}

impl AggregateMetrics {
    /// Computes aggregate metrics from a run's per-monitor metrics and the
    /// overall wall-clock elapsed time.
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn from_per_monitor(per_monitor: &[PerMonitorMetrics], elapsed: Duration) -> Self {
        let total_sent_frames = per_monitor.iter().map(|metric| metric.sent_frames).sum();
        let total_sent_bytes = per_monitor.iter().map(|metric| metric.sent_bytes).sum();
        let total_delivered_frames = per_monitor
            .iter()
            .map(|metric| metric.delivered_frames)
            .sum();
        let total_delivered_bytes: u64 = per_monitor
            .iter()
            .map(|metric| metric.delivered_bytes)
            .sum();
        // `total_delivered_bytes` is bounded well under 2^52 by
        // `MAX_BENCH_TOTAL_BYTES`, so this diagnostic-only throughput
        // calculation cannot lose meaningful precision converting to `f64`.
        let aggregate_throughput_bytes_per_sec = if elapsed.as_secs_f64() > 0.0 {
            total_delivered_bytes as f64 / elapsed.as_secs_f64()
        } else {
            0.0
        };
        // Fairness is computed over each monitor's delivery *ratio*
        // (delivered/sent), not raw delivered bytes: see the doc comment
        // on `fairness_index` above for why raw volume would conflate the
        // synthetic workload's intentional per-monitor imbalance with an
        // actual unfair delivery outcome. A monitor offered nothing in
        // this run (`sent_bytes == 0`) is vacuously fully served.
        let delivery_ratios: Vec<f64> = per_monitor
            .iter()
            .map(|metric| {
                if metric.sent_bytes == 0 {
                    1.0
                } else {
                    metric.delivered_bytes as f64 / metric.sent_bytes as f64
                }
            })
            .collect();
        let fairness_index = jains_fairness_index(&delivery_ratios);
        let delivered_bytes_values: Vec<f64> = per_monitor
            .iter()
            .map(|metric| metric.delivered_bytes as f64)
            .collect();
        let max_delivered = delivered_bytes_values
            .iter()
            .copied()
            .fold(0.0_f64, f64::max);
        let min_delivered = delivered_bytes_values
            .iter()
            .copied()
            .fold(f64::INFINITY, f64::min);
        let delivered_bytes_max_min_spread_ratio = if min_delivered > 0.0 {
            max_delivered / min_delivered
        } else if max_delivered > 0.0 {
            f64::INFINITY
        } else {
            1.0
        };
        let total_completion_failures = per_monitor
            .iter()
            .map(|metric| metric.completion_failures)
            .sum();
        let total_monitor_id_mismatches = per_monitor
            .iter()
            .map(|metric| metric.monitor_id_mismatches)
            .sum();
        let total_recovery_failures = per_monitor
            .iter()
            .map(|metric| metric.recovery_failures)
            .sum();
        Self {
            total_sent_frames,
            total_sent_bytes,
            total_delivered_frames,
            total_delivered_bytes,
            elapsed,
            aggregate_throughput_bytes_per_sec,
            fairness_index,
            delivered_bytes_max_min_spread_ratio,
            total_completion_failures,
            total_monitor_id_mismatches,
            total_recovery_failures,
        }
    }

    /// Renders this aggregate as one indented JSON object.
    #[must_use]
    pub fn to_json(&self, indent: usize) -> String {
        let pad = " ".repeat(indent);
        let pad_inner = " ".repeat(indent + 2);
        format!(
            "{pad}{{\n\
             {pad_inner}\"total_sent_frames\": {},\n\
             {pad_inner}\"total_sent_bytes\": {},\n\
             {pad_inner}\"total_delivered_frames\": {},\n\
             {pad_inner}\"total_delivered_bytes\": {},\n\
             {pad_inner}\"elapsed_secs\": {:.6},\n\
             {pad_inner}\"aggregate_throughput_bytes_per_sec\": {:.3},\n\
             {pad_inner}\"fairness_index\": {:.6},\n\
             {pad_inner}\"delivered_bytes_max_min_spread_ratio\": {},\n\
             {pad_inner}\"total_completion_failures\": {},\n\
             {pad_inner}\"total_monitor_id_mismatches\": {},\n\
             {pad_inner}\"total_recovery_failures\": {}\n\
             {pad}}}",
            self.total_sent_frames,
            self.total_sent_bytes,
            self.total_delivered_frames,
            self.total_delivered_bytes,
            self.elapsed.as_secs_f64(),
            self.aggregate_throughput_bytes_per_sec,
            self.fairness_index,
            json_f64(self.delivered_bytes_max_min_spread_ratio),
            self.total_completion_failures,
            self.total_monitor_id_mismatches,
            self.total_recovery_failures,
        )
    }
}

fn json_f64(value: f64) -> String {
    if value.is_finite() {
        format!("{value:.6}")
    } else {
        // JSON has no literal infinity; render it as a quoted sentinel so
        // the document stays valid JSON while remaining unambiguous.
        "\"inf\"".to_owned()
    }
}

/// One carrier's complete run result: every monitor's metrics plus the
/// aggregate.
#[derive(Debug, Clone)]
pub struct CarrierRunResult {
    /// Which carrier produced this result.
    pub carrier: CarrierKind,
    /// Per-monitor metrics, ordered by monitor id.
    pub per_monitor: Vec<PerMonitorMetrics>,
    /// Aggregate metrics across every monitor.
    pub aggregate: AggregateMetrics,
}

impl CarrierRunResult {
    /// Renders this run as one indented JSON object.
    #[must_use]
    pub fn to_json(&self, indent: usize) -> String {
        let pad = " ".repeat(indent);
        let pad_inner = " ".repeat(indent + 2);
        let per_monitor_json = self
            .per_monitor
            .iter()
            .map(|metric| metric.to_json(indent + 4))
            .collect::<Vec<_>>()
            .join(",\n");
        format!(
            "{pad}{{\n\
             {pad_inner}\"carrier\": \"{}\",\n\
             {pad_inner}\"per_monitor\": [\n{per_monitor_json}\n{pad_inner}],\n\
             {pad_inner}\"aggregate\": {}\n\
             {pad}}}",
            self.carrier,
            self.aggregate.to_json(indent + 2).trim_start(),
        )
    }

    /// Renders a concise, human-readable summary of this run.
    #[must_use]
    pub fn human_summary(&self) -> String {
        let mut lines = vec![format!(
            "{}: {} monitors, {} frames sent / {} delivered, {:.3} MiB/s aggregate, fairness={:.3}, raw_delivered_bytes_spread={}",
            self.carrier,
            self.per_monitor.len(),
            self.aggregate.total_sent_frames,
            self.aggregate.total_delivered_frames,
            self.aggregate.aggregate_throughput_bytes_per_sec / (1024.0 * 1024.0),
            self.aggregate.fairness_index,
            if self
                .aggregate
                .delivered_bytes_max_min_spread_ratio
                .is_finite()
            {
                format!("{:.3}", self.aggregate.delivered_bytes_max_min_spread_ratio)
            } else {
                "inf".to_owned()
            },
        )];
        for metric in &self.per_monitor {
            lines.push(format!(
                "  monitor {}: sent={} delivered={} bytes={} first_latency={:?} p50={:?} p95={:?} p99={:?} max_gap={:?} order_fail={} payload_fail={} monitor_mismatch={} completion_fail={}",
                metric.monitor_id,
                metric.sent_frames,
                metric.delivered_frames,
                metric.delivered_bytes,
                metric.first_frame_latency_nanos.map(Duration::from_nanos),
                metric.p50_latency_nanos.map(Duration::from_nanos),
                metric.p95_latency_nanos.map(Duration::from_nanos),
                metric.p99_latency_nanos.map(Duration::from_nanos),
                Duration::from_nanos(metric.max_inter_arrival_gap_nanos),
                metric.ordering_failures,
                metric.payload_failures,
                metric.monitor_id_mismatches,
                metric.completion_failures,
            ));
        }
        lines.join("\n")
    }
}

/// A full Carrier A vs Carrier B comparison: the configuration used plus
/// each requested carrier's result. Only the carriers selected by
/// [`CarrierSelection`] are populated.
#[derive(Debug, Clone)]
pub struct ComparisonResult {
    /// The configuration this comparison ran under.
    pub config: BenchConfig,
    /// Carrier A's result, if selected.
    pub carrier_a: Option<CarrierRunResult>,
    /// Carrier B's result, if selected.
    pub carrier_b: Option<CarrierRunResult>,
}

impl ComparisonResult {
    /// Renders this comparison as one stable, indented JSON document.
    ///
    /// Top-level shape (stable keys, safe for a parent process to parse):
    ///
    /// ```text
    /// {
    ///   "config": {
    ///     "monitors": <u8>,
    ///     "workload": "frames:<u64>" | "duration_ms:<u128>",
    ///     "payload_bytes": <usize>,
    ///     "pattern": "all-active" | "one-active-rest-idle",
    ///     "receiver_delay_nanos": <u64>,
    ///     "offered_frame_count": <u64>
    ///   },
    ///   "carrier_a": <carrier run object> | null,
    ///   "carrier_b": <carrier run object> | null
    /// }
    /// ```
    ///
    /// `offered_frame_count` is this run's exact, pattern-expanded total
    /// frame count across every monitor (see [`offered_frame_count`]) — the
    /// same authoritative figure used to derive the total-bytes cap, the
    /// receiver-delay completion floor, and the completion deadline, so a
    /// caller can independently sanity-check delivered totals against the
    /// offered load. Each present carrier run object is
    /// [`CarrierRunResult::to_json`]'s own shape: `carrier`, `per_monitor`
    /// (array), and `aggregate`.
    #[must_use]
    pub fn to_json(&self) -> String {
        let json_a = self.carrier_a.as_ref().map_or_else(
            || "null".to_owned(),
            |run| run.to_json(4).trim_start().to_owned(),
        );
        let json_b = self.carrier_b.as_ref().map_or_else(
            || "null".to_owned(),
            |run| run.to_json(4).trim_start().to_owned(),
        );
        format!(
            "{{\n\
             \x20\x20\"config\": {{\n\
             \x20\x20\x20\x20\"monitors\": {},\n\
             \x20\x20\x20\x20\"workload\": \"{}\",\n\
             \x20\x20\x20\x20\"payload_bytes\": {},\n\
             \x20\x20\x20\x20\"pattern\": \"{}\",\n\
             \x20\x20\x20\x20\"receiver_delay_nanos\": {},\n\
             \x20\x20\x20\x20\"offered_frame_count\": {}\n\
             \x20\x20}},\n\
             \x20\x20\"carrier_a\": {json_a},\n\
             \x20\x20\"carrier_b\": {json_b}\n\
             }}",
            self.config.monitors,
            match self.config.workload {
                Workload::Frames(frames) => format!("frames:{frames}"),
                Workload::Duration(duration) => format!("duration_ms:{}", duration.as_millis()),
            },
            self.config.payload_bytes,
            self.config.pattern,
            self.config.receiver_delay.as_nanos(),
            offered_frame_count(&self.config),
        )
    }

    /// Renders a concise, human-readable summary of both runs.
    #[must_use]
    pub fn human_summary(&self) -> String {
        let mut sections = vec![format!(
            "QUIC multi-monitor carrier bench: monitors={} pattern={} payload_bytes={} receiver_delay={:?} offered_frame_count={}",
            self.config.monitors,
            self.config.pattern,
            self.config.payload_bytes,
            self.config.receiver_delay,
            offered_frame_count(&self.config),
        )];
        if let Some(run) = &self.carrier_a {
            sections.push(run.human_summary());
        }
        if let Some(run) = &self.carrier_b {
            sections.push(run.human_summary());
        }
        sections.push(
            "NOTE: localhost/single-process diagnostic only, not glass-to-glass. \
             Real hardware (pier-linux.example.internal / Deck) end-to-end validation is required \
             before any production carrier selection (see ADR 0009)."
                .to_owned(),
        );
        sections.join("\n\n")
    }
}

// ---------------------------------------------------------------------------
// Runtime errors
// ---------------------------------------------------------------------------

/// Failure surfaced while running one carrier benchmark.
#[derive(Debug)]
pub enum BenchRunError {
    /// The supplied configuration failed validation.
    Config(BenchConfigError),
    /// A QUIC transport-level failure.
    Transport(QuicTransportError),
    /// A benchmark frame failed to decode.
    Frame(FrameCodecError),
    /// The Carrier A scheduler rejected an operation.
    Scheduler(SchedulerError),
    /// A direct-monitor stream preface failed to construct.
    Identity(MonitorStreamPrefaceError),
    /// A received frame named a monitor id this run did not expect.
    UnknownMonitorId(u16),
    /// A spawned task panicked or was cancelled.
    TaskJoin(String),
    /// The run did not complete within its predicted outer completion
    /// deadline (see [`predicted_completion_deadline`]) — a stall/
    /// backpressure safety net, not a throughput or latency claim.
    CompletionTimeout {
        /// The predicted completion deadline that was exceeded.
        deadline: Duration,
    },
}

impl Display for BenchRunError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Config(error) => write!(formatter, "invalid benchmark configuration: {error}"),
            Self::Transport(error) => write!(formatter, "transport failure: {error}"),
            Self::Frame(error) => write!(formatter, "frame codec failure: {error}"),
            Self::Scheduler(error) => write!(formatter, "scheduler failure: {error}"),
            Self::Identity(error) => write!(formatter, "monitor stream identity failure: {error}"),
            Self::UnknownMonitorId(monitor_id) => {
                write!(
                    formatter,
                    "received frame for unexpected monitor id {monitor_id}"
                )
            }
            Self::TaskJoin(message) => write!(formatter, "benchmark task failed: {message}"),
            Self::CompletionTimeout { deadline } => write!(
                formatter,
                "benchmark run exceeded its predicted completion deadline of {deadline:?}"
            ),
        }
    }
}

impl std::error::Error for BenchRunError {}

impl From<BenchConfigError> for BenchRunError {
    fn from(error: BenchConfigError) -> Self {
        Self::Config(error)
    }
}

impl From<QuicTransportError> for BenchRunError {
    fn from(error: QuicTransportError) -> Self {
        Self::Transport(error)
    }
}

impl From<FrameCodecError> for BenchRunError {
    fn from(error: FrameCodecError) -> Self {
        Self::Frame(error)
    }
}

impl From<MonitorStreamPrefaceError> for BenchRunError {
    fn from(error: MonitorStreamPrefaceError) -> Self {
        Self::Identity(error)
    }
}

// ---------------------------------------------------------------------------
// Async frame I/O helpers
// ---------------------------------------------------------------------------

async fn write_frame(send: &mut SendStream, frame: &BenchFrame) -> Result<(), BenchRunError> {
    send.write_all(&frame.encode())
        .await
        .map_err(QuicTransportError::StreamWrite)?;
    Ok(())
}

/// A destination [`carrier_a_drain`] can write encoded frames to.
///
/// Implemented for [`SendStream`] in production. Abstracting over this
/// (rather than hard-coding `SendStream`) lets `carrier_a_drain`'s
/// `Notify`-based wait/wake coordination be exercised by an in-memory test
/// sink in unit tests, without requiring a real QUIC connection just to
/// prove the drain loop's suspend/resume behaviour.
trait FrameSink {
    async fn write_frame(&mut self, frame: &BenchFrame) -> Result<(), BenchRunError>;
}

impl FrameSink for SendStream {
    async fn write_frame(&mut self, frame: &BenchFrame) -> Result<(), BenchRunError> {
        write_frame(self, frame).await
    }
}

/// Reads one frame, returning `Ok(None)` on a clean end-of-stream (no more
/// frames) as distinct from a truncated/errored read.
async fn read_frame(recv: &mut RecvStream) -> Result<Option<BenchFrame>, BenchRunError> {
    let mut header = [0_u8; BENCH_FRAME_HEADER_BYTES];
    match recv.read_exact(&mut header).await {
        Ok(()) => {}
        Err(quinn::ReadExactError::FinishedEarly(0)) => return Ok(None),
        Err(error) => return Err(QuicTransportError::StreamRead(error).into()),
    }
    let payload_len = u32::from_be_bytes([header[24], header[25], header[26], header[27]]) as usize;
    if payload_len > MAX_BENCH_PAYLOAD_BYTES {
        return Err(FrameCodecError::OversizedPayload.into());
    }
    let mut buffer = vec![0_u8; BENCH_FRAME_HEADER_BYTES + payload_len];
    buffer[..BENCH_FRAME_HEADER_BYTES].copy_from_slice(&header);
    recv.read_exact(&mut buffer[BENCH_FRAME_HEADER_BYTES..])
        .await
        .map_err(QuicTransportError::StreamRead)?;
    Ok(Some(BenchFrame::decode(&buffer)?))
}

// ---------------------------------------------------------------------------
// Structured task ownership
//
// Every task this module spawns — Carrier A's per-monitor producer tasks,
// its single demux reader task ([`carrier_a_receive_all`]), and its
// per-monitor consumer tasks; Carrier B's per-monitor sender tasks, its
// per-monitor reader tasks ([`carrier_b_receive_one`]), and its per-monitor
// consumer tasks — is spawned via [`spawn_tracked`], which returns a
// [`tokio_util::task::AbortOnDropHandle`] instead of a bare
// [`tokio::task::JoinHandle`]. A bare `JoinHandle` that is dropped without
// being awaited does **not** abort its task — the task keeps running
// fully detached in the background — which was this module's original bug:
// an early `?` return from `run_carrier_a`/`run_carrier_b`'s own
// `completion` future, or that future being dropped when the outer
// `tokio::time::timeout` in [`run_carrier_a`]/[`run_carrier_b`] elapses,
// would silently abandon every producer/receiver/consumer task that had
// not yet been individually joined, and leave the QUIC connection/streams
// exactly as they were at that instant (an implicit `finish()` on drop,
// never an explicit reset, and no connection close at all).
//
// `AbortOnDropHandle` fixes this structurally, not by convention: every
// call site that used to store a `JoinHandle` in a local variable or `Vec`
// now stores an `AbortOnDropHandle` instead, and every place that already
// joined those handles (by iterating a `Vec` or awaiting a single handle)
// keeps working completely unchanged, since `AbortOnDropHandle` also
// implements `Future<Output = Result<T, JoinError>>`. The only behavioral
// change is what happens when a handle is dropped *before* being joined:
// dropping it now calls [`tokio::task::JoinHandle::abort`] on the
// underlying task. Because Rust drops locals (including the remaining,
// not-yet-iterated elements of a `Vec`/`for` loop, and every field of an
// `async` block's own captured state) on *every* exit path — a normal
// return, an early `?` return, a panic unwind, or the future simply being
// dropped by an enclosing `tokio::time::timeout` — this cascades
// automatically through every current and future early-return path in
// this module, without needing each one to be individually audited or
// remembered. Tokio's own `abort()` schedules the task for cancellation
// directly on the runtime (it does not depend on the target task's future
// ever receiving an external wakeup), so this also correctly terminates a
// task that is blocked on a `tokio::sync::Notify`/bounded-channel wait that
// will otherwise never fire — the exact "wake blocked Notify/queue waits"
// case a plain dropped `JoinHandle` could not handle at all.
//
// `run_carrier_a`/`run_carrier_b` additionally perform two things no
// `AbortOnDropHandle` alone provides: on every non-success exit (a
// propagated `?` error or a `CompletionTimeout`), they (1) explicitly
// `reset()` (Carrier A's shared send stream) or rely on (2) below to reset
// every per-monitor stream (Carrier B has no single shared stream to reset
// directly), and (2) explicitly `close()` both connections they were
// given — Quinn immediately resets every stream still open on a closed
// connection, and unblocks any task still parked on that connection's own
// `accept_uni`/`read`/`write` calls with a prompt connection-level error,
// which is faster and more direct than waiting for task abortion alone to
// eventually tear down an I/O-blocked task. This never touches a
// successful run's connections/streams: the shared `send_stream.finish()`
// (Carrier A) / per-monitor `send.finish()` (Carrier B) calls already made
// on the success path are left exactly as they were. They also explicitly
// `abort()` and then `.await` every one of their own direct producer/
// receiver task handles before returning on a failure path, so the run's
// own top-level task graph is confirmed torn down — not merely "abort
// requested" — by the time the call returns to its caller.
//
// That last guarantee depends on every collection of handles a failure
// path hands to cleanup still actually *containing* whatever has not yet
// been joined. [`join_last`]/[`join_taken`] exist for exactly this: unlike
// `handles.drain(..)`/`handles.take()`/a plain `for handle in handles`
// loop — every one of which removes (or, for `drain`, logically detaches)
// *every* handle from the caller's own `Vec`/`Option` up front, before any
// of them have actually been joined — `join_last`/`join_taken` join a
// handle *in place* (via `&mut`) and only remove it from `handles` once
// that specific join has actually completed. So if the future doing the
// joining is itself dropped mid-loop (an outer `tokio::time::timeout`
// elapsing) or returns an error partway through (a sibling task's own
// `?`), every handle not yet joined is still sitting in the same `Vec`/
// `Option` the corresponding `cleanup_failed_carrier_a_run`/
// `cleanup_failed_carrier_b_run` call receives, ready to be explicitly
// `abort()`-ed and joined there — never silently reduced to an unjoined,
// merely-`abort()`-ed `Drop`.
//
// Critically, **no task spawned by this module ever spawns or owns a
// child task of its own.** Every producer/sender, reader, and consumer
// task is a leaf: `run_carrier_a`/`run_carrier_b` themselves create every
// per-monitor channel and spawn every consumer task
// ([`carrier_receive_consume_one`]) directly, before spawning any reader
// task, and hand the reader(s) only a `HashMap` of already-created
// `mpsc::Sender`s to look up and forward into
// ([`carrier_a_receive_all`], [`carrier_b_receive_one`]) — a reader's own
// early `?` return (an unrecognized identity, a decode failure, a closed
// channel) simply drops its own copy of that map, closing the channel(s)
// it was feeding, which lets the matching consumer task(s) observe a
// clean end-of-stream and return on their own; there is no nested handle
// for the reader to abort/join because it owns none. This eliminates a
// prior, structurally-inherent residual race in an earlier revision of
// this module (where a receiver task spawned and owned its own nested
// per-monitor consumer tasks): if *that* receiver's entire task had ever
// been aborted from the outside while still mid-way through its own
// nested cleanup, Tokio's cancellation model provides no way to run
// further `.await`s after `abort()` takes effect — dropping a cancelled
// future only runs synchronous `Drop` impls, never another `.await` — so
// a nested handle caught at exactly that moment could only ever be
// fire-and-forget aborted, not guaranteed-joined. With every task now a
// leaf owned solely by `run_carrier_a`/`run_carrier_b` themselves, every
// `abort()` + `.await` pair in `cleanup_failed_carrier_a_run`/
// `cleanup_failed_carrier_b_run` targets a task with no children of its
// own to lose track of, closing that race entirely rather than merely
// narrowing it.
//
// See `tests/quic_carrier_bench_task_lifecycle.rs` for deterministic,
// real-Quinn proof: forced `CompletionTimeout` (after every producer,
// reader, and consumer task is already confirmed running — not merely
// assumed to be), forced stream/connection failure, and forced early-error
// scenarios for both carriers each leave [`carrier_bench_live_task_count`]
// at exactly its pre-run baseline the instant the call returns (no polling
// needed), and a fresh, ordinary run launched immediately afterward still
// completes successfully — proving neither a leaked task nor any other
// global state blocks future runs.
// ---------------------------------------------------------------------------

/// Process-wide count of tasks currently spawned (via [`spawn_tracked`]) by
/// any in-flight `run_carrier_a`/`run_carrier_b` call that have not yet
/// fully finished (including tasks in the process of being torn down after
/// an `abort()`). This is diagnostic/test instrumentation for this
/// measurement tool only — not a stability-guaranteed metric, and not
/// itself part of any [`CarrierRunResult`]/[`ComparisonResult`] output —
/// kept process-wide (rather than threaded per-run through every internal
/// function signature) purely so `tests/quic_carrier_bench_task_lifecycle.rs`
/// can assert "no task leaked by this run" deterministically without
/// requiring test-only parameters on any production code path. Tests that
/// use it run in a single, serialized sequence (see that file's own
/// `SERIAL` guard) specifically so concurrent, unrelated bench runs in the
/// same test binary process can never make its readings ambiguous.
static LIVE_TASK_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Returns [`LIVE_TASK_COUNT`]'s current value. See that static's own doc
/// comment for exactly what it counts and its intended (test-only) use.
#[must_use]
pub fn carrier_bench_live_task_count() -> usize {
    LIVE_TASK_COUNT.load(Ordering::SeqCst)
}

/// RAII guard incrementing [`LIVE_TASK_COUNT`] on construction and
/// decrementing it on drop — held across a tracked task's *entire* body via
/// [`spawn_tracked`], including any time spent blocked on a `Notify` or
/// bounded-channel wait, so it is decremented exactly once whether the task
/// finishes normally, returns an error, or is aborted (a task that is
/// aborted still has its future — and therefore this guard — dropped in
/// place once the runtime processes the cancellation).
struct LiveTaskGuard;

impl LiveTaskGuard {
    fn new() -> Self {
        LIVE_TASK_COUNT.fetch_add(1, Ordering::SeqCst);
        Self
    }
}

impl Drop for LiveTaskGuard {
    fn drop(&mut self) {
        LIVE_TASK_COUNT.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Spawns `future` on the current Tokio runtime and returns an
/// [`AbortOnDropHandle`] instead of a bare [`tokio::task::JoinHandle`] —
/// every task this module spawns is owned this way for its entire
/// lifetime. See this module's "Structured task ownership" section above
/// for why this — not a bare `tokio::spawn`/`JoinHandle` — is what makes an
/// early `?` return or an enclosing `tokio::time::timeout` abort the task
/// instead of silently detaching it.
fn spawn_tracked<F>(future: F) -> AbortOnDropHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    AbortOnDropHandle::new(tokio::spawn(track_live(future)))
}

/// Wraps `future` so [`LIVE_TASK_COUNT`] is incremented/decremented across
/// its entire execution via [`LiveTaskGuard`]. Only ever called by
/// [`spawn_tracked`].
async fn track_live<F: Future>(future: F) -> F::Output {
    let _guard = LiveTaskGuard::new();
    future.await
}

/// Best-effort, non-propagating cleanup applied to both carriers' shared
/// connections on any non-success exit from `run_carrier_a`/`run_carrier_b`
/// (an early `?` error or a `CompletionTimeout`) — never on a successful
/// run. Closing a connection immediately resets every stream still open on
/// it and unblocks any task still parked on that connection's own
/// `accept_uni`/`accept_bi`/read/write calls with a prompt connection-level
/// error, which is faster and more direct than relying on task abortion
/// alone to eventually tear down an I/O-blocked task. Errors from closing
/// an already-closed/errored connection are not meaningful here (this run
/// has already failed for its own, already-captured reason) and are
/// intentionally not observable — `Connection::close` itself does not
/// return a `Result`.
fn close_connections_after_failed_run(
    sender_connection: &Connection,
    receiver_connection: &Connection,
) {
    let reason: &[u8] = b"carrier bench run failed";
    sender_connection.close(VarInt::from_u32(BENCH_RUN_FAILURE_ERROR_CODE), reason);
    receiver_connection.close(VarInt::from_u32(BENCH_RUN_FAILURE_ERROR_CODE), reason);
}

/// Aborts and joins every handle in `handles`, discarding each task's own
/// result — used only on a run's own failure/timeout path, strictly after
/// the real failure has already been captured, so a sibling task's own
/// abort-induced `JoinError` can never mask the actual cause. Every handle
/// is explicitly *awaited* after `abort()` (not merely dropped) so the
/// caller can rely on every task having actually finished — not merely
/// "abort requested" — by the time this function returns, which is what
/// lets `tests/quic_carrier_bench_task_lifecycle.rs` assert
/// [`carrier_bench_live_task_count`] deterministically instead of needing
/// its own bespoke bounded-retry logic for this part of cleanup.
async fn abort_and_join_all<T>(handles: Vec<AbortOnDropHandle<T>>) {
    for handle in handles {
        handle.abort();
        let _ = handle.await;
    }
}

/// Single-handle counterpart to [`abort_and_join_all`].
async fn abort_and_join_one<T>(handle: AbortOnDropHandle<T>) {
    handle.abort();
    let _ = handle.await;
}

/// Awaits (joins) the *last* handle in `handles`, removing it from `handles`
/// only once that specific join has actually completed — never before.
///
/// This is deliberately **not** `handles.pop()` followed by an `.await` on
/// the popped value, nor a `for handle in handles.drain(..)`/
/// `for handle in handles` loop: both of those remove (or, for `drain`,
/// logically detach) every element from `handles` up front, before any of
/// them have actually been joined. If the future doing the awaiting is
/// itself dropped mid-loop — which is exactly what happens when an
/// enclosing `tokio::time::timeout` elapses, or a sibling `?` inside the
/// same `async` block returns early — whatever handles that drain/pop-then-
/// await approach had already removed from `handles` are gone from the
/// caller's own scope, so an explicit failure-path cleanup call reaching
/// into that same `handles` variable afterward would find some or all of
/// them already missing, and could only rely on their own (unjoined,
/// merely `abort()`-ed) `Drop` impl to eventually tear them down.
///
/// By awaiting the last element *in place* (via `&mut`, not by value) and
/// popping only after that `.await` resolves, a handle is removed from
/// `handles` if and only if this function's own await actually completed —
/// including when it completes because the joined task itself returned an
/// `Err` (a `JoinError`, e.g. a panic): that is still a genuine, observed
/// completion, not a cancellation of this wait. If this function's own
/// `.await` is instead the exact point where the calling future is
/// dropped, the handle it was awaiting is still sitting in `handles`,
/// unremoved, for a failure-path cleanup call to find and explicitly
/// `abort()` + join for itself.
async fn join_last<T>(
    handles: &mut Vec<AbortOnDropHandle<T>>,
) -> Option<Result<T, tokio::task::JoinError>> {
    let result = handles.last_mut()?.await;
    handles.pop();
    Some(result)
}

/// `Option`-shaped counterpart to [`join_last`], for Carrier A's single
/// shared receiver handle (see `run_carrier_a`): awaits `*slot` in place
/// and only clears it to `None` once that join has actually completed, for
/// exactly the same reason `join_last` pops only after its own await
/// resolves.
async fn join_taken<T>(
    slot: &mut Option<AbortOnDropHandle<T>>,
) -> Option<Result<T, tokio::task::JoinError>> {
    let result = slot.as_mut()?.await;
    *slot = None;
    Some(result)
}

/// One producer/sender task's tracked handle — shared by Carrier A's
/// [`carrier_a_produce_one`] and Carrier B's [`carrier_b_send_one`], which
/// return the same `(monitor_id, sent_frames, sent_bytes)` shape.
type SenderTaskHandle = AbortOnDropHandle<Result<(u16, u64, u64), BenchRunError>>;
/// Carrier A's single demux reader task's tracked handle (see
/// [`carrier_a_receive_all`]). The reader owns no child task of its own —
/// every per-monitor consumer task is spawned directly by [`run_carrier_a`]
/// — so this handle's result carries no validators, only whether
/// demultiplexing itself succeeded.
type CarrierAReaderTaskHandle = AbortOnDropHandle<Result<(), BenchRunError>>;
/// One Carrier B per-monitor reader task's tracked handle (see
/// [`carrier_b_receive_one`]). Likewise owns no child task of its own —
/// every per-monitor consumer task is spawned directly by [`run_carrier_b`].
type CarrierBReaderTaskHandle = AbortOnDropHandle<Result<(), BenchRunError>>;
/// One per-monitor consumer task's tracked handle — shared by both
/// carriers (see [`carrier_receive_consume_one`]), spawned directly by
/// `run_carrier_a`/`run_carrier_b` themselves (never nested inside a
/// reader task), so every consumer handle is reachable, alongside every
/// producer/reader handle, in the same outer scope a failure/timeout path
/// cleans up. See this module's "Structured task ownership" section.
type ConsumerTaskHandle = AbortOnDropHandle<(u16, PerMonitorValidator)>;

// ---------------------------------------------------------------------------
// Carrier runners
// ---------------------------------------------------------------------------

fn monitor_ids_for(monitors: u8) -> Vec<u16> {
    (1..=u16::from(monitors)).collect()
}

/// Sleeps, if necessary, until `epoch + interval * tick` — the deterministic
/// wall-clock target for scheduling tick `tick` under a paced
/// `Workload::Duration` run (see [`tick_pacing_for`]). A `tick` at or past
/// its own target (e.g. the run is running behind schedule) sleeps for
/// `Duration::ZERO`, never blocking backwards.
async fn sleep_until_tick(epoch: Instant, interval: Duration, tick: u64) {
    let tick_index = u32::try_from(tick).unwrap_or(u32::MAX);
    let target = epoch + interval.saturating_mul(tick_index);
    let remaining = target.saturating_duration_since(Instant::now());
    if !remaining.is_zero() {
        tokio::time::sleep(remaining).await;
    }
}

/// Carrier A reader task body: accepts the single multiplexed stream and
/// demuxes each decoded frame, by its `monitor_id`, into that monitor's own
/// bounded consumer channel, then drives the same shared per-monitor
/// consumer stage Carrier B uses (see [`carrier_receive_consume_one`]) —
/// so both carriers' receive pipelines are structurally identical from
/// immediately after transport framing onward: this reader only validates
/// the envelope enough to demux it (an unrecognized `monitor_id` is a hard
/// [`BenchRunError::UnknownMonitorId`]), while the shared consumer stage
/// validates payload/order/identity and applies `receiver_delay` at the
/// same point in the pipeline for both carriers.
///
/// This demux loop itself never sleeps for `receiver_delay` and never
/// captures `receive_elapsed` — it only ever blocks on an ordinary
/// bounded-channel send if a monitor's own consumer is behind — so
/// reading the wire and applying/timing the artificial `receiver_delay`
/// stay entirely within [`carrier_receive_consume_one`], exactly matching
/// Carrier B's per-monitor reader ([`carrier_b_receive_one`]) instead of
/// serializing every monitor's frames behind one shared artificial sleep
/// on the single reader task, as an earlier revision of this module did.
///
/// `senders` — one entry per monitor — is created by [`run_carrier_a`]
/// itself, which also spawns every matching consumer task
/// ([`carrier_receive_consume_one`]) *before* this reader task is ever
/// spawned. This reader therefore owns no child task of its own: on any
/// error here (an unrecognized `monitor_id`, a closed per-monitor channel,
/// a frame-decode failure, or the underlying stream itself erroring),
/// there is no nested handle to abort/join — only this task's own
/// `senders`, which it still owns and drops on every exit path (`Ok` or
/// `Err`), closing every consumer's channel so each one's own `recv()`
/// loop in [`carrier_receive_consume_one`] observes a clean end-of-stream
/// and returns on its own, rather than blocking forever. This lets
/// [`run_carrier_a`] join (or, on a failure path, explicitly abort+join)
/// every consumer handle it owns directly, with no nested-task
/// cancellation race of the kind a reader that owned its own child tasks
/// could otherwise be caught mid-cleanup of if the reader's own task were
/// ever aborted from the outside. See this module's "Structured task
/// ownership" section.
async fn carrier_a_receive_all(
    receiver_connection: Connection,
    senders: HashMap<u16, mpsc::Sender<BenchFrame>>,
) -> Result<(), BenchRunError> {
    let mut recv = receiver_connection
        .accept_uni()
        .await
        .map_err(QuicTransportError::Connection)?;
    while let Some(frame) = read_frame(&mut recv).await? {
        let Some(sender) = senders.get(&frame.monitor_id) else {
            return Err(BenchRunError::UnknownMonitorId(frame.monitor_id));
        };
        sender.send(frame).await.map_err(|_| {
            BenchRunError::TaskJoin(
                "carrier A per-monitor consumer channel closed early".to_owned(),
            )
        })?;
    }
    Ok(())
    // `senders` drops here — on this `Ok(())` return and identically on
    // every early `?` return above — closing every consumer's channel.
}

/// Shared per-monitor consumer stage used by **both** carriers: receives
/// frames demuxed for exactly one monitor over a bounded channel (Carrier
/// A: [`carrier_a_receive_all`]'s demux loop; Carrier B:
/// [`carrier_b_receive_one`]'s reader sub-loop), applies the configured
/// `receiver_delay` (if nonzero), and only *then* captures
/// `receive_elapsed` and calls [`PerMonitorValidator::record`] — which is
/// itself where payload/ordering/identity validation happens, at the same
/// stage for both carriers.
///
/// Capturing `receive_elapsed` *after* the delay (rather than before it)
/// is what makes `receiver_delay` carrier-neutral in the *recorded latency
/// itself*, not merely in overall wall-clock floor: for both carriers,
/// every latency-derived metric (`first_frame_latency_nanos`,
/// `p50`/`p95`/`p99_latency_nanos`) reflects frame-send to
/// post-consume-observation, including the configured delay's cost, at
/// exactly the same point in the pipeline. This runs on each monitor's own
/// independent path, in parallel with every other monitor's consumer task
/// (Carrier A) or per-monitor stream task (Carrier B), so the delay's cost
/// is per-monitor rather than serialized across monitors for either
/// carrier.
async fn carrier_receive_consume_one(
    monitor_id: u16,
    mut receiver: mpsc::Receiver<BenchFrame>,
    receiver_delay: Duration,
    epoch: Instant,
) -> (u16, PerMonitorValidator) {
    let mut validator = PerMonitorValidator::new(monitor_id);
    while let Some(frame) = receiver.recv().await {
        if !receiver_delay.is_zero() {
            tokio::time::sleep(receiver_delay).await;
        }
        let receive_elapsed = epoch.elapsed();
        validator.record(&frame, receive_elapsed);
    }
    (monitor_id, validator)
}

/// Bundled parameters for one Carrier A producer task, kept as a struct so
/// [`carrier_a_produce_one`] stays within Clippy's argument-count guidance.
struct CarrierAProducerParams {
    monitor_id: u16,
    monitor_index: usize,
    tick_count: u64,
    pattern: ActivePattern,
    payload_bytes: usize,
    epoch: Instant,
    /// `Some(BENCH_TICK_INTERVAL)` for a paced `Workload::Duration` run;
    /// `None` for an unpaced `Workload::Frames` run. See
    /// [`tick_pacing_for`].
    paced_tick_interval: Option<Duration>,
    scheduler: Arc<Mutex<WeightedRoundRobinScheduler>>,
    done_producers: Arc<AtomicUsize>,
    /// Signaled after every successful enqueue and once more after this
    /// producer's own completion, so [`carrier_a_drain`] can block instead
    /// of polling while every queue is momentarily empty. See
    /// [`carrier_a_drain`]'s own doc comment for why a plain
    /// `notify_one()`-per-event scheme (no `enable()`-based multi-waiter
    /// guard) is sufficient here: `carrier_a_drain` is this run's only
    /// ever caller of `notify.notified()`.
    scheduler_activity: Arc<Notify>,
    /// Signaled (via `notify_waiters()`) by [`carrier_a_drain`] after
    /// every successful `pop_next()`, so [`carrier_a_enqueue_with_backpressure`]
    /// can wait for a freed queue slot instead of retrying `try_enqueue`
    /// on a fixed sleep/poll interval. Up to `monitors` producer tasks can
    /// be waiting on this one shared `Notify` at once (unlike
    /// `scheduler_activity`, which only ever has `carrier_a_drain` as a
    /// waiter), which is why this uses the `notify_waiters()`/`enable()`
    /// multi-waiter pattern rather than `scheduler_activity`'s
    /// single-waiter `notify_one()` pattern — see
    /// [`carrier_a_enqueue_with_backpressure`]'s doc comment.
    space_available: Arc<Notify>,
}

/// Enqueues `frame` onto `monitor_id`'s bounded scheduler queue, waiting
/// for [`carrier_a_drain`] to free a slot (via `space_available`) instead
/// of retrying `try_enqueue` on a fixed sleep/poll interval whenever the
/// queue is momentarily full.
///
/// Every wait registers interest in `space_available` *before* attempting
/// the enqueue (the pinned-future `enable()` pattern documented on
/// [`tokio::sync::Notify::notified`]), not after a failed attempt: up to
/// `monitors` producer tasks can be waiting on this single shared `Notify`
/// at once (unlike [`carrier_a_drain`]'s `scheduler_activity`, which only
/// ever has one waiter), and [`Notify::notify_waiters`] — unlike
/// `notify_one`, which can store one permit for a future waiter — only
/// wakes waiters *already registered* at the time it is called. Without
/// `enable()`-before-check, a pop between this task's failed `try_enqueue`
/// and its subsequent `notified().await` call would notify no one yet
/// registered, and this task would then wait for a wakeup that already
/// happened — a lost wakeup. Registering first closes that race: any pop
/// (and its `notify_waiters()`) that happens after registration is never
/// missed, even if it happens before this task's own retry check.
///
/// This wait is not given its own separate deadline: it always executes as
/// part of one of `run_carrier_a`'s producer tasks, which that function
/// joins from inside its own outer `tokio::time::timeout(deadline, ..)` —
/// see `run_carrier_a`'s doc comment. If the queue genuinely never frees
/// up before that deadline (for example a stalled drain task), the whole
/// run still surfaces a typed [`BenchRunError::CompletionTimeout`] instead
/// of hanging, exactly as it would for any other stuck step of the run.
async fn carrier_a_enqueue_with_backpressure(
    scheduler: &Arc<Mutex<WeightedRoundRobinScheduler>>,
    monitor_id: u16,
    mut frame: BenchFrame,
    space_available: &Notify,
) -> Result<(), BenchRunError> {
    loop {
        let notified = space_available.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();

        let outcome = {
            let mut guard = scheduler.lock().await;
            guard.try_enqueue(monitor_id, frame)
        };
        match outcome {
            Ok(()) => return Ok(()),
            Err((SchedulerError::QueueFull, returned_frame)) => {
                frame = returned_frame;
                notified.await;
            }
            Err((error, _)) => return Err(BenchRunError::Scheduler(error)),
        }
    }
}

/// Carrier A producer task body: generates one monitor's frames per its
/// duty cycle and enqueues them onto the shared scheduler, waiting (never
/// dropping) via [`carrier_a_enqueue_with_backpressure`] while the
/// monitor's bounded queue is full.
///
/// When `paced_tick_interval` is `Some`, this sleeps until `epoch + tick *
/// interval` before considering each tick, so a `Workload::Duration` run's
/// production phase takes approximately its requested wall-clock duration
/// rather than however long blasting the resolved tick count happens to
/// take. When `None` (`Workload::Frames`), ticks are considered back to
/// back with no artificial pacing.
async fn carrier_a_produce_one(
    params: CarrierAProducerParams,
) -> Result<(u16, u64, u64), BenchRunError> {
    let CarrierAProducerParams {
        monitor_id,
        monitor_index,
        tick_count,
        pattern,
        payload_bytes,
        epoch,
        paced_tick_interval,
        scheduler,
        done_producers,
        scheduler_activity,
        space_available,
    } = params;
    let mut sequence = 0_u64;
    let mut sent_bytes = 0_u64;
    for tick in 0..tick_count {
        if let Some(interval) = paced_tick_interval {
            sleep_until_tick(epoch, interval, tick).await;
        }
        if !produces_at_tick(pattern, monitor_index, tick) {
            continue;
        }
        let payload = deterministic_payload(monitor_id, sequence, payload_bytes);
        let send_nanos = u64::try_from(epoch.elapsed().as_nanos()).unwrap_or(u64::MAX);
        sent_bytes += payload.len() as u64;
        let frame = BenchFrame {
            monitor_id,
            sequence,
            send_nanos,
            payload,
        };
        sequence += 1;
        carrier_a_enqueue_with_backpressure(&scheduler, monitor_id, frame, &space_available)
            .await?;
        // Wake `carrier_a_drain` if it is currently parked waiting for
        // scheduler activity — see `CarrierAProducerParams::scheduler_activity`'s
        // own doc comment.
        scheduler_activity.notify_one();
    }
    if let Some(interval) = paced_tick_interval {
        // The loop above's last processed tick (`tick_count - 1`) only
        // waited until `epoch + interval * (tick_count - 1)` — one full
        // `interval` short of the run's intended total paced span `epoch +
        // interval * tick_count`. This final wait closes that gap so a
        // paced `Workload::Duration` run's production phase spans the full
        // requested wall-clock duration end-to-end, rather than finishing
        // one tick interval early (most visible, as a 100% relative error,
        // at `tick_count == 1`, i.e. `MIN_BENCH_DURATION`).
        sleep_until_tick(epoch, interval, tick_count).await;
    }
    done_producers.fetch_add(1, Ordering::SeqCst);
    // Wake `carrier_a_drain` once more on this producer's own completion:
    // it may currently be parked believing more frames are still coming
    // from this monitor, and this producer's own last enqueue (if any)
    // already happened, and was already notified, strictly before this
    // point — so this call is what lets `carrier_a_drain` notice this was
    // the *last* producer to finish and re-check for overall completion,
    // rather than only ever waking on a fresh enqueue.
    scheduler_activity.notify_one();
    Ok((monitor_id, sequence, sent_bytes))
}

/// Drains the Carrier A scheduler onto the single multiplexed stream until
/// every producer has finished *and* the scheduler is confirmed empty.
///
/// This blocks on `scheduler_activity` (a [`tokio::sync::Notify`]) whenever
/// every queue is momentarily empty and at least one producer is still
/// running, rather than repeatedly polling `pop_next` in a hot loop: each
/// producer calls `scheduler_activity.notify_one()` after every successful
/// enqueue and once more after its own completion (see
/// [`CarrierAProducerParams::scheduler_activity`]), so this task is always
/// woken promptly instead of spinning while it waits — this is the
/// single-consumer `Notify` usage pattern documented on
/// [`tokio::sync::Notify`] itself (the "Unbound multi-producer
/// single-consumer (mpsc) channel" example): because this function is this
/// run's only ever caller of `scheduler_activity.notified()`, a
/// `notify_one()` racing with the check below is always either delivered
/// to an already-registered waiter or stored as a permit the very next
/// `notified().await` call consumes immediately — no `enable()`-based
/// multi-waiter guard is needed, since there is only ever one waiter.
///
/// A single "done producers == total" check right after an empty pop is not
/// quite enough on its own: a producer's queue push and its `done` counter
/// increment are two independent synchronized operations, so an empty pop
/// observed a moment before the last producer's increment becomes visible
/// can still race with that producer's final, not-yet-drained push. The
/// explicit re-check under the scheduler lock immediately below that
/// condition removes that race without needing a second, separate drain
/// pass or any additional polling.
///
/// `space_available` is signaled (`notify_waiters()`) after every
/// successful pop, waking any producer task waiting in
/// [`carrier_a_enqueue_with_backpressure`] for a freed queue slot instead
/// of retrying `try_enqueue` on a fixed sleep/poll interval — see that
/// function's doc comment for the multi-waiter `enable()` reasoning this
/// requires (unlike `scheduler_activity`, which only ever has this
/// function as a waiter).
async fn carrier_a_drain(
    scheduler: &Arc<Mutex<WeightedRoundRobinScheduler>>,
    send_stream: &mut impl FrameSink,
    done_producers: &Arc<AtomicUsize>,
    total_producers: usize,
    scheduler_activity: &Notify,
    space_available: &Notify,
) -> Result<(), BenchRunError> {
    loop {
        let popped = {
            let mut guard = scheduler.lock().await;
            guard.pop_next()
        };
        if let Some(frame) = popped {
            // A slot just freed up: wake any producer task waiting in
            // `carrier_a_enqueue_with_backpressure` for `space_available`
            // — see that function's doc comment for why every wait
            // registers before this call can ever race it.
            space_available.notify_waiters();
            send_stream.write_frame(&frame).await?;
            continue;
        }
        if done_producers.load(Ordering::SeqCst) == total_producers {
            // Re-check once more under the scheduler lock before
            // concluding the run is actually done — see this function's
            // own doc comment for why a single check right after an empty
            // pop is not quite enough on its own.
            let popped_once_more = {
                let mut guard = scheduler.lock().await;
                guard.pop_next()
            };
            match popped_once_more {
                Some(frame) => {
                    space_available.notify_waiters();
                    send_stream.write_frame(&frame).await?;
                    continue;
                }
                None => break,
            }
        }
        // Every queue is empty and at least one producer is still
        // running: block on the shared `Notify` instead of spinning. See
        // this function's own doc comment for why the plain single-waiter
        // `notified().await` pattern (no pre-created future, no
        // `enable()`) is correct here.
        scheduler_activity.notified().await;
    }
    Ok(())
}

/// Runs Carrier A (all monitor frames multiplexed over one reliable
/// stream, interleaved by [`WeightedRoundRobinScheduler`]) once over an
/// already-established connection pair.
///
/// `sender_connection` opens the single stream and produces frames;
/// `receiver_connection` must be the peer side of the same QUIC connection.
/// A `Workload::Duration` config paces production at [`BENCH_TICK_INTERVAL`]
/// (see [`tick_pacing_for`]); a `Workload::Frames` config remains unpaced.
/// The whole run is bounded by its [`predicted_completion_deadline`].
///
/// # Errors
///
/// Returns [`BenchRunError::Config`] if `config` fails validation,
/// [`BenchRunError::CompletionTimeout`] if the run does not finish within
/// its predicted completion deadline, or a transport/frame/task failure if
/// the run itself fails.
// This function's length is inherent to keeping one run's entire
// orchestration (spawn, drain, join, and — on any failure/timeout path —
// explicit cleanup) linear and easy to read top-to-bottom in one place,
// rather than splitting an already-small number of sequential steps across
// several near-single-use helper functions (matching this crate's existing
// precedent for a similarly long, sequential function in
// `shared/media/src/video/plan.rs`'s `parse_ready_v1`).
#[allow(clippy::too_many_lines)]
pub async fn run_carrier_a(
    sender_connection: &Connection,
    receiver_connection: &Connection,
    config: &BenchConfig,
) -> Result<CarrierRunResult, BenchRunError> {
    config.validate()?;
    let tick_count = resolve_tick_count(config.workload);
    let paced_tick_interval = tick_pacing_for(config.workload);
    let deadline = predicted_completion_deadline(config);
    let monitor_ids = monitor_ids_for(config.monitors);
    let weights: Vec<(u16, u32)> = monitor_ids
        .iter()
        .enumerate()
        .map(|(index, &id)| (id, scheduler_weight_for(config.pattern, index)))
        .collect();

    let epoch = Instant::now();
    let scheduler = Arc::new(Mutex::new(WeightedRoundRobinScheduler::new(
        &weights,
        BENCH_SCHEDULER_QUEUE_CAPACITY,
    )));
    let done_producers = Arc::new(AtomicUsize::new(0));
    let total_producers = monitor_ids.len();
    let scheduler_activity = Arc::new(Notify::new());
    let space_available = Arc::new(Notify::new());

    let mut send_stream = sender_connection
        .open_uni()
        .await
        .map_err(QuicTransportError::Connection)?;

    // Every per-monitor consumer task is spawned here, directly by
    // `run_carrier_a` itself — never nested inside the reader task below
    // — so every consumer handle is reachable, alongside every
    // producer/reader handle, in this same outer scope for a
    // failure/timeout path to explicitly abort+join. See this module's
    // "Structured task ownership" section for why a reader that spawned
    // and owned its own child consumer tasks could not give the same
    // unconditional guarantee if the reader's own task were ever aborted
    // from the outside while still mid-cleanup of its own children.
    let mut senders: HashMap<u16, mpsc::Sender<BenchFrame>> =
        HashMap::with_capacity(monitor_ids.len());
    let mut consumer_handles: Vec<ConsumerTaskHandle> = Vec::with_capacity(monitor_ids.len());
    for &monitor_id in &monitor_ids {
        let (sender, receiver) = mpsc::channel(BENCH_RECEIVER_QUEUE_CAPACITY);
        senders.insert(monitor_id, sender);
        consumer_handles.push(spawn_tracked(carrier_receive_consume_one(
            monitor_id,
            receiver,
            config.receiver_delay,
            epoch,
        )));
    }

    // Kept in the outer scope (as a plain `Vec`/`Option`, not moved into
    // `completion` below) specifically so a failure or timeout path can
    // still reach whichever handles `completion` did not finish draining
    // itself — see this module's "Structured task ownership" section.
    let mut receiver_handle = Some(spawn_tracked(carrier_a_receive_all(
        receiver_connection.clone(),
        senders,
    )));

    let mut producer_handles = Vec::with_capacity(monitor_ids.len());
    for (monitor_index, &monitor_id) in monitor_ids.iter().enumerate() {
        producer_handles.push(spawn_tracked(carrier_a_produce_one(
            CarrierAProducerParams {
                monitor_id,
                monitor_index,
                tick_count,
                pattern: config.pattern,
                payload_bytes: config.payload_bytes,
                epoch,
                paced_tick_interval,
                scheduler: Arc::clone(&scheduler),
                done_producers: Arc::clone(&done_producers),
                scheduler_activity: Arc::clone(&scheduler_activity),
                space_available: Arc::clone(&space_available),
            },
        )));
    }

    // Everything from here on — draining the scheduler onto the wire,
    // finishing the stream, and joining every producer/reader/consumer
    // task — is bounded by `deadline` so an unexpected stall (backpressure,
    // a scheduler bug, or a pathological `receiver_delay`) surfaces as a
    // typed `CompletionTimeout` instead of hanging this call indefinitely.
    // This is also what bounds `carrier_a_enqueue_with_backpressure`'s
    // wait for `space_available` — see that function's doc comment.
    //
    // This is a plain `async` block, not `async move`: it borrows
    // `send_stream`, `producer_handles`, `receiver_handle`, and
    // `consumer_handles` so that whatever those still hold at the moment
    // this future either returns an error or is dropped by the outer
    // `tokio::time::timeout` elapsing remains reachable afterward for
    // explicit cleanup — see this module's "Structured task ownership"
    // section above.
    //
    // Crucially, it joins `producer_handles`/`receiver_handle`/
    // `consumer_handles` via [`join_last`]/[`join_taken`], **not**
    // `producer_handles.drain(..)`/`receiver_handle.take()`/a plain
    // `for handle in consumer_handles` loop: those would remove every
    // handle from the outer `Vec`/`Option` up front, before any of them
    // were actually joined, so if this future is dropped mid-loop (an
    // outer timeout) or returns an error partway through (a sibling
    // task's own `?`), the still-unjoined handles would already be gone
    // from their owning `Vec`/`Option` by the time
    // `cleanup_failed_carrier_a_run` below gets to look at them — leaving
    // cleanup nothing to explicitly `abort()` + join for itself, and
    // relying instead on each handle's own unjoined, merely-`abort()`-ed
    // `Drop`. `join_last`/`join_taken` instead only remove a handle once
    // its own join has actually completed, so every handle `completion`
    // has not yet finished joining at the moment it exits (however it
    // exits) is still sitting in `producer_handles`/`receiver_handle`/
    // `consumer_handles`, exactly where `cleanup_failed_carrier_a_run`
    // expects to find it.
    let completion = async {
        carrier_a_drain(
            &scheduler,
            &mut send_stream,
            &done_producers,
            total_producers,
            &scheduler_activity,
            &space_available,
        )
        .await?;
        send_stream
            .finish()
            .map_err(|_| BenchRunError::Transport(QuicTransportError::Closed))?;

        let mut sent_totals: HashMap<u16, (u64, u64)> = HashMap::new();
        while let Some(joined) = join_last(&mut producer_handles).await {
            let (monitor_id, sent_frames, sent_bytes) =
                joined.map_err(|error| BenchRunError::TaskJoin(error.to_string()))??;
            sent_totals.insert(monitor_id, (sent_frames, sent_bytes));
        }
        let Some(joined) = join_taken(&mut receiver_handle).await else {
            // Structurally unreachable: `completion` runs to completion at
            // most once and this is its only successful join of
            // `receiver_handle`, so it is always still `Some` here.
            // Handled as an ordinary error rather than a panic/`expect()`
            // regardless, consistent with this module never panicking on
            // a task's own failure path.
            return Err(BenchRunError::TaskJoin(
                "receiver_handle unexpectedly already taken".to_owned(),
            ));
        };
        joined.map_err(|error| BenchRunError::TaskJoin(error.to_string()))??;

        let mut validators = BTreeMap::new();
        while let Some(joined) = join_last(&mut consumer_handles).await {
            let (monitor_id, validator) =
                joined.map_err(|error| BenchRunError::TaskJoin(error.to_string()))?;
            validators.insert(monitor_id, validator);
        }
        Ok::<_, BenchRunError>((sent_totals, validators))
    };
    let (sent_totals, mut validators) = match tokio::time::timeout(deadline, completion).await {
        Ok(Ok(result)) => result,
        Ok(Err(error)) => {
            cleanup_failed_carrier_a_run(
                sender_connection,
                receiver_connection,
                &mut send_stream,
                producer_handles,
                receiver_handle,
                consumer_handles,
            )
            .await;
            return Err(error);
        }
        Err(_) => {
            cleanup_failed_carrier_a_run(
                sender_connection,
                receiver_connection,
                &mut send_stream,
                producer_handles,
                receiver_handle,
                consumer_handles,
            )
            .await;
            return Err(BenchRunError::CompletionTimeout { deadline });
        }
    };

    let elapsed = epoch.elapsed();
    let mut per_monitor = Vec::with_capacity(monitor_ids.len());
    for &monitor_id in &monitor_ids {
        let (sent_frames, sent_bytes) = sent_totals.get(&monitor_id).copied().unwrap_or((0, 0));
        let Some(validator) = validators.remove(&monitor_id) else {
            return Err(BenchRunError::UnknownMonitorId(monitor_id));
        };
        per_monitor.push(validator.finish(sent_frames, sent_bytes));
    }

    let aggregate = AggregateMetrics::from_per_monitor(&per_monitor, elapsed);
    Ok(CarrierRunResult {
        carrier: CarrierKind::A,
        per_monitor,
        aggregate,
    })
}

/// Cleanup applied on every `run_carrier_a` failure/timeout path (never on
/// success): aborts and joins whichever producer/reader/consumer task
/// handles `completion` had not already fully joined itself, resets the
/// shared send stream, and closes both connections — see
/// [`abort_and_join_all`], [`abort_and_join_one`], and
/// [`close_connections_after_failed_run`] for exactly what each step
/// guarantees, and this module's "Structured task ownership" section for
/// why this is sufficient to reach a confirmed-clean task/connection state
/// before `run_carrier_a` returns its error to its own caller. Every one of
/// these handles is a leaf task with no child of its own, so every
/// `abort()` + `.await` here is a direct, complete teardown of that one
/// task — never a task that could itself still be mid-way through
/// cleaning up further nested children of its own.
async fn cleanup_failed_carrier_a_run(
    sender_connection: &Connection,
    receiver_connection: &Connection,
    send_stream: &mut SendStream,
    producer_handles: Vec<SenderTaskHandle>,
    receiver_handle: Option<CarrierAReaderTaskHandle>,
    consumer_handles: Vec<ConsumerTaskHandle>,
) {
    abort_and_join_all(producer_handles).await;
    if let Some(receiver_handle) = receiver_handle {
        abort_and_join_one(receiver_handle).await;
    }
    abort_and_join_all(consumer_handles).await;
    let _ = send_stream.reset(VarInt::from_u32(BENCH_RUN_FAILURE_ERROR_CODE));
    close_connections_after_failed_run(sender_connection, receiver_connection);
}

/// Runs Carrier B (one reliable unidirectional stream per monitor, opened
/// via the existing [`open_monitor_stream`]/[`accept_monitor_stream`]
/// direct-monitor foundation) once over an already-established connection
/// pair.
///
/// `sender_connection` opens one stream per monitor and produces frames;
/// `receiver_connection` must be the peer side of the same QUIC connection.
/// Receiver tasks dispatch by each stream's parsed
/// [`MonitorStreamIdentity::session_monitor_id`], not by stream-accept
/// order, since QUIC does not guarantee streams are accepted in the order
/// they were opened. A `Workload::Duration` config paces production at
/// [`BENCH_TICK_INTERVAL`] (see [`tick_pacing_for`]); a `Workload::Frames`
/// config remains unpaced. The whole run is bounded by its
/// [`predicted_completion_deadline`].
///
/// # Errors
///
/// Returns [`BenchRunError::Config`] if `config` fails validation,
/// [`BenchRunError::CompletionTimeout`] if the run does not finish within
/// its predicted completion deadline, or a transport/frame/identity/task
/// failure if the run itself fails.
pub async fn run_carrier_b(
    sender_connection: &Connection,
    receiver_connection: &Connection,
    config: &BenchConfig,
) -> Result<CarrierRunResult, BenchRunError> {
    config.validate()?;
    let tick_count = resolve_tick_count(config.workload);
    let paced_tick_interval = tick_pacing_for(config.workload);
    let deadline = predicted_completion_deadline(config);
    let monitor_ids = monitor_ids_for(config.monitors);

    let epoch = Instant::now();

    // Every per-monitor consumer task is spawned here, directly by
    // `run_carrier_b` itself — never nested inside a reader task below —
    // for the same reason `run_carrier_a` does the same; see this
    // module's "Structured task ownership" section. `senders` is a shared
    // map (one entry per monitor) rather than one dedicated channel per
    // reader task: unlike Carrier A's single shared stream, which physical
    // stream (and therefore which reader task below) ends up serving
    // which monitor is only known *after* that reader accepts and parses
    // its own stream's identity (QUIC does not guarantee streams are
    // accepted in the order they were opened) — so every reader task gets
    // its own clone of the full map and looks up only the one entry
    // matching its own accepted stream.
    let mut senders: HashMap<u16, mpsc::Sender<BenchFrame>> =
        HashMap::with_capacity(monitor_ids.len());
    let mut consumer_handles: Vec<ConsumerTaskHandle> = Vec::with_capacity(monitor_ids.len());
    for &monitor_id in &monitor_ids {
        let (sender, receiver) = mpsc::channel(BENCH_RECEIVER_QUEUE_CAPACITY);
        senders.insert(monitor_id, sender);
        consumer_handles.push(spawn_tracked(carrier_receive_consume_one(
            monitor_id,
            receiver,
            config.receiver_delay,
            epoch,
        )));
    }

    // Reader: one task per monitor, each independently accepting the next
    // inbound uni stream and dispatching by the stream's own parsed
    // identity rather than accept order.
    //
    // Kept in the outer scope (as plain `Vec`s, not moved into `completion`
    // below) specifically so a failure or timeout path can still reach
    // whichever handles `completion` did not finish draining itself — see
    // this module's "Structured task ownership" section.
    let mut reader_handles = Vec::with_capacity(monitor_ids.len());
    for _ in &monitor_ids {
        reader_handles.push(spawn_tracked(carrier_b_receive_one(
            receiver_connection.clone(),
            senders.clone(),
        )));
    }
    // Every reader task above holds its own clone of `senders`; this
    // function's own original copy must be dropped once every reader has
    // its clone, or every consumer channel would stay artificially open
    // for `senders`' entire remaining lifetime — which, left un-dropped,
    // would be this function's own stack, for as long as `completion`
    // below is still running: a live-forever cycle, since `completion`
    // itself does not return until every consumer task finishes, and a
    // consumer only finishes once every clone of its own channel's sender
    // (including this one) has dropped.
    drop(senders);

    // Sender: one task per monitor, each opening its own dedicated stream.
    let mut sender_handles = Vec::with_capacity(monitor_ids.len());
    for (monitor_index, &monitor_id) in monitor_ids.iter().enumerate() {
        sender_handles.push(spawn_tracked(carrier_b_send_one(CarrierBSenderParams {
            sender_connection: sender_connection.clone(),
            monitor_id,
            monitor_index,
            tick_count,
            pattern: config.pattern,
            payload_bytes: config.payload_bytes,
            epoch,
            paced_tick_interval,
        })));
    }

    // Bounded by `deadline`: see `run_carrier_a`'s matching comment on why
    // joining these tasks must not be allowed to hang indefinitely. Also a
    // plain `async` block (not `async move`), borrowing `sender_handles`/
    // `reader_handles`/`consumer_handles` and joining them via
    // [`join_last`] rather than `.drain(..)`/a plain `for handle in
    // handles` loop, for the same reason `run_carrier_a`'s `completion`
    // does (see that function's own comment and this module's "Structured
    // task ownership" section): `join_last` only removes a handle from
    // `sender_handles`/`reader_handles`/`consumer_handles` once its own
    // join has actually completed, so any handle still unjoined at the
    // moment this future exits — an early `?` here, or the outer
    // `tokio::time::timeout` elapsing — is still sitting in the same `Vec`
    // `cleanup_failed_carrier_b_run` below receives, instead of having
    // already been removed (and only implicitly, unjoined-ly,
    // `abort()`-ed) by a `drain`/`for` loop.
    let completion = async {
        let mut sent_totals: HashMap<u16, (u64, u64)> = HashMap::new();
        while let Some(joined) = join_last(&mut sender_handles).await {
            let (monitor_id, sent_frames, sent_bytes) =
                joined.map_err(|error| BenchRunError::TaskJoin(error.to_string()))??;
            sent_totals.insert(monitor_id, (sent_frames, sent_bytes));
        }

        while let Some(joined) = join_last(&mut reader_handles).await {
            joined.map_err(|error| BenchRunError::TaskJoin(error.to_string()))??;
        }

        let mut validators: HashMap<u16, PerMonitorValidator> = HashMap::new();
        while let Some(joined) = join_last(&mut consumer_handles).await {
            let (monitor_id, validator) =
                joined.map_err(|error| BenchRunError::TaskJoin(error.to_string()))?;
            validators.insert(monitor_id, validator);
        }
        Ok::<_, BenchRunError>((sent_totals, validators))
    };
    let (sent_totals, mut validators) = match tokio::time::timeout(deadline, completion).await {
        Ok(Ok(result)) => result,
        Ok(Err(error)) => {
            cleanup_failed_carrier_b_run(
                sender_connection,
                receiver_connection,
                sender_handles,
                reader_handles,
                consumer_handles,
            )
            .await;
            return Err(error);
        }
        Err(_) => {
            cleanup_failed_carrier_b_run(
                sender_connection,
                receiver_connection,
                sender_handles,
                reader_handles,
                consumer_handles,
            )
            .await;
            return Err(BenchRunError::CompletionTimeout { deadline });
        }
    };

    let elapsed = epoch.elapsed();
    let mut per_monitor = Vec::with_capacity(monitor_ids.len());
    for &monitor_id in &monitor_ids {
        let (sent_frames, sent_bytes) = sent_totals.get(&monitor_id).copied().unwrap_or((0, 0));
        let Some(validator) = validators.remove(&monitor_id) else {
            return Err(BenchRunError::UnknownMonitorId(monitor_id));
        };
        per_monitor.push(validator.finish(sent_frames, sent_bytes));
    }

    let aggregate = AggregateMetrics::from_per_monitor(&per_monitor, elapsed);
    Ok(CarrierRunResult {
        carrier: CarrierKind::B,
        per_monitor,
        aggregate,
    })
}

/// Cleanup applied on every `run_carrier_b` failure/timeout path (never on
/// success): aborts and joins whichever sender/reader/consumer task
/// handles `completion` had not already fully joined itself, then closes
/// both connections — each per-monitor stream is owned entirely inside its
/// own reader task (unlike Carrier A's single shared send stream), so
/// closing both connections is what resets every one of those streams at
/// once, rather than resetting each stream individually. See
/// [`abort_and_join_all`] and [`close_connections_after_failed_run`] for
/// exactly what each step guarantees, and this module's "Structured task
/// ownership" section for why this is sufficient to reach a
/// confirmed-clean task/connection state before `run_carrier_b` returns
/// its error to its own caller. Every one of these handles is a leaf task
/// with no child of its own, so every `abort()` + `.await` here is a
/// direct, complete teardown of that one task.
async fn cleanup_failed_carrier_b_run(
    sender_connection: &Connection,
    receiver_connection: &Connection,
    sender_handles: Vec<SenderTaskHandle>,
    reader_handles: Vec<CarrierBReaderTaskHandle>,
    consumer_handles: Vec<ConsumerTaskHandle>,
) {
    abort_and_join_all(sender_handles).await;
    abort_and_join_all(reader_handles).await;
    abort_and_join_all(consumer_handles).await;
    close_connections_after_failed_run(sender_connection, receiver_connection);
}

/// Carrier B reader task body: accepts the next inbound uni stream and
/// dispatches by the stream's own parsed
/// [`MonitorStreamIdentity::session_monitor_id`], not by accept order, then
/// forwards each decoded frame into the matching per-monitor channel in
/// `senders` — a shared map, one entry per monitor, created and handed a
/// clone of by [`run_carrier_b`] itself, which also spawns every matching
/// consumer task ([`carrier_receive_consume_one`]) *before* any reader
/// task is spawned, since which physical stream (and therefore which
/// reader task) ends up serving which monitor is only known after this
/// task accepts and parses its own stream's identity. This reader owns no
/// child task of its own — on any error here (an unrecognized identity, a
/// stream-accept timeout, a closed channel, or a frame-decode failure),
/// there is no nested handle to abort/join, only this task's own clone of
/// `senders`, dropped on every exit path so the one consumer this reader
/// was feeding sees a clean end-of-stream rather than blocking forever.
/// See this module's "Structured task ownership" section.
///
/// This reader loop itself never sleeps for `receiver_delay` and never
/// captures `receive_elapsed` — it only ever blocks on an ordinary
/// bounded-channel send if the consumer is behind — exactly matching
/// Carrier A's demux reader ([`carrier_a_receive_all`]).
async fn carrier_b_receive_one(
    receiver_connection: Connection,
    senders: HashMap<u16, mpsc::Sender<BenchFrame>>,
) -> Result<(), BenchRunError> {
    let (mut recv, identity) =
        accept_monitor_stream(&receiver_connection, MONITOR_STREAM_ACCEPT_TIMEOUT).await?;
    let monitor_id = identity.session_monitor_id().get();
    let Some(sender) = senders.get(&monitor_id) else {
        return Err(BenchRunError::UnknownMonitorId(monitor_id));
    };
    while let Some(frame) = read_frame(&mut recv).await? {
        sender.send(frame).await.map_err(|_| {
            BenchRunError::TaskJoin(
                "carrier B per-monitor consumer channel closed early".to_owned(),
            )
        })?;
    }
    Ok(())
    // `senders` (this task's own clone) drops here — on this `Ok(())`
    // return and identically on every early `?` return above — closing
    // the one consumer channel this reader was feeding.
}

/// Parameters for one Carrier B per-monitor sender task (see
/// [`carrier_b_send_one`]).
struct CarrierBSenderParams {
    sender_connection: Connection,
    monitor_id: u16,
    monitor_index: usize,
    tick_count: u64,
    pattern: ActivePattern,
    payload_bytes: usize,
    epoch: Instant,
    /// `Some(BENCH_TICK_INTERVAL)` for a paced `Workload::Duration` run;
    /// `None` for an unpaced `Workload::Frames` run. See
    /// [`tick_pacing_for`].
    paced_tick_interval: Option<Duration>,
}

/// Carrier B sender task body: opens this monitor's own dedicated stream
/// and produces its frames per its duty cycle.
///
/// When `paced_tick_interval` is `Some`, this sleeps until `epoch + tick *
/// interval` before considering each tick, so a `Workload::Duration` run's
/// production phase takes approximately its requested wall-clock duration.
/// When `None` (`Workload::Frames`), ticks are considered back to back
/// with no artificial pacing.
async fn carrier_b_send_one(
    params: CarrierBSenderParams,
) -> Result<(u16, u64, u64), BenchRunError> {
    let CarrierBSenderParams {
        sender_connection,
        monitor_id,
        monitor_index,
        tick_count,
        pattern,
        payload_bytes,
        epoch,
        paced_tick_interval,
    } = params;
    let identity = MonitorStreamIdentity::new(
        "carrier-bench",
        std::num::NonZeroU64::MIN,
        std::num::NonZeroU64::MIN,
        NonZeroU16::new(monitor_id).ok_or(BenchRunError::UnknownMonitorId(monitor_id))?,
        [0_u8; MEDIA_PLAN_FINGERPRINT_BYTES],
    )?;
    let mut send = open_monitor_stream(&sender_connection, &identity).await?;
    let mut sequence = 0_u64;
    let mut sent_bytes = 0_u64;
    for tick in 0..tick_count {
        if let Some(interval) = paced_tick_interval {
            sleep_until_tick(epoch, interval, tick).await;
        }
        if !produces_at_tick(pattern, monitor_index, tick) {
            continue;
        }
        let payload = deterministic_payload(monitor_id, sequence, payload_bytes);
        let send_nanos = u64::try_from(epoch.elapsed().as_nanos()).unwrap_or(u64::MAX);
        sent_bytes += payload.len() as u64;
        let frame = BenchFrame {
            monitor_id,
            sequence,
            send_nanos,
            payload,
        };
        sequence += 1;
        write_frame(&mut send, &frame).await?;
    }
    if let Some(interval) = paced_tick_interval {
        // See the matching comment in `carrier_a_produce_one`: without
        // this final wait, the loop above's last processed tick finishes
        // one full `interval` short of the run's intended total paced
        // span, undershooting the requested wall-clock duration.
        sleep_until_tick(epoch, interval, tick_count).await;
    }
    send.finish()
        .map_err(|_| BenchRunError::Transport(QuicTransportError::Closed))?;
    Ok((monitor_id, sequence, sent_bytes))
}

// ---------------------------------------------------------------------------
// CLI argument parsing (kept in-library so validation is unit-testable
// without a subprocess)
// ---------------------------------------------------------------------------

/// Fail-closed CLI argument rejection reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliArgError {
    /// A required argument was missing.
    MissingArg(&'static str),
    /// An argument's value failed to parse or was not one of its allowed
    /// values.
    InvalidValue {
        /// The argument name (including its leading `--`).
        arg: &'static str,
        /// The raw value that failed to parse.
        value: String,
    },
    /// An unrecognized argument was supplied.
    UnknownArg(String),
    /// A singleton argument (every recognized flag accepts at most one
    /// occurrence) was supplied more than once. The second (or later)
    /// occurrence is rejected outright, before it can silently overwrite
    /// the first — so a duplicated flag is a fail-closed parse error, not
    /// last-value-wins.
    DuplicateArg(&'static str),
    /// Both `--frames` and `--duration` were supplied (exactly one is
    /// required).
    ConflictingWorkload,
    /// Neither `--frames` nor `--duration` was supplied.
    MissingWorkload,
    /// The parsed configuration failed [`BenchConfig::validate`].
    Config(BenchConfigError),
}

impl Display for CliArgError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingArg(arg) => write!(formatter, "missing required argument {arg}"),
            Self::InvalidValue { arg, value } => {
                write!(formatter, "invalid value for {arg}: {value:?}")
            }
            Self::UnknownArg(arg) => write!(formatter, "unknown argument {arg:?}"),
            Self::DuplicateArg(arg) => {
                write!(formatter, "argument {arg} was supplied more than once")
            }
            Self::ConflictingWorkload => {
                formatter.write_str("supply exactly one of --frames or --duration, not both")
            }
            Self::MissingWorkload => {
                formatter.write_str("supply exactly one of --frames or --duration")
            }
            Self::Config(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for CliArgError {}

impl From<BenchConfigError> for CliArgError {
    fn from(error: BenchConfigError) -> Self {
        Self::Config(error)
    }
}

impl FromStr for CarrierSelection {
    type Err = CliArgError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "a" => Ok(Self::Only(CarrierKind::A)),
            "b" => Ok(Self::Only(CarrierKind::B)),
            "both" => Ok(Self::Both),
            _ => Err(CliArgError::InvalidValue {
                arg: "--carrier",
                value: value.to_owned(),
            }),
        }
    }
}

/// Parses a duration argument such as `10s`, `250ms`, or `1500us`. Bare
/// digits (no suffix) are interpreted as whole seconds.
fn parse_duration_arg(value: &str) -> Result<Duration, CliArgError> {
    let invalid = || CliArgError::InvalidValue {
        arg: "--duration",
        value: value.to_owned(),
    };
    let (digits, unit) = value
        .find(|ch: char| !ch.is_ascii_digit())
        .map_or((value, ""), |index| value.split_at(index));
    let magnitude: u64 = digits.parse().map_err(|_| invalid())?;
    match unit {
        "" | "s" => Ok(Duration::from_secs(magnitude)),
        "ms" => Ok(Duration::from_millis(magnitude)),
        "us" => Ok(Duration::from_micros(magnitude)),
        _ => Err(invalid()),
    }
}

/// Rejects a second occurrence of a singleton CLI flag outright, before it
/// can silently overwrite the first occurrence's already-parsed value.
fn reject_duplicate(already_set: bool, flag: &'static str) -> Result<(), CliArgError> {
    if already_set {
        return Err(CliArgError::DuplicateArg(flag));
    }
    Ok(())
}

/// Parses a single CLI value via [`FromStr`], mapping any failure to a
/// [`CliArgError::InvalidValue`] naming `flag`.
fn parse_arg<T: FromStr>(value: &str, flag: &'static str) -> Result<T, CliArgError> {
    value.parse::<T>().map_err(|_| CliArgError::InvalidValue {
        arg: flag,
        value: value.to_owned(),
    })
}

/// Parses CLI-style arguments (excluding the program name) into a validated
/// [`BenchConfig`]. Lives in the library, rather than the example binary,
/// so tests can exercise argument validation directly.
///
/// Recognized flags: `--monitors <2|4>`, exactly one of `--frames <N>`
/// (an exact, deterministic, unpaced tick count) or `--duration <e.g.
/// 10s|250ms>` (wall-clock paced at [`BENCH_TICK_INTERVAL`] — the run's
/// production phase takes approximately this long, spanning the full
/// requested interval end-to-end, not however long blasting its resolved
/// tick count would otherwise take; see [`tick_pacing_for`]),
/// `--payload-bytes <N>`, `--pattern <all-active|one-active-rest-idle>`,
/// `--receiver-delay-ms <N>` (optional, default `0`; a carrier-neutral,
/// per-monitor, post-demux consumer/validation delay applied identically
/// by both carriers — see [`BenchConfig::receiver_delay`]), `--carrier
/// <a|b|both>` (optional, default `both`). Every flag above is a
/// singleton: supplying any of them a second time is a
/// [`CliArgError::DuplicateArg`] parse error, not a silent overwrite.
///
/// # Errors
///
/// Returns [`CliArgError`] for a missing, duplicated, unparsable, or
/// unrecognized argument, or a validation failure from
/// [`BenchConfig::validate`] — including a `--duration`/`--receiver-delay-ms`
/// combination whose own arithmetic would already guarantee an
/// unreasonably long run (see [`MAX_BENCH_RECEIVER_DELAY_FLOOR`]).
pub fn parse_cli_args(args: &[String]) -> Result<BenchConfig, CliArgError> {
    let mut monitors: Option<u8> = None;
    let mut frames: Option<u64> = None;
    let mut duration: Option<Duration> = None;
    let mut payload_bytes: Option<usize> = None;
    let mut pattern: Option<ActivePattern> = None;
    let mut receiver_delay_ms: Option<u64> = None;
    let mut carriers: Option<CarrierSelection> = None;

    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        let mut next_value = || -> Result<&str, CliArgError> {
            index += 1;
            args.get(index)
                .map(String::as_str)
                .ok_or(CliArgError::MissingArg("<value>"))
        };
        match arg {
            "--monitors" => {
                reject_duplicate(monitors.is_some(), "--monitors")?;
                monitors = Some(parse_arg(next_value()?, "--monitors")?);
            }
            "--frames" => {
                reject_duplicate(frames.is_some(), "--frames")?;
                frames = Some(parse_arg(next_value()?, "--frames")?);
            }
            "--duration" => {
                reject_duplicate(duration.is_some(), "--duration")?;
                duration = Some(parse_duration_arg(next_value()?)?);
            }
            "--payload-bytes" => {
                reject_duplicate(payload_bytes.is_some(), "--payload-bytes")?;
                payload_bytes = Some(parse_arg(next_value()?, "--payload-bytes")?);
            }
            "--pattern" => {
                reject_duplicate(pattern.is_some(), "--pattern")?;
                pattern = Some(next_value()?.parse()?);
            }
            "--receiver-delay-ms" => {
                reject_duplicate(receiver_delay_ms.is_some(), "--receiver-delay-ms")?;
                receiver_delay_ms = Some(parse_arg(next_value()?, "--receiver-delay-ms")?);
            }
            "--carrier" => {
                reject_duplicate(carriers.is_some(), "--carrier")?;
                carriers = Some(next_value()?.parse()?);
            }
            other => return Err(CliArgError::UnknownArg(other.to_owned())),
        }
        index += 1;
    }

    let workload = match (frames, duration) {
        (Some(_), Some(_)) => return Err(CliArgError::ConflictingWorkload),
        (Some(frames), None) => Workload::Frames(frames),
        (None, Some(duration)) => Workload::Duration(duration),
        (None, None) => return Err(CliArgError::MissingWorkload),
    };
    let receiver_delay = Duration::from_millis(receiver_delay_ms.unwrap_or(0));

    let config = BenchConfig {
        monitors: monitors.ok_or(CliArgError::MissingArg("--monitors"))?,
        workload,
        payload_bytes: payload_bytes.ok_or(CliArgError::MissingArg("--payload-bytes"))?,
        pattern: pattern.ok_or(CliArgError::MissingArg("--pattern"))?,
        receiver_delay,
        carriers: carriers.unwrap_or(CarrierSelection::Both),
    };
    config.validate()?;
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_config() -> BenchConfig {
        BenchConfig {
            monitors: 2,
            workload: Workload::Frames(10),
            payload_bytes: 64,
            pattern: ActivePattern::AllActive,
            receiver_delay: Duration::ZERO,
            carriers: CarrierSelection::Both,
        }
    }

    // -- BenchConfig validation ---------------------------------------

    #[test]
    fn validate_accepts_two_and_four_monitors() {
        assert!(
            BenchConfig {
                monitors: 2,
                ..base_config()
            }
            .validate()
            .is_ok()
        );
        assert!(
            BenchConfig {
                monitors: 4,
                ..base_config()
            }
            .validate()
            .is_ok()
        );
    }

    #[test]
    fn validate_rejects_other_monitor_counts() {
        for monitors in [0_u8, 1, 3, 5, 8] {
            let result = BenchConfig {
                monitors,
                ..base_config()
            }
            .validate();
            assert_eq!(result, Err(BenchConfigError::InvalidMonitorCount(monitors)));
        }
    }

    #[test]
    fn validate_rejects_frame_count_out_of_range() {
        let low = BenchConfig {
            workload: Workload::Frames(0),
            ..base_config()
        }
        .validate();
        assert_eq!(low, Err(BenchConfigError::FrameCountOutOfRange(0)));

        let high = BenchConfig {
            workload: Workload::Frames(MAX_BENCH_FRAMES + 1),
            ..base_config()
        }
        .validate();
        assert_eq!(
            high,
            Err(BenchConfigError::FrameCountOutOfRange(MAX_BENCH_FRAMES + 1))
        );
    }

    #[test]
    fn validate_rejects_duration_out_of_range() {
        let too_short = BenchConfig {
            workload: Workload::Duration(Duration::ZERO),
            ..base_config()
        }
        .validate();
        assert!(matches!(
            too_short,
            Err(BenchConfigError::DurationOutOfRange(_))
        ));

        // The previous minimum (1ms) is below `BENCH_TICK_INTERVAL` (2ms)
        // and is now rejected: `Workload::Duration` is paced/resolved at
        // tick granularity, so anything shorter than one tick cannot be
        // meaningfully represented.
        let below_one_tick = BenchConfig {
            workload: Workload::Duration(Duration::from_millis(1)),
            ..base_config()
        }
        .validate();
        assert!(matches!(
            below_one_tick,
            Err(BenchConfigError::DurationOutOfRange(_))
        ));

        let too_long = BenchConfig {
            workload: Workload::Duration(MAX_BENCH_DURATION + Duration::from_secs(1)),
            ..base_config()
        }
        .validate();
        assert!(matches!(
            too_long,
            Err(BenchConfigError::DurationOutOfRange(_))
        ));
    }

    #[test]
    fn validate_accepts_a_duration_at_exactly_the_minimum_tick_boundary() {
        // `MIN_BENCH_DURATION` is fixed at `BENCH_TICK_INTERVAL` itself —
        // the smallest representable resolution — and must still be
        // accepted (not just rejected one nanosecond below it).
        assert_eq!(MIN_BENCH_DURATION, BENCH_TICK_INTERVAL);
        let config = BenchConfig {
            workload: Workload::Duration(BENCH_TICK_INTERVAL),
            ..base_config()
        };
        assert_eq!(config.validate(), Ok(()));
        assert_eq!(resolve_tick_count(config.workload), 1);
    }

    #[test]
    fn validate_rejects_payload_bytes_out_of_range() {
        let zero = BenchConfig {
            payload_bytes: 0,
            ..base_config()
        }
        .validate();
        assert_eq!(zero, Err(BenchConfigError::PayloadBytesOutOfRange(0)));

        let oversized = BenchConfig {
            payload_bytes: MAX_BENCH_PAYLOAD_BYTES + 1,
            ..base_config()
        }
        .validate();
        assert_eq!(
            oversized,
            Err(BenchConfigError::PayloadBytesOutOfRange(
                MAX_BENCH_PAYLOAD_BYTES + 1
            ))
        );
    }

    #[test]
    fn validate_rejects_receiver_delay_out_of_range() {
        let result = BenchConfig {
            receiver_delay: MAX_RECEIVER_DELAY + Duration::from_millis(1),
            ..base_config()
        }
        .validate();
        assert!(matches!(
            result,
            Err(BenchConfigError::ReceiverDelayOutOfRange(_))
        ));
    }

    #[test]
    fn validate_rejects_total_bytes_exceeding_cap() {
        let result = BenchConfig {
            monitors: 4,
            workload: Workload::Frames(MAX_BENCH_FRAMES),
            payload_bytes: MAX_BENCH_PAYLOAD_BYTES,
            ..base_config()
        }
        .validate();
        assert!(matches!(
            result,
            Err(BenchConfigError::TotalBytesExceedsCap(_))
        ));
    }

    #[test]
    fn validate_rejects_duration_workload_that_expands_past_total_bytes_cap() {
        // `MAX_BENCH_DURATION` (600s) resolves to 300,000 scheduling ticks
        // at `BENCH_TICK_INTERVAL` (2ms). At 4 monitors and the 8 MiB
        // payload ceiling that is ~9.16 TiB of offered load — this must be
        // rejected up front by `validate`, not merely fail (or silently
        // run) once `resolve_tick_count` is applied inside the run itself.
        // This is a regression test: prior to fixing `validate`, the
        // total-bytes cap was only checked for `Workload::Frames`, so this
        // exact config passed validation despite the multi-TiB expansion.
        let resolved_ticks = resolve_tick_count(Workload::Duration(MAX_BENCH_DURATION));
        assert_eq!(resolved_ticks, 300_000);
        let result = BenchConfig {
            monitors: 4,
            workload: Workload::Duration(MAX_BENCH_DURATION),
            payload_bytes: MAX_BENCH_PAYLOAD_BYTES,
            ..base_config()
        }
        .validate();
        assert!(matches!(
            result,
            Err(BenchConfigError::TotalBytesExceedsCap(_))
        ));
    }

    #[test]
    fn validate_accepts_a_duration_workload_within_the_total_bytes_cap() {
        // A short duration with a modest payload must still validate
        // cleanly: the new duration-aware cap check must not reject
        // legitimate, small configurations.
        let result = BenchConfig {
            monitors: 4,
            workload: Workload::Duration(Duration::from_millis(20)),
            payload_bytes: 4096,
            ..base_config()
        }
        .validate();
        assert_eq!(result, Ok(()));
    }

    #[test]
    fn validate_rejects_a_receiver_delay_floor_that_would_take_days_even_though_every_field_is_in_range()
     {
        // Every individual field is within its own bound (600s <=
        // MAX_BENCH_DURATION, 500ms == MAX_RECEIVER_DELAY, monitors == 4,
        // payload_bytes small enough to stay well under the total-bytes
        // cap) yet the combination's carrier-neutral, per-monitor
        // receiver-delay floor (`max_per_monitor_offered_frame_count` *
        // `receiver_delay`, bounded by the single busiest monitor's own
        // frame count, not `monitors * frames`) is `300,000 ticks * 500ms
        // == 150,000s` ~= 41.7 hours (~1.7 days) — this must still be
        // rejected by the dedicated receiver-delay-floor cap, not silently
        // accepted (or left to actually run for the better part of two
        // days).
        let result = BenchConfig {
            monitors: 4,
            workload: Workload::Duration(MAX_BENCH_DURATION),
            payload_bytes: 64,
            receiver_delay: MAX_RECEIVER_DELAY,
            ..base_config()
        }
        .validate();
        assert!(matches!(
            result,
            Err(BenchConfigError::ReceiverDelayFloorExceedsCap(_))
        ));
    }

    #[test]
    fn validate_rejects_the_pathological_600s_duration_500ms_receiver_delay_config() {
        // The exact pathological configuration called out by the task:
        // 600s duration + 500ms receiver delay. With a small payload this
        // does not trip the total-bytes cap, isolating the receiver-delay
        // floor as the rejection reason, and proving `validate` alone
        // (with no run ever started) rejects it in well under a second.
        let started = Instant::now();
        let result = BenchConfig {
            monitors: 2,
            workload: Workload::Duration(Duration::from_secs(600)),
            payload_bytes: 64,
            receiver_delay: Duration::from_millis(500),
            ..base_config()
        }
        .validate();
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "validate() must reject pathological configs immediately, not after any delay"
        );
        assert!(matches!(
            result,
            Err(BenchConfigError::ReceiverDelayFloorExceedsCap(_))
        ));
    }

    #[test]
    fn validate_accepts_a_receiver_delay_floor_at_exactly_the_cap_boundary() {
        // `MAX_BENCH_RECEIVER_DELAY_FLOOR` itself (60s) must still
        // validate: only floors that *exceed* the cap are rejected. The
        // floor is now `max_per_monitor_offered_frame_count(config) *
        // receiver_delay` (carrier-neutral: both carriers apply
        // `receiver_delay` on an independent, parallel path per monitor),
        // and for either active pattern the busiest single monitor always
        // produces exactly `resolve_tick_count(workload)` frames — so with
        // `monitors = 2` this floor does not depend on the monitor count
        // at all: 120 ticks * 500ms == 60s exactly, regardless of whether
        // that is 2 or 4 monitors.
        let result = BenchConfig {
            monitors: 2,
            workload: Workload::Frames(120),
            payload_bytes: 64,
            receiver_delay: Duration::from_millis(500),
            ..base_config()
        }
        .validate();
        assert_eq!(result, Ok(()));
    }

    #[test]
    fn validate_accepts_a_receiver_delay_floor_at_exactly_the_cap_boundary_regardless_of_monitor_count()
     {
        // Regression for the max-per-monitor floor formula: 4 monitors
        // with the same per-monitor frame count and `receiver_delay`
        // produce the *same* floor (120 ticks * 500ms == 60s) as the
        // 2-monitor case above, proving the floor no longer scales with
        // `monitors` the way the old total-frame-count formula did (which
        // would have doubled this floor to 120s and wrongly rejected it).
        let result = BenchConfig {
            monitors: 4,
            workload: Workload::Frames(120),
            payload_bytes: 64,
            receiver_delay: Duration::from_millis(500),
            ..base_config()
        }
        .validate();
        assert_eq!(result, Ok(()));
    }

    // -- offered_frame_count (authoritative, pattern-expanded offered load) --

    #[test]
    fn offered_frame_count_for_all_active_is_monitors_times_effective_frames() {
        let config = BenchConfig {
            monitors: 4,
            workload: Workload::Frames(1_000),
            pattern: ActivePattern::AllActive,
            ..base_config()
        };
        assert_eq!(offered_frame_count(&config), 4_000);
    }

    #[test]
    fn offered_frame_count_for_one_active_rest_idle_counts_the_active_monitor_every_tick_and_idle_monitors_every_tenth_tick()
     {
        // Hand-verified against `produces_at_tick`'s own duty cycle for a
        // range of effective-frame counts, including exact and non-exact
        // multiples of `IDLE_DUTY_CYCLE_TICKS` (10).
        let cases: [(u64, u8, u64); 6] = [
            // (effective_frames, monitors, expected offered_frame_count)
            (1, 4, 1 + 3 * 1),   // ceil(1/10) == 1
            (9, 4, 9 + 3 * 1),   // ceil(9/10) == 1
            (10, 4, 10 + 3 * 1), // ceil(10/10) == 1
            (11, 4, 11 + 3 * 2), // ceil(11/10) == 2
            (20, 2, 20 + 1 * 2), // ceil(20/10) == 2
            (21, 2, 21 + 1 * 3), // ceil(21/10) == 3
        ];
        for (effective_frames, monitors, expected) in cases {
            let config = BenchConfig {
                monitors,
                workload: Workload::Frames(effective_frames),
                pattern: ActivePattern::OneActiveRestIdle,
                ..base_config()
            };
            assert_eq!(
                offered_frame_count(&config),
                expected,
                "effective_frames={effective_frames} monitors={monitors}"
            );
        }
    }

    #[test]
    fn offered_frame_count_matches_an_exhaustive_per_tick_produces_at_tick_count() {
        // Cross-check `offered_frame_count`'s closed-form formula against
        // literally summing `produces_at_tick` over every monitor and tick,
        // for both patterns and a range of monitor counts/tick counts —
        // the strongest possible proof the closed form exactly mirrors the
        // scheduler's own per-tick decision, not merely an approximation
        // of it.
        for &pattern in &[ActivePattern::AllActive, ActivePattern::OneActiveRestIdle] {
            for &monitors in &[2_u8, 4] {
                for effective_frames in [0_u64, 1, 9, 10, 11, 19, 20, 21, 47] {
                    let config = BenchConfig {
                        monitors,
                        workload: Workload::Frames(effective_frames.max(1)),
                        pattern,
                        ..base_config()
                    };
                    // `Workload::Frames` requires >= MIN_BENCH_FRAMES (1),
                    // so directly exercise the formula against arbitrary
                    // `effective_frames` (including 0) rather than only
                    // ever going through a validated config.
                    let expected: u64 = (0..monitors)
                        .map(|monitor_index| {
                            (0..effective_frames)
                                .filter(|&tick| {
                                    produces_at_tick(pattern, monitor_index.into(), tick)
                                })
                                .count() as u64
                        })
                        .sum();
                    let actual = match pattern {
                        ActivePattern::AllActive => {
                            u64::from(monitors).saturating_mul(effective_frames)
                        }
                        ActivePattern::OneActiveRestIdle => {
                            let idle_monitors = u64::from(monitors).saturating_sub(1);
                            effective_frames
                                + idle_monitors * effective_frames.div_ceil(IDLE_DUTY_CYCLE_TICKS)
                        }
                    };
                    assert_eq!(
                        actual, expected,
                        "pattern={pattern:?} monitors={monitors} effective_frames={effective_frames}"
                    );
                    // Sanity: the config's own resolved `offered_frame_count`
                    // agrees once `effective_frames` is itself a valid,
                    // in-range frame count.
                    if effective_frames >= MIN_BENCH_FRAMES {
                        assert_eq!(offered_frame_count(&config), expected);
                    }
                }
            }
        }
    }

    #[test]
    fn max_per_monitor_offered_frame_count_equals_resolved_tick_count_for_both_patterns() {
        // The busiest single monitor always produces exactly
        // `resolve_tick_count(workload)` frames under either active
        // pattern — every monitor is fully active under `AllActive`, and
        // the one active monitor is fully active under
        // `OneActiveRestIdle` — so this function is deliberately *not*
        // pattern-branching, unlike `offered_frame_count` (a genuine sum
        // across monitors). Exercise both patterns and both allowed
        // monitor counts to pin that invariant down.
        for &pattern in &[ActivePattern::AllActive, ActivePattern::OneActiveRestIdle] {
            for &monitors in &[2_u8, 4] {
                for &workload in &[
                    Workload::Frames(1),
                    Workload::Frames(47),
                    Workload::Duration(Duration::from_secs(1)),
                ] {
                    let config = BenchConfig {
                        monitors,
                        workload,
                        pattern,
                        ..base_config()
                    };
                    assert_eq!(
                        max_per_monitor_offered_frame_count(&config),
                        resolve_tick_count(workload),
                        "pattern={pattern:?} monitors={monitors} workload={workload:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn offered_frame_count_regression_40s_duration_1ms_receiver_delay_is_accepted_for_one_active_rest_idle()
     {
        // Regression for the offered-load over-count bug in the
        // *total-bytes* cap (which is, and remains, keyed to the
        // pattern-aware `offered_frame_count` sum across every monitor):
        // the previous, pattern-naive formula (`monitors *
        // effective_frames`, as if every monitor were fully `AllActive`)
        // would compute `total_effective_frames = 4 * 20,000 = 80,000` for
        // this config. The corrected, pattern-aware `offered_frame_count`
        // instead counts the one active monitor every tick plus each of
        // the 3 idle monitors only once every `IDLE_DUTY_CYCLE_TICKS`th
        // (10th) tick: `20,000 + 3 * ceil(20,000 / 10) == 26,000`.
        //
        // Note the receiver-delay-floor cap is a *separate* check keyed to
        // `max_per_monitor_offered_frame_count` (the busiest single
        // monitor's frame count, `20,000` here — see
        // `offered_frame_count_regression_100s_duration_1ms_receiver_delay_floor_uses_max_per_monitor_not_total`
        // below for a regression dedicated to *that* formula), not this
        // total; both checks independently accept this config.
        let config = BenchConfig {
            monitors: 4,
            workload: Workload::Duration(Duration::from_secs(40)),
            payload_bytes: 64,
            pattern: ActivePattern::OneActiveRestIdle,
            receiver_delay: Duration::from_millis(1),
            ..base_config()
        };
        let effective_frames = resolve_tick_count(config.workload);
        assert_eq!(effective_frames, 20_000);
        let naive_over_count = u64::from(config.monitors).saturating_mul(effective_frames);
        assert_eq!(naive_over_count, 80_000);

        let offered = offered_frame_count(&config);
        assert_eq!(offered, 26_000);

        assert_eq!(config.validate(), Ok(()));
    }

    #[test]
    fn offered_frame_count_regression_100s_duration_1ms_receiver_delay_floor_uses_max_per_monitor_not_total()
     {
        // Regression for the receiver-delay-floor's own switch from a
        // total-across-every-monitor bound to a max-per-monitor bound (see
        // `max_per_monitor_offered_frame_count`'s doc comment): with the
        // *old* total, pattern-aware `offered_frame_count` formula this
        // 4-monitor, one-active-rest-idle, 100s/1ms config would compute
        // `50,000 + 3 * ceil(50,000 / 10) == 65,000` effective frames,
        // giving `receiver_delay_floor = 65,000 * 1ms = 65s`, which
        // exceeds `MAX_BENCH_RECEIVER_DELAY_FLOOR` (60s) and would have
        // wrongly rejected this otherwise perfectly legitimate
        // configuration now that `receiver_delay` is a carrier-neutral,
        // per-monitor, parallel consumer cost rather than a serialized
        // total.
        //
        // The corrected `max_per_monitor_offered_frame_count` instead uses
        // only the busiest monitor's own frame count (`50,000`, the same
        // for either active pattern), giving `receiver_delay_floor =
        // 50,000 * 1ms = 50s`, comfortably under the 60s cap — so this
        // config must be *accepted*.
        let config = BenchConfig {
            monitors: 4,
            workload: Workload::Duration(Duration::from_secs(100)),
            payload_bytes: 64,
            pattern: ActivePattern::OneActiveRestIdle,
            receiver_delay: Duration::from_millis(1),
            ..base_config()
        };
        let effective_frames = resolve_tick_count(config.workload);
        assert_eq!(effective_frames, 50_000);

        let old_total_formula = offered_frame_count(&config);
        assert_eq!(old_total_formula, 65_000);
        assert!(
            Duration::from_millis(1).saturating_mul(65_000_u32) > MAX_BENCH_RECEIVER_DELAY_FLOOR,
            "the old total-across-every-monitor formula must indeed have wrongly \
             exceeded the cap for this config"
        );

        let max_per_monitor = max_per_monitor_offered_frame_count(&config);
        assert_eq!(max_per_monitor, 50_000);
        assert!(
            Duration::from_millis(1).saturating_mul(50_000_u32) <= MAX_BENCH_RECEIVER_DELAY_FLOOR
        );

        assert_eq!(config.validate(), Ok(()));
    }

    #[test]
    fn offered_frame_count_regression_600s_duration_4096_byte_payload_is_accepted_for_one_active_rest_idle()
     {
        // Regression for the same offered-load over-count bug, this time
        // tripping the total-bytes cap rather than the receiver-delay
        // floor. The previous, pattern-naive formula would compute
        // `total_effective_frames = 4 * 300,000 = 1,200,000`, giving
        // `total_bytes = 1,200,000 * 4096 ~= 4.92 GiB`, which exceeds
        // `MAX_BENCH_TOTAL_BYTES` (4 GiB) and would have wrongly rejected
        // this configuration.
        //
        // The corrected formula instead computes
        // `300,000 + 3 * ceil(300,000 / 10) == 390,000`, giving
        // `total_bytes = 390,000 * 4096 ~= 1.597 GiB`, comfortably under
        // the 4 GiB cap — so this config must be *accepted*.
        let config = BenchConfig {
            monitors: 4,
            workload: Workload::Duration(MAX_BENCH_DURATION),
            payload_bytes: 4096,
            pattern: ActivePattern::OneActiveRestIdle,
            receiver_delay: Duration::ZERO,
            ..base_config()
        };
        let effective_frames = resolve_tick_count(config.workload);
        assert_eq!(effective_frames, 300_000);
        let naive_over_count = u64::from(config.monitors).saturating_mul(effective_frames);
        assert_eq!(naive_over_count, 1_200_000);
        assert!(
            naive_over_count.saturating_mul(4096) > MAX_BENCH_TOTAL_BYTES,
            "the naive formula must indeed have wrongly exceeded the cap for this config"
        );

        let offered = offered_frame_count(&config);
        assert_eq!(offered, 390_000);
        assert!(offered.saturating_mul(4096) <= MAX_BENCH_TOTAL_BYTES);

        assert_eq!(config.validate(), Ok(()));
    }

    #[test]
    fn encoded_frame_bytes_includes_the_fixed_header_on_top_of_the_payload() {
        assert_eq!(encoded_frame_bytes(0), BENCH_FRAME_HEADER_BYTES as u64);
        assert_eq!(
            encoded_frame_bytes(64),
            BENCH_FRAME_HEADER_BYTES as u64 + 64
        );
        assert_eq!(
            encoded_frame_bytes(MAX_BENCH_PAYLOAD_BYTES),
            BENCH_FRAME_HEADER_BYTES as u64 + MAX_BENCH_PAYLOAD_BYTES as u64
        );
    }

    #[test]
    fn encoded_frame_bytes_regression_tiny_payload_multi_million_frame_count_gets_a_sufficient_completion_deadline()
     {
        // Regression for the total-bytes cap and completion-deadline
        // transfer budget both switching from `payload_bytes` alone to
        // `encoded_frame_bytes(payload_bytes)` (payload plus the fixed
        // wire header). With a 1-byte payload and the maximum allowed
        // frame count, the fixed 28-byte header is *28 times* the payload
        // itself — the header, not the payload, dominates the true wire
        // byte total here, so a transfer budget computed on payload bytes
        // alone would badly under-estimate the time a real run's transfer
        // needs, risking a false `CompletionTimeout` even though the
        // config itself is entirely legitimate (well within the
        // total-bytes cap either way).
        let config = BenchConfig {
            monitors: 4,
            workload: Workload::Frames(MAX_BENCH_FRAMES),
            payload_bytes: MIN_BENCH_PAYLOAD_BYTES,
            pattern: ActivePattern::AllActive,
            receiver_delay: Duration::ZERO,
            ..base_config()
        };
        // This tiny-payload, multi-million-frame config must be accepted,
        // not rejected — its true wire byte total is nowhere near the cap
        // (see the encoded-vs-payload-only math below).
        assert_eq!(config.validate(), Ok(()));

        let offered = offered_frame_count(&config);
        assert_eq!(offered, 8_000_000);

        // The *old*, payload-bytes-only formula this module used to
        // compute the transfer budget with.
        let old_payload_only_total = offered.saturating_mul(config.payload_bytes as u64);
        assert_eq!(old_payload_only_total, 8_000_000);
        let old_transfer_budget_nanos = old_payload_only_total
            .saturating_mul(1_000_000_000)
            .checked_div(MIN_ASSUMED_TRANSFER_BYTES_PER_SEC)
            .unwrap();
        let old_transfer_budget = Duration::from_nanos(old_transfer_budget_nanos);

        // The corrected, encoded (wire) bytes formula.
        let new_encoded_total = offered.saturating_mul(encoded_frame_bytes(config.payload_bytes));
        assert_eq!(new_encoded_total, 232_000_000);
        assert!(
            new_encoded_total > old_payload_only_total * 28,
            "the fixed header must dominate the wire byte total for a 1-byte payload"
        );

        let deadline = predicted_completion_deadline(&config);
        // The corrected deadline must be meaningfully larger than what the
        // old, payload-only formula would have produced (plus the fixed
        // drain allowance) — proving the fix actually changes the
        // computed budget, not just the cap check, and gives a real run
        // sufficient headroom rather than a false timeout.
        assert!(
            deadline > old_transfer_budget + BENCH_COMPLETION_DRAIN_ALLOWANCE,
            "corrected deadline {deadline:?} should exceed the old \
             payload-only budget {old_transfer_budget:?} + drain allowance"
        );
        // Sanity bound: the corrected deadline should be in the tens of
        // seconds for this configuration (roughly 232 MB / 8 MiB/s ~= 27.7s
        // plus the 5s drain allowance), not still sub-6-second like the
        // old formula's ~0.95s transfer budget + 5s drain would give.
        assert!(deadline > Duration::from_secs(20));
    }

    #[test]
    fn encoded_frame_bytes_regression_a_payload_size_the_old_formula_would_have_wrongly_accepted_is_now_rejected()
     {
        // Regression demonstrating the total-bytes cap itself (not just
        // the completion deadline) now depends on encoded, not payload,
        // bytes: `payload_bytes = 520` at the maximum frame count and
        // monitor count stays under `MAX_BENCH_TOTAL_BYTES` counting
        // payload bytes alone, but exceeds it once the true 28-byte wire
        // header per frame is counted too.
        let config = BenchConfig {
            monitors: 4,
            workload: Workload::Frames(MAX_BENCH_FRAMES),
            payload_bytes: 520,
            pattern: ActivePattern::AllActive,
            receiver_delay: Duration::ZERO,
            ..base_config()
        };
        let offered = offered_frame_count(&config);
        assert_eq!(offered, 8_000_000);

        let old_payload_only_total = offered.saturating_mul(config.payload_bytes as u64);
        assert_eq!(old_payload_only_total, 4_160_000_000);
        assert!(
            old_payload_only_total <= MAX_BENCH_TOTAL_BYTES,
            "the old payload-only formula must indeed have wrongly accepted this config"
        );

        let new_encoded_total = offered.saturating_mul(encoded_frame_bytes(config.payload_bytes));
        assert_eq!(new_encoded_total, 4_384_000_000);
        assert!(new_encoded_total > MAX_BENCH_TOTAL_BYTES);

        assert_eq!(
            config.validate(),
            Err(BenchConfigError::TotalBytesExceedsCap(new_encoded_total))
        );
    }

    #[test]
    fn tick_pacing_for_paces_duration_and_leaves_frames_unpaced() {
        assert_eq!(
            tick_pacing_for(Workload::Duration(Duration::from_secs(1))),
            Some(BENCH_TICK_INTERVAL)
        );
        assert_eq!(tick_pacing_for(Workload::Frames(10)), None);
    }

    #[test]
    fn predicted_completion_deadline_for_an_unpaced_zero_delay_frames_workload_is_transfer_budget_plus_drain()
     {
        let config = BenchConfig {
            monitors: 2,
            workload: Workload::Frames(100),
            payload_bytes: 1024,
            receiver_delay: Duration::ZERO,
            ..base_config()
        };
        let deadline = predicted_completion_deadline(&config);
        // total_bytes = 2 * 100 * 1024 = 204,800 bytes; at the 8 MiB/s
        // assumed floor that's well under a millisecond of transfer
        // budget, so the deadline collapses to essentially just the fixed
        // drain allowance.
        assert!(deadline >= BENCH_COMPLETION_DRAIN_ALLOWANCE);
        assert!(deadline < BENCH_COMPLETION_DRAIN_ALLOWANCE + Duration::from_secs(1));
    }

    #[test]
    fn predicted_completion_deadline_for_a_paced_duration_workload_is_dominated_by_the_requested_duration()
     {
        let config = BenchConfig {
            monitors: 2,
            workload: Workload::Duration(Duration::from_secs(10)),
            payload_bytes: 64,
            receiver_delay: Duration::ZERO,
            ..base_config()
        };
        let deadline = predicted_completion_deadline(&config);
        // production_budget (10s) dominates the small transfer_budget;
        // plus the fixed drain allowance, with zero receiver-delay floor.
        assert_eq!(
            deadline,
            Duration::from_secs(10) + BENCH_COMPLETION_DRAIN_ALLOWANCE
        );
    }

    #[test]
    fn predicted_completion_deadline_adds_the_receiver_delay_floor_on_top_of_the_main_phase() {
        let config = BenchConfig {
            monitors: 2,
            workload: Workload::Frames(10),
            payload_bytes: 64,
            receiver_delay: Duration::from_millis(100),
            ..base_config()
        };
        let deadline = predicted_completion_deadline(&config);
        // receiver_delay_floor = max_per_monitor_offered_frame_count(10
        // frames) * 100ms == 1s (carrier-neutral: `receiver_delay` is
        // applied on an independent, parallel path per monitor by both
        // carriers, so the floor is bounded by the single busiest
        // monitor's own frame count, not `monitors * frames`), additive on
        // top of the tiny (sub-millisecond, 1280-byte) transfer budget and
        // the fixed drain allowance.
        let floor = Duration::from_secs(1) + BENCH_COMPLETION_DRAIN_ALLOWANCE;
        assert!(deadline >= floor);
        assert!(deadline < floor + Duration::from_millis(10));
    }

    #[test]
    fn resolve_tick_count_is_identity_for_frames() {
        assert_eq!(resolve_tick_count(Workload::Frames(42)), 42);
    }

    #[test]
    fn resolve_tick_count_converts_duration_deterministically() {
        let ticks = resolve_tick_count(Workload::Duration(BENCH_TICK_INTERVAL * 10));
        assert_eq!(ticks, 10);
        // `resolve_tick_count` itself is a pure conversion independent of
        // `BenchConfig::validate`'s own bounds, and still clamps a
        // sub-one-tick duration up to at least one tick — even though
        // `MIN_BENCH_DURATION` now rejects such a duration before it could
        // reach this function via the normal validated CLI/config path.
        assert_eq!(
            resolve_tick_count(Workload::Duration(Duration::from_nanos(1))),
            1
        );
    }

    #[test]
    fn produces_at_tick_all_active_is_always_true() {
        for monitor_index in 0..4 {
            for tick in 0..25 {
                assert!(produces_at_tick(
                    ActivePattern::AllActive,
                    monitor_index,
                    tick
                ));
            }
        }
    }

    #[test]
    fn produces_at_tick_one_active_rest_idle_matches_duty_cycle() {
        // Monitor 0 (the "active" monitor) always produces.
        for tick in 0..25 {
            assert!(produces_at_tick(ActivePattern::OneActiveRestIdle, 0, tick));
        }
        // Idle monitors only produce every IDLE_DUTY_CYCLE_TICKS-th tick.
        for tick in 0..30 {
            let expected = tick % IDLE_DUTY_CYCLE_TICKS == 0;
            assert_eq!(
                produces_at_tick(ActivePattern::OneActiveRestIdle, 1, tick),
                expected,
                "tick {tick}"
            );
        }
    }

    // -- Scheduler fairness --------------------------------------------

    #[test]
    fn scheduler_equal_weights_alternate_strictly() {
        let mut scheduler = WeightedRoundRobinScheduler::new(&[(1, 1), (2, 1)], 100);
        for sequence in 0..20 {
            scheduler
                .try_enqueue(
                    1,
                    BenchFrame {
                        monitor_id: 1,
                        sequence,
                        send_nanos: 0,
                        payload: Vec::new(),
                    },
                )
                .expect("enqueue monitor 1");
            scheduler
                .try_enqueue(
                    2,
                    BenchFrame {
                        monitor_id: 2,
                        sequence,
                        send_nanos: 0,
                        payload: Vec::new(),
                    },
                )
                .expect("enqueue monitor 2");
        }
        let mut picks = Vec::new();
        while let Some(frame) = scheduler.pop_next() {
            picks.push(frame.monitor_id);
        }
        let expected: Vec<u16> = (0..20).flat_map(|_| [1, 2]).collect();
        assert_eq!(picks, expected);
    }

    #[test]
    fn scheduler_four_to_one_weight_repeats_hand_verified_sequence() {
        // Smooth weighted round robin over weights [4, 1] repeats the
        // pattern 0,0,1,0,0 every five picks (hand-verified nginx-style
        // SWRR trace): current_weight starts at [0,0]; each pick adds the
        // weight then subtracts the total (5) from the winner.
        //   pick1: cw=[4,1]  -> winner 0 (id 1), cw=[-1,1]
        //   pick2: cw=[3,2]  -> winner 0 (id 1), cw=[-2,2]
        //   pick3: cw=[2,3]  -> winner 1 (id 2), cw=[2,-2]
        //   pick4: cw=[6,-1] -> winner 0 (id 1), cw=[1,-1]
        //   pick5: cw=[5,0]  -> winner 0 (id 1), cw=[0,0]  (back to start)
        let mut scheduler = WeightedRoundRobinScheduler::new(&[(1, 4), (2, 1)], 100);
        // 4 full cycles of the 5-pick pattern require monitor 1 to produce
        // 4-per-cycle (16 frames) and monitor 2 to produce 1-per-cycle (4
        // frames); uneven counts would let one queue drain early and shift
        // the remaining picks, so the two counts must stay in this 4:1
        // ratio for the sequence below to hold exactly.
        for sequence in 0..16 {
            scheduler
                .try_enqueue(
                    1,
                    BenchFrame {
                        monitor_id: 1,
                        sequence,
                        send_nanos: 0,
                        payload: Vec::new(),
                    },
                )
                .expect("enqueue monitor 1");
        }
        for sequence in 0..4 {
            scheduler
                .try_enqueue(
                    2,
                    BenchFrame {
                        monitor_id: 2,
                        sequence,
                        send_nanos: 0,
                        payload: Vec::new(),
                    },
                )
                .expect("enqueue monitor 2");
        }
        let mut picks = Vec::new();
        while let Some(frame) = scheduler.pop_next() {
            picks.push(frame.monitor_id);
        }
        let expected: Vec<u16> = (0..4).flat_map(|_| [1, 1, 2, 1, 1]).collect();
        assert_eq!(picks, expected);
    }

    #[test]
    fn scheduler_try_enqueue_rejects_unknown_monitor() {
        let mut scheduler = WeightedRoundRobinScheduler::new(&[(1, 1)], 4);
        let frame = BenchFrame {
            monitor_id: 99,
            sequence: 0,
            send_nanos: 0,
            payload: Vec::new(),
        };
        let result = scheduler.try_enqueue(99, frame);
        assert!(matches!(result, Err((SchedulerError::UnknownMonitor, _))));
    }

    #[test]
    fn scheduler_try_enqueue_rejects_when_queue_full() {
        let mut scheduler = WeightedRoundRobinScheduler::new(&[(1, 1)], 2);
        for sequence in 0..2 {
            scheduler
                .try_enqueue(
                    1,
                    BenchFrame {
                        monitor_id: 1,
                        sequence,
                        send_nanos: 0,
                        payload: Vec::new(),
                    },
                )
                .expect("enqueue within capacity");
        }
        let overflow = BenchFrame {
            monitor_id: 1,
            sequence: 2,
            send_nanos: 0,
            payload: Vec::new(),
        };
        let result = scheduler.try_enqueue(1, overflow.clone());
        assert_eq!(result, Err((SchedulerError::QueueFull, overflow)));
    }

    #[test]
    fn scheduler_pop_next_returns_none_when_all_queues_empty() {
        let mut scheduler = WeightedRoundRobinScheduler::new(&[(1, 1), (2, 1)], 4);
        assert!(scheduler.pop_next().is_none());
        assert!(scheduler.is_empty());
    }

    #[test]
    fn scheduler_never_starves_a_populated_low_weight_queue_when_high_weight_queue_is_empty() {
        // Regression for a real bug: the high-weight (4) monitor's queue
        // is never populated in this run; only the low-weight (1)
        // monitor's queue has frames. A winner search that considers
        // *every* registered entry (rather than only entries whose queue
        // currently has data) keeps re-selecting the empty high-weight
        // entry and can exhaust its bounded retry budget before ever
        // trying the populated low-weight entry — incorrectly returning
        // `None` even though data is available the entire time. `pop_next`
        // must never do that: it must return a frame every single call
        // here, in strict FIFO order, until the one populated queue is
        // itself drained.
        let mut scheduler = WeightedRoundRobinScheduler::new(&[(1, 4), (2, 1)], 100);
        for sequence in 0..5 {
            scheduler
                .try_enqueue(
                    2,
                    BenchFrame {
                        monitor_id: 2,
                        sequence,
                        send_nanos: 0,
                        payload: Vec::new(),
                    },
                )
                .expect("enqueue monitor 2");
        }
        let mut picks = Vec::new();
        let mut sequences = Vec::new();
        while let Some(frame) = scheduler.pop_next() {
            picks.push(frame.monitor_id);
            sequences.push(frame.sequence);
        }
        assert_eq!(picks, vec![2, 2, 2, 2, 2]);
        assert_eq!(sequences, vec![0, 1, 2, 3, 4]);
        assert!(scheduler.is_empty());
    }

    #[test]
    fn scheduler_recovers_the_high_weight_queue_once_it_gains_data_after_starvation() {
        // Extends the regression above: after the low-weight queue's
        // initial data is drained (with the high-weight queue still
        // empty), newly enqueued frames on *both* queues must still be
        // scheduled — proving the "active queue" restriction in
        // `pop_next` does not permanently exclude a monitor that was
        // merely empty for a while.
        let mut scheduler = WeightedRoundRobinScheduler::new(&[(1, 4), (2, 1)], 100);
        scheduler
            .try_enqueue(
                2,
                BenchFrame {
                    monitor_id: 2,
                    sequence: 0,
                    send_nanos: 0,
                    payload: Vec::new(),
                },
            )
            .expect("enqueue monitor 2");
        assert_eq!(scheduler.pop_next().map(|frame| frame.monitor_id), Some(2));
        assert!(scheduler.pop_next().is_none());
        assert!(scheduler.is_empty());

        for sequence in 0..4 {
            scheduler
                .try_enqueue(
                    1,
                    BenchFrame {
                        monitor_id: 1,
                        sequence,
                        send_nanos: 0,
                        payload: Vec::new(),
                    },
                )
                .expect("enqueue monitor 1");
        }
        let mut picks = Vec::new();
        while let Some(frame) = scheduler.pop_next() {
            picks.push(frame.monitor_id);
        }
        assert_eq!(picks, vec![1, 1, 1, 1]);
    }

    // -- Metrics math ----------------------------------------------------

    #[test]
    fn percentile_matches_hand_computed_nearest_rank_values() {
        let values: Vec<u64> = (1..=100).collect();
        // Nearest-rank: rank = ceil(pct/100 * len), 1-based, index = rank-1.
        // len=100. p50: ceil(0.50*100)=50 -> values[49] = 50.
        assert_eq!(percentile(&values, 50.0), Some(50));
        assert_eq!(percentile(&values, 95.0), Some(95));
        assert_eq!(percentile(&values, 99.0), Some(99));
        assert_eq!(percentile(&values, 0.0), Some(1));
        assert_eq!(percentile(&values, 100.0), Some(100));
    }

    #[test]
    fn percentile_matches_hand_computed_nearest_rank_values_for_small_n() {
        let values: Vec<u64> = vec![10, 20, 30];
        // len=3. p50: ceil(0.50*3)=ceil(1.5)=2 -> values[1] = 20.
        assert_eq!(percentile(&values, 50.0), Some(20));
        // p95: ceil(0.95*3)=ceil(2.85)=3 -> values[2] = 30.
        assert_eq!(percentile(&values, 95.0), Some(30));
        // p0: ceil(0.0*3)=0, clamped to 1 -> values[0] = 10.
        assert_eq!(percentile(&values, 0.0), Some(10));
        // p100: ceil(1.0*3)=3 -> values[2] = 30.
        assert_eq!(percentile(&values, 100.0), Some(30));
    }

    #[test]
    fn percentile_returns_none_for_empty_input() {
        assert_eq!(percentile(&[], 50.0), None);
    }

    #[test]
    fn jains_fairness_index_is_one_for_equal_values() {
        let index = jains_fairness_index(&[100.0, 100.0, 100.0, 100.0]);
        assert!((index - 1.0).abs() < 1e-9, "index={index}");
    }

    #[test]
    fn jains_fairness_index_is_lower_for_skewed_values() {
        // Hand-computed: (100+0)^2 / (2 * (100^2 + 0^2)) = 10000/20000 = 0.5
        let index = jains_fairness_index(&[100.0, 0.0]);
        assert!((index - 0.5).abs() < 1e-9, "index={index}");
    }

    #[test]
    fn jains_fairness_index_is_one_for_empty_or_all_zero() {
        assert!((jains_fairness_index(&[]) - 1.0).abs() < 1e-9);
        assert!((jains_fairness_index(&[0.0, 0.0]) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn per_monitor_validator_tracks_ordering_and_payload_failures() {
        let mut validator = PerMonitorValidator::new(1);
        let good_payload = deterministic_payload(1, 0, 8);
        validator.record(
            &BenchFrame {
                monitor_id: 1,
                sequence: 0,
                send_nanos: 0,
                payload: good_payload,
            },
            Duration::from_millis(1),
        );
        // Out-of-order: expected sequence 1, got 5.
        let out_of_order_payload = deterministic_payload(1, 5, 8);
        validator.record(
            &BenchFrame {
                monitor_id: 1,
                sequence: 5,
                send_nanos: 0,
                payload: out_of_order_payload,
            },
            Duration::from_millis(2),
        );
        // Corrupted payload for the expected next sequence (6).
        validator.record(
            &BenchFrame {
                monitor_id: 1,
                sequence: 6,
                send_nanos: 0,
                payload: vec![0xFF; 8],
            },
            Duration::from_millis(3),
        );
        let metrics = validator.finish(3, 24);
        assert_eq!(metrics.delivered_frames, 3);
        assert_eq!(metrics.ordering_failures, 1);
        assert_eq!(metrics.payload_failures, 1);
        assert_eq!(metrics.monitor_id_mismatches, 0);
        assert_eq!(metrics.completion_failures, 0);
        assert_eq!(metrics.recovery_failures, 0);
    }

    #[test]
    fn aggregate_metrics_completion_failures_reflect_sent_minus_delivered() {
        let validator = PerMonitorValidator::new(1);
        let metrics = validator.finish(10, 0);
        assert_eq!(metrics.completion_failures, 10);
    }

    fn full_delivery_metric(monitor_id: u16, sent_bytes: u64) -> PerMonitorMetrics {
        PerMonitorMetrics {
            monitor_id,
            sent_frames: 1,
            sent_bytes,
            delivered_frames: 1,
            delivered_bytes: sent_bytes,
            elapsed: Duration::from_millis(1),
            throughput_bytes_per_sec: 0.0,
            first_frame_latency_nanos: Some(0),
            p50_latency_nanos: Some(0),
            p95_latency_nanos: Some(0),
            p99_latency_nanos: Some(0),
            max_inter_arrival_gap_nanos: 0,
            ordering_failures: 0,
            payload_failures: 0,
            monitor_id_mismatches: 0,
            completion_failures: 0,
            recovery_failures: 0,
        }
    }

    #[test]
    fn aggregate_fairness_index_is_normalized_against_offered_load_under_one_active_rest_idle() {
        // Mirrors a one-active-rest-idle workload: monitor 1 was offered
        // (and fully delivered) far more bytes than monitors 2/3, which
        // were each offered (and fully delivered) a tiny idle-pattern
        // trickle. Raw delivered-byte Jain's fairness over
        // [1_000_000, 64, 64] would report substantial "unfairness" purely
        // from this intentional workload skew, even though every monitor
        // received 100% of what it was sent.
        let per_monitor = vec![
            full_delivery_metric(1, 1_000_000),
            full_delivery_metric(2, 64),
            full_delivery_metric(3, 64),
        ];
        let aggregate = AggregateMetrics::from_per_monitor(&per_monitor, Duration::from_millis(1));
        assert!(
            (aggregate.fairness_index - 1.0).abs() < 1e-9,
            "fairness_index should be ~1.0 for full delivery regardless of \
             workload volume skew, got {}",
            aggregate.fairness_index
        );
        // The raw volume spread must still show the workload's real
        // imbalance, proving the two metrics are genuinely decoupled.
        assert!(
            aggregate.delivered_bytes_max_min_spread_ratio > 1.0,
            "raw spread should reflect the intentional workload skew"
        );
    }

    #[test]
    fn aggregate_fairness_index_drops_for_a_genuinely_unfair_delivery_outcome() {
        // Same offered load per monitor, but monitor 2 only delivered
        // half of what it was sent — a genuine delivery-outcome
        // unfairness, which the normalized index must still detect.
        let mut partial = full_delivery_metric(2, 1000);
        partial.delivered_bytes = 500;
        let per_monitor = vec![full_delivery_metric(1, 1000), partial];
        let aggregate = AggregateMetrics::from_per_monitor(&per_monitor, Duration::from_millis(1));
        assert!(
            aggregate.fairness_index < 0.99,
            "fairness_index should drop below ~1.0 for a genuinely uneven \
             delivery outcome, got {}",
            aggregate.fairness_index
        );
    }

    #[test]
    fn per_monitor_validator_rejects_and_counts_monitor_id_mismatch() {
        // Validator accepted for monitor 1's stream identity.
        let mut validator = PerMonitorValidator::new(1);
        // A frame that claims to belong to monitor 2 must never be folded
        // into monitor 1's delivered/ordering/latency state, even though
        // it arrived on monitor 1's accepted stream (e.g. spoofed or
        // corrupted envelope) — see finding #2's release-mode check.
        let spoofed_payload = deterministic_payload(2, 0, 8);
        validator.record(
            &BenchFrame {
                monitor_id: 2,
                sequence: 0,
                send_nanos: 0,
                payload: spoofed_payload,
            },
            Duration::from_millis(1),
        );
        // A legitimate frame for monitor 1 afterward is still accepted
        // normally; the mismatch above must not have poisoned state.
        let good_payload = deterministic_payload(1, 0, 8);
        validator.record(
            &BenchFrame {
                monitor_id: 1,
                sequence: 0,
                send_nanos: 0,
                payload: good_payload,
            },
            Duration::from_millis(2),
        );
        let metrics = validator.finish(2, 16);
        assert_eq!(
            metrics.monitor_id_mismatches, 1,
            "the spoofed frame must be counted as a mismatch"
        );
        assert_eq!(
            metrics.delivered_frames, 1,
            "only the legitimate frame counts as delivered"
        );
        assert_eq!(metrics.ordering_failures, 0);
        assert_eq!(metrics.payload_failures, 0);
    }

    #[test]
    fn per_monitor_validator_preserves_true_first_frame_latency_despite_later_lower_latency() {
        let mut validator = PerMonitorValidator::new(1);
        // First frame arrives with a *high* latency (e.g. cold-start
        // handshake overhead).
        validator.record(
            &BenchFrame {
                monitor_id: 1,
                sequence: 0,
                send_nanos: 0,
                payload: deterministic_payload(1, 0, 8),
            },
            Duration::from_millis(500),
        );
        // A later frame arrives with much *lower* latency. Sorting
        // `latencies_nanos` for percentiles would put this smaller value
        // first — `first_frame_latency_nanos` must not be derived from
        // that sorted view.
        validator.record(
            &BenchFrame {
                monitor_id: 1,
                sequence: 1,
                send_nanos: 0,
                payload: deterministic_payload(1, 1, 8),
            },
            Duration::from_millis(1),
        );
        let metrics = validator.finish(2, 16);
        assert_eq!(
            metrics.first_frame_latency_nanos,
            Some(u64::try_from(Duration::from_millis(500).as_nanos()).expect("fits u64")),
            "first_frame_latency_nanos must reflect the chronologically \
             first frame, not the minimum latency after sorting"
        );
        // Sanity: the (sorted) p50 percentile must reflect the smaller
        // value, proving the two are genuinely decoupled.
        assert_eq!(
            metrics.p50_latency_nanos,
            Some(u64::try_from(Duration::from_millis(1).as_nanos()).expect("fits u64"))
        );
    }

    // -- Frame codec -------------------------------------------------------

    #[test]
    fn frame_encode_decode_round_trips() {
        let frame = BenchFrame {
            monitor_id: 3,
            sequence: 7,
            send_nanos: 12345,
            payload: deterministic_payload(3, 7, 32),
        };
        let encoded = frame.encode();
        let decoded = BenchFrame::decode(&encoded).expect("decode round trip");
        assert_eq!(decoded, frame);
    }

    #[test]
    fn frame_decode_rejects_short_header() {
        let result = BenchFrame::decode(&[0_u8; 4]);
        assert_eq!(result, Err(FrameCodecError::Malformed));
    }

    #[test]
    fn frame_decode_rejects_bad_magic() {
        let mut bytes = BenchFrame {
            monitor_id: 1,
            sequence: 0,
            send_nanos: 0,
            payload: vec![1, 2, 3],
        }
        .encode();
        bytes[0] = b'X';
        assert_eq!(BenchFrame::decode(&bytes), Err(FrameCodecError::Malformed));
    }

    #[test]
    fn frame_decode_rejects_bad_version() {
        let mut bytes = BenchFrame {
            monitor_id: 1,
            sequence: 0,
            send_nanos: 0,
            payload: vec![1, 2, 3],
        }
        .encode();
        bytes[4..6].copy_from_slice(&99_u16.to_be_bytes());
        assert_eq!(BenchFrame::decode(&bytes), Err(FrameCodecError::Malformed));
    }

    #[test]
    fn frame_decode_rejects_length_mismatch() {
        let mut bytes = BenchFrame {
            monitor_id: 1,
            sequence: 0,
            send_nanos: 0,
            payload: vec![1, 2, 3],
        }
        .encode();
        bytes.pop();
        assert_eq!(
            BenchFrame::decode(&bytes),
            Err(FrameCodecError::LengthMismatch)
        );
    }

    #[test]
    fn frame_decode_rejects_oversized_payload_declaration() {
        let mut bytes = BenchFrame {
            monitor_id: 1,
            sequence: 0,
            send_nanos: 0,
            payload: vec![1, 2, 3],
        }
        .encode();
        let oversized = u32::try_from(MAX_BENCH_PAYLOAD_BYTES + 1).expect("fits u32");
        bytes[24..28].copy_from_slice(&oversized.to_be_bytes());
        assert_eq!(
            BenchFrame::decode(&bytes),
            Err(FrameCodecError::OversizedPayload)
        );
    }

    #[test]
    fn deterministic_payload_is_stable_across_calls() {
        let first = deterministic_payload(2, 41, 100);
        let second = deterministic_payload(2, 41, 100);
        assert_eq!(first, second);
        assert_eq!(first.len(), 100);
        // Different sequence numbers must produce different payloads.
        let different = deterministic_payload(2, 42, 100);
        assert_ne!(first, different);
    }

    // -- CLI argument parsing --------------------------------------------

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn parse_cli_args_accepts_a_minimal_valid_invocation() {
        let config = parse_cli_args(&args(&[
            "--monitors",
            "2",
            "--frames",
            "10",
            "--payload-bytes",
            "64",
            "--pattern",
            "all-active",
        ]))
        .expect("valid args parse");
        assert_eq!(config.monitors, 2);
        assert_eq!(config.workload, Workload::Frames(10));
        assert_eq!(config.payload_bytes, 64);
        assert_eq!(config.pattern, ActivePattern::AllActive);
        assert_eq!(config.receiver_delay, Duration::ZERO);
        assert_eq!(config.carriers, CarrierSelection::Both);
    }

    #[test]
    fn parse_cli_args_accepts_duration_and_optional_flags() {
        let config = parse_cli_args(&args(&[
            "--monitors",
            "4",
            "--duration",
            "250ms",
            "--payload-bytes",
            "128",
            "--pattern",
            "one-active-rest-idle",
            "--receiver-delay-ms",
            "5",
            "--carrier",
            "a",
        ]))
        .expect("valid args parse");
        assert_eq!(
            config.workload,
            Workload::Duration(Duration::from_millis(250))
        );
        assert_eq!(config.receiver_delay, Duration::from_millis(5));
        assert_eq!(config.carriers, CarrierSelection::Only(CarrierKind::A));
    }

    #[test]
    fn parse_cli_args_rejects_missing_required_arg() {
        let result = parse_cli_args(&args(&[
            "--frames",
            "10",
            "--payload-bytes",
            "64",
            "--pattern",
            "all-active",
        ]));
        assert_eq!(result, Err(CliArgError::MissingArg("--monitors")));
    }

    #[test]
    fn parse_cli_args_rejects_conflicting_workload() {
        let result = parse_cli_args(&args(&[
            "--monitors",
            "2",
            "--frames",
            "10",
            "--duration",
            "1s",
            "--payload-bytes",
            "64",
            "--pattern",
            "all-active",
        ]));
        assert_eq!(result, Err(CliArgError::ConflictingWorkload));
    }

    #[test]
    fn parse_cli_args_rejects_every_repeated_singleton_flag() {
        // Every recognized flag is a singleton: a second occurrence must
        // be rejected outright (fail-closed), not silently overwrite the
        // first. Duplicate detection happens while scanning arguments, so
        // it fires before any later, unrelated check (e.g. the
        // frames/duration mutual-exclusion check, which only runs once the
        // whole argument list has been scanned) — appending a second
        // `--duration` after an already-present `--frames` still reports
        // `DuplicateArg("--duration")`, not `ConflictingWorkload`.
        let base: Vec<&str> = vec![
            "--monitors",
            "2",
            "--frames",
            "10",
            "--payload-bytes",
            "64",
            "--pattern",
            "all-active",
            "--receiver-delay-ms",
            "0",
            "--carrier",
            "both",
        ];
        let duplicated_flags: [(&str, &str); 7] = [
            ("--monitors", "4"),
            ("--frames", "20"),
            ("--duration", "1s"),
            ("--payload-bytes", "128"),
            ("--pattern", "one-active-rest-idle"),
            ("--receiver-delay-ms", "5"),
            ("--carrier", "a"),
        ];
        for (flag, value) in duplicated_flags {
            let mut invocation = base.clone();
            // Every other flag already appears once in `base`, so pushing
            // it a second time creates the duplicate. `--duration` does
            // not appear in `base` at all (it uses `--frames` as its
            // workload flag), so it must be pushed twice here to create
            // its own duplicate occurrence.
            invocation.push(flag);
            invocation.push(value);
            if flag == "--duration" {
                invocation.push(flag);
                invocation.push(value);
            }
            let result = parse_cli_args(&args(&invocation));
            assert_eq!(
                result,
                Err(CliArgError::DuplicateArg(flag)),
                "expected {flag} to be rejected as a duplicate"
            );
        }
    }

    #[test]
    fn parse_cli_args_rejects_missing_workload() {
        let result = parse_cli_args(&args(&[
            "--monitors",
            "2",
            "--payload-bytes",
            "64",
            "--pattern",
            "all-active",
        ]));
        assert_eq!(result, Err(CliArgError::MissingWorkload));
    }

    #[test]
    fn parse_cli_args_rejects_invalid_pattern_value() {
        let result = parse_cli_args(&args(&[
            "--monitors",
            "2",
            "--frames",
            "10",
            "--payload-bytes",
            "64",
            "--pattern",
            "bogus",
        ]));
        assert_eq!(
            result,
            Err(CliArgError::InvalidValue {
                arg: "--pattern",
                value: "bogus".to_owned()
            })
        );
    }

    #[test]
    fn parse_cli_args_rejects_unknown_argument() {
        let result = parse_cli_args(&args(&[
            "--monitors",
            "2",
            "--frames",
            "10",
            "--payload-bytes",
            "64",
            "--pattern",
            "all-active",
            "--bogus",
            "1",
        ]));
        assert_eq!(result, Err(CliArgError::UnknownArg("--bogus".to_owned())));
    }

    #[test]
    fn parse_cli_args_rejects_invalid_monitor_count_via_validate() {
        let result = parse_cli_args(&args(&[
            "--monitors",
            "3",
            "--frames",
            "10",
            "--payload-bytes",
            "64",
            "--pattern",
            "all-active",
        ]));
        assert_eq!(
            result,
            Err(CliArgError::Config(BenchConfigError::InvalidMonitorCount(
                3
            )))
        );
    }

    #[test]
    fn parse_duration_arg_supports_suffixes() {
        assert_eq!(
            parse_duration_arg("10").expect("bare digits"),
            Duration::from_secs(10)
        );
        assert_eq!(
            parse_duration_arg("10s").expect("seconds"),
            Duration::from_secs(10)
        );
        assert_eq!(
            parse_duration_arg("250ms").expect("millis"),
            Duration::from_millis(250)
        );
        assert_eq!(
            parse_duration_arg("500us").expect("micros"),
            Duration::from_micros(500)
        );
        assert!(parse_duration_arg("abc").is_err());
    }

    // -- Carrier A drain: Notify-based suspend, not busy-poll ----------

    #[test]
    fn carrier_a_drain_suspends_on_notify_instead_of_busy_polling_while_idle() {
        // An in-memory `FrameSink` proves `carrier_a_drain`'s own
        // suspend/resume behaviour directly, without needing a real QUIC
        // connection.
        struct VecSink(Vec<BenchFrame>);
        impl FrameSink for VecSink {
            async fn write_frame(&mut self, frame: &BenchFrame) -> Result<(), BenchRunError> {
                self.0.push(frame.clone());
                Ok(())
            }
        }

        // Runs the scenario on its own paused-clock runtime, on a
        // background OS thread, bounded by a genuine (real, un-paused)
        // wall-clock `recv_timeout` on this test thread. Tokio's paused
        // clock only auto-advances past a registered timer once every
        // task on the runtime is truly non-runnable — never while a
        // busy-spin loop keeps some task perpetually runnable. So if
        // `carrier_a_drain` ever regressed back to polling in a hot loop
        // instead of awaiting `Notify`, the producer's 600 *virtual*
        // second sleep below could never resolve: this test would hang,
        // not merely run slowly, which the outer `recv_timeout` turns
        // into a deterministic assertion failure instead of hanging the
        // whole test binary. This is a state/behavioural proof, not a
        // tight CPU-timing gate: correct behaviour resolves in a handful
        // of milliseconds regardless of the simulated 600-second delay,
        // so the generous 60-second real-world bound below exists only
        // to convert a genuine hang into a prompt, deterministic failure
        // on a busy/shared machine, not to time anything precisely.
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .start_paused(true)
                .build()
                .expect("building a paused current-thread runtime must succeed");
            let frames = runtime.block_on(async {
                let scheduler =
                    Arc::new(Mutex::new(WeightedRoundRobinScheduler::new(&[(1, 1)], 4)));
                let done_producers = Arc::new(AtomicUsize::new(0));
                let scheduler_activity = Arc::new(Notify::new());
                let mut sink = VecSink(Vec::new());

                let producer_scheduler = Arc::clone(&scheduler);
                let producer_done = Arc::clone(&done_producers);
                let producer_activity = Arc::clone(&scheduler_activity);
                let producer = tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(600)).await;
                    let frame = BenchFrame {
                        monitor_id: 1,
                        sequence: 0,
                        send_nanos: 0,
                        payload: Vec::new(),
                    };
                    producer_scheduler
                        .lock()
                        .await
                        .try_enqueue(1, frame)
                        .expect("the single registered monitor's queue must accept one frame");
                    producer_activity.notify_one();
                    producer_done.fetch_add(1, Ordering::SeqCst);
                    producer_activity.notify_one();
                });

                carrier_a_drain(
                    &scheduler,
                    &mut sink,
                    &done_producers,
                    1,
                    &scheduler_activity,
                    &Notify::new(),
                )
                .await
                .expect("the drain must complete cleanly once its one producer finishes");
                producer.await.expect("producer task must not panic");
                sink.0
            });
            let _ = result_tx.send(frames);
        });

        let frames = result_rx.recv_timeout(Duration::from_secs(60)).expect(
            "carrier_a_drain must suspend on Notify rather than busy-poll: a real busy-spin \
             regression would prevent the paused clock from ever auto-advancing past the \
             producer's simulated delay, hanging this test instead of completing it",
        );
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].monitor_id, 1);
    }

    // -- Carrier A enqueue backpressure: wake-on-space, not fixed-poll --

    #[test]
    fn carrier_a_enqueue_with_backpressure_waits_on_notify_not_a_fixed_poll_interval() {
        // Mirrors `carrier_a_drain_suspends_on_notify_instead_of_busy_polling_while_idle`
        // above, from the producer side: a capacity-1 queue is kept full
        // by one already-enqueued sentinel frame, so the very next
        // enqueue attempt observes `QueueFull` and must wait on
        // `space_available` for a freed slot. A "drain" task only frees
        // that slot after an 8-*virtual*-hour sleep. Under
        // `start_paused(true)`, Tokio's clock only auto-advances past a
        // registered timer once every task is truly non-runnable, jumping
        // straight to the next one — but unlike the drain test above
        // (whose old busy-spin regression used `yield_now`, which
        // registers *no* timer and so could never let the paused clock
        // advance at all, a guaranteed hang), a fixed-interval
        // `sleep`-based poll *does* register a timer each retry, so a
        // paused clock can still advance through it, just slowly: this
        // regression is "much slower", not "hangs forever". Proving that
        // difference needs the two possible timer counts pushed far
        // enough apart that only a real regression can miss the bound
        // below, not merely CI/scheduling noise. One single 200us-interval
        // retry loop covering this 8-virtual-hour wait would need
        // roughly 8 * 3600 / 0.0002 = 144,000,000 individual timer
        // registrations/wakeups; measured on this crate's own
        // implementation, 200us-interval polling costs roughly 1.6
        // million such hops per real second, so 144,000,000 of them costs
        // on the order of 90 real seconds — comfortably past the 60-second
        // bound below. Correct (`Notify`-based) behaviour has exactly one
        // timer registered in this whole scenario (the drain's), so it
        // resolves in a handful of milliseconds of real time regardless
        // of how long the simulated wait is.
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .start_paused(true)
                .build()
                .expect("building a paused current-thread runtime must succeed");
            runtime.block_on(async {
                let scheduler =
                    Arc::new(Mutex::new(WeightedRoundRobinScheduler::new(&[(1, 1)], 1)));
                let space_available = Arc::new(Notify::new());

                scheduler
                    .lock()
                    .await
                    .try_enqueue(
                        1,
                        BenchFrame {
                            monitor_id: 1,
                            sequence: 0,
                            send_nanos: 0,
                            payload: Vec::new(),
                        },
                    )
                    .expect("filling the single slot must succeed");

                let drain_scheduler = Arc::clone(&scheduler);
                let drain_space = Arc::clone(&space_available);
                let drain = tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(8 * 60 * 60)).await;
                    drain_scheduler
                        .lock()
                        .await
                        .pop_next()
                        .expect("the queue must still hold the sentinel frame");
                    drain_space.notify_waiters();
                });

                let frame = BenchFrame {
                    monitor_id: 1,
                    sequence: 1,
                    send_nanos: 0,
                    payload: Vec::new(),
                };
                carrier_a_enqueue_with_backpressure(&scheduler, 1, frame, &space_available)
                    .await
                    .expect("enqueue with backpressure must succeed once the slot is freed");
                drain.await.expect("drain task must not panic");
            });
            let _ = result_tx.send(());
        });

        result_rx.recv_timeout(Duration::from_secs(60)).expect(
            "carrier_a_enqueue_with_backpressure must wait on Notify rather than retry on a \
             fixed sleep/poll interval: a regression back to fixed-interval polling would \
             register millions of its own timers before the paused clock could ever reach the \
             drain's simulated 8-hour delay, making this test far exceed its real-time bound \
             instead of completing promptly",
        );
    }
}
