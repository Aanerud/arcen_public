// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The macOS Deck's canvas executor (ADR 0028 §10): one
//! persistent `BGRA8Unorm` Metal texture per monitor, written by the
//! [`CanvasWork`] the shared state owner (`arcen_media::canvas::CanvasState`)
//! validated, and presented by the eight-bit layer
//! (`video_metal_layer.rs`, [`super::video_metal_layer::EightBitLayerSource::Canvas`]).
//!
//! This is a pure executor: every decision (what to paint, where, from
//! which surface, what to cache) was made by the Pier's planner and checked
//! by the shared state owner. Each operation does exactly what the CPU
//! executor `arcen_media::canvas::CanvasPixels` does, which the tests below
//! prove on the shared corpus (`arcen_media::canvas::corpus`):
//!
//! - `Reset`: a new canvas of the size, cleared to opaque black (a render
//!   pass with a clear load action); the cache is emptied.
//! - `Fill`: a render pass, scissored to the rectangle, writing the colour
//!   (`code / 255.0`, which a unorm8 store keeps exactly).
//! - `Copy`: a blit within the canvas; overlapping rectangles go through a
//!   scratch texture, so the copy behaves as if through a temporary.
//! - `Tile`: the tile's codes are written as BGRA8 into one shared staging
//!   buffer per record and blitted into the canvas. Not `replaceRegion`:
//!   a CPU write into the canvas could race a GPU command still reading it
//!   (the `RefineFrameTag` rule, ADR 0016 R2); the blit is ordered on the
//!   GPU like every other write.
//! - `CacheStore`/`CachePaint`/`CacheEvict`: blits between the canvas and
//!   private buffers sub-allocated from tracked Metal heaps.
//! - `SurfacePaint`: the decoded `CVPixelBuffer`'s planes are imported
//!   without a copy and converted with the shared
//!   [`arcen_media::video::present::VideoUniform`] (`fs_canvas_surface`,
//!   which reuses `fs_convert`'s conversion), one scissored draw per listed
//!   block, each canvas pixel taking the picture's nearest sample exactly
//!   as the CPU executor scales.
//!
//! Every write of a record is encoded into one command buffer on the
//! canvas's own queue. The eight-bit layer encodes its present on the same
//! queue under [`CanvasTarget`]'s ordering lock, so a present sees exactly
//! the records committed before it, never part of one
//! ([`CanvasTarget::complete`]), and a later record's writes wait for it.

use core::ffi::c_void;
use core::ptr::NonNull;
use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use arcen_deck_core::present::{
    CanvasExecution, CanvasExecutorFacts, CanvasOpInput, CanvasReadback, PresenterFailure,
    PresenterFailureStage,
};
use arcen_media::canvas::{CanvasOp, CanvasWork};
use arcen_media::refine::tile::{TileCodes, TileRect};
use block2::RcBlock;
use objc2::rc::{autoreleasepool, Retained};
use objc2::runtime::ProtocolObject;
use objc2_metal::{
    MTLBlitCommandEncoder, MTLBuffer, MTLClearColor, MTLCommandBuffer, MTLCommandBufferStatus,
    MTLCommandEncoder, MTLCommandQueue, MTLCreateSystemDefaultDevice, MTLDevice,
    MTLHazardTrackingMode, MTLHeap, MTLHeapDescriptor, MTLLoadAction, MTLOrigin, MTLPixelFormat,
    MTLPrimitiveType, MTLRenderCommandEncoder, MTLRenderPassDescriptor, MTLRenderPipelineState,
    MTLResourceOptions, MTLScissorRect, MTLSize, MTLStorageMode, MTLStoreAction, MTLTexture,
    MTLTextureDescriptor, MTLTextureUsage,
};

use super::video_metal_layer::{
    build_pipeline_states, create_cv_plane_texture, plane_pixel_formats, MetalVideoUniform,
};
use super::video_render::VideoColorContract;

/// The largest canvas side this executor creates: Metal's two-dimensional
/// texture limit on every Mac GPU family the Deck runs on.
pub(crate) const MAX_CANVAS_PX: u32 = 16_384;

/// What the eight-bit presenter states as its canvas executor.
pub(crate) const EXECUTOR_FACTS: CanvasExecutorFacts = CanvasExecutorFacts {
    max_canvas_px: MAX_CANVAS_PX,
};

/// Size of each heap cache entries are allocated from (more heaps are
/// added as the cache grows; the state owner bounds the cache's bytes).
const CACHE_HEAP_BYTES: usize = 16 * 1024 * 1024;

const fn failure(stage: PresenterFailureStage, detail: &'static str) -> PresenterFailure {
    PresenterFailure { stage, detail }
}

const NO_CANVAS: PresenterFailure = failure(
    PresenterFailureStage::Submission,
    "canvas operation before its reset",
);
const BAD_INPUT: PresenterFailure = failure(
    PresenterFailureStage::Submission,
    "canvas input does not match its operation",
);
const OUTSIDE: PresenterFailure = failure(
    PresenterFailureStage::Submission,
    "canvas rectangle outside the canvas",
);
const CACHE_MISS: PresenterFailure =
    failure(PresenterFailureStage::Submission, "canvas cache slot empty");
const GPU_FAILED: PresenterFailure = failure(
    PresenterFailureStage::DeviceLost,
    "canvas command buffer failed",
);

/// One monitor's persistent canvas: the texture the executor writes and the
/// eight-bit layer presents, and the queue both submit to.
pub(crate) struct CanvasTarget {
    monitor_id: u16,
    width: u32,
    height: u32,
    texture: Retained<ProtocolObject<dyn MTLTexture>>,
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    device_id: u64,
    /// Held while a command buffer that touches the canvas is committed, so
    /// the queue receives them in the order the canvas must see them.
    order: Mutex<TargetOrder>,
    /// A command buffer that wrote the canvas failed on the GPU: the canvas
    /// is no longer what the Pier's mirror holds.
    gpu_failed: AtomicBool,
}

#[derive(Debug)]
struct TargetOrder {
    /// The newest committed write ended a record (its last part), so a
    /// present now shows whole records only.
    complete: bool,
}

// SAFETY: `MTLTexture` and `MTLCommandQueue` are documented thread-safe
// Metal objects; the texture is only written by GPU commands, and command
// buffers reach the queue under `order`. Nothing else is shared.
unsafe impl Send for CanvasTarget {}
// SAFETY: as above; every shared mutable state is behind `order` or atomic.
unsafe impl Sync for CanvasTarget {}

impl std::fmt::Debug for CanvasTarget {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CanvasTarget")
            .field("monitor_id", &self.monitor_id)
            .field("width", &self.width)
            .field("height", &self.height)
            .finish_non_exhaustive()
    }
}

impl CanvasTarget {
    /// Canvas width.
    pub(crate) const fn width(&self) -> u32 {
        self.width
    }

    /// Canvas height.
    pub(crate) const fn height(&self) -> u32 {
        self.height
    }

    /// The device the canvas lives on (`MTLDevice.registryID`).
    pub(crate) const fn device_id(&self) -> u64 {
        self.device_id
    }

    /// Whether a GPU failure lost the canvas.
    pub(crate) fn is_lost(&self) -> bool {
        self.gpu_failed.load(Ordering::Acquire)
    }

    /// Runs `submit` with the canvas's queue and texture while no write can
    /// be committed, when the canvas holds whole records; `None` while a
    /// record's parts are still arriving. `submit` must commit what it
    /// encodes before it returns.
    pub(crate) fn with_whole_canvas<R>(
        &self,
        submit: impl FnOnce(&ProtocolObject<dyn MTLCommandQueue>, &ProtocolObject<dyn MTLTexture>) -> R,
    ) -> Option<R> {
        let order = self.order.lock().expect("canvas order poisoned");
        order.complete.then(|| submit(&self.queue, &self.texture))
    }
}

/// Device, queue and pipelines, created with the first canvas.
struct CanvasGpu {
    device: Retained<ProtocolObject<dyn MTLDevice>>,
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    texture_cache: apple_cf::cv::CVMetalTextureCache,
    fill: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    surface: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
}

