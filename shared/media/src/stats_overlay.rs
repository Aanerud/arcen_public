// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The compact stream-statistics overlay (P2-4): path, delivery, contract,
//! dynamic range and lossless-refinement coverage in three short lines.
//!
//! Portable formatting only; a Deck paints the lines. Every value comes
//! from what the Pier reported (`PathQualityMsg`, the hello's active colour
//! axes) or what the Deck measured (presented frames, refinement state);
//! nothing is guessed, and an unreported axis reads as unknown.

use crate::colour_check::ColourAxes;
use crate::{BitDepth, ChromaSubsampling, ColorPrimaries, TransferCharacteristics, VideoCodec};
use arcen_protocol::messages::PathQualityMsg;

/// Inputs of [`stats_overlay_lines`].
#[derive(Debug, Clone, Copy, Default)]
pub struct StatsOverlayInputs<'a> {
    /// The Pier's latest path measurement.
    pub path: Option<&'a PathQualityMsg>,
    /// Frames the Deck presented per second.
    pub presented_fps: f64,
    /// The stream's frame-rate ceiling, when known.
    pub max_fps: u32,
    /// The colour axes the Pier says it serves.
    pub axes: Option<&'a ColourAxes>,
    /// The served pipeline's label (`Auto`, `Speed`, `Grading`, `HDR`).
    pub pipeline: Option<&'a str>,
    /// Lossless refinement status line (scoped wording, ADR 0016 R4).
    pub refine: Option<&'a str>,
}

/// `SDR` or `HDR` from the transfer: bit depth never implies HDR.
#[must_use]
pub fn dynamic_range_label(transfer: Option<TransferCharacteristics>) -> &'static str {
    match transfer {
        Some(TransferCharacteristics::Pq) => "HDR PQ",
        Some(TransferCharacteristics::Hlg) => "HDR HLG",
        Some(TransferCharacteristics::Bt709) => "SDR BT.709",
        Some(TransferCharacteristics::Srgb) => "SDR sRGB",
        None => "range unknown",
    }
}

fn codec_label(codec: Option<VideoCodec>) -> &'static str {
    match codec {
        Some(VideoCodec::H264) => "H.264",
        Some(VideoCodec::H265) => "HEVC",
        Some(VideoCodec::Av1) => "AV1",
        Some(VideoCodec::Vp9) => "VP9",
        Some(VideoCodec::Jpeg) => "JPEG",
        None => "codec ?",
    }
}

fn chroma_label(chroma: Option<ChromaSubsampling>) -> &'static str {
    match chroma {
        Some(ChromaSubsampling::Yuv420) => "4:2:0",
        Some(ChromaSubsampling::Yuv422) => "4:2:2",
        Some(ChromaSubsampling::Yuv444) => "4:4:4",
        None => "chroma ?",
    }
}

fn depth_label(depth: Option<BitDepth>) -> String {
    depth.map_or_else(
        || "depth ?".to_string(),
        |depth| format!("{}-bit", depth.bits()),
    )
}

fn primaries_label(primaries: Option<ColorPrimaries>) -> &'static str {
    match primaries {
        Some(ColorPrimaries::Bt709) => "BT.709",
        Some(ColorPrimaries::Bt2020) => "BT.2020",
        Some(ColorPrimaries::DisplayP3) => "P3",
        None => "primaries ?",
    }
}

/// Three lines: path, stream contract, lossless refinement.
#[must_use]
pub fn stats_overlay_lines(inputs: &StatsOverlayInputs<'_>) -> [String; 3] {
    let path = inputs.path.map_or_else(
        || "RTT - · loss - · path not reported".to_string(),
        |path| {
            let loss = path.loss_permille().map_or_else(
                || "-".to_string(),
                |permille| format!("{}.{}%", permille / 10, permille % 10),
            );
            let rate = path.wire_bps().map_or_else(
                || "-".to_string(),
                |bps| format!("{}.{} Mbit/s", bps / 1_000_000, (bps % 1_000_000) / 100_000),
            );
            format!(
                "RTT {} ms (min {}) · loss {loss} · {rate}",
                path.rtt_us / 1000,
                path.min_rtt_us / 1000
            )
        },
    );
    let fps = if inputs.max_fps > 0 {
        format!("{:.1}/{} fps", inputs.presented_fps, inputs.max_fps)
    } else {
        format!("{:.1} fps", inputs.presented_fps)
    };
    let axes = inputs.axes.copied().unwrap_or_default();
    let stream = format!(
        "{fps} · {} {} {} · {} · {} · {} primaries",
        codec_label(axes.codec),
        chroma_label(axes.chroma),
        depth_label(axes.bit_depth),
        inputs.pipeline.unwrap_or("pipeline ?"),
        dynamic_range_label(axes.transfer),
        primaries_label(axes.primaries),
    );
    let refine = format!(
        "Lossless: {}",
        inputs.refine.unwrap_or("not negotiated with this Pier")
    );
    [path, stream, refine]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grading_is_sdr_and_hdr_needs_a_pq_or_hlg_transfer() {
        let axes = ColourAxes {
            codec: Some(VideoCodec::H265),
            chroma: Some(ChromaSubsampling::Yuv444),
            bit_depth: Some(BitDepth::Ten),
            primaries: Some(ColorPrimaries::Bt709),
            transfer: Some(TransferCharacteristics::Bt709),
            ..ColourAxes::default()
        };
        let path = PathQualityMsg {
            rtt_us: 23_400,
            min_rtt_us: 20_100,
            sent_packets: 1000,
            lost_packets: 2,
            wire_bytes: 1_550_000,
            window_ms: 1000,
            ..PathQualityMsg::default()
        };
        let lines = stats_overlay_lines(&StatsOverlayInputs {
            path: Some(&path),
            presented_fps: 59.84,
            max_fps: 60,
            axes: Some(&axes),
            pipeline: Some("Grading"),
            refine: None,
        });
        assert!(
            lines[0].starts_with("RTT 23 ms (min 20) · loss "),
            "{}",
            lines[0]
        );
        assert_eq!(
            lines[1],
            "59.8/60 fps · HEVC 4:4:4 10-bit · Grading · SDR BT.709 · BT.709 primaries"
        );
        assert_eq!(lines[2], "Lossless: not negotiated with this Pier");
        assert_eq!(
            dynamic_range_label(Some(TransferCharacteristics::Pq)),
            "HDR PQ"
        );
        let unknown = stats_overlay_lines(&StatsOverlayInputs::default());
        assert_eq!(unknown[0], "RTT - · loss - · path not reported");
        assert!(unknown[1].contains("range unknown"));
    }
}
