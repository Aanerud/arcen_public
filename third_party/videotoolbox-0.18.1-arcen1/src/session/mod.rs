//! Video codecs supported by `VideoToolbox`.

use core::ffi::c_void;
use core::ptr;

use apple_cf::cf::{CFDictionary, CFType};

use crate::error::VTError;
use crate::ffi;

/// Video codec families. Maps to `CMVideoCodecType` four-character codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Codec {
    /// H.264 / AVC
    H264,
    /// H.265 / HEVC
    HEVC,
    /// Apple `ProRes` 422 (Standard Definition)
    ProRes422,
    /// Apple `ProRes` 422 HQ
    ProRes422HQ,
    /// Apple `ProRes` 422 LT
    ProRes422LT,
    /// Apple `ProRes` 422 Proxy
    ProRes422Proxy,
    /// Apple `ProRes` 4444 (with alpha)
    ProRes4444,
}

impl Codec {
    /// Lower into a `CMVideoCodecType` for the FFI boundary.
    #[must_use]
    pub const fn as_cm_codec_type(self) -> ffi::CMVideoCodecType {
        match self {
            Self::H264 => ffi::kCMVideoCodecType_H264,
            Self::HEVC => ffi::kCMVideoCodecType_HEVC,
            Self::ProRes422 => ffi::kCMVideoCodecType_AppleProRes422,
            Self::ProRes422HQ => ffi::kCMVideoCodecType_AppleProRes422HQ,
            Self::ProRes422LT => ffi::kCMVideoCodecType_AppleProRes422LT,
            Self::ProRes422Proxy => ffi::kCMVideoCodecType_AppleProRes422Proxy,
            Self::ProRes4444 => ffi::kCMVideoCodecType_AppleProRes4444,
        }
    }
}

pub(crate) unsafe fn copy_property(
    session: *mut c_void,
    key: ffi::CFStringRef,
) -> Result<Option<CFType>, VTError> {
    let mut value: *mut c_void = ptr::null_mut();
    // SAFETY: `VTSessionCopyProperty` is a standard Apple SDK function.
    // The session pointer is validated by the caller. The key is a valid CFStringRef.
    // The value pointer is properly initialized on return.
    let status = unsafe {
        ffi::VTSessionCopyProperty(
            session,
            key,
            ffi::kCFAllocatorDefault,
            (&raw mut value).cast(),
        )
    };
    if status != 0 {
        return Err(VTError::ApiFailed {
            api: "VTSessionCopyProperty",
            status,
        });
    }
    Ok(CFType::from_raw(value))
}

pub(crate) unsafe fn copy_supported_property_dictionary(
    session: *mut c_void,
) -> Result<CFDictionary, VTError> {
    let mut out: ffi::CFDictionaryRef = ptr::null();
    // SAFETY: `VTSessionCopySupportedPropertyDictionary` is a standard Apple SDK function.
    // The session pointer is validated by the caller. The out pointer is properly initialized.
    let status = unsafe { ffi::VTSessionCopySupportedPropertyDictionary(session, &mut out) };
    if status != 0 || out.is_null() {
        return Err(VTError::ApiFailed {
            api: "VTSessionCopySupportedPropertyDictionary",
            status,
        });
    }
    CFDictionary::from_raw(out.cast_mut().cast()).ok_or(VTError::ApiFailed {
        api: "VTSessionCopySupportedPropertyDictionary",
        status,
    })
}

pub(crate) unsafe fn copy_serializable_properties(
    session: *mut c_void,
) -> Result<CFDictionary, VTError> {
    let mut out: ffi::CFDictionaryRef = ptr::null();
    // SAFETY: `VTSessionCopySerializableProperties` is a standard Apple SDK function.
    // The session pointer is validated by the caller. The out pointer is properly initialized.
    let status = unsafe {
        ffi::VTSessionCopySerializableProperties(session, ffi::kCFAllocatorDefault, &mut out)
    };
    if status != 0 || out.is_null() {
        return Err(VTError::ApiFailed {
            api: "VTSessionCopySerializableProperties",
            status,
        });
    }
    CFDictionary::from_raw(out.cast_mut().cast()).ok_or(VTError::ApiFailed {
        api: "VTSessionCopySerializableProperties",
        status,
    })
}

pub(crate) unsafe fn set_properties(
    session: *mut c_void,
    properties: &CFDictionary,
) -> Result<(), VTError> {
    let status = unsafe { ffi::VTSessionSetProperties(session, properties.as_ptr().cast()) };
    if status == 0 {
        Ok(())
    } else {
        Err(VTError::ApiFailed {
            api: "VTSessionSetProperties",
            status,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::Codec;
    use crate::ffi;

    #[test]
    fn h264_and_hevc_codecs_match_expected_fourcc_values() {
        assert_eq!(Codec::H264.as_cm_codec_type(), u32::from_be_bytes(*b"avc1"));
        assert_eq!(Codec::H264.as_cm_codec_type(), ffi::kCMVideoCodecType_H264);
        assert_eq!(Codec::HEVC.as_cm_codec_type(), u32::from_be_bytes(*b"hvc1"));
        assert_eq!(Codec::HEVC.as_cm_codec_type(), ffi::kCMVideoCodecType_HEVC);
    }

    #[test]
    fn prores_family_codecs_match_expected_fourcc_values() {
        assert_eq!(
            Codec::ProRes422.as_cm_codec_type(),
            u32::from_be_bytes(*b"apcn")
        );
        assert_eq!(
            Codec::ProRes422HQ.as_cm_codec_type(),
            u32::from_be_bytes(*b"apch")
        );
        assert_eq!(
            Codec::ProRes422LT.as_cm_codec_type(),
            u32::from_be_bytes(*b"apcs")
        );
        assert_eq!(
            Codec::ProRes422Proxy.as_cm_codec_type(),
            u32::from_be_bytes(*b"apco")
        );
        assert_eq!(
            Codec::ProRes4444.as_cm_codec_type(),
            u32::from_be_bytes(*b"ap4h")
        );
    }

    #[test]
    fn codec_mappings_are_unique_across_variants() {
        let codecs = [
            Codec::H264,
            Codec::HEVC,
            Codec::ProRes422,
            Codec::ProRes422HQ,
            Codec::ProRes422LT,
            Codec::ProRes422Proxy,
            Codec::ProRes4444,
        ];
        let mut seen = HashSet::new();

        for codec in codecs {
            assert!(seen.insert(codec.as_cm_codec_type()));
        }
        assert_eq!(seen.len(), 7);
    }
}
