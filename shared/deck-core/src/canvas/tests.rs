// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The Deck canvas against a fake sink whose executor is the shared CPU
//! oracle (`arcen_media::canvas::CanvasPixels`): advertisement, ordering,
//! fences and admission, surfaces, the cache budget, flow control, reset
//! recovery and refusal.

use super::*;
use crate::decode::{DecodedVideoFrame, NativePictureFormat, VideoDecodeError};
use crate::present::{
    LayerColourState, PresentOutcome, PresentableFrame, PresenterFailure, PresenterFailureStage,
    PresenterPipeline, PresenterTelemetryDelta,
};
use arcen_media::canvas::{
    CanvasExecError, CanvasMirror, CanvasPixels, NoSurfacePixels, PackedTileCodes, SurfaceDecoder,
    SurfacePicture,
};
use arcen_media::refine::tile::{decode_residuals, encode_residuals};
use arcen_media::{SessionMonitorId, TopologyGeneration};
use arcen_protocol::canvas::{
    CanvasCommand, CanvasFrame, CanvasMonitorStatusMsg, CanvasPixelFormat, CanvasRect, CanvasReset,
    CanvasResetReason, CanvasTile, SURFACE_CODEC_H264, SurfacePaint, canvas_block_count,
};
use arcen_protocol::refine::{BlockRuns, CODEC_DELTA_ZSTD1};
use std::cell::Cell;
use std::num::NonZeroU32;
use std::rc::Rc;

const WIDTH: u32 = 96;
const HEIGHT: u32 = 64;
const NOW: PresentTiming = PresentTiming::Newest;

// ------------------------------------------------------------ fake decoder

/// A picture the fake decoder "decodes": an access unit is `[width u16,
/// height u16, r, g, b]` and decodes to that solid colour.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Picture {
    width: u32,
    height: u32,
    bgra: Vec<u8>,
}

struct FakeDecoder {
    alive: Rc<Cell<i32>>,
}

impl Drop for FakeDecoder {
    fn drop(&mut self) {
        self.alive.set(self.alive.get() - 1);
    }
}

impl VideoDecoderBackend for FakeDecoder {
    type NativeFrame = Picture;

    fn set_session_color(&mut self, _session_color: SessionColor) {}

    fn set_prefer_native_frames(&mut self, _prefer: bool) -> bool {
        false
    }

    fn set_decode_latency_policy(&mut self, _policy: DecodeLatencyPolicy) {}

    fn decode(
        &mut self,
        header: &VideoHeader,
        payload: &[u8],
    ) -> Result<Option<DecodedVideoFrame<Picture>>, VideoDecodeError> {
        let [w0, w1, h0, h1, r, g, b] = payload else {
            return Err(VideoDecodeError::Backend("not a test access unit".into()));
        };
        assert_eq!(header.codec, VideoCodec::H264);
        let width = u32::from(u16::from_be_bytes([*w0, *w1]));
        let height = u32::from(u16::from_be_bytes([*h0, *h1]));
        let bgra = [*b, *g, *r, 255].repeat((width * height) as usize);
        let video = crate::decode::header_video_configuration(
            header,
            arcen_media::VideoCodec::H264,
            SessionColor::default(),
        )?;
        Ok(Some(DecodedVideoFrame {
            width: width as usize,
            height: height as usize,
            rgba: Vec::new(),
            timestamp_ms: header.timestamp_ms,
            pixel_format: "BGRA".into(),
            backend: "test",
            native: Some(NativeDecodedVideoFrame {
                picture: Picture {
                    width,
                    height,
                    bgra,
                },
                format: NativePictureFormat::Bgra8,
                video,
            }),
        }))
    }

    fn backend_name(&self) -> &'static str {
        "test"
    }

    fn is_hardware_accelerated(&self) -> Option<bool> {
        Some(false)
    }

    fn wants_keyframe(&self) -> bool {
        false
    }

    fn notify_discontinuity(&mut self) {}
}

fn access_unit(width: u16, height: u16, rgb: [u8; 3]) -> Vec<u8> {
    let mut au = width.to_be_bytes().to_vec();
    au.extend_from_slice(&height.to_be_bytes());
    au.extend_from_slice(&rgb);
    au
}

// --------------------------------------------------------------- fake sink

/// Tiles as raw residuals (the default build has no Zstandard).
struct ResidualTiles;

impl TileDecoder for ResidualTiles {
    fn decode(
        &mut self,
        payload: &[u8],
        width: u32,
        height: u32,
    ) -> Result<TileCodes, CanvasExecError> {
        decode_residuals(payload, width, height, 8).map_err(|_| CanvasExecError::Tile)
    }
}

/// The decoded inputs as the CPU executor's surface seam.
struct InputSurfaces {
    pictures: VecDeque<Option<Picture>>,
    current: Option<Picture>,
}

impl SurfaceDecoder for InputSurfaces {
    fn open(&mut self, _surface: u8, _width: u32, _height: u32) {}

    fn close(&mut self, _surface: u8) {}

    fn decode(
        &mut self,
        _surface: u8,
        _access_unit: &[u8],
        _idr: bool,
    ) -> Result<Option<SurfacePicture<'_>>, CanvasExecError> {
        self.current = self.pictures.pop_front().ok_or(CanvasExecError::Surface)?;
        Ok(self.current.as_ref().map(|picture| SurfacePicture {
            width: picture.width,
            height: picture.height,
            bgra: &picture.bgra,
        }))
    }
}

/// A sink whose canvas executor is the shared CPU oracle.
#[derive(Default)]
struct FakeSink {
    canvases: BTreeMap<u16, CanvasPixels>,
    /// `(monitor, generation, fence, ops)` of every execution, in order.
    executed: Vec<(u16, u32, u64, usize)>,
    hidden: bool,
    fail: bool,
    readbacks: VecDeque<CanvasReadback>,
}

impl VideoPresenterSink for FakeSink {
    type NativeFrame = Picture;

    const CANVAS_EXECUTOR: Option<CanvasExecutorFacts> = Some(CanvasExecutorFacts {
        max_canvas_px: 16_384,
    });

