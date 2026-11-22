// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Paints a [`Capture`] offscreen with egui-wgpu — the renderer both Decks
//! draw their UI with, with the same options, into an eight-bit UNORM
//! target as their surfaces are — reads it back and writes a PNG.

use std::path::Path;

use super::capture::Capture;

/// The texture format both Decks' egui targets use (UNORM: egui blends in
/// gamma space and writes premultiplied colour).
const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

/// A GPU device for offscreen rendering, with no window or surface.
pub struct Gpu {
    device: wgpu::Device,
    queue: wgpu::Queue,
    adapter: String,
}

impl std::fmt::Debug for Gpu {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Gpu")
            .field("adapter", &self.adapter)
            .finish_non_exhaustive()
    }
}

impl Gpu {
    /// The first adapter any compiled-in backend offers (Metal on a Mac,
    /// DirectX 12 on Windows), without a display.
    ///
    /// # Errors
    ///
    /// No backend or adapter (a build without one, or no GPU).
    pub fn new() -> Result<Self, String> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: None,
        }))
        .map_err(|error| format!("no GPU adapter for offscreen rendering: {error}"))?;
        let info = adapter.get_info();
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("arcen-deck doc-shots"),
            ..wgpu::DeviceDescriptor::default()
        }))
        .map_err(|error| format!("GPU device: {error}"))?;
        Ok(Self {
            device,
            queue,
            adapter: format!("{} ({:?})", info.name, info.backend),
        })
    }

    /// The adapter's name and backend.
    #[must_use]
    pub fn adapter(&self) -> &str {
        &self.adapter
    }

    /// Paints `capture` at `size` pixels and returns its straight-alpha
    /// RGBA8 pixels, row by row.
    ///
    /// # Errors
    ///
    /// The read-back failed.
    pub fn render(&self, capture: &Capture, size: [u32; 2]) -> Result<Vec<u8>, String> {
        let mut renderer =
            egui_wgpu::Renderer::new(&self.device, FORMAT, egui_wgpu::RendererOptions::default());
        // Every texture as the window had it by the last pass: each pass's
        // uploads, then its frees, in order.
        for delta in &capture.textures {
            for (id, image) in &delta.set {
                renderer.update_texture(&self.device, &self.queue, *id, image);
            }
            for id in &delta.free {
                renderer.free_texture(id);
            }
        }
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("arcen-deck doc-shots target"),
            size: wgpu::Extent3d {
                width: size[0],
                height: size[1],
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let screen = egui_wgpu::ScreenDescriptor {
            size_in_pixels: size,
            pixels_per_point: capture.pixels_per_point,
        };
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("arcen-deck doc-shots"),
            });
        let extra = renderer.update_buffers(
            &self.device,
            &self.queue,
            &mut encoder,
            &capture.primitives,
            &screen,
        );
        {
            let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("arcen-deck doc-shots"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..wgpu::RenderPassDescriptor::default()
            });
            let mut pass = pass.forget_lifetime();
            renderer.render(&mut pass, &capture.primitives, &screen);
        }
        let mut pixels = self.read_back(encoder, extra, &texture, size)?;
        unpremultiply(&mut pixels);
        Ok(pixels)
    }

    /// Submits `encoder` (and egui's `extra` buffers) with a copy of
    /// `texture` into a mappable buffer, waits, and returns its RGBA8 rows
    /// without the row padding.
    fn read_back(
        &self,
        mut encoder: wgpu::CommandEncoder,
        extra: Vec<wgpu::CommandBuffer>,
        texture: &wgpu::Texture,
        size: [u32; 2],
    ) -> Result<Vec<u8>, String> {
        let unpadded = size[0] * 4;
        let padded = unpadded.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
            * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("arcen-deck doc-shots read-back"),
            size: u64::from(padded) * u64::from(size[1]),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded),
                    rows_per_image: Some(size[1]),
                },
            },
            wgpu::Extent3d {
                width: size[0],
                height: size[1],
                depth_or_array_layers: 1,
            },
        );
        self.queue
            .submit(extra.into_iter().chain(std::iter::once(encoder.finish())));
        let slice = buffer.slice(..);
        let (sender, receiver) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = sender.send(result);
        });
        self.device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|error| format!("GPU wait: {error}"))?;
        receiver
            .recv()
            .map_err(|error| format!("read-back: {error}"))?
            .map_err(|error| format!("read-back: {error}"))?;
        let mapped = slice.get_mapped_range();
        let mut pixels = Vec::with_capacity((unpadded * size[1]) as usize);
        for row in mapped.chunks(padded as usize) {
            pixels.extend_from_slice(&row[..unpadded as usize]);
        }
        drop(mapped);
        buffer.unmap();
        Ok(pixels)
    }
}

/// egui writes premultiplied colour; a PNG holds straight alpha. The
/// launcher's opaque background makes this a no-op wherever it paints.
fn unpremultiply(pixels: &mut [u8]) {
    for pixel in pixels.chunks_exact_mut(4) {
        let alpha = u32::from(pixel[3]);
        if alpha != 0 && alpha != 255 {
            for channel in &mut pixel[..3] {
                let straight = (u32::from(*channel) * 255 + alpha / 2) / alpha;
                *channel = u8::try_from(straight.min(255)).unwrap_or(u8::MAX);
            }
        }
    }
}

/// Writes RGBA8 `pixels` of `size` as a PNG.
///
/// # Errors
///
/// The file could not be written.
pub fn write_png(path: &Path, size: [u32; 2], pixels: &[u8]) -> Result<(), String> {
    let file =
        std::fs::File::create(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), size[0], size[1]);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    // Both Decks' surfaces are shown as sRGB.
    encoder.set_source_srgb(png::SrgbRenderingIntent::Perceptual);
    let mut writer = encoder
        .write_header()
        .map_err(|error| format!("{}: {error}", path.display()))?;
    writer
        .write_image_data(pixels)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    writer
        .finish()
        .map_err(|error| format!("{}: {error}", path.display()))
}
