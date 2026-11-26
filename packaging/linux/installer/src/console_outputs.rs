// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Forced virtual outputs for a headless console (ADR 0025 §6).
//!
//! A host without monitors (a virtual GPU, a rack workstation) has one
//! connected output, so a Deck that matches a layout of several monitors
//! has nowhere to put the others. `--console-outputs N` (or a list of
//! NVIDIA heads) writes `/etc/X11/xorg.conf.d/10-arcen-console-outputs.conf`:
//! an `OutputClass` that makes the console's X server (the login screen's
//! and every user session's) see those heads as connected monitors, only
//! the first one lit. The Deck's layout lights the others while it is
//! attached and the hand-back restores the console's own layout. This
//! changes the host's display configuration, so it is only ever an
//! explicit choice: `--no-console-outputs` and `--uninstall` remove the
//! file (only if it is Arcen's), and nothing else is touched. The X server
//! reads it when it starts: at the next boot, or a display-manager restart
//! while nobody is signed in.
//!
//! A plain `--uninstall` remembers the heads in [`REMEMBERED_PATH`], and the
//! next install without `--console-outputs` or `--no-console-outputs`
//! forces them again, so an uninstall and reinstall leaves the console as
//! it was. `--purge` forgets them with the rest of `/var/lib/arcen`.

use arcen_outputs::console_layout::{
    MAX_VIRTUAL_OUTPUTS, VIRTUAL_OUTPUTS_CONF_PATH, default_forced_heads, forced_heads,
    headless_console_outputs_advice, is_forced_head_token, is_virtual_outputs_conf,
    virtual_outputs_conf,
};

use crate::desktop::InstallLog;
use crate::{Options, map_path, remove_dir_if_empty, remove_file, write_atomic};

/// The heads a plain `--uninstall` removed, for the next install to force
/// again (comma-separated, one line).
pub const REMEMBERED_PATH: &str = "/var/lib/arcen/console-outputs";

/// The administrator's choice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Choice {
    /// Force these heads (`--console-outputs`).
    Heads(Vec<String>),
    /// Remove Arcen's file (`--no-console-outputs`).
    Remove,
}

/// Parses `--console-outputs`: a count (2 to 4: `DFP-0` …) or a
/// comma-separated list of NVIDIA heads.
///
/// # Errors
///
/// A count out of range or a token that is not a head.
pub fn parse(value: &str) -> Result<Vec<String>, String> {
    let range = format!(
        "--console-outputs takes 2 to {MAX_VIRTUAL_OUTPUTS} or a list of NVIDIA heads (DFP-0,DFP-1)"
    );
    if let Ok(count) = value.parse::<usize>() {
        return if (2..=MAX_VIRTUAL_OUTPUTS).contains(&count) {
            Ok(default_forced_heads(count))
        } else {
            Err(range)
        };
    }
    let heads: Vec<String> = value
        .split(',')
        .map(|head| head.trim().to_owned())
        .collect();
    let mut unique = heads.clone();
    unique.sort();
    unique.dedup();
    if heads.len() < 2
        || heads.len() > MAX_VIRTUAL_OUTPUTS
        || unique.len() != heads.len()
        || !heads.iter().all(|head| is_forced_head_token(head))
    {
        return Err(range);
    }
    Ok(heads)
}

/// Applies the command line's choice or, without one, the heads the last
/// plain `--uninstall` remembered, then forgets them: once the file is
/// back, it is the record.
///
/// # Errors
///
/// Writing or removing the file failed.
pub fn install(options: &Options, log: &mut InstallLog) -> Result<(), String> {
    let remembered_path = map_path(&options.prefix, REMEMBERED_PATH);
    let Ok(remembered) = std::fs::read_to_string(&remembered_path) else {
        return apply(options, options.console_outputs.as_ref(), log, false);
    };
    if options.console_outputs.is_some() {
        apply(options, options.console_outputs.as_ref(), log, false)?;
    } else {
        match parse(remembered.trim()) {
            Ok(heads) => apply(options, Some(&Choice::Heads(heads)), log, true)?,
            Err(_) => log.note(format!(
                "warning: {REMEMBERED_PATH} names no valid heads; no console outputs are forced"
            )),
        }
    }
    forget(options)
}

/// Removes the remembered heads.
fn forget(options: &Options) -> Result<(), String> {
    if options.dry_run {
        return Ok(());
    }
    let path = map_path(&options.prefix, REMEMBERED_PATH);
    match std::fs::remove_file(&path) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            Err(format!("remove {}: {error}", path.display()))
        }
        _ => {
            remove_dir_if_empty(options, "/var/lib/arcen");
            Ok(())
        }
    }
}

