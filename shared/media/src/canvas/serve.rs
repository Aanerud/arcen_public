// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Whether a Pier serves a canvas to one Deck, what its hello says, and the
//! one `capenc` argument that carries the decision to the encode loop
//! (ADR 0028). Every Pier calls these; no platform decides it.

use arcen_protocol::canvas::{
    CanvasCapability, CanvasMonitorStatusMsg, CanvasStateMsg, CanvasStatusMsg, CanvasTerms,
};
use arcen_protocol::refine::CODEC_DELTA_ZSTD1_TOKEN;

use super::encode_loop::CanvasLoopTerms;
use super::governor::CpuShare;

/// Prefix of the `capenc` argument that turns the canvas on
/// (`canvas=v1:<max_px>:<surfaces>:<cache_bytes>:<cache_slots>:<monitor>:<topology_generation>:<stream_epoch>:<cpu_share>`).
pub const CAPENC_CANVAS_ARG_PREFIX: &str = "canvas=";

const ARG_VERSION: &str = "v1";

/// Stable reason tokens of a canvas that is off.
pub mod reason {
    /// The Deck did not ask.
    pub const NOT_REQUESTED: &str = "not_requested";
    /// The Deck asked with terms this protocol cannot use.
    pub const UNSUPPORTED_TERMS: &str = "unsupported_terms";
    /// The session is served by a hardware contract (canvas is the
    /// Software contract's pipeline only).
    pub const NOT_SOFTWARE: &str = "not_software";
    /// A multi-monitor session (one canvas per monitor is not served yet).
    pub const MULTI_MONITOR: &str = "multi_monitor";
    /// The monitor is larger than the Deck's canvas.
    pub const CANVAS_TOO_LARGE: &str = "canvas_too_large";
    /// The operator switched it off.
    pub const OPERATOR_OFF: &str = "operator_off";
    /// This Pier's capture path has no canvas loop.
    pub const NOT_BUILT: &str = "not_built";
}

/// What a Pier knows when it decides.
#[derive(Debug, Clone, Copy)]
pub struct CanvasServeFacts<'a> {
    /// `AuthResponse.canvas_v1`.
    pub offer: Option<&'a CanvasCapability>,
    /// The session is served by the Software contract.
    pub software: bool,
    /// The session's monitors: `(monitor_id, width, height)`; `0x0` is a
    /// capture at the screen's own size, known only when it opens.
    pub monitors: &'a [(u16, u32, u32)],
    /// The operator allows it.
    pub operator_enabled: bool,
    /// The capture path has the shared canvas loop.
    pub built: bool,
}

/// The decision for one Deck.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanvasServePlan {
    /// `ServerHelloMsg.canvas` (`None` for a Deck that did not ask).
    pub status: Option<CanvasStatusMsg>,
    /// The terms the loop uses when on.
    pub terms: Option<CanvasTerms>,
}

impl CanvasServePlan {
    /// Whether canvas records are sent.
    #[must_use]
    pub const fn is_on(&self) -> bool {
        self.terms.is_some()
    }

    /// The off reason, for logs and READY.
    #[must_use]
    pub fn off_reason(&self) -> &str {
        self.status
            .as_ref()
            .and_then(|status| status.reason.as_deref())
            .unwrap_or(reason::NOT_REQUESTED)
    }
}

/// The plan for a Deck that offered the canvas to a multi-monitor session
/// (one canvas per monitor is not served yet).
#[must_use]
pub fn multi_monitor_off() -> CanvasServePlan {
    off(reason::MULTI_MONITOR)
}

fn off(reason: &str) -> CanvasServePlan {
    CanvasServePlan {
        status: Some(CanvasStatusMsg {
            state: CanvasStateMsg::Off,
            reason: Some(reason.to_owned()),
            tile_codec: None,
            cache_bytes: None,
            cache_slots: None,
            max_surfaces_per_monitor: None,
            monitors: Vec::new(),
        }),
        terms: None,
    }
}

