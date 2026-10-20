//! Pure capture-surface and source evidence.
//!
//! A native capture adapter (PipeWire/DMA-BUF today, VA-API or Vulkan later)
//! describes each delivered buffer with a [`CaptureSurface`] and validates it
//! against what its importer and encoder actually proved
//! ([`CaptureImporter`]) before any conversion or encode reads it. The shared
//! type carries facts as data only: no file descriptors, fences, DRM, EGL,
//! CUDA or compositor handles cross into this crate.
//!
//! Every fact distinguishes authoritative evidence from inference and
//! absence ([`SourceFact`]). Unknown never satisfies a requirement: an
//! implicit DRM modifier is not "linear", an unreported GPU is not "the same
//! GPU", and missing damage is full damage.
//!
//! Some producers deliver no damage at all (GNOME 40 Mutter `ScreenCast`):
//! that is [`DamageEvidence::Unknown`], which Keel treats as full damage.
//! A cursor-only update (a size-0 buffer carrying cursor metadata) is not a
//! frame: the adapter routes the cursor metadata and never builds a
//! `CaptureSurface` for it; if it did, validation refuses the empty geometry.
//!
//! Copy cost is reported per delivered path ([`CopyPath`]), independently of
//! [`CaptureBackend::zero_copy`], which keeps describing the backend family as
//! it always has.

use std::fmt::{Display, Formatter};

use arcen_keel::{DamageMetadataStatus, PixelRect};

use super::{CaptureBackend, ConversionBackend, PackedRgb10Layout};
use crate::{ColorMatrix, ColorPrimaries, ColorRange, OutputIdentity, RegionGeneration};

/// Most planes any supported capture format uses.
pub const MAX_CAPTURE_PLANES: usize = 4;
/// Most damage rectangles retained per frame; more means "full".
pub const MAX_CAPTURE_DAMAGE_RECTS: usize = 256;

/// How strongly a native fact is known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceFact<T> {
    /// Reported by the producer through a documented interface.
    Authoritative(T),
    /// Derived by Arcen (a default, a format name, a heuristic). Never
    /// satisfies a requirement that needs proof.
    Inferred(T),
    /// Not available.
    Unknown,
}

impl<T: Copy> SourceFact<T> {
    /// The value only when authoritative.
    #[must_use]
    pub const fn authoritative(&self) -> Option<T> {
        match self {
            Self::Authoritative(value) => Some(*value),
            Self::Inferred(_) | Self::Unknown => None,
        }
    }

    /// The value, authoritative or inferred.
    #[must_use]
    pub const fn value(&self) -> Option<T> {
        match self {
            Self::Authoritative(value) | Self::Inferred(value) => Some(*value),
            Self::Unknown => None,
        }
    }

    /// Stable evidence-strength token.
    #[must_use]
    pub const fn strength_token(&self) -> &'static str {
        match self {
            Self::Authoritative(_) => "authoritative",
            Self::Inferred(_) => "inferred",
            Self::Unknown => "unknown",
        }
    }
}

/// Sample precision class of a capture format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SourcePrecision {
    /// Eight bits per component.
    Eight,
    /// Ten bits per component (packed RGB10).
    Ten,
    /// IEEE half-float per component.
    HalfFloat,
}

impl SourcePrecision {
    /// Whether the format can carry more than eight bits per component.
    #[must_use]
    pub const fn is_wide(self) -> bool {
        !matches!(self, Self::Eight)
    }
}

/// Pixel formats a capture provider may deliver. Names follow memory order
/// conventions of the DRM fourcc family; this is a description, not a binding
/// to DRM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapturePixelFormat {
    /// B, G, R, A bytes.
    Bgra8,
    /// B, G, R, unused bytes.
    Bgrx8,
    /// R, G, B, A bytes.
    Rgba8,
    /// R, G, B, unused bytes.
    Rgbx8,
    /// Two-plane 8-bit Y + interleaved `CbCr`, 4:2:0.
    Nv12,
    /// Packed little-endian 32-bit `XRGB2101010`.
    Xrgb2101010,
    /// Packed little-endian 32-bit `XBGR2101010`.
    Xbgr2101010,
    /// Four half-float components, R, G, B, A.
    RgbaF16,
}

impl CapturePixelFormat {
    /// Every format.
    pub const ALL: &'static [Self] = &[
        Self::Bgra8,
        Self::Bgrx8,
        Self::Rgba8,
        Self::Rgbx8,
        Self::Nv12,
        Self::Xrgb2101010,
        Self::Xbgr2101010,
        Self::RgbaF16,
    ];

    /// Stable token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Bgra8 => "bgra8",
            Self::Bgrx8 => "bgrx8",
            Self::Rgba8 => "rgba8",
            Self::Rgbx8 => "rgbx8",
            Self::Nv12 => "nv12",
            Self::Xrgb2101010 => "xrgb2101010",
            Self::Xbgr2101010 => "xbgr2101010",
            Self::RgbaF16 => "rgba-f16",
        }
    }

    /// Parses a stable token.
    #[must_use]
    pub fn from_token(value: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|format| format.token() == value)
    }

    /// Sample precision.
    #[must_use]
    pub const fn precision(self) -> SourcePrecision {
        match self {
            Self::Bgra8 | Self::Bgrx8 | Self::Rgba8 | Self::Rgbx8 | Self::Nv12 => {
                SourcePrecision::Eight
            }
            Self::Xrgb2101010 | Self::Xbgr2101010 => SourcePrecision::Ten,
            Self::RgbaF16 => SourcePrecision::HalfFloat,
        }
    }

    /// Whether samples are YCbCr rather than RGB.
    #[must_use]
    pub const fn is_yuv(self) -> bool {
        matches!(self, Self::Nv12)
    }

    /// Number of planes.
    #[must_use]
    pub const fn plane_count(self) -> usize {
        match self {
            Self::Nv12 => 2,
            _ => 1,
        }
    }

    /// The shared packed-RGB10 layout, for ten-bit formats.
    #[must_use]
    pub const fn packed_rgb10_layout(self) -> Option<PackedRgb10Layout> {
        match self {
            Self::Xrgb2101010 => Some(PackedRgb10Layout::XRGB2101010),
            Self::Xbgr2101010 => Some(PackedRgb10Layout::XBGR2101010),
            _ => None,
        }
    }

    /// Minimum bytes in one row of `plane` for `width` pixels, unpadded.
    #[must_use]
    pub fn min_row_bytes(self, plane: usize, width: u32) -> Option<u64> {
        let width = u64::from(width);
        match (self, plane) {
            (Self::Nv12, 0) => Some(width),
            (Self::Nv12, 1) => width.div_ceil(2).checked_mul(2),
            (Self::RgbaF16, 0) => width.checked_mul(8),
            (_, 0) => width.checked_mul(4),
            _ => None,
        }
    }

    /// Rows in `plane` for `height` pixels.
    #[must_use]
    pub const fn plane_rows(self, plane: usize, height: u32) -> u32 {
        match (self, plane) {
            (Self::Nv12, 1) => height.div_ceil(2),
            _ => height,
        }
    }
}

