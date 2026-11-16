// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The Deck-owned DirectComposition tree of one session window, and the
//! host-cursor layer.
//!
//! wgpu's own HWND target would leave the video above egui; so the Deck
//! owns the target and gives the `ui` shell the overlay visual
//! (`wgpu::SurfaceTargetUnsafe::CompositionVisual`). The target is topmost:
//! above the window's redirection surface, which the shell keeps for the
//! native menu bar and fills only where the tree is clear (letterbox). With
//! a caller-owned visual wgpu does not commit: the shell calls
//! [`CompositionTree::commit`] after every configure/resize.
//!
//! Order, bottom to top: video, host cursor, overlay. The host cursor is
//! part of the remote desktop (below the Deck's own UI) and moves by a
//! composition offset and a commit — no video or UI redraw.
//!
//! **The overlay covers only what the Deck draws** ([`OverlayRegion`]):
//! clipped to the bounds of egui's shapes, and out of the tree while there
//! are none. Every full-window layer DWM blends brings its composition
//! deadline forward: on the Windows lab the full-window overlay over the
//! window's GDI surface (kept for the native menu bar) moved it from ~3 ms
//! to ~8 ms before vsync, +5–6 ms present to glass on average
//! (`files/windeck/present/REPORT.md`, phase-swept A/B).

use super::device::Gpu;
use super::swapchain::VideoSwapchain;
use super::{Result, hr};
use windows::Win32::Foundation::{HWND, POINT};
use windows::Win32::Graphics::Direct3D11::{D3D11_BOX, ID3D11Texture2D};
use windows::Win32::Graphics::DirectComposition::*;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_PREMULTIPLIED, DXGI_FORMAT_B8G8R8A8_UNORM,
};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::core::Interface;

/// Where the source picture lands in the client area: uniform scale,
/// centred (letterbox / pillarbox), in client pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VideoFit {
    pub scale: f32,
    pub offset_x: f32,
    pub offset_y: f32,
}

impl VideoFit {
    /// Fits `source` into `client` keeping its aspect ratio. A source that
    /// equals the client is placed 1:1 (pixel exact).
    #[must_use]
    #[allow(clippy::cast_precision_loss, reason = "pixel sizes are far below 2^24")]
    pub fn new(source: (u32, u32), client: (u32, u32)) -> Self {
        let (sw, sh) = (source.0.max(1) as f32, source.1.max(1) as f32);
        let (cw, ch) = (client.0 as f32, client.1 as f32);
        let scale = (cw / sw).min(ch / sh);
        Self {
            scale,
            offset_x: ((cw - sw * scale) / 2.0).round(),
            offset_y: ((ch - sh * scale) / 2.0).round(),
        }
    }

    /// A source pixel position in client pixels.
    #[must_use]
    pub fn to_client(self, x: f32, y: f32) -> (f32, f32) {
        (
            self.offset_x + x * self.scale,
            self.offset_y + y * self.scale,
        )
    }
}

pub struct CompositionTree {
    pub device: IDCompositionDevice,
    _target: IDCompositionTarget,
    root: IDCompositionVisual,
    pub video: IDCompositionVisual,
    pub cursor: IDCompositionVisual,
    pub overlay: IDCompositionVisual,
    scale: IDCompositionScaleTransform,
    translate: IDCompositionTranslateTransform,
    overlay_attached: bool,
    overlay_clip: IDCompositionRectangleClip,
    /// The clip on the overlay visual, in client pixels.
    overlay_bounds: Option<PixelRect>,
    fit: Option<VideoFit>,
    client: (u32, u32),
    source: (u32, u32),
    /// Which presenter's swapchain the video visual shows.
    shown: Option<usize>,
    /// The window covers its monitor (no menu bar, no frame).
    fullscreen: bool,
    /// The last independent-flip verdict logged.
    flip_verdict: Option<&'static str>,
}

// SAFETY: DirectComposition objects are not thread-affine; the tree is
// shared between the UI thread and the presenter threads behind a mutex
// ([`SharedTree`]), so calls never overlap.
unsafe impl Send for CompositionTree {}

