// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The dedicated video layers' portable vocabulary: presentation statuses,
//! failure outcomes, presenter telemetry and frame identities. These are
//! plain data, compiled on every target; the Metal layers that produce them
//! live in `video_metal_layer` (macOS only).

/// The exact-still-image identity of a presented frame (ADR 0016 R2): the
/// presenter composites exactly the tiles valid at `fence` and reports the
/// fence presented once the GPU finished. The shared presenter seam's type.
pub use arcen_deck_core::present::RefineFrameTag;

/// The result of one attempt to establish or render through the dedicated
/// 10-bit video layer. Every non-[`Ready`][Self::Ready] variant is a
/// distinct, precise reason -- mirroring
/// [`super::video_render::ColorspaceOutcome`]'s own "always diagnosable,
/// never a single opaque failure" shape exactly -- so a fallback to the
/// existing wgpu/egui path is always explainable; see the module doc's
/// "fail safe and loud" section.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DedicatedLayerOutcome {
    /// The frame was rendered into the dedicated layer successfully.
    Ready,
    /// Not called from the main thread; every AppKit/window call here
    /// requires it.
    NotMainThread,
    /// No open `NSWindow` currently has
    /// [`crate::ui::multi_window_runtime::ROOT_WINDOW_TITLE`] as its title.
    NoRootWindow,
    /// The root window currently has no content view.
    NoContentView,
    /// `-[NSView layer]` returned `nil` (the view is not yet layer-backed).
    NoRootLayer,
    /// `MTLCreateSystemDefaultDevice()` returned `nil`.
    NoMetalDevice,
    /// `-[MTLDevice newCommandQueue]` returned `nil`.
    CommandQueueCreationFailed,
    /// `CVMetalTextureCacheCreate`-equivalent (this module's
    /// `apple_cf::cv::CVMetalTextureCache::system_default`) returned
    /// `nil`. One of the three failure categories the task's own
    /// description names explicitly ("the texture cache[...] cannot be
    /// established").
    TextureCacheCreationFailed,
    /// Compiling `video_metal_layer.metal` from source, or looking up
    /// `vs_main`/`fs_convert` within it, failed.
    ShaderCompilationFailed,
    /// `-[MTLDevice newRenderPipelineStateWithDescriptor:error:]` failed.
    PipelineStateCreationFailed,
    /// The layer or a drawable texture read back a format other than
    /// `RGB10A2Unorm` after configuration.
    PixelFormatMismatch,
    /// `CVMetalTextureCacheCreateTextureFromImage` failed (or returned a
    /// null texture) for either plane. The other of the three failure
    /// categories the task's own description names explicitly ("the
    /// texture cache[...] cannot be established" -- this is that same
    /// texture cache, failing to vend a texture rather than failing to be
    /// created at all).
    PlaneTextureCreationFailed,
    /// `-[CAMetalLayer nextDrawable]` returned `nil`. This is also the
    /// only observable symptom this module can name if
    /// `MTLPixelFormatRGB10A2Unorm` (the third of the task's three named
    /// failure categories, "the 10-bit pixel format[...] cannot be
    /// established") turns out to be unsupported: `CAMetalLayer`'s
    /// `pixelFormat` setter cannot itself fail or report rejection, so an
    /// unsupported format's only observable symptom is a drawable that
    /// never becomes available.
    NoDrawableAvailable,
    /// `-[MTLDevice newBufferWithBytes:length:options:]` (for the uniform
    /// buffer) returned `nil`.
    UniformBufferCreationFailed,
    /// `-[MTLCommandQueue commandBuffer]` returned `nil`.
    CommandBufferCreationFailed,
    /// `-[MTLCommandBuffer renderCommandEncoderWithDescriptor:]` returned
    /// `nil`.
    RenderEncoderCreationFailed,
}

/// The presentation truth the app needs for a 10-bit frame.
///
/// `FallbackToEightBit` is deliberately distinct from `Inactive`: the latter
/// means no ten-bit dedicated presentation was attempted, while the former
/// means RGB10A2Unorm was attempted and the existing 8-bit wgpu path must be
/// used for this frame.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum DedicatedPresentationStatus {
    #[default]
    Inactive,
    DedicatedTenBit,
    SkippedOccluded,
    DroppedFrame,
    FallbackToEightBit(DedicatedLayerOutcome),
}

impl DedicatedPresentationStatus {
    pub(crate) const fn from_outcome(outcome: DedicatedLayerOutcome) -> Self {
        match outcome {
            DedicatedLayerOutcome::Ready => Self::DedicatedTenBit,
            DedicatedLayerOutcome::NoDrawableAvailable => Self::DroppedFrame,
            other => Self::FallbackToEightBit(other),
        }
    }

    pub(crate) const fn is_dedicated_ten_bit(self) -> bool {
        matches!(self, Self::DedicatedTenBit)
    }

    pub(crate) const fn is_eight_bit_fallback(self) -> bool {
        matches!(self, Self::FallbackToEightBit(_))
    }

    pub(crate) const fn is_transient_drop_or_skip(self) -> bool {
        matches!(self, Self::SkippedOccluded | Self::DroppedFrame)
    }

    pub(crate) const fn is_dropped_frame(self) -> bool {
        matches!(self, Self::DroppedFrame)
    }

    pub(crate) const fn is_skipped_occluded(self) -> bool {
        matches!(self, Self::SkippedOccluded)
    }

