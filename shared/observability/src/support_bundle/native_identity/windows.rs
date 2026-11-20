// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Windows facts. Win32 cannot open relative to a folder handle, so every
//! folder is opened by path with `FILE_FLAG_OPEN_REPARSE_POINT` (a reparse
//! point is opened, seen and refused, never followed), and its real
//! location is read back from the handle (`GetFinalPathNameByHandleW`) for
//! the shared walk to prove it sits inside the folder before it. Identity
//! is the 128-bit `FileIdInfo`; volume flags and the remote-protocol query
//! say whether the volume can prove it.
//!
//! This module is the crate's one reviewed exception to `deny(unsafe_code)`:
//! four read-only kernel32 queries on a handle this module owns, each into
//! a stack buffer whose size is passed with it.
#![allow(
    unsafe_code,
    reason = "reviewed Win32 file-identity queries; see module docs"
)]

use std::ffi::{OsStr, OsString};
use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};

use windows_sys::Win32::Storage::FileSystem::{
    FILE_ID_INFO, FILE_NAME_NORMALIZED, FILE_REMOTE_PROTOCOL_INFO, FileIdInfo,
    FileRemoteProtocolInfo, GetFileInformationByHandleEx, GetFinalPathNameByHandleW,
    GetVolumeInformationByHandleW, VOLUME_NAME_NT,
};

use super::super::source::{
    ContainerFacts, FileIdentity, IdentityScheme, LinkFacts, Owner, SourceFacts, SourceKind,
    VolumeFacts,
};
use super::{Child, Inspection, OpenedLeaf};

const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
const FILE_ATTRIBUTE_DIRECTORY: u64 = 0x0010;
const FILE_ATTRIBUTE_REPARSE_POINT: u64 = 0x0400;
const FILE_SUPPORTS_OPEN_BY_FILE_ID: u32 = 0x0100_0000;
const ERROR_INVALID_FUNCTION: i32 = 1;
const ERROR_NOT_SUPPORTED: i32 = 50;
const ERROR_INVALID_PARAMETER: i32 = 87;
/// The longest path Win32 returns, in UTF-16 units.
const MAX_FINAL_PATH: usize = 32_768;

/// An open folder on the way to a source: the path it was opened by, the
/// handle, and its real location read from that handle.
pub(crate) struct Folder {
    path: PathBuf,
    location: PathBuf,
    _handle: File,
}

/// Opens `path` for its attributes only (no data access, no sharing
/// restriction), not following a reparse point; folders open too.
fn open_attributes(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .access_mode(0)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}

/// Opens the walk's root (`C:\`, `\\server\share\`, or the current folder
/// for a relative path). The root itself is trusted.
pub(crate) fn open_root(root: Option<&Path>) -> io::Result<Folder> {
    let path = root.map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    let handle = OpenOptions::new()
        .access_mode(0)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(&path)?;
    let location = final_path(&handle)?;
    Ok(Folder {
        path,
        location,
        _handle: handle,
    })
}

/// Opens `name` in `parent` as a folder without following a reparse
/// point. A reparse point is described, with no target to follow.
pub(crate) fn child(parent: &Folder, name: &OsStr) -> io::Result<Child> {
    let path = parent.path.join(name);
    let handle = open_attributes(&path)?;
    let information = winapi_util::file::information(&handle)?;
    let attributes = information.file_attributes();
    if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Ok(Child::Link(LinkFacts {
            target: None,
            owner: Owner::Other,
            container: ContainerFacts {
                owner: Owner::Other,
                writable_by_others: true,
            },
        }));
    }
    if attributes & FILE_ATTRIBUTE_DIRECTORY == 0 {
        return Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            "a folder on the way to a source is a file",
        ));
    }
    let location = final_path(&handle)?;
    Ok(Child::Directory(Folder {
        path,
        location,
        _handle: handle,
    }))
}

/// A folder's real location, from its handle.
#[allow(
    clippy::unnecessary_wraps,
    reason = "one signature with Unix, which walks by descriptor and has none"
)]
pub(crate) fn location(folder: &Folder) -> Option<&Path> {
    Some(&folder.location)
}

/// Inspects `name` in `parent` through a handle that reads no data and
/// follows no reparse point, held until the identities are compared.
pub(crate) fn inspect(parent: &Folder, name: &OsStr) -> io::Result<Inspection> {
    let handle = open_attributes(&parent.path.join(name))?;
    let facts = facts(&handle)?;
    Ok(Inspection {
        facts,
        _held: Some(handle),
    })
}

/// Opens `name` in `parent` for reading, opening a reparse point itself
/// rather than following it, and reads its facts, its volume's and its
/// real location from the handle. Never `None`: Windows opens a link.
pub(crate) fn open(parent: &Folder, name: &OsStr) -> io::Result<Option<OpenedLeaf>> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(parent.path.join(name))?;
    let facts = facts(&file)?;
    let mut volume = volume_facts(&file)?;
    let location = final_path(&file)?;
    // Every network redirector sits under the Multiple UNC Provider, even
    // one that does not answer `FileRemoteProtocolInfo`.
    volume.remote |= location.starts_with(r"\Device\Mup");
    Ok(Some(OpenedLeaf {
        file,
        facts,
        volume,
        location: Some(location),
    }))
}

