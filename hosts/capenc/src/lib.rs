// arcen-capenc — fused capture+encode helper.
//
// One native process per host that captures the desktop and encodes it on
// the SAME GPU with zero host pixel copies, writing Annex-B H.264/HEVC
// access units to stdout for the Python server (server/capenc_backend.py).
// stdin carries one-word commands ("IDR" = force keyframe); stderr carries
// diagnostics + a 1 Hz stats line. The wire/CLI contract is identical on
// every platform:
//
//   arcen-capenc <output_index> <codec> [fps] [yuv444] [framed-v1]
//                      [selftest [WxH]]
//
// stdout defaults to raw Annex-B for the deployed Python host. `framed-v1`
// switches stdout to repeated `u32_be payload_len || Annex-B AU` records for
// native hosts; pipe reads never need to guess where a child write ended.
//
// Platform backends:
//   Windows: DXGI Desktop Duplication -> NVENC via D3D11 (win.rs, nvenc.rs)
//   Linux:   NvFBC SHARED_CUDA        -> NVENC via CUDA  (linux.rs, nvenc_cuda.rs)
//
// NVIDIA entry points are loaded at runtime (LoadLibrary / dlopen) with
// vendored struct definitions — no NVENC SDK, no CUDA toolkit, no NVIDIA
// build-time dependencies at all.

pub mod admission_probe;

// Vendored NVENC bindings (bindgen output, patched to fixed-width LONG/GUID
// so the layout is correct on both windows-x64 and linux-x64).
#[cfg(feature = "nvenc")]
#[allow(warnings)]
mod nvenc_sys;

// `capenc probe-matrix`: pure report-shaping/aggregation logic, deliberately
// free of any `windows`/`nvenc_sys` type so it compiles and is unit-testable
// in every feature/OS combination this crate builds (see the module doc).
// The real per-backend trial calls it is fed live in `win.rs`.
mod probe_matrix;

#[cfg(any(windows, test))]
mod frame_policy;
#[cfg(all(feature = "nvenc", windows))]
mod nvenc;
mod nvenc_policy;
/// Keel damage to NVENC QP-map translation. Built on any target so the
/// geometry and the Keel block-size invariant stay under test everywhere,
/// not only where NVENC happens to compile.
mod qp_map;
#[cfg(all(
    any(feature = "nvenc", feature = "mf", feature = "software-h264"),
    windows
))]
mod wgc;
#[cfg(windows)]
mod win;

#[cfg(all(feature = "mf", windows))]
mod annexb;
#[cfg(all(feature = "mf", windows))]
mod mf_encoder;
#[cfg(all(any(feature = "mf", feature = "software-h264"), windows))]
mod win_mf;

#[cfg(all(feature = "nvenc", target_os = "linux"))]
mod linux;
#[cfg(any(target_os = "linux", test))]
mod linux_policy;
#[cfg(all(feature = "software-h264", target_os = "linux"))]
mod linux_x11;
#[cfg(all(feature = "nvenc", target_os = "linux"))]
mod nvenc_cuda;
#[cfg(any(
    test,
    all(windows, any(feature = "mf", feature = "software-h264")),
    all(target_os = "linux", feature = "software-h264")
))]
// Host-neutral binding shared by the Linux X11 and Windows MF capture loops.
// Neither platform consumes the whole surface (Linux needs the degraded
// fallback and re-arm, Windows needs the hash kernel and bypass accounting), so
// per-platform builds legitimately leave part of it unused.
#[allow(dead_code)]
mod region_schedule;

#[cfg(any(
    test,
    windows,
    all(target_os = "linux", any(feature = "nvenc", feature = "software-h264"))
))]
use std::io::BufRead;
use std::io::Write;
#[cfg(any(
    test,
    windows,
    all(target_os = "linux", any(feature = "nvenc", feature = "software-h264"))
))]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(any(
    windows,
    all(target_os = "linux", any(feature = "nvenc", feature = "software-h264"))
))]
use std::sync::Arc;
use std::sync::OnceLock;
#[cfg(any(
    windows,
    all(target_os = "linux", any(feature = "nvenc", feature = "software-h264"))
))]
use std::time::Duration;

#[cfg(any(
    windows,
    all(target_os = "linux", any(feature = "nvenc", feature = "software-h264"))
))]
use arcen_media::video::format_ready_v1;
#[cfg(any(test, windows, target_os = "linux"))]
use arcen_media::video::EncoderBackend;
#[cfg(any(windows, target_os = "linux"))]
use arcen_media::video::{
    format_unavailable_v1, BackendUnavailableNotice, BackendUnavailableReason,
};
#[cfg(any(
    test,
    windows,
    all(target_os = "linux", any(feature = "nvenc", feature = "software-h264"))
))]
use arcen_media::video::{
    resolve_media_plan, BackendAvailability, BackendCandidate, BackendLimits, EncoderRequest,
    MediaRequest, ResolvedMediaPlan,
};
#[cfg(any(test, windows, target_os = "linux"))]
use arcen_media::EncodeIntent;
#[cfg(any(
    test,
    windows,
    all(target_os = "linux", any(feature = "nvenc", feature = "software-h264"))
))]
use arcen_media::{VideoCodec, VideoConfiguration};
#[cfg(any(
    test,
    windows,
    all(target_os = "linux", any(feature = "nvenc", feature = "software-h264"))
))]
use arcen_protocol::messages::CursorMode;
// The colour vocabulary is unconditional: `ColorSpec` is part of capenc's
// public surface, so it must exist on every platform and feature combination
// even where no encoder is compiled in.
use arcen_media::video::{ColorTransform, VideoVariant};
use arcen_media::{
    BitDepth, ChromaSubsampling, ColorMatrix, ColorPrimaries, ColorRange, TransferCharacteristics,
};
use arcen_telemetry::CorrelationId;

