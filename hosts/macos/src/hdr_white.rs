//! Moves SDR white inside an HDR capture to where Arcen's PQ contract puts it.
//!
//! `ScreenCaptureKit` delivers PQ with SDR white near 100 nits; Arcen's
//! contract (`arcen_media::video::pq_white`) is BT.2408's 203. The difference
//! is a gain in linear light, which Core Image applies on the GPU: it decodes
//! the surface's PQ, scales, and re-encodes PQ into a fresh 4:4:4 surface for
//! the encoder. The rule and its reference implementation are shared; this
//! module is the macOS way of running it.
//!
//! Measured before it was written: Core Image mapped code 520 (100 nits) to
//! 594 (203 nits), kept neutral chroma neutral, and left a zero gain exact.

use std::ffi::c_void;
use std::sync::Arc;

use apple_cf::iosurface::IOSurface;
use objc2::msg_send;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject};
use objc2_foundation::{NSNumber, NSString};

use crate::capture::CapturedFrame;

#[link(name = "CoreImage", kind = "framework")]
unsafe extern "C" {
    static kCIFormatRGBAh: i32;
    static kCIContextWorkingColorSpace: *const c_void;
    static kCIContextWorkingFormat: *const c_void;
    static kCIContextCacheIntermediates: *const c_void;
}

#[link(name = "CoreVideo", kind = "framework")]
unsafe extern "C" {
    fn CVPixelBufferCreateWithIOSurface(
        allocator: *const c_void,
        surface: *mut c_void,
        attributes: *const c_void,
        out: *mut *mut c_void,
    ) -> i32;
    fn CVPixelBufferPoolCreate(
        allocator: *const c_void,
        pool_attributes: *const c_void,
        buffer_attributes: *const c_void,
        out: *mut *mut c_void,
    ) -> i32;
    fn CVPixelBufferPoolCreatePixelBuffer(
        allocator: *const c_void,
        pool: *mut c_void,
        out: *mut *mut c_void,
    ) -> i32;
    fn CVPixelBufferGetIOSurface(buffer: *mut c_void) -> *mut c_void;
    fn CVPixelBufferRelease(buffer: *mut c_void);
    fn CVPixelBufferPoolRelease(pool: *mut c_void);
    fn CVBufferSetAttachment(
        buffer: *mut c_void,
        key: *const c_void,
        value: *const c_void,
        mode: u32,
    );
    static kCVPixelBufferIOSurfacePropertiesKey: *const c_void;
    static kCVPixelBufferPixelFormatTypeKey: *const c_void;
    static kCVPixelBufferWidthKey: *const c_void;
    static kCVPixelBufferHeightKey: *const c_void;
    static kCVImageBufferTransferFunctionKey: *const c_void;
    static kCVImageBufferColorPrimariesKey: *const c_void;
    static kCVImageBufferYCbCrMatrixKey: *const c_void;
    static kCVImageBufferTransferFunction_SMPTE_ST_2084_PQ: *const c_void;
    static kCVImageBufferColorPrimaries_ITU_R_2020: *const c_void;
    static kCVImageBufferYCbCrMatrix_ITU_R_2020: *const c_void;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRetain(value: *const c_void) -> *const c_void;
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGColorSpaceCreateWithName(name: *const c_void) -> *mut c_void;
    fn CGColorSpaceRelease(space: *mut c_void);
    static kCGColorSpaceITUR_2100_PQ: *const c_void;
    static kCGColorSpaceExtendedLinearITUR_2020: *const c_void;
}

/// `CVBufferRef` as Objective-C method signatures spell it.
#[repr(C)]
struct CvBuffer {
    _private: [u8; 0],
}

// SAFETY: matches `^{__CVBuffer=}`, the encoding Core Image's methods declare.
unsafe impl objc2::RefEncode for CvBuffer {
    const ENCODING_REF: objc2::Encoding =
        objc2::Encoding::Pointer(&objc2::Encoding::Struct("__CVBuffer", &[]));
}

