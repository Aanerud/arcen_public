//! `w4-dedicated-metal-layer`: a dedicated, genuinely 10-bit `CAMetalLayer`
//! for remote video, sitting beneath `eframe`'s own egui/wgpu-compositied
//! view, bypassing that view's render pass (and its `Bgra8Unorm` swapchain)
//! entirely.
//!
//! # Why this module exists, and what it does not fix by itself
//!
//! `video_render.rs`'s own module doc (see its "swapchain itself stays
//! 8-bit" and "Unblocked 2026-08-14" sections) records why the *existing*
//! presentation surface cannot be made 10-bit: `egui-wgpu` always picks
//! `Bgra8Unorm` when it is offered, and `wgpu-hal` re-asserts it on every
//! resize, so patching the drawable in place either fights `wgpu` on every
//! frame or desyncs it from pipelines it already built against
//! `Bgra8Unorm`. That doc names the other option as "route 2": give video
//! its own `CAMetalLayer`, configured for 10-bit, entirely outside
//! `eframe`'s surface management -- UI genuinely does not need 10 bits, and
//! a dedicated layer also removes video from egui's own compositing/latency
//! path. This module is that route.
//!
//! **This module does not by itself make the dedicated layer visible.**
//! `eframe`/`egui-wgpu`'s own render pass clears and repaints the *entire*
//! window surface every frame (see `egui_wgpu::Renderer`'s own painting);
//! that surface's `CAMetalLayer` sits *above* this module's layer in the
//! same content view (as it must, to keep compositing UI chrome such as
//! window controls, menus and any future on-video overlays), and nothing
//! in this change makes egui paint transparently over the video rect. That
//! is a real, separate, cross-cutting change -- touching `NSWindow`
//! opacity, `eframe`'s own `WgpuConfiguration`/`clear_color`, and exactly
//! which UI code path is responsible for leaving the video rect
//! transparent -- and is out of scope for a change confined to
//! `src/ui/`/`Cargo.toml` (see "What a Mac still needs to verify/wire" at
//! the bottom of this doc). What *is* fully implemented and (where
//! possible) unit-tested here is everything the task specifically asked
//! for: the layer's own lifecycle, its 10-bit configuration, rendering a
//! decoded frame into it with the negotiated matrix/range, and the
//! fail-safe/fall-back decision -- all correct and ready for that
//! remaining integration step.
//!
//! # What is real today vs. what is a seam
//!
//! Exactly like [`super::video_render::RawVideoPayload::Planar16`] (see
//! that module's own "What is real today vs. what is a seam" section):
//! [`DedicatedLayerFrame`], [`DedicatedVideoLayer`] and
//! [`DedicatedVideoPresenter`] are fully implemented and wired for the root
//! Grading/HDR viewport. `video_decoder.rs` publishes the decoded
//! `CVPixelBuffer` plus its negotiated
//! [`super::video_render::VideoColorContract`], and the media worker submits
//! the newest frame to the dedicated presenter thread. The existing RGBA
//! payload is still kept as the visible fallback if the dedicated layer fails.
//!
//! # Layer lifecycle
//!
//! [`DedicatedVideoLayer::attach`] finds the root viewport's `NSWindow`
//! (via [`super::video_render::find_root_window`], shared with
//! [`super::video_render::apply_reference_colorspace`] so the two
//! independent `CAMetalLayer`-reaching paths this crate now has can never
//! disagree about which window is "the root viewport"), gets its content
//! view, marks it layer-backed, and adds a new `CAMetalLayer` as a
//! **sibling** sublayer of whatever `wgpu` has already attached there (see
//! "What this does not fix" above for why it must stay a sibling, not a
//! replacement). That new layer is tagged
//! [`DEDICATED_VIDEO_LAYER_NAME`] via `CALayer.name` for exactly one
//! reason: so [`super::video_render::apply_reference_colorspace`]'s own
//! sublayer search -- written before this module existed, to find `wgpu`'s
//! *different*, implicitly-created `CAMetalLayer` -- does not
//! misidentify this one as that one (or vice versa) purely because both
//! are `CAMetalLayer`s; see that function's own doc for the exact
//! mechanics. The main-thread attach/resize path repositions/resizes the
//! layer's `frame` (in the content view's own coordinate space -- *not*
//! `drawableSize`, which [`DedicatedVideoLayer::render`] instead sizes to
//! the source video's own native pixel dimensions every frame, letting
//! Core Animation's own GPU compositor do the final scale-to-fit onto
//! `frame`, exactly like any other image-backed `CALayer`; see
//! `video_metal_layer.metal`'s own module doc for why that makes this
//! shader single-pass). Teardown is `Drop`: `-[CALayer removeFromSuperlayer]`,
//! with every other Metal resource released by its own `Retained`/`CVBuffer`
//! handling as this struct's fields drop in turn.
//!
//! # 10-bit configuration
//!
//! `attach` sets, once, at creation:
//!
//! - `pixelFormat = MTLPixelFormatRGB10A2Unorm`. **The task brief that
//!   named this task cited the raw value `552` for this constant; that is
//!   wrong, and this is deliberately flagged rather than silently
//!   "corrected" without comment.** Read directly from the vendored
//!   `objc2-metal-0.3.2/src/generated/MTLPixelFormat.rs`:
//!   `MTLPixelFormatRGB10A2Unorm` is `90`; `552` is
//!   `MTLPixelFormatBGRA10_XR`, an entirely different (and EDR-oriented)
//!   extended-range format. This module always uses the typed
//!   `MTLPixelFormat::RGB10A2Unorm` constant, never a hand-written integer,
//!   specifically so this kind of transcription error cannot recur silently
//!   -- and [`tests::rgb10a2unorm_is_90_not_the_552_the_task_brief_cited`]
//!   pins the correct value against exactly this regression.
//! - `framebufferOnly = true`: this layer is presented, never read back or
//!   used as a compute/blit source, which is the case `framebufferOnly`
//!   exists to optimise (Apple's own documented guidance: set it whenever
//!   the drawable's texture is only ever a render-pass attachment).
//! - `wantsExtendedDynamicRangeContent`, and `EDRMetadata`, driven by the
//!   **negotiated transfer function** and nothing else. `Pq` turns EDR on,
//!   tags the layer `kCGColorSpaceITUR_2100_PQ`, and attaches
//!   `CAEDRMetadata.HDR10MetadataWithMinLuminance:maxLuminance:opticalOutputScale:`;
//!   every other transfer leaves EDR off and the layer on an SDR working
//!   space. This is SDR 10-bit *reference* viewing by default -- absorbing
//!   RGB<->YCbCr rounding error, not displaying a wider dynamic range --
//!   and only becomes HDR when the host says the stream genuinely is.
//!   Deliberately **not** keyed on bit depth: `Grading Reference` is
//!   4:4:4 ten-bit BT.709 and entirely SDR, so a depth-keyed rule would
//!   light EDR up for a colour-critical SDR session and have macOS
//!   tone-map it against a 1000-nit curve. See
//!   [`super::video_render::presentation_colorspace_for`], whose own tests
//!   pin exactly that distinction.
//! - `colorspace`, from the negotiated [`arcen_media::ColorPrimaries`] via
//!   [`super::video_render::reference_colorspace_for`] (reused directly, not
//!   reimplemented, so the two independent presentation paths this crate now
//!   has can never disagree about which colour space a given `ColorPrimaries`
//!   maps to) -- but through the *typed* `objc2-core-graphics` `CGColorSpace`
//!   (`with_name`/`kCGColorSpaceSRGB`/`kCGColorSpaceDisplayP3`), not
//!   `apple_cf::cg::CGColorSpace`. `video_render.rs`'s own
//!   `apply_reference_colorspace` needed the latter (and raw `msg_send!`
//!   throughout) specifically because it had to reach an *implicitly*
//!   created layer without adding a new Cargo dependency (see that
//!   function's own doc). This module creates its own layer outright, so it
//!   can and does depend on `objc2-quartz-core`/`objc2-metal` directly and
//!   use every typed accessor `CAMetalLayer` exposes -- no raw `msg_send!`
//!   anywhere in this file.
//!
//! # Rendering a frame
//!
//! [`DedicatedVideoLayer::render`] takes a real, negotiated
//! [`DedicatedLayerFrame`] (a [`CVPixelBuffer`][apple_cf::cv::CVPixelBuffer]
//! plus its [`super::video_render::VideoColorContract`]) and, with **no CPU
//! copy of the pixel data at any point**:
//!
//! 1. Wraps each plane (luma, and the interleaved Cb/Cr plane) as an
//!    `MTLTexture` via `CVMetalTextureCacheCreateTextureFromImage` --
//!    [`DedicatedVideoLayer::create_plane_texture`] -- at the
//!    [`PlanePixelFormatPlan`] [`plane_pixel_formats`] derives for the
//!    negotiated [`arcen_media::BitDepth`].
//! 2. Builds [`MetalVideoUniform`] -- the matrix/range/identity uniform,
//!    built by calling
//!    [`super::video_render::VideoUniform::from_contract`] **directly**
//!    (not a re-derivation: see that struct's own doc for exactly why this
//!    guarantees the two shaders' colour maths can never silently diverge
//!    on the shared fields) and appending one Metal-only field, described
//!    below.
//! 3. Requests the layer's next drawable and records one render pass: the
//!    single `fs_convert` fragment function in `video_metal_layer.metal`
//!    (its own module doc records the full derivation this comment only
//!    summarises) converts YCbCr/GBR straight into that drawable's
//!    `Rgb10a2Unorm` texture, and the command buffer presents and commits.
//!
//! ## The one thing WGSL does not need: Unorm reconstruction
//!
//! `video_render.wgsl` reads its plane textures as raw integer codes
//! (`texture_2d<u32>`, `textureLoad`), because `video_render.rs` uploads
//! CPU-side `u16` bytes into textures it creates itself in whatever format
//! it likes. This module's planes are real `CVPixelBuffer` IOSurfaces,
//! which are only Metal-compatible as `Unorm` views -- there is no way to
//! ask `CVMetalTextureCacheCreateTextureFromImage` for an *integer* view of
//! the same plane. A `read()` from an `Unorm` texture is therefore a
//! **normalised float**, and undoing that normalisation back to the coded
//! ITU value is not simply "multiply by the max code" once depth exceeds
//! eight bits, because CoreVideo's ten/twelve-bit biplanar formats MSB-align
//! the code inside a 16-bit container while Metal's `Unorm` read always
//! normalises by the *full* 16-bit range. [`plane_pixel_formats`] derives
//! and unit-tests the exact scale used to reverse that representation.
//!
//! # Cargo dependencies added
//!
//! Four new *direct* dependencies of `arcen-deck-macos`, all from the same
//! `objc2` project (`https://github.com/madsmtm/objc2`, licence `Zlib OR
//! Apache-2.0 OR MIT` -- identical licensing to `objc2`/`objc2-app-kit`/
//! `objc2-foundation`, already direct dependencies of this crate) and all
//! pinned at `0.3.2`, the exact version already resolved in `Cargo.lock`
//! for every one of them (confirmed directly against the checked-in
//! `Cargo.lock`, not assumed): `objc2-metal` and `objc2-quartz-core` are
//! pulled there today only *transitively*, by `wgpu-hal` 29.0.4's own
//! `Cargo.toml` (see `video_render.rs`'s own module doc, which documents
//! this exact transitive-vs-direct distinction at length for the identical
//! pair of crates); `objc2-core-foundation`/`objc2-core-graphics` are pulled
//! transitively by several existing dependencies already. None of the four
//! requires a new package version or a new package entry in `Cargo.lock` --
//! only new dependency *edges* onto packages already resolved there (see
//! this task's own final report for the exact `cargo metadata --locked`
//! evidence). `clients/macos/Cargo.toml`'s existing `objc2-app-kit`
//! dependency additionally gains one more already-optional feature of its
//! own, `"objc2-quartz-core"`, which is what turns
//! `NSView::layer()`/`setLayer()` (used by the dedicated layer attach paths)
//! from absent into a typed, safe accessor -- see
//! `video_render.rs`'s own "Unblocked 2026-08-14" section for the identical
//! feature-gating fact already discovered for a different accessor.
//!
//! # Fail safe and loud
//!
//! [`DedicatedVideoPresenter`] is the entry point the app installs and the
//! media worker feeds: the app attaches/resizes it on the main thread, then
//! decoded frames are rendered by a presenter thread using a latest-frame
//! mailbox. When every attempt fails, the worker disables the dedicated path
//! and the existing egui/wgpu path remains the visible fallback. Every
//! distinct failure reason ([`DedicatedLayerOutcome`]) is logged at `warn`,
//! naming what failed,
//! at most once per distinct reason -- and success is logged once at
//! `info` -- via [`DedicatedLayerFallback`], which mirrors
//! [`super::video_render::ColorspaceApplication`]'s own log-once-per-reason
//! shape exactly (see that type's own doc). A failure at any step during
//! `render` tears the layer down (`self.layer = None`, invoking `Drop`) so
//! the *next* call re-attaches cleanly rather than silently limping along
//! against a possibly-wedged layer. Nothing in this module ever panics on
//! a runtime/data condition (only `debug_assert_eq!` on this module's own
//! internal byte-layout arithmetic, identical in spirit to
//! `video_render.rs`'s own `VideoUniform::to_bytes`).
//!
//! # Compile status
//!
//! **None of this has been compiled, type-checked, or run.** Exactly like
//! `video_render.rs` (see its own identical disclaimer): this is macOS-only
//! code edited from Windows, where only `rustfmt --edition 2024` (a parse
//! check) is available. Every `objc2`/`objc2-metal`/`objc2-quartz-core`/
//! `objc2-core-foundation`/`objc2-core-graphics`/`apple-cf` API used here
//! was read directly out of the vendored registry sources at the exact
//! pinned versions (`objc2` 0.6.4, `objc2-app-kit`/`objc2-foundation`/
//! `objc2-metal`/`objc2-quartz-core`/`objc2-core-foundation`/
//! `objc2-core-graphics` 0.3.2, `apple-cf` 0.9.3) rather than assumed, and
//! every piece of pure data/derivation logic
//! ([`plane_pixel_formats`]/[`PlanePixelFormatPlan`],
//! [`MetalVideoUniform`]'s byte layout, [`DedicatedLayerFallback`]'s
//! log-once decision, the `RGB10A2Unorm` value correction) has a unit test
//! below -- but nothing here has been exercised end to end. Specific,
//! narrower points of remaining doubt, beyond the blanket disclaimer above:
//!
//! - Whether `ProtocolObject::from_ref(&*drawable)` (in
//!   [`DedicatedVideoLayer::render`], coercing the `nextDrawable()` result
//!   from `&ProtocolObject<dyn CAMetalDrawable>` to the
//!   `&ProtocolObject<dyn MTLDrawable>` supertrait reference
//!   `presentDrawable` expects) resolves its generic parameter the way this
//!   module assumes.
//!   `newBufferWithBytes_length_options`) resolve; this module wrote the
//!   trait-import list by inspecting every method call site by hand, not
//!   from a compiler error.
//! - Whether `CAMetalLayer`'s `Deref`-to-`CALayer` chain (used for
//!   `setFrame`/`setName`/`removeFromSuperlayer`/`addSublayer`'s argument)
//!   coerces exactly the way `video_render.rs`'s own, differently-shaped
//!   `objc2` usage already establishes elsewhere in this crate.
//! - The exact reconstruction formula in the "Unorm reconstruction" section
//!   above is derived from first principles (documented, tested) but is
//!   **not cross-checked against a real decoded `xf44` frame on real
//!   hardware** -- that is precisely the kind of claim
//!   `docs/architecture/color-fidelity.md`'s own "hardware testing is a
//!   gate rather than a formality" lesson exists to catch.
//!
//! # What a Mac still needs to verify/wire
//!
//! 1. Compile, run, and confirm every point in "Compile status" above.
//! 2. Wire the actual seam: teach `video_decoder.rs` to additionally hand
//!    back a real `CVPixelBuffer` (see "What is real vs. a seam" above),
//!    construct [`DedicatedLayerFrame`] from it, and call
//!    [`DedicatedVideoPresenter::try_paint`] from wherever
//!    [`super::video_render::RemoteVideoFrame::paint`] is currently invoked
//!    -- skipping that call when `try_paint` returns `true`.
//! 3. Make the video rect **actually visible**: egui's own render pass
//!    currently paints the whole window opaquely every frame, so this
//!    layer -- even once wired in and rendering correctly -- is presently
//!    hidden behind it. Punching a transparent hole for the video rect
//!    (window/`NSView`/`CAMetalLayer` opacity, and whatever `eframe`/
//!    `egui` app code currently paints a background there) is a distinct,
//!    cross-cutting change outside `src/ui/`'s narrower "own the video
//!    layer" scope this task asked for; see "What this module does not fix
//!    by itself" above.
//! 4. Confirm on real hardware that `CVMetalTextureCache::system_default()`
//!    (used by [`DedicatedVideoLayer::attach`]) and this module's own
//!    `MTLCreateSystemDefaultDevice()` call resolve to the *same* Metal
//!    device. On every Apple Silicon Mac (the only hardware
//!    `docs/architecture/color-fidelity.md` reports this feature actually
//!    tested against) there is exactly one GPU, so this is not a live
//!    concern there; an Intel Mac with automatic graphics switching between
//!    two GPUs is the one configuration where these two independent calls
//!    could theoretically disagree, and this module does not defend
//!    against that (seem `create_plane_texture`'s own doc comment).
//! 5. The layer explicitly keeps Core Animation's resize gravity and filtered
//!    minification/magnification so `drawableSize` remains the source's native
//!    resolution while the compositor, not this shader's integer-coordinate
//!    `texture.read()`, performs final viewport scaling.

use core::ffi::c_void;
use core::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use arcen_media::presentation::{
    FramePacer, FramePacerCounters, FramePacerDropReason, FramePacerRefresh,
};
use block2::RcBlock;
use objc2::rc::{autoreleasepool, Retained};
use objc2::runtime::ProtocolObject;
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_core_graphics::{
    kCGColorSpaceDisplayP3, kCGColorSpaceITUR_2100_PQ, kCGColorSpaceSRGB, CGColorSpace,
    CGDirectDisplayID,
};
use objc2_core_video::{
    kCVReturnSuccess, CVDisplayLink, CVGetCurrentHostTime, CVGetHostClockFrequency, CVOptionFlags,
    CVReturn, CVTimeStamp,
};
use objc2_foundation::{MainThreadMarker, NSNumber, NSString};
use objc2_metal::{
    MTLBuffer, MTLClearColor, MTLCommandBuffer, MTLCommandEncoder, MTLCommandQueue,
    MTLCreateSystemDefaultDevice, MTLDevice, MTLDrawable, MTLLibrary, MTLLoadAction,
    MTLPixelFormat, MTLPrimitiveType, MTLRenderCommandEncoder, MTLRenderPassDescriptor,
    MTLRenderPipelineDescriptor, MTLRenderPipelineState, MTLResourceOptions, MTLStoreAction,
    MTLTexture,
};
use objc2_quartz_core::{
    kCAFilterLinear, kCAFilterTrilinear, kCAGravityResize, CAEDRMetadata, CAMetalDrawable,
    CAMetalLayer, CATransaction,
};

