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
    CertificateEvent, HostTlsError, HostTlsMaterial, HostTlsSource, MaterialKind, ReloadOutcome,
    SecureFileSystem, UnixFileFacts,
};
use arcen_transport::tls::{
    CertificateKeyPolicy, CertificateTimePolicy, SystemUnixClock, TlsPosture,
};

/// The process named in certificate lifecycle events.
const COMPONENT: &str = "arcen-pier-macos";
/// How often the served certificate is checked for its warning window and
/// expiry; the same minute the Linux Pier uses.
const STATUS_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

/// One SIGHUP reload at a time; a native file read that outlives its caller is
/// still supervised until it returns.
static RELOAD_SLOT: crate::blocking::BlockingSlot = crate::blocking::BlockingSlot::new();

fn reload_failure(failure: crate::blocking::ExclusiveFailure) -> String {
    match failure {
        crate::blocking::ExclusiveFailure::Busy => "a reload is already running".to_owned(),
        crate::blocking::ExclusiveFailure::Spawn(error) => format!("worker not started: {error}"),
        crate::blocking::ExclusiveFailure::Disconnected => {
            "worker ended without a result".to_owned()
        }
    }
}

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

/// The material this Pier serves, reloadable while it runs.
///
/// The QUIC endpoint is built over the material's resolver, so a reload that
/// passes every check is what the next handshake gets; a reload that fails
/// any check leaves the last good material serving.
#[derive(Debug)]
pub struct TlsLifecycle {
    material: HostTlsMaterial,
}

impl TlsLifecycle {
    /// Loads the material through the macOS file-system checks.
    ///
    /// # Errors
    ///
    /// Returns [`load`]'s refusal.
    pub fn load(certificate_path: &Path, key_path: &Path) -> Result<Self, HostTlsError> {
        load(certificate_path, key_path).map(|material| Self { material })
    }

    /// The material being served.
    #[must_use]
    pub const fn material(&self) -> &HostTlsMaterial {
        &self.material
    }

    /// The event describing the material at start: active, or expiring.
    #[must_use]
    pub fn startup_events(&self) -> Vec<CertificateEvent> {
        self.material.startup_event().into_iter().collect()
    }

    /// Reads the files again; serves them only when they pass every check.
    #[must_use]
    pub fn reload(&self) -> ReloadOutcome {
        self.material.reload(&MacSecureFileSystem)
    }

    /// The expiring and expired events, each once per certificate.
    #[must_use]
    pub fn status_events(&self) -> Vec<CertificateEvent> {
        self.material.status_events()
    }
}

/// Emits certificate lifecycle events through the Pier's telemetry.
pub fn emit(
    telemetry: &crate::observability::HostTelemetry,
    events: impl IntoIterator<Item = CertificateEvent>,
) {
    for event in events {
        telemetry.emit(
            event.kind,
            &crate::observability::SessionScope::service(
                arcen_telemetry::CorrelationId::from_uuid_v4_bytes(
                    crate::observability::random_correlation_bytes(),
                ),
            ),
            event.fields,
            arcen_telemetry::names::target::HEALTH,
            "tls certificate",
        );
    }
}

