// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Exact still image on the macOS Pier (ADR 0016): a thin adapter between
//! the encode loop and the shared refinement runtime.
//!
//! Grading and HDR are captured as `ScreenCaptureKit` `xf44` (ten-bit
//! Y′CbCr 4:4:4, full range, two planes) and, for HDR, re-rendered by the
//! Core Image white stage into a fresh `xf44` surface. That surface is the
//! one `VideoToolbox` encodes, so it is the refinement source: its codes are
//! the encoder's input, and tiles are refined in the Y′CbCr domain
//! ([`arcen_media::refine::ycbcr::YCbCr444Source`]).
//!
//! Damage is exact: the worker compares every encoded surface with its
//! mirror. `ScreenCaptureKit`'s dirty rectangles are never trusted for it.
//! The compare runs on the worker thread while `VideoToolbox` encodes the
//! same surface, and the access unit waits only for the fence that names
//! what changed in it ([`MacRefine::fence_records`]); tiles, compression
//! and the digest never run on the capture or encode thread.

use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TryRecvError, sync_channel};
use std::time::{Duration, Instant};

use arcen_media::refine::RefineSourceKind;
use arcen_media::refine::tile::RefineGrid;
use arcen_media::refine::ycbcr::{PlaneRef, YCbCr444Format, YCbCr444Source};
use arcen_media::{ColorMatrix, ColorRange, TransferCharacteristics};
use arcen_protocol::refine::RefineControlMsg;

use crate::capture::CapturedFrame;

/// Whether this build contains the refinement producer.
pub const COMPILED: bool = cfg!(feature = "lossless-refine");

/// `kCVPixelFormatType_444YpCbCr10BiPlanarFullRange`.
const XF44: u32 = u32::from_be_bytes(*b"xf44");

/// How often the worker offers tiles while no frame arrives.
const PUMP_INTERVAL: Duration = Duration::from_millis(10);
/// Refine-lane records waiting for the session's writer, at most; beyond
/// it the worker stops taking records from the runtime (its token bucket
/// and outstanding-tile bound then hold the rest).
const OUTGOING_LIMIT: usize = 64;
/// Deck refinement controls waiting for the worker, at most.
const CONTROL_CHANNEL: usize = 16;
/// Capacity of the channel from the worker to the session.
pub const TILE_CHANNEL: usize = 32;
/// How often the worker logs its cost.
const STATS_INTERVAL: Duration = Duration::from_secs(10);

/// The Y′CbCr source of a ten-bit capture whose transfer is `transfer`: the
/// matrix the encoder tags (`crate::encode::colour_for_capture`), BT.2020
/// for PQ, BT.709 otherwise, and `xf44`'s full range.
#[must_use]
pub const fn source_kind(transfer: TransferCharacteristics) -> RefineSourceKind {
    RefineSourceKind::CpuYCbCr444 {
        matrix: matrix_for(transfer),
        range: ColorRange::Full,
    }
}

const fn matrix_for(transfer: TransferCharacteristics) -> ColorMatrix {
    match transfer {
        TransferCharacteristics::Pq | TransferCharacteristics::Hlg => ColorMatrix::Bt2020Ncl,
        TransferCharacteristics::Bt709 | TransferCharacteristics::Srgb => ColorMatrix::Bt709,
    }
}

/// The matrix of an encode colour's `CoreVideo` name, if it is one the
/// refinement source can state.
#[must_use]
pub fn matrix_from_core_video(name: &str) -> Option<ColorMatrix> {
    match name {
        "ITU_R_709_2" => Some(ColorMatrix::Bt709),
        "ITU_R_2020" => Some(ColorMatrix::Bt2020Ncl),
        _ => None,
    }
}

/// A frame handed to the worker.
#[derive(Debug)]
struct SendFrame(CapturedFrame);

// SAFETY: as `stream::StagedFrame`: a captured frame owns an `IOSurface`
// (itself `Send`), the `CMSampleBuffer` holding its pool lease and, for HDR,
// the converted buffer. Their retain and release are atomic and carry no
// thread affinity. The worker only reads the surface under a read-only
// lock while the encoder reads the same surface, and drops the frame as
// soon as it is compared, returning the lease.
#[allow(unsafe_code)]
unsafe impl Send for SendFrame {}

#[derive(Debug)]
enum Job {
    /// Compare an encoded surface with the mirror. `recovery` starts a new
    /// generation first (a keyframe the Deck asked for: nothing refined
    /// before it inherits validity). `rate_bps` is the encoder's current
    /// target, which refinement never exceeds.
    Observe {
        frame: SendFrame,
        recovery: bool,
        rate_bps: u64,
    },
    /// The records that must precede the access unit just encoded.
    Fence,
}

