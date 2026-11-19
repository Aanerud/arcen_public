// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! A Deck for one capture, without a display or a network (ADR 0028): runs
//! a software `capenc` as its child, applies every canvas record it writes
//! to a canvas with the shared CPU executor (`CanvasMirror`, the oracle
//! every Deck executor is tested against), acknowledges each frame on the
//! child's stdin as a Deck would, and logs the canvas digest at the fences
//! the Pier logs its mirror's (`CANVAS_DIGEST_EVERY_FENCES`), so the two
//! compare line for line. Without `canvas=` it counts the full-frame
//! stream instead.
//!
//! ```sh
//! canvas_verify SECONDS -- arcen-pier capenc 0 h264 60 encoder=software-h264 \
//!     cursor=local framed-v2 canvas=v1:16384:2:104857600:25600:0:0:0:50
//! ```
//!
//! Prints one line a second (`wire_bytes`, records, frames, resets) and a
//! final `summary` line; the child's stderr is inherited.

use arcen_media::canvas::serve::CanvasCapencArgs;
use arcen_media::canvas::{
    CANVAS_DIGEST_EVERY_FENCES, CanvasMirror, NoSurfacePixels, digest_fields,
    pixels::ZstdTileDecoder,
};
use arcen_media::capenc_control::CapencControlCommand;
use arcen_media::capenc_stream::{
    CapencRecordKind, RECORD_LENGTH_BYTES, record_body_length, record_kind,
};
use arcen_protocol::canvas::{CanvasControlMsg, CanvasRecord};
use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Default)]
struct Window {
    bytes: u64,
    records: u64,
    frames: u64,
    resets: u64,
    access_units: u64,
}

#[allow(clippy::too_many_lines)]
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let Some(split) = args.iter().position(|arg| arg == "--") else {
        eprintln!("usage: canvas_verify SECONDS -- CAPENC COMMAND…");
        std::process::exit(2);
    };
    let limit = Duration::from_secs(args.get(1).and_then(|s| s.parse().ok()).unwrap_or(30));
    let command = &args[split + 1..];
    let canvas = match CanvasCapencArgs::from_args(command) {
        Ok(canvas) => canvas,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(2);
        }
    };
    let framed_v2 = command.iter().any(|arg| arg == "framed-v2");
    let mut child = Command::new(&command[0])
        .args(&command[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap_or_else(|error| {
            eprintln!("spawn {}: {error}", command[0]);
            std::process::exit(2);
        });
    let (Some(mut stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        eprintln!("the child has no pipes");
        std::process::exit(2);
    };
    let mut stdout = std::io::BufReader::new(stdout);
    let mut mirror = canvas.map(|canvas| CanvasMirror::new(canvas.monitor_id, canvas.terms));
    let mut decoder = ZstdTileDecoder;
    let started = Instant::now();
    let mut last_report = Instant::now();
    let mut window = Window::default();
    let mut total = Window::default();
    let (mut digests, mut refused) = (0u64, 0u64);
    let mut generation = 0u32;
    while started.elapsed() < limit {
        let mut length = [0_u8; RECORD_LENGTH_BYTES];
        if stdout.read_exact(&mut length).is_err() {
            break;
        }
        let (kind, body) = if framed_v2 {
            let Ok(body_length) = record_body_length(length) else {
                eprintln!("bad record length");
                break;
            };
            let mut kind = [0_u8; 1];
            if stdout.read_exact(&mut kind).is_err() {
                break;
            }
            let Ok(kind) = record_kind(kind[0], body_length) else {
                eprintln!("bad record kind {}", kind[0]);
                break;
            };
            let mut body = vec![0_u8; body_length];
            if stdout.read_exact(&mut body).is_err() {
                break;
            }
            (kind, body)
        } else {
            let mut body = vec![0_u8; u32::from_be_bytes(length) as usize];
            if stdout.read_exact(&mut body).is_err() {
                break;
            }
            (CapencRecordKind::AccessUnit, body)
        };
        window.bytes += body.len() as u64;
        window.records += 1;
        match kind {
            CapencRecordKind::AccessUnit => window.access_units += 1,
            CapencRecordKind::Canvas => {
                let Some(mirror) = mirror.as_mut() else {
                    continue;
                };
                let Ok(record) = CanvasRecord::decode(&body) else {
                    refused += 1;
                    continue;
                };
                if let CanvasRecord::Reset(reset) = &record {
                    window.resets += 1;
                    generation = reset.scope.generation.get();
                } else {
                    window.frames += 1;
                }
                if let Err(error) = mirror.apply(&record, &mut decoder, &mut NoSurfacePixels) {
                    refused += 1;
                    eprintln!("canvas record refused: {error:?}");
                    let request = CapencControlCommand::CanvasReset {
                        monitor_id: mirror.state().monitor_id(),
                        generation,
                    };
                    let _ = stdin.write_all(request.as_wire_line().as_bytes());
                    continue;
                }
                let Some(digest) = mirror.digest() else {
                    continue;
                };
                if digest.fence != 0 && digest.fence % CANVAS_DIGEST_EVERY_FENCES == 0 {
                    digests += 1;
                    println!("canvas digest {}", digest_fields(generation, &digest));
                }
                // A prompt Deck: everything applied, nothing waiting.
                let ack = CapencControlCommand::from_canvas_control(&CanvasControlMsg::CanvasAck {
                    monitor_id: mirror.state().monitor_id(),
                    generation,
                    fence: digest.fence,
                    unprocessed_bytes: 0,
                    decode_ms_p95: 1,
                });
                if let Some(ack) = ack
                    && stdin.write_all(ack.as_wire_line().as_bytes()).is_err()
                {
                    break;
                }
            }
            _ => {}
        }
        if last_report.elapsed() >= Duration::from_secs(1) {
            println!(
                "t={:.1}s wire_bytes={} records={} canvas_frames={} resets={} access_units={}",
                started.elapsed().as_secs_f64(),
                window.bytes,
                window.records,
                window.frames,
                window.resets,
                window.access_units
            );
            total.bytes += window.bytes;
            total.records += window.records;
            total.frames += window.frames;
            total.resets += window.resets;
            total.access_units += window.access_units;
            window = Window::default();
            last_report = Instant::now();
        }
    }
    let _ = stdin.write_all(b"STOP\n");
    drop(stdin);
    // Stop reading too: a child blocked writing a record must see the pipe
    // close, or it never reaches its stop check.
    drop(stdout);
    let _ = child.wait();
    total.bytes += window.bytes;
    total.records += window.records;
    total.frames += window.frames;
    total.resets += window.resets;
    total.access_units += window.access_units;
    println!(
        "summary seconds={:.1} wire_bytes={} records={} canvas_frames={} resets={} access_units={} digests={digests} refused={refused}",
        started.elapsed().as_secs_f64(),
        total.bytes,
        total.records,
        total.frames,
        total.resets,
        total.access_units
    );
}