impl CanvasGpu {
    fn new() -> Result<Self, PresenterFailure> {
        let device = MTLCreateSystemDefaultDevice()
            .ok_or(failure(PresenterFailureStage::NoDevice, "no Metal device"))?;
        let queue = device.newCommandQueue().ok_or(failure(
            PresenterFailureStage::NoDevice,
            "Metal command queue creation failed",
        ))?;
        let texture_cache = apple_cf::cv::CVMetalTextureCache::system_default().ok_or(failure(
            PresenterFailureStage::PictureImport,
            "CoreVideo texture cache creation failed",
        ))?;
        let [fill, surface] = build_pipeline_states(
            &device,
            MTLPixelFormat::BGRA8Unorm,
            ["fs_canvas_fill", "fs_canvas_surface"],
        )
        .map_err(|_| {
            failure(
                PresenterFailureStage::Pipeline,
                "canvas pipeline creation failed",
            )
        })?;
        Ok(Self {
            device,
            queue,
            texture_cache,
            fill,
            surface,
        })
    }

    fn texture(
        &self,
        width: u32,
        height: u32,
        usage: MTLTextureUsage,
    ) -> Result<Retained<ProtocolObject<dyn MTLTexture>>, PresenterFailure> {
        // SAFETY: a plain descriptor with a valid format and a non-zero size
        // within the device's texture limit (checked by the callers).
        let descriptor = unsafe {
            MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                MTLPixelFormat::BGRA8Unorm,
                width as usize,
                height as usize,
                false,
            )
        };
        descriptor.setUsage(usage);
        descriptor.setStorageMode(MTLStorageMode::Private);
        self.device
            .newTextureWithDescriptor(&descriptor)
            .ok_or(failure(
                PresenterFailureStage::Submission,
                "canvas texture allocation failed",
            ))
    }
}

/// A cache entry: its pixels in a private buffer, `width × 4` bytes a row.
struct CacheEntry {
    width: u32,
    height: u32,
    buffer: Retained<ProtocolObject<dyn MTLBuffer>>,
}

/// One monitor's canvas and its cache.
struct MonitorCanvas {
    target: Arc<CanvasTarget>,
    cache: BTreeMap<u32, CacheEntry>,
    heaps: Vec<Retained<ProtocolObject<dyn MTLHeap>>>,
    scratch: Option<Retained<ProtocolObject<dyn MTLTexture>>>,
}

impl MonitorCanvas {
    fn new(
        gpu: &CanvasGpu,
        monitor_id: u16,
        width: u32,
        height: u32,
    ) -> Result<Self, PresenterFailure> {
        if width == 0 || height == 0 || width > MAX_CANVAS_PX || height > MAX_CANVAS_PX {
            return Err(OUTSIDE);
        }
        let texture = gpu.texture(
            width,
            height,
            MTLTextureUsage::RenderTarget | MTLTextureUsage::ShaderRead,
        )?;
        Ok(Self {
            target: Arc::new(CanvasTarget {
                monitor_id,
                width,
                height,
                texture,
                queue: gpu.queue.clone(),
                device_id: gpu.device.registryID(),
                order: Mutex::new(TargetOrder { complete: false }),
                gpu_failed: AtomicBool::new(false),
            }),
            cache: BTreeMap::new(),
            heaps: Vec::new(),
            scratch: None,
        })
    }

    fn inside(&self, rect: TileRect) -> bool {
        rect.width > 0
            && rect.height > 0
            && rect
                .x
                .checked_add(rect.width)
                .is_some_and(|right| right <= self.target.width)
            && rect
                .y
                .checked_add(rect.height)
                .is_some_and(|bottom| bottom <= self.target.height)
    }

    fn scratch(
        &mut self,
        gpu: &CanvasGpu,
        width: u32,
        height: u32,
    ) -> Result<Retained<ProtocolObject<dyn MTLTexture>>, PresenterFailure> {
        if let Some(scratch) = &self.scratch {
            if scratch.width() >= width as usize && scratch.height() >= height as usize {
                return Ok(scratch.clone());
            }
        }
        let (old_width, old_height) = self.scratch.as_ref().map_or((0, 0), |scratch| {
            (scratch.width() as u32, scratch.height() as u32)
        });
        let scratch = gpu.texture(
            width.max(old_width),
            height.max(old_height),
            MTLTextureUsage::ShaderRead,
        )?;
        self.scratch = Some(scratch.clone());
        Ok(scratch)
    }

    fn cache_buffer(
        &mut self,
        gpu: &CanvasGpu,
        bytes: usize,
    ) -> Result<Retained<ProtocolObject<dyn MTLBuffer>>, PresenterFailure> {
        for heap in &self.heaps {
            if let Some(buffer) = heap.newBufferWithLength_options(bytes, heap.resourceOptions()) {
                return Ok(buffer);
            }
        }
        let descriptor = MTLHeapDescriptor::new();
        descriptor.setSize(bytes.max(CACHE_HEAP_BYTES));
        descriptor.setStorageMode(MTLStorageMode::Private);
        descriptor.setHazardTrackingMode(MTLHazardTrackingMode::Tracked);
        let heap = gpu
            .device
            .newHeapWithDescriptor(&descriptor)
            .ok_or(failure(
                PresenterFailureStage::Submission,
                "canvas cache heap allocation failed",
            ))?;
        let buffer = heap
            .newBufferWithLength_options(bytes, heap.resourceOptions())
            .ok_or(failure(
                PresenterFailureStage::Submission,
                "canvas cache allocation failed",
            ))?;
        self.heaps.push(heap);
        Ok(buffer)
    }
}

const CANVAS: usize = 0;
const SCRATCH: usize = 1;

/// A resource region a blit reads or writes, in the resource's own
/// coordinates (a cache buffer's rows start at its origin).
#[derive(Debug, Clone, Copy)]
struct Access {
    resource: usize,
    rect: TileRect,
    write: bool,
}

enum Encoder {
    None,
    Blit(Retained<ProtocolObject<dyn MTLBlitCommandEncoder>>),
    Render(Retained<ProtocolObject<dyn MTLRenderCommandEncoder>>),
}

/// What a command buffer must keep alive until the GPU completed it
/// besides the Metal objects it retains itself: the CoreVideo owners of
/// imported surface planes.
#[derive(Default)]
struct KeepAlive {
    pictures: Vec<apple_cf::cv::CVPixelBuffer>,
    planes: Vec<apple_cf::cv::CVBuffer>,
}

// SAFETY: the CoreVideo objects are retain-counted handles that the
// completion handler only releases, after the GPU finished with them.
unsafe impl Send for KeepAlive {}

/// One record's command buffer on one canvas.
struct Recording {
    target: Arc<CanvasTarget>,
    command_buffer: Retained<ProtocolObject<dyn MTLCommandBuffer>>,
    encoder: Encoder,
    /// What the open blit encoder reads and writes: a conflicting blit
    /// starts a new encoder, which Metal orders after this one.
    accesses: Vec<Access>,
    keep: KeepAlive,
}

fn intersect(a: TileRect, b: TileRect) -> Option<TileRect> {
    let x = a.x.max(b.x);
    let y = a.y.max(b.y);
    let right = (a.x + a.width).min(b.x + b.width);
    let bottom = (a.y + a.height).min(b.y + b.height);
    (right > x && bottom > y).then(|| TileRect {
        x,
        y,
        width: right - x,
        height: bottom - y,
    })
}

const fn origin(x: u32, y: u32) -> MTLOrigin {
    MTLOrigin {
        x: x as usize,
        y: y as usize,
        z: 0,
    }
}

const fn size(width: u32, height: u32) -> MTLSize {
    MTLSize {
        width: width as usize,
        height: height as usize,
        depth: 1,
    }
}

impl Recording {
    fn new(gpu: &CanvasGpu, target: Arc<CanvasTarget>) -> Result<Self, PresenterFailure> {
        let command_buffer = gpu.queue.commandBuffer().ok_or(failure(
            PresenterFailureStage::Submission,
            "command buffer creation failed",
        ))?;
        Ok(Self {
            target,
            command_buffer,
            encoder: Encoder::None,
            accesses: Vec::new(),
            keep: KeepAlive::default(),
        })
    }

    fn end_encoder(&mut self) {
        match std::mem::replace(&mut self.encoder, Encoder::None) {
            Encoder::None => {}
            Encoder::Blit(encoder) => encoder.endEncoding(),
            Encoder::Render(encoder) => encoder.endEncoding(),
        }
        self.accesses.clear();
    }

