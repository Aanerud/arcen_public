// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The frame rate a session is served at, and why.
//!
//! Auto and Speed follow the Deck display's refresh: a 60 Hz display gets
//! 60 fps, a 120 Hz one 120, capped only by what the host proves (the
//! encoder's limits, the capture source, an administrator's `video.fps`) and
//! lowered at run time when the host cannot hold it
//! ([`HostRateGovernor`]) or the link cannot carry it (the rate
//! controller's Detail shedding). Grading and HDR keep their fixed fidelity
//! rate. Every Pier resolves the rate here, so the same Deck on the same
//! display gets the same answer from every host, and the answer carries its
//! reason ([`FrameRateBasis`]) into READY, the hello and the stream-start
//! record.
//!
//! A Deck keeps sending the preset's legacy rate (`PipelineContract::max_fps`:
//! 30 for Auto, 60 for Speed) in `QualitySettings.max_fps`, so a Pier that
//! predates display-following serves exactly what it did. A current Pier
//! reads a follow-display preset's request as a ceiling only when it is
//! below that legacy rate: an explicit "less, please" (a metered link, a
//! command-line `--max-fps`).

use super::{PipelineId, pipeline_contract};

/// The rate assumed for a Deck that named no display refresh.
pub const DISPLAY_REFRESH_FALLBACK_HZ: u32 = 60;

/// The highest session rate any contract serves (the wire's own bound on
/// `QualitySettings.max_fps`).
pub const MAX_SESSION_FPS: u32 = 240;

/// The most a copying capture route without a rate measurement serves a
/// follow-display contract at: the rate Auto was always served at there.
/// Above it, the host's own qualification must prove the route
/// (`capture_admission`'s `copy_path_not_rate_qualified`).
pub const UNQUALIFIED_COPY_FPS: u32 = 30;

/// How a pipeline contract chooses its frame rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameRatePolicy {
    /// The Deck display's refresh, raised to `floor_fps` and lowered to
    /// `ceiling_fps`.
    FollowDisplay { floor_fps: u32, ceiling_fps: u32 },
    /// A fixed rate (the fidelity contracts and the software fallback).
    Fixed { fps: u32 },
}

impl FrameRatePolicy {
    /// The highest rate this policy ever serves.
    #[must_use]
    pub const fn ceiling_fps(self) -> u32 {
        match self {
            Self::FollowDisplay { ceiling_fps, .. } => ceiling_fps,
            Self::Fixed { fps } => fps,
        }
    }

    /// The lowest rate this policy serves before an explicit request, a host
    /// limit or the link lowers it.
    #[must_use]
    pub const fn floor_fps(self) -> u32 {
        match self {
            Self::FollowDisplay { floor_fps, .. } => floor_fps,
            Self::Fixed { fps } => fps,
        }
    }

    /// Whether the rate follows the Deck display.
    #[must_use]
    pub const fn follows_display(self) -> bool {
        matches!(self, Self::FollowDisplay { .. })
    }
}

/// What decided a session's frame rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameRateBasis {
    /// The Deck display's refresh.
    Display,
    /// The Deck named no refresh; [`DISPLAY_REFRESH_FALLBACK_HZ`] assumed.
    DisplayUnknown,
    /// Speed's motion floor raised a slower display's rate.
    MotionFloor,
    /// The contract's own rate (fixed) or ceiling.
    Contract,
    /// The Deck asked for less than the contract.
    Request,
    /// The administrator's `video.fps`.
    Administrator,
    /// The encoder backend's limit at this size.
    Encoder,
    /// What the capture source proves it delivers (a refresh it can be
    /// created at, a qualification measurement).
    CaptureSource,
    /// The host encoded below the rate while the source delivered it
    /// ([`HostRateGovernor`]).
    HostShortfall,
}

