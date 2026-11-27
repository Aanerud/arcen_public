// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! When a session display's Windows colour state admits capture.
//!
//! Windows 11 composes a display in one of three modes: SDR, WCG (wide
//! colour gamut, for example Auto Color Management on an SDR panel) or HDR.
//! Arcen changes exactly one thing, the user's HDR toggle on the session's own
//! targets. WCG is not SDR and not HDR, and no HDR operation leaves it: an SDR
//! session on a WCG display whose HDR toggle is off is admitted at once as
//! `WcgWithoutHdr` instead of waiting for an SDR mode nothing can produce. An
//! SDR session is never admitted while the user's HDR toggle is on. HDR
//! keeps its exact proof: the HDR toggle on, HDR supported, Advanced Colour
//! active and the HDR composition mode.
//!
//! The vocabulary is Windows' `DISPLAYCONFIG` advanced-colour report. The
//! Windows Pier (`hosts/windows/src/advanced_color.rs`) reads and sets that
//! state; every admission and restoration decision is made here, without OS
//! calls, so it is tested on every OS.

use std::time::Duration;

/// The composition mode Windows reports for a display target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompositionMode {
    /// Standard dynamic range.
    Sdr,
    /// Wide colour gamut without HDR.
    Wcg,
    /// High dynamic range.
    Hdr,
    /// A value this build does not know.
    Unknown,
}

impl CompositionMode {
    /// The shared display-truth colour mode.
    #[must_use]
    pub const fn colour_mode(self) -> crate::display_truth::ColourMode {
        use crate::display_truth::ColourMode;
        match self {
            Self::Sdr => ColourMode::Sdr,
            Self::Wcg => ColourMode::Wcg,
            Self::Hdr => ColourMode::Hdr,
            Self::Unknown => ColourMode::Unknown,
        }
    }

    /// Maps `DISPLAYCONFIG_ADVANCED_COLOR_MODE` (`0` SDR, `1` WCG, `2` HDR).
    #[must_use]
    pub fn from_raw(mode: i32) -> Self {
        match mode {
            0 => Self::Sdr,
            1 => Self::Wcg,
            2 => Self::Hdr,
            _ => Self::Unknown,
        }
    }

    #[must_use]
    /// The lower-case name used in log lines.
    pub fn name(self) -> &'static str {
        match self {
            Self::Sdr => "sdr",
            Self::Wcg => "wcg",
            Self::Hdr => "hdr",
            Self::Unknown => "unknown",
        }
    }
}

/// The parts of one target's advanced-colour report that decide admission.
// The fields mirror the independent flags of Windows' report.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColourReading {
    /// Windows offers HDR on this target.
    pub hdr_supported: bool,
    /// The user's HDR toggle (Settings › Display › Use HDR).
    pub hdr_user_enabled: bool,
    /// Advanced Colour (HDR or WCG) composition is active.
    pub advanced_color_active: bool,
    /// The user's WCG choice, for example Auto Color Management.
    pub wcg_user_enabled: bool,
    /// The composition mode Windows reports.
    pub mode: CompositionMode,
}

impl ColourReading {
    /// HDR composition proven by the report itself; the session additionally
    /// requires the user's HDR toggle and, later, the DXGI PQ proof.
    #[must_use]
    pub fn is_genuinely_hdr(self) -> bool {
        self.hdr_supported && self.advanced_color_active && self.mode == CompositionMode::Hdr
    }

    fn is_sdr(self) -> bool {
        !self.advanced_color_active && self.mode == CompositionMode::Sdr
    }
}

/// How a target was admitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmittedAs {
    /// Proven HDR composition with the user's HDR toggle on.
    Hdr,
    /// SDR composition with no HDR toggle to change.
    Sdr,
    /// An SDR session on a WCG composition with no HDR to switch off.
    WcgWithoutHdr,
}

/// What one target needs next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetStep {
    /// Capture may start on this target.
    Admitted(AdmittedAs),
    /// Set the user's HDR toggle; the report is checked again afterwards.
    SetHdr {
        /// The toggle as read.
        from_hdr: bool,
        /// The toggle the session needs.
        to_hdr: bool,
    },
    /// A transition may still be in flight; poll again within the budget.
    Wait,
}