/// `CGColorSpaceRef` as Objective-C method signatures spell it.
#[repr(C)]
struct CgColorSpace {
    _private: [u8; 0],
}

// SAFETY: matches `^{CGColorSpace=}`.
unsafe impl objc2::RefEncode for CgColorSpace {
    const ENCODING_REF: objc2::Encoding =
        objc2::Encoding::Pointer(&objc2::Encoding::Struct("CGColorSpace", &[]));
}

/// `kCVAttachmentMode_ShouldPropagate`.
const SHOULD_PROPAGATE: u32 = 1;

/// A pixel buffer the stage produced, released when the last frame holding
/// it — the encoder's lease included — lets go.
#[derive(Debug)]
pub struct ConvertedBuffer(*mut c_void);

// SAFETY: a CVPixelBuffer reference may be retained and released on any
// thread; nothing here reads it concurrently.
unsafe impl Send for ConvertedBuffer {}
// SAFETY: as above; the wrapper only ever releases.
unsafe impl Sync for ConvertedBuffer {}

impl Drop for ConvertedBuffer {
    fn drop(&mut self) {
        // SAFETY: the stage created this buffer with a +1 reference.
        unsafe { CVPixelBufferRelease(self.0) };
    }
}

/// Why the stage could not run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PqWhiteError(pub String);

impl std::fmt::Display for PqWhiteError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "HDR white stage: {}", self.0)
    }
}

impl std::error::Error for PqWhiteError {}

/// The GPU stage, sized for one capture.
pub struct PqWhiteStage {
    context: Retained<AnyObject>,
    exposure: Retained<NSNumber>,
    pool: *mut c_void,
    output_space: *mut c_void,
    width: usize,
    height: usize,
    gain: f64,
}

// SAFETY: the stage is used by one encode thread at a time; Core Image
// contexts and pools are safe to use from a thread other than the creator's.
unsafe impl Send for PqWhiteStage {}

impl std::fmt::Debug for PqWhiteStage {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PqWhiteStage")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("gain", &self.gain)
            .finish_non_exhaustive()
    }
}

impl Drop for PqWhiteStage {
    fn drop(&mut self) {
        // SAFETY: both were created by `new` with a +1 reference.
        unsafe {
            CVPixelBufferPoolRelease(self.pool);
            CGColorSpaceRelease(self.output_space);
        }
    }
}

impl PqWhiteStage {
    /// A stage for `width`x`height` 4:4:4 full-range PQ frames that moves
    /// SDR white from `source_white_nits` to the shared contract's white.
    ///
    /// # Errors
    ///
    /// Returns [`PqWhiteError`] when Core Image or Core Video refuses.
    pub fn new(width: usize, height: usize, source_white_nits: f64) -> Result<Self, PqWhiteError> {
        let gain = arcen_media::video::pq_white::reference_white_gain(source_white_nits);
        let fail = |what: &str| PqWhiteError(what.to_owned());
        let context_class = AnyClass::get(c"CIContext").ok_or_else(|| fail("no CIContext"))?;
        // SAFETY: CoreGraphics' own constant names; released in `Drop`.
        let (working_space, output_space) = unsafe {
            (
                CGColorSpaceCreateWithName(kCGColorSpaceExtendedLinearITUR_2020),
                CGColorSpaceCreateWithName(kCGColorSpaceITUR_2100_PQ),
            )
        };
        if working_space.is_null() || output_space.is_null() {
            return Err(fail("no PQ or extended-linear colour space"));
        }
        // SAFETY: the dictionary holds CF objects toll-free bridged to
        // Foundation; every key is a Core Image constant and every selector is
        // sent to the class that declares it.
        let context: Option<Retained<AnyObject>> = unsafe {
            let format = NSNumber::new_i32(kCIFormatRGBAh);
            let no = NSNumber::new_bool(false);
            let keys: [&AnyObject; 3] = [
                &*kCIContextWorkingColorSpace.cast::<AnyObject>(),
                &*kCIContextWorkingFormat.cast::<AnyObject>(),
                &*kCIContextCacheIntermediates.cast::<AnyObject>(),
            ];
            let values: [&AnyObject; 3] = [&*working_space.cast::<AnyObject>(), &format, &no];
            let options: Retained<AnyObject> = msg_send![
                AnyClass::get(c"NSDictionary").ok_or_else(|| fail("no NSDictionary"))?,
                dictionaryWithObjects: values.as_ptr(),
                forKeys: keys.as_ptr(),
                count: 3_usize,
            ];
            msg_send![context_class, contextWithOptions: &*options]
        };
        // SAFETY: the context retained the working space it was given.
        unsafe { CGColorSpaceRelease(working_space) };
        let context = context.ok_or_else(|| fail("Core Image refused a context"))?;

        let pool = create_pool(width, height)?;
        Ok(Self {
            context,
            exposure: NSNumber::new_f64(gain.log2()),
            pool,
            output_space,
            width,
            height,
            gain,
        })
    }

