// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Tests for the shared host TLS material path, with an in-memory file system
//! standing in for each platform, and the on-disk formats deployed Piers
//! already hold.

use std::cell::Cell;
use std::collections::HashMap;
use std::sync::atomic::AtomicU64;

use rcgen::{CertificateParams, ExtendedKeyUsagePurpose, KeyPair};

use super::*;
use crate::test_keys::{openssl_sec1_key, sec1_label};
use crate::tls::{ClockError, RingCipherSuite, TlsVersionFloor};

const NOW: u64 = 1_800_000_000;
const SERVICE_UID: u32 = 1001;

/// Which platform's facts the fake reports.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Facts {
    Unix(UnixFileFacts),
    Windows(WindowsFileFacts, bool),
}

impl MaterialSnapshot for Facts {
    fn length(&self) -> u64 {
        match self {
            Self::Unix(facts) => facts.length,
            Self::Windows(facts, _) => facts.length,
        }
    }
}

#[derive(Clone)]
struct FakeSpec {
    bytes: Vec<u8>,
    facts: Facts,
    changed: bool,
}

struct FakeHandle {
    cursor: Cursor<Vec<u8>>,
    before: Facts,
    after: Facts,
    snapshots: Cell<u8>,
}

impl Read for FakeHandle {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.cursor.read(buffer)
    }
}

#[derive(Default)]
struct FakeFileSystem {
    files: HashMap<PathBuf, FakeSpec>,
    windows: bool,
}

fn unix_facts(length: usize, kind: MaterialKind, inode: u64) -> UnixFileFacts {
    UnixFileFacts {
        device: 1,
        inode,
        length: length as u64,
        modified_seconds: 1,
        modified_nanoseconds: 0,
        changed_seconds: 1,
        changed_nanoseconds: 0,
        mode: match kind {
            MaterialKind::Certificate => 0o100_644,
            MaterialKind::PrivateKey => 0o100_600,
        },
        owner: SERVICE_UID,
        regular: true,
        symlink: false,
    }
}

fn windows_facts(length: usize, identity: u128) -> WindowsFileFacts {
    WindowsFileFacts {
        identity,
        length: length as u64,
        modified: 1,
        regular: true,
        reparse: false,
        disk: true,
    }
}

impl FakeFileSystem {
    fn platform(windows: bool) -> Self {
        Self {
            windows,
            ..Self::default()
        }
    }

    fn add(&mut self, path: &str, bytes: Vec<u8>, kind: MaterialKind) {
        let index = self.files.len() as u64 + 1;
        let facts = if self.windows {
            Facts::Windows(windows_facts(bytes.len(), u128::from(index)), true)
        } else {
            Facts::Unix(unix_facts(bytes.len(), kind, index))
        };
        self.files.insert(
            PathBuf::from(path),
            FakeSpec {
                bytes,
                facts,
                changed: false,
            },
        );
    }

    fn set_facts(&mut self, path: &str, facts: Facts) {
        self.files.get_mut(Path::new(path)).expect("file").facts = facts;
    }
}

impl SecureFileSystem for FakeFileSystem {
    type Handle = FakeHandle;
    type Snapshot = Facts;

    fn open(&self, path: &Path, kind: MaterialKind) -> Result<Self::Handle, HostTlsError> {
        let spec = self
            .files
            .get(path)
            .ok_or(HostTlsError::Missing(kind.label()))?;
        let mut after = spec.facts.clone();
        if spec.changed {
            match &mut after {
                Facts::Unix(facts) => facts.changed_nanoseconds += 1,
                Facts::Windows(facts, _) => facts.modified += 1,
            }
        }
        Ok(FakeHandle {
            cursor: Cursor::new(spec.bytes.clone()),
            before: spec.facts.clone(),
            after,
            snapshots: Cell::new(0),
        })
    }

    fn snapshot(&self, handle: &Self::Handle, _kind: MaterialKind) -> Result<Facts, HostTlsError> {
        let count = handle.snapshots.get();
        handle.snapshots.set(count + 1);
        Ok(if count == 0 {
            handle.before.clone()
        } else {
            handle.after.clone()
        })
    }

