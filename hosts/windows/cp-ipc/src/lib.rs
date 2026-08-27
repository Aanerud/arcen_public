//! Framing + sealed-credential contract for the control pipe between the
//! Arcen LocalSystem broker and the native Windows Credential Provider.
//!
//! # What this crate is
//!
//! This is the single, platform-neutral source of truth for:
//!
//! * the **wire schema** exchanged on the SYSTEM-only named pipe (readiness,
//!   sealed-credential push, acknowledgements, session-complete);
//! * the **sealed-credential envelope** ([`crypto`]) — X25519 + HKDF-SHA256 +
//!   AES-256-GCM, bound to a per-Advise transcript; and
//! * the **credential payload** ([`payload`]) and the CP-side one-shot
//!   **autologon state machine** ([`cp_session`]).
//!
//! It builds and unit-tests on every dev OS. The named-pipe transport itself and
//! the peer identity checks live in the platform crates (the LocalSystem host and
//! the COM provider); this crate deliberately performs no I/O.
//!
//! # Threat model & scope
//!
//! Neither `Role`, `pid`, a replay nonce, nor a decryptable envelope
//! *authenticate* a process. The transport must independently enforce a
//! SYSTEM-only pipe ACL and verify its peer (LogonUI.exe + SYSTEM on the broker
//! side; the configured SYSTEM broker on the provider side), and both ends must
//! re-check Windows session/token state before acting. The crypto layer only
//! guarantees that a credential can be read solely by the provider instance that
//! published the matching `Ready`, and only once.
//!
//! No message carries a plaintext password. The single credential-bearing
//! message ([`PushCredentials`]) carries an authenticated ciphertext whose key is
//! established per Advise lifecycle and whose associated data binds the protocol
//! version, both ephemeral public keys, the challenge, the account SID, and a
//! single-use request id. Replay protection and single-use expiry are mandatory
//! and enforced by [`NonceTracker`] and [`cp_session::AutologonState`].
//!
//! # Framing
//!
//! Every message is a single frame: a 4-byte big-endian unsigned length prefix
//! followed by exactly that many bytes of UTF-8 JSON. Frames larger than
//! [`MAX_FRAME_LEN`] are refused before any allocation so a hostile or buggy
//! peer cannot exhaust memory.

use serde::{Deserialize, Serialize};

pub mod broker;
pub mod cp_session;
pub mod crypto;
pub mod payload;
pub mod transport;

pub use crypto::{CryptoError, EphemeralRecipient, SealedCredential, Transcript};
pub use payload::{CredentialPayload, PayloadError};
pub use transport::{FrameIo, StreamFrames, TransportError};

/// Name of the SYSTEM-only control pipe the broker serves and the provider
/// connects to. The transport crates apply the `D:(A;;FA;;;SY)` SDDL and the
/// peer checks; the name alone is not a capability.
pub const CP_PIPE_NAME: &str = r"\\.\pipe\arcen-credential-provider";

/// Wire protocol version. Bumped to 2 for the sealed-credential push contract.
pub const PROTOCOL_VERSION: u16 = 2;

/// Hard cap on a single encoded frame (length prefix excluded). Sized to hold a
/// hex-encoded sealed credential (bounded by [`crypto::MAX_CIPHERTEXT_LEN`])
/// plus the surrounding JSON, and still refuse abuse.
pub const MAX_FRAME_LEN: usize = 16 * 1024;

/// Cap on any single human/identity string carried on the wire.
pub const MAX_STRING_LEN: usize = 256;

/// Number of most-recent peer nonces a [`NonceTracker`] remembers for replay
/// rejection. Sized for the handful of control messages exchanged per logon.
pub const NONCE_HISTORY: usize = 64;

/// Volatile-zero a byte buffer. Used by [`crypto`] to scrub transient
/// plaintext/key buffers; exposed crate-wide so every scrub uses one primitive.
pub(crate) fn zeroize_slice(buffer: &mut [u8]) {
    use zeroize::Zeroize;
    buffer.zeroize();
}

