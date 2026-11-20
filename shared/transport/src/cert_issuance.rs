// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The one certificate Arcen issues for itself, decided once for every Pier,
//! installer and helper.
//!
//! [`crate::cert_provisioning`] decides *whether* to issue. This module
//! decides *what*: the subject, the names, the key usage and the validity
//! window ([`IssuanceProfile`]), which names a renewal keeps
//! ([`IssuanceProfile::for_action`]), which key an existing deployment holds
//! and the public-key pin it stands for ([`HostKey`]), and whether an issued
//! certificate is exactly the one asked for ([`IssuanceProfile::verify_issued`]).
//!
//! Every issuer checks its output with the same verification, so a
//! certificate that drops a name, moves the public key, widens its key usage
//! or carries the wrong window is refused before it is published, whatever
//! produced it. The `issuer` feature adds [`issue`], the shared issuer the
//! Rust Piers and installers use; the shell helper keeps `openssl` and takes
//! its arguments from [`IssuanceProfile::openssl_req_arguments`].
//!
//! Deployed keys are kept, never rotated to suit the issuer: Linux Piers hold
//! SEC1 keys from `openssl ecparam`, the macOS Pier and the Windows installer
//! PKCS #8 keys. [`HostKey`] reads both and wraps a SEC1 key as PKCS #8 for
//! signing, so the key file on disk, and the SPKI pin every paired Deck holds,
//! stay byte-for-byte what they were.

use std::fmt::{Display, Formatter};
use std::net::IpAddr;

use rustls::pki_types::PrivateKeyDer;
use sha2::Digest as _;
use zeroize::Zeroizing;

use crate::cert_marker;
use crate::cert_provisioning::ProvisioningAction;
use crate::cert_store::VALIDITY_DAYS;

/// The subject common name of every certificate Arcen issues for itself.
pub const COMMON_NAME: &str = "Arcen Pier";
/// How far before the moment of issue a certificate becomes valid, so a Deck
/// whose clock runs slightly behind the Pier's does not refuse it as not yet
/// valid.
pub const BACKDATE_SECONDS: i64 = 5 * 60;
/// Seconds in [`VALIDITY_DAYS`].
pub const VALIDITY_SECONDS: i64 = VALIDITY_DAYS * 24 * 60 * 60;
/// Slack allowed between the clock a profile was made with and the clock the
/// issuer read, and for issuers that cannot backdate (`openssl req` before
/// 3.4 has no `-not_before`).
const CLOCK_SLACK_SECONDS: i64 = 120;
/// Longest DNS name a certificate carries.
const MAX_DNS_NAME_BYTES: usize = 253;

const OID_EC_PUBLIC_KEY: &[u8] = &[0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01];
const OID_P256: &[u8] = &[0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07];
const OID_P384: &[u8] = &[0x06, 0x05, 0x2b, 0x81, 0x04, 0x00, 0x22];

/// Why a certificate could not be planned, issued or accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssuanceError {
    /// The plan keeps the existing material; there is nothing to issue.
    NothingToIssue,
    /// No subject alternative name was given or kept.
    NoSubjectAltName,
    /// A name is neither an address nor a plain DNS name.
    InvalidSubjectAltName,
    /// A renewal needs the certificate it renews, and it could not be read.
    UnreadableExistingCertificate,
    /// The key file holds no key, more than one, or one that is malformed.
    UnreadableKey,
    /// The key is not an elliptic-curve key on P-256 or P-384.
    UnsupportedKey,
    /// The key does not carry its public half, so its pin cannot be known.
    KeyWithoutPublicKey,
    /// The clock is outside the range a certificate can state.
    ClockOutOfRange,
    /// The issuer failed.
    IssuerFailed,
    /// The issued certificate could not be parsed or is not self-signed.
    NotSelfSigned,
    /// The subject is not exactly `CN=Arcen Pier`.
    WrongSubject,
    /// The certificate is a CA or lacks a critical `CA:FALSE`.
    WrongBasicConstraints,
    /// Key usage is not exactly a critical `digitalSignature`.
    WrongKeyUsage,
    /// Extended key usage is not exactly `serverAuth`.
    WrongExtendedKeyUsage,
    /// The names are not exactly the ones asked for.
    WrongSubjectAltNames,
    /// The validity window is not the profile's.
    WrongValidity,
    /// The certificate's public key is not the key it was issued over: every
    /// SPKI pin would break.
    PublicKeyChanged,
}

