// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Whether a Pier served the displays a Deck asked for (ADR 0025 §13).
//!
//! Every attachment negotiates the Deck's displays as they are now. This
//! module compares what the Deck put in its `auth_response`
//! ([`LayoutRequest`]) with what the Pier's `server_hello` says it serves,
//! monitor by monitor, and then each decoded stream with the size the hello
//! promised ([`ServedLayoutReport::check_decoded`]). It never decides what
//! to do about a difference: the Decks say it, the smoke tools fail on it,
//! and a Pier may log it before it sends its hello.
//!
//! A difference is never hidden behind a plausible picture. A display the
//! Pier fitted, stretched, scaled, left out or placed elsewhere is a
//! [`LayoutMismatch`], also when the Pier keeps its desktop's own layout
//! (`fixed_topology`, [`ServedLayoutReport::host_kept_layout`]): that is a
//! truthful "shown as it is", never a Match My Layout.
//!
//! Units: a requested monitor's `width_px`/`height_px` are its stream
//! extent as the Deck sees it (a turned display's turned size, as both
//! Decks' shared topology builder reports it) and its `x`/`y` the Deck's
//! logical arrangement; an applied monitor's rectangle is the desktop
//! rectangle in host pixels, as every Pier reports it. A display must be
//! served at exactly the requested size, at the requested rotation or
//! upright (a host that realises any turn as an upright display of the
//! size the Deck sees, as the Windows Pier does). Each stream must be
//! encoded at its served size, turned by its served rotation. Arrangements
//! are compared by which side of each other the displays sit, which logical
//! and pixel spaces share.

use std::cmp::Ordering;
use std::fmt;

use crate::messages::{
    AppliedMonitorDescriptorMsg, AuthResponse, RequestedMonitorDescriptorMsg,
    RequestedMonitorTopologyMsg, RotationMsg, ServerHelloMsg,
};

/// What a Deck asked for.
#[derive(Debug, Clone, PartialEq)]
pub enum LayoutRequest {
    /// One stream of this size: Primary display, a window, or Match My
    /// Layout on a Deck with one display (`screen_width`×`screen_height`).
    Single { width_px: u32, height_px: u32 },
    /// Match My Layout: every monitor of this roster, one stream each.
    Monitors(RequestedMonitorTopologyMsg),
}

impl LayoutRequest {
    /// The request an `auth_response` carries. `None` when it names no size
    /// (a Deck that let the host choose).
    #[must_use]
    pub fn from_auth_response(response: &AuthResponse) -> Option<Self> {
        if let Some(request) = &response.multi_monitor_v1 {
            return Some(Self::Monitors(request.requested_topology().clone()));
        }
        (response.screen_width > 0 && response.screen_height > 0).then_some(Self::Single {
            width_px: response.screen_width,
            height_px: response.screen_height,
        })
    }

    /// How many displays were asked for.
    #[must_use]
    pub fn display_count(&self) -> usize {
        match self {
            Self::Single { .. } => 1,
            Self::Monitors(topology) => topology.monitors().len(),
        }
    }
}

/// The whole request an `auth_response` makes of a desktop, hashed: the
/// one display's size, every monitor of the legacy roster, the Displays
/// mode, the time zone, the cursor, and the canonical Match My Layout
/// request (every monitor's size, place, rotation and safe area, and the
/// carriers) when there is one. A Pier binds a resume grant to it, so a
/// grant never authorizes a request other than the one it was issued for.
///
/// A request without Match My Layout hashes as it did before that field
/// existed.
///
/// # Errors
///
/// When the request cannot be serialized.
pub fn resume_request_digest(response: &AuthResponse) -> Result<[u8; 32], serde_json::Error> {
    use sha2::{Digest, Sha256};

    #[derive(serde::Serialize)]
    struct Request<'a> {
        screen_width: u32,
        screen_height: u32,
        monitors: &'a [crate::messages::ClientMonitor],
        displays_mode: &'a str,
        timezone: &'a Option<String>,
        cursor_preference: crate::messages::CursorMode,
        #[serde(skip_serializing_if = "Option::is_none")]
        multi_monitor_v1: &'a Option<crate::messages::AuthMultiMonitorRequestMsg>,
    }

    let bytes = serde_json::to_vec(&Request {
        screen_width: response.screen_width,
        screen_height: response.screen_height,
        monitors: &response.monitors,
        displays_mode: &response.displays_mode,
        timezone: &response.timezone,
        cursor_preference: response.cursor_preference,
        multi_monitor_v1: &response.multi_monitor_v1,
    })?;
    Ok(Sha256::digest(bytes).into())
}