/// Errors produced while validating or decoding a control message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolError {
    /// The declared frame length exceeds [`MAX_FRAME_LEN`].
    FrameTooLarge { declared: usize, max: usize },
    /// The buffer was shorter than the declared frame (or the 4-byte prefix).
    FrameTruncated,
    /// A full-frame decoder received bytes after the one declared JSON body.
    FrameTrailingData,
    /// JSON (de)serialization failed.
    Malformed(String),
    /// The message failed a semantic validation rule.
    Invalid(&'static str),
    /// A hex-encoded field was not valid lowercase hex of the expected length.
    InvalidHex(&'static str),
    /// The peer reused a nonce we have already seen.
    ReplayedNonce(u64),
}

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::FrameTooLarge { declared, max } => {
                write!(f, "control frame too large: {declared} > {max}")
            }
            Self::FrameTruncated => write!(f, "control frame truncated"),
            Self::FrameTrailingData => write!(f, "control frame has trailing data"),
            Self::Malformed(detail) => write!(f, "malformed control frame: {detail}"),
            Self::Invalid(reason) => write!(f, "invalid control message: {reason}"),
            Self::InvalidHex(field) => write!(f, "invalid hex field: {field}"),
            Self::ReplayedNonce(nonce) => write!(f, "replayed control nonce: {nonce}"),
        }
    }
}

impl std::error::Error for ProtocolError {}

/// Lowercase-hex encode a byte slice.
pub fn encode_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
}

/// Decode a lowercase-hex string into bytes, bounding the output length.
pub fn decode_hex(
    value: &str,
    field: &'static str,
    max_bytes: usize,
) -> Result<Vec<u8>, ProtocolError> {
    let bytes = value.as_bytes();
    if bytes.len() & 1 != 0 || bytes.len() / 2 > max_bytes {
        return Err(ProtocolError::InvalidHex(field));
    }
    let mut out = Vec::with_capacity(bytes.len() / 2);
    let mut index = 0;
    while index < bytes.len() {
        let high = hex_value(bytes[index]).ok_or(ProtocolError::InvalidHex(field))?;
        let low = hex_value(bytes[index + 1]).ok_or(ProtocolError::InvalidHex(field))?;
        out.push((high << 4) | low);
        index += 2;
    }
    Ok(out)
}

/// Decode a fixed-length lowercase-hex string into an `N`-byte array.
pub fn decode_hex_fixed<const N: usize>(
    value: &str,
    field: &'static str,
) -> Result<[u8; N], ProtocolError> {
    let bytes = decode_hex(value, field, N)?;
    if bytes.len() != N {
        return Err(ProtocolError::InvalidHex(field));
    }
    let mut out = [0u8; N];
    out.copy_from_slice(&bytes);
    Ok(out)
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

/// Case-insensitive comparison of a full process image path's file name against
/// an expected basename (e.g. checking a pipe peer is `LogonUI.exe`). Pure so the
/// peer-image rule has one tested implementation shared by both pipe endpoints.
pub fn image_basename_matches(image_path: &str, expected_basename: &str) -> bool {
    let file = image_path.rsplit(['\\', '/']).next().unwrap_or(image_path);
    !file.is_empty() && file.eq_ignore_ascii_case(expected_basename)
}

/// Side a future sender claims to represent. This value is not authentication.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    /// The credential provider DLL loaded inside LogonUI.
    Provider,
    /// The LocalSystem broker (the Windows host process).
    Broker,
}

/// Winlogon usage scenario the CP is currently serving. Mirrors the subset of
/// `CREDENTIAL_PROVIDER_USAGE_SCENARIO` the provider supports; other scenarios
/// are never reported because the CP refuses them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UsageScenario {
    /// `CPUS_LOGON`.
    Logon,
    /// `CPUS_UNLOCK_WORKSTATION`.
    UnlockWorkstation,
}

/// Opening handshake. Establishes protocol version, author role, and the first
/// replay nonce.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hello {
    pub protocol_version: u16,
    pub role: Role,
    pub nonce: u64,
}