/// Decides whether the Deck gets a canvas. Canvas is the Software
/// contract's pipeline only (hardware contracts keep their streams), one
/// monitor for now, within the Deck's terms.
#[must_use]
pub fn plan_canvas(facts: &CanvasServeFacts<'_>) -> CanvasServePlan {
    let Some(offer) = facts.offer else {
        return CanvasServePlan {
            status: None,
            terms: None,
        };
    };
    let Some(terms) = offer.terms() else {
        return off(reason::UNSUPPORTED_TERMS);
    };
    if !facts.built {
        return off(reason::NOT_BUILT);
    }
    if !facts.operator_enabled {
        return off(reason::OPERATOR_OFF);
    }
    if !facts.software {
        return off(reason::NOT_SOFTWARE);
    }
    let [(_, width, height)] = facts.monitors else {
        return off(reason::MULTI_MONITOR);
    };
    // 0x0 is a capture at the screen's own size (a joined console), which
    // the loop checks against the Deck's terms when it opens.
    let own_size = (*width, *height) == (0, 0);
    if !own_size
        && (*width > terms.max_canvas_px
            || *height > terms.max_canvas_px
            || *width == 0
            || *height == 0)
    {
        return off(reason::CANVAS_TOO_LARGE);
    }
    CanvasServePlan {
        status: Some(CanvasStatusMsg {
            state: CanvasStateMsg::On,
            reason: None,
            tile_codec: Some(CODEC_DELTA_ZSTD1_TOKEN.to_owned()),
            cache_bytes: Some(terms.cache_bytes),
            cache_slots: Some(terms.cache_slots),
            max_surfaces_per_monitor: Some(terms.max_surfaces_per_monitor),
            // A single-monitor session's canvas is monitor 0 (WIRE.md).
            monitors: vec![CanvasMonitorStatusMsg {
                monitor_id: 0,
                state: CanvasStateMsg::On,
                reason: None,
            }],
        }),
        terms: Some(terms),
    }
}

/// Whether `record` is a canvas reset (`Some(true)`), a canvas frame
/// (`Some(false)`) or not a canvas record's prefix at all (`None`). A Pier
/// routes canvas records without parsing them; the Deck validates them.
#[must_use]
pub fn canvas_record_is_reset(record: &[u8]) -> Option<bool> {
    use arcen_protocol::canvas::{CANVAS_PREFIX_BYTES, CANVAS_RECORD_VERSION, CanvasRecordKind};
    use arcen_protocol::wire::FrameType;
    if record.len() < CANVAS_PREFIX_BYTES
        || record[0] != FrameType::Canvas as u8
        || record[1] != CANVAS_RECORD_VERSION
    {
        return None;
    }
    match CanvasRecordKind::from_wire(record[2])? {
        CanvasRecordKind::Reset => Some(true),
        CanvasRecordKind::Frame => Some(false),
    }
}

/// Byte offset of a canvas frame's `timestamp_ms` (after the prefix and
/// the fence).
const FRAME_TIMESTAMP_OFFSET: usize = arcen_protocol::canvas::CANVAS_PREFIX_BYTES + 8;

/// Stamps a canvas frame with the Pier's wire clock (ADR 0026), as every
/// video frame header is stamped when the Pier sends it. A reset carries no
/// timestamp and is left as it is.
pub fn stamp_canvas_frame(record: &mut [u8], timestamp_ms: u32) {
    if canvas_record_is_reset(record) == Some(false) && record.len() >= FRAME_TIMESTAMP_OFFSET + 4 {
        record[FRAME_TIMESTAMP_OFFSET..FRAME_TIMESTAMP_OFFSET + 4]
            .copy_from_slice(&timestamp_ms.to_be_bytes());
    }
}