impl IssuanceError {
    /// Returns a stable operator-facing code.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NothingToIssue => "nothing_to_issue",
            Self::NoSubjectAltName => "no_subject_alt_name",
            Self::InvalidSubjectAltName => "invalid_subject_alt_name",
            Self::UnreadableExistingCertificate => "unreadable_existing_certificate",
            Self::UnreadableKey => "unreadable_key",
            Self::UnsupportedKey => "unsupported_key",
            Self::KeyWithoutPublicKey => "key_without_public_key",
            Self::ClockOutOfRange => "clock_out_of_range",
            Self::IssuerFailed => "issuer_failed",
            Self::NotSelfSigned => "not_self_signed",
            Self::WrongSubject => "wrong_subject",
            Self::WrongBasicConstraints => "wrong_basic_constraints",
            Self::WrongKeyUsage => "wrong_key_usage",
            Self::WrongExtendedKeyUsage => "wrong_extended_key_usage",
            Self::WrongSubjectAltNames => "wrong_subject_alt_names",
            Self::WrongValidity => "wrong_validity",
            Self::PublicKeyChanged => "public_key_changed",
        }
    }
}

impl Display for IssuanceError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl std::error::Error for IssuanceError {}

/// One DNS or IP subject alternative name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SubjectAltName {
    /// A DNS name, lower-cased, without a trailing dot.
    Dns(String),
    /// An address; a Deck dialling it validates against this entry.
    Ip(IpAddr),
}

impl SubjectAltName {
    /// Reads `DNS:name`, `IP:address`, or a bare name or address (an address
    /// becomes an IP entry, anything else a DNS entry).
    ///
    /// A DNS name is limited to letters, digits, `-`, `_`, `.` and `*`, so a
    /// name can never smuggle another entry or an extension into an
    /// `openssl -addext` argument.
    ///
    /// # Errors
    ///
    /// Returns [`IssuanceError::InvalidSubjectAltName`] for anything else.
    pub fn parse(value: &str) -> Result<Self, IssuanceError> {
        let normalized = cert_marker::normalize_subject_alt_name_entry(value)
            .ok_or(IssuanceError::InvalidSubjectAltName)?;
        if let Some(address) = normalized.strip_prefix("IP:") {
            return address
                .parse()
                .map(Self::Ip)
                .map_err(|_| IssuanceError::InvalidSubjectAltName);
        }
        let name = normalized
            .strip_prefix("DNS:")
            .ok_or(IssuanceError::InvalidSubjectAltName)?;
        let valid = !name.is_empty()
            && name.len() <= MAX_DNS_NAME_BYTES
            && name.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'*')
            });
        if valid {
            Ok(Self::Dns(name.to_owned()))
        } else {
            Err(IssuanceError::InvalidSubjectAltName)
        }
    }

    /// The `DNS:name` / `IP:address` form `openssl` and the pin tools use.
    #[must_use]
    pub fn tagged(&self) -> String {
        match self {
            Self::Dns(name) => format!("DNS:{name}"),
            Self::Ip(address) => format!("IP:{address}"),
        }
    }
}

/// Exactly what Arcen issues: `CN=Arcen Pier`, critical `CA:FALSE`, critical
/// `digitalSignature`, `serverAuth`, these names, and a window of
/// [`VALIDITY_DAYS`] that starts [`BACKDATE_SECONDS`] before issue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssuanceProfile {
    names: Vec<SubjectAltName>,
    issued_at: i64,
}

