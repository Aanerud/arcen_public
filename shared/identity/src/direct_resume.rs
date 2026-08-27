//! Direct-QUIC reconnect grant contracts.

use crate::{DisclaimerDigest, HostIdentity};
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::error::Error;
use std::fmt::{Debug, Display, Formatter};
use zeroize::Zeroize;

/// Exact domain separator prepended to canonical direct-resume signing bytes.
pub const DIRECT_RESUME_SIGNING_DOMAIN: &str = "arcen-direct-quic-resume-v1";
/// Fixed direct-resume claims schema.
pub const DIRECT_RESUME_SCHEMA: &str = "arcen.direct_wss_resume";
/// Current direct-resume claims version.
pub const DIRECT_RESUME_VERSION: u16 = 1;
/// Fixed direct-QUIC audience.
pub const DIRECT_RESUME_AUDIENCE: &str = "arcen-direct-quic";
/// Fixed algorithm identifier interpreted by host-owned HMAC adapters.
pub const DIRECT_RESUME_ALGORITHM: &str = "hmac-sha256";
/// Maximum direct-resume grant lifetime.
pub const MAX_DIRECT_RESUME_LIFETIME_SECONDS: u64 = 14_400;
/// Maximum encoded token bytes.
pub const MAX_DIRECT_RESUME_TOKEN_BYTES: usize = 8_192;
/// Maximum active-session identifier bytes.
pub const MAX_DIRECT_RESUME_SESSION_ID_BYTES: usize = 128;
/// Maximum Windows SID text bytes.
pub const MAX_WINDOWS_SID_BYTES: usize = 184;
/// Maximum logind session identifier bytes.
pub const MAX_LOGIND_SESSION_ID_BYTES: usize = 128;
/// Maximum disclaimer-version bytes.
pub const MAX_DISCLAIMER_VERSION_BYTES: usize = 64;
const DIRECT_RESUME_TOKEN_PREFIX: &str = "v1.";
const DIRECT_RESUME_SIGNATURE_BYTES: usize = 32;

macro_rules! bounded_resume_string {
    ($name:ident, $maximum:ident, $kind:expr) => {
        #[derive(Clone, PartialEq, Eq, Hash, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// Creates a bounded, non-empty value.
            ///
            /// # Errors
            ///
            /// Returns a precise error when the value is empty or oversized.
            pub fn new(value: impl Into<String>) -> Result<Self, DirectResumeError> {
                let value = value.into();
                validate_bounded_string(&value, $maximum, $kind)?;
                Ok(Self(value))
            }

            /// Returns the validated text.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl Debug for $name {
            fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
                formatter.write_str(concat!(stringify!($name), "(<redacted>)"))
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::new(value).map_err(D::Error::custom)
            }
        }
    };
}

bounded_resume_string!(
    ActiveHostSessionId,
    MAX_DIRECT_RESUME_SESSION_ID_BYTES,
    DirectResumeStringKind::ActiveSession
);
bounded_resume_string!(
    LogindSessionId,
    MAX_LOGIND_SESSION_ID_BYTES,
    DirectResumeStringKind::LogindSession
);
bounded_resume_string!(
    DisclaimerVersion,
    MAX_DISCLAIMER_VERSION_BYTES,
    DirectResumeStringKind::DisclaimerVersion
);

/// A validated Windows SID bound to a native WTS session.
#[derive(Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct WindowsSid(String);

impl WindowsSid {
    /// Creates a bounded canonical-looking SID string.
    ///
    /// # Errors
    ///
    /// Returns an error unless the value is bounded ASCII in `S-...` form.
    pub fn new(value: impl Into<String>) -> Result<Self, DirectResumeError> {
        let value = value.into();
        validate_bounded_string(
            &value,
            MAX_WINDOWS_SID_BYTES,
            DirectResumeStringKind::WindowsSid,
        )?;
        let Some(components) = value.strip_prefix("S-") else {
            return Err(DirectResumeError::InvalidWindowsSid);
        };
        let mut component_count = 0_usize;
        for component in components.split('-') {
            if component.is_empty() || !component.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(DirectResumeError::InvalidWindowsSid);
            }
            component_count += 1;
        }
        if component_count < 2 {
            return Err(DirectResumeError::InvalidWindowsSid);
        }
        Ok(Self(value))
    }

    /// Returns the SID text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Debug for WindowsSid {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("WindowsSid(<redacted>)")
    }
}

impl<'de> Deserialize<'de> for WindowsSid {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(D::Error::custom)
    }
}

/// Native OS principal and native-session identity.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "platform", rename_all = "snake_case", deny_unknown_fields)]
pub enum NativePrincipal {
    /// Windows security principal in a WTS session.
    Windows {
        /// Canonical SID text.
        sid: WindowsSid,
        /// WTS session identifier.
        wts_session_id: u32,
    },
    /// Linux uid in a logind session.
    Linux {
        /// Kernel uid.
        uid: u32,
        /// Stable active logind session identifier.
        logind_session_id: LogindSessionId,
    },
}

impl Debug for NativePrincipal {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Windows { .. } => "NativePrincipal::Windows(<redacted>)",
            Self::Linux { .. } => "NativePrincipal::Linux(<redacted>)",
        })
    }
}

/// Deck-generated holder nonce.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DeckHolderNonce([u8; 32]);

impl DeckHolderNonce {
    /// Creates a holder nonce from exactly 32 random bytes.
    #[must_use]
    pub const fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Returns nonce bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl Debug for DeckHolderNonce {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("DeckHolderNonce(<redacted>)")
    }
}

/// Host-generated nonce for one grant generation.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DirectResumeNonce([u8; 32]);

impl DirectResumeNonce {
    /// Creates a grant nonce from exactly 32 random bytes.
    #[must_use]
    pub const fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Returns nonce bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl Debug for DirectResumeNonce {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("DirectResumeNonce(<redacted>)")
    }
}

/// Strict claims for one direct-QUIC reconnect generation.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirectResumeGrantClaims {
    schema: String,
    version: u16,
    audience: String,
    algorithm: String,
    host_identity: HostIdentity,
    active_session_id: ActiveHostSessionId,
    native_principal: NativePrincipal,
    holder_nonce: DeckHolderNonce,
    generation: u64,
    nonce: DirectResumeNonce,
    disclaimer_digest: DisclaimerDigest,
    disclaimer_version: DisclaimerVersion,
    issued_at: u64,
    expires_at: u64,
}