/// The canvas half of a `capenc` command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CanvasCapencArgs {
    /// The Deck's clamped terms.
    pub terms: CanvasTerms,
    /// Monitor id (0 for a single-monitor session).
    pub monitor_id: u16,
    /// Topology generation (0 for a single-monitor session).
    pub topology_generation: u64,
    /// Stream epoch (0 for a single-monitor session).
    pub stream_epoch: u64,
    /// `video.software.cpu_share`.
    pub cpu_share: CpuShare,
}

impl CanvasCapencArgs {
    /// A single-monitor session's canvas (monitor 0, generation 0,
    /// epoch 0).
    #[must_use]
    pub const fn single_monitor(terms: CanvasTerms, cpu_share: CpuShare) -> Self {
        Self {
            terms,
            monitor_id: 0,
            topology_generation: 0,
            stream_epoch: 0,
            cpu_share,
        }
    }

    /// The argument.
    #[must_use]
    pub fn to_arg(&self) -> String {
        format!(
            "{CAPENC_CANVAS_ARG_PREFIX}{ARG_VERSION}:{}:{}:{}:{}:{}:{}:{}:{}",
            self.terms.max_canvas_px,
            self.terms.max_surfaces_per_monitor,
            self.terms.cache_bytes,
            self.terms.cache_slots,
            self.monitor_id,
            self.topology_generation,
            self.stream_epoch,
            self.cpu_share.percent(),
        )
    }

    /// Reads the argument from a command line: `Ok(None)` without one.
    /// The terms are clamped to the protocol's bounds again, so a helper
    /// never trusts its parent beyond them.
    ///
    /// # Errors
    ///
    /// A readable message for a malformed or repeated argument.
    pub fn from_args(args: &[String]) -> Result<Option<Self>, String> {
        let mut found = args
            .iter()
            .filter_map(|arg| arg.strip_prefix(CAPENC_CANVAS_ARG_PREFIX));
        let Some(value) = found.next() else {
            return Ok(None);
        };
        if found.next().is_some() {
            return Err(format!("{CAPENC_CANVAS_ARG_PREFIX} given twice"));
        }
        let invalid = || format!("invalid {CAPENC_CANVAS_ARG_PREFIX}{value}");
        let fields: Vec<&str> = value.split(':').collect();
        let [
            version,
            max_px,
            surfaces,
            cache_bytes,
            cache_slots,
            monitor,
            topology,
            epoch,
            share,
        ] = fields.as_slice()
        else {
            return Err(invalid());
        };
        if *version != ARG_VERSION {
            return Err(invalid());
        }
        let number = |field: &str| field.parse::<u64>().map_err(|_| invalid());
        let offer = CanvasCapability {
            max_canvas_px: u32::try_from(number(max_px)?).map_err(|_| invalid())?,
            max_surfaces_per_monitor: u8::try_from(number(surfaces)?).map_err(|_| invalid())?,
            cache_bytes: number(cache_bytes)?,
            cache_slots: u32::try_from(number(cache_slots)?).map_err(|_| invalid())?,
            tile_codecs: vec![CODEC_DELTA_ZSTD1_TOKEN.to_owned()],
        };
        let terms = offer.terms().ok_or_else(invalid)?;
        let cpu_share = u8::try_from(number(share)?)
            .ok()
            .and_then(CpuShare::from_percent)
            .ok_or_else(invalid)?;
        Ok(Some(Self {
            terms,
            monitor_id: u16::try_from(number(monitor)?).map_err(|_| invalid())?,
            topology_generation: number(topology)?,
            stream_epoch: number(epoch)?,
            cpu_share,
        }))
    }

    /// The loop's canvas terms on a host with `logical_cores`.
    #[must_use]
    pub const fn loop_terms(&self, logical_cores: u32) -> CanvasLoopTerms {
        CanvasLoopTerms {
            terms: self.terms,
            monitor_id: self.monitor_id,
            topology_generation: self.topology_generation,
            stream_epoch: self.stream_epoch,
            cpu_share: self.cpu_share,
            logical_cores,
        }
    }
}

