// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The canvas loop's encoder on every Pier: one shared
//! [`SoftwareH264Session`] per canvas surface, and one at the monitor's
//! size for a Deck that receives full frames (ADR 0028).

use std::time::Duration;

use arcen_keel::BgraFrame;

use super::{
    SoftwareH264Content, SoftwareH264Session, SoftwareRate, SoftwareSessionConfig, StageDamage,
};
use crate::VideoConfiguration;
use crate::canvas::encode_loop::{FullFrameUnit, LoopEncoder};
use crate::canvas::{SurfaceEncodeError, SurfaceEncodeRequest, SurfaceEncoder};

/// Closed sessions kept for reuse (a one-shot picture opens and closes a
/// surface every time; opening `OpenH264` costs milliseconds).
const SPARE_SESSIONS: usize = 2;

/// Software H.264 for the canvas loop.
#[derive(Debug)]
pub struct SoftwareSurfaces {
    surface_video: VideoConfiguration,
    fps: u32,
    threads: u16,
    content: SoftwareH264Content,
    full: Option<SoftwareH264Session>,
    surfaces: Vec<Option<Surface>>,
    spare: Vec<SoftwareH264Session>,
    scratch: Vec<u8>,
}

#[derive(Debug)]
struct Surface {
    session: SoftwareH264Session,
    bitrate_bps: u64,
}

impl SoftwareSurfaces {
    /// Surfaces only (a canvas Deck): pictures in `surface_video`
    /// (`crate::canvas::canvas_surface_video`).
    #[must_use]
    pub fn for_canvas(surface_video: VideoConfiguration, fps: u32) -> Self {
        Self {
            surface_video,
            fps: fps.max(1),
            threads: SoftwareH264Session::host_threads(),
            content: SoftwareH264Content::Motion,
            full: None,
            surfaces: Vec::new(),
            spare: Vec::new(),
            scratch: Vec::new(),
        }
    }

    /// The monitor-sized session of a full-frame Deck.
    #[must_use]
    pub fn for_full_frame(
        session: SoftwareH264Session,
        video: VideoConfiguration,
        fps: u32,
    ) -> Self {
        let mut surfaces = Self::for_canvas(video, fps);
        surfaces.full = Some(session);
        surfaces
    }

    /// Surfaces encode `content` (camera mode, [`SoftwareH264Content::Motion`],
    /// unless told otherwise; Keel sends text as tiles).
    #[must_use]
    pub const fn with_content(mut self, content: SoftwareH264Content) -> Self {
        self.content = content;
        self
    }

    /// The full-frame session, when there is one (bitrate bounds and
    /// threading for the Pier's log).
    #[must_use]
    pub const fn full_session(&self) -> Option<&SoftwareH264Session> {
        self.full.as_ref()
    }

    /// Surfaces open now.
    #[must_use]
    pub fn open_surfaces(&self) -> usize {
        self.surfaces.iter().flatten().count()
    }

    fn take_session(
        &mut self,
        width: u32,
        height: u32,
    ) -> Result<SoftwareH264Session, SurfaceEncodeError> {
        if let Some(index) = self
            .spare
            .iter()
            .position(|session| session.width() == width && session.height() == height)
        {
            let mut session = self.spare.swap_remove(index);
            session.force_idr();
            return Ok(session);
        }
        SoftwareH264Session::new(SoftwareSessionConfig {
            threads: self.threads,
            content: self.content,
            rate: SoftwareRate::Granted,
            ..SoftwareSessionConfig::for_stream(width, height, self.fps, self.surface_video)
        })
        .map_err(|_| SurfaceEncodeError::Open)
    }

    fn park(&mut self, session: SoftwareH264Session) {
        if self.spare.len() >= SPARE_SESSIONS {
            self.spare.remove(0);
        }
        self.spare.push(session);
    }
}

/// Copies `rect` of `source` into `scratch` at `coded` size: rows as they
/// are when the sizes match, nearest-neighbour otherwise (a scaled
/// surface, degradation step c).
fn scaled_rect<'a>(
    source: BgraFrame<'_>,
    rect: crate::refine::tile::TileRect,
    coded: (u32, u32),
    scratch: &'a mut Vec<u8>,
) -> Result<BgraFrame<'a>, SurfaceEncodeError> {
    let (width, height) = (coded.0 as usize, coded.1 as usize);
    let (rx, ry) = (rect.x as usize, rect.y as usize);
    let (rw, rh) = (rect.width as usize, rect.height as usize);
    let grid = source.grid();
    if rw == 0 || rh == 0 || rx + rw > grid.width() || ry + rh > grid.height() {
        return Err(SurfaceEncodeError::Encode);
    }
    let stride = source.stride();
    let pixels = source.pixels();
    scratch.clear();
    scratch.resize(width * height * 4, 0);
    for y in 0..height {
        // The coded picture may be larger than the rect by an odd edge
        // pixel; clamp to the rect's last row and column.
        let sy = ry + (y * rh / height).min(rh - 1);
        let row = &pixels[sy * stride..sy * stride + grid.width() * 4];
        let out = &mut scratch[y * width * 4..(y + 1) * width * 4];
        if width == rw {
            out.copy_from_slice(&row[rx * 4..(rx + rw) * 4]);
        } else {
            for x in 0..width {
                let sx = rx + (x * rw / width).min(rw - 1);
                out[x * 4..x * 4 + 4].copy_from_slice(&row[sx * 4..sx * 4 + 4]);
            }
        }
    }
    BgraFrame::new(scratch, width, height, width * 4).map_err(|_| SurfaceEncodeError::Encode)
}

