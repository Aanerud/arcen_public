// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The Windows `VideoPresenterSink`s: one per pipeline, as on the Mac.
//!
//! The eight-bit sink (Auto, Speed) owns a `B8G8R8A8` swapchain; the ten-bit
//! sink (Grading, HDR) owns an `R10G10B10A2` (or FP16 scRGB) swapchain and
//! the Exact still image compositor. Each runs its own presenter thread;
//! the one that presents last owns the window's video visual. Neither ever
//! routes the other's contract (root `AGENTS.md`, streaming pipeline
//! boundary).

use super::compose::DecodedFrame;
use super::composition::SharedTree;
use super::device::Gpu;
use super::picture::D3d11Picture;
use super::swapchain::{PresentMode, VideoSwapchain};
use super::thread::{PresenterEvent, PresenterThread};
use arcen_deck_core::present::{
    LayerColourState, PresentOutcome, PresentableFrame, PresenterFailure, PresenterFailureStage,
    PresenterPipeline, PresenterTelemetryDelta, VideoPresenterSink,
};
use arcen_media::BitDepth;
use arcen_media::video::PresentTiming;
use std::sync::Mutex;
use std::sync::atomic::Ordering;

/// Counter values at the last telemetry drain.
#[derive(Debug, Default, Clone, Copy)]
struct Baseline {
    received: u64,
    presented: u64,
    confirmed: u64,
    unconfirmed: u64,
    replaced: u64,
    skipped: u64,
    failed: u64,
    latency_total: u64,
    latency_samples: u64,
}

pub struct D3d11VideoSink {
    pipeline: PresenterPipeline,
    gpu: Gpu,
    tree: SharedTree,
    thread: Option<PresenterThread>,
    failure: Option<PresenterFailure>,
    baseline: Mutex<Baseline>,
    visible: bool,
    /// The served pipeline's presentation timing (before this pipeline's
    /// depth is applied).
    timing: PresentTiming,
}

const fn failure(stage: PresenterFailureStage, detail: &'static str) -> PresenterFailure {
    PresenterFailure { stage, detail }
}

impl D3d11VideoSink {
    /// A sink for `pipeline` on `gpu` (the adapter that owns the window's
    /// monitor; the decoder shares it), drawing into `tree`'s video visual.
    #[must_use]
    pub fn new(pipeline: PresenterPipeline, gpu: Gpu, tree: SharedTree) -> Self {
        Self {
            pipeline,
            gpu,
            tree,
            thread: None,
            failure: None,
            baseline: Mutex::new(Baseline::default()),
            visible: true,
            timing: PresentTiming::Vsync,
        }
    }

    /// When each frame is presented: the served pipeline's shared
    /// [`PresentTiming`], carried by every frame the shared media worker
    /// hands over.
    fn set_timing(&mut self, timing: PresentTiming) {
        if timing == self.timing {
            return;
        }
        self.timing = timing;
        if let Some(thread) = &self.thread {
            thread.handle().set_timing(self.effective_timing());
            self.log_timing();
        }
    }

    /// The timing this pipeline applies: a ten-bit picture always waits
    /// for the refresh ([`PresentTiming::for_depth`]).
    fn effective_timing(&self) -> PresentTiming {
        self.timing.for_depth(match self.pipeline {
            PresenterPipeline::EightBit => BitDepth::Eight,
            PresenterPipeline::TenBit => BitDepth::Ten,
        })
    }

    fn log_timing(&self) {
        let timing = self.effective_timing();
        tracing::info!(
            target: arcen_deck_core::log_target::VIDEO,
            pipeline = self.owner(),
            served = self.timing.token(),
            timing = timing.token(),
            tearing_supported = timing.allows_tearing()
                && super::swapchain::tearing_supported(&self.gpu),
            "video presentation timing: {}",
            match timing {
                PresentTiming::Vsync => "on the refresh (sync interval 1)",
                PresentTiming::Newest =>
                    "each frame at once, newest wins, no tearing (sync interval 0)",
                PresentTiming::Tearing =>
                    "each frame at once, may tear (sync interval 0, ALLOW_TEARING)",
            }
        );
    }

    fn owner(&self) -> usize {
        match self.pipeline {
            PresenterPipeline::EightBit => 8,
            PresenterPipeline::TenBit => 10,
        }
    }

    fn accepts(&self, depth: BitDepth) -> bool {
        match self.pipeline {
            PresenterPipeline::EightBit => depth == BitDepth::Eight,
            PresenterPipeline::TenBit => depth != BitDepth::Eight,
        }
    }

    fn start(&mut self, frame: &DecodedFrame) -> Result<(), PresenterFailure> {
        let mode = PresentMode::for_contract(frame.contract, false);
        let (width, height) = frame.picture.size();
        let swapchain =
            VideoSwapchain::create(&self.gpu, width, height, mode).map_err(|error| {
                if error.contains("cannot present") {
                    failure(
                        PresenterFailureStage::ColourSpaceRejected,
                        "SetColorSpace1 refused",
                    )
                } else {
                    failure(PresenterFailureStage::NoWindow, "composition swapchain")
                }
            })?;
        let thread = PresenterThread::spawn(
            self.gpu.clone(),
            swapchain,
            Some((self.tree.clone(), self.owner())),
        )
        .map_err(|_| {
            failure(
                PresenterFailureStage::Pipeline,
                "presenter shaders or thread",
            )
        })?;
        thread.handle().set_timing(self.effective_timing());
        thread.handle().set_visible(self.visible);
        self.log_timing();
        self.thread = Some(thread);
        Ok(())
    }
}

