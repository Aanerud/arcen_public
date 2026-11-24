// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Whether the console has a login screen of its display manager, on a
//! host where another remote-desktop product is the display manager (ADR
//! 0025 §8).
//!
//! Such a product may make itself `display-manager.service` and run GDM
//! without its local login screen: XDMCP on and `ShowLocalGreeter` off, in
//! GDM's runtime settings while the product runs. GDM then never starts its
//! local display factory: no login screen on `seat0` after boot, none after
//! a session ends, and the product's own sign-in is the only one there is.
//! A transient login screen asked of that GDM has no terminal to reach: it
//! waits until the product's X server leaves its terminal and then takes
//! the GPU the product needs. So on such a host Arcen neither signs in
//! through a login screen nor asks GDM for one ([`LoginScreen`]); a
//! sign-in with nobody signed in is refused with the product named
//! ([`super::ConsoleRefusal::SignInOtherProduct`]).
//!
//! The rule follows GDM: the runtime settings (`/run/gdm/custom.conf`)
//! answer for each key they hold, the administrator's (`/etc/gdm/custom.conf`)
//! for the rest, and GDM's defaults for keys neither holds. Which keys
//! count depends on GDM's version ([`gdm_shows_local_login_screen`]):
//!
//! - GDM 49 and earlier: the local login screen runs unless
//!   `xdmcp/Enable` (default false) is on and `xdmcp/ShowLocalGreeter`
//!   (default true) is off (`gdm-manager.c`, `!xdmcp_enabled ||
//!   show_local_greeter`).
//! - GDM 50 removed XDMCP and `ShowLocalGreeter`: the local login screen
//!   always runs, whatever an old `[xdmcp]` group says.
//! - GDM 51 and later: `daemon/ShowLocalGreeter` (default true) alone.
//! - Unknown version: a `ShowLocalGreeter=false` in either group cannot be
//!   proven either way (`None`); otherwise the login screen runs.
//!
//! The Linux Pier reads the files and GDM's version; nothing here touches
//! the host.

use super::DISPLAY_MANAGER_UNITS;

/// GDM's settings group for XDMCP, which held `ShowLocalGreeter` until
/// GDM 49.
const XDMCP_GROUP: &str = "xdmcp";
/// GDM's settings group that holds `ShowLocalGreeter` from GDM 51.
const DAEMON_GROUP: &str = "daemon";
const ENABLE_KEY: &str = "Enable";
const SHOW_LOCAL_GREETER_KEY: &str = "ShowLocalGreeter";
/// The last GDM major version whose rule is XDMCP's.
const LAST_XDMCP_GDM: u32 = 49;
/// The GDM major version without XDMCP or `ShowLocalGreeter`.
const GDM_WITHOUT_SWITCH: u32 = 50;

/// One of GDM's settings files, as the host gave it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsFile<'a> {
    /// The file does not exist: it holds no key.
    Absent,
    /// Its text.
    Read(&'a str),
    /// It exists but could not be read: it may hold any key.
    Unreadable,
}

/// What one settings file says about one key.
enum KeyValue {
    Unset,
    Set(bool),
    Unknown,
}

/// The value of `key` in `[group]` of a `GLib` key file: the last line that
/// sets it wins; `#` starts a comment.
fn key_file_value<'a>(text: &'a str, group: &str, key: &str) -> Option<&'a str> {
    let mut in_group = false;
    let mut value = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            in_group = name == group;
            continue;
        }
        if !in_group {
            continue;
        }
        if let Some((name, found)) = line.split_once('=') {
            if name.trim_end() == key {
                value = Some(found.trim());
            }
        }
    }
    value
}

/// GDM's reading of a boolean setting.
fn parse_bool(value: &str) -> Option<bool> {
    if value.eq_ignore_ascii_case("true") || value == "1" {
        Some(true)
    } else if value.eq_ignore_ascii_case("false") || value == "0" {
        Some(false)
    } else {
        None
    }
}

fn file_value(file: SettingsFile<'_>, group: &str, key: &str) -> KeyValue {
    match file {
        SettingsFile::Absent => KeyValue::Unset,
        SettingsFile::Unreadable => KeyValue::Unknown,
        SettingsFile::Read(text) => match key_file_value(text, group, key) {
            None => KeyValue::Unset,
            Some(value) => parse_bool(value).map_or(KeyValue::Unknown, KeyValue::Set),
        },
    }
}