/// Starts watching the served material: `SIGHUP` reloads it (`sudo launchctl
/// kill HUP system/pier.arcen.tech.service`), and once a minute it is
/// checked for its warning window and expiry. Every outcome goes to `sink`.
///
/// The signal handler is installed before this returns, so a `SIGHUP` sent
/// after it can never take the default action and end the process. Must be
/// called inside a Tokio runtime.
///
/// # Errors
///
/// Returns the error when the `SIGHUP` handler cannot be installed; the
/// caller still serves, without a reload trigger.
pub fn spawn_watch<S>(
    lifecycle: Arc<TlsLifecycle>,
    sink: S,
) -> std::io::Result<tokio::task::JoinHandle<()>>
where
    S: Fn(Vec<CertificateEvent>) + Send + Sync + 'static,
{
    let mut hangup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
    Ok(tokio::spawn(async move {
        let sink = Arc::new(sink);
        let mut status = tokio::time::interval(STATUS_INTERVAL);
        status.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // The first tick is immediate; start-up already reported the state.
        status.tick().await;
        loop {
            tokio::select! {
                received = hangup.recv() => {
                    if received.is_none() {
                        break;
                    }
                    let worker = Arc::clone(&lifecycle);
                    match crate::blocking::run_exclusive(
                        "tls-reload",
                        &RELOAD_SLOT,
                        reload_failure,
                        move || worker.reload(),
                    )
                    .await
                    {
                        Ok(outcome) => {
                            match &outcome.result {
                                Ok(()) => eprintln!("tls: SIGHUP reload: serving the replacement"),
                                Err(error) => eprintln!(
                                    "tls: SIGHUP reload refused ({}); the last good certificate keeps serving",
                                    error.reason_class()
                                ),
                            }
                            sink(outcome.events);
                        }
                        Err(reason) => eprintln!("tls: SIGHUP reload not run: {reason}; the last good certificate keeps serving"),
                    }
                }
                _ = status.tick() => {
                    let events = lifecycle.status_events();
                    if !events.is_empty() {
                        sink(events);
                    }
                }
            }
        }
    }))
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
    fn a_reload_serves_a_valid_replacement_and_keeps_the_last_good_one_otherwise() -> TestResult {
        let directory = scratch("reload")?;
        let names = ["pier.example.internal".to_owned()];
        crate::host_cert::provision(
            &directory,
            arcen_transport::cert_provisioning::ProvisioningRequest::Ensure,
            &names,
            now_epoch_secs(),
        )?;
        let paths = crate::host_cert::MaterialPaths::in_directory(&directory);
        let lifecycle = TlsLifecycle::load(&paths.certificate, &paths.key)?;
        assert_eq!(
            lifecycle
                .startup_events()
                .iter()
                .map(|event| event.kind)
                .collect::<Vec<_>>(),
            vec![arcen_telemetry::LifecycleEventKind::TlsCertificateActive]
        );
        let first = lifecycle.material().metadata()?.cert_sha256;
        let identity = lifecycle.material().host_identity()?;

        // A malformed or half-written replacement is refused; the last good
        // certificate keeps serving.
        let good = std::fs::read(&paths.certificate)?;
        for replacement in [
            b"not a certificate".to_vec(),
            good[..good.len() / 2].to_vec(),
        ] {
            std::fs::write(&paths.certificate, &replacement)?;
            let failed = lifecycle.reload();
            assert!(failed.result.is_err());
            assert_eq!(
                failed
                    .events
                    .iter()
                    .map(|event| event.kind)
                    .collect::<Vec<_>>(),
                vec![arcen_telemetry::LifecycleEventKind::TlsCertificateReloadFailed]
            );
            assert_eq!(lifecycle.material().metadata()?.cert_sha256, first);
        }
        std::fs::write(&paths.certificate, &good)?;

        // A renewal is served at the next reload, under the same identity.
        crate::host_cert::provision(
            &directory,
            arcen_transport::cert_provisioning::ProvisioningRequest::Renew,
            &names,
            now_epoch_secs(),
        )?;
        let reloaded = lifecycle.reload();
        assert_eq!(reloaded.result, Ok(()));
        assert_eq!(
            reloaded.events.first().map(|event| event.kind),
            Some(arcen_telemetry::LifecycleEventKind::TlsCertificateReloaded)
        );
        assert_ne!(lifecycle.material().metadata()?.cert_sha256, first);
        assert_eq!(lifecycle.material().host_identity()?, identity);
        std::fs::remove_dir_all(&directory).ok();
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sighup_reloads_the_served_material() -> TestResult {
        let directory = scratch("sighup")?;
        let names = ["pier.example.internal".to_owned()];
        crate::host_cert::provision(
            &directory,
            arcen_transport::cert_provisioning::ProvisioningRequest::Ensure,
            &names,
            now_epoch_secs(),
        )?;
        let paths = crate::host_cert::MaterialPaths::in_directory(&directory);
        let lifecycle = Arc::new(TlsLifecycle::load(&paths.certificate, &paths.key)?);
        let first = lifecycle.material().metadata()?.cert_sha256;
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let watch = spawn_watch(Arc::clone(&lifecycle), move |events| {
            let _ = sender.send(events);
        })?;
        crate::host_cert::provision(
            &directory,
            arcen_transport::cert_provisioning::ProvisioningRequest::Renew,
            &names,
            now_epoch_secs(),
        )?;
        // SAFETY: raising a signal this process handles; the handler was
        // installed by `spawn_watch` before it returned.
        unsafe {
            libc::raise(libc::SIGHUP);
        }
        let events = tokio::time::timeout(std::time::Duration::from_secs(10), receiver.recv())
            .await?
            .ok_or("watch ended")?;
        assert_eq!(
            events.first().map(|event| event.kind),
            Some(arcen_telemetry::LifecycleEventKind::TlsCertificateReloaded)
        );
        assert_ne!(lifecycle.material().metadata()?.cert_sha256, first);
        watch.abort();
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