impl DirectResumeGrantClaims {
    /// Creates strict claims with fixed schema, audience, and algorithm values.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed bindings or a zero/overlong lifetime.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        host_identity: HostIdentity,
        active_session_id: ActiveHostSessionId,
        native_principal: NativePrincipal,
        holder_nonce: DeckHolderNonce,
        generation: u64,
        nonce: DirectResumeNonce,
        disclaimer_digest: DisclaimerDigest,
        disclaimer_version: DisclaimerVersion,
        issued_at: u64,
        expires_at: u64,
    ) -> Result<Self, DirectResumeError> {
        let claims = Self {
            schema: DIRECT_RESUME_SCHEMA.to_owned(),
            version: DIRECT_RESUME_VERSION,
            audience: DIRECT_RESUME_AUDIENCE.to_owned(),
            algorithm: DIRECT_RESUME_ALGORITHM.to_owned(),
            host_identity,
            active_session_id,
            native_principal,
            holder_nonce,
            generation,
            nonce,
            disclaimer_digest,
            disclaimer_version,
            issued_at,
            expires_at,
        };
        validate_claim_structure(&claims)?;
        validate_lifetime(&claims)?;
        Ok(claims)
    }

    /// Returns canonical, domain-separated, length-delimited signing bytes.
    ///
    /// # Errors
    ///
    /// Returns an error if deserialized claims violate fixed structural bounds.
    pub fn canonical_signing_bytes(&self) -> Result<Vec<u8>, DirectResumeError> {
        validate_claim_structure(self)?;
        let mut output = Vec::with_capacity(1_024);
        append_part(&mut output, DIRECT_RESUME_SIGNING_DOMAIN.as_bytes())?;
        append_part(&mut output, self.schema.as_bytes())?;
        append_part(&mut output, &self.version.to_be_bytes())?;
        append_part(&mut output, self.audience.as_bytes())?;
        append_part(&mut output, self.algorithm.as_bytes())?;
        append_part(&mut output, self.host_identity.as_str().as_bytes())?;
        append_part(&mut output, self.active_session_id.as_str().as_bytes())?;
        match &self.native_principal {
            NativePrincipal::Windows {
                sid,
                wts_session_id,
            } => {
                append_part(&mut output, b"windows")?;
                append_part(&mut output, sid.as_str().as_bytes())?;
                append_part(&mut output, &wts_session_id.to_be_bytes())?;
            }
            NativePrincipal::Linux {
                uid,
                logind_session_id,
            } => {
                append_part(&mut output, b"linux")?;
                append_part(&mut output, &uid.to_be_bytes())?;
                append_part(&mut output, logind_session_id.as_str().as_bytes())?;
            }
        }
        append_part(&mut output, self.holder_nonce.as_bytes())?;
        append_part(&mut output, &self.generation.to_be_bytes())?;
        append_part(&mut output, self.nonce.as_bytes())?;
        append_part(&mut output, self.disclaimer_digest.as_bytes())?;
        append_part(&mut output, self.disclaimer_version.as_str().as_bytes())?;
        append_part(&mut output, &self.issued_at.to_be_bytes())?;
        append_part(&mut output, &self.expires_at.to_be_bytes())?;
        Ok(output)
    }

    /// Returns the target Host identity.
    #[must_use]
    pub const fn host_identity(&self) -> &HostIdentity {
        &self.host_identity
    }

    /// Returns the active host session.
    #[must_use]
    pub const fn active_session_id(&self) -> &ActiveHostSessionId {
        &self.active_session_id
    }

    /// Returns the native principal binding.
    #[must_use]
    pub const fn native_principal(&self) -> &NativePrincipal {
        &self.native_principal
    }

    /// Returns the Deck holder nonce.
    #[must_use]
    pub const fn holder_nonce(&self) -> DeckHolderNonce {
        self.holder_nonce
    }

    /// Returns the grant generation.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Returns the generation nonce.
    #[must_use]
    pub const fn nonce(&self) -> DirectResumeNonce {
        self.nonce
    }

    /// Returns the disclaimer digest.
    #[must_use]
    pub const fn disclaimer_digest(&self) -> DisclaimerDigest {
        self.disclaimer_digest
    }

    /// Returns the disclaimer version.
    #[must_use]
    pub const fn disclaimer_version(&self) -> &DisclaimerVersion {
        &self.disclaimer_version
    }

    /// Returns issued-at epoch seconds.
    #[must_use]
    pub const fn issued_at(&self) -> u64 {
        self.issued_at
    }

    /// Returns expiry epoch seconds.
    #[must_use]
    pub const fn expires_at(&self) -> u64 {
        self.expires_at
    }
}

impl Debug for DirectResumeGrantClaims {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DirectResumeGrantClaims")
            .field("schema", &self.schema)
            .field("version", &self.version)
            .field("audience", &self.audience)
            .field("algorithm", &self.algorithm)
            .field("host_identity", &self.host_identity)
            .field("active_session_id", &self.active_session_id)
            .field("native_principal", &self.native_principal)
            .field("holder_nonce", &"<redacted>")
            .field("generation", &self.generation)
            .field("nonce", &"<redacted>")
            .field("disclaimer_digest", &self.disclaimer_digest)
            .field("disclaimer_version", &self.disclaimer_version)
            .field("issued_at", &self.issued_at)
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SignedDirectResumeEnvelope {
    claims: DirectResumeGrantClaims,
    signature: [u8; DIRECT_RESUME_SIGNATURE_BYTES],
}

impl Drop for SignedDirectResumeEnvelope {
    fn drop(&mut self) {
        self.signature.zeroize();
    }
}

impl Debug for SignedDirectResumeEnvelope {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SignedDirectResumeEnvelope")
            .field("claims", &self.claims)
            .field("signature", &"<redacted>")
            .finish()
    }
}

/// Opaque text token carrying strict claims and a host-owned HMAC signature.
#[derive(PartialEq, Eq)]
pub struct DirectResumeGrantToken(String);

