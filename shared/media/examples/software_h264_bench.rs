// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Measures Arcen's software H.264 encoder on synthetic desktop scenes:
//! BGRA to I420 conversion and `OpenH264` encode time per picture, the
//! encode rate one CPU reaches, the stream size and (with `--psnr`) the
//! luma PSNR an `OpenH264` decode of the stream reaches.
//!
//! ```text
//! software_h264_bench [--width W] [--height H] [--frames N] [--threads T]
//!                     [--scene motion|scroll|typing] [--bitrate BPS]
//!                     [--psnr] [--out FILE.264]
//! ```
//!
//! Conversion runs on as many threads as the encoder (as the shared session
//! does); `--threads 1` is the single-threaded path Arcen shipped before.
//! Every picture is converted and encoded back to back, unpaced: the result
//! is the encoder's capacity, not a capture loop's delivered rate. The
//! bitrate defaults to the Software contract's start bitrate for the
//! geometry at 30 fps. The picture size is held to `OpenH264`'s own limits
//! only, so sizes above the Software contract can be measured.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::expect_used,
    clippy::similar_names,
    clippy::too_many_lines
)]

use std::fmt::Write as _;
use std::io::Write as _;
use std::time::Instant;

use arcen_keel::BgraFrame;
use arcen_media::video::{
    ColorTransform, I420Frame, I420FrameMut, SoftwareH264Config, SoftwareH264Encoder,
    convert_bgra_to_i420_rows_parallel, software_bitrate_bounds,
};
use arcen_media::{BitDepth, ChromaSubsampling, ColorMatrix, ColorRange};
use openh264::formats::YUVSource as _;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Scene {
    /// Every pixel moves: a textured picture panning diagonally (video,
    /// a dragged window, a 3D view).
    Motion,
    /// A page of text scrolling 24 rows per picture.
    Scroll,
    /// A still page of text with one word-sized area changing.
    Typing,
}

struct Args {
    width: u32,
    height: u32,
    frames: usize,
    threads: u16,
    scene: Scene,
    bitrate: Option<u32>,
    psnr: bool,
    out: Option<String>,
}