impl IssuanceProfile {
    /// A profile for `names` issued at `now_epoch_secs`.
    ///
    /// Duplicates are dropped; the first spelling of each name is kept.
    ///
    /// # Errors
    ///
    /// Returns [`IssuanceError::NoSubjectAltName`] when no name is left, a
    /// name's own refusal, or [`IssuanceError::ClockOutOfRange`].
    pub fn new(names: &[String], now_epoch_secs: u64) -> Result<Self, IssuanceError> {
        let mut parsed = Vec::with_capacity(names.len());
        for name in names {
            let name = SubjectAltName::parse(name)?;
            if !parsed.contains(&name) {
                parsed.push(name);
            }
        }
        if parsed.is_empty() {
            return Err(IssuanceError::NoSubjectAltName);
        }
        let issued_at =
            i64::try_from(now_epoch_secs).map_err(|_| IssuanceError::ClockOutOfRange)?;
        issued_at
            .checked_add(VALIDITY_SECONDS)
            .ok_or(IssuanceError::ClockOutOfRange)?;
        Ok(Self {
            names: parsed,
            issued_at,
        })
    }

    /// The profile for what [`crate::cert_provisioning::plan`] decided.
    ///
    /// A renewal or adoption reissues over the existing key and keeps every
    /// name the existing certificate carries, adding `requested` after them:
    /// a Deck that dials a name the old certificate covered keeps connecting.
    /// A new key (first issue or an explicit rekey) carries `requested` only;
    /// that is how an operator changes the names.
    ///
    /// # Errors
    ///
    /// Returns [`IssuanceError::NothingToIssue`] for a plan that keeps the
    /// material, [`IssuanceError::UnreadableExistingCertificate`] for a
    /// renewal without a readable certificate, or [`Self::new`]'s refusal.
    pub fn for_action(
        action: ProvisioningAction,
        requested: &[String],
        existing_certificate_pem: Option<&[u8]>,
        now_epoch_secs: u64,
    ) -> Result<Self, IssuanceError> {
        match action {
            ProvisioningAction::KeepExisting => Err(IssuanceError::NothingToIssue),
            ProvisioningAction::CreateNew | ProvisioningAction::ReplaceKeyAndCertificate => {
                Self::new(requested, now_epoch_secs)
            }
            ProvisioningAction::RenewPreservingKey | ProvisioningAction::AdoptAndRenew => {
                let existing =
                    existing_certificate_pem.ok_or(IssuanceError::UnreadableExistingCertificate)?;
                // The early Linux installers issued without names; such a
                // certificate keeps none and takes the requested ones.
                let mut names = match cert_marker::subject_alt_names_from_pem(existing) {
                    Some(names) => names,
                    None if cert_marker::pins_from_pem(existing).is_some() => Vec::new(),
                    None => return Err(IssuanceError::UnreadableExistingCertificate),
                };
                names.extend(requested.iter().cloned());
                Self::new(&names, now_epoch_secs)
            }
        }
    }

    /// Whether `action` reissues over the key already on disk.
    #[must_use]
    pub const fn preserves_key(action: ProvisioningAction) -> bool {
        matches!(
            action,
            ProvisioningAction::RenewPreservingKey | ProvisioningAction::AdoptAndRenew
        )
    }

    /// The names, in issue order.
    #[must_use]
    pub fn names(&self) -> &[SubjectAltName] {
        &self.names
    }

    /// The names in `DNS:`/`IP:` form.
    #[must_use]
    pub fn tagged_names(&self) -> Vec<String> {
        self.names.iter().map(SubjectAltName::tagged).collect()
    }

    /// When the certificate becomes valid.
    #[must_use]
    pub const fn not_before_epoch_secs(&self) -> i64 {
        self.issued_at - BACKDATE_SECONDS
    }

    /// When the certificate expires: [`VALIDITY_DAYS`] after issue.
    #[must_use]
    pub const fn not_after_epoch_secs(&self) -> i64 {
        self.issued_at + VALIDITY_SECONDS
    }

    /// The `openssl req -x509 -new -key KEY -out CERT` arguments for this
    /// profile. `openssl` before 3.4 cannot backdate, so its window starts
    /// at issue; [`Self::verify_issued`] accepts that.
    #[must_use]
    pub fn openssl_req_arguments(&self) -> Vec<String> {
        [
            "-sha256",
            "-days",
            &VALIDITY_DAYS.to_string(),
            "-subj",
            &format!("/CN={COMMON_NAME}"),
            "-addext",
            "basicConstraints=critical,CA:FALSE",
            "-addext",
            "keyUsage=critical,digitalSignature",
            "-addext",
            "extendedKeyUsage=serverAuth",
            "-addext",
            &format!("subjectAltName={}", self.tagged_names().join(",")),
        ]
        .iter()
        .map(|argument| (*argument).to_owned())
        .collect()
    }

