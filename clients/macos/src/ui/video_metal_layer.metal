// Arcen Deck -- dedicated 10-bit `CAMetalLayer` video shader (MSL).
//
// This is the Metal Shading Language twin of `video_render.wgsl`'s
// `fs_main` (see that file's own module doc for the full pass structure of
// the *other*, egui/wgpu-shared path). Both shaders implement the exact
// same YCbCr/GBR -> RGB conversion -- BT.709/601/2020 NCL matrix, and the
// ITU-T H.273 identity/GBR passthrough -- from the same
// `arcen_media::video::convert::ColorTransform::to_bgr8` reference this
// crate must never numerically disagree with (see `video_metal_layer.rs`'s
// module doc). If you change the maths in one of these two files, change
// it in the other, in the same way, in the same commit -- a silent
// divergence between them would be a silent colour bug that neither
// compiler nor test suite can catch by itself (there is no shared source
// of truth between a `.wgsl` and a `.metal` file; only human discipline
// keeps them in step, which is exactly why this comment exists).
//
// # Why this shader is single-pass, unlike `video_render.wgsl`'s two passes
//
// `video_render.wgsl` converts into an intermediate `Rgb10a2Unorm` target at
// the source's native resolution, then a *second* pass bilinear-resamples
// that target into whatever render pass `egui_wgpu::Renderer` is currently
// recording -- because that second render pass's size is dictated by
// `egui`'s own shared swapchain and viewport, not by this module.
//
// This shader has no such constraint: it is the *only* thing that ever
// renders into this `CAMetalLayer`'s drawable (see `video_metal_layer.rs`),
// so `video_metal_layer.rs` sizes the drawable itself
// (`CAMetalLayer.drawableSize`) to the source's native pixel dimensions,
// and leaves the final on-screen scale-to-fit to Core Animation's own GPU
// compositor (`CALayer.frame` vs `drawableSize` -- the same mechanism any
// other image-backed `CALayer` uses to fill its frame). That makes a single
// conversion pass sufficient and correct: every pixel this shader writes is
// a 1:1 texel of the source, with no resampling of *already-converted* RGB
// needed here at all.
//
// # Reading `CVMetalTextureCache`-vended planes: the Unorm reconstruction
//
// `video_render.wgsl` reads its plane textures with `textureLoad` on
// `texture_2d<u32>` views -- a raw integer code, 0..=1023 at ten bits,
// because `video_render.rs` uploads CPU-side `u16` plane bytes directly
// into `wgpu::TextureFormat::R16Uint`/`Rg16Uint` textures it creates
// itself.
//
// This shader's planes instead come from `CVMetalTextureCacheCreateTextureFromImage`
// against the *real* decoded `CVPixelBuffer` (see `video_metal_layer.rs`'s
// `create_plane_texture`), requested as `MTLPixelFormatR16Unorm`/
// `MTLPixelFormatRG16Unorm` (ten/twelve bits) or `MTLPixelFormatR8Unorm`/
// `MTLPixelFormatRG8Unorm` (eight bits) -- the only Metal pixel formats
// CoreVideo's biplanar IOSurfaces are actually compatible with; there is no
// way to request an *integer* view of that same IOSurface plane the way
// `wgpu` requests one of its own textures. A `read()` from an `Unorm`
// texture therefore returns a *normalised* float, not the raw code, and
// undoing that normalisation is not simply "multiply by the max code":
//
//   - **Eight-bit** planes are native 8-bit-per-component IOSurfaces, so
//     `read()` yields exactly `code / 255.0`.
//   - **Ten/twelve-bit** planes are MSB-aligned inside a 16-bit container,
//     while Metal normalises `R16Unorm`/`RG16Unorm` reads by 65535. The CPU
//     therefore supplies the depth-specific `code_unnormalize_scale` needed
//     to recover the original code value. See `plane_pixel_formats` in
//     `video_metal_layer.rs`.
//
// Once `luma_code`/`cb_code`/`cr_code` below are reconstructed this way,
// every remaining line is the *identical* formula `video_render.wgsl`'s
// `fs_main` applies to its own raw integer codes -- same offsets, same
// spans, same matrix coefficients, same identity/GBR branch. Only the
// route to a comparable "code" float differs between the two files; the
// colour maths itself does not.
//
// # Compile status
//
// Compiled and executed by `video_metal_layer::shader_tests` on every Mac
// test run: those tests build the production pipeline state from this
// source, render 8-bit and 10-bit 4:2:0, 4:2:2 and 4:4:4 planes through
// `fs_convert` on the real Metal device, and compare the read-back pixels
// with the CPU reference in `video_render.rs`.

