// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! GPU-vs-shared-CPU-reference checks of the conversion pass and the
//! refinement compositor. `cargo test` runs them on WARP (any Windows, no
//! GPU); the ignored test `..._on_every_hardware_adapter` runs them on each
//! of the machine's GPUs. The references are `arcen_media::video::present`'s,
//! the same the Mac's WGSL and Metal shaders are tested against.

#![allow(
    clippy::unwrap_used,
    clippy::too_many_lines,
    reason = "diagnostic checks over pictures this module builds itself"
)]

use super::Result;
use super::convert::{ConvertUniform, Converter, OutputKind, RefineDomain, SourcePlanes};
use super::device::Gpu;
use super::refine::D3d11RefineOverlay;
use arcen_media::refine::compositor::OverlayWork;
use arcen_media::refine::hub::RefineCompositor;
use arcen_media::video::pq_bt2020_to_scrgb;
use arcen_media::video::present::{VideoColorContract, VideoUniform};
use arcen_media::{
    BitDepth, ChromaSubsampling, ColorMatrix, ColorPrimaries, ColorRange, TransferCharacteristics,
};

/// Auto/Speed: 4:2:0 8-bit BT.709 limited.
pub const AUTO: VideoColorContract = VideoColorContract {
    chroma: ChromaSubsampling::Yuv420,
    range: ColorRange::Limited,
    depth: BitDepth::Eight,
    matrix: ColorMatrix::Bt709,
    primaries: ColorPrimaries::Bt709,
    transfer: TransferCharacteristics::Bt709,
};
/// Grading: 4:4:4 10-bit BT.709 full range, sRGB transfer.
pub const GRADING: VideoColorContract = VideoColorContract {
    chroma: ChromaSubsampling::Yuv444,
    range: ColorRange::Full,
    depth: BitDepth::Ten,
    matrix: ColorMatrix::Bt709,
    primaries: ColorPrimaries::Bt709,
    transfer: TransferCharacteristics::Srgb,
};
/// HDR: 4:4:4 10-bit BT.2020 NCL full range, PQ.
pub const HDR: VideoColorContract = VideoColorContract {
    chroma: ChromaSubsampling::Yuv444,
    range: ColorRange::Full,
    depth: BitDepth::Ten,
    matrix: ColorMatrix::Bt2020Ncl,
    primaries: ColorPrimaries::Bt2020,
    transfer: TransferCharacteristics::Pq,
};

use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;

#[derive(Debug)]
pub struct Rng(pub u32);
impl Rng {
    pub fn next_u32(&mut self) -> u32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 17;
        self.0 ^= self.0 << 5;
        self.0
    }
    fn code(&mut self, low: u16, high: u16) -> u16 {
        low + (self.next_u32() % u32::from(high - low + 1)) as u16
    }
}

/// A CPU picture: luma and interleaved chroma codes.
#[derive(Debug, Clone)]
pub struct Picture {
    pub width: u32,
    pub height: u32,
    pub chroma_width: u32,
    pub chroma_height: u32,
    pub luma: Vec<u16>,
    pub chroma: Vec<[u16; 2]>,
}

impl Picture {
    fn luma_at(&self, x: u32, y: u32) -> f32 {
        f32::from(self.luma[(y * self.width + x) as usize])
    }
    fn chroma_at(&self, x: u32, y: u32) -> [f32; 2] {
        let [cb, cr] = self.chroma[(y * self.chroma_width + x) as usize];
        [f32::from(cb), f32::from(cr)]
    }