/// The HDR toggle change a session needs on this target, if Windows offers
/// HDR on it at all. The same answer decides what is journalled for
/// restoration.
#[must_use]
pub fn hdr_toggle(reading: ColourReading, desired_hdr: bool) -> Option<(bool, bool)> {
    if !reading.hdr_supported || reading.hdr_user_enabled == desired_hdr {
        return None;
    }
    Some((reading.hdr_user_enabled, desired_hdr))
}

/// The next step for one target. `arcen_toggled` says Arcen changed this
/// target's HDR toggle during this setup, so a WCG report may be a transient
/// on the way out of HDR rather than the display's own steady state.
#[must_use]
pub fn target_step(reading: ColourReading, desired_hdr: bool, arcen_toggled: bool) -> TargetStep {
    if desired_hdr && reading.hdr_user_enabled && reading.is_genuinely_hdr() {
        return TargetStep::Admitted(AdmittedAs::Hdr);
    }
    if let Some((from_hdr, to_hdr)) = hdr_toggle(reading, desired_hdr) {
        return TargetStep::SetHdr { from_hdr, to_hdr };
    }
    // Windows can take most of the setup budget to publish HDR support after
    // an EDID change. An HDR session's budget ends in `hdr_not_activated`; an
    // SDR session never starts while the user's HDR toggle is on, because HDR
    // could compose it once support appears, with nothing journalled.
    if desired_hdr || reading.hdr_user_enabled {
        return TargetStep::Wait;
    }
    match reading.mode {
        CompositionMode::Sdr if reading.is_sdr() => TargetStep::Admitted(AdmittedAs::Sdr),
        // Turning HDR off on a display with the user's WCG choice lands in
        // WCG; without that choice, WCG straight after Arcen's change may be
        // a step on the way to SDR.
        CompositionMode::Wcg if !arcen_toggled || reading.wcg_user_enabled => {
            TargetStep::Admitted(AdmittedAs::WcgWithoutHdr)
        }
        _ => TargetStep::Wait,
    }
}

/// One required target as read in one poll.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TargetObservation {
    /// The target's report in this poll.
    pub reading: ColourReading,
    /// Arcen changed this target's HDR toggle earlier in this setup.
    pub arcen_toggled: bool,
    /// Arcen gave this target an HDR-capable display identity this session
    /// ([`DisplayIdentity::may_gain_hdr`]). Windows may then take most of the
    /// setup budget to publish HDR support, so an HDR session keeps waiting
    /// for it. Without that, the target's own EDID decides and "no HDR
    /// support" is final.
    pub arcen_display_identity: bool,
}

/// Where a session display's identity (its EDID) came from this session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayIdentity {
    /// The display's own EDID: a monitor or a dummy plug Arcen did not write.
    Own,
    /// Arcen wrote an SDR EDID for an exact timing (the multi-display
    /// NVIDIA exact path). It claims no HDR, so it is final like the
    /// display's own.
    ArcenSdrEdid,
    /// Arcen wrote an HDR EDID (an NVIDIA headless output, or an exact timing
    /// written for an HDR request).
    ArcenHdrEdid,
    /// Arcen created the display (`IddCx`).
    ArcenCreated,
}

/// The EDID this session wrote on a display, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdidWrite {
    /// The session wrote no EDID on it.
    None,
    /// An SDR EDID (the multi-display NVIDIA exact path, or an SDR session's
    /// headless display).
    Sdr,
    /// An EDID that declares HDR10.
    Hdr,
}

impl DisplayIdentity {
    /// Whether Windows may still publish HDR support for the display during
    /// setup, so an HDR session waits for it instead of serving Grading.
    #[must_use]
    pub const fn may_gain_hdr(self) -> bool {
        matches!(self, Self::ArcenHdrEdid | Self::ArcenCreated)
    }

    /// A session display's identity from what this session did to it and the
    /// EDID it carries now, never from configuration: a host configured for
    /// NVIDIA headless whose display NVAPI could not rebuild (a `GeForce`, a
    /// display another adapter owns) keeps its own EDID, and its "no HDR
    /// support" is final.
    ///
    /// An HDR EDID the session wrote counts until the display is seen to
    /// carry a monitor's own EDID instead (the write did not land there); an
    /// unread EDID, or an Arcen EDID Windows has not re-enumerated yet, keeps
    /// the wait.
    #[must_use]
    pub fn of_session_display(created: bool, wrote: EdidWrite, edid_now: Option<&[u8]>) -> Self {
        if created {
            return Self::ArcenCreated;
        }
        match wrote {
            EdidWrite::Hdr if edid_now.is_some_and(|edid| !crate::edid::is_arcen(edid)) => {
                Self::Own
            }
            EdidWrite::Hdr => Self::ArcenHdrEdid,
            EdidWrite::Sdr => Self::ArcenSdrEdid,
            EdidWrite::None => Self::Own,
        }
    }
}

