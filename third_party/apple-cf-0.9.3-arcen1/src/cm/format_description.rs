//! `CMFormatDescription` - Media format description

#![allow(dead_code)]

use crate::{
    cf::{CFArray, CFDictionary},
    ffi,
};
use std::{fmt, ops::Deref};

/// Owned wrapper around `CMFormatDescriptionRef`.
pub struct CMFormatDescription(*mut std::ffi::c_void);

impl PartialEq for CMFormatDescription {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl Eq for CMFormatDescription {}

impl std::hash::Hash for CMFormatDescription {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        unsafe {
            let hash_value = ffi::cm_format_description_hash(self.0);
            hash_value.hash(state);
        }
    }
}

/// Common media type constants
pub mod media_types {
    use crate::utils::four_char_code::FourCharCode;

    /// Video media type ('vide')
    pub const VIDEO: FourCharCode = FourCharCode::from_bytes(*b"vide");
    /// Audio media type ('soun')
    pub const AUDIO: FourCharCode = FourCharCode::from_bytes(*b"soun");
    /// Muxed media type ('mux ')
    pub const MUXED: FourCharCode = FourCharCode::from_bytes(*b"mux ");
    /// Text/subtitle media type ('text')
    pub const TEXT: FourCharCode = FourCharCode::from_bytes(*b"text");
    /// Closed caption media type ('clcp')
    pub const CLOSED_CAPTION: FourCharCode = FourCharCode::from_bytes(*b"clcp");
    /// Metadata media type ('meta')
    pub const METADATA: FourCharCode = FourCharCode::from_bytes(*b"meta");
    /// Timecode media type ('tmcd')
    pub const TIMECODE: FourCharCode = FourCharCode::from_bytes(*b"tmcd");
}

/// Common codec type constants
pub mod codec_types {
    use crate::utils::four_char_code::FourCharCode;

    // Video codecs
    /// H.264/AVC ('avc1')
    pub const H264: FourCharCode = FourCharCode::from_bytes(*b"avc1");
    /// HEVC/H.265 ('hvc1')
    pub const HEVC: FourCharCode = FourCharCode::from_bytes(*b"hvc1");
    /// HEVC/H.265 alternative ('hev1')
    pub const HEVC_2: FourCharCode = FourCharCode::from_bytes(*b"hev1");
    /// JPEG ('jpeg')
    pub const JPEG: FourCharCode = FourCharCode::from_bytes(*b"jpeg");
    /// Apple `ProRes` 422 ('apcn')
    pub const PRORES_422: FourCharCode = FourCharCode::from_bytes(*b"apcn");
    /// Apple `ProRes` 4444 ('ap4h')
    pub const PRORES_4444: FourCharCode = FourCharCode::from_bytes(*b"ap4h");

    // Audio codecs
    /// AAC ('aac ')
    pub const AAC: FourCharCode = FourCharCode::from_bytes(*b"aac ");
    /// Linear PCM ('lpcm')
    pub const LPCM: FourCharCode = FourCharCode::from_bytes(*b"lpcm");
    /// Apple Lossless ('alac')
    pub const ALAC: FourCharCode = FourCharCode::from_bytes(*b"alac");
    /// Opus ('opus')
    pub const OPUS: FourCharCode = FourCharCode::from_bytes(*b"opus");
    /// FLAC ('flac')
    pub const FLAC: FourCharCode = FourCharCode::from_bytes(*b"flac");
}

/// Metadata format subtypes (`CMMetadataFormatType`).
pub mod metadata_format_types {
    use crate::utils::four_char_code::FourCharCode;

    /// `SHOUTCast` / `ICY` metadata ('icy ').
    pub const ICY: FourCharCode = FourCharCode::from_bytes(*b"icy ");
    /// ID3 metadata ('id3 ').
    pub const ID3: FourCharCode = FourCharCode::from_bytes(*b"id3 ");
    /// Boxed metadata ('mebx').
    pub const BOXED: FourCharCode = FourCharCode::from_bytes(*b"mebx");
    /// Event message metadata ('emsg').
    pub const EMSG: FourCharCode = FourCharCode::from_bytes(*b"emsg");
}

