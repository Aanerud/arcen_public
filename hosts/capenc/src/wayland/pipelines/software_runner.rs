//! The epoch runner of `linux-wayland-software` (see [`super::software`]).
//!
//! One epoch, in order, mirroring the NVENC runner without CUDA:
//!
//! 1. refuse what the Software contract cannot serve (quality intent, a QP
//!    map); an `admission-v1` request probes `OpenH264` with synthetic
//!    pictures at the hand-off's exact size and ends;
//! 2. start the `PipeWire` source offering only shared-memory BGRx/BGRA at
//!    no more than the Software ceiling, at the hand-off's size;
//! 3. take the first real frame, build source evidence (`host-copy`, no GPU
//!    claim) and run the requested contract's shared admission with
//!    `OpenH264`; open the session for the served plan; state the decision
//!    (`ADMISSION`);
//! 4. stage that frame (Keel hash, converted rows) and release it; then, at
//!    the served cadence, stage the newest leased frame and encode as Keel's
//!    idle cadence decides;
//! 5. `READY` after the first access unit (`backend=openh264-sw-h264`,
//!    `capture=pipewire`, `conversion_backend=cpu`), then framed access
//!    units and a stats line every second.
//!
//! As on the NVENC path, a quiet stream is not a dead one; the epoch ends
//! when the source closes, the producer renegotiates another size, or the
//! parent closes stdin.

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use arcen_keel::{BgraFrame, EmitMode};
use arcen_media::video::{
    AdmissionRequest, BackendUnavailableReason, CaptureAdmissionNotice, CaptureBackend,
    CaptureMemory, CaptureTakeError, ConversionBackend, CursorEvidence, EncoderBackend,
    FirstFrameEvidence, LiveChange, PresetAdmission, SoftwareEncodeLoop, SoftwareH264Session,
    SoftwareSessionConfig, StagedPicture, SustainedRateMeter,
};
use arcen_media::VideoCodec;

use super::software::SoftwarePipeline;
use super::{sdr8_video, EpochRequest, WaylandPipeline};
use crate::admission_probe::AdmissionProbeOptions;
use crate::log;
use crate::wayland::evidence::{capture_source_evidence, EpochFacts, ImportRoute};
use crate::wayland::handoff::WaylandHandoff;
use crate::wayland::host_copy::plane_bytes;
use crate::wayland::pipewire::{BufferToken, PipeWireSource, PlaneView, SourceConfig};

const NEGOTIATION_TIMEOUT: Duration = Duration::from_secs(5);
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(8);
const IDLE_KEEPALIVE: Duration = Duration::from_secs(1);
const MAILBOX_CAPACITY: usize = 2;
const BACKEND: EncoderBackend = EncoderBackend::OpenH264;
const ROUTE: ImportRoute = ImportRoute::HostCopyConvert;

fn unavailable(reason: BackendUnavailableReason) -> i32 {
    let _ = crate::announce_unavailable(BACKEND, reason);
    2
}

