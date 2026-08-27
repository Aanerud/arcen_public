//! Shared rustls posture, certificate validation, pinning, and reload contracts.

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt::{Debug, Display, Formatter};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::str::FromStr;
use std::sync::{Arc, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use rustls::{CipherSuite, ServerConfig, SupportedCipherSuite};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use x509_parser::extensions::GeneralName;
use x509_parser::parse_x509_certificate;

const SECONDS_PER_DAY: u64 = 86_400;
const DEFAULT_WARNING_DAYS: u64 = 30;
const RSA_OID: &str = "1.2.840.113549.1.1.1";
const EC_PUBLIC_KEY_OID: &str = "1.2.840.10045.2.1";
const P256_OID: &str = "1.2.840.10045.3.1.7";
const P384_OID: &str = "1.3.132.0.34";
const ED25519_OID: &str = "1.3.101.112";
#[cfg(feature = "wss-compat")]
static TLS12_AND_TLS13: [&rustls::SupportedProtocolVersion; 2] =
    [&rustls::version::TLS13, &rustls::version::TLS12];
static TLS13_ONLY: [&rustls::SupportedProtocolVersion; 1] = [&rustls::version::TLS13];

/// Lowest TLS protocol version admitted by the shared posture.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TlsVersionFloor {
    /// TLS 1.2 and TLS 1.3.
    #[cfg(feature = "wss-compat")]
    Tls12,
    /// TLS 1.3 only.
    #[default]
    Tls13,
}

impl Display for TlsVersionFloor {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            #[cfg(feature = "wss-compat")]
            Self::Tls12 => "TLS1.2",
            Self::Tls13 => "TLS1.3",
        })
    }
}

impl FromStr for TlsVersionFloor {
    type Err = TlsError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            #[cfg(feature = "wss-compat")]
            "TLS1.2" => Ok(Self::Tls12),
            "TLS1.3" => Ok(Self::Tls13),
            _ => Err(TlsError::UnsupportedVersionFloor),
        }
    }
}

/// One cipher suite supplied by rustls's ring provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RingCipherSuite {
    /// TLS 1.3 AES-256-GCM with SHA-384.
    Tls13Aes256GcmSha384,
    /// TLS 1.3 AES-128-GCM with SHA-256.
    Tls13Aes128GcmSha256,
    /// TLS 1.3 ChaCha20-Poly1305 with SHA-256.
    Tls13Chacha20Poly1305Sha256,
    /// TLS 1.2 ECDHE-ECDSA AES-256-GCM with SHA-384.
    #[cfg(feature = "wss-compat")]
    TlsEcdheEcdsaWithAes256GcmSha384,
    /// TLS 1.2 ECDHE-ECDSA AES-128-GCM with SHA-256.
    #[cfg(feature = "wss-compat")]
    TlsEcdheEcdsaWithAes128GcmSha256,
    /// TLS 1.2 ECDHE-ECDSA ChaCha20-Poly1305 with SHA-256.
    #[cfg(feature = "wss-compat")]
    TlsEcdheEcdsaWithChacha20Poly1305Sha256,
    /// TLS 1.2 ECDHE-RSA AES-256-GCM with SHA-384.
    #[cfg(feature = "wss-compat")]
    TlsEcdheRsaWithAes256GcmSha384,
    /// TLS 1.2 ECDHE-RSA AES-128-GCM with SHA-256.
    #[cfg(feature = "wss-compat")]
    TlsEcdheRsaWithAes128GcmSha256,
    /// TLS 1.2 ECDHE-RSA ChaCha20-Poly1305 with SHA-256.
    #[cfg(feature = "wss-compat")]
    TlsEcdheRsaWithChacha20Poly1305Sha256,
}

impl RingCipherSuite {
    /// All ring suites in rustls provider preference order.
    #[cfg(feature = "wss-compat")]
    pub const ALL: [Self; 9] = [
        Self::Tls13Aes256GcmSha384,
        Self::Tls13Aes128GcmSha256,
        Self::Tls13Chacha20Poly1305Sha256,
        Self::TlsEcdheEcdsaWithAes256GcmSha384,
        Self::TlsEcdheEcdsaWithAes128GcmSha256,
        Self::TlsEcdheEcdsaWithChacha20Poly1305Sha256,
        Self::TlsEcdheRsaWithAes256GcmSha384,
        Self::TlsEcdheRsaWithAes128GcmSha256,
        Self::TlsEcdheRsaWithChacha20Poly1305Sha256,
    ];
    /// All TLS 1.3 ring suites in provider preference order.
    #[cfg(not(feature = "wss-compat"))]
    pub const ALL: [Self; 3] = [
        Self::Tls13Aes256GcmSha384,
        Self::Tls13Aes128GcmSha256,
        Self::Tls13Chacha20Poly1305Sha256,
    ];

    /// Returns the stable IANA-style suite name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Tls13Aes256GcmSha384 => "TLS13_AES_256_GCM_SHA384",
            Self::Tls13Aes128GcmSha256 => "TLS13_AES_128_GCM_SHA256",
            Self::Tls13Chacha20Poly1305Sha256 => "TLS13_CHACHA20_POLY1305_SHA256",
            #[cfg(feature = "wss-compat")]
            Self::TlsEcdheEcdsaWithAes256GcmSha384 => "TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384",
            #[cfg(feature = "wss-compat")]
            Self::TlsEcdheEcdsaWithAes128GcmSha256 => "TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256",
            #[cfg(feature = "wss-compat")]
            Self::TlsEcdheEcdsaWithChacha20Poly1305Sha256 => {
                "TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256"
            }
            #[cfg(feature = "wss-compat")]
            Self::TlsEcdheRsaWithAes256GcmSha384 => "TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384",
            #[cfg(feature = "wss-compat")]
            Self::TlsEcdheRsaWithAes128GcmSha256 => "TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256",
            #[cfg(feature = "wss-compat")]
            Self::TlsEcdheRsaWithChacha20Poly1305Sha256 => {
                "TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256"
            }
        }
    }

    const fn rustls_name(self) -> CipherSuite {
        match self {
            Self::Tls13Aes256GcmSha384 => CipherSuite::TLS13_AES_256_GCM_SHA384,
            Self::Tls13Aes128GcmSha256 => CipherSuite::TLS13_AES_128_GCM_SHA256,
            Self::Tls13Chacha20Poly1305Sha256 => CipherSuite::TLS13_CHACHA20_POLY1305_SHA256,
            #[cfg(feature = "wss-compat")]
            Self::TlsEcdheEcdsaWithAes256GcmSha384 => {
                CipherSuite::TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384
            }
            #[cfg(feature = "wss-compat")]
            Self::TlsEcdheEcdsaWithAes128GcmSha256 => {
                CipherSuite::TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256
            }
            #[cfg(feature = "wss-compat")]
            Self::TlsEcdheEcdsaWithChacha20Poly1305Sha256 => {
                CipherSuite::TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256
            }
            #[cfg(feature = "wss-compat")]
            Self::TlsEcdheRsaWithAes256GcmSha384 => {
                CipherSuite::TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384
            }
            #[cfg(feature = "wss-compat")]
            Self::TlsEcdheRsaWithAes128GcmSha256 => {
                CipherSuite::TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256
            }
            #[cfg(feature = "wss-compat")]
            Self::TlsEcdheRsaWithChacha20Poly1305Sha256 => {
                CipherSuite::TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256
            }
        }
    }
}

