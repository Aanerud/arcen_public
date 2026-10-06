#![allow(unsafe_code)]

//! `ScreenCaptureKit` capture for the macOS Pier.
//!
//! This is the native adapter only. It turns an already-resolved capture plan
//! into real frames and reports what the system actually produced. Choosing a
//! plan, deciding whether the result satisfies a media contract, and degrading
//! a stream all stay in the shared crates.
//!
//! Frames are handed on as `IOSurface`s so the encoder can stay on the GPU
//! path; nothing is copied through main memory here.

use std::ffi::c_void;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TryRecvError, sync_channel};
use std::time::Duration;
use std::time::Instant;

use apple_cf::iosurface::IOSurface;
use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{AnyThread, DefinedClass, define_class, sel};
use objc2_core_foundation::{CFRetained, Type};
use objc2_core_media::CMSampleBuffer;
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::SCStreamFrameInfoDirtyRects;
use objc2_screen_capture_kit::{
    SCContentFilter, SCShareableContent, SCStream, SCStreamConfiguration, SCStreamDelegate,
    SCStreamOutput, SCStreamOutputType,
};
use serde::Serialize;

/// How long to wait for `WindowServer` to answer a one-shot query.
const CONTENT_TIMEOUT: Duration = Duration::from_secs(10);
/// Bounded frame queue. Capture must never block `WindowServer`, so a slow
/// consumer loses frames rather than stalling the compositor.
const FRAME_QUEUE_DEPTH: usize = 8;

/// How many frames `ScreenCaptureKit` may hold before handing one over.
///
/// Eight, not the three Apple documents as the default, because three was
/// measured slower. Back to back on the lab against a real Deck, changing
/// only this number:
///
/// | queueDepth | capture to socket | encode |
/// | --- | --- | --- |
/// | 3 | 19.56 ms | 19.55 ms |
/// | 8 | 12.82 ms | 12.81 ms |
///
/// The intuition that a shallower queue means fresher frames is wrong here:
/// this host holds a surface while it encodes, so a three-deep pool leaves
/// ScreenCaptureKit short of somewhere to render the next frame and the
/// pipeline stalls. Kept as its own constant rather than shared with
/// [`FRAME_QUEUE_DEPTH`] so that the two can be reasoned about separately —
/// they answer different questions and only coincidentally have the same
/// value.
const SOURCE_QUEUE_DEPTH: usize = 8;

#[link(name = "CoreMedia", kind = "framework")]
unsafe extern "C" {
    fn CMSampleBufferGetImageBuffer(sample_buffer: *mut c_void) -> *mut c_void;
}

#[link(name = "CoreVideo", kind = "framework")]
unsafe extern "C" {
    fn CVPixelBufferGetIOSurface(pixel_buffer: *mut c_void) -> *mut c_void;
    fn CVBufferCopyAttachment(
        buffer: *mut c_void,
        key: *const c_void,
        mode: *mut u32,
    ) -> *const c_void;
    static kCVImageBufferTransferFunctionKey: *const c_void;
    static kCVImageBufferColorPrimariesKey: *const c_void;
    static kCVImageBufferYCbCrMatrixKey: *const c_void;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRetain(reference: *const c_void) -> *const c_void;
    fn CFRelease(reference: *const c_void);
    fn CFGetTypeID(reference: *const c_void) -> usize;
    fn CFStringGetTypeID() -> usize;
    fn CFStringGetCString(
        string: *const c_void,
        buffer: *mut u8,
        size: isize,
        encoding: u32,
    ) -> bool;
}

/// Reads one string attachment of a `CoreVideo` buffer, or `None`.
///
/// # Safety
///
/// `buffer` must be a live `CVBuffer` and `key` a `CoreVideo` key constant.
unsafe fn string_attachment(buffer: *mut c_void, key: *const c_void) -> Option<String> {
    // SAFETY: the caller guarantees both; the copy is released below.
    let value = unsafe { CVBufferCopyAttachment(buffer, key, std::ptr::null_mut()) };
    if value.is_null() {
        return None;
    }
    let mut text = [0_u8; 128];
    // SAFETY: `value` is a live CF object owned here; `text` is writable for
    // its length; 0x0800_0100 is kCFStringEncodingUTF8.
    let read = unsafe {
        CFGetTypeID(value) == CFStringGetTypeID()
            && CFStringGetCString(value, text.as_mut_ptr(), 128, 0x0800_0100)
    };
    // SAFETY: balances the copy.
    unsafe { CFRelease(value) };
    read.then(|| {
        let end = text
            .iter()
            .position(|&byte| byte == 0)
            .unwrap_or(text.len());
        String::from_utf8_lossy(&text[..end]).into_owned()
    })
}

/// Reports what `ScreenCaptureKit` actually tagged a frame with.
///
/// The configuration asks for a colour space and matrix; the frame's own
/// attachments say what was delivered. The two are compared once per
/// capture, on its first frame, because the attachments are the truth the
/// encoder then inherits.
///
/// # Safety
///
/// `sample_buffer` must be a live sample buffer.
unsafe fn report_surface_colour(sample_buffer: &CMSampleBuffer, frame: &CapturedFrame) {
    let pixel_format = frame.pixel_format;
    let sample_ptr = std::ptr::from_ref::<CMSampleBuffer>(sample_buffer)
        .cast::<c_void>()
        .cast_mut();
    // SAFETY: the caller guarantees a live buffer; the image buffer is borrowed.
    let image = unsafe { CMSampleBufferGetImageBuffer(sample_ptr) };
    if image.is_null() {
        return;
    }
    // SAFETY: a live CVBuffer and CoreVideo's own key constants.
    let (transfer, primaries, matrix) = unsafe {
        (
            string_attachment(image, kCVImageBufferTransferFunctionKey),
            string_attachment(image, kCVImageBufferColorPrimariesKey),
            string_attachment(image, kCVImageBufferYCbCrMatrixKey),
        )
    };
    let code = pixel_format.to_be_bytes();
    let luma = ten_bit_luma_census(frame);
    let pq = transfer.as_deref() == Some("SMPTE_ST_2084_PQ");
    tracing::info!(
        target: arcen_telemetry::names::target::MEDIA,
        pixel_format = %String::from_utf8_lossy(&code),
        transfer = transfer.as_deref().unwrap_or("<absent>"),
        primaries = primaries.as_deref().unwrap_or("<absent>"),
        matrix = matrix.as_deref().unwrap_or("<absent>"),
        luma_p99 = luma.map(|(p99, _)| p99),
        luma_max = luma.map(|(_, max)| max),
        luma_p99_nits = luma.filter(|_| pq).map(|(p99, _)| pq_code_to_nits(p99)),
        luma_max_nits = luma.filter(|_| pq).map(|(_, max)| pq_code_to_nits(max)),
        "captured surface colour"
    );
}