    fn pipeline(&self) -> PresenterPipeline {
        PresenterPipeline::EightBit
    }

    fn present(&mut self, _frame: &PresentableFrame<Picture>) -> PresentOutcome {
        PresentOutcome::Presented
    }

    fn represent_last(&mut self) -> Option<PresentOutcome> {
        None
    }

    fn set_surface_visible(&mut self, visible: bool) {
        self.hidden = !visible;
    }

    fn take_async_status(&mut self) -> Option<PresentOutcome> {
        None
    }

    fn has_presented_picture(&self) -> bool {
        !self.executed.is_empty()
    }

    fn take_telemetry(&self) -> PresenterTelemetryDelta {
        PresenterTelemetryDelta::default()
    }

    fn colour_state(&self) -> Option<LayerColourState> {
        None
    }

    fn teardown(&mut self) {
        self.canvases.clear();
    }

    fn execute(&mut self, canvas: &CanvasExecution<'_, '_, Picture>) -> CanvasOutcome {
        if self.fail {
            return CanvasOutcome::Failed(PresenterFailure {
                stage: PresenterFailureStage::DeviceLost,
                detail: "test device lost",
            });
        }
        let work = canvas.work;
        assert_eq!(canvas.inputs.len(), work.ops.len(), "inputs align with ops");
        let mut tiles = PackedTileCodes::default();
        let mut surfaces = InputSurfaces {
            pictures: VecDeque::new(),
            current: None,
        };
        for input in canvas.inputs {
            match input {
                CanvasOpInput::Tile(codes) => tiles.push(codes.clone()),
                CanvasOpInput::Surface(picture) => surfaces
                    .pictures
                    .push_back(picture.as_ref().map(|frame| frame.picture.clone())),
                CanvasOpInput::None => {}
            }
        }
        let pixels = self.canvases.entry(work.monitor_id).or_default();
        if pixels.execute(work, &mut tiles, &mut surfaces).is_err() {
            return CanvasOutcome::Failed(PresenterFailure {
                stage: PresenterFailureStage::Submission,
                detail: "cpu executor refused",
            });
        }
        self.executed.push((
            work.monitor_id,
            work.generation.get(),
            work.fence,
            work.ops.len(),
        ));
        if canvas.readback {
            self.readbacks.push_back(CanvasReadback {
                monitor_id: work.monitor_id,
                generation: work.generation,
                fence: work.fence,
                stride: pixels.stride(),
                bgra: pixels.bgra().to_vec(),
            });
        }
        if self.hidden || !canvas.present {
            CanvasOutcome::Applied
        } else {
            CanvasOutcome::Presented
        }
    }

    fn take_canvas_readback(&mut self) -> Option<CanvasReadback> {
        self.readbacks.pop_front()
    }
}

/// A Deck sink without a canvas executor: what both Decks ship until
/// their executors exist.
#[derive(Default)]
struct StubSink;

impl VideoPresenterSink for StubSink {
    type NativeFrame = Picture;

    const CANVAS_EXECUTOR: Option<CanvasExecutorFacts> = None;

    fn pipeline(&self) -> PresenterPipeline {
        PresenterPipeline::EightBit
    }

    fn present(&mut self, _frame: &PresentableFrame<Picture>) -> PresentOutcome {
        PresentOutcome::Presented
    }

    fn represent_last(&mut self) -> Option<PresentOutcome> {
        None
    }

    fn set_surface_visible(&mut self, _visible: bool) {}

    fn take_async_status(&mut self) -> Option<PresentOutcome> {
        None
    }

    fn has_presented_picture(&self) -> bool {
        false
    }

    fn take_telemetry(&self) -> PresenterTelemetryDelta {
        PresenterTelemetryDelta::default()
    }

    fn colour_state(&self) -> Option<LayerColourState> {
        None
    }

    fn teardown(&mut self) {}

    fn execute(&mut self, _canvas: &CanvasExecution<'_, '_, Picture>) -> CanvasOutcome {
        CanvasOutcome::Unsupported
    }
}

// ------------------------------------------------------------------ records

fn scope(monitor_id: u16, generation: u32) -> CanvasScope {
    CanvasScope {
        monitor_id,
        topology_generation: 0,
        stream_epoch: 0,
        generation: NonZeroU32::new(generation).unwrap(),
    }
}

fn reset(scope: CanvasScope, width: u32, height: u32) -> Vec<u8> {
    CanvasReset {
        scope,
        reason: CanvasResetReason::Start,
        format: CanvasPixelFormat::Bgra8,
        width,
        height,
    }
    .encode()
    .unwrap()
}

fn frame(
    scope: CanvasScope,
    fence: u64,
    width: u32,
    height: u32,
    commands: Vec<CanvasCommand<'_>>,
) -> Vec<u8> {
    CanvasFrame {
        scope,
        fence,
        timestamp_ms: 1_000 + u32::try_from(fence).unwrap(),
        block_count: canvas_block_count(width, height),
        commands,
    }
    .encode()
    .unwrap()
}

fn rect(x: u16, y: u16, width: u16, height: u16) -> CanvasRect {
    CanvasRect {
        x,
        y,
        width,
        height,
    }
}

/// A raw-residual tile payload of a ramp.
fn tile_payload(width: u32, height: u32, seed: u16) -> Vec<u8> {
    let pixels = (width * height) as usize;
    let mut samples = Vec::with_capacity(pixels * 3);
    for plane in 0..3_u16 {
        for at in 0..pixels {
            samples.push((u16::try_from(at).unwrap() * 7 + seed + plane * 40) % 256);
        }
    }
    encode_residuals(&TileCodes::new(width, height, samples).unwrap(), 8).unwrap()
}

