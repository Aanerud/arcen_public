//! [`PixelTransferSession`] and [`PixelRotationSession`] — Apple's
//! zero-copy pixel-format conversion / scaling / rotation engines.
//!
//! Both are typically used as pre/post-encode stages in a video
//! pipeline:
//!
//! - `PixelTransferSession` copies + converts (and optionally scales)
//!   one `CVPixelBuffer` into another. Pixel format, color space, and
//!   dimensions of the destination buffer determine what conversion
//!   is performed.
//! - `PixelRotationSession` rotates one `CVPixelBuffer` into another
//!   in 90° increments (with optional horizontal/vertical flips).
//!   For 90°/270°, the destination's width and height must be swapped.

use core::ptr;

use apple_cf::cf::{CFDictionary, CFType};
use apple_cf::cv::CVPixelBuffer;

use crate::error::VTError;
use crate::ffi;
use crate::session;

/// `kVTRotation_*` enum values, mapped to a Rust enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rotation {
    None,
    Clockwise90,
    Half180,
    CounterClockwise90,
}

impl Rotation {
    fn as_cf_string(self) -> ffi::CFStringRef {
        unsafe {
            match self {
                Self::None => ffi::kVTRotation_0,
                Self::Clockwise90 => ffi::kVTRotation_CW90,
                Self::Half180 => ffi::kVTRotation_180,
                Self::CounterClockwise90 => ffi::kVTRotation_CCW90,
            }
        }
    }
}

/// `kVTScalingMode_*` values for `VTPixelTransferSession`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScalingMode {
    Normal,
    CropSourceToCleanAperture,
    Letterbox,
    Trim,
}

impl ScalingMode {
    fn as_cf_string(self) -> ffi::CFStringRef {
        unsafe {
            match self {
                Self::Normal => ffi::kVTScalingMode_Normal,
                Self::CropSourceToCleanAperture => ffi::kVTScalingMode_CropSourceToCleanAperture,
                Self::Letterbox => ffi::kVTScalingMode_Letterbox,
                Self::Trim => ffi::kVTScalingMode_Trim,
            }
        }
    }
}

/// `kVTDownsamplingMode_*` values for `VTPixelTransferSession`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownsamplingMode {
    Decimate,
    Average,
}

impl DownsamplingMode {
    fn as_cf_string(self) -> ffi::CFStringRef {
        unsafe {
            match self {
                Self::Decimate => ffi::kVTDownsamplingMode_Decimate,
                Self::Average => ffi::kVTDownsamplingMode_Average,
            }
        }
    }
}

/// Apple's `VTPixelTransferSession` — color/format/scale conversion
/// for `CVPixelBuffer`s.
pub struct PixelTransferSession {
    session: ffi::VTPixelTransferSessionRef,
}

unsafe impl Send for PixelTransferSession {}
unsafe impl Sync for PixelTransferSession {}

crate::utils::retained::vt_retained!(
    PixelTransferSession,
    field = session,
    invalidate = ffi::VTPixelTransferSessionInvalidate,
    release = ffi::CFRelease,
);

impl PixelTransferSession {
    /// CoreFoundation type identifier for `VTPixelTransferSession`.
    #[must_use]
    pub fn type_id() -> usize {
        unsafe { ffi::VTPixelTransferSessionGetTypeID() }
    }

    /// Create a new pixel transfer session.
    ///
    /// # Errors
    ///
    /// Returns [`VTError::SessionCreateFailed`] if Apple refuses.
    pub fn new() -> Result<Self, VTError> {
        let mut session: ffi::VTPixelTransferSessionRef = ptr::null_mut();
        let status =
            unsafe { ffi::VTPixelTransferSessionCreate(ffi::kCFAllocatorDefault, &mut session) };
        if status != 0 || session.is_null() {
            return Err(VTError::SessionCreateFailed(status));
        }
        Ok(Self { session })
    }

