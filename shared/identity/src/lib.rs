//! Provider-neutral identity and signed session-grant validation contracts.

#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used))]

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::error::Error;
use std::fmt::{Debug, Display, Formatter};

mod direct_resume;

pub use direct_resume::*;

/// Maximum accepted lifetime for an Arcen session grant.
pub const MAX_SESSION_GRANT_LIFETIME_SECONDS: u64 = 300;
/// Current session-grant claims schema version.
pub const SESSION_GRANT_VERSION_V1: u16 = 1;
/// Maximum bytes accepted for any grant identity or nonce value.
pub const MAX_GRANT_BINDING_VALUE_BYTES: usize = 512;
/// Maximum exact UTF-8 disclaimer content accepted from an operator file.
pub const MAX_DISCLAIMER_CONTENT_BYTES: usize = 16 * 1024;
/// Maximum bytes accepted for a locale identifier.
pub const MAX_DISCLAIMER_LOCALE_BYTES: usize = 64;

/// Bounded path-safe locale identifier selected by the host operator.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DisclaimerLocale(String);

impl DisclaimerLocale {
    /// Validates a locale such as `en_US`.
    ///
    /// # Errors
    ///
    /// Returns an error for empty, oversized, non-ASCII, dotted, traversal-like,
    /// separator-containing, or empty-segment identifiers.
    pub fn new(value: impl Into<String>) -> Result<Self, DisclaimerError> {
        let value = value.into();
        if value.is_empty() {
            return Err(DisclaimerError::EmptyLocale);
        }
        if value.len() > MAX_DISCLAIMER_LOCALE_BYTES {
            return Err(DisclaimerError::LocaleTooLong);
        }
        if !value.is_ascii() {
            return Err(DisclaimerError::UnsafeLocale);
        }
        let mut segment_len = 0_usize;
        for byte in value.bytes() {
            match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' => segment_len += 1,
                b'_' | b'-' if segment_len > 0 => segment_len = 0,
                _ => return Err(DisclaimerError::UnsafeLocale),
            }
        }
        if segment_len == 0 {
            return Err(DisclaimerError::UnsafeLocale);
        }
        Ok(Self(value))
    }

    /// Returns the validated locale.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Fixed SHA-256 digest of the exact disclaimer bytes sent on the wire.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DisclaimerDigest([u8; 32]);

impl DisclaimerDigest {
    /// Parses exactly 64 lowercase hexadecimal characters.
    ///
    /// # Errors
    ///
    /// Returns an error unless the input is canonical lowercase SHA-256 hex.
    pub fn parse_lower_hex(value: &str) -> Result<Self, DisclaimerError> {
        if value.len() != 64 {
            return Err(DisclaimerError::InvalidDigest);
        }
        let mut bytes = [0_u8; 32];
        for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
            let high = lower_hex_nibble(pair[0]).ok_or(DisclaimerError::InvalidDigest)?;
            let low = lower_hex_nibble(pair[1]).ok_or(DisclaimerError::InvalidDigest)?;
            bytes[index] = (high << 4) | low;
        }
        Ok(Self(bytes))
    }

    /// Returns the fixed digest bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Formats canonical lowercase hexadecimal.
    #[must_use]
    pub fn to_lower_hex(self) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut output = String::with_capacity(64);
        for byte in self.0 {
            output.push(char::from(HEX[usize::from(byte >> 4)]));
            output.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
        output
    }

    /// Compares fixed digest bytes without data-dependent early exit.
    #[must_use]
    pub fn matches(self, other: Self) -> bool {
        self.0
            .iter()
            .zip(other.0)
            .fold(0_u8, |difference, (left, right)| {
                difference | (*left ^ right)
            })
            == 0
    }
}

impl Debug for DisclaimerDigest {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.to_lower_hex())
    }
}

fn lower_hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

/// Validated exact disclaimer text prepared once before a host starts listening.
#[derive(Clone, PartialEq, Eq)]
pub struct PreparedDisclaimer {
    locale: DisclaimerLocale,
    text: String,
    digest: DisclaimerDigest,
}

impl PreparedDisclaimer {
    /// Validates exact file bytes and computes their digest.
    ///
    /// # Errors
    ///
    /// Returns an error for empty, invalid UTF-8, or oversized content.
    pub fn from_bytes(locale: DisclaimerLocale, content: &[u8]) -> Result<Self, DisclaimerError> {
        if content.is_empty() {
            return Err(DisclaimerError::EmptyContent);
        }
        if content.len() > MAX_DISCLAIMER_CONTENT_BYTES {
            return Err(DisclaimerError::ContentTooLarge);
        }
        let text = std::str::from_utf8(content)
            .map_err(|_| DisclaimerError::InvalidUtf8)?
            .to_owned();
        let digest = DisclaimerDigest(Sha256::digest(content).into());
        Ok(Self {
            locale,
            text,
            digest,
        })
    }

