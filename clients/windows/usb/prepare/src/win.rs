// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The Windows work of `arcen-usb-prepare`. This module is the crate's only
//! FFI; every `unsafe` block names what it relies on.
#![allow(unsafe_code)]

// Shared with the service; this binary uses only part of it.
#[allow(dead_code)]
#[path = "../../service/src/win/common.rs"]
mod common;

use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use arcen_usb_bridge::winusb_package::{
    CERTIFICATE_SUBJECT_PREFIX, InfDate, TabletModel, catalog_definition, winusb_inf,
};
use windows::Win32::Foundation::{BOOL, HWND, SYSTEMTIME};
use windows::Win32::Security::Cryptography::{
    BCRYPT_RSA_ALGORITHM, CERT_BASIC_CONSTRAINTS2_INFO, CERT_CONTEXT,
    CERT_CREATE_SELFSIGN_NO_KEY_INFO, CERT_EXTENSION, CERT_EXTENSIONS, CERT_KEY_CONTEXT_PROP_ID,
    CERT_KEY_SPEC, CERT_NCRYPT_KEY_SPEC, CERT_SET_PROPERTY_INHIBIT_PERSIST_FLAG,
    CERT_X500_NAME_STR, CRYPT_ALGORITHM_IDENTIFIER, CRYPT_BIT_BLOB, CRYPT_ENCODE_OBJECT_FLAGS,
    CRYPT_INTEGER_BLOB, CTL_USAGE, CertCreateSelfSignCertificate, CertFreeCertificateContext,
    CertSetCertificateContextProperty, CertStrToNameW, CryptEncodeObjectEx,
    HCRYPTPROV_OR_NCRYPT_KEY_HANDLE, MS_KEY_STORAGE_PROVIDER, NCRYPT_FLAGS, NCRYPT_KEY_HANDLE,
    NCRYPT_LENGTH_PROPERTY, NCRYPT_PROV_HANDLE, NCryptCreatePersistedKey, NCryptFinalizeKey,
    NCryptFreeObject, NCryptOpenStorageProvider, NCryptSetProperty, X509_ASN_ENCODING,
    X509_BASIC_CONSTRAINTS2, X509_ENHANCED_KEY_USAGE, X509_KEY_USAGE,
};
use windows::core::{PCSTR, PCWSTR, PSTR};

use common::wide;

pub(crate) fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("arcen-usb-prepare: {error}");
            ExitCode::FAILURE
        }
    }
}

fn parse_model(text: &str) -> Result<TabletModel, String> {
    let (vendor, product) = text
        .split_once(':')
        .ok_or("a model is VID:PID in hex, for example 056A:0317")?;
    let hex = |value: &str| {
        (value.len() == 4)
            .then(|| u16::from_str_radix(value, 16).ok())
            .flatten()
            .ok_or_else(|| format!("{value:?} is not four hex digits"))
    };
    Ok(TabletModel {
        vendor_id: hex(vendor)?,
        product_id: hex(product)?,
    })
}

fn run(args: &[String]) -> Result<(), String> {
    let packages = common::secure_component_root()
        .map_err(|error| format!("{error} (run as administrator)"))?
        .join("packages");
    match args {
        [flag, instance] if flag == "--instance" => prepare(
            &packages,
            TabletModel::from_device_id(instance)
                .ok_or("not a device-level USB instance ID (USB\\VID_xxxx&PID_yyyy\\…)")?,
        ),
        [flag, model] if flag == "--model" => prepare(&packages, parse_model(model)?),
        [flag, model] if flag == "--remove" => remove(&packages, parse_model(model)?),
        [flag] if flag == "--remove-all" => {
            for entry in std::fs::read_dir(&packages).map_err(|error| error.to_string())? {
                let path = entry.map_err(|error| error.to_string())?.path();
                if path
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with("arcen_usb_"))
                {
                    std::fs::remove_dir_all(&path)
                        .map_err(|error| format!("remove {}: {error}", path.display()))?;
                    println!("removed {}", path.display());
                }
            }
            Ok(())
        }
        [flag] if flag == "--list" => {
            for entry in std::fs::read_dir(&packages).map_err(|error| error.to_string())? {
                println!("{}", entry.map_err(|error| error.to_string())?.file_name().to_string_lossy());
            }
            Ok(())
        }
        _ => Err(
            "usage: arcen-usb-prepare --instance ID | --model VID:PID | --remove VID:PID | --remove-all | --list"
                .to_owned(),
        ),
    }
}

