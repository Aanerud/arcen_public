// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The media worker moved to [`arcen_deck_core::runtime::media_worker`]
//! (ADR 0024 extraction step 5). This is its macOS half: the VideoToolbox
//! decoder, the two Metal presenters as
//! [`arcen_deck_core::present::VideoPresenterSink`]s, and the Metal
//! presentation statuses the UI keeps, converted to and from the shared
//! [`PresentOutcome`] without loss.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use arcen_deck_core::decode::VideoDecoderFactory;
use arcen_deck_core::present::{
    LayerColourState, PresentOutcome, PresentableFrame, PresenterFailure, PresenterFailureStage,
    PresenterPipeline, PresenterTelemetryDelta, VideoPresenterSink,
};
use arcen_deck_core::runtime::media_worker as core;
use tokio::sync::mpsc;

use crate::observability::ClientTelemetry;
use crate::pipeline::frame_queue::IncomingMediaReceiver;
use crate::pipeline::video_decoder::{NativePixelBuffer, NativeVideoDecoder};
use crate::transport::websocket::{SessionCommandSender, SessionEvent};
use crate::ui::video_metal_layer::{
    current_host_time, DedicatedEightBitLayerFrame, DedicatedEightBitPresentationStatus,
    DedicatedEightBitVideoPresenter, DedicatedLayerFrame, DedicatedLayerOutcome,
    DedicatedPresentationStatus, DedicatedPresenterTelemetryDelta, DedicatedVideoPresenter,
};

pub use arcen_deck_core::runtime::media_worker::{
    mirror_audio_playback, MonitorMediaCounters, MonitorPresentationTarget,
    NativePresentationMetadata,
};

/// The macOS Deck's media pipeline: VideoToolbox pictures
/// (`CVPixelBuffer`) presented by the eight-bit and ten-bit Metal layers.
#[derive(Debug, Default)]
pub(crate) struct MacMedia;

impl core::MediaPlatform for MacMedia {
    type NativeFrame = NativePixelBuffer;
    type Decoder = NativeVideoDecoder;
    type EightBitPresenter = DedicatedEightBitVideoPresenter;
    type TenBitPresenter = DedicatedVideoPresenter;
    type EightBitStatus = DedicatedEightBitPresentationStatus;
    type TenBitStatus = DedicatedPresentationStatus;
}

/// What the media worker publishes for the UI thread.
pub(crate) type SharedMediaState = core::SharedMediaState<MacMedia>;

/// A fresh VideoToolbox decoder per connection attempt, created on the
/// media worker's thread.
struct VideoToolboxDecoders;

impl VideoDecoderFactory for VideoToolboxDecoders {
    type Decoder = NativeVideoDecoder;

    fn create(&self) -> NativeVideoDecoder {
        NativeVideoDecoder::new()
    }
}

/// Starts the media worker for one connection attempt with the VideoToolbox
/// decoder and the CoreAudio player.
pub(crate) fn spawn_media_worker(
    events: mpsc::UnboundedReceiver<SessionEvent>,
    media: IncomingMediaReceiver,
    commands: SessionCommandSender,
    shared: Arc<Mutex<SharedMediaState>>,
    repaint: egui::Context,
    telemetry: Arc<ClientTelemetry>,
) {
    core::spawn_media_worker(
        events,
        media,
        commands,
        shared,
        Arc::new(crate::ui::pass_pacing::MediaWake(repaint)),
        telemetry,
        VideoToolboxDecoders,
        crate::ui::audio_worker::macos_player,
    );
}

/// The host-clock time of `arrival`, which the Metal presenters measure
/// on-glass latency against.
fn host_time_at(arrival: Instant) -> Duration {
    current_host_time().saturating_sub(arrival.elapsed())
}

impl VideoPresenterSink for DedicatedVideoPresenter {
    type NativeFrame = NativePixelBuffer;

    fn pipeline(&self) -> PresenterPipeline {
        PresenterPipeline::TenBit
    }

    // Off macOS the picture type is uninhabited, so nothing after it runs.
    #[cfg_attr(not(target_os = "macos"), allow(unreachable_code, unused_variables))]
    fn present(&mut self, frame: &PresentableFrame<NativePixelBuffer>) -> PresentOutcome {
        let layer_frame = DedicatedLayerFrame {
            pixel_buffer: frame.picture.clone(),
            contract: frame.contract.into(),
            arrival_host_time: host_time_at(frame.arrival),
            source_fps: frame.source_fps,
            refine: frame.refine.clone(),
        };
        Self::present(self, &layer_frame).into()
    }

    fn represent_last(&mut self) -> Option<PresentOutcome> {
        Self::represent_last(self).map(Into::into)
    }