    /// Returns the selected locale.
    #[must_use]
    pub const fn locale(&self) -> &DisclaimerLocale {
        &self.locale
    }

    /// Returns the exact validated text sent to the client.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Returns the digest of the exact text bytes.
    #[must_use]
    pub const fn digest(&self) -> DisclaimerDigest {
        self.digest
    }

    /// Parses and compares a client acknowledgment.
    ///
    /// # Errors
    ///
    /// Returns an error when the acknowledgment is not canonical lowercase hex.
    pub fn matches_acknowledgment(&self, value: &str) -> Result<bool, DisclaimerError> {
        Ok(self
            .digest
            .matches(DisclaimerDigest::parse_lower_hex(value)?))
    }
}

impl Debug for PreparedDisclaimer {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreparedDisclaimer")
            .field("locale", &self.locale)
            .field("text", &"<redacted>")
            .field("digest", &self.digest)
            .finish()
    }
}

/// Standalone operational evidence created only after successful OS authentication.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisclaimerAcceptance {
    locale: DisclaimerLocale,
    digest: DisclaimerDigest,
    accepted_at_epoch_seconds: u64,
}

impl DisclaimerAcceptance {
    /// Creates evidence for a previously prepared and acknowledged disclaimer.
    #[must_use]
    pub fn new(disclaimer: &PreparedDisclaimer, accepted_at_epoch_seconds: u64) -> Self {
        Self {
            locale: disclaimer.locale.clone(),
            digest: disclaimer.digest,
            accepted_at_epoch_seconds,
        }
    }

    /// Returns the locale.
    #[must_use]
    pub const fn locale(&self) -> &DisclaimerLocale {
        &self.locale
    }

    /// Returns the exact-content digest.
    #[must_use]
    pub const fn digest(&self) -> DisclaimerDigest {
        self.digest
    }

    /// Returns the host-recorded acceptance time.
    #[must_use]
    pub const fn accepted_at_epoch_seconds(&self) -> u64 {
        self.accepted_at_epoch_seconds
    }
}

/// Disclaimer validation failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisclaimerError {
    /// Locale was empty.
    EmptyLocale,
    /// Locale exceeded its fixed bound.
    LocaleTooLong,
    /// Locale was not a safe bounded ASCII identifier.
    UnsafeLocale,
    /// Disclaimer content was empty.
    EmptyContent,
    /// Disclaimer content exceeded 16 KiB.
    ContentTooLarge,
    /// Disclaimer content was not valid UTF-8.
    InvalidUtf8,
    /// Digest was not canonical lowercase SHA-256 hexadecimal.
    InvalidDigest,
}

impl Display for DisclaimerError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::EmptyLocale => "disclaimer locale is empty",
            Self::LocaleTooLong => "disclaimer locale exceeds its bound",
            Self::UnsafeLocale => "disclaimer locale is not a safe ASCII identifier",
            Self::EmptyContent => "disclaimer content is empty",
            Self::ContentTooLarge => "disclaimer content exceeds 16 KiB",
            Self::InvalidUtf8 => "disclaimer content is not valid UTF-8",
            Self::InvalidDigest => "disclaimer digest is not lowercase SHA-256 hexadecimal",
        })
    }
}

impl Error for DisclaimerError {}

/// Provider-neutral OIDC validation configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OidcConfiguration {
    issuer: String,
    audiences: BTreeSet<String>,
    required_tenant: Option<String>,
}

impl OidcConfiguration {
    /// Creates an OIDC configuration from standards-based values.
    ///
    /// Microsoft Entra is represented by its issuer, audience, and optional
    /// tenant values; it is not a distinct shared wire protocol.
    ///
    /// # Errors
    ///
    /// Returns an error when the issuer or accepted audience set is empty.
    pub fn new(
        issuer: impl Into<String>,
        audiences: impl IntoIterator<Item = String>,
        required_tenant: Option<String>,
    ) -> Result<Self, IdentityValidationError> {
        let issuer = issuer.into();
        if issuer.is_empty() {
            return Err(IdentityValidationError::EmptyIssuer);
        }
        let audiences = audiences.into_iter().collect::<BTreeSet<_>>();
        if audiences.is_empty() || audiences.contains("") {
            return Err(IdentityValidationError::EmptyAudience);
        }
        Ok(Self {
            issuer,
            audiences,
            required_tenant,
        })
    }