    /// A serial number unique to this issue: the digest of the key, the
    /// window and the names. Never the bare key digest, which would give
    /// every renewal over the same key the same issuer and serial.
    #[must_use]
    pub fn serial_number(&self, spki_sha256: &[u8; 32]) -> [u8; 16] {
        let mut digest = sha2::Sha256::new();
        digest.update(b"arcen-certificate-serial-v1");
        digest.update(spki_sha256);
        digest.update(self.not_before_epoch_secs().to_be_bytes());
        digest.update(self.not_after_epoch_secs().to_be_bytes());
        for name in &self.names {
            digest.update(name.tagged().as_bytes());
            digest.update([0]);
        }
        let mut serial = [0_u8; 16];
        serial.copy_from_slice(&digest.finalize()[..16]);
        // Positive and a full 16 bytes long.
        serial[0] = (serial[0] & 0x7f) | 0x40;
        serial
    }

    /// Checks that `certificate_pem` is exactly this profile over the key
    /// whose SPKI digest is `spki_sha256`.
    ///
    /// # Errors
    ///
    /// Returns the first difference.
    pub fn verify_issued(
        &self,
        certificate_pem: &[u8],
        spki_sha256: &[u8; 32],
    ) -> Result<(), IssuanceError> {
        use x509_parser::extensions::GeneralName;

        let der = std::str::from_utf8(certificate_pem)
            .ok()
            .and_then(cert_marker::pem_to_der)
            .ok_or(IssuanceError::NotSelfSigned)?;
        let (_, certificate) =
            x509_parser::parse_x509_certificate(&der).map_err(|_| IssuanceError::NotSelfSigned)?;
        if certificate.issuer().as_raw() != certificate.subject().as_raw()
            || certificate.verify_signature(None).is_err()
        {
            return Err(IssuanceError::NotSelfSigned);
        }
        if sha2::Sha256::digest(certificate.tbs_certificate.subject_pki.raw).as_slice()
            != spki_sha256
        {
            return Err(IssuanceError::PublicKeyChanged);
        }

        let subject = certificate.subject();
        let mut common_names = subject.iter_common_name();
        let single_common_name = common_names
            .next()
            .is_some_and(|name| name.as_str().ok() == Some(COMMON_NAME))
            && common_names.next().is_none()
            && subject.iter().count() == 1;
        if !single_common_name {
            return Err(IssuanceError::WrongSubject);
        }

        let basic_constraints_ok = certificate.basic_constraints().is_ok_and(|value| {
            value.is_some_and(|constraints| constraints.critical && !constraints.value.ca)
        });
        if !basic_constraints_ok {
            return Err(IssuanceError::WrongBasicConstraints);
        }
        let key_usage_ok = certificate.key_usage().is_ok_and(|value| {
            value.is_some_and(|usage| {
                usage.critical && usage.value.flags == 1 && usage.value.digital_signature()
            })
        });
        if !key_usage_ok {
            return Err(IssuanceError::WrongKeyUsage);
        }
        let extended_key_usage_ok = certificate.extended_key_usage().is_ok_and(|value| {
            value.is_some_and(|usage| {
                let usage = &usage.value;
                usage.server_auth
                    && !usage.any
                    && !usage.client_auth
                    && !usage.code_signing
                    && !usage.email_protection
                    && !usage.time_stamping
                    && !usage.ocsp_signing
                    && usage.other.is_empty()
            })
        });
        if !extended_key_usage_ok {
            return Err(IssuanceError::WrongExtendedKeyUsage);
        }

        // Every general name must be a DNS or IP entry of the profile, and
        // every profile name must be present.
        let only_dns_and_ip = certificate.subject_alternative_name().is_ok_and(|value| {
            value.is_some_and(|san| {
                san.value
                    .general_names
                    .iter()
                    .all(|name| matches!(name, GeneralName::DNSName(_) | GeneralName::IPAddress(_)))
            })
        });
        let issued_names = cert_marker::subject_alt_names_from_pem(certificate_pem)
            .ok_or(IssuanceError::WrongSubjectAltNames)?;
        if !only_dns_and_ip
            || !cert_marker::subject_alt_name_sets_match(&issued_names, &self.tagged_names())
        {
            return Err(IssuanceError::WrongSubjectAltNames);
        }

        let validity = certificate.validity();
        let not_before = validity.not_before.timestamp();
        let not_after = validity.not_after.timestamp();
        let lifetime = not_after - not_before;
        let window_ok = (self.not_before_epoch_secs() - CLOCK_SLACK_SECONDS
            ..=self.issued_at + CLOCK_SLACK_SECONDS)
            .contains(&not_before)
            && (self.not_after_epoch_secs() - CLOCK_SLACK_SECONDS
                ..=self.not_after_epoch_secs() + CLOCK_SLACK_SECONDS)
                .contains(&not_after)
            && (VALIDITY_SECONDS - CLOCK_SLACK_SECONDS
                ..=VALIDITY_SECONDS + BACKDATE_SECONDS + CLOCK_SLACK_SECONDS)
                .contains(&lifetime);
        if !window_ok {
            return Err(IssuanceError::WrongValidity);
        }
        Ok(())
    }
}