fn remove(packages: &Path, model: TabletModel) -> Result<(), String> {
    let dir = packages.join(model.package_name());
    if dir.exists() {
        std::fs::remove_dir_all(&dir)
            .map_err(|error| format!("remove {}: {error}", dir.display()))?;
        println!("removed {}", dir.display());
    }
    Ok(())
}

fn key_files() -> Vec<PathBuf> {
    let program_data = std::env::var_os("ProgramData")
        .map_or_else(|| PathBuf::from(r"C:\ProgramData"), PathBuf::from);
    let mut dirs = vec![
        program_data.join(r"Microsoft\Crypto\Keys"),
        program_data.join(r"Microsoft\Crypto\RSA\MachineKeys"),
    ];
    if let Some(app_data) = std::env::var_os("APPDATA") {
        let app_data = PathBuf::from(app_data);
        dirs.push(app_data.join(r"Microsoft\Crypto\Keys"));
        dirs.push(app_data.join(r"Microsoft\Crypto\RSA"));
    }
    let mut files = Vec::new();
    let mut stack = dirs;
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                files.push(path);
            }
        }
    }
    files
}

fn today() -> InfDate {
    // SAFETY: returns the current UTC time.
    let time = unsafe { windows::Win32::System::SystemInformation::GetSystemTime() };
    InfDate {
        year: time.wYear,
        month: u8::try_from(time.wMonth).unwrap_or(1),
        day: u8::try_from(time.wDay).unwrap_or(1),
    }
}

fn prepare(packages: &Path, model: TabletModel) -> Result<(), String> {
    let name = model.package_name();
    let dir = packages.join(&name);
    // A fresh package each time: a new key, a new certificate.
    if dir.exists() {
        std::fs::remove_dir_all(&dir)
            .map_err(|error| format!("replace {}: {error}", dir.display()))?;
    }
    std::fs::create_dir_all(&dir).map_err(|error| format!("create {}: {error}", dir.display()))?;
    let inf = dir.join(format!("{name}.inf"));
    let cat = dir.join(format!("{name}.cat"));
    let cer = dir.join(format!("{name}.cer"));

    let mut utf16 = vec![0xff_u8, 0xfe];
    for unit in winusb_inf(model, today()).encode_utf16() {
        utf16.extend_from_slice(&unit.to_le_bytes());
    }
    std::fs::write(&inf, utf16).map_err(|error| format!("write {}: {error}", inf.display()))?;

    let cdf = dir.join(format!("{name}.cdf"));
    std::fs::write(&cdf, catalog_definition(model, &dir.to_string_lossy()))
        .map_err(|error| format!("write {}: {error}", cdf.display()))?;
    let made = make_catalog(&cdf);
    let _ = std::fs::remove_file(&cdf);
    made?;

    let before = key_files();
    let outcome = sign_with_ephemeral_key(&cat, &cer);
    let after = key_files();
    let left: Vec<_> = after.iter().filter(|file| !before.contains(file)).collect();
    if let Err(error) = outcome {
        if std::env::var_os("ARCEN_PREPARE_KEEP_FAILED").is_none() {
            let _ = std::fs::remove_dir_all(&dir);
        }
        return Err(error);
    }
    if !left.is_empty() {
        let _ = std::fs::remove_dir_all(&dir);
        return Err(format!(
            "a key file appeared on disk while signing: {left:?}"
        ));
    }
    println!("prepared {} in {}", model.hardware_id(), dir.display());
    println!("key: in memory only, freed; key files created on disk: 0");
    Ok(())
}

/// `CRYPTUI_WIZ_DIGITAL_SIGN_EXTENDED_INFO` (`cryptuiapi.h`).
#[repr(C)]
struct SignExtendedInfo {
    size: u32,
    attr_flags: u32,
    description: PCWSTR,
    more_info: PCWSTR,
    hash_algorithm: PCSTR,
    signing_cert_display: PCWSTR,
    additional_store: *mut c_void,
    authenticated: *mut c_void,
    unauthenticated: *mut c_void,
}

/// `CRYPTUI_WIZ_DIGITAL_SIGN_INFO` (`cryptuiapi.h`), file subject and a
/// certificate context.
#[repr(C)]
struct SignInfo {
    size: u32,
    subject_choice: u32,
    file_name: PCWSTR,
    cert_choice: u32,
    cert: *const CERT_CONTEXT,
    timestamp_url: PCWSTR,
    additional_cert_choice: u32,
    extended: *mut SignExtendedInfo,
}

