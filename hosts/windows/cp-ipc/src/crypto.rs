//! Ephemeral authenticated encryption for the sealed-credential envelope.
//!
//! # Construction
//!
//! Every Advise lifecycle the credential provider generates a fresh X25519
//! [`EphemeralRecipient`] and a random 32-byte challenge, and publishes the
//! recipient's public key and the challenge in its `Ready` message. To hand a
//! credential to that provider the broker:
//!
//! 1. generates its own ephemeral X25519 key pair;
//! 2. performs X25519 key agreement against the provider's public key;
//! 3. derives a 256-bit key with `HKDF-SHA256`, salted by the challenge and
//!    bound to the full transcript (see [`Transcript`]);
//! 4. seals the [`crate::CredentialPayload`] with `AES-256-GCM` under a fresh
//!    random 96-bit nonce, using the transcript bytes as the AEAD's associated
//!    data.
//!
//! The recipient reverses this exactly once — [`EphemeralRecipient::open`]
//! consumes the ephemeral private key, so a captured envelope can never be
//! re-opened by the same provider instance. Both the derived key and every
//! plaintext buffer are zeroized on the way out.
//!
//! # What this does and does not prove
//!
//! A successfully opened envelope proves only that whoever sealed it knew the
//! recipient's ephemeral public key and challenge (i.e. observed this Advise
//! lifecycle's `Ready`) and produced a tag under the agreed key. It does *not*
//! identify the peer process. The SYSTEM-only pipe ACL and the explicit
//! LogonUI/SYSTEM peer checks in the transport crates provide that; the crypto
//! and the transport are independent, defence-in-depth layers.

use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM, NONCE_LEN};
use ring::agreement::{agree_ephemeral, EphemeralPrivateKey, UnparsedPublicKey, X25519};
use ring::hkdf::{Prk, Salt, HKDF_SHA256};
use ring::rand::{SecureRandom, SystemRandom};
use zeroize::Zeroizing;

use crate::payload::CredentialPayload;

/// Length of an X25519 public key.
pub const PUBLIC_KEY_LEN: usize = 32;
/// Length of the provider challenge.
pub const CHALLENGE_LEN: usize = 32;
/// Length of the AES-256-GCM key HKDF derives.
const AEAD_KEY_LEN: usize = 32;
/// AES-256-GCM authentication tag length.
const TAG_LEN: usize = 16;
/// Hard cap on ciphertext (plaintext + tag) so a decode never allocates an
/// attacker-chosen buffer. The bounded credential plaintext is far smaller.
pub const MAX_CIPHERTEXT_LEN: usize = 4096;
/// Maximum account-SID string length bound into the transcript.
pub const MAX_ACCOUNT_SID_LEN: usize = 256;

/// HKDF `info` / AEAD associated-data domain separation label. Bumping this
/// value invalidates every previously derived key, by design.
const AAD_CONTEXT: &[u8] = b"arcen-cp-ipc/v2/sealed-credential";

/// Errors from sealing or opening a credential envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CryptoError {
    /// A supplied public key / challenge / SID was the wrong size or shape.
    BadParameter(&'static str),
    /// Ciphertext exceeded [`MAX_CIPHERTEXT_LEN`] or was too short to hold a tag.
    CiphertextBounds,
    /// Key agreement failed (e.g. a malformed peer public key).
    Agreement,
    /// HKDF key derivation failed.
    KeyDerivation,
    /// AEAD sealing failed.
    Seal,
    /// AEAD opening failed: wrong key, tampered ciphertext/nonce, or a
    /// transcript/AAD mismatch. Deliberately indistinguishable.
    Open,
    /// The system RNG failed.
    Rng,
    /// The decrypted plaintext was not a valid [`CredentialPayload`].
    Payload(crate::payload::PayloadError),
}

impl std::fmt::Display for CryptoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadParameter(what) => write!(f, "invalid crypto parameter: {what}"),
            Self::CiphertextBounds => f.write_str("ciphertext length out of bounds"),
            Self::Agreement => f.write_str("X25519 key agreement failed"),
            Self::KeyDerivation => f.write_str("HKDF key derivation failed"),
            Self::Seal => f.write_str("AEAD seal failed"),
            Self::Open => f.write_str("AEAD open failed"),
            Self::Rng => f.write_str("system RNG failed"),
            Self::Payload(error) => write!(f, "decrypted credential payload invalid: {error}"),
        }
    }
}

