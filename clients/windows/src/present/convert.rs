// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The conversion pass: decoded planes → the swapchain's (or a test
//! target's) R′G′B′, one draw at the source size.

use super::device::Gpu;
use super::{Result, created, hr};
use arcen_media::video::present::{VIDEO_UNIFORM_BYTES, VideoUniform};
use windows::Win32::Graphics::Direct3D::D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;

macro_rules! blob {
    ($name:literal) => {
        include_bytes!(concat!(env!("OUT_DIR"), "/present/", $name, ".cso"))
    };
}

/// How the pass writes its result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum OutputKind {
    /// The stream's own (gamma or PQ) codes; the swapchain carries the tag.
    Codes = 0,
    /// FP16 scRGB fallback: PQ BT.2020 → linear BT.709, 1.0 = 80 nits.
    PqToScrgb = 1,
}

/// The cbuffer: the shared 48-byte [`VideoUniform`] and 16 Windows-only
/// bytes (P010's code shift, the output kind, the scRGB scale).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ConvertUniform {
    pub shared: VideoUniform,
    pub code_shift: u32,
    pub output: OutputKind,
    pub scrgb_nits_per_unit: f32,
}

/// Bytes of [`ConvertUniform::to_bytes`].
pub const CONVERT_UNIFORM_BYTES: usize = VIDEO_UNIFORM_BYTES + 16;

impl ConvertUniform {
    #[must_use]
    pub fn new(shared: VideoUniform, output: OutputKind) -> Self {
        Self {
            shared,
            code_shift: 0,
            output,
            scrgb_nits_per_unit: 80.0,
        }
    }

    #[must_use]
    pub fn to_bytes(self) -> [u8; CONVERT_UNIFORM_BYTES] {
        let mut bytes = [0_u8; CONVERT_UNIFORM_BYTES];
        bytes[..VIDEO_UNIFORM_BYTES].copy_from_slice(&self.shared.to_bytes());
        let tail = [
            self.code_shift,
            self.output as u32,
            self.scrgb_nits_per_unit.to_bits(),
            0,
        ];
        for (chunk, word) in bytes[VIDEO_UNIFORM_BYTES..].chunks_exact_mut(4).zip(tail) {
            chunk.copy_from_slice(&word.to_le_bytes());
        }
        bytes
    }
}

/// How the decoded picture is laid out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaneLayout {
    /// NV12 / P010 (and their 4:2:2/4:4:4 biplanar cousins).
    Biplanar,
    /// Y410: R10G10B10A2 with R=Cb, G=Y, B=Cr.
    Y410,
    /// AYUV: R8G8B8A8 with R=Cr, G=Cb, B=Y.
    Ayuv,
}

/// The refinement domain the overlay holds (ADR 0016).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefineDomain {
    Rgb,
    YCbCr,
}

/// Bindable views of one decoded picture.
pub struct SourcePlanes {
    pub layout: PlaneLayout,
    pub views: Vec<ID3D11ShaderResourceView>,
    pub code_shift: u32,
    pub luma_size: (u32, u32),
    pub chroma_size: (u32, u32),
}

impl SourcePlanes {
    /// Views of the visible picture of a decoded frame. In place when it
    /// can be sampled there; otherwise the visible rectangle is copied into
    /// `scratch` (a shader-bindable texture this presenter owns) first.
    pub fn of_picture(
        gpu: &Gpu,
        picture: &super::picture::D3d11Picture,
        scratch: &mut Option<ID3D11Texture2D>,
    ) -> Result<Self> {
        let (width, height) = picture.size();
        if width == 0 || height == 0 {
            return Err("empty visible picture".into());
        }
        let mut planes = if picture.samples_in_place() {
            Self::of_texture(gpu, &picture.texture, picture.array_slice)?
        } else {
            // Planar 4:2:0 copies must be even-aligned.
            let even = |v: u32| {
                if matches!(picture.format, DXGI_FORMAT_NV12 | DXGI_FORMAT_P010) {
                    v + (v & 1)
                } else {
                    v
                }
            };
            let (cw, ch) = (even(width), even(height));
            let reuse = scratch.as_ref().is_some_and(|texture| {
                let mut desc = D3D11_TEXTURE2D_DESC::default();
                unsafe { texture.GetDesc(&mut desc) };
                desc.Format == picture.format && (desc.Width, desc.Height) == (cw, ch)
            });
            if !reuse {
                *scratch =
                    Some(gpu.texture(cw, ch, picture.format, D3D11_BIND_SHADER_RESOURCE, None)?);
            }
            let target = scratch.as_ref().ok_or("no scratch texture")?;
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            unsafe { picture.texture.GetDesc(&mut desc) };
            let (left, top) = (picture.visible.0, picture.visible.1);
            let region = D3D11_BOX {
                left,
                top,
                front: 0,
                right: (left + cw).min(desc.Width),
                bottom: (top + ch).min(desc.Height),
                back: 1,
            };
            let subresource = picture.array_slice * desc.MipLevels.max(1);
            unsafe {
                gpu.context.CopySubresourceRegion(
                    target,
                    0,
                    0,
                    0,
                    0,
                    &picture.texture,
                    subresource,
                    Some(&region),
                );
            }
            Self::of_texture(gpu, target, 0)?
        };
        // Only the visible picture is read (a bottom/right crop).
        let (sx, sy) = (
            planes.luma_size.0 / planes.chroma_size.0.max(1),
            planes.luma_size.1 / planes.chroma_size.1.max(1),
        );
        planes.luma_size = (width, height);
        planes.chroma_size = (width.div_ceil(sx.max(1)), height.div_ceil(sy.max(1)));
        Ok(planes)
    }

