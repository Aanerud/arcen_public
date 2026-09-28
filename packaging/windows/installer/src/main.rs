#[cfg(not(windows))]
fn main() {
    eprintln!("install-arcen-pier is Windows-only");
    std::process::exit(1);
}

/// Not `#[cfg(windows)]`: the ACL rules decide who can read the Pier's private
/// key, so they are kept unit-testable on every host.
#[cfg_attr(not(windows), allow(dead_code))]
mod acl;

/// Not `#[cfg(windows)]` for the same reason as `acl`: this rule decides
/// whether an operator's configuration is preserved or replaced.
#[cfg_attr(not(windows), allow(dead_code))]
mod diagnosis;
#[cfg_attr(not(windows), allow(dead_code))]
mod service_outcome;

/// Not `#[cfg(windows)]` for the same reason as `acl`: uninstall must not
/// delete a binary whose service is still starting.
#[cfg_attr(not(windows), allow(dead_code))]
mod scm_state;

#[cfg(windows)]
#[path = "../../../quic_config_migration.rs"]
mod quic_config_migration;

#[cfg(windows)]
fn main() {
    if let Err(error) = windows_main() {
        eprintln!("install-arcen-pier failed: {error}");
        std::process::exit(1);
    }
}

#[cfg(windows)]
mod imp {
    use std::ffi::OsStr;
    use std::fs;
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use arcen_transport::cert_marker::{self, OwnershipMarker};
    use arcen_transport::cert_provisioning::{
        MaterialOwnership, MaterialState, ProvisioningAction, ProvisioningRefusal,
        ProvisioningRequest, plan,
    };
    use rcgen::{CertificateParams, ExtendedKeyUsagePurpose, KeyPair, KeyUsagePurpose};
    use time::{Duration, OffsetDateTime};

    use crate::acl::{AclClass, OWNER_SID, assert_acl_sddl, unexpected_trustees};
    use crate::diagnosis::is_tls_failure;
    use crate::scm_state::{ScmState, parse_state};
    use crate::service_outcome::{ServiceOutcome, finish_install};
    use arcen_session::install_lifecycle::{InstallEvent, InstallTransaction};

