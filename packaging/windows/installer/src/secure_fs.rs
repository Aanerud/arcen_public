// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! File-system guards for the directories and files that hold the Pier's
//! private key.
//!
//! A non-administrator can create `C:\ProgramData\Arcen` before the installer
//! runs, or make it a junction to a folder they own. Applying an ACL
//! afterwards, through the path, then protects their folder rather than ours,
//! and a key written before its ACL is applied is readable for a moment by
//! whatever the folder grants. So:
//!
//! - no component of a TLS path may be a reparse point, checked before and
//!   after a directory is created;
//! - an existing directory the installer restricts must already belong to
//!   SYSTEM or Administrators;
//! - a directory is created, and a file staged, with its owner and
//!   protected DACL in the `SECURITY_ATTRIBUTES` of the call that creates it;
//! - an ACL is applied through a handle to the object itself, opened
//!   without following a link and proven to be at its path, never by name
//!   in another process; a directory's handle is held without delete
//!   sharing, so neither it nor any folder above it can be renamed while
//!   the installer works in it;
//! - a staged file is opened without following a final link, and its final
//!   path is checked against the directory it was meant for before a byte
//!   is written.

// FFI to the Win32 security and file APIs this module exists to call.
#![allow(unsafe_code)]

use std::ffi::{OsStr, OsString, c_void};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::windows::ffi::{OsStrExt as _, OsStringExt as _};
use std::os::windows::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _};
use std::path::{Component, Path, PathBuf};
use std::ptr::null_mut;

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, ERROR_SUCCESS, HANDLE, INVALID_HANDLE_VALUE, LUID, LocalFree,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
    ConvertStringSidToSidW, GetSecurityInfo, SDDL_REVISION_1, SE_FILE_OBJECT, SetSecurityInfo,
};
use windows_sys::Win32::Security::{
    ACL, AdjustTokenPrivileges, DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl,
    LUID_AND_ATTRIBUTES, LookupPrivilegeValueW, OWNER_SECURITY_INFORMATION,
    PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SE_BACKUP_NAME,
    SE_PRIVILEGE_ENABLED, SE_RESTORE_NAME, SE_TAKE_OWNERSHIP_NAME, SECURITY_ATTRIBUTES,
    TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
};
use windows_sys::Win32::Storage::FileSystem::{
    CREATE_NEW, CreateDirectoryW, CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_WRITE,
    FILE_LIST_DIRECTORY, FILE_NAME_NORMALIZED, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE,
    FILE_SHARE_NONE, FILE_SHARE_READ, FILE_SHARE_WRITE, GetFinalPathNameByHandleW, READ_CONTROL,
    VOLUME_NAME_DOS, WRITE_DAC, WRITE_OWNER,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

const SID_SYSTEM: &str = "S-1-5-18";
const SID_ADMINISTRATORS: &str = "S-1-5-32-544";

/// The `D:` part of an SDDL string.
fn dacl_part(sddl: &str) -> &str {
    sddl.find("D:").map_or(sddl, |start| &sddl[start..])
}

fn wide(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(Some(0)).collect()
}

/// A `LocalAlloc`ed pointer, freed on drop.
#[derive(Debug)]
struct LocalMemory(*mut c_void);

impl Drop for LocalMemory {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the pointer came from an API that documents LocalFree.
            unsafe { LocalFree(self.0) };
        }
    }
}

/// A security descriptor built from SDDL, for `SECURITY_ATTRIBUTES`.
#[derive(Debug)]
pub(crate) struct Descriptor(LocalMemory);

impl Descriptor {
    /// Administrators as owner and the DACL of `sddl` (its `D:` part). An
    /// elevated administrator may name Administrators as the owner of what
    /// it creates, whatever the "default owner" policy says.
    pub(crate) fn owned_by_administrators(sddl: &str) -> Result<Self, String> {
        Self::parse(&format!("O:BA{}", dacl_part(sddl)))
    }

    fn parse(sddl: &str) -> Result<Self, String> {
        let text = wide(OsStr::new(sddl));
        let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();
        // SAFETY: `text` is NUL-terminated and outlives the call; the
        // descriptor is returned in LocalAlloc memory owned by `Self`.
        let converted = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                text.as_ptr(),
                SDDL_REVISION_1,
                &raw mut descriptor,
                null_mut(),
            )
        };
        if converted == 0 {
            return Err(format!(
                "security descriptor {sddl}: {}",
                io::Error::last_os_error()
            ));
        }
        Ok(Self(LocalMemory(descriptor)))
    }

    /// The descriptor's DACL, owned by `self`.
    fn dacl(&self) -> Result<*mut ACL, String> {
        let mut present = 0;
        let mut defaulted = 0;
        let mut dacl: *mut ACL = null_mut();
        // SAFETY: `self.0` is a valid descriptor; `dacl` points into it.
        let read = unsafe {
            GetSecurityDescriptorDacl(
                self.0.0,
                &raw mut present,
                &raw mut dacl,
                &raw mut defaulted,
            )
        };
        if read == 0 || present == 0 || dacl.is_null() {
            return Err(format!("descriptor DACL: {}", io::Error::last_os_error()));
        }
        Ok(dacl)
    }

    fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: u32::try_from(std::mem::size_of::<SECURITY_ATTRIBUTES>()).unwrap_or(0),
            lpSecurityDescriptor: self.0.0,
            bInheritHandle: 0,
        }
    }
}

