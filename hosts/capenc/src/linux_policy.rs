use std::time::Duration;

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
pub(crate) use arcen_keel::{SubmissionGate, SubmissionMode};

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

/// Which CUDA device capture opens.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum CudaDeviceRequest {
    /// The GPU at this PCI address, in the `0000:00:10.0` form that
    /// `cuDeviceGetByPCIBusId` accepts. Set when the Pier knows which GPU
    /// drives the X screen, so NvFBC, CUDA and NVENC share it.
    PciBusId(String),
    /// A CUDA ordinal: the operator's `ARCEN_CUDA_DEVICE`, otherwise 0.
    Ordinal(i32),
}

/// Resolves the capture GPU from `ARCEN_CAPTURE_GPU_PCI` and
/// `ARCEN_CUDA_DEVICE` as read from the environment (`None` when unset).
///
/// A PCI address wins: it is the X server's own GPU, which an ordinal cannot
/// name reliably on a host whose GPUs differ. Malformed values are refused
/// rather than silently replaced by device 0.
pub(crate) fn cuda_device_request(
    capture_gpu_pci: Option<&str>,
    ordinal: Option<&str>,
) -> Result<CudaDeviceRequest, String> {
    if let Some(pci) = capture_gpu_pci {
        let address = arcen_outputs::PciAddress::parse_sysfs(pci.trim()).map_err(|error| {
            format!(
                "{} must be a PCI address like 0000:00:10.0: {error}",
                crate::CAPTURE_GPU_PCI_ENV
            )
        })?;
        return Ok(CudaDeviceRequest::PciBusId(address.to_string()));
    }
    match ordinal {
        Some(value) => value
            .parse::<i32>()
            .map(CudaDeviceRequest::Ordinal)
            .map_err(|_| "ARCEN_CUDA_DEVICE must be a signed integer".to_string()),
        None => Ok(CudaDeviceRequest::Ordinal(0)),
    }
}

/// One second of a damage-gated capture loop's pacing, for the "why this
/// fps" verdict on its stats line.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct PacingWindow {
    due: u32,
    without_change: u32,
    fresh: u32,
}

impl PacingWindow {
    /// Counts one due deadline. Only first, IDR and activity submissions carry
    /// new content; a keepalive or pipeline flush re-encodes what is already
    /// on screen.
    pub(crate) fn record_due(&mut self, mode: Option<SubmissionMode>) {
        self.due = self.due.saturating_add(1);
        match mode {
            Some(SubmissionMode::FirstFrame | SubmissionMode::Idr | SubmissionMode::Activity) => {
                self.fresh = self.fresh.saturating_add(1);
            }
            Some(SubmissionMode::Keepalive | SubmissionMode::PipelineFlush) | None => {
                self.without_change = self.without_change.saturating_add(1);
            }
        }
    }

    /// A due deadline counted fresh turned out to carry no new pixels (a
    /// compositor repaint the frame comparison found identical).
    pub(crate) fn note_unchanged_content(&mut self) {
        if self.fresh > 0 {
            self.fresh -= 1;
            self.without_change = self.without_change.saturating_add(1);
        }
    }

    pub(crate) fn verdict_fields(
        self,
        contract_fps: u32,
        target_fps: u32,
        busy: Duration,
        elapsed: Duration,
    ) -> String {
        let verdict =
            arcen_media::video::classify_frame_rate(arcen_media::video::FrameRateWindow {
                contract_fps,
                target_fps,
                ticks_due: self.due,
                ticks_without_change: self.without_change,
                frames_submitted: self.fresh,
                busy,
                elapsed,
            });
        format!(
            " fps_limiter={} rate_capped={} contract_fps={contract_fps} target_fps={target_fps} \
             ticks_due={} ticks_no_change={} fresh_frames={} busy_ms={}",
            verdict.limiter.token(),
            verdict.rate_capped,
            self.due,
            self.without_change,
            self.fresh,
            busy.as_millis(),
        )
    }
}

/// Whether the Xorg depth-30 loop may read only what XDamage reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WideRegionOptions {
    /// `wide-regions=off` reads every picture whole.
    pub(crate) enabled: bool,
    /// `wide-region-verify=N`: every Nth region picture is compared with a
    /// whole capture taken right after it (lab evidence; costs a full read).
    pub(crate) verify_every: Option<std::num::NonZeroU32>,
}

