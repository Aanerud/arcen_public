//! Linux Pier TLS material loading, validation, reload, and lifecycle events.

use std::fmt::{Display, Formatter};
use std::io::{BufReader, Cursor, Read};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use arcen_transport::tls::{
    CertificateKeyPolicy, CertificateMetadata, CertificateTimePolicy, ReloadingCertifiedKey,
    SystemUnixClock, TlsPosture, UnixClock, ValidatedCertificate,
};
use rustls::server::ResolvesServerCert;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
#[cfg(feature = "wss-compat")]
use tokio_rustls::TlsAcceptor;
use zeroize::Zeroizing;

pub(crate) const MAX_CERTIFICATE_PEM_BYTES: u64 = 1024 * 1024;
pub(crate) const MAX_PRIVATE_KEY_PEM_BYTES: u64 = 128 * 1024;
const COMPONENT: &str = "arcen-pier";
const SOURCE: &str = "pem";
const MAX_RELOAD_REPORT_BYTES: usize = 512;

#[derive(Debug, Clone)]
pub(crate) struct TlsFileSource {
    certificate_path: PathBuf,
    private_key_path: PathBuf,
    expected_sans: Vec<String>,
    posture: TlsPosture,
    key_policy: CertificateKeyPolicy,
    time_policy: CertificateTimePolicy,
}