impl SurfaceEncoder for SoftwareSurfaces {
    fn open(
        &mut self,
        surface: u8,
        coded_width: u32,
        coded_height: u32,
    ) -> Result<(), SurfaceEncodeError> {
        let index = usize::from(surface);
        if self.surfaces.len() <= index {
            self.surfaces.resize_with(index + 1, || None);
        }
        if let Some(previous) = self.surfaces[index].take() {
            self.park(previous.session);
        }
        let session = self.take_session(coded_width, coded_height)?;
        self.surfaces[index] = Some(Surface {
            session,
            bitrate_bps: 0,
        });
        Ok(())
    }

    fn close(&mut self, surface: u8) {
        if let Some(closed) = self
            .surfaces
            .get_mut(usize::from(surface))
            .and_then(Option::take)
        {
            self.park(closed.session);
        }
    }

    fn encode(
        &mut self,
        request: &SurfaceEncodeRequest<'_>,
    ) -> Result<Vec<u8>, SurfaceEncodeError> {
        let mut scratch = std::mem::take(&mut self.scratch);
        let result = (|| {
            let surface = self
                .surfaces
                .get_mut(usize::from(request.surface))
                .and_then(Option::as_mut)
                .ok_or(SurfaceEncodeError::Encode)?;
            if surface.session.width() != request.coded_width
                || surface.session.height() != request.coded_height
            {
                return Err(SurfaceEncodeError::Encode);
            }
            // Spread the grant over the pictures the surface gets.
            let fps = request.quality.fps;
            if fps != 0 && fps != surface.session.encoder_config().fps {
                let _ = surface.session.reconfigure_framerate(fps);
            }
            // Follow the governor's share only when it moved by more than
            // a tenth: every change resets OpenH264's rate control window.
            let wanted = request.quality.bitrate_bps.max(1);
            if surface.bitrate_bps == 0
                || wanted.abs_diff(surface.bitrate_bps) * 10 > surface.bitrate_bps
            {
                let _ = surface.session.reconfigure_bitrate(wanted);
                surface.bitrate_bps = wanted;
            }
            let picture = scaled_rect(
                request.source,
                request.rect,
                (request.coded_width, request.coded_height),
                &mut scratch,
            )?;
            surface
                .session
                .stage(picture, StageDamage::Full)
                .map_err(|_| SurfaceEncodeError::Encode)?;
            if request.idr {
                surface.session.force_idr();
            }
            match surface.session.encode() {
                Ok(Some(unit)) if !unit.bytes.is_empty() => Ok(unit.bytes.to_vec()),
                _ => Err(SurfaceEncodeError::Encode),
            }
        })();
        self.scratch = scratch;
        result
    }
}

impl LoopEncoder for SoftwareSurfaces {
    fn encode_full(
        &mut self,
        frame: BgraFrame<'_>,
        damage: StageDamage<'_>,
        idr: bool,
    ) -> Result<Option<FullFrameUnit>, SurfaceEncodeError> {
        let session = self.full.as_mut().ok_or(SurfaceEncodeError::Encode)?;
        session
            .stage(frame, damage)
            .map_err(|_| SurfaceEncodeError::Encode)?;
        if idr {
            session.force_idr();
        }
        match session.encode() {
            Ok(Some(unit)) => Ok(Some(FullFrameUnit {
                bytes: unit.bytes.to_vec(),
                keyframe: unit.is_keyframe,
            })),
            Ok(None) => Ok(None),
            Err(_) => Err(SurfaceEncodeError::Encode),
        }
    }

    fn set_full_frame_rate(&mut self, fps: u32) {
        if let Some(session) = self.full.as_mut() {
            let _ = session.reconfigure_framerate(fps);
        }
    }

    fn set_full_frame_bitrate(&mut self, bps: u64) {
        if let Some(session) = self.full.as_mut() {
            let _ = session.reconfigure_bitrate(bps);
        }
    }
}

/// What a Pier's software capture asks the shared loop for.
#[derive(Debug, Clone, Copy)]
pub struct SoftwareLoopConfig {
    /// Monitor width.
    pub width: u32,
    /// Monitor height.
    pub height: u32,
    /// Session rate.
    pub fps: u32,
    /// The full-frame stream's coded format (canvas surfaces always use
    /// `crate::canvas::canvas_surface_video`).
    pub video: VideoConfiguration,
    /// The canvas, for a Deck that offered it.
    pub canvas: Option<crate::canvas::serve::CanvasCapencArgs>,
    /// A start bitrate the Pier named, else the Software contract's.
    pub start_bitrate_bps: Option<u64>,
    /// Idle re-send interval.
    pub keepalive: Duration,
}