impl Display for RingCipherSuite {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for RingCipherSuite {
    type Err = TlsError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|suite| suite.as_str() == value)
            .ok_or(TlsError::UnsupportedCipherSuite)
    }
}

/// Explicit rustls/ring protocol and cipher-suite posture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsPosture {
    version_floor: TlsVersionFloor,
    disabled_suites: BTreeSet<RingCipherSuite>,
    enabled_suites: Vec<RingCipherSuite>,
}

impl Default for TlsPosture {
    fn default() -> Self {
        Self {
            version_floor: TlsVersionFloor::default(),
            disabled_suites: BTreeSet::new(),
            enabled_suites: RingCipherSuite::ALL.to_vec(),
        }
    }
}

impl TlsPosture {
    /// Builds and checks an explicit suite blacklist.
    ///
    /// # Errors
    ///
    /// Returns an error if the blacklist leaves no suite usable at the selected
    /// protocol floor.
    pub fn new(
        version_floor: TlsVersionFloor,
        disabled_suites: impl IntoIterator<Item = RingCipherSuite>,
    ) -> Result<Self, TlsError> {
        let disabled_suites = disabled_suites.into_iter().collect::<BTreeSet<_>>();
        let enabled_suites = RingCipherSuite::ALL
            .into_iter()
            .filter(|suite| !disabled_suites.contains(suite))
            .collect::<Vec<_>>();
        let posture = Self {
            version_floor,
            disabled_suites,
            enabled_suites,
        };
        posture.checked_provider()?;
        Ok(posture)
    }

    /// Returns the configured minimum version.
    #[must_use]
    pub const fn version_floor(&self) -> TlsVersionFloor {
        self.version_floor
    }

    /// Returns the explicit disabled-suite blacklist.
    #[must_use]
    pub const fn disabled_suites(&self) -> &BTreeSet<RingCipherSuite> {
        &self.disabled_suites
    }

    /// Returns enabled suites in ring provider preference order.
    #[must_use]
    pub fn enabled_suites(&self) -> &[RingCipherSuite] {
        &self.enabled_suites
    }

    /// Builds a server configuration with no client authentication.
    ///
    /// This does not set ALPN, key logging, early data, or any client-auth
    /// behavior beyond rustls's fail-closed defaults.
    ///
    /// # Errors
    ///
    /// Returns an error if the provider and version selection is unusable.
    pub fn server_config(
        &self,
        resolver: Arc<dyn ResolvesServerCert>,
    ) -> Result<ServerConfig, TlsError> {
        let provider = self.checked_provider()?;
        let builder = ServerConfig::builder_with_provider(provider)
            .with_protocol_versions(protocol_versions(self.version_floor))
            .map_err(|_| TlsError::InvalidTlsPosture)?;
        Ok(builder.with_no_client_auth().with_cert_resolver(resolver))
    }

    fn checked_provider(&self) -> Result<Arc<CryptoProvider>, TlsError> {
        let mut provider = rustls::crypto::ring::default_provider();
        provider.cipher_suites = self
            .enabled_suites
            .iter()
            .map(|suite| supported_suite(*suite))
            .collect::<Result<Vec<_>, _>>()?;
        let provider = Arc::new(provider);
        ServerConfig::builder_with_provider(Arc::clone(&provider))
            .with_protocol_versions(protocol_versions(self.version_floor))
            .map_err(|_| TlsError::InvalidTlsPosture)?;
        Ok(provider)
    }
}

fn protocol_versions(
    floor: TlsVersionFloor,
) -> &'static [&'static rustls::SupportedProtocolVersion] {
    match floor {
        #[cfg(feature = "wss-compat")]
        TlsVersionFloor::Tls12 => &TLS12_AND_TLS13,
        TlsVersionFloor::Tls13 => &TLS13_ONLY,
    }
}

fn supported_suite(suite: RingCipherSuite) -> Result<SupportedCipherSuite, TlsError> {
    rustls::crypto::ring::ALL_CIPHER_SUITES
        .iter()
        .copied()
        .find(|candidate| candidate.suite() == suite.rustls_name())
        .ok_or(TlsError::UnsupportedCipherSuite)
}

/// Certificate-expiry warning policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CertificateTimePolicy {
    /// Warning window in whole seconds.
    pub warning_window_secs: u64,
}

impl Default for CertificateTimePolicy {
    fn default() -> Self {
        Self {
            warning_window_secs: DEFAULT_WARNING_DAYS * SECONDS_PER_DAY,
        }
    }
}

impl CertificateTimePolicy {
    /// Returns whole days remaining, rounded down.
    ///
    /// # Errors
    ///
    /// Returns an error if the clock fails or the certificate has expired.
    pub fn days_remaining(
        self,
        metadata: &CertificateMetadata,
        clock: &dyn UnixClock,
    ) -> Result<u64, TlsError> {
        let now = clock.now_epoch_secs().map_err(|_| TlsError::ClockFailed)?;
        metadata
            .not_after_epoch_secs
            .checked_sub(now)
            .map(|seconds| seconds / SECONDS_PER_DAY)
            .ok_or(TlsError::CertificateExpired)
    }

    /// Returns whether the certificate is at or inside the warning window.
    ///
    /// # Errors
    ///
    /// Returns an error if the clock fails or the certificate has expired.
    pub fn is_expiring(
        self,
        metadata: &CertificateMetadata,
        clock: &dyn UnixClock,
    ) -> Result<bool, TlsError> {
        let now = clock.now_epoch_secs().map_err(|_| TlsError::ClockFailed)?;
        let remaining = metadata
            .not_after_epoch_secs
            .checked_sub(now)
            .ok_or(TlsError::CertificateExpired)?;
        Ok(remaining <= self.warning_window_secs)
    }
}

/// Public-key admission policy for server certificates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CertificateKeyPolicy {
    minimum_rsa_bits: u16,
}

impl Default for CertificateKeyPolicy {
    fn default() -> Self {
        Self {
            minimum_rsa_bits: 2048,
        }
    }
}

impl CertificateKeyPolicy {
    /// Returns the non-weakenable minimum RSA modulus size.
    #[must_use]
    pub const fn minimum_rsa_bits(self) -> u16 {
        self.minimum_rsa_bits
    }
}

/// Supported certificate source class.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum CertificateSourcePolicy {
    /// Operator-managed PEM certificate and private-key files.
    #[default]
    PemFiles,
}

