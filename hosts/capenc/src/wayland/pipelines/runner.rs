//! The SDR8 epoch runner shared by the Wayland Auto and Speed compositions.
//!
//! One epoch, in order:
//!
//! 1. read the hand-off and the request, select the composition once
//!    ([`super::WaylandPipeline::select`]) and check cursor truth;
//! 2. open CUDA on the encoder GPU, open the `gpu-convert` importer when
//!    the host verified it (`gpu-import=verified`), choose the import route
//!    ([`decide_import`]) and start the `PipeWire` source with the
//!    composition's offer (DMA-BUF only on the verified same-GPU route);
//! 3. take the first real frame (Mutter sends one when a consumer connects,
//!    even to a still desktop), import it into a retained CUDA frame (a host
//!    copy from shared memory, or the GL blit and CUDA copy from a DMA-BUF)
//!    and release the producer buffer; a failed GPU import withdraws
//!    DMA-BUF and the first shared-memory frame is taken instead;
//! 4. build source evidence ([`capture_source_evidence`]) and run the
//!    composition's shared admission; open NVENC for the *served* contract;
//!    state the decision and the first frame on stderr (`ADMISSION`);
//! 5. run Keel's submission gate at the served cadence: each tick takes the
//!    newest leased frame (superseded frames were already returned with
//!    their damage carried), imports it, and encodes on change, on a
//!    keyframe request, once more to flush NVENC, and otherwise only at the
//!    idle keepalive; the retained frame is restaged into NVENC's current
//!    input slot on every submission, as the NvFBC path does, so a still
//!    desktop can always be re-encoded as a keyframe;
//! 6. `READY` after the first access unit, then framed access units on
//!    stdout and a stats line every second.
//!
//! A quiet stream is not a dead one: no frame for any length of time only
//! means nothing changed. The epoch ends when the source closes, when the
//! producer renegotiates a different size (resize is a new epoch), or when
//! the parent closes stdin.

use std::os::fd::BorrowedFd;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use arcen_keel::SubmissionMode;
use arcen_media::annexb::{annex_b_is_keyframe, NalCodec};
use arcen_media::video::{
    AdmissionReason, AdmissionRequest, BackendUnavailableReason, CaptureAdmissionNotice,
    CaptureBackend, CaptureLease, CapturePixelFormat, CaptureTakeError, ConversionBackend,
    CopyPath, CursorEvidence, DamageEvidence, EncoderBackend, EncoderCapability,
    FirstFrameEvidence, GpuIdentity, GpuMatch, KeyframePolicy, PendingCaptureDamage,
    PresetAdmission, SustainedRateMeter,
};
use arcen_media::VideoCodec;
use arcen_telemetry::{CorrelationId, OperationalProfile};

use super::{EpochRequest, WaylandPipeline};
use crate::linux::{cuda, NativeStartupError};
use crate::linux_policy::{RequestedEncoder, SubmissionGate};
use crate::log;
use crate::wayland::evidence::{
    capture_path_event, capture_source_evidence, decide_import, DmabufImporterFacts, EpochFacts,
    EvidenceRefusal, ImportDecision, ImportRoute,
};
use crate::wayland::explicit_sync::SyncobjDevice;
use crate::wayland::gpu_convert::{AcquireSync, GpuConvertImporter, GpuImportReport};
use crate::wayland::handoff::{self, WaylandHandoff};
use crate::wayland::host_copy::{HostBgraSink, HostCopyImporter, ImportError};
use crate::wayland::pipewire::{BufferToken, PipeWireSource, PlaneView, SourceConfig};
use crate::wayland::telemetry::CaptureTelemetry;

const NEGOTIATION_TIMEOUT: Duration = Duration::from_secs(5);
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(8);
const IDLE_KEEPALIVE: Duration = Duration::from_secs(1);
const MAILBOX_CAPACITY: usize = 2;

