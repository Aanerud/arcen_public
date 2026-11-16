// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The presenter seam: where the media worker hands decoded pictures to the
//! platform's dedicated video presenters.
//!
//! Each Deck has two presentation pipelines that never merge (root
//! `AGENTS.md`, "Streaming pipeline boundary"; `clients/macos/AGENTS.md`):
//!
//! - [`PresenterPipeline::EightBit`]: Auto and Speed. macOS: an 8-bit
//!   `CAMetalLayer`; Windows: a `B8G8R8A8` flip-model swapchain in the
//!   `DirectComposition` video visual.
//! - [`PresenterPipeline::TenBit`]: Grading and HDR. macOS: an `RGB10A2`
//!   `CAMetalLayer` (EDR only for PQ); Windows: an `R10G10B10A2` swapchain,
//!   `G2084_P2020` + HDR10 metadata only when the Pier returned PQ.
//!
//! When neither dedicated presenter can show a frame, the media worker
//! falls back to the egui path (CPU RGBA) and the Deck shows a persistent
//! warning for a ten-bit contract.

use std::time::Instant;

use arcen_media::VideoConfiguration;
use arcen_media::refine::hub::DeckRefineHub;
use arcen_media::video::PresentTiming;

/// The Exact still image identity of a frame (ADR 0016 R2).
///
/// A [`PresenterPipeline::TenBit`] sink that receives a frame with a tag:
///
/// 1. calls `hub.compose(compositor, monitor_id, fence)` before it draws
///    that frame, where `compositor` is its
///    [`arcen_media::refine::hub::RefineCompositor`] (overlay writes are
///    ordered before the composite that samples them, never a CPU write
///    into a texture a GPU command in flight may read);
/// 2. composites in source pixels, before scaling or display transform;
/// 3. calls `hub.presented(monitor_id, fence)` only once *that exact frame*
///    is confirmed on screen (macOS: the drawable's presented handler;
///    Windows: DXGI frame statistics matched to the present that showed it),
///    never for a frame that was dropped, skipped or superseded;
/// 4. performs a requested readback after that composite completed and
///    hands the result to `hub.complete_readback`.
///
/// An eight-bit sink never receives a tag (ADR 0016 R7: exact tiles are
/// never fed to the eight-bit path).
#[derive(Debug, Clone)]
pub struct RefineFrameTag {
    /// This connection's hub.
    pub hub: DeckRefineHub,
    /// The monitor the frame belongs to.
    pub monitor_id: u16,
    /// The fence that preceded the frame's access unit on the video lane.
    pub fence: u64,
}

/// Which dedicated pipeline a sink implements.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresenterPipeline {
    /// Auto and Speed: 8-bit BT.709.
    EightBit,
    /// Grading (10-bit SDR) and HDR (10-bit PQ).
    TenBit,
}

/// One picture for a dedicated presenter.
#[derive(Debug, Clone)]
pub struct PresentableFrame<N> {
    /// The decoder's native picture (see `decode::VideoDecoderBackend` for
    /// its readiness and lifetime rules).
    pub picture: N,
    /// The complete video configuration it was decoded under. The
    /// presenter derives its colour space from this and the session's
    /// transfer, never from the bit depth alone.
    pub contract: VideoConfiguration,
    /// When the media worker received it, for on-glass latency statistics.
    pub arrival: Instant,
    /// The Pier's source frame rate, when known (pacing).
    pub source_fps: Option<u32>,
    /// The Exact still image tag; `None` for eight-bit and untagged frames.
    pub refine: Option<RefineFrameTag>,
    /// When to show it: the served pipeline's shared timing
    /// (`arcen_media::video::operational_present_timing`). A presenter
    /// applies [`PresentTiming::for_depth`] for its own depth.
    pub timing: PresentTiming,
}

/// Where establishing or drawing through a dedicated presenter failed. Every
/// fallback is explainable in the stream statistics and the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresenterFailureStage {
    /// Called from a thread the platform forbids.
    WrongThread,
    /// No session window or video surface to attach to.
    NoWindow,
    /// No GPU device or command queue.
    NoDevice,
    /// The native picture could not be imported as textures.
    PictureImport,
    /// Shader compilation or pipeline creation failed.
    Pipeline,
    /// The surface read back another format than the contract needs.
    PixelFormatMismatch,
    /// The output refused the colour space (Windows `SetColorSpace1`).
    ColourSpaceRejected,
    /// The measured effective output depth is below the contract (for
    /// example DWM composes the ten-bit surface at eight bits).
    EffectiveDepthBelowContract,
    /// Command buffer or present submission failed.
    Submission,
    /// The GPU device was lost or removed.
    DeviceLost,
}

