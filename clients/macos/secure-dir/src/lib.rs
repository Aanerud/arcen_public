// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Directories a privileged process can trust.
//!
//! A root process that keeps state or a socket under a path must not let a
//! less privileged user decide where that path leads. `/var/run` on macOS, for
//! example, is `root:daemon` 0775 without the sticky bit: anything in group
//! `daemon` can rename, replace or delete its entries between a check and a
//! use.
//!
//! [`TrustedDir`] therefore walks the path from `/` one component at a time,
//! opening each with `O_DIRECTORY | O_NOFOLLOW` relative to its parent's
//! descriptor, and checks each on that descriptor:
//!
//! - every ancestor is a directory owned by root (or by the policy's owner)
//!   that neither group nor others can write — so nobody else can swap a
//!   component — and
//! - the directory itself is owned by the policy's owner with exactly the
//!   policy's mode.
//!
//! The result is pinned by its descriptor: callers create, open, rename and
//! remove entries relative to it (`openat`, `renameat`, `unlinkat`), never by
//! re-resolving the path. There is no unsafe code here; descriptors and
//! `*at` calls come from `rustix`.

#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used))]

use rustix::fs::{AtFlags, FileType, Mode, OFlags, RawMode, Stat, Uid};
use rustix::io::Errno;
use std::ffi::OsStr;
use std::io::ErrorKind;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::net::UnixListener;
use std::path::{Component, Path, PathBuf};

/// Who must own the directory, and its exact permission bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirPolicy {
    pub owner_uid: u32,
    /// Exact mode, for example `0o700` or `0o755`.
    pub mode: u32,
}

/// What a descriptor's `fstat` says about one path component.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    pub is_directory: bool,
    pub uid: u32,
    /// Permission bits, including set-id and sticky bits.
    pub mode: u32,
}

impl Entry {
    fn of(stat: &Stat) -> Self {
        Self {
            is_directory: FileType::from_raw_mode(stat.st_mode) == FileType::Directory,
            uid: stat.st_uid,
            mode: u32::from(stat.st_mode) & 0o7777,
        }
    }
}

/// Checks one ancestor of a trusted directory.
///
/// # Errors
///
/// Refuses anything but a directory owned by root or `owner_uid` that group
/// and others cannot write (a sticky bit does not make a writable directory
/// acceptable: it stops deletion, not creation of a look-alike).
pub fn check_ancestor(path: &Path, entry: Entry, owner_uid: u32) -> Result<(), String> {
    if !entry.is_directory {
        return Err(format!("{} is not a directory", path.display()));
    }
    if entry.uid != 0 && entry.uid != owner_uid {
        return Err(format!(
            "{} is owned by uid {}, not root",
            path.display(),
            entry.uid
        ));
    }
    if entry.mode & 0o022 != 0 {
        return Err(format!(
            "{} is writable by group or others (mode {:o})",
            path.display(),
            entry.mode
        ));
    }
    Ok(())
}

/// Checks the trusted directory itself.
///
/// # Errors
///
/// Refuses anything but a directory owned by the policy's owner with exactly
/// the policy's mode.
pub fn check_final(path: &Path, entry: Entry, policy: DirPolicy) -> Result<(), String> {
    if !entry.is_directory {
        return Err(format!("{} is not a directory", path.display()));
    }
    if entry.uid != policy.owner_uid {
        return Err(format!(
            "{} is owned by uid {}, not {}",
            path.display(),
            entry.uid,
            policy.owner_uid
        ));
    }
    if entry.mode != policy.mode {
        return Err(format!(
            "{} has mode {:o}, not {:o}",
            path.display(),
            entry.mode,
            policy.mode
        ));
    }
    Ok(())
}

/// The components below `/` of an absolute path with no `.` or `..`.
///
/// # Errors
///
/// Refuses a relative path, `/` itself, and any `..` component.
pub fn components(path: &Path) -> Result<Vec<&OsStr>, String> {
    let mut parts = path.components();
    if parts.next() != Some(Component::RootDir) {
        return Err(format!("{} is not an absolute path", path.display()));
    }
    let mut names = Vec::new();
    for part in parts {
        match part {
            Component::Normal(name) => names.push(name),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(format!("{} must not contain `..`", path.display()));
            }
        }
    }
    if names.is_empty() {
        return Err("the root directory itself cannot be a trusted directory".to_owned());
    }
    Ok(names)
}

fn refused(reason: impl Into<String>) -> std::io::Error {
    std::io::Error::new(ErrorKind::PermissionDenied, reason.into())
}