impl FrameRateBasis {
    /// Every basis, in a stable order.
    pub const ALL: &'static [Self] = &[
        Self::Display,
        Self::DisplayUnknown,
        Self::MotionFloor,
        Self::Contract,
        Self::Request,
        Self::Administrator,
        Self::Encoder,
        Self::CaptureSource,
        Self::HostShortfall,
    ];

    /// Stable log and wire token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Display => "display",
            Self::DisplayUnknown => "display_unknown",
            Self::MotionFloor => "motion_floor",
            Self::Contract => "contract",
            Self::Request => "request",
            Self::Administrator => "administrator",
            Self::Encoder => "encoder",
            Self::CaptureSource => "capture_source",
            Self::HostShortfall => "host_shortfall",
        }
    }

    /// Parses a stable token.
    #[must_use]
    pub fn from_token(value: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|basis| basis.token() == value)
    }

    /// Whether a host limit (not the display, the contract or the Deck)
    /// holds the rate down.
    #[must_use]
    pub const fn is_host_limit(self) -> bool {
        matches!(
            self,
            Self::Administrator | Self::Encoder | Self::CaptureSource | Self::HostShortfall
        )
    }

    /// A short phrase for a Deck's statistics ("limited by …").
    #[must_use]
    pub const fn describe(self) -> &'static str {
        match self {
            Self::Display => "your display's refresh",
            Self::DisplayUnknown => "an assumed 60 Hz display",
            Self::MotionFloor => "Speed's 60 fps floor",
            Self::Contract => "the preset",
            Self::Request => "this Deck's request",
            Self::Administrator => "the host administrator's limit",
            Self::Encoder => "the host's encoder",
            Self::CaptureSource => "the host's capture",
            Self::HostShortfall => "the host keeping up",
        }
    }
}

/// A session's frame rate with its reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionFrameRate {
    /// Frames per second the session is served at.
    pub fps: u32,
    /// The Deck display's refresh the rate followed, when the Deck named one.
    pub display_refresh_hz: Option<u32>,
    /// What decided `fps`.
    pub basis: FrameRateBasis,
}

impl SessionFrameRate {
    /// The rate at most `cap` (never below one), naming `basis` when `cap`
    /// lowers it.
    #[must_use]
    pub fn narrow(self, cap: u32, basis: FrameRateBasis) -> Self {
        let cap = cap.max(1);
        if cap < self.fps {
            Self {
                fps: cap,
                basis,
                ..self
            }
        } else {
            self
        }
    }

    /// The rate a hello states (`ServerHelloMsg::stream_rate`).
    #[must_use]
    pub fn wire(self) -> arcen_protocol::messages::StreamRateMsg {
        arcen_protocol::messages::StreamRateMsg {
            fps: self.fps,
            display_refresh_hz: self.display_refresh_hz,
            basis: self.basis.token().to_owned(),
        }
    }

    /// Whether the session runs below the Deck display's refresh.
    #[must_use]
    pub fn below_display(&self) -> bool {
        self.display_refresh_hz
            .is_some_and(|refresh| self.fps < refresh)
    }
}

/// The inputs of [`resolve_session_frame_rate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameRateRequest {
    /// The named pipeline the Deck asked for; `None` for exact/custom axes.
    pub pipeline: Option<PipelineId>,
    /// The Deck's `QualitySettings.max_fps`.
    pub requested_fps: u32,
    /// The served Deck display's refresh (`AuthResponse::display_refresh_hz`).
    pub display_refresh_hz: Option<u32>,
    /// The administrator's `video.fps`, when one is configured.
    pub administrator_max_fps: Option<u32>,
}

/// The frame rate a session starts at, before the host narrows it to what
/// its encoder and capture prove ([`SessionFrameRate::narrow`]).
#[must_use]
pub fn resolve_session_frame_rate(request: FrameRateRequest) -> SessionFrameRate {
    let requested = request.requested_fps.clamp(1, MAX_SESSION_FPS);
    let display = request
        .display_refresh_hz
        .filter(|refresh| *refresh > 0)
        .map(|refresh| refresh.min(MAX_SESSION_FPS));
    let contract = request.pipeline.map(pipeline_contract);
    let resolved = match contract {
        Some(contract) => match contract.frame_rate {
            FrameRatePolicy::FollowDisplay {
                floor_fps,
                ceiling_fps,
            } => {
                let (fps, basis) = display.map_or(
                    (DISPLAY_REFRESH_FALLBACK_HZ, FrameRateBasis::DisplayUnknown),
                    |refresh| (refresh, FrameRateBasis::Display),
                );
                let (fps, basis) = if fps < floor_fps {
                    (floor_fps, FrameRateBasis::MotionFloor)
                } else {
                    (fps, basis)
                };
                let followed = SessionFrameRate {
                    fps,
                    display_refresh_hz: display,
                    basis,
                }
                .narrow(ceiling_fps, FrameRateBasis::Contract);
                if requested < contract.max_fps {
                    followed.narrow(requested, FrameRateBasis::Request)
                } else {
                    followed
                }
            }
            FrameRatePolicy::Fixed { fps } => SessionFrameRate {
                fps,
                display_refresh_hz: display,
                basis: FrameRateBasis::Contract,
            }
            .narrow(requested, FrameRateBasis::Request),
        },
        None => SessionFrameRate {
            fps: requested,
            display_refresh_hz: display,
            basis: FrameRateBasis::Request,
        },
    };
    match request.administrator_max_fps {
        Some(ceiling) => resolved.narrow(ceiling, FrameRateBasis::Administrator),
        None => resolved,
    }
}

