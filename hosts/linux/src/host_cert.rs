// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Linux Pier certificate material: `arcen-pier new-host-cert`.
//!
//! Every decision is shared. Whether to create, keep, renew, rekey or adopt
//! comes from [`arcen_transport::cert_provisioning`]; what to issue (the
//! profile, the names a renewal keeps, the key it keeps) from
//! [`arcen_transport::cert_issuance`]; and how the files are replaced and an
//! interrupted replacement recovered from [`arcen_transport::cert_publication`].
//! What is Linux's own is the file system: the directory is refused if any
//! component is a link or if anyone but its owner can write it, files are
//! created with their final mode and `O_NOFOLLOW` and synced with their
//! directory, and the transaction holds the same `flock` on
//! `.arcen-cert.lock` the Linux installer takes.
//!
//! `packaging/linux/new-host-cert.sh` (`arcen-new-host-cert`) runs this.

use std::fs;
use std::io::Write as _;
use std::os::unix::fs::{
    DirBuilderExt as _, MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _,
};
use std::path::{Component, Path, PathBuf};
use std::process::ExitCode;

use arcen_transport::cert_issuance::{
    issue, HostKey, IssuanceError, IssuanceProfile, IssuedMaterial, IssuerKey,
};
use arcen_transport::cert_provisioning::{plan, ProvisioningAction, ProvisioningRequest};
use arcen_transport::cert_publication::{
    self, Publication, PublicationError, PublicationFiles, Slot,
};
use arcen_transport::cert_store::{self, InspectContext, MaterialPaths};
use arcen_transport::cert_transaction::{transaction_id, JOURNAL_FILE, MANAGED_FILES};
use zeroize::Zeroizing;

/// Where the Linux Pier keeps its material (the FHS configuration directory).
const DEFAULT_DIRECTORY: &str = "/etc/arcen";
/// The lock every certificate transaction holds, beside the journal; the
/// Linux installer and the macOS Pier lock the same name.
const LOCK_FILE: &str = ".arcen-cert.lock";

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

/// Runs the `new-host-cert` subcommand.
///
/// # Errors
///
/// Returns a message describing why provisioning was refused or failed.
pub fn run(args: &[String]) -> Result<String, String> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| format!("system clock is before the Unix epoch: {error}"))?
        .as_secs();
    run_with(args, discover, now)
}

/// The names this host can see of itself, for a request that names none.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Discovered {
    /// `hostname -f`, or `hostname` when that fails.
    fqdn: Option<String>,
    /// `hostname -I`: the host's addresses, without loopback or link-local.
    addresses: Vec<String>,
}

fn discover() -> Discovered {
    let output = |args: &[&str]| {
        std::process::Command::new("hostname")
            .args(args)
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
            .filter(|text| !text.is_empty())
    };
    let fqdn = output(&["-f"]).or_else(|| output(&[]));
    let addresses = output(&["-I"])
        .unwrap_or_default()
        .split_whitespace()
        .filter(|address| {
            address
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| match ip {
                    std::net::IpAddr::V4(v4) => !v4.is_loopback() && !v4.is_link_local(),
                    std::net::IpAddr::V6(v6) => {
                        !v6.is_loopback() && (v6.segments()[0] & 0xffc0) != 0xfe80
                    }
                })
        })
        .map(str::to_owned)
        .collect();
    Discovered { fqdn, addresses }
}

/// The names to issue for: those given, and what the host sees of itself
/// for whichever kind (`--dns`, `--ip`) none were given for, as the earlier
/// helper did.
fn names_for(given: &[String], discovered: &Discovered) -> Vec<String> {
    let mut names = given.to_vec();
    if !given.iter().any(|name| name.starts_with("DNS:")) {
        if let Some(fqdn) = &discovered.fqdn {
            names.insert(0, format!("DNS:{fqdn}"));
        }
    }
    if !given.iter().any(|name| name.starts_with("IP:")) {
        names.extend(
            discovered
                .addresses
                .iter()
                .map(|address| format!("IP:{address}")),
        );
    }
    names
}

