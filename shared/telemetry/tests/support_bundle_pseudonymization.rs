// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS

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
    // A host and a peer address are both a machine: one rule, one pseudonym
    // for the same machine in either place.
    assert_eq!(value["peer_addr"], value["host"]);
    assert!(pseudonyms[1].ends_with("[name]"));
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
    assert_eq!(
        report.redacted_key_paths,
        ["/fields/ssid", "/host", "/peer_addr", "/user"]
    );
}

#[test]
fn identities_products_log_inside_fields_are_replaced_by_their_class() {
    // Found in live Pier bundles: the Linux Pier logs a sign-in's
    // `fields.username` and a peer's `fields.peer`, and its bind address as
    // `fields.addr`.
    let mut record: serde_json::Value = serde_json::from_slice(&canonical_line(
        "artist",
        "pier-01",
        "192.0.2.10:54000",
        "studio",
    ))
    .expect("fixture");
    let fields = record["fields"].as_object_mut().expect("fields");
    for (field, _) in arcen_telemetry::BUNDLE_IDENTITY_FIELDS {
        fields.insert((*field).to_owned(), "artist@192.0.2.10:54000".into());
    }
    fields.insert("frames".to_owned(), 42.into());
    fields.insert("socket".to_owned(), "/run/arcen/usb.sock".into());
    let mut input = serde_json::to_vec(&record).expect("line");
    input.push(b'\n');
    let (output, report) = transform([0x55; 32], &input);
    let value: serde_json::Value = serde_json::from_slice(&output).expect("output JSONL");
    assert!(!String::from_utf8_lossy(&output).contains("192.0.2.10"));
    for (field, kind) in arcen_telemetry::BUNDLE_IDENTITY_FIELDS {
        let pseudonym = BundlePseudonymizer::new(BundlePseudonymKey::from_bytes([0x55; 32]))
            .pseudonymize_identity(*kind, "artist@192.0.2.10:54000");
        assert_eq!(value["fields"][field], pseudonym.as_str(), "{field}");
        assert!(
            report
                .redacted_key_paths
                .contains(&format!("/fields/{field}")),
            "{field}"
        );
    }
    // Non-identity fields are kept as they were.
    assert_eq!(value["fields"]["frames"], 42);
    assert_eq!(value["fields"]["socket"], "/run/arcen/usb.sock");
    assert_eq!(
        report.redacted_key_paths.len(),
        3 + arcen_telemetry::BUNDLE_IDENTITY_FIELDS.len()
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

#[test]
fn identity_shaped_field_names_are_replaced_too() {
    // Found by review: the Windows Pier logs `windows_user`,
    // `windows_domain`, `requested_account`, `listener_addr` and
    // `interface_addresses`; the Deck logs `uri`.
    let mut record: serde_json::Value = serde_json::from_slice(&canonical_line(
        "artist",
        "pier-01",
        "192.0.2.10:54000",
        "studio",
    ))
    .expect("fixture");
    let fields = record["fields"].as_object_mut().expect("fields");
    for (field, value) in [
        ("windows_user", "kari"),
        ("windows_domain", "CONTOSO"),
        ("requested_account", "kari@contoso.example"),
        ("listener_addr", "192.0.2.7:18444"),
        ("interface_addresses", "192.0.2.7,198.51.100.4"),
        ("uri", "quic://render07.example.internal:18444"),
    ] {
        fields.insert(field.to_owned(), value.into());
    }
    fields.insert("changed_host".to_owned(), true.into());
    fields.insert("slice_segment_address".to_owned(), 7.into());
    let mut input = serde_json::to_vec(&record).expect("line");
    input.push(b'\n');
    let (output, report) = transform([0x77; 32], &input);
    let text = String::from_utf8_lossy(&output);
    for planted in [
        "kari",
        "CONTOSO",
        "contoso",
        "192.0.2.7",
        "198.51.100",
        "render07",
    ] {
        assert!(!text.contains(planted), "{planted} survived: {text}");
    }
    let value: serde_json::Value = serde_json::from_slice(&output).expect("output");
    assert_eq!(value["fields"]["changed_host"], true);
    assert_eq!(value["fields"]["slice_segment_address"], 7);
    assert!(
        report
            .redacted_key_paths
            .contains(&"/fields/windows_user".to_owned())
    );
    assert_eq!(
        arcen_telemetry::bundle_identity_field("frames"),
        None,
        "an ordinary field is kept"
    );
}

#[test]
fn ip_literals_in_free_text_are_replaced_and_paths_and_times_are_not() {
    // Found in a live Mac Deck bundle: a dependency's error message naming
    // the Pier (`destination: <ip>:18444`) and `fields.resolved`.
    let pseudonymizer = BundlePseudonymizer::new(BundlePseudonymKey::from_bytes([0x88; 32]));
    let message = "sendmsg error: Os { code: 53 }, Transmit: { destination: 192.0.2.82:18444, \
                   src_ip: None }, via [2001:db8::7]:443 and fe80::1.";
    let scrubbed = pseudonymizer
        .pseudonymize_ip_literals(message)
        .expect("replaced");
    assert!(
        !scrubbed.contains("192.0.2")
            && !scrubbed.contains("2001:db8")
            && !scrubbed.contains("fe80")
    );
    assert!(scrubbed.contains(&format!(
        "destination: {}, ",
        pseudonymizer.pseudonymize_address("192.0.2.82:18444")
    )));
    // Shapes a dependency's `{:?}` and terse log text produce.
    for (text, address) in [
        ("connecting to [203.0.113.5:443]", "203.0.113.5:443"),
        (
            "addrs=[203.0.113.5:443, 198.51.100.7:443]",
            "203.0.113.5:443",
        ),
        (
            "addrs=[203.0.113.5:443, 198.51.100.7:443]",
            "198.51.100.7:443",
        ),
        ("peer:203.0.113.5:443", "203.0.113.5:443"),
        ("Address:2001:db8::5 failed", "2001:db8::5"),
        ("to [::1]:80.", "[::1]:80"),
    ] {
        let scrubbed = pseudonymizer.pseudonymize_ip_literals(text).expect(text);
        assert!(
            scrubbed.contains(&pseudonymizer.pseudonymize_address(address)),
            "{text} -> {scrubbed}"
        );
        assert!(
            !scrubbed.contains("203.0.113")
                && !scrubbed.contains("198.51.100")
                && !scrubbed.contains("db8"),
            "{scrubbed}"
        );
    }
    for kept in [
        "arcen::net::quic at 2026-07-24T16:00:00.000000Z",
        "version 0.19.1, sha256 deadbeef, 16:00:00",
        "Option::None and abc::def",
    ] {
        assert_eq!(pseudonymizer.pseudonymize_ip_literals(kept), None, "{kept}");
    }
    let mut record: serde_json::Value = serde_json::from_slice(&canonical_line(
        "artist",
        "pier-01",
        "192.0.2.10:54000",
        "studio",
    ))
    .expect("fixture");
    record["message"] = message.into();
    record["fields"]
        .as_object_mut()
        .expect("fields")
        .insert("resolved".to_owned(), "192.0.2.82".into());
    let mut input = serde_json::to_vec(&record).expect("line");
    input.push(b'\n');
    let (output, report) = transform([0x88; 32], &input);
    assert!(!String::from_utf8_lossy(&output).contains("192.0.2.82"));
    assert!(report.redacted_key_paths.contains(&"/message".to_owned()));
    assert!(
        report
            .redacted_key_paths
            .contains(&"/fields/resolved".to_owned())
    );
}

#[test]
fn an_address_keeps_its_family_scope_and_port_and_its_host_correlates() {
    let pseudonymizer = BundlePseudonymizer::new(BundlePseudonymKey::from_bytes([0x66; 32]));
    let host = |value: &str| pseudonymizer.pseudonymize(BundleIdentityKind::Host, value);
    for (value, expected) in [
        (
            "203.0.113.82:18444",
            format!("{}[ipv4-public]:18444", host("203.0.113.82")),
        ),
        (
            "203.0.113.82",
            format!("{}[ipv4-public]", host("203.0.113.82")),
        ),
        (
            "10.20.30.40:53710",
            format!("{}[ipv4-private]:53710", host("10.20.30.40")),
        ),
        (
            "127.0.0.1:1",
            format!("{}[ipv4-loopback]:1", host("127.0.0.1")),
        ),
        (
            "0.0.0.0:18444",
            format!("{}[ipv4-unspecified]:18444", host("0.0.0.0")),
        ),
        ("100.64.1.2", format!("{}[ipv4-shared]", host("100.64.1.2"))),
        (
            "[fd00::1]:18444",
            format!("{}[ipv6-ula]:18444", host("fd00::1")),
        ),
        ("fe80::1", format!("{}[ipv6-link-local]", host("fe80::1"))),
        (
            "[2001:db8::5]:443",
            format!("{}[ipv6-public]:443", host("2001:db8::5")),
        ),
        (
            "::ffff:10.0.0.1",
            format!("{}[ipv4-private]", host("::ffff:10.0.0.1")),
        ),
        (
            "render07.example.internal:18444",
            format!("{}[name]:18444", host("render07.example.internal")),
        ),
        ("render07", format!("{}[name]", host("render07"))),
        ("host:notaport", format!("{}[name]", host("host:notaport"))),
    ] {
        let pseudonym = pseudonymizer.pseudonymize_address(value);
        assert_eq!(pseudonym, expected, "{value}");
        assert!(!pseudonym.contains("203.0.113") && !pseudonym.contains("render07"));
    }
    let id = pseudonymizer.bundle_id();
    assert_eq!(id.len(), 13);
    assert!(id.starts_with("ARC-") && id.as_bytes()[8] == b'-');
    assert_ne!(
        id,
        BundlePseudonymizer::new(BundlePseudonymKey::from_bytes([0x67; 32])).bundle_id()
    );
}

#[test]
fn a_home_folder_keeps_its_path_and_loses_its_account_on_every_os() {
    let pseudonymizer = BundlePseudonymizer::new(BundlePseudonymKey::from_bytes([0x89; 32]));
    let user = |name: &str| pseudonymizer.pseudonymize(BundleIdentityKind::User, name);
    for (text, expected) in [
        (
            r"log folder C:\Users\alice\AppData\Local\Arcen\Deck\Logs",
            format!(
                r"log folder C:\Users\{}\AppData\Local\Arcen\Deck\Logs",
                user("alice")
            ),
        ),
        (
            "settings c:/users/alice/AppData/Roaming/Arcen/Deck/settings.json",
            format!(
                "settings c:/users/{}/AppData/Roaming/Arcen/Deck/settings.json",
                user("alice")
            ),
        ),
        (
            r#"{"dir":"C:\\Users\\alice\\AppData"}"#,
            format!(r#"{{"dir":"C:\\Users\\{}\\AppData"}}"#, user("alice")),
        ),
        (
            r"D:\Users\alice smith\Desktop and C:\Documents and Settings\alice\x",
            format!(
                r"D:\Users\{}\Desktop and C:\Documents and Settings\{}\x",
                user("alice smith"),
                user("alice")
            ),
        ),
        (
            "/Users/alice/Library/Logs/Arcen and /home/alice/.config/arcen",
            format!(
                "/Users/{0}/Library/Logs/Arcen and /home/{0}/.config/arcen",
                user("alice")
            ),
        ),
        (
            "cannot open /home/alice: denied",
            format!("cannot open /home/{}: denied", user("alice")),
        ),
    ] {
        assert_eq!(
            pseudonymizer.pseudonymize_home_paths(text).as_deref(),
            Some(expected.as_str()),
            "{text}"
        );
    }
    for kept in [
        r"C:\Users\Public\Documents, C:\Users\Default\x, C:\Users\All Users\y",
        "/Users/Shared/Arcen, ~/Library/Logs/Arcen, C:/Users/<user>/AppData",
        r"%LOCALAPPDATA%\Arcen\Deck\Logs, C:\Users\%USERNAME%, C:\ProgramData\Arcen",
        "https://example.com/users/x and arcen::home::thing",
    ] {
        assert_eq!(pseudonymizer.pseudonymize_home_paths(kept), None, "{kept}");
    }
    let once = pseudonymizer
        .pseudonymize_home_paths(r"C:\Users\alice\x")
        .expect("replaced");
    assert_eq!(pseudonymizer.pseudonymize_home_paths(&once), None);

    let mut record: serde_json::Value = serde_json::from_slice(&canonical_line(
        "alice",
        "pier-01",
        "192.0.2.10:54000",
        "studio",
    ))
    .expect("fixture");
    record["message"] = r"could not write C:\Users\alice\AppData\Local\Arcen\Deck\Logs".into();
    record["fields"].as_object_mut().expect("fields").insert(
        "path".to_owned(),
        "C:/Users/alice/AppData/Roaming/Arcen/Deck".into(),
    );
    let mut input = serde_json::to_vec(&record).expect("line");
    input.push(b'\n');
    let (output, report) = transform([0x89; 32], &input);
    let text = String::from_utf8(output).expect("utf-8");
    assert!(!text.contains("alice"), "{text}");
    let line: serde_json::Value = serde_json::from_str(text.trim()).expect("record");
    assert_eq!(
        line["user"],
        user("alice"),
        "the record's user and its home folder correlate"
    );
    assert_eq!(
        line["fields"]["path"],
        format!("C:/Users/{}/AppData/Roaming/Arcen/Deck", user("alice"))
    );
    for path in ["/message", "/fields/path"] {
        assert!(
            report.redacted_key_paths.contains(&path.to_owned()),
            "{path}"
        );
    }
}
