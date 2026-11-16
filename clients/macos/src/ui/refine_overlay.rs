// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Exact still image on the ten-bit Metal presenter: the Deck's
//! [`RefineCompositor`] adapter (ADR 0016 R1, R2, R4).
//!
//! Every decision — which blocks are valid at the fence being presented,
//! when a digest verifies, which level a readback raises, the overlay's
//! texel packing and bounds guard — is `arcen_media::refine`'s (the
//! compositor's portable half is `arcen_media::refine::compositor`, shared
//! with the Windows Deck). This module only turns the shared overlay
//! changes into Metal work **inside the presenter's own command buffer**,
//! before the conversion pass that samples them, and reads blocks back
//! after the GPU finished, for verification levels 2 and 3.

use core::ffi::c_void;
use core::ptr::NonNull;

pub(crate) use arcen_media::refine::compositor::{inside, needs_composite, OverlayWork};
use arcen_media::refine::compositor::{pack_block, unpack_block, OVERLAY_TEXEL_BYTES as TEXEL};
use arcen_media::refine::hub::{ReadbackRequest, ReadbackResult};
use arcen_media::refine::tile::{RefineGrid, TileRect};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_metal::{
    MTLBlitCommandEncoder, MTLBuffer, MTLClearColor, MTLCommandBuffer, MTLCommandEncoder,
    MTLDevice, MTLLoadAction, MTLOrigin, MTLPixelFormat, MTLRenderPassDescriptor,
    MTLRenderPipelineState, MTLResourceOptions, MTLSize, MTLStorageMode, MTLStoreAction,
    MTLTexture, MTLTextureDescriptor, MTLTextureUsage,
};

/// Bytes of one whole 16×16 block in the overlay.
const BLOCK_BYTES: usize = 16 * 16 * TEXEL;

/// The overlay texture and the refined conversion pipeline.
pub(crate) struct MetalRefineOverlay {
    pub(crate) pipeline: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    texture: Option<(Retained<ProtocolObject<dyn MTLTexture>>, RefineGrid)>,
}

// SAFETY: used only under the presenter's external `Mutex`, like
// `DedicatedVideoLayer`, whose command buffers it encodes into.
unsafe impl Send for MetalRefineOverlay {}

fn shared_buffer(
    device: &ProtocolObject<dyn MTLDevice>,
    bytes: usize,
) -> Option<Retained<ProtocolObject<dyn MTLBuffer>>> {
    device.newBufferWithLength_options(bytes.max(TEXEL), MTLResourceOptions::StorageModeShared)
}

fn size(rect: TileRect) -> MTLSize {
    MTLSize {
        width: rect.width as usize,
        height: rect.height as usize,
        depth: 1,
    }
}

fn origin(rect: TileRect) -> MTLOrigin {
    MTLOrigin {
        x: rect.x as usize,
        y: rect.y as usize,
        z: 0,
    }
}

/// A private texture of `format` at `grid`'s size.
pub(crate) fn private_texture(
    device: &ProtocolObject<dyn MTLDevice>,
    format: MTLPixelFormat,
    grid: RefineGrid,
    usage: MTLTextureUsage,
) -> Option<Retained<ProtocolObject<dyn MTLTexture>>> {
    // SAFETY: plain descriptor construction with a valid format and size.
    let descriptor = unsafe {
        MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
            format,
            grid.width as usize,
            grid.height as usize,
            false,
        )
    };
    descriptor.setUsage(usage);
    descriptor.setStorageMode(MTLStorageMode::Private);
    device.newTextureWithDescriptor(&descriptor)
}

impl MetalRefineOverlay {
    /// The refined pipeline for the presenter's `RGB10A2Unorm` drawable.
    pub(crate) fn new(pipeline: Retained<ProtocolObject<dyn MTLRenderPipelineState>>) -> Self {
        Self {
            pipeline,
            texture: None,
        }
    }

