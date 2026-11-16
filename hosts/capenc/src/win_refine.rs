// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Exact still image on the Windows FP16 NVENC path (ADR 0016): the thin
//! adapter between the capture loop and the shared runtime. Everything it
//! decides with is shared (`arcen_media::refine::scrgb`); this file only
//! chooses how the FP16 pixels reach the source.
//!
//! - **CPU conversion path**: the staging texture is mapped for every
//!   capture anyway; the source observes those bytes before the frame is
//!   encoded (exact compare-and-copy damage, synchronous).
//! - **GPU conversion path with WGC dirty regions**: the regions invalidate
//!   blocks with the access unit that carries them; the pixels arrive later
//!   through one bounded, non-blocking readback at a time (at most every
//!   [`READBACK_SPACING`]), the source is stale until it lands, and
//!   [`DamageWitness`] checks every readback against the regions. A change
//!   the regions missed switches the session to the synchronous feed and
//!   starts a new generation.
//! - **GPU conversion path without trustworthy regions**: every new capture
//!   is read back and mapped before it is encoded (exact, synchronous; costs
//!   one copy and one map per new frame, logged as `refine_sync_ms`).

use std::time::{Duration, Instant};

use arcen_media::refine::runtime::{RefineRuntime, RefineStream};
use arcen_media::refine::scrgb::{DamageWitness, ScrgbHalfSource, WitnessVerdict};
use arcen_media::refine::tile::RefineGrid;
use arcen_media::refine::CapencRefineArgs;

use crate::log;
use crate::nvenc::{ConversionBackend, Encoder, RefineReadbackPoll};

/// Shortest interval between two readbacks on the GPU path: bounds the
/// readback cost of a moving picture to ten frames a second. Blocks are
/// refined only after 300 ms of stillness, so the mirror is always current
/// by then.
pub(crate) const READBACK_SPACING: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Feed {
    CpuStaging,
    GpuDirtyRegions,
    GpuSynchronous,
}

impl Feed {
    const fn token(self) -> &'static str {
        match self {
            Self::CpuStaging => "cpu-staging",
            Self::GpuDirtyRegions => "gpu-readback-dirty-regions",
            Self::GpuSynchronous => "gpu-readback-sync",
        }
    }
}

#[derive(Debug, Default)]
struct Window {
    observes: u64,
    observe_ms_sum: f64,
    observe_ms_max: f64,
    readbacks: u64,
    readback_lag_ms_sum: f64,
    readback_lag_ms_max: f64,
}

/// The refinement of one Windows FP16 capture.
pub(crate) struct WinRefine {
    pub(crate) runtime: RefineRuntime<ScrgbHalfSource>,
    feed: Feed,
    witness: DamageWitness,
    last_started: Option<Instant>,
    window: Window,
}

impl WinRefine {
    /// Starts refinement over `encoder`'s FP16 capture of `width × height`.
    /// `dirty_regions` says whether the capture reports WGC dirty regions.
    ///
    /// # Safety
    ///
    /// D3D11 calls on the encoder's device.
    pub(crate) unsafe fn start(
        args: &CapencRefineArgs,
        color: crate::ColorSpec,
        encoder: &mut Encoder,
        width: u32,
        height: u32,
        dirty_regions: bool,
    ) -> Result<Self, String> {
        let codes = encoder
            .refine_codes()
            .ok_or_else(|| "refinement needs the FP16 capture".to_string())?;
        let feed = match encoder.conversion_backend() {
            ConversionBackend::Cpu => Feed::CpuStaging,
            ConversionBackend::Gpu => {
                encoder.enable_refine_readback()?;
                if dirty_regions {
                    Feed::GpuDirtyRegions
                } else {
                    Feed::GpuSynchronous
                }
            }
        };
        let grid = RefineGrid { width, height };
        let runtime = RefineRuntime::start(
            args,
            RefineStream {
                depth: color.bit_depth,
                chroma: color.chroma,
                transfer: color.transfer,
                primaries: color.primaries,
            },
            ScrgbHalfSource::new(grid, codes),
        )?;
        log(&format!(
            "lossless refinement feed: {} (codes: {:?})",
            feed.token(),
            codes
        ));
        Ok(Self {
            runtime,
            feed,
            witness: DamageWitness::new(grid),
            last_started: None,
            window: Window::default(),
        })
    }

    /// Whether the capture loop must collect dirty regions for refinement.
    pub(crate) fn needs_dirty_regions(&self) -> bool {
        self.feed == Feed::GpuDirtyRegions
    }

    /// The CPU path's mapped FP16 frame, before it is encoded.
    pub(crate) fn observe_mapped(&mut self, bytes: &[u8], pitch: usize) {
        let started = Instant::now();
        let _ = self.runtime.observe_frame(bytes, pitch);
        self.note_observe(started);
    }