#[link(name = "cryptui")]
unsafe extern "system" {
    fn CryptUIWizDigitalSign(
        flags: u32,
        parent: HWND,
        title: PCWSTR,
        info: *const SignInfo,
        context: *mut *mut c_void,
    ) -> BOOL;
    fn CryptUIWizFreeDigitalSignContext(context: *mut c_void) -> BOOL;
}

/// Whether the signer can reach the certificate's private key, for errors.
fn private_key_reachable(certificate: &Certificate) -> String {
    use windows::Win32::Security::Cryptography::{
        CRYPT_ACQUIRE_ALLOW_NCRYPT_KEY_FLAG, CryptAcquireCertificatePrivateKey,
    };
    let mut handle = HCRYPTPROV_OR_NCRYPT_KEY_HANDLE::default();
    let mut free = BOOL::default();
    // SAFETY: a valid context; the handle is not freed when `free` is false.
    match unsafe {
        CryptAcquireCertificatePrivateKey(
            certificate.0,
            CRYPT_ACQUIRE_ALLOW_NCRYPT_KEY_FLAG,
            None,
            &raw mut handle,
            None,
            Some(&raw mut free),
        )
    } {
        Ok(()) => format!("reachable, caller frees: {}", free.as_bool()),
        Err(error) => format!("not reachable: {error}"),
    }
}

/// Authenticode-signs `file` (the catalog) with the in-memory key, without
/// UI and without a timestamp, through the same in-box signer PowerShell's
/// `Set-AuthenticodeSignature` uses.
fn sign_file(file: &Path, certificate: &Certificate) -> Result<(), String> {
    const NO_UI: u32 = 1;
    const SUBJECT_FILE: u32 = 1;
    const SIGN_CERT: u32 = 1;
    const ADD_NONE: u32 = 0;
    const SHA256: &[u8] = b"2.16.840.1.101.3.4.2.1\0";
    let path = wide(&file.to_string_lossy());
    let mut extended = SignExtendedInfo {
        size: u32::try_from(size_of::<SignExtendedInfo>()).unwrap_or(0),
        attr_flags: 0,
        description: PCWSTR::null(),
        more_info: PCWSTR::null(),
        hash_algorithm: PCSTR(SHA256.as_ptr()),
        signing_cert_display: PCWSTR::null(),
        additional_store: std::ptr::null_mut(),
        authenticated: std::ptr::null_mut(),
        unauthenticated: std::ptr::null_mut(),
    };
    let info = SignInfo {
        size: u32::try_from(size_of::<SignInfo>()).unwrap_or(0),
        subject_choice: SUBJECT_FILE,
        file_name: PCWSTR(path.as_ptr()),
        cert_choice: SIGN_CERT,
        cert: certificate.0,
        timestamp_url: PCWSTR::null(),
        additional_cert_choice: ADD_NONE,
        extended: &raw mut extended,
    };
    let mut context: *mut c_void = std::ptr::null_mut();
    // SAFETY: every structure is a live local; no UI is shown.
    let signed = unsafe {
        CryptUIWizDigitalSign(
            NO_UI,
            HWND::default(),
            PCWSTR::null(),
            &raw const info,
            &raw mut context,
        )
    };
    let error = windows::core::Error::from_win32();
    if !context.is_null() {
        // SAFETY: frees the context returned above.
        let _ = unsafe { CryptUIWizFreeDigitalSignContext(context) };
    }
    if signed.as_bool() {
        Ok(())
    } else {
        Err(format!(
            "sign the catalog: {error} (private key {})",
            private_key_reachable(certificate)
        ))
    }
}

type ParseError = unsafe extern "system" fn(u32, u32, PCWSTR);

#[link(name = "wintrust")]
unsafe extern "system" {
    fn CryptCATCDFOpen(path: PCWSTR, callback: Option<ParseError>) -> *mut c_void;
    fn CryptCATCDFEnumMembersByCDFTagEx(
        cdf: *mut c_void,
        previous_tag: PCWSTR,
        callback: Option<ParseError>,
        member: *mut *mut c_void,
        continue_on_error: BOOL,
        reserved: *mut c_void,
    ) -> PCWSTR;
    fn CryptCATCDFEnumAttributesWithCDFTag(
        cdf: *mut c_void,
        member_tag: PCWSTR,
        member: *mut c_void,
        previous: *mut c_void,
        callback: Option<ParseError>,
    ) -> *mut c_void;
    fn CryptCATCDFClose(cdf: *mut c_void) -> BOOL;
}