/// One key from the runtime settings over the administrator's, then
/// GDM's default; `None` when it cannot be known.
fn effective(
    runtime: SettingsFile<'_>,
    custom: SettingsFile<'_>,
    group: &str,
    key: &str,
    default: bool,
) -> Option<bool> {
    for file in [runtime, custom] {
        match file_value(file, group, key) {
            KeyValue::Set(value) => return Some(value),
            KeyValue::Unknown => return None,
            KeyValue::Unset => {}
        }
    }
    Some(default)
}

/// GDM's major version from what `gdm --version` prints (`GDM 40.1`);
/// `None` for anything else.
#[must_use]
pub fn gdm_major_version(version_output: &str) -> Option<u32> {
    let version = version_output.lines().next()?.trim().strip_prefix("GDM ")?;
    let digits = version
        .find(|c: char| !c.is_ascii_digit())
        .map_or(version, |end| &version[..end]);
    digits.parse().ok()
}

/// Whether GDM runs its local login screen (its local display factory),
/// from its major version (`None`: unknown) and its runtime settings over
/// the administrator's; `None` when that cannot be known: a file could not
/// be read, holds a value GDM would not read as a boolean, or switches
/// `ShowLocalGreeter` off for a GDM whose version is unknown.
#[must_use]
pub fn gdm_shows_local_login_screen(
    gdm_major: Option<u32>,
    runtime: SettingsFile<'_>,
    custom: SettingsFile<'_>,
) -> Option<bool> {
    let show_local = |group| effective(runtime, custom, group, SHOW_LOCAL_GREETER_KEY, true);
    match gdm_major {
        Some(major) if major <= LAST_XDMCP_GDM => {
            let xdmcp = effective(runtime, custom, XDMCP_GROUP, ENABLE_KEY, false)?;
            Some(!xdmcp || show_local(XDMCP_GROUP)?)
        }
        Some(GDM_WITHOUT_SWITCH) => Some(true),
        Some(_) => show_local(DAEMON_GROUP),
        // Either GDM's rule could hold: a switch that is off in either
        // group may withhold the login screen, or may not.
        None => {
            let legacy = show_local(XDMCP_GROUP)?;
            let current = show_local(DAEMON_GROUP)?;
            effective(runtime, custom, XDMCP_GROUP, ENABLE_KEY, false)?;
            (legacy && current).then_some(true)
        }
    }
}

/// Whether the console may have a login screen of its display manager.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LoginScreen {
    /// The display manager shows its login screen on the console, or the
    /// host has a display manager of its own (or none at all, ADR 0025
    /// §10): everything as before.
    #[default]
    Shown,
    /// Another product is the running display manager and has GDM's local
    /// login screen switched off: no sign-in goes through a login screen
    /// and none is asked of GDM.
    Withheld,
    /// Another product is the running display manager and GDM's settings
    /// could not be read: a sign-in is decided as before, but no login
    /// screen is asked of GDM (it may be the case above).
    Unproven,
}

impl LoginScreen {
    /// The console's login screen from `display-manager.service`'s target,
    /// whether that alias counts as a running display manager
    /// ([`super::ConsoleHost::from_display_manager`]) and GDM's settings
    /// ([`gdm_shows_local_login_screen`], asked only for another product's
    /// alias). A display manager's own alias is always [`Self::Shown`].
    #[must_use]
    pub fn of(
        alias_target: Option<&str>,
        display_manager: bool,
        gdm_shows_local: impl FnOnce() -> Option<bool>,
    ) -> Self {
        match alias_target {
            Some(unit) if display_manager && !DISPLAY_MANAGER_UNITS.contains(&unit) => {
                match gdm_shows_local() {
                    Some(true) => Self::Shown,
                    Some(false) => Self::Withheld,
                    None => Self::Unproven,
                }
            }
            _ => Self::Shown,
        }
    }

    /// A sign-in with nobody at the console is refused, and a login screen
    /// left on the console is never signed in through.
    #[must_use]
    pub const fn refuses_sign_in(self) -> bool {
        matches!(self, Self::Withheld)
    }