/// Entry point for a capenc started with a Wayland capture hand-off.
/// Returns the process exit code.
pub fn run(args: &[String]) -> i32 {
    let request = match EpochRequest::from_args(args) {
        Ok(request) => request,
        Err(error) => {
            log(&format!("ERROR: invalid Wayland capture request: {error}"));
            return 2;
        }
    };
    let backend = match request.encoder {
        RequestedEncoder::SoftwareH264 => EncoderBackend::OpenH264,
        RequestedEncoder::Auto | RequestedEncoder::Nvenc => EncoderBackend::NativeNvenc,
    };
    let handoff = match handoff::from_process_environment() {
        Ok(handoff) => handoff,
        Err(error) => {
            log(&format!("ERROR: Wayland capture hand-off: {error}"));
            let _ =
                crate::announce_unavailable(backend, BackendUnavailableReason::UnsupportedDisplay);
            return 2;
        }
    };
    let pipeline = match WaylandPipeline::select(&request) {
        Ok(pipeline) => pipeline,
        Err(refusal) => {
            log(&format!("ERROR: {refusal}"));
            let _ = crate::announce_unavailable(
                backend,
                BackendUnavailableReason::UnsupportedConfiguration,
            );
            return 2;
        }
    };
    // `hidden` leaves the pointer entirely to the client, like `metadata`
    // without the sprite.
    let cursor = handoff.cursor_mode().cursor_evidence();
    let authority = handoff.cursor_mode().cursor_authority();
    if authority != request.cursor {
        log(&format!(
            "ERROR: cursor={} was requested, but the session's stream uses cursor-mode {}, which \
             needs cursor authority {authority:?}",
            match request.cursor {
                arcen_protocol::messages::CursorMode::Local => "local",
                arcen_protocol::messages::CursorMode::Host => "host",
            },
            handoff.cursor_mode().token()
        ));
        return 2;
    }
    log(&format!(
        "Wayland pipeline selected path=linux-wayland-{} requested={} node={} output={} \
         generation={} size={}x{} cursor_mode={} encoder={}",
        pipeline.id().token(),
        pipeline.preset().pipeline().token(),
        handoff.node_id(),
        handoff.output().as_str(),
        handoff.generation().get(),
        handoff.size().0,
        handoff.size().1,
        handoff.cursor_mode().token(),
        backend.ready_token(),
    ));
    if backend == EncoderBackend::OpenH264 {
        // `linux-wayland-software`: no CUDA, no NVENC, shared memory only.
        #[cfg(feature = "software-h264")]
        {
            let admission_probe = match crate::admission_probe::options_from_args(args) {
                Ok(options) => options,
                Err(error) => {
                    log(&format!("ERROR: {error}"));
                    return 2;
                }
            };
            return super::software_runner::run(
                &request,
                &handoff,
                pipeline,
                cursor,
                admission_probe.as_ref(),
            );
        }
        #[cfg(not(feature = "software-h264"))]
        {
            log("ERROR: this capenc was built without the software-h264 encoder");
            let _ = crate::announce_unavailable(backend, BackendUnavailableReason::NotBuilt);
            return 2;
        }
    }
    match crate::admission_probe::options_from_args(args) {
        Ok(Some(options)) => {
            // SAFETY: CUDA is initialised and used only on this thread.
            return unsafe { run_admission_probe(&request, &handoff, &options) };
        }
        Ok(None) => {}
        Err(error) => {
            log(&format!("ERROR: {error}"));
            return 2;
        }
    }
    // SAFETY: CUDA is initialised and used only on this thread, and every
    // CUDA object the epoch creates is dropped before its context. Grading
    // and HDR run their own wide mechanism, never this eight-bit runner.
    let epoch = if pipeline.is_wide() && !request.probe {
        // A `RecordVirtual` stream offers eight-bit formats only, whatever
        // its monitor's colour state; the session records the same output
        // again with `RecordMonitor` for the wide compositions. A session
        // whose main stream is already a `RecordMonitor` stream has none.
        let wide = handoff.wide().unwrap_or_else(|| handoff.clone());
        if wide.node_id() != handoff.node_id() {
            log(&format!(
                "Wayland wide composition consumes the output's wide stream node={}",
                wide.node_id()
            ));
        }
        unsafe { super::wide::run_native(&request, &wide, pipeline, cursor) }
    } else {
        unsafe { run_native(&request, &handoff, pipeline, cursor) }
    };
    match epoch {
        Ok(code) => code,
        Err(NativeStartupError::Unavailable { reason, detail }) => {
            log(&format!("native NVENC unavailable: {detail}"));
            let _ = crate::announce_unavailable(EncoderBackend::NativeNvenc, reason);
            2
        }
        Err(NativeStartupError::Fatal(detail)) => {
            log(&format!(
                "ERROR: native NVENC startup failed closed: {detail}"
            ));
            5
        }
    }
}

/// The retained frame: the newest imported pixels, kept on the encoder GPU
/// so a keyframe or keepalive can be encoded while the producer sends
/// nothing.
struct RetainedFrame {
    pointer: cuda::CUdeviceptr,
    pitch: usize,
    height: usize,
}

impl RetainedFrame {
    /// # Safety
    ///
    /// The encoder's CUDA context must be current on this thread.
    unsafe fn new(width: u32, height: u32) -> Result<Self, String> {
        let pitch = (width as usize)
            .checked_mul(4)
            .ok_or("retained frame width overflow")?;
        let height = height as usize;
        let bytes = pitch
            .checked_mul(height)
            .ok_or("retained frame size overflow")?;
        let pointer = cuda::mem_alloc(bytes)?;
        Ok(Self {
            pointer,
            pitch,
            height,
        })
    }
}

impl HostBgraSink for RetainedFrame {
    fn stage_host_bgra(&mut self, rows: &[u8], stride: usize) -> Result<(), String> {
        if stride < self.pitch {
            return Err(format!(
                "source stride {stride} is shorter than a {}-byte row",
                self.pitch
            ));
        }
        let needed = stride
            .checked_mul(self.height.saturating_sub(1))
            .and_then(|bytes| bytes.checked_add(self.pitch))
            .ok_or("source frame size overflow")?;
        if rows.len() < needed {
            return Err(format!(
                "source frame holds {} bytes, needs {needed}",
                rows.len()
            ));
        }
        // SAFETY: the bounds above keep the copy inside `rows` and inside the
        // retained allocation; the runner only calls this on the thread that
        // made the context current. A copy from pageable host memory returns
        // once the source has been read, so the lease may be released after.
        unsafe {
            cuda::memcpy_2d_htod(
                self.pointer,
                self.pitch,
                rows.as_ptr().cast(),
                stride,
                self.pitch,
                self.height,
            )
        }
    }
}

impl Drop for RetainedFrame {
    fn drop(&mut self) {
        // SAFETY: this frame uniquely owns the allocation; the encoder that
        // reads it is dropped first (declared later in `run_native`).
        unsafe {
            let _ = cuda::mem_free(self.pointer);
        }
    }
}

/// The `gpu-convert` route of one epoch: the importer, the device that
/// waits for and signals explicit-sync points, and whether the route was
/// withdrawn.
struct GpuRoute {
    importer: Option<GpuConvertImporter>,
    sync: Option<Arc<SyncobjDevice>>,
    facts: Option<DmabufImporterFacts>,
    unavailable: Option<String>,
}