/// Movement, in 1/65536ths of the picture's width or height, after which a
/// Pier forwards the applied input position again: 1/256 of a side, about
/// one canvas block on a 4K monitor and half a block at 1080p.
pub const INPUT_MOVE: u16 = 256;

/// How often a Pier forwards input that stays in place (a click or a held
/// drag on one spot), half the planner's input window, so the neighbourhood
/// stays mandatory while the user acts there.
pub const INPUT_REFRESH: std::time::Duration = std::time::Duration::from_millis(125);

/// The Pier's half of canvas input priority (ADR 0028 §11): turns where the
/// Pier applied pointer or pen input into `CANVAS-INPUT` lines for the
/// encode loop, whose planner never defers that neighbourhood.
///
/// It is fed only after the Pier authorised and applied an event, with the
/// event's normalised position (the fraction the injection used); it holds
/// one position, not a queue, carries no input content, and forwards a
/// position only when it moved [`INPUT_MOVE`] or the last one is
/// [`INPUT_REFRESH`] old, so redundant samples cost no traffic. Positions
/// travel as 1/65536ths of the picture: the loop, which knows its size,
/// turns them into pixels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanvasInputFeed {
    monitor_id: u16,
    topology_generation: u64,
    last: Option<(u16, u16, std::time::Instant)>,
}

impl CanvasInputFeed {
    /// The feed for a capture's canvas, `None` when the plan serves none.
    /// A single-monitor canvas is monitor 0 of topology generation 0.
    #[must_use]
    pub fn for_plan(plan: &CanvasServePlan) -> Option<Self> {
        plan.is_on().then_some(Self {
            monitor_id: 0,
            topology_generation: 0,
            last: None,
        })
    }