impl DirectResumeGrantToken {
    /// Parses and bounds an encoded token without validating its signature.
    ///
    /// # Errors
    ///
    /// Returns precise oversized, version, or malformed-token errors.
    pub fn parse(value: impl Into<String>) -> Result<Self, DirectResumeError> {
        let value = value.into();
        if value.len() > MAX_DIRECT_RESUME_TOKEN_BYTES {
            return Err(DirectResumeError::TokenTooLong);
        }
        if !value.starts_with(DIRECT_RESUME_TOKEN_PREFIX) {
            return if value.contains('.') {
                Err(DirectResumeError::UnsupportedTokenVersion)
            } else {
                Err(DirectResumeError::MalformedToken)
            };
        }
        if value.len() == DIRECT_RESUME_TOKEN_PREFIX.len() {
            return Err(DirectResumeError::MalformedToken);
        }
        Ok(Self(value))
    }

    /// Returns opaque token text for transport.
    #[must_use]
    pub fn expose_for_transport(&self) -> &str {
        &self.0
    }

    fn decode(&self) -> Result<SignedDirectResumeEnvelope, DirectResumeError> {
        let encoded = self
            .0
            .strip_prefix(DIRECT_RESUME_TOKEN_PREFIX)
            .ok_or(DirectResumeError::UnsupportedTokenVersion)?;
        let mut bytes = decode_hex(encoded)?;
        let decoded = serde_json::from_slice(&bytes).map_err(|_| DirectResumeError::MalformedToken);
        bytes.zeroize();
        decoded
    }
}

impl Clone for DirectResumeGrantToken {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl Debug for DirectResumeGrantToken {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("DirectResumeGrantToken(<redacted>)")
    }
}

impl Drop for DirectResumeGrantToken {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl Serialize for DirectResumeGrantToken {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for DirectResumeGrantToken {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(D::Error::custom)
    }
}

/// Host-owned adapter for signing canonical direct-resume bytes.
pub trait DirectResumeGrantSigner {
    /// Adapter-specific signing failure.
    type Error;

    /// Produces a 32-byte HMAC signature.
    ///
    /// # Errors
    ///
    /// Returns an adapter failure without issuing a token.
    fn sign(&self, canonical_signing_bytes: &[u8]) -> Result<[u8; 32], Self::Error>;
}

/// Host-owned adapter for constant-time direct-resume HMAC verification.
pub trait DirectResumeGrantVerifier {
    /// Adapter-specific verification failure.
    type Error;

    /// Verifies the signature using constant-time adapter semantics.
    ///
    /// # Errors
    ///
    /// Returns an adapter failure for any invalid signature or key state.
    fn verify(
        &self,
        canonical_signing_bytes: &[u8],
        signature: &[u8; 32],
    ) -> Result<(), Self::Error>;
}

/// Signs structurally valid, currently issuable claims.
///
/// # Errors
///
/// Returns a claims, serialization, size, or adapter signing failure.
pub fn sign_direct_resume_grant<S: DirectResumeGrantSigner>(
    signer: &S,
    claims: DirectResumeGrantClaims,
    now_epoch_seconds: u64,
) -> Result<DirectResumeGrantToken, DirectResumeSigningError<S::Error>> {
    validate_claim_structure(&claims).map_err(DirectResumeSigningError::Claims)?;
    validate_timing(&claims, now_epoch_seconds).map_err(DirectResumeSigningError::Claims)?;
    let canonical = claims
        .canonical_signing_bytes()
        .map_err(DirectResumeSigningError::Claims)?;
    let signature = signer
        .sign(&canonical)
        .map_err(DirectResumeSigningError::Signer)?;
    let envelope = SignedDirectResumeEnvelope { claims, signature };
    let mut bytes =
        serde_json::to_vec(&envelope).map_err(|_| DirectResumeSigningError::Serialization)?;
    let token = format!("{DIRECT_RESUME_TOKEN_PREFIX}{}", encode_hex(&bytes));
    bytes.zeroize();
    if token.len() > MAX_DIRECT_RESUME_TOKEN_BYTES {
        return Err(DirectResumeSigningError::Claims(
            DirectResumeError::TokenTooLong,
        ));
    }
    Ok(DirectResumeGrantToken(token))
}

/// Expected local bindings for direct-QUIC resume validation.
#[derive(Debug, Clone, Copy)]
pub struct DirectResumeValidationContext<'a> {
    /// Stable expected Host identity.
    pub expected_host_identity: &'a HostIdentity,
    /// Existing active host session.
    pub expected_active_session_id: &'a ActiveHostSessionId,
    /// Current native principal and native session.
    pub expected_native_principal: &'a NativePrincipal,
    /// Deck holder nonce retained from initial opt-in.
    pub expected_holder_nonce: DeckHolderNonce,
    /// Current grant generation.
    pub expected_generation: u64,
    /// Current one-time grant nonce.
    pub expected_nonce: DirectResumeNonce,
    /// Disclaimer content digest accepted for this session.
    pub expected_disclaimer_digest: DisclaimerDigest,
    /// Disclaimer policy version accepted for this session.
    pub expected_disclaimer_version: &'a DisclaimerVersion,
    /// Current epoch seconds.
    pub now_epoch_seconds: u64,
}

/// Immutable local bindings for authenticating a direct-resume candidate.
///
/// Generation and one-time nonce comparison remain the host registry's
/// responsibility so it can distinguish the current grant from its exact
/// predecessor while holding the registry lock.
#[derive(Clone, Copy)]
pub struct DirectResumeBindingContext<'a> {
    /// Stable expected Host identity.
    pub expected_host_identity: &'a HostIdentity,
    /// Existing active host session.
    pub expected_active_session_id: &'a ActiveHostSessionId,
    /// Current native principal and native session.
    pub expected_native_principal: &'a NativePrincipal,
    /// Deck holder nonce retained from initial opt-in.
    pub expected_holder_nonce: DeckHolderNonce,
    /// Disclaimer content digest accepted for this session.
    pub expected_disclaimer_digest: DisclaimerDigest,
    /// Disclaimer policy version accepted for this session.
    pub expected_disclaimer_version: &'a DisclaimerVersion,
    /// Current epoch seconds.
    pub now_epoch_seconds: u64,
}

impl Debug for DirectResumeBindingContext<'_> {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DirectResumeBindingContext")
            .field("expected_host_identity", self.expected_host_identity)
            .field(
                "expected_active_session_id",
                self.expected_active_session_id,
            )
            .field("expected_native_principal", &"<redacted>")
            .field("expected_holder_nonce", &"<redacted>")
            .field(
                "expected_disclaimer_digest",
                &self.expected_disclaimer_digest,
            )
            .field(
                "expected_disclaimer_version",
                self.expected_disclaimer_version,
            )
            .field("now_epoch_seconds", &self.now_epoch_seconds)
            .finish()
    }
}