    pub(crate) const fn preserve_established_on_transient(
        self,
        incoming: Self,
        has_presented_picture: bool,
    ) -> Self {
        if has_presented_picture
            && self.is_dedicated_ten_bit()
            && incoming.is_transient_drop_or_skip()
        {
            self
        } else {
            incoming
        }
    }

    pub(crate) const fn fallback_reason(self) -> Option<DedicatedLayerOutcome> {
        match self {
            Self::FallbackToEightBit(reason) => Some(reason),
            Self::Inactive | Self::DedicatedTenBit | Self::SkippedOccluded | Self::DroppedFrame => {
                None
            }
        }
    }
}

/// Marker set on this module's own `CAMetalLayer` sublayer (`CALayer.name`)
/// so [`super::video_render::apply_reference_colorspace`]'s sublayer
/// search -- written before this module existed, to find a *different*,
/// implicitly-created `CAMetalLayer` -- can tell the two apart and skip
/// this one. See that function's own doc for exactly why this matters.
pub(crate) const DEDICATED_VIDEO_LAYER_NAME: &str = "arcen-dedicated-video-layer";

pub(crate) const DEDICATED_EIGHT_BIT_VIDEO_LAYER_NAME: &str =
    "arcen-dedicated-eight-bit-video-layer";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum DedicatedEightBitPresentationStatus {
    #[default]
    Inactive,
    DedicatedEightBit,
    SkippedOccluded,
    DroppedFrame,
    FallbackToEgui(DedicatedLayerOutcome),
}

impl DedicatedEightBitPresentationStatus {
    pub(crate) const fn from_outcome(outcome: DedicatedLayerOutcome) -> Self {
        match outcome {
            DedicatedLayerOutcome::Ready => Self::DedicatedEightBit,
            DedicatedLayerOutcome::NoDrawableAvailable => Self::DroppedFrame,
            other => Self::FallbackToEgui(other),
        }
    }

    pub(crate) const fn is_dedicated_eight_bit(self) -> bool {
        matches!(self, Self::DedicatedEightBit)
    }

    pub(crate) const fn is_fallback(self) -> bool {
        matches!(self, Self::FallbackToEgui(_))
    }

    pub(crate) const fn is_transient_drop_or_skip(self) -> bool {
        matches!(self, Self::SkippedOccluded | Self::DroppedFrame)
    }

    pub(crate) const fn is_dropped_frame(self) -> bool {
        matches!(self, Self::DroppedFrame)
    }

    pub(crate) const fn is_skipped_occluded(self) -> bool {
        matches!(self, Self::SkippedOccluded)
    }

    pub(crate) const fn preserve_established_on_transient(
        self,
        incoming: Self,
        has_presented_picture: bool,
    ) -> Self {
        if has_presented_picture
            && self.is_dedicated_eight_bit()
            && incoming.is_transient_drop_or_skip()
        {
            self
        } else {
            incoming
        }
    }

    pub(crate) const fn fallback_reason(self) -> Option<DedicatedLayerOutcome> {
        match self {
            Self::FallbackToEgui(reason) => Some(reason),
            Self::Inactive
            | Self::DedicatedEightBit
            | Self::SkippedOccluded
            | Self::DroppedFrame => None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct DedicatedPresenterTelemetryDelta {
    pub(crate) drops: u64,
    pub(crate) skips: u64,
    /// Refreshes that found every drawable slot busy and left the next frame
    /// queued for the following refresh.
    pub(crate) slot_waits: u64,
    pub(crate) frames_received: u64,
    pub(crate) frames_submitted: u64,
    pub(crate) frames_confirmed: u64,
    pub(crate) metal_dropped: u64,
    pub(crate) presented_time_unconfirmed: u64,
    pub(crate) dropped_overflow: u64,
    pub(crate) dropped_stale: u64,
    pub(crate) dropped_hidden: u64,
    pub(crate) underruns: u64,
    pub(crate) queue_depth_samples: u64,
    pub(crate) queue_depth_total: u64,
    pub(crate) queue_depth_max: u64,
    pub(crate) interval_1x: u64,
    pub(crate) interval_2x: u64,
    pub(crate) interval_3x_plus: u64,
    pub(crate) latency_p50_us: Option<u64>,
    pub(crate) latency_p99_us: Option<u64>,
    /// Frame arrival to the drawable's confirmed on-glass time.
    pub(crate) on_glass_latency_p50_us: Option<u64>,
    pub(crate) on_glass_latency_p99_us: Option<u64>,
    pub(crate) presented_after_target_p50_us: Option<u64>,
    pub(crate) submitted_presented_at: Vec<std::time::Instant>,
    pub(crate) confirmed_presented_at: Vec<std::time::Instant>,
    pub(crate) refresh_period_us: Option<u64>,
    /// Submission to the glass, for pictures submitted while none other was
    /// in flight (the eight-bit layer under `Newest`).
    pub(crate) submit_to_glass_p50_us: Option<u64>,
    /// Pictures the layer may keep in flight, as of the last presented one.
    pub(crate) max_in_flight: Option<u64>,
}

/// A presentation layer's colour state, read back from Core Animation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LayerColourState {
    /// `wantsExtendedDynamicRangeContent`.
    pub edr: bool,
    /// The layer's colour space name, without the `kCGColorSpace` prefix.
    pub colour_space: Option<String>,
}