impl GpuRoute {
    /// Opens the importer on the encoder's render node when the host
    /// verified the route; any failure leaves the epoch on shared memory
    /// with the reason recorded.
    ///
    /// # Safety
    ///
    /// On the thread whose CUDA context is current; the route is dropped
    /// before that context.
    unsafe fn open(
        verified: bool,
        encoder_gpu: Option<GpuIdentity>,
        gpu: GpuMatch,
        width: u32,
        height: u32,
    ) -> Self {
        let mut route = Self {
            importer: None,
            sync: None,
            facts: None,
            unavailable: None,
        };
        if !verified {
            route.unavailable = Some("gpu_import_not_verified_on_host".to_owned());
            return route;
        }
        let Some(minor) = encoder_gpu.and_then(|identity| identity.render_node_minor) else {
            route.unavailable = Some("encoder_render_node_unknown".to_owned());
            return route;
        };
        let node = std::path::PathBuf::from(format!("/dev/dri/renderD{minor}"));
        match GpuConvertImporter::open(&node, width, height) {
            Ok(importer) => {
                let modifiers = offerable_modifiers(&importer);
                log(&format!(
                    "gpu-convert importer ready on {} {} modifiers={}",
                    node.display(),
                    importer.describe(),
                    modifiers
                        .iter()
                        .map(|modifier| format!("{:#018x}", modifier.0))
                        .collect::<Vec<_>>()
                        .join(",")
                ));
                route.facts = Some(DmabufImporterFacts {
                    modifiers,
                    gpu,
                    host_verified: true,
                });
                route.importer = Some(importer);
                // Without a syncobj device the producer is not asked for
                // explicit sync and imports wait on implicit fences.
                match SyncobjDevice::open(&node) {
                    Ok(device) => route.sync = Some(Arc::new(device)),
                    Err(error) => log(&format!(
                        "explicit sync unavailable ({error}); DMA-BUF imports use implicit fences"
                    )),
                }
            }
            Err(error) => {
                route.unavailable = Some(format!("{}: {error}", error.token()));
            }
        }
        route
    }

    /// Withdraws the route after a failed import.
    fn withdraw(&mut self, reason: String) {
        self.importer = None;
        self.unavailable = Some(reason);
    }
}

/// Modifiers offered for the eight-bit SDR family: importable for both
/// BGRx and BGRA, which the offer lists under one modifier set.
fn offerable_modifiers(importer: &GpuConvertImporter) -> Vec<arcen_media::video::FormatModifier> {
    let bgra = importer.offerable_modifiers(CapturePixelFormat::Bgra8);
    importer
        .offerable_modifiers(CapturePixelFormat::Bgrx8)
        .into_iter()
        .filter(|modifier| bgra.contains(modifier))
        .collect()
}

/// What importing one leased frame did.
enum FrameImport {
    /// The retained frame now holds this frame's pixels.
    Imported {
        route: ImportRoute,
        elapsed: Duration,
        gpu: Option<GpuImportReport>,
        explicit_sync: bool,
    },
    /// The lease was revoked (renegotiation); nothing was read.
    Revoked,
    /// The import failed; the retained frame must not be encoded until the
    /// next successful import.
    Failed(ImportRoute, String),
}

/// Imports `lease` into `retained` by the route its memory takes.
///
/// # Safety
///
/// On the thread whose CUDA (and the importer's GL) context is current.
unsafe fn import_frame(
    lease: &CaptureLease<BufferToken>,
    retained: &mut RetainedFrame,
    gpu: &mut GpuRoute,
) -> FrameImport {
    let frame = lease.frame();
    let surface = frame.surface.surface();
    match ImportRoute::for_memory(surface.memory) {
        Some(ImportRoute::HostCopyUpload) => match HostCopyImporter.import(lease, retained) {
            Ok(report) => FrameImport::Imported {
                route: ImportRoute::HostCopyUpload,
                elapsed: report.elapsed,
                gpu: None,
                explicit_sync: false,
            },
            Err(ImportError::Revoked) => FrameImport::Revoked,
            Err(error) => FrameImport::Failed(ImportRoute::HostCopyUpload, error.to_string()),
        },
        Some(ImportRoute::DmabufGpuConvert) => {
            let route = ImportRoute::DmabufGpuConvert;
            let Some(importer) = gpu.importer.as_mut() else {
                return FrameImport::Failed(route, "the gpu-convert route was withdrawn".into());
            };
            let started = Instant::now();
            let points = frame.token.explicit_sync();
            let result = frame.token.read(|views| {
                let fds: Vec<BorrowedFd<'_>> = views
                    .iter()
                    .filter_map(|view| match view {
                        PlaneView::DmaBuf(fd) => Some(*fd),
                        _ => None,
                    })
                    .collect();
                let acquire = match (points, gpu.sync.as_deref()) {
                    (Some(points), Some(device)) => AcquireSync::Explicit {
                        device,
                        fd: points.acquire_fd,
                        point: points.acquire_point,
                    },
                    _ => AcquireSync::Implicit,
                };
                importer.import(surface, &fds, acquire, retained.pointer, retained.pitch)
            });
            match result {
                Ok(Ok(report)) => FrameImport::Imported {
                    route,
                    elapsed: started.elapsed(),
                    gpu: Some(report),
                    explicit_sync: points.is_some() && gpu.sync.is_some(),
                },
                Ok(Err(error)) => FrameImport::Failed(route, format!("{}: {error}", error.token())),
                Err(_) => FrameImport::Revoked,
            }
        }
        // `for_memory` names only the CUDA routes; the software route has its
        // own runner.
        Some(ImportRoute::HostCopyConvert) | None => FrameImport::Failed(
            ImportRoute::HostCopyUpload,
            format!("no route imports {} memory", surface.memory.token()),
        ),
    }
}