/// A DRM-style 64-bit format modifier, carried as opaque data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FormatModifier(pub u64);

impl FormatModifier {
    /// Linear, row-major layout (`DRM_FORMAT_MOD_LINEAR`).
    pub const LINEAR: Self = Self(0);
    /// No explicit modifier was supplied (`DRM_FORMAT_MOD_INVALID`): the
    /// layout is implicit and unknown to Arcen.
    pub const IMPLICIT: Self = Self(0x00ff_ffff_ffff_ffff);

    /// Classification.
    #[must_use]
    pub const fn class(self) -> ModifierClass {
        match self.0 {
            0 => ModifierClass::Linear,
            0x00ff_ffff_ffff_ffff => ModifierClass::Implicit,
            _ => ModifierClass::Vendor,
        }
    }
}

/// What a modifier tells a consumer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModifierClass {
    /// Row-major; a CPU may read it with the stated strides.
    Linear,
    /// Tiled or compressed vendor layout; only an importer that proved this
    /// exact modifier may read it.
    Vendor,
    /// Unknown layout. Never satisfies an import requirement.
    Implicit,
}

impl ModifierClass {
    /// Stable token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Linear => "linear",
            Self::Vendor => "vendor",
            Self::Implicit => "implicit",
        }
    }
}

/// Where a delivered buffer lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureMemory {
    /// A DMA-BUF with an explicit or implicit modifier.
    DmaBuf {
        /// Modifier shared by every plane.
        modifier: FormatModifier,
    },
    /// Host shared memory (for example `PipeWire` `MemFd`/`MemPtr`, `XShm`).
    HostShared,
    /// Device memory already owned by the capture API on a GPU.
    GpuDevice,
}

impl CaptureMemory {
    /// Stable token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::DmaBuf { .. } => "dmabuf",
            Self::HostShared => "host-shared",
            Self::GpuDevice => "gpu-device",
        }
    }
}

/// One plane's layout within the delivered buffer objects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaneLayout {
    /// Index of the buffer object (DMA-BUF fd or shared-memory chunk) that
    /// holds the plane.
    pub buffer: u8,
    /// Byte offset of the first row.
    pub offset: u64,
    /// Bytes between row starts. May exceed the row's pixel bytes (padding).
    pub stride: u32,
    /// Bytes the producer declares for this plane.
    pub size: u64,
}

/// Coded and visible geometry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureGeometry {
    /// Allocated width in pixels.
    pub coded_width: u32,
    /// Allocated height in pixels.
    pub coded_height: u32,
    /// Picture area within the coded size.
    pub visible: PixelRect,
}

/// Source transfer function as delivered, which is wider than the coded
/// [`crate::TransferCharacteristics`] vocabulary: a capture buffer may hold
/// linear light that no coded stream carries directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceTransfer {
    /// IEC 61966-2-1 sRGB.
    Srgb,
    /// ITU-R BT.709.
    Bt709,
    /// SMPTE ST 2084 (PQ).
    Pq,
    /// ARIB STD-B67 (HLG).
    Hlg,
    /// Extended linear light (scRGB-style), 1.0 = SDR white.
    LinearExtended,
}

impl SourceTransfer {
    /// Stable token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Srgb => "srgb",
            Self::Bt709 => "bt709",
            Self::Pq => "pq",
            Self::Hlg => "hlg",
            Self::LinearExtended => "linear-extended",
        }
    }

    /// Whether this is an SDR display transfer.
    #[must_use]
    pub const fn is_sdr(self) -> bool {
        matches!(self, Self::Srgb | Self::Bt709)
    }
}

/// Whether wide samples carry genuine wide content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SamplePrecision {
    /// The compositor composed at the delivered precision.
    NativeWide,
    /// Eight-bit content placed in a wider container.
    UpconvertedEightBit,
}

/// Colour facts of a capture source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceColour {
    /// Colour primaries.
    pub primaries: SourceFact<ColorPrimaries>,
    /// Transfer function.
    pub transfer: SourceFact<SourceTransfer>,
    /// Sample range; meaningful for YCbCr formats.
    pub range: SourceFact<ColorRange>,
    /// Matrix; meaningful for YCbCr formats.
    pub matrix: SourceFact<ColorMatrix>,
    /// Where SDR white sits, in nits, for linear and PQ sources.
    pub sdr_white_nits: SourceFact<u16>,
}

impl SourceColour {
    /// Nothing reported.
    pub const UNKNOWN: Self = Self {
        primaries: SourceFact::Unknown,
        transfer: SourceFact::Unknown,
        range: SourceFact::Unknown,
        matrix: SourceFact::Unknown,
        sdr_white_nits: SourceFact::Unknown,
    };
}