    /// Views of a decoded texture (or one slice of a decoder array).
    pub fn of_texture(gpu: &Gpu, texture: &ID3D11Texture2D, slice: u32) -> Result<Self> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&mut desc) };
        let (w, h) = (desc.Width, desc.Height);
        Ok(match desc.Format {
            DXGI_FORMAT_NV12 => Self {
                layout: PlaneLayout::Biplanar,
                views: vec![
                    gpu.srv_slice(texture, DXGI_FORMAT_R8_UINT, slice)?,
                    gpu.srv_slice(texture, DXGI_FORMAT_R8G8_UINT, slice)?,
                ],
                code_shift: 0,
                luma_size: (w, h),
                chroma_size: (w / 2, h / 2),
            },
            DXGI_FORMAT_P010 => Self {
                layout: PlaneLayout::Biplanar,
                views: vec![
                    gpu.srv_slice(texture, DXGI_FORMAT_R16_UINT, slice)?,
                    gpu.srv_slice(texture, DXGI_FORMAT_R16G16_UINT, slice)?,
                ],
                code_shift: 6,
                luma_size: (w, h),
                chroma_size: (w / 2, h / 2),
            },
            DXGI_FORMAT_Y410 => Self {
                layout: PlaneLayout::Y410,
                views: vec![gpu.srv_slice(texture, DXGI_FORMAT_R10G10B10A2_UINT, slice)?],
                code_shift: 0,
                luma_size: (w, h),
                chroma_size: (w, h),
            },
            DXGI_FORMAT_AYUV => Self {
                layout: PlaneLayout::Ayuv,
                views: vec![gpu.srv_slice(texture, DXGI_FORMAT_R8G8B8A8_UINT, slice)?],
                code_shift: 0,
                luma_size: (w, h),
                chroma_size: (w, h),
            },
            other => return Err(format!("unsupported decoded format {other:?}")),
        })
    }
}

pub struct Converter {
    vs: ID3D11VertexShader,
    biplanar: ID3D11PixelShader,
    y410: ID3D11PixelShader,
    ayuv: ID3D11PixelShader,
    biplanar_rgb: ID3D11PixelShader,
    biplanar_ycbcr: ID3D11PixelShader,
    y410_rgb: ID3D11PixelShader,
    y410_ycbcr: ID3D11PixelShader,
    uniform: ID3D11Buffer,
    rasterizer: ID3D11RasterizerState,
}

fn pixel_shader(gpu: &Gpu, name: &str, bytes: &[u8]) -> Result<ID3D11PixelShader> {
    if bytes.is_empty() {
        return Err(format!("shader {name} was not compiled into this build"));
    }
    let mut shader = None;
    hr(&format!("CreatePixelShader({name})"), unsafe {
        gpu.device.CreatePixelShader(bytes, None, Some(&mut shader))
    })?;
    created(name, shader)
}

