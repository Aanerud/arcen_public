// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! A Pier's certificate directory: the file names every Pier and installer
//! uses, what is in the directory (the input to
//! [`crate::cert_provisioning::plan`]) and the pin and ownership files written
//! beside a certificate.
//!
//! The names and the pin-file text are a compatibility contract: Decks were
//! paired against the pins in these files, operators compare them with
//! `openssl x509 -fingerprint -sha256`, and the ownership marker is how a
//! later installer recognises material an earlier one wrote. They do not
//! change between Piers, or between releases.
//!
//! Where the directory is, and how files are written into it (modes, ACLs,
//! atomic publication), belong to each platform.

use std::path::{Path, PathBuf};

use crate::cert_marker::{self, LegacyArcenEvidence, OwnershipMarker};
use crate::cert_provisioning::{MaterialOwnership, MaterialState};

/// Certificate chain.
pub const CERTIFICATE_FILE: &str = "host.crt";
/// Private key.
pub const KEY_FILE: &str = "host.key";
/// Ownership marker: the pins of the certificate Arcen issued.
pub const MARKER_FILE: &str = "host.generated-by-arcen";
/// Whole-certificate pin, as `openssl x509 -fingerprint -sha256` prints it.
pub const CERTIFICATE_PIN_FILE: &str = "host.cert-sha256";
/// Subject public key pin, `sha256/<base64>`.
pub const SPKI_PIN_FILE: &str = "host.spki-sha256";
/// Lifetime of a certificate a Pier issues, in days: the CA/Browser Forum
/// maximum.
pub const VALIDITY_DAYS: i64 = 825;
/// How close to expiry a certificate is due for renewal.
pub const RENEW_WITHIN_SECONDS: i64 = 30 * 24 * 60 * 60;

/// The managed files in one directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaterialPaths {
    /// The directory holding all of it.
    pub directory: PathBuf,
    /// [`CERTIFICATE_FILE`].
    pub certificate: PathBuf,
    /// [`KEY_FILE`].
    pub key: PathBuf,
    /// [`MARKER_FILE`].
    pub marker: PathBuf,
    /// [`CERTIFICATE_PIN_FILE`].
    pub certificate_pin: PathBuf,
    /// [`SPKI_PIN_FILE`].
    pub spki_pin: PathBuf,
}

impl MaterialPaths {
    /// The managed files inside `directory`.
    #[must_use]
    pub fn in_directory(directory: &Path) -> Self {
        Self {
            directory: directory.to_path_buf(),
            certificate: directory.join(CERTIFICATE_FILE),
            key: directory.join(KEY_FILE),
            marker: directory.join(MARKER_FILE),
            certificate_pin: directory.join(CERTIFICATE_PIN_FILE),
            spki_pin: directory.join(SPKI_PIN_FILE),
        }
    }

    /// Every managed file, in [`crate::cert_transaction::MANAGED_FILES`] order.
    #[must_use]
    pub fn managed(&self) -> [&PathBuf; 5] {
        [
            &self.key,
            &self.certificate,
            &self.certificate_pin,
            &self.spki_pin,
            &self.marker,
        ]
    }
}

/// What the platform knows beyond the directory's contents.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InspectContext<'a> {
    /// An interrupted publication left staged files behind.
    pub stale_staging_present: bool,
    /// The names the platform's historical issuer would have put in a
    /// certificate on this machine, when it can tell.
    pub machine_sans: Option<&'a [String]>,
}

/// Reads the directory into the input of
/// [`crate::cert_provisioning::plan`].
///
/// # Errors
///
/// Returns the read error when a certificate is present but cannot be read:
/// a decision about material nobody can inspect would be a guess.
pub fn inspect(
    paths: &MaterialPaths,
    now_epoch_secs: u64,
    context: InspectContext<'_>,
) -> std::io::Result<MaterialState> {
    let certificate_present = paths.certificate.is_file();
    let key_present = paths.key.is_file();
    if !certificate_present && !key_present {
        return Ok(MaterialState {
            stale_staging_present: context.stale_staging_present,
            ..MaterialState::absent()
        });
    }

    let certificate_bytes = if certificate_present {
        Some(std::fs::read(&paths.certificate)?)
    } else {
        None
    };
    let ownership = certificate_bytes
        .as_deref()
        .map(|bytes| ownership(paths, bytes));
    let now = i64::try_from(now_epoch_secs).unwrap_or(i64::MAX);
    let (certificate_valid, expiring_or_expired) = certificate_bytes
        .as_deref()
        .and_then(cert_marker::validity_from_pem)
        .map_or((false, false), |window| {
            (
                window.is_current(now),
                window.is_due_for_renewal(now, RENEW_WITHIN_SECONDS),
            )
        });

    Ok(MaterialState {
        certificate_present,
        key_present,
        ownership,
        certificate_valid,
        expiring_or_expired,
        stale_staging_present: context.stale_staging_present,
        legacy_arcen_self_signed: certificate_bytes.as_deref().is_some_and(|bytes| {
            cert_marker::is_legacy_arcen_self_signed_pem(
                bytes,
                legacy_evidence(paths, bytes, context.machine_sans),
            )
        }),
    })
}

