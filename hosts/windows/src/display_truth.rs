// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The Windows Pier's reader for the shared display truth
//! (`arcen_outputs::display_truth`, ADR 0025 §14).
//!
//! It reads every target Windows lists as connected, active or not, with
//! `DisplayConfig` (`QDC_ALL_PATHS`): the source mode's size and place, the
//! target's refresh and rotation, the EDID the monitor device carries in the
//! registry, and the Advanced Color state (`GET_ADVANCED_COLOR_INFO_2` on
//! Windows 11). An output is identified by its adapter's device path and
//! target id, which survive a reboot and an EDID change; the `\\.\DISPLAYn`
//! name is only its label. Nothing here changes a display.
//!
//! `QueryDisplayConfig` answers only inside an interactive session (Win32
//! error 5 from a service or over SSH), so the agent reads it before a Deck
//! attaches and after it leaves, and `arcen-pier diagnose displays` reads it
//! when run on the desktop.

use crate::logging::DISPLAY;

pub(crate) const SOURCE: &str = "windows-displayconfig";

/// The registry key holding a monitor's `EDID` value, from its monitor
/// device path (`\\?\DISPLAY#ARN0927#5&2b5c2d1&0&UID24835#{e6f07b5f-…}` →
/// `SYSTEM\CurrentControlSet\Enum\DISPLAY\ARN0927\5&2b5c2d1&0&UID24835\Device Parameters`).
pub(crate) fn edid_registry_key(monitor_device_path: &str) -> Option<String> {
    let rest = monitor_device_path.strip_prefix(r"\\?\")?;
    let mut parts = rest.split('#');
    let class = parts.next()?;
    let hardware = parts.next()?;
    let instance = parts.next()?;
    let interface = parts.next()?;
    let plain = |part: &str| {
        !part.is_empty()
            && part
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || "&_-.".contains(character))
    };
    if !class.eq_ignore_ascii_case("DISPLAY")
        || !plain(hardware)
        || !plain(instance)
        || !interface.starts_with('{')
        || parts.next().is_some()
    {
        return None;
    }
    Some(format!(
        r"SYSTEM\CurrentControlSet\Enum\DISPLAY\{hardware}\{instance}\Device Parameters"
    ))
}

#[cfg(windows)]
pub(crate) use native::read;

#[cfg(not(windows))]
pub(crate) fn read() -> Result<arcen_outputs::display_truth::DisplayTruth, String> {
    Err("display truth is read on Windows only".to_string())
}

/// How long the displays may take to read back as they were after the
/// session's restore: Windows re-enumerates a monitor whose EDID was put back
/// and settles Advanced Color asynchronously.
const RESTORE_SETTLE: std::time::Duration = std::time::Duration::from_secs(6);
const RESTORE_POLL: std::time::Duration = std::time::Duration::from_millis(250);

/// The displays before a Deck changes anything (contract part 1), logged as
/// the shared JSON. `None` when Windows cannot answer (logged): the session
/// still runs, and its restore is then not proved.
pub(crate) fn read_before() -> Option<arcen_outputs::display_truth::DisplayTruth> {
    match read() {
        Ok(truth) => {
            tracing::info!(
                target: DISPLAY,
                outputs = truth.outputs.len(),
                "displays before the session"
            );
            log_outputs("before", &truth);
            Some(truth)
        }
        Err(error) => {
            tracing::warn!(
                target: DISPLAY,
                %error,
                "the displays before the session could not be read; their restore cannot be proved"
            );
            None
        }
    }
}

/// Reads the displays again after the session's restore until they equal
/// `before` (shared `restore_differences`), for at most [`RESTORE_SETTLE`].
///
/// # Errors
///
/// `display_restore_incomplete: …` naming every provable difference, or the
/// read failure, when they are not back.
pub(crate) fn verify_restored(
    before: &arcen_outputs::display_truth::DisplayTruth,
) -> Result<(), String> {
    verify_restored_with(before, read, RESTORE_SETTLE, RESTORE_POLL)
}

