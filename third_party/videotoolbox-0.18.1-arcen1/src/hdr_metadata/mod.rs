//! `VTHDRPerFrameMetadataGenerationSession` — generate Dolby Vision
//! per-frame HDR metadata (macOS 15+).

use core::ptr;

use apple_cf::{
    cf::{AsCFType, CFArray, CFDictionary, CFType},
    cv::CVPixelBuffer,
};

use crate::error::VTError;
use crate::ffi;

/// HDR metadata formats accepted by `VTHDRPerFrameMetadataGenerationSession`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HdrMetadataFormat {
    DolbyVision,
}

impl HdrMetadataFormat {
    fn as_cf_string(self) -> ffi::VTHDRPerFrameMetadataGenerationHDRFormatType {
        match self {
            Self::DolbyVision => unsafe {
                ffi::kVTHDRPerFrameMetadataGenerationHDRFormatType_DolbyVision
            },
        }
    }
}

/// `VTHDRPerFrameMetadataGenerationSessionRef`.
pub struct HdrMetadataSession {
    inner: ffi::VTHDRPerFrameMetadataGenerationSessionRef,
}

unsafe impl Send for HdrMetadataSession {}
unsafe impl Sync for HdrMetadataSession {}

crate::utils::retained::vt_retained!(HdrMetadataSession, field = inner, release = ffi::CFRelease);

impl HdrMetadataSession {
    /// CoreFoundation type identifier for `VTHDRPerFrameMetadataGenerationSession`.
    #[must_use]
    pub fn type_id() -> usize {
        unsafe { ffi::VTHDRPerFrameMetadataGenerationSessionGetTypeID() }
    }

    /// Create a new HDR-metadata generation session configured for Dolby Vision.
    /// `fps` is the source frame rate (used for temporal-coherence calculations).
    ///
    /// # Errors
    ///
    /// Returns [`VTError::SessionCreateFailed`] on failure.
    pub fn new(fps: f32) -> Result<Self, VTError> {
        Self::new_with_formats(fps, &[HdrMetadataFormat::DolbyVision])
    }

    /// Create a new HDR-metadata generation session with explicit HDR formats.
    ///
    /// Apple's current public macOS SDK only advertises
    /// [`HdrMetadataFormat::DolbyVision`]. Passing an empty slice falls back to
    /// the framework default.
    ///
    /// # Errors
    ///
    /// Returns [`VTError::SessionCreateFailed`] on failure.
    pub fn new_with_formats(fps: f32, hdr_formats: &[HdrMetadataFormat]) -> Result<Self, VTError> {
        let options = build_hdr_options(hdr_formats);
        let mut p: ffi::VTHDRPerFrameMetadataGenerationSessionRef = ptr::null_mut();
        let s = unsafe {
            ffi::VTHDRPerFrameMetadataGenerationSessionCreate(
                ffi::kCFAllocatorDefault,
                fps,
                options
                    .as_ref()
                    .map_or(ptr::null(), |dict| dict.as_ptr().cast_const().cast()),
                &mut p,
            )
        };
        if s != 0 || p.is_null() {
            return Err(VTError::SessionCreateFailed(s));
        }
        Ok(Self { inner: p })
    }

    /// Analyze `pixel_buffer` and attach generated HDR metadata to
    /// its `CVPixelBuffer` attachments and backing `IOSurface`.
    /// Set `scene_change = true` when the frame is a hard scene
    /// transition.
    ///
    /// # Errors
    ///
    /// Returns [`VTError::EncodeFailed`] on `OSStatus` failure.
    pub fn attach_metadata(
        &self,
        pixel_buffer: &CVPixelBuffer,
        scene_change: bool,
    ) -> Result<(), VTError> {
        let s = unsafe {
            ffi::VTHDRPerFrameMetadataGenerationSessionAttachMetadata(
                self.inner,
                pixel_buffer.as_ptr().cast(),
                scene_change,
            )
        };
        if s == 0 {
            Ok(())
        } else {
            Err(VTError::EncodeFailed(s))
        }
    }

    /// Raw `VTHDRPerFrameMetadataGenerationSessionRef`.
    #[must_use]
    pub const fn as_ptr(&self) -> ffi::VTHDRPerFrameMetadataGenerationSessionRef {
        self.inner
    }
}

fn build_hdr_options(hdr_formats: &[HdrMetadataFormat]) -> Option<CFDictionary> {
    if hdr_formats.is_empty() {
        return None;
    }

    let formats: Vec<CFType> = hdr_formats
        .iter()
        .map(|format| retained_cf_type(format.as_cf_string().cast_mut()))
        .collect();
    let format_refs: Vec<&dyn AsCFType> = formats
        .iter()
        .map(|format| format as &dyn AsCFType)
        .collect();
    let array = CFArray::from_values(&format_refs);
    let key = retained_cf_type(unsafe {
        ffi::kVTHDRPerFrameMetadataGenerationOptionsKey_HDRFormats.cast_mut()
    });
    Some(CFDictionary::from_pairs(&[(
        &key as &dyn AsCFType,
        &array as &dyn AsCFType,
    )]))
}

fn retained_cf_type<T>(raw: *mut T) -> CFType {
    unsafe {
        CFType::from_raw_retained(raw.cast()).expect("VideoToolbox HDR constant must be non-null")
    }
}