impl std::error::Error for CryptoError {}

/// The transcript that binds a sealed credential to exactly one Advise
/// lifecycle, account, and request. It is fed to HKDF (as `info`) *and* used as
/// the AEAD associated data, so neither the derived key nor the ciphertext can
/// be reused against a different transcript.
#[derive(Clone, Copy)]
pub struct Transcript<'a> {
    /// Wire protocol version.
    pub protocol_version: u16,
    /// The provider's ephemeral public key from `Ready`.
    pub cp_public: &'a [u8],
    /// The broker's ephemeral public key from the sealed envelope.
    pub broker_public: &'a [u8],
    /// The provider's challenge from `Ready`.
    pub challenge: &'a [u8],
    /// The authenticated account's SID string (e.g. `S-1-5-21-...`).
    pub account_sid: &'a [u8],
    /// The broker's single-use request id.
    pub request_id: u64,
}

impl Transcript<'_> {
    fn validate(&self) -> Result<(), CryptoError> {
        if self.cp_public.len() != PUBLIC_KEY_LEN {
            return Err(CryptoError::BadParameter("cp public key length"));
        }
        if self.broker_public.len() != PUBLIC_KEY_LEN {
            return Err(CryptoError::BadParameter("broker public key length"));
        }
        if self.challenge.len() != CHALLENGE_LEN {
            return Err(CryptoError::BadParameter("challenge length"));
        }
        if self.account_sid.is_empty() || self.account_sid.len() > MAX_ACCOUNT_SID_LEN {
            return Err(CryptoError::BadParameter("account sid length"));
        }
        Ok(())
    }

    /// Canonical, unambiguous byte encoding: a fixed context label followed by
    /// every field, each length-prefixed so no two distinct transcripts can
    /// ever collide.
    fn encode_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(128);
        push_field(&mut bytes, AAD_CONTEXT);
        push_field(&mut bytes, &self.protocol_version.to_be_bytes());
        push_field(&mut bytes, self.cp_public);
        push_field(&mut bytes, self.broker_public);
        push_field(&mut bytes, self.challenge);
        push_field(&mut bytes, self.account_sid);
        push_field(&mut bytes, &self.request_id.to_be_bytes());
        bytes
    }
}

fn push_field(buffer: &mut Vec<u8>, field: &[u8]) {
    buffer.extend_from_slice(&(field.len() as u32).to_be_bytes());
    buffer.extend_from_slice(field);
}

/// A sealed credential: the broker's ephemeral public key, the AEAD nonce, and
/// the ciphertext-with-tag. Carries no key material and reveals nothing under
/// `Debug`.
#[derive(Clone, PartialEq, Eq)]
pub struct SealedCredential {
    broker_public: [u8; PUBLIC_KEY_LEN],
    nonce: [u8; NONCE_LEN],
    ciphertext: Vec<u8>,
}

impl SealedCredential {
    pub fn broker_public(&self) -> &[u8; PUBLIC_KEY_LEN] {
        &self.broker_public
    }

    pub fn nonce(&self) -> &[u8; NONCE_LEN] {
        &self.nonce
    }

    pub fn ciphertext(&self) -> &[u8] {
        &self.ciphertext
    }

    /// Reassemble from wire fields, bounds-checking the ciphertext.
    pub fn from_parts(
        broker_public: [u8; PUBLIC_KEY_LEN],
        nonce: [u8; NONCE_LEN],
        ciphertext: Vec<u8>,
    ) -> Result<Self, CryptoError> {
        if ciphertext.len() < TAG_LEN || ciphertext.len() > MAX_CIPHERTEXT_LEN {
            return Err(CryptoError::CiphertextBounds);
        }
        Ok(Self {
            broker_public,
            nonce,
            ciphertext,
        })
    }
}

