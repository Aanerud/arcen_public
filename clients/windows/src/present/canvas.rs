// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The eight-bit presenter's canvas executor (ADR 0028): the D3D11 twin of
//! `arcen_media::canvas::CanvasPixels`, which is its oracle.
//!
//! One persistent `B8G8R8A8_UNORM` canvas per monitor on the presenter's
//! own device, written in operation order on its immediate context:
//!
//! | Operation | D3D11 |
//! | --- | --- |
//! | `Reset` | (re)create the texture, `ClearRenderTargetView` opaque black, empty cache |
//! | `Fill` | instanced rectangles (`shaders/canvas.hlsl` `ps_fill`), consecutive fills in one draw |
//! | `Copy` | `CopySubresourceRegion`; overlapping moves through a scratch texture |
//! | `Tile` | `UpdateSubresource` of the tile's BGRA8 codes |
//! | `CacheStore` / `CachePaint` / `CacheEvict` | one texture per slot, `CopySubresourceRegion` both ways |
//! | `SurfacePaint` | the conversion pass (`present.hlsl`, unchanged) of the decoder's picture into a picture-sized BGRA8 texture, then instanced block spans (`ps_paint`) scaled by nearest sample into the canvas |
//!
//! A surface picture decoded on the decoder's own device is read through
//! the picture hand-over ([`super::handover`]), one reader per surface.
//! Every write is a command on the immediate context queued before the
//! present that copies the canvas into the swapchain (the `RefineFrameTag`
//! R2 rule: no CPU write into a texture a GPU command in flight may read).
//! There is no decision here: the shared state owner validated every
//! rectangle, slot and surface.

use super::convert::{ConvertUniform, Converter, OutputKind, SourcePlanes};
use super::device::Gpu;
use super::handover::SharedReads;
use super::picture::D3d11Picture;
use super::{Result, created, hr};
use arcen_deck_core::present::{CanvasExecution, CanvasOpInput, CanvasReadback};
use arcen_media::canvas::CanvasOp;
use arcen_media::refine::tile::TileRect;
use arcen_media::video::present::VideoUniform;
use arcen_protocol::canvas::MAX_SURFACES_PER_MONITOR;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use windows::Win32::Graphics::Direct3D::{
    D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST, D3D11_SRV_DIMENSION_BUFFER,
};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;

macro_rules! blob {
    ($name:literal) => {
        include_bytes!(concat!(env!("OUT_DIR"), "/present/", $name, ".cso"))
    };
}

/// Largest canvas side: the D3D11 texture limit at feature level 11.
pub const MAX_CANVAS_PX: u32 = D3D11_REQ_TEXTURE2D_U_OR_V_DIMENSION;

const CANVAS_FORMAT: DXGI_FORMAT = DXGI_FORMAT_B8G8R8A8_UNORM;

/// One rectangle of `shaders/canvas.hlsl`'s `CanvasRect`: x, y, width,
/// height, then a fill's colour as `0xAARRGGBB` and padding.
type RectItem = [u32; 8];
const RECT_BYTES: u32 = 32;
const PASS_BYTES: u32 = 32;

/// The canvas the presenter thread shows: a monitor's texture, and whether
/// a record is half applied (its parts arrive in several
/// [`D3d11CanvasExecutor::execute`] calls; the canvas is shown only whole).
#[derive(Clone)]
pub struct CanvasPicture {
    pub texture: ID3D11Texture2D,
    pub size: (u32, u32),
    partial: Arc<AtomicBool>,
}

impl CanvasPicture {
    /// Whether every applied record is complete. Read under the device's
    /// context lock, which every canvas write holds.
    #[must_use]
    pub fn is_whole(&self) -> bool {
        !self.partial.load(Ordering::Acquire)
    }
}

struct CachedRect {
    texture: ID3D11Texture2D,
    width: u32,
    height: u32,
}

/// One monitor's canvas.
struct MonitorCanvas {
    texture: ID3D11Texture2D,
    rtv: ID3D11RenderTargetView,
    width: u32,
    height: u32,
    partial: Arc<AtomicBool>,
    cache: HashMap<u32, CachedRect>,
    /// The hand-over reader of each surface's pictures (each surface has
    /// a decoder, and so a picture pool and fence, of its own).
    reads: Vec<SharedReads>,
}