/// Who owns the certificate on disk: marked by Arcen, unmarked, or marked
/// for a different certificate.
#[must_use]
pub fn ownership(paths: &MaterialPaths, certificate_pem: &[u8]) -> MaterialOwnership {
    if !paths.marker.exists() {
        MaterialOwnership::Foreign
    } else if marker_matches(paths, certificate_pem) {
        MaterialOwnership::Owned
    } else {
        MaterialOwnership::Ambiguous
    }
}

/// Whether the ownership marker describes `certificate_pem`: both pins must
/// match, so a marker left behind under a replaced certificate does not.
#[must_use]
pub fn marker_matches(paths: &MaterialPaths, certificate_pem: &[u8]) -> bool {
    let Ok(recorded) = std::fs::read_to_string(&paths.marker) else {
        return false;
    };
    let Ok(marker) = OwnershipMarker::parse(&recorded) else {
        return false;
    };
    let Some(pins) = cert_marker::pins_from_pem(certificate_pem) else {
        return false;
    };
    marker.matches(&pins.certificate, &pins.spki)
}

fn legacy_evidence(
    paths: &MaterialPaths,
    certificate_pem: &[u8],
    machine_sans: Option<&[String]>,
) -> LegacyArcenEvidence {
    let companion_pins_match = std::fs::read_to_string(&paths.certificate_pin)
        .ok()
        .zip(std::fs::read_to_string(&paths.spki_pin).ok())
        .is_some_and(|(certificate_pin, spki_pin)| {
            cert_marker::companion_pins_match_pem(certificate_pem, &certificate_pin, &spki_pin)
        });
    LegacyArcenEvidence {
        arcen_tls_directory: true,
        companion_pins_match,
        machine_sans_match: machine_sans
            .is_some_and(|names| cert_marker::subject_alt_names_match_pem(certificate_pem, names)),
    }
}

/// The text of the three files written beside an issued certificate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinFiles {
    /// [`CERTIFICATE_PIN_FILE`]: `sha256 Fingerprint=AB:CD:…`.
    pub certificate_pin: String,
    /// [`SPKI_PIN_FILE`]: `sha256/<base64>`.
    pub spki_pin: String,
    /// [`MARKER_FILE`].
    pub marker: String,
}

impl PinFiles {
    /// Renders the pin and marker files for `certificate_pem`.
    ///
    /// # Errors
    ///
    /// Returns a message when the certificate cannot be pinned.
    pub fn for_certificate(certificate_pem: &[u8]) -> Result<Self, String> {
        let pins = cert_marker::pins_from_pem(certificate_pem)
            .ok_or_else(|| "cannot pin the generated certificate".to_owned())?;
        let marker = OwnershipMarker::new(&pins.certificate, &pins.spki)
            .map_err(|error| format!("ownership marker: {error}"))?;
        Ok(Self {
            certificate_pin: format!(
                "sha256 Fingerprint={}\n",
                cert_marker::colon_hex(&pins.certificate)
            ),
            spki_pin: format!("{}\n", pins.spki),
            marker: marker.render(),
        })
    }

    /// Each file's path and text, in the order they are written.
    #[must_use]
    pub fn files<'a>(&'a self, paths: &'a MaterialPaths) -> [(&'a Path, &'a str); 3] {
        [
            (&paths.certificate_pin, &self.certificate_pin),
            (&paths.spki_pin, &self.spki_pin),
            (&paths.marker, &self.marker),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cert_provisioning::{ProvisioningAction, ProvisioningRequest, plan};

    fn scratch(name: &str) -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("arcen-cert-store-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).expect("scratch directory");
        directory
    }

    fn issue(names: &[&str]) -> (String, String) {
        let key = rcgen::KeyPair::generate().expect("key");
        let mut params = rcgen::CertificateParams::new(
            names
                .iter()
                .map(|name| (*name).to_owned())
                .collect::<Vec<_>>(),
        )
        .expect("params");
        params.not_before = rcgen::date_time_ymd(2026, 1, 1);
        params.not_after = rcgen::date_time_ymd(2028, 4, 5);
        params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
        let certificate = params.self_signed(&key).expect("certificate");
        (certificate.pem(), key.serialize_pem())
    }

    const JUNE_2026: u64 = 1_780_000_000;

    #[test]
    fn names_are_the_deployed_contract() {
        let paths = MaterialPaths::in_directory(Path::new("/etc/arcen"));
        assert_eq!(paths.certificate, Path::new("/etc/arcen/host.crt"));
        assert_eq!(paths.key, Path::new("/etc/arcen/host.key"));
        assert_eq!(
            paths.marker,
            Path::new("/etc/arcen/host.generated-by-arcen")
        );
        assert_eq!(
            paths.certificate_pin,
            Path::new("/etc/arcen/host.cert-sha256")
        );
        assert_eq!(paths.spki_pin, Path::new("/etc/arcen/host.spki-sha256"));
        let names = paths.managed().map(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default()
        });
        assert_eq!(names, crate::cert_transaction::MANAGED_FILES);
    }

