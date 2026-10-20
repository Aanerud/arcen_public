use std::env;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use arcen_session::install_provider::{InstallPlan, plan_install_provider, summary_lines};
use arcen_session::wayland_qualification::{
    QUALIFY_TEMPORARY_USER, QUALIFY_TEMPORARY_USER_GECOS, QualificationTarget, RECORD_PATH,
};
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

/// Desktop-provider facts, the embedded Pier's runtime check, the Wayland
/// qualification run and the install log.
mod desktop;
/// The display manager beside native Wayland desktops: explained, and
/// disabled only on request (`--disable-gdm`, undone by `--restore-gdm`).
mod display_manager;
/// The Pier's sysfs PCI reader, compiled here too so the installer and the
/// Pier reach the same answer about which GPU Xorg must use. It depends only
/// on `std` and `arcen-outputs`, which owns the decision itself.
#[path = "../../../../hosts/linux/src/display/gpu_pci.rs"]
mod gpu_pci;
/// The launcher's rule for trusting the Xorg template, so the installer never
/// repairs (and so launders) a file the Pier itself would refuse.
#[path = "../../../../hosts/linux/src/display/xorg_template_trust.rs"]
mod xorg_template_trust;

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
const XORG_CONFIG_PATH: &str = "/etc/arcen/xorg.conf";
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
/// Arcen's SELinux policy module: one rule that lets the Pier's session
/// launcher (a systemd service, `unconfined_service_t`) start the session
/// user's first process in the context `pam_selinux` assigned
/// (`docs/security/linux-selinux.md`). Kept on disk so an operator who
/// enables SELinux later can load it without the installer.
const SELINUX_MODULE: &str = include_str!("../../selinux/arcen.cil");
const SELINUX_MODULE_NAME: &str = "arcen";
const SELINUX_DIR: &str = "/usr/share/arcen/selinux";
const SELINUX_MODULE_PATH: &str = "/usr/share/arcen/selinux/arcen.cil";
const SEMODULE: &str = "/usr/sbin/semodule";
/// Present exactly when SELinux is enabled (enforcing or permissive).
const SELINUXFS_ENFORCE: &str = "/sys/fs/selinux/enforce";

/// Runtime commands the Pier invokes. Checked before anything is written, so a
/// host missing a dependency is told up front rather than after a half
/// install that leaves a service which cannot start.
const REQUIRED_COMMANDS: &[(&str, &str)] = &[
    (
        "/usr/bin/openssl",
        "openssl, used to generate the TLS host certificate",
    ),
    (
        "/usr/bin/systemctl",
        "systemd, used to register and run the service",
    ),
];

/// Required only where the Pier serves the dedicated X11 desktop.
const X11_REQUIRED_COMMANDS: &[(&str, &str)] = &[(
    "/usr/bin/xauth",
    "xauth, used to authorise the dedicated X server",
)];

/// Required only where the Pier serves a headless Wayland desktop: the
/// qualification run creates, uses and removes a temporary local account
/// and runs outside the operator's login session. The Wayland session stack
/// itself (compositor, D-Bus, PipeWire, WirePlumber, DRM) is part of the
/// provider plan.
const WAYLAND_REQUIRED_COMMANDS: &[(&str, &str)] = &[
    (
        "/usr/sbin/useradd",
        "useradd, for the temporary qualification account",
    ),
    ("/usr/sbin/userdel", "userdel, to remove that account"),
    (
        "/usr/sbin/chpasswd",
        "chpasswd, to give it a one-time password",
    ),
    ("/usr/bin/loginctl", "loginctl, to end its logind session"),
    (
        "/usr/bin/systemd-run",
        "systemd-run, to qualify outside the installing login session",
    ),
];

/// Optional at install time, but the Pier cannot serve a session without them.
/// Reported as warnings so an operator can fix the host afterwards rather than
/// being blocked.
const RECOMMENDED_COMMANDS: &[(&str, &str)] = &[(
    "/usr/bin/pactl",
    "PulseAudio client tools, required for audio capture",
)];

/// Records that this installer opened UDP 18444 (`firewalld` or `ufw`), so
/// uninstall closes exactly what install opened and nothing an
/// administrator opened.
const FIREWALL_MARKER: &str = "/var/lib/arcen/installer-opened-firewall";

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
    /// Print what this host has and which desktop the installer would
    /// choose, then exit without writing anything.
    check: bool,
    /// Display sizes to qualify Speed for on a Wayland host
    /// (`WIDTHxHEIGHT@HZ`, passed to `wayland qualify --output`).
    qualify_outputs: Vec<String>,
    /// Disable the display manager (recorded; undone by `--restore-gdm` and
    /// `--uninstall`) so it can never put a login screen on a native
    /// desktop's seat.
    disable_gdm: bool,
    /// Put back what `--disable-gdm` changed, then exit.
    restore_gdm: bool,
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
    if options.check {
        return check(&options);
    }
    if current_euid()? != 0 {
        return Err("must run as root".to_string());
    }
    if options.restore_gdm && !options.uninstall {
        let mut log = desktop::InstallLog::default();
        return display_manager::restore(
            options.dry_run,
            display_manager::RestoreMode::Standalone,
            &mut log,
        );
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
        check: false,
        qualify_outputs: Vec::new(),
        disable_gdm: false,
        restore_gdm: false,
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
            "--check" => options.check = true,
            "--disable-gdm" => options.disable_gdm = true,
            "--restore-gdm" => options.restore_gdm = true,
            "--qualify-output" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--qualify-output requires WIDTHxHEIGHT@HZ".to_string())?;
                if !desktop::valid_qualify_output(&value) {
                    return Err(format!("--qualify-output {value:?} is not WIDTHxHEIGHT@HZ"));
                }
                if !options.qualify_outputs.contains(&value) {
                    options.qualify_outputs.push(value);
                }
            }
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
                     \x20                        [--extra-san NAME-OR-IP] [--check]\n\
                     \x20                        [--qualify-output WIDTHxHEIGHT@HZ]\n\
                     \x20                        [--disable-gdm | --restore-gdm]\n\
                     \n\
                     The installer chooses the desktop from what this host has: an\n\
                     installed Xorg server keeps the dedicated X11 desktop; without one,\n\
                     a headless GNOME stack with a DRM render node is qualified for\n\
                     Wayland (`arcen-pier wayland qualify --write`) and the service\n\
                     starts only if that passes. It prints what it found and why.\n\
                     \n\
                     --check      Print what this host has and which desktop would be\n\
                     \x20            chosen, then exit; writes nothing and needs no root.\n\
                     \x20            Exit status 1 when the installer would stop.\n\
                     --qualify-output  An extra Deck display size to qualify Speed for on\n\
                     \x20            a Wayland host (repeatable). Rarely needed: every\n\
                     \x20            qualification measures the previous record's displays,\n\
                     \x20            those Decks asked Speed for, and 1920x1080, 2560x1600\n\
                     \x20            and 3840x2160 at 60 Hz; smaller displays are covered.\n\
                     \n\
                     --disable-gdm  Disable and stop the display manager before qualifying,\n\
                     \x20            so it can never put a login screen on a native Wayland\n\
                     \x20            desktop's seat (wayland.session_mode). Its state is recorded\n\
                     \x20            in /var/lib/arcen/display-manager-disabled.json.\n\
                     --restore-gdm  Put the display manager back as it was, then exit.\n\
                     \x20            --uninstall does this too.\n\
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

/// Commands this host lacks for `plan`: `(required, recommended)`, each as
/// `path  (why)`. Without a plan (a staging prefix) only the common ones.
fn missing_commands(plan: Option<InstallPlan>) -> (Vec<String>, Vec<String>) {
    let provider: &[(&str, &str)] = match plan {
        Some(InstallPlan::Wayland { .. }) => WAYLAND_REQUIRED_COMMANDS,
        Some(InstallPlan::Xorg { .. }) => X11_REQUIRED_COMMANDS,
        None => &[],
    };
    let absent = |commands: &[(&str, &str)]| {
        commands
            .iter()
            .filter(|(path, _)| !Path::new(path).exists())
            .map(|(path, why)| format!("{path}  ({why})"))
            .collect::<Vec<_>>()
    };
    let selinux: &[(&str, &str)] = if Path::new(SELINUXFS_ENFORCE).exists() {
        &[(
            SEMODULE,
            "semodule (policycoreutils): SELinux is enabled, and without Arcen's policy \
             module every session would be refused",
        )]
    } else {
        &[]
    };
    let required = REQUIRED_COMMANDS
        .iter()
        .chain(provider)
        .chain(selinux)
        .copied()
        .collect::<Vec<_>>();
    (absent(&required), absent(RECOMMENDED_COMMANDS))
}

fn preflight(options: &Options, plan: Option<InstallPlan>) -> Result<(), String> {
    if !is_root_prefix(&options.prefix) {
        return Ok(());
    }
    let (missing, recommended) = missing_commands(plan);
    if !missing.is_empty() {
        return Err(format!(
            "this host is missing required commands:\n  {}\n\nInstall them and run the installer again.",
            missing.join("\n  ")
        ));
    }
    for absent in recommended {
        println!("warning: {absent} is absent; sessions will fail until it is installed");
    }
    Ok(())
}