/// The tree, shared by the UI thread (resize, overlay, cursor) and the
/// presenter threads (which swapchain the video visual shows).
pub type SharedTree = std::sync::Arc<std::sync::Mutex<CompositionTree>>;

impl CompositionTree {
    /// Builds the tree on `gpu`'s device for `hwnd`'s client area, above
    /// the window's own (GDI) surface when it has one: the Deck window keeps
    /// it for the native menu bar.
    pub fn new(gpu: &Gpu, hwnd: HWND) -> Result<Self> {
        let dxgi: IDXGIDevice = hr("IDXGIDevice", gpu.device.cast())?;
        let device: IDCompositionDevice = hr("DCompositionCreateDevice", unsafe {
            DCompositionCreateDevice(&dxgi)
        })?;
        let target = hr("CreateTargetForHwnd", unsafe {
            device.CreateTargetForHwnd(hwnd, true)
        })?;
        let root = hr("CreateVisual(root)", unsafe { device.CreateVisual() })?;
        let video = hr("CreateVisual(video)", unsafe { device.CreateVisual() })?;
        let cursor = hr("CreateVisual(cursor)", unsafe { device.CreateVisual() })?;
        let overlay = hr("CreateVisual(overlay)", unsafe { device.CreateVisual() })?;
        hr("AddVisual(video)", unsafe {
            root.AddVisual(&video, false, None)
        })?;
        hr("AddVisual(cursor)", unsafe {
            root.AddVisual(&cursor, true, &video)
        })?;
        hr("AddVisual(overlay)", unsafe {
            root.AddVisual(&overlay, true, &cursor)
        })?;
        let scale = hr("CreateScaleTransform", unsafe {
            device.CreateScaleTransform()
        })?;
        let translate = hr("CreateTranslateTransform", unsafe {
            device.CreateTranslateTransform()
        })?;
        let group = hr("CreateTransformGroup", unsafe {
            device.CreateTransformGroup(&[
                Some(scale.cast().map_err(|e| e.to_string())?),
                Some(translate.cast().map_err(|e| e.to_string())?),
            ])
        })?;
        hr("SetTransform(video)", unsafe { video.SetTransform(&group) })?;
        let overlay_clip = hr("CreateRectangleClip(overlay)", unsafe {
            device.CreateRectangleClip()
        })?;
        let _ = unsafe {
            video.SetBitmapInterpolationMode(DCOMPOSITION_BITMAP_INTERPOLATION_MODE_LINEAR)
        };
        hr("SetRoot", unsafe { target.SetRoot(&root) })?;
        Ok(Self {
            device,
            _target: target,
            root,
            video,
            cursor,
            overlay,
            scale,
            translate,
            overlay_attached: true,
            overlay_clip,
            overlay_bounds: None,
            fit: None,
            client: (1, 1),
            source: (1, 1),
            shown: None,
            fullscreen: false,
            flip_verdict: None,
        })
    }

    /// Shows `swapchain` in the video visual.
    pub fn set_video(&self, swapchain: &VideoSwapchain) -> Result<()> {
        hr("SetContent(video)", unsafe {
            self.video.SetContent(&swapchain.swapchain)
        })
    }

    /// Places the source picture in the client area.
    pub fn fit_video(&mut self, source: (u32, u32), client: (u32, u32)) -> Result<VideoFit> {
        let fit = VideoFit::new(source, client);
        if self.fit != Some(fit) {
            unsafe {
                hr("SetScaleX", self.scale.SetScaleX2(fit.scale))?;
                hr("SetScaleY", self.scale.SetScaleY2(fit.scale))?;
                hr("SetOffsetX", self.translate.SetOffsetX2(fit.offset_x))?;
                hr("SetOffsetY", self.translate.SetOffsetY2(fit.offset_y))?;
            }
            // Pixel exact at 1:1: no filtering.
            let mode = if (fit.scale - 1.0).abs() < f32::EPSILON {
                DCOMPOSITION_BITMAP_INTERPOLATION_MODE_NEAREST_NEIGHBOR
            } else {
                DCOMPOSITION_BITMAP_INTERPOLATION_MODE_LINEAR
            };
            let _ = unsafe { self.video.SetBitmapInterpolationMode(mode) };
            self.fit = Some(fit);
        }
        Ok(fit)
    }

