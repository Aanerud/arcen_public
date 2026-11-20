// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Opening a source file safely, the same way on every platform.
//!
//! A source path is walked one folder at a time from its root, through
//! handles: every folder is opened relative to the one before it without
//! following a link, so no folder can be swapped for a link between the
//! check and the use. A link on the way is followed only when nobody else
//! could have put it there ([`link_is_trusted`]); a Windows reparse point
//! never is. Where the OS cannot open relative to a handle (Windows), each
//! folder's real location, read back from its handle, must be inside the
//! folder before it.
//!
//! The file itself is inspected without opening it for reading, then opened
//! without following a link and without blocking (a FIFO answers at once),
//! and must be a regular file with one name ([`check_source`]) and the file
//! that was inspected ([`verify_same_source`]). Only OS facts come from
//! `native_identity`; every decision is here.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::fs::{File, Metadata};
use std::path::{Component, Path, PathBuf};

use arcen_telemetry::{NoticeCode, NoticeKind};

use super::SourceOpenError;
use super::native_identity::{self as native, Child, Folder};

/// The most links one source path may pass through (Linux's `MAXSYMLINKS`).
const MAX_LINKS: u32 = 40;

/// Which file a source is, as its operating system numbers it: the volume
/// it is on and its file number there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileIdentity {
    /// Unix: the device. Windows: the volume serial number (64-bit from
    /// `FileIdInfo`, 32-bit in the index fallback).
    pub volume: u64,
    /// Unix: the inode. Windows: the 128-bit `FILE_ID_128`, or the 64-bit
    /// file index where the volume has no `FileIdInfo`.
    pub file: u128,
}

/// Where a [`FileIdentity`] came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityScheme {
    /// Unix device and inode.
    DeviceInode,
    /// Windows `GetFileInformationByHandleEx(FileIdInfo)`: unique on `ReFS`
    /// as well as NTFS.
    FileId128,
    /// Windows `GetFileInformationByHandle`'s 64-bit file index, used only
    /// where `FileIdInfo` is unsupported; not guaranteed unique on `ReFS`.
    FileIndex64,
}

/// What a path's last component is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    /// A regular file: the only kind a source may be.
    Regular,
    /// A folder.
    Directory,
    /// A symbolic link or (Windows) a reparse point.
    Link,
    /// A FIFO, socket, device or anything else.
    Other,
}

/// What the OS says about one source file, read from a handle (or, for the
/// inspection on Unix, `fstatat` without following a link).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceFacts {
    /// What it is.
    pub kind: SourceKind,
    /// Windows: it carries `FILE_ATTRIBUTE_REPARSE_POINT`.
    pub reparse_point: bool,
    /// How many names it has (`st_nlink`, `NumberOfLinks`).
    pub links: u64,
    /// Which file it is.
    pub identity: FileIdentity,
    /// Where the identity came from.
    pub scheme: IdentityScheme,
}

/// What the volume an opened source is on says about its file identities.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VolumeFacts {
    /// A network volume (Windows `FileRemoteProtocolInfo` answers).
    pub remote: bool,
    /// The volume gives files stable IDs it can open them by (Windows
    /// `FILE_SUPPORTS_OPEN_BY_FILE_ID`; always on Unix, where the inode is
    /// the identity).
    pub stable_file_ids: bool,
}

impl VolumeFacts {
    /// A local volume with stable file numbers.
    pub const LOCAL: Self = Self {
        remote: false,
        stable_file_ids: true,
    };
}

/// Who owns a link or a folder, relative to the collecting process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Owner {
    /// The system administrator (Unix uid 0).
    System,
    /// The account the collector runs as (Unix effective uid).
    Process,
    /// Anyone else, or unknown.
    Other,
}

/// The folder a link sits in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContainerFacts {
    /// Its owner.
    pub owner: Owner,
    /// Someone other than its owner may create, rename or delete entries in
    /// it (Unix group- or other-write permission).
    pub writable_by_others: bool,
}

/// A link met on the way to a source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkFacts {
    /// Where it points; `None` where the OS gives no target to follow
    /// (a Windows reparse point).
    pub target: Option<PathBuf>,
    /// Who owns the link.
    pub owner: Owner,
    /// The folder it sits in.
    pub container: ContainerFacts,
}

