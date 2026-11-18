// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Linux Pier TLS material: the Linux file-system checks behind the shared
//! `arcen_transport::host_material` loader, and the Pier's lifecycle events.
//!
//! Reading, validation, pinning, reload and expiry are shared with every Pier.
//! What is Linux's own is how a file is opened and judged: every path
//! component is walked with `openat(O_NOFOLLOW)`, so no link anywhere in the
//! path is followed, and the open file's `fstat` is held to the shared Unix
//! owner and mode rule.

use std::path::{Component, Path};
use std::sync::Arc;

use arcen_transport::host_material::{
    CertificateEvent, HostTlsError, HostTlsMaterial, HostTlsSource, MaterialKind, SecureFileSystem,
    UnixFileFacts,
};
use arcen_transport::tls::{CertificateKeyPolicy, SystemUnixClock};

/// The process named in certificate lifecycle events.
const COMPONENT: &str = "arcen-pier";
const MAX_RELOAD_REPORT_BYTES: usize = 512;

/// The configured certificate and key, when both are set.
pub(crate) fn source_from_config(config: &crate::cli::Config) -> Option<HostTlsSource> {
    Some(HostTlsSource {
        certificate_path: config.tls_cert.clone()?,
        private_key_path: config.tls_key.clone()?,
        expected_sans: config.tls_expected_sans.clone(),
        posture: config.tls_posture.clone(),
        key_policy: CertificateKeyPolicy::default(),
        time_policy: config.tls_time_policy,
    })
}

/// The material this Pier serves.
pub(crate) struct TlsLifecycle {
    material: HostTlsMaterial,
}

impl TlsLifecycle {
    pub(crate) fn load(source: HostTlsSource) -> Result<Self, HostTlsError> {
        let material = HostTlsMaterial::load(
            &SystemSecureFileSystem,
            source,
            Arc::new(SystemUnixClock),
            COMPONENT,
        )?;
        Ok(Self { material })
    }

    /// The rustls server configuration for the QUIC endpoint; the caller
    /// sets ALPN.
    pub(crate) fn rustls_server_config_for_quic(
        &self,
    ) -> Result<rustls::ServerConfig, HostTlsError> {
        self.material.server_config()
    }

    /// Stable direct-session host identity derived only from the active TLS SPKI.
    pub(crate) fn host_identity(&self) -> Result<arcen_identity::HostIdentity, HostTlsError> {
        self.material.host_identity()
    }

    pub(crate) fn emit_startup(&self, emitter: &crate::LifecycleEmitter) {
        emit(emitter, self.material.startup_event());
    }

    pub(crate) fn reload(&self, emitter: &crate::LifecycleEmitter) -> Result<(), HostTlsError> {
        let outcome = self.material.reload(&SystemSecureFileSystem);
        emit(emitter, outcome.events);
        outcome.result
    }

    pub(crate) fn check_and_emit_status(&self, emitter: &crate::LifecycleEmitter) {
        emit(emitter, self.material.status_events());
    }
}

fn emit(emitter: &crate::LifecycleEmitter, events: impl IntoIterator<Item = CertificateEvent>) {
    for event in events {
        crate::emit_lifecycle_event(
            emitter,
            event.kind,
            crate::eventlog::random_correlation_id(),
            event.fields,
        );
    }
}

/// Runs the logging and TLS halves of a SIGHUP independently and joins their
/// failures into one bounded report.
pub(crate) fn coordinate_sighup<L, T>(logging: L, tls: T) -> Result<(), String>
where
    L: FnOnce() -> Result<(), String>,
    T: FnOnce() -> Result<(), String>,
{
    let mut errors = Vec::new();
    if let Err(error) = logging() {
        errors.push(format!("logging: {error}"));
    }
    if let Err(error) = tls() {
        errors.push(format!("tls: {error}"));
    }
    if errors.is_empty() {
        return Ok(());
    }
    let mut report = errors.join("; ");
    if report.len() > MAX_RELOAD_REPORT_BYTES {
        let boundary = (0..=MAX_RELOAD_REPORT_BYTES)
            .rev()
            .find(|index| report.is_char_boundary(*index))
            .unwrap_or(0);
        report.truncate(boundary);
    }
    Err(report)
}

pub fn init_crypto() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

struct SystemSecureFileSystem;

#[cfg(target_os = "linux")]
impl SecureFileSystem for SystemSecureFileSystem {
    type Handle = std::fs::File;
    type Snapshot = UnixFileFacts;

    fn open(&self, path: &Path, kind: MaterialKind) -> Result<Self::Handle, HostTlsError> {
        linux_open_without_symlinks(path, kind)
    }

