#![allow(clippy::expect_used)]

use arcen_telemetry::{
    BundleComponent, BundleEntry, BundleNotice, BundlePath, BundleSource, BundleTruncation,
    NoticeCode, NoticeKind, REDACTED_VALUE, RedactionDecision, RedactionReason,
    SUPPORT_BUNDLE_SCHEMA_VERSION, Sha256Digest, SupportBundleContractError,
    SupportBundleManifestBuilder, SupportBundleRedactionPolicy, TruncationReason,
    redact_json_document_at,
};

fn path(value: &str) -> BundlePath {
    BundlePath::new(value).expect("valid fixture path")
}

#[test]
fn secret_keys_are_classified_without_weakening_benign_keys() {
    for key in [
        "tls-key",
        "Private_Key",
        "ACCESS_TOKEN",
        "password",
        "clientCredential",
        "authorization",
        "session-cookie",
        "passphrase",
        "XAUTHORITY",
    ] {
        assert_eq!(
            SupportBundleRedactionPolicy::classify_key(key),
            RedactionDecision::Redact(RedactionReason::SensitiveKey)
        );
    }
    for key in ["listen", "port", "adapter", "retention_days"] {
        assert_eq!(
            SupportBundleRedactionPolicy::classify_key(key),
            RedactionDecision::Keep
        );
    }
}

#[test]
fn nested_json_redaction_is_stable_and_forgets_original_values() {
    let entry = path("config/pier.json");
    let mut document = serde_json::json!({
        "listen": {"port": 18444},
        "tls": {"key": "do-not-retain", "cert": "public.pem"},
        "nested": [{"AccessToken": "also-forget"}]
    });
    let records = redact_json_document_at(&entry, &mut document).expect("redaction");
    assert_eq!(document["tls"]["key"], REDACTED_VALUE);
    assert_eq!(document["nested"][0]["AccessToken"], REDACTED_VALUE);
    assert_eq!(document["tls"]["cert"], "public.pem");
    assert_eq!(
        records
            .iter()
            .map(|record| record.key_path.as_str())
            .collect::<Vec<_>>(),
        ["/nested/0/AccessToken", "/tls/key"]
    );
    let rendered = serde_json::to_string(&document).expect("render");
    assert!(!rendered.contains("do-not-retain"));
    assert!(!rendered.contains("also-forget"));
}

#[test]
fn bundle_paths_reject_absolute_traversal_and_noncanonical_forms() {
    for invalid in [
        "",
        "/absolute",
        r"C:\absolute",
        r"\\server\share",
        r"logs\pier.log",
        "logs//pier.log",
        "./pier.log",
        "../pier.log",
        "logs/../pier.log",
        "logs/\npier.log",
    ] {
        assert_eq!(
            BundlePath::new(invalid),
            Err(SupportBundleContractError::InvalidBundlePath)
        );
    }
    assert_eq!(
        path("logs/archive/pier.log").as_str(),
        "logs/archive/pier.log"
    );
}

#[test]
fn sha256_is_strict_lowercase_hex() {
    let text = "11".repeat(32);
    let digest = Sha256Digest::parse(&text).expect("digest");
    assert_eq!(digest, Sha256Digest::from_bytes([0x11; 32]));
    assert_eq!(digest.to_string(), text);
    assert!(Sha256Digest::parse(&"AA".repeat(32)).is_err());
    assert!(Sha256Digest::parse("11").is_err());
}

#[test]
fn manifest_is_sorted_bounded_and_never_indexes_itself() {
    let mut builder = SupportBundleManifestBuilder::new(
        BundleComponent {
            name: "arcen-pier".to_string(),
            version: "0.1.0".to_string(),
            os: "windows".to_string(),
            arch: "x86_64".to_string(),
        },
        1_700_000_000,
    );
    builder
        .add_entry(BundleEntry {
            path: path("logs/z.log"),
            source: BundleSource::Log,
            original_size_bytes: 10,
            included_size_bytes: 5,
            sha256: Sha256Digest::from_bytes([0x22; 32]),
            truncation: Some(BundleTruncation {
                original_offset: 5,
                reason: TruncationReason::PerSourceLimit,
            }),
        })
        .expect("entry");
    builder
        .add_entry(BundleEntry {
            path: path("config/pier.json"),
            source: BundleSource::Configuration,
            original_size_bytes: 4,
            included_size_bytes: 4,
            sha256: Sha256Digest::from_bytes([0x11; 32]),
            truncation: None,
        })
        .expect("entry");
    builder
        .add_notice(BundleNotice {
            source: path("config/tls/key"),
            kind: NoticeKind::Omitted,
            code: NoticeCode::PrivateKeyExcluded,
        })
        .expect("notice");
    assert_eq!(
        builder.add_entry(BundleEntry {
            path: path("manifest.json"),
            source: BundleSource::Diagnostics,
            original_size_bytes: 0,
            included_size_bytes: 0,
            sha256: Sha256Digest::from_bytes([0; 32]),
            truncation: None,
        }),
        Err(SupportBundleContractError::ManifestCannotIndexItself)
    );

    let manifest = builder.finish();
    assert_eq!(manifest.schema_version, SUPPORT_BUNDLE_SCHEMA_VERSION);
    assert_eq!(manifest.entries[0].path.as_str(), "config/pier.json");
    assert!(
        manifest
            .entries
            .iter()
            .all(|entry| entry.path.as_str() != "manifest.json")
    );
}

#[test]
fn manifest_v1_matches_the_golden_fixture_and_round_trips() {
    let mut builder = SupportBundleManifestBuilder::new(
        BundleComponent {
            name: "arcen-pier".to_string(),
            version: "0.1.0".to_string(),
            os: "linux".to_string(),
            arch: "x86_64".to_string(),
        },
        1_700_000_000,
    );
    builder
        .add_entry(BundleEntry {
            path: path("diagnostics/system.json"),
            source: BundleSource::Diagnostics,
            original_size_bytes: 2,
            included_size_bytes: 2,
            sha256: Sha256Digest::from_bytes([0x11; 32]),
            truncation: None,
        })
        .expect("entry");
    builder
        .add_notice(BundleNotice {
            source: path("config/tls/key"),
            kind: NoticeKind::Omitted,
            code: NoticeCode::PrivateKeyExcluded,
        })
        .expect("notice");
    let manifest = builder.finish();
    let expected: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/manifest-v1.json")).expect("fixture");
    let actual = serde_json::to_value(&manifest).expect("manifest");
    assert_eq!(actual, expected);
    let round_trip = serde_json::from_value(actual).expect("round trip");
    assert_eq!(manifest, round_trip);
}
