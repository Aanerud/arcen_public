//! Pure `xrandr --query` output parser and applied-topology verifier for the
//! `multi_monitor_v1` Linux vertical slice.
//!
//! After the generated multi-head Xorg config
//! (`session::xorg_multihead::render_multi_head_xorg_config`) is written and
//! the dedicated Xorg server reports ready
//! (`session::launcher::DedicatedXorg::wait_ready`), the privileged launcher
//! runs `xrandr --query` against that display and this module verifies the
//! *exact* RandR state it reports — every planned output's geometry,
//! rotation, and primary flag, plus the overall `Screen 0: current WxH`
//! bounds — matches the [`LinuxTopologyPlan`] this session committed to
//! before any capenc child or client-facing `server_hello` is ever produced.
//! A generated Xorg config that Xorg silently reinterpreted differently (a
//! rejected MetaMode, a driver falling back to a default layout, a
//! disconnected head) must never be served to a client as if it were the
//! planned topology; this module is the one place that closes that gap.
//!
//! This module performs no process I/O itself: the caller runs `xrandr
//! --query` and passes this module its captured stdout text.
//!
//! # `xrandr --query` output shape this parser understands
//!
//! ```text
//! Screen 0: minimum 320 x 200, current 3200 x 1080, maximum 8192 x 8192
//! DFP-0 connected primary 1920x1080+0+0 (normal left inverted right x axis y axis) 521mm x 293mm
//!    1920x1080     60.00*+
//! DFP-1 connected 1280x720+1920+0 right (normal left inverted right x axis y axis) 400mm x 300mm
//!    1280x720      60.00*+
//! DFP-2 disconnected (normal left inverted right x axis y axis)
//! ```
//!
//! An output line's rotation word (`normal`/`left`/`inverted`/`right`) is
//! xrandr's *currently applied* transform, printed only when it is not
//! `normal`; the parenthesized `(normal left inverted right ...)` clause that
//! always follows is the output's list of *supported* rotations, not the
//! applied one, and this parser does not read it. Mode lines (indented,
//! starting with a resolution) are skipped: this parser only reads the one
//! `Screen` line and each un-indented output header line.

use crate::display::topology::LinuxTopologyPlan;
use arcen_media::Rotation;
use thiserror::Error;

/// One parsed RandR output header line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedRandrOutput {
    pub name: String,
    pub connected: bool,
    pub primary: bool,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub rotation: Rotation,
}

/// Complete parsed `xrandr --query` state this module reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedRandrState {
    pub screen_width: u32,
    pub screen_height: u32,
    pub outputs: Vec<ParsedRandrOutput>,
}

/// Typed rejection parsing or verifying `xrandr --query` text.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RandrVerifyError {
    #[error("xrandr --query output is empty")]
    EmptyOutput,
    #[error("xrandr --query output did not start with a recognized \"Screen 0:\" line")]
    MissingScreenLine,
    #[error("xrandr --query \"Screen\" line could not be parsed: {0:?}")]
    UnparsableScreenLine(String),
    #[error("xrandr --query output line could not be parsed as a connected output: {0:?}")]
    UnparsableOutputLine(String),
    #[error(
        "applied screen bounds {actual_width}x{actual_height} do not match the planned virtual framebuffer {expected_width}x{expected_height}"
    )]
    ScreenBoundsMismatch {
        expected_width: u32,
        expected_height: u32,
        actual_width: u32,
        actual_height: u32,
    },
    #[error("planned head {0:?} is not reported as a connected RandR output at all")]
    HeadNotConnected(String),
    #[error(
        "RandR reports connected output {0:?}, which is not any head this session's topology plan assigned — an unplanned/extra connected head must never be silently tolerated alongside the applied roster"
    )]
    UnexpectedConnectedHead(String),
    #[error(
        "RandR's connected-output order does not match the plan's dense head order: applied position {position} is {actual_head:?}, but the plan expects {expected_head:?} there. Every monitor's own NvFBC \"dense capture index\" (see `media::multi_capenc::dense_output_index`) assumes RandR enumerates connected outputs in exactly this plan order, so this mismatch is verified explicitly rather than assumed — left unchecked, it would silently route a monitor's capture to the wrong physical head"
    )]
    OutputOrderMismatch {
        position: usize,
        expected_head: String,
        actual_head: String,
    },
    #[error(
        "planned head {head:?} geometry {expected_x}+{expected_y} {expected_width}x{expected_height} does not match applied {actual_x}+{actual_y} {actual_width}x{actual_height}"
    )]
    GeometryMismatch {
        head: String,
        expected_x: i32,
        expected_y: i32,
        expected_width: u32,
        expected_height: u32,
        actual_x: i32,
        actual_y: i32,
        actual_width: u32,
        actual_height: u32,
    },
    #[error(
        "planned head {head:?} rotation {expected:?} does not match applied rotation {actual:?}"
    )]
    RotationMismatch {
        head: String,
        expected: Rotation,
        actual: Rotation,
    },
    #[error("planned head {head:?} primary={expected} does not match applied primary={actual}")]
    PrimaryMismatch {
        head: String,
        expected: bool,
        actual: bool,
    },
}