    fn set_surface_visible(&mut self, visible: bool) {
        Self::set_surface_visible(self, visible);
    }

    fn take_async_status(&mut self) -> Option<PresentOutcome> {
        Self::take_async_status(self).map(Into::into)
    }

    fn has_presented_picture(&self) -> bool {
        Self::has_presented_picture(self)
    }

    fn take_telemetry(&self) -> PresenterTelemetryDelta {
        shared_telemetry(Self::take_telemetry(self))
    }

    fn colour_state(&self) -> Option<LayerColourState> {
        self.layer_colour_state().map(shared_colour_state)
    }

    fn teardown(&mut self) {
        Self::teardown(self);
    }
}

impl VideoPresenterSink for DedicatedEightBitVideoPresenter {
    type NativeFrame = NativePixelBuffer;

    fn pipeline(&self) -> PresenterPipeline {
        PresenterPipeline::EightBit
    }

    // Off macOS the picture type is uninhabited, so nothing after it runs.
    #[cfg_attr(not(target_os = "macos"), allow(unreachable_code, unused_variables))]
    fn present(&mut self, frame: &PresentableFrame<NativePixelBuffer>) -> PresentOutcome {
        let layer_frame = DedicatedEightBitLayerFrame {
            pixel_buffer: frame.picture.clone(),
            contract: frame.contract.into(),
            arrival_host_time: host_time_at(frame.arrival),
            source_fps: frame.source_fps,
            timing: frame.timing,
        };
        Self::present(self, &layer_frame).into()
    }

    fn represent_last(&mut self) -> Option<PresentOutcome> {
        None
    }

    fn set_surface_visible(&mut self, visible: bool) {
        Self::set_surface_visible(self, visible);
    }

    fn take_async_status(&mut self) -> Option<PresentOutcome> {
        Self::take_async_status(self).map(Into::into)
    }

    fn has_presented_picture(&self) -> bool {
        Self::has_presented_picture(self)
    }

    fn take_telemetry(&self) -> PresenterTelemetryDelta {
        shared_telemetry(Self::take_telemetry(self))
    }

    fn colour_state(&self) -> Option<LayerColourState> {
        self.layer_colour_state().map(shared_colour_state)
    }

    fn teardown(&mut self) {
        Self::teardown(self);
    }
}

fn shared_colour_state(state: crate::ui::video_metal_layer::LayerColourState) -> LayerColourState {
    LayerColourState {
        hdr_output: state.edr,
        colour_space: state.colour_space,
        depth_warning: None,
    }
}

fn shared_telemetry(delta: DedicatedPresenterTelemetryDelta) -> PresenterTelemetryDelta {
    PresenterTelemetryDelta {
        drops: delta.drops,
        skips: delta.skips,
        slot_waits: delta.slot_waits,
        frames_received: delta.frames_received,
        frames_submitted: delta.frames_submitted,
        frames_confirmed: delta.frames_confirmed,
        compositor_dropped: delta.metal_dropped,
        presented_time_unconfirmed: delta.presented_time_unconfirmed,
        dropped_overflow: delta.dropped_overflow,
        dropped_stale: delta.dropped_stale,
        dropped_hidden: delta.dropped_hidden,
        underruns: delta.underruns,
        queue_depth_samples: delta.queue_depth_samples,
        queue_depth_total: delta.queue_depth_total,
        queue_depth_max: delta.queue_depth_max,
        interval_1x: delta.interval_1x,
        interval_2x: delta.interval_2x,
        interval_3x_plus: delta.interval_3x_plus,
        latency_p50_us: delta.latency_p50_us,
        latency_p99_us: delta.latency_p99_us,
        on_glass_latency_p50_us: delta.on_glass_latency_p50_us,
        on_glass_latency_p99_us: delta.on_glass_latency_p99_us,
        presented_after_target_p50_us: delta.presented_after_target_p50_us,
        submitted_presented_at: delta.submitted_presented_at,
        confirmed_presented_at: delta.confirmed_presented_at,
        refresh_period_us: delta.refresh_period_us,
    }
}

/// Every Metal layer outcome, for the reverse of [`layer_failure`].
const LAYER_OUTCOMES: [DedicatedLayerOutcome; 16] = [
    DedicatedLayerOutcome::Ready,
    DedicatedLayerOutcome::NotMainThread,
    DedicatedLayerOutcome::NoRootWindow,
    DedicatedLayerOutcome::NoContentView,
    DedicatedLayerOutcome::NoRootLayer,
    DedicatedLayerOutcome::NoMetalDevice,
    DedicatedLayerOutcome::CommandQueueCreationFailed,
    DedicatedLayerOutcome::TextureCacheCreationFailed,
    DedicatedLayerOutcome::ShaderCompilationFailed,
    DedicatedLayerOutcome::PipelineStateCreationFailed,
    DedicatedLayerOutcome::PixelFormatMismatch,
    DedicatedLayerOutcome::PlaneTextureCreationFailed,
    DedicatedLayerOutcome::NoDrawableAvailable,
    DedicatedLayerOutcome::UniformBufferCreationFailed,
    DedicatedLayerOutcome::CommandBufferCreationFailed,
    DedicatedLayerOutcome::RenderEncoderCreationFailed,
];

