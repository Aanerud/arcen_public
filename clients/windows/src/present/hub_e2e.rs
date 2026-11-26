// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! End to end on the GPU (tests and the presenter spike; needs the Pier's
//! `refine-zstd-encode` feature of `arcen-media`, so it is never in a
//! release Deck): a Pier refinement scheduler and Zstandard tile
//! encoder produce the real wire records for a still 10-bit picture; the
//! Deck's shared `DeckRefineHub` admits, decodes and verifies them; the
//! D3D11 `FrameComposer` composites, presents and reads back. Passes when
//! the shared status text says the whole picture is verified at the top
//! level the domain allows.

#![allow(
    clippy::expect_used,
    clippy::missing_panics_doc,
    reason = "a GPU test harness (tests and the spike only): a failed step is the test's failure"
)]

use super::Result;
use super::compose::{DecodedFrame, FrameComposer, RefineFrameTag};
use super::convert::OutputKind;
use super::device::Gpu;
use super::selftest::{GRADING, Rng};
use arcen_media::refine::digest::tile_block_fingerprints;
use arcen_media::refine::hub::DeckRefineHub;
use arcen_media::refine::pier::{PierMonitorRefine, TileJob};
use arcen_media::refine::tile::{RefineGrid, TileCodes};
use arcen_media::refine::zstd_encode::TileEncoder;
use arcen_media::refine::{PRIMARY_MONITOR, RefineContract};
use arcen_media::{ChromaSubsampling, ColorMatrix, ColorRange};
use arcen_protocol::refine::{CODEC_DELTA_ZSTD1, RefineDomain, RefineTileHeader};
use std::time::{Duration, Instant};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;

#[derive(Debug)]
pub struct HubOutcome {
    pub domain: &'static str,
    pub status: String,
    pub frames: u32,
    pub elapsed: Duration,
    pub level: Option<String>,
}

/// Source codes per pixel (planar per block on extraction).
struct Source {
    grid: RefineGrid,
    codes: Vec<[u16; 3]>,
}

impl Source {
    fn extract(&self, job: &TileJob) -> TileCodes {
        let rect = job.rect;
        let mut planar = vec![0u16; rect.pixels() * 3];
        for line in 0..rect.height {
            for column in 0..rect.width {
                let pixel = (line * rect.width + column) as usize;
                let code =
                    self.codes[((rect.y + line) * self.grid.width + rect.x + column) as usize];
                for plane in 0..3 {
                    planar[plane * rect.pixels() + pixel] = code[plane];
                }
            }
        }
        TileCodes::new(rect.width, rect.height, planar).expect("tile")
    }
}

/// A still 10-bit picture refined through the real wire records: the
/// Pier side (scheduler, Zstandard encoder) and the Deck's shared hub.
/// A presenter under test draws [`Self::frame`]s.
pub struct Scenario {
    pub hub: DeckRefineHub,
    pub texture: windows::Win32::Graphics::Direct3D11::ID3D11Texture2D,
    pub grid: RefineGrid,
    domain: RefineDomain,
    source: Source,
    pier: PierMonitorRefine,
    encoder: TileEncoder,
    pier_now: Duration,
    fence: u64,
}