/// The 99th-percentile and largest luma code of a 10-bit bi-planar frame,
/// sampled on a grid. `None` for other layouts.
///
/// How bright the desktop is inside a PQ stream is a measurement, not a
/// constant: it is where macOS put SDR white, and it decides how the Deck's
/// display renders every ordinary window.
pub(crate) fn ten_bit_luma_census(frame: &CapturedFrame) -> Option<(u16, u16)> {
    if !CapturePixelFormat::from_os_type(frame.pixel_format)
        .is_some_and(CapturePixelFormat::is_high_precision)
    {
        return None;
    }
    let guard = frame.surface.lock_read_only().ok()?;
    let base = guard.base_address_of_plane(0)?;
    let row = frame.surface.bytes_per_row_of_plane(0);
    let (width, height) = (frame.width, frame.height);
    let mut histogram = vec![0_u32; 1024];
    let mut samples = 0_u32;
    for y in (0..height).step_by(4) {
        for x in (0..width).step_by(4) {
            // SAFETY: inside plane 0, locked for reading; samples are 16-bit
            // with ten significant bits at the top.
            let sample = unsafe { base.add(y * row + x * 2).cast::<u16>().read_unaligned() };
            histogram[usize::from(sample >> 6)] += 1;
            samples += 1;
        }
    }
    let max = u16::try_from(histogram.iter().rposition(|&count| count > 0)?).ok()?;
    let threshold = samples - samples / 100;
    let mut seen = 0;
    let p99 = histogram
        .iter()
        .position(|&count| {
            seen += count;
            seen >= threshold
        })
        .and_then(|code| u16::try_from(code).ok())?;
    Some((p99, max))
}

/// A full-range 10-bit PQ code's luminance, to a tenth of a nit, for logs.
pub(crate) fn pq_code_to_nits(code: u16) -> f64 {
    (arcen_media::video::pq_white::pq_code_to_nits(code) * 10.0).round() / 10.0
}

/// Pixel layout requested from `WindowServer`.
///
/// These are the only layouts the Pier asks for, one per media contract. A
/// wider set would imply capture paths the product does not actually have.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturePixelFormat {
    /// 8-bit BGRA. The SDR interchange layout.
    Bgra8,
    /// 8-bit 4:2:0 bi-planar, video range (`420v`).
    Nv12VideoRange,
    /// 10-bit 4:2:0 bi-planar, video range (`x420`).
    Nv12TenBitVideoRange,
    /// 10-bit 4:4:4, video range (`x444`).
    FourFourFourTenBit,
    /// 10-bit 4:4:4, full range (`xf44`). Grading: the contract is full
    /// range, and capturing video range would spend a tenth of the codes on
    /// headroom the desktop never uses.
    FourFourFourTenBitFullRange,
}

impl CapturePixelFormat {
    /// The chroma subsampling a stream of this layout carries.
    #[must_use]
    pub const fn chroma(self) -> arcen_media::ChromaSubsampling {
        match self {
            // BGRA is not subsampled, but it is converted to 4:2:0 before it
            // reaches an encoder, and what matters here is what is encoded.
            Self::Bgra8 | Self::Nv12VideoRange | Self::Nv12TenBitVideoRange => {
                arcen_media::ChromaSubsampling::Yuv420
            }
            Self::FourFourFourTenBit | Self::FourFourFourTenBitFullRange => {
                arcen_media::ChromaSubsampling::Yuv444
            }
        }
    }

    /// The sample depth a stream of this layout carries.
    #[must_use]
    pub const fn bit_depth(self) -> arcen_media::BitDepth {
        match self {
            Self::Bgra8 | Self::Nv12VideoRange => arcen_media::BitDepth::Eight,
            Self::Nv12TenBitVideoRange
            | Self::FourFourFourTenBit
            | Self::FourFourFourTenBitFullRange => arcen_media::BitDepth::Ten,
        }
    }

    /// Whether this layout carries YCbCr planes.
    ///
    /// `ScreenCaptureKit`'s `colorMatrix` is documented as applying only to the
    /// 4:2:0 biplanar formats. Setting it on a packed RGB layout would assert
    /// a conversion that is not taking place.
    #[must_use]
    pub const fn is_ycbcr(self) -> bool {
        match self {
            Self::Bgra8 => false,
            Self::Nv12VideoRange
            | Self::Nv12TenBitVideoRange
            | Self::FourFourFourTenBit
            | Self::FourFourFourTenBitFullRange => true,
        }
    }
}

impl CapturePixelFormat {
    /// Returns the `CoreVideo` `OSType` for this layout.
    #[must_use]
    pub const fn os_type(self) -> u32 {
        match self {
            Self::Bgra8 => 0x4247_5241,
            Self::Nv12VideoRange => 0x3432_3076,
            Self::Nv12TenBitVideoRange => 0x7834_3230,
            Self::FourFourFourTenBit => 0x7834_3434,
            Self::FourFourFourTenBitFullRange => 0x7866_3434,
        }
    }

    /// Returns the layout for an `OSType`, if the Pier requests it at all.
    #[must_use]
    pub const fn from_os_type(value: u32) -> Option<Self> {
        match value {
            0x4247_5241 => Some(Self::Bgra8),
            0x3432_3076 => Some(Self::Nv12VideoRange),
            0x7834_3230 => Some(Self::Nv12TenBitVideoRange),
            0x7834_3434 => Some(Self::FourFourFourTenBit),
            0x7866_3434 => Some(Self::FourFourFourTenBitFullRange),
            _ => None,
        }
    }

    /// Returns whether this layout carries more than eight bits per component.
    #[must_use]
    pub const fn is_high_precision(self) -> bool {
        matches!(
            self,
            Self::Nv12TenBitVideoRange
                | Self::FourFourFourTenBit
                | Self::FourFourFourTenBitFullRange
        )
    }
}

/// Dynamic range requested from `ScreenCaptureKit`.
///
/// HDR capture is Apple-silicon only. Requesting it is not proof of an HDR
/// stream: the resulting surface's transfer function still has to be verified
/// before the Pier may advertise HDR.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureDynamicRange {
    /// Ordinary SDR capture.
    Sdr,
    /// HDR referenced to the local display's characteristics.
    HdrLocalDisplay,
    /// HDR referenced to a canonical display.
    HdrCanonicalDisplay,
}

impl CaptureDynamicRange {
    const fn raw(self) -> isize {
        match self {
            Self::Sdr => 0,
            Self::HdrLocalDisplay => 1,
            Self::HdrCanonicalDisplay => 2,
        }
    }

    const fn requires_screen_capture_kit_setter(self) -> bool {
        !matches!(self, Self::Sdr)
    }
}

/// A resolved capture plan for one display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureConfig {
    /// `CoreGraphics` display to capture.
    pub display_id: u32,
    /// Requested output width in pixels.
    pub width: usize,
    /// Requested output height in pixels.
    pub height: usize,
    /// Requested frame rate.
    pub fps: u32,
    /// Whether `WindowServer` composites the cursor into the frame.
    pub shows_cursor: bool,
    /// Requested pixel layout.
    pub pixel_format: CapturePixelFormat,
    /// Requested dynamic range.
    pub dynamic_range: CaptureDynamicRange,
}

impl CaptureConfig {
    /// Returns this plan with the compositor drawing the pointer, or not.
    ///
    /// A composited pointer is always the right shape, because it is the real
    /// one; it is also exactly as late as the picture it is drawn into. A Deck
    /// drawing its own is instant and always an arrow. Neither is better, so
    /// the choice belongs to the person and arrives in the handshake.
    #[must_use]
    pub const fn showing_cursor(mut self, shows_cursor: bool) -> Self {
        self.shows_cursor = shows_cursor;
        self
    }

    /// Creates an 8-bit SDR plan, the Auto and Speed capture shape.
    #[must_use]
    pub const fn sdr(display_id: u32, width: usize, height: usize, fps: u32) -> Self {
        Self {
            display_id,
            width,
            height,
            fps,
            shows_cursor: false,
            pixel_format: CapturePixelFormat::Nv12VideoRange,
            dynamic_range: CaptureDynamicRange::Sdr,
        }
    }

