// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
// Arcen Windows Deck -- canvas passes (HLSL, Shader Model 5.0), ADR 0028.
//
// The GPU half of `arcen_media::canvas::CanvasPixels` for the operations a
// copy cannot do: fills, and surface paints that write listed blocks from
// a converted picture scaled by nearest sample. Both draw instanced
// rectangles into the B8G8R8A8_UNORM canvas; the output merger writes them
// in instance order, as the CPU executor writes them in operation order.
//
// Exactness: a fill writes code / 255.0 and a paint writes the UNORM texel
// it loaded, both of which an eight-bit UNORM target stores as exactly that
// code. The paint's source texel is the conversion pass's own result
// (`present.hlsl` into a B8G8R8A8_UNORM picture), so the canvas shows the
// surface as the eight-bit presenter would.

cbuffer CanvasPass : register(b0)
{
    // Destination of the whole picture: x, y, width, height.
    uint4 dest;
    // Picture size, then canvas size.
    uint2 picture_size;
    uint2 canvas_size;
};

struct CanvasRect
{
    // x, y, width, height in canvas pixels.
    uint4 rect;
    // Fills: the colour as 0xAARRGGBB; paints: unused.
    uint4 extra;
};

StructuredBuffer<CanvasRect> rects : register(t0);
Texture2D<float4> picture : register(t1);

struct RectOut
{
    float4 position : SV_Position;
    nointerpolation uint colour : COLOUR;
};

RectOut vs_rect(uint vertex_id : SV_VertexID, uint instance : SV_InstanceID)
{
    // Two triangles per rectangle: corners 0 1 2, 2 1 3 (bit 0 = right,
    // bit 1 = bottom).
    static const uint corners[6] = { 0, 1, 2, 2, 1, 3 };
    CanvasRect item = rects[instance];
    uint corner = corners[vertex_id];
    float x = float(item.rect.x + ((corner & 1) != 0 ? item.rect.z : 0));
    float y = float(item.rect.y + ((corner & 2) != 0 ? item.rect.w : 0));
    RectOut output_vertex;
    output_vertex.position = float4(
        x / float(canvas_size.x) * 2.0 - 1.0,
        1.0 - y / float(canvas_size.y) * 2.0,
        0.0,
        1.0);
    output_vertex.colour = item.extra.x;
    return output_vertex;
}

float4 ps_fill(RectOut input) : SV_Target
{
    uint colour = input.colour;
    return float4(
        float((colour >> 16) & 0xff) / 255.0,
        float((colour >> 8) & 0xff) / 255.0,
        float(colour & 0xff) / 255.0,
        1.0);
}

// `CanvasPixels::paint_picture`: canvas pixel (x, y) inside `dest` takes
// picture pixel ((x - dest.x) * width / dest.width, (y - dest.y) * height /
// dest.height), opaque.
float4 ps_paint(RectOut input) : SV_Target
{
    uint2 at = uint2(input.position.xy);
    uint sx = (at.x - dest.x) * picture_size.x / dest.z;
    uint sy = (at.y - dest.y) * picture_size.y / dest.w;
    return float4(picture.Load(int3(int(sx), int(sy), 0)).rgb, 1.0);
}
