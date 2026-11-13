// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! ADR 0016 acceptance tests for the portable refinement core: a Pier
//! scheduler and a Deck state owner exchanging the real wire records, with
//! the codec-free residual transform standing in for Zstandard (the codec
//! adapters have their own tests behind their features).

use arcen_media::refine::RefineContract;
use arcen_media::refine::deck::{
    DeckRefineSession, RefineAdmission, RefineDisabled, RefineDrop, TileTicket, VerificationLevel,
    VideoRecord,
};
use arcen_media::refine::digest::tile_block_fingerprints;
use arcen_media::refine::limits::MAX_PENDING_JOBS;
use arcen_media::refine::pier::{PierMonitorRefine, TileJob};
use arcen_media::refine::tile::{
    RefineGrid, TileCodes, decode_residuals, encode_residuals, extract_packed_rgb10,
};
use arcen_media::video::{PackedRgb10Layout, Rgb10Signal};
use arcen_media::{
    BitDepth, ChromaSubsampling, ColorPrimaries, ColorRange, TransferCharacteristics,
};
use arcen_protocol::refine::{
    BarrierReason, CODEC_DELTA_ZSTD1, RefineControlMsg, RefineDomain, RefineTileHeader,
};
use std::time::Duration;

const MS: Duration = Duration::from_millis(1);
const GRID: RefineGrid = RefineGrid {
    width: 200,
    height: 72,
};

fn contract() -> RefineContract {
    RefineContract {
        domain: RefineDomain::Rgb,
        depth: BitDepth::Ten,
        transfer: TransferCharacteristics::Bt709,
        primaries: ColorPrimaries::Bt709,
        range: ColorRange::Full,
        chroma: ChromaSubsampling::Yuv444,
    }
}

/// A ten-bit test pattern: ramps and a checker, packed XRGB2101010.
fn frame(seed: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity((GRID.width * GRID.height * 4) as usize);
    for y in 0..GRID.height {
        for x in 0..GRID.width {
            let r = (x * 5 + seed) % 1024;
            let g = (y * 13) % 1024;
            let b = if (x / 8 + y / 8) % 2 == 0 { 1023 } else { 64 };
            out.extend_from_slice(&(r << 20 | g << 10 | b).to_le_bytes());
        }
    }
    out
}

struct Link {
    pier: PierMonitorRefine,
    deck: DeckRefineSession,
    now: Duration,
    source: Vec<u8>,
}

impl Link {
    fn new() -> Self {
        Self {
            pier: PierMonitorRefine::new(1, 1, 1, contract(), GRID, 300),
            deck: DeckRefineSession::new(),
            now: Duration::ZERO,
            source: frame(0),
        }
    }

    /// One access unit: records before it go to the Deck's video lane
    /// reader; the frame is presented at once.
    fn access_unit(&mut self) -> u64 {
        let mut fence = 0;
        for record in self.pier.before_access_unit() {
            if let Ok(VideoRecord::Fence { fence: value, .. }) =
                self.deck.on_video_record(&record, self.now)
            {
                fence = value;
            }
        }
        self.deck.on_presented(1, fence, self.now);
        fence
    }

    fn advance(&mut self, by: Duration) {
        self.now += by;
    }

    fn codes(&self, job: &TileJob) -> TileCodes {
        extract_packed_rgb10(
            &self.source,
            GRID.width as usize * 4,
            GRID,
            job.rect,
            PackedRgb10Layout::XRGB2101010,
            Rgb10Signal::Direct,
        )
        .unwrap()
    }