/// Supported certificate rotation model.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum RotationPolicy {
    /// The operator explicitly requests a validated reload.
    #[default]
    OperatorReload,
}

/// Type of SHA-256 certificate pin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PinKind {
    /// Hash of the complete leaf certificate DER.
    CertificateSha256,
    /// Hash of the leaf's exact `SubjectPublicKeyInfo` DER.
    SubjectPublicKeyInfoSha256,
}

/// A typed SHA-256 pin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TlsPin {
    /// Pin namespace.
    pub kind: PinKind,
    /// SHA-256 digest.
    pub digest: [u8; 32],
}

impl TlsPin {
    /// Creates a typed pin.
    #[must_use]
    pub const fn new(kind: PinKind, digest: [u8; 32]) -> Self {
        Self { kind, digest }
    }

    /// Compares equal-kind pin values in constant time.
    #[must_use]
    pub fn matches(&self, candidate: &Self) -> bool {
        self.kind == candidate.kind && bool::from(self.digest.ct_eq(&candidate.digest))
    }
}

/// Admitted leaf public-key algorithm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CertificateKeyAlgorithm {
    /// RSA with a modulus of at least the configured minimum.
    Rsa,
    /// NIST P-256 ECDSA.
    P256,
    /// NIST P-384 ECDSA.
    P384,
    /// Ed25519.
    Ed25519,
}

impl CertificateKeyAlgorithm {
    /// Returns the stable telemetry-safe algorithm label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Rsa => "rsa",
            Self::P256 => "p256",
            Self::P384 => "p384",
            Self::Ed25519 => "ed25519",
        }
    }
}

/// Non-secret metadata extracted from a validated leaf certificate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertificateMetadata {
    /// Whole-certificate SHA-256.
    pub cert_sha256: [u8; 32],
    /// Exact `SubjectPublicKeyInfo` DER SHA-256.
    pub spki_sha256: [u8; 32],
    /// Inclusive start of validity.
    pub not_before_epoch_secs: u64,
    /// Inclusive end of validity.
    pub not_after_epoch_secs: u64,
    /// Admitted public-key algorithm.
    pub key_algorithm: CertificateKeyAlgorithm,
    /// Public-key strength in bits.
    pub key_bits: u16,
    /// Number of DNS SAN entries.
    pub dns_san_count: u16,
    /// Number of IP SAN entries.
    pub ip_san_count: u16,
}

impl CertificateMetadata {
    /// Returns a whole-certificate pin.
    #[must_use]
    pub const fn certificate_pin(&self) -> TlsPin {
        TlsPin::new(PinKind::CertificateSha256, self.cert_sha256)
    }

    /// Returns a `SubjectPublicKeyInfo` pin.
    #[must_use]
    pub const fn spki_pin(&self) -> TlsPin {
        TlsPin::new(PinKind::SubjectPublicKeyInfoSha256, self.spki_sha256)
    }
}

/// Failure returned by an injected Unix clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockError;

impl Display for ClockError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Unix clock is unavailable")
    }
}

impl Error for ClockError {}

/// Injectable source of Unix epoch seconds.
pub trait UnixClock: Debug + Send + Sync {
    /// Returns current Unix epoch seconds.
    ///
    /// # Errors
    ///
    /// Returns an error when trustworthy wall-clock time is unavailable.
    fn now_epoch_secs(&self) -> Result<u64, ClockError>;
}

/// Operating-system wall clock.
#[derive(Debug, Default)]
pub struct SystemUnixClock;

impl UnixClock for SystemUnixClock {
    fn now_epoch_secs(&self) -> Result<u64, ClockError> {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .map_err(|_| ClockError)
    }
}

/// A certificate chain and key admitted by all shared checks.
#[derive(Debug, Clone)]
pub struct ValidatedCertificate {
    certified_key: Arc<CertifiedKey>,
    metadata: CertificateMetadata,
}

impl ValidatedCertificate {
    /// Parses and validates a server certificate chain and private key.
    ///
    /// `expected_server_name` is parsed as an exact DNS name or IP address.
    /// Wildcard matching is deliberately not performed.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed, inapplicable, weak, mismatched, or
    /// currently invalid certificate material.
    pub fn new(
        certificate_chain: Vec<CertificateDer<'static>>,
        private_key: PrivateKeyDer<'static>,
        expected_server_name: Option<&str>,
        clock: &dyn UnixClock,
        key_policy: CertificateKeyPolicy,
    ) -> Result<Self, TlsError> {
        let expected_server_names = expected_server_name.into_iter().collect::<Vec<_>>();
        Self::new_for_server_names(
            certificate_chain,
            private_key,
            &expected_server_names,
            clock,
            key_policy,
        )
    }

    /// Parses and validates material against every configured exact DNS/IP SAN.
    ///
    /// # Errors
    ///
    /// Returns an error if any expected name is invalid or absent, or for any
    /// other malformed, weak, mismatched, or currently invalid material.
    pub fn new_for_server_names(
        certificate_chain: Vec<CertificateDer<'static>>,
        private_key: PrivateKeyDer<'static>,
        expected_server_names: &[&str],
        clock: &dyn UnixClock,
        key_policy: CertificateKeyPolicy,
    ) -> Result<Self, TlsError> {
        let leaf_der = certificate_chain
            .first()
            .ok_or(TlsError::EmptyCertificateChain)?;
        for certificate in &certificate_chain {
            let (trailing, _) = parse_x509_certificate(certificate.as_ref())
                .map_err(|_| TlsError::MalformedCertificate)?;
            if !trailing.is_empty() {
                return Err(TlsError::TrailingCertificateData);
            }
        }

        let now = clock.now_epoch_secs().map_err(|_| TlsError::ClockFailed)?;
        let metadata =
            validate_server_certificate_der(leaf_der, expected_server_names, now, key_policy)?;
        let certified_key = Self::build_certified_key(certificate_chain, private_key)?;
        Ok(Self {
            certified_key: Arc::new(certified_key),
            metadata,
        })
    }

    fn build_certified_key(
        certificate_chain: Vec<CertificateDer<'static>>,
        private_key: PrivateKeyDer<'static>,
    ) -> Result<CertifiedKey, TlsError> {
        let certified_key = CertifiedKey::from_der(
            certificate_chain,
            private_key,
            &rustls::crypto::ring::default_provider(),
        )
        .map_err(|error| match error {
            rustls::Error::InconsistentKeys(_) => TlsError::CertificateKeyMismatch,
            _ => TlsError::PrivateKeyRejected,
        })?;
        certified_key
            .keys_match()
            .map_err(|_| TlsError::CertificateKeyMismatch)?;
        Ok(certified_key)
    }

    /// Returns validated non-secret metadata.
    #[must_use]
    pub const fn metadata(&self) -> &CertificateMetadata {
        &self.metadata
    }

