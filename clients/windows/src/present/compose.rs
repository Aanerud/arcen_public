// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! One presented frame: refinement composition (domain guard, overlay
//! writes before the draw, readbacks), the conversion draw, and the
//! presented fence reported once that frame is confirmed on screen
//! (`arcen_deck_core::present::RefineFrameTag` rule 3).

use super::Result;
use super::convert::{ConvertUniform, Converter, OutputKind, RefineDomain, SourcePlanes};
use super::device::Gpu;
use super::picture::D3d11Picture;
use super::refine::{D3d11RefineOverlay, PendingReadback};
pub use arcen_deck_core::present::RefineFrameTag;
use arcen_media::refine::compositor::{OverlayWork, composites_over, inside, needs_composite};
use arcen_media::refine::tile::RefineGrid;
use arcen_media::video::present::{VideoColorContract, VideoUniform};
use std::collections::VecDeque;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;

/// One frame to draw: the decoder's picture, its colour contract and its
/// Exact still image tag (`None` on the eight-bit path, ADR 0016 R7).
#[derive(Debug, Clone)]
pub struct DecodedFrame {
    pub picture: D3d11Picture,
    pub contract: VideoColorContract,
    pub refine: Option<RefineFrameTag>,
}

impl DecodedFrame {
    /// The frame the media worker handed over.
    #[must_use]
    pub fn from_presentable(
        frame: &arcen_deck_core::present::PresentableFrame<D3d11Picture>,
    ) -> Self {
        Self {
            picture: frame.picture.clone(),
            contract: frame.contract.into(),
            refine: frame.refine.clone(),
        }
    }
}

/// Whether the present that showed a frame is known to be on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confirmation {
    /// DXGI frame statistics count it as displayed.
    Displayed,
    /// Not yet.
    Pending,
    /// Statistics are unavailable (DWM reported them disjoint); the GPU
    /// finished the frame and the swapchain's latency-1 wait released it.
    Unconfirmed,
}

struct InFlight {
    query: Option<ID3D11Query>,
    present_id: Option<u32>,
    tag: Option<RefineFrameTag>,
    readbacks: Vec<PendingReadback>,
    _keep: Vec<ID3D11Texture2D>,
}

pub struct FrameComposer {
    converter: Converter,
    overlay: D3d11RefineOverlay,
    in_flight: VecDeque<InFlight>,
    /// The visible picture, copied out when it cannot be sampled in place.
    scratch: Option<ID3D11Texture2D>,
    pub refine_refusals: u64,
}

/// Whether a generation's tiles may be composited over this frame (the
/// shared domain guard).
pub fn refine_matches(
    tag: &RefineFrameTag,
    contract: VideoColorContract,
    source: RefineGrid,
) -> bool {
    composites_over(
        tag.hub.grid(tag.monitor_id),
        tag.hub.contract(tag.monitor_id),
        contract,
        source,
    )
}

impl FrameComposer {
    pub fn new(gpu: &Gpu) -> Result<Self> {
        Ok(Self {
            converter: Converter::new(gpu)?,
            overlay: D3d11RefineOverlay::new(),
            in_flight: VecDeque::new(),
            scratch: None,
            refine_refusals: 0,
        })
    }