/// Capture timing facts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureTimestamps {
    /// Producer frame sequence; strictly increasing within a generation.
    pub sequence: u64,
    /// Monotonic capture time, nanoseconds.
    pub capture_monotonic_ns: u64,
    /// Compositor presentation time, when reported.
    pub presentation_ns: Option<u64>,
}

/// Damage reported with a frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DamageEvidence {
    /// No damage metadata: treat as full damage.
    Unknown,
    /// The producer reported the whole frame changed.
    Full,
    /// The producer reported exactly these rectangles changed (possibly none).
    Regions(Vec<PixelRect>),
}

impl DamageEvidence {
    /// Keel metadata verdict: anything but an authoritative region list is
    /// [`DamageMetadataStatus::Unknown`], which Keel marks as full.
    #[must_use]
    pub const fn metadata_status(&self) -> DamageMetadataStatus {
        match self {
            Self::Regions(_) => DamageMetadataStatus::Complete,
            Self::Unknown | Self::Full => DamageMetadataStatus::Unknown,
        }
    }

    /// Whether the frame must be treated as entirely changed.
    #[must_use]
    pub const fn is_full(&self) -> bool {
        !matches!(self, Self::Regions(_))
    }
}

/// How the pointer is represented.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorEvidence {
    /// Cursor image/position delivered as separate metadata; local cursor
    /// authority is possible.
    SeparateMetadata,
    /// Cursor drawn into the frame by the producer.
    Composited,
    /// Cursor neither drawn nor reported.
    Absent,
    /// Not reported.
    Unknown,
}

impl CursorEvidence {
    /// Whether the cursor is in the video pixels; `None` when unknown.
    #[must_use]
    pub const fn in_video(self) -> Option<bool> {
        match self {
            Self::Composited => Some(true),
            Self::SeparateMetadata | Self::Absent => Some(false),
            Self::Unknown => None,
        }
    }

    /// The cursor authority this evidence requires: the host's when the
    /// cursor is in the pixels, the Deck's when it is reported separately or
    /// not at all (the Deck then draws its own pointer without the host's
    /// shape), and none when the source states nothing, which a pipeline
    /// must refuse rather than guess.
    #[must_use]
    pub const fn authority(self) -> Option<arcen_protocol::messages::CursorMode> {
        match self {
            Self::Composited => Some(arcen_protocol::messages::CursorMode::Host),
            Self::SeparateMetadata | Self::Absent => {
                Some(arcen_protocol::messages::CursorMode::Local)
            }
            Self::Unknown => None,
        }
    }

    /// Stable token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::SeparateMetadata => "metadata",
            Self::Composited => "composited",
            Self::Absent => "absent",
            Self::Unknown => "unknown",
        }
    }
}

/// PCI address of a GPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PciAddress {
    /// Domain.
    pub domain: u16,
    /// Bus.
    pub bus: u8,
    /// Device (0..32).
    pub device: u8,
    /// Function (0..8).
    pub function: u8,
}

impl PciAddress {
    /// Validated address.
    ///
    /// # Errors
    ///
    /// [`CaptureSurfaceError::InvalidPciAddress`] for an out-of-range device
    /// or function.
    pub const fn new(
        domain: u16,
        bus: u8,
        device: u8,
        function: u8,
    ) -> Result<Self, CaptureSurfaceError> {
        if device >= 32 || function >= 8 {
            return Err(CaptureSurfaceError::InvalidPciAddress);
        }
        Ok(Self {
            domain,
            bus,
            device,
            function,
        })
    }
}

/// Vendor-neutral GPU identity, as data.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct GpuIdentity {
    /// PCI address, the strongest identity.
    pub pci: Option<PciAddress>,
    /// DRM render-node minor, meaningful only within one host.
    pub render_node_minor: Option<u32>,
    /// PCI vendor id.
    pub vendor_id: Option<u16>,
}

/// Whether two GPU identities name the same device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuMatch {
    /// At least one identity key agrees and none disagree.
    Same,
    /// A key disagrees.
    Different,
    /// No common key; never treated as the same device.
    Unknown,
}

impl GpuMatch {
    /// Stable token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Same => "same",
            Self::Different => "different",
            Self::Unknown => "unknown",
        }
    }
}

impl GpuIdentity {
    /// Compares two identities conservatively.
    #[must_use]
    pub fn compare(&self, other: &Self) -> GpuMatch {
        let mut agreed = false;
        let keys = [
            compare_key(self.pci, other.pci),
            compare_key(self.render_node_minor, other.render_node_minor),
        ];
        for key in keys {
            match key {
                Some(true) => agreed = true,
                Some(false) => return GpuMatch::Different,
                None => {}
            }
        }
        if compare_key(self.vendor_id, other.vendor_id) == Some(false) {
            return GpuMatch::Different;
        }
        if agreed {
            GpuMatch::Same
        } else {
            GpuMatch::Unknown
        }
    }
}

fn compare_key<T: PartialEq>(left: Option<T>, right: Option<T>) -> Option<bool> {
    Some(left? == right?)
}

/// Compares optional identities; absence on either side is unknown.
#[must_use]
pub fn compare_gpus(source: Option<&GpuIdentity>, encoder: Option<&GpuIdentity>) -> GpuMatch {
    match (source, encoder) {
        (Some(source), Some(encoder)) => source.compare(encoder),
        _ => GpuMatch::Unknown,
    }
}

/// How pixels travel from the capture buffer to the encoder input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyPath {
    /// The encoder reads the capture buffer through a same-GPU import with
    /// no copy.
    ZeroCopyImport,
    /// A same-GPU blit or conversion pass produces the encoder input.
    GpuConvert,
    /// Pixels pass through host memory: CPU readback, CPU conversion, or a
    /// CPU upload.
    HostCopy,
    /// Pixels move between two GPUs.
    CrossGpu,
}

