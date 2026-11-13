// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Verifies a capenc framed-v2 stream with lossless refinement (ADR 0016)
//! the way a Deck's state owner would, without a display: every access
//! unit received counts as presented (so verification is level 1 only).
//!
//! ```sh
//! capenc … framed-v2 refine=on | refine_verify [seconds]
//! ```
//!
//! Prints one status line a second and a final summary; exits 0 when the
//! stream reached "source codes verified".

use arcen_media::capenc_stream::{
    CapencRecordKind, RECORD_LENGTH_BYTES, record_body_length, record_kind,
};
use arcen_media::refine::deck::{DeckRefineSession, RefineAdmission, VideoRecord};
use arcen_media::refine::zstd_decode::decode_tile;
use arcen_protocol::refine::{REFINE_TILE_HEADER_BYTES, RefineRecordError};
use std::io::Read;
use std::time::{Duration, Instant};

fn main() {
    let limit = std::env::args()
        .nth(1)
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_secs);
    // `REFINE_VERIFY_FRAMING=v1` reads a framed-v1 stream (refinement off)
    // to measure the same cadence without refinement.
    let framed_v1 = std::env::var("REFINE_VERIFY_FRAMING").is_ok_and(|value| value == "v1");
    let started = Instant::now();
    let mut stdin = std::io::stdin().lock();
    let mut deck = DeckRefineSession::new();
    let mut pending_fence: Option<(u16, u64)> = None;
    let mut monitor = 1_u16;
    let mut last_au: Option<Instant> = None;
    let mut window_gap_max = Duration::ZERO;
    let mut window_aus = 0_u64;
    let mut total_aus = 0_u64;
    let mut decode_errors = 0_u64;
    let mut verified_at: Option<Duration> = None;
    let mut last_report = Instant::now();
    loop {
        let mut length = [0_u8; RECORD_LENGTH_BYTES];
        if stdin.read_exact(&mut length).is_err() {
            break;
        }
        if framed_v1 {
            // framed-v1: `u32 length || access unit` (refinement off).
            let mut body = vec![0_u8; u32::from_be_bytes(length) as usize];
            if stdin.read_exact(&mut body).is_err() {
                break;
            }
            let arrived = Instant::now();
            if let Some(previous) = last_au {
                window_gap_max = window_gap_max.max(arrived - previous);
            }
            last_au = Some(arrived);
            window_aus += 1;
            total_aus += 1;
            if last_report.elapsed() >= Duration::from_secs(1) {
                println!(
                    "t={:.1}s aus={window_aus} max_au_gap_ms={} status=\"refinement off (framed-v1)\"",
                    started.elapsed().as_secs_f64(),
                    window_gap_max.as_millis()
                );
                window_aus = 0;
                window_gap_max = Duration::ZERO;
                last_report = Instant::now();
            }
            if limit.is_some_and(|limit| started.elapsed() >= limit) {
                break;
            }
            continue;
        }
        let Ok(body_length) = record_body_length(length) else {
            eprintln!("bad record length");
            std::process::exit(2);
        };
        let mut kind = [0_u8; 1];
        if stdin.read_exact(&mut kind).is_err() {
            break;
        }
        let Ok(kind) = record_kind(kind[0], body_length) else {
            eprintln!("bad record kind {}", kind[0]);
            std::process::exit(2);
        };
        let mut body = vec![0_u8; body_length];
        if stdin.read_exact(&mut body).is_err() {
            break;
        }
        let now = started.elapsed();
        match kind {
            CapencRecordKind::AccessUnit => {
                let arrived = Instant::now();
                if let Some(previous) = last_au {
                    window_gap_max = window_gap_max.max(arrived - previous);
                }
                last_au = Some(arrived);
                window_aus += 1;
                total_aus += 1;
                if let Some((monitor_id, fence)) = pending_fence.take() {
                    deck.on_presented(monitor_id, fence, now);
                }
            }
            CapencRecordKind::Cursor | CapencRecordKind::CursorPosition => {}
            CapencRecordKind::RefineVideo => match deck.on_video_record(&body, now) {
                Ok(VideoRecord::Fence { monitor_id, fence }) => {
                    monitor = monitor_id;
                    pending_fence = Some((monitor_id, fence));
                }
                Ok(_) => {}
                Err(drop) => eprintln!("video record dropped: {drop:?}"),
            },
            CapencRecordKind::RefineLane => match deck.on_refine_record(&body, now) {
                RefineAdmission::Tile(ticket) => {
                    let payload = &body[REFINE_TILE_HEADER_BYTES..];
                    match decode_tile(payload, ticket.rect.width, ticket.rect.height, ticket.depth)
                    {
                        Ok(codes) => {
                            let _ = deck.commit(&ticket, &codes, started.elapsed());
                        }
                        Err(error) => {
                            decode_errors += 1;
                            eprintln!("tile decode failed: {error:?}");
                            deck.abandon(&ticket, RefineRecordError::PayloadLength, now);
                        }
                    }
                }
                RefineAdmission::Digest => {}
                RefineAdmission::Dropped(drop) => eprintln!("refine record dropped: {drop:?}"),
            },
        }
        let status = deck.status(monitor);
        if verified_at.is_none() && status.verdict.is_some() {
            verified_at = Some(now);
        }
        if last_report.elapsed() >= Duration::from_secs(1) {
            let stats = deck.stats();
            println!(
                "t={:.1}s aus={window_aus} max_au_gap_ms={} status=\"{}\" tiles={} blocks={} stale_at_commit={} invalidated={} payload_kb={} matches={} mismatches={} decode_errors={decode_errors}",
                now.as_secs_f64(),
                window_gap_max.as_millis(),
                status.text,
                stats.tiles_admitted,
                stats.blocks_committed,
                stats.blocks_stale_at_commit,
                stats.blocks_invalidated,
                stats.payload_bytes / 1024,
                stats.digest_matches,
                stats.digest_mismatches,
            );
            for message in deck.take_control_messages() {
                println!("control: {message:?}");
            }
            window_aus = 0;
            window_gap_max = Duration::ZERO;
            last_report = Instant::now();
        }
        if limit.is_some_and(|limit| started.elapsed() >= limit) {
            break;
        }
    }
    let status = deck.status(monitor);
    println!(
        "final: aus={total_aus} status=\"{}\" first_verified_s={} stats={:?}",
        status.text,
        verified_at.map_or_else(|| "-".to_string(), |at| format!("{:.2}", at.as_secs_f64())),
        deck.stats()
    );
    std::process::exit(if verified_at.is_some() { 0 } else { 1 });
}
