// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Native tablet: the administrator's opt-in (`usb.native_tablet` in
//! `pier.json`, set by `--native-tablet` / `--no-native-tablet`) and its
//! virtual USB host controller, the distribution's in-tree USB/IP
//! `vhci-hcd`.
//!
//! Nothing here happens unless Native tablet is enabled. Rocky Linux and
//! RHEL build `vhci-hcd` but ship it only in `kernel-modules-internal` (the
//! `devel` repository; Red Hat labels the package internal and
//! unsupported), one package per kernel release. With Native tablet enabled,
//! on a Rocky/RHEL-family host the installer installs that package for the
//! exact running kernel, with GPG checking on, verifies that the module is
//! signed by the same key as the kernel's own modules, and records that it
//! did so: `--uninstall` and `--no-native-tablet` remove only a package this
//! installer installed. Nothing is loaded at install time; an enabled Pier
//! runs `modprobe vhci-hcd` once, when it starts (a Pier already running
//! needs a restart). Other distributions are told where the module would
//! come from, and Native tablet stays unavailable until it exists. Secure
//! Boot and the machine owner key list are never touched (ADR 0020).

use std::path::Path;
use std::process::{Command, Stdio};

use crate::{Options, is_root_prefix, map_path};

#[path = "../../../../hosts/linux/src/usb_bridge/kmod.rs"]
#[allow(dead_code)]
mod kmod;

/// Records each `kernel-modules-internal` package this installer installed,
/// one NEVRA per line, so uninstall removes exactly those.
pub const MARKER: &str = "/var/lib/arcen/installer-installed-kernel-modules-internal";
const MAX_MARKER_BYTES: usize = 16 * 1024;
const RPM: &str = "/usr/bin/rpm";
const DNF: &str = "/usr/bin/dnf";
const MODINFO: &str = "/usr/sbin/modinfo";
/// Modules of the kernel's own packages whose signer the bridged controller's
/// must match. The first one present is used.
const REFERENCE_MODULES: &[&str] = &["xfs", "ext4", "usb-storage", "hid-multitouch", "uinput"];

/// The operator's Native tablet choice on the command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    /// `--native-tablet`: set `usb.native_tablet = "enabled"`.
    Enable,
    /// `--no-native-tablet`: set `usb.native_tablet = "disabled"` and
    /// remove the controller package this installer installed.
    Disable,
}

impl Choice {
    const fn token(self) -> &'static str {
        match self {
            Self::Enable => "enabled",
            Self::Disable => "disabled",
        }
    }
}

const CONFIG_PATH: &str = "/etc/arcen/pier.json";

/// `usb.native_tablet` of a configuration document, when it is a string.
#[must_use]
pub fn configured(text: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(text)
        .ok()?
        .pointer("/usb/native_tablet")?
        .as_str()
        .map(str::to_owned)
}

