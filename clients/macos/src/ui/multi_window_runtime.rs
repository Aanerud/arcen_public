// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Native per-display fullscreen window plumbing for multi-monitor-v1.
//!
//! This module is the presentation-layer counterpart to
//! `crate::pipeline::monitor_router` (per-monitor frame routing) and
//! `crate::display::topology` (local requested topology / Separate Spaces
//! gate): it turns a validated 1-4 monitor plan into real, additional
//! `eframe`/`egui`/`winit` native windows, each pinned to exactly one macOS
//! display and placed into genuine `NSWindow` fullscreen (a real Space, via
//! `toggleFullScreen:` under the hood — winit and egui call this
//! "borderless" fullscreen purely because that is the same cross-platform
//! enum variant Windows/Linux use; on macOS it is never a fake/windowed
//! fullscreen).
//!
//! # Design: reuse the root viewport for the primary monitor
//!
//! [`crate::ui::app::run_native_app`] already opens exactly one `eframe` root
//! viewport (`egui::ViewportId::ROOT`), and that is kept as-is here: it always
//! presents the *first* (primary) negotiated monitor, preserving today's
//! single-monitor `VideoHeader` behavior byte-for-byte. This module only
//! plans and opens *additional* native fullscreen viewports for monitors
//! 2..=4, via `egui::Context::show_viewport_immediate` /
//! [`egui::ViewportBuilder::with_monitor`] + [`egui::ViewportBuilder::with_fullscreen`].
//! Session-global singletons (audio, clipboard, keyboard capture ownership,
//! disconnect) stay exactly where they are today, in the single
//! `ArcenApp`/session controller — this module never spawns a second session
//! or network controller.
//!
//! # Monitor targeting without new AppKit FFI
//!
//! `egui::ViewportBuilder::with_monitor(index)` targets "the monitor at
//! `index` in winit's `available_monitors()` order". On macOS, winit's
//! `available_monitors()` is exactly `CGDisplay::active_displays()` in order
//! (each `MonitorHandle`'s `native_identifier()` is the raw
//! `CGDirectDisplayID`) — the identical underlying `CGGetActiveDisplayList`
//! call `core_graphics::display::CGDisplay::active_displays()` already makes
//! (a dependency this crate already uses in `crate::display::topology`). So
//! resolving "which winit monitor index is display N" needs no new `unsafe`
//! AppKit call at all: [`live_active_display_ids`] calls the already-safe
//! `CGDisplay::active_displays()` and [`monitor_index_in`] finds the target
//! id's position in that same list, which is guaranteed to match winit's
//! enumeration order because both read the same OS list the same way.
//!
//! # Shared engine, Mac windows
//!
//! The plan and the transactional enter engine are shared
//! (`arcen_deck_core::multi_window::runtime`, generic over the window key);
//! this module keys them by `egui::ViewportId` and keeps what is AppKit's:
//! the CG display list, the viewport builder and the window lookups.
//!
//! # What is wired here vs. in `ArcenApp` itself
//!
//! Wired and tested in this module: the validated plan builder, the
//! transactional enter/rollback state machine (poll-based, since egui's
//! viewport API does not report window-creation failure back to app code
//! synchronously — see [`MultiWindowEnterAttempt`]), the monitor-index
//! resolver, the focus-follows-active-monitor decision, and (in the sibling
//! `crate::ui::multi_window_diagnostic` module) a real, independently
//! runnable native diagnostic that opens actual per-display fullscreen
//! windows painted with `crate::pipeline::synthetic_multi_monitor` patterns
//! and proves isolation — with no real host required, exactly the
//! synthetic-harness pattern used for the routing foundation.
//!
//! Hooking a *production* `ServerHello` with an applied multi-monitor-v1
//! topology (`crate::ui::multi_window::detect_multi_window_presentation_gap`)
//! into actually calling this module's plan/enter/rollback engine, and
//! per-window input-capture forwarding into the single session controller,
//! both live in `crate::ui::app::ArcenApp` and
//! `crate::ui::multi_window_activation` (see their own module docs) rather
//! than here; see `crate::ui::multi_window::multi_window_runtime_available`
//! for the flag that gates all of it, which this module does not read or
//! flip on its own.

pub use arcen_deck_core::multi_window::runtime::{
    active_monitor, display_id_from_client_display_id, evaluate_root_viewport_bind_during_move,
    evaluate_viewport_bind, monitor_index_in, viewport_bind_confirmed,
    viewport_confirmed_on_exact_display, window_title_for, ActiveDisplayInfo, ActiveMonitor,
    HardFailureKind, MonitorResolutionError, MonitorWindowMediaError, MultiWindowPlanError,
    ViewportBindEvaluation, ViewportBindObservation,
};
use arcen_media::SessionMonitorId;