    /// Encodes `work`'s overlay writes into `command_buffer`, which must not
    /// have an open encoder; returns the overlay texture the conversion
    /// pass should sample, if one exists.
    pub(crate) fn encode_updates(
        &mut self,
        device: &ProtocolObject<dyn MTLDevice>,
        command_buffer: &ProtocolObject<dyn MTLCommandBuffer>,
        work: &OverlayWork,
        source: RefineGrid,
        keep_alive: &mut Vec<Retained<ProtocolObject<dyn MTLBuffer>>>,
    ) -> Option<Retained<ProtocolObject<dyn MTLTexture>>> {
        if let Some(grid) = work.reset {
            self.texture = None;
            // The overlay is exactly the decoded picture's size, never what
            // a barrier alone claimed.
            if grid.width > 0 && grid.height > 0 && grid == source {
                if let Some(texture) = private_texture(
                    device,
                    MTLPixelFormat::RGB10A2Uint,
                    grid,
                    MTLTextureUsage::ShaderRead | MTLTextureUsage::RenderTarget,
                ) {
                    // A cleared overlay: every texel invalid.
                    let pass = MTLRenderPassDescriptor::renderPassDescriptor();
                    // SAFETY: colour attachment 0 is valid on every pass.
                    let attachment = unsafe { pass.colorAttachments().objectAtIndexedSubscript(0) };
                    attachment.setTexture(Some(&*texture));
                    attachment.setLoadAction(MTLLoadAction::Clear);
                    attachment.setStoreAction(MTLStoreAction::Store);
                    attachment.setClearColor(MTLClearColor {
                        red: 0.0,
                        green: 0.0,
                        blue: 0.0,
                        alpha: 0.0,
                    });
                    if let Some(encoder) = command_buffer.renderCommandEncoderWithDescriptor(&pass)
                    {
                        encoder.endEncoding();
                        self.texture = Some((texture, grid));
                    }
                }
            }
        }
        let (texture, grid) = self.texture.as_ref()?;
        if *grid != source
            || !inside(&work.clears, *grid)
            || !inside(work.sets.iter().map(|(rect, _)| rect), *grid)
        {
            // Never blit outside the overlay: drop it and let the caller
            // start over.
            self.texture = None;
            return None;
        }
        if work.clears.is_empty() && work.sets.is_empty() {
            return Some(texture.clone());
        }
        // One staging buffer: a zero block for clears, then every set block.
        let set_bytes: usize = work
            .sets
            .iter()
            .map(|(rect, _)| rect.pixels() * TEXEL)
            .sum();
        let staging = shared_buffer(device, BLOCK_BYTES + set_bytes)?;
        let base = staging.contents().as_ptr().cast::<u8>();
        // SAFETY: the buffer is `BLOCK_BYTES + set_bytes` long, CPU-visible
        // (shared storage) and not yet used by any command buffer.
        unsafe {
            std::ptr::write_bytes(base, 0, BLOCK_BYTES);
        }
        let mut offset = BLOCK_BYTES;
        let mut placed = Vec::with_capacity(work.sets.len());
        for (rect, planar) in &work.sets {
            let packed = pack_block(planar, rect.pixels());
            if packed.len() != rect.pixels() * TEXEL {
                continue;
            }
            // SAFETY: `offset + packed.len()` stays within the buffer, sized
            // from the same rectangles above.
            unsafe {
                std::ptr::copy_nonoverlapping(packed.as_ptr(), base.add(offset), packed.len());
            }
            placed.push((*rect, offset));
            offset += packed.len();
        }
        let blit = command_buffer.blitCommandEncoder()?;
        for rect in &work.clears {
            // SAFETY: the zero block covers any 16×16 block; the rectangle
            // lies inside the overlay (it is one of the grid's blocks).
            unsafe {
                blit.copyFromBuffer_sourceOffset_sourceBytesPerRow_sourceBytesPerImage_sourceSize_toTexture_destinationSlice_destinationLevel_destinationOrigin(
                    &staging,
                    0,
                    rect.width as usize * TEXEL,
                    rect.pixels() * TEXEL,
                    size(*rect),
                    texture,
                    0,
                    0,
                    origin(*rect),
                );
            }
        }
        for (rect, at) in placed {
            // SAFETY: as above; `at` is where this block's texels were
            // written.
            unsafe {
                blit.copyFromBuffer_sourceOffset_sourceBytesPerRow_sourceBytesPerImage_sourceSize_toTexture_destinationSlice_destinationLevel_destinationOrigin(
                    &staging,
                    at,
                    rect.width as usize * TEXEL,
                    rect.pixels() * TEXEL,
                    size(rect),
                    texture,
                    0,
                    0,
                    origin(rect),
                );
            }
        }
        blit.endEncoding();
        keep_alive.push(staging);
        Some(texture.clone())
    }