/// Decides the desktop from this machine's facts before anything is
/// written, printing and logging what was found and why. `None` for a
/// staging prefix, which describes another machine.
///
/// # Errors
///
/// The embedded Pier cannot start here, or the host can serve no desktop:
/// the installer stops with the explanation.
fn detect_desktop(
    options: &Options,
    log: &mut desktop::InstallLog,
) -> Result<Option<InstallPlan>, String> {
    if !is_root_prefix(&options.prefix) {
        println!("staging prefix: skipped desktop provider detection");
        return Ok(None);
    }
    let runtime = desktop::check_pier_runtime(PIER);
    let wayland_built = match &runtime {
        desktop::PierRuntime::Runs { wayland_built } => *wayland_built,
        // Release builds contain the provider; the qualification run proves
        // it either way.
        desktop::PierRuntime::LoaderRefused(_) | desktop::PierRuntime::Unchecked(_) => true,
    };
    let existing = existing_config(options, log);
    // The Pier refuses to start with a prefix it may not execute from; the
    // installer reports it and runs nothing from it.
    if let Some(refusal) = desktop::compositor_prefix_refusal(&existing) {
        log.note(format!("  {:<18}REFUSED: {refusal}", "Compositor prefix:"));
        log.record("decision: refused (compositor_prefix)");
        return Err(format!(
            "{refusal}\nFix the prefix's ownership (root-owned, writable by no one else) or \
             remove capture.wayland_compositor_prefix, then run the installer again."
        ));
    }
    let facts = desktop::gather_host_facts(&existing, wayland_built);
    let decision = plan_install_provider(&facts);
    let mut lines = summary_lines(&facts, &decision);
    let decision_line = lines.pop();
    lines.push(format!(
        "  {:<18}{}",
        "SELinux:",
        desktop::selinux_summary()
    ));
    lines.push(format!(
        "  {:<18}{}",
        "Pier runtime:",
        match &runtime {
            desktop::PierRuntime::Runs {
                wayland_built: true,
            } => "starts on this host; X11 and Wayland providers built in".to_owned(),
            desktop::PierRuntime::Runs {
                wayland_built: false,
            } => "starts on this host; X11 provider only".to_owned(),
            desktop::PierRuntime::Unchecked(reason) => {
                format!("not test-run before installing ({reason})")
            }
            desktop::PierRuntime::LoaderRefused(missing) => {
                format!(
                    "CANNOT START on this host: {} not found",
                    missing.join(", ")
                )
            }
        }
    ));
    lines.extend(decision_line);
    for line in lines {
        log.note(line);
    }
    if let desktop::PierRuntime::LoaderRefused(missing) = &runtime {
        log.record("decision: refused (pier_runtime)");
        return Err(desktop::loader_refusal_message(missing));
    }
    log.record(format!(
        "decision: {}",
        match &decision {
            Ok(plan) => format!("{} ({})", plan.provider_token(), plan.reason().token()),
            Err(refusal) => format!("refused ({})", refusal.token()),
        }
    ));
    decision.map(Some).map_err(|refusal| refusal.to_string())
}

/// `capture.provider` and `xorg_bin` the installed Pier will run with: the
/// existing configuration, which an upgrade keeps, unless `--force` is about
/// to replace it with the packaged default.
fn existing_config(options: &Options, log: &mut desktop::InstallLog) -> desktop::ExistingConfig {
    let path = map_path(&options.prefix, "/etc/arcen/pier.json");
    if options.force {
        return desktop::ExistingConfig::default();
    }
    match fs::read_to_string(&path) {
        Ok(text) => desktop::parse_existing_config(&text).unwrap_or_else(|error| {
            log.note(format!(
                "warning: {}: {error}; deciding as for capture.provider = \"auto\"",
                path.display()
            ));
            desktop::ExistingConfig::default()
        }),
        Err(_) => desktop::ExistingConfig::default(),
    }
}

