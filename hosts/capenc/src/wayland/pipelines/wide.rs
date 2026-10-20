//! The wide epoch runner of the Wayland Grading and HDR compositions.
//!
//! One epoch, in order:
//!
//! 1. open CUDA on the encoder GPU and, when the compositor is that same
//!    GPU, the wide read-back ([`WideReadbackImporter`]); offer the
//!    composition's wide formats as DMA-BUF with the importable modifiers,
//!    then as shared memory (whose frames are served only when the host's
//!    qualification proved that memory carries wide content: Mutter 50 and
//!    51 paint eight-bit pixels into wide shared-memory buffers);
//! 2. take the first real frame and read it back into a host frame
//!    (`XBGR2101010` words or RGBA half floats), releasing the producer
//!    buffer at once;
//! 3. build source evidence from the negotiated format's colour tags and
//!    the host's qualification facts (`fidelity-route=`, `wide-proven=`,
//!    `sdr-white=`) and run the composition's shared admission; open NVENC
//!    for the *served* contract and state the decision (`ADMISSION`);
//! 4. on every new frame, read it back; half floats are normalised once by
//!    the shared conversion into ten-bit codes in the served transfer; on
//!    every submission the shared packed-RGB10 conversion stages NVENC's
//!    P16 input from that host frame (the Xorg Grading mechanism);
//! 5. `READY` after the first access unit, framed access units, a stats line
//!    every second.
//!
//! With `fidelity-probe`, the epoch measures the first frame instead of
//! serving it: a drawn ramp's distinct codes and error, and the SDR white
//! patch's luminance, on one `FIDELITY` line, all on the host.

use std::os::fd::BorrowedFd;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use arcen_keel::SubmissionMode;
use arcen_media::annexb::{annex_b_is_keyframe, NalCodec};
use arcen_media::video::compositor_fidelity::{
    channel_order_verified, channel_patch_at, measure_ramp, precision_from_ramp, white_patch_at,
};
use arcen_media::video::{
    convert_rgba_f16_pq_to_packed_rgb10, pq_white, AdmissionRequest, CaptureAdmissionNotice,
    CaptureBackend, CaptureLease, CaptureMemory, CapturePixelFormat, CaptureSourceEvidence,
    CaptureTakeError, ConversionBackend, CopyPath, CursorEvidence, EncoderBackend,
    EncoderCapability, FirstFrameEvidence, FormatModifier, GpuMatch, PackedRgb10Layout,
    PresetAdmission, Rgb10Signal, SamplePrecision, SourceFact, SourceTransfer, SourceTransform,
    SustainedRateMeter, VideoVariant,
};
use arcen_media::VideoCodec;

use super::{EpochRequest, FidelityArgs, WaylandPipeline};
use crate::linux::{cuda, NativeStartupError};
use crate::linux_policy::SubmissionGate;
use crate::log;
use crate::wayland::explicit_sync::SyncobjDevice;
use crate::wayland::format::{MemoryOffer, NegotiatedFormat};
use crate::wayland::gpu_convert::AcquireSync;
use crate::wayland::handoff::WaylandHandoff;
use crate::wayland::host_copy::plane_bytes;
use crate::wayland::pipewire::{BufferToken, PipeWireSource, PlaneView, SourceConfig};
use crate::wayland::wide_readback::{WideHostLayout, WideReadbackImporter};

const NEGOTIATION_TIMEOUT: Duration = Duration::from_secs(5);
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(8);
const IDLE_KEEPALIVE: Duration = Duration::from_secs(1);
const MAILBOX_CAPACITY: usize = 2;
/// Prefix of the measurement line on capenc's stderr.
pub const FIDELITY_PREFIX: &str = "[capenc] FIDELITY ";

/// The newest wide frame, in host memory, as the shared conversion takes it.
struct HostWideFrame {
    /// The read-back (or shared-memory copy) of the producer's buffer.
    raw: Vec<u8>,
    raw_layout: WideHostLayout,
    /// The channel order of the ten-bit words in `raw`, as the last import
    /// wrote them: the read-back's `XBGR2101010` for a DMA-BUF, the
    /// producer's own order for a shared-memory copy.
    words: PackedRgb10Layout,
    /// Packed ten-bit words NVENC's staging reads; the raw frame itself for
    /// ten-bit sources, the normalised half floats otherwise.
    packed: Vec<u8>,
    width: usize,
    height: usize,
    valid: bool,
}

impl HostWideFrame {
    fn new(width: u32, height: u32, format: CapturePixelFormat) -> Result<Self, String> {
        let raw_layout = WideHostLayout::for_format(format)
            .ok_or_else(|| format!("{} is not a wide format", format.token()))?;
        let (width, height) = (width as usize, height as usize);
        let raw = vec![0u8; width * height * raw_layout.bytes_per_pixel()];
        let packed = match raw_layout {
            WideHostLayout::Xbgr2101010 => Vec::new(),
            WideHostLayout::RgbaF16 => vec![0u8; width * height * 4],
        };
        Ok(Self {
            raw,
            raw_layout,
            words: PackedRgb10Layout::XBGR2101010,
            packed,
            width,
            height,
            valid: false,
        })
    }

    const fn raw_stride(&self) -> usize {
        self.width * self.raw_layout.bytes_per_pixel()
    }