    fn admit(
        &self,
        _handle: &Self::Handle,
        snapshot: &Facts,
        kind: MaterialKind,
    ) -> Result<(), HostTlsError> {
        match snapshot {
            Facts::Unix(facts) => facts.admit(kind, SERVICE_UID),
            Facts::Windows(facts, restricted_acl) => {
                facts.admit(kind)?;
                if kind == MaterialKind::PrivateKey && !restricted_acl {
                    return Err(HostTlsError::BroadPrivateKeyAcl);
                }
                Ok(())
            }
        }
    }
}

#[derive(Debug)]
struct TestClock(AtomicU64);

impl TestClock {
    fn new(now: u64) -> Self {
        Self(AtomicU64::new(now))
    }
}

impl UnixClock for TestClock {
    fn now_epoch_secs(&self) -> Result<u64, ClockError> {
        Ok(self.0.load(Ordering::SeqCst))
    }
}

fn issued(name: &str) -> (Vec<u8>, Vec<u8>) {
    let mut params = CertificateParams::new(vec![name.to_string()]).expect("params");
    params.not_before = rcgen::date_time_ymd(2020, 1, 1);
    params.not_after = rcgen::date_time_ymd(2040, 1, 1);
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let key = KeyPair::generate().expect("key");
    let certificate = params.self_signed(&key).expect("certificate");
    (
        certificate.pem().into_bytes(),
        key.serialize_pem().into_bytes(),
    )
}

fn source(expected_sans: Vec<String>) -> HostTlsSource {
    HostTlsSource {
        certificate_path: PathBuf::from("host.crt"),
        private_key_path: PathBuf::from("host.key"),
        expected_sans,
        posture: TlsPosture::new(
            TlsVersionFloor::default(),
            std::iter::empty::<RingCipherSuite>(),
        )
        .expect("posture"),
        key_policy: CertificateKeyPolicy::default(),
        time_policy: CertificateTimePolicy::default(),
    }
}

fn file_system_with(certificate: Vec<u8>, key: Vec<u8>, windows: bool) -> FakeFileSystem {
    let mut file_system = FakeFileSystem::platform(windows);
    file_system.add("host.crt", certificate, MaterialKind::Certificate);
    file_system.add("host.key", key, MaterialKind::PrivateKey);
    file_system
}

fn good_file_system() -> FakeFileSystem {
    let (certificate, key) = issued("host.example");
    file_system_with(certificate, key, false)
}

#[test]
fn reader_rejects_missing_partial_oversized_and_changed_material() {
    for windows in [false, true] {
        let empty = FakeFileSystem::platform(windows);
        assert_eq!(
            read_bounded(&empty, Path::new("missing.crt"), MaterialKind::Certificate),
            Err(HostTlsError::Missing("certificate"))
        );

        let mut partial = FakeFileSystem::platform(windows);
        partial.add(
            "host.crt",
            b"certificate".to_vec(),
            MaterialKind::Certificate,
        );
        assert_eq!(
            read_bounded(&partial, Path::new("host.key"), MaterialKind::PrivateKey),
            Err(HostTlsError::Missing("private key"))
        );

        let mut oversized = FakeFileSystem::platform(windows);
        oversized.add(
            "host.key",
            vec![0; usize::try_from(MAX_PRIVATE_KEY_PEM_BYTES).expect("size") + 1],
            MaterialKind::PrivateKey,
        );
        assert_eq!(
            read_bounded(&oversized, Path::new("host.key"), MaterialKind::PrivateKey),
            Err(HostTlsError::Oversized(
                "private key",
                MAX_PRIVATE_KEY_PEM_BYTES
            ))
        );

        let (certificate, key) = issued("host.example");
        let mut changed = file_system_with(certificate, key, windows);
        changed
            .files
            .get_mut(Path::new("host.key"))
            .expect("key")
            .changed = true;
        assert_eq!(
            read_bounded(&changed, Path::new("host.key"), MaterialKind::PrivateKey),
            Err(HostTlsError::ChangedDuringRead("private key"))
        );
    }
}