#[cfg(any(
    test,
    windows,
    all(target_os = "linux", any(feature = "nvenc", feature = "software-h264"))
))]
pub(crate) const FRAMED_OUTPUT_V1: &str = "framed-v1";
#[cfg(any(
    test,
    windows,
    all(target_os = "linux", any(feature = "nvenc", feature = "software-h264"))
))]
pub(crate) const MAX_ACCESS_UNIT_BYTES: usize = 16 * 1024 * 1024;

#[cfg(any(
    test,
    windows,
    all(target_os = "linux", any(feature = "nvenc", feature = "software-h264"))
))]
pub(crate) struct ControlState {
    want_idr: AtomicBool,
    input_activity: AtomicBool,
    stop: AtomicBool,
}

#[cfg(any(
    test,
    windows,
    all(target_os = "linux", any(feature = "nvenc", feature = "software-h264"))
))]
impl ControlState {
    fn new() -> Self {
        Self {
            want_idr: AtomicBool::new(false),
            input_activity: AtomicBool::new(false),
            stop: AtomicBool::new(false),
        }
    }

    /// Read the pending IDR flag without clearing it.
    ///
    /// Narrower than the rest of this impl on purpose. The Linux caller lives
    /// in `linux.rs`, which is `#[cfg(all(feature = "nvenc", ...))]`; the
    /// software-h264 X11 path only ever calls `take_idr`. Gating this with the
    /// others made it dead code in a Linux build with `software-h264` but not
    /// `nvenc` — a configuration CI builds and a macOS workstation cannot,
    /// because off Linux the whole impl only exists under `test`, where this is
    /// used.
    #[cfg(any(test, windows, all(target_os = "linux", feature = "nvenc")))]
    pub(crate) fn idr_pending(&self) -> bool {
        self.want_idr.load(Ordering::Acquire)
    }

    pub(crate) fn take_idr(&self) -> bool {
        self.want_idr.swap(false, Ordering::AcqRel)
    }

    /// Consumes a pending input/focus wake for this region.
    ///
    /// A wake keeps a region responsive across the activity scheduler's
    /// static-content suppression. It never replaces a keyframe, a refresh
    /// deadline, or any encoder admission decision.
    pub(crate) fn take_input_activity(&self) -> bool {
        self.input_activity.swap(false, Ordering::AcqRel)
    }

    pub(crate) fn stop_requested(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }
}

#[cfg(any(
    test,
    windows,
    all(target_os = "linux", any(feature = "nvenc", feature = "software-h264"))
))]
fn read_control<R: BufRead>(reader: R, state: &ControlState, label: &str) {
    for line in reader.lines() {
        match line {
            Ok(command) if command.trim().eq_ignore_ascii_case("idr") => {
                state.want_idr.store(true, Ordering::Release);
            }
            // Input/focus activity for this region. Advisory: it wakes a
            // suppressed static region early and never suppresses, delays, or
            // downgrades a keyframe, refresh deadline, or admitted frame.
            Ok(command) if command.trim().eq_ignore_ascii_case("wake") => {
                state.input_activity.store(true, Ordering::Release);
            }
            Ok(command) if command.trim().eq_ignore_ascii_case("stop") => break,
            Ok(command) => log(&format!("{label}: unknown stdin command: {command:?}")),
            Err(error) => {
                log(&format!("{label}: stdin read error: {error}"));
                break;
            }
        }
    }
    state.stop.store(true, Ordering::Release);
}

#[cfg(any(
    windows,
    all(target_os = "linux", any(feature = "nvenc", feature = "software-h264"))
))]
pub(crate) fn spawn_control_thread(label: &'static str) -> Arc<ControlState> {
    let state = Arc::new(ControlState::new());
    let reader_state = Arc::clone(&state);
    let _ = std::thread::Builder::new()
        .name("arcen-capenc-control".to_string())
        .spawn(move || read_control(std::io::stdin().lock(), &reader_state, label));
    state
}

#[cfg(any(
    test,
    windows,
    all(target_os = "linux", any(feature = "nvenc", feature = "software-h264"))
))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CursorCaptureMode {
    Local,
    Host,
}

#[cfg(any(
    test,
    windows,
    all(target_os = "linux", any(feature = "nvenc", feature = "software-h264"))
))]
impl CursorCaptureMode {
    pub(crate) const fn include_cursor(self) -> bool {
        matches!(self, Self::Host)
    }

    #[allow(dead_code)]
    pub(crate) const fn requires_wgc(self) -> bool {
        matches!(self, Self::Host)
    }
}

