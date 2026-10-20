//! `capenc wayland-capture-probe`: the Wayland capture harness.
//!
//! Lab evidence only; not a product mode. Given the session hand-off in the
//! environment ([`super::handoff`]), it opens CUDA on the encoder GPU,
//! connects to the `PipeWire` node, captures `frames=N` frames through the
//! bounded lease path, stages each through the selected import route into a
//! CUDA/NVENC surface, optionally encodes the first `encode-frames=K` with
//! the existing NVENC adapter, and prints one line of facts per frame and a
//! summary. It never writes video to stdout unless `out=` names a file.
//! A stream whose negotiated size is not the hand-off's expected size is
//! refused: the node no longer records the output the session handed over.
//!
//! ```text
//! arcen-capenc wayland-capture-probe [frames=N] [codec=h264|h265]
//!     [encode-frames=K] [max-fps=F] [timeout-ms=T] [verify-upload=V]
//!     [offer-dmabuf] [verify-gpu-import] [out=/path/stream.bin]
//! ```
//!
//! `offer-dmabuf` offers DMA-BUF through the `gpu-convert` importer (the
//! probe is how a host verifies it, so it needs no prior verification).
//! `verify-gpu-import` is that verification: the first DMA-BUF frame is
//! imported on the GPU and read back, DMA-BUF is withdrawn, and the first
//! shared-memory frame of the same still desktop is the host-copy
//! reference; the result is one `wayland-probe gpu-import-verify` line.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use arcen_media::video::{
    CaptureLease, CaptureMemory, DamageEvidence, GpuIdentity, KeyframePolicy, MotionPriority,
};
use arcen_media::EncodeIntent;
use arcen_telemetry::{CorrelationId, OperationalProfile};

use super::evidence::{
    capture_path_event, capture_source_evidence, decide_import, DmabufImporterFacts, EpochFacts,
    ImportRoute,
};
use super::explicit_sync::SyncobjDevice;
use super::format::FormatOffer;
use super::gpu;
use super::gpu_convert::{AcquireSync, GpuConvertImporter, GpuImportReport};
use super::handoff::WaylandHandoff;
use super::host_copy::{plane_bytes, HostCopyImporter};
use super::pipewire::{PipeWireSource, PlaneView, SourceConfig};
use super::reference::{compare_bgr, discriminates_channel_order, FrameComparison};

/// The outcome of `verify-gpu-import`.
#[derive(Debug, Clone, Copy)]
struct GpuVerify {
    comparison: FrameComparison,
    discriminating: bool,
}

impl GpuVerify {
    const fn verified(self) -> bool {
        self.comparison.verified() && self.discriminating
    }
}
use super::telemetry::CaptureTelemetry;
use crate::linux::cuda;
use crate::log;
use arcen_media::video::PendingCaptureDamage;

/// Harness options.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeOptions {
    /// Frames to capture.
    pub frames: u32,
    /// Codec for the encode check.
    pub codec: String,
    /// Frames to encode (0 = upload only).
    pub encode_frames: u32,
    /// Highest frame rate to ask for.
    pub max_fps: u32,
    /// Per-frame wait.
    pub timeout: Duration,
    /// Frames whose CUDA upload is read back and compared.
    pub verify_upload: u32,
    /// Offer DMA-BUF through the `gpu-convert` importer.
    pub offer_dmabuf: bool,
    /// Verify the GPU import against a host-copy reference.
    pub verify_gpu_import: bool,
    /// Where to write encoded access units.
    pub out: Option<PathBuf>,
}

impl Default for ProbeOptions {
    fn default() -> Self {
        Self {
            frames: 60,
            codec: "h264".to_string(),
            encode_frames: 0,
            max_fps: 60,
            timeout: Duration::from_millis(5000),
            verify_upload: 3,
            offer_dmabuf: false,
            verify_gpu_import: false,
            out: None,
        }
    }
}

/// Parses `key=value` harness arguments after `wayland-capture-probe`.
///
/// # Errors
///
/// A description of the first unknown or malformed argument.
pub fn parse_options<'a>(args: impl IntoIterator<Item = &'a str>) -> Result<ProbeOptions, String> {
    let mut options = ProbeOptions::default();
    for argument in args {
        let number = |value: &str| {
            value
                .parse::<u32>()
                .map_err(|_| format!("{argument}: expected a number"))
        };
        match argument.split_once('=') {
            None if argument == "offer-dmabuf" => options.offer_dmabuf = true,
            None if argument == "verify-gpu-import" => {
                options.offer_dmabuf = true;
                options.verify_gpu_import = true;
            }
            Some(("frames", value)) => options.frames = number(value)?.max(1),
            Some(("codec", value)) if matches!(value, "h264" | "h265") => {
                options.codec = value.to_string();
            }
            Some(("encode-frames", value)) => options.encode_frames = number(value)?,
            Some(("max-fps", value)) => options.max_fps = number(value)?.clamp(1, 240),
            Some(("timeout-ms", value)) => {
                options.timeout = Duration::from_millis(u64::from(number(value)?.max(1)));
            }
            Some(("verify-upload", value)) => options.verify_upload = number(value)?,
            Some(("out", value)) if Path::new(value).is_absolute() => {
                options.out = Some(PathBuf::from(value));
            }
            _ => {
                return Err(format!(
                    "unknown wayland-capture-probe argument {argument:?}"
                ))
            }
        }
    }
    Ok(options)
}

