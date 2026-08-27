//! `arcen-deck probe-matrix` (`dev-tools` feature) -- the colour-matrix
//! hardware-decode probe.
//!
//! Arcen is adding grader/VFX-grade colour (4:4:4, 10-bit, full range). The
//! single highest-value unknown in that workstream is: does macOS
//! VideoToolbox decode HEVC Rext at ten bits, and in hardware? Apple
//! publishes no per-profile hardware-decode matrix and
//! `VTIsHardwareDecodeSupported` is codec-level only (it cannot distinguish
//! "HEVC" from "HEVC Rext 4:4:4 10-bit"), so the only reliable answer is a
//! real decode session. This subcommand builds one for every row of
//! [`arcen_media::video::PROBE_MATRIX`] and reports what actually happened.
//!
//! # Usage
//!
//! ```text
//! arcen-deck probe-matrix [--parameter-sets <dir>] [--output <path>] \
//!     [--reference-pattern <token>]
//! ```
//!
//! `--parameter-sets <dir>` is a directory of host-produced capture files,
//! one per variant id (see [`candidate_input_paths`] for the filenames
//! tried). Each file is expected to be a real Annex-B elementary stream --
//! parameter sets (VPS/SPS/PPS) plus at least one coded picture -- captured
//! from an actual `capenc variant=<id>` run on the host side. This tool
//! never synthesises parameter sets or picture data itself: doing so would
//! test the parser, not VideoToolbox. A row with no input file (or with
//! parameter sets but no coded picture) is reported `untested`, not
//! invented. `--output <path>` writes the JSON report there instead of
//! stdout.
//!
//! # What every row records
//!
//! Each row builds a `CMVideoFormatDescription` from the file's real
//! VPS/SPS/PPS, requests the `CVPixelBuffer` format
//! [`crate::pipeline::video_decoder::preferred_pixel_format`] resolves for
//! that row's chroma/depth/range, and creates a real
//! `VTDecompressionSession`. It separately attempts a second, throwaway
//! session with
//! `kVTVideoDecoderSpecification_RequireHardwareAcceleratedVideoDecoder`
//! forced to `true`: Apple's documented behaviour is that session creation
//! itself fails outright when that requirement cannot be met, which is the
//! only way to tell "decodes, but only in software" apart from "does not
//! decode here at all" -- both look identical to
//! `kVTDecompressionPropertyKey_UsingHardwareAcceleratedVideoDecoder` on a
//! session created without the requirement. That distinction is folded into
//! `notes` (the JSON schema this emits has no dedicated field for it).
//! `VTRegisterSupplementalVideoDecoderIfAvailable` is called once for both
//! H.264 and HEVC before any row runs, since some decoders are not
//! registered by default.
//!
//! **A row that fails is a finding, not an error.** Every row runs to
//! completion independently; nothing here aborts the whole matrix because
//! one row could not decode.
//!
//! # Output shape
//!
//! The emitted JSON matches the shape of
//! `docs/testing/color-matrix-results.json`: `schema_version`,
//! `environments` (one entry, populated with this Mac's model/chip/macOS
//! version where obtainable -- deliberately *not* including the hostname,
//! which is personally identifying and would otherwise leak into a
//! committed results file), and `results` (one row per
//! [`arcen_media::video::PROBE_MATRIX`] entry, in matrix order). It omits
//! the tracked file's `_comment` and `field_reference` blocks (static
//! documentation for humans, not a finding) and leaves every row's
//! `encoder_init`/`encoder_error` as `untested`/`null`: this subcommand only
//! ever decodes, it never encodes, so it cannot know whether the host's
//! NVENC encoder init succeeded for a row -- the tracked file's own
//! comments describe the host (`capenc variant=<id>`) and Deck
//! (`arcen-deck probe-matrix`) sides as two separate runs whose results are
//! merged, and this is the Deck half.
//!
//! # Known limitations
//!
//! `roundtrip_max_error`/`roundtrip_mean_error` are only computed when
//! `--reference-pattern <token>` is given (naming one of
//! [`arcen_media::test_pattern::TestPattern`]'s tokens): computing them
//! needs to know which deterministic pattern the host encoded, so the
//! reference can be regenerated locally and compared against what actually
//! got decoded. If `<parameter-sets-dir>/roundtrip-meta.json` is present
//! (written by `capenc probe-matrix --roundtrip-pattern <token>
//! --roundtrip-output-dir <dir>`; see `hosts/capenc/src/probe_matrix.rs`'s
//! module doc), its recorded pattern and geometry are cross-checked against
//! `--reference-pattern` and the buffer VideoToolbox actually delivered --
//! a disagreement disables the measurement for the whole run rather than
//! silently reporting a number that would not mean what it looks like (see
//! `resolve_reference_pattern`). Absent both a metadata file and the flag,
//! every row's round-trip fields stay `null`, as before.
//!
//! **This measures a real encode/decode, not pure colour-space
//! arithmetic**: it includes whatever quantisation, chroma subsampling and
//! in-loop filtering the codec itself performed, and is therefore not the
//! same claim as `arcen_media::test_pattern::measure_transform_roundtrip`'s
//! exact, codec-free figure. Conflating the two would let "our colour maths
//! is exact" (true, and proven by that function's own tests) stand in for
//! "the codec is lossless" (false for any lossy encode) -- see that
//! module's doc.
//!
//! `sustained_fps`/`bitrate_mbps`, when present,
//! measure this tool feeding a static file's access units back-to-back as
//! fast as possible -- a real-time-paced sustained-playback measurement
//! would need per-frame timing a static capture file does not carry, and
//! `notes` says so on every row where a rate is reported.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use arcen_media::test_pattern::TestPattern;
use arcen_media::video::{VideoVariant, PROBE_MATRIX};
use arcen_media::{BitDepth, ChromaSubsampling, ColorRange, VideoCodec};
use serde::{Deserialize, Serialize};

use crate::pipeline::video_decoder::{av1, preferred_pixel_format};
use crate::protocol::ChromaSubsampling as WireChroma;

/// `docs/testing/color-matrix-results.json`'s own `schema_version`.
const SCHEMA_VERSION: u32 = 1;

/// Options for the `probe-matrix` subcommand. Both fields are plain,
/// argv-agnostic data -- `main.rs` owns parsing `--parameter-sets`/
/// `--output` out of `std::env::args()`.
///
/// `--reference-pattern <token>` is deliberately **not** a field here: it is
/// parsed directly from `std::env::args()` inside [`execute`] instead (see
/// [`parse_reference_pattern`]'s doc for why).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProbeMatrixOptions {
    /// Directory of host-produced per-variant capture files. `None` means
    /// every row is reported `untested`.
    pub parameter_sets_dir: Option<PathBuf>,
    /// Where to write the JSON report. `None` means the caller should print
    /// the returned string itself.
    pub output_path: Option<PathBuf>,
}

/// Runs every row of [`PROBE_MATRIX`], renders the JSON report, and writes
/// it to `options.output_path` if one was given. Always returns the
/// rendered JSON (even when it was also written to a file) so the caller
/// can decide whether to print it.
///
/// # Errors
///
/// Only for genuine tool failures: JSON serialisation failing (should not
/// happen for this data shape), `--output` naming an unwritable path, or
/// `--reference-pattern` naming an unrecognised token or being repeated.
/// A row that could not decode is never an `Err` here -- it is a `"failed"`
/// entry in the returned JSON, which is the whole point of this tool.
pub fn execute(options: &ProbeMatrixOptions) -> Result<String, String> {
    let process_args: Vec<String> = std::env::args().collect();
    let reference_pattern = parse_reference_pattern(&process_args)?;
    let report = run(options.parameter_sets_dir.as_deref(), reference_pattern);
    let json = render_json(&report)?;
    if let Some(path) = &options.output_path {
        std::fs::write(path, &json)
            .map_err(|error| format!("failed to write {}: {error}", path.display()))?;
    }
    Ok(json)
}

/// Runs every row of [`PROBE_MATRIX`] and assembles the report. Never
/// fails: a row this function cannot probe is recorded as `untested` /
/// `unsupported` rather than propagated as an error.
fn run(
    parameter_sets_dir: Option<&Path>,
    cli_reference_pattern: Option<TestPattern>,
) -> ProbeReport {
    backend::register_supplemental_decoders();
    let roundtrip_meta = parameter_sets_dir.and_then(read_roundtrip_meta);
    let (reference_pattern, mismatch_note) =
        resolve_reference_pattern(cli_reference_pattern, roundtrip_meta);
    let results = PROBE_MATRIX
        .iter()
        .copied()
        .map(|row| {
            let mut finding =
                probe_one_row(row, parameter_sets_dir, reference_pattern, roundtrip_meta);
            if let Some(note) = &mismatch_note {
                finding.push_note(note);
            }
            finding
        })
        .collect();
    ProbeReport {
        schema_version: SCHEMA_VERSION,
        environments: vec![environment_info()],
        results,
    }
}

/// Parses `--reference-pattern <token>` directly out of process argv.
///
/// Unlike `--parameter-sets`/`--output` (owned by `main.rs`, which threads
/// them through [`ProbeMatrixOptions`]), this flag is read here, directly
/// from the argv `execute` is given, because adding a field to
/// `ProbeMatrixOptions` would also require changing how `main.rs`
/// constructs one (it uses an explicit field list there, not struct-update
/// syntax) -- `main.rs` is out of reach for this change. Reading argv a
/// second time, independently, is safe: `std::env::args()` reflects the
/// real process argv regardless of which subset of it `main.rs` chose to
/// thread through `ProbeMatrixOptions`.
///
/// # Errors
///
/// Rejects an unrecognised pattern token or a repeated flag rather than
/// silently defaulting or picking one arbitrarily.
fn parse_reference_pattern(args: &[String]) -> Result<Option<TestPattern>, String> {
    let mut found: Option<&str> = None;
    for (index, argument) in args.iter().enumerate() {
        if argument == "--reference-pattern" {
            if found.is_some() {
                return Err("--reference-pattern may be specified only once".to_string());
            }
            found = args.get(index + 1).map(String::as_str);
        }
    }
    match found {
        None => Ok(None),
        Some(token) => TestPattern::from_token(token)
            .map(Some)
            .ok_or_else(|| format!("unknown --reference-pattern `{token}`")),
    }
}

/// The content of `<parameter-sets-dir>/roundtrip-meta.json`, written by
/// `capenc probe-matrix --roundtrip-pattern <token> --roundtrip-output-dir
/// <dir>` (see `hosts/capenc/src/probe_matrix.rs`'s module doc).
#[derive(Debug, Clone, Deserialize)]
struct RawRoundtripMeta {
    pattern: String,
    width: u32,
    height: u32,
}

/// [`RawRoundtripMeta`] with its pattern token already resolved to a real
/// [`TestPattern`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RoundtripMeta {
    pattern: TestPattern,
    width: u32,
    height: u32,
}

/// Reads and parses `<dir>/roundtrip-meta.json`. `None` if the file is
/// absent, unreadable, malformed, or names a pattern token this build does
/// not recognise -- this is a best-effort cross-check, not a requirement:
/// [`resolve_reference_pattern`] still honours a bare `--reference-pattern`
/// without it.
fn read_roundtrip_meta(dir: &Path) -> Option<RoundtripMeta> {
    let bytes = std::fs::read(dir.join("roundtrip-meta.json")).ok()?;
    let raw: RawRoundtripMeta = serde_json::from_slice(&bytes).ok()?;
    let pattern = TestPattern::from_token(&raw.pattern)?;
    Some(RoundtripMeta {
        pattern,
        width: raw.width,
        height: raw.height,
    })
}

/// Reconciles `--reference-pattern` with `roundtrip-meta.json` (when
/// present) into the pattern that should actually be used for round-trip
/// measurement this run, plus an optional note explaining why it is `None`.
///
/// If both are given and *disagree*, round-trip measurement is disabled
/// entirely for this run (returning `None`) rather than silently trusting
/// one over the other and reporting a figure that would not mean what it
/// looks like. If only one is present, it wins outright: a bare
/// `--reference-pattern` (no metadata file -- e.g. an older `capenc` build,
/// or files copied over without it) still works exactly as this module's
/// doc originally proposed, and a metadata file with no explicit flag is
/// trusted automatically. Geometry is deliberately not compared here: the
/// buffer VideoToolbox actually delivers is what matters, and that is only
/// known per row, after decode (see [`backend::probe_row`]'s own geometry
/// note).
fn resolve_reference_pattern(
    cli_reference_pattern: Option<TestPattern>,
    roundtrip_meta: Option<RoundtripMeta>,
) -> (Option<TestPattern>, Option<String>) {
    match (cli_reference_pattern, roundtrip_meta) {
        (Some(cli), Some(meta)) if cli != meta.pattern => (
            None,
            Some(format!(
                "roundtrip_max_error was not computed: --reference-pattern `{}` disagrees with \
                 roundtrip-meta.json's recorded pattern `{}`; fix whichever one is wrong rather \
                 than trust either blindly",
                cli.token(),
                meta.pattern.token(),
            )),
        ),
        (Some(cli), _) => (Some(cli), None),
        (None, Some(meta)) => (Some(meta.pattern), None),
        (None, None) => (None, None),
    }
}

