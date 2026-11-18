// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Small typed registry reads and writes.

use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_ENUMERATE_SUB_KEYS, KEY_SET_VALUE, REG_DWORD,
    REG_EXPAND_SZ, REG_OPTION_NON_VOLATILE, RRF_RT_REG_DWORD, RRF_RT_REG_SZ, RegCloseKey,
    RegCreateKeyExW, RegEnumKeyExW, RegGetValueW, RegOpenKeyExW, RegSetValueExW,
};
use windows::core::PWSTR;
use windows::core::{HSTRING, PCWSTR};

/// A registry root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hive {
    /// `HKEY_LOCAL_MACHINE`.
    LocalMachine,
    /// `HKEY_CURRENT_USER`.
    CurrentUser,
}

impl Hive {
    const fn key(self) -> HKEY {
        match self {
            Self::LocalMachine => HKEY_LOCAL_MACHINE,
            Self::CurrentUser => HKEY_CURRENT_USER,
        }
    }
}

/// A `REG_SZ` value, `None` when absent or unreadable. At most 4 KiB.
#[must_use]
pub fn read_string(hive: Hive, key: &str, value: &str) -> Option<String> {
    let key = HSTRING::from(key);
    let value = HSTRING::from(value);
    let mut buffer = [0_u16; 2048];
    let mut size = u32::try_from(std::mem::size_of_val(&buffer)).ok()?;
    // SAFETY: both names are NUL-terminated HSTRINGs that outlive the call;
    // `buffer` is writable for `size` bytes and `size` is a live out-parameter.
    let status = unsafe {
        RegGetValueW(
            hive.key(),
            PCWSTR(key.as_ptr()),
            PCWSTR(value.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            Some(buffer.as_mut_ptr().cast()),
            Some(&raw mut size),
        )
    };
    if status != ERROR_SUCCESS {
        return None;
    }
    let units = usize::try_from(size).ok()? / 2;
    let text = &buffer[..units.min(buffer.len())];
    let end = text
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(text.len());
    String::from_utf16(&text[..end]).ok()
}

/// The names of `key`'s subkeys (at most 4096), empty when it cannot be
/// opened.
#[must_use]
pub fn subkeys(hive: Hive, key: &str) -> Vec<String> {
    let key = HSTRING::from(key);
    let mut handle = HKEY::default();
    // SAFETY: the key name is a NUL-terminated HSTRING; `handle` is a live
    // out-parameter closed below.
    let status = unsafe {
        RegOpenKeyExW(
            hive.key(),
            PCWSTR(key.as_ptr()),
            0,
            KEY_ENUMERATE_SUB_KEYS,
            &raw mut handle,
        )
    };
    if status != ERROR_SUCCESS {
        return Vec::new();
    }
    let mut names = Vec::new();
    for index in 0..4096_u32 {
        // A key name is at most 255 characters.
        let mut name = [0_u16; 256];
        let mut length = 256_u32;
        // SAFETY: `handle` is open for KEY_ENUMERATE_SUB_KEYS; `name` is
        // writable for `length` characters, both live for the call.
        let status = unsafe {
            RegEnumKeyExW(
                handle,
                index,
                PWSTR(name.as_mut_ptr()),
                &raw mut length,
                None,
                PWSTR::null(),
                None,
                None,
            )
        };
        if status != ERROR_SUCCESS {
            break;
        }
        let length = usize::try_from(length).unwrap_or(0).min(name.len());
        if let Ok(text) = String::from_utf16(&name[..length]) {
            names.push(text);
        }
    }
    // SAFETY: `handle` was opened above and is closed once.
    let _ = unsafe { RegCloseKey(handle) };
    names
}

/// A `REG_DWORD` value, `None` when absent or unreadable.
#[must_use]
pub fn read_dword(hive: Hive, key: &str, value: &str) -> Option<u32> {
    let key = HSTRING::from(key);
    let value = HSTRING::from(value);
    let mut data = 0_u32;
    let mut size = 4_u32;
    // SAFETY: both names are NUL-terminated HSTRINGs; `data` is a live
    // four-byte out-parameter whose size is passed.
    let status = unsafe {
        RegGetValueW(
            hive.key(),
            PCWSTR(key.as_ptr()),
            PCWSTR(value.as_ptr()),
            RRF_RT_REG_DWORD,
            None,
            Some(std::ptr::from_mut(&mut data).cast()),
            Some(&raw mut size),
        )
    };
    (status == ERROR_SUCCESS).then_some(data)
}

/// One value to write with [`write_values`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryValue {
    /// `REG_DWORD`.
    Dword(u32),
    /// `REG_EXPAND_SZ` (environment variables expanded by the reader).
    ExpandString(String),
}

/// Creates `key` under `hive` when needed and sets every value.
///
/// # Errors
///
/// The first Win32 error code that failed.
pub fn write_values(hive: Hive, key: &str, values: &[(&str, RegistryValue)]) -> Result<(), u32> {
    let key = HSTRING::from(key);
    let mut handle = HKEY::default();
    // SAFETY: the key name is a NUL-terminated HSTRING; `handle` is a live
    // out-parameter closed below.
    let status = unsafe {
        RegCreateKeyExW(
            hive.key(),
            PCWSTR(key.as_ptr()),
            0,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            None,
            &raw mut handle,
            None,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(status.0);
    }
    let mut result = Ok(());
    for (name, value) in values {
        let name = HSTRING::from(*name);
        let status = match value {
            RegistryValue::Dword(number) => {
                let bytes = number.to_le_bytes();
                // SAFETY: `handle` is open for KEY_SET_VALUE; the data slice
                // lives for the call.
                unsafe { RegSetValueExW(handle, PCWSTR(name.as_ptr()), 0, REG_DWORD, Some(&bytes)) }
            }
            RegistryValue::ExpandString(text) => {
                let bytes: Vec<u8> = text
                    .encode_utf16()
                    .chain(std::iter::once(0))
                    .flat_map(u16::to_le_bytes)
                    .collect();
                // SAFETY: as above; the data is NUL-terminated UTF-16.
                unsafe {
                    RegSetValueExW(
                        handle,
                        PCWSTR(name.as_ptr()),
                        0,
                        REG_EXPAND_SZ,
                        Some(&bytes),
                    )
                }
            }
        };
        if status != ERROR_SUCCESS {
            result = Err(status.0);
            break;
        }
    }
    // SAFETY: `handle` was opened above and is closed once.
    let _ = unsafe { RegCloseKey(handle) };
    result
}