/// Refuses `path` when any existing component of it is a reparse point
/// (a junction, a symbolic link or a mount point). Components past the
/// first missing one cannot exist.
pub(crate) fn refuse_reparse_chain(path: &Path) -> Result<(), String> {
    let absolute = std::path::absolute(path)
        .map_err(|error| format!("resolve {}: {error}", path.display()))?;
    let mut current = PathBuf::new();
    for component in absolute.components() {
        current.push(component);
        match component {
            Component::Prefix(_) | Component::RootDir | Component::CurDir => continue,
            Component::ParentDir => {
                return Err(format!(
                    "refusing a TLS path with a parent component: {}",
                    path.display()
                ));
            }
            Component::Normal(_) => {}
        }
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 => {
                return Err(format!(
                    "refusing reparse point {} in the TLS path {}",
                    current.display(),
                    path.display()
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(format!("inspect {}: {error}", current.display())),
        }
    }
    Ok(())
}

/// A handle to a file or directory itself, never a link's target, proven to
/// be at the path it was opened by. A directory's handle does not share
/// deletion, so while it is held neither the directory nor any folder
/// above it can be renamed or replaced.
#[derive(Debug)]
pub(crate) struct Pinned {
    object: File,
    path: PathBuf,
}

impl Pinned {
    pub(crate) fn directory(path: &Path) -> Result<Self, String> {
        Self::open(
            path,
            FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES | READ_CONTROL | WRITE_DAC | WRITE_OWNER,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
        )
    }

    pub(crate) fn file(path: &Path) -> Result<Self, String> {
        Self::open(
            path,
            FILE_READ_ATTRIBUTES | READ_CONTROL | WRITE_DAC | WRITE_OWNER,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
        )
    }

    fn open(path: &Path, access: u32, share: u32) -> Result<Self, String> {
        enable_repair_privileges();
        refuse_reparse_chain(path)?;
        let object = OpenOptions::new()
            .access_mode(access)
            .share_mode(share)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)
            .map_err(|error| format!("open {}: {error}", path.display()))?;
        let attributes = object
            .metadata()
            .map_err(|error| format!("inspect {}: {error}", path.display()))?
            .file_attributes();
        if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(format!("refusing reparse point {}", path.display()));
        }
        // Checked again now that the object is open: a directory's handle
        // keeps every folder above it in place from here on.
        refuse_reparse_chain(path)?;
        // The path as written, not as resolved: resolving would follow a
        // link swapped in for a moment. A short (8.3) name fails closed.
        let expected = lexical(path)?;
        let actual = final_path(&object)?;
        if !same_path(&actual, &expected) {
            return Err(format!(
                "{} opened as {}; refusing a redirected path",
                path.display(),
                actual.display()
            ));
        }
        Ok(Self {
            object,
            path: path.to_path_buf(),
        })
    }

    /// The owner SID, read from the handle.
    pub(crate) fn owner(&self) -> Result<String, String> {
        let mut owner: PSID = null_mut();
        let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();
        // SAFETY: the handle is open for READ_CONTROL; `owner` points into
        // `descriptor`, which is freed only after the SID is converted.
        let status = unsafe {
            GetSecurityInfo(
                self.object.as_raw_handle(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION,
                &raw mut owner,
                null_mut(),
                null_mut(),
                null_mut(),
                &raw mut descriptor,
            )
        };
        let _descriptor = LocalMemory(descriptor);
        if status != ERROR_SUCCESS {
            return Err(format!(
                "read the owner of {}: {}",
                self.path.display(),
                win32_error(status)
            ));
        }
        sid_string(owner)
            .map_err(|error| format!("read the owner of {}: {error}", self.path.display()))
    }

    /// Refuses an object that does not belong to SYSTEM or Administrators:
    /// whoever owns it can change its ACL whatever the installer sets, and
    /// may still hold a handle opened while it was theirs.
    pub(crate) fn require_trusted_owner(&self) -> Result<(), String> {
        let owner = self.owner()?;
        if owner == SID_SYSTEM || owner == SID_ADMINISTRATORS {
            Ok(())
        } else {
            Err(format!(
                "{} is owned by {owner}, not SYSTEM or Administrators, so its owner could read \
                 or replace the Pier's key; delete it and run again",
                self.path.display()
            ))
        }
    }

    /// Sets Administrators as owner and the protected DACL of `sddl` on the
    /// object this handle holds. Inheritable entries reach existing
    /// children, as `icacls /grant:r` with `(OI)(CI)` did.
    pub(crate) fn set_security(&self, sddl: &str) -> Result<(), String> {
        let descriptor = Descriptor::parse(dacl_part(sddl))?;
        let dacl = descriptor.dacl()?;
        let owner = Sid::parse(SID_ADMINISTRATORS)?;
        // SAFETY: the handle is open for WRITE_DAC and WRITE_OWNER; the SID
        // and DACL outlive the call.
        let status = unsafe {
            SetSecurityInfo(
                self.object.as_raw_handle(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION
                    | DACL_SECURITY_INFORMATION
                    | PROTECTED_DACL_SECURITY_INFORMATION,
                owner.0.0,
                null_mut(),
                dacl,
                std::ptr::null(),
            )
        };
        if status == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(format!(
                "set the ACL of {}: {}",
                self.path.display(),
                win32_error(status)
            ))
        }
    }
}

fn win32_error(status: u32) -> io::Error {
    io::Error::from_raw_os_error(i32::try_from(status).unwrap_or(i32::MAX))
}

/// A SID from its string form, in `LocalAlloc` memory.
#[derive(Debug)]
struct Sid(LocalMemory);

impl Sid {
    fn parse(text: &str) -> Result<Self, String> {
        let text = wide(OsStr::new(text));
        let mut sid: PSID = null_mut();
        // SAFETY: `text` is NUL-terminated; the SID is freed by `Self`.
        if unsafe { ConvertStringSidToSidW(text.as_ptr(), &raw mut sid) } == 0 {
            return Err(format!("SID: {}", io::Error::last_os_error()));
        }
        Ok(Self(LocalMemory(sid)))
    }
}

/// The string form of `sid`.
fn sid_string(sid: PSID) -> Result<String, io::Error> {
    // A string SID is at most a few hundred characters.
    const MAX_SID_UNITS: usize = 512;
    let mut text: *mut u16 = null_mut();
    // SAFETY: `sid` is a valid SID; `text` is LocalAlloc memory freed by
    // `_text`.
    if unsafe { ConvertSidToStringSidW(sid, &raw mut text) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let _text = LocalMemory(text.cast());
    let mut length = 0;
    // SAFETY: ConvertSidToStringSidW returns a NUL-terminated string; the
    // walk stops at its terminator.
    while length < MAX_SID_UNITS && unsafe { *text.add(length) } != 0 {
        length += 1;
    }
    // SAFETY: `length` units were just read from the same buffer.
    let units = unsafe { std::slice::from_raw_parts(text, length) };
    Ok(String::from_utf16_lossy(units))
}

/// Creates the directory `path` owned by Administrators with the DACL of
/// `sddl` from the start.
/// `Ok(false)` when it already exists.
pub(crate) fn create_protected_dir(path: &Path, sddl: &str) -> Result<bool, String> {
    let descriptor = Descriptor::owned_by_administrators(sddl)?;
    let attributes = descriptor.attributes();
    let name = wide(path.as_os_str());
    // SAFETY: `name` is NUL-terminated; `attributes` and the descriptor it
    // points to outlive the call.
    if unsafe { CreateDirectoryW(name.as_ptr(), &raw const attributes) } != 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == i32::try_from(ERROR_ALREADY_EXISTS).ok() {
        Ok(false)
    } else {
        Err(format!("create {}: {error}", path.display()))
    }
}

/// The final path of an open file, `\\?\C:\...`.
fn final_path(file: &File) -> Result<PathBuf, String> {
    let mut buffer = vec![0u16; 512];
    loop {
        let capacity = u32::try_from(buffer.len()).unwrap_or(u32::MAX);
        // SAFETY: `buffer` holds `capacity` units.
        let length = unsafe {
            GetFinalPathNameByHandleW(
                file.as_raw_handle(),
                buffer.as_mut_ptr(),
                capacity,
                FILE_NAME_NORMALIZED | VOLUME_NAME_DOS,
            )
        };
        let length = usize::try_from(length).unwrap_or(usize::MAX);
        if length == 0 {
            return Err(format!("final path: {}", io::Error::last_os_error()));
        }
        if length < buffer.len() {
            return Ok(PathBuf::from(OsString::from_wide(&buffer[..length])));
        }
        buffer.resize(length + 1, 0);
    }
}

/// `path` made absolute lexically (`GetFullPathNameW`), without resolving
/// any link.
pub(crate) fn lexical(path: &Path) -> Result<PathBuf, String> {
    std::path::absolute(path).map_err(|error| format!("resolve {}: {error}", path.display()))
}

/// A path without its `\\?\` (or `\\?\UNC\`) prefix, lower-cased and
/// without a trailing separator, for comparison.
fn comparable(path: &Path) -> String {
    let text = path.as_os_str().to_string_lossy();
    let text = if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = text.strip_prefix(r"\\?\") {
        rest.to_owned()
    } else {
        text.into_owned()
    };
    text.trim_end_matches('\\').to_lowercase()
}

fn same_path(left: &Path, right: &Path) -> bool {
    comparable(left) == comparable(right)
}

/// Enables the backup, restore and take-ownership privileges an elevated
/// administrator holds, once, so an object whose DACL no longer grants
/// Administrators anything (an interrupted earlier run, another owner) can
/// still be opened, its owner checked and its ACL repaired, as
/// `icacls /setowner` did. Without them, the open fails closed.
fn enable_repair_privileges() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let mut token: HANDLE = null_mut();
        // SAFETY: the pseudo-handle needs no closing; `token` is closed
        // below.
        if unsafe {
            OpenProcessToken(
                GetCurrentProcess(),
                TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
                &raw mut token,
            )
        } == 0
        {
            return;
        }
        for name in [SE_BACKUP_NAME, SE_RESTORE_NAME, SE_TAKE_OWNERSHIP_NAME] {
            let mut luid = LUID::default();
            // SAFETY: `name` is a static wide string; `luid` is written.
            if unsafe { LookupPrivilegeValueW(std::ptr::null(), name, &raw mut luid) } == 0 {
                continue;
            }
            let privileges = TOKEN_PRIVILEGES {
                PrivilegeCount: 1,
                Privileges: [LUID_AND_ATTRIBUTES {
                    Luid: luid,
                    Attributes: SE_PRIVILEGE_ENABLED,
                }],
            };
            // SAFETY: `privileges` is a complete one-entry structure.
            unsafe {
                AdjustTokenPrivileges(token, 0, &raw const privileges, 0, null_mut(), null_mut());
            }
        }
        // SAFETY: `token` was opened above.
        unsafe { CloseHandle(token) };
    });
}