fn render_json(report: &ProbeReport) -> Result<String, String> {
    serde_json::to_string_pretty(report)
        .map_err(|error| format!("failed to serialise probe-matrix report: {error}"))
}

/// One row of the emitted `results` array. Field names and value
/// vocabulary match `docs/testing/color-matrix-results.json` exactly
/// (`decode`/`encoder_init` are `ok | failed | unsupported | untested`;
/// every other unknown field is JSON `null`, never omitted), plus two
/// fields that file's current schema does not have yet
/// (`roundtrip_mean_error`, `roundtrip_pattern`): additive, and needed to
/// report the round-trip mean alongside the max and to record which
/// pattern was actually used, per this module's doc.
#[derive(Debug, Clone, Serialize)]
struct RowFinding {
    variant: String,
    encoder_init: ProbeOutcome,
    encoder_error: Option<String>,
    decode: ProbeOutcome,
    hardware_decode: Option<bool>,
    delivered_pixel_format: Option<String>,
    color_extensions_attached: Option<String>,
    roundtrip_max_error: Option<f64>,
    roundtrip_mean_error: Option<f64>,
    roundtrip_pattern: Option<String>,
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
            hardware_decode: None,
            delivered_pixel_format: None,
            color_extensions_attached: None,
            roundtrip_max_error: None,
            roundtrip_mean_error: None,
            roundtrip_pattern: None,
            sustained_fps: None,
            bitrate_mbps: None,
            notes: String::new(),
        }
    }

    /// Appends `text` to `notes`, separating existing notes with `"; "` so
    /// every explanation this tool accumulates for a row survives, rather
    /// than the last one overwriting the rest.
    fn push_note(&mut self, text: &str) {
        if !self.notes.is_empty() {
            self.notes.push_str("; ");
        }
        self.notes.push_str(text);
    }
}

/// `docs/testing/color-matrix-results.json`'s `decode`/`encoder_init`
/// value vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum ProbeOutcome {
    Ok,
    Failed,
    Unsupported,
    Untested,
}

#[derive(Debug, Clone, Serialize)]
struct ClientInfo {
    model: Option<String>,
    chip: Option<String>,
    macos_version: Option<String>,
    display: Option<String>,
    reference_mode: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct EnvironmentInfo {
    environment_id: String,
    /// Always `null`: host-side fields (`os`/`gpu`/`driver_version`/
    /// `nvenc_generation`) are for the `capenc`-side run to fill in when
    /// its results are merged with this file, not something the Deck side
    /// can observe.
    host: Option<serde_json::Value>,
    client: ClientInfo,
    recorded_at: String,
    arcen_commit: String,
}

#[derive(Debug, Clone, Serialize)]
struct ProbeReport {
    schema_version: u32,
    environments: Vec<EnvironmentInfo>,
    results: Vec<RowFinding>,
}

fn environment_info() -> EnvironmentInfo {
    let client = backend::client_environment();
    // Deliberately built from model + macOS version rather than hostname:
    // a hostname (e.g. "andreas-mbp.local") is personally identifying and
    // this identifier ends up in a file the tool's caller may commit.
    let environment_id = format!(
        "{}-{}",
        client.model.as_deref().unwrap_or("unknown-model"),
        client.macos_version.as_deref().unwrap_or("unknown-macos"),
    );
    EnvironmentInfo {
        environment_id,
        host: None,
        client,
        recorded_at: format_utc_timestamp(SystemTime::now()),
        arcen_commit: option_env!("ARCEN_SOURCE_REVISION")
            .unwrap_or("unknown")
            .to_string(),
    }
}

/// Runs the whole per-row pipeline: locate input, parse it, and (if
/// possible) hand it to [`backend::probe_row`] (H.264/H.265) or
/// [`probe_one_av1_row`] (AV1). Every early return here or in
/// [`probe_one_av1_row`] is itself a finding recorded on `finding`, never a
/// panic or a propagated error, so [`run`] can map this over every row
/// unconditionally.
fn probe_one_row(
    row: VideoVariant,
    parameter_sets_dir: Option<&Path>,
    reference_pattern: Option<TestPattern>,
    roundtrip_meta: Option<RoundtripMeta>,
) -> RowFinding {
    let id = row.id();
    let mut finding = RowFinding::new(id.clone());

    if row.video.codec == VideoCodec::Av1 {
        probe_one_av1_row(
            &row,
            &id,
            &mut finding,
            parameter_sets_dir,
            reference_pattern,
            roundtrip_meta,
        );
        return finding;
    }

    let Some(codec) = ProbeCodec::from_media_codec(row.video.codec) else {
        finding.decode = ProbeOutcome::Unsupported;
        finding.push_note(
            "VP9/JPEG have no CMVideoFormatDescriptionCreateFromParameterSets entry point in \
             the vendored apple-cf bindings this tool uses, so this row is not attempted here",
        );
        return finding;
    };

    let Some(dir) = parameter_sets_dir else {
        finding.push_note("no --parameter-sets directory was supplied; every row is untested");
        return finding;
    };
    let Some(path) = find_input_file(dir, &id) else {
        finding.push_note(&format!(
            "no parameter-set input found for `{id}` under {} (tried: no extension, .bin, \
             .annexb, .h264, .hevc, .264, .265, .obu)",
            dir.display(),
        ));
        return finding;
    };
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) => {
            finding.push_note(&format!("failed to read {}: {error}", path.display()));
            return finding;
        }
    };

    let nals = split_annex_b(&bytes);
    if nals.is_empty() {
        finding.push_note(&format!(
            "{} contained no Annex-B NAL units",
            path.display()
        ));
        return finding;
    }

    let params = latest_parameter_sets(&nals, codec);
    if !params.is_complete(codec) {
        finding.push_note(&format!(
            "{} lacked a complete parameter set for {codec:?} (need {})",
            path.display(),
            codec.required_parameter_sets_description(),
        ));
        return finding;
    }

    let access_units = group_access_units(&nals, codec);
    let requested_pixel_format = requested_pixel_format_for(&row);
    backend::probe_row(
        &mut finding,
        codec,
        &params,
        &access_units,
        requested_pixel_format,
        row,
        reference_pattern,
        roundtrip_meta,
    );
    finding
}

/// AV1 half of [`probe_one_row`]: unlike H.264/H.265, AV1 has no
/// `ProbeCodec` (see that enum's doc) and uses OBU framing rather than
/// Annex-B, so it cannot share the pipeline above. Mutates `finding` in
/// place and only ever returns early, exactly like every other branch of
/// `probe_one_row` -- never a panic or a propagated error.
fn probe_one_av1_row(
    row: &VideoVariant,
    id: &str,
    finding: &mut RowFinding,
    parameter_sets_dir: Option<&Path>,
    reference_pattern: Option<TestPattern>,
    roundtrip_meta: Option<RoundtripMeta>,
) {
    let Some(dir) = parameter_sets_dir else {
        finding.push_note("no --parameter-sets directory was supplied; every row is untested");
        return;
    };
    let Some(path) = find_input_file(dir, id) else {
        finding.push_note(&format!(
            "no parameter-set input found for `{id}` under {} (tried: no extension, .bin, \
             .annexb, .h264, .hevc, .264, .265, .obu)",
            dir.display(),
        ));
        return;
    };
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) => {
            finding.push_note(&format!("failed to read {}: {error}", path.display()));
            return;
        }
    };
    let obus = match av1::parse_obus(&bytes) {
        Ok(obus) => obus,
        Err(error) => {
            finding.decode = ProbeOutcome::Failed;
            finding.push_note(&format!(
                "{} did not parse as AV1 OBUs: {error}",
                path.display()
            ));
            return;
        }
    };
    let Some(sequence_header) = obus
        .iter()
        .find(|obu| obu.obu_type == av1::ObuType::SequenceHeader)
    else {
        finding.push_note(&format!(
            "{} contained no AV1 Sequence Header OBU",
            path.display()
        ));
        return;
    };
    let fields = match av1::parse_sequence_header(sequence_header.payload) {
        Ok(fields) => fields,
        Err(error) => {
            finding.decode = ProbeOutcome::Failed;
            finding.push_note(&format!(
                "failed to parse the AV1 Sequence Header OBU: {error}"
            ));
            return;
        }
    };
    let temporal_units = av1_temporal_units(&obus);
    let requested_pixel_format = requested_pixel_format_for(row);
    backend::probe_av1_row(
        finding,
        &fields,
        sequence_header.whole,
        &temporal_units,
        requested_pixel_format,
        *row,
        reference_pattern,
        roundtrip_meta,
    );
}

/// Groups a whole host-captured AV1 file's OBUs (in bitstream order) into
/// per-Temporal-Unit sample buffers, mirroring [`group_access_units`]'s
/// role for the Annex-B codecs but split on
/// [`av1::ObuType::TemporalDelimiter`] rather than on VCL NAL boundaries
/// (av1-spec: a Temporal Delimiter OBU begins each Temporal Unit in the
/// Low Overhead Bitstream Format). `OBU_TEMPORAL_DELIMITER`,
/// `OBU_PADDING` and `OBU_REDUNDANT_FRAME_HEADER` are dropped from each
/// unit's bytes, per AOMediaCodec/av1-isobmff `index.bs`'s "AV1 Sample
/// Format" ("SHOULD NOT be used" in a sample) -- every other OBU's real
/// bytes ([`av1::Obu::whole`]) are concatenated verbatim, since AV1 needs
/// no AVCC-style re-framing (unlike H.264/HEVC's `nals_to_avcc`).
///
/// A file with no Temporal Delimiter OBU at all is treated as one single
/// Temporal Unit rather than zero -- a capture missing delimiters is
/// still real, decodable data, and reporting nothing decodable would be
/// invented pessimism, not a finding.
fn av1_temporal_units(obus: &[av1::Obu<'_>]) -> Vec<Vec<u8>> {
    let mut units: Vec<Vec<u8>> = Vec::new();
    let mut current: Vec<u8> = Vec::new();
    let mut current_has_content = false;
    for obu in obus {
        if obu.obu_type == av1::ObuType::TemporalDelimiter {
            if current_has_content {
                units.push(std::mem::take(&mut current));
                current_has_content = false;
            }
            continue;
        }
        if matches!(
            obu.obu_type,
            av1::ObuType::Padding | av1::ObuType::RedundantFrameHeader
        ) {
            continue;
        }
        current.extend_from_slice(obu.whole);
        current_has_content = true;
    }
    if current_has_content {
        units.push(current);
    }
    units
}

/// The two Annex-B wire codecs this tool builds a real
/// `CMVideoFormatDescription` for via `CMVideoFormatDescriptionCreateFrom*
/// ParameterSets`. VP9/JPEG have no such entry point in the vendored
/// `apple-cf` bindings and are reported `unsupported`. AV1 *also* has no
/// such entry point, but is not `ProbeCodec::None` here: it has its own
/// OBU-framed path (see [`probe_one_av1_row`]/[`av1`]) built on
/// `CMVideoFormatDescriptionCreate` plus an `av1C` extension instead, so
/// it never reaches this enum at all rather than mapping to `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProbeCodec {
    H264,
    H265,
}

impl ProbeCodec {
    fn from_media_codec(codec: VideoCodec) -> Option<Self> {
        match codec {
            VideoCodec::H264 => Some(Self::H264),
            VideoCodec::H265 => Some(Self::H265),
            VideoCodec::Jpeg | VideoCodec::Vp9 | VideoCodec::Av1 => None,
        }
    }

    fn required_parameter_sets_description(self) -> &'static str {
        match self {
            Self::H264 => "SPS+PPS",
            Self::H265 => "VPS+SPS+PPS",
        }
    }
}

/// What one Annex-B NAL unit is, for the purpose of building an access unit
/// and a `CMVideoFormatDescription`. Slice/Keyframe are VCL (picture data);
/// everything else is non-VCL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProbeNalKind {
    Slice,
    Keyframe,
    Vps,
    Sps,
    Pps,
    Other,
}

