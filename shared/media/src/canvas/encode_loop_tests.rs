// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The shared encode loop: both of its paths, its recovery and control, and
//! a replay of everything it sends through an independent mirror.

use std::time::{Duration, Instant};

use arcen_keel::scenario::{Scenario, ScenarioKind};
use arcen_keel::{BgraFrame, ExternalDamage};
use arcen_protocol::canvas::{CanvasControlMsg, CanvasRecord, CanvasResetReason, CanvasTerms};

use super::pixels::ResidualTileDecoder;
use super::tile::ResidualTilePacker;
use super::*;
use crate::video::StageDamage;

const WIDTH: u32 = 320;
const HEIGHT: u32 = 192;
const TICK: Duration = Duration::from_nanos(1_000_000_000 / 60);
const KEEPALIVE: Duration = Duration::from_secs(1);

fn terms() -> CanvasTerms {
    CanvasTerms {
        max_canvas_px: 16_384,
        max_surfaces_per_monitor: 2,
        cache_bytes: 100 * 1024 * 1024,
        cache_slots: 4096,
    }
}

/// Small deterministic access units; counts full-frame pictures and the
/// damage it was asked to stage.
#[derive(Debug, Default)]
struct StubEncoder {
    open: Vec<u8>,
    surface_pictures: u32,
    full_pictures: u32,
    staged_rows: Vec<&'static str>,
    fail_next_full: bool,
    rate: Option<u32>,
    bitrate: Option<u64>,
}

impl SurfaceEncoder for StubEncoder {
    fn open(&mut self, surface: u8, _: u32, _: u32) -> Result<(), SurfaceEncodeError> {
        self.open.push(surface);
        Ok(())
    }

    fn close(&mut self, surface: u8) {
        self.open.retain(|open| *open != surface);
    }

    fn encode(
        &mut self,
        request: &SurfaceEncodeRequest<'_>,
    ) -> Result<Vec<u8>, SurfaceEncodeError> {
        assert!(self.open.contains(&request.surface));
        self.surface_pictures += 1;
        Ok(vec![0, 0, 0, 1, if request.idr { 0x65 } else { 0x41 }, 7])
    }
}

impl LoopEncoder for StubEncoder {
    fn encode_full(
        &mut self,
        _: BgraFrame<'_>,
        damage: StageDamage<'_>,
        idr: bool,
    ) -> Result<Option<FullFrameUnit>, SurfaceEncodeError> {
        if std::mem::take(&mut self.fail_next_full) {
            return Err(SurfaceEncodeError::Encode);
        }
        self.staged_rows.push(match damage {
            StageDamage::Full => "full",
            StageDamage::Rows(_) => "rows",
            StageDamage::Clean => "clean",
        });
        self.full_pictures += 1;
        Ok(Some(FullFrameUnit {
            bytes: vec![0, 0, 0, 1, if idr { 0x65 } else { 0x41 }],
            keyframe: idr,
        }))
    }

    fn set_full_frame_rate(&mut self, fps: u32) {
        self.rate = Some(fps);
    }

    fn set_full_frame_bitrate(&mut self, bps: u64) {
        self.bitrate = Some(bps);
    }
}

fn config(mode: LoopMode) -> EncodeLoopConfig {
    EncodeLoopConfig {
        width: WIDTH,
        height: HEIGHT,
        fps: 60,
        keepalive: KEEPALIVE,
        initial_bitrate_bps: 8_000_000,
        mode,
    }
}

fn canvas_mode() -> LoopMode {
    LoopMode::Canvas(CanvasLoopTerms {
        terms: terms(),
        monitor_id: 0,
        topology_generation: 0,
        stream_epoch: 0,
        cpu_share: CpuShare::default(),
        logical_cores: 8,
    })
}

fn frame(pixels: &[u8]) -> BgraFrame<'_> {
    BgraFrame::new(pixels, WIDTH as usize, HEIGHT as usize, WIDTH as usize * 4).unwrap()
}

/// A Deck: replays every canvas record into its own mirror.
struct Replay {
    mirror: CanvasMirror,
    records: u32,
    resets: Vec<CanvasResetReason>,
    generation: u32,
}

