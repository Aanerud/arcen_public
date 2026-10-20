//! Key, button and scroll planning shared by every Linux injector.
//!
//! Both the Xorg uinput devices and the session-bound Wayland injector speak
//! evdev codes, so the decisions that do not depend on a device — which key
//! and modifier edges a Deck key event produces, which releases a reset
//! needs, how legacy scroll becomes 120-unit wheel steps — live here once
//! and compile and test on every platform.

use std::collections::HashSet;

use super::keymap::{MODIFIER_CODES, MOD_ALT, MOD_CTRL, MOD_META, MOD_SHIFT};
use super::InputError;

/// evdev `BTN_LEFT`.
pub const BTN_LEFT: u16 = 0x110;
/// evdev `BTN_RIGHT`.
pub const BTN_RIGHT: u16 = 0x111;
/// evdev `BTN_MIDDLE`.
pub const BTN_MIDDLE: u16 = 0x112;

/// Hi-res wheel units per legacy detent.
pub const WHEEL_UNITS_PER_DETENT: i64 = 120;

/// The evdev button for a protocol mouse button (1 left, 2 middle, 3 right).
///
/// # Errors
///
/// [`InputError::InvalidButton`] for any other button.
pub fn mouse_button_code(button: u8) -> Result<u16, InputError> {
    match button {
        1 => Ok(BTN_LEFT),
        2 => Ok(BTN_MIDDLE),
        3 => Ok(BTN_RIGHT),
        button => Err(InputError::InvalidButton(button)),
    }
}

/// The key edges one Deck key event produces: the modifier mask reconciled
/// first (synthesising or releasing Shift, Ctrl, Alt and Meta), then the key
/// itself. Every edge is idempotent against `held`, which it updates.
pub fn plan_key_edges(
    held: &mut HashSet<u16>,
    code: u16,
    pressed: bool,
    modifiers: u32,
) -> Vec<(u16, bool)> {
    if is_modifier(code) {
        let mut edges = Vec::with_capacity(1);
        append_idempotent_edge(held, &mut edges, code, pressed);
        return edges;
    }
    let mut edges = plan_modifier_edges(held, modifiers);
    append_idempotent_edge(held, &mut edges, code, pressed);
    edges
}

/// The Shift, Ctrl, Alt and Meta edges that make `held` agree with a
/// compact wire modifier mask.
pub fn plan_modifier_edges(held: &mut HashSet<u16>, modifiers: u32) -> Vec<(u16, bool)> {
    let mut edges = Vec::with_capacity(5);
    for (bit, modifier_code) in [
        (MOD_SHIFT, 42),
        (MOD_CTRL, 29),
        (MOD_ALT, 56),
        (MOD_META, 125),
    ] {
        append_idempotent_edge(held, &mut edges, modifier_code, modifiers & bit != 0);
    }
    edges
}

fn append_idempotent_edge(
    held: &mut HashSet<u16>,
    edges: &mut Vec<(u16, bool)>,
    code: u16,
    pressed: bool,
) {
    let changed = if pressed {
        held.insert(code)
    } else {
        held.remove(&code)
    };
    if changed {
        edges.push((code, pressed));
    }
}

/// The button edge to emit, or `None` when `held` already agrees.
#[must_use]
pub fn plan_button_edge(held: &HashSet<u16>, code: u16, pressed: bool) -> Option<(u16, bool)> {
    (held.contains(&code) != pressed).then_some((code, pressed))
}

/// Records a button edge that was emitted.
pub fn commit_button_edge(held: &mut HashSet<u16>, code: u16, pressed: bool) {
    if pressed {
        held.insert(code);
    } else {
        held.remove(&code);
    }
}

/// Every key and button to release on a full reset, plus every modifier.
#[must_use]
pub fn plan_reset_releases(held_keys: &HashSet<u16>, held_buttons: &HashSet<u16>) -> Vec<u16> {
    let mut codes: Vec<u16> = held_keys
        .iter()
        .chain(held_buttons.iter())
        .copied()
        .collect();
    for code in MODIFIER_CODES {
        if !codes.contains(&code) {
            codes.push(code);
        }
    }
    codes
}

