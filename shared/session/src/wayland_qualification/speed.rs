//! Speed's rate evidence: which output refresh a Speed session needs, what
//! rate its capture is admitted on, and whether that rests on a measurement
//! of this exact output or is inferred from a larger one.
//!
//! Every number comes from a qualification session of this host's exact
//! stack ([`super::SourceRateMeasurement`]). The decision picks, in order:
//!
//! 1. a **measured** rate: the same size, output refresh, codec and capture
//!    route, at the lowest candidate refresh that qualifies;
//! 2. an **inferred** rate: a qualifying measurement at the same output
//!    refresh, codec and route whose output is at least as wide *and* at
//!    least as tall as the request (so never more pixels, never a wider
//!    line than the encoder proved), the smallest such anchor, again at the
//!    lowest refresh. Inference never goes up in size and never crosses a
//!    refresh, codec or route. It is labelled `inferred` wherever it is
//!    reported, and the Pier's runtime guard still watches the encoder.
//!
//! A measured rate that reaches the floor (`speed_minimum_fps`, 95 % of the
//! contract ceiling) qualifies the contract ceiling itself: the floor exists
//! so that jitter in a source that holds the rate does not disqualify it, so
//! the same jitter must not clamp the served rate either.

use std::fmt::{Display, Formatter};

use serde::{Deserialize, Serialize};

use super::{CaptureRoute, SourceRateMeasurement};
use crate::wayland_session::VirtualOutputRequest;

/// A Deck display a qualification measures Speed for: the stream size and
/// the display's refresh rate (the session's output refresh is decided from
/// it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualifyOutput {
    /// Width.
    pub width: u32,
    /// Height.
    pub height: u32,
    /// The Deck display's refresh rate.
    pub display_refresh_hz: u32,
}

impl QualifyOutput {
    /// A display a headless compositor can be asked for.
    #[must_use]
    pub fn new(width: u32, height: u32, display_refresh_hz: u32) -> Option<Self> {
        VirtualOutputRequest::new(width, height, display_refresh_hz)
            .ok()
            .map(|_| Self {
                width,
                height,
                display_refresh_hz,
            })
    }

    /// Parses `WIDTHxHEIGHT[@HZ]` (60 Hz when omitted).
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        let (size, refresh) = value.split_once('@').unwrap_or((value, "60"));
        let (width, height) = size.split_once('x')?;
        let digits = |text: &str| !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit());
        if !digits(width) || !digits(height) || !digits(refresh) {
            return None;
        }
        Self::new(
            width.parse().ok()?,
            height.parse().ok()?,
            refresh.parse().ok()?,
        )
    }

    /// The display a measurement was taken for.
    #[must_use]
    pub const fn of(measurement: &SourceRateMeasurement) -> Self {
        Self {
            width: measurement.width,
            height: measurement.height,
            display_refresh_hz: measurement.display_refresh_hz,
        }
    }
}

impl Display for QualifyOutput {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{}x{}@{}",
            self.width, self.height, self.display_refresh_hz
        )
    }
}

/// The anchors a default qualification measures (60 Hz Deck displays):
/// 1080p, a 16:10 laptop panel class and 4K. Inference covers every request
/// no wider and no taller than a qualifying anchor.
pub const SPEED_QUALIFICATION_LADDER: [QualifyOutput; 3] = [
    QualifyOutput {
        width: 1920,
        height: 1080,
        display_refresh_hz: 60,
    },
    QualifyOutput {
        width: 2560,
        height: 1600,
        display_refresh_hz: 60,
    },
    QualifyOutput {
        width: 3840,
        height: 2160,
        display_refresh_hz: 60,
    },
];

/// Most displays one qualification measures (about a minute each).
pub const MAX_QUALIFY_OUTPUTS: usize = 10;

/// Codec tokens a qualification measures Speed with, in the order the
/// shared Speed codec ladder prefers them; a host whose encoder lacks one
/// records no rate for it.
pub const QUALIFY_CODECS: [&str; 2] = ["h265", "h264"];