/// The encode loop's handle on the refinement worker.
#[derive(Debug)]
pub struct MacRefine {
    jobs: SyncSender<Job>,
    fences: Receiver<Vec<Vec<u8>>>,
    observed: bool,
}

/// What the session keeps: the Deck's refinement controls in, tiles out.
#[derive(Debug)]
pub struct MacRefineSession {
    /// `refine_reset` / `refine_control` from the Deck, bounded: a Deck
    /// that floods them loses the excess (each only restarts or pauses).
    pub controls: SyncSender<RefineControlMsg>,
    /// Refine-lane records (tiles, digests) for the session's writer.
    pub tiles: tokio::sync::mpsc::Receiver<Vec<u8>>,
    /// The link budget's latest rate and hold (ADR 0016 R6) for the worker.
    link_rate: LinkRate,
}

/// The link budget's latest `(rate_bps, hold)`, taken by the worker.
type LinkRate = std::sync::Arc<std::sync::Mutex<Option<(u64, bool)>>>;

impl MacRefineSession {
    /// The session's link budget (ADR 0016 R6): the rate from the link's
    /// residual, or a hold. The worker applies it at its next pass; the
    /// encoder's own target is only the floor before the first one.
    pub fn set_link_rate(&self, bps: u64, hold: bool) {
        *self
            .link_rate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((bps, hold));
    }
}

impl MacRefine {
    /// Starts the worker for a `width`×`height` capture whose encoder is
    /// tagged with `matrix`, refining as `plan` decided.
    ///
    /// # Errors
    ///
    /// When the shared runtime refuses the stream (its plan re-checks the
    /// contract) or the worker cannot start.
    pub fn start(
        plan: arcen_media::refine::RefinePlan,
        width: usize,
        height: usize,
        transfer: TransferCharacteristics,
        primaries: arcen_media::ColorPrimaries,
        matrix: ColorMatrix,
    ) -> Result<(Self, MacRefineSession), String> {
        let arcen_media::refine::RefinePlan::On {
            contract,
            still_ms,
            max_mbps,
        } = plan
        else {
            return Err("refinement is off for this session".to_owned());
        };
        let grid = RefineGrid {
            width: u32::try_from(width).map_err(|_| "capture width")?,
            height: u32::try_from(height).map_err(|_| "capture height")?,
        };
        let source = YCbCr444Source::new(
            grid,
            YCbCr444Format::BIPLANAR_TEN_MSB,
            matrix,
            ColorRange::Full,
        );
        let runtime = arcen_media::refine::runtime::RefineRuntime::start(
            &arcen_media::refine::CapencRefineArgs {
                scope: (arcen_media::refine::PRIMARY_MONITOR, 1, 1),
                still_ms,
                max_mbps,
            },
            arcen_media::refine::runtime::RefineStream {
                depth: contract.depth,
                chroma: contract.chroma,
                transfer,
                primaries,
            },
            source,
        )?;
        let (jobs, job_rx) = sync_channel::<Job>(2);
        let (fence_tx, fences) = sync_channel::<Vec<Vec<u8>>>(1);
        let (controls, control_rx) = sync_channel::<RefineControlMsg>(CONTROL_CHANNEL);
        let (tile_tx, tiles) = tokio::sync::mpsc::channel::<Vec<u8>>(TILE_CHANNEL);
        let link_rate = LinkRate::default();
        let worker_link_rate = std::sync::Arc::clone(&link_rate);
        std::thread::Builder::new()
            .name("arcen-refine".into())
            .spawn(move || {
                Worker {
                    runtime,
                    grid,
                    fences: fence_tx,
                    controls: control_rx,
                    tiles: tile_tx,
                    outgoing: std::collections::VecDeque::new(),
                    rate_bps: 0,
                    link_rate: worker_link_rate,
                    untrusted: false,
                    cost: Cost::default(),
                }
                .run(&job_rx);
            })
            .map_err(|error| format!("refine worker: {error}"))?;
        tracing::info!(
            target: arcen_telemetry::names::target::MEDIA,
            width,
            height,
            matrix = ?matrix,
            still_ms,
            max_mbps,
            "exact still image runs on the encoded xf44 surface (Y′CbCr 4:4:4)"
        );
        Ok((
            Self {
                jobs,
                fences,
                observed: false,
            },
            MacRefineSession {
                controls,
                tiles,
                link_rate,
            },
        ))
    }