#[cfg(any(
    test,
    windows,
    all(target_os = "linux", any(feature = "nvenc", feature = "software-h264"))
))]
pub(crate) fn cursor_mode_from_args(args: &[String]) -> Result<CursorCaptureMode, &'static str> {
    match args
        .iter()
        .find_map(|argument| argument.strip_prefix("cursor="))
        .unwrap_or("local")
    {
        "local" => Ok(CursorCaptureMode::Local),
        "host" => Ok(CursorCaptureMode::Host),
        _ => Err("cursor must be local or host"),
    }
}

static SESSION_LOG_ID: OnceLock<Option<CorrelationId>> = OnceLock::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompiledBackendFeatures {
    pub nvenc: bool,
    pub mf: bool,
    pub software_h264: bool,
    /// Whether the `rav1e` software AV1 backend (`EncoderBackend::Rav1e`) is
    /// compiled in. Always `false` today: unlike `software_h264`, capenc's
    /// `Cargo.toml` has no `software-av1` feature yet to forward
    /// `arcen-media/software-av1-source` -- see `probe_matrix.rs`'s
    /// `attempt_rav1e_for_row` doc for exactly what that feature needs to
    /// say and why it cannot be added from this file. This field exists now,
    /// alongside the other three, so the day that feature is added this
    /// struct needs no further change to report it.
    pub software_av1: bool,
}

#[must_use]
pub const fn compiled_backend_features() -> CompiledBackendFeatures {
    CompiledBackendFeatures {
        nvenc: cfg!(feature = "nvenc"),
        mf: cfg!(feature = "mf"),
        software_h264: cfg!(feature = "software-h264"),
        software_av1: cfg!(feature = "software-av1"),
    }
}

fn format_log_line(msg: &str, session_log_id: Option<&CorrelationId>) -> String {
    session_log_id.map_or_else(
        || format!("[capenc] {msg}"),
        |id| format!("[capenc] {msg} sid={id}"),
    )
}

pub fn log(msg: &str) {
    let session_log_id = SESSION_LOG_ID.get().and_then(Option::as_ref);
    let _ = writeln!(
        std::io::stderr(),
        "{}",
        format_log_line(msg, session_log_id)
    );
}

/// The colour half of a capenc request, separate from codec and geometry.
///
/// Bundled rather than passed as five more positional arguments because the
/// call sites already carry a codec string, a size and a cursor mode, and a
/// bare `bool` for chroma was exactly the kind of argument that gets passed in
/// the wrong position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColorSpec {
    pub chroma: ChromaSubsampling,
    pub bit_depth: BitDepth,
    pub range: ColorRange,
    pub matrix: ColorMatrix,
    pub primaries: ColorPrimaries,
    pub transfer: TransferCharacteristics,
}

impl ColorSpec {
    /// The contract capenc produced before colour was negotiable: 8-bit,
    /// BT.709, limited range, with chroma chosen by the old `yuv444` flag.
    #[must_use]
    pub const fn legacy(yuv444: bool) -> Self {
        Self {
            chroma: if yuv444 {
                ChromaSubsampling::Yuv444
            } else {
                ChromaSubsampling::Yuv420
            },
            bit_depth: BitDepth::Eight,
            range: ColorRange::Limited,
            matrix: ColorMatrix::Bt709,
            primaries: ColorPrimaries::Bt709,
            transfer: TransferCharacteristics::Bt709,
        }
    }

    /// The colour half of a probe-matrix variant.
    #[must_use]
    pub const fn from_variant(variant: VideoVariant) -> Self {
        Self {
            chroma: variant.video.chroma,
            bit_depth: variant.video.bit_depth,
            range: variant.video.range,
            matrix: variant.video.matrix,
            primaries: variant.video.primaries,
            transfer: variant.video.transfer,
        }
    }

    /// The conversion this colour spec implies.
    #[must_use]
    pub fn transform(self) -> ColorTransform {
        ColorTransform::new(self.matrix, self.range, self.bit_depth)
    }
}

impl Default for ColorSpec {
    fn default() -> Self {
        Self::legacy(false)
    }
}

/// Parse an optional `variant=<id>` argument into a colour spec.
///
/// This is how the probe matrix is driven: one stable id selects codec, chroma,
/// depth, range and matrix together, so a row recorded in the findings file
/// names exactly the format that was attempted. Absent, the legacy 8-bit
/// limited contract applies, with chroma still coming from the positional
/// `yuv444` token so existing invocations keep working unchanged.
///
/// Lives here, unconditionally, rather than in `linux_policy.rs` (where it
/// used to live): every real host entry point needs it —
/// `win.rs`/`win_mf.rs` on Windows, `linux.rs`/`linux_x11.rs` on Linux — and
/// `linux_policy` is `#[cfg(any(target_os = "linux", test))]`, invisible to
/// a non-test Windows build. That gap is exactly why no Windows call site
/// ever wired this in: the function they would have called did not exist
/// there.
///
/// # Errors
///
/// Rejects a repeated argument and any id the shared vocabulary does not
/// recognise, rather than falling back to a default that would silently
/// mislabel the resulting stream.
pub(crate) fn requested_variant(args: &[String]) -> Result<Option<VideoVariant>, String> {
    let mut selected = None;
    for value in args
        .iter()
        .filter_map(|argument| argument.strip_prefix("variant="))
    {
        if selected.is_some() {
            return Err("variant may be specified only once".to_string());
        }
        selected = Some(
            VideoVariant::from_id(&value.to_ascii_lowercase())
                .map_err(|error| format!("unsupported variant {value:?}: {error}"))?,
        );
    }
    Ok(selected)
}