/// How long an HDR session reads "no HDR support" on a target whose display
/// identity Arcen did not write before treating it as final: long enough to
/// ride out a report taken mid mode change, short of the setup budget.
pub const HDR_SUPPORT_SETTLE: Duration = Duration::from_secs(2);

/// The required targets (indices) an HDR session cannot be served on: an
/// HDR session that has waited [`HDR_SUPPORT_SETTLE`] and reads targets
/// whose own display (not an EDID Arcen wrote) offers no HDR. Empty when
/// the session is SDR, still settling, or every target can still become HDR.
///
/// A dummy plug or an SDR monitor never offers HDR, so waiting the whole
/// setup budget for it and refusing the session (`hdr_not_activated`) helps
/// no one: the caller serves Grading instead, with the reason.
#[must_use]
pub fn hdr_unsupported_targets(
    desired_hdr: bool,
    observations: &[Option<TargetObservation>],
    waited: Duration,
) -> Vec<usize> {
    if !desired_hdr || waited < HDR_SUPPORT_SETTLE {
        return Vec::new();
    }
    observations
        .iter()
        .enumerate()
        .filter_map(|(index, seen)| {
            seen.as_ref()
                .filter(|seen| !seen.reading.hdr_supported && !seen.arcen_display_identity)
                .map(|_| index)
        })
        .collect()
}

/// The outcome of one setup poll over every required target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetupPoll {
    /// Every required target is admitted, in required-target order.
    Admitted(Vec<AdmittedAs>),
    /// Apply these HDR toggles (required-target index, from, to) and poll
    /// again. An empty list means waiting for Windows to finish a change.
    Pending {
        /// `(required-target index, from_hdr, to_hdr)` for each change.
        toggles: Vec<(usize, bool, bool)>,
    },
    /// An SDR session goes on without proof: its targets could not be read.
    ContinueUnverified(String),
    /// An HDR session is refused: its exact targets could not be read.
    Refuse(String),
}

/// Decides one poll. `observations` holds, per required target, its reading
/// or `None` when no active display path matched it.
#[must_use]
pub fn setup_poll(
    desired_hdr: bool,
    observations: Result<&[Option<TargetObservation>], &str>,
) -> SetupPoll {
    let observations = match observations {
        Ok(observations) => observations,
        Err(error) if desired_hdr => return SetupPoll::Refuse(error.to_string()),
        Err(error) => return SetupPoll::ContinueUnverified(error.to_string()),
    };
    if observations.is_empty() {
        let message = "Advanced Color session requires at least one display target".to_string();
        return SetupPoll::Refuse(message);
    }
    let missing = observations.iter().filter(|seen| seen.is_none()).count();
    if missing != 0 {
        let message =
            format!("Advanced Color setup could not find {missing} required display target(s)");
        return if desired_hdr {
            SetupPoll::Refuse(message)
        } else {
            SetupPoll::ContinueUnverified(message)
        };
    }
    let mut admitted = Vec::with_capacity(observations.len());
    let mut toggles = Vec::new();
    let mut pending = false;
    for (index, seen) in observations.iter().flatten().enumerate() {
        match target_step(seen.reading, desired_hdr, seen.arcen_toggled) {
            TargetStep::Admitted(how) => admitted.push(how),
            TargetStep::SetHdr { from_hdr, to_hdr } => {
                pending = true;
                toggles.push((index, from_hdr, to_hdr));
            }
            TargetStep::Wait => pending = true,
        }
    }
    if pending {
        SetupPoll::Pending { toggles }
    } else {
        SetupPoll::Admitted(admitted)
    }
}

/// The HDR toggle change that puts a journalled target back, if any.
#[must_use]
pub fn restore_toggle(reading: ColourReading, original_hdr_enabled: bool) -> Option<(bool, bool)> {
    (reading.hdr_user_enabled != original_hdr_enabled)
        .then_some((reading.hdr_user_enabled, original_hdr_enabled))
}