    /// The linear-light gain this stage applies.
    #[must_use]
    pub const fn gain(&self) -> f64 {
        self.gain
    }

    /// Returns `frame` with SDR white moved, as a new surface the encoder can
    /// take. The input is left untouched.
    ///
    /// # Errors
    ///
    /// Returns [`PqWhiteError`] when a buffer cannot be made or rendered.
    pub fn apply(&self, frame: &CapturedFrame) -> Result<CapturedFrame, PqWhiteError> {
        let (surface, buffer) = self.convert_surface(&frame.surface)?;
        Ok(frame.with_converted_surface(surface, buffer))
    }

    /// Converts one 4:4:4 PQ surface, returning the new surface and the
    /// buffer that owns it.
    ///
    /// # Errors
    ///
    /// Returns [`PqWhiteError`] when a buffer cannot be made or rendered.
    pub fn convert_surface(
        &self,
        source: &IOSurface,
    ) -> Result<(IOSurface, Arc<ConvertedBuffer>), PqWhiteError> {
        // Core Image hands back autoreleased objects, and the encode thread has
        // no pool of its own. Without one every frame's image stayed alive,
        // holding its capture surface in use: measured, ScreenCaptureKit
        // delivered exactly its eight pooled frames and then nothing.
        objc2::rc::autoreleasepool(|_| self.convert_surface_in_pool(source))
    }