    /// Creates a 10-bit 4:4:4 HDR plan: PQ over BT.2020, full range.
    ///
    /// The same surface as Grading — Apple's own HDR stream preset is `xf44`
    /// too — with the dynamic range and colour space that make it HDR. The
    /// preset pairs it with Display P3 PQ and a BT.709 matrix; this asks for
    /// ITU-R BT.2100 PQ and the BT.2020 matrix instead, because that is the
    /// contract on the wire, and reads the frames' own attachments back to
    /// see which it was given.
    #[must_use]
    pub const fn hdr(display_id: u32, width: usize, height: usize, fps: u32) -> Self {
        Self {
            display_id,
            width,
            height,
            fps,
            shows_cursor: false,
            pixel_format: CapturePixelFormat::FourFourFourTenBitFullRange,
            dynamic_range: CaptureDynamicRange::HdrLocalDisplay,
        }
    }

    /// Creates a 10-bit 4:4:4 SDR plan, the Grading capture shape.
    ///
    /// A separate constructor rather than a flag on [`Self::sdr`]. Grading and
    /// the 8-bit path are different contracts with different surface formats
    /// and different copy costs, and widening the fast path to carry both is
    /// how the cheap one stops being cheap.
    ///
    /// The dynamic range stays SDR. Ten bits and 4:4:4 buy precision and
    /// chroma resolution; they do not change the transfer function, and
    /// labelling this HDR because it is deep would present an ordinary desktop
    /// as one.
    #[must_use]
    pub const fn grading(display_id: u32, width: usize, height: usize, fps: u32) -> Self {
        Self {
            display_id,
            width,
            height,
            fps,
            shows_cursor: false,
            pixel_format: CapturePixelFormat::FourFourFourTenBitFullRange,
            dynamic_range: CaptureDynamicRange::Sdr,
        }
    }
}

/// A changed region of a captured frame, in surface pixels.
///
/// Plain numbers on purpose. `ScreenCaptureKit` hands these over as a
/// `CFArray` of `CGRect` dictionaries in an attachment, and none of that is
/// anything the portable damage logic should have to know about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DamageRect {
    /// Left edge in pixels from the surface origin.
    pub x: u32,
    /// Top edge in pixels from the surface origin.
    pub y: u32,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

/// What `ScreenCaptureKit` said changed in a frame.
///
/// The distinction between "nothing changed" and "we were not told" is the
/// whole point of this type. Treating an unknown as unchanged is how a host
/// stops sending a picture that is actually moving, so the two cases are
/// different variants rather than an empty list standing in for both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameDamage {
    /// The frame carried no usable damage attachment, so assume everything
    /// changed.
    Unknown,
    /// The frame carried damage, which may legitimately be empty.
    Rects(Vec<DamageRect>),
}

impl FrameDamage {
    /// Returns true when the source positively said nothing changed.
    #[must_use]
    pub fn is_known_clean(&self) -> bool {
        matches!(self, Self::Rects(rects) if rects.is_empty())
    }

    /// Merges damage from a superseded frame into the newer frame that will be
    /// encoded instead. Unknown dominates because a missing damage attachment
    /// means the whole older frame may have changed.
    pub fn merge_superseded(&mut self, superseded: Self) {
        match (self, superseded) {
            (current @ Self::Unknown, _) | (current, Self::Unknown) => *current = Self::Unknown,
            (Self::Rects(current), Self::Rects(mut old)) => current.append(&mut old),
        }
    }
}

/// One captured frame, still on the GPU.
#[derive(Debug, Clone)]
pub struct CapturedFrame {
    /// The backing surface, ready to hand to the encoder.
    pub surface: IOSurface,
    /// Keeps `ScreenCaptureKit`'s backing pool lease alive until encode.
    _sample_buffer: CFRetained<CMSampleBuffer>,
    /// Surface width in pixels.
    pub width: usize,
    /// Surface height in pixels.
    pub height: usize,
    /// Actual `CoreVideo` pixel format of the surface.
    pub pixel_format: u32,
    /// What the compositor said changed since the previous frame.
    pub damage: FrameDamage,
    /// A converted copy's own buffer, kept alive for as long as the frame,
    /// so the pool cannot hand it out again while the encoder still reads it.
    converted: Option<Arc<crate::hdr_white::ConvertedBuffer>>,
}

impl CapturedFrame {
    /// This frame with its pixels replaced by a converted surface. The
    /// capture's own lease is kept too: the frame still describes that
    /// capture, including its damage.
    #[must_use]
    pub fn with_converted_surface(
        &self,
        surface: IOSurface,
        buffer: Arc<crate::hdr_white::ConvertedBuffer>,
    ) -> Self {
        Self {
            width: surface.width(),
            height: surface.height(),
            pixel_format: surface.pixel_format(),
            surface,
            _sample_buffer: self._sample_buffer.clone(),
            damage: self.damage.clone(),
            converted: Some(buffer),
        }
    }
}

impl CapturedFrame {
    /// Wraps the `IOSurface` behind a `ScreenCaptureKit` sample buffer.
    ///
    /// Returns `None` for frames that carry no image (`ScreenCaptureKit` emits
    /// these for idle displays) or that are not `IOSurface`-backed.
    ///
    /// # Safety
    ///
    /// `sample_buffer` must be a live sample buffer delivered by
    /// `ScreenCaptureKit`.
    unsafe fn from_sample_buffer(sample_buffer: &CMSampleBuffer) -> Option<Self> {
        let sample_ptr = std::ptr::from_ref::<CMSampleBuffer>(sample_buffer)
            .cast::<c_void>()
            .cast_mut();
        // SAFETY: the caller guarantees a live `CMSampleBuffer`; CoreMedia
        // returns a borrowed image buffer for the duration of that lifetime.
        let image_buffer = unsafe { CMSampleBufferGetImageBuffer(sample_ptr) };
        if image_buffer.is_null() {
            return None;
        }
        // SAFETY: `image_buffer` is the live CoreVideo image buffer returned
        // from the sample buffer above.
        let surface_ref = unsafe { CVPixelBufferGetIOSurface(image_buffer) };
        if surface_ref.is_null() {
            return None;
        }
        // Read before the surface is adopted, so a frame that carries no
        // usable damage is still a frame rather than a failure.
        // SAFETY: the caller guarantees a live sample buffer.
        let damage = unsafe { read_frame_damage(sample_buffer) };
        let sample_buffer = sample_buffer.retain();
        // `IOSurface::from_raw` adopts a +1 object reference. The retained
        // sample buffer is the pool lease that keeps the contents from being
        // recycled before the queued frame reaches VideoToolbox.
        // SAFETY: `surface_ref` is a non-null IOSurface borrowed from a live
        // CVPixelBuffer; retaining it creates the +1 reference adopted below.
        let retained = unsafe { CFRetain(surface_ref.cast_const()) };
        let surface = IOSurface::from_raw(retained.cast_mut())?;
        Some(Self {
            width: surface.width(),
            height: surface.height(),
            pixel_format: surface.pixel_format(),
            surface,
            _sample_buffer: sample_buffer,
            damage,
            converted: None,
        })
    }
}

/// A `CGRect` as CoreGraphics lays it out.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct CGRectRaw {
    origin_x: f64,
    origin_y: f64,
    width: f64,
    height: f64,
}

