// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The transient machine trust a staging step needs: the package's public
//! certificate in `LocalMachine\Root` and `LocalMachine\TrustedPublisher`,
//! for as long as staging takes and no longer.

use std::ffi::c_void;

use windows::Win32::Security::Cryptography::{
    CERT_CONTEXT, CERT_FIND_SHA1_HASH, CERT_OPEN_STORE_FLAGS, CERT_QUERY_ENCODING_TYPE,
    CERT_SHA1_HASH_PROP_ID, CERT_STORE_ADD_REPLACE_EXISTING, CERT_STORE_PROV_SYSTEM_W,
    CERT_SYSTEM_STORE_LOCAL_MACHINE, CRYPT_INTEGER_BLOB, CertAddEncodedCertificateToStore,
    CertCloseStore, CertCreateCertificateContext, CertDeleteCertificateFromStore,
    CertFindCertificateInStore, CertFreeCertificateContext, CertGetCertificateContextProperty,
    CertOpenStore, HCERTSTORE, HCRYPTPROV_LEGACY, PKCS_7_ASN_ENCODING, X509_ASN_ENCODING,
};

use super::wide;

/// The two stores a self-signed driver package needs.
pub(crate) const STORES: [&str; 2] = ["Root", "TrustedPublisher"];

const ENCODING: CERT_QUERY_ENCODING_TYPE =
    CERT_QUERY_ENCODING_TYPE(X509_ASN_ENCODING.0 | PKCS_7_ASN_ENCODING.0);

struct Store(HCERTSTORE);

impl Drop for Store {
    fn drop(&mut self) {
        // SAFETY: opened by CertOpenStore, closed once.
        let _ = unsafe { CertCloseStore(self.0, 0) };
    }
}

fn open(name: &str) -> Result<Store, String> {
    let name_w = wide(name);
    // SAFETY: the system store provider with a NUL-terminated wide name.
    unsafe {
        CertOpenStore(
            CERT_STORE_PROV_SYSTEM_W,
            CERT_QUERY_ENCODING_TYPE(0),
            HCRYPTPROV_LEGACY(0),
            CERT_OPEN_STORE_FLAGS(CERT_SYSTEM_STORE_LOCAL_MACHINE),
            Some(name_w.as_ptr().cast::<c_void>()),
        )
    }
    .map(Store)
    .map_err(|error| format!("open LocalMachine\\{name}: {error}"))
}

/// The SHA-1 thumbprint (upper-case hex) of a DER certificate.
pub(crate) fn thumbprint(der: &[u8]) -> Result<String, String> {
    // SAFETY: parses the DER bytes into a context we free below.
    let context = unsafe { CertCreateCertificateContext(ENCODING, der) };
    if context.is_null() {
        return Err("the package certificate does not parse".to_owned());
    }
    let mut hash = [0_u8; 20];
    let mut size = 20_u32;
    // SAFETY: reads the 20-byte SHA-1 property into `hash`.
    let read = unsafe {
        CertGetCertificateContextProperty(
            context,
            CERT_SHA1_HASH_PROP_ID,
            Some(hash.as_mut_ptr().cast()),
            &raw mut size,
        )
    };
    // SAFETY: created above.
    let _ = unsafe { CertFreeCertificateContext(Some(context)) };
    read.map_err(|error| format!("certificate thumbprint: {error}"))?;
    Ok(hash.iter().fold(String::new(), |mut text, byte| {
        use std::fmt::Write as _;
        let _ = write!(text, "{byte:02X}");
        text
    }))
}

/// Adds the certificate to both stores.
pub(crate) fn add(der: &[u8]) -> Result<(), String> {
    for name in STORES {
        let store = open(name)?;
        // SAFETY: adds the encoded certificate; no context is returned.
        unsafe {
            CertAddEncodedCertificateToStore(
                store.0,
                X509_ASN_ENCODING,
                der,
                CERT_STORE_ADD_REPLACE_EXISTING,
                None,
            )
        }
        .map_err(|error| format!("trust in LocalMachine\\{name}: {error}"))?;
    }
    Ok(())
}

/// Removes every certificate with this thumbprint from both stores. Removing
/// one that is not there is not an error.
pub(crate) fn remove(thumbprint_hex: &str) -> Result<(), String> {
    let mut hash = parse_hex(thumbprint_hex).ok_or("malformed thumbprint")?;
    let blob = CRYPT_INTEGER_BLOB {
        cbData: u32::try_from(hash.len()).unwrap_or(0),
        pbData: hash.as_mut_ptr(),
    };
    for name in STORES {
        let store = open(name)?;
        loop {
            // SAFETY: finds by SHA-1 in an open store.
            let found: *mut CERT_CONTEXT = unsafe {
                CertFindCertificateInStore(
                    store.0,
                    ENCODING,
                    0,
                    CERT_FIND_SHA1_HASH,
                    Some((&raw const blob).cast()),
                    None,
                )
            };
            if found.is_null() {
                break;
            }
            // SAFETY: deletes (and frees) the found context.
            unsafe { CertDeleteCertificateFromStore(found) }
                .map_err(|error| format!("remove from LocalMachine\\{name}: {error}"))?;
        }
    }
    Ok(())
}

/// Thumbprints of every Arcen USB component certificate still in either
/// store (a crash before the journal recorded it).
pub(crate) fn arcen_leftovers() -> Vec<String> {
    let mut found = Vec::new();
    for name in STORES {
        let Ok(store) = open(name) else { continue };
        let mut previous: Option<*const CERT_CONTEXT> = None;
        loop {
            // SAFETY: enumerates every certificate (CERT_FIND_ANY = 0).
            let context = unsafe {
                CertFindCertificateInStore(
                    store.0,
                    ENCODING,
                    0,
                    windows::Win32::Security::Cryptography::CERT_FIND_ANY,
                    None,
                    previous,
                )
            };
            if context.is_null() {
                break;
            }
            previous = Some(context);
            // SAFETY: a live context from the store.
            let der = unsafe {
                std::slice::from_raw_parts(
                    (*context).pbCertEncoded,
                    (*context).cbCertEncoded as usize,
                )
            };
            if subject_is_arcen(der) {
                if let Ok(print) = thumbprint(der) {
                    if !found.contains(&print) {
                        found.push(print);
                    }
                }
            }
        }
    }
    found
}

/// Whether a DER certificate's subject carries the Arcen USB component
/// common name (a byte search for the UTF-8/printable CN value).
fn subject_is_arcen(der: &[u8]) -> bool {
    let needle = arcen_usb_bridge::winusb_package::CERTIFICATE_SUBJECT_PREFIX
        .trim_start_matches("CN=")
        .as_bytes();
    der.windows(needle.len()).any(|window| window == needle)
}

fn parse_hex(text: &str) -> Option<Vec<u8>> {
    if text.len() != 40 {
        return None;
    }
    (0..20)
        .map(|index| u8::from_str_radix(text.get(index * 2..index * 2 + 2)?, 16).ok())
        .collect()
}