    /// Extracts, "compresses" (residual transform only) and frames a job.
    fn tile_record(&mut self, job: &TileJob) -> (Vec<u8>, Vec<u8>) {
        let codes = self.codes(job);
        self.pier
            .tile_fingerprints(job, &tile_block_fingerprints(GRID, job.rect, &codes));
        let payload = encode_residuals(&codes, 10).unwrap();
        let header = RefineTileHeader {
            scope: job.scope,
            valid_from_fence: job.valid_from_fence,
            domain: RefineDomain::Rgb,
            depth: 10,
            codec: CODEC_DELTA_ZSTD1,
            x: u16::try_from(job.rect.x).unwrap(),
            y: u16::try_from(job.rect.y).unwrap(),
            width: u16::try_from(job.rect.width).unwrap(),
            height: u16::try_from(job.rect.height).unwrap(),
            payload_len: u32::try_from(payload.len()).unwrap(),
        };
        (header.encode_with(&payload), payload)
    }

    fn admit(&mut self, record: &[u8]) -> TileTicket {
        match self.deck.on_refine_record(record, self.now) {
            RefineAdmission::Tile(ticket) => ticket,
            other => panic!("not admitted: {other:?}"),
        }
    }

    fn decode(ticket: &TileTicket, payload: &[u8]) -> TileCodes {
        decode_residuals(payload, ticket.rect.width, ticket.rect.height, ticket.depth).unwrap()
    }

    /// Runs refinement of a still screen to completion and sends the digest.
    fn refine_everything(&mut self) {
        self.advance(MS * 400);
        let jobs = self.pier.due_tiles(self.now, usize::MAX);
        assert!(!jobs.is_empty());
        for job in jobs {
            let (record, payload) = self.tile_record(&job);
            let ticket = self.admit(&record);
            let codes = Self::decode(&ticket, &payload);
            self.deck.commit(&ticket, &codes, self.now).unwrap();
        }
        self.advance(MS * 600);
        self.send_digest();
    }

    fn send_digest(&mut self) {
        let covered = self.pier.digest_candidates(self.now).expect("settled");
        let recomputed: Vec<(u32, u128)> = covered
            .iter()
            .map(|&index| {
                let rect = GRID.block_rect(index).unwrap();
                let job = TileJob {
                    scope: self.pier.scope(),
                    valid_from_fence: 0,
                    rect,
                };
                tile_block_fingerprints(GRID, rect, &self.codes(&job))[0]
            })
            .collect();
        let digest = self.pier.digest(self.now, &recomputed).unwrap();
        assert_eq!(
            self.deck.on_refine_record(&digest.encode(), self.now),
            RefineAdmission::Digest
        );
    }
}

#[test]
fn a_still_test_pattern_reaches_level_one_then_two_and_three() {
    let mut link = Link::new();
    link.pier
        .on_capture(0..GRID.block_count() as usize, link.now);
    link.access_unit();
    link.refine_everything();
    let status = link.deck.status(1);
    let verdict = status.verdict.clone().expect("verified");
    assert_eq!(verdict.level, VerificationLevel::SourceCodes);
    assert_eq!(verdict.coverage_permille(), 1000);
    assert_eq!(
        status.text,
        "Source codes verified (pre-display transforms); 100.0% coverage; level 1"
    );
    assert_eq!(link.deck.stats().digest_matches, 1);

    // The presenter uploads the overlay; reading the GPU texture back and
    // then the rendered composite raises the level.
    let delta = link.deck.take_overlay_delta(1, link.pier.fence());
    assert!(delta.clear_all);
    assert_eq!(delta.set.len(), GRID.block_count() as usize);
    let readback: Vec<(u32, u128)> = delta
        .set
        .iter()
        .map(|(rect, planar)| {
            let codes = TileCodes::new(rect.width, rect.height, planar.clone()).unwrap();
            tile_block_fingerprints(GRID, *rect, &codes)[0]
        })
        .collect();
    let fence = verdict.fence;
    assert!(
        !link
            .deck
            .on_readback(1, VerificationLevel::RenderedRgb, fence, &readback)
    );
    assert!(
        link.deck
            .on_readback(1, VerificationLevel::GpuOverlay, fence, &readback)
    );
    assert!(
        link.deck
            .on_readback(1, VerificationLevel::RenderedRgb, fence, &readback)
    );
    assert!(link.deck.status(1).text.ends_with("level 3"));
    let mut wrong = readback.clone();
    wrong[0].1 ^= 1;
    assert!(
        !link
            .deck
            .on_readback(1, VerificationLevel::GpuOverlay, fence, &wrong)
    );
}

