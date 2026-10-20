//! The display manager and native Wayland desktops.
//!
//! A native desktop (`wayland.session_mode`, see
//! `arcen_session::native_seat`) runs on its own logind seat. GDM starts a
//! login screen on every new seat. The Pier builds each seat in an order
//! that makes that login screen fail, but a display-manager restart while a
//! native desktop runs would put a login screen on the desktop's seat and
//! take it from the user. On a host whose GPU has no display connectors the
//! display manager shows nothing anyway (its own login screen fails at
//! every boot), so the installer explains this and, only when asked with
//! `--disable-gdm`, disables it. It records what it changed and puts it
//! back with `--restore-gdm` and on `--uninstall`.

use std::path::Path;
use std::process::Command;

use crate::desktop::InstallLog;

/// What the installer changed, so it can be undone exactly.
pub const MARKER_PATH: &str = "/var/lib/arcen/display-manager-disabled.json";

const SYSTEMCTL: &str = "/usr/bin/systemctl";

/// The display manager unit and its state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayManager {
    /// `display-manager.service` resolved, e.g. `gdm.service`.
    pub unit: String,
    pub enabled: bool,
    pub active: bool,
}

/// The marker's content: the unit and the state to restore.
#[must_use]
pub fn marker_json(manager: &DisplayManager) -> String {
    serde_json::json!({
        "unit": manager.unit,
        "was_enabled": manager.enabled,
        "was_active": manager.active,
    })
    .to_string()
}

/// Reads a marker; `None` for anything this installer did not write.
#[must_use]
pub fn parse_marker(text: &str) -> Option<DisplayManager> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    let unit = value.get("unit")?.as_str()?;
    if !valid_unit(unit) {
        return None;
    }
    Some(DisplayManager {
        unit: unit.to_owned(),
        enabled: value.get("was_enabled")?.as_bool()?,
        active: value.get("was_active")?.as_bool()?,
    })
}

/// A plain systemd service name, never an argument or a path.
fn valid_unit(unit: &str) -> bool {
    unit.strip_suffix(".service").is_some_and(|name| {
        !name.is_empty()
            && !name.starts_with('-')
            && name.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'@')
            })
    })
}

/// The lines the installer prints about a running display manager on a
/// host that can run native desktops.
#[must_use]
pub fn explanation(unit: &str) -> Vec<String> {
    vec![
        String::new(),
        format!(" Native desktops and {unit}:"),
        "   This host can run a Wayland desktop on its own seat (a real pen in".to_owned(),
        format!("   the desktop). {unit} starts a login screen on every new seat. The Pier"),
        "   builds each seat so that login screen fails, but restarting the display".to_owned(),
        "   manager while a native desktop runs would put a login screen on that".to_owned(),
        "   desktop's seat and freeze it. This GPU has no display connectors, so the".to_owned(),
        "   display manager shows nothing on this host. To disable it (undone by".to_owned(),
        "   --restore-gdm and by --uninstall), run the installer again with:".to_owned(),
        "     --disable-gdm".to_owned(),
        "   Or set wayland.session_mode to \"headless\" to keep every desktop headless.".to_owned(),
    ]
}

/// `display-manager.service` resolved, with its state; `None` without one.
#[must_use]
pub fn current() -> Option<DisplayManager> {
    let output = Command::new(SYSTEMCTL)
        .args(["show", "-p", "Id", "--value", "display-manager.service"])
        .output()
        .ok()?;
    let unit = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if !output.status.success() || !valid_unit(&unit) || unit == "display-manager.service" {
        return None;
    }
    let quiet = |verb: &str| {
        Command::new(SYSTEMCTL)
            .args([verb, "--quiet", &unit])
            .status()
            .is_ok_and(|status| status.success())
    };
    Some(DisplayManager {
        enabled: quiet("is-enabled"),
        active: quiet("is-active"),
        unit,
    })
}

/// `--disable-gdm`: records the display manager's state, then disables and
/// stops it.
///
/// # Errors
///
/// When the marker cannot be written or systemctl fails.
pub fn disable(dry_run: bool, log: &mut InstallLog) -> Result<(), String> {
    let Some(manager) = current() else {
        log.note("--disable-gdm: this host runs no display manager; nothing to disable");
        return Ok(());
    };
    if !manager.enabled && !manager.active {
        log.note(format!(
            "--disable-gdm: {} is already disabled and stopped",
            manager.unit
        ));
        return Ok(());
    }
    if dry_run {
        println!(
            "dry-run: would record {} in {MARKER_PATH} and run `systemctl disable --now {}`",
            manager.unit, manager.unit
        );
        return Ok(());
    }
    if !Path::new(MARKER_PATH).exists() {
        if let Some(parent) = Path::new(MARKER_PATH).parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("{}: {error}", parent.display()))?;
        }
        std::fs::write(MARKER_PATH, marker_json(&manager))
            .map_err(|error| format!("{MARKER_PATH}: {error}"))?;
    }
    let status = Command::new(SYSTEMCTL)
        .args(["disable", "--now", &manager.unit])
        .status()
        .map_err(|error| format!("systemctl: {error}"))?;
    if !status.success() {
        return Err(format!(
            "systemctl disable --now {}: {status}",
            manager.unit
        ));
    }
    log.note(format!(
        "--disable-gdm: {} disabled and stopped (was enabled: {}, active: {}); \
         --restore-gdm or --uninstall puts it back",
        manager.unit, manager.enabled, manager.active
    ));
    Ok(())
}