/// The size a decoded stream must have.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExpectedStream {
    /// Its session monitor id; `None` for the one stream of a
    /// [`LayoutRequest::Single`] session, whatever id its frames carry.
    pub session_monitor_id: Option<u16>,
    pub width_px: u32,
    pub height_px: u32,
}

/// One way a served layout differs from the request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayoutMismatch {
    /// The hello could not be read for its layout.
    Unreadable { detail: String },
    /// Match My Layout was asked for and the hello carries no applied
    /// layout: the host served one stream of this size.
    LayoutNotApplied { served: [u32; 2] },
    /// The one stream is not the size asked for.
    ScreenSize {
        requested: [u32; 2],
        served: [u32; 2],
    },
    /// A requested display is not served.
    Omitted { display: String, name: String },
    /// A served monitor answers no requested display.
    Unrequested {
        display: String,
        session_monitor_id: u16,
    },
    /// A display is served at another size (in either size convention).
    Size {
        display: String,
        requested: [u32; 2],
        served: [u32; 2],
    },
    /// A display is served at another rotation.
    Rotation {
        display: String,
        requested: RotationMsg,
        served: RotationMsg,
    },
    /// The primary display is another one.
    Primary { display: String, requested: bool },
    /// Two displays sit on other sides of each other than asked.
    Arrangement { display: String, other: String },
    /// A display's stream is encoded at another size than the display, so
    /// the Deck would scale it.
    StreamScaled {
        display: String,
        served: [u32; 2],
        encoded: [u32; 2],
    },
    /// A decoded frame is not the size its stream was served at.
    Decoded {
        session_monitor_id: u16,
        expected: [u32; 2],
        decoded: [u32; 2],
    },
}

impl LayoutMismatch {
    /// Stable token for logs and telemetry.
    #[must_use]
    pub const fn token(&self) -> &'static str {
        match self {
            Self::Unreadable { .. } => "unreadable",
            Self::LayoutNotApplied { .. } => "layout_not_applied",
            Self::ScreenSize { .. } => "screen_size",
            Self::Omitted { .. } => "omitted",
            Self::Unrequested { .. } => "unrequested",
            Self::Size { .. } => "size",
            Self::Rotation { .. } => "rotation",
            Self::Primary { .. } => "primary",
            Self::Arrangement { .. } => "arrangement",
            Self::StreamScaled { .. } => "stream_scaled",
            Self::Decoded { .. } => "decoded",
        }
    }
}

fn size(extent: [u32; 2]) -> String {
    format!("{}x{}", extent[0], extent[1])
}

impl fmt::Display for LayoutMismatch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unreadable { detail } => write!(formatter, "unreadable hello layout: {detail}"),
            Self::LayoutNotApplied { served } => write!(
                formatter,
                "layout_not_applied: one stream of {} served for a multi-display request",
                size(*served)
            ),
            Self::ScreenSize { requested, served } => write!(
                formatter,
                "screen_size requested={} served={}",
                size(*requested),
                size(*served)
            ),
            Self::Omitted { display, name } => {
                write!(formatter, "omitted display={display} name={name:?}")
            }
            Self::Unrequested {
                display,
                session_monitor_id,
            } => write!(
                formatter,
                "unrequested display={display} monitor={session_monitor_id}"
            ),
            Self::Size {
                display,
                requested,
                served,
            } => write!(
                formatter,
                "size display={display} requested={} served={}",
                size(*requested),
                size(*served)
            ),
            Self::Rotation {
                display,
                requested,
                served,
            } => write!(
                formatter,
                "rotation display={display} requested={requested} served={served}"
            ),
            Self::Primary { display, requested } => write!(
                formatter,
                "primary display={display} requested_primary={requested}"
            ),
            Self::Arrangement { display, other } => {
                write!(formatter, "arrangement display={display} other={other}")
            }
            Self::StreamScaled {
                display,
                served,
                encoded,
            } => write!(
                formatter,
                "stream_scaled display={display} served={} encoded={}",
                size(*served),
                size(*encoded)
            ),
            Self::Decoded {
                session_monitor_id,
                expected,
                decoded,
            } => write!(
                formatter,
                "decoded monitor={session_monitor_id} expected={} decoded={}",
                size(*expected),
                size(*decoded)
            ),
        }
    }
}