/// Validated direct-QUIC resume evidence.
#[derive(PartialEq, Eq)]
pub struct ValidatedDirectResumeGrant {
    claims: DirectResumeGrantClaims,
}

impl Debug for ValidatedDirectResumeGrant {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ValidatedDirectResumeGrant(<redacted>)")
    }
}

impl ValidatedDirectResumeGrant {
    /// Returns validated claims.
    #[must_use]
    pub const fn claims(&self) -> &DirectResumeGrantClaims {
        &self.claims
    }

    /// Consumes evidence and returns validated claims.
    #[must_use]
    pub fn into_claims(self) -> DirectResumeGrantClaims {
        self.claims
    }
}

/// Verifies a direct-resume token and every current binding.
///
/// # Errors
///
/// Returns a precise token, fixed-field, signature, binding, or timing failure.
pub fn validate_direct_resume_grant<V: DirectResumeGrantVerifier>(
    verifier: &V,
    token: &DirectResumeGrantToken,
    context: DirectResumeValidationContext<'_>,
) -> Result<ValidatedDirectResumeGrant, DirectResumeValidationError<V::Error>> {
    let validated = validate_direct_resume_grant_candidate(
        verifier,
        token,
        DirectResumeBindingContext {
            expected_host_identity: context.expected_host_identity,
            expected_active_session_id: context.expected_active_session_id,
            expected_native_principal: context.expected_native_principal,
            expected_holder_nonce: context.expected_holder_nonce,
            expected_disclaimer_digest: context.expected_disclaimer_digest,
            expected_disclaimer_version: context.expected_disclaimer_version,
            now_epoch_seconds: context.now_epoch_seconds,
        },
    )?;
    validate_slot_binding(
        validated.claims(),
        context.expected_generation,
        context.expected_nonce,
    )
    .map_err(DirectResumeValidationError::Claims)?;
    Ok(validated)
}

/// Authenticates a direct-resume candidate without comparing mutable slot data.
///
/// This verifies strict structure, the host-owned signature, every immutable
/// session binding, and timing. Callers must compare the returned generation
/// and nonce with their registry slot while still holding its lock.
///
/// # Errors
///
/// Returns a precise token, fixed-field, signature, immutable-binding, or
/// timing failure.
pub fn validate_direct_resume_grant_candidate<V: DirectResumeGrantVerifier>(
    verifier: &V,
    token: &DirectResumeGrantToken,
    context: DirectResumeBindingContext<'_>,
) -> Result<ValidatedDirectResumeGrant, DirectResumeValidationError<V::Error>> {
    let envelope = token
        .decode()
        .map_err(DirectResumeValidationError::Claims)?;
    let claims = &envelope.claims;
    validate_claim_structure(claims).map_err(DirectResumeValidationError::Claims)?;
    let canonical = claims
        .canonical_signing_bytes()
        .map_err(DirectResumeValidationError::Claims)?;
    verifier
        .verify(&canonical, &envelope.signature)
        .map_err(DirectResumeValidationError::Signature)?;
    validate_immutable_bindings(claims, &context).map_err(DirectResumeValidationError::Claims)?;
    validate_timing(claims, context.now_epoch_seconds)
        .map_err(DirectResumeValidationError::Claims)?;
    Ok(ValidatedDirectResumeGrant {
        claims: envelope.claims.clone(),
    })
}

fn validate_claim_structure(claims: &DirectResumeGrantClaims) -> Result<(), DirectResumeError> {
    if claims.schema != DIRECT_RESUME_SCHEMA {
        return Err(DirectResumeError::UnsupportedSchema);
    }
    if claims.version != DIRECT_RESUME_VERSION {
        return Err(DirectResumeError::UnsupportedClaimsVersion);
    }
    if claims.audience != DIRECT_RESUME_AUDIENCE {
        return Err(DirectResumeError::AudienceMismatch);
    }
    if claims.algorithm != DIRECT_RESUME_ALGORITHM {
        return Err(DirectResumeError::AlgorithmMismatch);
    }
    validate_bounded_string(
        claims.host_identity.as_str(),
        crate::MAX_GRANT_BINDING_VALUE_BYTES,
        DirectResumeStringKind::HostIdentity,
    )?;
    validate_bounded_string(
        claims.active_session_id.as_str(),
        MAX_DIRECT_RESUME_SESSION_ID_BYTES,
        DirectResumeStringKind::ActiveSession,
    )?;
    match &claims.native_principal {
        NativePrincipal::Windows { sid, .. } => validate_bounded_string(
            sid.as_str(),
            MAX_WINDOWS_SID_BYTES,
            DirectResumeStringKind::WindowsSid,
        )?,
        NativePrincipal::Linux {
            logind_session_id, ..
        } => validate_bounded_string(
            logind_session_id.as_str(),
            MAX_LOGIND_SESSION_ID_BYTES,
            DirectResumeStringKind::LogindSession,
        )?,
    }
    validate_bounded_string(
        claims.disclaimer_version.as_str(),
        MAX_DISCLAIMER_VERSION_BYTES,
        DirectResumeStringKind::DisclaimerVersion,
    )?;
    validate_lifetime(claims)
}

fn validate_lifetime(claims: &DirectResumeGrantClaims) -> Result<(), DirectResumeError> {
    let lifetime = claims
        .expires_at
        .checked_sub(claims.issued_at)
        .ok_or(DirectResumeError::InvalidLifetime)?;
    if lifetime == 0 || lifetime > MAX_DIRECT_RESUME_LIFETIME_SECONDS {
        return Err(DirectResumeError::InvalidLifetime);
    }
    Ok(())
}

fn validate_timing(
    claims: &DirectResumeGrantClaims,
    now_epoch_seconds: u64,
) -> Result<(), DirectResumeError> {
    validate_lifetime(claims)?;
    if claims.issued_at > now_epoch_seconds {
        return Err(DirectResumeError::IssuedInFuture);
    }
    if claims.expires_at <= now_epoch_seconds {
        return Err(DirectResumeError::Expired);
    }
    Ok(())
}