/// How a key file is encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyEncoding {
    /// `PRIVATE KEY`: what the macOS Pier, the Windows installer and the
    /// PowerShell helper write.
    Pkcs8,
    /// RFC 5915 `ECPrivateKey`: what `openssl ecparam -genkey` writes, and
    /// so what every Linux Pier holds.
    Sec1,
}

/// The elliptic curve of a host key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyCurve {
    /// NIST P-256, what Arcen generates.
    P256,
    /// NIST P-384.
    P384,
}

/// A host's private key as it is on disk, ready to sign a renewal without
/// rewriting the file.
pub struct HostKey {
    encoding: KeyEncoding,
    curve: KeyCurve,
    pkcs8: Zeroizing<Vec<u8>>,
    spki_sha256: [u8; 32],
}

impl std::fmt::Debug for HostKey {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HostKey")
            .field("encoding", &self.encoding)
            .field("curve", &self.curve)
            .field("spki_sha256", &cert_marker::hex_lower(&self.spki_sha256))
            .finish_non_exhaustive()
    }
}

impl HostKey {
    /// Reads exactly one SEC1 or PKCS #8 elliptic-curve key.
    ///
    /// # Errors
    ///
    /// Returns [`IssuanceError::UnreadableKey`],
    /// [`IssuanceError::UnsupportedKey`] or
    /// [`IssuanceError::KeyWithoutPublicKey`].
    pub fn from_pem(private_key_pem: &[u8]) -> Result<Self, IssuanceError> {
        let key = crate::host_material::parse_private_key(private_key_pem)
            .map_err(|_| IssuanceError::UnreadableKey)?;
        match key {
            PrivateKeyDer::Sec1(sec1) => {
                let (curve_oid, public_key) = ec_private_key_parts(sec1.secret_sec1_der(), None)?;
                let curve = curve_from_oid(curve_oid)?;
                let pkcs8 = wrap_sec1(sec1.secret_sec1_der(), curve_oid);
                Ok(Self {
                    encoding: KeyEncoding::Sec1,
                    curve,
                    spki_sha256: spki_sha256(curve_oid, public_key),
                    pkcs8,
                })
            }
            PrivateKeyDer::Pkcs8(pkcs8) => {
                let (curve_oid, inner) = pkcs8_ec_parts(pkcs8.secret_pkcs8_der())?;
                let curve = curve_from_oid(curve_oid)?;
                let (_, public_key) = ec_private_key_parts(inner, Some(curve_oid))?;
                Ok(Self {
                    encoding: KeyEncoding::Pkcs8,
                    curve,
                    spki_sha256: spki_sha256(curve_oid, public_key),
                    pkcs8: Zeroizing::new(pkcs8.secret_pkcs8_der().to_vec()),
                })
            }
            _ => Err(IssuanceError::UnsupportedKey),
        }
    }

