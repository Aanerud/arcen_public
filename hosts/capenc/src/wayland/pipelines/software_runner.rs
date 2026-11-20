// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
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
//! 4. hand that frame to the shared software loop
//!    (`arcen_media::canvas::CanvasEncodeLoop`, ADR 0028) with the
//!    producer's damage regions and release it; then, at the served
//!    cadence, hand it the newest leased frame whenever it wants one;
//! 5. `READY` before the first record (`backend=openh264-sw-h264`,
//!    `capture=pipewire`, `conversion_backend=cpu`), then the loop's records
//!    (canvas records for a canvas Deck, framed access units otherwise) and
//!    a stats line every second.
//!
//! As on the NVENC path, a quiet stream is not a dead one; the epoch ends
//! when the source closes, the producer renegotiates another size, or the
//! parent closes stdin.

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use arcen_keel::{BgraFrame, ExternalDamage};
use arcen_media::canvas::{CanvasEncodeLoop, LoopError, LoopTick};
use arcen_media::capenc_control::CapencControlCommand;
use arcen_media::video::{
    open_software_loop, AdmissionRequest, BackendUnavailableReason, CaptureAdmissionNotice,
    CaptureBackend, CaptureMemory, CaptureTakeError, ConversionBackend, CursorEvidence,
    DamageEvidence, EncoderBackend, FirstFrameEvidence, PresetAdmission, SoftwareLoop,
    SoftwareLoopConfig, SoftwareSurfaces, SustainedRateMeter,
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
        return crate::admission_probe::run_software_probe(
            sdr8_video(VideoCodec::H264),
            super::software::software_fps(request.fps),
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
            sdr_transfer: request.video.transfer,
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
    // The loop (OpenH264 for the served plan) is opened before the decision
    // is announced, as NVENC is on the native path.
    let SoftwareLoop {
        encode: mut encoder,
        bitrate_bounds: (start_bps, ceiling_bps),
        threading,
    } = match open_software_loop(SoftwareLoopConfig {
        width,
        height,
        fps: plan.fps,
        video: plan.video,
        canvas: request.canvas,
        start_bitrate_bps: None,
        keepalive: IDLE_KEEPALIVE,
    }) {
        Ok(built) => built,
        Err(error) => {
            log(&format!("ERROR: {error}"));
            return 4;
        }
    };
    if let Err(error) = crate::announce_admission(&notice) {
        log(&format!("ERROR: emit ADMISSION: {error}"));
        return 5;
    }
    let mut os_damage = ExternalDamage::new(width as usize, height as usize).ok();
    let mut retained = Vec::new();
    let mut narrowed = false;
    match copy_lease(&first, &mut retained, &mut os_damage, &mut narrowed) {
        Ok(true) => {}
        Ok(false) => {
            log("ERROR: the first frame was revoked before it was read");
            return 3;
        }
        Err(error) => {
            log(&format!("ERROR: Wayland software loop: {error}"));
            return 5;
        }
    }
    drop(first);
    let first_tick = match tick_retained(
        &mut encoder,
        &retained,
        (width, height),
        None,
        Instant::now(),
    ) {
        Ok(tick) => tick,
        Err(error) => {
            log(&format!("ERROR: Wayland software loop: {error}"));
            return 5;
        }
    };
    log(&format!(
        "Wayland epoch serving {} for {} at up to {} fps: priority={} intent={} codec={} \
         backend={} copy_path={} route={} memory={} colour={} cursor={} {threading} \
         bitrate_start={start_bps} bitrate_ceiling={ceiling_bps} damage=keel canvas={}{}",
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
        if encoder.is_canvas() { "on" } else { "off" },
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
    let mut ready_announced = false;
    let mut second = Instant::now();
    let mut rate = SustainedRateMeter::new(Instant::now());
    let source_stats = &source.shared().stats;
    let leases = source.lease_counters();
    let mut source_frames_seen = source_stats.frames.load(Ordering::Relaxed);
    let mut superseded_seen = leases.superseded.load(Ordering::Relaxed);
    let (mut emitted, mut bytes, mut keyframes, mut changed) = (0u64, 0u64, 0u64, 0u64);
    let (mut total_emitted, mut total_bytes, mut total_keyframes) = (0u64, 0u64, 0u64);
    // Cursor events only after READY.
    let mut epoch_output = super::output::EpochOutput::new(request, cursor);
    let mut pending = Some(first_tick);
    let mut last_error: Option<Instant> = None;

    let exit = 'epoch: loop {
        if control.stop_requested() {
            break 0;
        }
        if ready_announced && epoch_output.cursor(&mut stdout, &source.cursor()).is_err() {
            break 'epoch 0;
        }
        if let Some(fps) = control.take_framerate_fps() {
            let fps = plan.live_fps(fps);
            if encoder.set_frame_rate(fps) {
                target_dt = crate::frame_interval_from_fps(fps);
                log(&format!("OpenH264 live framerate reconfigured: fps={fps}"));
            }
        }
        if let Some(bps) = control.take_bitrate_bps() {
            encoder.set_link_bitrate(bps);
            log(&format!("software loop live bitrate: target={bps}bps"));
        }
        let now = Instant::now();
        for command in control.take_canvas_controls() {
            match command {
                CapencControlCommand::CanvasInput {
                    monitor_id,
                    topology_generation,
                    x,
                    y,
                } => encoder.note_input(monitor_id, topology_generation, x, y, now),
                command => {
                    if let Some(message) = command.canvas_control() {
                        encoder.on_control(&message);
                    }
                }
            }
        }
        if control.take_idr() {
            encoder.request_recovery();
        }
        if now < next && pending.is_none() {
            std::thread::sleep((next - now).min(Duration::from_millis(50)));
            continue;
        }
        // The newest frame only; anything older was already returned to the
        // producer with its damage merged into this one's.
        let tick = match pending.take() {
            Some(tick) => Some(Ok(tick)),
            None => {
                let delivered = match source.next_frame(Duration::ZERO) {
                    Ok(Some(lease)) => {
                        match copy_lease(&lease, &mut retained, &mut os_damage, &mut narrowed) {
                            Ok(read) => read,
                            Err(error) => {
                                log(&format!("ERROR: Wayland software loop: {error}"));
                                break 5;
                            }
                        }
                    }
                    Ok(None) => false,
                    Err(CaptureTakeError::Closed) => {
                        log(&format!(
                            "ERROR: Wayland capture: the stream closed (phase {:?})",
                            source.phase()
                        ));
                        break 3;
                    }
                };
                if delivered {
                    changed += 1;
                } else if let Some(damage) = os_damage.as_mut() {
                    // Nothing new: the retained picture, unchanged.
                    damage.reset();
                    narrowed = true;
                }
                (delivered || encoder.wants_frame(now, false)).then(|| {
                    tick_retained(
                        &mut encoder,
                        &retained,
                        (width, height),
                        if narrowed { os_damage.as_ref() } else { None },
                        now,
                    )
                })
            }
        };
        if let Some(current) = source.current_format() {
            if (current.width, current.height) != (width, height) {
                log(&format!(
                    "ERROR: Wayland capture: the producer renegotiated {}x{}; this epoch ends",
                    current.width, current.height
                ));
                break 3;
            }
        }
        match tick {
            Some(Ok(tick)) => {
                if let Some(event) = tick.event {
                    log(&format!("canvas governor: {event:?}"));
                }
                if let Some((generation, digest)) = tick.digest {
                    log(&format!(
                        "canvas mirror digest {}",
                        arcen_media::canvas::digest_fields(generation, &digest)
                    ));
                }
                for record in &tick.records {
                    if !ready_announced {
                        if let Err(error) = crate::announce_ready_from_with_conversion(
                            ready,
                            Some(CaptureBackend::PipeWire),
                            Some(ConversionBackend::Cpu),
                        ) {
                            log(&format!("ERROR: emit READY: {error}"));
                            break 'epoch 5;
                        }
                        ready_announced = true;
                    }
                    if record.is_recovery_point() {
                        keyframes += 1;
                    }
                    if epoch_output.loop_record(&mut stdout, record).is_err() {
                        break 'epoch 0;
                    }
                    bytes += record.bytes().len() as u64;
                }
                if !tick.records.is_empty() {
                    emitted += 1;
                }
            }
            Some(Err(LoopError::Geometry)) => {
                log("ERROR: Wayland capture: a picture of another size arrived");
                break 3;
            }
            Some(Err(error)) => {
                if last_error.is_none_or(|at| at.elapsed() >= Duration::from_secs(1)) {
                    log(&format!("software loop tick failed: {error}"));
                    last_error = Some(Instant::now());
                }
            }
            None => {}
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
            let superseded = leases.superseded.load(Ordering::Relaxed);
            let canvas_on = encoder.is_canvas();
            let state = encoder.governor_state();
            log(&format!(
                "enc_fps={emitted} source_fps={} source_superseded={} kbps={} \
                 keyframes={keyframes} backend={} capture_backend=pipewire copy_path={} route={} \
                 served={} fps_ceiling={} want_idr={} damage_source=pipewire_meta cursor={} {}",
                // The rate the Pier's governor judges the encoder against:
                // pictures the compositor delivered (it delivers only on a
                // change), including those superseded while the loop was busy.
                changed.saturating_add(superseded - superseded_seen),
                superseded - superseded_seen,
                bytes * 8 / 1000,
                BACKEND.ready_token(),
                ROUTE.copy_path().token(),
                ROUTE.token(),
                plan.pipeline.token(),
                plan.fps,
                control.idr_pending(),
                cursor.token(),
                encoder.take_stats().line(canvas_on, state),
            ));
            superseded_seen = superseded;
            total_emitted += emitted;
            total_bytes += bytes;
            total_keyframes += keyframes;
            (emitted, bytes, keyframes, changed) = (0, 0, 0, 0);
            second = now;
        }
    };
    log(&format!(
        "Wayland epoch summary served={} requested={} backend={} emitted={} keyframes={} \
         bytes={} source_frames={} superseded={} \
         best_sustained_source_fps={} cursor_events={} cursor_unknown_formats={}",
        plan.pipeline.token(),
        requested.preset().pipeline().token(),
        BACKEND.ready_token(),
        total_emitted + emitted,
        total_keyframes + keyframes,
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

/// Copies a leased shared-memory frame into `retained` (packed rows) while
/// the lease is held, and puts the damage the producer reported for it and
/// every frame it superseded into `os_damage`. The loop then runs on the
/// retained picture, so the lease goes back to the compositor at once and a
/// keepalive, a reset or deferred work never waits for the next delivery.
/// `Ok(false)`: the lease was revoked (renegotiation) and nothing was read.
fn copy_lease(
    lease: &arcen_media::video::CaptureLease<BufferToken>,
    retained: &mut Vec<u8>,
    os_damage: &mut Option<ExternalDamage>,
    narrowed: &mut bool,
) -> Result<bool, String> {
    let frame = lease.frame();
    let surface = frame.surface.surface();
    if surface.memory != CaptureMemory::HostShared {
        return Err(format!(
            "a frame arrived in {} memory; only shared memory is read",
            surface.memory.token()
        ));
    }
    // PipeWire's damage meta narrows Keel's hash; without it Keel hashes
    // the whole picture.
    *narrowed = match (os_damage.as_mut(), &frame.damage) {
        (Some(damage), DamageEvidence::Regions(regions)) => {
            damage.reset();
            for region in regions {
                damage.mark_rect(*region);
            }
            true
        }
        _ => false,
    };
    let width = surface.geometry.coded_width as usize;
    let height = surface.geometry.coded_height as usize;
    let plane = surface.planes[0];
    let stride = plane.stride as usize;
    let result = frame.token.read(|views| -> Result<(), String> {
        let PlaneView::Host(buffer) = views[usize::from(plane.buffer)] else {
            return Err("the shared-memory plane is not mapped".to_owned());
        };
        let rows = plane_bytes(buffer, plane.offset, plane.size)
            .ok_or("the plane lies outside its mapping")?;
        let picture = BgraFrame::new(rows, width, height, stride)
            .map_err(|error| format!("BGRx layout: {error}"))?;
        retained.resize(width * height * 4, 0);
        for row in 0..height {
            let source = picture
                .active_row(row)
                .ok_or("a row lies outside the picture")?;
            retained[row * width * 4..(row + 1) * width * 4].copy_from_slice(source);
        }
        Ok(())
    });
    match result {
        Ok(read) => read.map(|()| true),
        Err(_) => Ok(false),
    }
}

/// Runs one tick of the loop on the retained picture.
fn tick_retained(
    encoder: &mut CanvasEncodeLoop<SoftwareSurfaces>,
    retained: &[u8],
    (width, height): (u32, u32),
    os_damage: Option<&ExternalDamage>,
    now: Instant,
) -> Result<LoopTick, LoopError> {
    let picture = BgraFrame::new(
        retained,
        width as usize,
        height as usize,
        width as usize * 4,
    )
    .map_err(|_| LoopError::Geometry)?;
    encoder.tick(picture, os_damage, now)
}