    /// Text-like content: flat colours with sharp edges, plus noise, so the
    /// chroma fit is exercised on 4:2:0.
    pub fn synthetic(contract: VideoColorContract, width: u32, height: u32, seed: u32) -> Self {
        let (chroma_width, chroma_height) = match contract.chroma {
            ChromaSubsampling::Yuv420 => (width / 2, height / 2),
            ChromaSubsampling::Yuv422 => (width / 2, height),
            ChromaSubsampling::Yuv444 => (width, height),
        };
        let (luma_lo, luma_hi) = contract.range.luma_bounds(contract.depth);
        let (chroma_lo, chroma_hi) = contract.range.chroma_bounds(contract.depth);
        let mut rng = Rng(seed | 1);
        let mut luma = Vec::with_capacity((width * height) as usize);
        for y in 0..height {
            for x in 0..width {
                let stroke = (x % 11 < 3) ^ (y % 7 < 2);
                let value = if (x / 16 + y / 16) % 3 == 0 {
                    rng.code(luma_lo, luma_hi)
                } else if stroke {
                    luma_lo + (luma_hi - luma_lo) / 8
                } else {
                    luma_hi - (luma_hi - luma_lo) / 10
                };
                luma.push(value);
            }
        }
        let mut chroma = Vec::with_capacity((chroma_width * chroma_height) as usize);
        for y in 0..chroma_height {
            for x in 0..chroma_width {
                if (x / 8 + y / 8) % 2 == 0 {
                    chroma.push([
                        rng.code(chroma_lo, chroma_hi),
                        rng.code(chroma_lo, chroma_hi),
                    ]);
                } else {
                    let stroke = x % 5 < 2;
                    let mid = chroma_lo.midpoint(chroma_hi);
                    chroma.push(if stroke {
                        [
                            chroma_lo + (mid - chroma_lo) / 3,
                            chroma_hi - (chroma_hi - mid) / 4,
                        ]
                    } else {
                        [mid, mid]
                    });
                }
            }
        }
        Self {
            width,
            height,
            chroma_width,
            chroma_height,
            luma,
            chroma,
        }
    }

    /// The texture a decoder would hand over, in `format`.
    pub fn upload(&self, gpu: &Gpu, format: DXGI_FORMAT) -> Result<ID3D11Texture2D> {
        let (w, h) = (self.width, self.height);
        let mut bytes: Vec<u8> = Vec::new();
        let pitch;
        match format {
            DXGI_FORMAT_NV12 => {
                pitch = w;
                bytes.extend(self.luma.iter().map(|&c| c as u8));
                for &[cb, cr] in &self.chroma {
                    bytes.push(cb as u8);
                    bytes.push(cr as u8);
                }
            }
            DXGI_FORMAT_P010 => {
                pitch = w * 2;
                for &c in &self.luma {
                    bytes.extend_from_slice(&(c << 6).to_le_bytes());
                }
                for &[cb, cr] in &self.chroma {
                    bytes.extend_from_slice(&(cb << 6).to_le_bytes());
                    bytes.extend_from_slice(&(cr << 6).to_le_bytes());
                }
            }
            DXGI_FORMAT_Y410 => {
                pitch = w * 4;
                for (index, &y) in self.luma.iter().enumerate() {
                    let [cb, cr] = self.chroma[index];
                    let word = u32::from(cb) | u32::from(y) << 10 | u32::from(cr) << 20 | 3 << 30;
                    bytes.extend_from_slice(&word.to_le_bytes());
                }
            }
            DXGI_FORMAT_AYUV => {
                pitch = w * 4;
                for (index, &y) in self.luma.iter().enumerate() {
                    let [cb, cr] = self.chroma[index];
                    bytes.extend_from_slice(&[cr as u8, cb as u8, y as u8, 255]);
                }
            }
            other => return Err(format!("no uploader for {other:?}")),
        }
        gpu.texture(
            w,
            h,
            format,
            D3D11_BIND_SHADER_RESOURCE,
            Some((&bytes, pitch)),
        )
    }

    /// CPU reference R′G′B′ (normalised) of one pixel, as the shader does it.
    pub fn reference(&self, uniform: &VideoUniform, x: u32, y: u32) -> [f32; 3] {
        let uv = [
            (x as f32 + 0.5) / self.width as f32,
            (y as f32 + 0.5) / self.height as f32,
        ];
        let luma = self.luma_at(x, y);
        let [cb, cr] = uniform.chroma_reference_codes(
            uv,
            luma,
            |lx, ly| self.luma_at(lx, ly),
            |cx, cy| self.chroma_at(cx, cy),
        );
        uniform.reference_rgb(luma, cb, cr)
    }
}