#[cfg(test)]
mod tests {
    use super::*;

    // The audit finding: a host configured for NVIDIA headless whose display
    // NVAPI did not rebuild (a GeForce, or a display another adapter owns)
    // was counted as Arcen-created from configuration alone, so an HDR
    // session waited the whole setup budget and was refused instead of
    // serving Grading.
    #[test]
    fn the_session_display_identity_follows_what_the_session_wrote_not_configuration() {
        use crate::edid::{EdidRequest, generate, generate_hdr10};
        let request = EdidRequest {
            width: 1920,
            height: 1080,
            refresh_hz: 60,
            width_mm: 0.0,
            height_mm: 0.0,
            scale: 1.0,
            product_id: 1,
            serial: 0,
            color: None,
        };
        let hdr_edid = generate_hdr10(request).expect("edid");
        let sdr_edid = generate(request).expect("edid");
        let mut plug = sdr_edid;
        plug[8..10].copy_from_slice(&[0x10, 0xac]);
        let identity = DisplayIdentity::of_session_display;
        assert_eq!(
            identity(false, EdidWrite::None, Some(&plug)),
            DisplayIdentity::Own
        );
        assert_eq!(identity(false, EdidWrite::None, None), DisplayIdentity::Own);
        assert_eq!(
            identity(false, EdidWrite::Hdr, Some(&hdr_edid)),
            DisplayIdentity::ArcenHdrEdid
        );
        // Windows has not re-enumerated the new EDID yet, or it is unread.
        assert_eq!(
            identity(false, EdidWrite::Hdr, Some(&sdr_edid)),
            DisplayIdentity::ArcenHdrEdid
        );
        assert_eq!(
            identity(false, EdidWrite::Hdr, None),
            DisplayIdentity::ArcenHdrEdid
        );
        // The write did not land on this display: its own EDID decides.
        assert_eq!(
            identity(false, EdidWrite::Hdr, Some(&plug)),
            DisplayIdentity::Own
        );
        assert_eq!(
            identity(false, EdidWrite::Sdr, Some(&sdr_edid)),
            DisplayIdentity::ArcenSdrEdid
        );
        assert_eq!(
            identity(true, EdidWrite::None, None),
            DisplayIdentity::ArcenCreated
        );
    }

    // The lab regression: a Match My Layout display on the multi-display
    // NVIDIA exact path got an SDR EDID from Arcen, was counted as an Arcen
    // identity, and an HDR session waited the whole budget and was refused
    // (`hdr_not_activated`) instead of serving Grading.
    #[test]
    fn only_an_hdr_identity_arcen_gave_keeps_an_hdr_session_waiting() {
        assert!(!DisplayIdentity::Own.may_gain_hdr());
        assert!(!DisplayIdentity::ArcenSdrEdid.may_gain_hdr());
        assert!(DisplayIdentity::ArcenHdrEdid.may_gain_hdr());
        assert!(DisplayIdentity::ArcenCreated.may_gain_hdr());
        let sdr_edid = TargetObservation {
            reading: reading(false, false, false, false, CompositionMode::Sdr),
            arcen_toggled: false,
            arcen_display_identity: DisplayIdentity::ArcenSdrEdid.may_gain_hdr(),
        };
        assert_eq!(
            hdr_unsupported_targets(true, &[Some(sdr_edid)], HDR_SUPPORT_SETTLE),
            vec![0]
        );
    }

    // The Windows Pier's budget is 25 s polled every 250 ms.
    const BUDGET_POLLS: usize = 100;

    #[allow(clippy::fn_params_excessive_bools)]
    fn reading(
        hdr_supported: bool,
        hdr_user_enabled: bool,
        advanced_color_active: bool,
        wcg_user_enabled: bool,
        mode: CompositionMode,
    ) -> ColourReading {
        ColourReading {
            hdr_supported,
            hdr_user_enabled,
            advanced_color_active,
            wcg_user_enabled,
            mode,
        }
    }

    fn sdr_display() -> ColourReading {
        reading(true, false, false, false, CompositionMode::Sdr)
    }

    fn hdr_display_on() -> ColourReading {
        reading(true, true, true, false, CompositionMode::Hdr)
    }

    /// A WCG panel that cannot do HDR, with the user's WCG choice on.
    fn wcg_without_hdr_support() -> ColourReading {
        reading(false, false, true, true, CompositionMode::Wcg)
    }