fn verify_restored_with(
    before: &arcen_outputs::display_truth::DisplayTruth,
    mut read: impl FnMut() -> Result<arcen_outputs::display_truth::DisplayTruth, String>,
    settle: std::time::Duration,
    poll: std::time::Duration,
) -> Result<(), String> {
    use arcen_outputs::display_truth::{restore_differences, DisplayTruthReason};
    let deadline = std::time::Instant::now() + settle;
    loop {
        let outcome = read().map(|after| {
            let differences = restore_differences(before, &after);
            (after, differences)
        });
        let settled = matches!(&outcome, Ok((_, differences)) if differences.iter().all(|difference| !difference.provable));
        if settled || std::time::Instant::now() >= deadline {
            return match outcome {
                Ok((after, differences)) if settled => {
                    if differences.is_empty() {
                        tracing::info!(
                            target: DISPLAY,
                            outputs = after.outputs.len(),
                            "the displays are back exactly as they were before the session"
                        );
                    } else {
                        tracing::warn!(
                            target: DISPLAY,
                            unproved = %join(&differences),
                            "the displays are back where they could be read; some values could not be read to prove it"
                        );
                        log_outputs("after", &after);
                    }
                    Ok(())
                }
                Ok((after, differences)) => {
                    let provable = differences
                        .into_iter()
                        .filter(|difference| difference.provable)
                        .collect::<Vec<_>>();
                    let message = DisplayTruthReason::RestoreIncomplete.message(&format!(
                        "the displays differ from before the session: {}",
                        join(&provable)
                    ));
                    tracing::error!(
                        target: DISPLAY,
                        reason_code = DisplayTruthReason::RestoreIncomplete.code(),
                        differences = %join(&provable),
                        "the displays are not back as they were before the session"
                    );
                    for difference in &provable {
                        tracing::error!(target: DISPLAY, %difference, "display restore difference");
                    }
                    log_outputs("after", &after);
                    Err(message)
                }
                Err(error) => {
                    tracing::error!(
                        target: DISPLAY,
                        reason_code = DisplayTruthReason::RestoreIncomplete.code(),
                        %error,
                        "the displays after the session could not be read; their restore is not proved"
                    );
                    Err(DisplayTruthReason::RestoreIncomplete.message(&format!(
                        "the displays after the session could not be read: {error}"
                    )))
                }
            };
        }
        std::thread::sleep(poll);
    }
}

/// One log line per output (a whole snapshot exceeds a log field's bound).
fn log_outputs(stage: &'static str, truth: &arcen_outputs::display_truth::DisplayTruth) {
    for output in truth.to_text().lines().skip(1) {
        tracing::info!(target: DISPLAY, stage, output, "display truth");
    }
}

