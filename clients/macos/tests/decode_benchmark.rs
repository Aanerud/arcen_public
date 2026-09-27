//! Measures the Deck's real decoder against a real Pier bitstream.
//!
//! Encode numbers on their own do not tell anyone whether a desktop is usable.
//! This decodes a stream the macOS Pier actually produced, through the same
//! `NativeVideoDecoder` the session uses, and reports what it managed.
//!
//! The stream is produced by:
//!
//! ```sh
//! arcen-pier-macos probe-media --frames 120 --codec hevc --format nv12 \
//!     --out /tmp/arcen-hevc.bin
//! ```
//!
//! Without that file the test reports that it was skipped rather than passing
//! silently, because a decode benchmark that quietly measures nothing is worse
//! than no benchmark.

#![cfg(target_os = "macos")]

use std::time::Instant;

use arcen_deck::pipeline::video_decoder::NativeVideoDecoder;
use arcen_protocol::wire::{decode_video_header, VIDEO_HEADER_SIZE};

/// Where the Pier writes its sample stream.
const STREAM_PATH: &str = "/tmp/arcen-hevc.bin";

/// Splits the length-prefixed capture into access units.
fn access_units(bytes: &[u8]) -> Vec<&[u8]> {
    let mut units = Vec::new();
    let mut offset = 0_usize;
    while offset + 4 <= bytes.len() {
        let length = u32::from_be_bytes([
            bytes[offset],
            bytes[offset + 1],
            bytes[offset + 2],
            bytes[offset + 3],
        ]) as usize;
        offset += 4;
        let end = offset.saturating_add(length);
        if length == 0 || end > bytes.len() {
            break;
        }
        units.push(&bytes[offset..end]);
        offset = end;
    }
    units
}

#[test]
fn the_deck_decoder_keeps_up_with_a_real_pier_stream() {
    let Ok(bytes) = std::fs::read(STREAM_PATH) else {
        println!(
            "decode benchmark skipped: no stream at {STREAM_PATH}; produce one with \
             `arcen-pier-macos probe-media --frames 120 --codec hevc --format nv12 --out {STREAM_PATH}`"
        );
        return;
    };

    let units = access_units(&bytes);
    assert!(
        !units.is_empty(),
        "the captured stream held no access units"
    );

    let mut decoder = NativeVideoDecoder::new();
    let mut decoded = 0_u64;
    let mut submitted = 0_u64;
    let mut first_frame: Option<std::time::Duration> = None;

    let started = Instant::now();
    for unit in &units {
        assert!(
            unit.len() > VIDEO_HEADER_SIZE,
            "an access unit carried a header with no picture behind it"
        );
        let header = decode_video_header(unit).expect("the Pier's header must decode");
        let payload = &unit[VIDEO_HEADER_SIZE..];
        submitted += 1;
        match decoder.decode(&header, payload) {
            Ok(Some(_frame)) => {
                decoded += 1;
                if first_frame.is_none() {
                    first_frame = Some(started.elapsed());
                }
            }
            // VideoToolbox reorders and may hold frames back; that is not a
            // failure, it is latency, and it shows up in the rate below.
            Ok(None) => {}
            Err(error) => panic!("the Deck could not decode the Pier's stream: {error:?}"),
        }
    }
    let elapsed = started.elapsed();

    let seconds = elapsed.as_secs_f64();
    let fps = if seconds > 0.0 {
        f64::from(u32::try_from(decoded).unwrap_or(u32::MAX)) / seconds
    } else {
        0.0
    };
    let mean_ms = if decoded > 0 {
        seconds * 1000.0 / f64::from(u32::try_from(decoded).unwrap_or(1))
    } else {
        0.0
    };

    println!(
        "deck decode: {decoded}/{submitted} frames, {fps:.1} fps, {mean_ms:.2} ms mean, \
         backend {}, hardware {:?}, first frame after {:?}",
        decoder.backend_name(),
        decoder.is_hardware_accelerated(),
        first_frame
    );

    assert!(
        decoded > 0,
        "the Deck decoded none of the {submitted} frames the Pier produced"
    );
    // A decoder that cannot keep up with the encoder is a stuttering desktop,
    // so this is asserted rather than merely printed.
    assert!(
        fps >= 30.0,
        "the Deck decoder managed only {fps:.1} fps, below a usable desktop"
    );
}