/// Maps an xrandr current-rotation word to [`Rotation`]. Exactly the inverse
/// of `session::xorg_multihead::rotation_token`'s NVIDIA MetaModes mapping:
/// `"right"` is 90 degrees clockwise, `"left"` is 270 degrees clockwise (90
/// degrees counter-clockwise), `"inverted"` is 180 degrees, and the absent
/// word (xrandr's implicit default) is `"normal"`/[`Rotation::Degrees0`].
fn parse_rotation_word(word: &str) -> Option<Rotation> {
    match word {
        "normal" => Some(Rotation::Degrees0),
        "right" => Some(Rotation::Degrees90),
        "inverted" => Some(Rotation::Degrees180),
        "left" => Some(Rotation::Degrees270),
        _ => None,
    }
}

/// NVIDIA's Xorg configuration vocabulary uses `DFP-N` while RandR exposes
/// the same VGX connectors as `DVI-D-N` on the current GRID driver. Treat
/// those names as exact aliases only when both suffixes parse to the same
/// connector index; no fuzzy/name-only matching is permitted.
fn same_nvidia_head(planned: &str, applied: &str) -> bool {
    if planned == applied {
        return true;
    }
    fn connector_index(name: &str) -> Option<u8> {
        name.strip_prefix("DFP-")
            .or_else(|| name.strip_prefix("DVI-D-"))?
            .parse()
            .ok()
    }
    connector_index(planned)
        .zip(connector_index(applied))
        .is_some_and(|(planned_index, applied_index)| planned_index == applied_index)
}

/// Parses one `WxH+X+Y` geometry token (e.g. `"1920x1080+0+0"`).
fn parse_geometry_token(token: &str) -> Option<(i32, i32, u32, u32)> {
    let (size, rest) = token.split_once('+')?;
    let (x, y) = rest.split_once('+')?;
    let (width, height) = size.split_once('x')?;
    Some((
        x.parse().ok()?,
        y.parse().ok()?,
        width.parse().ok()?,
        height.parse().ok()?,
    ))
}

/// Parses the leading `Screen 0: minimum ... , current W x H, maximum ...`
/// line, returning `(width, height)` from its `current` clause.
fn parse_screen_line(line: &str) -> Result<(u32, u32), RandrVerifyError> {
    let current = line
        .split(',')
        .find_map(|clause| clause.trim().strip_prefix("current "))
        .ok_or_else(|| RandrVerifyError::UnparsableScreenLine(line.to_owned()))?;
    let mut tokens = current.split_whitespace();
    let width = tokens.next().and_then(|value| value.parse::<u32>().ok());
    let separator = tokens.next();
    let height = tokens.next().and_then(|value| value.parse::<u32>().ok());
    match (width, separator, height) {
        (Some(width), Some("x"), Some(height)) => Ok((width, height)),
        _ => Err(RandrVerifyError::UnparsableScreenLine(line.to_owned())),
    }
}