    /// Shows the swapchain of presenter `owner` at `source` size, fitted to
    /// the client area, and commits.
    pub fn show_swapchain(
        &mut self,
        owner: usize,
        swapchain: &VideoSwapchain,
        source: (u32, u32),
    ) -> Result<()> {
        if self.shown != Some(owner) {
            self.set_video(swapchain)?;
            self.shown = Some(owner);
        }
        self.source = source;
        self.fit_video(source, self.client)?;
        self.commit()?;
        self.note_arrangement();
        Ok(())
    }

    /// Whether the video visual shows presenter `owner`'s swapchain.
    #[must_use]
    pub fn shows(&self, owner: usize) -> bool {
        self.shown == Some(owner)
    }

    /// Clears the video visual if presenter `owner` shows it, and commits.
    pub fn hide_swapchain(&mut self, owner: usize) -> Result<()> {
        if self.shown == Some(owner) {
            hr("SetContent(video, none)", unsafe {
                self.video.SetContent(None)
            })?;
            self.shown = None;
            self.commit()?;
            self.note_arrangement();
        }
        Ok(())
    }

    /// The window's client area changed (resize, DPI, monitor move).
    pub fn set_client_size(&mut self, client: (u32, u32)) -> Result<VideoFit> {
        self.client = client;
        let fit = self.fit_video(self.source, client)?;
        self.commit()?;
        self.note_arrangement();
        Ok(fit)
    }

    /// The window enters or leaves fullscreen. Its new client area follows
    /// with [`Self::set_client_size`], which logs the verdict.
    pub fn set_fullscreen(&mut self, fullscreen: bool) {
        self.fullscreen = fullscreen;
    }

    /// What DWM is offered: the video alone, 1:1 over the whole monitor, is
    /// what it can flip without composing ([`super::flip`]).
    #[must_use]
    pub fn arrangement(&self) -> super::flip::Arrangement {
        super::flip::Arrangement {
            fullscreen: self.fullscreen,
            fit: self.shown.and(self.fit),
            overlay_attached: self.overlay_attached,
        }
    }

    /// Logs the independent-flip verdict when it changes.
    fn note_arrangement(&mut self) {
        let verdict = self.arrangement().describe();
        if self.flip_verdict != Some(verdict) {
            self.flip_verdict = Some(verdict);
            tracing::info!(
                target: arcen_deck_core::log_target::VIDEO,
                fullscreen = self.fullscreen,
                client = ?self.client,
                source = ?self.source,
                overlay = self.overlay_attached,
                "video arrangement: {verdict}"
            );
        }
    }

    #[must_use]
    pub fn fit(&self) -> Option<VideoFit> {
        self.fit
    }

    /// The fit of the picture a presenter shows now, with that picture's
    /// size (`None` while no presenter shows one: before a session's first
    /// frame, the fit is of a placeholder or an earlier session).
    #[must_use]
    pub fn shown_fit(&self) -> Option<(VideoFit, (u32, u32))> {
        self.shown?;
        Some((self.fit?, self.source))
    }

    /// The raw `IDCompositionVisual*` for
    /// `wgpu::SurfaceTargetUnsafe::CompositionVisual`.
    #[must_use]
    pub fn overlay_visual_raw(&self) -> *mut std::ffi::c_void {
        self.overlay.as_raw()
    }

    /// Detaches the overlay when no UI is visible, so a fullscreen video
    /// swapchain can be promoted to independent flip / an overlay plane.
    pub fn set_overlay_attached(&mut self, attached: bool) -> Result<()> {
        if attached == self.overlay_attached {
            return Ok(());
        }
        if attached {
            hr("AddVisual(overlay)", unsafe {
                self.root.AddVisual(&self.overlay, true, &self.cursor)
            })?;
        } else {
            hr("RemoveVisual(overlay)", unsafe {
                self.root.RemoveVisual(&self.overlay)
            })?;
        }
        self.overlay_attached = attached;
        Ok(())
    }