    fn convert_surface_in_pool(
        &self,
        source: &IOSurface,
    ) -> Result<(IOSurface, Arc<ConvertedBuffer>), PqWhiteError> {
        let fail = |what: &str| PqWhiteError(what.to_owned());
        let (width, height) = (source.width(), source.height());
        let mut input: *mut c_void = std::ptr::null_mut();
        // SAFETY: the surface is live for this call; the wrapper is released
        // at the end of it.
        let status = unsafe {
            CVPixelBufferCreateWithIOSurface(
                std::ptr::null(),
                source.as_ptr(),
                std::ptr::null(),
                &raw mut input,
            )
        };
        if status != 0 || input.is_null() {
            return Err(fail("could not wrap the captured surface"));
        }
        let input = ConvertedBuffer(input);
        tag_pq_bt2020(input.0);
        let mut output: *mut c_void = std::ptr::null_mut();
        // SAFETY: a live pool; the +1 buffer is owned by `ConvertedBuffer`.
        let status = unsafe {
            CVPixelBufferPoolCreatePixelBuffer(std::ptr::null(), self.pool, &raw mut output)
        };
        if status != 0 || output.is_null() {
            return Err(fail("the output pool is exhausted"));
        }
        let output = Arc::new(ConvertedBuffer(output));
        tag_pq_bt2020(output.0);

        // SAFETY: every object is live for the call; selectors are sent to
        // the classes that declare them; `render:toCVPixelBuffer:` waits for
        // the GPU, so the output is complete when it returns.
        unsafe {
            let image_class = AnyClass::get(c"CIImage").ok_or_else(|| fail("no CIImage"))?;
            let image: Option<Retained<AnyObject>> =
                msg_send![image_class, imageWithCVPixelBuffer: input.0.cast::<CvBuffer>()];
            let image = image.ok_or_else(|| fail("Core Image refused the surface"))?;
            let filter_class = AnyClass::get(c"CIFilter").ok_or_else(|| fail("no CIFilter"))?;
            let name = NSString::from_str("CIExposureAdjust");
            let filter: Option<Retained<AnyObject>> =
                msg_send![filter_class, filterWithName: &*name];
            let filter = filter.ok_or_else(|| fail("no exposure filter"))?;
            let _: () =
                msg_send![&*filter, setValue: &*image, forKey: &*NSString::from_str("inputImage")];
            let _: () = msg_send![&*filter, setValue: &*self.exposure, forKey: &*NSString::from_str("inputEV")];
            let adjusted: Option<Retained<AnyObject>> = msg_send![&*filter, outputImage];
            let adjusted = adjusted.ok_or_else(|| fail("the exposure filter produced nothing"))?;
            let bounds = objc2_foundation::NSRect::new(
                objc2_foundation::NSPoint::new(0.0, 0.0),
                objc2_foundation::NSSize::new(width as f64, height as f64),
            );
            let _: () = msg_send![
                &*self.context,
                render: &*adjusted,
                toCVPixelBuffer: output.0.cast::<CvBuffer>(),
                bounds: bounds,
                colorSpace: self.output_space.cast::<CgColorSpace>(),
            ];
        }
        // SAFETY: the pool made an IOSurface-backed buffer; retaining the
        // surface gives `IOSurface::from_raw` the +1 it adopts.
        let surface = unsafe {
            let raw = CVPixelBufferGetIOSurface(output.0);
            if raw.is_null() {
                return Err(fail("the output buffer has no surface"));
            }
            IOSurface::from_raw(CFRetain(raw).cast_mut())
        }
        .ok_or_else(|| fail("the output surface could not be adopted"))?;
        Ok((surface, output))
    }
}

fn tag_pq_bt2020(buffer: *mut c_void) {
    // SAFETY: a live buffer and Core Video's own constants.
    unsafe {
        CVBufferSetAttachment(
            buffer,
            kCVImageBufferTransferFunctionKey,
            kCVImageBufferTransferFunction_SMPTE_ST_2084_PQ,
            SHOULD_PROPAGATE,
        );
        CVBufferSetAttachment(
            buffer,
            kCVImageBufferColorPrimariesKey,
            kCVImageBufferColorPrimaries_ITU_R_2020,
            SHOULD_PROPAGATE,
        );
        CVBufferSetAttachment(
            buffer,
            kCVImageBufferYCbCrMatrixKey,
            kCVImageBufferYCbCrMatrix_ITU_R_2020,
            SHOULD_PROPAGATE,
        );
    }
}