#include <metal_stdlib>
using namespace metal;

// Keep numerically in sync with `MetalVideoUniform` in `video_metal_layer.rs`
// (which builds this buffer's bytes by hand -- see that struct's own doc)
// and, for every field except `code_unnormalize_scale`, with `VideoUniform`
// in `video_render.wgsl`/`video_render.rs`.
struct VideoUniform {
    float luma_offset;
    float luma_span_inv;
    float chroma_center;
    float chroma_span_inv;
    float kr;
    float kb;
    float kg_inv;
    uint mode;
    float luma_width;
    float luma_height;
    float chroma_width;
    float chroma_height;
    // MSL-only: see the "Unorm reconstruction" section above. Absent from
    // `video_render.wgsl`'s uniform because that shader never needs it.
    float code_unnormalize_scale;
};

// Keep in sync with `ShaderMode` in `video_render.rs` (reused directly by
// `MetalVideoUniform::from_contract` -- see that function's own doc) and
// the `MODE_*` constants in `video_render.wgsl`. `MODE_PACKED_RGBA` (2) is
// deliberately absent here: this shader's only source is a real decoded
// `CVPixelBuffer` (see `video_metal_layer.rs`'s `DedicatedLayerFrame`),
// which is never the already-converted-RGB payload that mode exists for.
constant uint MODE_MATRIX = 0;
constant uint MODE_IDENTITY_GBR = 1;

struct VsOut {
    float4 position [[position]];
    float2 uv;
};