fn run_with(
    args: &[String],
    discover: impl FnOnce() -> Discovered,
    now: u64,
) -> Result<String, String> {
    let (directory, request, given) = parse_arguments(args)?;
    prepare_directory(&directory)?;
    let paths = MaterialPaths::in_directory(&directory);
    // One run at a time, from recovery to commit, shared with the installer.
    let _lock = DirectoryLock::acquire(&directory)?;
    // Finish or undo an interrupted transaction (this command's, the
    // installer's or the earlier helper's) before judging what is here.
    if let Some(transaction) =
        cert_publication::recover(&mut LinuxFiles { paths: &paths }).map_err(|e| e.to_string())?
    {
        eprintln!("new-host-cert: recovered interrupted transaction {transaction}");
    }
    let state = cert_store::inspect(
        &paths,
        now,
        InspectContext {
            stale_staging_present: paths
                .managed()
                .iter()
                .any(|path| staging_for(path).exists()),
            machine_sans: None,
        },
    )
    .map_err(|error| format!("read {}: {error}", paths.certificate.display()))?;
    let decided = plan(request, state)
        .map_err(|refusal| format!("{}: {}", refusal.as_str(), refusal.guidance()))?;
    if decided.clear_stale_staging {
        LinuxFiles { paths: &paths }.discard_staging()?;
    }

    if decided.action == ProvisioningAction::KeepExisting {
        return Ok(format!(
            "TLS certificate already present at {} and {}",
            paths.certificate.display(),
            paths.key.display()
        ));
    }

    let names = names_for(&given, &discover());
    let existing_certificate = fs::read(&paths.certificate).ok();
    let profile =
        IssuanceProfile::for_action(decided.action, &names, existing_certificate.as_deref(), now)
            .map_err(describe)?;
    let issued = if IssuanceProfile::preserves_key(decided.action) {
        let key = HostKey::from_pem(&read_no_follow(&paths.key)?).map_err(describe)?;
        issue(&profile, IssuerKey::Existing(&key)).map_err(describe)?
    } else {
        issue(&profile, IssuerKey::Generate).map_err(describe)?
    };
    publish_in_transaction(&paths, &issued, now, usize::MAX)?;

    Ok(match decided.action {
        ProvisioningAction::RenewPreservingKey | ProvisioningAction::AdoptAndRenew => format!(
            "reissued TLS certificate at {} over the existing key; existing client pins \
             remain valid\nReload the Pier to serve it: systemctl reload arcen-pier",
            paths.certificate.display()
        ),
        ProvisioningAction::CreateNew => format!(
            "generated TLS certificate at {} and key at {}\n\
             Reload the Pier to serve it: systemctl reload arcen-pier",
            paths.certificate.display(),
            paths.key.display()
        ),
        _ => format!(
            "generated TLS certificate at {} and key at {}\n\
             This replaced the host key. Every Deck that pinned the previous certificate \
             must re-pin before it will connect again.\n\
             Reload the Pier to serve it: systemctl reload arcen-pier",
            paths.certificate.display(),
            paths.key.display()
        ),
    })
}

/// Explains a shared issuance refusal.
fn describe(error: IssuanceError) -> String {
    match error {
        IssuanceError::NoSubjectAltName => "at least one --dns or --ip subject alternative name \
                                            is required; a certificate without one is refused \
                                            by every client"
            .to_owned(),
        other => format!("certificate profile: {other}"),
    }
}

/// Publishes `issued` by the shared transaction. `limit` stops after that
/// many steps, as a crash would; production passes `usize::MAX`.
fn publish_in_transaction(
    paths: &MaterialPaths,
    issued: &IssuedMaterial,
    now: u64,
    limit: usize,
) -> Result<(), String> {
    cert_publication::publish(
        &mut LinuxFiles { paths },
        &Publication::from(issued),
        &transaction_id(std::process::id(), now),
        limit,
    )
    .map_err(|error: PublicationError<String>| error.to_string())
}

/// Refuses a link anywhere in `path`, as the Pier's own TLS loader does.
fn reject_symlink_chain(path: &Path) -> Result<(), String> {
    let mut current = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                return Err(format!("unsafe parent traversal: {}", path.display()));
            }
            Component::CurDir => continue,
            other => current.push(other),
        }
        if fs::symlink_metadata(&current).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
            return Err(format!(
                "refusing symbolic-link path component: {}",
                current.display()
            ));
        }
    }
    Ok(())
}