/// Slots of a full qualification kept for displays Decks asked Speed for,
/// so a host with many earlier sizes still measures what Decks use now.
pub const REQUESTED_RESERVED_SLOTS: usize = 4;

/// The displays a qualification measures, each once, at most `cap`: the
/// operator's, the default ladder, up to [`REQUESTED_RESERVED_SLOTS`] of
/// those Decks asked Speed for without evidence (most recent first), the
/// previous record's that are worth measuring again
/// ([`outputs_worth_remeasuring`]), then the rest of the requested ones.
/// Sizes beyond the cap are returned second so the caller can keep them for
/// the next qualification.
#[must_use]
pub fn qualification_outputs(
    operator: &[QualifyOutput],
    previous: &[QualifyOutput],
    requested: &[QualifyOutput],
    ladder: &[QualifyOutput],
    cap: usize,
) -> (Vec<QualifyOutput>, Vec<QualifyOutput>) {
    let reserved = requested.len().min(REQUESTED_RESERVED_SLOTS);
    let mut chosen: Vec<QualifyOutput> = Vec::new();
    let mut dropped = Vec::new();
    for output in operator
        .iter()
        .chain(ladder)
        .chain(&requested[..reserved])
        .chain(previous)
        .chain(&requested[reserved..])
    {
        if chosen.contains(output) || dropped.contains(output) {
            continue;
        }
        if chosen.len() < cap {
            chosen.push(*output);
        } else {
            dropped.push(*output);
        }
    }
    (chosen, dropped)
}

/// The previous record's displays worth measuring again: those with a
/// measurement that held `minimum_fps` and that no larger qualifying
/// measurement of the same output refresh, codec and route covers. A display
/// that only failed is not carried (a Deck asking for it again puts it back
/// in the requested list); a covered one is served by inference.
#[must_use]
pub fn outputs_worth_remeasuring(
    rates: &[SourceRateMeasurement],
    minimum_fps: u32,
) -> Vec<QualifyOutput> {
    let held = |rate: &SourceRateMeasurement| {
        rate.evidence == RateEvidence::Measured
            && rate
                .sustained_source_fps
                .is_some_and(|fps| fps >= minimum_fps)
    };
    let mut outputs: Vec<QualifyOutput> = Vec::new();
    for rate in rates.iter().filter(|rate| held(rate)) {
        let covered = rates.iter().filter(|other| held(other)).any(|other| {
            other.refresh_hz == rate.refresh_hz
                && other.codec == rate.codec
                && other.route == rate.route
                && (other.width, other.height) != (rate.width, rate.height)
                && contains((other.width, other.height), (rate.width, rate.height))
        });
        let output = QualifyOutput::of(rate);
        if !covered && !outputs.contains(&output) {
            outputs.push(output);
        }
    }
    outputs
}

/// What a Speed rate rests on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum RateEvidence {
    /// Measured on exactly this output.
    #[default]
    Measured,
    /// Inferred from the measurement of a larger output (`anchor`).
    Inferred {
        /// Anchor width.
        anchor_width: u32,
        /// Anchor height.
        anchor_height: u32,
    },
}

impl RateEvidence {
    /// Stable token: `measured`, or `inferred:<W>x<H>`.
    #[must_use]
    pub fn token(&self) -> String {
        match self {
            Self::Measured => "measured".to_owned(),
            Self::Inferred {
                anchor_width,
                anchor_height,
            } => format!("inferred:{anchor_width}x{anchor_height}"),
        }
    }

    /// Whether this is an inference.
    #[must_use]
    pub const fn is_inferred(&self) -> bool {
        matches!(self, Self::Inferred { .. })
    }
}

impl Display for RateEvidence {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.token())
    }
}

/// A Speed request on a Wayland session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpeedRequest<'a> {
    /// Stream width.
    pub width: u32,
    /// Stream height.
    pub height: u32,
    /// Encoder codec token (`h264`, `h265`, `av1`).
    pub codec: &'a str,
    /// The capture route the session will use.
    pub route: CaptureRoute,
    /// Output refresh rates the session may run at, any order.
    pub candidate_refreshes: &'a [u32],
    /// Speed's contract ceiling, frames per second.
    pub ceiling_fps: u32,
    /// The floor a measurement must reach (`speed_minimum_fps`).
    pub minimum_fps: u32,
}