    /// Encodes copies of `request.blocks` out of `source` (the overlay for
    /// level 2, an unscaled composite for level 3) into a CPU-visible
    /// buffer. Must run after the pass that wrote `source`.
    pub(crate) fn encode_readback(
        device: &ProtocolObject<dyn MTLDevice>,
        command_buffer: &ProtocolObject<dyn MTLCommandBuffer>,
        source: &ProtocolObject<dyn MTLTexture>,
        request: &ReadbackRequest,
    ) -> Option<Retained<ProtocolObject<dyn MTLBuffer>>> {
        let bytes: usize = request
            .blocks
            .iter()
            .map(|rect| rect.pixels() * TEXEL)
            .sum();
        let buffer = shared_buffer(device, bytes)?;
        let blit = command_buffer.blitCommandEncoder()?;
        let mut offset = 0;
        for rect in &request.blocks {
            // SAFETY: each block lies inside `source` (both are the grid's
            // size) and `offset` advances within the buffer sized above.
            unsafe {
                blit.copyFromTexture_sourceSlice_sourceLevel_sourceOrigin_sourceSize_toBuffer_destinationOffset_destinationBytesPerRow_destinationBytesPerImage(
                    source,
                    0,
                    0,
                    origin(*rect),
                    size(*rect),
                    &buffer,
                    offset,
                    rect.width as usize * TEXEL,
                    rect.pixels() * TEXEL,
                );
            }
            offset += rect.pixels() * TEXEL;
        }
        blit.endEncoding();
        Some(buffer)
    }
}

/// Reads a completed readback buffer back into planar codes per block.
#[must_use]
pub(crate) fn readback_result(
    request: ReadbackRequest,
    buffer: &ProtocolObject<dyn MTLBuffer>,
) -> ReadbackResult {
    let total: usize = request
        .blocks
        .iter()
        .map(|rect| rect.pixels() * TEXEL)
        .sum();
    let base: NonNull<c_void> = buffer.contents();
    let length = buffer.length().min(total);
    // SAFETY: the command buffer that wrote the buffer completed; the slice
    // covers only its CPU-visible, initialised length.
    let bytes = unsafe { std::slice::from_raw_parts(base.as_ptr().cast::<u8>(), length) };
    let mut blocks = Vec::with_capacity(request.blocks.len());
    let mut offset = 0;
    for rect in &request.blocks {
        let end = (offset + rect.pixels() * TEXEL).min(bytes.len());
        blocks.push(unpack_block(&bytes[offset..end], rect.pixels()));
        offset = end;
    }
    ReadbackResult { request, blocks }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcen_media::refine::deck::{OverlayDelta, VerificationLevel};
    use arcen_media::refine::hub::RefineCompositor;

    /// Level 2 basis: blocks staged through the presenter's own command
    /// buffer land in the overlay texture exactly, a cleared block reads
    /// back invalid, and the readback path returns the codes.
    #[test]
    fn staged_blocks_reach_the_overlay_exactly_and_read_back() {
        use objc2_metal::{MTLCommandQueue, MTLCreateSystemDefaultDevice};
        let Some(device) = MTLCreateSystemDefaultDevice() else {
            return;
        };
        let queue = device.newCommandQueue().expect("queue");
        let pipeline = crate::ui::video_metal_layer::refined_pipeline_for_tests(&device);
        let mut overlay = MetalRefineOverlay::new(pipeline);
        let grid = RefineGrid {
            width: 40,
            height: 20,
        };
        let first = grid.block_rect(0).unwrap();
        let edge = grid.block_rect(5).unwrap(); // 8×4, clipped at both edges
        let codes = |rect: TileRect, seed: u16| -> Vec<u16> {
            (0..rect.pixels() as u16 * 3)
                .map(|index| (index * 37 + seed) % 1024)
                .collect()
        };
        let mut work = OverlayWork::default();
        work.apply_overlay(
            grid,
            &OverlayDelta {
                clear_all: true,
                clear: Vec::new(),
                set: vec![(first, codes(first, 1)), (edge, codes(edge, 2))],
            },
        );
        let mut keep = Vec::new();
        let command_buffer = queue.commandBuffer().expect("command buffer");
        let texture = overlay
            .encode_updates(&device, &command_buffer, &work, grid, &mut keep)
            .expect("overlay texture");
        let request = ReadbackRequest {
            monitor_id: 1,
            level: VerificationLevel::GpuOverlay,
            fence: 1,
            blocks: vec![first, edge],
            depth: 10,
        };
        let buffer =
            MetalRefineOverlay::encode_readback(&device, &command_buffer, &texture, &request)
                .expect("readback");
        command_buffer.commit();
        command_buffer.waitUntilCompleted();
        let result = readback_result(request, &buffer);
        assert_eq!(result.blocks[0], codes(first, 1));
        assert_eq!(result.blocks[1], codes(edge, 2));
    }
}
