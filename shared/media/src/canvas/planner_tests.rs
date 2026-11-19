// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The planner and governor gates: corpus replays over Keel's scenarios
//! (command mix, CPU estimate, mirror digest == independent replay digest)
//! and the budget simulation on the `WorkMeter` cost model (mandatory work
//! never deferred, text latency under a saturating video, one budget
//! across monitors).

use super::corpus::StubSurfaceEncoder as StubEncoder;
use super::pixels::ResidualTileDecoder;
use super::tile::ResidualTilePacker;
use super::*;
use arcen_keel::scenario::{Scenario, ScenarioKind};
use arcen_keel::{BgraFrame, ExternalDamage, PixelRect, WorkKind, WorkMeter};
use arcen_protocol::canvas::{
    CanvasControlMsg, CanvasRecord, CanvasResetReason, CanvasTerms, canvas_block_count,
};
use std::time::Duration;

const TICK: Duration = Duration::from_nanos(1_000_000_000 / 60);

fn terms() -> CanvasTerms {
    CanvasTerms {
        max_canvas_px: 16_384,
        max_surfaces_per_monitor: 4,
        cache_bytes: 100 * 1024 * 1024,
        cache_slots: 4096,
    }
}

/// One monitor: its planner, encoder, and an independent Deck replaying
/// only the wire bytes.
struct Monitor {
    planner: CanvasPlanner,
    encoder: StubEncoder,
    replay: CanvasMirror,
    width: u32,
    height: u32,
}

impl Monitor {
    fn new(monitor_id: u16, width: u32, height: u32) -> Self {
        Self {
            planner: CanvasPlanner::new(
                monitor_id,
                1,
                1,
                width,
                height,
                terms(),
                PlannerConfig::default(),
            )
            .unwrap(),
            encoder: StubEncoder::default(),
            replay: CanvasMirror::new(monitor_id, terms()),
            width,
            height,
        }
    }

    fn frame<'a>(&self, pixels: &'a [u8]) -> BgraFrame<'a> {
        BgraFrame::new(
            pixels,
            self.width as usize,
            self.height as usize,
            self.width as usize * 4,
        )
        .unwrap()
    }
}

/// A Pier: the governor and every monitor's planner.
struct Pier {
    governor: CanvasGovernor,
    monitors: Vec<Monitor>,
    packer: ResidualTilePacker,
    tick: u32,
    events: Vec<GovernorEvent>,
}

impl Pier {
    fn new(governor: GovernorConfig, costs: &CostModel, monitors: Vec<Monitor>) -> Self {
        Self {
            governor: CanvasGovernor::new(governor, *costs),
            monitors,
            packer: ResidualTilePacker,
            tick: 0,
            events: Vec::new(),
        }
    }

    /// A generous Pier (8 cores at 50 %, the default priors).
    fn generous(monitors: Vec<Monitor>) -> Self {
        let mut config = GovernorConfig::new(60, 8, CpuShare::default(), 20_000_000);
        config.learn = false;
        Self::new(config, &CostModel::default(), monitors)
    }