static CDF_ERRORS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

unsafe extern "system" fn on_cdf_error(_area: u32, _error: u32, _line: PCWSTR) {
    CDF_ERRORS.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
}

/// Builds the catalog the CDF describes, as `makecat` does.
fn make_catalog(cdf: &Path) -> Result<(), String> {
    let path = wide(&cdf.to_string_lossy());
    CDF_ERRORS.store(0, std::sync::atomic::Ordering::Release);
    // SAFETY: opens the CDF we just wrote.
    let handle = unsafe { CryptCATCDFOpen(PCWSTR(path.as_ptr()), Some(on_cdf_error)) };
    if handle.is_null() {
        return Err(format!(
            "catalog definition: {}",
            windows::core::Error::from_win32()
        ));
    }
    let mut tag = PCWSTR::null();
    let mut member: *mut c_void = std::ptr::null_mut();
    let mut members = 0;
    loop {
        // SAFETY: enumerates members; `tag` is owned by the CDF.
        tag = unsafe {
            CryptCATCDFEnumMembersByCDFTagEx(
                handle,
                tag,
                Some(on_cdf_error),
                &raw mut member,
                true.into(),
                std::ptr::null_mut(),
            )
        };
        if tag.is_null() {
            break;
        }
        members += 1;
        let mut attribute: *mut c_void = std::ptr::null_mut();
        loop {
            // SAFETY: enumerates the member's attributes.
            attribute = unsafe {
                CryptCATCDFEnumAttributesWithCDFTag(
                    handle,
                    tag,
                    member,
                    attribute,
                    Some(on_cdf_error),
                )
            };
            if attribute.is_null() {
                break;
            }
        }
    }
    // SAFETY: closes (and writes) the catalog.
    let closed = unsafe { CryptCATCDFClose(handle) };
    let errors = CDF_ERRORS.load(std::sync::atomic::Ordering::Acquire);
    if !closed.as_bool() || members != 1 || errors != 0 {
        return Err(format!(
            "catalog: members={members} errors={errors} closed={}",
            closed.as_bool()
        ));
    }
    Ok(())
}

/// DER-encodes one structure with `CryptEncodeObjectEx`.
fn encode(kind: PCSTR, value: *const c_void) -> Result<Vec<u8>, String> {
    let mut size = 0_u32;
    // SAFETY: sizing call.
    unsafe {
        CryptEncodeObjectEx(
            X509_ASN_ENCODING,
            kind,
            value,
            CRYPT_ENCODE_OBJECT_FLAGS(0),
            None,
            None,
            &raw mut size,
        )
    }
    .map_err(|error| format!("encode extension: {error}"))?;
    let mut out = vec![0_u8; size as usize];
    // SAFETY: encodes into a buffer of the size just reported.
    unsafe {
        CryptEncodeObjectEx(
            X509_ASN_ENCODING,
            kind,
            value,
            CRYPT_ENCODE_OBJECT_FLAGS(0),
            None,
            Some(out.as_mut_ptr().cast()),
            &raw mut size,
        )
    }
    .map_err(|error| format!("encode extension: {error}"))?;
    out.truncate(size as usize);
    Ok(out)
}

struct Key(NCRYPT_KEY_HANDLE);
impl Drop for Key {
    fn drop(&mut self) {
        // SAFETY: frees the in-memory key once; an unnamed key has no
        // persisted copy to delete.
        let _ = unsafe { NCryptFreeObject(self.0) };
    }
}

struct Provider(NCRYPT_PROV_HANDLE);
impl Drop for Provider {
    fn drop(&mut self) {
        // SAFETY: frees the provider handle once.
        let _ = unsafe { NCryptFreeObject(self.0) };
    }
}

struct Certificate(*mut CERT_CONTEXT);
impl Drop for Certificate {
    fn drop(&mut self) {
        // SAFETY: frees the context once.
        let _ = unsafe { CertFreeCertificateContext(Some(self.0)) };
    }
}

