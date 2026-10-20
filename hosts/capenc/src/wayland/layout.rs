//! From a delivered `PipeWire` buffer, described as plain data, to a
//! [`CaptureSurface`].
//!
//! The `PipeWire` callback copies the facts of one `spa_buffer` (data kinds,
//! chunk offsets/sizes/strides, buffer sizes, flags and metadata) into a
//! [`RawBuffer`]. [`classify`] decides, without any native handle, whether it
//! is a frame, a cursor-only update, or something to release unread. Every
//! layout rule a GPU or CPU reader depends on is enforced afterwards by the
//! shared [`CaptureSurface::validate`].

use arcen_keel::PixelRect;
use arcen_media::video::{
    CaptureBackend, CaptureGeometry, CaptureMemory, CaptureSurface, CaptureTimestamps, CopyPath,
    CursorEvidence, DamageEvidence, FormatModifier, GpuIdentity, PlaneLayout,
};
use arcen_media::{OutputIdentity, RegionGeneration};

use super::cursor::{CursorMeta, CursorMetaError};
use super::format::{NegotiatedFormat, DAMAGE_META_MAX_REGIONS};

/// SPA data kind of one plane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataKind {
    /// `SPA_DATA_MemFd`: shared memory backed by a file descriptor.
    MemFd,
    /// `SPA_DATA_MemPtr`: shared memory already mapped by the producer.
    MemPtr,
    /// `SPA_DATA_DmaBuf`.
    DmaBuf,
    /// Anything else.
    Other(u32),
}

/// One `spa_data` and its `spa_chunk`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawPlane {
    /// Memory kind.
    pub kind: DataKind,
    /// File descriptor, for `MemFd` and `DmaBuf`.
    pub fd: Option<i64>,
    /// Whether `PipeWire` mapped the plane into this process.
    pub mapped: bool,
    /// Bytes in the mapped region or buffer object (`spa_data.maxsize`).
    pub max_size: u32,
    /// Offset of the plane's data within its buffer object (`spa_chunk.offset`).
    pub chunk_offset: u32,
    /// Valid bytes (`spa_chunk.size`); zero for a cursor-only update.
    pub chunk_size: u32,
    /// Row stride (`spa_chunk.stride`); negative means bottom-up.
    pub chunk_stride: i32,
    /// `SPA_CHUNK_FLAG_CORRUPTED`.
    pub corrupted: bool,
}

/// `spa_meta_header` facts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeaderMeta {
    /// `SPA_META_HEADER_FLAG_CORRUPTED` was set.
    pub corrupted: bool,
    /// Producer sequence.
    pub seq: u64,
    /// Producer presentation timestamp, nanoseconds.
    pub pts: i64,
}

/// Buffer metadata, as plain data.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RawMeta {
    /// `SPA_META_Header`, when present.
    pub header: Option<HeaderMeta>,
    /// `SPA_META_VideoCrop` as `(x, y, width, height)`, when present.
    pub crop: Option<(i32, i32, u32, u32)>,
    /// `SPA_META_VideoDamage` regions in order, when the meta is present.
    /// Iteration stops at the first empty region, as SPA defines.
    pub damage: Option<Vec<(i32, i32, u32, u32)>>,
    /// `SPA_META_Cursor`, parsed while the buffer was dequeued, when present.
    pub cursor: Option<Result<CursorMeta, CursorMetaError>>,
}

/// One dequeued buffer, as plain data.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RawBuffer {
    /// Planes in `spa_buffer.datas` order.
    pub planes: Vec<RawPlane>,
    /// Metadata.
    pub meta: RawMeta,
}