impl Converter {
    pub fn new(gpu: &Gpu) -> Result<Self> {
        let vs_bytes: &[u8] = blob!("vs_main");
        if cfg!(arcen_shaders_missing) || vs_bytes.is_empty() {
            return Err("the presenter shaders were not compiled into this build (no fxc.exe at build time)".into());
        }
        let mut vs = None;
        hr("CreateVertexShader", unsafe {
            gpu.device.CreateVertexShader(vs_bytes, None, Some(&mut vs))
        })?;
        let desc = D3D11_BUFFER_DESC {
            ByteWidth: CONVERT_UNIFORM_BYTES as u32,
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
            StructureByteStride: 0,
        };
        let mut uniform = None;
        hr("CreateBuffer(uniform)", unsafe {
            gpu.device.CreateBuffer(&desc, None, Some(&mut uniform))
        })?;
        let raster = D3D11_RASTERIZER_DESC {
            FillMode: D3D11_FILL_SOLID,
            CullMode: D3D11_CULL_NONE,
            DepthClipEnable: true.into(),
            ..Default::default()
        };
        let mut rasterizer = None;
        hr("CreateRasterizerState", unsafe {
            gpu.device
                .CreateRasterizerState(&raster, Some(&mut rasterizer))
        })?;
        Ok(Self {
            vs: created("CreateVertexShader", vs)?,
            biplanar: pixel_shader(gpu, "ps_biplanar", blob!("ps_biplanar"))?,
            y410: pixel_shader(gpu, "ps_y410", blob!("ps_y410"))?,
            ayuv: pixel_shader(gpu, "ps_ayuv", blob!("ps_ayuv"))?,
            biplanar_rgb: pixel_shader(
                gpu,
                "ps_biplanar_refined_rgb",
                blob!("ps_biplanar_refined_rgb"),
            )?,
            biplanar_ycbcr: pixel_shader(
                gpu,
                "ps_biplanar_refined_ycbcr",
                blob!("ps_biplanar_refined_ycbcr"),
            )?,
            y410_rgb: pixel_shader(gpu, "ps_y410_refined_rgb", blob!("ps_y410_refined_rgb"))?,
            y410_ycbcr: pixel_shader(gpu, "ps_y410_refined_ycbcr", blob!("ps_y410_refined_ycbcr"))?,
            uniform: created("CreateBuffer(uniform)", uniform)?,
            rasterizer: created("CreateRasterizerState", rasterizer)?,
        })
    }

    fn pick(
        &self,
        layout: PlaneLayout,
        refined: Option<RefineDomain>,
    ) -> Result<&ID3D11PixelShader> {
        Ok(match (layout, refined) {
            (PlaneLayout::Biplanar, None) => &self.biplanar,
            (PlaneLayout::Y410, None) => &self.y410,
            (PlaneLayout::Ayuv, None) => &self.ayuv,
            (PlaneLayout::Biplanar, Some(RefineDomain::Rgb)) => &self.biplanar_rgb,
            (PlaneLayout::Biplanar, Some(RefineDomain::YCbCr)) => &self.biplanar_ycbcr,
            (PlaneLayout::Y410, Some(RefineDomain::Rgb)) => &self.y410_rgb,
            (PlaneLayout::Y410, Some(RefineDomain::YCbCr)) => &self.y410_ycbcr,
            // Exact still image runs on 10-bit 4:4:4 only (ADR 0016 R1).
            (PlaneLayout::Ayuv, Some(_)) => return Err("no refined pass for 8-bit AYUV".into()),
        })
    }

    /// Draws `source` into `target` (a render target of the source size)
    /// with the overlay composited when `refined` is set.
    pub fn draw(
        &self,
        gpu: &Gpu,
        source: &SourcePlanes,
        uniform: ConvertUniform,
        target: &ID3D11RenderTargetView,
        overlay: Option<(&ID3D11ShaderResourceView, RefineDomain)>,
    ) -> Result<()> {
        let shader = self.pick(source.layout, overlay.map(|(_, domain)| domain))?;
        let bytes = ConvertUniform {
            code_shift: source.code_shift,
            ..uniform
        }
        .to_bytes();
        let context = &gpu.context;
        unsafe {
            context.UpdateSubresource(&self.uniform, 0, None, bytes.as_ptr().cast(), 0, 0);
            context.OMSetRenderTargets(Some(&[Some(target.clone())]), None);
            context.RSSetViewports(Some(&[D3D11_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: source.luma_size.0 as f32,
                Height: source.luma_size.1 as f32,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            }]));
            context.RSSetState(&self.rasterizer);
            context.IASetInputLayout(None);
            context.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            context.VSSetShader(&self.vs, None);
            context.PSSetShader(shader, None);
            context.PSSetConstantBuffers(0, Some(&[Some(self.uniform.clone())]));
            let mut views: [Option<ID3D11ShaderResourceView>; 4] = [None, None, None, None];
            match source.layout {
                PlaneLayout::Biplanar => {
                    views[0] = Some(source.views[0].clone());
                    views[1] = Some(source.views[1].clone());
                }
                PlaneLayout::Y410 | PlaneLayout::Ayuv => views[3] = Some(source.views[0].clone()),
            }
            if let Some((overlay, _)) = overlay {
                views[2] = Some(overlay.clone());
            }
            context.PSSetShaderResources(0, Some(&views));
            context.Draw(3, 0);
            // Unbind, so the planes can go back to the decoder and the
            // overlay can be written before the next draw.
            context.PSSetShaderResources(0, Some(&[None, None, None, None]));
            context.OMSetRenderTargets(None, None);
        }
        Ok(())
    }
}

super::opaque_debug!(SourcePlanes, Converter);