    /// One tick over every monitor's picture; checks every record against
    /// the independent replay and the mirror against the source.
    fn step(&mut self, pictures: &[(&[u8], Option<&ExternalDamage>)]) -> Vec<PlannedTick> {
        self.tick += 1;
        let now = TICK * self.tick;
        let mut demands = Vec::new();
        for (monitor, (pixels, damage)) in self.monitors.iter_mut().zip(pictures) {
            let frame = monitor.frame(pixels);
            demands.push(
                monitor
                    .planner
                    .observe(
                        frame,
                        *damage,
                        now,
                        self.governor.costs(),
                        self.governor.scale(),
                    )
                    .unwrap(),
            );
        }
        let (grants, event) = self.governor.allocate(&demands);
        self.events.extend(event);
        let mut ticks = Vec::new();
        for ((monitor, (pixels, _)), grant) in self.monitors.iter_mut().zip(pictures).zip(&grants) {
            let frame = monitor.frame(pixels);
            let tick = monitor
                .planner
                .plan(
                    frame,
                    grant,
                    self.tick,
                    &mut monitor.encoder,
                    &mut self.packer,
                )
                .unwrap();
            for record in &tick.records {
                monitor
                    .replay
                    .apply(
                        &CanvasRecord::decode(record).unwrap(),
                        &mut ResidualTileDecoder,
                        &mut NoSurfacePixels,
                    )
                    .unwrap();
            }
            assert_eq!(
                monitor.planner.mirror().digest(),
                monitor.replay.digest(),
                "monitor {} tick {}: mirror digest == replay digest",
                monitor.planner.monitor_id(),
                self.tick
            );
            assert_exact(monitor, pixels);
            ticks.push(tick);
        }
        let outcomes: Vec<TickOutcome> = self
            .monitors
            .iter()
            .zip(&ticks)
            .map(|(monitor, tick)| tick.outcome(monitor.planner.monitor_id()))
            .collect();
        self.governor.complete(&outcomes);
        // A prompt Deck acknowledges every frame.
        for outcome in outcomes {
            self.governor.on_control(&CanvasControlMsg::CanvasAck {
                monitor_id: outcome.monitor_id,
                generation: outcome.generation,
                fence: outcome.fence,
                unprocessed_bytes: 0,
                decode_ms_p95: 2,
            });
        }
        ticks
    }
}

/// Every exact block the Deck shows that is not waiting equals the source.
fn assert_exact(monitor: &Monitor, source: &[u8]) {
    let mirror = monitor.planner.mirror();
    let grid = mirror.state().grid().unwrap();
    let canvas = mirror.pixels().bgra();
    for index in mirror.state().lossless_blocks() {
        if monitor.planner.is_pending(index) {
            continue;
        }
        let block = grid.block_rect(index).unwrap();
        for y in block.y..block.y + block.height {
            let start = ((y * monitor.width + block.x) * 4) as usize;
            let end = start + block.width as usize * 4;
            assert_eq!(
                canvas[start..end],
                source[start..end],
                "block {index} of monitor {} differs from the source",
                monitor.planner.monitor_id()
            );
        }
    }
}

/// OS damage as a compositor reports it, with the scroll as a move.
fn os_damage(scenario: Scenario, tick: u64, damage: &mut ExternalDamage, moves: bool) {
    damage.reset();
    let mut rects = Vec::new();
    scenario.damage_rects(tick.saturating_sub(1), tick, &mut rects);
    for rect in rects {
        damage.mark_rect(rect);
    }
    if moves
        && let Some(truth) = scenario.scroll_truth(tick.saturating_sub(1), tick)
        && truth.dy != 0
    {
        let viewport = truth.viewport;
        let signed = |value: usize| i64::try_from(value).unwrap();
        let (x, y, w, h) = (
            signed(viewport.x),
            signed(viewport.y),
            signed(viewport.width),
            signed(viewport.height),
        );
        let dy = truth.dy;
        if dy.abs() < h {
            let (source_y, top) = if dy > 0 { (y + dy, y) } else { (y, y - dy) };
            damage.mark_move_rect(x, source_y, x, top, x + w, top + h - dy.abs());
        }
    }
}

#[derive(Debug, Default)]
struct Corpus {
    mix: CommandMix,
    estimate: Duration,
    peak: Duration,
    records: usize,
    bytes: usize,
    full_motion: bool,
}

fn replay_scenario(
    kind: ScenarioKind,
    width: u32,
    height: u32,
    ticks: u64,
    os: Option<bool>,
) -> Corpus {
    let scenario = Scenario::new(width as usize, height as usize, kind, 7);
    let mut pier = Pier::generous(vec![Monitor::new(1, width, height)]);
    let mut damage = ExternalDamage::new(width as usize, height as usize).unwrap();
    let mut pixels = Vec::new();
    let mut corpus = Corpus::default();
    for tick in 0..ticks {
        scenario.render(tick, &mut pixels);
        let report = if let Some(moves) = os {
            os_damage(scenario, tick, &mut damage, moves);
            Some(&damage)
        } else {
            None
        };
        let planned = pier.step(&[(&pixels, report)]).remove(0);
        corpus.mix.add(planned.mix);
        corpus.estimate += planned.estimate.total();
        corpus.peak = corpus.peak.max(planned.estimate.total());
        corpus.records += planned.records.len();
        corpus.bytes += planned.records.iter().map(Vec::len).sum::<usize>();
        // Generous budget: nothing exact waits.
        assert!(
            planned.exact_latency.iter().all(|(_, waited)| *waited == 0),
            "{kind:?} tick {tick}: exact work deferred under a generous budget"
        );
    }
    corpus.full_motion = pier.monitors[0].planner.full_motion();
    corpus
}

