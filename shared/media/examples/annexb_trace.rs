// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Replays a recorded Annex B stream through Arcen's H.264/H.265 picture
//! managers and prints one line per access unit: picture type, POC, slot,
//! references and output. `annexb_trace h264|h265 FILE [--pool N]`.

use arcen_media::annexb::{NalCodec, split_access_units};
use arcen_media::bitstream::h264::H264Parser;
use arcen_media::bitstream::hevc::HevcParser;

fn main() -> Result<(), String> {
    let args: Vec<String> = std::env::args().collect();
    let codec = match args.get(1).map(String::as_str) {
        Some("h264") => NalCodec::H264,
        Some("h265") => NalCodec::H265,
        _ => return Err("usage: annexb_trace h264|h265 FILE".into()),
    };
    let path = args.get(2).ok_or("missing FILE")?;
    let data = std::fs::read(path).map_err(|error| format!("{path}: {error}"))?;
    let units = split_access_units(codec, &data);
    println!("units={}", units.len());
    match codec {
        NalCodec::H265 => {
            let mut parser = HevcParser::new(1);
            for (index, unit) in units.iter().enumerate() {
                match parser.decode_access_unit(unit, &|_| false) {
                    Ok(au) => {
                        if au.format_changed {
                            println!("format {:?}", au.format);
                        }
                        let out: Vec<String> = au
                            .output
                            .iter()
                            .map(|o| format!("{}@{}", o.poc, o.slot.0))
                            .collect();
                        match au.picture {
                            Some(p) => println!(
                                "au={index} nut={} poc={} slot={} slices={} intra={} refs={:?} before={:?} after={:?} lt={:?} l0={:?} out=[{}]",
                                p.nal_unit_type,
                                p.poc,
                                p.slot.0,
                                p.slices.len(),
                                p.intra_only,
                                p.references
                                    .iter()
                                    .map(|r| (r.poc, r.slot.0, r.long_term))
                                    .collect::<Vec<_>>(),
                                p.st_curr_before,
                                p.st_curr_after,
                                p.lt_curr,
                                p.slices
                                    .first()
                                    .map(|s| s.ref_list0.clone())
                                    .unwrap_or_default(),
                                out.join(",")
                            ),
                            None => println!("au={index} no picture out=[{}]", out.join(",")),
                        }
                    }
                    Err(error) => println!("au={index} error: {error}"),
                }
            }
        }
        NalCodec::H264 => {
            let mut parser = H264Parser::new(1);
            for (index, unit) in units.iter().enumerate() {
                match parser.decode_access_unit(unit, &|_| false) {
                    Ok(au) => {
                        if au.format_changed {
                            println!("format {:?}", au.format);
                        }
                        let out: Vec<String> = au
                            .output
                            .iter()
                            .map(|o| format!("{}@{}", o.poc, o.slot.0))
                            .collect();
                        match au.picture {
                            Some(p) => println!(
                                "au={index} idr={} ref={} frame_num={} poc={} slot={} slices={} intra={} refs={:?} l0={:?} out=[{}]",
                                p.idr,
                                p.reference,
                                p.frame_num,
                                p.top_poc.min(p.bottom_poc),
                                p.slot.0,
                                p.slices.len(),
                                p.intra_only,
                                p.references
                                    .iter()
                                    .map(|r| (r.frame_num_or_long_term_idx, r.slot.0, r.long_term))
                                    .collect::<Vec<_>>(),
                                p.slices
                                    .first()
                                    .map(|s| s.ref_list0.clone())
                                    .unwrap_or_default(),
                                out.join(",")
                            ),
                            None => println!("au={index} no picture out=[{}]", out.join(",")),
                        }
                    }
                    Err(error) => println!("au={index} error: {error}"),
                }
            }
        }
    }
    Ok(())
}
