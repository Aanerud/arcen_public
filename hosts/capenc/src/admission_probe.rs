//! Bounded child-process protocol for aggregate encoder admission.
//!
//! The Pier starts one finite `capenc admission-v1` child per exact planned
//! encoder binding. Platform backends generate representative content and
//! report one sample per requested frame; this module owns strict argument
//! parsing, pacing, bounded process supervision, and trace parsing.

#![cfg_attr(not(any(test, windows, target_os = "linux")), allow(dead_code))]

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use arcen_media::{
    DirtyRatio, EncoderProbeFailure, EncoderProbeRequest, EncoderProbeSample, EncoderProbeTrace,
    RepresentativeFrame, RepresentativeFrameKind, MAX_ENCODER_PROBE_FRAMES_PER_REGION,
    MAX_ENCODER_PROBE_WARMUP_FRAMES, MAX_ENCODER_PROBE_WINDOW,
};

pub const ADMISSION_PROBE_V1: &str = "admission-v1";
const SAMPLE_PREFIX: &str = "ARCEN-ENCODER-PROBE-SAMPLE";
const DONE_PREFIX: &str = "ARCEN-ENCODER-PROBE-DONE";
const MAX_CHILD_OUTPUT_BYTES: usize = 128 * 1024;
const CHILD_POLL_INTERVAL: Duration = Duration::from_millis(5);

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AdmissionProbeOptions {
    pub width: u32,
    pub height: u32,
    pub measurement_window: Duration,
    pub warmup_frames: u16,
    pub sample_frames: Vec<RepresentativeFrame>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ProbeFrameInput {
    pub kind: RepresentativeFrameKind,
    pub dirty_ratio: DirtyRatio,
    pub force_idr: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ProbeEncodeResult {
    pub encode_latency: Duration,
    pub delivered: bool,
}

pub(crate) fn options_from_args(args: &[String]) -> Result<Option<AdmissionProbeOptions>, String> {
    if !args.iter().any(|argument| argument == ADMISSION_PROBE_V1) {
        return Ok(None);
    }
    let (width, height) = required_value(args, "probe-geometry=")?
        .split_once('x')
        .ok_or_else(|| "probe-geometry must be WIDTHxHEIGHT".to_string())
        .and_then(|(width, height)| {
            let width = parse_value::<u32>(width, "probe width")?;
            let height = parse_value::<u32>(height, "probe height")?;
            Ok((width, height))
        })?;
    if width == 0 || height == 0 {
        return Err("probe geometry must be nonzero".to_string());
    }
    let window_ns = parse_value::<u64>(required_value(args, "probe-window-ns=")?, "probe window")?;
    let measurement_window = Duration::from_nanos(window_ns);
    if measurement_window.is_zero() || measurement_window > MAX_ENCODER_PROBE_WINDOW {
        return Err("probe measurement window is outside its safety bound".to_string());
    }
    let warmup_frames = parse_value::<u16>(
        required_value(args, "probe-warmup=")?,
        "probe warm-up count",
    )?;
    if warmup_frames > MAX_ENCODER_PROBE_WARMUP_FRAMES {
        return Err("probe warm-up count exceeds its safety bound".to_string());
    }
    let pattern = required_value(args, "probe-pattern=")?;
    if pattern.is_empty() || pattern.len() > usize::from(MAX_ENCODER_PROBE_FRAMES_PER_REGION) {
        return Err("probe frame pattern is outside its safety bound".to_string());
    }
    let dirty_basis_points = parse_value::<u16>(
        required_value(args, "probe-dirty-bps=")?,
        "probe dirty ratio",
    )?;
    if dirty_basis_points > 10_000 {
        return Err("probe dirty ratio exceeds 10000 basis points".to_string());
    }
    let dirty_ratio = dirty_ratio(dirty_basis_points);
    let sample_frames = pattern
        .bytes()
        .enumerate()
        .map(|(index, token)| {
            let kind = match token {
                b's' => RepresentativeFrameKind::Sparse,
                b'm' => RepresentativeFrameKind::FullMotion,
                _ => return Err("probe pattern accepts only 's' or 'm'".to_string()),
            };
            let sequence =
                u16::try_from(index).map_err(|_| "probe sequence exceeds u16".to_string())?;
            Ok(RepresentativeFrame {
                sequence,
                kind,
                dirty_ratio: if kind == RepresentativeFrameKind::FullMotion {
                    DirtyRatio::FULL
                } else {
                    dirty_ratio
                },
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Some(AdmissionProbeOptions {
        width,
        height,
        measurement_window,
        warmup_frames,
        sample_frames,
    }))
}

fn dirty_ratio(basis_points: u16) -> DirtyRatio {
    DirtyRatio::from_basis_points(basis_points)
        .expect("admission-probe parser validated dirty ratio")
}

fn required_value<'a>(args: &'a [String], prefix: &str) -> Result<&'a str, String> {
    let mut values = args
        .iter()
        .filter_map(|argument| argument.strip_prefix(prefix));
    let value = values
        .next()
        .ok_or_else(|| format!("missing {prefix} admission-probe argument"))?;
    if values.next().is_some() {
        return Err(format!("duplicate {prefix} admission-probe argument"));
    }
    Ok(value)
}

fn parse_value<T>(value: &str, name: &str) -> Result<T, String>
where
    T: std::str::FromStr,
{
    value.parse().map_err(|_| format!("invalid {name}"))
}

pub(crate) fn run_probe_loop<W, F>(
    options: &AdmissionProbeOptions,
    mut writer: W,
    mut encode: F,
) -> Result<(), String>
where
    W: Write,
    F: FnMut(ProbeFrameInput) -> Result<ProbeEncodeResult, String>,
{
    let mut force_idr = true;
    for _ in 0..options.warmup_frames {
        encode(ProbeFrameInput {
            kind: RepresentativeFrameKind::FullMotion,
            dirty_ratio: DirtyRatio::FULL,
            force_idr,
        })?;
        force_idr = false;
    }

    let measurement_started = Instant::now();
    let frame_count = options.sample_frames.len();
    let window_ns = options.measurement_window.as_nanos();
    for (index, frame) in options.sample_frames.iter().enumerate() {
        let offset_ns = window_ns
            .saturating_mul(index as u128)
            .checked_div(frame_count as u128)
            .unwrap_or(0);
        let offset_ns =
            u64::try_from(offset_ns).map_err(|_| "probe schedule exceeds u64".to_string())?;
        let scheduled = measurement_started + Duration::from_nanos(offset_ns);
        let now = Instant::now();
        if now < scheduled {
            std::thread::sleep(scheduled - now);
        }
        let encode_started = Instant::now();
        let queue_age = encode_started.saturating_duration_since(scheduled);
        let result = encode(ProbeFrameInput {
            kind: frame.kind,
            dirty_ratio: frame.dirty_ratio,
            force_idr,
        })?;
        force_idr = false;
        writeln!(
            writer,
            "{SAMPLE_PREFIX} version=1 sequence={} kind={} queue_ns={} encode_ns={} delivered={}",
            frame.sequence,
            frame_kind_token(frame.kind),
            queue_age.as_nanos(),
            result.encode_latency.as_nanos(),
            result.delivered
        )
        .map_err(|error| format!("write probe sample: {error}"))?;
    }
    let measurement_end = measurement_started + options.measurement_window;
    let now = Instant::now();
    if now < measurement_end {
        std::thread::sleep(measurement_end - now);
    }
    let elapsed = measurement_started.elapsed();
    writeln!(
        writer,
        "{DONE_PREFIX} version=1 elapsed_ns={} samples={}",
        elapsed.as_nanos(),
        frame_count
    )
    .map_err(|error| format!("write probe completion: {error}"))?;
    writer
        .flush()
        .map_err(|error| format!("flush probe output: {error}"))
}

fn frame_kind_token(kind: RepresentativeFrameKind) -> &'static str {
    match kind {
        RepresentativeFrameKind::Sparse => "sparse",
        RepresentativeFrameKind::FullMotion => "full-motion",
    }
}

fn parse_frame_kind(value: &str) -> Result<RepresentativeFrameKind, String> {
    match value {
        "sparse" => Ok(RepresentativeFrameKind::Sparse),
        "full-motion" => Ok(RepresentativeFrameKind::FullMotion),
        _ => Err("invalid probe frame kind".to_string()),
    }
}

fn probe_args(request: &EncoderProbeRequest) -> Result<Vec<String>, EncoderProbeFailure> {
    let window_ns = u64::try_from(request.measurement_window.as_nanos())
        .map_err(|_| EncoderProbeFailure::invalid("probe window exceeds u64 nanoseconds"))?;
    let dirty_basis_points = request
        .sample_frames
        .iter()
        .filter(|frame| frame.kind == RepresentativeFrameKind::Sparse)
        .map(|frame| frame.dirty_ratio.basis_points())
        .next()
        .unwrap_or(0);
    if request
        .sample_frames
        .iter()
        .filter(|frame| frame.kind == RepresentativeFrameKind::Sparse)
        .any(|frame| frame.dirty_ratio.basis_points() != dirty_basis_points)
    {
        return Err(EncoderProbeFailure::invalid(
            "child probe requires one sparse dirty ratio per request",
        ));
    }
    let pattern = request
        .sample_frames
        .iter()
        .map(|frame| match frame.kind {
            RepresentativeFrameKind::Sparse => 's',
            RepresentativeFrameKind::FullMotion => 'm',
        })
        .collect::<String>();
    Ok(vec![
        ADMISSION_PROBE_V1.to_string(),
        format!(
            "probe-geometry={}x{}",
            request.plan.width, request.plan.height
        ),
        format!("probe-window-ns={window_ns}"),
        format!("probe-warmup={}", request.warmup_frames),
        format!("probe-pattern={pattern}"),
        format!("probe-dirty-bps={dirty_basis_points}"),
    ])
}

/// Runs one finite admission-probe child with a hard parent-owned deadline.
///
/// The caller must configure the exact platform binary, backend, output/GPU
/// selector, identity, and environment before calling this function.
///
/// # Errors
///
/// Returns a typed spawn/open, encode, deadline, or protocol failure.
pub fn run_admission_probe_child(
    command: &mut Command,
    request: &EncoderProbeRequest,
) -> Result<EncoderProbeTrace, EncoderProbeFailure> {
    command
        .args(probe_args(request)?)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|error| EncoderProbeFailure::context_open(format!("spawn probe: {error}")))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| EncoderProbeFailure::context_open("probe stdout is unavailable"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| EncoderProbeFailure::context_open("probe stderr is unavailable"))?;
    let stdout_reader = spawn_bounded_reader(stdout);
    let stderr_reader = spawn_bounded_reader(stderr);

    let deadline = Instant::now() + request.max_probe_duration;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(CHILD_POLL_INTERVAL),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = join_reader(stdout_reader);
                let _ = join_reader(stderr_reader);
                return Err(EncoderProbeFailure::encode(format!(
                    "poll probe child: {error}"
                )));
            }
        }
    };
    let stdout = join_reader(stdout_reader)?;
    let stderr = join_reader(stderr_reader)?;
    let stderr = bounded_detail(&stderr);
    let Some(status) = status else {
        // A timed-out probe is either stuck before its first encode or merely
        // too slow to finish the requested pattern. Only the partial samples
        // separate those, and they are discarded once the child is killed, so
        // state their count and cost in the failure detail.
        return Err(EncoderProbeFailure::deadline(format!(
            "probe exceeded {:?} after {} of {} sample(s); stderr={stderr}",
            request.max_probe_duration,
            partial_progress(&stdout),
            request.sample_frames.len()
        )));
    };
    if !status.success() {
        let kind = if stdout.is_empty() {
            EncoderProbeFailure::context_open
        } else {
            EncoderProbeFailure::encode
        };
        return Err(kind(format!("probe exited with {status}; stderr={stderr}")));
    }
    parse_trace(&stdout, request)
        .map_err(|detail| EncoderProbeFailure::invalid(format!("{detail}; stderr={stderr}")))
}

