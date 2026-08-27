//! The plaintext that the broker seals for the Credential Provider.
//!
//! This is the *only* place a remote account secret exists in cleartext inside
//! this crate, and it exists only transiently: between decoding a decrypted
//! envelope and handing the fields to the platform's autologon path. The type
//! therefore
//!
//! * carries a bounded username and a bounded password and nothing else;
//! * derives no `serde` impl, so it can never be JSON-encoded into a log or a
//!   wire frame by accident — the only serialization is [`CredentialPayload::encode`]
//!   / [`CredentialPayload::decode`], which produce raw bytes destined straight
//!   for the AEAD;
//! * redacts its `Debug` output; and
//! * zeroizes its password on drop and on every explicit scrub.
//!
//! None of this is authentication. Producing or accepting a `CredentialPayload`
//! implies nothing about *who* sent it; that guarantee comes entirely from the
//! sealed envelope's key agreement and the transport's peer checks.

use zeroize::{Zeroize, Zeroizing};

/// Maximum UTF-8 bytes of the account name carried in a sealed payload. Matches
/// the provider's 256-UTF-16-unit username cap with headroom for multi-byte
/// code points, and stays far under [`crate::MAX_FRAME_LEN`].
pub const MAX_PAYLOAD_USERNAME_BYTES: usize = 1024;

/// Maximum UTF-8 bytes of the password carried in a sealed payload. Comfortably
/// covers the provider's 512-UTF-16-unit password cap.
pub const MAX_PAYLOAD_PASSWORD_BYTES: usize = 2048;

/// One-byte version tag prefixing the encoded plaintext so a future field
/// change is detected rather than silently misparsed.
const PLAINTEXT_VERSION: u8 = 1;

/// Why a credential payload could not be built or decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayloadError {
    /// The username was empty.
    EmptyUsername,
    /// The password was empty.
    EmptyPassword,
    /// The username exceeded [`MAX_PAYLOAD_USERNAME_BYTES`].
    UsernameTooLong,
    /// The password exceeded [`MAX_PAYLOAD_PASSWORD_BYTES`].
    PasswordTooLong,
    /// A field contained an embedded NUL.
    ContainsNul,
    /// The decoded plaintext was structurally invalid (bad version/lengths).
    Malformed,
    /// A decoded field was not valid UTF-8.
    InvalidUtf8,
}

impl std::fmt::Display for PayloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            Self::EmptyUsername => "credential username is empty",
            Self::EmptyPassword => "credential password is empty",
            Self::UsernameTooLong => "credential username exceeds the payload bound",
            Self::PasswordTooLong => "credential password exceeds the payload bound",
            Self::ContainsNul => "credential field contains a NUL",
            Self::Malformed => "credential plaintext is malformed",
            Self::InvalidUtf8 => "credential field is not valid UTF-8",
        };
        f.write_str(text)
    }
}

impl std::error::Error for PayloadError {}

/// A bounded, zeroizing username/password pair sealed by the broker and opened
/// by the credential provider.
pub struct CredentialPayload {
    username: String,
    password: Zeroizing<String>,
}

impl CredentialPayload {
    /// Build a payload, enforcing bounds and rejecting embedded NULs.
    ///
    /// The broker normally supplies the SID-resolved Windows account name
    /// (`MACHINE\user` or `DOMAIN\user`). The payload remains generic enough for
    /// the manual/provider tests to cover other Windows-supported forms.
    pub fn new(username: &str, password: &str) -> Result<Self, PayloadError> {
        if username.is_empty() {
            return Err(PayloadError::EmptyUsername);
        }
        if password.is_empty() {
            return Err(PayloadError::EmptyPassword);
        }
        if username.len() > MAX_PAYLOAD_USERNAME_BYTES {
            return Err(PayloadError::UsernameTooLong);
        }
        if password.len() > MAX_PAYLOAD_PASSWORD_BYTES {
            return Err(PayloadError::PasswordTooLong);
        }
        if username.contains('\0') || password.contains('\0') {
            return Err(PayloadError::ContainsNul);
        }
        Ok(Self {
            username: username.to_string(),
            password: Zeroizing::new(password.to_string()),
        })
    }

    /// The account name (identity, not a secret).
    pub fn username(&self) -> &str {
        &self.username
    }

    /// The password. Callers must not persist or log the returned string.
    pub fn password(&self) -> &str {
        &self.password
    }

    /// Serialize to the raw bytes that go straight into the AEAD.
    ///
    /// Layout: `version(1) | user_len(u16 BE) | user | pass_len(u16 BE) | pass`.
    /// The returned buffer is itself secret material and is wrapped in
    /// [`Zeroizing`] so it is scrubbed when the caller drops it.
    pub fn encode(&self) -> Zeroizing<Vec<u8>> {
        let user = self.username.as_bytes();
        let pass = self.password.as_bytes();
        let mut out = Vec::with_capacity(1 + 2 + user.len() + 2 + pass.len());
        out.push(PLAINTEXT_VERSION);
        out.extend_from_slice(&(user.len() as u16).to_be_bytes());
        out.extend_from_slice(user);
        out.extend_from_slice(&(pass.len() as u16).to_be_bytes());
        out.extend_from_slice(pass);
        Zeroizing::new(out)
    }