/// Entry point; returns the process exit code.
pub fn run(args: &[String]) -> i32 {
    let options = match parse_options(
        args.iter()
            .skip_while(|argument| argument.as_str() != "wayland-capture-probe")
            .skip(1)
            .map(String::as_str),
    ) {
        Ok(options) => options,
        Err(error) => {
            log(&format!("ERROR: {error}"));
            return 2;
        }
    };
    let handoff = match super::handoff::from_process_environment() {
        Ok(handoff) => handoff,
        Err(error) => {
            log(&format!("ERROR: Wayland hand-off: {error}"));
            return 2;
        }
    };
    log(&format!(
        "wayland-probe handoff node={} output={} generation={} expected_size={}x{} \
         position={},{} cursor_mode={} socket={} encoder_pci={:?} compositor_pci={:?} \
         compositor_render_node={:?}",
        handoff.node_id(),
        handoff.output().as_str(),
        handoff.generation().get(),
        handoff.size().0,
        handoff.size().1,
        handoff.position().0,
        handoff.position().1,
        handoff.cursor_mode().token(),
        handoff.pipewire_socket().display(),
        handoff.encoder_gpu_pci().map(gpu::sysfs_pci_name),
        handoff.compositor_gpu_pci().map(gpu::sysfs_pci_name),
        handoff.compositor_render_node(),
    ));
    // SAFETY: CUDA is initialised and used only on this thread; every CUDA
    // object below is dropped before the context.
    unsafe { run_with_cuda(&handoff, &options) }
}

pub(crate) fn identities(
    handoff: &WaylandHandoff,
    cuda_pci: Option<&str>,
) -> (Option<GpuIdentity>, Option<GpuIdentity>) {
    let sys = Path::new("/sys");
    let encoder = cuda_pci
        .and_then(super::handoff::parse_pci_address)
        .or(handoff.encoder_gpu_pci())
        .map(|pci| gpu::identity_for_pci(sys, pci));
    let from_pci = handoff
        .compositor_gpu_pci()
        .map(|pci| gpu::identity_for_pci(sys, pci));
    let from_node = handoff
        .compositor_render_node()
        .map(|node| gpu::identity_for_render_node(sys, node));
    let compositor = gpu::compositor_identity(from_pci, from_node);
    if let gpu::CompositorGpu::Contradictory {
        from_pci,
        from_node,
    } = compositor
    {
        log(&format!(
            "wayland-probe compositor GPU facts contradict each other (pci {from_pci:?} vs \
             render node {from_node:?}); no same-GPU route can be proven"
        ));
    }
    (compositor.identity(), encoder)
}