impl TlsFileSource {
    pub(crate) fn from_config(config: &crate::cli::Config) -> Option<Self> {
        Some(Self {
            certificate_path: config.tls_cert.clone()?,
            private_key_path: config.tls_key.clone()?,
            expected_sans: config.tls_expected_sans.clone(),
            posture: config.tls_posture.clone(),
            key_policy: CertificateKeyPolicy::default(),
            time_policy: config.tls_time_policy,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MaterialKind {
    Certificate,
    PrivateKey,
}

impl MaterialKind {
    const fn label(self) -> &'static str {
        match self {
            Self::Certificate => "certificate",
            Self::PrivateKey => "private key",
        }
    }

    const fn maximum(self) -> u64 {
        match self {
            Self::Certificate => MAX_CERTIFICATE_PEM_BYTES,
            Self::PrivateKey => MAX_PRIVATE_KEY_PEM_BYTES,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TlsLoadError {
    Missing(&'static str),
    Open(&'static str),
    UnsafeTraversal(&'static str),
    Symlink(&'static str),
    NotRegular(&'static str),
    UnexpectedOwner(&'static str),
    InsecureMode(&'static str),
    Oversized(&'static str, u64),
    Read(&'static str),
    ChangedDuringRead(&'static str),
    MalformedCertificatePem,
    EmptyCertificateChain,
    MalformedPrivateKeyPem,
    MissingPrivateKey,
    MultiplePrivateKeys,
    CertificateRejected,
    ResolverRejected,
    PostureRejected,
}

impl TlsLoadError {
    pub(crate) const fn reason_class(&self) -> &'static str {
        match self {
            Self::Missing(_) => "material_missing",
            Self::Open(_) | Self::Read(_) => "material_unavailable",
            Self::UnsafeTraversal(_) | Self::Symlink(_) => "unsafe_path",
            Self::NotRegular(_) => "unsafe_file_type",
            Self::UnexpectedOwner(_) => "unexpected_owner",
            Self::InsecureMode(_) => "insecure_mode",
            Self::Oversized(_, _) => "material_oversized",
            Self::ChangedDuringRead(_) => "material_changed",
            Self::MalformedCertificatePem
            | Self::EmptyCertificateChain
            | Self::MalformedPrivateKeyPem
            | Self::MissingPrivateKey
            | Self::MultiplePrivateKeys => "pem_invalid",
            Self::CertificateRejected | Self::ResolverRejected => "certificate_rejected",
            Self::PostureRejected => "posture_rejected",
        }
    }
}

impl Display for TlsLoadError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing(kind) => write!(formatter, "TLS {kind} file does not exist"),
            Self::Open(kind) => write!(formatter, "TLS {kind} file could not be opened securely"),
            Self::UnsafeTraversal(kind) => {
                write!(formatter, "TLS {kind} path contains unsafe traversal")
            }
            Self::Symlink(kind) => write!(formatter, "TLS {kind} path contains a symbolic link"),
            Self::NotRegular(kind) => write!(formatter, "TLS {kind} is not a regular file"),
            Self::UnexpectedOwner(kind) => {
                write!(formatter, "TLS {kind} has an untrusted owner")
            }
            Self::InsecureMode(kind) => write!(formatter, "TLS {kind} has insecure permissions"),
            Self::Oversized(kind, maximum) => {
                write!(formatter, "TLS {kind} exceeds the {maximum}-byte limit")
            }
            Self::Read(kind) => write!(formatter, "TLS {kind} could not be read"),
            Self::ChangedDuringRead(kind) => {
                write!(formatter, "TLS {kind} changed while it was read")
            }
            Self::MalformedCertificatePem => {
                formatter.write_str("TLS certificate PEM contains an invalid or unsupported block")
            }
            Self::EmptyCertificateChain => {
                formatter.write_str("TLS certificate PEM contains no certificates")
            }
            Self::MalformedPrivateKeyPem => {
                formatter.write_str("TLS private-key PEM contains an invalid or unsupported block")
            }
            Self::MissingPrivateKey => {
                formatter.write_str("TLS private-key PEM contains no usable private key")
            }
            Self::MultiplePrivateKeys => {
                formatter.write_str("TLS private-key PEM must contain exactly one private key")
            }
            Self::CertificateRejected => formatter.write_str(
                "TLS certificate/key failed validity, SAN, key-policy, or key-match validation",
            ),
            Self::ResolverRejected => {
                formatter.write_str("TLS certificate resolver rejected the active certificate")
            }
            Self::PostureRejected => {
                formatter.write_str("TLS version/cipher posture could not build a server config")
            }
        }
    }
}

impl std::error::Error for TlsLoadError {}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileSnapshot {
    device: u64,
    inode: u64,
    length: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
    mode: u32,
    owner: u32,
    regular: bool,
    symlink: bool,
}

trait SecureFileSystem {
    type Handle: Read;

    fn open(&self, path: &Path, kind: MaterialKind) -> Result<Self::Handle, TlsLoadError>;
    fn snapshot(
        &self,
        handle: &Self::Handle,
        kind: MaterialKind,
    ) -> Result<FileSnapshot, TlsLoadError>;
    fn effective_uid(&self) -> u32;
}

fn read_bounded<F: SecureFileSystem>(
    file_system: &F,
    path: &Path,
    kind: MaterialKind,
) -> Result<Zeroizing<Vec<u8>>, TlsLoadError> {
    let mut handle = file_system.open(path, kind)?;
    let before = file_system.snapshot(&handle, kind)?;
    validate_snapshot(&before, kind, file_system.effective_uid())?;
    if before.length > kind.maximum() {
        return Err(TlsLoadError::Oversized(kind.label(), kind.maximum()));
    }

    let mut bytes = Zeroizing::new(Vec::with_capacity(before.length as usize));
    (&mut handle)
        .take(kind.maximum() + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| TlsLoadError::Read(kind.label()))?;
    if bytes.len() as u64 > kind.maximum() {
        return Err(TlsLoadError::Oversized(kind.label(), kind.maximum()));
    }
    let after = file_system.snapshot(&handle, kind)?;
    if before != after || bytes.len() as u64 != before.length {
        return Err(TlsLoadError::ChangedDuringRead(kind.label()));
    }
    Ok(bytes)
}

fn validate_snapshot(
    snapshot: &FileSnapshot,
    kind: MaterialKind,
    effective_uid: u32,
) -> Result<(), TlsLoadError> {
    if snapshot.symlink {
        return Err(TlsLoadError::Symlink(kind.label()));
    }
    if !snapshot.regular {
        return Err(TlsLoadError::NotRegular(kind.label()));
    }
    if snapshot.owner != 0 && snapshot.owner != effective_uid {
        return Err(TlsLoadError::UnexpectedOwner(kind.label()));
    }
    let mode = snapshot.mode & 0o7777;
    let secure = match kind {
        MaterialKind::PrivateKey => mode == 0o600,
        MaterialKind::Certificate => mode & !0o644 == 0 && mode & 0o400 != 0,
    };
    if !secure {
        return Err(TlsLoadError::InsecureMode(kind.label()));
    }
    Ok(())
}

fn parse_material(
    certificate_pem: &[u8],
    private_key_pem: &[u8],
) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>), TlsLoadError> {
    let mut certificates = Vec::new();
    for item in rustls_pemfile::read_all(&mut BufReader::new(Cursor::new(certificate_pem))) {
        match item.map_err(|_| TlsLoadError::MalformedCertificatePem)? {
            rustls_pemfile::Item::X509Certificate(certificate) => certificates.push(certificate),
            _ => return Err(TlsLoadError::MalformedCertificatePem),
        }
    }
    if certificates.is_empty() {
        return Err(TlsLoadError::EmptyCertificateChain);
    }

    let mut private_key = None;
    for item in rustls_pemfile::read_all(&mut BufReader::new(Cursor::new(private_key_pem))) {
        let key = match item.map_err(|_| TlsLoadError::MalformedPrivateKeyPem)? {
            rustls_pemfile::Item::Pkcs1Key(key) => PrivateKeyDer::Pkcs1(key),
            rustls_pemfile::Item::Pkcs8Key(key) => PrivateKeyDer::Pkcs8(key),
            rustls_pemfile::Item::Sec1Key(key) => PrivateKeyDer::Sec1(key),
            _ => return Err(TlsLoadError::MalformedPrivateKeyPem),
        };
        if private_key.replace(key).is_some() {
            return Err(TlsLoadError::MultiplePrivateKeys);
        }
    }
    Ok((
        certificates,
        private_key.ok_or(TlsLoadError::MissingPrivateKey)?,
    ))
}

fn load_validated<F: SecureFileSystem>(
    file_system: &F,
    source: &TlsFileSource,
    clock: &dyn UnixClock,
) -> Result<ValidatedCertificate, TlsLoadError> {
    let certificate_pem = read_bounded(
        file_system,
        &source.certificate_path,
        MaterialKind::Certificate,
    )?;
    let private_key_pem = read_bounded(
        file_system,
        &source.private_key_path,
        MaterialKind::PrivateKey,
    )?;
    let (certificate_chain, private_key) = parse_material(&certificate_pem, &private_key_pem)?;
    let expected = source
        .expected_sans
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    ValidatedCertificate::new_for_server_names(
        certificate_chain,
        private_key,
        &expected,
        clock,
        source.key_policy,
    )
    .map_err(|_| TlsLoadError::CertificateRejected)
}

pub(crate) struct TlsLifecycle {
    resolver: Arc<ReloadingCertifiedKey>,
    #[cfg(feature = "wss-compat")]
    acceptor: TlsAcceptor,
    source: TlsFileSource,
    clock: Arc<dyn UnixClock>,
    expiring_emitted: AtomicBool,
    expired_emitted: AtomicBool,
}

impl TlsLifecycle {
    pub(crate) fn load(source: TlsFileSource) -> Result<Self, TlsLoadError> {
        Self::load_with(&SystemSecureFileSystem, source, Arc::new(SystemUnixClock))
    }

    fn load_with<F: SecureFileSystem>(
        file_system: &F,
        source: TlsFileSource,
        clock: Arc<dyn UnixClock>,
    ) -> Result<Self, TlsLoadError> {
        let validated = load_validated(file_system, &source, clock.as_ref())?;
        let resolver = Arc::new(
            ReloadingCertifiedKey::new(validated, Arc::clone(&clock))
                .map_err(|_| TlsLoadError::ResolverRejected)?,
        );
        #[cfg(feature = "wss-compat")]
        let acceptor = {
            let resolver_for_server: Arc<dyn ResolvesServerCert> = resolver.clone();
            TlsAcceptor::from(Arc::new(
                source
                    .posture
                    .server_config(resolver_for_server)
                    .map_err(|_| TlsLoadError::PostureRejected)?,
            ))
        };
        Ok(Self {
            resolver,
            #[cfg(feature = "wss-compat")]
            acceptor,
            source,
            clock,
            expiring_emitted: AtomicBool::new(false),
            expired_emitted: AtomicBool::new(false),
        })
    }

    #[cfg(feature = "wss-compat")]
    pub(crate) fn acceptor(&self) -> TlsAcceptor {
        self.acceptor.clone()
    }

    /// Builds a `rustls::ServerConfig` suitable for use with a Quinn QUIC
    /// endpoint. The config uses the same cert resolver, posture, and cipher
    /// suite policy as the shared direct listener, but without client-cert auth
    /// (QUIC-level peer auth goes through the Arcen handshake protocol).
    ///
    /// The caller must override `alpn_protocols` before converting to a
    /// `quinn::ServerConfig`.
    ///
    /// # Errors
    ///
    /// Returns a [`TlsLoadError`] when the resolver or posture is unavailable.
    pub(crate) fn rustls_server_config_for_quic(
        &self,
    ) -> Result<rustls::ServerConfig, TlsLoadError> {
        let resolver: Arc<dyn ResolvesServerCert> = self.resolver.clone();
        self.source
            .posture
            .server_config(resolver)
            .map_err(|_| TlsLoadError::PostureRejected)
    }

    pub(crate) fn metadata(&self) -> Result<CertificateMetadata, TlsLoadError> {
        self.resolver
            .metadata()
            .map_err(|_| TlsLoadError::ResolverRejected)
    }

    /// Stable direct-session host identity derived only from the active TLS SPKI.
    pub(crate) fn host_identity(&self) -> Result<arcen_identity::HostIdentity, TlsLoadError> {
        let metadata = self.metadata()?;
        arcen_identity::HostIdentity::new(format!(
            "spki-sha256:{}",
            hex_digest(&metadata.spki_sha256)
        ))
        .map_err(|_| TlsLoadError::ResolverRejected)
    }

    pub(crate) fn emit_startup(&self, emitter: &crate::LifecycleEmitter) {
        let Ok(metadata) = self.metadata() else {
            return;
        };
        let (kind, days_remaining) =
            startup_certificate_event(&metadata, self.source.time_policy, self.clock.as_ref());
        self.expiring_emitted.store(
            kind == arcen_telemetry::LifecycleEventKind::TlsCertificateExpiring,
            Ordering::Release,
        );
        emit_certificate(emitter, kind, &metadata, days_remaining);
    }

    pub(crate) fn reload(&self, emitter: &crate::LifecycleEmitter) -> Result<(), TlsLoadError> {
        self.reload_with(&SystemSecureFileSystem, emitter)
    }

    fn reload_with<F: SecureFileSystem>(
        &self,
        file_system: &F,
        emitter: &crate::LifecycleEmitter,
    ) -> Result<(), TlsLoadError> {
        let replacement = load_validated(file_system, &self.source, self.clock.as_ref());
        match replacement.and_then(|replacement| {
            self.resolver
                .reload_validated(replacement)
                .map_err(|_| TlsLoadError::ResolverRejected)
        }) {
            Ok(metadata) => {
                self.expiring_emitted.store(false, Ordering::Release);
                self.expired_emitted.store(false, Ordering::Release);
                emit_certificate(
                    emitter,
                    arcen_telemetry::LifecycleEventKind::TlsCertificateReloaded,
                    &metadata,
                    None,
                );
                self.check_and_emit_status(emitter);
                Ok(())
            }
            Err(error) => {
                emit_reload_failed(emitter, error.reason_class());
                Err(error)
            }
        }
    }

    pub(crate) fn check_and_emit_status(&self, emitter: &crate::LifecycleEmitter) {
        let Ok(metadata) = self.metadata() else {
            return;
        };
        let Ok(now) = self.clock.now_epoch_secs() else {
            return;
        };
        let expired = now > metadata.not_after_epoch_secs;
        let expiring = !expired
            && metadata.not_after_epoch_secs.saturating_sub(now)
                <= self.source.time_policy.warning_window_secs;
        if expiring && !self.expiring_emitted.swap(true, Ordering::AcqRel) {
            emit_certificate(
                emitter,
                arcen_telemetry::LifecycleEventKind::TlsCertificateExpiring,
                &metadata,
                Some(
                    self.source
                        .time_policy
                        .days_remaining(&metadata, self.clock.as_ref())
                        .unwrap_or(0),
                ),
            );
        }
        if expired && !self.expired_emitted.swap(true, Ordering::AcqRel) {
            emit_certificate(
                emitter,
                arcen_telemetry::LifecycleEventKind::TlsCertificateExpired,
                &metadata,
                None,
            );
        }
    }
}

fn startup_certificate_event(
    metadata: &CertificateMetadata,
    policy: CertificateTimePolicy,
    clock: &dyn UnixClock,
) -> (arcen_telemetry::LifecycleEventKind, Option<u64>) {
    if policy.is_expiring(metadata, clock).unwrap_or(false) {
        (
            arcen_telemetry::LifecycleEventKind::TlsCertificateExpiring,
            Some(policy.days_remaining(metadata, clock).unwrap_or(0)),
        )
    } else {
        (
            arcen_telemetry::LifecycleEventKind::TlsCertificateActive,
            None,
        )
    }
}

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

fn hex_digest(digest: &[u8; 32]) -> String {
    let mut value = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(value, "{byte:02x}");
    }
    value
}

fn certificate_fields(
    metadata: &CertificateMetadata,
    days_remaining: Option<u64>,
) -> arcen_telemetry::StructuredFields {
    let mut fields = arcen_telemetry::StructuredFields::default();
    let _ = fields.insert(
        "component",
        arcen_telemetry::FieldValue::String(COMPONENT.to_string()),
    );
    let _ = fields.insert(
        "source",
        arcen_telemetry::FieldValue::String(SOURCE.to_string()),
    );
    let _ = fields.insert(
        "cert_sha256",
        arcen_telemetry::FieldValue::String(hex_digest(&metadata.cert_sha256)),
    );
    let _ = fields.insert(
        "spki_sha256",
        arcen_telemetry::FieldValue::String(hex_digest(&metadata.spki_sha256)),
    );
    let _ = fields.insert(
        "key_algorithm",
        arcen_telemetry::FieldValue::String(metadata.key_algorithm.as_str().to_string()),
    );
    let _ = fields.insert(
        "key_bits",
        arcen_telemetry::FieldValue::Integer(i64::from(metadata.key_bits)),
    );
    let _ = fields.insert(
        "not_after_epoch_secs",
        arcen_telemetry::FieldValue::Integer(
            i64::try_from(metadata.not_after_epoch_secs).unwrap_or(i64::MAX),
        ),
    );
    if let Some(days) = days_remaining {
        let _ = fields.insert(
            "days_remaining",
            arcen_telemetry::FieldValue::Integer(i64::try_from(days).unwrap_or(i64::MAX)),
        );
    }
    fields
}

fn emit_certificate(
    emitter: &crate::LifecycleEmitter,
    kind: arcen_telemetry::LifecycleEventKind,
    metadata: &CertificateMetadata,
    days_remaining: Option<u64>,
) {
    crate::emit_lifecycle_event(
        emitter,
        kind,
        crate::eventlog::random_correlation_id(),
        certificate_fields(metadata, days_remaining),
    );
}

fn emit_reload_failed(emitter: &crate::LifecycleEmitter, reason_class: &'static str) {
    let mut fields = arcen_telemetry::StructuredFields::default();
    let _ = fields.insert(
        "component",
        arcen_telemetry::FieldValue::String(COMPONENT.to_string()),
    );
    let _ = fields.insert(
        "source",
        arcen_telemetry::FieldValue::String(SOURCE.to_string()),
    );
    let _ = fields.insert(
        "reason_class",
        arcen_telemetry::FieldValue::String(reason_class.to_string()),
    );
    crate::emit_lifecycle_event(
        emitter,
        arcen_telemetry::LifecycleEventKind::TlsCertificateReloadFailed,
        crate::eventlog::random_correlation_id(),
        fields,
    );
}

struct SystemSecureFileSystem;

pub fn init_crypto() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

#[cfg(target_os = "linux")]
impl SecureFileSystem for SystemSecureFileSystem {
    type Handle = std::fs::File;

    fn open(&self, path: &Path, kind: MaterialKind) -> Result<Self::Handle, TlsLoadError> {
        linux_open_without_symlinks(path, kind)
    }

    fn snapshot(
        &self,
        handle: &Self::Handle,
        kind: MaterialKind,
    ) -> Result<FileSnapshot, TlsLoadError> {
        use std::os::fd::AsRawFd;

        let status = nix::sys::stat::fstat(handle.as_raw_fd())
            .map_err(|_| TlsLoadError::Read(kind.label()))?;
        Ok(FileSnapshot {
            device: status.st_dev,
            inode: status.st_ino,
            length: u64::try_from(status.st_size).map_err(|_| TlsLoadError::Read(kind.label()))?,
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

    fn effective_uid(&self) -> u32 {
        nix::unistd::geteuid().as_raw()
    }
}

#[cfg(target_os = "linux")]
fn linux_open_without_symlinks(
    path: &Path,
    kind: MaterialKind,
) -> Result<std::fs::File, TlsLoadError> {
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
                return Err(TlsLoadError::UnsafeTraversal(kind.label()));
            }
        }
    }
    if parts.is_empty() {
        return Err(TlsLoadError::Open(kind.label()));
    }

    let mut directory = std::fs::File::open(if path.is_absolute() { "/" } else { "." })
        .map_err(|_| TlsLoadError::Open(kind.label()))?;
    for part in &parts[..parts.len() - 1] {
        let descriptor = openat(
            Some(directory.as_raw_fd()),
            part.as_os_str(),
            OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| TlsLoadError::Symlink(kind.label()))?;
        // SAFETY: `openat` returned a new owned descriptor.
        directory = unsafe { std::fs::File::from_raw_fd(descriptor) };
    }
    let descriptor = openat(
        Some(directory.as_raw_fd()),
        parts.last().expect("non-empty path").as_os_str(),
        OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| {
        if error == nix::errno::Errno::ENOENT {
            TlsLoadError::Missing(kind.label())
        } else if error == nix::errno::Errno::ELOOP {
            TlsLoadError::Symlink(kind.label())
        } else {
            TlsLoadError::Open(kind.label())
        }
    })?;
    // SAFETY: `openat` returned a new owned descriptor.
    Ok(unsafe { std::fs::File::from_raw_fd(descriptor) })
}

#[cfg(not(target_os = "linux"))]
impl SecureFileSystem for SystemSecureFileSystem {
    type Handle = std::fs::File;

    fn open(&self, path: &Path, kind: MaterialKind) -> Result<Self::Handle, TlsLoadError> {
        if path
            .components()
            .any(|component| matches!(component, Component::ParentDir))
        {
            return Err(TlsLoadError::UnsafeTraversal(kind.label()));
        }
        let metadata =
            std::fs::symlink_metadata(path).map_err(|error| map_open_error(error, kind))?;
        if metadata.file_type().is_symlink() {
            return Err(TlsLoadError::Symlink(kind.label()));
        }
        std::fs::File::open(path).map_err(|error| map_open_error(error, kind))
    }

    fn snapshot(
        &self,
        handle: &Self::Handle,
        kind: MaterialKind,
    ) -> Result<FileSnapshot, TlsLoadError> {
        let metadata = handle
            .metadata()
            .map_err(|_| TlsLoadError::Read(kind.label()))?;
        let modified = metadata
            .modified()
            .ok()
            .and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok())
            .unwrap_or_default();
        Ok(FileSnapshot {
            device: 0,
            inode: 0,
            length: metadata.len(),
            modified_seconds: modified.as_secs() as i64,
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

    fn effective_uid(&self) -> u32 {
        0
    }
}

#[cfg(not(target_os = "linux"))]
fn map_open_error(error: std::io::Error, kind: MaterialKind) -> TlsLoadError {
    if error.kind() == std::io::ErrorKind::NotFound {
        TlsLoadError::Missing(kind.label())
    } else {
        TlsLoadError::Open(kind.label())
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::collections::HashMap;
    use std::sync::atomic::AtomicU64;

    use arcen_transport::tls::{ClockError, RingCipherSuite, TlsVersionFloor};
    use rcgen::{CertificateParams, ExtendedKeyUsagePurpose, KeyPair};

    use super::*;

    const NOW: u64 = 1_800_000_000;
    const SERVICE_UID: u32 = 1001;

    #[derive(Clone)]
    struct FakeSpec {
        bytes: Vec<u8>,
        snapshot: FileSnapshot,
        changed: bool,
    }

    struct FakeHandle {
        cursor: Cursor<Vec<u8>>,
        before: FileSnapshot,
        after: FileSnapshot,
        snapshots: Cell<u8>,
    }

    impl Read for FakeHandle {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            self.cursor.read(buffer)
        }
    }

    #[derive(Default)]
    struct FakeFileSystem {
        files: HashMap<PathBuf, FakeSpec>,
    }

    impl FakeFileSystem {
        fn add(&mut self, path: &str, bytes: Vec<u8>, kind: MaterialKind) {
            self.files.insert(
                PathBuf::from(path),
                FakeSpec {
                    snapshot: FileSnapshot {
                        device: 1,
                        inode: self.files.len() as u64 + 1,
                        length: bytes.len() as u64,
                        modified_seconds: 1,
                        modified_nanoseconds: 0,
                        changed_seconds: 1,
                        changed_nanoseconds: 0,
                        mode: match kind {
                            MaterialKind::Certificate => 0o644,
                            MaterialKind::PrivateKey => 0o600,
                        },
                        owner: SERVICE_UID,
                        regular: true,
                        symlink: false,
                    },
                    bytes,
                    changed: false,
                },
            );
        }
    }

    impl SecureFileSystem for FakeFileSystem {
        type Handle = FakeHandle;

        fn open(&self, path: &Path, kind: MaterialKind) -> Result<Self::Handle, TlsLoadError> {
            let spec = self
                .files
                .get(path)
                .ok_or(TlsLoadError::Missing(kind.label()))?;
            let mut after = spec.snapshot.clone();
            if spec.changed {
                after.changed_nanoseconds += 1;
            }
            Ok(FakeHandle {
                cursor: Cursor::new(spec.bytes.clone()),
                before: spec.snapshot.clone(),
                after,
                snapshots: Cell::new(0),
            })
        }

        fn snapshot(
            &self,
            handle: &Self::Handle,
            _kind: MaterialKind,
        ) -> Result<FileSnapshot, TlsLoadError> {
            let count = handle.snapshots.get();
            handle.snapshots.set(count + 1);
            Ok(if count == 0 {
                handle.before.clone()
            } else {
                handle.after.clone()
            })
        }

        fn effective_uid(&self) -> u32 {
            SERVICE_UID
        }
    }

    #[derive(Debug)]
    struct TestClock(AtomicU64);

    impl TestClock {
        fn new(now: u64) -> Self {
            Self(AtomicU64::new(now))
        }
    }

    impl UnixClock for TestClock {
        fn now_epoch_secs(&self) -> Result<u64, ClockError> {
            Ok(self.0.load(Ordering::SeqCst))
        }
    }

    fn material(name: &str) -> (Vec<u8>, Vec<u8>) {
        let mut params = CertificateParams::new(vec![name.to_string()]).expect("params");
        params.not_before = rcgen::date_time_ymd(2020, 1, 1);
        params.not_after = rcgen::date_time_ymd(2040, 1, 1);
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let key = KeyPair::generate().expect("key");
        let certificate = params.self_signed(&key).expect("certificate");
        (
            certificate.pem().into_bytes(),
            key.serialize_pem().into_bytes(),
        )
    }

    fn source(expected_sans: Vec<String>) -> TlsFileSource {
        TlsFileSource {
            certificate_path: PathBuf::from("host.crt"),
            private_key_path: PathBuf::from("host.key"),
            expected_sans,
            posture: TlsPosture::new(
                TlsVersionFloor::default(),
                std::iter::empty::<RingCipherSuite>(),
            )
            .expect("posture"),
            key_policy: CertificateKeyPolicy::default(),
            time_policy: CertificateTimePolicy::default(),
        }
    }

    fn good_file_system() -> FakeFileSystem {
        let (certificate, key) = material("host.example");
        let mut file_system = FakeFileSystem::default();
        file_system.add("host.crt", certificate, MaterialKind::Certificate);
        file_system.add("host.key", key, MaterialKind::PrivateKey);
        file_system
    }

    #[test]
    fn fake_openat_seam_rejects_missing_partial_size_and_changed_material() {
        let file_system = FakeFileSystem::default();
        assert_eq!(
            read_bounded(
                &file_system,
                Path::new("missing.crt"),
                MaterialKind::Certificate
            ),
            Err(TlsLoadError::Missing("certificate"))
        );

        let mut partial = FakeFileSystem::default();
        partial.add(
            "host.crt",
            b"certificate".to_vec(),
            MaterialKind::Certificate,
        );
        assert_eq!(
            read_bounded(&partial, Path::new("host.key"), MaterialKind::PrivateKey),
            Err(TlsLoadError::Missing("private key"))
        );

        let mut oversized = FakeFileSystem::default();
        oversized.add(
            "host.key",
            vec![0; MAX_PRIVATE_KEY_PEM_BYTES as usize + 1],
            MaterialKind::PrivateKey,
        );
        assert_eq!(
            read_bounded(&oversized, Path::new("host.key"), MaterialKind::PrivateKey),
            Err(TlsLoadError::Oversized(
                "private key",
                MAX_PRIVATE_KEY_PEM_BYTES
            ))
        );

        let mut changed = good_file_system();
        changed
            .files
            .get_mut(Path::new("host.key"))
            .expect("key")
            .changed = true;
        assert_eq!(
            read_bounded(&changed, Path::new("host.key"), MaterialKind::PrivateKey),
            Err(TlsLoadError::ChangedDuringRead("private key"))
        );
    }

    #[test]
    fn fake_openat_seam_rejects_symlink_nonregular_owner_and_modes() {
        for (mut snapshot, expected) in [
            (
                FileSnapshot {
                    symlink: true,
                    ..good_file_system()
                        .files
                        .get(Path::new("host.key"))
                        .expect("key")
                        .snapshot
                        .clone()
                },
                TlsLoadError::Symlink("private key"),
            ),
            (
                FileSnapshot {
                    regular: false,
                    ..good_file_system()
                        .files
                        .get(Path::new("host.key"))
                        .expect("key")
                        .snapshot
                        .clone()
                },
                TlsLoadError::NotRegular("private key"),
            ),
            (
                FileSnapshot {
                    owner: 2222,
                    ..good_file_system()
                        .files
                        .get(Path::new("host.key"))
                        .expect("key")
                        .snapshot
                        .clone()
                },
                TlsLoadError::UnexpectedOwner("private key"),
            ),
            (
                FileSnapshot {
                    mode: 0o640,
                    ..good_file_system()
                        .files
                        .get(Path::new("host.key"))
                        .expect("key")
                        .snapshot
                        .clone()
                },
                TlsLoadError::InsecureMode("private key"),
            ),
        ] {
            snapshot.length = 1;
            let mut file_system = FakeFileSystem::default();
            file_system.files.insert(
                PathBuf::from("host.key"),
                FakeSpec {
                    bytes: vec![1],
                    snapshot,
                    changed: false,
                },
            );
            assert_eq!(
                read_bounded(
                    &file_system,
                    Path::new("host.key"),
                    MaterialKind::PrivateKey
                ),
                Err(expected)
            );
        }

        let mut certificate = good_file_system();
        certificate
            .files
            .get_mut(Path::new("host.crt"))
            .expect("certificate")
            .snapshot
            .mode = 0o664;
        assert_eq!(
            read_bounded(
                &certificate,
                Path::new("host.crt"),
                MaterialKind::Certificate
            ),
            Err(TlsLoadError::InsecureMode("certificate"))
        );
    }

    #[test]
    fn exact_one_key_matching_pair_and_every_expected_san_are_required() {
        let (certificate, first_key) = material("host.example");
        let (_, second_key) = material("host.example");
        assert_eq!(
            parse_material(&certificate, &[first_key.clone(), second_key].concat()).map(|_| ()),
            Err(TlsLoadError::MultiplePrivateKeys)
        );
        assert_eq!(
            parse_material(&certificate, b"").map(|_| ()),
            Err(TlsLoadError::MissingPrivateKey)
        );

        let mut mismatch = FakeFileSystem::default();
        mismatch.add("host.crt", certificate, MaterialKind::Certificate);
        mismatch.add(
            "host.key",
            material("other.example").1,
            MaterialKind::PrivateKey,
        );
        assert_eq!(
            load_validated(
                &mismatch,
                &source(vec!["host.example".to_string()]),
                &TestClock::new(NOW)
            )
            .map(|_| ()),
            Err(TlsLoadError::CertificateRejected)
        );
        assert_eq!(
            load_validated(
                &good_file_system(),
                &source(vec![
                    "host.example".to_string(),
                    "missing.example".to_string()
                ]),
                &TestClock::new(NOW)
            )
            .map(|_| ()),
            Err(TlsLoadError::CertificateRejected)
        );
    }

    #[test]
    fn lifecycle_metadata_is_bounded_and_private() {
        let validated = load_validated(
            &good_file_system(),
            &source(vec!["host.example".to_string()]),
            &TestClock::new(NOW),
        )
        .expect("validated");
        let rendered = format!("{:?}", certificate_fields(validated.metadata(), Some(30)));
        assert!(rendered.contains("cert_sha256"));
        assert!(rendered.contains("spki_sha256"));
        assert!(!rendered.contains("host.example"));
        assert!(!rendered.contains("host.crt"));
        assert!(!rendered.contains("issuer"));
        assert!(!rendered.contains("subject"));

        let (active, days) = startup_certificate_event(
            validated.metadata(),
            CertificateTimePolicy::default(),
            &TestClock::new(NOW),
        );
        assert_eq!(
            active,
            arcen_telemetry::LifecycleEventKind::TlsCertificateActive
        );
        assert_eq!(days, None);
        let (expiring, days) = startup_certificate_event(
            validated.metadata(),
            CertificateTimePolicy {
                warning_window_secs: u64::MAX,
            },
            &TestClock::new(NOW),
        );
        assert_eq!(
            expiring,
            arcen_telemetry::LifecycleEventKind::TlsCertificateExpiring
        );
        assert!(days.is_some());
    }

    #[test]
    fn reload_failure_keeps_last_good_and_expiry_refuses_new_handshakes() {
        let file_system = good_file_system();
        let clock = Arc::new(TestClock::new(NOW));
        let lifecycle = TlsLifecycle::load_with(
            &file_system,
            source(vec!["host.example".to_string()]),
            clock.clone(),
        )
        .expect("lifecycle");
        assert_eq!(
            lifecycle.host_identity().unwrap().as_str(),
            format!(
                "spki-sha256:{}",
                hex_digest(&lifecycle.metadata().unwrap().spki_sha256)
            )
        );
        let last_good = lifecycle.metadata().expect("metadata").cert_sha256;

        let (_, wrong_key) = material("other.example");
        let mut mismatch = good_file_system();
        mismatch
            .files
            .get_mut(Path::new("host.key"))
            .expect("key")
            .bytes = wrong_key;
        assert!(load_validated(
            &mismatch,
            &source(vec!["host.example".to_string()]),
            clock.as_ref()
        )
        .is_err());
        assert_eq!(
            lifecycle.metadata().expect("metadata").cert_sha256,
            last_good
        );
        clock.0.store(3_000_000_000, Ordering::SeqCst);
        assert!(lifecycle.resolver.resolve_current().is_none());
    }

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
}
