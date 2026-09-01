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
//! [`DedicatedVideoPresenter`] are fully implemented, but **no production
//! code path constructs a [`DedicatedLayerFrame`] today**, because nothing
//! in this change can: `video_decoder.rs` (this task's constraints forbid
//! editing `src/pipeline/`) never hands back a `CVPixelBuffer` to its
//! caller, only an already-CPU-converted
//! [`super::video_render::DecodedVideoFrame::rgba`]. This is the *same*
//! seam `video_render.rs` already documents needing, and wiring either one
//! in wires in both: whatever future change teaches `video_decoder.rs` to
//! hand back a real `CVPixelBuffer` plus its negotiated
//! [`super::video_render::VideoColorContract`] unlocks both
//! [`super::video_render::RemoteVideoFrame::from_planar16`] (the CPU
//! fallback) and [`DedicatedLayerFrame`] (this module's zero-copy path,
//! the better of the two once available) at the same time.
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
//! mechanics. [`DedicatedVideoLayer::resize`] repositions/resizes the
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
//! `NSView::layer()`/`setLayer()` (used by [`DedicatedVideoLayer::attach`])
//! from absent into a typed, safe accessor -- see
//! `video_render.rs`'s own "Unblocked 2026-08-14" section for the identical
//! feature-gating fact already discovered for a different accessor.
//!
//! # Fail safe and loud
//!
//! [`DedicatedVideoPresenter::try_paint`] is the single entry point a
//! future caller wires in: it returns `true` exactly when the dedicated
//! layer handled this frame (the caller must then skip
//! [`super::video_render::RemoteVideoFrame::paint`] for it, so video is
//! never drawn twice), `false` when every attempt failed and the caller
//! must invoke that existing path instead. Every distinct failure reason
//! ([`DedicatedLayerOutcome`]) is logged at `warn`, naming what failed,
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
//! 5. Confirm `CALayer.contentsGravity`'s default (`kCAGravityResize`, a
//!    non-uniform stretch-to-fill) is acceptable given this layer's
//!    `drawableSize` is the source's native resolution and its `frame` is
//!    whatever aspect-correct rect the caller already computed (via
//!    `display_fit.rs`) -- this module deliberately leaves
//!    `contentsGravity` untouched rather than guessing whether an explicit
//!    `kCAGravityResizeAspect` is also warranted as a defensive measure.

use core::ffi::c_void;
use core::ptr::NonNull;

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_core_graphics::{
    kCGColorSpaceDisplayP3, kCGColorSpaceITUR_2100_PQ, kCGColorSpaceSRGB, CGColorSpace,
};
use objc2_foundation::{MainThreadMarker, NSString};
use objc2_metal::{
    MTLClearColor, MTLCommandBuffer, MTLCommandEncoder, MTLCommandQueue,
    MTLCreateSystemDefaultDevice, MTLDevice, MTLLibrary, MTLLoadAction, MTLPixelFormat,
    MTLPrimitiveType, MTLRenderCommandEncoder, MTLRenderPassDescriptor,
    MTLRenderPipelineDescriptor, MTLRenderPipelineState, MTLResourceOptions, MTLStoreAction,
    MTLTexture,
};
use objc2_quartz_core::{CAEDRMetadata, CAMetalDrawable, CAMetalLayer};

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
    FallbackToEightBit(DedicatedLayerOutcome),
}

impl DedicatedPresentationStatus {
    const fn from_outcome(outcome: DedicatedLayerOutcome) -> Self {
        match outcome {
            DedicatedLayerOutcome::Ready => Self::DedicatedTenBit,
            other => Self::FallbackToEightBit(other),
        }
    }

    pub(crate) const fn is_dedicated_ten_bit(self) -> bool {
        matches!(self, Self::DedicatedTenBit)
    }

    pub(crate) const fn is_eight_bit_fallback(self) -> bool {
        matches!(self, Self::FallbackToEightBit(_))
    }