    /// GDM may be asked for a login screen in front of a session that did
    /// not lock (`CreateTransientDisplay`).
    #[must_use]
    pub const fn may_ask_for_login_screen(self) -> bool {
        matches!(self, Self::Shown)
    }

    /// Stable token for logs.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Shown => "shown",
            Self::Withheld => "withheld",
            Self::Unproven => "unproven",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PRODUCT_RUNTIME: &str =
        "[daemon]\nWaylandEnable=false\n[xdmcp]\nEnable=true\nShowLocalGreeter=false\nPort=-1\n";
    const ADMIN: &str = "[daemon]\nWaylandEnable=false\n[security]\n[xdmcp]\n[chooser]\n[debug]\n";

    /// GDM 51's runtime settings with its local login screen switched off.
    const GDM51_RUNTIME: &str = "[daemon]\nWaylandEnable=false\nShowLocalGreeter=false\n";

    /// GDM 40 (RHEL 9 family), whose rule is XDMCP's.
    fn shows(runtime: SettingsFile<'_>, custom: SettingsFile<'_>) -> Option<bool> {
        gdm_shows_local_login_screen(Some(40), runtime, custom)
    }

    #[test]
    fn gdm_reports_its_major_version() {
        assert_eq!(gdm_major_version("GDM 40.1\n"), Some(40));
        assert_eq!(gdm_major_version("GDM 51.0"), Some(51));
        assert_eq!(gdm_major_version("GDM 50.alpha\n"), Some(50));
        assert_eq!(gdm_major_version("GDM 52\n"), Some(52));
        assert_eq!(gdm_major_version("gdm 40.1"), None);
        assert_eq!(gdm_major_version("GDM .1"), None);
        assert_eq!(gdm_major_version(""), None);
    }

    #[test]
    fn each_gdm_version_reads_its_own_switch_for_the_local_login_screen() {
        use SettingsFile::{Absent, Read, Unreadable};
        // GDM 49 is the last with XDMCP's rule; `[daemon]` does not count.
        assert_eq!(
            gdm_shows_local_login_screen(Some(49), Read(PRODUCT_RUNTIME), Absent),
            Some(false)
        );
        assert_eq!(
            gdm_shows_local_login_screen(Some(49), Read(GDM51_RUNTIME), Absent),
            Some(true)
        );
        // GDM 50 has neither XDMCP nor the switch: a stale `[xdmcp]` pair
        // withholds nothing.
        for runtime in [PRODUCT_RUNTIME, GDM51_RUNTIME] {
            assert_eq!(
                gdm_shows_local_login_screen(Some(50), Read(runtime), Unreadable),
                Some(true)
            );
        }
        // GDM 51 and later: `[daemon]` alone, whatever XDMCP says.
        assert_eq!(
            gdm_shows_local_login_screen(Some(51), Read(GDM51_RUNTIME), Read(ADMIN)),
            Some(false)
        );
        assert_eq!(
            gdm_shows_local_login_screen(Some(52), Read(PRODUCT_RUNTIME), Read(ADMIN)),
            Some(true)
        );
        assert_eq!(
            gdm_shows_local_login_screen(
                Some(51),
                Read("[daemon]\nShowLocalGreeter=true\n"),
                Read(GDM51_RUNTIME)
            ),
            Some(true)
        );
        assert_eq!(
            gdm_shows_local_login_screen(Some(51), Absent, Read(GDM51_RUNTIME)),
            Some(false)
        );
        assert_eq!(
            gdm_shows_local_login_screen(Some(51), Absent, Absent),
            Some(true)
        );
        assert_eq!(
            gdm_shows_local_login_screen(Some(51), Unreadable, Absent),
            None
        );
        // An unknown version: the switch off in either group is unproven,
        // never shown or withheld; without it the login screen runs.
        for runtime in [
            PRODUCT_RUNTIME,
            GDM51_RUNTIME,
            "[xdmcp]\nShowLocalGreeter=false\n",
        ] {
            assert_eq!(
                gdm_shows_local_login_screen(None, Read(runtime), Absent),
                None
            );
        }
        assert_eq!(
            gdm_shows_local_login_screen(None, Read("[xdmcp]\nEnable=true\n"), Read(ADMIN)),
            Some(true)
        );
        assert_eq!(
            gdm_shows_local_login_screen(None, Absent, Absent),
            Some(true)
        );
        assert_eq!(gdm_shows_local_login_screen(None, Unreadable, Absent), None);
    }