/// Resolve the QP-map policy for this run.
///
/// Absent means [`crate::qp_map::QpMapPolicy::Off`] — the shipped behaviour,
/// and deliberately the default until the benchmark says otherwise. An
/// unknown value is an error rather than a fallback, for the same reason
/// `variant=` and `intent=` refuse to guess: a benchmark run that silently
/// measured the wrong arm would be worse than no benchmark.
#[cfg(any(test, windows, target_os = "linux"))]
pub(crate) fn requested_qp_map(args: &[String]) -> Result<crate::qp_map::QpMapPolicy, String> {
    let mut selected = None;
    for value in args
        .iter()
        .filter_map(|argument| argument.strip_prefix("qp-map="))
    {
        if selected.is_some() {
            return Err("qp-map may be specified only once".to_string());
        }
        selected = Some(
            crate::qp_map::QpMapPolicy::from_token(&value.to_ascii_lowercase()).ok_or_else(
                || {
                    let known = crate::qp_map::QpMapPolicy::ALL
                        .iter()
                        .map(|policy| policy.token())
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!("unsupported qp-map {value:?}: expected one of {known}")
                },
            )?,
        );
    }
    Ok(selected.unwrap_or_default())
}

/// Resolve the colour contract for this run.
///
/// An explicit `variant=` wins; otherwise the legacy contract is reconstructed
/// from the positional `yuv444` token. Every real run entry point
/// (`win::run_with_args`, `win_mf`'s `MfRunOpts`/`OpenH264RunOpts`
/// construction, `linux::run_with_args`/`probe_with_args`,
/// `linux_x11::run_with_args`) must call this rather than constructing
/// `ColorSpec::legacy(...)` directly, and must feed the *same* resolved
/// value to both the encoder and `resolved_media_plan` — see
/// `variant_argv_reaches_resolved_media_plan_with_the_requested_contract`
/// for why: a value resolved twice, independently, is how the READY line and
/// the actual encode end up disagreeing.
///
/// # Errors
///
/// Propagates `requested_variant`'s error for an unknown or repeated
/// `variant=`.
pub(crate) fn requested_color(args: &[String], yuv444: bool) -> Result<ColorSpec, String> {
    Ok(requested_variant(args)?.map_or_else(|| ColorSpec::legacy(yuv444), ColorSpec::from_variant))
}

/// Resolve what the encoder should optimise for.
///
/// Absent argument means [`EncodeIntent::Interactive`], which is what capenc
/// has always encoded. An unknown value is an error rather than a silent
/// fallback: a caller asking for `quality` and quietly getting latency-first
/// output is precisely the kind of unreported degradation this work removes.
#[cfg(any(test, windows, target_os = "linux"))]
pub(crate) fn requested_intent(args: &[String]) -> Result<EncodeIntent, String> {
    let mut selected = None;
    for value in args
        .iter()
        .filter_map(|argument| argument.strip_prefix("intent="))
    {
        if selected.is_some() {
            return Err("intent may be specified only once".to_string());
        }
        selected = Some(
            EncodeIntent::from_token(&value.to_ascii_lowercase()).ok_or_else(|| {
                let known = EncodeIntent::ALL
                    .iter()
                    .map(|intent| intent.token())
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("unsupported intent {value:?}: expected one of {known}")
            })?,
        );
    }
    Ok(selected.unwrap_or_default())
}

/// Linux's portable OpenH264 path has neither the quality NVENC preset nor
/// NVENC's per-block QP-map API. Keep this check pure so every dispatch path
/// (initial selection and native-to-software fallback) rejects the same
/// unsupported experiment arms.
#[cfg(any(test, target_os = "linux"))]
pub(crate) fn linux_software_policy_supported(
    intent: EncodeIntent,
    qp_map_policy: crate::qp_map::QpMapPolicy,
) -> bool {
    intent == EncodeIntent::Interactive && !qp_map_policy.submits_map()
}