/// Runs one software epoch. Returns the process exit code.
pub fn run(
    request: &EpochRequest,
    handoff: &WaylandHandoff,
    requested: WaylandPipeline,
    cursor: CursorEvidence,
    admission_probe: Option<&AdmissionProbeOptions>,
) -> i32 {
    if !crate::linux_software_policy_supported(request.intent, request.qp_map) {
        log(
            "ERROR: software-h264 supports only intent=interactive and qp-map=off; quality and \
             QP delta maps require native NVENC",
        );
        return unavailable(BackendUnavailableReason::UnsupportedConfiguration);
    }
    if request.probe {
        log("ERROR: probe-v1 probes a native encoder; the software encoder needs no probe");
        return unavailable(BackendUnavailableReason::UnsupportedConfiguration);
    }
    let software = SoftwarePipeline::new(requested);
    let (width, height) = handoff.size();
    if let Some(options) = admission_probe {
        if (options.width, options.height) != (width, height) {
            log(&format!(
                "ERROR: admission probe geometry {}x{} differs from the Wayland output {width}x{height}",
                options.width, options.height
            ));
            return 2;
        }
        let fps = software.contract().max_fps.min(request.fps.max(1));
        return crate::admission_probe::run_software_probe(
            sdr8_video(VideoCodec::H264),
            fps,
            options,
        );
    }

    let mut offer = software.offer(request.fps);
    // Exactly the hand-off's size: a `RecordVirtual` monitor takes the size
    // its consumer negotiates.
    offer.size = Some((width, height));
    log(&format!(
        "Wayland capture offer formats=bgrx8,bgra8 max_fps={} memory=shm encoder={} \
         (the software composition never offers DMA-BUF)",
        offer.max_fps,
        BACKEND.ready_token(),
    ));
    let source = match PipeWireSource::start(
        handoff,
        SourceConfig {
            offer,
            mailbox_capacity: MAILBOX_CAPACITY,
            source_gpu: None,
            encoder_gpu: None,
            dmabuf_copy_path: None,
            explicit_sync: None,
        },
    ) {
        Ok(source) => source,
        Err(error) => {
            log(&format!("ERROR: Wayland capture: {error}"));
            return 3;
        }
    };
    let Some(negotiated) = source.wait_negotiated(NEGOTIATION_TIMEOUT) else {
        log(&format!(
            "ERROR: Wayland capture: no format negotiated within {NEGOTIATION_TIMEOUT:?} (phase {:?})",
            source.phase()
        ));
        return 3;
    };
    if (negotiated.width, negotiated.height) != (width, height) {
        log(&format!(
            "ERROR: Wayland capture: negotiated {}x{} is not the hand-off's {width}x{height}",
            negotiated.width, negotiated.height
        ));
        return 3;
    }

    let first = match source.next_frame(FIRST_FRAME_TIMEOUT) {
        Ok(Some(lease)) => lease,
        Ok(None) => {
            log(&format!(
                "ERROR: Wayland capture: no first frame within {FIRST_FRAME_TIMEOUT:?} (phase {:?})",
                source.phase()
            ));
            return 3;
        }
        Err(CaptureTakeError::Closed) => {
            log("ERROR: Wayland capture: the stream closed before its first frame");
            return 3;
        }
    };
    let surface = first.frame().surface.surface();
    if surface.memory != CaptureMemory::HostShared {
        log(&format!(
            "ERROR: Wayland capture: the first frame arrived in {} memory; the software \
             composition reads shared memory only",
            surface.memory.token()
        ));
        return 3;
    }
    let first_frame = FirstFrameEvidence {
        node_id: handoff.node_id(),
        format: surface.format,
        width: surface.geometry.coded_width,
        height: surface.geometry.coded_height,
        memory: surface.memory,
    };
    let facts = EpochFacts {
        negotiated: source.current_format().unwrap_or(negotiated),
        memory: surface.memory,
        route: ROUTE,
        gpu: first.frame().surface.gpu_match(),
        cursor,
        measured_fps: None,
    };
    let Ok(evidence) = capture_source_evidence(&facts) else {
        // A host copy never claims a GPU; this cannot refuse.
        log("ERROR: the shared-memory source evidence was refused");
        return 5;
    };
    let admission = software.admit(
        AdmissionRequest {
            preset: requested.preset(),
            requested_codec: request.codec,
            width,
            height,
            max_fps: request.fps,
        },
        &evidence,
    );
    let notice = CaptureAdmissionNotice {
        requested: requested.preset(),
        requested_fps: request.fps,
        admission,
        capture: CaptureBackend::PipeWire,
        copy_path: ROUTE.copy_path(),
        cursor,
        measured_fps: None,
        // No rate is claimed: the Software contract needs no rate evidence.
        rate_evidence: arcen_media::video::CaptureRateEvidence::default(),
        frame: first_frame,
    };
    let Some(served) = admission.served().copied() else {
        let _ = crate::announce_admission(&notice);
        log(&format!(
            "ERROR: the Wayland source cannot be served in software as {}: {}",
            requested.preset().pipeline().token(),
            match admission {
                PresetAdmission::Unavailable { reason, .. } => reason.to_string(),
                _ => String::new(),
            }
        ));
        return unavailable(BackendUnavailableReason::UnsupportedConfiguration);
    };
    let Some(plan) = served.epoch() else {
        log("ERROR: the admitted plan states no cursor truth");
        return 5;
    };
    // OpenH264 is opened for the served plan before the decision is
    // announced, as NVENC is on the native path.
    let session = match SoftwareH264Session::new(SoftwareSessionConfig::for_stream(
        width, height, plan.fps, plan.video,
    )) {
        Ok(session) => session,
        Err(error) => {
            log(&format!("ERROR: OpenH264 initialization failed: {error}"));
            return 4;
        }
    };
    let (start_bps, ceiling_bps) = session.bitrate_bounds();
    let threads = session.encoder_config().num_threads;
    let mut encoder = SoftwareEncodeLoop::new(session, IDLE_KEEPALIVE);
    if let Err(error) = crate::announce_admission(&notice) {
        log(&format!("ERROR: emit ADMISSION: {error}"));
        return 5;
    }
    let first_convert = match stage_lease(&mut encoder, &first) {
        Ok(Some(_)) => encoder.take_stats().convert_ms_sum,
        Ok(None) => {
            log("ERROR: the first frame was revoked before it was read");
            return 3;
        }
        Err(error) => {
            log(&format!("ERROR: Wayland software stage: {error}"));
            return 5;
        }
    };
    drop(first);
    log(&format!(
        "Wayland epoch serving {} for {} at up to {} fps: priority={} intent={} codec={} \
         backend={} copy_path={} route={} memory={} colour={} cursor={} threads={threads} \
         bitrate_start={start_bps} bitrate_ceiling={ceiling_bps} damage={} \
         first_convert_ms={first_convert:.2}{}",
        plan.pipeline.token(),
        requested.preset().pipeline().token(),
        plan.fps,
        plan.priority.token(),
        plan.intent.token(),
        plan.video.codec.token(),
        BACKEND.ready_token(),
        served.copy_path.token(),
        ROUTE.token(),
        facts.memory.token(),
        if served.colour_assumed {
            "bt709-limited-assumed"
        } else {
            "bt709-limited-reported"
        },
        cursor.token(),
        if encoder.damage_tracked() {
            "keel"
        } else {
            "full"
        },
        match admission {
            PresetAdmission::Downgrade { reason, .. } => format!(
                " downgraded_from={} reason={}",
                requested.preset().pipeline().token(),
                reason.token()
            ),
            _ => String::new(),
        },
    ));

    let color = crate::ColorSpec {
        chroma: plan.video.chroma,
        bit_depth: plan.video.bit_depth,
        range: plan.video.range,
        matrix: plan.video.matrix,
        primaries: plan.video.primaries,
        transfer: plan.video.transfer,
    };
    let cursor_capture = match plan.cursor_mode {
        arcen_protocol::messages::CursorMode::Local => crate::CursorCaptureMode::Local,
        arcen_protocol::messages::CursorMode::Host => crate::CursorCaptureMode::Host,
    };
    let ready = match crate::resolved_media_plan_for_source(
        BACKEND,
        plan.video.codec.token(),
        color,
        width,
        height,
        plan.fps,
        cursor_capture,
        cursor,
    ) {
        Ok(ready) => ready,
        Err(error) => {
            log(&format!("ERROR: {error}"));
            return 5;
        }
    };

    let control = crate::spawn_control_thread("OpenH264-Wayland");
    let mut stdout = std::io::stdout();
    let mut target_dt = crate::frame_interval_from_fps(plan.fps);
    let mut next = Instant::now();
    let mut last_emit = Instant::now();
    let mut ready_announced = false;
    let mut second = Instant::now();
    let mut rate = SustainedRateMeter::new(Instant::now());
    let source_stats = &source.shared().stats;
    let leases = source.lease_counters();
    let mut source_frames_seen = source_stats.frames.load(Ordering::Relaxed);
    let mut superseded_seen = leases.superseded.load(Ordering::Relaxed);
    let (mut emitted, mut bytes, mut keyframes) = (0u64, 0u64, 0u64);
    let (mut total_emitted, mut total_bytes, mut total_keyframes) = (0u64, 0u64, 0u64);
    let mut total_modes = [0u64; 4];
    // Cursor events only after READY.
    let mut epoch_output = super::output::EpochOutput::new(request, cursor);

    let exit = 'epoch: loop {
        if control.stop_requested() {
            break 0;
        }
        if ready_announced && epoch_output.cursor(&mut stdout, &source.cursor()).is_err() {
            break 'epoch 0;
        }
        if let Some(fps) = control.take_framerate_fps() {
            match encoder
                .session_mut()
                .reconfigure_framerate(plan.live_fps(fps))
            {
                Ok(LiveChange::Applied(fps)) => {
                    target_dt = crate::frame_interval_from_fps(fps);
                    log(&format!("OpenH264 live framerate reconfigured: fps={fps}"));
                }
                Ok(LiveChange::Unchanged) => {}
                Err(error) => log(&format!(
                    "OpenH264 live framerate reconfigure failed: {error}"
                )),
            }
        }
        if let Some(bps) = control.take_bitrate_bps() {
            match encoder.session_mut().reconfigure_bitrate(bps) {
                Ok(LiveChange::Applied(bps)) => {
                    log(&format!(
                        "OpenH264 live bitrate reconfigured: target={bps}bps"
                    ));
                }
                Ok(LiveChange::Unchanged) => {}
                Err(error) => log(&format!(
                    "OpenH264 live bitrate reconfigure failed: {error}"
                )),
            }
        }
        let now = Instant::now();
        if now < next {
            std::thread::sleep((next - now).min(Duration::from_millis(50)));
            continue;
        }
        // The newest frame only; anything older was already returned to the
        // producer (its damage is re-measured by Keel's hash).
        match source.next_frame(Duration::ZERO) {
            Ok(Some(lease)) => {
                if let Err(error) = stage_lease(&mut encoder, &lease) {
                    log(&format!("ERROR: Wayland software stage: {error}"));
                    break 5;
                }
            }
            Ok(None) => {}
            Err(CaptureTakeError::Closed) => {
                log(&format!(
                    "ERROR: Wayland capture: the stream closed (phase {:?})",
                    source.phase()
                ));
                break 3;
            }
        }
        if let Some(current) = source.current_format() {
            if (current.width, current.height) != (width, height) {
                log(&format!(
                    "ERROR: Wayland capture: the producer renegotiated {}x{}; this epoch ends",
                    current.width, current.height
                ));
                break 3;
            }
        }

        let idr_pending = control.idr_pending();
        if let Some(mode) = encoder.decision(idr_pending, last_emit.elapsed()) {
            if idr_pending && control.take_idr() && mode != EmitMode::FirstFrame {
                log("consuming IDR request");
            }
            match encoder.encode(mode) {
                Ok(Some(access_unit)) => {
                    if !ready_announced {
                        if let Err(error) =
                            crate::validate_access_unit(access_unit.bytes, epoch_output.framed())
                        {
                            log(&format!(
                                "ERROR: first access unit failed output validation: {error}"
                            ));
                            break 5;
                        }
                        if let Err(error) = crate::announce_ready_from_with_conversion(
                            ready,
                            Some(CaptureBackend::PipeWire),
                            Some(ConversionBackend::Cpu),
                        ) {
                            log(&format!("ERROR: emit READY: {error}"));
                            break 5;
                        }
                        ready_announced = true;
                    }
                    if access_unit.is_keyframe {
                        keyframes += 1;
                    }
                    let length = access_unit.bytes.len() as u64;
                    if epoch_output
                        .access_unit(&mut stdout, access_unit.bytes)
                        .is_err()
                    {
                        break 'epoch 0;
                    }
                    emitted += 1;
                    bytes += length;
                    last_emit = now;
                }
                // OpenH264 skipped the picture; the cadence stays due.
                Ok(None) => {}
                Err(error) => {
                    log(&format!("ERROR: OpenH264 encode failed: {error}"));
                    break 5;
                }
            }
        }
        next += target_dt;
        if next < now {
            next = now + target_dt;
        }

        let now = Instant::now();
        let frames = source_stats.frames.load(Ordering::Relaxed);
        rate.record(
            now,
            u32::try_from(frames.saturating_sub(source_frames_seen)).unwrap_or(u32::MAX),
        );
        source_frames_seen = frames;
        if now.duration_since(second) >= Duration::from_secs(1) {
            let stats = encoder.take_stats();
            let superseded = leases.superseded.load(Ordering::Relaxed);
            log(&format!(
                "enc_fps={emitted} encode_submitted={} source_fps={} source_superseded={} \
                 staged={} clean_frames={} dirty_blocks={} avg_convert_ms={:.2} \
                 avg_encode_ms={:.2} max_encode_ms={:.2} kbps={} keyframes={keyframes} \
                 backend={} capture_backend=pipewire copy_path={} route={} served={} \
                 fps_ceiling={} emit_first={} emit_idr={} emit_activity={} emit_keepalive={} \
                 want_idr={} damage_source={} cursor={}",
                stats.submitted,
                rate.last_window().unwrap_or(0),
                superseded - superseded_seen,
                stats.staged,
                stats.clean,
                stats.dirty_blocks,
                average(stats.convert_ms_sum, stats.staged),
                average(stats.encode_ms_sum, stats.submitted),
                stats.encode_ms_max,
                bytes * 8 / 1000,
                BACKEND.ready_token(),
                ROUTE.copy_path().token(),
                ROUTE.token(),
                plan.pipeline.token(),
                plan.fps,
                stats.by_mode[0],
                stats.by_mode[1],
                stats.by_mode[2],
                stats.by_mode[3],
                control.idr_pending(),
                if encoder.damage_tracked() {
                    "keel_hash"
                } else {
                    "unknown_full"
                },
                cursor.token(),
            ));
            superseded_seen = superseded;
            total_emitted += emitted;
            total_bytes += bytes;
            total_keyframes += keyframes;
            for (total, mode) in total_modes.iter_mut().zip(stats.by_mode) {
                *total += mode;
            }
            (emitted, bytes, keyframes) = (0, 0, 0);
            second = now;
        }
    };
    log(&format!(
        "Wayland epoch summary served={} requested={} backend={} emitted={} keyframes={} \
         keepalives={} activity={} idr={} bytes={} source_frames={} superseded={} \
         best_sustained_source_fps={} cursor_events={} cursor_unknown_formats={}",
        plan.pipeline.token(),
        requested.preset().pipeline().token(),
        BACKEND.ready_token(),
        total_emitted + emitted,
        total_keyframes + keyframes,
        total_modes[3],
        total_modes[2],
        total_modes[1],
        total_bytes + bytes,
        source_stats.frames.load(Ordering::Relaxed),
        leases.superseded.load(Ordering::Relaxed),
        rate.best_sustained()
            .map_or_else(|| "unmeasured".to_owned(), |fps| fps.to_string()),
        epoch_output.cursor_events_sent(),
        epoch_output.unknown_cursor_formats(),
    ));
    drop(control);
    drop(encoder);
    drop(source);
    exit
}