    /// How the file is encoded.
    #[must_use]
    pub const fn encoding(&self) -> KeyEncoding {
        self.encoding
    }

    /// The key's curve.
    #[must_use]
    pub const fn curve(&self) -> KeyCurve {
        self.curve
    }

    /// The SHA-256 of the key's `SubjectPublicKeyInfo`: the SPKI pin.
    #[must_use]
    pub const fn spki_sha256(&self) -> &[u8; 32] {
        &self.spki_sha256
    }

    /// The SPKI pin in the `sha256/<base64>` form of `host.spki-sha256`.
    #[must_use]
    pub fn spki_pin(&self) -> String {
        format!("sha256/{}", cert_marker::base64_encode(&self.spki_sha256))
    }

    /// The key as PKCS #8 DER: the file itself, or a SEC1 key wrapped.
    #[must_use]
    pub fn pkcs8_der(&self) -> &[u8] {
        &self.pkcs8
    }
}

fn curve_from_oid(oid: &[u8]) -> Result<KeyCurve, IssuanceError> {
    if oid == OID_P256 {
        Ok(KeyCurve::P256)
    } else if oid == OID_P384 {
        Ok(KeyCurve::P384)
    } else {
        Err(IssuanceError::UnsupportedKey)
    }
}

/// One DER element: its tag, its whole encoding, its contents, and the
/// input after it.
struct Element<'a> {
    tag: u8,
    whole: &'a [u8],
    contents: &'a [u8],
}

fn read_element(input: &[u8]) -> Result<(Element<'_>, &[u8]), IssuanceError> {
    let (&tag, rest) = input.split_first().ok_or(IssuanceError::UnreadableKey)?;
    let (&first, rest) = rest.split_first().ok_or(IssuanceError::UnreadableKey)?;
    let (length, rest) = if first < 0x80 {
        (usize::from(first), rest)
    } else {
        let count = usize::from(first & 0x7f);
        if count == 0 || count > 3 || rest.len() < count {
            return Err(IssuanceError::UnreadableKey);
        }
        let (length_bytes, rest) = rest.split_at(count);
        (
            length_bytes
                .iter()
                .fold(0_usize, |length, byte| (length << 8) | usize::from(*byte)),
            rest,
        )
    };
    if rest.len() < length {
        return Err(IssuanceError::UnreadableKey);
    }
    let header = input.len() - rest.len();
    let (contents, after) = rest.split_at(length);
    Ok((
        Element {
            tag,
            whole: &input[..header + length],
            contents,
        },
        after,
    ))
}

fn expect(input: &[u8], tag: u8) -> Result<(Element<'_>, &[u8]), IssuanceError> {
    let (element, rest) = read_element(input)?;
    if element.tag == tag {
        Ok((element, rest))
    } else {
        Err(IssuanceError::UnreadableKey)
    }
}

/// The curve OID (as a whole DER element) and the public-key BIT STRING
/// (whole element) of an RFC 5915 `ECPrivateKey`. `expected_curve` is the
/// curve a PKCS #8 wrapper named; a SEC1 file must name its own.
fn ec_private_key_parts<'a>(
    der: &'a [u8],
    expected_curve: Option<&'a [u8]>,
) -> Result<(&'a [u8], &'a [u8]), IssuanceError> {
    let (sequence, trailing) = expect(der, 0x30)?;
    if !trailing.is_empty() {
        return Err(IssuanceError::UnreadableKey);
    }
    let (version, rest) = expect(sequence.contents, 0x02)?;
    if version.contents != [1] {
        return Err(IssuanceError::UnreadableKey);
    }
    let (_, mut rest) = expect(rest, 0x04)?;
    let mut curve = expected_curve;
    let mut public_key = None;
    while !rest.is_empty() {
        let (element, after) = read_element(rest)?;
        match element.tag {
            0xa0 => {
                let (oid, inner_rest) = expect(element.contents, 0x06)?;
                if !inner_rest.is_empty()
                    || expected_curve.is_some_and(|expected| expected != oid.whole)
                {
                    return Err(IssuanceError::UnsupportedKey);
                }
                curve = Some(oid.whole);
            }
            0xa1 => {
                let (bits, inner_rest) = expect(element.contents, 0x03)?;
                if !inner_rest.is_empty() || bits.contents.first() != Some(&0) {
                    return Err(IssuanceError::UnreadableKey);
                }
                public_key = Some(bits.whole);
            }
            _ => return Err(IssuanceError::UnreadableKey),
        }
        rest = after;
    }
    let curve = curve.ok_or(IssuanceError::UnsupportedKey)?;
    let public_key = public_key.ok_or(IssuanceError::KeyWithoutPublicKey)?;
    Ok((curve, public_key))
}

