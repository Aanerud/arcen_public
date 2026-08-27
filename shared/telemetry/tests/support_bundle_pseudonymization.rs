#![allow(clippy::expect_used)]

use std::io::{Cursor, Read};

use arcen_telemetry::{
    BundleIdentityKind, BundlePseudonymKey, BundlePseudonymizer, CanonicalJsonlTransformLimits,
    MAX_CANONICAL_JSON_LINE_BYTES, transform_canonical_jsonl,
};

fn canonical_line(user: &str, host: &str, peer: &str, ssid: &str) -> Vec<u8> {
    let mut line = serde_json::to_vec(&serde_json::json!({
        "schema_version": 1,
        "timestamp": "2026-07-24T16:00:00.000000Z",
        "sequence": 42,
        "profile_level": 0,
        "profile_name": "critical",
        "severity": "info",
        "role": "host",
        "component": "pier",
        "platform": "linux",
        "target": "arcen::session",
        "sid": "canonical-session-correlation-id",
        "user": user,
        "host": host,
        "peer_addr": peer,
        "health_state": "ok",
        "message": "session authentication succeeded",
        "fields": {"ssid": ssid}
    }))
    .expect("fixture serialization");
    line.push(b'\n');
    line
}

fn transform(
    key: [u8; 32],
    input: &[u8],
) -> (Vec<u8>, arcen_telemetry::CanonicalJsonlTransformReport) {
    let mut output = Vec::new();
    let report = transform_canonical_jsonl(
        Cursor::new(input),
        &mut output,
        &BundlePseudonymizer::new(BundlePseudonymKey::from_bytes(key)),
        CanonicalJsonlTransformLimits {
            max_input_bytes: input.len() as u64,
            max_output_bytes: u64::MAX,
            discard_initial_fragment: false,
        },
    )
    .expect("transform");
    (output, report)
}

#[test]
fn same_key_correlates_across_lines_and_different_keys_are_unlinkable() {
    let first = canonical_line("artist", "pier-01", "192.0.2.10:54000", "studio");
    let mut two_lines = first.clone();
    two_lines.extend_from_slice(&first);
    let (same_bundle, report) = transform([0x11; 32], &two_lines);
    let lines = same_bundle
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice::<serde_json::Value>(line).expect("output JSON"))
        .collect::<Vec<_>>();
    assert_eq!(report.accepted_lines, 2);
    assert_eq!(lines[0]["user"], lines[1]["user"]);
    assert_eq!(lines[0]["fields"]["ssid"], lines[1]["fields"]["ssid"]);

    let (other_bundle, _) = transform([0x22; 32], &first);
    let other: serde_json::Value = serde_json::from_slice(&other_bundle).expect("output JSONL");
    assert_ne!(lines[0]["user"], other["user"]);
    assert_ne!(lines[0]["fields"]["ssid"], other["fields"]["ssid"]);
}

#[test]
fn domains_separate_equal_values_and_all_identity_locations_are_replaced() {
    let raw = "same-identity";
    let input = canonical_line(raw, raw, raw, raw);
    let (output, report) = transform([0x33; 32], &input);
    let value: serde_json::Value = serde_json::from_slice(&output).expect("output JSONL");
    let pseudonyms = [
        value["user"].as_str().expect("user"),
        value["host"].as_str().expect("host"),
        value["peer_addr"].as_str().expect("peer"),
        value["fields"]["ssid"].as_str().expect("ssid"),
    ];
    assert!(pseudonyms.iter().all(|value| value.starts_with("anon:")));
    assert_eq!(
        pseudonyms[0],
        "anon:df9b259294a8116c5659b9d23b2987da7b4b12523b5607e0144c17bfae21d3c5"
    );
    assert!(
        pseudonyms
            .iter()
            .enumerate()
            .all(|(index, value)| pseudonyms[..index].iter().all(|other| other != value))
    );
    assert!(!String::from_utf8(output).expect("UTF-8").contains(raw));
    assert_eq!(
        report.redacted_kinds,
        vec![
            BundleIdentityKind::User,
            BundleIdentityKind::Host,
            BundleIdentityKind::PeerAddress,
            BundleIdentityKind::NetworkIdentity,
        ]
    );
}

#[test]
fn malformed_oversized_and_incomplete_records_are_omitted_without_raw_fallback() {
    let valid = canonical_line("artist", "pier-01", "192.0.2.1", "studio");
    let mut input = b"legacy plaintext artist\n{\"schema_version\":2}\n".to_vec();
    input.extend(std::iter::repeat_n(b'x', MAX_CANONICAL_JSON_LINE_BYTES + 1));
    input.push(b'\n');
    input.extend_from_slice(&valid);
    input.extend_from_slice(b"{\"schema_version\":1,\"user\":\"unfinished artist\"");

    let (output, report) = transform([0x44; 32], &input);
    assert_eq!(report.accepted_lines, 1);
    assert_eq!(report.invalid_lines, 2);
    assert_eq!(report.oversized_lines, 1);
    assert_eq!(report.incomplete_lines, 1);
    let rendered = String::from_utf8(output).expect("UTF-8");
    assert!(!rendered.contains("artist"));
}

struct TinyChunks<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl Read for TinyChunks<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let count = buffer
            .len()
            .min(3)
            .min(self.bytes.len().saturating_sub(self.offset));
        buffer[..count].copy_from_slice(&self.bytes[self.offset..self.offset + count]);
        self.offset += count;
        Ok(count)
    }
}

#[test]
fn line_streaming_survives_arbitrary_read_boundaries_and_never_exposes_key() {
    let key = [0xab; 32];
    let input = canonical_line("artist", "pier-01", "192.0.2.1", "studio");
    let pseudonymizer = BundlePseudonymizer::new(BundlePseudonymKey::from_bytes(key));
    assert_eq!(
        format!("{pseudonymizer:?}"),
        "BundlePseudonymizer(<redacted>)"
    );
    let mut output = Vec::new();
    let report = transform_canonical_jsonl(
        TinyChunks {
            bytes: &input,
            offset: 0,
        },
        &mut output,
        &pseudonymizer,
        CanonicalJsonlTransformLimits {
            max_input_bytes: input.len() as u64,
            max_output_bytes: u64::MAX,
            discard_initial_fragment: false,
        },
    )
    .expect("stream transform");
    assert_eq!(report.accepted_lines, 1);
    let rendered = String::from_utf8(output).expect("UTF-8");
    assert!(!rendered.contains(&"ab".repeat(32)));
    assert!(!rendered.contains("artist"));
}
