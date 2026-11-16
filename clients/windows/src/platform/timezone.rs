// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The system time zone as an IANA identifier (`Europe/Oslo`), which the
//! Pier needs; Windows names zones its own way (`W. Europe Standard Time`).
//!
//! The mapping is the CLDR one that ships with Windows itself: the in-box
//! ICU (`icu.dll`, Windows 10 1903 and later) answers
//! `ucal_getTimeZoneIDForWindowsID` for the zone's key name and the user's
//! region, so `W. Europe Standard Time` in Norway is `Europe/Oslo`, not the
//! zone's CLDR default `Europe/Berlin`.

use windows::Win32::Globalization::GetUserDefaultGeoName;
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};
use windows::Win32::System::Time::{DYNAMIC_TIME_ZONE_INFORMATION, GetDynamicTimeZoneInformation};
use windows::core::{s, w};

type TimeZoneIdForWindowsId = unsafe extern "C" fn(
    winid: *const u16,
    len: i32,
    region: *const u8,
    id: *mut u16,
    id_capacity: i32,
    status: *mut i32,
) -> i32;

/// The system zone's IANA identifier; `None` when Windows cannot say.
#[must_use]
pub fn current_identifier() -> Option<String> {
    let mut info = DYNAMIC_TIME_ZONE_INFORMATION::default();
    // SAFETY: `info` is a live out-parameter of the documented type.
    let result = unsafe { GetDynamicTimeZoneInformation(&raw mut info) };
    if result == u32::MAX {
        return None;
    }
    let key = utf16_until_nul(&info.TimeZoneKeyName);
    if key.is_empty() {
        return None;
    }
    let mut geo = [0_u16; 16];
    // SAFETY: `geo` is a writable buffer whose length the slice carries.
    let geo_len = unsafe { GetUserDefaultGeoName(&mut geo) };
    let region = usize::try_from(geo_len)
        .ok()
        .and_then(|len| geo.get(..len.saturating_sub(1)))
        .map(String::from_utf16_lossy)
        .filter(|region| region.len() == 2 && region.bytes().all(|b| b.is_ascii_alphabetic()));
    let identifier = icu_identifier(key, region.as_deref())?;
    is_plausible_iana_identifier(&identifier).then_some(identifier)
}

fn icu_identifier(windows_key: &[u16], region: Option<&str>) -> Option<String> {
    // SAFETY: loads the system ICU from System32 only; the module stays
    // loaded for the process (it is part of the OS and already mapped by
    // most processes).
    let module =
        unsafe { LoadLibraryExW(w!("icu.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32) }.ok()?;
    // SAFETY: `module` is a loaded module and the name a static C string.
    let function = unsafe { GetProcAddress(module, s!("ucal_getTimeZoneIDForWindowsID")) }?;
    // SAFETY: ICU's C API declares exactly this signature (ucal.h).
    let function: TimeZoneIdForWindowsId = unsafe { std::mem::transmute(function) };
    let region = region.map(|region| {
        let mut bytes = region.to_ascii_uppercase().into_bytes();
        bytes.push(0);
        bytes
    });
    let mut output = [0_u16; 128];
    let mut status = 0_i32;
    // SAFETY: `windows_key` is valid for its length; `region` is NUL
    // terminated or null; `output` is writable for its capacity; `status`
    // is a live out-parameter.
    let written = unsafe {
        function(
            windows_key.as_ptr(),
            i32::try_from(windows_key.len()).ok()?,
            region
                .as_ref()
                .map_or(std::ptr::null(), std::vec::Vec::as_ptr),
            output.as_mut_ptr(),
            i32::try_from(output.len()).ok()?,
            &raw mut status,
        )
    };
    // U_ZERO_ERROR is 0; warnings are negative, failures positive.
    if status > 0 || written <= 0 {
        return None;
    }
    let written = usize::try_from(written).ok()?.min(output.len());
    String::from_utf16(&output[..written]).ok()
}

fn utf16_until_nul(units: &[u16]) -> &[u16] {
    let end = units
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(units.len());
    &units[..end]
}

/// Whether `identifier` looks like an IANA zone name (`Area/Location`,
/// `UTC`, `Etc/GMT+5`); the Pier validates it again.
#[must_use]
pub fn is_plausible_iana_identifier(identifier: &str) -> bool {
    !identifier.is_empty()
        && identifier.len() <= 64
        && identifier != "Etc/Unknown"
        && identifier
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'_' | b'-' | b'+'))
        && !identifier.starts_with('/')
        && !identifier.ends_with('/')
        && !identifier.contains("//")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;

    #[test]
    fn only_iana_shaped_identifiers_pass() {
        for good in [
            "Europe/Oslo",
            "UTC",
            "Etc/GMT+5",
            "America/Argentina/Buenos_Aires",
        ] {
            assert!(is_plausible_iana_identifier(good), "{good}");
        }
        for bad in [
            "",
            "Etc/Unknown",
            "W. Europe Standard Time",
            "/Oslo",
            "Europe//Oslo",
        ] {
            assert!(!is_plausible_iana_identifier(bad), "{bad}");
        }
    }

    #[test]
    fn this_machine_reports_an_iana_zone() {
        let zone = current_identifier().expect("Windows 11 ships ICU");
        assert!(is_plausible_iana_identifier(&zone), "{zone}");
    }
}