impl MonitorCanvas {
    fn picture(&self) -> CanvasPicture {
        CanvasPicture {
            texture: self.texture.clone(),
            size: (self.width, self.height),
            partial: Arc::clone(&self.partial),
        }
    }

    fn inside(&self, rect: TileRect) -> bool {
        rect.width > 0
            && rect.height > 0
            && rect
                .x
                .checked_add(rect.width)
                .is_some_and(|right| right <= self.width)
            && rect
                .y
                .checked_add(rect.height)
                .is_some_and(|bottom| bottom <= self.height)
    }

    fn checked(&self, rect: TileRect) -> Result<()> {
        if self.inside(rect) {
            Ok(())
        } else {
            Err(format!("canvas rectangle {rect:?} outside the canvas"))
        }
    }
}

/// The canvas passes' shaders and state, created on first use.
struct Passes {
    vs: ID3D11VertexShader,
    fill: ID3D11PixelShader,
    paint: ID3D11PixelShader,
    constants: ID3D11Buffer,
    rasterizer: ID3D11RasterizerState,
    /// The rectangle buffer, its view and its capacity in rectangles.
    rects: Option<(ID3D11Buffer, ID3D11ShaderResourceView, u32)>,
}

/// A picture-sized BGRA8 texture the conversion pass draws a surface into.
struct SurfaceTarget {
    rtv: ID3D11RenderTargetView,
    srv: ID3D11ShaderResourceView,
    size: (u32, u32),
}

/// The canvas executor of one eight-bit presenter (one per window, on its
/// device).
#[derive(Default)]
pub struct D3d11CanvasExecutor {
    canvases: HashMap<u16, MonitorCanvas>,
    passes: Option<Passes>,
    converter: Option<Converter>,
    /// Overlapping copies go through it (grows, never shrinks).
    copy_scratch: Option<(ID3D11Texture2D, u32, u32)>,
    /// A surface picture copied out when it cannot be sampled in place.
    picture_scratch: Option<ID3D11Texture2D>,
    surface: Option<SurfaceTarget>,
    readbacks: VecDeque<CanvasReadback>,
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

impl Passes {
    fn new(gpu: &Gpu) -> Result<Self> {
        let vs_bytes: &[u8] = blob!("vs_rect");
        if cfg!(arcen_shaders_missing) || vs_bytes.is_empty() {
            return Err(
                "the canvas shaders were not compiled into this build (no fxc.exe at build time)"
                    .into(),
            );
        }
        let mut vs = None;
        hr("CreateVertexShader(vs_rect)", unsafe {
            gpu.device.CreateVertexShader(vs_bytes, None, Some(&mut vs))
        })?;
        let desc = D3D11_BUFFER_DESC {
            ByteWidth: PASS_BYTES,
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
            StructureByteStride: 0,
        };
        let mut constants = None;
        hr("CreateBuffer(canvas pass)", unsafe {
            gpu.device.CreateBuffer(&desc, None, Some(&mut constants))
        })?;
        let raster = D3D11_RASTERIZER_DESC {
            FillMode: D3D11_FILL_SOLID,
            CullMode: D3D11_CULL_NONE,
            DepthClipEnable: true.into(),
            ..Default::default()
        };
        let mut rasterizer = None;
        hr("CreateRasterizerState(canvas)", unsafe {
            gpu.device
                .CreateRasterizerState(&raster, Some(&mut rasterizer))
        })?;
        Ok(Self {
            vs: created("CreateVertexShader(vs_rect)", vs)?,
            fill: pixel_shader(gpu, "ps_fill", blob!("ps_fill"))?,
            paint: pixel_shader(gpu, "ps_paint", blob!("ps_paint"))?,
            constants: created("CreateBuffer(canvas pass)", constants)?,
            rasterizer: created("CreateRasterizerState(canvas)", rasterizer)?,
            rects: None,
        })
    }