/// One additional native fullscreen window, keyed by its egui viewport.
pub type MonitorWindowAssignment =
    arcen_deck_core::multi_window::runtime::MonitorWindowAssignment<egui::ViewportId>;
/// The validated multi-window plan, keyed by egui viewport.
pub type MultiWindowPlan =
    arcen_deck_core::multi_window::runtime::MultiWindowPlan<egui::ViewportId>;
/// Why a multi-window entry aborted, naming the egui viewport.
pub type MultiWindowAbortReason =
    arcen_deck_core::multi_window::runtime::MultiWindowAbortReason<egui::ViewportId>;
/// One poll of a multi-window entry.
pub type MultiWindowEnterPoll =
    arcen_deck_core::multi_window::runtime::MultiWindowEnterPoll<egui::ViewportId>;
/// The transactional multi-window entry, keyed by egui viewport.
pub type MultiWindowEnterAttempt =
    arcen_deck_core::multi_window::runtime::MultiWindowEnterAttempt<egui::ViewportId>;

/// Deterministic `ViewportId` for a session monitor id, stable across
/// frames/rebuilds of the same plan (egui viewport identity is derived from
/// a hash, not an incrementing counter).
#[must_use]
pub fn viewport_id_for(session_monitor_id: SessionMonitorId) -> egui::ViewportId {
    egui::ViewportId::from_hash_of(("arcen-deck-monitor-window", session_monitor_id.get()))
}

/// The shared plan for a full negotiated roster, with the root viewport
/// presenting the primary and one viewport per further monitor.
///
/// # Errors
///
/// See [`MultiWindowPlanError`].
pub fn build_plan(
    monitor_ids: &[SessionMonitorId],
    display_ids: &[u32],
) -> Result<MultiWindowPlan, MultiWindowPlanError> {
    MultiWindowPlan::build(
        monitor_ids,
        display_ids,
        egui::ViewportId::ROOT,
        viewport_id_for,
    )
}

/// Live wrapper for [`monitor_index_in`], reading the current
/// `CGDisplay::active_displays()` list.
///
/// # SAFETY
///
/// No `unsafe` block is needed here: `core_graphics::display::CGDisplay::active_displays()`
/// is already a safe function (the crate's own `unsafe extern "C"` FFI is
/// fully encapsulated inside `core-graphics`, an existing vetted
/// dependency), and is not main-thread-restricted (mirroring
/// `crate::display::topology::build_requested_topology`, which calls the
/// same underlying `CGGetActiveDisplayList` API off the main thread today).
#[must_use]
pub fn resolve_monitor_index_live(target: u32) -> Option<usize> {
    #[cfg(target_os = "macos")]
    {
        monitor_index_in(&live_active_display_ids(), target)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = target;
        None
    }
}

/// The current active display list in the exact order winit's
/// `available_monitors()` uses on macOS (see module docs).
#[cfg(target_os = "macos")]
#[must_use]
pub fn live_active_display_ids() -> Vec<u32> {
    core_graphics::display::CGDisplay::active_displays().unwrap_or_default()
}

/// The current active display list -- identity plus logical size -- in the
/// same order as [`live_active_display_ids`] (and therefore winit's
/// `available_monitors()`).
///
/// # SAFETY
///
/// No `unsafe` block is needed: `core_graphics::display::CGDisplay::bounds()`
/// is already the exact safe API `crate::display::metrics::LogicalRect::from_cg_bounds`
/// (via `crate::display::display_metrics`) uses to read a display's logical
/// rectangle, and `active_displays()` is the same already-safe call
/// [`live_active_display_ids`] uses.
#[cfg(target_os = "macos")]
#[must_use]
pub fn live_active_displays() -> Vec<ActiveDisplayInfo> {
    live_active_display_ids()
        .into_iter()
        .map(|display_id| {
            let bounds = core_graphics::display::CGDisplay::new(display_id).bounds();
            ActiveDisplayInfo {
                display_id,
                width_pts: bounds.size.width as f32,
                height_pts: bounds.size.height as f32,
            }
        })
        .collect()
}

#[cfg(not(target_os = "macos"))]
#[must_use]
pub fn live_active_displays() -> Vec<ActiveDisplayInfo> {
    Vec::new()
}