fn ephemeral_key() -> Result<Key, String> {
    let mut provider = NCRYPT_PROV_HANDLE::default();
    // SAFETY: opens the software key storage provider.
    unsafe { NCryptOpenStorageProvider(&raw mut provider, MS_KEY_STORAGE_PROVIDER, 0) }
        .map_err(|error| format!("key provider: {error}"))?;
    let provider = Provider(provider);
    let mut key = NCRYPT_KEY_HANDLE::default();
    // SAFETY: a key with no name is ephemeral: it is never written to disk.
    unsafe {
        NCryptCreatePersistedKey(
            provider.0,
            &raw mut key,
            BCRYPT_RSA_ALGORITHM,
            PCWSTR::null(),
            CERT_KEY_SPEC(0),
            NCRYPT_FLAGS(0),
        )
    }
    .map_err(|error| format!("create key: {error}"))?;
    let key = Key(key);
    // SAFETY: sets the key length before finalising.
    unsafe {
        NCryptSetProperty(
            key.0,
            NCRYPT_LENGTH_PROPERTY,
            &3072_u32.to_le_bytes(),
            NCRYPT_FLAGS(0),
        )
    }
    .map_err(|error| format!("key length: {error}"))?;
    // SAFETY: generates the key pair.
    unsafe { NCryptFinalizeKey(key.0, NCRYPT_FLAGS(0)) }
        .map_err(|error| format!("generate key: {error}"))?;
    Ok(key)
}

fn system_time_years_from_now(years: u16) -> SYSTEMTIME {
    // SAFETY: returns the current UTC time.
    let mut time = unsafe { windows::Win32::System::SystemInformation::GetSystemTime() };
    time.wYear = time.wYear.saturating_add(years);
    if time.wMonth == 2 && time.wDay == 29 {
        time.wDay = 28;
    }
    time
}

fn encoded_subject(subject: &str) -> Result<Vec<u8>, String> {
    let subject_w = wide(subject);
    let mut size = 0_u32;
    // SAFETY: sizing call for the encoded subject.
    unsafe {
        CertStrToNameW(
            X509_ASN_ENCODING,
            PCWSTR(subject_w.as_ptr()),
            CERT_X500_NAME_STR,
            None,
            None,
            &raw mut size,
            None,
        )
    }
    .map_err(|error| format!("subject: {error}"))?;
    let mut name = vec![0_u8; size as usize];
    // SAFETY: encodes into a buffer of the reported size.
    unsafe {
        CertStrToNameW(
            X509_ASN_ENCODING,
            PCWSTR(subject_w.as_ptr()),
            CERT_X500_NAME_STR,
            None,
            Some(name.as_mut_ptr()),
            &raw mut size,
            None,
        )
    }
    .map_err(|error| format!("subject: {error}"))?;
    name.truncate(size as usize);
    Ok(name)
}

/// The DER values of the three extensions: code signing only, not a CA,
/// digital signature only.
fn encoded_extensions() -> Result<[Vec<u8>; 3], String> {
    let mut code_signing = *b"1.3.6.1.5.5.7.3.3\0";
    let mut usages = [PSTR(code_signing.as_mut_ptr())];
    let usage = CTL_USAGE {
        cUsageIdentifier: 1,
        rgpszUsageIdentifier: usages.as_mut_ptr(),
    };
    let eku = encode(X509_ENHANCED_KEY_USAGE, (&raw const usage).cast())?;
    let constraints = CERT_BASIC_CONSTRAINTS2_INFO {
        fCA: false.into(),
        fPathLenConstraint: false.into(),
        dwPathLenConstraint: 0,
    };
    let basic = encode(X509_BASIC_CONSTRAINTS2, (&raw const constraints).cast())?;
    let mut digital_signature = [0x80_u8];
    let bits = CRYPT_BIT_BLOB {
        cbData: 1,
        pbData: digital_signature.as_mut_ptr(),
        cUnusedBits: 7,
    };
    let usage_bits = encode(X509_KEY_USAGE, (&raw const bits).cast())?;
    Ok([eku, basic, usage_bits])
}

/// `CERT_KEY_CONTEXT` (wincrypt.h) for a CNG key.
#[repr(C)]
struct KeyContext {
    size: u32,
    key: usize,
    key_spec: u32,
}

