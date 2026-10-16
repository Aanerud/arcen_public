//! [`CMSampleBuffer`] — framework-agnostic safe wrapper around a `CoreMedia`
//! `CMSampleBufferRef`.
//!
//! This wrapper exposes the *generic* `CMSampleBuffer` surface that every
//! consumer needs: presentation timestamp, format description, attached
//! data buffer (`CMBlockBuffer`), sample count, validity. Framework-specific
//! attachment readers (e.g. `SCStreamFrameInfo`'s frame status, content
//! rect, dirty rects) live in the consuming crates so that, for example,
//! `screencapturekit-rs`'s SC-attachment readers don't get pulled into
//! `videotoolbox-rs`.

use super::{CMBlockBuffer, CMFormatDescription, CMTime};
use crate::ffi;
use std::fmt;

/// Owned reference to a `CoreMedia` `CMSampleBufferRef`.
///
/// Cloning increments the underlying refcount via `CFRetain`; dropping
/// releases via `CFRelease`. The pointer is opaque to safe Rust — accessor
/// methods on this type are the only sanctioned way to inspect it.
pub struct CMSampleBuffer(*mut std::ffi::c_void);

// SAFETY: CMSampleBufferRef is documented as thread-safe for read access;
// we only share the opaque pointer between threads and never dereference
// it from Rust.
unsafe impl Send for CMSampleBuffer {}
unsafe impl Sync for CMSampleBuffer {}

impl CMSampleBuffer {
    /// Wrap a raw `CMSampleBufferRef` without bumping its refcount or
    /// checking for NULL.
    ///
    /// # Safety
    ///
    /// The caller must ensure `ptr` is a valid `CMSampleBufferRef` retained
    /// at +1 (the wrapper will release on drop). For NULL-tolerant
    /// construction prefer [`Self::from_raw`].
    #[must_use]
    pub const unsafe fn from_ptr(ptr: *mut std::ffi::c_void) -> Self {
        Self(ptr)
    }

    /// Wrap a raw `CMSampleBufferRef` without bumping its refcount.
    ///
    /// Use this when the caller has just received a `+1` retained pointer
    /// (e.g. a Swift `Unmanaged.passRetained(...).toOpaque()`). The
    /// returned `CMSampleBuffer` will release the pointer when dropped.
    ///
    /// Returns `None` for a NULL pointer.
    #[must_use]
    pub fn from_raw(ptr: *mut std::ffi::c_void) -> Option<Self> {
        if ptr.is_null() {
            None
        } else {
            Some(Self(ptr))
        }
    }

    /// Wrap a raw `CMSampleBufferRef`, calling `CFRetain` to bump its
    /// refcount before taking ownership.
    ///
    /// Use this when the caller holds a borrowed (non-owning) reference
    /// and wants to take ownership without affecting the source.
    ///
    /// # Safety
    ///
    /// `ptr` must be a valid `CMSampleBufferRef` (or NULL). Passing a
    /// dangling or wrong-type pointer is undefined behaviour.
    #[must_use]
    pub unsafe fn from_raw_retained(ptr: *mut std::ffi::c_void) -> Option<Self> {
        if ptr.is_null() {
            None
        } else {
            let retained = unsafe { ffi::cm_sample_buffer_retain(ptr) };
            Self::from_raw(retained)
        }
    }

    /// Borrow the underlying `CMSampleBufferRef` for hand-off to other
    /// Apple bindings without changing its refcount. The pointer remains
    /// valid for the lifetime of `self`.
    #[must_use]
    pub const fn as_ptr(&self) -> *mut std::ffi::c_void {
        self.0
    }