/// The shared failure for a Metal layer outcome. Each outcome has its own
/// detail, so [`layer_outcome`] recovers it exactly.
const fn layer_failure(outcome: DedicatedLayerOutcome) -> PresenterFailure {
    use DedicatedLayerOutcome as O;
    use PresenterFailureStage as S;
    let (stage, detail) = match outcome {
        O::Ready => (S::Submission, "metal layer ready"),
        O::NotMainThread => (S::WrongThread, "not on the main thread"),
        O::NoRootWindow => (S::NoWindow, "no root window"),
        O::NoContentView => (S::NoWindow, "root window has no content view"),
        O::NoRootLayer => (S::NoWindow, "content view has no layer"),
        O::NoMetalDevice => (S::NoDevice, "no Metal device"),
        O::CommandQueueCreationFailed => (S::NoDevice, "Metal command queue creation failed"),
        O::TextureCacheCreationFailed => {
            (S::PictureImport, "CoreVideo texture cache creation failed")
        }
        O::ShaderCompilationFailed => (S::Pipeline, "Metal shader compilation failed"),
        O::PipelineStateCreationFailed => (S::Pipeline, "Metal pipeline state creation failed"),
        O::PixelFormatMismatch => (S::PixelFormatMismatch, "layer pixel format mismatch"),
        O::PlaneTextureCreationFailed => (S::PictureImport, "plane texture creation failed"),
        O::NoDrawableAvailable => (S::Submission, "no drawable available"),
        O::UniformBufferCreationFailed => (S::Submission, "uniform buffer creation failed"),
        O::CommandBufferCreationFailed => (S::Submission, "command buffer creation failed"),
        O::RenderEncoderCreationFailed => (S::Submission, "render encoder creation failed"),
    };
    PresenterFailure { stage, detail }
}

/// The Metal layer outcome behind a shared failure. Only the Metal
/// presenters produce failures on the macOS Deck, so the detail always
/// matches; a foreign failure maps by its stage.
fn layer_outcome(failure: PresenterFailure) -> DedicatedLayerOutcome {
    if let Some(outcome) = LAYER_OUTCOMES
        .iter()
        .copied()
        .find(|outcome| layer_failure(*outcome) == failure)
    {
        return outcome;
    }
    match failure.stage {
        PresenterFailureStage::WrongThread => DedicatedLayerOutcome::NotMainThread,
        PresenterFailureStage::NoWindow => DedicatedLayerOutcome::NoRootLayer,
        PresenterFailureStage::NoDevice => DedicatedLayerOutcome::NoMetalDevice,
        PresenterFailureStage::PictureImport => DedicatedLayerOutcome::PlaneTextureCreationFailed,
        PresenterFailureStage::Pipeline => DedicatedLayerOutcome::PipelineStateCreationFailed,
        PresenterFailureStage::PixelFormatMismatch => DedicatedLayerOutcome::PixelFormatMismatch,
        PresenterFailureStage::ColourSpaceRejected
        | PresenterFailureStage::EffectiveDepthBelowContract
        | PresenterFailureStage::Submission
        | PresenterFailureStage::DeviceLost => DedicatedLayerOutcome::CommandBufferCreationFailed,
    }
}

impl From<DedicatedPresentationStatus> for PresentOutcome {
    fn from(status: DedicatedPresentationStatus) -> Self {
        match status {
            DedicatedPresentationStatus::Inactive => Self::Inactive,
            DedicatedPresentationStatus::DedicatedTenBit => Self::Presented,
            DedicatedPresentationStatus::SkippedOccluded => Self::SkippedOccluded,
            DedicatedPresentationStatus::DroppedFrame => Self::DroppedFrame,
            DedicatedPresentationStatus::FallbackToEightBit(outcome) => {
                Self::Fallback(layer_failure(outcome))
            }
        }
    }
}

impl From<PresentOutcome> for DedicatedPresentationStatus {
    fn from(outcome: PresentOutcome) -> Self {
        match outcome {
            PresentOutcome::Inactive => Self::Inactive,
            PresentOutcome::Presented => Self::DedicatedTenBit,
            PresentOutcome::SkippedOccluded => Self::SkippedOccluded,
            PresentOutcome::DroppedFrame => Self::DroppedFrame,
            PresentOutcome::Fallback(failure) => Self::FallbackToEightBit(layer_outcome(failure)),
        }
    }
}