    /// Shows the overlay visual only where the Deck draws: clipped to
    /// `region`'s bounds, or out of the tree when it is empty. Commits a
    /// change.
    ///
    /// Call it with the union of the shown and the next pass's region before
    /// presenting the next pass, and with the next pass's own region after
    /// (an empty region only once its cleared frame was presented, so a
    /// returning overlay never flashes stale content).
    pub fn set_overlay_region(&mut self, region: OverlayRegion) -> Result<()> {
        let mut changed = false;
        if let Some(bounds) = region.bounds {
            if self.overlay_bounds != Some(bounds) {
                unsafe {
                    hr(
                        "SetLeft(overlay clip)",
                        self.overlay_clip.SetLeft2(bounds.left),
                    )?;
                    hr(
                        "SetTop(overlay clip)",
                        self.overlay_clip.SetTop2(bounds.top),
                    )?;
                    hr(
                        "SetRight(overlay clip)",
                        self.overlay_clip.SetRight2(bounds.right),
                    )?;
                    hr(
                        "SetBottom(overlay clip)",
                        self.overlay_clip.SetBottom2(bounds.bottom),
                    )?;
                    if self.overlay_bounds.is_none() {
                        hr("SetClip(overlay)", self.overlay.SetClip(&self.overlay_clip))?;
                    }
                }
                self.overlay_bounds = Some(bounds);
                changed = true;
            }
            if !self.overlay_attached {
                self.set_overlay_attached(true)?;
                changed = true;
            }
        } else if self.overlay_attached {
            self.set_overlay_attached(false)?;
            changed = true;
        }
        if changed {
            tracing::debug!(
                target: arcen_deck_core::log_target::VIDEO,
                attached = self.overlay_attached,
                bounds = ?self.overlay_bounds,
                "overlay visual region"
            );
            self.commit()?;
            self.note_arrangement();
        }
        Ok(())
    }

    pub fn commit(&self) -> Result<()> {
        hr("Commit", unsafe { self.device.Commit() })
    }
}

/// A cursor surface with its size and hotspot.
type CursorSurface = (IDCompositionSurface, (u32, u32), (i32, i32));

/// A rectangle in client pixels (edges, exclusive right/bottom).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PixelRect {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

impl PixelRect {
    #[must_use]
    pub fn union(self, other: Self) -> Self {
        Self {
            left: self.left.min(other.left),
            top: self.top.min(other.top),
            right: self.right.max(other.right),
            bottom: self.bottom.max(other.bottom),
        }
    }
}

/// What the Deck's own UI covers in one pass: the bounds of everything egui
/// tessellated, in client pixels, or nothing.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct OverlayRegion {
    pub bounds: Option<PixelRect>,
}

impl OverlayRegion {
    /// Antialiasing feathers a shape by up to a pixel outside its bounds.
    const FEATHER: f32 = 2.0;

    /// The region `primitives` cover in a `client`-pixel area.
    #[must_use]
    pub fn of(
        primitives: &[egui::ClippedPrimitive],
        pixels_per_point: f32,
        client: (u32, u32),
    ) -> Self {
        let mut points: Option<egui::Rect> = None;
        for clipped in primitives {
            let shape = match &clipped.primitive {
                egui::epaint::Primitive::Mesh(mesh) if !mesh.indices.is_empty() => {
                    mesh.calc_bounds()
                }
                egui::epaint::Primitive::Callback(callback) => callback.rect,
                egui::epaint::Primitive::Mesh(_) => continue,
            };
            let visible = shape.intersect(clipped.clip_rect);
            if visible.is_positive() {
                points = Some(points.map_or(visible, |bounds| bounds.union(visible)));
            }
        }
        let bounds = points.and_then(|rect| {
            let (width, height) = (client.0 as f32, client.1 as f32);
            let left = (rect.min.x * pixels_per_point - Self::FEATHER)
                .floor()
                .max(0.0);
            let top = (rect.min.y * pixels_per_point - Self::FEATHER)
                .floor()
                .max(0.0);
            let right = (rect.max.x * pixels_per_point + Self::FEATHER)
                .ceil()
                .min(width);
            let bottom = (rect.max.y * pixels_per_point + Self::FEATHER)
                .ceil()
                .min(height);
            (right > left && bottom > top).then_some(PixelRect {
                left,
                top,
                right,
                bottom,
            })
        });
        Self { bounds }
    }