    /// Draws `frame` into `target` (source-sized). Call [`Self::submitted`]
    /// after the frame was presented (or the target otherwise consumed).
    #[allow(
        clippy::too_many_lines,
        reason = "one draw: refinement writes, plane views, uniforms and the pass, in order"
    )]
    pub fn draw(
        &mut self,
        gpu: &Gpu,
        frame: &DecodedFrame,
        target: &ID3D11RenderTargetView,
        output: OutputKind,
    ) -> Result<()> {
        if let Some(fence) = &frame.picture.fence {
            // The decoder's work, GPU-side, before anything samples it.
            let context4: ID3D11DeviceContext4 = super::hr(
                "ID3D11DeviceContext4",
                windows::core::Interface::cast(&gpu.context),
            )?;
            super::hr("Wait(decode fence)", unsafe {
                context4.Wait(fence, frame.picture.fence_value)
            })?;
        }
        let planes = SourcePlanes::of_picture(gpu, &frame.picture, &mut self.scratch)?;
        let uniform = ConvertUniform::new(
            VideoUniform::from_contract(frame.contract, planes.luma_size, planes.chroma_size),
            output,
        );
        let source_grid = RefineGrid {
            width: planes.luma_size.0,
            height: planes.luma_size.1,
        };
        let tag = frame.refine.as_ref().filter(|tag| {
            let ok = refine_matches(tag, frame.contract, source_grid);
            if !ok && tag.hub.grid(tag.monitor_id).is_some() {
                self.refine_refusals += 1;
            }
            ok
        });
        let mut work = OverlayWork::default();
        let active = tag.is_some_and(|tag| tag.hub.compose(&mut work, tag.monitor_id, tag.fence));
        let domain = tag.and_then(|tag| {
            tag.hub.contract(tag.monitor_id).map(|bytes| {
                if bytes.domain == arcen_protocol::refine::RefineDomain::YCbCr.wire() {
                    RefineDomain::YCbCr
                } else {
                    RefineDomain::Rgb
                }
            })
        });
        // Overlay writes go on this context before the draw that samples
        // them.
        let overlay_view = if tag.is_some() {
            self.overlay.encode_updates(gpu, &work, source_grid)
        } else {
            None
        };
        if let Some(tag) = tag.filter(|_| active && overlay_view.is_none()) {
            tag.hub
                .decoder_reset(tag.monitor_id, "overlay_upload_failed");
        }
        let refined = active && overlay_view.is_some();
        let overlay = match (refined, &overlay_view, domain) {
            (true, Some(view), Some(domain)) => Some((view, domain)),
            _ => None,
        };
        self.converter
            .draw(gpu, &planes, uniform, target, overlay)?;
        let mut readbacks = Vec::new();
        let mut keep: Vec<ID3D11Texture2D> = Vec::new();
        if let Some((view, domain)) = overlay {
            let block_count = source_grid.block_count() as usize;
            for request in std::mem::take(&mut work.readbacks) {
                // Never more than the picture's own blocks (the hub asks
                // for a verdict's blocks; a request beyond that is refused
                // rather than sized from untrusted counts).
                if request.blocks.len() > block_count || !inside(&request.blocks, source_grid) {
                    continue;
                }
                let source = if needs_composite(&request) {
                    if let Some(composite) = keep.first() {
                        // One unscaled composite per frame serves every
                        // level-3 request.
                        composite.clone()
                    } else {
                        // An unscaled composite drawn exactly as the swapchain
                        // was (the codes, never scRGB).
                        let composite = gpu.texture(
                            source_grid.width,
                            source_grid.height,
                            DXGI_FORMAT_R10G10B10A2_UNORM,
                            D3D11_BIND_RENDER_TARGET,
                            None,
                        )?;
                        let rtv = gpu.rtv(&composite)?;
                        self.converter.draw(
                            gpu,
                            &planes,
                            ConvertUniform {
                                output: OutputKind::Codes,
                                ..uniform
                            },
                            &rtv,
                            Some((view, domain)),
                        )?;
                        keep.push(composite.clone());
                        composite
                    }
                } else {
                    match self.overlay.texture() {
                        Some(texture) => texture.clone(),
                        None => continue,
                    }
                };
                if let Ok(pending) = D3d11RefineOverlay::encode_readback(gpu, &source, request) {
                    readbacks.push(pending);
                }
            }
        }
        // A frame drawn but never presented is forgotten, never reported.
        self.abandon_unsubmitted();
        self.in_flight.push_back(InFlight {
            query: None,
            present_id: None,
            tag: tag.cloned(),
            readbacks,
            _keep: keep,
        });
        Ok(())
    }

    /// The frame drawn last was presented as present number `present_id`
    /// (`IDXGISwapChain::GetLastPresentCount`, `None` off a swapchain):
    /// fence it.
    pub fn submitted(&mut self, gpu: &Gpu, present_id: Option<u32>) -> Result<()> {
        if let Some(last) = self.in_flight.back_mut()
            && last.query.is_none()
        {
            let query = gpu.event_query()?;
            unsafe { gpu.context.End(&query) };
            last.query = Some(query);
            last.present_id = present_id;
        }
        Ok(())
    }

    /// Reports frames that reached the screen: `presented`, then their
    /// readbacks. `confirm` says whether a present number is displayed.
    /// Returns `(confirmed, unconfirmed)` counts.
    pub fn poll(&mut self, gpu: &Gpu, confirm: impl Fn(u32) -> Confirmation) -> (u64, u64) {
        let mut confirmed = 0;
        let mut unconfirmed = 0;
        while let Some(front) = self.in_flight.front() {
            let Some(query) = front.query.as_ref() else {
                break;
            };
            if !gpu.query_done(query) {
                break;
            }
            match front.present_id.map_or(Confirmation::Unconfirmed, &confirm) {
                Confirmation::Pending => break,
                Confirmation::Displayed => confirmed += 1,
                Confirmation::Unconfirmed => unconfirmed += 1,
            }
            let Some(done) = self.in_flight.pop_front() else {
                break;
            };
            if let Some(tag) = done.tag {
                tag.hub.presented(tag.monitor_id, tag.fence);
                for pending in done.readbacks {
                    if let Ok(result) = pending.complete(gpu) {
                        let _ = tag.hub.complete_readback(&result);
                    }
                }
            }
        }
        (confirmed, unconfirmed)
    }

    /// Forgets the last drawn frame if it was never presented (its present
    /// failed): its fence is never reported and later frames are not held
    /// behind it.
    pub fn abandon_unsubmitted(&mut self) {
        if self
            .in_flight
            .back()
            .is_some_and(|last| last.query.is_none())
        {
            self.in_flight.pop_back();
        }
    }

    /// Forgets frames not yet confirmed (teardown): their fences are never
    /// reported presented.
    pub fn discard(&mut self) {
        self.in_flight.clear();
    }

    pub fn in_flight(&self) -> usize {
        self.in_flight.len()
    }
}

super::opaque_debug!(FrameComposer);