    /// Returns the rustls certified key.
    #[must_use]
    pub fn certified_key(&self) -> Arc<CertifiedKey> {
        Arc::clone(&self.certified_key)
    }
}

/// Validates a DER server leaf against the shared Arcen certificate policy.
///
/// This intentionally does not validate issuer trust or a private key. Clients
/// use it only after rustls has checked the requested server name, while hosts
/// use [`ValidatedCertificate`] to additionally check the complete chain and
/// key pair.
///
/// # Errors
///
/// Returns an error for malformed, inapplicable, weak, or currently invalid
/// server certificates, or when any exact expected SAN is absent.
pub fn validate_server_certificate_der(
    certificate_der: &CertificateDer<'_>,
    expected_server_names: &[&str],
    now_epoch_secs: u64,
    key_policy: CertificateKeyPolicy,
) -> Result<CertificateMetadata, TlsError> {
    let (trailing, leaf) = parse_x509_certificate(certificate_der.as_ref())
        .map_err(|_| TlsError::MalformedCertificate)?;
    if !trailing.is_empty() {
        return Err(TlsError::TrailingCertificateData);
    }
    reject_malformed_extensions(&leaf)?;
    let validity = leaf.validity();
    let not_before = u64::try_from(validity.not_before.timestamp())
        .map_err(|_| TlsError::InvalidValidityInterval)?;
    let not_after = u64::try_from(validity.not_after.timestamp())
        .map_err(|_| TlsError::InvalidValidityInterval)?;
    if not_after <= not_before {
        return Err(TlsError::InvalidValidityInterval);
    }
    if now_epoch_secs < not_before {
        return Err(TlsError::CertificateNotYetValid);
    }
    if now_epoch_secs > not_after {
        return Err(TlsError::CertificateExpired);
    }

    if leaf
        .basic_constraints()
        .map_err(|_| TlsError::MalformedCertificateExtension)?
        .is_some_and(|constraints| constraints.value.ca)
    {
        return Err(TlsError::CaLeaf);
    }
    if let Some(usage) = leaf
        .extended_key_usage()
        .map_err(|_| TlsError::MalformedCertificateExtension)?
    {
        if usage.value.any || !usage.value.server_auth {
            return Err(TlsError::InvalidExtendedKeyUsage);
        }
    }
    if let Some(usage) = leaf
        .key_usage()
        .map_err(|_| TlsError::MalformedCertificateExtension)?
    {
        if !usage.value.digital_signature() {
            return Err(TlsError::InvalidKeyUsage);
        }
    }

    let sans = leaf
        .subject_alternative_name()
        .map_err(|_| TlsError::MalformedCertificateExtension)?
        .ok_or(TlsError::MissingServerSubjectAlternativeName)?;
    let mut dns_names = Vec::new();
    let mut ip_addresses = Vec::new();
    for name in &sans.value.general_names {
        match name {
            GeneralName::DNSName(name) => {
                let canonical = if let Some(suffix) = name.strip_prefix("*.") {
                    let suffix = rustls::pki_types::DnsName::try_from(suffix)
                        .map_err(|_| TlsError::MalformedSubjectAlternativeName)?
                        .to_lowercase_owned();
                    format!("*.{}", suffix.as_ref())
                } else {
                    rustls::pki_types::DnsName::try_from(*name)
                        .map_err(|_| TlsError::MalformedSubjectAlternativeName)?
                        .to_lowercase_owned()
                        .as_ref()
                        .to_owned()
                };
                dns_names.push(canonical);
            }
            GeneralName::IPAddress(bytes) => {
                ip_addresses.push(parse_ip_san(bytes)?);
            }
            _ => {}
        }
    }
    if dns_names.is_empty() && ip_addresses.is_empty() {
        return Err(TlsError::MissingServerSubjectAlternativeName);
    }
    for expected in expected_server_names {
        validate_expected_san(expected, &dns_names, &ip_addresses)?;
    }

    let (key_algorithm, key_bits) = classify_key(&leaf.tbs_certificate.subject_pki, key_policy)?;
    Ok(CertificateMetadata {
        cert_sha256: sha256(certificate_der.as_ref()),
        spki_sha256: sha256(leaf.tbs_certificate.subject_pki.raw),
        not_before_epoch_secs: not_before,
        not_after_epoch_secs: not_after,
        key_algorithm,
        key_bits,
        dns_san_count: u16::try_from(dns_names.len()).map_err(|_| TlsError::TooManySans)?,
        ip_san_count: u16::try_from(ip_addresses.len()).map_err(|_| TlsError::TooManySans)?,
    })
}

fn reject_malformed_extensions(
    certificate: &x509_parser::certificate::X509Certificate<'_>,
) -> Result<(), TlsError> {
    certificate
        .extensions()
        .iter()
        .all(|extension| {
            !matches!(
                extension.parsed_extension(),
                x509_parser::extensions::ParsedExtension::ParseError { .. }
            )
        })
        .then_some(())
        .ok_or(TlsError::MalformedCertificateExtension)
}

fn classify_key(
    spki: &x509_parser::x509::SubjectPublicKeyInfo<'_>,
    policy: CertificateKeyPolicy,
) -> Result<(CertificateKeyAlgorithm, u16), TlsError> {
    let algorithm = spki.algorithm.algorithm.to_id_string();
    if algorithm == RSA_OID {
        let parsed = spki.parsed().map_err(|_| TlsError::UnsupportedPublicKey)?;
        let x509_parser::public_key::PublicKey::RSA(rsa) = parsed else {
            return Err(TlsError::UnsupportedPublicKey);
        };
        let bits = rsa_modulus_bits(rsa.modulus)?;
        if bits < policy.minimum_rsa_bits() {
            return Err(TlsError::WeakRsaKey {
                bits,
                minimum: policy.minimum_rsa_bits(),
            });
        }
        return Ok((CertificateKeyAlgorithm::Rsa, bits));
    }
    if algorithm == EC_PUBLIC_KEY_OID {
        let curve = spki
            .algorithm
            .parameters
            .as_ref()
            .and_then(|parameter| parameter.as_oid().ok())
            .map(|oid| oid.to_id_string())
            .ok_or(TlsError::UnsupportedEcCurve)?;
        return match curve.as_str() {
            P256_OID => Ok((CertificateKeyAlgorithm::P256, 256)),
            P384_OID => Ok((CertificateKeyAlgorithm::P384, 384)),
            _ => Err(TlsError::UnsupportedEcCurve),
        };
    }
    if algorithm == ED25519_OID && spki.subject_public_key.data.len() == 32 {
        return Ok((CertificateKeyAlgorithm::Ed25519, 256));
    }
    Err(TlsError::UnsupportedPublicKey)
}