/// Whether a link on the way to a source may be followed: only one that
/// nobody but the system or the collector's own account could have made or
/// replaced: owned by one of them, in a folder owned by one of them that
/// nobody else can write to. That keeps a system's own links (macOS
/// `/var` → `private/var`, Linux `/var/run` → `/run`) and refuses one
/// planted in a shared folder. A link with no target (a Windows reparse
/// point) is never followed.
#[must_use]
pub fn link_is_trusted(link: &LinkFacts) -> bool {
    link.target.is_some()
        && link.owner != Owner::Other
        && link.container.owner != Owner::Other
        && !link.container.writable_by_others
}

/// Whether one file may be read as a source: a regular file (not a link,
/// reparse point, FIFO, socket or device) with at most one name. A second
/// name means another file's contents could sit under an allowed name;
/// zero names is an opened file deleted since, which is still the file
/// that was inspected.
///
/// # Errors
///
/// [`SourceOpenError::UnsafeType`] or [`SourceOpenError::HardLinked`].
pub fn check_source(facts: &SourceFacts) -> Result<(), SourceOpenError> {
    if facts.kind != SourceKind::Regular || facts.reparse_point {
        return Err(SourceOpenError::UnsafeType);
    }
    if facts.links > 1 {
        return Err(SourceOpenError::HardLinked);
    }
    Ok(())
}

/// Whether the file that was opened is provably the one that was
/// inspected. First the volume must be able to prove identity at all: a
/// network volume, or one without stable file IDs, cannot
/// ([`SourceOpenError::IdentityUnstable`]), and neither can two identities
/// read different ways. Then a different identity means the path was
/// pointed at another file in between ([`SourceOpenError::Replaced`]), and
/// equal identities prove it only when they name a file: file number zero
/// is what a file system without file numbers reports, and two zeros prove
/// nothing ([`SourceOpenError::IdentityUnproven`]). Every platform's source
/// is held to this one rule.
///
/// # Errors
///
/// The volume cannot prove identity, the identities differ, or they do not
/// name a file.
pub fn verify_same_source(
    inspected: &SourceFacts,
    opened: &SourceFacts,
    volume: VolumeFacts,
) -> Result<(), SourceOpenError> {
    if volume.remote || !volume.stable_file_ids || inspected.scheme != opened.scheme {
        return Err(SourceOpenError::IdentityUnstable);
    }
    if inspected.identity != opened.identity {
        return Err(SourceOpenError::Replaced);
    }
    if opened.identity.file == 0 {
        return Err(SourceOpenError::IdentityUnproven);
    }
    Ok(())
}

/// A source opened for reading by [`open_source`].
#[derive(Debug)]
pub struct OpenedSource {
    /// The file, blocking, positioned at its start.
    pub file: File,
    /// Its metadata as opened.
    pub metadata: Metadata,
    /// How its identity was proven.
    pub scheme: IdentityScheme,
}

impl OpenedSource {
    /// The advisory the manifest records for this source, if any: a
    /// Windows volume without `FileIdInfo` proved it by a 64-bit file index
    /// only ([`NoticeCode::SourceIdentityIndexOnly`]).
    #[must_use]
    pub fn advisory(&self) -> Option<(NoticeKind, NoticeCode)> {
        (self.scheme == IdentityScheme::FileIndex64)
            .then_some((NoticeKind::Advisory, NoticeCode::SourceIdentityIndexOnly))
    }
}

/// Opens a source for reading: walks its folders without following an
/// untrusted link or any reparse point, inspects the file, opens it without
/// following a link or blocking, and checks it is a regular file with one
/// name that is the inspected file on a volume that can prove it.
///
/// # Errors
///
/// The source is missing or unreadable ([`SourceOpenError::Io`]), or is
/// refused for one of [`SourceOpenError`]'s reasons.
pub fn open_source(path: &Path) -> Result<OpenedSource, SourceOpenError> {
    open_source_with(path, &mut |_| {})
}

/// A point in [`open_source_with`] where a test can change the file system.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Stage {
    /// The folders have been walked; the file is not yet inspected.
    Walked,
    /// The file has been inspected; it is not yet opened.
    Inspected,
}