/// Parses `wide-regions=on|off` (default on) and `wide-region-verify=N`.
pub(crate) fn wide_region_options(args: &[String]) -> Result<WideRegionOptions, String> {
    let mut options = WideRegionOptions {
        enabled: true,
        verify_every: None,
    };
    for arg in args {
        if let Some(value) = arg.strip_prefix("wide-regions=") {
            options.enabled = match value {
                "on" => true,
                "off" => false,
                other => return Err(format!("invalid wide-regions={other:?}; use on or off")),
            };
        } else if let Some(value) = arg.strip_prefix("wide-region-verify=") {
            options.verify_every = Some(value.parse().map_err(|_| {
                format!("invalid wide-region-verify={value:?}; use a count above zero")
            })?);
        }
    }
    Ok(options)
}

/// How long region pictures may follow one another before a whole frame is
/// read again, bounding what a missed damage report could leave stale.
pub(crate) const WIDE_FULL_REFRESH: Duration = Duration::from_secs(2);

/// Whether the depth-30 loop may read regions now: the setting and the
/// encoder's GPU master frame allowed it at start (`ready`), and the current
/// capture reads through MIT-SHM. Re-evaluated after every recreation, since
/// a recreated capture may have fallen back to `XGetImage`.
pub(crate) const fn wide_regions_engaged(ready: bool, capture_supports_regions: bool) -> bool {
    ready && capture_supports_regions
}

/// Whole pictures staged without comparing after a comparison found more
/// than half the frame changed (half a second at 30 fps).
pub(crate) const WIDE_DIFF_BACKOFF_PICTURES: u32 = 15;

/// What one picture of the Xorg depth-30 loop reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WideCapturePlan {
    /// Read, convert and upload the whole frame: a keyframe, keepalive or
    /// refresh, or region staging is off.
    Full,
    /// Read the whole frame, because the damage report covers most of it or
    /// cannot be trusted; what is converted and uploaded may still be bounded
    /// by comparing it with the last whole frame.
    Whole,
    /// Read, convert and upload only this rectangle.
    Region(arcen_media::video::FrameRegion),
    /// Nothing new: re-send the retained GPU frame.
    Restage,
}

/// Chooses what a picture reads. Keyframes, keepalives, refreshes and any
/// damage the source cannot vouch for read the whole frame.
pub(crate) fn wide_capture_plan(
    regions_engaged: bool,
    mode: SubmissionMode,
    idr_pending: bool,
    damage_pending: bool,
    refresh_due: bool,
    reported: arcen_media::video::CaptureRegion,
) -> WideCapturePlan {
    if !regions_engaged
        || idr_pending
        || refresh_due
        || matches!(
            mode,
            SubmissionMode::FirstFrame | SubmissionMode::Idr | SubmissionMode::Keepalive
        )
    {
        return WideCapturePlan::Full;
    }
    if mode == SubmissionMode::PipelineFlush && !damage_pending {
        return WideCapturePlan::Restage;
    }
    match reported {
        arcen_media::video::CaptureRegion::Rect(region) => WideCapturePlan::Region(region),
        arcen_media::video::CaptureRegion::Full => WideCapturePlan::Whole,
    }
}

/// One stats window of the Xorg depth-30 loop's reads.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct RegionWindow {
    pub(crate) full: u32,
    pub(crate) region: u32,
    pub(crate) restaged: u32,
    /// Sum of each region's share of the frame, in basis points.
    pub(crate) area_bps: u64,
    pub(crate) verified: u32,
    pub(crate) verify_skipped: u32,
    pub(crate) mismatched_samples: usize,
    /// Whole reads whose difference from the last whole frame was bounded.
    pub(crate) diffed: u32,
    /// Whole reads identical to the last whole frame.
    pub(crate) unchanged: u32,
}

impl RegionWindow {
    pub(crate) fn note_region(
        &mut self,
        region: arcen_media::video::FrameRegion,
        width: u32,
        height: u32,
    ) {
        self.region = self.region.saturating_add(1);
        self.note_area(region, width, height);
    }

