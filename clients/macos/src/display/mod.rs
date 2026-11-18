// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Client display enumeration.
//!
//! The host adapts to the client: on accepted auth it reads the client's real
//! display layout (from the `AuthResponse`) and builds a matching X session so
//! it streams at the client's effective desktop resolution — a 2K client gets
//! 2K, while a Retina client gets its logical framebuffer rather than wasting
//! bandwidth on backing pixels. This module produces that layout as protocol
//! [`ClientMonitor`]s.
//!
//! macOS uses CoreGraphics directly (no event loop, no main-thread requirement
//! for the `CGDisplay*` calls). Every consumer -- this roster, the pinned
//! fullscreen stream size and the multi-monitor-v1 requested topology -- reads
//! one validated [`DisplayMetrics`] value per display, so a display's
//! advertised scale, logical arrangement and stream extent can never be
//! derived twice and disagree. On other platforms we return an empty layout
//! and the host falls back to its configured default size.
//!
//! All arrangement coordinates here are the pinned CoreGraphics top-left,
//! y-down point space; see [`metrics`] for the convention and the single
//! AppKit conversion.

use crate::protocol::messages::ClientMonitor;

pub use arcen_deck_core::display::metrics;
pub mod topology;

pub use arcen_deck_core::display::{edid_feasible, presentation_extent_with_edid_fallback};
pub use metrics::{
    BackingScale, DisplayMetrics, DisplayMetricsError, LogicalRect, SafeAreaInsets,
    ScreenSpaceFlip, StreamExtent,
};

/// Enumerate the client's active physical displays, primary first.
///
/// Returns an empty vec when enumeration is unsupported or fails; callers must
/// treat that as "let the host choose" rather than an error.
pub fn enumerate() -> Vec<ClientMonitor> {
    #[cfg(target_os = "macos")]
    {
        macos::enumerate()
    }
    #[cfg(not(target_os = "macos"))]
    {
        Vec::new()
    }
}

