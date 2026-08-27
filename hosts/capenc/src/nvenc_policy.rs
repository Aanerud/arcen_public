use arcen_media::{BitDepth, ChromaSubsampling, EncodeIntent};

/// Bits/second and VBV sizing shared by the D3D11 and CUDA NVENC paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RateControlSizing {
    pub(crate) average_bitrate_bps: u32,
    pub(crate) max_bitrate_bps: u32,
    pub(crate) vbv_buffer_size_bits: u32,
}

const fn samples_per_pixel(chroma: ChromaSubsampling) -> f64 {
    match chroma {
        ChromaSubsampling::Yuv420 => 1.5,
        ChromaSubsampling::Yuv422 => 2.0,
        ChromaSubsampling::Yuv444 => 3.0,
    }
}

const fn depth_scale(depth: BitDepth) -> f64 {
    match depth {
        BitDepth::Eight => 1.0,
        BitDepth::Ten => 1.25,
        BitDepth::Twelve => 1.5,
    }
}

const BASE_BITS_PER_SAMPLE: f64 = 0.05;

/// The bounded number of input/output slots NVENC may retain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OutputDrainPolicy {
    max_inflight: usize,
}

impl OutputDrainPolicy {
    pub(crate) const fn max_inflight(self) -> usize {
        self.max_inflight
    }

    pub(crate) const fn slot_count(self) -> usize {
        self.max_inflight
    }
}

/// NVENC documents 32 as the largest supported lookahead depth. Clamp a
/// malformed or future preset to that API bound so allocation stays bounded.
const MAX_NVENC_LOOKAHEAD_DEPTH: usize = 32;
const MIN_INTERACTIVE_INFLIGHT: usize = 2;
const MIN_QUALITY_INFLIGHT: usize = 8;

/// Build the bounded output-drain policy from the *resolved* preset and its
/// codec-specific capability record.
///
/// **This is a ceiling, not a target.** Both backends drain as soon as
/// `NvEncEncodePicture` reports `NV_ENC_SUCCESS`, so a driver that answers
/// honestly runs one-in, one-out regardless of what this returns. The window
/// only matters for drivers that report `NEED_MORE_INPUT` pessimistically —
/// current synchronous Linux drivers do this even when a blocking
/// `LockBitstream` could complete, which is why the status can be trusted
/// when it says *yes* but not when it says *no*.
///
/// Sizing it still matters, because the window is what those drivers actually
/// wait for. `frameIntervalP - 1` is the preset's explicit B-frame delay,
/// which is now always zero (see `EncodeIntent::REQUIRED_FRAME_INTERVAL_P`).
/// Quality keeps a floor as headroom for the pessimistic case; at 30 fps that
/// floor is worth `(floor - 1) / 30` seconds of latency **only** when the
/// driver never reports success, so it is a fallback cost rather than a
/// standing one.
pub(crate) const fn output_drain_policy(
    intent: EncodeIntent,
    frame_interval_p: i32,
    lookahead_depth: u16,
    // Retained in the signature, deliberately unused. Callers still query
    // `NV_ENC_CAPS_NUM_MAX_BFRAMES` and it remains worth logging, but it must
    // not size the window: the device's B-frame *capability* says nothing
    // about a configuration that forbids B-frames outright.
    _max_bframes_cap: i32,
) -> OutputDrainPolicy {
    let capped_bframes = match intent {
        EncodeIntent::Interactive => 0,
        EncodeIntent::Quality => {
            // Only the configured delay. `frameIntervalP` is now pinned to 1
            // by `EncodeIntent::REQUIRED_FRAME_INTERVAL_P`, so this is always
            // zero — and that is the point.
            //
            // This used to take `max(configured, device capability)`, adding
            // headroom for B-frames the device *could* produce. That made
            // sense while the preset was allowed to request reordering. Now
            // that it cannot, the capability term only reserves pipeline for
            // something the configuration forbids, and on a GPU reporting a
            // large maximum it would push the window past the floor below.
            // Every extra slot is a frame the encoder holds before it emits
            // anything, which a live session pays for on every frame.
            //
            // Defensive headroom for drivers that return NEED_MORE_INPUT
            // anyway now lives entirely in `MIN_QUALITY_INFLIGHT`, which is
            // bounded and hardware-proven rather than device-reported.
            let requested_delay = frame_interval_p.saturating_sub(1);
            if requested_delay > 0 {
                requested_delay as usize
            } else {
                0
            }
        }
    };
    let lookahead = match intent {
        EncodeIntent::Interactive => 0,
        EncodeIntent::Quality => {
            let requested = lookahead_depth as usize;
            if requested < MAX_NVENC_LOOKAHEAD_DEPTH {
                requested
            } else {
                MAX_NVENC_LOOKAHEAD_DEPTH
            }
        }
    };
    let derived = 1 + capped_bframes + lookahead;
    OutputDrainPolicy {
        max_inflight: match intent {
            EncodeIntent::Interactive if derived < MIN_INTERACTIVE_INFLIGHT => {
                MIN_INTERACTIVE_INFLIGHT
            }
            EncodeIntent::Interactive => derived,
            EncodeIntent::Quality if derived < MIN_QUALITY_INFLIGHT => MIN_QUALITY_INFLIGHT,
            EncodeIntent::Quality => derived,
        },
    }
}

