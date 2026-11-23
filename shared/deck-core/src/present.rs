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
//!
//! Where a window places either presenter's picture, and where its pointer
//! lands on it, is one shared value: [`fit::PictureRect`].

pub mod fit;

use std::num::NonZeroU32;
use std::time::Instant;

use arcen_media::VideoConfiguration;
use arcen_media::canvas::CanvasWork;
use arcen_media::refine::hub::DeckRefineHub;
use arcen_media::refine::tile::TileCodes;
use arcen_media::video::PresentTiming;

use crate::decode::NativeDecodedVideoFrame;

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
    /// Presents submitted to the GPU, re-presents included.
    pub frames_submitted: u64,
    /// The part of `frames_submitted` that showed a frame already shown
    /// again ([`VideoPresenterSink::represent_last`]); 0 from a presenter
    /// that does not tell them apart.
    pub frames_represented: u64,
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

impl PresenterTelemetryDelta {
    /// New decoded frames submitted to the GPU: submissions less
    /// re-presents. A submission, not a confirmation that it reached the
    /// glass.
    #[must_use]
    pub const fn new_frames_submitted(&self) -> u64 {
        self.frames_submitted
            .saturating_sub(self.frames_represented)
    }
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

/// What a sink's canvas executor can hold (ADR 0028). A sink without one
/// states `None` ([`VideoPresenterSink::CANVAS_EXECUTOR`]); its Deck then
/// never sends `canvas_v1` ([`crate::canvas::CanvasRequest::for_sink`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CanvasExecutorFacts {
    /// Largest canvas side in pixels the executor creates (its texture
    /// limit).
    pub max_canvas_px: u32,
}

/// What the Deck decoded for one canvas operation, aligned index for index
/// with [`CanvasWork::ops`].
#[derive(Debug, Clone)]
pub enum CanvasOpInput<N> {
    /// The operation needs nothing decoded (reset, fill, copy, cache and
    /// surface lifecycle operations).
    None,
    /// A `Tile`'s lossless codes: depth-8 planar R′G′B′ of the tile's
    /// rectangle ([`crate::canvas::tile_bgra`] writes them as BGRA8).
    Tile(TileCodes),
    /// A `SurfacePaint`'s decoded picture; `None` when the access unit
    /// produced no picture, which paints nothing.
    Surface(Option<NativeDecodedVideoFrame<N>>),
}

/// One canvas record's work for a sink's canvas executor.
#[derive(Debug)]
pub struct CanvasExecution<'w, 'a, N> {
    /// The validated operations, in order (the shared state owner,
    /// `arcen_media::canvas::CanvasState`, already checked every rectangle
    /// against the canvas).
    pub work: &'w CanvasWork<'a>,
    /// What the Deck decoded for each operation.
    pub inputs: &'w [CanvasOpInput<N>],
    /// When the media worker received the record.
    pub arrival: Instant,
    /// When to show the canvas: the served pipeline's shared timing.
    pub timing: PresentTiming,
    /// Whether this is the record's last part. A record whose decoded tiles
    /// are large reaches the sink in parts, all for the same fence: only
    /// the last one presents, so the record is shown whole.
    pub present: bool,
    /// Read the whole canvas back once this record's writes completed and
    /// hand it over through [`VideoPresenterSink::take_canvas_readback`]
    /// (the Deck's canvas digest).
    pub readback: bool,
}

/// The result of one [`VideoPresenterSink::execute`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanvasOutcome {
    /// Applied to the canvas, and the canvas was handed to the presenter.
    Presented,
    /// Applied to the canvas, which is not shown now (hidden, occluded or
    /// no free drawable); the next present shows it.
    Applied,
    /// This sink has no canvas executor. Its Deck never advertises canvas,
    /// so this answer means the work reached the wrong sink.
    Unsupported,
    /// The executor could not apply the work; the canvas is lost and the
    /// Deck asks the Pier for a reset.
    Failed(PresenterFailure),
}

/// A canvas the executor read back ([`CanvasExecution::readback`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanvasReadback {
    /// Monitor.
    pub monitor_id: u16,
    /// Canvas generation of the record that asked.
    pub generation: NonZeroU32,
    /// Fence of the record that asked (0 for a reset).
    pub fence: u64,
    /// Bytes per row of `bgra`.
    pub stride: usize,
    /// The canvas, opaque BGRA8, top row first.
    pub bgra: Vec<u8>,
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

    /// The sink's canvas executor (ADR 0028), or `None` when it has none.
    /// A sink stating `None` refuses every [`Self::execute`] with
    /// [`CanvasOutcome::Unsupported`], and its Deck never advertises
    /// `canvas_v1`: the request is built from this constant
    /// ([`crate::canvas::CanvasRequest::for_sink`]).
    const CANVAS_EXECUTOR: Option<CanvasExecutorFacts>;

    /// Applies one canvas record to the monitor's canvas
    /// (`canvas.work.monitor_id`), then presents the canvas. An eight-bit
    /// sink with a canvas executor:
    ///
    /// 1. keeps one persistent opaque BGRA8 canvas per monitor, created by
    ///    the work's `Reset` operation at its size (opaque black, cache
    ///    emptied) and kept across presents, resizes and visibility changes
    ///    (a canvas the sink lost is a [`CanvasOutcome::Failed`]);
    /// 2. performs the operations in order, exactly as the CPU executor
    ///    `arcen_media::canvas::CanvasPixels` does (the oracle its tests
    ///    compare against): fills, copies as if through a temporary, tiles
    ///    from [`CanvasOpInput::Tile`], cache stores and paints, and surface
    ///    paints that write only the listed blocks from
    ///    [`CanvasOpInput::Surface`] scaled into the destination;
    /// 3. orders every write before the composite that samples the canvas
    ///    (the [`RefineFrameTag`] rule, ADR 0016 R2: never a CPU write into
    ///    a texture a GPU command in flight may read), then, when
    ///    `canvas.present` is set, presents the canvas with `canvas.timing`
    ///    (a part without it is applied and returns
    ///    [`CanvasOutcome::Applied`]);
    /// 4. applies the work even while the surface is hidden or no drawable
    ///    is free ([`CanvasOutcome::Applied`]): skipping a record would
    ///    desynchronise the canvas from the Pier's mirror;
    /// 5. when `canvas.readback` is set, reads the canvas back after the
    ///    record's writes completed and returns it through
    ///    [`Self::take_canvas_readback`].
    ///
    /// Surface decoders are the media worker's (one per surface, from the
    /// Deck's `VideoDecoderFactory`); the sink only samples their pictures.
    fn execute(&mut self, canvas: &CanvasExecution<'_, '_, Self::NativeFrame>) -> CanvasOutcome;

    /// A canvas read back for an earlier [`CanvasExecution::readback`], at
    /// most once per request. A sink that cannot read back returns `None`;
    /// the canvas digest is then not shown.
    fn take_canvas_readback(&mut self) -> Option<CanvasReadback> {
        None
    }
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