fn telemetry() -> Option<CaptureTelemetry> {
    // Debug-level events, so only when the parent asked for diagnostics.
    std::env::var_os("ARCEN_CAPENC_DEBUG")?;
    let sid = crate::session_log_id()
        .cloned()
        .or_else(|| CorrelationId::new("capenc-wayland").ok())?;
    CaptureTelemetry::to_stderr(sid, OperationalProfile::Debug).ok()
}

const fn refusal_reason(refusal: EvidenceRefusal) -> AdmissionReason {
    match refusal {
        // A same-GPU route without a proven same GPU lacks the GPU proof
        // admission needs.
        EvidenceRefusal::GpuNotSame => AdmissionReason::GpuIdentityUnproven,
    }
}

#[derive(Default)]
struct Stats {
    emitted: u64,
    submitted: u64,
    bytes: u64,
    keyframes: u64,
    imports: u64,
    import_ms_sum: f64,
    import_ms_max: f64,
    gpu_imports: u64,
    sync_ms_sum: f64,
    blit_ms_sum: f64,
    cuda_ms_sum: f64,
    import_failures: u64,
    stage_ms_sum: f64,
    encode_ms_sum: f64,
    encode_ms_max: f64,
    dirty_blocks: u64,
    damage_regions: u64,
    by_mode: [u64; 5],
}

impl Stats {
    fn note_import(&mut self, elapsed: Duration, gpu: Option<GpuImportReport>) {
        let ms = elapsed.as_secs_f64() * 1000.0;
        self.imports += 1;
        self.import_ms_sum += ms;
        self.import_ms_max = self.import_ms_max.max(ms);
        if let Some(report) = gpu {
            self.gpu_imports += 1;
            self.sync_ms_sum += report.sync_wait.as_secs_f64() * 1000.0;
            self.blit_ms_sum += report.blit.as_secs_f64() * 1000.0;
            self.cuda_ms_sum += report.cuda_copy.as_secs_f64() * 1000.0;
        }
    }

    const fn mode_index(mode: SubmissionMode) -> usize {
        match mode {
            SubmissionMode::FirstFrame => 0,
            SubmissionMode::Idr => 1,
            SubmissionMode::Activity => 2,
            SubmissionMode::Keepalive => 3,
            SubmissionMode::PipelineFlush => 4,
        }
    }
}

fn average(sum: f64, count: u64) -> f64 {
    if count == 0 {
        0.0
    } else {
        sum / count as f64
    }
}

/// Aggregate encoder admission for one Wayland output (`admission-v1`):
/// NVENC opened on the hand-off's encoder GPU at exactly the output's size
/// and measured on synthetic frames. The session's stream is not consumed:
/// a probe that negotiated with a `RecordVirtual` stream would resize or
/// re-pace a live output.
unsafe fn run_admission_probe(
    request: &EpochRequest,
    handoff: &WaylandHandoff,
    options: &crate::admission_probe::AdmissionProbeOptions,
) -> i32 {
    if handoff.size() != (options.width, options.height) {
        log(&format!(
            "ERROR: admission probe geometry {}x{} differs from the output's {}x{}",
            options.width,
            options.height,
            handoff.size().0,
            handoff.size().1
        ));
        return 2;
    }
    let context = match cuda::init_from_env() {
        Ok(context) => context,
        Err(NativeStartupError::Unavailable { reason, detail }) => {
            log(&format!("native NVENC unavailable: {detail}"));
            let _ = crate::announce_unavailable(EncoderBackend::NativeNvenc, reason);
            return 2;
        }
        Err(NativeStartupError::Fatal(detail)) => {
            log(&format!(
                "ERROR: admission probe CUDA startup failed: {detail}"
            ));
            return 6;
        }
    };
    let code = crate::linux::run_synthetic_admission_probe(
        &context,
        request.codec.token(),
        crate::ColorSpec::legacy(false),
        request.intent,
        request.priority,
        crate::qp_map::QpMapPolicy::Off,
        request.fps,
        false,
        options,
    );
    drop(context);
    code
}