impl Replay {
    fn new() -> Self {
        Self {
            mirror: CanvasMirror::new(0, terms()),
            records: 0,
            resets: Vec::new(),
            generation: 0,
        }
    }

    /// A prompt Deck acknowledges what it applied.
    fn ack<E: LoopEncoder>(&self, encode: &mut CanvasEncodeLoop<E>) {
        if let Some(digest) = self.mirror.digest() {
            encode.on_control(&CanvasControlMsg::CanvasAck {
                monitor_id: 0,
                generation: self.generation,
                fence: digest.fence,
                unprocessed_bytes: 0,
                decode_ms_p95: 2,
            });
        }
    }

    fn apply(&mut self, tick: &LoopTick) {
        for record in &tick.records {
            let LoopRecord::Canvas { bytes, reset } = record else {
                panic!("a canvas Deck gets canvas records only");
            };
            let decoded = CanvasRecord::decode(bytes).unwrap();
            assert_eq!(*reset, matches!(decoded, CanvasRecord::Reset(_)));
            if let CanvasRecord::Reset(reset) = &decoded {
                // Single-monitor rule (WIRE.md): monitor 0, generation 0, epoch 0.
                assert_eq!(
                    (
                        reset.scope.monitor_id,
                        reset.scope.topology_generation,
                        reset.scope.stream_epoch
                    ),
                    (0, 0, 0)
                );
                self.resets.push(reset.reason);
                self.generation = reset.scope.generation.get();
            }
            self.mirror
                .apply(&decoded, &mut ResidualTileDecoder, &mut NoSurfacePixels)
                .unwrap();
            self.records += 1;
        }
    }
}

#[test]
fn a_canvas_deck_gets_records_that_replay_to_the_pier_mirror_over_every_scenario() {
    for kind in [
        ScenarioKind::Idle,
        ScenarioKind::Typing,
        ScenarioKind::Scroll,
        ScenarioKind::Drag,
        ScenarioKind::Video,
        ScenarioKind::Editor,
    ] {
        let start = Instant::now();
        let mut encode = CanvasEncodeLoop::new(
            config(canvas_mode()),
            StubEncoder::default(),
            Box::new(ResidualTilePacker),
            start,
        )
        .unwrap();
        assert!(encode.is_canvas());
        let scenario = Scenario::new(WIDTH as usize, HEIGHT as usize, kind, 7);
        let mut replay = Replay::new();
        let mut pixels = Vec::new();
        let mut digests = 0;
        for step in 0..90u32 {
            scenario.render(u64::from(step), &mut pixels);
            let now = start + TICK * step;
            let tick = encode.tick(frame(&pixels), None, now).unwrap();
            replay.apply(&tick);
            replay.ack(&mut encode);
            if let Some((generation, digest)) = tick.digest {
                assert_eq!(digest.fence % CANVAS_DIGEST_EVERY_FENCES, 0);
                assert_eq!(generation, 1);
                assert_eq!(Some(digest), replay.mirror.digest(), "{kind:?} step {step}");
                digests += 1;
            }
            assert_eq!(
                encode.mirror_digest().map(|(_, digest)| digest),
                replay.mirror.digest(),
                "{kind:?} step {step}: the Deck's canvas equals the Pier's mirror"
            );
        }
        assert_eq!(replay.resets, [CanvasResetReason::Start], "{kind:?}");
        let stats = encode.take_stats();
        assert_eq!(stats.ticks, 90);
        assert_eq!(stats.errors, 0);
        assert!(
            stats
                .line(true, encode.governor_state())
                .contains("software_loop=canvas")
        );
        if kind != ScenarioKind::Idle {
            assert!(replay.records > 2, "{kind:?}");
        }
        // Idle sends the start repaint and keepalives only: no digest fence.
        if kind == ScenarioKind::Idle {
            assert_eq!(digests, 0);
        }
    }
}