#[test]
fn a_one_code_difference_is_a_defect_and_asks_for_a_reset() {
    let mut link = Link::new();
    link.pier
        .on_capture(0..GRID.block_count() as usize, link.now);
    link.access_unit();
    link.advance(MS * 400);
    for job in link.pier.due_tiles(link.now, usize::MAX) {
        let (record, payload) = link.tile_record(&job);
        let ticket = link.admit(&record);
        let mut planar: Vec<u16> = (0..3)
            .flat_map(|plane| Link::decode(&ticket, &payload).plane(plane).to_vec())
            .collect();
        if job.rect.x == 0 && job.rect.y == 0 {
            planar[17] ^= 1;
        }
        let codes = TileCodes::new(job.rect.width, job.rect.height, planar).unwrap();
        link.deck.commit(&ticket, &codes, link.now).unwrap();
    }
    link.advance(MS * 600);
    link.send_digest();
    assert!(link.deck.status(1).verdict.is_none());
    assert_eq!(link.deck.stats().digest_mismatches, 1);
    assert!(matches!(
        link.deck.take_control_messages().as_slice(),
        [RefineControlMsg::RefineReset { reason, .. }] if reason == "digest_mismatch"
    ));
}

#[test]
fn reordered_delivery_waits_for_presentation_and_survives_its_own_fence() {
    let mut link = Link::new();
    link.pier
        .on_capture(0..GRID.block_count() as usize, link.now);
    link.access_unit();
    link.advance(MS * 400);
    // The Pier emits the next access unit (fence 2, invalidating block 0),
    // and captures the tile at fence 2 — but the refine lane overtakes the
    // video lane: the Deck sees the tile first.
    link.pier.on_capture([0], link.now);
    let video = link.pier.before_access_unit();
    // Block 0 changed twice within a second: busy, so it waits 600 ms.
    link.advance(MS * 700);
    let job = link
        .pier
        .due_tiles(link.now, usize::MAX)
        .into_iter()
        .find(|job| job.rect.x == 0 && job.rect.y == 0)
        .unwrap();
    assert_eq!(job.valid_from_fence, 2);
    let (record, payload) = link.tile_record(&job);
    let ticket = link.admit(&record);
    link.deck
        .commit(&ticket, &Link::decode(&ticket, &payload), link.now)
        .unwrap();
    // Not visible: the frame it belongs to has not been presented.
    let (_, map) = link.deck.block_map(1).unwrap();
    assert_eq!(map[0], None);
    // Fence 2 arrives late; it invalidates block 0 for content *older* than
    // the tile, so the tile stays.
    for record in &video {
        link.deck.on_video_record(record, link.now).unwrap();
    }
    link.deck.on_presented(1, 2, link.now);
    let (_, map) = link.deck.block_map(1).unwrap();
    assert_eq!(map[0], Some(false));
}

#[test]
fn an_invalidation_during_decompression_wins_at_commit() {
    let mut link = Link::new();
    link.pier
        .on_capture(0..GRID.block_count() as usize, link.now);
    link.access_unit();
    link.advance(MS * 400);
    let job = link.pier.due_tiles(link.now, 1)[0];
    let (record, payload) = link.tile_record(&job);
    let ticket = link.admit(&record);
    // While the worker decompresses, the content changes.
    let changed: Vec<usize> = GRID
        .blocks_of(job.rect)
        .into_iter()
        .map(|index| index as usize)
        .collect();
    link.pier.on_capture(changed, link.now);
    link.access_unit();
    assert_eq!(
        link.deck
            .commit(&ticket, &Link::decode(&ticket, &payload), link.now),
        Err(RefineDrop::Stale)
    );
    assert_eq!(link.deck.stats().blocks_committed, 0);
    let (_, map) = link.deck.block_map(1).unwrap();
    assert!(map.iter().all(Option::is_none));
}