/// A presenter failure with a fixed, loggable detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PresenterFailure {
    /// Where it failed.
    pub stage: PresenterFailureStage,
    /// What failed, in a few words (no allocation on the video thread).
    pub detail: &'static str,
}

/// The result of one hand-over to a dedicated presenter.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PresentOutcome {
    /// Not attached yet.
    #[default]
    Inactive,
    /// The frame went to the dedicated presenter.
    Presented,
    /// The surface is hidden or occluded; the frame was not drawn.
    SkippedOccluded,
    /// No drawable or swapchain buffer was free; the frame was dropped.
    DroppedFrame,
    /// The dedicated presenter cannot be used: eight-bit falls back to egui,
    /// ten-bit to the eight-bit path with a persistent visible warning.
    Fallback(PresenterFailure),
}

impl PresentOutcome {
    /// The frame reached the dedicated presenter.
    #[must_use]
    pub const fn is_presented(self) -> bool {
        matches!(self, Self::Presented)
    }

    /// The pipeline must fall back.
    #[must_use]
    pub const fn is_fallback(self) -> bool {
        matches!(self, Self::Fallback(_))
    }

    /// A transient miss that does not end dedicated presentation.
    #[must_use]
    pub const fn is_transient_drop_or_skip(self) -> bool {
        matches!(self, Self::SkippedOccluded | Self::DroppedFrame)
    }

    /// Keeps an established `Presented` across a transient miss once a
    /// picture is on screen, so the statistics do not flicker.
    #[must_use]
    pub const fn preserve_established_on_transient(
        self,
        incoming: Self,
        has_presented_picture: bool,
    ) -> Self {
        if has_presented_picture && self.is_presented() && incoming.is_transient_drop_or_skip() {
            self
        } else {
            incoming
        }
    }

    /// The failure behind a fallback.
    #[must_use]
    pub const fn fallback_reason(self) -> Option<PresenterFailure> {
        match self {
            Self::Fallback(reason) => Some(reason),
            Self::Inactive | Self::Presented | Self::SkippedOccluded | Self::DroppedFrame => None,
        }
    }
}

/// Presenter statistics since the last [`VideoPresenterSink::take_telemetry`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PresenterTelemetryDelta {
    /// Frames dropped before submission.
    pub drops: u64,
    /// Frames skipped while hidden or occluded.
    pub skips: u64,
    /// Waits for a free swapchain buffer or drawable.
    pub slot_waits: u64,
    /// Frames handed to the presenter.
    pub frames_received: u64,
    /// Frames submitted to the GPU.
    pub frames_submitted: u64,
    /// Frames confirmed on screen.
    pub frames_confirmed: u64,
    /// Frames the compositor (Core Animation, DWM) reported dropped.
    pub compositor_dropped: u64,
    /// Submitted frames whose on-screen time could not be confirmed.
    pub presented_time_unconfirmed: u64,
    /// Dropped because the presenter queue was full.
    pub dropped_overflow: u64,
    /// Dropped because a newer frame superseded them.
    pub dropped_stale: u64,
    /// Dropped while the surface was hidden.
    pub dropped_hidden: u64,
    /// Refreshes with no new frame to show.
    pub underruns: u64,
    /// Queue-depth samples taken.
    pub queue_depth_samples: u64,
    /// Sum of sampled queue depths.
    pub queue_depth_total: u64,
    /// Largest sampled queue depth.
    pub queue_depth_max: u64,
    /// Frames shown for one refresh.
    pub interval_1x: u64,
    /// Frames shown for two refreshes.
    pub interval_2x: u64,
    /// Frames shown for three or more refreshes.
    pub interval_3x_plus: u64,
    /// Median arrival-to-submit latency.
    pub latency_p50_us: Option<u64>,
    /// 99th percentile arrival-to-submit latency.
    pub latency_p99_us: Option<u64>,
    /// Median arrival-to-glass latency.
    pub on_glass_latency_p50_us: Option<u64>,
    /// 99th percentile arrival-to-glass latency.
    pub on_glass_latency_p99_us: Option<u64>,
    /// Median lateness against the target refresh.
    pub presented_after_target_p50_us: Option<u64>,
    /// When submitted frames were presented.
    pub submitted_presented_at: Vec<Instant>,
    /// When confirmed frames were presented.
    pub confirmed_presented_at: Vec<Instant>,
    /// The output's refresh period.
    pub refresh_period_us: Option<u64>,
}