/// `text` with `usb.native_tablet` set to `choice`; `None` when it already
/// is. A document without a `usb` section gets one inserted at the top, so
/// the operator's layout and key order stay as written; otherwise the
/// document is re-serialized. Either way the result is parsed back and must
/// equal the original plus this one value.
///
/// # Errors
///
/// When `text` is not a JSON object.
pub fn with_choice(text: &str, choice: Choice) -> Result<Option<String>, String> {
    let original: serde_json::Value =
        serde_json::from_str(text).map_err(|error| format!("{CONFIG_PATH}: {error}"))?;
    let serde_json::Value::Object(map) = &original else {
        return Err(format!("{CONFIG_PATH}: not a JSON object"));
    };
    if configured(text).as_deref() == Some(choice.token()) {
        return Ok(None);
    }
    let mut expected = original.clone();
    if let Some(object) = expected.as_object_mut() {
        let usb = object
            .entry("usb")
            .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
        if !usb.is_object() {
            *usb = serde_json::Value::Object(serde_json::Map::new());
        }
        if let Some(usb) = usb.as_object_mut() {
            usb.insert(
                "native_tablet".to_owned(),
                serde_json::Value::String(choice.token().to_owned()),
            );
        }
    }
    let inserted = (!map.contains_key("usb"))
        .then(|| {
            let open = text.find('{')?;
            text[..open].trim().is_empty().then(|| {
                let rest = &text[open + 1..];
                let separator = if map.is_empty() { "" } else { "," };
                format!(
                    "{{\n  \"usb\": {{\n    \"native_tablet\": \"{}\"\n  }}{separator}{rest}",
                    choice.token()
                )
            })
        })
        .flatten();
    // An existing value is replaced in place when the key appears once.
    let replaced = || {
        let old = configured(text)?;
        let needle = "\"native_tablet\"";
        (text.matches(needle).count() == 1).then_some(())?;
        let key = text.find(needle)?;
        let value_at = key + text[key..].find(&format!("\"{old}\""))?;
        text[key + needle.len()..value_at].trim().eq(":").then(|| {
            format!(
                "{}\"{}\"{}",
                &text[..value_at],
                choice.token(),
                &text[value_at + old.len() + 2..]
            )
        })
    };
    let candidate = match inserted.or_else(replaced) {
        Some(candidate) => candidate,
        None => {
            let mut pretty = serde_json::to_string_pretty(&expected)
                .map_err(|error| format!("{CONFIG_PATH}: {error}"))?;
            pretty.push('\n');
            pretty
        }
    };
    let reparsed: serde_json::Value = serde_json::from_str(&candidate)
        .map_err(|error| format!("{CONFIG_PATH}: edited document does not parse: {error}"))?;
    if reparsed != expected {
        return Err(format!(
            "{CONFIG_PATH}: setting usb.native_tablet changed more than that key"
        ));
    }
    Ok(Some(candidate))
}

/// Applies `--native-tablet` / `--no-native-tablet` to the installed
/// `pier.json` and returns whether Native tablet is enabled afterwards.
///
/// # Errors
///
/// When the configuration cannot be read, edited or written.
pub fn apply_choice(
    options: &Options,
    choice: Option<Choice>,
    log: &mut crate::desktop::InstallLog,
) -> Result<bool, String> {
    let path = map_path(&options.prefix, CONFIG_PATH);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(_) if options.dry_run => crate::CONFIG_TEMPLATE.to_owned(),
        Err(error) => return Err(format!("read {}: {error}", path.display())),
    };
    let Some(choice) = choice else {
        let enabled = configured(&text).as_deref() == Some("enabled");
        if !enabled {
            log.note(
                "Native tablet: not enabled (usb.native_tablet in /etc/arcen/pier.json); \
                 nothing installed for it. Enable it with --native-tablet."
                    .to_owned(),
            );
        }
        return Ok(enabled);
    };
    if let Some(edited) = with_choice(&text, choice)? {
        crate::write_atomic(options, CONFIG_PATH, edited.as_bytes(), 0o644, true)?;
    }
    log.note(match choice {
        Choice::Enable => "Native tablet: enabled (usb.native_tablet = \"enabled\"). A Deck's \
             physical pen tablet is attached to the remote desktop as a virtual USB device \
             through the kernel's USB/IP controller vhci-hcd; the device is classified and \
             admitted only as a pen tablet, and only into the signed-in user's own desktop \
             (ADR 0021). Restart the Pier for it to take effect."
            .to_owned(),
        Choice::Disable => "Native tablet: disabled (usb.native_tablet = \"disabled\"); Decks \
             are offered Tablet support instead. Restart the Pier for it to take effect."
            .to_owned(),
    });
    if choice == Choice::Disable {
        remove(options);
    }
    Ok(choice == Choice::Enable)
}

/// What the installer does about `vhci-hcd`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// The module is loaded or installed for the running kernel.
    Available,
    /// Rocky/RHEL family without the module: install the package.
    Install { kernel: String },
    /// Opted out with `--no-native-tablet-modules`.
    OptedOut { reason: String },
    /// Not Rocky/RHEL, and the module is missing: explain.
    Unavailable { reason: String },
}