#[test]
fn an_old_tile_after_a_reset_and_a_recovery_keyframe_never_apply() {
    let mut link = Link::new();
    link.pier
        .on_capture(0..GRID.block_count() as usize, link.now);
    link.access_unit();
    link.refine_everything();
    assert!(link.deck.status(1).verdict.is_some());

    // A tile of generation 1 is in flight when the Deck's decoder recovers.
    link.pier.on_capture([0], link.now);
    link.access_unit();
    link.advance(MS * 400);
    let old_job = link.pier.due_tiles(link.now, 1)[0];
    let (old_record, old_payload) = link.tile_record(&old_job);
    let old_ticket = link.admit(&old_record);

    link.deck.decoder_reset(1, "decoder_recovery");
    assert!(link.deck.status(1).verdict.is_none());
    assert!(matches!(
        link.deck.take_control_messages().as_slice(),
        [RefineControlMsg::RefineReset { reason, .. }] if reason == "decoder_recovery"
    ));
    // The Pier answers with a new generation behind a barrier, before the
    // recovery keyframe.
    link.pier.reset(BarrierReason::Recovery);
    let video = link.pier.before_access_unit();
    assert_eq!(video.len(), 2, "barrier, then fence");
    for record in &video {
        link.deck.on_video_record(record, link.now).unwrap();
    }
    link.deck.on_presented(1, link.pier.fence(), link.now);
    // The old generation's tile is refused at admission and at commit.
    assert_eq!(
        link.deck.on_refine_record(&old_record, link.now),
        RefineAdmission::Dropped(RefineDrop::StaleGeneration)
    );
    assert_eq!(
        link.deck.commit(
            &old_ticket,
            &Link::decode(&old_ticket, &old_payload),
            link.now
        ),
        Err(RefineDrop::StaleGeneration)
    );
    // Nothing from before the reset is visible; the new generation refines
    // from scratch and verifies again.
    let delta = link.deck.take_overlay_delta(1, link.pier.fence());
    assert!(delta.clear_all && delta.set.is_empty());
    link.refine_everything();
    assert_eq!(
        link.deck
            .status(1)
            .verdict
            .map(|verdict| verdict.coverage_permille()),
        Some(1000)
    );
}

#[test]
fn a_tile_flood_is_bounded_and_malformed_records_disable_refinement_visibly() {
    let mut link = Link::new();
    link.pier
        .on_capture(0..GRID.block_count() as usize, link.now);
    link.access_unit();
    link.advance(MS * 400);
    let job = link.pier.due_tiles(link.now, 1)[0];
    let (record, _) = link.tile_record(&job);
    let mut admitted = 0;
    let mut backpressure = 0;
    for _ in 0..(MAX_PENDING_JOBS + 50) {
        match link.deck.on_refine_record(&record, link.now) {
            RefineAdmission::Tile(_) => admitted += 1,
            RefineAdmission::Dropped(RefineDrop::Backpressure) => backpressure += 1,
            other => panic!("{other:?}"),
        }
    }
    assert!(admitted <= MAX_PENDING_JOBS);
    assert!(backpressure >= 50);
    // Pending jobs expire; malformed records then disable the session.
    link.advance(MS * 2500);
    let mut broken = record.clone();
    broken[1] = 9;
    for _ in 0..8 {
        let _ = link.deck.on_refine_record(&broken, link.now);
    }
    assert_eq!(link.deck.disabled(), Some(RefineDisabled::MalformedRecords));
    assert_eq!(
        link.deck.status(1).text,
        "Refinement disabled (malformed records)"
    );
    assert_eq!(
        link.deck.on_refine_record(&record, link.now),
        RefineAdmission::Dropped(RefineDrop::Disabled)
    );
}

