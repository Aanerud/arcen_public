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
        width,
        height,
        PackedRgb10Layout::XRGB2101010,
        Rgb10Signal::Direct,
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
    let run = |runtime: &mut RefineRuntime, deck: &mut DeckRefineSession, seed: u32| {
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
        "Source codes verified (pre-display transforms); 100.0% coverage; level 1"
    );
    // Motion: the next frame differs everywhere; its fence clears the
    // verdict, and the new content verifies again.
    run(&mut runtime, &mut deck, 7);
    assert!(deck.stats().blocks_invalidated > 0);
    assert_eq!(deck.stats().digest_matches, 2);
}