/// The CP announces it is loaded and ready to serve an interactive logon, and
/// publishes the ephemeral public key + challenge the broker must seal against.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ready {
    /// Canonical registered CLSID string, `{XXXXXXXX-....}` form.
    pub clsid: String,
    pub usage: UsageScenario,
    /// LogonUI-hosting process id, for the broker's peer cross-check.
    pub pid: u32,
    /// The provider's ephemeral X25519 public key, lowercase hex (64 chars).
    /// Fresh every Advise lifecycle.
    pub cp_public: String,
    /// The provider's per-lifecycle challenge, lowercase hex (64 chars).
    pub challenge: String,
    pub nonce: u64,
}

impl std::fmt::Debug for Ready {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Public key + challenge are not secret, but keep readiness logs terse.
        f.debug_struct("Ready")
            .field("clsid", &self.clsid)
            .field("usage", &self.usage)
            .field("pid", &self.pid)
            .field("cp_public_len", &self.cp_public.len())
            .field("challenge_len", &self.challenge.len())
            .field("nonce", &self.nonce)
            .finish()
    }
}

impl Ready {
    /// Decode and validate the ephemeral public key into fixed-size bytes.
    pub fn cp_public_bytes(&self) -> Result<[u8; crypto::PUBLIC_KEY_LEN], ProtocolError> {
        decode_hex_fixed(&self.cp_public, "cp_public")
    }

    /// Decode and validate the challenge into fixed-size bytes.
    pub fn challenge_bytes(&self) -> Result<[u8; crypto::CHALLENGE_LEN], ProtocolError> {
        decode_hex_fixed(&self.challenge, "challenge")
    }
}

/// The broker hands the provider a sealed credential to auto-submit at the
/// console. Carries only ciphertext and its public transcript inputs — never a
/// plaintext secret. The provider decrypts it with the ephemeral key it
/// published in [`Ready`].
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PushCredentials {
    /// Single-use request id, bound into the AEAD associated data. Distinct from
    /// the transport `nonce`, which guards message replay.
    pub request_id: u64,
    /// The authenticated account's SID string (e.g. `S-1-5-21-...`), bound into
    /// the AEAD associated data. Not a secret; its integrity comes from the tag.
    pub account_sid: String,
    /// The broker's ephemeral X25519 public key, lowercase hex (64 chars).
    pub broker_public: String,
    /// The AES-256-GCM nonce, lowercase hex (24 chars).
    pub aead_nonce: String,
    /// The ciphertext-with-tag, lowercase hex.
    pub ciphertext: String,
    /// Transport replay nonce.
    pub nonce: u64,
}

impl std::fmt::Debug for PushCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The ciphertext is encrypted, but there is never a reason to spill it
        // into a log; show only lengths and the non-secret binding inputs.
        f.debug_struct("PushCredentials")
            .field("request_id", &self.request_id)
            .field("account_sid", &self.account_sid)
            .field("broker_public_len", &self.broker_public.len())
            .field("aead_nonce_len", &self.aead_nonce.len())
            .field("ciphertext_len", &self.ciphertext.len())
            .field("nonce", &self.nonce)
            .finish()
    }
}

impl PushCredentials {
    /// Rebuild the [`SealedCredential`] from the hex wire fields, bounds-checked.
    pub fn to_sealed(&self) -> Result<SealedCredential, ProtocolError> {
        let broker_public =
            decode_hex_fixed::<{ crypto::PUBLIC_KEY_LEN }>(&self.broker_public, "broker_public")?;
        let aead_nonce = decode_hex_fixed::<12>(&self.aead_nonce, "aead_nonce")?;
        let ciphertext = decode_hex(&self.ciphertext, "ciphertext", crypto::MAX_CIPHERTEXT_LEN)?;
        SealedCredential::from_parts(broker_public, aead_nonce, ciphertext)
            .map_err(|_| ProtocolError::Invalid("ciphertext length out of bounds"))
    }

