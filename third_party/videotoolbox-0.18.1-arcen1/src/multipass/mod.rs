//! `VTFrameSilo` + `VTMultiPassStorage` — multi-pass video encoding storage.
//!
//! `VTFrameSilo` collects `CMSampleBuffer`s produced by a multi-pass
//! compression session and lets the encoder re-iterate over them for
//! subsequent passes. `VTMultiPassStorage` holds the encoder's private
//! per-pass scratch data (associate it with a `CompressionSession` via
//! `kVTCompressionPropertyKey_MultiPassStorage`).

use core::ffi::c_void;
use core::ptr;

use apple_cf::cm::CMSampleBuffer;

use crate::error::VTError;
use crate::ffi;

/// `VTFrameSiloRef` — keyed-by-PTS storage of encoded sample buffers.
pub struct FrameSilo {
    inner: ffi::VTFrameSiloRef,
}

unsafe impl Send for FrameSilo {}
unsafe impl Sync for FrameSilo {}

crate::utils::retained::vt_retained!(FrameSilo, field = inner, release = ffi::CFRelease);

impl FrameSilo {
    /// CoreFoundation type identifier for `VTFrameSilo`.
    #[must_use]
    pub fn type_id() -> usize {
        unsafe { ffi::VTFrameSiloGetTypeID() }
    }

    /// Create an in-memory frame silo.
    ///
    /// # Errors
    ///
    /// Returns [`VTError::SessionCreateFailed`] if Apple refuses.
    pub fn new() -> Result<Self, VTError> {
        let mut p: ffi::VTFrameSiloRef = ptr::null_mut();
        let s = unsafe {
            ffi::VTFrameSiloCreate(
                ffi::kCFAllocatorDefault,
                ptr::null(),
                ffi::CMTimeRange::INVALID,
                ptr::null(),
                &mut p,
            )
        };
        if s != 0 || p.is_null() {
            return Err(VTError::SessionCreateFailed(s));
        }
        Ok(Self { inner: p })
    }

    /// Append a `CMSampleBuffer` to the silo. Sample buffers must
    /// be added in ascending decode-timestamp order within a pass.
    ///
    /// # Errors
    ///
    /// Returns [`VTError::EncodeFailed`] on `OSStatus` failure.
    pub fn add_sample_buffer(&self, sample: &CMSampleBuffer) -> Result<(), VTError> {
        let s = unsafe { ffi::VTFrameSiloAddSampleBuffer(self.inner, sample.as_ptr().cast()) };
        if s == 0 {
            Ok(())
        } else {
            Err(VTError::EncodeFailed(s))
        }
    }

    /// Return Apple's progress estimate for the current pass
    /// (`0.0..=1.0`).
    ///
    /// # Errors
    ///
    /// Returns [`VTError::EncodeFailed`] on `OSStatus` failure.
    pub fn progress_of_current_pass(&self) -> Result<f32, VTError> {
        let mut p: f32 = 0.0;
        let s = unsafe { ffi::VTFrameSiloGetProgressOfCurrentPass(self.inner, &mut p) };
        if s == 0 {
            Ok(p)
        } else {
            Err(VTError::EncodeFailed(s))
        }
    }

    /// Replace the time ranges used for the next pass.
    ///
    /// # Errors
    ///
    /// Returns [`VTError::ApiFailed`] on a non-zero `OSStatus`.
    pub fn set_time_ranges_for_next_pass(
        &self,
        time_ranges: &[ffi::CMTimeRange],
    ) -> Result<(), VTError> {
        let count = ffi::CMItemCount::try_from(time_ranges.len()).map_err(|_| {
            VTError::InvalidArgument("time range count overflowed CMItemCount".to_string())
        })?;
        let status = unsafe {
            ffi::VTFrameSiloSetTimeRangesForNextPass(self.inner, count, time_ranges.as_ptr())
        };
        if status == 0 {
            Ok(())
        } else {
            Err(VTError::ApiFailed {
                api: "VTFrameSiloSetTimeRangesForNextPass",
                status,
            })
        }
    }

