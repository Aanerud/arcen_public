//! [`available_video_encoders`] / [`available_video_encoder_details`] —
//! enumerate Apple's installed video encoders and their selection metadata.

use core::ffi::{c_char, c_void};
use core::ptr;

use apple_cf::cf::{CFDictionary, CFNumber, CFString};

use crate::ffi;
use crate::session::Codec;

/// One entry from `VTCopyVideoEncoderList`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoEncoder {
    /// `CMVideoCodecType` four-character code (e.g. `0x61766331` for `'avc1'`).
    pub codec_type: u32,
    /// Reverse-DNS unique encoder id (e.g.
    /// `"com.apple.videotoolbox.videoencoder.h264"`).
    pub encoder_id: String,
    /// Codec name (e.g. `"H.264"`).
    pub codec_name: String,
    /// Encoder name (e.g. `"Apple H.264"`).
    pub encoder_name: String,
    /// Display name (codec name if there's only one encoder for that
    /// format, otherwise the encoder name).
    pub display_name: String,
}

/// Extended metadata attached to `VTCopyVideoEncoderList` entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoEncoderDetails {
    /// Basic encoder identity fields.
    pub base: VideoEncoder,
    /// `MTLDevice::registryID()` when the encoder is pinned to a specific GPU.
    pub gpu_registry_id: Option<u64>,
    /// Encoder-selection properties Apple suggests consulting during selection.
    pub supported_selection_properties: Option<CFDictionary>,
    /// Relative performance score among encoders for the same codec.
    pub performance_rating: Option<i64>,
    /// Relative quality score among encoders for the same codec.
    pub quality_rating: Option<i64>,
    /// Whether this encoder is globally instance-limited.
    pub instance_limit: Option<bool>,
    /// Whether this encoder is hardware accelerated.
    pub is_hardware_accelerated: Option<bool>,
    /// Whether this encoder supports frame reordering (B-frames).
    pub supports_frame_reordering: Option<bool>,
}

/// Options passed to `VTCopyVideoEncoderList`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VideoEncoderListOptions {
    include_standard_definition_dv_encoders: bool,
}

impl VideoEncoderListOptions {
    /// Default options.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            include_standard_definition_dv_encoders: false,
        }
    }

    /// Include standard-definition DV encoders in the returned list.
    #[must_use]
    pub const fn with_include_standard_definition_dv_encoders(mut self, include: bool) -> Self {
        self.include_standard_definition_dv_encoders = include;
        self
    }

    fn to_cf_dictionary(self) -> Option<CFDictionary> {
        if !self.include_standard_definition_dv_encoders {
            return None;
        }
        Some(dictionary_with_boolean(
            unsafe { ffi::kVTVideoEncoderListOption_IncludeStandardDefinitionDVEncoders },
            true,
        ))
    }
}

/// Supported-property dictionary returned by
/// `VTCopySupportedPropertyDictionaryForEncoder`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncoderSupportedProperties {
    /// Encoder identifier Apple selected for the query.
    pub encoder_id: Option<String>,
    /// Supported property dictionary for that encoder/format combination.
    pub supported_properties: Option<CFDictionary>,
}

/// Enumerate all video encoders installed on this system.
///
/// # Errors
///
/// Returns the raw `OSStatus` from `VTCopyVideoEncoderList` on
/// failure.
pub fn available_video_encoders() -> Result<Vec<VideoEncoder>, i32> {
    available_video_encoder_details().map(|encoders| {
        encoders
            .into_iter()
            .map(|details| details.base)
            .collect::<Vec<_>>()
    })
}

/// Enumerate all video encoders installed on this system, including the
/// extended metadata Apple publishes in `VTVideoEncoderList.h`.
///
/// # Errors
///
/// Returns the raw `OSStatus` from `VTCopyVideoEncoderList` on failure.
pub fn available_video_encoder_details() -> Result<Vec<VideoEncoderDetails>, i32> {
    available_video_encoder_details_with_options(&VideoEncoderListOptions::default())
}

