// Arcen Deck -- primary remote-video surface shader.
//
// Replaces the CPU-side "decode -> BGRA -> RGBA `Vec<u8>` -> `egui::ColorImage`"
// path (see `video_render.rs` and the `video_decoder.rs` follow-up it
// documents) with a GPU conversion that can carry full negotiated bit depth
// end to end instead of flattening every frame to 8-bit RGBA before the
// matrix is even applied.
//
// Two passes, both driven from `video_render.rs`'s `VideoRendererResources`:
//
//  1. `vs_main`/`fs_main` ("convert"): plane0/plane1 (integer code planes)
//     -> a `Rgb10a2Unorm` "reference" render target, at the source's native
//     resolution. This is the SDR 10-bit reference-viewing target itself
//     (w4-10bit-drawable): `wgpu`'s Metal backend maps `Rgb10a2Unorm` to
//     `MTLPixelFormatRGB10A2Unorm` natively (verified against the vendored
//     `wgpu-hal` 29.0.4 source; see the module doc in `video_render.rs` for
//     exactly what is and is not reachable here). Runs once per genuinely
//     new decoded frame (see `VideoRendererResources::update`'s sequence
//     check), not once per UI repaint.
//  2. `vs_main`/`fs_composite` ("present"): samples that resolved 10-bit
//     target with a bilinear filter and writes into `egui`'s own shared
//     render pass -- whatever surface format that is (`Bgra8Unorm` on every
//     Metal surface `eframe`/`egui-wgpu` 0.35 configures; see the module doc
//     for why the *true* presentation surface's format is not reachable
//     from here). Runs every repaint, like the rest of `egui`'s own
//     painting.
//
// The two non-passthrough `fs_main` branches mirror, field for field and
// formula for formula, `arcen_media::video::convert::ColorTransform::to_bgr8`
// -- the canonical CPU reference this shader must never numerically
// disagree with. See `VideoUniform::from_contract` in `video_render.rs` for
// exactly how the uniform fields below are derived from a negotiated
// chroma/range/depth/matrix contract.
//
// `fs_main` reads plane0/plane1 with `textureLoad`: they are integer code
// planes (`texture_2d<u32>`), which cannot be filtered by a sampler. Luma is
// an exact texel fetch. A subsampled chroma plane (4:2:0, 4:2:2) is
// reconstructed by `chroma_codes` -- a luma-guided fit over the four
// nearest centre-sited chroma samples -- because a nearest fetch repeats
// each sample over its whole block and paints hard colour steps on text
// edges. The YCbCr -> RGB matrix is affine in the code values, so
// reconstructing codes before converting is sound; only the final clamp is
// non-linear and it still happens last. 4:4:4 keeps the exact fetch, so its
// output is unchanged.

struct VideoUniform {
    // Luma code range: `code` -> `(code - luma_offset) * luma_span_inv`,
    // i.e. `(luma_offset, 1.0 / luma_span)` from
    // `arcen_media::ColorRange::luma_bounds`. Also the *only* scaling used
    // by the identity/GBR branch for all three planes (ITU-T H.273 leaves
    // identity planes on the luma scaling -- see
    // `ColorTransform::scale_identity`), not `chroma_center`/`chroma_span_inv`.
    luma_offset: f32,
    luma_span_inv: f32,
    // Chroma code range/centre for the matrix branch only -- see
    // `arcen_media::ColorRange::chroma_bounds`, except full-range centring
    // uses the ITU convention `1 << (depth.bits() - 1)` rather than the
    // arithmetic mean of the bounds (they differ by half a code).
    chroma_center: f32,
    chroma_span_inv: f32,
    // Matrix coefficients (Kr, Kb; Kg = 1 - Kr - Kb, precomputed as
    // `kg_inv = 1 / Kg`). Unused (left at the identity-safe values
    // `kr = kb = 0`, `kg_inv = 1`) for `MODE_IDENTITY_GBR`/`MODE_PACKED_RGBA`.
    kr: f32,
    kb: f32,
    kg_inv: f32,
    // One of `MODE_MATRIX` / `MODE_IDENTITY_GBR` / `MODE_PACKED_RGBA`.
    mode: u32,
    // Plane pixel dimensions, used to map a `[0, 1]` UV onto an exact
    // integer texel coordinate per plane (chroma may be subsampled
    // relative to luma for 4:2:0/4:2:2; for 4:4:4 -- this feature's
    // flagship target -- chroma_size == luma_size).
    luma_width: f32,
    luma_height: f32,
    chroma_width: f32,
    chroma_height: f32,
}

// Keep in sync with `ShaderMode` in `video_render.rs`.
const MODE_MATRIX: u32 = 0u;
const MODE_IDENTITY_GBR: u32 = 1u;
const MODE_PACKED_RGBA: u32 = 2u;