#[test]
fn an_idle_canvas_asks_for_a_picture_only_for_its_keepalive() {
    let start = Instant::now();
    let mut encode = CanvasEncodeLoop::new(
        config(canvas_mode()),
        StubEncoder::default(),
        Box::new(ResidualTilePacker),
        start,
    )
    .unwrap();
    assert!(encode.wants_frame(start, false), "the first picture");
    let pixels = vec![0x40; (WIDTH * HEIGHT * 4) as usize];
    let first = encode.tick(frame(&pixels), None, start).unwrap();
    assert!(first.records[0].is_recovery_point());
    let later = start + KEEPALIVE / 2;
    assert!(!encode.wants_frame(later, false));
    assert!(encode.wants_frame(later, true), "OS damage");
    assert!(encode.wants_frame(start + KEEPALIVE, false), "keepalive");
    // A reset is owed at once, not at the keepalive, on a still screen.
    encode.request_recovery();
    assert!(encode.wants_frame(later, false), "recovery");
    let reset = encode.tick(frame(&pixels), None, later).unwrap();
    assert!(reset.records[0].is_recovery_point());
    assert!(!encode.wants_frame(later, false));
    encode.on_control(&CanvasControlMsg::CanvasResetRequest {
        monitor_id: 0,
        generation: 2,
        reason: "decode".to_owned(),
    });
    assert!(encode.wants_frame(later, false), "Deck reset request");
}

#[test]
fn recovery_and_a_deck_reset_request_start_new_generations() {
    let start = Instant::now();
    let mut encode = CanvasEncodeLoop::new(
        config(canvas_mode()),
        StubEncoder::default(),
        Box::new(ResidualTilePacker),
        start,
    )
    .unwrap();
    let mut replay = Replay::new();
    let pixels = vec![0x20; (WIDTH * HEIGHT * 4) as usize];
    replay.apply(&encode.tick(frame(&pixels), None, start).unwrap());
    // A dropped record (the Pier's queue) or a Deck's full-frame request.
    encode.request_recovery();
    replay.apply(&encode.tick(frame(&pixels), None, start + TICK).unwrap());
    // A Deck's reset request for the generation it holds.
    encode.on_control(&CanvasControlMsg::CanvasResetRequest {
        monitor_id: 0,
        generation: 2,
        reason: "decode".to_owned(),
    });
    replay.apply(&encode.tick(frame(&pixels), None, start + TICK * 2).unwrap());
    // Another monitor's request is not this loop's.
    encode.on_control(&CanvasControlMsg::CanvasResetRequest {
        monitor_id: 3,
        generation: 1,
        reason: "decode".to_owned(),
    });
    replay.apply(&encode.tick(frame(&pixels), None, start + TICK * 3).unwrap());
    assert_eq!(
        replay.resets,
        [
            CanvasResetReason::Start,
            CanvasResetReason::Recovery,
            CanvasResetReason::ResetRequest
        ]
    );
    assert_eq!(
        encode.mirror_digest().map(|(_, d)| d),
        replay.mirror.digest()
    );
    // A Pier frame-rate request leaves a canvas at its tick rate.
    assert!(!encode.set_frame_rate(30));
}

/// A mandatory reset under an optional backlog: a Deck that acknowledges
/// nothing leaves the loop with its optional window full, yet its reset
/// request is answered at the next tick, first, and the new generation
/// replays to the Pier's mirror.
#[test]
fn a_reset_request_is_served_at_once_under_an_optional_backlog() {
    let start = Instant::now();
    let mut encode = CanvasEncodeLoop::new(
        config(canvas_mode()),
        StubEncoder::default(),
        Box::new(ResidualTilePacker),
        start,
    )
    .unwrap();
    let scenario = Scenario::new(WIDTH as usize, HEIGHT as usize, ScenarioKind::Video, 7);
    let mut replay = Replay::new();
    let mut pixels = Vec::new();
    let mut step = 0_u32;
    while encode.encoder().surface_pictures < 3 {
        assert!(step < 120, "the video scenario paints surfaces");
        scenario.render(u64::from(step), &mut pixels);
        replay.apply(
            &encode
                .tick(frame(&pixels), None, start + TICK * step)
                .unwrap(),
        );
        step += 1;
    }
    // Never acknowledged: the optional window stays full.
    for _ in 0..10 {
        scenario.render(u64::from(step), &mut pixels);
        replay.apply(
            &encode
                .tick(frame(&pixels), None, start + TICK * step)
                .unwrap(),
        );
        step += 1;
    }
    let painted = encode.encoder().surface_pictures;
    assert!(painted <= 3, "optional work waits for acknowledgements");
    encode.on_control(&CanvasControlMsg::CanvasResetRequest {
        monitor_id: 0,
        generation: 1,
        reason: "decode".to_owned(),
    });
    let now = start + TICK * step;
    assert!(encode.wants_frame(now, false));
    let tick = encode.tick(frame(&pixels), None, now).unwrap();
    assert!(tick.records[0].is_recovery_point(), "the reset goes first");
    replay.apply(&tick);
    assert_eq!(
        replay.resets,
        [CanvasResetReason::Start, CanvasResetReason::ResetRequest]
    );
    assert_eq!(
        encode.mirror_digest().map(|(_, digest)| digest),
        replay.mirror.digest()
    );
}