/// Creates the directory (`0750`) and refuses one that passes through a
/// link, is writable by anyone but its owner, or is owned by another account.
fn prepare_directory(directory: &Path) -> Result<(), String> {
    reject_symlink_chain(directory)?;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o750)
        .create(directory)
        .map_err(|error| format!("create {}: {error}", directory.display()))?;
    reject_symlink_chain(directory)?;
    let metadata = fs::symlink_metadata(directory)
        .map_err(|error| format!("inspect {}: {error}", directory.display()))?;
    if !metadata.is_dir()
        || metadata.uid() != nix::unistd::geteuid().as_raw()
        || metadata.mode() & 0o022 != 0
    {
        return Err(format!(
            "TLS directory {} must be owned by the invoking account and not group/other \
             writable (owner {}, mode {:o})",
            directory.display(),
            metadata.uid(),
            metadata.mode() & 0o7777
        ));
    }
    Ok(())
}

/// An exclusive, non-blocking `flock` on the directory's lock file, held
/// for the whole transaction and released on drop or process exit.
#[derive(Debug)]
struct DirectoryLock {
    _file: fs::File,
}

impl DirectoryLock {
    fn acquire(directory: &Path) -> Result<Self, String> {
        let path = directory.join(LOCK_FILE);
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC)
            .open(&path)
            .map_err(|error| format!("open {}: {error}", path.display()))?;
        if !file.metadata().is_ok_and(|metadata| metadata.is_file()) {
            return Err(format!("unsafe TLS transaction lock {}", path.display()));
        }
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|error| format!("chmod {}: {error}", path.display()))?;
        rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive).map_err(
            |error| {
                if error == rustix::io::Errno::WOULDBLOCK {
                    "another host-certificate transaction is active".to_owned()
                } else {
                    format!("lock {}: {error}", path.display())
                }
            },
        )?;
        Ok(Self { _file: file })
    }
}

/// Where the replacement for `target` is staged.
fn staging_for(target: &Path) -> PathBuf {
    let name = target.file_name().map_or_else(
        || "material".to_owned(),
        |name| name.to_string_lossy().into_owned(),
    );
    target.with_file_name(format!(".{name}.staging"))
}

/// Where a backup is copied before it is complete.
fn partial_backup(backup: &Path) -> PathBuf {
    let mut name = backup.as_os_str().to_owned();
    name.push(".partial");
    PathBuf::from(name)
}

const fn mode_for(secret: bool) -> u32 {
    if secret {
        0o600
    } else {
        0o644
    }
}

fn is_regular_file(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_file())
}

fn sync_directory(path: &Path) -> Result<(), String> {
    let directory = path.parent().unwrap_or(path);
    fs::File::open(directory)
        .and_then(|file| file.sync_all())
        .map_err(|error| format!("sync {}: {error}", directory.display()))
}

fn remove_if_present(path: &Path) -> Result<(), String> {
    match fs::remove_file(path) {
        Ok(()) => sync_directory(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("remove {}: {error}", path.display())),
    }
}

fn read_no_follow(path: &Path) -> Result<Zeroizing<Vec<u8>>, String> {
    use std::io::Read as _;

    let mut file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC)
        .open(path)
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    let mut bytes = Zeroizing::new(Vec::new());
    file.read_to_end(&mut bytes)
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    Ok(bytes)
}

/// Writes `bytes` to a fresh file at `path` (never through a link) with
/// `mode` from the start, and syncs it.
fn write_new(path: &Path, bytes: &[u8], mode: u32) -> Result<(), String> {
    remove_if_present(path)?;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC)
        .open(path)
        .map_err(|error| format!("create {}: {error}", path.display()))?;
    // The umask may have narrowed the mode, never widened it; set it exactly.
    file.set_permissions(fs::Permissions::from_mode(mode))
        .map_err(|error| format!("chmod {}: {error}", path.display()))?;
    file.write_all(bytes)
        .map_err(|error| format!("write {}: {error}", path.display()))?;
    file.sync_all()
        .map_err(|error| format!("sync {}: {error}", path.display()))
}

fn rename(from: &Path, to: &Path) -> Result<(), String> {
    fs::rename(from, to)
        .map_err(|error| format!("rename {} to {}: {error}", from.display(), to.display()))?;
    sync_directory(to)
}