#[cfg(any(
    test,
    windows,
    all(target_os = "linux", any(feature = "nvenc", feature = "software-h264"))
))]
pub(crate) fn resolved_media_plan(
    backend: EncoderBackend,
    codec: &str,
    color: ColorSpec,
    width: u32,
    height: u32,
    fps: u32,
    cursor_mode: CursorCaptureMode,
) -> Result<ResolvedMediaPlan, String> {
    // Table lookup rather than a match: a new codec is added to the shared
    // vocabulary and is understood here without an edit.
    let codec = VideoCodec::from_token(&codec.to_ascii_lowercase()).ok_or_else(|| {
        // Name the value. "unsupported READY codec" with no subject cannot
        // distinguish a genuinely unknown codec from an argument-position bug,
        // and that ambiguity cost hours.
        format!(
            "unsupported READY codec {codec:?}; expected one of {:?}",
            VideoCodec::ALL
                .iter()
                .map(|value| value.token())
                .collect::<Vec<_>>()
        )
    })?;
    let encoder = match backend {
        EncoderBackend::NativeNvenc => EncoderRequest::NativeNvenc,
        EncoderBackend::WindowsMediaFoundation => EncoderRequest::WindowsMediaFoundation,
        EncoderBackend::OpenH264 => EncoderRequest::SoftwareH264,
        EncoderBackend::Rav1e => EncoderRequest::SoftwareAv1,
    };
    let contract = backend.contract();
    let request = MediaRequest {
        encoder,
        video: VideoConfiguration {
            codec,
            chroma: color.chroma,
            bit_depth: color.bit_depth,
            range: color.range,
            matrix: color.matrix,
            primaries: color.primaries,
            transfer: color.transfer,
        },
        width,
        height,
        fps,
        cursor_mode: match cursor_mode {
            CursorCaptureMode::Local => CursorMode::Local,
            CursorCaptureMode::Host => CursorMode::Host,
        },
    };
    resolve_media_plan(
        request,
        &[BackendCandidate {
            backend,
            // Capability comes from the backend's declared contract, narrowed
            // to what this run actually produced. Previously it was rebuilt by
            // hand here, which is how the child's idea of a backend could drift
            // from the host's.
            availability: BackendAvailability::Available(BackendLimits {
                max_width: width.min(contract.max_width),
                max_height: height.min(contract.max_height),
                max_fps: fps.min(contract.max_fps),
                cursor_in_video: cursor_mode.include_cursor() && contract.cursor_in_video,
                ..contract
            }),
        }],
    )
    .map_err(|error| format!("resolve active media plan: {error}"))
}

#[cfg(any(
    windows,
    all(target_os = "linux", any(feature = "nvenc", feature = "software-h264"))
))]
pub(crate) fn announce_ready(plan: ResolvedMediaPlan) -> std::io::Result<()> {
    let session_log_id = SESSION_LOG_ID
        .get()
        .and_then(Option::as_ref)
        .map(CorrelationId::as_str);
    writeln!(
        std::io::stderr(),
        "{}",
        format_ready_v1(plan, session_log_id)
    )
}

#[cfg(any(windows, target_os = "linux"))]
pub(crate) fn announce_unavailable(
    backend: EncoderBackend,
    reason: BackendUnavailableReason,
) -> std::io::Result<()> {
    writeln!(
        std::io::stderr(),
        "{}",
        format_unavailable_v1(BackendUnavailableNotice { backend, reason })
    )
}

fn initialize_session_log_id() -> Result<(), &'static str> {
    let session_log_id = match std::env::var("ARCEN_SESSION_LOG_ID") {
        Ok(value) => Some(
            CorrelationId::parse_uuid(value)
                .map_err(|_| "ARCEN_SESSION_LOG_ID must be a canonical lowercase UUID")?,
        ),
        Err(std::env::VarError::NotPresent) => None,
        Err(std::env::VarError::NotUnicode(_)) => {
            return Err("ARCEN_SESSION_LOG_ID must be valid UTF-8");
        }
    };
    SESSION_LOG_ID
        .set(session_log_id)
        .map_err(|_| "session log id was already initialized")
}

#[cfg(any(
    test,
    windows,
    all(target_os = "linux", any(feature = "nvenc", feature = "software-h264"))
))]
pub(crate) fn framed_output_from_args(args: &[String]) -> bool {
    args.iter().any(|arg| arg == FRAMED_OUTPUT_V1)
}

#[cfg(any(
    test,
    windows,
    all(target_os = "linux", any(feature = "nvenc", feature = "software-h264"))
))]
pub(crate) fn validate_access_unit(access_unit: &[u8], framed: bool) -> std::io::Result<()> {
    if access_unit.is_empty() || access_unit.len() > MAX_ACCESS_UNIT_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "encoded access unit is empty or exceeds the 16 MiB cap",
        ));
    }
    if framed {
        u32::try_from(access_unit.len()).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "encoded access unit exceeds framed-v1 u32 length",
            )
        })?;
    }
    Ok(())
}

#[cfg(any(
    test,
    windows,
    all(target_os = "linux", any(feature = "nvenc", feature = "software-h264"))
))]
pub(crate) fn write_access_unit(
    writer: &mut impl Write,
    access_unit: &[u8],
    framed: bool,
) -> std::io::Result<()> {
    validate_access_unit(access_unit, framed)?;
    if framed {
        let len = u32::try_from(access_unit.len()).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "encoded access unit exceeds framed-v1 u32 length",
            )
        })?;
        writer.write_all(&len.to_be_bytes())?;
    }
    writer.write_all(access_unit)?;
    writer.flush()
}

#[cfg(any(
    windows,
    all(target_os = "linux", any(feature = "nvenc", feature = "software-h264"))
))]
pub(crate) fn frame_interval_from_fps(fps: u32) -> Duration {
    let fps = fps.clamp(1, 240);
    Duration::from_micros(1_000_000 / u64::from(fps))
}