/// Checks that `file`, opened as `directory\<name>`, really is in
/// `directory` (given as its canonical `\\?\` path): no junction swapped in
/// after the directory was checked has redirected it.
pub(crate) fn verify_in_directory(file: &File, canonical_directory: &Path) -> Result<(), String> {
    let actual = final_path(file)?;
    if actual
        .parent()
        .is_some_and(|parent| same_path(parent, canonical_directory))
    {
        Ok(())
    } else {
        Err(format!(
            "{} resolved outside {}; refusing a redirected TLS directory",
            actual.display(),
            canonical_directory.display()
        ))
    }
}

/// Writes `bytes` to the new file `path`, created with the DACL of `sddl`,
/// without following a final link and only after its final path is proven
/// to be in `canonical_directory`.
pub(crate) fn write_new_protected(
    path: &Path,
    bytes: &[u8],
    sddl: &str,
    canonical_directory: &Path,
) -> Result<(), String> {
    let descriptor = Descriptor::owned_by_administrators(sddl)?;
    let attributes = descriptor.attributes();
    let name = wide(path.as_os_str());
    // SAFETY: `name` is NUL-terminated; `attributes` and its descriptor
    // outlive the call. CREATE_NEW never opens an existing file or link.
    let handle = unsafe {
        CreateFileW(
            name.as_ptr(),
            FILE_GENERIC_WRITE,
            FILE_SHARE_NONE,
            &raw const attributes,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
            null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(format!(
            "create {}: {}",
            path.display(),
            io::Error::last_os_error()
        ));
    }
    // SAFETY: a valid handle this function owns from here on.
    let mut file = unsafe { File::from_raw_handle(handle) };
    if let Err(error) = verify_in_directory(&file, canonical_directory) {
        // Empty, and readable by SYSTEM and Administrators only; removed
        // from wherever it landed.
        if let Ok(actual) = final_path(&file) {
            drop(file);
            let _ = fs::remove_file(actual);
        }
        return Err(error);
    }
    file.write_all(bytes)
        .map_err(|error| format!("write {}: {error}", path.display()))?;
    file.sync_all()
        .map_err(|error| format!("sync {}: {error}", path.display()))
}