    fn snapshot(
        &self,
        handle: &Self::Handle,
        kind: MaterialKind,
    ) -> Result<UnixFileFacts, HostTlsError> {
        use std::os::fd::AsRawFd;

        let status = nix::sys::stat::fstat(handle.as_raw_fd())
            .map_err(|_| HostTlsError::Read(kind.label()))?;
        Ok(UnixFileFacts {
            device: status.st_dev,
            inode: status.st_ino,
            length: u64::try_from(status.st_size).map_err(|_| HostTlsError::Read(kind.label()))?,
            modified_seconds: status.st_mtime,
            modified_nanoseconds: status.st_mtime_nsec,
            changed_seconds: status.st_ctime,
            changed_nanoseconds: status.st_ctime_nsec,
            mode: status.st_mode,
            owner: status.st_uid,
            regular: nix::sys::stat::SFlag::from_bits_truncate(status.st_mode)
                .contains(nix::sys::stat::SFlag::S_IFREG),
            symlink: false,
        })
    }

    fn admit(
        &self,
        _handle: &Self::Handle,
        snapshot: &UnixFileFacts,
        kind: MaterialKind,
    ) -> Result<(), HostTlsError> {
        snapshot.admit(kind, nix::unistd::geteuid().as_raw())
    }
}

#[cfg(target_os = "linux")]
fn linux_open_without_symlinks(
    path: &Path,
    kind: MaterialKind,
) -> Result<std::fs::File, HostTlsError> {
    use std::ffi::OsString;
    use std::os::fd::{AsRawFd, FromRawFd};

    use nix::fcntl::{openat, OFlag};
    use nix::sys::stat::Mode;

    let mut parts = Vec::<OsString>::new();
    for component in path.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(part) => parts.push(part.to_os_string()),
            Component::ParentDir | Component::Prefix(_) => {
                return Err(HostTlsError::UnsafeTraversal(kind.label()));
            }
        }
    }
    let Some((leaf, ancestors)) = parts.split_last() else {
        return Err(HostTlsError::Open(kind.label()));
    };

    let mut directory = std::fs::File::open(if path.is_absolute() { "/" } else { "." })
        .map_err(|_| HostTlsError::Open(kind.label()))?;
    for part in ancestors {
        let descriptor = openat(
            Some(directory.as_raw_fd()),
            part.as_os_str(),
            OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| HostTlsError::Symlink(kind.label()))?;
        // SAFETY: `openat` returned a new owned descriptor.
        directory = unsafe { std::fs::File::from_raw_fd(descriptor) };
    }
    let descriptor = openat(
        Some(directory.as_raw_fd()),
        leaf.as_os_str(),
        OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| {
        if error == nix::errno::Errno::ENOENT {
            HostTlsError::Missing(kind.label())
        } else if error == nix::errno::Errno::ELOOP {
            HostTlsError::Symlink(kind.label())
        } else {
            HostTlsError::Open(kind.label())
        }
    })?;
    // SAFETY: `openat` returned a new owned descriptor.
    Ok(unsafe { std::fs::File::from_raw_fd(descriptor) })
}

/// Development builds on another Unix: the same open-file facts through std,
/// a leaf that must not be a link, and no owner rule.
#[cfg(not(target_os = "linux"))]
impl SecureFileSystem for SystemSecureFileSystem {
    type Handle = std::fs::File;
    type Snapshot = UnixFileFacts;

    fn open(&self, path: &Path, kind: MaterialKind) -> Result<Self::Handle, HostTlsError> {
        if path
            .components()
            .any(|component| matches!(component, Component::ParentDir))
        {
            return Err(HostTlsError::UnsafeTraversal(kind.label()));
        }
        let metadata =
            std::fs::symlink_metadata(path).map_err(|error| map_open_error(&error, kind))?;
        if metadata.file_type().is_symlink() {
            return Err(HostTlsError::Symlink(kind.label()));
        }
        std::fs::File::open(path).map_err(|error| map_open_error(&error, kind))
    }

    fn snapshot(
        &self,
        handle: &Self::Handle,
        kind: MaterialKind,
    ) -> Result<UnixFileFacts, HostTlsError> {
        let metadata = handle
            .metadata()
            .map_err(|_| HostTlsError::Read(kind.label()))?;
        let modified = metadata
            .modified()
            .ok()
            .and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok())
            .unwrap_or_default();
        Ok(UnixFileFacts {
            device: 0,
            inode: 0,
            length: metadata.len(),
            modified_seconds: i64::try_from(modified.as_secs()).unwrap_or(i64::MAX),
            modified_nanoseconds: i64::from(modified.subsec_nanos()),
            changed_seconds: 0,
            changed_nanoseconds: 0,
            mode: match kind {
                MaterialKind::Certificate => 0o644,
                MaterialKind::PrivateKey => 0o600,
            },
            owner: 0,
            regular: metadata.is_file(),
            symlink: false,
        })
    }

    fn admit(
        &self,
        _handle: &Self::Handle,
        snapshot: &UnixFileFacts,
        kind: MaterialKind,
    ) -> Result<(), HostTlsError> {
        snapshot.admit(kind, 0)
    }
}

