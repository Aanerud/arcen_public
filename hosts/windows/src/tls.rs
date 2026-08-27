//! Windows Pier TLS material loading, validation, reload, and lifecycle events.

use std::fmt::{Display, Formatter};
use std::io::{BufReader, Cursor, Read};
#[cfg(windows)]
use std::path::Prefix;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use arcen_transport::tls::{
    CertificateKeyPolicy, CertificateMetadata, CertificateTimePolicy, ReloadingCertifiedKey,
    SystemUnixClock, TlsPosture, UnixClock, ValidatedCertificate,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
#[cfg(feature = "wss-compat")]
use tokio_rustls::TlsAcceptor;
use zeroize::Zeroizing;

pub(crate) const MAX_CERTIFICATE_PEM_BYTES: u64 = 1024 * 1024;
pub(crate) const MAX_PRIVATE_KEY_PEM_BYTES: u64 = 128 * 1024;
const COMPONENT: &str = "pier_broker";
const SOURCE: &str = "pem";

#[derive(Debug, Clone)]
pub(crate) struct TlsFileSource {
    pub(crate) certificate_path: PathBuf,
    pub(crate) private_key_path: PathBuf,
    pub(crate) expected_sans: Vec<String>,
    pub(crate) posture: TlsPosture,
    pub(crate) key_policy: CertificateKeyPolicy,
    pub(crate) time_policy: CertificateTimePolicy,
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
    NotRegular(&'static str),
    ReparsePoint(&'static str),
    NonDiskDevice(&'static str),
    Oversized(&'static str, u64),
    Read(&'static str),
    ChangedDuringRead(&'static str),
    BroadPrivateKeyAcl,
    AclQueryFailed,
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
            Self::NotRegular(_) | Self::NonDiskDevice(_) => "unsafe_file_type",
            Self::ReparsePoint(_) => "reparse_point",
            Self::Oversized(_, _) => "material_oversized",
            Self::ChangedDuringRead(_) => "material_changed",
            Self::BroadPrivateKeyAcl => "private_key_acl",
            Self::AclQueryFailed => "acl_query_failed",
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
            Self::NotRegular(kind) => write!(formatter, "TLS {kind} is not a regular file"),
            Self::ReparsePoint(kind) => {
                write!(formatter, "TLS {kind} path contains a reparse point")
            }
            Self::NonDiskDevice(kind) => write!(formatter, "TLS {kind} is not a disk file"),
            Self::Oversized(kind, maximum) => {
                write!(formatter, "TLS {kind} exceeds the {maximum}-byte limit")
            }
            Self::Read(kind) => write!(formatter, "TLS {kind} could not be read"),
            Self::ChangedDuringRead(kind) => {
                write!(formatter, "TLS {kind} changed while it was read")
            }
            Self::BroadPrivateKeyAcl => formatter.write_str(
                "TLS private key DACL must be protected and contain only SYSTEM and Administrators full-control entries",
            ),
            Self::AclQueryFailed => {
                formatter.write_str("TLS private key DACL could not be validated")
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
    identity: u128,
    length: u64,
    modified: u64,
    regular: bool,
    reparse: bool,
    disk: bool,
}

trait SecureFileSystem {
    type Handle: Read;

    fn open(&self, path: &Path, kind: MaterialKind) -> Result<Self::Handle, TlsLoadError>;
    fn snapshot(
        &self,
        handle: &Self::Handle,
        kind: MaterialKind,
    ) -> Result<FileSnapshot, TlsLoadError>;
    fn private_key_acl_restricted(&self, handle: &Self::Handle) -> Result<bool, TlsLoadError>;
}

fn read_bounded<F: SecureFileSystem>(
    file_system: &F,
    path: &Path,
    kind: MaterialKind,
) -> Result<Vec<u8>, TlsLoadError> {
    let mut handle = file_system.open(path, kind)?;
    let before = file_system.snapshot(&handle, kind)?;
    if !before.regular {
        return Err(TlsLoadError::NotRegular(kind.label()));
    }
    if before.reparse {
        return Err(TlsLoadError::ReparsePoint(kind.label()));
    }
    if !before.disk {
        return Err(TlsLoadError::NonDiskDevice(kind.label()));
    }
    if before.length > kind.maximum() {
        return Err(TlsLoadError::Oversized(kind.label(), kind.maximum()));
    }
    if kind == MaterialKind::PrivateKey && !file_system.private_key_acl_restricted(&handle)? {
        return Err(TlsLoadError::BroadPrivateKeyAcl);
    }

    let mut bytes = Vec::with_capacity(before.length as usize);
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
    let private_key_pem = Zeroizing::new(read_bounded(
        file_system,
        &source.private_key_path,
        MaterialKind::PrivateKey,
    )?);
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
        let acceptor = TlsAcceptor::from(Arc::new(
            source
                .posture
                .server_config(resolver.clone())
                .map_err(|_| TlsLoadError::PostureRejected)?,
        ));
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

    /// Builds a `rustls::ServerConfig` for use with a Quinn QUIC endpoint,
    /// using the same cert resolver and cipher-suite posture as the direct
    /// `TlsAcceptor`. The caller must set `alpn_protocols` before converting.
    pub(crate) fn rustls_server_config_for_quic(
        &self,
    ) -> Result<rustls::ServerConfig, TlsLoadError> {
        self.source
            .posture
            .server_config(self.resolver.clone())
            .map_err(|_| TlsLoadError::PostureRejected)
    }

    pub(crate) fn metadata(&self) -> Result<CertificateMetadata, TlsLoadError> {
        self.resolver
            .metadata()
            .map_err(|_| TlsLoadError::ResolverRejected)
    }

    /// Stable direct-session Host identity derived only from the validated TLS SPKI.
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
        let expiring = self
            .source
            .time_policy
            .is_expiring(&metadata, self.clock.as_ref())
            .unwrap_or(false);
        self.expiring_emitted.store(expiring, Ordering::Release);
        emit_certificate(
            emitter,
            if expiring {
                arcen_telemetry::LifecycleEventKind::TlsCertificateExpiring
            } else {
                arcen_telemetry::LifecycleEventKind::TlsCertificateActive
            },
            &metadata,
            expiring.then(|| {
                self.source
                    .time_policy
                    .days_remaining(&metadata, self.clock.as_ref())
                    .unwrap_or(0)
            }),
        );
    }

    pub(crate) fn reload(&self, emitter: &crate::LifecycleEmitter) -> Result<(), TlsLoadError> {
        let replacement =
            load_validated(&SystemSecureFileSystem, &self.source, self.clock.as_ref());
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

/// Produces a fresh non-secret correlation id for a standalone lifecycle event.
/// On Windows delegates to `eventlog`; on other platforms uses `getrandom` directly.
fn random_correlation_id() -> arcen_telemetry::CorrelationId {
    #[cfg(windows)]
    {
        crate::eventlog::random_correlation_id()
    }
    #[cfg(not(windows))]
    {
        let mut bytes = [0u8; 16];
        let _ = getrandom::getrandom(&mut bytes);
        arcen_telemetry::CorrelationId::from_uuid_v4_bytes(bytes)
    }
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
        random_correlation_id(),
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
        random_correlation_id(),
        fields,
    );
}

#[cfg(windows)]
struct SystemSecureFileSystem;

#[cfg(windows)]
impl SecureFileSystem for SystemSecureFileSystem {
    type Handle = std::fs::File;

    fn open(&self, path: &Path, kind: MaterialKind) -> Result<Self::Handle, TlsLoadError> {
        use std::os::windows::fs::OpenOptionsExt;
        use windows::Win32::Storage::FileSystem::{
            FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ,
        };

        let absolute = absolute_without_traversal(path, kind)?;
        let components = absolute.components().collect::<Vec<_>>();
        let mut current = PathBuf::new();
        let mut parent_handles = Vec::new();
        for (index, component) in components.iter().enumerate() {
            current.push(component.as_os_str());
            if index + 1 == components.len() || !matches!(component, Component::Normal(_)) {
                continue;
            }
            let directory = std::fs::OpenOptions::new()
                .read(true)
                .share_mode(FILE_SHARE_READ.0)
                .custom_flags((FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT).0)
                .open(&current)
                .map_err(|error| map_open_error(error, kind))?;
            let snapshot = windows_snapshot(&directory, kind)?;
            if snapshot.reparse {
                return Err(TlsLoadError::ReparsePoint(kind.label()));
            }
            if snapshot.regular || !snapshot.disk {
                return Err(TlsLoadError::NotRegular(kind.label()));
            }
            // Keeping every checked directory open without FILE_SHARE_DELETE
            // prevents an attacker from replacing an ancestor before the leaf opens.
            parent_handles.push(directory);
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ.0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
            .open(absolute)
            .map_err(|error| map_open_error(error, kind))?;
        drop(parent_handles);
        Ok(file)
    }

    fn snapshot(
        &self,
        handle: &Self::Handle,
        kind: MaterialKind,
    ) -> Result<FileSnapshot, TlsLoadError> {
        windows_snapshot(handle, kind)
    }

    fn private_key_acl_restricted(&self, handle: &Self::Handle) -> Result<bool, TlsLoadError> {
        validate_restricted_key_acl(handle)
    }
}

#[cfg(not(windows))]
struct SystemSecureFileSystem;

#[cfg(not(windows))]
impl SecureFileSystem for SystemSecureFileSystem {
    type Handle = std::fs::File;

    fn open(&self, path: &Path, kind: MaterialKind) -> Result<Self::Handle, TlsLoadError> {
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
            .map_or(0, |value| value.as_nanos() as u64);
        Ok(FileSnapshot {
            identity: 0,
            length: metadata.len(),
            modified,
            regular: metadata.is_file(),
            reparse: false,
            disk: true,
        })
    }

    fn private_key_acl_restricted(&self, _handle: &Self::Handle) -> Result<bool, TlsLoadError> {
        Err(TlsLoadError::AclQueryFailed)
    }
}

fn map_open_error(error: std::io::Error, kind: MaterialKind) -> TlsLoadError {
    if error.kind() == std::io::ErrorKind::NotFound {
        TlsLoadError::Missing(kind.label())
    } else {
        TlsLoadError::Open(kind.label())
    }
}

#[cfg(windows)]
fn absolute_without_traversal(path: &Path, kind: MaterialKind) -> Result<PathBuf, TlsLoadError> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|_| TlsLoadError::Open(kind.label()))?
            .join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(prefix) if matches!(prefix.kind(), Prefix::Disk(_)) => {
                normalized.push(component.as_os_str());
            }
            Component::Prefix(_) => {
                return Err(TlsLoadError::NonDiskDevice(kind.label()));
            }
            Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(TlsLoadError::ReparsePoint(kind.label()));
                }
            }
        }
    }
    Ok(normalized)
}

#[cfg(windows)]
fn windows_snapshot(
    file: &std::fs::File,
    kind: MaterialKind,
) -> Result<FileSnapshot, TlsLoadError> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, GetFileType, BY_HANDLE_FILE_INFORMATION,
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_TYPE_DISK,
    };

    let handle = HANDLE(file.as_raw_handle());
    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: `handle` is borrowed from a live `File`; `information` is valid
    // writable storage of the exact structure expected by the API.
    unsafe { GetFileInformationByHandle(handle, &raw mut information) }
        .map_err(|_| TlsLoadError::Read(kind.label()))?;
    // SAFETY: `handle` remains live and GetFileType does not take ownership.
    let disk = unsafe { GetFileType(handle) } == FILE_TYPE_DISK;
    let identity = (u128::from(information.dwVolumeSerialNumber) << 64)
        | (u128::from(information.nFileIndexHigh) << 32)
        | u128::from(information.nFileIndexLow);
    let length = (u64::from(information.nFileSizeHigh) << 32) | u64::from(information.nFileSizeLow);
    let modified = (u64::from(information.ftLastWriteTime.dwHighDateTime) << 32)
        | u64::from(information.ftLastWriteTime.dwLowDateTime);
    Ok(FileSnapshot {
        identity,
        length,
        modified,
        regular: information.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 == 0,
        reparse: information.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0,
        disk,
    })
}

#[cfg(windows)]
fn validate_restricted_key_acl(file: &std::fs::File) -> Result<bool, TlsLoadError> {
    use std::mem::{offset_of, size_of};
    use std::os::windows::io::AsRawHandle;
    use std::ptr::{addr_of, null_mut};
    use windows::Win32::Foundation::{LocalFree, HANDLE, HLOCAL};
    use windows::Win32::Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT};
    use windows::Win32::Security::{
        CreateWellKnownSid, EqualSid, GetAce, GetSecurityDescriptorControl, IsValidSid,
        WinBuiltinAdministratorsSid, WinLocalSystemSid, ACCESS_ALLOWED_ACE, ACE_HEADER,
        DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
        SECURITY_MAX_SID_SIZE, SE_DACL_PROTECTED,
    };
    use windows::Win32::Storage::FileSystem::FILE_ALL_ACCESS;

    struct Descriptor(PSECURITY_DESCRIPTOR);
    impl Drop for Descriptor {
        fn drop(&mut self) {
            if !self.0 .0.is_null() {
                // SAFETY: GetSecurityInfo allocated this descriptor with LocalAlloc.
                let _ = unsafe { LocalFree(HLOCAL(self.0 .0)) };
            }
        }
    }

    let handle = HANDLE(file.as_raw_handle());
    let mut dacl = null_mut();
    let mut owner = PSID::default();
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    // SAFETY: all requested out-pointers refer to live writable storage and the
    // borrowed file handle remains valid throughout the query.
    let status = unsafe {
        GetSecurityInfo(
            handle,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            Some(&raw mut owner),
            None,
            Some(&raw mut dacl),
            None,
            Some(&raw mut descriptor),
        )
    };
    if status.0 != 0 || descriptor.0.is_null() {
        return Err(TlsLoadError::AclQueryFailed);
    }
    let descriptor = Descriptor(descriptor);
    let mut control = 0_u16;
    let mut revision = 0_u32;
    // SAFETY: the descriptor remains live and both output pointers are valid.
    unsafe { GetSecurityDescriptorControl(descriptor.0, &raw mut control, &raw mut revision) }
        .map_err(|_| TlsLoadError::AclQueryFailed)?;
    if control & SE_DACL_PROTECTED.0 == 0 || dacl.is_null() {
        return Ok(false);
    }
    // SAFETY: `dacl` aliases the live descriptor.
    let header = unsafe { &*dacl };
    if header.AceCount != 2 || usize::from(header.AclSize) < size_of_val(header) {
        return Ok(false);
    }

    fn known_sid(
        kind: windows::Win32::Security::WELL_KNOWN_SID_TYPE,
    ) -> Result<Vec<u8>, TlsLoadError> {
        let mut bytes = vec![0_u8; SECURITY_MAX_SID_SIZE as usize];
        let mut length = bytes.len() as u32;
        // SAFETY: the buffer and size pointer are valid and large enough for a
        // well-known SID; no domain SID is needed for these two identities.
        unsafe { CreateWellKnownSid(kind, None, PSID(bytes.as_mut_ptr().cast()), &raw mut length) }
            .map_err(|_| TlsLoadError::AclQueryFailed)?;
        bytes.truncate(length as usize);
        Ok(bytes)
    }

    let system = known_sid(WinLocalSystemSid)?;
    let administrators = known_sid(WinBuiltinAdministratorsSid)?;
    if owner.0.is_null() {
        return Ok(false);
    }
    let system_sid = PSID(system.as_ptr().cast_mut().cast());
    let admin_sid = PSID(administrators.as_ptr().cast_mut().cast());
    // SAFETY: the owner aliases the live descriptor, while both known SID
    // buffers remain live and were produced by CreateWellKnownSid.
    if !unsafe { IsValidSid(owner) }.as_bool()
        || !(unsafe { EqualSid(owner, system_sid) }.is_ok()
            || unsafe { EqualSid(owner, admin_sid) }.is_ok())
    {
        return Ok(false);
    }
    let mut saw_system = false;
    let mut saw_administrators = false;
    for index in 0..2 {
        let mut raw_ace = null_mut();
        // SAFETY: AceCount proves both queried indexes are in range.
        unsafe { GetAce(dacl, index, &raw mut raw_ace) }
            .map_err(|_| TlsLoadError::AclQueryFailed)?;
        if raw_ace.is_null() {
            return Ok(false);
        }
        // SAFETY: GetAce returns a pointer to an ACE beginning with ACE_HEADER.
        let ace_header = unsafe { &*raw_ace.cast::<ACE_HEADER>() };
        if ace_header.AceType != 0
            || usize::from(ace_header.AceSize) < size_of::<ACCESS_ALLOWED_ACE>()
        {
            return Ok(false);
        }
        // SAFETY: the checked type and size establish ACCESS_ALLOWED_ACE layout.
        let ace = unsafe { &*raw_ace.cast::<ACCESS_ALLOWED_ACE>() };
        if ace.Mask != FILE_ALL_ACCESS.0 {
            return Ok(false);
        }
        let sid_available = usize::from(ace_header.AceSize)
            .saturating_sub(offset_of!(ACCESS_ALLOWED_ACE, SidStart));
        let sid = PSID(addr_of!(ace.SidStart).cast_mut().cast());
        if sid_available < 8 {
            return Ok(false);
        }
        // SAFETY: the minimum SID header fits inside the checked ACE.
        if !unsafe { IsValidSid(sid) }.as_bool() {
            return Ok(false);
        }
        // SAFETY: all three validated SID buffers remain live for comparison.
        if unsafe { EqualSid(sid, system_sid) }.is_ok() {
            saw_system = true;
        } else if unsafe { EqualSid(sid, admin_sid) }.is_ok() {
            saw_administrators = true;
        } else {
            return Ok(false);
        }
    }
    Ok(saw_system && saw_administrators)
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

    #[derive(Clone)]
    struct FakeSpec {
        bytes: Vec<u8>,
        snapshot: FileSnapshot,
        changed: bool,
        restricted_acl: bool,
    }

    struct FakeHandle {
        cursor: Cursor<Vec<u8>>,
        before: FileSnapshot,
        after: FileSnapshot,
        snapshots: Cell<u8>,
        restricted_acl: bool,
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
        fn add(&mut self, path: &str, bytes: Vec<u8>, restricted_acl: bool) {
            self.files.insert(
                PathBuf::from(path),
                FakeSpec {
                    snapshot: FileSnapshot {
                        identity: self.files.len() as u128 + 1,
                        length: bytes.len() as u64,
                        modified: 1,
                        regular: true,
                        reparse: false,
                        disk: true,
                    },
                    bytes,
                    changed: false,
                    restricted_acl,
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
                after.modified += 1;
            }
            Ok(FakeHandle {
                cursor: Cursor::new(spec.bytes.clone()),
                before: spec.snapshot.clone(),
                after,
                snapshots: Cell::new(0),
                restricted_acl: spec.restricted_acl,
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

        fn private_key_acl_restricted(&self, handle: &Self::Handle) -> Result<bool, TlsLoadError> {
            Ok(handle.restricted_acl)
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
        let mut fs = FakeFileSystem::default();
        fs.add("host.crt", certificate, true);
        fs.add("host.key", key, true);
        fs
    }

    #[test]
    fn fake_secure_reader_rejects_missing_partial_and_oversized_material() {
        let fs = FakeFileSystem::default();
        assert_eq!(
            read_bounded(&fs, Path::new("missing.crt"), MaterialKind::Certificate),
            Err(TlsLoadError::Missing("certificate"))
        );

        let mut partial = FakeFileSystem::default();
        partial.add("host.crt", b"certificate".to_vec(), true);
        assert_eq!(
            read_bounded(&partial, Path::new("host.key"), MaterialKind::PrivateKey),
            Err(TlsLoadError::Missing("private key"))
        );

        let mut oversized = FakeFileSystem::default();
        oversized.add(
            "host.key",
            vec![0; MAX_PRIVATE_KEY_PEM_BYTES as usize + 1],
            true,
        );
        assert_eq!(
            read_bounded(&oversized, Path::new("host.key"), MaterialKind::PrivateKey),
            Err(TlsLoadError::Oversized(
                "private key",
                MAX_PRIVATE_KEY_PEM_BYTES
            ))
        );
    }

    #[test]
    fn fake_secure_reader_rejects_directory_reparse_device_acl_and_change() {
        for (mut snapshot, expected) in [
            (
                FileSnapshot {
                    identity: 1,
                    length: 0,
                    modified: 1,
                    regular: false,
                    reparse: false,
                    disk: true,
                },
                TlsLoadError::NotRegular("private key"),
            ),
            (
                FileSnapshot {
                    identity: 1,
                    length: 0,
                    modified: 1,
                    regular: true,
                    reparse: true,
                    disk: true,
                },
                TlsLoadError::ReparsePoint("private key"),
            ),
            (
                FileSnapshot {
                    identity: 1,
                    length: 0,
                    modified: 1,
                    regular: true,
                    reparse: false,
                    disk: false,
                },
                TlsLoadError::NonDiskDevice("private key"),
            ),
        ] {
            snapshot.length = 1;
            let mut fs = FakeFileSystem::default();
            fs.files.insert(
                PathBuf::from("host.key"),
                FakeSpec {
                    bytes: vec![1],
                    snapshot,
                    changed: false,
                    restricted_acl: true,
                },
            );
            assert_eq!(
                read_bounded(&fs, Path::new("host.key"), MaterialKind::PrivateKey),
                Err(expected)
            );
        }

        let mut broad = FakeFileSystem::default();
        broad.add("host.key", vec![1], false);
        assert_eq!(
            read_bounded(&broad, Path::new("host.key"), MaterialKind::PrivateKey),
            Err(TlsLoadError::BroadPrivateKeyAcl)
        );

        let mut changed = FakeFileSystem::default();
        changed.add("host.key", vec![1], true);
        changed
            .files
            .get_mut(Path::new("host.key"))
            .expect("file")
            .changed = true;
        assert_eq!(
            read_bounded(&changed, Path::new("host.key"), MaterialKind::PrivateKey),
            Err(TlsLoadError::ChangedDuringRead("private key"))
        );
    }

    #[cfg(windows)]
    #[test]
    fn relative_parent_components_are_lexically_normalized() {
        let current = std::env::current_dir().expect("current directory");
        let expected = current.join("tls").join("host.crt");
        let normalized = absolute_without_traversal(
            Path::new(r"fixtures\..\tls\host.crt"),
            MaterialKind::Certificate,
        )
        .expect("ordinary parent component");
        assert_eq!(normalized, expected);
    }

    #[cfg(windows)]
    #[test]
    fn unc_and_device_namespace_paths_are_rejected() {
        for path in [
            Path::new(r"\\server\share\host.crt"),
            Path::new(r"\\?\C:\tls\host.crt"),
            Path::new(r"\\.\C:\tls\host.crt"),
        ] {
            assert_eq!(
                absolute_without_traversal(path, MaterialKind::Certificate),
                Err(TlsLoadError::NonDiskDevice("certificate"))
            );
        }
    }

    #[test]
    fn exact_one_key_and_matching_pair_are_required() {
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

        let mut fs = FakeFileSystem::default();
        fs.add("host.crt", certificate, true);
        fs.add("host.key", material("other.example").1, true);
        assert_eq!(
            load_validated(
                &fs,
                &source(vec!["host.example".to_string()]),
                &TestClock::new(NOW)
            )
            .map(|_| ()),
            Err(TlsLoadError::CertificateRejected)
        );
    }

    #[test]
    fn every_expected_san_is_required_and_lifecycle_metadata_is_private() {
        let fs = good_file_system();
        let validated = load_validated(
            &fs,
            &source(vec!["host.example".to_string()]),
            &TestClock::new(NOW),
        )
        .expect("validated");
        let fields = certificate_fields(validated.metadata(), None);
        let rendered = format!("{fields:?}");
        assert!(rendered.contains("cert_sha256"));
        assert!(rendered.contains("spki_sha256"));
        assert!(!rendered.contains("host.example"));
        assert!(!rendered.contains("host.crt"));

        assert_eq!(
            load_validated(
                &fs,
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
    fn reload_success_failure_and_expiry_keep_fail_closed_state() {
        let fs = good_file_system();
        let clock = Arc::new(TestClock::new(NOW));
        let lifecycle =
            TlsLifecycle::load_with(&fs, source(vec!["host.example".to_string()]), clock.clone())
                .expect("lifecycle");
        assert!(lifecycle.resolver.resolve_current().is_some());

        let replacement = load_validated(
            &fs,
            &source(vec!["host.example".to_string()]),
            clock.as_ref(),
        )
        .expect("replacement");
        lifecycle
            .resolver
            .reload_validated(replacement)
            .expect("reload");
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
}