/// Input-neighbourhood priority through the whole plumbing: where the Pier
/// applied input becomes a `CANVAS-INPUT` line (`CanvasInputFeed`), crosses
/// capenc's control parser and reaches the loop, whose planner paints the
/// typed text there in the tick it changes even when the CPU budget defers
/// the rest. The same session without that input defers some of it, and a
/// line for another monitor or topology generation changes nothing.
#[test]
fn input_the_pier_applied_is_painted_first_under_a_starved_budget() {
    use crate::canvas::serve::{CanvasInputFeed, CanvasServePlan};
    use crate::capenc_control::CapencControlCommand;
    use arcen_keel::PixelRect;
    let starved = LoopMode::Canvas(CanvasLoopTerms {
        terms: terms(),
        monitor_id: 0,
        topology_generation: 0,
        stream_epoch: 0,
        cpu_share: CpuShare::from_percent(1).unwrap(),
        logical_cores: 1,
    });
    let plan = CanvasServePlan {
        status: None,
        terms: Some(terms()),
    };
    // `forward`: Some(lines are rewritten by this) or None (no input).
    let run = |forward: Option<fn(CapencControlCommand) -> CapencControlCommand>| -> u32 {
        let start = Instant::now();
        let mut encode = CanvasEncodeLoop::new(
            config(starved),
            StubEncoder::default(),
            Box::new(ResidualTilePacker),
            start,
        )
        .unwrap();
        let mut feed = CanvasInputFeed::for_plan(&plan).unwrap();
        let editor = Scenario::new(WIDTH as usize, HEIGHT as usize, ScenarioKind::Editor, 9);
        let mut pixels = Vec::new();
        let mut rects: Vec<PixelRect> = Vec::new();
        let mut waiting_at_input = 0;
        for tick in 0..120_u64 {
            let now = start + TICK * u32::try_from(tick).unwrap();
            editor.render(tick, &mut pixels);
            editor.damage_rects(tick.saturating_sub(1), tick, &mut rects);
            let typed = rects.last().copied();
            if let (Some(rect), Some(forward)) = (typed, forward) {
                let x = (rect.x + rect.width / 2) as f64 / f64::from(WIDTH);
                let y = (rect.y + rect.height / 2) as f64 / f64::from(HEIGHT);
                if let Some(command) = feed.applied(x, y, now) {
                    // The line capenc reads, parsed as its control thread does.
                    let line = forward(command).as_line();
                    if let Ok(CapencControlCommand::CanvasInput {
                        monitor_id,
                        topology_generation,
                        x,
                        y,
                    }) = line.parse()
                    {
                        encode.note_input(monitor_id, topology_generation, x, y, now);
                    }
                }
            }
            encode.tick(frame(&pixels), None, now).unwrap();
            if tick == 0 {
                continue;
            }
            if let Some(rect) = typed {
                let center = (
                    u32::try_from(rect.x + rect.width / 2).unwrap(),
                    u32::try_from(rect.y + rect.height / 2).unwrap(),
                );
                waiting_at_input += u32::from(encode.is_pending_at(center.0, center.1));
            }
        }
        waiting_at_input
    };
    assert_eq!(
        run(Some(|command| command)),
        0,
        "the input neighbourhood waits"
    );
    assert!(run(None) > 0, "without input the budget defers the typing");
    let other_monitor = run(Some(|command| match command {
        CapencControlCommand::CanvasInput {
            topology_generation,
            x,
            y,
            ..
        } => CapencControlCommand::CanvasInput {
            monitor_id: 3,
            topology_generation,
            x,
            y,
        },
        other => other,
    }));
    let stale_generation = run(Some(|command| match command {
        CapencControlCommand::CanvasInput {
            monitor_id, x, y, ..
        } => CapencControlCommand::CanvasInput {
            monitor_id,
            topology_generation: 7,
            x,
            y,
        },
        other => other,
    }));
    assert!(other_monitor > 0 && stale_generation > 0);
}