fn rsa_modulus_bits(modulus: &[u8]) -> Result<u16, TlsError> {
    let significant = modulus
        .iter()
        .position(|byte| *byte != 0)
        .map(|index| &modulus[index..])
        .ok_or(TlsError::UnsupportedPublicKey)?;
    let first_bits = 8_u32.saturating_sub(significant[0].leading_zeros());
    let tail_bits = u32::try_from(significant.len().saturating_sub(1))
        .map_err(|_| TlsError::UnsupportedPublicKey)?
        .checked_mul(8)
        .ok_or(TlsError::UnsupportedPublicKey)?;
    u16::try_from(first_bits + tail_bits).map_err(|_| TlsError::UnsupportedPublicKey)
}

fn parse_ip_san(bytes: &[u8]) -> Result<IpAddr, TlsError> {
    match bytes.len() {
        4 => <[u8; 4]>::try_from(bytes)
            .map(Ipv4Addr::from)
            .map(IpAddr::V4)
            .map_err(|_| TlsError::MalformedSubjectAlternativeName),
        16 => <[u8; 16]>::try_from(bytes)
            .map(Ipv6Addr::from)
            .map(IpAddr::V6)
            .map_err(|_| TlsError::MalformedSubjectAlternativeName),
        _ => Err(TlsError::MalformedSubjectAlternativeName),
    }
}

fn validate_expected_san(
    expected: &str,
    dns_names: &[String],
    ip_addresses: &[IpAddr],
) -> Result<(), TlsError> {
    match ServerName::try_from(expected).map_err(|_| TlsError::InvalidExpectedServerName)? {
        ServerName::DnsName(expected_dns) => {
            let expected_dns = expected_dns.to_lowercase_owned();
            let matched = dns_names
                .iter()
                .any(|candidate| candidate == expected_dns.as_ref());
            matched.then_some(()).ok_or(TlsError::ExpectedSanMismatch)
        }
        ServerName::IpAddress(expected_ip) => ip_addresses
            .contains(&IpAddr::from(expected_ip))
            .then_some(())
            .ok_or(TlsError::ExpectedSanMismatch),
        _ => Err(TlsError::InvalidExpectedServerName),
    }
}

fn sha256(value: &[u8]) -> [u8; 32] {
    Sha256::digest(value).into()
}

#[derive(Debug)]
struct ActiveCertificate {
    certified_key: Arc<CertifiedKey>,
    metadata: CertificateMetadata,
}

/// Atomically reloadable rustls server certificate resolver.
pub struct ReloadingCertifiedKey {
    active: RwLock<ActiveCertificate>,
    clock: Arc<dyn UnixClock>,
}

impl Debug for ReloadingCertifiedKey {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ReloadingCertifiedKey")
            .finish_non_exhaustive()
    }
}

impl ReloadingCertifiedKey {
    /// Creates a resolver from already validated, currently valid material.
    ///
    /// # Errors
    ///
    /// Returns an error if the injected clock fails or the material is not
    /// currently valid according to that clock.
    pub fn new(initial: ValidatedCertificate, clock: Arc<dyn UnixClock>) -> Result<Self, TlsError> {
        ensure_current(&initial.metadata, clock.as_ref())?;
        Ok(Self {
            active: RwLock::new(ActiveCertificate {
                certified_key: initial.certified_key,
                metadata: initial.metadata,
            }),
            clock,
        })
    }

    /// Validates replacement material and atomically installs it.
    ///
    /// Validation and key construction complete before the write lock is
    /// acquired. Failure leaves the prior certificate untouched.
    ///
    /// # Errors
    ///
    /// Returns the validation, clock, or lock failure.
    pub fn reload(
        &self,
        certificate_chain: Vec<CertificateDer<'static>>,
        private_key: PrivateKeyDer<'static>,
        expected_server_name: Option<&str>,
        key_policy: CertificateKeyPolicy,
    ) -> Result<CertificateMetadata, TlsError> {
        let replacement = ValidatedCertificate::new(
            certificate_chain,
            private_key,
            expected_server_name,
            self.clock.as_ref(),
            key_policy,
        )?;
        self.reload_validated(replacement)
    }

    /// Atomically installs already validated replacement material.
    ///
    /// # Errors
    ///
    /// Returns an error if the write lock was poisoned.
    pub fn reload_validated(
        &self,
        replacement: ValidatedCertificate,
    ) -> Result<CertificateMetadata, TlsError> {
        ensure_current(&replacement.metadata, self.clock.as_ref())?;
        let metadata = replacement.metadata.clone();
        let mut active = self.active.write().map_err(|_| TlsError::LockPoisoned)?;
        *active = ActiveCertificate {
            certified_key: replacement.certified_key,
            metadata: replacement.metadata,
        };
        Ok(metadata)
    }

    /// Returns a snapshot of current non-secret metadata.
    ///
    /// # Errors
    ///
    /// Returns an error if the read lock was poisoned.
    pub fn metadata(&self) -> Result<CertificateMetadata, TlsError> {
        self.active
            .read()
            .map(|active| active.metadata.clone())
            .map_err(|_| TlsError::LockPoisoned)
    }

    /// Returns the current key only while it remains valid.
    ///
    /// Clock failure, expiry, or lock poisoning refuses new handshakes.
    #[must_use]
    pub fn resolve_current(&self) -> Option<Arc<CertifiedKey>> {
        let now = self.clock.now_epoch_secs().ok()?;
        let active = self.active.read().ok()?;
        if now < active.metadata.not_before_epoch_secs || now > active.metadata.not_after_epoch_secs
        {
            return None;
        }
        Some(Arc::clone(&active.certified_key))
    }
}

fn ensure_current(metadata: &CertificateMetadata, clock: &dyn UnixClock) -> Result<(), TlsError> {
    let now = clock.now_epoch_secs().map_err(|_| TlsError::ClockFailed)?;
    if now < metadata.not_before_epoch_secs {
        return Err(TlsError::CertificateNotYetValid);
    }
    if now > metadata.not_after_epoch_secs {
        return Err(TlsError::CertificateExpired);
    }
    Ok(())
}

impl ResolvesServerCert for ReloadingCertifiedKey {
    fn resolve(&self, _client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        self.resolve_current()
    }
}