fn self_signed(key: Key) -> Result<Certificate, String> {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos())
        ^ u128::from(std::process::id());
    let subject = format!(
        "{CERTIFICATE_SUBJECT_PREFIX}{:012x} (this PC only), O=Arcen local",
        nonce & 0xffff_ffff_ffff
    );
    let mut name = encoded_subject(&subject)?;
    let size = u32::try_from(name.len()).unwrap_or(0);
    let name_blob = CRYPT_INTEGER_BLOB {
        cbData: size,
        pbData: name.as_mut_ptr(),
    };

    let [mut eku, mut basic, mut usage_bits] = encoded_extensions()?;
    let mut eku_oid = *b"2.5.29.37\0";
    let mut basic_oid = *b"2.5.29.19\0";
    let mut key_usage_oid = *b"2.5.29.15\0";
    let blob = |bytes: &mut Vec<u8>| CRYPT_INTEGER_BLOB {
        cbData: u32::try_from(bytes.len()).unwrap_or(0),
        pbData: bytes.as_mut_ptr(),
    };
    let mut extensions = [
        CERT_EXTENSION {
            pszObjId: PSTR(eku_oid.as_mut_ptr()),
            fCritical: true.into(),
            Value: blob(&mut eku),
        },
        CERT_EXTENSION {
            pszObjId: PSTR(basic_oid.as_mut_ptr()),
            fCritical: true.into(),
            Value: blob(&mut basic),
        },
        CERT_EXTENSION {
            pszObjId: PSTR(key_usage_oid.as_mut_ptr()),
            fCritical: true.into(),
            Value: blob(&mut usage_bits),
        },
    ];
    let all = CERT_EXTENSIONS {
        cExtension: 3,
        rgExtension: extensions.as_mut_ptr(),
    };
    let mut sha256_rsa = *b"1.2.840.113549.1.1.11\0";
    let algorithm = CRYPT_ALGORITHM_IDENTIFIER {
        pszObjId: PSTR(sha256_rsa.as_mut_ptr()),
        Parameters: CRYPT_INTEGER_BLOB::default(),
    };
    let mut start = system_time_years_from_now(0);
    start.wHour = start.wHour.saturating_sub(1);
    let end = system_time_years_from_now(10);
    // SAFETY: every pointer refers to a local that outlives the call.
    let context = unsafe {
        CertCreateSelfSignCertificate(
            HCRYPTPROV_OR_NCRYPT_KEY_HANDLE(key.0.0),
            &raw const name_blob,
            // The key is unnamed, so there is no provider info to record;
            // the signer reaches it through CERT_NCRYPT_KEY_HANDLE_PROP_ID.
            CERT_CREATE_SELFSIGN_NO_KEY_INFO,
            None,
            Some(&raw const algorithm),
            Some(&raw const start),
            Some(&raw const end),
            Some(&raw const all),
        )
    };
    if context.is_null() {
        return Err(format!(
            "self-signed certificate: {}",
            windows::core::Error::from_win32()
        ));
    }
    let certificate = Certificate(context);
    // The signer finds the in-memory key through the certificate's key
    // context, which also takes ownership of the handle: freeing the
    // certificate context frees the key.
    let context = KeyContext {
        size: u32::try_from(size_of::<KeyContext>()).unwrap_or(0),
        key: key.0.0,
        key_spec: CERT_NCRYPT_KEY_SPEC.0,
    };
    // SAFETY: a valid context and a CERT_KEY_CONTEXT naming a live key.
    unsafe {
        CertSetCertificateContextProperty(
            certificate.0,
            CERT_KEY_CONTEXT_PROP_ID,
            CERT_SET_PROPERTY_INHIBIT_PERSIST_FLAG,
            Some((&raw const context).cast()),
        )
    }
    .map_err(|error| format!("attach key: {error}"))?;
    std::mem::forget(key);
    Ok(certificate)
}

fn sign_with_ephemeral_key(cat: &Path, cer: &Path) -> Result<(), String> {
    let key = ephemeral_key()?;
    let certificate = self_signed(key)?;
    sign_file(cat, &certificate)?;
    // SAFETY: the context's encoded bytes are valid while it lives.
    let der = unsafe {
        std::slice::from_raw_parts(
            (*certificate.0).pbCertEncoded,
            (*certificate.0).cbCertEncoded as usize,
        )
    }
    .to_vec();
    std::fs::write(cer, der).map_err(|error| format!("write {}: {error}", cer.display()))?;
    // Freeing the certificate context frees the key it owns.
    drop(certificate);
    Ok(())
}