#[test]
fn motion_pauses_refinement_and_clears_the_verdict_on_receipt() {
    let mut link = Link::new();
    link.pier
        .on_capture(0..GRID.block_count() as usize, link.now);
    link.access_unit();
    link.refine_everything();
    assert!(link.deck.status(1).verdict.is_some());
    // A window drag: well over 5 % of the blocks change every frame.
    let mut fences = Vec::new();
    for step in 0..10_u32 {
        let _ = step;
        link.pier.on_capture(0..20, link.now);
        fences.push(link.access_unit());
        assert!(link.pier.due_tiles(link.now, usize::MAX).is_empty());
        assert!(link.deck.status(1).verdict.is_none());
        link.advance(MS * 16);
    }
    // The invalidated blocks are cleared from the overlay at once.
    let (_, map) = link.deck.block_map(1).unwrap();
    assert!(map[..20].iter().all(Option::is_none));
    assert!(map[20..].iter().all(|block| *block == Some(false)));
    let delta = link.deck.take_overlay_delta(1, *fences.last().unwrap());
    assert!(delta.clear.len() <= 20);
    // Stillness again: only the moved blocks are refined — after a longer
    // wait, since they were busy — and the whole screen verifies again.
    link.advance(MS * 4500);
    link.refine_everything();
    assert_eq!(
        link.deck
            .status(1)
            .verdict
            .map(|verdict| verdict.coverage_permille()),
        Some(1000)
    );
}

#[test]
fn an_eight_bit_fallback_disables_the_monitor_and_re_enable_needs_a_new_generation() {
    let mut link = Link::new();
    link.pier
        .on_capture(0..GRID.block_count() as usize, link.now);
    link.access_unit();
    link.refine_everything();
    link.deck.set_monitor_enabled(1, false);
    assert_eq!(link.deck.status(1).text, "Refinement off for this display");
    assert_eq!(
        link.deck.take_control_messages(),
        vec![RefineControlMsg::RefineControl {
            monitor_id: 1,
            enabled: false
        }]
    );
    link.deck.set_monitor_enabled(1, true);
    assert_eq!(link.deck.status(1).text, "Refinement off");
    link.pier.reset(BarrierReason::Reenable);
    link.access_unit();
    assert!(link.deck.status(1).text.starts_with("Refining"));
}

const HD: RefineGrid = RefineGrid {
    width: 1920,
    height: 1080,
};

fn hd_frame() -> Vec<u8> {
    let mut out = Vec::with_capacity((HD.width * HD.height * 4) as usize);
    for y in 0..HD.height {
        for x in 0..HD.width {
            let r = (x + y) % 1024;
            let g = (x * 3) % 1024;
            let b = (y * 7) % 1024;
            out.extend_from_slice(&(r << 20 | g << 10 | b).to_le_bytes());
        }
    }
    out
}