/// Every key to release on a keyboard-only reset, plus every modifier,
/// sorted. Buttons are never part of it.
#[must_use]
pub fn plan_keyboard_reset_releases(held_keys: &HashSet<u16>) -> Vec<u16> {
    let mut codes = held_keys.iter().copied().collect::<Vec<_>>();
    for code in MODIFIER_CODES {
        if !codes.contains(&code) {
            codes.push(code);
        }
    }
    codes.sort_unstable();
    codes
}

/// Whether `code` is one of the eight modifier keys.
#[must_use]
pub fn is_modifier(code: u16) -> bool {
    MODIFIER_CODES.contains(&code)
}

/// Whole legacy scroll lines, never rounding a nonzero value to zero.
#[must_use]
pub fn scroll_steps(value: f64) -> i32 {
    if value == 0.0 || !value.is_finite() {
        return 0;
    }
    let rounded = value.round() as i32;
    if rounded == 0 {
        value.signum() as i32
    } else {
        rounded
    }
}

/// A legacy scroll value in 120-unit hi-res wheel units.
#[must_use]
pub fn legacy_scroll_to_wheel_units(value: f64, unit: arcen_input::ScrollUnit) -> i64 {
    match unit {
        arcen_input::ScrollUnit::Line => i64::from(scroll_steps(value)) * WHEEL_UNITS_PER_DETENT,
        arcen_input::ScrollUnit::Point => {
            if value == 0.0 || !value.is_finite() {
                0
            } else {
                let rounded = value.round();
                if rounded == 0.0 {
                    value.signum() as i64
                } else if rounded > i64::MAX as f64 {
                    i64::MAX
                } else if rounded < i64::MIN as f64 {
                    i64::MIN
                } else {
                    rounded as i64
                }
            }
        }
    }
}

/// Adds `hi_res_units` to `remainder` and returns the whole legacy detents
/// it completes, keeping the rest.
pub fn split_legacy_detents(remainder: &mut i64, hi_res_units: i64) -> i32 {
    let total = remainder.saturating_add(hi_res_units);
    let detents = total / WHEEL_UNITS_PER_DETENT;
    *remainder = total % WHEEL_UNITS_PER_DETENT;
    i32::try_from(detents).unwrap_or(if detents.is_negative() {
        i32::MIN
    } else {
        i32::MAX
    })
}