    /// The Pier applied input at normalised `(x, y)` (0 to 1 across the
    /// picture) at `now`. Returns the line to send, if one is due. A
    /// position that is not finite yields nothing; one outside the picture
    /// is clamped to its edge.
    #[must_use]
    pub fn applied(
        &mut self,
        x: f64,
        y: f64,
        now: std::time::Instant,
    ) -> Option<crate::capenc_control::CapencControlCommand> {
        let fixed = |fraction: f64| -> Option<u16> {
            if !fraction.is_finite() {
                return None;
            }
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "clamped to 0..=65535 before the cast"
            )]
            let at = (fraction.clamp(0.0, 1.0) * 65_536.0).min(65_535.0) as u16;
            Some(at)
        };
        let (x, y) = (fixed(x)?, fixed(y)?);
        if let Some((last_x, last_y, at)) = self.last {
            let moved = last_x.abs_diff(x).max(last_y.abs_diff(y)) >= INPUT_MOVE;
            if !moved && now.saturating_duration_since(at) < INPUT_REFRESH {
                return None;
            }
        }
        self.last = Some((x, y, now));
        Some(crate::capenc_control::CapencControlCommand::CanvasInput {
            monitor_id: self.monitor_id,
            topology_generation: self.topology_generation,
            x,
            y,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Pier forwards where it applied input only to a capture that
    /// serves a canvas, once per [`INPUT_MOVE`] of movement or per refresh
    /// while the input stays in place, in 1/65536ths of the picture, as
    /// monitor 0 of topology generation 0; a position that is not a number
    /// is dropped.
    #[test]
    fn applied_input_is_forwarded_once_per_move_or_refresh() {
        use crate::capenc_control::CapencControlCommand;
        let start = std::time::Instant::now();
        assert_eq!(CanvasInputFeed::for_plan(&multi_monitor_off()), None);
        let on = CanvasServePlan {
            status: None,
            terms: Some(CanvasTerms {
                max_canvas_px: 16_384,
                max_surfaces_per_monitor: 2,
                cache_bytes: 1 << 20,
                cache_slots: 256,
            }),
        };
        let mut feed = CanvasInputFeed::for_plan(&on).expect("a canvas");
        let line = |x, y| CapencControlCommand::CanvasInput {
            monitor_id: 0,
            topology_generation: 0,
            x,
            y,
        };
        assert_eq!(feed.applied(0.5, 0.25, start), Some(line(32_768, 16_384)));
        // Redundant samples: less than INPUT_MOVE away, within the refresh.
        for step in 1..10 {
            let now = start + std::time::Duration::from_millis(step * 10);
            assert_eq!(feed.applied(0.5 + 0.001, 0.25, now), None);
        }
        assert_eq!(
            feed.applied(0.505, 0.25, start + INPUT_REFRESH / 2),
            Some(line(33_095, 16_384)),
            "moved"
        );
        assert_eq!(
            feed.applied(0.505, 0.25, start + INPUT_REFRESH * 2),
            Some(line(33_095, 16_384)),
            "still acting there"
        );
        assert_eq!(
            feed.applied(1.5, -1.0, start + INPUT_REFRESH * 4),
            Some(line(65_535, 0)),
            "clamped to the picture"
        );
        assert_eq!(feed.applied(f64::NAN, 0.5, start), None);
    }

    fn offer() -> CanvasCapability {
        CanvasCapability {
            max_canvas_px: 16_384,
            max_surfaces_per_monitor: 2,
            cache_bytes: 100 * 1024 * 1024,
            cache_slots: 25_600,
            tile_codecs: vec![CODEC_DELTA_ZSTD1_TOKEN.to_owned()],
        }
    }

    fn facts<'a>(
        offer: Option<&'a CanvasCapability>,
        monitors: &'a [(u16, u32, u32)],
    ) -> CanvasServeFacts<'a> {
        CanvasServeFacts {
            offer,
            software: true,
            monitors,
            operator_enabled: true,
            built: true,
        }
    }

    #[test]
    fn a_software_single_monitor_session_serves_the_canvas_within_the_decks_terms() {
        let offer = offer();
        let plan = plan_canvas(&facts(Some(&offer), &[(0, 3840, 2160)]));
        assert!(plan.is_on());
        let status = plan.status.unwrap();
        status.validate(&offer.terms().unwrap()).unwrap();
        assert!(status.is_on_for(0));
        assert!(!status.is_on_for(1));
    }

    #[test]
    fn every_off_case_says_why_and_a_deck_that_did_not_ask_gets_nothing() {
        let offer = offer();
        assert_eq!(
            plan_canvas(&facts(None, &[(0, 1920, 1080)])),
            CanvasServePlan {
                status: None,
                terms: None
            }
        );
        let one = [(0, 1920, 1080)];
        let cases: [(CanvasServeFacts<'_>, &str); 5] = [
            (
                CanvasServeFacts {
                    software: false,
                    ..facts(Some(&offer), &one)
                },
                reason::NOT_SOFTWARE,
            ),
            (
                CanvasServeFacts {
                    operator_enabled: false,
                    ..facts(Some(&offer), &one)
                },
                reason::OPERATOR_OFF,
            ),
            (
                CanvasServeFacts {
                    built: false,
                    ..facts(Some(&offer), &one)
                },
                reason::NOT_BUILT,
            ),
            (
                facts(Some(&offer), &[(1, 1920, 1080), (2, 1920, 1080)]),
                reason::MULTI_MONITOR,
            ),
            (
                facts(Some(&offer), &[(0, 20_000, 1080)]),
                reason::CANVAS_TOO_LARGE,
            ),
        ];
        for (facts, expected) in cases {
            let plan = plan_canvas(&facts);
            assert!(!plan.is_on());
            assert_eq!(plan.off_reason(), expected);
            plan.status
                .unwrap()
                .validate(&offer.terms().unwrap())
                .unwrap();
        }
        // A capture at the screen's own size is served; the loop checks it.
        assert!(plan_canvas(&facts(Some(&offer), &[(0, 0, 0)])).is_on());
        assert_eq!(
            plan_canvas(&facts(Some(&offer), &[(0, 0, 1080)])).off_reason(),
            reason::CANVAS_TOO_LARGE
        );
        let mut no_codec = offer.clone();
        no_codec.tile_codecs.clear();
        assert_eq!(
            plan_canvas(&facts(Some(&no_codec), &one)).off_reason(),
            reason::UNSUPPORTED_TERMS
        );
    }

    #[test]
    fn a_pier_tells_resets_from_frames_and_stamps_frames_with_its_clock() {
        use arcen_protocol::canvas::{
            CanvasFrame, CanvasPixelFormat, CanvasRecord, CanvasReset, CanvasResetReason,
            CanvasScope,
        };
        let scope = CanvasScope {
            monitor_id: 0,
            topology_generation: 0,
            stream_epoch: 0,
            generation: std::num::NonZeroU32::new(1).unwrap(),
        };
        let reset = CanvasReset {
            scope,
            reason: CanvasResetReason::Start,
            format: CanvasPixelFormat::Bgra8,
            width: 64,
            height: 32,
        }
        .encode()
        .unwrap();
        let mut frame = CanvasFrame {
            scope,
            fence: 9,
            timestamp_ms: 1,
            block_count: 8,
            commands: Vec::new(),
        }
        .encode()
        .unwrap();
        assert_eq!(canvas_record_is_reset(&reset), Some(true));
        assert_eq!(canvas_record_is_reset(&frame), Some(false));
        assert_eq!(canvas_record_is_reset(&frame[..10]), None);
        assert_eq!(canvas_record_is_reset(&[0x03; 64]), None);
        stamp_canvas_frame(&mut frame, 0xDEAD_BEEF);
        let CanvasRecord::Frame(stamped) = CanvasRecord::decode(&frame).unwrap() else {
            panic!("frame");
        };
        assert_eq!((stamped.fence, stamped.timestamp_ms), (9, 0xDEAD_BEEF));
        let mut unchanged = reset.clone();
        stamp_canvas_frame(&mut unchanged, 7);
        assert_eq!(unchanged, reset);
    }

    #[test]
    fn the_capenc_argument_round_trips_and_refuses_what_it_cannot_read() {
        let terms = offer().terms().unwrap();
        let args = CanvasCapencArgs::single_monitor(terms, CpuShare::default());
        let line = vec!["0".to_owned(), args.to_arg()];
        assert_eq!(CanvasCapencArgs::from_args(&line), Ok(Some(args)));
        assert_eq!(CanvasCapencArgs::from_args(&["0".to_owned()]), Ok(None));
        for bad in [
            "canvas=v2:16384:2:1:1:0:0:0:50",
            "canvas=v1:16384:2:1:1:0:0:0",
            "canvas=v1:16384:0:1:1:0:0:0:50",
            "canvas=v1:16384:2:1:1:0:0:0:0",
            "canvas=v1:16384:2:1:1:70000:0:0:50",
            "canvas=v1:x:2:1:1:0:0:0:50",
        ] {
            assert!(
                CanvasCapencArgs::from_args(&[bad.to_owned()]).is_err(),
                "{bad}"
            );
        }
        let twice = vec![args.to_arg(), args.to_arg()];
        assert!(CanvasCapencArgs::from_args(&twice).is_err());
        // Terms beyond the protocol are clamped again.
        let wide = "canvas=v1:99999:9:1:1:0:0:0:50".to_owned();
        let read = CanvasCapencArgs::from_args(&[wide]).unwrap().unwrap();
        assert_eq!(read.terms.max_canvas_px, 16_384);
        assert_eq!(read.terms.max_surfaces_per_monitor, 4);
    }
}