use super::video_render::{
    presentation_colorspace_for, PresentationColorSpace, ReferenceColorSpace, VideoColorContract,
    VideoUniform,
};

/// The mastering luminance HDR10 is signalled against when the stream
/// carries no ST 2086 mastering-display metadata of its own -- which
/// Arcen's never does: a desktop is synthetic content with no colourist and
/// no mastering monitor behind it. These are the reference HDR10 grade, and
/// also what Windows composites its own `AdvancedColor` desktop against, so
/// a desktop captured in scRGB and encoded to PQ is already effectively
/// graded to them.
const HDR10_MIN_LUMINANCE_NITS: f32 = 0.005;
const HDR10_MAX_LUMINANCE_NITS: f32 = 1000.0;
/// Apple requires normalized PQ pixel formats to use the ST 2084 reference
/// peak as their optical-output scale: normalized code `1.0` means 10,000 nits.
const HDR10_NORMALIZED_OPTICAL_OUTPUT_SCALE: f32 = 10_000.0;
const FALLBACK_REFRESH_PERIOD: Duration = Duration::from_nanos(16_666_667);

fn host_clock_frequency() -> f64 {
    let frequency = CVGetHostClockFrequency();
    if frequency.is_finite() && frequency > 0.0 {
        frequency
    } else {
        1_000_000_000.0
    }
}

fn host_ticks_to_duration(ticks: u64) -> Duration {
    Duration::from_secs_f64(ticks as f64 / host_clock_frequency())
}

pub(crate) fn current_host_time() -> Duration {
    host_ticks_to_duration(CVGetCurrentHostTime())
}

fn display_id_for_window(window: &objc2_app_kit::NSWindow) -> Option<CGDirectDisplayID> {
    let key = NSString::from_str("NSScreenNumber");
    let screen = window.screen()?;
    let number = screen.deviceDescription().objectForKey(&key)?;
    let number = number.downcast::<NSNumber>().ok()?;
    Some(number.as_u32())
}

fn log_window_presenter_state(
    window: &objc2_app_kit::NSWindow,
    display_id: Option<CGDirectDisplayID>,
    refresh_period: Option<Duration>,
    reason: &str,
) {
    tracing::debug!(
        target: crate::logging::target::VIDEO,
        reason,
        occlusion_state = window.occlusionState().0,
        visible = window.occlusionState().contains(objc2_app_kit::NSWindowOcclusionState::Visible),
        key = window.isKeyWindow(),
        main = window.isMainWindow(),
        window_visible = window.isVisible(),
        screen = %window.screen().map(|screen| screen.localizedName().to_string()).unwrap_or_else(|| "none".to_string()),
        display_id = ?display_id,
        refresh_hz = ?refresh_period.map(|period| 1.0 / period.as_secs_f64()),
        "dedicated video presenter window state",
    );
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct DedicatedLayerGeometry {
    rect: CGRect,
    contents_scale: f64,
}

impl DedicatedLayerGeometry {
    fn new(rect: CGRect, contents_scale: f64) -> Self {
        Self {
            rect,
            contents_scale: normalized_contents_scale(contents_scale),
        }
    }
}

fn normalized_contents_scale(contents_scale: f64) -> f64 {
    if contents_scale.is_finite() && contents_scale > 0.0 {
        contents_scale
    } else {
        1.0
    }
}

fn source_drawable_size(width: usize, height: usize) -> CGSize {
    CGSize {
        width: width as f64,
        height: height as f64,
    }
}

trait DedicatedLayerGeometryTarget {
    fn begin_geometry_transaction(&self);
    fn set_geometry_actions_disabled(&self, disabled: bool);
    fn set_geometry_contents_scale(&self, contents_scale: f64);
    fn set_geometry_frame(&self, rect: CGRect);
    fn commit_geometry_transaction(&self);
}

impl DedicatedLayerGeometryTarget for CAMetalLayer {
    fn begin_geometry_transaction(&self) {
        CATransaction::begin();
    }

    fn set_geometry_actions_disabled(&self, disabled: bool) {
        CATransaction::setDisableActions(disabled);
    }

    fn set_geometry_contents_scale(&self, contents_scale: f64) {
        self.setContentsScale(contents_scale);
    }

    fn set_geometry_frame(&self, rect: CGRect) {
        self.setFrame(rect);
    }

    fn commit_geometry_transaction(&self) {
        CATransaction::commit();
    }
}

fn apply_dedicated_layer_geometry(
    layer: &impl DedicatedLayerGeometryTarget,
    geometry: DedicatedLayerGeometry,
) {
    layer.begin_geometry_transaction();
    layer.set_geometry_actions_disabled(true);
    layer.set_geometry_contents_scale(geometry.contents_scale);
    layer.set_geometry_frame(geometry.rect);
    layer.commit_geometry_transaction();
}

trait DedicatedLayerDrawableTarget {
    fn begin_drawable_transaction(&self);
    fn set_drawable_actions_disabled(&self, disabled: bool);
    fn set_source_drawable_size(&self, drawable_size: CGSize);
    fn commit_drawable_transaction(&self);
    fn flush_drawable_transaction(&self);
}

impl DedicatedLayerDrawableTarget for CAMetalLayer {
    fn begin_drawable_transaction(&self) {
        CATransaction::begin();
    }

    fn set_drawable_actions_disabled(&self, disabled: bool) {
        CATransaction::setDisableActions(disabled);
    }

    fn set_source_drawable_size(&self, drawable_size: CGSize) {
        self.setDrawableSize(drawable_size);
    }

    fn commit_drawable_transaction(&self) {
        CATransaction::commit();
    }

    fn flush_drawable_transaction(&self) {
        CATransaction::flush();
    }
}

fn apply_dedicated_layer_source_drawable_size(
    layer: &impl DedicatedLayerDrawableTarget,
    drawable_size: CGSize,
) {
    layer.begin_drawable_transaction();
    layer.set_drawable_actions_disabled(true);
    layer.set_source_drawable_size(drawable_size);
    layer.commit_drawable_transaction();
    layer.flush_drawable_transaction();
}

fn configure_dedicated_layer_scaling(layer: &CAMetalLayer) {
    // SAFETY: These are immutable QuartzCore framework constants with static
    // lifetime; the setters copy the NSString values.
    layer.setContentsGravity(unsafe { kCAGravityResize });
    layer.setMagnificationFilter(unsafe { kCAFilterLinear });
    layer.setMinificationFilter(unsafe { kCAFilterTrilinear });
}

// ============================================================================
// The seam: a real decoded frame, negotiated contract attached
// ============================================================================

/// A negotiated-format decoded video frame backed directly by a
/// `CVPixelBuffer`, consumed by [`DedicatedVideoLayer::render`]. This is
/// the seam a future `video_decoder.rs` change wires into -- see this
/// module's own "What is real today vs. what is a seam" doc section; no
/// production code path constructs one today.
#[derive(Debug, Clone)]
pub struct DedicatedLayerFrame {
    /// The decoded, biplanar (or, for [`arcen_media::ColorMatrix::Identity`],
    /// biplanar-shaped GBR) `CVPixelBuffer` -- `xf44` for the target format
    /// (HEVC Main 4:4:4 10-bit full range). Plane 0 is luma (or G), plane 1
    /// is the interleaved Cb/Cr (or B/R) pair, matching
    /// [`super::video_render`]'s own plane-layout convention exactly.
    pub pixel_buffer: apple_cf::cv::CVPixelBuffer,
    /// The negotiated chroma/range/depth/matrix/primaries this frame was
    /// actually decoded with.
    pub contract: VideoColorContract,
    /// Frame arrival time in CoreVideo/Metal host-clock time.
    pub arrival_host_time: Duration,
    /// Negotiated source cadence used only for underrun accounting.
    pub source_fps: Option<u32>,
}

/// A decoded 8-bit frame for the Auto/Speed layer. This is deliberately
/// separate from [`DedicatedLayerFrame`]: Auto/Speed must not be routed through
/// the 10-bit RGB10A2 layer even though both paths share the same CoreVideo
/// texture-cache and shader helpers.
#[derive(Debug, Clone)]
pub struct DedicatedEightBitLayerFrame {
    pub pixel_buffer: apple_cf::cv::CVPixelBuffer,
    pub contract: VideoColorContract,
    pub arrival_host_time: Duration,
    pub source_fps: Option<u32>,
}

// ============================================================================
// Pure logic: plane pixel-format selection + Unorm reconstruction
// ============================================================================

/// The `CVMetalTextureCacheCreateTextureFromImage` pixel-format pair (luma,
/// chroma) for a biplanar `CVPixelBuffer` at a given
/// [`arcen_media::BitDepth`], plus the factor that reconstructs the
/// original ITU-R code from Metal's `Unorm`-normalised plane read. See this
/// module's own "Unorm reconstruction" doc section and
/// `video_metal_layer.metal`'s module doc for the full derivation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct PlanePixelFormatPlan {
    pub(crate) luma_format: MTLPixelFormat,
    pub(crate) chroma_format: MTLPixelFormat,
    pub(crate) code_unnormalize_scale: f32,
}

/// Derives [`PlanePixelFormatPlan`] for `depth`. See the module doc's
/// "Unorm reconstruction" section for why eight bits is a distinct case
/// from ten/twelve.
/// How often [`DedicatedVideoLayer::log_plane_statistics`] samples again.
const PLANE_STATISTICS_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

/// The number of different codes among `values`, each shifted down by
/// `storage_shift` from its MSB-aligned storage to its code.
fn distinct_codes(values: &[u16], storage_shift: u32) -> usize {
    let mut seen = std::collections::BTreeSet::new();
    for value in values {
        seen.insert(value >> storage_shift);
    }
    seen.len()
}

pub(crate) fn plane_pixel_formats(depth: arcen_media::BitDepth) -> PlanePixelFormatPlan {
    match depth {
        // CoreVideo's eight-bit biplanar formats ('444v'/'444f' and
        // friends) are a native 8-bit-per-component IOSurface layout -- no
        // MSB alignment to undo, so an 8-bit `Unorm` read is already
        // exactly `code / 255.0`.
        arcen_media::BitDepth::Eight => PlanePixelFormatPlan {
            luma_format: MTLPixelFormat::R8Unorm,
            chroma_format: MTLPixelFormat::RG8Unorm,
            code_unnormalize_scale: 255.0,
        },
        // Ten-bit biplanar samples are MSB-aligned: `raw16 = code << 6`.
        arcen_media::BitDepth::Ten => PlanePixelFormatPlan {
            luma_format: MTLPixelFormat::R16Unorm,
            chroma_format: MTLPixelFormat::RG16Unorm,
            code_unnormalize_scale: 65535.0 / 64.0,
        },
        // The same MSB-alignment convention at twelve bits: `raw16 = code << 4`.
        arcen_media::BitDepth::Twelve => PlanePixelFormatPlan {
            luma_format: MTLPixelFormat::R16Unorm,
            chroma_format: MTLPixelFormat::RG16Unorm,
            code_unnormalize_scale: 65535.0 / 16.0,
        },
    }
}

// ============================================================================
// Pure logic: the Metal shader uniform
// ============================================================================

/// The Metal-side twin of [`VideoUniform`]: the exact same twelve scalar
/// fields, obtained by calling [`VideoUniform::from_contract`] directly
/// (not re-derived -- see this module's own "Rendering a frame" doc section
/// for why that guarantees the two shaders' colour maths can never
/// silently diverge on these shared fields), plus one Metal-only
/// thirteenth field this struct alone adds. See `video_metal_layer.metal`'s
/// module doc for exactly why Metal needs that extra field and WGSL does
/// not.
#[derive(Debug, Clone, Copy, PartialEq)]
struct MetalVideoUniform {
    shared: VideoUniform,
    /// See [`PlanePixelFormatPlan::code_unnormalize_scale`]'s own doc.
    code_unnormalize_scale: f32,
}

impl MetalVideoUniform {
    fn from_contract(
        contract: VideoColorContract,
        luma_size: (u32, u32),
        chroma_size: (u32, u32),
        code_unnormalize_scale: f32,
    ) -> Self {
        Self {
            shared: VideoUniform::from_contract(contract, luma_size, chroma_size),
            code_unnormalize_scale,
        }
    }

    /// Byte layout matching `struct VideoUniform` in
    /// `video_metal_layer.metal` field-for-field: the same twelve 4-byte
    /// scalars [`VideoUniform::to_bytes`] produces, with this struct's own
    /// thirteenth `f32` appended -- 52 bytes total, still a flat run of
    /// plain 4-byte scalars needing no padding (see that method's own doc
    /// for why: MSL, like WGSL, only forces extra alignment on
    /// vector/struct/array *members*, none of which appear in this
    /// uniform).
    fn to_bytes(self) -> Vec<u8> {
        let mut bytes = self.shared.to_bytes();
        bytes.extend_from_slice(&self.code_unnormalize_scale.to_le_bytes());
        debug_assert_eq!(bytes.len(), 52);
        bytes
    }
}

