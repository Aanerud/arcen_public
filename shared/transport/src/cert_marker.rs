//! OS-free host certificate ownership markers.
//!
//! A marker records that this host issued the certificate sitting next to it.
//! Without one, material is treated as something an administrator installed
//! deliberately and is never replaced silently.
//!
//! The marker holds two pins: the certificate digest and the digest of its
//! subject public key info. Both are compared before the material is accepted
//! as ours, so replacing the certificate while leaving the marker behind does
//! not pass.
//!
//! Comparison is on *normalised* values rather than raw tool output. The
//! existing shell helper stores whatever `openssl x509 -fingerprint -sha256`
//! printed, and that text is not stable: OpenSSL 3.6 prints
//! `sha256 Fingerprint=AB:CD:...` while older OpenSSL and `LibreSSL` print
//! `SHA256 Fingerprint=...`. Comparing those strings literally means a routine
//! OpenSSL upgrade makes a host reject its own certificate and refuse to renew
//! it. Normalising first removes that failure mode while still reading every
//! marker already written.

use std::fmt::{Display, Formatter};

/// The only marker version this code understands.
pub const MARKER_VERSION: u32 = 3;

/// Why a marker could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkerError {
    /// The marker did not declare a version.
    MissingVersion,
    /// The marker declared a version this code does not understand.
    ///
    /// Refused rather than guessed at: a newer marker may carry a pin this
    /// version would not check.
    UnsupportedVersion,
    /// The certificate pin was absent.
    MissingCertificatePin,
    /// The subject public key pin was absent.
    MissingSpkiPin,
    /// A pin was not in any recognised form.
    MalformedPin,
    /// The marker held a line this version cannot interpret.
    UnrecognizedEntry,
}

impl MarkerError {
    /// Returns a stable operator-facing code.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MissingVersion => "missing_marker_version",
            Self::UnsupportedVersion => "unsupported_marker_version",
            Self::MissingCertificatePin => "missing_certificate_pin",
            Self::MissingSpkiPin => "missing_spki_pin",
            Self::MalformedPin => "malformed_pin",
            Self::UnrecognizedEntry => "unrecognized_marker_entry",
        }
    }
}

impl Display for MarkerError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl std::error::Error for MarkerError {}

/// Normalises a certificate fingerprint into lowercase hex.
///
/// Accepts the forms tools actually produce: an optional
/// `<algorithm> Fingerprint=` prefix in any case, colon or space separators,
/// and either hex case. Returns `None` for anything that is not a hex digest.
#[must_use]
pub fn normalize_certificate_pin(value: &str) -> Option<String> {
    let body = value
        .rsplit_once('=')
        .map_or(value, |(_, remainder)| remainder);
    let cleaned: String = body
        .chars()
        .filter(|character| !matches!(character, ':' | ' ' | '\t' | '\r' | '\n'))
        .collect();
    if cleaned.is_empty() || !cleaned.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(cleaned.to_ascii_lowercase())
}

/// Normalises a subject public key pin.
///
/// The established form is `sha256/<base64>`. The algorithm prefix is kept,
/// because unlike the fingerprint text it is part of the value's meaning, but
/// surrounding whitespace is not significant.
#[must_use]
pub fn normalize_spki_pin(value: &str) -> Option<String> {
    let trimmed = value.trim();
    let (algorithm, digest) = trimmed.split_once('/')?;
    if !algorithm.eq_ignore_ascii_case("sha256") || digest.is_empty() {
        return None;
    }
    // Base64 is case sensitive, so only the algorithm name is folded.
    Some(format!("sha256/{digest}"))
}

/// A certificate's two pins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertificatePins {
    /// Whole-certificate SHA-256 over the DER, as lowercase hex.
    pub certificate: String,
    /// Subject public key pin as `sha256/<base64>`.
    pub spki: String,
}