    /// Hands the surface about to be encoded to the worker, before the
    /// encode, so the compare runs while `VideoToolbox` encodes.
    ///
    /// # Errors
    ///
    /// When the worker has stopped: the stream must end, because access
    /// units without fences could leave exact tiles over changed pixels.
    pub fn observe(
        &mut self,
        frame: &CapturedFrame,
        recovery: bool,
        rate_bps: u64,
    ) -> Result<(), String> {
        self.jobs
            .send(Job::Observe {
                frame: SendFrame(frame.clone()),
                recovery,
                rate_bps,
            })
            .map_err(|_| "exact still image worker stopped".to_owned())?;
        self.observed = true;
        Ok(())
    }

    /// The records that must precede the access unit just encoded from the
    /// observed surface; waits for the worker's compare of it.
    ///
    /// # Errors
    ///
    /// As [`Self::observe`].
    pub fn fence_records(&mut self) -> Result<Vec<Vec<u8>>, String> {
        if !std::mem::take(&mut self.observed) {
            return Ok(Vec::new());
        }
        self.jobs
            .send(Job::Fence)
            .map_err(|_| "exact still image worker stopped".to_owned())?;
        self.fences
            .recv()
            .map_err(|_| "exact still image worker stopped".to_owned())
    }

    /// The observed surface produced no access unit: its damage goes with
    /// the next one.
    pub fn skip_fence(&mut self) {
        self.observed = false;
    }
}

#[derive(Default)]
struct Cost {
    frames: u64,
    compare: Duration,
    worst: Duration,
    damaged_blocks: u64,
    since: Option<Instant>,
}

struct Worker {
    runtime: arcen_media::refine::runtime::RefineRuntime<YCbCr444Source>,
    grid: RefineGrid,
    fences: SyncSender<Vec<Vec<u8>>>,
    controls: Receiver<RefineControlMsg>,
    tiles: tokio::sync::mpsc::Sender<Vec<u8>>,
    outgoing: std::collections::VecDeque<Vec<u8>>,
    rate_bps: u64,
    link_rate: LinkRate,
    /// A surface this source cannot read exactly was encoded: tiles stop
    /// (fences continue, and every block was invalidated).
    untrusted: bool,
    cost: Cost,
}