#[test]
fn corpus_replays_over_every_keel_scenario_match_the_mirror() {
    let mut table = Vec::new();
    for (width, height) in [(320, 192), (330, 202)] {
        for kind in [
            ScenarioKind::Idle,
            ScenarioKind::Typing,
            ScenarioKind::Editor,
            ScenarioKind::Drag,
            ScenarioKind::Scroll,
            ScenarioKind::DocumentScroll,
            ScenarioKind::Video,
            ScenarioKind::Burst,
        ] {
            let corpus = replay_scenario(kind, width, height, 150, None);
            let mix = corpus.mix;
            match kind {
                ScenarioKind::Idle => {
                    // A reset, the first paint, then a keepalive a second.
                    assert_eq!(corpus.records, 1 + 1 + 2, "{kind:?}");
                    assert_eq!(mix.surface_open + mix.copy, 0, "{kind:?}");
                }
                ScenarioKind::Editor => {
                    assert_eq!(mix.surface_open, 0, "{kind:?}: typing opens no surface");
                    assert!(mix.tile > 0, "{kind:?}");
                }
                ScenarioKind::Scroll | ScenarioKind::DocumentScroll => {
                    assert!(mix.copy > 0, "{kind:?} at {width}x{height}: {mix:?}");
                }
                ScenarioKind::Video => {
                    assert!(mix.surface_open >= 1 && mix.surface_paint > 100, "{mix:?}");
                    assert!(corpus.full_motion, "full-frame video is one surface");
                }
                _ => {}
            }
            table.push((kind, width, height, corpus));
        }
    }
    let editor = &table
        .iter()
        .find(|row| row.0 == ScenarioKind::Editor)
        .unwrap()
        .3;
    let video = &table
        .iter()
        .find(|row| row.0 == ScenarioKind::Video)
        .unwrap()
        .3;
    assert!(
        editor.estimate * 4 < video.estimate,
        "typing costs a fraction of video: {:?} vs {:?}",
        editor.estimate,
        video.estimate
    );
    eprintln!(
        "| scenario | size | fill | copy | tile | cache store/paint | surface open/paint | records | KiB | CPU est. ms/tick (mean, peak) |"
    );
    eprintln!("| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |");
    for (kind, width, height, corpus) in &table {
        let mix = corpus.mix;
        eprintln!(
            "| {kind:?} | {width}x{height} | {} | {} | {} | {}/{} | {}/{} | {} | {} | {:.3}, {:.3} |",
            mix.fill,
            mix.copy,
            mix.tile,
            mix.cache_store,
            mix.cache_paint,
            mix.surface_open,
            mix.surface_paint,
            corpus.records,
            corpus.bytes / 1024,
            corpus.estimate.as_secs_f64() * 1_000.0 / 150.0,
            corpus.peak.as_secs_f64() * 1_000.0,
        );
    }
}

#[test]
fn corpus_replays_with_os_damage_and_os_moves_match_the_mirror() {
    for kind in [
        ScenarioKind::Editor,
        ScenarioKind::DocumentScroll,
        ScenarioKind::Drag,
        ScenarioKind::Video,
    ] {
        let narrowed = replay_scenario(kind, 320, 192, 120, Some(false));
        let moved = replay_scenario(kind, 320, 192, 120, Some(true));
        if kind == ScenarioKind::DocumentScroll {
            assert!(narrowed.mix.copy > 0, "Keel's estimator finds the scroll");
            assert!(moved.mix.copy > 0, "OS moves are verified and copied");
        }
    }
}