// Big-triangle trick: three vertices, no vertex buffer, covering the whole
// clip-space rectangle -- identical in structure to `vs_main` in
// `video_render.wgsl` (see that file's own doc for why this covers exactly
// the render pass's viewport with no further transform needed). Metal's
// clip space is `x`/`y`-up in `[-1, 1]` after the perspective divide, the
// same convention WGSL/wgpu already targets on this same Metal backend, so
// the formula below is copied verbatim rather than re-derived.
vertex VsOut vs_main(uint vertex_id [[vertex_id]]) {
    float x = float((vertex_id << 1) & 2);
    float y = float(vertex_id & 2);
    VsOut out;
    out.position = float4(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
    out.uv = float2(x, y);
    return out;
}

// Chroma reconstruction for a subsampled plane (4:2:0 and 4:2:2).
//
// Every Arcen host builds subsampled chroma with a box filter
// (`arcen_media::video::convert`): one Cb/Cr pair per 2x2 (4:2:0) or 2x1
// (4:2:2) luma block, sited at the block's centre. The old nearest fetch
// repeated that pair over the whole block, so a block straddling a glyph
// edge painted the stroke's colour onto the background (and the
// background's onto the stroke) in hard 2-pixel steps.
//
// This is a luma-guided ("chroma from luma") reconstruction. Around the
// pixel it takes the four nearest chroma samples with their bilinear
// weights and each sample's own block-mean luma, fits chroma as a linear
// function of luma over those four points, and evaluates the fit at the
// pixel's own full-resolution luma, clamped to the four samples' range.
// Where two colours meet -- coloured text on a flat background, a button
// label -- chroma really is a function of luma, so the edge is rebuilt
// sharply at the luma edge. Where the fit explains little (r^2 low:
// sub-pixel-rendered text, iso-luminant colour edges, flat areas), the
// result falls back towards the block's own sample, which preserves every
// block's transmitted average exactly as the nearest fetch did.
//
// The matrix below is affine in the code values, so reconstructing codes
// before converting is sound; the final `saturate` still happens last.
// An axis at full resolution keeps the exact fetch, and a 4:4:4 picture
// takes the early return -- bit-identical to before.
//
// `video_render.wgsl`'s `chroma_codes` is the WGSL twin of this function and
// `video_render::VideoUniform::chroma_reference_codes` the CPU reference
// both are tested against; change all three together.
constant float CHROMA_FIT_EPSILON = 3.0e-4; // ~16 code^2 at 8-bit limited range

static float2 chroma_codes(
    texture2d<float, access::sample> plane0,
    texture2d<float, access::read> plane1,
    float2 uv,
    float pixel_luma,
    constant VideoUniform& u
) {
    float2 luma_size = float2(u.luma_width, u.luma_height);
    float2 chroma_size = float2(u.chroma_width, u.chroma_height);
    uint2 exact = uint2(uv * chroma_size);
    bool2 full = chroma_size >= luma_size;
    float2 own = plane1.read(exact).rg;
    if (all(full)) {
        return own;
    }
    float2 position = uv * chroma_size - 0.5;
    float2 base = floor(position);
    float2 weight = position - base;
    if (full.x) {
        base.x = float(exact.x);
        weight.x = 0.0;
    }
    if (full.y) {
        base.y = float(exact.y);
        weight.y = 0.0;
    }
    int2 last = int2(chroma_size) - 1;
    int2 c0 = clamp(int2(base), int2(0), last);
    int2 c1 = clamp(int2(base) + 1, int2(0), last);
    // A full-resolution axis has exactly one tap: the neighbour must not
    // enter the clamp range (4:2:2 rows are independent).
    if (full.x) {
        c1.x = c0.x;
    }
    if (full.y) {
        c1.y = c0.y;
    }
    int2 taps[4] = { int2(c0.x, c0.y), int2(c1.x, c0.y), int2(c0.x, c1.y), int2(c1.x, c1.y) };
    float tap_weight[4] = {
        (1.0 - weight.x) * (1.0 - weight.y),
        weight.x * (1.0 - weight.y),
        (1.0 - weight.x) * weight.y,
        weight.x * weight.y,
    };

    float2 tap_chroma[4];
    float2 low = float2(INFINITY);
    float2 high = float2(-INFINITY);
    for (int i = 0; i < 4; i++) {
        tap_chroma[i] = plane1.read(uint2(taps[i])).rg;
        low = min(low, tap_chroma[i]);
        high = max(high, tap_chroma[i]);
    }
    // Most of a desktop is flat colour: when the four samples agree there is
    // nothing to reconstruct, and the luma reads below can be skipped.
    if (all(high == low)) {
        return own;
    }

    // A linear sample at a chroma texel's centre, in luma coordinates, is the
    // mean of exactly the luma block that texel was averaged from.
    constexpr sampler block_mean(coord::normalized, address::clamp_to_edge, filter::linear);
    float tap_luma[4];
    float mean_luma = 0.0;
    float2 mean_chroma = float2(0.0);
    for (int i = 0; i < 4; i++) {
        float2 centre = (float2(taps[i]) + 0.5) / chroma_size;
        tap_luma[i] = (plane0.sample(block_mean, centre).r * u.code_unnormalize_scale - u.luma_offset)
            * u.luma_span_inv;
        mean_luma += tap_weight[i] * tap_luma[i];
        mean_chroma += tap_weight[i] * tap_chroma[i];
    }
    float luma_variance = 0.0;
    float2 chroma_variance = float2(0.0);
    float2 covariance = float2(0.0);
    for (int i = 0; i < 4; i++) {
        float dy = tap_luma[i] - mean_luma;
        float2 dc = tap_chroma[i] - mean_chroma;
        luma_variance += tap_weight[i] * dy * dy;
        chroma_variance += tap_weight[i] * dc * dc;
        covariance += tap_weight[i] * dy * dc;
    }
    float luma_here = (pixel_luma - u.luma_offset) * u.luma_span_inv;
    float2 slope = covariance / (luma_variance + CHROMA_FIT_EPSILON);
    float2 fitted = clamp(mean_chroma + slope * (luma_here - mean_luma), low, high);
    float2 denominator = luma_variance * chroma_variance;
    float2 confidence = select(
        float2(0.0),
        saturate(covariance * covariance / max(denominator, 1.0e-12)),
        denominator > 0.0
    );
    return mix(own, fitted, confidence);
}

// The single conversion pass -- see the module doc above for why this
// shader needs only one, unlike `video_render.wgsl`'s "convert" +
// "present" pair. Luma is an exact texel fetch at an integer coordinate;
// `plane0` is bound with sample access only so `chroma_codes` can take a
// block mean in one filtered sample. Chroma is reconstructed explicitly by
// `chroma_codes`, identically to the WGSL twin.
fragment float4 fs_convert(
    VsOut in [[stage_in]],
    texture2d<float, access::sample> plane0 [[texture(0)]],
    texture2d<float, access::read> plane1 [[texture(1)]],
    constant VideoUniform& u [[buffer(0)]]
) {
    uint2 luma_coord = uint2(in.uv * float2(u.luma_width, u.luma_height));

    float luma_raw = plane0.read(luma_coord).r;

    // Reconstruct the original ITU-R code from the Unorm-normalised read --
    // see the module doc's "Unorm reconstruction" section. This is the
    // *only* step with no WGSL equivalent; everything from here down
    // mirrors `video_render.wgsl`'s `fs_main` field for field, formula for
    // formula.
    float luma_code = luma_raw * u.code_unnormalize_scale;
    float2 chroma_code = chroma_codes(plane0, plane1, in.uv, luma_code, u) * u.code_unnormalize_scale;
    float cb_code = chroma_code.x;
    float cr_code = chroma_code.y;

    if (u.mode == MODE_IDENTITY_GBR) {
        // Identity/GBR passthrough (ITU-T H.273): plane0 -> G, the chroma
        // plane's Cb slot -> B, its Cr slot -> R, all three scaled by the
        // *luma* range exactly like `ColorTransform::to_bgr8`'s own
        // identity branch -- see `video_render.wgsl`'s identical comment.
        float g = (luma_code - u.luma_offset) * u.luma_span_inv;
        float b = (cb_code - u.luma_offset) * u.luma_span_inv;
        float r = (cr_code - u.luma_offset) * u.luma_span_inv;
        return float4(saturate(r), saturate(g), saturate(b), 1.0);
    }

    // Matrix branch (BT.709 / BT.601 / BT.2020 NCL), mirroring
    // `ColorTransform::to_bgr8`'s non-identity path exactly:
    //   luma_norm = (Y - luma_offset) / luma_span
    //   blue_diff = (Cb - chroma_center) / chroma_span
    //   red_diff  = (Cr - chroma_center) / chroma_span
    //   R = luma_norm + 2*(1-Kr)*red_diff
    //   B = luma_norm + 2*(1-Kb)*blue_diff
    //   G = (luma_norm - Kr*R - Kb*B) / Kg
    float luma_norm = (luma_code - u.luma_offset) * u.luma_span_inv;
    float blue_diff = (cb_code - u.chroma_center) * u.chroma_span_inv;
    float red_diff = (cr_code - u.chroma_center) * u.chroma_span_inv;
    float red = luma_norm + 2.0 * (1.0 - u.kr) * red_diff;
    float blue = luma_norm + 2.0 * (1.0 - u.kb) * blue_diff;
    float green = (luma_norm - u.kr * red - u.kb * blue) * u.kg_inv;
    return float4(saturate(red), saturate(green), saturate(blue), 1.0);
}
