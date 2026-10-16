//! Core Foundation property-list helpers.
//!
#![allow(clippy::missing_errors_doc)]
//!
//! ```rust
//! use apple_cf::cf::{
//!     CFDictionary, CFPropertyList, CFPropertyListFormat, CFPropertyListMutabilityOptions,
//!     CFString,
//! };
//!
//! let key = CFString::new("name");
//! let value = CFString::new("doom-fish");
//! let plist = CFDictionary::from_pairs(&[(&key, &value)]);
//!
//! let data = CFPropertyList::create_data(&plist, CFPropertyListFormat::BinaryV1_0, 0)
//!     .expect("serialize property list");
//! let (decoded, detected_format) =
//!     CFPropertyList::create_with_data(&data, CFPropertyListMutabilityOptions::IMMUTABLE)
//!         .expect("decode property list");
//!
//! assert_eq!(detected_format, CFPropertyListFormat::BinaryV1_0);
//! assert_eq!(decoded.type_id(), CFDictionary::type_id());
//! ```

use super::{
    AsCFType, CFData, CFError as CoreFoundationError, CFReadStream, CFType, CFWriteStream,
};
use crate::ffi;
use std::fmt;

/// `CFPropertyListFormat` values mirrored from Core Foundation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(isize)]
pub enum CFPropertyListFormat {
    OpenStep = 1,
    XmlV1_0 = 100,
    BinaryV1_0 = 200,
}

impl TryFrom<isize> for CFPropertyListFormat {
    type Error = isize;

    fn try_from(value: isize) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::OpenStep),
            100 => Ok(Self::XmlV1_0),
            200 => Ok(Self::BinaryV1_0),
            other => Err(other),
        }
    }
}

/// Mutability options for property-list decoding and deep copies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct CFPropertyListMutabilityOptions(u64);

impl CFPropertyListMutabilityOptions {
    /// Decode immutable containers and leaves.
    pub const IMMUTABLE: Self = Self(0);
    /// Decode mutable arrays and dictionaries, but immutable leaves.
    pub const MUTABLE_CONTAINERS: Self = Self(1 << 0);
    /// Decode mutable containers and mutable leaf strings/data values.
    pub const MUTABLE_CONTAINERS_AND_LEAVES: Self = Self(1 << 1);

    /// Create options from raw Core Foundation bits.
    #[must_use]
    pub const fn from_bits(bits: u64) -> Self {
        Self(bits)
    }

    /// Raw Core Foundation bitmask.
    #[must_use]
    pub const fn as_u64(self) -> u64 {
        self.0
    }

    /// Whether `other` is contained in this bitmask.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }
}

impl From<CFPropertyListMutabilityOptions> for u64 {
    fn from(options: CFPropertyListMutabilityOptions) -> Self {
        options.0
    }
}

/// Errors returned by property-list decode / serialize helpers.
#[derive(Debug)]
pub enum CFPropertyListError {
    /// Core Foundation produced a `CFErrorRef` describing the failure.
    CoreFoundation(CoreFoundationError),
    /// The API returned `NULL` without populating a Core Foundation error.
    Null(crate::CFError),
}

impl fmt::Display for CFPropertyListError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CoreFoundation(error) => fmt::Display::fmt(error, f),
            Self::Null(error) => fmt::Display::fmt(error, f),
        }
    }
}

impl std::error::Error for CFPropertyListError {}

fn property_list_error(
    operation: &'static str,
    error_ptr: *mut std::ffi::c_void,
) -> CFPropertyListError {
    CoreFoundationError::from_raw(error_ptr).map_or_else(
        || CFPropertyListError::Null(crate::CFError::new(operation)),
        CFPropertyListError::CoreFoundation,
    )
}

fn property_list_format(raw: isize) -> CFPropertyListFormat {
    CFPropertyListFormat::try_from(raw)
        .expect("Core Foundation returned an unknown CFPropertyListFormat")
}

/// Namespace for property-list parse / serialize helpers.
#[derive(Debug)]
pub struct CFPropertyList;

impl CFPropertyList {
    /// Deep-copy a property list, optionally changing its mutability semantics.
    pub fn create_deep_copy(
        property_list: &dyn AsCFType,
        options: CFPropertyListMutabilityOptions,
    ) -> Result<CFType, crate::CFError> {
        let ptr = unsafe {
            ffi::cf_property_list_create_deep_copy(property_list.as_ptr(), options.as_u64())
        };
        CFType::from_raw(ptr).ok_or(crate::CFError::new("CFPropertyListCreateDeepCopy"))
    }

    /// Decode a property list from an in-memory data blob.
    pub fn create_with_data(
        data: &CFData,
        options: CFPropertyListMutabilityOptions,
    ) -> Result<(CFType, CFPropertyListFormat), CFPropertyListError> {
        let mut format = 0_isize;
        let mut error = std::ptr::null_mut();
        let ptr = unsafe {
            ffi::cf_property_list_create_with_data(
                data.as_ptr(),
                options.as_u64(),
                &mut format,
                &mut error,
            )
        };
        CFType::from_raw(ptr)
            .map(|value| (value, property_list_format(format)))
            .ok_or_else(|| property_list_error("CFPropertyListCreateWithData", error))
    }

    /// Decode a property list from an already-open Core Foundation read stream.
    pub fn create_with_stream(
        stream: &CFReadStream,
        stream_length: usize,
        options: CFPropertyListMutabilityOptions,
    ) -> Result<(CFType, CFPropertyListFormat), CFPropertyListError> {
        let mut format = 0_isize;
        let mut error = std::ptr::null_mut();
        let stream_length = isize::try_from(stream_length).unwrap_or(isize::MAX);
        let ptr = unsafe {
            ffi::cf_property_list_create_with_stream(
                stream.as_ptr(),
                stream_length,
                options.as_u64(),
                &mut format,
                &mut error,
            )
        };
        CFType::from_raw(ptr)
            .map(|value| (value, property_list_format(format)))
            .ok_or_else(|| property_list_error("CFPropertyListCreateWithStream", error))
    }

    /// Serialize a property list into a `CFData` blob.
    pub fn create_data(
        property_list: &dyn AsCFType,
        format: CFPropertyListFormat,
        options: u64,
    ) -> Result<CFData, CFPropertyListError> {
        let mut error = std::ptr::null_mut();
        let ptr = unsafe {
            ffi::cf_property_list_create_data(
                property_list.as_ptr(),
                format as isize,
                options,
                &mut error,
            )
        };
        CFData::from_raw(ptr).ok_or_else(|| property_list_error("CFPropertyListCreateData", error))
    }

    /// Write a serialized property list to an already-open Core Foundation write stream.
    pub fn write(
        property_list: &dyn AsCFType,
        stream: &CFWriteStream,
        format: CFPropertyListFormat,
        options: u64,
    ) -> Result<usize, CFPropertyListError> {
        let mut error = std::ptr::null_mut();
        let written = unsafe {
            ffi::cf_property_list_write(
                property_list.as_ptr(),
                stream.as_ptr(),
                format as isize,
                options,
                &mut error,
            )
        };
        if written > 0 {
            usize::try_from(written).map_err(|_| {
                CFPropertyListError::Null(crate::CFError::new("CFPropertyListWrite overflow"))
            })
        } else {
            Err(property_list_error("CFPropertyListWrite", error))
        }
    }

    /// Validate whether a Core Foundation object can be serialized as a property list.
    #[must_use]
    pub fn is_valid(property_list: &dyn AsCFType, format: CFPropertyListFormat) -> bool {
        unsafe { ffi::cf_property_list_is_valid(property_list.as_ptr(), format as isize) }
    }
}