/// The output colour state a ten-bit presenter applied, for the stream
/// statistics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayerColourState {
    /// Extended range is on (macOS EDR; Windows an HDR colour space).
    pub hdr_output: bool,
    /// The colour space named on the surface, for example
    /// `itur_2100_PQ` or `DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020`.
    pub colour_space: Option<String>,
    /// A persistent warning when the depth measured at the glass is below
    /// the contract (for example "this display shows 8 bits per colour;
    /// the 10-bit picture is reduced to 8 bits"). Shown in Stream
    /// Statistics for as long as it holds; `None` when the picture reaches
    /// the display at its full depth (root `AGENTS.md`: an 8-bit result is
    /// never silent).
    pub depth_warning: Option<String>,
}

/// Wakes the UI thread so it repaints (statistics, fallbacks, still-image
/// verdicts). The egui feature implements it for `egui::Context`.
pub trait RepaintRequester: Send + Sync {
    /// Requests one repaint as soon as possible.
    fn request_repaint(&self);
}

/// A dedicated video presenter, driven by the media worker.
///
/// The sink is shared between the UI thread (attach, resize, visibility,
/// teardown) and the media worker (present) behind a mutex, so
/// [`Self::present`] must not block on the display: it hands the frame to
/// the presenter's own pacing (a display-link or waitable-swapchain thread)
/// and returns the hand-over's outcome. Later outcomes arrive through
/// [`Self::take_async_status`].
pub trait VideoPresenterSink {
    /// The decoder's native picture this presenter samples. It must equal
    /// the decoder's `VideoDecoderBackend::NativeFrame`.
    type NativeFrame;

    /// Which pipeline this sink implements.
    fn pipeline(&self) -> PresenterPipeline;

    /// Hands one frame over. See [`RefineFrameTag`] for the Exact still
    /// image obligations of a ten-bit sink.
    fn present(&mut self, frame: &PresentableFrame<Self::NativeFrame>) -> PresentOutcome;

    /// Presents the last frame again (a still frame whose Exact still image
    /// overlay changed). `None` when nothing was presented yet.
    fn represent_last(&mut self) -> Option<PresentOutcome>;

    /// The video surface became visible or hidden (window minimised,
    /// occluded, moved off a display).
    fn set_surface_visible(&mut self, visible: bool);

    /// An outcome the presenter thread reached after `present` returned
    /// (for example a device loss), at most once.
    fn take_async_status(&mut self) -> Option<PresentOutcome>;

    /// Whether at least one picture is on screen.
    fn has_presented_picture(&self) -> bool;

    /// Statistics since the last call.
    fn take_telemetry(&self) -> PresenterTelemetryDelta;

    /// The applied output colour state; `None` before the first present.
    fn colour_state(&self) -> Option<LayerColourState>;

    /// Detaches from the window and releases GPU resources. Queued frames
    /// are discarded, never shown late.
    fn teardown(&mut self);
}

#[cfg(test)]
mod tests {
    use super::*;

    const FAILURE: PresenterFailure = PresenterFailure {
        stage: PresenterFailureStage::EffectiveDepthBelowContract,
        detail: "DWM composed at 8 bits",
    };

    #[test]
    fn an_established_picture_survives_transient_misses_but_not_a_fallback() {
        let established = PresentOutcome::Presented;
        assert_eq!(
            established.preserve_established_on_transient(PresentOutcome::DroppedFrame, true),
            PresentOutcome::Presented
        );
        assert_eq!(
            established.preserve_established_on_transient(PresentOutcome::DroppedFrame, false),
            PresentOutcome::DroppedFrame
        );
        let fallback = PresentOutcome::Fallback(FAILURE);
        assert_eq!(
            established.preserve_established_on_transient(fallback, true),
            fallback
        );
        assert_eq!(fallback.fallback_reason(), Some(FAILURE));
        assert!(fallback.is_fallback() && !fallback.is_presented());
    }
}