impl Scenario {
    pub fn new(gpu: &Gpu, domain: RefineDomain, width: u32, height: u32) -> Result<Self> {
        let grid = RefineGrid { width, height };
        let stream = GRADING;
        let refine_contract = RefineContract {
            domain,
            depth: stream.depth,
            transfer: stream.transfer,
            primaries: stream.primaries,
            range: ColorRange::Full,
            chroma: ChromaSubsampling::Yuv444,
            matrix: match domain {
                RefineDomain::Rgb => ColorMatrix::Identity,
                RefineDomain::YCbCr => stream.matrix,
            },
        };
        // The still picture: R'G'B' (RGB domain) or Y'CbCr codes, and a
        // lossy decoded version of it (codes off by up to 6).
        let mut rng = Rng(0x5eed);
        let codes: Vec<[u16; 3]> = (0..width * height)
            .map(|index| {
                let (x, y) = (index % width, index / width);
                match domain {
                    RefineDomain::Rgb => [
                        (x * 7 % 1024) as u16,
                        (y * 5 % 1024) as u16,
                        ((x ^ y) % 1024) as u16,
                    ],
                    RefineDomain::YCbCr => [
                        (64 + (x * 3 + y) % 880) as u16,
                        (300 + (rng.next_u32() % 400)) as u16,
                        (300 + (x % 400)) as u16,
                    ],
                }
            })
            .collect();
        let mut rng = Rng(77);
        let mut decoded = Vec::with_capacity((width * height * 4) as usize);
        for code in &codes {
            let (y, cb, cr) = match domain {
                // Any Y'CbCr picture near the source will do: refinement
                // replaces what the decoder made of it.
                RefineDomain::Rgb => (code[1], 512u16, 512u16),
                RefineDomain::YCbCr => (code[0], code[1], code[2]),
            };
            let wobble = |value: u16, rng: &mut Rng| {
                (i32::from(value) + (rng.next_u32() % 13) as i32 - 6).clamp(0, 1023) as u32
            };
            let word = wobble(cb, &mut rng)
                | wobble(y, &mut rng) << 10
                | wobble(cr, &mut rng) << 20
                | 3 << 30;
            decoded.extend_from_slice(&word.to_le_bytes());
        }
        let texture = gpu.texture(
            width,
            height,
            DXGI_FORMAT_Y410,
            D3D11_BIND_SHADER_RESOURCE,
            Some((&decoded, width * 4)),
        )?;
        let hub = DeckRefineHub::new();
        hub.set_accepting(true);
        let mut pier = PierMonitorRefine::new(PRIMARY_MONITOR, 1, 1, refine_contract, grid, 300);
        pier.on_capture(0..grid.block_count() as usize, Duration::ZERO);
        Ok(Self {
            hub,
            texture,
            grid,
            domain,
            source: Source { grid, codes },
            pier,
            encoder: TileEncoder::new().map_err(|error| format!("TileEncoder: {error:?}"))?,
            pier_now: Duration::ZERO,
            fence: 0,
        })
    }

    /// The records before the next access unit (barrier, fence) go to the
    /// hub as the video lane reader would; returns the fence.
    pub fn access_unit(&mut self) -> u64 {
        for record in self.pier.before_access_unit() {
            if let Ok(Some((_, value))) = self.hub.on_video_record(&record) {
                self.fence = value;
            }
        }
        self.fence
    }

    /// The decoded frame that followed `fence`, tagged for Exact still image.
    #[must_use]
    pub fn frame(&self, fence: u64) -> DecodedFrame {
        DecodedFrame {
            picture: crate::present::D3d11Picture::texture(self.texture.clone()),
            contract: GRADING,
            refine: Some(RefineFrameTag {
                hub: self.hub.clone(),
                monitor_id: PRIMARY_MONITOR,
                fence,
            }),
        }
    }

    /// The screen stayed still: every due tile goes to the refine lane.
    pub fn send_tiles(&mut self) -> Result<usize> {
        self.pier_now += Duration::from_millis(400);
        let jobs = self.pier.due_tiles(self.pier_now, usize::MAX);
        for job in &jobs {
            let tile = self.source.extract(job);
            self.pier
                .tile_fingerprints(job, &tile_block_fingerprints(self.grid, job.rect, &tile));
            let payload = self
                .encoder
                .encode(&tile, 10)
                .map_err(|error| format!("encode: {error:?}"))?;
            let header = RefineTileHeader {
                scope: job.scope,
                valid_from_fence: job.valid_from_fence,
                domain: self.domain,
                depth: 10,
                codec: CODEC_DELTA_ZSTD1,
                x: job.rect.x as u16,
                y: job.rect.y as u16,
                width: job.rect.width as u16,
                height: job.rect.height as u16,
                payload_len: payload.len() as u32,
            };
            while self.hub.backlog_full() {
                std::thread::sleep(Duration::from_millis(1));
            }
            self.hub.on_refine_record(&header.encode_with(&payload));
        }
        Ok(jobs.len())
    }

