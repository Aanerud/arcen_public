use arcen_transport::tls::{
    validate_server_certificate_der, CertificateKeyPolicy, PinKind, TlsPin,
};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::{verify_server_name, WebPkiServerVerifier};
use rustls::server::ParsedCertificate;
use rustls::{
    CertificateError, ClientConfig, DigitallySignedStruct, DistinguishedName, Error as RustlsError,
    RootCertStore, SignatureScheme,
};
use rustls_pki_types::{CertificateDer, ServerName, UnixTime};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;
use std::fs::File;
use std::io::BufReader;
use std::path::PathBuf;
use std::sync::Arc;
use thiserror::Error;
#[cfg(feature = "wss-compat")]
use tokio_tungstenite::Connector;
use x509_parser::parse_x509_certificate;

pub const INSECURE_ENV_KEY: &str = "ARCEN_ACCEPT_INSECURE";
pub const INSECURE_ENV_VALUE: &str = "1";
pub const TOFU_PIN_MISMATCH_ERROR: &str = "Arcen TOFU certificate fingerprint changed";
pub const TOFU_CAPTURE_REJECT_ERROR: &str = "Arcen TOFU certificate captured; handshake rejected";

pub type Sha256Fingerprint = [u8; 32];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TlsTrustMode {
    SystemCa,
    PrivateCa { ca_bundle: PathBuf },
    TofuPending,
    TofuPinned { pin: TlsPin },
    InsecureSkipVerify,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BookmarkTlsMode {
    Auto,
    PrivateCa,
    TofuPinned,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BookmarkPinKind {
    CertificateSha256,
    SpkiSha256,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TlsBookmarkConfig {
    pub mode: BookmarkTlsMode,
    #[serde(default)]
    pub pin_kind: Option<BookmarkPinKind>,
    pub pinned_fingerprint_sha256: Option<String>,
    #[serde(default)]
    pub pinned_spki_sha256: Option<String>,
    pub pinned_at: Option<String>,
    pub pinned_label: Option<String>,
    pub ca_bundle_path: Option<PathBuf>,
    pub session_only: bool,
}

impl Default for TlsBookmarkConfig {
    fn default() -> Self {
        Self {
            mode: BookmarkTlsMode::Auto,
            pin_kind: None,
            pinned_fingerprint_sha256: None,
            pinned_spki_sha256: None,
            pinned_at: None,
            pinned_label: None,
            ca_bundle_path: None,
            session_only: false,
        }
    }
}

impl TlsBookmarkConfig {
    pub fn validated_pin(&self) -> Result<Option<TlsPin>, TlsTrustError> {
        let certificate = self
            .pinned_fingerprint_sha256
            .as_deref()
            .map(parse_fingerprint)
            .transpose()?;
        let spki = self
            .pinned_spki_sha256
            .as_deref()
            .map(parse_fingerprint)
            .transpose()?;

        if self.mode != BookmarkTlsMode::TofuPinned {
            return if self.pin_kind.is_none() && certificate.is_none() && spki.is_none() {
                Ok(None)
            } else {
                Err(TlsTrustError::InvalidBookmarkPin(
                    "pin fields require tofu_pinned mode",
                ))
            };
        }

        match self.pin_kind {
            None => match (certificate, spki) {
                (Some(digest), None) => Ok(Some(TlsPin::new(PinKind::CertificateSha256, digest))),
                _ => Err(TlsTrustError::InvalidBookmarkPin(
                    "legacy tofu_pinned bookmarks require only pinned_fingerprint_sha256",
                )),
            },
            Some(BookmarkPinKind::CertificateSha256) => match (certificate, spki) {
                (Some(digest), None) => Ok(Some(TlsPin::new(PinKind::CertificateSha256, digest))),
                _ => Err(TlsTrustError::InvalidBookmarkPin(
                    "certificate_sha256 requires only pinned_fingerprint_sha256",
                )),
            },
            Some(BookmarkPinKind::SpkiSha256) => match (certificate, spki) {
                (None, Some(digest)) => Ok(Some(TlsPin::new(
                    PinKind::SubjectPublicKeyInfoSha256,
                    digest,
                ))),
                _ => Err(TlsTrustError::InvalidBookmarkPin(
                    "spki_sha256 requires only pinned_spki_sha256",
                )),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertInfo {
    pub endpoint: String,
    pub server_name: String,
    /// The address this certificate actually came from.
    ///
    /// `endpoint` and `server_name` are what the user typed, and are identical
    /// for every address a hostname resolves to. When several are dialled
    /// concurrently the dialog would otherwise be unable to say which peer
    /// presented the fingerprint being approved.
    pub peer_address: Option<String>,
    pub certificate_sha256: TlsPin,
    pub certificate_sha256_display: String,
    pub spki_sha256: TlsPin,
    pub spki_sha256_display: String,
    pub not_before_epoch_secs: u64,
    pub not_after_epoch_secs: u64,
    pub cert_der_len: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TofuDecision {
    Cancel,
    TrustOnce,
    TrustAndRemember,
}

#[derive(Clone)]
pub struct TlsTrustConfig {
    pub mode: TlsTrustMode,
    pub insecure_cli_flag: bool,
    tofu_endpoint: Option<String>,
    captured_certificate: Arc<std::sync::Mutex<Option<CertInfo>>>,
}

impl fmt::Debug for TlsTrustConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TlsTrustConfig")
            .field("mode", &self.mode)
            .field("insecure_cli_flag", &self.insecure_cli_flag)
            .finish_non_exhaustive()
    }
}

impl Default for TlsTrustConfig {
    fn default() -> Self {
        Self {
            mode: TlsTrustMode::SystemCa,
            insecure_cli_flag: false,
            tofu_endpoint: None,
            captured_certificate: Arc::new(std::sync::Mutex::new(None)),
        }
    }
}

impl TlsTrustConfig {
    pub fn system_ca() -> Self {
        Self::default()
    }

    pub fn private_ca(ca_bundle: PathBuf) -> Self {
        Self {
            mode: TlsTrustMode::PrivateCa { ca_bundle },
            ..Self::default()
        }
    }

    pub fn pinned(fingerprint_sha256: Sha256Fingerprint) -> Self {
        Self::pinned_certificate(fingerprint_sha256)
    }

    pub fn pinned_certificate(fingerprint_sha256: Sha256Fingerprint) -> Self {
        Self {
            mode: TlsTrustMode::TofuPinned {
                pin: TlsPin::new(PinKind::CertificateSha256, fingerprint_sha256),
            },
            ..Self::default()
        }
    }

    pub fn pinned_spki(fingerprint_sha256: Sha256Fingerprint) -> Self {
        Self {
            mode: TlsTrustMode::TofuPinned {
                pin: TlsPin::new(PinKind::SubjectPublicKeyInfoSha256, fingerprint_sha256),
            },
            ..Self::default()
        }
    }

    pub fn tofu_probe(endpoint: impl Into<String>) -> Self {
        Self {
            mode: TlsTrustMode::TofuPending,
            tofu_endpoint: Some(endpoint.into()),
            ..Self::default()
        }
    }

    pub fn insecure_dev_escape_hatch(insecure_cli_flag: bool) -> Self {
        Self {
            mode: TlsTrustMode::InsecureSkipVerify,
            insecure_cli_flag,
            ..Self::default()
        }
    }

    #[cfg(feature = "wss-compat")]
    pub fn rustls_connector(&self) -> Result<Option<Connector>, TlsTrustError> {
        match &self.mode {
            TlsTrustMode::SystemCa => Ok(None),
            TlsTrustMode::PrivateCa { ca_bundle } => {
                let roots = roots_with_private_ca(ca_bundle)?;
                Ok(Some(Connector::Rustls(Arc::new(
                    ClientConfig::builder()
                        .with_root_certificates(roots)
                        .with_no_client_auth(),
                ))))
            }
            TlsTrustMode::TofuPinned { pin } => {
                Ok(Some(Connector::Rustls(custom_verifier_config(Arc::new(
                    PinningVerifier::new(*pin, webpki_verifier(native_roots()?)?),
                )))))
            }
            TlsTrustMode::TofuPending => {
                *self
                    .captured_certificate
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
                Ok(Some(Connector::Rustls(custom_verifier_config(Arc::new(
                    TofuVerifier::new(
                        webpki_verifier(native_roots()?)?,
                        self.tofu_endpoint.clone().unwrap_or_default(),
                        self.captured_certificate.clone(),
                    ),
                )))))
            }
            TlsTrustMode::InsecureSkipVerify => {
                if !insecure_tls_allowed(self.insecure_cli_flag) {
                    return Err(TlsTrustError::InsecureDoubleGateMissing);
                }
                Ok(Some(Connector::Rustls(custom_verifier_config(Arc::new(
                    InsecureVerifier::new(webpki_verifier(native_roots()?)?),
                )))))
            }
        }
    }

    pub fn quic_rustls_config(&self, alpn_protocol: &[u8]) -> Result<ClientConfig, TlsTrustError> {
        let mut config = match &self.mode {
            TlsTrustMode::SystemCa => ClientConfig::builder()
                .with_root_certificates(native_roots()?)
                .with_no_client_auth(),
            TlsTrustMode::PrivateCa { ca_bundle } => ClientConfig::builder()
                .with_root_certificates(roots_with_private_ca(ca_bundle)?)
                .with_no_client_auth(),
            TlsTrustMode::TofuPinned { pin } => custom_verifier_client_config(Arc::new(
                PinningVerifier::new(*pin, webpki_verifier(native_roots()?)?),
            )),
            TlsTrustMode::TofuPending => {
                *self
                    .captured_certificate
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
                custom_verifier_client_config(Arc::new(TofuVerifier::new(
                    webpki_verifier(native_roots()?)?,
                    self.tofu_endpoint.clone().unwrap_or_default(),
                    self.captured_certificate.clone(),
                )))
            }
            TlsTrustMode::InsecureSkipVerify => {
                if !insecure_tls_allowed(self.insecure_cli_flag) {
                    return Err(TlsTrustError::InsecureDoubleGateMissing);
                }
                custom_verifier_client_config(Arc::new(InsecureVerifier::new(webpki_verifier(
                    native_roots()?,
                )?)))
            }
        };
        config.alpn_protocols = vec![alpn_protocol.to_vec()];
        Ok(config)
    }

    pub fn take_captured_certificate(&self) -> Option<CertInfo> {
        self.captured_certificate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }

    /// A clone that captures into its own slot.
    ///
    /// `quic_rustls_config` hands the verifier an `Arc` of this config's
    /// capture slot, so cloning the config shares the slot. When several
    /// addresses for one hostname are dialled concurrently, every attempt's
    /// verifier then writes the certificate it saw into that one slot and the
    /// last writer wins — so the fingerprint shown to the user need not belong
    /// to the connection whose error was reported. Each attempt gets its own
    /// slot instead.
    #[must_use]
    pub fn with_private_capture_slot(&self) -> Self {
        Self {
            mode: self.mode.clone(),
            insecure_cli_flag: self.insecure_cli_flag,
            tofu_endpoint: self.tofu_endpoint.clone(),
            captured_certificate: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    #[cfg(test)]
    pub(crate) fn set_captured_certificate_for_test(&self, info: CertInfo) {
        *self
            .captured_certificate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(info);
    }
}

#[derive(Debug, Error)]
pub enum TlsTrustError {
    #[error("no native root certificates were found")]
    NoNativeRoots,
    #[error("failed to load native root certificates: {0}")]
    NativeRoots(String),
    #[error("failed to open CA bundle {path}: {source}")]
    OpenCaBundle {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to read certificate from CA bundle {path}: {source}")]
    ReadCaBundle {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("CA bundle {0} did not contain any certificates")]
    EmptyCaBundle(PathBuf),
    #[error("failed to add CA certificate from {0}")]
    AddCaCertificate(PathBuf),
    #[error("failed to build rustls verifier: {0}")]
    VerifierBuilder(#[from] rustls::client::VerifierBuilderError),
    #[error("--insecure-skip-verify also requires ARCEN_ACCEPT_INSECURE=1")]
    InsecureDoubleGateMissing,
    #[error("invalid fingerprint: {0}")]
    InvalidFingerprint(String),
    #[error("invalid TLS bookmark pin: {0}")]
    InvalidBookmarkPin(&'static str),
}

pub fn insecure_tls_allowed(cli_flag: bool) -> bool {
    cli_flag && std::env::var(INSECURE_ENV_KEY).as_deref() == Ok(INSECURE_ENV_VALUE)
}

pub fn fingerprint_sha256(cert_der: &[u8]) -> Sha256Fingerprint {
    Sha256::digest(cert_der).into()
}

pub fn format_fingerprint(fingerprint: &Sha256Fingerprint) -> String {
    fingerprint
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

fn pin_kind_label(kind: PinKind) -> &'static str {
    match kind {
        PinKind::CertificateSha256 => "certificate_sha256",
        PinKind::SubjectPublicKeyInfoSha256 => "spki_sha256",
    }
}

pub fn parse_fingerprint(value: &str) -> Result<Sha256Fingerprint, TlsTrustError> {
    let compact: String = value
        .chars()
        .filter(|ch| !matches!(ch, ':' | '-' | ' ' | '\n' | '\t'))
        .collect();
    if compact.len() != 64 || !compact.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return Err(TlsTrustError::InvalidFingerprint(value.to_string()));
    }

    let mut out = [0u8; 32];
    for (index, chunk) in compact.as_bytes().chunks_exact(2).enumerate() {
        let hex = std::str::from_utf8(chunk)
            .map_err(|_| TlsTrustError::InvalidFingerprint(value.to_string()))?;
        out[index] = u8::from_str_radix(hex, 16)
            .map_err(|_| TlsTrustError::InvalidFingerprint(value.to_string()))?;
    }
    Ok(out)
}

pub fn is_tofu_eligible_error(error: &RustlsError) -> bool {
    matches!(
        error,
        RustlsError::InvalidCertificate(CertificateError::UnknownIssuer)
    )
}

pub fn is_tofu_pin_mismatch_message(error: &str) -> bool {
    error.contains(TOFU_PIN_MISMATCH_ERROR)
}

pub fn is_tofu_capture_reject_message(error: &str) -> bool {
    error.contains(TOFU_CAPTURE_REJECT_ERROR)
}

#[cfg(feature = "wss-compat")]
fn custom_verifier_config(verifier: Arc<dyn ServerCertVerifier>) -> Arc<ClientConfig> {
    Arc::new(custom_verifier_client_config(verifier))
}

fn custom_verifier_client_config(verifier: Arc<dyn ServerCertVerifier>) -> ClientConfig {
    ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth()
}

fn native_roots() -> Result<RootCertStore, TlsTrustError> {
    native_roots_from_result(rustls_native_certs::load_native_certs())
}

fn native_roots_from_result(
    result: rustls_native_certs::CertificateResult,
) -> Result<RootCertStore, TlsTrustError> {
    let mut roots = RootCertStore::empty();
    let errors = result.errors;
    let (added, rejected) = roots.add_parsable_certificates(result.certs);
    if added == 0 {
        return if errors.is_empty() {
            Err(TlsTrustError::NoNativeRoots)
        } else {
            Err(TlsTrustError::NativeRoots(format!("{errors:?}")))
        };
    }
    if !errors.is_empty() || rejected != 0 {
        tracing::warn!(
            loader_error_count = errors.len(),
            rejected_certificate_count = rejected,
            usable_certificate_count = added,
            "some native root certificates could not be loaded"
        );
    }
    Ok(roots)
}

fn roots_with_private_ca(path: &PathBuf) -> Result<RootCertStore, TlsTrustError> {
    let file = File::open(path).map_err(|source| TlsTrustError::OpenCaBundle {
        path: path.clone(),
        source,
    })?;
    roots_with_private_ca_reader(path, BufReader::new(file))
}

fn roots_with_private_ca_reader(
    path: &PathBuf,
    mut reader: impl std::io::BufRead,
) -> Result<RootCertStore, TlsTrustError> {
    let mut roots = native_roots().unwrap_or_else(|_| RootCertStore::empty());
    let mut cert_count = 0usize;
    for cert in rustls_pemfile::certs(&mut reader) {
        let cert = cert.map_err(|source| TlsTrustError::ReadCaBundle {
            path: path.clone(),
            source,
        })?;
        roots
            .add(cert)
            .map_err(|_| TlsTrustError::AddCaCertificate(path.clone()))?;
        cert_count += 1;
    }
    if cert_count == 0 {
        return Err(TlsTrustError::EmptyCaBundle(path.clone()));
    }
    Ok(roots)
}

fn webpki_verifier(
    roots: RootCertStore,
) -> Result<Arc<dyn ServerCertVerifier>, rustls::client::VerifierBuilderError> {
    Ok(WebPkiServerVerifier::builder(Arc::new(roots)).build()?)
}

fn invalid_certificate(error: CertificateError) -> RustlsError {
    RustlsError::InvalidCertificate(error)
}

fn verify_tofu_leaf(
    end_entity: &CertificateDer<'_>,
    server_name: &ServerName<'_>,
    now: UnixTime,
) -> Result<(), RustlsError> {
    let parsed = ParsedCertificate::try_from(end_entity)?;
    verify_server_name(&parsed, server_name)?;
    validate_server_certificate_der(
        end_entity,
        &[],
        now.as_secs(),
        CertificateKeyPolicy::default(),
    )
    .map(|_| ())
    .map_err(|error| {
        invalid_certificate(match error {
            arcen_transport::tls::TlsError::CertificateNotYetValid => CertificateError::NotValidYet,
            arcen_transport::tls::TlsError::CertificateExpired => CertificateError::Expired,
            _ => CertificateError::ApplicationVerificationFailure,
        })
    })
}

fn cert_info(
    endpoint: &str,
    end_entity: &CertificateDer<'_>,
    server_name: &ServerName<'_>,
) -> Result<CertInfo, RustlsError> {
    let (remaining, certificate) = parse_x509_certificate(end_entity.as_ref())
        .map_err(|_| invalid_certificate(CertificateError::BadEncoding))?;
    if !remaining.is_empty() {
        return Err(invalid_certificate(CertificateError::BadEncoding));
    }
    let validity = certificate.validity();
    let not_before_epoch_secs = u64::try_from(validity.not_before.timestamp())
        .map_err(|_| invalid_certificate(CertificateError::ApplicationVerificationFailure))?;
    let not_after_epoch_secs = u64::try_from(validity.not_after.timestamp())
        .map_err(|_| invalid_certificate(CertificateError::ApplicationVerificationFailure))?;
    let cert_der_len = u32::try_from(end_entity.as_ref().len())
        .map_err(|_| invalid_certificate(CertificateError::BadEncoding))?;
    let certificate_digest = fingerprint_sha256(end_entity.as_ref());
    let spki_digest = fingerprint_sha256(certificate.tbs_certificate.subject_pki.raw);

    Ok(CertInfo {
        endpoint: if endpoint.is_empty() {
            server_name.to_str().into_owned()
        } else {
            endpoint.to_string()
        },
        server_name: server_name.to_str().into_owned(),
        // Filled in by the dial loop, which is the only place that knows which
        // of the resolved addresses this handshake was with.
        peer_address: None,
        certificate_sha256: TlsPin::new(PinKind::CertificateSha256, certificate_digest),
        certificate_sha256_display: format_fingerprint(&certificate_digest),
        spki_sha256: TlsPin::new(PinKind::SubjectPublicKeyInfoSha256, spki_digest),
        spki_sha256_display: format_fingerprint(&spki_digest),
        not_before_epoch_secs,
        not_after_epoch_secs,
        cert_der_len,
    })
}

#[derive(Debug)]
struct PinningVerifier {
    pin: TlsPin,
    signature_verifier: Arc<dyn ServerCertVerifier>,
}

impl PinningVerifier {
    fn new(pin: TlsPin, signature_verifier: Arc<dyn ServerCertVerifier>) -> Self {
        Self {
            pin,
            signature_verifier,
        }
    }
}

impl ServerCertVerifier for PinningVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, RustlsError> {
        verify_tofu_leaf(end_entity, server_name, now)?;
        let info = cert_info("", end_entity, server_name)?;
        let candidate = match self.pin.kind {
            PinKind::CertificateSha256 => info.certificate_sha256,
            PinKind::SubjectPublicKeyInfoSha256 => info.spki_sha256,
        };
        let matched = self.pin.matches(&candidate);
        tracing::info!(
            target: crate::logging::target::TLS,
            pin_kind = pin_kind_label(self.pin.kind),
            certificate_sha256 = %info.certificate_sha256_display,
            spki_sha256 = %info.spki_sha256_display,
            not_before_epoch_secs = info.not_before_epoch_secs,
            not_after_epoch_secs = info.not_after_epoch_secs,
            "checked session TLS pin",
        );
        if matched {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(RustlsError::General(TOFU_PIN_MISMATCH_ERROR.to_string()))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        self.signature_verifier
            .verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        self.signature_verifier
            .verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.signature_verifier.supported_verify_schemes()
    }

    fn root_hint_subjects(&self) -> Option<&[DistinguishedName]> {
        self.signature_verifier.root_hint_subjects()
    }
}

#[derive(Debug)]
struct TofuVerifier {
    system_verifier: Arc<dyn ServerCertVerifier>,
    endpoint: String,
    captured_certificate: Arc<std::sync::Mutex<Option<CertInfo>>>,
}

impl TofuVerifier {
    fn new(
        system_verifier: Arc<dyn ServerCertVerifier>,
        endpoint: String,
        captured_certificate: Arc<std::sync::Mutex<Option<CertInfo>>>,
    ) -> Self {
        Self {
            system_verifier,
            endpoint,
            captured_certificate,
        }
    }
}

impl ServerCertVerifier for TofuVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, RustlsError> {
        match self.system_verifier.verify_server_cert(
            end_entity,
            intermediates,
            server_name,
            ocsp_response,
            now,
        ) {
            Ok(verified) => return Ok(verified),
            Err(error) if is_tofu_eligible_error(&error) => {
                verify_tofu_leaf(end_entity, server_name, now)?;
            }
            Err(error) => return Err(error),
        }

        let cert_info = cert_info(&self.endpoint, end_entity, server_name)?;
        tracing::info!(
            target: crate::logging::target::TLS,
            pin_kind = pin_kind_label(PinKind::SubjectPublicKeyInfoSha256),
            certificate_sha256 = %cert_info.certificate_sha256_display,
            spki_sha256 = %cert_info.spki_sha256_display,
            not_before_epoch_secs = cert_info.not_before_epoch_secs,
            not_after_epoch_secs = cert_info.not_after_epoch_secs,
            "captured untrusted TLS certificate",
        );
        *self
            .captured_certificate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(cert_info);
        Err(RustlsError::General(TOFU_CAPTURE_REJECT_ERROR.to_string()))
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        self.system_verifier
            .verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        self.system_verifier
            .verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.system_verifier.supported_verify_schemes()
    }

    fn root_hint_subjects(&self) -> Option<&[DistinguishedName]> {
        self.system_verifier.root_hint_subjects()
    }
}

#[derive(Debug)]
struct InsecureVerifier {
    signature_verifier: Arc<dyn ServerCertVerifier>,
}

impl InsecureVerifier {
    fn new(signature_verifier: Arc<dyn ServerCertVerifier>) -> Self {
        Self { signature_verifier }
    }
}

impl ServerCertVerifier for InsecureVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, RustlsError> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        self.signature_verifier
            .verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        self.signature_verifier
            .verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.signature_verifier.supported_verify_schemes()
    }

    fn root_hint_subjects(&self) -> Option<&[DistinguishedName]> {
        self.signature_verifier.root_hint_subjects()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcgen::{
        date_time_ymd, BasicConstraints, CertificateParams, DistinguishedName, DnType,
        ExtendedKeyUsagePurpose, IsCa, KeyPair,
    };
    use std::sync::Mutex;
    use std::time::Duration;

    #[derive(Debug)]
    struct UnknownIssuerVerifier;

    impl ServerCertVerifier for UnknownIssuerVerifier {
        fn verify_server_cert(
            &self,
            _end_entity: &CertificateDer<'_>,
            _intermediates: &[CertificateDer<'_>],
            _server_name: &ServerName<'_>,
            _ocsp_response: &[u8],
            _now: UnixTime,
        ) -> Result<ServerCertVerified, RustlsError> {
            Err(invalid_certificate(CertificateError::UnknownIssuer))
        }

        fn verify_tls12_signature(
            &self,
            _message: &[u8],
            _cert: &CertificateDer<'_>,
            _dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, RustlsError> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn verify_tls13_signature(
            &self,
            _message: &[u8],
            _cert: &CertificateDer<'_>,
            _dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, RustlsError> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            vec![SignatureScheme::ECDSA_NISTP256_SHA256]
        }
    }

    #[derive(Debug)]
    struct TrustedVerifier;

    impl ServerCertVerifier for TrustedVerifier {
        fn verify_server_cert(
            &self,
            _end_entity: &CertificateDer<'_>,
            _intermediates: &[CertificateDer<'_>],
            _server_name: &ServerName<'_>,
            _ocsp_response: &[u8],
            _now: UnixTime,
        ) -> Result<ServerCertVerified, RustlsError> {
            Ok(ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            _message: &[u8],
            _cert: &CertificateDer<'_>,
            _dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, RustlsError> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn verify_tls13_signature(
            &self,
            _message: &[u8],
            _cert: &CertificateDer<'_>,
            _dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, RustlsError> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            vec![SignatureScheme::ECDSA_NISTP256_SHA256]
        }
    }

    fn test_now() -> UnixTime {
        let timestamp = date_time_ymd(2026, 7, 1).unix_timestamp();
        UnixTime::since_unix_epoch(Duration::from_secs(timestamp.try_into().unwrap()))
    }

    fn server_name(name: &'static str) -> ServerName<'static> {
        ServerName::try_from(name).unwrap()
    }

    fn generated_leaf(
        subject_alt_names: &[&str],
        configure: impl FnOnce(&mut CertificateParams),
    ) -> CertificateDer<'static> {
        let key = KeyPair::generate().unwrap();
        generated_leaf_with_key(subject_alt_names, &key, configure)
    }

    fn generated_leaf_with_key(
        subject_alt_names: &[&str],
        key: &KeyPair,
        configure: impl FnOnce(&mut CertificateParams),
    ) -> CertificateDer<'static> {
        let mut params = CertificateParams::new(
            subject_alt_names
                .iter()
                .map(|name| (*name).to_string())
                .collect::<Vec<_>>(),
        )
        .unwrap();
        params.not_before = date_time_ymd(2026, 1, 1);
        params.not_after = date_time_ymd(2027, 1, 1);
        params
            .extended_key_usages
            .push(ExtendedKeyUsagePurpose::ServerAuth);
        configure(&mut params);

        params.self_signed(key).unwrap().der().clone()
    }

    fn capture() -> Arc<Mutex<Option<CertInfo>>> {
        Arc::new(Mutex::new(None))
    }

    fn tofu_verifier(captured: Arc<Mutex<Option<CertInfo>>>) -> TofuVerifier {
        TofuVerifier::new(
            Arc::new(UnknownIssuerVerifier),
            "tofu.example.test:18444".to_string(),
            captured,
        )
    }

    fn assert_rejected_before_tofu(
        verifier: &TofuVerifier,
        captured: &Mutex<Option<CertInfo>>,
        cert: &CertificateDer<'_>,
        name: &ServerName<'_>,
        expected_error: CertificateError,
    ) {
        let error = verifier
            .verify_server_cert(cert, &[], name, &[], test_now())
            .unwrap_err();
        if expected_error == CertificateError::NotValidForName {
            assert!(matches!(
                error,
                RustlsError::InvalidCertificate(
                    CertificateError::NotValidForName
                        | CertificateError::NotValidForNameContext { .. }
                )
            ));
        } else {
            assert_eq!(error, invalid_certificate(expected_error));
        }
        assert!(captured.lock().unwrap().is_none());
    }

    #[test]
    fn formats_and_parses_fingerprint() {
        let mut fingerprint = [0u8; 32];
        fingerprint[0] = 0xA3;
        fingerprint[1] = 0x8F;
        fingerprint[31] = 0x01;
        let formatted = format_fingerprint(&fingerprint);
        assert_eq!(formatted.len(), 95);
        assert_eq!(parse_fingerprint(&formatted).unwrap(), fingerprint);
        assert_eq!(
            parse_fingerprint(&formatted.replace(':', "")).unwrap(),
            fingerprint
        );
    }

    #[test]
    fn rejects_invalid_fingerprint() {
        assert!(parse_fingerprint("not-a-fingerprint").is_err());
        assert!(parse_fingerprint("AA:BB").is_err());
    }

    #[test]
    fn insecure_mode_is_double_gated() {
        std::env::remove_var(INSECURE_ENV_KEY);
        assert!(!insecure_tls_allowed(true));
        std::env::set_var(INSECURE_ENV_KEY, INSECURE_ENV_VALUE);
        assert!(insecure_tls_allowed(true));
        assert!(!insecure_tls_allowed(false));
        std::env::remove_var(INSECURE_ENV_KEY);
    }

    #[test]
    #[cfg(feature = "wss-compat")]
    fn system_and_private_ca_trust_paths_remain_available() {
        assert!(TlsTrustConfig::system_ca()
            .rustls_connector()
            .unwrap()
            .is_none());

        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let key = KeyPair::generate().unwrap();
        let ca = params.self_signed(&key).unwrap();
        let roots = roots_with_private_ca_reader(
            &PathBuf::from("in-memory-ca.pem"),
            std::io::Cursor::new(ca.pem()),
        )
        .unwrap();
        assert!(!roots.is_empty());
    }

    #[test]
    fn native_roots_keep_usable_certificates_when_loading_is_partially_successful() {
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let key = KeyPair::generate().unwrap();
        let ca = params.self_signed(&key).unwrap();

        let mut result = rustls_native_certs::CertificateResult::default();
        result.certs.push(ca.der().clone());
        result.errors.push(rustls_native_certs::Error {
            context: "test native root load",
            kind: rustls_native_certs::ErrorKind::Io {
                inner: std::io::Error::other("one source was unavailable"),
                path: PathBuf::from("unavailable-root-source"),
            },
        });

        let roots = native_roots_from_result(result).unwrap();
        assert_eq!(roots.len(), 1);
    }

    #[test]
    fn native_roots_report_loader_errors_when_none_are_usable() {
        let mut result = rustls_native_certs::CertificateResult::default();
        result.errors.push(rustls_native_certs::Error {
            context: "test native root load",
            kind: rustls_native_certs::ErrorKind::Io {
                inner: std::io::Error::other("native root store unavailable"),
                path: PathBuf::from("unavailable-root-source"),
            },
        });

        assert!(matches!(
            native_roots_from_result(result),
            Err(TlsTrustError::NativeRoots(_))
        ));
    }

    #[test]
    fn only_unknown_issuer_is_tofu_eligible() {
        assert!(is_tofu_eligible_error(&RustlsError::InvalidCertificate(
            CertificateError::UnknownIssuer
        )));
        assert!(!is_tofu_eligible_error(&RustlsError::InvalidCertificate(
            CertificateError::Expired
        )));
        assert!(!is_tofu_eligible_error(&RustlsError::InvalidCertificate(
            CertificateError::NotValidForName
        )));
    }

    #[test]
    fn system_trusted_certificate_bypasses_capture_probe() {
        let cert = generated_leaf(&["tofu.example.test"], |_| {});
        let captured = capture();
        let verifier = TofuVerifier::new(
            Arc::new(TrustedVerifier),
            "tofu.example.test:18444".to_string(),
            captured.clone(),
        );

        assert!(verifier
            .verify_server_cert(
                &cert,
                &[],
                &server_name("tofu.example.test"),
                &[],
                test_now(),
            )
            .is_ok());
        assert!(captured.lock().unwrap().is_none());
    }

    #[test]
    fn valid_self_signed_dns_and_ip_sans_reach_tofu_decision() {
        let cert = generated_leaf(&["tofu.example.test", "192.0.2.10"], |_| {});
        let captured = capture();
        let verifier = tofu_verifier(captured.clone());

        let error = verifier
            .verify_server_cert(
                &cert,
                &[],
                &server_name("tofu.example.test"),
                &[],
                test_now(),
            )
            .unwrap_err();
        assert!(is_tofu_capture_reject_message(&error.to_string()));
        assert_eq!(
            captured.lock().unwrap().take().unwrap().server_name,
            "tofu.example.test"
        );
        let error = verifier
            .verify_server_cert(&cert, &[], &server_name("192.0.2.10"), &[], test_now())
            .unwrap_err();
        assert!(is_tofu_capture_reject_message(&error.to_string()));
        assert_eq!(
            captured.lock().unwrap().take().unwrap().server_name,
            "192.0.2.10"
        );
    }

    #[test]
    fn wrong_dns_and_ip_names_are_rejected_before_tofu() {
        let dns_cert = generated_leaf(&["tofu.example.test"], |_| {});
        let ip_cert = generated_leaf(&["192.0.2.10"], |_| {});
        let captured = capture();
        let verifier = tofu_verifier(captured.clone());

        assert_rejected_before_tofu(
            &verifier,
            &captured,
            &dns_cert,
            &server_name("other.example.test"),
            CertificateError::NotValidForName,
        );
        assert_rejected_before_tofu(
            &verifier,
            &captured,
            &ip_cert,
            &server_name("192.0.2.11"),
            CertificateError::NotValidForName,
        );
    }

    #[test]
    fn expired_and_not_yet_valid_certificates_are_rejected_before_tofu() {
        let expired = generated_leaf(&["tofu.example.test"], |params| {
            params.not_before = date_time_ymd(2024, 1, 1);
            params.not_after = date_time_ymd(2025, 1, 1);
        });
        let not_yet_valid = generated_leaf(&["tofu.example.test"], |params| {
            params.not_before = date_time_ymd(2027, 1, 1);
            params.not_after = date_time_ymd(2028, 1, 1);
        });
        let captured = capture();
        let verifier = tofu_verifier(captured.clone());
        let name = server_name("tofu.example.test");

        assert_rejected_before_tofu(
            &verifier,
            &captured,
            &expired,
            &name,
            CertificateError::Expired,
        );
        assert_rejected_before_tofu(
            &verifier,
            &captured,
            &not_yet_valid,
            &name,
            CertificateError::NotValidYet,
        );
    }

    #[test]
    fn ca_and_non_server_auth_certificates_are_rejected_before_tofu() {
        let ca = generated_leaf(&["tofu.example.test"], |params| {
            params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        });
        let client_only = generated_leaf(&["tofu.example.test"], |params| {
            params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        });
        let any_usage = generated_leaf(&["tofu.example.test"], |params| {
            params.extended_key_usages = vec![ExtendedKeyUsagePurpose::Any];
        });
        let captured = capture();
        let verifier = tofu_verifier(captured.clone());
        let name = server_name("tofu.example.test");

        assert_rejected_before_tofu(
            &verifier,
            &captured,
            &ca,
            &name,
            CertificateError::ApplicationVerificationFailure,
        );
        assert_rejected_before_tofu(
            &verifier,
            &captured,
            &client_only,
            &name,
            CertificateError::ApplicationVerificationFailure,
        );
        assert_rejected_before_tofu(
            &verifier,
            &captured,
            &any_usage,
            &name,
            CertificateError::ApplicationVerificationFailure,
        );
    }

    #[test]
    fn cn_only_certificate_is_rejected_like_rustls_webpki() {
        let cert = generated_leaf(&[], |params| {
            params.distinguished_name = DistinguishedName::new();
            params
                .distinguished_name
                .push(DnType::CommonName, "tofu.example.test");
        });
        let captured = capture();
        let verifier = tofu_verifier(captured.clone());

        assert_rejected_before_tofu(
            &verifier,
            &captured,
            &cert,
            &server_name("tofu.example.test"),
            CertificateError::NotValidForName,
        );
    }

    #[test]
    fn webpki_wildcard_rules_apply_before_tofu() {
        let cert = generated_leaf(&["*.example.test"], |_| {});
        let accepted_capture = capture();
        let accepted_verifier = tofu_verifier(accepted_capture.clone());

        let error = accepted_verifier
            .verify_server_cert(
                &cert,
                &[],
                &server_name("host.example.test"),
                &[],
                test_now(),
            )
            .unwrap_err();
        assert!(is_tofu_capture_reject_message(&error.to_string()));
        assert!(accepted_capture.lock().unwrap().take().is_some());

        for invalid_name in ["nested.host.example.test", "example.test"] {
            let rejected_capture = capture();
            let rejected_verifier = tofu_verifier(rejected_capture.clone());
            assert_rejected_before_tofu(
                &rejected_verifier,
                &rejected_capture,
                &cert,
                &server_name(invalid_name),
                CertificateError::NotValidForName,
            );
        }
    }

    #[test]
    fn valid_tofu_retries_repeat_leaf_validation() {
        let cert = generated_leaf(&["tofu.example.test"], |_| {});
        let captured = capture();
        let verifier = tofu_verifier(captured.clone());
        let name = server_name("tofu.example.test");

        for _ in 0..2 {
            let error = verifier
                .verify_server_cert(&cert, &[], &name, &[], test_now())
                .unwrap_err();
            assert!(is_tofu_capture_reject_message(&error.to_string()));
            assert!(captured.lock().unwrap().take().is_some());
        }
    }

    #[test]
    fn pinned_certificate_still_requires_a_valid_leaf() {
        let expired = generated_leaf(&["tofu.example.test"], |params| {
            params.not_before = date_time_ymd(2024, 1, 1);
            params.not_after = date_time_ymd(2025, 1, 1);
        });
        let verifier = PinningVerifier::new(
            TlsPin::new(
                PinKind::CertificateSha256,
                fingerprint_sha256(expired.as_ref()),
            ),
            Arc::new(UnknownIssuerVerifier),
        );

        let error = verifier
            .verify_server_cert(
                &expired,
                &[],
                &server_name("tofu.example.test"),
                &[],
                test_now(),
            )
            .unwrap_err();
        assert_eq!(error, invalid_certificate(CertificateError::Expired));
    }

    #[test]
    fn cert_info_reports_exact_hashes_validity_and_endpoint() {
        let accepted = generated_leaf(&["tofu.example.test"], |_| {});
        let captured = capture();
        let error = tofu_verifier(captured.clone())
            .verify_server_cert(
                &accepted,
                &[],
                &server_name("tofu.example.test"),
                &[],
                test_now(),
            )
            .unwrap_err();
        assert!(is_tofu_capture_reject_message(&error.to_string()));
        let info = captured.lock().unwrap().take().unwrap();
        let (_, parsed) = parse_x509_certificate(accepted.as_ref()).unwrap();
        assert_eq!(info.endpoint, "tofu.example.test:18444");
        assert_eq!(
            info.certificate_sha256,
            TlsPin::new(
                PinKind::CertificateSha256,
                fingerprint_sha256(accepted.as_ref())
            )
        );
        assert_eq!(
            info.spki_sha256,
            TlsPin::new(
                PinKind::SubjectPublicKeyInfoSha256,
                fingerprint_sha256(parsed.tbs_certificate.subject_pki.raw)
            )
        );
        assert_eq!(
            info.not_before_epoch_secs,
            u64::try_from(parsed.validity().not_before.timestamp()).unwrap()
        );
        assert_eq!(
            info.not_after_epoch_secs,
            u64::try_from(parsed.validity().not_after.timestamp()).unwrap()
        );
        assert_eq!(
            info.cert_der_len,
            u32::try_from(accepted.as_ref().len()).unwrap()
        );
    }

    #[test]
    fn certificate_and_spki_pins_have_renewal_semantics() {
        let key = KeyPair::generate().unwrap();
        let accepted = generated_leaf_with_key(&["tofu.example.test"], &key, |_| {});
        let renewed = generated_leaf_with_key(&["tofu.example.test"], &key, |params| {
            params.not_before = date_time_ymd(2026, 2, 1);
            params.not_after = date_time_ymd(2027, 2, 1);
        });
        let rekeyed = generated_leaf(&["tofu.example.test"], |_| {});
        let info = cert_info("", &accepted, &server_name("tofu.example.test")).unwrap();
        let certificate_verifier =
            PinningVerifier::new(info.certificate_sha256, Arc::new(UnknownIssuerVerifier));
        let spki_verifier = PinningVerifier::new(info.spki_sha256, Arc::new(UnknownIssuerVerifier));
        let name = server_name("tofu.example.test");

        assert!(certificate_verifier
            .verify_server_cert(&accepted, &[], &name, &[], test_now())
            .is_ok());
        let error = certificate_verifier
            .verify_server_cert(&renewed, &[], &name, &[], test_now())
            .unwrap_err();
        assert!(is_tofu_pin_mismatch_message(&error.to_string()));
        assert!(spki_verifier
            .verify_server_cert(&renewed, &[], &name, &[], test_now())
            .is_ok());
        let error = spki_verifier
            .verify_server_cert(&rekeyed, &[], &name, &[], test_now())
            .unwrap_err();
        assert!(is_tofu_pin_mismatch_message(&error.to_string()));
    }

    #[test]
    fn bookmark_legacy_and_typed_pins_validate() {
        let digest = [0x42; 32];
        let display = format_fingerprint(&digest);
        let legacy_json = serde_json::json!({
            "mode": "tofu_pinned",
            "pinned_fingerprint_sha256": display,
            "pinned_at": null,
            "pinned_label": null,
            "ca_bundle_path": null,
            "session_only": false
        });
        let legacy: TlsBookmarkConfig = serde_json::from_value(legacy_json).unwrap();
        assert_eq!(
            legacy.validated_pin().unwrap(),
            Some(TlsPin::new(PinKind::CertificateSha256, digest))
        );

        let spki = TlsBookmarkConfig {
            mode: BookmarkTlsMode::TofuPinned,
            pin_kind: Some(BookmarkPinKind::SpkiSha256),
            pinned_spki_sha256: Some(format_fingerprint(&digest)),
            ..TlsBookmarkConfig::default()
        };
        assert_eq!(
            spki.validated_pin().unwrap(),
            Some(TlsPin::new(PinKind::SubjectPublicKeyInfoSha256, digest))
        );
    }

    #[test]
    fn bookmark_conflicting_and_incomplete_pins_are_rejected() {
        let display = format_fingerprint(&[0x42; 32]);
        for config in [
            TlsBookmarkConfig {
                mode: BookmarkTlsMode::TofuPinned,
                ..TlsBookmarkConfig::default()
            },
            TlsBookmarkConfig {
                mode: BookmarkTlsMode::TofuPinned,
                pin_kind: Some(BookmarkPinKind::SpkiSha256),
                pinned_fingerprint_sha256: Some(display.clone()),
                pinned_spki_sha256: Some(display.clone()),
                ..TlsBookmarkConfig::default()
            },
            TlsBookmarkConfig {
                mode: BookmarkTlsMode::Auto,
                pinned_fingerprint_sha256: Some(display.clone()),
                ..TlsBookmarkConfig::default()
            },
        ] {
            assert!(matches!(
                config.validated_pin(),
                Err(TlsTrustError::InvalidBookmarkPin(_))
            ));
        }
    }
}
