//! Core Foundation resource, locale, formatter, and preferences wrappers.
//!
#![allow(clippy::missing_panics_doc)]

//! ```rust
//! use apple_cf::cf::{
//!     CFCalendar, CFCharacterSet, CFDate, CFDateFormatter, CFDateFormatterStyle, CFFileSecurity,
//!     CFLocale, CFNumber, CFNumberFormatter, CFNumberFormatterStyle, CFPreferences, CFString,
//!     CFTimeZone, CFURL, CFUUID, CFXML,
//! };
//!
//! let url = CFURL::from_file_system_path("/System/Library", true);
//! assert!(url.has_directory_path());
//!
//! let locale = CFLocale::current();
//! let tz = CFTimeZone::current();
//! let calendar = CFCalendar::current();
//! assert!(!locale.identifier().is_empty());
//! assert!(!tz.name().is_empty());
//! assert!(!calendar.identifier().is_empty());
//!
//! let charset = CFCharacterSet::from_characters_in_string(&CFString::new("abc"));
//! assert!(charset.contains('a'));
//!
//! let formatter = CFNumberFormatter::new(None, CFNumberFormatterStyle::Decimal);
//! let rendered = formatter.format_number(&CFNumber::from_i64(1234));
//! assert!(!rendered.is_empty());
//!
//! let date_formatter = CFDateFormatter::new(None, CFDateFormatterStyle::Short, CFDateFormatterStyle::NoStyle);
//! assert!(!date_formatter.format_date(&CFDate::now()).is_empty());
//!
//! let app_id = CFString::new("com.doomfish.apple-cf.tests");
//! CFPreferences::set_app_value(&CFString::new("example"), Some(&CFString::new("value")), &app_id);
//! let _ = CFPreferences::synchronize(&app_id);
//!
//! let file_security = CFFileSecurity::new();
//! let owner = CFUUID::new();
//! assert!(file_security.set_owner_uuid(&owner));
//!
//! let escaped = CFXML::escape_entities(&CFString::new("<tag>"));
//! assert!(escaped.to_string().contains("&lt;"));
//! ```

use super::base::{impl_cf_type_wrapper, AsCFType, CFType};
use super::{CFDate, CFNumber, CFString, CFUUID};
use crate::ffi;
use std::ffi::CString;

fn to_cstring(value: &str) -> CString {
    CString::new(value).expect("Core Foundation strings may not contain interior NUL bytes")
}

impl_cf_type_wrapper!(CFURL, cf_url_get_type_id);
impl_cf_type_wrapper!(CFBundle, cf_bundle_get_type_id);
impl_cf_type_wrapper!(CFLocale, cf_locale_get_type_id);
impl_cf_type_wrapper!(CFCalendar, cf_calendar_get_type_id);
impl_cf_type_wrapper!(CFTimeZone, cf_time_zone_get_type_id);
impl_cf_type_wrapper!(CFCharacterSet, cf_character_set_get_type_id);
impl_cf_type_wrapper!(CFNumberFormatter, cf_number_formatter_get_type_id);
impl_cf_type_wrapper!(CFDateFormatter, cf_date_formatter_get_type_id);
impl_cf_type_wrapper!(CFFileSecurity, cf_file_security_get_type_id);

/// `CFNumberFormatterStyle` values mirrored from Core Foundation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(i32)]
pub enum CFNumberFormatterStyle {
    NoStyle = 0,
    Decimal = 1,
    Currency = 2,
    Percent = 3,
    Scientific = 4,
    SpellOut = 5,
}

/// `CFDateFormatterStyle` values mirrored from Core Foundation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(i32)]
pub enum CFDateFormatterStyle {
    NoStyle = 0,
    Short = 1,
    Medium = 2,
    Long = 3,
    Full = 4,
}

impl CFURL {
    /// Create a URL from an absolute string.
    #[must_use]
    pub fn from_string(value: &str) -> Self {
        let value = to_cstring(value);
        let ptr = unsafe { ffi::cf_url_create_with_string(value.as_ptr()) };
        Self::from_raw(ptr).expect("CFURLCreateWithString returned NULL")
    }

    /// Create a file URL from a POSIX path.
    #[must_use]
    pub fn from_file_system_path(path: &str, is_directory: bool) -> Self {
        let path = to_cstring(path);
        let ptr = unsafe { ffi::cf_url_create_file_path(path.as_ptr(), is_directory) };
        Self::from_raw(ptr).expect("CFURLCreateWithFileSystemPath returned NULL")
    }

