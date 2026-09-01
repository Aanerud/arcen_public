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
// **Not compiled.** This file is edited from Windows, where no Metal
// compiler is available; `video_metal_layer.rs`'s module doc records
// exactly what could and could not be verified about the Rust-side calls
// that compile and run this source at runtime via
// `MTLDevice.newLibraryWithSource:options:error:`. The MSL syntax here
// (attribute qualifiers, `texture2d<T, access>`, `[[stage_in]]`,
// `[[vertex_id]]`, `[[position]]`, `[[texture(n)]]`, `[[buffer(n)]]`) was
// written to match the Metal Shading Language Specification's documented
// grammar as precisely as possible without a compiler to check it against,
// but is explicitly **not** verified to compile.

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

// The single conversion pass -- see the module doc above for why this
// shader needs only one, unlike `video_render.wgsl`'s "convert" +
// "present" pair. `access::read` (not `access::sample`): every plane read
// here is an exact, nearest-neighbour texel fetch at an integer
// coordinate, never a filtered/interpolated sample -- matching
// `video_render.wgsl`'s own `textureLoad` (never `textureSample`) for its
// plane inputs, and for the identical reason documented there: blending
// two different YCbCr codes before the matrix is applied is not the same
// operation as blending after, so no filtering happens before conversion.
fragment float4 fs_convert(
    VsOut in [[stage_in]],
    texture2d<float, access::read> plane0 [[texture(0)]],
    texture2d<float, access::read> plane1 [[texture(1)]],
    constant VideoUniform& u [[buffer(0)]]
) {
    uint2 luma_coord = uint2(in.uv * float2(u.luma_width, u.luma_height));
    uint2 chroma_coord = uint2(in.uv * float2(u.chroma_width, u.chroma_height));

    float luma_raw = plane0.read(luma_coord).r;
    float2 chroma_raw = plane1.read(chroma_coord).rg;

    // Reconstruct the original ITU-R code from the Unorm-normalised read --
    // see the module doc's "Unorm reconstruction" section. This is the
    // *only* step with no WGSL equivalent; everything from here down
    // mirrors `video_render.wgsl`'s `fs_main` field for field, formula for
    // formula.
    float luma_code = luma_raw * u.code_unnormalize_scale;
    float cb_code = chroma_raw.r * u.code_unnormalize_scale;
    float cr_code = chroma_raw.g * u.code_unnormalize_scale;

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
