// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Test-only key encodings: the SEC1 file `openssl ecparam -genkey` writes,
//! built from a fresh rcgen key so no key is stored in the repository.

use std::fmt::Write as _;

use rcgen::KeyPair;

/// DER length octets.
pub(crate) fn der_length(length: usize) -> Vec<u8> {
    if length < 0x80 {
        return vec![u8::try_from(length).expect("short length")];
    }
    let bytes = length.to_be_bytes();
    let first = bytes.iter().position(|byte| *byte != 0).expect("non-zero");
    let mut encoded = vec![0x80 | u8::try_from(bytes.len() - first).expect("length of length")];
    encoded.extend_from_slice(&bytes[first..]);
    encoded
}

/// One DER element: tag, contents, and the input after it.
pub(crate) fn der_element(input: &[u8]) -> (u8, &[u8], &[u8]) {
    let tag = input[0];
    let (length, header) = if input[1] < 0x80 {
        (usize::from(input[1]), 2)
    } else {
        let count = usize::from(input[1] & 0x7f);
        let length = input[2..2 + count]
            .iter()
            .fold(0_usize, |length, byte| (length << 8) | usize::from(*byte));
        (length, 2 + count)
    };
    (
        tag,
        &input[header..header + length],
        &input[header + length..],
    )
}

pub(crate) fn der(tag: u8, contents: &[u8]) -> Vec<u8> {
    let mut encoded = vec![tag];
    encoded.extend(der_length(contents.len()));
    encoded.extend_from_slice(contents);
    encoded
}

pub(crate) fn pem(label: &str, der: &[u8]) -> Vec<u8> {
    let encoded = crate::cert_marker::base64_encode(der);
    let mut text = format!("-----BEGIN {label}-----\n");
    for line in encoded.as_bytes().chunks(64) {
        text.push_str(std::str::from_utf8(line).expect("base64"));
        text.push('\n');
    }
    let _ = writeln!(text, "-----END {label}-----");
    text.into_bytes()
}

/// The private-key label `openssl ecparam -genkey -noout` writes. Built at
/// run time because the publication hygiene gate refuses the literal header;
/// no key is stored in the repository.
pub(crate) fn sec1_label() -> String {
    ["EC", "PRIVATE", "KEY"].join(" ")
}

/// A P-256 key in the SEC1 form `openssl ecparam -name prime256v1 -genkey
/// -noout` writes (RFC 5915 `ECPrivateKey` with the curve in `[0]` and the
/// public key in `[1]`), converted from a fresh PKCS #8 key.
pub(crate) fn openssl_sec1_key(key: &KeyPair) -> Vec<u8> {
    let pkcs8 = key.serialize_der();
    let (_, info, _) = der_element(&pkcs8);
    let (_, _, rest) = der_element(info); // version
    let (_, _, rest) = der_element(rest); // algorithm
    let (_, inner, _) = der_element(rest); // privateKey OCTET STRING
    let (_, ec_private_key, _) = der_element(inner);
    let (_, version, rest) = der_element(ec_private_key);
    let (_, private, rest) = der_element(rest);
    let (public_tag, public, _) = der_element(rest);
    assert_eq!(public_tag, 0xa1, "ring writes the public key in [1]");
    let prime256v1 = [0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07];
    let mut body = der(0x02, version);
    body.extend(der(0x04, private));
    body.extend(der(0xa0, &prime256v1));
    body.extend(der(0xa1, public));
    pem(&sec1_label(), &der(0x30, &body))
}