    /// Absolute string form of the URL.
    #[must_use]
    pub fn absolute_string(&self) -> CFString {
        let ptr = unsafe { ffi::cf_url_copy_absolute_string(self.as_ptr()) };
        CFString::from_raw(ptr).expect("CFURLCopyAbsoluteString returned NULL")
    }

    /// File-system path (POSIX style) for file URLs.
    #[must_use]
    pub fn file_system_path(&self) -> CFString {
        let ptr = unsafe { ffi::cf_url_copy_file_system_path(self.as_ptr()) };
        CFString::from_raw(ptr).expect("CFURLCopyFileSystemPath returned NULL")
    }

    /// Whether the URL ends with a directory path separator.
    #[must_use]
    pub fn has_directory_path(&self) -> bool {
        unsafe { ffi::cf_url_has_directory_path(self.as_ptr()) }
    }
}

impl CFBundle {
    /// Main bundle for the current process, if any.
    #[must_use]
    pub fn main() -> Option<Self> {
        let ptr = unsafe { ffi::cf_bundle_get_main() };
        Self::from_raw(ptr)
    }

    /// Create a bundle wrapper from a bundle URL.
    #[must_use]
    pub fn from_url(url: &CFURL) -> Option<Self> {
        let ptr = unsafe { ffi::cf_bundle_create(url.as_ptr()) };
        Self::from_raw(ptr)
    }

    /// Bundle identifier, if present.
    #[must_use]
    pub fn identifier(&self) -> Option<CFString> {
        let ptr = unsafe { ffi::cf_bundle_copy_identifier(self.as_ptr()) };
        CFString::from_raw(ptr)
    }

    /// Bundle URL.
    #[must_use]
    pub fn bundle_url(&self) -> CFURL {
        let ptr = unsafe { ffi::cf_bundle_copy_bundle_url(self.as_ptr()) };
        CFURL::from_raw(ptr).expect("CFBundleCopyBundleURL returned NULL")
    }

    /// Locate a resource by name and optional extension/subdirectory.
    #[must_use]
    pub fn resource_url(
        &self,
        name: &str,
        extension: Option<&str>,
        subdir: Option<&str>,
    ) -> Option<CFURL> {
        let name = to_cstring(name);
        let extension = extension.map(to_cstring);
        let subdir = subdir.map(to_cstring);
        let ptr = unsafe {
            ffi::cf_bundle_copy_resource_url(
                self.as_ptr(),
                name.as_ptr(),
                extension.as_ref().map_or(std::ptr::null(), |s| s.as_ptr()),
                subdir.as_ref().map_or(std::ptr::null(), |s| s.as_ptr()),
            )
        };
        CFURL::from_raw(ptr)
    }
}

impl CFLocale {
    /// Current user locale.
    #[must_use]
    pub fn current() -> Self {
        let ptr = unsafe { ffi::cf_locale_copy_current() };
        Self::from_raw(ptr).expect("CFLocaleCopyCurrent returned NULL")
    }

    /// Create a locale from an identifier such as `en_US`.
    #[must_use]
    pub fn new(identifier: &str) -> Self {
        let identifier = to_cstring(identifier);
        let ptr = unsafe { ffi::cf_locale_create(identifier.as_ptr()) };
        Self::from_raw(ptr).expect("CFLocaleCreate returned NULL")
    }

    /// Locale identifier.
    #[must_use]
    pub fn identifier(&self) -> CFString {
        let ptr = unsafe { ffi::cf_locale_copy_identifier(self.as_ptr()) };
        CFString::from_raw(ptr).expect("CFLocale identifier should be non-null")
    }
}

impl CFCalendar {
    /// Current user calendar.
    #[must_use]
    pub fn current() -> Self {
        let ptr = unsafe { ffi::cf_calendar_copy_current() };
        Self::from_raw(ptr).expect("CFCalendarCopyCurrent returned NULL")
    }

    /// Create a calendar by identifier (for example `gregorian`).
    #[must_use]
    pub fn new(identifier: &str) -> Self {
        let identifier = to_cstring(identifier);
        let ptr = unsafe { ffi::cf_calendar_create(identifier.as_ptr()) };
        Self::from_raw(ptr).expect("CFCalendarCreateWithIdentifier returned NULL")
    }