// ============================================================================
// Pure logic: fail-safe outcome + log-once fallback bookkeeping
// ============================================================================

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
    const fn from_outcome(outcome: DedicatedLayerOutcome) -> Self {
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

/// Log-once-per-distinct-reason bookkeeping for [`DedicatedVideoPresenter`],
/// mirroring [`super::video_render::ColorspaceApplication`]'s identical
/// shape and rationale exactly (see that type's own doc): a persistent
/// failure logs once, a change to a *different* reason (including a change
/// to/from success) logs again, and an identical repeat is silent.
/// Deliberately a standalone, plain-data type with no AppKit/Metal in it,
/// so -- like `ColorspaceApplication` -- this exact "fallback decision" is
/// unit-tested directly, even though [`DedicatedVideoLayer`] itself cannot
/// be (it needs a live `NSApplication`/window/`MTLDevice`).
#[derive(Debug, Default)]
pub(crate) struct DedicatedLayerFallback {
    last_logged: Option<DedicatedLayerOutcome>,
}

impl DedicatedLayerFallback {
    /// Returns `Some(outcome)` exactly when this is new information worth
    /// logging: the first call ever, or a change from the last-logged
    /// outcome. Returns `None` on a repeat of the identical outcome, so a
    /// caller logging whatever this returns never spams an identical line
    /// every frame at 60-120Hz.
    pub(crate) fn record(
        &mut self,
        outcome: DedicatedLayerOutcome,
    ) -> Option<DedicatedLayerOutcome> {
        if self.last_logged == Some(outcome) {
            return None;
        }
        self.last_logged = Some(outcome);
        Some(outcome)
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

fn build_pipeline_state(
    device: &ProtocolObject<dyn MTLDevice>,
    color_format: MTLPixelFormat,
) -> Result<Retained<ProtocolObject<dyn MTLRenderPipelineState>>, DedicatedLayerOutcome> {
    let source = NSString::from_str(include_str!("video_metal_layer.metal"));
    let library = device
        .newLibraryWithSource_options_error(&source, None)
        .map_err(|_| DedicatedLayerOutcome::ShaderCompilationFailed)?;
    let vertex_function = library
        .newFunctionWithName(&NSString::from_str("vs_main"))
        .ok_or(DedicatedLayerOutcome::ShaderCompilationFailed)?;
    let fragment_function = library
        .newFunctionWithName(&NSString::from_str("fs_convert"))
        .ok_or(DedicatedLayerOutcome::ShaderCompilationFailed)?;

    let descriptor = MTLRenderPipelineDescriptor::new();
    descriptor.setVertexFunction(Some(&*vertex_function));
    descriptor.setFragmentFunction(Some(&*fragment_function));
    // SAFETY: index 0 is always a valid colour-attachment slot; every Metal
    // device supports at least one.
    let color_attachment = unsafe { descriptor.colorAttachments().objectAtIndexedSubscript(0) };
    color_attachment.setPixelFormat(color_format);

    device
        .newRenderPipelineStateWithDescriptor_error(&descriptor)
        .map_err(|_| DedicatedLayerOutcome::PipelineStateCreationFailed)
}

// ============================================================================
// Live AppKit/Metal code (not unit-tested; see the module doc)
// ============================================================================

/// Owns the dedicated 10-bit `CAMetalLayer` and every Metal resource its
/// rendering needs. See the module doc's "Layer lifecycle",
/// "10-bit configuration" and "Rendering a frame" sections for what each
/// method below does and why.
pub struct DedicatedVideoLayer {
    layer: Retained<CAMetalLayer>,
    device: Retained<ProtocolObject<dyn MTLDevice>>,
    command_queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    pipeline_state: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    texture_cache: apple_cf::cv::CVMetalTextureCache,
    last_colorspace: Option<PresentationColorSpace>,
    in_flight: Arc<Mutex<LayerInFlightGate>>,
    /// When the decoded planes were last sampled for the log.
    plane_statistics_logged_at: Option<std::time::Instant>,
}

impl DedicatedVideoLayer {
    /// Creates the layer, configures it for 10-bit SDR reference viewing,
    /// and attaches it as a sublayer of the root viewport's content view,
    /// positioned at `rect` (in that view's own coordinate space -- the
    /// caller is responsible for any conversion from egui/window
    /// coordinates; see the module doc's "What a Mac still needs to
    /// verify/wire" section).
    pub fn attach(rect: CGRect) -> Result<Self, DedicatedLayerOutcome> {
        let mtm = MainThreadMarker::new().ok_or(DedicatedLayerOutcome::NotMainThread)?;
        let window = super::video_render::find_root_window(mtm)
            .ok_or(DedicatedLayerOutcome::NoRootWindow)?;
        let view = window
            .contentView()
            .ok_or(DedicatedLayerOutcome::NoContentView)?;
        view.setWantsLayer(true);
        let root_layer = view.layer().ok_or(DedicatedLayerOutcome::NoRootLayer)?;

        let device = MTLCreateSystemDefaultDevice().ok_or(DedicatedLayerOutcome::NoMetalDevice)?;
        let command_queue = device
            .newCommandQueue()
            .ok_or(DedicatedLayerOutcome::CommandQueueCreationFailed)?;
        // See the module doc's "What a Mac still needs to verify/wire"
        // item 4: this asks CoreVideo for *a* system-default Metal device's
        // texture cache rather than explicitly this `device`, because
        // `apple-cf` 0.9.3 wraps only `CVMetalTextureCacheCreate`'s
        // system-default convenience, not the general
        // `CVMetalTextureCacheCreate(..., metalDevice, ...)` form. On every
        // single-GPU Mac (every Apple Silicon Mac) these are unconditionally
        // the same device.
        let texture_cache = apple_cf::cv::CVMetalTextureCache::system_default()
            .ok_or(DedicatedLayerOutcome::TextureCacheCreationFailed)?;
        let pipeline_state = build_pipeline_state(&device, MTLPixelFormat::RGB10A2Unorm)?;

        let layer = CAMetalLayer::new();
        layer.setDevice(Some(&*device));
        layer.setPixelFormat(MTLPixelFormat::RGB10A2Unorm);
        if layer.pixelFormat() != MTLPixelFormat::RGB10A2Unorm {
            return Err(DedicatedLayerOutcome::PixelFormatMismatch);
        }
        layer.setFramebufferOnly(true);
        layer.setDisplaySyncEnabled(true);
        layer.setPresentsWithTransaction(false);
        layer.setWantsExtendedDynamicRangeContent(false);
        layer.setOpaque(true);
        configure_dedicated_layer_scaling(&layer);
        layer.setName(Some(&NSString::from_str(DEDICATED_VIDEO_LAYER_NAME)));
        apply_dedicated_layer_geometry(
            &*layer,
            DedicatedLayerGeometry::new(rect, window.backingScaleFactor()),
        );
        root_layer.insertSublayer_atIndex(&layer, 0);

        Ok(Self {
            layer,
            device,
            command_queue,
            pipeline_state,
            texture_cache,
            last_colorspace: None,
            in_flight: Arc::new(Mutex::new(LayerInFlightGate::default())),
            plane_statistics_logged_at: None,
        })
    }

    /// Compiles `video_metal_layer.metal` from source and builds the
    /// render-pipeline state targeting `Rgb10a2Unorm`. Runtime source
    /// compilation (`-[MTLDevice newLibraryWithSource:options:error:]`),
    /// not a precompiled `.metallib`, because this task's constraints do
    /// not permit editing `build.rs` (outside `src/ui/`/`Cargo.toml`) to add
    /// an `xcrun metal`/`metallib` step.
    /// Applies the presentation colour space, EDR flag and HDR metadata if
    /// the negotiated primaries/transfer map to a different
    /// [`PresentationColorSpace`] than the one last applied. Unlike
    /// `video_render::ColorspaceApplication` (which retries every frame
    /// until an implicitly-created layer is even found), this layer is
    /// directly owned from the moment [`Self::attach`] succeeds, so there
    /// is no "not found yet" failure mode to retry against -- only "did the
    /// desired choice change".
    ///
    /// Returns the applied space when it changed this call, so the caller
    /// can log the switch exactly once per change rather than per frame.
    fn ensure_colorspace(
        &mut self,
        primaries: arcen_media::ColorPrimaries,
        transfer: arcen_media::TransferCharacteristics,
    ) -> Option<PresentationColorSpace> {
        let desired = presentation_colorspace_for(primaries, transfer);
        if self.last_colorspace == Some(desired) {
            return None;
        }
        // SAFETY (not actually unsafe, just worth noting): these are all
        // `extern "C"` statics, hence the `unsafe` blocks reading them --
        // reading a well-known, always-valid system framework constant, not
        // a soundness-sensitive operation.
        let colorspace = match desired {
            PresentationColorSpace::Sdr(ReferenceColorSpace::Srgb) => {
                CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB }))
            }
            PresentationColorSpace::Sdr(ReferenceColorSpace::DisplayP3) => {
                CGColorSpace::with_name(Some(unsafe { kCGColorSpaceDisplayP3 }))
            }
            PresentationColorSpace::Hdr10Pq => {
                CGColorSpace::with_name(Some(unsafe { kCGColorSpaceITUR_2100_PQ }))
            }
        };
        let Some(colorspace) = colorspace else {
            // Never expected for these built-in system spaces, but
            // `CGColorSpaceCreateWithName` gives no infallible constructor.
            // Deliberately does *not* update `last_colorspace`, so the next
            // frame retries rather than silently keeping a stale colour
            // space forever.
            return None;
        };
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        // Order matters on the way *in* as well as the way out: EDR is only
        // meaningful once the layer is already tagged with an HDR transfer,
        // and must be withdrawn before the layer is retagged back to SDR,
        // or there is a window of frames claiming extended range against an
        // sRGB curve.
        match desired {
            PresentationColorSpace::Hdr10Pq => {
                self.layer.setColorspace(Some(&colorspace));
                self.layer.setWantsExtendedDynamicRangeContent(true);
                // The mastering luminance HDR10 assumes when the stream
                // carries no mastering-display metadata of its own -- which
                // Arcen's does not, because a desktop is synthetic content
                // with no colourist and no mastering monitor behind it.
                // 0.005 - 1000 nits is the ST 2086 reference HDR10 grade
                // and what Windows itself targets for `AdvancedColor`
                // desktop composition, so a desktop captured in scRGB and
                // encoded to PQ is already effectively graded to it.
                // RGB10A2Unorm carries normalized PQ signal codes. Apple's
                // CAEDRMetadata contract therefore requires 10,000 nits as
                // the optical-output scale for code 1.0.
                let metadata =
                    CAEDRMetadata::HDR10MetadataWithMinLuminance_maxLuminance_opticalOutputScale(
                        HDR10_MIN_LUMINANCE_NITS,
                        HDR10_MAX_LUMINANCE_NITS,
                        if cfg!(feature = "dev-tools") {
                            std::env::var("ARCEN_DECK_PQ_OPTICAL_SCALE")
                                .ok()
                                .and_then(|value| value.parse::<f32>().ok())
                                .unwrap_or(HDR10_NORMALIZED_OPTICAL_OUTPUT_SCALE)
                        } else {
                            HDR10_NORMALIZED_OPTICAL_OUTPUT_SCALE
                        },
                    );
                self.layer.setEDRMetadata(Some(&metadata));
            }
            PresentationColorSpace::Sdr(_) => {
                self.layer.setEDRMetadata(None);
                self.layer.setWantsExtendedDynamicRangeContent(false);
                self.layer.setColorspace(Some(&colorspace));
            }
        }
        CATransaction::commit();
        CATransaction::flush();

        self.last_colorspace = Some(desired);
        Some(desired)
    }

    /// Wraps one plane of `pixel_buffer` as an `MTLTexture` via
    /// `CVMetalTextureCacheCreateTextureFromImage`, with no CPU copy at
    /// all.
    ///
    /// Returns the owning [`apple_cf::cv::CVBuffer`] alongside a raw
    /// `id<MTLTexture>` pointer, rather than a typed
    /// `Retained<ProtocolObject<dyn MTLTexture>>`: the pointer
    /// `CVMetalTextureGetTexture` returns is a +0 (non-owning) reference
    /// into the `CVMetalTextureRef` the returned `CVBuffer` wraps (Apple's
    /// own documented `CVMetalTextureCache` lifetime contract), so there is
    /// nothing else to separately retain -- the `CVBuffer` alone keeps it
    /// alive. Every caller must keep that `CVBuffer` alive for at least as
    /// long as it uses the returned pointer (see [`Self::render`]'s own
    /// `SAFETY` comment at its use site).
    ///
    /// # Errors
    ///
    /// Returns `None` on any `CVReturn` failure or a null output texture --
    /// deliberately not a richer error type, since every caller folds this
    /// into the single [`DedicatedLayerOutcome::PlaneTextureCreationFailed`]
    /// reason.
    fn create_plane_texture(
        &self,
        pixel_buffer: &apple_cf::cv::CVPixelBuffer,
        format: MTLPixelFormat,
        width: usize,
        height: usize,
        plane_index: usize,
    ) -> Option<(apple_cf::cv::CVBuffer, *mut c_void)> {
        let mut texture_out: apple_cf::raw::CVMetalTextureRef = std::ptr::null_mut();
        // SAFETY: `self.texture_cache`/`pixel_buffer` are both live, valid
        // CoreVideo objects for the whole call (borrowed, not stored past
        // it); `texture_out` is a valid `*mut _` output slot on the stack.
        // This is exactly the raw FFI signature `apple_cf` 0.9.3 itself
        // declares (`apple_cf::raw`, re-exported from its own
        // `raw::extras`) -- there is no bespoke safe wrapper for this
        // specific function in that crate (unlike `CVPixelBuffer`/
        // `CVMetalTextureCache` themselves, which do have one and are used
        // above/elsewhere).
        let status = unsafe {
            apple_cf::raw::CVMetalTextureCacheCreateTextureFromImage(
                std::ptr::null(),
                self.texture_cache.as_ptr().cast(),
                pixel_buffer.as_ptr().cast(),
                std::ptr::null(),
                format.0,
                width,
                height,
                plane_index,
                &mut texture_out,
            )
        };
        if status != 0 || texture_out.is_null() {
            return None;
        }
        // SAFETY: `texture_out` is a non-null +1 `CVMetalTextureRef`
        // (itself a `CVBufferRef` subtype) just returned by the "Create"
        // call above, per Core Foundation's create-rule naming convention;
        // `CVBuffer::from_raw` takes ownership of that +1 reference and
        // releases it on `Drop`.
        let cv_texture = apple_cf::cv::CVBuffer::from_raw(texture_out.cast())?;
        // SAFETY: `texture_out` (kept alive by `cv_texture`, returned
        // below) is that same live `CVMetalTextureRef`;
        // `CVMetalTextureGetTexture` returns a live, non-owning
        // `id<MTLTexture>` valid for exactly as long as the
        // `CVMetalTextureRef` that produced it is retained (Apple's
        // documented `CVMetalTextureCache` contract) -- i.e. for as long as
        // the caller keeps the returned `CVBuffer` alive.
        let raw_texture = unsafe { apple_cf::raw::CVMetalTextureGetTexture(texture_out.cast()) };
        if raw_texture.is_null() {
            return None;
        }
        Some((cv_texture, raw_texture))
    }

    /// Renders `frame` into this layer's next drawable and presents it.
    /// See the module doc's "Rendering a frame" section for the full
    /// sequence this follows.
    /// Report what the decoded planes actually contain: on the first frame,
    /// then every [`PLANE_STATISTICS_INTERVAL`].
    ///
    /// Logs the raw visible sample range so a new decoder/format can be
    /// checked against [`plane_pixel_formats`]. A ten-bit neutral chroma
    /// sample is expected near `512 << 6 = 32768`. `distinct_codes` counts
    /// the different code values along the sampled row: a full-width ramp
    /// shows about 1024 through a genuine ten-bit path and about 256 through
    /// an eight-bit one, whatever the container says.
    fn log_plane_statistics(&mut self, frame: &DedicatedLayerFrame) {
        if self
            .plane_statistics_logged_at
            .is_some_and(|at| at.elapsed() < PLANE_STATISTICS_INTERVAL)
        {
            return;
        }
        self.plane_statistics_logged_at = Some(std::time::Instant::now());
        let buffer = &frame.pixel_buffer;
        let plan = plane_pixel_formats(frame.contract.depth);
        let pixel_format =
            String::from_utf8_lossy(&buffer.pixel_format().to_be_bytes()).into_owned();
        let Ok(guard) = buffer.lock(apple_cf::cv::CVPixelBufferLockFlags::READ_ONLY) else {
            return;
        };
        // Sample the vertical centre, not the first rows: the top of a
        // desktop capture is usually a title bar, and a uniform dark strip
        // says nothing about whether chroma varies across the picture.
        let bytes_per_component = if frame.contract.depth == arcen_media::BitDepth::Eight {
            1
        } else {
            2
        };
        let storage_shift = 16u32.saturating_sub(u32::from(frame.contract.depth.bits()));
        let summarise = |data: &[u8],
                         stride: usize,
                         width: usize,
                         height: usize,
                         label: &str,
                         interleaved: bool| {
            let row = height / 2;
            let start = row * stride;
            let components = if interleaved { 2 } else { 1 };
            let visible_bytes = width
                .saturating_mul(components)
                .saturating_mul(bytes_per_component);
            let Some(row_bytes) = data.get(start..start.saturating_add(visible_bytes)) else {
                return;
            };
            let samples: Vec<u16> = if bytes_per_component == 1 {
                row_bytes.iter().map(|value| u16::from(*value)).collect()
            } else {
                row_bytes
                    .chunks_exact(2)
                    .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                    .collect()
            };
            if samples.is_empty() {
                return;
            }
            let stat = |values: &[u16], slot: &str| {
                let min = values.iter().copied().min().unwrap_or(0);
                let max = values.iter().copied().max().unwrap_or(0);
                let mean = values.iter().map(|v| u32::from(*v)).sum::<u32>() / values.len() as u32;
                let code_mean =
                    (mean as f32 / 65535.0 * plan.code_unnormalize_scale).round() as u32;
                let low_bits_nonzero = if storage_shift == 0 {
                    0
                } else {
                    let mask = (1u16 << storage_shift) - 1;
                    values.iter().filter(|value| **value & mask != 0).count()
                };
                let distinct_codes = distinct_codes(values, storage_shift);
                tracing::info!(
                    target: crate::logging::target::VIDEO,
                    plane = label,
                    slot,
                    pixel_format,
                    width,
                    height,
                    stride,
                    min,
                    max,
                    mean,
                    code_mean,
                    low_bits_nonzero,
                    distinct_codes,
                    "decoded plane sample statistics",
                );
            };
            if interleaved {
                let cb: Vec<u16> = samples.iter().copied().step_by(2).collect();
                let cr: Vec<u16> = samples.iter().copied().skip(1).step_by(2).collect();
                stat(&cb, "cb");
                stat(&cr, "cr");
            } else {
                stat(&samples, "y");
            }
        };
        if let Some(data) = guard.plane_data(0) {
            summarise(
                data,
                guard.bytes_per_row_of_plane(0),
                guard.width_of_plane(0),
                guard.height_of_plane(0),
                "luma",
                false,
            );
        }
        if let Some(data) = guard.plane_data(1) {
            summarise(
                data,
                guard.bytes_per_row_of_plane(1),
                guard.width_of_plane(1),
                guard.height_of_plane(1),
                "chroma",
                true,
            );
        }
    }

    fn render_at(
        &mut self,
        frame: &DedicatedLayerFrame,
        target_host_time: Duration,
        refresh_period: Duration,
        telemetry: Arc<DedicatedPresenterTelemetry>,
    ) -> Result<(), DedicatedLayerOutcome> {
        let in_flight = Arc::clone(&self.in_flight);
        if !in_flight
            .lock()
            .expect("in-flight gate poisoned")
            .submitted()
        {
            return Err(DedicatedLayerOutcome::NoDrawableAvailable);
        }
        let result = autoreleasepool(|_| {
            self.render_inner(
                frame,
                Arc::clone(&in_flight),
                target_host_time,
                refresh_period,
                telemetry,
            )
        });
        if result.is_err() {
            in_flight
                .lock()
                .expect("in-flight gate poisoned")
                .completed();
        }
        result
    }

    fn render_inner(
        &mut self,
        frame: &DedicatedLayerFrame,
        in_flight: Arc<Mutex<LayerInFlightGate>>,
        target_host_time: Duration,
        refresh_period: Duration,
        telemetry: Arc<DedicatedPresenterTelemetry>,
    ) -> Result<(), DedicatedLayerOutcome> {
        self.log_plane_statistics(frame);
        if let Some(applied) =
            self.ensure_colorspace(frame.contract.primaries, frame.contract.transfer)
        {
            // The fifth and last link in the HDR chain: the Deck saying,
            // in its own log, what it switched its presentation surface to
            // on receiving this stream.
            let (mode, colorspace) = match applied {
                PresentationColorSpace::Hdr10Pq => ("hdr10", "ITUR_2100_PQ"),
                PresentationColorSpace::Sdr(ReferenceColorSpace::DisplayP3) => ("sdr", "DisplayP3"),
                PresentationColorSpace::Sdr(ReferenceColorSpace::Srgb) => ("sdr", "sRGB"),
            };
            tracing::info!(
                target: crate::logging::target::VIDEO,
                mode,
                colorspace,
                transfer = frame.contract.transfer.token(),
                primaries = frame.contract.primaries.token(),
                bit_depth = frame.contract.depth.bits(),
                edr = matches!(applied, PresentationColorSpace::Hdr10Pq),
                "deck switched video presentation mode",
            );
        }

        let plan = plane_pixel_formats(frame.contract.depth);
        let pixel_buffer = &frame.pixel_buffer;
        let luma_width = pixel_buffer.width_of_plane(0);
        let luma_height = pixel_buffer.height_of_plane(0);
        let chroma_width = pixel_buffer.width_of_plane(1);
        let chroma_height = pixel_buffer.height_of_plane(1);
        apply_dedicated_layer_source_drawable_size(
            &*self.layer,
            source_drawable_size(luma_width, luma_height),
        );

        let Some((luma_cv, luma_ptr)) =
            self.create_plane_texture(pixel_buffer, plan.luma_format, luma_width, luma_height, 0)
        else {
            return Err(DedicatedLayerOutcome::PlaneTextureCreationFailed);
        };
        let Some((chroma_cv, chroma_ptr)) = self.create_plane_texture(
            pixel_buffer,
            plan.chroma_format,
            chroma_width,
            chroma_height,
            1,
        ) else {
            return Err(DedicatedLayerOutcome::PlaneTextureCreationFailed);
        };

        let Some(drawable) = self.layer.nextDrawable() else {
            return Err(DedicatedLayerOutcome::NoDrawableAvailable);
        };
        let drawable_texture = drawable.texture();
        if self.layer.pixelFormat() != MTLPixelFormat::RGB10A2Unorm
            || drawable_texture.pixelFormat() != MTLPixelFormat::RGB10A2Unorm
        {
            return Err(DedicatedLayerOutcome::PixelFormatMismatch);
        }

        let uniform = MetalVideoUniform::from_contract(
            frame.contract,
            (luma_width as u32, luma_height as u32),
            (chroma_width as u32, chroma_height as u32),
            plan.code_unnormalize_scale,
        );
        let uniform_bytes = uniform.to_bytes();
        // SAFETY: the byte slice is live for the call and Metal copies it.
        let uniform_buffer = unsafe {
            self.device.newBufferWithBytes_length_options(
                NonNull::new(uniform_bytes.as_ptr() as *mut c_void)
                    .expect("uniform_bytes is never empty"),
                uniform_bytes.len(),
                MTLResourceOptions::StorageModeShared,
            )
        }
        .ok_or(DedicatedLayerOutcome::UniformBufferCreationFailed)?;

        let render_pass = MTLRenderPassDescriptor::renderPassDescriptor();
        // SAFETY: index 0 is always a valid colour-attachment slot.
        let color_attachment =
            unsafe { render_pass.colorAttachments().objectAtIndexedSubscript(0) };
        color_attachment.setTexture(Some(&*drawable_texture));
        color_attachment.setLoadAction(MTLLoadAction::Clear);
        color_attachment.setStoreAction(MTLStoreAction::Store);
        color_attachment.setClearColor(MTLClearColor {
            red: 0.0,
            green: 0.0,
            blue: 0.0,
            alpha: 1.0,
        });

        let command_buffer = self
            .command_queue
            .commandBuffer()
            .ok_or(DedicatedLayerOutcome::CommandBufferCreationFailed)?;
        let encoder = command_buffer
            .renderCommandEncoderWithDescriptor(&render_pass)
            .ok_or(DedicatedLayerOutcome::RenderEncoderCreationFailed)?;
        encoder.setRenderPipelineState(&self.pipeline_state);
        // SAFETY: the raw texture pointers are valid while `_luma_cv` and
        // `_chroma_cv` retain their CoreVideo owners in this stack frame.
        let luma_texture: &ProtocolObject<dyn MTLTexture> =
            unsafe { &*(luma_ptr as *const ProtocolObject<dyn MTLTexture>) };
        let chroma_texture: &ProtocolObject<dyn MTLTexture> =
            unsafe { &*(chroma_ptr as *const ProtocolObject<dyn MTLTexture>) };
        // SAFETY: the encoder is live; indices match the shader bindings.
        unsafe {
            encoder.setFragmentTexture_atIndex(Some(luma_texture), 0);
            encoder.setFragmentTexture_atIndex(Some(chroma_texture), 1);
            encoder.setFragmentBuffer_offset_atIndex(Some(&*uniform_buffer), 0, 0);
            encoder.drawPrimitives_vertexStart_vertexCount(MTLPrimitiveType::Triangle, 0, 3);
        }
        encoder.endEncoding();
        let drawable_for_present: &ProtocolObject<dyn MTLDrawable> =
            ProtocolObject::from_ref(&*drawable);
        command_buffer.presentDrawable_afterMinimumDuration(
            drawable_for_present,
            present_minimum_duration_s(refresh_period),
        );
        telemetry.record_submitted(target_host_time, frame.arrival_host_time, refresh_period);
        install_presented_handler(drawable_for_present, telemetry, Arc::clone(&in_flight));
        let retained_frame = TenBitInFlightFrame {
            _pixel_buffer: pixel_buffer.clone(),
            _luma_cv: luma_cv,
            _chroma_cv: chroma_cv,
            _uniform_buffer: uniform_buffer,
        };
        let retained_frame = Arc::new(Mutex::new(Some(retained_frame)));
        let completion = RcBlock::new(
            move |_buffer: NonNull<ProtocolObject<dyn MTLCommandBuffer>>| {
                retained_frame
                    .lock()
                    .expect("retained frame poisoned")
                    .take();
            },
        );
        // SAFETY: Metal copies completion handlers and invokes them after GPU use.
        unsafe {
            command_buffer.addCompletedHandler(RcBlock::as_ptr(&completion).cast());
        }
        command_buffer.commit();

        Ok(())
    }
}