#[test]
fn unix_rule_rejects_symlink_nonregular_owner_and_modes() {
    let key = unix_facts(1, MaterialKind::PrivateKey, 1);
    for (facts, expected) in [
        (
            UnixFileFacts {
                symlink: true,
                ..key.clone()
            },
            HostTlsError::Symlink("private key"),
        ),
        (
            UnixFileFacts {
                regular: false,
                ..key.clone()
            },
            HostTlsError::NotRegular("private key"),
        ),
        (
            UnixFileFacts {
                owner: 2222,
                ..key.clone()
            },
            HostTlsError::UnexpectedOwner("private key"),
        ),
        (
            UnixFileFacts {
                mode: 0o100_640,
                ..key.clone()
            },
            HostTlsError::InsecureMode("private key"),
        ),
        (
            UnixFileFacts {
                mode: 0o104_600,
                ..key.clone()
            },
            HostTlsError::InsecureMode("private key"),
        ),
    ] {
        let mut file_system = FakeFileSystem::default();
        file_system.add("host.key", vec![1], MaterialKind::PrivateKey);
        file_system.set_facts("host.key", Facts::Unix(facts));
        assert_eq!(
            read_bounded(
                &file_system,
                Path::new("host.key"),
                MaterialKind::PrivateKey
            ),
            Err(expected)
        );
    }

    let root_owned = UnixFileFacts { owner: 0, ..key };
    assert_eq!(
        root_owned.admit(MaterialKind::PrivateKey, SERVICE_UID),
        Ok(())
    );

    let certificate = unix_facts(1, MaterialKind::Certificate, 2);
    for (mode, admitted) in [
        (0o100_644, true),
        (0o100_640, true),
        (0o100_400, true),
        (0o100_664, false),
        (0o100_646, false),
        (0o100_244, false),
    ] {
        assert_eq!(
            UnixFileFacts {
                mode,
                ..certificate.clone()
            }
            .admit(MaterialKind::Certificate, SERVICE_UID)
            .is_ok(),
            admitted,
            "certificate mode {mode:o}"
        );
    }
}

#[test]
fn windows_rule_rejects_directory_reparse_device_and_broad_acl() {
    let key = windows_facts(1, 1);
    for (facts, expected) in [
        (
            WindowsFileFacts {
                regular: false,
                ..key.clone()
            },
            HostTlsError::NotRegular("private key"),
        ),
        (
            WindowsFileFacts {
                reparse: true,
                ..key.clone()
            },
            HostTlsError::ReparsePoint("private key"),
        ),
        (
            WindowsFileFacts {
                disk: false,
                ..key.clone()
            },
            HostTlsError::NonDiskDevice("private key"),
        ),
    ] {
        let mut file_system = FakeFileSystem::platform(true);
        file_system.add("host.key", vec![1], MaterialKind::PrivateKey);
        file_system.set_facts("host.key", Facts::Windows(facts, true));
        assert_eq!(
            read_bounded(
                &file_system,
                Path::new("host.key"),
                MaterialKind::PrivateKey
            ),
            Err(expected)
        );
    }

    let mut broad = FakeFileSystem::platform(true);
    broad.add("host.key", vec![1], MaterialKind::PrivateKey);
    broad.set_facts("host.key", Facts::Windows(key.clone(), false));
    assert_eq!(
        read_bounded(&broad, Path::new("host.key"), MaterialKind::PrivateKey),
        Err(HostTlsError::BroadPrivateKeyAcl)
    );
    // The DACL rule is the key's: a certificate with a broad DACL is read.
    let mut certificate = FakeFileSystem::platform(true);
    certificate.add("host.crt", vec![1], MaterialKind::Certificate);
    certificate.set_facts("host.crt", Facts::Windows(key, false));
    assert!(
        read_bounded(
            &certificate,
            Path::new("host.crt"),
            MaterialKind::Certificate
        )
        .is_ok()
    );
}