    /// Build a push message from a sealed credential and its transcript inputs.
    pub fn from_sealed(
        request_id: u64,
        account_sid: impl Into<String>,
        sealed: &SealedCredential,
        nonce: u64,
    ) -> Self {
        Self {
            request_id,
            account_sid: account_sid.into(),
            broker_public: encode_hex(sealed.broker_public()),
            aead_nonce: encode_hex(sealed.nonce()),
            ciphertext: encode_hex(sealed.ciphertext()),
            nonce,
        }
    }
}

/// The provider acknowledges a credential push: `armed` is true once it has
/// decrypted the credential and marked a one-shot autologon. Carries no secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PushAck {
    /// Echoes the request id of the push being acknowledged.
    pub request_id: u64,
    pub armed: bool,
    pub nonce: u64,
}

/// The CP reports a Winlogon serialization completed for an account, so the
/// broker can retry binding the now-created (or unlocked) WTS session.
///
/// Carries only the *identity* that signed in — never a secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionComplete {
    pub username: String,
    pub domain: String,
    pub nonce: u64,
}

/// Broker acknowledgement of a prior message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ack {
    /// Echoes the nonce of the message being acknowledged.
    pub nonce: u64,
    pub accepted: bool,
}

/// Every notification represented by the schema.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum CpMessage {
    Hello(Hello),
    Ready(Ready),
    PushCredentials(PushCredentials),
    PushAck(PushAck),
    SessionComplete(SessionComplete),
    Ack(Ack),
}

fn check_string(value: &str) -> Result<(), ProtocolError> {
    if value.len() > MAX_STRING_LEN {
        return Err(ProtocolError::Invalid(
            "string field exceeds MAX_STRING_LEN",
        ));
    }
    if value.contains('\0') {
        return Err(ProtocolError::Invalid("string field contains NUL"));
    }
    Ok(())
}

/// A CLSID string is `{8-4-4-4-12}` hex with braces: 38 chars total.
pub fn is_canonical_clsid(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 38 {
        return false;
    }
    if bytes[0] != b'{' || bytes[37] != b'}' {
        return false;
    }
    // Dash positions inside the braces at overall indices 9, 14, 19, 24.
    const DASHES: [usize; 4] = [9, 14, 19, 24];
    for (index, &byte) in bytes.iter().enumerate().take(37).skip(1) {
        if DASHES.contains(&index) {
            if byte != b'-' {
                return false;
            }
        } else if !byte.is_ascii_hexdigit() {
            return false;
        }
    }
    true
}

impl CpMessage {
    /// The replay nonce carried by this message.
    pub fn nonce(&self) -> u64 {
        match self {
            Self::Hello(m) => m.nonce,
            Self::Ready(m) => m.nonce,
            Self::PushCredentials(m) => m.nonce,
            Self::PushAck(m) => m.nonce,
            Self::SessionComplete(m) => m.nonce,
            Self::Ack(m) => m.nonce,
        }
    }