// SAFETY: these are public CoreFoundation and CoreGraphics symbols with the
// signatures given in `CFDictionary.h`, `CFArray.h` and `CGGeometry.h`. They
// are declared here rather than taken from the typed bindings because the
// attachment is an untyped `CFDictionary` of untyped values, which is exactly
// the shape the generated wrappers refuse to express.
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFDictionaryGetValue(dictionary: *const c_void, key: *const c_void) -> *const c_void;
    fn CFArrayGetCount(array: *const c_void) -> isize;
    fn CFArrayGetValueAtIndex(array: *const c_void, index: isize) -> *const c_void;
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGRectMakeWithDictionaryRepresentation(
        dictionary: *const c_void,
        rect: *mut CGRectRaw,
    ) -> bool;
}

/// Reads the changed rectangles `ScreenCaptureKit` attached to a frame.
///
/// Returns [`FrameDamage::Unknown`] whenever the attachment is missing or does
/// not parse, which makes the caller treat the whole frame as changed. That is
/// the safe direction: claiming a moving desktop is still costs bandwidth,
/// while claiming a still desktop moved costs the person their picture.
///
/// The rectangles are documented as the union of what was redrawn and moved,
/// in pixels, and are available from macOS 12.3.
///
/// # Safety
///
/// `sample_buffer` must be a live sample buffer delivered by
/// `ScreenCaptureKit`.
unsafe fn read_frame_damage(sample_buffer: &CMSampleBuffer) -> FrameDamage {
    // SAFETY: the caller guarantees a live sample buffer. `false` means do not
    // manufacture an attachments array that the frame did not come with.
    let Some(attachments) = (unsafe { sample_buffer.sample_attachments_array(false) }) else {
        return FrameDamage::Unknown;
    };
    let attachments = std::ptr::from_ref(&*attachments).cast::<c_void>();
    // SAFETY: `attachments` is the live array returned above.
    if unsafe { CFArrayGetCount(attachments) } == 0 {
        return FrameDamage::Unknown;
    }
    // SAFETY: index 0 is in bounds per the count check.
    let info = unsafe { CFArrayGetValueAtIndex(attachments, 0) };
    if info.is_null() {
        return FrameDamage::Unknown;
    }
    // SAFETY: `SCStreamFrameInfoDirtyRects` is a process-lifetime constant
    // exported by ScreenCaptureKit; taking its address reads nothing.
    let key = unsafe { std::ptr::from_ref(SCStreamFrameInfoDirtyRects).cast::<c_void>() };
    // SAFETY: `info` is the attachment dictionary for sample 0 and `key` is a
    // process-lifetime ScreenCaptureKit constant.
    let value = unsafe { CFDictionaryGetValue(info, key) };
    if value.is_null() {
        return FrameDamage::Unknown;
    }
    // SAFETY: `SCStreamFrameInfoDirtyRects` is documented to carry a CFArray of
    // CGRect dictionaries.
    let count = unsafe { CFArrayGetCount(value) };
    if count < 0 {
        return FrameDamage::Unknown;
    }
    let mut damage = Vec::with_capacity(count.unsigned_abs());
    for index in 0..count {
        // SAFETY: `index` is in bounds by construction.
        let entry = unsafe { CFArrayGetValueAtIndex(value, index) };
        if entry.is_null() {
            return FrameDamage::Unknown;
        }
        // SAFETY: each entry is a CGRect serialised as a CFDictionary.
        let Some(rect) = (unsafe { rect_from_dictionary(entry) }) else {
            return FrameDamage::Unknown;
        };
        damage.push(rect);
    }
    FrameDamage::Rects(damage)
}

/// Converts one `CGRect` dictionary into whole pixels.
///
/// Rounds outward rather than to nearest: a rectangle that covers part of a
/// pixel has changed that pixel, and rounding inward would leave a changed
/// edge looking clean.
///
/// # Safety
///
/// `entry` must be a `CGRect` dictionary representation.
unsafe fn rect_from_dictionary(entry: *const c_void) -> Option<DamageRect> {
    let mut rect = CGRectRaw::default();
    // SAFETY: the caller guarantees a CGRect dictionary representation, and
    // `rect` is a live, correctly laid out output.
    if !unsafe { CGRectMakeWithDictionaryRepresentation(entry, &raw mut rect) } {
        return None;
    }
    Some(pixel_rect(rect))
}

/// Rounds a `CGRect` outward to whole pixels.
///
/// Separated from the CoreFoundation reading so the arithmetic can be tested
/// without a live capture session.
fn pixel_rect(rect: CGRectRaw) -> DamageRect {
    if !rect.origin_x.is_finite()
        || !rect.origin_y.is_finite()
        || !rect.width.is_finite()
        || !rect.height.is_finite()
        || rect.width <= 0.0
        || rect.height <= 0.0
    {
        return DamageRect {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        };
    }
    let left = rect.origin_x.floor().max(0.0);
    let top = rect.origin_y.floor().max(0.0);
    let right = (rect.origin_x + rect.width).ceil().max(left);
    let bottom = (rect.origin_y + rect.height).ceil().max(top);
    DamageRect {
        x: whole_pixels(left),
        y: whole_pixels(top),
        width: whole_pixels(right - left),
        height: whole_pixels(bottom - top),
    }
}

/// Narrows an already non-negative, already rounded coordinate to a pixel count.
///
/// Saturating rather than wrapping. A coordinate larger than any surface is
/// nonsense whatever produced it, and clamping it marks too much as changed,
/// which costs bandwidth; wrapping it would mark too little, which costs the
/// person the part of their screen that moved.
fn whole_pixels(value: f64) -> u32 {
    if !value.is_finite() || value <= 0.0 {
        return 0;
    }
    if value >= f64::from(u32::MAX) {
        return u32::MAX;
    }
    // The bounds above leave a finite, non-negative value inside u32.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    {
        value as u32
    }
}

/// How often `ScreenCaptureKit` actually hands this process a frame.
///
/// The consumer's own wait already had a number, and it was the largest in the
/// pipeline. What it could not say was which side of the handoff produced it:
/// a source pacing at half the requested rate and a consumer taking every
/// second frame are indistinguishable downstream, and they need opposite
/// fixes. This counts arrivals at the callback, before any queue.
///
/// Recorded with relaxed atomics because this runs on `ScreenCaptureKit`'s
/// delivery callback, where a lock or an allocation would stall the compositor
/// for every application on the machine.
#[derive(Debug, Default)]
struct SourceCadence {
    callbacks: AtomicU64,
    /// Nanoseconds since [`cadence_epoch`] at the previous callback.
    last_ns: AtomicU64,
    /// Sum of gaps between consecutive callbacks, in nanoseconds.
    interval_sum_ns: AtomicU64,
    /// Longest single gap, in nanoseconds.
    interval_max_ns: AtomicU64,
    /// Total time spent inside the delivery callback, in nanoseconds.
    ///
    /// `ScreenCaptureKit` paces against its own schedule, and a handler that
    /// overruns one interval gets offered the next slot instead of the one it
    /// missed — which halves the rate rather than shaving it. Measuring the
    /// handler is how that explanation is confirmed or dismissed.
    work_sum_ns: AtomicU64,
    /// Longest single stay inside the delivery callback, in nanoseconds.
    work_max_ns: AtomicU64,
    /// Total age of frames on arrival, in nanoseconds.
    ///
    /// The gap between the timestamp the compositor stamped a frame with and
    /// the moment it reached this process. This is the one part of the latency
    /// budget the host cannot see any other way: everything downstream is ours
    /// to measure, but a frame that is already old when it arrives is late
    /// before this host has done anything at all.
    arrival_age_sum_ns: AtomicU64,
    /// Longest single arrival age, in nanoseconds.
    arrival_age_max_ns: AtomicU64,
    /// Frames whose timestamp could be read at all.
    arrival_age_samples: AtomicU64,
}