#[test]
fn exactly_one_key_a_matching_pair_and_every_expected_san_are_required() {
    let (certificate, first_key) = issued("host.example");
    let (_, second_key) = issued("host.example");
    assert_eq!(
        parse_material(&certificate, &[first_key.clone(), second_key].concat()).map(|_| ()),
        Err(HostTlsError::MultiplePrivateKeys)
    );
    assert_eq!(
        parse_material(&certificate, b"").map(|_| ()),
        Err(HostTlsError::MissingPrivateKey)
    );
    assert_eq!(
        parse_material(b"", &first_key).map(|_| ()),
        Err(HostTlsError::EmptyCertificateChain)
    );
    assert_eq!(
        parse_material(
            &[certificate.clone(), first_key.clone()].concat(),
            &first_key
        )
        .map(|_| ()),
        Err(HostTlsError::MalformedCertificatePem)
    );
    assert_eq!(
        parse_material(
            &certificate,
            &[first_key.clone(), certificate.clone()].concat()
        )
        .map(|_| ()),
        Err(HostTlsError::MalformedPrivateKeyPem)
    );
    // Prose around the blocks, as an operator's editor may leave, is fine.
    let annotated = [b"Arcen Pier key\n".as_slice(), &first_key].concat();
    assert!(parse_material(&certificate, &annotated).is_ok());

    let mismatch = file_system_with(certificate, issued("other.example").1, false);
    assert_eq!(
        load_validated(
            &mismatch,
            &source(vec!["host.example".to_string()]),
            &TestClock::new(NOW)
        )
        .map(|_| ()),
        Err(HostTlsError::CertificateRejected)
    );
    assert_eq!(
        load_validated(
            &good_file_system(),
            &source(vec![
                "host.example".to_string(),
                "missing.example".to_string()
            ]),
            &TestClock::new(NOW)
        )
        .map(|_| ()),
        Err(HostTlsError::CertificateRejected)
    );
}

#[test]
fn lifecycle_fields_are_bounded_and_private() {
    let validated = load_validated(
        &good_file_system(),
        &source(vec!["host.example".to_string()]),
        &TestClock::new(NOW),
    )
    .expect("validated");
    let rendered = format!(
        "{:?}",
        certificate_fields("arcen-pier", validated.metadata(), Some(30))
    );
    assert!(rendered.contains("cert_sha256"));
    assert!(rendered.contains("spki_sha256"));
    assert!(rendered.contains("days_remaining"));
    assert!(!rendered.contains("host.example"));
    assert!(!rendered.contains("host.crt"));
    assert!(!rendered.contains("issuer"));
    assert!(!rendered.contains("subject"));
}

#[test]
fn startup_reports_active_or_expiring() {
    let clock = Arc::new(TestClock::new(NOW));
    let material = HostTlsMaterial::load(
        &good_file_system(),
        source(vec!["host.example".to_string()]),
        clock.clone(),
        "arcen-pier",
    )
    .expect("material");
    let event = material.startup_event().expect("event");
    assert_eq!(
        event.kind,
        arcen_telemetry::LifecycleEventKind::TlsCertificateActive
    );
    assert!(material.status_events().is_empty());

    let mut warning = source(vec!["host.example".to_string()]);
    warning.time_policy = CertificateTimePolicy {
        warning_window_secs: u64::MAX,
    };
    let expiring = HostTlsMaterial::load(&good_file_system(), warning, clock, "pier_broker")
        .expect("material");
    let event = expiring.startup_event().expect("event");
    assert_eq!(
        event.kind,
        arcen_telemetry::LifecycleEventKind::TlsCertificateExpiring
    );
    assert!(format!("{:?}", event.fields).contains("pier_broker"));
    // Start-up already said "expiring": the status check does not repeat it.
    assert!(expiring.status_events().is_empty());
}