/// The root viewport's own literal, unchanging window title: set exactly
/// once at `NativeOptions`/`eframe::run_native` construction time in
/// `crate::ui::app::run_native_app` and never renamed at runtime. Defined
/// once here (rather than as a separate literal in both that construction
/// site and every place root's current display must be resolved) so
/// [`window_display_id`] can reliably resolve root's own window the exact
/// same way [`window_title_for`] lets it resolve any secondary window --
/// both paths share the identical `NSWindow -> NSScreen ->
/// CGDirectDisplayID` lookup, just keyed on a different constant title.
pub const ROOT_WINDOW_TITLE: &str = "Arcen Deck";

/// Builds the `egui::ViewportBuilder` that opens `assignment`'s window
/// directly into real native fullscreen on its target display.
///
/// # Errors
///
/// Returns [`MonitorResolutionError::DisplayNotActive`] when the target
/// `CGDirectDisplayID` cannot be resolved to a winit monitor index in
/// `active_displays` -- there is deliberately no monitor-less fullscreen
/// fallback (a window with no target monitor could land on the wrong
/// display, or on the primary display a second time, silently duplicating
/// content); the caller must treat this as a transactional-enter failure and
/// never open the window.
pub fn viewport_builder_for(
    assignment: &MonitorWindowAssignment,
    active_displays: &[ActiveDisplayInfo],
) -> Result<egui::ViewportBuilder, MonitorResolutionError> {
    let ids: Vec<u32> = active_displays
        .iter()
        .map(|display| display.display_id)
        .collect();
    let index = monitor_index_in(&ids, assignment.display_id).ok_or(
        MonitorResolutionError::DisplayNotActive(assignment.display_id),
    )?;
    Ok(egui::ViewportBuilder::default()
        .with_title(window_title_for(assignment.session_monitor_id))
        .with_decorations(false)
        .with_resizable(false)
        .with_fullscreen(true)
        .with_monitor(index))
}

/// Resolves the exact `CGDirectDisplayID` currently backing the on-screen
/// `NSWindow` whose title is exactly `window_title`.
///
/// Uses the same authoritative `NSScreen.deviceDescription()`'s
/// `NSScreenNumber` entry that `crate::display::mod::macos::safe_area_insets`
/// already reads in the opposite direction (there:
/// `CGDirectDisplayID -> NSScreen`; here: `NSWindow -> NSScreen ->
/// CGDirectDisplayID`), via `NSWindow.screen()`.
///
/// # SAFETY
///
/// No `unsafe` block is needed: every step -- `NSApplication.windows()`,
/// `NSWindow.title()`/`.screen()`, `NSScreen.deviceDescription()`,
/// `NSDictionary.objectForKey()`, and the `NSNumber` downcast/read -- goes
/// through `objc2_app_kit`/`objc2_foundation`'s own safe, already-validated
/// typed bindings (the same ones `crate::display::mod::macos` already uses
/// with no `unsafe` blocks). `MainThreadMarker::new()` proves the
/// main-thread precondition AppKit requires for all of these calls; this
/// returns `None` rather than ever calling them off the main thread.
///
/// Returns `None` -- fail closed, never a coordinate/size guess -- when: this
/// is not the main thread, no currently open window has this exact title
/// (matched via `NSString::isEqualToString`, an exact string identity check,
/// not a substring/heuristic match), that window currently reports no
/// screen (e.g. off-screen/minimized), or the screen's `deviceDescription`
/// dictionary is missing its `NSScreenNumber` entry or that entry is not an
/// `NSNumber`.
#[cfg(target_os = "macos")]
#[must_use]
pub fn window_display_id(window_title: &str) -> Option<u32> {
    use objc2_app_kit::NSApplication;
    use objc2_foundation::{MainThreadMarker, NSNumber, NSString};

    let mtm = MainThreadMarker::new()?;
    let app = NSApplication::sharedApplication(mtm);
    let screen_number_key = NSString::from_str("NSScreenNumber");
    let target_title = NSString::from_str(window_title);
    for window in app.windows() {
        if !window.title().isEqualToString(&target_title) {
            continue;
        }
        let screen = window.screen()?;
        let number = screen
            .deviceDescription()
            .objectForKey(&screen_number_key)?;
        let number = number.downcast::<NSNumber>().ok()?;
        return Some(number.as_u32());
    }
    None
}

/// Non-macOS stub: this module's native window plumbing only ever runs on
/// macOS (see [`crate::ui::multi_window_diagnostic`]'s `UnsupportedPlatform`
/// gate), so there is never a real `NSWindow` to resolve here.
#[cfg(not(target_os = "macos"))]
#[must_use]
pub fn window_display_id(_window_title: &str) -> Option<u32> {
    None
}