fn status(monitors: &[u16]) -> CanvasStatusMsg {
    CanvasStatusMsg {
        state: CanvasStateMsg::On,
        reason: None,
        tile_codec: Some(arcen_protocol::refine::CODEC_DELTA_ZSTD1_TOKEN.to_owned()),
        cache_bytes: Some(cache_budget_bytes(MIN_CACHE_MIB)),
        cache_slots: Some(cache_slots(cache_budget_bytes(MIN_CACHE_MIB))),
        max_surfaces_per_monitor: Some(DECK_SURFACES_PER_MONITOR),
        monitors: monitors
            .iter()
            .map(|monitor_id| CanvasMonitorStatusMsg {
                monitor_id: *monitor_id,
                state: CanvasStateMsg::On,
                reason: None,
            })
            .collect(),
    }
}

fn request(cache_mib: u32) -> CanvasRequest {
    CanvasRequest::for_executor(
        Some(CanvasExecutorFacts {
            max_canvas_px: 16_384,
        }),
        true,
        true,
        cache_mib,
    )
}

struct Rig {
    canvas: DeckCanvas<FakeDecoder>,
    sink: FakeSink,
    alive: Rc<Cell<i32>>,
    now: Instant,
}

impl Rig {
    fn new(monitors: &[u16]) -> Self {
        Self::with_cache(monitors, DEFAULT_CACHE_MIB)
    }

    fn with_cache(monitors: &[u16], cache_mib: u32) -> Self {
        let mut canvas = DeckCanvas::new(&request(cache_mib));
        canvas.on_hello(Some(&status(monitors)));
        assert!(canvas.is_on());
        Self {
            canvas,
            sink: FakeSink::default(),
            alive: Rc::new(Cell::new(0)),
            now: Instant::now(),
        }
    }

    fn apply(&mut self, bytes: &[u8]) -> CanvasApplied {
        let alive = Rc::clone(&self.alive);
        let decoders = move || {
            alive.set(alive.get() + 1);
            FakeDecoder {
                alive: Rc::clone(&alive),
            }
        };
        let mut env = CanvasEnv {
            now: self.now,
            decoders: &decoders,
            tiles: &mut ResidualTiles,
            sink: Some(&mut self.sink),
            timing: NOW,
        };
        self.canvas.apply(bytes, &mut env)
    }

    fn apply_without_sink(&mut self, bytes: &[u8]) -> CanvasApplied {
        let alive = Rc::clone(&self.alive);
        let decoders = move || FakeDecoder {
            alive: Rc::clone(&alive),
        };
        let mut env: CanvasEnv<'_, FakeDecoder, FakeSink> = CanvasEnv {
            now: self.now,
            decoders: &decoders,
            tiles: &mut ResidualTiles,
            sink: None,
            timing: NOW,
        };
        self.canvas.apply(bytes, &mut env)
    }

    fn control(&mut self) -> Vec<CanvasControlMsg> {
        self.canvas.take_control_messages(self.now, |_| true, |_| 0)
    }

    fn advance(&mut self, by: Duration) {
        self.now += by;
    }
}

fn reset_requests(messages: &[CanvasControlMsg]) -> Vec<(u16, u32, String)> {
    messages
        .iter()
        .filter_map(|message| match message {
            CanvasControlMsg::CanvasResetRequest {
                monitor_id,
                generation,
                reason,
            } => Some((*monitor_id, *generation, reason.clone())),
            CanvasControlMsg::CanvasAck { .. } => None,
        })
        .collect()
}

fn acks(messages: &[CanvasControlMsg]) -> Vec<(u16, u32, u64, u64)> {
    messages
        .iter()
        .filter_map(|message| match message {
            CanvasControlMsg::CanvasAck {
                monitor_id,
                generation,
                fence,
                unprocessed_bytes,
                ..
            } => Some((*monitor_id, *generation, *fence, *unprocessed_bytes)),
            CanvasControlMsg::CanvasResetRequest { .. } => None,
        })
        .collect()
}

// -------------------------------------------------------------------- tests

#[test]
fn only_a_sink_with_an_executor_and_the_switch_on_advertises_canvas() {
    assert_eq!(
        CanvasRequest::for_sink::<StubSink>(true, DEFAULT_CACHE_MIB),
        CanvasRequest::Unavailable,
        "a Deck whose sink cannot execute canvas work never asks for it"
    );
    let facts = Some(CanvasExecutorFacts {
        max_canvas_px: 20_000,
    });
    assert_eq!(
        CanvasRequest::for_executor(facts, false, true, DEFAULT_CACHE_MIB),
        CanvasRequest::Unavailable,
        "a build that cannot decode tiles never asks"
    );
    assert_eq!(
        CanvasRequest::for_executor(facts, true, false, DEFAULT_CACHE_MIB),
        CanvasRequest::Off
    );
    let capability = CanvasRequest::for_executor(facts, true, true, DEFAULT_CACHE_MIB)
        .capability()
        .cloned()
        .unwrap();
    assert_eq!(capability.max_canvas_px, 16_384);
    assert_eq!(capability.max_surfaces_per_monitor, 2);
    assert_eq!(capability.cache_bytes, 100 * 1024 * 1024);
    assert_eq!(capability.cache_slots, 25_600);
    assert_eq!(capability.tile_codecs, ["delta-zstd1"]);
    assert!(
        capability.terms().is_some(),
        "a Pier accepts the capability"
    );
}

#[test]
fn the_cache_budget_defaults_to_100_mib_and_never_drops_below_16() {
    assert_eq!(cache_budget_bytes(DEFAULT_CACHE_MIB), 100 * MIB);
    assert_eq!(cache_budget_bytes(0), 16 * MIB);
    assert_eq!(cache_budget_bytes(8), 16 * MIB);
    assert_eq!(cache_budget_bytes(u32::MAX), MAX_CACHE_BYTES);
    assert_eq!(cache_slots(16 * MIB), 4_096);
    assert_eq!(cache_slots(MAX_CACHE_BYTES), MAX_CACHE_SLOTS);
}