/// The Linux file operations behind the shared transaction. A backup is a
/// copy made under a partial name, synced, then renamed, so the served file
/// stays in place until the rename that replaces it and no backup is ever
/// seen incomplete.
struct LinuxFiles<'a> {
    paths: &'a MaterialPaths,
}

impl LinuxFiles<'_> {
    fn path(&self, name: &str) -> Result<&PathBuf, String> {
        MANAGED_FILES
            .iter()
            .position(|managed| *managed == name)
            .map(|index| self.paths.managed()[index])
            .ok_or_else(|| format!("unmanaged file {name}"))
    }

    fn in_directory(&self, name: &str) -> Result<PathBuf, String> {
        let path = self.paths.directory.join(name);
        if fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
            return Err(format!(
                "refusing symbolic-link path component: {}",
                path.display()
            ));
        }
        Ok(path)
    }
}

impl PublicationFiles for LinuxFiles<'_> {
    type Error = String;

    fn stage(&mut self, name: &'static str, bytes: &[u8], secret: bool) -> Result<(), String> {
        write_new(&staging_for(self.path(name)?), bytes, mode_for(secret))
    }

    fn read(&mut self, name: &'static str, slot: Slot) -> Result<Zeroizing<Vec<u8>>, String> {
        let path = self.path(name)?;
        match slot {
            Slot::Published => read_no_follow(path),
            Slot::Staged => read_no_follow(&staging_for(path)),
        }
    }

    fn exists(&mut self, name: &'static str) -> Result<bool, String> {
        Ok(is_regular_file(self.path(name)?))
    }

    fn back_up(&mut self, name: &'static str, backup: &str) -> Result<(), String> {
        let backup = self.in_directory(backup)?;
        let partial = partial_backup(&backup);
        let source = read_no_follow(self.path(name)?)?;
        let mode = fs::symlink_metadata(self.path(name)?)
            .map_err(|error| format!("inspect {name}: {error}"))?
            .mode()
            & 0o777;
        write_new(&partial, &source, mode)?;
        rename(&partial, &backup)
    }

    fn backup_exists(&mut self, backup: &str) -> Result<bool, String> {
        Ok(is_regular_file(&self.in_directory(backup)?))
    }

    fn publish(&mut self, name: &'static str, secret: bool) -> Result<(), String> {
        let path = self.path(name)?;
        rename(&staging_for(path), path)?;
        fs::set_permissions(path, fs::Permissions::from_mode(mode_for(secret)))
            .map_err(|error| format!("chmod {}: {error}", path.display()))
    }

    fn restore(&mut self, name: &'static str, backup: &str) -> Result<(), String> {
        let backup = self.in_directory(backup)?;
        rename(&backup, self.path(name)?)
    }

    fn remove(&mut self, name: &'static str) -> Result<(), String> {
        remove_if_present(self.path(name)?)
    }

    fn remove_backup(&mut self, backup: &str) -> Result<(), String> {
        let backup = self.in_directory(backup)?;
        remove_if_present(&backup)?;
        remove_if_present(&partial_backup(&backup))
    }

    fn discard_staging(&mut self) -> Result<(), String> {
        for path in self.paths.managed() {
            remove_if_present(&staging_for(path))?;
        }
        Ok(())
    }

    fn write_journal(&mut self, text: &str) -> Result<(), String> {
        let journal = self.in_directory(JOURNAL_FILE)?;
        let staged = staging_for(&journal);
        write_new(&staged, text.as_bytes(), 0o600)?;
        rename(&staged, &journal)
    }

    fn read_journal(&mut self) -> Result<Option<String>, String> {
        let journal = self.in_directory(JOURNAL_FILE)?;
        if !is_regular_file(&journal) {
            return Ok(None);
        }
        String::from_utf8(read_no_follow(&journal)?.to_vec())
            .map(Some)
            .map_err(|_| "certificate journal is not text".to_owned())
    }

    fn remove_journal(&mut self) -> Result<(), String> {
        remove_if_present(&self.in_directory(JOURNAL_FILE)?)
    }
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
            mode @ ("--renew" | "--new-key") => {
                let chosen = if mode == "--renew" {
                    ProvisioningRequest::Renew
                } else {
                    ProvisioningRequest::Rekey
                };
                if request != ProvisioningRequest::Ensure && request != chosen {
                    return Err("choose only one of --renew and --new-key".to_owned());
                }
                request = chosen;
            }
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

