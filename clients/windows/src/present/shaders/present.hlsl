// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
// Arcen Windows Deck -- video conversion pass (HLSL, Shader Model 5.0).
//
// The HLSL twin of `clients/macos/src/ui/video_render.wgsl` (`fs_main`,
// `chroma_codes`) and `video_metal_layer.metal` (`fs_convert*`), tested
// against the shared CPU references in `arcen_media::video::present`. The colour
// maths -- offsets, spans, matrix, identity/GBR branch, the luma-guided
// chroma reconstruction -- is identical, formula for formula; change all
// three together. Planes are bound as UINT views (raw codes), like WGSL:
//
//   biplanar NV12  : t0 R8_UINT  (Y),  t1 R8G8_UINT  (Cb, Cr)
//   biplanar P010  : t0 R16_UINT (Y),  t1 R16G16_UINT (Cb, Cr), codes >> 6
//   packed Y410    : t3 R10G10B10A2_UINT (R=Cb, G=Y, B=Cr)
//   packed AYUV    : t3 R8G8B8A8_UINT    (R=Cr, G=Cb, B=Y)
//
// Every plane is a Texture2DArray view of one slice, so decoder output
// arrays (D3D11_BIND_DECODER) and plain textures bind the same way.
//
// The target is drawn at the source's own size; DirectComposition scales
// the visual (the Mac's `drawableSize` vs `CALayer.frame`).

cbuffer VideoUniform : register(b0)
{
    // Field for field `arcen_media::video::present::VideoUniform::to_bytes`
    // (48 bytes, shared with the WGSL and Metal shaders) ...
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
    // ... then the Windows-only 16 bytes.
    uint code_shift;   // P010: 6; everything else 0
    uint output_kind;  // OUTPUT_CODES or OUTPUT_PQ_TO_SCRGB
    float scrgb_scale; // nits per scRGB 1.0 (80)
    uint pad0;
};

static const uint MODE_MATRIX = 0;
static const uint MODE_IDENTITY_GBR = 1;

// Codes as they are: the swapchain is tagged with the stream's own
// transfer (G22_P709 for SDR, G2084_P2020 for PQ), like the Mac's layer.
static const uint OUTPUT_CODES = 0;
// The FP16 scRGB fallback: PQ BT.2020 codes -> linear BT.709 / 80 nits.
static const uint OUTPUT_PQ_TO_SCRGB = 1;

Texture2DArray<uint> plane0 : register(t0);
Texture2DArray<uint2> plane1 : register(t1);
Texture2D<uint4> overlay : register(t2);
Texture2DArray<uint4> packed_plane : register(t3);

struct VsOut
{
    float4 position : SV_Position;
    float2 uv : TEXCOORD0;
};