    #[test]
    fn pin_files_keep_the_deployed_text() {
        let (certificate, _) = issue(&["pier.example.internal"]);
        let pins = cert_marker::pins_from_pem(certificate.as_bytes()).expect("pins");
        let files = PinFiles::for_certificate(certificate.as_bytes()).expect("pin files");
        assert_eq!(
            files.certificate_pin,
            format!(
                "sha256 Fingerprint={}\n",
                cert_marker::colon_hex(&pins.certificate)
            )
        );
        assert_eq!(files.spki_pin, format!("{}\n", pins.spki));
        assert!(files.spki_pin.starts_with("sha256/"));
        assert!(
            OwnershipMarker::parse(&files.marker)
                .expect("marker")
                .matches(&pins.certificate, &pins.spki)
        );
        // What a Deck operator copies from the pin file is what a Deck pins.
        assert!(cert_marker::companion_pins_match_pem(
            certificate.as_bytes(),
            &files.certificate_pin,
            &files.spki_pin
        ));
    }

    #[test]
    fn inspect_reads_owned_foreign_and_ambiguous_material() {
        let directory = scratch("ownership");
        let paths = MaterialPaths::in_directory(&directory);
        assert_eq!(
            inspect(&paths, JUNE_2026, InspectContext::default()).expect("inspect"),
            MaterialState::absent()
        );

        let (certificate, key) = issue(&["pier.example.internal"]);
        std::fs::write(&paths.certificate, &certificate).expect("certificate");
        std::fs::write(&paths.key, &key).expect("key");
        let foreign = inspect(&paths, JUNE_2026, InspectContext::default()).expect("inspect");
        assert_eq!(foreign.ownership, Some(MaterialOwnership::Foreign));
        assert!(foreign.certificate_valid && !foreign.expiring_or_expired);

        let files = PinFiles::for_certificate(certificate.as_bytes()).expect("pins");
        for (path, text) in files.files(&paths) {
            std::fs::write(path, text).expect("pin file");
        }
        let owned = inspect(&paths, JUNE_2026, InspectContext::default()).expect("inspect");
        assert_eq!(owned.ownership, Some(MaterialOwnership::Owned));
        assert_eq!(
            plan(ProvisioningRequest::Ensure, owned).map(|plan| plan.action),
            Ok(ProvisioningAction::KeepExisting)
        );

        let (other, _) = issue(&["pier.example.internal"]);
        std::fs::write(&paths.certificate, &other).expect("replace certificate");
        assert_eq!(
            inspect(&paths, JUNE_2026, InspectContext::default())
                .expect("inspect")
                .ownership,
            Some(MaterialOwnership::Ambiguous)
        );
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn inspect_reports_renewal_partial_material_and_staging() {
        let directory = scratch("renewal");
        let paths = MaterialPaths::in_directory(&directory);
        let (certificate, _) = issue(&["pier.example.internal"]);
        std::fs::write(&paths.certificate, &certificate).expect("certificate");
        let context = InspectContext {
            stale_staging_present: true,
            machine_sans: None,
        };
        let state = inspect(&paths, JUNE_2026, context).expect("inspect");
        assert!(state.is_partial());
        assert!(state.stale_staging_present);

        // 2028-03-20 is within thirty days of the 2028-04-05 expiry.
        let due = inspect(&paths, 1_837_000_000, InspectContext::default()).expect("inspect");
        assert!(due.certificate_valid && due.expiring_or_expired);
        let expired = inspect(&paths, 1_900_000_000, InspectContext::default()).expect("inspect");
        assert!(!expired.certificate_valid && expired.expiring_or_expired);
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn machine_names_are_legacy_evidence_only_when_given() {
        let directory = scratch("machine");
        let paths = MaterialPaths::in_directory(&directory);
        let (certificate, key) = issue(&["pier.example.internal", "203.0.113.7"]);
        std::fs::write(&paths.certificate, &certificate).expect("certificate");
        std::fs::write(&paths.key, &key).expect("key");
        let names = vec![
            "DNS:pier.example.internal".to_owned(),
            "IP:203.0.113.7".to_owned(),
        ];
        let evidence = legacy_evidence(&paths, certificate.as_bytes(), Some(&names));
        assert!(evidence.machine_sans_match);
        assert!(!legacy_evidence(&paths, certificate.as_bytes(), None).machine_sans_match);
        let _ = std::fs::remove_dir_all(&directory);
    }
}