fn average(sum: f64, count: u64) -> f64 {
    if count == 0 {
        0.0
    } else {
        sum / count as f64
    }
}

/// Stages a leased shared-memory frame into the encode loop while the lease
/// is held. `Ok(None)`: the lease was revoked (renegotiation) and nothing
/// was read.
fn stage_lease(
    encoder: &mut SoftwareEncodeLoop,
    lease: &arcen_media::video::CaptureLease<BufferToken>,
) -> Result<Option<StagedPicture>, String> {
    let frame = lease.frame();
    let surface = frame.surface.surface();
    if surface.memory != CaptureMemory::HostShared {
        return Err(format!(
            "a frame arrived in {} memory; only shared memory is read",
            surface.memory.token()
        ));
    }
    let width = surface.geometry.coded_width as usize;
    let height = surface.geometry.coded_height as usize;
    let plane = surface.planes[0];
    let stride = plane.stride as usize;
    let result = frame.token.read(|views| -> Result<_, String> {
        let PlaneView::Host(buffer) = views[usize::from(plane.buffer)] else {
            return Err("the shared-memory plane is not mapped".to_owned());
        };
        let rows = plane_bytes(buffer, plane.offset, plane.size)
            .ok_or("the plane lies outside its mapping")?;
        let picture = BgraFrame::new(rows, width, height, stride)
            .map_err(|error| format!("BGRx layout: {error}"))?;
        encoder.stage(picture).map_err(|error| error.to_string())
    });
    match result {
        Ok(staged) => staged.map(Some),
        Err(_) => Ok(None),
    }
}
