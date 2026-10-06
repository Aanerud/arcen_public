use std::env;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use arcen_transport::cert_marker::{self, OwnershipMarker};
use arcen_transport::cert_provisioning::{
    MaterialOwnership, MaterialState, ProvisioningAction, ProvisioningRefusal, ProvisioningRequest,
    plan,
};
use arcen_transport::cert_transaction::{
    FileBefore, FileRecovery, MANAGED_FILES, TransactionJournal, TransactionPhase,
};

/// Canonical location of the corresponding source.
///
/// The installer is a distributed binary of an AGPL-3.0 work, so the offer
/// belongs in it too, not only in the Pier it installs.
const SOURCE_URL: &str = "https://github.com/Aanerud/arcen_public";
/// AGPL-3.0 section 13 source offer, surfaced by `--version`.
const SOURCE_OFFER: &str = "Arcen is free software under the GNU AGPL-3.0. It comes with ABSOLUTELY NO WARRANTY. \
     You may redistribute it under the terms of that licence. If you run a modified version \
     that others connect to over a network, you must offer them its corresponding source.";

#[path = "../../../quic_config_migration.rs"]
mod quic_config_migration;

const PIER: &[u8] = include_bytes!(env!("ARCEN_PIER_BINARY_ABS"));

/// Where the Pier binary lives.
///
/// `/opt/<vendor>` is what the FHS reserves for add-on application software
/// packages, which is exactly what a vendor-shipped Pier is. The installer
/// previously used `/usr/local/libexec/arcen`, but `/usr/local` is reserved for
/// software the local administrator builds and installs, so a distributed
/// binary landing there is the wrong side of that boundary.
const PIER_DIR: &str = "/opt/arcen/bin";
const PIER_PATH: &str = "/opt/arcen/bin/arcen-pier";

/// Symlink that puts the Pier on a normal `PATH`.
///
/// Administration is CLI-based, and before this an administrator had to type
/// the full path to run any of it. `/usr/local/bin` is the right home for the
/// link because it belongs to the administrator rather than to the
/// distribution's package manager, so this cannot collide with a
/// distro-packaged file.
const PIER_SYMLINK: &str = "/usr/local/bin/arcen-pier";

/// Pre-`/opt` install location, removed on install and uninstall.
///
/// Leaving it in place is not harmless: an operator who had previously
/// installed or hand-deployed a Pier would keep a second binary of the same
/// name, and `arcen-pier` resolved through `PATH` could then be a different
/// build from the one systemd runs.
const LEGACY_PIER_DIR: &str = "/usr/local/libexec/arcen";
const LEGACY_PIER_PATH: &str = "/usr/local/libexec/arcen/arcen-pier";
/// Ownership marker name, shared with Windows, macOS and the Linux helper.
const MARKER_FILE: &str = "host.generated-by-arcen";
/// Whole-certificate pin file.
const CERT_PIN_FILE: &str = "host.cert-sha256";
/// Subject public key pin file.
const SPKI_PIN_FILE: &str = "host.spki-sha256";
/// How close to expiry counts as due for renewal.
const RENEW_WITHIN_SECONDS: i64 = 30 * 24 * 60 * 60;
const JOURNAL_FILE: &str = ".arcen-cert.transaction";
const LOCK_FILE: &str = ".arcen-cert.lock";

const SERVICE_TEMPLATE: &str = include_str!("../../arcen-pier.service");
const CONFIG_TEMPLATE: &str = include_str!("../../arcen-pier.json");
const XORG_TEMPLATE: &str = include_str!("../../arcen-xorg.conf");
const LOGROTATE_CONF: &str = include_str!("../../arcen-pier.logrotate");
/// Third-party notices, shipped with the binary rather than kept only in the
/// repository. The Pier statically links the Cisco OpenH264 source through
/// `openh264-sys2`, and BSD-2-Clause requires binary distributions to reproduce
/// that notice. A single-binary installer that omitted it would be
/// redistributing the codec without its licence text.
const THIRD_PARTY_NOTICES: &str = include_str!("../../../../legal/THIRD_PARTY_NOTICES.md");
/// The administration guide, placed on the host so a sysadmin can tune the
/// Pier without going back to the repository.
const ADMIN_GUIDE: &str = include_str!("../../../../docs/operations/pier-administration.md");

/// Runtime commands the Pier invokes. Checked before anything is written, so a
/// host missing a dependency is told up front rather than after a half
/// install that leaves a service which cannot start.
const REQUIRED_COMMANDS: &[(&str, &str)] = &[
    (
        "/usr/bin/openssl",
        "openssl, used to generate the TLS host certificate",
    ),
    (
        "/usr/bin/xauth",
        "xauth, used to authorise the dedicated X server",
    ),
    (
        "/usr/bin/systemctl",
        "systemd, used to register and run the service",
    ),
];

/// Optional at install time, but the Pier cannot serve a session without them.
/// Reported as warnings so an operator can fix the host afterwards rather than
/// being blocked.
const RECOMMENDED_COMMANDS: &[(&str, &str)] = &[
    (
        "/usr/libexec/Xorg",
        "Xorg, required for the dedicated session display",
    ),
    (
        "/usr/bin/pactl",
        "PulseAudio client tools, required for audio capture",
    ),
];

#[derive(Debug)]
struct Options {
    prefix: PathBuf,
    dry_run: bool,
    uninstall: bool,
    purge: bool,
    force: bool,
    /// Skip enabling and starting the service. Used by staging validation so a
    /// test install cannot fight the live unit for the listening port.
    no_service: bool,
    /// Restart an already-running Pier so the freshly installed binary takes
    /// effect immediately. Off by default because a restart drops every live
    /// remote session on the host.
    restart: bool,
    /// Extra names or addresses to place in the generated TLS certificate.
    ///
    /// The certificate is otherwise built from what the machine can see of
    /// itself, and a host published through NAT or a firewall is dialled on an
    /// address that appears on none of its interfaces. The operator knows that
    /// value; nothing on the host does.
    extra_sans: Vec<String>,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("install-arcen-pier: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let options = parse_args()?;
    if current_euid()? != 0 {
        return Err("must run as root".to_string());
    }
    if options.uninstall {
        uninstall(&options)
    } else {
        install(&options)
    }
}

fn parse_args() -> Result<Options, String> {
    let mut options = Options {
        prefix: PathBuf::from("/"),
        dry_run: false,
        uninstall: false,
        purge: false,
        force: false,
        no_service: false,
        restart: false,
        extra_sans: Vec::new(),
    };
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--prefix" => {
                options.prefix = PathBuf::from(
                    args.next()
                        .ok_or_else(|| "--prefix requires a directory".to_string())?,
                );
            }
            "--dry-run" => options.dry_run = true,
            "--uninstall" => options.uninstall = true,
            "--purge" => options.purge = true,
            "--force" => options.force = true,
            "--no-service" => options.no_service = true,
            "--restart" => options.restart = true,
            "--extra-san" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--extra-san requires a DNS name or IP address".to_string())?;
                for entry in value.split(',') {
                    let entry = entry.trim();
                    if !entry.is_empty() {
                        options.extra_sans.push(entry.to_ascii_lowercase());
                    }
                }
            }
            "--version" => {
                println!("install-arcen-pier {}", env!("CARGO_PKG_VERSION"));
                println!("{SOURCE_OFFER}");
                println!("Source: {SOURCE_URL}");
                std::process::exit(0);
            }
            "--help" | "-h" => {
                println!(
                    "Usage: install-arcen-pier [--prefix DIR] [--dry-run] [--force] [--restart]\n\
                     \x20                        [--no-service] [--uninstall] [--purge] [--version]\n\
                     \x20                        [--extra-san NAME-OR-IP]\n\
                     \n\
                     --extra-san  Add a DNS name or IP address to the generated TLS\n\
                     \x20            certificate. Repeatable, or comma-separated. Use this when\n\
                     \x20            the host is reached through NAT or a firewall: the\n\
                     \x20            certificate is built from what the machine can see of\n\
                     \x20            itself, and it cannot see the public address a Deck dials.\n\
                     \x20            Without it the Deck reports \"certificate not valid for\n\
                     \x20            name ...\". Only affects a certificate being generated, so\n\
                     \x20            pass --force to replace one that already exists."
                );
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(options)
}

/// Refuse to start writing until the host can actually run what we install.
///
/// A half install that produces a service which cannot start is worse than a
/// refusal, because the operator has to work out which of several missing
/// pieces is the cause.
fn migrate_existing_config(options: &Options) -> Result<(), String> {
    const CONFIG_PATH: &str = "/etc/arcen/pier.json";
    const BACKUP_PATH: &str = "/etc/arcen/pier.json.pre-quic";

    let target = map_path(&options.prefix, CONFIG_PATH);
    if !target.exists() {
        return Ok(());
    }
    let original =
        fs::read(&target).map_err(|error| format!("read {}: {error}", target.display()))?;
    let Some(migrated) = quic_config_migration::migrate_quic_product_config(&original)? else {
        return Ok(());
    };
    write_atomic(options, BACKUP_PATH, &original, 0o644, false)?;
    write_atomic(options, CONFIG_PATH, &migrated, 0o644, true)?;
    println!(
        "migrated {} to QUIC/UDP 18444 and TLS 1.3; rollback copy: {}",
        target.display(),
        map_path(&options.prefix, BACKUP_PATH).display()
    );
    Ok(())
}

