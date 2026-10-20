//! Which autostart entries an Arcen session runs.
//!
//! Every verdict carries a reason token used in logs and documentation.
//! The table is by desktop-file id (and, for the keyring, by the program it
//! runs), so a distribution that renames nothing gets the same decisions on
//! every GNOME release.

use std::collections::BTreeSet;
use std::path::Path;

use super::autostart::AutostartEntry;

/// What an Arcen session does with an autostart entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComponentVerdict {
    /// Start it as `gnome-session` would.
    Run,
    /// Arcen starts this service itself, earlier and differently; the entry
    /// is not started a second time.
    ProvidedByArcen(ProvidedBy),
    /// Start it once the session has an X11 display (Xwayland): the X11
    /// settings bridge has nothing to serve before.
    AfterX11Display,
    /// Never in a remote session: it acts on the host, not on the remote
    /// user's desktop.
    HostScoped(HostScopedReason),
}

impl ComponentVerdict {
    /// Stable token for logs.
    #[must_use]
    pub const fn token(&self) -> &'static str {
        match self {
            Self::Run => "run",
            Self::ProvidedByArcen(ProvidedBy::Keyring) => "provided_by_arcen:keyring",
            Self::ProvidedByArcen(ProvidedBy::AccessibilityBus) => {
                "provided_by_arcen:accessibility_bus"
            }
            Self::AfterX11Display => "after_x11_display",
            Self::HostScoped(reason) => reason.token(),
        }
    }
}

/// A service Arcen starts itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProvidedBy {
    /// `gnome-keyring-daemon`, started unlocked with the union of the
    /// enabled keyring components.
    Keyring,
    /// The AT-SPI accessibility bus, started before the compositor.
    AccessibilityBus,
}

/// Why an entry never runs in a remote session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostScopedReason {
    /// Automatic time zone changes the host's zone; Arcen redirects the
    /// zone per session instead.
    HostTimeZone,
    /// Airplane mode switches the host's radios off, the Deck's connection
    /// with them.
    HostRadios,
    /// Starts other remote-access and sharing services on the host.
    HostSharing,
    /// Changes the host-wide `USBGuard` policy.
    HostUsbPolicy,
    /// Unlocks the host's mobile-broadband modem.
    HostModem,
    /// A virtual-machine console agent: it belongs to the console seat.
    VmConsoleAgent,
}

impl HostScopedReason {
    /// Stable token for logs.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::HostTimeZone => "host_scoped:time_zone",
            Self::HostRadios => "host_scoped:radios",
            Self::HostSharing => "host_scoped:sharing",
            Self::HostUsbPolicy => "host_scoped:usb_policy",
            Self::HostModem => "host_scoped:modem",
            Self::VmConsoleAgent => "host_scoped:vm_console_agent",
        }
    }
}

/// Keyring components Arcen may start.
pub const KEYRING_COMPONENTS: [&str; 4] = ["pkcs11", "secrets", "ssh", "gpg"];

const KEYRING_DAEMON: &str = "gnome-keyring-daemon";

/// The verdict for `entry`.
#[must_use]
pub fn classify(entry: &AutostartEntry) -> ComponentVerdict {
    if is_keyring(entry) {
        return ComponentVerdict::ProvidedByArcen(ProvidedBy::Keyring);
    }
    match entry.id.as_str() {
        "at-spi-dbus-bus" => ComponentVerdict::ProvidedByArcen(ProvidedBy::AccessibilityBus),
        "org.gnome.SettingsDaemon.XSettings" => ComponentVerdict::AfterX11Display,
        "org.gnome.SettingsDaemon.Datetime" => {
            ComponentVerdict::HostScoped(HostScopedReason::HostTimeZone)
        }
        "org.gnome.SettingsDaemon.Rfkill" => {
            ComponentVerdict::HostScoped(HostScopedReason::HostRadios)
        }
        "org.gnome.SettingsDaemon.Sharing" => {
            ComponentVerdict::HostScoped(HostScopedReason::HostSharing)
        }
        "org.gnome.SettingsDaemon.UsbProtection" => {
            ComponentVerdict::HostScoped(HostScopedReason::HostUsbPolicy)
        }
        "org.gnome.SettingsDaemon.Wwan" => {
            ComponentVerdict::HostScoped(HostScopedReason::HostModem)
        }
        "spice-vdagent" | "vmware-user" | "vboxclient" | "VBoxClient" | "vboxclient-all" => {
            ComponentVerdict::HostScoped(HostScopedReason::VmConsoleAgent)
        }
        _ => ComponentVerdict::Run,
    }
}

/// The keyring components the enabled `gnome-keyring-daemon` entries name
/// (`--components=a,b`), restricted to [`KEYRING_COMPONENTS`]. Empty when
/// no keyring entry is enabled: the session then starts no keyring.
#[must_use]
pub fn keyring_components<'a>(
    enabled: impl IntoIterator<Item = &'a AutostartEntry>,
) -> Vec<String> {
    let mut components = BTreeSet::new();
    for entry in enabled.into_iter().filter(|entry| is_keyring(entry)) {
        for argument in &entry.argv[1..] {
            if let Some(list) = argument.strip_prefix("--components=") {
                for component in list.split(',').map(str::trim) {
                    if KEYRING_COMPONENTS.contains(&component) {
                        components.insert(component.to_owned());
                    }
                }
            }
        }
    }
    components.into_iter().collect()
}

