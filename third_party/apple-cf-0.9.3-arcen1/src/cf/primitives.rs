//! Core Foundation primitive value wrappers.
//!
#![allow(clippy::missing_panics_doc)]

//! ```rust
//! use apple_cf::cf::{CFData, CFDate, CFNumber, CFString, CFUUID};
//!
//! let string = CFString::new("hello");
//! let number = CFNumber::from_i64(42);
//! let data = CFData::from_bytes([1, 2, 3, 4]);
//! let uuid = CFUUID::new();
//!
//! assert_eq!(string.to_string(), "hello");
//! assert_eq!(number.to_i64(), Some(42));
//! assert_eq!(data.to_vec(), vec![1, 2, 3, 4]);
//! assert_eq!(uuid.bytes().len(), 16);
//!
//! let now = CFDate::now();
//! assert!(now.to_system_time().is_some());
//! ```

use super::base::{impl_cf_type_wrapper, CFType};
use crate::ffi;
use std::ffi::CString;
use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const CF_ABSOLUTE_TIME_INTERVAL_SINCE_1970: f64 = 978_307_200.0;

fn to_cstring(value: &str) -> CString {
    CString::new(value).expect("Core Foundation strings may not contain interior NUL bytes")
}

impl_cf_type_wrapper!(CFString, cf_string_get_type_id);
impl_cf_type_wrapper!(CFNumber, cf_number_get_type_id);
impl_cf_type_wrapper!(CFData, cf_data_get_type_id);
impl_cf_type_wrapper!(CFDate, cf_date_get_type_id);
impl_cf_type_wrapper!(CFUUID, cf_uuid_get_type_id);
impl_cf_type_wrapper!(CFError, cf_error_get_type_id);

impl CFString {
    /// Create a UTF-8 `CFString`.
    #[must_use]
    pub fn new(value: &str) -> Self {
        let value = to_cstring(value);
        let ptr = unsafe { ffi::cf_string_create_with_cstring(value.as_ptr()) };
        Self::from_raw(ptr).expect("CFStringCreateWithCString returned NULL")
    }

    /// Number of Unicode scalar values in the string.
    #[must_use]
    pub fn len(&self) -> usize {
        unsafe { ffi::cf_string_get_length(self.as_ptr()) }
    }

    /// Whether the string is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Copy the string into a Rust `String`.
    #[must_use]
    pub fn to_string_lossy(&self) -> String {
        let ptr = unsafe { ffi::cf_string_copy_cstring(self.as_ptr()) };
        if ptr.is_null() {
            return String::new();
        }
        let string = unsafe { std::ffi::CStr::from_ptr(ptr) }
            .to_string_lossy()
            .into_owned();
        unsafe { ffi::acf_free_string(ptr) };
        string
    }
}

impl Default for CFString {
    fn default() -> Self {
        Self::new("")
    }
}

impl fmt::Display for CFString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_string_lossy())
    }
}

impl CFNumber {
    /// Create an integer number.
    #[must_use]
    pub fn from_i64(value: i64) -> Self {
        let ptr = unsafe { ffi::cf_number_create_i64(value) };
        Self::from_raw(ptr).expect("CFNumberCreate returned NULL")
    }

    /// Create an unsigned integer number.
    #[must_use]
    pub fn from_u64(value: u64) -> Self {
        let ptr = unsafe { ffi::cf_number_create_u64(value) };
        Self::from_raw(ptr).expect("CFNumberCreate returned NULL")
    }

    /// Create a floating-point number.
    #[must_use]
    pub fn from_f64(value: f64) -> Self {
        let ptr = unsafe { ffi::cf_number_create_f64(value) };
        Self::from_raw(ptr).expect("CFNumberCreate returned NULL")
    }

    /// Convert to `i64` if representable.
    #[must_use]
    pub fn to_i64(&self) -> Option<i64> {
        let mut out = 0_i64;
        let ok = unsafe { ffi::cf_number_get_i64(self.as_ptr(), &mut out) };
        ok.then_some(out)
    }

    /// Convert to `u64` if representable.
    #[must_use]
    pub fn to_u64(&self) -> Option<u64> {
        let mut out = 0_u64;
        let ok = unsafe { ffi::cf_number_get_u64(self.as_ptr(), &mut out) };
        ok.then_some(out)
    }

    /// Convert to `f64` if representable.
    #[must_use]
    pub fn to_f64(&self) -> Option<f64> {
        let mut out = 0.0_f64;
        let ok = unsafe { ffi::cf_number_get_f64(self.as_ptr(), &mut out) };
        ok.then_some(out)
    }

    /// Whether the number was created from a floating-point representation.
    #[must_use]
    pub fn is_float_type(&self) -> bool {
        unsafe { ffi::cf_number_is_float_type(self.as_ptr()) }
    }
}

impl From<i64> for CFNumber {
    fn from(value: i64) -> Self {
        Self::from_i64(value)
    }
}

impl From<u64> for CFNumber {
    fn from(value: u64) -> Self {
        Self::from_u64(value)
    }
}

impl From<f64> for CFNumber {
    fn from(value: f64) -> Self {
        Self::from_f64(value)
    }
}

impl CFData {
    /// Copy bytes into a new `CFData`.
    #[must_use]
    pub fn from_bytes<B: AsRef<[u8]>>(bytes: B) -> Self {
        let bytes = bytes.as_ref();
        let ptr = unsafe { ffi::cf_data_create(bytes.as_ptr(), bytes.len()) };
        Self::from_raw(ptr).expect("CFDataCreate returned NULL")
    }

    /// Number of bytes stored in the data blob.
    #[must_use]
    pub fn len(&self) -> usize {
        unsafe { ffi::cf_data_get_length(self.as_ptr()) }
    }