/// Parses one un-indented output header line (e.g.
/// `"DFP-0 connected primary 1920x1080+0+0 (normal ...) 521mm x 293mm"`), or
/// returns `None` for a `disconnected` output (never planned against, so this
/// parser has nothing useful to check on it).
fn parse_output_line(line: &str) -> Result<Option<ParsedRandrOutput>, RandrVerifyError> {
    let mut tokens = line.split_whitespace();
    let Some(name) = tokens.next() else {
        return Err(RandrVerifyError::UnparsableOutputLine(line.to_owned()));
    };
    let Some(state) = tokens.next() else {
        return Err(RandrVerifyError::UnparsableOutputLine(line.to_owned()));
    };
    if state == "disconnected" {
        return Ok(None);
    }
    if state != "connected" {
        return Err(RandrVerifyError::UnparsableOutputLine(line.to_owned()));
    }
    let mut next = tokens
        .next()
        .ok_or_else(|| RandrVerifyError::UnparsableOutputLine(line.to_owned()))?;
    let primary = next == "primary";
    if primary {
        next = tokens
            .next()
            .ok_or_else(|| RandrVerifyError::UnparsableOutputLine(line.to_owned()))?;
    }
    let (x, y, width, height) = parse_geometry_token(next)
        .ok_or_else(|| RandrVerifyError::UnparsableOutputLine(line.to_owned()))?;
    // The current-rotation word, when present, is the very next token and is
    // never itself the start of the parenthesized supported-rotations
    // clause; its absence means xrandr's implicit "normal".
    let rotation = match tokens.next() {
        Some(word) if !word.starts_with('(') => parse_rotation_word(word)
            .ok_or_else(|| RandrVerifyError::UnparsableOutputLine(line.to_owned()))?,
        _ => Rotation::Degrees0,
    };
    Ok(Some(ParsedRandrOutput {
        name: name.to_owned(),
        connected: true,
        primary,
        x,
        y,
        width,
        height,
        rotation,
    }))
}

/// Parses complete `xrandr --query` stdout text into [`ParsedRandrState`].
///
/// # Errors
///
/// Returns an error when the output is empty, the leading `Screen` line is
/// missing/unparsable, or a connected output's header line cannot be parsed.
/// Disconnected outputs and indented mode lines are silently skipped.
pub fn parse_xrandr_query(text: &str) -> Result<ParsedRandrState, RandrVerifyError> {
    let mut lines = text.lines();
    let screen_line = lines.next().ok_or(RandrVerifyError::EmptyOutput)?;
    if !screen_line.trim_start().starts_with("Screen") {
        return Err(RandrVerifyError::MissingScreenLine);
    }
    let (screen_width, screen_height) = parse_screen_line(screen_line)?;
    let mut outputs = Vec::new();
    for line in lines {
        // Mode lines are always indented; output header lines never are.
        if line.is_empty() || line.starts_with(char::is_whitespace) {
            continue;
        }
        if let Some(output) = parse_output_line(line)? {
            outputs.push(output);
        }
    }
    Ok(ParsedRandrState {
        screen_width,
        screen_height,
        outputs,
    })
}

