//! Turning a stream of pen samples into the edges a host must inject.
//!
//! A pen sample says what the tool *is* doing. A host has to inject what
//! *changed*: a tip that was down and is now up has to produce a release, and
//! a tool that has left the tablet has to produce one for everything it was
//! holding. Deciding that is policy, not platform, so it lives here and each
//! host maps the result onto its own event API — evdev codes on Linux,
//! `CGEvent` tablet events on macOS.
//!
//! The invariants below are the reason this is shared rather than written
//! twice. Each exists because getting it wrong strands input on someone's
//! desktop, and none of them is visible from the platform API being called.

use serde::{Deserialize, Serialize};

use crate::PenTool;

/// How many barrel buttons a sample may report.
///
/// The wire carries a `u16` bitmask. Only the low bits are meaningful, and
/// bounding the count keeps a malformed peer from making a host iterate
/// sixteen thousand times per sample.
pub const MAX_BARREL_BUTTONS: u8 = 8;

/// What the tool was doing when it was last seen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PenToolState {
    /// Whether the tool was within digitizer proximity.
    pub in_proximity: bool,
    /// Whether the tip was touching the surface.
    pub touching: bool,
    /// Barrel buttons held, as a bitmask.
    pub buttons: u16,
    /// Which end of the tool was in use.
    pub tool: PenTool,
}

// Written out rather than derived: `PenTool` has no meaningful default of its
// own, and giving it one would put an arbitrary answer on a shared type that
// other code can reach. A *state* does have a sensible starting value — a tool
// that is away from the tablet holding nothing — so the default lives here,
// where "tip" is a placeholder the first sample replaces rather than a claim.
impl Default for PenToolState {
    fn default() -> Self {
        Self::released(PenTool::Tip)
    }
}

impl PenToolState {
    /// Returns the state of a tool that is away from the tablet holding
    /// nothing.
    #[must_use]
    pub const fn released(tool: PenTool) -> Self {
        Self {
            in_proximity: false,
            touching: false,
            buttons: 0,
            tool,
        }
    }

    /// Returns whether anything is logically held.
    #[must_use]
    pub const fn holds_anything(self) -> bool {
        self.touching || self.buttons != 0
    }
}

/// A single transition a host must inject.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "edge")]
pub enum PenEdge {
    /// The tool entered digitizer proximity.
    ToolIn(PenTool),
    /// The tool left digitizer proximity.
    ToolOut(PenTool),
    /// The tip made contact.
    TipDown,
    /// The tip broke contact.
    TipUp,
    /// A barrel button changed.
    Barrel {
        /// Zero-based button index.
        index: u8,
        /// Whether it is now held.
        pressed: bool,
    },
}

/// Plans the edges between `previous` and the sample described by the
/// arguments, and returns the state to remember.
///
/// Three rules are enforced here rather than by callers:
///
/// * **A sample out of proximity is fully released.** Whatever the peer put in
///   `touching` or `buttons`, a tool that is not on the tablet is holding
///   nothing. A stale or malformed payload must not be able to leave a tip or
///   a barrel button logically down once the pen has physically lifted away.
/// * **Entering proximity asserts the tool before any press.** A press that
///   arrives before the tool is known is attributed to nothing.
/// * **Leaving proximity releases presses before the tool.** This is the order
///   a physical tablet reports, and the reverse strands the press.
///
/// A tool change while in proximity (tip to eraser) is reported as the old
/// tool leaving and the new one entering, with everything the old tool held
/// released first, because the two ends are different pointers to the host.
#[must_use]
pub fn plan_pen_edges(
    previous: PenToolState,
    tool: PenTool,
    in_proximity: bool,
    touching: bool,
    buttons: u16,
) -> (Vec<PenEdge>, PenToolState) {
    // Out of proximity is holding nothing, whatever the sample claims.
    let (touching, buttons) = if in_proximity {
        (touching, buttons)
    } else {
        (false, 0)
    };

    let mut edges = Vec::new();
    let tool_changed = previous.in_proximity && in_proximity && previous.tool != tool;

    // Release everything the previous tool held before it stops being the
    // tool, whether it is leaving or being swapped for the other end.
    if (previous.in_proximity && !in_proximity) || tool_changed {
        release_held(&mut edges, previous);
        edges.push(PenEdge::ToolOut(previous.tool));
        // The swap continues below as a fresh entry.
        if !tool_changed {
            return (edges, PenToolState::released(tool));
        }
    }

    let entering = (!previous.in_proximity && in_proximity) || tool_changed;
    if entering {
        edges.push(PenEdge::ToolIn(tool));
    }

    // After an entry the tool holds nothing, so edges are measured from a
    // released baseline rather than from a previous tool's state.
    let baseline = if entering {
        PenToolState::released(tool)
    } else {
        previous
    };

    if in_proximity {
        // Barrel buttons before the tip, matching the Linux host, which has
        // been in the field long enough for applications to have been written
        // against the order it produces. The two hosts disagreeing about the
        // sequence for the same physical action is exactly the drift this
        // module exists to prevent, and the shipped order wins over the one
        // that merely reads more naturally.
        plan_barrel_edges(&mut edges, baseline.buttons, buttons);
        if touching != baseline.touching {
            edges.push(if touching {
                PenEdge::TipDown
            } else {
                PenEdge::TipUp
            });
        }
    }

    (
        edges,
        PenToolState {
            in_proximity,
            touching,
            buttons,
            tool,
        },
    )
}

