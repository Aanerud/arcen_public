// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! What a host did with the display the Deck asked for, when it did not do
//! exactly that: one line every Deck shows (status/notice and Stream
//! Statistics) from the hello's `display_resolution` capability.

use std::collections::BTreeMap;

/// The host's notice that an earlier session's displays could not be put
/// back (shared `display_truth::DisplayTruthReason::RestorePending`).
pub const DISPLAY_RESTORE_PENDING: &str = "display_restore_pending";

/// What the Deck shows for [`DISPLAY_RESTORE_PENDING`].
pub const RESTORE_PENDING_NOTE: &str =
    "The displays could not be put back after an earlier session.";

/// Report what the host actually did with the requested display size and
/// UI scale.
///
/// Hosts publish `display_resolution` in `ServerHello.device_capabilities` with
/// the size they were asked for, the size they settled on, whether live
/// resize is available and, on Windows, each display's UI scale. Returns
/// `None` when the host honoured the request and can resize.
#[must_use]
pub fn display_negotiation_note(
    capabilities: &BTreeMap<String, serde_json::Value>,
) -> Option<String> {
    let display = capabilities.get("display_resolution")?;
    let dimension = |key: &str, axis: &str| display.get(key)?.get(axis)?.as_u64();
    let mut notes: Vec<String> = Vec::new();

    // A Windows Pier whose earlier session's displays it could not put back
    // says so (`display_restore_pending`) and serves this session anyway.
    if display
        .get("restore_notice")
        .and_then(serde_json::Value::as_str)
        == Some(DISPLAY_RESTORE_PENDING)
    {
        notes.push(RESTORE_PENDING_NOTE.to_owned());
    }

    if let (Some(rw), Some(rh), Some(aw), Some(ah)) = (
        dimension("requested", "width"),
        dimension("requested", "height"),
        dimension("applied", "width"),
        dimension("applied", "height"),
    ) && (rw, rh) != (aw, ah)
    {
        let backend = display
            .get("backend")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown display backend");
        notes.push(format!(
            "Host could not set {rw}x{rh}; it is serving {aw}x{ah} ({backend})."
        ));
    }

    if display
        .get("resize")
        .and_then(|resize| resize.get("available"))
        == Some(&serde_json::Value::Bool(false))
        && let Some(explanation) = display
            .get("resize")
            .and_then(|resize| resize.get("explanation"))
            .and_then(serde_json::Value::as_str)
            .filter(|explanation| !explanation.is_empty())
    {
        notes.push(format!("Windowed resize is unavailable: {explanation}."));
    }

    notes.extend(display_scale_notes(display));

    (!notes.is_empty()).then(|| notes.join(" "))
}

/// The display line a Deck shows for an attachment: the host's own account
/// of the size ([`display_negotiation_note`]) first; else this Deck's
/// check of every display it asked for against what is served
/// ([`served_layout_note`]).
#[must_use]
pub fn attachment_display_note(
    hello: &arcen_protocol::messages::ServerHelloMsg,
    served_layout: Option<&arcen_protocol::served_layout::ServedLayoutReport>,
) -> Option<String> {
    display_negotiation_note(&hello.device_capabilities).or_else(|| {
        served_layout
            .and_then(served_layout_note)
            .map(str::to_owned)
    })
}

/// What the session says when the host did not serve the displays this
/// attachment asked for (`arcen_protocol::served_layout`); `None` when it
/// served them exactly. A host that keeps its desktop's own layout is said
/// as such, never as a match.
#[must_use]
pub fn served_layout_note(
    report: &arcen_protocol::served_layout::ServedLayoutReport,
) -> Option<&'static str> {
    use arcen_protocol::served_layout::LayoutMismatch;
    if report.matches() {
        return None;
    }
    if report.host_kept_layout {
        return Some("This desktop keeps its own layout; your displays show it as it is.");
    }
    let mismatches = &report.mismatches;
    let any = |test: fn(&LayoutMismatch) -> bool| mismatches.iter().any(test);
    Some(
        if any(|mismatch| {
            matches!(
                mismatch,
                LayoutMismatch::Omitted { .. }
                    | LayoutMismatch::Unrequested { .. }
                    | LayoutMismatch::LayoutNotApplied { .. }
                    | LayoutMismatch::Unreadable { .. }
            )
        }) {
            "The host serves a different set of displays from yours."
        } else if any(|mismatch| {
            matches!(
                mismatch,
                LayoutMismatch::Size { .. }
                    | LayoutMismatch::ScreenSize { .. }
                    | LayoutMismatch::StreamScaled { .. }
                    | LayoutMismatch::Decoded { .. }
            )
        }) {
            "The host couldn\u{2019}t match your display sizes; the picture is scaled to fit."
        } else {
            "The host arranges the displays differently from yours."
        },
    )
}

