// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! `delta-zstd1` adapters: the native Pier encoder and the pure-Rust Deck
//! decoder agree, and the decoder refuses malformed frames.
#![cfg(all(feature = "refine-zstd-encode", feature = "refine-zstd-decode"))]

use arcen_media::refine::limits::ZstdHeaderError;
use arcen_media::refine::tile::{TileCodes, encode_residuals};
use arcen_media::refine::zstd_decode::{TileDecodeError, decode_tile};
use arcen_media::refine::zstd_encode::TileEncoder;
use std::time::Instant;

/// Deterministic noise (xorshift) for photo-like content.
fn noise(seed: &mut u32) -> u32 {
    *seed ^= *seed << 13;
    *seed ^= *seed >> 17;
    *seed ^= *seed << 5;
    *seed
}

fn ui_tile(width: u32, height: u32) -> TileCodes {
    let mut samples = Vec::new();
    for plane in 0..3_u32 {
        for y in 0..height {
            for x in 0..width {
                let text = (x % 9 < 2 && y % 12 < 9) as u32;
                samples.push(u16::try_from((900 - text * 800 + plane * 20) % 1024).unwrap());
            }
        }
    }
    TileCodes::new(width, height, samples).unwrap()
}

fn photo_tile(width: u32, height: u32) -> TileCodes {
    let mut seed = 0x1234_5678;
    let mut samples = Vec::new();
    for plane in 0..3_u32 {
        for y in 0..height {
            for x in 0..width {
                let base = (x * 3 + y * 2 + plane * 100) % 1024;
                let grain = noise(&mut seed) % 5;
                samples.push(u16::try_from((base + grain) % 1024).unwrap());
            }
        }
    }
    TileCodes::new(width, height, samples).unwrap()
}

#[test]
fn encoder_and_decoder_round_trip_exactly() {
    let mut encoder = TileEncoder::new().unwrap();
    for codes in [ui_tile(256, 64), photo_tile(256, 64), photo_tile(8, 5)] {
        let payload = encoder.encode(&codes, 10).unwrap();
        assert!(payload.len() < codes.width() as usize * codes.height() as usize * 6);
        assert_eq!(
            decode_tile(&payload, codes.width(), codes.height(), 10).unwrap(),
            codes
        );
    }
}

#[test]
fn malformed_frames_are_refused() {
    let mut encoder = TileEncoder::new().unwrap();
    let codes = photo_tile(64, 32);
    let payload = encoder.encode(&codes, 10).unwrap();

    let mut trailing = payload.clone();
    trailing.push(0);
    assert_eq!(
        decode_tile(&trailing, 64, 32, 10),
        Err(TileDecodeError::TrailingData)
    );

    let mut two_frames = payload.clone();
    two_frames.extend_from_slice(&payload);
    assert_eq!(
        decode_tile(&two_frames, 64, 32, 10),
        Err(TileDecodeError::TrailingData)
    );

    // The declared size must match the rectangle the header announced.
    assert_eq!(
        decode_tile(&payload, 64, 16, 10),
        Err(TileDecodeError::Header(ZstdHeaderError::ContentSize))
    );

    let truncated = &payload[..payload.len() / 2];
    assert!(decode_tile(truncated, 64, 32, 10).is_err());

    let mut corrupt = payload.clone();
    let middle = corrupt.len() / 2;
    corrupt[middle] ^= 0xFF;
    // Either the frame is refused or the codes differ; it never panics and
    // never yields more than the declared size.
    match decode_tile(&corrupt, 64, 32, 10) {
        Ok(decoded) => assert_ne!(decoded, codes),
        Err(_) => {}
    }

    let mut dictionary = payload.clone();
    dictionary[4] |= 0x01;
    assert_eq!(
        decode_tile(&dictionary, 64, 32, 10),
        Err(TileDecodeError::Header(ZstdHeaderError::Dictionary))
    );

    // A highly compressible bomb that declares a small size cannot expand
    // past it: the header check fixes the size before decoding.
    let bomb = zstd::bulk::compress(&vec![0_u8; 4 * 1024 * 1024], 19).unwrap();
    assert!(decode_tile(&bomb, 64, 32, 10).is_err());
}