#[allow(clippy::too_many_lines)]
unsafe fn run_with_cuda(handoff: &WaylandHandoff, options: &ProbeOptions) -> i32 {
    let context = match cuda::init_from_env() {
        Ok(context) => context,
        Err(error) => {
            log(&format!("ERROR: CUDA: {error}"));
            return 4;
        }
    };
    let (source_gpu, encoder_gpu) = identities(handoff, context.device_pci());
    let gpu_match = arcen_media::video::compare_gpus(source_gpu.as_ref(), encoder_gpu.as_ref());
    log(&format!(
        "wayland-probe gpu source={source_gpu:?} encoder={encoder_gpu:?} match={}",
        gpu_match.token()
    ));

    // The gpu-convert importer on the encoder GPU, when DMA-BUF is offered.
    let (width, height) = handoff.size();
    let render_node = encoder_gpu
        .and_then(|identity| identity.render_node_minor)
        .map(|minor| PathBuf::from(format!("/dev/dri/renderD{minor}")));
    let mut gpu_importer = None;
    let mut syncobj = None;
    let dmabuf_facts = render_node
        .as_deref()
        .filter(|_| options.offer_dmabuf)
        .and_then(|node| match GpuConvertImporter::open(node, width, height) {
            Ok(importer) => {
                let bgra =
                    importer.offerable_modifiers(arcen_media::video::CapturePixelFormat::Bgra8);
                let modifiers: Vec<_> = importer
                    .offerable_modifiers(arcen_media::video::CapturePixelFormat::Bgrx8)
                    .into_iter()
                    .filter(|modifier| bgra.contains(modifier))
                    .collect();
                log(&format!(
                    "wayland-probe gpu-convert render_node={} {} xb24_modifiers={} offered={}",
                    importer.render_node().display(),
                    importer.describe(),
                    importer
                        .modifiers(arcen_media::video::CapturePixelFormat::Rgbx8)
                        .iter()
                        .map(|entry| format!(
                            "{:#018x}{}",
                            entry.modifier.0,
                            if entry.external_only {
                                "(ext-only)"
                            } else {
                                ""
                            }
                        ))
                        .collect::<Vec<_>>()
                        .join(","),
                    modifiers.len(),
                ));
                match SyncobjDevice::open(node) {
                    Ok(device) => syncobj = Some(std::sync::Arc::new(device)),
                    Err(error) => log(&format!("wayland-probe explicit sync unavailable: {error}")),
                }
                gpu_importer = Some(importer);
                Some(DmabufImporterFacts {
                    modifiers,
                    gpu: gpu_match,
                    host_verified: true,
                })
            }
            Err(error) => {
                log(&format!("wayland-probe gpu-convert unavailable: {error}"));
                None
            }
        });
    let decision = decide_import(dmabuf_facts.as_ref());
    log(&format!(
        "wayland-probe import decision preferred={} copy_path={} offer_dmabuf={} fallback_from={:?}",
        decision.preferred.token(),
        decision.preferred.copy_path().token(),
        decision.offer.offers_dmabuf(),
        decision.fallback_from,
    ));
    let gpu_frame = if decision.offer.offers_dmabuf() {
        match cuda::mem_alloc(width as usize * 4 * height as usize) {
            Ok(pointer) => Some(pointer),
            Err(error) => {
                log(&format!("ERROR: CUDA allocation: {error}"));
                return 4;
            }
        }
    } else {
        gpu_importer = None;
        None
    };

    let mut offer = FormatOffer::sdr8_host(options.max_fps);
    offer.memory = decision.offer.clone();
    offer.size = Some(handoff.size());
    let source = match PipeWireSource::start(
        handoff,
        SourceConfig {
            offer,
            mailbox_capacity: 2,
            source_gpu,
            encoder_gpu,
            dmabuf_copy_path: decision
                .offer
                .offers_dmabuf()
                .then_some(ImportRoute::DmabufGpuConvert.copy_path()),
            explicit_sync: if decision.offer.offers_dmabuf() {
                syncobj.clone()
            } else {
                None
            },
        },
    ) {
        Ok(source) => source,
        Err(error) => {
            log(&format!("ERROR: {error}"));
            return 3;
        }
    };
    let Some(negotiated) = source.wait_negotiated(options.timeout) else {
        log(&format!(
            "ERROR: no format negotiated within {:?} (phase {:?}, last refusal {:?})",
            options.timeout,
            source.phase(),
            source
                .shared()
                .stats
                .last_refusal
                .lock()
                .ok()
                .and_then(|last| *last)
        ));
        return 5;
    };
    log(&format!(
        "wayland-probe negotiated format={} size={}x{} modifier={:?} framerate={}/{} \
         max_framerate={}/{} transfer={} primaries={}",
        negotiated.format.token(),
        negotiated.width,
        negotiated.height,
        negotiated
            .modifier
            .map(|modifier| format!("{:#018x}", modifier.0)),
        negotiated.framerate.0,
        negotiated.framerate.1,
        negotiated.max_framerate.0,
        negotiated.max_framerate.1,
        negotiated.colour.transfer.strength_token(),
        negotiated.colour.primaries.strength_token(),
    ));
    if (negotiated.width, negotiated.height) != handoff.size() {
        // The node no longer records the output the session handed over
        // (resized or a later generation): never capture a guess.
        log(&format!(
            "ERROR: negotiated size {}x{} is not the hand-off's {}x{}",
            negotiated.width,
            negotiated.height,
            handoff.size().0,
            handoff.size().1,
        ));
        return 5;
    }

    let mut encoder = if options.encode_frames > 0 {
        match crate::nvenc_cuda::Encoder::new(
            context.as_raw(),
            negotiated.width,
            negotiated.height,
            &options.codec,
            crate::ColorSpec::legacy(false),
            EncodeIntent::Interactive,
            MotionPriority::Detail,
            crate::qp_map::QpMapPolicy::Off,
            options.max_fps,
            None,
            KeyframePolicy::ON_DEMAND_ONLY,
        ) {
            Ok(encoder) => Some(encoder),
            Err(error) => {
                log(&format!("ERROR: NVENC init failed: {error}"));
                return 4;
            }
        }
    } else {
        None
    };
    let mut out = match options
        .out
        .as_deref()
        .map(std::fs::File::create)
        .transpose()
    {
        Ok(file) => file,
        Err(error) => {
            log(&format!("ERROR: out: {error}"));
            return 2;
        }
    };
    let telemetry = CorrelationId::new("capenc-wayland-probe")
        .map_err(|error| error.to_string())
        .and_then(|sid| CaptureTelemetry::to_stderr(sid, OperationalProfile::Debug));
    if let Err(error) = &telemetry {
        log(&format!("wayland-probe telemetry unavailable: {error}"));
    }

    let mut pending =
        match PendingCaptureDamage::new(negotiated.width as usize, negotiated.height as usize) {
            Ok(pending) => pending,
            Err(error) => {
                log(&format!("ERROR: damage tracker: {error}"));
                return 5;
            }
        };
    let mut stream_truth = crate::StreamTruthLog::new(&options.codec);
    let mut verify = VerifyUpload::new(options.verify_upload);
    let mut summary = Summary::default();
    let started = Instant::now();
    let mut first_epoch: Option<EpochFacts> = None;
    let mut gpu_reference: Option<Vec<u8>> = None;
    let mut gpu_verify: Option<GpuVerify> = None;

    while summary.frames < options.frames {
        let lease = match source.next_frame(options.timeout) {
            Ok(Some(lease)) => lease,
            Ok(None) => {
                log(&format!(
                    "ERROR: no frame within {:?} after {} frames (phase {:?}, format {})",
                    options.timeout,
                    summary.frames,
                    source.phase(),
                    if source.current_format().is_some() {
                        "fixed"
                    } else {
                        "cleared"
                    }
                ));
                break;
            }
            Err(_) => {
                log(&format!(
                    "ERROR: stream closed (phase {:?})",
                    source.phase()
                ));
                break;
            }
        };
        let taken = Instant::now();
        let frame = lease.frame();
        let surface = frame.surface.surface();
        let Some(route) = ImportRoute::for_memory(surface.memory) else {
            log("ERROR: unexpected GPU-device memory from PipeWire");
            break;
        };
        if first_epoch.is_none() {
            let facts = EpochFacts {
                negotiated,
                memory: surface.memory,
                route,
                gpu: frame.surface.gpu_match(),
                cursor: surface.cursor,
                measured_fps: None,
            };
            emit_capture_path(telemetry.as_ref().ok(), &facts, decision.fallback_from);
            first_epoch = Some(facts);
        }
        if let Err(error) = pending.observe(&frame.damage) {
            log(&format!("ERROR: damage: {error}"));
            break;
        }

        // Import into the encoder input (or, with no encoder, a CUDA
        // round-trip check below). Errors leave the slot untouched, so the
        // frame is not encoded.
        let mut import_ms = None;
        let mut import_error = None;
        let mut gpu_report = None;
        let encode_this = summary.frames < options.encode_frames;
        if route == ImportRoute::DmabufGpuConvert {
            // Every DMA-BUF frame is imported (the read-back check needs it
            // even without an encoder), then staged when encoding.
            let result = import_dmabuf(
                gpu_importer.as_mut(),
                syncobj.as_deref(),
                &lease,
                gpu_frame.unwrap_or(0),
            )
            .and_then(|(elapsed, report)| {
                if let Some(encoder) = encoder.as_mut().filter(|_| encode_this) {
                    encoder.stage(gpu_frame.unwrap_or(0), width as usize * 4)?;
                }
                Ok((elapsed, report))
            });
            match result {
                Ok((elapsed, report)) => {
                    import_ms = Some(elapsed.as_secs_f64() * 1000.0);
                    gpu_report = Some(report);
                    summary.gpu_imports += 1;
                }
                Err(error) => {
                    import_error = Some(error);
                    summary.gpu_import_failures += 1;
                }
            }
        } else if let Some(encoder) = encoder.as_mut().filter(|_| encode_this) {
            match HostCopyImporter.import(&lease, encoder) {
                Ok(report) => import_ms = Some(report.elapsed.as_secs_f64() * 1000.0),
                Err(error) => import_error = Some(error.to_string()),
            }
        }
        if options.verify_gpu_import && gpu_verify.is_none() {
            match route {
                ImportRoute::DmabufGpuConvert if gpu_reference.is_none() => {
                    if let (None, Some(pointer)) = (&import_error, gpu_frame) {
                        let mut imported = vec![0u8; width as usize * 4 * height as usize];
                        match cuda::memcpy_dtoh(
                            imported.as_mut_ptr().cast(),
                            pointer,
                            imported.len(),
                        ) {
                            Ok(()) => {
                                gpu_reference = Some(imported);
                                // The reference: the same still desktop in
                                // shared memory, after renegotiation.
                                source.fall_back_to_host_memory();
                            }
                            Err(error) => log(&format!("ERROR: GPU import read-back: {error}")),
                        }
                    }
                }
                ImportRoute::HostCopyUpload => {
                    if let Some(imported) = gpu_reference.as_ref() {
                        let checked = host_reference(&lease).map(|reference| {
                            (
                                compare_bgr(imported, &reference),
                                discriminates_channel_order(&reference),
                            )
                        });
                        if let Some((comparison, discriminating)) = checked {
                            let result = match (comparison.verified(), discriminating) {
                                (true, true) => "ok",
                                (true, false) => "UNDISCRIMINATING",
                                (false, _) => "MISMATCH",
                            };
                            log(&format!(
                                "wayland-probe gpu-import-verify result={result} pixels={} \
                                 mismatched={} max_channel_diff={} discriminating={discriminating} \
                                 gpu_frames={} sync={}",
                                comparison.pixels,
                                comparison.mismatched,
                                comparison.max_channel_diff,
                                summary.gpu_imports,
                                summary.sync_token,
                            ));
                            gpu_verify = Some(GpuVerify {
                                comparison,
                                discriminating,
                            });
                        }
                    }
                }
                // `for_memory` never names the software route.
                ImportRoute::DmabufGpuConvert | ImportRoute::HostCopyConvert => {}
            }
        }
        if let Some(report) = gpu_report {
            summary
                .sync_ms
                .push(report.sync_wait.as_secs_f64() * 1000.0);
            summary.blit_ms.push(report.blit.as_secs_f64() * 1000.0);
            summary
                .cuda_ms
                .push(report.cuda_copy.as_secs_f64() * 1000.0);
            summary.sync_token =
                if lease.frame().token.explicit_sync().is_some() && syncobj.is_some() {
                    "explicit"
                } else {
                    "implicit"
                };
        }
        let verified = if route == ImportRoute::HostCopyUpload {
            verify.check(&lease)
        } else {
            None
        };
        if verified == Some(false) {
            summary.verify_failures += 1;
        }
        if import_error.is_none() {
            if let Some(ms) = import_ms {
                if route == ImportRoute::HostCopyUpload {
                    summary.upload_ms.push(ms);
                } else {
                    summary.gpu_import_ms.push(ms);
                }
            }
        }
        let wait_ms = taken.duration_since(frame.received).as_secs_f64() * 1000.0;
        let damage = match &frame.damage {
            DamageEvidence::Unknown => "unknown".to_string(),
            DamageEvidence::Full => "full".to_string(),
            DamageEvidence::Regions(rects) => format!("regions:{}", rects.len()),
        };
        let pending_blocks = if pending.is_pending() {
            pending.damage_map().dirty_blocks().count()
        } else {
            0
        };
        let superseded = frame.superseded;
        let sequence = surface.timestamps.sequence;
        let line_facts = format!(
            "seq={sequence} format={} size={}x{} stride={} offset={} size_bytes={} memory={} \
             modifier={} copy_path={} gpu_match={} damage={damage} pending_blocks={pending_blocks} cursor={} \
             superseded={superseded} mailbox_wait_ms={wait_ms:.2}",
            surface.format.token(),
            surface.geometry.coded_width,
            surface.geometry.coded_height,
            surface.planes[0].stride,
            surface.planes[0].offset,
            surface.planes[0].size,
            surface.memory.token(),
            match surface.memory {
                CaptureMemory::DmaBuf { modifier } => format!("{:#018x}", modifier.0),
                _ => "-".to_string(),
            },
            frame.surface.copy_path().token(),
            frame.surface.gpu_match().token(),
            surface.cursor.token(),
        );
        // The copy finished (synchronously) above: release the producer
        // buffer before encoding so PipeWire can reuse it.
        drop(lease);

        let mut encode_ms = None;
        let mut au_bytes = None;
        if let (Some(encoder), true, None) = (encoder.as_mut(), encode_this, import_error.as_ref())
        {
            let encode_started = Instant::now();
            match encoder.encode(summary.encoded == 0) {
                Ok(output) => {
                    encode_ms = Some(encode_started.elapsed().as_secs_f64() * 1000.0);
                    pending.mark_submitted();
                    summary.encoded += 1;
                    if let Some(access_unit) = output {
                        if let Err(error) = crate::validate_access_unit(&access_unit, false) {
                            log(&format!("ERROR: access unit invalid: {error}"));
                            summary.invalid_access_units += 1;
                        }
                        stream_truth.observe(&access_unit);
                        au_bytes = Some(access_unit.len());
                        summary.access_units += 1;
                        if let Some(file) = out.as_mut() {
                            let _ = file.write_all(&access_unit);
                        }
                    }
                }
                Err(error) => {
                    log(&format!("ERROR: encode: {error}"));
                    summary.encode_errors += 1;
                }
            }
        }
        if let Some(ms) = encode_ms {
            summary.encode_ms.push(ms);
        }
        let cursor = source.cursor();
        log(&format!(
            "wayland-probe frame {line_facts} upload_ms={} encode_ms={} au_bytes={} verify={} \
             import_error={} cursor_pos={},{} cursor_visible={} cursor_bitmap_serial={} \
             cursor_empty={} cursor_bitmap={} cursor_hotspot={},{} cursor_only_total={}",
            import_ms.map_or("-".to_string(), |ms| format!("{ms:.2}")),
            encode_ms.map_or("-".to_string(), |ms| format!("{ms:.2}")),
            au_bytes.map_or("-".to_string(), |bytes| bytes.to_string()),
            verified.map_or("-", |ok| if ok { "ok" } else { "MISMATCH" }),
            import_error.as_deref().unwrap_or("-"),
            cursor.position.0,
            cursor.position.1,
            cursor.visible,
            cursor.bitmap_serial,
            cursor.empty,
            cursor.bitmap.as_deref().map_or_else(
                || "-".to_owned(),
                |bitmap| format!(
                    "{}x{}/fmt{}",
                    bitmap.width, bitmap.height, bitmap.spa_format
                )
            ),
            cursor.hotspot.0,
            cursor.hotspot.1,
            cursor.cursor_only_updates,
        ));
        summary.frames += 1;
        if options.verify_gpu_import && gpu_verify.is_some() {
            break;
        }
    }

    let elapsed = started.elapsed().as_secs_f64();
    let stats = &source.shared().stats;
    let leases = source.lease_counters();
    let process_calls = stats.process_calls.load(Ordering::Relaxed).max(1);
    let fps = f64::from(summary.frames) / elapsed.max(f64::EPSILON);
    let cursor = source.cursor();
    log(&format!(
        "wayland-probe gpu-convert gpu_imports={} failures={} sync={} avg_import_ms={:.2} \
         max_import_ms={:.2} avg_sync_ms={:.2} avg_blit_ms={:.2} avg_cuda_copy_ms={:.2} \
         explicit_sync_frames={} release_signals={} release_signal_failures={} \
         release_fallbacks={}",
        summary.gpu_imports,
        summary.gpu_import_failures,
        summary.sync_token,
        mean(&summary.gpu_import_ms),
        max(&summary.gpu_import_ms),
        mean(&summary.sync_ms),
        mean(&summary.blit_ms),
        mean(&summary.cuda_ms),
        stats.explicit_sync.load(Ordering::Relaxed),
        stats.release_signals.load(Ordering::Relaxed),
        stats.release_signal_failures.load(Ordering::Relaxed),
        stats.release_fallbacks.load(Ordering::Relaxed),
    ));
    log(&format!(
        "wayland-probe summary frames={} elapsed_s={elapsed:.2} fps={fps:.1} buffers={} \
         leased={} cursor_only={} corrupted={} refused={} validation_refused={} \
         empty_process={} host_memory={} dmabuf={} superseded={} max_outstanding={} \
         outstanding={} requeued={} stale_releases={} format_changes={} revocations={} \
         callback_avg_us={:.1} callback_max_us={:.1} upload_avg_ms={:.2} upload_max_ms={:.2} \
         encode_avg_ms={:.2} encode_max_ms={:.2} encoded={} access_units={} invalid_aus={} \
         encode_errors={} verify_checked={} verify_failures={} cursor_metadata={} \
         cursor_bitmap_serial={} cursor_bitmap={} cursor_empty={} last_refusal={:?}",
        summary.frames,
        stats.buffers.load(Ordering::Relaxed),
        stats.frames.load(Ordering::Relaxed),
        stats.cursor_only.load(Ordering::Relaxed),
        stats.corrupted.load(Ordering::Relaxed),
        stats.refused.load(Ordering::Relaxed),
        stats.validation_refused.load(Ordering::Relaxed),
        stats.empty_process.load(Ordering::Relaxed),
        stats.host_memory.load(Ordering::Relaxed),
        stats.dmabuf.load(Ordering::Relaxed),
        leases.superseded.load(Ordering::Relaxed),
        leases.max_outstanding.load(Ordering::Relaxed),
        leases.outstanding.load(Ordering::Relaxed),
        stats.requeued.load(Ordering::Relaxed),
        stats.stale_releases.load(Ordering::Relaxed),
        stats.format_changes.load(Ordering::Relaxed),
        stats.revocations.load(Ordering::Relaxed),
        stats.callback_ns.load(Ordering::Relaxed) as f64 / process_calls as f64 / 1000.0,
        stats.callback_ns_max.load(Ordering::Relaxed) as f64 / 1000.0,
        mean(&summary.upload_ms),
        max(&summary.upload_ms),
        mean(&summary.encode_ms),
        max(&summary.encode_ms),
        summary.encoded,
        summary.access_units,
        summary.invalid_access_units,
        summary.encode_errors,
        verify.checked,
        summary.verify_failures,
        cursor.metadata_seen,
        cursor.bitmap_serial,
        cursor.bitmap.as_deref().map_or_else(
            || "-".to_owned(),
            |bitmap| format!(
                "{}x{}/fmt{}",
                bitmap.width, bitmap.height, bitmap.spa_format
            )
        ),
        cursor.empty,
        stats.last_refusal.lock().ok().and_then(|last| *last),
    ));
    if let Some(mut facts) = first_epoch {
        facts.measured_fps = Some(fps.floor() as u32);
        match capture_source_evidence(&facts) {
            Ok(evidence) => log(&format!(
                "wayland-probe admission evidence copy_path={} gpu={} sustained_fps={:?} cursor={}",
                evidence.copy_path.token(),
                evidence.gpu.token(),
                evidence.sustained_fps.value(),
                evidence.cursor.token()
            )),
            Err(refusal) => log(&format!(
                "wayland-probe admission evidence refused: {}",
                refusal.token()
            )),
        }
    }
    drop(encoder);
    drop(source);
    drop(gpu_importer);
    if let Some(pointer) = gpu_frame {
        let _ = cuda::mem_free(pointer);
    }
    if let Ok(telemetry) = &telemetry {
        telemetry.flush();
    }
    log("wayland-probe teardown complete");
    if options.verify_gpu_import && gpu_verify.is_none() {
        log("wayland-probe gpu-import-verify result=UNVERIFIED (no DMA-BUF frame, or no shared-memory reference after it)");
    }
    let failed = (summary.frames < options.frames && !options.verify_gpu_import)
        || (options.verify_gpu_import && !gpu_verify.is_some_and(GpuVerify::verified))
        || summary.verify_failures > 0
        || summary.invalid_access_units > 0
        || summary.encode_errors > 0
        || (options.encode_frames > 0 && summary.access_units == 0);
    drop(context);
    i32::from(failed) * 5
}