fn preflight(options: &Options) -> Result<(), String> {
    if !is_root_prefix(&options.prefix) {
        return Ok(());
    }
    let mut missing = Vec::new();
    for (path, why) in REQUIRED_COMMANDS {
        if !Path::new(path).exists() {
            missing.push(format!("  {path}  ({why})"));
        }
    }
    if !missing.is_empty() {
        return Err(format!(
            "this host is missing required commands:\n{}\n\nInstall them and run the installer again.",
            missing.join("\n")
        ));
    }
    for (path, why) in RECOMMENDED_COMMANDS {
        if !Path::new(path).exists() {
            println!("warning: {path} is absent ({why}); sessions will fail until it is installed");
        }
    }
    Ok(())
}

/// Open the Pier's listening port where a recognised firewall is running.
///
/// Best effort by design: a host with no firewall, or one we do not recognise,
/// is not an install failure. It is reported so the operator knows to open the
/// QUIC port themselves.
fn open_firewall(options: &Options) {
    if options.dry_run || !is_root_prefix(&options.prefix) {
        return;
    }
    if Path::new("/usr/bin/firewall-cmd").exists() {
        // `firewall-cmd --permanent` needs the daemon, so an installed but
        // stopped firewalld used to read as a failed update and told the
        // operator to fix a firewall that was not filtering anything.
        let running = Command::new("/usr/bin/firewall-cmd")
            .arg("--state")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        if !running {
            println!(
                "firewalld: installed but not running, so nothing was changed; if you start it, \
                 open 18444/udp"
            );
            return;
        }
        let quic = Command::new("/usr/bin/firewall-cmd")
            .args(["--permanent", "--add-port=18444/udp"])
            .status();
        let _remove_legacy = Command::new("/usr/bin/firewall-cmd")
            .args(["--permanent", "--remove-port=18443/tcp"])
            .status();
        let reload = Command::new("/usr/bin/firewall-cmd")
            .arg("--reload")
            .status();
        if matches!(quic, Ok(status) if status.success())
            && matches!(reload, Ok(status) if status.success())
        {
            println!("firewalld: opened 18444/udp and removed legacy 18443/tcp");
            return;
        }
        println!(
            "warning: firewalld QUIC update failed; open 18444/udp and remove legacy 18443/tcp manually"
        );
        return;
    }
    if Path::new("/usr/sbin/ufw").exists() {
        let quic = Command::new("/usr/sbin/ufw")
            .args(["allow", "18444/udp"])
            .status();
        let _remove_legacy = Command::new("/usr/sbin/ufw")
            .args(["delete", "allow", "18443/tcp"])
            .status();
        if matches!(quic, Ok(status) if status.success()) {
            println!("ufw: opened 18444/udp and removed legacy 18443/tcp");
            return;
        }
        println!(
            "warning: ufw QUIC update failed; open 18444/udp and remove legacy 18443/tcp manually"
        );
        return;
    }
    println!(
        "note: no recognised firewall found; ensure UDP 18444 is reachable and legacy TCP 18443 is closed"
    );
}

/// What bringing the unit up achieved.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ServiceOutcome {
    /// Dry run, staging prefix, or `--no-service`: nothing was started.
    NotRequested,
    /// systemd settled on `active`.
    Running,
    /// systemd settled on anything else.
    NotRunning(String),
}

/// Carries the unit's outcome through the shared installer transaction, whose
/// `finish` is the only thing that decides whether the install succeeded.
fn finish_install(
    mut transaction: arcen_session::install_lifecycle::InstallTransaction,
    outcome: &ServiceOutcome,
) -> Result<(), String> {
    use arcen_session::install_lifecycle::InstallEvent;
    let events: &[InstallEvent] = match outcome {
        ServiceOutcome::NotRequested => &[InstallEvent::ServiceNotRequested],
        ServiceOutcome::Running => &[
            InstallEvent::ServiceQuiesced,
            InstallEvent::ActivationCommitted,
            InstallEvent::SmokePassed,
        ],
        ServiceOutcome::NotRunning(_) => &[
            InstallEvent::ServiceQuiesced,
            InstallEvent::ActivationCommitted,
            InstallEvent::TransactionFailed,
        ],
    };
    for event in events {
        transaction
            .apply(*event)
            .map_err(|error| format!("installer transaction: {error}"))?;
    }
    transaction
        .finish()
        .map(|_| ())
        .map_err(|error| match outcome {
            ServiceOutcome::NotRunning(state) => format!(
                "{error}: arcen-pier.service is {state:?}, not active. The files are installed; \
             see `journalctl -u arcen-pier` for why the service stopped"
            ),
            _ => error.to_string(),
        })
}

fn install(options: &Options) -> Result<(), String> {
    use arcen_session::install_lifecycle::{InstallEvent, InstallTransaction};
    let mut transaction = InstallTransaction::new();
    preflight(options)?;
    transaction
        .apply(InstallEvent::PreflightPassed)
        .map_err(|error| format!("installer transaction: {error}"))?;
    create_dir(options, PIER_DIR, 0o755)?;
    create_dir(options, "/etc/arcen", 0o755)?;
    create_dir(options, "/var/log/arcen", 0o750)?;
    create_dir(options, "/run/arcen", 0o755)?;
    create_dir(options, "/usr/share/doc/arcen", 0o755)?;
    write_atomic(options, PIER_PATH, PIER, 0o755, true)?;
    retire_legacy_pier(options)?;
    link_pier_onto_path(options)?;
    write_atomic(
        options,
        "/usr/share/doc/arcen/THIRD_PARTY_NOTICES.md",
        THIRD_PARTY_NOTICES.as_bytes(),
        0o644,
        true,
    )?;
    write_atomic(
        options,
        "/usr/share/doc/arcen/pier-administration.md",
        ADMIN_GUIDE.as_bytes(),
        0o644,
        true,
    )?;
    write_atomic(
        options,
        "/etc/logrotate.d/arcen-pier",
        LOGROTATE_CONF.as_bytes(),
        0o644,
        true,
    )?;
    write_atomic(
        options,
        "/etc/systemd/system/arcen-pier.service",
        SERVICE_TEMPLATE.as_bytes(),
        0o644,
        true,
    )?;
    write_atomic(
        options,
        "/etc/arcen/pier.json",
        CONFIG_TEMPLATE.as_bytes(),
        0o644,
        options.force,
    )?;
    if !options.force {
        migrate_existing_config(options)?;
    }
    write_atomic(
        options,
        "/etc/arcen/xorg.conf",
        XORG_TEMPLATE.as_bytes(),
        0o644,
        options.force,
    )?;
    ensure_cert(options)?;
    if options.dry_run {
        println!("dry-run: would run systemctl daemon-reload when installing to /");
    } else if is_root_prefix(&options.prefix) {
        run_systemctl(&["daemon-reload"])?;
    } else {
        println!("staging prefix: skipped systemctl daemon-reload");
    }
    transaction
        .apply(InstallEvent::PayloadStaged)
        .map_err(|error| format!("installer transaction: {error}"))?;
    open_firewall(options);
    let outcome = enable_and_start(options)?;
    report_pending_restart(options);
    if is_root_prefix(&options.prefix) && !options.dry_run {
        println!();
        println!("Administration guide: /usr/share/doc/arcen/pier-administration.md");
    }
    finish_install(transaction, &outcome)
}