    /// An HDR-capable panel composing WCG with the user's HDR toggle off.
    fn wcg_with_hdr_off() -> ColourReading {
        reading(true, false, true, true, CompositionMode::Wcg)
    }

    // A present target, as `setup_poll` takes it; `None` is a missing one.
    #[allow(clippy::unnecessary_wraps)]
    fn seen(reading: ColourReading, arcen_toggled: bool) -> Option<TargetObservation> {
        Some(TargetObservation {
            reading,
            arcen_toggled,
            arcen_display_identity: false,
        })
    }

    /// Runs the setup loop as the Pier's `set_required_targets` does:
    /// each `SetHdr` makes Windows answer with `after_toggle` from the next poll.
    /// Returns the final poll and how many polls it took, or `None` when the
    /// budget ran out.
    fn run_setup(
        desired_hdr: bool,
        initial: ColourReading,
        after_toggle: impl Fn(usize) -> ColourReading,
    ) -> (Option<SetupPoll>, usize) {
        let mut current = initial;
        let mut toggled_at = None;
        for poll in 1..=BUDGET_POLLS {
            if let Some(at) = toggled_at {
                current = after_toggle(poll - at);
            }
            let observations = [seen(current, toggled_at.is_some())];
            match setup_poll(desired_hdr, Ok(&observations)) {
                SetupPoll::Pending { toggles } => {
                    if !toggles.is_empty() && toggled_at.is_none() {
                        toggled_at = Some(poll);
                    }
                }
                done => return (Some(done), poll),
            }
        }
        (None, BUDGET_POLLS)
    }

    #[test]
    fn sdr_session_on_a_wcg_display_without_hdr_is_admitted_at_the_first_poll() {
        for display in [wcg_without_hdr_support(), wcg_with_hdr_off()] {
            let (outcome, polls) = run_setup(false, display, |_| display);
            assert_eq!(
                outcome,
                Some(SetupPoll::Admitted(vec![AdmittedAs::WcgWithoutHdr])),
                "{display:?} used {polls} polls"
            );
            assert_eq!(polls, 1, "{display:?} must not wait for an SDR mode");
        }
    }

    #[test]
    fn wcg_is_admitted_as_wcg_not_reported_as_sdr() {
        assert_eq!(
            target_step(wcg_with_hdr_off(), false, false),
            TargetStep::Admitted(AdmittedAs::WcgWithoutHdr)
        );
        assert_eq!(
            target_step(sdr_display(), false, false),
            TargetStep::Admitted(AdmittedAs::Sdr)
        );
        assert_eq!(hdr_toggle(wcg_without_hdr_support(), false), None);
        assert_eq!(hdr_toggle(wcg_with_hdr_off(), false), None);
    }

    #[test]
    fn sdr_session_switches_the_users_hdr_off_then_waits_for_the_change() {
        let (outcome, polls) = run_setup(false, hdr_display_on(), |since| match since {
            1 => reading(true, false, true, false, CompositionMode::Hdr),
            2 => reading(true, false, true, false, CompositionMode::Wcg),
            _ => sdr_display(),
        });
        assert_eq!(outcome, Some(SetupPoll::Admitted(vec![AdmittedAs::Sdr])));
        assert_eq!(polls, 4, "one toggle, HDR and WCG transients, then SDR");
    }

    #[test]
    fn hdr_switched_off_onto_the_users_wcg_choice_is_admitted() {
        let mut hdr_with_wcg_choice = hdr_display_on();
        hdr_with_wcg_choice.wcg_user_enabled = true;
        let (outcome, polls) = run_setup(false, hdr_with_wcg_choice, |_| wcg_with_hdr_off());
        assert_eq!(
            outcome,
            Some(SetupPoll::Admitted(vec![AdmittedAs::WcgWithoutHdr]))
        );
        assert_eq!(polls, 2);
    }

    #[test]
    fn hdr_toggle_left_on_by_the_user_is_switched_off_even_when_composition_is_sdr() {
        let enabled_but_inactive = reading(true, true, false, false, CompositionMode::Sdr);
        assert_eq!(
            target_step(enabled_but_inactive, false, false),
            TargetStep::SetHdr {
                from_hdr: true,
                to_hdr: false
            }
        );
        let enabled_on_wcg = reading(true, true, true, true, CompositionMode::Wcg);
        assert_eq!(
            target_step(enabled_on_wcg, false, false),
            TargetStep::SetHdr {
                from_hdr: true,
                to_hdr: false
            }
        );
    }