impl CopyPath {
    /// Stable token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::ZeroCopyImport => "zero-copy-import",
            Self::GpuConvert => "gpu-convert",
            Self::HostCopy => "host-copy",
            Self::CrossGpu => "cross-gpu",
        }
    }

    /// Whether the path has no copy at all.
    #[must_use]
    pub const fn zero_copy(self) -> bool {
        matches!(self, Self::ZeroCopyImport)
    }

    /// Whether pixels cross host memory.
    #[must_use]
    pub const fn host_round_trip(self) -> bool {
        matches!(self, Self::HostCopy)
    }

    /// Whether the path requires the source and encoder GPU to be one device.
    #[must_use]
    pub const fn requires_same_gpu(self) -> bool {
        matches!(self, Self::ZeroCopyImport | Self::GpuConvert)
    }

    /// Where conversion runs, in READY vocabulary.
    #[must_use]
    pub const fn conversion_backend(self) -> ConversionBackend {
        match self {
            Self::HostCopy => ConversionBackend::Cpu,
            Self::ZeroCopyImport | Self::GpuConvert | Self::CrossGpu => ConversionBackend::Gpu,
        }
    }
}

/// One delivered capture buffer, described as data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureSurface {
    /// Output the buffer belongs to.
    pub output: OutputIdentity,
    /// Output/region generation the producer captured under.
    pub generation: RegionGeneration,
    /// Capture API family.
    pub backend: CaptureBackend,
    /// Pixel format.
    pub format: CapturePixelFormat,
    /// Geometry.
    pub geometry: CaptureGeometry,
    /// Plane layouts, in format plane order.
    pub planes: Vec<PlaneLayout>,
    /// Size of each buffer object. `None` is accepted only for
    /// [`CaptureMemory::GpuDevice`], whose importer validates the opaque
    /// allocation itself; DMA-BUF and host-memory planes must be bounded by a
    /// known size.
    pub buffer_sizes: Vec<Option<u64>>,
    /// Memory kind.
    pub memory: CaptureMemory,
    /// Colour the producer attached to this buffer.
    pub colour: SourceColour,
    /// Timing.
    pub timestamps: CaptureTimestamps,
    /// Damage.
    pub damage: DamageEvidence,
    /// Cursor representation.
    pub cursor: CursorEvidence,
    /// GPU the buffer lives on, when known.
    pub gpu: Option<GpuIdentity>,
    /// Path the adapter will use to reach the encoder.
    pub copy_path: CopyPath,
}

/// What the consuming importer and encoder proved, for validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureImporter {
    /// Output this capture epoch serves.
    pub output: OutputIdentity,
    /// Current output/region generation.
    pub generation: RegionGeneration,
    /// Colour admitted for this epoch. A buffer tagged otherwise belongs to
    /// a new epoch and is never relabelled.
    pub colour: SourceColour,
    /// Encoder GPU, when known.
    pub encoder_gpu: Option<GpuIdentity>,
    /// DMA-BUF modifiers the GPU importer proved for this format. Linear is
    /// listed explicitly when supported; [`FormatModifier::IMPLICIT`] is
    /// never honoured.
    pub gpu_modifiers: Vec<FormatModifier>,
    /// Last accepted producer sequence in this generation.
    pub last_sequence: Option<u64>,
}

/// A surface that passed validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedCaptureSurface {
    surface: CaptureSurface,
    damage: DamageEvidence,
    gpu: GpuMatch,
}

impl ValidatedCaptureSurface {
    /// The validated surface.
    #[must_use]
    pub const fn surface(&self) -> &CaptureSurface {
        &self.surface
    }

    /// Damage to apply: the producer's regions, or full when unknown,
    /// excessive or out of bounds.
    #[must_use]
    pub const fn damage(&self) -> &DamageEvidence {
        &self.damage
    }

    /// Source/encoder GPU relation.
    #[must_use]
    pub const fn gpu_match(&self) -> GpuMatch {
        self.gpu
    }

    /// Truthful copy path.
    #[must_use]
    pub const fn copy_path(&self) -> CopyPath {
        self.surface.copy_path
    }
}

/// Why a capture surface cannot be consumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureSurfaceError {
    /// The buffer belongs to another output.
    OutputMismatch,
    /// The buffer was captured under an older generation.
    StaleGeneration {
        /// Generation presented.
        presented: u64,
        /// Current generation.
        current: u64,
    },
    /// The buffer names a generation the consumer has not reached.
    FutureGeneration {
        /// Generation presented.
        presented: u64,
        /// Current generation.
        current: u64,
    },
    /// Zero coded or visible size.
    EmptyGeometry,
    /// Visible rectangle outside the coded size.
    VisibleOutsideCoded,
    /// Plane count differs from the format.
    PlaneCountMismatch {
        /// Planes the format needs.
        expected: usize,
        /// Planes delivered.
        actual: usize,
    },
    /// A plane names a buffer object that was not delivered.
    UnknownBuffer {
        /// Plane index.
        plane: usize,
    },
    /// A row stride is shorter than the row's pixel bytes.
    StrideTooSmall {
        /// Plane index.
        plane: usize,
    },
    /// The plane's declared size cannot hold its rows.
    PlaneTruncated {
        /// Plane index.
        plane: usize,
    },
    /// The buffer object's size is unknown, so the plane cannot be bounded.
    BufferSizeUnknown {
        /// Plane index.
        plane: usize,
    },
    /// The plane extends past its buffer object.
    PlaneOutsideBuffer {
        /// Plane index.
        plane: usize,
    },
    /// Two planes overlap in one buffer object.
    PlaneOverlap,
    /// Layout arithmetic overflowed.
    LayoutOverflow,
    /// The modifier is not one the importer proved.
    UnsupportedModifier(ModifierClass),
    /// Host memory, or a CPU path, cannot read a non-linear buffer.
    NonLinearForCpu(ModifierClass),
    /// The buffer's colour differs from the colour admitted for the epoch.
    ColourChanged,
    /// The copy path contradicts the memory kind or GPU relation.
    CopyPathMismatch,
    /// A same-GPU path without proof that both are one device.
    GpuIdentityUnproven,
    /// A same-GPU path across two devices.
    GpuMismatch,
    /// The producer sequence did not increase.
    NonMonotonicSequence,
    /// A PCI device or function out of range.
    InvalidPciAddress,
}