/// Register the unit and bring it up.
fn enable_and_start(options: &Options) -> Result<ServiceOutcome, String> {
    if options.dry_run {
        println!("dry-run: would enable and start arcen-pier.service");
        return Ok(ServiceOutcome::NotRequested);
    }
    if !is_root_prefix(&options.prefix) {
        println!("staging prefix: skipped enabling the service");
        return Ok(ServiceOutcome::NotRequested);
    }
    if options.no_service {
        println!("--no-service: installed without enabling the unit");
        return Ok(ServiceOutcome::NotRequested);
    }
    run_systemctl(&["enable", "arcen-pier.service"])?;
    let _ = Command::new("/usr/bin/systemctl")
        .args(["start", "arcen-pier.service"])
        .status();
    // Ask systemd what actually happened rather than trusting the exit status
    // of `start`. systemd accepts the start request and returns success even
    // when the unit then fails, so reporting on that status claims a running
    // service that is not running.
    //
    // Poll to a terminal state: reading `is-active` immediately after `start`
    // catches "activating" and reports success for a unit that is about to
    // fail, which is the same lie in a different place.
    let mut active = String::new();
    for _ in 0..20 {
        active = Command::new("/usr/bin/systemctl")
            .args(["is-active", "arcen-pier.service"])
            .output()
            .ok()
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
            .unwrap_or_default();
        if active != "activating" && !active.is_empty() {
            // "active" can still be a unit that dies a moment later, so give a
            // short settle window before believing it.
            std::thread::sleep(std::time::Duration::from_millis(500));
            let confirmed = Command::new("/usr/bin/systemctl")
                .args(["is-active", "arcen-pier.service"])
                .output()
                .ok()
                .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
                .unwrap_or_default();
            if confirmed == active {
                break;
            }
            active = confirmed;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    if active == "active" {
        println!("service: enabled and running");
        Ok(ServiceOutcome::Running)
    } else {
        println!("service: enabled but not running (state: {active})");
        Ok(ServiceOutcome::NotRunning(active))
    }
}

/// Remove a Pier left behind by a pre-`/opt` install.
///
/// Without this an upgraded host keeps two binaries called `arcen-pier`: the
/// one systemd now runs from `/opt`, and a stale one under `/usr/local`. They
/// drift apart on the next upgrade, and an administrator debugging with
/// an administrator command can end up reading a different build from the
/// one actually serving sessions.
fn retire_legacy_pier(options: &Options) -> Result<(), String> {
    let legacy = map_path(&options.prefix, LEGACY_PIER_PATH);
    if !legacy.exists() {
        return Ok(());
    }
    println!("migrating: superseded by {PIER_PATH}");
    remove_file(options, LEGACY_PIER_PATH)?;
    remove_dir_if_empty(options, LEGACY_PIER_DIR);
    Ok(())
}

/// Publish `arcen-pier` on `PATH` as a symlink into `/opt`.
///
/// A symlink rather than a copy, so the CLI an administrator runs is by
/// construction the same build systemd runs; two copies can disagree.
///
/// Any existing file is replaced, which is deliberate: hand-deployments left a
/// real binary at this path, and silently keeping it would preserve exactly the
/// stale-CLI problem this is here to remove.
fn link_pier_onto_path(options: &Options) -> Result<(), String> {
    let link = map_path(&options.prefix, PIER_SYMLINK);
    let target = map_path(&options.prefix, PIER_PATH);
    if options.dry_run {
        println!(
            "dry-run: symlink {} -> {}",
            link.display(),
            target.display()
        );
        return Ok(());
    }
    if let Some(parent) = link.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("create {}: {error}", parent.display()))?;
    }
    match fs::symlink_metadata(&link) {
        Ok(metadata) if metadata.is_dir() => {
            return Err(format!(
                "{} is a directory; refusing to replace it",
                link.display()
            ));
        }
        Ok(_) => fs::remove_file(&link)
            .map_err(|error| format!("replace {}: {error}", link.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("inspect {}: {error}", link.display())),
    }
    std::os::unix::fs::symlink(&target, &link)
        .map_err(|error| format!("symlink {}: {error}", link.display()))?;
    println!("linked {} -> {}", link.display(), target.display());
    Ok(())
}

/// Tell the operator when the running Pier is not the one just installed.
///
/// `systemctl start` on an already-active unit does nothing, so an in-place
/// upgrade leaves the previous process serving while the new binary sits on
/// disk unused. The installer used to report "service: enabled and running",
/// which is true and yet exactly wrong: the running executable can be an older
/// build, and after the move to `/opt` it is a binary that no longer exists on
/// disk at all.
///
/// The restart is not automatic by default because this is a remote desktop
/// host — restarting drops every live session on it. So the default is to be
/// loud and let the operator choose a moment; `--restart` opts in.
fn report_pending_restart(options: &Options) {
    if options.dry_run || !is_root_prefix(&options.prefix) || options.no_service {
        return;
    }
    let Some(running) = running_pier_executable() else {
        return;
    };
    // `/proc/<pid>/exe` keeps resolving after the file is replaced or removed,
    // and the kernel appends this marker when it has been unlinked.
    // `/proc/<pid>/exe` keeps resolving after the file it names is replaced,
    // and the kernel appends this marker once the original inode is unlinked.
    //
    // That marker is the whole signal. An in-place upgrade writes the new
    // binary at the same path, so comparing paths alone always matches and
    // reports an up-to-date service that is in fact still running the previous
    // build. This code stripped the marker before comparing and so never fired
    // on the common case, which was only visible by upgrading a host twice and
    // noticing the process start time had not moved.
    let running = running.to_string_lossy().to_string();
    let replaced_in_place = running.ends_with(" (deleted)");
    let stale_path = running.trim_end_matches(" (deleted)").to_string();
    if !replaced_in_place && stale_path == PIER_PATH {
        return;
    }
    let stale = if replaced_in_place {
        format!("{stale_path} (the build it was started from, since replaced)")
    } else {
        stale_path
    };
    if options.restart {
        println!("restarting arcen-pier so the installed binary takes effect");
        match run_systemctl(&["restart", "arcen-pier.service"]) {
            Ok(()) => println!("service: restarted on {PIER_PATH}"),
            Err(error) => eprintln!("warning: restart failed: {error}"),
        }
        return;
    }
    println!();
    println!("=====================================================================");
    println!(" NOTE: the running service is still the PREVIOUS Pier.");
    println!();
    println!("   running:   {stale}");
    println!("   installed: {PIER_PATH}");
    println!();
    println!(" Nothing was restarted, because that would disconnect every live");
    println!(" session on this host. Restart when convenient:");
    println!();
    println!("   sudo systemctl restart arcen-pier.service");
    println!();
    println!(" Or re-run this installer with --restart to do it immediately.");
    println!("=====================================================================");
}

/// Path of the executable the running Pier service is using, if it is running.
fn running_pier_executable() -> Option<PathBuf> {
    let output = Command::new("systemctl")
        .args(["show", "arcen-pier.service", "-p", "MainPID", "--value"])
        .output()
        .ok()?;
    let pid: u32 = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .ok()?;
    if pid == 0 {
        return None;
    }
    fs::read_link(format!("/proc/{pid}/exe")).ok()
}

fn uninstall(options: &Options) -> Result<(), String> {
    if options.dry_run {
        println!("dry-run: would uninstall arcen-pier service and binary");
    } else if is_root_prefix(&options.prefix) {
        let _ = Command::new("systemctl")
            .args(["stop", "arcen-pier.service"])
            .status();
        let _ = Command::new("systemctl")
            .args(["disable", "arcen-pier.service"])
            .status();
    }
    remove_file(options, PIER_PATH)?;
    remove_symlink_if_ours(options);
    remove_file(options, LEGACY_PIER_PATH)?;
    remove_file(options, "/usr/share/doc/arcen/THIRD_PARTY_NOTICES.md")?;
    remove_file(options, "/usr/share/doc/arcen/pier-administration.md")?;
    remove_file(options, "/etc/logrotate.d/arcen-pier")?;
    remove_dir_if_empty(options, PIER_DIR);
    remove_dir_if_empty(options, "/opt/arcen");
    remove_dir_if_empty(options, LEGACY_PIER_DIR);
    remove_dir_if_empty(options, "/usr/share/doc/arcen");
    remove_file(options, "/etc/systemd/system/arcen-pier.service")?;
    if options.purge {
        preserve_config_before_purge(options)?;
        remove_dir_all(options, "/etc/arcen")?;
        remove_dir_all(options, "/var/lib/arcen")?;
        remove_dir_all(options, "/var/log/arcen")?;
    }
    if !options.dry_run && is_root_prefix(&options.prefix) {
        run_systemctl(&["daemon-reload"])?;
    }
    if options.purge {
        println!("uninstall complete; configuration, runtime state and logs were removed");
    } else {
        println!(
            "uninstall complete; /etc/arcen, /var/lib/arcen and /var/log/arcen were kept (use --purge to remove them)"
        );
    }
    Ok(())
}

/// Build the `subjectAltName` extension for the generated host certificate.
///
/// A certificate with no SAN is rejected outright by the Pier's own TLS
/// validation with `MissingServerSubjectAlternativeName`, whatever
/// `tls.expected_sans` is set to, because a Common Name alone has not been an
/// acceptable identity for years. The installer previously generated exactly
/// that, so a fresh install produced a service that refused to start.
///
/// Every name a client might plausibly dial is included: the short hostname,
/// the FQDN, `localhost`, the loopback address, and each non-loopback IPv4
/// address the host currently has.
fn subject_alt_name(extra: &[String]) -> String {
    format!(
        "subjectAltName={}",
        subject_alt_name_entries(extra).join(",")
    )
}

fn subject_alt_name_entries(extra: &[String]) -> Vec<String> {
    let mut dns: Vec<String> = vec!["localhost".to_string()];
    let mut ips: Vec<String> = vec!["127.0.0.1".to_string()];

    // `-s` is required, not redundant. On a host whose configured hostname is
    // already the FQDN — the default on domain-joined RHEL — bare `hostname`
    // returns the FQDN too, so querying only `-f` and `` yields the same string
    // twice and the short name never enters the certificate. A client dialling
    // the machine by its short name then gets
    //     certificate not valid for name "pier-linux"; certificate is only valid
    //     for DnsName("localhost"), DnsName("pier-linux.ad.example.internal"), ...
    // which is exactly the name a person types.
    for args in [vec!["-f"], vec!["-s"], vec![]] {
        if let Ok(output) = Command::new("/usr/bin/hostname").args(&args).output() {
            let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !name.is_empty() && !dns.contains(&name) {
                dns.push(name);
            }
        }
    }
    // Belt and braces: derive the short name from any dotted name we collected,
    // so a host without `hostname -s` still gets both forms.
    let derived: Vec<String> = dns
        .iter()
        .filter_map(|name| name.split_once('.').map(|(short, _)| short.to_string()))
        .filter(|short| !short.is_empty())
        .collect();
    for short in derived {
        if !dns.contains(&short) {
            dns.push(short);
        }
    }
    if let Ok(output) = Command::new("/usr/bin/hostname").arg("-I").output() {
        for address in String::from_utf8_lossy(&output.stdout).split_whitespace() {
            // IPv4 only: the SAN encoding for IPv6 differs and the Pier is
            // reached over IPv4 in every deployment we support today.
            let looks_v4 = address.split('.').count() == 4
                && address
                    .split('.')
                    .all(|octet| !octet.is_empty() && octet.chars().all(|c| c.is_ascii_digit()));
            if looks_v4 && !ips.contains(&address.to_string()) {
                ips.push(address.to_string());
            }
        }
    }

    // Operator-supplied values last, classified the same way discovered ones
    // are: openssl needs `IP:` for an address and `DNS:` for a name, and an
    // address written as `DNS:` produces a certificate that silently fails to
    // match when a Deck dials the address.
    for value in extra {
        if value.parse::<std::net::Ipv4Addr>().is_ok() {
            if !ips.contains(value) {
                ips.push(value.clone());
            }
        } else if !dns.contains(value) {
            dns.push(value.clone());
        }
    }

    let mut entries: Vec<String> = dns.iter().map(|name| format!("DNS:{name}")).collect();
    entries.extend(ips.iter().map(|address| format!("IP:{address}")));
    entries
}

fn merge_san_entry(entries: &mut Vec<String>, entry: String) {
    if !entries
        .iter()
        .any(|existing| existing.eq_ignore_ascii_case(&entry))
    {
        entries.push(entry);
    }
}

/// Paths to the managed TLS material.
#[derive(Debug)]
struct TlsPaths {
    certificate: PathBuf,
    key: PathBuf,
    marker: PathBuf,
    certificate_pin: PathBuf,
    spki_pin: PathBuf,
}

impl TlsPaths {
    fn from_options(options: &Options) -> Self {
        let directory = map_path(&options.prefix, "/etc/arcen");
        Self {
            certificate: directory.join("host.crt"),
            key: directory.join("host.key"),
            marker: directory.join(MARKER_FILE),
            certificate_pin: directory.join(CERT_PIN_FILE),
            spki_pin: directory.join(SPKI_PIN_FILE),
        }
    }

    fn directory(&self) -> &Path {
        self.certificate.parent().unwrap_or(&self.certificate)
    }

    fn managed(&self) -> [&Path; MANAGED_FILES.len()] {
        [
            &self.key,
            &self.certificate,
            &self.certificate_pin,
            &self.spki_pin,
            &self.marker,
        ]
    }

    fn staging_for(path: &Path) -> PathBuf {
        let name = path.file_name().map_or_else(
            || "material".to_owned(),
            |name| name.to_string_lossy().into_owned(),
        );
        path.with_file_name(format!(".{name}.installing.{}", std::process::id()))
    }
}

struct TlsDirectoryLock {
    file: fs::File,
}

impl TlsDirectoryLock {
    fn acquire(paths: &TlsPaths) -> Result<Self, String> {
        let path = paths.directory().join(LOCK_FILE);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|error| format!("open {}: {error}", path.display()))?;
        chmod(&path, 0o600)?;
        rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive)
            .map_err(|error| format!("another host-certificate transaction is active: {error}"))?;
        Ok(Self { file })
    }
}