/// Shared TLS certificate/posture failure.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum TlsError {
    /// Version-floor text is not supported.
    UnsupportedVersionFloor,
    /// Cipher-suite text or provider mapping is not supported.
    UnsupportedCipherSuite,
    /// No cipher suite is usable for the selected versions.
    InvalidTlsPosture,
    /// No certificate was supplied.
    EmptyCertificateChain,
    /// A certificate is not valid DER.
    MalformedCertificate,
    /// DER bytes remain after a certificate.
    TrailingCertificateData,
    /// A parsed certificate extension is malformed or duplicated.
    MalformedCertificateExtension,
    /// The certificate interval is invalid or cannot be represented.
    InvalidValidityInterval,
    /// The leaf is not yet valid.
    CertificateNotYetValid,
    /// The leaf has expired.
    CertificateExpired,
    /// The leaf asserts CA capability.
    CaLeaf,
    /// The leaf has no DNS or IP SAN.
    MissingServerSubjectAlternativeName,
    /// An IP SAN has an invalid encoded length.
    MalformedSubjectAlternativeName,
    /// Expected server name text is neither canonical DNS nor IP syntax.
    InvalidExpectedServerName,
    /// No leaf SAN exactly matches the expected server name.
    ExpectedSanMismatch,
    /// Present EKU does not authorize server authentication.
    InvalidExtendedKeyUsage,
    /// Present key usage does not authorize digital signatures.
    InvalidKeyUsage,
    /// The public-key algorithm is unsupported.
    UnsupportedPublicKey,
    /// The EC named curve is unsupported.
    UnsupportedEcCurve,
    /// RSA modulus is below policy.
    WeakRsaKey {
        /// Observed modulus bits.
        bits: u16,
        /// Required modulus bits.
        minimum: u16,
    },
    /// The private key could not be loaded.
    PrivateKeyRejected,
    /// The private key does not match the leaf public key.
    CertificateKeyMismatch,
    /// SAN count cannot be represented in metadata.
    TooManySans,
    /// The injected clock failed.
    ClockFailed,
    /// Resolver state lock was poisoned.
    LockPoisoned,
}

impl Display for TlsError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedVersionFloor => formatter.write_str("unsupported TLS version floor"),
            Self::UnsupportedCipherSuite => formatter.write_str("unsupported ring cipher suite"),
            Self::InvalidTlsPosture => formatter.write_str("TLS posture has no usable suite"),
            Self::EmptyCertificateChain => formatter.write_str("certificate chain is empty"),
            Self::MalformedCertificate => formatter.write_str("certificate DER is malformed"),
            Self::TrailingCertificateData => {
                formatter.write_str("certificate DER contains trailing data")
            }
            Self::MalformedCertificateExtension => {
                formatter.write_str("certificate extension is malformed")
            }
            Self::InvalidValidityInterval => {
                formatter.write_str("certificate validity interval is invalid")
            }
            Self::CertificateNotYetValid => formatter.write_str("certificate is not yet valid"),
            Self::CertificateExpired => formatter.write_str("certificate has expired"),
            Self::CaLeaf => formatter.write_str("server leaf certificate is a CA"),
            Self::MissingServerSubjectAlternativeName => {
                formatter.write_str("server leaf has no DNS or IP SAN")
            }
            Self::MalformedSubjectAlternativeName => {
                formatter.write_str("certificate SAN is malformed")
            }
            Self::InvalidExpectedServerName => {
                formatter.write_str("expected server name is invalid")
            }
            Self::ExpectedSanMismatch => formatter.write_str("expected SAN does not match"),
            Self::InvalidExtendedKeyUsage => {
                formatter.write_str("certificate EKU does not authorize server authentication")
            }
            Self::InvalidKeyUsage => {
                formatter.write_str("certificate key usage lacks digital signature")
            }
            Self::UnsupportedPublicKey => {
                formatter.write_str("certificate public-key algorithm is unsupported")
            }
            Self::UnsupportedEcCurve => formatter.write_str("certificate EC curve is unsupported"),
            Self::WeakRsaKey { bits, minimum } => {
                write!(formatter, "RSA key has {bits} bits; {minimum} required")
            }
            Self::PrivateKeyRejected => formatter.write_str("private key was rejected"),
            Self::CertificateKeyMismatch => {
                formatter.write_str("certificate and private key do not match")
            }
            Self::TooManySans => formatter.write_str("certificate has too many SAN entries"),
            Self::ClockFailed => formatter.write_str("Unix clock failed"),
            Self::LockPoisoned => formatter.write_str("certificate resolver lock is poisoned"),
        }
    }
}