    /// Parse the raw plaintext produced by [`Self::encode`], enforcing the same
    /// bounds. The input is treated as secret: callers pass a buffer they will
    /// scrub, and this function keeps no extra copy beyond the returned payload.
    pub fn decode(bytes: &[u8]) -> Result<Self, PayloadError> {
        let mut cursor = bytes;
        let version = take(&mut cursor, 1).ok_or(PayloadError::Malformed)?[0];
        if version != PLAINTEXT_VERSION {
            return Err(PayloadError::Malformed);
        }
        let user_len = take_u16(&mut cursor)? as usize;
        if user_len > MAX_PAYLOAD_USERNAME_BYTES {
            return Err(PayloadError::UsernameTooLong);
        }
        let user_bytes = take(&mut cursor, user_len).ok_or(PayloadError::Malformed)?;
        let pass_len = take_u16(&mut cursor)? as usize;
        if pass_len > MAX_PAYLOAD_PASSWORD_BYTES {
            return Err(PayloadError::PasswordTooLong);
        }
        let pass_bytes = take(&mut cursor, pass_len).ok_or(PayloadError::Malformed)?;
        if !cursor.is_empty() {
            return Err(PayloadError::Malformed);
        }

        // Build the password inside a scrubbing owner first so an invalid-UTF-8
        // or bounds failure still zeroizes any bytes we copied.
        let mut password = Zeroizing::new(
            std::str::from_utf8(pass_bytes)
                .map_err(|_| PayloadError::InvalidUtf8)?
                .to_string(),
        );
        let username = std::str::from_utf8(user_bytes)
            .map_err(|_| PayloadError::InvalidUtf8)?
            .to_string();
        let result = Self::new(&username, &password);
        password.zeroize();
        result
    }
}

impl Drop for CredentialPayload {
    fn drop(&mut self) {
        // `password` is a Zeroizing<String> and scrubs itself; the username is
        // identity, but scrub it too so no trace of the account survives.
        self.username.zeroize();
    }
}

impl std::fmt::Debug for CredentialPayload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialPayload")
            .field("username", &"<redacted>")
            .field("password", &"<redacted>")
            .finish()
    }
}

// Equality is provided only for tests. Production code must never branch on a
// non-constant-time secret comparison, so this impl is deliberately test-gated.
#[cfg(test)]
impl PartialEq for CredentialPayload {
    fn eq(&self, other: &Self) -> bool {
        self.username == other.username && *self.password == *other.password
    }
}

fn take<'a>(cursor: &mut &'a [u8], len: usize) -> Option<&'a [u8]> {
    if cursor.len() < len {
        return None;
    }
    let (head, tail) = cursor.split_at(len);
    *cursor = tail;
    Some(head)
}

fn take_u16(cursor: &mut &[u8]) -> Result<u16, PayloadError> {
    let bytes = take(cursor, 2).ok_or(PayloadError::Malformed)?;
    Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_local_domain_and_upn_forms() {
        for name in [r"WORKGROUP\artist", "artist@studio.example", "artist"] {
            let payload = CredentialPayload::new(name, "hunter2").expect("payload");
            let encoded = payload.encode();
            let decoded = CredentialPayload::decode(&encoded).expect("decode");
            assert_eq!(decoded.username(), name);
            assert_eq!(decoded.password(), "hunter2");
        }
    }

    #[test]
    fn debug_never_reveals_secret_or_account() {
        let payload = CredentialPayload::new(r"CORP\alice", "s3cr3t").expect("payload");
        let rendered = format!("{payload:?}");
        assert!(!rendered.contains("s3cr3t"));
        assert!(!rendered.contains("alice"));
        assert!(rendered.contains("<redacted>"));
    }

    #[test]
    fn bounds_and_empties_are_rejected() {
        assert_eq!(
            CredentialPayload::new("", "pw"),
            Err(PayloadError::EmptyUsername)
        );
        assert_eq!(
            CredentialPayload::new("user", ""),
            Err(PayloadError::EmptyPassword)
        );
        let long_user = "u".repeat(MAX_PAYLOAD_USERNAME_BYTES + 1);
        assert_eq!(
            CredentialPayload::new(&long_user, "pw"),
            Err(PayloadError::UsernameTooLong)
        );
        let long_pass = "p".repeat(MAX_PAYLOAD_PASSWORD_BYTES + 1);
        assert_eq!(
            CredentialPayload::new("user", &long_pass),
            Err(PayloadError::PasswordTooLong)
        );
        assert_eq!(
            CredentialPayload::new("a\0b", "pw"),
            Err(PayloadError::ContainsNul)
        );
    }

    #[test]
    fn decode_rejects_trailing_bytes_and_bad_version() {
        let payload = CredentialPayload::new("user", "pw").expect("payload");
        let mut encoded = payload.encode().to_vec();
        encoded.push(0xff);
        assert_eq!(
            CredentialPayload::decode(&encoded),
            Err(PayloadError::Malformed)
        );

        let mut bad_version = payload.encode().to_vec();
        bad_version[0] = 9;
        assert_eq!(
            CredentialPayload::decode(&bad_version),
            Err(PayloadError::Malformed)
        );
    }

    #[test]
    fn decode_rejects_truncated_input() {
        let payload = CredentialPayload::new("user", "password").expect("payload");
        let encoded = payload.encode();
        for cut in 0..encoded.len() {
            assert!(CredentialPayload::decode(&encoded[..cut]).is_err());
        }
        assert!(CredentialPayload::decode(&encoded).is_ok());
    }

    #[test]
    fn decode_rejects_length_that_overruns_buffer() {
        // version + user_len says 8 but no bytes follow.
        let framed = [PLAINTEXT_VERSION, 0x00, 0x08];
        assert_eq!(
            CredentialPayload::decode(&framed),
            Err(PayloadError::Malformed)
        );
    }
}