/// How a Speed request is served.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpeedDecision {
    /// The output refresh the session needs.
    pub output_refresh_hz: u32,
    /// The rate the capture is admitted on (`capture-rate=`).
    pub capture_rate: u32,
    /// The sustained rate the evidence measured.
    pub measured_fps: u32,
    /// Measured here, or inferred from a larger output.
    pub evidence: RateEvidence,
}

/// Measurements of `request`'s codec and route at `refresh_hz`, each with
/// whether it held the floor. Inferred entries are never evidence.
fn measured<'r>(
    rates: &'r [SourceRateMeasurement],
    request: &SpeedRequest<'_>,
    refresh_hz: u32,
) -> impl Iterator<Item = (&'r SourceRateMeasurement, Option<u32>)> {
    let codec = request.codec.to_owned();
    let route = request.route;
    let minimum = request.minimum_fps;
    rates
        .iter()
        .filter(move |rate| {
            rate.refresh_hz == refresh_hz
                && rate.codec == codec
                && rate.route == route
                && rate.evidence == RateEvidence::Measured
        })
        .map(move |rate| {
            (
                rate,
                rate.sustained_source_fps.filter(|fps| *fps >= minimum),
            )
        })
}

/// Whether `outer` is at least as wide and as tall as `inner`.
const fn contains(outer: (u32, u32), inner: (u32, u32)) -> bool {
    outer.0 >= inner.0 && outer.1 >= inner.1
}

/// The inference at one refresh, or `None`. This host's own "no" always
/// wins: a failed measurement of a size the request contains (the request
/// itself included) refuses inference, and so does one that lies between
/// the request and the anchor (it contains the request and the anchor
/// contains it) — a larger output that held Speed says nothing over a
/// closer one that did not.
fn infer_at<'r>(
    rates: &'r [SourceRateMeasurement],
    request: &SpeedRequest<'_>,
    refresh_hz: u32,
) -> Option<(&'r SourceRateMeasurement, u32)> {
    let size = (request.width, request.height);
    let failed = measured(rates, request, refresh_hz)
        .filter(|(_, held)| held.is_none())
        .map(|(rate, _)| (rate.width, rate.height))
        .collect::<Vec<_>>();
    if failed.iter().any(|failure| contains(size, *failure)) {
        return None;
    }
    let (anchor, fps) = measured(rates, request, refresh_hz)
        .filter_map(|(rate, held)| held.map(|fps| (rate, fps)))
        .filter(|(rate, _)| contains((rate.width, rate.height), size))
        .min_by_key(|(rate, fps)| {
            (
                u64::from(rate.width) * u64::from(rate.height),
                std::cmp::Reverse(*fps),
            )
        })?;
    let anchor_size = (anchor.width, anchor.height);
    if failed
        .iter()
        .any(|failure| contains(*failure, size) && contains(anchor_size, *failure))
    {
        return None;
    }
    Some((anchor, fps))
}

