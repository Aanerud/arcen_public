// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Times the shared software loop (`CanvasEncodeLoop`, ADR 0028) on one of
//! Keel's scenarios. With `none` the surface encoder encodes nothing, so a
//! profiler sees the Pier's canvas work alone; `screen` and `motion` encode
//! surfaces with `OpenH264` in that content mode.
//!
//! ```sh
//! canvas_loop_bench WIDTH HEIGHT SCENARIO TICKS [none|screen|motion] [LINK_BPS] [osdamage]
//! ```

use arcen_keel::BgraFrame;
use arcen_keel::scenario::{Scenario, ScenarioKind};
use arcen_media::canvas::tile::ZstdTilePacker;
use arcen_media::canvas::{
    CanvasEncodeLoop, CanvasLoopTerms, CpuShare, EncodeLoopConfig, FullFrameUnit, LoopEncoder,
    LoopMode, SurfaceEncodeError, SurfaceEncodeRequest, SurfaceEncoder,
};
use arcen_media::video::{SoftwareH264Content, SoftwareSurfaces, StageDamage};
use arcen_protocol::canvas::{CanvasControlMsg, CanvasTerms};
use std::time::{Duration, Instant};

struct NoEncoder;

impl SurfaceEncoder for NoEncoder {
    fn open(&mut self, _: u8, _: u32, _: u32) -> Result<(), SurfaceEncodeError> {
        Ok(())
    }
    fn close(&mut self, _: u8) {}
    fn encode(&mut self, _: &SurfaceEncodeRequest<'_>) -> Result<Vec<u8>, SurfaceEncodeError> {
        Ok(vec![0, 0, 0, 1, 0x65, 0])
    }
}

impl LoopEncoder for NoEncoder {
    fn encode_full(
        &mut self,
        _: BgraFrame<'_>,
        _: StageDamage<'_>,
        _: bool,
    ) -> Result<Option<FullFrameUnit>, SurfaceEncodeError> {
        Ok(None)
    }
    fn set_full_frame_rate(&mut self, _: u32) {}
    fn set_full_frame_bitrate(&mut self, _: u64) {}
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let link_bps: u64 = args
        .get(6)
        .and_then(|v| v.parse().ok())
        .unwrap_or(20_000_000);
    match args.get(5).map_or("none", String::as_str) {
        "screen" => run(&args, link_bps, surfaces(SoftwareH264Content::Screen)),
        "motion" => run(&args, link_bps, surfaces(SoftwareH264Content::Motion)),
        _ => run(&args, link_bps, NoEncoder),
    }
}

/// The lab's X11 scenario page: glyph strokes on light paper with a few
/// coloured panels (what `scenprobe` draws on the lab Pier).
fn page(out: &mut [u8], width: usize, height: usize, seed: usize) {
    for y in 0..height {
        for x in 0..width {
            let (cx, cy, ix, iy) = (x / 8, (y + seed) / 18, x % 8, (y + seed) % 18);
            let text = cy % 4 != 3
                && (cx * 7 + cy * 13) % 11 > 2
                && iy > 3
                && iy < 14
                && (ix == 1 || ix == 5 || (iy == 9 && ix < 6))
                && (cx * 31 + cy * 17) % 5 != 0;
            let panel = ((x / 320) + (y + seed) / 240) % 5 == 0;
            let pixel = if text {
                [0x22, 0x22, 0x22, 255]
            } else if panel {
                [0xE8, 0xD8, 0xC0, 255]
            } else {
                [0xF8, 0xF8, 0xF8, 255]
            };
            out[(y * width + x) * 4..(y * width + x) * 4 + 4].copy_from_slice(&pixel);
        }
    }
}

/// `scenprobe scroll`: a viewport under an 80-pixel toolbar, beside a
/// 240-pixel sidebar, scrolling 24 pixels every other tick.
fn probe_scroll(width: usize, height: usize, step: u32, out: &mut Vec<u8>) {
    out.resize(width * height * 4, 0);
    page(out, width, height, 0);
    let (top, view_w) = (80, width - 240);
    let view_h = height - top;
    let mut doc = vec![0; view_w * view_h * 4 * 4];
    page(&mut doc, view_w, view_h * 4, 7);
    let offset = (step as usize / 2 * 24) % (view_h * 3);
    for row in 0..view_h {
        let source = &doc[(offset + row) * view_w * 4..(offset + row + 1) * view_w * 4];
        let at = ((top + row) * width) * 4;
        out[at..at + view_w * 4].copy_from_slice(source);
    }
}

