#[cfg(any(feature = "nvenc", test))]
use core::time::Duration;

#[cfg(any(feature = "nvenc", test))]
use arcen_keel::{EmitMode, IdleCadence};
use arcen_media::BitDepth;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum RequestedEncoder {
    #[default]
    Auto,
    Nvenc,
    SoftwareH264,
}

impl RequestedEncoder {
    pub(crate) fn from_args(args: &[String]) -> Result<Self, String> {
        let mut selected = None;
        for value in args
            .iter()
            .filter_map(|argument| argument.strip_prefix("encoder="))
        {
            if selected.is_some() {
                return Err("encoder may be specified only once".to_string());
            }
            selected = Some(match value.to_ascii_lowercase().as_str() {
                "auto" => Self::Auto,
                "nvenc" => Self::Nvenc,
                "software-h264" => Self::SoftwareH264,
                other => {
                    return Err(format!(
                        "unsupported encoder {other:?}; expected auto|nvenc|software-h264"
                    ));
                }
            });
        }
        Ok(selected.unwrap_or_default())
    }
}

// `requested_variant`/`requested_color` (parsing `variant=<id>` into a
// `ColorSpec`) used to live here, but every real host entry point needs them
// (`win.rs`/`win_mf.rs`/`linux.rs`/`linux_x11.rs`) and this module is
// cfg-gated to `any(target_os = "linux", test)` — invisible to a non-test
// Windows build, which is exactly why no Windows call site could ever use
// them. They now live in `lib.rs` as `crate::requested_variant`/
// `crate::requested_color`, unconditionally compiled.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StartupPath {
    Software,
    NativeProbe,
    Native,
}

pub(crate) const fn startup_path(requested: RequestedEncoder, probe_token: bool) -> StartupPath {
    if matches!(requested, RequestedEncoder::SoftwareH264) {
        StartupPath::Software
    } else if probe_token {
        StartupPath::NativeProbe
    } else {
        StartupPath::Native
    }
}

#[cfg(any(feature = "nvenc", test))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SubmissionMode {
    FirstFrame,
    Idr,
    Activity,
    Keepalive,
    PipelineFlush,
}

#[cfg(any(feature = "nvenc", test))]
impl From<EmitMode> for SubmissionMode {
    fn from(value: EmitMode) -> Self {
        match value {
            EmitMode::FirstFrame => Self::FirstFrame,
            EmitMode::Idr => Self::Idr,
            EmitMode::Activity => Self::Activity,
            EmitMode::Keepalive => Self::Keepalive,
        }
    }
}

/// Idle cadence plus the one-deep CUDA NVENC pipeline flush.
///
/// Every first/activity/IDR submission leaves the newest frame in NVENC while
/// returning the prior slot, so one duplicate submission is required when
/// activity stops. Continuous activity supersedes that flush naturally.
#[cfg(any(feature = "nvenc", test))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SubmissionGate {
    cadence: IdleCadence,
    pipeline_flush_pending: bool,
}

#[cfg(any(feature = "nvenc", test))]
impl SubmissionGate {
    pub(crate) const fn new(keepalive: Duration) -> Self {
        Self {
            cadence: IdleCadence::new(keepalive),
            pipeline_flush_pending: false,
        }
    }

    pub(crate) const fn note_frame(&mut self) {
        self.cadence.note_frame();
    }

    pub(crate) const fn reset(&mut self) {
        self.cadence.reset();
        self.pipeline_flush_pending = false;
    }

    pub(crate) fn decision(
        self,
        idr_pending: bool,
        elapsed_since_emit: Duration,
    ) -> Option<SubmissionMode> {
        self.cadence
            .decision(idr_pending, elapsed_since_emit)
            .map(SubmissionMode::from)
            .or_else(|| {
                self.pipeline_flush_pending
                    .then_some(SubmissionMode::PipelineFlush)
            })
    }

    pub(crate) const fn on_submitted(&mut self, mode: SubmissionMode, output_ready: bool) {
        self.cadence.on_submitted();
        self.pipeline_flush_pending = !output_ready
            || matches!(
                mode,
                SubmissionMode::FirstFrame | SubmissionMode::Idr | SubmissionMode::Activity
            );
    }
}

/// Which Linux capture backend a colour contract needs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LinuxCaptureBackend {
    /// NVIDIA Frame Buffer Capture straight into CUDA memory. The fast path,
    /// and eight bits per channel forever.
    NvFbc,
    /// X11 MIT-SHM out of a depth-30 root window, into host memory. The only
    /// way to read more than eight bits per channel on Linux.
    XShm,
}