#[test]
#[allow(clippy::too_many_lines, reason = "one scenario, record by record")]
fn records_execute_in_wire_order_and_the_canvas_equals_the_cpu_mirror() {
    let mut rig = Rig::new(&[0]);
    let s = scope(0, 1);
    let tile = tile_payload(32, 16, 3);
    let au = access_unit(32, 32, [200, 10, 10]);
    let records = vec![
        reset(s, WIDTH, HEIGHT),
        frame(
            s,
            1,
            WIDTH,
            HEIGHT,
            vec![
                CanvasCommand::Fill {
                    rect: rect(0, 0, 96, 64),
                    rgb: [10, 20, 30],
                },
                CanvasCommand::Tile(CanvasTile {
                    rect: rect(16, 16, 32, 16),
                    codec: CODEC_DELTA_ZSTD1,
                    depth: 8,
                    payload: &tile,
                }),
            ],
        ),
        frame(
            s,
            2,
            WIDTH,
            HEIGHT,
            vec![
                CanvasCommand::CacheStore {
                    slot: 3,
                    rect: rect(16, 16, 32, 16),
                },
                CanvasCommand::Copy {
                    src: rect(16, 16, 32, 16),
                    dst_x: 20,
                    dst_y: 22,
                },
                CanvasCommand::CachePaint {
                    slot: 3,
                    dst_x: 48,
                    dst_y: 32,
                },
                CanvasCommand::SurfaceOpen {
                    surface: 0,
                    codec: SURFACE_CODEC_H264,
                    width: 32,
                    height: 32,
                },
                CanvasCommand::SurfacePaint(SurfacePaint {
                    surface: 0,
                    idr: true,
                    dest: rect(64, 32, 32, 32),
                    blocks: BlockRuns::from_sorted_indices([2 * 6 + 4, 3 * 6 + 5]),
                    access_unit: &au,
                }),
            ],
        ),
    ];
    let terms = request(DEFAULT_CACHE_MIB)
        .capability()
        .and_then(CanvasCapability::terms)
        .unwrap();
    let mut mirror = CanvasMirror::new(0, terms);
    for bytes in &records {
        assert_eq!(rig.apply(bytes), CanvasApplied::Presented);
        let record = CanvasRecord::decode(bytes).unwrap();
        mirror
            .apply(&record, &mut ResidualTiles, &mut NoSurfacePixels)
            .unwrap();
    }
    assert_eq!(
        rig.sink.executed,
        [(0, 1, 0, 1), (0, 1, 1, 2), (0, 1, 2, 5)],
        "one execution per record, in wire order, every op handed over"
    );
    assert_eq!(rig.canvas.position(0), Some((1, 2)));
    assert_eq!(rig.canvas.open_surfaces(0), 1);
    assert_eq!(rig.alive.get(), 1, "one decoder for the open surface");

    // The surface painted its listed block with the decoded picture.
    let pixels = &rig.sink.canvases[&0];
    let at = (40 * WIDTH as usize + 72) * 4;
    assert_eq!(&pixels.bgra()[at..at + 4], &[10, 10, 200, 255]);

    // Readbacks happen only at the fences the Pier logs its mirror's digest
    // at (multiples of `CANVAS_DIGEST_EVERY_FENCES`).
    assert!(rig.sink.take_canvas_readback().is_none());

    // At such a fence the Deck's canvas digest equals the Pier mirror's
    // (exact blocks bit-identical, surface blocks excluded as lossy).
    rig.advance(DIGEST_INTERVAL);
    let later = frame(s, 30, WIDTH, HEIGHT, vec![]);
    assert_eq!(rig.apply(&later), CanvasApplied::Presented);
    mirror
        .apply(
            &CanvasRecord::decode(&later).unwrap(),
            &mut ResidualTiles,
            &mut NoSurfacePixels,
        )
        .unwrap();
    let readback = rig.sink.take_canvas_readback().unwrap();
    assert_eq!(readback.fence, 30);
    assert_eq!(rig.canvas.on_readback(&readback), mirror.digest());
    assert_eq!(
        rig.canvas.stats().last_digest.map(|report| report.digest),
        mirror.digest()
    );
}

#[test]
fn fences_must_increase_and_a_refused_record_loses_the_canvas_until_a_reset() {
    let mut rig = Rig::new(&[0]);
    let s = scope(0, 1);
    assert_eq!(
        rig.apply(&reset(s, WIDTH, HEIGHT)),
        CanvasApplied::Presented
    );
    assert_eq!(
        rig.apply(&frame(s, 5, WIDTH, HEIGHT, vec![])),
        CanvasApplied::Presented
    );
    assert_eq!(
        rig.apply(&frame(s, 5, WIDTH, HEIGHT, vec![])),
        CanvasApplied::Lost("fence_order")
    );
    assert!(rig.canvas.is_lost(0));
    assert_eq!(
        reset_requests(&rig.control()),
        [(0, 1, "fence_order".to_owned())]
    );
    // Later frames of the lost generation are never executed.
    let executed = rig.sink.executed.len();
    assert_eq!(
        rig.apply(&frame(s, 6, WIDTH, HEIGHT, vec![])),
        CanvasApplied::Dropped
    );
    assert_eq!(rig.sink.executed.len(), executed);
    assert_eq!(rig.canvas.stats().refusals, 1);
}