    /// Both regions (the overlay while one pass replaces the other).
    #[must_use]
    pub fn union(self, other: Self) -> Self {
        Self {
            bounds: match (self.bounds, other.bounds) {
                (Some(a), Some(b)) => Some(a.union(b)),
                (a, b) => a.or(b),
            },
        }
    }
}

/// The host cursor (or the Native-tablet pen dot) as its own visual.
pub struct CursorLayer {
    surface: Option<CursorSurface>,
    visible: bool,
}

impl Default for CursorLayer {
    fn default() -> Self {
        Self::new()
    }
}

impl CursorLayer {
    #[must_use]
    pub fn new() -> Self {
        Self {
            surface: None,
            visible: false,
        }
    }

    /// Sets the cursor image: premultiplied BGRA8, `width × height`, with
    /// its hotspot.
    pub fn set_image(
        &mut self,
        gpu: &Gpu,
        tree: &CompositionTree,
        bgra_premultiplied: &[u8],
        size: (u32, u32),
        hotspot: (i32, i32),
    ) -> Result<()> {
        let (width, height) = size;
        if width == 0 || height == 0 || bgra_premultiplied.len() < (width * height * 4) as usize {
            return Err("cursor image size mismatch".into());
        }
        let surface = match &self.surface {
            Some((surface, existing, _)) if *existing == size => surface.clone(),
            _ => hr("CreateSurface(cursor)", unsafe {
                tree.device.CreateSurface(
                    width,
                    height,
                    DXGI_FORMAT_B8G8R8A8_UNORM,
                    DXGI_ALPHA_MODE_PREMULTIPLIED,
                )
            })?,
        };
        let _context = gpu.lock_context();
        let mut offset = POINT::default();
        let texture: ID3D11Texture2D = hr("BeginDraw(cursor)", unsafe {
            surface.BeginDraw(None, &mut offset)
        })?;
        let region = D3D11_BOX {
            left: offset.x as u32,
            top: offset.y as u32,
            front: 0,
            right: offset.x as u32 + width,
            bottom: offset.y as u32 + height,
            back: 1,
        };
        unsafe {
            gpu.context.UpdateSubresource(
                &texture,
                0,
                Some(&region),
                bgra_premultiplied.as_ptr().cast(),
                width * 4,
                0,
            );
        }
        hr("EndDraw(cursor)", unsafe { surface.EndDraw() })?;
        hr("SetContent(cursor)", unsafe {
            tree.cursor.SetContent(&surface)
        })?;
        self.surface = Some((surface, size, hotspot));
        self.visible = true;
        Ok(())
    }

    /// Moves the hotspot to `(x, y)` client pixels; commit to show.
    #[allow(clippy::cast_precision_loss, reason = "hotspots are tiny")]
    pub fn set_position(&self, tree: &CompositionTree, x: f32, y: f32) -> Result<()> {
        let Some((_, _, hotspot)) = &self.surface else {
            return Ok(());
        };
        unsafe {
            hr(
                "SetOffsetX(cursor)",
                tree.cursor.SetOffsetX2((x - hotspot.0 as f32).round()),
            )?;
            hr(
                "SetOffsetY(cursor)",
                tree.cursor.SetOffsetY2((y - hotspot.1 as f32).round()),
            )
        }
    }