/// Per-epoch facts the classifier needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayoutContext {
    /// Negotiated stream format.
    pub negotiated: NegotiatedFormat,
    /// Output the stream serves.
    pub output: OutputIdentity,
    /// Output generation.
    pub generation: RegionGeneration,
    /// GPU the producer renders on, when known.
    pub source_gpu: Option<GpuIdentity>,
    /// Copy path for host shared memory (always [`CopyPath::HostCopy`]).
    pub host_copy_path: CopyPath,
    /// Copy path the DMA-BUF importer would take, if DMA-BUF is accepted.
    pub dmabuf_copy_path: Option<CopyPath>,
}

/// Why a non-empty buffer is released without being read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BufferRefusal {
    /// Planes disagree with the format.
    PlaneCount,
    /// Mixed or unknown memory kinds.
    MemoryKind,
    /// DMA-BUF arrived though none was accepted.
    DmaBufNotAccepted,
    /// A host plane was not mapped into this process.
    NotMapped,
    /// A plane is empty while another is not.
    PartialChunk,
    /// Bottom-up or zero stride.
    Stride,
    /// A DMA-BUF plane carries no file descriptor.
    MissingFd,
    /// The cursor metadata is malformed.
    Cursor(CursorMetaError),
}

impl BufferRefusal {
    /// Stable token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::PlaneCount => "plane_count",
            Self::MemoryKind => "memory_kind",
            Self::DmaBufNotAccepted => "dmabuf_not_accepted",
            Self::NotMapped => "not_mapped",
            Self::PartialChunk => "partial_chunk",
            Self::Stride => "stride",
            Self::MissingFd => "missing_fd",
            Self::Cursor(_) => "cursor_meta",
        }
    }
}

/// What a dequeued buffer is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Classified {
    /// A frame to validate and lease.
    Frame {
        /// Unvalidated description; `validate` decides.
        surface: Box<CaptureSurface>,
        /// Cursor metadata that rode with the frame.
        cursor: Option<CursorMeta>,
    },
    /// A size-0 buffer: cursor metadata only, no pixels.
    CursorOnly(Option<CursorMeta>),
    /// The producer marked the buffer corrupted.
    Corrupted,
    /// Released unread.
    Refused(BufferRefusal),
}

/// Classifies one buffer. `sequence` is capenc's own strictly increasing
/// frame counter and `capture_ns` the monotonic arrival time; a producer
/// header sequence is not trusted for ordering.
#[must_use]
pub fn classify(
    raw: &RawBuffer,
    context: &LayoutContext,
    sequence: u64,
    capture_ns: u64,
) -> Classified {
    if raw.meta.header.is_some_and(|header| header.corrupted)
        || raw.planes.iter().any(|plane| plane.corrupted)
    {
        return Classified::Corrupted;
    }
    let cursor = match &raw.meta.cursor {
        None => None,
        Some(Ok(meta)) => Some(meta.clone()),
        Some(Err(error)) => return Classified::Refused(BufferRefusal::Cursor(*error)),
    };
    if raw.planes.iter().all(|plane| plane.chunk_size == 0) {
        return Classified::CursorOnly(cursor);
    }
    match surface_for(raw, context, sequence, capture_ns, cursor.is_some()) {
        Ok(surface) => Classified::Frame {
            surface: Box::new(surface),
            cursor,
        },
        Err(refusal) => Classified::Refused(refusal),
    }
}