/// `--check`: what this host has and what the installer would do. Writes
/// nothing, starts nothing, needs no root.
fn check(options: &Options) -> Result<(), String> {
    if !is_root_prefix(&options.prefix) {
        return Err("--check describes this machine and takes no --prefix".to_string());
    }
    let mut log = desktop::InstallLog::default();
    let plan = detect_desktop(options, &mut log)?;
    let (missing, recommended) = missing_commands(plan);
    for absent in &missing {
        println!("missing (required): {absent}");
    }
    for absent in &recommended {
        println!("missing (recommended): {absent}");
    }
    println!();
    match plan {
        Some(InstallPlan::Xorg { .. }) => println!(
            "The installer would install the Pier for the dedicated X11 desktop: write \
             {XORG_CONFIG_PATH} if absent and name this machine's NVIDIA GPU in it, then enable \
             and start arcen-pier.service."
        ),
        Some(InstallPlan::Wayland { .. }) => println!(
            "The installer would install the Pier, run `{PIER_PATH} wayland qualify --write` (a \
             short real session \
             as the temporary local account {QUALIFY_TEMPORARY_USER}), and enable and start \
             arcen-pier.service only if that passes."
        ),
        None => {}
    }
    if missing.is_empty() {
        println!("Nothing was written.");
        Ok(())
    } else {
        Err(
            "the installer would stop: required commands are missing (nothing was written)"
                .to_string(),
        )
    }
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
        let already_open = firewalld_port_open();
        let runtime_was_open = firewalld_runtime_port_open();
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
            if !already_open {
                record_firewall_owner(&FirewallMarker {
                    owner: FirewallOwner::Firewalld,
                    runtime_was_open,
                });
            }
            println!("firewalld: opened 18444/udp and removed legacy 18443/tcp");
            return;
        }
        println!(
            "warning: firewalld QUIC update failed; open 18444/udp and remove legacy 18443/tcp manually"
        );
        return;
    }
    if Path::new("/usr/sbin/ufw").exists() {
        let already_open = ufw_rule_present();
        let quic = Command::new("/usr/sbin/ufw")
            .args(["allow", "18444/udp"])
            .status();
        let _remove_legacy = Command::new("/usr/sbin/ufw")
            .args(["delete", "allow", "18443/tcp"])
            .status();
        if matches!(quic, Ok(status) if status.success()) {
            if !already_open {
                record_firewall_owner(&FirewallMarker {
                    owner: FirewallOwner::Ufw,
                    runtime_was_open: false,
                });
            }
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

fn firewalld_running() -> bool {
    Command::new("/usr/bin/firewall-cmd")
        .arg("--state")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Whether the permanent firewalld configuration already allows 18444/udp.
fn firewalld_port_open() -> bool {
    Command::new("/usr/bin/firewall-cmd")
        .args(["--permanent", "--query-port=18444/udp"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Whether the running firewalld already allows 18444/udp (perhaps only
/// until its next reload).
fn firewalld_runtime_port_open() -> bool {
    Command::new("/usr/bin/firewall-cmd")
        .arg("--query-port=18444/udp")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Which firewall the installer opened 18444/udp in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FirewallOwner {
    Firewalld,
    Ufw,
}

/// What [`FIREWALL_MARKER`] records: where the installer opened the port,
/// and whether the running firewalld already allowed it (someone else's
/// runtime rule, which uninstall must leave in place).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FirewallMarker {
    owner: FirewallOwner,
    runtime_was_open: bool,
}

impl FirewallMarker {
    const RUNTIME_WAS_OPEN: &'static str = "runtime-was-open";

    fn render(self) -> String {
        let owner = match self.owner {
            FirewallOwner::Firewalld => "firewalld",
            FirewallOwner::Ufw => "ufw",
        };
        if self.runtime_was_open {
            format!("{owner}\n{}\n", Self::RUNTIME_WAS_OPEN)
        } else {
            format!("{owner}\n")
        }
    }

    fn parse(text: &str) -> Option<Self> {
        let mut lines = text.lines().map(str::trim).filter(|line| !line.is_empty());
        let owner = match lines.next()? {
            "firewalld" => FirewallOwner::Firewalld,
            "ufw" => FirewallOwner::Ufw,
            _ => return None,
        };
        Some(Self {
            owner,
            runtime_was_open: lines.any(|line| line == Self::RUNTIME_WAS_OPEN),
        })
    }
}

/// Whether ufw already has the rule.
fn ufw_rule_present() -> bool {
    Command::new("/usr/sbin/ufw")
        .args(["show", "added"])
        .output()
        .is_ok_and(|output| ufw_lists_quic_rule(&String::from_utf8_lossy(&output.stdout)))
}

/// Whether `ufw show added` lists the Pier's rule.
fn ufw_lists_quic_rule(listing: &str) -> bool {
    listing
        .lines()
        .any(|line| line.trim() == "ufw allow 18444/udp")
}

/// Remember that this installer, not an administrator, opened 18444/udp.
fn record_firewall_owner(record: &FirewallMarker) {
    let marker = Path::new(FIREWALL_MARKER);
    let written = marker
        .parent()
        .map_or(Ok(()), fs::create_dir_all)
        .and_then(|()| fs::write(marker, record.render()));
    if let Err(error) = written {
        println!(
            "warning: could not record that the installer opened 18444/udp ({error}); \
             uninstall will leave the port open"
        );
    }
}

/// Close 18444/udp on uninstall, exactly when this installer opened it.
fn close_firewall(options: &Options) {
    let marker = map_path(&options.prefix, FIREWALL_MARKER);
    if options.dry_run {
        println!(
            "dry-run: would close 18444/udp if {} says the installer opened it",
            marker.display()
        );
        return;
    }
    if !is_root_prefix(&options.prefix) {
        let _ = fs::remove_file(&marker);
        return;
    }
    let text = match fs::read_to_string(&marker) {
        Ok(text) => text,
        Err(_) => {
            if Path::new("/usr/bin/firewall-cmd").exists() && firewall_port_left_open() {
                println!(
                    "firewalld: 18444/udp stays open: this installer did not record opening it \
                     (an earlier installer or an administrator did). Close it if nothing else \
                     uses it: firewall-cmd --permanent --remove-port=18444/udp && \
                     firewall-cmd --reload"
                );
            }
            return;
        }
    };
    match FirewallMarker::parse(&text) {
        // No reload: it would also drop other runtime-only rules.
        Some(FirewallMarker {
            owner: FirewallOwner::Firewalld,
            runtime_was_open,
        }) => {
            let firewall_cmd = |args: &[&str]| {
                Command::new("/usr/bin/firewall-cmd")
                    .args(args)
                    .stdout(std::process::Stdio::null())
                    .status()
                    .is_ok_and(|status| status.success())
            };
            let closed = if firewalld_running() {
                firewall_cmd(&["--permanent", "--remove-port=18444/udp"])
                    && (runtime_was_open || firewall_cmd(&["--remove-port=18444/udp"]))
            } else {
                Command::new("/usr/bin/firewall-offline-cmd")
                    .arg("--remove-port=18444/udp")
                    .stdout(std::process::Stdio::null())
                    .status()
                    .is_ok_and(|status| status.success())
            };
            if closed {
                println!(
                    "firewalld: closed 18444/udp, which the installer had opened{}",
                    if runtime_was_open {
                        " (the runtime rule that was there before is kept)"
                    } else {
                        ""
                    }
                );
            } else {
                println!(
                    "warning: firewalld: could not close 18444/udp; run \
                     `firewall-cmd --permanent --remove-port=18444/udp && firewall-cmd --reload`"
                );
            }
        }
        Some(FirewallMarker {
            owner: FirewallOwner::Ufw,
            ..
        }) => {
            if Command::new("/usr/sbin/ufw")
                .args(["delete", "allow", "18444/udp"])
                .status()
                .is_ok_and(|status| status.success())
            {
                println!("ufw: closed 18444/udp, which the installer had opened");
            } else {
                println!(
                    "warning: ufw: could not close 18444/udp; run `ufw delete allow 18444/udp`"
                );
            }
        }
        None => println!(
            "warning: {} names no known firewall; 18444/udp left as it is",
            marker.display()
        ),
    }
    let _ = fs::remove_file(&marker);
}

fn firewall_port_left_open() -> bool {
    firewalld_running() && firewalld_port_open()
}

/// Remove the qualification's temporary account if an interrupted run left
/// it. Only the account the Pier creates (recognised by its `GECOS`) is
/// ever removed; an `arcen-wlq` someone else made is left alone.
fn remove_qualification_user(options: &Options) {
    if !is_root_prefix(&options.prefix) {
        return;
    }
    let passwd = fs::read_to_string("/etc/passwd").unwrap_or_default();
    match desktop::local_account_gecos(&passwd, QUALIFY_TEMPORARY_USER) {
        None => {}
        Some(gecos) if gecos != QUALIFY_TEMPORARY_USER_GECOS => println!(
            "kept local account {QUALIFY_TEMPORARY_USER}: it is not the qualification's \
             temporary account"
        ),
        Some(_) if options.dry_run => println!(
            "dry-run: would remove the leftover temporary account {QUALIFY_TEMPORARY_USER}"
        ),
        Some(_) => {
            let _ = Command::new("/usr/bin/loginctl")
                .args(["terminate-user", QUALIFY_TEMPORARY_USER])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
            for _ in 0..80 {
                let known = Command::new("/usr/bin/loginctl")
                    .args(["show-user", QUALIFY_TEMPORARY_USER])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .is_ok_and(|status| status.success());
                if !known {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
            let removed = Command::new("/usr/sbin/userdel")
                .args(["--remove", QUALIFY_TEMPORARY_USER])
                .status()
                .is_ok_and(|status| status.success());
            if removed {
                println!("removed the leftover temporary account {QUALIFY_TEMPORARY_USER}");
            } else {
                println!(
                    "warning: could not remove the temporary account {QUALIFY_TEMPORARY_USER}; \
                     run `userdel --remove {QUALIFY_TEMPORARY_USER}`"
                );
            }
        }
    }
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
    /// Installed, deliberately not started: the Pier would refuse to start
    /// on this host (a Wayland host that did not qualify).
    HeldBack(String),
}

/// Carries the unit's outcome through the shared installer transaction, whose
/// `finish` is the only thing that decides whether the install succeeded.
fn finish_install(
    mut transaction: arcen_session::install_lifecycle::InstallTransaction,
    outcome: &ServiceOutcome,
) -> Result<(), String> {
    use arcen_session::install_lifecycle::InstallEvent;
    let events: &[InstallEvent] = match outcome {
        ServiceOutcome::NotRequested | ServiceOutcome::HeldBack(_) => {
            &[InstallEvent::ServiceNotRequested]
        }
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
    let finished = transaction.finish().map(|_| ());
    if let (Ok(()), ServiceOutcome::HeldBack(reason)) = (&finished, outcome) {
        return Err(format!(
            "the Pier is installed, but arcen-pier.service was not started: {reason}"
        ));
    }
    finished.map_err(|error| match outcome {
        ServiceOutcome::NotRunning(state) => format!(
            "{error}: arcen-pier.service is {state:?}, not active. The files are installed; \
             see `journalctl -u arcen-pier` for why the service stopped"
        ),
        _ => error.to_string(),
    })
}

fn install(options: &Options) -> Result<(), String> {
    let mut log = desktop::InstallLog::default();
    let result = install_logged(options, &mut log);
    if let Err(error) = &result {
        log.record(format!("install-arcen-pier: {error}"));
    }
    if is_root_prefix(&options.prefix) && !options.dry_run {
        if let Err(error) = log.append_to(Path::new(desktop::INSTALL_LOG), "install") {
            eprintln!("warning: install log: {error}");
        }
    }
    result
}

fn install_logged(options: &Options, log: &mut desktop::InstallLog) -> Result<(), String> {
    use arcen_session::install_lifecycle::{InstallEvent, InstallTransaction};
    let mut transaction = InstallTransaction::new();
    let plan = detect_desktop(options, log)?;
    preflight(options, plan)?;
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
    let xorg_written = options.force || !map_path(&options.prefix, XORG_CONFIG_PATH).exists();
    write_atomic(
        options,
        XORG_CONFIG_PATH,
        XORG_TEMPLATE.as_bytes(),
        0o644,
        options.force,
    )?;
    if let Some(InstallPlan::Wayland { .. }) = plan {
        println!(
            "xorg: not used by this host's Wayland desktop; {XORG_CONFIG_PATH} names no GPU \
             (run the installer again if you install an Xorg server)"
        );
    } else {
        configure_xorg_gpu(options, xorg_written);
    }
    ensure_cert(options)?;
    install_selinux_module(options)?;
    install_polkit_rule(options);
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
    if options.disable_gdm && is_root_prefix(&options.prefix) {
        display_manager::disable(options.dry_run, log)?;
    }
    let outcome = if let Some(InstallPlan::Wayland { .. }) = plan {
        activate_wayland(options, log)?
    } else {
        let outcome = enable_and_start(options)?;
        report_pending_restart(options);
        outcome
    };
    if let Some(plan) = plan {
        log.note(format!(
            "Desktop provider: {}{}",
            match plan {
                InstallPlan::Xorg { .. } => "X11",
                InstallPlan::Wayland { .. } => "Wayland",
            },
            match &outcome {
                ServiceOutcome::HeldBack(_) => ", NOT serving: the service was not started",
                ServiceOutcome::NotRequested if options.dry_run => ", dry run: nothing changed",
                ServiceOutcome::NotRequested => ", service not started",
                ServiceOutcome::Running => ", service running",
                ServiceOutcome::NotRunning(_) => ", service not running",
            }
        ));
    }
    if is_root_prefix(&options.prefix) && !options.dry_run {
        println!();
        println!("Administration guide: /usr/share/doc/arcen/pier-administration.md");
    }
    finish_install(transaction, &outcome)
}

/// Qualify a Wayland host with the Pier just installed, then start it only
/// if that passed.
///
/// A Pier on an unqualified Wayland host refuses to start, so starting it
/// anyway would only spend systemd's restart budget and leave a failed
/// unit. Instead a failed qualification leaves the Pier installed, the
/// unit not started, and the operator told exactly what failed and what to
/// run once it is fixed. Qualification cannot run beside a running Pier
/// (they would compete for the encoder), so an upgrade under a running
/// service qualifies only with `--restart`.
fn activate_wayland(
    options: &Options,
    log: &mut desktop::InstallLog,
) -> Result<ServiceOutcome, String> {
    if options.dry_run {
        println!(
            "dry-run: would run `{PIER_PATH} wayland qualify --write`, then enable and start \
             arcen-pier.service only if it passes"
        );
        return Ok(ServiceOutcome::NotRequested);
    }
    if !is_root_prefix(&options.prefix) {
        return Ok(ServiceOutcome::NotRequested);
    }
    if options.no_service {
        log.note(format!(
            "--no-service: the Wayland qualification was not run and the unit was not enabled. \
             Before starting the service run `{PIER_PATH} wayland qualify --write`, then \
             `systemctl enable --now arcen-pier.service`."
        ));
        return Ok(ServiceOutcome::NotRequested);
    }
    let active = service_active();
    if active && !options.restart {
        let outcome = enable_and_start(options)?;
        if record_matches_installed_pier() {
            report_pending_restart(options);
        } else {
            report_pending_wayland_qualification(log);
        }
        return Ok(outcome);
    }
    if active {
        log.note("restarting arcen-pier: stopping the previous Pier to qualify this build");
        run_systemctl(&["stop", "arcen-pier.service"])?;
    }
    log.note(
        "Wayland: qualifying this host with the installed Pier: a short real session as the \
         temporary local account arcen-wlq, removed afterwards (about a minute per display size: \
         those the previous record measured, those Decks asked Speed for, and the default \
         1920x1080, 2560x1600 and 3840x2160 at 60 Hz)",
    );
    let run = desktop::run_qualification(Path::new(PIER_PATH), &options.qualify_outputs, log)?;
    if run.passed {
        log.note(format!(
            "Wayland qualification passed; recorded in {RECORD_PATH}"
        ));
        explain_native_desktops(log);
        return enable_and_start(options);
    }
    let enabled = Command::new("/usr/bin/systemctl")
        .args(["is-enabled", "arcen-pier.service"])
        .output()
        .ok()
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .unwrap_or_default();
    let status = run.exit_code.map_or_else(
        || "it was killed".to_string(),
        |code| format!("exit status {code}"),
    );
    log.note("");
    log.note("=====================================================================");
    log.note(format!(
        " The Pier is installed but NOT started: the Wayland qualification failed ({status})."
    ));
    log.note("");
    if run.failures.is_empty() {
        log.note(" What failed: see the qualification output above.");
    } else {
        log.note(" What failed:");
        for failure in &run.failures {
            log.note(format!("   {failure}"));
        }
    }
    log.note("");
    log.note(" The Pier refuses to start on a Wayland host that has not qualified, so");
    log.note(" arcen-pier.service was not started. Fix what is named above, then run");
    log.note(" (it measures every display size already known; nothing needs repeating):");
    log.note("");
    log.note(format!("   sudo {PIER_PATH} wayland qualify --write"));
    log.note("   sudo systemctl enable --now arcen-pier.service");
    log.note("");
    log.note(" or run this installer again.");
    if enabled == "enabled" {
        log.note(" (The unit is still enabled from an earlier install; until a qualification");
        log.note("  passes it will refuse to start at boot.)");
    }
    log.note(format!(" Full output: {}", desktop::INSTALL_LOG));
    log.note("=====================================================================");
    Ok(ServiceOutcome::HeldBack(format!(
        "the Wayland qualification failed ({status})"
    )))
}

/// What the record says about native desktops, and the display manager
/// beside them.
fn explain_native_desktops(log: &mut desktop::InstallLog) {
    let proof = fs::read_to_string(RECORD_PATH)
        .ok()
        .and_then(|text| {
            serde_json::from_str::<
                    arcen_session::wayland_qualification::WaylandQualificationRecord,
                >(&text)
                .ok()
        })
        .and_then(|record| record.facts.native_seat);
    match proof {
        Some(proof) if proof.proven() => {
            log.note(
                "Wayland: native desktops proven: a desktop runs on its own seat with a real pen \
                 (one per host; wayland.session_mode = auto)",
            );
            if let Some(manager) = display_manager::current().filter(|manager| manager.active) {
                for line in display_manager::explanation(&manager.unit) {
                    log.note(line);
                }
            }
        }
        Some(proof) => {
            let reason = proof
                .unavailable
                .unwrap_or(arcen_session::native_seat::NativeSeatUnavailable::QualificationFailed);
            log.note(format!(
                "Wayland: desktops are headless ({}): {}",
                reason.token(),
                reason.explanation()
            ));
        }
        None => {}
    }
}

fn service_active() -> bool {
    Command::new("/usr/bin/systemctl")
        .args(["is-active", "--quiet", "arcen-pier.service"])
        .status()
        .is_ok_and(|status| status.success())
}

/// Whether the qualification record on disk was written by exactly the Pier
/// now installed (its `host.pier.executable_sha256`).
fn record_matches_installed_pier() -> bool {
    let Ok(record) = fs::read_to_string(RECORD_PATH) else {
        return false;
    };
    let Some(recorded) = serde_json::from_str::<serde_json::Value>(&record)
        .ok()
        .and_then(|value| {
            value
                .pointer("/host/pier/executable_sha256")
                .and_then(serde_json::Value::as_str)
                .map(str::to_ascii_lowercase)
        })
    else {
        return false;
    };
    openssl_stdout(&["dgst", "-sha256", "-r", PIER_PATH], None).is_ok_and(|output| {
        String::from_utf8_lossy(&output)
            .split_whitespace()
            .next()
            .is_some_and(|digest| digest.eq_ignore_ascii_case(&recorded))
    })
}

/// An upgrade under a running Wayland Pier: the new build needs its own
/// qualification, which cannot run beside the old one.
fn report_pending_wayland_qualification(log: &mut desktop::InstallLog) {
    for line in [
        "",
        "=====================================================================",
        " NOTE: the running service is still the PREVIOUS Pier, and this host",
        " serves Wayland: its qualification record belongs to that build.",
        "",
        " The new Pier qualifies this host itself when the service next starts",
        " (a restart or reboot; a few minutes before it listens, each step in",
        " the log), keeping every display size already known. To do it now",
        " and watch it (this disconnects every live session):",
        "",
        "   sudo systemctl stop arcen-pier.service",
        &format!("   sudo {PIER_PATH} wayland qualify --write"),
        "   sudo systemctl start arcen-pier.service",
        "",
        " or re-run this installer with --restart. If capture.wayland_auto_requalify",
        " is false, the service refuses to start until you do.",
        "=====================================================================",
    ] {
        log.note(line);
    }
}

/// Load Arcen's SELinux policy module where SELinux is enabled.
///
/// Without it an enforcing host refuses every session: the targeted policy
/// does not let `unconfined_service_t` enter the user's PAM-assigned context,
/// and the launcher will not run the user's desktop in the service's domain
/// instead. `semodule -i` replaces an older copy of the module in place.
fn install_selinux_module(options: &Options) -> Result<(), String> {
    write_atomic(
        options,
        SELINUX_MODULE_PATH,
        SELINUX_MODULE.as_bytes(),
        0o644,
        true,
    )?;
    if !is_root_prefix(&options.prefix) {
        println!("staging prefix: skipped loading the SELinux policy module");
        return Ok(());
    }
    if !Path::new(SELINUXFS_ENFORCE).exists() {
        println!(
            "SELinux is not enabled; the Arcen policy module was not loaded \
             (it is kept at {SELINUX_MODULE_PATH})"
        );
        return Ok(());
    }
    if options.dry_run {
        println!("dry-run: would run {SEMODULE} -i {SELINUX_MODULE_PATH}");
        return Ok(());
    }
    // Preflight refused a host with SELinux enabled and no semodule.
    let status = Command::new(SEMODULE)
        .args(["-i", SELINUX_MODULE_PATH])
        .status()
        .map_err(|error| format!("start {SEMODULE} -i: {error}"))?;
    if !status.success() || !selinux_module_loaded(Path::new(SEMODULE))? {
        return Err(format!(
            "{SEMODULE} -i {SELINUX_MODULE_PATH} failed; sessions on this SELinux host would \
             be refused (see /usr/share/doc/arcen/pier-administration.md, SELinux)"
        ));
    }
    println!("SELinux: loaded policy module {SELINUX_MODULE_NAME}");
    Ok(())
}

/// Arcen's polkit rule: a remote desktop with a seat (every X11 desktop) is
/// remote to polkit only with it, and the Pier refuses such desktops while it
/// is missing. The Pier writes it from the actions installed now
/// (`polkit-rule install`) and rewrites it at every start, so a failure here
/// is reported, not fatal.
fn install_polkit_rule(options: &Options) {
    use arcen_session::polkit_remote::RULE_PATH;
    if !is_root_prefix(&options.prefix) {
        println!("staging prefix: skipped the polkit rule ({RULE_PATH})");
        return;
    }
    if options.dry_run {
        println!("dry-run: would run `{PIER_PATH} polkit-rule install` ({RULE_PATH})");
        return;
    }
    match Command::new(PIER_PATH)
        .args(["polkit-rule", "install"])
        .output()
    {
        Ok(output) => {
            for line in String::from_utf8_lossy(&output.stdout)
                .lines()
                .chain(String::from_utf8_lossy(&output.stderr).lines())
            {
                println!("{line}");
            }
            if !output.status.success() {
                println!(
                    "warning: the polkit rule is not installed (see \
                     /usr/share/doc/arcen/pier-administration.md, polkit)"
                );
            }
        }
        Err(error) => println!(
            "warning: {PIER_PATH} polkit-rule install did not run ({error}); the Pier writes \
             the rule at start"
        ),
    }
}

/// Removes Arcen's polkit rules (this build's and earlier builds'), never a
/// file at those paths that Arcen did not write.
fn remove_polkit_rules(options: &Options) -> Result<(), String> {
    use arcen_session::polkit_remote::{LEGACY_RULE_PATHS, RULE_PATH, is_arcen_rule};
    for path in std::iter::once(RULE_PATH).chain(LEGACY_RULE_PATHS.iter().copied()) {
        if fs::read_to_string(map_path(&options.prefix, path))
            .is_ok_and(|text| is_arcen_rule(&text))
        {
            remove_file(options, path)?;
        }
    }
    Ok(())
}

/// Unload the module and remove its source, so an uninstalled host carries
/// no Arcen policy.
///
/// Runs before any payload is removed and never fails the uninstall: a
/// `semodule` that cannot list or remove the module is reported with the
/// command that finishes the job, and the rest of the uninstall (unit,
/// `daemon-reload`) still happens. `semodule` is consulted only where the
/// module can be loaded: SELinux is enabled, or this installer left the
/// module's source on the host.
fn remove_selinux_module(options: &Options) -> Result<(), String> {
    if options.dry_run {
        println!("dry-run: would run {SEMODULE} -r {SELINUX_MODULE_NAME} if it is loaded");
    } else if is_root_prefix(&options.prefix) {
        let may_be_loaded = Path::new(SELINUXFS_ENFORCE).exists()
            || map_path(&options.prefix, SELINUX_MODULE_PATH).exists();
        if let Some(warning) = unload_selinux_module(Path::new(SEMODULE), may_be_loaded) {
            println!("warning: {warning}");
        }
    }
    remove_file(options, SELINUX_MODULE_PATH)?;
    remove_dir_if_empty(options, SELINUX_DIR);
    remove_dir_if_empty(options, "/usr/share/arcen");
    Ok(())
}

/// Removes the `arcen` module through `semodule`. `None` when it is gone
/// (or could not have been loaded); otherwise the warning to print.
fn unload_selinux_module(semodule: &Path, may_be_loaded: bool) -> Option<String> {
    if !may_be_loaded || !semodule.exists() {
        return None;
    }
    let manual = format!(
        "if `{} -l` lists {SELINUX_MODULE_NAME}, remove it with `{} -r {SELINUX_MODULE_NAME}`",
        semodule.display(),
        semodule.display()
    );
    match selinux_module_loaded(semodule) {
        Err(error) => return Some(format!("{error}; {manual}")),
        Ok(false) => return None,
        Ok(true) => {}
    }
    let removed = Command::new(semodule)
        .args(["-r", SELINUX_MODULE_NAME])
        .status()
        .is_ok_and(|status| status.success());
    match (removed, selinux_module_loaded(semodule)) {
        (true, Ok(false)) => {
            println!("SELinux: removed policy module {SELINUX_MODULE_NAME}");
            None
        }
        _ => Some(format!(
            "the SELinux policy module {SELINUX_MODULE_NAME} could not be removed; {manual}"
        )),
    }
}

fn selinux_module_loaded(semodule: &Path) -> Result<bool, String> {
    let output = Command::new(semodule)
        .arg("-l")
        .output()
        .map_err(|error| format!("start {} -l: {error}", semodule.display()))?;
    if !output.status.success() {
        return Err(format!("{} -l failed", semodule.display()));
    }
    Ok(module_listed(
        &String::from_utf8_lossy(&output.stdout),
        SELINUX_MODULE_NAME,
    ))
}

/// Whether `semodule -l` output lists `name` as a module of its own.
fn module_listed(listing: &str, name: &str) -> bool {
    listing
        .lines()
        .any(|line| line.split_whitespace().next() == Some(name))
}

/// Name this machine's NVIDIA GPU in `/etc/arcen/xorg.conf`.
///
/// The `BusID` is a fact about one machine, so the packaged template carries
/// none, and a file from an earlier package may name another machine's GPU.
/// Either way the NVIDIA driver then finds no device and no session can start.
/// The shared `arcen_outputs::decide_xorg_gpu` decides from sysfs; this applies
/// it. Only on the real root: a staging prefix describes another machine's
/// files, not this one's GPU. Never fails the install: the Pier re-checks the
/// `BusID` before every X server it starts.
fn configure_xorg_gpu(options: &Options, freshly_written: bool) {
    if !is_root_prefix(&options.prefix) {
        println!("staging prefix: skipped choosing the GPU for {XORG_CONFIG_PATH}");
        return;
    }
    let step = XorgGpuStep {
        config: Path::new(XORG_CONFIG_PATH),
        sysfs_root: Path::new(gpu_pci::SYSFS_ROOT),
        dry_run: options.dry_run,
        freshly_written,
        backup_suffix: env!("CARGO_PKG_VERSION"),
        owner: 0,
    };
    match step.run() {
        Ok(lines) => {
            for line in lines {
                println!("{line}");
            }
        }
        Err(warning) => println!("warning: xorg: {warning}; {XORG_CONFIG_PATH} was not changed"),
    }
}

/// One run of the `BusID` step against an explicit file and sysfs root.
struct XorgGpuStep<'a> {
    config: &'a Path,
    sysfs_root: &'a Path,
    dry_run: bool,
    /// The installer has just written the stock template there (or would
    /// have, in a dry run), so there is no operator file to back up.
    freshly_written: bool,
    /// Names the backup of an operator file: `xorg.conf.before-<suffix>`.
    backup_suffix: &'a str,
    /// The uid the file must belong to: root, except in tests.
    owner: u32,
}

impl XorgGpuStep<'_> {
    /// What was decided and done, one terminal line each, or why nothing was.
    fn run(&self) -> Result<Vec<String>, String> {
        let label = self.config.display().to_string();
        let text = if self.dry_run && self.freshly_written {
            XORG_TEMPLATE.to_owned()
        } else {
            self.read_trusted(&label)?
        };
        let candidates = gpu_pci::pci_candidates(self.sysfs_root).map_err(|error| {
            format!(
                "cannot list PCI devices under {}: {error}",
                self.sysfs_root.display()
            )
        })?;
        let decision = arcen_outputs::decide_xorg_gpu(&text, &candidates)
            .map_err(|error| gpu_pci::describe_template_error(&label, &error))?;
        let mut lines = Vec::new();
        match &decision {
            arcen_outputs::XorgGpuDecision::Keep { .. } => {
                lines.push(format!(
                    "xorg: {}",
                    gpu_pci::describe_decision(&label, &decision, "keeping")
                ));
            }
            arcen_outputs::XorgGpuDecision::NoNvidiaGpu { .. } => {
                lines.push(format!(
                    "warning: xorg: {}. NVIDIA display devices found: {}. Nothing was changed; \
                     install the NVIDIA driver, then rerun this installer",
                    gpu_pci::describe_decision(&label, &decision, "keeping"),
                    gpu_pci::describe_nvidia_gpus(&candidates)
                ));
            }
            arcen_outputs::XorgGpuDecision::Set { address, .. } => {
                let verb = if self.dry_run { "would set" } else { "setting" };
                let prefix = if self.dry_run {
                    "dry-run: xorg"
                } else {
                    "xorg"
                };
                lines.push(format!(
                    "{prefix}: {} so headless sessions can start",
                    gpu_pci::describe_decision(&label, &decision, verb)
                ));
                if let Some(others) = gpu_pci::describe_other_gpus(&label, &decision) {
                    lines.push(format!("{prefix}: {others}"));
                }
                let updated = arcen_outputs::apply_bus_id(&text, *address)
                    .map_err(|error| gpu_pci::describe_template_error(&label, &error))?;
                if !self.freshly_written {
                    if self.dry_run {
                        let backup = self
                            .backup_candidates()
                            .find(|candidate| fs::symlink_metadata(candidate).is_err())
                            .unwrap_or_else(|| self.backup_name(None));
                        lines.push(format!(
                            "dry-run: would keep the previous file as {}",
                            backup.display()
                        ));
                    } else {
                        let backup = self.keep_backup(text.as_bytes())?;
                        lines.push(format!("xorg: previous file kept as {}", backup.display()));
                    }
                }
                if !self.dry_run {
                    write_atomic_path(self.config, updated.as_bytes(), 0o644)?;
                }
            }
        }
        Ok(lines)
    }

    /// Reads the existing file the way the launcher does: without following a
    /// symlink, and only when it is a regular root-owned file that group and
    /// other users cannot write. Anything else was not written by an
    /// administrator through the usual route, and rewriting it root-owned
    /// `0644` would hand an untrusted edit to a root X server.
    fn read_trusted(&self, label: &str) -> Result<String, String> {
        use std::io::Read as _;
        use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};

        let refuse = |reason: String| {
            format!(
                "{label} is not safe to repair: {reason}. The Pier refuses it too; make it a \
                 regular file owned by root with mode 0644, then rerun this installer"
            )
        };
        let nofollow = rustix::fs::OFlags::NOFOLLOW.bits() as i32;
        let mut file = match OpenOptions::new()
            .read(true)
            .custom_flags(nofollow)
            .open(self.config)
        {
            Ok(file) => file,
            Err(_)
                if fs::symlink_metadata(self.config)
                    .is_ok_and(|metadata| metadata.file_type().is_symlink()) =>
            {
                return Err(refuse("it is a symbolic link".to_owned()));
            }
            Err(error) => return Err(format!("read {label}: {error}")),
        };
        let metadata = file
            .metadata()
            .map_err(|error| format!("read {label}: {error}"))?;
        let trust = xorg_template_trust::template_trust(
            metadata.file_type().is_file(),
            metadata.uid(),
            metadata.mode(),
            self.owner,
        );
        if trust != xorg_template_trust::TemplateTrust::Trusted {
            return Err(refuse(trust.describe()));
        }
        let mut text = String::new();
        file.read_to_string(&mut text)
            .map_err(|error| format!("read {label}: {error}"))?;
        Ok(text)
    }

    /// `xorg.conf.before-<suffix>`, then `.1`, `.2`, ... for later repairs.
    fn backup_name(&self, attempt: Option<u32>) -> PathBuf {
        let name = self.config.file_name().map_or_else(
            || "xorg.conf".to_owned(),
            |name| name.to_string_lossy().into_owned(),
        );
        let base = format!("{name}.before-{}", self.backup_suffix);
        self.config.with_file_name(match attempt {
            None => base,
            Some(attempt) => format!("{base}.{attempt}"),
        })
    }

    fn backup_candidates(&self) -> impl Iterator<Item = PathBuf> + '_ {
        std::iter::once(None)
            .chain((1..=MAX_BACKUP_ATTEMPTS).map(Some))
            .map(|attempt| self.backup_name(attempt))
    }

    /// Writes `content` to a staged file, then links it into the first free
    /// backup name. `link` never replaces an existing name, so an earlier
    /// backup — including one made a moment ago by another run — is never
    /// overwritten, and the backup appears whole or not at all.
    fn keep_backup(&self, content: &[u8]) -> Result<PathBuf, String> {
        let staged = self
            .config
            .with_file_name(format!(".xorg.conf.backup.{}", std::process::id()));
        match fs::remove_file(&staged) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("remove {}: {error}", staged.display())),
        }
        write_file_path(&staged, content, 0o644)?;
        let mut linked = None;
        for candidate in self.backup_candidates() {
            match fs::hard_link(&staged, &candidate) {
                Ok(()) => {
                    linked = Some(candidate);
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    let _ = fs::remove_file(&staged);
                    return Err(format!("keep backup {}: {error}", candidate.display()));
                }
            }
        }
        let _ = fs::remove_file(&staged);
        let backup = linked.ok_or_else(|| {
            format!(
                "every backup name up to {} is taken",
                self.backup_name(Some(MAX_BACKUP_ATTEMPTS)).display()
            )
        })?;
        if let Some(parent) = backup.parent() {
            fsync_directory(parent)?;
        }
        Ok(backup)
    }
}

/// How many numbered backups of `xorg.conf` one version may leave.
const MAX_BACKUP_ATTEMPTS: u32 = 999;

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
    } else if is_root_prefix(&options.prefix)
        && map_path(&options.prefix, "/etc/systemd/system/arcen-pier.service").exists()
    {
        // Only a unit that is installed is stopped: on a host that was
        // already uninstalled, systemctl would print "Failed to stop" and
        // "Failed to disable" for a unit it does not know.
        let _ = Command::new("systemctl")
            .args(["stop", "arcen-pier.service"])
            .status();
        let _ = Command::new("systemctl")
            .args(["disable", "arcen-pier.service"])
            .status();
    }
    remove_selinux_module(options)?;
    remove_polkit_rules(options)?;
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
    // What the Wayland path and the firewall step added. The record names
    // this build and is meaningless without it; the account exists only if
    // a qualification was interrupted.
    remove_qualification_user(options);
    if is_root_prefix(&options.prefix) {
        let mut log = desktop::InstallLog::default();
        if let Err(error) = display_manager::restore(
            options.dry_run,
            display_manager::RestoreMode::Uninstall,
            &mut log,
        ) {
            eprintln!("warning: display manager not restored: {error}");
        }
    }
    remove_file(options, RECORD_PATH)?;
    remove_file(
        options,
        &QualificationTarget::production().requested_outputs_path(),
    )?;
    remove_file(
        options,
        &QualificationTarget::production().requalify_marker_path(),
    )?;
    close_firewall(options);
    remove_dir_if_empty(options, "/var/lib/arcen");
    remove_dir_if_empty(options, "/run/arcen/sessions");
    remove_dir_if_empty(options, "/run/arcen");
    if options.purge {
        preserve_config_before_purge(options)?;
        remove_dir_all(options, "/etc/arcen")?;
        remove_dir_all(options, "/var/lib/arcen")?;
        remove_dir_all(options, "/var/log/arcen")?;
        // Runtime state the stopped service left in tmpfs (the NVIDIA
        // discovery cache under sessions/), which the message below says
        // was removed.
        remove_dir_all(options, "/run/arcen")?;
    }
    if !options.dry_run && is_root_prefix(&options.prefix) {
        run_systemctl(&["daemon-reload"])?;
        if !options.purge {
            let mut log = desktop::InstallLog::default();
            log.record("uninstalled; configuration and logs kept (--purge removes them)");
            if let Err(error) = log.append_to(Path::new(desktop::INSTALL_LOG), "uninstall") {
                eprintln!("warning: install log: {error}");
            }
        }
    }
    if options.purge {
        println!("uninstall complete; configuration, runtime state and logs were removed");
    } else {
        println!(
            "uninstall complete; the configuration (/etc/arcen), the logs (/var/log/arcen) and any other \
             state in /var/lib/arcen were kept (use --purge to remove them)"
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
    fn the_firewall_marker_records_only_what_the_installer_changed() {
        for marker in [
            FirewallMarker {
                owner: FirewallOwner::Firewalld,
                runtime_was_open: false,
            },
            FirewallMarker {
                owner: FirewallOwner::Firewalld,
                runtime_was_open: true,
            },
            FirewallMarker {
                owner: FirewallOwner::Ufw,
                runtime_was_open: false,
            },
        ] {
            assert_eq!(FirewallMarker::parse(&marker.render()), Some(marker));
        }
        // Markers written before the runtime flag existed.
        assert_eq!(
            FirewallMarker::parse("firewalld\n"),
            Some(FirewallMarker {
                owner: FirewallOwner::Firewalld,
                runtime_was_open: false,
            })
        );
        assert_eq!(FirewallMarker::parse("nftables\n"), None);
        assert_eq!(FirewallMarker::parse(""), None);
        assert!(ufw_lists_quic_rule(
            "ufw allow 22/tcp\nufw allow 18444/udp\n"
        ));
        assert!(!ufw_lists_quic_rule("ufw allow 18443/tcp\n"));
    }

    #[test]
    fn selinux_module_listing_matches_only_the_arcen_module() {
        assert!(module_listed("abrt\narcen\nzabbix\n", "arcen"));
        assert!(!module_listed("arcen_extra\nmyarcen\n", "arcen"));
        assert!(!module_listed("", "arcen"));
    }

    #[test]
    fn the_shipped_selinux_module_adds_only_the_session_transition() {
        let rules: Vec<&str> = SELINUX_MODULE
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with(';'))
            .collect();
        assert_eq!(
            rules,
            ["(allow unconfined_service_t unconfined_t (process (transition)))"],
            "any widening of the SELinux module needs Release/Security review"
        );
    }

    #[test]
    fn staging_install_and_uninstall_of_the_selinux_module_are_symmetric() {
        let prefix = test_prefix("selinux");
        let options = Options {
            prefix: prefix.clone(),
            dry_run: false,
            uninstall: false,
            purge: false,
            force: false,
            no_service: true,
            restart: false,
            check: false,
            qualify_outputs: Vec::new(),
            disable_gdm: false,
            restore_gdm: false,
            extra_sans: Vec::new(),
        };
        install_selinux_module(&options).expect("stage the module");
        let staged = prefix.join("usr/share/arcen/selinux/arcen.cil");
        assert_eq!(
            fs::read_to_string(&staged).expect("staged module"),
            SELINUX_MODULE
        );
        remove_selinux_module(&options).expect("remove the module");
        assert!(!staged.exists());
        assert!(!prefix.join("usr/share/arcen").exists());
        let _ = fs::remove_dir_all(&prefix);
    }

    #[test]
    fn uninstall_removes_arcen_polkit_rules_and_keeps_foreign_ones() {
        use arcen_session::polkit_remote::{CatPath, LEGACY_RULE_PATHS, RULE_PATH, rule_text};
        let prefix = test_prefix("polkit");
        let options = Options {
            prefix: prefix.clone(),
            dry_run: false,
            uninstall: true,
            purge: false,
            force: false,
            no_service: true,
            restart: false,
            check: false,
            qualify_outputs: Vec::new(),
            extra_sans: Vec::new(),
            disable_gdm: false,
            restore_gdm: false,
        };
        let ours = map_path(&prefix, RULE_PATH);
        let legacy = map_path(&prefix, LEGACY_RULE_PATHS[0]);
        let foreign = ours.with_file_name("04-site.rules");
        fs::create_dir_all(ours.parent().expect("rules dir")).expect("rules dir");
        fs::write(&ours, rule_text(&[], CatPath::UsrBin)).expect("our rule");
        fs::write(
            &legacy,
            "// Arcen native-seat polkit rule (generated by arcen-pier; do not edit).\n",
        )
        .expect("legacy rule");
        fs::write(&foreign, "polkit.addRule(function () { return null; });\n")
            .expect("foreign rule");
        remove_polkit_rules(&options).expect("remove rules");
        assert!(!ours.exists());
        assert!(!legacy.exists());
        assert!(foreign.exists(), "a rule Arcen did not write stays");
        fs::write(&ours, "// someone else's rule at Arcen's path\n").expect("squatter");
        remove_polkit_rules(&options).expect("remove rules");
        assert!(
            ours.exists(),
            "a file at Arcen's path that Arcen did not write stays"
        );
        let _ = fs::remove_dir_all(&prefix);
    }

    /// A stand-in `semodule`: `-l` lists `arcen` while `state` exists and
    /// fails when `list-fails` exists; `-r` removes `state` unless
    /// `remove-fails` exists.
    fn fake_semodule(directory: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        fs::create_dir_all(directory).expect("fake semodule directory");
        let script = directory.join("semodule");
        let dir = directory.display();
        fs::write(
            &script,
            format!(
                "#!/bin/sh\n\
                 case \"$1\" in\n\
                 -l) [ -e '{dir}'/list-fails ] && exit 1; echo base; [ -e '{dir}'/state ] && echo arcen; exit 0;;\n\
                 -r) [ -e '{dir}'/remove-fails ] && exit 1; rm -f '{dir}'/state; exit 0;;\n\
                 esac\nexit 2\n"
            ),
        )
        .expect("write fake semodule");
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("chmod");
        script
    }

    #[test]
    fn selinux_module_removal_never_aborts_and_says_how_to_finish() {
        let directory = test_prefix("semodule");
        let semodule = fake_semodule(&directory);
        let mark = |name: &str| fs::write(directory.join(name), b"").expect("mark");

        // Not loadable here (SELinux off, no module source): semodule is
        // not consulted at all, even one that would fail.
        mark("list-fails");
        assert_eq!(unload_selinux_module(&semodule, false), None);
        // A semodule that cannot list is a warning naming the manual step.
        let warning = unload_selinux_module(&semodule, true).expect("listing failure warns");
        assert!(warning.contains("-r arcen"), "{warning}");
        fs::remove_file(directory.join("list-fails")).expect("unmark");

        // Not loaded: nothing to do.
        assert_eq!(unload_selinux_module(&semodule, true), None);
        // Loaded and removable: removed.
        mark("state");
        assert_eq!(unload_selinux_module(&semodule, true), None);
        assert!(!directory.join("state").exists());
        // Loaded but removal fails: a warning, not an error.
        mark("state");
        mark("remove-fails");
        let warning = unload_selinux_module(&semodule, true).expect("removal failure warns");
        assert!(warning.contains("could not be removed"), "{warning}");
        assert!(warning.contains("-r arcen"), "{warning}");
        // No semodule at all: nothing to run.
        assert_eq!(unload_selinux_module(&directory.join("absent"), true), None);
        let _ = fs::remove_dir_all(&directory);
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
            check: false,
            qualify_outputs: Vec::new(),
            disable_gdm: false,
            restore_gdm: false,
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
            check: false,
            qualify_outputs: Vec::new(),
            disable_gdm: false,
            restore_gdm: false,
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
            check: false,
            qualify_outputs: Vec::new(),
            disable_gdm: false,
            restore_gdm: false,
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
            check: false,
            qualify_outputs: Vec::new(),
            disable_gdm: false,
            restore_gdm: false,
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
            check: false,
            qualify_outputs: Vec::new(),
            disable_gdm: false,
            restore_gdm: false,
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
            check: false,
            qualify_outputs: Vec::new(),
            disable_gdm: false,
            restore_gdm: false,
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
            check: false,
            qualify_outputs: Vec::new(),
            disable_gdm: false,
            restore_gdm: false,
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
                    check: false,
                    qualify_outputs: Vec::new(),
                    disable_gdm: false,
                    restore_gdm: false,
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
            check: false,
            qualify_outputs: Vec::new(),
            disable_gdm: false,
            restore_gdm: false,
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
            check: false,
            qualify_outputs: Vec::new(),
            disable_gdm: false,
            restore_gdm: false,
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

    const LAB_BUS_ID_LINE: &str = "    BusID          \"PCI:6:16:0\"\n";
    const DRIVER_LINE: &str = "    Driver         \"nvidia\"\n";

    /// The 0.16.1 template: the stock file with the development lab's GPU.
    fn lab_template() -> String {
        XORG_TEMPLATE.replacen(DRIVER_LINE, &format!("{DRIVER_LINE}{LAB_BUS_ID_LINE}"), 1)
    }

    struct XorgFixture {
        root: PathBuf,
        sysfs: gpu_pci::test_support::FakeSysfs,
        config: PathBuf,
        /// The test user stands in for root as the trusted owner.
        owner: u32,
    }

    impl XorgFixture {
        fn new(
            name: &str,
            existing: Option<&str>,
            gpus: &[(&str, &str, &str, Option<&str>)],
        ) -> Self {
            let root = test_prefix(name);
            let config = root.join("etc/arcen/xorg.conf");
            fs::create_dir_all(config.parent().expect("parent")).expect("create etc");
            if let Some(existing) = existing {
                fs::write(&config, existing).expect("write xorg.conf");
                chmod(&config, 0o644).expect("chmod");
            }
            let sysfs = gpu_pci::test_support::fake_sysfs(name, gpus);
            let owner = std::os::unix::fs::MetadataExt::uid(&fs::metadata(&root).expect("root"));
            Self {
                root,
                sysfs,
                config,
                owner,
            }
        }

        fn run(&self, dry_run: bool, freshly_written: bool) -> Result<Vec<String>, String> {
            XorgGpuStep {
                config: &self.config,
                sysfs_root: self.sysfs.path(),
                dry_run,
                freshly_written,
                backup_suffix: "0.16.1",
                owner: self.owner,
            }
            .run()
        }

        fn backup(&self) -> PathBuf {
            self.config.with_file_name("xorg.conf.before-0.16.1")
        }

        fn read(&self) -> String {
            fs::read_to_string(&self.config).expect("read xorg.conf")
        }
    }

    impl Drop for XorgFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    const NVIDIA_VGPU: (&str, &str, &str, Option<&str>) =
        ("0000:00:10.0", "0x10de", "0x030000", Some("nvidia"));
    const VIRTIO_VGA: (&str, &str, &str, Option<&str>) =
        ("0000:00:01.0", "0x1af4", "0x030000", Some("virtio-pci"));

    /// systemd counts a SIGHUP death as clean; the shipped unit restarts
    /// after one, and logrotate's reload reaches only the Pier, which
    /// handles SIGHUP from the start of start-up.
    #[test]
    fn the_unit_restarts_after_an_accidental_hangup_and_logrotate_only_reloads() {
        let directive = |name: &str| {
            SERVICE_TEMPLATE
                .lines()
                .find_map(|line| line.strip_prefix(name)?.strip_prefix('='))
                .map(str::trim)
        };
        assert_eq!(directive("Restart"), Some("on-failure"));
        let forced = directive("RestartForceExitStatus").unwrap_or_default();
        assert!(forced.split_whitespace().any(|status| status == "SIGHUP"));
        assert!(!forced.split_whitespace().any(|status| status == "SIGTERM"));
        assert!(
            directive("StartLimitBurst").is_some() && directive("StartLimitIntervalSec").is_some()
        );
        assert_eq!(directive("ExecReload"), Some("/bin/kill -HUP $MAINPID"));
        assert!(
            LOGROTATE_CONF.contains("systemctl kill --kill-who=main -s HUP arcen-pier.service")
        );
        assert!(!LOGROTATE_CONF.contains("copytruncate"));
    }

    #[test]
    fn the_stock_template_names_no_machines_gpu() {
        assert!(!XORG_TEMPLATE.contains("PCI:6:16:0"));
        assert_eq!(
            arcen_outputs::decide_xorg_gpu(XORG_TEMPLATE, &[]),
            Ok(arcen_outputs::XorgGpuDecision::NoNvidiaGpu { previous: None })
        );
    }

    #[test]
    fn a_stale_lab_bus_id_is_corrected_with_a_backup() {
        let original = lab_template();
        let fixture = XorgFixture::new("xorg-stale", Some(&original), &[VIRTIO_VGA, NVIDIA_VGPU]);
        let lines = fixture.run(false, false).expect("step");
        assert_eq!(
            lines[0],
            format!(
                "xorg: {} named BusID \"PCI:6:16:0\", but the NVIDIA GPU is at 0000:00:10.0; \
                 setting BusID \"PCI:0:16:0\" so headless sessions can start",
                fixture.config.display()
            )
        );
        assert_eq!(
            lines[1],
            format!("xorg: previous file kept as {}", fixture.backup().display())
        );
        assert_eq!(
            fixture.read(),
            original.replace("\"PCI:6:16:0\"", "\"PCI:0:16:0\"")
        );
        assert_eq!(
            fs::read_to_string(fixture.backup()).expect("backup"),
            original
        );
        for path in [&fixture.config, &fixture.backup()] {
            let mode = fs::metadata(path).expect("metadata").permissions().mode() & 0o777;
            assert_eq!(mode, 0o644, "{}", path.display());
        }

        // A second correction must not overwrite the operator's original.
        let second = original.replace("PCI:6:16:0", "PCI:7:0:0");
        fs::write(&fixture.config, &second).expect("restale");
        let lines = fixture.run(false, false).expect("step again");
        assert_eq!(
            fs::read_to_string(fixture.backup()).expect("backup"),
            original
        );
        let numbered = fixture.config.with_file_name("xorg.conf.before-0.16.1.1");
        assert_eq!(fs::read_to_string(&numbered).expect("numbered"), second);
        assert_eq!(
            lines[1],
            format!("xorg: previous file kept as {}", numbered.display())
        );
        let backups = fs::read_dir(fixture.config.parent().expect("parent"))
            .expect("list")
            .filter(|entry| {
                entry.as_ref().is_ok_and(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with("xorg.conf.before-")
                })
            })
            .count();
        assert_eq!(backups, 2);
    }

    #[test]
    fn a_freshly_written_template_gets_the_bus_id_without_a_backup() {
        let fixture = XorgFixture::new("xorg-fresh", Some(XORG_TEMPLATE), &[NVIDIA_VGPU]);
        let lines = fixture.run(false, true).expect("step");
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("had no BusID"), "{}", lines[0]);
        assert_eq!(
            fixture.read(),
            XORG_TEMPLATE.replacen(
                DRIVER_LINE,
                &format!("{DRIVER_LINE}    BusID          \"PCI:0:16:0\"\n"),
                1
            )
        );
        assert!(!fixture.backup().exists());
    }

    #[test]
    fn a_correct_bus_id_is_kept_untouched() {
        let original = lab_template().replace("PCI:6:16:0", "PCI:0:16:0");
        let fixture = XorgFixture::new("xorg-keep", Some(&original), &[NVIDIA_VGPU]);
        let lines = fixture.run(false, false).expect("step");
        assert_eq!(
            lines,
            [format!(
                "xorg: {} uses the NVIDIA GPU at PCI 0000:00:10.0 (BusID PCI:0:16:0)",
                fixture.config.display()
            )]
        );
        assert_eq!(fixture.read(), original);
        assert!(!fixture.backup().exists());
    }

    #[test]
    fn no_nvidia_gpu_warns_and_changes_nothing() {
        let original = lab_template();
        let fixture = XorgFixture::new(
            "xorg-none",
            Some(&original),
            &[
                VIRTIO_VGA,
                ("0000:3b:00.0", "0x10de", "0x030200", Some("vfio-pci")),
            ],
        );
        let lines = fixture.run(false, false).expect("step");
        assert_eq!(lines.len(), 1);
        assert!(lines[0].starts_with("warning: xorg: "), "{}", lines[0]);
        assert!(lines[0].contains("no usable NVIDIA GPU"), "{}", lines[0]);
        assert!(
            lines[0].contains("0000:3b:00.0 (bound to vfio-pci, not usable)"),
            "{}",
            lines[0]
        );
        assert!(lines[0].contains("Nothing was changed"), "{}", lines[0]);
        assert_eq!(fixture.read(), original);
        assert!(!fixture.backup().exists());
    }

    #[test]
    fn a_dry_run_reports_without_writing() {
        let original = lab_template();
        let fixture = XorgFixture::new("xorg-dry", Some(&original), &[NVIDIA_VGPU]);
        let lines = fixture.run(true, false).expect("step");
        assert!(lines[0].starts_with("dry-run: xorg: "), "{}", lines[0]);
        assert!(
            lines[0].contains("would set BusID \"PCI:0:16:0\""),
            "{}",
            lines[0]
        );
        assert!(lines[1].starts_with("dry-run: would keep the previous file as"));
        assert_eq!(fixture.read(), original);
        assert!(!fixture.backup().exists());

        // A dry run of a fresh install judges the template it would write.
        let fresh = XorgFixture::new("xorg-dry-fresh", None, &[NVIDIA_VGPU]);
        let lines = fresh.run(true, true).expect("step");
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("had no BusID"), "{}", lines[0]);
        assert!(!fresh.config.exists());
    }

    #[test]
    fn several_gpus_name_the_choice_and_the_alternatives() {
        let fixture = XorgFixture::new(
            "xorg-several",
            Some(XORG_TEMPLATE),
            &[
                ("0000:af:00.0", "0x10de", "0x030200", Some("nvidia")),
                ("0000:3b:00.0", "0x10de", "0x030200", Some("nvidia")),
            ],
        );
        let lines = fixture.run(false, true).expect("step");
        assert!(
            lines[0].contains("0000:3b:00.0; setting BusID \"PCI:59:0:0\""),
            "{}",
            lines[0]
        );
        assert!(
            lines[1].contains("the others are 0000:af:00.0 (BusID \"PCI:175:0:0\")"),
            "{}",
            lines[1]
        );
        assert!(lines[1].contains("set its BusID in"), "{}", lines[1]);
    }

    #[test]
    fn an_unreadable_sysfs_is_a_warning_not_a_failure() {
        let fixture = XorgFixture::new("xorg-nosysfs", Some(XORG_TEMPLATE), &[]);
        fs::remove_dir_all(fixture.sysfs.path().join("bus")).expect("remove fake pci");
        let error = fixture.run(false, true).expect_err("no sysfs");
        assert!(
            error.starts_with("cannot list PCI devices under"),
            "{error}"
        );
        assert_eq!(fixture.read(), XORG_TEMPLATE);
    }

    #[test]
    fn backups_never_replace_an_existing_name() {
        let original = lab_template();
        let fixture = XorgFixture::new("xorg-collide", Some(&original), &[NVIDIA_VGPU]);
        let first = fixture.backup();
        let second = fixture.config.with_file_name("xorg.conf.before-0.16.1.1");
        fs::write(&first, "first").expect("first");
        fs::write(&second, "second").expect("second");
        // A dangling symlink is a taken name too.
        let third = fixture.config.with_file_name("xorg.conf.before-0.16.1.2");
        std::os::unix::fs::symlink(fixture.root.join("nowhere"), &third).expect("symlink");
        let lines = fixture.run(false, false).expect("step");
        let fourth = fixture.config.with_file_name("xorg.conf.before-0.16.1.3");
        assert_eq!(
            lines[1],
            format!("xorg: previous file kept as {}", fourth.display())
        );
        assert_eq!(fs::read_to_string(&first).expect("first"), "first");
        assert_eq!(fs::read_to_string(&second).expect("second"), "second");
        assert!(
            fs::symlink_metadata(&third)
                .expect("third")
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read_to_string(&fourth).expect("fourth"), original);
        let mode = fs::metadata(&fourth)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o644);
        let leftovers: Vec<_> = fs::read_dir(fixture.config.parent().expect("parent"))
            .expect("list")
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().starts_with('.'))
            .collect();
        assert!(
            leftovers.is_empty(),
            "staged files left behind: {leftovers:?}"
        );

        // A dry run names the first free slot without taking it.
        fs::write(&fixture.config, &original).expect("restale");
        let lines = fixture.run(true, false).expect("dry run");
        let fifth = fixture.config.with_file_name("xorg.conf.before-0.16.1.4");
        assert_eq!(
            lines[1],
            format!(
                "dry-run: would keep the previous file as {}",
                fifth.display()
            )
        );
        assert!(!fifth.exists());
    }

    fn assert_refused(fixture: &XorgFixture, owner: u32, reason: &str) {
        let before = fs::symlink_metadata(&fixture.config).expect("metadata");
        let result = XorgGpuStep {
            config: &fixture.config,
            sysfs_root: fixture.sysfs.path(),
            dry_run: false,
            freshly_written: false,
            backup_suffix: "0.16.1",
            owner,
        }
        .run();
        let error = result.expect_err("an untrusted template must be refused");
        assert!(error.contains("is not safe to repair"), "{error}");
        assert!(error.contains(reason), "{error}");
        let after = fs::symlink_metadata(&fixture.config).expect("metadata");
        assert_eq!(before.modified().ok(), after.modified().ok());
        assert_eq!(before.permissions(), after.permissions());
        assert!(!fixture.backup().exists());
    }

    #[test]
    fn a_template_others_can_write_is_not_repaired() {
        let original = lab_template();
        let fixture = XorgFixture::new("xorg-writable", Some(&original), &[NVIDIA_VGPU]);
        chmod(&fixture.config, 0o664).expect("chmod");
        assert_refused(&fixture, fixture.owner, "mode 0664");
        assert_eq!(fixture.read(), original);
        chmod(&fixture.config, 0o646).expect("chmod");
        assert_refused(&fixture, fixture.owner, "mode 0646");
        assert_eq!(fixture.read(), original);
    }

    #[test]
    fn a_template_owned_by_someone_else_is_not_repaired() {
        let original = lab_template();
        let fixture = XorgFixture::new("xorg-owner", Some(&original), &[NVIDIA_VGPU]);
        let stranger = fixture.owner.wrapping_add(1);
        assert_refused(
            &fixture,
            stranger,
            &format!("owned by uid {}", fixture.owner),
        );
        assert_eq!(fixture.read(), original);
    }

    #[test]
    fn a_symlinked_template_is_not_followed() {
        let original = lab_template();
        let fixture = XorgFixture::new("xorg-symlink", None, &[NVIDIA_VGPU]);
        let real = fixture.root.join("attacker.conf");
        fs::write(&real, &original).expect("target");
        chmod(&real, 0o644).expect("chmod");
        std::os::unix::fs::symlink(&real, &fixture.config).expect("symlink");
        assert_refused(&fixture, fixture.owner, "symbolic link");
        assert_eq!(fs::read_to_string(&real).expect("target"), original);
        assert!(
            fs::symlink_metadata(&fixture.config)
                .expect("link")
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn a_template_that_is_not_a_file_is_not_repaired() {
        let fixture = XorgFixture::new("xorg-dir", None, &[NVIDIA_VGPU]);
        fs::create_dir(&fixture.config).expect("directory");
        chmod(&fixture.config, 0o755).expect("chmod");
        assert_refused(&fixture, fixture.owner, "not a regular file");
    }
}