fn emit_capture_path(
    telemetry: Option<&CaptureTelemetry>,
    facts: &EpochFacts,
    fallback_from: Option<(ImportRoute, &'static str)>,
) {
    let Some(telemetry) = telemetry else {
        return;
    };
    match capture_path_event(telemetry.sid().clone(), facts, fallback_from) {
        Ok(event) => {
            if let Err(error) = telemetry.emit(&event, "wayland capture path selected") {
                log(&format!("wayland-probe telemetry emit failed: {error}"));
            }
        }
        Err(error) => log(&format!("wayland-probe telemetry event invalid: {error}")),
    }
}

unsafe fn import_dmabuf(
    importer: Option<&mut GpuConvertImporter>,
    syncobj: Option<&SyncobjDevice>,
    lease: &CaptureLease<super::pipewire::BufferToken>,
    destination: cuda::CUdeviceptr,
) -> Result<(Duration, GpuImportReport), String> {
    let importer = importer.ok_or_else(|| "no gpu-convert importer".to_string())?;
    let frame = lease.frame();
    let surface = frame.surface.surface();
    let points = frame.token.explicit_sync();
    let started = Instant::now();
    let pitch = surface.geometry.coded_width as usize * 4;
    let report = frame
        .token
        .read(|views| -> Result<GpuImportReport, String> {
            let fds: Vec<_> = views
                .iter()
                .filter_map(|view| match view {
                    PlaneView::DmaBuf(fd) => Some(*fd),
                    _ => None,
                })
                .collect();
            let acquire = match (points, syncobj) {
                (Some(points), Some(device)) => AcquireSync::Explicit {
                    device,
                    fd: points.acquire_fd,
                    point: points.acquire_point,
                },
                _ => AcquireSync::Implicit,
            };
            // `import` returns only after the GPU finished reading the
            // producer's buffer, so the lease may be dropped afterwards.
            importer
                .import(surface, &fds, acquire, destination, pitch)
                .map_err(|error| error.to_string())
        })
        .map_err(|_| "lease revoked".to_string())??;
    Ok((started.elapsed(), report))
}

/// The tightly packed host-copy reference of a shared-memory frame.
fn host_reference(lease: &CaptureLease<super::pipewire::BufferToken>) -> Option<Vec<u8>> {
    let frame = lease.frame();
    let surface = frame.surface.surface();
    let plane = surface.planes[0];
    let width = surface.geometry.coded_width as usize;
    let height = surface.geometry.coded_height as usize;
    frame
        .token
        .read(|views| {
            let PlaneView::Host(buffer) = views[usize::from(plane.buffer)] else {
                return None;
            };
            let rows = plane_bytes(buffer, plane.offset, plane.size)?;
            Some(super::reference::host_copy_reference(
                rows,
                plane.stride as usize,
                width,
                height,
            ))
        })
        .ok()
        .flatten()
}

/// Reads back the host-copy upload of a few frames and compares it with the
/// CPU's tightly packed rows: the GPU input must equal the shared CPU
/// reference byte for byte (padding removed, BGRx bytes unchanged).
struct VerifyUpload {
    remaining: u32,
    checked: u32,
}

impl VerifyUpload {
    const fn new(frames: u32) -> Self {
        Self {
            remaining: frames,
            checked: 0,
        }
    }

    unsafe fn check(&mut self, lease: &CaptureLease<super::pipewire::BufferToken>) -> Option<bool> {
        if self.remaining == 0 {
            return None;
        }
        self.remaining -= 1;
        self.checked += 1;
        let frame = lease.frame();
        let surface = frame.surface.surface();
        let plane = surface.planes[0];
        let width = surface.geometry.coded_width as usize;
        let height = surface.geometry.coded_height as usize;
        let row_bytes = width * 4;
        let result = frame.token.read(|views| -> Result<bool, String> {
            let PlaneView::Host(buffer) = views[usize::from(plane.buffer)] else {
                return Err("unmapped".to_string());
            };
            let rows = plane_bytes(buffer, plane.offset, plane.size).ok_or("plane outside")?;
            let device = cuda::mem_alloc(row_bytes * height)?;
            let copied = cuda::memcpy_2d_htod(
                device,
                row_bytes,
                rows.as_ptr().cast(),
                plane.stride as usize,
                row_bytes,
                height,
            );
            let mut readback = vec![0u8; row_bytes * height];
            let read = copied.and_then(|()| {
                cuda::memcpy_dtoh(readback.as_mut_ptr().cast(), device, readback.len())
            });
            let _ = cuda::mem_free(device);
            read?;
            let reference =
                super::reference::host_copy_reference(rows, plane.stride as usize, width, height);
            Ok(reference == readback)
        });
        match result {
            Ok(Ok(equal)) => Some(equal),
            Ok(Err(error)) => {
                log(&format!("wayland-probe verify-upload error: {error}"));
                Some(false)
            }
            Err(_) => Some(false),
        }
    }
}

#[derive(Default)]
struct Summary {
    gpu_imports: u32,
    gpu_import_failures: u32,
    gpu_import_ms: Vec<f64>,
    sync_ms: Vec<f64>,
    blit_ms: Vec<f64>,
    cuda_ms: Vec<f64>,
    sync_token: &'static str,
    frames: u32,
    encoded: u32,
    access_units: u32,
    invalid_access_units: u32,
    encode_errors: u32,
    verify_failures: u32,
    upload_ms: Vec<f64>,
    encode_ms: Vec<f64>,
}

fn mean(values: &[f64]) -> f64 {
    if values.is_empty() {
        0.0
    } else {
        values.iter().sum::<f64>() / values.len() as f64
    }
}

fn max(values: &[f64]) -> f64 {
    values.iter().copied().fold(0.0, f64::max)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_parse_and_refuse_unknown_arguments() {
        let options = parse_options([
            "frames=10",
            "codec=h265",
            "encode-frames=4",
            "max-fps=30",
            "timeout-ms=2000",
            "verify-upload=2",
            "offer-dmabuf",
            "out=/srv/probe/x.h265",
        ])
        .unwrap();
        assert_eq!(options.frames, 10);
        assert_eq!(options.codec, "h265");
        assert_eq!(options.encode_frames, 4);
        assert!(options.offer_dmabuf);
        assert!(!options.verify_gpu_import);
        assert_eq!(options.out, Some(PathBuf::from("/srv/probe/x.h265")));
        let verify = parse_options(["verify-gpu-import"]).unwrap();
        assert!(verify.verify_gpu_import && verify.offer_dmabuf);
        assert!(parse_options(["codec=av1"]).is_err());
        assert!(parse_options(["frames=x"]).is_err());
        assert!(parse_options(["out=relative"]).is_err());
        assert!(parse_options(["bogus"]).is_err());
    }
}