/// Decoder choice for the Deck (ADR 0016 R8): `cargo test --release -p
/// arcen-media --features refine-zstd-encode,refine-zstd-decode --test
/// refine_zstd -- --ignored --nocapture`.
#[test]
#[ignore = "benchmark"]
fn benchmark_pure_rust_and_native_decoders() {
    let mut encoder = TileEncoder::new().unwrap();
    for (name, codes) in [
        ("ui", ui_tile(256, 64)),
        ("photo+grain", photo_tile(256, 64)),
    ] {
        let payload = encoder.encode(&codes, 10).unwrap();
        let residuals = encode_residuals(&codes, 10).unwrap();
        let pixels = f64::from(codes.width() * codes.height());
        let rounds = 2000_u32;
        let started = Instant::now();
        for _ in 0..rounds {
            std::hint::black_box(decode_tile(&payload, codes.width(), codes.height(), 10).unwrap());
        }
        let ruzstd = pixels * f64::from(rounds) / started.elapsed().as_secs_f64() / 1e6;
        let started = Instant::now();
        for _ in 0..rounds {
            let bytes = zstd::bulk::decompress(&payload, residuals.len()).unwrap();
            std::hint::black_box(
                arcen_media::refine::tile::decode_residuals(
                    &bytes,
                    codes.width(),
                    codes.height(),
                    10,
                )
                .unwrap(),
            );
        }
        let native = pixels * f64::from(rounds) / started.elapsed().as_secs_f64() / 1e6;
        let started = Instant::now();
        for _ in 0..rounds {
            std::hint::black_box(encoder.encode(&codes, 10).unwrap());
        }
        let encode = pixels * f64::from(rounds) / started.elapsed().as_secs_f64() / 1e6;
        println!(
            "{name}: ratio {:.2}, decode ruzstd {ruzstd:.0} Mpx/s, native zstd {native:.0} Mpx/s, encode {encode:.0} Mpx/s",
            residuals.len() as f64 / payload.len() as f64
        );
    }
}

/// The capenc runtime end to end: frames in, framed-v2 records out, a Deck
/// state owner verifies them (level 1), then motion invalidates and the
/// screen verifies again.
#[test]
fn the_runtime_output_verifies_on_a_deck() {
    use arcen_media::capenc_stream::{CapencRecordKind, record_body_length, record_kind};
    use arcen_media::refine::CapencRefineArgs;
    use arcen_media::refine::deck::{DeckRefineSession, RefineAdmission, VideoRecord};
    use arcen_media::refine::runtime::{RefineRuntime, RefineStream};
    use arcen_media::video::{PackedRgb10Layout, Rgb10Signal};
    use arcen_media::{BitDepth, ChromaSubsampling, ColorPrimaries, TransferCharacteristics};
    use std::time::Duration;

    let (width, height) = (320_u32, 180_u32);
    let frame = |seed: u32| {
        let mut out = Vec::new();
        for y in 0..height {
            for x in 0..width {
                let r = (x * 3 + seed) % 1024;
                let g = (y * 5) % 1024;
                let b = (x * y + seed) % 1024;
                out.extend_from_slice(&(r << 20 | g << 10 | b).to_le_bytes());
            }
        }
        out
    };
    let mut runtime = RefineRuntime::start(
        &CapencRefineArgs {
            scope: (1, 1, 1),
            still_ms: 50,
            max_mbps: 200,
        },
        RefineStream {
            depth: BitDepth::Ten,
            chroma: ChromaSubsampling::Yuv444,
            transfer: TransferCharacteristics::Bt709,
            primaries: ColorPrimaries::Bt709,
        },
        arcen_media::refine::source::PackedRgb10Source::new(
            arcen_media::refine::tile::RefineGrid { width, height },
            PackedRgb10Layout::XRGB2101010,
            Rgb10Signal::Direct,
        ),
    )
    .unwrap();
    let started = std::time::Instant::now();
    let mut deck = DeckRefineSession::new();
    let feed = |bytes: &[u8], deck: &mut DeckRefineSession| {
        let mut at = 0;
        while at < bytes.len() {
            let length = record_body_length(bytes[at..at + 4].try_into().unwrap()).unwrap();
            let kind = record_kind(bytes[at + 4], length).unwrap();
            let body = &bytes[at + 5..at + 5 + length];
            let now = started.elapsed();
            match kind {
                CapencRecordKind::RefineVideo => {
                    if let Ok(VideoRecord::Fence { monitor_id, fence }) =
                        deck.on_video_record(body, now)
                    {
                        deck.on_presented(monitor_id, fence, now);
                    }
                }
                CapencRecordKind::RefineLane => {
                    if let RefineAdmission::Tile(ticket) = deck.on_refine_record(body, now) {
                        let codes = decode_tile(
                            &body[48..],
                            ticket.rect.width,
                            ticket.rect.height,
                            ticket.depth,
                        )
                        .unwrap();
                        deck.commit(&ticket, &codes, now).unwrap();
                    }
                }
                _ => {}
            }
            at += 5 + length;
        }
    };
    let run = |runtime: &mut RefineRuntime<arcen_media::refine::source::PackedRgb10Source>,
               deck: &mut DeckRefineSession,
               seed: u32| {
        let pixels = frame(seed);
        runtime.observe_whole(&pixels, width as usize * 4);
        let mut out = Vec::new();
        runtime.before_access_unit(&mut out).unwrap();
        feed(&out, deck);
        for _ in 0..200 {
            std::thread::sleep(Duration::from_millis(10));
            let mut out = Vec::new();
            runtime.pump(&mut out).unwrap();
            feed(&out, deck);
            if deck.status(1).verdict.is_some() {
                return;
            }
        }
        panic!("not verified: {}", deck.status(1).text);
    };
    run(&mut runtime, &mut deck, 0);
    assert_eq!(
        deck.status(1).text,
        "Exact: 100% of picture verified (level 1)"
    );
    // Motion: the next frame differs everywhere; its fence clears the
    // verdict, and the new content verifies again.
    run(&mut runtime, &mut deck, 7);
    assert!(deck.stats().blocks_invalidated > 0);
    assert_eq!(deck.stats().digest_matches, 2);
}