struct TenBitInFlightFrame {
    _pixel_buffer: apple_cf::cv::CVPixelBuffer,
    _luma_cv: apple_cf::cv::CVBuffer,
    _chroma_cv: apple_cf::cv::CVBuffer,
    _uniform_buffer: Retained<ProtocolObject<dyn MTLBuffer>>,
}

// SAFETY: The frame owns retain-counted CoreVideo and Metal objects whose
// lifetime is extended solely so Metal's completion callback can drop them
// after GPU use. The callback does not dereference or mutate these objects; it
// only releases the owned references and updates the separate in-flight gate.
unsafe impl Send for TenBitInFlightFrame {}

// SAFETY: This type is only shared behind a `Mutex` by the Deck, and the only
// cross-thread operation performed after main-thread attachment is Metal/Core
// Video rendering. AppKit/layer hierarchy mutation remains in `attach`/`resize`,
// which the UI calls on the main thread.
unsafe impl Send for DedicatedVideoLayer {}

impl Drop for DedicatedVideoLayer {
    /// Tears the layer down: removes it from the view hierarchy. Every
    /// other resource here (`device`/`command_queue`/`pipeline_state`/
    /// `texture_cache`) is released by its own `Retained`/`CVMetalTextureCache`
    /// handling as this struct's fields drop in turn.
    fn drop(&mut self) {
        if MainThreadMarker::new().is_some() {
            self.layer.removeFromSuperlayer();
        }
    }
}

// ============================================================================
// Dedicated 8-bit Auto/Speed layer
// ============================================================================

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
    const fn from_outcome(outcome: DedicatedLayerOutcome) -> Self {
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

const fn retained_handle_detach_before_install(has_retained_handle: bool) -> bool {
    has_retained_handle
}

/// Small pure in-flight gate for the dedicated layer. Runtime rendering waits
/// for completion today, so the counter normally returns to zero immediately;
/// the type exists to keep the pacing invariant testable before a completion
/// callback/display-link variant grows out of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LayerInFlightGate {
    max_in_flight: u8,
    in_flight: u8,
}

impl LayerInFlightGate {
    pub(crate) const fn new(max_in_flight: u8) -> Self {
        Self {
            max_in_flight,
            in_flight: 0,
        }
    }

    pub(crate) const fn can_submit(self) -> bool {
        self.in_flight < self.max_in_flight
    }

    pub(crate) fn submitted(&mut self) -> bool {
        if !self.can_submit() {
            return false;
        }
        self.in_flight += 1;
        true
    }

    pub(crate) fn completed(&mut self) {
        self.in_flight = self.in_flight.saturating_sub(1);
    }
}

impl Default for LayerInFlightGate {
    fn default() -> Self {
        Self::new(2)
    }
}

pub struct DedicatedEightBitVideoLayer {
    layer: Retained<CAMetalLayer>,
    device: Retained<ProtocolObject<dyn MTLDevice>>,
    command_queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    pipeline_state: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    texture_cache: apple_cf::cv::CVMetalTextureCache,
    last_colorspace: Option<PresentationColorSpace>,
    in_flight: Arc<Mutex<LayerInFlightGate>>,
}

struct EightBitInFlightFrame {
    _pixel_buffer: apple_cf::cv::CVPixelBuffer,
    _luma_cv: apple_cf::cv::CVBuffer,
    _chroma_cv: apple_cf::cv::CVBuffer,
    _uniform_buffer: Retained<ProtocolObject<dyn MTLBuffer>>,
}

// SAFETY: The frame owns retain-counted CoreVideo and Metal objects whose
// lifetime is extended solely so Metal's completion callback can drop them
// after GPU use. The callback does not dereference or mutate these objects; it
// only releases the owned references and updates the separate in-flight gate.
unsafe impl Send for EightBitInFlightFrame {}

// SAFETY: This type is only shared behind a `Mutex` by the Deck, and the only
// cross-thread operation performed after main-thread attachment is Metal/Core
// Video rendering (`nextDrawable`, command-buffer creation/commit, texture-cache
// wrapping). The task's platform contract explicitly allows drawing a
// `CAMetalLayer` from any thread; AppKit/layer hierarchy mutation remains in
// `attach`/`resize`, which the UI calls on the main thread.
unsafe impl Send for DedicatedEightBitVideoLayer {}

impl DedicatedEightBitVideoLayer {
    pub fn attach(rect: CGRect) -> Result<Self, DedicatedLayerOutcome> {
        let mtm = MainThreadMarker::new().ok_or(DedicatedLayerOutcome::NotMainThread)?;
        let window = super::video_render::find_root_window(mtm)
            .ok_or(DedicatedLayerOutcome::NoRootWindow)?;
        let view = window
            .contentView()
            .ok_or(DedicatedLayerOutcome::NoContentView)?;
        view.setWantsLayer(true);
        let root_layer = view.layer().ok_or(DedicatedLayerOutcome::NoRootLayer)?;

        let device = MTLCreateSystemDefaultDevice().ok_or(DedicatedLayerOutcome::NoMetalDevice)?;
        let command_queue = device
            .newCommandQueue()
            .ok_or(DedicatedLayerOutcome::CommandQueueCreationFailed)?;
        let texture_cache = apple_cf::cv::CVMetalTextureCache::system_default()
            .ok_or(DedicatedLayerOutcome::TextureCacheCreationFailed)?;
        let pipeline_state = build_pipeline_state(&device, MTLPixelFormat::BGRA8Unorm)?;

        let layer = CAMetalLayer::new();
        layer.setDevice(Some(&*device));
        layer.setPixelFormat(MTLPixelFormat::BGRA8Unorm);
        if layer.pixelFormat() != MTLPixelFormat::BGRA8Unorm {
            return Err(DedicatedLayerOutcome::PixelFormatMismatch);
        }
        layer.setFramebufferOnly(true);
        layer.setDisplaySyncEnabled(true);
        layer.setPresentsWithTransaction(false);
        layer.setOpaque(true);
        layer.setDisplaySyncEnabled(true);
        layer.setPresentsWithTransaction(false);
        layer.setAllowsNextDrawableTimeout(true);
        layer.setMaximumDrawableCount(3);
        layer.setWantsExtendedDynamicRangeContent(false);
        configure_dedicated_layer_scaling(&layer);
        layer.setName(Some(&NSString::from_str(
            DEDICATED_EIGHT_BIT_VIDEO_LAYER_NAME,
        )));
        apply_dedicated_layer_geometry(
            &*layer,
            DedicatedLayerGeometry::new(rect, window.backingScaleFactor()),
        );
        root_layer.insertSublayer_atIndex(&layer, 0);

        Ok(Self {
            layer,
            device,
            command_queue,
            pipeline_state,
            texture_cache,
            last_colorspace: None,
            in_flight: Arc::new(Mutex::new(LayerInFlightGate::default())),
        })
    }

    fn ensure_colorspace(
        &mut self,
        primaries: arcen_media::ColorPrimaries,
        transfer: arcen_media::TransferCharacteristics,
    ) {
        let desired = presentation_colorspace_for(primaries, transfer);
        if self.last_colorspace == Some(desired) {
            return;
        }
        let colorspace = match desired {
            PresentationColorSpace::Sdr(ReferenceColorSpace::Srgb) => {
                CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB }))
            }
            PresentationColorSpace::Sdr(ReferenceColorSpace::DisplayP3) => {
                CGColorSpace::with_name(Some(unsafe { kCGColorSpaceDisplayP3 }))
            }
            PresentationColorSpace::Hdr10Pq => {
                CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB }))
            }
        };
        if let Some(colorspace) = colorspace {
            CATransaction::begin();
            CATransaction::setDisableActions(true);
            self.layer.setColorspace(Some(&colorspace));
            self.layer.setEDRMetadata(None);
            self.layer.setWantsExtendedDynamicRangeContent(false);
            CATransaction::commit();
            CATransaction::flush();
            self.last_colorspace = Some(desired);
        }
    }

    fn create_plane_texture(
        &self,
        pixel_buffer: &apple_cf::cv::CVPixelBuffer,
        format: MTLPixelFormat,
        width: usize,
        height: usize,
        plane_index: usize,
    ) -> Option<(apple_cf::cv::CVBuffer, *mut c_void)> {
        let mut texture_out: apple_cf::raw::CVMetalTextureRef = std::ptr::null_mut();
        // SAFETY: same invariants as the 10-bit path above: the texture cache,
        // pixel buffer and out-pointer are live for the call, and the returned
        // Create-rule object is owned immediately below.
        let status = unsafe {
            apple_cf::raw::CVMetalTextureCacheCreateTextureFromImage(
                std::ptr::null(),
                self.texture_cache.as_ptr().cast(),
                pixel_buffer.as_ptr().cast(),
                std::ptr::null(),
                format.0,
                width,
                height,
                plane_index,
                &mut texture_out,
            )
        };
        if status != 0 || texture_out.is_null() {
            return None;
        }
        let cv_texture = apple_cf::cv::CVBuffer::from_raw(texture_out.cast())?;
        // SAFETY: the returned raw texture is +0 and remains valid while
        // `cv_texture` retains the owning `CVMetalTextureRef`.
        let raw_texture = unsafe { apple_cf::raw::CVMetalTextureGetTexture(texture_out.cast()) };
        if raw_texture.is_null() {
            return None;
        }
        Some((cv_texture, raw_texture))
    }

    fn render_at(
        &mut self,
        frame: &DedicatedEightBitLayerFrame,
        target_host_time: Duration,
        refresh_period: Duration,
        telemetry: Arc<DedicatedPresenterTelemetry>,
    ) -> Result<(), DedicatedLayerOutcome> {
        if frame.contract.depth != arcen_media::BitDepth::Eight {
            return Err(DedicatedLayerOutcome::PlaneTextureCreationFailed);
        }
        let in_flight = Arc::clone(&self.in_flight);
        if !in_flight
            .lock()
            .expect("in-flight gate poisoned")
            .submitted()
        {
            return Err(DedicatedLayerOutcome::NoDrawableAvailable);
        }
        let result = autoreleasepool(|_| {
            self.render_inner(
                frame,
                Arc::clone(&in_flight),
                target_host_time,
                refresh_period,
                telemetry,
            )
        });
        if result.is_err() {
            in_flight
                .lock()
                .expect("in-flight gate poisoned")
                .completed();
        }
        result
    }

    fn render_inner(
        &mut self,
        frame: &DedicatedEightBitLayerFrame,
        in_flight: Arc<Mutex<LayerInFlightGate>>,
        target_host_time: Duration,
        refresh_period: Duration,
        telemetry: Arc<DedicatedPresenterTelemetry>,
    ) -> Result<(), DedicatedLayerOutcome> {
        self.ensure_colorspace(frame.contract.primaries, frame.contract.transfer);
        let plan = plane_pixel_formats(frame.contract.depth);
        let pixel_buffer = &frame.pixel_buffer;
        if pixel_buffer.plane_count() != 2 {
            return Err(DedicatedLayerOutcome::PlaneTextureCreationFailed);
        }
        let luma_width = pixel_buffer.width_of_plane(0);
        let luma_height = pixel_buffer.height_of_plane(0);
        let chroma_width = pixel_buffer.width_of_plane(1);
        let chroma_height = pixel_buffer.height_of_plane(1);
        apply_dedicated_layer_source_drawable_size(
            &*self.layer,
            source_drawable_size(luma_width, luma_height),
        );

        let Some((luma_cv, luma_ptr)) =
            self.create_plane_texture(pixel_buffer, plan.luma_format, luma_width, luma_height, 0)
        else {
            return Err(DedicatedLayerOutcome::PlaneTextureCreationFailed);
        };
        let Some((chroma_cv, chroma_ptr)) = self.create_plane_texture(
            pixel_buffer,
            plan.chroma_format,
            chroma_width,
            chroma_height,
            1,
        ) else {
            return Err(DedicatedLayerOutcome::PlaneTextureCreationFailed);
        };
        let Some(drawable) = self.layer.nextDrawable() else {
            return Err(DedicatedLayerOutcome::NoDrawableAvailable);
        };
        let drawable_texture = drawable.texture();
        if self.layer.pixelFormat() != MTLPixelFormat::BGRA8Unorm
            || drawable_texture.pixelFormat() != MTLPixelFormat::BGRA8Unorm
        {
            return Err(DedicatedLayerOutcome::PixelFormatMismatch);
        }

        let uniform = MetalVideoUniform::from_contract(
            frame.contract,
            (luma_width as u32, luma_height as u32),
            (chroma_width as u32, chroma_height as u32),
            plan.code_unnormalize_scale,
        );
        let uniform_bytes = uniform.to_bytes();
        // SAFETY: the byte slice is live for the call and Metal copies it.
        let uniform_buffer = unsafe {
            self.device.newBufferWithBytes_length_options(
                NonNull::new(uniform_bytes.as_ptr() as *mut c_void)
                    .expect("uniform_bytes is never empty"),
                uniform_bytes.len(),
                MTLResourceOptions::StorageModeShared,
            )
        }
        .ok_or(DedicatedLayerOutcome::UniformBufferCreationFailed)?;

        let render_pass = MTLRenderPassDescriptor::renderPassDescriptor();
        // SAFETY: colour attachment 0 is valid on every render pass.
        let color_attachment =
            unsafe { render_pass.colorAttachments().objectAtIndexedSubscript(0) };
        color_attachment.setTexture(Some(&*drawable_texture));
        color_attachment.setLoadAction(MTLLoadAction::Clear);
        color_attachment.setStoreAction(MTLStoreAction::Store);
        color_attachment.setClearColor(MTLClearColor {
            red: 0.0,
            green: 0.0,
            blue: 0.0,
            alpha: 1.0,
        });

        let command_buffer = self
            .command_queue
            .commandBuffer()
            .ok_or(DedicatedLayerOutcome::CommandBufferCreationFailed)?;
        let encoder = command_buffer
            .renderCommandEncoderWithDescriptor(&render_pass)
            .ok_or(DedicatedLayerOutcome::RenderEncoderCreationFailed)?;
        encoder.setRenderPipelineState(&self.pipeline_state);
        // SAFETY: the raw texture pointers are valid while `_luma_cv` and
        // `_chroma_cv` retain their CoreVideo owners in this stack frame.
        let luma_texture: &ProtocolObject<dyn MTLTexture> =
            unsafe { &*(luma_ptr as *const ProtocolObject<dyn MTLTexture>) };
        let chroma_texture: &ProtocolObject<dyn MTLTexture> =
            unsafe { &*(chroma_ptr as *const ProtocolObject<dyn MTLTexture>) };
        // SAFETY: the encoder is live; indices match the shader bindings.
        unsafe {
            encoder.setFragmentTexture_atIndex(Some(luma_texture), 0);
            encoder.setFragmentTexture_atIndex(Some(chroma_texture), 1);
            encoder.setFragmentBuffer_offset_atIndex(Some(&*uniform_buffer), 0, 0);
            encoder.drawPrimitives_vertexStart_vertexCount(MTLPrimitiveType::Triangle, 0, 3);
        }
        encoder.endEncoding();
        let drawable_for_present: &ProtocolObject<dyn MTLDrawable> =
            ProtocolObject::from_ref(&*drawable);
        command_buffer.presentDrawable_afterMinimumDuration(
            drawable_for_present,
            present_minimum_duration_s(refresh_period),
        );
        telemetry.record_submitted(target_host_time, frame.arrival_host_time, refresh_period);
        install_presented_handler(drawable_for_present, telemetry, Arc::clone(&in_flight));
        let retained_frame = EightBitInFlightFrame {
            _pixel_buffer: pixel_buffer.clone(),
            _luma_cv: luma_cv,
            _chroma_cv: chroma_cv,
            _uniform_buffer: uniform_buffer,
        };
        let retained_frame = Arc::new(Mutex::new(Some(retained_frame)));
        let completion = RcBlock::new(
            move |_buffer: NonNull<ProtocolObject<dyn MTLCommandBuffer>>| {
                retained_frame
                    .lock()
                    .expect("retained frame poisoned")
                    .take();
            },
        );
        // SAFETY: `completion` is a valid heap block for this call. Metal
        // copies completion handlers and invokes them after the command buffer
        // has completed; the block captures owned CoreVideo/Metal resources
        // and drops them only from that completion point.
        unsafe {
            command_buffer.addCompletedHandler(RcBlock::as_ptr(&completion).cast());
        }
        command_buffer.commit();
        Ok(())
    }
}

impl Drop for DedicatedEightBitVideoLayer {
    fn drop(&mut self) {
        if MainThreadMarker::new().is_some() {
            self.layer.removeFromSuperlayer();
        }
    }
}

