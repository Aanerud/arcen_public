//! `capenc probe-matrix` — the host half of the colour-matrix hardware probe.
//!
//! Arcen is adding grader/VFX-grade colour (4:4:4, 10-bit, full range). The
//! NVENC capability booleans NVENC exposes (`NV_ENC_CAPS_SUPPORT_YUV444_ENCODE`,
//! `NV_ENC_CAPS_SUPPORT_10BIT_ENCODE`, ...) are **independent**: a GPU/driver
//! reporting both `true` is not proof the *combination* initialises, and
//! there is no NVIDIA document that states which combinations do. The only
//! reliable answer is a trial `NvEncInitializeEncoder` — this subcommand
//! walks every row of [`arcen_media::video::PROBE_MATRIX`] and attempts
//! exactly that (plus the software encoder(s) compiled into this build),
//! recording what actually happened rather than what the capability bits
//! claimed. See `docs/testing/color-matrix-results.json`, which this report
//! is shaped to match and which the Deck side (`arcen-deck probe-matrix`,
//! `clients/macos/src/probe_matrix.rs`) fills in the other half of.
//!
//! # Usage
//!
//! ```text
//! capenc probe-matrix [--output <path>]
//! ```
//!
//! Emits the JSON report to stdout, or to `--output <path>` if given (both
//! cases also print a one-line confirmation to stderr via the caller). A row
//! that cannot initialise is a **finding**, not an error: every row runs to
//! completion regardless of any other row's outcome, and this subcommand
//! exits 0 as long as the matrix was walked and the report was rendered —
//! only an argument or I/O error (e.g. an unwritable `--output` path) exits
//! non-zero.
//!
//! # What this module owns vs. what the caller owns
//!
//! Everything in this file is platform- and feature-independent on purpose:
//! it depends only on `arcen_media` and `std`, so it compiles and is
//! unit-testable in every build of this crate, including the default
//! (`nvenc`/`mf` both off) build tested in CI. The actual `NvEncInitializeEncoder`
//! /Media Foundation trial calls are real FFI that need a live GPU and a
//! `windows`-crate device — those live in `win.rs`
//! (`win::run_probe_matrix_subcommand`), which builds one
//! [`EncoderAttemptOutcome`] per backend per row and hands it to
//! [`probe_one_row`]/[`build_report`] here. This split is what makes the
//! aggregation and JSON-shaping logic testable without hardware, while
//! keeping the one thing that truly needs a GPU (the trial init itself)
//! everything win.rs already owns.
//!
//! # Output shape
//!
//! Matches `docs/testing/color-matrix-results.json`: `schema_version`,
//! `environments` (one entry — this host's OS/GPU/driver where obtainable;
//! `client` is always `null`, since this subcommand never decodes) and
//! `results` (one row per [`PROBE_MATRIX`] entry, in matrix order). It omits
//! the tracked file's `_comment`/`field_reference` blocks, which are static
//! documentation for a human, not a finding.
//!
//! # Known limitations
//!
//! `decode`/`hardware_decode`/`delivered_pixel_format`/
//! `color_extensions_attached`/`roundtrip_max_error` are always
//! `untested`/`null`: this subcommand only ever encodes, so it cannot know
//! whether a client's decoder would accept what it produced — that is
//! exactly the Deck-side half this report is meant to be merged with.
//! `sustained_fps`/`bitrate_mbps`, when present, measure a short synthetic
//! burst run back-to-back as fast as possible immediately after a
//! successful init, not a real-time-paced sustained session; `notes` says so
//! on every row where a rate is reported. `driver_version`/`nvenc_generation`
//! are best-effort and often empty: this crate has no registry/NVAPI access
//! wired in to query them (see `win.rs`), so a human filling in
//! `docs/testing/color-matrix-results.json` may need to add them by hand.
//!
//! # Round-trip colour sources (`--roundtrip-pattern`/`--roundtrip-output-dir`)
//!
//! When both flags are given, this subcommand *additionally* encodes a
//! chosen [`arcen_media::test_pattern::TestPattern`] (instead of live
//! capture, and instead of the ordinary capability sweep's synthetic
//! gradient burst) for every row whose codec has a real encoder in this
//! build, and writes one Annex-B bitstream file per variant id plus a single
//! shared `roundtrip-meta.json` into `--roundtrip-output-dir`. This is
//! deliberately a *separate* output, not a new field on the ordinary
//! [`ProbeReport`]: the bitstream files use exactly the filename
//! `arcen-deck probe-matrix --parameter-sets <dir>` already tries first for
//! every row (see `clients/macos/src/probe_matrix.rs`'s
//! `candidate_input_paths`), so the existing parameter-set/decode pipeline
//! needs no changes at all to consume them; only the Deck side's own
//! `--reference-pattern <token>` flag (which cross-checks
//! `roundtrip-meta.json` when present) is new there.
//!
//! `TestPattern` is a pure function of `(column, row, width, height)`, so the
//! Deck side reproduces the exact reference pixels from the pattern token
//! and geometry recorded in `roundtrip-meta.json` alone -- only the *coded
//! bitstream* ever needs to cross the machine boundary. See
//! `arcen_media::test_pattern`'s module doc for why this two-halves-plus-a-
//! comparison shape is the only way to measure a real encode/decode round
//! trip when the two ends run on different machines.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use arcen_media::test_pattern::TestPattern;
use arcen_media::video::{VideoVariant, PROBE_MATRIX};
use arcen_media::{ChromaSubsampling, VideoCodec};

/// `docs/testing/color-matrix-results.json`'s own `schema_version`.
const SCHEMA_VERSION: u32 = 1;

/// `docs/testing/color-matrix-results.json`'s `decode`/`encoder_init` value
/// vocabulary, exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProbeOutcome {
    Ok,
    Failed,
    Unsupported,
    Untested,
}

impl ProbeOutcome {
    const fn token(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Failed => "failed",
            Self::Unsupported => "unsupported",
            Self::Untested => "untested",
        }
    }
}

