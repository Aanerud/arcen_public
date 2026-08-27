use std::env;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

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

fn install(options: &Options) -> Result<(), String> {
    preflight(options)?;
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
    let config_path = map_path(&options.prefix, "/etc/arcen/pier.json");
    let fresh_config = !config_path.exists();
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
    if fresh_config || options.force {
        println!(
            "multi-monitor safe-auto: disabled; the installer cannot prove a Linux NVIDIA head \
             roster without starting an X/NV-CONTROL session. Configure \
             platform.multi_monitor.heads after validating the host, or leave it disabled."
        );
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
    open_firewall(options);
    enable_and_start(options)?;
    report_pending_restart(options);
    if is_root_prefix(&options.prefix) && !options.dry_run {
        println!();
        println!("Administration guide: /usr/share/doc/arcen/pier-administration.md");
    }
    Ok(())
}

/// Register the unit and bring it up.
fn enable_and_start(options: &Options) -> Result<(), String> {
    if options.dry_run {
        println!("dry-run: would enable and start arcen-pier.service");
        return Ok(());
    }
    if !is_root_prefix(&options.prefix) {
        println!("staging prefix: skipped enabling the service");
        return Ok(());
    }
    if options.no_service {
        println!("--no-service: installed without enabling the unit");
        return Ok(());
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
    } else {
        println!("service: enabled but not running (state: {active})");
    }
    Ok(())
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
    format!("subjectAltName={}", entries.join(","))
}

fn ensure_cert(options: &Options) -> Result<(), String> {
    let cert = map_path(&options.prefix, "/etc/arcen/host.crt");
    let key = map_path(&options.prefix, "/etc/arcen/host.key");
    if cert.exists() && key.exists() && !options.force {
        // Say what to do next, not just what happened. "kept existing" reads as
        // success to an operator who has just added --extra-san to change the
        // names the certificate covers, and the Deck then goes on rejecting the
        // same certificate with no clue that the option was silently ignored.
        println!(
            "keeping existing TLS certificate and key in {} (pass --force to replace them, \
             for example after adding --extra-san)",
            cert.parent().unwrap_or(&cert).display()
        );
        return Ok(());
    }
    if options.dry_run {
        println!(
            "dry-run: would generate TLS certificate {} and {}",
            cert.display(),
            key.display()
        );
        return Ok(());
    }
    if cert.exists() || key.exists() {
        println!(
            "--force: replacing the TLS certificate in {}",
            cert.parent().unwrap_or(&cert).display()
        );
    }
    let staged_key = key.with_file_name(format!(".host.key.installing.{}", std::process::id()));
    let staged_cert = cert.with_file_name(format!(".host.crt.installing.{}", std::process::id()));
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
        return Err("openssl key generation failed".to_string());
    }
    chmod(&staged_key, 0o600)?;
    let status = Command::new("openssl")
        .args(["req", "-x509", "-new", "-sha256", "-days", "825"])
        .args(["-key"])
        .arg(&staged_key)
        .args(["-out"])
        .arg(&staged_cert)
        .args(["-subj", "/CN=Arcen Pier"])
        .args(["-addext", "basicConstraints=critical,CA:FALSE"])
        .args(["-addext", "keyUsage=critical,digitalSignature"])
        .args(["-addext", "extendedKeyUsage=serverAuth"])
        .args(["-addext", &subject_alt_name(&options.extra_sans)])
        .status()
        .map_err(|error| format!("start openssl certificate generation: {error}"))?;
    if !status.success() {
        let _ = fs::remove_file(&staged_key);
        let _ = fs::remove_file(&staged_cert);
        return Err("openssl certificate generation failed".to_string());
    }
    fs::rename(&staged_key, &key).map_err(|error| format!("install {}: {error}", key.display()))?;
    fs::rename(&staged_cert, &cert)
        .map_err(|error| format!("install {}: {error}", cert.display()))?;
    chmod(&key, 0o600)?;
    chmod(&cert, 0o600)?;
    println!("generated TLS certificate and key");
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
/// hand-tuned host to whatever the safe-auto defaults happen to select.
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

    #[test]
    fn purge_preserves_a_copy_of_the_configuration() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let prefix = std::env::temp_dir().join(format!(
            "arcen-linux-installer-purge-{}-{unique}",
            std::process::id()
        ));
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
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let prefix = std::env::temp_dir().join(format!(
            "arcen-linux-installer-purge-empty-{}-{unique}",
            std::process::id()
        ));
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
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let prefix = std::env::temp_dir().join(format!(
            "arcen-linux-installer-migration-{}-{unique}",
            std::process::id()
        ));
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