    /// Whether the sample buffer is in a valid state.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        unsafe { ffi::cm_sample_buffer_is_valid(self.0) }
    }

    /// Whether the sample buffer's data is ready for consumption.
    #[must_use]
    pub fn data_is_ready(&self) -> bool {
        unsafe { ffi::cm_sample_buffer_data_is_ready(self.0) }
    }

    /// Number of samples carried by this buffer (1 for video, N for audio).
    #[must_use]
    pub fn num_samples(&self) -> i64 {
        unsafe { ffi::cm_sample_buffer_get_num_samples(self.0) }
    }

    /// Presentation timestamp of the first sample.
    ///
    /// Returns [`CMTime::INVALID`] if the buffer has no PTS.
    #[must_use]
    pub fn presentation_timestamp(&self) -> CMTime {
        let mut t = CMTime::INVALID;
        unsafe {
            ffi::cm_sample_buffer_get_presentation_timestamp(
                self.0,
                &mut t.value,
                &mut t.timescale,
                &mut t.flags,
                &mut t.epoch,
            );
        }
        t
    }

    /// Decode timestamp of the first sample (matters when there's B-frame
    /// reordering between PTS and DTS).
    #[must_use]
    pub fn decode_timestamp(&self) -> CMTime {
        let mut t = CMTime::INVALID;
        unsafe {
            ffi::cm_sample_buffer_get_decode_timestamp(
                self.0,
                &mut t.value,
                &mut t.timescale,
                &mut t.flags,
                &mut t.epoch,
            );
        }
        t
    }

    /// Total duration of all samples in this buffer.
    #[must_use]
    pub fn duration(&self) -> CMTime {
        let mut t = CMTime::INVALID;
        unsafe {
            ffi::cm_sample_buffer_get_duration(
                self.0,
                &mut t.value,
                &mut t.timescale,
                &mut t.flags,
                &mut t.epoch,
            );
        }
        t
    }

    /// The attached [`CMBlockBuffer`] holding the encoded sample data, if
    /// the sample buffer is data-bearing (as opposed to image-bearing).
    ///
    /// Video frames from `VTCompressionSession` always have a data buffer
    /// (the encoded NAL units / `ProRes` frame data). Decoded video frames
    /// from a capture pipeline typically use an image buffer instead — see
    /// [`Self::image_buffer_ptr`].
    #[must_use]
    pub fn data_buffer(&self) -> Option<CMBlockBuffer> {
        let ptr = unsafe { ffi::cm_sample_buffer_get_data_buffer(self.0) };
        if ptr.is_null() {
            None
        } else {
            // CMSampleBufferGetDataBuffer returns an unretained reference;
            // bump the refcount so our wrapper can release on drop.
            let retained = unsafe { ffi::cm_block_buffer_retain(ptr) };
            CMBlockBuffer::from_raw(retained)
        }
    }

    /// Format description (codec, dimensions, audio params, ...) attached
    /// to this sample buffer.
    #[must_use]
    pub fn format_description(&self) -> Option<CMFormatDescription> {
        let ptr = unsafe { ffi::cm_sample_buffer_get_format_description(self.0) };
        if ptr.is_null() {
            None
        } else {
            let retained = unsafe { ffi::cm_format_description_retain(ptr) };
            CMFormatDescription::from_raw(retained)
        }
    }

    /// Raw `CVImageBufferRef` if the sample is image-bearing (decoded
    /// video frames from a capture pipeline). Use a CV crate's wrapper to
    /// turn this into a safe `CVPixelBuffer`.
    ///
    /// Returns NULL for sample buffers that don't carry an image buffer
    /// (e.g. compressed video from `VideoToolbox`, audio samples).
    #[must_use]
    pub fn image_buffer_ptr(&self) -> *mut std::ffi::c_void {
        unsafe { ffi::cm_sample_buffer_get_image_buffer(self.0) }
    }
}

impl Clone for CMSampleBuffer {
    fn clone(&self) -> Self {
        let retained = unsafe { ffi::cm_sample_buffer_retain(self.0) };
        Self(retained)
    }
}

impl Drop for CMSampleBuffer {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { ffi::cm_sample_buffer_release(self.0) };
        }
    }
}

impl PartialEq for CMSampleBuffer {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl Eq for CMSampleBuffer {}

impl std::hash::Hash for CMSampleBuffer {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        unsafe {
            let h = ffi::cm_sample_buffer_hash(self.0);
            h.hash(state);
        }
    }
}

impl fmt::Debug for CMSampleBuffer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CMSampleBuffer")
            .field("ptr", &self.0)
            .field("num_samples", &self.num_samples())
            .field("pts", &self.presentation_timestamp())
            .finish()
    }
}