/// One backend's (NVENC, Media Foundation, ...) attempt to initialise a
/// single [`PROBE_MATRIX`] row. Built by the platform-specific caller
/// (`win.rs`) from a real trial; folded into a [`RowFinding`] by
/// [`fold_attempts_into`].
#[derive(Debug, Clone)]
pub(crate) enum EncoderAttemptOutcome {
    /// A real `NvEncInitializeEncoder` (or equivalent) succeeded.
    /// `sustained_fps`/`bitrate_mbps` come from a short post-init encode
    /// burst, when one was feasible; `note` records anything else worth
    /// keeping (whether a first access unit was produced, burst caveats).
    Ok {
        sustained_fps: Option<f64>,
        bitrate_mbps: Option<f64>,
        note: String,
    },
    /// The backend (or a pre-driver check, e.g. `resolve_pixel_format`)
    /// reported this exact combination cannot be encoded at all.
    Unsupported { detail: String },
    /// A real trial was attempted and failed for a reason other than "this
    /// combination is unsupported" (e.g. no NVENC device, a driver-internal
    /// error) — an environment/hardware finding, not a format finding.
    Failed { detail: String },
    /// This backend is not compiled into the running binary at all, so no
    /// trial was possible.
    NotCompiled { detail: String },
}

/// One row of the emitted `results` array. Field names and value vocabulary
/// match `docs/testing/color-matrix-results.json` exactly; every field this
/// module cannot observe from the encode side is left at its zero value
/// (`Untested`/`None`/empty), never fabricated.
#[derive(Debug, Clone)]
pub(crate) struct RowFinding {
    variant: String,
    encoder_init: ProbeOutcome,
    encoder_error: Option<String>,
    /// Always `Untested`: this subcommand never decodes. See the module doc.
    decode: ProbeOutcome,
    sustained_fps: Option<f64>,
    bitrate_mbps: Option<f64>,
    notes: String,
}

impl RowFinding {
    fn new(variant: String) -> Self {
        Self {
            variant,
            encoder_init: ProbeOutcome::Untested,
            encoder_error: None,
            decode: ProbeOutcome::Untested,
            sustained_fps: None,
            bitrate_mbps: None,
            notes: String::new(),
        }
    }

    /// Appends `text` to `notes`, separating existing notes with `"; "` so
    /// every explanation accumulated for a row survives, rather than the
    /// last one overwriting the rest.
    fn push_note(&mut self, text: &str) {
        if !self.notes.is_empty() {
            self.notes.push_str("; ");
        }
        self.notes.push_str(text);
    }

    fn to_json(&self) -> Json {
        Json::Object(vec![
            ("variant", Json::str(self.variant.clone())),
            ("encoder_init", Json::str(self.encoder_init.token())),
            ("encoder_error", Json::opt_str(self.encoder_error.clone())),
            ("decode", Json::str(self.decode.token())),
            ("hardware_decode", Json::Null),
            ("delivered_pixel_format", Json::Null),
            ("color_extensions_attached", Json::Null),
            ("roundtrip_max_error", Json::Null),
            ("sustained_fps", Json::opt_f64(self.sustained_fps)),
            ("bitrate_mbps", Json::opt_f64(self.bitrate_mbps)),
            ("notes", Json::str(self.notes.clone())),
        ])
    }
}