#[test]
fn an_older_deck_gets_full_frames_from_the_same_loop_gated_by_keel() {
    let start = Instant::now();
    let mut encode = CanvasEncodeLoop::new(
        config(LoopMode::FullFrame),
        StubEncoder::default(),
        Box::new(ResidualTilePacker),
        start,
    )
    .unwrap();
    assert!(!encode.is_canvas());
    let still = vec![0x30; (WIDTH * HEIGHT * 4) as usize];
    let first = encode.tick(frame(&still), None, start).unwrap();
    assert!(matches!(
        first.records.as_slice(),
        [LoopRecord::AccessUnit { keyframe: true, .. }]
    ));
    // Unchanged: nothing is staged or encoded, and no picture is wanted.
    let later = start + TICK;
    assert!(!encode.wants_frame(later, false));
    assert!(
        encode
            .tick(frame(&still), None, later)
            .unwrap()
            .records
            .is_empty()
    );
    // One changed band: only its rows are staged.
    let mut band = still.clone();
    band[(WIDTH * 4 * 40) as usize..(WIDTH * 4 * 56) as usize].fill(0xC0);
    let changed = encode.tick(frame(&band), None, later + TICK).unwrap();
    assert!(matches!(
        changed.records.as_slice(),
        [LoopRecord::AccessUnit {
            keyframe: false,
            ..
        }]
    ));
    // OS damage narrows the hash to the reported rect.
    let mut damage = ExternalDamage::new(WIDTH as usize, HEIGHT as usize).unwrap();
    damage.mark_rect(arcen_keel::PixelRect {
        x: 0,
        y: 40,
        width: WIDTH as usize,
        height: 16,
    });
    let narrowed = encode.tick(frame(&still), Some(&damage), later + TICK * 2);
    assert_eq!(narrowed.unwrap().records.len(), 1);
    // Recovery: a keyframe of the retained picture.
    encode.request_recovery();
    assert!(encode.wants_frame(later + TICK * 3, false));
    let recovered = encode.tick(frame(&still), None, later + TICK * 3).unwrap();
    assert!(recovered.records[0].is_recovery_point());
    // Keepalive.
    assert!(encode.wants_frame(later + TICK * 3 + KEEPALIVE, false));
    let kept = encode
        .tick(frame(&still), None, later + TICK * 3 + KEEPALIVE)
        .unwrap();
    assert_eq!(kept.records.len(), 1);
    assert_eq!(
        encode.encoder().staged_rows,
        ["full", "rows", "rows", "clean", "clean"]
    );
    // Live changes reach the full-frame session.
    assert!(encode.set_frame_rate(24));
    encode.set_link_bitrate(3_000_000);
    assert_eq!(encode.encoder().rate, Some(24));
    assert_eq!(encode.encoder().bitrate, Some(3_000_000));
    assert!(
        encode
            .take_stats()
            .line(false, None)
            .contains("software_loop=full_frame")
    );
}

#[test]
fn a_failed_full_frame_encode_rebuilds_the_planes_and_sends_a_keyframe() {
    let start = Instant::now();
    let mut encode = CanvasEncodeLoop::new(
        config(LoopMode::FullFrame),
        StubEncoder::default(),
        Box::new(ResidualTilePacker),
        start,
    )
    .unwrap();
    let still = vec![0x30; (WIDTH * HEIGHT * 4) as usize];
    encode.tick(frame(&still), None, start).unwrap();
    let mut band = still.clone();
    band[..(WIDTH * 4 * 16) as usize].fill(0x90);
    encode.encoder_mut().fail_next_full = true;
    assert!(encode.tick(frame(&band), None, start + TICK).is_err());
    let next = encode.tick(frame(&band), None, start + TICK * 2).unwrap();
    assert!(next.records[0].is_recovery_point());
    assert_eq!(encode.encoder().staged_rows.last(), Some(&"full"));
}