    /// Returns the configured issuer.
    #[must_use]
    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    /// Returns accepted audiences.
    #[must_use]
    pub fn audiences(&self) -> &BTreeSet<String> {
        &self.audiences
    }

    /// Returns the required tenant, when tenant restriction is configured.
    #[must_use]
    pub fn required_tenant(&self) -> Option<&str> {
        self.required_tenant.as_deref()
    }
}

/// Untrusted provider-neutral OIDC claims.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OidcClaims {
    /// Issuer claim.
    pub issuer: String,
    /// Audience claims.
    pub audiences: Vec<String>,
    /// Provider subject identifier.
    pub subject: String,
    /// Tenant or organization identifier, when supplied by the provider.
    pub tenant: Option<String>,
    /// Issued-at epoch seconds.
    pub issued_at: u64,
    /// Expiry epoch seconds.
    pub expires_at: u64,
}

/// Validated online identity evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedIdentity {
    issuer: String,
    subject: String,
    tenant: Option<String>,
    authenticated_at: u64,
    expires_at: u64,
}

impl ValidatedIdentity {
    /// Returns the OIDC issuer.
    #[must_use]
    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    /// Returns the provider subject.
    #[must_use]
    pub fn subject(&self) -> &str {
        &self.subject
    }

    /// Returns the tenant claim when present.
    #[must_use]
    pub fn tenant(&self) -> Option<&str> {
        self.tenant.as_deref()
    }

    /// Returns the provider authentication time.
    #[must_use]
    pub const fn authenticated_at(&self) -> u64 {
        self.authenticated_at
    }

    /// Returns the provider claim expiry.
    #[must_use]
    pub const fn expires_at(&self) -> u64 {
        self.expires_at
    }
}

/// Validates untrusted OIDC claims against local issuer, audience, tenant, and
/// clock policy.
///
/// # Errors
///
/// Returns a precise claim validation failure.
pub fn validate_oidc_claims(
    configuration: &OidcConfiguration,
    claims: &OidcClaims,
    now_epoch_seconds: u64,
) -> Result<ValidatedIdentity, IdentityValidationError> {
    if claims.issuer != configuration.issuer {
        return Err(IdentityValidationError::IssuerMismatch);
    }
    if !claims
        .audiences
        .iter()
        .any(|audience| configuration.audiences.contains(audience))
    {
        return Err(IdentityValidationError::AudienceMismatch);
    }
    if claims.subject.is_empty() {
        return Err(IdentityValidationError::EmptySubject);
    }
    if claims.issued_at > now_epoch_seconds {
        return Err(IdentityValidationError::IssuedInFuture);
    }
    if claims.expires_at <= now_epoch_seconds {
        return Err(IdentityValidationError::Expired);
    }
    if configuration
        .required_tenant
        .as_ref()
        .is_some_and(|required_tenant| claims.tenant.as_deref() != Some(required_tenant.as_str()))
    {
        return Err(IdentityValidationError::TenantMismatch);
    }
    Ok(ValidatedIdentity {
        issuer: claims.issuer.clone(),
        subject: claims.subject.clone(),
        tenant: claims.tenant.clone(),
        authenticated_at: claims.issued_at,
        expires_at: claims.expires_at,
    })
}

macro_rules! grant_binding_value {
    ($name:ident, $description:literal, $empty_error:ident) => {
        #[doc = $description]
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// Creates a bounded, non-empty grant binding value.
            ///
            /// # Errors
            ///
            /// Returns an error when the value is empty or exceeds the shared bound.
            pub fn new(value: impl Into<String>) -> Result<Self, IdentityValidationError> {
                let value = value.into();
                validate_grant_binding_value(&value, IdentityValidationError::$empty_error)?;
                Ok(Self(value))
            }

            /// Returns the binding value.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
    };
}

grant_binding_value!(
    ClientIdentity,
    "Authenticated client identity bound to a session grant.",
    EmptyClientIdentity
);
grant_binding_value!(
    HostIdentity,
    "Target Host identity bound to a session grant.",
    EmptyHostIdentity
);
grant_binding_value!(
    GrantNonce,
    "Single-use nonce bound to a session grant.",
    EmptyNonce
);

fn validate_grant_binding_value(
    value: &str,
    empty_error: IdentityValidationError,
) -> Result<(), IdentityValidationError> {
    if value.is_empty() {
        return Err(empty_error);
    }
    if value.len() > MAX_GRANT_BINDING_VALUE_BYTES {
        return Err(IdentityValidationError::GrantBindingValueTooLong);
    }
    Ok(())
}