    /// Copy one `VTSession` property from the transfer session.
    ///
    /// # Errors
    ///
    /// Returns [`VTError::ApiFailed`] if the query fails.
    ///
    /// # Safety
    ///
    /// `key` must be a valid CoreFoundation string pointer accepted by
    /// `VTSessionCopyProperty` for a pixel-transfer session.
    pub unsafe fn copy_property(&self, key: ffi::CFStringRef) -> Result<Option<CFType>, VTError> {
        session::copy_property(self.session.cast(), key)
    }

    /// Copy the transfer session's supported-property dictionary.
    ///
    /// # Errors
    ///
    /// Returns [`VTError::ApiFailed`] if the query fails.
    pub fn supported_property_dictionary(&self) -> Result<CFDictionary, VTError> {
        unsafe { session::copy_supported_property_dictionary(self.session.cast()) }
    }

    /// Copy the transfer session's serializable property dictionary.
    ///
    /// # Errors
    ///
    /// Returns [`VTError::ApiFailed`] if the query fails.
    pub fn serializable_properties(&self) -> Result<CFDictionary, VTError> {
        unsafe { session::copy_serializable_properties(self.session.cast()) }
    }

    /// Set multiple transfer properties at once.
    ///
    /// # Errors
    ///
    /// Returns [`VTError::ApiFailed`] if `VideoToolbox` rejects the dictionary.
    pub fn set_properties(&self, properties: &CFDictionary) -> Result<(), VTError> {
        unsafe { session::set_properties(self.session.cast(), properties) }
    }

    /// Set the scaling mode used by [`Self::transfer`].
    ///
    /// # Errors
    ///
    /// Returns [`VTError::SetPropertyFailed`] on rejection.
    pub fn set_scaling_mode(&self, mode: ScalingMode) -> Result<(), VTError> {
        unsafe {
            self.set_property(
                ffi::kVTPixelTransferPropertyKey_ScalingMode,
                mode.as_cf_string().cast(),
            )
        }
    }

    /// Set the downsampling mode used by [`Self::transfer`].
    ///
    /// # Errors
    ///
    /// Returns [`VTError::SetPropertyFailed`] on rejection.
    pub fn set_downsampling_mode(&self, mode: DownsamplingMode) -> Result<(), VTError> {
        unsafe {
            self.set_property(
                ffi::kVTPixelTransferPropertyKey_DownsamplingMode,
                mode.as_cf_string().cast(),
            )
        }
    }

    /// Mark the transfer session as real-time.
    ///
    /// # Errors
    ///
    /// Returns [`VTError::SetPropertyFailed`] on rejection.
    pub fn set_real_time(&self, real_time: bool) -> Result<(), VTError> {
        let cf_value = unsafe {
            if real_time {
                ffi::kCFBooleanTrue
            } else {
                ffi::kCFBooleanFalse
            }
        };
        unsafe { self.set_property(ffi::kVTPixelTransferPropertyKey_RealTime, cf_value.cast()) }
    }

    /// Copy + convert pixels from `src` into `dst`. The destination's
    /// pixel format, color space, and dimensions determine the
    /// conversion / scaling that gets applied.
    ///
    /// # Errors
    ///
    /// Returns [`VTError::EncodeFailed`] on a non-zero `OSStatus`.
    pub fn transfer(&self, src: &CVPixelBuffer, dst: &CVPixelBuffer) -> Result<(), VTError> {
        let status = unsafe {
            ffi::VTPixelTransferSessionTransferImage(
                self.session,
                src.as_ptr().cast(),
                dst.as_ptr().cast(),
            )
        };
        if status == 0 {
            Ok(())
        } else {
            Err(VTError::EncodeFailed(status))
        }
    }

    /// Set an arbitrary `VTSession` property on this transfer session.
    ///
    /// # Errors
    ///
    /// Returns [`VTError::SetPropertyFailed`] on rejection.
    ///
    /// # Safety
    ///
    /// `key` / `value` must be valid CoreFoundation references
    /// matching Apple's `VTPixelTransferProperties.h` contract.
    pub unsafe fn set_property(
        &self,
        key: ffi::CFStringRef,
        value: ffi::CFTypeRef,
    ) -> Result<(), VTError> {
        let status = ffi::VTSessionSetProperty(self.session, key, value);
        if status != 0 {
            return Err(VTError::SetPropertyFailed {
                key: "<custom>".to_string(),
                status,
            });
        }
        Ok(())
    }
}