    /// The blit encoder for an operation with these accesses: the open one,
    /// unless it already touched a region this one conflicts with.
    fn blit(
        &mut self,
        accesses: &[Access],
    ) -> Result<Retained<ProtocolObject<dyn MTLBlitCommandEncoder>>, PresenterFailure> {
        let conflicts = accesses.iter().any(|new| {
            self.accesses.iter().any(|old| {
                old.resource == new.resource
                    && (old.write || new.write)
                    && intersect(old.rect, new.rect).is_some()
            })
        });
        if conflicts || !matches!(self.encoder, Encoder::Blit(_)) {
            self.end_encoder();
            let encoder = self.command_buffer.blitCommandEncoder().ok_or(failure(
                PresenterFailureStage::Submission,
                "blit encoder creation failed",
            ))?;
            self.encoder = Encoder::Blit(encoder);
        }
        self.accesses.extend_from_slice(accesses);
        match &self.encoder {
            Encoder::Blit(encoder) => Ok(encoder.clone()),
            Encoder::None | Encoder::Render(_) => unreachable!("a blit encoder was just opened"),
        }
    }

    /// A render encoder into the canvas: `clear` starts a new pass that
    /// clears it to opaque black, otherwise the open pass (or a new one
    /// that keeps the canvas) is reused.
    fn render(
        &mut self,
        clear: bool,
    ) -> Result<Retained<ProtocolObject<dyn MTLRenderCommandEncoder>>, PresenterFailure> {
        if clear || !matches!(self.encoder, Encoder::Render(_)) {
            self.end_encoder();
            let pass = MTLRenderPassDescriptor::renderPassDescriptor();
            // SAFETY: colour attachment 0 is valid on every render pass.
            let attachment = unsafe { pass.colorAttachments().objectAtIndexedSubscript(0) };
            attachment.setTexture(Some(&*self.target.texture));
            attachment.setLoadAction(if clear {
                MTLLoadAction::Clear
            } else {
                MTLLoadAction::Load
            });
            attachment.setStoreAction(MTLStoreAction::Store);
            attachment.setClearColor(MTLClearColor {
                red: 0.0,
                green: 0.0,
                blue: 0.0,
                alpha: 1.0,
            });
            let encoder = self
                .command_buffer
                .renderCommandEncoderWithDescriptor(&pass)
                .ok_or(failure(
                    PresenterFailureStage::Submission,
                    "render encoder creation failed",
                ))?;
            self.encoder = Encoder::Render(encoder);
        }
        match &self.encoder {
            Encoder::Render(encoder) => Ok(encoder.clone()),
            Encoder::None | Encoder::Blit(_) => unreachable!("a render encoder was just opened"),
        }
    }

    /// Ends encoding and commits under the canvas's ordering lock; `whole`
    /// says whether the canvas now holds whole records.
    fn commit(mut self, whole: bool) -> Retained<ProtocolObject<dyn MTLCommandBuffer>> {
        self.end_encoder();
        let target = Arc::clone(&self.target);
        let keep = Mutex::new(Some(std::mem::take(&mut self.keep)));
        let completed = RcBlock::new(
            move |buffer: NonNull<ProtocolObject<dyn MTLCommandBuffer>>| {
                // SAFETY: Metal passes the live command buffer that completed.
                let status = unsafe { buffer.as_ref() }.status();
                if status == MTLCommandBufferStatus::Error {
                    target.gpu_failed.store(true, Ordering::Release);
                }
                keep.lock().expect("keep-alive poisoned").take();
            },
        );
        // SAFETY: `completed` is a valid heap block; Metal copies it and
        // calls it once after the command buffer completed.
        unsafe {
            self.command_buffer
                .addCompletedHandler(RcBlock::as_ptr(&completed).cast());
        }
        let mut order = self.target.order.lock().expect("canvas order poisoned");
        self.command_buffer.commit();
        order.complete = whole;
        drop(order);
        self.command_buffer.clone()
    }
}

impl Drop for Recording {
    /// A record that failed part-way is dropped uncommitted: its open
    /// encoder must still be ended (Metal forbids releasing one that was
    /// not).
    fn drop(&mut self) {
        self.end_encoder();
    }
}

/// The eight-bit presenter's canvas executor: every monitor's canvas this
/// presenter shows, and the readbacks it owes.
#[derive(Default)]
pub(crate) struct MetalCanvasExecutor {
    gpu: Option<CanvasGpu>,
    canvases: BTreeMap<u16, MonitorCanvas>,
    readbacks: VecDeque<CanvasReadback>,
}

impl MetalCanvasExecutor {
    /// Releases every canvas and cache (the connection ended).
    pub(crate) fn release(&mut self) {
        self.canvases.clear();
        self.readbacks.clear();
    }

    /// A readback an earlier [`Self::execute`] produced.
    pub(crate) fn take_readback(&mut self) -> Option<CanvasReadback> {
        self.readbacks.pop_front()
    }

    /// Applies one execution to its monitor's canvas and returns the canvas
    /// to present. Nothing is presented here.
    pub(crate) fn execute(
        &mut self,
        execution: &CanvasExecution<'_, '_, apple_cf::cv::CVPixelBuffer>,
    ) -> Result<Arc<CanvasTarget>, PresenterFailure> {
        autoreleasepool(|_| self.execute_inner(execution))
    }

    fn execute_inner(
        &mut self,
        execution: &CanvasExecution<'_, '_, apple_cf::cv::CVPixelBuffer>,
    ) -> Result<Arc<CanvasTarget>, PresenterFailure> {
        let work = execution.work;
        if execution.inputs.len() != work.ops.len() {
            return Err(BAD_INPUT);
        }
        if self.gpu.is_none() {
            self.gpu = Some(CanvasGpu::new()?);
        }
        let gpu = self.gpu.as_ref().expect("created above");
        let monitor_id = work.monitor_id;
        let staging = stage_tiles(gpu, work, execution.inputs)?;
        let mut recording: Option<Recording> = None;
        for (index, (op, input)) in work.ops.iter().zip(execution.inputs).enumerate() {
            if let CanvasOp::Reset { width, height } = op {
                if let Some(previous) = recording.take() {
                    previous.commit(true);
                }
                let canvas = MonitorCanvas::new(gpu, monitor_id, *width, *height)?;
                self.canvases.insert(monitor_id, canvas);
                let canvas = self.canvases.get(&monitor_id).expect("inserted above");
                let mut fresh = Recording::new(gpu, Arc::clone(&canvas.target))?;
                fresh.render(true)?;
                recording = Some(fresh);
                continue;
            }
            let canvas = self.canvases.get_mut(&monitor_id).ok_or(NO_CANVAS)?;
            if canvas.target.is_lost() {
                return Err(GPU_FAILED);
            }
            if recording.is_none() {
                recording = Some(Recording::new(gpu, Arc::clone(&canvas.target))?);
            }
            let recording = recording.as_mut().expect("created above");
            apply_op(gpu, canvas, recording, op, input, staging.at(index))?;
        }
        let canvas = self.canvases.get_mut(&monitor_id).ok_or(NO_CANVAS)?;
        let target = Arc::clone(&canvas.target);
        let readback = if execution.readback {
            let recording = match recording.take() {
                Some(recording) => recording,
                None => Recording::new(gpu, Arc::clone(&target))?,
            };
            Some(read_back(gpu, recording, execution)?)
        } else {
            if let Some(recording) = recording.take() {
                recording.commit(execution.present);
            } else if execution.present {
                target.order.lock().expect("canvas order poisoned").complete = true;
            }
            None
        };
        if let Some(readback) = readback {
            self.readbacks.push_back(readback);
        }
        if target.is_lost() {
            return Err(GPU_FAILED);
        }
        Ok(target)
    }
}

/// Every tile of a record, written as BGRA8 rows into one shared buffer.
struct Staging {
    buffer: Option<Retained<ProtocolObject<dyn MTLBuffer>>>,
    offsets: Vec<Option<usize>>,
}

impl Staging {
    fn at(&self, index: usize) -> Option<(&ProtocolObject<dyn MTLBuffer>, usize)> {
        let offset = self.offsets.get(index).copied().flatten()?;
        self.buffer.as_deref().map(|buffer| (buffer, offset))
    }
}

