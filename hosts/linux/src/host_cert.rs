//! Linux Pier certificate material.
//!
//! The decision of what to do — create, keep, renew, rekey or adopt — comes
//! from [`arcen_transport::cert_provisioning`], the same module the macOS host
//! uses, so both answer the question identically. Ownership and pins come from
//! [`arcen_transport::cert_marker`]. Only generation is Linux-specific, and it
//! shells out to `openssl` so material produced here is indistinguishable from
//! what `packaging/linux/new-host-cert.sh` produces.
//!
//! This path previously made its own decision, and got one case wrong: it
//! returned early only when *both* the certificate and the key were present,
//! so a directory holding just `host.crt` fell through and regenerated both,
//! silently replacing a certificate whose key had gone missing. The shared
//! policy refuses that, as the helper script always has.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use arcen_transport::cert_marker::{self, OwnershipMarker};
use arcen_transport::cert_provisioning::{
    plan, MaterialOwnership, MaterialState, ProvisioningAction, ProvisioningRequest,
};

const DEFAULT_DIRECTORY: &str = "/etc/arcen";
/// Ownership marker name, shared with the helper script and the macOS host.
const MARKER_FILE: &str = "host.generated-by-arcen";
/// Whole-certificate pin file.
const CERT_PIN_FILE: &str = "host.cert-sha256";
/// Subject public key pin file.
const SPKI_PIN_FILE: &str = "host.spki-sha256";
/// Certificate lifetime in days.
const VALIDITY_DAYS: &str = "825";
/// How close to expiry counts as due for renewal.
const RENEW_WITHIN_SECONDS: i64 = 30 * 24 * 60 * 60;