fn spawn_bounded_reader<R>(
    reader: R,
) -> std::thread::JoinHandle<Result<Vec<u8>, EncoderProbeFailure>>
where
    R: Read + Send + 'static,
{
    std::thread::spawn(move || read_bounded(reader))
}

fn read_bounded(mut reader: impl Read) -> Result<Vec<u8>, EncoderProbeFailure> {
    let mut output = Vec::new();
    let mut limited = reader.by_ref().take((MAX_CHILD_OUTPUT_BYTES + 1) as u64);
    limited
        .read_to_end(&mut output)
        .map_err(|error| EncoderProbeFailure::invalid(format!("read probe output: {error}")))?;
    if output.len() > MAX_CHILD_OUTPUT_BYTES {
        return Err(EncoderProbeFailure::invalid(
            "probe output exceeds its safety bound",
        ));
    }
    Ok(output)
}

fn join_reader(
    reader: std::thread::JoinHandle<Result<Vec<u8>, EncoderProbeFailure>>,
) -> Result<Vec<u8>, EncoderProbeFailure> {
    reader
        .join()
        .map_err(|_| EncoderProbeFailure::invalid("probe output reader panicked"))?
}

fn bounded_detail(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).trim().to_string()
}

/// Samples a killed child had already reported before its deadline, with the
/// worst per-frame cost among them: a slow encoder and a stuck one both time
/// out, and only this separates them.
fn partial_progress(stdout: &[u8]) -> String {
    let text = String::from_utf8_lossy(stdout);
    let samples: Vec<EncoderProbeSample> = text
        .lines()
        .filter_map(|line| line.strip_prefix(SAMPLE_PREFIX))
        .filter_map(|fields| parse_sample(fields).ok())
        .collect();
    let Some(slowest) = samples.iter().max_by_key(|sample| sample.encode_latency) else {
        return "0".to_string();
    };
    format!(
        "{} (slowest encode {:?}, queued {:?})",
        samples.len(),
        slowest.encode_latency,
        slowest.queue_age
    )
}