#[derive(Debug, Default)]
pub struct Comparison {
    pub name: String,
    pub pixels: usize,
    pub exact: usize,
    pub off_by_one: usize,
    pub worst: f32,
}

impl Comparison {
    pub fn passed(&self, tolerance: f32) -> bool {
        self.worst <= tolerance
    }
    pub fn line(&self, tolerance: f32) -> String {
        format!(
            "{} {}: pixels={} exact={} ({:.3}%) off_by_one={} worst={:.4} tolerance={tolerance}",
            if self.passed(tolerance) {
                "PASS"
            } else {
                "FAIL"
            },
            self.name,
            self.pixels,
            self.exact,
            100.0 * self.exact as f64 / self.pixels.max(1) as f64,
            self.off_by_one,
            self.worst,
        )
    }
}

fn words(bytes: &[u8]) -> impl Iterator<Item = u32> + '_ {
    bytes
        .chunks_exact(4)
        .map(|chunk| u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
}

fn rgb10(word: u32) -> [u16; 3] {
    [
        (word & 0x3ff) as u16,
        ((word >> 10) & 0x3ff) as u16,
        ((word >> 20) & 0x3ff) as u16,
    ]
}

fn half(bits: u16) -> f32 {
    arcen_media::video::half_to_f32(bits)
}

fn render(
    gpu: &Gpu,
    converter: &Converter,
    picture: &Picture,
    format: DXGI_FORMAT,
    uniform: ConvertUniform,
    target_format: DXGI_FORMAT,
    overlay: Option<(&ID3D11ShaderResourceView, RefineDomain)>,
) -> Result<(ID3D11Texture2D, Vec<u8>)> {
    let texture = picture.upload(gpu, format)?;
    let planes = SourcePlanes::of_texture(gpu, &texture, 0)?;
    let target = gpu.texture(
        picture.width,
        picture.height,
        target_format,
        D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE,
        None,
    )?;
    let rtv = gpu.rtv(&target)?;
    converter.draw(gpu, &planes, uniform, &rtv, overlay)?;
    let texel = if target_format == DXGI_FORMAT_R16G16B16A16_FLOAT {
        8
    } else {
        4
    };
    let bytes = gpu.read_region(&target, 0, 0, picture.width, picture.height, texel)?;
    Ok((target, bytes))
}

/// Converts `picture` on the GPU into R10G10B10A2_UNORM and compares every
/// pixel with the CPU reference, in 10-bit codes.
fn compare_ten_bit(
    gpu: &Gpu,
    converter: &Converter,
    name: &str,
    contract: VideoColorContract,
    format: DXGI_FORMAT,
    picture: &Picture,
) -> Result<Comparison> {
    let uniform = VideoUniform::from_contract(
        contract,
        (picture.width, picture.height),
        (picture.chroma_width, picture.chroma_height),
    );
    let (_, bytes) = render(
        gpu,
        converter,
        picture,
        format,
        ConvertUniform::new(uniform, OutputKind::Codes),
        DXGI_FORMAT_R10G10B10A2_UNORM,
        None,
    )?;
    let mut result = Comparison {
        name: name.into(),
        ..Default::default()
    };
    for (index, word) in words(&bytes).enumerate() {
        let (x, y) = (index as u32 % picture.width, index as u32 / picture.width);
        let reference = picture.reference(&uniform, x, y);
        let got = rgb10(word);
        // Distance from the exactly rounded reference, in codes.
        let rounded = reference.map(|c| (c * 1023.0).round() as u16);
        let diff = (0..3).map(|c| got[c].abs_diff(rounded[c])).max().unwrap();
        result.pixels += 1;
        if diff == 0 {
            result.exact += 1;
        } else if diff == 1 {
            result.off_by_one += 1;
        }
        result.worst = result.worst.max(f32::from(diff));
    }
    Ok(result)
}