/// Classifies one NAL by its header byte(s). Mirrors
/// `crate::pipeline::video_decoder::nal_kind` (private to that module's own
/// `NalKind`/wire pipeline) since the same H.264/H.265 NAL header rules
/// apply to a standalone capture file as to a wire access unit.
fn classify_nal(codec: ProbeCodec, nal: &[u8]) -> ProbeNalKind {
    match codec {
        ProbeCodec::H264 => match nal.first().map(|byte| byte & 0x1f) {
            Some(1) => ProbeNalKind::Slice,
            Some(5) => ProbeNalKind::Keyframe,
            Some(7) => ProbeNalKind::Sps,
            Some(8) => ProbeNalKind::Pps,
            _ => ProbeNalKind::Other,
        },
        // H.265 NAL type lives in bits 6..1 of the first header byte.
        ProbeCodec::H265 => match nal.first().map(|byte| (byte >> 1) & 0x3f) {
            Some(32) => ProbeNalKind::Vps,
            Some(33) => ProbeNalKind::Sps,
            Some(34) => ProbeNalKind::Pps,
            // IRAP pictures (BLA 16-18, IDR 19-20, CRA 21) are decoder entry
            // points -- the H.265 analogue of an H.264 IDR.
            Some(nal_type) if (16..=21).contains(&nal_type) => ProbeNalKind::Keyframe,
            Some(nal_type) if nal_type <= 31 => ProbeNalKind::Slice,
            _ => ProbeNalKind::Other,
        },
    }
}

/// Finds the next Annex-B start code (`00 00 01` or `00 00 00 01`) at or
/// after `from`, returning its offset and length.
fn find_start_code(data: &[u8], from: usize) -> Option<(usize, usize)> {
    let mut index = from;
    while index + 3 <= data.len() {
        if data[index..].starts_with(&[0, 0, 1]) {
            return Some((index, 3));
        }
        if index + 4 <= data.len() && data[index..].starts_with(&[0, 0, 0, 1]) {
            return Some((index, 4));
        }
        index += 1;
    }
    None
}

/// Trims trailing zero padding (a NAL that ends right at EOF, or right
/// before the next start code, often carries zero bytes that belong to
/// that start code's own prefix) and pushes the remainder, unless trimming
/// leaves nothing -- a NAL that is entirely zero padding is not a NAL.
fn push_trimmed_nal<'a>(out: &mut Vec<&'a [u8]>, nal: &'a [u8]) {
    let end = nal
        .iter()
        .rposition(|byte| *byte != 0)
        .map_or(0, |index| index + 1);
    if end != 0 {
        out.push(&nal[..end]);
    }
}

/// Splits a whole host-produced capture file into Annex-B NAL units.
/// Mirrors `crate::pipeline::video_decoder`'s private `annex_b_nals`/
/// `find_start_code` (that module parses one wire access unit at a time;
/// this tool parses a whole file), reimplemented here rather than imported
/// since those are private helpers of that module's own `platform`
/// pipeline. A buffer with no start code at all is treated as one bare NAL
/// (e.g. a lone raw parameter set with no Annex-B prefix) rather than
/// nothing, matching that same convention.
fn split_annex_b(data: &[u8]) -> Vec<&[u8]> {
    let Some((mut start_code, mut start_code_len)) = find_start_code(data, 0) else {
        return (!data.is_empty()).then_some(data).into_iter().collect();
    };

    let mut out = Vec::new();
    loop {
        let nal_start = start_code + start_code_len;
        let Some((next_start_code, next_start_code_len)) = find_start_code(data, nal_start) else {
            push_trimmed_nal(&mut out, &data[nal_start..]);
            break;
        };
        push_trimmed_nal(&mut out, &data[nal_start..next_start_code]);
        start_code = next_start_code;
        start_code_len = next_start_code_len;
    }

    out
}

/// Length-prefixes each NAL with a big-endian `u32`, the AVCC/HVCC framing
/// `VTDecompressionSessionDecodeFrame` expects for a
/// `CMVideoFormatDescriptionCreateFrom*ParameterSets`-built session.
fn nals_to_avcc(nals: &[&[u8]]) -> Vec<u8> {
    let total = nals.iter().map(|nal| 4 + nal.len()).sum();
    let mut out = Vec::with_capacity(total);
    for nal in nals {
        out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        out.extend_from_slice(nal);
    }
    out
}

/// The most recently observed VPS/SPS/PPS across a whole input file.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct ParameterSets<'a> {
    vps: Option<&'a [u8]>,
    sps: Option<&'a [u8]>,
    pps: Option<&'a [u8]>,
}

impl ParameterSets<'_> {
    fn is_complete(&self, codec: ProbeCodec) -> bool {
        match codec {
            ProbeCodec::H264 => self.sps.is_some() && self.pps.is_some(),
            ProbeCodec::H265 => self.vps.is_some() && self.sps.is_some() && self.pps.is_some(),
        }
    }
}

/// Scans every NAL in the file and keeps the latest of each parameter-set
/// kind. A single-variant capture is not expected to change its parameter
/// sets mid-stream, so "latest anywhere in the file" and "in effect at any
/// given access unit" coincide in practice; this is what
/// `build_format_description` is built from up front.
fn latest_parameter_sets<'a>(nals: &[&'a [u8]], codec: ProbeCodec) -> ParameterSets<'a> {
    let mut sets = ParameterSets::default();
    for nal in nals {
        match classify_nal(codec, nal) {
            ProbeNalKind::Vps => sets.vps = Some(nal),
            ProbeNalKind::Sps => sets.sps = Some(nal),
            ProbeNalKind::Pps => sets.pps = Some(nal),
            _ => {}
        }
    }
    sets
}

/// One coded picture's worth of NALs, as found in the file (parameter sets
/// this unit happened to carry inline are included in `nals` too;
/// [`access_unit_sample_bytes`] filters those back out and prepends the
/// cached [`ParameterSets`] instead, so a unit's parameter sets are never
/// duplicated regardless of whether it carried them inline).
#[derive(Debug, Clone, PartialEq, Eq)]
struct AccessUnit<'a> {
    nals: Vec<&'a [u8]>,
    is_keyframe: bool,
}

/// Groups a file's NALs into access units. A VCL (Slice/Keyframe) NAL
/// always closes the unit it belongs to; every other kind (parameter sets,
/// SEI, ...) accumulates for whichever VCL NAL comes next. Trailing
/// parameter sets with no following VCL NAL describe nothing decodable and
/// are dropped rather than kept as a bogus unit.
fn group_access_units<'a>(nals: &[&'a [u8]], codec: ProbeCodec) -> Vec<AccessUnit<'a>> {
    let mut units = Vec::new();
    let mut pending: Vec<&[u8]> = Vec::new();
    for nal in nals {
        pending.push(nal);
        // A Keyframe NAL closes its own unit in this same arm, so by the
        // time a Slice NAL is seen, `pending` can never already contain one.
        match classify_nal(codec, nal) {
            ProbeNalKind::Keyframe => {
                units.push(AccessUnit {
                    nals: std::mem::take(&mut pending),
                    is_keyframe: true,
                });
            }
            ProbeNalKind::Slice => {
                units.push(AccessUnit {
                    nals: std::mem::take(&mut pending),
                    is_keyframe: false,
                });
            }
            _ => {}
        }
    }
    units
}

/// Builds one access unit's AVCC sample payload: the cached VPS/SPS/PPS
/// prepended only for a keyframe unit (never duplicated -- any parameter
/// sets the unit carried inline are filtered back out first), followed by
/// its VCL NALs.
fn access_unit_sample_bytes(
    unit: &AccessUnit<'_>,
    params: &ParameterSets<'_>,
    codec: ProbeCodec,
) -> Vec<u8> {
    let mut nals: Vec<&[u8]> = Vec::new();
    if unit.is_keyframe {
        if codec == ProbeCodec::H265 {
            if let Some(vps) = params.vps {
                nals.push(vps);
            }
        }
        if let Some(sps) = params.sps {
            nals.push(sps);
        }
        if let Some(pps) = params.pps {
            nals.push(pps);
        }
    }
    nals.extend(unit.nals.iter().copied().filter(|nal| {
        matches!(
            classify_nal(codec, nal),
            ProbeNalKind::Slice | ProbeNalKind::Keyframe
        )
    }));
    nals_to_avcc(&nals)
}

/// Converts `arcen_media`'s [`ChromaSubsampling`] (what [`VideoVariant`]
/// rows carry) to `arcen_protocol`'s identically-shaped but distinct type
/// of the same name, which
/// [`preferred_pixel_format`](crate::pipeline::video_decoder::preferred_pixel_format)
/// expects. A plain value mapping, not a cast -- the two crates each define
/// their own `ChromaSubsampling`.
fn wire_chroma(chroma: ChromaSubsampling) -> WireChroma {
    match chroma {
        ChromaSubsampling::Yuv420 => WireChroma::Yuv420,
        ChromaSubsampling::Yuv422 => WireChroma::Yuv422,
        ChromaSubsampling::Yuv444 => WireChroma::Yuv444,
    }
}

/// The `CVPixelBuffer` FourCC this row's negotiated chroma/depth/range
/// resolves to, via the same [`preferred_pixel_format`] the production
/// decoder uses -- the row-to-pixel-format decision every stream in the
/// product goes through, not a probe-only shortcut.
fn requested_pixel_format_for(row: &VideoVariant) -> Option<u32> {
    let ten_bit = matches!(row.video.bit_depth, BitDepth::Ten | BitDepth::Twelve);
    let full_range = matches!(row.video.range, ColorRange::Full);
    preferred_pixel_format(wire_chroma(row.video.chroma), ten_bit, full_range)
}

/// Renders a `CVPixelBuffer`/`CMVideoCodecType` FourCC as its four ASCII
/// characters, e.g. `0x78663434` -> `"xf44"`.
fn fourcc_string(format: u32) -> String {
    String::from_utf8_lossy(&format.to_be_bytes()).into_owned()
}

/// Days since the Unix epoch to a proleptic-Gregorian `(year, month, day)`.
/// Howard Hinnant's public-domain `civil_from_days` algorithm
/// (<https://howardhinnant.github.io/date_algorithms.html>), transcribed
/// with an explicit per-branch cast (rather than casting the whole
/// `if`/`else` result) so there is no reliance on operator-precedence
/// between `as` and a block expression. Reproduced here rather than pulled
/// in as a new Cargo dependency, since this diagnostic tool's `recorded_at`
/// timestamp is the only place that needs it.
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

/// Formats a [`SystemTime`] as a UTC `YYYY-MM-DDTHH:MM:SSZ` timestamp for
/// the report's `recorded_at` field.
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

/// Filenames this tool looks for inside `--parameter-sets <dir>`, tried in
/// this order, for one variant id. The host side (`capenc`) is expected to
/// produce a real capture per variant under one of these names: Annex-B
/// for H.264/H.265, or a raw "Low Overhead Bitstream Format" OBU stream
/// (av1-spec section 5.2) for AV1 -- `.obu` is the conventional extension
/// for the latter (matching `aomdec`/`libaom`'s own test-file convention).
fn candidate_input_paths(dir: &Path, id: &str) -> Vec<PathBuf> {
    [
        "", ".bin", ".annexb", ".h264", ".hevc", ".264", ".265", ".obu",
    ]
    .iter()
    .map(|suffix| dir.join(format!("{id}{suffix}")))
    .collect()
}

fn find_input_file(dir: &Path, id: &str) -> Option<PathBuf> {
    candidate_input_paths(dir, id)
        .into_iter()
        .find(|path| path.is_file())
}

#[cfg(target_os = "macos")]
mod backend {
    use std::ffi::c_void;
    use std::ptr;
    use std::sync::mpsc;
    use std::time::Instant;

    use apple_cf::cf::{CFData, CFDictionary, CFNumber, CFString, CFType};
    use apple_cf::cm::{CMBlockBuffer, CMFormatDescription, CMSampleBuffer, CMTime};
    use apple_cf::cv::CVPixelBuffer;
    use apple_cf::raw;
    use videotoolbox::{
        ffi, register_supplemental_video_decoder_if_available, Codec, DecompressionSession,
    };

    use arcen_media::test_pattern::measure_rgb_roundtrip;
    use arcen_media::video::ColorTransform;

    use super::{
        av1, AccessUnit, ClientInfo, ParameterSets, ProbeCodec, ProbeOutcome, RoundtripMeta,
        RowFinding, TestPattern, VideoVariant,
    };

    /// `CMSampleTimingInfo` timescale used for every probe sample. This
    /// tool never inspects presentation timing, so the exact value is
    /// irrelevant as long as it is a valid, non-zero timescale.
    const FRAME_TIMESCALE: i32 = 1_000;