/// Monotonic base for [`SourceCadence`], so callback times fit in an atomic.
fn cadence_epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

impl SourceCadence {
    /// Records one frame arriving from `ScreenCaptureKit`.
    fn observe(&self) {
        // `u64` nanoseconds overflow after 584 years of uptime.
        #[allow(clippy::cast_possible_truncation)]
        let now_ns = cadence_epoch().elapsed().as_nanos() as u64;
        let previous = self.last_ns.swap(now_ns, Ordering::Relaxed);
        if self.callbacks.fetch_add(1, Ordering::Relaxed) > 0 {
            let gap = now_ns.saturating_sub(previous);
            self.interval_sum_ns.fetch_add(gap, Ordering::Relaxed);
            self.interval_max_ns.fetch_max(gap, Ordering::Relaxed);
        }
    }

    /// Records how old a frame already was when it reached this process.
    fn observe_arrival_age(&self, age: Duration) {
        // `u64` nanoseconds overflow after 584 years.
        #[allow(clippy::cast_possible_truncation)]
        let nanos = age.as_nanos() as u64;
        self.arrival_age_sum_ns.fetch_add(nanos, Ordering::Relaxed);
        self.arrival_age_max_ns.fetch_max(nanos, Ordering::Relaxed);
        self.arrival_age_samples.fetch_add(1, Ordering::Relaxed);
    }

    /// Records how long the delivery callback itself took.
    fn record_work(&self, elapsed: Duration) {
        // `u64` nanoseconds overflow after 584 years.
        #[allow(clippy::cast_possible_truncation)]
        let nanos = elapsed.as_nanos() as u64;
        self.work_sum_ns.fetch_add(nanos, Ordering::Relaxed);
        self.work_max_ns.fetch_max(nanos, Ordering::Relaxed);
    }

    /// Callback count, mean gap and worst gap in milliseconds.
    fn snapshot(&self) -> SourceCadenceSnapshot {
        let callbacks = self.callbacks.load(Ordering::Relaxed);
        let gaps = callbacks.saturating_sub(1);
        let sum_ns = self.interval_sum_ns.load(Ordering::Relaxed);
        SourceCadenceSnapshot {
            callbacks,
            mean_interval_ms: if gaps == 0 {
                0.0
            } else {
                nanos_to_millis(sum_ns) / f64::from(u32::try_from(gaps).unwrap_or(u32::MAX))
            },
            max_interval_ms: nanos_to_millis(self.interval_max_ns.load(Ordering::Relaxed)),
            mean_work_ms: if callbacks == 0 {
                0.0
            } else {
                nanos_to_millis(self.work_sum_ns.load(Ordering::Relaxed))
                    / f64::from(u32::try_from(callbacks).unwrap_or(u32::MAX))
            },
            max_work_ms: nanos_to_millis(self.work_max_ns.load(Ordering::Relaxed)),
            mean_arrival_age_ms: mean_millis(
                self.arrival_age_sum_ns.load(Ordering::Relaxed),
                self.arrival_age_samples.load(Ordering::Relaxed),
            ),
            max_arrival_age_ms: nanos_to_millis(self.arrival_age_max_ns.load(Ordering::Relaxed)),
        }
    }
}

/// How old a sample already was when it reached this process.
///
/// `ScreenCaptureKit` stamps each frame with a presentation time on the host
/// clock, so comparing it against that same clock gives the compositor's own
/// latency: the part of the budget that is spent before this host is told a
/// frame exists. Every other stage can be timed from inside; this one cannot.
///
/// Returns `None` when either time is unreadable or the arithmetic would be
/// nonsense, because a made-up latency is worse than a missing one.
///
/// # Safety
///
/// `sample_buffer` must be a live `CMSampleBuffer`.
unsafe fn sample_arrival_age(sample_buffer: &CMSampleBuffer) -> Option<Duration> {
    // SAFETY: the caller guarantees a live sample buffer.
    let stamped = unsafe { sample_buffer.presentation_time_stamp() };
    // SAFETY: CoreMedia's host clock is a process-lifetime singleton.
    let clock = unsafe { objc2_core_media::CMClock::host_time_clock() };
    // SAFETY: the host time clock is a process-lifetime CoreMedia singleton.
    let now = unsafe { clock.time() };
    let seconds = |time: objc2_core_media::CMTime| -> Option<f64> {
        // A zero or negative timescale means the value is invalid, not slow.
        if time.timescale <= 0 {
            return None;
        }
        Some(as_f64(time.value) / f64::from(time.timescale))
    };
    let age = seconds(now)? - seconds(stamped)?;
    // A frame stamped in the future, or absurdly far in the past, means the
    // two values were not on the same clock after all.
    if !(0.0..=1.0).contains(&age) {
        return None;
    }
    Some(Duration::from_secs_f64(age))
}

/// `i64` to `f64` for clock arithmetic, where the values are small.
fn as_f64(value: i64) -> f64 {
    // Presentation timestamps are nanosecond counts since boot, far below the
    // 2^53 where `f64` stops holding every integer.
    #[allow(clippy::cast_precision_loss)]
    {
        value as f64
    }
}

/// Mean of a nanosecond total over a sample count, in milliseconds.
fn mean_millis(total_ns: u64, samples: u64) -> f64 {
    if samples == 0 {
        return 0.0;
    }
    nanos_to_millis(total_ns) / f64::from(u32::try_from(samples).unwrap_or(u32::MAX))
}

/// Converts a nanosecond count to milliseconds without an integer cast trap.
fn nanos_to_millis(nanos: u64) -> f64 {
    // A f64 holds every integer below 2^53; nanosecond counts here are session
    // lifetimes, far under that.
    #[allow(clippy::cast_precision_loss)]
    {
        nanos as f64 / 1_000_000.0
    }
}

/// What [`CaptureSession::source_cadence`] reports.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SourceCadenceSnapshot {
    /// Frames `ScreenCaptureKit` delivered to the callback.
    pub callbacks: u64,
    /// Mean gap between consecutive deliveries.
    pub mean_interval_ms: f64,
    /// Longest gap between consecutive deliveries.
    pub max_interval_ms: f64,
    /// Mean time spent inside the delivery callback.
    pub mean_work_ms: f64,
    /// Longest single stay inside the delivery callback.
    pub max_work_ms: f64,
    /// Mean age of a frame at the moment it reached this process.
    pub mean_arrival_age_ms: f64,
    /// Worst age of a frame at the moment it reached this process.
    pub max_arrival_age_ms: f64,
}

/// Instance state for the stream output object.
struct FrameSink {
    frames: SyncSender<CapturedFrame>,
    stopped: SyncSender<String>,
    /// How often the source actually delivers, measured at the callback.
    cadence: Arc<SourceCadence>,
    /// Frames the consumer was too slow to take.
    ///
    /// Shedding is correct here, but shedding silently is not: a session
    /// dropping half its frames looked exactly like a still desktop, because
    /// both produce a low delivered rate and neither said why. An atomic is
    /// the whole cost — this runs on `ScreenCaptureKit`'s callback, where a
    /// lock or an allocation would stall the compositor for every application
    /// on the machine.
    dropped: Arc<AtomicU64>,
    /// Whether the first frame's colour attachments have been reported.
    colour_reported: std::sync::atomic::AtomicBool,
}