impl std::fmt::Debug for SealedCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SealedCredential")
            .field("broker_public_len", &self.broker_public.len())
            .field("nonce_len", &self.nonce.len())
            .field("ciphertext_len", &self.ciphertext.len())
            .finish()
    }
}

/// Seal a credential for the provider whose ephemeral public key and challenge
/// came from `Ready`.
///
/// `request_id` must be unique per push; `account_sid` is the authenticated
/// account's SID string. Both, plus the protocol version and both public keys,
/// are bound into the AEAD so the envelope is single-context.
pub fn seal_credential(
    cp_public: &[u8],
    challenge: &[u8],
    account_sid: &[u8],
    request_id: u64,
    protocol_version: u16,
    payload: &CredentialPayload,
) -> Result<SealedCredential, CryptoError> {
    if cp_public.len() != PUBLIC_KEY_LEN {
        return Err(CryptoError::BadParameter("cp public key length"));
    }
    if challenge.len() != CHALLENGE_LEN {
        return Err(CryptoError::BadParameter("challenge length"));
    }

    let rng = SystemRandom::new();
    let broker_private =
        EphemeralPrivateKey::generate(&X25519, &rng).map_err(|_| CryptoError::Rng)?;
    let broker_public_key = broker_private
        .compute_public_key()
        .map_err(|_| CryptoError::Agreement)?;
    let mut broker_public = [0u8; PUBLIC_KEY_LEN];
    let public_bytes = broker_public_key.as_ref();
    if public_bytes.len() != PUBLIC_KEY_LEN {
        return Err(CryptoError::Agreement);
    }
    broker_public.copy_from_slice(public_bytes);

    let transcript = Transcript {
        protocol_version,
        cp_public,
        broker_public: &broker_public,
        challenge,
        account_sid,
        request_id,
    };
    transcript.validate()?;
    let transcript_bytes = transcript.encode_bytes();

    // Fresh random nonce per seal. The transcript already binds every unique
    // input, but a random nonce keeps AES-GCM safe even if a request id ever
    // repeated.
    let mut nonce = [0u8; NONCE_LEN];
    rng.fill(&mut nonce).map_err(|_| CryptoError::Rng)?;

    let key = broker_derive(cp_public, broker_private, challenge, &transcript_bytes)?;
    let sealing = aead_key(&key)?;

    let mut in_out = payload.encode().to_vec();
    let nonce_obj = Nonce::assume_unique_for_key(nonce);
    sealing
        .seal_in_place_append_tag(nonce_obj, Aad::from(&transcript_bytes), &mut in_out)
        .map_err(|_| CryptoError::Seal)?;

    // `in_out` is now ciphertext+tag. Copy it into the envelope, then scrub the
    // working buffer so no plaintext-adjacent bytes linger after this call.
    let sealed = SealedCredential::from_parts(broker_public, nonce, in_out.clone())?;
    crate::zeroize_slice(&mut in_out);
    Ok(sealed)
}

/// A provider-side ephemeral X25519 recipient generated once per Advise
/// lifecycle. [`Self::open`] consumes it, giving single-use decryption.
pub struct EphemeralRecipient {
    private: EphemeralPrivateKey,
    public: [u8; PUBLIC_KEY_LEN],
    challenge: [u8; CHALLENGE_LEN],
}

impl EphemeralRecipient {
    /// Generate a fresh recipient and challenge from the system RNG.
    pub fn generate() -> Result<Self, CryptoError> {
        let rng = SystemRandom::new();
        let private = EphemeralPrivateKey::generate(&X25519, &rng).map_err(|_| CryptoError::Rng)?;
        let public_key = private
            .compute_public_key()
            .map_err(|_| CryptoError::Agreement)?;
        let mut public = [0u8; PUBLIC_KEY_LEN];
        let bytes = public_key.as_ref();
        if bytes.len() != PUBLIC_KEY_LEN {
            return Err(CryptoError::Agreement);
        }
        public.copy_from_slice(bytes);
        let mut challenge = [0u8; CHALLENGE_LEN];
        rng.fill(&mut challenge).map_err(|_| CryptoError::Rng)?;
        Ok(Self {
            private,
            public,
            challenge,
        })
    }

