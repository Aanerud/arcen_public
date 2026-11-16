// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Lab tool for Arcen's D3D11 decoder.
//!
//! - `d3d11_decode probe`: every hardware adapter and what each decode
//!   profile proved (what the hello would advertise).
//! - `d3d11_decode decode h264|h265 IN OUT [--adapter N]`: decodes a
//!   recorded Annex B stream and writes every output picture's visible area
//!   as planar YUV (the layout `ffmpeg -f rawvideo` writes for the stream's
//!   native pixel format), for a bit-exact comparison with a software decode.

#[cfg(windows)]
fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("d3d11_decode: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(windows)]
fn run() -> Result<(), String> {
    use std::io::Write as _;

    use arcen_deck_windows::decode::readback::read_planar;
    use arcen_deck_windows::decode::{D3d11VideoDecoder, VideoDevice, hardware_adapters};
    use arcen_media::annexb::{NalCodec, split_access_units};
    use arcen_media::bitstream::picture::StreamCodecKind;

    let args: Vec<String> = std::env::args().skip(1).collect();
    let adapters = hardware_adapters().map_err(|error| format!("adapters: {error}"))?;
    let adapter_index = args
        .iter()
        .position(|arg| arg == "--adapter")
        .and_then(|index| args.get(index + 1))
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    match args.first().map(String::as_str) {
        Some("probe") => {
            for (index, (adapter, name, vendor, device, _)) in adapters.iter().enumerate() {
                println!("adapter {index}: {name} vendor={vendor:04x} device={device:04x}");
                let video = match VideoDevice::create(adapter, name.clone(), *vendor, *device) {
                    Ok(video) => video,
                    Err(error) => {
                        println!("  device: {error}");
                        continue;
                    }
                };
                let (capabilities, results) = arcen_deck_windows::decode::prove(&video);
                for (profile, result) in results {
                    match result {
                        Ok(()) => println!("  {:<16} ok ({:?})", profile.name, profile.output),
                        Err(reason) => println!("  {:<16} refused: {reason}", profile.name),
                    }
                }
                println!(
                    "  hello: backend={} h264={} h265={} main10={} yuv444={}",
                    capabilities.decoder_backend_label(),
                    capabilities.h264,
                    capabilities.h265,
                    capabilities.main10,
                    capabilities.yuv444
                );
            }
            Ok(())
        }
        Some("decode") => {
            let (codec, nal_codec) = match args.get(1).map(String::as_str) {
                Some("h264") => (StreamCodecKind::H264, NalCodec::H264),
                Some("h265") => (StreamCodecKind::H265, NalCodec::H265),
                _ => return Err("decode h264|h265 IN OUT".into()),
            };
            let input = args.get(2).ok_or("missing IN")?;
            let output = args.get(3).ok_or("missing OUT")?;
            let (adapter, name, vendor, device, _) =
                adapters.get(adapter_index).ok_or("no such adapter")?;
            let video = VideoDevice::create(adapter, name.clone(), *vendor, *device)
                .map_err(|error| format!("device: {error}"))?;
            let d3d = video.device.clone();
            let lock = std::sync::Arc::clone(&video.context_lock);
            let mut decoder = D3d11VideoDecoder::new(video);
            let stream = std::fs::read(input).map_err(|error| format!("{input}: {error}"))?;
            let mut sink =
                std::fs::File::create(output).map_err(|error| format!("{output}: {error}"))?;
            let started = std::time::Instant::now();
            let (mut pictures, mut errors) = (0_usize, 0_usize);
            for (index, unit) in split_access_units(nal_codec, &stream).iter().enumerate() {
                match decoder.decode_annex_b(codec, unit) {
                    Ok(ready) => {
                        for picture in ready {
                            let planar = read_planar(&d3d, &lock, &picture)?;
                            sink.write_all(&planar).map_err(|error| error.to_string())?;
                            pictures += 1;
                        }
                    }
                    Err(error) => {
                        errors += 1;
                        eprintln!("unit {index}: {error}");
                    }
                }
            }
            println!(
                "adapter={name} backend={} pictures={pictures} errors={errors} seconds={:.2}",
                arcen_deck_core::decode::VideoDecoderBackend::backend_name(&decoder),
                started.elapsed().as_secs_f64()
            );
            if errors == 0 {
                Ok(())
            } else {
                Err(format!("{errors} errors"))
            }
        }
        _ => Err("usage: d3d11_decode probe | decode h264|h265 IN OUT [--adapter N]".into()),
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("d3d11_decode runs on Windows only");
}