    /// The rectangle buffer's view, holding at least `count` rectangles.
    fn rect_view(&mut self, gpu: &Gpu, count: u32) -> Result<ID3D11ShaderResourceView> {
        if let Some((_, view, capacity)) = &self.rects
            && *capacity >= count
        {
            return Ok(view.clone());
        }
        let capacity = count.next_power_of_two().max(64);
        let desc = D3D11_BUFFER_DESC {
            ByteWidth: capacity * RECT_BYTES,
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: D3D11_RESOURCE_MISC_BUFFER_STRUCTURED.0 as u32,
            StructureByteStride: RECT_BYTES,
        };
        let mut buffer = None;
        hr("CreateBuffer(canvas rectangles)", unsafe {
            gpu.device.CreateBuffer(&desc, None, Some(&mut buffer))
        })?;
        let buffer = created("CreateBuffer(canvas rectangles)", buffer)?;
        let view_desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
            Format: DXGI_FORMAT_UNKNOWN,
            ViewDimension: D3D11_SRV_DIMENSION_BUFFER,
            Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                Buffer: D3D11_BUFFER_SRV {
                    Anonymous1: D3D11_BUFFER_SRV_0 { FirstElement: 0 },
                    Anonymous2: D3D11_BUFFER_SRV_1 {
                        NumElements: capacity,
                    },
                },
            },
        };
        let mut view = None;
        hr("CreateShaderResourceView(canvas rectangles)", unsafe {
            gpu.device
                .CreateShaderResourceView(&buffer, Some(&view_desc), Some(&mut view))
        })?;
        let view = created("CreateShaderResourceView(canvas rectangles)", view)?;
        self.rects = Some((buffer, view.clone(), capacity));
        Ok(view)
    }

    /// Draws `items` into `canvas` with `pixel` (fill or paint);
    /// `picture` is the paint's source and `dest` its destination.
    fn draw(
        &mut self,
        gpu: &Gpu,
        canvas: &MonitorCanvas,
        paint: Option<(&ID3D11ShaderResourceView, (u32, u32), TileRect)>,
        items: &[RectItem],
    ) -> Result<()> {
        if items.is_empty() {
            return Ok(());
        }
        let count = u32::try_from(items.len()).map_err(|_| "too many canvas rectangles")?;
        let view = self.rect_view(gpu, count)?;
        let Some((buffer, _, _)) = &self.rects else {
            return Err("no canvas rectangle buffer".into());
        };
        let bytes: Vec<u8> = items
            .iter()
            .flatten()
            .flat_map(|word| word.to_le_bytes())
            .collect();
        let region = D3D11_BOX {
            left: 0,
            top: 0,
            front: 0,
            right: count * RECT_BYTES,
            bottom: 1,
            back: 1,
        };
        let (dest, picture_size) = paint.map_or(
            (
                TileRect {
                    x: 0,
                    y: 0,
                    width: 1,
                    height: 1,
                },
                (1, 1),
            ),
            |(_, size, dest)| (dest, size),
        );
        let pass: [u32; 8] = [
            dest.x,
            dest.y,
            dest.width,
            dest.height,
            picture_size.0,
            picture_size.1,
            canvas.width,
            canvas.height,
        ];
        let pass_bytes: Vec<u8> = pass.iter().flat_map(|word| word.to_le_bytes()).collect();
        let context = &gpu.context;
        unsafe {
            context.UpdateSubresource(buffer, 0, Some(&region), bytes.as_ptr().cast(), 0, 0);
            context.UpdateSubresource(&self.constants, 0, None, pass_bytes.as_ptr().cast(), 0, 0);
            context.OMSetRenderTargets(Some(&[Some(canvas.rtv.clone())]), None);
            context.OMSetBlendState(None::<&ID3D11BlendState>, None, u32::MAX);
            context.OMSetDepthStencilState(None::<&ID3D11DepthStencilState>, 0);
            context.RSSetViewports(Some(&[D3D11_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: canvas.width as f32,
                Height: canvas.height as f32,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            }]));
            context.RSSetState(&self.rasterizer);
            context.IASetInputLayout(None);
            context.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            context.VSSetShader(&self.vs, None);
            context.PSSetShader(
                if paint.is_some() {
                    &self.paint
                } else {
                    &self.fill
                },
                None,
            );
            context.VSSetConstantBuffers(0, Some(&[Some(self.constants.clone())]));
            context.PSSetConstantBuffers(0, Some(&[Some(self.constants.clone())]));
            context.VSSetShaderResources(0, Some(&[Some(view)]));
            context.PSSetShaderResources(1, Some(&[paint.map(|(srv, _, _)| srv.clone())]));
            context.DrawInstanced(6, count, 0, 0);
            // Unbind, so the picture can be drawn into again and the
            // canvas copied.
            context.VSSetShaderResources(0, Some(&[None]));
            context.PSSetShaderResources(0, Some(&[None, None]));
            context.OMSetRenderTargets(None, None);
        }
        Ok(())
    }
}