    /// The packed words and their layout, after [`Self::normalise`].
    fn packed(&self) -> (&[u8], PackedRgb10Layout) {
        match self.raw_layout {
            WideHostLayout::Xbgr2101010 => (&self.raw, self.words),
            WideHostLayout::RgbaF16 => (&self.packed, PackedRgb10Layout::XRGB2101010),
        }
    }

    /// Applies the served half-float signal, in parallel rows.
    fn normalise(&mut self, signal: Rgb10Signal, workers: usize) -> Result<(), String> {
        if self.raw_layout != WideHostLayout::RgbaF16 {
            return Ok(());
        }
        let (width, height) = (self.width, self.height);
        let (src_stride, dst_stride) = (width * 8, width * 4);
        let workers = workers.clamp(1, height.max(1));
        let rows_per = height.div_ceil(workers);
        std::thread::scope(|scope| {
            let jobs: Vec<_> = self
                .raw
                .chunks(rows_per * src_stride)
                .zip(self.packed.chunks_mut(rows_per * dst_stride))
                .map(|(src, dst)| {
                    let rows = src.len() / src_stride;
                    scope.spawn(move || {
                        convert_rgba_f16_pq_to_packed_rgb10(
                            src, src_stride, dst, dst_stride, width, rows, signal,
                        )
                        .map_err(|error| error.to_string())
                    })
                })
                .collect();
            jobs.into_iter().try_for_each(|job| {
                job.join()
                    .map_err(|_| "half-float conversion worker panicked".to_owned())?
            })
        })
    }

    /// Ten-bit PQ codes of row `row` (green channel), for measurement.
    /// The red, green and blue PQ codes of every pixel of `row`.
    fn row_pq_pixels(&self, row: usize) -> Vec<[u16; 3]> {
        let row = row.min(self.height.saturating_sub(1));
        let stride = self.raw_stride();
        let bytes = &self.raw[row * stride..(row + 1) * stride];
        match self.raw_layout {
            WideHostLayout::Xbgr2101010 => bytes
                .chunks_exact(4)
                .map(|word| {
                    self.words
                        .components(u32::from_le_bytes([word[0], word[1], word[2], word[3]]))
                })
                .collect(),
            WideHostLayout::RgbaF16 => bytes
                .chunks_exact(8)
                .map(|pixel| {
                    let half =
                        |offset: usize| u16::from_le_bytes([pixel[offset], pixel[offset + 1]]);
                    Rgb10Signal::Direct.map_pq_half([half(0), half(2), half(4)])
                })
                .collect(),
        }
    }

    /// The green PQ codes of `row` (a grey ramp's measure).
    fn row_pq_codes(&self, row: usize) -> Vec<u16> {
        self.row_pq_pixels(row)
            .into_iter()
            .map(|[_, green, _]| green)
            .collect()
    }
}

/// The DMA-BUF read-back of one epoch, and why it is absent.
struct Readback {
    importer: Option<WideReadbackImporter>,
    sync: Option<Arc<SyncobjDevice>>,
    modifiers: Vec<FormatModifier>,
    unavailable: Option<String>,
}

impl Readback {
    /// # Safety
    ///
    /// On the thread that runs the epoch; dropped before the CUDA context.
    unsafe fn open(
        render_minor: Option<u32>,
        gpu: GpuMatch,
        formats: &[CapturePixelFormat],
        width: u32,
        height: u32,
    ) -> Self {
        let mut readback = Self {
            importer: None,
            sync: None,
            modifiers: Vec::new(),
            unavailable: None,
        };
        if gpu != GpuMatch::Same {
            readback.unavailable = Some(format!("gpu_{}", gpu.token()));
            return readback;
        }
        let Some(minor) = render_minor else {
            readback.unavailable = Some("encoder_render_node_unknown".to_owned());
            return readback;
        };
        let node = std::path::PathBuf::from(format!("/dev/dri/renderD{minor}"));
        match WideReadbackImporter::open(&node, width, height) {
            Ok(importer) => {
                // One modifier list serves every offered format: keep those
                // importable for all of them.
                let mut common: Option<Vec<FormatModifier>> = None;
                for format in formats {
                    let modifiers = importer.offerable_modifiers(*format);
                    common = Some(match common {
                        None => modifiers,
                        Some(previous) => previous
                            .into_iter()
                            .filter(|modifier| modifiers.contains(modifier))
                            .collect(),
                    });
                }
                readback.modifiers = common.unwrap_or_default();
                log(&format!(
                    "wide read-back ready on {} {} modifiers={}",
                    node.display(),
                    importer.describe(),
                    readback
                        .modifiers
                        .iter()
                        .map(|modifier| format!("{:#018x}", modifier.0))
                        .collect::<Vec<_>>()
                        .join(",")
                ));
                if readback.modifiers.is_empty() {
                    readback.unavailable = Some("no_importable_modifier".to_owned());
                } else {
                    readback.importer = Some(importer);
                    match SyncobjDevice::open(&node) {
                        Ok(device) => readback.sync = Some(Arc::new(device)),
                        Err(error) => log(&format!(
                            "explicit sync unavailable ({error}); wide read-backs wait on \
                             implicit fences"
                        )),
                    }
                }
            }
            Err(error) => readback.unavailable = Some(format!("{}: {error}", error.token())),
        }
        readback
    }
}