impl VideoPresenterSink for D3d11VideoSink {
    type NativeFrame = D3d11Picture;

    fn pipeline(&self) -> PresenterPipeline {
        self.pipeline
    }

    fn present(&mut self, frame: &PresentableFrame<D3d11Picture>) -> PresentOutcome {
        if let Some(reason) = self.failure {
            return PresentOutcome::Fallback(reason);
        }
        if !self.accepts(frame.contract.bit_depth) {
            return PresentOutcome::Fallback(failure(
                PresenterFailureStage::PixelFormatMismatch,
                "frame depth is not this pipeline's",
            ));
        }
        self.set_timing(frame.timing);
        let mut decoded = DecodedFrame::from_presentable(frame);
        if self.pipeline == PresenterPipeline::EightBit {
            // ADR 0016 R7: exact tiles never reach the eight-bit path.
            decoded.refine = None;
        }
        if self.thread.is_none()
            && let Err(reason) = self.start(&decoded)
        {
            self.failure = Some(reason);
            return PresentOutcome::Fallback(reason);
        }
        if !self.visible {
            return PresentOutcome::SkippedOccluded;
        }
        let Some(thread) = &self.thread else {
            return PresentOutcome::Inactive;
        };
        thread.handle().submit_at(decoded, frame.arrival);
        PresentOutcome::Presented
    }

    fn represent_last(&mut self) -> Option<PresentOutcome> {
        let thread = self.thread.as_ref()?;
        let handle = thread.handle();
        if !handle.has_presented() {
            return None;
        }
        handle.represent();
        Some(PresentOutcome::Presented)
    }

    fn set_surface_visible(&mut self, visible: bool) {
        self.visible = visible;
        if let Some(thread) = &self.thread {
            thread.handle().set_visible(visible);
        }
    }

    fn take_async_status(&mut self) -> Option<PresentOutcome> {
        let thread = self.thread.as_ref()?;
        let mut outcome = None;
        for event in thread.handle().take_events() {
            match event {
                PresenterEvent::DeviceLost(_) => {
                    let reason = failure(PresenterFailureStage::DeviceLost, "device removed");
                    self.failure = Some(reason);
                    outcome = Some(PresentOutcome::Fallback(reason));
                }
                PresenterEvent::Failed(_) if outcome.is_none() => {
                    outcome = Some(PresentOutcome::DroppedFrame);
                }
                PresenterEvent::Failed(_) | PresenterEvent::ModeChanged(_) => {}
            }
        }
        outcome
    }

    fn has_presented_picture(&self) -> bool {
        self.thread
            .as_ref()
            .is_some_and(|thread| thread.handle().has_presented())
    }

    fn take_telemetry(&self) -> PresenterTelemetryDelta {
        let Some(thread) = &self.thread else {
            return PresenterTelemetryDelta::default();
        };
        let handle = thread.handle();
        let c = handle.counters();
        let now = Baseline {
            received: c.received.load(Ordering::Relaxed),
            presented: c.presented.load(Ordering::Relaxed) + c.represented.load(Ordering::Relaxed),
            confirmed: c.confirmed.load(Ordering::Relaxed),
            unconfirmed: c.unconfirmed.load(Ordering::Relaxed),
            replaced: c.replaced_before_present.load(Ordering::Relaxed),
            skipped: c.skipped_hidden.load(Ordering::Relaxed),
            failed: c.failed.load(Ordering::Relaxed),
            latency_total: c.latency_us_total.load(Ordering::Relaxed),
            latency_samples: c.latency_samples.load(Ordering::Relaxed),
        };
        let mut baseline = self
            .baseline
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let before = *baseline;
        *baseline = now;
        let samples = now.latency_samples - before.latency_samples;
        PresenterTelemetryDelta {
            frames_received: now.received - before.received,
            frames_submitted: now.presented - before.presented,
            frames_confirmed: now.confirmed - before.confirmed,
            presented_time_unconfirmed: now.unconfirmed - before.unconfirmed,
            dropped_stale: now.replaced - before.replaced,
            dropped_hidden: now.skipped - before.skipped,
            drops: now.failed - before.failed,
            skips: now.skipped - before.skipped,
            // The mean stands in for the median until samples are kept.
            latency_p50_us: (samples > 0)
                .then(|| (now.latency_total - before.latency_total) / samples),
            ..PresenterTelemetryDelta::default()
        }
    }

    fn colour_state(&self) -> Option<LayerColourState> {
        let mode = self.thread.as_ref()?.handle().mode()?;
        Some(LayerColourState {
            hdr_output: mode.is_hdr(),
            colour_space: Some(format!(
                "DXGI_COLOR_SPACE_{}",
                super::display::color_space_name(mode.color_space())
            )),
            // `present` fills this from `depth::effective_depth`.
            depth_warning: None,
        })
    }

    fn teardown(&mut self) {
        // Dropping the thread discards queued frames and clears the video
        // visual if this pipeline showed it.
        self.thread = None;
        self.failure = None;
    }
}

super::opaque_debug!(D3d11VideoSink);