impl Drop for TlsDirectoryLock {
    fn drop(&mut self) {
        let _ = rustix::fs::flock(&self.file, rustix::fs::FlockOperation::Unlock);
    }
}

#[derive(Debug)]
struct CertTransaction {
    id: String,
    journal: PathBuf,
    backups: Vec<(PathBuf, PathBuf)>,
    existed: [bool; MANAGED_FILES.len()],
}

impl CertTransaction {
    fn begin(paths: &TlsPaths) -> Result<Self, String> {
        let id = format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |duration| duration.as_secs())
        );
        let journal = paths.directory().join(JOURNAL_FILE);
        let mut existed = [false; MANAGED_FILES.len()];
        for (index, path) in paths.managed().into_iter().enumerate() {
            existed[index] = path.is_file();
        }
        write_atomic_path(
            &journal,
            TransactionJournal::new(id.clone(), TransactionPhase::Prepared, existed)
                .render()
                .as_bytes(),
            0o600,
        )?;
        fsync_path(&journal)?;
        fsync_directory(paths.directory())?;

        let mut transaction = Self {
            id,
            journal,
            backups: Vec::new(),
            existed,
        };
        for (index, path) in paths.managed().into_iter().enumerate() {
            if existed[index] {
                let backup = paths.directory().join(format!(
                    ".arcen-cert.backup.{}.{}",
                    transaction.id, MANAGED_FILES[index]
                ));
                if let Err(error) = backup_rename(index, path, &backup) {
                    return match transaction.roll_back(paths) {
                        Ok(()) => Err(format!("back up {}: {error}", path.display())),
                        Err(rollback) => Err(format!(
                            "back up {}: {error}; rollback incomplete: {rollback}",
                            path.display()
                        )),
                    };
                }
                transaction
                    .backups
                    .push((path.to_path_buf(), backup.clone()));
                if let Err(error) = fsync_backup(index, &backup)
                    .and_then(|()| fsync_backup_directory(index, paths.directory()))
                {
                    return match transaction.roll_back(paths) {
                        Ok(()) => Err(error),
                        Err(rollback) => Err(format!("{error}; rollback incomplete: {rollback}")),
                    };
                }
            }
        }
        if let Err(error) = fsync_directory(paths.directory()) {
            return match transaction.roll_back(paths) {
                Ok(()) => Err(error),
                Err(rollback) => Err(format!("{error}; rollback incomplete: {rollback}")),
            };
        }
        Ok(transaction)
    }

    fn commit(self, paths: &TlsPaths) -> Result<(), String> {
        let mut existed = [true; MANAGED_FILES.len()];
        for (index, path) in paths.managed().into_iter().enumerate() {
            existed[index] = path.is_file();
        }
        write_atomic_path(
            &self.journal,
            TransactionJournal::new(self.id, TransactionPhase::Committed, existed)
                .render()
                .as_bytes(),
            0o600,
        )?;
        fsync_path(&self.journal)?;
        fsync_directory(paths.directory())?;
        for (_, backup) in &self.backups {
            fs::remove_file(backup)
                .map_err(|error| format!("remove backup {}: {error}", backup.display()))?;
        }
        fs::remove_file(&self.journal)
            .map_err(|error| format!("clear {}: {error}", self.journal.display()))?;
        fsync_directory(paths.directory())
    }

    fn roll_back(self, paths: &TlsPaths) -> Result<(), String> {
        let mut failures = Vec::new();
        for (final_path, backup) in &self.backups {
            let _ = fs::remove_file(final_path);
            if let Err(error) = fs::rename(backup, final_path) {
                failures.push(format!("restore {}: {error}", final_path.display()));
            } else if let Err(error) = fsync_path(final_path) {
                failures.push(error);
            }
        }
        for path in paths.managed() {
            let index = paths
                .managed()
                .iter()
                .position(|managed| *managed == path)
                .unwrap_or(0);
            if !self.backups.iter().any(|(backed_up, _)| backed_up == path)
                && !self.existed[index]
                && let Err(error) = fs::remove_file(path)
                && error.kind() != std::io::ErrorKind::NotFound
            {
                failures.push(format!("remove {}: {error}", path.display()));
            }
        }
        if let Err(error) = fsync_directory(paths.directory()) {
            failures.push(error);
        }
        if failures.is_empty() {
            fs::remove_file(&self.journal)
                .map_err(|error| format!("clear {}: {error}", self.journal.display()))?;
            fsync_directory(paths.directory())?;
            Ok(())
        } else {
            Err(failures.join("; "))
        }
    }
}

fn backup_rename(index: usize, source: &Path, backup: &Path) -> Result<(), String> {
    maybe_inject_backup_failure(TestBackupOp::Rename, index)?;
    fs::rename(source, backup).map_err(|error| format!("back up {}: {error}", source.display()))
}

fn fsync_backup(index: usize, backup: &Path) -> Result<(), String> {
    maybe_inject_backup_failure(TestBackupOp::FileFsync, index)?;
    fsync_path(backup)
}

fn fsync_backup_directory(index: usize, directory: &Path) -> Result<(), String> {
    maybe_inject_backup_failure(TestBackupOp::DirFsync, index)?;
    fsync_directory(directory)
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TestBackupOp {
    Rename,
    FileFsync,
    DirFsync,
}

#[cfg(not(test))]
enum TestBackupOp {
    Rename,
    FileFsync,
    DirFsync,
}

#[cfg(test)]
fn test_backup_failure_slot() -> &'static std::sync::Mutex<Option<(TestBackupOp, usize)>> {
    static SLOT: std::sync::OnceLock<std::sync::Mutex<Option<(TestBackupOp, usize)>>> =
        std::sync::OnceLock::new();
    SLOT.get_or_init(|| std::sync::Mutex::new(None))
}

#[cfg(test)]
fn set_test_backup_failure(failure: Option<(TestBackupOp, usize)>) {
    *test_backup_failure_slot()
        .lock()
        .expect("test backup failure lock") = failure;
}

#[cfg(test)]
fn maybe_inject_backup_failure(op: TestBackupOp, index: usize) -> Result<(), String> {
    let mut guard = test_backup_failure_slot()
        .lock()
        .expect("test backup failure lock");
    if *guard == Some((op, index)) {
        *guard = None;
        return Err(format!("injected {op:?} failure at backup {index}"));
    }
    Ok(())
}

#[cfg(not(test))]
fn maybe_inject_backup_failure(_op: TestBackupOp, _index: usize) -> Result<(), String> {
    Ok(())
}

fn recover_interrupted(paths: &TlsPaths) -> Result<(), String> {
    let journal_path = paths.directory().join(JOURNAL_FILE);
    if !journal_path.is_file() {
        return Ok(());
    }
    let text = fs::read_to_string(&journal_path)
        .map_err(|error| format!("read {}: {error}", journal_path.display()))?;
    let journal = TransactionJournal::parse(&text)
        .map_err(|error| format!("unreadable certificate transaction journal: {error}"))?;
    for (index, path) in paths.managed().into_iter().enumerate() {
        let name = MANAGED_FILES[index];
        let backup = paths.directory().join(format!(
            ".arcen-cert.backup.{}.{name}",
            journal.transaction_id()
        ));
        let decision = journal
            .recover_file(
                name,
                FileBefore {
                    existed: path.is_file(),
                    backup_present: backup.is_file(),
                },
            )
            .map_err(|error| format!("journal decision for {name}: {error}"))?;
        match decision {
            FileRecovery::RequirePublished | FileRecovery::RequireUntouched => {
                if !path.is_file() {
                    return Err(format!(
                        "interrupted certificate transaction cannot restore {name}"
                    ));
                }
            }
            FileRecovery::RestoreBackup => {
                let _ = fs::remove_file(path);
                fs::rename(&backup, path)
                    .map_err(|error| format!("restore {}: {error}", path.display()))?;
            }
            FileRecovery::RemoveFile => {
                let _ = fs::remove_file(path);
            }
        }
        let _ = fs::remove_file(&backup);
    }
    for path in paths.managed() {
        let _ = fs::remove_file(TlsPaths::staging_for(path));
    }
    fs::remove_file(&journal_path)
        .map_err(|error| format!("clear {}: {error}", journal_path.display()))?;
    fsync_directory(paths.directory())
}

