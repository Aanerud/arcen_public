//! Challenge/response authentication primitives.
//!
//! Byte-identical to the legacy Python `common/messages.py`:
//! - [`hash_password`] mirrors `hash_password(password, challenge)` =
//!   `sha256(f"{password}:{challenge}").hexdigest()`.
//! - [`generate_challenge`] mirrors `generate_challenge()` = `secrets.token_hex(32)`
//!   (64 lowercase hex chars from 32 cryptographically-random bytes).
//!
//! The client only needs [`hash_password`] (it answers the host's challenge);
//! the hosts issue challenges via [`generate_challenge`]. Both live here so this
//! crate is the complete auth source of truth for every peer.

use sha2::{Digest, Sha256};

/// Hash a password with the server-issued challenge (challenge-response auth).
///
/// Equivalent to Python `hashlib.sha256(f"{password}:{challenge}".encode()).hexdigest()`.
/// Returns 64 lowercase hex characters.
pub fn hash_password(password: &str, challenge: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(format!("{password}:{challenge}").as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Generate a random authentication challenge: 64 lowercase hex characters from
/// 32 cryptographically-random bytes. Mirrors Python `secrets.token_hex(32)`.
///
/// Host-side: the host issues this in the `auth_request`. Uses `getrandom`
/// (OS CSPRNG) so the crate stays free of a heavy `rand` dependency.
pub fn generate_challenge() -> String {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).expect("OS CSPRNG (getrandom) must be available");
    let mut out = String::with_capacity(64);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_password_matches_python_contract() {
        // Golden value locked against Python
        // sha256("secret:challenge").hexdigest().
        assert_eq!(
            hash_password("secret", "challenge"),
            "ada2c96fa6369f7e33b8f4ec728133c80468ab52d21827349fed8bc89a15ff55"
        );
    }

    #[test]
    fn generate_challenge_is_64_hex_chars() {
        let c = generate_challenge();
        assert_eq!(c.len(), 64);
        assert!(c
            .chars()
            .all(|ch| ch.is_ascii_hexdigit() && !ch.is_ascii_uppercase()));
    }

    #[test]
    fn generate_challenge_is_not_constant() {
        assert_ne!(generate_challenge(), generate_challenge());
    }
}