/// The shared software loop a capture adapter drives, and what it logs.
#[cfg(feature = "refine-zstd-encode")]
#[derive(Debug)]
pub struct SoftwareLoop {
    /// The loop.
    pub encode: crate::canvas::CanvasEncodeLoop<SoftwareSurfaces>,
    /// The Software contract's start and ceiling bitrate.
    pub bitrate_bounds: (u32, u32),
    /// How the encoder spreads over the CPU, for the log.
    pub threading: String,
}

/// Opens the shared software loop: canvas surfaces for a canvas Deck, the
/// monitor-sized session otherwise (ADR 0028). Every Pier's software
/// capture calls this; none builds its own encoder.
///
/// # Errors
///
/// A readable message when `OpenH264`, the tile packer or the planner
/// cannot start.
#[cfg(feature = "refine-zstd-encode")]
pub fn open_software_loop(config: SoftwareLoopConfig) -> Result<SoftwareLoop, String> {
    use crate::canvas::{CanvasEncodeLoop, EncodeLoopConfig, LoopMode};
    let SoftwareLoopConfig {
        width,
        height,
        fps,
        video,
        canvas,
        start_bitrate_bps,
        keepalive,
    } = config;
    let (encoder, bitrate_bounds, threading) = if canvas.is_some() {
        (
            SoftwareSurfaces::for_canvas(crate::canvas::canvas_surface_video(), fps),
            super::software_bitrate_bounds(width, height, fps, video.chroma, video.bit_depth),
            format!("surfaces threads={}", SoftwareH264Session::host_threads()),
        )
    } else {
        let mut session =
            SoftwareH264Session::new(SoftwareSessionConfig::for_stream(width, height, fps, video))
                .map_err(|error| format!("OpenH264 initialization failed: {error}"))?;
        if let Some(bps) = start_bitrate_bps {
            session
                .reconfigure_bitrate(bps)
                .map_err(|error| format!("OpenH264 start bitrate: {error}"))?;
        }
        let bounds = session.bitrate_bounds();
        let threading = session.threading();
        (
            SoftwareSurfaces::for_full_frame(session, video, fps),
            bounds,
            format!("slices={} threads={}", threading.slices, threading.threads),
        )
    };
    let packer = crate::canvas::tile::ZstdTilePacker::new()
        .map_err(|error| format!("canvas tile packer: {error:?}"))?;
    let logical_cores = std::thread::available_parallelism()
        .ok()
        .and_then(|count| u32::try_from(count.get()).ok())
        .unwrap_or(1);
    let encode = CanvasEncodeLoop::new(
        EncodeLoopConfig {
            width,
            height,
            fps,
            keepalive,
            initial_bitrate_bps: start_bitrate_bps.unwrap_or(u64::from(bitrate_bounds.0)),
            mode: canvas.map_or(LoopMode::FullFrame, |canvas| {
                LoopMode::Canvas(canvas.loop_terms(logical_cores))
            }),
        },
        encoder,
        Box::new(packer),
        std::time::Instant::now(),
    )
    .map_err(|error| format!("software loop: {error}"))?;
    Ok(SoftwareLoop {
        encode,
        bitrate_bounds,
        threading,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canvas::{SurfaceQuality, canvas_surface_video};
    use crate::refine::tile::TileRect;

    #[test]
    fn a_surface_takes_the_governors_grant_and_rate() {
        const W: u32 = 1280;
        const H: u32 = 720;
        let (start, _) = crate::video::software_bitrate_bounds(
            W,
            H,
            60,
            crate::ChromaSubsampling::Yuv420,
            crate::BitDepth::Eight,
        );
        // Below the floor the Software contract holds a stream to.
        let grant = u64::from(start / 8);
        let mut surfaces = SoftwareSurfaces::for_canvas(canvas_surface_video(), 60);
        surfaces.open(0, W, H).expect("open");
        let pixels = vec![0x40u8; (W * H * 4) as usize];
        let source =
            BgraFrame::new(&pixels, W as usize, H as usize, W as usize * 4).expect("frame");
        surfaces
            .encode(&SurfaceEncodeRequest {
                surface: 0,
                source,
                rect: TileRect {
                    x: 0,
                    y: 0,
                    width: W,
                    height: H,
                },
                coded_width: W,
                coded_height: H,
                idr: true,
                quality: SurfaceQuality {
                    bitrate_bps: grant,
                    min_qp_raise: 0,
                    fps: 20,
                },
            })
            .expect("encode");
        let session = &surfaces.surfaces[0].as_ref().expect("open").session;
        let config = session.encoder_config();
        assert_eq!(u64::from(config.bitrate_bps), grant);
        assert_eq!(config.fps, 20);
        assert_eq!(config.content, crate::video::SoftwareH264Content::Motion);
    }
}
