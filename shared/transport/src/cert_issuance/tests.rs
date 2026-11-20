// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Tests for the shared certificate profile, key reading and verification.

use rcgen::{
    CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair,
    KeyUsagePurpose,
};

use super::*;
use crate::test_keys::openssl_sec1_key;

const NOW: u64 = 1_800_000_000;

fn names(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

/// A certificate in the shape `openssl req -x509` writes from
/// [`IssuanceProfile::openssl_req_arguments`]: the profile's names, window
/// starting at issue (no backdate).
fn openssl_shaped(key: &KeyPair, profile: &IssuanceProfile) -> Vec<u8> {
    let mut params = CertificateParams::new(Vec::<String>::new()).expect("params");
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
            SubjectAltName::Dns(name) => rcgen::SanType::DnsName(
                rcgen::string::Ia5String::try_from(name.as_str()).expect("dns"),
            ),
            SubjectAltName::Ip(address) => rcgen::SanType::IpAddress(*address),
        })
        .collect();
    params.not_before =
        time::OffsetDateTime::from_unix_timestamp(profile.issued_at).expect("not before");
    params.not_after = time::OffsetDateTime::from_unix_timestamp(profile.not_after_epoch_secs())
        .expect("not after");
    params
        .self_signed(key)
        .expect("certificate")
        .pem()
        .into_bytes()
}

fn pkcs8_pem(key: &KeyPair) -> Vec<u8> {
    key.serialize_pem().into_bytes()
}

#[test]
fn sec1_and_pkcs8_files_of_one_key_give_the_same_pin_as_its_certificate() {
    let key = KeyPair::generate().expect("key");
    let profile = IssuanceProfile::new(&names(&["pier.example.internal"]), NOW).expect("profile");
    let certificate = openssl_shaped(&key, &profile);
    let served_pin = cert_marker::pins_from_pem(&certificate).expect("pins").spki;

    let sec1 = HostKey::from_pem(&openssl_sec1_key(&key)).expect("SEC1 key reads");
    let pkcs8 = HostKey::from_pem(&pkcs8_pem(&key)).expect("PKCS #8 key reads");
    assert_eq!(sec1.encoding(), KeyEncoding::Sec1);
    assert_eq!(pkcs8.encoding(), KeyEncoding::Pkcs8);
    assert_eq!(sec1.curve(), KeyCurve::P256);
    assert_eq!(sec1.spki_pin(), served_pin, "SEC1 pin is the served pin");
    assert_eq!(
        pkcs8.spki_pin(),
        served_pin,
        "PKCS #8 pin is the served pin"
    );
    assert_eq!(sec1.spki_sha256(), pkcs8.spki_sha256());
}

#[test]
fn a_wrapped_sec1_key_signs_as_the_same_key() {
    // ring, under rcgen, reads only PKCS #8: the wrap is what lets a shared
    // issuer renew over the SEC1 key a Linux Pier holds.
    let key = KeyPair::generate().expect("key");
    let sec1 = HostKey::from_pem(&openssl_sec1_key(&key)).expect("SEC1 key reads");
    let wrapped = KeyPair::from_pkcs8_der_and_sign_algo(
        &rustls::pki_types::PrivatePkcs8KeyDer::from(sec1.pkcs8_der()),
        &rcgen::PKCS_ECDSA_P256_SHA256,
    )
    .expect("ring accepts the wrapped key");
    assert_eq!(wrapped.public_key_raw(), key.public_key_raw());
}

#[test]
fn keys_arcen_cannot_renew_over_are_refused_by_name() {
    assert_eq!(
        HostKey::from_pem(b"not a key").err(),
        Some(IssuanceError::UnreadableKey)
    );
    let ed25519 = KeyPair::generate_for(&rcgen::PKCS_ED25519).expect("ed25519");
    assert_eq!(
        HostKey::from_pem(&pkcs8_pem(&ed25519)).err(),
        Some(IssuanceError::UnsupportedKey)
    );
    let mut two = pkcs8_pem(&KeyPair::generate().expect("key"));
    two.extend(pkcs8_pem(&KeyPair::generate().expect("key")));
    assert_eq!(
        HostKey::from_pem(&two).err(),
        Some(IssuanceError::UnreadableKey)
    );
}