/// Claims carried by a short-lived Arcen session grant.
///
/// Version 1 is intentionally strict: the version and all binding dimensions are
/// mandatory, and unknown JSON fields are rejected. Unversioned grants are not
/// interpreted as version 1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionGrantClaims {
    /// Claims schema version.
    pub version: u16,
    /// Arcen grant issuer.
    pub issuer: String,
    /// Intended Arcen service audience.
    pub audience: String,
    /// OIDC subject bound to the grant.
    pub subject: String,
    /// Tenant bound to the grant.
    pub tenant: Option<String>,
    /// Authenticated client identity.
    pub client_identity: ClientIdentity,
    /// Target Host identity.
    pub host_identity: HostIdentity,
    /// Arcen active-session identifier.
    pub session_id: String,
    /// Single-use replay nonce.
    pub nonce: GrantNonce,
    /// Issued-at epoch seconds.
    pub issued_at: u64,
    /// Expiry epoch seconds.
    pub expires_at: u64,
}

/// Signed Arcen session grant.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedSessionGrant {
    /// Claims to verify and validate.
    pub claims: SessionGrantClaims,
    /// Signing key identifier selected by deployment policy.
    pub key_id: String,
    /// Registered signature algorithm name.
    pub algorithm: String,
    /// Opaque signature bytes. Cryptographic verification belongs to an adapter.
    pub signature: Vec<u8>,
}

impl Debug for SignedSessionGrant {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SignedSessionGrant")
            .field("claims", &self.claims)
            .field("key_id", &self.key_id)
            .field("algorithm", &self.algorithm)
            .field("signature", &"<redacted>")
            .finish()
    }
}

/// Local expectations for an Arcen session grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GrantValidationContext<'a> {
    /// Required Arcen issuer.
    pub issuer: &'a str,
    /// Required service audience.
    pub audience: &'a str,
    /// Required OIDC subject.
    pub expected_subject: &'a str,
    /// Required tenant. `None` requires a tenantless grant.
    pub expected_tenant: Option<&'a str>,
    /// Required authenticated client.
    pub expected_client_identity: &'a ClientIdentity,
    /// Required target Host.
    pub expected_host_identity: &'a HostIdentity,
    /// Required active-session binding.
    pub expected_session_id: &'a str,
    /// Required single-use nonce.
    pub expected_nonce: &'a GrantNonce,
    /// Current epoch seconds.
    pub now_epoch_seconds: u64,
}

/// Validated grant evidence safe to pass into replay and admission contracts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedSessionGrant {
    version: u16,
    issuer: String,
    audience: String,
    subject: String,
    tenant: Option<String>,
    client_identity: ClientIdentity,
    host_identity: HostIdentity,
    session_id: String,
    nonce: GrantNonce,
    expires_at: u64,
}

/// Non-cloneable evidence that a validated grant nonce was atomically consumed.
#[derive(Debug, PartialEq, Eq)]
pub struct ConsumedSessionGrant {
    grant: ValidatedSessionGrant,
    consumed_at: u64,
}

impl ConsumedSessionGrant {
    /// Returns validated grant evidence.
    #[must_use]
    pub const fn validated(&self) -> &ValidatedSessionGrant {
        &self.grant
    }

    /// Returns the replay-consumption time.
    #[must_use]
    pub const fn consumed_at(&self) -> u64 {
        self.consumed_at
    }

    /// Consumes the one-time evidence and returns its validated grant.
    #[must_use]
    pub fn into_validated(self) -> ValidatedSessionGrant {
        self.grant
    }
}

impl ValidatedSessionGrant {
    /// Returns the claims schema version.
    #[must_use]
    pub const fn version(&self) -> u16 {
        self.version
    }

    /// Returns the issuer.
    #[must_use]
    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    /// Returns the audience.
    #[must_use]
    pub fn audience(&self) -> &str {
        &self.audience
    }

    /// Returns the OIDC subject.
    #[must_use]
    pub fn subject(&self) -> &str {
        &self.subject
    }

    /// Returns the tenant.
    #[must_use]
    pub fn tenant(&self) -> Option<&str> {
        self.tenant.as_deref()
    }

    /// Returns the authenticated client identity.
    #[must_use]
    pub const fn client_identity(&self) -> &ClientIdentity {
        &self.client_identity
    }

    /// Returns the target Host identity.
    #[must_use]
    pub const fn host_identity(&self) -> &HostIdentity {
        &self.host_identity
    }

    /// Returns the active-session identifier.
    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Returns the single-use nonce.
    #[must_use]
    pub const fn nonce(&self) -> &GrantNonce {
        &self.nonce
    }

    /// Returns the grant expiry.
    #[must_use]
    pub const fn expires_at(&self) -> u64 {
        self.expires_at
    }