#[test]
fn records_are_admitted_by_monitor_state_and_region_fences_without_breaking_the_canvas() {
    // Single monitor: monitor 0, topology generation 0, stream epoch 0.
    let mut rig = Rig::new(&[0]);
    let good = scope(0, 1);
    assert_eq!(
        rig.apply(&reset(good, WIDTH, HEIGHT)),
        CanvasApplied::Presented
    );
    let stale = CanvasScope {
        stream_epoch: 4,
        ..good
    };
    assert_eq!(
        rig.apply(&frame(stale, 1, WIDTH, HEIGHT, vec![])),
        CanvasApplied::Dropped
    );
    let off = scope(3, 1);
    assert_eq!(
        rig.apply(&reset(off, WIDTH, HEIGHT)),
        CanvasApplied::Dropped
    );
    assert!(!rig.canvas.is_lost(0), "a stale record is not a wrong one");
    assert_eq!(
        rig.apply(&frame(good, 1, WIDTH, HEIGHT, vec![])),
        CanvasApplied::Presented
    );

    // Multi-monitor: the committed roster's generation and monitors.
    let mut rig = Rig::new(&[1, 2]);
    let ids = [
        SessionMonitorId::new(1).unwrap(),
        SessionMonitorId::new(2).unwrap(),
    ];
    rig.canvas.set_roster(Some(
        RegionFrameRoster::negotiated(TopologyGeneration::new(7).unwrap(), &ids).unwrap(),
    ));
    let on_roster = CanvasScope {
        monitor_id: 2,
        topology_generation: 7,
        stream_epoch: 1,
        generation: NonZeroU32::new(1).unwrap(),
    };
    assert_eq!(
        rig.apply(&reset(on_roster, WIDTH, HEIGHT)),
        CanvasApplied::Presented
    );
    let old_topology = CanvasScope {
        topology_generation: 6,
        ..on_roster
    };
    assert_eq!(
        rig.apply(&reset(old_topology, WIDTH, HEIGHT)),
        CanvasApplied::Dropped
    );
    let legacy = scope(0, 1);
    assert_eq!(
        rig.apply(&reset(legacy, WIDTH, HEIGHT)),
        CanvasApplied::Dropped
    );
    assert_eq!(rig.canvas.position(2), Some((1, 0)));
    assert_eq!(rig.canvas.stats().dropped, 2);
}

#[test]
fn each_surface_gets_its_own_decoder_and_at_most_two_per_monitor() {
    let mut rig = Rig::new(&[0]);
    let s = scope(0, 1);
    let au = access_unit(32, 32, [5, 6, 7]);
    let open = |surface| CanvasCommand::SurfaceOpen {
        surface,
        codec: SURFACE_CODEC_H264,
        width: 32,
        height: 32,
    };
    let paint = |surface| {
        CanvasCommand::SurfacePaint(SurfacePaint {
            surface,
            idr: true,
            dest: rect(0, 0, 32, 32),
            blocks: BlockRuns::from_sorted_indices([0]),
            access_unit: &au,
        })
    };
    assert_eq!(
        rig.apply(&reset(s, WIDTH, HEIGHT)),
        CanvasApplied::Presented
    );
    // Opening costs nothing; a decoder is created at the first paint.
    assert_eq!(
        rig.apply(&frame(s, 1, WIDTH, HEIGHT, vec![open(0), open(1)])),
        CanvasApplied::Presented
    );
    assert_eq!((rig.canvas.open_surfaces(0), rig.alive.get()), (2, 0));
    assert_eq!(
        rig.apply(&frame(s, 2, WIDTH, HEIGHT, vec![paint(0), paint(1)])),
        CanvasApplied::Presented
    );
    assert_eq!(rig.alive.get(), 2);
    // Reopening (a resize) replaces the decoder, even twice in one record;
    // closing drops it.
    assert_eq!(
        rig.apply(&frame(
            s,
            3,
            WIDTH,
            HEIGHT,
            vec![
                open(0),
                paint(0),
                open(0),
                paint(0),
                CanvasCommand::SurfaceClose { surface: 1 }
            ]
        )),
        CanvasApplied::Presented
    );
    assert_eq!(rig.alive.get(), 1);
    assert_eq!(rig.canvas.open_surfaces(0), 1);
    // A third surface is above this Deck's limit.
    assert_eq!(
        rig.apply(&frame(s, 4, WIDTH, HEIGHT, vec![open(2)])),
        CanvasApplied::Lost("surface_limit")
    );
    assert_eq!(rig.alive.get(), 0, "a lost canvas closes its surfaces");
}

#[test]
fn the_cache_budget_is_enforced_at_its_floor() {
    let mut rig = Rig::with_cache(&[0], 1);
    let s = scope(0, 1);
    let (width, height) = (2_048, 2_048);
    assert_eq!(
        rig.apply(&reset(s, width, height)),
        CanvasApplied::Presented
    );
    // 256 entries of 256 × 64 BGRA8 fill the 16 MiB floor exactly.
    let stores: Vec<CanvasCommand<'_>> = (0..256)
        .map(|slot| CanvasCommand::CacheStore {
            slot,
            rect: rect(0, 0, 256, 64),
        })
        .collect();
    assert_eq!(
        rig.apply(&frame(s, 1, width, height, stores)),
        CanvasApplied::Presented
    );
    assert_eq!(
        rig.apply(&frame(
            s,
            2,
            width,
            height,
            vec![CanvasCommand::CacheStore {
                slot: 256,
                rect: rect(0, 0, 16, 16),
            }]
        )),
        CanvasApplied::Lost("cache_budget")
    );
    assert_eq!(
        reset_requests(&rig.control()),
        [(0, 1, "cache_budget".to_owned())]
    );
}

#[test]
fn a_reset_recovers_a_lost_canvas_and_requests_are_not_repeated_too_often() {
    let mut rig = Rig::new(&[0]);
    let first = scope(0, 1);
    let au = access_unit(16, 16, [1, 2, 3]);
    assert_eq!(
        rig.apply(&reset(first, WIDTH, HEIGHT)),
        CanvasApplied::Presented
    );
    assert_eq!(
        rig.apply(&frame(
            first,
            1,
            WIDTH,
            HEIGHT,
            vec![CanvasCommand::SurfaceOpen {
                surface: 0,
                codec: SURFACE_CODEC_H264,
                width: 16,
                height: 16,
            }]
        )),
        CanvasApplied::Presented
    );
    // The executor loses its device.
    rig.sink.fail = true;
    let paint = CanvasCommand::SurfacePaint(SurfacePaint {
        surface: 0,
        idr: true,
        dest: rect(0, 0, 16, 16),
        blocks: BlockRuns::from_sorted_indices([0]),
        access_unit: &au,
    });
    assert_eq!(
        rig.apply(&frame(first, 2, WIDTH, HEIGHT, vec![paint])),
        CanvasApplied::Lost("executor_failed")
    );
    assert_eq!(rig.alive.get(), 0);
    rig.sink.fail = false;
    assert_eq!(reset_requests(&rig.control()).len(), 1);
    rig.advance(Duration::from_millis(500));
    assert!(reset_requests(&rig.control()).is_empty(), "rate-limited");
    rig.advance(RESET_REQUEST_RETRY);
    assert_eq!(
        reset_requests(&rig.control()),
        [(0, 1, "executor_failed".to_owned())]
    );
    // The Pier answers with a reset of a new generation; the canvas runs
    // again and is acknowledged in the new generation.
    let second = scope(0, 2);
    assert_eq!(
        rig.apply(&reset(second, WIDTH, HEIGHT)),
        CanvasApplied::Presented
    );
    assert!(!rig.canvas.is_lost(0));
    assert_eq!(
        rig.apply(&frame(second, 1, WIDTH, HEIGHT, vec![])),
        CanvasApplied::Presented
    );
    let messages = rig.control();
    assert!(reset_requests(&messages).is_empty());
    assert_eq!(acks(&messages), [(0, 2, 1, 0)]);
    assert_eq!(rig.canvas.stats().resets, 2);
    assert_eq!(rig.canvas.stats().reset_requests, 2);
}