VsOut vs_main(uint vertex_id : SV_VertexID)
{
    float x = float((vertex_id << 1) & 2);
    float y = float(vertex_id & 2);
    VsOut output_vertex;
    output_vertex.position = float4(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
    output_vertex.uv = float2(x, y);
    return output_vertex;
}

uint load_luma(int2 at)
{
    return plane0.Load(int4(at, 0, 0)) >> code_shift;
}

float2 load_chroma(int2 at)
{
    return float2(plane1.Load(int4(at, 0, 0)) >> code_shift);
}

static const float CHROMA_FIT_EPSILON = 3.0e-4;

float block_mean_luma(int2 tap, int2 ratio)
{
    int2 last = int2(int(luma_width), int(luma_height)) - int2(1, 1);
    float sum = 0.0;
    for (int dy = 0; dy < ratio.y; dy++)
    {
        for (int dx = 0; dx < ratio.x; dx++)
        {
            int2 at = clamp(tap * ratio + int2(dx, dy), int2(0, 0), last);
            sum += float(load_luma(at));
        }
    }
    float code = sum / float(ratio.x * ratio.y);
    return (code - luma_offset) * luma_span_inv;
}

float2 chroma_codes(float2 uv, float pixel_luma)
{
    float2 luma_size = float2(luma_width, luma_height);
    float2 chroma_size = float2(chroma_width, chroma_height);
    int2 exact = int2(uv * chroma_size);
    bool2 full = chroma_size >= luma_size;
    float2 own = load_chroma(exact);
    if (all(full))
    {
        return own;
    }
    int2 ratio = int2(round(luma_size / chroma_size));
    float2 position = uv * chroma_size - float2(0.5, 0.5);
    float2 base = floor(position);
    float2 weight = position - base;
    if (full.x)
    {
        base.x = float(exact.x);
        weight.x = 0.0;
    }
    if (full.y)
    {
        base.y = float(exact.y);
        weight.y = 0.0;
    }
    int2 last = int2(chroma_size) - int2(1, 1);
    int2 c0 = clamp(int2(base), int2(0, 0), last);
    int2 c1 = clamp(int2(base) + int2(1, 1), int2(0, 0), last);
    if (full.x)
    {
        c1.x = c0.x;
    }
    if (full.y)
    {
        c1.y = c0.y;
    }
    int2 taps[4] = { int2(c0.x, c0.y), int2(c1.x, c0.y), int2(c0.x, c1.y), int2(c1.x, c1.y) };
    float tap_weight[4] = {
        (1.0 - weight.x) * (1.0 - weight.y),
        weight.x * (1.0 - weight.y),
        (1.0 - weight.x) * weight.y,
        weight.x * weight.y,
    };
    float tap_luma[4];
    float2 tap_chroma[4];
    float2 low = float2(3.0e38, 3.0e38);
    float2 high = float2(-3.0e38, -3.0e38);
    [unroll]
    for (int i = 0; i < 4; i++)
    {
        tap_chroma[i] = load_chroma(taps[i]);
        low = min(low, tap_chroma[i]);
        high = max(high, tap_chroma[i]);
    }
    if (all(high == low))
    {
        return own;
    }
    float mean_luma = 0.0;
    float2 mean_chroma = float2(0.0, 0.0);
    [unroll]
    for (int j = 0; j < 4; j++)
    {
        tap_luma[j] = block_mean_luma(taps[j], ratio);
        mean_luma += tap_weight[j] * tap_luma[j];
        mean_chroma += tap_weight[j] * tap_chroma[j];
    }
    float luma_variance = 0.0;
    float2 chroma_variance = float2(0.0, 0.0);
    float2 covariance = float2(0.0, 0.0);
    [unroll]
    for (int k = 0; k < 4; k++)
    {
        float dy = tap_luma[k] - mean_luma;
        float2 dc = tap_chroma[k] - mean_chroma;
        luma_variance += tap_weight[k] * dy * dy;
        chroma_variance += tap_weight[k] * dc * dc;
        covariance += tap_weight[k] * dy * dc;
    }
    float luma_here = (pixel_luma - luma_offset) * luma_span_inv;
    float2 slope = covariance / (luma_variance + CHROMA_FIT_EPSILON);
    float2 fitted = clamp(mean_chroma + slope * (luma_here - mean_luma), low, high);
    float2 denominator = luma_variance * chroma_variance;
    float2 confidence = denominator > float2(0.0, 0.0)
        ? saturate(covariance * covariance / max(denominator, float2(1.0e-12, 1.0e-12)))
        : float2(0.0, 0.0);
    return lerp(own, fitted, confidence);
}

// ST 2084 EOTF: PQ signal -> nits.
float3 pq_to_nits(float3 signal)
{
    const float m1 = 2610.0 / 16384.0;
    const float m2 = 2523.0 / 32.0;
    const float c1 = 3424.0 / 4096.0;
    const float c2 = 2413.0 / 128.0;
    const float c3 = 2392.0 / 128.0;
    float3 powered = pow(saturate(signal), 1.0 / m2);
    float3 numerator = max(powered - c1, 0.0);
    float3 denominator = c2 - c3 * powered;
    return pow(numerator / denominator, 1.0 / m1) * 10000.0;
}

float4 finish(float3 rgb)
{
    if (output_kind == OUTPUT_PQ_TO_SCRGB)
    {
        // BT.2020 -> BT.709 primaries (ITU-R BT.2087 linear matrix).
        float3 nits = pq_to_nits(rgb);
        float3 bt709 = float3(
            dot(float3(1.660491, -0.587641, -0.072850), nits),
            dot(float3(-0.124550, 1.132900, -0.008349), nits),
            dot(float3(-0.018151, -0.100579, 1.118730), nits));
        return float4(bt709 / scrgb_scale, 1.0);
    }
    return float4(rgb, 1.0);
}

// The range and matrix step from the three codes of one pixel; shared by
// the video and by Y'CbCr refinement tiles (ADR 0016 R1).
float3 codes_to_rgb(float luma_code, float cb_code, float cr_code)
{
    if (mode == MODE_IDENTITY_GBR)
    {
        float g = (luma_code - luma_offset) * luma_span_inv;
        float b = (cb_code - luma_offset) * luma_span_inv;
        float r = (cr_code - luma_offset) * luma_span_inv;
        return saturate(float3(r, g, b));
    }
    float luma_norm = (luma_code - luma_offset) * luma_span_inv;
    float blue_diff = (cb_code - chroma_center) * chroma_span_inv;
    float red_diff = (cr_code - chroma_center) * chroma_span_inv;
    float red = luma_norm + 2.0 * (1.0 - kr) * red_diff;
    float blue = luma_norm + 2.0 * (1.0 - kb) * blue_diff;
    float green = (luma_norm - kr * red - kb * blue) * kg_inv;
    return saturate(float3(red, green, blue));
}

int2 luma_coord(float2 uv)
{
    return int2(uv * float2(luma_width, luma_height));
}

float3 convert_biplanar(float2 uv)
{
    float luma_code = float(load_luma(luma_coord(uv)));
    float2 chroma = chroma_codes(uv, luma_code);
    return codes_to_rgb(luma_code, chroma.x, chroma.y);
}

// Packed 4:4:4 layouts carry all three codes in one texel.
float3 convert_y410(float2 uv)
{
    uint4 texel = packed_plane.Load(int4(luma_coord(uv), 0, 0));
    return codes_to_rgb(float(texel.g), float(texel.r), float(texel.b));
}

float3 convert_ayuv(float2 uv)
{
    uint4 texel = packed_plane.Load(int4(luma_coord(uv), 0, 0));
    return codes_to_rgb(float(texel.b), float(texel.g), float(texel.r));
}

// Exact still image (ADR 0016 R1). The overlay is R10G10B10A2_UINT at the
// source size; a nonzero alpha marks a refined, currently valid pixel.
bool refined_codes(float2 uv, out uint4 code)
{
    uint width;
    uint height;
    overlay.GetDimensions(width, height);
    int2 at = luma_coord(uv);
    code = uint4(0, 0, 0, 0);
    if (at.x < int(width) && at.y < int(height))
    {
        code = overlay.Load(int3(at, 0));
    }
    return code.a != 0;
}

// RGB domain: the source's own R'G'B' code, written directly (code / 1023
// into an R10G10B10A2_UNORM target stores exactly the code); no matrix.
float4 refined_rgb(uint4 code)
{
    return finish(float3(code.rgb) / 1023.0);
}

// Y'CbCr domain: the codes replace the decoded ones before the range and
// matrix step, with the video's own uniforms.
float4 refined_ycbcr(uint4 code)
{
    return finish(codes_to_rgb(float(code.r), float(code.g), float(code.b)));
}

float4 ps_biplanar(VsOut input) : SV_Target
{
    return finish(convert_biplanar(input.uv));
}

float4 ps_y410(VsOut input) : SV_Target
{
    return finish(convert_y410(input.uv));
}

float4 ps_ayuv(VsOut input) : SV_Target
{
    return finish(convert_ayuv(input.uv));
}

float4 ps_biplanar_refined_rgb(VsOut input) : SV_Target
{
    uint4 code;
    if (refined_codes(input.uv, code))
    {
        return refined_rgb(code);
    }
    return finish(convert_biplanar(input.uv));
}

float4 ps_biplanar_refined_ycbcr(VsOut input) : SV_Target
{
    uint4 code;
    if (refined_codes(input.uv, code))
    {
        return refined_ycbcr(code);
    }
    return finish(convert_biplanar(input.uv));
}

float4 ps_y410_refined_rgb(VsOut input) : SV_Target
{
    uint4 code;
    if (refined_codes(input.uv, code))
    {
        return refined_rgb(code);
    }
    return finish(convert_y410(input.uv));
}

float4 ps_y410_refined_ycbcr(VsOut input) : SV_Target
{
    uint4 code;
    if (refined_codes(input.uv, code))
    {
        return refined_ycbcr(code);
    }
    return finish(convert_y410(input.uv));
}
