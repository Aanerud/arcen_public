// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The canvas corpus every executor is tested against (ADR 0028 §8): the
//! product planner ([`CanvasPlanner`]) and governor run over Keel's
//! scenarios and a re-shown dialog, and the records they send are what a
//! Deck's GPU executor must reproduce exactly as the CPU executor
//! ([`super::CanvasPixels`]) does.
//!
//! Test support only: compiled for this crate's tests and, behind the
//! `canvas-corpus` feature, for a Deck's executor tests. Surface pictures
//! are stub access units (`[0, 0, 0, 1, nal, surface, n]`, `n` counting
//! every picture of the run); an executor test paints its own picture for
//! each.

// Test support: a corpus that cannot run is a bug in the corpus, and says so.
#![allow(
    clippy::expect_used,
    clippy::missing_panics_doc,
    reason = "test support: a corpus bug panics with its cause"
)]

use super::CostModel;
use super::governor::{CanvasGovernor, CpuShare, GovernorConfig, TickOutcome};
use super::planner::{CanvasPlanner, CommandMix, PlannerConfig};
use super::surface::{SurfaceEncodeError, SurfaceEncodeRequest, SurfaceEncoder};
use super::tile::ResidualTilePacker;
use arcen_keel::BgraFrame;
use arcen_keel::scenario::Scenario;
use arcen_protocol::canvas::{CanvasControlMsg, CanvasTerms};
use std::time::Duration;

pub use arcen_keel::scenario::ScenarioKind;

/// Every Keel scenario the corpus replays.
pub const SCENARIOS: [ScenarioKind; 8] = [
    ScenarioKind::Idle,
    ScenarioKind::Typing,
    ScenarioKind::Editor,
    ScenarioKind::Drag,
    ScenarioKind::Scroll,
    ScenarioKind::DocumentScroll,
    ScenarioKind::Video,
    ScenarioKind::Burst,
];

const TICK: Duration = Duration::from_nanos(1_000_000_000 / 60);

/// A surface encoder that checks the lifecycle and returns small
/// deterministic access units.
#[derive(Debug, Default)]
pub struct StubSurfaceEncoder {
    open: [Option<(u32, u32, bool)>; 4],
    /// Pictures encoded.
    pub pictures: u32,
    /// IDR pictures encoded.
    pub idrs: u32,
}

impl SurfaceEncoder for StubSurfaceEncoder {
    fn open(&mut self, surface: u8, width: u32, height: u32) -> Result<(), SurfaceEncodeError> {
        assert!(width % 2 == 0 && height % 2 == 0, "even coded sizes");
        self.open[usize::from(surface)] = Some((width, height, true));
        Ok(())
    }

    fn close(&mut self, surface: u8) {
        self.open[usize::from(surface)] = None;
    }

    fn encode(
        &mut self,
        request: &SurfaceEncodeRequest<'_>,
    ) -> Result<Vec<u8>, SurfaceEncodeError> {
        let slot = self.open[usize::from(request.surface)]
            .as_mut()
            .expect("encode of an open surface");
        assert_eq!(
            (slot.0, slot.1),
            (request.coded_width, request.coded_height)
        );
        if slot.2 {
            assert!(request.idr, "a surface's first picture is an IDR");
        }
        slot.2 = false;
        assert!(
            request.quality.bitrate_bps > 0,
            "every picture gets a share of the link"
        );
        let grid = request.source.grid();
        assert!(
            (request.rect.x + request.rect.width) as usize <= grid.width()
                && (request.rect.y + request.rect.height) as usize <= grid.height()
        );
        self.pictures += 1;
        self.idrs += u32::from(request.idr);
        Ok(vec![
            0,
            0,
            0,
            1,
            if request.idr { 0x65 } else { 0x41 },
            request.surface,
            u8::try_from(self.pictures % 256).expect("below 256"),
        ])
    }
}

/// What one corpus run sent.
#[derive(Debug, Default)]
pub struct CorpusRun {
    /// Every record, in wire order.
    pub records: Vec<Vec<u8>>,
    /// The command mix over the run.
    pub mix: CommandMix,
}