#[test]
fn a_canvas_without_a_presenter_is_lost_and_reset_once_one_exists() {
    let mut rig = Rig::new(&[0]);
    let s = scope(0, 1);
    assert_eq!(
        rig.apply_without_sink(&reset(s, WIDTH, HEIGHT)),
        CanvasApplied::Lost("no_presenter")
    );
    assert!(
        rig.canvas
            .take_control_messages(rig.now, |_| false, |_| 0)
            .is_empty(),
        "no reset request while nothing could apply the reset"
    );
    assert_eq!(
        reset_requests(&rig.control()),
        [(0, 0, "no_presenter".to_owned())]
    );
    assert_eq!(
        rig.apply(&reset(scope(0, 2), WIDTH, HEIGHT)),
        CanvasApplied::Presented
    );
}

#[test]
fn acks_follow_presented_frames_and_come_at_least_every_quarter_second() {
    let mut rig = Rig::new(&[0]);
    let s = scope(0, 1);
    let au = access_unit(16, 16, [9, 9, 9]);
    assert_eq!(
        rig.apply(&reset(s, WIDTH, HEIGHT)),
        CanvasApplied::Presented
    );
    let surface = vec![
        CanvasCommand::SurfaceOpen {
            surface: 0,
            codec: SURFACE_CODEC_H264,
            width: 16,
            height: 16,
        },
        CanvasCommand::SurfacePaint(SurfacePaint {
            surface: 0,
            idr: true,
            dest: rect(0, 0, 16, 16),
            blocks: BlockRuns::from_sorted_indices([0]),
            access_unit: &au,
        }),
    ];
    assert_eq!(
        rig.apply(&frame(s, 1, WIDTH, HEIGHT, surface)),
        CanvasApplied::Presented
    );
    let messages =
        rig.canvas
            .take_control_messages(rig.now, |_| true, |monitor| u64::from(monitor) + 4_096);
    let [
        CanvasControlMsg::CanvasAck {
            monitor_id: 0,
            generation: 1,
            fence: 1,
            unprocessed_bytes: 4_096,
            decode_ms_p95,
        },
    ] = messages.as_slice()
    else {
        panic!("one ack for the presented frame: {messages:?}");
    };
    assert!(*decode_ms_p95 <= 1_000);

    // Hidden: frames are applied, not presented; acks come every 250 ms.
    rig.sink.set_surface_visible(false);
    assert_eq!(
        rig.apply(&frame(s, 2, WIDTH, HEIGHT, vec![])),
        CanvasApplied::Applied
    );
    rig.advance(Duration::from_millis(100));
    assert!(acks(&rig.control()).is_empty());
    rig.advance(Duration::from_millis(150));
    assert_eq!(acks(&rig.control()), [(0, 1, 2, 0)]);
    // Nothing arrived since: no ack.
    rig.advance(Duration::from_secs(1));
    assert!(acks(&rig.control()).is_empty());
}

#[test]
fn a_sink_without_an_executor_refuses_and_the_deck_stops_asking() {
    let mut canvas = DeckCanvas::<FakeDecoder>::new(&request(DEFAULT_CACHE_MIB));
    canvas.on_hello(Some(&status(&[0])));
    let alive = Rc::new(Cell::new(0));
    let decoders = || FakeDecoder {
        alive: Rc::clone(&alive),
    };
    let mut sink = StubSink;
    let now = Instant::now();
    let mut env = CanvasEnv {
        now,
        decoders: &decoders,
        tiles: &mut ResidualTiles,
        sink: Some(&mut sink),
        timing: NOW,
    };
    assert_eq!(
        canvas.apply(&reset(scope(0, 1), WIDTH, HEIGHT), &mut env),
        CanvasApplied::SinkRefused
    );
    assert_eq!(
        canvas.apply(&reset(scope(0, 2), WIDTH, HEIGHT), &mut env),
        CanvasApplied::SinkRefused
    );
    assert!(
        canvas
            .take_control_messages(now, |_| true, |_| 0)
            .is_empty(),
        "no reset request a sink without an executor could never apply"
    );
    assert_eq!(
        canvas.stats().line().unwrap(),
        "Screen updates: stopped: this Deck cannot show the canvas (no canvas executor); \
         turn Canvas screen updates off and reconnect for full frames"
    );
}