    /// This recipient's ephemeral public key, published in `Ready`.
    pub fn public_key(&self) -> &[u8; PUBLIC_KEY_LEN] {
        &self.public
    }

    /// This lifecycle's challenge, published in `Ready`.
    pub fn challenge(&self) -> &[u8; CHALLENGE_LEN] {
        &self.challenge
    }

    /// Open a sealed credential, binding the transcript from this recipient's
    /// own public key and challenge plus the request's account SID and id.
    ///
    /// Consumes `self`: the ephemeral private key is destroyed by the key
    /// agreement, so the same recipient can never open a second envelope. The
    /// caller must generate a new recipient (and `Ready`) for any retry.
    pub fn open(
        self,
        sealed: &SealedCredential,
        account_sid: &[u8],
        request_id: u64,
        protocol_version: u16,
    ) -> Result<CredentialPayload, CryptoError> {
        if sealed.ciphertext.len() < TAG_LEN || sealed.ciphertext.len() > MAX_CIPHERTEXT_LEN {
            return Err(CryptoError::CiphertextBounds);
        }
        let transcript = Transcript {
            protocol_version,
            cp_public: &self.public,
            broker_public: &sealed.broker_public,
            challenge: &self.challenge,
            account_sid,
            request_id,
        };
        transcript.validate()?;
        let transcript_bytes = transcript.encode_bytes();

        let key = derive_key_recipient(
            self.private,
            &sealed.broker_public,
            &self.challenge,
            &transcript_bytes,
        )?;
        let opening = aead_key(&key)?;

        let mut in_out = sealed.ciphertext.clone();
        let nonce = Nonce::assume_unique_for_key(sealed.nonce);
        let plaintext = opening
            .open_in_place(nonce, Aad::from(&transcript_bytes), &mut in_out)
            .map_err(|_| CryptoError::Open)?;
        // Copy the plaintext into a scrubbing owner, then wipe the working
        // buffer, before parsing the payload.
        let mut scratch = Zeroizing::new(plaintext.to_vec());
        // Overwrite the decrypted region of `in_out` (which still holds the
        // opened plaintext followed by the now-meaningless tag bytes).
        crate::zeroize_slice(&mut in_out);
        let payload = CredentialPayload::decode(&scratch).map_err(CryptoError::Payload);
        crate::zeroize_slice(&mut scratch);
        payload
    }
}

impl std::fmt::Debug for EphemeralRecipient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EphemeralRecipient")
            .field("public_len", &self.public.len())
            .finish()
    }
}

/// Derive the AES-256-GCM key on the broker side: X25519 agreement against the
/// provider's public key followed by HKDF, written inline because
/// `agree_ephemeral` consumes the ephemeral private key.
fn broker_derive(
    cp_public: &[u8],
    broker_private: EphemeralPrivateKey,
    challenge: &[u8],
    transcript: &[u8],
) -> Result<Zeroizing<[u8; AEAD_KEY_LEN]>, CryptoError> {
    let peer = UnparsedPublicKey::new(&X25519, cp_public);
    agree_ephemeral(broker_private, &peer, |shared| {
        hkdf_key(challenge, shared, transcript)
    })
    .map_err(|_| CryptoError::Agreement)?
}
fn derive_key_recipient(
    private: EphemeralPrivateKey,
    broker_public: &[u8],
    challenge: &[u8],
    transcript: &[u8],
) -> Result<Zeroizing<[u8; AEAD_KEY_LEN]>, CryptoError> {
    let peer = UnparsedPublicKey::new(&X25519, broker_public);
    agree_ephemeral(private, &peer, |shared| {
        hkdf_key(challenge, shared, transcript)
    })
    .map_err(|_| CryptoError::Agreement)?
}