macro_rules! cfstring_constant_fn {
    ($vis:vis fn $name:ident => $ffi_name:ident) => {
        #[must_use]
        $vis fn $name() -> CFString {
            let ptr = unsafe { ffi::$ffi_name() };
            CFString::from_raw(ptr).expect(concat!(stringify!($ffi_name), " returned NULL"))
        }
    };
}

/// `CMFormatDescription` extension keys related to metadata descriptions.
pub mod format_description_extension_keys {
    use crate::{cf::CFString, ffi};

    cfstring_constant_fn!(pub fn metadata_key_table => cm_metadata_format_description_extension_key_metadata_key_table);
}

/// `kCMMetadataFormatDescriptionKey_*` constants.
pub mod metadata_description_keys {
    use crate::{cf::CFString, ffi};

    cfstring_constant_fn!(pub fn conforming_data_types => cm_metadata_format_description_key_conforming_data_types);
    cfstring_constant_fn!(pub fn data_type => cm_metadata_format_description_key_data_type);
    cfstring_constant_fn!(pub fn data_type_namespace => cm_metadata_format_description_key_data_type_namespace);
    cfstring_constant_fn!(pub fn language_tag => cm_metadata_format_description_key_language_tag);
    cfstring_constant_fn!(pub fn local_id => cm_metadata_format_description_key_local_id);
    cfstring_constant_fn!(pub fn namespace => cm_metadata_format_description_key_namespace);
    cfstring_constant_fn!(pub fn setup_data => cm_metadata_format_description_key_setup_data);
    cfstring_constant_fn!(pub fn structural_dependency => cm_metadata_format_description_key_structural_dependency);
    cfstring_constant_fn!(pub fn value => cm_metadata_format_description_key_value);
}

/// `kCMMetadataFormatDescriptionMetadataSpecificationKey_*` constants.
pub mod metadata_specification_keys {
    use crate::{cf::CFString, ffi};

    cfstring_constant_fn!(pub fn data_type => cm_metadata_format_description_metadata_specification_key_data_type);
    cfstring_constant_fn!(pub fn extended_language_tag => cm_metadata_format_description_metadata_specification_key_extended_language_tag);
    cfstring_constant_fn!(pub fn identifier => cm_metadata_format_description_metadata_specification_key_identifier);
    cfstring_constant_fn!(pub fn setup_data => cm_metadata_format_description_metadata_specification_key_setup_data);
    cfstring_constant_fn!(pub fn structural_dependency => cm_metadata_format_description_metadata_specification_key_structural_dependency);
}

/// `kCMMetadataFormatDescription_StructuralDependencyKey_*` constants.
pub mod metadata_structural_dependency_keys {
    use crate::{cf::CFString, ffi};

    cfstring_constant_fn!(pub fn dependency_is_invalid_flag => cm_metadata_format_description_structural_dependency_key_dependency_is_invalid_flag);
}

impl CMFormatDescription {
    /// Wraps a +1 retained `CMFormatDescriptionRef` and returns `None` for null.
    pub fn from_raw(ptr: *mut std::ffi::c_void) -> Option<Self> {
        if ptr.is_null() {
            None
        } else {
            Some(Self(ptr))
        }
    }

    /// Wraps a raw `CMFormatDescriptionRef` by taking ownership without retaining it.
    ///
    /// # Safety
    /// The caller must ensure `ptr` is a valid +1 retained `CMFormatDescriptionRef`.
    pub const unsafe fn from_ptr(ptr: *mut std::ffi::c_void) -> Self {
        Self(ptr)
    }

    /// Returns the wrapped raw `CMFormatDescriptionRef`.
    #[must_use]
    pub const fn as_ptr(&self) -> *mut std::ffi::c_void {
        self.0
    }

    /// Get the media type as a raw u32 value
    #[must_use]
    pub fn media_type_raw(&self) -> u32 {
        unsafe { ffi::cm_format_description_get_media_type(self.0) }
    }

    /// Get the media type as `FourCharCode`
    #[must_use]
    pub fn media_type(&self) -> crate::utils::four_char_code::FourCharCode {
        crate::utils::four_char_code::FourCharCode::from(self.media_type_raw())
    }

    /// Get the media subtype (codec type) as a raw u32 value
    #[must_use]
    pub fn media_subtype_raw(&self) -> u32 {
        unsafe { ffi::cm_format_description_get_media_subtype(self.0) }
    }