/// The curve OID and the inner `ECPrivateKey` of a PKCS #8 EC key.
fn pkcs8_ec_parts(der: &[u8]) -> Result<(&[u8], &[u8]), IssuanceError> {
    let (sequence, trailing) = expect(der, 0x30)?;
    if !trailing.is_empty() {
        return Err(IssuanceError::UnreadableKey);
    }
    let (version, rest) = expect(sequence.contents, 0x02)?;
    if version.contents != [0] && version.contents != [1] {
        return Err(IssuanceError::UnreadableKey);
    }
    let (algorithm, rest) = expect(rest, 0x30)?;
    let (algorithm_oid, parameters) = expect(algorithm.contents, 0x06)?;
    if algorithm_oid.whole != OID_EC_PUBLIC_KEY {
        return Err(IssuanceError::UnsupportedKey);
    }
    let (curve, after_curve) = expect(parameters, 0x06)?;
    if !after_curve.is_empty() {
        return Err(IssuanceError::UnsupportedKey);
    }
    let (private_key, _) = expect(rest, 0x04)?;
    Ok((curve.whole, private_key.contents))
}

fn der(tag: u8, contents: &[u8]) -> Vec<u8> {
    let mut encoded = Vec::with_capacity(contents.len() + 4);
    encoded.push(tag);
    let length = contents.len();
    if length < 0x80 {
        encoded.push(u8::try_from(length).unwrap_or(0));
    } else {
        let bytes = length.to_be_bytes();
        let first = bytes
            .iter()
            .position(|byte| *byte != 0)
            .unwrap_or(bytes.len() - 1);
        encoded.push(0x80 | u8::try_from(bytes.len() - first).unwrap_or(0));
        encoded.extend_from_slice(&bytes[first..]);
    }
    encoded.extend_from_slice(contents);
    encoded
}

/// RFC 5208 `PrivateKeyInfo { 0, { id-ecPublicKey, curve }, OCTET STRING
/// ECPrivateKey }` around a SEC1 key. The SEC1 bytes are carried unchanged.
fn wrap_sec1(sec1: &[u8], curve_oid: &[u8]) -> Zeroizing<Vec<u8>> {
    let mut algorithm = Vec::with_capacity(OID_EC_PUBLIC_KEY.len() + curve_oid.len());
    algorithm.extend_from_slice(OID_EC_PUBLIC_KEY);
    algorithm.extend_from_slice(curve_oid);
    let mut body = Zeroizing::new(vec![0x02, 0x01, 0x00]);
    body.extend(der(0x30, &algorithm));
    let octets = Zeroizing::new(der(0x04, sec1));
    body.extend_from_slice(&octets);
    Zeroizing::new(der(0x30, &body))
}

/// SHA-256 of `SubjectPublicKeyInfo { { id-ecPublicKey, curve }, publicKey }`,
/// the same DER a certificate carries.
fn spki_sha256(curve_oid: &[u8], public_key_bits: &[u8]) -> [u8; 32] {
    let mut algorithm = Vec::with_capacity(OID_EC_PUBLIC_KEY.len() + curve_oid.len());
    algorithm.extend_from_slice(OID_EC_PUBLIC_KEY);
    algorithm.extend_from_slice(curve_oid);
    let mut body = der(0x30, &algorithm);
    body.extend_from_slice(public_key_bits);
    sha2::Sha256::digest(der(0x30, &body)).into()
}

