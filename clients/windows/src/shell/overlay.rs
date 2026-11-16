// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! egui on the composition tree's overlay visual: a DX12 wgpu swapchain
//! made for composition, premultiplied alpha, so the video visual below
//! shows wherever the UI paints nothing.

use egui_wgpu::{Renderer, RendererOptions, ScreenDescriptor};

/// The overlay's wgpu device, surface and egui renderer.
pub(super) struct OverlayRenderer {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    renderer: Renderer,
}

impl std::fmt::Debug for OverlayRenderer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OverlayRenderer")
            .field("size", &(self.config.width, self.config.height))
            .field("format", &self.config.format)
            .finish_non_exhaustive()
    }
}

impl OverlayRenderer {
    /// Creates the renderer on `visual` (an `IDCompositionVisual*` the tree
    /// keeps alive for the window's life) at `size` physical pixels.
    pub(super) fn new(visual: *mut std::ffi::c_void, size: (u32, u32)) -> Result<Self, String> {
        let mut descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
        descriptor.backends = wgpu::Backends::DX12;
        let instance = wgpu::Instance::new(descriptor);
        // SAFETY: `visual` is the tree's overlay `IDCompositionVisual`; the
        // tree outlives this renderer (the shell drops the renderer first),
        // and wgpu takes its own reference for the surface's life.
        let surface = unsafe {
            instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::CompositionVisual(visual))
        }
        .map_err(|error| format!("overlay surface: {error}"))?;
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: Some(&surface),
        }))
        .map_err(|error| format!("overlay adapter: {error}"))?;
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("arcen-deck overlay"),
            ..wgpu::DeviceDescriptor::default()
        }))
        .map_err(|error| format!("overlay device: {error}"))?;
        let capabilities = surface.get_capabilities(&adapter);
        // egui blends in gamma space and writes premultiplied colour, so the
        // target is a plain 8-bit UNORM format, never sRGB.
        let format = capabilities
            .formats
            .iter()
            .copied()
            .find(|format| !format.is_srgb())
            .or_else(|| capabilities.formats.first().copied())
            .ok_or_else(|| "overlay surface offers no format".to_owned())?;
        let alpha_mode = if capabilities
            .alpha_modes
            .contains(&wgpu::CompositeAlphaMode::PreMultiplied)
        {
            wgpu::CompositeAlphaMode::PreMultiplied
        } else {
            return Err(format!(
                "overlay surface cannot blend over the video (alpha modes {:?})",
                capabilities.alpha_modes
            ));
        };
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.0.max(1),
            height: size.1.max(1),
            present_mode: wgpu::PresentMode::AutoNoVsync,
            desired_maximum_frame_latency: 2,
            alpha_mode,
            view_formats: Vec::new(),
        };
        surface.configure(&device, &config);
        let renderer = Renderer::new(&device, format, RendererOptions::default());
        Ok(Self {
            surface,
            device,
            queue,
            config,
            renderer,
        })
    }

    /// The largest texture egui may upload.
    pub(super) fn max_texture_side(&self) -> usize {
        usize::try_from(self.device.limits().max_texture_dimension_2d).unwrap_or(2048)
    }

    /// Follows the window's client size.
    pub(super) fn resize(&mut self, size: (u32, u32)) {
        let size = (size.0.max(1), size.1.max(1));
        if (self.config.width, self.config.height) != size {
            self.config.width = size.0;
            self.config.height = size.1;
            self.surface.configure(&self.device, &self.config);
        }
    }

    /// Draws one egui pass. Returns `false` when the frame was skipped (the
    /// window is hidden or the surface had to be reconfigured).
    pub(super) fn paint(
        &mut self,
        primitives: &[egui::ClippedPrimitive],
        textures: &egui::TexturesDelta,
        pixels_per_point: f32,
    ) -> bool {
        for (id, delta) in &textures.set {
            self.renderer
                .update_texture(&self.device, &self.queue, *id, delta);
        }
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame)
            | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => frame,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.device, &self.config);
                self.free(textures);
                return false;
            }
            _ => {
                self.free(textures);
                return false;
            }
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let screen = ScreenDescriptor {
            size_in_pixels: [self.config.width, self.config.height],
            pixels_per_point,
        };
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("arcen-deck overlay"),
            });
        let extra = self.renderer.update_buffers(
            &self.device,
            &self.queue,
            &mut encoder,
            primitives,
            &screen,
        );
        {
            let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("arcen-deck overlay"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        // Transparent: the video visual shows through.
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..wgpu::RenderPassDescriptor::default()
            });
            let mut pass = pass.forget_lifetime();
            self.renderer.render(&mut pass, primitives, &screen);
        }
        self.queue
            .submit(extra.into_iter().chain(std::iter::once(encoder.finish())));
        frame.present();
        self.free(textures);
        true
    }

    fn free(&mut self, textures: &egui::TexturesDelta) {
        for id in &textures.free {
            self.renderer.free_texture(id);
        }
    }
}