/// The served layout compared with the request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServedLayoutReport {
    /// How many displays the Deck asked for.
    pub requested: usize,
    /// How many streams the host serves.
    pub served: usize,
    /// The host keeps its desktop's own layout (`fixed_topology`): each
    /// display is shown as it is, and the differences below are expected.
    pub host_kept_layout: bool,
    /// The size each decoded stream must have.
    pub streams: Vec<ExpectedStream>,
    /// Every difference, in request order. Empty: the request was served
    /// exactly.
    pub mismatches: Vec<LayoutMismatch>,
}

impl ServedLayoutReport {
    /// Whether every requested display was served exactly.
    #[must_use]
    pub fn matches(&self) -> bool {
        self.mismatches.is_empty()
    }

    /// One line for a log or a smoke's output.
    #[must_use]
    pub fn summary(&self) -> String {
        let verdict = if self.matches() {
            "exact"
        } else if self.host_kept_layout {
            "host_layout_kept"
        } else {
            "mismatch"
        };
        let mut line = format!(
            "served-layout verdict={verdict} requested={} served={}",
            self.requested, self.served
        );
        for stream in &self.streams {
            let id = stream
                .session_monitor_id
                .map_or_else(|| "single".to_owned(), |id| id.to_string());
            line.push_str(&format!(
                " stream[{id}]={}",
                size([stream.width_px, stream.height_px])
            ));
        }
        for mismatch in &self.mismatches {
            line.push_str(&format!(" [{mismatch}]"));
        }
        line
    }

    /// Checks a decoded frame of `session_monitor_id` against the size its
    /// stream was served at.
    ///
    /// # Errors
    ///
    /// [`LayoutMismatch::Decoded`] when the sizes differ, or when no stream
    /// of that id was served ([`LayoutMismatch::Unrequested`]).
    pub fn check_decoded(
        &self,
        session_monitor_id: u16,
        width_px: u32,
        height_px: u32,
    ) -> Result<(), LayoutMismatch> {
        let stream = self
            .streams
            .iter()
            .find(|stream| stream.session_monitor_id == Some(session_monitor_id))
            .or_else(|| {
                self.streams
                    .iter()
                    .find(|stream| stream.session_monitor_id.is_none())
            })
            .ok_or_else(|| LayoutMismatch::Unrequested {
                display: String::new(),
                session_monitor_id,
            })?;
        if [stream.width_px, stream.height_px] == [width_px, height_px] {
            Ok(())
        } else {
            Err(LayoutMismatch::Decoded {
                session_monitor_id,
                expected: [stream.width_px, stream.height_px],
                decoded: [width_px, height_px],
            })
        }
    }
}

/// Compares the layout `hello` serves with `request`.
#[must_use]
pub fn served_layout(request: &LayoutRequest, hello: &ServerHelloMsg) -> ServedLayoutReport {
    match request {
        LayoutRequest::Single {
            width_px,
            height_px,
        } => single(*width_px, *height_px, hello),
        LayoutRequest::Monitors(topology) => monitors(topology, hello),
    }
}

fn single(width_px: u32, height_px: u32, hello: &ServerHelloMsg) -> ServedLayoutReport {
    let served = [hello.screen_width, hello.screen_height];
    let requested = [width_px, height_px];
    let host_kept_layout = hello
        .multi_monitor_v1()
        .ok()
        .flatten()
        .is_some_and(|capability| capability.fixed_topology());
    ServedLayoutReport {
        requested: 1,
        served: 1,
        host_kept_layout,
        streams: vec![ExpectedStream {
            session_monitor_id: None,
            width_px: served[0],
            height_px: served[1],
        }],
        mismatches: if served == requested {
            Vec::new()
        } else {
            vec![LayoutMismatch::ScreenSize { requested, served }]
        },
    }
}