fn stage_tiles<N>(
    gpu: &CanvasGpu,
    work: &CanvasWork<'_>,
    inputs: &[CanvasOpInput<N>],
) -> Result<Staging, PresenterFailure> {
    let mut offsets = Vec::with_capacity(work.ops.len());
    let mut total = 0_usize;
    for (op, input) in work.ops.iter().zip(inputs) {
        match (op, input) {
            (CanvasOp::Tile { rect, .. }, CanvasOpInput::Tile(codes)) => {
                if codes.width() != rect.width || codes.height() != rect.height {
                    return Err(BAD_INPUT);
                }
                offsets.push(Some(total));
                total += rect.width as usize * rect.height as usize * 4;
            }
            (CanvasOp::Tile { .. }, _) => return Err(BAD_INPUT),
            _ => offsets.push(None),
        }
    }
    if total == 0 {
        return Ok(Staging {
            buffer: None,
            offsets,
        });
    }
    let buffer = gpu
        .device
        .newBufferWithLength_options(total, MTLResourceOptions::StorageModeShared)
        .ok_or(failure(
            PresenterFailureStage::Submission,
            "canvas tile staging allocation failed",
        ))?;
    // SAFETY: a shared buffer of `total` bytes, not yet used by the GPU;
    // the slice ends before this function returns.
    let bytes =
        unsafe { std::slice::from_raw_parts_mut(buffer.contents().as_ptr().cast::<u8>(), total) };
    for ((_, input), offset) in work.ops.iter().zip(inputs).zip(&offsets) {
        if let (CanvasOpInput::Tile(codes), Some(offset)) = (input, offset) {
            write_tile(codes, &mut bytes[*offset..])?;
        }
    }
    Ok(Staging {
        buffer: Some(buffer),
        offsets,
    })
}

/// Writes a tile's depth-8 planar codes as opaque BGRA8 rows.
fn write_tile(codes: &TileCodes, out: &mut [u8]) -> Result<(), PresenterFailure> {
    let pixels = codes.width() as usize * codes.height() as usize;
    let (r, g, b) = (codes.plane(0), codes.plane(1), codes.plane(2));
    if r.len() < pixels || g.len() < pixels || b.len() < pixels || out.len() < pixels * 4 {
        return Err(BAD_INPUT);
    }
    let code = |value: u16| u8::try_from(value).map_err(|_| BAD_INPUT);
    for (at, pixel) in out[..pixels * 4].chunks_exact_mut(4).enumerate() {
        pixel.copy_from_slice(&[code(b[at])?, code(g[at])?, code(r[at])?, 255]);
    }
    Ok(())
}

fn apply_op(
    gpu: &CanvasGpu,
    canvas: &mut MonitorCanvas,
    recording: &mut Recording,
    op: &CanvasOp<'_>,
    input: &CanvasOpInput<apple_cf::cv::CVPixelBuffer>,
    staged: Option<(&ProtocolObject<dyn MTLBuffer>, usize)>,
) -> Result<(), PresenterFailure> {
    let checked = |rect: TileRect| {
        if canvas.inside(rect) {
            Ok(rect)
        } else {
            Err(OUTSIDE)
        }
    };
    match op {
        CanvasOp::Reset { .. } => unreachable!("resets are handled by the caller"),
        CanvasOp::Fill { rect, bgra } => {
            let rect = checked(*rect)?;
            let colour = [
                f32::from(bgra[2]) / 255.0,
                f32::from(bgra[1]) / 255.0,
                f32::from(bgra[0]) / 255.0,
                1.0,
            ];
            let encoder = recording.render(false)?;
            encoder.setRenderPipelineState(&gpu.fill);
            encoder.setScissorRect(scissor(rect));
            // SAFETY: 16 bytes (Metal copies at most 4 KiB from a live slice);
            // the encoder is live and the index matches the shader binding.
            unsafe {
                encoder.setFragmentBytes_length_atIndex(
                    NonNull::from(&colour).cast(),
                    std::mem::size_of_val(&colour),
                    0,
                );
                encoder.drawPrimitives_vertexStart_vertexCount(MTLPrimitiveType::Triangle, 0, 3);
            }
        }
        CanvasOp::Copy { src, dst_x, dst_y } => {
            let src = checked(*src)?;
            let dst = checked(TileRect {
                x: *dst_x,
                y: *dst_y,
                ..src
            })?;
            let texture = canvas.target.texture.clone();
            if intersect(src, dst).is_none() {
                let encoder = recording.blit(&[
                    Access {
                        resource: CANVAS,
                        rect: src,
                        write: false,
                    },
                    Access {
                        resource: CANVAS,
                        rect: dst,
                        write: true,
                    },
                ])?;
                blit_texture(&encoder, &texture, src, &texture, dst.x, dst.y);
            } else {
                let scratch = canvas.scratch(gpu, src.width, src.height)?;
                let staged = TileRect { x: 0, y: 0, ..src };
                let encoder = recording.blit(&[
                    Access {
                        resource: CANVAS,
                        rect: src,
                        write: false,
                    },
                    Access {
                        resource: SCRATCH,
                        rect: staged,
                        write: true,
                    },
                ])?;
                blit_texture(&encoder, &texture, src, &scratch, 0, 0);
                let encoder = recording.blit(&[
                    Access {
                        resource: SCRATCH,
                        rect: staged,
                        write: false,
                    },
                    Access {
                        resource: CANVAS,
                        rect: dst,
                        write: true,
                    },
                ])?;
                blit_texture(&encoder, &scratch, staged, &texture, dst.x, dst.y);
            }
        }
        CanvasOp::Tile { rect, .. } => {
            let rect = checked(*rect)?;
            let (buffer, offset) = staged.ok_or(BAD_INPUT)?;
            let encoder = recording.blit(&[Access {
                resource: CANVAS,
                rect,
                write: true,
            }])?;
            // SAFETY: the staging buffer holds `rect`'s rows at `offset`
            // (4-byte aligned, `width × 4` bytes a row) and the destination
            // lies inside the canvas.
            unsafe {
                encoder.copyFromBuffer_sourceOffset_sourceBytesPerRow_sourceBytesPerImage_sourceSize_toTexture_destinationSlice_destinationLevel_destinationOrigin(
                    buffer,
                    offset,
                    rect.width as usize * 4,
                    rect.width as usize * rect.height as usize * 4,
                    size(rect.width, rect.height),
                    &canvas.target.texture,
                    0,
                    0,
                    origin(rect.x, rect.y),
                );
            }
        }
        CanvasOp::CacheStore { slot, rect } => {
            let rect = checked(*rect)?;
            let bytes = rect.width as usize * rect.height as usize * 4;
            let buffer = canvas.cache_buffer(gpu, bytes)?;
            let encoder = recording.blit(&[
                Access {
                    resource: CANVAS,
                    rect,
                    write: false,
                },
                Access {
                    resource: Retained::as_ptr(&buffer) as usize,
                    rect: TileRect { x: 0, y: 0, ..rect },
                    write: true,
                },
            ])?;
            // SAFETY: `rect` lies inside the canvas and the buffer holds
            // exactly its rows.
            unsafe {
                encoder.copyFromTexture_sourceSlice_sourceLevel_sourceOrigin_sourceSize_toBuffer_destinationOffset_destinationBytesPerRow_destinationBytesPerImage(
                    &canvas.target.texture,
                    0,
                    0,
                    origin(rect.x, rect.y),
                    size(rect.width, rect.height),
                    &buffer,
                    0,
                    rect.width as usize * 4,
                    bytes,
                );
            }
            canvas.cache.insert(
                *slot,
                CacheEntry {
                    width: rect.width,
                    height: rect.height,
                    buffer,
                },
            );
        }
        CanvasOp::CachePaint { slot, rect } => {
            let rect = checked(*rect)?;
            let entry = canvas.cache.get(slot).ok_or(CACHE_MISS)?;
            if entry.width != rect.width || entry.height != rect.height {
                return Err(CACHE_MISS);
            }
            let buffer = entry.buffer.clone();
            let encoder = recording.blit(&[
                Access {
                    resource: Retained::as_ptr(&buffer) as usize,
                    rect: TileRect { x: 0, y: 0, ..rect },
                    write: false,
                },
                Access {
                    resource: CANVAS,
                    rect,
                    write: true,
                },
            ])?;
            // SAFETY: the buffer holds the entry's rows and `rect` (its size)
            // lies inside the canvas.
            unsafe {
                encoder.copyFromBuffer_sourceOffset_sourceBytesPerRow_sourceBytesPerImage_sourceSize_toTexture_destinationSlice_destinationLevel_destinationOrigin(
                    &buffer,
                    0,
                    rect.width as usize * 4,
                    rect.width as usize * rect.height as usize * 4,
                    size(rect.width, rect.height),
                    &canvas.target.texture,
                    0,
                    0,
                    origin(rect.x, rect.y),
                );
            }
        }
        CanvasOp::CacheEvict { slot } => {
            canvas.cache.remove(slot).ok_or(CACHE_MISS)?;
        }
        // Surface decoders are the media worker's; the canvas keeps no
        // surface state.
        CanvasOp::SurfaceOpen { .. } | CanvasOp::SurfaceClose { .. } => {}
        CanvasOp::SurfacePaint { dest, blocks, .. } => {
            let dest = checked(*dest)?;
            let CanvasOpInput::Surface(picture) = input else {
                return Err(BAD_INPUT);
            };
            let Some(picture) = picture else {
                return Ok(());
            };
            paint_surface(
                gpu,
                canvas,
                recording,
                &picture.picture,
                picture.video,
                dest,
                blocks,
            )?;
        }
    }
    Ok(())
}