#[test]
fn a_renewal_keeps_every_existing_name_and_adds_the_requested_ones() {
    let key = KeyPair::generate().expect("key");
    let first = IssuanceProfile::new(&names(&["pier.example.internal", "203.0.113.7"]), NOW)
        .expect("first");
    let existing = openssl_shaped(&key, &first);
    for action in [
        ProvisioningAction::RenewPreservingKey,
        ProvisioningAction::AdoptAndRenew,
    ] {
        let renewal = IssuanceProfile::for_action(
            action,
            &names(&["203.0.113.8", "PIER.example.internal."]),
            Some(&existing),
            NOW + 1_000,
        )
        .expect("renewal");
        assert_eq!(
            renewal.tagged_names(),
            names(&[
                "DNS:pier.example.internal",
                "IP:203.0.113.7",
                "IP:203.0.113.8"
            ]),
            "{action:?} keeps the old names, merges new ones once"
        );
        assert!(IssuanceProfile::preserves_key(action));
    }
}

#[test]
fn a_name_less_legacy_certificate_takes_the_requested_names() {
    // Early Linux installers issued CN-only certificates; adopting one must
    // not fail for want of names to keep.
    let key = KeyPair::generate().expect("key");
    let mut params = CertificateParams::new(Vec::<String>::new()).expect("params");
    params.distinguished_name = DistinguishedName::new();
    params
        .distinguished_name
        .push(DnType::CommonName, COMMON_NAME);
    let legacy = params.self_signed(&key).expect("cert").pem().into_bytes();
    assert_eq!(cert_marker::subject_alt_names_from_pem(&legacy), None);
    let profile = IssuanceProfile::for_action(
        ProvisioningAction::AdoptAndRenew,
        &names(&["pier.example.internal"]),
        Some(&legacy),
        NOW,
    )
    .expect("profile");
    assert_eq!(
        profile.tagged_names(),
        names(&["DNS:pier.example.internal"])
    );
}

#[test]
fn a_new_key_carries_only_the_requested_names() {
    for action in [
        ProvisioningAction::CreateNew,
        ProvisioningAction::ReplaceKeyAndCertificate,
    ] {
        let profile = IssuanceProfile::for_action(action, &names(&["new.example"]), None, NOW)
            .expect("profile");
        assert_eq!(profile.tagged_names(), names(&["DNS:new.example"]));
        assert!(!IssuanceProfile::preserves_key(action));
    }
    assert_eq!(
        IssuanceProfile::for_action(ProvisioningAction::KeepExisting, &[], None, NOW).err(),
        Some(IssuanceError::NothingToIssue)
    );
    assert_eq!(
        IssuanceProfile::for_action(
            ProvisioningAction::RenewPreservingKey,
            &[],
            Some(b"garbage"),
            NOW
        )
        .err(),
        Some(IssuanceError::UnreadableExistingCertificate)
    );
    assert_eq!(
        IssuanceProfile::new(&[], NOW).err(),
        Some(IssuanceError::NoSubjectAltName)
    );
}

#[test]
fn a_name_cannot_smuggle_another_entry_into_openssl() {
    for hostile in [
        "pier.example,IP:198.51.100.1",
        "pier.example\nkeyUsage=keyCertSign",
        "pier example",
        "DNS:a=b",
        "",
    ] {
        assert!(
            SubjectAltName::parse(hostile).is_err(),
            "{hostile:?} must be refused"
        );
    }
    assert_eq!(
        SubjectAltName::parse("IP:2001:db8::1").expect("v6"),
        SubjectAltName::Ip("2001:db8::1".parse().expect("address"))
    );
}

#[test]
fn the_openssl_arguments_state_the_whole_profile() {
    let profile =
        IssuanceProfile::new(&names(&["pier.example.internal", "203.0.113.7"]), NOW).expect("p");
    let arguments = profile.openssl_req_arguments();
    for expected in [
        "-sha256",
        "/CN=Arcen Pier",
        "basicConstraints=critical,CA:FALSE",
        "keyUsage=critical,digitalSignature",
        "extendedKeyUsage=serverAuth",
        "subjectAltName=DNS:pier.example.internal,IP:203.0.113.7",
        "825",
    ] {
        assert!(
            arguments.iter().any(|argument| argument == expected),
            "{expected} missing from {arguments:?}"
        );
    }
}

#[test]
fn the_window_starts_five_minutes_before_issue_and_lasts_the_validity() {
    let profile = IssuanceProfile::new(&names(&["pier.example"]), NOW).expect("profile");
    assert_eq!(
        profile.not_before_epoch_secs(),
        i64::try_from(NOW).expect("now") - BACKDATE_SECONDS
    );
    assert_eq!(
        profile.not_after_epoch_secs() - i64::try_from(NOW).expect("now"),
        VALIDITY_SECONDS
    );
}

