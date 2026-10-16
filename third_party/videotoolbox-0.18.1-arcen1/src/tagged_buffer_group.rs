//! Minimal wrapper around `CoreMedia` `CMTaggedBufferGroupRef`.
//!
//! Multi-image `VideoToolbox` encode/decode entry points accept or return tagged
//! buffer groups that bundle multiple pixel or sample buffers into one logical
//! frame (for example stereo MV-HEVC left/right eye images).

use apple_cf::cm::CMSampleBuffer;
use apple_cf::cv::CVPixelBuffer;

use crate::ffi;

/// Owned `CMTaggedBufferGroupRef`.
pub struct TaggedBufferGroup {
    inner: ffi::CMTaggedBufferGroupRef,
}

unsafe impl Send for TaggedBufferGroup {}
unsafe impl Sync for TaggedBufferGroup {}

impl TaggedBufferGroup {
    /// CoreFoundation type identifier for `CMTaggedBufferGroup`.
    #[must_use]
    pub fn type_id() -> usize {
        unsafe { ffi::CMTaggedBufferGroupGetTypeID() }
    }

    /// Adopt a retained `CMTaggedBufferGroupRef`.
    ///
    /// # Safety
    ///
    /// `ptr` must be NULL or a valid retained `CMTaggedBufferGroupRef`.
    #[must_use]
    pub unsafe fn from_raw(ptr: ffi::CMTaggedBufferGroupRef) -> Option<Self> {
        if ptr.is_null() {
            None
        } else {
            Some(Self { inner: ptr })
        }
    }

    /// Retain a borrowed `CMTaggedBufferGroupRef` before wrapping it.
    ///
    /// # Safety
    ///
    /// `ptr` must be NULL or a valid `CMTaggedBufferGroupRef`.
    #[must_use]
    pub unsafe fn from_raw_retained(ptr: ffi::CMTaggedBufferGroupRef) -> Option<Self> {
        if ptr.is_null() {
            None
        } else {
            unsafe { ffi::CFRetain(ptr.cast()) };
            Self::from_raw(ptr)
        }
    }

    /// Borrow the raw `CMTaggedBufferGroupRef`.
    #[must_use]
    pub const fn as_ptr(&self) -> ffi::CMTaggedBufferGroupRef {
        self.inner
    }

    /// Number of buffers in the group.
    #[must_use]
    pub fn len(&self) -> usize {
        usize::try_from(unsafe { ffi::CMTaggedBufferGroupGetCount(self.inner) }).unwrap_or(0)
    }

    /// Whether the group is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Copy the pixel buffer at `index`, if that entry stores a `CVPixelBuffer`.
    #[must_use]
    pub fn pixel_buffer_at(&self, index: usize) -> Option<CVPixelBuffer> {
        let index = isize::try_from(index).ok()?;
        let ptr = unsafe { ffi::CMTaggedBufferGroupGetCVPixelBufferAtIndex(self.inner, index) };
        if ptr.is_null() {
            None
        } else {
            unsafe { ffi::CFRetain(ptr.cast()) };
            CVPixelBuffer::from_raw(ptr.cast())
        }
    }

    /// Copy the sample buffer at `index`, if that entry stores a `CMSampleBuffer`.
    #[must_use]
    pub fn sample_buffer_at(&self, index: usize) -> Option<CMSampleBuffer> {
        let index = isize::try_from(index).ok()?;
        let ptr = unsafe { ffi::CMTaggedBufferGroupGetCMSampleBufferAtIndex(self.inner, index) };
        unsafe { CMSampleBuffer::from_raw_retained(ptr.cast()) }
    }
}

impl Clone for TaggedBufferGroup {
    fn clone(&self) -> Self {
        unsafe { Self::from_raw_retained(self.inner) }
            .expect("tagged buffer group pointer must be non-null")
    }
}

crate::utils::retained::vt_retained!(TaggedBufferGroup, field = inner, release = ffi::CFRelease);

impl core::fmt::Debug for TaggedBufferGroup {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("TaggedBufferGroup")
            .field("ptr", &self.inner)
            .field("len", &self.len())
            .finish()
    }
}