/// The 8-bit Auto path against the shared `ColorTransform::to_bgr8`
/// (AYUV 4:4:4, so no chroma reconstruction is involved).
fn compare_auto_with_shared_transform(gpu: &Gpu, converter: &Converter) -> Result<Comparison> {
    let contract = VideoColorContract {
        chroma: ChromaSubsampling::Yuv444,
        ..AUTO
    };
    let picture = Picture::synthetic(contract, 256, 64, 7);
    let uniform = VideoUniform::from_contract(contract, (256, 64), (256, 64));
    let (_, bytes) = render(
        gpu,
        converter,
        &picture,
        DXGI_FORMAT_AYUV,
        ConvertUniform::new(uniform, OutputKind::Codes),
        DXGI_FORMAT_B8G8R8A8_UNORM,
        None,
    )?;
    let transform = arcen_media::video::ColorTransform::new(
        ColorMatrix::Bt709,
        ColorRange::Limited,
        BitDepth::Eight,
    );
    let mut result = Comparison {
        name: "auto8-ayuv-vs-ColorTransform::to_bgr8".into(),
        ..Default::default()
    };
    for (index, texel) in bytes.chunks_exact(4).enumerate() {
        let y = i32::from(picture.luma[index]);
        let [cb, cr] = picture.chroma[index];
        let (b, g, r) = transform.to_bgr8(y, i32::from(cb), i32::from(cr));
        let diff = [
            texel[0].abs_diff(b),
            texel[1].abs_diff(g),
            texel[2].abs_diff(r),
        ]
        .into_iter()
        .max()
        .unwrap();
        result.pixels += 1;
        match diff {
            0 => result.exact += 1,
            1 => result.off_by_one += 1,
            _ => {}
        }
        result.worst = result.worst.max(f32::from(diff));
    }
    Ok(result)
}

/// A full-range 10-bit grey ramp through Y410 must come out as the same
/// 1024 codes: ten bits survive the shader and the R10G10B10A2 target.
fn grey_ramp_is_identity(gpu: &Gpu, converter: &Converter) -> Result<Comparison> {
    let contract = GRADING;
    let picture = Picture {
        width: 1024,
        height: 4,
        chroma_width: 1024,
        chroma_height: 4,
        luma: (0..4).flat_map(|_| 0..1024u16).collect(),
        chroma: vec![[512, 512]; 4096],
    };
    let uniform = VideoUniform::from_contract(contract, (1024, 4), (1024, 4));
    let (_, bytes) = render(
        gpu,
        converter,
        &picture,
        DXGI_FORMAT_Y410,
        ConvertUniform::new(uniform, OutputKind::Codes),
        DXGI_FORMAT_R10G10B10A2_UNORM,
        None,
    )?;
    let mut result = Comparison {
        name: "grading-grey-ramp-1024-codes".into(),
        ..Default::default()
    };
    let mut distinct = std::collections::BTreeSet::new();
    for (index, word) in words(&bytes).enumerate() {
        let expected = (index % 1024) as u16;
        let got = rgb10(word);
        distinct.insert(got[1]);
        let diff = got.iter().map(|c| c.abs_diff(expected)).max().unwrap();
        result.pixels += 1;
        if diff == 0 {
            result.exact += 1;
        }
        result.worst = result.worst.max(f32::from(diff));
    }
    result.name = format!("{} distinct={}", result.name, distinct.len());
    Ok(result)
}

