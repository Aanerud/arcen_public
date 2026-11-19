// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Arcen's H.264/H.265 picture managers against streams Arcen's own encoders
//! produced (see `tests/data/bitstream/README.md`), and against corrupted
//! copies of them: untrusted input must give errors, never panics.
//!
//! `ARCEN_DECODER_VECTORS=<dir>` additionally replays every `*.264`/`*.265`
//! file in that directory (larger lab recordings kept outside the repository).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};

use arcen_media::annexb::{NalCodec, split_access_units};
use arcen_media::bitstream::h264::H264Parser;
use arcen_media::bitstream::hevc::HevcParser;
use arcen_media::bitstream::picture::{OutputPicture, Slot, StreamFormat};

fn data(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/data/bitstream")
        .join(name)
}

/// Per picture: (slot, POC, reference slots, output).
#[derive(Debug)]
struct Decoded {
    format: Option<StreamFormat>,
    pictures: Vec<(Slot, i32, Vec<Slot>, Vec<OutputPicture>)>,
}

fn replay(codec: NalCodec, stream: &[u8]) -> Result<Decoded, String> {
    let units = split_access_units(codec, stream);
    let mut decoded = Decoded {
        format: None,
        pictures: Vec::new(),
    };
    match codec {
        NalCodec::H264 => {
            let mut parser = H264Parser::new(1);
            for (index, unit) in units.iter().enumerate() {
                let au = parser
                    .decode_access_unit(unit, &|_| false)
                    .map_err(|error| format!("unit {index}: {error}"))?;
                decoded.format = decoded.format.or(au.format);
                let picture = au.picture.ok_or(format!("unit {index}: no picture"))?;
                for slice in &picture.slices {
                    assert!(matches!(unit[slice.nal.start] & 0x1f, 1 | 5));
                    for entry in slice.ref_list0.iter().chain(&slice.ref_list1).flatten() {
                        assert!(usize::from(*entry) < picture.references.len());
                    }
                }
                let poc = picture.top_poc.min(picture.bottom_poc);
                let references = picture.references.iter().map(|r| r.slot).collect();
                decoded
                    .pictures
                    .push((picture.slot, poc, references, au.output));
            }
        }
        NalCodec::H265 => {
            let mut parser = HevcParser::new(1);
            for (index, unit) in units.iter().enumerate() {
                let au = parser
                    .decode_access_unit(unit, &|_| false)
                    .map_err(|error| format!("unit {index}: {error}"))?;
                decoded.format = decoded.format.or(au.format);
                let picture = au.picture.ok_or(format!("unit {index}: no picture"))?;
                for slice in &picture.slices {
                    assert!((unit[slice.nal.start] >> 1) & 0x3f < 32);
                    for entry in slice.ref_list0.iter().chain(&slice.ref_list1) {
                        assert!(usize::from(*entry) < picture.references.len());
                    }
                }
                let references = picture.references.iter().map(|r| r.slot).collect();
                decoded
                    .pictures
                    .push((picture.slot, picture.poc, references, au.output));
            }
        }
    }
    Ok(decoded)
}

/// Arcen's encoders never reorder: each picture is shown as soon as it is
/// decoded, never decodes into a slot it references, and its POC grows
/// within a coded video sequence.
fn assert_low_latency(decoded: &Decoded) {
    let pool = decoded.format.unwrap().dpb_slots + 1;
    let mut previous_poc = i32::MIN;
    for (index, (slot, poc, references, output)) in decoded.pictures.iter().enumerate() {
        assert!(
            slot.0 < pool,
            "picture {index}: slot {slot:?} outside the pool"
        );
        assert!(
            !references.contains(slot),
            "picture {index}: decodes over a reference"
        );
        assert_eq!(
            output,
            &vec![OutputPicture {
                slot: *slot,
                poc: *poc
            }],
            "picture {index} was not output immediately"
        );
        if references.is_empty() {
            previous_poc = i32::MIN;
        }
        assert!(
            *poc > previous_poc || references.is_empty(),
            "picture {index}: POC went back"
        );
        previous_poc = *poc;
    }
}

fn check_fixture(name: &str, codec: NalCodec, expected: &str, pictures: usize) {
    let stream = std::fs::read(data(name)).unwrap();
    let decoded = replay(codec, &stream).unwrap();
    let format = decoded.format.unwrap();
    let summary = format!(
        "profile={} coded={}x{} visible={}x{} chroma={} depth={}/{} colour={:?}",
        format.profile_idc,
        format.coded_width,
        format.coded_height,
        format.visible.2,
        format.visible.3,
        format.chroma_format_idc,
        format.bit_depth_luma,
        format.bit_depth_chroma,
        format
            .colour
            .map(|c| (c.full_range, c.primaries, c.transfer, c.matrix)),
    );
    assert_eq!(summary, expected, "{name}");
    assert_eq!(decoded.pictures.len(), pictures, "{name}");
    assert_low_latency(&decoded);
}