pub fn main(args: &[String]) -> ExitCode {
    match run(args) {
        Ok(message) => {
            println!("{message}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("new-host-cert: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Paths to the managed material.
struct MaterialPaths {
    certificate: PathBuf,
    key: PathBuf,
    marker: PathBuf,
    certificate_pin: PathBuf,
    spki_pin: PathBuf,
}

impl MaterialPaths {
    fn in_directory(directory: &Path) -> Self {
        Self {
            certificate: directory.join("host.crt"),
            key: directory.join("host.key"),
            marker: directory.join(MARKER_FILE),
            certificate_pin: directory.join(CERT_PIN_FILE),
            spki_pin: directory.join(SPKI_PIN_FILE),
        }
    }
}

/// Runs the `new-host-cert` subcommand.
///
/// # Errors
///
/// Returns a message describing why provisioning was refused or failed.
pub fn run(args: &[String]) -> Result<String, String> {
    let (directory, request, sans) = parse_arguments(args)?;
    std::fs::create_dir_all(&directory)
        .map_err(|error| format!("create {}: {error}", directory.display()))?;
    let paths = MaterialPaths::in_directory(&directory);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| format!("system clock is before the Unix epoch: {error}"))?
        .as_secs();

    let state = inspect(&paths, now);
    let decided = plan(request, state)
        .map_err(|refusal| format!("{}: {}", refusal.as_str(), refusal.guidance()))?;

    match decided.action {
        ProvisioningAction::KeepExisting => Ok(format!(
            "TLS certificate already present at {} and {}",
            paths.certificate.display(),
            paths.key.display()
        )),
        ProvisioningAction::CreateNew | ProvisioningAction::ReplaceKeyAndCertificate => {
            issue(&paths, false, &sans)?;
            Ok(format!(
                "generated TLS certificate at {} and key at {}\n\
                 This replaced the host key. Every Deck that pinned the previous \
                 certificate must re-pin before it will connect again.",
                paths.certificate.display(),
                paths.key.display()
            ))
        }
        ProvisioningAction::RenewPreservingKey | ProvisioningAction::AdoptAndRenew => {
            issue(&paths, true, &sans)?;
            Ok(format!(
                "reissued TLS certificate at {} over the existing key; \
                 existing client pins remain valid",
                paths.certificate.display()
            ))
        }
    }
}

/// Reads the directory into the shared decision input.
fn inspect(paths: &MaterialPaths, now_epoch_secs: u64) -> MaterialState {
    let certificate_present = paths.certificate.is_file();
    let key_present = paths.key.is_file();
    if !certificate_present && !key_present {
        return MaterialState::absent();
    }

    let certificate_bytes = std::fs::read(&paths.certificate).ok();
    let ownership = certificate_bytes.as_ref().map(|bytes| {
        if marker_matches(paths, bytes) {
            MaterialOwnership::Owned
        } else {
            MaterialOwnership::Foreign
        }
    });

    let now = i64::try_from(now_epoch_secs).unwrap_or(i64::MAX);
    let (certificate_valid, expiring_or_expired) = certificate_bytes
        .as_ref()
        .and_then(|bytes| cert_marker::validity_from_pem(bytes))
        .map_or((false, false), |window| {
            (
                window.is_current(now),
                window.is_due_for_renewal(now, RENEW_WITHIN_SECONDS),
            )
        });

    MaterialState {
        certificate_present,
        key_present,
        ownership,
        certificate_valid,
        expiring_or_expired,
        // The helper script owns transaction recovery on this platform, so
        // this path does not claim to detect its staging files.
        stale_staging_present: false,
        self_signed: certificate_bytes
            .as_ref()
            .is_some_and(|bytes| cert_marker::is_self_signed_pem(bytes)),
    }
}

/// Returns whether the ownership marker describes the certificate on disk.
fn marker_matches(paths: &MaterialPaths, certificate_bytes: &[u8]) -> bool {
    let Ok(recorded) = std::fs::read_to_string(&paths.marker) else {
        return false;
    };
    let Ok(marker) = OwnershipMarker::parse(&recorded) else {
        return false;
    };
    let Some(pins) = cert_marker::pins_from_pem(certificate_bytes) else {
        return false;
    };
    marker.matches(&pins.certificate, &pins.spki)
}

/// Generates material through `openssl` and publishes it.
///
/// `sans` are the subject alternative names the certificate is valid for.
/// A certificate without them is rejected by every Deck during the TLS
/// handshake, so an empty list is refused here rather than producing material
/// that looks fine on disk and fails at connection time.
fn issue(paths: &MaterialPaths, reuse_key: bool, sans: &[String]) -> Result<(), String> {
    if sans.is_empty() {
        return Err(
            "at least one --dns or --ip subject alternative name is required; \
                    a certificate without one is refused by every client"
                .to_owned(),
        );
    }
    let san = sans.join(",");
    let staged_key = staging_for(&paths.key);
    let staged_cert = staging_for(&paths.certificate);

    let key_for_cert = if reuse_key {
        paths.key.clone()
    } else {
        let status = std::process::Command::new("openssl")
            .args([
                "ecparam",
                "-name",
                "prime256v1",
                "-genkey",
                "-noout",
                "-out",
            ])
            .arg(&staged_key)
            .status()
            .map_err(|error| format!("start openssl key generation: {error}"))?;
        if !status.success() {
            return Err("openssl key generation failed".to_owned());
        }
        chmod(&staged_key, 0o600)?;
        staged_key.clone()
    };

    let status = std::process::Command::new("openssl")
        .args(["req", "-x509", "-new", "-sha256", "-days", VALIDITY_DAYS])
        .arg("-key")
        .arg(&key_for_cert)
        .arg("-out")
        .arg(&staged_cert)
        .args(["-subj", "/CN=Arcen Pier"])
        .args(["-addext", &format!("subjectAltName={san}")])
        .args(["-addext", "basicConstraints=critical,CA:FALSE"])
        .args(["-addext", "keyUsage=critical,digitalSignature"])
        .args(["-addext", "extendedKeyUsage=serverAuth"])
        .status()
        .map_err(|error| format!("start openssl certificate generation: {error}"))?;
    if !status.success() {
        let _ = std::fs::remove_file(&staged_key);
        let _ = std::fs::remove_file(&staged_cert);
        return Err("openssl certificate generation failed".to_owned());
    }

    if key_for_cert == staged_key {
        std::fs::rename(&staged_key, &paths.key)
            .map_err(|error| format!("install {}: {error}", paths.key.display()))?;
        chmod(&paths.key, 0o600)?;
    }
    std::fs::rename(&staged_cert, &paths.certificate)
        .map_err(|error| format!("install {}: {error}", paths.certificate.display()))?;
    chmod(&paths.certificate, 0o644)?;

    write_pins_and_marker(paths)
}

/// Writes the pin files and ownership marker for the published certificate.
fn write_pins_and_marker(paths: &MaterialPaths) -> Result<(), String> {
    let bytes = std::fs::read(&paths.certificate)
        .map_err(|error| format!("read {}: {error}", paths.certificate.display()))?;
    let pins = cert_marker::pins_from_pem(&bytes)
        .ok_or_else(|| "cannot pin the generated certificate".to_owned())?;
    let marker = OwnershipMarker::new(&pins.certificate, &pins.spki)
        .map_err(|error| format!("ownership marker: {error}"))?;

    write_file(
        &paths.certificate_pin,
        &format!(
            "sha256 Fingerprint={}\n",
            cert_marker::colon_hex(&pins.certificate)
        ),
    )?;
    write_file(&paths.spki_pin, &format!("{}\n", pins.spki))?;
    write_file(&paths.marker, &marker.render())
}

fn write_file(path: &Path, contents: &str) -> Result<(), String> {
    let staging = staging_for(path);
    std::fs::write(&staging, contents)
        .map_err(|error| format!("write {}: {error}", staging.display()))?;
    std::fs::rename(&staging, path)
        .map_err(|error| format!("publish {}: {error}", path.display()))?;
    chmod(path, 0o644)
}

fn staging_for(target: &Path) -> PathBuf {
    let name = target.file_name().map_or_else(
        || "material".to_owned(),
        |name| name.to_string_lossy().into_owned(),
    );
    target.with_file_name(format!(".{name}.installing.{}", std::process::id()))
}

fn parse_arguments(args: &[String]) -> Result<(PathBuf, ProvisioningRequest, Vec<String>), String> {
    let mut directory = PathBuf::from(DEFAULT_DIRECTORY);
    let mut request = ProvisioningRequest::Ensure;
    let mut adopt = false;
    let mut sans: Vec<String> = Vec::new();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--directory" => {
                index += 1;
                directory = PathBuf::from(
                    args.get(index)
                        .ok_or_else(|| "--directory requires a path".to_owned())?,
                );
            }
            "--renew" => request = ProvisioningRequest::Renew,
            "--new-key" => request = ProvisioningRequest::Rekey,
            "--adopt-legacy" => adopt = true,
            "--dns" | "--ip" => {
                let flag = args[index].clone();
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| format!("{flag} requires a value"))?;
                if value.is_empty() {
                    return Err(format!("{flag} requires a non-empty value"));
                }
                // openssl's own SAN syntax, so the two paths produce
                // certificates an operator can compare directly.
                sans.push(if flag == "--dns" {
                    format!("DNS:{value}")
                } else {
                    format!("IP:{value}")
                });
            }
            "--help" | "-h" => {
                return Err("usage: arcen-pier new-host-cert \
                            [--renew|--new-key|--adopt-legacy] [--directory DIR] \
                            [--dns NAME]... [--ip ADDRESS]..."
                    .to_owned());
            }
            other => return Err(format!("unknown argument: {other}")),
        }
        index += 1;
    }
    // Adoption is its own request, not a modifier, so it cannot be combined
    // with one that means something different.
    if adopt {
        if matches!(request, ProvisioningRequest::Rekey) {
            return Err("--adopt-legacy cannot be combined with --new-key".to_owned());
        }
        request = ProvisioningRequest::AdoptLegacy;
    }
    Ok((directory, request, sans))
}