#[test]
fn verification_accepts_exactly_the_profile_over_the_same_key() {
    let key = KeyPair::generate().expect("key");
    let host_key = HostKey::from_pem(&openssl_sec1_key(&key)).expect("key");
    let profile =
        IssuanceProfile::new(&names(&["pier.example.internal", "203.0.113.7"]), NOW).expect("p");
    let certificate = openssl_shaped(&key, &profile);
    assert_eq!(
        profile.verify_issued(&certificate, host_key.spki_sha256()),
        Ok(())
    );

    // Over another key: every SPKI pin would break.
    let other = HostKey::from_pem(&pkcs8_pem(&KeyPair::generate().expect("key"))).expect("key");
    assert_eq!(
        profile.verify_issued(&certificate, other.spki_sha256()),
        Err(IssuanceError::PublicKeyChanged)
    );

    // A name dropped, or one added, is not this profile.
    let fewer = IssuanceProfile::new(&names(&["pier.example.internal"]), NOW).expect("p");
    assert_eq!(
        fewer.verify_issued(&certificate, host_key.spki_sha256()),
        Err(IssuanceError::WrongSubjectAltNames)
    );
    let dropped = openssl_shaped(&key, &fewer);
    assert_eq!(
        profile.verify_issued(&dropped, host_key.spki_sha256()),
        Err(IssuanceError::WrongSubjectAltNames)
    );

    // A window from another day.
    let later = IssuanceProfile::new(
        &names(&["pier.example.internal", "203.0.113.7"]),
        NOW + 86_400,
    )
    .expect("p");
    assert_eq!(
        later.verify_issued(&certificate, host_key.spki_sha256()),
        Err(IssuanceError::WrongValidity)
    );
}

#[test]
fn verification_refuses_the_old_rcgen_defaults() {
    // What the macOS Pier issued before: rcgen's default subject, no key
    // usage, no basic constraints.
    let key = KeyPair::generate().expect("key");
    let host_key = HostKey::from_pem(&pkcs8_pem(&key)).expect("key");
    let profile = IssuanceProfile::new(&names(&["pier.example"]), NOW).expect("p");
    let mut params = CertificateParams::new(names(&["pier.example"])).expect("params");
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    params.not_before = time::OffsetDateTime::from_unix_timestamp(profile.issued_at).expect("t");
    params.not_after =
        time::OffsetDateTime::from_unix_timestamp(profile.not_after_epoch_secs()).expect("t");
    let old_macos = params.self_signed(&key).expect("cert").pem().into_bytes();
    assert_eq!(
        profile.verify_issued(&old_macos, host_key.spki_sha256()),
        Err(IssuanceError::WrongSubject)
    );

    let mut subject = DistinguishedName::new();
    subject.push(DnType::CommonName, COMMON_NAME);
    params.distinguished_name = subject;
    let no_constraints = params.self_signed(&key).expect("cert").pem().into_bytes();
    assert_eq!(
        profile.verify_issued(&no_constraints, host_key.spki_sha256()),
        Err(IssuanceError::WrongBasicConstraints)
    );
    params.is_ca = IsCa::ExplicitNoCa;
    let no_key_usage = params.self_signed(&key).expect("cert").pem().into_bytes();
    assert_eq!(
        profile.verify_issued(&no_key_usage, host_key.spki_sha256()),
        Err(IssuanceError::WrongKeyUsage)
    );
    params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyCertSign,
    ];
    let wide_key_usage = params.self_signed(&key).expect("cert").pem().into_bytes();
    assert_eq!(
        profile.verify_issued(&wide_key_usage, host_key.spki_sha256()),
        Err(IssuanceError::WrongKeyUsage)
    );
}

#[test]
fn every_renewal_gets_its_own_serial() {
    let key = HostKey::from_pem(&pkcs8_pem(&KeyPair::generate().expect("key"))).expect("key");
    let first = IssuanceProfile::new(&names(&["pier.example"]), NOW).expect("p");
    let second = IssuanceProfile::new(&names(&["pier.example"]), NOW + 1).expect("p");
    let serial = first.serial_number(key.spki_sha256());
    assert_ne!(serial, second.serial_number(key.spki_sha256()));
    assert_eq!(serial[0] & 0x80, 0, "positive");
    assert_ne!(serial[0], 0, "full length");
}

#[cfg(feature = "issuer")]
mod issuer {
    use super::*;