#[test]
fn a_malformed_expired_or_half_written_replacement_keeps_the_last_good_material() {
    let clock = Arc::new(TestClock::new(NOW));
    let (certificate, key) = issued("host.example");
    let material = HostTlsMaterial::load(
        &file_system_with(certificate.clone(), key.clone(), false),
        source(vec!["host.example".to_string()]),
        clock,
        "arcen-pier",
    )
    .expect("material");
    let last_good = material.metadata().expect("metadata").cert_sha256;

    let mut expired_params = CertificateParams::new(vec!["host.example".to_owned()]).expect("p");
    expired_params.not_before = rcgen::date_time_ymd(2020, 1, 1);
    expired_params.not_after = rcgen::date_time_ymd(2021, 1, 1);
    expired_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let key_pair = KeyPair::from_pem(std::str::from_utf8(&key).expect("pem")).expect("key");
    let expired = expired_params
        .self_signed(&key_pair)
        .expect("expired")
        .pem()
        .into_bytes();
    let half = certificate[..certificate.len() / 2].to_vec();
    let (new_certificate, _) = issued("host.example");

    for (label, replacement_certificate, replacement_key, reason) in [
        (
            "malformed",
            b"not a certificate".to_vec(),
            key.clone(),
            "pem_invalid",
        ),
        ("expired", expired, key.clone(), "certificate_rejected"),
        ("half-written certificate", half, key.clone(), "pem_invalid"),
        (
            "certificate replaced, key not yet",
            new_certificate,
            key.clone(),
            "certificate_rejected",
        ),
        ("empty key", certificate.clone(), Vec::new(), "pem_invalid"),
    ] {
        let failed = material.reload(&file_system_with(
            replacement_certificate,
            replacement_key,
            false,
        ));
        assert!(failed.result.is_err(), "{label} must be refused");
        assert_eq!(
            failed
                .events
                .iter()
                .map(|event| event.kind)
                .collect::<Vec<_>>(),
            vec![arcen_telemetry::LifecycleEventKind::TlsCertificateReloadFailed],
            "{label}"
        );
        assert!(
            format!("{:?}", failed.events[0].fields).contains(reason),
            "{label}: {:?}",
            failed.events[0].fields
        );
        assert_eq!(
            material.metadata().expect("metadata").cert_sha256,
            last_good,
            "{label}: the last good certificate keeps serving"
        );
        assert!(material.resolver().resolve_current().is_some(), "{label}");
    }
}

#[test]
fn reload_keeps_the_last_good_material_and_expiry_refuses_new_handshakes() {
    let clock = Arc::new(TestClock::new(NOW));
    let material = HostTlsMaterial::load(
        &good_file_system(),
        source(vec!["host.example".to_string()]),
        clock.clone(),
        "arcen-pier",
    )
    .expect("material");
    let metadata = material.metadata().expect("metadata");
    assert_eq!(
        material.host_identity().expect("identity").as_str(),
        format!(
            "spki-sha256:{}",
            crate::cert_marker::hex_lower(&metadata.spki_sha256)
        )
    );
    let last_good = metadata.cert_sha256;

    let mut mismatch = good_file_system();
    mismatch
        .files
        .get_mut(Path::new("host.key"))
        .expect("key")
        .bytes = issued("other.example").1;
    let failed = material.reload(&mismatch);
    assert_eq!(failed.result, Err(HostTlsError::CertificateRejected));
    assert_eq!(failed.events.len(), 1);
    assert_eq!(
        failed.events[0].kind,
        arcen_telemetry::LifecycleEventKind::TlsCertificateReloadFailed
    );
    assert!(format!("{:?}", failed.events[0].fields).contains("certificate_rejected"));
    assert_eq!(
        material.metadata().expect("metadata").cert_sha256,
        last_good
    );

    let reloaded = material.reload(&good_file_system());
    assert_eq!(reloaded.result, Ok(()));
    assert_eq!(
        reloaded.events[0].kind,
        arcen_telemetry::LifecycleEventKind::TlsCertificateReloaded
    );
    assert_ne!(
        material.metadata().expect("metadata").cert_sha256,
        last_good
    );

    clock.0.store(3_000_000_000, Ordering::SeqCst);
    assert!(material.resolver().resolve_current().is_none());
    let expired = material.status_events();
    assert_eq!(
        expired.iter().map(|event| event.kind).collect::<Vec<_>>(),
        vec![arcen_telemetry::LifecycleEventKind::TlsCertificateExpired]
    );
    assert!(material.status_events().is_empty(), "expired is said once");
}

// --- Material deployed Piers hold today ------------------------------------

/// The certificate profile `new-host-cert.sh` and the Linux Pier's
/// `new-host-cert` issue through `openssl req -x509`.
fn openssl_profile_certificate(key: &KeyPair, names: &[&str]) -> Vec<u8> {
    let mut params = CertificateParams::new(
        names
            .iter()
            .map(|name| (*name).to_owned())
            .collect::<Vec<_>>(),
    )
    .expect("params");
    params.distinguished_name = rcgen::DistinguishedName::new();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "Arcen Pier");
    params.is_ca = rcgen::IsCa::ExplicitNoCa;
    params.key_usages = vec![rcgen::KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    params.not_before = rcgen::date_time_ymd(2026, 3, 1);
    params.not_after = rcgen::date_time_ymd(2028, 6, 3);
    params
        .self_signed(key)
        .expect("certificate")
        .pem()
        .into_bytes()
}