fn surface_for(
    raw: &RawBuffer,
    context: &LayoutContext,
    sequence: u64,
    capture_ns: u64,
    cursor_meta: bool,
) -> Result<CaptureSurface, BufferRefusal> {
    let negotiated = context.negotiated;
    if raw.planes.len() != negotiated.format.plane_count() {
        return Err(BufferRefusal::PlaneCount);
    }
    if raw.planes.iter().any(|plane| plane.chunk_size == 0) {
        return Err(BufferRefusal::PartialChunk);
    }
    let (memory, copy_path) = memory_of(raw, context)?;
    let mut planes = Vec::with_capacity(raw.planes.len());
    let mut buffer_sizes: Vec<Option<u64>> = Vec::with_capacity(raw.planes.len());
    let mut buffer_fds: Vec<Option<i64>> = Vec::with_capacity(raw.planes.len());
    for plane in &raw.planes {
        let stride = u32::try_from(plane.chunk_stride)
            .ok()
            .filter(|stride| *stride > 0)
            .ok_or(BufferRefusal::Stride)?;
        // DMA-BUF planes that share a file descriptor share one buffer
        // object, so their extents are checked for overlap together.
        let shared = matches!(plane.kind, DataKind::DmaBuf)
            .then(|| {
                buffer_fds
                    .iter()
                    .position(|fd| *fd == plane.fd && fd.is_some())
            })
            .flatten();
        let buffer = match shared {
            Some(index) => {
                let size = &mut buffer_sizes[index];
                *size = Some(size.unwrap_or(0).max(u64::from(plane.max_size)));
                index
            }
            None => {
                buffer_sizes.push(Some(u64::from(plane.max_size)));
                buffer_fds.push(plane.fd);
                buffer_sizes.len() - 1
            }
        };
        planes.push(PlaneLayout {
            buffer: u8::try_from(buffer).map_err(|_| BufferRefusal::PlaneCount)?,
            offset: u64::from(plane.chunk_offset),
            stride,
            size: u64::from(plane.chunk_size),
        });
    }
    Ok(CaptureSurface {
        output: context.output.clone(),
        generation: context.generation,
        backend: CaptureBackend::PipeWire,
        format: negotiated.format,
        geometry: geometry(negotiated, raw.meta.crop),
        planes,
        buffer_sizes,
        memory,
        colour: negotiated.colour,
        timestamps: CaptureTimestamps {
            sequence,
            capture_monotonic_ns: capture_ns,
            presentation_ns: raw
                .meta
                .header
                .and_then(|header| u64::try_from(header.pts).ok()),
        },
        damage: damage(raw.meta.damage.as_deref()),
        cursor: if cursor_meta {
            CursorEvidence::SeparateMetadata
        } else {
            CursorEvidence::Unknown
        },
        gpu: context.source_gpu,
        copy_path,
    })
}

fn memory_of(
    raw: &RawBuffer,
    context: &LayoutContext,
) -> Result<(CaptureMemory, CopyPath), BufferRefusal> {
    let host = raw
        .planes
        .iter()
        .all(|plane| matches!(plane.kind, DataKind::MemFd | DataKind::MemPtr));
    if host {
        if raw.planes.iter().any(|plane| !plane.mapped) {
            return Err(BufferRefusal::NotMapped);
        }
        return Ok((CaptureMemory::HostShared, context.host_copy_path));
    }
    if raw
        .planes
        .iter()
        .all(|plane| matches!(plane.kind, DataKind::DmaBuf))
    {
        let path = context
            .dmabuf_copy_path
            .ok_or(BufferRefusal::DmaBufNotAccepted)?;
        if raw.planes.iter().any(|plane| plane.fd.is_none()) {
            return Err(BufferRefusal::MissingFd);
        }
        let modifier = context
            .negotiated
            .modifier
            .unwrap_or(FormatModifier::IMPLICIT);
        return Ok((CaptureMemory::DmaBuf { modifier }, path));
    }
    Err(BufferRefusal::MemoryKind)
}

fn geometry(negotiated: NegotiatedFormat, crop: Option<(i32, i32, u32, u32)>) -> CaptureGeometry {
    let full = PixelRect {
        x: 0,
        y: 0,
        width: negotiated.width as usize,
        height: negotiated.height as usize,
    };
    // An empty crop means "no crop"; an out-of-range one is left for
    // validation to refuse rather than silently clamped.
    let visible = match crop {
        Some((x, y, width, height)) if width > 0 && height > 0 => {
            match (usize::try_from(x), usize::try_from(y)) {
                (Ok(x), Ok(y)) => PixelRect {
                    x,
                    y,
                    width: width as usize,
                    height: height as usize,
                },
                _ => PixelRect {
                    x: usize::MAX,
                    y: usize::MAX,
                    width: width as usize,
                    height: height as usize,
                },
            }
        }
        _ => full,
    };
    CaptureGeometry {
        coded_width: negotiated.width,
        coded_height: negotiated.height,
        visible,
    }
}