fn parse_trace(output: &[u8], request: &EncoderProbeRequest) -> Result<EncoderProbeTrace, String> {
    let text = std::str::from_utf8(output).map_err(|_| "probe output is not UTF-8".to_string())?;
    let mut samples = Vec::with_capacity(request.sample_frames.len());
    let mut elapsed = None;
    for line in text.lines().filter(|line| !line.is_empty()) {
        if let Some(fields) = line.strip_prefix(SAMPLE_PREFIX) {
            if elapsed.is_some() {
                return Err("probe sample followed completion".to_string());
            }
            samples.push(parse_sample(fields)?);
            continue;
        }
        if let Some(fields) = line.strip_prefix(DONE_PREFIX) {
            if elapsed.is_some() {
                return Err("duplicate probe completion".to_string());
            }
            let values = parse_fields(fields)?;
            require_version(&values)?;
            let sample_count = field::<usize>(&values, "samples")?;
            if sample_count != samples.len() {
                return Err("probe completion sample count mismatch".to_string());
            }
            elapsed = Some(Duration::from_nanos(field::<u64>(&values, "elapsed_ns")?));
            continue;
        }
        return Err("unknown probe output line".to_string());
    }
    let elapsed = elapsed.ok_or_else(|| "probe completion is missing".to_string())?;
    if samples.len() != request.sample_frames.len() {
        return Err("probe returned the wrong sample count".to_string());
    }
    for (expected, actual) in request.sample_frames.iter().zip(&samples) {
        if expected.sequence != actual.sequence || expected.kind != actual.kind {
            return Err("probe sample sequence or kind mismatch".to_string());
        }
    }
    Ok(EncoderProbeTrace { elapsed, samples })
}