/// A permission mode from plain bits.
#[must_use]
pub fn mode(bits: u32) -> Mode {
    Mode::from_bits_truncate(RawMode::try_from(bits & 0o7777).unwrap_or(0))
}

const DIRECTORY_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

/// A directory verified from `/` down and pinned by its descriptor.
#[derive(Debug)]
pub struct TrustedDir {
    path: PathBuf,
    fd: OwnedFd,
}

impl TrustedDir {
    /// Opens and verifies an existing directory.
    ///
    /// # Errors
    ///
    /// Returns `NotFound` when a component is missing, `PermissionDenied`
    /// with the reason when a component is a link, not a directory, or fails
    /// its ownership or mode check, and other I/O errors as they are.
    pub fn open(path: &Path, policy: DirPolicy) -> std::io::Result<Self> {
        walk(path, policy, None)
    }

    /// Like [`Self::open`], but creates missing components: ancestors with
    /// `parent_mode`, the directory itself with the policy's mode, each set
    /// exactly with `fchmod` on its new descriptor (no umask dependence).
    ///
    /// # Errors
    ///
    /// As [`Self::open`], plus the error of a failed creation.
    pub fn create(path: &Path, policy: DirPolicy, parent_mode: u32) -> std::io::Result<Self> {
        walk(path, policy, Some(parent_mode))
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The pinned descriptor: use it with `*at` calls.
    #[must_use]
    pub fn fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }

    /// Binds the Unix socket `name` in this directory for one client: owned
    /// by `client_uid`, mode 0600.
    ///
    /// A stale socket at `name` is removed first; anything else there (a
    /// link, a file, a FIFO, a directory) is refused, never removed. After
    /// the bind, the mode and owner are set with `fchmodat`/`fchownat` and
    /// `AT_SYMLINK_NOFOLLOW` relative to the pinned directory: no `chmod` or
    /// `chown` ever resolves a path a non-root user could influence. The
    /// result is checked again before it is returned.
    ///
    /// The process umask is not changed here (it is process-wide). A caller
    /// that must keep the socket unconnectable by others even for the moment
    /// between the bind and the `fchmodat` sets a restrictive umask itself,
    /// before it starts any thread; the USB helper sets 0077 at start.
    ///
    /// # Errors
    ///
    /// Refuses an invalid name or an entry that is not a socket, and returns
    /// the I/O error of a failed bind, ownership change or check.
    pub fn bind_socket(&self, name: &str, client_uid: u32) -> std::io::Result<UnixListener> {
        check_entry_name(name).map_err(refused)?;
        match rustix::fs::statat(self.fd(), name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) if FileType::from_raw_mode(stat.st_mode) == FileType::Socket => {
                rustix::fs::unlinkat(self.fd(), name, AtFlags::empty())?;
            }
            Ok(_) => {
                return Err(refused(format!(
                    "{} exists and is not a socket; refusing to replace it",
                    self.path.join(name).display()
                )));
            }
            Err(Errno::NOENT) => {}
            Err(error) => return Err(error.into()),
        }
        let listener = UnixListener::bind(self.path.join(name))?;
        rustix::fs::chmodat(self.fd(), name, mode(0o600), AtFlags::SYMLINK_NOFOLLOW)?;
        rustix::fs::chownat(
            self.fd(),
            name,
            Some(Uid::from_raw(client_uid)),
            None,
            AtFlags::SYMLINK_NOFOLLOW,
        )?;
        let stat = rustix::fs::statat(self.fd(), name, AtFlags::SYMLINK_NOFOLLOW)?;
        let permissions = u32::from(stat.st_mode) & 0o7777;
        if FileType::from_raw_mode(stat.st_mode) != FileType::Socket
            || stat.st_uid != client_uid
            || permissions != 0o600
        {
            return Err(refused(format!(
                "{} is not the expected 0600 socket of uid {client_uid} (uid {}, mode {permissions:o})",
                self.path.join(name).display(),
                stat.st_uid
            )));
        }
        Ok(listener)
    }
}

/// A single directory entry name: not empty, not `.` or `..`, no `/`.
///
/// # Errors
///
/// Returns a reason for any other name.
pub fn check_entry_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\0') {
        return Err(format!("{name:?} is not a single directory entry name"));
    }
    Ok(())
}