/// The stream rate a host states once its encoder or capture admitted
/// `served_fps`: the resolved `frame_rate` lowered to it, naming `basis`
/// when that lowers it. A session without a resolved request (an exact
/// developer stream) states the served rate as the Deck's request.
#[must_use]
pub fn served_stream_rate(
    frame_rate: Option<SessionFrameRate>,
    served_fps: u32,
    basis: FrameRateBasis,
) -> SessionFrameRate {
    frame_rate.map_or(
        SessionFrameRate {
            fps: served_fps.max(1),
            display_refresh_hz: None,
            basis: FrameRateBasis::Request,
        },
        |rate| rate.narrow(served_fps, basis),
    )
}

/// Seconds a [`HostRateGovernor`] judges before it decides.
pub const GOVERNOR_WINDOWS: u32 = 5;

/// Of those, how many encoded below the rate make it step down.
pub const GOVERNOR_SHORT_WINDOWS: u32 = 3;

/// Seconds at a lowered rate before the first probe back up.
pub const GOVERNOR_FIRST_HOLD_SECS: u32 = 20;

/// The longest hold between probes after repeated failures.
pub const GOVERNOR_MAX_HOLD_SECS: u32 = 320;

/// The lowest rate the governor steps an Auto session down to.
pub const GOVERNOR_MIN_FPS: u32 = 10;

/// The share of a rate an encoder must hold for a second to count as
/// holding it (jitter of a host that holds the rate does not count).
const HOLD_PERCENT: u32 = 95;

const fn held_floor(fps: u32) -> u32 {
    fps.saturating_mul(HOLD_PERCENT).div_ceil(100)
}

/// Why a [`HostRateGovernor`] moved the rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GovernorReason {
    /// The source delivered at the rate and the encoder did not keep up.
    Shortfall {
        /// Lowest encoded rate among the short seconds.
        encoded_fps: u32,
        /// Delivered rate in that second.
        source_fps: u32,
    },
    /// The lowered rate held long enough to try the next one up.
    Probe,
}

impl GovernorReason {
    /// The encoded and delivered counts a shortfall was judged on.
    #[must_use]
    pub const fn counts(self) -> Option<(u32, u32)> {
        match self {
            Self::Shortfall {
                encoded_fps,
                source_fps,
            } => Some((encoded_fps, source_fps)),
            Self::Probe => None,
        }
    }

    /// Stable log token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Shortfall { .. } => "host_shortfall",
            Self::Probe => "probe",
        }
    }
}

/// A rate change the governor asks the encoder for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GovernorStep {
    pub fps: u32,
    pub previous_fps: u32,
    pub reason: GovernorReason,
}

/// Holds a follow-display session at the rate the host proves it encodes,
/// on the host's own once-a-second counts (encoded access units and the
/// frames the capture source delivered), never on what a Deck received.
///
/// Only seconds in which the source delivered at least 95 % of the rate the
/// encoder was asked for are judged: a still desktop or a 24 fps video is
/// content, not a slow host, and a rate the link's Detail shedding lowered is
/// the link's. When three of five judged seconds encode below that, the rate
/// steps down to the next even divisor of the session rate (60 → 30 → 20,
/// 120 → 60 → 40) at or below what the host encoded, so pacing stays even.
/// After a hold it probes one step back up; a probe that fails doubles the
/// hold (up to [`GOVERNOR_MAX_HOLD_SECS`]), one that holds resets it.
#[derive(Debug, Clone)]
pub struct HostRateGovernor {
    session_fps: u32,
    floor_fps: u32,
    current_fps: u32,
    judged: u32,
    short: u32,
    lowest: Option<(u32, u32)>,
    hold_secs: u32,
    held_for: u32,
    probing: bool,
    last_asked: Option<u32>,
}