#[test]
fn a_re_shown_picture_is_painted_from_the_cache() {
    // A dialog appears, goes, and comes back.
    let (width, height) = (320_u32, 192_u32);
    let background = |pixels: &mut Vec<u8>| {
        pixels.clear();
        pixels.extend(
            (0..width * height).flat_map(|at| [u8::try_from(at % 7).unwrap() * 30, 40, 50, 255]),
        );
    };
    let dialog = |pixels: &mut Vec<u8>| {
        for y in 64..192 {
            for x in 64..256 {
                let at = ((y * width + x) * 4) as usize;
                let ink = u8::try_from((x * 13 + y * 7) % 5).unwrap() * 50;
                pixels[at..at + 4].copy_from_slice(&[ink, 255 - ink, 128, 255]);
            }
        }
    };
    let mut pier = Pier::generous(vec![Monitor::new(1, width, height)]);
    let mut pixels = Vec::new();
    let mut mix = CommandMix::default();
    for tick in 0..60 {
        background(&mut pixels);
        if (10..20).contains(&tick) || (40..50).contains(&tick) {
            dialog(&mut pixels);
        }
        mix.add(pier.step(&[(&pixels, None)])[0].mix);
    }
    assert!(mix.cache_store >= 6, "{mix:?}");
    assert!(mix.cache_paint >= 6, "{mix:?}");
}

/// The simulation's host: costs a VM's software encoder would show, fed
/// through the `WorkMeter` as measurements.
fn vm_costs(surface_ms_per_mpx: f64) -> CostModel {
    let mut meter = WorkMeter::new(1.0);
    for (kind, rate) in WorkKind::ALL
        .into_iter()
        .zip([0.4, 0.3, 1.5, 4.0, surface_ms_per_mpx, 0.2])
    {
        meter.record(kind, 1_000_000, Duration::from_secs_f64(rate / 1_000.0));
    }
    CostModel::new(meter)
}

fn sim_config(cores: u32, percent: u8) -> GovernorConfig {
    let mut config = GovernorConfig::new(
        60,
        cores,
        CpuShare::from_percent(percent).unwrap(),
        8_000_000,
    );
    config.learn = false;
    config
}

/// Renders `scenario` into `out` at column `x` (rows `stride` pixels
/// wide).
fn blit(
    scenario: Scenario,
    tick: u64,
    scratch: &mut Vec<u8>,
    out: &mut [u8],
    x: usize,
    stride: usize,
) {
    scenario.render(tick, scratch);
    let row = scenario.width() * 4;
    for y in 0..scenario.height() {
        let at = (y * stride + x) * 4;
        out[at..at + row].copy_from_slice(&scratch[y * row..(y + 1) * row]);
    }
}

fn p95(values: &mut [u32]) -> u32 {
    values.sort_unstable();
    values[(values.len() * 95).div_ceil(100).saturating_sub(1)]
}

#[test]
fn text_latency_stays_within_one_tick_under_a_saturating_video() {
    // An editor beside a playing video on one 640 × 360 monitor; one core
    // at 50 %, and surface pictures cost 120 ms/Mpx (14 ms each): the
    // video alone wants 1.7 times the budget.
    let (width, height) = (640_usize, 360_usize);
    let editor = Scenario::new(320, 360, ScenarioKind::Editor, 3);
    let video = Scenario::new(320, 360, ScenarioKind::Video, 5);
    let mut pier = Pier::new(
        sim_config(1, 50),
        &vm_costs(120.0),
        vec![Monitor::new(
            1,
            u32::try_from(width).unwrap(),
            u32::try_from(height).unwrap(),
        )],
    );
    let mut pixels = vec![0_u8; width * height * 4];
    let mut scratch = Vec::new();
    let mut text = Vec::new();
    let mut surfaces = 0;
    for tick in 0..900 {
        blit(editor, tick, &mut scratch, &mut pixels, 0, width);
        blit(video, tick, &mut scratch, &mut pixels, 320, width);
        let planned = pier.step(&[(&pixels, None)]).remove(0);
        surfaces += planned.surfaces_painted;
        if tick > 0 {
            let cols = u32::try_from(width.div_ceil(16)).unwrap();
            text.extend(
                planned
                    .exact_latency
                    .iter()
                    .filter(|(index, _)| index % cols < 16)
                    .map(|(_, waited)| *waited),
            );
        }
    }
    let state = pier.governor.state();
    assert!(
        state.surface_fps < 60,
        "the video saturates the budget: {state:?}"
    );
    assert!(surfaces > 100, "the video still plays: {surfaces}");
    assert!(text.len() > 100, "the editor typed: {}", text.len());
    let p95 = p95(&mut text);
    assert!(p95 <= 1, "text p95 latency {p95} ticks");
    eprintln!(
        "saturating video: surface rate {} fps, {} surface pictures in 15 s, text p95 {} ticks over {} blocks, events {:?}",
        state.surface_fps,
        surfaces,
        p95,
        text.len(),
        pier.events
    );
}