fn validate_immutable_bindings(
    claims: &DirectResumeGrantClaims,
    context: &DirectResumeBindingContext<'_>,
) -> Result<(), DirectResumeError> {
    if &claims.host_identity != context.expected_host_identity {
        return Err(DirectResumeError::HostIdentityMismatch);
    }
    if &claims.active_session_id != context.expected_active_session_id {
        return Err(DirectResumeError::ActiveSessionMismatch);
    }
    if &claims.native_principal != context.expected_native_principal {
        return Err(DirectResumeError::NativePrincipalMismatch);
    }
    if claims.holder_nonce != context.expected_holder_nonce {
        return Err(DirectResumeError::HolderNonceMismatch);
    }
    if !claims
        .disclaimer_digest
        .matches(context.expected_disclaimer_digest)
    {
        return Err(DirectResumeError::DisclaimerDigestMismatch);
    }
    if &claims.disclaimer_version != context.expected_disclaimer_version {
        return Err(DirectResumeError::DisclaimerVersionMismatch);
    }
    Ok(())
}

fn validate_slot_binding(
    claims: &DirectResumeGrantClaims,
    expected_generation: u64,
    expected_nonce: DirectResumeNonce,
) -> Result<(), DirectResumeError> {
    if claims.generation != expected_generation {
        return Err(DirectResumeError::GenerationMismatch);
    }
    if claims.nonce != expected_nonce {
        return Err(DirectResumeError::NonceMismatch);
    }
    Ok(())
}

fn append_part(output: &mut Vec<u8>, part: &[u8]) -> Result<(), DirectResumeError> {
    let length = u32::try_from(part.len()).map_err(|_| DirectResumeError::TokenTooLong)?;
    output.extend_from_slice(&length.to_be_bytes());
    output.extend_from_slice(part);
    Ok(())
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

fn decode_hex(value: &str) -> Result<Vec<u8>, DirectResumeError> {
    if value.len() % 2 != 0 {
        return Err(DirectResumeError::MalformedToken);
    }
    let mut output = Vec::with_capacity(value.len() / 2);
    for pair in value.as_bytes().chunks_exact(2) {
        let high = hex_nibble(pair[0]).ok_or(DirectResumeError::MalformedToken)?;
        let low = hex_nibble(pair[1]).ok_or(DirectResumeError::MalformedToken)?;
        output.push((high << 4) | low);
    }
    Ok(output)
}

const fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

#[derive(Clone, Copy)]
enum DirectResumeStringKind {
    HostIdentity,
    ActiveSession,
    WindowsSid,
    LogindSession,
    DisclaimerVersion,
}

fn validate_bounded_string(
    value: &str,
    maximum: usize,
    kind: DirectResumeStringKind,
) -> Result<(), DirectResumeError> {
    if value.is_empty() {
        return Err(match kind {
            DirectResumeStringKind::HostIdentity => DirectResumeError::EmptyHostIdentity,
            DirectResumeStringKind::ActiveSession => DirectResumeError::EmptyActiveSession,
            DirectResumeStringKind::WindowsSid => DirectResumeError::InvalidWindowsSid,
            DirectResumeStringKind::LogindSession => DirectResumeError::EmptyLogindSession,
            DirectResumeStringKind::DisclaimerVersion => DirectResumeError::EmptyDisclaimerVersion,
        });
    }
    if value.len() > maximum {
        return Err(match kind {
            DirectResumeStringKind::HostIdentity => DirectResumeError::HostIdentityTooLong,
            DirectResumeStringKind::ActiveSession => DirectResumeError::ActiveSessionTooLong,
            DirectResumeStringKind::WindowsSid => DirectResumeError::WindowsSidTooLong,
            DirectResumeStringKind::LogindSession => DirectResumeError::LogindSessionTooLong,
            DirectResumeStringKind::DisclaimerVersion => {
                DirectResumeError::DisclaimerVersionTooLong
            }
        });
    }
    Ok(())
}

/// Non-cryptographic direct-resume token or claims failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectResumeError {
    /// Encoded token exceeded its fixed bound.
    TokenTooLong,
    /// Encoded token was malformed.
    MalformedToken,
    /// Token envelope version was unknown.
    UnsupportedTokenVersion,
    /// Claims schema was unknown.
    UnsupportedSchema,
    /// Claims version was unknown.
    UnsupportedClaimsVersion,
    /// Audience was not direct QUIC.
    AudienceMismatch,
    /// Algorithm was not the fixed direct-resume HMAC algorithm.
    AlgorithmMismatch,
    /// Host identity was empty.
    EmptyHostIdentity,
    /// Host identity exceeded its bound.
    HostIdentityTooLong,
    /// Active session was empty.
    EmptyActiveSession,
    /// Active session exceeded its bound.
    ActiveSessionTooLong,
    /// Windows SID syntax was invalid.
    InvalidWindowsSid,
    /// Windows SID exceeded its bound.
    WindowsSidTooLong,
    /// logind session was empty.
    EmptyLogindSession,
    /// logind session exceeded its bound.
    LogindSessionTooLong,
    /// Disclaimer version was empty.
    EmptyDisclaimerVersion,
    /// Disclaimer version exceeded its bound.
    DisclaimerVersionTooLong,
    /// Issued-at was later than the validation clock.
    IssuedInFuture,
    /// Token was expired at the validation clock.
    Expired,
    /// Lifetime was zero, inverted, or longer than 7200 seconds.
    InvalidLifetime,
    /// Stable Host identity did not match.
    HostIdentityMismatch,
    /// Active host session did not match.
    ActiveSessionMismatch,
    /// Native principal or native session did not match.
    NativePrincipalMismatch,
    /// Deck holder nonce did not match.
    HolderNonceMismatch,
    /// Current grant generation did not match.
    GenerationMismatch,
    /// Current grant nonce did not match.
    NonceMismatch,
    /// Disclaimer digest did not match.
    DisclaimerDigestMismatch,
    /// Disclaimer version did not match.
    DisclaimerVersionMismatch,
}

