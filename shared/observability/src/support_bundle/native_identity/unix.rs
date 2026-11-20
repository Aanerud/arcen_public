// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Unix facts: every folder opened with `openat` relative to the one
//! before it, `O_NOFOLLOW` everywhere, and the file opened `O_NONBLOCK` so a
//! FIFO answers at once. Through rustix, without unsafe code.

use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use rustix::fs::{AtFlags, FileType, Mode, OFlags, Stat};
use rustix::io::Errno;

use super::super::source::{
    ContainerFacts, FileIdentity, IdentityScheme, LinkFacts, Owner, SourceFacts, SourceKind,
    VolumeFacts,
};
use super::{Child, Inspection, OpenedLeaf};

const FOLDER: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);
const SOURCE: OFlags = OFlags::RDONLY
    .union(OFlags::NOFOLLOW)
    .union(OFlags::NONBLOCK)
    .union(OFlags::CLOEXEC)
    .union(OFlags::NOCTTY);

/// An open folder on the way to a source.
pub(crate) struct Folder(File);

/// Opens the walk's root (`/`, or the current folder for a relative path).
/// The root itself is trusted.
pub(crate) fn open_root(root: Option<&Path>) -> io::Result<Folder> {
    let path = root.unwrap_or_else(|| Path::new("."));
    let fd = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    Ok(Folder(File::from(fd)))
}

/// Opens `name` in `parent` as a folder without following it; a link is
/// described (owner, target, the folder it sits in), not followed.
pub(crate) fn child(parent: &Folder, name: &OsStr) -> io::Result<Child> {
    match rustix::fs::openat(&parent.0, name, FOLDER, Mode::empty()) {
        Ok(fd) => Ok(Child::Directory(Folder(File::from(fd)))),
        Err(errno) if errno == Errno::LOOP || errno == Errno::NOTDIR => {
            let link = rustix::fs::statat(&parent.0, name, AtFlags::SYMLINK_NOFOLLOW)?;
            if FileType::from_raw_mode(link.st_mode) != FileType::Symlink {
                return Err(errno.into());
            }
            let container = rustix::fs::fstat(&parent.0)?;
            let target = rustix::fs::readlinkat(&parent.0, name, Vec::new())?;
            let permissions = Mode::from_raw_mode(container.st_mode);
            Ok(Child::Link(LinkFacts {
                target: Some(PathBuf::from(OsStr::from_bytes(target.as_bytes()))),
                owner: owner(link.st_uid),
                container: ContainerFacts {
                    owner: owner(container.st_uid),
                    writable_by_others: permissions.intersects(Mode::WGRP | Mode::WOTH),
                },
            }))
        }
        Err(errno) => Err(errno.into()),
    }
}

/// Unix walks by descriptor and needs no location proof.
pub(crate) fn location(_folder: &Folder) -> Option<&Path> {
    None
}

/// Inspects `name` in `parent` with `fstatat`, not following a link.
pub(crate) fn inspect(parent: &Folder, name: &OsStr) -> io::Result<Inspection> {
    let stat = rustix::fs::statat(&parent.0, name, AtFlags::SYMLINK_NOFOLLOW)?;
    Ok(Inspection {
        facts: facts(&stat),
        _held: None,
    })
}

/// Opens `name` in `parent` read-only with `O_NOFOLLOW | O_NONBLOCK |
/// O_CLOEXEC | O_NOCTTY`. `None`: it is a link, which the OS refused to
/// follow (`ELOOP`).
pub(crate) fn open(parent: &Folder, name: &OsStr) -> io::Result<Option<OpenedLeaf>> {
    let fd = match rustix::fs::openat(&parent.0, name, SOURCE, Mode::empty()) {
        Ok(fd) => fd,
        Err(errno) if errno == Errno::LOOP => return Ok(None),
        Err(errno) => return Err(errno.into()),
    };
    let stat = rustix::fs::fstat(&fd)?;
    Ok(Some(OpenedLeaf {
        file: File::from(fd),
        facts: facts(&stat),
        volume: VolumeFacts::LOCAL,
        location: None,
    }))
}

/// Clears `O_NONBLOCK` on an accepted regular file before it is read.
pub(crate) fn finish(file: &File) -> io::Result<()> {
    let flags = rustix::fs::fcntl_getfl(file)?;
    rustix::fs::fcntl_setfl(file, flags.difference(OFlags::NONBLOCK))?;
    Ok(())
}

fn owner(uid: u32) -> Owner {
    if uid == 0 {
        Owner::System
    } else if uid == rustix::process::geteuid().as_raw() {
        Owner::Process
    } else {
        Owner::Other
    }
}

#[allow(
    clippy::unnecessary_cast,
    clippy::cast_sign_loss,
    clippy::useless_conversion,
    clippy::cast_lossless,
    reason = "stat field types differ between Unix targets (macOS dev_t is i32)"
)]
fn facts(stat: &Stat) -> SourceFacts {
    let kind = match FileType::from_raw_mode(stat.st_mode) {
        FileType::RegularFile => SourceKind::Regular,
        FileType::Directory => SourceKind::Directory,
        FileType::Symlink => SourceKind::Link,
        _ => SourceKind::Other,
    };
    SourceFacts {
        kind,
        reparse_point: false,
        links: stat.st_nlink as u64,
        identity: FileIdentity {
            volume: stat.st_dev as u64,
            file: u128::from(stat.st_ino as u64),
        },
        scheme: IdentityScheme::DeviceInode,
    }
}