/// Resolves the `-[NSEvent windowNumber]`-comparable native window number of
/// the on-screen `NSWindow` whose title is exactly `window_title`.
///
/// This is the counterpart lookup [`crate::tablet`]'s AppKit-facing capture
/// needs: a native tablet/pen sample carries only the `NSWindow` it was
/// delivered against (`NativeTabletPoint::window_number` /
/// `NativeTabletProximity::window_number`, captured from the same
/// `-[NSEvent windowNumber]`), never which *viewport role* (root vs. a
/// specific secondary monitor) that window plays. Resolving each known
/// viewport's own current window number by title, the same way
/// [`window_display_id`] resolves each one's current display, lets a caller
/// match an incoming sample's `window_number` back to the exact viewport
/// that must normalize it -- root's own rect, one specific secondary's own
/// rect, or (no title matches) dropped as ambiguous rather than ever
/// defaulting to root.
///
/// # SAFETY
///
/// No `unsafe` block is needed: `NSApplication.windows()`,
/// `NSWindow.title()`, and `NSWindow.windowNumber()` all go through
/// `objc2_app_kit`'s own safe, already-validated typed bindings, exactly
/// like [`window_display_id`]'s identical window-title matching loop.
///
/// Returns `None` -- fail closed -- when this is not the main thread or no
/// currently open window has this exact title (matched via
/// `NSString::isEqualToString`, an exact string identity check).
#[cfg(target_os = "macos")]
#[must_use]
pub fn window_number_for_title(window_title: &str) -> Option<isize> {
    use objc2_app_kit::NSApplication;
    use objc2_foundation::{MainThreadMarker, NSString};

    let mtm = MainThreadMarker::new()?;
    let app = NSApplication::sharedApplication(mtm);
    let target_title = NSString::from_str(window_title);
    for window in app.windows() {
        if !window.title().isEqualToString(&target_title) {
            continue;
        }
        return Some(window.windowNumber());
    }
    None
}

/// Non-macOS stub: see [`window_display_id`]'s identical non-macOS stub --
/// this module's native window plumbing only ever runs on macOS.
#[cfg(not(target_os = "macos"))]
#[must_use]
pub fn window_number_for_title(_window_title: &str) -> Option<isize> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sid(value: u16) -> SessionMonitorId {
        SessionMonitorId::new(value).expect("test session monitor id must be nonzero")
    }

    fn active_display(display_id: u32) -> ActiveDisplayInfo {
        ActiveDisplayInfo {
            display_id,
            width_pts: 1920.0,
            height_pts: 1080.0,
        }
    }

    #[test]
    fn viewport_builder_targets_the_resolved_monitor_index() {
        let assignment = MonitorWindowAssignment {
            session_monitor_id: sid(2),
            window: viewport_id_for(sid(2)),
            display_id: 300,
        };
        let active = [
            active_display(500),
            active_display(100),
            active_display(300),
        ];
        let builder = viewport_builder_for(&assignment, &active).expect("display resolves");
        assert_eq!(builder.monitor, Some(2));
        assert_eq!(builder.fullscreen, Some(true));
        assert_eq!(builder.decorations, Some(false));
        assert_eq!(
            builder.title.as_deref(),
            Some(window_title_for(sid(2)).as_str())
        );
    }

    #[test]
    fn viewport_builder_for_fails_when_the_display_cannot_be_resolved() {
        // Finding 1: no monitor-less fullscreen fallback -- an unresolved
        // target display must be a typed failure, not a window with no
        // target monitor.
        let assignment = MonitorWindowAssignment {
            session_monitor_id: sid(2),
            window: viewport_id_for(sid(2)),
            display_id: 999,
        };
        let active = [
            active_display(500),
            active_display(100),
            active_display(300),
        ];
        let error = viewport_builder_for(&assignment, &active).unwrap_err();
        assert_eq!(error, MonitorResolutionError::DisplayNotActive(999));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn window_display_id_fails_closed_for_a_title_with_no_open_window() {
        // Exercises the real safe-AppKit path end-to-end (or its
        // main-thread guard, if the test harness does not run this test on
        // the main thread) without needing any real window to already be
        // open: there is no window anywhere in this process with this
        // title, so this must return `None` -- never panic, and never
        // invent an id.
        assert_eq!(
            window_display_id("arcen-deck-test-window-title-that-is-never-created"),
            None
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn window_display_id_is_always_none_off_macos() {
        assert_eq!(window_display_id("anything"), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn window_number_for_title_fails_closed_for_a_title_with_no_open_window() {
        // Same fail-closed contract as `window_display_id`, proven the same
        // way: no window anywhere in this process ever has this title.
        assert_eq!(
            window_number_for_title("arcen-deck-test-window-title-that-is-never-created"),
            None
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn window_number_for_title_is_always_none_off_macos() {
        assert_eq!(window_number_for_title("anything"), None);
    }
}