fn parse_sample(fields: &str) -> Result<EncoderProbeSample, String> {
    let values = parse_fields(fields)?;
    require_version(&values)?;
    Ok(EncoderProbeSample {
        sequence: field(&values, "sequence")?,
        kind: parse_frame_kind(value(&values, "kind")?)?,
        queue_age: Duration::from_nanos(field(&values, "queue_ns")?),
        encode_latency: Duration::from_nanos(field(&values, "encode_ns")?),
        delivered: field(&values, "delivered")?,
    })
}

fn parse_fields(fields: &str) -> Result<Vec<(&str, &str)>, String> {
    fields
        .split_ascii_whitespace()
        .map(|field| {
            field
                .split_once('=')
                .ok_or_else(|| "probe field is missing '='".to_string())
        })
        .collect()
}

fn require_version(fields: &[(&str, &str)]) -> Result<(), String> {
    if value(fields, "version")? != "1" {
        return Err("unsupported probe protocol version".to_string());
    }
    Ok(())
}

fn value<'a>(fields: &'a [(&str, &str)], name: &str) -> Result<&'a str, String> {
    let mut values = fields
        .iter()
        .filter_map(|(field, value)| (*field == name).then_some(*value));
    let value = values
        .next()
        .ok_or_else(|| format!("probe field {name} is missing"))?;
    if values.next().is_some() {
        return Err(format!("probe field {name} is duplicated"));
    }
    Ok(value)
}