impl Error for TlsError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::thread;

    use rcgen::{
        BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa, KeyPair,
        KeyUsagePurpose, PKCS_ECDSA_P256_SHA256, PKCS_ECDSA_P384_SHA384, PKCS_ED25519,
        SerialNumber, date_time_ymd,
    };
    use rustls::pki_types::PrivatePkcs8KeyDer;

    const NOW: u64 = 1_800_000_000;

    #[derive(Debug)]
    struct TestClock {
        now: AtomicU64,
        fail: AtomicBool,
    }

    impl TestClock {
        fn new(now: u64) -> Self {
            Self {
                now: AtomicU64::new(now),
                fail: AtomicBool::new(false),
            }
        }
    }

    impl UnixClock for TestClock {
        fn now_epoch_secs(&self) -> Result<u64, ClockError> {
            if self.fail.load(Ordering::SeqCst) {
                Err(ClockError)
            } else {
                Ok(self.now.load(Ordering::SeqCst))
            }
        }
    }

    fn params(names: &[&str]) -> CertificateParams {
        let mut params = CertificateParams::new(
            names
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<String>>(),
        )
        .expect("valid names");
        params.not_before = date_time_ymd(2020, 1, 1);
        params.not_after = date_time_ymd(2040, 1, 1);
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params
    }

    fn material_with(
        params: CertificateParams,
        algorithm: &'static rcgen::SignatureAlgorithm,
    ) -> (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>) {
        let key = KeyPair::generate_for(algorithm).expect("test key");
        material_for_key(params, &key)
    }

    fn material_for_key(
        params: CertificateParams,
        key: &KeyPair,
    ) -> (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>) {
        let cert = params.self_signed(key).expect("test certificate");
        (
            vec![cert.der().clone()],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())),
        )
    }

    fn good_material() -> (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>) {
        material_with(
            params(&["Example.COM", "192.0.2.10"]),
            &PKCS_ECDSA_P256_SHA256,
        )
    }

    fn validate(
        material: (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>),
        expected: Option<&str>,
    ) -> Result<ValidatedCertificate, TlsError> {
        ValidatedCertificate::new(
            material.0,
            material.1,
            expected,
            &TestClock::new(NOW),
            CertificateKeyPolicy::default(),
        )
    }

    fn replace_der_bytes(value: &mut [u8], needle: &[u8], replacement: &[u8]) {
        let offset = value
            .windows(needle.len())
            .position(|window| window == needle)
            .expect("DER sequence");
        value[offset..offset + needle.len()].copy_from_slice(replacement);
    }

    #[test]
    fn suite_names_parse_format_order_and_filter() {
        let parsed = RingCipherSuite::ALL
            .map(|suite| suite.to_string().parse::<RingCipherSuite>().expect("suite"));
        assert_eq!(parsed, RingCipherSuite::ALL);
        let provider_names = rustls::crypto::ring::ALL_CIPHER_SUITES
            .iter()
            .map(|suite| suite.suite())
            .collect::<Vec<_>>();
        let contract_names = RingCipherSuite::ALL
            .iter()
            .map(|suite| suite.rustls_name())
            .collect::<Vec<_>>();
        assert_eq!(contract_names, provider_names);

        #[cfg(feature = "wss-compat")]
        let disabled = [
            RingCipherSuite::Tls13Aes128GcmSha256,
            RingCipherSuite::TlsEcdheRsaWithAes128GcmSha256,
        ];
        #[cfg(not(feature = "wss-compat"))]
        let disabled = [RingCipherSuite::Tls13Aes128GcmSha256];
        let posture = TlsPosture::new(TlsVersionFloor::default(), disabled).expect("posture");
        assert_eq!(
            posture.enabled_suites(),
            RingCipherSuite::ALL
                .iter()
                .filter(|suite| !disabled.contains(suite))
                .copied()
                .collect::<Vec<_>>()
        );
        assert_eq!(
            "not-a-suite".parse::<RingCipherSuite>(),
            Err(TlsError::UnsupportedCipherSuite)
        );
    }

    #[test]
    fn empty_or_version_incompatible_suite_sets_fail() {
        assert_eq!(
            TlsPosture::new(TlsVersionFloor::default(), RingCipherSuite::ALL),
            Err(TlsError::InvalidTlsPosture)
        );
        #[cfg(feature = "wss-compat")]
        {
            assert_eq!(
                TlsPosture::new(
                    TlsVersionFloor::Tls13,
                    RingCipherSuite::ALL[..3].iter().copied()
                ),
                Err(TlsError::InvalidTlsPosture)
            );
            assert!(
                TlsPosture::new(
                    TlsVersionFloor::Tls13,
                    RingCipherSuite::ALL[3..].iter().copied()
                )
                .is_ok()
            );
            assert_eq!("TLS1.2".parse(), Ok(TlsVersionFloor::Tls12));
        }
        #[cfg(not(feature = "wss-compat"))]
        assert_eq!(
            "TLS1.2".parse::<TlsVersionFloor>(),
            Err(TlsError::UnsupportedVersionFloor)
        );
        assert_eq!("TLS1.3".parse(), Ok(TlsVersionFloor::Tls13));
        assert_eq!(
            "TLS1.1".parse::<TlsVersionFloor>(),
            Err(TlsError::UnsupportedVersionFloor)
        );
    }

    #[test]
    fn supported_key_algorithms_and_exact_sans_validate() {
        for (algorithm, expected_key) in [
            (&PKCS_ECDSA_P256_SHA256, CertificateKeyAlgorithm::P256),
            (&PKCS_ECDSA_P384_SHA384, CertificateKeyAlgorithm::P384),
            (&PKCS_ED25519, CertificateKeyAlgorithm::Ed25519),
        ] {
            let validated = validate(
                material_with(params(&["Example.COM"]), algorithm),
                Some("example.com"),
            )
            .expect("supported key");
            assert_eq!(validated.metadata.key_algorithm, expected_key);
        }

        let validated = validate(good_material(), Some("192.0.2.10")).expect("IP SAN");
        assert_eq!(validated.metadata.dns_san_count, 1);
        assert_eq!(validated.metadata.ip_san_count, 1);
        assert_eq!(
            validate(good_material(), Some("other.example")).map(|_| ()),
            Err(TlsError::ExpectedSanMismatch)
        );
        assert_eq!(
            validate(good_material(), Some("not a name")).map(|_| ()),
            Err(TlsError::InvalidExpectedServerName)
        );
    }

    #[test]
    fn malformed_empty_and_trailing_chains_fail() {
        let (_, key) = good_material();
        assert_eq!(
            ValidatedCertificate::new(
                Vec::new(),
                key,
                None,
                &TestClock::new(NOW),
                CertificateKeyPolicy::default()
            )
            .map(|_| ()),
            Err(TlsError::EmptyCertificateChain)
        );
        let (_, key) = good_material();
        assert_eq!(
            validate((vec![CertificateDer::from(vec![1, 2, 3])], key), None).map(|_| ()),
            Err(TlsError::MalformedCertificate)
        );
        let (mut chain, key) = good_material();
        chain.push(CertificateDer::from(vec![1, 2, 3]));
        assert_eq!(
            validate((chain, key), None).map(|_| ()),
            Err(TlsError::MalformedCertificate)
        );
        let (mut chain, key) = good_material();
        let mut trailing = chain.remove(0).as_ref().to_vec();
        trailing.push(0);
        assert_eq!(
            validate((vec![CertificateDer::from(trailing)], key), None).map(|_| ()),
            Err(TlsError::TrailingCertificateData)
        );
    }

    #[test]
    fn validity_ca_san_eku_and_ku_fail_closed() {
        let mut future = params(&["example.com"]);
        future.not_before = date_time_ymd(2090, 1, 1);
        future.not_after = date_time_ymd(2100, 1, 1);
        assert_eq!(
            validate(material_with(future, &PKCS_ECDSA_P256_SHA256), None).map(|_| ()),
            Err(TlsError::CertificateNotYetValid)
        );

        let mut expired = params(&["example.com"]);
        expired.not_after = date_time_ymd(2021, 1, 1);
        assert_eq!(
            validate(material_with(expired, &PKCS_ECDSA_P256_SHA256), None).map(|_| ()),
            Err(TlsError::CertificateExpired)
        );

        let mut invalid = params(&["example.com"]);
        invalid.not_before = date_time_ymd(2040, 1, 1);
        invalid.not_after = date_time_ymd(2030, 1, 1);
        assert_eq!(
            validate(material_with(invalid, &PKCS_ECDSA_P256_SHA256), None).map(|_| ()),
            Err(TlsError::InvalidValidityInterval)
        );

        let mut ca = params(&["example.com"]);
        ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        assert_eq!(
            validate(material_with(ca, &PKCS_ECDSA_P256_SHA256), None).map(|_| ()),
            Err(TlsError::CaLeaf)
        );

        let mut no_san = params(&[]);
        no_san.subject_alt_names.clear();
        assert_eq!(
            validate(material_with(no_san, &PKCS_ECDSA_P256_SHA256), None).map(|_| ()),
            Err(TlsError::MissingServerSubjectAlternativeName)
        );

        for usages in [
            vec![ExtendedKeyUsagePurpose::ClientAuth],
            vec![ExtendedKeyUsagePurpose::Any],
        ] {
            let mut wrong_eku = params(&["example.com"]);
            wrong_eku.extended_key_usages = usages;
            assert_eq!(
                validate(material_with(wrong_eku, &PKCS_ECDSA_P256_SHA256), None).map(|_| ()),
                Err(TlsError::InvalidExtendedKeyUsage)
            );
        }

        let mut wrong_ku = params(&["example.com"]);
        wrong_ku.key_usages = vec![KeyUsagePurpose::KeyEncipherment];
        assert_eq!(
            validate(material_with(wrong_ku, &PKCS_ECDSA_P256_SHA256), None).map(|_| ()),
            Err(TlsError::InvalidKeyUsage)
        );
    }

    #[test]
    fn key_mismatch_is_rejected() {
        let (chain, _) = good_material();
        let (_, other_key) = good_material();
        assert_eq!(
            validate((chain, other_key), Some("example.com")).map(|_| ()),
            Err(TlsError::CertificateKeyMismatch)
        );
    }

    #[test]
    fn unsupported_public_key_algorithm_and_ec_curve_are_rejected() {
        const EC_ALGORITHM: &[u8] = &[0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01];
        const OTHER_ALGORITHM: &[u8] = &[0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x02];
        const P256_CURVE: &[u8] = &[0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07];
        const OTHER_CURVE: &[u8] = &[0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x08];

        let (chain, key) = good_material();
        let mut leaf = chain[0].as_ref().to_vec();
        replace_der_bytes(&mut leaf, EC_ALGORITHM, OTHER_ALGORITHM);
        assert_eq!(
            validate((vec![CertificateDer::from(leaf)], key), None).map(|_| ()),
            Err(TlsError::UnsupportedPublicKey)
        );

        let (chain, key) = good_material();
        let mut leaf = chain[0].as_ref().to_vec();
        replace_der_bytes(&mut leaf, P256_CURVE, OTHER_CURVE);
        assert_eq!(
            validate((vec![CertificateDer::from(leaf)], key), None).map(|_| ()),
            Err(TlsError::UnsupportedEcCurve)
        );
    }

    #[test]
    fn renewal_and_rekey_hashes_have_expected_semantics() {
        let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("key");
        let mut first_params = params(&["example.com"]);
        first_params.serial_number = Some(SerialNumber::from(1_u64));
        let mut renewed_params = params(&["example.com"]);
        renewed_params.serial_number = Some(SerialNumber::from(2_u64));
        let first = validate(material_for_key(first_params, &key), None).expect("first");
        let renewed = validate(material_for_key(renewed_params, &key), None).expect("renewed");
        let rekeyed = validate(
            material_with(params(&["example.com"]), &PKCS_ECDSA_P256_SHA256),
            None,
        )
        .expect("rekeyed");
        assert_ne!(first.metadata.cert_sha256, renewed.metadata.cert_sha256);
        assert_eq!(first.metadata.spki_sha256, renewed.metadata.spki_sha256);
        assert_ne!(first.metadata.spki_sha256, rekeyed.metadata.spki_sha256);
    }

    #[test]
    fn typed_pins_never_cross_namespaces() {
        let validated = validate(good_material(), None).expect("certificate");
        let certificate = validated.metadata.certificate_pin();
        let same = TlsPin::new(PinKind::CertificateSha256, certificate.digest);
        let cross_kind = TlsPin::new(PinKind::SubjectPublicKeyInfoSha256, certificate.digest);
        let mut changed = certificate.digest;
        changed[31] ^= 1;
        assert!(certificate.matches(&same));
        assert!(!certificate.matches(&cross_kind));
        assert!(!certificate.matches(&TlsPin::new(PinKind::CertificateSha256, changed)));
    }

    #[test]
    fn warning_boundary_and_clock_failure_are_explicit() {
        let validated = validate(good_material(), None).expect("certificate");
        let mut metadata = validated.metadata.clone();
        let policy = CertificateTimePolicy::default();
        metadata.not_after_epoch_secs = NOW + policy.warning_window_secs;
        assert!(
            policy
                .is_expiring(&metadata, &TestClock::new(NOW))
                .expect("boundary")
        );
        metadata.not_after_epoch_secs += 1;
        assert!(
            !policy
                .is_expiring(&metadata, &TestClock::new(NOW))
                .expect("outside")
        );
        let failing = TestClock::new(NOW);
        failing.fail.store(true, Ordering::SeqCst);
        assert_eq!(
            policy.is_expiring(&metadata, &failing),
            Err(TlsError::ClockFailed)
        );
    }

    #[test]
    fn reload_failure_retains_good_key_and_expiry_refuses_new_handshakes() {
        let clock = Arc::new(TestClock::new(NOW));
        let initial = ValidatedCertificate::new(
            good_material().0,
            good_material().1,
            None,
            clock.as_ref(),
            CertificateKeyPolicy::default(),
        );
        assert_eq!(initial.map(|_| ()), Err(TlsError::CertificateKeyMismatch));

        let initial = validate(good_material(), None).expect("initial");
        let initial_hash = initial.metadata.cert_sha256;
        let resolver =
            Arc::new(ReloadingCertifiedKey::new(initial, clock.clone()).expect("initial resolver"));
        let config = TlsPosture::default()
            .server_config(resolver.clone())
            .expect("server configuration");
        assert!(config.alpn_protocols.is_empty());
        assert_eq!(config.max_early_data_size, 0);
        assert!(!config.send_half_rtt_data);
        let established = resolver.resolve_current().expect("current key");
        let (_, wrong_key) = good_material();
        let (chain, _) = good_material();
        assert!(
            resolver
                .reload(chain, wrong_key, None, CertificateKeyPolicy::default())
                .is_err()
        );
        assert_eq!(
            resolver.metadata().expect("metadata").cert_sha256,
            initial_hash
        );
        clock.now.store(3_000_000_000, Ordering::SeqCst);
        assert!(resolver.resolve_current().is_none());
        assert!(!established.cert.is_empty());
        clock.fail.store(true, Ordering::SeqCst);
        assert!(resolver.resolve_current().is_none());
    }

    #[test]
    fn resolver_reload_is_atomic_under_concurrent_reads() {
        let clock = Arc::new(TestClock::new(NOW));
        let initial = validate(good_material(), None).expect("initial");
        let resolver =
            Arc::new(ReloadingCertifiedKey::new(initial, clock).expect("initial resolver"));
        let reader = {
            let resolver = Arc::clone(&resolver);
            thread::spawn(move || {
                for _ in 0..1_000 {
                    assert!(resolver.resolve_current().is_some());
                }
            })
        };
        let replacement = validate(
            material_with(params(&["replacement.example"]), &PKCS_ED25519),
            None,
        )
        .expect("replacement");
        let expected = replacement.metadata.cert_sha256;
        resolver
            .reload_validated(replacement)
            .expect("atomic reload");
        reader.join().expect("reader");
        assert_eq!(resolver.metadata().expect("metadata").cert_sha256, expected);
    }

    #[test]
    fn poisoned_resolver_lock_refuses_new_handshakes() {
        let clock = Arc::new(TestClock::new(NOW));
        let initial = validate(good_material(), None).expect("initial");
        let resolver =
            Arc::new(ReloadingCertifiedKey::new(initial, clock).expect("initial resolver"));
        let poisoner = {
            let resolver = Arc::clone(&resolver);
            thread::spawn(move || {
                let _guard = resolver.active.write().expect("write lock");
                panic!("poison resolver lock");
            })
        };
        assert!(poisoner.join().is_err());
        assert!(resolver.resolve_current().is_none());
        assert_eq!(resolver.metadata(), Err(TlsError::LockPoisoned));
    }
}
