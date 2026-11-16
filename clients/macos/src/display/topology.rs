// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Local multi-monitor-v1 requested-topology builder and the Separate Spaces
//! preflight gate.
//!
//! This module produces the *domain-level* [`arcen_media::RequestedMonitorTopology`]
//! from the Mac's currently active `NSScreen`/`CGDisplay` layout. It is the
//! single source of truth Deck uses both to negotiate multi-monitor-v1 with a
//! host that offers it (see `crate::transport::multi_monitor`) and to hash the
//! requested layout into reconnect identity (see `crate::reconnect`).
//!
//! Building the topology itself only needs CoreGraphics (`CGDisplay`), which is
//! not main-thread-restricted — mirroring `crate::display::enumerate`. Only the
//! `NSScreen.screensHaveSeparateSpaces` preflight check needs the main thread,
//! which is why it is a separate, narrowly scoped function.

use std::fmt;

use arcen_media::RequestedMonitorTopology;

pub use arcen_deck_core::display::topology::{
    build_requested_topology_from, TopologyPreflightError,
};

/// Exact System Settings guidance shown when Match My Layout needs multiple
/// displays but macOS is not configured to give each one its own fullscreen
/// Space. There is no supported borderless/windowed multi-monitor fallback;
/// the user must change the setting (or fall back to Primary Display Only).
pub const SEPARATE_SPACES_GUIDANCE: &str =
    "Match My Layout needs each display to have its own Space. Open System Settings > \
     Desktop & Dock, scroll to Mission Control, and turn on \"Displays have separate Spaces,\" \
     then reconnect. Primary Display Only does not require this setting.";

/// Failure gating Match My Layout on the local Separate Spaces preference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchLayoutPreflightError {
    /// `NSScreen.screensHaveSeparateSpaces` is `false` with more than one
    /// active display selected for Match My Layout.
    SeparateSpacesDisabled { display_count: usize },
    /// The check could not run because it was not attempted on the main
    /// thread. Callers must always invoke the live check from the main
    /// thread; this variant exists so a threading bug fails loudly instead of
    /// silently assuming a value.
    IndeterminateMainThread,
}

impl fmt::Display for MatchLayoutPreflightError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SeparateSpacesDisabled { display_count } => {
                write!(
                    formatter,
                    "{display_count} displays active but Separate Spaces is off: {SEPARATE_SPACES_GUIDANCE}"
                )
            }
            Self::IndeterminateMainThread => formatter.write_str(
                "Separate Spaces could not be checked (not on the main thread); try again",
            ),
        }
    }
}

impl std::error::Error for MatchLayoutPreflightError {}