#[test]
fn mandatory_work_is_never_deferred() {
    // One core at 1 %: 0.17 ms a tick, far below what anything costs.
    let (width, height) = (320_u32, 192_u32);
    let editor = Scenario::new(width as usize, height as usize, ScenarioKind::Editor, 9);
    let video = Scenario::new(width as usize, height as usize, ScenarioKind::Video, 4);
    let idle = Scenario::new(width as usize, height as usize, ScenarioKind::Idle, 2);
    let mut pier = Pier::new(
        sim_config(1, 1),
        &vm_costs(120.0),
        vec![
            Monitor::new(1, width, height),
            Monitor::new(2, width, height),
            Monitor::new(3, width, height),
        ],
    );
    let (mut typing, mut playing, mut still) = (Vec::new(), Vec::new(), Vec::new());
    let mut rects = Vec::new();
    let mut near_input = 0;
    let mut keepalives = 0;
    for tick in 0..300_u64 {
        editor.render(tick, &mut typing);
        video.render(tick, &mut playing);
        idle.render(tick, &mut still);
        // The user types: the key lands where the editor changes.
        editor.damage_rects(tick.saturating_sub(1), tick, &mut rects);
        let typed: Option<PixelRect> = rects.last().copied();
        if let Some(rect) = typed {
            let (x, y) = (
                u32::try_from(rect.x + rect.width / 2).unwrap(),
                u32::try_from(rect.y + rect.height / 2).unwrap(),
            );
            pier.monitors[0]
                .planner
                .note_input(x, y, TICK * (pier.tick + 1));
        }
        if tick == 150 {
            pier.monitors[0]
                .planner
                .request_reset(CanvasResetReason::ResetRequest);
        }
        let planned = pier.step(&[(&typing, None), (&playing, None), (&still, None)]);
        // A reset (the first tick, and tick 150) repaints every block in
        // the same tick.
        if tick == 0 || tick == 150 {
            assert!(planned[0].records.len() >= 2, "a reset and its repaint");
            assert_eq!(
                planned[0].pending_blocks, 0,
                "tick {tick}: repaint deferred"
            );
            assert_eq!(planned[2].pending_blocks, 0);
        }
        // Damage at the input is painted the tick it changes.
        if let Some(rect) = typed {
            let grid = pier.monitors[0].planner.mirror().state().grid().unwrap();
            for index in grid.blocks_of(crate::refine::tile::TileRect {
                x: u32::try_from(rect.x).unwrap(),
                y: u32::try_from(rect.y).unwrap(),
                width: u32::try_from(rect.width).unwrap(),
                height: u32::try_from(rect.height).unwrap(),
            }) {
                assert!(
                    !pier.monitors[0].planner.is_pending(index),
                    "tick {tick}: block {index} at the input waits"
                );
                near_input += 1;
            }
        }
        // Every surface the video opens paints its IDR in the same tick.
        for slot in pier.monitors[1].planner.surfaces().slots() {
            assert!(!slot.awaiting_idr, "tick {tick}: surface {} waits", slot.id);
        }
        // The idle monitor keeps its canvas alive once a second.
        if tick > 0 && !planned[2].records.is_empty() {
            assert_eq!(planned[2].mix.total(), 0, "only keepalives");
            keepalives += 1;
        }
    }
    assert!(near_input > 50, "{near_input}");
    assert_eq!(keepalives, 4, "one keepalive a second");
    let video = &pier.monitors[1].encoder;
    assert!(video.idrs >= 1, "the video surface opened");
    // The starved budget left only IDRs: the non-IDR pictures waited.
    assert!(pier.governor.state().surface_fps < 60);
}