/// HDR's FP16 scRGB fallback against the PQ → scRGB reference (relative
/// error, half precision).
fn pq_to_scrgb(gpu: &Gpu, converter: &Converter) -> Result<Comparison> {
    let contract = HDR;
    let picture = Picture::synthetic(contract, 128, 32, 11);
    let uniform = VideoUniform::from_contract(contract, (128, 32), (128, 32));
    let (_, bytes) = render(
        gpu,
        converter,
        &picture,
        DXGI_FORMAT_Y410,
        ConvertUniform::new(uniform, OutputKind::PqToScrgb),
        DXGI_FORMAT_R16G16B16A16_FLOAT,
        None,
    )?;
    // Error relative to the pixel's largest scRGB component (floor 1 nit):
    // FP16 keeps ~11 significant bits, so a dark channel next to a bright,
    // out-of-BT.709-gamut one carries the bright one's absolute precision.
    let mut result = Comparison {
        name: "hdr-pq-to-scrgb-fp16 (relative to pixel peak)".into(),
        ..Default::default()
    };
    for (index, texel) in bytes.chunks_exact(8).enumerate() {
        let (x, y) = (index as u32 % 128, index as u32 / 128);
        let expected = pq_bt2020_to_scrgb(picture.reference(&uniform, x, y));
        let got: [f32; 3] =
            std::array::from_fn(|c| half(u16::from_le_bytes([texel[c * 2], texel[c * 2 + 1]])));
        let peak = expected.iter().fold(1.0f32 / 80.0, |m, v| m.max(v.abs()));
        let worst = (0..3)
            .map(|c| (got[c] - expected[c]).abs() / peak)
            .fold(0.0f32, f32::max);
        result.pixels += 1;
        if worst < 1.0e-3 {
            result.exact += 1;
        }
        result.worst = result.worst.max(worst);
    }
    Ok(result)
}