fn scissor(rect: TileRect) -> MTLScissorRect {
    MTLScissorRect {
        x: rect.x as usize,
        y: rect.y as usize,
        width: rect.width as usize,
        height: rect.height as usize,
    }
}

fn blit_texture(
    encoder: &ProtocolObject<dyn MTLBlitCommandEncoder>,
    source: &ProtocolObject<dyn MTLTexture>,
    rect: TileRect,
    destination: &ProtocolObject<dyn MTLTexture>,
    x: u32,
    y: u32,
) {
    // SAFETY: both regions lie inside their textures (checked by the
    // caller) and do not overlap when the textures are the same.
    unsafe {
        encoder.copyFromTexture_sourceSlice_sourceLevel_sourceOrigin_sourceSize_toTexture_destinationSlice_destinationLevel_destinationOrigin(
            source,
            0,
            0,
            origin(rect.x, rect.y),
            size(rect.width, rect.height),
            destination,
            0,
            0,
            origin(x, y),
        );
    }
}

#[allow(clippy::too_many_arguments, reason = "one surface paint's inputs")]
fn paint_surface(
    gpu: &CanvasGpu,
    canvas: &MonitorCanvas,
    recording: &mut Recording,
    picture: &apple_cf::cv::CVPixelBuffer,
    video: arcen_media::VideoConfiguration,
    dest: TileRect,
    blocks: &[TileRect],
) -> Result<(), PresenterFailure> {
    const IMPORT: PresenterFailure = failure(
        PresenterFailureStage::PictureImport,
        "surface picture import failed",
    );
    if video.bit_depth != arcen_media::BitDepth::Eight || picture.plane_count() != 2 {
        return Err(IMPORT);
    }
    let (width, height) = (picture.width_of_plane(0), picture.height_of_plane(0));
    let (chroma_width, chroma_height) = (picture.width_of_plane(1), picture.height_of_plane(1));
    if width == 0 || height == 0 || chroma_width == 0 || chroma_height == 0 {
        return Err(IMPORT);
    }
    let spans: Vec<TileRect> = blocks
        .iter()
        .filter_map(|block| intersect(*block, dest))
        .filter(|span| canvas.inside(*span))
        .collect();
    if spans.is_empty() {
        return Ok(());
    }
    let plan = plane_pixel_formats(video.bit_depth);
    let (luma_cv, luma) = create_cv_plane_texture(
        &gpu.texture_cache,
        picture,
        plan.luma_format,
        width,
        height,
        0,
    )
    .ok_or(IMPORT)?;
    let (chroma_cv, chroma) = create_cv_plane_texture(
        &gpu.texture_cache,
        picture,
        plan.chroma_format,
        chroma_width,
        chroma_height,
        1,
    )
    .ok_or(IMPORT)?;
    recording.keep.pictures.push(picture.clone());
    recording.keep.planes.push(luma_cv);
    recording.keep.planes.push(chroma_cv);
    let as_u32 = |value: usize| u32::try_from(value).map_err(|_| IMPORT);
    let uniform = MetalVideoUniform::from_contract(
        VideoColorContract::from(video),
        (as_u32(width)?, as_u32(height)?),
        (as_u32(chroma_width)?, as_u32(chroma_height)?),
        plan.code_unnormalize_scale,
    )
    .to_bytes();
    let paint = [
        dest.x,
        dest.y,
        dest.width,
        dest.height,
        as_u32(width)?,
        as_u32(height)?,
    ];
    let encoder = recording.render(false)?;
    encoder.setRenderPipelineState(&gpu.surface);
    // SAFETY: the raw texture pointers stay valid while their CoreVideo
    // owners, kept in the recording until the command buffer completed, are
    // alive.
    let (luma, chroma): (
        &ProtocolObject<dyn MTLTexture>,
        &ProtocolObject<dyn MTLTexture>,
    ) = unsafe {
        (
            &*(luma as *const ProtocolObject<dyn MTLTexture>),
            &*(chroma as *const ProtocolObject<dyn MTLTexture>),
        )
    };
    // SAFETY: the encoder is live, the indices match `fs_canvas_surface`'s
    // bindings, and Metal copies the (under 4 KiB) byte slices.
    unsafe {
        encoder.setFragmentTexture_atIndex(Some(luma), 0);
        encoder.setFragmentTexture_atIndex(Some(chroma), 1);
        encoder.setFragmentBytes_length_atIndex(
            NonNull::new(uniform.as_ptr() as *mut c_void).expect("uniform bytes"),
            uniform.len(),
            0,
        );
        encoder.setFragmentBytes_length_atIndex(
            NonNull::from(&paint).cast(),
            std::mem::size_of_val(&paint),
            1,
        );
    }
    for span in spans {
        encoder.setScissorRect(scissor(span));
        // SAFETY: the pipeline and its bindings are set above.
        unsafe {
            encoder.drawPrimitives_vertexStart_vertexCount(MTLPrimitiveType::Triangle, 0, 3);
        }
    }
    Ok(())
}

/// Copies the whole canvas into a shared buffer at the end of `recording`,
/// commits and waits, and returns it (the Deck's canvas digest, at most
/// every ten seconds).
fn read_back(
    gpu: &CanvasGpu,
    mut recording: Recording,
    execution: &CanvasExecution<'_, '_, apple_cf::cv::CVPixelBuffer>,
) -> Result<CanvasReadback, PresenterFailure> {
    let target = Arc::clone(&recording.target);
    let whole = TileRect {
        x: 0,
        y: 0,
        width: target.width,
        height: target.height,
    };
    let stride = target.width as usize * 4;
    let bytes = stride * target.height as usize;
    let buffer = gpu
        .device
        .newBufferWithLength_options(bytes, MTLResourceOptions::StorageModeShared)
        .ok_or(failure(
            PresenterFailureStage::Submission,
            "canvas readback allocation failed",
        ))?;
    let encoder = recording.blit(&[Access {
        resource: CANVAS,
        rect: whole,
        write: false,
    }])?;
    // SAFETY: the buffer holds the whole canvas at `stride` bytes a row.
    unsafe {
        encoder.copyFromTexture_sourceSlice_sourceLevel_sourceOrigin_sourceSize_toBuffer_destinationOffset_destinationBytesPerRow_destinationBytesPerImage(
            &target.texture,
            0,
            0,
            origin(0, 0),
            size(target.width, target.height),
            &buffer,
            0,
            stride,
            bytes,
        );
    }
    let command_buffer = recording.commit(execution.present);
    command_buffer.waitUntilCompleted();
    if command_buffer.status() != MTLCommandBufferStatus::Completed {
        target.gpu_failed.store(true, Ordering::Release);
        return Err(GPU_FAILED);
    }
    // SAFETY: the GPU completed the copy into this shared buffer of `bytes`.
    let bgra =
        unsafe { std::slice::from_raw_parts(buffer.contents().as_ptr().cast::<u8>(), bytes) }
            .to_vec();
    Ok(CanvasReadback {
        monitor_id: target.monitor_id,
        generation: execution.work.generation,
        fence: execution.work.fence,
        stride,
        bgra,
    })
}

#[cfg(test)]
mod tests {
    //! The D2 gate: the Metal canvas read back equals the CPU executor
    //! (`CanvasPixels`, the oracle) on the shared corpus, bit-exact on
    //! every lossless block and within one code on surface blocks (the
    //! shared conversion's documented tolerance: the GPU rounds the same
    //! `VideoUniform` maths the CPU reference rounds).