/// Applies `choice`; `None` leaves the host as it is (an upgrade keeps the
/// file). `restored`: the heads are the ones the last uninstall remembered.
fn apply(
    options: &Options,
    choice: Option<&Choice>,
    log: &mut InstallLog,
    restored: bool,
) -> Result<(), String> {
    let existing =
        std::fs::read_to_string(map_path(&options.prefix, VIRTUAL_OUTPUTS_CONF_PATH)).ok();
    let ours = existing.as_deref().is_some_and(is_virtual_outputs_conf);
    match choice {
        None => {
            if ours {
                log.note(format!(
                    "Console outputs: {VIRTUAL_OUTPUTS_CONF_PATH} kept (remove with --no-console-outputs)"
                ));
            } else if existing.is_none() {
                let discovered = discovered_heads(options);
                if let Some(value) =
                    headless_console_outputs_advice(nvidia_monitor_connected(options), &discovered)
                {
                    log.note(format!(
                        "Console outputs: this host's NVIDIA GPU drives no monitor, so a Deck that                          matches a layout of several displays has one output to use. Rerun with                          --console-outputs {value} ({}), then restart the display manager while                          nobody is signed in.",
                        if discovered.is_empty() {
                            "the Pier names the GPU's real heads in                              /run/arcen/sessions/discovery/nvidia-heads.json once it has started                              with the GPU free; use those"
                        } else {
                            "the GPU's heads as the Pier found them"
                        }
                    ));
                }
            }
            Ok(())
        }
        Some(Choice::Remove) => {
            if ours {
                remove_file(options, VIRTUAL_OUTPUTS_CONF_PATH)?;
                log.note(format!(
                    "Console outputs: removed {VIRTUAL_OUTPUTS_CONF_PATH}; the console's X server \
                     shows only its own outputs after its next start (reboot, or `systemctl restart \
                     gdm` while nobody is signed in)"
                ));
            } else if existing.is_some() {
                log.note(format!(
                    "warning: {VIRTUAL_OUTPUTS_CONF_PATH} is not Arcen's; kept"
                ));
            }
            Ok(())
        }
        Some(Choice::Heads(heads)) => {
            if existing.is_some() && !ours {
                log.note(format!(
                    "warning: {VIRTUAL_OUTPUTS_CONF_PATH} exists and is not Arcen's; kept, and no \
                     console outputs are forced"
                ));
                return Ok(());
            }
            let text = virtual_outputs_conf(heads)
                .ok_or_else(|| "console outputs: invalid head list".to_owned())?;
            if existing.as_deref() == Some(text.as_str()) {
                log.note(format!(
                    "Console outputs: {} forced (unchanged)",
                    heads.join(", ")
                ));
                return Ok(());
            }
            write_atomic(
                options,
                VIRTUAL_OUTPUTS_CONF_PATH,
                text.as_bytes(),
                0o644,
                true,
            )?;
            if restored {
                log.note(format!(
                    "Console outputs: forcing {} again, as before the uninstall \
                     ({VIRTUAL_OUTPUTS_CONF_PATH}); undo with --no-console-outputs",
                    heads.join(", ")
                ));
                return Ok(());
            }
            log.note(format!(
                "Console outputs: wrote {VIRTUAL_OUTPUTS_CONF_PATH}, forcing {} as connected \
                 monitors (the first lit; a Deck's layout lights the others while it is attached). \
                 The console's X server reads it when it starts: reboot, or `systemctl restart \
                 gdm` while nobody is signed in at the console. Undo with --no-console-outputs.",
                heads.join(", ")
            ));
            Ok(())
        }
    }
}

/// Whether an NVIDIA GPU of this host drives a monitor, from DRM's connector
/// status (`nvidia-drm`): `None` when no NVIDIA card exposes connectors.
fn nvidia_monitor_connected(options: &Options) -> Option<bool> {
    let root = map_path(&options.prefix, "/sys/class/drm");
    let entries = std::fs::read_dir(&root).ok()?;
    let mut seen = false;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some((card, _connector)) = name.split_once('-') else {
            continue;
        };
        let driver = std::fs::read_link(root.join(card).join("device/driver")).ok();
        if driver
            .as_deref()
            .and_then(std::path::Path::file_name)
            .is_none_or(|driver| driver != "nvidia")
        {
            continue;
        }
        let Ok(status) = std::fs::read_to_string(entry.path().join("status")) else {
            continue;
        };
        seen = true;
        if status.trim() == "connected" {
            return Some(true);
        }
    }
    seen.then_some(false)
}

/// The heads the Pier's start-time probe found, if it has run.
fn discovered_heads(options: &Options) -> Vec<String> {
    std::fs::read_to_string(map_path(
        &options.prefix,
        "/run/arcen/sessions/discovery/nvidia-heads.json",
    ))
    .ok()
    .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
    .and_then(|discovery| {
        discovery.get("heads")?.as_array().map(|heads| {
            heads
                .iter()
                .filter_map(|head| head.as_str().map(str::to_owned))
                .collect()
        })
    })
    .unwrap_or_default()
}