#[derive(Default)]
pub struct DedicatedEightBitVideoPresenter {
    thread: DedicatedEightBitPresenterThread,
    fallback: DedicatedLayerFallback,
    last_rect: Option<CGRect>,
    presentation_status: DedicatedEightBitPresentationStatus,
    surface_visible: bool,
}

// SAFETY: all cross-thread rendering is guarded by the presenter's external
// `Mutex`; see `DedicatedEightBitVideoLayer`'s `Send` justification. Geometry
// mutation remains a main-thread-only method.
unsafe impl Send for DedicatedEightBitVideoPresenter {}

trait DedicatedThreadStatus: Copy + Send + 'static {
    fn from_outcome(outcome: DedicatedLayerOutcome) -> Self;
    fn is_fallback(self) -> bool;
    fn is_dropped_frame(self) -> bool;
    fn is_skipped_occluded(self) -> bool;
}

impl DedicatedThreadStatus for DedicatedEightBitPresentationStatus {
    fn from_outcome(outcome: DedicatedLayerOutcome) -> Self {
        Self::from_outcome(outcome)
    }

    fn is_fallback(self) -> bool {
        self.is_fallback()
    }

    fn is_dropped_frame(self) -> bool {
        self.is_dropped_frame()
    }

    fn is_skipped_occluded(self) -> bool {
        self.is_skipped_occluded()
    }
}

impl DedicatedThreadStatus for DedicatedPresentationStatus {
    fn from_outcome(outcome: DedicatedLayerOutcome) -> Self {
        Self::from_outcome(outcome)
    }

    fn is_fallback(self) -> bool {
        self.is_eight_bit_fallback()
    }

    fn is_dropped_frame(self) -> bool {
        self.is_dropped_frame()
    }

    fn is_skipped_occluded(self) -> bool {
        self.is_skipped_occluded()
    }
}

trait DedicatedRenderableLayer<F>: Send + 'static {
    fn render_at(
        &mut self,
        frame: &F,
        target_host_time: Duration,
        refresh_period: Duration,
        telemetry: Arc<DedicatedPresenterTelemetry>,
    ) -> Result<(), DedicatedLayerOutcome>;
    fn layer_handle(&self) -> Retained<CAMetalLayer>;
    /// Whether a drawable slot is free. A refresh that finds none leaves the
    /// next frame queued in the pacer instead of taking it and discarding it.
    fn ready_for_frame(&self) -> bool {
        true
    }
}

impl DedicatedRenderableLayer<DedicatedEightBitLayerFrame> for DedicatedEightBitVideoLayer {
    fn ready_for_frame(&self) -> bool {
        self.in_flight
            .lock()
            .expect("in-flight gate poisoned")
            .can_submit()
    }

    fn render_at(
        &mut self,
        frame: &DedicatedEightBitLayerFrame,
        target_host_time: Duration,
        refresh_period: Duration,
        telemetry: Arc<DedicatedPresenterTelemetry>,
    ) -> Result<(), DedicatedLayerOutcome> {
        Self::render_at(self, frame, target_host_time, refresh_period, telemetry)
    }

    fn layer_handle(&self) -> Retained<CAMetalLayer> {
        self.layer.clone()
    }
}

impl DedicatedRenderableLayer<DedicatedLayerFrame> for DedicatedVideoLayer {
    fn ready_for_frame(&self) -> bool {
        self.in_flight
            .lock()
            .expect("in-flight gate poisoned")
            .can_submit()
    }

    fn render_at(
        &mut self,
        frame: &DedicatedLayerFrame,
        target_host_time: Duration,
        refresh_period: Duration,
        telemetry: Arc<DedicatedPresenterTelemetry>,
    ) -> Result<(), DedicatedLayerOutcome> {
        Self::render_at(self, frame, target_host_time, refresh_period, telemetry)
    }

    fn layer_handle(&self) -> Retained<CAMetalLayer> {
        self.layer.clone()
    }
}

type DedicatedThreadShared<L, F, S> = Arc<(Mutex<DedicatedPresenterThreadState<L, F, S>>, Condvar)>;

trait DedicatedPacedFrame {
    fn source_fps(&self) -> Option<u32>;
}

impl DedicatedPacedFrame for DedicatedLayerFrame {
    fn source_fps(&self) -> Option<u32> {
        self.source_fps
    }
}

impl DedicatedPacedFrame for DedicatedEightBitLayerFrame {
    fn source_fps(&self) -> Option<u32> {
        self.source_fps
    }
}

#[cfg(test)]
impl DedicatedPacedFrame for u32 {
    fn source_fps(&self) -> Option<u32> {
        Some(60)
    }
}

#[derive(Debug, Clone)]
struct RetainedPresentationFrame<F> {
    frame: F,
    arrival_host_time: Duration,
    replayed_after_restore: bool,
}

#[derive(Default)]
struct DedicatedPresenterTelemetry {
    drops: AtomicU64,
    skips: AtomicU64,
    slot_waits: AtomicU64,
    frames_received: AtomicU64,
    frames_submitted: AtomicU64,
    frames_confirmed: AtomicU64,
    metal_dropped: AtomicU64,
    presented_time_unconfirmed: AtomicU64,
    dropped_overflow: AtomicU64,
    dropped_stale: AtomicU64,
    dropped_hidden: AtomicU64,
    underruns: AtomicU64,
    queue_depth_samples: AtomicU64,
    queue_depth_total: AtomicU64,
    queue_depth_max: AtomicU64,
    interval_1x: AtomicU64,
    interval_2x: AtomicU64,
    interval_3x_plus: AtomicU64,
    latency_sample_count: AtomicU64,
    latency_us: Mutex<Vec<u64>>,
    submitted_presented_at: Mutex<Vec<std::time::Instant>>,
    confirmed_presented_at: Mutex<Vec<std::time::Instant>>,
    last_submitted_host_time_us: AtomicU64,
    last_refresh_period_us: AtomicU64,
    latest_refresh_period_us: AtomicU64,
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
    pub(crate) submitted_presented_at: Vec<std::time::Instant>,
    pub(crate) confirmed_presented_at: Vec<std::time::Instant>,
    pub(crate) refresh_period_us: Option<u64>,
}

impl DedicatedPresenterTelemetry {
    fn reset_presentation_baseline(&self) {
        self.last_submitted_host_time_us.store(0, Ordering::Relaxed);
        self.last_refresh_period_us.store(0, Ordering::Relaxed);
    }

    fn add_pacer_delta(&self, before: FramePacerCounters, after: FramePacerCounters) {
        self.frames_received.fetch_add(
            after.frames_received.saturating_sub(before.frames_received),
            Ordering::Relaxed,
        );
        self.dropped_overflow.fetch_add(
            after
                .dropped_overflow
                .saturating_sub(before.dropped_overflow),
            Ordering::Relaxed,
        );
        self.dropped_stale.fetch_add(
            after.dropped_stale.saturating_sub(before.dropped_stale),
            Ordering::Relaxed,
        );
        self.dropped_hidden.fetch_add(
            after.dropped_hidden.saturating_sub(before.dropped_hidden),
            Ordering::Relaxed,
        );
        self.underruns.fetch_add(
            after.underruns.saturating_sub(before.underruns),
            Ordering::Relaxed,
        );
        self.queue_depth_samples.fetch_add(
            after
                .queue_depth_samples
                .saturating_sub(before.queue_depth_samples),
            Ordering::Relaxed,
        );
        self.queue_depth_total.fetch_add(
            after
                .queue_depth_total
                .saturating_sub(before.queue_depth_total),
            Ordering::Relaxed,
        );
        atomic_max(
            &self.queue_depth_max,
            u64::try_from(after.queue_depth_max).unwrap_or(u64::MAX),
        );
    }

    fn record_presented_time_unconfirmed(&self) {
        self.presented_time_unconfirmed
            .fetch_add(1, Ordering::Relaxed);
    }

    fn record_submitted(
        &self,
        submitted_host_time: Duration,
        arrival_host_time: Duration,
        refresh_period: Duration,
    ) {
        self.frames_submitted.fetch_add(1, Ordering::Relaxed);
        let submitted_us = duration_micros(submitted_host_time);
        let refresh_us = duration_micros(refresh_period.max(Duration::from_micros(1)));
        self.latest_refresh_period_us
            .store(refresh_us, Ordering::Relaxed);
        let previous_refresh = self
            .last_refresh_period_us
            .swap(refresh_us, Ordering::Relaxed);
        let previous = if previous_refresh == 0 || previous_refresh.abs_diff(refresh_us) > 100 {
            self.last_submitted_host_time_us
                .store(submitted_us, Ordering::Relaxed);
            0
        } else {
            self.last_submitted_host_time_us
                .swap(submitted_us, Ordering::Relaxed)
        };
        if previous > 0 && submitted_us > previous {
            let interval_us = submitted_us - previous;
            let nearest = ((interval_us + refresh_us / 2) / refresh_us).max(1);
            match nearest {
                1 => self.interval_1x.fetch_add(1, Ordering::Relaxed),
                2 => self.interval_2x.fetch_add(1, Ordering::Relaxed),
                _ => self.interval_3x_plus.fetch_add(1, Ordering::Relaxed),
            };
        }
        if let Some(latency) = submitted_host_time.checked_sub(arrival_host_time) {
            self.latency_sample_count.fetch_add(1, Ordering::Relaxed);
            self.latency_us
                .lock()
                .expect("latency telemetry poisoned")
                .push(duration_micros(latency));
        }
        self.submitted_presented_at
            .lock()
            .expect("submitted presentation telemetry poisoned")
            .push(std::time::Instant::now());
    }

    fn record_confirmed(&self) {
        self.frames_confirmed.fetch_add(1, Ordering::Relaxed);
        self.confirmed_presented_at
            .lock()
            .expect("confirmed presentation telemetry poisoned")
            .push(std::time::Instant::now());
    }

    fn drain(&self) -> DedicatedPresenterTelemetryDelta {
        let mut samples = self.latency_us.lock().expect("latency telemetry poisoned");
        samples.sort_unstable();
        let latency_p50_us = percentile(&samples, 50);
        let latency_p99_us = percentile(&samples, 99);
        samples.clear();
        let submitted_presented_at = self
            .submitted_presented_at
            .lock()
            .expect("submitted presentation telemetry poisoned")
            .drain(..)
            .collect();
        let confirmed_presented_at = self
            .confirmed_presented_at
            .lock()
            .expect("confirmed presentation telemetry poisoned")
            .drain(..)
            .collect();
        let refresh_period_us = match self.latest_refresh_period_us.load(Ordering::Relaxed) {
            0 => None,
            value => Some(value),
        };
        DedicatedPresenterTelemetryDelta {
            drops: self.drops.swap(0, Ordering::Relaxed),
            skips: self.skips.swap(0, Ordering::Relaxed),
            slot_waits: self.slot_waits.swap(0, Ordering::Relaxed),
            frames_received: self.frames_received.swap(0, Ordering::Relaxed),
            frames_submitted: self.frames_submitted.swap(0, Ordering::Relaxed),
            frames_confirmed: self.frames_confirmed.swap(0, Ordering::Relaxed),
            metal_dropped: self.metal_dropped.swap(0, Ordering::Relaxed),
            presented_time_unconfirmed: self.presented_time_unconfirmed.swap(0, Ordering::Relaxed),
            dropped_overflow: self.dropped_overflow.swap(0, Ordering::Relaxed),
            dropped_stale: self.dropped_stale.swap(0, Ordering::Relaxed),
            dropped_hidden: self.dropped_hidden.swap(0, Ordering::Relaxed),
            underruns: self.underruns.swap(0, Ordering::Relaxed),
            queue_depth_samples: self.queue_depth_samples.swap(0, Ordering::Relaxed),
            queue_depth_total: self.queue_depth_total.swap(0, Ordering::Relaxed),
            queue_depth_max: self.queue_depth_max.swap(0, Ordering::Relaxed),
            interval_1x: self.interval_1x.swap(0, Ordering::Relaxed),
            interval_2x: self.interval_2x.swap(0, Ordering::Relaxed),
            interval_3x_plus: self.interval_3x_plus.swap(0, Ordering::Relaxed),
            latency_p50_us,
            latency_p99_us,
            submitted_presented_at,
            confirmed_presented_at,
            refresh_period_us,
        }
    }
}

fn duration_micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

fn percentile(samples: &[u64], percentile: usize) -> Option<u64> {
    if samples.is_empty() {
        return None;
    }
    let index = (samples.len().saturating_sub(1) * percentile) / 100;
    samples.get(index).copied()
}

fn atomic_max(target: &AtomicU64, value: u64) {
    let mut current = target.load(Ordering::Relaxed);
    while current < value {
        match target.compare_exchange_weak(current, value, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => break,
            Err(observed) => current = observed,
        }
    }
}

fn install_presented_handler(
    drawable: &ProtocolObject<dyn MTLDrawable>,
    telemetry: Arc<DedicatedPresenterTelemetry>,
    in_flight: Arc<Mutex<LayerInFlightGate>>,
) {
    let handler = RcBlock::new(move |drawable: NonNull<ProtocolObject<dyn MTLDrawable>>| {
        // SAFETY: Metal invokes the block with the drawable being presented.
        let drawable = unsafe { drawable.as_ref() };
        let presented = Duration::from_secs_f64(drawable.presentedTime());
        if presented.is_zero() {
            telemetry.record_presented_time_unconfirmed();
        } else {
            telemetry.record_confirmed();
        }
        in_flight
            .lock()
            .expect("in-flight gate poisoned")
            .completed();
        tracing::debug!(
            target: crate::logging::target::VIDEO,
            presented_time_s = presented.as_secs_f64(),
            confirmed = !presented.is_zero(),
            "dedicated video drawable presented callback",
        );
    });
    // SAFETY: Metal copies the block and invokes it after presentation.
    unsafe {
        drawable.addPresentedHandler(RcBlock::as_ptr(&handler).cast());
    }
}

struct DedicatedPresenterThread<L, F, S>
where
    L: DedicatedRenderableLayer<F>,
    F: DedicatedPacedFrame + Clone + Send + 'static,
    S: DedicatedThreadStatus,
{
    shared: DedicatedThreadShared<L, F, S>,
    telemetry: Arc<DedicatedPresenterTelemetry>,
    display_link: Option<DedicatedDisplayLink<L, F, S>>,
}

struct DedicatedPresenterThreadState<L, F, S>
where
    L: DedicatedRenderableLayer<F>,
    F: DedicatedPacedFrame + Clone + Send + 'static,
    S: DedicatedThreadStatus,
{
    layer: Option<L>,
    layer_handle: Option<Retained<CAMetalLayer>>,
    layer_installed: bool,
    layer_generation: u64,
    has_presented_picture: bool,
    pacer: FramePacer<F>,
    last_frame: Option<RetainedPresentationFrame<F>>,
    last_geometry: Option<DedicatedLayerGeometry>,
    display_id: Option<CGDirectDisplayID>,
    visible: bool,
    stopping: bool,
    async_status: Option<S>,
    wake: Option<egui::Context>,
    accepted_frames: u64,
}

impl<L, F, S> Default for DedicatedPresenterThreadState<L, F, S>
where
    L: DedicatedRenderableLayer<F>,
    F: DedicatedPacedFrame + Clone + Send + 'static,
    S: DedicatedThreadStatus,
{
    fn default() -> Self {
        Self {
            layer: None,
            layer_handle: None,
            layer_installed: false,
            layer_generation: 0,
            has_presented_picture: false,
            pacer: FramePacer::new(),
            last_frame: None,
            last_geometry: None,
            display_id: None,
            visible: false,
            stopping: false,
            async_status: None,
            wake: None,
            accepted_frames: 0,
        }
    }
}

// SAFETY: The state is protected by `Mutex`, and the only non-main-thread
// payload is an explicitly Send layer wrapper plus Send frame/status data.
unsafe impl<L, F, S> Send for DedicatedPresenterThreadState<L, F, S>
where
    L: DedicatedRenderableLayer<F>,
    F: DedicatedPacedFrame + Clone + Send + 'static,
    S: DedicatedThreadStatus,
{
}

impl<L, F, S> DedicatedPresenterThreadState<L, F, S>
where
    L: DedicatedRenderableLayer<F>,
    F: DedicatedPacedFrame + Clone + Send + 'static,
    S: DedicatedThreadStatus,
{
    fn depth_reset_for_tests(&mut self) {}
}

impl<L, F, S> Default for DedicatedPresenterThread<L, F, S>
where
    L: DedicatedRenderableLayer<F>,
    F: DedicatedPacedFrame + Clone + Send + 'static,
    S: DedicatedThreadStatus,
{
    fn default() -> Self {
        Self {
            shared: Arc::new((
                Mutex::new(DedicatedPresenterThreadState::default()),
                Condvar::new(),
            )),
            telemetry: Arc::new(DedicatedPresenterTelemetry::default()),
            display_link: None,
        }
    }
}

struct DisplayLinkOwner<L, F, S>
where
    L: DedicatedRenderableLayer<F>,
    F: DedicatedPacedFrame + Clone + Send + 'static,
    S: DedicatedThreadStatus,
{
    shared: DedicatedThreadShared<L, F, S>,
    telemetry: Arc<DedicatedPresenterTelemetry>,
    refresh_seq: AtomicU64,
}

unsafe impl<L, F, S> Send for DisplayLinkOwner<L, F, S>
where
    L: DedicatedRenderableLayer<F>,
    F: DedicatedPacedFrame + Clone + Send + 'static,
    S: DedicatedThreadStatus,
{
}

unsafe impl<L, F, S> Sync for DisplayLinkOwner<L, F, S>
where
    L: DedicatedRenderableLayer<F>,
    F: DedicatedPacedFrame + Clone + Send + 'static,
    S: DedicatedThreadStatus,
{
}

struct DedicatedDisplayLink<L, F, S>
where
    L: DedicatedRenderableLayer<F>,
    F: DedicatedPacedFrame + Clone + Send + 'static,
    S: DedicatedThreadStatus,
{
    link: objc2_core_foundation::CFRetained<CVDisplayLink>,
    owner: Box<DisplayLinkOwner<L, F, S>>,
    display_id: CGDirectDisplayID,
}