fn damage(regions: Option<&[(i32, i32, u32, u32)]>) -> DamageEvidence {
    let Some(regions) = regions else {
        return DamageEvidence::Unknown;
    };
    let mut rects = Vec::new();
    for &(x, y, width, height) in regions {
        if width == 0 || height == 0 {
            break;
        }
        let (Ok(x), Ok(y)) = (usize::try_from(x), usize::try_from(y)) else {
            return DamageEvidence::Unknown;
        };
        if rects.len() == DAMAGE_META_MAX_REGIONS {
            return DamageEvidence::Unknown;
        }
        rects.push(PixelRect {
            x,
            y,
            width: width as usize,
            height: height as usize,
        });
    }
    // A damage meta that names nothing cannot be told apart from a producer
    // that did not fill it in; missing damage is full damage.
    if rects.is_empty() {
        DamageEvidence::Unknown
    } else {
        DamageEvidence::Regions(rects)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use arcen_media::video::{
        CaptureImporter, CapturePixelFormat, CaptureSurfaceError, PciAddress, SourceColour,
    };

    use super::*;
    use crate::wayland::cursor::{encode_cursor_meta, parse_cursor_meta};

    pub(crate) fn negotiated(width: u32, height: u32) -> NegotiatedFormat {
        NegotiatedFormat {
            format: CapturePixelFormat::Bgrx8,
            width,
            height,
            modifier: None,
            colour: SourceColour::UNKNOWN,
            framerate: (0, 1),
            max_framerate: (60, 1),
        }
    }

    pub(crate) fn context(width: u32, height: u32) -> LayoutContext {
        LayoutContext {
            negotiated: negotiated(width, height),
            output: OutputIdentity::new("Meta-0").unwrap(),
            generation: RegionGeneration::new(1).unwrap(),
            source_gpu: None,
            host_copy_path: CopyPath::HostCopy,
            dmabuf_copy_path: None,
        }
    }

    pub(crate) fn importer(context: &LayoutContext) -> CaptureImporter {
        CaptureImporter {
            output: context.output.clone(),
            generation: context.generation,
            colour: context.negotiated.colour,
            encoder_gpu: None,
            gpu_modifiers: Vec::new(),
            last_sequence: None,
        }
    }

    pub(crate) fn memfd(stride: u32, height: u32) -> RawPlane {
        RawPlane {
            kind: DataKind::MemFd,
            fd: Some(42),
            mapped: true,
            max_size: stride * height,
            chunk_offset: 0,
            chunk_size: stride * height,
            chunk_stride: i32::try_from(stride).unwrap(),
            corrupted: false,
        }
    }

    pub(crate) fn host_buffer(stride: u32, height: u32) -> RawBuffer {
        RawBuffer {
            planes: vec![memfd(stride, height)],
            meta: RawMeta {
                crop: Some((0, 0, 0, 0)),
                cursor: Some(parse_cursor_meta(&encode_cursor_meta(
                    1,
                    (300, 300),
                    (0, 0),
                    None,
                ))),
                ..RawMeta::default()
            },
        }
    }

    fn frame(classified: Classified) -> CaptureSurface {
        match classified {
            Classified::Frame { surface, .. } => *surface,
            other => panic!("expected a frame, got {other:?}"),
        }
    }

    #[test]
    fn mutter_40_shm_bgrx_frame_is_host_copy_with_unknown_damage() {
        let context = context(1920, 1080);
        let surface = frame(classify(&host_buffer(7680, 1080), &context, 1, 5));
        assert_eq!(surface.memory, CaptureMemory::HostShared);
        assert_eq!(surface.copy_path, CopyPath::HostCopy);
        assert_eq!(surface.damage, DamageEvidence::Unknown);
        assert_eq!(surface.cursor, CursorEvidence::SeparateMetadata);
        assert_eq!(surface.planes[0].stride, 7680);
        let validated = surface.validate(&importer(&context)).unwrap();
        assert!(validated.damage().is_full());
    }

    #[test]
    fn padded_stride_and_odd_size_validate() {
        // 1366x767 with rows padded to 64 bytes.
        let context = context(1366, 767);
        let stride = (1366 * 4u32).div_ceil(64) * 64;
        let surface = frame(classify(&host_buffer(stride, 767), &context, 1, 5));
        assert_eq!(surface.planes[0].stride, stride);
        surface.validate(&importer(&context)).unwrap();
    }

    #[test]
    fn last_row_without_padding_is_enough() {
        let context = context(3, 2);
        let mut buffer = host_buffer(16, 2);
        buffer.planes[0].chunk_size = 16 + 12;
        buffer.planes[0].max_size = 16 + 12;
        frame(classify(&buffer, &context, 1, 0))
            .validate(&importer(&context))
            .unwrap();
    }

    #[test]
    fn short_stride_and_truncated_plane_are_refused_by_validation() {
        let context = context(1920, 1080);
        let short = frame(classify(&host_buffer(1920 * 4 - 4, 1080), &context, 1, 0));
        assert_eq!(
            short.validate(&importer(&context)),
            Err(CaptureSurfaceError::StrideTooSmall { plane: 0 })
        );
        let mut truncated = host_buffer(7680, 1080);
        truncated.planes[0].chunk_size -= 1;
        assert_eq!(
            frame(classify(&truncated, &context, 1, 0)).validate(&importer(&context)),
            Err(CaptureSurfaceError::PlaneTruncated { plane: 0 })
        );
        let mut outside = host_buffer(7680, 1080);
        outside.planes[0].chunk_offset = 4;
        assert_eq!(
            frame(classify(&outside, &context, 1, 0)).validate(&importer(&context)),
            Err(CaptureSurfaceError::PlaneOutsideBuffer { plane: 0 })
        );
    }

    #[test]
    fn size_zero_buffer_is_cursor_only() {
        let context = context(1920, 1080);
        let mut buffer = host_buffer(7680, 1080);
        buffer.planes[0].chunk_size = 0;
        buffer.planes[0].chunk_stride = 0;
        match classify(&buffer, &context, 1, 0) {
            Classified::CursorOnly(Some(meta)) => assert_eq!(meta.position, (300, 300)),
            other => panic!("{other:?}"),
        }
        buffer.meta.cursor = None;
        assert_eq!(
            classify(&buffer, &context, 1, 0),
            Classified::CursorOnly(None)
        );
    }

    #[test]
    fn corrupted_and_unusable_buffers_are_released_unread() {
        let context = context(64, 64);
        let mut corrupted = host_buffer(256, 64);
        corrupted.planes[0].corrupted = true;
        assert_eq!(classify(&corrupted, &context, 1, 0), Classified::Corrupted);
        let mut header = host_buffer(256, 64);
        header.meta.header = Some(HeaderMeta {
            corrupted: true,
            seq: 1,
            pts: 0,
        });
        assert_eq!(classify(&header, &context, 1, 0), Classified::Corrupted);
        let mut unmapped = host_buffer(256, 64);
        unmapped.planes[0].mapped = false;
        assert_eq!(
            classify(&unmapped, &context, 1, 0),
            Classified::Refused(BufferRefusal::NotMapped)
        );
        let mut bottom_up = host_buffer(256, 64);
        bottom_up.planes[0].chunk_stride = -256;
        assert_eq!(
            classify(&bottom_up, &context, 1, 0),
            Classified::Refused(BufferRefusal::Stride)
        );
        let mut dmabuf = host_buffer(256, 64);
        dmabuf.planes[0].kind = DataKind::DmaBuf;
        assert_eq!(
            classify(&dmabuf, &context, 1, 0),
            Classified::Refused(BufferRefusal::DmaBufNotAccepted)
        );
        let mut two = host_buffer(256, 64);
        two.planes.push(memfd(256, 64));
        assert_eq!(
            classify(&two, &context, 1, 0),
            Classified::Refused(BufferRefusal::PlaneCount)
        );
        let mut bad_cursor = host_buffer(256, 64);
        bad_cursor.meta.cursor = Some(parse_cursor_meta(&[0; 4]));
        assert_eq!(
            classify(&bad_cursor, &context, 1, 0),
            Classified::Refused(BufferRefusal::Cursor(CursorMetaError::Truncated))
        );
    }

    #[test]
    fn damage_meta_becomes_regions_or_unknown() {
        let context = context(64, 64);
        let mut buffer = host_buffer(256, 64);
        buffer.meta.damage = Some(vec![
            (0, 0, 16, 16),
            (32, 32, 8, 8),
            (0, 0, 0, 0),
            (1, 1, 1, 1),
        ]);
        let surface = frame(classify(&buffer, &context, 1, 0));
        assert_eq!(
            surface.damage,
            DamageEvidence::Regions(vec![
                PixelRect {
                    x: 0,
                    y: 0,
                    width: 16,
                    height: 16
                },
                PixelRect {
                    x: 32,
                    y: 32,
                    width: 8,
                    height: 8
                },
            ])
        );
        buffer.meta.damage = Some(vec![(0, 0, 0, 0)]);
        assert_eq!(
            frame(classify(&buffer, &context, 1, 0)).damage,
            DamageEvidence::Unknown
        );
        buffer.meta.damage = Some(vec![(-1, 0, 4, 4)]);
        assert_eq!(
            frame(classify(&buffer, &context, 1, 0)).damage,
            DamageEvidence::Unknown
        );
        buffer.meta.damage = Some(vec![(0, 0, 1, 1); DAMAGE_META_MAX_REGIONS + 1]);
        assert_eq!(
            frame(classify(&buffer, &context, 1, 0)).damage,
            DamageEvidence::Unknown
        );
        // Out-of-bounds regions survive classification; validation widens
        // them to full damage.
        buffer.meta.damage = Some(vec![(60, 60, 8, 8)]);
        let validated = frame(classify(&buffer, &context, 1, 0))
            .validate(&importer(&context))
            .unwrap();
        assert_eq!(validated.damage(), &DamageEvidence::Unknown);
    }

    #[test]
    fn crop_meta_sets_visible_rect_and_bad_crop_is_refused() {
        let context = context(64, 64);
        let mut buffer = host_buffer(256, 64);
        buffer.meta.crop = Some((8, 8, 32, 16));
        let surface = frame(classify(&buffer, &context, 1, 0));
        assert_eq!(
            surface.geometry.visible,
            PixelRect {
                x: 8,
                y: 8,
                width: 32,
                height: 16
            }
        );
        buffer.meta.crop = Some((-1, 0, 8, 8));
        assert_eq!(
            frame(classify(&buffer, &context, 1, 0)).validate(&importer(&context)),
            Err(CaptureSurfaceError::VisibleOutsideCoded)
        );
    }

    #[test]
    fn dmabuf_planes_need_proof_and_share_fd_extents() {
        let gpu = GpuIdentity {
            pci: Some(PciAddress::new(0, 0, 0x10, 0).unwrap()),
            render_node_minor: Some(128),
            vendor_id: Some(0x10de),
        };
        let tiled = FormatModifier(0x0300_0000_004f_e010);
        let mut context = context(64, 64);
        context.negotiated.modifier = Some(tiled);
        context.source_gpu = Some(gpu);
        context.dmabuf_copy_path = Some(CopyPath::GpuConvert);
        let mut buffer = host_buffer(256, 64);
        buffer.planes[0].kind = DataKind::DmaBuf;
        buffer.planes[0].mapped = false;
        let surface = frame(classify(&buffer, &context, 1, 0));
        assert_eq!(surface.memory, CaptureMemory::DmaBuf { modifier: tiled });
        // No proven modifier, and no proven encoder GPU: refused.
        let mut proven = importer(&context);
        assert_eq!(
            surface.clone().validate(&proven),
            Err(CaptureSurfaceError::GpuIdentityUnproven)
        );
        proven.encoder_gpu = Some(gpu);
        assert!(matches!(
            surface.clone().validate(&proven),
            Err(CaptureSurfaceError::UnsupportedModifier(_))
        ));
        proven.gpu_modifiers = vec![tiled];
        surface.validate(&proven).unwrap();

        // A different GPU is never a same-GPU path.
        proven.encoder_gpu = Some(GpuIdentity {
            pci: Some(PciAddress::new(0, 0x41, 0, 0).unwrap()),
            ..gpu
        });
        assert_eq!(
            frame(classify(&buffer, &context, 1, 0)).validate(&proven),
            Err(CaptureSurfaceError::GpuMismatch)
        );

        let mut missing_fd = buffer.clone();
        missing_fd.planes[0].fd = None;
        assert_eq!(
            classify(&missing_fd, &context, 1, 0),
            Classified::Refused(BufferRefusal::MissingFd)
        );

        // NV12 in one DMA-BUF: both planes share buffer object 0.
        context.negotiated.format = CapturePixelFormat::Nv12;
        let luma = RawPlane {
            kind: DataKind::DmaBuf,
            fd: Some(9),
            mapped: false,
            max_size: 64 * 96,
            chunk_offset: 0,
            chunk_size: 64 * 64,
            chunk_stride: 64,
            corrupted: false,
        };
        let chroma = RawPlane {
            chunk_offset: 64 * 64,
            chunk_size: 64 * 32,
            ..luma
        };
        let nv12 = RawBuffer {
            planes: vec![luma, chroma],
            meta: RawMeta::default(),
        };
        let surface = frame(classify(&nv12, &context, 1, 0));
        assert_eq!(surface.buffer_sizes, vec![Some(64 * 96)]);
        assert_eq!(surface.planes[1].buffer, 0);
        proven.encoder_gpu = Some(gpu);
        surface.validate(&proven).unwrap();
        let overlapping = RawBuffer {
            planes: vec![
                luma,
                RawPlane {
                    chunk_offset: 64 * 63,
                    ..chroma
                },
            ],
            meta: RawMeta::default(),
        };
        assert_eq!(
            frame(classify(&overlapping, &context, 1, 0)).validate(&proven),
            Err(CaptureSurfaceError::PlaneOverlap)
        );
    }

    #[test]
    fn implicit_modifier_is_never_importable() {
        let gpu = GpuIdentity {
            render_node_minor: Some(128),
            ..GpuIdentity::default()
        };
        let mut context = context(16, 16);
        context.source_gpu = Some(gpu);
        context.dmabuf_copy_path = Some(CopyPath::GpuConvert);
        let mut buffer = host_buffer(64, 16);
        buffer.planes[0].kind = DataKind::DmaBuf;
        let surface = frame(classify(&buffer, &context, 1, 0));
        assert_eq!(
            surface.memory,
            CaptureMemory::DmaBuf {
                modifier: FormatModifier::IMPLICIT
            }
        );
        let mut proven = importer(&context);
        proven.encoder_gpu = Some(gpu);
        proven.gpu_modifiers = vec![FormatModifier::IMPLICIT];
        assert!(matches!(
            surface.validate(&proven),
            Err(CaptureSurfaceError::UnsupportedModifier(_))
        ));
    }
}