    /// Waits until the hub's worker committed every block.
    pub fn wait_committed(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while self.hub.stats().blocks_committed < u64::from(self.grid.block_count()) {
            if Instant::now() > deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        true
    }

    /// Settled: the Pier's digest.
    pub fn send_digest(&mut self) -> Result<()> {
        self.pier_now += Duration::from_millis(1200);
        let covered = self
            .pier
            .digest_candidates(self.pier_now)
            .ok_or("pier not settled")?;
        let recomputed: Vec<(u32, u128)> = covered
            .iter()
            .map(|&index| {
                let rect = self.grid.block_rect(index).expect("covered block");
                let job = TileJob {
                    scope: self.pier.scope(),
                    valid_from_fence: 0,
                    rect,
                };
                tile_block_fingerprints(self.grid, rect, &self.source.extract(&job))[0]
            })
            .collect();
        let digest = self
            .pier
            .digest(self.pier_now, &recomputed)
            .ok_or("no digest")?;
        self.hub.on_refine_record(&digest.encode());
        Ok(())
    }

    #[must_use]
    pub fn status(&self) -> String {
        self.hub.status(PRIMARY_MONITOR).text
    }

    #[must_use]
    pub fn level(&self) -> Option<String> {
        self.hub
            .status(PRIMARY_MONITOR)
            .verdict
            .map(|verdict| format!("{:?}", verdict.level))
    }
}

/// The scenario through a [`FrameComposer`] drawing into an offscreen
/// target (no window needed; GPU completion stands in for the screen).
pub fn run(gpu: &Gpu, domain: RefineDomain, width: u32, height: u32) -> Result<HubOutcome> {
    let mut scenario = Scenario::new(gpu, domain, width, height)?;
    let target = gpu.texture(
        width,
        height,
        DXGI_FORMAT_R10G10B10A2_UNORM,
        D3D11_BIND_RENDER_TARGET,
        None,
    )?;
    let rtv = gpu.rtv(&target)?;
    let mut composer = FrameComposer::new(gpu)?;
    let started = Instant::now();
    let mut frames = 0;
    let mut present = |scenario: &mut Scenario, composer: &mut FrameComposer| -> Result<()> {
        let fence = scenario.access_unit();
        composer.draw(gpu, &scenario.frame(fence), &rtv, OutputKind::Codes)?;
        composer.submitted(gpu, None)?;
        gpu.flush_and_wait()?;
        composer.poll(gpu, || None);
        frames += 1;
        Ok(())
    };
    present(&mut scenario, &mut composer)?;
    scenario.send_tiles()?;
    if !scenario.wait_committed(Duration::from_secs(60)) {
        return Err("tiles were not committed".into());
    }
    present(&mut scenario, &mut composer)?;
    scenario.send_digest()?;
    // Levels 2 and 3 need presented frames.
    for _ in 0..8 {
        present(&mut scenario, &mut composer)?;
    }
    Ok(HubOutcome {
        domain: match domain {
            RefineDomain::Rgb => "rgb",
            RefineDomain::YCbCr => "ycbcr",
        },
        status: scenario.status(),
        frames,
        elapsed: started.elapsed(),
        level: scenario.level(),
    })
}

super::opaque_debug!(Scenario);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::present::device::AdapterChoice;

    /// ADR 0016 end to end on WARP: the shared hub reaches the top level
    /// each domain allows through the D3D11 compositor.
    #[test]
    fn a_still_picture_is_verified_exact_through_the_d3d11_compositor() {
        let gpu = Gpu::create(&AdapterChoice::Warp).expect("WARP device");
        let rgb = run(&gpu, RefineDomain::Rgb, 320, 180).expect("rgb");
        assert_eq!(rgb.status, "Exact: 100% of picture verified (level 3)");
        let ycbcr = run(&gpu, RefineDomain::YCbCr, 320, 180).expect("ycbcr");
        assert_eq!(ycbcr.status, "Exact: 100% of picture verified (level 2)");
    }
}