fn monitors(topology: &RequestedMonitorTopologyMsg, hello: &ServerHelloMsg) -> ServedLayoutReport {
    let requested = topology.monitors();
    let not_applied = |mismatch: LayoutMismatch, host_kept_layout: bool| ServedLayoutReport {
        requested: requested.len(),
        served: 1,
        host_kept_layout,
        streams: vec![ExpectedStream {
            session_monitor_id: None,
            width_px: hello.screen_width,
            height_px: hello.screen_height,
        }],
        mismatches: vec![mismatch],
    };
    let capability = match hello.multi_monitor_v1() {
        Ok(capability) => capability,
        Err(error) => {
            return not_applied(
                LayoutMismatch::Unreadable {
                    detail: error.to_string(),
                },
                false,
            );
        }
    };
    let host_kept_layout = capability
        .as_ref()
        .is_some_and(|capability| capability.fixed_topology());
    let Some(applied) = capability
        .as_ref()
        .and_then(|capability| capability.applied_topology())
    else {
        return not_applied(
            LayoutMismatch::LayoutNotApplied {
                served: [hello.screen_width, hello.screen_height],
            },
            host_kept_layout,
        );
    };
    let applied = applied.monitors();
    let mut mismatches = Vec::new();
    let mut pairs: Vec<(&RequestedMonitorDescriptorMsg, &AppliedMonitorDescriptorMsg)> =
        Vec::with_capacity(requested.len());
    for wanted in requested {
        match applied
            .iter()
            .find(|shown| shown.client_display_id == wanted.client_display_id)
        {
            Some(shown) => {
                compare_monitor(wanted, shown, &mut mismatches);
                pairs.push((wanted, shown));
            }
            None => mismatches.push(LayoutMismatch::Omitted {
                display: wanted.client_display_id.as_str().to_owned(),
                name: wanted.name.clone(),
            }),
        }
    }
    for shown in applied {
        if !requested
            .iter()
            .any(|wanted| wanted.client_display_id == shown.client_display_id)
        {
            mismatches.push(LayoutMismatch::Unrequested {
                display: shown.client_display_id.as_str().to_owned(),
                session_monitor_id: shown.session_monitor_id,
            });
        }
    }
    for (index, (wanted, shown)) in pairs.iter().enumerate() {
        for (other_wanted, other_shown) in &pairs[index + 1..] {
            let side = |a: i64, b: i64| a.cmp(&b);
            let requested_side = (
                side(i64::from(wanted.x), i64::from(other_wanted.x)),
                side(i64::from(wanted.y), i64::from(other_wanted.y)),
            );
            let served_side = (
                side(i64::from(shown.x), i64::from(other_shown.x)),
                side(i64::from(shown.y), i64::from(other_shown.y)),
            );
            if !same_side(requested_side, served_side) {
                mismatches.push(LayoutMismatch::Arrangement {
                    display: wanted.client_display_id.as_str().to_owned(),
                    other: other_wanted.client_display_id.as_str().to_owned(),
                });
            }
        }
    }
    ServedLayoutReport {
        requested: requested.len(),
        served: applied.len(),
        host_kept_layout,
        streams: applied
            .iter()
            .map(|shown| ExpectedStream {
                session_monitor_id: Some(shown.session_monitor_id),
                width_px: shown.media_plan.width_px,
                height_px: shown.media_plan.height_px,
            })
            .collect(),
        mismatches,
    }
}

const fn same_side(requested: (Ordering, Ordering), served: (Ordering, Ordering)) -> bool {
    matches!(
        (requested.0, served.0),
        (Ordering::Less, Ordering::Less)
            | (Ordering::Equal, Ordering::Equal)
            | (Ordering::Greater, Ordering::Greater)
    ) && matches!(
        (requested.1, served.1),
        (Ordering::Less, Ordering::Less)
            | (Ordering::Equal, Ordering::Equal)
            | (Ordering::Greater, Ordering::Greater)
    )
}

/// The sizes a stream served at `[width, height]` and `rotation` may be
/// encoded at: as served, or turned by its rotation (the two are the same
/// for an upright or upside-down display).
const fn conventions(width: u32, height: u32, rotation: RotationMsg) -> [[u32; 2]; 2] {
    match rotation {
        RotationMsg::Degrees0 | RotationMsg::Degrees180 => [[width, height], [width, height]],
        RotationMsg::Degrees90 | RotationMsg::Degrees270 => [[width, height], [height, width]],
    }
}