/// What reading one leased frame did.
enum WideImport {
    Imported(Duration),
    Revoked,
    Failed(String),
}

/// Reads `lease` into `frame` by the route its memory takes.
///
/// # Safety
///
/// On the epoch's thread (the read-back's GL context is current there).
unsafe fn import_wide(
    lease: &CaptureLease<BufferToken>,
    frame: &mut HostWideFrame,
    readback: &mut Readback,
) -> WideImport {
    let leased = lease.frame();
    let surface = leased.surface.surface();
    if WideHostLayout::for_format(surface.format) != Some(frame.raw_layout) {
        return WideImport::Failed(format!(
            "frame format {} is not the epoch's",
            surface.format.token()
        ));
    }
    let started = Instant::now();
    let stride = frame.raw_stride();
    match surface.memory {
        CaptureMemory::HostShared => {
            let plane = surface.planes[0];
            let result = leased.token.read(|views| -> Result<(), String> {
                let PlaneView::Host(buffer) = views[usize::from(plane.buffer)] else {
                    return Err("host plane is not mapped".to_owned());
                };
                let rows = plane_bytes(buffer, plane.offset, plane.size)
                    .ok_or("plane outside its mapping")?;
                let source_stride = plane.stride as usize;
                for row in 0..frame.height {
                    let source = rows
                        .get(row * source_stride..row * source_stride + stride)
                        .ok_or("short shared-memory frame")?;
                    frame.raw[row * stride..(row + 1) * stride].copy_from_slice(source);
                }
                // The producer's bytes as they are: their own channel order.
                frame.words = shared_memory_words(surface.format);
                Ok(())
            });
            match result {
                Ok(Ok(())) => WideImport::Imported(started.elapsed()),
                Ok(Err(error)) => WideImport::Failed(error),
                Err(_) => WideImport::Revoked,
            }
        }
        CaptureMemory::DmaBuf { .. } => {
            let Some(importer) = readback.importer.as_mut() else {
                return WideImport::Failed("the wide read-back is not open".to_owned());
            };
            let points = leased.token.explicit_sync();
            let result = leased.token.read(|views| {
                let fds: Vec<BorrowedFd<'_>> = views
                    .iter()
                    .filter_map(|view| match view {
                        PlaneView::DmaBuf(fd) => Some(*fd),
                        _ => None,
                    })
                    .collect();
                let acquire = match (points, readback.sync.as_deref()) {
                    (Some(points), Some(device)) => AcquireSync::Explicit {
                        device,
                        fd: points.acquire_fd,
                        point: points.acquire_point,
                    },
                    _ => AcquireSync::Implicit,
                };
                let imported = importer.import(surface, &fds, acquire, &mut frame.raw, stride);
                // GL reads channels by name into XBGR2101010 words.
                frame.words = PackedRgb10Layout::XBGR2101010;
                imported
            });
            match result {
                Ok(Ok(_)) => WideImport::Imported(started.elapsed()),
                Ok(Err(error)) => WideImport::Failed(format!("{}: {error}", error.token())),
                Err(_) => WideImport::Revoked,
            }
        }
        CaptureMemory::GpuDevice => {
            WideImport::Failed("no route reads GPU device memory".to_owned())
        }
    }
}

/// The channel order of a producer's ten-bit shared-memory words.
const fn shared_memory_words(format: CapturePixelFormat) -> PackedRgb10Layout {
    match format {
        CapturePixelFormat::Xrgb2101010 => PackedRgb10Layout::XRGB2101010,
        _ => PackedRgb10Layout::XBGR2101010,
    }
}

/// The admission input for a wide epoch: the producer's colour tags, and
/// the host's qualification facts for everything a tag cannot prove.
#[must_use]
pub fn wide_source_evidence(
    negotiated: &NegotiatedFormat,
    memory: CaptureMemory,
    gpu: GpuMatch,
    cursor: CursorEvidence,
    facts: &FidelityArgs,
) -> CaptureSourceEvidence {
    let mut colour = negotiated.colour;
    colour.sdr_white_nits = facts
        .sdr_white_nits
        .map_or(SourceFact::Unknown, SourceFact::Authoritative);
    let pq = colour.transfer.authoritative() == Some(SourceTransfer::Pq);
    CaptureSourceEvidence {
        backend: CaptureBackend::PipeWire,
        format: negotiated.format,
        colour,
        sample_precision: if facts.proves(negotiated.format, memory) {
            SourceFact::Authoritative(SamplePrecision::NativeWide)
        } else {
            SourceFact::Unknown
        },
        // The producer's PQ tags say what the buffers hold; the session's
        // proven route says the output is composed in that HDR state.
        output_hdr_active: if pq && facts.route.is_some() {
            SourceFact::Authoritative(true)
        } else {
            SourceFact::Unknown
        },
        // Pixels transit host memory on every route of this runner.
        copy_path: CopyPath::HostCopy,
        gpu,
        sustained_fps: SourceFact::Unknown,
        cursor,
    }
}