fn records(bytes: &[u8]) -> Vec<(arcen_media::capenc_stream::CapencRecordKind, Vec<u8>)> {
    use arcen_media::capenc_stream::{record_body_length, record_kind};
    let mut out = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let length = record_body_length(bytes[at..at + 4].try_into().unwrap()).unwrap();
        let kind = record_kind(bytes[at + 4], length).unwrap();
        out.push((kind, bytes[at + 5..at + 5 + length].to_vec()));
        at += 5 + length;
    }
    out
}

/// A compositor that keeps its overlay in memory, as a GPU texture would.
struct MemoryCompositor {
    width: u32,
    overlay: Vec<Option<[u16; 3]>>,
    readbacks: Vec<arcen_media::refine::hub::ReadbackRequest>,
}

impl arcen_media::refine::hub::RefineCompositor for MemoryCompositor {
    fn apply_overlay(
        &mut self,
        grid: arcen_media::refine::tile::RefineGrid,
        delta: &arcen_media::refine::deck::OverlayDelta,
    ) {
        if delta.clear_all {
            self.width = grid.width;
            self.overlay = vec![None; grid.width as usize * grid.height as usize];
        }
        for rect in &delta.clear {
            for y in rect.y..rect.y + rect.height {
                for x in rect.x..rect.x + rect.width {
                    self.overlay[(y * self.width + x) as usize] = None;
                }
            }
        }
        for (rect, planar) in &delta.set {
            let pixels = rect.pixels();
            let mut index = 0;
            for y in rect.y..rect.y + rect.height {
                for x in rect.x..rect.x + rect.width {
                    self.overlay[(y * self.width + x) as usize] = Some([
                        planar[index],
                        planar[pixels + index],
                        planar[2 * pixels + index],
                    ]);
                    index += 1;
                }
            }
        }
    }

    fn request_readback(&mut self, request: arcen_media::refine::hub::ReadbackRequest) {
        self.readbacks.push(request);
    }
}

impl MemoryCompositor {
    fn read(&self, request: &arcen_media::refine::hub::ReadbackRequest) -> Vec<Vec<u16>> {
        request
            .blocks
            .iter()
            .map(|rect| {
                let mut planes = vec![Vec::new(), Vec::new(), Vec::new()];
                for y in rect.y..rect.y + rect.height {
                    for x in rect.x..rect.x + rect.width {
                        let texel = self.overlay[(y * self.width + x) as usize].unwrap_or([0; 3]);
                        for (plane, code) in planes.iter_mut().zip(texel) {
                            plane.push(code);
                        }
                    }
                }
                planes.concat()
            })
            .collect()
    }
}