    /// Get the media subtype as `FourCharCode`
    #[must_use]
    pub fn media_subtype(&self) -> crate::utils::four_char_code::FourCharCode {
        crate::utils::four_char_code::FourCharCode::from(self.media_subtype_raw())
    }

    /// Get format description extensions
    #[must_use]
    pub fn extensions(&self) -> Option<*const std::ffi::c_void> {
        unsafe {
            let ptr = ffi::cm_format_description_get_extensions(self.0);
            if ptr.is_null() {
                None
            } else {
                Some(ptr)
            }
        }
    }

    /// Check if this is a video format description
    #[must_use]
    pub fn is_video(&self) -> bool {
        self.media_type() == media_types::VIDEO
    }

    /// Check if this is an audio format description
    #[must_use]
    pub fn is_audio(&self) -> bool {
        self.media_type() == media_types::AUDIO
    }

    /// Check if this is a muxed format description
    #[must_use]
    pub fn is_muxed(&self) -> bool {
        self.media_type() == media_types::MUXED
    }

    /// Check if this is a text/subtitle format description
    #[must_use]
    pub fn is_text(&self) -> bool {
        self.media_type() == media_types::TEXT
    }

    /// Check if this is a closed caption format description
    #[must_use]
    pub fn is_closed_caption(&self) -> bool {
        self.media_type() == media_types::CLOSED_CAPTION
    }

    /// Check if this is a metadata format description
    #[must_use]
    pub fn is_metadata(&self) -> bool {
        self.media_type() == media_types::METADATA
    }

    /// Check if this is a timecode format description
    #[must_use]
    pub fn is_timecode(&self) -> bool {
        self.media_type() == media_types::TIMECODE
    }

    /// Get a human-readable string for the media type
    #[must_use]
    pub fn media_type_string(&self) -> String {
        self.media_type().display()
    }

    /// Get a human-readable string for the media subtype (codec)
    #[must_use]
    pub fn media_subtype_string(&self) -> String {
        self.media_subtype().display()
    }

    /// Check if the codec is H.264
    #[must_use]
    pub fn is_h264(&self) -> bool {
        self.media_subtype() == codec_types::H264
    }

    /// Check if the codec is HEVC/H.265
    #[must_use]
    pub fn is_hevc(&self) -> bool {
        let subtype = self.media_subtype();
        subtype == codec_types::HEVC || subtype == codec_types::HEVC_2
    }

    /// Check if the codec is AAC
    #[must_use]
    pub fn is_aac(&self) -> bool {
        self.media_subtype() == codec_types::AAC
    }

    /// Check if the codec is PCM
    #[must_use]
    pub fn is_pcm(&self) -> bool {
        self.media_subtype() == codec_types::LPCM
    }

    /// Check if the codec is `ProRes`
    #[must_use]
    pub fn is_prores(&self) -> bool {
        let subtype = self.media_subtype();
        subtype == codec_types::PRORES_422 || subtype == codec_types::PRORES_4444
    }

    /// Check if the codec is Apple Lossless (ALAC)
    #[must_use]
    pub fn is_alac(&self) -> bool {
        self.media_subtype() == codec_types::ALAC
    }

    // Audio format description methods

    /// Get the audio sample rate in Hz
    ///
    /// Returns `None` if this is not an audio format description.
    #[must_use]
    pub fn audio_sample_rate(&self) -> Option<f64> {
        if !self.is_audio() {
            return None;
        }
        let rate = unsafe { ffi::cm_format_description_get_audio_sample_rate(self.0) };
        if rate > 0.0 {
            Some(rate)
        } else {
            None
        }
    }

    /// Get the number of audio channels
    ///
    /// Returns `None` if this is not an audio format description.
    #[must_use]
    pub fn audio_channel_count(&self) -> Option<u32> {
        if !self.is_audio() {
            return None;
        }
        let count = unsafe { ffi::cm_format_description_get_audio_channel_count(self.0) };
        if count > 0 {
            Some(count)
        } else {
            None
        }
    }

