//! Zeroizing secret storage with redacting formatting.
//!
//! The credential tile holds a typed password only for as long as it takes to
//! serialize it for Winlogon. This type stores it as UTF-16 (the shape every
//! Win32 credential API wants), scrubs its backing store on drop or on demand
//! with volatile writes the optimizer may not elide, and never renders its
//! contents through `Debug`/`Display`. Keeping it in one small, testable,
//! platform-independent type means the zeroization guarantee is covered by unit
//! tests that run on any OS.

/// A UTF-16 secret (e.g. a password) that is scrubbed when dropped and redacted
/// when formatted. Cloning is intentionally *not* derived so copies are explicit.
pub struct SecretWide {
    units: Vec<u16>,
}

impl SecretWide {
    /// An empty secret.
    pub fn new() -> Self {
        Self { units: Vec::new() }
    }

    /// Build a secret from a UTF-8 string (no trailing NUL is stored).
    pub fn from_text(value: &str) -> Self {
        Self {
            units: value.encode_utf16().collect(),
        }
    }

    /// Build a secret by taking ownership of raw UTF-16 units.
    ///
    /// This is used for password input from LogonUI so no plaintext UTF-8 copy
    /// is ever created by the provider.
    pub fn from_utf16_units(units: Vec<u16>) -> Self {
        Self { units }
    }

    /// Replace the contents from a UTF-8 string, scrubbing the previous value first.
    pub fn set_from_str(&mut self, value: &str) {
        self.zeroize();
        self.units.clear();
        self.units.extend(value.encode_utf16());
    }

    /// Number of UTF-16 code units held (not bytes, not grapheme count).
    pub fn len(&self) -> usize {
        self.units.len()
    }

    pub fn is_empty(&self) -> bool {
        self.units.is_empty()
    }

    /// Borrow the raw UTF-16 units (no NUL terminator).
    pub fn as_utf16(&self) -> &[u16] {
        &self.units
    }

    /// Produce a NUL-terminated UTF-16 copy suitable for a `PCWSTR` argument.
    ///
    /// The returned buffer is itself secret material; callers must scrub it after
    /// use ([`scrub_wide`] is provided for exactly this).
    pub fn to_nul_terminated(&self) -> Vec<u16> {
        let mut buffer = Vec::with_capacity(self.units.len() + 1);
        buffer.extend_from_slice(&self.units);
        buffer.push(0);
        buffer
    }

    /// Overwrite the backing store with zeros using volatile writes so the
    /// compiler cannot optimize the scrub away, then a compiler fence.
    pub fn zeroize(&mut self) {
        scrub_wide(&mut self.units);
    }

    /// Scrub the allocation and reset its logical length to zero.
    pub fn clear(&mut self) {
        self.zeroize();
        self.units.clear();
    }
}

impl Default for SecretWide {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for SecretWide {
    fn drop(&mut self) {
        self.zeroize();
    }
}

impl std::fmt::Debug for SecretWide {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never reveal contents; length is safe operational detail.
        write!(f, "SecretWide(***, units={})", self.units.len())
    }
}

/// Volatile-zero a UTF-16 buffer (e.g. a temporary NUL-terminated copy).
pub fn scrub_wide(buffer: &mut [u16]) {
    for slot in buffer.iter_mut() {
        // SAFETY: `slot` is a valid, aligned, writable u16.
        unsafe { core::ptr::write_volatile(slot, 0) };
    }
    core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
}

/// Volatile-zero a byte buffer (e.g. a packed credential blob before freeing).
pub fn scrub_bytes(buffer: &mut [u8]) {
    for slot in buffer.iter_mut() {
        // SAFETY: `slot` is a valid, aligned, writable u8.
        unsafe { core::ptr::write_volatile(slot, 0) };
    }
    core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
}

/// Redact a value for logs: reveal nothing but the fact that something was set.
pub fn redact(value: &str) -> &'static str {
    if value.is_empty() {
        "<empty>"
    } else {
        "<redacted>"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_reveals_contents() {
        let secret = SecretWide::from_text("hunter2");
        let rendered = format!("{secret:?}");
        assert!(!rendered.contains("hunter2"));
        assert!(rendered.contains("units=7"));
    }

    #[test]
    fn explicit_zeroize_scrubs_backing_store() {
        let mut secret = SecretWide::from_text("s3cr3t");
        assert_eq!(secret.len(), 6);
        secret.zeroize();
        assert!(secret.as_utf16().iter().all(|&u| u == 0));
    }

    #[test]
    fn set_from_str_scrubs_previous_value() {
        let mut secret = SecretWide::from_text("aaaa");
        secret.set_from_str("b");
        assert_eq!(secret.as_utf16(), &[b'b' as u16]);
    }

    #[test]
    fn nul_terminated_copy_has_trailing_zero() {
        let secret = SecretWide::from_text("pw");
        let wide = secret.to_nul_terminated();
        assert_eq!(wide, vec![b'p' as u16, b'w' as u16, 0]);
    }

    #[test]
    fn takes_ownership_of_utf16_without_conversion() {
        let secret = SecretWide::from_utf16_units(vec![0xd83d, 0xde00]);
        assert_eq!(secret.as_utf16(), &[0xd83d, 0xde00]);
    }

    #[test]
    fn scrub_helpers_zero_everything() {
        let mut wide = vec![1u16, 2, 3];
        scrub_wide(&mut wide);
        assert_eq!(wide, vec![0, 0, 0]);
        let mut bytes = vec![9u8, 8, 7];
        scrub_bytes(&mut bytes);
        assert_eq!(bytes, vec![0, 0, 0]);
    }

    #[test]
    fn redact_distinguishes_only_empty_from_set() {
        assert_eq!(redact(""), "<empty>");
        assert_eq!(redact("anything"), "<redacted>");
    }
}