fn release_held(edges: &mut Vec<PenEdge>, state: PenToolState) {
    // Same order as a press, for the same reason: buttons then tip, then the
    // tool itself, which the caller appends.
    plan_barrel_edges(edges, state.buttons, 0);
    if state.touching {
        edges.push(PenEdge::TipUp);
    }
}

fn plan_barrel_edges(edges: &mut Vec<PenEdge>, previous: u16, current: u16) {
    let changed = previous ^ current;
    if changed == 0 {
        return;
    }
    for index in 0..MAX_BARREL_BUTTONS {
        let mask = 1_u16 << u16::from(index);
        if changed & mask != 0 {
            edges.push(PenEdge::Barrel {
                index,
                pressed: current & mask != 0,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn away() -> PenToolState {
        PenToolState::released(PenTool::Tip)
    }

    #[test]
    fn barrel_buttons_are_reported_before_the_tip_as_linux_does() {
        // The two hosts must agree on the order for the same physical action.
        let hovering = PenToolState {
            in_proximity: true,
            touching: false,
            buttons: 0,
            tool: PenTool::Tip,
        };
        let (edges, _) = plan_pen_edges(hovering, PenTool::Tip, true, true, 0b01);
        assert_eq!(
            edges,
            vec![
                PenEdge::Barrel {
                    index: 0,
                    pressed: true
                },
                PenEdge::TipDown,
            ]
        );
    }

    #[test]
    fn entering_proximity_reports_the_tool_before_any_press() {
        let (edges, state) = plan_pen_edges(away(), PenTool::Tip, true, true, 0);
        assert_eq!(edges, vec![PenEdge::ToolIn(PenTool::Tip), PenEdge::TipDown]);
        assert!(state.in_proximity && state.touching);
    }

    #[test]
    fn leaving_proximity_releases_the_tip_before_the_tool() {
        let down = PenToolState {
            in_proximity: true,
            touching: true,
            buttons: 0,
            tool: PenTool::Tip,
        };
        let (edges, state) = plan_pen_edges(down, PenTool::Tip, false, true, 0);
        assert_eq!(edges, vec![PenEdge::TipUp, PenEdge::ToolOut(PenTool::Tip)]);
        assert_eq!(state, PenToolState::released(PenTool::Tip));
    }

    #[test]
    fn a_sample_out_of_proximity_holds_nothing_whatever_it_claims() {
        // A peer insisting the tip is down and two buttons are held while the
        // pen is off the tablet must not leave anything stuck.
        let (edges, state) = plan_pen_edges(away(), PenTool::Tip, false, true, 0b11);
        assert!(edges.is_empty());
        assert!(!state.touching);
        assert_eq!(state.buttons, 0);
    }

    #[test]
    fn a_stale_out_of_proximity_sample_releases_what_was_held() {
        let holding = PenToolState {
            in_proximity: true,
            touching: true,
            buttons: 0b01,
            tool: PenTool::Tip,
        };
        let (edges, state) = plan_pen_edges(holding, PenTool::Tip, false, true, 0b11);
        assert_eq!(
            edges,
            vec![
                PenEdge::Barrel {
                    index: 0,
                    pressed: false
                },
                PenEdge::TipUp,
                PenEdge::ToolOut(PenTool::Tip),
            ]
        );
        assert!(!state.holds_anything());
    }

    #[test]
    fn flipping_to_the_eraser_swaps_the_tool_and_drops_what_the_tip_held() {
        let tip_down = PenToolState {
            in_proximity: true,
            touching: true,
            buttons: 0,
            tool: PenTool::Tip,
        };
        let (edges, state) = plan_pen_edges(tip_down, PenTool::Eraser, true, false, 0);
        assert_eq!(
            edges,
            vec![
                PenEdge::TipUp,
                PenEdge::ToolOut(PenTool::Tip),
                PenEdge::ToolIn(PenTool::Eraser),
            ]
        );
        assert_eq!(state.tool, PenTool::Eraser);
        assert!(!state.touching);
    }

    #[test]
    fn an_unchanged_sample_produces_no_edges() {
        let hovering = PenToolState {
            in_proximity: true,
            touching: false,
            buttons: 0b10,
            tool: PenTool::Tip,
        };
        let (edges, state) = plan_pen_edges(hovering, PenTool::Tip, true, false, 0b10);
        assert!(edges.is_empty());
        assert_eq!(state, hovering);
    }

    #[test]
    fn barrel_buttons_are_reported_individually_and_in_order() {
        let hovering = PenToolState {
            in_proximity: true,
            touching: false,
            buttons: 0b001,
            tool: PenTool::Tip,
        };
        let (edges, _) = plan_pen_edges(hovering, PenTool::Tip, true, false, 0b110);
        assert_eq!(
            edges,
            vec![
                PenEdge::Barrel {
                    index: 0,
                    pressed: false
                },
                PenEdge::Barrel {
                    index: 1,
                    pressed: true
                },
                PenEdge::Barrel {
                    index: 2,
                    pressed: true
                },
            ]
        );
    }

    #[test]
    fn buttons_above_the_supported_count_are_ignored_rather_than_iterated() {
        let hovering = PenToolState {
            in_proximity: true,
            touching: false,
            buttons: 0,
            tool: PenTool::Tip,
        };
        let (edges, state) = plan_pen_edges(hovering, PenTool::Tip, true, false, 0xFF00);
        assert!(edges.is_empty(), "no edge for unsupported button indices");
        // The state still records what was sent, so a later release of those
        // same bits does not look like a change either.
        assert_eq!(state.buttons, 0xFF00);
    }
}