/// Hi-res wheel units clamped to an evdev value.
#[must_use]
pub fn saturating_hi_res_units(value: i64) -> i32 {
    if value == 0 {
        return 0;
    }
    i32::try_from(value).unwrap_or(if value.is_negative() {
        i32::MIN
    } else {
        i32::MAX
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fractional_scroll_preserves_direction() {
        assert_eq!(scroll_steps(0.1), 1);
        assert_eq!(scroll_steps(-0.1), -1);
        assert_eq!(scroll_steps(f64::NAN), 0);
        assert_eq!(
            legacy_scroll_to_wheel_units(0.1, arcen_input::ScrollUnit::Line),
            120
        );
        assert_eq!(
            legacy_scroll_to_wheel_units(30.4, arcen_input::ScrollUnit::Point),
            30
        );
    }

    #[test]
    fn hi_res_scroll_accumulates_legacy_detents() {
        let mut remainder = 0;
        assert_eq!(split_legacy_detents(&mut remainder, 30), 0);
        assert_eq!(remainder, 30);
        assert_eq!(split_legacy_detents(&mut remainder, 89), 0);
        assert_eq!(remainder, 119);
        assert_eq!(split_legacy_detents(&mut remainder, 1), 1);
        assert_eq!(remainder, 0);
        assert_eq!(split_legacy_detents(&mut remainder, -180), -1);
        assert_eq!(remainder, -60);
        assert_eq!(split_legacy_detents(&mut remainder, -60), -1);
        assert_eq!(remainder, 0);
        assert_eq!(saturating_hi_res_units(i64::MAX), i32::MAX);
        assert_eq!(saturating_hi_res_units(i64::MIN), i32::MIN);
    }

    #[test]
    fn missing_ctrl_edge_is_synthesized_before_ctrl_a() {
        let mut held = HashSet::new();
        assert_eq!(
            plan_key_edges(&mut held, 30, true, MOD_CTRL),
            vec![(29, true), (30, true)]
        );
    }

    #[test]
    fn explicit_modifier_edges_are_idempotent() {
        let mut held = HashSet::new();
        assert_eq!(
            plan_key_edges(&mut held, 29, true, MOD_CTRL),
            vec![(29, true)]
        );
        assert!(plan_key_edges(&mut held, 29, true, MOD_CTRL).is_empty());
        assert_eq!(plan_key_edges(&mut held, 29, false, 0), vec![(29, false)]);
        assert!(plan_key_edges(&mut held, 29, false, 0).is_empty());
    }

    #[test]
    fn modifier_mask_reconciles_shift_alt_meta_and_releases_stale_state() {
        let mut held = HashSet::from([29]);
        assert_eq!(
            plan_key_edges(&mut held, 2, true, MOD_SHIFT | MOD_ALT | MOD_META),
            vec![(42, true), (29, false), (56, true), (125, true), (2, true)]
        );
    }

    #[test]
    fn cmd_to_ctrl_policy_arrives_as_compact_ctrl_and_synthesizes_ctrl() {
        let mut held = HashSet::new();
        assert_eq!(
            plan_key_edges(&mut held, 31, true, MOD_CTRL),
            vec![(29, true), (31, true)]
        );
    }

    #[test]
    fn a_repeated_press_of_a_held_key_emits_nothing() {
        let mut held = HashSet::new();
        assert_eq!(plan_key_edges(&mut held, 30, true, 0), vec![(30, true)]);
        assert!(plan_key_edges(&mut held, 30, true, 0).is_empty());
        assert_eq!(plan_key_edges(&mut held, 30, false, 0), vec![(30, false)]);
    }

    #[test]
    fn drag_focus_loss_releases_held_mouse_button_on_reset() {
        let held_keys = HashSet::new();
        let mut held_buttons = HashSet::new();
        assert_eq!(
            plan_button_edge(&held_buttons, BTN_LEFT, true),
            Some((BTN_LEFT, true))
        );
        commit_button_edge(&mut held_buttons, BTN_LEFT, true);
        assert!(plan_button_edge(&held_buttons, BTN_LEFT, true).is_none());
        assert!(plan_reset_releases(&held_keys, &held_buttons).contains(&BTN_LEFT));
    }

    #[test]
    fn keyboard_reset_never_releases_a_held_mouse_button() {
        let held_keys = HashSet::from([30]);
        let held_buttons = HashSet::from([BTN_LEFT]);
        let keyboard_releases = plan_keyboard_reset_releases(&held_keys);
        assert!(keyboard_releases.contains(&30));
        assert!(!keyboard_releases.contains(&BTN_LEFT));
        assert!(plan_reset_releases(&held_keys, &held_buttons).contains(&BTN_LEFT));
    }

    #[test]
    fn explicit_mouse_release_is_idempotent_and_clears_tracking() {
        let mut held = HashSet::new();
        assert_eq!(
            plan_button_edge(&held, BTN_RIGHT, true),
            Some((BTN_RIGHT, true))
        );
        commit_button_edge(&mut held, BTN_RIGHT, true);
        assert_eq!(
            plan_button_edge(&held, BTN_RIGHT, false),
            Some((BTN_RIGHT, false))
        );
        commit_button_edge(&mut held, BTN_RIGHT, false);
        assert!(plan_button_edge(&held, BTN_RIGHT, false).is_none());
        assert!(held.is_empty());
    }

    #[test]
    fn protocol_buttons_map_to_evdev_buttons() {
        assert_eq!(mouse_button_code(1).unwrap(), BTN_LEFT);
        assert_eq!(mouse_button_code(2).unwrap(), BTN_MIDDLE);
        assert_eq!(mouse_button_code(3).unwrap(), BTN_RIGHT);
        assert!(mouse_button_code(4).is_err());
    }
}