fn create_pool(width: usize, height: usize) -> Result<*mut c_void, PqWhiteError> {
    let number = |value: usize| NSNumber::new_usize(value);
    let empty: Retained<AnyObject> = {
        let class = AnyClass::get(c"NSDictionary")
            .ok_or_else(|| PqWhiteError("no NSDictionary".to_owned()))?;
        // SAFETY: `+dictionary` has no preconditions.
        unsafe { msg_send![class, dictionary] }
    };
    let format = NSNumber::new_u32(0x7866_3434);
    let (width_value, height_value) = (number(width), number(height));
    let mut pool: *mut c_void = std::ptr::null_mut();
    // SAFETY: Core Video keys and Foundation values, toll-free bridged; the
    // pool is returned +1 and released by the stage.
    let status = unsafe {
        let keys: [&AnyObject; 4] = [
            &*kCVPixelBufferIOSurfacePropertiesKey.cast::<AnyObject>(),
            &*kCVPixelBufferPixelFormatTypeKey.cast::<AnyObject>(),
            &*kCVPixelBufferWidthKey.cast::<AnyObject>(),
            &*kCVPixelBufferHeightKey.cast::<AnyObject>(),
        ];
        let values: [&AnyObject; 4] = [&empty, &format, &width_value, &height_value];
        let attributes: Retained<AnyObject> = msg_send![
            AnyClass::get(c"NSDictionary").ok_or_else(|| PqWhiteError("no NSDictionary".to_owned()))?,
            dictionaryWithObjects: values.as_ptr(),
            forKeys: keys.as_ptr(),
            count: 4_usize,
        ];
        CVPixelBufferPoolCreate(
            std::ptr::null(),
            std::ptr::null(),
            Retained::as_ptr(&attributes).cast(),
            &raw mut pool,
        )
    };
    if status != 0 || pool.is_null() {
        return Err(PqWhiteError(format!(
            "Core Video refused the pool: {status}"
        )));
    }
    Ok(pool)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcen_media::video::pq_white::{
        MACOS_CAPTURE_WHITE_NITS, reference_white_gain, rescale_pq_code,
    };

    fn uniform_surface(code: u16) -> IOSurface {
        let surface = crate::encode::ten_bit_probe_surface(64, 64, true).expect("surface");
        {
            let mut guard = surface.lock_read_write().expect("lock");
            let luma_row = surface.bytes_per_row_of_plane(0);
            let luma = guard.base_address_of_plane_mut(0).expect("luma");
            for y in 0..64 {
                for x in 0..64 {
                    // SAFETY: inside plane 0, locked for writing.
                    unsafe {
                        luma.add(y * luma_row + x * 2)
                            .cast::<u16>()
                            .write_unaligned(code << 6)
                    };
                }
            }
        }
        surface
    }

    fn luma_at(surface: &IOSurface) -> (u16, u16, u16) {
        let guard = surface.lock_read_only().expect("lock");
        let luma = guard.base_address_of_plane(0).expect("luma");
        let chroma = guard.base_address_of_plane(1).expect("chroma");
        // SAFETY: inside the planes, locked for reading.
        unsafe {
            (
                luma.add(40).cast::<u16>().read_unaligned() >> 6,
                chroma.add(80).cast::<u16>().read_unaligned() >> 6,
                chroma.add(82).cast::<u16>().read_unaligned() >> 6,
            )
        }
    }

    /// Runs Core Image on this machine's GPU and checks it against the
    /// shared reference remap, code for code.
    #[test]
    fn the_gpu_stage_moves_white_as_the_shared_reference_does() {
        let stage = PqWhiteStage::new(64, 64, MACOS_CAPTURE_WHITE_NITS).expect("stage");
        let gain = reference_white_gain(MACOS_CAPTURE_WHITE_NITS);
        for code in [300_u16, 520, 594, 700, 766] {
            let (converted, _buffer) = stage
                .convert_surface(&uniform_surface(code))
                .expect("converts");
            let (luma, cb, cr) = luma_at(&converted);
            let expected = rescale_pq_code(code, gain);
            assert!(
                luma.abs_diff(expected) <= 2,
                "code {code}: GPU {luma}, reference {expected}"
            );
            assert!(
                cb.abs_diff(512) <= 1 && cr.abs_diff(512) <= 1,
                "neutral stays neutral: {cb} {cr}"
            );
        }
    }

    #[test]
    fn a_unit_gain_is_exact() {
        let stage = PqWhiteStage::new(64, 64, arcen_media::video::pq_white::GRAPHICS_WHITE_NITS)
            .expect("stage");
        let (converted, _buffer) = stage
            .convert_surface(&uniform_surface(594))
            .expect("converts");
        assert_eq!(luma_at(&converted).0, 594);
    }
}