/// Nothing to undo: Windows opens a source blocking.
#[allow(
    clippy::unnecessary_wraps,
    reason = "one signature with Unix, which clears O_NONBLOCK"
)]
pub(crate) fn finish(_file: &File) -> io::Result<()> {
    Ok(())
}

/// Type, attributes, names and identity from one handle.
fn facts(file: &File) -> io::Result<SourceFacts> {
    let information = winapi_util::file::information(file)?;
    let attributes = information.file_attributes();
    let reparse_point = attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0;
    let kind = if reparse_point {
        SourceKind::Link
    } else if attributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
        SourceKind::Directory
    } else if winapi_util::file::typ(file)?.is_disk() {
        SourceKind::Regular
    } else {
        SourceKind::Other
    };
    let (identity, scheme) = match file_id(file)? {
        Some(identity) => (identity, IdentityScheme::FileId128),
        None => (
            FileIdentity {
                volume: information.volume_serial_number(),
                file: u128::from(information.file_index()),
            },
            IdentityScheme::FileIndex64,
        ),
    };
    Ok(SourceFacts {
        kind,
        reparse_point,
        links: information.number_of_links(),
        identity,
        scheme,
    })
}

/// The 128-bit file ID and 64-bit volume serial number
/// (`GetFileInformationByHandleEx(FileIdInfo)`); `None` where the file
/// system does not support the query.
fn file_id(file: &File) -> io::Result<Option<FileIdentity>> {
    let mut information = FILE_ID_INFO::default();
    let size = u32::try_from(size_of::<FILE_ID_INFO>()).unwrap_or(u32::MAX);
    // SAFETY: the handle is open for the whole call, and `information` is
    // a writable FILE_ID_INFO of exactly `size` bytes.
    let succeeded = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileIdInfo,
            (&raw mut information).cast(),
            size,
        )
    };
    if succeeded == 0 {
        let error = io::Error::last_os_error();
        return match error.raw_os_error() {
            Some(ERROR_INVALID_PARAMETER | ERROR_NOT_SUPPORTED | ERROR_INVALID_FUNCTION) => {
                Ok(None)
            }
            _ => Err(error),
        };
    }
    Ok(Some(FileIdentity {
        volume: information.VolumeSerialNumber,
        file: u128::from_le_bytes(information.FileId.Identifier),
    }))
}

/// Whether the volume is remote, and whether it gives files stable IDs.
fn volume_facts(file: &File) -> io::Result<VolumeFacts> {
    let mut flags = 0_u32;
    // SAFETY: the handle is open for the whole call; the only buffer asked
    // for is `flags`, a writable u32; every other pointer is null with a
    // zero size, which the API allows.
    let succeeded = unsafe {
        GetVolumeInformationByHandleW(
            file.as_raw_handle(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &raw mut flags,
            std::ptr::null_mut(),
            0,
        )
    };
    if succeeded == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut remote = FILE_REMOTE_PROTOCOL_INFO::default();
    let size = u32::try_from(size_of::<FILE_REMOTE_PROTOCOL_INFO>()).unwrap_or(u32::MAX);
    // SAFETY: the handle is open for the whole call, and `remote` is a
    // writable FILE_REMOTE_PROTOCOL_INFO of exactly `size` bytes. The query
    // succeeds only for a file on a network redirector.
    let remote = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileRemoteProtocolInfo,
            (&raw mut remote).cast(),
            size,
        )
    } != 0;
    Ok(VolumeFacts {
        remote,
        stable_file_ids: flags & FILE_SUPPORTS_OPEN_BY_FILE_ID != 0,
    })
}

/// Where a handle really is: its normalized path on its NT volume device
/// (`\Device\HarddiskVolume3\…`), after any reparse point that led there.
fn final_path(file: &File) -> io::Result<PathBuf> {
    let mut buffer = vec![0_u16; 512];
    loop {
        let capacity = u32::try_from(buffer.len()).unwrap_or(u32::MAX);
        // SAFETY: the handle is open for the whole call, and `buffer` is
        // writable for `capacity` UTF-16 units.
        let written = unsafe {
            GetFinalPathNameByHandleW(
                file.as_raw_handle(),
                buffer.as_mut_ptr(),
                capacity,
                FILE_NAME_NORMALIZED | VOLUME_NAME_NT,
            )
        };
        let written = usize::try_from(written).unwrap_or(usize::MAX);
        if written == 0 {
            return Err(io::Error::last_os_error());
        }
        if written < buffer.len() {
            buffer.truncate(written);
            return Ok(PathBuf::from(OsString::from_wide(&buffer)));
        }
        if written > MAX_FINAL_PATH {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "a source's final path is longer than Win32 allows",
            ));
        }
        // Too small: `written` is the size needed, terminator included.
        // Always grow, so a misreported size cannot loop forever.
        let grown = written.max(buffer.len() + 1);
        buffer.resize(grown, 0);
    }
}