    /// Enforce every semantic rule. Callers must run this on the *decoded*
    /// message before acting on it, on both ends of the pipe.
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.nonce() == 0 {
            return Err(ProtocolError::Invalid("nonce must be non-zero"));
        }
        match self {
            Self::Hello(m) => {
                if m.protocol_version != PROTOCOL_VERSION {
                    return Err(ProtocolError::Invalid("protocol version mismatch"));
                }
                Ok(())
            }
            Self::Ready(m) => {
                if !is_canonical_clsid(&m.clsid) {
                    return Err(ProtocolError::Invalid("clsid is not canonical"));
                }
                if m.pid == 0 {
                    return Err(ProtocolError::Invalid("pid must be non-zero"));
                }
                // Public key and challenge must be exact-length lowercase hex.
                m.cp_public_bytes()?;
                m.challenge_bytes()?;
                Ok(())
            }
            Self::PushCredentials(m) => {
                if m.request_id == 0 {
                    return Err(ProtocolError::Invalid("request id must be non-zero"));
                }
                if m.account_sid.is_empty() {
                    return Err(ProtocolError::Invalid("account sid must not be empty"));
                }
                check_string(&m.account_sid)?;
                // Every crypto field must decode to its exact bound.
                m.to_sealed()?;
                Ok(())
            }
            Self::PushAck(m) => {
                if m.request_id == 0 {
                    return Err(ProtocolError::Invalid("request id must be non-zero"));
                }
                Ok(())
            }
            Self::SessionComplete(m) => {
                if m.username.is_empty() {
                    return Err(ProtocolError::Invalid("username must not be empty"));
                }
                check_string(&m.username)?;
                check_string(&m.domain)?;
                Ok(())
            }
            Self::Ack(_) => Ok(()),
        }
    }

    /// Serialize and frame this message: 4-byte big-endian length prefix + JSON.
    pub fn encode(&self) -> Result<Vec<u8>, ProtocolError> {
        self.validate()?;
        let body = serde_json::to_vec(self).map_err(|e| ProtocolError::Malformed(e.to_string()))?;
        if body.len() > MAX_FRAME_LEN {
            return Err(ProtocolError::FrameTooLarge {
                declared: body.len(),
                max: MAX_FRAME_LEN,
            });
        }
        let mut framed = Vec::with_capacity(4 + body.len());
        framed.extend_from_slice(&(body.len() as u32).to_be_bytes());
        framed.extend_from_slice(&body);
        Ok(framed)
    }

    /// Decode a message body (the JSON payload without the length prefix) and
    /// validate it. Use with [`read_frame_len`] when reading from a stream.
    pub fn decode_body(body: &[u8]) -> Result<Self, ProtocolError> {
        if body.len() > MAX_FRAME_LEN {
            return Err(ProtocolError::FrameTooLarge {
                declared: body.len(),
                max: MAX_FRAME_LEN,
            });
        }
        let message: CpMessage =
            serde_json::from_slice(body).map_err(|e| ProtocolError::Malformed(e.to_string()))?;
        message.validate()?;
        Ok(message)
    }

    /// Decode a full frame (length prefix + body), validating the declared
    /// length against [`MAX_FRAME_LEN`] before touching the body.
    pub fn decode_frame(frame: &[u8]) -> Result<Self, ProtocolError> {
        let declared = read_frame_len(frame)?;
        let body = frame
            .get(4..4 + declared)
            .ok_or(ProtocolError::FrameTruncated)?;
        if frame.len() != 4 + declared {
            return Err(ProtocolError::FrameTrailingData);
        }
        Self::decode_body(body)
    }
}

/// Parse and bound-check the 4-byte big-endian length prefix. Returns the
/// declared body length. Rejects any length over [`MAX_FRAME_LEN`] so a caller
/// never allocates an attacker-chosen buffer.
pub fn read_frame_len(prefixed: &[u8]) -> Result<usize, ProtocolError> {
    let raw = prefixed.get(0..4).ok_or(ProtocolError::FrameTruncated)?;
    let declared = u32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]) as usize;
    if declared > MAX_FRAME_LEN {
        return Err(ProtocolError::FrameTooLarge {
            declared,
            max: MAX_FRAME_LEN,
        });
    }
    Ok(declared)
}

/// Bounded, allocation-light replay-nonce guard. Remembers the last
/// [`NONCE_HISTORY`] accepted nonces and refuses any repeat.
#[derive(Debug, Default)]
pub struct NonceTracker {
    seen: std::collections::VecDeque<u64>,
}

impl NonceTracker {
    pub fn new() -> Self {
        Self {
            seen: std::collections::VecDeque::with_capacity(NONCE_HISTORY),
        }
    }