    /// The GPU path captured a new frame; `dirty` are its dirty-region
    /// blocks when the feed uses them.
    ///
    /// # Safety
    ///
    /// D3D11 calls on the encoder's device.
    pub(crate) unsafe fn new_gpu_frame(
        &mut self,
        encoder: &mut Encoder,
        dirty: impl Iterator<Item = usize>,
    ) -> Result<(), String> {
        match self.feed {
            Feed::CpuStaging => Ok(()),
            Feed::GpuDirtyRegions => {
                let blocks: Vec<usize> = dirty.collect();
                self.witness.note_reported(blocks.iter().copied());
                self.runtime.observe_blocks(blocks);
                self.runtime.source_mut().set_stale(true);
                Ok(())
            }
            Feed::GpuSynchronous => self.read_back_now(encoder),
        }
    }

    /// Advances the GPU path's bounded readback; never waits (except the
    /// one synchronous readback that follows untrusted dirty regions).
    /// Returns whether invalidations are waiting for an access unit that
    /// no capture will cause (a missed change on a still desktop): the
    /// loop must submit one now, never at the next keepalive.
    ///
    /// # Safety
    ///
    /// D3D11 calls on the encoder's device.
    pub(crate) unsafe fn poll(&mut self, encoder: &mut Encoder) -> Result<bool, String> {
        if self.feed != Feed::GpuDirtyRegions {
            return Ok(false);
        }
        let may_start = self
            .last_started
            .is_none_or(|at| at.elapsed() >= READBACK_SPACING);
        let mut landed = None;
        let runtime = &mut self.runtime;
        let observe_started = Instant::now();
        let poll = encoder.poll_refine_readback(may_start, false, &mut |bytes, pitch| {
            landed = Some(runtime.observe_frame(bytes, pitch));
        })?;
        match poll {
            RefineReadbackPoll::Started => {
                self.witness.begin_readback();
                self.last_started = Some(Instant::now());
            }
            RefineReadbackPoll::Landed => {
                self.note_observe(observe_started);
                if let Some(started) = self.last_started {
                    let lag = started.elapsed().as_secs_f64() * 1000.0;
                    self.window.readbacks += 1;
                    self.window.readback_lag_ms_sum += lag;
                    self.window.readback_lag_ms_max = self.window.readback_lag_ms_max.max(lag);
                }
                let damage = landed.unwrap_or_default();
                match self.witness.finish_readback(&damage) {
                    WitnessVerdict::Trusted | WitnessVerdict::First => {
                        // Still stale when a newer capture arrived meanwhile.
                        let behind = encoder.refine_readback_behind();
                        self.runtime.source_mut().set_stale(behind);
                    }
                    WitnessVerdict::Untrusted { unreported } => {
                        log(&format!(
                            "lossless refinement: WGC dirty regions missed {unreported} changed \
                             block(s); switching to the synchronous readback and starting over"
                        ));
                        self.feed = Feed::GpuSynchronous;
                        self.runtime.reset_request();
                        // The newest capture now, not at the next one: a
                        // still desktop delivers none.
                        self.read_back_now(encoder)?;
                        return Ok(true);
                    }
                }
            }
            RefineReadbackPoll::Idle | RefineReadbackPoll::Pending => {}
        }
        Ok(false)
    }

    /// Reads the newest capture back and maps it before anything else
    /// happens (the synchronous feed).
    unsafe fn read_back_now(&mut self, encoder: &mut Encoder) -> Result<(), String> {
        let started = Instant::now();
        // Land a copy a previous feed left in flight, then the newest.
        for _ in 0..2 {
            let runtime = &mut self.runtime;
            let poll = encoder.poll_refine_readback(true, true, &mut |bytes, pitch| {
                let _ = runtime.observe_frame(bytes, pitch);
            })?;
            if poll != RefineReadbackPoll::Landed || !encoder.refine_readback_behind() {
                break;
            }
        }
        self.runtime.source_mut().set_stale(false);
        self.note_observe(started);
        Ok(())
    }

    fn note_observe(&mut self, started: Instant) {
        let ms = started.elapsed().as_secs_f64() * 1000.0;
        self.window.observes += 1;
        self.window.observe_ms_sum += ms;
        self.window.observe_ms_max = self.window.observe_ms_max.max(ms);
    }

    /// Per-second log fields; resets the window.
    pub(crate) fn log_fields(&mut self) -> String {
        let window = std::mem::take(&mut self.window);
        let mean = |sum: f64, count: u64| {
            if count == 0 {
                0.0
            } else {
                sum / count as f64
            }
        };
        format!(
            "{} refine_feed={} refine_observes={} refine_observe_ms_avg={:.2} \
             refine_observe_ms_max={:.2} refine_readbacks={} refine_readback_lag_ms_avg={:.2} \
             refine_readback_lag_ms_max={:.2} refine_witness_untrusted={}",
            self.runtime.log_fields(),
            self.feed.token(),
            window.observes,
            mean(window.observe_ms_sum, window.observes),
            window.observe_ms_max,
            window.readbacks,
            mean(window.readback_lag_ms_sum, window.readbacks),
            window.readback_lag_ms_max,
            self.witness.untrusted(),
        )
    }
}