/// `scenprobe drag`: an 800x500 window with a title bar moving (6, 2) a
/// tick over the page.
fn probe_drag(width: usize, height: usize, step: u32, out: &mut Vec<u8>) {
    let (cw, ch) = (800, 500);
    out.resize(width * height * 4, 0);
    page(out, width, height, 0);
    let mut window = vec![0; cw * ch * 4];
    page(&mut window, cw, ch, 3);
    for pixel in window[..cw * 28 * 4].chunks_exact_mut(4) {
        pixel.copy_from_slice(&[0x80, 0x50, 0x30, 255]);
    }
    let span_x = (width - cw) * 2;
    let span_y = (height - ch) * 2;
    let bounce = |position: usize, span: usize| {
        let p = position % span;
        if p > span / 2 { span - p } else { p }
    };
    let x = bounce(40 + step as usize * 6, span_x);
    let y = bounce(40 + step as usize * 2, span_y);
    for row in 0..ch {
        let at = ((y + row) * width + x) * 4;
        out[at..at + cw * 4].copy_from_slice(&window[row * cw * 4..(row + 1) * cw * 4]);
    }
}

fn surfaces(content: SoftwareH264Content) -> SoftwareSurfaces {
    SoftwareSurfaces::for_canvas(arcen_media::canvas::canvas_surface_video(), 60)
        .with_content(content)
}

fn run<E: LoopEncoder>(args: &[String], link_bps: u64, encoder: E) {
    let coded_width: u32 = args.get(1).and_then(|v| v.parse().ok()).unwrap_or(3840);
    let coded_height: u32 = args.get(2).and_then(|v| v.parse().ok()).unwrap_or(2160);
    let (width, height) = (coded_width as usize, coded_height as usize);
    let kind = match args.get(3).map_or("scroll", String::as_str) {
        "typing" => ScenarioKind::Typing,
        "drag" => ScenarioKind::Drag,
        "video" => ScenarioKind::Video,
        "document" => ScenarioKind::DocumentScroll,
        "editor" => ScenarioKind::Editor,
        _ => ScenarioKind::Scroll,
    };
    let ticks: u32 = args.get(4).and_then(|v| v.parse().ok()).unwrap_or(120);
    let terms = CanvasTerms {
        max_canvas_px: 16_384,
        max_surfaces_per_monitor: 2,
        cache_bytes: 100 * 1024 * 1024,
        cache_slots: 25_600,
    };
    let start = Instant::now();
    let Ok(packer) = ZstdTilePacker::new() else {
        return;
    };
    let Ok(mut encode) = CanvasEncodeLoop::new(
        EncodeLoopConfig {
            width: coded_width,
            height: coded_height,
            fps: 60,
            keepalive: Duration::from_secs(1),
            initial_bitrate_bps: link_bps,
            mode: LoopMode::Canvas(CanvasLoopTerms {
                terms,
                monitor_id: 0,
                topology_generation: 0,
                stream_epoch: 0,
                cpu_share: CpuShare::default(),
                logical_cores: 12,
            }),
        },
        encoder,
        Box::new(packer),
        start,
    ) else {
        return;
    };
    let scenario = Scenario::new(width, height, kind, 7);
    // `osdamage`: the capture reports the whole picture damaged every tick.
    let mut os = (args.get(7).map(String::as_str) == Some("osdamage"))
        .then(|| arcen_keel::ExternalDamage::new(width, height).ok())
        .flatten();
    // `replay=FILE`: raw BGRA pictures of WIDTH x HEIGHT, one after another.
    let replay = args
        .get(3)
        .and_then(|name| name.strip_prefix("replay="))
        .and_then(|path| std::fs::read(path).ok());
    let mut pixels = Vec::new();
    let mut total = Duration::ZERO;
    for step in 0..ticks {
        let picture = width * height * 4;
        match args.get(3).map(String::as_str) {
            _ if replay.is_some() => {
                let all = replay.as_deref().unwrap_or_default();
                let count = (all.len() / picture).max(1);
                let at = (step as usize % count) * picture;
                pixels.clear();
                pixels.extend_from_slice(all.get(at..at + picture).unwrap_or_default());
            }
            Some("probe-scroll") => probe_scroll(width, height, step, &mut pixels),
            Some("probe-drag") => probe_drag(width, height, step, &mut pixels),
            _ => scenario.render(u64::from(step), &mut pixels),
        }
        let Ok(frame) = BgraFrame::new(&pixels, width, height, width * 4) else {
            return;
        };
        let now = start + Duration::from_millis(16) * step;
        let begun = Instant::now();
        if let Some(os) = os.as_mut() {
            os.reset();
            os.mark_full_frame();
        }
        let _ = encode.tick(frame, os.as_ref(), now);
        total += begun.elapsed();
        if let Some((generation, digest)) = encode.mirror_digest() {
            encode.on_control(&CanvasControlMsg::CanvasAck {
                monitor_id: 0,
                generation,
                fence: digest.fence,
                unprocessed_bytes: 0,
                decode_ms_p95: 1,
            });
        }
    }
    let stats = encode.take_stats();
    println!(
        "{width}x{height} {kind:?} ticks={ticks} mean_ms={:.2} {}",
        total.as_secs_f64() * 1000.0 / f64::from(ticks),
        stats.line(true, encode.governor_state())
    );
}