    /// Set by `--verbose`. Off by default, so an administrator sees what
    /// changed, not every icacls and reg.exe line behind it.
    static VERBOSE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    fn verbose() -> bool {
        VERBOSE.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Prints only with `--verbose`.
    macro_rules! detail {
        ($($arg:tt)*) => {
            if verbose() {
                println!($($arg)*);
            }
        };
    }

    const PIER_BYTES: &[u8] = include_bytes!(env!("ARCEN_EMBED_PIER_EXE"));
    const CP_BYTES: &[u8] = include_bytes!(env!("ARCEN_EMBED_CP_DLL"));
    /// Canonical location of the corresponding source.
    ///
    /// The installer is a distributed binary of an AGPL-3.0 work, so the offer
    /// belongs in it too, not only in the Pier it installs.
    const SOURCE_URL: &str = "https://github.com/Aanerud/arcen_public";
    /// AGPL-3.0 section 13 source offer, surfaced by `--version`.
    const SOURCE_OFFER: &str = "Arcen is free software under the GNU AGPL-3.0. It comes with ABSOLUTELY NO WARRANTY. \
         You may redistribute it under the terms of that licence. If you run a modified version \
         that others connect to over a network, you must offer them its corresponding source.";
    /// Lifetime of a self-signed Pier certificate, in days.
    ///
    /// Matches `packaging/linux/new-host-cert.sh` and
    /// `hosts/windows/scripts/new-host-cert.ps1`. 825 days is the CA/Browser
    /// Forum maximum that public clients accept, and keeping all three
    /// generators on one number means an operator sees the same renewal cadence
    /// whichever produced their certificate.
    const CERTIFICATE_VALIDITY_DAYS: i64 = 825;
    const DEFAULT_CONFIG: &str = include_str!("../../pier.json");
    const EVENTLOG_SOURCE_SCRIPT: &str = include_str!("../../host/eventlog-source.ps1");
    /// Shipped with the binary rather than kept only in the repository, so a
    /// sysadmin can tune the Pier on the host and so the third-party notices
    /// travel with what they describe.
    const ADMIN_GUIDE: &str = include_str!("../../../../docs/operations/pier-administration.md");
    const THIRD_PARTY_NOTICES: &str = include_str!("../../../../legal/THIRD_PARTY_NOTICES.md");
    const SERVICE_NAME: &str = "ArcenPier";
    const CP_DLL: &str = "arcen_credential_provider.dll";
    const EVENTLOG_SOURCE_SCRIPT_NAME: &str = "eventlog-source.ps1";
    const CLSID: &str = "{2FBE34F2-9E7A-42FA-BFBF-44897694BE60}";
    const PROVIDER_NAME: &str = "Arcen Credential Provider";
    const THREADING_MODEL: &str = "Apartment";
    #[derive(Debug)]
    struct Options {
        prefix: PathBuf,
        programdata: PathBuf,
        dry_run: bool,
        uninstall: bool,
        purge: bool,
        version: bool,
        force: bool,
        /// Take ownership of a certificate an earlier installer left without
        /// an ownership marker, keeping its key so paired Decks still trust it.
        adopt_legacy: bool,
        service_name: String,
        /// Extra names or addresses to place in the generated TLS certificate.
        ///
        /// The certificate is otherwise built from what the machine can see of
        /// itself, and a host published through NAT or a firewall cannot see
        /// the address a Deck actually dials. The admin knows it; nothing on
        /// the host does.
        extra_sans: Vec<String>,
    }

    impl Options {
        fn parse() -> Result<Self, String> {
            let mut opts = Self {
                prefix: PathBuf::from(r"C:\Program Files\Arcen\Pier"),
                programdata: std::env::var_os("ProgramData")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
                    .join("Arcen"),
                dry_run: false,
                uninstall: false,
                purge: false,
                version: false,
                force: false,
                adopt_legacy: false,
                service_name: SERVICE_NAME.to_string(),
                extra_sans: Vec::new(),
            };
            let mut args = std::env::args().skip(1);
            while let Some(arg) = args.next() {
                match arg.as_str() {
                    "--prefix" => {
                        opts.prefix =
                            PathBuf::from(args.next().ok_or("--prefix requires a directory")?)
                    }
                    "--programdata" => {
                        opts.programdata =
                            PathBuf::from(args.next().ok_or("--programdata requires a directory")?)
                    }
                    "--dry-run" => opts.dry_run = true,
                    "--uninstall" => opts.uninstall = true,
                    "--purge" => opts.purge = true,
                    "--version" => opts.version = true,
                    "--force" => opts.force = true,
                    "--adopt-legacy" => opts.adopt_legacy = true,
                    "--verbose" | "-v" => {
                        VERBOSE.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                    "--service-name" => {
                        opts.service_name = args.next().ok_or("--service-name requires a name")?
                    }
                    "--extra-san" => {
                        let value = args
                            .next()
                            .ok_or("--extra-san requires a DNS name or IP address")?;
                        for entry in value.split(',') {
                            let entry = entry.trim();
                            if !entry.is_empty() {
                                opts.extra_sans.push(entry.to_ascii_lowercase());
                            }
                        }
                    }
                    "-h" | "--help" => {
                        print_usage();
                        std::process::exit(0);
                    }
                    other => return Err(format!("unknown argument: {other}")),
                }
            }
            if opts.adopt_legacy && opts.force {
                return Err(
                    "--adopt-legacy keeps the existing key and --force replaces it; \
                     pass one of them"
                        .to_owned(),
                );
            }
            if opts.adopt_legacy && opts.uninstall {
                return Err("--adopt-legacy applies to an install, not --uninstall".to_owned());
            }
            Ok(opts)
        }

        fn staging(&self) -> bool {
            self.service_name != SERVICE_NAME
                || self.prefix != PathBuf::from(r"C:\Program Files\Arcen\Pier")
                || self.programdata
                    != std::env::var_os("ProgramData")
                        .map(PathBuf::from)
                        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
                        .join("Arcen")
        }
    }

    pub(super) fn run() -> Result<(), String> {
        let opts = Options::parse()?;
        if opts.version {
            println!("install-arcen-pier {}", env!("CARGO_PKG_VERSION"));
            println!("{SOURCE_OFFER}");
            println!("Source: {SOURCE_URL}");
            return Ok(());
        }
        if !opts.dry_run {
            require_elevated()?;
        }
        if opts.uninstall {
            uninstall(&opts)
        } else {
            install(&opts)
        }
    }

    fn print_usage() {
        println!(
            "USAGE: install-arcen-pier [--prefix <dir>] [--programdata <dir>] [--dry-run]\n\
             \x20                        [--uninstall] [--purge] [--version]\n\
             \x20                        [--force | --adopt-legacy] [--verbose]\n\
             \x20                        [--service-name <name>] [--extra-san <name-or-ip>]\n\
             \n\
             --adopt-legacy  Not needed any more: a self-signed key pair an older Arcen\n\
             \x20            install left behind is taken over automatically, keeping the\n\
             \x20            key so paired Decks keep trusting the host. Accepted so\n\
             \x20            existing scripts keep working.\n\
             --force      Replace the TLS key and certificate. Every paired Deck must\n\
             \x20            re-pin the host.\n\
             --verbose    Show every command and access-control check as it runs.\n\
             \n\
             --extra-san  Add a DNS name or IP address to the generated TLS certificate.\n\
             \x20            Repeatable, or comma-separated. Use this when the host is\n\
             \x20            reached through NAT or a firewall: the certificate is built\n\
             \x20            from what the machine can see of itself, and it cannot see\n\
             \x20            the public address a Deck dials. Without it the Deck reports\n\
             \x20            \"certificate not valid for name ...\".\n\
             \x20            Only affects a certificate being generated, so pass --force\n\
             \x20            to replace one that already exists."
        );
    }

    fn require_elevated() -> Result<(), String> {
        // `net session` prints "There are no entries in the list" in the
        // console's language; only its exit status matters here.
        let status = Command::new("net")
            .arg("session")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map_err(|e| format!("check Administrator elevation: {e}"))?;
        if status.success() {
            Ok(())
        } else {
            Err("Administrator elevation is required: net session returned access denied; rerun from an elevated console".to_string())
        }
    }

    fn install(opts: &Options) -> Result<(), String> {
        println!(
            "Installing Arcen Pier {} into {} (data in {})",
            env!("CARGO_PKG_VERSION"),
            opts.prefix.display(),
            opts.programdata.display()
        );
        // Windows will not let anyone replace a running executable, so an
        // upgrade must stop the service first. Refusing and telling the
        // operator to do it by hand made every upgrade fail for anyone who did
        // not already know the service name — and this installer is aimed at
        // people who do not run services for a living. Stop it here, once, and
        // put it back at the end exactly as it was found.
        //
        // Deliberately before the first `create_dir`: the guard that produced
        // "refusing to replace ... without stopping it first" fires deep inside
        // the file writes, after directories and ACLs have already been
        // changed, which left a half-configured machine behind every time.
        let restart_service = if opts.dry_run {
            let running = service_running(&opts.service_name).unwrap_or(false);
            if running {
                println!(
                    "dry-run: stop service {} for the upgrade, then start it again",
                    opts.service_name
                );
            }
            false
        } else if service_running(&opts.service_name)? {
            println!(
                "service {} is running; stopping it to replace the installed files",
                opts.service_name
            );
            println!("any connected session will be disconnected");
            stop_service_and_wait(opts)?;
            true
        } else {
            false
        };
        let outcome = install_files(opts);
        // A successful install starts the service itself, at the end of
        // `install_files`. This only has to repair the failure case: leaving a
        // machine that was serving sessions with a stopped service, because an
        // unrelated later step failed, is a worse outcome than the failure.
        if restart_service && outcome.is_err() {
            println!(
                "install failed; restarting service {} as it was found",
                opts.service_name
            );
            match start_service(opts) {
                Ok(ServiceOutcome::Running | ServiceOutcome::NotRequested) => {}
                Ok(ServiceOutcome::NotRunning(state)) => println!(
                    "warning: {} did not come back after the failed install (state: {state})",
                    opts.service_name
                ),
                Err(error) => {
                    println!("warning: could not restart {}: {error}", opts.service_name);
                }
            }
        }
        outcome
    }

    fn install_files(opts: &Options) -> Result<(), String> {
        // Elevation and argument checks already passed in `windows_main`.
        let mut transaction = InstallTransaction::new();
        transaction
            .apply(InstallEvent::PreflightPassed)
            .map_err(|error| format!("installer transaction: {error}"))?;
        remove_set_aside_credential_providers(opts);
        let logs = opts.programdata.join("logs");
        let sessions = logs.join("sessions");
        let runtime = opts.programdata.join("runtime");
        let tls = opts.programdata.join("tls");
        let rollback = opts.programdata.join("rollback");
        // Crash-recovery journals (time zone, display) live here. Nothing
        // created it, so time-zone redirection failed on every session.
        let recovery = opts.programdata.join("recovery");
        for dir in [
            &opts.prefix,
            &opts.programdata,
            &logs,
            &sessions,
            &runtime,
            &tls,
            &rollback,
            &recovery,
        ] {
            create_dir(opts, dir)?;
        }
        // The Arcen root carries a protected DACL with exactly two entries,
        // SYSTEM and Administrators. Sub-directories the session agent must
        // write, such as runtime, carry their own explicit grant and remain
        // reachable because Windows gives Everyone bypass-traverse-checking by
        // default.
        apply_secret_dir_acl(opts, &opts.programdata)?;
        for dir in [&logs, &sessions, &rollback, &recovery] {
            apply_secret_dir_acl(opts, dir)?;
        }
        // The install prefix holds arcen-pier.exe and, below,
        // arcen_credential_provider.dll — the DLL registered under
        // HKLM\...\Credential Providers and loaded by LogonUI as SYSTEM.
        //
        // Both files already get a protected DACL of their own when written.
        // That is not sufficient on its own: FILE_DELETE_CHILD on the
        // *directory* lets a caller delete and replace a file regardless of the
        // file's ACL. With a default --prefix under Program Files the inherited
        // ACL happens to be safe, but --prefix is operator-supplied, and a
        // directory created fresh under, say, C:\ inherits the root's
        // inherit-only "Authenticated Users: Modify" ACE. On such a deployment
        // any local user could swap the DLL and get SYSTEM code execution on
        // the secure desktop at the next lock screen.
        //
        // AclClass::PublicDirectory and its helper existed and were unit-tested
        // but were called from nowhere, so the protection was designed and then
        // never applied.
        apply_public_dir_acl(opts, &opts.prefix)?;
        // The per-session agent writes the display-recovery journal here under
        // the user's unelevated token. Treating it as secret made every session
        // fail with "create display recovery journal ...: Access is denied".
        apply_acl(opts, &runtime, AclClass::AgentWritableDirectory)?;
        for dir in [&tls] {
            apply_secret_dir_acl(opts, dir)?;
        }
        let pier_path = opts.prefix.join("arcen-pier.exe");
        atomic_write(opts, &pier_path, PIER_BYTES, true)?;
        let config = opts.programdata.join("pier.json");
        let kept_config = config.exists();
        if kept_config {
            println!("keeping existing config: {}", config.display());
            migrate_existing_config(opts, &config)?;
        } else {
            atomic_write(opts, &config, DEFAULT_CONFIG.as_bytes(), false)?;
            detail!(
                "wrote the packaged default config; multi-monitor selection is resolved at Pier startup"
            );
        }
        apply_secret_file_acl(opts, &config)?;
        ensure_tls(opts, &tls)?;
        verify_acl(opts, &tls.join("host.key"), AclClass::SecretFile)?;
        if kept_config {
            // A kept config is not necessarily a config this binary can read.
            // The QUIC migration only rewrites transport keys, so a file
            // written before a later field was added still parses as invalid,
            // and nothing noticed until the service failed to start long after
            // the install reported success.
            //
            // Deliberately after `ensure_tls`. `validate-config --schema-only`
            // loads the TLS material before it honours the flag, so running it
            // first judged a perfectly good config unreadable whenever the
            // certificate, the key, or that key's ACL needed the repair
            // `ensure_tls` was about to perform anyway.
            validate_kept_config(opts, &pier_path, &config)?;
        }
        atomic_write(opts, &opts.prefix.join(CP_DLL), CP_BYTES, true)?;
        atomic_write(
            opts,
            &opts.programdata.join("pier-administration.md"),
            ADMIN_GUIDE.as_bytes(),
            false,
        )?;
        atomic_write(
            opts,
            &opts.programdata.join("THIRD_PARTY_NOTICES.md"),
            THIRD_PARTY_NOTICES.as_bytes(),
            false,
        )?;
        let eventlog_script = opts.programdata.join(EVENTLOG_SOURCE_SCRIPT_NAME);
        atomic_write(
            opts,
            &eventlog_script,
            EVENTLOG_SOURCE_SCRIPT.as_bytes(),
            false,
        )?;
        if let Err(error) = register_eventlog_source(opts, &eventlog_script) {
            eprintln!(
                "warning: ArcenPier Event Log source registration failed; \
                 file logging remains active: {error}"
            );
        }
        transaction
            .apply(InstallEvent::PayloadStaged)
            .map_err(|error| format!("installer transaction: {error}"))?;
        register_service(opts, &config)?;
        register_credential_provider(opts)?;
        open_firewall(opts);
        let outcome = start_service(opts)?;
        if !opts.dry_run && !opts.staging() {
            println!();
            println!(
                "Restart Windows once before the first remote sign-in: the sign-in screen \
                 picks up Arcen's credential provider only when it starts."
            );
            println!(
                "Administration guide: {}",
                opts.programdata.join("pier-administration.md").display()
            );
        }
        finish_install(transaction, &outcome, &opts.service_name)
    }

    /// Proves the kept configuration is one this binary can actually read.
    ///
    /// A config is only preserved because an operator may have tuned it, so an
    /// unreadable one is moved aside and replaced rather than silently kept:
    /// the previous behaviour reported a successful install and then produced a
    /// service that could not start, with the reason visible only in a log.
    ///
    /// `--schema-only` is deliberate. A full validation also resolves the
    /// display adapter, which legitimately finds nothing when this runs without
    /// an attached desktop, and that must not be mistaken for a bad config.
    fn validate_kept_config(opts: &Options, pier: &Path, config: &Path) -> Result<(), String> {
        if opts.dry_run {
            println!("dry-run: would validate {}", config.display());
            return Ok(());
        }
        let output = Command::new(pier)
            .args(["validate-config", "--schema-only", "--config"])
            .arg(config)
            .output()
            .map_err(|error| format!("validate {}: {error}", config.display()))?;
        if output.status.success() {
            return Ok(());
        }
        let reason = String::from_utf8_lossy(&output.stderr);
        let reason = reason
            .lines()
            .find(|line| line.contains("config validation failed") || line.contains("error:"))
            .unwrap_or_else(|| reason.lines().next().unwrap_or("unreadable"))
            .trim();

        // `validate-config` checks TLS before it honours `--schema-only`, so a
        // broken certificate or key fails a config that is in fact perfectly
        // readable. `ensure_tls` has already run and kept whatever material was
        // there, which is correct for operator-supplied certificates, so the
        // answer here is to name the real fault rather than destroy a good
        // config on its behalf.
        if is_tls_failure(reason) {
            println!("existing config was kept; the TLS material is what this build rejects:");
            println!("  {reason}");
            println!(
                "  the service will not start until it is fixed. Replace the pair with \
                 --force (optionally with --extra-san), or install a matching \
                 certificate and key in {}.",
                config.parent().map_or_else(
                    || "the TLS directory".to_string(),
                    |dir| dir.join("tls").display().to_string()
                )
            );
            return Ok(());
        }

        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs());
        let preserved = config.with_extension(format!("json.unreadable-{stamp}"));
        fs::rename(config, &preserved)
            .map_err(|error| format!("preserve {}: {error}", config.display()))?;
        println!("existing config cannot be read by this build: {reason}");
        println!("preserved it as {}", preserved.display());

        atomic_write(opts, config, DEFAULT_CONFIG.as_bytes(), false)?;
        println!("wrote a fresh default config: {}", config.display());
        detail!(
            "wrote the packaged default config; multi-monitor selection is resolved at Pier startup"
        );
        println!("re-apply any settings you had customised, using the preserved copy.");
        Ok(())
    }

    fn migrate_existing_config(opts: &Options, path: &Path) -> Result<(), String> {
        let original =
            fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))?;
        let Some(migrated) = crate::quic_config_migration::migrate_quic_product_config(&original)?
        else {
            return Ok(());
        };
        atomic_write(opts, path, &migrated, false)?;
        println!("migrated {} to QUIC/UDP 18444 and TLS 1.3", path.display());
        Ok(())
    }

    /// Open the Pier's listening port. Best effort: an unrecognised or absent
    /// firewall is not an install failure, but the operator is told.
    const FIREWALL_RULE: &str = "Arcen Pier QUIC 18444";

    /// Deletes every inbound rule with this name; netsh reports "No rules
    /// match" when there is none, which is not an error here.
    fn delete_firewall_rule(name: &str) {
        let _ = Command::new("netsh")
            .args(["advfirewall", "firewall", "delete", "rule"])
            .arg(format!("name={name}"))
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }

    fn open_firewall(opts: &Options) {
        if opts.dry_run || opts.staging() {
            return;
        }
        delete_firewall_rule("Arcen Pier 18443");
        // netsh adds a new rule on every call, so an upgrade or repair that
        // only added one left a duplicate per run.
        delete_firewall_rule(FIREWALL_RULE);
        let status = Command::new("netsh")
            .args([
                "advfirewall",
                "firewall",
                "add",
                "rule",
                &format!("name={FIREWALL_RULE}"),
                "dir=in",
                "action=allow",
                "protocol=UDP",
                "localport=18444",
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        if matches!(status, Ok(status) if status.success()) {
            println!("firewall: opened 18444/udp");
        } else {
            println!("warning: could not open 18444/udp automatically; open it manually");
        }
    }

    /// Bring the service up after installation.
    fn start_service(opts: &Options) -> Result<ServiceOutcome, String> {
        if opts.dry_run || opts.staging() {
            println!("staging or dry-run: service not started");
            return Ok(ServiceOutcome::NotRequested);
        }
        // Capture rather than inherit: sc.exe prints a status block that is
        // noise in an installer transcript.
        let _ = Command::new("sc.exe")
            .arg("start")
            .arg(&opts.service_name)
            .output()
            .map_err(|error| format!("start {}: {error}", opts.service_name))?;
        // Ask the service control manager what actually happened. sc.exe start
        // succeeds once the start is accepted, so reporting on its exit status
        // claims a running service for one that is about to fail.
        // Wait as long as the service control manager itself does before it
        // gives up on a start (30 s by default): a Pier that reads its
        // configuration, certificate and display inventory legitimately takes
        // longer than a few seconds, and reporting that as a failed install
        // is as untruthful as reporting a failed start as success.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let mut state = String::new();
        while std::time::Instant::now() < deadline {
            let query = Command::new("sc.exe")
                .arg("query")
                .arg(&opts.service_name)
                .output()
                .map(|output| String::from_utf8_lossy(&output.stdout).to_string())
                .unwrap_or_default();
            state = if query.contains("RUNNING") {
                "running".to_string()
            } else if query.contains("START_PENDING") {
                "start_pending".to_string()
            } else if query.contains("STOPPED") {
                "stopped".to_string()
            } else {
                "unknown".to_string()
            };
            if state != "start_pending" {
                std::thread::sleep(std::time::Duration::from_millis(500));
                let confirm = Command::new("sc.exe")
                    .arg("query")
                    .arg(&opts.service_name)
                    .output()
                    .map(|output| String::from_utf8_lossy(&output.stdout).contains("RUNNING"))
                    .unwrap_or(false);
                if confirm == (state == "running") {
                    break;
                }
                state = if confirm { "running" } else { "stopped" }.to_string();
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
        if state == "running" {
            println!("service: registered and running");
            Ok(ServiceOutcome::Running)
        } else {
            println!("service: registered but not running (state: {state})");
            Ok(ServiceOutcome::NotRunning(state))
        }
    }

    /// Stop the service and wait for it to actually be gone.
    ///
    /// `sc.exe delete` only marks a service for deletion; a running process
    /// keeps its executable locked, so removing the files afterwards fails with
    /// "Access is denied" and leaves a half-uninstalled machine.
    fn stop_service_and_wait(opts: &Options) -> Result<(), String> {
        if opts.dry_run {
            println!("dry-run: would stop {}", opts.service_name);
            return Ok(());
        }
        let query = |name: &str| {
            Command::new("sc.exe")
                .arg("query")
                .arg(name)
                .output()
                .ok()
                .and_then(|output| parse_state(&String::from_utf8_lossy(&output.stdout)))
        };
        // A service that is still starting refuses a stop request (1052) and
        // does not read as RUNNING, so stopping it straight away deleted a
        // service whose binary was still in use. Wait out the start first,
        // for as long as the SCM itself allows a start to take.
        let mut state = query(&opts.service_name);
        for _ in 0..60 {
            if !state.is_some_and(ScmState::is_transitional) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
            state = query(&opts.service_name);
        }
        if matches!(state, None | Some(ScmState::Stopped)) {
            return Ok(());
        }
        let _ = Command::new("sc.exe")
            .arg("stop")
            .arg(&opts.service_name)
            .stdout(std::process::Stdio::null())
            .status();
        for _ in 0..60 {
            state = query(&opts.service_name);
            if matches!(state, None | Some(ScmState::Stopped)) {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        Err(format!(
            "{} did not stop within 30 seconds (state: {state:?}); stop it and retry",
            opts.service_name
        ))
    }

    fn uninstall(opts: &Options) -> Result<(), String> {
        // Uninstall must tolerate any partial prior state. A previous run that
        // stopped halfway leaves the service already deleted, and treating
        // "service does not exist" as fatal aborts before the files are
        // removed, so retrying can never converge.
        if opts.dry_run || service_exists(&opts.service_name)? {
            stop_service_and_wait(opts)?;
            if opts.dry_run {
                println!("dry-run: sc.exe delete {}", opts.service_name);
            } else {
                let output = Command::new("sc.exe")
                    .arg("delete")
                    .arg(&opts.service_name)
                    .output()
                    .map_err(|error| format!("sc.exe delete: {error}"))?;
                let text = String::from_utf8_lossy(&output.stdout);
                if output.status.success() {
                    println!("service {} deleted", opts.service_name);
                } else if text.contains("1060") {
                    println!("service {} was already absent", opts.service_name);
                } else {
                    return Err(format!(
                        "sc.exe delete {} failed: {}",
                        opts.service_name,
                        text.trim()
                    ));
                }
            }
        } else {
            println!("service {} is not registered", opts.service_name);
        }
        if let Err(error) = unregister_eventlog_source(opts) {
            eprintln!(
                "warning: ArcenPier Event Log source removal failed; preserving the registry \
                 entry and continuing uninstall: {error}"
            );
        }
        if !opts.staging() || opts.force {
            unregister_credential_provider(opts)?;
        } else {
            println!("staging mode: skipped live Credential Provider registry removal");
        }
        if opts.dry_run {
            println!("dry-run: would delete firewall rule {FIREWALL_RULE}");
        } else if !opts.staging() {
            delete_firewall_rule(FIREWALL_RULE);
            println!("firewall: closed 18444/udp ({FIREWALL_RULE})");
        }
        remove_file(opts, &opts.prefix.join("arcen-pier.exe"))?;
        // LogonUI can pin the Credential Provider DLL until the next reboot.
        // That is worth reporting, but it must not cancel the purge: aborting
        // here left ProgramData intact after an explicit --purge, so a stale
        // configuration survived and broke the following install, while the
        // operator had every reason to believe the machine was now clean.
        let credential_provider = remove_credential_provider_file(opts, &opts.prefix.join(CP_DLL));
        if credential_provider.is_ok() {
            if opts.purge {
                remove_installer_leftovers(opts)?;
            }
            remove_dir_if_empty(opts, &opts.prefix)?;
        }
        if opts.purge {
            preserve_config_before_purge(opts)?;
            remove_dir_all(opts, &opts.programdata)?;
        }
        credential_provider
    }

    /// Copy `pier.json` clear of the tree `--purge` is about to delete.
    ///
    /// The configuration is the one thing on a Pier the installer must not
    /// reconstruct. GPU exclusions, monitor layout and transport tuning are
    /// site facts, not product defaults.
    ///
    /// The copy lands beside the purged directory rather than inside it, and
    /// purge still proceeds if it cannot be made: refusing to clean a machine
    /// because a backup failed is worse than the lost file.
    fn preserve_config_before_purge(opts: &Options) -> Result<(), String> {
        let config = opts.programdata.join("pier.json");
        let Some(parent) = opts.programdata.parent() else {
            return Ok(());
        };
        if opts.dry_run {
            if config.exists() {
                println!(
                    "dry-run: preserve {} beside {}",
                    config.display(),
                    parent.display()
                );
            }
            return Ok(());
        }
        if !config.exists() {
            return Ok(());
        }
        let backup = parent.join(format!("arcen-pier.json.purged-{}", timestamp()));
        match fs::copy(&config, &backup) {
            Ok(_) => println!("preserved config before purge: {}", backup.display()),
            Err(e) => println!(
                "warning: could not preserve {} before purge: {e}",
                config.display()
            ),
        }
        Ok(())
    }

    /// Remove the copies the installer itself made in the prefix.
    ///
    /// Upgrades leave `arcen-pier.exe.rollback-<stamp>` and `arcen-pier.exe.new`
    /// beside the binary. Uninstall removed only `arcen-pier.exe`, so
    /// `remove_dir_if_empty` then quietly did nothing and a purged machine kept
    /// a prefix full of old product binaries — on a lab host, seven of them.
    /// An operator who ran `--purge` had every reason to believe the prefix was
    /// gone.
    ///
    /// Only files this installer creates are matched. Anything else in the
    /// prefix was put there by someone else and is left alone, which is also why
    /// the directory removal stays conditional on the prefix then being empty.
    fn remove_installer_leftovers(opts: &Options) -> Result<(), String> {
        let entries = match fs::read_dir(&opts.prefix) {
            Ok(entries) => entries,
            // Already gone is the desired end state.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(format!("read {}: {error}", opts.prefix.display())),
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let ours = name.starts_with("arcen-pier.exe.")
                || name.starts_with("arcen_credential_provider.dll.");
            if ours {
                remove_file(opts, &entry.path())?;
            }
        }
        Ok(())
    }

    fn create_dir(opts: &Options, path: &Path) -> Result<(), String> {
        if opts.dry_run {
            println!("dry-run: create dir {}", path.display());
            return Ok(());
        }
        fs::create_dir_all(path).map_err(|e| format!("create {}: {e}", path.display()))
    }

    fn atomic_write(
        opts: &Options,
        path: &Path,
        bytes: &[u8],
        executable: bool,
    ) -> Result<(), String> {
        atomic_write_with_acl(opts, path, bytes, executable, AclClass::PublicFile)
    }

    fn atomic_write_with_acl(
        opts: &Options,
        path: &Path,
        bytes: &[u8],
        executable: bool,
        acl_class: AclClass,
    ) -> Result<(), String> {
        // Skipping an identical file is an optimisation, so failing to read it
        // must not fail the install. A file with an empty DACL cannot be read
        // by anyone, including Administrators, and that is a state this
        // installer can leave behind if it is interrupted while applying an
        // ACL. Treating the read as fatal made that state unrecoverable: every
        // retry, with or without --force, died on
        //     read ...\tls\host.crt: Access is denied. (os error 5)
        // before reaching the code that would overwrite the file and repair the
        // ACL. A file that cannot be read is simply not known to be identical,
        // so fall through to the rewrite that was going to happen anyway.
        //
        // Identical bytes do not imply an up-to-date ACL. ACL policy can change
        // between releases, and verifying without applying made an otherwise
        // repairable upgrade fail on the unchanged Credential Provider DLL.
        if path.exists() && fs::read(path).is_ok_and(|existing| existing == bytes) {
            detail!("unchanged: {}", path.display());
            return apply_acl(opts, path, acl_class);
        }
        if opts.dry_run {
            println!("dry-run: write {} bytes to {}", bytes.len(), path.display());
            return Ok(());
        }
        if executable && service_running(&opts.service_name)? {
            return Err(format!(
                "service {} is running; refusing to replace {} without stopping it first",
                opts.service_name,
                path.display()
            ));
        }
        let parent = path
            .parent()
            .ok_or_else(|| format!("{} has no parent", path.display()))?;
        fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
        let tmp = parent.join(format!(
            ".{}.new-{}",
            path.file_name()
                .and_then(OsStr::to_str)
                .unwrap_or("payload"),
            std::process::id()
        ));
        {
            let mut file =
                fs::File::create(&tmp).map_err(|e| format!("create {}: {e}", tmp.display()))?;
            file.write_all(bytes)
                .map_err(|e| format!("write {}: {e}", tmp.display()))?;
            file.sync_all()
                .map_err(|e| format!("sync {}: {e}", tmp.display()))?;
        }
        if path.exists() {
            let backup = opts.programdata.join("rollback").join(format!(
                "{}.pre-{}",
                path.file_name()
                    .and_then(OsStr::to_str)
                    .unwrap_or("payload"),
                timestamp()
            ));
            fs::create_dir_all(backup.parent().expect("rollback parent"))
                .map_err(|e| format!("create rollback: {e}"))?;
            fs::rename(path, &backup).map_err(|e| {
                format!(
                    "move existing {} to rollback {}: {e}",
                    path.display(),
                    backup.display()
                )
            })?;
            detail!("rollback backup: {}", backup.display());
        }
        fs::rename(&tmp, path).map_err(|e| format!("publish {}: {e}", path.display()))?;
        apply_acl(opts, path, acl_class)
    }

    /// Assemble the certificate name list from discovered facts.
    ///
    /// Pure so the awkward cases can be tested without a domain controller:
    /// a workgroup machine must never be given a `host.WORKGROUP` name, and a
    /// domain machine must get both `host` and `host.domain.tld` even when only
    /// one of the discovery sources answered.
    fn assemble_names(
        short: Option<&str>,
        domain: Option<&str>,
        dns_fqdn: Option<&str>,
        ips: &[String],
    ) -> Vec<String> {
        let mut dns: Vec<String> = vec!["localhost".to_string(), "arcen-pier.local".to_string()];
        let lower = |value: &str| value.trim().trim_matches('.').to_ascii_lowercase();
        let push = |value: String, dns: &mut Vec<String>| {
            if !value.is_empty() && !dns.contains(&value) {
                dns.push(value);
            }
        };

        if let Some(short) = short.map(lower).filter(|value| !value.is_empty()) {
            push(short.clone(), &mut dns);
            // `Win32_ComputerSystem.Domain` is "WORKGROUP" on a machine that is
            // not domain-joined. Appending it would mint a name that resolves
            // nowhere, so only a domain that actually looks like a DNS suffix
            // is used.
            if let Some(domain) = domain.map(lower).filter(|value| value.contains('.')) {
                push(format!("{short}.{domain}"), &mut dns);
            }
        }
        if let Some(fqdn) = dns_fqdn.map(lower).filter(|value| !value.is_empty()) {
            push(fqdn.clone(), &mut dns);
            // Mirror of the Linux defect: a host known only by its FQDN must
            // still answer to the short name a person types.
            if let Some((short, _)) = fqdn.split_once('.') {
                push(short.to_string(), &mut dns);
            }
        }

        let mut ordered = dns;
        for address in ips {
            if !ordered.contains(address) {
                ordered.push(address.clone());
            }
        }
        ordered
    }

    /// Names and addresses this host will actually be reached by.
    ///
    /// A certificate for `localhost` and `arcen-pier.local` alone is useless:
    /// a Deck connecting to `192.168.1.20` is told
    ///     certificate not valid for name "192.168.1.20";
    ///     certificate is only valid for DnsName("localhost") or
    ///     DnsName("arcen-pier.local")
    /// and cannot connect at all. The session probe missed this for weeks
    /// because it disables certificate verification; the real client does not.
    ///
    /// Both the short name and the fully qualified name must appear, because
    /// either is a reasonable thing to type. Discovery therefore asks several
    /// independent sources and unions the answers rather than trusting one:
    /// `USERDNSDOMAIN` was tried first and is empty when the installer runs
    /// elevated without a domain user token, which is the normal case, so a
    /// domain-joined host got a certificate with no FQDN in it and refused
    /// `host.domain.tld` with a hostname mismatch.
    ///
    /// rcgen classifies each entry itself, so an address string becomes an IP
    /// SAN and a name becomes a DNS SAN.
    fn subject_alt_names() -> Vec<String> {
        let mut short = std::env::var("COMPUTERNAME").ok();
        let mut domain = None;
        let mut dns_fqdn = None;
        let mut ips: Vec<String> = vec!["127.0.0.1".to_string()];

        // One call, structured output. Every field is queried from an API whose
        // property names are English regardless of display language, unlike
        // parsing `ipconfig`.
        if let Ok(output) = powershell_command(
            "$cs = Get-CimInstance Win32_ComputerSystem; \
                 'NAME=' + $cs.Name; \
                 if ($cs.PartOfDomain) { 'DOMAIN=' + $cs.Domain }; \
                 try { 'FQDN=' + [System.Net.Dns]::GetHostEntry($env:COMPUTERNAME).HostName } catch {}; \
                 Get-NetIPAddress -AddressFamily IPv4 | ForEach-Object { 'IP=' + $_.IPAddress }",
        )
        .output()
        {
            for line in String::from_utf8_lossy(&output.stdout).lines() {
                let line = line.trim();
                if let Some(value) = line.strip_prefix("NAME=") {
                    if !value.is_empty() {
                        short = Some(value.to_string());
                    }
                } else if let Some(value) = line.strip_prefix("DOMAIN=") {
                    if !value.is_empty() {
                        domain = Some(value.to_string());
                    }
                } else if let Some(value) = line.strip_prefix("FQDN=") {
                    if !value.is_empty() {
                        dns_fqdn = Some(value.to_string());
                    }
                } else if let Some(value) = line.strip_prefix("IP=") {
                    // Link-local addresses are not reachable identities.
                    if value.parse::<std::net::Ipv4Addr>().is_ok()
                        && !value.starts_with("169.254.")
                        && !ips.contains(&value.to_string())
                    {
                        ips.push(value.to_string());
                    }
                }
            }
        }

        assemble_names(
            short.as_deref(),
            domain.as_deref(),
            dns_fqdn.as_deref(),
            &ips,
        )
    }

    /// Certificate parameters for a self-signed Pier certificate.
    ///
    /// Separate from `ensure_tls` so the validity window can be asserted
    /// without touching the filesystem.
    fn certificate_params(names: Vec<String>) -> Result<CertificateParams, String> {
        let mut params =
            CertificateParams::new(names).map_err(|e| format!("create cert params: {e}"))?;
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        // Without an explicit window rcgen applies its own defaults, which are
        // 1975-01-01 to 4096-01-01. A Pier certificate that never expires is
        // not cosmetic here: the trust anchor is a user-approved pin with no
        // revocation channel, so expiry is the only event that would ever force
        // a Deck to re-verify a host. It also left the whole certificate-expiry
        // apparatus — CertificateTimePolicy, days_remaining, the
        // TlsCertificateExpiring lifecycle event and the documented
        // --tls-expiry-warning-days knob — permanently inert on Windows.
        //
        // The five-minute backdate matches the PowerShell helper and absorbs
        // clock skew between Pier and Deck; the Deck rejects a not-yet-valid
        // certificate outright.
        let now = OffsetDateTime::now_utc();
        params.not_before = now - Duration::minutes(5);
        params.not_after = now + Duration::days(CERTIFICATE_VALIDITY_DAYS);
        Ok(params)
    }

    /// Reads the TLS directory into the shared provisioning input.
    ///
    /// The decision itself belongs to `arcen_transport::cert_provisioning`, so
    /// Windows, Linux and macOS answer create/keep/renew/rekey/adopt
    /// identically rather than each installer inventing its own rules.
    fn inspect_tls(tls: &Path) -> MaterialState {
        let cert = tls.join("host.crt");
        let key = tls.join("host.key");
        let certificate_present = cert.is_file();
        let key_present = key.is_file();
        if !certificate_present && !key_present {
            return MaterialState::absent();
        }

        let bytes = std::fs::read(&cert).ok();
        let ownership = bytes.as_ref().map(|bytes| {
            if marker_matches(tls, bytes) {
                MaterialOwnership::Owned
            } else {
                MaterialOwnership::Foreign
            }
        });
        let now = OffsetDateTime::now_utc().unix_timestamp();
        let (certificate_valid, expiring_or_expired) = bytes
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
            stale_staging_present: false,
            self_signed: bytes
                .as_ref()
                .is_some_and(|bytes| cert_marker::is_self_signed_pem(bytes)),
        }
    }

    /// Returns whether the ownership marker describes the certificate on disk.
    fn marker_matches(tls: &Path, certificate_bytes: &[u8]) -> bool {
        let Ok(recorded) = std::fs::read_to_string(tls.join(MARKER_FILE)) else {
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

    /// Writes the pin files and ownership marker beside the certificate.
    fn write_pins_and_marker(opts: &Options, tls: &Path) -> Result<(), String> {
        let bytes = std::fs::read(tls.join("host.crt"))
            .map_err(|error| format!("read host.crt: {error}"))?;
        let pins = cert_marker::pins_from_pem(&bytes)
            .ok_or_else(|| "cannot pin the generated certificate".to_string())?;
        let marker = OwnershipMarker::new(&pins.certificate, &pins.spki)
            .map_err(|error| format!("ownership marker: {error}"))?;
        atomic_write(
            opts,
            &tls.join("host.cert-sha256"),
            format!(
                "sha256 Fingerprint={}\n",
                cert_marker::colon_hex(&pins.certificate)
            )
            .as_bytes(),
            false,
        )?;
        atomic_write(
            opts,
            &tls.join("host.spki-sha256"),
            format!("{}\n", pins.spki).as_bytes(),
            false,
        )?;
        atomic_write(
            opts,
            &tls.join(MARKER_FILE),
            marker.render().as_bytes(),
            false,
        )
    }

    /// Ownership marker name, shared with the Linux helper and the macOS host.
    const MARKER_FILE: &str = "host.generated-by-arcen";
    /// How close to expiry counts as due for renewal.
    const RENEW_WITHIN_SECONDS: i64 = 30 * 24 * 60 * 60;

    fn ensure_tls(opts: &Options, tls: &Path) -> Result<(), String> {
        let cert = tls.join("host.crt");
        let key = tls.join("host.key");
        // `--force` means "replace the key too", which is what lets an operator
        // change the names the certificate covers after installation. Without
        // it, existing material is kept or renewed, never silently replaced.
        let request = if opts.force {
            ProvisioningRequest::Rekey
        } else if opts.adopt_legacy {
            ProvisioningRequest::AdoptLegacy
        } else {
            ProvisioningRequest::Ensure
        };
        let state = inspect_tls(tls);
        let decided = plan(request, state).map_err(|refusal| {
            let hint = if refusal == ProvisioningRefusal::ForeignMaterial {
                format!(
                    "\n  To keep this key so paired Decks still trust the host, re-run with \
                     --adopt-legacy.\n  Only do that if {} was created by an earlier Arcen \
                     installer; otherwise move it aside and re-run to create a new one.",
                    tls.display()
                )
            } else {
                String::new()
            };
            format!("{}: {}{hint}", refusal.as_str(), refusal.guidance())
        })?;

        match decided.action {
            ProvisioningAction::KeepExisting => {
                println!(
                    "keeping existing TLS material in {} (pass --force to replace it, \
                     for example after adding --extra-san)",
                    tls.display()
                );
                return apply_secret_file_acl(opts, &key);
            }
            ProvisioningAction::RenewPreservingKey | ProvisioningAction::AdoptAndRenew => {
                // Reissuing over the existing key keeps every pinned Deck
                // working, so it is done without a warning.
                println!("reissuing the TLS certificate in {}", tls.display());
            }
            ProvisioningAction::CreateNew | ProvisioningAction::ReplaceKeyAndCertificate => {
                if decided.invalidates_pins && (cert.exists() || key.exists()) {
                    println!(
                        "--force: replacing the TLS key and certificate in {}. \
                         Every Deck that pinned the previous certificate must re-pin.",
                        tls.display()
                    );
                }
            }
        }

        let mut names = subject_alt_names();
        // Operator-supplied names last, so a duplicate of something discovered
        // locally does not appear twice; rcgen would emit both.
        for extra in &opts.extra_sans {
            if !names.iter().any(|existing| existing == extra) {
                names.push(extra.clone());
            }
        }
        println!("TLS certificate covers: {}", names.join(", "));
        let params = certificate_params(names)?;

        // Whether the key survives is the whole difference between renewal and
        // rekey. Generating unconditionally here would make "reissuing over the
        // existing key" a lie and silently break every SPKI-pinned Deck at the
        // moment the host renewed itself — the least convenient moment to
        // discover it.
        let preserve_key = matches!(
            decided.action,
            ProvisioningAction::RenewPreservingKey | ProvisioningAction::AdoptAndRenew
        );
        let keypair = if preserve_key {
            let existing = std::fs::read_to_string(&key)
                .map_err(|e| format!("read existing TLS key {}: {e}", key.display()))?;
            KeyPair::from_pem(&existing)
                .map_err(|e| format!("reuse existing TLS key {}: {e}", key.display()))?
        } else {
            KeyPair::generate().map_err(|e| format!("generate TLS key: {e}"))?
        };
        let certificate = params
            .self_signed(&keypair)
            .map_err(|e| format!("self-sign TLS cert: {e}"))?;
        atomic_write(opts, &cert, certificate.pem().as_bytes(), false)?;
        // Rewriting an unchanged key would churn its ACL and mtime for no
        // reason, and a failure there would destroy material that was fine.
        if preserve_key {
            // A kept key still gets the secret-file ACL: an adopted key from an
            // older install, or one restored by hand, may carry inherited access.
            apply_secret_file_acl(opts, &key)?;
        } else {
            atomic_write_with_acl(
                opts,
                &key,
                keypair.serialize_pem().as_bytes(),
                false,
                AclClass::SecretFile,
            )?;
        }
        // The marker is what stops a later run replacing material an operator
        // installed themselves, and the pins are what they compare against.
        write_pins_and_marker(opts, tls)
    }

    /// A directory whose contents users must read and execute but never write.
    ///
    /// The install prefix: users run `arcen-pier.exe`, and LogonUI loads
    /// `arcen_credential_provider.dll` from here as SYSTEM. Neither may be
    /// replaceable by a non-administrator, and a file DACL alone does not
    /// achieve that — `FILE_DELETE_CHILD` on the directory would still allow
    /// delete-and-replace.
    fn apply_public_dir_acl(opts: &Options, path: &Path) -> Result<(), String> {
        apply_acl(opts, path, AclClass::PublicDirectory)
    }

    fn apply_secret_dir_acl(opts: &Options, path: &Path) -> Result<(), String> {
        apply_acl(opts, path, AclClass::SecretDirectory)
    }

    fn apply_secret_file_acl(opts: &Options, path: &Path) -> Result<(), String> {
        apply_acl(opts, path, AclClass::SecretFile)
    }

    fn apply_acl(opts: &Options, path: &Path, acl_class: AclClass) -> Result<(), String> {
        detail!(
            "applying {} SDDL to {}: {}",
            acl_class.label(),
            path.display(),
            acl_class.sddl()
        );
        // `/inheritance:r` and `/grant:r` go in one invocation on purpose.
        // Issued separately, the first strips every inherited ACE and leaves an
        // empty DACL that nobody can read, and the second restores access. Any
        // interruption between them — a killed process, a failing icacls, a
        // reboot — leaves the file permanently unreadable. Combining them means
        // the file is never observable without a DACL.
        // Ownership first, and by SID for the same localization reason as the
        // grants. Installed service paths require the owner to be SYSTEM or
        // Administrators; a directory this installer created is otherwise owned
        // by whoever ran it, so the service installs cleanly and then refuses
        // to start with `directory_chain_invalid`.
        //
        // `hosts/windows/INSTALL.md` has always done this. The binary did not,
        // which is the third place these two install paths had drifted apart.
        run_or_print(
            opts,
            Command::new("icacls")
                .arg(path)
                .args(["/setowner", OWNER_SID]),
        )?;
        let mut reset = Command::new("icacls");
        reset.arg(path).arg("/inheritance:r").arg("/grant:r");
        for grant in acl_class.grants() {
            reset.arg(grant);
        }
        run_or_print(opts, &mut reset)?;
        let revoked = acl_class.revoked_grants();
        if !revoked.is_empty() {
            // `/grant:r` replaces only ACEs for trustees named above. Remove
            // stale broad trustees after the required grants are in place, so
            // upgrades remain readable throughout and converge on one exact
            // ACL. This repairs paths that moved between the original manual
            // AU grant and the first binary installer's BU grant.
            let mut remove = Command::new("icacls");
            remove.arg(path).arg("/remove:g");
            for trustee in revoked {
                remove.arg(trustee);
            }
            run_or_print(opts, &mut remove)?;
        }
        remove_unexpected_trustees(opts, path, acl_class)?;
        verify_acl(opts, path, acl_class)
    }

    /// Removes every trustee the class does not allow.
    ///
    /// `/grant:r` only replaces the trustees it names. Opening a protected
    /// folder in Explorer and accepting "You don't currently have permission"
    /// adds the signed-in user with full control, and the installer then
    /// refused to run until someone repaired the ACL by hand.
    fn remove_unexpected_trustees(
        opts: &Options,
        path: &Path,
        acl_class: AclClass,
    ) -> Result<(), String> {
        if opts.dry_run {
            return Ok(());
        }
        let sddl = read_sddl(path)?;
        let extra = unexpected_trustees(&sddl, acl_class)?;
        if extra.is_empty() {
            return Ok(());
        }
        println!(
            "removing access this installer does not grant from {}: {}",
            path.display(),
            extra.join(", ")
        );
        let mut remove = Command::new("icacls");
        remove.arg(path).arg("/remove");
        for sid in &extra {
            remove.arg(format!("*{sid}"));
        }
        run_or_print(opts, &mut remove)
    }

    fn verify_acl(opts: &Options, path: &Path, acl_class: AclClass) -> Result<(), String> {
        if opts.dry_run {
            println!(
                "dry-run: verify {} ACL {}",
                acl_class.label(),
                path.display()
            );
            return Ok(());
        }
        // Read the descriptor as SDDL rather than parsing `icacls` output.
        // icacls prints resolved account names in the display language, so a
        // name comparison passes on English Windows and fails on every other
        // localization. SDDL carries SIDs, which are identical everywhere.
        let sddl = read_sddl(path)?;
        detail!("ACL {} {}", path.display(), sddl);
        assert_acl_sddl(&path.to_string_lossy(), &sddl, acl_class)
    }

    /// Windows PowerShell, with the module search path sanitized.
    ///
    /// The installer is routinely launched from PowerShell 7, which exports its
    /// own `PSModulePath`. Windows PowerShell 5.1 inherits that variable and
    /// then cannot find its own modules, so an autoloaded cmdlet fails with
    /// "the module could not be loaded". Removing the variable makes 5.1
    /// compute its documented defaults.
    ///
    /// Both call sites need this. `Get-Acl` fails loudly, which is how this was
    /// found; the host/IP query fails *silently* into an empty result, which
    /// would issue a TLS certificate carrying no hostname or address and only
    /// surface much later as "certificate not valid for name".
    fn powershell_command(script: &str) -> Command {
        let mut command = Command::new("powershell");
        command.env_remove("PSModulePath").args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            script,
        ]);
        command
    }

    fn read_sddl(path: &Path) -> Result<String, String> {
        // Single-quoted PowerShell literal: the only escape is a doubled quote.
        let literal = path.to_string_lossy().replace('\'', "''");
        let output = powershell_command(&format!("(Get-Acl -LiteralPath '{literal}').Sddl"))
            .output()
            .map_err(|e| format!("read security descriptor for {}: {e}", path.display()))?;
        if !output.status.success() {
            return Err(format!(
                "read security descriptor for {} failed: {}",
                path.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        let sddl = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if sddl.is_empty() {
            return Err(format!(
                "read security descriptor for {} returned nothing",
                path.display()
            ));
        }
        Ok(sddl)
    }

    fn register_service(opts: &Options, config: &Path) -> Result<(), String> {
        let binary_path = format!(
            "\"{}\" service --config \"{}\"",
            opts.prefix.join("arcen-pier.exe").display(),
            config.display()
        );
        if service_exists(&opts.service_name)? {
            run_or_print(
                opts,
                Command::new("sc.exe")
                    .arg("config")
                    .arg(&opts.service_name)
                    .arg("binPath=")
                    .arg(&binary_path)
                    .arg("start=")
                    .arg("auto"),
            )?;
        } else {
            run_or_print(
                opts,
                Command::new("sc.exe")
                    .arg("create")
                    .arg(&opts.service_name)
                    .arg("binPath=")
                    .arg(&binary_path)
                    .arg("start=")
                    .arg("auto")
                    .arg("obj=")
                    .arg("LocalSystem"),
            )?;
        }
        detail!(
            "service {} BinaryPathName: {}",
            opts.service_name,
            binary_path
        );
        Ok(())
    }

    fn register_credential_provider(opts: &Options) -> Result<(), String> {
        if opts.staging() && !opts.force {
            println!(
                "staging mode: skipped live Credential Provider registry; rerun with --force on production paths to register HKLM"
            );
            return Ok(());
        }
        let dll = opts.prefix.join(CP_DLL).display().to_string();
        let clsid = format!(r"HKLM\SOFTWARE\Classes\CLSID\{}", CLSID);
        let inproc = format!(r"HKLM\SOFTWARE\Classes\CLSID\{}\InprocServer32", CLSID);
        let provider = format!(
            r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Authentication\Credential Providers\{}",
            CLSID
        );
        run_or_print(
            opts,
            Command::new("reg.exe").args(["add", &clsid, "/ve", "/d", PROVIDER_NAME, "/f"]),
        )?;
        // Register only a provider that can actually load. A CLSID pointing at
        // a DLL that is not on disk produces a registered credential provider
        // Windows cannot instantiate, and the visible symptom is a sign-in
        // failure telling the operator to install the provider that the
        // registry already claims is installed. Observed in the field.
        if !opts.dry_run && !Path::new(&dll).is_file() {
            return Err(format!(
                "refusing to register the credential provider: {dll} is not present"
            ));
        }
        run_or_print(
            opts,
            Command::new("reg.exe").args(["add", &inproc, "/ve", "/d", &dll, "/f"]),
        )?;
        run_or_print(
            opts,
            Command::new("reg.exe").args([
                "add",
                &inproc,
                "/v",
                "ThreadingModel",
                "/t",
                "REG_SZ",
                "/d",
                THREADING_MODEL,
                "/f",
            ]),
        )?;
        run_or_print(
            opts,
            Command::new("reg.exe").args(["add", &provider, "/ve", "/d", PROVIDER_NAME, "/f"]),
        )?;
        Ok(())
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum EventLogSourceAction {
        Install,
        Uninstall,
    }

    impl EventLogSourceAction {
        const fn switch(self) -> &'static str {
            match self {
                Self::Install => "-Install",
                Self::Uninstall => "-Uninstall",
            }
        }
    }

    fn register_eventlog_source(opts: &Options, script: &Path) -> Result<(), String> {
        run_eventlog_source_script(opts, script, EventLogSourceAction::Install)
    }

    fn unregister_eventlog_source(opts: &Options) -> Result<(), String> {
        if opts.staging() && !opts.force {
            println!("staging mode: skipped live ArcenPier Event Log source registry removal");
            return Ok(());
        }
        let installed_script = opts.programdata.join(EVENTLOG_SOURCE_SCRIPT_NAME);
        if installed_script.is_file() || opts.dry_run {
            return run_eventlog_source_script(
                opts,
                &installed_script,
                EventLogSourceAction::Uninstall,
            );
        }

        let temporary_script = write_temporary_eventlog_source_script()?;
        let result =
            run_eventlog_source_script(opts, &temporary_script, EventLogSourceAction::Uninstall);
        let cleanup = fs::remove_file(&temporary_script).map_err(|error| {
            format!(
                "remove temporary Event Log source script {}: {error}",
                temporary_script.display()
            )
        });
        result.and(cleanup)
    }

    fn run_eventlog_source_script(
        opts: &Options,
        script: &Path,
        action: EventLogSourceAction,
    ) -> Result<(), String> {
        if opts.staging() && !opts.force {
            println!(
                "staging mode: skipped live ArcenPier Event Log source registry {}",
                action.switch()
            );
            return Ok(());
        }
        let mut command = Command::new("powershell.exe");
        command
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
            ])
            .arg(script)
            .arg(action.switch());
        run_or_print(opts, &mut command)
    }

    fn write_temporary_eventlog_source_script() -> Result<PathBuf, String> {
        let path = std::env::temp_dir().join(format!(
            "arcen-eventlog-source-{}-{}.ps1",
            std::process::id(),
            timestamp()
        ));
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|error| {
                format!(
                    "create temporary Event Log source script {}: {error}",
                    path.display()
                )
            })?;
        file.write_all(EVENTLOG_SOURCE_SCRIPT.as_bytes())
            .map_err(|error| {
                format!(
                    "write temporary Event Log source script {}: {error}",
                    path.display()
                )
            })?;
        file.sync_all().map_err(|error| {
            format!(
                "sync temporary Event Log source script {}: {error}",
                path.display()
            )
        })?;
        Ok(path)
    }

    fn unregister_credential_provider(opts: &Options) -> Result<(), String> {
        let clsid = format!(r"HKLM\SOFTWARE\Classes\CLSID\{}", CLSID);
        let provider = format!(
            r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Authentication\Credential Providers\{}",
            CLSID
        );
        if opts.dry_run || registry_key_exists(&provider)? {
            run_or_print(
                opts,
                Command::new("reg.exe").args(["delete", &provider, "/f"]),
            )?;
        }
        if opts.dry_run || registry_key_exists(&clsid)? {
            run_or_print(opts, Command::new("reg.exe").args(["delete", &clsid, "/f"]))?;
        }
        Ok(())
    }

    fn registry_key_exists(key: &str) -> Result<bool, String> {
        Ok(Command::new("reg.exe")
            .args(["query", key])
            .output()
            .map_err(|e| format!("query registry key {key}: {e}"))?
            .status
            .success())
    }

    fn service_exists(name: &str) -> Result<bool, String> {
        Ok(Command::new("sc.exe")
            .arg("query")
            .arg(name)
            .output()
            .map_err(|e| format!("query service {name}: {e}"))?
            .status
            .success())
    }

    fn service_running(name: &str) -> Result<bool, String> {
        let output = Command::new("sc.exe")
            .arg("query")
            .arg(name)
            .output()
            .map_err(|e| format!("query service {name}: {e}"))?;
        Ok(output.status.success() && String::from_utf8_lossy(&output.stdout).contains("RUNNING"))
    }

    fn run_or_print(opts: &Options, cmd: &mut Command) -> Result<(), String> {
        if opts.dry_run {
            println!("dry-run: {:?}", cmd);
            return Ok(());
        }
        let output = cmd.output().map_err(|e| format!("run {:?}: {e}", cmd))?;
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if !stdout.trim().is_empty() {
                detail!("{}", stdout.trim_end());
            }
            Ok(())
        } else {
            Err(format!(
                "command {:?} failed: {}{}",
                cmd,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            ))
        }
    }

    fn remove_file(opts: &Options, path: &Path) -> Result<(), String> {
        if opts.dry_run {
            println!("dry-run: remove file {}", path.display());
        } else if path.exists() {
            fs::remove_file(path).map_err(|e| format!("remove {}: {e}", path.display()))?;
        }
        Ok(())
    }

    fn remove_credential_provider_file(opts: &Options, path: &Path) -> Result<(), String> {
        if opts.dry_run {
            println!("dry-run: remove file {}", path.display());
            return Ok(());
        }
        if !path.exists() {
            return Ok(());
        }
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                // The sign-in screen keeps the DLL loaded until it restarts.
                // It is already unregistered, so after a reboot nothing loads
                // it; a loaded DLL can still be renamed, which frees its name
                // and lets the uninstall finish instead of demanding a reboot
                // and a second run. The next install or --purge removes it.
                let aside = path.with_extension(format!("dll.pending-delete-{}", timestamp()));
                match fs::rename(path, &aside) {
                    Ok(()) => {
                        println!(
                            "The sign-in screen still has the credential provider loaded; it is \
                             unregistered and set aside as {}. Restart Windows to unload it.",
                            aside.display()
                        );
                        Ok(())
                    }
                    Err(_) => Err(format!(
                        "Credential Provider is still loaded by LogonUI and cannot be removed \
                         yet: {}. Reboot Windows, then rerun this same --uninstall{} command",
                        path.display(),
                        if opts.purge { " --purge" } else { "" }
                    )),
                }
            }
            Err(error) => Err(format!("remove {}: {error}", path.display())),
        }
    }

    /// Deletes credential provider DLLs an earlier uninstall set aside while
    /// the sign-in screen still had them loaded. One that is still loaded
    /// stays until a later run.
    fn remove_set_aside_credential_providers(opts: &Options) {
        if opts.dry_run {
            return;
        }
        let Ok(entries) = fs::read_dir(&opts.prefix) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            if name.to_str().is_some_and(|name| {
                name.starts_with("arcen_credential_provider.dll.pending-delete-")
            }) {
                let _ = fs::remove_file(entry.path());
            }
        }
    }

    fn remove_dir_if_empty(opts: &Options, path: &Path) -> Result<(), String> {
        if opts.dry_run {
            println!("dry-run: remove dir if empty {}", path.display());
        } else if path.exists() {
            match fs::remove_dir(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::DirectoryNotEmpty => {}
                Err(error) => return Err(format!("remove {}: {error}", path.display())),
            }
        }
        Ok(())
    }

    fn remove_dir_all(opts: &Options, path: &Path) -> Result<(), String> {
        if opts.dry_run {
            println!("dry-run: purge {}", path.display());
        } else if path.exists() {
            fs::remove_dir_all(path).map_err(|e| format!("purge {}: {e}", path.display()))?;
        }
        Ok(())
    }

    fn timestamp() -> String {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        secs.to_string()
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// `--purge` deletes ProgramData outright, and the configuration is the
        /// one thing there the installer cannot rebuild: adapter exclusions,
        /// monitor layout and transport tuning are site facts.
        #[test]
        fn purge_preserves_a_copy_of_the_configuration() {
            let unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos());
            let root = std::env::temp_dir().join(format!(
                "arcen-windows-installer-purge-{}-{unique}",
                std::process::id()
            ));
            let programdata = root.join("Arcen");
            fs::create_dir_all(&programdata).expect("create programdata");
            let tuned = br#"{"platform":{"desktop":{"adapter":"reserved-gpu","output":1}}}"#;
            fs::write(programdata.join("pier.json"), tuned).expect("write tuned config");

            let opts = Options {
                prefix: root.join("prefix"),
                programdata: programdata.clone(),
                dry_run: false,
                uninstall: true,
                purge: true,
                version: false,
                force: false,
                adopt_legacy: false,
                service_name: SERVICE_NAME.to_string(),
                extra_sans: Vec::new(),
            };

            preserve_config_before_purge(&opts).expect("preserve config");
            remove_dir_all(&opts, &programdata).expect("purge programdata");

            assert!(
                !programdata.exists(),
                "purge must still remove ProgramData\\Arcen"
            );
            let preserved: Vec<_> = fs::read_dir(&root)
                .expect("read purge root")
                .filter_map(Result::ok)
                .filter(|e| {
                    e.file_name()
                        .to_string_lossy()
                        .starts_with("arcen-pier.json.purged-")
                })
                .collect();
            assert_eq!(preserved.len(), 1, "expected exactly one preserved copy");
            assert_eq!(
                fs::read(preserved[0].path()).expect("read preserved"),
                tuned,
                "preserved copy must be byte-identical to the tuned config"
            );
            let _ = fs::remove_dir_all(&root);
        }

        /// A machine with no configuration must still purge cleanly.
        #[test]
        fn purge_without_a_configuration_is_not_an_error() {
            let unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos());
            let root = std::env::temp_dir().join(format!(
                "arcen-windows-installer-purge-empty-{}-{unique}",
                std::process::id()
            ));
            let programdata = root.join("Arcen");
            fs::create_dir_all(&programdata).expect("create programdata");

            let opts = Options {
                prefix: root.join("prefix"),
                programdata,
                dry_run: false,
                uninstall: true,
                purge: true,
                version: false,
                force: false,
                adopt_legacy: false,
                service_name: SERVICE_NAME.to_string(),
                extra_sans: Vec::new(),
            };

            preserve_config_before_purge(&opts).expect("absent config must not fail the purge");

            let _ = fs::remove_dir_all(&root);
        }

        /// Launching the installer from PowerShell 7 exports a `PSModulePath`
        /// that Windows PowerShell 5.1 inherits and then cannot load its own
        /// modules from, so `Get-Acl` fails with "the module could not be
        /// loaded" and the install aborts. The host/IP query fails silently the
        /// same way, which would issue a certificate with no SANs.
        #[test]
        fn powershell_is_always_invoked_with_a_sanitized_module_path() {
            let command = powershell_command("'probe'");
            let removed = command
                .get_envs()
                .any(|(key, value)| key == "PSModulePath" && value.is_none());
            assert!(
                removed,
                "PSModulePath must be removed, or a pwsh 7 parent breaks module autoloading"
            );
            let args: Vec<_> = command.get_args().map(|a| a.to_string_lossy()).collect();
            assert!(args.contains(&"-NoProfile".into()));
            assert!(args.contains(&"-NonInteractive".into()));
        }

        #[test]
        fn dry_run_does_not_query_the_live_service_before_simulating_a_write() {
            let unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos());
            let root = std::env::temp_dir().join(format!(
                "arcen-windows-installer-dry-run-{}-{unique}",
                std::process::id()
            ));
            let path = root.join("payload.exe");
            let opts = Options {
                prefix: root.join("prefix"),
                programdata: root.join("programdata"),
                dry_run: true,
                uninstall: false,
                purge: false,
                version: false,
                force: false,
                adopt_legacy: false,
                service_name: "invalid\0service".to_string(),
                extra_sans: Vec::new(),
            };

            atomic_write_with_acl(&opts, &path, b"replacement", true, AclClass::PublicFile)
                .expect("dry-run must not query or require stopping the live service");
            assert!(!path.exists(), "dry-run must not write the payload");
        }

        #[test]
        fn a_lone_certificate_is_refused_rather_than_regenerated() {
            // The bug this migration fixes. `ensure_tls` previously returned
            // early only when both files existed, so a directory holding just
            // `host.crt` fell through and regenerated both, replacing a
            // certificate whose key had gone missing.
            let state = MaterialState {
                certificate_present: true,
                key_present: false,
                ..MaterialState::absent()
            };
            assert!(plan(ProvisioningRequest::Ensure, state).is_err());
            assert!(plan(ProvisioningRequest::Rekey, state).is_err());
        }

        #[test]
        fn material_the_installer_did_not_issue_is_not_replaced() {
            // Without an ownership marker the installer could overwrite an
            // enterprise certificate an operator placed deliberately.
            let foreign = MaterialState {
                ownership: Some(MaterialOwnership::Foreign),
                self_signed: false,
                ..MaterialState::owned_valid()
            };
            assert_eq!(
                plan(ProvisioningRequest::Ensure, foreign).map(|plan| plan.action),
                Ok(ProvisioningAction::KeepExisting)
            );
            assert!(plan(ProvisioningRequest::Rekey, foreign).is_err());
        }

        #[test]
        fn force_replaces_the_key_and_plain_install_keeps_material() {
            let owned = MaterialState::owned_valid();
            let kept = plan(ProvisioningRequest::Ensure, owned).expect("keep");
            assert_eq!(kept.action, ProvisioningAction::KeepExisting);
            assert!(!kept.invalidates_pins);

            let forced = plan(ProvisioningRequest::Rekey, owned).expect("rekey");
            assert_eq!(forced.action, ProvisioningAction::ReplaceKeyAndCertificate);
            assert!(
                forced.invalidates_pins,
                "--force must be reported as breaking pins"
            );
        }

        #[test]
        fn generated_certificates_expire() {
            // rcgen's defaults are 1975-01-01 to 4096-01-01. Shipping those
            // produced a Pier certificate that never expired, and because the
            // trust anchor is a user-approved pin with no revocation channel,
            // nothing else would ever have forced re-verification.
            let params =
                certificate_params(vec!["pier.example.internal".to_string()]).expect("params");
            let now = OffsetDateTime::now_utc();

            assert!(
                params.not_before <= now,
                "certificate must already be valid, got {}",
                params.not_before
            );
            assert!(
                params.not_before > now - Duration::hours(1),
                "backdate is for clock skew, not history, got {}",
                params.not_before
            );

            let lifetime = params.not_after - params.not_before;
            assert!(
                lifetime <= Duration::days(CERTIFICATE_VALIDITY_DAYS + 1),
                "certificate outlives the 825-day policy: {lifetime}"
            );
            assert!(
                lifetime >= Duration::days(CERTIFICATE_VALIDITY_DAYS - 1),
                "certificate is shorter than the 825-day policy: {lifetime}"
            );
        }

        #[test]
        fn generated_certificates_are_server_auth_leaves() {
            let params =
                certificate_params(vec!["pier.example.internal".to_string()]).expect("params");
            assert_eq!(
                params.extended_key_usages,
                vec![ExtendedKeyUsagePurpose::ServerAuth]
            );
            // The Deck's leaf policy rejects a certificate whose key usage is
            // present but omits digitalSignature, so this is load-bearing.
            assert_eq!(params.key_usages, vec![KeyUsagePurpose::DigitalSignature]);
        }

        #[test]
        fn embedded_eventlog_source_contract_matches_installer_actions() {
            assert!(EVENTLOG_SOURCE_SCRIPT.contains("$script:EventSourceName = 'ArcenPier'"));
            assert!(EVENTLOG_SOURCE_SCRIPT.contains("$script:OwnershipMarkerName = 'ArcenOwned'"));
            assert!(
                EVENTLOG_SOURCE_SCRIPT
                    .contains("$script:OwnershipMarkerValue = 'arcen-pier-windows'")
            );
            assert!(EVENTLOG_SOURCE_SCRIPT.contains("$script:TypesSupportedValue = 7"));
            assert_eq!(EventLogSourceAction::Install.switch(), "-Install");
            assert_eq!(EventLogSourceAction::Uninstall.switch(), "-Uninstall");
        }

        /// A domain-joined host must answer to both names a person might type.
        ///
        /// This shipped broken: the FQDN was built from `USERDNSDOMAIN`, which
        /// is empty when the installer runs elevated without a domain user
        /// token. A domain-joined host's certificate then carried only its short
        /// name, so dialling the FQDN failed with
        /// `Verify return code: 62 (hostname mismatch)`.
        #[test]
        fn a_domain_joined_host_gets_both_the_short_name_and_the_fqdn() {
            let names = assemble_names(
                Some("PIER-WINDOWS"),
                Some("ad.example.internal"),
                Some("pier-windows.ad.example.internal"),
                &["127.0.0.1".to_string(), "203.0.113.12".to_string()],
            );
            assert!(names.contains(&"pier-windows".to_string()));
            assert!(names.contains(&"pier-windows.ad.example.internal".to_string()));
            assert!(names.contains(&"203.0.113.12".to_string()));
        }

        /// Either source alone is enough. If DNS cannot answer, the domain
        /// membership still yields the FQDN; if the machine reports no domain,
        /// the DNS answer still yields both forms.
        #[test]
        fn the_fqdn_survives_losing_either_discovery_source() {
            let domain_only = assemble_names(
                Some("PIER-WINDOWS"),
                Some("ad.example.internal"),
                None,
                &["127.0.0.1".to_string()],
            );
            assert!(domain_only.contains(&"pier-windows.ad.example.internal".to_string()));

            let dns_only = assemble_names(
                None,
                None,
                Some("pier-windows.ad.example.internal"),
                &["127.0.0.1".to_string()],
            );
            assert!(dns_only.contains(&"pier-windows.ad.example.internal".to_string()));
            assert!(dns_only.contains(&"pier-windows".to_string()));
        }

        /// A workgroup machine must not be given a name that resolves nowhere.
        ///
        /// `Win32_ComputerSystem.Domain` reads "WORKGROUP" when the machine is
        /// not domain-joined, so appending it blindly would mint
        /// `examplehost.workgroup`.
        #[test]
        fn a_workgroup_host_is_never_given_a_synthetic_domain_name() {
            let names = assemble_names(
                Some("EXAMPLEHOST"),
                Some("WORKGROUP"),
                Some("pier-windows.example.internal"),
                &["127.0.0.1".to_string(), "203.0.113.11".to_string()],
            );
            assert!(names.contains(&"pier-windows.example.internal".to_string()));
            assert!(
                !names.iter().any(|name| name.contains("workgroup")),
                "a workgroup name must never enter the certificate: {names:?}"
            );
        }

        /// Names are lowercased and de-duplicated, and localhost stays first.
        #[test]
        fn names_are_normalised_and_never_repeated() {
            let names = assemble_names(
                Some("Host"),
                Some("Example.Com"),
                Some("host.example.com."),
                &["127.0.0.1".to_string(), "127.0.0.1".to_string()],
            );
            let mut seen = names.clone();
            seen.sort();
            seen.dedup();
            assert_eq!(seen.len(), names.len(), "duplicate SAN entry: {names:?}");
            assert_eq!(names[0], "localhost");
            assert!(names.contains(&"host.example.com".to_string()));
            assert!(names.iter().all(|name| name == &name.to_ascii_lowercase()));
        }
    }
}

#[cfg(windows)]
fn windows_main() -> Result<(), String> {
    imp::run()
}