/// Permanent sink failure: a sink that cannot create a fresh canvas is not
/// asked again and again. After [`SINK_FAILED_RESETS`] failed resets the
/// canvas stops for the connection: no further reset request (each would be
/// a full repaint on the link), later records are ignored, and the
/// statistics say why and how to get full frames.
#[test]
fn a_sink_that_cannot_create_a_canvas_stops_after_failed_resets() {
    let mut rig = Rig::new(&[0]);
    assert_eq!(
        rig.apply(&reset(scope(0, 1), WIDTH, HEIGHT)),
        CanvasApplied::Presented
    );
    // The device is gone for good.
    rig.sink.fail = true;
    assert_eq!(
        rig.apply(&frame(scope(0, 1), 1, WIDTH, HEIGHT, vec![])),
        CanvasApplied::Lost("executor_failed")
    );
    assert_eq!(reset_requests(&rig.control()).len(), 1);
    // The Pier's first answer cannot be applied either: asked once more.
    assert_eq!(
        rig.apply(&reset(scope(0, 2), WIDTH, HEIGHT)),
        CanvasApplied::Lost("executor_failed")
    );
    rig.advance(RESET_REQUEST_RETRY);
    assert_eq!(reset_requests(&rig.control()).len(), 1);
    assert_eq!(
        rig.apply(&reset(scope(0, 3), WIDTH, HEIGHT)),
        CanvasApplied::SinkRefused
    );
    for _ in 0..3 {
        rig.advance(RESET_REQUEST_RETRY);
        assert!(
            rig.control().is_empty(),
            "no more resets for a sink that cannot apply them"
        );
    }
    assert_eq!(
        rig.apply(&reset(scope(0, 4), WIDTH, HEIGHT)),
        CanvasApplied::SinkRefused
    );
    assert_eq!(rig.canvas.next_due(rig.now), None);
    assert_eq!(
        rig.canvas.stats().line().unwrap(),
        "Screen updates: stopped: this Deck cannot show the canvas (test device lost); \
         turn Canvas screen updates off and reconnect for full frames"
    );
}

/// A temporary failure is not permanent: a sink that fails one record and
/// then applies the Pier's reset keeps its canvas.
#[test]
fn one_failed_reset_followed_by_a_good_one_keeps_the_canvas() {
    let mut rig = Rig::new(&[0]);
    rig.sink.fail = true;
    assert_eq!(
        rig.apply(&reset(scope(0, 1), WIDTH, HEIGHT)),
        CanvasApplied::Lost("executor_failed")
    );
    rig.sink.fail = false;
    assert_eq!(
        rig.apply(&reset(scope(0, 2), WIDTH, HEIGHT)),
        CanvasApplied::Presented
    );
    rig.sink.fail = true;
    assert_eq!(
        rig.apply(&reset(scope(0, 3), WIDTH, HEIGHT)),
        CanvasApplied::Lost("executor_failed"),
        "the count starts again after a reset that applied"
    );
    assert_eq!(rig.canvas.stats().sink_refused, None);
}

/// The first canvas arriving before the layer is attached (or while the
/// window is hidden) is not a failure: the sink applies it, nothing is
/// lost or asked for, and the next record is shown once the surface is.
#[test]
fn a_canvas_that_arrives_before_its_layer_is_shown_is_kept() {
    let mut rig = Rig::new(&[0]);
    rig.sink.hidden = true;
    assert_eq!(
        rig.apply(&reset(scope(0, 1), WIDTH, HEIGHT)),
        CanvasApplied::Applied
    );
    assert_eq!(
        rig.apply(&frame(
            scope(0, 1),
            1,
            WIDTH,
            HEIGHT,
            vec![CanvasCommand::Fill {
                rect: rect(0, 0, 16, 16),
                rgb: [1, 2, 3],
            }]
        )),
        CanvasApplied::Applied
    );
    assert!(!rig.canvas.is_lost(0));
    rig.advance(ACK_INTERVAL);
    assert!(reset_requests(&rig.control()).is_empty());
    rig.sink.hidden = false;
    assert_eq!(
        rig.apply(&frame(scope(0, 1), 2, WIDTH, HEIGHT, vec![])),
        CanvasApplied::Presented
    );
    assert_eq!(rig.canvas.stats().resets, 1);
}

#[test]
fn an_invalid_or_missing_host_answer_reads_as_off() {
    let mut canvas = DeckCanvas::<FakeDecoder>::new(&request(MIN_CACHE_MIB));
    canvas.on_hello(None);
    assert!(!canvas.is_on());
    assert_eq!(
        canvas.stats().line().unwrap(),
        "Screen updates: full frames (canvas off: not offered)"
    );
    let mut greedy = status(&[0]);
    greedy.cache_bytes = Some(MAX_CACHE_BYTES);
    canvas.on_hello(Some(&greedy));
    assert!(!canvas.is_on(), "terms above the Deck's offer are refused");
    let mut off = status(&[]);
    off.state = CanvasStateMsg::Off;
    off.tile_codec = None;
    off.reason = Some("not_software".to_owned());
    canvas.on_hello(Some(&off));
    assert_eq!(
        canvas.stats().line().unwrap(),
        "Screen updates: full frames (canvas off: not software)"
    );
}

#[test]
fn the_statistics_line_shows_how_the_screen_was_updated() {
    assert_eq!(
        CanvasStats::default().line(),
        None,
        "nothing on a Deck without canvas"
    );
    let off = DeckCanvas::<FakeDecoder>::new(&CanvasRequest::Off).stats();
    assert_eq!(
        off.line().unwrap(),
        "Screen updates: full frames (canvas off in Settings)"
    );
    let mut rig = Rig::new(&[0]);
    let s = scope(0, 1);
    let au = access_unit(32, 32, [1, 1, 1]);
    rig.apply(&reset(s, WIDTH, HEIGHT));
    rig.apply(&frame(
        s,
        1,
        WIDTH,
        HEIGHT,
        vec![
            // 1,024 tile pixels, 512 copied, 512 from the surface.
            CanvasCommand::Fill {
                rect: rect(0, 0, 32, 32),
                rgb: [0, 0, 0],
            },
            CanvasCommand::Copy {
                src: rect(0, 0, 32, 16),
                dst_x: 32,
                dst_y: 0,
            },
            CanvasCommand::SurfaceOpen {
                surface: 0,
                codec: SURFACE_CODEC_H264,
                width: 32,
                height: 32,
            },
            CanvasCommand::SurfacePaint(SurfacePaint {
                surface: 0,
                idr: true,
                dest: rect(64, 0, 32, 32),
                blocks: BlockRuns::from_sorted_indices([4, 5]),
                access_unit: &au,
            }),
        ],
    ));
    assert_eq!(
        rig.canvas.stats().line().unwrap(),
        "Screen updates: canvas (tiles 50%, surface 25%, copy 25%)"
    );
}