#[test]
fn one_budget_across_monitors_serves_focus_and_motion() {
    // Three monitors: a video, an editor with focus, and an idle one. The
    // editor's text keeps its latency; the idle monitor costs Keel's
    // hashing and keepalives only.
    let (width, height) = (320_u32, 192_u32);
    let video = Scenario::new(width as usize, height as usize, ScenarioKind::Video, 4);
    let editor = Scenario::new(width as usize, height as usize, ScenarioKind::Editor, 9);
    let idle = Scenario::new(width as usize, height as usize, ScenarioKind::Idle, 2);
    let mut pier = Pier::new(
        sim_config(1, 50),
        &vm_costs(400.0),
        vec![
            Monitor::new(1, width, height),
            Monitor::new(2, width, height),
            Monitor::new(3, width, height),
        ],
    );
    pier.monitors[1].planner.set_focus(true);
    let (mut a, mut b, mut c) = (Vec::new(), Vec::new(), Vec::new());
    let mut text = Vec::new();
    let mut idle_cost = Duration::ZERO;
    for tick in 0..600_u64 {
        video.render(tick, &mut a);
        editor.render(tick, &mut b);
        idle.render(tick, &mut c);
        let planned = pier.step(&[(&a, None), (&b, None), (&c, None)]);
        if tick > 0 {
            text.extend(planned[1].exact_latency.iter().map(|(_, waited)| *waited));
            idle_cost += planned[2]
                .estimate
                .total()
                .saturating_sub(planned[2].estimate.sunk);
        }
    }
    assert!(pier.governor.state().surface_fps < 60, "video is governed");
    assert!(p95(&mut text) <= 1, "focus monitor text p95");
    assert_eq!(
        idle_cost,
        Duration::ZERO,
        "an idle monitor sends keepalives only"
    );
}

#[test]
fn degradation_reaches_the_surfaces_in_order_and_scaled_surfaces_reopen() {
    // A VM whose surface pictures cost 1.6 s/Mpx (98 ms each): nothing
    // fits at any rate until the surface is encoded smaller.
    let (width, height) = (320_u32, 192_u32);
    let video = Scenario::new(width as usize, height as usize, ScenarioKind::Video, 4);
    let mut pier = Pier::new(
        sim_config(1, 50),
        &vm_costs(1_600.0),
        vec![Monitor::new(1, width, height)],
    );
    let mut pixels = Vec::new();
    let mut scaled = None;
    for tick in 0..900_u64 {
        video.render(tick, &mut pixels);
        // The planner observes at the scale in force before the tick's
        // allocation (which may step it).
        let observed = pier.governor.scale();
        pier.step(&[(&pixels, None)]);
        if observed != SurfaceScale::Full && scaled.is_none() {
            scaled = Some(tick);
        }
        if let Some(slot) = pier.monitors[0].planner.surfaces().slots().next() {
            assert_eq!(slot.scale, observed, "surfaces follow the scale step");
        }
    }
    assert!(scaled.is_some(), "events {:?}", pier.events);
    let kinds: Vec<_> = pier.events.iter().map(|event| event.step).collect();
    assert!(
        matches!(
            kinds.as_slice(),
            [
                DegradationStep::SurfaceFps { .. },
                DegradationStep::Quality { .. },
                DegradationStep::Quality { .. },
                DegradationStep::Scale { .. },
                ..
            ]
        ),
        "{kinds:?}"
    );
}