/// Fold every backend's [`EncoderAttemptOutcome`] for one row into `finding`.
///
/// Priority, in order: any backend that actually succeeded wins the overall
/// verdict (`Ok`) even if another backend failed or refused the request —
/// `encoder_error` is then left `None`, since the row *did* initialise, and
/// every backend's individual outcome is preserved in `notes` instead. Absent
/// a success, a real failure (`Failed`) outranks a typed refusal
/// (`Unsupported`) for the overall verdict, and the first backend's detail
/// (NVENC is always tried first — see `win.rs`) becomes `encoder_error`,
/// matching that field's own documented meaning ("Exact NVENC/encoder status
/// string when init failed"). `NotCompiled` never changes the verdict; it is
/// recorded only as a note.
fn fold_attempts_into(
    finding: &mut RowFinding,
    attempts: Vec<(&'static str, EncoderAttemptOutcome)>,
) {
    let mut any_ok = false;
    let mut any_failed = false;
    let mut any_unsupported = false;
    let mut primary_error: Option<String> = None;

    for (name, outcome) in attempts {
        match outcome {
            EncoderAttemptOutcome::Ok {
                sustained_fps,
                bitrate_mbps,
                note,
            } => {
                any_ok = true;
                if finding.sustained_fps.is_none() {
                    finding.sustained_fps = sustained_fps;
                }
                if finding.bitrate_mbps.is_none() {
                    finding.bitrate_mbps = bitrate_mbps;
                }
                finding.push_note(&format!("{name}: ok ({note})"));
            }
            EncoderAttemptOutcome::Unsupported { detail } => {
                any_unsupported = true;
                if primary_error.is_none() {
                    primary_error = Some(detail.clone());
                }
                finding.push_note(&format!("{name}: unsupported ({detail})"));
            }
            EncoderAttemptOutcome::Failed { detail } => {
                any_failed = true;
                if primary_error.is_none() {
                    primary_error = Some(detail.clone());
                }
                finding.push_note(&format!("{name}: failed ({detail})"));
            }
            EncoderAttemptOutcome::NotCompiled { detail } => {
                finding.push_note(&format!("{name}: not compiled into this build ({detail})"));
            }
        }
    }

    finding.encoder_init = if any_ok {
        ProbeOutcome::Ok
    } else if any_failed {
        ProbeOutcome::Failed
    } else if any_unsupported {
        ProbeOutcome::Unsupported
    } else {
        ProbeOutcome::Untested
    };
    if !any_ok {
        finding.encoder_error = primary_error;
    }
}

/// Real `rav1e` trial for one AV1 4:4:4 [`PROBE_MATRIX`] row.
///
/// Unlike NVENC and Media Foundation, `rav1e` needs no GPU or OS device
/// handle at all -- it is pure-Rust, CPU-only software (see
/// `arcen_media::video::software_av1`'s module doc) -- so its trial can live
/// entirely in this feature/platform-independent module rather than needing
/// a `win.rs`/`linux.rs` counterpart to supply a device the way
/// [`fold_attempts_into`]'s NVENC/MF callers do. This is [`probe_one_row`]'s
/// clean entry point for rav1e: it is called directly, bypassing the
/// `attempt_backends` closure NVENC/MF are fed through, because there is
/// nothing platform-specific this trial needs from that caller.
///
/// **Not reachable yet.** `arcen-media` compiles its rav1e wrapper in behind
/// its own `software-av1-source` feature, but unlike `software-h264` (which
/// `hosts/capenc/Cargo.toml` forwards as `arcen-media/software-h264-source`),
/// this crate's `Cargo.toml` has no `software-av1` feature to forward
/// `arcen-media/software-av1-source` -- adding
/// `software-av1 = ["arcen-media/software-av1-source"]` there is a one-line
/// change outside this file's scope. Until it exists, every build compiles
/// the `#[cfg(not(...))]` fallback below and reports
/// [`EncoderAttemptOutcome::NotCompiled`], which is an accurate finding
/// today -- this build genuinely has no rav1e in it -- not a placeholder.
#[cfg(feature = "software-av1")]
fn attempt_rav1e_for_row(row: VideoVariant) -> EncoderAttemptOutcome {
    use arcen_media::video::{
        I444P16FrameMut, SoftwareAv1Config, SoftwareAv1Encoder, SoftwareAv1Error,
    };

    // rav1e is CPU-bound (~3.1 fps at 1080p, ~0.67 fps at 4K for this 4:4:4
    // tier -- see `shared/media/src/video/software_av1.rs`'s own throughput
    // benchmark), so a trial that only needs to prove initialisation and one
    // real encoded access unit uses a canvas two orders of magnitude smaller
    // than NVENC/MF's `win.rs::PROBE_WIDTH`/`PROBE_HEIGHT`, rather than
    // costing whole seconds per row it does not need to spend.
    const PROBE_WIDTH: u32 = 320;
    const PROBE_HEIGHT: u32 = 240;
    const PROBE_FRAMES: usize = 4;
    // Mid-grey at any bit depth: `I444P16FrameMut` samples are MSB-aligned in
    // the 16-bit word, so this one constant centres a ten- or twelve-bit
    // plane without shifting by bit depth.
    const MID_GREY_16: u16 = 0x8000;

    let color = crate::ColorSpec::from_variant(row);
    let config = SoftwareAv1Config {
        width: PROBE_WIDTH,
        height: PROBE_HEIGHT,
        fps: 30,
        chroma: color.chroma,
        bit_depth: color.bit_depth,
        range: color.range,
        matrix: color.matrix,
        primaries: color.primaries,
        transfer: color.transfer,
        bitrate_bps: 0,
        speed: 10,
        low_latency: true,
        tiles: 0,
        num_threads: 1,
    };

    let mut encoder = match SoftwareAv1Encoder::new(config) {
        Ok(encoder) => encoder,
        Err(SoftwareAv1Error::InvalidConfig) => {
            return EncoderAttemptOutcome::Unsupported {
                detail: "rav1e rejected this row's chroma/bit-depth/range/matrix combination \
                         against its own contract"
                    .to_string(),
            };
        }
        Err(error) => {
            return EncoderAttemptOutcome::Failed {
                detail: format!("SoftwareAv1Encoder::new: {error}"),
            };
        }
    };

    let sample_count = PROBE_WIDTH as usize * PROBE_HEIGHT as usize;
    let mut plane0 = vec![MID_GREY_16; sample_count];
    let mut plane1 = vec![MID_GREY_16; sample_count];
    let mut plane2 = vec![MID_GREY_16; sample_count];
    let strides = [PROBE_WIDTH as usize; 3];

    let mut produced_any = false;
    for _ in 0..PROBE_FRAMES {
        let frame = match I444P16FrameMut::new(
            PROBE_WIDTH,
            PROBE_HEIGHT,
            [&mut plane0, &mut plane1, &mut plane2],
            strides,
        ) {
            Ok(frame) => frame,
            Err(error) => {
                return EncoderAttemptOutcome::Failed {
                    detail: format!("building the probe frame: {error}"),
                };
            }
        };
        match encoder.encode_i444_high_bit_depth(&frame) {
            Ok(Some(_unit)) => produced_any = true,
            Ok(None) => {}
            Err(error) => {
                return EncoderAttemptOutcome::Failed {
                    detail: format!("encode_i444_high_bit_depth: {error}"),
                };
            }
        }
    }
    match encoder.finish() {
        Ok(drained) => produced_any |= !drained.is_empty(),
        Err(error) => {
            return EncoderAttemptOutcome::Failed {
                detail: format!("finish: {error}"),
            };
        }
    }

    EncoderAttemptOutcome::Ok {
        sustained_fps: None,
        bitrate_mbps: None,
        note: if produced_any {
            format!(
                "rav1e initialised and produced a real access unit at a reduced \
                 {PROBE_WIDTH}x{PROBE_HEIGHT} probe canvas (no burst measured -- rav1e's real \
                 throughput at delivery resolution is a separate, deliberate measurement, not a \
                 probe-matrix side effect)"
            )
        } else {
            format!(
                "rav1e initialised at a reduced {PROBE_WIDTH}x{PROBE_HEIGHT} probe canvas but \
                 produced no access unit within {PROBE_FRAMES} frames plus finish"
            )
        },
    }
}

/// Without the `software-av1` feature there is no rav1e to trial -- see the
/// doc on the real implementation above for exactly what adding that feature
/// (outside this file's scope) requires.
#[cfg(not(feature = "software-av1"))]
fn attempt_rav1e_for_row(_row: VideoVariant) -> EncoderAttemptOutcome {
    EncoderAttemptOutcome::NotCompiled {
        detail: "capenc was built without --features software-av1".to_string(),
    }
}

/// Probes one [`PROBE_MATRIX`] row.
///
/// H.264/HEVC rows, and now 4:2:0 AV1 rows, are handed to `attempt_backends`,
/// which the caller builds from real per-backend (NVENC, Media Foundation)
/// trials. 4:2:0 is the chroma NVENC's AV1 Main profile actually offers
/// (Ada onward), and the chroma Apple hardware-decodes from M3 onward, so
/// this is the tier that can answer whether the mainline delivery path can
/// move off the H.264/HEVC patent-pool royalties (see `variant.rs`'s matrix
/// doc). Routing it through the same `attempt_backends` path H.264/HEVC
/// already use is what lets a GPU/driver that cannot do it refuse cleanly,
/// rather than the row staying an untested assumption forever -- exactly
/// like every other row this matrix walks. This does assume the NVENC
/// dispatch `attempt_backends` reaches recognises `"av1"` as its own codec
/// (`row.video.codec.token()`) rather than defaulting an unrecognised token
/// to another codec's GUID; that recognition is `nvenc.rs`'s responsibility,
/// not this module's.
///
/// 4:4:4 AV1 rows -- the software, twelve-bit tier -- never reach
/// `attempt_backends` at all: no NVENC profile or Media Foundation MFT
/// handles 4:4:4 AV1 (or ever will -- neither is in either backend's
/// contract), so this calls [`attempt_rav1e_for_row`] directly instead.
///
/// Any other codec (VP9, JPEG) has no encoder wired into capenc at all yet
/// and is refused before either path runs, exactly as every AV1 row used to
/// be refused.
pub(crate) fn probe_one_row<F>(row: VideoVariant, mut attempt_backends: F) -> RowFinding
where
    F: FnMut(VideoVariant) -> Vec<(&'static str, EncoderAttemptOutcome)>,
{
    let mut finding = RowFinding::new(row.id());
    match row.video.codec {
        VideoCodec::H264 | VideoCodec::H265 => {
            fold_attempts_into(&mut finding, attempt_backends(row));
        }
        VideoCodec::Av1 if row.video.chroma == ChromaSubsampling::Yuv420 => {
            fold_attempts_into(&mut finding, attempt_backends(row));
        }
        VideoCodec::Av1 => {
            fold_attempts_into(&mut finding, vec![("rav1e", attempt_rav1e_for_row(row))]);
        }
        other => {
            finding.encoder_init = ProbeOutcome::Unsupported;
            finding.push_note(&format!(
                "no encoder wired into capenc handles {other:?} yet"
            ));
        }
    }
    finding
}

/// GPU/driver facts for this report's one `environments[].host` entry.
/// `client` in the emitted JSON is always `null` — a capenc run never
/// decodes, so it never knows anything about a client. Every field here is
/// best-effort: `String::new()` (rendered as JSON `""`, matching
/// `docs/testing/color-matrix-results.json`'s own fill-in-by-hand template)
/// when this build has no way to obtain it.
#[derive(Debug, Clone, Default)]
pub(crate) struct HostInfo {
    pub(crate) os: String,
    pub(crate) gpu: String,
    pub(crate) driver_version: String,
    pub(crate) nvenc_generation: String,
}

#[derive(Debug, Clone)]
pub(crate) struct EnvironmentInfo {
    environment_id: String,
    host: HostInfo,
    recorded_at: String,
    arcen_commit: String,
}

impl EnvironmentInfo {
    /// Builds the one `environments[]` entry this run contributes.
    /// `environment_id` is derived from `host.os`/`host.gpu` rather than the
    /// hostname, which would be personally identifying and would otherwise
    /// leak into a file this tool's caller may commit (the same reasoning
    /// `clients/macos/src/probe_matrix.rs` documents for its own
    /// `environment_id`).
    pub(crate) fn new(host: HostInfo) -> Self {
        let gpu_label = if host.gpu.is_empty() {
            "unknown-gpu"
        } else {
            host.gpu.as_str()
        };
        let os_label = if host.os.is_empty() {
            "unknown-os"
        } else {
            host.os.as_str()
        };
        let environment_id = format!(
            "{}-{}",
            os_label.replace(char::is_whitespace, "-"),
            gpu_label.replace(char::is_whitespace, "-"),
        );
        Self {
            environment_id,
            host,
            recorded_at: format_utc_timestamp(SystemTime::now()),
            arcen_commit: option_env!("ARCEN_SOURCE_REVISION")
                .unwrap_or("unknown")
                .to_string(),
        }
    }

    fn to_json(&self) -> Json {
        Json::Object(vec![
            ("environment_id", Json::str(self.environment_id.clone())),
            (
                "host",
                Json::Object(vec![
                    ("os", Json::str(self.host.os.clone())),
                    ("gpu", Json::str(self.host.gpu.clone())),
                    (
                        "driver_version",
                        Json::str(self.host.driver_version.clone()),
                    ),
                    (
                        "nvenc_generation",
                        Json::str(self.host.nvenc_generation.clone()),
                    ),
                ]),
            ),
            ("client", Json::Null),
            ("recorded_at", Json::str(self.recorded_at.clone())),
            ("arcen_commit", Json::str(self.arcen_commit.clone())),
        ])
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ProbeReport {
    schema_version: u32,
    environments: Vec<EnvironmentInfo>,
    results: Vec<RowFinding>,
}

impl ProbeReport {
    /// Renders this report as pretty JSON matching
    /// `docs/testing/color-matrix-results.json`'s shape (minus its
    /// `_comment`/`field_reference` blocks — see the module doc).
    pub(crate) fn render(&self) -> String {
        Json::Object(vec![
            (
                "schema_version",
                Json::Number(f64::from(self.schema_version)),
            ),
            (
                "environments",
                Json::Array(
                    self.environments
                        .iter()
                        .map(EnvironmentInfo::to_json)
                        .collect(),
                ),
            ),
            (
                "results",
                Json::Array(self.results.iter().map(RowFinding::to_json).collect()),
            ),
        ])
        .to_pretty_string()
    }
}

/// Runs every row of [`PROBE_MATRIX`], in matrix order, through
/// `attempt_backends` and assembles the full report. Never fails: a row that
/// cannot initialise is recorded as a finding (see [`probe_one_row`]), not
/// propagated as an error.
pub(crate) fn build_report<F>(environment: EnvironmentInfo, mut attempt_backends: F) -> ProbeReport
where
    F: FnMut(VideoVariant) -> Vec<(&'static str, EncoderAttemptOutcome)>,
{
    let results = PROBE_MATRIX
        .iter()
        .copied()
        .map(|row| probe_one_row(row, &mut attempt_backends))
        .collect();
    ProbeReport {
        schema_version: SCHEMA_VERSION,
        environments: vec![environment],
        results,
    }
}

/// Parses `--output <path>` out of argv. `None` means the caller should
/// print the rendered report to stdout instead of writing it to a file.
pub(crate) fn output_path_from_args(args: &[String]) -> Option<PathBuf> {
    args.iter()
        .position(|argument| argument == "--output")
        .and_then(|index| args.get(index + 1))
        .map(PathBuf::from)
}

/// One request to produce round-trip colour-fidelity sources: a single
/// deterministic [`TestPattern`], encoded for real for every [`PROBE_MATRIX`]
/// row whose codec has a working encoder in this build, written to
/// `output_dir`. See this module's doc for the on-disk shape and why it is
/// deliberately separate from the ordinary capability-sweep [`ProbeReport`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RoundtripRequest {
    pub(crate) pattern: TestPattern,
    pub(crate) output_dir: PathBuf,
}

/// Failure to parse `--roundtrip-pattern`/`--roundtrip-output-dir` out of
/// argv.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RoundtripArgError {
    /// Exactly one of `--roundtrip-pattern`/`--roundtrip-output-dir` was
    /// given. Either both are present (round-trip sources are written
    /// alongside the ordinary sweep) or neither is (the sweep runs alone) --
    /// never a half-configured state that would silently produce nothing
    /// useful from the flag a caller did remember to pass.
    IncompleteRequest,
    /// `--roundtrip-pattern <value>` did not name a pattern
    /// [`TestPattern::from_token`] recognises.
    UnknownPattern(String),
    /// A flag was given more than once.
    Repeated(&'static str),
}

impl std::fmt::Display for RoundtripArgError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::IncompleteRequest => formatter.write_str(
                "--roundtrip-pattern and --roundtrip-output-dir must both be given, or neither",
            ),
            Self::UnknownPattern(value) => {
                write!(formatter, "unknown --roundtrip-pattern `{value}`")
            }
            Self::Repeated(flag) => write!(formatter, "{flag} may be specified only once"),
        }
    }
}

impl std::error::Error for RoundtripArgError {}

/// Finds a `--flag <value>` pair in argv.
///
/// # Errors
///
/// Returns [`RoundtripArgError::Repeated`] if `flag` appears more than once
/// -- even with the same value each time -- rather than silently accepting
/// the first or last: a repeated flag is a caller mistake worth surfacing.
fn single_flag_value(
    args: &[String],
    flag: &'static str,
) -> Result<Option<String>, RoundtripArgError> {
    let mut found = None;
    for (index, argument) in args.iter().enumerate() {
        if argument == flag {
            if found.is_some() {
                return Err(RoundtripArgError::Repeated(flag));
            }
            found = args.get(index + 1).cloned();
        }
    }
    Ok(found)
}

/// Parses `--roundtrip-pattern <token>` and `--roundtrip-output-dir <dir>`
/// out of argv. `Ok(None)` means neither flag was given, so the caller
/// should run the ordinary capability sweep only.
///
/// # Errors
///
/// Rejects a repeated flag, an unrecognised pattern token, or exactly one of
/// the two flags being present without the other.
pub(crate) fn parse_roundtrip_request(
    args: &[String],
) -> Result<Option<RoundtripRequest>, RoundtripArgError> {
    let pattern_value = single_flag_value(args, "--roundtrip-pattern")?;
    let output_dir_value = single_flag_value(args, "--roundtrip-output-dir")?;
    match (pattern_value, output_dir_value) {
        (None, None) => Ok(None),
        (Some(pattern_token), Some(output_dir)) => {
            let pattern = TestPattern::from_token(&pattern_token)
                .ok_or(RoundtripArgError::UnknownPattern(pattern_token))?;
            Ok(Some(RoundtripRequest {
                pattern,
                output_dir: PathBuf::from(output_dir),
            }))
        }
        (Some(_), None) | (None, Some(_)) => Err(RoundtripArgError::IncompleteRequest),
    }
}

/// The bitstream filename one row's round-trip source is written to inside
/// `output_dir`: exactly the variant id, with no extension -- the first
/// filename `arcen-deck probe-matrix --parameter-sets <dir>` already tries
/// for every row (see `clients/macos/src/probe_matrix.rs`'s
/// `candidate_input_paths`), so the client side needs no changes at all to
/// locate these files.
pub(crate) fn roundtrip_bitstream_path(output_dir: &Path, row: VideoVariant) -> PathBuf {
    output_dir.join(row.id())
}

/// The one shared metadata file written per round-trip run.
///
/// A single run always encodes one pattern at one resolution for the whole
/// matrix, so this file (not a per-row one) is what lets the client side
/// detect a mismatch -- a wrong `--reference-pattern` token, or a decoded
/// size that does not match what was actually rendered -- instead of
/// silently producing a meaningless error figure.
pub(crate) fn roundtrip_meta_path(output_dir: &Path) -> PathBuf {
    output_dir.join("roundtrip-meta.json")
}

/// Renders [`roundtrip_meta_path`]'s content: the pattern token and exact
/// geometry used for this run, so the Deck side can regenerate the identical
/// reference pixels locally (see [`arcen_media::test_pattern`]'s module doc)
/// without ever receiving them.
fn roundtrip_meta_json(pattern: TestPattern, width: u32, height: u32) -> String {
    Json::Object(vec![
        ("pattern", Json::str(pattern.token())),
        ("width", Json::Number(f64::from(width))),
        ("height", Json::Number(f64::from(height))),
    ])
    .to_pretty_string()
}

/// Writes one row's coded round-trip bitstream, plus the shared metadata
/// file, into `request.output_dir` (created if it does not exist yet).
///
/// The metadata file is (re)written on every call rather than once: its
/// content is invariant across a whole run (one pattern, one geometry), so
/// the repeated write is simply idempotent, and it means a run interrupted
/// after only its first row still leaves usable, correct metadata behind.
///
/// # Errors
///
/// Propagates any I/O failure creating the directory or writing either file.
pub(crate) fn write_roundtrip_outputs(
    request: &RoundtripRequest,
    width: u32,
    height: u32,
    row: VideoVariant,
    bitstream: &[u8],
) -> std::io::Result<()> {
    std::fs::create_dir_all(&request.output_dir)?;
    std::fs::write(
        roundtrip_bitstream_path(&request.output_dir, row),
        bitstream,
    )?;
    std::fs::write(
        roundtrip_meta_path(&request.output_dir),
        roundtrip_meta_json(request.pattern, width, height),
    )
}

/// Days since the Unix epoch to a proleptic-Gregorian `(year, month, day)`.
/// Howard Hinnant's public-domain `civil_from_days` algorithm
/// (<https://howardhinnant.github.io/date_algorithms.html>). Reproduced here
/// (as `clients/macos/src/probe_matrix.rs` also does, independently, for its
/// own report's `recorded_at`) rather than pulled in as a new dependency,
/// since this is the only place in this crate that needs a calendar date.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m: u32 = if mp < 10 {
        (mp + 3) as u32
    } else {
        (mp - 9) as u32
    };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Formats a [`SystemTime`] as a UTC `YYYY-MM-DDTHH:MM:SSZ` timestamp for the