/// Enumerate video encoders using the given list options.
///
/// # Errors
///
/// Returns the raw `OSStatus` from `VTCopyVideoEncoderList` on failure.
pub fn available_video_encoder_details_with_options(
    options: &VideoEncoderListOptions,
) -> Result<Vec<VideoEncoderDetails>, i32> {
    let options_dict = options.to_cf_dictionary();
    let mut arr: ffi::CFArrayRef = ptr::null();
    let status = unsafe {
        ffi::VTCopyVideoEncoderList(
            options_dict
                .as_ref()
                .map_or(ptr::null(), |dict| dict.as_ptr().cast_const().cast()),
            &mut arr,
        )
    };
    if status != 0 {
        return Err(status);
    }
    if arr.is_null() {
        return Ok(Vec::new());
    }
    let count = unsafe { ffi::CFArrayGetCount(arr) };
    let mut encoders = Vec::with_capacity(usize::try_from(count).unwrap_or(0));
    for index in 0..count {
        let dict: ffi::CFDictionaryRef = unsafe { ffi::CFArrayGetValueAtIndex(arr, index) }.cast();
        if !dict.is_null() {
            encoders.push(unsafe { parse_video_encoder_details(dict) });
        }
    }
    unsafe { ffi::CFRelease(arr.cast()) };
    Ok(encoders)
}

/// Query the supported-property dictionary for a specific codec/size combination.
///
/// Pass `encoder_id` to pin the query to one encoder from
/// [`available_video_encoder_details`] / [`available_video_encoders`].
///
/// # Errors
///
/// Returns the raw `OSStatus` from `VTCopySupportedPropertyDictionaryForEncoder`
/// on failure.
pub fn supported_property_dictionary_for_encoder(
    width: i32,
    height: i32,
    codec: Codec,
    encoder_id: Option<&str>,
) -> Result<EncoderSupportedProperties, i32> {
    let encoder_specification = encoder_id.map(dictionary_with_encoder_id);
    let mut encoder_id_out: ffi::CFStringRef = ptr::null();
    let mut supported_properties_out: ffi::CFDictionaryRef = ptr::null();
    let status = unsafe {
        ffi::VTCopySupportedPropertyDictionaryForEncoder(
            width,
            height,
            codec.as_cm_codec_type(),
            encoder_specification
                .as_ref()
                .map_or(ptr::null(), |dict| dict.as_ptr().cast_const().cast()),
            &mut encoder_id_out,
            &mut supported_properties_out,
        )
    };
    if status != 0 {
        return Err(status);
    }

    let encoder_id =
        CFString::from_raw(encoder_id_out.cast_mut().cast()).map(|string| string.to_string_lossy());
    let supported_properties = CFDictionary::from_raw(supported_properties_out.cast_mut().cast());

    Ok(EncoderSupportedProperties {
        encoder_id,
        supported_properties,
    })
}

unsafe fn parse_video_encoder_details(dict: ffi::CFDictionaryRef) -> VideoEncoderDetails {
    let codec_type = unsafe {
        let number = ffi::CFDictionaryGetValue(dict, ffi::kVTVideoEncoderList_CodecType.cast());
        cf_number_u32(number).unwrap_or(0)
    };
    let encoder_id = unsafe {
        let string = ffi::CFDictionaryGetValue(dict, ffi::kVTVideoEncoderList_EncoderID.cast());
        cf_string_to_rust(string.cast())
    };
    let codec_name = unsafe {
        let string = ffi::CFDictionaryGetValue(dict, ffi::kVTVideoEncoderList_CodecName.cast());
        cf_string_to_rust(string.cast())
    };
    let encoder_name = unsafe {
        let string = ffi::CFDictionaryGetValue(dict, ffi::kVTVideoEncoderList_EncoderName.cast());
        cf_string_to_rust(string.cast())
    };
    let display_name = unsafe {
        let string = ffi::CFDictionaryGetValue(dict, ffi::kVTVideoEncoderList_DisplayName.cast());
        cf_string_to_rust(string.cast())
    };

    VideoEncoderDetails {
        base: VideoEncoder {
            codec_type,
            encoder_id,
            codec_name,
            encoder_name,
            display_name,
        },
        gpu_registry_id: unsafe {
            let value =
                ffi::CFDictionaryGetValue(dict, ffi::kVTVideoEncoderList_GPURegistryID.cast());
            cf_number_u64(value)
        },
        supported_selection_properties: unsafe {
            let value = ffi::CFDictionaryGetValue(
                dict,
                ffi::kVTVideoEncoderList_SupportedSelectionProperties.cast(),
            );
            cf_dictionary(value)
        },
        performance_rating: unsafe {
            let value =
                ffi::CFDictionaryGetValue(dict, ffi::kVTVideoEncoderList_PerformanceRating.cast());
            cf_number_i64(value)
        },
        quality_rating: unsafe {
            let value =
                ffi::CFDictionaryGetValue(dict, ffi::kVTVideoEncoderList_QualityRating.cast());
            cf_number_i64(value)
        },
        instance_limit: unsafe {
            let value =
                ffi::CFDictionaryGetValue(dict, ffi::kVTVideoEncoderList_InstanceLimit.cast());
            cf_bool(value)
        },
        is_hardware_accelerated: unsafe {
            let value = ffi::CFDictionaryGetValue(
                dict,
                ffi::kVTVideoEncoderList_IsHardwareAccelerated.cast(),
            );
            cf_bool(value)
        },
        supports_frame_reordering: unsafe {
            let value = ffi::CFDictionaryGetValue(
                dict,
                ffi::kVTVideoEncoderList_SupportsFrameReordering.cast(),
            );
            cf_bool(value)
        },
    }
}