    /// Returns the durable replay key.
    #[must_use]
    pub fn replay_key(&self) -> GrantReplayKey {
        GrantReplayKey {
            issuer: self.issuer.clone(),
            nonce: self.nonce.clone(),
        }
    }
}

/// Durable replay key scoped by issuer and nonce.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GrantReplayKey {
    issuer: String,
    nonce: GrantNonce,
}

impl GrantReplayKey {
    /// Returns the issuer namespace.
    #[must_use]
    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    /// Returns the single-use nonce.
    #[must_use]
    pub const fn nonce(&self) -> &GrantNonce {
        &self.nonce
    }
}

/// Atomic replay-consumption result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantReplayConsumption {
    /// This call durably consumed the key.
    Consumed,
    /// The key had already been consumed and remains protected.
    AlreadyConsumed,
}

/// Durable atomic one-time grant replay store.
///
/// Implementations used by multiple Gateway instances must share authoritative
/// state and atomically perform "insert if absent". A consumed key must remain
/// protected through `retain_until_epoch_seconds`; storage cleanup may occur only
/// after that instant.
pub trait GrantReplayConsumer: Send + Sync {
    /// Storage-specific failure.
    type Error;

    /// Atomically consumes a replay key once.
    ///
    /// # Errors
    ///
    /// Returns a storage error without reporting successful consumption.
    fn consume_once(
        &self,
        key: &GrantReplayKey,
        retain_until_epoch_seconds: u64,
        now_epoch_seconds: u64,
    ) -> Result<GrantReplayConsumption, Self::Error>;
}

/// Adapter boundary for deployment-selected signature verification.
pub trait GrantSignatureVerifier {
    /// Adapter-specific verification error.
    type Error;

    /// Verifies the opaque signature and configured algorithm/key policy.
    ///
    /// # Errors
    ///
    /// Returns the adapter error when signature verification fails.
    fn verify(&self, grant: &SignedSessionGrant) -> Result<(), Self::Error>;
}

/// Verifies a signature through the adapter, then validates short-lived claims.
///
/// # Errors
///
/// Returns either the verifier error or a precise claim-boundary failure.
pub fn validate_session_grant<V: GrantSignatureVerifier>(
    verifier: &V,
    grant: &SignedSessionGrant,
    context: GrantValidationContext<'_>,
) -> Result<ValidatedSessionGrant, GrantValidationError<V::Error>> {
    verifier
        .verify(grant)
        .map_err(GrantValidationError::Signature)?;
    let claims = &grant.claims;
    validate_grant_identity_bindings(claims, &context).map_err(GrantValidationError::Claims)?;
    validate_grant_timing(claims, context.now_epoch_seconds)
        .map_err(GrantValidationError::Claims)?;
    Ok(ValidatedSessionGrant {
        version: claims.version,
        issuer: claims.issuer.clone(),
        audience: claims.audience.clone(),
        subject: claims.subject.clone(),
        tenant: claims.tenant.clone(),
        client_identity: claims.client_identity.clone(),
        host_identity: claims.host_identity.clone(),
        session_id: claims.session_id.clone(),
        nonce: claims.nonce.clone(),
        expires_at: claims.expires_at,
    })
}

fn validate_grant_identity_bindings(
    claims: &SessionGrantClaims,
    context: &GrantValidationContext<'_>,
) -> Result<(), IdentityValidationError> {
    if claims.version != SESSION_GRANT_VERSION_V1 {
        return Err(IdentityValidationError::UnsupportedGrantVersion);
    }
    validate_grant_binding_value(&claims.issuer, IdentityValidationError::EmptyIssuer)?;
    if claims.issuer != context.issuer {
        return Err(IdentityValidationError::IssuerMismatch);
    }
    validate_grant_binding_value(&claims.audience, IdentityValidationError::EmptyAudience)?;
    if claims.audience != context.audience {
        return Err(IdentityValidationError::AudienceMismatch);
    }
    validate_grant_binding_value(&claims.subject, IdentityValidationError::EmptySubject)?;
    if claims.subject != context.expected_subject {
        return Err(IdentityValidationError::SubjectMismatch);
    }
    if claims.tenant.as_ref().is_some_and(String::is_empty) {
        return Err(IdentityValidationError::TenantMismatch);
    }
    if claims
        .tenant
        .as_ref()
        .is_some_and(|tenant| tenant.len() > MAX_GRANT_BINDING_VALUE_BYTES)
    {
        return Err(IdentityValidationError::GrantBindingValueTooLong);
    }
    if claims.tenant.as_deref() != context.expected_tenant {
        return Err(IdentityValidationError::TenantMismatch);
    }
    validate_grant_binding_value(
        claims.client_identity.as_str(),
        IdentityValidationError::EmptyClientIdentity,
    )?;
    if &claims.client_identity != context.expected_client_identity {
        return Err(IdentityValidationError::ClientIdentityMismatch);
    }
    validate_grant_binding_value(
        claims.host_identity.as_str(),
        IdentityValidationError::EmptyHostIdentity,
    )?;
    if &claims.host_identity != context.expected_host_identity {
        return Err(IdentityValidationError::HostIdentityMismatch);
    }
    validate_grant_binding_value(&claims.session_id, IdentityValidationError::EmptySessionId)?;
    if claims.session_id != context.expected_session_id {
        return Err(IdentityValidationError::SessionBindingMismatch);
    }
    validate_grant_binding_value(claims.nonce.as_str(), IdentityValidationError::EmptyNonce)?;
    if &claims.nonce != context.expected_nonce {
        return Err(IdentityValidationError::NonceMismatch);
    }
    Ok(())
}

