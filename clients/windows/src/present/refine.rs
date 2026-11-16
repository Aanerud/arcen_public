// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Exact still image on the D3D11 presenter: the Windows twin of the Mac's
//! `MetalRefineOverlay` (ADR 0016 R1, R2, R4).
//!
//! Every overlay write goes through the presenter's own immediate context
//! **before** the conversion draw that samples it (D3D11 executes one
//! context in order: the equivalent of "uploads inside the presenter's
//! command buffer"); readbacks are copies into staging, mapped only once an
//! event query says the GPU passed them.

use super::device::Gpu;
use super::{Result, created, hr};
use arcen_media::refine::compositor::{
    OVERLAY_TEXEL_BYTES as TEXEL, OverlayWork, inside, pack_block, unpack_block,
};
use arcen_media::refine::hub::{ReadbackRequest, ReadbackResult};
use arcen_media::refine::tile::RefineGrid;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;

/// Blocks per staging row (16384 / 16: D3D11's texture width limit).
const STAGING_BLOCKS_PER_ROW: u32 = 1024;
const BLOCK: u32 = 16;

pub struct D3d11RefineOverlay {
    texture: Option<(ID3D11Texture2D, ID3D11ShaderResourceView, RefineGrid)>,
}

/// A readback whose copies were issued; map it once the frame's event
/// query completed.
pub struct PendingReadback {
    pub request: ReadbackRequest,
    staging: ID3D11Texture2D,
}

impl Default for D3d11RefineOverlay {
    fn default() -> Self {
        Self::new()
    }
}

impl D3d11RefineOverlay {
    #[must_use]
    pub fn new() -> Self {
        Self { texture: None }
    }

    pub fn texture(&self) -> Option<&ID3D11Texture2D> {
        self.texture.as_ref().map(|(texture, ..)| texture)
    }

    /// Applies `work` to the overlay; returns the view the conversion pass
    /// should sample, if an overlay exists for `source`.
    pub fn encode_updates(
        &mut self,
        gpu: &Gpu,
        work: &OverlayWork,
        source: RefineGrid,
    ) -> Option<ID3D11ShaderResourceView> {
        if let Some(grid) = work.reset {
            self.texture = None;
            // Exactly the decoded picture's size, never what a barrier
            // alone claimed.
            if grid.width > 0 && grid.height > 0 && grid == source {
                let zeros = vec![0u8; grid.width as usize * grid.height as usize * TEXEL];
                let texture = gpu
                    .texture(
                        grid.width,
                        grid.height,
                        DXGI_FORMAT_R10G10B10A2_UINT,
                        D3D11_BIND_SHADER_RESOURCE,
                        Some((&zeros, grid.width * TEXEL as u32)),
                    )
                    .ok()?;
                let view = gpu.srv_2d(&texture, DXGI_FORMAT_R10G10B10A2_UINT).ok()?;
                self.texture = Some((texture, view, grid));
            }
        }
        let (texture, view, grid) = self.texture.as_ref()?;
        if *grid != source
            || !inside(&work.clears, *grid)
            || !inside(work.sets.iter().map(|(rect, _)| rect), *grid)
        {
            // Never write outside the overlay: drop it, start over.
            self.texture = None;
            return None;
        }
        let zero_block = [0u8; (BLOCK * BLOCK) as usize * TEXEL];
        for rect in &work.clears {
            let region = D3D11_BOX {
                left: rect.x,
                top: rect.y,
                front: 0,
                right: rect.x + rect.width,
                bottom: rect.y + rect.height,
                back: 1,
            };
            unsafe {
                gpu.context.UpdateSubresource(
                    texture,
                    0,
                    Some(&region),
                    zero_block.as_ptr().cast(),
                    rect.width * TEXEL as u32,
                    0,
                );
            }
        }
        for (rect, planar) in &work.sets {
            let packed = pack_block(planar, rect.pixels());
            if packed.len() != rect.pixels() * TEXEL {
                continue;
            }
            let region = D3D11_BOX {
                left: rect.x,
                top: rect.y,
                front: 0,
                right: rect.x + rect.width,
                bottom: rect.y + rect.height,
                back: 1,
            };
            unsafe {
                gpu.context.UpdateSubresource(
                    texture,
                    0,
                    Some(&region),
                    packed.as_ptr().cast(),
                    rect.width * TEXEL as u32,
                    0,
                );
            }
        }
        Some(view.clone())
    }

    /// Issues copies of `request.blocks` out of `source` (the overlay for
    /// level 2, an unscaled composite for level 3). Must follow the draw
    /// that wrote `source` on the same context.
    pub fn encode_readback(
        gpu: &Gpu,
        source: &ID3D11Texture2D,
        request: ReadbackRequest,
    ) -> Result<PendingReadback> {
        let count = request.blocks.len() as u32;
        if count == 0 {
            return Err("empty readback".into());
        }
        let columns = count.min(STAGING_BLOCKS_PER_ROW);
        let rows = count.div_ceil(STAGING_BLOCKS_PER_ROW);
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { source.GetDesc(&mut desc) };
        let staging_desc = D3D11_TEXTURE2D_DESC {
            Width: columns * BLOCK,
            Height: rows * BLOCK,
            MipLevels: 1,
            ArraySize: 1,
            Format: desc.Format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut staging = None;
        hr("CreateTexture2D(readback)", unsafe {
            gpu.device
                .CreateTexture2D(&staging_desc, None, Some(&mut staging))
        })?;
        let staging = created("CreateTexture2D(readback)", staging)?;
        for (index, rect) in request.blocks.iter().enumerate() {
            let index = index as u32;
            let region = D3D11_BOX {
                left: rect.x,
                top: rect.y,
                front: 0,
                right: rect.x + rect.width,
                bottom: rect.y + rect.height,
                back: 1,
            };
            unsafe {
                gpu.context.CopySubresourceRegion(
                    &staging,
                    0,
                    (index % STAGING_BLOCKS_PER_ROW) * BLOCK,
                    (index / STAGING_BLOCKS_PER_ROW) * BLOCK,
                    0,
                    source,
                    0,
                    Some(&region),
                );
            }
        }
        Ok(PendingReadback { request, staging })
    }
}

impl PendingReadback {
    /// Maps the staging copy (the GPU must have passed it) into planar
    /// codes per block.
    pub fn complete(self, gpu: &Gpu) -> Result<ReadbackResult> {
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        hr("Map(readback)", unsafe {
            gpu.context
                .Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
        })?;
        let mut blocks = Vec::with_capacity(self.request.blocks.len());
        for (index, rect) in self.request.blocks.iter().enumerate() {
            let index = index as u32;
            let x0 = (index % STAGING_BLOCKS_PER_ROW * BLOCK) as usize;
            let y0 = (index / STAGING_BLOCKS_PER_ROW * BLOCK) as usize;
            let mut bytes = Vec::with_capacity(rect.pixels() * TEXEL);
            for line in 0..rect.height as usize {
                let offset = (y0 + line) * mapped.RowPitch as usize + x0 * TEXEL;
                let row = unsafe {
                    std::slice::from_raw_parts(
                        (mapped.pData as *const u8).add(offset),
                        rect.width as usize * TEXEL,
                    )
                };
                bytes.extend_from_slice(row);
            }
            blocks.push(unpack_block(&bytes, rect.pixels()));
        }
        unsafe { gpu.context.Unmap(&self.staging, 0) };
        Ok(ReadbackResult {
            request: self.request,
            blocks,
        })
    }
}

super::opaque_debug!(D3d11RefineOverlay, PendingReadback);