fn dictionary_with_boolean(key: ffi::CFStringRef, value: bool) -> CFDictionary {
    let dict = unsafe {
        ffi::CFDictionaryCreateMutable(
            ffi::kCFAllocatorDefault,
            1,
            (&raw const ffi::kCFTypeDictionaryKeyCallBacks).cast(),
            (&raw const ffi::kCFTypeDictionaryValueCallBacks).cast(),
        )
    };
    unsafe {
        ffi::CFDictionarySetValue(
            dict,
            key.cast(),
            if value {
                ffi::kCFBooleanTrue.cast()
            } else {
                ffi::kCFBooleanFalse.cast()
            },
        );
    }
    CFDictionary::from_raw(dict.cast()).expect("CFDictionaryCreateMutable returned NULL")
}

fn dictionary_with_encoder_id(encoder_id: &str) -> CFDictionary {
    let dict = unsafe {
        ffi::CFDictionaryCreateMutable(
            ffi::kCFAllocatorDefault,
            1,
            (&raw const ffi::kCFTypeDictionaryKeyCallBacks).cast(),
            (&raw const ffi::kCFTypeDictionaryValueCallBacks).cast(),
        )
    };
    let encoder_id = CFString::new(encoder_id);
    unsafe {
        ffi::CFDictionarySetValue(
            dict,
            ffi::kVTVideoEncoderSpecification_EncoderID.cast(),
            encoder_id.as_ptr().cast(),
        );
    }
    CFDictionary::from_raw(dict.cast()).expect("CFDictionaryCreateMutable returned NULL")
}

#[allow(
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap
)]
unsafe fn cf_string_to_rust(s: ffi::CFStringRef) -> String {
    if s.is_null() {
        return String::new();
    }
    let len = unsafe { ffi::CFStringGetLength(s) };
    let buf_len = usize::try_from(len * 4 + 1).unwrap_or(0);
    let mut buf = vec![0u8; buf_len];
    let ok = unsafe {
        ffi::CFStringGetCString(
            s,
            buf.as_mut_ptr().cast::<c_char>(),
            buf_len as isize,
            0x0800_0100,
        )
    };
    if !ok {
        return String::new();
    }
    if let Some(end) = buf.iter().position(|&b| b == 0) {
        buf.truncate(end);
    }
    String::from_utf8_lossy(&buf).into_owned()
}

#[allow(clippy::cast_sign_loss)]
unsafe fn cf_number_u32(n: *const c_void) -> Option<u32> {
    if n.is_null() {
        return None;
    }
    let mut v: i32 = 0;
    let ok = unsafe {
        ffi::CFNumberGetValue(
            n.cast(),
            ffi::kCFNumberSInt32Type,
            (&raw mut v).cast::<c_void>(),
        )
    };
    ok.then_some(v as u32)
}

unsafe fn cf_number_i64(n: *const c_void) -> Option<i64> {
    unsafe { CFNumber::from_raw_retained(n.cast_mut()) }.and_then(|number| number.to_i64())
}

unsafe fn cf_number_u64(n: *const c_void) -> Option<u64> {
    unsafe { CFNumber::from_raw_retained(n.cast_mut()) }.and_then(|number| number.to_u64())
}

unsafe fn cf_dictionary(value: *const c_void) -> Option<CFDictionary> {
    unsafe { CFDictionary::from_raw_retained(value.cast_mut()) }
}

unsafe fn cf_bool(value: *const c_void) -> Option<bool> {
    if value.is_null() {
        None
    } else if value == unsafe { ffi::kCFBooleanTrue.cast() } {
        Some(true)
    } else if value == unsafe { ffi::kCFBooleanFalse.cast() } {
        Some(false)
    } else {
        None
    }
}