    /// Get the bits per audio channel
    ///
    /// Returns `None` if this is not an audio format description.
    #[must_use]
    pub fn audio_bits_per_channel(&self) -> Option<u32> {
        if !self.is_audio() {
            return None;
        }
        let bits = unsafe { ffi::cm_format_description_get_audio_bits_per_channel(self.0) };
        if bits > 0 {
            Some(bits)
        } else {
            None
        }
    }

    /// Get the bytes per audio frame
    ///
    /// Returns `None` if this is not an audio format description.
    #[must_use]
    pub fn audio_bytes_per_frame(&self) -> Option<u32> {
        if !self.is_audio() {
            return None;
        }
        let bytes = unsafe { ffi::cm_format_description_get_audio_bytes_per_frame(self.0) };
        if bytes > 0 {
            Some(bytes)
        } else {
            None
        }
    }

    /// Get the audio format flags
    ///
    /// Returns `None` if this is not an audio format description.
    #[must_use]
    pub fn audio_format_flags(&self) -> Option<u32> {
        if !self.is_audio() {
            return None;
        }
        Some(unsafe { ffi::cm_format_description_get_audio_format_flags(self.0) })
    }

    /// Check if audio is float format (based on format flags)
    #[must_use]
    pub fn audio_is_float(&self) -> bool {
        // kAudioFormatFlagIsFloat = 1
        self.audio_format_flags().is_some_and(|f| f & 1 != 0)
    }

    /// Check if audio is big-endian (based on format flags)
    #[must_use]
    pub fn audio_is_big_endian(&self) -> bool {
        // kAudioFormatFlagIsBigEndian = 2
        self.audio_format_flags().is_some_and(|f| f & 2 != 0)
    }
}

impl Clone for CMFormatDescription {
    fn clone(&self) -> Self {
        unsafe {
            let ptr = ffi::cm_format_description_retain(self.0);
            Self(ptr)
        }
    }
}

impl Drop for CMFormatDescription {
    fn drop(&mut self) {
        unsafe {
            ffi::cm_format_description_release(self.0);
        }
    }
}

// SAFETY: `CMFormatDescriptionRef` is a Core Foundation type; Apple documents
// its retain/release as thread-safe and the description data is immutable after
// creation.
unsafe impl Send for CMFormatDescription {}
unsafe impl Sync for CMFormatDescription {}

impl fmt::Debug for CMFormatDescription {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CMFormatDescription")
            .field("media_type", &self.media_type_string())
            .field("codec", &self.media_subtype_string())
            .finish()
    }
}

impl fmt::Display for CMFormatDescription {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "CMFormatDescription(type: 0x{:08X}, subtype: 0x{:08X})",
            self.media_type_raw(),
            self.media_subtype_raw()
        )
    }
}

/// Metadata-specific wrapper around `CMFormatDescriptionRef`.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct CMMetadataFormatDescription(CMFormatDescription);

impl CMMetadataFormatDescription {
    /// Wraps a +1 retained raw metadata format-description pointer and returns `None` for null.
    #[must_use]
    pub fn from_raw(ptr: *mut std::ffi::c_void) -> Option<Self> {
        CMFormatDescription::from_raw(ptr).map(Self)
    }

    /// Wraps a raw metadata `CMFormatDescriptionRef` by taking ownership without retaining it.
    ///
    /// # Safety
    /// The caller must ensure `ptr` is a valid +1 retained metadata `CMFormatDescriptionRef`.
    pub const unsafe fn from_ptr(ptr: *mut std::ffi::c_void) -> Self {
        Self(CMFormatDescription::from_ptr(ptr))
    }

    /// Returns the wrapped raw metadata format-description pointer.
    #[must_use]
    pub const fn as_ptr(&self) -> *mut std::ffi::c_void {
        self.0.as_ptr()
    }

    /// Access the metadata description as a plain `CMFormatDescription`.
    #[must_use]
    pub const fn as_format_description(&self) -> &CMFormatDescription {
        &self.0
    }

    /// Consume the metadata wrapper and return the underlying `CMFormatDescription`.
    #[must_use]
    pub fn into_format_description(self) -> CMFormatDescription {
        self.0
    }