/// Reads the TLS directory into the shared provisioning input.
fn inspect_tls(paths: &TlsPaths) -> MaterialState {
    let certificate_present = paths.certificate.is_file();
    let key_present = paths.key.is_file();
    if !certificate_present && !key_present {
        return MaterialState::absent();
    }

    let certificate_bytes = std::fs::read(&paths.certificate).ok();
    let ownership = certificate_bytes
        .as_ref()
        .map(|bytes| marker_ownership(paths, bytes));
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(i64::MAX, |duration| {
            i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
        });
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
        stale_staging_present: false,
        legacy_arcen_self_signed: certificate_bytes.as_ref().is_some_and(|bytes| {
            cert_marker::is_legacy_arcen_self_signed_pem(bytes, legacy_arcen_evidence(paths, bytes))
        }),
    }
}

fn marker_ownership(paths: &TlsPaths, certificate_bytes: &[u8]) -> MaterialOwnership {
    if !paths.marker.exists() {
        return MaterialOwnership::Foreign;
    }
    if marker_matches(paths, certificate_bytes) {
        MaterialOwnership::Owned
    } else {
        MaterialOwnership::Ambiguous
    }
}

/// Returns whether the ownership marker describes the certificate on disk.
fn marker_matches(paths: &TlsPaths, certificate_bytes: &[u8]) -> bool {
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

fn legacy_arcen_evidence(
    paths: &TlsPaths,
    certificate_bytes: &[u8],
) -> cert_marker::LegacyArcenEvidence {
    let companion_pins_match = fs::read_to_string(&paths.certificate_pin)
        .ok()
        .zip(fs::read_to_string(&paths.spki_pin).ok())
        .is_some_and(|(certificate_pin, spki_pin)| {
            cert_marker::companion_pins_match_pem(certificate_bytes, &certificate_pin, &spki_pin)
        });
    cert_marker::LegacyArcenEvidence {
        arcen_tls_directory: true,
        companion_pins_match,
        machine_sans_match: false,
    }
}

/// Writes the pin files and ownership marker beside the active certificate.
fn write_pins_and_marker(paths: &TlsPaths) -> Result<(), String> {
    let bytes = std::fs::read(&paths.certificate)
        .map_err(|error| format!("read {}: {error}", paths.certificate.display()))?;
    let pins = cert_marker::pins_from_pem(&bytes)
        .ok_or_else(|| "cannot pin the generated certificate".to_string())?;
    let marker = OwnershipMarker::new(&pins.certificate, &pins.spki)
        .map_err(|error| format!("ownership marker: {error}"))?;

    write_atomic_path(
        &paths.certificate_pin,
        format!(
            "sha256 Fingerprint={}\n",
            cert_marker::colon_hex(&pins.certificate)
        )
        .as_bytes(),
        0o644,
    )?;
    write_atomic_path(
        &paths.spki_pin,
        format!("{}\n", pins.spki).as_bytes(),
        0o644,
    )?;
    write_atomic_path(&paths.marker, marker.render().as_bytes(), 0o644)
}

fn write_atomic_path(target: &Path, content: &[u8], mode: u32) -> Result<(), String> {
    let staged = TlsPaths::staging_for(target);
    {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staged)
            .map_err(|error| format!("create {}: {error}", staged.display()))?;
        file.write_all(content)
            .map_err(|error| format!("write {}: {error}", staged.display()))?;
        file.sync_all()
            .map_err(|error| format!("sync {}: {error}", staged.display()))?;
    }
    chmod(&staged, mode)?;
    fs::rename(&staged, target)
        .map_err(|error| format!("install {}: {error}", target.display()))?;
    fsync_path(target)?;
    if let Some(parent) = target.parent() {
        fsync_directory(parent)?;
    }
    Ok(())
}

fn write_file_path(path: &Path, content: &[u8], mode: u32) -> Result<(), String> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| format!("create {}: {error}", path.display()))?;
    chmod(path, mode)?;
    file.write_all(content)
        .map_err(|error| format!("write {}: {error}", path.display()))?;
    file.sync_all()
        .map_err(|error| format!("sync {}: {error}", path.display()))
}

fn fsync_path(path: &Path) -> Result<(), String> {
    fs::File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(|error| format!("sync {}: {error}", path.display()))
}

fn fsync_directory(path: &Path) -> Result<(), String> {
    fs::File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("sync directory {}: {error}", path.display()))
}

fn issue_certificate(
    options: &Options,
    paths: &TlsPaths,
    reuse_key: bool,
    san_entries: &[String],
) -> Result<(), String> {
    if options.dry_run {
        return Ok(());
    }
    let staged_key = TlsPaths::staging_for(&paths.key);
    let staged_cert = TlsPaths::staging_for(&paths.certificate);
    let staged_cert_pin = TlsPaths::staging_for(&paths.certificate_pin);
    let staged_spki_pin = TlsPaths::staging_for(&paths.spki_pin);
    let staged_marker = TlsPaths::staging_for(&paths.marker);
    for path in [
        &staged_key,
        &staged_cert,
        &staged_cert_pin,
        &staged_spki_pin,
        &staged_marker,
    ] {
        let _ = fs::remove_file(path);
    }
    let key_for_cert = if reuse_key {
        fs::copy(&paths.key, &staged_key)
            .map_err(|error| format!("stage existing TLS key {}: {error}", paths.key.display()))?;
        chmod(&staged_key, 0o600)?;
        staged_key.clone()
    } else {
        let status = Command::new("openssl")
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
            let _ = fs::remove_file(&staged_key);
            return Err("openssl key generation failed".to_string());
        }
        chmod(&staged_key, 0o600)?;
        staged_key.clone()
    };

    let san = format!("subjectAltName={}", san_entries.join(","));
    let status = Command::new("openssl")
        .args(["req", "-x509", "-new", "-sha256", "-days", "825"])
        .arg("-key")
        .arg(&key_for_cert)
        .arg("-out")
        .arg(&staged_cert)
        .args(["-subj", "/CN=Arcen Pier"])
        .args(["-addext", "basicConstraints=critical,CA:FALSE"])
        .args(["-addext", "keyUsage=critical,digitalSignature"])
        .args(["-addext", "extendedKeyUsage=serverAuth"])
        .args(["-addext", &san])
        .status()
        .map_err(|error| format!("start openssl certificate generation: {error}"))?;
    if !status.success() {
        let _ = fs::remove_file(&staged_key);
        let _ = fs::remove_file(&staged_cert);
        return Err("openssl certificate generation failed".to_string());
    }

    chmod(&staged_cert, 0o644)?;
    fsync_path(&staged_key)?;
    fsync_path(&staged_cert)?;
    verify_key_matches_certificate(&staged_cert, &key_for_cert)?;
    verify_certificate_covers(&staged_cert, san_entries)?;
    let staged_cert_bytes = fs::read(&staged_cert)
        .map_err(|error| format!("read {}: {error}", staged_cert.display()))?;
    let pins = cert_marker::pins_from_pem(&staged_cert_bytes)
        .ok_or_else(|| "cannot pin the generated certificate".to_string())?;
    let marker = OwnershipMarker::new(&pins.certificate, &pins.spki)
        .map_err(|error| format!("ownership marker: {error}"))?;
    write_file_path(
        &staged_cert_pin,
        format!(
            "sha256 Fingerprint={}\n",
            cert_marker::colon_hex(&pins.certificate)
        )
        .as_bytes(),
        0o644,
    )?;
    write_file_path(
        &staged_spki_pin,
        format!("{}\n", pins.spki).as_bytes(),
        0o644,
    )?;
    write_file_path(&staged_marker, marker.render().as_bytes(), 0o644)?;

    let transaction = CertTransaction::begin(paths)?;
    let publish = (|| {
        fs::rename(&staged_key, &paths.key)
            .map_err(|error| format!("install {}: {error}", paths.key.display()))?;
        chmod(&paths.key, 0o600)?;
        fs::rename(&staged_cert, &paths.certificate)
            .map_err(|error| format!("install {}: {error}", paths.certificate.display()))?;
        fs::rename(&staged_cert_pin, &paths.certificate_pin)
            .map_err(|error| format!("install {}: {error}", paths.certificate_pin.display()))?;
        fs::rename(&staged_spki_pin, &paths.spki_pin)
            .map_err(|error| format!("install {}: {error}", paths.spki_pin.display()))?;
        fs::rename(&staged_marker, &paths.marker)
            .map_err(|error| format!("install {}: {error}", paths.marker.display()))?;
        chmod(&paths.certificate, 0o644)?;
        chmod(&paths.certificate_pin, 0o644)?;
        chmod(&paths.spki_pin, 0o644)?;
        chmod(&paths.marker, 0o644)?;
        for path in paths.managed() {
            fsync_path(path)?;
        }
        fsync_directory(paths.directory())?;
        verify_key_matches_certificate(&paths.certificate, &paths.key)
    })();
    match publish {
        Ok(()) => transaction.commit(paths),
        Err(error) => match transaction.roll_back(paths) {
            Ok(()) => Err(error),
            Err(rollback) => Err(format!("{error}; rollback incomplete: {rollback}")),
        },
    }
}

