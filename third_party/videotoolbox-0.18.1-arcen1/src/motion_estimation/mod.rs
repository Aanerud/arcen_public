//! `VTMotionEstimationSession` — between-frame motion-vector
//! estimation backed by the Apple Neural Engine (macOS 26+).
//!
//! Session lifecycle and property access use the public C API directly.
//! The per-frame motion-estimation submit path still goes through a small Swift
//! bridge because Apple exposes completion through a block-based callback.

use core::ffi::c_void;
use core::ptr;

use apple_cf::{
    cf::{AsCFType, CFDictionary, CFNumber, CFString, CFType},
    cv::CVPixelBuffer,
};

use crate::error::VTError;
use crate::ffi;

extern "C" {
    fn vtb_motion_session_estimate(
        session: *mut c_void,
        reference_image: *mut c_void,
        current_image: *mut c_void,
        frame_flags: ffi::VTMotionEstimationFrameFlags,
        info_flags_out: *mut ffi::VTMotionEstimationInfoFlags,
        out: *mut *mut c_void,
    ) -> i32;
}

/// Creation-time options for `VTMotionEstimationSessionCreate`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MotionEstimationSessionOptions {
    pub motion_vector_size: Option<u32>,
    pub use_multi_pass_search: bool,
    pub label: Option<String>,
}

/// Result of one motion-estimation submission.
pub struct MotionEstimationResult {
    pub motion_vectors: CVPixelBuffer,
    pub info_flags: ffi::VTMotionEstimationInfoFlags,
}

/// `VTMotionEstimationSessionRef`.
pub struct MotionEstimationSession {
    inner: ffi::VTMotionEstimationSessionRef,
}

unsafe impl Send for MotionEstimationSession {}
unsafe impl Sync for MotionEstimationSession {}

crate::utils::retained::vt_retained!(
    MotionEstimationSession,
    field = inner,
    invalidate = ffi::VTMotionEstimationSessionInvalidate,
    release = ffi::CFRelease,
);

impl MotionEstimationSession {
    /// CoreFoundation type identifier for `VTMotionEstimationSession`.
    #[must_use]
    pub fn type_id() -> usize {
        unsafe { ffi::VTMotionEstimationSessionGetTypeID() }
    }

    /// Create a motion-estimation session that accepts pixel buffers
    /// of the given size (must match for both `reference` and `current`).
    ///
    /// # Errors
    ///
    /// Returns [`VTError::SessionCreateFailed`] on failure or when
    /// running on macOS < 26.
    pub fn new(width: u32, height: u32) -> Result<Self, VTError> {
        Self::new_with_options(width, height, &MotionEstimationSessionOptions::default())
    }

    /// Create a motion-estimation session with explicit selection options.
    ///
    /// # Errors
    ///
    /// Returns [`VTError::InvalidArgument`] when `motion_vector_size` is not 4 or 16,
    /// or [`VTError::SessionCreateFailed`] when the framework rejects the session.
    pub fn new_with_options(
        width: u32,
        height: u32,
        options: &MotionEstimationSessionOptions,
    ) -> Result<Self, VTError> {
        let creation_options = build_creation_options(options)?;
        let mut p: ffi::VTMotionEstimationSessionRef = ptr::null_mut();
        let s = unsafe {
            ffi::VTMotionEstimationSessionCreate(
                ffi::kCFAllocatorDefault,
                creation_options
                    .as_ref()
                    .map_or(ptr::null(), |dict| dict.as_ptr().cast_const().cast()),
                width,
                height,
                &mut p,
            )
        };
        if s != 0 || p.is_null() {
            return Err(VTError::SessionCreateFailed(s));
        }
        Ok(Self { inner: p })
    }

    /// Copy the source pixel-buffer attributes Apple recommends for
    /// feeding this session.
    ///
    /// # Errors
    ///
    /// Returns [`VTError::EncodeFailed`] on `OSStatus` failure.
    pub fn source_pixel_buffer_attributes(&self) -> Result<*const c_void, VTError> {
        let mut attrs: ffi::CFDictionaryRef = ptr::null();
        let s = unsafe {
            ffi::VTMotionEstimationSessionCopySourcePixelBufferAttributes(self.inner, &mut attrs)
        };
        if s != 0 {
            return Err(VTError::EncodeFailed(s));
        }
        Ok(attrs.cast())
    }