define_class!(
    // SAFETY:
    // - `NSObject` has no subclassing requirements.
    // - `StreamOutput` does not implement `Drop`.
    #[unsafe(super(NSObject))]
    #[name = "ArcenPierStreamOutput"]
    #[ivars = FrameSink]
    struct StreamOutput;

    unsafe impl NSObjectProtocol for StreamOutput {}

    unsafe impl SCStreamDelegate for StreamOutput {
        #[unsafe(method(stream:didStopWithError:))]
        #[allow(non_snake_case)]
        unsafe fn stream_didStopWithError(&self, _stream: &SCStream, error: &NSError) {
            drop(
                self.ivars()
                    .stopped
                    .try_send(error.localizedDescription().to_string()),
            );
        }
    }

    unsafe impl SCStreamOutput for StreamOutput {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        unsafe fn stream_did_output_sample_buffer(
            &self,
            _stream: &SCStream,
            sample_buffer: &CMSampleBuffer,
            output_type: SCStreamOutputType,
        ) {
            if output_type != SCStreamOutputType::Screen {
                return;
            }
            // Counted before anything can reject the frame: this is the rate
            // the source offers, which is a different claim from the rate the
            // consumer manages to take.
            let started = Instant::now();
            self.ivars().cadence.observe();
            // How late the frame already was on arrival. Measured against the
            // compositor's own clock, so it includes everything ScreenCaptureKit
            // did before this process was told anything.
            if let Some(age) = unsafe { sample_arrival_age(sample_buffer) } {
                self.ivars().cadence.observe_arrival_age(age);
            }
            // SAFETY: ScreenCaptureKit guarantees the buffer is live for the
            // duration of this callback.
            let Some(frame) = (unsafe { CapturedFrame::from_sample_buffer(sample_buffer) }) else {
                self.ivars().cadence.record_work(started.elapsed());
                return;
            };
            if !self.ivars().colour_reported.swap(true, Ordering::Relaxed) {
                // SAFETY: the buffer is live for this callback.
                unsafe { report_surface_colour(sample_buffer, &frame) };
            }
            // Dropping a frame is correct back-pressure here. Blocking would
            // stall the compositor for every application on the machine.
            // Counted, so a shedding session can be told from an idle one.
            if self.ivars().frames.try_send(frame).is_err() {
                self.ivars().dropped.fetch_add(1, Ordering::Relaxed);
            }
            self.ivars().cadence.record_work(started.elapsed());
        }
    }
);

/// Why capture could not start or continue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "detail")]
pub enum CaptureError {
    /// `WindowServer` did not answer the shareable-content query in time.
    ContentTimeout,
    /// `ScreenCaptureKit` refused the shareable-content query. Screen Recording
    /// consent is the usual cause.
    ContentUnavailable(String),
    /// The requested display is not currently capturable.
    DisplayNotFound(u32),
    /// `ScreenCaptureKit` refused to attach the frame consumer.
    OutputRejected(String),
    /// The stream did not start.
    StartFailed(String),
    /// No frame arrived within the caller's deadline.
    FrameTimeout,
    /// The stream started but never produced the initial image.
    FirstFrameTimeout,
    /// The stream ended and will produce no further frames.
    StreamEnded,
    /// `ScreenCaptureKit` reported that the stream stopped.
    StreamStopped(String),
}

impl std::fmt::Display for CaptureError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ContentTimeout => formatter.write_str("timed out querying shareable content"),
            Self::ContentUnavailable(detail) => {
                write!(formatter, "shareable content unavailable: {detail}")
            }
            Self::DisplayNotFound(id) => write!(formatter, "display {id} is not capturable"),
            Self::OutputRejected(detail) => {
                write!(formatter, "stream output rejected: {detail}")
            }
            Self::StartFailed(detail) => write!(formatter, "stream start failed: {detail}"),
            Self::FrameTimeout => formatter.write_str("timed out waiting for a frame"),
            Self::FirstFrameTimeout => formatter.write_str("timed out waiting for the first frame"),
            Self::StreamEnded => formatter.write_str("capture stream ended"),
            Self::StreamStopped(detail) => write!(formatter, "capture stream stopped: {detail}"),
        }
    }
}

impl std::error::Error for CaptureError {}

fn describe(error: *mut NSError) -> String {
    if error.is_null() {
        return "no error detail".to_owned();
    }
    // SAFETY: ScreenCaptureKit hands back a live error for the callback.
    let error = unsafe { &*error };
    error.localizedDescription().to_string()
}

/// Queries the displays and windows this process may capture.
///
/// # Errors
///
/// Returns [`CaptureError::ContentUnavailable`] when `ScreenCaptureKit` refuses
/// the query, which is what a missing Screen Recording grant looks like, and
/// [`CaptureError::ContentTimeout`] when `WindowServer` does not answer.
pub fn shareable_content() -> Result<Retained<SCShareableContent>, CaptureError> {
    let (sender, receiver) = sync_channel::<Result<Retained<SCShareableContent>, String>>(1);
    let handler = RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            let result = if content.is_null() {
                Err(describe(error))
            } else {
                // SAFETY: ScreenCaptureKit hands back a live +0 object.
                Ok(unsafe { Retained::retain(content) }
                    .map_or_else(|| Err("shareable content vanished".to_owned()), Ok))
                .and_then(|inner| inner)
            };
            drop(sender.try_send(result));
        },
    );
    // SAFETY: the block outlives the call because `recv_timeout` blocks here.
    unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&handler) };
    match receiver.recv_timeout(CONTENT_TIMEOUT) {
        Ok(Ok(content)) => Ok(content),
        Ok(Err(detail)) => Err(CaptureError::ContentUnavailable(detail)),
        Err(RecvTimeoutError::Timeout) => Err(CaptureError::ContentTimeout),
        Err(RecvTimeoutError::Disconnected) => Err(CaptureError::ContentUnavailable(
            "completion handler dropped".to_owned(),
        )),
    }
}

/// A running `ScreenCaptureKit` stream.
pub struct CaptureSession {
    stream: Retained<SCStream>,
    output: Retained<StreamOutput>,
    frames: Receiver<CapturedFrame>,
    stopped: Receiver<String>,
    config: CaptureConfig,
    dropped: Arc<AtomicU64>,
    cadence: Arc<SourceCadence>,
    stop_requested: std::sync::atomic::AtomicBool,
}