#[test]
fn openh264_baseline_stream() {
    check_fixture(
        "openh264-160x96.264",
        NalCodec::H264,
        "profile=66 coded=160x96 visible=160x96 chroma=1 depth=8/8 colour=Some((false, 1, 1, 1))",
        24,
    );
}

#[test]
fn openh264_four_slice_stream() {
    // The software encoder's threaded stream: every picture is four slices
    // (deblocking off across their edges), and each Deck's picture manager
    // must take it as one low-latency picture.
    check_fixture(
        "openh264-4slice-320x192.264",
        NalCodec::H264,
        "profile=66 coded=320x192 visible=320x192 chroma=1 depth=8/8 colour=Some((false, 1, 1, 1))",
        12,
    );
    let stream = std::fs::read(data("openh264-4slice-320x192.264")).unwrap();
    let mut parser = H264Parser::new(1);
    for (index, unit) in split_access_units(NalCodec::H264, &stream)
        .into_iter()
        .enumerate()
    {
        let decoded = parser.decode_access_unit(unit, &|_| false).unwrap();
        let picture = decoded.picture.unwrap();
        assert_eq!(picture.slices.len(), 4, "picture {index}");
    }
}

#[test]
fn nvenc_h264_high_stream() {
    check_fixture(
        "nvenc-h264-high-640x360.264",
        NalCodec::H264,
        "profile=100 coded=640x368 visible=640x360 chroma=1 depth=8/8 colour=Some((false, 1, 1, 1))",
        40,
    );
}

#[test]
fn nvenc_hevc_main_stream() {
    check_fixture(
        "nvenc-hevc-main-640x360.265",
        NalCodec::H265,
        "profile=1 coded=640x384 visible=640x360 chroma=1 depth=8/8 colour=Some((false, 1, 1, 1))",
        40,
    );
}

#[test]
fn nvenc_hevc_main10_stream() {
    check_fixture(
        "nvenc-hevc-main10-640x360.265",
        NalCodec::H265,
        "profile=2 coded=640x384 visible=640x360 chroma=1 depth=10/10 colour=Some((false, 1, 13, 1))",
        40,
    );
}

#[test]
fn nvenc_hevc_rext_444_10_grading_stream() {
    check_fixture(
        "nvenc-hevc-rext444-10-640x360.265",
        NalCodec::H265,
        "profile=4 coded=640x384 visible=640x360 chroma=3 depth=10/10 colour=Some((true, 1, 13, 1))",
        40,
    );
}

#[test]
fn nvenc_hevc_rext_444_10_hdr_pq_stream() {
    check_fixture(
        "nvenc-hevc-rext444-10-pq-640x360.265",
        NalCodec::H265,
        "profile=4 coded=640x384 visible=640x360 chroma=3 depth=10/10 colour=Some((true, 9, 16, 9))",
        40,
    );
}

/// Deterministic xorshift, so a failure reproduces.
struct Noise(u64);

impl Noise {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, bound: usize) -> usize {
        usize::try_from(self.next() % u64::try_from(bound.max(1)).unwrap()).unwrap()
    }
}

/// Flips, truncates, duplicates and zeroes bytes of every access unit and
/// feeds the results to fresh and warmed-up parsers. Any outcome but a panic
/// is acceptable.
fn mutate_and_replay(codec: NalCodec, stream: &[u8], rounds: usize, seed: u64) {
    let units: Vec<Vec<u8>> = split_access_units(codec, stream)
        .into_iter()
        .map(<[u8]>::to_vec)
        .collect();
    let mut noise = Noise(seed);
    let mut h264 = H264Parser::new(1);
    let mut hevc = HevcParser::new(1);
    let mut feed = |unit: &[u8]| match codec {
        NalCodec::H264 => drop(h264.decode_access_unit(unit, &|_| false)),
        NalCodec::H265 => drop(hevc.decode_access_unit(unit, &|_| false)),
    };
    for round in 0..rounds {
        let mut unit = units[round % units.len()].clone();
        match noise.below(5) {
            0 => {
                for _ in 0..=noise.below(4) {
                    let index = noise.below(unit.len());
                    unit[index] ^= 1 << noise.below(8);
                }
            }
            1 => unit.truncate(noise.below(unit.len())),
            2 => {
                let index = noise.below(unit.len());
                let byte = unit[index];
                unit.splice(index..index, std::iter::repeat_n(byte, noise.below(16)));
            }
            3 => {
                let start = noise.below(unit.len());
                let end = (start + noise.below(32)).min(unit.len());
                unit[start..end].fill(if noise.below(2) == 0 { 0 } else { 0xff });
            }
            _ => {}
        }
        feed(&unit);
    }
    let mut garbage = vec![0_u8; 4096];
    for _ in 0..200 {
        let length = noise.below(garbage.len());
        for byte in &mut garbage[..length] {
            *byte = u8::try_from(noise.next() & 0xff).unwrap();
        }
        garbage[..4].copy_from_slice(&[0, 0, 0, 1]);
        feed(&garbage[..length.max(5)]);
    }
}