/// `--uninstall`: removes the file if it is Arcen's and, without `--purge`,
/// remembers its heads for the next install.
///
/// # Errors
///
/// Recording the heads or removing the file failed.
pub fn remove(options: &Options) -> Result<(), String> {
    let Ok(text) = std::fs::read_to_string(map_path(&options.prefix, VIRTUAL_OUTPUTS_CONF_PATH))
    else {
        return Ok(());
    };
    if !is_virtual_outputs_conf(&text) {
        return Ok(());
    }
    if let Some(heads) = forced_heads(&text).filter(|_| !options.purge) {
        write_atomic(
            options,
            REMEMBERED_PATH,
            format!("{}\n", heads.join(",")).as_bytes(),
            0o644,
            true,
        )?;
        println!(
            "Console outputs: {} remembered in {REMEMBERED_PATH}; a reinstall forces them again \
             (--purge forgets them)",
            heads.join(", ")
        );
    }
    remove_file(options, VIRTUAL_OUTPUTS_CONF_PATH)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_and_lists_parse_and_nonsense_is_refused() {
        assert_eq!(parse("2").expect("two"), ["DFP-0", "DFP-1"]);
        assert_eq!(parse("DFP-1, DFP-3").expect("list"), ["DFP-1", "DFP-3"]);
        for bad in [
            "1",
            "5",
            "0",
            "DFP-0",
            "DFP-0,DFP-0",
            "DFP-0,/etc/x",
            "a,b",
            "",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }

    fn options(name: &str, purge: bool, choice: Option<Choice>) -> Options {
        Options {
            prefix: std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join(".arcen-linux-installer-tests")
                .join(format!("console-outputs-{name}-{}", std::process::id())),
            dry_run: false,
            uninstall: false,
            purge,
            force: false,
            no_service: true,
            restart: false,
            check: false,
            qualify_outputs: Vec::new(),
            disable_gdm: false,
            restore_gdm: false,
            no_native_tablet_modules: false,
            native_tablet: None,
            console_outputs: choice,
            extra_sans: Vec::new(),
        }
    }

    /// A host installed with `--console-outputs 4`, uninstalled without
    /// `--purge`.
    fn uninstalled(options: &Options) -> String {
        let _ = std::fs::remove_dir_all(&options.prefix);
        let mut log = InstallLog::default();
        apply(
            options,
            Some(&Choice::Heads(parse("4").expect("four"))),
            &mut log,
            false,
        )
        .expect("install");
        let conf = map_path(&options.prefix, VIRTUAL_OUTPUTS_CONF_PATH);
        let written = std::fs::read_to_string(&conf).expect("written");
        remove(options).expect("uninstall");
        assert!(!conf.exists(), "uninstall removes the file");
        written
    }

    #[test]
    fn a_plain_reinstall_forces_the_heads_the_uninstall_removed() {
        let options = options("reinstall", false, None);
        let written = uninstalled(&options);
        let remembered = map_path(&options.prefix, REMEMBERED_PATH);
        assert_eq!(
            std::fs::read_to_string(&remembered).expect("remembered"),
            "DFP-0,DFP-1,DFP-2,DFP-3\n"
        );
        let mut log = InstallLog::default();
        install(&options, &mut log).expect("reinstall");
        let conf = map_path(&options.prefix, VIRTUAL_OUTPUTS_CONF_PATH);
        assert_eq!(std::fs::read_to_string(&conf).expect("restored"), written);
        assert!(format!("{log:?}").contains("again, as before the uninstall"));
        assert!(!remembered.exists(), "the file is the record again");
        assert!(
            !map_path(&options.prefix, "/var/lib/arcen").exists(),
            "no empty state directory is left behind"
        );
        let _ = std::fs::remove_dir_all(&options.prefix);
    }

    #[test]
    fn the_command_line_wins_over_what_the_uninstall_remembered() {
        let options = options("explicit", false, Some(Choice::Remove));
        uninstalled(&options);
        install(&options, &mut InstallLog::default()).expect("reinstall");
        assert!(!map_path(&options.prefix, VIRTUAL_OUTPUTS_CONF_PATH).exists());
        assert!(!map_path(&options.prefix, REMEMBERED_PATH).exists());

        let options = self::options("two", false, Some(Choice::Heads(parse("2").expect("two"))));
        uninstalled(&options);
        install(&options, &mut InstallLog::default()).expect("reinstall");
        let conf = map_path(&options.prefix, VIRTUAL_OUTPUTS_CONF_PATH);
        assert_eq!(
            forced_heads(&std::fs::read_to_string(conf).expect("two heads")),
            Some(parse("2").expect("two"))
        );
        assert!(!map_path(&options.prefix, REMEMBERED_PATH).exists());
        let _ = std::fs::remove_dir_all(&options.prefix);
        let _ = std::fs::remove_dir_all(&self::options("explicit", false, None).prefix);
    }

    #[test]
    fn a_purge_remembers_nothing() {
        let options = options("purge", true, None);
        uninstalled(&options);
        assert!(!map_path(&options.prefix, REMEMBERED_PATH).exists());
        install(&options, &mut InstallLog::default()).expect("reinstall");
        assert!(!map_path(&options.prefix, VIRTUAL_OUTPUTS_CONF_PATH).exists());
        let _ = std::fs::remove_dir_all(&options.prefix);
    }
}