pub fn run_with_args(args: Vec<String>) {
    if let Err(error) = initialize_session_log_id() {
        let _ = writeln!(std::io::stderr(), "[capenc] ERROR: {error}");
        std::process::exit(2);
    }
    // Only genuinely unused where neither platform arm consumes it.
    #[cfg(not(any(target_os = "linux", windows)))]
    let _ = &args;

    #[cfg(windows)]
    win::run_with_args(args);

    #[cfg(target_os = "linux")]
    {
        // Validate these experiment axes before encoder dispatch. In
        // particular, the software path must not silently ignore an unknown
        // or repeated token just because it has no NVENC constructor.
        let intent = match requested_intent(&args) {
            Ok(intent) => intent,
            Err(error) => {
                log(&format!("ERROR: invalid intent: {error}"));
                std::process::exit(2);
            }
        };
        let qp_map_policy = match requested_qp_map(&args) {
            Ok(policy) => policy,
            Err(error) => {
                log(&format!("ERROR: invalid qp-map: {error}"));
                std::process::exit(2);
            }
        };
        if args.iter().any(|argument| argument == "probe-matrix") {
            #[cfg(feature = "nvenc")]
            std::process::exit(linux::probe_matrix_with_args(&args));
            #[cfg(not(feature = "nvenc"))]
            {
                log("ERROR: probe-matrix requires this build to have --features nvenc");
                std::process::exit(2);
            }
        }
        let requested = match linux_policy::RequestedEncoder::from_args(&args) {
            Ok(requested) => requested,
            Err(error) => {
                log(&format!("ERROR: {error}"));
                std::process::exit(2);
            }
        };
        let startup_path = linux_policy::startup_path(
            requested,
            args.iter().any(|argument| argument == "probe-v1"),
        );
        if startup_path == linux_policy::StartupPath::Software {
            if !linux_software_policy_supported(intent, qp_map_policy) {
                log(
                    "ERROR: software-h264 supports only intent=interactive and qp-map=off; \
                     quality and QP delta maps require native NVENC",
                );
                std::process::exit(2);
            }
            #[cfg(feature = "software-h264")]
            linux_x11::run_with_args(args);
            #[cfg(not(feature = "software-h264"))]
            {
                let _ = announce_unavailable(
                    EncoderBackend::OpenH264,
                    BackendUnavailableReason::NotBuilt,
                );
                std::process::exit(2);
            }
        }
        if startup_path == linux_policy::StartupPath::NativeProbe {
            #[cfg(feature = "nvenc")]
            linux::probe_with_args(args);
            #[cfg(not(feature = "nvenc"))]
            {
                let _ = announce_unavailable(
                    EncoderBackend::NativeNvenc,
                    BackendUnavailableReason::NotBuilt,
                );
                std::process::exit(2);
            }
        }
        #[cfg(feature = "nvenc")]
        linux::run_with_args(args, requested);
        #[cfg(not(feature = "nvenc"))]
        {
            let _ = announce_unavailable(
                EncoderBackend::NativeNvenc,
                BackendUnavailableReason::NotBuilt,
            );
            std::process::exit(2);
        }
    }

    #[cfg(not(any(windows, target_os = "linux")))]
    {
        log("ERROR: no backend for this platform/feature combination");
        std::process::exit(1);
    }
}

