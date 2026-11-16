// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Writes a small Annex B test stream from Arcen's OpenH264 software encoder:
//! a synthetic picture with moving blocks, so decoders can be tested against
//! a stream Arcen itself produces. `software_h264_vector OUT [W H FRAMES]`.

use std::io::Write as _;

use arcen_media::video::{I420Frame, SoftwareH264Config, SoftwareH264Encoder};

fn main() -> Result<(), String> {
    let args: Vec<String> = std::env::args().collect();
    let out = args
        .get(1)
        .ok_or("usage: software_h264_vector OUT [W H FRAMES]")?;
    let number = |index: usize, default: u32| {
        args.get(index)
            .and_then(|value| value.parse().ok())
            .unwrap_or(default)
    };
    let (width, height, frames) = (number(2, 320), number(3, 180), number(4, 60));
    let config = SoftwareH264Config::legacy_bt709_limited(width, height, 30, 2_000_000, 1);
    let mut encoder = SoftwareH264Encoder::new(config).map_err(|error| error.to_string())?;
    let (w, h) = (width as usize, height as usize);
    let mut y = vec![0_u8; w * h];
    let mut u = vec![128_u8; w * h / 4];
    let mut v = vec![128_u8; w * h / 4];
    let mut file = std::fs::File::create(out).map_err(|error| error.to_string())?;
    for frame in 0..frames as usize {
        for row in 0..h {
            for column in 0..w {
                let block = (column + frame * 3) / 16 % 2 == (row + frame) / 16 % 2;
                y[row * w + column] = if block {
                    200
                } else {
                    ((column + row) % 160 + 16) as u8
                };
            }
        }
        for row in 0..h / 2 {
            for column in 0..w / 2 {
                u[row * w / 2 + column] = ((column * 2 + frame * 4) % 224 + 16) as u8;
                v[row * w / 2 + column] = ((row * 2 + frame * 2) % 224 + 16) as u8;
            }
        }
        if frame == frames as usize / 2 {
            encoder.force_idr();
        }
        let picture = I420Frame::new(width, height, &y, w, &u, w / 2, &v, w / 2)
            .map_err(|error| error.to_string())?;
        if let Some(unit) = encoder.encode(picture).map_err(|error| error.to_string())? {
            file.write_all(unit.bytes)
                .map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}