fn validate_grant_timing(
    claims: &SessionGrantClaims,
    now_epoch_seconds: u64,
) -> Result<(), IdentityValidationError> {
    if claims.issued_at > now_epoch_seconds {
        return Err(IdentityValidationError::IssuedInFuture);
    }
    if claims.expires_at <= now_epoch_seconds {
        return Err(IdentityValidationError::Expired);
    }
    let lifetime = claims
        .expires_at
        .checked_sub(claims.issued_at)
        .ok_or(IdentityValidationError::InvalidLifetime)?;
    if lifetime == 0 || lifetime > MAX_SESSION_GRANT_LIFETIME_SECONDS {
        return Err(IdentityValidationError::InvalidLifetime);
    }
    Ok(())
}

/// Atomically consumes validated grant evidence.
///
/// # Errors
///
/// Returns an expiry, replay, or replay-store failure.
pub fn consume_validated_session_grant<C: GrantReplayConsumer>(
    consumer: &C,
    grant: &ValidatedSessionGrant,
    now_epoch_seconds: u64,
) -> Result<ConsumedSessionGrant, GrantReplayError<C::Error>> {
    if grant.expires_at <= now_epoch_seconds {
        return Err(GrantReplayError::Expired);
    }
    match consumer
        .consume_once(&grant.replay_key(), grant.expires_at, now_epoch_seconds)
        .map_err(GrantReplayError::Store)?
    {
        GrantReplayConsumption::Consumed => Ok(ConsumedSessionGrant {
            grant: grant.clone(),
            consumed_at: now_epoch_seconds,
        }),
        GrantReplayConsumption::AlreadyConsumed => Err(GrantReplayError::AlreadyConsumed),
    }
}

/// Identity claim validation failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityValidationError {
    /// Configuration issuer was empty.
    EmptyIssuer,
    /// Configuration audience was empty.
    EmptyAudience,
    /// Issuer did not match.
    IssuerMismatch,
    /// No accepted audience was present.
    AudienceMismatch,
    /// Subject was empty.
    EmptySubject,
    /// Tenant did not match deployment policy.
    TenantMismatch,
    /// Claims were issued in the future.
    IssuedInFuture,
    /// Claims were expired.
    Expired,
    /// Grant lifetime was zero or exceeded the short-lived policy.
    InvalidLifetime,
    /// Grant was bound to another active session.
    SessionBindingMismatch,
    /// Grant active-session identifier was empty.
    EmptySessionId,
    /// Grant schema version is unsupported.
    UnsupportedGrantVersion,
    /// Grant subject did not match the expected human.
    SubjectMismatch,
    /// Authenticated client identity was empty.
    EmptyClientIdentity,
    /// Authenticated client identity did not match.
    ClientIdentityMismatch,
    /// Target Host identity was empty.
    EmptyHostIdentity,
    /// Target Host identity did not match.
    HostIdentityMismatch,
    /// Grant nonce was empty.
    EmptyNonce,
    /// Grant nonce did not match.
    NonceMismatch,
    /// A grant binding value exceeded its shared bound.
    GrantBindingValueTooLong,
}