/// Arcen native desktop seats logind has now (`seat-arcen-pen-…`).
#[must_use]
pub fn live_arcen_seats() -> Vec<String> {
    Command::new("/usr/bin/loginctl")
        .args(["list-seats", "--no-legend"])
        .output()
        .map(|output| arcen_seats(&String::from_utf8_lossy(&output.stdout)))
        .unwrap_or_default()
}

/// The Arcen desktop seats in a `loginctl list-seats --no-legend` listing.
#[must_use]
pub fn arcen_seats(listing: &str) -> Vec<String> {
    listing
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .filter(|seat| seat.starts_with(arcen_session::polkit_remote::ARCEN_SEAT_PREFIX))
        .map(str::to_owned)
        .collect()
}

/// How [`restore`] may bring the display manager back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreMode {
    /// `--restore-gdm`: refused while a native desktop's seat is live, since
    /// a started display manager would put a login screen on it.
    Standalone,
    /// `--uninstall` (the Pier is already stopped): a seat that is still
    /// live only defers the start to the next boot.
    Uninstall,
}

/// `--restore-gdm` and `--uninstall`: puts back what `--disable-gdm`
/// changed, then removes the marker. Without a marker it does nothing.
///
/// # Errors
///
/// When systemctl fails (the marker is then kept for another try), or, for
/// `--restore-gdm`, while a native desktop's seat is live.
pub fn restore(dry_run: bool, mode: RestoreMode, log: &mut InstallLog) -> Result<(), String> {
    let Ok(text) = std::fs::read_to_string(MARKER_PATH) else {
        return Ok(());
    };
    let Some(manager) = parse_marker(&text) else {
        log.note(format!(
            "{MARKER_PATH} is not one this installer wrote; the display manager was left as it is"
        ));
        return Ok(());
    };
    if dry_run {
        println!(
            "dry-run: would restore {} (enabled: {}, active: {}) and remove {MARKER_PATH}",
            manager.unit, manager.enabled, manager.active
        );
        return Ok(());
    }
    let seats = live_arcen_seats();
    if !seats.is_empty() && mode == RestoreMode::Standalone {
        return Err(format!(
            "an Arcen native desktop's seat is live ({}): starting {} now would put a login screen \
             on it. Stop the Pier first (`systemctl stop arcen-pier`), then run --restore-gdm again",
            seats.join(", "),
            manager.unit
        ));
    }
    let mut verbs = Vec::new();
    if manager.enabled {
        verbs.push("enable");
    }
    if manager.active && seats.is_empty() {
        verbs.push("start");
    } else if manager.active {
        log.note(format!(
            "{} is enabled but not started: an Arcen native desktop's seat is still live ({}); \
             it starts at the next boot",
            manager.unit,
            seats.join(", ")
        ));
    }
    for verb in verbs {
        let status = Command::new(SYSTEMCTL)
            .args([verb, &manager.unit])
            .status()
            .map_err(|error| format!("systemctl: {error}"))?;
        if !status.success() {
            return Err(format!("systemctl {verb} {}: {status}", manager.unit));
        }
    }
    std::fs::remove_file(MARKER_PATH).map_err(|error| format!("{MARKER_PATH}: {error}"))?;
    log.note(format!(
        "{} restored (enabled: {}, active: {})",
        manager.unit, manager.enabled, manager.active
    ));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_marker_round_trips_and_refuses_anything_else() {
        let manager = DisplayManager {
            unit: "gdm.service".into(),
            enabled: true,
            active: false,
        };
        assert_eq!(parse_marker(&marker_json(&manager)), Some(manager));
        for refused in [
            "",
            "{}",
            r#"{"unit":"gdm","was_enabled":true,"was_active":true}"#,
            r#"{"unit":"--now.service","was_enabled":true,"was_active":true}"#,
            r#"{"unit":"../x.service","was_enabled":true,"was_active":true}"#,
            r#"{"unit":"gdm.service; rm","was_enabled":true,"was_active":true}"#,
            r#"{"unit":"gdm.service","was_enabled":"yes","was_active":true}"#,
        ] {
            assert_eq!(parse_marker(refused), None, "{refused}");
        }
    }

    #[test]
    fn only_arcen_desktop_seats_hold_the_display_manager_back() {
        let listing = "seat0\nseat-arcen-pen-0123456789abcdef\nseat-other\n";
        assert_eq!(arcen_seats(listing), ["seat-arcen-pen-0123456789abcdef"]);
        assert!(arcen_seats("seat0\n").is_empty());
        assert!(arcen_seats("").is_empty());
    }

    #[test]
    fn the_explanation_names_the_unit_and_both_ways_out() {
        let text = explanation("gdm.service").join("\n");
        assert!(text.contains("gdm.service"));
        assert!(text.contains("--disable-gdm"));
        assert!(text.contains("--restore-gdm"));
        assert!(text.contains("\"headless\""));
    }
}