/// Which key a certificate is issued over.
#[cfg(feature = "issuer")]
#[derive(Debug, Clone, Copy)]
pub enum IssuerKey<'a> {
    /// The key already on disk; the file is not rewritten.
    Existing(&'a HostKey),
    /// A fresh P-256 key (first issue or an explicit rekey).
    Generate,
}

/// What [`issue`] produced.
#[cfg(feature = "issuer")]
pub struct IssuedMaterial {
    /// The certificate, PEM.
    pub certificate_pem: String,
    /// The new key, PKCS #8 PEM; `None` when the existing key was kept.
    pub new_key_pem: Option<Zeroizing<String>>,
    /// The SPKI digest the certificate was verified against.
    pub spki_sha256: [u8; 32],
}

#[cfg(feature = "issuer")]
impl std::fmt::Debug for IssuedMaterial {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("IssuedMaterial")
            .field("new_key", &self.new_key_pem.is_some())
            .field("spki_sha256", &cert_marker::hex_lower(&self.spki_sha256))
            .finish_non_exhaustive()
    }
}

/// Issues `profile` over `key`, then verifies the result against the same
/// profile and key before returning it.
///
/// # Errors
///
/// Returns [`IssuanceError::IssuerFailed`] or the verification's refusal.
#[cfg(feature = "issuer")]
pub fn issue(
    profile: &IssuanceProfile,
    key: IssuerKey<'_>,
) -> Result<IssuedMaterial, IssuanceError> {
    use rcgen::{
        CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair,
        KeyUsagePurpose, SanType, SerialNumber,
    };

    let generated;
    let (signer, new_key_pem, host_key) = match key {
        IssuerKey::Existing(host_key) => {
            let algorithm = match host_key.curve {
                KeyCurve::P256 => &rcgen::PKCS_ECDSA_P256_SHA256,
                KeyCurve::P384 => &rcgen::PKCS_ECDSA_P384_SHA384,
            };
            let signer = KeyPair::from_pkcs8_der_and_sign_algo(
                &rustls::pki_types::PrivatePkcs8KeyDer::from(host_key.pkcs8_der()),
                algorithm,
            )
            .map_err(|_| IssuanceError::UnreadableKey)?;
            (signer, None, host_key)
        }
        IssuerKey::Generate => {
            let signer = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)
                .map_err(|_| IssuanceError::IssuerFailed)?;
            let pem = Zeroizing::new(signer.serialize_pem());
            generated = HostKey::from_pem(pem.as_bytes())?;
            (signer, Some(pem), &generated)
        }
    };

    let mut params = CertificateParams::default();
    let mut subject = DistinguishedName::new();
    subject.push(DnType::CommonName, COMMON_NAME);
    params.distinguished_name = subject;
    params.is_ca = IsCa::ExplicitNoCa;
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    params.subject_alt_names = profile
        .names()
        .iter()
        .map(|name| match name {
            SubjectAltName::Dns(name) => rcgen::string::Ia5String::try_from(name.as_str())
                .map(SanType::DnsName)
                .map_err(|_| IssuanceError::InvalidSubjectAltName),
            SubjectAltName::Ip(address) => Ok(SanType::IpAddress(*address)),
        })
        .collect::<Result<_, _>>()?;
    params.not_before = time::OffsetDateTime::from_unix_timestamp(profile.not_before_epoch_secs())
        .map_err(|_| IssuanceError::ClockOutOfRange)?;
    params.not_after = time::OffsetDateTime::from_unix_timestamp(profile.not_after_epoch_secs())
        .map_err(|_| IssuanceError::ClockOutOfRange)?;
    params.serial_number = Some(SerialNumber::from(
        profile.serial_number(host_key.spki_sha256()).to_vec(),
    ));

    let certificate_pem = params
        .self_signed(&signer)
        .map_err(|_| IssuanceError::IssuerFailed)?
        .pem();
    profile.verify_issued(certificate_pem.as_bytes(), host_key.spki_sha256())?;
    Ok(IssuedMaterial {
        certificate_pem,
        new_key_pem,
        spki_sha256: *host_key.spki_sha256(),
    })
}

#[cfg(test)]
mod tests;