    use super::*;
    use arcen_deck_core::decode::{NativeDecodedVideoFrame, NativePictureFormat};
    use arcen_deck_core::present::{CanvasOutcome, VideoPresenterSink};
    use arcen_media::canvas::corpus::{self, ScenarioKind};
    use arcen_media::canvas::{
        CanvasExecError, CanvasPixels, CanvasState, PackedTileCodes, SurfaceDecoder, SurfacePicture,
    };
    use arcen_media::refine::tile::decode_residuals;
    use arcen_media::video::present::VideoUniform;
    use arcen_media::video::PresentTiming;
    use arcen_protocol::canvas::{CanvasRecord, CanvasTerms};
    use std::collections::VecDeque;
    use std::num::NonZeroU32;
    use std::time::Instant;

    use crate::ui::video_metal_layer::DedicatedEightBitVideoPresenter;

    const NV12_VIDEO_RANGE: u32 = u32::from_be_bytes(*b"420v");

    fn terms() -> CanvasTerms {
        CanvasTerms {
            max_canvas_px: MAX_CANVAS_PX,
            max_surfaces_per_monitor: 2,
            cache_bytes: 16 * 1024 * 1024,
            cache_slots: 4096,
        }
    }

    fn has_metal() -> bool {
        let available = MTLCreateSystemDefaultDevice().is_some();
        if !available {
            eprintln!("no Metal device; canvas executor test skipped");
        }
        available
    }

    /// A decoded surface picture: an IOSurface-backed `420v` buffer (what
    /// VideoToolbox hands the Deck) and the shared CPU reference's BGRA of
    /// it.
    struct Picture {
        native: apple_cf::cv::CVPixelBuffer,
        width: u32,
        height: u32,
        bgra: Vec<u8>,
    }

    fn noise(seed: u32) -> u8 {
        let mut state = seed.wrapping_mul(2_654_435_761).wrapping_add(0x9E37_79B9);
        state ^= state >> 15;
        state = state.wrapping_mul(0x2C1B_3C6D);
        state ^= state >> 12;
        state.to_le_bytes()[0]
    }

    fn iosurface_buffer(width: usize, height: usize) -> apple_cf::cv::CVPixelBuffer {
        use apple_cf::cf::{CFDictionary, CFNumber, CFString, CFType};
        use apple_cf::raw;
        // SAFETY: well-known CoreVideo/CoreFoundation constants.
        let (format_key, metal_key, iosurface_key, yes) = unsafe {
            (
                CFString::from_raw_retained(
                    raw::kCVPixelBufferPixelFormatTypeKey.cast_mut().cast(),
                )
                .expect("format key"),
                CFString::from_raw_retained(
                    raw::kCVPixelBufferMetalCompatibilityKey.cast_mut().cast(),
                )
                .expect("metal key"),
                CFString::from_raw_retained(
                    raw::kCVPixelBufferIOSurfacePropertiesKey.cast_mut().cast(),
                )
                .expect("iosurface key"),
                CFType::from_raw_retained(raw::kCFBooleanTrue.cast_mut().cast()).expect("true"),
            )
        };
        let format = CFNumber::from_i64(i64::from(NV12_VIDEO_RANGE));
        let properties = CFDictionary::from_pairs(&[]);
        let attributes = CFDictionary::from_pairs(&[
            (&format_key, &format),
            (&metal_key, &yes),
            (&iosurface_key, &properties),
        ]);
        let mut out: raw::CVPixelBufferRef = std::ptr::null_mut();
        // SAFETY: valid size, format and attributes; `out` receives a +1
        // reference owned by the wrapper below.
        let status = unsafe {
            raw::CVPixelBufferCreate(
                std::ptr::null(),
                width,
                height,
                NV12_VIDEO_RANGE,
                attributes.as_ptr().cast(),
                &mut out,
            )
        };
        assert_eq!(status, 0, "CVPixelBufferCreate");
        apple_cf::cv::CVPixelBuffer::from_raw(out.cast()).expect("pixel buffer")
    }

    /// A limited-range BT.709 4:2:0 picture with sharp luma and chroma
    /// detail, and its CPU reference conversion (the shaders' tested
    /// reference: `VideoUniform::reference_rgb` after
    /// `chroma_reference_codes`, rounded to eight bits).
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    fn picture(width: u32, height: u32, seed: u32) -> Picture {
        let (w, h) = (width as usize, height as usize);
        let (cw, ch) = (w / 2, h / 2);
        let luma: Vec<u8> = (0..w * h)
            .map(|at| 16 + noise(seed ^ (at as u32).wrapping_mul(31)) % 220)
            .collect();
        let chroma: Vec<[u8; 2]> = (0..cw * ch)
            .map(|at| {
                let at = at as u32;
                [
                    16 + noise(seed.wrapping_add(at * 7)) % 225,
                    16 + noise(seed.wrapping_add(at * 13 + 1)) % 225,
                ]
            })
            .collect();
        let native = iosurface_buffer(w, h);
        {
            let mut guard = native.lock_read_write().expect("lock");
            let luma_stride = guard.bytes_per_row_of_plane(0);
            let chroma_stride = guard.bytes_per_row_of_plane(1);
            let luma_base = guard.base_address_of_plane_mut(0).expect("luma plane");
            for y in 0..h {
                // SAFETY: row `y` of the locked luma plane holds `w` bytes.
                let row =
                    unsafe { std::slice::from_raw_parts_mut(luma_base.add(y * luma_stride), w) };
                row.copy_from_slice(&luma[y * w..(y + 1) * w]);
            }
            let chroma_base = guard.base_address_of_plane_mut(1).expect("chroma plane");
            for y in 0..ch {
                // SAFETY: row `y` of the locked chroma plane holds `2 cw`
                // bytes.
                let row = unsafe {
                    std::slice::from_raw_parts_mut(chroma_base.add(y * chroma_stride), cw * 2)
                };
                for x in 0..cw {
                    row[x * 2..x * 2 + 2].copy_from_slice(&chroma[y * cw + x]);
                }
            }
        }
        let uniform = VideoUniform::from_contract(
            VideoColorContract::from(arcen_media::VideoConfiguration::legacy_h264()),
            (width, height),
            (width / 2, height / 2),
        );
        let luma_at = |x: u32, y: u32| f32::from(luma[y as usize * w + x as usize]);
        let chroma_at = |x: u32, y: u32| {
            let [cb, cr] = chroma[y as usize * cw + x as usize];
            [f32::from(cb), f32::from(cr)]
        };
        let mut bgra = Vec::with_capacity(w * h * 4);
        for y in 0..height {
            for x in 0..width {
                let uv = [
                    (x as f32 + 0.5) / width as f32,
                    (y as f32 + 0.5) / height as f32,
                ];
                let [cb, cr] =
                    uniform.chroma_reference_codes(uv, luma_at(x, y), luma_at, chroma_at);
                let [r, g, b] = uniform.reference_rgb(luma_at(x, y), cb, cr);
                let quantise = |v: f32| (v * 255.0).round() as u8;
                bgra.extend_from_slice(&[quantise(b), quantise(g), quantise(r), 255]);
            }
        }
        Picture {
            native,
            width,
            height,
            bgra,
        }
    }

    /// The oracle's surface seam: the reference pictures, in paint order.
    #[derive(Default)]
    struct ReferenceSurfaces {
        /// `None` for a paint whose access unit decoded to no picture.
        pictures: VecDeque<Option<Picture>>,
        current: Option<Picture>,
    }

    impl SurfaceDecoder for ReferenceSurfaces {
        fn open(&mut self, _surface: u8, _width: u32, _height: u32) {}

        fn close(&mut self, _surface: u8) {}