#[cfg(not(target_os = "linux"))]
fn map_open_error(error: &std::io::Error, kind: MaterialKind) -> HostTlsError {
    if error.kind() == std::io::ErrorKind::NotFound {
        HostTlsError::Missing(kind.label())
    } else {
        HostTlsError::Open(kind.label())
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    #[test]
    fn sighup_attempts_logging_and_tls_independently_in_all_combinations() {
        for (logging_ok, tls_ok) in [(true, true), (true, false), (false, true), (false, false)] {
            let logging_calls = Cell::new(0);
            let tls_calls = Cell::new(0);
            let result = coordinate_sighup(
                || {
                    logging_calls.set(logging_calls.get() + 1);
                    logging_ok.then_some(()).ok_or_else(|| "failed".to_string())
                },
                || {
                    tls_calls.set(tls_calls.get() + 1);
                    tls_ok.then_some(()).ok_or_else(|| "failed".to_string())
                },
            );
            assert_eq!(logging_calls.get(), 1);
            assert_eq!(tls_calls.get(), 1);
            assert_eq!(result.is_ok(), logging_ok && tls_ok);
            if let Err(report) = result {
                assert!(report.len() <= MAX_RELOAD_REPORT_BYTES);
            }
        }
    }

    /// The real Linux file system: material in the deployed layout loads,
    /// keeps the pins written beside it, and a link anywhere in the path or a
    /// readable key is refused.
    #[cfg(target_os = "linux")]
    #[test]
    fn deployed_layout_loads_through_the_linux_file_system() {
        use std::os::unix::fs::PermissionsExt as _;

        use arcen_transport::cert_marker;
        use arcen_transport::cert_store::{MaterialPaths, PinFiles};

        let root = std::env::temp_dir().join(format!("arcen-linux-tls-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let directory = root.join("etc-arcen");
        std::fs::create_dir_all(&directory).expect("directory");
        let paths = MaterialPaths::in_directory(&directory);

        let key = rcgen::KeyPair::generate().expect("key");
        let mut params =
            rcgen::CertificateParams::new(vec!["203.0.113.7".to_owned()]).expect("params");
        params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
        let certificate = params.self_signed(&key).expect("certificate").pem();
        std::fs::write(&paths.certificate, &certificate).expect("certificate");
        std::fs::write(&paths.key, key.serialize_pem()).expect("key");
        std::fs::set_permissions(&paths.certificate, std::fs::Permissions::from_mode(0o644))
            .expect("certificate mode");
        std::fs::set_permissions(&paths.key, std::fs::Permissions::from_mode(0o600))
            .expect("key mode");
        let pins = PinFiles::for_certificate(certificate.as_bytes()).expect("pins");

        let source = |directory: &Path| HostTlsSource {
            certificate_path: directory.join("host.crt"),
            private_key_path: directory.join("host.key"),
            expected_sans: vec!["203.0.113.7".to_owned()],
            posture: arcen_transport::tls::TlsPosture::default(),
            key_policy: CertificateKeyPolicy::default(),
            time_policy: arcen_transport::tls::CertificateTimePolicy::default(),
        };
        let lifecycle = TlsLifecycle::load(source(&directory)).expect("deployed material loads");
        let identity = lifecycle.host_identity().expect("identity");
        let spki = cert_marker::normalize_spki_pin(&pins.spki_pin).expect("spki pin");
        let spki_hex = cert_marker::hex_lower(
            &cert_marker::base64_decode(spki.trim_start_matches("sha256/")).expect("base64"),
        );
        assert_eq!(identity.as_str(), format!("spki-sha256:{spki_hex}"));

        let linked = root.join("linked");
        std::os::unix::fs::symlink(&directory, &linked).expect("link");
        assert_eq!(
            TlsLifecycle::load(source(&linked)).err(),
            Some(HostTlsError::Symlink("certificate"))
        );

        std::fs::set_permissions(&paths.key, std::fs::Permissions::from_mode(0o640))
            .expect("key mode");
        assert_eq!(
            TlsLifecycle::load(source(&directory)).err(),
            Some(HostTlsError::InsecureMode("private key"))
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