impl CaptureSurfaceError {
    /// Stable token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::OutputMismatch => "output_mismatch",
            Self::StaleGeneration { .. } => "stale_generation",
            Self::FutureGeneration { .. } => "future_generation",
            Self::EmptyGeometry => "empty_geometry",
            Self::VisibleOutsideCoded => "visible_outside_coded",
            Self::PlaneCountMismatch { .. } => "plane_count_mismatch",
            Self::UnknownBuffer { .. } => "unknown_buffer",
            Self::StrideTooSmall { .. } => "stride_too_small",
            Self::PlaneTruncated { .. } => "plane_truncated",
            Self::BufferSizeUnknown { .. } => "buffer_size_unknown",
            Self::PlaneOutsideBuffer { .. } => "plane_outside_buffer",
            Self::PlaneOverlap => "plane_overlap",
            Self::LayoutOverflow => "layout_overflow",
            Self::UnsupportedModifier(_) => "unsupported_modifier",
            Self::NonLinearForCpu(_) => "non_linear_for_cpu",
            Self::ColourChanged => "colour_changed",
            Self::CopyPathMismatch => "copy_path_mismatch",
            Self::GpuIdentityUnproven => "gpu_identity_unproven",
            Self::GpuMismatch => "gpu_mismatch",
            Self::NonMonotonicSequence => "non_monotonic_sequence",
            Self::InvalidPciAddress => "invalid_pci_address",
        }
    }
}

impl Display for CaptureSurfaceError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "capture surface refused: {}", self.token())
    }
}

impl std::error::Error for CaptureSurfaceError {}

impl CaptureSurface {
    /// Validates the surface against the importer's proven facts. A refused
    /// surface must be released by the adapter, never encoded as stale
    /// pixels.
    ///
    /// # Errors
    ///
    /// The first [`CaptureSurfaceError`] found.
    pub fn validate(
        self,
        importer: &CaptureImporter,
    ) -> Result<ValidatedCaptureSurface, CaptureSurfaceError> {
        self.validate_identity(importer)?;
        self.validate_geometry()?;
        self.validate_planes()?;
        if self.colour != importer.colour {
            return Err(CaptureSurfaceError::ColourChanged);
        }
        let gpu = compare_gpus(self.gpu.as_ref(), importer.encoder_gpu.as_ref());
        self.validate_path(importer, gpu)?;
        if importer
            .last_sequence
            .is_some_and(|last| self.timestamps.sequence <= last)
        {
            return Err(CaptureSurfaceError::NonMonotonicSequence);
        }
        let damage = self.effective_damage();
        Ok(ValidatedCaptureSurface {
            surface: self,
            damage,
            gpu,
        })
    }

    fn validate_identity(&self, importer: &CaptureImporter) -> Result<(), CaptureSurfaceError> {
        if self.output != importer.output {
            return Err(CaptureSurfaceError::OutputMismatch);
        }
        let presented = self.generation.get();
        let current = importer.generation.get();
        if presented < current {
            return Err(CaptureSurfaceError::StaleGeneration { presented, current });
        }
        if presented > current {
            return Err(CaptureSurfaceError::FutureGeneration { presented, current });
        }
        Ok(())
    }

    fn validate_geometry(&self) -> Result<(), CaptureSurfaceError> {
        let geometry = self.geometry;
        let visible = geometry.visible;
        if geometry.coded_width == 0
            || geometry.coded_height == 0
            || visible.width == 0
            || visible.height == 0
        {
            return Err(CaptureSurfaceError::EmptyGeometry);
        }
        if !rect_within(visible, geometry.coded_width, geometry.coded_height) {
            return Err(CaptureSurfaceError::VisibleOutsideCoded);
        }
        Ok(())
    }

    fn validate_planes(&self) -> Result<(), CaptureSurfaceError> {
        let expected = self.format.plane_count();
        if self.planes.len() != expected || self.buffer_sizes.len() > MAX_CAPTURE_PLANES {
            return Err(CaptureSurfaceError::PlaneCountMismatch {
                expected,
                actual: self.planes.len(),
            });
        }
        let mut extents: Vec<(u8, u64, u64)> = Vec::with_capacity(expected);
        for (index, plane) in self.planes.iter().enumerate() {
            let buffer_size = self
                .buffer_sizes
                .get(usize::from(plane.buffer))
                .ok_or(CaptureSurfaceError::UnknownBuffer { plane: index })?;
            let row_bytes = self
                .format
                .min_row_bytes(index, self.geometry.coded_width)
                .ok_or(CaptureSurfaceError::LayoutOverflow)?;
            let stride = u64::from(plane.stride);
            if stride < row_bytes {
                return Err(CaptureSurfaceError::StrideTooSmall { plane: index });
            }
            let rows = u64::from(self.format.plane_rows(index, self.geometry.coded_height));
            let needed = stride
                .checked_mul(rows - 1)
                .and_then(|bytes| bytes.checked_add(row_bytes))
                .ok_or(CaptureSurfaceError::LayoutOverflow)?;
            if plane.size < needed {
                return Err(CaptureSurfaceError::PlaneTruncated { plane: index });
            }
            let end = plane
                .offset
                .checked_add(plane.size)
                .ok_or(CaptureSurfaceError::LayoutOverflow)?;
            match buffer_size {
                Some(size) if end > *size => {
                    return Err(CaptureSurfaceError::PlaneOutsideBuffer { plane: index });
                }
                Some(_) => {}
                None if matches!(self.memory, CaptureMemory::GpuDevice) => {}
                None => return Err(CaptureSurfaceError::BufferSizeUnknown { plane: index }),
            }
            if extents.iter().any(|(buffer, start, stop)| {
                *buffer == plane.buffer && plane.offset < *stop && *start < end
            }) {
                return Err(CaptureSurfaceError::PlaneOverlap);
            }
            extents.push((plane.buffer, plane.offset, end));
        }
        Ok(())
    }