/// Entry point when this helper runs as its own binary.
pub fn run() {
    let args: Vec<String> = std::env::args().collect();
    run_with_args(args);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_output_remains_backward_compatible_default() {
        let mut out = Vec::new();
        write_access_unit(&mut out, b"\0\0\0\x01\x65payload", false).unwrap();
        assert_eq!(out, b"\0\0\0\x01\x65payload");
    }

    #[test]
    fn framed_v1_prefixes_big_endian_payload_length() {
        let au = b"\0\0\0\x01\x65payload";
        let mut out = Vec::new();
        write_access_unit(&mut out, au, true).unwrap();
        assert_eq!(&out[..4], &(au.len() as u32).to_be_bytes());
        assert_eq!(&out[4..], au);
    }

    #[test]
    fn stop_command_preserves_idr_and_requests_graceful_teardown() {
        let state = ControlState::new();
        read_control(
            std::io::Cursor::new(b"IDR\nSTOP\nignored\n"),
            &state,
            "test",
        );
        assert!(state.take_idr());
        assert!(state.stop_requested());
    }

    #[test]
    fn control_eof_requests_graceful_teardown() {
        let state = ControlState::new();
        read_control(std::io::Cursor::new(Vec::<u8>::new()), &state, "test");
        assert!(state.stop_requested());
        assert!(!state.idr_pending());
    }

    #[test]
    fn wake_command_is_a_one_shot_input_signal_independent_of_idr() {
        let state = ControlState::new();
        read_control(std::io::Cursor::new(b"WAKE\nwake\nSTOP\n"), &state, "test");
        // A wake must never imply a keyframe: the two signals are independent.
        assert!(!state.idr_pending());
        assert!(state.take_input_activity());
        assert!(
            !state.take_input_activity(),
            "an input wake is consumed exactly once"
        );
    }

    #[test]
    fn access_unit_cap_is_16_mib_and_rejects_empty_output() {
        assert_eq!(MAX_ACCESS_UNIT_BYTES, 16 * 1024 * 1024);
        assert!(validate_access_unit(&[], true).is_err());
        assert!(
            validate_access_unit(&vec![0; MAX_ACCESS_UNIT_BYTES.saturating_add(1)], true).is_err()
        );
        assert!(write_access_unit(&mut Vec::new(), &[], true).is_err());
        assert!(write_access_unit(
            &mut Vec::new(),
            &vec![0; MAX_ACCESS_UNIT_BYTES.saturating_add(1)],
            true
        )
        .is_err());
    }

    #[test]
    fn framed_mode_token_is_position_independent() {
        let args = vec![
            "arcen-capenc".to_string(),
            "0".to_string(),
            "h265".to_string(),
            "yuv444".to_string(),
            FRAMED_OUTPUT_V1.to_string(),
        ];
        assert!(framed_output_from_args(&args));
    }

    #[test]
    fn cursor_mode_is_strict_and_defaults_local() {
        assert_eq!(
            cursor_mode_from_args(&["arcen-capenc".to_string()]).unwrap(),
            CursorCaptureMode::Local
        );
        assert_eq!(
            cursor_mode_from_args(&["arcen-capenc".to_string(), "cursor=host".to_string()])
                .unwrap(),
            CursorCaptureMode::Host
        );
        assert!(
            cursor_mode_from_args(&["arcen-capenc".to_string(), "cursor=dynamic".to_string()])
                .is_err()
        );
        assert!(CursorCaptureMode::Host.include_cursor());
        assert!(CursorCaptureMode::Host.requires_wgc());
        assert!(!CursorCaptureMode::Local.include_cursor());
        assert!(!CursorCaptureMode::Local.requires_wgc());
    }

    fn args_vec(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn variant_selection_drives_the_whole_colour_contract() {
        // Absent, the legacy contract is reconstructed from the positional
        // yuv444 token so existing invocations are unchanged.
        let legacy = requested_color(&args_vec(&["0", "h265", "60", "yuv444"]), true).unwrap();
        assert_eq!(legacy.chroma, ChromaSubsampling::Yuv444);
        assert_eq!(legacy.bit_depth, BitDepth::Eight);
        assert_eq!(legacy.range, ColorRange::Limited);

        // An explicit variant selects every colour axis at once.
        let grading = requested_color(
            &args_vec(&["0", "h265", "variant=hevc-444-10-full-bt709"]),
            false,
        )
        .unwrap();
        assert_eq!(grading.chroma, ChromaSubsampling::Yuv444);
        assert_eq!(grading.bit_depth, BitDepth::Ten);
        assert_eq!(grading.range, ColorRange::Full);
        assert_eq!(grading.matrix, ColorMatrix::Bt709);

        // The variant wins over the positional token rather than merging with
        // it, so a row in the findings file is unambiguous.
        let identity = requested_color(
            &args_vec(&["0", "h265", "yuv420", "variant=hevc-444-10-full-identity"]),
            false,
        )
        .unwrap();
        assert_eq!(identity.chroma, ChromaSubsampling::Yuv444);
        assert_eq!(identity.matrix, ColorMatrix::Identity);
    }

    /// Intent parses from argv, defaults to latency-first, and refuses to
    /// guess — for the same reason `variant=` does. A session that asked for
    /// grading quality and silently got the interactive encoder would look
    /// like a codec regression and be debugged as one.
    #[test]
    fn intent_parses_defaults_to_interactive_and_refuses_to_guess() {
        assert_eq!(
            requested_intent(&args_vec(&["0", "h265", "60"])).unwrap(),
            EncodeIntent::Interactive,
            "absent intent must keep the shipped behaviour",
        );
        assert_eq!(
            requested_intent(&args_vec(&["0", "h265", "60", "intent=quality"])).unwrap(),
            EncodeIntent::Quality,
        );
        // Case is normalised, matching `variant=`.
        assert_eq!(
            requested_intent(&args_vec(&["intent=QUALITY"])).unwrap(),
            EncodeIntent::Quality,
        );
        assert!(requested_intent(&args_vec(&["intent=nonsense"])).is_err());
        assert!(requested_intent(&args_vec(&["intent=quality", "intent=interactive"])).is_err());
    }

    /// The QP-map policy must default to off and refuse to guess, so a
    /// benchmark can never silently measure the wrong arm.
    #[test]
    fn qp_map_policy_parses_defaults_to_off_and_refuses_to_guess() {
        use crate::qp_map::QpMapPolicy;
        assert_eq!(
            requested_qp_map(&args_vec(&["0", "h265", "60"])).unwrap(),
            QpMapPolicy::Off,
        );
        assert_eq!(
            requested_qp_map(&args_vec(&["qp-map=on"])).unwrap(),
            QpMapPolicy::On,
        );
        assert_eq!(
            requested_qp_map(&args_vec(&["qp-map=NEUTRAL"])).unwrap(),
            QpMapPolicy::Neutral,
        );
        assert!(requested_qp_map(&args_vec(&["qp-map=maybe"])).is_err());
        assert!(requested_qp_map(&args_vec(&["qp-map=on", "qp-map=off"])).is_err());
    }

    #[test]
    fn linux_software_policy_refuses_unimplementable_nvenc_arms() {
        use crate::qp_map::QpMapPolicy;
        assert!(linux_software_policy_supported(
            EncodeIntent::Interactive,
            QpMapPolicy::Off
        ));
        assert!(!linux_software_policy_supported(
            EncodeIntent::Quality,
            QpMapPolicy::Off
        ));
        assert!(!linux_software_policy_supported(
            EncodeIntent::Interactive,
            QpMapPolicy::Neutral
        ));
        assert!(!linux_software_policy_supported(
            EncodeIntent::Interactive,
            QpMapPolicy::On
        ));
    }

    #[test]
    fn unknown_or_repeated_variants_fail_rather_than_defaulting() {
        // Silently defaulting would mislabel the resulting stream, which is
        // the exact failure the colour work exists to remove.
        assert!(requested_variant(&args_vec(&["variant=hevc-444-12-full-bt709"])).is_err());
        assert!(requested_variant(&args_vec(&["variant=nonsense"])).is_err());
        assert!(requested_variant(&args_vec(&[
            "variant=hevc-444-10-full-bt709",
            "variant=h264-420-8-full-bt709"
        ]))
        .is_err());
        assert!(requested_variant(&args_vec(&["0", "h265"]))
            .unwrap()
            .is_none());
    }

    /// The bug this test guards against: `variant=<id>` used to parse and
    /// validate successfully while every real run entry point still built
    /// `ColorSpec::legacy(...)`, so the encoder silently produced 8-bit
    /// BT.709 limited regardless of what was requested. This traces a
    /// *realistic* argv (as `win::run_with_args`/`linux::run_with_args`
    /// actually receive it — output index, codec, fps, then `variant=`)
    /// through `requested_color` and into `resolved_media_plan`, the same
    /// function that builds the READY line a real run announces, so the
    /// assertion is not merely about the parser in isolation: it is about
    /// the exact contract a client would be told this session produced.
    /// `Encoder::new` itself cannot be exercised here (it needs a live
    /// device), but every real call site now feeds it this identical
    /// `color` value — see `win.rs`/`win_mf.rs`/`linux.rs`/`linux_x11.rs`.
    #[test]
    fn variant_argv_reaches_resolved_media_plan_with_the_requested_contract() {
        let args = args_vec(&["0", "h265", "60", "variant=hevc-444-10-full-bt709"]);
        let color = requested_color(&args, false).expect("a real, coherent probe-matrix variant");
        assert_eq!(color.chroma, ChromaSubsampling::Yuv444);
        assert_eq!(color.bit_depth, BitDepth::Ten);
        assert_eq!(color.range, ColorRange::Full);
        assert_eq!(color.matrix, ColorMatrix::Bt709);

        let plan = resolved_media_plan(
            EncoderBackend::NativeNvenc,
            "h265",
            color,
            3840,
            2160,
            60,
            CursorCaptureMode::Local,
        )
        .expect("the grading-reference contract must resolve to a valid plan");
        assert_eq!(plan.video.chroma, ChromaSubsampling::Yuv444);
        assert_eq!(plan.video.bit_depth, BitDepth::Ten);
        assert_eq!(plan.video.range, ColorRange::Full);
        assert_eq!(plan.video.matrix, ColorMatrix::Bt709);
        assert!(plan.supports_yuv444());
        assert!(plan.supports_main10());
        assert!(plan.supports_full_range());
    }

    #[test]
    fn openh264_ready_plan_is_h264_420_local_cursor_and_explicit_backend() {
        let plan = resolved_media_plan(
            EncoderBackend::OpenH264,
            "h264",
            crate::ColorSpec::legacy(false),
            1920,
            1080,
            30,
            CursorCaptureMode::Local,
        )
        .expect("valid OpenH264 plan");
        assert_eq!(plan.backend, EncoderBackend::OpenH264);
        assert_eq!(plan.video.codec, VideoCodec::H264);
        assert_eq!(plan.video.chroma, ChromaSubsampling::Yuv420);
        assert!(plan.supports_h264());
        assert!(!plan.supports_h265());
        assert!(!plan.supports_yuv444());
        assert!(!plan.cursor_in_video);
    }

    /// OpenH264 must refuse a host-composited cursor rather than quietly
    /// dropping it.
    ///
    /// The portable software encoder cannot composite the cursor into the
    /// frame, so a `Host` request is not a preference it can partially honour —
    /// `resolved_media_plan` rejects the whole plan. That matters because the
    /// alternative failure mode is a session that silently never shows a
    /// cursor.
    ///
    /// This case used to be asserted the other way round: the test requested
    /// `Host` and expected a valid plan with the cursor in video. That had
    /// stopped being true and the test was failing, invisibly, because an
    /// earlier `cargo check` step in the same CI job failed first and this one
    /// never ran.
    #[test]
    fn openh264_refuses_a_host_composited_cursor() {
        let error = resolved_media_plan(
            EncoderBackend::OpenH264,
            "h264",
            crate::ColorSpec::legacy(false),
            1920,
            1080,
            30,
            CursorCaptureMode::Host,
        )
        .expect_err("OpenH264 cannot composite a host cursor");
        assert!(
            error.contains("backend does not support"),
            "unexpected rejection reason: {error}"
        );
    }

    #[test]
    fn session_log_id_prefix_is_additive() {
        let id = CorrelationId::from_uuid_v4_bytes([0; 16]);
        assert_eq!(
            format_log_line("enc_fps=60", Some(&id)),
            "[capenc] enc_fps=60 sid=00000000-0000-4000-8000-000000000000"
        );
        assert_eq!(format_log_line("selftest", None), "[capenc] selftest");
    }
}