/// Decides from the facts and the operator's choice.
#[must_use]
pub fn plan(facts: &arcen_usb_bridge::VhciHcdFacts, opted_out: bool) -> Plan {
    if facts.loaded || facts.module_for_running_kernel {
        return Plan::Available;
    }
    let reason = arcen_usb_bridge::missing_vhci_hcd_reason(facts);
    if !facts.rhel_family || !arcen_usb_bridge::valid_kernel_release(&facts.running_kernel) {
        return Plan::Unavailable { reason };
    }
    if opted_out {
        return Plan::OptedOut { reason };
    }
    Plan::Install {
        kernel: facts.running_kernel.clone(),
    }
}

/// Whether `dnf repolist --all` lists a repository with id `devel`.
#[must_use]
pub fn lists_devel_repository(repolist: &str) -> bool {
    repolist
        .lines()
        .any(|line| line.split_whitespace().next() == Some("devel"))
}

/// The `dnf` invocations to try, in order: the package from the configured
/// repositories (with `devel` enabled when it exists), then — for a Rocky
/// kernel whose point release has moved on — the exact signed package from
/// Rocky's own `devel` tree or vault. GPG checking is on for every one.
#[must_use]
pub fn install_attempts(kernel: &str, devel_repository: bool) -> Vec<Vec<String>> {
    let package = arcen_usb_bridge::kernel_modules_internal_package(kernel);
    let mut first = vec![
        "-y".to_owned(),
        "--setopt=gpgcheck=1".to_owned(),
        "--setopt=localpkg_gpgcheck=1".to_owned(),
    ];
    if devel_repository {
        first.push("--enablerepo=devel".to_owned());
    }
    first.push("install".to_owned());
    first.push(package);
    let mut attempts = vec![first];
    for url in arcen_usb_bridge::rocky_package_urls(kernel) {
        attempts.push(vec![
            "-y".to_owned(),
            "--setopt=gpgcheck=1".to_owned(),
            "--setopt=localpkg_gpgcheck=1".to_owned(),
            "install".to_owned(),
            url,
        ]);
    }
    attempts
}

/// Whether the controller's signer is the kernel's own signing key.
#[must_use]
pub fn signer_matches(vhci_signer: &str, reference_signer: &str) -> bool {
    let vhci = vhci_signer.trim();
    !vhci.is_empty() && vhci == reference_signer.trim()
}

/// Packages named in the marker (valid `kernel-modules-internal-*` only).
#[must_use]
pub fn parse_marker(text: &str) -> Vec<String> {
    let mut packages: Vec<String> = text
        .lines()
        .map(str::trim)
        .filter(|line| {
            line.strip_prefix("kernel-modules-internal-")
                .is_some_and(arcen_usb_bridge::valid_kernel_release)
        })
        .map(str::to_owned)
        .collect();
    packages.sort();
    packages.dedup();
    packages
}

/// The marker after recording `package`.
#[must_use]
pub fn render_marker(existing: &str, package: &str) -> String {
    let mut packages = parse_marker(existing);
    packages.push(package.to_owned());
    packages.sort();
    packages.dedup();
    let mut text = packages.join("\n");
    text.push('\n');
    text
}