fn openssl_stdout(args: &[&str], input: Option<&[u8]>) -> Result<Vec<u8>, String> {
    let mut command = Command::new("openssl");
    command.args(args);
    if input.is_some() {
        command.stdin(std::process::Stdio::piped());
    }
    command.stdout(std::process::Stdio::piped());
    command.stderr(std::process::Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|error| format!("start openssl {}: {error}", args.join(" ")))?;
    if let Some(input) = input {
        child
            .stdin
            .as_mut()
            .ok_or_else(|| "open openssl stdin".to_string())?
            .write_all(input)
            .map_err(|error| format!("write openssl stdin: {error}"))?;
    }
    let output = child
        .wait_with_output()
        .map_err(|error| format!("wait for openssl {}: {error}", args.join(" ")))?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(format!(
            "openssl {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

fn verify_key_matches_certificate(certificate: &Path, key: &Path) -> Result<(), String> {
    let cert_pub = openssl_stdout(
        &[
            "x509",
            "-in",
            certificate
                .to_str()
                .ok_or_else(|| "certificate path".to_string())?,
            "-pubkey",
            "-noout",
        ],
        None,
    )?;
    let cert_der = openssl_stdout(&["pkey", "-pubin", "-outform", "DER"], Some(&cert_pub))?;
    let key_der = openssl_stdout(
        &[
            "pkey",
            "-in",
            key.to_str().ok_or_else(|| "key path".to_string())?,
            "-pubout",
            "-outform",
            "DER",
        ],
        None,
    )?;
    if cert_der == key_der {
        Ok(())
    } else {
        Err("generated certificate and key do not match".to_string())
    }
}

fn verify_certificate_covers(certificate: &Path, required: &[String]) -> Result<(), String> {
    let cert = fs::read(certificate)
        .map_err(|error| format!("read {}: {error}", certificate.display()))?;
    let covered = cert_marker::subject_alt_names_from_pem(&cert)
        .ok_or_else(|| "generated certificate has no readable DNS/IP SANs".to_string())?;
    for required in required {
        if !covered
            .iter()
            .any(|covered| covered.eq_ignore_ascii_case(required))
        {
            return Err(format!(
                "generated certificate does not cover required SAN {required}"
            ));
        }
    }
    Ok(())
}

fn ensure_cert(options: &Options) -> Result<(), String> {
    let paths = TlsPaths::from_options(options);
    let _lock = if options.dry_run {
        None
    } else {
        Some(TlsDirectoryLock::acquire(&paths)?)
    };
    if !options.dry_run {
        recover_interrupted(&paths)?;
    }
    let request = if options.force {
        ProvisioningRequest::Rekey
    } else {
        ProvisioningRequest::Ensure
    };
    let existing_certificate_bytes = fs::read(&paths.certificate).ok();
    let state = inspect_tls(&paths);
    let decided = plan(request, state).map_err(|refusal| {
        let hint = match refusal {
            ProvisioningRefusal::ForeignMaterial => {
                " Operator-supplied TLS material is never overwritten by the installer."
            }
            _ => "",
        };
        format!("{}: {}{hint}", refusal.as_str(), refusal.guidance())
    })?;

    match decided.action {
        ProvisioningAction::KeepExisting => {
            let reason = match state.ownership {
                Some(MaterialOwnership::Ambiguous) => {
                    "ownership marker is invalid or does not match; treating material as operator-managed"
                }
                Some(MaterialOwnership::Foreign) => {
                    "operator-managed or not recognised as legacy Arcen material"
                }
                _ => "not due",
            };
            println!(
                "keeping existing TLS certificate and key in {} ({reason})",
                paths.directory().display()
            );
            if !options.dry_run {
                chmod(&paths.key, 0o600)?;
            }
        }
        ProvisioningAction::CreateNew => {
            println!(
                "{}generating TLS certificate and key in {}",
                if options.dry_run {
                    "dry-run: would "
                } else {
                    ""
                },
                paths.directory().display()
            );
            let san_entries = subject_alt_name_entries(&options.extra_sans);
            issue_certificate(options, &paths, false, &san_entries)?;
        }
        ProvisioningAction::RenewPreservingKey | ProvisioningAction::AdoptAndRenew => {
            println!(
                "{}reissuing TLS certificate in {} over the existing key; Deck SPKI pins remain valid",
                if options.dry_run {
                    "dry-run: would "
                } else {
                    ""
                },
                paths.directory().display()
            );
            let mut san_entries = subject_alt_name_entries(&options.extra_sans);
            if let Some(existing) = existing_certificate_bytes
                .as_deref()
                .and_then(cert_marker::subject_alt_names_from_pem)
            {
                for entry in existing {
                    merge_san_entry(&mut san_entries, entry);
                }
            }
            issue_certificate(options, &paths, true, &san_entries)?;
        }
        ProvisioningAction::ReplaceKeyAndCertificate => {
            if decided.invalidates_pins {
                println!(
                    "{}replacing the TLS key and certificate in {}. Every Deck that pinned the previous certificate must re-pin.",
                    if options.dry_run {
                        "dry-run: would "
                    } else {
                        "--force: "
                    },
                    paths.directory().display()
                );
            }
            let san_entries = subject_alt_name_entries(&options.extra_sans);
            issue_certificate(options, &paths, false, &san_entries)?;
        }
    }
    Ok(())
}

fn create_dir(options: &Options, path: &str, mode: u32) -> Result<(), String> {
    let target = map_path(&options.prefix, path);
    if options.dry_run {
        println!("dry-run: mkdir -p -m {mode:o} {}", target.display());
        return Ok(());
    }
    fs::create_dir_all(&target).map_err(|error| format!("create {}: {error}", target.display()))?;
    chmod(&target, mode)
}

fn write_atomic(
    options: &Options,
    path: &str,
    content: &[u8],
    mode: u32,
    overwrite: bool,
) -> Result<(), String> {
    let target = map_path(&options.prefix, path);
    if target.exists() && !overwrite {
        println!("kept existing {}", target.display());
        return Ok(());
    }
    if options.dry_run {
        println!("dry-run: write {} mode {mode:o}", target.display());
        return Ok(());
    }
    let parent = target
        .parent()
        .ok_or_else(|| format!("{} has no parent", target.display()))?;
    fs::create_dir_all(parent).map_err(|error| format!("create {}: {error}", parent.display()))?;
    let staged = target.with_file_name(format!(
        ".{}.installing.{}",
        target
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("file"),
        std::process::id()
    ));
    {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staged)
            .map_err(|error| format!("create {}: {error}", staged.display()))?;
        file.write_all(content)
            .map_err(|error| format!("write {}: {error}", staged.display()))?;
        file.sync_all()
            .map_err(|error| format!("sync {}: {error}", staged.display()))?;
    }
    chmod(&staged, mode)?;
    fs::rename(&staged, &target).map_err(|error| format!("install {}: {error}", target.display()))
}

fn remove_file(options: &Options, path: &str) -> Result<(), String> {
    let target = map_path(&options.prefix, path);
    if options.dry_run {
        println!("dry-run: remove {}", target.display());
        return Ok(());
    }
    match fs::remove_file(&target) {
        Ok(()) => println!("removed {}", target.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("remove {}: {error}", target.display())),
    }
    Ok(())
}

/// Remove a directory only when nothing is left in it.
///
/// Uninstall previously left empty `/usr/local/libexec/arcen` and
/// `/usr/share/doc/arcen` behind, so "uninstalled" did not mean the machine
/// looked untouched. Anything the operator put there is preserved, because a
/// non-empty directory is left alone.
fn remove_dir_if_empty(options: &Options, path: &str) {
    let target = map_path(&options.prefix, path);
    if options.dry_run {
        println!("dry-run: would remove {} if empty", target.display());
        return;
    }
    if fs::read_dir(&target).is_ok_and(|mut entries| entries.next().is_none()) {
        let _ = fs::remove_dir(&target);
    }
}

/// Remove the `PATH` symlink, but only when it still points at our binary.
///
/// Uninstall should leave the machine looking untouched without destroying
/// something it did not create: if an operator has since replaced the link with
/// their own file or repointed it elsewhere, that is theirs to keep.
fn remove_symlink_if_ours(options: &Options) {
    let link = map_path(&options.prefix, PIER_SYMLINK);
    let expected = map_path(&options.prefix, PIER_PATH);
    if options.dry_run {
        println!(
            "dry-run: remove {} if it points at {}",
            link.display(),
            expected.display()
        );
        return;
    }
    match fs::read_link(&link) {
        Ok(destination) if destination == expected => match fs::remove_file(&link) {
            Ok(()) => println!("removed {}", link.display()),
            Err(error) => eprintln!("warning: remove {}: {error}", link.display()),
        },
        Ok(destination) => println!(
            "kept {}: points at {}, not ours",
            link.display(),
            destination.display()
        ),
        Err(_) => {}
    }
}

/// Copy `pier.json` clear of the tree `--purge` is about to delete.
///
/// The configuration is the one thing on a Pier the installer cannot
/// reconstruct. GPU pinning, monitor layout and transport tuning are site
/// facts, not product defaults, so a purge-and-reinstall silently reverted a
/// hand-tuned host to whatever the product defaults happen to select.
///
/// The copy lands outside `/etc/arcen`, and purge still proceeds if it cannot
/// be made: refusing to clean a machine because a backup failed is worse than
/// the lost file.
fn preserve_config_before_purge(options: &Options) -> Result<(), String> {
    let source = map_path(&options.prefix, "/etc/arcen/pier.json");
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let backup = map_path(
        &options.prefix,
        &format!("/etc/arcen-pier.json.purged-{stamp}"),
    );
    if options.dry_run {
        if source.exists() {
            println!(
                "dry-run: preserve {} as {}",
                source.display(),
                backup.display()
            );
        }
        return Ok(());
    }
    if !source.exists() {
        return Ok(());
    }
    match fs::copy(&source, &backup) {
        Ok(_) => println!("preserved config before purge: {}", backup.display()),
        Err(error) => println!(
            "warning: could not preserve {} before purge: {error}",
            source.display()
        ),
    }
    Ok(())
}

fn remove_dir_all(options: &Options, path: &str) -> Result<(), String> {
    let target = map_path(&options.prefix, path);
    if options.dry_run {
        println!("dry-run: remove tree {}", target.display());
        return Ok(());
    }
    match fs::remove_dir_all(&target) {
        Ok(()) => println!("removed tree {}", target.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("remove tree {}: {error}", target.display())),
    }
    Ok(())
}

fn map_path(prefix: &Path, absolute: &str) -> PathBuf {
    let relative = absolute.trim_start_matches('/');
    if is_root_prefix(prefix) {
        PathBuf::from("/").join(relative)
    } else {
        prefix.join(relative)
    }
}

fn is_root_prefix(prefix: &Path) -> bool {
    prefix == Path::new("/")
}

fn chmod(path: &Path, mode: u32) -> Result<(), String> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(|error| format!("chmod {}: {error}", path.display()))
}

fn run_systemctl(args: &[&str]) -> Result<(), String> {
    let status = Command::new("systemctl")
        .args(args)
        .status()
        .map_err(|error| format!("start systemctl {}: {error}", args.join(" ")))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("systemctl {} failed", args.join(" ")))
    }
}