/// The `FIDELITY` measurement of one frame.
fn measurement_line(
    frame: &HostWideFrame,
    first: &FirstFrameEvidence,
    negotiated: &NegotiatedFormat,
    import: Duration,
) -> String {
    let ramp = measure_ramp(&frame.row_pq_codes(frame.height / 4));
    let precision = match precision_from_ramp(ramp) {
        Some(SamplePrecision::NativeWide) => "native-wide",
        Some(SamplePrecision::UpconvertedEightBit) => "upconverted-eight-bit",
        None => "inconclusive",
    };
    let (row, column) = white_patch_at(frame.width, frame.height);
    let white_code = frame.row_pq_codes(row)[column];
    let (row, column) = channel_patch_at(frame.width, frame.height);
    let channel = frame.row_pq_pixels(row)[column];
    let channels = if channel_order_verified(channel) {
        "ok"
    } else {
        "wrong"
    };
    format!(
        "{FIDELITY_PREFIX}version=2 format={} memory={} transfer={} primaries={} width={} \
         height={} ramp_expected={} ramp_distinct={} ramp_max_error={} precision={precision} \
         channels={channels} channel_codes={},{},{} white_pq_code={white_code} \
         white_nits={:.1} import_ms={:.2}",
        first.format.token(),
        first.memory.token(),
        negotiated
            .colour
            .transfer
            .value()
            .map_or("unknown", SourceTransfer::token),
        negotiated
            .colour
            .primaries
            .value()
            .map_or("unknown", |primaries| primaries.token()),
        first.width,
        first.height,
        ramp.expected_codes,
        ramp.distinct_codes,
        ramp.max_error_codes,
        channel[0],
        channel[1],
        channel[2],
        pq_white::pq_code_to_nits(white_code),
        import.as_secs_f64() * 1000.0,
    )
}

const fn served_wide_source(transform: SourceTransform) -> Option<crate::nvenc_cuda::WideSource> {
    match transform {
        // PQ codes: the encoder keeps them (HDR) or converts them (Grading).
        SourceTransform::PackedRgb10(_) | SourceTransform::HalfFloatPq(Rgb10Signal::Direct) => {
            Some(crate::nvenc_cuda::WideSource::ColorManagedPq)
        }
        // Already BT.709 SDR codes after the half-float conversion.
        SourceTransform::HalfFloatPq(Rgb10Signal::PqBt2020ToSdrBt709) => {
            Some(crate::nvenc_cuda::WideSource::NormalisedSdr)
        }
        _ => None,
    }
}