// plane0: luma (`MODE_MATRIX`/`MODE_IDENTITY_GBR`) or the whole packed RGBA
// image (`MODE_PACKED_RGBA`, today's only reachable mode -- see
// `RawVideoPayload::PackedRgba8`).
@group(0) @binding(0) var plane0: texture_2d<u32>;
// plane1: interleaved two-component chroma -- `.r` is the Cb slot, `.g` is
// the Cr slot, matching CoreVideo's biplanar layout and
// `ColorTransform::to_bgr8`'s own `(cb, cr)` parameter order. Unused (bound
// to a 1x1 dummy) for `MODE_PACKED_RGBA`.
@group(0) @binding(1) var plane1: texture_2d<u32>;
@group(0) @binding(2) var<uniform> u: VideoUniform;

struct VsOut {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

// Big-triangle trick: three vertices, no vertex buffer, covering the whole
// clip-space rectangle. `egui_wgpu::Renderer::render` has already set the
// render pass viewport (and scissor) to this callback's assigned `rect`
// before `paint()` runs (see `epaint::PaintCallback`'s own doc), so no
// further transform is needed here -- the viewport transform alone places
// this triangle exactly where the caller asked for it.
@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> VsOut {
    let x = f32((vertex_index << 1u) & 2u);
    let y = f32(vertex_index & 2u);
    var out: VsOut;
    out.clip_position = vec4<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
    out.uv = vec2<f32>(x, y);
    return out;
}

// The WGSL twin of `video_metal_layer.metal`'s `chroma_codes` (luma-guided
// "chroma from luma" reconstruction); see that function for the siting, the
// fit and why. `VideoUniform::chroma_reference_codes` in `video_render.rs`
// is the CPU reference both are tested against -- change all three
// together. Integer code planes cannot be filtered, so each chroma sample's
// block-mean luma is gathered with explicit loads here.
const CHROMA_FIT_EPSILON: f32 = 3.0e-4;

fn block_mean_luma(tap: vec2<i32>, ratio: vec2<i32>) -> f32 {
    let last = vec2<i32>(i32(u.luma_width), i32(u.luma_height)) - vec2<i32>(1, 1);
    var sum = 0.0;
    for (var dy = 0; dy < ratio.y; dy++) {
        for (var dx = 0; dx < ratio.x; dx++) {
            let at = clamp(tap * ratio + vec2<i32>(dx, dy), vec2<i32>(0, 0), last);
            sum += f32(textureLoad(plane0, at, 0).r);
        }
    }
    let code = sum / f32(ratio.x * ratio.y);
    return (code - u.luma_offset) * u.luma_span_inv;
}

fn chroma_codes(uv: vec2<f32>, pixel_luma: f32) -> vec2<f32> {
    let luma_size = vec2<f32>(u.luma_width, u.luma_height);
    let chroma_size = vec2<f32>(u.chroma_width, u.chroma_height);
    let exact = vec2<i32>(uv * chroma_size);
    let full = chroma_size >= luma_size;
    let own = vec2<f32>(textureLoad(plane1, exact, 0).rg);
    if (all(full)) {
        return own;
    }
    let ratio = vec2<i32>(round(luma_size / chroma_size));
    let position = uv * chroma_size - vec2<f32>(0.5, 0.5);
    var base = floor(position);
    var weight = position - base;
    if (full.x) {
        base.x = f32(exact.x);
        weight.x = 0.0;
    }
    if (full.y) {
        base.y = f32(exact.y);
        weight.y = 0.0;
    }
    let last = vec2<i32>(chroma_size) - vec2<i32>(1, 1);
    let c0 = clamp(vec2<i32>(base), vec2<i32>(0, 0), last);
    var c1 = clamp(vec2<i32>(base) + vec2<i32>(1, 1), vec2<i32>(0, 0), last);
    if (full.x) {
        c1.x = c0.x;
    }
    if (full.y) {
        c1.y = c0.y;
    }
    var taps = array<vec2<i32>, 4>(
        vec2<i32>(c0.x, c0.y),
        vec2<i32>(c1.x, c0.y),
        vec2<i32>(c0.x, c1.y),
        vec2<i32>(c1.x, c1.y),
    );
    var tap_weight = array<f32, 4>(
        (1.0 - weight.x) * (1.0 - weight.y),
        weight.x * (1.0 - weight.y),
        (1.0 - weight.x) * weight.y,
        weight.x * weight.y,
    );
    var tap_luma: array<f32, 4>;
    var tap_chroma: array<vec2<f32>, 4>;
    var low = vec2<f32>(3.0e38, 3.0e38);
    var high = vec2<f32>(-3.0e38, -3.0e38);
    for (var i = 0; i < 4; i++) {
        tap_chroma[i] = vec2<f32>(textureLoad(plane1, taps[i], 0).rg);
        low = min(low, tap_chroma[i]);
        high = max(high, tap_chroma[i]);
    }
    if (all(high == low)) {
        return own;
    }
    var mean_luma = 0.0;
    var mean_chroma = vec2<f32>(0.0, 0.0);
    for (var i = 0; i < 4; i++) {
        tap_luma[i] = block_mean_luma(taps[i], ratio);
        mean_luma += tap_weight[i] * tap_luma[i];
        mean_chroma += tap_weight[i] * tap_chroma[i];
    }
    var luma_variance = 0.0;
    var chroma_variance = vec2<f32>(0.0, 0.0);
    var covariance = vec2<f32>(0.0, 0.0);
    for (var i = 0; i < 4; i++) {
        let dy = tap_luma[i] - mean_luma;
        let dc = tap_chroma[i] - mean_chroma;
        luma_variance += tap_weight[i] * dy * dy;
        chroma_variance += tap_weight[i] * dc * dc;
        covariance += tap_weight[i] * dy * dc;
    }
    let luma_here = (pixel_luma - u.luma_offset) * u.luma_span_inv;
    let slope = covariance / (luma_variance + CHROMA_FIT_EPSILON);
    let fitted = clamp(mean_chroma + slope * (luma_here - mean_luma), low, high);
    let denominator = luma_variance * chroma_variance;
    let confidence = select(
        vec2<f32>(0.0, 0.0),
        clamp(covariance * covariance / max(denominator, vec2<f32>(1.0e-12, 1.0e-12)), vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 1.0)),
        denominator > vec2<f32>(0.0, 0.0),
    );
    return mix(own, fitted, confidence);
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    if (u.mode == MODE_PACKED_RGBA) {
        // Already-decoded RGB (today's only reachable source -- see
        // `video_decoder.rs`'s CPU `VTPixelTransferSession` -> BGRA ->
        // RGBA path): no matrix, just normalise the 8-bit-in-u32 texel.
        let coord = vec2<i32>(in.uv * vec2<f32>(u.luma_width, u.luma_height));
        let texel = textureLoad(plane0, coord, 0);
        return vec4<f32>(vec3<f32>(texel.rgb) / 255.0, 1.0);
    }