    #[test]
    fn hdr_session_on_a_genuine_hdr_display_is_admitted_without_change() {
        assert_eq!(
            target_step(hdr_display_on(), true, false),
            TargetStep::Admitted(AdmittedAs::Hdr)
        );
        let (outcome, polls) = run_setup(true, sdr_display(), |since| match since {
            1..=3 => reading(true, true, false, false, CompositionMode::Sdr),
            _ => hdr_display_on(),
        });
        assert_eq!(outcome, Some(SetupPoll::Admitted(vec![AdmittedAs::Hdr])));
        assert_eq!(polls, 5);
    }

    #[test]
    fn hdr_session_never_accepts_wcg_or_an_unproven_hdr_report() {
        let user_on_wcg = reading(true, true, true, false, CompositionMode::Wcg);
        let user_on_inactive = reading(true, true, false, false, CompositionMode::Hdr);
        let unsupported_hdr_mode = reading(false, true, true, false, CompositionMode::Hdr);
        for display in [
            user_on_wcg,
            user_on_inactive,
            unsupported_hdr_mode,
            wcg_without_hdr_support(),
        ] {
            assert_eq!(target_step(display, true, false), TargetStep::Wait);
            let (outcome, polls) = run_setup(true, display, |_| display);
            assert_eq!(outcome, None, "{display:?} must reach hdr_not_activated");
            assert_eq!(polls, BUDGET_POLLS);
        }
        // HDR off but supported is switched on, not admitted.
        assert_eq!(
            target_step(wcg_with_hdr_off(), true, false),
            TargetStep::SetHdr {
                from_hdr: false,
                to_hdr: true
            }
        );
    }

    #[test]
    fn wcg_straight_after_arcen_switched_hdr_off_waits_without_the_users_wcg_choice() {
        let transient = reading(true, false, true, false, CompositionMode::Wcg);
        assert_eq!(target_step(transient, false, true), TargetStep::Wait);
        assert_eq!(
            target_step(transient, false, false),
            TargetStep::Admitted(AdmittedAs::WcgWithoutHdr)
        );
    }

    #[test]
    fn sdr_waits_while_the_users_hdr_toggle_is_on_even_before_windows_offers_hdr() {
        // Windows can publish HDR support late; until then the toggle cannot
        // be switched off, and admitting would let HDR compose an SDR session
        // with nothing journalled to restore.
        let unsupported_on_sdr = reading(false, true, false, false, CompositionMode::Sdr);
        let unsupported_on_wcg = reading(false, true, true, true, CompositionMode::Wcg);
        let displays = [unsupported_on_sdr, unsupported_on_wcg];
        assert_eq!(
            displays.map(|display| hdr_toggle(display, false)),
            [None, None]
        );
        assert_eq!(
            displays.map(|display| target_step(display, false, false)),
            [TargetStep::Wait, TargetStep::Wait]
        );
        assert_eq!(
            displays.map(|display| setup_poll(false, Ok(&[seen(display, false)]))),
            [
                SetupPoll::Pending { toggles: vec![] },
                SetupPoll::Pending { toggles: vec![] }
            ]
        );
        // Once Windows offers HDR, the toggle is switched off and journalled.
        let (outcome, polls) = run_setup(false, unsupported_on_sdr, |_| sdr_display());
        assert_eq!(outcome, None, "no toggle while HDR is unsupported");
        assert_eq!(polls, BUDGET_POLLS);
        let mut supported_later = unsupported_on_sdr;
        supported_later.hdr_supported = true;
        assert_eq!(
            target_step(supported_later, false, false),
            TargetStep::SetHdr {
                from_hdr: true,
                to_hdr: false
            }
        );
    }

    #[test]
    fn still_composing_hdr_or_an_unknown_mode_is_never_admitted_for_sdr() {
        let hdr_after_off = reading(true, false, true, false, CompositionMode::Hdr);
        let unknown = reading(true, false, true, false, CompositionMode::Unknown);
        assert_eq!(target_step(hdr_after_off, false, false), TargetStep::Wait);
        assert_eq!(target_step(unknown, false, false), TargetStep::Wait);
    }