fn parse() -> Result<Args, String> {
    let mut args = Args {
        width: 1920,
        height: 1080,
        frames: 120,
        threads: 1,
        scene: Scene::Motion,
        bitrate: None,
        psnr: false,
        out: None,
    };
    let mut items = std::env::args().skip(1);
    while let Some(flag) = items.next() {
        let mut value = || items.next().ok_or(format!("{flag} needs a value"));
        let number = |text: String| text.parse::<u64>().map_err(|error| error.to_string());
        match flag.as_str() {
            "--width" => {
                args.width = u32::try_from(number(value()?)?).map_err(|e| e.to_string())?;
            }
            "--height" => {
                args.height = u32::try_from(number(value()?)?).map_err(|e| e.to_string())?;
            }
            "--frames" => {
                args.frames = usize::try_from(number(value()?)?).map_err(|e| e.to_string())?;
            }
            "--threads" => {
                args.threads = u16::try_from(number(value()?)?).map_err(|e| e.to_string())?;
            }
            "--bitrate" => {
                args.bitrate = Some(u32::try_from(number(value()?)?).map_err(|e| e.to_string())?);
            }
            "--scene" => {
                args.scene = match value()?.as_str() {
                    "motion" => Scene::Motion,
                    "scroll" => Scene::Scroll,
                    "typing" => Scene::Typing,
                    other => return Err(format!("unknown scene {other}")),
                };
            }
            "--psnr" => args.psnr = true,
            "--out" => args.out = Some(value()?),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(args)
}

/// A desktop-like picture larger than the frame: a page of text over
/// smooth gradients with a little grain, so motion search has real work
/// and something to find.
fn texture(width: usize, height: usize) -> Vec<u8> {
    let mut pixels = page(width, height);
    let mut state = 0x2545_f491_4f6c_dd1d_u64;
    for row in 0..height {
        for column in 0..width {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let grain = (state & 0x3) as usize;
            let index = (row * width + column) * 4;
            if pixels[index] == 0x20 {
                continue;
            }
            let panel = (row / 270 + column / 480) % 3;
            pixels[index] = ((column / 4 + grain + panel * 60) % 256) as u8;
            pixels[index + 1] = ((row / 3 + column / 9 + grain) % 256) as u8;
            pixels[index + 2] = (((row + column) / 6 + panel * 40) % 256) as u8;
        }
    }
    pixels
}

/// Dark glyph-like strokes on a light page, one 8x16 cell per character.
fn page(width: usize, height: usize) -> Vec<u8> {
    let mut pixels = vec![0xf0_u8; width * height * 4];
    for row in 0..height {
        for column in 0..width {
            let (cell_x, cell_y) = (column / 8, row / 16);
            let (x, y) = (column % 8, row % 16);
            let line_has_text = cell_y % 3 != 2 && (cell_x * 7 + cell_y * 13) % 11 > 2;
            let stroke = (y > 3 && y < 13) && (x == 1 || x == 5 || (y == 8 && x < 6));
            let seed = (cell_x * 31 + cell_y * 17) % 5;
            if line_has_text && stroke && seed != 0 {
                let index = (row * width + column) * 4;
                pixels[index..index + 3].copy_from_slice(&[0x20, 0x20, 0x20]);
            }
        }
    }
    pixels
}

struct Source {
    scene: Scene,
    width: usize,
    height: usize,
    base: Vec<u8>,
    base_width: usize,
    frame: Vec<u8>,
}

impl Source {
    fn new(scene: Scene, width: usize, height: usize) -> Self {
        let (base, base_width) = match scene {
            Scene::Motion => (texture(width + 512, height + 512), width + 512),
            Scene::Scroll => (page(width, height * 4), width),
            Scene::Typing => (page(width, height), width),
        };
        Self {
            scene,
            width,
            height,
            base,
            base_width,
            frame: vec![0; width * height * 4],
        }
    }

    fn render(&mut self, index: usize) -> BgraFrame<'_> {
        let row_bytes = self.width * 4;
        match self.scene {
            Scene::Motion => {
                let (dx, dy) = ((index * 7) % 512, (index * 3) % 512);
                for row in 0..self.height {
                    let from = ((row + dy) * self.base_width + dx) * 4;
                    self.frame[row * row_bytes..(row + 1) * row_bytes]
                        .copy_from_slice(&self.base[from..from + row_bytes]);
                }
            }
            Scene::Scroll => {
                let total = self.height * 3;
                let offset = (index * 24) % total;
                for row in 0..self.height {
                    let from = ((row + offset) % (self.height * 4)) * row_bytes;
                    self.frame[row * row_bytes..(row + 1) * row_bytes]
                        .copy_from_slice(&self.base[from..from + row_bytes]);
                }
            }
            Scene::Typing => {
                if index == 0 {
                    self.frame.copy_from_slice(&self.base);
                }
                let (cell_x, cell_y) = (8 + index % 64, 10 + (index / 64) % 20);
                for row in cell_y * 16..(cell_y * 16 + 16).min(self.height) {
                    for column in cell_x * 8..(cell_x * 8 + 8).min(self.width) {
                        let index = (row * self.width + column) * 4;
                        let ink = if (row + column) % 3 == 0 { 0x20 } else { 0xf0 };
                        self.frame[index..index + 3].copy_from_slice(&[ink, ink, ink]);
                    }
                }
            }
        }
        BgraFrame::new(&self.frame, self.width, self.height, row_bytes).expect("BGRA frame")
    }
}

fn psnr(source: (&[u8], usize), decoded: (&[u8], usize), width: usize, height: usize) -> f64 {
    let mut squared = 0_u64;
    for row in 0..height {
        let a = &source.0[row * source.1..row * source.1 + width];
        let b = &decoded.0[row * decoded.1..row * decoded.1 + width];
        for (x, y) in a.iter().zip(b) {
            let difference = u64::from(x.abs_diff(*y));
            squared += difference * difference;
        }
    }
    if squared == 0 {
        return 99.0;
    }
    let mse = squared as f64 / (width * height) as f64;
    10.0 * (255.0 * 255.0 / mse).log10()
}

fn percentile(sorted: &[f64], fraction: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let index = ((sorted.len() - 1) as f64 * fraction).round() as usize;
    sorted[index]
}

fn main() -> Result<(), String> {
    let args = parse()?;
    let (width, height) = (args.width as usize, args.height as usize);
    let bitrate = args.bitrate.unwrap_or_else(|| {
        software_bitrate_bounds(
            args.width,
            args.height,
            30,
            ChromaSubsampling::Yuv420,
            BitDepth::Eight,
        )
        .0
    });
    let config = SoftwareH264Config::legacy_bt709_limited(
        args.width,
        args.height,
        30,
        bitrate,
        args.threads,
    );
    let opened = Instant::now();
    let mut encoder = SoftwareH264Encoder::new(config).map_err(|error| error.to_string())?;
    let open_ms = opened.elapsed().as_secs_f64() * 1000.0;
    let threading = encoder.threading();
    let transform = ColorTransform::new(ColorMatrix::Bt709, ColorRange::Limited, BitDepth::Eight);
    let mut source = Source::new(args.scene, width, height);
    let (mut y, mut u, mut v) = (
        vec![0_u8; width * height],
        vec![0_u8; width * height / 4],
        vec![0_u8; width * height / 4],
    );
    let mut decoder = if args.psnr {
        Some(openh264::decoder::Decoder::new().map_err(|error| error.to_string())?)
    } else {
        None
    };
    let mut out = match &args.out {
        Some(path) => Some(std::fs::File::create(path).map_err(|error| error.to_string())?),
        None => None,
    };
    let (mut convert_ms, mut encode_ms, mut coded_ms) = (Vec::new(), Vec::new(), Vec::new());
    let mut skipped = 0_usize;
    let (mut bytes, mut psnr_sum, mut psnr_min, mut decoded) = (0_u64, 0.0, f64::MAX, 0_usize);
    let full = 0..height;
    let started = Instant::now();
    for index in 0..args.frames {
        let picture = source.render(index);
        let converting = Instant::now();
        {
            let mut destination = I420FrameMut::new(
                args.width,
                args.height,
                &mut y,
                width,
                &mut u,
                width / 2,
                &mut v,
                width / 2,
            )
            .map_err(|error| error.to_string())?;
            convert_bgra_to_i420_rows_parallel(
                picture,
                &mut destination,
                std::slice::from_ref(&full),
                transform,
                usize::from(threading.threads),
            )
            .map_err(|error| error.to_string())?;
        }
        convert_ms.push(converting.elapsed().as_secs_f64() * 1000.0);
        let frame = I420Frame::new(
            args.width,
            args.height,
            &y,
            width,
            &u,
            width / 2,
            &v,
            width / 2,
        )
        .map_err(|error| error.to_string())?;
        let encoding = Instant::now();
        let unit = encoder.encode(frame).map_err(|error| error.to_string())?;
        encode_ms.push(encoding.elapsed().as_secs_f64() * 1000.0);
        let Some(unit) = unit else {
            skipped += 1;
            continue;
        };
        coded_ms.push(encode_ms[encode_ms.len() - 1]);
        bytes += unit.bytes.len() as u64;
        if let Some(file) = out.as_mut() {
            file.write_all(unit.bytes)
                .map_err(|error| error.to_string())?;
        }
        if let Some(decoder) = decoder.as_mut()
            && let Some(picture) = decoder
                .decode(unit.bytes)
                .map_err(|error| error.to_string())?
        {
            let value = psnr(
                (&y, width),
                (picture.y(), picture.strides().0),
                width,
                height,
            );
            psnr_sum += value;
            psnr_min = psnr_min.min(value);
            decoded += 1;
        }
    }
    let wall = started.elapsed().as_secs_f64();
    let frames = encode_ms.len().max(1) as f64;
    let encode_mean = encode_ms.iter().sum::<f64>() / frames;
    let convert_mean = convert_ms.iter().sum::<f64>() / frames;
    encode_ms.sort_by(f64::total_cmp);
    let coded_mean = coded_ms.iter().sum::<f64>() / coded_ms.len().max(1) as f64;
    let scene = match args.scene {
        Scene::Motion => "motion",
        Scene::Scroll => "scroll",
        Scene::Typing => "typing",
    };
    let mut line = format!(
        "scene={scene} size={}x{} frames={} threads_requested={} slices={} threads={} \
         bitrate_bps={bitrate} open_ms={open_ms:.1} encode_ms_mean={encode_mean:.2} \
         encode_ms_p50={:.2} encode_ms_p95={:.2} encode_ms_max={:.2} convert_ms_mean={convert_mean:.2} \
         encode_fps={:.1} convert_encode_fps={:.1} skipped={skipped} coded_ms_mean={coded_mean:.2} \
         kbps_at_30={:.0} wall_s={wall:.2}",
        args.width,
        args.height,
        args.frames,
        args.threads,
        threading.slices,
        threading.threads,
        percentile(&encode_ms, 0.5),
        percentile(&encode_ms, 0.95),
        percentile(&encode_ms, 1.0),
        1000.0 / encode_mean,
        1000.0 / (encode_mean + convert_mean),
        bytes as f64 * 8.0 / frames * 30.0 / 1000.0,
    );
    if decoded > 0 {
        let _ = write!(
            line,
            " decoded={decoded} psnr_y_mean={:.2} psnr_y_min={psnr_min:.2}",
            psnr_sum / decoded as f64
        );
    }
    println!("{line}");
    Ok(())
}