fn walk(path: &Path, policy: DirPolicy, create: Option<u32>) -> std::io::Result<TrustedDir> {
    let names = components(path).map_err(refused)?;
    let mut current = rustix::fs::open("/", DIRECTORY_FLAGS, Mode::empty())?;
    let mut walked = PathBuf::from("/");
    check_ancestor(
        &walked,
        Entry::of(&rustix::fs::fstat(&current)?),
        policy.owner_uid,
    )
    .map_err(refused)?;
    for (index, name) in names.iter().enumerate() {
        let last = index + 1 == names.len();
        walked.push(name);
        let next = match rustix::fs::openat(&current, *name, DIRECTORY_FLAGS, Mode::empty()) {
            Ok(fd) => fd,
            Err(Errno::NOENT) if create.is_some() => {
                let wanted = if last {
                    policy.mode
                } else {
                    create.unwrap_or(policy.mode)
                };
                let created = match rustix::fs::mkdirat(&current, *name, mode(wanted)) {
                    Ok(()) => true,
                    Err(Errno::EXIST) => false,
                    Err(error) => return Err(error.into()),
                };
                let fd = rustix::fs::openat(&current, *name, DIRECTORY_FLAGS, Mode::empty())
                    .map_err(|error| link_or(error, &walked))?;
                if created {
                    rustix::fs::fchmod(&fd, mode(wanted))?;
                }
                fd
            }
            Err(error) => return Err(link_or(error, &walked)),
        };
        let entry = Entry::of(&rustix::fs::fstat(&next)?);
        if last {
            check_final(&walked, entry, policy)
        } else {
            check_ancestor(&walked, entry, policy.owner_uid)
        }
        .map_err(refused)?;
        current = next;
    }
    Ok(TrustedDir {
        path: walked,
        fd: current,
    })
}

/// A link or a non-directory where a directory must be is a refusal, not an
/// ordinary I/O error.
fn link_or(error: Errno, walked: &Path) -> std::io::Error {
    if error == Errno::LOOP || error == Errno::NOTDIR {
        refused(format!(
            "{} is a symbolic link or not a directory",
            walked.display()
        ))
    } else {
        error.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn directory(uid: u32, mode: u32) -> Entry {
        Entry {
            is_directory: true,
            uid,
            mode,
        }
    }

    #[test]
    fn ancestors_must_be_root_or_owner_directories_nobody_else_can_write() {
        let path = Path::new("/private/var/db");
        assert!(check_ancestor(path, directory(0, 0o755), 0).is_ok());
        assert!(check_ancestor(path, directory(501, 0o750), 501).is_ok());
        // `/var/run` on macOS: root:daemon 0775.
        assert!(check_ancestor(path, directory(0, 0o775), 0).is_err());
        // `/tmp`: sticky, world-writable.
        assert!(check_ancestor(path, directory(0, 0o1777), 0).is_err());
        assert!(check_ancestor(path, directory(0, 0o757), 0).is_err());
        assert!(check_ancestor(path, directory(501, 0o755), 0).is_err());
        let file = Entry {
            is_directory: false,
            ..directory(0, 0o755)
        };
        assert!(check_ancestor(path, file, 0).is_err());
    }

    #[test]
    fn the_directory_itself_needs_its_exact_owner_and_mode() {
        let path = Path::new("/private/var/db/arcen/state");
        let policy = DirPolicy {
            owner_uid: 0,
            mode: 0o700,
        };
        assert!(check_final(path, directory(0, 0o700), policy).is_ok());
        assert!(check_final(path, directory(0, 0o750), policy).is_err());
        assert!(check_final(path, directory(0, 0o1700), policy).is_err());
        assert!(check_final(path, directory(501, 0o700), policy).is_err());
        let error = check_final(path, directory(0, 0o755), policy).unwrap_err();
        assert_eq!(error, "/private/var/db/arcen/state has mode 755, not 700");
    }

    #[test]
    fn entry_names_are_single_components() {
        assert!(check_entry_name("usb-helper.sock").is_ok());
        for bad in [
            "",
            ".",
            "..",
            "run/usb-helper.sock",
            "/usb-helper.sock",
            "a\0b",
        ] {
            assert!(check_entry_name(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn only_absolute_paths_without_parent_steps_are_walked() {
        assert_eq!(
            components(Path::new("/private/var/db/arcen")).unwrap(),
            vec!["private", "var", "db", "arcen"]
        );
        assert_eq!(
            components(Path::new("/private/./var")).unwrap(),
            vec!["private", "var"]
        );
        assert!(components(Path::new("private/var")).is_err());
        assert!(components(Path::new("/private/../tmp")).is_err());
        assert!(components(Path::new("/")).is_err());
    }
}