#[test]
fn a_photo_opening_is_one_lossy_picture_then_refined_to_exact() {
    // Rule 5 then rule 6: a many-colour 320 × 256 picture appears at once
    // on a flat desktop, then stays.
    let (width, height) = (480_u32, 320_u32);
    let photo = |pixels: &mut Vec<u8>, shown: bool| {
        pixels.clear();
        for y in 0..height {
            for x in 0..width {
                let inside = shown && (64..384).contains(&x) && (32..288).contains(&y);
                let noise = (x.wrapping_mul(2_654_435_761) ^ y.wrapping_mul(40_503)) >> 7;
                let [b, g, r, _] = noise.to_le_bytes();
                pixels.extend(if inside {
                    [b, g, r, 255]
                } else {
                    [90, 60, 30, 255]
                });
            }
        }
    };
    let mut pier = Pier::generous(vec![Monitor::new(1, width, height)]);
    let mut pixels = Vec::new();
    let mut mix = Vec::new();
    let mut refined = false;
    for tick in 0..60 {
        photo(&mut pixels, tick >= 5);
        let planned = pier.step(&[(&pixels, None)]).remove(0);
        if tick > 5 && planned.mix.tile > 0 {
            assert!(planned.carried_optional, "refinement is optional work");
            refined = true;
        }
        mix.push(planned.mix);
    }
    let shown = mix[5];
    assert_eq!(
        (
            shown.surface_open,
            shown.surface_paint,
            shown.surface_close,
            shown.tile
        ),
        (1, 1, 1, 0),
        "one picture, not 320 tiles: {shown:?}"
    );
    assert!(refined, "the settled picture is rebuilt exactly");
    let state = pier.monitors[0].planner.mirror().state();
    assert_eq!(
        (0..canvas_block_count(width, height))
            .filter(|index| state.is_lossy(*index))
            .count(),
        0,
        "every block exact once settled"
    );
}

#[test]
fn a_move_hint_that_does_not_hold_is_never_copied() {
    // The OS claims a scroll while an editor types: the mirror check
    // refuses it and the damage goes as tiles.
    let (width, height) = (320_u32, 192_u32);
    let editor = Scenario::new(width as usize, height as usize, ScenarioKind::Editor, 9);
    let mut pier = Pier::generous(vec![Monitor::new(1, width, height)]);
    let mut damage = ExternalDamage::new(width as usize, height as usize).unwrap();
    let mut pixels = Vec::new();
    let mut mix = CommandMix::default();
    for tick in 0..90 {
        editor.render(tick, &mut pixels);
        os_damage(editor, tick, &mut damage, false);
        damage.mark_move_rect(0, 40, 0, 0, 320, 120);
        mix.add(pier.step(&[(&pixels, Some(&damage))])[0].mix);
    }
    assert_eq!(mix.copy, 0, "{mix:?}");
    assert!(mix.tile > 0);
}

#[test]
fn a_reset_after_a_scroll_repaints_the_copied_blocks_exactly() {
    // A scroll copies text over what was a solid band; the copied blocks are
    // not measured. A later reset must not paint them from the band's old
    // traits.
    let (width, height) = (128_u32, 128_u32);
    let text = |seed: u32, y: u32, x: u32| -> [u8; 4] {
        let on = (x.wrapping_mul(7) ^ y.wrapping_mul(13) ^ seed) % 5 == 0;
        if on {
            [0x20, 0x20, 0x20, 255]
        } else {
            [0xF8, 0xF8, 0xF8, 255]
        }
    };
    let picture = |rows: &dyn Fn(u32, u32) -> [u8; 4]| -> Vec<u8> {
        let mut pixels = Vec::with_capacity((width * height * 4) as usize);
        for y in 0..height {
            for x in 0..width {
                pixels.extend_from_slice(&rows(y, x));
            }
        }
        pixels
    };
    let before = picture(&|y, x| {
        if y < 64 {
            [0xF0, 0xE0, 0xD0, 255]
        } else {
            text(1, y, x)
        }
    });
    let after = picture(&|y, x| {
        if y < 64 {
            text(1, y + 64, x)
        } else {
            text(2, y, x)
        }
    });
    let mut pier = Pier::generous(vec![Monitor::new(1, width, height)]);
    pier.step(&[(&before, None)]);
    pier.step(&[(&before, None)]);
    let mut damage = ExternalDamage::new(width as usize, height as usize).unwrap();
    damage.mark_move_rect(0, 64, 0, 0, 128, 64);
    damage.mark_rect_bounds(0, 64, 128, 128);
    let scrolled = pier.step(&[(&after, Some(&damage))]);
    assert!(scrolled[0].mix.copy > 0, "{:?}", scrolled[0].mix);
    pier.monitors[0]
        .planner
        .request_reset(CanvasResetReason::ResetRequest);
    // `step` checks every exact block of the mirror against the picture.
    pier.step(&[(&after, None)]);
    pier.step(&[(&after, None)]);
}
