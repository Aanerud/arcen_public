// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! A bundle that leaves the computer sealed: encrypted to a recipient's
//! public key before it is published, so the plaintext archive is never a
//! file a user could attach somewhere by mistake.
//!
//! The cipher is not chosen here. A [`BundleSealer`] supplies it (for
//! example an `age` recipient for the project's key); this module owns the
//! file handling: the plaintext archive lives only in a private file that
//! has no name while it is written (unlinked on Unix, delete-on-close on
//! Windows), is fingerprinted, sealed into a private partial file, and only
//! the sealed file is synced and published.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use super::summary::{ArchiveFingerprint, fingerprint};
use super::{SupportBundleError, create_output_file};

/// Encrypts a finished archive to one recipient.
pub trait BundleSealer {
    /// Appended to the archive's name: `<name>.<extension>` (for example
    /// `age`, giving `arcen-deck-support-<time>.zip.age`). ASCII letters and
    /// digits only.
    fn extension(&self) -> &'static str;

    /// Reads the whole plaintext archive from `archive` and writes only
    /// ciphertext to `sealed`.
    ///
    /// # Errors
    ///
    /// Sealing failed; nothing is published.
    fn seal(
        &self,
        archive: &mut dyn Read,
        sealed: &mut dyn Write,
    ) -> Result<(), SupportBundleError>;
}

/// A published sealed bundle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SealedArchive<T> {
    /// The sealed file.
    pub path: PathBuf,
    /// The plaintext archive's fingerprint, for the public summary
    /// ([`super::summary::PublicSummary`]); whoever opens the sealed file
    /// can check it.
    pub archive: ArchiveFingerprint,
    /// What `build` returned.
    pub value: T,
}

/// Writes a bundle into `directory` sealed by `sealer`: `build` writes the
/// plaintext archive into a private file without a lasting name
/// ([`create_unnamed_private`]), which is fingerprinted and sealed into the
/// first free `name(suffix).<extension>`; only that file is published
/// (synced, then renamed from its own private partial file, which is removed
/// on any failure or panic).
///
/// # Errors
///
/// The extension is not plain, no free name, a file could not be created,
/// `build` or the sealer failed, or the sealed file could not be synced or
/// published.
pub fn write_sealed_archive<T>(
    directory: &Path,
    name: impl Fn(&str) -> String,
    sealer: &dyn BundleSealer,
    build: impl FnOnce(File) -> Result<(File, T), SupportBundleError>,
) -> Result<SealedArchive<T>, SupportBundleError> {
    let extension = sealer.extension();
    if extension.is_empty()
        || extension.len() > 16
        || !extension.bytes().all(|byte| byte.is_ascii_alphanumeric())
    {
        return Err(SupportBundleError::Output(
            "support-bundle sealer extension must be 1 to 16 ASCII letters or digits".to_owned(),
        ));
    }
    let (path, sealed_partial, sealed_file) =
        create_output_file(directory, |suffix| format!("{}.{extension}", name(suffix)))?;
    let mut plain_name = sealed_partial.as_os_str().to_owned();
    plain_name.push(format!(".{}.plain", std::process::id()));
    let mut partial = RemoveOnDrop(Some(sealed_partial.clone()));
    // Every handle is closed inside `seal_into` on failure, before the guard
    // removes the partial file (Windows cannot remove an open one).
    let (sealed_file, archive, value) =
        seal_into(Path::new(&plain_name), sealed_file, sealer, build)?;
    let published = sealed_file.sync_all().and_then(|()| {
        drop(sealed_file);
        std::fs::rename(&sealed_partial, &path)
    });
    published.map_err(|error| {
        SupportBundleError::Output(format!(
            "publish sealed support bundle {}: {error}",
            path.display()
        ))
    })?;
    partial.disarm();
    Ok(SealedArchive {
        path,
        archive,
        value,
    })
}

/// Creates a private file for this process at `path` that no longer has a
/// name once this returns, so nothing — not a crash, not a failed delete —
/// can leave it behind: on Unix it is unlinked while open; on Windows it is
/// opened delete-on-close. Other platforms remove it when sealing ends.
///
/// # Errors
///
/// The file exists or could not be created or unlinked.
pub fn create_unnamed_private(path: &Path) -> std::io::Result<File> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ: u32 = 0x0000_0001;
        const FILE_SHARE_DELETE: u32 = 0x0000_0004;
        const FILE_FLAG_DELETE_ON_CLOSE: u32 = 0x0400_0000;
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_DELETE_ON_CLOSE)
            .open(path)
    }
    #[cfg(not(windows))]
    {
        let file = super::create_private_new(path)?;
        #[cfg(unix)]
        std::fs::remove_file(path)?;
        Ok(file)
    }
}

/// Removes a partial file when dropped, unless it was published.
struct RemoveOnDrop(Option<PathBuf>);

impl RemoveOnDrop {
    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn seal_into<T>(
    plain_name: &Path,
    mut sealed_file: File,
    sealer: &dyn BundleSealer,
    build: impl FnOnce(File) -> Result<(File, T), SupportBundleError>,
) -> Result<(File, ArchiveFingerprint, T), SupportBundleError> {
    let plain_file = create_unnamed_private(plain_name).map_err(|error| {
        SupportBundleError::Output(format!("create support-bundle plaintext file: {error}"))
    })?;
    let (mut plain_file, value) = build(plain_file)?;
    plain_file.flush()?;
    plain_file.seek(SeekFrom::Start(0))?;
    let archive = fingerprint(&mut plain_file)?;
    plain_file.seek(SeekFrom::Start(0))?;
    sealer.seal(&mut plain_file, &mut sealed_file)?;
    sealed_file.flush()?;
    Ok((sealed_file, archive, value))
}