    /// Collect all sample buffers in `time_range` (or the whole silo when
    /// `None`). Returned sample buffers are retained for the caller.
    ///
    /// # Errors
    ///
    /// Returns [`VTError::ApiFailed`] on a non-zero `OSStatus`.
    pub fn sample_buffers(
        &self,
        time_range: Option<ffi::CMTimeRange>,
    ) -> Result<Vec<CMSampleBuffer>, VTError> {
        let mut samples = Vec::new();
        let status = unsafe {
            ffi::VTFrameSiloCallFunctionForEachSampleBuffer(
                self.inner,
                time_range.unwrap_or(ffi::CMTimeRange::INVALID),
                (&raw mut samples).cast(),
                Some(frame_silo_collect_sample_buffer),
            )
        };
        if status == 0 {
            Ok(samples)
        } else {
            Err(VTError::ApiFailed {
                api: "VTFrameSiloCallFunctionForEachSampleBuffer",
                status,
            })
        }
    }

    /// Raw `VTFrameSiloRef`.
    #[must_use]
    pub const fn as_ptr(&self) -> ffi::VTFrameSiloRef {
        self.inner
    }
}

/// `VTMultiPassStorageRef` — opaque encoder scratch storage for
/// multi-pass encoding.
pub struct MultiPassStorage {
    inner: ffi::VTMultiPassStorageRef,
}

unsafe impl Send for MultiPassStorage {}
unsafe impl Sync for MultiPassStorage {}

crate::utils::retained::vt_retained!(
    MultiPassStorage,
    field = inner,
    invalidate = ffi::VTMultiPassStorageClose,
    release = ffi::CFRelease,
);

impl MultiPassStorage {
    /// CoreFoundation type identifier for `VTMultiPassStorage`.
    #[must_use]
    pub fn type_id() -> usize {
        unsafe { ffi::VTMultiPassStorageGetTypeID() }
    }

    /// Create a new multi-pass storage backed by a temp file.
    ///
    /// # Errors
    ///
    /// Returns [`VTError::SessionCreateFailed`] on failure.
    pub fn new() -> Result<Self, VTError> {
        let mut p: ffi::VTMultiPassStorageRef = ptr::null_mut();
        let s = unsafe {
            ffi::VTMultiPassStorageCreate(
                ffi::kCFAllocatorDefault,
                ptr::null(),
                ffi::CMTimeRange::INVALID,
                ptr::null(),
                &mut p,
            )
        };
        if s != 0 || p.is_null() {
            return Err(VTError::SessionCreateFailed(s));
        }
        Ok(Self { inner: p })
    }

    /// Flush pending data and close the backing storage file.
    ///
    /// # Errors
    ///
    /// Returns [`VTError::ApiFailed`] on a non-zero `OSStatus`.
    pub fn close(&self) -> Result<(), VTError> {
        let status = unsafe { ffi::VTMultiPassStorageClose(self.inner) };
        if status == 0 {
            Ok(())
        } else {
            Err(VTError::ApiFailed {
                api: "VTMultiPassStorageClose",
                status,
            })
        }
    }

    /// Raw `VTMultiPassStorageRef` — pass to a
    /// `CompressionSession::set_property` with
    /// `kVTCompressionPropertyKey_MultiPassStorage`.
    #[must_use]
    pub const fn as_ptr(&self) -> ffi::VTMultiPassStorageRef {
        self.inner
    }
}

unsafe extern "C" fn frame_silo_collect_sample_buffer(
    refcon: *mut c_void,
    sample_buffer: ffi::CMSampleBufferRef,
) -> ffi::OSStatus {
    let Some(samples) = (unsafe { refcon.cast::<Vec<CMSampleBuffer>>().as_mut() }) else {
        return -1;
    };
    let Some(sample) = apple_cf::cm::CMSampleBuffer::from_raw_retained(sample_buffer.cast()) else {
        return -1;
    };
    samples.push(sample);
    0
}