fn chmod(path: &Path, mode: u32) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .map_err(|error| format!("chmod {}: {error}", path.display()))?;
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_ensuring_material_in_the_managed_directory() {
        let (directory, request, _) = parse_arguments(&[]).expect("no arguments is valid");
        assert_eq!(directory, PathBuf::from(DEFAULT_DIRECTORY));
        assert_eq!(request, ProvisioningRequest::Ensure);
    }

    #[test]
    fn flags_map_onto_the_shared_requests() {
        for (flag, expected) in [
            ("--renew", ProvisioningRequest::Renew),
            ("--new-key", ProvisioningRequest::Rekey),
            ("--adopt-legacy", ProvisioningRequest::AdoptLegacy),
        ] {
            let (_, request, _) = parse_arguments(&[flag.to_owned()]).expect("valid flag");
            assert_eq!(request, expected, "{flag}");
        }
    }

    #[test]
    fn adoption_cannot_be_combined_with_a_rekey() {
        // They mean different things: one preserves the key, the other
        // replaces it.
        assert!(parse_arguments(&["--new-key".to_owned(), "--adopt-legacy".to_owned()]).is_err());
    }

    #[test]
    fn a_directory_can_be_chosen_and_must_have_a_value() {
        let (directory, _, _) =
            parse_arguments(&["--directory".to_owned(), "/tmp/pier".to_owned()]).expect("valid");
        assert_eq!(directory, PathBuf::from("/tmp/pier"));
        assert!(parse_arguments(&["--directory".to_owned()]).is_err());
        assert!(parse_arguments(&["--nonsense".to_owned()]).is_err());
    }

    #[test]
    fn a_lone_certificate_is_refused_rather_than_regenerated() {
        // The bug this migration fixes: the previous implementation returned
        // early only when both files existed, so a directory holding just
        // `host.crt` fell through and regenerated both, replacing a
        // certificate whose key had gone missing.
        let state = MaterialState {
            certificate_present: true,
            key_present: false,
            ..MaterialState::absent()
        };
        assert!(
            plan(ProvisioningRequest::Ensure, state).is_err(),
            "a half pair must be refused"
        );
    }

    #[test]
    fn healthy_owned_material_is_left_alone() {
        let decided =
            plan(ProvisioningRequest::Ensure, MaterialState::owned_valid()).expect("usable");
        assert_eq!(decided.action, ProvisioningAction::KeepExisting);
    }
    #[test]
    fn subject_alternative_names_are_collected_in_openssl_syntax() {
        // A certificate without a SAN is refused by every client during the
        // TLS handshake, so these have to reach `openssl` rather than being
        // parsed and dropped.
        let (_, _, sans) = parse_arguments(&[
            "--dns".to_owned(),
            "pier.example.internal".to_owned(),
            "--ip".to_owned(),
            "203.0.113.10".to_owned(),
        ])
        .expect("valid");
        assert_eq!(sans, vec!["DNS:pier.example.internal", "IP:203.0.113.10"]);
    }

    #[test]
    fn an_empty_name_is_refused_rather_than_producing_an_empty_san() {
        assert!(parse_arguments(&["--dns".to_owned(), String::new()]).is_err());
        assert!(parse_arguments(&["--dns".to_owned()]).is_err());
        assert!(parse_arguments(&["--ip".to_owned()]).is_err());
    }

    #[test]
    fn issuing_without_a_name_is_refused_before_openssl_runs() {
        // The failure an operator must see is "you did not name the host",
        // not a Deck refusing to connect a week later.
        let directory = std::env::temp_dir().join("arcen-cert-san-test");
        let paths = MaterialPaths::in_directory(&directory);
        let error = issue(&paths, false, &[]).expect_err("no names must be refused");
        assert!(
            error.contains("subject alternative name"),
            "the refusal must say what is missing: {error}"
        );
        assert!(
            !paths.certificate.exists(),
            "nothing may be written when the request is refused"
        );
    }
}