/// Review item 1: a still screen with a 2 Hz caret and 1 Hz keepalive access
/// units costs the capture thread (observe, fence, pump, digest) well under
/// 1 ms per pump, and never re-fingerprints the screen: a keepalive fence is
/// not new content, and the caret is not covered.
#[test]
fn an_idle_screen_with_a_blinking_caret_costs_the_capture_thread_under_a_millisecond() {
    use arcen_media::refine::producer::RefineProducer;
    use arcen_media::refine::tile::TileRect;
    use std::time::Instant;

    let stride = HD.width as usize * 4;
    let mut frame = hd_frame();
    let mut producer = RefineProducer::new(
        (1, 1, 1),
        contract(),
        HD,
        PackedRgb10Layout::XRGB2101010,
        Rgb10Signal::Direct,
        300,
        1_000_000_000,
        Duration::ZERO,
    );
    // Setup (not measured): the whole screen is refined and digested.
    producer.observe_whole(&frame, stride, Duration::ZERO);
    producer.before_access_unit();
    let mut now = MS * 400;
    loop {
        let work = producer.take_work(now);
        if work.is_empty() {
            break;
        }
        for item in work {
            producer.finish(&item.job, Some(&[1, 2, 3]));
        }
        assert!(!producer.drain_ready(now).is_empty());
    }
    now = MS * 1_000;
    assert!(
        producer.digest(now).is_some(),
        "the settled screen is digested"
    );

    let caret = TileRect {
        x: 200,
        y: 300,
        width: 2,
        height: 16,
    };
    let mut digests = 0;
    let mut tiles = 0;
    let mut measured = Duration::ZERO;
    let pumps = 300_u32;
    let mut last_au = now;
    for step in 1..=pumps {
        now = MS * 1_000 + MS * (step * 33);
        let blink = step % 15 == 0; // every ~500 ms
        if blink {
            for row in 0..caret.height as usize {
                let at = (caret.y as usize + row) * stride + caret.x as usize * 4;
                for byte in &mut frame[at..at + caret.width as usize * 4] {
                    *byte ^= 0xFF;
                }
            }
        }
        let started = Instant::now();
        if blink {
            let at = caret.y as usize * stride + caret.x as usize * 4;
            producer.observe_rect(&frame[at..], stride, caret, now);
        }
        if blink || now.saturating_sub(last_au) >= Duration::from_secs(1) {
            producer.before_access_unit();
            last_au = now;
        }
        let work = producer.take_work(now);
        tiles += work.len();
        for item in work {
            producer.finish(&item.job, Some(&[1, 2, 3]));
        }
        producer.drain_ready(now);
        if producer.digest(now).is_some() {
            digests += 1;
        }
        measured += started.elapsed();
    }
    let per_pump = measured / pumps;
    eprintln!("idle: {per_pump:?} per pump, {digests} digests, {tiles} tiles");
    assert!(
        per_pump < Duration::from_millis(1),
        "{per_pump:?} per pump over {pumps} pumps"
    );
    // The caret block leaves the coverage once (one digest) and, busy,
    // is never refined again while it blinks; keepalives add nothing.
    assert!(digests <= 2, "{digests} digests while idle");
    assert!(tiles <= 2, "{tiles} tiles while idle");
}

/// Review item 2: the motion gate counts distinct blocks. A 1 % region (a
/// small video) changing at 60 fps does not stop the rest of the screen
/// from refining.
#[test]
fn a_one_percent_region_at_sixty_fps_does_not_close_the_motion_gate() {
    let mut pier = PierMonitorRefine::new(1, 1, 1, contract(), HD, 300);
    pier.on_capture(0..HD.block_count() as usize, Duration::ZERO);
    pier.before_access_unit();
    // 9×9 blocks = 81 of 8160 (≈1 %) at the top left.
    let cols = HD.cols() as usize;
    let video: Vec<usize> = (0..9)
        .flat_map(|row| (0..9).map(move |col| row * cols + col))
        .collect();
    let mut refined_elsewhere = 0;
    for frame in 1..=120_u32 {
        // The first full-screen change is itself motion; the video
        // starts once it has left the window.
        let now = MS * 300 + Duration::from_micros(u64::from(frame) * 16_667);
        pier.on_capture(video.iter().copied(), now);
        pier.before_access_unit();
        assert!(!pier.in_motion(now), "gate closed at frame {frame}");
        for job in pier.due_tiles(now, usize::MAX) {
            let blocks = HD.blocks_of(job.rect);
            assert!(
                blocks
                    .iter()
                    .all(|index| !video.contains(&(*index as usize)))
            );
            refined_elsewhere += blocks.len();
        }
    }
    assert_eq!(refined_elsewhere, HD.block_count() as usize - video.len());
    // A real drag (≈7 % distinct blocks in the window) still closes it.
    let drag: Vec<usize> = (0..600).collect();
    pier.on_capture(drag, Duration::from_secs(3));
    assert!(pier.in_motion(Duration::from_secs(3)));
}