    /// Calls `VTRegisterSupplementalVideoDecoderIfAvailable` for both
    /// codecs this tool probes, once, before any row runs. Apple documents
    /// this as a safe, idempotent opt-in that is a no-op when no
    /// supplemental decoder exists for the codec on this system; some
    /// decoders are not registered by default. The vendored `videotoolbox`
    /// crate exposes a safe wrapper over the raw
    /// `ffi::VTRegisterSupplementalVideoDecoderIfAvailable` (confirmed
    /// present in `videotoolbox-0.18.1/src/utilities/mod.rs`, re-exported
    /// at the crate root). The task only asked for HEVC; this also covers
    /// H.264 defensively, since the call is documented as harmless when
    /// inapplicable.
    pub(super) fn register_supplemental_decoders() {
        register_supplemental_video_decoder_if_available(Codec::H264);
        register_supplemental_video_decoder_if_available(Codec::HEVC);
    }

    /// Mac model / chip / macOS version for the report's
    /// `environments[].client` entry. Each field is `None` (JSON `null`)
    /// when the underlying query failed, rather than a fabricated
    /// placeholder string.
    pub(super) fn client_environment() -> ClientInfo {
        ClientInfo {
            model: sysctl_string("hw.model"),
            chip: sysctl_string("machdep.cpu.brand_string"),
            macos_version: Some(
                objc2_foundation::NSProcessInfo::processInfo()
                    .operatingSystemVersionString()
                    .to_string(),
            ),
            display: None,
            reference_mode: None,
        }
    }

    /// Reads a `sysctlbyname` string value using the documented two-call
    /// idiom: first query the required buffer length with a NULL
    /// destination, then read into a buffer of exactly that length.
    fn sysctl_string(name: &str) -> Option<String> {
        let c_name = std::ffi::CString::new(name).ok()?;
        let mut len: usize = 0;
        // SAFETY: a NULL `oldp` with a valid `oldlenp` only queries the
        // required buffer length; `sysctlbyname` writes nothing through a
        // NULL destination pointer.
        let probed = unsafe {
            libc::sysctlbyname(
                c_name.as_ptr(),
                ptr::null_mut(),
                &mut len,
                ptr::null_mut(),
                0,
            )
        };
        if probed != 0 || len == 0 {
            return None;
        }
        let mut buffer = vec![0_u8; len];
        // SAFETY: `buffer` is sized to exactly the length the identical
        // call just reported, so `sysctlbyname` cannot write past it.
        let filled = unsafe {
            libc::sysctlbyname(
                c_name.as_ptr(),
                buffer.as_mut_ptr().cast::<c_void>(),
                &mut len,
                ptr::null_mut(),
                0,
            )
        };
        if filled != 0 {
            return None;
        }
        buffer.truncate(len);
        // `sysctlbyname` string results are NUL-terminated within `len`.
        while buffer.last() == Some(&0) {
            buffer.pop();
        }
        String::from_utf8(buffer).ok()
    }