impl Display for DirectResumeError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::TokenTooLong => "direct resume token exceeds its bound",
            Self::MalformedToken => "direct resume token is malformed",
            Self::UnsupportedTokenVersion => "direct resume token version is unsupported",
            Self::UnsupportedSchema => "direct resume claims schema is unsupported",
            Self::UnsupportedClaimsVersion => "direct resume claims version is unsupported",
            Self::AudienceMismatch => "direct resume audience does not match",
            Self::AlgorithmMismatch => "direct resume algorithm does not match",
            Self::EmptyHostIdentity => "direct resume Host identity is empty",
            Self::HostIdentityTooLong => "direct resume Host identity exceeds its bound",
            Self::EmptyActiveSession => "direct resume active session is empty",
            Self::ActiveSessionTooLong => "direct resume active session exceeds its bound",
            Self::InvalidWindowsSid => "direct resume Windows SID is invalid",
            Self::WindowsSidTooLong => "direct resume Windows SID exceeds its bound",
            Self::EmptyLogindSession => "direct resume logind session is empty",
            Self::LogindSessionTooLong => "direct resume logind session exceeds its bound",
            Self::EmptyDisclaimerVersion => "direct resume disclaimer version is empty",
            Self::DisclaimerVersionTooLong => "direct resume disclaimer version exceeds its bound",
            Self::IssuedInFuture => "direct resume claims were issued in the future",
            Self::Expired => "direct resume grant is expired",
            Self::InvalidLifetime => "direct resume grant lifetime is invalid",
            Self::HostIdentityMismatch => "direct resume Host identity does not match",
            Self::ActiveSessionMismatch => "direct resume active session does not match",
            Self::NativePrincipalMismatch => "direct resume native principal does not match",
            Self::HolderNonceMismatch => "direct resume holder nonce does not match",
            Self::GenerationMismatch => "direct resume generation does not match",
            Self::NonceMismatch => "direct resume nonce does not match",
            Self::DisclaimerDigestMismatch => "direct resume disclaimer digest does not match",
            Self::DisclaimerVersionMismatch => "direct resume disclaimer version does not match",
        })
    }
}

impl Error for DirectResumeError {}

/// Direct-resume signing failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirectResumeSigningError<E> {
    /// Claims failed local policy.
    Claims(DirectResumeError),
    /// Host-owned signer failed.
    Signer(E),
    /// Bounded envelope serialization failed.
    Serialization,
}