fn compare_monitor(
    wanted: &RequestedMonitorDescriptorMsg,
    shown: &AppliedMonitorDescriptorMsg,
    mismatches: &mut Vec<LayoutMismatch>,
) {
    let display = || wanted.client_display_id.as_str().to_owned();
    let requested = [wanted.width_px, wanted.height_px];
    let served = [shown.width_px, shown.height_px];
    let encoded = [shown.media_plan.width_px, shown.media_plan.height_px];
    // A host may realise any turn as an upright display of the size the
    // Deck sees: the stream the Deck shows is the same. Upright at another
    // size, or turned at the swapped size, it is not.
    let served_upright = shown.rotation == RotationMsg::Degrees0
        && wanted.rotation != RotationMsg::Degrees0
        && served == requested;
    if shown.rotation != wanted.rotation && !served_upright {
        mismatches.push(LayoutMismatch::Rotation {
            display: display(),
            requested: wanted.rotation,
            served: shown.rotation,
        });
    }
    if served != requested {
        mismatches.push(LayoutMismatch::Size {
            display: display(),
            requested,
            served,
        });
    } else if !conventions(served[0], served[1], shown.rotation).contains(&encoded) {
        mismatches.push(LayoutMismatch::StreamScaled {
            display: display(),
            served,
            encoded,
        });
    }
    if shown.is_primary != wanted.is_primary {
        mismatches.push(LayoutMismatch::Primary {
            display: display(),
            requested: wanted.is_primary,
        });
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use crate::messages::{
        AppliedMonitorMediaPlanMsg, AppliedMonitorTopologyMsg, AuthMultiMonitorRequestMsg,
        ClientDisplayId, CursorMode, MonitorQualityIntentMsg, MultiMonitorCarrierMsg,
        SafeAreaPolicyMsg, ServerMultiMonitorMsg, TopologyBackendKindMsg,
    };

    /// `(id, x, y, width, height, rotation, primary)`, logical = native.
    type Display = (&'static str, i32, i32, u32, u32, RotationMsg, bool);

    fn requested(displays: &[Display]) -> RequestedMonitorTopologyMsg {
        RequestedMonitorTopologyMsg::new(
            displays
                .iter()
                .enumerate()
                .map(
                    |(index, &(id, x, y, width_px, height_px, rotation, is_primary))| {
                        RequestedMonitorDescriptorMsg {
                            client_display_id: ClientDisplayId::new(id).unwrap(),
                            client_monitor_id: u32::try_from(index).unwrap() + 1,
                            x,
                            y,
                            width_px,
                            height_px,
                            logical_width: width_px,
                            logical_height: height_px,
                            scale: 1.0,
                            refresh_hz: 60,
                            rotation,
                            is_primary,
                            name: format!("Display {id}"),
                            width_mm: 0.0,
                            height_mm: 0.0,
                            vendor: 0,
                            model: 0,
                            serial: 0,
                            edid: String::new(),
                            color: None,
                            safe_area_policy: SafeAreaPolicyMsg::StandardFullscreen,
                            quality_intent: MonitorQualityIntentMsg::HostDefault,
                        }
                    },
                )
                .collect(),
        )
        .unwrap()
    }

    fn applied(displays: &[Display], fixed_topology: bool) -> ServerHelloMsg {
        let monitors: Vec<_> = displays
            .iter()
            .enumerate()
            .map(
                |(index, &(id, x, y, width_px, height_px, rotation, is_primary))| {
                    AppliedMonitorDescriptorMsg {
                        client_display_id: ClientDisplayId::new(id).unwrap(),
                        session_monitor_id: u16::try_from(index).unwrap() + 1,
                        x,
                        y,
                        width_px,
                        height_px,
                        refresh_hz: 60,
                        rotation,
                        is_primary,
                        media_plan: AppliedMonitorMediaPlanMsg {
                            stream_epoch: 1,
                            encoder_backend: "native-nvenc".to_owned(),
                            encoder_class: "hardware".to_owned(),
                            codec: "h264".to_owned(),
                            chroma: "yuv420".to_owned(),
                            width_px,
                            height_px,
                            fps: 60,
                            bitrate_kbps: 10_000,
                            cursor_mode: CursorMode::Local,
                            degraded: false,
                            degradation_reason: String::new(),
                        },
                    }
                },
            )
            .collect();
        let right = monitors
            .iter()
            .map(|monitor| i64::from(monitor.x) + i64::from(monitor.width_px))
            .max()
            .unwrap();
        let bottom = monitors
            .iter()
            .map(|monitor| i64::from(monitor.y) + i64::from(monitor.height_px))
            .max()
            .unwrap();
        let left = monitors.iter().map(|monitor| monitor.x).min().unwrap();
        let top = monitors.iter().map(|monitor| monitor.y).min().unwrap();
        let topology = AppliedMonitorTopologyMsg::new(
            1,
            left,
            top,
            u32::try_from(right - i64::from(left)).unwrap(),
            u32::try_from(bottom - i64::from(top)).unwrap(),
            0,
            0,
            MultiMonitorCarrierMsg::MuxedReliableStream,
            monitors,
        )
        .unwrap();
        let capability = ServerMultiMonitorMsg::new(
            4,
            RotationMsg::ALL.to_vec(),
            fixed_topology,
            TopologyBackendKindMsg::VirtualOutputs,
            vec![MultiMonitorCarrierMsg::MuxedReliableStream],
            Some(topology),
        )
        .unwrap();
        hello(2560, 1440)
            .with_multi_monitor_v1(&capability)
            .unwrap()
    }

    fn hello(width: u32, height: u32) -> ServerHelloMsg {
        serde_json::from_str(&format!(
            r#"{{"type":"server_hello","screen_width":{width},"screen_height":{height}}}"#
        ))
        .unwrap()
    }

    const UP: RotationMsg = RotationMsg::Degrees0;
    const TWO: [Display; 2] = [
        ("a", 0, 0, 2560, 1440, UP, true),
        ("b", 2560, 0, 1920, 1080, UP, false),
    ];

    #[test]
    fn an_exactly_served_layout_matches_and_promises_each_stream() {
        let report = served_layout(
            &LayoutRequest::Monitors(requested(&TWO)),
            &applied(&TWO, false),
        );
        assert!(report.matches(), "{}", report.summary());
        assert_eq!(report.served, 2);
        assert_eq!(report.check_decoded(2, 1920, 1080), Ok(()));
        assert_eq!(
            report.check_decoded(1, 1280, 720),
            Err(LayoutMismatch::Decoded {
                session_monitor_id: 1,
                expected: [2560, 1440],
                decoded: [1280, 720],
            })
        );
    }

    #[test]
    fn every_monitor_is_checked_not_only_the_primary() {
        let mut served = TWO;
        served[1].3 = 1280;
        served[1].4 = 720;
        let report = served_layout(
            &LayoutRequest::Monitors(requested(&TWO)),
            &applied(&served, false),
        );
        assert_eq!(
            report.mismatches,
            vec![LayoutMismatch::Size {
                display: "b".into(),
                requested: [1920, 1080],
                served: [1280, 720],
            }]
        );
    }

    #[test]
    fn an_old_two_monitor_layout_cannot_answer_a_one_monitor_request() {
        let one = [("a", 0, 0, 2560, 1440, UP, true)];
        let report = served_layout(
            &LayoutRequest::Monitors(requested(&one)),
            &applied(&TWO, false),
        );
        assert_eq!(
            report.mismatches,
            vec![LayoutMismatch::Unrequested {
                display: "b".into(),
                session_monitor_id: 2,
            }]
        );
        let report = served_layout(
            &LayoutRequest::Monitors(requested(&TWO)),
            &applied(&one, false),
        );
        assert_eq!(report.mismatches.len(), 1);
        assert_eq!(report.mismatches[0].token(), "omitted");
    }

    #[test]
    fn a_changed_primary_rotation_or_side_is_a_mismatch() {
        let mut served = TWO;
        served[0].6 = false;
        served[1].6 = true;
        let tokens = |served: &[Display]| {
            served_layout(
                &LayoutRequest::Monitors(requested(&TWO)),
                &applied(served, false),
            )
            .mismatches
            .iter()
            .map(LayoutMismatch::token)
            .collect::<Vec<_>>()
        };
        assert_eq!(tokens(&served), ["primary", "primary"]);
        let mut served = TWO;
        served[1].5 = RotationMsg::Degrees180;
        assert_eq!(tokens(&served), ["rotation"]);
        let mut served = TWO;
        served[0].1 = 1920;
        served[1].1 = 0;
        assert_eq!(tokens(&served), ["arrangement"]);
    }

    #[test]
    fn a_turned_display_matches_in_either_size_convention() {
        // The Deck asks for a portrait display turned 270 degrees at the
        // size it sees (1440x2560), as both Decks' topology builder does.
        const TURNED: RotationMsg = RotationMsg::Degrees270;
        let request = [
            ("a", 0, 0, 3024, 1964, UP, true),
            ("b", 3024, -300, 1440, 2560, TURNED, false),
        ];
        let served = |width, height, rotation| {
            [
                ("a", 0, 300, 3024, 1964, UP, true),
                ("b", 3024, 0, width, height, rotation, false),
            ]
        };
        let tokens = |shown: &[Display]| {
            served_layout(
                &LayoutRequest::Monitors(requested(&request)),
                &applied(shown, false),
            )
            .mismatches
            .iter()
            .map(LayoutMismatch::token)
            .collect::<Vec<_>>()
        };
        // Turned at the size the Deck sees, or upright at it, as the
        // Windows Pier serves it.
        for shown in [served(1440, 2560, TURNED), served(1440, 2560, UP)] {
            assert!(tokens(&shown).is_empty(), "{shown:?}");
        }
        // No Pier reports an unturned panel size: turned at the swapped
        // size is another desktop rectangle.
        assert_eq!(tokens(&served(2560, 1440, TURNED)), ["size"]);
        // Upright at the landscape size: the portrait request was fitted.
        assert_eq!(tokens(&served(2560, 1440, UP)), ["rotation", "size"]);
        // Turned the other way, it is not the same picture.
        assert_eq!(
            tokens(&served(1440, 2560, RotationMsg::Degrees90)),
            ["rotation"]
        );
    }

    #[test]
    fn an_upside_down_display_served_upright_at_its_size_matches() {
        const FLIPPED: RotationMsg = RotationMsg::Degrees180;
        let request = [
            ("a", 0, 0, 2560, 1440, UP, true),
            ("b", 2560, 0, 1920, 1080, FLIPPED, false),
        ];
        let tokens = |b: Display| {
            served_layout(
                &LayoutRequest::Monitors(requested(&request)),
                &applied(&[("a", 0, 0, 2560, 1440, UP, true), b], false),
            )
            .mismatches
            .iter()
            .map(LayoutMismatch::token)
            .collect::<Vec<_>>()
        };
        // As the Windows Pier serves every turn: upright, at the size.
        assert!(tokens(("b", 2560, 0, 1920, 1080, UP, false)).is_empty());
        assert!(tokens(("b", 2560, 0, 1920, 1080, FLIPPED, false)).is_empty());
        assert_eq!(
            tokens(("b", 2560, 0, 1920, 1080, RotationMsg::Degrees90, false)),
            ["rotation"]
        );
        assert_eq!(
            tokens(("b", 2560, 0, 1280, 720, UP, false)),
            ["rotation", "size"]
        );
    }

    #[test]
    fn a_stream_is_encoded_at_its_served_size() {
        const TURNED: RotationMsg = RotationMsg::Degrees270;
        let request = [
            ("a", 0, 0, 3024, 1964, UP, true),
            ("b", 3024, -300, 1440, 2560, TURNED, false),
        ];
        // Served upright at 1440x2560 but encoded landscape: the Deck would
        // scale it, although the landscape size is one convention of the
        // request.
        let mut hello = applied(
            &[
                ("a", 0, 300, 3024, 1964, UP, true),
                ("b", 3024, 0, 1440, 2560, UP, false),
            ],
            false,
        );
        let capability = hello.multi_monitor_v1().unwrap().unwrap();
        let topology = capability.applied_topology().unwrap().clone();
        let mut monitors = topology.monitors().to_vec();
        monitors[1].media_plan.width_px = 2560;
        monitors[1].media_plan.height_px = 1440;
        let topology = AppliedMonitorTopologyMsg::new(
            topology.topology_generation(),
            0,
            0,
            4464,
            2560,
            0,
            0,
            MultiMonitorCarrierMsg::MuxedReliableStream,
            monitors,
        )
        .unwrap();
        let capability = ServerMultiMonitorMsg::new(
            4,
            RotationMsg::ALL.to_vec(),
            false,
            TopologyBackendKindMsg::VirtualOutputs,
            vec![MultiMonitorCarrierMsg::MuxedReliableStream],
            Some(topology),
        )
        .unwrap();
        hello = hello.with_multi_monitor_v1(&capability).unwrap();
        let report = served_layout(&LayoutRequest::Monitors(requested(&request)), &hello);
        assert_eq!(
            report
                .mismatches
                .iter()
                .map(LayoutMismatch::token)
                .collect::<Vec<_>>(),
            ["stream_scaled"]
        );
    }

    #[test]
    fn a_scaled_stream_is_not_a_match() {
        let mut hello = applied(&TWO, false);
        let mut capability = hello.multi_monitor_v1().unwrap().unwrap();
        let mut topology = capability.applied_topology().unwrap().clone();
        let mut monitors = topology.monitors().to_vec();
        monitors[1].media_plan.width_px = 1280;
        monitors[1].media_plan.height_px = 720;
        topology = AppliedMonitorTopologyMsg::new(
            topology.topology_generation(),
            0,
            0,
            4480,
            1440,
            0,
            0,
            MultiMonitorCarrierMsg::MuxedReliableStream,
            monitors,
        )
        .unwrap();
        capability = ServerMultiMonitorMsg::new(
            4,
            RotationMsg::ALL.to_vec(),
            false,
            TopologyBackendKindMsg::VirtualOutputs,
            vec![MultiMonitorCarrierMsg::MuxedReliableStream],
            Some(topology),
        )
        .unwrap();
        hello = hello.with_multi_monitor_v1(&capability).unwrap();
        let report = served_layout(&LayoutRequest::Monitors(requested(&TWO)), &hello);
        assert_eq!(report.mismatches[0].token(), "stream_scaled");
        assert_eq!(report.check_decoded(2, 1280, 720), Ok(()));
    }

    #[test]
    fn a_kept_layout_is_reported_truthfully_never_as_a_match() {
        let shown = [
            ("a", 0, 0, 1920, 1080, UP, true),
            ("b", 1920, 0, 1920, 1080, UP, false),
        ];
        let report = served_layout(
            &LayoutRequest::Monitors(requested(&TWO)),
            &applied(&shown, true),
        );
        assert!(report.host_kept_layout);
        assert!(!report.matches());
        assert!(report.summary().contains("verdict=host_layout_kept"));
    }

    #[test]
    fn a_single_request_compares_the_one_stream() {
        let request = LayoutRequest::Single {
            width_px: 3024,
            height_px: 1964,
        };
        assert!(served_layout(&request, &hello(3024, 1964)).matches());
        let report = served_layout(&request, &hello(1920, 1080));
        assert_eq!(
            report.mismatches,
            vec![LayoutMismatch::ScreenSize {
                requested: [3024, 1964],
                served: [1920, 1080],
            }]
        );
        assert_eq!(report.check_decoded(0, 1920, 1080), Ok(()));
    }

    #[test]
    fn a_multi_display_request_answered_by_one_stream_says_so() {
        let report = served_layout(
            &LayoutRequest::Monitors(requested(&TWO)),
            &hello(2560, 1440),
        );
        assert_eq!(
            report.mismatches,
            vec![LayoutMismatch::LayoutNotApplied {
                served: [2560, 1440],
            }]
        );
    }

    #[test]
    fn a_resume_binding_covers_every_monitor_of_the_request() {
        let mut response = AuthResponse::password("artist", "secret");
        response.screen_width = 2560;
        response.screen_height = 1440;
        let single = resume_request_digest(&response).unwrap();
        response.multi_monitor_v1 = Some(
            AuthMultiMonitorRequestMsg::new(
                requested(&TWO),
                vec![MultiMonitorCarrierMsg::MuxedReliableStream],
            )
            .unwrap(),
        );
        let two = resume_request_digest(&response).unwrap();
        assert_ne!(single, two);
        let mut turned = TWO;
        turned[1].5 = RotationMsg::Degrees90;
        response.multi_monitor_v1 = Some(
            AuthMultiMonitorRequestMsg::new(
                requested(&turned),
                vec![MultiMonitorCarrierMsg::MuxedReliableStream],
            )
            .unwrap(),
        );
        assert_ne!(two, resume_request_digest(&response).unwrap());
        response.multi_monitor_v1 = None;
        assert_eq!(single, resume_request_digest(&response).unwrap());
    }

    #[test]
    fn the_request_comes_from_the_auth_response() {
        let mut response = AuthResponse::password("artist", "secret");
        assert_eq!(LayoutRequest::from_auth_response(&response), None);
        response.screen_width = 1920;
        response.screen_height = 1200;
        assert_eq!(
            LayoutRequest::from_auth_response(&response),
            Some(LayoutRequest::Single {
                width_px: 1920,
                height_px: 1200,
            })
        );
        response.multi_monitor_v1 = Some(
            AuthMultiMonitorRequestMsg::new(
                requested(&TWO),
                vec![MultiMonitorCarrierMsg::MuxedReliableStream],
            )
            .unwrap(),
        );
        let request = LayoutRequest::from_auth_response(&response).unwrap();
        assert_eq!(request.display_count(), 2);
    }
}