/// Runs one wide epoch.
///
/// # Safety
///
/// CUDA is initialised and used only on this thread, and every CUDA and GL
/// object the epoch creates is dropped before its context.
#[allow(clippy::too_many_lines)] // One linear epoch.
pub unsafe fn run_native(
    request: &EpochRequest,
    handoff: &WaylandHandoff,
    pipeline: WaylandPipeline,
    cursor: CursorEvidence,
) -> Result<i32, NativeStartupError> {
    let context = cuda::init_from_env()?;
    let (width, height) = handoff.size();
    let (source_gpu, encoder_gpu) =
        crate::wayland::probe::identities(handoff, context.device_pci());
    let gpu = arcen_media::video::compare_gpus(source_gpu.as_ref(), encoder_gpu.as_ref());
    let mut offer = pipeline.offer(request.fps);
    // A serving epoch asks only for what this host proved; the probe asks
    // for the composition's own preference, which is what it measures.
    if !request.fidelity.probe && !request.fidelity.proven.is_empty() {
        offer.formats.retain(|format| {
            request
                .fidelity
                .proven
                .iter()
                .any(|proven| proven.format == *format)
        });
    }
    let mut readback = Readback::open(
        encoder_gpu.and_then(|identity| identity.render_node_minor),
        gpu,
        &offer.formats,
        width,
        height,
    );
    if readback.importer.is_some() {
        offer.memory = MemoryOffer::DmaBufThenHost {
            modifiers: readback.modifiers.clone(),
        };
    }
    offer.size = Some(handoff.size());
    log(&format!(
        "Wayland wide capture offer formats={} max_fps={} memory={}{} route={} wide_proven={} \
         sdr_white={}",
        offer
            .formats
            .iter()
            .map(|format| format.token())
            .collect::<Vec<_>>()
            .join(","),
        offer.max_fps,
        if readback.importer.is_some() {
            "dmabuf+shm"
        } else {
            "shm"
        },
        readback
            .unavailable
            .as_ref()
            .map_or_else(String::new, |reason| format!(
                " dmabuf_unavailable={reason}"
            )),
        request.fidelity.route.map_or("none", |route| route.token()),
        request.fidelity.proven_token(),
        request
            .fidelity
            .sdr_white_nits
            .map_or_else(|| "unmeasured".to_owned(), |nits| nits.to_string()),
    ));
    let dmabuf_offered = offer.memory.offers_dmabuf();
    let source = match PipeWireSource::start(
        handoff,
        SourceConfig {
            offer,
            mailbox_capacity: MAILBOX_CAPACITY,
            source_gpu,
            encoder_gpu,
            // The DMA-BUF is imported on the encoder GPU (validated as the
            // same-GPU import it is) and then read back; the epoch reports
            // `host-copy` because the pixels transit host memory.
            dmabuf_copy_path: dmabuf_offered.then_some(CopyPath::GpuConvert),
            explicit_sync: if dmabuf_offered {
                readback.sync.clone()
            } else {
                None
            },
        },
    ) {
        Ok(source) => source,
        Err(error) => {
            log(&format!("ERROR: Wayland wide capture: {error}"));
            return Ok(3);
        }
    };
    let Some(negotiated) = source.wait_negotiated(NEGOTIATION_TIMEOUT) else {
        log(&format!(
            "ERROR: Wayland wide capture: no wide format negotiated within \
             {NEGOTIATION_TIMEOUT:?} (phase {:?}); the compositor offers no ten-bit or half-float \
             format in this colour state",
            source.phase()
        ));
        return Ok(3);
    };
    if (negotiated.width, negotiated.height) != (width, height) {
        log(&format!(
            "ERROR: Wayland wide capture: negotiated {}x{} is not the hand-off's {width}x{height}",
            negotiated.width, negotiated.height
        ));
        return Ok(3);
    }
    let mut frame =
        HostWideFrame::new(width, height, negotiated.format).map_err(NativeStartupError::fatal)?;

    let first_deadline = Instant::now() + FIRST_FRAME_TIMEOUT;
    let (first_import, first_frame, first_gpu) = loop {
        let wait = first_deadline.saturating_duration_since(Instant::now());
        let lease = match source.next_frame(wait) {
            Ok(Some(lease)) => lease,
            Ok(None) => {
                log(&format!(
                    "ERROR: Wayland wide capture: no first frame within {FIRST_FRAME_TIMEOUT:?} \
                     (phase {:?})",
                    source.phase()
                ));
                return Ok(3);
            }
            Err(CaptureTakeError::Closed) => {
                log("ERROR: Wayland wide capture: the stream closed before its first frame");
                return Ok(3);
            }
        };
        match import_wide(&lease, &mut frame, &mut readback) {
            WideImport::Imported(elapsed) => {
                let surface = lease.frame().surface.surface();
                break (
                    elapsed,
                    FirstFrameEvidence {
                        node_id: handoff.node_id(),
                        format: surface.format,
                        width: surface.geometry.coded_width,
                        height: surface.geometry.coded_height,
                        memory: surface.memory,
                    },
                    lease.frame().surface.gpu_match(),
                );
            }
            WideImport::Revoked => {}
            WideImport::Failed(error) => {
                log(&format!(
                    "ERROR: Wayland wide capture: first frame read-back failed: {error}"
                ));
                return Ok(3);
            }
        }
    };
    frame.valid = true;
    let negotiated = source.current_format().unwrap_or(negotiated);

    if request.fidelity.probe {
        let line = measurement_line(&frame, &first_frame, &negotiated, first_import);
        eprintln!("{line}");
        drop(source);
        drop(readback);
        drop(context);
        return Ok(0);
    }

    let evidence = wide_source_evidence(
        &negotiated,
        first_frame.memory,
        if first_gpu == GpuMatch::Unknown {
            gpu
        } else {
            first_gpu
        },
        cursor,
        &request.fidelity,
    );
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
    let admission = pipeline.admit(admission_request, &evidence, &capability);
    let notice = CaptureAdmissionNotice {
        requested: pipeline.preset(),
        requested_fps: request.fps,
        admission,
        capture: CaptureBackend::PipeWire,
        copy_path: CopyPath::HostCopy,
        cursor,
        measured_fps: None,
        rate_evidence: pipeline.rate_source(),
        frame: first_frame,
    };
    let Some(served) = admission.served().copied() else {
        let _ = crate::announce_admission(&notice);
        log(&format!(
            "ERROR: the Wayland wide source cannot serve {}: {}",
            pipeline.preset().pipeline().token(),
            match admission {
                PresetAdmission::Unavailable {
                    reason, fallback, ..
                } => fallback.map_or_else(
                    || reason.to_string(),
                    |fallback| format!("{reason} (Grading: {fallback})")
                ),
                _ => String::new(),
            }
        ));
        let _ = crate::announce_unavailable(
            EncoderBackend::NativeNvenc,
            arcen_media::video::BackendUnavailableReason::UnsupportedConfiguration,
        );
        return Ok(2);
    };
    let plan = served
        .epoch()
        .ok_or_else(|| NativeStartupError::fatal("the admitted plan states no cursor truth"))?;
    let Some(wide_source) = served_wide_source(served.transform) else {
        return Err(NativeStartupError::fatal(format!(
            "admission chose {} for a wide epoch",
            served.transform.token()
        )));
    };
    let signal = match served.transform {
        SourceTransform::HalfFloatPq(signal) => signal,
        _ => Rgb10Signal::Direct,
    };
    let workers = std::thread::available_parallelism().map_or(4, |count| count.get().min(8));
    frame
        .normalise(signal, workers)
        .map_err(NativeStartupError::fatal)?;
    let codec = plan.video.codec;
    let colour = crate::ColorSpec::from_variant(VideoVariant::new(plan.video));
    let nal_codec = match codec {
        VideoCodec::H264 => Some(NalCodec::H264),
        VideoCodec::H265 => Some(NalCodec::H265),
        _ => None,
    };
    let mut encoder = crate::nvenc_cuda::Encoder::new_for_source(
        context.as_raw(),
        width,
        height,
        codec.token(),
        colour,
        plan.intent,
        plan.priority,
        crate::qp_map::QpMapPolicy::Off,
        plan.fps,
        request.max_bitrate_bps,
        plan.keyframe,
        wide_source,
    )?;
    if let Err(error) = crate::announce_admission(&notice) {
        log(&format!("ERROR: emit ADMISSION: {error}"));
        return Ok(5);
    }
    log(&format!(
        "Wayland wide epoch serving {} for {} at up to {} fps: transform={} wide_source={:?} \
         format={} memory={} colour={:?}/{:?}/{:?} copy_path=host-copy cursor={} \
         first_import_ms={:.2}{}",
        plan.pipeline.token(),
        pipeline.preset().pipeline().token(),
        plan.fps,
        served.transform.token(),
        wide_source,
        first_frame.format.token(),
        first_frame.memory.token(),
        plan.video.transfer,
        plan.video.primaries,
        plan.video.matrix,
        cursor.token(),
        first_import.as_secs_f64() * 1000.0,
        match admission {
            PresetAdmission::Downgrade { reason, .. } => format!(
                " downgraded_from={} reason={reason}",
                pipeline.preset().pipeline().token()
            ),
            _ => String::new(),
        },
    ));

    let control = crate::spawn_control_thread("NVENC-Wayland-wide");
    let mut stdout = std::io::stdout();
    let mut target_dt = crate::frame_interval_from_fps(plan.fps);
    let mut next = Instant::now();
    let mut gate = SubmissionGate::new(IDLE_KEEPALIVE);
    gate.note_frame();
    let mut last_emit = Instant::now();
    let mut ready_announced = false;
    let mut stream_truth = crate::StreamTruthLog::new(codec.token());
    let mut rate = SustainedRateMeter::new(Instant::now());
    let source_stats = &source.shared().stats;
    let mut source_frames_seen = source_stats.frames.load(Ordering::Relaxed);
    let mut second = Instant::now();
    let mut pacing = crate::linux_policy::PacingWindow::default();
    let mut target_fps = plan.fps;
    let (mut emitted, mut submitted, mut bytes, mut keyframes) = (0u64, 0u64, 0u64, 0u64);
    let (mut imports, mut import_ms_sum, mut import_ms_max) = (0u64, 0f64, 0f64);
    let (mut stage_ms_sum, mut encode_ms_sum) = (0f64, 0f64);
    let mut total_emitted = 0u64;
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
        match source.next_frame(Duration::ZERO) {
            Ok(Some(lease)) => {
                let memory = lease.frame().surface.surface().memory;
                if std::mem::discriminant(&memory) != std::mem::discriminant(&first_frame.memory) {
                    // Admission proved the first frame's memory only; a move
                    // to another (a DMA-BUF withdrawal onto shared memory,
                    // whose wide buffers Mutter fills with eight-bit pixels)
                    // has nothing proven to serve.
                    log(&format!(
                        "ERROR: Wayland wide capture: the producer moved to {} memory, which this \
                         host did not prove; the epoch ends",
                        memory.token()
                    ));
                    break 3;
                }
                match import_wide(&lease, &mut frame, &mut readback) {
                    WideImport::Imported(elapsed) => {
                        drop(lease);
                        if let Err(error) = frame.normalise(signal, workers) {
                            log(&format!("ERROR: half-float conversion: {error}"));
                            break 5;
                        }
                        let ms = elapsed.as_secs_f64() * 1000.0;
                        imports += 1;
                        import_ms_sum += ms;
                        import_ms_max = import_ms_max.max(ms);
                        frame.valid = true;
                        gate.note_frame();
                    }
                    WideImport::Revoked => {}
                    WideImport::Failed(error) => {
                        // The host frame may be partial: never encode it.
                        log(&format!(
                            "ERROR: Wayland wide read-back: {error}; the epoch ends"
                        ));
                        frame.valid = false;
                        break 3;
                    }
                }
            }
            Ok(None) => {}
            Err(CaptureTakeError::Closed) => {
                log(&format!(
                    "ERROR: Wayland wide capture: the stream closed (phase {:?})",
                    source.phase()
                ));
                break 3;
            }
        }
        if let Some(current) = source.current_format() {
            if (current.width, current.height, current.format)
                != (width, height, first_frame.format)
            {
                log(&format!(
                    "ERROR: Wayland wide capture: the producer renegotiated {}x{} {}; this epoch \
                     ends",
                    current.width,
                    current.height,
                    current.format.token()
                ));
                break 3;
            }
        }

        let idr_pending = control.idr_pending();
        let decision = if frame.valid {
            gate.decision(idr_pending, last_emit.elapsed())
        } else {
            None
        };
        pacing.record_due(decision);
        if let Some(mode) = decision {
            let stage_started = Instant::now();
            let (packed, layout) = frame.packed();
            if let Err(error) = encoder.stage_wide_host(packed, frame.width * 4, layout) {
                log(&format!("ERROR: wide stage failed: {error}"));
                break 5;
            }
            stage_ms_sum += stage_started.elapsed().as_secs_f64() * 1000.0;
            let requested_idr = idr_pending && control.take_idr();
            let force = mode == SubmissionMode::FirstFrame || requested_idr;
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
            let encode_started = Instant::now();
            match encoder.encode(force) {
                Ok(output) => {
                    encode_ms_sum += encode_started.elapsed().as_secs_f64() * 1000.0;
                    submitted += 1;
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
                                colour,
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
                            // The wide conversion runs on the CPU, as for
                            // Xorg Grading; the copy path is in ADMISSION.
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
                        if nal_codec.is_some_and(|nal| annex_b_is_keyframe(&access_unit, nal)) {
                            keyframes += 1;
                        }
                        stream_truth.observe(&access_unit);
                        if epoch_output.access_unit(&mut stdout, &access_unit).is_err() {
                            break 'epoch 0;
                        }
                        emitted += 1;
                        bytes += access_unit.len() as u64;
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
            let average = |sum: f64, count: u64| {
                if count == 0 {
                    0.0
                } else {
                    sum / count as f64
                }
            };
            log(&format!(
                "enc_fps={emitted} encode_submitted={submitted} source_fps={} \
                 avg_import_ms={:.2} max_import_ms={:.2} avg_stage_ms={:.2} avg_encode_ms={:.2} \
                 kbps={} keyframes={keyframes} capture_backend=pipewire copy_path=host-copy \
                 memory={} served={} fps_ceiling={} transform={}{}{}",
                rate.last_window().unwrap_or(0),
                average(import_ms_sum, imports),
                import_ms_max,
                average(stage_ms_sum, submitted),
                average(encode_ms_sum, submitted),
                bytes * 8 / 1000,
                first_frame.memory.token(),
                plan.pipeline.token(),
                plan.fps,
                served.transform.token(),
                encoder.take_wide_stage_stats().fields(),
                pacing.verdict_fields(
                    plan.fps,
                    target_fps,
                    Duration::from_secs_f64(
                        (import_ms_sum + stage_ms_sum + encode_ms_sum).max(0.0) / 1000.0,
                    ),
                    now.duration_since(second),
                ),
            ));
            pacing = crate::linux_policy::PacingWindow::default();
            total_emitted += emitted;
            (emitted, submitted, bytes, keyframes) = (0, 0, 0, 0);
            (imports, import_ms_sum, import_ms_max) = (0, 0.0, 0.0);
            (stage_ms_sum, encode_ms_sum) = (0.0, 0.0);
            second = now;
        }
    };
    log(&format!(
        "Wayland wide epoch summary served={} requested={} emitted={} source_frames={} \
         best_sustained_source_fps={} cursor_events={} cursor_unknown_formats={}",
        plan.pipeline.token(),
        pipeline.preset().pipeline().token(),
        total_emitted + emitted,
        source_stats.frames.load(Ordering::Relaxed),
        rate.best_sustained()
            .map_or_else(|| "unmeasured".to_owned(), |fps| fps.to_string()),
        epoch_output.cursor_events_sent(),
        epoch_output.unknown_cursor_formats(),
    ));
    drop(control);
    drop(encoder);
    drop(source);
    drop(readback);
    drop(context);
    Ok(exit)
}

#[cfg(test)]
mod tests {
    use arcen_media::video::compositor_fidelity::{
        expected_ramp_code, FidelityRoute, CHANNEL_PROBE_CODES,
    };
    use arcen_media::video::SourceColour;
    use arcen_media::ColorPrimaries;

    use super::*;

    fn negotiated(format: CapturePixelFormat) -> NegotiatedFormat {
        NegotiatedFormat {
            format,
            width: 1920,
            height: 1080,
            modifier: None,
            colour: SourceColour {
                primaries: SourceFact::Authoritative(ColorPrimaries::Bt2020),
                transfer: SourceFact::Authoritative(SourceTransfer::Pq),
                ..SourceColour::UNKNOWN
            },
            framerate: (0, 1),
            max_framerate: (60, 1),
        }
    }

    fn dmabuf() -> CaptureMemory {
        CaptureMemory::DmaBuf {
            modifier: FormatModifier(0x0300_0000_00cd_b014),
        }
    }

    #[test]
    fn evidence_claims_only_what_the_host_proved() {
        let unproven = wide_source_evidence(
            &negotiated(CapturePixelFormat::Xbgr2101010),
            dmabuf(),
            GpuMatch::Same,
            CursorEvidence::Composited,
            &FidelityArgs::default(),
        );
        assert_eq!(unproven.sample_precision, SourceFact::Unknown);
        assert_eq!(unproven.output_hdr_active, SourceFact::Unknown);
        assert_eq!(unproven.colour.sdr_white_nits, SourceFact::Unknown);
        assert_eq!(unproven.copy_path, CopyPath::HostCopy);
        let facts = FidelityArgs {
            route: Some(FidelityRoute::ForcedHdr),
            proven: vec![super::super::ProvenWide {
                format: CapturePixelFormat::Xbgr2101010,
                memory: "dmabuf",
            }],
            sdr_white_nits: Some(203),
            ..FidelityArgs::default()
        };
        let proven = wide_source_evidence(
            &negotiated(CapturePixelFormat::Xbgr2101010),
            dmabuf(),
            GpuMatch::Same,
            CursorEvidence::Composited,
            &facts,
        );
        assert_eq!(
            proven.sample_precision,
            SourceFact::Authoritative(SamplePrecision::NativeWide)
        );
        assert_eq!(proven.output_hdr_active, SourceFact::Authoritative(true));
        assert_eq!(proven.colour.sdr_white_nits, SourceFact::Authoritative(203));
        // The proof is per memory: Mutter's shared-memory wide buffers carry
        // eight-bit pixels and are never trusted on a DMA-BUF proof.
        let shm = wide_source_evidence(
            &negotiated(CapturePixelFormat::Xbgr2101010),
            CaptureMemory::HostShared,
            GpuMatch::Same,
            CursorEvidence::Composited,
            &facts,
        );
        assert_eq!(shm.sample_precision, SourceFact::Unknown);
        // ... and per format: a proof of xBGR does not vouch for half floats.
        let f16 = wide_source_evidence(
            &negotiated(CapturePixelFormat::RgbaF16),
            dmabuf(),
            GpuMatch::Same,
            CursorEvidence::Composited,
            &facts,
        );
        assert_eq!(f16.sample_precision, SourceFact::Unknown);
    }

    #[test]
    fn shared_memory_words_keep_the_producers_channel_order() {
        let (width, height) = (16usize, 2usize);
        let [red, green, blue] = CHANNEL_PROBE_CODES.map(u32::from);
        for (format, word) in [
            (
                CapturePixelFormat::Xrgb2101010,
                (red << 20) | (green << 10) | blue,
            ),
            (
                CapturePixelFormat::Xbgr2101010,
                (blue << 20) | (green << 10) | red,
            ),
        ] {
            let mut frame = HostWideFrame::new(16, 2, format).expect("frame");
            for pixel in frame.raw.chunks_exact_mut(4) {
                pixel.copy_from_slice(&word.to_le_bytes());
            }
            frame.words = shared_memory_words(format);
            assert_eq!(frame.row_pq_pixels(1)[3], CHANNEL_PROBE_CODES, "{format:?}");
            let (packed, layout) = frame.packed();
            assert_eq!(packed.len(), width * height * 4);
            assert_eq!(
                layout.components(u32::from_le_bytes([
                    packed[0], packed[1], packed[2], packed[3]
                ])),
                CHANNEL_PROBE_CODES,
                "{format:?} is staged with red as red"
            );
        }
    }

    #[test]
    fn the_served_transform_picks_the_encoder_source() {
        use crate::nvenc_cuda::WideSource;
        assert_eq!(
            served_wide_source(SourceTransform::PackedRgb10(Rgb10Signal::Direct)),
            Some(WideSource::ColorManagedPq)
        );
        assert_eq!(
            served_wide_source(SourceTransform::PackedRgb10(
                Rgb10Signal::PqBt2020ToSdrBt709
            )),
            Some(WideSource::ColorManagedPq)
        );
        assert_eq!(
            served_wide_source(SourceTransform::HalfFloatPq(Rgb10Signal::Direct)),
            Some(WideSource::ColorManagedPq)
        );
        assert_eq!(
            served_wide_source(SourceTransform::HalfFloatPq(
                Rgb10Signal::PqBt2020ToSdrBt709
            )),
            Some(WideSource::NormalisedSdr)
        );
        assert_eq!(served_wide_source(SourceTransform::Sdr8Rgb), None);
    }

    #[test]
    fn a_ten_bit_frame_is_measured_and_staged_as_it_was_read() {
        let (width, height) = (1920usize, 8usize);
        let mut frame =
            HostWideFrame::new(1920, 8, CapturePixelFormat::Xbgr2101010).expect("frame");
        for row in 0..height {
            for x in 0..width {
                let code = u32::from(expected_ramp_code(x, width));
                let word = code | (code << 10) | (code << 20);
                let at = (row * width + x) * 4;
                frame.raw[at..at + 4].copy_from_slice(&word.to_le_bytes());
            }
        }
        let ramp = measure_ramp(&frame.row_pq_codes(height / 4));
        assert_eq!(ramp.distinct_codes, 1024);
        assert_eq!(ramp.max_error_codes, 0);
        frame.normalise(Rgb10Signal::PqBt2020ToSdrBt709, 4).unwrap();
        let (packed, layout) = frame.packed();
        assert_eq!(layout, PackedRgb10Layout::XBGR2101010);
        assert_eq!(packed.len(), width * height * 4);
    }

    #[test]
    fn half_floats_are_normalised_once_into_packed_words() {
        let (width, height) = (16usize, 3usize);
        let mut frame = HostWideFrame::new(16, 3, CapturePixelFormat::RgbaF16).expect("frame");
        // PQ signal 1.0 everywhere: HDR keeps code 1023.
        for pixel in frame.raw.chunks_exact_mut(8) {
            for channel in pixel.chunks_exact_mut(2) {
                channel.copy_from_slice(&0x3c00u16.to_le_bytes());
            }
        }
        frame.normalise(Rgb10Signal::Direct, 2).unwrap();
        let (packed, layout) = frame.packed();
        assert_eq!(layout, PackedRgb10Layout::XRGB2101010);
        assert_eq!(packed.len(), width * height * 4);
        assert!(packed.chunks_exact(4).all(|word| layout
            .components(u32::from_le_bytes([word[0], word[1], word[2], word[3]]))
            == [1023, 1023, 1023]));
    }
}