    /// Calendar identifier.
    #[must_use]
    pub fn identifier(&self) -> CFString {
        let ptr = unsafe { ffi::cf_calendar_copy_identifier(self.as_ptr()) };
        CFString::from_raw(ptr).expect("CFCalendar identifier should be non-null")
    }

    /// Time zone attached to the calendar.
    #[must_use]
    pub fn time_zone(&self) -> CFTimeZone {
        let ptr = unsafe { ffi::cf_calendar_copy_time_zone(self.as_ptr()) };
        CFTimeZone::from_raw(ptr).expect("CFCalendarCopyTimeZone returned NULL")
    }

    /// Update the calendar's time zone.
    pub fn set_time_zone(&self, time_zone: &CFTimeZone) {
        unsafe { ffi::cf_calendar_set_time_zone(self.as_ptr(), time_zone.as_ptr()) };
    }
}

impl CFTimeZone {
    /// Current system time zone.
    #[must_use]
    pub fn current() -> Self {
        let ptr = unsafe { ffi::cf_time_zone_copy_current() };
        Self::from_raw(ptr).expect("CFTimeZoneCopyCurrent returned NULL")
    }

    /// Create a time zone by name, for example `UTC`.
    #[must_use]
    pub fn new(name: &str) -> Self {
        let name = to_cstring(name);
        let ptr = unsafe { ffi::cf_time_zone_create(name.as_ptr()) };
        Self::from_raw(ptr).expect("CFTimeZoneCreateWithName returned NULL")
    }

    /// Time zone name.
    #[must_use]
    pub fn name(&self) -> CFString {
        let ptr = unsafe { ffi::cf_time_zone_copy_name(self.as_ptr()) };
        CFString::from_raw(ptr).expect("CFTimeZoneGetName returned NULL")
    }

    /// Offset from GMT in seconds for the supplied date.
    #[must_use]
    pub fn seconds_from_gmt(&self, date: &CFDate) -> i32 {
        unsafe { ffi::cf_time_zone_get_seconds_from_gmt(self.as_ptr(), date.as_ptr()) }
    }
}

impl CFCharacterSet {
    /// Create a character set from the characters contained in `string`.
    #[must_use]
    pub fn from_characters_in_string(string: &CFString) -> Self {
        let ptr =
            unsafe { ffi::cf_character_set_create_with_characters_in_string(string.as_ptr()) };
        Self::from_raw(ptr).expect("CFCharacterSetCreateWithCharactersInString returned NULL")
    }

    /// Invert the character set.
    #[must_use]
    pub fn inverted(&self) -> Self {
        let ptr = unsafe { ffi::cf_character_set_create_inverted_set(self.as_ptr()) };
        Self::from_raw(ptr).expect("CFCharacterSetCreateInvertedSet returned NULL")
    }

    /// Whether `character` is a member of the set.
    #[must_use]
    pub fn contains(&self, character: char) -> bool {
        unsafe { ffi::cf_character_set_is_character_member(self.as_ptr(), u32::from(character)) }
    }
}

impl CFNumberFormatter {
    /// Create a number formatter for the given locale and style.
    #[must_use]
    pub fn new(locale: Option<&CFLocale>, style: CFNumberFormatterStyle) -> Self {
        let ptr = unsafe {
            ffi::cf_number_formatter_create(
                locale.map_or(std::ptr::null_mut(), CFLocale::as_ptr),
                style as i32,
            )
        };
        Self::from_raw(ptr).expect("CFNumberFormatterCreate returned NULL")
    }

    /// Format a number into a string.
    #[must_use]
    pub fn format_number(&self, number: &CFNumber) -> CFString {
        let ptr = unsafe {
            ffi::cf_number_formatter_create_string_with_number(self.as_ptr(), number.as_ptr())
        };
        CFString::from_raw(ptr).expect("CFNumberFormatterCreateStringWithNumber returned NULL")
    }

    /// Parse a string into a Core Foundation number.
    #[must_use]
    pub fn parse_number(&self, string: &CFString) -> Option<CFNumber> {
        let ptr = unsafe {
            ffi::cf_number_formatter_create_number_from_string(self.as_ptr(), string.as_ptr())
        };
        CFNumber::from_raw(ptr)
    }
}