/// Report any display whose UI scale is not the one the Deck asked for.
///
/// Windows hosts publish `effective_display_scale.monitors`: per display the
/// scale the Deck asked for (`requested_scale_percent`, its Remote UI scale
/// on that display's stream), the Windows step the host applied
/// (`step_percent`; Windows has only 100, 125, 150, 175, 200, 225, 250, 300,
/// 350, 400, 450 and 500% per monitor) and what `GetDpiForMonitor` reports.
///
/// - Windows shows the step but the request has none (a 1.5x Retina stream
///   at 125% asks for 187.5%): the line names the step it shows.
/// - Windows shows something else than the host applied (or an older host
///   that applied nothing): the line names both numbers, so "it just looks
///   too big" becomes a fix in Windows Display settings.
fn display_scale_notes(display: &serde_json::Value) -> Vec<String> {
    let Some(monitors) = display
        .get("effective_display_scale")
        .and_then(|scale| scale.get("monitors"))
        .and_then(serde_json::Value::as_array)
    else {
        return Vec::new();
    };

    monitors
        .iter()
        .filter(|monitor| monitor.get("matches_requested") == Some(&serde_json::Value::Bool(false)))
        .filter_map(|monitor| {
            let requested = monitor.get("requested_scale_percent")?.as_u64()?;
            let effective = monitor.get("effective_scale_percent")?.as_u64()?;
            let step = monitor
                .get("step_percent")
                .and_then(serde_json::Value::as_u64);
            let label = monitor
                .get("client_display_id")
                .and_then(serde_json::Value::as_str)
                .filter(|id| !id.is_empty())
                .map_or_else(|| "The display".to_string(), |id| format!("Display {id}"));
            if monitor.get("matches_step") == Some(&serde_json::Value::Bool(true)) {
                let step = step.unwrap_or(effective);
                return Some(format!(
                    "{label} is at {step}%, the Windows scale nearest to the requested \
                     {requested}%."
                ));
            }
            let target = step.unwrap_or(requested);
            Some(format!(
                "{label} is showing at {effective}% instead of the requested {target}%; \
                 set it to {target}% in Windows Display settings."
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_served_layout_is_said_only_when_it_differs() {
        use arcen_protocol::served_layout::{LayoutMismatch, ServedLayoutReport};
        let mut report = ServedLayoutReport {
            requested: 2,
            served: 2,
            host_kept_layout: false,
            streams: Vec::new(),
            mismatches: Vec::new(),
        };
        assert_eq!(served_layout_note(&report), None);
        report.mismatches.push(LayoutMismatch::Size {
            display: "b".into(),
            requested: [1920, 1080],
            served: [1280, 720],
        });
        assert_eq!(
            served_layout_note(&report),
            Some(
                "The host couldn\u{2019}t match your display sizes; the picture is scaled to fit."
            )
        );
        report.host_kept_layout = true;
        assert_eq!(
            served_layout_note(&report),
            Some("This desktop keeps its own layout; your displays show it as it is.")
        );
    }

    use super::*;

    fn display_capabilities(value: serde_json::Value) -> BTreeMap<String, serde_json::Value> {
        BTreeMap::from([("display_resolution".to_string(), value)])
    }

    #[test]
    fn display_note_says_when_an_earlier_session_s_displays_were_not_put_back() {
        let capabilities = display_capabilities(serde_json::json!({
            "requested": {"width": 2560, "height": 1440},
            "applied": {"width": 2560, "height": 1440, "refresh_hz": 60},
            "restore_notice": "display_restore_pending",
            "resize": {"available": true, "explanation": ""},
        }));
        assert_eq!(
            display_negotiation_note(&capabilities).as_deref(),
            Some(RESTORE_PENDING_NOTE)
        );
        let quiet = display_capabilities(serde_json::json!({
            "requested": {"width": 2560, "height": 1440},
            "applied": {"width": 2560, "height": 1440, "refresh_hz": 60},
            "restore_notice": null,
        }));
        assert_eq!(display_negotiation_note(&quiet), None);
    }

    #[test]
    fn display_note_is_silent_when_the_host_honoured_the_request() {
        let capabilities = display_capabilities(serde_json::json!({
            "requested": {"width": 2560, "height": 1440},
            "applied": {"width": 2560, "height": 1440, "refresh_hz": 60},
            "backend": "nvidia-nvapi-exact",
            "resize": {"available": true, "explanation": ""},
        }));
        assert_eq!(display_negotiation_note(&capabilities), None);
    }

    /// The exact pier-windows.example.internal measurement (an older host
    /// that applied no scale): 200% requested on both monitors, Windows
    /// resolved 250% on one (the oversized Philips) and 100% on the other.
    /// Both must be named, with the number to set.
    #[test]
    fn display_note_names_every_monitor_windows_scaled_differently() {
        let capabilities = display_capabilities(serde_json::json!({
            "requested": {"width": 1800, "height": 1169},
            "applied": {"width": 1800, "height": 1169, "refresh_hz": 60},
            "backend": "windows-ccd",
            "resize": {"available": true, "explanation": ""},
            "effective_display_scale": {
                "source": "GetDpiForMonitor(MDT_EFFECTIVE_DPI)",
                "baseline_dpi": 96,
                "monitors": [
                    {
                        "client_display_id": "3",
                        "requested_scale_percent": 200,
                        "effective_scale_percent": 250,
                        "matches_requested": false,
                    },
                    {
                        "client_display_id": "1",
                        "requested_scale_percent": 200,
                        "effective_scale_percent": 100,
                        "matches_requested": false,
                    },
                ],
            },
        }));
        let note = display_negotiation_note(&capabilities).expect("mismatch must be reported");
        assert!(
            note.contains("Display 3"),
            "names the oversized one: {note}"
        );
        assert!(note.contains("250%"), "names what Windows applied: {note}");
        assert!(note.contains("Display 1"), "names the other one: {note}");
        assert!(note.contains("100%"), "names its applied scale: {note}");
        assert!(
            note.contains("200%"),
            "names the scale to set so the fix is actionable: {note}",
        );
    }

    /// A 1.5x Retina stream at a Remote UI scale of 125% asks for 188%;
    /// Windows has no such step and shows 175%, which the host applied.
    #[test]
    fn display_note_names_the_windows_step_a_request_snapped_to() {
        let capabilities = display_capabilities(serde_json::json!({
            "requested": {"width": 2880, "height": 1800},
            "applied": {"width": 2880, "height": 1800, "refresh_hz": 60},
            "resize": {"available": true, "explanation": ""},
            "effective_display_scale": {
                "monitors": [
                    {
                        "client_display_id": "",
                        "requested_scale_percent": 188,
                        "step_percent": 175,
                        "effective_scale_percent": 175,
                        "matches_requested": false,
                        "matches_step": true,
                    },
                ],
            },
        }));
        assert_eq!(
            display_negotiation_note(&capabilities).as_deref(),
            Some("The display is at 175%, the Windows scale nearest to the requested 188%.")
        );
    }

    #[test]
    fn display_note_names_the_step_to_set_when_windows_shows_another_scale() {
        let capabilities = display_capabilities(serde_json::json!({
            "effective_display_scale": {
                "monitors": [
                    {
                        "client_display_id": "11",
                        "requested_scale_percent": 188,
                        "step_percent": 175,
                        "effective_scale_percent": 300,
                        "matches_requested": false,
                        "matches_step": false,
                    },
                ],
            },
        }));
        assert_eq!(
            display_negotiation_note(&capabilities).as_deref(),
            Some(
                "Display 11 is showing at 300% instead of the requested 175%; set it to 175% \
                 in Windows Display settings."
            )
        );
    }

    /// A host that scaled every monitor exactly as asked must stay silent --
    /// the banner is for genuine mismatches, not a permanent fixture.
    #[test]
    fn display_note_is_silent_when_every_monitor_matched_the_requested_scale() {
        let capabilities = display_capabilities(serde_json::json!({
            "requested": {"width": 1800, "height": 1169},
            "applied": {"width": 1800, "height": 1169, "refresh_hz": 60},
            "backend": "windows-ccd",
            "resize": {"available": true, "explanation": ""},
            "effective_display_scale": {
                "monitors": [
                    {
                        "client_display_id": "1",
                        "requested_scale_percent": 200,
                        "step_percent": 200,
                        "effective_scale_percent": 200,
                        "matches_requested": true,
                        "matches_step": true,
                    },
                ],
            },
        }));
        assert_eq!(display_negotiation_note(&capabilities), None);
    }

    #[test]
    fn display_note_reports_a_size_the_host_could_not_apply() {
        let capabilities = display_capabilities(serde_json::json!({
            "requested": {"width": 2560, "height": 1440},
            "applied": {"width": 1280, "height": 800, "refresh_hz": 60},
            "backend": "change-display-settings-ex-temporary-fallback",
            "resize": {"available": true, "explanation": ""},
        }));
        let note = display_negotiation_note(&capabilities).expect("mismatch must be reported");
        assert!(
            note.contains("2560x1440"),
            "names the requested size: {note}"
        );
        assert!(note.contains("1280x800"), "names the served size: {note}");
        assert!(
            note.contains("change-display-settings-ex-temporary-fallback"),
            "names the backend: {note}"
        );
    }

    #[test]
    fn display_note_explains_why_windowed_resize_is_unavailable() {
        let capabilities = display_capabilities(serde_json::json!({
            "requested": {"width": 1920, "height": 1080},
            "applied": {"width": 1920, "height": 1080, "refresh_hz": 60},
            "backend": "change-display-settings-ex-temporary",
            "resize": {
                "available": false,
                "reason": "display_backend_cannot_retarget",
                "explanation": "display backend cannot retarget without NVAPI custom timings",
            },
        }));
        let note = display_negotiation_note(&capabilities).expect("reason must be reported");
        assert!(note.contains("Windowed resize is unavailable"), "{note}");
        assert!(note.contains("NVAPI custom timings"), "{note}");
    }

    #[test]
    fn display_note_is_silent_for_hosts_that_publish_no_display_capability() {
        assert_eq!(display_negotiation_note(&BTreeMap::new()), None);
    }
}