#[allow(clippy::too_many_lines)] // One linear epoch.
unsafe fn run_native(
    request: &EpochRequest,
    handoff: &WaylandHandoff,
    pipeline: WaylandPipeline,
    cursor: CursorEvidence,
) -> Result<i32, NativeStartupError> {
    let context = cuda::init_from_env()?;
    let (width, height) = handoff.size();
    if request.probe {
        let encoder = crate::nvenc_cuda::Encoder::new(
            context.as_raw(),
            width,
            height,
            request.codec.token(),
            crate::ColorSpec::legacy(false),
            request.intent,
            request.priority,
            crate::qp_map::QpMapPolicy::Off,
            request.fps,
            None,
            KeyframePolicy::ON_DEMAND_ONLY,
        )?;
        drop(encoder);
        drop(context);
        log("native probe capture backend=pipewire");
        log("PROBE version=1 backend=native-nvenc available=true");
        return Ok(0);
    }

    let (source_gpu, encoder_gpu) =
        crate::wayland::probe::identities(handoff, context.device_pci());
    let gpu = arcen_media::video::compare_gpus(source_gpu.as_ref(), encoder_gpu.as_ref());
    let mut gpu_route =
        GpuRoute::open(request.gpu_import_verified, encoder_gpu, gpu, width, height);
    let decision: ImportDecision = decide_import(gpu_route.facts.as_ref());
    let dmabuf_offered = decision.offer.offers_dmabuf();
    let mut fallback_from = decision.fallback_from.map(|(route, reason)| {
        (
            route,
            gpu_route
                .unavailable
                .clone()
                .unwrap_or_else(|| reason.to_owned()),
        )
    });
    if !dmabuf_offered {
        // The importer is useless without the offer; release it now.
        gpu_route.importer = None;
    }
    let mut offer = pipeline.offer(request.fps);
    offer.memory = decision.offer.clone();
    // The hand-off's size, exactly: a `RecordVirtual` monitor takes the size
    // its consumer negotiates, so offering any other would resize the
    // session's output; a `RecordMonitor` stream offers only its own size.
    offer.size = Some(handoff.size());
    log(&format!(
        "Wayland capture offer formats=bgrx8,bgra8 max_fps={} memory={} explicit_sync={}{}",
        offer.max_fps,
        if dmabuf_offered { "dmabuf+shm" } else { "shm" },
        dmabuf_offered && gpu_route.sync.is_some(),
        fallback_from
            .as_ref()
            .map_or_else(String::new, |(route, reason)| format!(
                " not_offered={} reason={reason}",
                route.token()
            )),
    ));
    let source = match PipeWireSource::start(
        handoff,
        SourceConfig {
            offer,
            mailbox_capacity: MAILBOX_CAPACITY,
            source_gpu,
            encoder_gpu,
            dmabuf_copy_path: dmabuf_offered.then_some(CopyPath::GpuConvert),
            explicit_sync: if dmabuf_offered {
                gpu_route.sync.clone()
            } else {
                None
            },
        },
    ) {
        Ok(source) => source,
        Err(error) => {
            log(&format!("ERROR: Wayland capture: {error}"));
            return Ok(3);
        }
    };
    let Some(negotiated) = source.wait_negotiated(NEGOTIATION_TIMEOUT) else {
        log(&format!(
            "ERROR: Wayland capture: no format negotiated within {NEGOTIATION_TIMEOUT:?} (phase {:?})",
            source.phase()
        ));
        return Ok(3);
    };
    if (negotiated.width, negotiated.height) != (width, height) {
        log(&format!(
            "ERROR: Wayland capture: negotiated {}x{} is not the hand-off's {width}x{height}",
            negotiated.width, negotiated.height
        ));
        return Ok(3);
    }
    let mut retained = RetainedFrame::new(width, height).map_err(NativeStartupError::fatal)?;
    let mut pending = PendingCaptureDamage::new(width as usize, height as usize)
        .map_err(|error| NativeStartupError::fatal(format!("damage tracker: {error}")))?;

    // The first frame that imports. A failed GPU import withdraws DMA-BUF
    // and the producer's first shared-memory frame is taken instead.
    let first_deadline = Instant::now() + FIRST_FRAME_TIMEOUT;
    let (first_import, first_route, first_frame, first_gpu_match, first_explicit) = loop {
        let wait = first_deadline.saturating_duration_since(Instant::now());
        let lease = match source.next_frame(wait) {
            Ok(Some(lease)) => lease,
            Ok(None) => {
                log(&format!(
                    "ERROR: Wayland capture: no first frame within {FIRST_FRAME_TIMEOUT:?} (phase {:?})",
                    source.phase()
                ));
                return Ok(3);
            }
            Err(CaptureTakeError::Closed) => {
                log("ERROR: Wayland capture: the stream closed before its first frame");
                return Ok(3);
            }
        };
        if let Err(error) = pending.observe(&lease.frame().damage) {
            return Err(NativeStartupError::fatal(format!("damage: {error}")));
        }
        match import_frame(&lease, &mut retained, &mut gpu_route) {
            FrameImport::Imported {
                route,
                elapsed,
                gpu: _,
                explicit_sync,
            } => {
                let surface = lease.frame().surface.surface();
                let evidence = FirstFrameEvidence {
                    node_id: handoff.node_id(),
                    format: surface.format,
                    width: surface.geometry.coded_width,
                    height: surface.geometry.coded_height,
                    memory: surface.memory,
                };
                break (
                    elapsed,
                    route,
                    evidence,
                    lease.frame().surface.gpu_match(),
                    explicit_sync,
                );
            }
            FrameImport::Revoked => {}
            FrameImport::Failed(route, error) => {
                if route != ImportRoute::DmabufGpuConvert {
                    log(&format!(
                        "ERROR: Wayland capture: first frame import failed: {error}"
                    ));
                    return Ok(3);
                }
                log(&format!(
                    "gpu import fallback: first DMA-BUF frame failed ({error}); withdrawing                      DMA-BUF, the producer renegotiates to shared memory"
                ));
                gpu_route.withdraw(error.clone());
                fallback_from = Some((route, error));
                source.fall_back_to_host_memory();
            }
        }
    };
    let memory = first_frame.memory;
    let route = first_route;
    let facts = EpochFacts {
        negotiated: source.current_format().unwrap_or(negotiated),
        memory,
        route,
        gpu: first_gpu_match,
        cursor,
        measured_fps: pipeline.rate_evidence(),
    };
    let mut retained_valid = true;
    let mut current_route = route;

    let telemetry = telemetry();
    if let Some(telemetry) = telemetry.as_ref() {
        let fallback = fallback_from
            .as_ref()
            .map(|(route, _)| (*route, "gpu_import_unavailable"));
        if let Ok(event) = capture_path_event(telemetry.sid().clone(), &facts, fallback) {
            let _ = telemetry.emit(&event, "wayland capture path selected");
        }
    }
    let admission_request = AdmissionRequest {
        preset: pipeline.preset(),
        requested_codec: request.codec,
        width,
        height,
        max_fps: request.fps,
    };
    let capability = EncoderCapability {
        backend: EncoderBackend::NativeNvenc,
        limits: EncoderBackend::NativeNvenc.contract(),
    };
    let admission = match capture_source_evidence(&facts) {
        Ok(evidence) => pipeline.admit(admission_request, &evidence, &capability),
        Err(refusal) => PresetAdmission::Unavailable {
            requested: pipeline.preset(),
            reason: refusal_reason(refusal),
            fallback: None,
        },
    };
    let notice = CaptureAdmissionNotice {
        requested: pipeline.preset(),
        requested_fps: request.fps,
        admission,
        capture: CaptureBackend::PipeWire,
        copy_path: route.copy_path(),
        cursor,
        measured_fps: pipeline.rate_evidence(),
        rate_evidence: pipeline.rate_source(),
        frame: first_frame,
    };
    let Some(served) = admission.served().copied() else {
        let _ = crate::announce_admission(&notice);
        log(&format!(
            "ERROR: the Wayland source cannot serve {}: {}",
            pipeline.preset().pipeline().token(),
            match admission {
                PresetAdmission::Unavailable { reason, .. } => reason.to_string(),
                _ => String::new(),
            }
        ));
        let _ = crate::announce_unavailable(
            EncoderBackend::NativeNvenc,
            BackendUnavailableReason::UnsupportedConfiguration,
        );
        return Ok(2);
    };
    let plan = served
        .epoch()
        .ok_or_else(|| NativeStartupError::fatal("the admitted plan states no cursor truth"))?;
    let codec = plan.video.codec;
    // Keyframe statistics only; AV1 is not Annex-B and is not counted.
    let nal_codec = match codec {
        VideoCodec::H264 => Some(NalCodec::H264),
        VideoCodec::H265 => Some(NalCodec::H265),
        _ => None,
    };
    // NVENC is opened for the served contract before the decision is
    // announced: if it cannot open, the parent sees only UNAVAILABLE and
    // tries its own codec ladder.
    let mut encoder = crate::nvenc_cuda::Encoder::new(
        context.as_raw(),
        width,
        height,
        codec.token(),
        crate::ColorSpec::legacy(false),
        plan.intent,
        plan.priority,
        request.qp_map,
        plan.fps,
        request.max_bitrate_bps,
        plan.keyframe,
    )?;
    // A QP map here takes the compositor's own damage (`SPA_META_VideoDamage`
    // regions), never a readback: a picture whose frames all carried region
    // lists is biased by them, any other submission is neutral.
    let qp_map_engaged = request.qp_map.submits_map()
        && encoder.enable_external_qp_map(
            request.qp_map,
            arcen_media::video::QpBias::default(),
            codec,
        );
    if request.qp_map.submits_map() {
        log(&format!(
            "Wayland QP map policy={} damage_source=compositor-regions engaged={qp_map_engaged}",
            request.qp_map.token()
        ));
    }
    if let Err(error) = crate::announce_admission(&notice) {
        log(&format!("ERROR: emit ADMISSION: {error}"));
        return Ok(5);
    }
    log(&format!(
        "Wayland epoch serving {} for {} at up to {} fps: priority={} intent={} vbv_frames={} \
         codec={} copy_path={} route={} memory={} sync={} colour={} cursor={} \
         first_import_ms={:.2}{}",
        plan.pipeline.token(),
        pipeline.preset().pipeline().token(),
        plan.fps,
        plan.priority.token(),
        plan.intent.token(),
        arcen_media::video::encoder_buffer_frames(plan.priority, plan.intent),
        codec.token(),
        served.copy_path.token(),
        route.token(),
        memory.token(),
        match (route, first_explicit) {
            (ImportRoute::HostCopyUpload | ImportRoute::HostCopyConvert, _) => "none",
            (ImportRoute::DmabufGpuConvert, true) => "explicit",
            (ImportRoute::DmabufGpuConvert, false) => "implicit",
        },
        if served.colour_assumed {
            "bt709-limited-assumed"
        } else {
            "bt709-limited-reported"
        },
        cursor.token(),
        first_import.as_secs_f64() * 1000.0,
        match admission {
            PresetAdmission::Downgrade { reason, .. } => format!(
                " downgraded_from={} reason={reason}",
                pipeline.preset().pipeline().token()
            ),
            _ => served
                .fps_limited_by
                .map_or_else(String::new, |reason| format!(" fps_limited_by={reason}")),
        },
    ));

    let control = crate::spawn_control_thread("NVENC-Wayland");
    let mut stdout = std::io::stdout();
    let mut target_dt = crate::frame_interval_from_fps(plan.fps);
    let mut next = Instant::now();
    let mut gate = SubmissionGate::new(IDLE_KEEPALIVE);
    gate.note_frame();
    let mut last_emit = Instant::now();
    let mut ready_announced = false;
    let mut stream_truth = crate::StreamTruthLog::new(codec.token());
    let mut stats = Stats::default();
    let mut totals = Stats::default();
    let mut second = Instant::now();
    let mut pacing = crate::linux_policy::PacingWindow::default();
    let mut target_fps = plan.fps;
    let mut rate = SustainedRateMeter::new(Instant::now());
    let source_stats = &source.shared().stats;
    let leases = source.lease_counters();
    let mut source_frames_seen = source_stats.frames.load(Ordering::Relaxed);
    let mut superseded_seen = leases.superseded.load(Ordering::Relaxed);
    let cursor_capture = match plan.cursor_mode {
        arcen_protocol::messages::CursorMode::Local => crate::CursorCaptureMode::Local,
        arcen_protocol::messages::CursorMode::Host => crate::CursorCaptureMode::Host,
    };

    // Cursor events only after READY.
    let mut epoch_output = super::output::EpochOutput::new(request, cursor);

    let exit = 'epoch: loop {
        if control.stop_requested() {
            break 0;
        }
        if ready_announced && epoch_output.cursor(&mut stdout, &source.cursor()).is_err() {
            break 'epoch 0;
        }
        let now = Instant::now();
        if now < next {
            std::thread::sleep((next - now).min(Duration::from_millis(50)));
            continue;
        }
        // The newest frame only; anything older was already returned to the
        // producer with its damage folded into this one.
        match source.next_frame(Duration::ZERO) {
            Ok(Some(lease)) => {
                // Missing damage is full damage (GNOME 40 sends none).
                if matches!(lease.frame().damage, DamageEvidence::Regions(_)) {
                    stats.damage_regions += 1;
                }
                if let Err(error) = pending.observe(&lease.frame().damage) {
                    log(&format!("ERROR: damage: {error}"));
                    break 5;
                }
                match import_frame(&lease, &mut retained, &mut gpu_route) {
                    FrameImport::Imported {
                        route,
                        elapsed,
                        gpu,
                        explicit_sync: _,
                    } => {
                        stats.note_import(elapsed, gpu);
                        current_route = route;
                        retained_valid = true;
                        gate.note_frame();
                    }
                    // A revoked lease (renegotiation) leaves the retained
                    // frame as it was; nothing new is encoded from it.
                    FrameImport::Revoked => {}
                    FrameImport::Failed(ImportRoute::DmabufGpuConvert, error) => {
                        // Nothing from this frame is encoded, and the
                        // retained frame is not re-encoded until a frame
                        // imports again: its pixels may be partial.
                        stats.import_failures += 1;
                        retained_valid = false;
                        log(&format!(
                            "gpu import fallback: {error}; withdrawing DMA-BUF, the producer \
                             renegotiates to shared memory"
                        ));
                        gpu_route.withdraw(error);
                        source.fall_back_to_host_memory();
                    }
                    FrameImport::Failed(
                        ImportRoute::HostCopyUpload | ImportRoute::HostCopyConvert,
                        error,
                    ) => {
                        log(&format!("ERROR: Wayland import: {error}"));
                        break 5;
                    }
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
        let decision = if retained_valid {
            gate.decision(idr_pending, last_emit.elapsed())
        } else {
            None
        };
        pacing.record_due(decision);
        if let Some(mode) = decision {
            let stage_started = Instant::now();
            if let Err(error) = encoder.stage(retained.pointer, retained.pitch) {
                log(&format!("ERROR: stage failed: {error}"));
                break 5;
            }
            stats.stage_ms_sum += stage_started.elapsed().as_secs_f64() * 1000.0;
            let requested_idr = idr_pending && control.take_idr();
            let force = mode == SubmissionMode::FirstFrame || requested_idr;
            if requested_idr && mode != SubmissionMode::FirstFrame {
                log("consuming IDR request");
            }
            if let Some(fps) = control.take_framerate_fps() {
                let fps = plan.live_fps(fps);
                target_dt = crate::frame_interval_from_fps(fps);
                target_fps = fps;
                if let Err(error) = encoder.reconfigure_framerate(fps) {
                    log(&format!(
                        "ERROR: live framerate reconfigure failed: {error}"
                    ));
                }
            }
            if let Some(bps) = control.take_bitrate_bps() {
                if let Err(error) = encoder.reconfigure_bitrate(bps) {
                    log(&format!("ERROR: live bitrate reconfigure failed: {error}"));
                }
            }
            if pending.is_pending() {
                stats.dirty_blocks += pending.damage_map().dirty_blocks().count() as u64;
            }
            if qp_map_engaged {
                if pending.is_authoritative() {
                    if let Err(error) = encoder.observe_external_damage(pending.damage_map()) {
                        log(&format!("QP map: {error}; this picture is neutral"));
                    }
                } else {
                    encoder.clear_qp_observation();
                }
            }
            let encode_started = Instant::now();
            match encoder.encode(force) {
                Ok(output) => {
                    let ms = encode_started.elapsed().as_secs_f64() * 1000.0;
                    stats.submitted += 1;
                    stats.encode_ms_sum += ms;
                    stats.encode_ms_max = stats.encode_ms_max.max(ms);
                    stats.by_mode[Stats::mode_index(mode)] += 1;
                    pending.mark_submitted();
                    gate.on_submitted(mode, output.is_some());
                    last_emit = now;
                    if let Some(access_unit) = output {
                        if !ready_announced {
                            if let Err(error) =
                                crate::validate_access_unit(&access_unit, epoch_output.framed())
                            {
                                log(&format!(
                                    "ERROR: first access unit failed output validation: {error}"
                                ));
                                break 5;
                            }
                            let ready = match crate::resolved_media_plan(
                                EncoderBackend::NativeNvenc,
                                codec.token(),
                                crate::ColorSpec::legacy(false),
                                width,
                                height,
                                plan.fps,
                                cursor_capture,
                            ) {
                                Ok(ready) => ready,
                                Err(error) => {
                                    log(&format!("ERROR: {error}"));
                                    break 5;
                                }
                            };
                            // NVENC converts BGRA to YCbCr on the GPU; the
                            // host copy is reported by the ADMISSION line.
                            if let Err(error) = crate::announce_ready_from_with_conversion(
                                ready,
                                Some(CaptureBackend::PipeWire),
                                Some(ConversionBackend::Gpu),
                            ) {
                                log(&format!("ERROR: emit READY: {error}"));
                                break 5;
                            }
                            ready_announced = true;
                        }
                        if nal_codec.is_some_and(|nal| annex_b_is_keyframe(&access_unit, nal)) {
                            stats.keyframes += 1;
                        }
                        stream_truth.observe(&access_unit);
                        if epoch_output.access_unit(&mut stdout, &access_unit).is_err() {
                            break 'epoch 0;
                        }
                        stats.emitted += 1;
                        stats.bytes += access_unit.len() as u64;
                    }
                }
                Err(error) => {
                    log(&format!("ERROR: encode error: {error}"));
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
            let superseded = leases.superseded.load(Ordering::Relaxed);
            log(&format!(
                "enc_fps={} encode_submitted={} source_fps={} source_superseded={} \
                 avg_upload_ms={:.2} max_upload_ms={:.2} avg_stage_ms={:.2} avg_encode_ms={:.2} \
                 max_encode_ms={:.2} kbps={} keyframes={} capture_backend=pipewire \
                 copy_path={} route={} gpu_imports={} avg_sync_ms={:.2} avg_blit_ms={:.2} \
                 avg_cuda_copy_ms={:.2} import_failures={} explicit_sync_frames={} \
                 release_signal_failures={} release_fallbacks={} served={} fps_ceiling={} \
                 emit_first={} emit_idr={} \
                 emit_activity={} emit_keepalive={} pipeline_flush={} want_idr={} \
                 damage_source={} dirty_blocks={} cursor={}{}{}",
                stats.emitted,
                stats.submitted,
                rate.last_window().unwrap_or(0),
                superseded - superseded_seen,
                average(stats.import_ms_sum, stats.imports),
                stats.import_ms_max,
                average(stats.stage_ms_sum, stats.submitted),
                average(stats.encode_ms_sum, stats.submitted),
                stats.encode_ms_max,
                stats.bytes * 8 / 1000,
                stats.keyframes,
                current_route.copy_path().token(),
                current_route.token(),
                stats.gpu_imports,
                average(stats.sync_ms_sum, stats.gpu_imports),
                average(stats.blit_ms_sum, stats.gpu_imports),
                average(stats.cuda_ms_sum, stats.gpu_imports),
                stats.import_failures,
                source_stats.explicit_sync.load(Ordering::Relaxed),
                source_stats.release_signal_failures.load(Ordering::Relaxed),
                source_stats.release_fallbacks.load(Ordering::Relaxed),
                plan.pipeline.token(),
                plan.fps,
                stats.by_mode[0],
                stats.by_mode[1],
                stats.by_mode[2],
                stats.by_mode[3],
                stats.by_mode[4],
                control.idr_pending(),
                if stats.damage_regions > 0 {
                    "metadata"
                } else {
                    "unknown_full"
                },
                stats.dirty_blocks,
                cursor.token(),
                if qp_map_engaged {
                    let qp = encoder.take_qp_map_stats();
                    format!(
                        " qp_stats.biased_maps={} qp_stats.neutral_maps={} \
                         qp_stats.mean_dirty_fraction={:.4}",
                        qp.biased_maps,
                        qp.neutral_maps,
                        qp.mean_dirty_fraction()
                    )
                } else {
                    String::new()
                },
                pacing.verdict_fields(
                    plan.fps,
                    target_fps,
                    Duration::from_secs_f64(
                        (stats.import_ms_sum + stats.stage_ms_sum + stats.encode_ms_sum).max(0.0)
                            / 1000.0,
                    ),
                    now.duration_since(second),
                ),
            ));
            pacing = crate::linux_policy::PacingWindow::default();
            superseded_seen = superseded;
            totals.emitted += stats.emitted;
            totals.submitted += stats.submitted;
            totals.keyframes += stats.keyframes;
            totals.bytes += stats.bytes;
            for (total, mode) in totals.by_mode.iter_mut().zip(stats.by_mode) {
                *total += mode;
            }
            stats = Stats::default();
            second = now;
        }
    };
    log(&format!(
        "Wayland epoch summary served={} requested={} emitted={} submitted={} keyframes={} \
         keepalives={} activity={} idr={} bytes={} source_frames={} superseded={} \
         best_sustained_source_fps={} qualified_rate={} cursor_events={} \
         cursor_unknown_formats={}",
        plan.pipeline.token(),
        pipeline.preset().pipeline().token(),
        totals.emitted + stats.emitted,
        totals.submitted + stats.submitted,
        totals.keyframes + stats.keyframes,
        totals.by_mode[3] + stats.by_mode[3],
        totals.by_mode[2] + stats.by_mode[2],
        totals.by_mode[1] + stats.by_mode[1],
        totals.bytes + stats.bytes,
        source_stats.frames.load(Ordering::Relaxed),
        leases.superseded.load(Ordering::Relaxed),
        rate.best_sustained()
            .map_or_else(|| "unmeasured".to_owned(), |fps| fps.to_string()),
        pipeline
            .rate_evidence()
            .map_or_else(|| "none".to_owned(), |fps| fps.to_string()),
        epoch_output.cursor_events_sent(),
        epoch_output.unknown_cursor_formats(),
    ));
    if let Some(telemetry) = telemetry.as_ref() {
        telemetry.flush();
    }
    drop(control);
    drop(encoder);
    // The source first: its leases must not outlive the importer that may
    // still be named in an in-flight release.
    drop(source);
    drop(gpu_route);
    drop(retained);
    drop(context);
    Ok(exit)
}