/// Canvas rectangle of a fill: `[b, g, r, 255]` packed as `0xAARRGGBB`.
fn fill_item(rect: TileRect, bgra: [u8; 4]) -> RectItem {
    let colour = u32::from_le_bytes(bgra);
    [rect.x, rect.y, rect.width, rect.height, colour, 0, 0, 0]
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

fn region(rect: TileRect) -> D3D11_BOX {
    D3D11_BOX {
        left: rect.x,
        top: rect.y,
        front: 0,
        right: rect.x + rect.width,
        bottom: rect.y + rect.height,
        back: 1,
    }
}

impl D3d11CanvasExecutor {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Applies one record's work (or part of it) to its monitor's canvas,
    /// in order, then reads the canvas back when asked. Returns the canvas
    /// to present. A failure leaves the canvas partly updated: the Deck
    /// asks the Pier for a reset.
    pub fn execute(
        &mut self,
        gpu: &Gpu,
        execution: &CanvasExecution<'_, '_, D3d11Picture>,
    ) -> Result<CanvasPicture> {
        let work = execution.work;
        if execution.inputs.len() != work.ops.len() {
            return Err("canvas inputs do not match the operations".into());
        }
        let picture = {
            let _context = gpu.lock_context();
            // Half applied until every operation succeeded: a record that
            // fails part-way is never shown (the reset that repairs it is).
            if let Some(canvas) = self.canvases.get(&work.monitor_id) {
                canvas.partial.store(true, Ordering::Release);
            }
            let mut fills: Vec<RectItem> = Vec::new();
            for (op, input) in work.ops.iter().zip(execution.inputs) {
                if let CanvasOp::Fill { rect, bgra } = op {
                    let canvas = self.canvas(work.monitor_id)?;
                    canvas.checked(*rect)?;
                    fills.push(fill_item(*rect, *bgra));
                    continue;
                }
                self.flush_fills(gpu, work.monitor_id, &mut fills)?;
                self.op(gpu, work.monitor_id, op, input)?;
            }
            self.flush_fills(gpu, work.monitor_id, &mut fills)?;
            let canvas = self.canvas(work.monitor_id)?;
            // Under the context lock, as every write: the presenter thread
            // never copies a half-applied record.
            canvas.partial.store(!execution.present, Ordering::Release);
            canvas.picture()
        };
        if execution.readback {
            let (width, height) = picture.size;
            let bgra = gpu.read_region(&picture.texture, 0, 0, width, height, 4)?;
            self.readbacks.push_back(CanvasReadback {
                monitor_id: work.monitor_id,
                generation: work.generation,
                fence: work.fence,
                stride: width as usize * 4,
                bgra,
            });
        }
        Ok(picture)
    }

    /// A canvas read back for an earlier [`CanvasExecution::readback`].
    pub fn take_readback(&mut self) -> Option<CanvasReadback> {
        self.readbacks.pop_front()
    }

    /// Teardown: the GPU passed every read of a surface picture (bounded
    /// wait), then every canvas is released.
    pub fn clear(&mut self) {
        for canvas in self.canvases.values() {
            for reads in &canvas.reads {
                reads.finish(std::time::Duration::from_millis(200));
            }
        }
        self.canvases.clear();
        self.readbacks.clear();
    }

    fn canvas(&self, monitor_id: u16) -> Result<&MonitorCanvas> {
        self.canvases
            .get(&monitor_id)
            .ok_or_else(|| format!("canvas operation before a reset (monitor {monitor_id})"))
    }

    fn passes(&mut self, gpu: &Gpu) -> Result<&mut Passes> {
        if self.passes.is_none() {
            self.passes = Some(Passes::new(gpu)?);
        }
        self.passes
            .as_mut()
            .ok_or_else(|| "no canvas passes".into())
    }

    fn flush_fills(&mut self, gpu: &Gpu, monitor_id: u16, fills: &mut Vec<RectItem>) -> Result<()> {
        if fills.is_empty() {
            return Ok(());
        }
        self.passes(gpu)?;
        let (Some(passes), Some(canvas)) = (self.passes.as_mut(), self.canvases.get(&monitor_id))
        else {
            return Err(format!("canvas fill before a reset (monitor {monitor_id})"));
        };
        let drawn = passes.draw(gpu, canvas, None, fills);
        fills.clear();
        drawn
    }

    fn reset(&mut self, gpu: &Gpu, monitor_id: u16, width: u32, height: u32) -> Result<()> {
        if width == 0 || height == 0 || width > MAX_CANVAS_PX || height > MAX_CANVAS_PX {
            return Err(format!("canvas size {width}x{height} out of range"));
        }
        let reuse = self
            .canvases
            .get(&monitor_id)
            .is_some_and(|canvas| (canvas.width, canvas.height) == (width, height));
        if reuse {
            if let Some(canvas) = self.canvases.get_mut(&monitor_id) {
                canvas.cache.clear();
            }
        } else {
            // A presenter thread may still hold the old texture; it is
            // released with its last reference.
            let texture =
                gpu.texture(width, height, CANVAS_FORMAT, D3D11_BIND_RENDER_TARGET, None)?;
            let rtv = gpu.rtv(&texture)?;
            let reads = self.canvases.remove(&monitor_id).map_or_else(
                || {
                    (0..MAX_SURFACES_PER_MONITOR)
                        .map(|_| SharedReads::default())
                        .collect()
                },
                |old| old.reads,
            );
            self.canvases.insert(
                monitor_id,
                MonitorCanvas {
                    texture,
                    rtv,
                    width,
                    height,
                    partial: Arc::new(AtomicBool::new(true)),
                    cache: HashMap::new(),
                    reads,
                },
            );
        }
        let canvas = self.canvas(monitor_id)?;
        unsafe {
            gpu.context
                .ClearRenderTargetView(&canvas.rtv, &[0.0, 0.0, 0.0, 1.0]);
        }
        Ok(())
    }

    fn copy(&mut self, gpu: &Gpu, monitor_id: u16, src: TileRect, dst: TileRect) -> Result<()> {
        let canvas = self.canvas(monitor_id)?;
        canvas.checked(src)?;
        canvas.checked(dst)?;
        let texture = canvas.texture.clone();
        if intersect(src, dst).is_none() {
            unsafe {
                gpu.context.CopySubresourceRegion(
                    &texture,
                    0,
                    dst.x,
                    dst.y,
                    0,
                    &texture,
                    0,
                    Some(&region(src)),
                );
            }
            return Ok(());
        }
        // Overlapping: as if through a temporary.
        let fits = self
            .copy_scratch
            .as_ref()
            .is_some_and(|(_, w, h)| *w >= src.width && *h >= src.height);
        if !fits {
            let (w, h) = self
                .copy_scratch
                .as_ref()
                .map_or((0, 0), |(_, w, h)| (*w, *h));
            let (w, h) = (w.max(src.width), h.max(src.height));
            let scratch = gpu.texture(w, h, CANVAS_FORMAT, D3D11_BIND_SHADER_RESOURCE, None)?;
            self.copy_scratch = Some((scratch, w, h));
        }
        let Some((scratch, _, _)) = &self.copy_scratch else {
            return Err("no canvas copy scratch".into());
        };
        let staged = TileRect { x: 0, y: 0, ..src };
        unsafe {
            gpu.context
                .CopySubresourceRegion(scratch, 0, 0, 0, 0, &texture, 0, Some(&region(src)));
            gpu.context.CopySubresourceRegion(
                &texture,
                0,
                dst.x,
                dst.y,
                0,
                scratch,
                0,
                Some(&region(staged)),
            );
        }
        Ok(())
    }

    fn tile(
        &self,
        gpu: &Gpu,
        monitor_id: u16,
        rect: TileRect,
        input: &CanvasOpInput<D3d11Picture>,
    ) -> Result<()> {
        let canvas = self.canvas(monitor_id)?;
        canvas.checked(rect)?;
        let CanvasOpInput::Tile(codes) = input else {
            return Err("a tile without its decoded codes".into());
        };
        if (codes.width(), codes.height()) != (rect.width, rect.height) {
            return Err("tile codes of another size".into());
        }
        let bgra =
            arcen_deck_core::canvas::tile_bgra(codes).ok_or("tile codes outside eight bits")?;
        unsafe {
            gpu.context.UpdateSubresource(
                &canvas.texture,
                0,
                Some(&region(rect)),
                bgra.as_ptr().cast(),
                rect.width * 4,
                0,
            );
        }
        Ok(())
    }

    fn cache_store(&mut self, gpu: &Gpu, monitor_id: u16, slot: u32, rect: TileRect) -> Result<()> {
        let canvas = self.canvas(monitor_id)?;
        canvas.checked(rect)?;
        let texture = gpu.texture(
            rect.width,
            rect.height,
            CANVAS_FORMAT,
            D3D11_BIND_SHADER_RESOURCE,
            None,
        )?;
        unsafe {
            gpu.context.CopySubresourceRegion(
                &texture,
                0,
                0,
                0,
                0,
                &canvas.texture,
                0,
                Some(&region(rect)),
            );
        }
        if let Some(canvas) = self.canvases.get_mut(&monitor_id) {
            canvas.cache.insert(
                slot,
                CachedRect {
                    texture,
                    width: rect.width,
                    height: rect.height,
                },
            );
        }
        Ok(())
    }

    fn cache_paint(&self, gpu: &Gpu, monitor_id: u16, slot: u32, rect: TileRect) -> Result<()> {
        let canvas = self.canvas(monitor_id)?;
        canvas.checked(rect)?;
        let entry = canvas
            .cache
            .get(&slot)
            .ok_or_else(|| format!("cache slot {slot} holds nothing"))?;
        if (entry.width, entry.height) != (rect.width, rect.height) {
            return Err(format!("cache slot {slot} holds another size"));
        }
        let whole = TileRect { x: 0, y: 0, ..rect };
        unsafe {
            gpu.context.CopySubresourceRegion(
                &canvas.texture,
                0,
                rect.x,
                rect.y,
                0,
                &entry.texture,
                0,
                Some(&region(whole)),
            );
        }
        Ok(())
    }

    fn op(
        &mut self,
        gpu: &Gpu,
        monitor_id: u16,
        op: &CanvasOp<'_>,
        input: &CanvasOpInput<D3d11Picture>,
    ) -> Result<()> {
        match op {
            CanvasOp::Reset { width, height } => self.reset(gpu, monitor_id, *width, *height),
            CanvasOp::Fill { rect, bgra } => {
                let mut fills = vec![fill_item(*rect, *bgra)];
                self.canvas(monitor_id)?.checked(*rect)?;
                self.flush_fills(gpu, monitor_id, &mut fills)
            }
            CanvasOp::Copy { src, dst_x, dst_y } => {
                let dst = TileRect {
                    x: *dst_x,
                    y: *dst_y,
                    ..*src
                };
                self.copy(gpu, monitor_id, *src, dst)
            }
            CanvasOp::Tile { rect, .. } => self.tile(gpu, monitor_id, *rect, input),
            CanvasOp::CacheStore { slot, rect } => self.cache_store(gpu, monitor_id, *slot, *rect),
            CanvasOp::CachePaint { slot, rect } => self.cache_paint(gpu, monitor_id, *slot, *rect),
            CanvasOp::CacheEvict { slot } => {
                let canvas = self
                    .canvases
                    .get_mut(&monitor_id)
                    .ok_or("canvas operation before a reset")?;
                canvas
                    .cache
                    .remove(slot)
                    .map(|_| ())
                    .ok_or_else(|| format!("cache slot {slot} holds nothing"))
            }
            // The media worker owns the surface decoders; the executor
            // only samples their pictures.
            CanvasOp::SurfaceOpen { .. } | CanvasOp::SurfaceClose { .. } => {
                self.canvas(monitor_id).map(|_| ())
            }
            CanvasOp::SurfacePaint {
                surface,
                dest,
                blocks,
                ..
            } => {
                self.canvas(monitor_id)?.checked(*dest)?;
                match input {
                    // The access unit produced no picture: nothing painted.
                    CanvasOpInput::Surface(None) => Ok(()),
                    CanvasOpInput::Surface(Some(frame)) => {
                        let contract = frame.video.into();
                        self.paint(
                            gpu,
                            monitor_id,
                            *surface,
                            &frame.picture,
                            contract,
                            *dest,
                            blocks,
                        )
                    }
                    _ => Err("a surface paint without its picture".into()),
                }
            }
        }
    }

    /// Converts `picture` as the eight-bit presenter does, then writes the
    /// listed block spans (clipped to `dest`) from it, scaled into `dest`
    /// by nearest sample.
    #[allow(
        clippy::too_many_arguments,
        reason = "one paint: the canvas, the surface's picture and where it lands"
    )]
    fn paint(
        &mut self,
        gpu: &Gpu,
        monitor_id: u16,
        surface: u8,
        picture: &D3d11Picture,
        contract: arcen_media::video::present::VideoColorContract,
        dest: TileRect,
        blocks: &[TileRect],
    ) -> Result<()> {
        let spans: Vec<RectItem> = blocks
            .iter()
            .filter_map(|span| intersect(*span, dest))
            .map(|span| [span.x, span.y, span.width, span.height, 0, 0, 0, 0])
            .collect();
        if self.converter.is_none() {
            self.converter = Some(Converter::new(gpu)?);
        }
        self.passes(gpu)?;
        let Self {
            canvases,
            passes,
            converter,
            picture_scratch,
            surface: target,
            ..
        } = self;
        let (Some(canvas), Some(passes), Some(converter)) = (
            canvases.get_mut(&monitor_id),
            passes.as_mut(),
            converter.as_ref(),
        ) else {
            return Err("canvas paint before a reset".into());
        };
        let reads = canvas
            .reads
            .get_mut(usize::from(surface))
            .ok_or_else(|| format!("surface {surface} beyond the surface limit"))?;
        // The decoder's work, GPU-side, before the conversion samples it; a
        // picture from the decoder's own device is opened on this one.
        let sampled = reads.acquire(gpu, picture)?;
        let drawn_size = (|| -> Result<(u32, u32)> {
            let planes = SourcePlanes::of_picture(gpu, &sampled, picture_scratch)?;
            let size = planes.luma_size;
            if target.as_ref().is_none_or(|target| target.size != size) {
                let texture = gpu.texture(
                    size.0,
                    size.1,
                    CANVAS_FORMAT,
                    D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE,
                    None,
                )?;
                *target = Some(SurfaceTarget {
                    rtv: gpu.rtv(&texture)?,
                    srv: gpu.srv_2d(&texture, CANVAS_FORMAT)?,
                    size,
                });
            }
            let Some(target) = target.as_ref() else {
                return Err("no surface target".into());
            };
            let uniform = ConvertUniform::new(
                VideoUniform::from_contract(contract, planes.luma_size, planes.chroma_size),
                OutputKind::Codes,
            );
            converter.draw(gpu, &planes, uniform, &target.rtv, None)?;
            Ok(size)
        })();
        // After every draw that read it, failed or not.
        let released = reads.release(gpu, picture);
        let size = drawn_size?;
        released?;
        let Some(target) = target.as_ref() else {
            return Err("no surface target".into());
        };
        passes.draw(gpu, canvas, Some((&target.srv, size, dest)), &spans)
    }
}

super::opaque_debug!(D3d11CanvasExecutor, CanvasPicture);

#[cfg(test)]
mod tests;