/// Exact still image: refined blocks come out as the tile's own codes (RGB
/// domain) or pass the video's matrix (Y′CbCr), unrefined pixels as the
/// video; level-2 and level-3 readbacks return exactly the codes.
fn refine_composite(gpu: &Gpu, converter: &Converter, domain: RefineDomain) -> Result<Comparison> {
    use arcen_media::refine::deck::{OverlayDelta, VerificationLevel};
    use arcen_media::refine::hub::ReadbackRequest;
    use arcen_media::refine::tile::RefineGrid;
    let contract = GRADING;
    let grid = RefineGrid {
        width: 72,
        height: 40,
    };
    let picture = Picture::synthetic(contract, grid.width, grid.height, 23);
    let mut rng = Rng(99);
    let refined: Vec<u32> = (0..grid.block_count())
        .filter(|index| index % 3 != 1)
        .collect();
    let mut sets = Vec::new();
    for &index in &refined {
        let rect = grid.block_rect(index).unwrap();
        let planar: Vec<u16> = (0..rect.pixels() * 3)
            .map(|_| (rng.next_u32() % 1024) as u16)
            .collect();
        sets.push((rect, planar));
    }
    let mut work = OverlayWork::default();
    work.apply_overlay(
        grid,
        &OverlayDelta {
            clear_all: true,
            clear: Vec::new(),
            set: sets.clone(),
        },
    );
    let mut overlay = D3d11RefineOverlay::new();
    overlay
        .encode_updates(gpu, &work, grid)
        .ok_or("overlay upload refused")?;
    // The next frame clears one block again: it must read back invalid.
    let cleared = grid.block_rect(refined[0]).unwrap();
    let mut next = OverlayWork::default();
    next.apply_overlay(
        grid,
        &OverlayDelta {
            clear_all: false,
            clear: vec![cleared],
            set: Vec::new(),
        },
    );
    let view = overlay
        .encode_updates(gpu, &next, grid)
        .ok_or("overlay update refused")?;
    let uniform = VideoUniform::from_contract(
        contract,
        (grid.width, grid.height),
        (grid.width, grid.height),
    );
    let (target, bytes) = render(
        gpu,
        converter,
        &picture,
        DXGI_FORMAT_Y410,
        ConvertUniform::new(uniform, OutputKind::Codes),
        DXGI_FORMAT_R10G10B10A2_UNORM,
        Some((&view, domain)),
    )?;
    let mut expected_codes = vec![None; (grid.width * grid.height) as usize];
    for (rect, planar) in &sets {
        if rect == &cleared {
            continue;
        }
        for line in 0..rect.height {
            for column in 0..rect.width {
                let pixel = (line * rect.width + column) as usize;
                let codes = [
                    planar[pixel],
                    planar[rect.pixels() + pixel],
                    planar[2 * rect.pixels() + pixel],
                ];
                expected_codes[((rect.y + line) * grid.width + rect.x + column) as usize] =
                    Some(codes);
            }
        }
    }
    let name = match domain {
        RefineDomain::Rgb => "refine-rgb-composite-bit-exact",
        RefineDomain::YCbCr => "refine-ycbcr-composite-matrix",
    };
    let mut result = Comparison {
        name: name.into(),
        ..Default::default()
    };
    for (index, word) in words(&bytes).enumerate() {
        let (x, y) = (index as u32 % grid.width, index as u32 / grid.width);
        let got = rgb10(word);
        let expected = match (expected_codes[index], domain) {
            (Some(codes), RefineDomain::Rgb) => codes,
            (Some([y_code, cb, cr]), RefineDomain::YCbCr) => uniform
                .reference_rgb(f32::from(y_code), f32::from(cb), f32::from(cr))
                .map(|c| (c * 1023.0).round() as u16),
            (None, _) => picture
                .reference(&uniform, x, y)
                .map(|c| (c * 1023.0).round() as u16),
        };
        let diff = (0..3).map(|c| got[c].abs_diff(expected[c])).max().unwrap();
        result.pixels += 1;
        match diff {
            0 => result.exact += 1,
            1 => result.off_by_one += 1,
            _ => {}
        }
        // Refined RGB pixels must be exact; everything else within a code.
        let limit = u16::from(!(expected_codes[index].is_some() && domain == RefineDomain::Rgb));
        if diff > limit {
            result.worst = result.worst.max(f32::from(diff) + 100.0);
        } else {
            result.worst = result.worst.max(f32::from(diff));
        }
    }
    // Level 2 (overlay) and level 3 (composite) readbacks.
    let blocks: Vec<_> = sets
        .iter()
        .map(|(rect, _)| *rect)
        .filter(|rect| rect != &cleared)
        .collect();
    for (level, source) in [
        (
            VerificationLevel::GpuOverlay,
            overlay.texture().unwrap().clone(),
        ),
        (VerificationLevel::RenderedRgb, target),
    ] {
        if level == VerificationLevel::RenderedRgb && domain == RefineDomain::YCbCr {
            continue;
        }
        let request = ReadbackRequest {
            monitor_id: 1,
            level,
            fence: 1,
            blocks: blocks.clone(),
            depth: 10,
        };
        let pending = D3d11RefineOverlay::encode_readback(gpu, &source, request)?;
        gpu.flush_and_wait()?;
        let readback = pending.complete(gpu)?;
        for (rect, planar) in readback.request.blocks.iter().zip(&readback.blocks) {
            let original = &sets.iter().find(|(r, _)| r == rect).unwrap().1;
            if planar != original {
                return Err(format!("{name}: {level:?} readback differs at {rect:?}"));
            }
        }
        result.name = format!("{} {level:?}-readback-exact", result.name);
    }
    // The cleared block reads back invalid (alpha 0) in the overlay.
    let raw = gpu.read_region(
        overlay.texture().unwrap(),
        cleared.x,
        cleared.y,
        cleared.width,
        cleared.height,
        4,
    )?;
    if words(&raw).any(|word| word >> 30 != 0) {
        return Err(format!("{name}: a cleared block is still valid"));
    }
    Ok(result)
}