/// Choose the capture backend for a colour contract.
///
/// **Keyed on bit depth, not on transfer.** This differs deliberately from
/// the Windows rule, and the reason is that the two platforms are answering
/// different questions.
///
/// On Windows the question is "is this HDR", because Advanced Color decides
/// whether the desktop is composited in wide-gamut scRGB at all, and only a
/// PQ transfer means HDR. On Linux there is no such switch: X11 has one
/// framebuffer at one depth, and the only question that matters is how many
/// bits a capture can read out of it. Ten-bit BT.709 -- Grading Reference,
/// an entirely SDR contract -- needs a wide capture just as much as HDR10
/// does, because its whole purpose is the extra banding headroom. Keying
/// this on `transfer` would hand Grading Reference an eight-bit capture and
/// quietly deliver exactly the thing it exists to avoid.
///
/// NvFBC stays the default for eight-bit because it is genuinely faster:
/// frames land in CUDA memory and NVENC reads them without crossing PCIe.
/// The XShm path gives that up -- host memory, a CPU conversion, and a copy
/// back to the device -- so it is taken only when the extra bits are
/// actually being asked for.
pub(crate) const fn linux_capture_backend(bit_depth: BitDepth) -> LinuxCaptureBackend {
    match bit_depth {
        BitDepth::Eight => LinuxCaptureBackend::NvFbc,
        BitDepth::Ten | BitDepth::Twelve => LinuxCaptureBackend::XShm,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEEPALIVE: Duration = Duration::from_secs(1);

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn encoder_selection_defaults_and_parses_explicit_modes() {
        assert_eq!(
            RequestedEncoder::from_args(&args(&["0", "h265"])).unwrap(),
            RequestedEncoder::Auto
        );
        assert_eq!(
            RequestedEncoder::from_args(&args(&["encoder=nvenc"])).unwrap(),
            RequestedEncoder::Nvenc
        );
        assert_eq!(
            RequestedEncoder::from_args(&args(&["encoder=software-h264"])).unwrap(),
            RequestedEncoder::SoftwareH264
        );
        assert!(RequestedEncoder::from_args(&args(&["encoder=unknown"])).is_err());
        assert!(RequestedEncoder::from_args(&args(&["encoder=auto", "encoder=nvenc"])).is_err());
        assert_eq!(
            startup_path(RequestedEncoder::SoftwareH264, true),
            StartupPath::Software,
            "an explicit software request must win over every native probe token"
        );
    }

    // `variant_selection_drives_the_whole_colour_contract` and
    // `unknown_or_repeated_variants_fail_rather_than_defaulting` moved to
    // `lib.rs`'s test module along with `requested_variant`/`requested_color`.

    fn primed_gate() -> SubmissionGate {
        let mut gate = SubmissionGate::new(KEEPALIVE);
        gate.note_frame();
        assert_eq!(
            gate.decision(false, Duration::ZERO),
            Some(SubmissionMode::FirstFrame)
        );
        gate.on_submitted(SubmissionMode::FirstFrame, false);
        assert_eq!(
            gate.decision(false, Duration::ZERO),
            Some(SubmissionMode::PipelineFlush)
        );
        gate.on_submitted(SubmissionMode::PipelineFlush, true);
        gate
    }

    #[test]
    fn no_frame_or_early_idle_tick_does_not_submit() {
        let gate = SubmissionGate::new(KEEPALIVE);
        assert_eq!(gate.decision(true, KEEPALIVE), None);

        let gate = primed_gate();
        assert_eq!(gate.decision(false, Duration::from_millis(999)), None);
    }

    #[test]
    fn activity_and_idr_submit_on_the_next_tick_then_flush_once() {
        let mut gate = primed_gate();
        gate.note_frame();
        assert_eq!(
            gate.decision(false, Duration::ZERO),
            Some(SubmissionMode::Activity)
        );
        gate.on_submitted(SubmissionMode::Activity, true);
        assert_eq!(
            gate.decision(false, Duration::ZERO),
            Some(SubmissionMode::PipelineFlush)
        );
        gate.on_submitted(SubmissionMode::PipelineFlush, true);

        assert_eq!(
            gate.decision(true, Duration::ZERO),
            Some(SubmissionMode::Idr)
        );
        gate.on_submitted(SubmissionMode::Idr, true);
        assert_eq!(
            gate.decision(false, Duration::ZERO),
            Some(SubmissionMode::PipelineFlush)
        );
    }

    #[test]
    fn continuous_activity_supersedes_pending_flush_and_keepalive_is_single() {
        let mut gate = primed_gate();
        gate.note_frame();
        gate.on_submitted(SubmissionMode::Activity, true);
        gate.note_frame();
        assert_eq!(
            gate.decision(false, Duration::ZERO),
            Some(SubmissionMode::Activity)
        );
        gate.on_submitted(SubmissionMode::Activity, true);
        assert_eq!(
            gate.decision(false, Duration::ZERO),
            Some(SubmissionMode::PipelineFlush)
        );
        gate.on_submitted(SubmissionMode::PipelineFlush, true);

        assert_eq!(
            gate.decision(false, KEEPALIVE),
            Some(SubmissionMode::Keepalive)
        );
        gate.on_submitted(SubmissionMode::Keepalive, true);
        assert_eq!(gate.decision(false, Duration::ZERO), None);
    }

    #[test]
    fn capture_recreate_discards_retained_frame_and_pending_flush() {
        let mut gate = primed_gate();
        gate.note_frame();
        gate.on_submitted(SubmissionMode::Activity, true);
        gate.reset();
        assert_eq!(gate.decision(true, KEEPALIVE), None);

        gate.note_frame();
        assert_eq!(
            gate.decision(false, Duration::ZERO),
            Some(SubmissionMode::FirstFrame)
        );
    }

    /// Eight-bit keeps the zero-copy NvFBC path.
    #[test]
    fn eight_bit_stays_on_nvfbc() {
        assert_eq!(
            linux_capture_backend(BitDepth::Eight),
            LinuxCaptureBackend::NvFbc
        );
    }

    /// Ten-bit needs XShm regardless of transfer.
    ///
    /// This is the regression guard for keying the choice on `transfer`
    /// instead of depth. Grading Reference is ten-bit BT.709 and entirely
    /// SDR; a transfer-keyed rule would give it NvFBC, which cannot read
    /// more than eight bits, and the extra headroom it exists for would
    /// silently not be there.
    #[test]
    fn every_depth_above_eight_needs_the_wide_capture() {
        for depth in [BitDepth::Ten, BitDepth::Twelve] {
            assert_eq!(
                linux_capture_backend(depth),
                LinuxCaptureBackend::XShm,
                "{depth:?} cannot be served by NvFBC"
            );
        }
    }
}