fn join(differences: &[arcen_outputs::display_truth::RestoreDifference]) -> String {
    differences
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

#[cfg(windows)]
mod native {
    use arcen_outputs::display_truth::{ColourMode, DisplayTruth, ModeTruth, OutputTruth};
    use windows::core::{HSTRING, PCWSTR};
    use windows::Win32::Devices::Display::{
        GetDisplayConfigBufferSizes, QueryDisplayConfig, DISPLAYCONFIG_MODE_INFO,
        DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE, DISPLAYCONFIG_PATH_INFO,
        DISPLAYCONFIG_ROTATION_ROTATE180, DISPLAYCONFIG_ROTATION_ROTATE270,
        DISPLAYCONFIG_ROTATION_ROTATE90, QDC_ALL_PATHS,
    };
    use windows::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS};
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_BINARY};

    use crate::advanced_color::{
        adapter_device_path, query_distinct_hdr, query_legacy, source_gdi_name,
        target_monitor_device_path, windows_11_or_later,
    };

    /// `DISPLAYCONFIG_PATH_ACTIVE`, which windows-rs does not export.
    const PATH_ACTIVE: u32 = 0x1;
    const QUERY_ATTEMPTS: usize = 4;
    const MAX_EDID: usize = 4096;

    /// Every connected target as Windows reads it now.
    pub(crate) fn read() -> Result<DisplayTruth, String> {
        let (paths, modes) = query_all()?;
        let distinct_hdr = windows_11_or_later()?;
        let mut truth = DisplayTruth::new(super::SOURCE);
        let mut seen: Vec<(u32, i32, u32)> = Vec::new();
        // Active paths first, so a target listed active and inactive is read
        // as active.
        let mut ordered = paths.iter().collect::<Vec<_>>();
        ordered.sort_by_key(|path| path.flags & PATH_ACTIVE == 0);
        for path in ordered {
            let active = path.flags & PATH_ACTIVE != 0;
            if !active && !path.targetInfo.targetAvailable.as_bool() {
                continue;
            }
            let key = (
                path.targetInfo.adapterId.LowPart,
                path.targetInfo.adapterId.HighPart,
                path.targetInfo.id,
            );
            if seen.contains(&key) {
                continue;
            }
            seen.push(key);
            truth
                .outputs
                .push(read_target(path, &modes, active, distinct_hdr)?);
        }
        Ok(truth)
    }

    fn read_target(
        path: &DISPLAYCONFIG_PATH_INFO,
        modes: &[DISPLAYCONFIG_MODE_INFO],
        active: bool,
        distinct_hdr: bool,
    ) -> Result<OutputTruth, String> {
        let adapter = adapter_device_path(path)?;
        let monitor = target_monitor_device_path(path).ok();
        let mut output = OutputTruth::new(
            format!("{adapter}#target{}", path.targetInfo.id),
            if active {
                source_gdi_name(path).unwrap_or_default()
            } else {
                String::new()
            },
        );
        output.edid = monitor.as_deref().and_then(read_edid);
        if !active {
            return Ok(output);
        }
        // SAFETY: queried without QDC_VIRTUAL_MODE_AWARE, so the union holds
        // the plain mode index (DISPLAYCONFIG_PATH_MODE_IDX_INVALID when none).
        let index = unsafe { path.sourceInfo.Anonymous.modeInfoIdx };
        let source = usize::try_from(index)
            .ok()
            .and_then(|index| modes.get(index))
            .filter(|mode| mode.infoType == DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE)
            // SAFETY: the info type says the union holds a source mode.
            .map(|mode| unsafe { mode.Anonymous.sourceMode });
        if let Some(source) = source {
            output.active = true;
            output.x = source.position.x;
            output.y = source.position.y;
            output.primary = source.position.x == 0 && source.position.y == 0;
            let rate = path.targetInfo.refreshRate;
            let refresh_mhz = if rate.Denominator == 0 {
                0
            } else {
                u32::try_from(u64::from(rate.Numerator) * 1000 / u64::from(rate.Denominator))
                    .unwrap_or(0)
            };
            output.mode = Some(ModeTruth {
                width: source.width,
                height: source.height,
                refresh_mhz,
            });
        }
        output.rotation = match path.targetInfo.rotation {
            DISPLAYCONFIG_ROTATION_ROTATE90 => arcen_media::Rotation::Degrees90,
            DISPLAYCONFIG_ROTATION_ROTATE180 => arcen_media::Rotation::Degrees180,
            DISPLAYCONFIG_ROTATION_ROTATE270 => arcen_media::Rotation::Degrees270,
            _ => arcen_media::Rotation::Degrees0,
        };
        let state = match monitor {
            Some(monitor) if distinct_hdr => query_distinct_hdr(
                path.targetInfo.adapterId,
                path.targetInfo.id,
                adapter,
                monitor,
            )
            .ok(),
            Some(monitor) => query_legacy(
                path.targetInfo.adapterId,
                path.targetInfo.id,
                adapter,
                monitor,
            )
            .ok(),
            None => None,
        };
        match state {
            Some(state) => {
                let (colour, switch, bits) = state.truth();
                output.colour = colour;
                output.hdr_switch = Some(switch);
                output.bits_per_channel = bits;
            }
            None => output.colour = ColourMode::Unknown,
        }
        Ok(output)
    }

    /// The EDID a monitor device carries in the registry; `None` when it has
    /// none or it cannot be read.
    fn read_edid(monitor_device_path: &str) -> Option<Vec<u8>> {
        let key = HSTRING::from(super::edid_registry_key(monitor_device_path)?);
        let value = HSTRING::from("EDID");
        let mut bytes = vec![0_u8; MAX_EDID];
        let mut len = u32::try_from(bytes.len()).ok()?;
        // SAFETY: both strings are live and null-terminated, and the buffer
        // and its byte length describe the live vector.
        let status = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                PCWSTR(key.as_ptr()),
                PCWSTR(value.as_ptr()),
                RRF_RT_REG_BINARY,
                None,
                Some(bytes.as_mut_ptr().cast()),
                Some(&mut len),
            )
        };
        if status != ERROR_SUCCESS {
            return None;
        }
        bytes.truncate(usize::try_from(len).ok()?);
        (!bytes.is_empty()).then_some(bytes)
    }

    fn query_all() -> Result<(Vec<DISPLAYCONFIG_PATH_INFO>, Vec<DISPLAYCONFIG_MODE_INFO>), String> {
        for _ in 0..QUERY_ATTEMPTS {
            let mut path_count = 0_u32;
            let mut mode_count = 0_u32;
            // SAFETY: both count pointers name initialized writable values.
            let sizes = unsafe {
                GetDisplayConfigBufferSizes(QDC_ALL_PATHS, &mut path_count, &mut mode_count)
            };
            if sizes != ERROR_SUCCESS {
                return Err(format!(
                    "size the display configuration: Win32 error {}",
                    sizes.0
                ));
            }
            let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); path_count as usize];
            let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); mode_count as usize];
            // SAFETY: the buffers are sized from the query just above and
            // live for the call; the counts match their capacities.
            let query = unsafe {
                QueryDisplayConfig(
                    QDC_ALL_PATHS,
                    &mut path_count,
                    paths.as_mut_ptr(),
                    &mut mode_count,
                    modes.as_mut_ptr(),
                    None,
                )
            };
            if query == ERROR_INSUFFICIENT_BUFFER {
                continue;
            }
            if query != ERROR_SUCCESS {
                return Err(format!(
                    "query the display configuration: Win32 error {} (it answers only on \
                     the interactive desktop)",
                    query.0
                ));
            }
            paths.truncate(path_count as usize);
            modes.truncate(mode_count as usize);
            return Ok((paths, modes));
        }
        Err("the display configuration changed while it was read".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcen_outputs::display_truth::{DisplayTruth, ModeTruth, OutputTruth};

    fn truth(refresh_hz: u32) -> DisplayTruth {
        let mut output = OutputTruth::new("adapter#target1", r"\\.\DISPLAY1");
        output.active = true;
        output.primary = true;
        output.mode = Some(ModeTruth::at_hz(2560, 1440, refresh_hz));
        DisplayTruth {
            source: SOURCE,
            outputs: vec![output],
        }
    }

    // The audit finding: the restore was judged by the active set and the
    // primary alone, so a display put back at another refresh passed.
    #[test]
    fn a_restore_is_proved_only_when_the_displays_read_back_as_before() {
        let instant = std::time::Duration::ZERO;
        let before = truth(60);
        let mut reads = vec![Ok(truth(30)), Ok(truth(60))].into_iter();
        assert_eq!(
            verify_restored_with(
                &before,
                || reads.next().unwrap(),
                std::time::Duration::from_secs(5),
                instant
            ),
            Ok(())
        );
        let error = verify_restored_with(&before, || Ok(truth(30)), instant, instant).unwrap_err();
        assert!(error.starts_with("display_restore_incomplete: "), "{error}");
        assert!(error.contains("mode: 2560x1440@60000mHz -> 2560x1440@30000mHz"));
        let error = verify_restored_with(&before, || Err("Win32 error 5".into()), instant, instant)
            .unwrap_err();
        assert!(error.starts_with("display_restore_incomplete: "), "{error}");
    }

    #[test]
    fn a_monitor_device_path_names_its_edid_key() {
        assert_eq!(
            edid_registry_key(
                r"\\?\DISPLAY#ARN0927#5&2b5c2d1&0&UID24835#{e6f07b5f-ee97-4a90-b076-33f57bf4eaa7}"
            )
            .as_deref(),
            Some(
                r"SYSTEM\CurrentControlSet\Enum\DISPLAY\ARN0927\5&2b5c2d1&0&UID24835\Device Parameters"
            )
        );
        for refused in [
            r"DISPLAY#ARN0927#5&2b&0&UID1#{e6f07b5f}",
            r"\\?\USB#VID_1#5&2b&0#{e6f07b5f}",
            r"\\?\DISPLAY#ARN0927\..\X#5&2b#{e6f07b5f}",
            r"\\?\DISPLAY#ARN0927#5&2b",
            r"\\?\DISPLAY##5&2b#{e6f07b5f}",
        ] {
            assert_eq!(edid_registry_key(refused), None, "{refused}");
        }
    }
}