/// The pin files the issuer wrote beside `certificate`: what Decks paired
/// against.
fn written_pins(certificate: &[u8]) -> (String, String) {
    let files = crate::cert_store::PinFiles::for_certificate(certificate).expect("pin files");
    (files.certificate_pin, files.spki_pin)
}

fn assert_serves_the_pinned_identity(certificate: &[u8], key: &[u8], sans: &[&str]) {
    let (certificate_pin, spki_pin) = written_pins(certificate);
    for windows in [false, true] {
        let material = HostTlsMaterial::load(
            &file_system_with(certificate.to_vec(), key.to_vec(), windows),
            source(sans.iter().map(|name| (*name).to_owned()).collect()),
            Arc::new(TestClock::new(1_790_000_000)),
            "arcen-pier",
        )
        .expect("deployed material loads");
        let metadata = material.metadata().expect("metadata");
        assert_eq!(
            crate::cert_marker::normalize_certificate_pin(&certificate_pin),
            Some(crate::cert_marker::hex_lower(&metadata.cert_sha256)),
            "certificate pin file still matches what is served"
        );
        assert_eq!(
            crate::cert_marker::normalize_spki_pin(&spki_pin),
            Some(format!(
                "sha256/{}",
                crate::cert_marker::base64_encode(&metadata.spki_sha256)
            )),
            "SPKI pin file still matches what is served"
        );
        assert!(material.resolver().resolve_current().is_some());
    }
}

#[test]
fn linux_openssl_material_keeps_its_pins() {
    let key = KeyPair::generate().expect("key");
    let certificate = openssl_profile_certificate(&key, &["pier.example.internal", "203.0.113.7"]);
    let sec1 = openssl_sec1_key(&key);
    assert!(sec1.starts_with(format!("-----BEGIN {}-----", sec1_label()).as_bytes()));
    let (_, parsed) = parse_material(&certificate, &sec1).expect("SEC1 key parses");
    assert!(matches!(parsed, PrivateKeyDer::Sec1(_)));
    assert_serves_the_pinned_identity(&certificate, &sec1, &["203.0.113.7"]);
}

#[test]
fn rcgen_material_from_the_macos_pier_and_windows_installer_keeps_its_pins() {
    let (certificate, key) = issued("pier.example.internal");
    let (_, parsed) = parse_material(&certificate, &key).expect("PKCS #8 key parses");
    assert!(matches!(parsed, PrivateKeyDer::Pkcs8(_)));
    assert_serves_the_pinned_identity(&certificate, &key, &["pier.example.internal"]);
}

#[test]
fn a_renewal_over_the_same_key_keeps_the_host_identity() {
    let key = KeyPair::generate().expect("key");
    let first = openssl_profile_certificate(&key, &["pier.example.internal"]);
    let mut params =
        CertificateParams::new(vec!["pier.example.internal".to_owned()]).expect("params");
    params.not_before = rcgen::date_time_ymd(2026, 6, 1);
    params.not_after = rcgen::date_time_ymd(2028, 9, 3);
    let renewed = params
        .self_signed(&key)
        .expect("renewed")
        .pem()
        .into_bytes();
    let sec1 = openssl_sec1_key(&key);
    let clock: Arc<dyn UnixClock> = Arc::new(TestClock::new(1_790_000_000));
    let identity = |certificate: &[u8]| {
        HostTlsMaterial::load(
            &file_system_with(certificate.to_vec(), sec1.clone(), false),
            source(Vec::new()),
            Arc::clone(&clock),
            "arcen-pier",
        )
        .expect("material")
        .host_identity()
        .expect("identity")
    };
    assert_eq!(identity(&first), identity(&renewed));
    assert_ne!(written_pins(&first).0, written_pins(&renewed).0);
    assert_eq!(written_pins(&first).1, written_pins(&renewed).1);
}