    /// Runs the real VideoToolbox probe for one row, mutating `finding`
    /// with everything [`super::probe_one_row`] could not determine from
    /// parsing alone. Never panics and never propagates an error: every
    /// failure mode this function can hit is itself a finding, recorded
    /// into `finding`'s fields and `notes`.
    ///
    /// `row`/`reference_pattern`/`roundtrip_meta` are only used for the
    /// round-trip measurement (see this module's doc): `row` supplies the
    /// negotiated matrix/range/bit depth needed to invert whatever the
    /// decoded planes actually carry, `reference_pattern` is the pattern to
    /// regenerate and compare against, and `roundtrip_meta`, when present,
    /// is cross-checked against what was actually decoded so a geometry
    /// mismatch is recorded rather than silently ignored.
    pub(super) fn probe_row(
        finding: &mut RowFinding,
        codec: ProbeCodec,
        params: &ParameterSets<'_>,
        access_units: &[AccessUnit<'_>],
        requested_pixel_format: Option<u32>,
        row: VideoVariant,
        reference_pattern: Option<TestPattern>,
        roundtrip_meta: Option<RoundtripMeta>,
    ) {
        let format = match build_format_description(codec, params) {
            Ok(format) => format,
            Err(error) => {
                finding.decode = ProbeOutcome::Failed;
                finding.push_note(&error);
                return;
            }
        };

        finding.color_extensions_attached = Some(describe_color_extensions(&format));

        let attributes = requested_pixel_format.and_then(build_destination_attributes);

        match probe_requires_hardware(&format, attributes.as_ref()) {
            Ok(()) => finding.push_note(
                "hardware-required probe: VTDecompressionSessionCreate succeeded with \
                 kVTVideoDecoderSpecification_RequireHardwareAcceleratedVideoDecoder=true",
            ),
            Err((status, error)) => finding.push_note(&format!(
                "hardware-required probe: {error} (OSStatus {status}); if the unforced session \
                 below still decodes, this Mac is falling back to SOFTWARE for this format \
                 rather than genuinely lacking support for it",
            )),
        }

        let (tx, rx) = mpsc::channel();
        let session = match DecompressionSession::new_with_image_buffer_attributes(
            &format,
            attributes.as_ref(),
            move |frame| {
                let _ = tx.send(frame);
            },
        ) {
            Ok(session) => session,
            Err(error) => {
                finding.decode = ProbeOutcome::Failed;
                finding.push_note(&format!("VTDecompressionSessionCreate: {error}"));
                return;
            }
        };
        if let Err(error) = session.set_real_time(true) {
            finding.push_note(&format!(
                "VTSessionSetProperty(RealTime) failed (non-fatal): {error}"
            ));
        }

        finding.hardware_decode = query_hardware_accelerated(&session);

        if access_units.is_empty() {
            finding.push_note(
                "parameter sets were present but the input contained no coded picture (VCL) \
                 NAL units; the session above was created but decode was not exercised",
            );
            return;
        }

        let mut submitted_bytes: u64 = 0;
        let mut decode_error: Option<String> = None;
        let start = Instant::now();
        for unit in access_units {
            let sample_bytes = super::access_unit_sample_bytes(unit, params, codec);
            submitted_bytes += sample_bytes.len() as u64;
            let sample = match make_sample_buffer(&format, &sample_bytes, 0) {
                Ok(sample) => sample,
                Err(error) => {
                    decode_error = Some(error);
                    break;
                }
            };
            if let Err(error) = session.decode(&sample) {
                decode_error = Some(format!("VTDecompressionSessionDecodeFrame: {error}"));
                break;
            }
        }
        if let Err(error) = session.wait_for_async_frames() {
            decode_error.get_or_insert(format!(
                "VTDecompressionSessionWaitForAsynchronousFrames: {error}"
            ));
        }
        let elapsed = start.elapsed();
        drop(session);

        let mut frames = Vec::new();
        while let Ok(frame) = rx.try_recv() {
            frames.push(frame);
        }
        let decoded_count = frames
            .iter()
            .filter(|frame| frame.status == 0 && frame.image_buffer.is_some())
            .count();
        let last_delivered_format = frames.iter().rev().find_map(|frame| {
            if frame.status != 0 {
                return None;
            }
            frame
                .image_buffer
                .as_ref()
                .map(|buffer| super::fourcc_string(buffer.pixel_format()))
        });
        // Kept separately from `last_delivered_format` (a `String`) because
        // the round-trip measurement below needs the actual pixels, not
        // just their FourCC.
        let last_image_buffer: Option<CVPixelBuffer> = frames.iter().rev().find_map(|frame| {
            if frame.status != 0 {
                return None;
            }
            frame.image_buffer.clone()
        });

        if decoded_count == 0 {
            finding.decode = ProbeOutcome::Failed;
            match decode_error {
                Some(error) => finding.push_note(&error),
                None => finding.push_note("no frame was delivered by the decode callback"),
            }
            return;
        }

        finding.decode = ProbeOutcome::Ok;
        finding.delivered_pixel_format = last_delivered_format;

        if let Some(pattern) = reference_pattern {
            match last_image_buffer {
                Some(buffer) => {
                    record_roundtrip_accuracy(finding, &buffer, row, pattern, roundtrip_meta)
                }
                None => finding.push_note(
                    "roundtrip_max_error was not computed: a frame decoded (status == 0) but \
                     carried no image buffer",
                ),
            }
        }

        let elapsed_secs = elapsed.as_secs_f64();
        if decoded_count >= 2 && elapsed_secs > 0.0 {
            finding.sustained_fps = Some(decoded_count as f64 / elapsed_secs);
            finding.bitrate_mbps =
                Some((submitted_bytes as f64 * 8.0) / elapsed_secs / 1_000_000.0);
            finding.push_note(&format!(
                "sustained_fps/bitrate_mbps are a tight decode-loop measurement over {} access \
                 unit(s) fed back-to-back from a static file, not paced real-time playback -- a \
                 static capture carries no per-frame timing to pace against",
                access_units.len(),
            ));
        } else {
            finding.push_note(&format!(
                "only {decoded_count} frame(s) decoded from this input; not enough to measure a \
                 rate",
            ));
        }
        let failed_units = frames.len().saturating_sub(decoded_count);
        if failed_units > 0 {
            finding.push_note(&format!(
                "{failed_units} of {} submitted access unit(s) did not decode cleanly",
                frames.len(),
            ));
        }
    }

    /// AV1 counterpart of [`probe_row`]: builds the format description from
    /// a real, host-encoder-produced Sequence Header OBU (`fields`/
    /// `sequence_header_obu`, extracted by [`super::probe_one_av1_row`])
    /// via [`build_av1_format_description`], then reuses every codec-
    /// agnostic step `probe_row` already established --
    /// `describe_color_extensions`, `probe_requires_hardware`,
    /// `query_hardware_accelerated`, `make_sample_buffer`,
    /// `record_roundtrip_accuracy` -- unchanged. Only sample preparation
    /// differs: each of `temporal_units` is already a complete, ready-to-
    /// submit Temporal Unit's raw OBU bytes (see
    /// `super::av1_temporal_units`), so there is no AVCC-style re-framing
    /// step here at all.
    ///
    /// Never panics and never propagates an error: every failure mode this
    /// function can hit is itself a finding, recorded into `finding`'s
    /// fields and `notes`, exactly like [`probe_row`].
    pub(super) fn probe_av1_row(
        finding: &mut RowFinding,
        fields: &super::av1::SequenceHeaderFields,
        sequence_header_obu: &[u8],
        temporal_units: &[Vec<u8>],
        requested_pixel_format: Option<u32>,
        row: VideoVariant,
        reference_pattern: Option<TestPattern>,
        roundtrip_meta: Option<RoundtripMeta>,
    ) {
        let format = match build_av1_format_description(fields, sequence_header_obu) {
            Ok(format) => format,
            Err(error) => {
                finding.decode = ProbeOutcome::Failed;
                finding.push_note(&error);
                return;
            }
        };

        finding.color_extensions_attached = Some(describe_color_extensions(&format));

        let attributes = requested_pixel_format.and_then(build_destination_attributes);

        match probe_requires_hardware(&format, attributes.as_ref()) {
            Ok(()) => finding.push_note(
                "hardware-required probe: VTDecompressionSessionCreate succeeded with \
                 kVTVideoDecoderSpecification_RequireHardwareAcceleratedVideoDecoder=true",
            ),
            Err((status, error)) => finding.push_note(&format!(
                "hardware-required probe: {error} (OSStatus {status}); if the unforced session \
                 below still decodes, this Mac is falling back to SOFTWARE for this format \
                 rather than genuinely lacking support for it",
            )),
        }

        let (tx, rx) = mpsc::channel();
        let session = match DecompressionSession::new_with_image_buffer_attributes(
            &format,
            attributes.as_ref(),
            move |frame| {
                let _ = tx.send(frame);
            },
        ) {
            Ok(session) => session,
            Err(error) => {
                finding.decode = ProbeOutcome::Failed;
                finding.push_note(&format!("VTDecompressionSessionCreate: {error}"));
                return;
            }
        };
        if let Err(error) = session.set_real_time(true) {
            finding.push_note(&format!(
                "VTSessionSetProperty(RealTime) failed (non-fatal): {error}"
            ));
        }

        finding.hardware_decode = query_hardware_accelerated(&session);

        if temporal_units.is_empty() {
            finding.push_note(
                "a Sequence Header OBU was present but the input contained no other Temporal \
                 Unit; the session above was created but decode was not exercised",
            );
            return;
        }

        let mut submitted_bytes: u64 = 0;
        let mut decode_error: Option<String> = None;
        let start = Instant::now();
        for unit in temporal_units {
            submitted_bytes += unit.len() as u64;
            let sample = match make_sample_buffer(&format, unit, 0) {
                Ok(sample) => sample,
                Err(error) => {
                    decode_error = Some(error);
                    break;
                }
            };
            if let Err(error) = session.decode(&sample) {
                decode_error = Some(format!("VTDecompressionSessionDecodeFrame: {error}"));
                break;
            }
        }
        if let Err(error) = session.wait_for_async_frames() {
            decode_error.get_or_insert(format!(
                "VTDecompressionSessionWaitForAsynchronousFrames: {error}"
            ));
        }
        let elapsed = start.elapsed();
        drop(session);

        let mut frames = Vec::new();
        while let Ok(frame) = rx.try_recv() {
            frames.push(frame);
        }
        let decoded_count = frames
            .iter()
            .filter(|frame| frame.status == 0 && frame.image_buffer.is_some())
            .count();
        let last_delivered_format = frames.iter().rev().find_map(|frame| {
            if frame.status != 0 {
                return None;
            }
            frame
                .image_buffer
                .as_ref()
                .map(|buffer| super::fourcc_string(buffer.pixel_format()))
        });
        let last_image_buffer: Option<CVPixelBuffer> = frames.iter().rev().find_map(|frame| {
            if frame.status != 0 {
                return None;
            }
            frame.image_buffer.clone()
        });

        if decoded_count == 0 {
            finding.decode = ProbeOutcome::Failed;
            match decode_error {
                Some(error) => finding.push_note(&error),
                None => finding.push_note("no frame was delivered by the decode callback"),
            }
            return;
        }

        finding.decode = ProbeOutcome::Ok;
        finding.delivered_pixel_format = last_delivered_format;

        if let Some(pattern) = reference_pattern {
            match last_image_buffer {
                Some(buffer) => {
                    record_roundtrip_accuracy(finding, &buffer, row, pattern, roundtrip_meta)
                }
                None => finding.push_note(
                    "roundtrip_max_error was not computed: a frame decoded (status == 0) but \
                     carried no image buffer",
                ),
            }
        }

        let elapsed_secs = elapsed.as_secs_f64();
        if decoded_count >= 2 && elapsed_secs > 0.0 {
            finding.sustained_fps = Some(decoded_count as f64 / elapsed_secs);
            finding.bitrate_mbps =
                Some((submitted_bytes as f64 * 8.0) / elapsed_secs / 1_000_000.0);
            finding.push_note(&format!(
                "sustained_fps/bitrate_mbps are a tight decode-loop measurement over {} \
                 Temporal Unit(s) fed back-to-back from a static file, not paced real-time \
                 playback -- a static capture carries no per-frame timing to pace against",
                temporal_units.len(),
            ));
        } else {
            finding.push_note(&format!(
                "only {decoded_count} frame(s) decoded from this input; not enough to measure a \
                 rate",
            ));
        }
        let failed_units = frames.len().saturating_sub(decoded_count);
        if failed_units > 0 {
            finding.push_note(&format!(
                "{failed_units} of {} submitted Temporal Unit(s) did not decode cleanly",
                frames.len(),
            ));
        }
    }

    /// Computes and records this row's real encode/decode round-trip
    /// accuracy against `pattern`, and cross-checks `roundtrip_meta` (when
    /// present) against what was actually decoded.
    ///
    /// Never fails loudly: a buffer this cannot convert (an unexpected
    /// pixel format, an out-of-bounds plane) is recorded as a note on
    /// `finding` rather than a panic, exactly like every other failure mode
    /// in this module.
    fn record_roundtrip_accuracy(
        finding: &mut RowFinding,
        buffer: &CVPixelBuffer,
        row: VideoVariant,
        pattern: TestPattern,
        roundtrip_meta: Option<RoundtripMeta>,
    ) {
        let width = buffer.width();
        let height = buffer.height();
        if let Some(meta) = roundtrip_meta {
            if meta.width as usize != width || meta.height as usize != height {
                finding.push_note(&format!(
                    "geometry mismatch: roundtrip-meta.json recorded {}x{} but VideoToolbox \
                     delivered {width}x{height}; comparing against the delivered size anyway, \
                     since that is what was actually decoded",
                    meta.width, meta.height,
                ));
            }
        }
        let transform = ColorTransform::new(row.video.matrix, row.video.range, row.video.bit_depth);
        let bgra = match decoded_buffer_to_bgra(buffer, transform) {
            Ok(bgra) => bgra,
            Err(error) => {
                finding.push_note(&format!("roundtrip_max_error was not computed: {error}"));
                return;
            }
        };
        match measure_rgb_roundtrip(pattern, width, height, &bgra) {
            Some(accuracy) => {
                finding.roundtrip_max_error = Some(f64::from(accuracy.max_error));
                finding.roundtrip_mean_error = Some(accuracy.mean_error);
                finding.roundtrip_pattern = Some(pattern.token().to_string());
                finding.push_note(&format!(
                    "roundtrip_max_error/roundtrip_mean_error measure a REAL encode+decode \
                     against pattern `{}` at {width}x{height}; this includes codec \
                     quantisation loss and is NOT the same claim as \
                     arcen_media::test_pattern::measure_transform_roundtrip's pure, codec-free \
                     figure (worst pixel at {:?})",
                    pattern.token(),
                    accuracy.worst_at,
                ));
            }
            None => finding.push_note(
                "roundtrip_max_error was not computed: the recovered buffer was shorter than \
                 width*height*4 bytes",
            ),
        }
    }

    /// Converts a decoded `CVPixelBuffer`'s planes into a tightly packed
    /// BGRA buffer, inverting whatever coded samples the format actually
    /// carries via `transform` -- YCbCr for a real matrix, or G/B/R
    /// directly for the identity/GBR row (`ColorTransform::to_bgr8` already
    /// branches on this; this function never needs to know which case it
    /// is in).
    ///
    /// Every pixel format this tool ever requests (see
    /// `super::requested_pixel_format_for`) is one of CoreVideo's
    /// *biplanar* families (`420v`/`420f`/`444v`/`444f`/`x420`/`xf20`/
    /// `x422`/`xf22`/`x444`/`xf44`): plane 0 carries one luma (or, for
    /// identity, G) sample per pixel; plane 1 carries interleaved Cb,Cr (or
    /// B,R) pairs, on its own subsampled grid -- `CVPixelBufferGetWidth/
    /// HeightOfPlane(_, 1)` already report that grid's own width/height
    /// (half-resolution for 4:2:0, full-resolution for the 4:4:4 rows this
    /// feature cares about most), so this reads plane 1's geometry rather
    /// than assuming a subsampling ratio, and works whether the row is
    /// 4:4:4, 4:2:2 or 4:2:0 (a subsampled row is still fed through here to
    /// measure exactly how much subsampling itself costs).
    ///
    /// Depth determines sample width: eight-bit families pack one byte per
    /// sample; the ten-bit `x`/`xf`-prefixed family packs each sample
    /// MSB-aligned in a 16-bit little-endian word --
    /// `ColorTransform::unpack_p16` is the exact inverse of the packing
    /// `arcen_media::video::convert`'s `pack_p16` performs on the host
    /// side, so this is not a second, possibly divergent, implementation of
    /// that convention.
    ///
    /// # Errors
    ///
    /// Returns a description of whatever is wrong: fewer than two planes,
    /// a plane reporting zero width or height, or a lock/read failure.
    fn decoded_buffer_to_bgra(
        buffer: &CVPixelBuffer,
        transform: ColorTransform,
    ) -> Result<Vec<u8>, String> {
        if buffer.plane_count() < 2 {
            return Err(format!(
                "expected a biplanar YCbCr/GBR buffer but got {} plane(s)",
                buffer.plane_count()
            ));
        }
        let width = buffer.width();
        let height = buffer.height();
        let luma_width = buffer.width_of_plane(0);
        let luma_height = buffer.height_of_plane(0);
        let chroma_width = buffer.width_of_plane(1);
        let chroma_height = buffer.height_of_plane(1);
        if luma_width == 0 || luma_height == 0 || chroma_width == 0 || chroma_height == 0 {
            return Err("a plane reported zero width or height".to_string());
        }
        let sample_bytes: usize = if transform.depth().bits() > 8 { 2 } else { 1 };

        let guard = buffer
            .lock_read_only()
            .map_err(|status| format!("CVPixelBufferLockBaseAddress failed: status {status}"))?;

        let read_component =
            |plane: usize, component_index: usize, y: usize| -> Result<i32, String> {
                let row_bytes = guard
                    .plane_row(plane, y)
                    .ok_or_else(|| format!("plane {plane} row {y} out of bounds"))?;
                let offset = component_index * sample_bytes;
                if offset + sample_bytes > row_bytes.len() {
                    return Err(format!(
                        "plane {plane} row {y} component {component_index} out of bounds"
                    ));
                }
                Ok(if sample_bytes == 2 {
                    let word = u16::from_le_bytes([row_bytes[offset], row_bytes[offset + 1]]);
                    transform.unpack_p16(word)
                } else {
                    i32::from(row_bytes[offset])
                })
            };

        let mut out = vec![0_u8; width * height * 4];
        for y in 0..height {
            // Nearest-neighbour chroma lookup, scaled by each plane's own
            // reported size rather than an assumed ratio -- correct for
            // 4:4:4 (chroma_height == luma_height, so this is an identity
            // map), 4:2:2 and 4:2:0 alike.
            let chroma_y = (y * chroma_height / luma_height).min(chroma_height - 1);
            for x in 0..width {
                let chroma_x = (x * chroma_width / luma_width).min(chroma_width - 1);
                let luma = read_component(0, x.min(luma_width - 1), y.min(luma_height - 1))?;
                let cb = read_component(1, chroma_x * 2, chroma_y)?;
                let cr = read_component(1, chroma_x * 2 + 1, chroma_y)?;
                let (b, g, r) = transform.to_bgr8(luma, cb, cr);
                let offset = (y * width + x) * 4;
                out[offset] = b;
                out[offset + 1] = g;
                out[offset + 2] = r;
                out[offset + 3] = 0xff;
            }
        }
        Ok(out)
    }

    fn build_format_description(
        codec: ProbeCodec,
        params: &ParameterSets<'_>,
    ) -> Result<CMFormatDescription, String> {
        match codec {
            ProbeCodec::H264 => {
                let sps = params.sps.ok_or_else(|| "missing SPS".to_string())?;
                let pps = params.pps.ok_or_else(|| "missing PPS".to_string())?;
                build_h264_format_description(sps, pps)
            }
            ProbeCodec::H265 => {
                let vps = params.vps.ok_or_else(|| "missing VPS".to_string())?;
                let sps = params.sps.ok_or_else(|| "missing SPS".to_string())?;
                let pps = params.pps.ok_or_else(|| "missing PPS".to_string())?;
                build_hevc_format_description(vps, sps, pps)
            }
        }
    }

    /// Mirrors `video_decoder::platform::make_h264_format_description`
    /// (private to that module's own `platform` submodule; reimplemented
    /// here against the same raw API rather than imported).
    fn build_h264_format_description(
        sps: &[u8],
        pps: &[u8],
    ) -> Result<CMFormatDescription, String> {
        let parameter_sets = [sps.as_ptr(), pps.as_ptr()];
        let parameter_set_sizes = [sps.len(), pps.len()];
        let mut format: raw::CMFormatDescriptionRef = ptr::null_mut();
        let status = unsafe {
            raw::CMVideoFormatDescriptionCreateFromH264ParameterSets(
                raw::kCFAllocatorDefault,
                parameter_sets.len(),
                parameter_sets.as_ptr(),
                parameter_set_sizes.as_ptr(),
                4,
                &mut format,
            )
        };
        if status != 0 || format.is_null() {
            return Err(format!(
                "CMVideoFormatDescriptionCreateFromH264ParameterSets failed: {status}"
            ));
        }
        CMFormatDescription::from_raw(format.cast_mut().cast::<c_void>()).ok_or_else(|| {
            "CMVideoFormatDescriptionCreateFromH264ParameterSets returned NULL".to_string()
        })
    }

    /// Mirrors `video_decoder::platform::make_hevc_format_description`
    /// (private to that module's own `platform` submodule; reimplemented
    /// here against the same raw API rather than imported).
    fn build_hevc_format_description(
        vps: &[u8],
        sps: &[u8],
        pps: &[u8],
    ) -> Result<CMFormatDescription, String> {
        let parameter_sets = [vps.as_ptr(), sps.as_ptr(), pps.as_ptr()];
        let parameter_set_sizes = [vps.len(), sps.len(), pps.len()];
        let mut format: raw::CMFormatDescriptionRef = ptr::null_mut();
        // No extensions dictionary -- VideoToolbox derives Rext 4:4:4
        // support from the SPS itself, exactly as `video_decoder` does.
        let extensions: raw::CFDictionaryRef = unsafe { std::mem::zeroed() };
        let status = unsafe {
            raw::CMVideoFormatDescriptionCreateFromHEVCParameterSets(
                raw::kCFAllocatorDefault,
                parameter_sets.len(),
                parameter_sets.as_ptr(),
                parameter_set_sizes.as_ptr(),
                4,
                extensions,
                &mut format,
            )
        };
        if status != 0 || format.is_null() {
            return Err(format!(
                "CMVideoFormatDescriptionCreateFromHEVCParameterSets failed: {status}"
            ));
        }
        CMFormatDescription::from_raw(format.cast_mut().cast::<c_void>()).ok_or_else(|| {
            "CMVideoFormatDescriptionCreateFromHEVCParameterSets returned NULL".to_string()
        })
    }

    /// Mirrors `video_decoder::platform::make_av1_format_description`
    /// (private to that module's own `platform` submodule; reimplemented
    /// here against the same raw API rather than imported). See that
    /// function's doc for why `CMVideoFormatDescriptionCreate` plus an
    /// `av1C`/`SampleDescriptionExtensionAtoms` extension is used instead
    /// of a `CMVideoFormatDescriptionCreateFromAV1ParameterSets` that does
    /// not exist in the vendored bindings.
    fn build_av1_format_description(
        fields: &av1::SequenceHeaderFields,
        sequence_header_obu: &[u8],
    ) -> Result<CMFormatDescription, String> {
        let width = i32::try_from(fields.max_frame_width).map_err(|_| {
            format!(
                "AV1 max_frame_width {} does not fit in i32",
                fields.max_frame_width
            )
        })?;
        let height = i32::try_from(fields.max_frame_height).map_err(|_| {
            format!(
                "AV1 max_frame_height {} does not fit in i32",
                fields.max_frame_height
            )
        })?;

        let av1c = av1::build_av1c(fields, sequence_header_obu);
        let av1c_data = CFData::from_bytes(&av1c);
        let av1c_key = CFString::new("av1C");
        let atoms = CFDictionary::from_pairs(&[(&av1c_key, &av1c_data)]);
        // SAFETY: `kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms`
        // is a well-known, process-wide singleton CFStringRef exported by
        // CoreMedia.
        let Some(atoms_key) = (unsafe {
            CFString::from_raw_retained(
                raw::kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms
                    .cast_mut()
                    .cast(),
            )
        }) else {
            return Err(
                "kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms was NULL"
                    .to_string(),
            );
        };
        let extensions = CFDictionary::from_pairs(&[(&atoms_key, &atoms)]);

        let mut format: raw::CMVideoFormatDescriptionRef = ptr::null_mut();
        // SAFETY: `extensions` is a valid, live `CFDictionary` for the
        // duration of this call; `format` is a valid out-pointer.
        let status = unsafe {
            raw::CMVideoFormatDescriptionCreate(
                raw::kCFAllocatorDefault,
                raw::kCMVideoCodecType_AV1,
                width,
                height,
                extensions.as_ptr().cast(),
                &mut format,
            )
        };
        if status != 0 || format.is_null() {
            return Err(format!(
                "CMVideoFormatDescriptionCreate (AV1) failed: {status}"
            ));
        }
        CMFormatDescription::from_raw(format.cast_mut().cast::<c_void>())
            .ok_or_else(|| "CMVideoFormatDescriptionCreate (AV1) returned NULL".to_string())
    }

    /// Mirrors `video_decoder::platform::build_destination_attributes`.
    fn build_destination_attributes(pixel_format: u32) -> Option<CFDictionary> {
        // SAFETY: `kCVPixelBufferPixelFormatTypeKey` is a well-known,
        // process-wide singleton CFStringRef exported by CoreVideo.
        let key = unsafe {
            CFString::from_raw_retained(raw::kCVPixelBufferPixelFormatTypeKey.cast_mut().cast())
        }?;
        let value = CFNumber::from_i64(i64::from(pixel_format));
        Some(CFDictionary::from_pairs(&[(&key, &value)]))
    }

    /// Reads back `CMFormatDescriptionGetExtensions()` for the four colour
    /// extensions this feature cares about, so the report can be compared
    /// against what a host's encoder intended. Mirrors
    /// `video_decoder::platform::log_color_extensions`'s technique (same
    /// four keys, same lookup), producing a bounded string instead of a
    /// `tracing` event.
    fn describe_color_extensions(format: &CMFormatDescription) -> String {
        let dict = format
            .extensions()
            .and_then(|ptr| unsafe { CFDictionary::from_raw_retained(ptr.cast_mut()) });
        let describe = |key: raw::CFStringRef| -> String {
            let Some(dict) = &dict else {
                return "<absent>".to_string();
            };
            // SAFETY: `key` is a well-known, process-wide singleton
            // CFStringRef exported by CoreMedia.
            let Some(key) = (unsafe { CFType::from_raw_retained(key.cast_mut().cast()) }) else {
                return "<absent>".to_string();
            };
            dict.get(&key)
                .map_or_else(|| "<absent>".to_string(), |value| value.description())
        };
        format!(
            "primaries={} transfer={} matrix={} full_range={}",
            describe(unsafe { raw::kCMFormatDescriptionExtension_ColorPrimaries }),
            describe(unsafe { raw::kCMFormatDescriptionExtension_TransferFunction }),
            describe(unsafe { raw::kCMFormatDescriptionExtension_YCbCrMatrix }),
            describe(unsafe { raw::kCMFormatDescriptionExtension_FullRangeVideo }),
        )
    }

    /// Mirrors `video_decoder::platform::query_hardware_accelerated`.
    fn query_hardware_accelerated(session: &DecompressionSession) -> Option<bool> {
        let property = match unsafe {
            session.copy_property(
                ffi::kVTDecompressionPropertyKey_UsingHardwareAcceleratedVideoDecoder,
            )
        } {
            Ok(property) => property,
            Err(_error) => return None,
        };
        let value = property?;
        let value_ptr = value.as_ptr().cast_const();
        let true_ptr = unsafe { ffi::kCFBooleanTrue }.cast::<c_void>();
        Some(value_ptr == true_ptr)
    }

    /// Mirrors `video_decoder::platform::make_sample_buffer`.
    fn make_sample_buffer(
        format: &CMFormatDescription,
        payload: &[u8],
        timestamp_ms: u32,
    ) -> Result<CMSampleBuffer, String> {
        let block = CMBlockBuffer::create(payload)
            .ok_or_else(|| "CMBlockBufferCreate failed".to_string())?;
        let timing = raw::CMSampleTimingInfo {
            duration: unsafe { raw::CMTimeMake(1, 60) },
            presentationTimeStamp: unsafe {
                raw::CMTimeMake(i64::from(timestamp_ms), FRAME_TIMESCALE)
            },
            decodeTimeStamp: unsafe { raw::CMTimeMake(i64::from(timestamp_ms), FRAME_TIMESCALE) },
        };
        let sample_size = payload.len();
        let mut sample: raw::CMSampleBufferRef = ptr::null_mut();
        let status = unsafe {
            raw::CMSampleBufferCreateReady(
                raw::kCFAllocatorDefault,
                block.as_ptr().cast(),
                format.as_ptr().cast(),
                1,
                1,
                &timing,
                1,
                &sample_size,
                &mut sample,
            )
        };
        if status != 0 || sample.is_null() {
            return Err(format!("CMSampleBufferCreateReady failed: {status}"));
        }
        Ok(unsafe { CMSampleBuffer::from_ptr(sample.cast()) })
    }

    /// Attempts to create a throwaway `VTDecompressionSession` with
    /// `kVTVideoDecoderSpecification_RequireHardwareAcceleratedVideoDecoder`
    /// forced to `true`. Apple's documented behaviour is that session
    /// creation itself fails outright rather than silently falling back
    /// when hardware decode is unavailable for the format, which is the
    /// only way to distinguish "decodes, but only in software" from "does
    /// not decode here at all" -- both look identical to a session created
    /// without the requirement. This bypasses the `videotoolbox` crate's
    /// safe `DecompressionSession` wrapper, which hard-codes a NULL
    /// `videoDecoderSpecification` and has no constructor accepting one
    /// (verified against `videotoolbox-0.18.1/src/decompression/mod.rs`);
    /// the raw `ffi::VTDecompressionSessionCreate` is the only way to pass
    /// this dictionary.
    fn probe_requires_hardware(
        format: &CMFormatDescription,
        image_buffer_attributes: Option<&CFDictionary>,
    ) -> Result<(), (i32, String)> {
        let Some(require_key) = retain_constant(
            unsafe { ffi::kVTVideoDecoderSpecification_RequireHardwareAcceleratedVideoDecoder }
                .cast(),
        ) else {
            return Err((
                0,
                "kVTVideoDecoderSpecification_RequireHardwareAcceleratedVideoDecoder was NULL"
                    .to_string(),
            ));
        };
        let Some(require_value) = retain_constant(unsafe { ffi::kCFBooleanTrue }.cast()) else {
            return Err((0, "kCFBooleanTrue was NULL".to_string()));
        };
        let specification = CFDictionary::from_pairs(&[(&require_key, &require_value)]);

        let record = ffi::VTDecompressionOutputCallbackRecord {
            decompression_output_callback: ignore_decode_output,
            decompression_output_ref_con: ptr::null_mut(),
        };

        let mut session: ffi::VTDecompressionSessionRef = ptr::null_mut();
        let status = unsafe {
            ffi::VTDecompressionSessionCreate(
                ffi::kCFAllocatorDefault,
                format.as_ptr().cast(),
                specification.as_ptr().cast(),
                image_buffer_attributes.map_or(ptr::null(), |dict| dict.as_ptr().cast()),
                &record,
                &mut session,
            )
        };
        if status != 0 || session.is_null() {
            return Err((
                status,
                format!("VTDecompressionSessionCreate (hardware required) failed: {status}"),
            ));
        }
        // SAFETY: `session` was just confirmed non-NULL with `status == 0`,
        // so it is a valid, uniquely-owned +1 `VTDecompressionSessionRef`;
        // this probe never decodes with it, so it is torn down immediately.
        unsafe {
            ffi::VTDecompressionSessionInvalidate(session);
            ffi::CFRelease(session.cast());
        }
        Ok(())
    }

    /// A `VTDecompressionOutputCallback` that does nothing: the
    /// hardware-required probe above never decodes a real sample, it only
    /// tests whether session creation itself succeeds, so no callback body
    /// is ever actually invoked in practice.
    unsafe extern "C" fn ignore_decode_output(
        _decompression_output_ref_con: *mut c_void,
        _source_frame_ref_con: *mut c_void,
        _status: ffi::OSStatus,
        _info_flags: ffi::VTDecodeInfoFlags,
        _image_buffer: *mut c_void,
        _presentation_time_stamp: CMTime,
        _presentation_duration: CMTime,
    ) {
    }

    /// Retains a borrowed (`Get`-convention) CoreFoundation constant as an
    /// owned [`CFType`]. Named Apple `k...` constants (e.g.
    /// `kVTVideoDecoderSpecification_RequireHardwareAcceleratedVideoDecoder`,
    /// `kCFBooleanTrue`) are +0 borrowed, process-wide singletons. Mirrors
    /// `video_decoder::platform::retain_extension_constant`, which this
    /// file cannot import because it is a private helper of that module's
    /// own `platform` submodule.
    fn retain_constant(ptr: *const c_void) -> Option<CFType> {
        // SAFETY: every caller passes a well-known, process-wide singleton
        // CFTypeRef constant exported by CoreFoundation or VideoToolbox.
        unsafe { CFType::from_raw_retained(ptr.cast_mut()) }
    }
}

#[cfg(not(target_os = "macos"))]
mod backend {
    use super::{
        av1, AccessUnit, ClientInfo, ParameterSets, ProbeCodec, ProbeOutcome, RoundtripMeta,
        RowFinding, TestPattern, VideoVariant,
    };