impl CFDateFormatter {
    /// Create a date formatter for the given locale and styles.
    #[must_use]
    pub fn new(
        locale: Option<&CFLocale>,
        date_style: CFDateFormatterStyle,
        time_style: CFDateFormatterStyle,
    ) -> Self {
        let ptr = unsafe {
            ffi::cf_date_formatter_create(
                locale.map_or(std::ptr::null_mut(), CFLocale::as_ptr),
                date_style as i32,
                time_style as i32,
            )
        };
        Self::from_raw(ptr).expect("CFDateFormatterCreate returned NULL")
    }

    /// Format a date into a localized string.
    #[must_use]
    pub fn format_date(&self, date: &CFDate) -> CFString {
        let ptr =
            unsafe { ffi::cf_date_formatter_create_string_with_date(self.as_ptr(), date.as_ptr()) };
        CFString::from_raw(ptr).expect("CFDateFormatterCreateStringWithDate returned NULL")
    }
}

impl CFFileSecurity {
    /// Create a mutable file-security object.
    #[must_use]
    pub fn new() -> Self {
        let ptr = unsafe { ffi::cf_file_security_create() };
        Self::from_raw(ptr).expect("CFFileSecurityCreate returned NULL")
    }

    /// Owner UUID, if present.
    #[must_use]
    pub fn owner_uuid(&self) -> Option<CFUUID> {
        let ptr = unsafe { ffi::cf_file_security_copy_owner_uuid(self.as_ptr()) };
        CFUUID::from_raw(ptr)
    }

    /// Set the owner UUID.
    #[must_use]
    pub fn set_owner_uuid(&self, uuid: &CFUUID) -> bool {
        unsafe { ffi::cf_file_security_set_owner_uuid(self.as_ptr(), uuid.as_ptr()) }
    }

    /// File mode, if present.
    #[must_use]
    pub fn mode(&self) -> Option<u32> {
        let mut mode = 0_u32;
        let ok = unsafe { ffi::cf_file_security_get_mode(self.as_ptr(), &mut mode) };
        ok.then_some(mode)
    }

    /// Set the file mode bits.
    #[must_use]
    pub fn set_mode(&self, mode: u32) -> bool {
        unsafe { ffi::cf_file_security_set_mode(self.as_ptr(), mode) }
    }
}

impl Default for CFFileSecurity {
    fn default() -> Self {
        Self::new()
    }
}

/// Core Foundation preferences helpers.
#[derive(Debug)]
pub struct CFPreferences;

impl CFPreferences {
    /// Set or clear an application-scoped preference value.
    pub fn set_app_value(key: &CFString, value: Option<&dyn AsCFType>, app_id: &CFString) {
        unsafe {
            ffi::cf_preferences_set_app_value(
                key.as_ptr(),
                value.map_or(std::ptr::null_mut(), AsCFType::as_ptr),
                app_id.as_ptr(),
            );
        }
    }

    /// Copy an application-scoped preference value.
    #[must_use]
    pub fn app_value(key: &CFString, app_id: &CFString) -> Option<CFType> {
        let ptr = unsafe { ffi::cf_preferences_copy_app_value(key.as_ptr(), app_id.as_ptr()) };
        CFType::from_raw(ptr)
    }

    /// Flush pending preference changes.
    #[must_use]
    pub fn synchronize(app_id: &CFString) -> bool {
        unsafe { ffi::cf_preferences_app_synchronize(app_id.as_ptr()) }
    }
}

/// Tiny wrapper around the remaining useful `CFXML` helpers.
#[derive(Debug)]
pub struct CFXML;

impl CFXML {
    /// Escape XML entities in `value`.
    #[must_use]
    pub fn escape_entities(value: &CFString) -> CFString {
        let ptr = unsafe { ffi::cf_xml_create_string_by_escaping_entities(value.as_ptr()) };
        CFString::from_raw(ptr).expect("CFXMLCreateStringByEscapingEntities returned NULL")
    }

    /// Unescape XML entities in `value`.
    #[must_use]
    pub fn unescape_entities(value: &CFString) -> CFString {
        let ptr = unsafe { ffi::cf_xml_create_string_by_unescaping_entities(value.as_ptr()) };
        CFString::from_raw(ptr).expect("CFXMLCreateStringByUnescapingEntities returned NULL")
    }
}