/// report's `recorded_at` field.
fn format_utc_timestamp(system_time: SystemTime) -> String {
    let duration = system_time.duration_since(UNIX_EPOCH).unwrap_or_default();
    let total_secs = duration.as_secs();
    let days = (total_secs / 86_400) as i64;
    let secs_of_day = total_secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    let hour = secs_of_day / 3600;
    let minute = (secs_of_day % 3600) / 60;
    let second = secs_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Minimal JSON value + pretty printer.
///
/// `arcen-capenc` deliberately carries no `serde`/`serde_json` dependency
/// (see `Cargo.toml`), so this report -- which must match the field names
/// and value vocabulary of `docs/testing/color-matrix-results.json` exactly
/// -- is built from this small hand-rolled model instead of adding a new
/// crate dependency for one report. A `Vec<(&str, Json)>`, not a map, backs
/// `Object`: Rust's std has no ordered map, and object field order here is
/// significant for a human comparing this output against the tracked file.
#[derive(Debug, Clone, PartialEq)]
enum Json {
    Null,
    Number(f64),
    String(String),
    Array(Vec<Json>),
    Object(Vec<(&'static str, Json)>),
}

impl Json {
    fn str(value: impl Into<String>) -> Self {
        Self::String(value.into())
    }

    fn opt_str(value: Option<String>) -> Self {
        value.map_or(Self::Null, Self::String)
    }

    fn opt_f64(value: Option<f64>) -> Self {
        value.map_or(Self::Null, Self::Number)
    }

    fn write(&self, out: &mut String, indent: usize) {
        match self {
            Self::Null => out.push_str("null"),
            Self::Number(value) => write_json_number(out, *value),
            Self::String(value) => write_json_string(out, value),
            Self::Array(items) => {
                Self::write_sequence(out, indent, items.len(), '[', ']', |out, index| {
                    items[index].write(out, indent + 1);
                })
            }
            Self::Object(fields) => {
                Self::write_sequence(out, indent, fields.len(), '{', '}', |out, index| {
                    let (key, value) = &fields[index];
                    write_json_string(out, key);
                    out.push_str(": ");
                    value.write(out, indent + 1);
                })
            }
        }
    }

    /// Shared array/object rendering: `open`/`close` bracket the same
    /// `{\n  item,\n  item\n}` shape either one uses, so the two `write`
    /// arms above only ever supply what differs (bracket characters and how
    /// to write one element).
    fn write_sequence(
        out: &mut String,
        indent: usize,
        len: usize,
        open: char,
        close: char,
        mut write_item: impl FnMut(&mut String, usize),
    ) {
        if len == 0 {
            out.push(open);
            out.push(close);
            return;
        }
        out.push(open);
        out.push('\n');
        for index in 0..len {
            push_indent(out, indent + 1);
            write_item(out, index);
            if index + 1 != len {
                out.push(',');
            }
            out.push('\n');
        }
        push_indent(out, indent);
        out.push(close);
    }

    fn to_pretty_string(&self) -> String {
        let mut out = String::new();
        self.write(&mut out, 0);
        out.push('\n');
        out
    }
}

fn push_indent(out: &mut String, indent: usize) {
    for _ in 0..indent {
        out.push_str("  ");
    }
}

fn write_json_number(out: &mut String, value: f64) {
    if value.is_finite() && value.fract() == 0.0 && value.abs() < 1e15 {
        #[allow(clippy::cast_possible_truncation)]
        out.push_str(&(value as i64).to_string());
    } else {
        out.push_str(&value.to_string());
    }
}

fn write_json_string(out: &mut String, value: &str) {
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            control if (control as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", control as u32));
            }
            other => out.push(other),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcen_media::{BitDepth, ChromaSubsampling, ColorMatrix, ColorRange};

    fn h265_yuv444_10_full() -> VideoVariant {
        VideoVariant::from_id("hevc-444-10-full-bt709").expect("a real probe-matrix id")
    }

    fn av1_444_row() -> VideoVariant {
        VideoVariant::from_id("av1-444-10-full-bt709").expect("a real probe-matrix id")
    }

    fn av1_420_row() -> VideoVariant {
        VideoVariant::from_id("av1-420-8-full-bt709").expect("a real probe-matrix id")
    }

    // ---- Json ----

    #[test]
    fn json_scalars_render_as_expected() {
        assert_eq!(Json::Null.to_pretty_string(), "null\n");
        assert_eq!(Json::Number(42.0).to_pretty_string(), "42\n");
        assert_eq!(Json::Number(0.5).to_pretty_string(), "0.5\n");
        assert_eq!(Json::str("hi").to_pretty_string(), "\"hi\"\n");
    }

    #[test]
    fn json_strings_escape_quotes_backslashes_and_control_characters() {
        let mut out = String::new();
        write_json_string(&mut out, "a\"b\\c\nd\te");
        assert_eq!(out, "\"a\\\"b\\\\c\\nd\\te\"");
    }

    #[test]
    fn json_empty_array_and_object_render_compactly() {
        assert_eq!(Json::Array(vec![]).to_pretty_string(), "[]\n");
        assert_eq!(Json::Object(vec![]).to_pretty_string(), "{}\n");
    }

    #[test]
    fn json_object_preserves_field_order_and_indents_nested_values() {
        let value = Json::Object(vec![
            ("a", Json::Number(1.0)),
            ("b", Json::Array(vec![Json::Null, Json::str("x")])),
        ]);
        assert_eq!(
            value.to_pretty_string(),
            "{\n  \"a\": 1,\n  \"b\": [\n    null,\n    \"x\"\n  ]\n}\n"
        );
    }

    // ---- fold_attempts_into ----

    fn ok(fps: f64, mbps: f64) -> EncoderAttemptOutcome {
        EncoderAttemptOutcome::Ok {
            sustained_fps: Some(fps),
            bitrate_mbps: Some(mbps),
            note: "test".to_string(),
        }
    }

    #[test]
    fn any_success_wins_the_overall_verdict_and_clears_the_error() {
        let mut finding = RowFinding::new("row".to_string());
        fold_attempts_into(
            &mut finding,
            vec![
                ("NVENC", ok(120.0, 50.0)),
                (
                    "MF",
                    EncoderAttemptOutcome::Unsupported {
                        detail: "MF rejects this chroma".to_string(),
                    },
                ),
            ],
        );
        assert_eq!(finding.encoder_init, ProbeOutcome::Ok);
        assert!(finding.encoder_error.is_none());
        assert_eq!(finding.sustained_fps, Some(120.0));
        assert_eq!(finding.bitrate_mbps, Some(50.0));
        assert!(finding.notes.contains("NVENC: ok"));
        assert!(finding.notes.contains("MF: unsupported"));
    }

    #[test]
    fn a_real_failure_outranks_a_typed_unsupported_refusal() {
        let mut finding = RowFinding::new("row".to_string());
        fold_attempts_into(
            &mut finding,
            vec![
                (
                    "NVENC",
                    EncoderAttemptOutcome::Failed {
                        detail: "NVENC status NV_ENC_ERR_NO_ENCODE_DEVICE".to_string(),
                    },
                ),
                (
                    "MF",
                    EncoderAttemptOutcome::Unsupported {
                        detail: "MF rejects 10-bit".to_string(),
                    },
                ),
            ],
        );
        assert_eq!(finding.encoder_init, ProbeOutcome::Failed);
        assert_eq!(
            finding.encoder_error.as_deref(),
            Some("NVENC status NV_ENC_ERR_NO_ENCODE_DEVICE")
        );
    }

    #[test]
    fn unsupported_alone_is_reported_as_unsupported_with_its_detail() {
        let mut finding = RowFinding::new("row".to_string());
        fold_attempts_into(
            &mut finding,
            vec![(
                "NVENC",
                EncoderAttemptOutcome::Unsupported {
                    detail: "12-bit unsupported".to_string(),
                },
            )],
        );
        assert_eq!(finding.encoder_init, ProbeOutcome::Unsupported);
        assert_eq!(finding.encoder_error.as_deref(), Some("12-bit unsupported"));
    }

    #[test]
    fn not_compiled_alone_leaves_the_row_untested() {
        let mut finding = RowFinding::new("row".to_string());
        fold_attempts_into(
            &mut finding,
            vec![
                (
                    "NVENC",
                    EncoderAttemptOutcome::NotCompiled {
                        detail: "built without --features nvenc".to_string(),
                    },
                ),
                (
                    "MF",
                    EncoderAttemptOutcome::NotCompiled {
                        detail: "built without --features mf".to_string(),
                    },
                ),
            ],
        );
        assert_eq!(finding.encoder_init, ProbeOutcome::Untested);
        assert!(finding.encoder_error.is_none());
        assert!(finding.notes.contains("NVENC: not compiled"));
        assert!(finding.notes.contains("MF: not compiled"));
    }

    // ---- probe_one_row / build_report ----

    #[test]
    fn av1_444_rows_never_reach_the_nvenc_mf_backend_closure() {
        // 4:4:4 AV1 has no NVENC profile or Media Foundation MFT at all --
        // only rav1e encodes it -- so the closure NVENC/MF trials are fed
        // through must never be called for these rows.
        let mut called = false;
        let finding = probe_one_row(av1_444_row(), |_row| {
            called = true;
            vec![]
        });
        assert!(
            !called,
            "4:4:4 AV1 must never reach the NVENC/MF backend closure -- only rav1e encodes it"
        );
        #[cfg(not(feature = "software-av1"))]
        {
            // Without `--features software-av1`, this build genuinely has no
            // rav1e compiled in, so the honest verdict is `untested`.
            assert_eq!(finding.encoder_init, ProbeOutcome::Untested);
            assert!(finding.notes.contains("rav1e"));
            assert!(finding.notes.contains("not compiled"));
        }
        #[cfg(feature = "software-av1")]
        {
            assert_eq!(finding.encoder_init, ProbeOutcome::Ok);
            assert!(finding.notes.contains("rav1e"));
        }
    }

    #[cfg(not(feature = "software-av1"))]
    #[test]
    fn rav1e_attempt_is_not_compiled_without_the_software_av1_feature() {
        assert!(matches!(
            attempt_rav1e_for_row(av1_444_row()),
            EncoderAttemptOutcome::NotCompiled { .. }
        ));
    }

    #[cfg(feature = "software-av1")]
    #[test]
    fn rav1e_attempt_runs_when_the_software_av1_feature_is_enabled() {
        assert!(matches!(
            attempt_rav1e_for_row(av1_444_row()),
            EncoderAttemptOutcome::Ok { .. }
        ));
    }

    #[test]
    fn av1_420_rows_are_handed_to_the_backend_closure() {
        // The hardware-both-ends royalty tier reaches NVENC through exactly
        // the same path H.264/HEVC already use, so a real GPU/driver decides
        // its fate -- never a hardcoded refusal.
        let mut called_with = None;
        let _finding = probe_one_row(av1_420_row(), |row| {
            called_with = Some(row);
            vec![("NVENC", ok(60.0, 10.0))]
        });
        assert_eq!(called_with, Some(av1_420_row()));
    }

    #[test]
    fn h265_rows_are_handed_to_the_backend_closure() {
        let mut called_with = None;
        let _finding = probe_one_row(h265_yuv444_10_full(), |row| {
            called_with = Some(row);
            vec![("NVENC", ok(60.0, 10.0))]
        });
        assert_eq!(called_with, Some(h265_yuv444_10_full()));
    }

    #[test]
    fn build_report_walks_every_row_in_matrix_order() {
        let environment = EnvironmentInfo::new(HostInfo {
            os: "windows".to_string(),
            gpu: "Test GPU".to_string(),
            ..HostInfo::default()
        });
        let report = build_report(environment, |_row| vec![("NVENC", ok(1.0, 1.0))]);
        assert_eq!(report.results.len(), PROBE_MATRIX.len());
        for (row, finding) in PROBE_MATRIX.iter().copied().zip(report.results.iter()) {
            assert_eq!(finding.variant, row.id());
        }
        assert_eq!(report.environments.len(), 1);
    }

    #[test]
    fn rendered_report_contains_every_variant_id_and_the_schema_version() {
        let environment = EnvironmentInfo::new(HostInfo::default());
        let report = build_report(environment, |_row| {
            vec![(
                "NVENC",
                EncoderAttemptOutcome::Unsupported {
                    detail: "test".to_string(),
                },
            )]
        });
        let json = report.render();
        assert!(json.contains("\"schema_version\": 1"));
        assert!(json.contains("\"environments\""));
        assert!(json.contains("\"results\""));
        assert!(json.contains("\"client\": null"));
        for row in PROBE_MATRIX.iter().copied() {
            assert!(json.contains(&row.id()), "missing row {}", row.id());
        }
    }

    // ---- output_path_from_args ----

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn output_path_is_parsed_when_present_and_absent_otherwise() {
        assert_eq!(output_path_from_args(&args(&["probe-matrix"])), None);
        assert_eq!(
            output_path_from_args(&args(&["probe-matrix", "--output", "report.json"])),
            Some(PathBuf::from("report.json"))
        );
        // A dangling flag with no value must not panic or return a bogus path.
        assert_eq!(
            output_path_from_args(&args(&["probe-matrix", "--output"])),
            None
        );
    }

    // ---- timestamp ----

    #[test]
    fn civil_from_days_matches_known_reference_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_716), (2023, 12, 25));
    }

    #[test]
    fn format_utc_timestamp_has_the_documented_shape() {
        let stamp = format_utc_timestamp(UNIX_EPOCH);
        assert_eq!(stamp, "1970-01-01T00:00:00Z");
    }

    // ---- sanity: identity/GBR row is present, matching w2-gbr's target ----

    #[test]
    fn the_identity_row_is_present_and_coherent() {
        let identity = VideoVariant::from_id("hevc-444-10-full-identity")
            .expect("the identity row is a coherent, documented probe-matrix id");
        assert!(PROBE_MATRIX.contains(&identity));
        assert_eq!(identity.video.matrix, ColorMatrix::Identity);
        assert_eq!(identity.video.chroma, ChromaSubsampling::Yuv444);
        assert_eq!(identity.video.bit_depth, BitDepth::Ten);
        assert_eq!(identity.video.range, ColorRange::Full);
    }

    // ---- round-trip request parsing / output shape ----

    #[test]
    fn roundtrip_request_is_none_when_neither_flag_is_given() {
        assert_eq!(parse_roundtrip_request(&args(&["probe-matrix"])), Ok(None));
    }

    #[test]
    fn roundtrip_request_parses_both_flags_together() {
        let parsed = parse_roundtrip_request(&args(&[
            "probe-matrix",
            "--roundtrip-pattern",
            "grey_ramp",
            "--roundtrip-output-dir",
            "out",
        ]))
        .expect("a complete, valid request must parse");
        let request = parsed.expect("both flags were given");
        assert_eq!(request.pattern, TestPattern::GreyRamp);
        assert_eq!(request.output_dir, PathBuf::from("out"));
    }

    #[test]
    fn roundtrip_request_rejects_exactly_one_flag_given() {
        assert_eq!(
            parse_roundtrip_request(&args(&["probe-matrix", "--roundtrip-pattern", "grey_ramp"])),
            Err(RoundtripArgError::IncompleteRequest)
        );
        assert_eq!(
            parse_roundtrip_request(&args(&["probe-matrix", "--roundtrip-output-dir", "out"])),
            Err(RoundtripArgError::IncompleteRequest)
        );
    }

    #[test]
    fn roundtrip_request_rejects_an_unknown_pattern_token() {
        assert_eq!(
            parse_roundtrip_request(&args(&[
                "probe-matrix",
                "--roundtrip-pattern",
                "not_a_pattern",
                "--roundtrip-output-dir",
                "out",
            ])),
            Err(RoundtripArgError::UnknownPattern(
                "not_a_pattern".to_string()
            ))
        );
    }

    #[test]
    fn roundtrip_request_rejects_a_repeated_flag() {
        assert_eq!(
            parse_roundtrip_request(&args(&[
                "probe-matrix",
                "--roundtrip-pattern",
                "grey_ramp",
                "--roundtrip-pattern",
                "chroma_detail",
                "--roundtrip-output-dir",
                "out",
            ])),
            Err(RoundtripArgError::Repeated("--roundtrip-pattern"))
        );
    }

    #[test]
    fn roundtrip_bitstream_path_is_the_bare_variant_id_with_no_extension() {
        let row = h265_yuv444_10_full();
        let path = roundtrip_bitstream_path(Path::new("out"), row);
        assert_eq!(path, PathBuf::from("out").join(row.id()));
        assert_eq!(
            path.extension(),
            None,
            "must match the first filename arcen-deck's candidate_input_paths tries"
        );
    }

    #[test]
    fn roundtrip_meta_path_is_shared_across_every_row() {
        assert_eq!(
            roundtrip_meta_path(Path::new("out")),
            PathBuf::from("out").join("roundtrip-meta.json")
        );
    }

    #[test]
    fn roundtrip_meta_json_records_pattern_and_exact_geometry() {
        let json = roundtrip_meta_json(TestPattern::ChromaDetail, 1920, 1080);
        assert!(json.contains("\"pattern\": \"chroma_detail\""));
        assert!(json.contains("\"width\": 1920"));
        assert!(json.contains("\"height\": 1080"));
    }
}