#[test]
fn queued_records_lost_by_the_inbox_lose_the_canvas() {
    let mut rig = Rig::new(&[0]);
    let s = scope(0, 1);
    rig.apply(&reset(s, WIDTH, HEIGHT));
    rig.canvas.records_lost(0);
    assert_eq!(
        reset_requests(&rig.control()),
        [(0, 1, "queue_overflow".to_owned())]
    );
    assert_eq!(
        rig.apply(&frame(s, 1, WIDTH, HEIGHT, vec![])),
        CanvasApplied::Dropped
    );
    assert_eq!(
        rig.apply(&reset(scope(0, 2), WIDTH, HEIGHT)),
        CanvasApplied::Presented
    );
}

#[test]
fn a_record_names_its_monitor_and_kind_in_its_prefix() {
    let bytes = reset(scope(5, 1), WIDTH, HEIGHT);
    assert_eq!(record_monitor(&bytes), Some(5));
    assert!(record_is_reset(&bytes));
    let bytes = frame(scope(5, 1), 1, WIDTH, HEIGHT, vec![]);
    assert!(!record_is_reset(&bytes));
    assert_eq!(record_monitor(&bytes[..4]), None);
}

#[test]
fn tile_codes_become_bgra_rows() {
    let codes = TileCodes::new(2, 1, vec![1, 2, 3, 4, 5, 6]).unwrap();
    assert_eq!(tile_bgra(&codes).unwrap(), [5, 3, 1, 255, 6, 4, 2, 255]);
    let deep = TileCodes::new(1, 1, vec![300, 0, 0]).unwrap();
    assert_eq!(tile_bgra(&deep), None);
}

#[test]
fn records_before_the_roster_lose_the_canvas_and_a_reset_restarts_it() {
    let mut rig = Rig::new(&[1, 2]);
    let early = CanvasScope {
        monitor_id: 1,
        topology_generation: 7,
        stream_epoch: 1,
        generation: NonZeroU32::new(1).unwrap(),
    };
    assert_eq!(
        rig.apply(&reset(early, WIDTH, HEIGHT)),
        CanvasApplied::Lost("roster_pending")
    );
    let ids = [
        SessionMonitorId::new(1).unwrap(),
        SessionMonitorId::new(2).unwrap(),
    ];
    rig.canvas.set_roster(Some(
        RegionFrameRoster::negotiated(TopologyGeneration::new(7).unwrap(), &ids).unwrap(),
    ));
    assert_eq!(
        reset_requests(&rig.control()),
        [(1, 0, "roster_pending".to_owned())]
    );
    let answer = CanvasScope {
        generation: NonZeroU32::new(2).unwrap(),
        ..early
    };
    assert_eq!(
        rig.apply(&reset(answer, WIDTH, HEIGHT)),
        CanvasApplied::Presented
    );
}

#[test]
fn a_record_with_many_tiles_reaches_the_sink_in_parts_and_presents_once() {
    let mut rig = Rig::new(&[0]);
    // One 32 × 16 tile's codes per part.
    rig.canvas.set_part_tile_bytes(32 * 16 * 6);
    let s = scope(0, 1);
    let tiles: Vec<Vec<u8>> = (0..3).map(|seed| tile_payload(32, 16, seed)).collect();
    let commands = tiles
        .iter()
        .enumerate()
        .map(|(at, payload)| {
            CanvasCommand::Tile(CanvasTile {
                rect: rect(u16::try_from(at * 32).unwrap(), 16, 32, 16),
                codec: CODEC_DELTA_ZSTD1,
                depth: 8,
                payload,
            })
        })
        .collect();
    let terms = request(DEFAULT_CACHE_MIB)
        .capability()
        .and_then(CanvasCapability::terms)
        .unwrap();
    let mut mirror = CanvasMirror::new(0, terms);
    let records = [
        reset(s, WIDTH, HEIGHT),
        frame(s, 1, WIDTH, HEIGHT, commands),
    ];
    for bytes in &records {
        assert_eq!(rig.apply(bytes), CanvasApplied::Presented);
        mirror
            .apply(
                &CanvasRecord::decode(bytes).unwrap(),
                &mut ResidualTiles,
                &mut NoSurfacePixels,
            )
            .unwrap();
    }
    assert_eq!(
        rig.sink.executed,
        [(0, 1, 0, 1), (0, 1, 1, 1), (0, 1, 1, 1), (0, 1, 1, 1)],
        "the frame's three tiles went over in three parts of one fence"
    );
    assert_eq!(rig.canvas.stats().records_presented, 2, "presented once");
    assert_eq!(rig.sink.canvases[&0].bgra(), mirror.pixels().bgra());
}

#[test]
fn the_next_duty_is_known_without_a_record() {
    let mut rig = Rig::new(&[0]);
    assert_eq!(rig.canvas.next_due(rig.now), None);
    let s = scope(0, 1);
    rig.sink.set_surface_visible(false);
    assert_eq!(rig.apply(&reset(s, WIDTH, HEIGHT)), CanvasApplied::Applied);
    assert_eq!(
        rig.canvas.next_due(rig.now),
        Some(rig.now),
        "an ack is owed"
    );
    let _ = rig.control();
    assert_eq!(rig.canvas.next_due(rig.now), None);
    assert_eq!(
        rig.apply(&frame(s, 1, WIDTH, HEIGHT, vec![])),
        CanvasApplied::Applied
    );
    assert_eq!(rig.canvas.next_due(rig.now), Some(rig.now + ACK_INTERVAL));
    rig.canvas.records_lost(0);
    assert_eq!(rig.canvas.next_due(rig.now), Some(rig.now));
    assert_eq!(reset_requests(&rig.control()).len(), 1);
    assert_eq!(
        rig.canvas.next_due(rig.now),
        Some(rig.now + RESET_REQUEST_RETRY)
    );
}
