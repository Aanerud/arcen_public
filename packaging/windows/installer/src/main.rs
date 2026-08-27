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

    use rcgen::{CertificateParams, ExtendedKeyUsagePurpose, KeyPair, KeyUsagePurpose};
    use time::{Duration, OffsetDateTime};

    use crate::acl::{AclClass, OWNER_SID, assert_acl_sddl};
    use crate::diagnosis::is_tls_failure;

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
             \x20                        [--uninstall] [--purge] [--version] [--force]\n\
             \x20                        [--service-name <name>] [--extra-san <name-or-ip>]\n\
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
        let status = Command::new("net")
            .arg("session")
            .status()
            .map_err(|e| format!("check Administrator elevation: {e}"))?;
        if status.success() {
            Ok(())
        } else {
            Err("Administrator elevation is required: net session returned access denied; rerun from an elevated console".to_string())
        }
    }

    fn install(opts: &Options) -> Result<(), String> {
        println!("install prefix: {}", opts.prefix.display());
        println!("programdata: {}", opts.programdata.display());
        let logs = opts.programdata.join("logs");
        let sessions = logs.join("sessions");
        let runtime = opts.programdata.join("runtime");
        let tls = opts.programdata.join("tls");
        let rollback = opts.programdata.join("rollback");
        for dir in [
            &opts.prefix,
            &opts.programdata,
            &logs,
            &sessions,
            &runtime,
            &tls,
            &rollback,
        ] {
            create_dir(opts, dir)?;
        }
        // The Arcen root carries a protected DACL with exactly two entries,
        // SYSTEM and Administrators. Sub-directories the session agent must
        // write, such as runtime, carry their own explicit grant and remain
        // reachable because Windows gives Everyone bypass-traverse-checking by
        // default.
        apply_secret_dir_acl(opts, &opts.programdata)?;
        for dir in [&logs, &sessions, &rollback] {
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
            let (payload, diagnostic) = safe_auto_windows_config(opts, &pier_path);
            atomic_write(opts, &config, &payload, false)?;
            println!("{diagnostic}");
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
        register_service(opts, &config)?;
        register_credential_provider(opts)?;
        open_firewall(opts);
        start_service(opts)?;
        if !opts.dry_run && !opts.staging() {
            println!();
            println!("=====================================================================");
            println!(" REBOOT REQUIRED before the first remote sign-in.");
            println!();
            println!(" Windows enumerates credential providers only when LogonUI starts,");
            println!(" so the provider registered just now is not visible to the running");
            println!(" logon session. Until this machine reboots, a remote sign-in fails");
            println!(" with a message asking you to install the credential provider that");
            println!(" is in fact already installed.");
            println!("=====================================================================");
            println!();
            println!(
                "Administration guide: {}",
                opts.programdata.join("pier-administration.md").display()
            );
        }
        Ok(())
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

        let (payload, diagnostic) = safe_auto_windows_config(opts, pier);
        atomic_write(opts, config, &payload, false)?;
        println!("wrote a fresh default config: {}", config.display());
        println!("{diagnostic}");
        println!("re-apply any settings you had customised, using the preserved copy.");
        Ok(())
    }

    fn safe_auto_windows_config(opts: &Options, pier_path: &Path) -> (Vec<u8>, String) {
        if opts.dry_run {
            return (
                DEFAULT_CONFIG.as_bytes().to_vec(),
                "multi-monitor safe-auto: left disabled during dry-run; no hardware probe was executed"
                    .to_string(),
            );
        }
        let diagnose = match diagnostic_json(pier_path, "diagnose-host") {
            Ok(value) => value,
            Err(error) => {
                return (
                    DEFAULT_CONFIG.as_bytes().to_vec(),
                    format!("multi-monitor safe-auto: disabled ({error})"),
                );
            }
        };
        let nvapi = match diagnostic_json(pier_path, "nvapi-inventory") {
            Ok(value) => value,
            Err(error) => {
                return (
                    DEFAULT_CONFIG.as_bytes().to_vec(),
                    format!("multi-monitor safe-auto: disabled ({error})"),
                );
            }
        };
        safe_auto_windows_config_from_reports(DEFAULT_CONFIG, &diagnose, &nvapi).unwrap_or_else(
            |error| {
                (
                    DEFAULT_CONFIG.as_bytes().to_vec(),
                    format!("multi-monitor safe-auto: disabled ({error})"),
                )
            },
        )
    }

    fn diagnostic_json(pier_path: &Path, command: &str) -> Result<serde_json::Value, String> {
        let output = Command::new(pier_path)
            .args([command, "--json"])
            .output()
            .map_err(|error| format!("could not run {command}: {error}"))?;
        if !output.status.success() {
            return Err(format!(
                "{command} failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        serde_json::from_slice(&output.stdout)
            .map_err(|error| format!("{command} returned invalid JSON: {error}"))
    }

    fn safe_auto_windows_config_from_reports(
        default_config: &str,
        diagnose: &serde_json::Value,
        nvapi: &serde_json::Value,
    ) -> Result<(Vec<u8>, String), String> {
        let adapters = diagnose
            .get("adapters")
            .and_then(serde_json::Value::as_array)
            .ok_or("diagnose-host did not report adapters")?;
        let gpus = nvapi
            .get("gpus")
            .and_then(serde_json::Value::as_array)
            .ok_or("nvapi-inventory did not report GPUs")?;

        let mut matches = Vec::new();
        for adapter in adapters {
            let description = adapter
                .get("description")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            let eligible = adapter.get("vendor_id").and_then(serde_json::Value::as_u64)
                == Some(0x10de)
                && !adapter
                    .get("software")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(true)
                && adapter
                    .get("direct_nvenc_candidate")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
            if !eligible || description.is_empty() {
                continue;
            }
            for gpu in gpus {
                let full_name = gpu
                    .get("full_name")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                let quadro = gpu
                    .get("quadro")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                let display_count = gpu
                    .get("displays")
                    .and_then(serde_json::Value::as_array)
                    .map_or(0, Vec::len);
                if quadro
                    && display_count >= 2
                    && normalized_nvidia_name(description)
                        .eq_ignore_ascii_case(normalized_nvidia_name(full_name))
                {
                    matches.push((description.to_string(), display_count));
                }
            }
        }
        if matches.len() != 1 {
            return Err(format!(
                "expected exactly one unambiguous NVENC-capable Quadro/GRID adapter, found {}",
                matches.len()
            ));
        }
        let (adapter, display_count) = &matches[0];
        let max_monitors = (*display_count).min(2);
        let mut config: serde_json::Value = serde_json::from_str(default_config)
            .map_err(|error| format!("packaged default config is invalid: {error}"))?;
        config["platform"]["multi_monitor"] = serde_json::json!({
            "advertise_enabled": true,
            "allowed_adapters": [adapter],
            "max_monitors": max_monitors,
            "nvenc_session_limit": null,
            "allow_software_fallback": true,
            "nvidia_headless_enabled": true
        });
        let payload = serde_json::to_vec_pretty(&config)
            .map_err(|error| format!("serialize safe-auto config: {error}"))?;
        Ok((
            payload,
            format!(
                "multi-monitor safe-auto: enabled {max_monitors} native NVIDIA headless displays on {adapter}; NVENC capacity uses measured runtime admission"
            ),
        ))
    }

    fn normalized_nvidia_name(name: &str) -> &str {
        name.strip_prefix("NVIDIA ").unwrap_or(name)
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
    fn open_firewall(opts: &Options) {
        if opts.dry_run || opts.staging() {
            return;
        }
        let _ = Command::new("netsh")
            .args([
                "advfirewall",
                "firewall",
                "delete",
                "rule",
                "name=Arcen Pier 18443",
            ])
            .status();
        let status = Command::new("netsh")
            .args([
                "advfirewall",
                "firewall",
                "add",
                "rule",
                "name=Arcen Pier QUIC 18444",
                "dir=in",
                "action=allow",
                "protocol=UDP",
                "localport=18444",
            ])
            .status();
        if matches!(status, Ok(status) if status.success()) {
            println!("firewall: opened 18444/udp");
        } else {
            println!("warning: could not open 18444/udp automatically; open it manually");
        }
    }

    /// Bring the service up after installation.
    fn start_service(opts: &Options) -> Result<(), String> {
        if opts.dry_run || opts.staging() {
            println!("staging or dry-run: service not started");
            return Ok(());
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
        let mut state = String::new();
        for _ in 0..20 {
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
        } else {
            println!("service: registered but not running (state: {state})");
        }
        Ok(())
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
        let _ = Command::new("sc.exe")
            .arg("stop")
            .arg(&opts.service_name)
            .status();
        for _ in 0..30 {
            let running = Command::new("sc.exe")
                .arg("query")
                .arg(&opts.service_name)
                .output()
                .map(|output| String::from_utf8_lossy(&output.stdout).contains("RUNNING"))
                .unwrap_or(false);
            if !running {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        Err(format!(
            "{} did not stop within 15 seconds; stop it and retry the uninstall",
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
    /// The configuration is the one thing on a Pier the installer cannot
    /// reconstruct. GPU pinning, monitor layout and transport tuning are site
    /// facts, not product defaults, and `safe_auto_windows_config` deliberately
    /// pins no adapter. So a purge-and-reinstall on a multi-GPU workstation
    /// silently moved streaming onto whichever adapter enumerated first —
    /// observed on a host where the second card is reserved for other work.
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
        if path.exists() && fs::read(path).is_ok_and(|existing| existing == bytes) {
            println!("unchanged: {}", path.display());
            verify_acl(opts, path, acl_class)?;
            return Ok(());
        }
        if executable && service_running(&opts.service_name)? {
            return Err(format!(
                "service {} is running; refusing to replace {} without stopping it first",
                opts.service_name,
                path.display()
            ));
        }
        if opts.dry_run {
            println!("dry-run: write {} bytes to {}", bytes.len(), path.display());
            return Ok(());
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
            println!("rollback backup: {}", backup.display());
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

    fn ensure_tls(opts: &Options, tls: &Path) -> Result<(), String> {
        let cert = tls.join("host.crt");
        let key = tls.join("host.key");
        // `--force` replaces the certificate, matching the Linux installer.
        //
        // Without the force check this returned early whenever a certificate
        // existed, so there was no way to change the names it covers short of
        // deleting the files by hand -- and `--uninstall` does not help either,
        // because it deliberately keeps ProgramData. An operator adding
        // `--extra-san` to a host that is already installed therefore saw the
        // option accepted, the install succeed, and the Deck keep rejecting the
        // same certificate.
        if cert.exists() && key.exists() && !opts.force {
            println!(
                "keeping existing TLS material in {} (pass --force to replace it, \
                 for example after adding --extra-san)",
                tls.display()
            );
            apply_secret_file_acl(opts, &key)?;
            return Ok(());
        }
        if cert.exists() || key.exists() {
            println!(
                "--force: replacing the TLS certificate in {}",
                tls.display()
            );
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
        let keypair = KeyPair::generate().map_err(|e| format!("generate TLS key: {e}"))?;
        let certificate = params
            .self_signed(&keypair)
            .map_err(|e| format!("self-sign TLS cert: {e}"))?;
        atomic_write(opts, &cert, certificate.pem().as_bytes(), false)?;
        atomic_write_with_acl(
            opts,
            &key,
            keypair.serialize_pem().as_bytes(),
            false,
            AclClass::SecretFile,
        )
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
        println!(
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
        if acl_class.secret() {
            // Belt and braces after the grants are already in place: the file
            // is readable throughout, so an interruption here is recoverable.
            run_or_print(
                opts,
                Command::new("icacls")
                    .arg(path)
                    .args(["/remove:g", "*S-1-5-32-545", "*S-1-5-11"]),
            )?;
        }
        verify_acl(opts, path, acl_class)
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
        println!("ACL {} {}", path.display(), sddl);
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
        println!(
            "service {} BinaryPathName: {}",
            opts.service_name, binary_path
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
                println!("{}", stdout.trim_end());
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
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => Err(format!(
                "Credential Provider is still loaded by LogonUI and cannot be removed yet: {}. \
                 Reboot Windows, then rerun this same --uninstall{} command",
                path.display(),
                if opts.purge { " --purge" } else { "" }
            )),
            Err(error) => Err(format!("remove {}: {error}", path.display())),
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
        /// one thing there the installer cannot rebuild: `safe_auto_windows_config`
        /// pins no adapter, so a purge-and-reinstall on a multi-GPU host quietly
        /// moved streaming to whichever adapter enumerated first.
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

        fn diagnose_adapter(description: &str) -> serde_json::Value {
            serde_json::json!({
                "adapters": [{
                    "description": description,
                    "vendor_id": 0x10de,
                    "software": false,
                    "direct_nvenc_candidate": true
                }]
            })
        }

        fn nvapi_gpu(full_name: &str, display_count: usize) -> serde_json::Value {
            serde_json::json!({
                "gpus": [{
                    "full_name": full_name,
                    "quadro": true,
                    "displays": (0..display_count)
                        .map(|display_id| serde_json::json!({"display_id": display_id + 1}))
                        .collect::<Vec<_>>()
                }]
            })
        }

        #[test]
        fn safe_auto_enables_one_unambiguous_quadro_grid_adapter() {
            let (payload, diagnostic) = safe_auto_windows_config_from_reports(
                DEFAULT_CONFIG,
                &diagnose_adapter("NVIDIA GRID V100D-16Q"),
                &nvapi_gpu("GRID V100D-16Q", 4),
            )
            .expect("unambiguous GRID adapter");
            let config: serde_json::Value = serde_json::from_slice(&payload).unwrap();
            let multi = &config["platform"]["multi_monitor"];
            assert_eq!(multi["advertise_enabled"], true);
            assert_eq!(multi["allowed_adapters"][0], "NVIDIA GRID V100D-16Q");
            assert_eq!(multi["max_monitors"], 2);
            assert!(multi["nvenc_session_limit"].is_null());
            assert_eq!(multi["allow_software_fallback"], true);
            assert_eq!(multi["nvidia_headless_enabled"], true);
            assert!(diagnostic.contains("measured runtime admission"));
        }

        #[test]
        fn safe_auto_rejects_ambiguous_multi_gpu_hosts() {
            let diagnose = serde_json::json!({
                "adapters": [
                    {
                        "description": "NVIDIA GRID V100D-16Q",
                        "vendor_id": 0x10de,
                        "software": false,
                        "direct_nvenc_candidate": true
                    },
                    {
                        "description": "NVIDIA GRID RTX6000-8Q",
                        "vendor_id": 0x10de,
                        "software": false,
                        "direct_nvenc_candidate": true
                    }
                ]
            });
            let nvapi = serde_json::json!({
                "gpus": [
                    {
                        "full_name": "GRID V100D-16Q",
                        "quadro": true,
                        "displays": [{}, {}]
                    },
                    {
                        "full_name": "GRID RTX6000-8Q",
                        "quadro": true,
                        "displays": [{}, {}]
                    }
                ]
            });
            assert!(
                safe_auto_windows_config_from_reports(DEFAULT_CONFIG, &diagnose, &nvapi)
                    .unwrap_err()
                    .contains("found 2")
            );
        }

        #[test]
        fn safe_auto_rejects_identically_named_multi_gpu_hosts() {
            let adapter = serde_json::json!({
                "description": "NVIDIA RTX 6000 Ada Generation",
                "vendor_id": 0x10de,
                "software": false,
                "direct_nvenc_candidate": true
            });
            let gpu = serde_json::json!({
                "full_name": "NVIDIA RTX 6000 Ada Generation",
                "quadro": true,
                "displays": [{}, {}]
            });
            let diagnose = serde_json::json!({
                "adapters": [adapter.clone(), adapter]
            });
            let nvapi = serde_json::json!({
                "gpus": [gpu.clone(), gpu]
            });
            assert!(
                safe_auto_windows_config_from_reports(DEFAULT_CONFIG, &diagnose, &nvapi)
                    .unwrap_err()
                    .contains("found 4")
            );
        }

        #[test]
        fn safe_auto_rejects_hosts_without_an_eligible_nvidia_adapter() {
            let diagnose = serde_json::json!({
                "adapters": [{
                    "description": "Microsoft Basic Render Driver",
                    "vendor_id": 0x1414,
                    "software": true,
                    "direct_nvenc_candidate": false
                }]
            });
            assert!(
                safe_auto_windows_config_from_reports(
                    DEFAULT_CONFIG,
                    &diagnose,
                    &serde_json::json!({"gpus": []})
                )
                .unwrap_err()
                .contains("found 0")
            );
        }

        #[test]
        fn safe_auto_rejects_incomplete_probe_output() {
            assert_eq!(
                safe_auto_windows_config_from_reports(
                    DEFAULT_CONFIG,
                    &serde_json::json!({}),
                    &serde_json::json!({"gpus": []})
                )
                .unwrap_err(),
                "diagnose-host did not report adapters"
            );
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