/// Apple's `VTPixelRotationSession` — 90° rotation + flip engine for
/// `CVPixelBuffer`s.
pub struct PixelRotationSession {
    session: ffi::VTPixelRotationSessionRef,
}

unsafe impl Send for PixelRotationSession {}
unsafe impl Sync for PixelRotationSession {}

crate::utils::retained::vt_retained!(
    PixelRotationSession,
    field = session,
    invalidate = ffi::VTPixelRotationSessionInvalidate,
    release = ffi::CFRelease,
);

impl PixelRotationSession {
    /// CoreFoundation type identifier for `VTPixelRotationSession`.
    #[must_use]
    pub fn type_id() -> usize {
        unsafe { ffi::VTPixelRotationSessionGetTypeID() }
    }

    /// Create a new pixel rotation session.
    ///
    /// # Errors
    ///
    /// Returns [`VTError::SessionCreateFailed`] if Apple refuses.
    pub fn new() -> Result<Self, VTError> {
        let mut session: ffi::VTPixelRotationSessionRef = ptr::null_mut();
        let status =
            unsafe { ffi::VTPixelRotationSessionCreate(ffi::kCFAllocatorDefault, &mut session) };
        if status != 0 || session.is_null() {
            return Err(VTError::SessionCreateFailed(status));
        }
        Ok(Self { session })
    }

    /// Copy one `VTSession` property from the rotation session.
    ///
    /// # Errors
    ///
    /// Returns [`VTError::ApiFailed`] if the query fails.
    ///
    /// # Safety
    ///
    /// `key` must be a valid CoreFoundation string pointer accepted by
    /// `VTSessionCopyProperty` for a pixel-rotation session.
    pub unsafe fn copy_property(&self, key: ffi::CFStringRef) -> Result<Option<CFType>, VTError> {
        session::copy_property(self.session.cast(), key)
    }

    /// Copy the rotation session's supported-property dictionary.
    ///
    /// # Errors
    ///
    /// Returns [`VTError::ApiFailed`] if the query fails.
    pub fn supported_property_dictionary(&self) -> Result<CFDictionary, VTError> {
        unsafe { session::copy_supported_property_dictionary(self.session.cast()) }
    }

    /// Copy the rotation session's serializable property dictionary.
    ///
    /// # Errors
    ///
    /// Returns [`VTError::ApiFailed`] if the query fails.
    pub fn serializable_properties(&self) -> Result<CFDictionary, VTError> {
        unsafe { session::copy_serializable_properties(self.session.cast()) }
    }

    /// Set multiple rotation properties at once.
    ///
    /// # Errors
    ///
    /// Returns [`VTError::ApiFailed`] if `VideoToolbox` rejects the dictionary.
    pub fn set_properties(&self, properties: &CFDictionary) -> Result<(), VTError> {
        unsafe { session::set_properties(self.session.cast(), properties) }
    }

    /// Configure the rotation applied by [`Self::rotate`]. Wraps
    /// `kVTPixelRotationPropertyKey_Rotation`.
    ///
    /// # Errors
    ///
    /// Returns [`VTError::SetPropertyFailed`] on rejection.
    pub fn set_rotation(&self, rotation: Rotation) -> Result<(), VTError> {
        let v = rotation.as_cf_string();
        let status = unsafe {
            ffi::VTSessionSetProperty(
                self.session,
                ffi::kVTPixelRotationPropertyKey_Rotation,
                v.cast(),
            )
        };
        if status != 0 {
            return Err(VTError::SetPropertyFailed {
                key: "Rotation".to_string(),
                status,
            });
        }
        Ok(())
    }