    /// Shows or hides the cursor (the Deck's own cursor took over, or the
    /// host hid its pointer).
    pub fn set_visible(&mut self, tree: &CompositionTree, visible: bool) -> Result<()> {
        if visible == self.visible {
            return Ok(());
        }
        let opacity = if visible { 1.0 } else { 0.0 };
        if let Ok(visual3) = tree.cursor.cast::<IDCompositionVisual3>() {
            hr("SetOpacity(cursor)", unsafe {
                visual3.SetOpacity2(opacity)
            })?;
        } else if visible {
            if let Some((surface, ..)) = &self.surface {
                hr("SetContent(cursor)", unsafe {
                    tree.cursor.SetContent(surface)
                })?;
            }
        } else {
            hr("SetContent(cursor)", unsafe {
                tree.cursor.SetContent(None)
            })?;
        }
        self.visible = visible;
        Ok(())
    }
}

super::opaque_debug!(CompositionTree, CursorLayer);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_same_size_source_is_placed_one_to_one() {
        assert_eq!(
            VideoFit::new((1800, 1130), (1800, 1130)),
            VideoFit {
                scale: 1.0,
                offset_x: 0.0,
                offset_y: 0.0
            }
        );
    }

    fn mesh_at(rect: egui::Rect, clip: egui::Rect) -> egui::ClippedPrimitive {
        let mut mesh = egui::epaint::Mesh::default();
        mesh.add_colored_rect(rect, egui::Color32::WHITE);
        egui::ClippedPrimitive {
            clip_rect: clip,
            primitive: egui::epaint::Primitive::Mesh(mesh),
        }
    }

    #[test]
    fn nothing_drawn_takes_the_overlay_out_of_the_tree() {
        assert_eq!(OverlayRegion::of(&[], 1.5, (1800, 1130)).bounds, None);
        let everywhere = egui::Rect::EVERYTHING;
        let empty = egui::ClippedPrimitive {
            clip_rect: everywhere,
            primitive: egui::epaint::Primitive::Mesh(egui::epaint::Mesh::default()),
        };
        assert_eq!(OverlayRegion::of(&[empty], 1.5, (1800, 1130)).bounds, None);
        // Clipped away entirely.
        let hidden = mesh_at(
            egui::Rect::from_min_max(egui::pos2(10.0, 10.0), egui::pos2(20.0, 20.0)),
            egui::Rect::from_min_max(egui::pos2(100.0, 100.0), egui::pos2(200.0, 200.0)),
        );
        assert_eq!(OverlayRegion::of(&[hidden], 1.0, (800, 600)).bounds, None);
    }

    #[test]
    fn the_overlay_is_clipped_to_what_egui_drew_in_client_pixels() {
        // A statistics panel in the bottom-left corner at 150 % scale.
        let panel = mesh_at(
            egui::Rect::from_min_max(egui::pos2(8.0, 700.0), egui::pos2(354.0, 745.0)),
            egui::Rect::EVERYTHING,
        );
        let region = OverlayRegion::of(&[panel], 1.5, (1800, 1130));
        assert_eq!(
            region.bounds,
            Some(PixelRect {
                left: 10.0,
                top: 1048.0,
                right: 533.0,
                bottom: 1120.0,
            })
        );
        // Never outside the client area.
        let off = mesh_at(
            egui::Rect::from_min_max(egui::pos2(-50.0, -50.0), egui::pos2(900.0, 700.0)),
            egui::Rect::EVERYTHING,
        );
        let clamped = OverlayRegion::of(&[off], 1.0, (800, 600)).bounds;
        assert_eq!(
            clamped,
            Some(PixelRect {
                left: 0.0,
                top: 0.0,
                right: 800.0,
                bottom: 600.0,
            })
        );
        // While one pass replaces another, the overlay covers both.
        let both = region.union(OverlayRegion::default());
        assert_eq!(both, region);
    }

    #[test]
    fn a_wider_source_is_letterboxed_and_centred() {
        let fit = VideoFit::new((1920, 1080), (1000, 1000));
        assert!((fit.scale - 1000.0 / 1920.0).abs() < 1e-6);
        assert!(fit.offset_x.abs() < f32::EPSILON);
        assert!((fit.offset_y - 219.0).abs() < f32::EPSILON);
        let (x, y) = fit.to_client(1920.0, 1080.0);
        assert!(
            (x - 1000.0).abs() < 1e-3 && (y - 781.5).abs() < 1e-3,
            "{x} {y}"
        );
    }
}