/// Computes both pins from a PEM certificate.
///
/// This is shared because every host needs the same answer. The digests are
/// taken over the DER, which is what `openssl x509 -fingerprint -sha256` and
/// every pinning client use; hashing the PEM text instead would produce a
/// value that never matches anything an operator or client compares against.
///
/// Returns `None` when the input is not a parseable PEM certificate.
#[must_use]
pub fn pins_from_pem(pem_bytes: &[u8]) -> Option<CertificatePins> {
    use sha2::Digest as _;

    let text = std::str::from_utf8(pem_bytes).ok()?;
    let der = pem_to_der(text)?;
    let (_, parsed) = x509_parser::parse_x509_certificate(&der).ok()?;
    let spki_der = parsed.tbs_certificate.subject_pki.raw;
    Some(CertificatePins {
        certificate: hex_lower(&sha2::Sha256::digest(&der)),
        spki: format!("sha256/{}", base64_encode(&sha2::Sha256::digest(spki_der))),
    })
}

/// Extracts the first certificate body from a PEM document.
#[must_use]
pub fn pem_to_der(text: &str) -> Option<Vec<u8>> {
    const BEGIN: &str = "-----BEGIN CERTIFICATE-----";
    const END: &str = "-----END CERTIFICATE-----";
    let start = text.find(BEGIN)?;
    let rest = &text[start + BEGIN.len()..];
    let end = rest.find(END)?;
    let body: String = rest[..end]
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect();
    base64_decode(&body)
}

/// Formats bytes as lowercase hex.
#[must_use]
pub fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(text, "{byte:02x}");
    }
    text
}

/// Formats a lowercase hex digest the way `openssl` prints a fingerprint:
/// uppercase, colon-separated pairs.
#[must_use]
pub fn colon_hex(hex: &str) -> String {
    let upper = hex.to_ascii_uppercase();
    let mut out = String::with_capacity(upper.len() * 3 / 2);
    for (index, chunk) in upper.as_bytes().chunks(2).enumerate() {
        if index > 0 {
            out.push(':');
        }
        out.push_str(std::str::from_utf8(chunk).unwrap_or_default());
    }
    out
}