/// Safe wrapper for `NSScreen.screensHaveSeparateSpaces`.
///
/// Returns `None` when the check cannot be performed (off the main thread);
/// `NSScreen` enumeration is main-thread-only on AppKit, and callers must
/// treat an indeterminate result as a real gate failure rather than guessing.
pub fn screens_have_separate_spaces() -> Option<bool> {
    #[cfg(target_os = "macos")]
    {
        macos::screens_have_separate_spaces()
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

/// Evaluates the Match My Layout Separate Spaces gate against an explicit
/// display count and (already read) Separate Spaces value.
///
/// Pure and independent of AppKit so it is directly unit-testable; the live
/// call site is [`evaluate_match_layout_preflight_live`].
#[must_use]
pub fn evaluate_match_layout_preflight(
    local_display_count: usize,
    separate_spaces: Option<bool>,
) -> Result<(), MatchLayoutPreflightError> {
    if local_display_count <= 1 {
        return Ok(());
    }
    match separate_spaces {
        Some(true) => Ok(()),
        Some(false) => Err(MatchLayoutPreflightError::SeparateSpacesDisabled {
            display_count: local_display_count,
        }),
        None => Err(MatchLayoutPreflightError::IndeterminateMainThread),
    }
}

/// Live Match My Layout Separate Spaces gate, reading the current main-thread
/// AppKit state. Call this before showing credentials or connecting.
pub fn evaluate_match_layout_preflight_live(
    local_display_count: usize,
) -> Result<(), MatchLayoutPreflightError> {
    evaluate_match_layout_preflight(local_display_count, screens_have_separate_spaces())
}

/// Builds the validated local requested multi-monitor-v1 topology from every
/// active display, up to [`arcen_media::MAX_MULTI_MONITOR_COUNT`].
///
/// `use_notch_area`/`hidpi` match the Settings → Displays choices
/// (`crate::display::presentation_size_for_display`): they control whether the
/// per-monitor stream size subtracts the safe area and whether it is backing
/// pixels or points. Logical arrangement (`x`/`y`/`logical_width`/
/// `logical_height`) always stays in the point-space `CGDisplay` bounds so the
/// requested layout matches the Mac's actual desktop arrangement regardless of
/// those presentation choices.
///
/// # Errors
///
/// Returns [`TopologyPreflightError::NoActiveDisplays`] when nothing is
/// enumerated, [`TopologyPreflightError::TooManyDisplays`] for more than
/// [`arcen_media::MAX_MULTI_MONITOR_COUNT`] active displays, and
/// [`TopologyPreflightError::InvalidTopology`] when the gathered facts fail a
/// domain invariant (should not happen for a real macOS layout).
pub fn build_requested_topology(
    use_notch_area: bool,
    hidpi_mask: u8,
) -> Result<RequestedMonitorTopology, TopologyPreflightError> {
    #[cfg(target_os = "macos")]
    {
        macos::build_requested_topology(use_notch_area, hidpi_mask)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (use_notch_area, hidpi_mask);
        Err(TopologyPreflightError::NoActiveDisplays)
    }
}

/// Local monitor count regardless of match-layout intent, used to decide
/// whether the Separate Spaces gate and multi-monitor-v1 negotiation apply at
/// all. Cheap: it reuses the already-enumerated legacy roster.
#[must_use]
pub fn local_display_count() -> usize {
    crate::display::enumerate().len()
}

#[cfg(target_os = "macos")]
mod macos {
    use objc2_app_kit::NSScreen;
    use objc2_foundation::MainThreadMarker;

    use arcen_media::RequestedMonitorTopology;

    use super::TopologyPreflightError;

    pub(super) fn screens_have_separate_spaces() -> Option<bool> {
        let mtm = MainThreadMarker::new()?;
        Some(NSScreen::screensHaveSeparateSpaces(mtm))
    }

    pub(super) fn build_requested_topology(
        use_notch_area: bool,
        hidpi_mask: u8,
    ) -> Result<RequestedMonitorTopology, TopologyPreflightError> {
        let legacy = crate::display::enumerate();
        if legacy.is_empty() {
            return Err(TopologyPreflightError::NoActiveDisplays);
        }

        let mut displays = Vec::with_capacity(legacy.len());
        for client_monitor in legacy {
            // The same single validated reading `crate::display::enumerate`
            // built this roster entry from; nothing here re-derives a scale
            // or a logical rectangle of its own.
            let metrics = crate::display::display_metrics(client_monitor.id).ok_or(
                TopologyPreflightError::DisplayUnavailable(client_monitor.id),
            )?;
            displays.push((client_monitor, metrics));
        }
        super::build_requested_topology_from(&displays, use_notch_area, hidpi_mask)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_display_never_needs_separate_spaces() {
        assert_eq!(evaluate_match_layout_preflight(0, None), Ok(()));
        assert_eq!(evaluate_match_layout_preflight(1, None), Ok(()));
        assert_eq!(evaluate_match_layout_preflight(1, Some(false)), Ok(()));
    }

    #[test]
    fn multi_display_requires_separate_spaces_enabled() {
        assert_eq!(evaluate_match_layout_preflight(2, Some(true)), Ok(()));
        assert_eq!(evaluate_match_layout_preflight(4, Some(true)), Ok(()));
    }

    #[test]
    fn multi_display_with_separate_spaces_off_fails_with_guidance() {
        assert_eq!(
            evaluate_match_layout_preflight(2, Some(false)),
            Err(MatchLayoutPreflightError::SeparateSpacesDisabled { display_count: 2 })
        );
        let error = evaluate_match_layout_preflight(3, Some(false)).unwrap_err();
        assert!(error.to_string().contains("Displays have separate Spaces"));
        assert!(error.to_string().contains("System Settings"));
    }

    #[test]
    fn multi_display_with_indeterminate_check_fails_rather_than_guessing() {
        assert_eq!(
            evaluate_match_layout_preflight(2, None),
            Err(MatchLayoutPreflightError::IndeterminateMainThread)
        );
    }

    #[test]
    fn preflight_guidance_names_the_exact_settings_path() {
        assert!(SEPARATE_SPACES_GUIDANCE.contains("System Settings"));
        assert!(SEPARATE_SPACES_GUIDANCE.contains("Desktop & Dock"));
        assert!(SEPARATE_SPACES_GUIDANCE.contains("Mission Control"));
        assert!(SEPARATE_SPACES_GUIDANCE.contains("Displays have separate Spaces"));
    }

    /// Headless CI has zero or one display; when running on a real multi-
    /// display Mac this additionally proves the builder produces a topology
    /// consistent with the legacy roster used for compatibility auth.
    #[test]
    fn build_requested_topology_matches_legacy_enumeration_or_is_empty() {
        let legacy = crate::display::enumerate();
        match build_requested_topology(false, 0) {
            Ok(topology) => {
                assert_eq!(topology.monitors().len(), legacy.len());
                assert!(topology.monitors().len() <= arcen_media::MAX_MULTI_MONITOR_COUNT);
                assert_eq!(
                    topology
                        .monitors()
                        .iter()
                        .filter(|m| m.monitor().primary)
                        .count(),
                    1
                );
            }
            Err(TopologyPreflightError::NoActiveDisplays) => assert!(legacy.is_empty()),
            Err(other) => panic!("unexpected topology preflight error: {other}"),
        }
    }

    /// Whatever the real machine's live refresh rates are, every monitor's
    /// advertised `refresh_hz` in the built topology must be pinned to
    /// [`crate::display::PINNED_MAX_REFRESH_HZ`] -- the exact same ceiling
    /// `primary_presentation_size` already applies for a legacy
    /// primary-only pinned session -- so a host can never see the
    /// multi-monitor-v1 sidecar and the legacy roster disagree on the same
    /// physical display's advertised refresh.
    #[test]
    fn build_requested_topology_pins_every_monitors_refresh_to_the_shared_ceiling() {
        match build_requested_topology(false, 0) {
            Ok(topology) => {
                for monitor in topology.monitors() {
                    assert!(
                        monitor.monitor().refresh_hz <= crate::display::PINNED_MAX_REFRESH_HZ,
                        "every monitor's advertised refresh must be pinned to the shared \
                         ceiling, got {}",
                        monitor.monitor().refresh_hz,
                    );
                }
            }
            Err(TopologyPreflightError::NoActiveDisplays) => {}
            Err(other) => panic!("unexpected topology preflight error: {other}"),
        }
    }
}