impl std::fmt::Debug for CaptureSession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CaptureSession")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl CaptureSession {
    /// Frames `ScreenCaptureKit` produced that the consumer could not take.
    ///
    /// Reported rather than inferred: a low delivered rate on a damage-driven
    /// capture means either a still desktop or a host that cannot keep up, and
    /// only this number tells the two apart.
    #[must_use]
    pub fn dropped_frames(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// How often `ScreenCaptureKit` handed this session a frame.
    ///
    /// Measured at the delivery callback, so it is independent of whatever the
    /// consumer then does with it. Comparing this against the consumer's own
    /// wait is what separates a slow source from a slow consumer.
    #[must_use]
    pub fn source_cadence(&self) -> SourceCadenceSnapshot {
        self.cadence.snapshot()
    }

    /// Starts capturing the display named by `config`.
    ///
    /// # Errors
    ///
    /// Returns a [`CaptureError`] when consent is missing, the display is not
    /// capturable, or `ScreenCaptureKit` refuses the stream.
    #[allow(clippy::too_many_lines)]
    pub fn start(config: CaptureConfig) -> Result<Self, CaptureError> {
        let content = shareable_content()?;
        // SAFETY: reading properties of a live shareable-content object.
        let displays = unsafe { content.displays() };
        let display = (0..displays.count())
            .map(|index| displays.objectAtIndex(index))
            .find(|display| unsafe { display.displayID() } == config.display_id)
            .ok_or(CaptureError::DisplayNotFound(config.display_id))?;

        let excluded = NSArray::new();
        // SAFETY: both arguments are live and the initializer is the documented one.
        let filter = unsafe {
            SCContentFilter::initWithDisplay_excludingWindows(
                SCContentFilter::alloc(),
                &display,
                &excluded,
            )
        };

        // SAFETY: allocating a fresh configuration object has no preconditions.
        let stream_config = unsafe { SCStreamConfiguration::new() };
        let can_set_dynamic_range = stream_config.respondsToSelector(sel!(setCaptureDynamicRange:));
        if config.dynamic_range.requires_screen_capture_kit_setter() && !can_set_dynamic_range {
            return Err(CaptureError::StartFailed(
                "HDR capture requires macOS 15.0 or later".to_owned(),
            ));
        }
        // SAFETY: plain property writes on a fresh configuration object.
        unsafe {
            stream_config.setWidth(config.width);
            stream_config.setHeight(config.height);
            stream_config.setPixelFormat(config.pixel_format.os_type());
            stream_config.setShowsCursor(config.shows_cursor);
            stream_config.setQueueDepth(isize::try_from(SOURCE_QUEUE_DEPTH).unwrap_or(3));
            stream_config.setMinimumFrameInterval(objc2_core_media::CMTime {
                value: 1,
                timescale: i32::try_from(config.fps.max(1)).unwrap_or(60),
                flags: objc2_core_media::CMTimeFlags(1),
                epoch: 0,
            });
            if can_set_dynamic_range {
                // A measurement lever: which HDR reference the capture uses is
                // being chosen by what each puts SDR white at.
                let dynamic_range = if config.dynamic_range == CaptureDynamicRange::HdrLocalDisplay
                    && std::env::var("ARCEN_HDR_CANONICAL").as_deref() == Ok("1")
                {
                    CaptureDynamicRange::HdrCanonicalDisplay
                } else {
                    config.dynamic_range
                };
                stream_config.setCaptureDynamicRange(
                    objc2_screen_capture_kit::SCCaptureDynamicRange(dynamic_range.raw()),
                );
            }

            // Stated, not assumed. ScreenCaptureKit's header is explicit: "If
            // not set the output buffer uses the same color space as the
            // display." Every frame this host sends is labelled BT.709 on the
            // wire, so leaving it unset meant a Display-P3 desktop was
            // captured in P3 and announced as BT.709 — the client then renders
            // P3 primaries as if they were BT.709 and the picture comes out
            // oversaturated, with nothing failing anywhere.
            let hdr = config.dynamic_range.requires_screen_capture_kit_setter();
            stream_config.setColorSpaceName(if hdr {
                objc2_core_graphics::kCGColorSpaceITUR_2100_PQ
            } else {
                objc2_core_graphics::kCGColorSpaceSRGB
            });

            // Only meaningful for the biplanar YCbCr formats; the header says
            // so. Setting it on a packed format is a claim about a conversion
            // that is not happening.
            if config.pixel_format.is_ycbcr() {
                stream_config.setColorMatrix(if hdr {
                    objc2_core_video::kCVImageBufferYCbCrMatrix_ITU_R_2020
                } else {
                    objc2_core_graphics::kCGDisplayStreamYCbCrMatrix_ITU_R_709_2
                });
            }
        }

        let (sender, frames) = sync_channel(FRAME_QUEUE_DEPTH);
        let (stopped_sender, stopped) = sync_channel(1);
        let dropped = Arc::new(AtomicU64::new(0));
        let cadence = Arc::new(SourceCadence::default());
        let output = StreamOutput::alloc().set_ivars(FrameSink {
            frames: sender,
            stopped: stopped_sender,
            cadence: Arc::clone(&cadence),
            dropped: Arc::clone(&dropped),
            colour_reported: std::sync::atomic::AtomicBool::new(false),
        });
        let output: Retained<StreamOutput> = unsafe { objc2::msg_send![super(output), init] };
        let protocol_delegate: &ProtocolObject<dyn SCStreamDelegate> =
            ProtocolObject::from_ref(&*output);

        // SAFETY: filter and configuration are live; the Pier handles stream
        // failure through the delegate passed below.
        let stream = unsafe {
            SCStream::initWithFilter_configuration_delegate(
                SCStream::alloc(),
                &filter,
                &stream_config,
                Some(protocol_delegate),
            )
        };

        let queue = dispatch2::DispatchQueue::new("com.arcen.pier.capture", None);
        let protocol_output = ProtocolObject::from_ref(&*output);
        // SAFETY: the output object is retained by the stream for its lifetime.
        unsafe {
            SCStream::addStreamOutput_type_sampleHandlerQueue_error(
                &stream,
                protocol_output,
                SCStreamOutputType::Screen,
                Some(&queue),
            )
        }
        .map_err(|error| CaptureError::OutputRejected(error.localizedDescription().to_string()))?;

        let (started_tx, started_rx) = sync_channel::<Option<String>>(1);
        let start_handler = RcBlock::new(move |error: *mut NSError| {
            let detail = if error.is_null() {
                None
            } else {
                Some(describe(error))
            };
            drop(started_tx.try_send(detail));
        });
        // SAFETY: the block outlives the call because we block on the channel.
        unsafe { stream.startCaptureWithCompletionHandler(Some(&start_handler)) };
        match started_rx.recv_timeout(CONTENT_TIMEOUT) {
            Ok(None) => {}
            Ok(Some(detail)) => return Err(CaptureError::StartFailed(detail)),
            Err(RecvTimeoutError::Timeout) => {
                return Err(CaptureError::StartFailed("start timed out".to_owned()));
            }
            Err(RecvTimeoutError::Disconnected) => {
                return Err(CaptureError::StartFailed(
                    "start handler dropped".to_owned(),
                ));
            }
        }

        Ok(Self {
            stream,
            output,
            frames,
            stopped,
            config,
            dropped,
            cadence,
            stop_requested: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// Returns the plan this session was started with.
    #[must_use]
    pub const fn config(&self) -> CaptureConfig {
        self.config
    }

    /// Waits for the next frame.
    ///
    /// # Errors
    ///
    /// Returns [`CaptureError::FrameTimeout`] if no frame arrives in time, and
    /// [`CaptureError::StreamEnded`] once the stream is finished.
    pub fn next_frame(&self, timeout: Duration) -> Result<CapturedFrame, CaptureError> {
        match self.stopped.try_recv() {
            Ok(detail) => return Err(CaptureError::StreamStopped(detail)),
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => return Err(CaptureError::StreamEnded),
        }
        match self.frames.recv_timeout(timeout) {
            Ok(frame) => Ok(frame),
            Err(RecvTimeoutError::Timeout) => match self.stopped.try_recv() {
                Ok(detail) => Err(CaptureError::StreamStopped(detail)),
                Err(TryRecvError::Empty) => Err(CaptureError::FrameTimeout),
                Err(TryRecvError::Disconnected) => Err(CaptureError::StreamEnded),
            },
            Err(RecvTimeoutError::Disconnected) => match self.stopped.try_recv() {
                Ok(detail) => Err(CaptureError::StreamStopped(detail)),
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => {
                    Err(CaptureError::StreamEnded)
                }
            },
        }
    }

    /// Stops the stream, waiting briefly for `WindowServer` to acknowledge.
    pub fn stop(&self) {
        if self
            .stop_requested
            .swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            return;
        }
        let (done_tx, done_rx) = sync_channel::<()>(1);
        let handler = RcBlock::new(move |_error: *mut NSError| {
            let _ = done_tx.try_send(());
        });
        // SAFETY: the block outlives the call because we block on the channel.
        unsafe { self.stream.stopCaptureWithCompletionHandler(Some(&handler)) };
        let _ = done_rx.recv_timeout(CONTENT_TIMEOUT);
        let protocol_output = ProtocolObject::from_ref(&*self.output);
        // SAFETY: the output object is the same live object added to this stream.
        let _ = unsafe {
            SCStream::removeStreamOutput_type_error(
                &self.stream,
                protocol_output,
                SCStreamOutputType::Screen,
            )
        };
    }
}

impl Drop for CaptureSession {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn only_ycbcr_layouts_carry_a_conversion_matrix() {
        // ScreenCaptureKit documents colorMatrix as applying to the 4:2:0
        // biplanar formats. Asserting it on packed BGRA claims a YCbCr
        // conversion that is not happening.
        assert!(!CapturePixelFormat::Bgra8.is_ycbcr());
        assert!(CapturePixelFormat::Nv12VideoRange.is_ycbcr());
        assert!(CapturePixelFormat::Nv12TenBitVideoRange.is_ycbcr());
        assert!(CapturePixelFormat::FourFourFourTenBit.is_ycbcr());
    }
    use super::*;

    #[test]
    fn pixel_formats_round_trip_through_os_type() {
        for format in [
            CapturePixelFormat::Bgra8,
            CapturePixelFormat::Nv12VideoRange,
            CapturePixelFormat::Nv12TenBitVideoRange,
            CapturePixelFormat::FourFourFourTenBit,
        ] {
            assert_eq!(
                CapturePixelFormat::from_os_type(format.os_type()),
                Some(format)
            );
        }
    }

    #[test]
    fn capture_session_owns_the_callback_output() {
        fn output_owner(session: &CaptureSession) -> &Retained<StreamOutput> {
            &session.output
        }

        let projection: fn(&CaptureSession) -> &Retained<StreamOutput> = output_owner;
        std::hint::black_box(projection);
    }

    #[test]
    fn os_types_match_core_video_fourccs() {
        assert_eq!(
            CapturePixelFormat::Nv12VideoRange.os_type(),
            u32::from_be_bytes(*b"420v")
        );
        assert_eq!(
            CapturePixelFormat::FourFourFourTenBit.os_type(),
            u32::from_be_bytes(*b"x444")
        );
        assert_eq!(
            CapturePixelFormat::Bgra8.os_type(),
            u32::from_be_bytes(*b"BGRA")
        );
    }

    #[test]
    fn only_ten_bit_layouts_are_high_precision() {
        assert!(!CapturePixelFormat::Bgra8.is_high_precision());
        assert!(!CapturePixelFormat::Nv12VideoRange.is_high_precision());
        assert!(CapturePixelFormat::Nv12TenBitVideoRange.is_high_precision());
        assert!(CapturePixelFormat::FourFourFourTenBit.is_high_precision());
    }

    #[test]
    fn only_hdr_dynamic_ranges_require_the_macos_15_setter() {
        assert!(!CaptureDynamicRange::Sdr.requires_screen_capture_kit_setter());
        assert!(CaptureDynamicRange::HdrLocalDisplay.requires_screen_capture_kit_setter());
        assert!(CaptureDynamicRange::HdrCanonicalDisplay.requires_screen_capture_kit_setter());
    }

    #[test]
    fn unknown_os_type_is_not_a_pier_capture_format() {
        assert_eq!(CapturePixelFormat::from_os_type(0), None);
    }
}

#[cfg(test)]
mod damage_tests {
    use super::{CGRectRaw, DamageRect, FrameDamage, pixel_rect};

    fn rect(origin_x: f64, origin_y: f64, width: f64, height: f64) -> CGRectRaw {
        CGRectRaw {
            origin_x,
            origin_y,
            width,
            height,
        }
    }

    #[test]
    fn a_whole_pixel_rectangle_survives_unchanged() {
        assert_eq!(
            pixel_rect(rect(10.0, 20.0, 30.0, 40.0)),
            DamageRect {
                x: 10,
                y: 20,
                width: 30,
                height: 40
            }
        );
    }

    #[test]
    fn a_partial_pixel_rectangle_rounds_outward() {
        // A rectangle covering part of a pixel has changed that pixel. Rounding
        // inward would leave a changed edge looking clean, which is the one
        // mistake damage tracking must not make.
        assert_eq!(
            pixel_rect(rect(10.4, 20.6, 30.2, 40.1)),
            DamageRect {
                x: 10,
                y: 20,
                width: 31,
                height: 41
            }
        );
    }

    #[test]
    fn a_negative_origin_is_clamped_to_the_surface() {
        let clamped = pixel_rect(rect(-5.0, -8.0, 20.0, 20.0));
        assert_eq!(clamped.x, 0);
        assert_eq!(clamped.y, 0);
    }

    #[test]
    fn a_degenerate_rectangle_covers_nothing() {
        assert_eq!(pixel_rect(rect(4.0, 4.0, 0.0, 10.0)).width, 0);
        assert_eq!(pixel_rect(rect(4.0, 4.0, 10.0, -1.0)).height, 0);
    }

    #[test]
    fn a_non_finite_rectangle_covers_nothing_rather_than_panicking() {
        assert_eq!(pixel_rect(rect(f64::NAN, 0.0, 10.0, 10.0)).width, 0);
        assert_eq!(pixel_rect(rect(0.0, 0.0, f64::INFINITY, 10.0)).width, 0);
    }

    #[test]
    fn unknown_damage_is_not_treated_as_clean() {
        // The whole point of the distinction: being told nothing is different
        // from being told nothing changed.
        assert!(!FrameDamage::Unknown.is_known_clean());
        assert!(FrameDamage::Rects(Vec::new()).is_known_clean());
        assert!(
            !FrameDamage::Rects(vec![DamageRect {
                x: 0,
                y: 0,
                width: 1,
                height: 1
            }])
            .is_known_clean()
        );
    }

    #[test]
    fn superseded_damage_merges_with_unknown_dominating() {
        let mut newer = FrameDamage::Rects(Vec::new());
        newer.merge_superseded(FrameDamage::Rects(vec![DamageRect {
            x: 1,
            y: 2,
            width: 3,
            height: 4,
        }]));
        assert_eq!(
            newer,
            FrameDamage::Rects(vec![DamageRect {
                x: 1,
                y: 2,
                width: 3,
                height: 4,
            }])
        );

        newer.merge_superseded(FrameDamage::Unknown);
        assert_eq!(newer, FrameDamage::Unknown);
    }
}