    /// Create a metadata format description from an optional array of key dictionaries.
    ///
    /// # Errors
    ///
    /// Returns the `OSStatus` reported by Core Media if the description could not be created.
    pub fn create_with_keys(
        metadata_type: crate::utils::four_char_code::FourCharCode,
        keys: Option<&CFArray>,
    ) -> Result<Self, i32> {
        let mut ptr = std::ptr::null_mut();
        let status = unsafe {
            ffi::cm_metadata_format_description_create_with_keys(
                metadata_type.into(),
                keys.map_or(std::ptr::null_mut(), CFArray::as_ptr),
                &mut ptr,
            )
        };
        if status == 0 && !ptr.is_null() {
            Self::from_raw(ptr).ok_or(status)
        } else {
            Err(status)
        }
    }

    /// Create a boxed metadata format description from metadata specification dictionaries.
    ///
    /// # Errors
    ///
    /// Returns the `OSStatus` reported by Core Media if the description could not be created.
    pub fn create_with_metadata_specifications(
        metadata_type: crate::utils::four_char_code::FourCharCode,
        metadata_specifications: &CFArray,
    ) -> Result<Self, i32> {
        let mut ptr = std::ptr::null_mut();
        let status = unsafe {
            ffi::cm_metadata_format_description_create_with_metadata_specifications(
                metadata_type.into(),
                metadata_specifications.as_ptr(),
                &mut ptr,
            )
        };
        if status == 0 && !ptr.is_null() {
            Self::from_raw(ptr).ok_or(status)
        } else {
            Err(status)
        }
    }

    /// Extend an existing metadata description with additional metadata specifications.
    ///
    /// # Errors
    ///
    /// Returns the `OSStatus` reported by Core Media if the extended description could not be created.
    pub fn extend_with_metadata_specifications(
        &self,
        metadata_specifications: &CFArray,
    ) -> Result<Self, i32> {
        let mut ptr = std::ptr::null_mut();
        let status = unsafe {
            ffi::cm_metadata_format_description_create_with_description_and_metadata_specifications(
                self.as_ptr(),
                metadata_specifications.as_ptr(),
                &mut ptr,
            )
        };
        if status == 0 && !ptr.is_null() {
            Self::from_raw(ptr).ok_or(status)
        } else {
            Err(status)
        }
    }

    /// Merge two metadata format descriptions into a new description.
    ///
    /// # Errors
    ///
    /// Returns the `OSStatus` reported by Core Media if the merged description could not be created.
    pub fn merge(&self, other: &Self) -> Result<Self, i32> {
        let mut ptr = std::ptr::null_mut();
        let status = unsafe {
            ffi::cm_metadata_format_description_create_by_merging_descriptions(
                self.as_ptr(),
                other.as_ptr(),
                &mut ptr,
            )
        };
        if status == 0 && !ptr.is_null() {
            Self::from_raw(ptr).ok_or(status)
        } else {
            Err(status)
        }
    }

    /// Copy the metadata identifiers declared by this description.
    #[must_use]
    pub fn identifiers(&self) -> Option<CFArray> {
        let ptr = unsafe { ffi::cm_metadata_format_description_get_identifiers(self.as_ptr()) };
        CFArray::from_raw(ptr)
    }

    /// Copy the metadata key dictionary for `local_id`, if present.
    #[must_use]
    pub fn key_with_local_id(&self, local_id: u32) -> Option<CFDictionary> {
        let ptr = unsafe {
            ffi::cm_metadata_format_description_get_key_with_local_id(self.as_ptr(), local_id)
        };
        CFDictionary::from_raw(ptr)
    }
}

impl Deref for CMMetadataFormatDescription {
    type Target = CMFormatDescription;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl TryFrom<CMFormatDescription> for CMMetadataFormatDescription {
    type Error = CMFormatDescription;

    fn try_from(value: CMFormatDescription) -> Result<Self, Self::Error> {
        if value.is_metadata() {
            Ok(Self(value))
        } else {
            Err(value)
        }
    }
}

impl From<CMMetadataFormatDescription> for CMFormatDescription {
    fn from(value: CMMetadataFormatDescription) -> Self {
        value.0
    }
}

impl fmt::Debug for CMMetadataFormatDescription {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CMMetadataFormatDescription")
            .field("media_type", &self.media_type_string())
            .field("codec", &self.media_subtype_string())
            .finish()
    }
}

impl fmt::Display for CMMetadataFormatDescription {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}