fn quiet(program: &str, args: &[&str]) -> Option<String> {
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn package_installed(package: &str) -> bool {
    quiet(RPM, &["-q", package]).is_some()
}

fn signer(kernel: &str, module: &str) -> Option<String> {
    quiet(MODINFO, &["-k", kernel, "-F", "signer", module]).filter(|value| !value.is_empty())
}

fn reference_signer(kernel: &str) -> Option<(String, &'static str)> {
    REFERENCE_MODULES
        .iter()
        .find_map(|module| signer(kernel, module).map(|value| (value, *module)))
}

fn read_marker(options: &Options) -> String {
    let path = map_path(&options.prefix, MARKER);
    std::fs::read(&path)
        .ok()
        .filter(|bytes| bytes.len() <= MAX_MARKER_BYTES)
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default()
}

/// What `--check` says about Native tablet and its controller.
pub fn describe(choice: Option<Choice>, opted_out: bool) {
    let configured = std::fs::read_to_string(CONFIG_PATH)
        .ok()
        .and_then(|text| configured(&text));
    let enabled = choice.map_or(configured.as_deref() == Some("enabled"), |choice| {
        choice == Choice::Enable
    });
    if !enabled {
        println!(
            "Native tablet: not enabled (usb.native_tablet is {}); the installer installs \
             nothing for it. --native-tablet enables it.",
            configured.map_or_else(|| "absent".to_owned(), |value| format!("{value:?}"))
        );
        return;
    }
    let facts = kmod::vhci_hcd_facts();
    match plan(&facts, opted_out) {
        Plan::Available => println!(
            "Native tablet: the in-tree USB/IP controller vhci-hcd is available for kernel {}",
            facts.running_kernel
        ),
        Plan::Install { kernel } => println!(
            "Native tablet: the installer would install {} (Rocky/RHEL devel repository, GPG \
             checked) for the in-tree USB/IP controller vhci-hcd",
            arcen_usb_bridge::kernel_modules_internal_package(&kernel)
        ),
        Plan::OptedOut { reason } | Plan::Unavailable { reason } => {
            println!("Native tablet: unavailable: {reason}");
        }
    }
}

/// Install step: makes `vhci-hcd` available for the running kernel where the
/// distribution ships it, and explains the outcome. Never fails the install.
pub fn ensure(options: &Options, opted_out: bool, log: &mut crate::desktop::InstallLog) {
    let facts = kmod::vhci_hcd_facts();
    let kernel = match plan(&facts, opted_out) {
        Plan::Available => {
            log.note(format!(
                "Native tablet: vhci-hcd (in-tree USB/IP virtual host controller) is available \
                 for kernel {}; the Pier loads it when it starts (restart a running Pier)",
                facts.running_kernel
            ));
            return;
        }
        Plan::OptedOut { reason } => {
            log.note(format!(
                "Native tablet: --no-native-tablet-modules: nothing installed ({reason})"
            ));
            return;
        }
        Plan::Unavailable { reason } => {
            log.note(format!("Native tablet: unavailable on this host: {reason}"));
            return;
        }
        Plan::Install { kernel } => kernel,
    };
    let package = arcen_usb_bridge::kernel_modules_internal_package(&kernel);
    if options.dry_run || !is_root_prefix(&options.prefix) {
        println!(
            "{}: would install {package} for Native tablet (vhci-hcd)",
            if options.dry_run {
                "dry-run"
            } else {
                "staging prefix"
            }
        );
        return;
    }
    log.note(format!(
        "Native tablet: installing {package}. Rocky Linux/RHEL ship the in-tree USB/IP virtual \
         host controller (vhci-hcd) only in this package, from the distribution's devel \
         repository; Red Hat labels it internal and unsupported. It is signed by the \
         distribution, so Secure Boot is unaffected. Opt out with --no-native-tablet-modules."
    ));
    let devel = quiet(DNF, &["-q", "repolist", "--all"])
        .is_some_and(|listing| lists_devel_repository(&listing));
    let installed = install_attempts(&kernel, devel).iter().any(|attempt| {
        Command::new(DNF)
            .args(attempt)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
            && package_installed(&package)
    });
    if !installed {
        log.note(format!(
            "Native tablet: {package} could not be installed (no repository or vault carries \
             it for this kernel, or its signature did not verify); Native tablet stays \
             unavailable. Tablet support is unaffected."
        ));
        return;
    }
    let vhci = signer(&kernel, "vhci-hcd").unwrap_or_default();
    match reference_signer(&kernel) {
        Some((reference, module)) if signer_matches(&vhci, &reference) => {
            record(options, &package, log);
            log.note(format!(
                "Native tablet: {package} installed; vhci-hcd is signed by \"{vhci}\", the key \
                 that signs this kernel's own modules ({module}). Nothing is loaded now. After \
                 a kernel update install the matching package \
                 (`{}`); the Pier and `arcen-pier validate-config` name the exact command.",
                arcen_usb_bridge::kernel_modules_internal_command("$(uname -r)")
            ));
        }
        reference => {
            log.note(format!(
                "Native tablet: vhci-hcd's signer \"{vhci}\" does not match this kernel's own \
                 ({}); removing {package} again",
                reference.map_or_else(|| "unknown".to_owned(), |(value, _)| value)
            ));
            let _ = Command::new(DNF)
                .args(["-y", "remove", &package])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .status();
        }
    }
}

fn record(options: &Options, package: &str, log: &mut crate::desktop::InstallLog) {
    let path = map_path(&options.prefix, MARKER);
    let text = render_marker(&read_marker(options), package);
    let written = path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| std::fs::write(&path, text));
    if let Err(error) = written {
        log.note(format!(
            "warning: could not record that the installer installed {package} ({error}); \
             --uninstall will leave it installed"
        ));
    }
}