    /// Accept `nonce` if non-zero and not recently seen; records it on success.
    pub fn accept(&mut self, nonce: u64) -> Result<(), ProtocolError> {
        if nonce == 0 {
            return Err(ProtocolError::Invalid("nonce must be non-zero"));
        }
        if self.seen.contains(&nonce) {
            return Err(ProtocolError::ReplayedNonce(nonce));
        }
        if self.seen.len() == NONCE_HISTORY {
            self.seen.pop_front();
        }
        self.seen.push_back(nonce);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_ready() -> CpMessage {
        CpMessage::Ready(Ready {
            clsid: "{2FBE34F2-9E7A-42FA-BFBF-44897694BE60}".to_string(),
            usage: UsageScenario::Logon,
            pid: 4321,
            cp_public: encode_hex(&[0x11u8; 32]),
            challenge: encode_hex(&[0x22u8; 32]),
            nonce: 7,
        })
    }

    fn sample_push() -> CpMessage {
        CpMessage::PushCredentials(PushCredentials {
            request_id: 99,
            account_sid: "S-1-5-21-1-2-3-1001".to_string(),
            broker_public: encode_hex(&[0x33u8; 32]),
            aead_nonce: encode_hex(&[0x44u8; 12]),
            ciphertext: encode_hex(&[0x55u8; 48]),
            nonce: 8,
        })
    }

    #[test]
    fn hello_roundtrips_through_a_frame() {
        let hello = CpMessage::Hello(Hello {
            protocol_version: PROTOCOL_VERSION,
            role: Role::Provider,
            nonce: 1,
        });
        let framed = hello.encode().expect("encode");
        assert_eq!(read_frame_len(&framed).expect("len"), framed.len() - 4);
        let decoded = CpMessage::decode_frame(&framed).expect("decode");
        assert_eq!(decoded, hello);
    }

    #[test]
    fn ready_requires_canonical_clsid() {
        let bad = CpMessage::Ready(Ready {
            clsid: "not-a-guid".to_string(),
            usage: UsageScenario::Logon,
            pid: 10,
            cp_public: encode_hex(&[0x11u8; 32]),
            challenge: encode_hex(&[0x22u8; 32]),
            nonce: 2,
        });
        assert_eq!(
            bad.validate(),
            Err(ProtocolError::Invalid("clsid is not canonical"))
        );
        assert!(sample_ready().validate().is_ok());
    }

    #[test]
    fn ready_requires_exact_hex_public_key_and_challenge() {
        let short_key = CpMessage::Ready(Ready {
            clsid: "{2FBE34F2-9E7A-42FA-BFBF-44897694BE60}".to_string(),
            usage: UsageScenario::Logon,
            pid: 10,
            cp_public: encode_hex(&[0x11u8; 31]),
            challenge: encode_hex(&[0x22u8; 32]),
            nonce: 2,
        });
        assert_eq!(
            short_key.validate(),
            Err(ProtocolError::InvalidHex("cp_public"))
        );
        let bad_challenge = CpMessage::Ready(Ready {
            clsid: "{2FBE34F2-9E7A-42FA-BFBF-44897694BE60}".to_string(),
            usage: UsageScenario::Logon,
            pid: 10,
            cp_public: encode_hex(&[0x11u8; 32]),
            challenge: "zz".to_string(),
            nonce: 2,
        });
        assert_eq!(
            bad_challenge.validate(),
            Err(ProtocolError::InvalidHex("challenge"))
        );
    }

    #[test]
    fn push_credentials_roundtrips_and_validates_bounds() {
        let push = sample_push();
        let framed = push.encode().expect("encode");
        let decoded = CpMessage::decode_frame(&framed).expect("decode");
        assert_eq!(decoded, push);

        // Ciphertext shorter than a tag is rejected.
        let too_short = CpMessage::PushCredentials(PushCredentials {
            request_id: 1,
            account_sid: "S-1-5-18".to_string(),
            broker_public: encode_hex(&[0u8; 32]),
            aead_nonce: encode_hex(&[0u8; 12]),
            ciphertext: encode_hex(&[0u8; 4]),
            nonce: 1,
        });
        assert!(matches!(
            too_short.validate(),
            Err(ProtocolError::Invalid(_))
        ));

        // Zero request id is rejected.
        let zero_request = CpMessage::PushCredentials(PushCredentials {
            request_id: 0,
            account_sid: "S-1-5-18".to_string(),
            broker_public: encode_hex(&[0u8; 32]),
            aead_nonce: encode_hex(&[0u8; 12]),
            ciphertext: encode_hex(&[0u8; 48]),
            nonce: 1,
        });
        assert_eq!(
            zero_request.validate(),
            Err(ProtocolError::Invalid("request id must be non-zero"))
        );
    }

    #[test]
    fn push_ack_roundtrips() {
        let ack = CpMessage::PushAck(PushAck {
            request_id: 42,
            armed: true,
            nonce: 5,
        });
        let framed = ack.encode().expect("encode");
        assert_eq!(CpMessage::decode_frame(&framed).expect("decode"), ack);
    }

    #[test]
    fn hex_helpers_roundtrip_and_reject_bad_input() {
        let bytes = [0x00u8, 0x0f, 0xa5, 0xff];
        assert_eq!(encode_hex(&bytes), "000fa5ff");
        assert_eq!(decode_hex("000fa5ff", "t", 4).unwrap(), bytes);
        assert_eq!(
            decode_hex("abc", "t", 4),
            Err(ProtocolError::InvalidHex("t"))
        ); // odd length
        assert_eq!(
            decode_hex("00ZZ", "t", 4),
            Err(ProtocolError::InvalidHex("t"))
        ); // non-hex
        assert_eq!(
            decode_hex("0011", "t", 1),
            Err(ProtocolError::InvalidHex("t"))
        ); // over bound
        assert!(decode_hex_fixed::<2>("0011", "t").is_ok());
        assert_eq!(
            decode_hex_fixed::<3>("0011", "t"),
            Err(ProtocolError::InvalidHex("t"))
        );
    }

    #[test]
    fn image_basename_match_is_case_insensitive_and_basename_only() {
        assert!(image_basename_matches(
            r"C:\Windows\System32\LogonUI.exe",
            "LogonUI.exe"
        ));
        assert!(image_basename_matches("logonui.EXE", "LogonUI.exe"));
        assert!(image_basename_matches(
            "/mnt/c/Windows/System32/LogonUI.exe",
            "LogonUI.exe"
        ));
        assert!(!image_basename_matches(
            r"C:\evil\LogonUI.exe.evil.exe",
            "LogonUI.exe"
        ));
        assert!(!image_basename_matches(r"C:\x\explorer.exe", "LogonUI.exe"));
        assert!(!image_basename_matches("", "LogonUI.exe"));
        assert!(!image_basename_matches(r"C:\x\", "LogonUI.exe"));
    }

    #[test]
    fn canonical_clsid_shape_is_strict() {
        assert!(is_canonical_clsid("{2FBE34F2-9E7A-42FA-BFBF-44897694BE60}"));
        assert!(!is_canonical_clsid("2FBE34F2-9E7A-42FA-BFBF-44897694BE60")); // no braces
        assert!(!is_canonical_clsid("{EB964364-F25C-4579-A9DE-4514C90F1B3}")); // short
        assert!(!is_canonical_clsid(
            "{EB964364xF25C-4579-A9DE-4514C90F1B39}"
        )); // bad dash
        assert!(!is_canonical_clsid(
            "{ZB964364-F25C-4579-A9DE-4514C90F1B39}"
        )); // non-hex
    }

    #[test]
    fn zero_nonce_is_always_invalid() {
        let m = CpMessage::Hello(Hello {
            protocol_version: PROTOCOL_VERSION,
            role: Role::Broker,
            nonce: 0,
        });
        assert_eq!(
            m.validate(),
            Err(ProtocolError::Invalid("nonce must be non-zero"))
        );
    }

    #[test]
    fn protocol_version_mismatch_is_rejected() {
        let m = CpMessage::Hello(Hello {
            protocol_version: PROTOCOL_VERSION + 1,
            role: Role::Broker,
            nonce: 3,
        });
        assert_eq!(
            m.validate(),
            Err(ProtocolError::Invalid("protocol version mismatch"))
        );
    }

    #[test]
    fn session_complete_bounds_are_enforced() {
        let too_long = "u".repeat(MAX_STRING_LEN + 1);
        let m = CpMessage::SessionComplete(SessionComplete {
            username: too_long,
            domain: "CORP".to_string(),
            nonce: 4,
        });
        assert!(matches!(m.validate(), Err(ProtocolError::Invalid(_))));

        let empty = CpMessage::SessionComplete(SessionComplete {
            username: String::new(),
            domain: "CORP".to_string(),
            nonce: 4,
        });
        assert_eq!(
            empty.validate(),
            Err(ProtocolError::Invalid("username must not be empty"))
        );

        let nul = CpMessage::SessionComplete(SessionComplete {
            username: "a\0b".to_string(),
            domain: String::new(),
            nonce: 4,
        });
        assert_eq!(
            nul.validate(),
            Err(ProtocolError::Invalid("string field contains NUL"))
        );

        let ok = CpMessage::SessionComplete(SessionComplete {
            username: "alice".to_string(),
            domain: "CORP".to_string(),
            nonce: 4,
        });
        assert!(ok.validate().is_ok());
    }

    #[test]
    fn schema_rejects_remote_password_fields() {
        let body = br#"{"type":"Hello","protocol_version":1,"role":"Provider","nonce":9,"password":"never"}"#;
        assert!(matches!(
            CpMessage::decode_body(body),
            Err(ProtocolError::Malformed(_))
        ));
    }

    #[test]
    fn oversize_frame_is_refused_without_decoding() {
        // Craft a prefix that lies about a huge body.
        let mut buf = ((MAX_FRAME_LEN + 1) as u32).to_be_bytes().to_vec();
        buf.extend_from_slice(b"{}");
        assert_eq!(
            read_frame_len(&buf),
            Err(ProtocolError::FrameTooLarge {
                declared: MAX_FRAME_LEN + 1,
                max: MAX_FRAME_LEN,
            })
        );
        assert!(matches!(
            CpMessage::decode_frame(&buf),
            Err(ProtocolError::FrameTooLarge { .. })
        ));
    }

    #[test]
    fn truncated_frame_is_refused() {
        assert_eq!(read_frame_len(&[0, 0]), Err(ProtocolError::FrameTruncated));
        let mut buf = 10u32.to_be_bytes().to_vec();
        buf.extend_from_slice(b"short"); // declares 10, provides 5
        assert_eq!(
            CpMessage::decode_frame(&buf),
            Err(ProtocolError::FrameTruncated)
        );
    }

    #[test]
    fn trailing_frame_data_is_refused() {
        let mut framed = sample_ready().encode().unwrap();
        framed.extend_from_slice(b"trailing");
        assert_eq!(
            CpMessage::decode_frame(&framed),
            Err(ProtocolError::FrameTrailingData)
        );
    }

    #[test]
    fn encode_validates_before_serializing() {
        let invalid = CpMessage::Hello(Hello {
            protocol_version: PROTOCOL_VERSION,
            role: Role::Provider,
            nonce: 0,
        });
        assert_eq!(
            invalid.encode(),
            Err(ProtocolError::Invalid("nonce must be non-zero"))
        );
    }

    #[test]
    fn nonce_tracker_rejects_replays_and_zero() {
        let mut tracker = NonceTracker::new();
        assert!(tracker.accept(1).is_ok());
        assert!(tracker.accept(2).is_ok());
        assert_eq!(tracker.accept(1), Err(ProtocolError::ReplayedNonce(1)));
        assert_eq!(
            tracker.accept(0),
            Err(ProtocolError::Invalid("nonce must be non-zero"))
        );
    }

    #[test]
    fn nonce_tracker_forgets_beyond_history_window() {
        let mut tracker = NonceTracker::new();
        for nonce in 1..=(NONCE_HISTORY as u64 + 1) {
            assert!(tracker.accept(nonce).is_ok());
        }
        // Nonce 1 has aged out of the window and is accepted again.
        assert!(tracker.accept(1).is_ok());
        // The newest nonce is still remembered.
        assert_eq!(
            tracker.accept(NONCE_HISTORY as u64 + 1),
            Err(ProtocolError::ReplayedNonce(NONCE_HISTORY as u64 + 1))
        );
    }

    #[test]
    fn malformed_json_is_reported() {
        let mut buf = 3u32.to_be_bytes().to_vec();
        buf.extend_from_slice(b"{ x");
        assert!(matches!(
            CpMessage::decode_frame(&buf),
            Err(ProtocolError::Malformed(_))
        ));
    }
}