/// Runs the planner over `ticks` pictures of one `width × height` monitor
/// under a generous budget (8 cores at the default share), acknowledging
/// every frame at once, and returns what it sent. `picture(tick, pixels)`
/// writes tick `tick`'s opaque BGRA8 picture.
///
/// # Panics
///
/// When the planner refuses the run (a corpus bug, not a product case).
pub fn planner_run(
    monitor_id: u16,
    width: u32,
    height: u32,
    terms: CanvasTerms,
    ticks: u64,
    mut picture: impl FnMut(u64, &mut Vec<u8>),
) -> CorpusRun {
    let mut planner = CanvasPlanner::new(
        monitor_id,
        0,
        0,
        width,
        height,
        terms,
        PlannerConfig::default(),
    )
    .expect("corpus planner");
    let mut config = GovernorConfig::new(60, 8, CpuShare::default(), 20_000_000);
    config.learn = false;
    let mut governor = CanvasGovernor::new(config, CostModel::default());
    let mut encoder = StubSurfaceEncoder::default();
    let mut packer = ResidualTilePacker;
    let mut pixels = Vec::new();
    let mut run = CorpusRun::default();
    for tick in 0..ticks {
        picture(tick, &mut pixels);
        let frame = BgraFrame::new(&pixels, width as usize, height as usize, width as usize * 4)
            .expect("corpus picture");
        let index = u32::try_from(tick + 1).expect("corpus length");
        let demand = planner
            .observe(
                frame,
                None,
                TICK * index,
                governor.costs(),
                governor.scale(),
            )
            .expect("corpus observe");
        let (grants, _) = governor.allocate(&[demand]);
        let sent = planner
            .plan(frame, &grants[0], index, &mut encoder, &mut packer)
            .expect("corpus plan");
        let outcome: TickOutcome = sent.outcome(monitor_id);
        let ack = CanvasControlMsg::CanvasAck {
            monitor_id: outcome.monitor_id,
            generation: outcome.generation,
            fence: outcome.fence,
            unprocessed_bytes: 0,
            decode_ms_p95: 2,
        };
        governor.complete(&[outcome]);
        governor.on_control(&ack);
        run.mix.add(sent.mix);
        run.records.extend(sent.records);
    }
    run
}

/// [`planner_run`] over a Keel scenario (seed 7).
#[must_use]
pub fn keel_run(
    kind: ScenarioKind,
    monitor_id: u16,
    width: u32,
    height: u32,
    terms: CanvasTerms,
    ticks: u64,
) -> CorpusRun {
    let scenario = Scenario::new(width as usize, height as usize, kind, 7);
    planner_run(monitor_id, width, height, terms, ticks, |tick, pixels| {
        scenario.render(tick, pixels);
    })
}

/// [`planner_run`] over a dialog that appears, goes and comes back twice,
/// so the planner stores it in the cache and paints it from there.
#[must_use]
pub fn dialog_run(monitor_id: u16, width: u32, height: u32, terms: CanvasTerms) -> CorpusRun {
    planner_run(monitor_id, width, height, terms, 60, |tick, pixels| {
        pixels.clear();
        pixels.extend(
            (0..width * height)
                .flat_map(|at| [u8::try_from(at % 7).expect("below 7") * 30, 40, 50, 255]),
        );
        if (10..20).contains(&tick) || (40..50).contains(&tick) {
            for y in height / 3..height {
                for x in width / 5..width * 4 / 5 {
                    let at = ((y * width + x) * 4) as usize;
                    let ink = u8::try_from((x * 13 + y * 7) % 5).expect("below 5") * 50;
                    pixels[at..at + 4].copy_from_slice(&[ink, 255 - ink, 128, 255]);
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deck_terms() -> CanvasTerms {
        CanvasTerms {
            max_canvas_px: 16_384,
            max_surfaces_per_monitor: 2,
            cache_bytes: 16 * 1024 * 1024,
            cache_slots: 4096,
        }
    }

    #[test]
    fn the_corpus_sends_every_operation_an_executor_performs() {
        let mut mix = CommandMix::default();
        for kind in SCENARIOS {
            mix.add(keel_run(kind, 0, 330, 202, deck_terms(), 90).mix);
        }
        mix.add(dialog_run(0, 320, 192, deck_terms()).mix);
        assert!(
            mix.fill > 0
                && mix.copy > 0
                && mix.tile > 0
                && mix.cache_store > 0
                && mix.cache_paint > 0
                && mix.surface_open > 0
                && mix.surface_paint > 0,
            "{mix:?}"
        );
    }
}