/// `HKDF-SHA256(salt = challenge, ikm = shared_secret, info = transcript)`.
fn hkdf_key(
    challenge: &[u8],
    shared: &[u8],
    transcript: &[u8],
) -> Result<Zeroizing<[u8; AEAD_KEY_LEN]>, CryptoError> {
    let salt = Salt::new(HKDF_SHA256, challenge);
    let prk: Prk = salt.extract(shared);
    let info = [transcript];
    let okm = prk
        .expand(&info, KeyMaterial)
        .map_err(|_| CryptoError::KeyDerivation)?;
    let mut key = Zeroizing::new([0u8; AEAD_KEY_LEN]);
    okm.fill(&mut key[..])
        .map_err(|_| CryptoError::KeyDerivation)?;
    Ok(key)
}

/// HKDF output-length marker for a 256-bit key.
struct KeyMaterial;

impl ring::hkdf::KeyType for KeyMaterial {
    fn len(&self) -> usize {
        AEAD_KEY_LEN
    }
}

fn aead_key(key: &[u8; AEAD_KEY_LEN]) -> Result<LessSafeKey, CryptoError> {
    let unbound = UnboundKey::new(&AES_256_GCM, key).map_err(|_| CryptoError::KeyDerivation)?;
    Ok(LessSafeKey::new(unbound))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aes_256_gcm_matches_the_known_answer_vector() {
        // McGrew & Viega GCM Test Case 13 (also NIST): AES-256, all-zero key and
        // 96-bit IV, empty plaintext and AAD -> empty ciphertext and the tag
        // 530f8afbc74536b9a963b4f1c4cb738b. This pins that `aead_key` really is
        // standard AES-256-GCM, independent of the higher-level envelope.
        let key = [0u8; AEAD_KEY_LEN];
        let sealing = aead_key(&key).expect("key");
        let mut in_out: Vec<u8> = Vec::new();
        sealing
            .seal_in_place_append_tag(
                Nonce::assume_unique_for_key([0u8; NONCE_LEN]),
                Aad::from([]),
                &mut in_out,
            )
            .expect("seal");
        assert_eq!(
            in_out,
            [
                0x53, 0x0f, 0x8a, 0xfb, 0xc7, 0x45, 0x36, 0xb9, 0xa9, 0x63, 0xb4, 0xf1, 0xc4, 0xcb,
                0x73, 0x8b
            ]
        );
    }

    fn sample_transcript_inputs() -> ([u8; 32], u64) {
        let mut sid = [0u8; 32];
        sid[..11].copy_from_slice(b"S-1-5-21-99");
        (sid, 0x0102_0304_0506_0708)
    }

    #[test]
    fn seal_then_open_roundtrips() {
        let recipient = EphemeralRecipient::generate().expect("recipient");
        let (sid, request_id) = sample_transcript_inputs();
        let payload = CredentialPayload::new(r"CORP\alice", "hunter2").expect("payload");
        let sealed = seal_credential(
            recipient.public_key(),
            recipient.challenge(),
            &sid[..11],
            request_id,
            2,
            &payload,
        )
        .expect("seal");
        let opened = recipient
            .open(&sealed, &sid[..11], request_id, 2)
            .expect("open");
        assert_eq!(opened.username(), r"CORP\alice");
        assert_eq!(opened.password(), "hunter2");
    }

    #[test]
    fn tampered_ciphertext_fails_to_open() {
        let recipient = EphemeralRecipient::generate().expect("recipient");
        let (sid, request_id) = sample_transcript_inputs();
        let payload = CredentialPayload::new("alice", "hunter2").expect("payload");
        let mut sealed = seal_credential(
            recipient.public_key(),
            recipient.challenge(),
            &sid[..11],
            request_id,
            2,
            &payload,
        )
        .expect("seal");
        sealed.ciphertext[0] ^= 0x01;
        assert_eq!(
            recipient.open(&sealed, &sid[..11], request_id, 2),
            Err(CryptoError::Open)
        );
    }

    #[test]
    fn tampered_nonce_fails_to_open() {
        let recipient = EphemeralRecipient::generate().expect("recipient");
        let (sid, request_id) = sample_transcript_inputs();
        let payload = CredentialPayload::new("alice", "hunter2").expect("payload");
        let mut sealed = seal_credential(
            recipient.public_key(),
            recipient.challenge(),
            &sid[..11],
            request_id,
            2,
            &payload,
        )
        .expect("seal");
        sealed.nonce[0] ^= 0xff;
        assert_eq!(
            recipient.open(&sealed, &sid[..11], request_id, 2),
            Err(CryptoError::Open)
        );
    }

    #[test]
    fn mismatched_request_id_in_aad_fails_to_open() {
        let recipient = EphemeralRecipient::generate().expect("recipient");
        let (sid, request_id) = sample_transcript_inputs();
        let payload = CredentialPayload::new("alice", "hunter2").expect("payload");
        let sealed = seal_credential(
            recipient.public_key(),
            recipient.challenge(),
            &sid[..11],
            request_id,
            2,
            &payload,
        )
        .expect("seal");
        // Opening with a different request id changes the transcript/AAD.
        assert_eq!(
            recipient.open(&sealed, &sid[..11], request_id ^ 1, 2),
            Err(CryptoError::Open)
        );
    }

    #[test]
    fn mismatched_account_sid_in_aad_fails_to_open() {
        let recipient = EphemeralRecipient::generate().expect("recipient");
        let (sid, request_id) = sample_transcript_inputs();
        let payload = CredentialPayload::new("alice", "hunter2").expect("payload");
        let sealed = seal_credential(
            recipient.public_key(),
            recipient.challenge(),
            &sid[..11],
            request_id,
            2,
            &payload,
        )
        .expect("seal");
        assert_eq!(
            recipient.open(&sealed, b"S-1-5-18", request_id, 2),
            Err(CryptoError::Open)
        );
    }

    #[test]
    fn a_different_recipient_cannot_open() {
        let recipient = EphemeralRecipient::generate().expect("recipient");
        let (sid, request_id) = sample_transcript_inputs();
        let payload = CredentialPayload::new("alice", "hunter2").expect("payload");
        let sealed = seal_credential(
            recipient.public_key(),
            recipient.challenge(),
            &sid[..11],
            request_id,
            2,
            &payload,
        )
        .expect("seal");
        // A fresh recipient has an unrelated private key and challenge.
        let attacker = EphemeralRecipient::generate().expect("recipient");
        assert_eq!(
            attacker.open(&sealed, &sid[..11], request_id, 2),
            Err(CryptoError::Open)
        );
    }

    #[test]
    fn seal_rejects_bad_key_and_challenge_lengths() {
        let payload = CredentialPayload::new("alice", "pw").expect("payload");
        let (sid, request_id) = sample_transcript_inputs();
        assert!(matches!(
            seal_credential(&[0u8; 31], &[0u8; 32], &sid[..11], request_id, 2, &payload),
            Err(CryptoError::BadParameter(_))
        ));
        assert!(matches!(
            seal_credential(&[0u8; 32], &[0u8; 8], &sid[..11], request_id, 2, &payload),
            Err(CryptoError::BadParameter(_))
        ));
    }

    #[test]
    fn from_parts_enforces_ciphertext_bounds() {
        assert_eq!(
            SealedCredential::from_parts([0u8; 32], [0u8; 12], vec![0u8; TAG_LEN - 1]),
            Err(CryptoError::CiphertextBounds)
        );
        assert_eq!(
            SealedCredential::from_parts([0u8; 32], [0u8; 12], vec![0u8; MAX_CIPHERTEXT_LEN + 1]),
            Err(CryptoError::CiphertextBounds)
        );
        assert!(SealedCredential::from_parts([0u8; 32], [0u8; 12], vec![0u8; TAG_LEN]).is_ok());
    }

    #[test]
    fn sealed_debug_reveals_no_bytes() {
        let recipient = EphemeralRecipient::generate().expect("recipient");
        let (sid, request_id) = sample_transcript_inputs();
        let payload = CredentialPayload::new("alice", "hunter2").expect("payload");
        let sealed = seal_credential(
            recipient.public_key(),
            recipient.challenge(),
            &sid[..11],
            request_id,
            2,
            &payload,
        )
        .expect("seal");
        let rendered = format!("{sealed:?}");
        assert!(rendered.contains("ciphertext_len"));
        assert!(!rendered.contains("hunter2"));
    }
}