/// The Deck hub end to end: lane records in, the decode worker commits,
/// a compositor paints the overlay, presented fences publish level 1 and
/// readbacks raise the verdict to level 3.
#[test]
fn the_hub_and_a_compositor_reach_level_three() {
    use arcen_media::capenc_stream::CapencRecordKind;
    use arcen_media::refine::CapencRefineArgs;
    use arcen_media::refine::hub::{DeckRefineHub, ReadbackResult};
    use arcen_media::refine::runtime::{RefineRuntime, RefineStream};
    use arcen_media::refine::source::PackedRgb10Source;
    use arcen_media::refine::tile::RefineGrid;
    use arcen_media::video::{PackedRgb10Layout, Rgb10Signal};
    use arcen_media::{BitDepth, ChromaSubsampling, ColorPrimaries, TransferCharacteristics};
    use std::time::Duration;

    let grid = RefineGrid {
        width: 200,
        height: 120,
    };
    let mut runtime = RefineRuntime::start(
        &CapencRefineArgs {
            scope: (1, 1, 1),
            still_ms: 30,
            max_mbps: 200,
        },
        RefineStream {
            depth: BitDepth::Ten,
            chroma: ChromaSubsampling::Yuv444,
            transfer: TransferCharacteristics::Bt709,
            primaries: ColorPrimaries::Bt709,
        },
        PackedRgb10Source::new(grid, PackedRgb10Layout::XRGB2101010, Rgb10Signal::Direct),
    )
    .unwrap();
    let mut pixels = Vec::new();
    for y in 0..grid.height {
        for x in 0..grid.width {
            let (r, g, b) = ((x * 5) % 1024, (y * 7) % 1024, (x ^ y) % 1024);
            pixels.extend_from_slice(&(r << 20 | g << 10 | b).to_le_bytes());
        }
    }
    let hub = DeckRefineHub::new();
    hub.set_accepting(true);
    let mut compositor = MemoryCompositor {
        width: 0,
        overlay: Vec::new(),
        readbacks: Vec::new(),
    };
    let mut frame_fence = None;
    runtime.observe_whole(&pixels, grid.width as usize * 4);
    for _ in 0..400 {
        let mut out = Vec::new();
        runtime.before_access_unit(&mut out).unwrap();
        runtime.pump(&mut out).unwrap();
        for (kind, body) in records(&out) {
            match kind {
                CapencRecordKind::RefineVideo => {
                    if let Some(tag) = hub.on_video_record(&body).unwrap() {
                        frame_fence = Some(tag);
                    }
                }
                CapencRecordKind::RefineLane => {
                    hub.on_refine_record(&body);
                }
                _ => {}
            }
        }
        // The presenter draws the last frame with its fence.
        if let Some((monitor, fence)) = frame_fence {
            hub.compose(&mut compositor, monitor, fence);
            hub.presented(monitor, fence);
            for request in std::mem::take(&mut compositor.readbacks) {
                let blocks = compositor.read(&request);
                assert!(hub.complete_readback(&ReadbackResult { request, blocks }));
            }
        }
        if hub.status(1).text == "Exact: 100% of picture verified (level 3)" {
            assert!(hub.take_control_messages().is_empty());
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("not verified to level 3: {}", hub.status(1).text);
}

/// The macOS Pier's path: an `xf44`-shaped Y′CbCr 4:4:4 source, observed
/// plane by plane, refined by the shared runtime through its record API,
/// and verified by the Deck hub: the Deck's digest over the codes it decoded
/// equals the Pier's over the source codes, and Y′CbCr stops at level 2
/// (the rendered pixel is the matrix of the code, not the code).
#[test]
fn a_ycbcr_444_source_is_verified_digest_exact_at_the_deck() {
    use arcen_media::refine::hub::{DeckRefineHub, ReadbackResult};
    use arcen_media::refine::runtime::{RefineRuntime, RefineStream};
    use arcen_media::refine::tile::RefineGrid;
    use arcen_media::refine::ycbcr::{PlaneRef, YCbCr444Format, YCbCr444Source};
    use arcen_media::refine::{RefineContract, RefineSourceKind};
    use arcen_media::{
        BitDepth, ChromaSubsampling, ColorMatrix, ColorPrimaries, ColorRange,
        TransferCharacteristics,
    };
    use arcen_protocol::refine::RefineDomain;
    use std::time::Duration;

    assert!(
        DeckRefineHub::capability()
            .expect("decoder built")
            .accepts(RefineDomain::YCbCr)
    );
    let grid = RefineGrid {
        width: 200,
        height: 120,
    };
    for (transfer, primaries, matrix) in [
        (
            TransferCharacteristics::Bt709,
            ColorPrimaries::Bt709,
            ColorMatrix::Bt709,
        ),
        (
            TransferCharacteristics::Pq,
            ColorPrimaries::Bt2020,
            ColorMatrix::Bt2020Ncl,
        ),
    ] {
        let source = YCbCr444Source::new(
            grid,
            YCbCr444Format::BIPLANAR_TEN_MSB,
            matrix,
            ColorRange::Full,
        );
        assert_eq!(
            arcen_media::refine::source::RefineSource::kind(&source),
            RefineSourceKind::CpuYCbCr444 {
                matrix,
                range: ColorRange::Full
            }
        );
        let mut runtime = RefineRuntime::start(
            &arcen_media::refine::CapencRefineArgs {
                scope: (1, 1, 1),
                still_ms: 30,
                max_mbps: 200,
            },
            RefineStream {
                depth: BitDepth::Ten,
                chroma: ChromaSubsampling::Yuv444,
                transfer,
                primaries,
            },
            source,
        )
        .unwrap();
        let (luma_stride, chroma_stride) = (grid.width as usize * 2 + 64, grid.width as usize * 4);
        let mut luma = vec![0_u8; luma_stride * grid.height as usize];
        let mut chroma = vec![0_u8; chroma_stride * grid.height as usize];
        for y in 0..grid.height as usize {
            for x in 0..grid.width as usize {
                let (yy, cb, cr) = ((x * 5 + y) % 1024, (y * 7) % 1024, (x ^ y) % 1024);
                let code = |value: usize| (u16::try_from(value).unwrap() << 6).to_le_bytes();
                luma[y * luma_stride + x * 2..][..2].copy_from_slice(&code(yy));
                chroma[y * chroma_stride + x * 4..][..2].copy_from_slice(&code(cb));
                chroma[y * chroma_stride + x * 4 + 2..][..2].copy_from_slice(&code(cr));
            }
        }
        let planes = [
            PlaneRef {
                bytes: &luma,
                stride: luma_stride,
            },
            PlaneRef {
                bytes: &chroma,
                stride: chroma_stride,
            },
        ];
        let damaged = runtime.source_mut().observe_whole_planes(&planes);
        assert_eq!(damaged.len(), grid.block_count() as usize);
        runtime.observe_blocks(damaged);
        let hub = DeckRefineHub::new();
        hub.set_accepting(true);
        let mut compositor = MemoryCompositor {
            width: 0,
            overlay: Vec::new(),
            readbacks: Vec::new(),
        };
        let mut frame_fence = None;
        let mut verified = false;
        for _ in 0..400 {
            for record in runtime.take_video_records() {
                if let Some(tag) = hub.on_video_record(&record).unwrap() {
                    frame_fence = Some(tag);
                }
            }
            for record in runtime.pump_records() {
                hub.on_refine_record(&record);
            }
            if let Some((monitor, fence)) = frame_fence {
                hub.compose(&mut compositor, monitor, fence);
                hub.presented(monitor, fence);
                for request in std::mem::take(&mut compositor.readbacks) {
                    assert_ne!(
                        request.level,
                        arcen_media::refine::deck::VerificationLevel::RenderedRgb,
                        "a Y′CbCr verdict never asks for a rendered readback"
                    );
                    let blocks = compositor.read(&request);
                    assert!(hub.complete_readback(&ReadbackResult { request, blocks }));
                }
            }
            if hub.status(1).text == "Exact: 100% of picture verified (level 2)" {
                verified = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(verified, "not verified to level 2: {}", hub.status(1).text);
        assert!(hub.take_control_messages().is_empty());
        let bytes = hub.contract(1).expect("barrier");
        assert_eq!(bytes.domain, RefineDomain::YCbCr.wire());
        assert!(RefineContract::describes_coded_stream(
            bytes,
            BitDepth::Ten,
            transfer,
            primaries,
            matrix,
            ColorRange::Full
        ));
        assert!(!RefineContract::describes_coded_stream(
            bytes,
            BitDepth::Ten,
            transfer,
            primaries,
            ColorMatrix::Bt601,
            ColorRange::Full
        ));
        assert!(!RefineContract::describes_coded_stream(
            bytes,
            BitDepth::Ten,
            transfer,
            primaries,
            matrix,
            ColorRange::Limited
        ));
    }
}

/// The source seam assumes neither ten bits nor packed RGB10: an
/// eight-bit RGB source drives the same producer, records and digest.
#[test]
fn an_eight_bit_source_drives_the_same_producer_and_verifies() {
    use arcen_media::refine::RefineContract;
    use arcen_media::refine::deck::{DeckRefineSession, RefineAdmission, VideoRecord};
    use arcen_media::refine::producer::RefineProducer;
    use arcen_media::refine::source::{RefineSource, SourceMirror};
    use arcen_media::refine::tile::{RefineGrid, TileError, TileRect};
    use arcen_media::{
        BitDepth, ChromaSubsampling, ColorMatrix, ColorPrimaries, ColorRange,
        TransferCharacteristics,
    };
    use arcen_protocol::refine::RefineDomain;
    use std::time::Duration;

    /// BGRA8, as a Windows DDA or WGC BGRA8 capture delivers it.
    struct Bgra8Source {
        grid: RefineGrid,
        mirror: SourceMirror,
    }
    impl RefineSource for Bgra8Source {
        fn grid(&self) -> RefineGrid {
            self.grid
        }
        fn depth(&self) -> u8 {
            8
        }
        fn domain(&self) -> RefineDomain {
            RefineDomain::Rgb
        }
        fn observe(&mut self, src: &[u8], stride: usize, rect: TileRect) -> Vec<usize> {
            self.mirror.observe_rect(src, stride, rect)
        }
        fn ready(&self) -> bool {
            self.mirror.is_valid()
        }
        fn extract(&self, rect: TileRect) -> Result<TileCodes, TileError> {
            self.grid.check(rect)?;
            let mut planes = vec![Vec::new(), Vec::new(), Vec::new()];
            for y in rect.y..rect.y + rect.height {
                for x in rect.x..rect.x + rect.width {
                    let at = y as usize * self.mirror.stride() + x as usize * 4;
                    let bgra = &self.mirror.bytes()[at..at + 4];
                    planes[0].push(u16::from(bgra[2]));
                    planes[1].push(u16::from(bgra[1]));
                    planes[2].push(u16::from(bgra[0]));
                }
            }
            TileCodes::new(rect.width, rect.height, planes.concat())
        }
    }

    let grid = RefineGrid {
        width: 64,
        height: 48,
    };
    let contract = RefineContract {
        domain: RefineDomain::Rgb,
        depth: BitDepth::Eight,
        transfer: TransferCharacteristics::Srgb,
        primaries: ColorPrimaries::Bt709,
        range: ColorRange::Full,
        chroma: ChromaSubsampling::Yuv444,
        matrix: ColorMatrix::Identity,
    };
    let source = Bgra8Source {
        grid,
        mirror: SourceMirror::new(grid),
    };
    let mut producer = RefineProducer::new(
        (1, 1, 1),
        contract,
        source,
        10,
        1_000_000_000,
        Duration::ZERO,
    );
    let frame: Vec<u8> = (0..grid.width * grid.height * 4)
        .map(|index| u8::try_from(index * 37 % 251).unwrap())
        .collect();
    let ms = Duration::from_millis(1);
    producer.observe_whole(&frame, grid.width as usize * 4, Duration::ZERO);
    let mut deck = DeckRefineSession::new();
    let mut encoder = TileEncoder::new().unwrap();
    for record in producer.before_access_unit() {
        if let Ok(VideoRecord::Fence { monitor_id, fence }) = deck.on_video_record(&record, ms) {
            deck.on_presented(monitor_id, fence, ms);
        }
    }
    // After the motion window of the first picture has passed.
    let mut tiles = 0;
    for work in producer.take_work(ms * 300) {
        let payload = encoder.encode(&work.codes, 8).unwrap();
        producer.finish(&work.job, Some(&payload));
    }
    for record in producer.drain_ready(ms * 310) {
        tiles += 1;
        let RefineAdmission::Tile(ticket) = deck.on_refine_record(&record, ms * 310) else {
            panic!("tile refused");
        };
        assert_eq!(ticket.depth, 8);
        let codes = decode_tile(&record[48..], ticket.rect.width, ticket.rect.height, 8).unwrap();
        deck.commit(&ticket, &codes, ms * 310).unwrap();
    }
    assert!(tiles > 0);
    let digest = producer.digest(ms * 900).expect("settled");
    deck.on_refine_record(&digest, ms * 900);
    assert_eq!(
        deck.status(1).text,
        "Exact: 100% of picture verified (level 1)"
    );
}