/// Verifies that `xrandr_query_output` (this display's captured `xrandr
/// --query` stdout) exactly matches every geometry/rotation/primary flag
/// [`LinuxTopologyPlan::monitors`] planned, plus the overall bounding
/// `Screen` size.
///
/// Verification is exact in three independent ways, checked in this order:
/// 1. **Roster**: the set of RandR-connected outputs must equal the set of
///    planned heads exactly — no head connected that the plan did not
///    assign ([`RandrVerifyError::UnexpectedConnectedHead`]), and no
///    planned head left unconnected
///    ([`RandrVerifyError::HeadNotConnected`]).
/// 2. **Dense order**: once the roster is known to match, RandR's own
///    connected-output enumeration order must match `plan.monitors`'
///    order position-for-position
///    ([`RandrVerifyError::OutputOrderMismatch`]) — every monitor's NvFBC
///    capture ordinal (`media::multi_capenc::dense_output_index`) is that
///    monitor's *position in the plan*, on the documented assumption that
///    NvFBC (like RandR) enumerates connected outputs in this same
///    plan-declared order; a same-set-but-reordered applied state would
///    otherwise "verify" while silently routing capture to the wrong
///    physical head, so this is checked explicitly rather than assumed.
/// 3. **Per-head applied state**: each planned monitor's geometry,
///    rotation, and primary flag.
///
/// # Errors
///
/// Returns [`RandrVerifyError`] the moment any planned monitor's assigned
/// head is missing/disconnected, any extra head is connected beyond the
/// plan, RandR's connected-output order does not match the plan's order,
/// any planned head's applied geometry, rotation, or primary flag differs
/// from the plan, or the overall applied screen bounds differ from
/// [`LinuxTopologyPlan::virtual_width`]/[`LinuxTopologyPlan::virtual_height`].
/// No partial match is accepted: every planned monitor must verify exactly.
pub fn verify_applied_topology(
    xrandr_query_output: &str,
    plan: &LinuxTopologyPlan,
) -> Result<(), RandrVerifyError> {
    let state = parse_xrandr_query(xrandr_query_output)?;
    if state.screen_width != plan.virtual_width || state.screen_height != plan.virtual_height {
        return Err(RandrVerifyError::ScreenBoundsMismatch {
            expected_width: plan.virtual_width,
            expected_height: plan.virtual_height,
            actual_width: state.screen_width,
            actual_height: state.screen_height,
        });
    }
    // Roster, no-extras direction: every RandR-connected output must be one
    // of this plan's assigned heads. Checked before the missing-head
    // direction below so an unplanned/hotplugged extra output is reported
    // as exactly that, not folded into a generic mismatch.
    for applied in &state.outputs {
        if !plan
            .monitors
            .iter()
            .any(|monitor| same_nvidia_head(&monitor.head, &applied.name))
        {
            return Err(RandrVerifyError::UnexpectedConnectedHead(
                applied.name.clone(),
            ));
        }
    }
    // Roster, no-missing direction: every planned head must be connected.
    for monitor in &plan.monitors {
        if !state
            .outputs
            .iter()
            .any(|applied| same_nvidia_head(&monitor.head, &applied.name))
        {
            return Err(RandrVerifyError::HeadNotConnected(monitor.head.clone()));
        }
    }
    // Dense order: the roster is now known to be an exact set match (same
    // length, same names) — but `state.outputs`' order, as RandR actually
    // reported it, may still be a permutation of `plan.monitors`' order.
    // Verify that assumption explicitly, position-for-position, rather than
    // relying on the name-keyed lookup below (which would silently accept
    // any permutation).
    for (position, (monitor, applied)) in plan.monitors.iter().zip(state.outputs.iter()).enumerate()
    {
        if !same_nvidia_head(&monitor.head, &applied.name) {
            return Err(RandrVerifyError::OutputOrderMismatch {
                position,
                expected_head: monitor.head.clone(),
                actual_head: applied.name.clone(),
            });
        }
    }
    for monitor in &plan.monitors {
        let applied = state
            .outputs
            .iter()
            .find(|output| same_nvidia_head(&monitor.head, &output.name))
            .expect("roster and order already verified exactly above");
        if applied.x != monitor.x
            || applied.y != monitor.y
            || applied.width != monitor.width
            || applied.height != monitor.height
        {
            return Err(RandrVerifyError::GeometryMismatch {
                head: monitor.head.clone(),
                expected_x: monitor.x,
                expected_y: monitor.y,
                expected_width: monitor.width,
                expected_height: monitor.height,
                actual_x: applied.x,
                actual_y: applied.y,
                actual_width: applied.width,
                actual_height: applied.height,
            });
        }
        if applied.rotation != monitor.rotation {
            return Err(RandrVerifyError::RotationMismatch {
                head: monitor.head.clone(),
                expected: monitor.rotation,
                actual: applied.rotation,
            });
        }
        if applied.primary != monitor.primary {
            return Err(RandrVerifyError::PrimaryMismatch {
                head: monitor.head.clone(),
                expected: monitor.primary,
                actual: applied.primary,
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display::topology::{plan_topology, HeadInventory, VALID_HEAD_TOKENS};
    use arcen_media::{
        Monitor, MonitorIdentity, RequestedMonitor, RequestedMonitorTopology, TopologyGeneration,
    };

    // NOTE: built with `concat!` rather than backslash-newline string
    // continuation, because Rust's `\`-newline continuation also strips the
    // *next* line's leading whitespace — which would silently delete the
    // indentation these fixtures rely on to distinguish an output header
    // line from an indented mode line.
    const ONE_HEAD: &str = concat!(
        "Screen 0: minimum 320 x 200, current 1920 x 1080, maximum 8192 x 8192\n",
        "DFP-0 connected primary 1920x1080+0+0 (normal left inverted right x axis y axis) 521mm x 293mm\n",
        "   1920x1080     60.00*+\n",
    );

    const TWO_HEAD: &str = concat!(
        "Screen 0: minimum 320 x 200, current 3200 x 1080, maximum 8192 x 8192\n",
        "DFP-0 connected primary 1920x1080+0+0 (normal left inverted right x axis y axis) 521mm x 293mm\n",
        "   1920x1080     60.00*+\n",
        "DFP-1 connected 1280x720+1920+0 (normal left inverted right x axis y axis) 400mm x 300mm\n",
        "   1280x720      60.00*+\n",
        "DFP-2 disconnected (normal left inverted right x axis y axis)\n",
    );

    const TWO_HEAD_RANDR_DVI_ALIASES: &str = concat!(
        "Screen 0: minimum 8 x 8, current 3200 x 1080, maximum 30720 x 17280\n",
        "DVI-D-0 connected primary 1920x1080+0+0 (normal left inverted right x axis y axis) 521mm x 293mm\n",
        "   2560x1600     59.86*+\n",
        "DVI-D-1 connected 1280x720+1920+0 (normal left inverted right x axis y axis) 400mm x 300mm\n",
        "   2560x1600     59.86*+\n",
    );

    const ROTATED_HEAD: &str = concat!(
        "Screen 0: minimum 320 x 200, current 1080 x 1920, maximum 8192 x 8192\n",
        "DFP-0 connected primary 1080x1920+0+0 right (normal left inverted right x axis y axis) 293mm x 521mm\n",
        "   1920x1080     60.00*+\n",
    );

    const ONE_HEAD_PLUS_UNPLANNED_EXTRA: &str = concat!(
        "Screen 0: minimum 320 x 200, current 1920 x 1080, maximum 8192 x 8192\n",
        "DFP-0 connected primary 1920x1080+0+0 (normal left inverted right x axis y axis) 521mm x 293mm\n",
        "   1920x1080     60.00*+\n",
        "DFP-1 connected 1280x720+1920+0 (normal left inverted right x axis y axis) 400mm x 300mm\n",
        "   1280x720      60.00*+\n",
    );

    // Same two connected outputs and overall bounds as `TWO_HEAD`, but with
    // the output header (and its mode line) for `DFP-1` listed *before*
    // `DFP-0` — exactly the same set, individually-correct geometry for
    // each head, just RandR's own reported enumeration order reversed
    // relative to the plan's `[DFP-0 primary, DFP-1 second]` order.
    const TWO_HEAD_REORDERED: &str = concat!(
        "Screen 0: minimum 320 x 200, current 3200 x 1080, maximum 8192 x 8192\n",
        "DFP-1 connected 1280x720+1920+0 (normal left inverted right x axis y axis) 400mm x 300mm\n",
        "   1280x720      60.00*+\n",
        "DFP-0 connected primary 1920x1080+0+0 (normal left inverted right x axis y axis) 521mm x 293mm\n",
        "   1920x1080     60.00*+\n",
    );

    fn requested_monitor(
        id: &str,
        x: i32,
        y: i32,
        width_px: u32,
        height_px: u32,
        primary: bool,
        rotation: Rotation,
    ) -> RequestedMonitor {
        let monitor = Monitor {
            identity: MonitorIdentity {
                id: id.to_owned(),
                name: format!("Display {id}"),
                ..MonitorIdentity::default()
            },
            x,
            y,
            width_px,
            height_px,
            scale: 1.0,
            refresh_hz: 60,
            rotation,
            primary,
            width_mm: 0.0,
            height_mm: 0.0,
        };
        RequestedMonitor::new(monitor, width_px, height_px).expect("requested monitor")
    }

    fn plan_for(monitors: Vec<RequestedMonitor>, heads: usize) -> LinuxTopologyPlan {
        let requested = RequestedMonitorTopology::new(monitors).expect("requested topology");
        let generation = TopologyGeneration::new(1).expect("generation");
        let inventory =
            HeadInventory::uniform(VALID_HEAD_TOKENS.iter().take(heads).copied()).expect("heads");
        plan_topology(&requested, generation, &inventory).expect("plan")
    }

    #[test]
    fn parses_the_screen_line_and_one_connected_primary_output() {
        let state = parse_xrandr_query(ONE_HEAD).expect("parsed");
        assert_eq!(state.screen_width, 1920);
        assert_eq!(state.screen_height, 1080);
        assert_eq!(
            state.outputs,
            vec![ParsedRandrOutput {
                name: "DFP-0".to_owned(),
                connected: true,
                primary: true,
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
                rotation: Rotation::Degrees0,
            }]
        );
    }

    #[test]
    fn parses_two_connected_outputs_and_skips_a_disconnected_one() {
        let state = parse_xrandr_query(TWO_HEAD).expect("parsed");
        assert_eq!(state.screen_width, 3200);
        assert_eq!(state.screen_height, 1080);
        assert_eq!(state.outputs.len(), 2);
        assert_eq!(state.outputs[1].name, "DFP-1");
        assert!(!state.outputs[1].primary);
        assert_eq!(state.outputs[1].x, 1920);
    }

    #[test]
    fn verifies_randr_dvi_aliases_against_planned_dfp_heads() {
        let plan = plan_for(
            vec![
                requested_monitor("primary", 0, 0, 1920, 1080, true, Rotation::Degrees0),
                requested_monitor("second", 1920, 0, 1280, 720, false, Rotation::Degrees0),
            ],
            2,
        );
        assert_eq!(
            verify_applied_topology(TWO_HEAD_RANDR_DVI_ALIASES, &plan),
            Ok(())
        );
        assert!(same_nvidia_head("DFP-0", "DVI-D-0"));
        assert!(!same_nvidia_head("DFP-0", "DVI-D-1"));
    }

    #[test]
    fn parses_a_non_normal_current_rotation_word() {
        let state = parse_xrandr_query(ROTATED_HEAD).expect("parsed");
        assert_eq!(state.outputs[0].rotation, Rotation::Degrees90);
        assert_eq!(state.outputs[0].width, 1080);
        assert_eq!(state.outputs[0].height, 1920);
    }

    #[test]
    fn rejects_empty_output() {
        assert_eq!(parse_xrandr_query(""), Err(RandrVerifyError::EmptyOutput));
    }

    #[test]
    fn rejects_output_missing_the_screen_line() {
        assert_eq!(
            parse_xrandr_query("DFP-0 connected primary 1920x1080+0+0\n"),
            Err(RandrVerifyError::MissingScreenLine)
        );
    }

    #[test]
    fn one_head_plan_verifies_against_matching_xrandr_output() {
        let plan = plan_for(
            vec![requested_monitor(
                "primary",
                0,
                0,
                1920,
                1080,
                true,
                Rotation::Degrees0,
            )],
            1,
        );
        assert_eq!(verify_applied_topology(ONE_HEAD, &plan), Ok(()));
    }

    #[test]
    fn two_head_plan_verifies_against_matching_xrandr_output() {
        let plan = plan_for(
            vec![
                requested_monitor("primary", 0, 0, 1920, 1080, true, Rotation::Degrees0),
                requested_monitor("second", 1920, 0, 1280, 720, false, Rotation::Degrees0),
            ],
            2,
        );
        assert_eq!(verify_applied_topology(TWO_HEAD, &plan), Ok(()));
    }

    #[test]
    fn rotated_head_plan_verifies_against_the_rotated_footprint() {
        let plan = plan_for(
            vec![requested_monitor(
                "primary",
                0,
                0,
                1920,
                1080,
                true,
                Rotation::Degrees90,
            )],
            1,
        );
        assert_eq!(verify_applied_topology(ROTATED_HEAD, &plan), Ok(()));
    }

    #[test]
    fn rejects_a_mismatched_overall_screen_bounds() {
        let plan = plan_for(
            vec![requested_monitor(
                "primary",
                0,
                0,
                1920,
                1080,
                true,
                Rotation::Degrees0,
            )],
            1,
        );
        let wrong_bounds = ONE_HEAD.replace("current 1920 x 1080", "current 1920 x 1200");
        assert_eq!(
            verify_applied_topology(&wrong_bounds, &plan),
            Err(RandrVerifyError::ScreenBoundsMismatch {
                expected_width: 1920,
                expected_height: 1080,
                actual_width: 1920,
                actual_height: 1200,
            })
        );
    }

    #[test]
    fn rejects_a_planned_head_that_is_disconnected() {
        let plan = plan_for(
            vec![requested_monitor(
                "primary",
                0,
                0,
                1920,
                1080,
                true,
                Rotation::Degrees0,
            )],
            1,
        );
        let disconnected =
            "Screen 0: minimum 320 x 200, current 1920 x 1080, maximum 8192 x 8192\n\
DFP-0 disconnected (normal left inverted right x axis y axis)\n";
        assert_eq!(
            verify_applied_topology(disconnected, &plan),
            Err(RandrVerifyError::HeadNotConnected("DFP-0".to_owned()))
        );
    }

    #[test]
    fn rejects_an_extra_connected_head_not_in_the_plan() {
        // The plan committed to exactly one monitor (`DFP-0`), but RandR
        // now also reports `DFP-1` connected — an extra head the plan never
        // assigned must be rejected, not silently tolerated alongside the
        // planned roster.
        let plan = plan_for(
            vec![requested_monitor(
                "primary",
                0,
                0,
                1920,
                1080,
                true,
                Rotation::Degrees0,
            )],
            1,
        );
        assert_eq!(
            verify_applied_topology(ONE_HEAD_PLUS_UNPLANNED_EXTRA, &plan),
            Err(RandrVerifyError::UnexpectedConnectedHead(
                "DFP-1".to_owned()
            ))
        );
    }

    #[test]
    fn rejects_a_dense_order_mismatch_even_when_every_head_individually_matches() {
        // Same connected-output set, same overall bounds, and each head's
        // own geometry/rotation/primary flag individually matches its
        // plan-assigned counterpart exactly — the only thing wrong is that
        // RandR enumerated `DFP-1` before `DFP-0`, the reverse of the
        // plan's `[DFP-0 primary, DFP-1 second]` order. A name-keyed lookup
        // alone would let this "verify" successfully and then silently
        // route every monitor's capenc capture to the wrong dense NvFBC
        // ordinal; this must be caught explicitly instead.
        let plan = plan_for(
            vec![
                requested_monitor("primary", 0, 0, 1920, 1080, true, Rotation::Degrees0),
                requested_monitor("second", 1920, 0, 1280, 720, false, Rotation::Degrees0),
            ],
            2,
        );
        assert_eq!(
            verify_applied_topology(TWO_HEAD_REORDERED, &plan),
            Err(RandrVerifyError::OutputOrderMismatch {
                position: 0,
                expected_head: "DFP-0".to_owned(),
                actual_head: "DFP-1".to_owned(),
            })
        );
    }

    #[test]
    fn rejects_a_mismatched_position() {
        let plan = plan_for(
            vec![
                requested_monitor("primary", 0, 0, 1920, 1080, true, Rotation::Degrees0),
                requested_monitor("second", 1920, 0, 1280, 720, false, Rotation::Degrees0),
            ],
            2,
        );
        // DFP-1 applied at +1921+0 instead of the planned +1920+0.
        let shifted = TWO_HEAD.replace("1280x720+1920+0", "1280x720+1921+0");
        assert!(matches!(
            verify_applied_topology(&shifted, &plan),
            Err(RandrVerifyError::GeometryMismatch { head, .. }) if head == "DFP-1"
        ));
    }

    #[test]
    fn rejects_a_mismatched_rotation() {
        let plan = plan_for(
            vec![requested_monitor(
                "primary",
                0,
                0,
                1920,
                1080,
                true,
                Rotation::Degrees0,
            )],
            1,
        );
        let inverted = ONE_HEAD.replace(
            "primary 1920x1080+0+0 (normal",
            "primary 1920x1080+0+0 inverted (normal",
        );
        assert_eq!(
            verify_applied_topology(&inverted, &plan),
            Err(RandrVerifyError::RotationMismatch {
                head: "DFP-0".to_owned(),
                expected: Rotation::Degrees0,
                actual: Rotation::Degrees180,
            })
        );
    }

    #[test]
    fn rejects_a_mismatched_primary_flag() {
        let plan = plan_for(
            vec![
                requested_monitor("primary", 0, 0, 1920, 1080, true, Rotation::Degrees0),
                requested_monitor("second", 1920, 0, 1280, 720, false, Rotation::Degrees0),
            ],
            2,
        );
        // Applied state disagrees about which head is primary.
        let swapped = TWO_HEAD
            .replace("DFP-0 connected primary", "DFP-0 connected")
            .replace("DFP-1 connected", "DFP-1 connected primary");
        assert!(matches!(
            verify_applied_topology(&swapped, &plan),
            Err(RandrVerifyError::PrimaryMismatch { head, .. }) if head == "DFP-0"
        ));
    }

    #[test]
    fn rejects_an_unparsable_output_line() {
        let broken = "Screen 0: minimum 320 x 200, current 1920 x 1080, maximum 8192 x 8192\nnot an output line at all\n";
        assert!(matches!(
            parse_xrandr_query(broken),
            Err(RandrVerifyError::UnparsableOutputLine(_))
        ));
    }
}