#[test]
fn a_new_picture_size_is_refused_so_the_adapter_builds_a_new_loop() {
    let start = Instant::now();
    let mut encode = CanvasEncodeLoop::new(
        config(canvas_mode()),
        StubEncoder::default(),
        Box::new(ResidualTilePacker),
        start,
    )
    .unwrap();
    let pixels = vec![0; 64 * 64 * 4];
    let small = BgraFrame::new(&pixels, 64, 64, 256).unwrap();
    assert_eq!(
        encode.tick(small, None, start).unwrap_err(),
        LoopError::Geometry
    );
}

/// The real encoder: surfaces and full frames through the shared `OpenH264`
/// session, and the access units decode.
#[cfg(feature = "software-h264-source")]
mod software {
    use super::*;
    use crate::video::{SoftwareH264Session, SoftwareSessionConfig, SoftwareSurfaces};

    #[test]
    fn video_surfaces_encode_through_the_shared_session_and_the_mirror_holds() {
        let start = Instant::now();
        let mut encode = CanvasEncodeLoop::new(
            config(canvas_mode()),
            SoftwareSurfaces::for_canvas(canvas_surface_video(), 60),
            Box::new(ResidualTilePacker),
            start,
        )
        .unwrap();
        let scenario = Scenario::new(WIDTH as usize, HEIGHT as usize, ScenarioKind::Video, 3);
        let mut replay = Replay::new();
        let mut pixels = Vec::new();
        let mut surface_decoder = openh264::decoder::Decoder::new().unwrap();
        let mut decoded_pictures = 0;
        for step in 0..60u32 {
            scenario.render(u64::from(step), &mut pixels);
            let tick = encode
                .tick(frame(&pixels), None, start + TICK * step)
                .unwrap();
            replay.apply(&tick);
            replay.ack(&mut encode);
            for record in &tick.records {
                if let Ok(CanvasRecord::Frame(frame)) = CanvasRecord::decode(record.bytes()) {
                    for command in &frame.commands {
                        if let arcen_protocol::canvas::CanvasCommand::SurfacePaint(paint) = command
                            && surface_decoder
                                .decode(paint.access_unit)
                                .ok()
                                .flatten()
                                .is_some()
                        {
                            decoded_pictures += 1;
                        }
                    }
                }
            }
        }
        let stats = encode.take_stats();
        assert!(stats.pictures > 10, "{stats:?}");
        assert!(
            decoded_pictures > 10,
            "surface pictures decode: {decoded_pictures}"
        );
        assert_eq!(
            encode.mirror_digest().map(|(_, d)| d),
            replay.mirror.digest()
        );
    }

    #[test]
    fn full_frames_encode_through_the_shared_session() {
        let video = canvas_surface_video();
        let session = SoftwareH264Session::new(SoftwareSessionConfig {
            threads: 1,
            ..SoftwareSessionConfig::for_stream(WIDTH, HEIGHT, 30, video)
        })
        .unwrap();
        let start = Instant::now();
        let mut encode = CanvasEncodeLoop::new(
            config(LoopMode::FullFrame),
            SoftwareSurfaces::for_full_frame(session, video, 30),
            Box::new(ResidualTilePacker),
            start,
        )
        .unwrap();
        let mut decoder = openh264::decoder::Decoder::new().unwrap();
        let scenario = Scenario::new(WIDTH as usize, HEIGHT as usize, ScenarioKind::Typing, 1);
        let mut pixels = Vec::new();
        let mut units = 0;
        for step in 0..20u32 {
            scenario.render(u64::from(step), &mut pixels);
            let tick = encode
                .tick(frame(&pixels), None, start + TICK * step)
                .unwrap();
            for record in tick.records {
                let LoopRecord::AccessUnit { bytes, keyframe } = record else {
                    panic!("full frames only");
                };
                assert_eq!(keyframe, units == 0);
                assert!(decoder.decode(&bytes).unwrap().is_some());
                units += 1;
            }
        }
        assert!(units > 1);
    }
}