/// The display (`ClientMonitor::id`) the session window is on: the display
/// that shows a single-stream session. `None` off the main thread, before
/// the window exists, and on other platforms.
pub fn session_window_display_id() -> Option<u32> {
    #[cfg(target_os = "macos")]
    {
        macos::session_window_display_id()
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

/// The refresh a display shows frames at, as the Deck reports it: the
/// larger of the current mode's rate (`CGDisplayModeGetRefreshRate`, 0 for
/// built-in panels) and `NSScreen.maximumFramesPerSecond` (a `ProMotion`
/// panel's maximum), else 60 Hz when neither is known.
#[must_use]
pub fn reported_refresh_hz(mode_refresh_hz: f64, max_frames_per_second: Option<u32>) -> u32 {
    let mode = if mode_refresh_hz.is_finite() && mode_refresh_hz > 0.0 {
        // A finite positive rate in hertz.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let rounded = mode_refresh_hz.round().min(f64::from(u32::MAX)) as u32;
        rounded
    } else {
        0
    };
    match mode.max(max_frames_per_second.unwrap_or(0)) {
        0 => 60,
        refresh => refresh,
    }
}

/// The gamut and HDR headroom the OS states for `display_id`. `None` off
/// the main thread and on platforms that do not report them.
pub fn display_color(display_id: u32) -> Option<arcen_protocol::messages::DisplayColorMsg> {
    #[cfg(target_os = "macos")]
    {
        macos::display_color(display_id)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = display_id;
        None
    }
}

/// Whether any of this client's displays can show HDR. HDR streaming is
/// offered only when one can.
pub fn any_display_is_hdr() -> bool {
    let colors: Vec<_> = enumerate()
        .iter()
        .filter_map(|monitor| monitor.color)
        .collect();
    arcen_media::display_color::any_display_is_hdr(&colors)
}

/// The top of a display a standard fullscreen window does not get, in
/// points.
///
/// On a notched panel AppKit reports the notch as `safeAreaInsets.top`, but
/// a fullscreen window starts below the menu bar, which is a point taller.
/// Measured on a 14-inch MacBook Pro (1800x1169 points) with a small AppKit
/// probe reading these properties, windowed, fullscreen and with the menu bar set to
/// hide automatically: safe-area top 38, the notch strips
/// (`auxiliaryTopLeftArea`/`RightArea`) 38, the main menu's `menuBarHeight`
/// 39 in every state, `visibleFrame`'s top gap 39 or 38 depending on whether
/// the menu bar shows, and the fullscreen window always 1800x1130 below a
/// 39-point gap. Sizing a Retina stream from the notch asked for 3600x2262
/// pixels on that 3600x2260 viewport, which the Deck shrank by 0.1%.
///
/// So the inset is the fullscreen window's own top gap when one is on the
/// display — the truth — and otherwise the larger of the notch and the menu
/// bar's height, which does not follow the menu bar's visibility. Nothing
/// that changes when the menu bar hides is an input. It is rounded up so the
/// stream is never taller than the viewport. A display without a notch keeps
/// no inset: its menu bar hides in fullscreen.
#[must_use]
pub(crate) fn fullscreen_top_inset(
    safe_top: f64,
    menu_bar_height: f64,
    fullscreen_window_top_gap: Option<f64>,
) -> u32 {
    if safe_top.is_nan() || safe_top <= 0.0 {
        return 0;
    }
    let finite = |value: f64| if value.is_finite() { value } else { 0.0 };
    let inset = match fullscreen_window_top_gap.map(finite) {
        Some(gap) if gap > 0.0 => safe_top.max(gap),
        _ => safe_top.max(finite(menu_bar_height)),
    };
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    {
        inset.ceil().min(f64::from(u32::MAX)) as u32
    }
}

/// The macOS fullscreen safe-area insets for `display_id`, in points.
///
/// Zero on failure, off the main thread, and on any display without a notch,
/// which reproduces the pre-existing behaviour exactly.
pub fn safe_area_insets(display_id: u32) -> SafeAreaInsets {
    #[cfg(target_os = "macos")]
    {
        macos::safe_area_insets(display_id)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = display_id;
        SafeAreaInsets::default()
    }
}

/// The single validated metrics reading for `display_id`: its CG top-left
/// logical arrangement rectangle, backing pixels, exact backing scale,
/// rotation, safe-area insets and every stream extent derived from them.
///
/// Returns `None` when the display's own facts cannot be read at all (and
/// always on non-macOS), which callers must treat exactly as they treat an
/// empty [`enumerate`]: let the host choose.
pub fn display_metrics(display_id: u32) -> Option<DisplayMetrics> {
    #[cfg(target_os = "macos")]
    {
        macos::display_metrics(display_id)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = display_id;
        None
    }
}

/// Auto-hide or restore the menu bar and Dock.
///
/// macOS has no public API to make a *standard* fullscreen window ignore the
/// notch: AppKit lays those out inside the safe area. Covering the whole panel
/// needs custom window management — a borderless window at the screen frame
/// with the menu bar and Dock auto-hidden — which is what this enables. They
/// auto-hide rather than disappear, so moving the pointer to the top edge still
/// drops the menu bar down.
///
/// Returns whether the request was applied.
pub fn set_menu_bar_hidden(hidden: bool) -> bool {
    #[cfg(target_os = "macos")]
    {
        macos::set_menu_bar_hidden(hidden)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = hidden;
        false
    }
}

/// Place the app's window over the entire main screen, notch strip included.
///
/// Done through AppKit rather than a viewport command because winit converts
/// window positions through the screen's *visible* frame, which lands the
/// window one menu-bar height too low on a notched panel.
///
/// Returns whether the window now covers the screen exactly.
pub fn cover_main_screen() -> bool {
    #[cfg(target_os = "macos")]
    {
        macos::cover_main_screen()
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

pub use arcen_deck_core::connect_plan::{pinned_refresh_hz, PINNED_MAX_REFRESH_HZ};

/// The stream size a pinned session should request for `display_id`: the size
/// the Deck can actually present once it enters fullscreen.
///
/// Pinning to the display's full logical size is wrong on a notched Mac. The
/// system hands a fullscreen window only the safe area, so a stream sized to
/// the whole panel is aspect-fit into a shorter viewport — it is downscaled and
/// pillarboxed. Subtracting the insets makes the stream exactly fill the
/// viewport at 1:1, which is the product promise.
///
/// `use_notch_area` selects the Settings → Displays behaviour: when set, the
/// caller intends to cover the notch region, so no inset is removed.
///
/// `hidpi` requests the backing-pixel size instead of the point size, so a
/// Retina panel gets true physical 1:1 rather than an integer upscale. The
/// conversion uses the display's own exact backing scale
/// ([`DisplayMetrics::presentation_stream_extent`]), so a non-integer scaled
/// Retina mode streams the pixels it advertises.
pub fn presentation_size_for_display(
    display_id: u32,
    use_notch_area: bool,
    hidpi: bool,
) -> Option<[u32; 2]> {
    Some(
        display_metrics(display_id)?
            .presentation_stream_extent(use_notch_area, hidpi)
            .as_array(),
    )
}

/// The stream size a session should actually request for `display_id`,
/// downgrading away from a HiDPI (backing-pixel) request when the host could
/// not express it as an EDID 1.4 detailed timing at `refresh_hz`.
///
/// This is the single, shared encoder/EDID/refresh/HiDPI fallback policy:
/// every caller that decides a per-display stream size for a `ClientHello` or
/// a multi-monitor-v1 sidecar topology entry must go through this function
/// (never `presentation_size_for_display` directly, except from here) so a
/// primary-only pinned session and a multi-monitor topology entry for the
/// exact same physical display always agree on the same fallback decision.
///
/// `refresh_hz` must already be the *pinned* refresh (see
/// [`pinned_refresh_hz`]) the caller intends to advertise for this display,
/// so the EDID feasibility check and the returned stream size always describe
/// what will actually be requested — callers must not check feasibility
/// against one refresh and then advertise a different one.
///
/// Returns `None` only when the display's own facts (logical size) could not
/// be read at all, exactly like [`presentation_size_for_display`].
pub fn presentation_size_for_display_with_edid_fallback(
    display_id: u32,
    refresh_hz: u32,
    use_notch_area: bool,
    hidpi: bool,
) -> Option<[u32; 2]> {
    let metrics = display_metrics(display_id)?;
    Some(
        presentation_extent_with_edid_fallback(&metrics, refresh_hz, use_notch_area, hidpi)
            .as_array(),
    )
}

/// One display's full logged stream mapping, compared as a whole to decide
/// whether `macOS display mapped to stream resolution` has anything new to
/// say (see [`display_mapping_changed`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DisplayMappingSnapshot {
    primary: bool,
    backing: (u32, u32),
    logical: (u32, u32),
    stream: (u32, u32),
    insets: SafeAreaInsets,
    /// `scale` rounded to milli-units so the snapshot stays exactly
    /// comparable; a float would make equality dependent on bit patterns.
    scale_millis: i64,
    origin: (i64, i64),
    /// Clockwise desktop rotation in degrees.
    rotation_degrees: u32,
    /// Whether AppKit's own (bottom-left) frame for this display, converted
    /// once through [`ScreenSpaceFlip`], disagreed with the CG arrangement.
    /// Never `true` off the main thread, where AppKit cannot be read at all,
    /// so the record does not flap between threads.
    appkit_arrangement_mismatch: bool,
}

/// Whether `display_id`'s stream mapping differs from the last one logged
/// for it, remembering `snapshot` as the new baseline either way.
///
/// Display enumeration runs on the client's frame loop, so logging the
/// mapping unconditionally is level-triggered: one live nine-minute
/// two-display session emitted 46,316 of those records. The mapping itself
/// only changes on a real display, mode, scale, notch, or layout change, so
/// reporting the *edges* keeps every genuinely new fact while bounding the
/// volume to a handful per session. Unknown displays always report `true`,
/// so a display's very first mapping is always logged.
fn display_mapping_changed(display_id: u32, snapshot: &DisplayMappingSnapshot) -> bool {
    use std::collections::BTreeMap;
    use std::sync::{Mutex, OnceLock};

    static LAST_LOGGED: OnceLock<Mutex<BTreeMap<u32, DisplayMappingSnapshot>>> = OnceLock::new();
    let mut last = match LAST_LOGGED.get_or_init(Mutex::default).lock() {
        Ok(last) => last,
        // A poisoned cache must never silence a diagnostic: fall back to
        // logging this record rather than dropping it.
        Err(_) => return true,
    };
    last.insert(display_id, *snapshot) != Some(*snapshot)
}

#[cfg(target_os = "macos")]
mod macos {
    use super::metrics::{
        backing_for_arrangement, rotation_from_degrees, DisplayMetrics, LogicalRect,
        ScreenSpaceFlip,
    };
    use super::SafeAreaInsets;
    use crate::protocol::messages::ClientMonitor;
    use core_graphics::display::CGDisplay;
    use objc2_app_kit::{
        NSApplication, NSApplicationPresentationOptions, NSScreen, NSWindowStyleMask,
    };
    use objc2_foundation::{MainThreadMarker, NSNumber, NSString};

    /// One display's live reading: its validated metrics plus, when AppKit
    /// could be consulted at all, the same display's arrangement rectangle as
    /// AppKit reports it — converted from AppKit's bottom-left/y-up space into
    /// the pinned CG top-left/y-down space exactly once, in
    /// [`appkit_screen_facts`].
    pub struct DisplayReading {
        pub metrics: DisplayMetrics,
        pub appkit_arrangement: Option<LogicalRect>,
    }

    impl DisplayReading {
        /// Whether AppKit's own frame for this display disagrees with the CG
        /// arrangement by more than a rounding step. Always `false` when
        /// AppKit could not be read (off the main thread), so an unreadable
        /// check never masquerades as a mismatch.
        pub fn appkit_arrangement_mismatch(&self) -> bool {
            self.appkit_arrangement
                .is_some_and(|appkit| appkit != self.metrics.arrangement())
        }
    }

    /// The display's logical arrangement rectangle.
    ///
    /// `CGDisplay::bounds` is already the pinned CG top-left, y-down point
    /// space the Mac arranges displays in, with the primary display's origin
    /// at (0, 0) and displays left of or above it at negative coordinates —
    /// exactly the space the wire's logical arrangement is defined in, so no
    /// conversion is applied here. A display that reports no usable bounds
    /// falls back to its raw pixel size at the origin rather than vanishing.
    fn arrangement_rect(display: &CGDisplay) -> Option<LogicalRect> {
        let bounds = display.bounds();
        if let Ok(rect) = LogicalRect::from_cg_bounds(
            bounds.origin.x,
            bounds.origin.y,
            bounds.size.width,
            bounds.size.height,
        ) {
            return Some(rect);
        }
        let (width, height) = mode_point_size(display)?;
        LogicalRect::new(0, 0, width, height).ok()
    }

    /// The display's logical (point) mode size, falling back to its raw
    /// framebuffer size when no mode can be read.
    fn mode_point_size(display: &CGDisplay) -> Option<(u32, u32)> {
        match display.display_mode() {
            Some(mode) => {
                let width = mode.width() as u32;
                let height = mode.height() as u32;
                (width > 0 && height > 0).then_some((width, height))
            }
            None => {
                let width = display.pixels_wide() as u32;
                let height = display.pixels_high() as u32;
                (width > 0 && height > 0).then_some((width, height))
            }
        }
    }

    /// The backing pixels covering `arrangement`, in the arrangement's own
    /// orientation. A rotated display's mode still describes the panel's
    /// landscape framebuffer, so [`backing_for_arrangement`] transposes it.
    fn backing_pixels(display: &CGDisplay, arrangement: LogicalRect) -> (u32, u32) {
        let Some(mode) = display.display_mode() else {
            return arrangement.size();
        };
        backing_for_arrangement(
            arrangement,
            (mode.width() as u32, mode.height() as u32),
            (mode.pixel_width() as u32, mode.pixel_height() as u32),
        )
    }

    /// The full live reading for `id`: the single place a display's geometry
    /// is read, validated and turned into [`DisplayMetrics`].
    pub fn read_display(id: u32) -> Option<DisplayReading> {
        let display = CGDisplay::new(id);
        let arrangement = arrangement_rect(&display)?;
        let (backing_width, backing_height) = backing_pixels(&display, arrangement);
        let rotation = rotation_from_degrees(display.rotation());
        let appkit = appkit_screen_facts(id);
        let insets = appkit
            .as_ref()
            .map(|facts| facts.insets)
            .unwrap_or_default();
        let metrics = validated_metrics_or_unscaled(
            id,
            arrangement,
            (backing_width, backing_height),
            rotation,
            insets,
        )?;
        Some(DisplayReading {
            metrics,
            appkit_arrangement: appkit.and_then(|facts| facts.arrangement),
        })
    }

    /// The display's validated metrics, or an honest 1:1 reading of the
    /// rectangle macOS actually laid out when the mode's pixels cannot be
    /// reconciled with it.
    ///
    /// A mode that disagrees with the display's own bounds must never
    /// silently advertise a scale nothing else agrees with, and it must never
    /// drop the display either: a wrong scale would desynchronise the
    /// advertised stream size from the advertised scale, which is exactly
    /// what one shared metrics reading exists to prevent.
    ///
    /// Pure apart from the diagnostic, so every fallback branch is directly
    /// testable without real hardware.
    fn validated_metrics_or_unscaled(
        id: u32,
        arrangement: LogicalRect,
        backing: (u32, u32),
        rotation: arcen_media::Rotation,
        insets: SafeAreaInsets,
    ) -> Option<DisplayMetrics> {
        let (backing_width, backing_height) = backing;
        match DisplayMetrics::new(
            id,
            arrangement,
            backing_width,
            backing_height,
            rotation,
            insets,
        ) {
            Ok(metrics) => Some(metrics),
            Err(error) => {
                tracing::warn!(
                    target: crate::logging::target::UI,
                    display_id = id,
                    backing = %format!("{backing_width}x{backing_height}"),
                    logical = %format!("{}x{}", arrangement.width(), arrangement.height()),
                    %error,
                    "display metrics failed validation; reporting the display at 1x",
                );
                DisplayMetrics::unscaled(id, arrangement, rotation, insets).ok()
            }
        }
    }

    /// The validated metrics for `id`, for every caller that does not need the
    /// AppKit cross-check.
    pub fn display_metrics(id: u32) -> Option<DisplayMetrics> {
        read_display(id).map(|reading| reading.metrics)
    }

    /// Everything one `NSScreen` pass can tell us about a display.
    #[derive(Debug, Clone)]
    struct AppKitScreenFacts {
        insets: SafeAreaInsets,
        /// Localized display name, as AppKit exposes it for the screen.
        name: Option<String>,
        /// Gamut and HDR headroom, as AppKit states them. `None` when no
        /// `NSScreen` backs the display.
        color: Option<arcen_protocol::messages::DisplayColorMsg>,
        /// The screen's frame in the pinned CG top-left/y-down space. `None`
        /// when the primary screen's height could not be read, so the flip
        /// would have been guesswork.
        arrangement: Option<LogicalRect>,
        /// `NSScreen.maximumFramesPerSecond`: the most frames a second the
        /// screen shows (a `ProMotion` panel's maximum). `None` when unknown.
        max_frames_per_second: Option<u32>,
    }

    /// Read `NSScreen.safeAreaInsets` and `NSScreen.frame` for the screen
    /// backing `id`, in one pass.
    ///
    /// This is the *only* place AppKit's bottom-left, y-up screen space enters
    /// the client: the frame is converted through [`ScreenSpaceFlip`] here and
    /// never again downstream. `NSScreen` is main-thread-only, so this returns
    /// `None` when called from anywhere else rather than risking a threading
    /// violation.
    fn appkit_screen_facts(id: u32) -> Option<AppKitScreenFacts> {
        let mtm = MainThreadMarker::new()?;
        let screens = NSScreen::screens(mtm);
        // AppKit measures every frame from the bottom-left of `screens[0]`,
        // the screen owning the menu bar.
        let flip = screens
            .iter()
            .next()
            .and_then(|primary| ScreenSpaceFlip::new(primary.frame().size.height));
        let key = NSString::from_str("NSScreenNumber");
        for screen in screens {
            let Some(number) = screen.deviceDescription().objectForKey(&key) else {
                continue;
            };
            let Ok(number) = number.downcast::<NSNumber>() else {
                continue;
            };
            if number.as_u32() != id {
                continue;
            }
            // Points, matching NSScreen.frame. Round up so a fractional inset
            // can never leave the stream taller than the viewport.
            let insets = screen.safeAreaInsets();
            let frame = screen.frame();
            let app = NSApplication::sharedApplication(mtm);
            let menu_bar_height = app.mainMenu().map_or(0.0, |menu| menu.menuBarHeight());
            // A standard fullscreen window of this app on this display shows
            // exactly what macOS gives fullscreen; the borderless notch-area
            // window is not one (it lacks the full-screen style).
            let fullscreen_window_top_gap = app.windows().iter().find_map(|window| {
                if !window.styleMask().contains(NSWindowStyleMask::FullScreen) {
                    return None;
                }
                let on_this_screen = window
                    .screen()
                    .and_then(|on| on.deviceDescription().objectForKey(&key))
                    .and_then(|number| number.downcast::<NSNumber>().ok())
                    .is_some_and(|number| number.as_u32() == id);
                on_this_screen.then(|| {
                    let content = window.contentRectForFrameRect(window.frame());
                    (frame.origin.y + frame.size.height) - (content.origin.y + content.size.height)
                })
            });
            let top =
                super::fullscreen_top_inset(insets.top, menu_bar_height, fullscreen_window_top_gap);
            let localized_name = screen.localizedName().to_string();
            let color = arcen_protocol::messages::DisplayColorMsg {
                gamut: if screen.canRepresentDisplayGamut(objc2_app_kit::NSDisplayGamut::P3) {
                    arcen_protocol::messages::DisplayGamutMsg::DisplayP3
                } else {
                    arcen_protocol::messages::DisplayGamutMsg::Srgb
                },
                // The potential, not the current, headroom: what the panel
                // can reach, independent of where its brightness is set now.
                hdr_headroom: {
                    // SAFETY: a documented `CGFloat` property of a live
                    // `NSScreen`, read on the main thread.
                    let headroom: f64 = unsafe {
                        objc2::msg_send![
                            &*screen,
                            maximumPotentialExtendedDynamicRangeColorComponentValue
                        ]
                    };
                    headroom as f32
                },
                ..Default::default()
            };
            let max_frames_per_second = u32::try_from(screen.maximumFramesPerSecond())
                .ok()
                .filter(|rate| *rate > 0);
            return Some(AppKitScreenFacts {
                max_frames_per_second,
                name: (!localized_name.trim().is_empty()).then_some(localized_name),
                color: Some(color),
                insets: SafeAreaInsets {
                    top,
                    bottom: insets.bottom.max(0.0).ceil() as u32,
                    left: insets.left.max(0.0).ceil() as u32,
                    right: insets.right.max(0.0).ceil() as u32,
                },
                arrangement: flip.and_then(|flip| {
                    flip.cg_rect_from_appkit_frame(
                        frame.origin.x,
                        frame.origin.y,
                        frame.size.width,
                        frame.size.height,
                    )
                    .ok()
                }),
            });
        }
        Some(AppKitScreenFacts {
            insets: SafeAreaInsets::ZERO,
            name: None,
            color: None,
            arrangement: None,
            max_frames_per_second: None,
        })
    }

    fn appkit_display_name(id: u32) -> Option<String> {
        appkit_screen_facts(id).and_then(|facts| facts.name)
    }

    fn display_name(id: u32, display: &CGDisplay) -> String {
        appkit_display_name(id).unwrap_or_else(|| {
            if display.is_builtin() {
                "Built-in Display".to_string()
            } else {
                format!("Display {id}")
            }
        })
    }

    /// Auto-hide or restore the menu bar and Dock via `NSApplication`.
    ///
    /// Auto-hide rather than hide: the window still owns the whole screen, but
    /// pushing the pointer to the top edge drops the menu bar down over the
    /// session the way it does in a normal fullscreen app. Fully hiding it
    /// would strand the user with no way to reach the menu. AppKit requires the
    /// Dock to follow the menu bar, so both move together.
    pub fn set_menu_bar_hidden(hidden: bool) -> bool {
        let Some(mtm) = MainThreadMarker::new() else {
            return false;
        };
        let app = NSApplication::sharedApplication(mtm);
        let options = if hidden {
            NSApplicationPresentationOptions::AutoHideMenuBar
                | NSApplicationPresentationOptions::AutoHideDock
        } else {
            NSApplicationPresentationOptions::Default
        };
        app.setPresentationOptions(options);
        true
    }

    /// Place the key window over the whole main screen, notch strip included.
    pub fn cover_main_screen() -> bool {
        let Some(mtm) = MainThreadMarker::new() else {
            tracing::warn!(target: "arcen::display", "notch fullscreen: not on the main thread");
            return false;
        };
        let app = NSApplication::sharedApplication(mtm);
        let Some(window) = app
            .mainWindow()
            .or_else(|| app.keyWindow())
            .or_else(|| app.windows().iter().next())
        else {
            tracing::warn!(target: "arcen::display", "notch fullscreen: no app window yet");
            return false;
        };
        let Some(screen) = window.screen().or_else(|| NSScreen::mainScreen(mtm)) else {
            tracing::warn!(target: "arcen::display", "notch fullscreen: no screen for the window");
            return false;
        };
        let frame = screen.frame();
        window.setFrame_display(frame, true);
        let applied = window.frame();
        let matches = (applied.origin.x - frame.origin.x).abs() < 0.5
            && (applied.origin.y - frame.origin.y).abs() < 0.5
            && (applied.size.width - frame.size.width).abs() < 0.5
            && (applied.size.height - frame.size.height).abs() < 0.5;
        tracing::info!(
            target: "arcen::display",
            requested = %format!("{}x{} at ({},{})", frame.size.width, frame.size.height, frame.origin.x, frame.origin.y),
            applied = %format!("{}x{} at ({},{})", applied.size.width, applied.size.height, applied.origin.x, applied.origin.y),
            matches,
            "covering the main screen for notch fullscreen",
        );
        matches
    }

    /// Read `NSScreen.safeAreaInsets` for the screen backing `id`.
    ///
    /// `NSScreen` is main-thread-only, so this returns zero insets when called
    /// from anywhere else rather than risking a threading violation. Zero is
    /// also the correct answer for every display without a notch.
    pub fn safe_area_insets(id: u32) -> SafeAreaInsets {
        appkit_screen_facts(id)
            .map(|facts| facts.insets)
            .unwrap_or_default()
    }

    /// The display the session window (the app's main, key or first
    /// window) is on. `None` off the main thread or without a window.
    pub fn session_window_display_id() -> Option<u32> {
        let mtm = MainThreadMarker::new()?;
        let app = NSApplication::sharedApplication(mtm);
        let window = app
            .mainWindow()
            .or_else(|| app.keyWindow())
            .or_else(|| app.windows().iter().next())?;
        let number = window
            .screen()?
            .deviceDescription()
            .objectForKey(&NSString::from_str("NSScreenNumber"))?;
        number
            .downcast::<NSNumber>()
            .ok()
            .map(|number| number.as_u32())
    }

    /// The gamut and HDR headroom AppKit states for the screen backing `id`.
    /// `None` off the main thread, or when no `NSScreen` backs the display.
    pub fn display_color(id: u32) -> Option<arcen_protocol::messages::DisplayColorMsg> {
        appkit_screen_facts(id).and_then(|facts| facts.color)
    }

    pub fn enumerate() -> Vec<ClientMonitor> {
        let ids = match CGDisplay::active_displays() {
            Ok(ids) => ids,
            Err(err) => {
                tracing::warn!(
                    target: crate::logging::target::UI,
                    error = err,
                    "CGGetActiveDisplayList failed; reporting no displays",
                );
                return Vec::new();
            }
        };

        let mut monitors: Vec<ClientMonitor> = ids.into_iter().filter_map(build_monitor).collect();

        // Primary first, then left-to-right by x. The host expects the primary
        // at (0, 0); macOS already guarantees the main display's bounds origin
        // is (0, 0), so we only need to order the list.
        monitors.sort_by(|a, b| {
            b.is_primary
                .cmp(&a.is_primary)
                .then(a.x.cmp(&b.x))
                .then(a.y.cmp(&b.y))
        });
        monitors
    }

    /// One display's legacy roster entry, built from the same single
    /// [`DisplayMetrics`] reading every other consumer uses.
    ///
    /// `None` when the display's own geometry cannot be read at all: an
    /// unreadable display must not reach a host as a degenerate 4x2 monitor.
    fn build_monitor(id: u32) -> Option<ClientMonitor> {
        let display = CGDisplay::new(id);
        let reading = read_display(id)?;
        let metrics = reading.metrics;
        let arrangement = metrics.arrangement();
        let (backing_width, backing_height) = metrics.backing_size();

        let refresh = display
            .display_mode()
            .map_or(0.0, |mode| mode.refresh_rate());
        // Built-in Apple panels report 0.0 from CGDisplayModeGetRefreshRate;
        // AppKit states a ProMotion panel's maximum (120 Hz). Hosts follow
        // this refresh; the virtual display mode a session asks for stays
        // pinned (`arcen_deck_core::connect_plan::pinned_refresh_hz`).
        let refresh_hz = super::reported_refresh_hz(
            refresh,
            appkit_screen_facts(id).and_then(|facts| facts.max_frames_per_second),
        );
        let name = display_name(id, &display);

        // Physical size in millimetres (CGDisplayScreenSize) → the host derives
        // the correct DPI for the synthesized EDID. Some virtual/unknown
        // displays report 0×0; leave it 0.0 so the host falls back to `scale`.
        let phys = display.screen_size();
        // Real display identity — the host may fold these into the synthesized
        // EDID's manufacturer/product fields for external monitors.
        let vendor = display.vendor_number();
        let model = display.model_number();
        let serial = display.serial_number();
        let stream = metrics.native_stream_extent();
        let is_primary = display.is_main();
        let insets = metrics.safe_area_insets();
        let (presentable_width, presentable_height) = metrics.presentable_size(false);
        let presentable = format!("{presentable_width}x{presentable_height}");
        // This runs on the client's frame loop, so an unconditional record
        // here is level-triggered: one live a Windows Pier session logged this
        // 46,316 times in nine minutes, burning bounded log I/O and burying
        // the records that actually matter. The mapping only ever *changes*
        // on a real display/mode/layout change, so log the edges instead --
        // same diagnostic value, bounded volume.
        let appkit_arrangement_mismatch = reading.appkit_arrangement_mismatch();
        let rotation_degrees = rotation_degrees(metrics.rotation());
        let color = display_color(id);
        if super::display_mapping_changed(
            id,
            &super::DisplayMappingSnapshot {
                primary: is_primary,
                backing: (backing_width, backing_height),
                logical: arrangement.size(),
                stream: (stream.width(), stream.height()),
                insets,
                scale_millis: metrics.scale().millis(),
                origin: (i64::from(arrangement.x()), i64::from(arrangement.y())),
                rotation_degrees,
                appkit_arrangement_mismatch,
            },
        ) {
            tracing::info!(
                target: crate::logging::target::UI,
                display_id = id,
                primary = is_primary,
                backing = %format!("{backing_width}x{backing_height}"),
                logical = %format!("{}x{}", arrangement.width(), arrangement.height()),
                stream = %format!("{}x{}", stream.width(), stream.height()),
                // The fullscreen presentation area. When this differs from `stream`
                // the panel has a notch: pinning to `stream` would be aspect-fit
                // into a shorter viewport and lose 1:1.
                presentable = %presentable,
                safe_area_top = insets.top,
                safe_area_bottom = insets.bottom,
                safe_area_left = insets.left,
                safe_area_right = insets.right,
                scale = metrics.scale().get(),
                rotation_degrees,
                // CG top-left, y-down arrangement origin -- the exact space
                // the wire's logical arrangement is defined in.
                x = arrangement.x(),
                y = arrangement.y(),
                // AppKit's own frame for this display, converted once into the
                // same space. `true` means the two coordinate spaces disagree,
                // which is always a bug worth seeing.
                appkit_arrangement_mismatch,
                "macOS display mapped to stream resolution"
            );
            // Its own record: the mapping above is at the per-record field cap.
            tracing::info!(
                target: crate::logging::target::UI,
                display_id = id,
                gamut = ?color.map(|color| color.gamut),
                hdr_headroom = ?color.map(|color| color.hdr_headroom),
                hdr = color.is_some_and(|color| {
                    arcen_media::display_color::DisplayColor::from_msg(&color).is_hdr()
                }),
                "macOS display colour"
            );
        }

        Some(ClientMonitor {
            id,
            x: arrangement.x(),
            y: arrangement.y(),
            width_px: stream.width(),
            height_px: stream.height(),
            scale: metrics.scale().get(),
            refresh_hz,
            is_primary,
            name,
            width_mm: phys.width as f32,
            height_mm: phys.height as f32,
            vendor,
            model,
            serial,
            // Apple Silicon exposes no raw EDID; host synthesizes from the
            // attributes above. Reserved for platforms that provide a blob.
            edid: String::new(),
            color,
            rotation_degrees: arcen_deck_core::connect_plan::rotation_degrees(metrics.rotation()),
        })
    }

    /// The logged clockwise rotation of a display, in degrees.
    fn rotation_degrees(rotation: arcen_media::Rotation) -> u32 {
        u32::from(arcen_deck_core::connect_plan::rotation_degrees(rotation))
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use arcen_media::Rotation;

        fn retina_arrangement() -> LogicalRect {
            LogicalRect::new(0, 0, 1800, 1169).expect("valid arrangement")
        }

        /// The AppKit cross-check is a *comparison of the same space*: the
        /// `NSScreen.frame` is flipped into CG top-left/y-down exactly once,
        /// in `appkit_screen_facts`, so a healthy display reports no
        /// mismatch and an unflipped (raw AppKit y-up) frame does.
        #[test]
        fn appkit_cross_check_compares_both_spaces_after_exactly_one_flip() {
            let flip = ScreenSpaceFlip::new(1169.0).expect("a real primary screen height");
            // A 1080p display stacked above the 1169 pt primary: AppKit
            // bottom-left y = +1169, CG top-left y = -1080.
            let arrangement = flip
                .cg_rect_from_appkit_frame(0.0, 1169.0, 1920.0, 1080.0)
                .expect("valid frame");
            assert_eq!((arrangement.x(), arrangement.y()), (0, -1080));

            let metrics =
                DisplayMetrics::unscaled(7, arrangement, Rotation::Degrees0, SafeAreaInsets::ZERO)
                    .expect("valid metrics");
            let agreeing = DisplayReading {
                metrics,
                appkit_arrangement: Some(arrangement),
            };
            assert!(
                !agreeing.appkit_arrangement_mismatch(),
                "one flip must land AppKit's own frame exactly on the CG arrangement"
            );

            // The regression the check exists to catch: the AppKit frame's
            // raw y-up origin, never flipped (or flipped twice).
            let unflipped = DisplayReading {
                metrics,
                appkit_arrangement: Some(
                    LogicalRect::new(0, 1169, 1920, 1080).expect("valid arrangement"),
                ),
            };
            assert!(unflipped.appkit_arrangement_mismatch());

            // AppKit unreadable (off the main thread) is never a mismatch.
            let unreadable = DisplayReading {
                metrics,
                appkit_arrangement: None,
            };
            assert!(!unreadable.appkit_arrangement_mismatch());
        }

        /// A mode whose pixels cannot be reconciled with the display's own
        /// bounds must fall back to an honest 1:1 reading rather than
        /// advertising a scale its stream size disagrees with -- and must
        /// never drop the display.
        #[test]
        fn unreconcilable_modes_fall_back_to_a_one_to_one_reading() {
            let arrangement = retina_arrangement();
            let good = validated_metrics_or_unscaled(
                1,
                arrangement,
                (3600, 2338),
                Rotation::Degrees0,
                SafeAreaInsets::ZERO,
            )
            .expect("a valid 2x reading");
            assert_eq!(good.scale().get(), 2.0);
            assert_eq!(good.backing_size(), (3600, 2338));

            // Axes that disagree (a transposed reading) are refused by
            // `DisplayMetrics::new` and reported at 1x instead.
            let fallback = validated_metrics_or_unscaled(
                1,
                arrangement,
                (3600, 1169),
                Rotation::Degrees90,
                SafeAreaInsets {
                    top: 38,
                    ..SafeAreaInsets::ZERO
                },
            )
            .expect("an unreconcilable mode still reports the display");
            assert_eq!(fallback.scale().get(), 1.0);
            assert_eq!(fallback.backing_size(), (1800, 1169));
            assert_eq!(fallback.arrangement(), arrangement);
            assert_eq!(fallback.rotation(), Rotation::Degrees90);
            assert_eq!(fallback.safe_area_insets().top, 38);
            assert_eq!(
                fallback.presentation_stream_extent(false, true).as_array(),
                [1800, 1130]
            );

            // A zero backing extent is the same story.
            assert_eq!(
                validated_metrics_or_unscaled(
                    1,
                    arrangement,
                    (0, 0),
                    Rotation::Degrees0,
                    SafeAreaInsets::ZERO,
                )
                .expect("a display with no readable pixels is still a display")
                .scale()
                .get(),
                1.0
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcen_media::Rotation;

    #[test]
    fn a_promotion_panel_reports_its_maximum_not_sixty() {
        // A built-in XDR panel: CG states no mode rate, AppKit 120.
        assert_eq!(reported_refresh_hz(0.0, Some(120)), 120);
        // An external 30 Hz panel stays 30; a 144 Hz one 144.
        assert_eq!(reported_refresh_hz(30.0, Some(30)), 30);
        assert_eq!(reported_refresh_hz(143.856, None), 144);
        // Nothing known: 60 Hz, as before.
        assert_eq!(reported_refresh_hz(0.0, None), 60);
        assert_eq!(reported_refresh_hz(f64::NAN, Some(0)), 60);
    }

    #[test]
    fn a_notched_panel_loses_the_menu_bar_not_just_the_notch_in_fullscreen() {
        // The 14-inch MacBook Pro as AppKit reports it: notch 38, menu bar 39.
        let top = fullscreen_top_inset(38.0, 39.0, None);
        assert_eq!(top, 39);
        let metrics = metrics_of(
            (1800, 1169),
            (3600, 2338),
            SafeAreaInsets {
                top,
                ..SafeAreaInsets::ZERO
            },
        );
        // Exactly the 1800x1130-point fullscreen viewport, at both scales.
        assert_eq!(
            metrics.presentation_stream_extent(false, true).as_array(),
            [3600, 2260]
        );
        assert_eq!(
            metrics.presentation_stream_extent(false, false).as_array(),
            [1800, 1130]
        );
        // Before: sized from the notch alone, two rows too tall.
        let notch_only = metrics_of(
            (1800, 1169),
            (3600, 2338),
            SafeAreaInsets {
                top: 38,
                ..SafeAreaInsets::ZERO
            },
        );
        assert_eq!(
            notch_only
                .presentation_stream_extent(false, true)
                .as_array(),
            [3600, 2262]
        );
    }

    #[test]
    fn the_fullscreen_top_inset_does_not_follow_the_menu_bar_being_hidden() {
        // Measured with the menu bar hidden (fullscreen, or auto-hide
        // "Always"): visibleFrame's gap drops to the 38-point notch, but it
        // is not an input; menuBarHeight still reads 39.
        assert_eq!(fullscreen_top_inset(38.0, 39.0, None), 39);
        // A real fullscreen window on the display is the truth, even when
        // the menu bar's height cannot be read.
        assert_eq!(fullscreen_top_inset(38.0, 0.0, Some(39.0)), 39);
        assert_eq!(fullscreen_top_inset(38.0, f64::NAN, Some(39.0)), 39);
        // Never less than the notch, rounded up.
        assert_eq!(fullscreen_top_inset(37.5, 0.0, None), 38);
        assert_eq!(fullscreen_top_inset(38.0, 39.0, Some(0.0)), 39);
        // No notch: the menu bar hides in fullscreen, nothing is lost.
        assert_eq!(fullscreen_top_inset(0.0, 24.0, Some(24.0)), 0);
        assert_eq!(fullscreen_top_inset(f64::NAN, 39.0, None), 0);
        assert_eq!(fullscreen_top_inset(38.0, f64::INFINITY, None), 38);
    }

    /// Metrics for one display at the desktop origin: `logical` points backed
    /// by `backing` pixels, with `insets` reserved by macOS.
    fn metrics_of(
        logical: (u32, u32),
        backing: (u32, u32),
        insets: SafeAreaInsets,
    ) -> DisplayMetrics {
        DisplayMetrics::new(
            1,
            LogicalRect::new(0, 0, logical.0, logical.1).expect("valid arrangement"),
            backing.0,
            backing.1,
            Rotation::Degrees0,
            insets,
        )
        .expect("valid metrics")
    }

    /// The measured 14" MacBook Pro "More Space" Retina panel.
    fn retina_1800x1169(insets: SafeAreaInsets) -> DisplayMetrics {
        metrics_of((1800, 1169), (3600, 2338), insets)
    }

    #[test]
    fn enumerate_reports_a_valid_layout_or_nothing() {
        // In headless CI there may be zero displays; when there are any, the
        // layout must satisfy the invariants the host relies on.
        let monitors = enumerate();
        if monitors.is_empty() {
            return;
        }
        // Exactly one primary.
        assert_eq!(
            monitors.iter().filter(|m| m.is_primary).count(),
            1,
            "layout must have exactly one primary display"
        );
        // Primary is sorted first.
        assert!(monitors[0].is_primary, "primary must be listed first");
        for m in &monitors {
            assert!(m.width_px >= 2 && m.height_px >= 2, "non-degenerate size");
            assert_eq!(m.width_px % 4, 0, "width must be encoder-aligned");
            assert!(m.scale > 0.0, "scale must be positive");
            assert!(m.refresh_hz > 0, "refresh must be positive");
            assert!(
                m.width_mm >= 0.0 && m.height_mm >= 0.0,
                "physical mm non-negative"
            );
        }
    }

    #[test]
    fn retina_uses_even_logical_stream_dimensions() {
        assert_eq!(
            retina_1800x1169(SafeAreaInsets::ZERO)
                .native_stream_extent()
                .as_array(),
            [1800, 1168]
        );
    }

    #[test]
    fn non_retina_keeps_native_pixels() {
        assert_eq!(
            metrics_of((2560, 1440), (2560, 1440), SafeAreaInsets::ZERO)
                .native_stream_extent()
                .as_array(),
            [2560, 1440]
        );
    }

    #[test]
    fn fractional_scaled_mode_uses_reported_logical_size() {
        let scaled = metrics_of((2000, 1333), (3000, 2000), SafeAreaInsets::ZERO);
        assert_eq!(scaled.native_stream_extent().as_array(), [2000, 1332]);
        // ...and advertises the exact ratio it streams at, never an integer
        // floor of it.
        assert_eq!(scaled.scale().get(), 1.5);
        assert_eq!(
            scaled.presentation_stream_extent(false, true).as_array(),
            [3000, 2000]
        );
    }

    #[test]
    fn missing_logical_mode_falls_back_to_backing_pixels() {
        let arrangement = LogicalRect::new(0, 0, 1920, 1080).expect("valid arrangement");
        // A display with no readable mode is a 1:1 display of whatever
        // rectangle macOS laid out for it.
        assert_eq!(
            metrics::backing_for_arrangement(arrangement, (0, 0), (0, 0)),
            (1920, 1080)
        );
        assert_eq!(
            metrics_of((1920, 1080), (1920, 1080), SafeAreaInsets::ZERO)
                .native_stream_extent()
                .as_array(),
            [1920, 1080]
        );
    }

    #[test]
    fn even_width_is_aligned_for_yuv444_capture() {
        assert_eq!(
            metrics_of((1366, 768), (1366, 768), SafeAreaInsets::ZERO)
                .native_stream_extent()
                .as_array(),
            [1364, 768]
        );
    }

    /// The bug the safe-area rule exists to fix. A 14" MacBook Pro at "More
    /// Space" reports a 1800x1169 logical mode, but macOS gives a fullscreen
    /// window only the 1130 pt below the notch. Pinning the stream to 1168
    /// made the viewer aspect-fit it into 1741x1130 — a 3.3% downscale with
    /// 29.5 pt pillarbox bars, measured exactly on 2026-08-03.
    #[test]
    fn notched_display_drops_the_menu_bar_inset() {
        // 38 is what NSScreen.safeAreaInsets actually returned on that machine,
        // and the resulting 1130 matches the fullscreen window AppKit handed
        // out (`0,39 1800x1130`), so this is the measured end-to-end number.
        for top in [38, 39] {
            let insets = SafeAreaInsets {
                top,
                ..SafeAreaInsets::ZERO
            };
            assert_eq!(
                metrics_of((1800, 1169), (1800, 1169), insets)
                    .presentation_stream_extent(false, false)
                    .as_array(),
                [1800, 1130],
                "safe-area top {top} must land on the 1800x1130 fullscreen viewport"
            );
        }
    }

    #[test]
    fn display_without_a_notch_is_unchanged() {
        let insets = SafeAreaInsets::default();
        assert!(insets.is_zero());
        // Matches the whole-display stream extent for the same display.
        let notchless = metrics_of((1800, 1169), (1800, 1169), insets);
        assert_eq!(
            notchless
                .presentation_stream_extent(false, false)
                .as_array(),
            [1800, 1168]
        );
        assert_eq!(notchless.native_stream_extent().as_array(), [1800, 1168]);
        assert_eq!(
            metrics_of((2560, 1440), (2560, 1440), insets)
                .presentation_stream_extent(false, false)
                .as_array(),
            [2560, 1440]
        );
    }

    #[test]
    fn presentation_size_stays_encoder_aligned() {
        let insets = SafeAreaInsets {
            top: 37,
            left: 3,
            ..SafeAreaInsets::ZERO
        };
        let stream =
            metrics_of((1800, 1169), (1800, 1169), insets).presentation_stream_extent(false, false);
        assert_eq!(stream.width() % 4, 0, "width must stay divisible by 4");
        assert_eq!(stream.height() % 2, 0, "height must stay even");
        assert_eq!(stream.as_array(), [1796, 1132]);
    }

    #[test]
    fn hidpi_pins_the_backing_pixels_for_true_physical_one_to_one() {
        // The 1:1 promise on a 2.0x Retina panel means the backing pixels, not
        // an integer upscale of the point-space image.
        let notched = retina_1800x1169(SafeAreaInsets {
            top: 39,
            ..SafeAreaInsets::ZERO
        });
        assert_eq!(
            notched.presentation_stream_extent(false, true).as_array(),
            [3600, 2260]
        );
        assert_eq!(
            retina_1800x1169(SafeAreaInsets::ZERO)
                .presentation_stream_extent(false, true)
                .as_array(),
            [3600, 2338]
        );
        // Covering the notch area asks for the whole panel back.
        assert_eq!(
            notched.presentation_stream_extent(true, true).as_array(),
            [3600, 2338]
        );
    }

    #[test]
    fn a_framebuffer_smaller_than_the_point_size_is_reported_one_to_one() {
        // Previously the two separate scale derivations both clamped this to
        // 1x; the shared reading keeps doing exactly that rather than
        // advertising a fractional sub-1 scale nothing can stream.
        let arrangement = LogicalRect::new(0, 0, 1800, 1169).expect("valid arrangement");
        assert_eq!(
            metrics::backing_for_arrangement(arrangement, (1800, 1169), (900, 584)),
            (1800, 1169)
        );
        let one_to_one = metrics_of((1800, 1169), (1800, 1169), SafeAreaInsets::ZERO);
        assert_eq!(one_to_one.scale().get(), 1.0);
        assert_eq!(
            one_to_one
                .presentation_stream_extent(false, true)
                .as_array(),
            [1800, 1168]
        );
    }

    #[test]
    fn absurd_insets_clamp_instead_of_underflowing() {
        let insets = SafeAreaInsets {
            top: 5_000,
            bottom: 5_000,
            left: 5_000,
            right: 5_000,
        };
        assert_eq!(
            metrics_of((1800, 1169), (1800, 1169), insets)
                .presentation_stream_extent(false, false)
                .as_array(),
            [4, 2]
        );
    }

    fn windows_pier_builtin_snapshot() -> DisplayMappingSnapshot {
        DisplayMappingSnapshot {
            primary: false,
            backing: (3600, 2338),
            logical: (1800, 1169),
            stream: (1800, 1168),
            insets: SafeAreaInsets {
                top: 38,
                bottom: 0,
                left: 0,
                right: 0,
            },
            scale_millis: 2_000,
            origin: (-1800, 832),
            rotation_degrees: 0,
            appkit_arrangement_mismatch: false,
        }
    }

    #[test]
    fn display_mapping_is_logged_once_per_change_not_once_per_frame() {
        // Regression: display enumeration runs on the frame loop, so the
        // unconditional record logged 46,316 times in one nine-minute
        // two-display session.
        let display_id = 9_000_001;
        let snapshot = windows_pier_builtin_snapshot();
        assert!(
            display_mapping_changed(display_id, &snapshot),
            "a display's first mapping must always be logged"
        );
        for _ in 0..10_000 {
            assert!(
                !display_mapping_changed(display_id, &snapshot),
                "an unchanged mapping must never log again, however many frames run"
            );
        }
    }

    #[test]
    fn every_logged_field_of_the_display_mapping_retriggers_it() {
        let display_id = 9_000_002;
        let base = windows_pier_builtin_snapshot();
        assert!(display_mapping_changed(display_id, &base));

        let mut mutations: Vec<DisplayMappingSnapshot> = Vec::new();
        let mut primary = base;
        primary.primary = true;
        mutations.push(primary);
        let mut backing = base;
        backing.backing = (3600, 2340);
        mutations.push(backing);
        let mut logical = base;
        logical.logical = (1800, 1170);
        mutations.push(logical);
        let mut stream = base;
        stream.stream = (1800, 1130);
        mutations.push(stream);
        let mut insets = base;
        insets.insets.top = 0;
        mutations.push(insets);
        let mut scale = base;
        scale.scale_millis = 1_000;
        mutations.push(scale);
        let mut origin = base;
        origin.origin = (0, 0);
        mutations.push(origin);
        let mut rotation = base;
        rotation.rotation_degrees = 90;
        mutations.push(rotation);
        let mut appkit = base;
        appkit.appkit_arrangement_mismatch = true;
        mutations.push(appkit);

        for mutated in mutations {
            assert!(
                display_mapping_changed(display_id, &mutated),
                "a genuinely changed mapping must always be logged: {mutated:?}"
            );
            assert!(!display_mapping_changed(display_id, &mutated));
            assert!(
                display_mapping_changed(display_id, &base),
                "and changing back must be logged too"
            );
            assert!(!display_mapping_changed(display_id, &base));
        }
    }

    #[test]
    fn display_mapping_edges_are_tracked_per_display() {
        let base = windows_pier_builtin_snapshot();
        assert!(display_mapping_changed(9_000_003, &base));
        assert!(
            display_mapping_changed(9_000_004, &base),
            "an identical mapping on a different display is still that display's first record"
        );
        assert!(!display_mapping_changed(9_000_003, &base));
        assert!(!display_mapping_changed(9_000_004, &base));
    }
}