    let luma_coord = vec2<i32>(in.uv * vec2<f32>(u.luma_width, u.luma_height));
    let luma_code = f32(textureLoad(plane0, luma_coord, 0).r);
    let chroma_code = chroma_codes(in.uv, luma_code);
    let cb_code = chroma_code.x;
    let cr_code = chroma_code.y;

    if (u.mode == MODE_IDENTITY_GBR) {
        // Identity/GBR passthrough: the host can emit this (no CoreVideo
        // `YCbCrMatrix` constant describes it, so the CVPixelBuffer-based
        // decode path cannot -- see `video_decoder.rs::ColorExtensionPlan`'s
        // own doc), and here it is handled correctly: plane0 -> G, the
        // chroma plane's Cb slot -> B, its Cr slot -> R, all three scaled
        // by the *luma* range exactly like `ColorTransform::to_bgr8`'s own
        // identity branch (`(unscale(cb), unscale(luma), unscale(cr))` as
        // `(b, g, r)`).
        let g = (luma_code - u.luma_offset) * u.luma_span_inv;
        let b = (cb_code - u.luma_offset) * u.luma_span_inv;
        let r = (cr_code - u.luma_offset) * u.luma_span_inv;
        return vec4<f32>(clamp(r, 0.0, 1.0), clamp(g, 0.0, 1.0), clamp(b, 0.0, 1.0), 1.0);
    }

    // Matrix branch (BT.709 / BT.601 / BT.2020 NCL), mirroring
    // `ColorTransform::to_bgr8`'s non-identity path exactly:
    //   luma_norm = (Y - luma_offset) / luma_span
    //   blue_diff = (Cb - chroma_center) / chroma_span
    //   red_diff  = (Cr - chroma_center) / chroma_span
    //   R = luma_norm + 2*(1-Kr)*red_diff
    //   B = luma_norm + 2*(1-Kb)*blue_diff
    //   G = (luma_norm - Kr*R - Kb*B) / Kg
    let luma_norm = (luma_code - u.luma_offset) * u.luma_span_inv;
    let blue_diff = (cb_code - u.chroma_center) * u.chroma_span_inv;
    let red_diff = (cr_code - u.chroma_center) * u.chroma_span_inv;
    let red = luma_norm + 2.0 * (1.0 - u.kr) * red_diff;
    let blue = luma_norm + 2.0 * (1.0 - u.kb) * blue_diff;
    let green = (luma_norm - u.kr * red - u.kb * blue) * u.kg_inv;
    return vec4<f32>(clamp(red, 0.0, 1.0), clamp(green, 0.0, 1.0), clamp(blue, 0.0, 1.0), 1.0);
}

// ---------------------------------------------------------------------------
// Pass 2 ("present"): bilinear resolve of the already-converted, already-
// 10-bit picture into whatever render pass the caller bound (`egui`'s own
// shared swapchain pass in production; see `VideoRendererResources::draw`).
// A *separate* bind group (still `@group(0)`, but a different pipeline/
// layout -- see `video_render.rs`) from the conversion pass above.

@group(0) @binding(0) var resolved_tex: texture_2d<f32>;
@group(0) @binding(1) var resolved_sampler: sampler;

@fragment
fn fs_composite(in: VsOut) -> @location(0) vec4<f32> {
    return textureSample(resolved_tex, resolved_sampler, in.uv);
}
