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

use arcen_outputs::console_layout::{
    MAX_VIRTUAL_OUTPUTS, VIRTUAL_OUTPUTS_CONF_PATH, default_forced_heads, is_forced_head_token,
    is_virtual_outputs_conf, virtual_outputs_conf,
};

use crate::desktop::InstallLog;
use crate::{Options, map_path, remove_file, write_atomic};

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

/// Applies `choice`; `None` leaves the host as it is (an upgrade keeps the
/// file).
///
/// # Errors
///
/// Writing or removing the file failed.
pub fn apply(
    options: &Options,
    choice: Option<&Choice>,
    log: &mut InstallLog,
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

/// `--uninstall`: removes the file if it is Arcen's.
///
/// # Errors
///
/// Removing it failed.
pub fn remove(options: &Options) -> Result<(), String> {
    if std::fs::read_to_string(map_path(&options.prefix, VIRTUAL_OUTPUTS_CONF_PATH))
        .is_ok_and(|text| is_virtual_outputs_conf(&text))
    {
        remove_file(options, VIRTUAL_OUTPUTS_CONF_PATH)?;
    }
    Ok(())
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
}