#[test]
fn corrupted_streams_never_panic() {
    for (name, codec) in fixtures() {
        let stream = std::fs::read(data(name)).unwrap();
        mutate_and_replay(codec, &stream, 3000, 0x5eed_0000 + stream.len() as u64);
    }
}

fn fixtures() -> Vec<(&'static str, NalCodec)> {
    vec![
        ("openh264-160x96.264", NalCodec::H264),
        ("openh264-4slice-320x192.264", NalCodec::H264),
        ("nvenc-h264-high-640x360.264", NalCodec::H264),
        ("nvenc-hevc-main-640x360.265", NalCodec::H265),
        ("nvenc-hevc-main10-640x360.265", NalCodec::H265),
        ("nvenc-hevc-rext444-10-640x360.265", NalCodec::H265),
        ("nvenc-hevc-rext444-10-pq-640x360.265", NalCodec::H265),
    ]
}

#[test]
fn lab_vectors_outside_the_repository_replay_cleanly() {
    let Ok(directory) = std::env::var("ARCEN_DECODER_VECTORS") else {
        return;
    };
    let mut entries: Vec<PathBuf> = std::fs::read_dir(&directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    entries.sort();
    for path in entries {
        let codec = match path.extension().and_then(|e| e.to_str()) {
            Some("264") => NalCodec::H264,
            Some("265") => NalCodec::H265,
            _ => continue,
        };
        let stream = std::fs::read(&path).unwrap();
        let decoded =
            replay(codec, &stream).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        assert_low_latency(&decoded);
        mutate_and_replay(codec, &stream, 500, 7);
        println!("{}: {} pictures", path.display(), decoded.pictures.len());
    }
}

/// The contract each recording was served under, from its name: what the
/// Pier's header and session colour claimed for it.
fn served_contract(name: &str) -> arcen_media::VideoConfiguration {
    use arcen_media::{
        BitDepth, ChromaSubsampling, ColorMatrix, ColorPrimaries, ColorRange,
        TransferCharacteristics, VideoCodec, VideoConfiguration,
    };
    let codec = if name.ends_with(".264") {
        VideoCodec::H264
    } else {
        VideoCodec::H265
    };
    let auto = |bit_depth| VideoConfiguration {
        codec,
        chroma: ChromaSubsampling::Yuv420,
        bit_depth,
        range: ColorRange::Limited,
        matrix: ColorMatrix::Bt709,
        primaries: ColorPrimaries::Bt709,
        transfer: TransferCharacteristics::Bt709,
    };
    if name.contains("hdr") || name.contains("-pq") {
        VideoConfiguration {
            codec,
            chroma: ChromaSubsampling::Yuv444,
            bit_depth: BitDepth::Ten,
            range: ColorRange::Full,
            matrix: ColorMatrix::Bt2020Ncl,
            primaries: ColorPrimaries::Bt2020,
            transfer: TransferCharacteristics::Pq,
        }
    } else if name.contains("grading") || name.contains("rext444") {
        VideoConfiguration {
            codec,
            chroma: ChromaSubsampling::Yuv444,
            bit_depth: BitDepth::Ten,
            range: ColorRange::Full,
            matrix: ColorMatrix::Bt709,
            primaries: ColorPrimaries::Bt709,
            transfer: TransferCharacteristics::Srgb,
        }
    } else if name.contains("main10") {
        auto(BitDepth::Ten)
    } else {
        auto(BitDepth::Eight)
    }
}

/// Our encoders' parameter sets agree with the contracts they were served
/// under, so a decoder that refuses contradictions (review R03) refuses no
/// real stream; and the same stream under another contract is refused.
#[test]
fn recorded_streams_agree_with_the_contract_they_were_served_under() {
    let mut paths: Vec<PathBuf> = fixtures().into_iter().map(|(name, _)| data(name)).collect();
    if let Ok(directory) = std::env::var("ARCEN_DECODER_VECTORS") {
        paths.extend(
            std::fs::read_dir(directory)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .filter(|path| {
                    matches!(
                        path.extension().and_then(|e| e.to_str()),
                        Some("264" | "265")
                    )
                }),
        );
    }
    for path in paths {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let codec = if name.ends_with(".264") {
            NalCodec::H264
        } else {
            NalCodec::H265
        };
        let format = replay(codec, &std::fs::read(&path).unwrap())
            .unwrap()
            .format
            .unwrap();
        let served = served_contract(&name);
        assert_eq!(format.contradiction(&served), None, "{name}: {format:?}");
        let mut other = served;
        other.chroma = if served.chroma == arcen_media::ChromaSubsampling::Yuv444 {
            arcen_media::ChromaSubsampling::Yuv420
        } else {
            arcen_media::ChromaSubsampling::Yuv444
        };
        assert!(format.contradiction(&other).is_some(), "{name}");
    }
}