impl HostRateGovernor {
    /// A governor for a session served at `session_fps` that never steps
    /// below `floor_fps` (Speed keeps its motion floor; Auto passes
    /// [`GOVERNOR_MIN_FPS`]).
    #[must_use]
    pub fn new(session_fps: u32, floor_fps: u32) -> Self {
        let session_fps = session_fps.max(1);
        Self {
            session_fps,
            floor_fps: floor_fps.clamp(1, session_fps),
            current_fps: session_fps,
            judged: 0,
            short: 0,
            lowest: None,
            hold_secs: GOVERNOR_FIRST_HOLD_SECS,
            held_for: 0,
            probing: false,
            last_asked: None,
        }
    }

    /// The governor of a session served under `pipeline` at `session_fps`,
    /// or `None` for a contract whose rate does not follow the display.
    #[must_use]
    pub fn for_pipeline(pipeline: PipelineId, session_fps: u32) -> Option<Self> {
        let policy = pipeline_contract(pipeline).frame_rate;
        if !policy.follows_display() {
            return None;
        }
        let floor = match pipeline {
            PipelineId::Speed => policy.floor_fps(),
            _ => GOVERNOR_MIN_FPS,
        };
        Some(Self::new(session_fps, floor))
    }

    /// The rate the encoder should run at now.
    #[must_use]
    pub const fn fps(&self) -> u32 {
        self.current_fps
    }

    /// Whether the governor holds the session below its rate.
    #[must_use]
    pub const fn lowered(&self) -> bool {
        self.current_fps < self.session_fps
    }