impl Display for IdentityValidationError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::EmptyIssuer => "OIDC issuer is empty",
            Self::EmptyAudience => "OIDC audience is empty",
            Self::IssuerMismatch => "issuer does not match",
            Self::AudienceMismatch => "audience does not match",
            Self::EmptySubject => "subject is empty",
            Self::TenantMismatch => "tenant does not match",
            Self::IssuedInFuture => "claims were issued in the future",
            Self::Expired => "claims are expired",
            Self::InvalidLifetime => "session grant lifetime is invalid",
            Self::SessionBindingMismatch => "session grant binding does not match",
            Self::EmptySessionId => "session grant active-session identifier is empty",
            Self::UnsupportedGrantVersion => "session grant version is unsupported",
            Self::SubjectMismatch => "subject does not match",
            Self::EmptyClientIdentity => "client identity is empty",
            Self::ClientIdentityMismatch => "client identity does not match",
            Self::EmptyHostIdentity => "Host identity is empty",
            Self::HostIdentityMismatch => "Host identity does not match",
            Self::EmptyNonce => "grant nonce is empty",
            Self::NonceMismatch => "grant nonce does not match",
            Self::GrantBindingValueTooLong => "grant binding value exceeds its bound",
        })
    }
}

impl Error for IdentityValidationError {}

/// Signed grant validation failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrantValidationError<E> {
    /// Cryptographic adapter rejected the signature.
    Signature(E),
    /// Signature was valid but claims violated local policy.
    Claims(IdentityValidationError),
}

/// Replay-consumption failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrantReplayError<E> {
    /// Grant expired before replay consumption.
    Expired,
    /// Grant nonce was already consumed.
    AlreadyConsumed,
    /// Durable replay storage failed.
    Store(E),
}

#[cfg(test)]
mod tests {
    use super::*;

    struct AcceptSignature;

    impl GrantSignatureVerifier for AcceptSignature {
        type Error = ();

        fn verify(&self, _grant: &SignedSessionGrant) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    fn grant() -> SignedSessionGrant {
        SignedSessionGrant {
            claims: SessionGrantClaims {
                version: SESSION_GRANT_VERSION_V1,
                issuer: "arcen".to_owned(),
                audience: "gateway".to_owned(),
                subject: "human".to_owned(),
                tenant: Some("tenant".to_owned()),
                client_identity: ClientIdentity::new("client-1").expect("client identity"),
                host_identity: HostIdentity::new("host-1").expect("Host identity"),
                session_id: "session-1".to_owned(),
                nonce: GrantNonce::new("nonce-1").expect("nonce"),
                issued_at: 100,
                expires_at: 200,
            },
            key_id: "key-1".to_owned(),
            algorithm: "deployment-selected".to_owned(),
            signature: vec![1, 2, 3],
        }
    }

    #[test]
    fn generic_oidc_configuration_accepts_entra_values_without_provider_semantics() {
        let configuration = OidcConfiguration::new(
            "https://login.microsoftonline.com/tenant/v2.0",
            ["arcen-client".to_owned()],
            Some("tenant".to_owned()),
        )
        .expect("valid configuration");
        let identity = validate_oidc_claims(
            &configuration,
            &OidcClaims {
                issuer: configuration.issuer().to_owned(),
                audiences: vec!["arcen-client".to_owned()],
                subject: "user-object-id".to_owned(),
                tenant: Some("tenant".to_owned()),
                issued_at: 100,
                expires_at: 200,
            },
            150,
        )
        .expect("valid claims");
        assert_eq!(identity.subject(), "user-object-id");
    }

    #[test]
    fn grant_requires_every_identity_and_session_binding_dimension() {
        let grant = grant();
        let client_identity = ClientIdentity::new("client-1").expect("client identity");
        let host_identity = HostIdentity::new("host-1").expect("Host identity");
        let nonce = GrantNonce::new("nonce-1").expect("nonce");
        let context = GrantValidationContext {
            issuer: "arcen",
            audience: "gateway",
            expected_subject: "human",
            expected_tenant: Some("tenant"),
            expected_client_identity: &client_identity,
            expected_host_identity: &host_identity,
            expected_session_id: "session-1",
            expected_nonce: &nonce,
            now_epoch_seconds: 150,
        };
        let validated =
            validate_session_grant(&AcceptSignature, &grant, context).expect("valid grant");
        assert_eq!(validated.client_identity(), &client_identity);
        assert_eq!(validated.host_identity(), &host_identity);
        assert_eq!(validated.nonce(), &nonce);

        let wrong_binding = GrantValidationContext {
            expected_session_id: "session-2",
            ..context
        };
        assert_eq!(
            validate_session_grant(&AcceptSignature, &grant, wrong_binding),
            Err(GrantValidationError::Claims(
                IdentityValidationError::SessionBindingMismatch
            ))
        );

        let wrong_subject = GrantValidationContext {
            expected_subject: "other-human",
            ..context
        };
        assert_eq!(
            validate_session_grant(&AcceptSignature, &grant, wrong_subject),
            Err(GrantValidationError::Claims(
                IdentityValidationError::SubjectMismatch
            ))
        );

        let wrong_tenant = GrantValidationContext {
            expected_tenant: None,
            ..context
        };
        assert_eq!(
            validate_session_grant(&AcceptSignature, &grant, wrong_tenant),
            Err(GrantValidationError::Claims(
                IdentityValidationError::TenantMismatch
            ))
        );

        let other_client = ClientIdentity::new("client-2").expect("client identity");
        let wrong_client = GrantValidationContext {
            expected_client_identity: &other_client,
            ..context
        };
        assert_eq!(
            validate_session_grant(&AcceptSignature, &grant, wrong_client),
            Err(GrantValidationError::Claims(
                IdentityValidationError::ClientIdentityMismatch
            ))
        );

        let other_host = HostIdentity::new("host-2").expect("Host identity");
        let wrong_host = GrantValidationContext {
            expected_host_identity: &other_host,
            ..context
        };
        assert_eq!(
            validate_session_grant(&AcceptSignature, &grant, wrong_host),
            Err(GrantValidationError::Claims(
                IdentityValidationError::HostIdentityMismatch
            ))
        );

        let other_nonce = GrantNonce::new("nonce-2").expect("nonce");
        let wrong_nonce = GrantValidationContext {
            expected_nonce: &other_nonce,
            ..context
        };
        assert_eq!(
            validate_session_grant(&AcceptSignature, &grant, wrong_nonce),
            Err(GrantValidationError::Claims(
                IdentityValidationError::NonceMismatch
            ))
        );
    }