    #[test]
    fn renewing_over_a_sec1_key_keeps_the_key_file_and_the_pin() {
        let key = KeyPair::generate().expect("key");
        let sec1_file = openssl_sec1_key(&key);
        let first_profile =
            IssuanceProfile::new(&names(&["pier.example.internal", "203.0.113.7"]), NOW)
                .expect("p");
        let first = openssl_shaped(&key, &first_profile);
        let deployed_spki_pin = cert_marker::pins_from_pem(&first).expect("pins").spki;

        let host_key = HostKey::from_pem(&sec1_file).expect("SEC1 key reads");
        let renewal = IssuanceProfile::for_action(
            ProvisioningAction::RenewPreservingKey,
            &names(&["pier.example.internal"]),
            Some(&first),
            NOW + 700 * 86_400,
        )
        .expect("renewal");
        let issued = issue(&renewal, IssuerKey::Existing(&host_key)).expect("issued");
        assert!(
            issued.new_key_pem.is_none(),
            "the key file is not rewritten"
        );
        let renewed_pins =
            cert_marker::pins_from_pem(issued.certificate_pem.as_bytes()).expect("pins");
        assert_eq!(renewed_pins.spki, deployed_spki_pin, "SPKI pin unchanged");
        assert_ne!(
            renewed_pins.certificate,
            cert_marker::pins_from_pem(&first)
                .expect("pins")
                .certificate
        );
        assert!(cert_marker::subject_alt_names_match_pem(
            issued.certificate_pem.as_bytes(),
            &names(&["DNS:pier.example.internal", "IP:203.0.113.7"])
        ));
        assert_eq!(
            renewal.verify_issued(issued.certificate_pem.as_bytes(), host_key.spki_sha256()),
            Ok(())
        );
    }

    #[test]
    fn renewing_over_a_pkcs8_key_keeps_the_pin() {
        let key = KeyPair::generate().expect("key");
        let host_key = HostKey::from_pem(&pkcs8_pem(&key)).expect("key");
        let profile = IssuanceProfile::new(&names(&["pier.example"]), NOW).expect("p");
        let issued = issue(&profile, IssuerKey::Existing(&host_key)).expect("issued");
        assert_eq!(
            cert_marker::pins_from_pem(issued.certificate_pem.as_bytes())
                .expect("pins")
                .spki,
            host_key.spki_pin()
        );
    }

    #[test]
    fn a_new_key_is_p256_pkcs8_and_its_certificate_is_the_profile() {
        let profile =
            IssuanceProfile::new(&names(&["pier.example", "2001:db8::7"]), NOW).expect("p");
        let issued = issue(&profile, IssuerKey::Generate).expect("issued");
        let key_pem = issued.new_key_pem.as_ref().expect("new key");
        let host_key = HostKey::from_pem(key_pem.as_bytes()).expect("key");
        assert_eq!(host_key.encoding(), KeyEncoding::Pkcs8);
        assert_eq!(host_key.curve(), KeyCurve::P256);
        assert_eq!(&issued.spki_sha256, host_key.spki_sha256());
        assert_eq!(
            profile.verify_issued(issued.certificate_pem.as_bytes(), host_key.spki_sha256()),
            Ok(())
        );
        let validity =
            cert_marker::validity_from_pem(issued.certificate_pem.as_bytes()).expect("validity");
        assert_eq!(validity.not_before, profile.not_before_epoch_secs());
        assert_eq!(validity.not_after, profile.not_after_epoch_secs());
        // And the shared legacy check still recognises Arcen's own profile.
        assert!(cert_marker::is_self_signed_pem(
            issued.certificate_pem.as_bytes()
        ));
    }

    #[test]
    fn two_renewals_over_one_key_do_not_share_a_serial() {
        // rcgen's default serial is the key digest, so before this every
        // renewal over the same key repeated issuer and serial.
        let host_key =
            HostKey::from_pem(&pkcs8_pem(&KeyPair::generate().expect("key"))).expect("key");
        let serial = |now| {
            let profile = IssuanceProfile::new(&names(&["pier.example"]), now).expect("p");
            let issued = issue(&profile, IssuerKey::Existing(&host_key)).expect("issued");
            let der = cert_marker::pem_to_der(&issued.certificate_pem).expect("der");
            let (_, certificate) = x509_parser::parse_x509_certificate(&der).expect("parse");
            certificate.raw_serial().to_vec()
        };
        assert_ne!(serial(NOW), serial(NOW + 60));
    }
}