fn field<T>(fields: &[(&str, &str)], name: &str) -> Result<T, String>
where
    T: std::str::FromStr,
{
    value(fields, name)?
        .parse()
        .map_err(|_| format!("probe field {name} is invalid"))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use arcen_media::video::EncoderBackend;
    use arcen_media::{
        ActivityClass, BitDepth, BitrateBudgetKbps, ChromaSubsampling, EncoderBindingId,
        MediaStreamEpoch, RegionActivityProfile, RegionAdmissionPriority, RegionGeneration,
        RegionId, RegionMediaPlan, SessionMonitorId, VideoCodec, VideoConfiguration,
    };

    use super::*;

    fn request() -> EncoderProbeRequest {
        let monitor_id = SessionMonitorId::new(1).expect("monitor");
        EncoderProbeRequest {
            candidate_index: 0,
            plan: RegionMediaPlan::new(
                monitor_id,
                MediaStreamEpoch::new(1).expect("epoch"),
                EncoderBackend::NativeNvenc,
                VideoConfiguration {
                    codec: VideoCodec::H264,
                    chroma: ChromaSubsampling::Yuv420,
                    ..VideoConfiguration::legacy_h264()
                },
                1_920,
                1_080,
                2,
                BitrateBudgetKbps::nominal_for_geometry(1_920, 1_080, 2),
            )
            .expect("plan"),
            binding_id: EncoderBindingId::new("test-binding").expect("binding"),
            activity: RegionActivityProfile {
                session_monitor_id: monitor_id,
                region_generation: RegionGeneration::new(1).expect("generation"),
                region_id: RegionId::new(1).expect("region"),
                activity_class: ActivityClass::FullMotion,
                dirty_ratio: DirtyRatio::FULL,
                target_fps: 2,
                priority: RegionAdmissionPriority::Standard,
            },
            measurement_window: Duration::from_secs(1),
            max_probe_duration: Duration::from_secs(5),
            warmup_frames: 2,
            sample_frames: vec![
                RepresentativeFrame {
                    sequence: 0,
                    kind: RepresentativeFrameKind::Sparse,
                    dirty_ratio: DirtyRatio::ZERO,
                },
                RepresentativeFrame {
                    sequence: 1,
                    kind: RepresentativeFrameKind::FullMotion,
                    dirty_ratio: DirtyRatio::FULL,
                },
            ],
        }
    }

    #[test]
    fn arguments_round_trip_into_bounded_options() {
        let request = request();
        let mut args = vec!["arcen-capenc".to_string()];
        args.extend(probe_args(&request).expect("args"));
        let options = options_from_args(&args).expect("parse").expect("probe");
        assert_eq!(options.width, 1_920);
        assert_eq!(options.height, 1_080);
        assert_eq!(options.warmup_frames, 2);
        assert_eq!(options.sample_frames, request.sample_frames);
    }

    #[test]
    fn trace_parser_preserves_an_865_ms_encode_stall() {
        let request = request();
        let output = concat!(
            "ARCEN-ENCODER-PROBE-SAMPLE version=1 sequence=0 kind=sparse queue_ns=1000000 encode_ns=865000000 delivered=true\n",
            "ARCEN-ENCODER-PROBE-SAMPLE version=1 sequence=1 kind=full-motion queue_ns=2000000 encode_ns=865000000 delivered=true\n",
            "ARCEN-ENCODER-PROBE-DONE version=1 elapsed_ns=1732000000 samples=2\n"
        );
        let trace = parse_trace(output.as_bytes(), &request).expect("trace");
        assert_eq!(trace.samples[0].encode_latency, Duration::from_millis(865));
        assert_eq!(trace.elapsed, Duration::from_millis(1_732));
    }

    #[test]
    fn runner_emits_exact_requested_samples_after_warmup() {
        let request = request();
        let mut args = vec!["arcen-capenc".to_string()];
        args.extend(probe_args(&request).expect("args"));
        let options = options_from_args(&args).expect("parse").expect("probe");
        let mut output = Vec::new();
        let mut kinds = BTreeMap::new();
        run_probe_loop(&options, &mut output, |input| {
            *kinds.entry(frame_kind_token(input.kind)).or_insert(0usize) += 1;
            Ok(ProbeEncodeResult {
                encode_latency: Duration::from_millis(5),
                delivered: true,
            })
        })
        .expect("run");
        let trace = parse_trace(&output, &request).expect("trace");
        assert_eq!(trace.samples.len(), 2);
        assert_eq!(kinds.get("full-motion"), Some(&3));
        assert_eq!(kinds.get("sparse"), Some(&1));
    }

    #[test]
    fn a_timed_out_probe_reports_the_samples_it_had_already_produced() {
        let partial = concat!(
            "ARCEN-ENCODER-PROBE-SAMPLE version=1 sequence=0 kind=full-motion queue_ns=0 encode_ns=5000000 delivered=true\n",
            "ARCEN-ENCODER-PROBE-SAMPLE version=1 sequence=1 kind=sparse queue_ns=1000000 encode_ns=1900000000 delivered=true\n",
        );
        let progress = partial_progress(partial.as_bytes());
        assert!(progress.starts_with('2'), "{progress}");
        assert!(progress.contains("1.9s"), "{progress}");
        assert_eq!(partial_progress(b""), "0");
        assert_eq!(
            partial_progress(b"ARCEN-ENCODER-PROBE-DONE version=1 elapsed_ns=1 samples=0\n"),
            "0"
        );
    }
}