    /// Whether the data blob is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Copy the data into a Rust-owned vector.
    #[must_use]
    pub fn to_vec(&self) -> Vec<u8> {
        let mut bytes = vec![0_u8; self.len()];
        if !bytes.is_empty() {
            unsafe { ffi::cf_data_copy_bytes(self.as_ptr(), bytes.as_mut_ptr()) };
        }
        bytes
    }
}

impl CFDate {
    /// Create a date from Core Foundation absolute time (seconds since 2001-01-01 00:00:00 UTC).
    #[must_use]
    pub fn from_absolute_time(absolute_time: f64) -> Self {
        let ptr = unsafe { ffi::cf_date_create(absolute_time) };
        Self::from_raw(ptr).expect("CFDateCreate returned NULL")
    }

    /// Current wall-clock time.
    #[must_use]
    pub fn now() -> Self {
        Self::from_system_time(SystemTime::now())
    }

    /// Convert from a Rust `SystemTime`.
    #[must_use]
    pub fn from_system_time(time: SystemTime) -> Self {
        let unix_seconds = match time.duration_since(UNIX_EPOCH) {
            Ok(duration) => duration.as_secs_f64(),
            Err(err) => -err.duration().as_secs_f64(),
        };
        let absolute = unix_seconds - CF_ABSOLUTE_TIME_INTERVAL_SINCE_1970;
        Self::from_absolute_time(absolute)
    }

    /// Core Foundation absolute time.
    #[must_use]
    pub fn absolute_time(&self) -> f64 {
        unsafe { ffi::cf_date_get_absolute_time(self.as_ptr()) }
    }

    /// Convert to `SystemTime` when representable.
    #[must_use]
    pub fn to_system_time(&self) -> Option<SystemTime> {
        let unix_seconds = self.absolute_time() + CF_ABSOLUTE_TIME_INTERVAL_SINCE_1970;
        if unix_seconds.is_nan() || !unix_seconds.is_finite() {
            return None;
        }
        if unix_seconds >= 0.0 {
            Some(UNIX_EPOCH + Duration::from_secs_f64(unix_seconds))
        } else {
            Some(UNIX_EPOCH - Duration::from_secs_f64(-unix_seconds))
        }
    }
}

impl CFUUID {
    /// Generate a new random UUID.
    #[must_use]
    pub fn new() -> Self {
        let ptr = unsafe { ffi::cf_uuid_create() };
        Self::from_raw(ptr).expect("CFUUIDCreate returned NULL")
    }

    /// Parse a UUID string.
    #[must_use]
    pub fn parse_str(value: &str) -> Option<Self> {
        let value = to_cstring(value);
        let ptr = unsafe { ffi::cf_uuid_create_from_string(value.as_ptr()) };
        Self::from_raw(ptr)
    }

    /// Canonical textual representation.
    #[must_use]
    pub fn string(&self) -> CFString {
        let ptr = unsafe { ffi::cf_uuid_copy_string(self.as_ptr()) };
        CFString::from_raw(ptr).expect("CFUUIDCreateString returned NULL")
    }

    /// UUID bytes in RFC-4122 order.
    #[must_use]
    pub fn bytes(&self) -> [u8; 16] {
        let mut bytes = [0_u8; 16];
        unsafe { ffi::cf_uuid_get_bytes(self.as_ptr(), bytes.as_mut_ptr()) };
        bytes
    }
}

impl Default for CFUUID {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for CFUUID {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.string(), f)
    }
}

impl CFError {
    /// Create a Core Foundation error object.
    #[must_use]
    pub fn new(domain: &CFString, code: i64, description: Option<&str>) -> Self {
        let description = description.map(to_cstring);
        let description_ptr = description
            .as_ref()
            .map_or(std::ptr::null(), |s| s.as_ptr());
        let ptr = unsafe { ffi::cf_error_create(domain.as_ptr(), code, description_ptr) };
        Self::from_raw(ptr).expect("CFErrorCreate returned NULL")
    }

    /// Error domain.
    #[must_use]
    pub fn domain(&self) -> CFString {
        let ptr = unsafe { ffi::cf_error_get_domain(self.as_ptr()) };
        CFString::from_raw(ptr).expect("CFErrorGetDomain returned NULL")
    }

    /// Numeric error code.
    #[must_use]
    pub fn code(&self) -> i64 {
        unsafe { ffi::cf_error_get_code(self.as_ptr()) }
    }

    /// Localized description if present.
    #[must_use]
    pub fn description_string(&self) -> Option<CFString> {
        let ptr = unsafe { ffi::cf_error_copy_description(self.as_ptr()) };
        CFString::from_raw(ptr)
    }

    /// Failure reason if present.
    #[must_use]
    pub fn failure_reason(&self) -> Option<CFString> {
        let ptr = unsafe { ffi::cf_error_copy_failure_reason(self.as_ptr()) };
        CFString::from_raw(ptr)
    }
}

impl fmt::Display for CFError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(description) = self.description_string() {
            write!(f, "{} ({})", description, self.code())
        } else {
            write!(f, "{} ({})", self.domain(), self.code())
        }
    }
}

impl std::error::Error for CFError {}

impl From<CFString> for CFType {
    fn from(value: CFString) -> Self {
        value.into_cf_type()
    }
}

impl From<CFNumber> for CFType {
    fn from(value: CFNumber) -> Self {
        value.into_cf_type()
    }
}

impl From<CFData> for CFType {
    fn from(value: CFData) -> Self {
        value.into_cf_type()
    }
}

impl From<CFDate> for CFType {
    fn from(value: CFDate) -> Self {
        value.into_cf_type()
    }
}

impl From<CFUUID> for CFType {
    fn from(value: CFUUID) -> Self {
        value.into_cf_type()
    }
}
