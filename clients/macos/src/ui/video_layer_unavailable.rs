// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! `video_metal_layer` off macOS: there is no Metal layer, so no dedicated
//! presentation is ever attempted. The presenters report `Inactive` and the
//! egui/wgpu path presents, which is what the Deck already does whenever a
//! dedicated layer is not in use. No dedicated frame can exist here: the
//! frame types are uninhabited, so every path that would hand one to a
//! presenter is unreachable. Compiled so the portable Deck type-checks for
//! other targets (ADR 0024, step 0); a Windows Deck has its own presenter.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

pub(crate) use super::video_layer_status::*;

use super::video_render::VideoColorContract;
use crate::pipeline::video_decoder::NativePixelBuffer;

/// A ten-bit frame for the Metal layer. Its picture is uninhabited off
/// macOS, so no value exists.
#[derive(Debug, Clone)]
pub struct DedicatedLayerFrame {
    pub pixel_buffer: NativePixelBuffer,
    pub contract: VideoColorContract,
    pub arrival_host_time: Duration,
    pub source_fps: Option<u32>,
    pub refine: Option<RefineFrameTag>,
}

/// What the eight-bit layer shows; off macOS only an (uninhabited)
/// picture, since there is no canvas executor.
#[derive(Debug, Clone)]
pub enum EightBitLayerSource {
    Picture(NativePixelBuffer),
}

/// An eight-bit frame for the Metal layer; uninhabited off macOS too.
#[derive(Debug, Clone)]
pub struct DedicatedEightBitLayerFrame {
    pub source: EightBitLayerSource,
    pub contract: VideoColorContract,
    pub arrival_host_time: Duration,
    pub source_fps: Option<u32>,
    pub timing: arcen_media::video::PresentTiming,
}

impl DedicatedEightBitLayerFrame {
    pub(crate) fn pixel_buffer(&self) -> Option<&NativePixelBuffer> {
        match &self.source {
            EightBitLayerSource::Picture(picture) => Some(picture),
        }
    }
}

/// Monotonic time since the first call; the Metal path uses the host clock.
pub(crate) fn current_host_time() -> Duration {
    static ORIGIN: OnceLock<Instant> = OnceLock::new();
    ORIGIN.get_or_init(Instant::now).elapsed()
}

/// The ten-bit presenter where no dedicated layer exists.
#[derive(Default)]
pub struct DedicatedVideoPresenter;

impl DedicatedVideoPresenter {
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    pub(crate) fn attach_or_resize_egui(
        &mut self,
        _rect: egui::Rect,
        _visible: bool,
    ) -> DedicatedPresentationStatus {
        DedicatedPresentationStatus::Inactive
    }

    pub(crate) fn present(&mut self, frame: &DedicatedLayerFrame) -> DedicatedPresentationStatus {
        match frame.pixel_buffer {}
    }

    pub(crate) fn set_surface_visible(&mut self, _visible: bool) {}

    pub(crate) fn set_wake_context(&mut self, _ctx: egui::Context) {}

    pub(crate) fn take_async_status(&mut self) -> Option<DedicatedPresentationStatus> {
        None
    }

    pub(crate) fn represent_last(&mut self) -> Option<DedicatedPresentationStatus> {
        None
    }

    pub(crate) fn has_presented_picture(&self) -> bool {
        false
    }

    pub(crate) fn take_telemetry(&self) -> DedicatedPresenterTelemetryDelta {
        DedicatedPresenterTelemetryDelta::default()
    }

    pub(crate) fn take_presentations(&self) -> DedicatedPresentations {
        DedicatedPresentations::default()
    }

    pub fn teardown(&mut self) {}

    pub(crate) fn layer_colour_state(&self) -> Option<LayerColourState> {
        None
    }

    pub(crate) fn last_presented(&self) -> Option<DedicatedLayerFrame> {
        None
    }
}

/// The eight-bit presenter where no dedicated layer exists.
#[derive(Default)]
pub struct DedicatedEightBitVideoPresenter;

impl DedicatedEightBitVideoPresenter {
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    pub(crate) fn attach_or_resize_egui(
        &mut self,
        _rect: egui::Rect,
        _visible: bool,
    ) -> DedicatedEightBitPresentationStatus {
        DedicatedEightBitPresentationStatus::Inactive
    }

    pub(crate) fn present(
        &mut self,
        frame: &DedicatedEightBitLayerFrame,
    ) -> DedicatedEightBitPresentationStatus {
        match frame.source {
            EightBitLayerSource::Picture(ref picture) => match *picture {},
        }
    }

    pub(crate) fn release_canvas(&mut self) {}

    pub(crate) fn set_surface_visible(&mut self, _visible: bool) {}

    pub(crate) fn set_wake_context(&mut self, _ctx: egui::Context) {}

    pub(crate) fn take_async_status(&mut self) -> Option<DedicatedEightBitPresentationStatus> {
        None
    }

    pub(crate) fn has_presented_picture(&self) -> bool {
        false
    }

    pub(crate) fn take_telemetry(&self) -> DedicatedPresenterTelemetryDelta {
        DedicatedPresenterTelemetryDelta::default()
    }

    pub(crate) fn take_presentations(&self) -> DedicatedPresentations {
        DedicatedPresentations::default()
    }

    pub fn teardown(&mut self) {}

    pub(crate) fn layer_colour_state(&self) -> Option<LayerColourState> {
        None
    }

    pub(crate) fn last_presented(&self) -> Option<DedicatedEightBitLayerFrame> {
        None
    }
}