impl<L, F, S> DedicatedDisplayLink<L, F, S>
where
    L: DedicatedRenderableLayer<F>,
    F: DedicatedPacedFrame + Clone + Send + 'static,
    S: DedicatedThreadStatus,
{
    fn create(
        display_id: CGDirectDisplayID,
        shared: DedicatedThreadShared<L, F, S>,
        telemetry: Arc<DedicatedPresenterTelemetry>,
    ) -> Result<Self, CVReturn> {
        let mut raw: *mut CVDisplayLink = std::ptr::null_mut();
        let status = unsafe {
            #[allow(deprecated)]
            CVDisplayLink::create_with_cg_display(
                display_id,
                NonNull::new(&mut raw).expect("display link out pointer is non-null"),
            )
        };
        if status != kCVReturnSuccess {
            return Err(status);
        }
        let Some(raw) = NonNull::new(raw) else {
            return Err(-1);
        };
        let link = unsafe { objc2_core_foundation::CFRetained::from_raw(raw) };
        let mut owner = Box::new(DisplayLinkOwner {
            shared,
            telemetry,
            refresh_seq: AtomicU64::new(0),
        });
        let user_info = owner.as_mut() as *mut DisplayLinkOwner<L, F, S> as *mut c_void;
        let status = unsafe {
            #[allow(deprecated)]
            link.set_output_callback(Some(display_link_callback::<L, F, S>), user_info)
        };
        if status != kCVReturnSuccess {
            return Err(status);
        }
        let status = {
            #[allow(deprecated)]
            link.start()
        };
        if status != kCVReturnSuccess {
            return Err(status);
        }
        Ok(Self {
            link,
            owner,
            display_id,
        })
    }

    fn actual_refresh_period(&self) -> Option<Duration> {
        #[allow(deprecated)]
        let seconds = self.link.actual_output_video_refresh_period();
        if seconds.is_finite() && seconds > 0.0 {
            Some(Duration::from_secs_f64(seconds))
        } else {
            None
        }
    }
}

impl<L, F, S> Drop for DedicatedDisplayLink<L, F, S>
where
    L: DedicatedRenderableLayer<F>,
    F: DedicatedPacedFrame + Clone + Send + 'static,
    S: DedicatedThreadStatus,
{
    fn drop(&mut self) {
        #[allow(deprecated)]
        let _ = self.link.stop();
        let _ = self.owner.shared.0.lock().is_ok();
    }
}

unsafe extern "C-unwind" fn display_link_callback<L, F, S>(
    _display_link: NonNull<CVDisplayLink>,
    _now: NonNull<CVTimeStamp>,
    output_time: NonNull<CVTimeStamp>,
    _flags_in: CVOptionFlags,
    _flags_out: NonNull<CVOptionFlags>,
    user_info: *mut c_void,
) -> CVReturn
where
    L: DedicatedRenderableLayer<F>,
    F: DedicatedPacedFrame + Clone + Send + 'static,
    S: DedicatedThreadStatus,
{
    let owner = &*(user_info as *const DisplayLinkOwner<L, F, S>);
    let output_time = output_time.as_ref();
    let target_time = host_ticks_to_duration(output_time.hostTime);
    let refresh_period = refresh_period_from_timestamp(output_time);
    let refresh_seq = owner
        .refresh_seq
        .fetch_add(1, Ordering::Relaxed)
        .saturating_add(1);
    drive_display_link_vsync::<L, F, S>(
        &owner.shared,
        Arc::clone(&owner.telemetry),
        refresh_seq,
        target_time,
        refresh_period,
    );
    kCVReturnSuccess
}

/// Minimum on-screen time for the previous drawable before the next one may
/// replace it.
///
/// Half a refresh still allows at most one new picture per vsync, which is all
/// the guard is for. A full refresh period made every present race the panel:
/// whenever the measured period was a hair longer than the real scan-out
/// interval, the drawable missed its vsync, stayed in flight a refresh longer,
/// and the next frame found no free slot. On a 60 Hz panel that cost about one
/// frame in four.
fn present_minimum_duration_s(refresh_period: Duration) -> f64 {
    (refresh_period / 2)
        .max(Duration::from_micros(1))
        .as_secs_f64()
}

fn refresh_period_from_timestamp(timestamp: &CVTimeStamp) -> Duration {
    if timestamp.videoTimeScale > 0 && timestamp.videoRefreshPeriod > 0 {
        let seconds = timestamp.videoRefreshPeriod as f64 / f64::from(timestamp.videoTimeScale);
        if seconds.is_finite() && seconds > 0.0 {
            return Duration::from_secs_f64(seconds);
        }
    }
    FALLBACK_REFRESH_PERIOD
}