/// Uninstall step: removes the packages this installer recorded (and only
/// those), then the record. `--purge` removes nothing more.
pub fn remove(options: &Options) {
    let marker = map_path(&options.prefix, MARKER);
    let packages = parse_marker(&read_marker(options));
    if packages.is_empty() {
        return;
    }
    if options.dry_run || !is_root_prefix(&options.prefix) {
        println!(
            "dry-run: would remove {} (installed by this installer for Native tablet)",
            packages.join(", ")
        );
        return;
    }
    if Path::new(kmod::VHCI_HCD_SYSFS).exists() {
        let _ = Command::new("modprobe")
            .args(["-r", "vhci-hcd"])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let present: Vec<&str> = packages
        .iter()
        .map(String::as_str)
        .filter(|package| package_installed(package))
        .collect();
    let removed = present.is_empty()
        || Command::new(DNF)
            .args(["-y", "remove"])
            .args(&present)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
    if removed {
        if !present.is_empty() {
            println!(
                "removed {} (installed by this installer for Native tablet)",
                present.join(", ")
            );
        }
        let _ = std::fs::remove_file(&marker);
    } else {
        println!(
            "warning: could not remove {}; remove it with `dnf remove` (the record {} is kept)",
            present.join(", "),
            marker.display()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(rhel: bool, running: bool, loaded: bool) -> arcen_usb_bridge::VhciHcdFacts {
        arcen_usb_bridge::VhciHcdFacts {
            running_kernel: "5.14.0-611.55.1.el9_7.x86_64".to_owned(),
            loaded,
            module_for_running_kernel: running,
            module_for_other_kernels: Vec::new(),
            rhel_family: rhel,
        }
    }

    #[test]
    fn the_choice_is_written_without_disturbing_the_rest_of_the_configuration() {
        let template = include_str!("../../arcen-pier.json");
        assert_eq!(configured(template).as_deref(), Some("disabled"));
        let enabled = with_choice(template, Choice::Enable)
            .expect("edit")
            .expect("changed");
        assert_eq!(configured(&enabled).as_deref(), Some("enabled"));
        // Only the value changed: same lines, one of them different.
        let differing = template
            .lines()
            .zip(enabled.lines())
            .filter(|(before, after)| before != after)
            .count();
        assert_eq!(differing, 1);
        assert_eq!(template.lines().count(), enabled.lines().count());
        assert_eq!(with_choice(&enabled, Choice::Enable).expect("edit"), None);

        // An operator's file without a usb section gets one at the top; the
        // rest keeps its order.
        let operator = "{\n  \"listen\": {\"port\": 18444},\n  \"audio\": {}\n}\n";
        let edited = with_choice(operator, Choice::Enable)
            .expect("edit")
            .expect("changed");
        assert!(edited.starts_with("{\n  \"usb\": {\n    \"native_tablet\": \"enabled\"\n  },"));
        assert!(edited.ends_with("\"listen\": {\"port\": 18444},\n  \"audio\": {}\n}\n"));
        let empty = with_choice("{}", Choice::Disable)
            .expect("edit")
            .expect("changed");
        assert_eq!(configured(&empty).as_deref(), Some("disabled"));

        // A usb section without the key is re-serialized, and still exact.
        let other = "{\"usb\": {}, \"b\": 1, \"a\": [1, 2]}";
        let rewritten = with_choice(other, Choice::Enable)
            .expect("edit")
            .expect("changed");
        let value: serde_json::Value = serde_json::from_str(&rewritten).expect("json");
        assert_eq!(value["a"], serde_json::json!([1, 2]));
        assert_eq!(value["usb"]["native_tablet"], "enabled");

        assert!(with_choice("[1]", Choice::Enable).is_err());
        assert!(with_choice("{not json", Choice::Enable).is_err());
    }

    #[test]
    fn rocky_without_the_module_installs_it_unless_opted_out() {
        assert_eq!(
            plan(&facts(true, false, false), false),
            Plan::Install {
                kernel: "5.14.0-611.55.1.el9_7.x86_64".to_owned()
            }
        );
        assert!(matches!(
            plan(&facts(true, false, false), true),
            Plan::OptedOut { .. }
        ));
        assert_eq!(plan(&facts(true, true, false), false), Plan::Available);
        assert_eq!(plan(&facts(false, false, true), false), Plan::Available);
        let Plan::Unavailable { reason } = plan(&facts(false, false, false), false) else {
            panic!("another distribution is told, not changed");
        };
        assert!(reason.contains("kernel-modules-internal"));
    }

    #[test]
    fn install_attempts_keep_gpg_checking_and_fall_back_to_the_vault() {
        let attempts = install_attempts("5.14.0-611.55.1.el9_7.x86_64", true);
        assert_eq!(attempts.len(), 3);
        for attempt in &attempts {
            assert!(attempt.contains(&"--setopt=gpgcheck=1".to_owned()));
            assert!(attempt.contains(&"--setopt=localpkg_gpgcheck=1".to_owned()));
            assert!(!attempt.iter().any(|arg| arg.contains("nogpgcheck")));
        }
        assert!(attempts[0].contains(&"--enablerepo=devel".to_owned()));
        assert_eq!(
            attempts[0].last().map(String::as_str),
            Some("kernel-modules-internal-5.14.0-611.55.1.el9_7.x86_64")
        );
        assert!(attempts[2].last().is_some_and(|url| {
            url.starts_with("https://dl.rockylinux.org/vault/rocky/9.7/devel/x86_64/os/Packages/k/")
        }));
        let without_devel = install_attempts("5.14.0-611.55.1.el9_7.x86_64", false);
        assert!(!without_devel[0].contains(&"--enablerepo=devel".to_owned()));
        assert_eq!(install_attempts("6.8.0-45-generic", true).len(), 1);
    }

    #[test]
    fn the_devel_repository_is_found_by_its_id() {
        let listing = "repo id    repo name    status\n\
                       baseos     Rocky Linux 10 - BaseOS   enabled\n\
                       devel      Rocky Linux 10 - Devel WARNING! FOR BUILDROOT USE ONLY!  disabled\n\
                       devel-source  Rocky Linux 10 - Devel Source  disabled\n";
        assert!(lists_devel_repository(listing));
        assert!(!lists_devel_repository(
            "repo id\nbaseos x enabled\ndevel-source y\n"
        ));
    }

    #[test]
    fn the_signer_must_be_the_kernels_own() {
        assert!(signer_matches(
            "Rocky Linux kernel signing key\n",
            "Rocky Linux kernel signing key"
        ));
        assert!(!signer_matches("", ""));
        assert!(!signer_matches("Lab MOK", "Rocky Linux kernel signing key"));
    }

    #[test]
    fn the_marker_records_only_well_formed_package_names() {
        let text = render_marker("", "kernel-modules-internal-6.12.0-211.66.1.el10_2.x86_64");
        let text = render_marker(
            &text,
            "kernel-modules-internal-6.12.0-211.72.1.el10_2.x86_64",
        );
        let text = render_marker(
            &text,
            "kernel-modules-internal-6.12.0-211.66.1.el10_2.x86_64",
        );
        assert_eq!(parse_marker(&text).len(), 2);
        assert!(parse_marker("kernel\nkernel-modules-internal-6.1;rm -rf /\nbash\n").is_empty());
    }
}