#[cfg(test)]
mod tests {
    use arcen_transport::cert_provisioning::MaterialState;

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
    fn renewal_and_rekey_cannot_both_be_asked_for() {
        assert!(parse_arguments(&["--renew".to_owned(), "--new-key".to_owned()]).is_err());
        let (_, request, _) =
            parse_arguments(&["--renew".to_owned(), "--adopt-legacy".to_owned()]).expect("valid");
        assert_eq!(request, ProvisioningRequest::AdoptLegacy);
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

    fn scratch(name: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "arcen-linux-host-cert-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&directory);
        directory
    }

    fn arguments(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    const NOW: u64 = 1_800_000_000;

    fn nothing() -> Discovered {
        Discovered::default()
    }

    /// Every file in `directory` but the lock and inert staging, with its
    /// bytes and mode.
    fn snapshot(directory: &Path) -> std::collections::BTreeMap<String, (Vec<u8>, u32)> {
        fs::read_dir(directory)
            .expect("read dir")
            .map(|entry| entry.expect("entry"))
            .filter(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                name != LOCK_FILE && !name.ends_with(".staging")
            })
            .map(|entry| {
                (
                    entry.file_name().to_string_lossy().into_owned(),
                    (
                        fs::read(entry.path()).expect("read"),
                        entry.metadata().expect("metadata").mode() & 0o777,
                    ),
                )
            })
            .collect()
    }

    fn create(directory: &Path) {
        run_with(
            &arguments(&[
                "--directory",
                directory.to_str().expect("path"),
                "--dns",
                "pier.example.internal",
                "--ip",
                "203.0.113.7",
            ]),
            nothing,
            NOW,
        )
        .expect("create");
    }

    #[test]
    fn issuing_without_a_name_is_refused_before_anything_is_written() {
        // The failure an operator must see is "you did not name the host",
        // not a Deck refusing to connect a week later.
        let directory = scratch("no-name");
        let error = run_with(
            &arguments(&["--directory", directory.to_str().expect("path")]),
            nothing,
            NOW,
        )
        .expect_err("no names must be refused");
        assert!(
            error.contains("subject alternative name"),
            "the refusal must say what is missing: {error}"
        );
        let paths = MaterialPaths::in_directory(&directory);
        assert!(!paths.certificate.exists() && !paths.key.exists());
        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn missing_kinds_of_name_are_discovered_given_ones_are_kept() {
        let discovered = Discovered {
            fqdn: Some("pier.example.internal".to_owned()),
            addresses: vec!["203.0.113.7".to_owned()],
        };
        assert_eq!(
            names_for(&[], &discovered),
            arguments(&["DNS:pier.example.internal", "IP:203.0.113.7"])
        );
        assert_eq!(
            names_for(&arguments(&["DNS:alias.example.internal"]), &discovered),
            arguments(&["DNS:alias.example.internal", "IP:203.0.113.7"])
        );
        assert_eq!(
            names_for(&arguments(&["IP:198.51.100.4"]), &discovered),
            arguments(&["DNS:pier.example.internal", "IP:198.51.100.4"])
        );
    }

    /// A pair as the earlier `openssl` helper deployed it: SEC1 key, the
    /// shared profile, the pins and the ownership marker.
    fn deploy_openssl_pair(directory: &Path) {
        let paths = MaterialPaths::in_directory(directory);
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o750)
            .create(directory)
            .expect("directory");
        let status = std::process::Command::new("openssl")
            .args([
                "ecparam",
                "-name",
                "prime256v1",
                "-genkey",
                "-noout",
                "-out",
            ])
            .arg(&paths.key)
            .status()
            .expect("openssl");
        assert!(status.success());
        fs::set_permissions(&paths.key, fs::Permissions::from_mode(0o600)).expect("mode");
        let profile = IssuanceProfile::new(
            &arguments(&["DNS:pier.example.internal", "IP:203.0.113.7"]),
            NOW,
        )
        .expect("profile");
        let status = std::process::Command::new("openssl")
            .args(["req", "-x509", "-new", "-key"])
            .arg(&paths.key)
            .arg("-out")
            .arg(&paths.certificate)
            .args(profile.openssl_req_arguments())
            .status()
            .expect("openssl");
        assert!(status.success());
        let pins =
            cert_store::PinFiles::for_certificate(&fs::read(&paths.certificate).expect("cert"))
                .expect("pins");
        for (path, text) in pins.files(&paths) {
            fs::write(path, text).expect("pin");
        }
    }

    #[test]
    fn renewal_keeps_the_sec1_key_its_pin_and_every_name() {
        let directory = scratch("renew");
        deploy_openssl_pair(&directory);
        let paths = MaterialPaths::in_directory(&directory);
        let key_before = fs::read(&paths.key).expect("key");
        let label = ["EC", "PRIVATE", "KEY"].join(" ");
        assert!(
            String::from_utf8_lossy(&key_before).contains(&label),
            "SEC1"
        );
        let spki_before = fs::read_to_string(&paths.spki_pin).expect("spki pin");
        let certificate_before = fs::read(&paths.certificate).expect("certificate");

        // Renewal naming only the host: the address must survive.
        run_with(
            &arguments(&[
                "--directory",
                directory.to_str().expect("path"),
                "--renew",
                "--dns",
                "pier.example.internal",
            ]),
            nothing,
            NOW + 86_400,
        )
        .expect("renew");
        assert_eq!(
            fs::read(&paths.key).expect("key"),
            key_before,
            "key unchanged"
        );
        assert_eq!(
            fs::read_to_string(&paths.spki_pin).expect("spki pin"),
            spki_before,
            "SPKI pin unchanged"
        );
        let certificate = fs::read(&paths.certificate).expect("certificate");
        assert_ne!(certificate, certificate_before);
        assert!(arcen_transport::cert_marker::subject_alt_names_match_pem(
            &certificate,
            &arguments(&["DNS:pier.example.internal", "IP:203.0.113.7"])
        ));
        let _ = fs::remove_dir_all(&directory);
    }

    fn reissue(directory: &Path, rekey: bool, now: u64) -> IssuedMaterial {
        let paths = MaterialPaths::in_directory(directory);
        let existing = fs::read(&paths.certificate).expect("certificate");
        let action = if rekey {
            ProvisioningAction::ReplaceKeyAndCertificate
        } else {
            ProvisioningAction::RenewPreservingKey
        };
        let profile = IssuanceProfile::for_action(
            action,
            &arguments(&["DNS:pier.example.internal"]),
            Some(&existing),
            now,
        )
        .expect("profile");
        if rekey {
            issue(&profile, IssuerKey::Generate).expect("issue")
        } else {
            let key = HostKey::from_pem(&fs::read(&paths.key).expect("key")).expect("key");
            issue(&profile, IssuerKey::Existing(&key)).expect("issue")
        }
    }

    #[test]
    fn a_crash_at_any_step_recovers_to_the_old_or_the_new_material_whole() {
        for rekey in [false, true] {
            let mut saw = (false, false);
            for limit in 0.. {
                let directory = scratch(&format!("crash-{rekey}-{limit}"));
                deploy_openssl_pair(&directory);
                let before = snapshot(&directory);
                let issued = reissue(&directory, rekey, NOW + 60);
                let paths = MaterialPaths::in_directory(&directory);
                publish_in_transaction(&paths, &issued, NOW + 60, limit).expect("stopped run");
                // The next start recovers before anything else.
                run_with(
                    &arguments(&["--directory", directory.to_str().expect("path")]),
                    nothing,
                    NOW + 120,
                )
                .expect("next run");
                let after = snapshot(&directory);
                let _ = fs::remove_dir_all(&directory);
                if after == before {
                    saw.0 = true;
                } else {
                    assert_eq!(
                        after.get("host.crt").map(|(bytes, _)| bytes.as_slice()),
                        Some(issued.certificate_pem.as_bytes()),
                        "rekey={rekey}: crash after {limit} steps left a mix"
                    );
                    assert_eq!(
                        after.len(),
                        MANAGED_FILES.len(),
                        "debris: {:?}",
                        after.keys()
                    );
                    assert_eq!(after["host.key"].1, 0o600);
                    if !rekey {
                        assert_eq!(after.get("host.key"), before.get("host.key"));
                    }
                    saw.1 = true;
                    if limit > 20 {
                        break;
                    }
                }
            }
            assert!(saw.0 && saw.1, "rekey={rekey}");
        }
    }

    #[test]
    fn an_interrupted_helper_transaction_is_recovered_before_anything_else() {
        // What the earlier helper (or the installer) leaves when it dies
        // after moving the old files aside: a prepared journal, the backups,
        // and a half-published directory. The next run puts the old pair back.
        let directory = scratch("helper-journal");
        deploy_openssl_pair(&directory);
        let before = snapshot(&directory);
        let id = "4242-1800000000";
        let journal = arcen_transport::cert_transaction::TransactionJournal::new(
            id,
            arcen_transport::cert_transaction::TransactionPhase::Prepared,
            [true; MANAGED_FILES.len()],
        );
        fs::write(directory.join(JOURNAL_FILE), journal.render()).expect("journal");
        for name in ["host.crt", "host.cert-sha256"] {
            fs::rename(
                directory.join(name),
                directory.join(arcen_transport::cert_transaction::backup_file_name(
                    id, name,
                )),
            )
            .expect("back up");
        }
        fs::write(directory.join("host.crt"), b"half written").expect("half");

        let outcome = run_with(
            &arguments(&["--directory", directory.to_str().expect("path")]),
            nothing,
            NOW + 60,
        )
        .expect("recovers and keeps");
        assert!(outcome.contains("already present"), "{outcome}");
        assert_eq!(snapshot(&directory), before);
        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_second_concurrent_run_refuses_and_changes_nothing() {
        let directory = scratch("locked");
        create(&directory);
        let before = snapshot(&directory);
        let held = DirectoryLock::acquire(&directory).expect("first run's lock");
        let error = run_with(
            &arguments(&[
                "--directory",
                directory.to_str().expect("path"),
                "--new-key",
            ]),
            nothing,
            NOW + 60,
        )
        .expect_err("busy");
        assert!(
            error.contains("another host-certificate transaction is active"),
            "{error}"
        );
        assert_eq!(snapshot(&directory), before);
        drop(held);
        run_with(
            &arguments(&[
                "--directory",
                directory.to_str().expect("path"),
                "--new-key",
                "--dns",
                "pier.example.internal",
            ]),
            nothing,
            NOW + 60,
        )
        .expect("runs once the lock is free");
        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_shared_or_linked_directory_is_refused() {
        let directory = scratch("shared");
        fs::create_dir_all(&directory).expect("directory");
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o770)).expect("chmod");
        let error = run_with(
            &arguments(&[
                "--directory",
                directory.to_str().expect("path"),
                "--dns",
                "x.example",
            ]),
            nothing,
            NOW,
        )
        .expect_err("group-writable");
        assert!(error.contains("not group/other writable"), "{error}");

        fs::set_permissions(&directory, fs::Permissions::from_mode(0o750)).expect("chmod");
        let linked = scratch("linked");
        std::os::unix::fs::symlink(&directory, &linked).expect("link");
        let error = run_with(
            &arguments(&[
                "--directory",
                linked.to_str().expect("path"),
                "--dns",
                "x.example",
            ]),
            nothing,
            NOW,
        )
        .expect_err("linked");
        assert!(error.contains("symbolic-link"), "{error}");
        let _ = fs::remove_file(&linked);
        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn created_material_is_the_shared_profile_with_private_modes() {
        let directory = scratch("modes");
        create(&directory);
        let paths = MaterialPaths::in_directory(&directory);
        for (path, mode) in [
            (&paths.key, 0o600),
            (&paths.certificate, 0o644),
            (&paths.certificate_pin, 0o644),
            (&paths.spki_pin, 0o644),
            (&paths.marker, 0o644),
            (&directory.join(LOCK_FILE), 0o600),
        ] {
            assert_eq!(
                fs::metadata(path).expect("metadata").mode() & 0o777,
                mode,
                "{}",
                path.display()
            );
        }
        let key = HostKey::from_pem(&fs::read(&paths.key).expect("key")).expect("key");
        IssuanceProfile::new(
            &arguments(&["DNS:pier.example.internal", "IP:203.0.113.7"]),
            NOW,
        )
        .expect("profile")
        .verify_issued(
            &fs::read(&paths.certificate).expect("cert"),
            key.spki_sha256(),
        )
        .expect("shared profile, backdated");
        assert!(!directory.join(JOURNAL_FILE).exists());
        let _ = fs::remove_dir_all(&directory);
    }
}