    fn validate_path(
        &self,
        importer: &CaptureImporter,
        gpu: GpuMatch,
    ) -> Result<(), CaptureSurfaceError> {
        let path = self.copy_path;
        if path.requires_same_gpu() {
            if matches!(self.memory, CaptureMemory::HostShared) {
                return Err(CaptureSurfaceError::CopyPathMismatch);
            }
            match gpu {
                GpuMatch::Same => {}
                GpuMatch::Different => return Err(CaptureSurfaceError::GpuMismatch),
                GpuMatch::Unknown => return Err(CaptureSurfaceError::GpuIdentityUnproven),
            }
        }
        if path == CopyPath::CrossGpu && gpu == GpuMatch::Same {
            return Err(CaptureSurfaceError::CopyPathMismatch);
        }
        if let CaptureMemory::DmaBuf { modifier } = self.memory {
            let class = modifier.class();
            if path.host_round_trip() {
                if class != ModifierClass::Linear {
                    return Err(CaptureSurfaceError::NonLinearForCpu(class));
                }
            } else if class == ModifierClass::Implicit
                || !importer.gpu_modifiers.contains(&modifier)
            {
                return Err(CaptureSurfaceError::UnsupportedModifier(class));
            }
        }
        Ok(())
    }

    fn effective_damage(&self) -> DamageEvidence {
        match &self.damage {
            DamageEvidence::Regions(rects)
                if rects.len() <= MAX_CAPTURE_DAMAGE_RECTS
                    && rects.iter().all(|rect| {
                        rect_within(*rect, self.geometry.coded_width, self.geometry.coded_height)
                    }) =>
            {
                DamageEvidence::Regions(rects.clone())
            }
            DamageEvidence::Full => DamageEvidence::Full,
            DamageEvidence::Regions(_) | DamageEvidence::Unknown => DamageEvidence::Unknown,
        }
    }
}