/// Decides how `request` is served from `rates`, or `None` when no
/// measurement covers it (Speed is then visibly served as Auto).
#[must_use]
pub fn resolve_speed(
    rates: &[SourceRateMeasurement],
    request: &SpeedRequest<'_>,
) -> Option<SpeedDecision> {
    let mut refreshes = request.candidate_refreshes.to_vec();
    refreshes.sort_unstable();
    refreshes.dedup();
    let decision = |refresh_hz, measured_fps, evidence| SpeedDecision {
        output_refresh_hz: refresh_hz,
        capture_rate: request.ceiling_fps.max(measured_fps),
        measured_fps,
        evidence,
    };
    for &refresh_hz in &refreshes {
        if let Some(fps) = measured(rates, request, refresh_hz)
            .filter(|(rate, _)| rate.width == request.width && rate.height == request.height)
            .filter_map(|(_, held)| held)
            .max()
        {
            return Some(decision(refresh_hz, fps, RateEvidence::Measured));
        }
    }
    for &refresh_hz in &refreshes {
        if let Some((anchor, fps)) = infer_at(rates, request, refresh_hz) {
            return Some(decision(
                refresh_hz,
                fps,
                RateEvidence::Inferred {
                    anchor_width: anchor.width,
                    anchor_height: anchor.height,
                },
            ));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rate(
        width: u32,
        height: u32,
        refresh_hz: u32,
        codec: &str,
        fps: Option<u32>,
    ) -> SourceRateMeasurement {
        SourceRateMeasurement {
            width,
            height,
            display_refresh_hz: 60,
            refresh_hz,
            codec: codec.to_owned(),
            route: CaptureRoute::GpuConvert,
            sustained_source_fps: fps,
            seconds: 8,
            evidence: RateEvidence::Measured,
        }
    }

    fn request<'a>(
        width: u32,
        height: u32,
        codec: &'a str,
        refreshes: &'a [u32],
    ) -> SpeedRequest<'a> {
        SpeedRequest {
            width,
            height,
            codec,
            route: CaptureRoute::GpuConvert,
            candidate_refreshes: refreshes,
            ceiling_fps: 60,
            minimum_fps: 57,
        }
    }

    #[test]
    fn a_measured_output_is_served_at_the_lowest_refresh_that_qualifies() {
        let rates = [
            rate(1920, 1080, 60, "h265", Some(59)),
            rate(1920, 1080, 120, "h265", Some(60)),
        ];
        let decision = resolve_speed(&rates, &request(1920, 1080, "h265", &[120, 60])).unwrap();
        assert_eq!(decision.output_refresh_hz, 60);
        assert_eq!(decision.evidence, RateEvidence::Measured);
        // 59 holds the floor: it qualifies the ceiling, not a 59 fps clamp.
        assert_eq!(decision.capture_rate, 60);
        assert_eq!(decision.measured_fps, 59);
        // Below the floor at 60 Hz: the paced 120 Hz output is used.
        let slow_60 = [
            rate(1920, 1080, 60, "h265", Some(41)),
            rate(1920, 1080, 120, "h265", Some(60)),
        ];
        let decision = resolve_speed(&slow_60, &request(1920, 1080, "h265", &[60, 120])).unwrap();
        assert_eq!(decision.output_refresh_hz, 120);
        // A refresh the session cannot run at is never used.
        assert_eq!(
            resolve_speed(&slow_60, &request(1920, 1080, "h265", &[60])),
            None
        );
    }

    #[test]
    fn inference_only_goes_down_in_size_and_never_across_codec_route_or_refresh() {
        let rates = [
            rate(2560, 1600, 60, "h265", Some(60)),
            rate(3840, 2160, 60, "h265", Some(51)),
            rate(3840, 2160, 60, "h264", Some(60)),
        ];
        let inferred = resolve_speed(&rates, &request(1800, 1130, "h265", &[60])).unwrap();
        assert_eq!(
            inferred.evidence,
            RateEvidence::Inferred {
                anchor_width: 2560,
                anchor_height: 1600
            }
        );
        assert_eq!(inferred.evidence.token(), "inferred:2560x1600");
        assert_eq!(inferred.output_refresh_hz, 60);
        assert_eq!(inferred.capture_rate, 60);
        // 3024x1964 fits only inside 4K, whose HEVC rate is below the floor.
        assert_eq!(
            resolve_speed(&rates, &request(3024, 1964, "h265", &[60])),
            None
        );
        // H.264 at 4K qualifies it for H.264 only.
        assert!(resolve_speed(&rates, &request(3024, 1964, "h264", &[60])).is_some());
        // Wider than every anchor (even with fewer pixels): never inferred.
        assert_eq!(
            resolve_speed(&rates, &request(5120, 1080, "h264", &[60])),
            None
        );
        // Another refresh, route or codec: never inferred.
        assert_eq!(
            resolve_speed(&rates, &request(1800, 1130, "h265", &[120])),
            None
        );
        assert_eq!(
            resolve_speed(&rates, &request(1800, 1130, "av1", &[60])),
            None
        );
        let host_copy = SpeedRequest {
            route: CaptureRoute::HostCopy,
            ..request(1800, 1130, "h265", &[60])
        };
        assert_eq!(resolve_speed(&rates, &host_copy), None);
        // A measurement of the exact size beats an inference.
        let mut exact = rates.to_vec();
        exact.push(rate(1800, 1130, 60, "h265", Some(58)));
        assert_eq!(
            resolve_speed(&exact, &request(1800, 1130, "h265", &[60]))
                .unwrap()
                .evidence,
            RateEvidence::Measured
        );
        // A measured exact size at 120 Hz beats an inference at 60 Hz.
        let exact_120 = [
            rate(1800, 1130, 120, "h265", Some(60)),
            rate(2560, 1600, 60, "h265", Some(60)),
        ];
        let decision = resolve_speed(&exact_120, &request(1800, 1130, "h265", &[60, 120])).unwrap();
        assert_eq!(
            (decision.output_refresh_hz, decision.evidence),
            (120, RateEvidence::Measured)
        );
    }

    #[test]
    fn this_hosts_own_no_beats_an_inference() {
        // (a) The exact size was measured below the floor: a larger output
        // that held Speed does not override it.
        let exact_failed = [
            rate(1800, 1130, 60, "h265", Some(41)),
            rate(2560, 1600, 60, "h265", Some(60)),
        ];
        assert_eq!(
            resolve_speed(&exact_failed, &request(1800, 1130, "h265", &[60])),
            None
        );
        // (b) A closer containing size failed; 4K held: no inference from 4K.
        let closer_failed = [
            rate(2560, 1600, 60, "h265", Some(43)),
            rate(3840, 2160, 60, "h265", Some(60)),
        ];
        assert_eq!(
            resolve_speed(&closer_failed, &request(1800, 1130, "h265", &[60])),
            None
        );
        // (c) The exact size never sustained a rate.
        let exact_unsustained = [
            rate(1800, 1130, 60, "h265", None),
            rate(2560, 1600, 60, "h265", Some(60)),
        ];
        assert_eq!(
            resolve_speed(&exact_unsustained, &request(1800, 1130, "h265", &[60])),
            None
        );
        // A smaller size that failed refuses a larger request too.
        let smaller_failed = [
            rate(1280, 800, 60, "h265", Some(40)),
            rate(3840, 2160, 60, "h265", Some(60)),
        ];
        assert_eq!(
            resolve_speed(&smaller_failed, &request(1800, 1130, "h265", &[60])),
            None
        );
        // A failure that is not between the request and the anchor (wider
        // but shorter than the request) does not refuse it; nor does one at
        // another refresh, codec or route.
        let unrelated = [
            rate(5120, 1080, 60, "h265", Some(30)),
            rate(1800, 1130, 120, "h265", Some(30)),
            rate(1800, 1130, 60, "h264", Some(30)),
            rate(2560, 1600, 60, "h265", Some(60)),
        ];
        assert_eq!(
            resolve_speed(&unrelated, &request(1800, 1130, "h265", &[60]))
                .unwrap()
                .evidence,
            RateEvidence::Inferred {
                anchor_width: 2560,
                anchor_height: 1600
            }
        );
        // Refused at 60 Hz, the paced refresh may still hold it.
        let paced = [
            rate(1800, 1130, 60, "h265", Some(41)),
            rate(2560, 1600, 120, "h265", Some(60)),
        ];
        let decision = resolve_speed(&paced, &request(1800, 1130, "h265", &[60, 120])).unwrap();
        assert_eq!(decision.output_refresh_hz, 120);
        assert!(decision.evidence.is_inferred());
    }

    #[test]
    fn unmeasured_and_slow_sources_never_qualify() {
        let rates = [
            rate(1920, 1080, 60, "h265", None),
            rate(2560, 1600, 60, "h265", Some(43)),
        ];
        assert_eq!(
            resolve_speed(&rates, &request(1920, 1080, "h265", &[60])),
            None
        );
        assert_eq!(
            resolve_speed(&rates, &request(1280, 800, "h265", &[60])),
            None
        );
        assert_eq!(resolve_speed(&[], &request(1280, 800, "h265", &[60])), None);
    }

    #[test]
    fn qualification_outputs_reserve_room_for_requested_displays() {
        let o = |w, h, r| QualifyOutput::new(w, h, r).unwrap();
        let operator = [o(1440, 900, 60)];
        let previous = [o(1800, 1130, 60), o(1440, 900, 60)];
        let requested = [o(3024, 1964, 120)];
        let (chosen, dropped) = qualification_outputs(
            &operator,
            &previous,
            &requested,
            &SPEED_QUALIFICATION_LADDER,
            MAX_QUALIFY_OUTPUTS,
        );
        assert_eq!(
            chosen,
            vec![
                o(1440, 900, 60),
                o(1920, 1080, 60),
                o(2560, 1600, 60),
                o(3840, 2160, 60),
                o(3024, 1964, 120),
                o(1800, 1130, 60),
            ]
        );
        assert!(dropped.is_empty());
        // Ten earlier sizes cannot crowd out what Decks ask for now: four
        // requested displays get a slot, the rest and the oldest previous
        // sizes are returned to keep for later.
        let previous = (0..10)
            .map(|i| o(1000 + i * 8, 700, 60))
            .collect::<Vec<_>>();
        let requested = (0..6)
            .map(|i| o(1200 + i * 8, 900, 120))
            .collect::<Vec<_>>();
        let (chosen, dropped) = qualification_outputs(
            &[],
            &previous,
            &requested,
            &SPEED_QUALIFICATION_LADDER,
            MAX_QUALIFY_OUTPUTS,
        );
        assert_eq!(chosen.len(), MAX_QUALIFY_OUTPUTS);
        assert_eq!(&chosen[..3], &SPEED_QUALIFICATION_LADDER);
        assert_eq!(&chosen[3..7], &requested[..4]);
        assert_eq!(&chosen[7..], &previous[..3]);
        assert!(dropped.contains(&requested[4]) && dropped.contains(&previous[9]));
        // Nothing known: the ladder.
        let (chosen, _) = qualification_outputs(
            &[],
            &[],
            &[],
            &SPEED_QUALIFICATION_LADDER,
            MAX_QUALIFY_OUTPUTS,
        );
        assert_eq!(chosen, SPEED_QUALIFICATION_LADDER.to_vec());
    }

    #[test]
    fn failed_and_covered_displays_are_not_measured_again() {
        let rates = [
            rate(1800, 1130, 60, "h265", Some(60)),
            rate(1800, 1130, 60, "h264", Some(59)),
            rate(2560, 1600, 60, "h265", Some(60)),
            rate(2560, 1600, 60, "h264", Some(58)),
            rate(1470, 956, 60, "h265", Some(40)),
            rate(5120, 1440, 60, "h265", None),
            rate(1800, 1168, 120, "h265", Some(60)),
        ];
        let o = |w, h, r| QualifyOutput::new(w, h, r).unwrap();
        // 1800x1130 is covered by 2560x1600 for both codecs; 1470x956 and
        // 5120x1440 only failed; 1800x1168 is the only 120 Hz measurement.
        assert_eq!(
            outputs_worth_remeasuring(&rates, 57),
            vec![o(2560, 1600, 60), o(1800, 1168, 60)]
        );
    }

    #[test]
    fn outputs_parse_like_the_command_line() {
        assert_eq!(
            QualifyOutput::parse("1800x1130"),
            QualifyOutput::new(1800, 1130, 60)
        );
        assert_eq!(
            QualifyOutput::parse("3024x1964@120").map(|o| o.to_string()),
            Some("3024x1964@120".to_owned())
        );
        for bad in [
            "",
            "x",
            "1920x",
            "1920x1080@",
            "1920x1080@0",
            "99999x10",
            "-1x5",
            "1920 x1080",
        ] {
            assert_eq!(QualifyOutput::parse(bad), None, "{bad}");
        }
    }
}