/// Standard-alphabet base64 encoder.
#[must_use]
pub fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = u32::from(chunk[0]);
        let second = chunk.get(1).copied().map_or(0, u32::from);
        let third = chunk.get(2).copied().map_or(0, u32::from);
        let triple = (first << 16) | (second << 8) | third;
        out.push(ALPHABET[((triple >> 18) & 0x3F) as usize] as char);
        out.push(ALPHABET[((triple >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[((triple >> 6) & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(triple & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

/// Standard-alphabet base64 decoder for PEM bodies.
#[must_use]
pub fn base64_decode(input: &str) -> Option<Vec<u8>> {
    const fn value(byte: u8) -> Option<u32> {
        match byte {
            b'A'..=b'Z' => Some((byte - b'A') as u32),
            b'a'..=b'z' => Some((byte - b'a') as u32 + 26),
            b'0'..=b'9' => Some((byte - b'0') as u32 + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    let mut chunk = 0_u32;
    let mut bits = 0_u32;
    for &byte in input.as_bytes() {
        if byte == b'=' {
            break;
        }
        let value = value(byte)?;
        chunk = (chunk << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(u8::try_from((chunk >> bits) & 0xFF).ok()?);
        }
    }
    Some(out)
}

/// A certificate's validity window, in Unix seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ValidityWindow {
    /// Not valid before this instant.
    pub not_before: i64,
    /// Not valid after this instant.
    pub not_after: i64,
}

impl ValidityWindow {
    /// Returns whether the certificate is usable at `now`.
    #[must_use]
    pub const fn is_current(self, now: i64) -> bool {
        now >= self.not_before && now < self.not_after
    }

    /// Returns whether renewal is due within `window_seconds` of expiry.
    #[must_use]
    pub const fn is_due_for_renewal(self, now: i64, window_seconds: i64) -> bool {
        self.not_after - now <= window_seconds
    }
}

/// Returns whether the certificate names itself as its issuer.
///
/// Every certificate an Arcen installer or helper generates is self-signed.
/// An enterprise CA-issued certificate is not, which is what lets a host tell
/// material an earlier Arcen install left behind from material an operator
/// provisioned on purpose. `false` when the PEM does not parse.
#[must_use]
pub fn is_self_signed_pem(pem_bytes: &[u8]) -> bool {
    let Some(der) = std::str::from_utf8(pem_bytes).ok().and_then(pem_to_der) else {
        return false;
    };
    x509_parser::parse_x509_certificate(&der).is_ok_and(|(_, certificate)| {
        certificate.issuer().as_raw() == certificate.subject().as_raw()
    })
}

/// Reads a PEM certificate's validity window.
///
/// Returns `None` when the certificate cannot be parsed. Callers treat that as
/// invalid rather than assuming it is good, which is what makes provisioning
/// refuse to keep serving material it cannot inspect.
#[must_use]
pub fn validity_from_pem(pem_bytes: &[u8]) -> Option<ValidityWindow> {
    let text = std::str::from_utf8(pem_bytes).ok()?;
    let der = pem_to_der(text)?;
    let (_, certificate) = x509_parser::parse_x509_certificate(&der).ok()?;
    Some(ValidityWindow {
        not_before: certificate.validity().not_before.timestamp(),
        not_after: certificate.validity().not_after.timestamp(),
    })
}

/// A parsed ownership marker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnershipMarker {
    certificate_pin: String,
    spki_pin: String,
}

impl OwnershipMarker {
    /// Creates a marker from already-normalised pins.
    ///
    /// # Errors
    ///
    /// Returns [`MarkerError::MalformedPin`] when either pin is not in a
    /// recognised form.
    pub fn new(certificate_pin: &str, spki_pin: &str) -> Result<Self, MarkerError> {
        Ok(Self {
            certificate_pin: normalize_certificate_pin(certificate_pin)
                .ok_or(MarkerError::MalformedPin)?,
            spki_pin: normalize_spki_pin(spki_pin).ok_or(MarkerError::MalformedPin)?,
        })
    }

    /// Returns the normalised certificate pin as lowercase hex.
    #[must_use]
    pub fn certificate_pin(&self) -> &str {
        &self.certificate_pin
    }

    /// Returns the normalised subject public key pin.
    #[must_use]
    pub fn spki_pin(&self) -> &str {
        &self.spki_pin
    }

    /// Parses a marker file.
    ///
    /// # Errors
    ///
    /// Returns a [`MarkerError`] for a marker that is incomplete, malformed, or
    /// from a version this code does not understand.
    pub fn parse(text: &str) -> Result<Self, MarkerError> {
        let mut version: Option<u32> = None;
        let mut certificate: Option<String> = None;
        let mut spki: Option<String> = None;

        for line in text.lines() {
            let line = line.trim_end_matches(['\r', '\n']);
            if line.trim().is_empty() {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                return Err(MarkerError::UnrecognizedEntry);
            };
            match key {
                "version" => {
                    version = Some(
                        value
                            .trim()
                            .parse()
                            .map_err(|_| MarkerError::UnrecognizedEntry)?,
                    );
                }
                // The fingerprint text itself contains `=`, so the remainder of
                // the line is the value.
                "certificate" => certificate = Some(value.to_owned()),
                "spki" => spki = Some(value.to_owned()),
                _ => return Err(MarkerError::UnrecognizedEntry),
            }
        }

        match version {
            None => return Err(MarkerError::MissingVersion),
            Some(found) if found != MARKER_VERSION => return Err(MarkerError::UnsupportedVersion),
            Some(_) => {}
        }
        let certificate = certificate.ok_or(MarkerError::MissingCertificatePin)?;
        let spki = spki.ok_or(MarkerError::MissingSpkiPin)?;
        Self::new(&certificate, &spki)
    }

    /// Renders the marker in the on-disk form.
    #[must_use]
    pub fn render(&self) -> String {
        format!(
            "version={MARKER_VERSION}\ncertificate={}\nspki={}\n",
            self.certificate_pin, self.spki_pin
        )
    }

    /// Returns whether this marker describes the given material.
    ///
    /// Both pins must match. A certificate swapped underneath a marker that was
    /// left in place fails here, which is what keeps ownership honest.
    #[must_use]
    pub fn matches(&self, certificate_pin: &str, spki_pin: &str) -> bool {
        normalize_certificate_pin(certificate_pin).is_some_and(|pin| pin == self.certificate_pin)
            && normalize_spki_pin(spki_pin).is_some_and(|pin| pin == self.spki_pin)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_generated_certificate_is_self_signed_and_a_ca_issued_one_is_not() {
        let ca_key = rcgen::KeyPair::generate().expect("ca key");
        let mut ca_params = rcgen::CertificateParams::new(Vec::<String>::new()).expect("ca");
        ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        ca_params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "Example Enterprise CA");
        let ca = ca_params.self_signed(&ca_key).expect("ca cert");
        assert!(is_self_signed_pem(ca.pem().as_bytes()));

        let leaf_key = rcgen::KeyPair::generate().expect("leaf key");
        let mut leaf_params =
            rcgen::CertificateParams::new(vec!["pier.example".to_owned()]).expect("leaf");
        leaf_params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "pier.example");
        let issuer = rcgen::Issuer::new(ca_params, ca_key);
        let leaf = leaf_params
            .signed_by(&leaf_key, &issuer)
            .expect("leaf cert");
        assert!(!is_self_signed_pem(leaf.pem().as_bytes()));
        assert!(!is_self_signed_pem(b"not a certificate"));
    }

    const HEX: &str = "2bfdf2fb2c67e38c2569c09b243fed94be54eefe2aba3c1815b24cd6e6cf86d4";
    const COLONS: &str = "2B:FD:F2:FB:2C:67:E3:8C:25:69:C0:9B:24:3F:ED:94:BE:54:EE:FE:2A:BA:3C:18:15:B2:4C:D6:E6:CF:86:D4";
    const SPKI: &str = "sha256/rSMZvYwKwbQ1uMh6dx6tqiXhonk6oADZ9CIY2I2dvrA=";

    #[test]
    fn openssl_version_differences_normalise_to_the_same_pin() {
        // Measured on OpenSSL 3.6.3, which prints a lowercase algorithm name.
        // Older OpenSSL and LibreSSL print `SHA256`. Comparing the raw strings
        // would make an OpenSSL upgrade look like tampered material.
        let modern = format!("sha256 Fingerprint={COLONS}");
        let legacy = format!("SHA256 Fingerprint={COLONS}");
        let bare = HEX.to_owned();
        let normalized: Vec<_> = [modern.as_str(), legacy.as_str(), bare.as_str()]
            .iter()
            .map(|value| normalize_certificate_pin(value).expect("recognised"))
            .collect();
        assert_eq!(normalized[0], HEX);
        assert_eq!(normalized[1], HEX);
        assert_eq!(normalized[2], HEX);
    }

    #[test]
    fn a_marker_in_the_helper_format_parses_here() {
        // The exact shape `packaging/linux/new-host-cert.sh` writes.
        let on_disk = format!("version=3\ncertificate=sha256 Fingerprint={COLONS}\nspki={SPKI}\n");
        let marker = OwnershipMarker::parse(&on_disk).expect("helper marker parses");
        assert_eq!(marker.certificate_pin(), HEX);
        assert_eq!(marker.spki_pin(), SPKI);
    }

    #[test]
    fn a_rendered_marker_round_trips() {
        let marker = OwnershipMarker::new(COLONS, SPKI).expect("valid pins");
        let parsed = OwnershipMarker::parse(&marker.render()).expect("round trip");
        assert_eq!(parsed, marker);
    }

    #[test]
    fn matching_requires_both_pins() {
        let marker = OwnershipMarker::new(COLONS, SPKI).expect("valid pins");
        assert!(marker.matches(HEX, SPKI));
        // A certificate swapped underneath a marker left in place must fail.
        let other = "00".repeat(32);
        assert!(!marker.matches(&other, SPKI));
        assert!(!marker.matches(HEX, "sha256/AAAA"));
    }

    #[test]
    fn matching_survives_a_different_openssl_spelling() {
        let marker = OwnershipMarker::new(COLONS, SPKI).expect("valid pins");
        assert!(marker.matches(&format!("SHA256 Fingerprint={COLONS}"), SPKI));
        assert!(marker.matches(&format!("sha256 Fingerprint={COLONS}"), SPKI));
    }

    #[test]
    fn incomplete_markers_are_refused() {
        assert_eq!(
            OwnershipMarker::parse(&format!("certificate={HEX}\nspki={SPKI}\n")),
            Err(MarkerError::MissingVersion)
        );
        assert_eq!(
            OwnershipMarker::parse(&format!("version=3\nspki={SPKI}\n")),
            Err(MarkerError::MissingCertificatePin)
        );
        assert_eq!(
            OwnershipMarker::parse(&format!("version=3\ncertificate={HEX}\n")),
            Err(MarkerError::MissingSpkiPin)
        );
    }

    #[test]
    fn a_marker_from_a_newer_version_is_refused_rather_than_guessed_at() {
        // It may carry a pin this version would not check.
        assert_eq!(
            OwnershipMarker::parse(&format!("version=4\ncertificate={HEX}\nspki={SPKI}\n")),
            Err(MarkerError::UnsupportedVersion)
        );
        assert_eq!(
            OwnershipMarker::parse(&format!(
                "version=3\ncertificate={HEX}\nspki={SPKI}\nextra=1\n"
            )),
            Err(MarkerError::UnrecognizedEntry)
        );
    }

    #[test]
    fn malformed_pins_are_refused() {
        assert_eq!(normalize_certificate_pin(""), None);
        assert_eq!(normalize_certificate_pin("not-hex-at-all"), None);
        assert_eq!(normalize_spki_pin("rSMZ"), None);
        assert_eq!(normalize_spki_pin("md5/abcd"), None);
        assert_eq!(normalize_spki_pin("sha256/"), None);
        assert_eq!(
            OwnershipMarker::parse("version=3\ncertificate=zz\nspki=sha256/AAAA\n"),
            Err(MarkerError::MalformedPin)
        );
    }

    #[test]
    fn base64_round_trips_including_padding_cases() {
        for (bytes, encoded) in [
            (&b"Man"[..], "TWFu"),
            (&b"Ma"[..], "TWE="),
            (&b"M"[..], "TQ=="),
            (&b""[..], ""),
        ] {
            assert_eq!(base64_encode(bytes), encoded);
            assert_eq!(base64_decode(encoded).expect("decodes"), bytes);
        }
    }

    #[test]
    fn hex_helpers_agree_with_the_openssl_shape() {
        assert_eq!(hex_lower(&[0x0a, 0x1b, 0x2c]), "0a1b2c");
        assert_eq!(colon_hex("0a1b2c"), "0A:1B:2C");
        assert_eq!(colon_hex(""), "");
    }

    #[test]
    fn a_validity_window_answers_current_and_due_for_renewal() {
        let window = ValidityWindow {
            not_before: 1_000,
            not_after: 2_000,
        };
        assert!(!window.is_current(999), "before the window");
        assert!(window.is_current(1_000), "at the lower bound");
        assert!(window.is_current(1_999));
        assert!(!window.is_current(2_000), "expiry is exclusive");

        assert!(!window.is_due_for_renewal(1_000, 100));
        assert!(window.is_due_for_renewal(1_900, 100), "at the threshold");
        assert!(window.is_due_for_renewal(2_500, 100), "already expired");
    }

    #[test]
    fn a_non_certificate_yields_no_validity() {
        assert!(validity_from_pem(b"not a certificate").is_none());
    }

    #[test]
    fn a_non_certificate_yields_no_pins() {
        assert!(pins_from_pem(b"not a certificate").is_none());
        assert!(pem_to_der("-----BEGIN CERTIFICATE-----\nnope").is_none());
    }

    #[test]
    fn base64_case_is_preserved_because_it_is_significant() {
        let upper = "sha256/ABCD";
        let lower = "sha256/abcd";
        assert_ne!(
            normalize_spki_pin(upper).expect("valid"),
            normalize_spki_pin(lower).expect("valid")
        );
    }
}