impl Worker {
    fn run(mut self, jobs: &Receiver<Job>) {
        loop {
            match jobs.recv_timeout(PUMP_INTERVAL) {
                Ok(Job::Observe {
                    frame,
                    recovery,
                    rate_bps,
                }) => {
                    if recovery {
                        self.runtime.recovery();
                    }
                    if rate_bps != 0 && rate_bps != self.rate_bps {
                        self.rate_bps = rate_bps;
                        self.runtime.cap_rate(rate_bps);
                    }
                    self.observe(&frame.0);
                }
                Ok(Job::Fence) => {
                    if self.fences.send(self.runtime.take_video_records()).is_err() {
                        return;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            self.apply_controls();
            if !self.pump() {
                return;
            }
            self.log_cost();
        }
    }

    fn apply_controls(&mut self) {
        let link = self
            .link_rate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some((bps, hold)) = link {
            self.runtime.set_link_rate(bps, hold);
        }
        loop {
            match self.controls.try_recv() {
                Ok(RefineControlMsg::RefineReset { .. }) => self.runtime.reset_request(),
                Ok(RefineControlMsg::RefineControl { enabled, .. }) => {
                    self.runtime.set_enabled(enabled && !self.untrusted);
                }
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return,
            }
        }
    }

    /// Compares one encoded surface with the mirror and schedules exactly
    /// the blocks whose codes changed.
    fn observe(&mut self, frame: &CapturedFrame) {
        let started = Instant::now();
        let damaged = read_planes(frame, self.grid, |planes| {
            self.runtime.source_mut().observe_whole_planes(planes)
        });
        let Some(damaged) = damaged else {
            // Not the `xf44` surface of this grid: nothing can be said about
            // which blocks changed, so all of them did, and tiles stop.
            if !self.untrusted {
                self.untrusted = true;
                self.runtime.set_enabled(false);
                tracing::warn!(
                    target: arcen_telemetry::names::target::MEDIA,
                    pixel_format = frame.pixel_format,
                    width = frame.width,
                    height = frame.height,
                    "exact still image stopped: the encoded surface is not the refined xf44 picture"
                );
            }
            self.runtime
                .observe_blocks((0..self.grid.block_count() as usize).collect());
            return;
        };
        let took = started.elapsed();
        self.cost.frames += 1;
        self.cost.compare += took;
        self.cost.worst = self.cost.worst.max(took);
        self.cost.damaged_blocks += damaged.len() as u64;
        self.runtime.observe_blocks(damaged);
    }

    /// Offers the refine-lane records the runtime admits now to the
    /// session; returns `false` once the session is gone.
    fn pump(&mut self) -> bool {
        if self.outgoing.len() < OUTGOING_LIMIT {
            self.outgoing.extend(self.runtime.pump_records());
        }
        while let Some(record) = self.outgoing.pop_front() {
            match self.tiles.try_send(record) {
                Ok(()) => {}
                Err(tokio::sync::mpsc::error::TrySendError::Full(record)) => {
                    self.outgoing.push_front(record);
                    break;
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => return false,
            }
        }
        true
    }

    fn log_cost(&mut self) {
        let since = *self.cost.since.get_or_insert_with(Instant::now);
        if since.elapsed() < STATS_INTERVAL {
            return;
        }
        let frames = u32::try_from(self.cost.frames.max(1)).unwrap_or(u32::MAX);
        tracing::info!(
            target: arcen_telemetry::names::target::MEDIA,
            frames_compared = self.cost.frames,
            compare_mean_ms = self.cost.compare.as_secs_f64() * 1000.0 / f64::from(frames),
            compare_max_ms = self.cost.worst.as_secs_f64() * 1000.0,
            damaged_blocks = self.cost.damaged_blocks,
            untrusted = self.untrusted,
            fields = %self.runtime.log_fields(),
            "{}",
            arcen_telemetry::names::refine::WINDOW
        );
        self.cost = Cost {
            since: Some(Instant::now()),
            ..Cost::default()
        };
    }
}

/// Runs `with` over the two planes of `frame` locked for reading, if the
/// frame is an `xf44` surface of exactly `grid`'s size.
fn read_planes<T>(
    frame: &CapturedFrame,
    grid: RefineGrid,
    with: impl FnOnce(&[PlaneRef<'_>]) -> T,
) -> Option<T> {
    let surface = &frame.surface;
    if frame.pixel_format != XF44
        || surface.plane_count() != 2
        || surface.width_of_plane(0) != grid.width as usize
        || surface.height_of_plane(0) != grid.height as usize
        || surface.width_of_plane(1) != grid.width as usize
        || surface.height_of_plane(1) != grid.height as usize
    {
        return None;
    }
    let guard = surface.lock_read_only().ok()?;
    let mut planes = Vec::with_capacity(2);
    for (plane, pixel_bytes) in [(0, 2), (1, 4)] {
        let base = guard.base_address_of_plane(plane)?;
        let stride = surface.bytes_per_row_of_plane(plane);
        let height = surface.height_of_plane(plane);
        if base.is_null() || stride < grid.width as usize * pixel_bytes {
            return None;
        }
        // SAFETY: the plane is `height` rows of `stride` bytes, locked for
        // reading until `guard` drops, which outlives `planes`.
        #[allow(unsafe_code)]
        let bytes = unsafe { std::slice::from_raw_parts(base, stride * height) };
        planes.push(PlaneRef { bytes, stride });
    }
    let result = with(&planes);
    drop(planes);
    drop(guard);
    Some(result)
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn the_source_states_the_matrix_the_encoder_tags() {
        // `encode::colour_for_capture`: SDR wide captures are BT.709, HDR
        // is BT.2020; the refinement source must say the same.
        assert_eq!(
            source_kind(TransferCharacteristics::Bt709),
            RefineSourceKind::CpuYCbCr444 {
                matrix: ColorMatrix::Bt709,
                range: ColorRange::Full
            }
        );
        assert_eq!(
            source_kind(TransferCharacteristics::Pq),
            RefineSourceKind::CpuYCbCr444 {
                matrix: ColorMatrix::Bt2020Ncl,
                range: ColorRange::Full
            }
        );
        for capture in [
            crate::capture::CaptureConfig::grading(1, 64, 32, 30),
            crate::capture::CaptureConfig::hdr(1, 64, 32, 30),
        ] {
            let colour = crate::encode::colour_for_capture(&capture).expect("tagged");
            let transfer = if colour.transfer == "SMPTE_ST_2084_PQ" {
                TransferCharacteristics::Pq
            } else {
                TransferCharacteristics::Bt709
            };
            assert_eq!(
                matrix_from_core_video(colour.matrix),
                Some(matrix_for(transfer))
            );
        }
        assert_eq!(XF44, 0x7866_3434);
    }
}
