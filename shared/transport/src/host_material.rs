// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! A Pier's TLS material at run time: reading the certificate and key from
//! disk, validating them, serving them, reloading them, and the lifecycle
//! events that describe them.
//!
//! Every Pier reads the same two PEM files under the same rules. Only the
//! file-system checks differ, because the operating system decides what a
//! safe file is:
//!
//! - Unix: an owner and a mode ([`UnixFileFacts`]). The key must be `0600`
//!   and the certificate at most `0644`, owned by root or the serving
//!   account. Whether a path may pass through a symbolic link is the
//!   platform's choice of open (Linux walks every component with
//!   `O_NOFOLLOW`; macOS refuses only a linked leaf, because its own system
//!   paths such as `/var` and `/tmp` are links into `/private`).
//! - Windows: a file identity, reparse points and a disk handle
//!   ([`WindowsFileFacts`]); the key's protected DACL is checked by the
//!   platform through [`SecureFileSystem::admit`].
//!
//! A platform implements [`SecureFileSystem`] (open, describe, admit) and
//! nothing else; reading, size bounds, change detection, PEM parsing,
//! validation, pinning, reload and expiry events are here.

use std::fmt::{Debug, Display, Formatter};
use std::io::{BufReader, Cursor, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use rustls::pki_types::pem::SectionKind;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::ResolvesServerCert;
use zeroize::Zeroizing;

use crate::tls::{
    CertificateKeyPolicy, CertificateMetadata, CertificateTimePolicy, ReloadingCertifiedKey,
    TlsPosture, UnixClock, ValidatedCertificate,
};

/// Largest certificate-chain PEM a Pier reads.
pub const MAX_CERTIFICATE_PEM_BYTES: u64 = 1024 * 1024;
/// Largest private-key PEM a Pier reads.
pub const MAX_PRIVATE_KEY_PEM_BYTES: u64 = 128 * 1024;
/// The `source` field of every certificate lifecycle event.
const SOURCE: &str = "pem";

/// Which of the two files is being read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaterialKind {
    /// The certificate chain.
    Certificate,
    /// The private key.
    PrivateKey,
}

impl MaterialKind {
    /// The name used in errors and logs.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Certificate => "certificate",
            Self::PrivateKey => "private key",
        }
    }

    /// The largest file of this kind a Pier reads.
    #[must_use]
    pub const fn maximum(self) -> u64 {
        match self {
            Self::Certificate => MAX_CERTIFICATE_PEM_BYTES,
            Self::PrivateKey => MAX_PRIVATE_KEY_PEM_BYTES,
        }
    }
}

/// Where a Pier's TLS material is and what it must satisfy.
#[derive(Debug, Clone)]
pub struct HostTlsSource {
    /// Certificate-chain PEM.
    pub certificate_path: PathBuf,
    /// Private-key PEM.
    pub private_key_path: PathBuf,
    /// Names the certificate must carry, each exactly (empty: no constraint).
    pub expected_sans: Vec<String>,
    /// Protocol and cipher-suite posture.
    pub posture: TlsPosture,
    /// Admitted key algorithms and sizes.
    pub key_policy: CertificateKeyPolicy,
    /// Expiry warning window.
    pub time_policy: CertificateTimePolicy,
}