fn current_euid() -> Result<u32, String> {
    let output = Command::new("/usr/bin/id")
        .arg("-u")
        .output()
        .map_err(|error| format!("run /usr/bin/id -u: {error}"))?;
    if !output.status.success() {
        return Err("/usr/bin/id -u failed".to_string());
    }
    String::from_utf8(output.stdout)
        .map_err(|_| "/usr/bin/id output was not UTF-8".to_string())?
        .trim()
        .parse::<u32>()
        .map_err(|error| format!("parse effective uid: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::{SystemTime, UNIX_EPOCH};

    fn staged() -> arcen_session::install_lifecycle::InstallTransaction {
        use arcen_session::install_lifecycle::{InstallEvent, InstallTransaction};
        let mut transaction = InstallTransaction::new();
        transaction
            .apply(InstallEvent::PreflightPassed)
            .expect("preflight");
        transaction
            .apply(InstallEvent::PayloadStaged)
            .expect("staged");
        transaction
    }

    fn test_prefix(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join(".arcen-linux-installer-tests")
            .join(format!("{name}-{}-{unique}", std::process::id()))
    }

    fn run_openssl(args: &[&str]) {
        let output = Command::new("openssl")
            .args(args)
            .output()
            .expect("start openssl");
        assert!(
            output.status.success(),
            "openssl {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn generate_self_signed_pair(directory: &Path) {
        let key = directory.join("host.key");
        let cert = directory.join("host.crt");
        run_openssl(&[
            "ecparam",
            "-name",
            "prime256v1",
            "-genkey",
            "-noout",
            "-out",
            key.to_str().expect("key path"),
        ]);
        run_openssl(&[
            "req",
            "-x509",
            "-new",
            "-sha256",
            "-days",
            "825",
            "-key",
            key.to_str().expect("key path"),
            "-out",
            cert.to_str().expect("cert path"),
            "-subj",
            "/CN=Arcen Pier",
            "-addext",
            "subjectAltName=DNS:pier.example.internal",
            "-addext",
            "basicConstraints=critical,CA:FALSE",
            "-addext",
            "keyUsage=critical,digitalSignature",
            "-addext",
            "extendedKeyUsage=serverAuth",
        ]);
    }

    fn generate_ca_issued_pair(directory: &Path) {
        let ca_key = directory.join("ca.key");
        let ca_cert = directory.join("ca.crt");
        let host_key = directory.join("host.key");
        let csr = directory.join("host.csr");
        let host_cert = directory.join("host.crt");
        run_openssl(&[
            "genpkey",
            "-algorithm",
            "EC",
            "-pkeyopt",
            "ec_paramgen_curve:P-256",
            "-out",
            ca_key.to_str().expect("ca key path"),
        ]);
        run_openssl(&[
            "req",
            "-x509",
            "-new",
            "-sha256",
            "-days",
            "825",
            "-key",
            ca_key.to_str().expect("ca key path"),
            "-out",
            ca_cert.to_str().expect("ca cert path"),
            "-subj",
            "/CN=Example Enterprise CA",
            "-addext",
            "basicConstraints=critical,CA:TRUE",
            "-addext",
            "keyUsage=critical,keyCertSign",
        ]);
        run_openssl(&[
            "ecparam",
            "-name",
            "prime256v1",
            "-genkey",
            "-noout",
            "-out",
            host_key.to_str().expect("host key path"),
        ]);
        run_openssl(&[
            "req",
            "-new",
            "-key",
            host_key.to_str().expect("host key path"),
            "-out",
            csr.to_str().expect("csr path"),
            "-subj",
            "/CN=Arcen Pier",
            "-addext",
            "subjectAltName=DNS:pier.example.internal",
            "-addext",
            "basicConstraints=critical,CA:FALSE",
            "-addext",
            "keyUsage=critical,digitalSignature",
            "-addext",
            "extendedKeyUsage=serverAuth",
        ]);
        run_openssl(&[
            "x509",
            "-req",
            "-in",
            csr.to_str().expect("csr path"),
            "-CA",
            ca_cert.to_str().expect("ca cert path"),
            "-CAkey",
            ca_key.to_str().expect("ca key path"),
            "-CAcreateserial",
            "-days",
            "825",
            "-sha256",
            "-copy_extensions",
            "copy",
            "-out",
            host_cert.to_str().expect("host cert path"),
        ]);
    }

    #[test]
    fn the_install_succeeds_only_when_the_service_is_proven_running() {
        assert_eq!(finish_install(staged(), &ServiceOutcome::Running), Ok(()));
        assert_eq!(
            finish_install(staged(), &ServiceOutcome::NotRequested),
            Ok(()),
            "a dry run, staging prefix or --no-service claims no running service"
        );
        let error = finish_install(staged(), &ServiceOutcome::NotRunning("failed".into()))
            .expect_err("a service that is not running fails the install");
        assert!(error.contains("\"failed\""), "{error}");
        assert!(error.contains("journalctl -u arcen-pier"), "{error}");
    }

    #[test]
    fn purge_preserves_a_copy_of_the_configuration() {
        let prefix = test_prefix("purge");
        let config = prefix.join("etc/arcen/pier.json");
        fs::create_dir_all(config.parent().expect("config parent")).expect("create config parent");
        let tuned = br#"{"platform":{"desktop":{"adapter":"reserved-gpu"}}}"#;
        fs::write(&config, tuned).expect("write tuned config");
        let options = Options {
            prefix: prefix.clone(),
            dry_run: false,
            uninstall: true,
            purge: true,
            force: false,
            no_service: true,
            restart: false,
            extra_sans: Vec::new(),
        };

        preserve_config_before_purge(&options).expect("preserve config");
        remove_dir_all(&options, "/etc/arcen").expect("purge config tree");

        assert!(!config.exists(), "purge must still remove the config tree");
        let preserved: Vec<_> = fs::read_dir(prefix.join("etc"))
            .expect("read etc")
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
        let _ = fs::remove_dir_all(&prefix);
    }

    #[test]
    fn purge_without_a_configuration_is_not_an_error() {
        let prefix = test_prefix("purge-empty");
        fs::create_dir_all(prefix.join("etc/arcen")).expect("create config dir");
        let options = Options {
            prefix: prefix.clone(),
            dry_run: false,
            uninstall: true,
            purge: true,
            force: false,
            no_service: true,
            restart: false,
            extra_sans: Vec::new(),
        };

        preserve_config_before_purge(&options).expect("absent config must not fail the purge");

        let _ = fs::remove_dir_all(&prefix);
    }

    #[test]
    fn existing_config_migration_preserves_rollback_copy() {
        let prefix = test_prefix("migration");
        let config = prefix.join("etc/arcen/pier.json");
        fs::create_dir_all(config.parent().expect("config parent")).expect("create config parent");
        let original = br#"{
            "listen":{"port":18443,"quic_port":18444},
            "tls":{
                "minimum_version":"TLS1.2",
                "disabled_cipher_suites":["TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256"]
            },
            "future":{"keep":true}
        }"#;
        fs::write(&config, original).expect("write original config");
        let options = Options {
            prefix: prefix.clone(),
            dry_run: false,
            uninstall: false,
            purge: false,
            force: false,
            no_service: true,
            restart: false,
            extra_sans: Vec::new(),
        };

        migrate_existing_config(&options).expect("migrate config");

        assert_eq!(
            fs::read(prefix.join("etc/arcen/pier.json.pre-quic")).expect("read rollback"),
            original
        );
        let migrated: serde_json::Value =
            serde_json::from_slice(&fs::read(&config).expect("read migrated config"))
                .expect("parse migrated config");
        assert_eq!(migrated["listen"]["port"], 18_444);
        assert!(migrated["listen"].get("quic_port").is_none());
        assert_eq!(migrated["tls"]["minimum_version"], "TLS1.3");
        assert_eq!(migrated["future"]["keep"], true);

        fs::remove_dir_all(prefix).expect("remove test directory");
    }

    #[test]
    fn unmarked_self_signed_pairs_follow_the_shared_adoption_plan() {
        let prefix = test_prefix("legacy-self-signed");
        let directory = prefix.join("etc/arcen");
        fs::create_dir_all(&directory).expect("create tls directory");
        generate_self_signed_pair(&directory);
        let options = Options {
            prefix: prefix.clone(),
            dry_run: false,
            uninstall: false,
            purge: false,
            force: false,
            no_service: true,
            restart: false,
            extra_sans: Vec::new(),
        };
        let state = inspect_tls(&TlsPaths::from_options(&options));

        assert_eq!(state.ownership, Some(MaterialOwnership::Foreign));
        assert!(
            state.legacy_arcen_self_signed,
            "legacy Arcen output is recognised positively"
        );
        assert_eq!(
            plan(ProvisioningRequest::Ensure, state).map(|plan| plan.action),
            Ok(ProvisioningAction::AdoptAndRenew),
            "an ordinary upgrade should renew over the existing key and write the marker"
        );

        fs::remove_dir_all(prefix).expect("remove test directory");
    }

    #[test]
    fn ca_issued_operator_pairs_are_kept_by_the_shared_plan() {
        let prefix = test_prefix("operator-ca");
        let directory = prefix.join("etc/arcen");
        fs::create_dir_all(&directory).expect("create tls directory");
        generate_ca_issued_pair(&directory);
        let options = Options {
            prefix: prefix.clone(),
            dry_run: false,
            uninstall: false,
            purge: false,
            force: false,
            no_service: true,
            restart: false,
            extra_sans: Vec::new(),
        };
        let state = inspect_tls(&TlsPaths::from_options(&options));

        assert_eq!(state.ownership, Some(MaterialOwnership::Foreign));
        assert!(
            !state.legacy_arcen_self_signed,
            "operator CA-issued material must not be classified as legacy Arcen output"
        );
        assert_eq!(
            plan(ProvisioningRequest::Ensure, state).map(|plan| plan.action),
            Ok(ProvisioningAction::KeepExisting)
        );

        fs::remove_dir_all(prefix).expect("remove test directory");
    }

    #[test]
    fn same_key_adoption_preserves_existing_sans_and_merges_new_ones() {
        let prefix = test_prefix("preserve-sans");
        let directory = prefix.join("etc/arcen");
        fs::create_dir_all(&directory).expect("create tls directory");
        generate_self_signed_pair(&directory);
        let before_key = fs::read(directory.join("host.key")).expect("read key");
        let options = Options {
            prefix: prefix.clone(),
            dry_run: false,
            uninstall: false,
            purge: false,
            force: false,
            no_service: true,
            restart: false,
            extra_sans: vec!["alias.example.internal".to_string()],
        };

        ensure_cert(&options).expect("adopt and renew");

        let cert = fs::read(directory.join("host.crt")).expect("read cert");
        let names = cert_marker::subject_alt_names_from_pem(&cert).expect("SANs");
        assert!(
            names.contains(&"DNS:pier.example.internal".to_string()),
            "existing SAN must survive same-key renewal: {names:?}"
        );
        assert!(
            names.contains(&"DNS:alias.example.internal".to_string()),
            "new operator SAN must be merged: {names:?}"
        );
        assert_eq!(
            fs::read(directory.join("host.key")).expect("read key"),
            before_key,
            "adoption must preserve the key"
        );

        fs::remove_dir_all(prefix).expect("remove test directory");
    }

    #[test]
    fn installer_refuses_while_helper_lock_is_held() {
        let prefix = test_prefix("lock-held");
        let directory = prefix.join("etc/arcen");
        fs::create_dir_all(&directory).expect("create tls directory");
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(directory.join(LOCK_FILE))
            .expect("open lock");
        rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive)
            .expect("hold lock");
        let options = Options {
            prefix: prefix.clone(),
            dry_run: false,
            uninstall: false,
            purge: false,
            force: false,
            no_service: true,
            restart: false,
            extra_sans: Vec::new(),
        };

        let error = ensure_cert(&options).expect_err("lock should be respected");

        assert!(
            error.contains("another host-certificate transaction is active"),
            "{error}"
        );
        let _ = rustix::fs::flock(&lock, rustix::fs::FlockOperation::Unlock);
        fs::remove_dir_all(prefix).expect("remove test directory");
    }

    #[test]
    fn backup_preparation_failures_restore_only_files_that_moved() {
        for op in [
            TestBackupOp::Rename,
            TestBackupOp::FileFsync,
            TestBackupOp::DirFsync,
        ] {
            for index in 0..MANAGED_FILES.len() {
                let prefix = test_prefix(&format!("backup-failure-{op:?}-{index}"));
                let directory = prefix.join("etc/arcen");
                fs::create_dir_all(&directory).expect("create tls directory");
                let options = Options {
                    prefix: prefix.clone(),
                    dry_run: false,
                    uninstall: false,
                    purge: false,
                    force: false,
                    no_service: true,
                    restart: false,
                    extra_sans: Vec::new(),
                };
                let paths = TlsPaths::from_options(&options);
                for (file_index, path) in paths.managed().into_iter().enumerate() {
                    fs::write(path, format!("original-{file_index}")).expect("write original");
                }

                set_test_backup_failure(Some((op, index)));
                let error = CertTransaction::begin(&paths).expect_err("injected failure");

                assert!(
                    error.contains("injected") || error.contains("back up"),
                    "{error}"
                );
                for (file_index, path) in paths.managed().into_iter().enumerate() {
                    assert_eq!(
                        fs::read_to_string(path).expect("read restored"),
                        format!("original-{file_index}"),
                        "{} should retain its original bytes",
                        path.display()
                    );
                    let backups: Vec<_> = fs::read_dir(&directory)
                        .expect("read tls directory")
                        .filter_map(Result::ok)
                        .filter(|entry| {
                            entry
                                .file_name()
                                .to_string_lossy()
                                .contains(&format!(".{}", MANAGED_FILES[file_index]))
                        })
                        .collect();
                    assert!(backups.is_empty(), "backup was left for {}", path.display());
                }
                assert!(
                    !directory.join(JOURNAL_FILE).exists(),
                    "complete rollback should clear the journal"
                );
                set_test_backup_failure(None);
                fs::remove_dir_all(prefix).expect("remove test directory");
            }
        }
    }

    #[test]
    fn dry_run_does_not_create_the_tls_lock_or_directory() {
        let prefix = test_prefix("dry-run-no-lock");
        let options = Options {
            prefix: prefix.clone(),
            dry_run: true,
            uninstall: false,
            purge: false,
            force: false,
            no_service: true,
            restart: false,
            extra_sans: Vec::new(),
        };

        ensure_cert(&options).expect("dry-run should not need a TLS directory");

        assert!(
            !prefix.join("etc/arcen").exists(),
            "dry-run must not create the TLS directory"
        );
        fs::remove_dir_all(prefix).ok();
    }

    #[test]
    fn installer_markers_bind_to_the_current_certificate_and_spki() {
        let prefix = test_prefix("marker");
        let directory = prefix.join("etc/arcen");
        fs::create_dir_all(&directory).expect("create tls directory");
        generate_self_signed_pair(&directory);
        let options = Options {
            prefix: prefix.clone(),
            dry_run: false,
            uninstall: false,
            purge: false,
            force: false,
            no_service: true,
            restart: false,
            extra_sans: Vec::new(),
        };
        let paths = TlsPaths::from_options(&options);

        write_pins_and_marker(&paths).expect("write marker");
        let marker = fs::read_to_string(directory.join(MARKER_FILE)).expect("read marker");

        assert!(
            marker.contains("version=3\ncertificate="),
            "marker should use the shared rendered format: {marker}"
        );
        assert!(
            marker_matches(
                &paths,
                &fs::read(directory.join("host.crt")).expect("read cert")
            ),
            "marker must match the active certificate pins"
        );

        fs::remove_dir_all(prefix).expect("remove test directory");
    }

    /// An operator-supplied address must be encoded as an IP SAN, not a DNS one.
    ///
    /// openssl accepts `DNS:203.0.113.133` without complaint and produces a
    /// certificate that then fails to match when a Deck dials that address,
    /// which is the exact failure `--extra-san` exists to fix. Worth pinning,
    /// because nothing else would catch it until a remote user could not
    /// connect.
    #[test]
    fn extra_sans_are_classified_as_addresses_or_names() {
        let rendered =
            subject_alt_name(&["203.0.113.133".to_string(), "arcen.example.com".to_string()]);
        assert!(
            rendered.contains("IP:203.0.113.133"),
            "address must be an IP SAN: {rendered}"
        );
        assert!(
            !rendered.contains("DNS:203.0.113.133"),
            "address must not also appear as a DNS SAN: {rendered}"
        );
        assert!(
            rendered.contains("DNS:arcen.example.com"),
            "name must be a DNS SAN: {rendered}"
        );
    }

    /// A duplicate of something already discovered must not be emitted twice.
    #[test]
    fn extra_sans_do_not_duplicate_discovered_entries() {
        let rendered = subject_alt_name(&["localhost".to_string(), "127.0.0.1".to_string()]);
        assert_eq!(rendered.matches("DNS:localhost").count(), 1, "{rendered}");
        assert_eq!(rendered.matches("IP:127.0.0.1").count(), 1, "{rendered}");
    }
}
