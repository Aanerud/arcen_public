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
    CloseHandle, ERROR_ALREADY_EXISTS, ERROR_SHARING_VIOLATION, ERROR_SUCCESS, HANDLE,
    INVALID_HANDLE_VALUE, LUID, LocalFree,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSecurityDescriptorToStringSecurityDescriptorW, ConvertSidToStringSidW,
    ConvertStringSecurityDescriptorToSecurityDescriptorW, ConvertStringSidToSidW, GetSecurityInfo,
    SDDL_REVISION_1, SE_FILE_OBJECT, SetSecurityInfo,
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
    FILE_SHARE_NONE, FILE_SHARE_READ, FILE_SHARE_WRITE, GetFinalPathNameByHandleW, QueryDosDeviceW,
    READ_CONTROL, VOLUME_NAME_DOS, WRITE_DAC, WRITE_OWNER,
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
            .map_err(|error| {
                // Never advise deleting it: it may be the installer's own
                // folder, with the key and the configuration in it, or one
                // that is Windows' own or shared.
                if held_open(share, &error) {
                    format!(
                        "{} is open in another program that may rename or delete it; close it \
                         and run again",
                        path.display()
                    )
                } else {
                    format!("open {}: {error}", path.display())
                }
            })?;
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

    /// The owner and DACL, as SDDL, read from the handle.
    pub(crate) fn sddl(&self) -> Result<String, String> {
        let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();
        // SAFETY: the handle is open for READ_CONTROL; the descriptor is
        // freed by `_descriptor` after it is converted.
        let status = unsafe {
            GetSecurityInfo(
                self.object.as_raw_handle(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                null_mut(),
                null_mut(),
                null_mut(),
                null_mut(),
                &raw mut descriptor,
            )
        };
        let _descriptor = LocalMemory(descriptor);
        if status != ERROR_SUCCESS {
            return Err(format!(
                "read the ACL of {}: {}",
                self.path.display(),
                win32_error(status)
            ));
        }
        let mut text: *mut u16 = null_mut();
        let mut length = 0_u32;
        // SAFETY: `descriptor` is valid; the string is LocalAlloc memory
        // freed by `_text`, `length` units long with its terminator.
        let converted = unsafe {
            ConvertSecurityDescriptorToStringSecurityDescriptorW(
                descriptor,
                SDDL_REVISION_1,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                &raw mut text,
                &raw mut length,
            )
        };
        if converted == 0 || text.is_null() {
            return Err(format!(
                "read the ACL of {}: {}",
                self.path.display(),
                io::Error::last_os_error()
            ));
        }
        let _text = LocalMemory(text.cast());
        // SAFETY: the conversion wrote `length` units to `text`.
        let units =
            unsafe { std::slice::from_raw_parts(text, usize::try_from(length).unwrap_or(0)) };
        Ok(String::from_utf16_lossy(units)
            .trim_end_matches('\0')
            .to_owned())
    }

    /// Refuses the object unless it is still a plain folder (not a link or
    /// junction) at the path it was opened by.
    pub(crate) fn require_plain_folder(&self) -> Result<(), String> {
        let attributes = self
            .object
            .metadata()
            .map_err(|error| format!("inspect {}: {error}", self.path.display()))?
            .file_attributes();
        if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(format!(
                "{} is a reparse point (a link or junction); the installer does not follow \
                 one, since it could lead to a share",
                self.path.display()
            ));
        }
        let actual = final_path(&self.object)?;
        if crate::acl::is_network_path(&actual.display().to_string()) {
            return Err(format!("{} is on a network share", self.path.display()));
        }
        if !same_path(&actual, &self.path) {
            return Err(format!(
                "{} opened as {}; refusing a redirected or short-name (8.3) path: give the \
                 full long path",
                self.path.display(),
                actual.display()
            ));
        }
        Ok(())
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
                 or replace what the installer puts there. Leave it as it is (it may be a \
                 system or shared folder) and choose a location owned by SYSTEM or \
                 Administrators",
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

/// Whether an open that did not share deletion failed because another
/// handle may rename or delete the object.
fn held_open(share: u32, error: &io::Error) -> bool {
    share & FILE_SHARE_DELETE == 0
        && error.raw_os_error() == i32::try_from(ERROR_SHARING_VIOLATION).ok()
}

/// The device a drive letter (`C:`) names, read from the object manager
/// (`QueryDosDeviceW`) without opening anything on the drive.
fn drive_device(letter: &str) -> Option<String> {
    let name = wide(OsStr::new(letter));
    let mut target = vec![0_u16; 1024];
    let capacity = u32::try_from(target.len()).unwrap_or(0);
    // SAFETY: `name` is NUL-terminated and `target` holds `capacity` units.
    let written = unsafe { QueryDosDeviceW(name.as_ptr(), target.as_mut_ptr(), capacity) };
    let written = usize::try_from(written)
        .ok()
        .filter(|written| *written > 0)?;
    // The first of the NUL-separated targets is the current one.
    let first = target[..written].split(|unit| *unit == 0).next()?;
    Some(String::from_utf16_lossy(first))
}

/// The volume root (`C:\`) of `dir`, or a refusal: `dir` must name a
/// local disk volume by its drive letter. Not a UNC or device path, nor a
/// drive letter that names a share, a redirector or a `subst` mapping
/// (judged from the drive's device name, before anything on the drive is
/// opened). Nothing is opened here: the walk ([`walk_to`]) opens the
/// folders, from this root down, without following a link.
fn local_volume_root(option: &str, dir: &Path) -> Result<PathBuf, String> {
    let refused = || {
        format!(
            "refusing {option} {}: it is on a network share, or not on a local drive \
             letter. The Pier service runs as SYSTEM and the sign-in screen loads its DLL \
             from the install folder, so both folders must be on a local drive",
            dir.display()
        )
    };
    // As typed and as made absolute: an NT path (`\\??\\UNC\\...`) only
    // shows as typed.
    if crate::acl::is_network_path(&dir.to_string_lossy()) {
        return Err(refused());
    }
    let absolute = lexical(dir)?;
    let text = absolute.display().to_string();
    if crate::acl::is_network_path(&text) {
        return Err(refused());
    }
    let drive = text.strip_prefix(r"\\?\").unwrap_or(&text);
    let letter = drive
        .get(..2)
        .filter(|letter| letter.as_bytes()[0].is_ascii_alphabetic() && letter.as_bytes()[1] == b':')
        .ok_or_else(refused)?;
    // A local disk volume only: a mapped share (`\Device\LanmanRedirector`,
    // `\Device\Mup`), WebDAV, a remote desktop's drive, or a `subst`
    // mapping (`\??\...`, which may lead anywhere) is refused here.
    if !drive_device(letter).is_some_and(|device| device.starts_with(r"\Device\Harddisk")) {
        return Err(refused());
    }
    Ok(PathBuf::from(format!(r"{letter}\")))
}

/// Refuses `dir` unless it names a local disk volume by its drive letter
/// ([`local_volume_root`]). Opens nothing: the folders are opened and
/// judged by the walk that holds them ([`walk_to`]).
pub(crate) fn refuse_network_location(option: &str, dir: &Path) -> Result<(), String> {
    local_volume_root(option, dir).map(drop)
}

/// Why a folder of a walk could not be opened.
enum ChildOpen {
    /// It does not exist.
    Missing,
    /// Another handle may rename or delete it.
    Held,
    /// Anything else, as a message.
    Failed(String),
}

/// Opens, or with `create` creates, the folder `name` in the folder held
/// by `parent`, relative to that handle (`NtCreateFile` with
/// `RootDirectory`): no path is looked up again, so nothing above it can be
/// swapped for a link in between. A link or junction is opened itself,
/// never followed (`FILE_OPEN_REPARSE_POINT`). Deletion is not shared, so
/// while the handle is held nobody can rename it. `create` carries the
/// security descriptor a new folder gets in the creating call, or `None`
/// for its parent's inherited ACL.
fn open_child(
    parent: &File,
    name: &OsStr,
    access: u32,
    share: u32,
    create: Option<Option<&Descriptor>>,
) -> Result<File, ChildOpen> {
    use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
    use windows_sys::Wdk::Storage::FileSystem::{
        FILE_CREATE, FILE_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_FOR_BACKUP_INTENT,
        FILE_OPEN_REPARSE_POINT, FILE_SYNCHRONOUS_IO_NONALERT, NtCreateFile,
    };
    use windows_sys::Win32::Foundation::{
        OBJ_CASE_INSENSITIVE, RtlNtStatusToDosError, STATUS_OBJECT_NAME_COLLISION,
        STATUS_OBJECT_NAME_NOT_FOUND, STATUS_SHARING_VIOLATION, UNICODE_STRING,
    };
    use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
    use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

    let units: Vec<u16> = name.encode_wide().collect();
    if units.is_empty()
        || units
            .iter()
            .any(|unit| *unit == u16::from(b'\\') || *unit == u16::from(b'/'))
    {
        return Err(ChildOpen::Failed(format!(
            "{} is not a single folder name",
            name.to_string_lossy()
        )));
    }
    let bytes = u16::try_from(units.len() * 2)
        .map_err(|_| ChildOpen::Failed("folder name too long".to_owned()))?;
    let object_name = UNICODE_STRING {
        Length: bytes,
        MaximumLength: bytes,
        Buffer: units.as_ptr().cast_mut(),
    };
    let descriptor = create.flatten().map_or(std::ptr::null(), |descriptor| {
        descriptor.0.0.cast_const().cast()
    });
    let attributes = OBJECT_ATTRIBUTES {
        Length: u32::try_from(std::mem::size_of::<OBJECT_ATTRIBUTES>()).unwrap_or(0),
        RootDirectory: parent.as_raw_handle(),
        ObjectName: &raw const object_name,
        Attributes: OBJ_CASE_INSENSITIVE,
        SecurityDescriptor: descriptor,
        SecurityQualityOfService: std::ptr::null(),
    };
    let options = FILE_DIRECTORY_FILE
        | FILE_OPEN_REPARSE_POINT
        | FILE_OPEN_FOR_BACKUP_INTENT
        | FILE_SYNCHRONOUS_IO_NONALERT;
    let open = |disposition| {
        let mut handle: HANDLE = null_mut();
        // SAFETY: an all-zero IO_STATUS_BLOCK is its documented initial
        // state.
        let mut status_block: IO_STATUS_BLOCK = unsafe { std::mem::zeroed() };
        // SAFETY: `attributes`, the name and the descriptor it points to
        // outlive the call; `handle` and `status_block` are written.
        let status = unsafe {
            NtCreateFile(
                &raw mut handle,
                access | SYNCHRONIZE,
                &raw const attributes,
                &raw mut status_block,
                std::ptr::null(),
                0,
                share,
                disposition,
                options,
                std::ptr::null(),
                0,
            )
        };
        (status, handle)
    };
    let (mut status, mut handle) = open(if create.is_some() {
        FILE_CREATE
    } else {
        FILE_OPEN
    });
    if create.is_some() && status == STATUS_OBJECT_NAME_COLLISION {
        // Made by someone else in between: opened, and judged, like any other.
        (status, handle) = open(FILE_OPEN);
    }
    match status {
        // SAFETY: a handle NtCreateFile just opened, owned from here on.
        0.. => Ok(unsafe { File::from_raw_handle(handle) }),
        STATUS_OBJECT_NAME_NOT_FOUND => Err(ChildOpen::Missing),
        STATUS_SHARING_VIOLATION => Err(ChildOpen::Held),
        _ => {
            // SAFETY: a plain conversion of a status code.
            let error = unsafe { RtlNtStatusToDosError(status) };
            Err(ChildOpen::Failed(win32_error(error).to_string()))
        }
    }
}

/// Whose the folders above a walked directory are.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Above<'a> {
    /// The installer's: a missing one is made with `sddl` from the start
    /// (an inherited ACL is the system drive's "Authenticated Users:
    /// Modify"), and one a non-administrator could replace is refused,
    /// `instead` naming the way out.
    Installer { sddl: &'a str, instead: &'a str },
    /// An operator's own (`--tls-only`): a missing one inherits its
    /// parent's ACL, none is refused for its permissions, and the first a
    /// non-administrator could replace is named in a warning.
    Operator,
}

/// Walks from `dir`'s local volume root ([`local_volume_root`]) to its
/// parent, one folder at a time, each opened, or made when `create`,
/// relative to its parent's handle ([`open_child`]): no link is followed
/// and no path is looked up twice, so nothing can be swapped in between.
/// Every folder must be a plain directory whose final path is the one it
/// was named by; each is held without sharing deletion in the returned
/// [`ChainHold`] until the caller drops it. For [`Above::Installer`] each
/// is also judged ([`crate::acl::ancestor_hazard`]) and a replaceable one
/// refused: never to be fixed in place (a handle opened while it was may
/// keep the right to change its permissions) nor deleted (it may be
/// Windows' own or shared); the message names it and the right and asks
/// for a separate protected location. For [`Above::Operator`] the first
/// replaceable one is returned as a warning. Without `create`, the walk
/// stops at the first missing folder.
pub(crate) fn walk_to(
    option: &str,
    dir: &Path,
    above: Above<'_>,
    create: bool,
) -> Result<(ChainHold, Option<String>), String> {
    let root = local_volume_root(option, dir)?;
    let absolute = lexical(dir)?;
    let installer = matches!(above, Above::Installer { .. });
    let instead = match above {
        Above::Installer { instead, .. } => instead,
        Above::Operator => "",
    };
    let refused = |reason: String| {
        if installer {
            format!(
                "refusing to install into {}: {reason}. Install into a separate location whose \
                 folders only SYSTEM and Administrators can change, {instead}",
                dir.display()
            )
        } else {
            format!("refusing {option} {}: {reason}", dir.display())
        }
    };
    let held = |path: &Path| {
        format!(
            "{} is open in another program that may rename or delete it; close it and run \
             again",
            path.display()
        )
    };
    let leave_it = "leave it as it is: it may be a system or shared folder, and a program \
                    that opened it while it was replaceable can keep that right after its \
                    permissions are fixed";
    let descriptor = match above {
        Above::Installer { sddl, .. } => Some(Descriptor::owned_by_administrators(sddl)?),
        Above::Operator => None,
    };
    let below_root = absolute
        .strip_prefix(&root)
        .or_else(|_| absolute.strip_prefix(format!(r"\\?\{}", root.display())))
        .map_err(|_| {
            refused(format!(
                "{} is not below {}",
                absolute.display(),
                root.display()
            ))
        })?;
    let mut names = Vec::new();
    for component in below_root.components() {
        match component {
            Component::Normal(name) => names.push(name.to_os_string()),
            _ => {
                return Err(refused(format!(
                    "{} has a component that is not a name",
                    dir.display()
                )));
            }
        }
    }
    // The folders above `dir`; `dir` itself is the caller's ([`ChainHold::open_own`]).
    if names.pop().is_none() {
        return Err(refused(format!("{} is a volume root", dir.display())));
    }
    enable_repair_privileges();
    let access = FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES | READ_CONTROL;
    let share = FILE_SHARE_READ | FILE_SHARE_WRITE;
    let root_handle = OpenOptions::new()
        .access_mode(access)
        .share_mode(share)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(&root)
        .map_err(|error| {
            if held_open(share, &error) {
                refused(held(&root))
            } else {
                refused(format!("open {}: {error}", root.display()))
            }
        })?;
    let mut hold = ChainHold::default();
    let mut warning = None;
    let mut folder = Pinned {
        object: root_handle,
        path: root,
    };
    let mut rest = names.into_iter();
    loop {
        folder.require_plain_folder().map_err(refused)?;
        let shown = folder.path.display().to_string();
        match crate::acl::ancestor_hazard(&shown, &folder.sddl()?) {
            Ok(()) => {}
            Err(hazard @ crate::acl::Hazard::Replaceable { .. }) if installer => {
                return Err(refused(format!("{hazard}; {leave_it}")));
            }
            Err(hazard) if installer => return Err(refused(hazard.to_string())),
            Err(hazard) => {
                warning.get_or_insert_with(|| hazard.to_string());
            }
        }
        let Some(name) = rest.next() else {
            hold.push(folder, installer);
            return Ok((hold, warning));
        };
        let path = folder.path.join(&name);
        let child = match open_child(&folder.object, &name, access, share, None) {
            Ok(child) => child,
            Err(ChildOpen::Missing) if create => {
                match open_child(
                    &folder.object,
                    &name,
                    access,
                    share,
                    Some(descriptor.as_ref()),
                ) {
                    Ok(child) => child,
                    Err(ChildOpen::Held) => return Err(refused(held(&path))),
                    Err(ChildOpen::Missing) => {
                        return Err(refused(format!("{} vanished while made", path.display())));
                    }
                    Err(ChildOpen::Failed(error)) => {
                        return Err(refused(format!("create {}: {error}", path.display())));
                    }
                }
            }
            Err(ChildOpen::Missing) => {
                hold.push(folder, installer);
                return Ok((hold, warning));
            }
            Err(ChildOpen::Held) => return Err(refused(held(&path))),
            Err(ChildOpen::Failed(error)) => {
                return Err(refused(format!("open {}: {error}", path.display())));
            }
        };
        hold.push(folder, installer);
        folder = Pinned {
            object: child,
            path,
        };
    }
}

/// The first existing folder above `dir` that a non-administrator could
/// replace, as a message; `None` when there is none. Nothing is created or
/// changed, and no link is followed ([`walk_to`]).
pub(crate) fn ancestor_warning(dir: &Path) -> Option<String> {
    match walk_to("--programdata", dir, Above::Operator, false) {
        Ok((_, warning)) => warning,
        Err(error) => Some(error),
    }
}

/// The folders from a volume root to an install directory, and the
/// directory itself, held open without sharing deletion from when the walk
/// ([`walk_to`]) opened them until the installer has written and registered
/// what goes in them: nobody can rename one meanwhile. [`Self::recheck`]
/// judges them again through the same handles before each payload is
/// written.
#[derive(Debug, Default)]
pub(crate) struct ChainHold {
    /// From the root down; `true` for a folder above an installer's
    /// directory, judged again at every recheck, `false` for one above an
    /// operator's own, only kept a plain folder.
    folders: Vec<(Pinned, bool)>,
    /// The install directory and the class its ACL must have.
    own: Option<(Pinned, crate::acl::AclClass)>,
}

impl ChainHold {
    fn push(&mut self, folder: Pinned, judged: bool) {
        self.folders.push((folder, judged));
    }

    /// Opens, or makes with the DACL of `class` from the start, the
    /// directory `dir` in the last folder of the walk, relative to its
    /// handle, never following a link; refuses one that another account
    /// owns; applies the class's ACL through the handle; and holds it.
    pub(crate) fn open_own(
        &mut self,
        dir: &Path,
        class: crate::acl::AclClass,
    ) -> Result<(), String> {
        let (parent, _) = self
            .folders
            .last()
            .ok_or_else(|| format!("{} has no folder above it to open it from", dir.display()))?;
        let name = dir
            .file_name()
            .ok_or_else(|| format!("{} has no folder name", dir.display()))?;
        let path = parent.path.join(name);
        if !same_path(&path, &lexical(dir)?) {
            return Err(format!(
                "{} is not in {}, the folder the walk ended at",
                dir.display(),
                parent.path.display()
            ));
        }
        let held = |path: &Path| {
            format!(
                "{} is open in another program that may rename or delete it; close it and run \
                 again",
                path.display()
            )
        };
        let access =
            FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES | READ_CONTROL | WRITE_DAC | WRITE_OWNER;
        let share = FILE_SHARE_READ | FILE_SHARE_WRITE;
        let object = match open_child(&parent.object, name, access, share, None) {
            Ok(object) => object,
            Err(ChildOpen::Missing) => {
                let descriptor = Descriptor::owned_by_administrators(class.sddl())?;
                match open_child(&parent.object, name, access, share, Some(Some(&descriptor))) {
                    Ok(object) => object,
                    Err(ChildOpen::Held) => return Err(held(&path)),
                    Err(ChildOpen::Missing) => {
                        return Err(format!("{} vanished while made", path.display()));
                    }
                    Err(ChildOpen::Failed(error)) => {
                        return Err(format!("create {}: {error}", path.display()));
                    }
                }
            }
            Err(ChildOpen::Held) => return Err(held(&path)),
            Err(ChildOpen::Failed(error)) => {
                return Err(format!("open {}: {error}", path.display()));
            }
        };
        let directory = Pinned { object, path };
        directory.require_plain_folder()?;
        directory.require_trusted_owner()?;
        directory.set_security(class.sddl())?;
        self.own = Some((directory, class));
        Ok(())
    }

    /// Where each held folder really is, from the root down.
    #[cfg(test)]
    pub(crate) fn held_paths(&self) -> Vec<PathBuf> {
        self.folders
            .iter()
            .map(|(folder, _)| &folder.object)
            .chain(self.own.as_ref().map(|(directory, _)| &directory.object))
            .filter_map(|object| final_path(object).ok())
            .collect()
    }

    /// Judges every held folder again, as it is now: still a plain folder
    /// at its path, a folder above an installer's directory still one no
    /// non-administrator could replace, and the install directory still
    /// owned by SYSTEM or Administrators with exactly its class's ACL.
    pub(crate) fn recheck(&self) -> Result<(), String> {
        for (folder, judged) in &self.folders {
            folder.require_plain_folder()?;
            if *judged {
                crate::acl::ancestor_hazard(&folder.path.display().to_string(), &folder.sddl()?)
                    .map_err(|hazard| {
                        format!("{hazard}, since the installer checked it; nothing more is written")
                    })?;
            }
        }
        if let Some((directory, class)) = &self.own {
            directory.require_plain_folder()?;
            directory.require_trusted_owner()?;
            crate::acl::assert_acl_sddl(
                &directory.path.display().to_string(),
                &directory.sddl()?,
                *class,
            )?;
        }
        Ok(())
    }
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

/// Reads the text file `path`, opened without following a final link and
/// only once its final path is proven to be in `canonical_directory`.
pub(crate) fn read_in_directory(path: &Path, canonical_directory: &Path) -> Result<String, String> {
    use std::io::Read as _;
    let mut file = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .map_err(|error| format!("open {}: {error}", path.display()))?;
    let attributes = file
        .metadata()
        .map_err(|error| format!("inspect {}: {error}", path.display()))?
        .file_attributes();
    if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(format!("refusing reparse point {}", path.display()));
    }
    verify_in_directory(&file, canonical_directory)?;
    let mut text = String::new();
    file.read_to_string(&mut text)
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    Ok(text)
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
