// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The macOS file-system checks behind the shared
//! `arcen_transport::host_material` loader.
//!
//! Reading, size bounds, change detection, validation and pinning are shared
//! with every Pier, as is the Unix owner and mode rule (the key `0600`, the
//! certificate at most `0644`, owned by root or the service account). What is
//! macOS's own is where links are refused: only the file itself is opened
//! with `O_NOFOLLOW`. macOS's own system paths (`/etc`, `/tmp`, `/var`) are
//! links into `/private`, so refusing a link anywhere in the path, as the
//! Linux Pier does, would refuse ordinary locations; the managed directory
//! is root-owned and created by the installer.

use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::path::{Component, Path};
use std::sync::Arc;

use arcen_transport::host_material::{
    HostTlsError, HostTlsMaterial, HostTlsSource, MaterialKind, SecureFileSystem, UnixFileFacts,
};
use arcen_transport::tls::{
    CertificateKeyPolicy, CertificateTimePolicy, SystemUnixClock, TlsPosture,
};

/// The process named in certificate lifecycle events.
const COMPONENT: &str = "arcen-pier-macos";

/// Loads and validates the certificate and key a Pier serves.
///
/// # Errors
///
/// Returns the shared refusal: a missing, linked, unsafe, oversized or
/// invalid file, or a key that does not match the certificate.
pub fn load(certificate_path: &Path, key_path: &Path) -> Result<HostTlsMaterial, HostTlsError> {
    HostTlsMaterial::load(
        &MacSecureFileSystem,
        HostTlsSource {
            certificate_path: certificate_path.to_path_buf(),
            private_key_path: key_path.to_path_buf(),
            expected_sans: Vec::new(),
            posture: TlsPosture::default(),
            key_policy: CertificateKeyPolicy::default(),
            time_policy: CertificateTimePolicy::default(),
        },
        Arc::new(SystemUnixClock),
        COMPONENT,
    )
}

struct MacSecureFileSystem;

impl SecureFileSystem for MacSecureFileSystem {
    type Handle = std::fs::File;
    type Snapshot = UnixFileFacts;

    fn open(&self, path: &Path, kind: MaterialKind) -> Result<Self::Handle, HostTlsError> {
        if path
            .components()
            .any(|component| matches!(component, Component::ParentDir))
        {
            return Err(HostTlsError::UnsafeTraversal(kind.label()));
        }
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)
            .map_err(|error| match error.kind() {
                std::io::ErrorKind::NotFound => HostTlsError::Missing(kind.label()),
                _ if error.raw_os_error() == Some(libc::ELOOP) => {
                    HostTlsError::Symlink(kind.label())
                }
                _ => HostTlsError::Open(kind.label()),
            })
    }

    fn snapshot(
        &self,
        handle: &Self::Handle,
        kind: MaterialKind,
    ) -> Result<UnixFileFacts, HostTlsError> {
        let metadata = handle
            .metadata()
            .map_err(|_| HostTlsError::Read(kind.label()))?;
        Ok(UnixFileFacts {
            device: metadata.dev(),
            inode: metadata.ino(),
            length: metadata.size(),
            modified_seconds: metadata.mtime(),
            modified_nanoseconds: metadata.mtime_nsec(),
            changed_seconds: metadata.ctime(),
            changed_nanoseconds: metadata.ctime_nsec(),
            mode: metadata.mode(),
            owner: metadata.uid(),
            regular: metadata.file_type().is_file(),
            symlink: metadata.file_type().is_symlink(),
        })
    }

    fn admit(
        &self,
        _handle: &Self::Handle,
        snapshot: &UnixFileFacts,
        kind: MaterialKind,
    ) -> Result<(), HostTlsError> {
        snapshot.admit(kind, crate::timezone::current_euid())
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    /// The loader checks the validity period against the real clock.
    fn now_epoch_secs() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_secs())
    }

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn scratch(name: &str) -> std::io::Result<std::path::PathBuf> {
        let directory = std::env::current_dir()?
            .join("target")
            .join(format!("arcen-tls-material-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory)?;
        Ok(directory)
    }

    #[test]
    fn provisioned_material_loads_and_keeps_its_pins() -> TestResult {
        let directory = scratch("provisioned")?;
        crate::host_cert::provision(
            &directory,
            arcen_transport::cert_provisioning::ProvisioningRequest::Ensure,
            &["pier.example.internal".to_owned()],
            now_epoch_secs(),
        )?;
        let paths = crate::host_cert::MaterialPaths::in_directory(&directory);
        let material = load(&paths.certificate, &paths.key)?;
        let metadata = material.metadata()?;
        let pin = std::fs::read_to_string(&paths.spki_pin)?;
        assert_eq!(
            arcen_transport::cert_marker::normalize_spki_pin(&pin),
            Some(format!(
                "sha256/{}",
                arcen_transport::cert_marker::base64_encode(&metadata.spki_sha256)
            ))
        );
        std::fs::remove_dir_all(&directory).ok();
        Ok(())
    }

    #[test]
    fn a_linked_or_readable_key_is_refused() -> TestResult {
        let directory = scratch("refusals")?;
        crate::host_cert::provision(
            &directory,
            arcen_transport::cert_provisioning::ProvisioningRequest::Ensure,
            &["pier.example.internal".to_owned()],
            now_epoch_secs(),
        )?;
        let paths = crate::host_cert::MaterialPaths::in_directory(&directory);
        let linked = directory.join("linked.key");
        std::os::unix::fs::symlink(&paths.key, &linked)?;
        assert_eq!(
            load(&paths.certificate, &linked).err(),
            Some(HostTlsError::Symlink("private key"))
        );
        std::fs::set_permissions(&paths.key, std::fs::Permissions::from_mode(0o644))?;
        assert_eq!(
            load(&paths.certificate, &paths.key).err(),
            Some(HostTlsError::InsecureMode("private key"))
        );
        std::fs::remove_dir_all(&directory).ok();
        Ok(())
    }
}