    #[test]
    fn signed_grant_debug_redacts_signature() {
        let mut grant = grant();
        grant.signature = b"sensitive-signature".to_vec();
        let debug = format!("{grant:?}");
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("sensitive-signature"));
    }

    #[test]
    fn disclaimer_locale_accepts_safe_identifiers_and_rejects_unsafe_values() {
        assert_eq!(
            DisclaimerLocale::new("en_US")
                .expect("safe locale")
                .as_str(),
            "en_US"
        );
        for invalid in [
            "",
            ".",
            "..",
            "en/US",
            "en\\US",
            "en__US",
            "_en",
            "en_",
            "en.US",
            "en\0US",
            "français",
        ] {
            assert!(DisclaimerLocale::new(invalid).is_err(), "{invalid:?}");
        }
        assert!(DisclaimerLocale::new("a".repeat(MAX_DISCLAIMER_LOCALE_BYTES + 1)).is_err());
    }

    #[test]
    fn disclaimer_content_is_exact_bounded_utf8() {
        let locale = DisclaimerLocale::new("en_US").expect("locale");
        let boundary = vec![b'a'; MAX_DISCLAIMER_CONTENT_BYTES];
        let prepared =
            PreparedDisclaimer::from_bytes(locale.clone(), &boundary).expect("boundary content");
        assert_eq!(prepared.text().as_bytes(), boundary);
        assert_eq!(
            PreparedDisclaimer::from_bytes(locale.clone(), b""),
            Err(DisclaimerError::EmptyContent)
        );
        assert_eq!(
            PreparedDisclaimer::from_bytes(locale.clone(), &[0xff]),
            Err(DisclaimerError::InvalidUtf8)
        );
        assert_eq!(
            PreparedDisclaimer::from_bytes(locale, &vec![b'a'; MAX_DISCLAIMER_CONTENT_BYTES + 1]),
            Err(DisclaimerError::ContentTooLarge)
        );
    }

    #[test]
    fn disclaimer_digest_has_stable_golden_and_exact_matching() {
        let prepared =
            PreparedDisclaimer::from_bytes(DisclaimerLocale::new("en_US").unwrap(), b"abc")
                .unwrap();
        let golden = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert_eq!(prepared.digest().to_lower_hex(), golden);
        assert!(prepared.matches_acknowledgment(golden).unwrap());
        assert!(
            !prepared
                .matches_acknowledgment(
                    "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ae"
                )
                .unwrap()
        );
        assert_eq!(
            prepared.matches_acknowledgment(&golden.to_uppercase()),
            Err(DisclaimerError::InvalidDigest)
        );
        assert!(!format!("{prepared:?}").contains("abc"));
    }

    #[test]
    fn acceptance_is_standalone_host_timed_evidence() {
        let prepared =
            PreparedDisclaimer::from_bytes(DisclaimerLocale::new("en_US").unwrap(), b"abc")
                .unwrap();
        let evidence = DisclaimerAcceptance::new(&prepared, 1234);
        assert_eq!(evidence.locale().as_str(), "en_US");
        assert_eq!(evidence.digest(), prepared.digest());
        assert_eq!(evidence.accepted_at_epoch_seconds(), 1234);
    }
}