/// A decoder-like picture: slice 1 of a two-slice Y410 array, coded 48×40,
/// with a visible rectangle. In place (bindable, origin crop) and copied
/// out (not bindable, offset crop) must both show exactly the visible
/// pixels, converted as the reference does.
fn decoder_picture_crop(gpu: &Gpu, in_place: bool) -> Result<Comparison> {
    use super::compose::{DecodedFrame, FrameComposer};
    use super::picture::D3d11Picture;
    let (coded_w, coded_h) = (48_u32, 40_u32);
    let visible = if in_place {
        (0, 0, 40, 36)
    } else {
        (8, 4, 32, 30)
    };
    let pictures = [
        Picture::synthetic(GRADING, coded_w, coded_h, 3),
        Picture::synthetic(GRADING, coded_w, coded_h, 9),
    ];
    let mut slices: Vec<Vec<u8>> = Vec::new();
    for picture in &pictures {
        let mut bytes = Vec::with_capacity((coded_w * coded_h * 4) as usize);
        for (index, &y) in picture.luma.iter().enumerate() {
            let [cb, cr] = picture.chroma[index];
            let word = u32::from(cb) | u32::from(y) << 10 | u32::from(cr) << 20 | 3 << 30;
            bytes.extend_from_slice(&word.to_le_bytes());
        }
        slices.push(bytes);
    }
    let desc = D3D11_TEXTURE2D_DESC {
        Width: coded_w,
        Height: coded_h,
        MipLevels: 1,
        ArraySize: 2,
        Format: DXGI_FORMAT_Y410,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let data: Vec<D3D11_SUBRESOURCE_DATA> = slices
        .iter()
        .map(|bytes| D3D11_SUBRESOURCE_DATA {
            pSysMem: bytes.as_ptr().cast(),
            SysMemPitch: coded_w * 4,
            SysMemSlicePitch: 0,
        })
        .collect();
    let mut texture = None;
    super::hr("CreateTexture2D(array)", unsafe {
        gpu.device
            .CreateTexture2D(&desc, Some(data.as_ptr()), Some(&mut texture))
    })?;
    let texture = super::created("CreateTexture2D(array)", texture)?;
    let mut picture = D3d11Picture::texture(texture);
    picture.array_slice = 1;
    picture.visible = visible;
    picture.shader_resource = in_place;
    let (width, height) = (visible.2, visible.3);
    let target = gpu.texture(
        width,
        height,
        DXGI_FORMAT_R10G10B10A2_UNORM,
        D3D11_BIND_RENDER_TARGET,
        None,
    )?;
    let rtv = gpu.rtv(&target)?;
    let mut composer = FrameComposer::new(gpu)?;
    let frame = DecodedFrame {
        picture,
        contract: GRADING,
        refine: None,
    };
    composer.draw(gpu, &frame, &rtv, OutputKind::Codes)?;
    let bytes = gpu.read_region(&target, 0, 0, width, height, 4)?;
    let source = &pictures[1];
    let uniform = VideoUniform::from_contract(GRADING, (coded_w, coded_h), (coded_w, coded_h));
    let name = if in_place {
        "decoder-slice-crop-in-place"
    } else {
        "decoder-slice-crop-copied-out"
    };
    let mut result = Comparison {
        name: name.into(),
        ..Default::default()
    };
    for (index, word) in words(&bytes).enumerate() {
        let (x, y) = (
            index as u32 % width + visible.0,
            index as u32 / width + visible.1,
        );
        let i = (y * coded_w + x) as usize;
        let [cb, cr] = source.chroma[i];
        let expected = uniform
            .reference_rgb(f32::from(source.luma[i]), f32::from(cb), f32::from(cr))
            .map(|c| (c * 1023.0).round() as u16);
        let got = rgb10(word);
        let diff = (0..3)
            .map(|c| got[c].abs_diff(expected[c]))
            .max()
            .unwrap_or(0);
        result.pixels += 1;
        match diff {
            0 => result.exact += 1,
            1 => result.off_by_one += 1,
            _ => {}
        }
        result.worst = result.worst.max(f32::from(diff));
    }
    Ok(result)
}

pub fn run(gpu: &Gpu) -> Result<Vec<(Comparison, f32)>> {
    let converter = Converter::new(gpu)?;
    let mut results = Vec::new();
    let p420_8 = AUTO;
    let p420_10 = VideoColorContract {
        depth: BitDepth::Ten,
        matrix: ColorMatrix::Bt2020Ncl,
        ..AUTO
    };
    let ayuv = VideoColorContract {
        chroma: ChromaSubsampling::Yuv444,
        ..AUTO
    };
    let identity = VideoColorContract {
        matrix: ColorMatrix::Identity,
        ..GRADING
    };
    let cases: [(&str, VideoColorContract, DXGI_FORMAT); 6] = [
        ("auto8-nv12-420-chroma-fit", p420_8, DXGI_FORMAT_NV12),
        ("main10-p010-420-bt2020-limited", p420_10, DXGI_FORMAT_P010),
        ("auto8-ayuv-444", ayuv, DXGI_FORMAT_AYUV),
        ("grading-y410-444-bt709-full", GRADING, DXGI_FORMAT_Y410),
        ("hdr-y410-444-bt2020-full-pq-codes", HDR, DXGI_FORMAT_Y410),
        ("identity-gbr-y410", identity, DXGI_FORMAT_Y410),
    ];
    for (name, contract, format) in cases {
        let picture = Picture::synthetic(contract, 192, 96, 5);
        match compare_ten_bit(gpu, &converter, name, contract, format, &picture) {
            Ok(result) => results.push((result, 1.0)),
            Err(error) => results.push((
                Comparison {
                    name: format!("{name} ERROR {error}"),
                    worst: f32::MAX,
                    ..Default::default()
                },
                1.0,
            )),
        }
    }
    let mut push = |result: Result<Comparison>, name: &str, tolerance: f32| match result {
        Ok(result) => results.push((result, tolerance)),
        Err(error) => results.push((
            Comparison {
                name: format!("{name} ERROR {error}"),
                worst: f32::MAX,
                ..Default::default()
            },
            tolerance,
        )),
    };
    push(
        compare_auto_with_shared_transform(gpu, &converter),
        "auto8-vs-shared",
        1.0,
    );
    push(grey_ramp_is_identity(gpu, &converter), "grey-ramp", 0.0);
    push(pq_to_scrgb(gpu, &converter), "pq-scrgb", 4.0e-3);
    push(
        refine_composite(gpu, &converter, RefineDomain::Rgb),
        "refine-rgb",
        1.0,
    );
    push(
        refine_composite(gpu, &converter, RefineDomain::YCbCr),
        "refine-ycbcr",
        1.0,
    );
    push(decoder_picture_crop(gpu, true), "crop-in-place", 1.0);
    push(decoder_picture_crop(gpu, false), "crop-copied-out", 1.0);
    Ok(results)
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "a failed step is the test's failure")]
mod tests {
    use super::*;
    use crate::present::device::AdapterChoice;