fn is_keyring(entry: &AutostartEntry) -> bool {
    entry
        .argv
        .first()
        .and_then(|program| Path::new(program).file_name())
        .is_some_and(|name| name == KEYRING_DAEMON)
}

#[cfg(test)]
mod tests {
    use super::super::autostart::{Phase, parse_entry};
    use super::*;

    fn entry(id: &str, exec: &str) -> AutostartEntry {
        parse_entry(
            &format!("{id}.desktop"),
            format!("[Desktop Entry]\nType=Application\nExec={exec}\n").as_bytes(),
        )
        .expect("parses")
    }

    /// The autostart ids of a Rocky Linux 10.2 GNOME 49 host (names only).
    const ROCKY10_IDS: [&str; 34] = [
        "at-spi-dbus-bus",
        "geoclue-demo-agent",
        "gnome-initial-setup-copy-worker",
        "gnome-initial-setup-first-login",
        "gnome-keyring-pkcs11",
        "gnome-keyring-secrets",
        "gnome-keyring-ssh",
        "orca-autostart",
        "org.gnome.Evolution-alarm-notify",
        "org.gnome.SettingsDaemon.A11ySettings",
        "org.gnome.SettingsDaemon.Color",
        "org.gnome.SettingsDaemon.Datetime",
        "org.gnome.SettingsDaemon.DiskUtilityNotify",
        "org.gnome.SettingsDaemon.Housekeeping",
        "org.gnome.SettingsDaemon.Keyboard",
        "org.gnome.SettingsDaemon.MediaKeys",
        "org.gnome.SettingsDaemon.Power",
        "org.gnome.SettingsDaemon.PrintNotifications",
        "org.gnome.SettingsDaemon.Rfkill",
        "org.gnome.SettingsDaemon.ScreensaverProxy",
        "org.gnome.SettingsDaemon.Sharing",
        "org.gnome.SettingsDaemon.Smartcard",
        "org.gnome.SettingsDaemon.Sound",
        "org.gnome.SettingsDaemon.UsbProtection",
        "org.gnome.SettingsDaemon.Wacom",
        "org.gnome.SettingsDaemon.Wwan",
        "org.gnome.SettingsDaemon.XSettings",
        "org.gnome.Software",
        "sealertauto",
        "spice-vdagent",
        "tracker-miner-fs-3",
        "user-dirs-update-gtk",
        "vmware-user",
        "xdg-user-dirs",
    ];

    #[test]
    fn every_rocky10_entry_has_a_deliberate_verdict() {
        let verdicts = ROCKY10_IDS
            .iter()
            .map(|id| {
                let exec = if id.starts_with("gnome-keyring-") {
                    format!(
                        "/usr/bin/gnome-keyring-daemon --start --components={}",
                        id.trim_start_matches("gnome-keyring-")
                    )
                } else {
                    "/usr/bin/true".to_owned()
                };
                (*id, classify(&entry(id, &exec)).token())
            })
            .collect::<Vec<_>>();
        let not_run = verdicts
            .iter()
            .filter(|(_, token)| *token != "run")
            .copied()
            .collect::<Vec<_>>();
        assert_eq!(
            not_run,
            [
                ("at-spi-dbus-bus", "provided_by_arcen:accessibility_bus"),
                ("gnome-keyring-pkcs11", "provided_by_arcen:keyring"),
                ("gnome-keyring-secrets", "provided_by_arcen:keyring"),
                ("gnome-keyring-ssh", "provided_by_arcen:keyring"),
                ("org.gnome.SettingsDaemon.Datetime", "host_scoped:time_zone"),
                ("org.gnome.SettingsDaemon.Rfkill", "host_scoped:radios"),
                ("org.gnome.SettingsDaemon.Sharing", "host_scoped:sharing"),
                (
                    "org.gnome.SettingsDaemon.UsbProtection",
                    "host_scoped:usb_policy"
                ),
                ("org.gnome.SettingsDaemon.Wwan", "host_scoped:modem"),
                ("org.gnome.SettingsDaemon.XSettings", "after_x11_display"),
                ("spice-vdagent", "host_scoped:vm_console_agent"),
                ("vmware-user", "host_scoped:vm_console_agent"),
            ]
        );
    }

    #[test]
    fn the_keyring_runs_the_union_of_its_enabled_components() {
        let entries = [
            entry(
                "gnome-keyring-secrets",
                "/usr/bin/gnome-keyring-daemon --start --components=secrets",
            ),
            entry(
                "gnome-keyring-ssh",
                "/usr/bin/gnome-keyring-daemon --start --components=ssh,bogus",
            ),
            entry("other", "/usr/bin/true --components=pkcs11"),
        ];
        assert_eq!(keyring_components(&entries), ["secrets", "ssh"]);
        assert!(keyring_components(&entries[2..]).is_empty());
        assert_eq!(entries[0].phase, Phase::Application);
    }
}