    /// Toggle horizontal flip. Wraps
    /// `kVTPixelRotationPropertyKey_FlipHorizontalOrientation`.
    ///
    /// # Errors
    ///
    /// Returns [`VTError::SetPropertyFailed`] on rejection.
    pub fn set_flip_horizontal(&self, flip: bool) -> Result<(), VTError> {
        let cf = unsafe {
            if flip {
                ffi::kCFBooleanTrue
            } else {
                ffi::kCFBooleanFalse
            }
        };
        let status = unsafe {
            ffi::VTSessionSetProperty(
                self.session,
                ffi::kVTPixelRotationPropertyKey_FlipHorizontalOrientation,
                cf.cast(),
            )
        };
        if status != 0 {
            return Err(VTError::SetPropertyFailed {
                key: "FlipHorizontalOrientation".to_string(),
                status,
            });
        }
        Ok(())
    }

    /// Toggle vertical flip. Wraps
    /// `kVTPixelRotationPropertyKey_FlipVerticalOrientation`.
    ///
    /// # Errors
    ///
    /// Returns [`VTError::SetPropertyFailed`] on rejection.
    pub fn set_flip_vertical(&self, flip: bool) -> Result<(), VTError> {
        let cf = unsafe {
            if flip {
                ffi::kCFBooleanTrue
            } else {
                ffi::kCFBooleanFalse
            }
        };
        let status = unsafe {
            ffi::VTSessionSetProperty(
                self.session,
                ffi::kVTPixelRotationPropertyKey_FlipVerticalOrientation,
                cf.cast(),
            )
        };
        if status != 0 {
            return Err(VTError::SetPropertyFailed {
                key: "FlipVerticalOrientation".to_string(),
                status,
            });
        }
        Ok(())
    }

    /// Rotate (and optionally flip) `src` into `dst`. For 90°/270°
    /// rotations the destination buffer's width and height must be
    /// the inverse of the source's.
    ///
    /// # Errors
    ///
    /// Returns [`VTError::EncodeFailed`] on a non-zero `OSStatus`.
    pub fn rotate(&self, src: &CVPixelBuffer, dst: &CVPixelBuffer) -> Result<(), VTError> {
        let status = unsafe {
            ffi::VTPixelRotationSessionRotateImage(
                self.session,
                src.as_ptr().cast(),
                dst.as_ptr().cast(),
            )
        };
        if status == 0 {
            Ok(())
        } else {
            Err(VTError::EncodeFailed(status))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{DownsamplingMode, Rotation, ScalingMode};
    use crate::ffi;

    #[test]
    fn rotation_variants_map_to_expected_cfstring_constants() {
        assert_eq!(Rotation::None.as_cf_string(), unsafe { ffi::kVTRotation_0 });
        assert_eq!(
            Rotation::Clockwise90.as_cf_string(),
            unsafe { ffi::kVTRotation_CW90 }
        );
        assert_eq!(Rotation::Half180.as_cf_string(), unsafe { ffi::kVTRotation_180 });
        assert_eq!(
            Rotation::CounterClockwise90.as_cf_string(),
            unsafe { ffi::kVTRotation_CCW90 }
        );
    }

    #[test]
    fn scaling_modes_map_to_expected_cfstring_constants() {
        assert_eq!(ScalingMode::Normal.as_cf_string(), unsafe { ffi::kVTScalingMode_Normal });
        assert_eq!(
            ScalingMode::CropSourceToCleanAperture.as_cf_string(),
            unsafe { ffi::kVTScalingMode_CropSourceToCleanAperture }
        );
        assert_eq!(
            ScalingMode::Letterbox.as_cf_string(),
            unsafe { ffi::kVTScalingMode_Letterbox }
        );
        assert_eq!(ScalingMode::Trim.as_cf_string(), unsafe { ffi::kVTScalingMode_Trim });
    }

    #[test]
    fn downsampling_modes_map_to_expected_cfstring_constants() {
        assert_eq!(
            DownsamplingMode::Decimate.as_cf_string(),
            unsafe { ffi::kVTDownsamplingMode_Decimate }
        );
        assert_eq!(
            DownsamplingMode::Average.as_cf_string(),
            unsafe { ffi::kVTDownsamplingMode_Average }
        );
    }
}