/// Direct-resume validation failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirectResumeValidationError<E> {
    /// Token or claims failed strict validation.
    Claims(DirectResumeError),
    /// Host-owned constant-time verifier rejected the signature.
    Signature(E),
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use subtle::ConstantTimeEq;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct SignatureError;

    struct TestHmacAdapter;

    impl TestHmacAdapter {
        fn signature(bytes: &[u8]) -> [u8; 32] {
            let mut hasher = Sha256::new();
            hasher.update(b"test-host-key");
            hasher.update(bytes);
            hasher.finalize().into()
        }
    }

    impl DirectResumeGrantSigner for TestHmacAdapter {
        type Error = SignatureError;

        fn sign(&self, bytes: &[u8]) -> Result<[u8; 32], Self::Error> {
            Ok(Self::signature(bytes))
        }
    }

    impl DirectResumeGrantVerifier for TestHmacAdapter {
        type Error = SignatureError;

        fn verify(&self, bytes: &[u8], signature: &[u8; 32]) -> Result<(), Self::Error> {
            let expected = Self::signature(bytes);
            if bool::from(expected.ct_eq(signature)) {
                Ok(())
            } else {
                Err(SignatureError)
            }
        }
    }

    fn digest(byte: u8) -> DisclaimerDigest {
        DisclaimerDigest::parse_lower_hex(&format!("{byte:02x}").repeat(32)).expect("digest")
    }

    fn fixtures() -> (
        DirectResumeGrantClaims,
        HostIdentity,
        ActiveHostSessionId,
        NativePrincipal,
        DisclaimerVersion,
    ) {
        let host = HostIdentity::new("host-stable-1").expect("host");
        let session = ActiveHostSessionId::new("native-session-9").expect("session");
        let principal = NativePrincipal::Linux {
            uid: 1_000,
            logind_session_id: LogindSessionId::new("c9").expect("logind"),
        };
        let disclaimer_version = DisclaimerVersion::new("policy-7").expect("version");
        let claims = DirectResumeGrantClaims::new(
            host.clone(),
            session.clone(),
            principal.clone(),
            DeckHolderNonce::new([3; 32]),
            4,
            DirectResumeNonce::new([5; 32]),
            digest(6),
            disclaimer_version.clone(),
            100,
            200,
        )
        .expect("claims");
        (claims, host, session, principal, disclaimer_version)
    }

    fn context<'a>(
        host: &'a HostIdentity,
        session: &'a ActiveHostSessionId,
        principal: &'a NativePrincipal,
        version: &'a DisclaimerVersion,
    ) -> DirectResumeValidationContext<'a> {
        DirectResumeValidationContext {
            expected_host_identity: host,
            expected_active_session_id: session,
            expected_native_principal: principal,
            expected_holder_nonce: DeckHolderNonce::new([3; 32]),
            expected_generation: 4,
            expected_nonce: DirectResumeNonce::new([5; 32]),
            expected_disclaimer_digest: digest(6),
            expected_disclaimer_version: version,
            now_epoch_seconds: 150,
        }
    }

    fn binding_context<'a>(
        host: &'a HostIdentity,
        session: &'a ActiveHostSessionId,
        principal: &'a NativePrincipal,
        version: &'a DisclaimerVersion,
    ) -> DirectResumeBindingContext<'a> {
        DirectResumeBindingContext {
            expected_host_identity: host,
            expected_active_session_id: session,
            expected_native_principal: principal,
            expected_holder_nonce: DeckHolderNonce::new([3; 32]),
            expected_disclaimer_digest: digest(6),
            expected_disclaimer_version: version,
            now_epoch_seconds: 150,
        }
    }

    fn token() -> (
        DirectResumeGrantToken,
        HostIdentity,
        ActiveHostSessionId,
        NativePrincipal,
        DisclaimerVersion,
    ) {
        let (claims, host, session, principal, version) = fixtures();
        let token =
            sign_direct_resume_grant(&TestHmacAdapter, claims, 100).expect("signed direct grant");
        (token, host, session, principal, version)
    }

    #[test]
    fn canonical_bytes_are_domain_separated_and_length_delimited() {
        let (claims, ..) = fixtures();
        let bytes = claims.canonical_signing_bytes().expect("canonical");
        assert_eq!(
            &bytes[0..4],
            &u32::try_from(DIRECT_RESUME_SIGNING_DOMAIN.len())
                .expect("length")
                .to_be_bytes()
        );
        assert_eq!(
            &bytes[4..4 + DIRECT_RESUME_SIGNING_DOMAIN.len()],
            DIRECT_RESUME_SIGNING_DOMAIN.as_bytes()
        );
        assert_eq!(bytes, claims.canonical_signing_bytes().expect("stable"));
    }

    #[test]
    fn signed_token_round_trips_and_redacts_all_opaque_material() {
        let (token, host, session, principal, version) = token();
        let encoded = serde_json::to_string(&token).expect("serialize");
        let parsed: DirectResumeGrantToken = serde_json::from_str(&encoded).expect("parse");
        let validated = validate_direct_resume_grant(
            &TestHmacAdapter,
            &parsed,
            context(&host, &session, &principal, &version),
        )
        .expect("validated");
        assert_eq!(validated.claims().generation(), 4);
        assert_eq!(format!("{token:?}"), "DirectResumeGrantToken(<redacted>)");
        assert!(!format!("{:?}", token.decode().expect("decode")).contains("[5, 5"));
    }

    #[test]
    fn candidate_validation_authenticates_bindings_and_time_without_slot_values() {
        let (token, host, session, principal, version) = token();
        let validated = validate_direct_resume_grant_candidate(
            &TestHmacAdapter,
            &token,
            binding_context(&host, &session, &principal, &version),
        )
        .expect("candidate");
        assert_eq!(validated.claims().generation(), 4);
        assert_eq!(validated.claims().nonce(), DirectResumeNonce::new([5; 32]));
        assert_eq!(
            format!("{validated:?}"),
            "ValidatedDirectResumeGrant(<redacted>)"
        );

        let other_host = HostIdentity::new("other-host").expect("host");
        let other_session = ActiveHostSessionId::new("other-session").expect("session");
        let other_principal = NativePrincipal::Windows {
            sid: WindowsSid::new("S-1-5-21-1").expect("sid"),
            wts_session_id: 8,
        };
        let other_version = DisclaimerVersion::new("other-policy").expect("version");
        let base = binding_context(&host, &session, &principal, &version);
        let cases = [
            (
                DirectResumeBindingContext {
                    expected_host_identity: &other_host,
                    ..base
                },
                DirectResumeError::HostIdentityMismatch,
            ),
            (
                DirectResumeBindingContext {
                    expected_active_session_id: &other_session,
                    ..base
                },
                DirectResumeError::ActiveSessionMismatch,
            ),
            (
                DirectResumeBindingContext {
                    expected_native_principal: &other_principal,
                    ..base
                },
                DirectResumeError::NativePrincipalMismatch,
            ),
            (
                DirectResumeBindingContext {
                    expected_holder_nonce: DeckHolderNonce::new([9; 32]),
                    ..base
                },
                DirectResumeError::HolderNonceMismatch,
            ),
            (
                DirectResumeBindingContext {
                    expected_disclaimer_digest: digest(9),
                    ..base
                },
                DirectResumeError::DisclaimerDigestMismatch,
            ),
            (
                DirectResumeBindingContext {
                    expected_disclaimer_version: &other_version,
                    ..base
                },
                DirectResumeError::DisclaimerVersionMismatch,
            ),
            (
                DirectResumeBindingContext {
                    now_epoch_seconds: 200,
                    ..base
                },
                DirectResumeError::Expired,
            ),
            (
                DirectResumeBindingContext {
                    now_epoch_seconds: 99,
                    ..base
                },
                DirectResumeError::IssuedInFuture,
            ),
        ];
        for (context, expected) in cases {
            assert_eq!(
                validate_direct_resume_grant_candidate(&TestHmacAdapter, &token, context),
                Err(DirectResumeValidationError::Claims(expected))
            );
        }
    }

    #[test]
    fn candidate_validation_rejects_malformed_signature_and_invalid_lifetime() {
        let (token, host, session, principal, version) = token();
        let context = binding_context(&host, &session, &principal, &version);
        let malformed = DirectResumeGrantToken("v1.zz".to_string());
        assert_eq!(
            validate_direct_resume_grant_candidate(&TestHmacAdapter, &malformed, context),
            Err(DirectResumeValidationError::Claims(
                DirectResumeError::MalformedToken
            ))
        );

        let mut envelope = token.decode().expect("envelope");
        envelope.signature[0] ^= 1;
        let bytes = serde_json::to_vec(&envelope).expect("serialize");
        let tampered =
            DirectResumeGrantToken::parse(format!("v1.{}", encode_hex(&bytes))).expect("token");
        assert_eq!(
            validate_direct_resume_grant_candidate(&TestHmacAdapter, &tampered, context),
            Err(DirectResumeValidationError::Signature(SignatureError))
        );

        let mut value = serde_json::to_value(token.decode().expect("decode")).expect("json");
        value["claims"]["expires_at"] = serde_json::json!(100);
        let bytes = serde_json::to_vec(&value).expect("serialize");
        let invalid_lifetime =
            DirectResumeGrantToken::parse(format!("v1.{}", encode_hex(&bytes))).expect("token");
        assert_eq!(
            validate_direct_resume_grant_candidate(&TestHmacAdapter, &invalid_lifetime, context),
            Err(DirectResumeValidationError::Claims(
                DirectResumeError::InvalidLifetime
            ))
        );
    }

    #[test]
    fn malformed_oversized_unknown_version_and_signature_tamper_fail() {
        assert_eq!(
            DirectResumeGrantToken::parse("bad"),
            Err(DirectResumeError::MalformedToken)
        );
        assert_eq!(
            DirectResumeGrantToken::parse("v2.00"),
            Err(DirectResumeError::UnsupportedTokenVersion)
        );
        assert_eq!(
            DirectResumeGrantToken::parse("x".repeat(MAX_DIRECT_RESUME_TOKEN_BYTES + 1)),
            Err(DirectResumeError::TokenTooLong)
        );

        let (token, host, session, principal, version) = token();
        let mut envelope = token.decode().expect("envelope");
        envelope.signature[0] ^= 1;
        let bytes = serde_json::to_vec(&envelope).expect("serialize");
        let tampered = DirectResumeGrantToken::parse(format!("v1.{}", encode_hex(&bytes)))
            .expect("bounded token");
        assert_eq!(
            validate_direct_resume_grant(
                &TestHmacAdapter,
                &tampered,
                context(&host, &session, &principal, &version)
            ),
            Err(DirectResumeValidationError::Signature(SignatureError))
        );
    }

    #[test]
    fn every_current_binding_mismatch_is_precise() {
        let (token, host, session, principal, version) = token();
        let other_host = HostIdentity::new("other-host").expect("host");
        let other_session = ActiveHostSessionId::new("other-session").expect("session");
        let other_principal = NativePrincipal::Windows {
            sid: WindowsSid::new("S-1-5-21-1").expect("sid"),
            wts_session_id: 8,
        };
        let other_version = DisclaimerVersion::new("other-policy").expect("version");
        let base = context(&host, &session, &principal, &version);

        let cases = [
            (
                DirectResumeValidationContext {
                    expected_host_identity: &other_host,
                    ..base
                },
                DirectResumeError::HostIdentityMismatch,
            ),
            (
                DirectResumeValidationContext {
                    expected_active_session_id: &other_session,
                    ..base
                },
                DirectResumeError::ActiveSessionMismatch,
            ),
            (
                DirectResumeValidationContext {
                    expected_native_principal: &other_principal,
                    ..base
                },
                DirectResumeError::NativePrincipalMismatch,
            ),
            (
                DirectResumeValidationContext {
                    expected_holder_nonce: DeckHolderNonce::new([9; 32]),
                    ..base
                },
                DirectResumeError::HolderNonceMismatch,
            ),
            (
                DirectResumeValidationContext {
                    expected_generation: 5,
                    ..base
                },
                DirectResumeError::GenerationMismatch,
            ),
            (
                DirectResumeValidationContext {
                    expected_nonce: DirectResumeNonce::new([9; 32]),
                    ..base
                },
                DirectResumeError::NonceMismatch,
            ),
            (
                DirectResumeValidationContext {
                    expected_disclaimer_digest: digest(9),
                    ..base
                },
                DirectResumeError::DisclaimerDigestMismatch,
            ),
            (
                DirectResumeValidationContext {
                    expected_disclaimer_version: &other_version,
                    ..base
                },
                DirectResumeError::DisclaimerVersionMismatch,
            ),
        ];
        for (context, expected) in cases {
            assert_eq!(
                validate_direct_resume_grant(&TestHmacAdapter, &token, context),
                Err(DirectResumeValidationError::Claims(expected))
            );
        }
    }

    #[test]
    fn timing_boundaries_accept_exact_max_and_reject_future_expired_zero_and_overlong() {
        let (claims, host, session, principal, version) = fixtures();
        assert_eq!(
            sign_direct_resume_grant(&TestHmacAdapter, claims.clone(), 99),
            Err(DirectResumeSigningError::Claims(
                DirectResumeError::IssuedInFuture
            ))
        );
        let token =
            sign_direct_resume_grant(&TestHmacAdapter, claims, 100).expect("signed direct grant");
        let expired = DirectResumeValidationContext {
            now_epoch_seconds: 200,
            ..context(&host, &session, &principal, &version)
        };
        assert_eq!(
            validate_direct_resume_grant(&TestHmacAdapter, &token, expired),
            Err(DirectResumeValidationError::Claims(
                DirectResumeError::Expired
            ))
        );

        let (claims, ..) = fixtures();
        let common = (
            claims.host_identity.clone(),
            claims.active_session_id.clone(),
            claims.native_principal.clone(),
            claims.holder_nonce,
            claims.generation,
            claims.nonce,
            claims.disclaimer_digest,
            claims.disclaimer_version.clone(),
        );
        assert_eq!(
            DirectResumeGrantClaims::new(
                common.0.clone(),
                common.1.clone(),
                common.2.clone(),
                common.3,
                common.4,
                common.5,
                common.6,
                common.7.clone(),
                100,
                100
            ),
            Err(DirectResumeError::InvalidLifetime)
        );
        assert!(
            DirectResumeGrantClaims::new(
                common.0.clone(),
                common.1.clone(),
                common.2.clone(),
                common.3,
                common.4,
                common.5,
                common.6,
                common.7.clone(),
                100,
                100 + MAX_DIRECT_RESUME_LIFETIME_SECONDS
            )
            .is_ok()
        );
        assert_eq!(
            DirectResumeGrantClaims::new(
                common.0,
                common.1,
                common.2,
                common.3,
                common.4,
                common.5,
                common.6,
                common.7,
                100,
                100 + MAX_DIRECT_RESUME_LIFETIME_SECONDS + 1
            ),
            Err(DirectResumeError::InvalidLifetime)
        );
    }

    #[test]
    fn fixed_fields_tamper_and_string_bounds_fail_closed() {
        assert_eq!(
            WindowsSid::new("not-a-sid"),
            Err(DirectResumeError::InvalidWindowsSid)
        );
        assert_eq!(
            WindowsSid::new("S-"),
            Err(DirectResumeError::InvalidWindowsSid)
        );
        assert_eq!(
            ActiveHostSessionId::new("x".repeat(MAX_DIRECT_RESUME_SESSION_ID_BYTES + 1)),
            Err(DirectResumeError::ActiveSessionTooLong)
        );
        assert_eq!(
            DisclaimerVersion::new(""),
            Err(DirectResumeError::EmptyDisclaimerVersion)
        );

        let (token, host, session, principal, version) = token();
        let envelope = token.decode().expect("decode");
        let json = serde_json::to_value(&envelope).expect("json");
        for (field, replacement, expected) in [
            (
                "schema",
                serde_json::json!("unknown"),
                DirectResumeError::UnsupportedSchema,
            ),
            (
                "version",
                serde_json::json!(2),
                DirectResumeError::UnsupportedClaimsVersion,
            ),
            (
                "audience",
                serde_json::json!("gateway"),
                DirectResumeError::AudienceMismatch,
            ),
            (
                "algorithm",
                serde_json::json!("none"),
                DirectResumeError::AlgorithmMismatch,
            ),
        ] {
            let mut changed = json.clone();
            changed["claims"][field] = replacement;
            let bytes = serde_json::to_vec(&changed).expect("json");
            let changed_token =
                DirectResumeGrantToken::parse(format!("v1.{}", encode_hex(&bytes))).expect("token");
            assert_eq!(
                validate_direct_resume_grant(
                    &TestHmacAdapter,
                    &changed_token,
                    context(&host, &session, &principal, &version)
                ),
                Err(DirectResumeValidationError::Claims(expected))
            );
        }
    }
}