/// Why TLS material was not loaded.
///
/// Every variant has a stable [`reason_class`](Self::reason_class) for
/// lifecycle events; the text never contains a path or a name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostTlsError {
    /// The file does not exist.
    Missing(&'static str),
    /// The file exists but could not be opened safely.
    Open(&'static str),
    /// The path contains `..` or another traversal the platform refuses.
    UnsafeTraversal(&'static str),
    /// The path is, or passes through, a symbolic link.
    Symlink(&'static str),
    /// The path contains a Windows reparse point.
    ReparsePoint(&'static str),
    /// The file is not a regular file.
    NotRegular(&'static str),
    /// The handle is not a disk file (a device or a pipe).
    NonDiskDevice(&'static str),
    /// The file is owned by an account other than root or the serving account.
    UnexpectedOwner(&'static str),
    /// The file's Unix mode lets someone else read or change it.
    InsecureMode(&'static str),
    /// The file is larger than [`MaterialKind::maximum`].
    Oversized(&'static str, u64),
    /// The file could not be read.
    Read(&'static str),
    /// The file changed while it was being read.
    ChangedDuringRead(&'static str),
    /// The private key's DACL grants more than SYSTEM and Administrators.
    BroadPrivateKeyAcl,
    /// The private key's DACL could not be read.
    AclQueryFailed,
    /// The certificate PEM holds a block that is not a certificate.
    MalformedCertificatePem,
    /// The certificate PEM holds no certificate.
    EmptyCertificateChain,
    /// The key PEM holds a block that is not a private key.
    MalformedPrivateKeyPem,
    /// The key PEM holds no private key.
    MissingPrivateKey,
    /// The key PEM holds more than one private key.
    MultiplePrivateKeys,
    /// The certificate failed validity, name, key-policy or key-match checks.
    CertificateRejected,
    /// The resolver refused the certificate.
    ResolverRejected,
    /// The protocol posture could not build a server configuration.
    PostureRejected,
}

impl HostTlsError {
    /// Stable class for lifecycle events.
    #[must_use]
    pub const fn reason_class(&self) -> &'static str {
        match self {
            Self::Missing(_) => "material_missing",
            Self::Open(_) | Self::Read(_) => "material_unavailable",
            Self::UnsafeTraversal(_) | Self::Symlink(_) => "unsafe_path",
            Self::ReparsePoint(_) => "reparse_point",
            Self::NotRegular(_) | Self::NonDiskDevice(_) => "unsafe_file_type",
            Self::UnexpectedOwner(_) => "unexpected_owner",
            Self::InsecureMode(_) => "insecure_mode",
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

impl Display for HostTlsError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing(kind) => write!(formatter, "TLS {kind} file does not exist"),
            Self::Open(kind) => write!(formatter, "TLS {kind} file could not be opened securely"),
            Self::UnsafeTraversal(kind) => {
                write!(formatter, "TLS {kind} path contains unsafe traversal")
            }
            Self::Symlink(kind) => write!(formatter, "TLS {kind} path contains a symbolic link"),
            Self::ReparsePoint(kind) => {
                write!(formatter, "TLS {kind} path contains a reparse point")
            }
            Self::NotRegular(kind) => write!(formatter, "TLS {kind} is not a regular file"),
            Self::NonDiskDevice(kind) => write!(formatter, "TLS {kind} is not a disk file"),
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

impl std::error::Error for HostTlsError {}

/// What a platform reports about an open file, compared before and after
/// reading to catch a file that changed underneath the read.
pub trait MaterialSnapshot: PartialEq + Debug {
    /// The file's length in bytes.
    fn length(&self) -> u64;
}

/// The platform's safe open, its description of an open file, and its
/// admission checks. This is the only part of TLS loading that differs
/// between Piers.
pub trait SecureFileSystem {
    /// An open file.
    type Handle: Read;
    /// What the platform reports about an open file.
    type Snapshot: MaterialSnapshot;

    /// Opens `path` without following links the platform refuses.
    ///
    /// # Errors
    ///
    /// Returns the reason the path may not be opened.
    fn open(&self, path: &Path, kind: MaterialKind) -> Result<Self::Handle, HostTlsError>;

    /// Describes the open file.
    ///
    /// # Errors
    ///
    /// Returns [`HostTlsError::Read`] when the platform cannot describe it.
    fn snapshot(
        &self,
        handle: &Self::Handle,
        kind: MaterialKind,
    ) -> Result<Self::Snapshot, HostTlsError>;

    /// Refuses a file the platform considers unsafe to trust.
    ///
    /// # Errors
    ///
    /// Returns the reason the file is refused.
    fn admit(
        &self,
        handle: &Self::Handle,
        snapshot: &Self::Snapshot,
        kind: MaterialKind,
    ) -> Result<(), HostTlsError>;
}

/// An open file as a Unix `fstat` reports it.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnixFileFacts {
    /// `st_dev`.
    pub device: u64,
    /// `st_ino`.
    pub inode: u64,
    /// `st_size`.
    pub length: u64,
    /// `st_mtime`.
    pub modified_seconds: i64,
    /// `st_mtime_nsec`.
    pub modified_nanoseconds: i64,
    /// `st_ctime`.
    pub changed_seconds: i64,
    /// `st_ctime_nsec`.
    pub changed_nanoseconds: i64,
    /// `st_mode`.
    pub mode: u32,
    /// `st_uid`.
    pub owner: u32,
    /// `S_ISREG`.
    pub regular: bool,
    /// The handle is a link (only possible when the platform opened one).
    pub symlink: bool,
}

impl MaterialSnapshot for UnixFileFacts {
    fn length(&self) -> u64 {
        self.length
    }
}

impl UnixFileFacts {
    /// The shared Unix rule: a regular, unlinked file owned by root or the
    /// serving account; the key `0600`, the certificate readable by its owner
    /// and writable by nobody else.
    ///
    /// # Errors
    ///
    /// Returns the first rule the file breaks.
    pub fn admit(&self, kind: MaterialKind, effective_uid: u32) -> Result<(), HostTlsError> {
        if self.symlink {
            return Err(HostTlsError::Symlink(kind.label()));
        }
        if !self.regular {
            return Err(HostTlsError::NotRegular(kind.label()));
        }
        if self.owner != 0 && self.owner != effective_uid {
            return Err(HostTlsError::UnexpectedOwner(kind.label()));
        }
        let mode = self.mode & 0o7777;
        let secure = match kind {
            MaterialKind::PrivateKey => mode == 0o600,
            MaterialKind::Certificate => mode & !0o644 == 0 && mode & 0o400 != 0,
        };
        if !secure {
            return Err(HostTlsError::InsecureMode(kind.label()));
        }
        Ok(())
    }
}

/// An open file as `GetFileInformationByHandle` and `GetFileType` report it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowsFileFacts {
    /// Volume serial number and file index.
    pub identity: u128,
    /// File size.
    pub length: u64,
    /// Last write time.
    pub modified: u64,
    /// Not a directory.
    pub regular: bool,
    /// `FILE_ATTRIBUTE_REPARSE_POINT`.
    pub reparse: bool,
    /// `FILE_TYPE_DISK`.
    pub disk: bool,
}

impl MaterialSnapshot for WindowsFileFacts {
    fn length(&self) -> u64 {
        self.length
    }
}

impl WindowsFileFacts {
    /// The shared Windows rule for the file itself: a regular disk file that
    /// is not a reparse point. The key's DACL is checked by the platform.
    ///
    /// # Errors
    ///
    /// Returns the first rule the file breaks.
    pub const fn admit(&self, kind: MaterialKind) -> Result<(), HostTlsError> {
        if !self.regular {
            return Err(HostTlsError::NotRegular(kind.label()));
        }
        if self.reparse {
            return Err(HostTlsError::ReparsePoint(kind.label()));
        }
        if !self.disk {
            return Err(HostTlsError::NonDiskDevice(kind.label()));
        }
        Ok(())
    }
}

/// Reads one material file within its size bound, refusing a file the
/// platform does not admit or one that changes while it is read.
///
/// # Errors
///
/// Returns the platform's refusal, [`HostTlsError::Oversized`],
/// [`HostTlsError::Read`] or [`HostTlsError::ChangedDuringRead`].
pub fn read_bounded<F: SecureFileSystem>(
    file_system: &F,
    path: &Path,
    kind: MaterialKind,
) -> Result<Zeroizing<Vec<u8>>, HostTlsError> {
    let mut handle = file_system.open(path, kind)?;
    let before = file_system.snapshot(&handle, kind)?;
    file_system.admit(&handle, &before, kind)?;
    if before.length() > kind.maximum() {
        return Err(HostTlsError::Oversized(kind.label(), kind.maximum()));
    }

    let capacity =
        usize::try_from(before.length()).map_err(|_| HostTlsError::Read(kind.label()))?;
    let mut bytes = Zeroizing::new(Vec::with_capacity(capacity));
    (&mut handle)
        .take(kind.maximum() + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| HostTlsError::Read(kind.label()))?;
    if bytes.len() as u64 > kind.maximum() {
        return Err(HostTlsError::Oversized(kind.label(), kind.maximum()));
    }
    let after = file_system.snapshot(&handle, kind)?;
    if before != after || bytes.len() as u64 != before.length() {
        return Err(HostTlsError::ChangedDuringRead(kind.label()));
    }
    Ok(bytes)
}

/// One PEM section, as the PEM reader classifies it.
enum PemItem {
    Certificate(CertificateDer<'static>),
    Key(PrivateKeyDer<'static>),
    Other,
}

/// Reads every recognised PEM section, skipping text between them and the
/// sections no TLS file carries (the same reading `rustls-pemfile` gives).
fn pem_items(pem: &[u8]) -> impl Iterator<Item = Result<PemItem, ()>> + '_ {
    let mut reader = BufReader::new(Cursor::new(pem));
    std::iter::from_fn(move || {
        loop {
            return match rustls::pki_types::pem::from_buf(&mut reader) {
                Ok(None) => None,
                Err(_) => Some(Err(())),
                Ok(Some((kind, data))) => Some(Ok(match kind {
                    SectionKind::Certificate => PemItem::Certificate(data.into()),
                    SectionKind::RsaPrivateKey => PemItem::Key(PrivateKeyDer::Pkcs1(data.into())),
                    SectionKind::PrivateKey => PemItem::Key(PrivateKeyDer::Pkcs8(data.into())),
                    SectionKind::EcPrivateKey => PemItem::Key(PrivateKeyDer::Sec1(data.into())),
                    SectionKind::PublicKey | SectionKind::Crl | SectionKind::Csr => PemItem::Other,
                    _ => continue,
                })),
            };
        }
    })
}

/// Parses a certificate chain and exactly one private key.
///
/// # Errors
///
/// Returns the PEM failure: an unexpected block, no certificate, no key or
/// more than one key.
pub fn parse_material(
    certificate_pem: &[u8],
    private_key_pem: &[u8],
) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>), HostTlsError> {
    let mut certificates = Vec::new();
    for item in pem_items(certificate_pem) {
        match item.map_err(|()| HostTlsError::MalformedCertificatePem)? {
            PemItem::Certificate(certificate) => certificates.push(certificate),
            PemItem::Key(_) | PemItem::Other => {
                return Err(HostTlsError::MalformedCertificatePem);
            }
        }
    }
    if certificates.is_empty() {
        return Err(HostTlsError::EmptyCertificateChain);
    }
    Ok((certificates, parse_private_key(private_key_pem)?))
}

/// Parses exactly one private key: PKCS #8, SEC1 (`openssl ecparam`) or
/// PKCS #1.
///
/// # Errors
///
/// Returns the PEM failure: an unexpected block, no key or more than one key.
pub fn parse_private_key(private_key_pem: &[u8]) -> Result<PrivateKeyDer<'static>, HostTlsError> {
    let mut private_key = None;
    for item in pem_items(private_key_pem) {
        let key = match item.map_err(|()| HostTlsError::MalformedPrivateKeyPem)? {
            PemItem::Key(key) => key,
            PemItem::Certificate(_) | PemItem::Other => {
                return Err(HostTlsError::MalformedPrivateKeyPem);
            }
        };
        if private_key.replace(key).is_some() {
            return Err(HostTlsError::MultiplePrivateKeys);
        }
    }
    private_key.ok_or(HostTlsError::MissingPrivateKey)
}

/// Reads, parses and validates the material `source` names.
///
/// # Errors
///
/// Returns the first file, PEM or validation failure.
pub fn load_validated<F: SecureFileSystem>(
    file_system: &F,
    source: &HostTlsSource,
    clock: &dyn UnixClock,
) -> Result<ValidatedCertificate, HostTlsError> {
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
    .map_err(|_| HostTlsError::CertificateRejected)
}

/// A lifecycle event for the Pier to emit with its own emitter.
#[derive(Debug, Clone, PartialEq)]
pub struct CertificateEvent {
    /// The event.
    pub kind: arcen_telemetry::LifecycleEventKind,
    /// Its bounded, non-secret fields.
    pub fields: arcen_telemetry::StructuredFields,
}

/// What a reload did, and what to tell the operator about it.
#[derive(Debug, Clone, PartialEq)]
pub struct ReloadOutcome {
    /// `Ok` when the replacement is now served; the last good material keeps
    /// serving otherwise.
    pub result: Result<(), HostTlsError>,
    /// Events to emit, in order.
    pub events: Vec<CertificateEvent>,
}

/// The material a Pier serves: validated, reloadable, and refused for new
/// handshakes once it expires.
pub struct HostTlsMaterial {
    resolver: Arc<ReloadingCertifiedKey>,
    source: HostTlsSource,
    clock: Arc<dyn UnixClock>,
    component: &'static str,
    expiring_emitted: AtomicBool,
    expired_emitted: AtomicBool,
}

impl Debug for HostTlsMaterial {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HostTlsMaterial")
            .field("component", &self.component)
            .finish_non_exhaustive()
    }
}

impl HostTlsMaterial {
    /// Loads and validates the material; `component` names the serving
    /// process in lifecycle events.
    ///
    /// # Errors
    ///
    /// Returns the first file, PEM, validation or posture failure.
    pub fn load<F: SecureFileSystem>(
        file_system: &F,
        source: HostTlsSource,
        clock: Arc<dyn UnixClock>,
        component: &'static str,
    ) -> Result<Self, HostTlsError> {
        let validated = load_validated(file_system, &source, clock.as_ref())?;
        let resolver = Arc::new(
            ReloadingCertifiedKey::new(validated, Arc::clone(&clock))
                .map_err(|_| HostTlsError::ResolverRejected)?,
        );
        let material = Self {
            resolver,
            source,
            clock,
            component,
            expiring_emitted: AtomicBool::new(false),
            expired_emitted: AtomicBool::new(false),
        };
        // A posture that cannot build a server is refused at load, not at the
        // first connection.
        material.server_config()?;
        Ok(material)
    }

    /// The resolver every handshake asks for the current certificate.
    #[must_use]
    pub fn resolver(&self) -> Arc<ReloadingCertifiedKey> {
        Arc::clone(&self.resolver)
    }

    /// A rustls server configuration over the reloading resolver, with the
    /// source's posture and no client authentication. The caller sets ALPN.
    ///
    /// # Errors
    ///
    /// Returns [`HostTlsError::PostureRejected`] when the posture is unusable.
    pub fn server_config(&self) -> Result<rustls::ServerConfig, HostTlsError> {
        let resolver: Arc<dyn ResolvesServerCert> = self.resolver.clone();
        self.source
            .posture
            .server_config(resolver)
            .map_err(|_| HostTlsError::PostureRejected)
    }

    /// Non-secret metadata of the certificate being served.
    ///
    /// # Errors
    ///
    /// Returns [`HostTlsError::ResolverRejected`] when the resolver is poisoned.
    pub fn metadata(&self) -> Result<CertificateMetadata, HostTlsError> {
        self.resolver
            .metadata()
            .map_err(|_| HostTlsError::ResolverRejected)
    }

    /// The Pier's direct-session identity: `spki-sha256:<hex>` of the served
    /// key, so a renewal over the same key keeps it.
    ///
    /// # Errors
    ///
    /// Returns [`HostTlsError::ResolverRejected`] when no identity can be formed.
    pub fn host_identity(&self) -> Result<arcen_identity::HostIdentity, HostTlsError> {
        let metadata = self.metadata()?;
        host_identity(&metadata).map_err(|_| HostTlsError::ResolverRejected)
    }

    /// The event describing the material at start-up: active, or expiring
    /// with the days left.
    #[must_use]
    pub fn startup_event(&self) -> Option<CertificateEvent> {
        let metadata = self.metadata().ok()?;
        let policy = self.source.time_policy;
        let expiring = policy
            .is_expiring(&metadata, self.clock.as_ref())
            .unwrap_or(false);
        self.expiring_emitted.store(expiring, Ordering::Release);
        Some(if expiring {
            certificate_event(
                self.component,
                arcen_telemetry::LifecycleEventKind::TlsCertificateExpiring,
                &metadata,
                Some(
                    policy
                        .days_remaining(&metadata, self.clock.as_ref())
                        .unwrap_or(0),
                ),
            )
        } else {
            certificate_event(
                self.component,
                arcen_telemetry::LifecycleEventKind::TlsCertificateActive,
                &metadata,
                None,
            )
        })
    }

    /// Reads the files again and serves them when they pass every check;
    /// otherwise keeps serving the last good material.
    pub fn reload<F: SecureFileSystem>(&self, file_system: &F) -> ReloadOutcome {
        let replacement = load_validated(file_system, &self.source, self.clock.as_ref());
        match replacement.and_then(|replacement| {
            self.resolver
                .reload_validated(replacement)
                .map_err(|_| HostTlsError::ResolverRejected)
        }) {
            Ok(metadata) => {
                self.expiring_emitted.store(false, Ordering::Release);
                self.expired_emitted.store(false, Ordering::Release);
                let mut events = vec![certificate_event(
                    self.component,
                    arcen_telemetry::LifecycleEventKind::TlsCertificateReloaded,
                    &metadata,
                    None,
                )];
                events.extend(self.status_events());
                ReloadOutcome {
                    result: Ok(()),
                    events,
                }
            }
            Err(error) => ReloadOutcome {
                events: vec![reload_failed_event(self.component, error.reason_class())],
                result: Err(error),
            },
        }
    }

    /// The expiring and expired events, each once per certificate.
    #[must_use]
    pub fn status_events(&self) -> Vec<CertificateEvent> {
        let mut events = Vec::new();
        let Ok(metadata) = self.metadata() else {
            return events;
        };
        let Ok(now) = self.clock.now_epoch_secs() else {
            return events;
        };
        let expired = now > metadata.not_after_epoch_secs;
        let expiring = !expired
            && metadata.not_after_epoch_secs.saturating_sub(now)
                <= self.source.time_policy.warning_window_secs;
        if expiring && !self.expiring_emitted.swap(true, Ordering::AcqRel) {
            events.push(certificate_event(
                self.component,
                arcen_telemetry::LifecycleEventKind::TlsCertificateExpiring,
                &metadata,
                Some(
                    self.source
                        .time_policy
                        .days_remaining(&metadata, self.clock.as_ref())
                        .unwrap_or(0),
                ),
            ));
        }
        if expired && !self.expired_emitted.swap(true, Ordering::AcqRel) {
            events.push(certificate_event(
                self.component,
                arcen_telemetry::LifecycleEventKind::TlsCertificateExpired,
                &metadata,
                None,
            ));
        }
        events
    }
}

/// The direct-session identity for `metadata`: `spki-sha256:<hex>`.
///
/// # Errors
///
/// Returns the identity crate's refusal.
pub fn host_identity(
    metadata: &CertificateMetadata,
) -> Result<arcen_identity::HostIdentity, arcen_identity::IdentityValidationError> {
    arcen_identity::HostIdentity::new(format!(
        "spki-sha256:{}",
        crate::cert_marker::hex_lower(&metadata.spki_sha256)
    ))
}

/// The fields of a certificate lifecycle event. Digests, algorithm, size and
/// expiry only: never a name, a path, an issuer or a subject.
#[must_use]
pub fn certificate_fields(
    component: &'static str,
    metadata: &CertificateMetadata,
    days_remaining: Option<u64>,
) -> arcen_telemetry::StructuredFields {
    use arcen_telemetry::FieldValue;

    let mut fields = arcen_telemetry::StructuredFields::default();
    let _ = fields.insert("component", FieldValue::String(component.to_owned()));
    let _ = fields.insert("source", FieldValue::String(SOURCE.to_owned()));
    let _ = fields.insert(
        "cert_sha256",
        FieldValue::String(crate::cert_marker::hex_lower(&metadata.cert_sha256)),
    );
    let _ = fields.insert(
        "spki_sha256",
        FieldValue::String(crate::cert_marker::hex_lower(&metadata.spki_sha256)),
    );
    let _ = fields.insert(
        "key_algorithm",
        FieldValue::String(metadata.key_algorithm.as_str().to_owned()),
    );
    let _ = fields.insert(
        "key_bits",
        FieldValue::Integer(i64::from(metadata.key_bits)),
    );
    let _ = fields.insert(
        "not_after_epoch_secs",
        FieldValue::Integer(i64::try_from(metadata.not_after_epoch_secs).unwrap_or(i64::MAX)),
    );
    if let Some(days) = days_remaining {
        let _ = fields.insert(
            "days_remaining",
            FieldValue::Integer(i64::try_from(days).unwrap_or(i64::MAX)),
        );
    }
    fields
}

fn certificate_event(
    component: &'static str,
    kind: arcen_telemetry::LifecycleEventKind,
    metadata: &CertificateMetadata,
    days_remaining: Option<u64>,
) -> CertificateEvent {
    CertificateEvent {
        kind,
        fields: certificate_fields(component, metadata, days_remaining),
    }
}

fn reload_failed_event(component: &'static str, reason_class: &'static str) -> CertificateEvent {
    use arcen_telemetry::FieldValue;

    let mut fields = arcen_telemetry::StructuredFields::default();
    let _ = fields.insert("component", FieldValue::String(component.to_owned()));
    let _ = fields.insert("source", FieldValue::String(SOURCE.to_owned()));
    let _ = fields.insert("reason_class", FieldValue::String(reason_class.to_owned()));
    CertificateEvent {
        kind: arcen_telemetry::LifecycleEventKind::TlsCertificateReloadFailed,
        fields,
    }
}

#[cfg(test)]
mod tests;