/// [`open_source`], calling `hook` at each [`Stage`]: the windows a
/// replacement race uses, which tests fill.
pub(super) fn open_source_with(
    path: &Path,
    hook: &mut dyn FnMut(Stage),
) -> Result<OpenedSource, SourceOpenError> {
    let (folder, leaf) = walk(path)?;
    hook(Stage::Walked);
    let inspection = native::inspect(&folder, &leaf).map_err(SourceOpenError::Io)?;
    check_source(&inspection.facts)?;
    hook(Stage::Inspected);
    let Some(opened) = native::open(&folder, &leaf).map_err(SourceOpenError::Io)? else {
        return Err(SourceOpenError::UnsafeType);
    };
    check_source(&opened.facts)?;
    verify_same_source(&inspection.facts, &opened.facts, opened.volume)?;
    prove_inside(native::location(&folder), opened.location.as_deref())?;
    drop(inspection);
    native::finish(&opened.file).map_err(SourceOpenError::Io)?;
    let metadata = opened.file.metadata().map_err(SourceOpenError::Io)?;
    Ok(OpenedSource {
        file: opened.file,
        metadata,
        scheme: opened.facts.scheme,
    })
}

/// That `child`'s real location is directly inside `parent`'s, where the
/// OS reports locations (Windows final paths). A child that a swapped
/// folder or reparse point led somewhere else is
/// [`SourceOpenError::PathLinked`].
fn prove_inside(parent: Option<&Path>, child: Option<&Path>) -> Result<(), SourceOpenError> {
    match (parent, child) {
        (None, None) => Ok(()),
        (Some(parent), Some(child)) if child.parent() == Some(parent) => Ok(()),
        _ => Err(SourceOpenError::PathLinked),
    }
}

/// One step of a path below its root.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Step {
    Current,
    Parent,
    Name(OsString),
}

/// A path's root (`/`, `C:\`, `\\server\share\`; `None` for a relative
/// path) and its steps below it.
fn split(path: &Path) -> Result<(Option<PathBuf>, Vec<Step>), SourceOpenError> {
    let mut root: Option<PathBuf> = None;
    let mut prefixed = false;
    let mut steps = Vec::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => {
                prefixed = true;
                root = Some(PathBuf::from(prefix.as_os_str()));
            }
            Component::RootDir => {
                let mut with_root = root.take().unwrap_or_default().into_os_string();
                with_root.push(std::path::MAIN_SEPARATOR_STR);
                root = Some(PathBuf::from(with_root));
                prefixed = false;
            }
            Component::CurDir => steps.push(Step::Current),
            Component::ParentDir => steps.push(Step::Parent),
            Component::Normal(name) => steps.push(Step::Name(name.to_owned())),
        }
    }
    if prefixed {
        // A drive-relative path (`C:logs`) depends on a per-drive current
        // folder; no source is named that way.
        return Err(SourceOpenError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "a source path names a drive without its root",
        )));
    }
    Ok((root, steps))
}

/// Walks to the folder that holds `path`'s file, returning it and the
/// file's name.
fn walk(path: &Path) -> Result<(Folder, OsString), SourceOpenError> {
    // A relative path starts at the current folder's real location, so a
    // leading `..` climbs from there instead of stopping at the start.
    let absolute;
    let path = if path.has_root() {
        path
    } else {
        absolute = std::env::current_dir()
            .map_err(SourceOpenError::Io)?
            .join(path);
        absolute.as_path()
    };
    let (root, mut steps) = split(path)?;
    let Some(Step::Name(leaf)) = steps.pop() else {
        return Err(SourceOpenError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "a source path ends in a file name",
        )));
    };
    let mut current = native::open_root(root.as_deref()).map_err(SourceOpenError::Io)?;
    let mut above: Vec<Folder> = Vec::new();
    let mut pending: VecDeque<Step> = steps.into();
    let mut links = 0_u32;
    while let Some(step) = pending.pop_front() {
        match step {
            Step::Current => {}
            Step::Parent => {
                // `..` at a root is the root itself.
                if let Some(parent) = above.pop() {
                    current = parent;
                }
            }
            Step::Name(name) => {
                match native::child(&current, &name).map_err(SourceOpenError::Io)? {
                    Child::Directory(folder) => {
                        prove_inside(native::location(&current), native::location(&folder))?;
                        above.push(std::mem::replace(&mut current, folder));
                    }
                    Child::Link(link) => {
                        links += 1;
                        if links > MAX_LINKS || !link_is_trusted(&link) {
                            return Err(SourceOpenError::PathLinked);
                        }
                        let Some(target) = link.target else {
                            return Err(SourceOpenError::PathLinked);
                        };
                        let (target_root, target_steps) = split(&target)?;
                        if let Some(target_root) = target_root {
                            current = native::open_root(Some(&target_root))
                                .map_err(SourceOpenError::Io)?;
                            above.clear();
                        }
                        for step in target_steps.into_iter().rev() {
                            pending.push_front(step);
                        }
                    }
                }
            }
        }
    }
    Ok((current, leaf))
}