    /// Every conversion and refinement check on WARP (no GPU needed).
    #[test]
    fn conversion_and_refinement_match_the_shared_reference_on_warp() {
        let gpu = Gpu::create(&AdapterChoice::Warp).expect("WARP device");
        let results = run(&gpu).expect("self-test");
        let failed: Vec<String> = results
            .iter()
            .filter(|(result, tolerance)| !result.passed(*tolerance))
            .map(|(result, tolerance)| result.line(*tolerance))
            .collect();
        assert!(failed.is_empty(), "{failed:#?}");
    }

    /// The same checks on every hardware adapter of the machine (NVIDIA's
    /// float-to-UNORM rounding is the ±1 code the tolerances allow). Needs a
    /// GPU, so it is opt-in:
    /// `cargo test -p arcen-deck-windows -- --ignored --nocapture on_every_hardware_adapter`.
    #[test]
    #[ignore = "needs a hardware D3D11 adapter"]
    fn conversion_and_refinement_match_the_shared_reference_on_every_hardware_adapter() {
        let hardware: Vec<_> = crate::present::device::adapters()
            .expect("adapters")
            .into_iter()
            .filter(|(_, facts)| !facts.software)
            .collect();
        assert!(!hardware.is_empty(), "no hardware adapter");
        let mut failed = Vec::new();
        for (_, facts) in hardware {
            let gpu = Gpu::create(&AdapterChoice::Name(facts.name.clone())).expect("device");
            let results = run(&gpu).expect("self-test");
            let passed = results.iter().filter(|(r, t)| r.passed(*t)).count();
            println!("{}: {passed}/{}", facts.name, results.len());
            for (result, tolerance) in &results {
                println!("  {}", result.line(*tolerance));
                if !result.passed(*tolerance) {
                    failed.push(format!("{}: {}", facts.name, result.line(*tolerance)));
                }
            }
        }
        assert!(failed.is_empty(), "{failed:#?}");
    }
}