    #[test]
    fn unreadable_or_missing_targets_refuse_hdr_and_continue_sdr_at_once() {
        assert!(matches!(
            setup_poll(true, Err("query failed")),
            SetupPoll::Refuse(message) if message == "query failed"
        ));
        assert!(matches!(
            setup_poll(false, Err("query failed")),
            SetupPoll::ContinueUnverified(_)
        ));
        let one_missing = [seen(hdr_display_on(), false), None];
        assert!(matches!(
            setup_poll(true, Ok(&one_missing)),
            SetupPoll::Refuse(message) if message.contains("could not find 1")
        ));
        assert!(matches!(
            setup_poll(false, Ok(&one_missing)),
            SetupPoll::ContinueUnverified(_)
        ));
        assert!(matches!(setup_poll(false, Ok(&[])), SetupPoll::Refuse(_)));
    }

    #[test]
    fn every_required_target_must_be_admitted_and_only_toggleable_ones_are_set() {
        let observations = [
            seen(wcg_without_hdr_support(), false),
            seen(hdr_display_on(), false),
            seen(sdr_display(), false),
        ];
        assert_eq!(
            setup_poll(false, Ok(&observations)),
            SetupPoll::Pending {
                toggles: vec![(1, true, false)]
            }
        );
        let observations = [
            seen(wcg_without_hdr_support(), false),
            seen(sdr_display(), true),
        ];
        assert_eq!(
            setup_poll(false, Ok(&observations)),
            SetupPoll::Admitted(vec![AdmittedAs::WcgWithoutHdr, AdmittedAs::Sdr])
        );
    }

    #[test]
    fn restoration_puts_back_only_what_arcen_changed() {
        // Nothing is journalled for a WCG display: there is nothing to undo.
        assert_eq!(hdr_toggle(wcg_without_hdr_support(), false), None);
        assert_eq!(hdr_toggle(wcg_with_hdr_off(), false), None);
        // The user's HDR, switched off for an SDR session, comes back.
        assert_eq!(hdr_toggle(hdr_display_on(), false), Some((true, false)));
        assert_eq!(restore_toggle(sdr_display(), true), Some((false, true)));
        // HDR switched on for an HDR session goes back off, and an already
        // restored target is left alone.
        assert_eq!(restore_toggle(hdr_display_on(), false), Some((true, false)));
        assert_eq!(restore_toggle(hdr_display_on(), true), None);
        assert_eq!(restore_toggle(wcg_with_hdr_off(), false), None);
    }

    #[test]
    fn composition_modes_map_from_the_sdk_values() {
        assert_eq!(CompositionMode::from_raw(0), CompositionMode::Sdr);
        assert_eq!(CompositionMode::from_raw(1), CompositionMode::Wcg);
        assert_eq!(CompositionMode::from_raw(2), CompositionMode::Hdr);
        assert_eq!(CompositionMode::from_raw(7), CompositionMode::Unknown);
    }

    fn no_hdr_display() -> ColourReading {
        reading(false, false, false, false, CompositionMode::Sdr)
    }

    /// A dummy plug offers no HDR: once settled, an HDR session stops
    /// waiting for it, instead of spending the setup budget and refusing.
    #[test]
    fn a_display_without_hdr_support_is_final_once_settled() {
        let observations = [seen(sdr_display(), false), seen(no_hdr_display(), false)];
        assert_eq!(
            hdr_unsupported_targets(true, &observations, HDR_SUPPORT_SETTLE),
            vec![1]
        );
        assert!(
            hdr_unsupported_targets(true, &observations, Duration::from_millis(500)).is_empty(),
            "a report read mid mode change is not final"
        );
        assert!(
            hdr_unsupported_targets(false, &observations, HDR_SUPPORT_SETTLE).is_empty(),
            "an SDR session never asks"
        );
    }

    /// A target whose EDID Arcen wrote may still publish HDR support: the
    /// session keeps waiting for it, as before.
    #[test]
    fn a_display_whose_edid_arcen_wrote_keeps_waiting_for_hdr_support() {
        let written = Some(TargetObservation {
            reading: no_hdr_display(),
            arcen_toggled: false,
            arcen_display_identity: true,
        });
        assert!(hdr_unsupported_targets(true, &[written], Duration::from_secs(20)).is_empty());
        assert_eq!(
            setup_poll(true, Ok(&[written])),
            SetupPoll::Pending { toggles: vec![] }
        );
    }
}