        fn decode(
            &mut self,
            _surface: u8,
            _access_unit: &[u8],
            _idr: bool,
        ) -> Result<Option<SurfacePicture<'_>>, CanvasExecError> {
            self.current = self.pictures.pop_front().ok_or(CanvasExecError::Surface)?;
            Ok(self.current.as_ref().map(|picture| SurfacePicture {
                width: picture.width,
                height: picture.height,
                bgra: &picture.bgra,
            }))
        }
    }

    fn native(picture: &Picture) -> CanvasOpInput<apple_cf::cv::CVPixelBuffer> {
        CanvasOpInput::Surface(Some(NativeDecodedVideoFrame {
            picture: picture.native.clone(),
            format: NativePictureFormat::Nv12,
            video: arcen_media::VideoConfiguration::legacy_h264(),
        }))
    }

    fn execution<'w, 'a>(
        work: &'w CanvasWork<'a>,
        inputs: &'w [CanvasOpInput<apple_cf::cv::CVPixelBuffer>],
        present: bool,
    ) -> CanvasExecution<'w, 'a, apple_cf::cv::CVPixelBuffer> {
        CanvasExecution {
            work,
            inputs,
            arrival: Instant::now(),
            timing: PresentTiming::Newest,
            present,
            readback: true,
        }
    }

    /// Every block equal: exactly where `exact(block)`, else within one
    /// code. Returns the surface blocks compared.
    fn compare(
        what: &str,
        gpu: &CanvasReadback,
        oracle: &CanvasPixels,
        exact: impl Fn(u32) -> bool,
    ) -> u32 {
        assert_eq!(gpu.stride, oracle.stride(), "{what}: stride");
        assert_eq!(gpu.bgra.len(), oracle.bgra().len(), "{what}: size");
        let grid = arcen_media::refine::tile::RefineGrid {
            width: oracle.width(),
            height: oracle.height(),
        };
        let mut lossy = 0;
        for index in 0..grid.block_count() {
            let block = grid.block_rect(index).expect("block");
            let exact = exact(index);
            lossy += u32::from(!exact);
            for y in block.y..block.y + block.height {
                let start = y as usize * gpu.stride + block.x as usize * 4;
                let end = start + block.width as usize * 4;
                let (a, b) = (&gpu.bgra[start..end], &oracle.bgra()[start..end]);
                if exact {
                    assert_eq!(a, b, "{what}: lossless block {index} row {y} differs");
                } else {
                    let worst = a.iter().zip(b).map(|(a, b)| a.abs_diff(*b)).max();
                    assert!(
                        worst <= Some(1),
                        "{what}: surface block {index} row {y} differs by {worst:?}"
                    );
                }
            }
        }
        lossy
    }

    /// Replays `records` through the shared state owner into the CPU oracle
    /// and, through the sink seam, the eight-bit presenter's Metal canvas,
    /// comparing the read-back canvas with the oracle after every record.
    /// Returns the surface blocks compared.
    fn replay(what: &str, records: &[Vec<u8>]) -> u32 {
        let mut state = CanvasState::new(0, terms());
        let mut oracle = CanvasPixels::new();
        let mut presenter = DedicatedEightBitVideoPresenter::new();
        let mut sizes: BTreeMap<u8, (u32, u32)> = BTreeMap::new();
        let mut seed = 0_u32;
        let mut surface_blocks = 0;
        for (index, bytes) in records.iter().enumerate() {
            let what = format!("{what} record {index}");
            let record = CanvasRecord::decode(bytes).expect("corpus record");
            let work = state.apply(&record).expect("corpus record admitted");
            let mut tiles = PackedTileCodes::default();
            let mut surfaces = ReferenceSurfaces::default();
            let mut inputs = Vec::with_capacity(work.ops.len());
            for op in &work.ops {
                inputs.push(match op {
                    CanvasOp::Tile { rect, payload } => {
                        let codes = decode_residuals(payload, rect.width, rect.height, 8)
                            .expect("corpus tile");
                        tiles.push(codes.clone());
                        CanvasOpInput::Tile(codes)
                    }
                    CanvasOp::SurfaceOpen {
                        surface,
                        width,
                        height,
                    } => {
                        sizes.insert(*surface, (*width, *height));
                        CanvasOpInput::None
                    }
                    CanvasOp::SurfacePaint { surface, .. } => {
                        let (width, height) = sizes[surface];
                        seed += 1;
                        let reference = picture(width, height, seed);
                        let input = native(&reference);
                        surfaces.pictures.push_back(Some(reference));
                        input
                    }
                    _ => CanvasOpInput::None,
                });
            }
            oracle
                .execute(&work, &mut tiles, &mut surfaces)
                .expect("oracle executes the corpus");
            let outcome =
                VideoPresenterSink::execute(&mut presenter, &execution(&work, &inputs, true));
            assert_eq!(outcome, CanvasOutcome::Applied, "{what}: no layer, applied");
            let gpu = VideoPresenterSink::take_canvas_readback(&mut presenter)
                .expect("a readback for every record");
            assert_eq!(
                (gpu.monitor_id, gpu.generation, gpu.fence),
                (0, work.generation, work.fence),
                "{what}"
            );
            surface_blocks += compare(&what, &gpu, &oracle, |block| !state.is_lossy(block));
        }
        surface_blocks
    }

    #[test]
    fn the_metal_canvas_equals_the_cpu_executor_on_the_corpus() {
        if !has_metal() {
            return;
        }
        let mut surface_blocks = 0;
        for kind in corpus::SCENARIOS {
            for (width, height) in [(330, 202), (320, 192)] {
                let run = corpus::keel_run(kind, 0, width, height, terms(), 90);
                surface_blocks += replay(&format!("{kind:?} {width}x{height}"), &run.records);
            }
        }
        let dialog = corpus::dialog_run(0, 320, 192, terms());
        assert!(dialog.mix.cache_paint > 0, "{:?}", dialog.mix);
        replay("dialog", &dialog.records);
        assert!(surface_blocks > 0, "the corpus painted surfaces");
        // The video scenario alone exercises surfaces, copies and fills.
        let video = corpus::keel_run(ScenarioKind::Video, 0, 330, 202, terms(), 30);
        assert!(video.mix.surface_paint > 0);
    }

    fn rect(x: u32, y: u32, width: u32, height: u32) -> TileRect {
        TileRect {
            x,
            y,
            width,
            height,
        }
    }

    fn work<'a>(fence: u64, ops: Vec<CanvasOp<'a>>) -> CanvasWork<'a> {
        CanvasWork {
            monitor_id: 3,
            generation: NonZeroU32::new(1).expect("non-zero"),
            fence,
            timestamp_ms: 0,
            ops,
        }
    }

    /// Runs `ops` on both executors with no state owner in between and
    /// compares the whole canvas: exact outside `lossy`, within one code
    /// inside it.
    fn check_ops(
        what: &str,
        executor: &mut MetalCanvasExecutor,
        oracle: &mut CanvasPixels,
        work: &CanvasWork<'_>,
        inputs: &[CanvasOpInput<apple_cf::cv::CVPixelBuffer>],
        pictures: Vec<Option<Picture>>,
        lossy: &[TileRect],
    ) {
        let mut tiles = PackedTileCodes::default();
        for input in inputs {
            if let CanvasOpInput::Tile(codes) = input {
                tiles.push(codes.clone());
            }
        }
        let mut surfaces = ReferenceSurfaces {
            pictures: pictures.into(),
            current: None,
        };
        oracle
            .execute(work, &mut tiles, &mut surfaces)
            .expect("oracle");
        executor
            .execute(&execution(work, inputs, true))
            .expect("metal executor");
        let gpu = executor.take_readback().expect("readback");
        let grid = arcen_media::refine::tile::RefineGrid {
            width: oracle.width(),
            height: oracle.height(),
        };
        compare(what, &gpu, oracle, |index| {
            let block = grid.block_rect(index).expect("block");
            lossy.iter().all(|area| intersect(*area, block).is_none())
        });
    }

    #[test]
    fn every_operation_matches_the_cpu_executor() {
        if !has_metal() {
            return;
        }
        let (width, height) = (197_u32, 133_u32);
        let mut executor = MetalCanvasExecutor::default();
        let mut oracle = CanvasPixels::new();
        let tile = |x: u32, y: u32, w: u32, h: u32, seed: u32| {
            let samples = (0..w * h * 3)
                .map(|at| u16::from(noise(seed ^ at)))
                .collect();
            (
                CanvasOp::Tile {
                    rect: rect(x, y, w, h),
                    payload: &[],
                },
                CanvasOpInput::Tile(TileCodes::new(w, h, samples).expect("codes")),
            )
        };
        // A reset, then a busy picture made of tiles.
        let mut ops = vec![CanvasOp::Reset { width, height }];
        let mut inputs = vec![CanvasOpInput::None];
        for (index, (x, y)) in [(0, 0), (64, 0), (128, 0), (0, 64), (64, 64), (128, 64)]
            .into_iter()
            .enumerate()
        {
            let (op, input) = tile(x, y, 64.min(width - x), 64, index as u32 * 977);
            ops.push(op);
            inputs.push(input);
        }
        let (op, input) = tile(0, 128, width, 5, 4242);
        ops.push(op);
        inputs.push(input);
        let first = work(0, ops);
        check_ops(
            "tiles",
            &mut executor,
            &mut oracle,
            &first,
            &inputs,
            Vec::new(),
            &[],
        );

        // Fills, then overlapping copies in every direction, a disjoint
        // copy, and copies that read what an earlier copy of the same
        // record wrote.
        let ops = vec![
            CanvasOp::Fill {
                rect: rect(5, 7, 30, 11),
                bgra: [10, 200, 30, 255],
            },
            CanvasOp::Copy {
                src: rect(10, 10, 90, 60),
                dst_x: 14,
                dst_y: 13,
            },
            CanvasOp::Copy {
                src: rect(20, 20, 90, 60),
                dst_x: 17,
                dst_y: 16,
            },
            CanvasOp::Copy {
                src: rect(0, 30, 197, 90),
                dst_x: 0,
                dst_y: 8,
            },
            CanvasOp::Copy {
                src: rect(0, 0, 197, 90),
                dst_x: 0,
                dst_y: 41,
            },
            CanvasOp::Copy {
                src: rect(100, 0, 60, 40),
                dst_x: 3,
                dst_y: 90,
            },
            CanvasOp::Copy {
                src: rect(3, 90, 60, 40),
                dst_x: 130,
                dst_y: 1,
            },
            CanvasOp::Fill {
                rect: rect(190, 120, 7, 13),
                bgra: [255, 0, 128, 255],
            },
        ];
        let inputs = vec![
            CanvasOpInput::None,
            CanvasOpInput::None,
            CanvasOpInput::None,
            CanvasOpInput::None,
            CanvasOpInput::None,
            CanvasOpInput::None,
            CanvasOpInput::None,
            CanvasOpInput::None,
        ];
        let second = work(1, ops);
        check_ops(
            "fills and copies",
            &mut executor,
            &mut oracle,
            &second,
            &inputs,
            Vec::new(),
            &[],
        );

        // The cache: store, overwrite a region, paint it back elsewhere,
        // re-store a slot, evict.
        let ops = vec![
            CanvasOp::CacheStore {
                slot: 7,
                rect: rect(40, 30, 50, 40),
            },
            CanvasOp::Fill {
                rect: rect(40, 30, 50, 40),
                bgra: [1, 2, 3, 255],
            },
            CanvasOp::CachePaint {
                slot: 7,
                rect: rect(100, 80, 50, 40),
            },
            CanvasOp::CacheStore {
                slot: 7,
                rect: rect(0, 0, 16, 16),
            },
            CanvasOp::CachePaint {
                slot: 7,
                rect: rect(181, 117, 16, 16),
            },
            CanvasOp::CacheStore {
                slot: 9,
                rect: rect(1, 1, 3, 3),
            },
            CanvasOp::CacheEvict { slot: 9 },
        ];
        let inputs = (0..ops.len())
            .map(|_| CanvasOpInput::None)
            .collect::<Vec<_>>();
        let third = work(2, ops);
        check_ops(
            "cache",
            &mut executor,
            &mut oracle,
            &third,
            &inputs,
            Vec::new(),
            &[],
        );

        // Surfaces: magnified, minified and 1:1, painted only on the
        // listed blocks (one partly outside the destination), and a paint
        // with no picture.
        let magnified = picture(48, 32, 11);
        let minified = picture(96, 64, 12);
        let exact = picture(32, 16, 13);
        let blocks_a = vec![
            rect(16, 16, 16, 16),
            rect(48, 16, 32, 16),
            rect(0, 0, 16, 16),
        ];
        let blocks_b = vec![rect(112, 48, 32, 16), rect(128, 64, 16, 16)];
        let blocks_c = vec![rect(160, 96, 32, 16)];
        let au: &[u8] = &[0, 0, 0, 1, 0x65];
        let ops = vec![
            CanvasOp::SurfacePaint {
                surface: 0,
                idr: true,
                dest: rect(10, 12, 77, 41),
                blocks: blocks_a.clone(),
                access_unit: au,
            },
            CanvasOp::SurfacePaint {
                surface: 1,
                idr: true,
                dest: rect(104, 40, 61, 37),
                blocks: blocks_b.clone(),
                access_unit: au,
            },
            CanvasOp::SurfacePaint {
                surface: 0,
                idr: false,
                dest: rect(160, 96, 32, 16),
                blocks: blocks_c.clone(),
                access_unit: au,
            },
            CanvasOp::SurfacePaint {
                surface: 1,
                idr: false,
                dest: rect(0, 0, 20, 20),
                blocks: vec![rect(0, 0, 16, 16)],
                access_unit: au,
            },
        ];
        let inputs = vec![
            native(&magnified),
            native(&minified),
            native(&exact),
            CanvasOpInput::Surface(None),
        ];
        let lossy: Vec<TileRect> = blocks_a
            .iter()
            .chain(&blocks_b)
            .chain(&blocks_c)
            .copied()
            .collect();
        let fourth = work(3, ops);
        check_ops(
            "surfaces",
            &mut executor,
            &mut oracle,
            &fourth,
            &inputs,
            vec![Some(magnified), Some(minified), Some(exact), None],
            &lossy,
        );
    }

    #[test]
    fn a_failed_operation_is_reported_and_parts_present_only_whole_records() {
        if !has_metal() {
            return;
        }
        let mut executor = MetalCanvasExecutor::default();
        let before_reset = work(
            1,
            vec![CanvasOp::Fill {
                rect: rect(0, 0, 4, 4),
                bgra: [0, 0, 0, 255],
            }],
        );
        assert!(executor
            .execute(&execution(&before_reset, &[CanvasOpInput::None], true))
            .is_err());
        let reset = work(
            0,
            vec![CanvasOp::Reset {
                width: 32,
                height: 32,
            }],
        );
        let canvas = executor
            .execute(&execution(&reset, &[CanvasOpInput::None], true))
            .expect("reset");
        assert!(canvas.with_whole_canvas(|_, _| ()).is_some());
        let part = work(
            1,
            vec![CanvasOp::Fill {
                rect: rect(0, 0, 4, 4),
                bgra: [9, 9, 9, 255],
            }],
        );
        executor
            .execute(&execution(&part, &[CanvasOpInput::None], false))
            .expect("first part");
        assert!(
            canvas.with_whole_canvas(|_, _| ()).is_none(),
            "a record's first part is never presented"
        );
        executor
            .execute(&execution(&part, &[CanvasOpInput::None], true))
            .expect("last part");
        assert!(canvas.with_whole_canvas(|_, _| ()).is_some());
        let missing = work(
            2,
            vec![CanvasOp::CachePaint {
                slot: 1,
                rect: rect(0, 0, 4, 4),
            }],
        );
        assert_eq!(
            executor
                .execute(&execution(&missing, &[CanvasOpInput::None], true))
                .err(),
            Some(CACHE_MISS)
        );
        let misaligned = work(
            3,
            vec![CanvasOp::Fill {
                rect: rect(30, 30, 4, 4),
                bgra: [0, 0, 0, 255],
            }],
        );
        assert_eq!(
            executor
                .execute(&execution(&misaligned, &[CanvasOpInput::None], true))
                .err(),
            Some(OUTSIDE)
        );
        assert_eq!(
            executor.execute(&execution(&misaligned, &[], true)).err(),
            Some(BAD_INPUT)
        );
        // A failure after the record opened encoders drops it uncommitted
        // (its encoders ended), and the next reset starts over.
        let failing = work(
            0,
            vec![
                CanvasOp::Reset {
                    width: 16,
                    height: 16,
                },
                CanvasOp::Fill {
                    rect: rect(0, 0, 8, 8),
                    bgra: [1, 1, 1, 255],
                },
                CanvasOp::CachePaint {
                    slot: 4,
                    rect: rect(0, 0, 4, 4),
                },
            ],
        );
        let inputs = [
            CanvasOpInput::None,
            CanvasOpInput::None,
            CanvasOpInput::None,
        ];
        assert_eq!(
            executor.execute(&execution(&failing, &inputs, true)).err(),
            Some(CACHE_MISS)
        );
        executor
            .execute(&execution(&reset, &[CanvasOpInput::None], true))
            .expect("a reset after a failure");
    }
}