    pub(crate) fn note_diffed(
        &mut self,
        region: arcen_media::video::FrameRegion,
        width: u32,
        height: u32,
    ) {
        self.diffed = self.diffed.saturating_add(1);
        self.note_area(region, width, height);
    }

    fn note_area(&mut self, region: arcen_media::video::FrameRegion, width: u32, height: u32) {
        let frame = u64::from(width).saturating_mul(u64::from(height)).max(1);
        let area = (region.width as u64).saturating_mul(region.height as u64);
        self.area_bps = self
            .area_bps
            .saturating_add(area.saturating_mul(10_000) / frame);
    }

    pub(crate) fn fields(&self) -> String {
        let bounded = u64::from(self.region) + u64::from(self.diffed);
        let mean_area_bps = if bounded == 0 {
            0
        } else {
            self.area_bps / bounded
        };
        format!(
            " full_reads={} region_reads={} diffed_reads={} unchanged_reads={} restaged={} \
             mean_region_area_bps={mean_area_bps} region_verified={} region_verify_skipped={} \
             region_mismatched_samples={}",
            self.full,
            self.region,
            self.diffed,
            self.unchanged,
            self.restaged,
            self.verified,
            self.verify_skipped,
            self.mismatched_samples,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn region() -> arcen_media::video::FrameRegion {
        arcen_media::video::FrameRegion {
            x: 100,
            y: 40,
            width: 64,
            height: 32,
        }
    }

    #[test]
    fn region_options_default_on_and_parse_verification() {
        assert_eq!(
            wide_region_options(&args(&["0", "h265"])).unwrap(),
            WideRegionOptions {
                enabled: true,
                verify_every: None,
            }
        );
        let parsed =
            wide_region_options(&args(&["wide-regions=off", "wide-region-verify=30"])).unwrap();
        assert!(!parsed.enabled);
        assert_eq!(parsed.verify_every.map(std::num::NonZeroU32::get), Some(30));
        assert!(wide_region_options(&args(&["wide-regions=maybe"])).is_err());
        assert!(wide_region_options(&args(&["wide-region-verify=0"])).is_err());
    }

    #[test]
    fn keyframes_keepalives_refreshes_and_unvouched_damage_read_whole_frames() {
        use arcen_media::video::CaptureRegion;
        let rect = CaptureRegion::Rect(region());
        let plan = |engaged, mode, idr, pending, refresh, reported| {
            wide_capture_plan(engaged, mode, idr, pending, refresh, reported)
        };
        assert_eq!(
            plan(true, SubmissionMode::Activity, false, true, false, rect),
            WideCapturePlan::Region(region())
        );
        for (engaged, mode, idr, refresh, reported) in [
            (false, SubmissionMode::Activity, false, false, rect),
            (true, SubmissionMode::FirstFrame, false, false, rect),
            (true, SubmissionMode::Idr, false, false, rect),
            (true, SubmissionMode::Keepalive, false, false, rect),
            (true, SubmissionMode::Activity, true, false, rect),
            (true, SubmissionMode::Activity, false, true, rect),
        ] {
            assert_eq!(
                plan(engaged, mode, idr, true, refresh, reported),
                WideCapturePlan::Full,
                "{engaged} {mode:?} {idr} {refresh} {reported:?}"
            );
        }
        assert_eq!(
            plan(
                true,
                SubmissionMode::Activity,
                false,
                true,
                false,
                CaptureRegion::Full
            ),
            WideCapturePlan::Whole
        );
        assert_eq!(
            plan(
                true,
                SubmissionMode::PipelineFlush,
                false,
                false,
                false,
                CaptureRegion::Full
            ),
            WideCapturePlan::Restage
        );
        assert_eq!(
            plan(
                true,
                SubmissionMode::PipelineFlush,
                false,
                true,
                false,
                rect
            ),
            WideCapturePlan::Region(region())
        );
    }

    /// A capture that fell back to `XGetImage` (no MIT-SHM 1.2) must never
    /// be planned a region read: every picture is whole, so the loop never
    /// asks the capture for something it cannot do.
    #[test]
    fn a_capture_without_mit_shm_plans_only_whole_reads() {
        use arcen_media::video::CaptureRegion;
        assert!(!wide_regions_engaged(true, false));
        assert!(!wide_regions_engaged(false, true));
        assert!(wide_regions_engaged(true, true));
        let engaged = wide_regions_engaged(true, false);
        for mode in [
            SubmissionMode::Activity,
            SubmissionMode::PipelineFlush,
            SubmissionMode::Keepalive,
        ] {
            for pending in [false, true] {
                assert_eq!(
                    wide_capture_plan(
                        engaged,
                        mode,
                        false,
                        pending,
                        false,
                        CaptureRegion::Rect(region())
                    ),
                    WideCapturePlan::Full,
                    "{mode:?} pending={pending}"
                );
            }
        }
    }

    #[test]
    fn region_window_reports_reads_and_area() {
        let mut window = RegionWindow {
            full: 1,
            restaged: 2,
            ..RegionWindow::default()
        };
        window.note_region(region(), 2560, 1440);
        let fields = window.fields();
        assert!(fields.contains(
            " full_reads=1 region_reads=1 diffed_reads=0 unchanged_reads=0 restaged=2 \
             mean_region_area_bps=5 "
        ));
        assert!(fields.ends_with(" region_mismatched_samples=0"));
    }

    #[test]
    fn pacing_counts_only_new_content_as_fresh() {
        let mut pacing = PacingWindow::default();
        for mode in [
            Some(SubmissionMode::FirstFrame),
            Some(SubmissionMode::Activity),
            Some(SubmissionMode::PipelineFlush),
            Some(SubmissionMode::Keepalive),
            None,
            Some(SubmissionMode::Idr),
        ] {
            pacing.record_due(mode);
        }
        assert_eq!(
            pacing,
            PacingWindow {
                due: 6,
                without_change: 3,
                fresh: 3,
            }
        );
    }

    #[test]
    fn identical_repaints_count_as_no_change() {
        let mut pacing = PacingWindow::default();
        pacing.record_due(Some(SubmissionMode::Activity));
        pacing.record_due(Some(SubmissionMode::Activity));
        pacing.note_unchanged_content();
        assert_eq!(
            pacing,
            PacingWindow {
                due: 2,
                without_change: 1,
                fresh: 1,
            }
        );
    }

    #[test]
    fn pacing_fields_name_the_limiter_and_the_counts() {
        let mut pacing = PacingWindow::default();
        for tick in 0..30 {
            pacing.record_due((tick % 3 == 0).then_some(SubmissionMode::Activity));
        }
        let fields =
            pacing.verdict_fields(30, 30, Duration::from_millis(170), Duration::from_secs(1));
        assert!(fields.starts_with(" fps_limiter=no_damage rate_capped=false"));
        assert!(fields.contains(" target_fps=30 ticks_due=30 ticks_no_change=20 fresh_frames=10"));
        assert!(fields.ends_with(" busy_ms=170"));
    }

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

    #[test]
    fn capture_gpu_defaults_to_cuda_ordinal_zero() {
        assert_eq!(
            cuda_device_request(None, None),
            Ok(CudaDeviceRequest::Ordinal(0))
        );
        assert_eq!(
            cuda_device_request(None, Some("1")),
            Ok(CudaDeviceRequest::Ordinal(1))
        );
        assert!(cuda_device_request(None, Some("first")).is_err());
    }

    #[test]
    fn the_x_servers_pci_gpu_wins_over_an_ordinal() {
        assert_eq!(
            cuda_device_request(Some("0000:00:10.0"), Some("1")),
            Ok(CudaDeviceRequest::PciBusId("0000:00:10.0".to_string()))
        );
        assert_eq!(
            cuda_device_request(Some("3b:00.0\n"), None),
            Ok(CudaDeviceRequest::PciBusId("0000:3b:00.0".to_string()))
        );
        for bad in ["", "PCI:0:16:0", "0000:00:10", "0000:00:10.0;rm"] {
            let error = cuda_device_request(Some(bad), None).expect_err(bad);
            assert!(error.starts_with("ARCEN_CAPTURE_GPU_PCI"), "{error}");
        }
    }
}