    /// The even divisors of the session rate the governor may run at,
    /// highest first, never below the floor.
    fn steps(&self) -> impl Iterator<Item = u32> + '_ {
        (1..=12)
            .map(|divisor| self.session_fps / divisor)
            .filter(|fps| *fps >= self.floor_fps && *fps > 0)
    }

    fn step_below(&self, encoded: u32) -> u32 {
        self.steps()
            .find(|fps| *fps < self.current_fps && *fps <= encoded.max(1))
            .or_else(|| self.steps().filter(|fps| *fps < self.current_fps).last())
            .unwrap_or(self.current_fps)
    }

    fn step_above(&self) -> u32 {
        self.steps()
            .filter(|fps| *fps > self.current_fps)
            .last()
            .unwrap_or(self.current_fps)
    }

    fn reset_window(&mut self) {
        self.judged = 0;
        self.short = 0;
        self.lowest = None;
    }

    /// Counts one second; returns a step when the rate should change.
    ///
    /// `asked_fps` is the rate the encoder was last asked to run at: the
    /// link's Detail shedding may ask for less than the governor allows, and
    /// an encoder that encodes what it was asked is holding. A host's counts
    /// for a second need not line up with its requests, so the second is
    /// judged against the lower of this and the previous request: a rate
    /// the link just raised is not yet the host's to keep.
    pub fn observe(
        &mut self,
        encoded_fps: u32,
        source_fps: Option<u32>,
        asked_fps: u32,
    ) -> Option<GovernorStep> {
        if self.lowered() && !self.probing {
            self.held_for = self.held_for.saturating_add(1);
            if self.held_for >= self.hold_secs {
                let previous = self.current_fps;
                self.current_fps = self.step_above();
                self.held_for = 0;
                self.probing = true;
                self.reset_window();
                return Some(GovernorStep {
                    fps: self.current_fps,
                    previous_fps: previous,
                    reason: GovernorReason::Probe,
                });
            }
        }
        let asked = asked_fps.max(1);
        let judged_rate = self.last_asked.map_or(asked, |last| last.min(asked));
        self.last_asked = Some(asked);
        let source = source_fps?;
        let floor = held_floor(self.current_fps.min(judged_rate));
        if source < floor {
            return None;
        }
        self.judged += 1;
        if encoded_fps < floor {
            self.short += 1;
            if self.lowest.is_none_or(|(lowest, _)| encoded_fps < lowest) {
                self.lowest = Some((encoded_fps, source));
            }
        }
        if self.short >= GOVERNOR_SHORT_WINDOWS {
            let (encoded, source) = self.lowest.unwrap_or((encoded_fps, source));
            let next = self.step_below(encoded);
            if self.probing {
                self.hold_secs = self.hold_secs.saturating_mul(2).min(GOVERNOR_MAX_HOLD_SECS);
            }
            self.probing = false;
            self.held_for = 0;
            self.reset_window();
            if next == self.current_fps {
                return None;
            }
            let previous = self.current_fps;
            self.current_fps = next;
            return Some(GovernorStep {
                fps: next,
                previous_fps: previous,
                reason: GovernorReason::Shortfall {
                    encoded_fps: encoded,
                    source_fps: source,
                },
            });
        }
        if self.judged >= GOVERNOR_WINDOWS {
            if self.probing {
                self.probing = false;
                self.hold_secs = GOVERNOR_FIRST_HOLD_SECS;
            }
            self.reset_window();
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(
        pipeline: Option<PipelineId>,
        requested: u32,
        display: Option<u32>,
    ) -> FrameRateRequest {
        FrameRateRequest {
            pipeline,
            requested_fps: requested,
            display_refresh_hz: display,
            administrator_max_fps: None,
        }
    }

    #[test]
    fn auto_follows_the_deck_display_whatever_legacy_rate_it_sent() {
        for (display, fps) in [(60, 60), (120, 120), (144, 144), (30, 30), (24, 24)] {
            let rate =
                resolve_session_frame_rate(request(Some(PipelineId::Auto), 30, Some(display)));
            assert_eq!(rate.fps, fps, "{display} Hz");
            assert_eq!(rate.basis, FrameRateBasis::Display);
            assert!(!rate.below_display());
        }
        let unknown = resolve_session_frame_rate(request(Some(PipelineId::Auto), 30, None));
        assert_eq!(unknown.fps, DISPLAY_REFRESH_FALLBACK_HZ);
        assert_eq!(unknown.basis, FrameRateBasis::DisplayUnknown);
    }

    #[test]
    fn speed_keeps_its_sixty_fps_floor_and_follows_faster_displays() {
        let slow = resolve_session_frame_rate(request(Some(PipelineId::Speed), 60, Some(30)));
        assert_eq!((slow.fps, slow.basis), (60, FrameRateBasis::MotionFloor));
        let fast = resolve_session_frame_rate(request(Some(PipelineId::Speed), 60, Some(144)));
        assert_eq!((fast.fps, fast.basis), (144, FrameRateBasis::Display));
    }

    #[test]
    fn an_explicit_lower_request_and_the_administrator_still_cap() {
        let metered = resolve_session_frame_rate(request(Some(PipelineId::Auto), 15, Some(60)));
        assert_eq!((metered.fps, metered.basis), (15, FrameRateBasis::Request));
        assert!(metered.below_display());
        let capped = resolve_session_frame_rate(FrameRateRequest {
            administrator_max_fps: Some(60),
            ..request(Some(PipelineId::Auto), 30, Some(120))
        });
        assert_eq!(
            (capped.fps, capped.basis),
            (60, FrameRateBasis::Administrator)
        );
        let encoder = capped.narrow(30, FrameRateBasis::Encoder);
        assert_eq!((encoder.fps, encoder.basis), (30, FrameRateBasis::Encoder));
        assert_eq!(encoder.narrow(45, FrameRateBasis::CaptureSource), encoder);
    }

    #[test]
    fn fidelity_and_custom_requests_keep_their_rates() {
        let grading = resolve_session_frame_rate(request(Some(PipelineId::Grading), 30, Some(120)));
        assert_eq!((grading.fps, grading.basis), (30, FrameRateBasis::Contract));
        let custom = resolve_session_frame_rate(request(None, 50, Some(120)));
        assert_eq!((custom.fps, custom.basis), (50, FrameRateBasis::Request));
        let over = resolve_session_frame_rate(request(None, 500, None));
        assert_eq!(over.fps, MAX_SESSION_FPS);
    }

    #[test]
    fn basis_tokens_round_trip() {
        for basis in FrameRateBasis::ALL {
            assert_eq!(FrameRateBasis::from_token(basis.token()), Some(*basis));
        }
    }

    fn run(
        governor: &mut HostRateGovernor,
        seconds: u32,
        encoded: u32,
        source: u32,
    ) -> Vec<GovernorStep> {
        (0..seconds)
            .filter_map(|_| {
                let asked = governor.fps();
                governor.observe(encoded, Some(source), asked)
            })
            .collect()
    }

    #[test]
    fn a_host_that_cannot_encode_the_rate_steps_to_an_even_divisor_and_probes_back() {
        let mut governor = HostRateGovernor::new(60, GOVERNOR_MIN_FPS);
        let steps = run(&mut governor, 3, 41, 60);
        assert_eq!(
            steps,
            vec![GovernorStep {
                fps: 30,
                previous_fps: 60,
                reason: GovernorReason::Shortfall {
                    encoded_fps: 41,
                    source_fps: 60
                }
            }]
        );
        assert!(governor.lowered());
        // Holding 30 while the source still offers 60: no change until the hold ends.
        assert!(run(&mut governor, GOVERNOR_FIRST_HOLD_SECS - 1, 30, 60).is_empty());
        let probe = governor.observe(30, Some(60), 30).expect("probe");
        assert_eq!((probe.fps, probe.reason), (60, GovernorReason::Probe));
        // The probe fails again (its first second is judged at the rate it
        // left): back to 30, and the next hold is longer.
        let again = run(&mut governor, 4, 40, 60);
        assert_eq!(again.len(), 1);
        assert_eq!(again[0].fps, 30);
        assert!(run(&mut governor, GOVERNOR_FIRST_HOLD_SECS * 2 - 1, 30, 60).is_empty());
        assert_eq!(
            governor.observe(30, Some(60), 30).map(|step| step.fps),
            Some(60)
        );
        // This time it holds: five judged seconds at the rate keep it.
        assert!(run(&mut governor, GOVERNOR_WINDOWS * 4, 60, 60).is_empty());
        assert!(!governor.lowered());
    }

    #[test]
    fn still_content_and_a_holding_host_change_nothing() {
        let mut governor = HostRateGovernor::new(120, GOVERNOR_MIN_FPS);
        assert!(run(&mut governor, 30, 5, 5).is_empty());
        assert!(run(&mut governor, 30, 118, 120).is_empty());
        assert!(governor.observe(0, None, 120).is_none());
        // The link asked for 40: encoding 40 of a 120 source is holding.
        assert!(
            (0..10)
                .filter_map(|_| governor.observe(40, Some(120), 40))
                .next()
                .is_none()
        );
        assert_eq!(governor.fps(), 120);
        let steps = run(&mut governor, 4, 70, 120);
        assert_eq!(steps.last().map(|step| step.fps), Some(60));
    }

    #[test]
    fn a_link_raising_the_rate_is_not_a_host_shortfall() {
        let mut governor = HostRateGovernor::new(60, GOVERNOR_MIN_FPS);
        // Detail climbs back 20 -> 30 -> 40 -> 50 -> 60; each second's
        // counts still show the previous request's rate.
        let mut encoded = 20;
        for asked in [30, 40, 50, 60, 60, 60, 60, 60] {
            assert_eq!(governor.observe(encoded, Some(60), asked), None, "{asked}");
            encoded = asked;
        }
        assert!(!governor.lowered());
    }

    #[test]
    fn speed_never_steps_below_its_floor_and_fixed_contracts_are_not_governed() {
        let mut speed = HostRateGovernor::for_pipeline(PipelineId::Speed, 60).expect("speed");
        assert!(run(&mut speed, 10, 20, 60).is_empty());
        assert_eq!(speed.fps(), 60);
        let mut fast = HostRateGovernor::for_pipeline(PipelineId::Speed, 120).expect("speed");
        assert_eq!(
            run(&mut fast, 3, 20, 120).last().map(|step| step.fps),
            Some(60)
        );
        assert!(HostRateGovernor::for_pipeline(PipelineId::Grading, 30).is_none());
        let mut auto = HostRateGovernor::for_pipeline(PipelineId::Auto, 60).expect("auto");
        assert_eq!(
            run(&mut auto, 3, 3, 60).last().map(|step| step.fps),
            Some(GOVERNOR_MIN_FPS)
        );
    }
}