/// Review item 3: a barrier on the ordered video lane is authoritative. A
/// restarted producer that reuses its whole identity (scope and generation
/// 1) still clears a verified overlay; production producers start from a
/// per-process generation anyway.
#[test]
fn a_restarted_producer_reusing_its_identity_cannot_leave_a_stale_overlay() {
    let mut link = Link::new();
    link.pier
        .on_capture(0..GRID.block_count() as usize, link.now);
    link.access_unit();
    link.refine_everything();
    assert!(link.deck.status(1).verdict.is_some());
    let restarted = PierMonitorRefine::new(1, 1, 1, contract(), GRID, 300);
    assert_eq!(restarted.scope(), link.pier.scope(), "identical identity");
    link.pier = restarted;
    link.pier
        .on_capture(0..GRID.block_count() as usize, link.now);
    link.access_unit();
    assert!(link.deck.status(1).verdict.is_none());
    let (_, map) = link.deck.block_map(1).unwrap();
    assert!(map.iter().all(Option::is_none), "nothing stale survives");
    use arcen_media::refine::producer::process_unique_generation;
    assert_ne!(process_unique_generation(), process_unique_generation());
}

/// Review item 3: a fence for any other identity clears the overlay and
/// asks for a new generation.
#[test]
fn a_fence_for_another_identity_clears_the_overlay() {
    let mut link = Link::new();
    link.pier
        .on_capture(0..GRID.block_count() as usize, link.now);
    link.access_unit();
    link.refine_everything();
    assert!(link.deck.status(1).verdict.is_some());
    let mut stranger = PierMonitorRefine::new(1, 1, 99, contract(), GRID, 300);
    let records = stranger.before_access_unit();
    // Only the fence (the barrier is withheld).
    assert!(link.deck.on_video_record(&records[1], link.now).is_err());
    assert!(link.deck.status(1).verdict.is_none());
    assert!(link.deck.block_map(1).is_none(), "overlay cleared");
    assert!(
        link.deck
            .take_control_messages()
            .iter()
            .any(|message| matches!(
                message,
                RefineControlMsg::RefineReset { reason, .. } if reason == "fence_mismatch"
            ))
    );
}

/// Security hardening: a generation id is never reused for another source
/// size, and a peer cannot make the Deck hold state for unbounded monitors
/// or blocks.
#[test]
fn generation_reuse_with_another_size_and_monitor_floods_are_refused() {
    use arcen_media::refine::deck::{MAX_REFINE_MONITORS, MAX_SESSION_REFINE_BLOCKS};
    let mut link = Link::new();
    link.pier
        .on_capture(0..GRID.block_count() as usize, link.now);
    link.access_unit();
    let resized = PierMonitorRefine::new(
        1,
        1,
        1,
        contract(),
        RefineGrid {
            width: 400,
            height: 72,
        },
        300,
    );
    let mut resized = resized;
    let records = resized.before_access_unit();
    assert!(matches!(
        link.deck.on_video_record(&records[0], link.now),
        Err(RefineDrop::Malformed(_))
    ));
    assert!(link.deck.block_map(1).is_none());

    let mut deck = DeckRefineSession::new();
    let small = RefineGrid {
        width: 64,
        height: 64,
    };
    for monitor in 0..MAX_REFINE_MONITORS as u16 {
        let mut pier = PierMonitorRefine::new(monitor, 1, 1, contract(), small, 300);
        assert!(
            deck.on_video_record(&pier.before_access_unit()[0], MS)
                .is_ok()
        );
    }
    let mut extra = PierMonitorRefine::new(999, 1, 1, contract(), small, 300);
    assert!(
        deck.on_video_record(&extra.before_access_unit()[0], MS)
            .is_err()
    );

    let mut deck = DeckRefineSession::new();
    let mut first = PierMonitorRefine::new(1, 1, 1, contract(), HD, 300);
    assert!(
        deck.on_video_record(&first.before_access_unit()[0], MS)
            .is_ok()
    );
    let huge = RefineGrid {
        width: 16_384,
        height: 16_384,
    };
    assert_eq!(u64::from(huge.block_count()), MAX_SESSION_REFINE_BLOCKS);
    let mut second = PierMonitorRefine::new(2, 1, 1, contract(), huge, 300);
    assert!(
        deck.on_video_record(&second.before_access_unit()[0], MS)
            .is_err()
    );
    assert!(deck.block_map(2).is_none());
    assert!(
        deck.block_map(1).is_some(),
        "the first monitor is untouched"
    );
}