impl From<DedicatedEightBitPresentationStatus> for PresentOutcome {
    fn from(status: DedicatedEightBitPresentationStatus) -> Self {
        match status {
            DedicatedEightBitPresentationStatus::Inactive => Self::Inactive,
            DedicatedEightBitPresentationStatus::DedicatedEightBit => Self::Presented,
            DedicatedEightBitPresentationStatus::SkippedOccluded => Self::SkippedOccluded,
            DedicatedEightBitPresentationStatus::DroppedFrame => Self::DroppedFrame,
            DedicatedEightBitPresentationStatus::FallbackToEgui(outcome) => {
                Self::Fallback(layer_failure(outcome))
            }
        }
    }
}

impl From<PresentOutcome> for DedicatedEightBitPresentationStatus {
    fn from(outcome: PresentOutcome) -> Self {
        match outcome {
            PresentOutcome::Inactive => Self::Inactive,
            PresentOutcome::Presented => Self::DedicatedEightBit,
            PresentOutcome::SkippedOccluded => Self::SkippedOccluded,
            PresentOutcome::DroppedFrame => Self::DroppedFrame,
            PresentOutcome::Fallback(failure) => Self::FallbackToEgui(layer_outcome(failure)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_metal_layer_outcome_survives_the_shared_outcome() {
        let mut details = std::collections::BTreeSet::new();
        for outcome in LAYER_OUTCOMES {
            assert!(details.insert(layer_failure(outcome).detail));
            assert_eq!(layer_outcome(layer_failure(outcome)), outcome);
            for status in [
                DedicatedPresentationStatus::FallbackToEightBit(outcome),
                DedicatedPresentationStatus::Inactive,
                DedicatedPresentationStatus::DedicatedTenBit,
                DedicatedPresentationStatus::SkippedOccluded,
                DedicatedPresentationStatus::DroppedFrame,
            ] {
                assert_eq!(
                    DedicatedPresentationStatus::from(PresentOutcome::from(status)),
                    status
                );
            }
            for status in [
                DedicatedEightBitPresentationStatus::FallbackToEgui(outcome),
                DedicatedEightBitPresentationStatus::Inactive,
                DedicatedEightBitPresentationStatus::DedicatedEightBit,
                DedicatedEightBitPresentationStatus::SkippedOccluded,
                DedicatedEightBitPresentationStatus::DroppedFrame,
            ] {
                assert_eq!(
                    DedicatedEightBitPresentationStatus::from(PresentOutcome::from(status)),
                    status
                );
            }
        }
    }

    #[test]
    fn shared_outcomes_keep_the_metal_status_meaning() {
        let fallback = DedicatedPresentationStatus::FallbackToEightBit(
            DedicatedLayerOutcome::ShaderCompilationFailed,
        );
        assert!(PresentOutcome::from(fallback).is_fallback());
        assert!(PresentOutcome::from(DedicatedPresentationStatus::DedicatedTenBit).is_presented());
        assert!(
            PresentOutcome::from(DedicatedEightBitPresentationStatus::DroppedFrame)
                .is_transient_drop_or_skip()
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn unattached_metal_presenters_skip_hidden_frames_and_fall_back_when_visible() {
        let picture = apple_cf::cv::CVPixelBuffer::create(2, 2, u32::from_be_bytes(*b"xf44"))
            .expect("test CVPixelBuffer");
        let frame = PresentableFrame {
            picture,
            contract: arcen_media::VideoConfiguration::legacy_h264(),
            arrival: Instant::now(),
            source_fps: Some(60),
            refine: None,
            timing: arcen_media::video::PresentTiming::Vsync,
        };
        let mut ten_bit = DedicatedVideoPresenter::new();
        assert_eq!(
            VideoPresenterSink::present(&mut ten_bit, &frame),
            PresentOutcome::SkippedOccluded
        );
        VideoPresenterSink::set_surface_visible(&mut ten_bit, true);
        let outcome = VideoPresenterSink::present(&mut ten_bit, &frame);
        assert_eq!(
            DedicatedPresentationStatus::from(outcome),
            DedicatedPresentationStatus::FallbackToEightBit(DedicatedLayerOutcome::NoRootLayer)
        );

        let mut eight_bit = DedicatedEightBitVideoPresenter::new();
        assert_eq!(
            VideoPresenterSink::present(&mut eight_bit, &frame),
            PresentOutcome::SkippedOccluded
        );
        assert_eq!(
            VideoPresenterSink::pipeline(&eight_bit),
            PresenterPipeline::EightBit
        );
    }
}