    #[test]
    fn gdm_shows_its_login_screen_unless_xdmcp_is_on_and_the_local_one_off() {
        use SettingsFile::{Absent, Read, Unreadable};
        // A stock host, and one with only the administrator's file.
        assert_eq!(shows(Absent, Absent), Some(true));
        assert_eq!(
            shows(Read("[daemon]\nWaylandEnable=false\n"), Read(ADMIN)),
            Some(true)
        );
        // A product's runtime settings switch it off.
        assert_eq!(shows(Read(PRODUCT_RUNTIME), Read(ADMIN)), Some(false));
        // XDMCP alone, or the local switch alone, leaves it on.
        assert_eq!(shows(Read("[xdmcp]\nEnable=true\n"), Absent), Some(true));
        assert_eq!(
            shows(Read("[xdmcp]\nShowLocalGreeter=false\n"), Absent),
            Some(true)
        );
        // The runtime file answers first, key by key; the administrator's
        // file for the keys it does not hold.
        assert_eq!(
            shows(
                Read("[xdmcp]\nShowLocalGreeter=false\n"),
                Read("[xdmcp]\nEnable=true\nShowLocalGreeter=true\n")
            ),
            Some(false)
        );
        assert_eq!(
            shows(
                Read("[xdmcp]\nShowLocalGreeter=true\n"),
                Read("[xdmcp]\nEnable=true\nShowLocalGreeter=false\n")
            ),
            Some(true)
        );
        // Only `[xdmcp]` counts; the last line wins; GDM's boolean words.
        assert_eq!(
            shows(
                Read("[daemon]\nEnable=true\nShowLocalGreeter=false\n"),
                Absent
            ),
            Some(true)
        );
        assert_eq!(
            shows(
                Read(
                    "[xdmcp]\nEnable = 1\n# ShowLocalGreeter=true\nShowLocalGreeter=TRUE\nShowLocalGreeter=0\n"
                ),
                Absent
            ),
            Some(false)
        );
        // Unknown: a file that may hold a key, or a value GDM cannot read.
        assert_eq!(shows(Unreadable, Read(ADMIN)), None);
        assert_eq!(shows(Absent, Unreadable), None);
        assert_eq!(shows(Read("[xdmcp]\nEnable=perhaps\n"), Absent), None);
        // The runtime file settles both keys before an unreadable one.
        assert_eq!(shows(Read(PRODUCT_RUNTIME), Unreadable), Some(false));
    }

    #[test]
    fn only_another_products_running_alias_withholds_the_login_screen() {
        let never = || -> Option<bool> { panic!("GDM's settings are read only for a product") };
        // A display manager's own alias: GDM's settings are not even read.
        for unit in DISPLAY_MANAGER_UNITS {
            assert_eq!(LoginScreen::of(Some(unit), true, never), LoginScreen::Shown);
        }
        assert_eq!(LoginScreen::of(None, false, never), LoginScreen::Shown);
        // A stopped product's alias is no display manager (ADR 0025 §10).
        assert_eq!(
            LoginScreen::of(Some("labproduct.service"), false, never),
            LoginScreen::Shown
        );
        let product = |shows| LoginScreen::of(Some("labproduct.service"), true, move || shows);
        assert_eq!(product(Some(true)), LoginScreen::Shown);
        assert_eq!(product(Some(false)), LoginScreen::Withheld);
        assert_eq!(product(None), LoginScreen::Unproven);
        assert!(LoginScreen::Withheld.refuses_sign_in());
        assert!(!LoginScreen::Unproven.refuses_sign_in());
        assert!(!LoginScreen::Shown.refuses_sign_in());
        assert!(LoginScreen::Shown.may_ask_for_login_screen());
        assert!(!LoginScreen::Withheld.may_ask_for_login_screen());
        assert!(!LoginScreen::Unproven.may_ask_for_login_screen());
    }
}