    pub(crate) const fn fallback_reason(self) -> Option<DedicatedLayerOutcome> {
        match self {
            Self::FallbackToEightBit(reason) => Some(reason),
            Self::Inactive | Self::DedicatedTenBit => None,
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
    /// One-shot guard for [`Self::log_plane_statistics`].
    logged_plane_statistics: bool,
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
        let pipeline_state = Self::build_pipeline_state(&device)?;

        let layer = CAMetalLayer::new();
        layer.setDevice(Some(&*device));
        layer.setPixelFormat(MTLPixelFormat::RGB10A2Unorm);
        if layer.pixelFormat() != MTLPixelFormat::RGB10A2Unorm {
            return Err(DedicatedLayerOutcome::PixelFormatMismatch);
        }
        layer.setFramebufferOnly(true);
        layer.setWantsExtendedDynamicRangeContent(false);
        layer.setOpaque(true);
        layer.setName(Some(&NSString::from_str(DEDICATED_VIDEO_LAYER_NAME)));
        layer.setFrame(rect);
        root_layer.insertSublayer_atIndex(&layer, 0);

        Ok(Self {
            layer,
            device,
            command_queue,
            pipeline_state,
            texture_cache,
            last_colorspace: None,
            logged_plane_statistics: false,
        })
    }

    /// Compiles `video_metal_layer.metal` from source and builds the
    /// render-pipeline state targeting `Rgb10a2Unorm`. Runtime source
    /// compilation (`-[MTLDevice newLibraryWithSource:options:error:]`),
    /// not a precompiled `.metallib`, because this task's constraints do
    /// not permit editing `build.rs` (outside `src/ui/`/`Cargo.toml`) to add
    /// an `xcrun metal`/`metallib` step.
    fn build_pipeline_state(
        device: &ProtocolObject<dyn MTLDevice>,
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
        // SAFETY: index 0 is always a valid colour-attachment slot; every
        // Metal device supports at least one.
        let color_attachment = unsafe { descriptor.colorAttachments().objectAtIndexedSubscript(0) };
        color_attachment.setPixelFormat(MTLPixelFormat::RGB10A2Unorm);

        device
            .newRenderPipelineStateWithDescriptor_error(&descriptor)
            .map_err(|_| DedicatedLayerOutcome::PipelineStateCreationFailed)
    }

    /// Repositions/resizes the layer's on-screen `frame`. Never fails: a
    /// `CALayer` property setter cannot itself be rejected.
    pub fn resize(&self, rect: CGRect) {
        self.layer.setFrame(rect);
    }

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
        self.layer.setColorspace(Some(&colorspace));

        // Order matters on the way *in* as well as the way out: EDR is only
        // meaningful once the layer is already tagged with an HDR transfer,
        // and must be withdrawn before the layer is retagged back to SDR,
        // or there is a window of frames claiming extended range against an
        // sRGB curve.
        match desired {
            PresentationColorSpace::Hdr10Pq => {
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
                        HDR10_NORMALIZED_OPTICAL_OUTPUT_SCALE,
                    );
                self.layer.setEDRMetadata(Some(&metadata));
            }
            PresentationColorSpace::Sdr(_) => {
                self.layer.setEDRMetadata(None);
                self.layer.setWantsExtendedDynamicRangeContent(false);
            }
        }

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
                format.0 as usize,
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
    /// Report what the decoded planes actually contain, once per layer.
    ///
    /// Logs the raw visible sample range once so a new decoder/format can be
    /// checked against [`plane_pixel_formats`]. A ten-bit neutral chroma
    /// sample is expected near `512 << 6 = 32768`.
    fn log_plane_statistics(&mut self, frame: &DedicatedLayerFrame) {
        if self.logged_plane_statistics {
            return;
        }
        self.logged_plane_statistics = true;
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

    pub fn render(&mut self, frame: &DedicatedLayerFrame) -> Result<(), DedicatedLayerOutcome> {
        self.log_plane_statistics(frame);
        if let Some(applied) =
            self.ensure_colorspace(frame.contract.primaries, frame.contract.transfer)
        {
            // The fifth and last link in the HDR chain: the Deck saying,
            // in its own log, what it switched its presentation surface to
            // on receiving this stream. Logged on change only -- this is a
            // per-frame path -- and it names the transfer it switched
            // *because of*, so a reader can line it up against the host's
            // own "streaming HDR" line and see the two agree.
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

        // Native source resolution, not the on-screen `frame` size -- see
        // the module doc's "Layer lifecycle" section for why Core
        // Animation's own compositor is left to do the final scale-to-fit.
        self.layer.setDrawableSize(CGSize {
            width: luma_width as f64,
            height: luma_height as f64,
        });

        let Some((_luma_cv, luma_ptr)) =
            self.create_plane_texture(pixel_buffer, plan.luma_format, luma_width, luma_height, 0)
        else {
            return Err(DedicatedLayerOutcome::PlaneTextureCreationFailed);
        };
        let Some((_chroma_cv, chroma_ptr)) = self.create_plane_texture(
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
        // SAFETY: `uniform_bytes` is a non-empty (52-byte), live local
        // `Vec<u8>` for the duration of this call;
        // `newBufferWithBytes:length:options:` copies its contents into a
        // new Metal-owned allocation rather than retaining this pointer
        // past the call.
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
        // SAFETY: `luma_ptr`/`chroma_ptr` are live, non-null `id<MTLTexture>`
        // pointers for exactly as long as `_luma_cv`/`_chroma_cv` (held in
        // this same stack frame, dropped only at the end of this function)
        // are retained -- see `create_plane_texture`'s own doc. Casting the
        // raw pointer to `*const ProtocolObject<dyn MTLTexture>` and
        // dereferencing it is the standard `objc2` technique for wrapping
        // an already-known-to-conform raw object reference.
        let luma_texture: &ProtocolObject<dyn MTLTexture> =
            unsafe { &*(luma_ptr as *const ProtocolObject<dyn MTLTexture>) };
        let chroma_texture: &ProtocolObject<dyn MTLTexture> =
            unsafe { &*(chroma_ptr as *const ProtocolObject<dyn MTLTexture>) };
        // SAFETY: every argument below is a live, correctly-typed
        // reference for the encoder's own lifetime (this same call), and
        // `drawPrimitives_vertexStart_vertexCount`'s `0..3` matches the
        // big-triangle trick `vs_main` (in `video_metal_layer.metal`)
        // expects -- exactly three vertices, no vertex buffer.
        unsafe {
            encoder.setFragmentTexture_atIndex(Some(luma_texture), 0);
            encoder.setFragmentTexture_atIndex(Some(chroma_texture), 1);
            encoder.setFragmentBuffer_offset_atIndex(Some(&*uniform_buffer), 0, 0);
            encoder.drawPrimitives_vertexStart_vertexCount(MTLPrimitiveType::Triangle, 0, 3);
        }
        encoder.endEncoding();
        // SAFETY: `drawable` (a `CAMetalDrawable`) is always also an
        // `MTLDrawable` (`CAMetalDrawable: MTLDrawable`, its own supertrait
        // bound) -- reinterpreting the reference as its supertrait
        // protocol is `ProtocolObject::from_ref`'s documented purpose.
        command_buffer.presentDrawable(ProtocolObject::from_ref(&*drawable));
        command_buffer.commit();

        Ok(())
    }
}

impl Drop for DedicatedVideoLayer {
    /// Tears the layer down: removes it from the view hierarchy. Every
    /// other resource here (`device`/`command_queue`/`pipeline_state`/
    /// `texture_cache`) is released by its own `Retained`/`CVMetalTextureCache`
    /// handling as this struct's fields drop in turn.
    fn drop(&mut self) {
        self.layer.removeFromSuperlayer();
    }
}

// ============================================================================
// The wiring point a future caller uses (see the module doc)
// ============================================================================

/// Owns the dedicated 10-bit video presentation path end to end: the
/// lazily created [`DedicatedVideoLayer`], plus the log-once fallback
/// bookkeeping ([`DedicatedLayerFallback`]). Mirrors
/// `video_render::VideoRendererResources`'s own "lives in a per-surface
/// resources bag, created and updated lazily" shape, but is not currently
/// placed anywhere a production code path reaches it -- see the module
/// doc's "What is real today vs. what is a seam" section.
#[derive(Default)]
pub struct DedicatedVideoPresenter {
    layer: Option<DedicatedVideoLayer>,
    fallback: DedicatedLayerFallback,
    last_rect: Option<CGRect>,
    presentation_status: DedicatedPresentationStatus,
}

impl DedicatedVideoPresenter {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Attempts to render `frame` through the dedicated 10-bit layer at
    /// `rect` (creating or resizing it as needed). Returns whether the
    /// dedicated path handled this frame or the caller must use the existing
    /// 8-bit `RemoteVideoFrame::paint` egui/wgpu path instead.
    /// See the module doc's "fail safe and loud" section: every distinct
    /// failure reason is logged at `warn` at most once
    /// ([`DedicatedLayerFallback`]), success is logged once at `info`, and
    /// this never panics.
    pub(crate) fn try_paint(
        &mut self,
        rect: CGRect,
        frame: &DedicatedLayerFrame,
    ) -> DedicatedPresentationStatus {
        let outcome = self.try_paint_inner(rect, frame);
        self.record_outcome(outcome)
    }

    fn record_outcome(&mut self, outcome: DedicatedLayerOutcome) -> DedicatedPresentationStatus {
        let status = DedicatedPresentationStatus::from_outcome(outcome);
        self.presentation_status = status;
        if let Some(logged) = self.fallback.record(outcome) {
            if logged == DedicatedLayerOutcome::Ready {
                tracing::info!(
                    target: crate::logging::target::VIDEO,
                    "established the dedicated 10-bit CAMetalLayer video path; \
                     layer and drawable are RGB10A2Unorm",
                );
            } else {
                tracing::warn!(
                    target: crate::logging::target::VIDEO,
                    ?logged,
                    "dedicated 10-bit video layer unavailable; falling back to the \
                     existing 8-bit wgpu/egui video path",
                );
            }
        }
        status
    }

    /// Converts egui's top-left-origin point coordinates into the root
    /// content view's bottom-left-origin Core Animation coordinates, then
    /// renders through [`Self::try_paint`].
    pub(crate) fn try_paint_egui(
        &mut self,
        rect: egui::Rect,
        frame: &DedicatedLayerFrame,
    ) -> DedicatedPresentationStatus {
        let Some(mtm) = MainThreadMarker::new() else {
            return self.record_outcome(DedicatedLayerOutcome::NotMainThread);
        };
        let Some(window) = super::video_render::find_root_window(mtm) else {
            return self.record_outcome(DedicatedLayerOutcome::NoRootWindow);
        };
        let Some(view) = window.contentView() else {
            return self.record_outcome(DedicatedLayerOutcome::NoContentView);
        };
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
        self.try_paint(content_rect, frame)
    }

    fn try_paint_inner(
        &mut self,
        rect: CGRect,
        frame: &DedicatedLayerFrame,
    ) -> DedicatedLayerOutcome {
        if let Some(layer) = self.layer.as_ref() {
            if self.last_rect != Some(rect) {
                layer.resize(rect);
            }
        } else {
            match DedicatedVideoLayer::attach(rect) {
                Ok(layer) => self.layer = Some(layer),
                Err(outcome) => return outcome,
            }
        }
        self.last_rect = Some(rect);

        // The `expect` below reflects this function's own control flow,
        // not an externally triggerable state: the branch immediately
        // above always leaves `self.layer` populated by this point --
        // either it already was, or `attach()` just populated it (any
        // failure there returns early, above).
        let render_result = self
            .layer
            .as_mut()
            .expect("self.layer is always Some here; attach() failure returns early above")
            .render(frame);

        match render_result {
            Ok(()) => DedicatedLayerOutcome::Ready,
            Err(outcome) => {
                // Tear down so the next call re-attaches cleanly rather
                // than retrying against a possibly wedged layer -- see the
                // module doc's "fail safe and loud" section.
                self.layer = None;
                self.last_rect = None;
                outcome
            }
        }
    }

    /// Tears the dedicated layer down (if any), returning this presenter to
    /// its initial state. Call when the owning window/view is torn down or
    /// video stops, so the layer does not linger in the view hierarchy.
    pub fn teardown(&mut self) {
        self.layer = None;
        self.last_rect = None;
        self.presentation_status = DedicatedPresentationStatus::Inactive;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- RGB10A2Unorm: the task-brief correction -------------------------

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
        assert!((plan.code_unnormalize_scale - 1023.984_375).abs() < 1e-6);
    }

    #[test]
    fn twelve_bit_planes_use_16bit_unorm_formats_with_the_msb_alignment_scale() {
        let plan = plane_pixel_formats(arcen_media::BitDepth::Twelve);
        assert_eq!(plan.luma_format, MTLPixelFormat::R16Unorm);
        assert_eq!(plan.chroma_format, MTLPixelFormat::RG16Unorm);
        assert!((plan.code_unnormalize_scale - 4095.937_5).abs() < 1e-6);
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
    fn presentation_status_keeps_10_bit_success_distinct_from_8_bit_fallback() {
        assert_eq!(
            DedicatedPresentationStatus::from_outcome(DedicatedLayerOutcome::Ready),
            DedicatedPresentationStatus::DedicatedTenBit
        );
        let fallback =
            DedicatedPresentationStatus::from_outcome(DedicatedLayerOutcome::NoDrawableAvailable);
        assert!(fallback.is_eight_bit_fallback());
        assert!(!fallback.is_dedicated_ten_bit());
        assert_eq!(
            fallback.fallback_reason(),
            Some(DedicatedLayerOutcome::NoDrawableAvailable)
        );
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