    pub(super) fn register_supplemental_decoders() {}

    pub(super) fn client_environment() -> ClientInfo {
        ClientInfo {
            model: None,
            chip: None,
            macos_version: None,
            display: None,
            reference_mode: None,
        }
    }

    pub(super) fn probe_row(
        finding: &mut RowFinding,
        _codec: ProbeCodec,
        _params: &ParameterSets<'_>,
        _access_units: &[AccessUnit<'_>],
        _requested_pixel_format: Option<u32>,
        _row: VideoVariant,
        _reference_pattern: Option<TestPattern>,
        _roundtrip_meta: Option<RoundtripMeta>,
    ) {
        finding.decode = ProbeOutcome::Unsupported;
        finding.push_note(
            "this arcen-deck binary was built for a non-macOS target, so VideoToolbox is \
             unavailable here; probe-matrix only probes real decode behaviour on macOS",
        );
    }

    pub(super) fn probe_av1_row(
        finding: &mut RowFinding,
        _fields: &av1::SequenceHeaderFields,
        _sequence_header_obu: &[u8],
        _temporal_units: &[Vec<u8>],
        _requested_pixel_format: Option<u32>,
        _row: VideoVariant,
        _reference_pattern: Option<TestPattern>,
        _roundtrip_meta: Option<RoundtripMeta>,
    ) {
        finding.decode = ProbeOutcome::Unsupported;
        finding.push_note(
            "this arcen-deck binary was built for a non-macOS target, so VideoToolbox is \
             unavailable here; probe-matrix only probes real decode behaviour on macOS",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- split_annex_b ----

    #[test]
    fn splits_three_and_four_byte_start_codes_and_trims_trailing_zero_padding() {
        let bytes = [
            0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 4, 0, 0,
        ];
        let nals = split_annex_b(&bytes);
        assert_eq!(
            nals,
            vec![&[0x67, 1, 2][..], &[0x68, 3][..], &[0x65, 4][..]]
        );
    }

    #[test]
    fn empty_input_yields_no_nals() {
        assert!(split_annex_b(&[]).is_empty());
    }

    #[test]
    fn missing_start_code_yields_whole_buffer_as_one_nal() {
        let bytes = [1, 2, 3, 4];
        assert_eq!(split_annex_b(&bytes), vec![&[1, 2, 3, 4][..]]);
    }

    #[test]
    fn all_zero_trailing_nal_is_dropped_not_emitted_empty() {
        let bytes = [0, 0, 0, 1, 0x67, 1, 0, 0, 0];
        let nals = split_annex_b(&bytes);
        assert_eq!(nals, vec![&[0x67, 1][..]]);
    }

    // ---- classify_nal ----

    #[test]
    fn classifies_h264_nal_types() {
        assert_eq!(classify_nal(ProbeCodec::H264, &[0x67]), ProbeNalKind::Sps);
        assert_eq!(classify_nal(ProbeCodec::H264, &[0x68]), ProbeNalKind::Pps);
        assert_eq!(
            classify_nal(ProbeCodec::H264, &[0x65]),
            ProbeNalKind::Keyframe
        );
        assert_eq!(classify_nal(ProbeCodec::H264, &[0x41]), ProbeNalKind::Slice);
        assert_eq!(classify_nal(ProbeCodec::H264, &[0x06]), ProbeNalKind::Other);
    }

    #[test]
    fn classifies_hevc_nal_types() {
        assert_eq!(
            classify_nal(ProbeCodec::H265, &[32 << 1, 1]),
            ProbeNalKind::Vps
        );
        assert_eq!(
            classify_nal(ProbeCodec::H265, &[33 << 1, 1]),
            ProbeNalKind::Sps
        );
        assert_eq!(
            classify_nal(ProbeCodec::H265, &[34 << 1, 1]),
            ProbeNalKind::Pps
        );
        assert_eq!(
            classify_nal(ProbeCodec::H265, &[19 << 1, 1]),
            ProbeNalKind::Keyframe
        );
        assert_eq!(
            classify_nal(ProbeCodec::H265, &[21 << 1, 1]),
            ProbeNalKind::Keyframe
        );
        assert_eq!(
            classify_nal(ProbeCodec::H265, &[1 << 1, 1]),
            ProbeNalKind::Slice
        );
        assert_eq!(
            classify_nal(ProbeCodec::H265, &[39 << 1, 1]),
            ProbeNalKind::Other
        );
    }

    // ---- nals_to_avcc ----

    #[test]
    fn avcc_length_prefixes_are_big_endian_u32() {
        let nals = vec![&[0x65, 1, 2][..], &[0x41, 3][..]];
        let avcc = nals_to_avcc(&nals);
        assert_eq!(avcc, vec![0, 0, 0, 3, 0x65, 1, 2, 0, 0, 0, 2, 0x41, 3]);
    }

    // ---- latest_parameter_sets / is_complete ----

    #[test]
    fn h264_parameter_sets_complete_needs_sps_and_pps_only() {
        let sps: &[u8] = &[0x67, 1];
        let pps: &[u8] = &[0x68, 2];
        let nals = vec![sps, pps];
        let sets = latest_parameter_sets(&nals, ProbeCodec::H264);
        assert!(sets.is_complete(ProbeCodec::H264));
        assert_eq!(sets.sps, Some(sps));
        assert_eq!(sets.pps, Some(pps));
        assert_eq!(sets.vps, None);
    }

    #[test]
    fn h265_parameter_sets_require_vps_too() {
        let sps: &[u8] = &[33 << 1, 1];
        let pps: &[u8] = &[34 << 1, 2];
        let nals = vec![sps, pps];
        let sets = latest_parameter_sets(&nals, ProbeCodec::H265);
        assert!(!sets.is_complete(ProbeCodec::H265));
    }

    #[test]
    fn latest_parameter_set_of_each_kind_wins_over_earlier_ones() {
        let sps_v1: &[u8] = &[0x67, 1];
        let sps_v2: &[u8] = &[0x67, 2];
        let pps: &[u8] = &[0x68, 9];
        let nals = vec![sps_v1, pps, sps_v2];
        let sets = latest_parameter_sets(&nals, ProbeCodec::H264);
        assert_eq!(sets.sps, Some(sps_v2));
    }

    // ---- group_access_units ----

    #[test]
    fn groups_single_keyframe_access_unit_with_leading_parameter_sets() {
        let sps: &[u8] = &[0x67, 1];
        let pps: &[u8] = &[0x68, 2];
        let idr: &[u8] = &[0x65, 3];
        let nals = vec![sps, pps, idr];
        let units = group_access_units(&nals, ProbeCodec::H264);
        assert_eq!(units.len(), 1);
        assert!(units[0].is_keyframe);
        assert_eq!(units[0].nals, vec![sps, pps, idr]);
    }

    #[test]
    fn groups_multiple_access_units_in_order() {
        let sps: &[u8] = &[0x67, 1];
        let pps: &[u8] = &[0x68, 2];
        let idr: &[u8] = &[0x65, 3];
        let p1: &[u8] = &[0x41, 4];
        let p2: &[u8] = &[0x41, 5];
        let nals = vec![sps, pps, idr, p1, p2];
        let units = group_access_units(&nals, ProbeCodec::H264);
        assert_eq!(units.len(), 3);
        assert!(units[0].is_keyframe);
        assert!(!units[1].is_keyframe);
        assert_eq!(units[1].nals, vec![p1]);
        assert!(!units[2].is_keyframe);
        assert_eq!(units[2].nals, vec![p2]);
    }

    #[test]
    fn parameter_sets_with_no_following_vcl_nal_yield_zero_access_units() {
        let vps: &[u8] = &[32 << 1, 1];
        let sps: &[u8] = &[33 << 1, 1];
        let pps: &[u8] = &[34 << 1, 1];
        let nals = vec![vps, sps, pps];
        let units = group_access_units(&nals, ProbeCodec::H265);
        assert!(units.is_empty());
    }

    // ---- access_unit_sample_bytes ----

    #[test]
    fn keyframe_unit_prepends_cached_parameter_sets_without_duplicating_inline_ones() {
        let vps: &[u8] = &[32 << 1, 1];
        let sps: &[u8] = &[33 << 1, 1];
        let pps: &[u8] = &[34 << 1, 1];
        let idr: &[u8] = &[19 << 1, 9];
        let nals = vec![vps, sps, pps, idr];
        let params = latest_parameter_sets(&nals, ProbeCodec::H265);
        let units = group_access_units(&nals, ProbeCodec::H265);
        assert_eq!(units.len(), 1);
        let sample = access_unit_sample_bytes(&units[0], &params, ProbeCodec::H265);
        let expected = nals_to_avcc(&[vps, sps, pps, idr]);
        assert_eq!(sample, expected, "vps/sps/pps must appear exactly once");
    }

    #[test]
    fn non_keyframe_unit_carries_only_its_own_slice_nals() {
        let sps: &[u8] = &[0x67, 1];
        let pps: &[u8] = &[0x68, 2];
        let idr: &[u8] = &[0x65, 3];
        let p1: &[u8] = &[0x41, 4];
        let nals = vec![sps, pps, idr, p1];
        let params = latest_parameter_sets(&nals, ProbeCodec::H264);
        let units = group_access_units(&nals, ProbeCodec::H264);
        let sample = access_unit_sample_bytes(&units[1], &params, ProbeCodec::H264);
        assert_eq!(sample, nals_to_avcc(&[p1]));
    }

    // ---- wire_chroma / requested_pixel_format_for ----

    #[test]
    fn wire_chroma_maps_every_variant() {
        assert_eq!(wire_chroma(ChromaSubsampling::Yuv420), WireChroma::Yuv420);
        assert_eq!(wire_chroma(ChromaSubsampling::Yuv422), WireChroma::Yuv422);
        assert_eq!(wire_chroma(ChromaSubsampling::Yuv444), WireChroma::Yuv444);
    }

    #[test]
    fn requested_pixel_format_matches_expected_fourcc_for_target_row() {
        let row = VideoVariant::from_id("hevc-444-10-full-bt709").expect("target row parses");
        let format = requested_pixel_format_for(&row).expect("format must resolve");
        assert_eq!(fourcc_string(format), "xf44");
    }

    #[test]
    fn requested_pixel_format_matches_expected_fourcc_for_control_row() {
        let row = VideoVariant::from_id("hevc-444-8-limited-bt709").expect("control row parses");
        let format = requested_pixel_format_for(&row).expect("format must resolve");
        assert_eq!(fourcc_string(format), "444v");
    }

    #[test]
    fn requested_pixel_format_matches_expected_fourcc_for_h264_cheap_row() {
        let row = VideoVariant::from_id("h264-420-8-full-bt709").expect("cheap row parses");
        let format = requested_pixel_format_for(&row).expect("format must resolve");
        assert_eq!(fourcc_string(format), "420f");
    }

    // ---- ProbeCodec::from_media_codec ----

    #[test]
    fn only_h264_and_h265_map_to_a_probe_codec() {
        assert_eq!(
            ProbeCodec::from_media_codec(VideoCodec::H264),
            Some(ProbeCodec::H264)
        );
        assert_eq!(
            ProbeCodec::from_media_codec(VideoCodec::H265),
            Some(ProbeCodec::H265)
        );
        assert_eq!(ProbeCodec::from_media_codec(VideoCodec::Av1), None);
        assert_eq!(ProbeCodec::from_media_codec(VideoCodec::Vp9), None);
        assert_eq!(ProbeCodec::from_media_codec(VideoCodec::Jpeg), None);
    }

    // ---- civil_from_days / format_utc_timestamp ----

    #[test]
    fn epoch_formats_as_expected() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(format_utc_timestamp(UNIX_EPOCH), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn known_dates_round_trip_through_civil_from_days() {
        // 2024-02-29 (leap day) is 19782 days after the epoch.
        assert_eq!(civil_from_days(19782), (2024, 2, 29));
        // 2000-03-01 is 11017 days after the epoch (crosses the y2k leap day).
        assert_eq!(civil_from_days(11017), (2000, 3, 1));
        // 1999-12-31 is 10956 days after the epoch.
        assert_eq!(civil_from_days(10956), (1999, 12, 31));
    }

    #[test]
    fn format_utc_timestamp_includes_time_of_day() {
        let time =
            UNIX_EPOCH + std::time::Duration::from_secs(19782 * 86_400 + 13 * 3600 + 5 * 60 + 9);
        assert_eq!(format_utc_timestamp(time), "2024-02-29T13:05:09Z");
    }

    // ---- JSON shape ----

    #[test]
    fn probe_outcome_serialises_to_exact_lowercase_strings() {
        assert_eq!(serde_json::to_string(&ProbeOutcome::Ok).unwrap(), "\"ok\"");
        assert_eq!(
            serde_json::to_string(&ProbeOutcome::Failed).unwrap(),
            "\"failed\""
        );
        assert_eq!(
            serde_json::to_string(&ProbeOutcome::Unsupported).unwrap(),
            "\"unsupported\""
        );
        assert_eq!(
            serde_json::to_string(&ProbeOutcome::Untested).unwrap(),
            "\"untested\""
        );
    }

    #[test]
    fn report_json_shape_matches_the_committed_schema_field_names() {
        let mut row = RowFinding::new("hevc-444-10-full-bt709".to_string());
        row.decode = ProbeOutcome::Ok;
        row.hardware_decode = Some(true);
        row.delivered_pixel_format = Some("xf44".to_string());
        row.push_note("first note");
        row.push_note("second note");
        assert_eq!(row.notes, "first note; second note");

        let report = ProbeReport {
            schema_version: 1,
            environments: vec![EnvironmentInfo {
                environment_id: "Mac15,6-macOS 15.0".to_string(),
                host: None,
                client: ClientInfo {
                    model: Some("Mac15,6".to_string()),
                    chip: Some("Apple M3".to_string()),
                    macos_version: Some("macOS 15.0".to_string()),
                    display: None,
                    reference_mode: None,
                },
                recorded_at: "2024-02-29T13:05:09Z".to_string(),
                arcen_commit: "unknown".to_string(),
            }],
            results: vec![row],
        };

        let json = render_json(&report).expect("render_json must succeed");
        let value: serde_json::Value = serde_json::from_str(&json).expect("valid json");

        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["results"][0]["variant"], "hevc-444-10-full-bt709");
        assert_eq!(value["results"][0]["decode"], "ok");
        assert_eq!(value["results"][0]["hardware_decode"], true);
        assert_eq!(value["results"][0]["delivered_pixel_format"], "xf44");
        assert_eq!(value["results"][0]["encoder_init"], "untested");
        assert!(value["results"][0]["encoder_error"].is_null());
        assert!(value["results"][0]["roundtrip_max_error"].is_null());
        assert!(value["results"][0]["sustained_fps"].is_null());
        assert!(value["results"][0]["bitrate_mbps"].is_null());
        assert_eq!(value["results"][0]["notes"], "first note; second note");
        assert_eq!(value["environments"][0]["client"]["model"], "Mac15,6");
        assert_eq!(value["environments"][0]["client"]["chip"], "Apple M3");
        assert!(value["environments"][0]["host"].is_null());
    }

    #[test]
    fn fresh_row_finding_defaults_to_untested_with_empty_notes() {
        let row = RowFinding::new("id".to_string());
        assert_eq!(row.decode, ProbeOutcome::Untested);
        assert_eq!(row.encoder_init, ProbeOutcome::Untested);
        assert_eq!(row.notes, "");
        assert!(row.hardware_decode.is_none());
    }

    // ---- candidate_input_paths ----

    #[test]
    fn candidate_paths_try_no_extension_before_known_extensions() {
        let dir = Path::new("/parameter-sets");
        let candidates = candidate_input_paths(dir, "hevc-444-10-full-bt709");
        let expected: Vec<PathBuf> = vec![
            dir.join("hevc-444-10-full-bt709"),
            dir.join("hevc-444-10-full-bt709.bin"),
            dir.join("hevc-444-10-full-bt709.annexb"),
            dir.join("hevc-444-10-full-bt709.h264"),
            dir.join("hevc-444-10-full-bt709.hevc"),
            dir.join("hevc-444-10-full-bt709.264"),
            dir.join("hevc-444-10-full-bt709.265"),
            dir.join("hevc-444-10-full-bt709.obu"),
        ];
        assert_eq!(candidates, expected);
    }

    // ---- "no parameter sets -> untested" path (no macOS backend touched) ----

    #[test]
    fn row_without_a_parameter_sets_directory_is_untested() {
        let row = VideoVariant::from_id("hevc-444-10-full-bt709").expect("target row parses");
        let finding = probe_one_row(row, None, None, None);
        assert_eq!(finding.decode, ProbeOutcome::Untested);
        assert!(finding.notes.contains("no --parameter-sets directory"));
    }

    #[test]
    fn row_with_a_directory_but_no_matching_file_is_untested() {
        let row = VideoVariant::from_id("hevc-444-10-full-bt709").expect("target row parses");
        // A directory that exists (the crate root) but certainly contains
        // no file named after this variant id.
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let finding = probe_one_row(row, Some(dir), None, None);
        assert_eq!(finding.decode, ProbeOutcome::Untested);
        assert!(finding.notes.contains("no parameter-set input found"));
    }

    #[test]
    fn av1_rows_without_a_parameter_sets_directory_are_untested_not_unsupported() {
        // Before this change every AV1 row was hard-coded `Unsupported`
        // regardless of input; now it is `Untested` absent a directory,
        // exactly like every other codec, since the harness can genuinely
        // attempt AV1 rows when given real host-produced data (see
        // `probe_one_av1_row`).
        for row in PROBE_MATRIX.iter().copied() {
            if row.video.codec != VideoCodec::Av1 {
                continue;
            }
            let finding = probe_one_row(row, None, None, None);
            assert_eq!(finding.decode, ProbeOutcome::Untested, "{}", row.id());
            assert!(finding.notes.contains("no --parameter-sets directory"));
        }
    }

    #[test]
    fn av1_row_with_a_directory_but_no_matching_file_is_untested() {
        let row = VideoVariant::from_id("av1-444-10-full-bt709").expect("av1 row parses");
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let finding = probe_one_row(row, Some(dir), None, None);
        assert_eq!(finding.decode, ProbeOutcome::Untested);
        assert!(finding.notes.contains("no parameter-set input found"));
        assert!(finding.notes.contains(".obu"));
    }

    // ---- av1_temporal_units ----

    #[test]
    fn av1_temporal_units_splits_on_temporal_delimiters_and_drops_structural_obus() {
        // Byte layout (same hand-computed framing as
        // `video_decoder`'s own `parses_a_temporal_delimiter_then_a_sequence_header_obu`
        // test): TD, seq_header(1-byte payload), TD, frame(1-byte payload).
        let bytes = [
            0x12, 0x00, // temporal delimiter
            0x0A, 0x01, 0xAB, // sequence header, payload [0xAB]
            0x12, 0x00, // temporal delimiter
            0x32, 0x01, 0xCD, // frame (obu_type=6), payload [0xCD]
        ];
        let obus = av1::parse_obus(&bytes).expect("well-formed OBU stream");
        let units = av1_temporal_units(&obus);
        assert_eq!(units, vec![vec![0x0A, 0x01, 0xAB], vec![0x32, 0x01, 0xCD]]);
    }

    #[test]
    fn av1_temporal_units_treats_a_delimiter_free_file_as_one_unit() {
        // No temporal delimiter at all: one sequence header immediately
        // followed by one frame, with no OBU_TEMPORAL_DELIMITER between
        // them at all.
        let bytes = [
            0x0A, 0x01, 0xAB, // sequence header
            0x32, 0x01, 0xCD, // frame
        ];
        let obus = av1::parse_obus(&bytes).expect("well-formed OBU stream");
        let units = av1_temporal_units(&obus);
        assert_eq!(units, vec![vec![0x0A, 0x01, 0xAB, 0x32, 0x01, 0xCD]]);
    }

    #[test]
    fn av1_temporal_units_drops_padding_and_redundant_frame_header_obus() {
        // Padding (obu_type=15 -> 0b0_1111_0_1_0 = 0x7A) and Redundant
        // Frame Header (obu_type=7 -> 0b0_0111_0_1_0 = 0x3A) OBUs must not
        // appear in a submitted sample per av1-isobmff's "AV1 Sample
        // Format" ("SHOULD NOT be used").
        let bytes = [
            0x0A, 0x01, 0xAB, // sequence header
            0x7A, 0x01, 0x00, // padding, dropped
            0x3A, 0x01, 0x00, // redundant frame header, dropped
            0x32, 0x01, 0xCD, // frame
        ];
        let obus = av1::parse_obus(&bytes).expect("well-formed OBU stream");
        let units = av1_temporal_units(&obus);
        assert_eq!(units, vec![vec![0x0A, 0x01, 0xAB, 0x32, 0x01, 0xCD]]);
    }

    #[test]
    fn every_probe_matrix_row_produces_a_finding_with_a_matching_variant_id() {
        // A cheap end-to-end smoke test that `run` never panics and always
        // returns exactly one finding per matrix row, in matrix order.
        let report = run(None, None);
        assert_eq!(report.results.len(), PROBE_MATRIX.len());
        for (row, finding) in PROBE_MATRIX.iter().copied().zip(report.results.iter()) {
            assert_eq!(finding.variant, row.id());
        }
    }

    // ---- parse_reference_pattern ----

    #[test]
    fn reference_pattern_is_none_when_the_flag_is_absent() {
        let args = vec!["probe-matrix".to_string()];
        assert_eq!(parse_reference_pattern(&args), Ok(None));
    }

    #[test]
    fn reference_pattern_parses_a_recognised_token() {
        let args = vec![
            "probe-matrix".to_string(),
            "--reference-pattern".to_string(),
            "grey_ramp".to_string(),
        ];
        assert_eq!(
            parse_reference_pattern(&args),
            Ok(Some(TestPattern::GreyRamp))
        );
    }

    #[test]
    fn reference_pattern_rejects_an_unknown_token() {
        let args = vec![
            "probe-matrix".to_string(),
            "--reference-pattern".to_string(),
            "not_a_pattern".to_string(),
        ];
        assert_eq!(
            parse_reference_pattern(&args),
            Err("unknown --reference-pattern `not_a_pattern`".to_string())
        );
    }

    #[test]
    fn reference_pattern_rejects_a_repeated_flag() {
        let args = vec![
            "probe-matrix".to_string(),
            "--reference-pattern".to_string(),
            "grey_ramp".to_string(),
            "--reference-pattern".to_string(),
            "chroma_detail".to_string(),
        ];
        assert_eq!(
            parse_reference_pattern(&args),
            Err("--reference-pattern may be specified only once".to_string())
        );
    }

    // ---- resolve_reference_pattern ----

    #[test]
    fn resolves_to_none_when_neither_source_is_present() {
        assert_eq!(resolve_reference_pattern(None, None), (None, None));
    }

    #[test]
    fn cli_flag_wins_outright_when_no_metadata_file_exists() {
        assert_eq!(
            resolve_reference_pattern(Some(TestPattern::GreyRamp), None),
            (Some(TestPattern::GreyRamp), None)
        );
    }

    #[test]
    fn metadata_file_is_trusted_automatically_with_no_explicit_flag() {
        let meta = RoundtripMeta {
            pattern: TestPattern::SaturatedPrimaries,
            width: 1920,
            height: 1080,
        };
        assert_eq!(
            resolve_reference_pattern(None, Some(meta)),
            (Some(TestPattern::SaturatedPrimaries), None)
        );
    }

    #[test]
    fn agreeing_flag_and_metadata_produce_no_note() {
        let meta = RoundtripMeta {
            pattern: TestPattern::ChromaDetail,
            width: 1920,
            height: 1080,
        };
        assert_eq!(
            resolve_reference_pattern(Some(TestPattern::ChromaDetail), Some(meta)),
            (Some(TestPattern::ChromaDetail), None)
        );
    }

    #[test]
    fn disagreeing_flag_and_metadata_disable_measurement_and_explain_why() {
        let meta = RoundtripMeta {
            pattern: TestPattern::GreyRamp,
            width: 1920,
            height: 1080,
        };
        let (pattern, note) =
            resolve_reference_pattern(Some(TestPattern::ChromaDetail), Some(meta));
        assert_eq!(
            pattern, None,
            "a disagreement must disable measurement rather than guess which side is right"
        );
        let note = note.expect("a disagreement must explain itself");
        assert!(note.contains("chroma_detail"));
        assert!(note.contains("grey_ramp"));
    }

    // ---- RawRoundtripMeta / RoundtripMeta deserialisation ----

    #[test]
    fn roundtrip_meta_json_deserialises_and_resolves_its_pattern_token() {
        let json = r#"{"pattern": "grey_ramp", "width": 1920, "height": 1080}"#;
        let raw: RawRoundtripMeta = serde_json::from_str(json).expect("valid roundtrip-meta.json");
        assert_eq!(raw.pattern, "grey_ramp");
        assert_eq!(raw.width, 1920);
        assert_eq!(raw.height, 1080);
        assert_eq!(
            TestPattern::from_token(&raw.pattern),
            Some(TestPattern::GreyRamp)
        );
    }

    #[test]
    fn roundtrip_meta_json_with_an_unknown_pattern_token_is_not_a_deserialisation_error() {
        // `read_roundtrip_meta` treats an unrecognised pattern token as a
        // `None` (best-effort cross-check), not a hard parse failure --
        // this documents that boundary at the type level: parsing the raw
        // JSON always succeeds if the shape is right, and resolving the
        // token to a real `TestPattern` is a separate, fallible step.
        let json = r#"{"pattern": "not_a_real_pattern", "width": 640, "height": 480}"#;
        let raw: RawRoundtripMeta = serde_json::from_str(json).expect("shape is still valid");
        assert_eq!(TestPattern::from_token(&raw.pattern), None);
    }
}