    /// Force-complete any outstanding estimations.
    ///
    /// # Errors
    ///
    /// Returns [`VTError::EncodeFailed`] on `OSStatus` failure.
    pub fn complete_frames(&self) -> Result<(), VTError> {
        let s = unsafe { ffi::VTMotionEstimationSessionCompleteFrames(self.inner) };
        if s == 0 {
            Ok(())
        } else {
            Err(VTError::EncodeFailed(s))
        }
    }

    /// Estimate motion vectors from `reference` to `current` using the
    /// framework defaults.
    ///
    /// # Errors
    ///
    /// Returns [`VTError::EncodeFailed`] on `OSStatus` failure.
    pub fn estimate(
        &self,
        reference: &CVPixelBuffer,
        current: &CVPixelBuffer,
    ) -> Result<CVPixelBuffer, VTError> {
        self.estimate_with_options(reference, current, 0)
            .map(|result| result.motion_vectors)
    }

    /// Estimate motion vectors from `reference` to `current` with explicit
    /// per-frame `VTMotionEstimationFrameFlags`.
    ///
    /// # Errors
    ///
    /// Returns [`VTError::EncodeFailed`] on `OSStatus` failure.
    pub fn estimate_with_options(
        &self,
        reference: &CVPixelBuffer,
        current: &CVPixelBuffer,
        frame_flags: ffi::VTMotionEstimationFrameFlags,
    ) -> Result<MotionEstimationResult, VTError> {
        let mut out: *mut c_void = ptr::null_mut();
        let mut info_flags: ffi::VTMotionEstimationInfoFlags = 0;
        let s = unsafe {
            vtb_motion_session_estimate(
                self.inner.cast(),
                reference.as_ptr().cast::<c_void>(),
                current.as_ptr().cast::<c_void>(),
                frame_flags,
                &mut info_flags,
                &mut out,
            )
        };
        if s != 0 || out.is_null() {
            return Err(VTError::EncodeFailed(s));
        }
        let motion_vectors = CVPixelBuffer::from_raw(out.cast()).ok_or(VTError::EncodeFailed(0))?;
        Ok(MotionEstimationResult {
            motion_vectors,
            info_flags,
        })
    }

    /// Raw `VTMotionEstimationSessionRef`.
    #[must_use]
    pub const fn as_ptr(&self) -> ffi::VTMotionEstimationSessionRef {
        self.inner
    }
}

fn build_creation_options(
    options: &MotionEstimationSessionOptions,
) -> Result<Option<CFDictionary>, VTError> {
    if let Some(size) = options.motion_vector_size {
        if !matches!(size, 4 | 16) {
            return Err(VTError::InvalidArgument(
                "motion_vector_size must be either 4 or 16".to_string(),
            ));
        }
    }

    let mut keys = Vec::new();
    let mut values = Vec::new();

    if let Some(size) = options.motion_vector_size {
        keys.push(retained_cf_type(unsafe {
            ffi::kVTMotionEstimationSessionCreationOption_MotionVectorSize.cast_mut()
        }));
        values.push(CFNumber::from_u64(u64::from(size)).to_cf_type());
    }
    if options.use_multi_pass_search {
        keys.push(retained_cf_type(unsafe {
            ffi::kVTMotionEstimationSessionCreationOption_UseMultiPassSearch.cast_mut()
        }));
        values.push(retained_cf_type(unsafe { ffi::kCFBooleanTrue.cast_mut() }));
    }
    if let Some(label) = &options.label {
        keys.push(retained_cf_type(unsafe {
            ffi::kVTMotionEstimationSessionCreationOption_Label.cast_mut()
        }));
        values.push(CFString::new(label).to_cf_type());
    }

    if keys.is_empty() {
        return Ok(None);
    }

    let pairs: Vec<(&dyn AsCFType, &dyn AsCFType)> = keys
        .iter()
        .zip(values.iter())
        .map(|(key, value)| (key as &dyn AsCFType, value as &dyn AsCFType))
        .collect();
    Ok(Some(CFDictionary::from_pairs(&pairs)))
}

fn retained_cf_type<T>(raw: *mut T) -> CFType {
    unsafe {
        CFType::from_raw_retained(raw.cast())
            .expect("VideoToolbox motion-estimation constant must be non-null")
    }
}