fn rect_within(rect: PixelRect, width: u32, height: u32) -> bool {
    let (Ok(width), Ok(height)) = (usize::try_from(width), usize::try_from(height)) else {
        return false;
    };
    rect.x
        .checked_add(rect.width)
        .is_some_and(|right| right <= width)
        && rect
            .y
            .checked_add(rect.height)
            .is_some_and(|bottom| bottom <= height)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NVIDIA: GpuIdentity = GpuIdentity {
        pci: Some(PciAddress {
            domain: 0,
            bus: 0x41,
            device: 0,
            function: 0,
        }),
        render_node_minor: Some(128),
        vendor_id: Some(0x10de),
    };
    const OTHER: GpuIdentity = GpuIdentity {
        pci: Some(PciAddress {
            domain: 0,
            bus: 0x05,
            device: 0,
            function: 0,
        }),
        render_node_minor: Some(129),
        vendor_id: Some(0x8086),
    };
    const NV_TILED: FormatModifier = FormatModifier(0x0300_0000_0060_6014);

    fn output() -> OutputIdentity {
        OutputIdentity::new("Meta-0").unwrap()
    }

    fn generation(value: u64) -> RegionGeneration {
        RegionGeneration::new(value).unwrap()
    }

    const COLOUR: SourceColour = SourceColour {
        primaries: SourceFact::Authoritative(ColorPrimaries::Bt709),
        transfer: SourceFact::Authoritative(SourceTransfer::Srgb),
        ..SourceColour::UNKNOWN
    };

    fn importer() -> CaptureImporter {
        CaptureImporter {
            output: output(),
            generation: generation(3),
            colour: COLOUR,
            encoder_gpu: Some(NVIDIA),
            gpu_modifiers: vec![FormatModifier::LINEAR, NV_TILED],
            last_sequence: Some(10),
        }
    }

    fn surface() -> CaptureSurface {
        CaptureSurface {
            output: output(),
            generation: generation(3),
            backend: CaptureBackend::PipeWire,
            format: CapturePixelFormat::Bgrx8,
            geometry: CaptureGeometry {
                coded_width: 1920,
                coded_height: 1080,
                visible: PixelRect {
                    x: 0,
                    y: 0,
                    width: 1920,
                    height: 1080,
                },
            },
            planes: vec![PlaneLayout {
                buffer: 0,
                offset: 0,
                stride: 1920 * 4,
                size: 1920 * 4 * 1080,
            }],
            buffer_sizes: vec![Some(1920 * 4 * 1080)],
            memory: CaptureMemory::DmaBuf {
                modifier: FormatModifier::LINEAR,
            },
            colour: COLOUR,
            timestamps: CaptureTimestamps {
                sequence: 11,
                capture_monotonic_ns: 1,
                presentation_ns: None,
            },
            damage: DamageEvidence::Regions(vec![PixelRect {
                x: 0,
                y: 0,
                width: 16,
                height: 16,
            }]),
            cursor: CursorEvidence::SeparateMetadata,
            gpu: Some(NVIDIA),
            copy_path: CopyPath::ZeroCopyImport,
        }
    }

    #[test]
    fn a_proven_linear_same_gpu_surface_is_zero_copy() {
        let validated = surface().validate(&importer()).unwrap();
        assert_eq!(validated.gpu_match(), GpuMatch::Same);
        assert!(validated.copy_path().zero_copy());
        assert!(!validated.damage().is_full());
    }

    #[test]
    fn padded_planes_are_accepted_and_short_strides_refused() {
        let mut padded = surface();
        padded.planes[0].stride = 8192;
        padded.planes[0].size = 8192 * 1080;
        padded.buffer_sizes = vec![Some(8192 * 1080)];
        assert!(padded.validate(&importer()).is_ok());

        let mut short = surface();
        short.planes[0].stride = 1919 * 4;
        assert_eq!(
            short.validate(&importer()).unwrap_err(),
            CaptureSurfaceError::StrideTooSmall { plane: 0 }
        );
    }

    #[test]
    fn last_row_need_not_be_padded() {
        let mut tight = surface();
        tight.planes[0].stride = 8192;
        tight.planes[0].size = 8192 * 1079 + 1920 * 4;
        tight.buffer_sizes = vec![Some(8192 * 1079 + 1920 * 4)];
        assert!(tight.validate(&importer()).is_ok());
    }

    #[test]
    fn unknown_buffer_size_bounds_nothing() {
        let mut dmabuf = surface();
        dmabuf.buffer_sizes = vec![None];
        assert_eq!(
            dmabuf.validate(&importer()).unwrap_err(),
            CaptureSurfaceError::BufferSizeUnknown { plane: 0 }
        );
        let mut shm = surface();
        shm.memory = CaptureMemory::HostShared;
        shm.copy_path = CopyPath::HostCopy;
        shm.buffer_sizes = vec![None];
        assert_eq!(
            shm.validate(&importer()).unwrap_err(),
            CaptureSurfaceError::BufferSizeUnknown { plane: 0 }
        );
        // An opaque GPU allocation is bounded by its importer, not by a size
        // the producer states.
        let mut device = surface();
        device.memory = CaptureMemory::GpuDevice;
        device.buffer_sizes = vec![None];
        assert!(device.validate(&importer()).is_ok());
    }

    #[test]
    fn truncated_layouts_are_refused() {
        let mut truncated = surface();
        truncated.planes[0].size -= 1;
        assert_eq!(
            truncated.validate(&importer()).unwrap_err(),
            CaptureSurfaceError::PlaneTruncated { plane: 0 }
        );
        let mut outside = surface();
        outside.planes[0].offset = 64;
        assert_eq!(
            outside.validate(&importer()).unwrap_err(),
            CaptureSurfaceError::PlaneOutsideBuffer { plane: 0 }
        );
        let mut missing = surface();
        missing.planes[0].buffer = 1;
        assert_eq!(
            missing.validate(&importer()).unwrap_err(),
            CaptureSurfaceError::UnknownBuffer { plane: 0 }
        );
        let mut overflow = surface();
        overflow.planes[0].offset = u64::MAX;
        assert_eq!(
            overflow.validate(&importer()).unwrap_err(),
            CaptureSurfaceError::LayoutOverflow
        );
    }

    #[test]
    fn nv12_planes_are_checked_independently() {
        let mut nv12 = surface();
        nv12.format = CapturePixelFormat::Nv12;
        nv12.planes = vec![
            PlaneLayout {
                buffer: 0,
                offset: 0,
                stride: 1920,
                size: 1920 * 1080,
            },
            PlaneLayout {
                buffer: 0,
                offset: 1920 * 1080,
                stride: 1920,
                size: 1920 * 540,
            },
        ];
        nv12.buffer_sizes = vec![Some(1920 * 1620)];
        assert!(nv12.clone().validate(&importer()).is_ok());

        let mut overlapping = nv12.clone();
        overlapping.planes[1].offset = 1920 * 1000;
        assert_eq!(
            overlapping.validate(&importer()).unwrap_err(),
            CaptureSurfaceError::PlaneOverlap
        );
        let mut one_plane = nv12;
        one_plane.planes.pop();
        assert_eq!(
            one_plane.validate(&importer()).unwrap_err(),
            CaptureSurfaceError::PlaneCountMismatch {
                expected: 2,
                actual: 1
            }
        );
    }

    #[test]
    fn non_linear_and_implicit_modifiers_need_proof() {
        let mut tiled = surface();
        tiled.memory = CaptureMemory::DmaBuf { modifier: NV_TILED };
        assert!(tiled.clone().validate(&importer()).is_ok());

        let mut unproven = importer();
        unproven.gpu_modifiers = vec![FormatModifier::LINEAR];
        assert_eq!(
            tiled.clone().validate(&unproven).unwrap_err(),
            CaptureSurfaceError::UnsupportedModifier(ModifierClass::Vendor)
        );

        let mut cpu = tiled;
        cpu.copy_path = CopyPath::HostCopy;
        assert_eq!(
            cpu.validate(&importer()).unwrap_err(),
            CaptureSurfaceError::NonLinearForCpu(ModifierClass::Vendor)
        );

        let mut implicit = surface();
        implicit.memory = CaptureMemory::DmaBuf {
            modifier: FormatModifier::IMPLICIT,
        };
        let mut permissive = importer();
        permissive.gpu_modifiers.push(FormatModifier::IMPLICIT);
        assert_eq!(
            implicit.validate(&permissive).unwrap_err(),
            CaptureSurfaceError::UnsupportedModifier(ModifierClass::Implicit)
        );
    }

    #[test]
    fn stale_and_future_generations_and_foreign_outputs_are_refused() {
        let mut stale = surface();
        stale.generation = generation(2);
        assert_eq!(
            stale.validate(&importer()).unwrap_err(),
            CaptureSurfaceError::StaleGeneration {
                presented: 2,
                current: 3
            }
        );
        let mut future = surface();
        future.generation = generation(4);
        assert!(matches!(
            future.validate(&importer()).unwrap_err(),
            CaptureSurfaceError::FutureGeneration { .. }
        ));
        let mut foreign = surface();
        foreign.output = OutputIdentity::new("Meta-1").unwrap();
        assert_eq!(
            foreign.validate(&importer()).unwrap_err(),
            CaptureSurfaceError::OutputMismatch
        );
        let mut replay = surface();
        replay.timestamps.sequence = 10;
        assert_eq!(
            replay.validate(&importer()).unwrap_err(),
            CaptureSurfaceError::NonMonotonicSequence
        );
    }

    #[test]
    fn wrong_or_unknown_gpu_never_claims_zero_copy() {
        let mut other = surface();
        other.gpu = Some(OTHER);
        assert_eq!(
            other.clone().validate(&importer()).unwrap_err(),
            CaptureSurfaceError::GpuMismatch
        );
        other.copy_path = CopyPath::CrossGpu;
        assert!(other.validate(&importer()).is_ok());

        let mut unknown = surface();
        unknown.gpu = None;
        assert_eq!(
            unknown.clone().validate(&importer()).unwrap_err(),
            CaptureSurfaceError::GpuIdentityUnproven
        );
        unknown.copy_path = CopyPath::GpuConvert;
        assert_eq!(
            unknown.validate(&importer()).unwrap_err(),
            CaptureSurfaceError::GpuIdentityUnproven
        );

        let mut vendor_only = surface();
        vendor_only.gpu = Some(GpuIdentity {
            vendor_id: Some(0x10de),
            ..GpuIdentity::default()
        });
        assert_eq!(
            vendor_only.validate(&importer()).unwrap_err(),
            CaptureSurfaceError::GpuIdentityUnproven,
            "a vendor id alone does not identify a device"
        );

        let mut mislabelled = surface();
        mislabelled.copy_path = CopyPath::CrossGpu;
        assert_eq!(
            mislabelled.validate(&importer()).unwrap_err(),
            CaptureSurfaceError::CopyPathMismatch
        );
    }

    #[test]
    fn host_memory_is_always_a_host_copy() {
        let mut shm = surface();
        shm.memory = CaptureMemory::HostShared;
        assert_eq!(
            shm.clone().validate(&importer()).unwrap_err(),
            CaptureSurfaceError::CopyPathMismatch
        );
        shm.copy_path = CopyPath::HostCopy;
        let validated = shm.validate(&importer()).unwrap();
        assert_eq!(
            validated.copy_path().conversion_backend(),
            ConversionBackend::Cpu
        );
        // The backend family keeps its existing classification.
        assert!(!CaptureBackend::PipeWire.zero_copy());
    }

    #[test]
    fn missing_or_untrustworthy_damage_is_full() {
        let mut unknown = surface();
        unknown.damage = DamageEvidence::Unknown;
        let validated = unknown.validate(&importer()).unwrap();
        assert!(validated.damage().is_full());
        assert_eq!(
            validated.damage().metadata_status(),
            DamageMetadataStatus::Unknown
        );

        let mut outside = surface();
        outside.damage = DamageEvidence::Regions(vec![PixelRect {
            x: 1910,
            y: 0,
            width: 32,
            height: 16,
        }]);
        assert!(outside.validate(&importer()).unwrap().damage().is_full());

        let mut excessive = surface();
        excessive.damage = DamageEvidence::Regions(vec![
            PixelRect {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            };
            MAX_CAPTURE_DAMAGE_RECTS + 1
        ]);
        assert!(excessive.validate(&importer()).unwrap().damage().is_full());

        let mut clean = surface();
        clean.damage = DamageEvidence::Regions(Vec::new());
        assert!(!clean.validate(&importer()).unwrap().damage().is_full());
    }

    #[test]
    fn a_colour_change_is_a_new_epoch_not_a_relabel() {
        let mut pq = surface();
        pq.colour.transfer = SourceFact::Authoritative(SourceTransfer::Pq);
        assert_eq!(
            pq.validate(&importer()).unwrap_err(),
            CaptureSurfaceError::ColourChanged
        );
    }

    #[test]
    fn geometry_must_be_nonempty_and_contained() {
        let mut empty = surface();
        empty.geometry.visible.width = 0;
        assert_eq!(
            empty.validate(&importer()).unwrap_err(),
            CaptureSurfaceError::EmptyGeometry
        );
        let mut outside = surface();
        outside.geometry.visible.x = 8;
        assert_eq!(
            outside.validate(&importer()).unwrap_err(),
            CaptureSurfaceError::VisibleOutsideCoded
        );
    }

    #[test]
    fn gpu_comparison_is_conservative() {
        assert_eq!(NVIDIA.compare(&NVIDIA), GpuMatch::Same);
        assert_eq!(NVIDIA.compare(&OTHER), GpuMatch::Different);
        let mut conflicting = NVIDIA;
        conflicting.render_node_minor = Some(129);
        assert_eq!(NVIDIA.compare(&conflicting), GpuMatch::Different);
        assert_eq!(compare_gpus(Some(&NVIDIA), None), GpuMatch::Unknown);
        assert!(PciAddress::new(0, 0, 32, 0).is_err());
        assert!(PciAddress::new(0, 0, 0, 8).is_err());
    }

    #[test]
    fn formats_describe_precision_and_planes() {
        for format in CapturePixelFormat::ALL {
            assert_eq!(
                CapturePixelFormat::from_token(format.token()),
                Some(*format)
            );
        }
        assert_eq!(
            CapturePixelFormat::Xrgb2101010.packed_rgb10_layout(),
            Some(PackedRgb10Layout::XRGB2101010)
        );
        assert!(CapturePixelFormat::RgbaF16.precision().is_wide());
        assert!(!CapturePixelFormat::Nv12.precision().is_wide());
        assert_eq!(CapturePixelFormat::RgbaF16.min_row_bytes(0, 10), Some(80));
        assert_eq!(CapturePixelFormat::Nv12.min_row_bytes(1, 11), Some(12));
        assert_eq!(CapturePixelFormat::Nv12.plane_rows(1, 11), 6);
        assert_eq!(FormatModifier::IMPLICIT.class(), ModifierClass::Implicit);
        assert_eq!(FormatModifier::LINEAR.class(), ModifierClass::Linear);
    }
}