fn drive_display_link_vsync<L, F, S>(
    shared: &DedicatedThreadShared<L, F, S>,
    telemetry: Arc<DedicatedPresenterTelemetry>,
    refresh_seq: u64,
    target_time: Duration,
    refresh_period: Duration,
) where
    L: DedicatedRenderableLayer<F>,
    F: DedicatedPacedFrame + Clone + Send + 'static,
    S: DedicatedThreadStatus,
{
    let (mut layer, decision, stopping, layer_generation) = {
        let (lock, _cvar) = &**shared;
        let mut state = lock
            .lock()
            .expect("dedicated presenter thread state poisoned");
        if state.stopping || !state.visible {
            return;
        }
        if state
            .layer
            .as_ref()
            .is_some_and(|layer| !layer.ready_for_frame())
        {
            let before = state.pacer.counters();
            state.pacer.on_refresh_blocked(refresh_seq);
            let after = state.pacer.counters();
            telemetry.add_pacer_delta(before, after);
            telemetry.slot_waits.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let before = state.pacer.counters();
        let decision = state
            .pacer
            .on_refresh(refresh_seq, target_time, refresh_period);
        let after = state.pacer.counters();
        telemetry.add_pacer_delta(before, after);
        (
            state.layer.take(),
            decision,
            state.stopping,
            state.layer_generation,
        )
    };
    if stopping {
        return;
    }
    match decision {
        FramePacerRefresh::Present(frame) => {
            if let Some(layer_ref) = layer.as_mut() {
                match layer_ref.render_at(
                    &frame.frame,
                    frame.callback_host_time,
                    refresh_period,
                    Arc::clone(&telemetry),
                ) {
                    Ok(()) => {
                        let (lock, _) = &**shared;
                        let mut state = lock
                            .lock()
                            .expect("dedicated presenter thread state poisoned");
                        if state.layer_generation == layer_generation && state.layer_installed {
                            state.has_presented_picture = true;
                        }
                    }
                    Err(outcome) => {
                        let status = S::from_outcome(outcome);
                        if status.is_dropped_frame() {
                            telemetry.drops.fetch_add(1, Ordering::Relaxed);
                        } else if status.is_skipped_occluded() {
                            telemetry.skips.fetch_add(1, Ordering::Relaxed);
                        }
                        let (lock, _) = &**shared;
                        let mut state = lock
                            .lock()
                            .expect("dedicated presenter thread state poisoned");
                        if state.layer_generation == layer_generation {
                            state.async_status = Some(status);
                            if let Some(wake) = state.wake.as_ref() {
                                wake.request_repaint();
                            }
                            if status.is_fallback() {
                                state.layer_installed = false;
                                state.has_presented_picture = false;
                            }
                        }
                    }
                }
            }
        }
        FramePacerRefresh::Idle => {}
    }
    let (lock, _) = &**shared;
    let mut state = lock
        .lock()
        .expect("dedicated presenter thread state poisoned");
    if !state.stopping && state.layer_installed && state.layer_generation == layer_generation {
        state.layer = layer;
    }
}

impl<L, F, S> DedicatedPresenterThread<L, F, S>
where
    L: DedicatedRenderableLayer<F>,
    F: DedicatedPacedFrame + Clone + Send + 'static,
    S: DedicatedThreadStatus,
{
    fn ensure_display_link(&mut self, display_id: Option<CGDirectDisplayID>) {
        let Some(display_id) = display_id else {
            return;
        };
        if self
            .display_link
            .as_ref()
            .is_some_and(|link| link.display_id == display_id)
        {
            return;
        }
        self.display_link = None;
        self.telemetry.reset_presentation_baseline();
        match DedicatedDisplayLink::create(
            display_id,
            Arc::clone(&self.shared),
            Arc::clone(&self.telemetry),
        ) {
            Ok(link) => {
                tracing::debug!(
                    target: crate::logging::target::VIDEO,
                    display_id,
                    refresh_hz = ?link.actual_refresh_period().map(|period| 1.0 / period.as_secs_f64()),
                    "dedicated video CVDisplayLink started",
                );
                self.display_link = Some(link);
            }
            Err(status) => tracing::warn!(
                target: crate::logging::target::VIDEO,
                display_id,
                status,
                "failed to start CVDisplayLink for dedicated video presenter",
            ),
        }
    }

    fn install_layer(
        &mut self,
        layer: L,
        visible: bool,
        geometry: DedicatedLayerGeometry,
        display_id: Option<CGDirectDisplayID>,
    ) {
        if visible {
            self.ensure_display_link(display_id);
        } else {
            self.display_link = None;
        }
        let (lock, _cvar) = &*self.shared;
        let mut state = lock
            .lock()
            .expect("dedicated presenter thread state poisoned");
        if retained_handle_detach_before_install(state.layer_handle.is_some()) {
            let old_layer = state
                .layer_handle
                .take()
                .expect("checked layer_handle is_some above");
            old_layer.removeFromSuperlayer();
        }
        state.layer_generation = state.layer_generation.saturating_add(1);
        state.layer_handle = Some(layer.layer_handle());
        state.layer = Some(layer);
        state.layer_installed = true;
        state.has_presented_picture = false;
        state.async_status = None;
        state.visible = visible;
        state.last_geometry = Some(geometry);
        state.display_id = display_id;
        state.pacer.clear(FramePacerDropReason::Hidden);
        state.last_frame = None;
    }

    fn resize_and_visibility(
        &mut self,
        geometry: DedicatedLayerGeometry,
        visible: bool,
        display_id: Option<CGDirectDisplayID>,
    ) {
        if visible {
            self.ensure_display_link(display_id);
        } else {
            self.display_link = None;
        }
        let (lock, _cvar) = &*self.shared;
        let mut state = lock
            .lock()
            .expect("dedicated presenter thread state poisoned");
        let geometry_changed = state.last_geometry != Some(geometry);
        let display_changed = state.display_id != display_id;
        if geometry_changed {
            if let Some(layer) = state.layer_handle.as_ref() {
                apply_dedicated_layer_geometry(&**layer, geometry);
            }
            state.last_geometry = Some(geometry);
        }
        let became_visible = !state.visible && visible;
        state.display_id = display_id;
        state.visible = visible;
        if display_changed {
            state.pacer.clear(FramePacerDropReason::Hidden);
            state.depth_reset_for_tests();
        }
        if became_visible {
            if let Some((frame, arrival_host_time)) = state
                .last_frame
                .as_ref()
                .filter(|retained| !retained.replayed_after_restore)
                .map(|retained| (retained.frame.clone(), retained.arrival_host_time))
            {
                let before = state.pacer.counters();
                let admission_host_time = current_host_time();
                state
                    .pacer
                    .enqueue_replay(frame, arrival_host_time, admission_host_time);
                let after = state.pacer.counters();
                self.telemetry.add_pacer_delta(before, after);
                if let Some(retained) = state.last_frame.as_mut() {
                    retained.replayed_after_restore = true;
                }
                state.accepted_frames = state.accepted_frames.saturating_add(1);
            }
        }
    }

    fn set_visible(&mut self, visible: bool) {
        if !visible {
            self.display_link = None;
        }
        let display_id = if visible {
            self.shared
                .0
                .lock()
                .expect("dedicated presenter thread state poisoned")
                .display_id
        } else {
            None
        };
        if visible {
            self.ensure_display_link(display_id);
        }
        let (lock, _cvar) = &*self.shared;
        let mut state = lock
            .lock()
            .expect("dedicated presenter thread state poisoned");
        let became_hidden = state.visible && !visible;
        let became_visible = !state.visible && visible;
        state.visible = visible;
        if became_hidden {
            state.pacer.clear(FramePacerDropReason::Hidden);
        }
        if became_visible {
            if let Some((frame, arrival_host_time)) = state
                .last_frame
                .as_ref()
                .filter(|retained| !retained.replayed_after_restore)
                .map(|retained| (retained.frame.clone(), retained.arrival_host_time))
            {
                let before = state.pacer.counters();
                let admission_host_time = current_host_time();
                state
                    .pacer
                    .enqueue_replay(frame, arrival_host_time, admission_host_time);
                let after = state.pacer.counters();
                self.telemetry.add_pacer_delta(before, after);
                if let Some(retained) = state.last_frame.as_mut() {
                    retained.replayed_after_restore = true;
                }
                state.accepted_frames = state.accepted_frames.saturating_add(1);
            }
        }
    }

    fn set_wake(&self, wake: egui::Context) {
        self.shared
            .0
            .lock()
            .expect("dedicated presenter thread state poisoned")
            .wake = Some(wake);
    }

    fn submit(&self, frame: F, arrival_host_time: Duration) -> bool {
        let (lock, _cvar) = &*self.shared;
        let mut state = lock
            .lock()
            .expect("dedicated presenter thread state poisoned");
        if !state.layer_installed {
            return false;
        }
        state.last_frame = Some(RetainedPresentationFrame {
            frame: frame.clone(),
            arrival_host_time,
            replayed_after_restore: false,
        });
        tracing::debug!(
            target: crate::logging::target::VIDEO,
            arrival_host_time_us = duration_micros(arrival_host_time),
            queue_depth = state.pacer.queue_depth(),
            "dedicated presenter frame arrival"
        );
        state.pacer.set_source_fps(frame.source_fps());
        let before = state.pacer.counters();
        state.pacer.enqueue(frame, arrival_host_time);
        let after = state.pacer.counters();
        self.telemetry.add_pacer_delta(before, after);
        state.accepted_frames = state.accepted_frames.saturating_add(1);
        true
    }

    fn retain_latest_while_hidden(&self, frame: F, arrival_host_time: Duration) -> bool {
        let (lock, _) = &*self.shared;
        let mut state = lock
            .lock()
            .expect("dedicated presenter thread state poisoned");
        if !state.layer_installed {
            return false;
        }
        state.pacer.set_source_fps(frame.source_fps());
        state.last_frame = Some(RetainedPresentationFrame {
            frame,
            arrival_host_time,
            replayed_after_restore: false,
        });
        let before = state.pacer.counters();
        state.pacer.clear(FramePacerDropReason::Hidden);
        let after = state.pacer.counters();
        self.telemetry.add_pacer_delta(before, after);
        true
    }

    fn has_presented_picture(&self) -> bool {
        self.shared
            .0
            .lock()
            .expect("dedicated presenter thread state poisoned")
            .has_presented_picture
    }

    fn take_telemetry(&self) -> DedicatedPresenterTelemetryDelta {
        self.telemetry.drain()
    }

    fn has_layer(&self) -> bool {
        self.shared
            .0
            .lock()
            .expect("dedicated presenter thread state poisoned")
            .layer_installed
    }

    fn teardown(&mut self) {
        self.display_link = None;
        {
            let (lock, cvar) = &*self.shared;
            let mut state = lock
                .lock()
                .expect("dedicated presenter thread state poisoned");
            if let Some(layer) = state.layer_handle.take() {
                if MainThreadMarker::new().is_some() {
                    layer.removeFromSuperlayer();
                }
            }
            state.layer = None;
            state.layer_installed = false;
            state.has_presented_picture = false;
            state.pacer.clear(FramePacerDropReason::Hidden);
            state.last_frame = None;
            state.last_geometry = None;
            state.display_id = None;
            state.stopping = true;
            cvar.notify_one();
        }
        self.shared = Arc::new((
            Mutex::new(DedicatedPresenterThreadState::default()),
            Condvar::new(),
        ));
        self.telemetry = Arc::new(DedicatedPresenterTelemetry::default());
    }

    fn take_async_status(&self) -> Option<S> {
        self.shared
            .0
            .lock()
            .expect("dedicated presenter thread state poisoned")
            .async_status
            .take()
    }

    #[cfg(test)]
    fn mark_presented_for_test(&self) {
        self.shared
            .0
            .lock()
            .expect("dedicated presenter thread state poisoned")
            .has_presented_picture = true;
    }

    #[cfg(test)]
    fn add_telemetry_for_test(&self, drops: u64, skips: u64) {
        self.telemetry.drops.fetch_add(drops, Ordering::Relaxed);
        self.telemetry.skips.fetch_add(skips, Ordering::Relaxed);
    }

    #[cfg(test)]
    fn inject_async_status_for_test(&self, status: S) {
        self.shared
            .0
            .lock()
            .expect("dedicated presenter thread state poisoned")
            .async_status = Some(status);
    }

    #[cfg(test)]
    fn queue_depth_for_test(&self) -> usize {
        self.shared
            .0
            .lock()
            .expect("dedicated presenter thread state poisoned")
            .pacer
            .queue_depth()
    }

    #[cfg(test)]
    fn pacer_counters_for_test(&self) -> FramePacerCounters {
        self.shared
            .0
            .lock()
            .expect("dedicated presenter thread state poisoned")
            .pacer
            .counters()
    }

    #[cfg(test)]
    fn accepted_frames_for_test(&self) -> u64 {
        self.shared
            .0
            .lock()
            .expect("dedicated presenter thread state poisoned")
            .accepted_frames
    }

    #[cfg(test)]
    fn retained_arrival_for_test(&self) -> Option<Duration> {
        self.shared
            .0
            .lock()
            .expect("dedicated presenter thread state poisoned")
            .last_frame
            .as_ref()
            .map(|frame| frame.arrival_host_time)
    }
}

type DedicatedEightBitPresenterThread = DedicatedPresenterThread<
    DedicatedEightBitVideoLayer,
    DedicatedEightBitLayerFrame,
    DedicatedEightBitPresentationStatus,
>;

type DedicatedTenBitPresenterThread =
    DedicatedPresenterThread<DedicatedVideoLayer, DedicatedLayerFrame, DedicatedPresentationStatus>;

impl DedicatedEightBitVideoPresenter {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub(crate) fn attach_or_resize_egui(
        &mut self,
        rect: egui::Rect,
        visible: bool,
    ) -> DedicatedEightBitPresentationStatus {
        self.surface_visible = visible;
        let Some(mtm) = MainThreadMarker::new() else {
            return self.record_outcome(DedicatedLayerOutcome::NotMainThread);
        };
        let Some(window) = super::video_render::find_root_window(mtm) else {
            return self.record_outcome(DedicatedLayerOutcome::NoRootWindow);
        };
        let Some(view) = window.contentView() else {
            return self.record_outcome(DedicatedLayerOutcome::NoContentView);
        };
        let display_id = display_id_for_window(&window);
        log_window_presenter_state(&window, display_id, None, "attach_or_resize_8bit");
        let bounds = view.bounds();
        let content_rect = CGRect {
            origin: CGPoint {
                x: f64::from(rect.left()),
                y: bounds.size.height - f64::from(rect.bottom()),
            },
            size: CGSize {
                width: f64::from(rect.width()),
                height: f64::from(rect.height()),
            },
        };
        self.attach_or_resize_on_display(content_rect, window.backingScaleFactor(), display_id)
    }

    fn attach_or_resize_on_display(
        &mut self,
        rect: CGRect,
        contents_scale: f64,
        display_id: Option<CGDirectDisplayID>,
    ) -> DedicatedEightBitPresentationStatus {
        let geometry = DedicatedLayerGeometry::new(rect, contents_scale);
        let outcome = if self.thread.has_layer() {
            self.thread
                .resize_and_visibility(geometry, self.surface_visible, display_id);
            DedicatedLayerOutcome::Ready
        } else {
            match DedicatedEightBitVideoLayer::attach(rect) {
                Ok(layer) => {
                    self.thread
                        .install_layer(layer, self.surface_visible, geometry, display_id);
                    DedicatedLayerOutcome::Ready
                }
                Err(outcome) => outcome,
            }
        };
        if outcome == DedicatedLayerOutcome::Ready {
            self.last_rect = Some(rect);
        }
        self.record_outcome(outcome)
    }

    pub(crate) fn present(
        &mut self,
        frame: &DedicatedEightBitLayerFrame,
    ) -> DedicatedEightBitPresentationStatus {
        if !self.surface_visible {
            let _ = self
                .thread
                .retain_latest_while_hidden(frame.clone(), frame.arrival_host_time);
            self.thread.telemetry.skips.fetch_add(1, Ordering::Relaxed);
            self.presentation_status = DedicatedEightBitPresentationStatus::SkippedOccluded;
            return self.presentation_status;
        }
        let outcome = if self.thread.submit(frame.clone(), frame.arrival_host_time) {
            DedicatedLayerOutcome::Ready
        } else {
            DedicatedLayerOutcome::NoRootLayer
        };
        if outcome == DedicatedLayerOutcome::NoDrawableAvailable {
            self.presentation_status = DedicatedEightBitPresentationStatus::DroppedFrame;
            return self.presentation_status;
        }
        self.record_outcome(outcome)
    }

    pub(crate) fn set_surface_visible(&mut self, visible: bool) {
        self.surface_visible = visible;
        self.thread.set_visible(visible);
    }

    pub(crate) fn set_wake_context(&mut self, ctx: egui::Context) {
        self.thread.set_wake(ctx);
    }

    pub(crate) fn take_async_status(&mut self) -> Option<DedicatedEightBitPresentationStatus> {
        self.thread.take_async_status()
    }

    pub(crate) fn has_presented_picture(&self) -> bool {
        self.thread.has_presented_picture()
    }

    pub(crate) fn take_telemetry(&self) -> DedicatedPresenterTelemetryDelta {
        self.thread.take_telemetry()
    }

    #[cfg(test)]
    pub(crate) fn inject_async_status_for_test(
        &mut self,
        status: DedicatedEightBitPresentationStatus,
    ) {
        self.thread.inject_async_status_for_test(status);
    }

    #[cfg(test)]
    pub(crate) fn mark_presented_for_test(&mut self) {
        self.thread.mark_presented_for_test();
    }

    #[cfg(test)]
    pub(crate) fn add_telemetry_for_test(&mut self, drops: u64, skips: u64) {
        self.thread.add_telemetry_for_test(drops, skips);
    }

    fn record_outcome(
        &mut self,
        outcome: DedicatedLayerOutcome,
    ) -> DedicatedEightBitPresentationStatus {
        let status = DedicatedEightBitPresentationStatus::from_outcome(outcome);
        self.presentation_status = status;
        if let Some(logged) = self.fallback.record(outcome) {
            if logged == DedicatedLayerOutcome::Ready {
                tracing::info!(
                    target: crate::logging::target::VIDEO,
                    "established the dedicated 8-bit CAMetalLayer video path; \
                     decoded IOSurfaces are presented without CPU copy",
                );
            } else {
                tracing::warn!(
                    target: crate::logging::target::VIDEO,
                    ?logged,
                    "dedicated 8-bit video layer unavailable; falling back to the \
                     existing wgpu/egui upload path",
                );
            }
        }
        status
    }

    pub fn teardown(&mut self) {
        self.thread.teardown();
        self.last_rect = None;
        self.presentation_status = DedicatedEightBitPresentationStatus::Inactive;
    }
}

// ============================================================================
// Dedicated presenter-thread wiring shared by the 8-bit and 10-bit layers.
// ============================================================================

/// Owns the dedicated 10-bit video presentation path end to end: main-thread
/// layer attachment plus the same presenter-thread/latest-frame mailbox used
/// by Auto/Speed's 8-bit path.
#[derive(Default)]
pub struct DedicatedVideoPresenter {
    thread: DedicatedTenBitPresenterThread,
    fallback: DedicatedLayerFallback,
    last_rect: Option<CGRect>,
    presentation_status: DedicatedPresentationStatus,
    surface_visible: bool,
}

// SAFETY: all cross-thread rendering is guarded by the presenter's external
// `Mutex`; see `DedicatedVideoLayer`'s `Send` justification. Geometry mutation
// remains a main-thread-only method.
unsafe impl Send for DedicatedVideoPresenter {}

impl DedicatedVideoPresenter {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub(crate) fn attach_or_resize_egui(
        &mut self,
        rect: egui::Rect,
        visible: bool,
    ) -> DedicatedPresentationStatus {
        self.surface_visible = visible;
        let Some(mtm) = MainThreadMarker::new() else {
            return self.record_outcome(DedicatedLayerOutcome::NotMainThread);
        };
        let Some(window) = super::video_render::find_root_window(mtm) else {
            return self.record_outcome(DedicatedLayerOutcome::NoRootWindow);
        };
        let Some(view) = window.contentView() else {
            return self.record_outcome(DedicatedLayerOutcome::NoContentView);
        };
        let display_id = display_id_for_window(&window);
        log_window_presenter_state(&window, display_id, None, "attach_or_resize_10bit");
        let bounds = view.bounds();
        let content_rect = CGRect {
            origin: CGPoint {
                x: f64::from(rect.left()),
                y: bounds.size.height - f64::from(rect.bottom()),
            },
            size: CGSize {
                width: f64::from(rect.width()),
                height: f64::from(rect.height()),
            },
        };
        self.attach_or_resize_on_display(content_rect, window.backingScaleFactor(), display_id)
    }

    fn attach_or_resize_on_display(
        &mut self,
        rect: CGRect,
        contents_scale: f64,
        display_id: Option<CGDirectDisplayID>,
    ) -> DedicatedPresentationStatus {
        let geometry = DedicatedLayerGeometry::new(rect, contents_scale);
        let outcome = if self.thread.has_layer() {
            self.thread
                .resize_and_visibility(geometry, self.surface_visible, display_id);
            DedicatedLayerOutcome::Ready
        } else {
            match DedicatedVideoLayer::attach(rect) {
                Ok(layer) => {
                    self.thread
                        .install_layer(layer, self.surface_visible, geometry, display_id);
                    DedicatedLayerOutcome::Ready
                }
                Err(outcome) => outcome,
            }
        };
        if outcome == DedicatedLayerOutcome::Ready {
            self.last_rect = Some(rect);
        }
        self.record_outcome(outcome)
    }

    pub(crate) fn present(&mut self, frame: &DedicatedLayerFrame) -> DedicatedPresentationStatus {
        if !self.surface_visible {
            let _ = self
                .thread
                .retain_latest_while_hidden(frame.clone(), frame.arrival_host_time);
            self.thread.telemetry.skips.fetch_add(1, Ordering::Relaxed);
            self.presentation_status = DedicatedPresentationStatus::SkippedOccluded;
            return self.presentation_status;
        }
        let outcome = if self.thread.submit(frame.clone(), frame.arrival_host_time) {
            DedicatedLayerOutcome::Ready
        } else {
            DedicatedLayerOutcome::NoRootLayer
        };
        if outcome == DedicatedLayerOutcome::NoDrawableAvailable {
            self.presentation_status = DedicatedPresentationStatus::DroppedFrame;
            return self.presentation_status;
        }
        self.record_outcome(outcome)
    }

    pub(crate) fn set_surface_visible(&mut self, visible: bool) {
        self.surface_visible = visible;
        self.thread.set_visible(visible);
    }

    pub(crate) fn set_wake_context(&mut self, ctx: egui::Context) {
        self.thread.set_wake(ctx);
    }

    pub(crate) fn take_async_status(&mut self) -> Option<DedicatedPresentationStatus> {
        self.thread.take_async_status()
    }

    pub(crate) fn has_presented_picture(&self) -> bool {
        self.thread.has_presented_picture()
    }

    pub(crate) fn take_telemetry(&self) -> DedicatedPresenterTelemetryDelta {
        self.thread.take_telemetry()
    }

    #[cfg(test)]
    pub(crate) fn inject_async_status_for_test(&mut self, status: DedicatedPresentationStatus) {
        self.thread.inject_async_status_for_test(status);
    }

    #[cfg(test)]
    pub(crate) fn mark_presented_for_test(&mut self) {
        self.thread.mark_presented_for_test();
    }

    fn record_outcome(&mut self, outcome: DedicatedLayerOutcome) -> DedicatedPresentationStatus {
        let status = DedicatedPresentationStatus::from_outcome(outcome);
        self.presentation_status = status;
        if let Some(logged) = self.fallback.record(outcome) {
            if logged == DedicatedLayerOutcome::Ready {
                tracing::info!(
                    target: crate::logging::target::VIDEO,
                    "established the dedicated 10-bit CAMetalLayer video path;                      layer and drawable are RGB10A2Unorm",
                );
            } else {
                tracing::warn!(
                    target: crate::logging::target::VIDEO,
                    ?logged,
                    "dedicated 10-bit video layer unavailable; falling back to the                      existing 8-bit wgpu/egui video path",
                );
            }
        }
        status
    }

    pub fn teardown(&mut self) {
        self.thread.teardown();
        self.last_rect = None;
        self.presentation_status = DedicatedPresentationStatus::Inactive;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;
    use std::thread::ThreadId;

    #[derive(Debug, Clone, PartialEq)]
    enum GeometryCall {
        Begin(ThreadId),
        DisableActions(bool, ThreadId),
        ContentsScale(f64, ThreadId),
        Frame(CGRect, ThreadId),
        Commit(ThreadId),
    }

    #[derive(Default)]
    struct FakeGeometryTarget {
        calls: StdMutex<Vec<GeometryCall>>,
    }

    impl FakeGeometryTarget {
        fn calls(&self) -> Vec<GeometryCall> {
            self.calls.lock().expect("calls").clone()
        }

        fn push(&self, call: GeometryCall) {
            self.calls.lock().expect("calls").push(call);
        }
    }

    impl DedicatedLayerGeometryTarget for FakeGeometryTarget {
        fn begin_geometry_transaction(&self) {
            self.push(GeometryCall::Begin(std::thread::current().id()));
        }

        fn set_geometry_actions_disabled(&self, disabled: bool) {
            self.push(GeometryCall::DisableActions(
                disabled,
                std::thread::current().id(),
            ));
        }

        fn set_geometry_contents_scale(&self, contents_scale: f64) {
            self.push(GeometryCall::ContentsScale(
                contents_scale,
                std::thread::current().id(),
            ));
        }

        fn set_geometry_frame(&self, rect: CGRect) {
            self.push(GeometryCall::Frame(rect, std::thread::current().id()));
        }

        fn commit_geometry_transaction(&self) {
            self.push(GeometryCall::Commit(std::thread::current().id()));
        }
    }

    #[derive(Debug, Clone, PartialEq)]
    enum DrawableCall {
        Begin(ThreadId),
        DisableActions(bool, ThreadId),
        DrawableSize(CGSize, ThreadId),
        Commit(ThreadId),
        Flush(ThreadId),
    }

    #[derive(Default)]
    struct FakeDrawableTarget {
        calls: StdMutex<Vec<DrawableCall>>,
    }

    impl FakeDrawableTarget {
        fn calls(&self) -> Vec<DrawableCall> {
            self.calls.lock().expect("calls").clone()
        }

        fn push(&self, call: DrawableCall) {
            self.calls.lock().expect("calls").push(call);
        }
    }

    impl DedicatedLayerDrawableTarget for FakeDrawableTarget {
        fn begin_drawable_transaction(&self) {
            self.push(DrawableCall::Begin(std::thread::current().id()));
        }

        fn set_drawable_actions_disabled(&self, disabled: bool) {
            self.push(DrawableCall::DisableActions(
                disabled,
                std::thread::current().id(),
            ));
        }

        fn set_source_drawable_size(&self, drawable_size: CGSize) {
            self.push(DrawableCall::DrawableSize(
                drawable_size,
                std::thread::current().id(),
            ));
        }

        fn commit_drawable_transaction(&self) {
            self.push(DrawableCall::Commit(std::thread::current().id()));
        }

        fn flush_drawable_transaction(&self) {
            self.push(DrawableCall::Flush(std::thread::current().id()));
        }
    }

    #[test]
    fn geometry_transaction_applies_frame_and_scale_on_caller_thread() {
        let target = FakeGeometryTarget::default();
        let rect = CGRect {
            origin: CGPoint { x: 2.0, y: 3.0 },
            size: CGSize {
                width: 1120.0,
                height: 760.0,
            },
        };
        let geometry = DedicatedLayerGeometry::new(rect, 2.0);
        let caller = std::thread::current().id();

        apply_dedicated_layer_geometry(&target, geometry);

        assert_eq!(
            target.calls(),
            vec![
                GeometryCall::Begin(caller),
                GeometryCall::DisableActions(true, caller),
                GeometryCall::ContentsScale(2.0, caller),
                GeometryCall::Frame(rect, caller),
                GeometryCall::Commit(caller),
            ],
        );
    }

    #[test]
    fn drawable_size_follows_source_not_viewport_geometry() {
        let target = FakeDrawableTarget::default();
        let rect = CGRect {
            origin: CGPoint { x: 0.0, y: 0.0 },
            size: CGSize {
                width: 1800.0,
                height: 1169.0,
            },
        };
        let geometry = DedicatedLayerGeometry::new(rect, 2.0);
        let source = source_drawable_size(1120, 760);
        let caller = std::thread::current().id();

        assert_eq!(geometry.rect, rect, "layer frame still follows viewport");
        apply_dedicated_layer_source_drawable_size(&target, source);

        assert_eq!(
            target.calls(),
            vec![
                DrawableCall::Begin(caller),
                DrawableCall::DisableActions(true, caller),
                DrawableCall::DrawableSize(
                    CGSize {
                        width: 1120.0,
                        height: 760.0,
                    },
                    caller,
                ),
                DrawableCall::Commit(caller),
                DrawableCall::Flush(caller),
            ],
            "drawable size must stay at source resolution so Core Animation, not the shader, scales"
        );
    }

    #[test]
    fn presenter_teardown_resets_presented_picture_state() {
        let mut presenter = DedicatedEightBitVideoPresenter::new();
        presenter.mark_presented_for_test();
        assert!(presenter.has_presented_picture());

        presenter.teardown();

        assert!(!presenter.has_presented_picture());
    }

    #[test]
    fn hidden_to_visible_replays_retained_frame_once_with_original_arrival() {
        let mut thread: DedicatedPresenterThread<
            FakeGeometryLayer,
            u32,
            DedicatedPresentationStatus,
        > = DedicatedPresenterThread::default();
        let rendered = FakeGeometryLayer::default();
        let rendered_probe = rendered.clone();
        {
            let (lock, _cvar) = &*thread.shared;
            let mut state = lock.lock().expect("state");
            state.layer_installed = true;
            state.visible = false;
            state.layer = Some(rendered);
        }
        let arrival = Duration::from_millis(42);
        assert!(thread.retain_latest_while_hidden(7, arrival));
        assert_eq!(thread.retained_arrival_for_test(), Some(arrival));

        thread.set_visible(true);
        assert_eq!(thread.queue_depth_for_test(), 1);
        assert_eq!(thread.accepted_frames_for_test(), 1);
        drive_display_link_vsync(
            &thread.shared,
            Arc::clone(&thread.telemetry),
            1,
            current_host_time().saturating_add(Duration::from_millis(16)),
            Duration::from_millis(16),
        );
        assert_eq!(rendered_probe.rendered(), vec![7]);

        thread.set_visible(true);
        assert_eq!(
            thread.queue_depth_for_test(),
            0,
            "unchanged visible passes must not enqueue retained content again"
        );
        assert_eq!(thread.accepted_frames_for_test(), 1);
    }

    #[test]
    fn hiding_suspends_and_drops_queued_frames_as_hidden() {
        let mut thread: DedicatedPresenterThread<
            FakeGeometryLayer,
            u32,
            DedicatedPresentationStatus,
        > = DedicatedPresenterThread::default();
        {
            let (lock, _cvar) = &*thread.shared;
            let mut state = lock.lock().expect("state");
            state.layer_installed = true;
            state.visible = true;
        }
        assert!(thread.submit(1, Duration::from_millis(1)));
        assert!(thread.submit(2, Duration::from_millis(2)));
        assert_eq!(thread.queue_depth_for_test(), 2);

        thread.set_visible(false);

        assert_eq!(thread.queue_depth_for_test(), 0);
        assert_eq!(thread.pacer_counters_for_test().dropped_hidden, 2);
    }

    #[test]
    fn display_link_callback_presents_at_most_one_oldest_frame_per_refresh() {
        let thread: DedicatedPresenterThread<FakeGeometryLayer, u32, DedicatedPresentationStatus> =
            DedicatedPresenterThread::default();
        let rendered = FakeGeometryLayer::default();
        let rendered_probe = rendered.clone();
        {
            let (lock, _cvar) = &*thread.shared;
            let mut state = lock.lock().expect("state");
            state.layer_installed = true;
            state.visible = true;
            state.layer = Some(rendered);
        }
        assert!(thread.submit(1, Duration::from_millis(1)));
        assert!(thread.submit(2, Duration::from_millis(2)));
        assert!(thread.submit(3, Duration::from_millis(3)));
        assert!(thread.submit(4, Duration::from_millis(4)));

        drive_display_link_vsync(
            &thread.shared,
            Arc::clone(&thread.telemetry),
            1,
            Duration::from_millis(16),
            Duration::from_millis(16),
        );
        drive_display_link_vsync(
            &thread.shared,
            Arc::clone(&thread.telemetry),
            1,
            Duration::from_millis(16),
            Duration::from_millis(16),
        );
        drive_display_link_vsync(
            &thread.shared,
            Arc::clone(&thread.telemetry),
            2,
            Duration::from_millis(32),
            Duration::from_millis(16),
        );

        assert_eq!(rendered_probe.rendered(), vec![2, 3]);
        let counters = thread.pacer_counters_for_test();
        assert_eq!(counters.dropped_overflow, 1);
        assert_eq!(counters.frames_presented, 2);
    }

    #[test]
    fn a_refresh_without_a_free_drawable_slot_keeps_the_frame_for_the_next_refresh() {
        let thread: DedicatedPresenterThread<FakeGeometryLayer, u32, DedicatedPresentationStatus> =
            DedicatedPresenterThread::default();
        let rendered = FakeGeometryLayer::default();
        let rendered_probe = rendered.clone();
        {
            let (lock, _cvar) = &*thread.shared;
            let mut state = lock.lock().expect("state");
            state.layer_installed = true;
            state.visible = true;
            state.layer = Some(rendered);
        }
        assert!(thread.submit(1, Duration::from_millis(1)));
        assert!(thread.submit(2, Duration::from_millis(17)));

        rendered_probe.set_slots_busy(true);
        drive_display_link_vsync(
            &thread.shared,
            Arc::clone(&thread.telemetry),
            1,
            Duration::from_millis(18),
            Duration::from_millis(16),
        );
        assert!(rendered_probe.rendered().is_empty());
        assert_eq!(thread.queue_depth_for_test(), 2);

        rendered_probe.set_slots_busy(false);
        for (seq, at) in [(2, 34), (3, 50)] {
            drive_display_link_vsync(
                &thread.shared,
                Arc::clone(&thread.telemetry),
                seq,
                Duration::from_millis(at),
                Duration::from_millis(16),
            );
        }

        assert_eq!(rendered_probe.rendered(), vec![1, 2]);
        let counters = thread.pacer_counters_for_test();
        assert_eq!(counters.never_presented(), 0);
        let delta = thread.telemetry.drain();
        assert_eq!(delta.slot_waits, 1);
        assert_eq!(delta.drops, 0);
    }

    #[test]
    fn a_multi_refresh_slot_wait_still_presents_the_newest_frame() {
        let thread: DedicatedPresenterThread<FakeGeometryLayer, u32, DedicatedPresentationStatus> =
            DedicatedPresenterThread::default();
        let rendered = FakeGeometryLayer::default();
        let rendered_probe = rendered.clone();
        {
            let (lock, _cvar) = &*thread.shared;
            let mut state = lock.lock().expect("state");
            state.layer_installed = true;
            state.visible = true;
            state.layer = Some(rendered);
        }
        assert!(thread.submit(1, Duration::from_millis(1)));
        drive_display_link_vsync(
            &thread.shared,
            Arc::clone(&thread.telemetry),
            1,
            Duration::from_millis(2),
            Duration::from_millis(16),
        );
        rendered_probe.set_slots_busy(true);
        for seq in 2..=5_u64 {
            let at = seq * 16;
            assert!(thread.submit(
                u32::try_from(seq).expect("small"),
                Duration::from_millis(at)
            ));
            drive_display_link_vsync(
                &thread.shared,
                Arc::clone(&thread.telemetry),
                seq,
                Duration::from_millis(at + 1),
                Duration::from_millis(16),
            );
        }
        rendered_probe.set_slots_busy(false);
        drive_display_link_vsync(
            &thread.shared,
            Arc::clone(&thread.telemetry),
            6,
            Duration::from_millis(97),
            Duration::from_millis(16),
        );

        assert_eq!(rendered_probe.rendered(), vec![1, 4]);
        assert_eq!(thread.pacer_counters_for_test().underruns, 0);
        assert_eq!(thread.telemetry.drain().slot_waits, 4);
    }

    #[test]
    fn present_minimum_duration_is_half_a_refresh() {
        let sixty_hz = Duration::from_nanos(16_666_667);
        let minimum = present_minimum_duration_s(sixty_hz);
        assert!((minimum - 0.008_333).abs() < 0.000_01, "{minimum}");
        assert!(present_minimum_duration_s(Duration::ZERO) > 0.0);
    }

    #[test]
    fn epoch_reset_allows_new_display_link_sequence_after_hide_and_display_change() {
        let thread: DedicatedPresenterThread<FakeGeometryLayer, u32, DedicatedPresentationStatus> =
            DedicatedPresenterThread::default();
        let rendered = FakeGeometryLayer::default();
        let rendered_probe = rendered.clone();
        {
            let (lock, _cvar) = &*thread.shared;
            let mut state = lock.lock().expect("state");
            state.layer_installed = true;
            state.visible = true;
            state.layer = Some(rendered);
        }
        for seq in 1..=600 {
            drive_display_link_vsync(
                &thread.shared,
                Arc::clone(&thread.telemetry),
                seq,
                Duration::from_millis(seq),
                Duration::from_millis(16),
            );
        }

        thread.submit(10, Duration::from_millis(601));
        {
            let (lock, _cvar) = &*thread.shared;
            lock.lock()
                .expect("state")
                .pacer
                .clear(FramePacerDropReason::Hidden);
        }
        thread.submit(11, Duration::from_millis(602));
        drive_display_link_vsync(
            &thread.shared,
            Arc::clone(&thread.telemetry),
            1,
            Duration::from_millis(603),
            Duration::from_millis(16),
        );
        assert_eq!(rendered_probe.rendered(), vec![11]);

        {
            let (lock, _cvar) = &*thread.shared;
            lock.lock()
                .expect("state")
                .pacer
                .clear(FramePacerDropReason::Hidden);
        }
        thread.submit(12, Duration::from_millis(604));
        drive_display_link_vsync(
            &thread.shared,
            Arc::clone(&thread.telemetry),
            1,
            Duration::from_millis(605),
            Duration::from_millis(16),
        );
        assert_eq!(rendered_probe.rendered(), vec![11, 12]);
    }

    #[derive(Clone, Default)]
    struct FakeGeometryLayer {
        rendered: Arc<StdMutex<Vec<u32>>>,
        slots_busy: Arc<StdMutex<bool>>,
    }

    impl FakeGeometryLayer {
        fn rendered(&self) -> Vec<u32> {
            self.rendered.lock().expect("rendered frames").clone()
        }

        fn set_slots_busy(&self, busy: bool) {
            *self.slots_busy.lock().expect("slot state") = busy;
        }
    }

    unsafe impl Send for FakeGeometryLayer {}

    impl DedicatedRenderableLayer<u32> for FakeGeometryLayer {
        fn render_at(
            &mut self,
            frame: &u32,
            target_host_time: Duration,
            refresh_period: Duration,
            telemetry: Arc<DedicatedPresenterTelemetry>,
        ) -> Result<(), DedicatedLayerOutcome> {
            telemetry.record_submitted(target_host_time, Duration::ZERO, refresh_period);
            self.rendered.lock().expect("rendered frames").push(*frame);
            Ok(())
        }

        fn layer_handle(&self) -> Retained<CAMetalLayer> {
            panic!("fake layer handle is never read by this unit test")
        }

        fn ready_for_frame(&self) -> bool {
            !*self.slots_busy.lock().expect("slot state")
        }
    }

    // ---- RGB10A2Unorm: the task-brief correction -------------------------

    #[test]
    fn zero_presented_time_callbacks_are_unconfirmed_but_submitted_is_estimated() {
        let telemetry = DedicatedPresenterTelemetry::default();
        for _ in 0..10 {
            telemetry.record_submitted(
                Duration::from_millis(16),
                Duration::ZERO,
                Duration::from_millis(16),
            );
            telemetry.record_presented_time_unconfirmed();
        }
        let delta = telemetry.drain();
        assert_eq!(delta.frames_submitted, 10);
        assert_eq!(delta.presented_time_unconfirmed, 10);
        assert_eq!(delta.frames_confirmed, 0);
        assert_eq!(delta.submitted_presented_at.len(), 10);
        assert!(delta.confirmed_presented_at.is_empty());
    }

    #[test]
    fn rgb10a2unorm_is_90_not_the_552_the_task_brief_cited() {
        // Verified directly against `objc2-metal-0.3.2/src/generated/MTLPixelFormat.rs`:
        // `MTLPixelFormatRGB10A2Unorm` is `90`. `552` (this task's own brief
        // cited that value) is actually `MTLPixelFormatBGRA10_XR`, an
        // unrelated, EDR-oriented extended-range format. This test pins the
        // *correct* value so this specific transcription error cannot
        // silently recur.
        assert_eq!(MTLPixelFormat::RGB10A2Unorm.0, 90);
        assert_ne!(MTLPixelFormat::RGB10A2Unorm.0, 552);
        assert_eq!(
            MTLPixelFormat::BGRA10_XR.0,
            552,
            "552 is BGRA10_XR, not RGB10A2Unorm"
        );
    }

    #[test]
    fn normalized_hdr10_pixels_use_the_pq_reference_peak_as_optical_scale() {
        assert_eq!(HDR10_NORMALIZED_OPTICAL_OUTPUT_SCALE, 10_000.0);
    }

    // ---- plane_pixel_formats: pixel-format constant selection ------------

    #[test]
    fn eight_bit_planes_use_native_8bit_unorm_formats_with_unit_scale_255() {
        let plan = plane_pixel_formats(arcen_media::BitDepth::Eight);
        assert_eq!(plan.luma_format, MTLPixelFormat::R8Unorm);
        assert_eq!(plan.chroma_format, MTLPixelFormat::RG8Unorm);
        assert_eq!(plan.code_unnormalize_scale, 255.0);
    }

    #[test]
    fn ten_bit_planes_use_16bit_unorm_formats_with_the_msb_alignment_scale() {
        let plan = plane_pixel_formats(arcen_media::BitDepth::Ten);
        assert_eq!(plan.luma_format, MTLPixelFormat::R16Unorm);
        assert_eq!(plan.chroma_format, MTLPixelFormat::RG16Unorm);
        assert!((plan.code_unnormalize_scale - (65_535.0_f32 / 64.0)).abs() < 1e-6);
    }

    #[test]
    fn twelve_bit_planes_use_16bit_unorm_formats_with_the_msb_alignment_scale() {
        let plan = plane_pixel_formats(arcen_media::BitDepth::Twelve);
        assert_eq!(plan.luma_format, MTLPixelFormat::R16Unorm);
        assert_eq!(plan.chroma_format, MTLPixelFormat::RG16Unorm);
        assert!((plan.code_unnormalize_scale - (65_535.0_f32 / 16.0)).abs() < 1e-6);
    }

    #[test]
    fn code_unnormalize_scale_round_trips_every_representative_code_within_half_a_code() {
        for (depth, storage_shift) in [
            (arcen_media::BitDepth::Ten, 6u32),
            (arcen_media::BitDepth::Twelve, 4u32),
        ] {
            let plan = plane_pixel_formats(depth);
            let max_code = (1u32 << depth.bits()) - 1;
            for code in [0u32, 1, max_code / 2, max_code - 1, max_code] {
                let raw16 = code << storage_shift;
                let normalized = f32::from(u16::try_from(raw16).unwrap()) / 65535.0;
                let reconstructed = normalized * plan.code_unnormalize_scale;
                assert!(
                    (reconstructed - code as f32).abs() < 0.5,
                    "depth={depth:?} code={code} reconstructed={reconstructed}"
                );
            }
        }
    }

    #[test]
    fn eight_bit_scale_round_trips_exactly_with_no_msb_shift() {
        let plan = plane_pixel_formats(arcen_media::BitDepth::Eight);
        for code in [0u32, 1, 127, 254, 255] {
            let normalized = code as f32 / 255.0;
            let reconstructed = normalized * plan.code_unnormalize_scale;
            assert!((reconstructed - code as f32).abs() < 1e-3, "code={code}");
        }
    }

    // ---- MetalVideoUniform: matrix/range uniform construction ------------

    fn sample_contract() -> VideoColorContract {
        VideoColorContract {
            chroma: arcen_media::ChromaSubsampling::Yuv444,
            range: arcen_media::ColorRange::Full,
            depth: arcen_media::BitDepth::Ten,
            matrix: arcen_media::ColorMatrix::Bt709,
            primaries: arcen_media::ColorPrimaries::Bt709,
            transfer: arcen_media::TransferCharacteristics::Bt709,
        }
    }

    #[test]
    fn metal_uniform_shares_the_wgsl_uniforms_bytes_for_every_field_they_have_in_common() {
        let contract = sample_contract();
        let shared = VideoUniform::from_contract(contract, (1920, 1080), (1920, 1080));
        let metal = MetalVideoUniform::from_contract(contract, (1920, 1080), (1920, 1080), 42.5);

        let shared_bytes = shared.to_bytes();
        let metal_bytes = metal.to_bytes();
        assert_eq!(metal_bytes.len(), 52);
        assert_eq!(
            &metal_bytes[..48],
            &shared_bytes[..],
            "the first 48 bytes (every field WGSL also has) must be byte-identical \
             between the two uniforms -- see the module doc for why this is load-bearing"
        );
        let appended = f32::from_le_bytes(metal_bytes[48..52].try_into().expect("4 bytes"));
        assert_eq!(appended, 42.5);
    }

    #[test]
    fn metal_uniform_carries_a_different_code_unnormalize_scale_per_depth() {
        let ten_bit_plan = plane_pixel_formats(arcen_media::BitDepth::Ten);
        let eight_bit_plan = plane_pixel_formats(arcen_media::BitDepth::Eight);
        assert_ne!(
            ten_bit_plan.code_unnormalize_scale,
            eight_bit_plan.code_unnormalize_scale
        );
    }

    // ---- DedicatedLayerFallback: the fallback decision --------------------

    #[test]
    fn fallback_logs_the_first_attempt_regardless_of_outcome() {
        let mut fallback = DedicatedLayerFallback::default();
        assert_eq!(
            fallback.record(DedicatedLayerOutcome::NoRootWindow),
            Some(DedicatedLayerOutcome::NoRootWindow)
        );
    }

    #[test]
    fn fallback_does_not_repeat_an_identical_outcome_every_frame() {
        let mut fallback = DedicatedLayerFallback::default();
        fallback.record(DedicatedLayerOutcome::NoRootWindow);
        assert_eq!(fallback.record(DedicatedLayerOutcome::NoRootWindow), None);
    }

    #[test]
    fn fallback_logs_a_change_from_one_failure_reason_to_another() {
        let mut fallback = DedicatedLayerFallback::default();
        fallback.record(DedicatedLayerOutcome::NoRootWindow);
        assert_eq!(
            fallback.record(DedicatedLayerOutcome::NoContentView),
            Some(DedicatedLayerOutcome::NoContentView)
        );
    }

    #[test]
    fn fallback_logs_eventual_success_after_earlier_failures() {
        let mut fallback = DedicatedLayerFallback::default();
        fallback.record(DedicatedLayerOutcome::NoRootWindow);
        assert_eq!(
            fallback.record(DedicatedLayerOutcome::Ready),
            Some(DedicatedLayerOutcome::Ready)
        );
    }

    #[test]
    fn fallback_logs_a_regression_from_success_back_to_a_failure() {
        let mut fallback = DedicatedLayerFallback::default();
        fallback.record(DedicatedLayerOutcome::Ready);
        assert_eq!(
            fallback.record(DedicatedLayerOutcome::NoDrawableAvailable),
            Some(DedicatedLayerOutcome::NoDrawableAvailable)
        );
    }

    #[test]
    fn fallback_does_not_repeat_identical_success_every_frame() {
        let mut fallback = DedicatedLayerFallback::default();
        fallback.record(DedicatedLayerOutcome::Ready);
        assert_eq!(fallback.record(DedicatedLayerOutcome::Ready), None);
    }

    #[test]
    fn eight_bit_status_separates_layer_success_from_egui_fallback() {
        assert!(
            DedicatedEightBitPresentationStatus::from_outcome(DedicatedLayerOutcome::Ready)
                .is_dedicated_eight_bit()
        );
        let fallback = DedicatedEightBitPresentationStatus::from_outcome(
            DedicatedLayerOutcome::PlaneTextureCreationFailed,
        );
        assert!(fallback.is_fallback());
        assert_eq!(
            fallback.fallback_reason(),
            Some(DedicatedLayerOutcome::PlaneTextureCreationFailed)
        );
    }

    #[test]
    fn eight_bit_missing_drawable_is_a_dropped_present_not_fallback() {
        let dropped = DedicatedEightBitPresentationStatus::from_outcome(
            DedicatedLayerOutcome::NoDrawableAvailable,
        );
        assert_eq!(dropped, DedicatedEightBitPresentationStatus::DroppedFrame);
        assert!(!dropped.is_fallback());
        assert_eq!(dropped.fallback_reason(), None);
    }

    #[test]
    fn eight_bit_occlusion_skip_is_not_a_fallback_warning() {
        let skipped = DedicatedEightBitPresentationStatus::SkippedOccluded;
        assert!(!skipped.is_dedicated_eight_bit());
        assert!(!skipped.is_fallback());
        assert_eq!(skipped.fallback_reason(), None);
    }

    #[test]
    fn presenter_teardown_stops_display_link_without_waiting_on_drawable() {
        let mut presenter = DedicatedEightBitPresenterThread::default();
        let started = std::time::Instant::now();
        presenter.teardown();
        assert!(started.elapsed() < std::time::Duration::from_millis(50));
    }

    #[test]
    fn replacement_install_detaches_any_retained_main_thread_layer_handle() {
        assert!(retained_handle_detach_before_install(true));
        assert!(!retained_handle_detach_before_install(false));
    }

    #[test]
    fn layer_in_flight_gate_enforces_a_small_submit_limit() {
        let mut gate = LayerInFlightGate::new(2);
        assert!(gate.submitted());
        assert!(gate.submitted());
        assert!(!gate.submitted());
        gate.completed();
        assert!(gate.submitted());
    }

    #[test]
    fn presentation_status_keeps_10_bit_success_distinct_from_8_bit_fallback() {
        assert_eq!(
            DedicatedPresentationStatus::from_outcome(DedicatedLayerOutcome::Ready),
            DedicatedPresentationStatus::DedicatedTenBit
        );
        let fallback = DedicatedPresentationStatus::from_outcome(
            DedicatedLayerOutcome::PlaneTextureCreationFailed,
        );
        assert!(fallback.is_eight_bit_fallback());
        assert!(!fallback.is_dedicated_ten_bit());
        assert_eq!(
            fallback.fallback_reason(),
            Some(DedicatedLayerOutcome::PlaneTextureCreationFailed)
        );
    }

    #[test]
    fn ten_bit_missing_drawable_is_a_dropped_present_not_fallback() {
        let dropped =
            DedicatedPresentationStatus::from_outcome(DedicatedLayerOutcome::NoDrawableAvailable);
        assert_eq!(dropped, DedicatedPresentationStatus::DroppedFrame);
        assert!(!dropped.is_eight_bit_fallback());
        assert_eq!(dropped.fallback_reason(), None);
    }

    #[test]
    fn ten_bit_occlusion_skip_is_not_a_fallback_warning() {
        let skipped = DedicatedPresentationStatus::SkippedOccluded;
        assert!(!skipped.is_dedicated_ten_bit());
        assert!(!skipped.is_eight_bit_fallback());
        assert_eq!(skipped.fallback_reason(), None);
    }

    // ---- DedicatedLayerFrame: seam-type plumbing (no AppKit/Metal) -------

    #[test]
    fn dedicated_layer_frame_is_send_and_sync() {
        // `CVPixelBuffer` is documented+asserted `Send + Sync` by
        // `apple-cf` itself, and `VideoColorContract` is plain `Copy` data,
        // so this struct should be too with no manual unsafe impl needed --
        // pin that as a compile-time fact, since a future caller
        // (`video_decoder.rs`'s eventual wiring) will likely construct this
        // on a decode thread and hand it to the main thread for rendering.
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<DedicatedLayerFrame>();
    }
}