pub(crate) const fn vbv_buffer_frames(intent: EncodeIntent) -> f64 {
    match intent {
        EncodeIntent::Interactive => 2.0,
        EncodeIntent::Quality => 8.0,
    }
}

pub(crate) fn rate_control_sizing(
    width: u32,
    height: u32,
    fps: u32,
    chroma: ChromaSubsampling,
    depth: BitDepth,
    intent: EncodeIntent,
) -> RateControlSizing {
    let pixels_per_second = f64::from(width) * f64::from(height) * f64::from(fps.max(1));
    let samples_per_second = pixels_per_second * samples_per_pixel(chroma);
    let bits_per_second = samples_per_second * depth_scale(depth) * BASE_BITS_PER_SAMPLE;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let average_bitrate_bps = bits_per_second.round().clamp(0.0, f64::from(u32::MAX)) as u32;
    let max_bitrate_bps = average_bitrate_bps;
    let vbv_buffer_bits = bits_per_second / f64::from(fps.max(1)) * vbv_buffer_frames(intent);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let vbv_buffer_size_bits = vbv_buffer_bits.round().clamp(0.0, f64::from(u32::MAX)) as u32;
    RateControlSizing {
        average_bitrate_bps,
        max_bitrate_bps,
        vbv_buffer_size_bits,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interactive_keeps_double_buffering() {
        let policy = output_drain_policy(EncodeIntent::Interactive, 4, 32, 3);
        assert_eq!(policy.max_inflight(), 2);
        assert_eq!(policy.slot_count(), 2);
    }

    #[test]
    fn quality_uses_preset_lookahead_and_conservative_driver_headroom() {
        // Four P-interval slots request three B-frames; a two-frame device cap
        // is smaller, so the preset remains authoritative. Sixteen frames of
        // preset lookahead remain part of the queue.
        let policy = output_drain_policy(EncodeIntent::Quality, 4, 16, 2);
        assert_eq!(policy.max_inflight(), 20);
        assert_eq!(policy.slot_count(), 20);
    }

    #[test]
    fn unavailable_bframe_cap_keeps_the_driver_supplied_preset_delay() {
        // FFI capability-query failure is represented as zero, and no longer
        // matters: the window is derived from the configured delay alone, so
        // a failed query cannot change it either way. The floor still applies.
        let policy = output_drain_policy(EncodeIntent::Quality, 4, 0, 0);
        assert_eq!(policy.max_inflight(), 8);
    }

    /// The values the encoders now actually feed this function.
    ///
    /// Both backends pin `frameIntervalP` to 1 and `lookaheadDepth` to 0 (see
    /// `EncodeIntent::REQUIRED_FRAME_INTERVAL_P`), so the only in-flight depth
    /// left is the empirically proven floor. This matters because the encoder
    /// holds `slot_count - 1` frames before it emits anything: every extra
    /// slot is latency a live session pays on every frame, and a stale
    /// lookahead depth of 8 used to buy eight of them for a lookahead that
    /// could not run.
    #[test]
    fn the_zero_reorder_configuration_lands_on_the_proven_floor() {
        // A deliberately large device B-frame capability: it must not inflate
        // the window, because the configuration forbids B-frames outright.
        // Before this was fixed a cap of 12 produced a 13-slot window, and the
        // encoder holds `slot_count - 1` frames before emitting — twelve
        // frames of pure added latency on the grading mode.
        let quality = output_drain_policy(EncodeIntent::Quality, 1, 0, 12);
        assert_eq!(
            quality.max_inflight(),
            MIN_QUALITY_INFLIGHT,
            "quality must not reserve pipeline for reordering it cannot do"
        );

        let interactive = output_drain_policy(EncodeIntent::Interactive, 1, 0, 12);
        assert_eq!(interactive.max_inflight(), MIN_INTERACTIVE_INFLIGHT);
    }

    #[test]
    fn malformed_caps_and_lookahead_stay_bounded() {
        let policy = output_drain_policy(EncodeIntent::Quality, i32::MIN, u16::MAX, -1);
        assert_eq!(policy.max_inflight(), 33);
        assert_eq!(policy.slot_count(), 33);
    }
}
