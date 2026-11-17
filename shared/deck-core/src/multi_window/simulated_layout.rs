// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Simulated Deck layouts for testing multi-monitor on one screen
//! (developer builds only, `dev-tools`).
//!
//! `ARCEN_DECK_WINDOWED_MONITORS` makes a Deck ask a real Pier for a
//! synthetic layout of several monitors and show each monitor's stream in an
//! ordinary window on one local display, arranged as the layout is. It
//! accepts:
//!
//! - `2`, `3` or `4`: that many monitors of one size (`1920x1080`, or
//!   `ARCEN_DECK_WINDOWED_MONITOR_SIZE=WxH`), two per row, the first primary;
//! - `field`: a 2560x1600 primary at 0,0 and a 1920x1200 at -1920,400 (left
//!   of and below it);
//! - a list `WxH@X,Y[:rDEG][:HZ][*]` separated by `;`: each monitor's size as
//!   seen on the desk, its place, an optional clockwise rotation (90, 180,
//!   270; a quarter-turned monitor is given in its portrait size), an
//!   optional refresh in Hz, and `*` on the primary (the first without one).
//!
//! Everything here is pure and shared by both Decks; the platform shells
//! only read the environment and place the windows.

use arcen_media::{
    Monitor, MonitorIdentity, RequestedMonitor, RequestedMonitorTopology, Rotation,
    SessionMonitorId,
};

pub const MONITORS_ENV_VAR: &str = "ARCEN_DECK_WINDOWED_MONITORS";
pub const MONITOR_SIZE_ENV_VAR: &str = "ARCEN_DECK_WINDOWED_MONITOR_SIZE";

const DEFAULT_SIZE: (u32, u32) = (1920, 1080);
const MIN_SIDE: u32 = 320;
const MAX_SIDE: u32 = 8192;
const MAX_MONITORS: usize = arcen_media::MAX_MULTI_MONITOR_COUNT;
const SYNTHETIC_ID_BASE: u32 = 0xA7CE_0000;
const DEFAULT_REFRESH_HZ: u32 = 60;

/// One simulated monitor: its size and place as seen on the desk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SimulatedMonitor {
    pub width: u32,
    pub height: u32,
    pub x: i32,
    pub y: i32,
    pub rotation: Rotation,
    pub refresh_hz: u32,
    pub primary: bool,
}

/// A simulated layout of two to four monitors (fixed capacity, so it is
/// `Copy` like the rest of a shell's window state).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SimulatedLayout {
    monitors: [SimulatedMonitor; MAX_MONITORS],
    count: usize,
}

impl SimulatedLayout {
    /// The layout's monitors, in request order (session monitor `i + 1`).
    #[must_use]
    pub fn monitors(&self) -> &[SimulatedMonitor] {
        &self.monitors[..self.count]
    }

    /// How many monitors.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.count
    }

    /// Never true for a parsed layout (two to four monitors).
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }

    fn from_monitors(mut list: Vec<SimulatedMonitor>) -> Result<Self, String> {
        if !(2..=MAX_MONITORS).contains(&list.len()) {
            return Err(format!(
                "{MONITORS_ENV_VAR}: a layout has 2 to {MAX_MONITORS} monitors"
            ));
        }
        match list.iter().filter(|monitor| monitor.primary).count() {
            0 => list[0].primary = true,
            1 => {}
            _ => return Err(format!("{MONITORS_ENV_VAR}: more than one primary (*)")),
        }
        // Primary first: Piers number session monitors primary first, and the
        // windows are matched to session monitors by position.
        if let Some(index) = list.iter().position(|monitor| monitor.primary) {
            let primary = list.remove(index);
            list.insert(0, primary);
        }
        let mut monitors = [list[0]; MAX_MONITORS];
        monitors[..list.len()].copy_from_slice(&list);
        Ok(Self {
            monitors,
            count: list.len(),
        })
    }
}

/// What the environment asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SimulatedLayoutEnv {
    Off,
    Active(SimulatedLayout),
    Invalid(String),
}

/// Parses the two environment values.
#[must_use]
pub fn parse_env(monitors: Option<&str>, size: Option<&str>) -> SimulatedLayoutEnv {
    let Some(raw) = monitors.map(str::trim) else {
        return SimulatedLayoutEnv::Off;
    };
    let parsed = if raw.eq_ignore_ascii_case("field") {
        field_layout()
    } else if let Ok(count) = raw.parse::<usize>() {
        size.map_or(Ok(DEFAULT_SIZE), parse_size)
            .and_then(|size| grid_layout(count, size))
    } else {
        raw.split(';')
            .filter(|entry| !entry.trim().is_empty())
            .map(parse_monitor)
            .collect::<Result<Vec<_>, _>>()
            .and_then(SimulatedLayout::from_monitors)
    };
    match parsed {
        Ok(layout) => SimulatedLayoutEnv::Active(layout),
        Err(reason) => SimulatedLayoutEnv::Invalid(reason),
    }
}

/// The process environment's layout.
#[must_use]
pub fn from_process_env() -> SimulatedLayoutEnv {
    parse_env(
        std::env::var(MONITORS_ENV_VAR).ok().as_deref(),
        std::env::var(MONITOR_SIZE_ENV_VAR).ok().as_deref(),
    )
}

fn monitor(width: u32, height: u32, x: i32, y: i32, primary: bool) -> SimulatedMonitor {
    SimulatedMonitor {
        width,
        height,
        x,
        y,
        rotation: Rotation::Degrees0,
        refresh_hz: DEFAULT_REFRESH_HZ,
        primary,
    }
}

/// A field layout: a 2560x1600 primary and a 1920x1200 left of and below it.
fn field_layout() -> Result<SimulatedLayout, String> {
    SimulatedLayout::from_monitors(vec![
        monitor(2560, 1600, 0, 0, true),
        monitor(1920, 1200, -1920, 400, false),
    ])
}

fn grid_layout(count: usize, (width, height): (u32, u32)) -> Result<SimulatedLayout, String> {
    if !(2..=MAX_MONITORS).contains(&count) {
        return Err(format!(
            "{MONITORS_ENV_VAR}={count}: accepted counts are 2, 3 or 4"
        ));
    }
    let to_i32 = |value: u32| i32::try_from(value).unwrap_or(i32::MAX);
    SimulatedLayout::from_monitors(
        (0..count)
            .map(|index| {
                let column = i32::try_from(index % 2).unwrap_or(0);
                let row = i32::try_from(index / 2).unwrap_or(0);
                monitor(
                    width,
                    height,
                    column * to_i32(width),
                    row * to_i32(height),
                    index == 0,
                )
            })
            .collect(),
    )
}

fn parse_size(raw: &str) -> Result<(u32, u32), String> {
    let invalid = || format!("{raw:?}: expected WxH with each side {MIN_SIDE}..={MAX_SIDE}");
    let (width, height) = raw.trim().split_once('x').ok_or_else(invalid)?;
    let width: u32 = width.parse().map_err(|_| invalid())?;
    let height: u32 = height.parse().map_err(|_| invalid())?;
    if !(MIN_SIDE..=MAX_SIDE).contains(&width) || !(MIN_SIDE..=MAX_SIDE).contains(&height) {
        return Err(invalid());
    }
    Ok((width, height))
}

fn parse_monitor(raw: &str) -> Result<SimulatedMonitor, String> {
    let raw = raw.trim();
    let (raw, primary) = raw
        .strip_suffix('*')
        .map_or((raw, false), |rest| (rest, true));
    let mut parts = raw.split(':');
    let geometry = parts.next().unwrap_or_default();
    let invalid = || format!("{raw:?}: expected WxH@X,Y[:rDEG][:HZ][*]");
    let (size, place) = geometry.split_once('@').ok_or_else(invalid)?;
    let (width, height) = parse_size(size)?;
    let (x, y) = place.split_once(',').ok_or_else(invalid)?;
    let mut monitor = monitor(
        width,
        height,
        x.trim().parse().map_err(|_| invalid())?,
        y.trim().parse().map_err(|_| invalid())?,
        primary,
    );
    for part in parts {
        if let Some(degrees) = part.strip_prefix('r') {
            monitor.rotation = match degrees {
                "0" => Rotation::Degrees0,
                "90" => Rotation::Degrees90,
                "180" => Rotation::Degrees180,
                "270" => Rotation::Degrees270,
                _ => return Err(invalid()),
            };
        } else {
            let hz: u32 = part.trim_end_matches("Hz").parse().map_err(|_| invalid())?;
            if !(1..=500).contains(&hz) {
                return Err(invalid());
            }
            monitor.refresh_hz = hz;
        }
    }
    Ok(monitor)
}

/// The synthetic display id of monitor `index` (never a real display's).
#[must_use]
pub const fn synthetic_display_id(index: usize) -> u32 {
    #[allow(clippy::cast_possible_truncation, reason = "index is below four")]
    let index = index as u32;
    SYNTHETIC_ID_BASE + index + 1
}

/// The window title of session monitor `id` of `total`.
#[must_use]
pub fn window_title_for(id: SessionMonitorId, total: usize) -> String {
    format!("Arcen Deck — monitor {} of {}", id.get(), total)
}

/// The topology a Deck requests for `layout`.
///
/// # Errors
///
/// A shared topology contract the layout breaks (overlap, size).
pub fn requested_topology(
    layout: &SimulatedLayout,
) -> Result<RequestedMonitorTopology, arcen_media::MediaContractError> {
    let requested = layout
        .monitors()
        .iter()
        .enumerate()
        .map(|(index, simulated)| {
            let display_id = synthetic_display_id(index);
            let monitor = Monitor {
                identity: MonitorIdentity {
                    id: display_id.to_string(),
                    name: format!("Simulated monitor {}", index + 1),
                    vendor: 0,
                    model: 0,
                    serial: u32::try_from(index + 1).unwrap_or(0),
                },
                x: simulated.x,
                y: simulated.y,
                // As seen on the desk, as every Deck states its monitors; the
                // rotation says how the panel is turned.
                width_px: simulated.width,
                height_px: simulated.height,
                scale: 1.0,
                refresh_hz: simulated.refresh_hz,
                rotation: simulated.rotation,
                primary: simulated.primary,
                width_mm: 530.0,
                height_mm: 300.0,
                color: None,
            };
            RequestedMonitor::new(monitor, simulated.width, simulated.height)
        })
        .collect::<Result<Vec<_>, _>>()?;
    RequestedMonitorTopology::new(requested)
}

/// The local display the windows are placed on, in points.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DisplayBoundsPts {
    pub display_id: u32,
    pub origin_x: f32,
    pub origin_y: f32,
    pub width: f32,
    pub height: f32,
}

/// Margins of the area the windows are arranged in.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TilingInsets {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
    pub gutter: f32,
    pub title_bar: f32,
}

impl Default for TilingInsets {
    fn default() -> Self {
        Self {
            left: 24.0,
            top: 64.0,
            right: 24.0,
            bottom: 96.0,
            gutter: 24.0,
            title_bar: 28.0,
        }
    }
}

/// Where one monitor's window goes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TilePlacement {
    pub session_monitor_id: SessionMonitorId,
    pub index: usize,
    pub total: usize,
    pub outer_x: f32,
    pub outer_y: f32,
    pub inner_width: f32,
    pub inner_height: f32,
}

/// The windows, arranged on `display` as the layout is arranged on the
/// desk (scaled to fit, each window keeping its monitor's shape).
///
/// # Errors
///
/// The display is too small for windows of at least 160x100 points.
pub fn tile_placements(
    layout: &SimulatedLayout,
    display: DisplayBoundsPts,
    insets: TilingInsets,
) -> Result<Vec<TilePlacement>, String> {
    let monitors = layout.monitors();
    let left = monitors.iter().map(|monitor| monitor.x).min().unwrap_or(0);
    let top = monitors.iter().map(|monitor| monitor.y).min().unwrap_or(0);
    let right = monitors
        .iter()
        .map(|monitor| i64::from(monitor.x) + i64::from(monitor.width))
        .max()
        .unwrap_or(1);
    let bottom = monitors
        .iter()
        .map(|monitor| i64::from(monitor.y) + i64::from(monitor.height))
        .max()
        .unwrap_or(1);
    #[allow(clippy::cast_precision_loss, reason = "desktop coordinates")]
    let (span_w, span_h) = (
        (right - i64::from(left)).max(1) as f32,
        (bottom - i64::from(top)).max(1) as f32,
    );
    let available_w = display.width - insets.left - insets.right;
    let available_h = display.height - insets.top - insets.bottom;
    let scale = (available_w / span_w).min(available_h / span_h);
    let mut placements = Vec::with_capacity(monitors.len());
    for (index, monitor) in monitors.iter().enumerate() {
        #[allow(clippy::cast_precision_loss, reason = "desktop coordinates")]
        let (cell_x, cell_y, cell_w, cell_h) = (
            (i64::from(monitor.x) - i64::from(left)) as f32 * scale,
            (i64::from(monitor.y) - i64::from(top)) as f32 * scale,
            monitor.width as f32 * scale,
            monitor.height as f32 * scale,
        );
        #[allow(clippy::cast_precision_loss, reason = "monitor sizes")]
        let aspect = monitor.width as f32 / monitor.height.max(1) as f32;
        let max_w = cell_w - insets.gutter;
        let max_h = cell_h - insets.gutter - insets.title_bar;
        let inner_width = max_w.min(max_h * aspect);
        let inner_height = inner_width / aspect;
        if !(inner_width >= 160.0 && inner_height >= 100.0) {
            return Err(format!(
                "the local display is too small for {} monitor windows",
                monitors.len()
            ));
        }
        placements.push(TilePlacement {
            session_monitor_id: SessionMonitorId::new(u16::try_from(index + 1).unwrap_or(u16::MAX))
                .map_err(|error| error.to_string())?,
            index,
            total: monitors.len(),
            outer_x: display.origin_x + insets.left + cell_x + insets.gutter / 2.0,
            outer_y: display.origin_y + insets.top + cell_y + insets.gutter / 2.0,
            inner_width,
            inner_height,
        });
    }
    Ok(placements)
}

/// What a simulated monitor's window tells the shared multi-window engine.
/// A window that is shown on the local display carrying the layout
/// (`backing_display_id`) and is not closing counts as fullscreen on its
/// synthetic display (`expected_virtual_display_id`). Anything else passes
/// through unchanged, so the engine still sees a closed or missing window.
#[must_use]
pub fn confirmation_observation(
    mut observation: super::runtime::ViewportBindObservation,
    expected_virtual_display_id: u32,
    backing_display_id: u32,
) -> super::runtime::ViewportBindObservation {
    if observation.inner_rect_known
        && !observation.close_requested
        && observation.fullscreen != Some(true)
        && observation.observed_display_id == Some(backing_display_id)
    {
        observation.fullscreen = Some(true);
        observation.observed_display_id = Some(expected_virtual_display_id);
    }
    observation
}

/// The displays the engine sees while a layout is simulated: one synthetic
/// display per monitor, each the size of the local display.
#[must_use]
pub fn synthetic_displays(
    layout: &SimulatedLayout,
    display: DisplayBoundsPts,
) -> Vec<super::runtime::ActiveDisplayInfo> {
    (0..layout.len())
        .map(|index| super::runtime::ActiveDisplayInfo {
            display_id: synthetic_display_id(index),
            width_pts: display.width,
            height_pts: display.height,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_shown_window_counts_as_fullscreen_on_its_synthetic_display_and_a_closing_one_does_not() {
        let shown = super::super::runtime::ViewportBindObservation {
            inner_rect_known: true,
            fullscreen: Some(false),
            close_requested: false,
            monitor_size_pts: Some((1920.0, 1080.0)),
            inner_rect_size_pts: Some((800.0, 500.0)),
            observed_display_id: Some(1),
        };
        let bound = confirmation_observation(shown, synthetic_display_id(1), 1);
        assert_eq!(bound.fullscreen, Some(true));
        assert_eq!(bound.observed_display_id, Some(synthetic_display_id(1)));
        let elsewhere = confirmation_observation(shown, synthetic_display_id(1), 2);
        assert_eq!(elsewhere, shown, "a window on another display is not bound");
        let closing = super::super::runtime::ViewportBindObservation {
            close_requested: true,
            ..shown
        };
        assert_eq!(
            confirmation_observation(closing, synthetic_display_id(1), 1),
            closing
        );
        let field = active(parse_env(Some("field"), None));
        let display = DisplayBoundsPts {
            display_id: 1,
            origin_x: 0.0,
            origin_y: 0.0,
            width: 1920.0,
            height: 1080.0,
        };
        let synthetic = synthetic_displays(&field, display);
        assert_eq!(synthetic.len(), 2);
        assert_eq!(synthetic[1].display_id, synthetic_display_id(1));
    }

    fn active(env: SimulatedLayoutEnv) -> SimulatedLayout {
        match env {
            SimulatedLayoutEnv::Active(layout) => layout,
            other => panic!("not active: {other:?}"),
        }
    }

    #[test]
    fn counts_the_field_preset_and_lists_parse() {
        assert_eq!(parse_env(None, None), SimulatedLayoutEnv::Off);
        let grid = active(parse_env(Some("3"), Some("1280x720")));
        assert_eq!(grid.len(), 3);
        assert_eq!((grid.monitors()[2].x, grid.monitors()[2].y), (0, 720));
        let field = active(parse_env(Some("field"), None));
        assert_eq!(field.monitors()[1], monitor(1920, 1200, -1920, 400, false));
        assert!(field.monitors()[0].primary);
        let list = active(parse_env(
            Some("1920x1200@-1920,400; 1200x1920@0,0:r90:75*"),
            None,
        ));
        assert!(
            list.monitors()[0].primary && !list.monitors()[1].primary,
            "primary first"
        );
        assert_eq!(
            (list.monitors()[0].width, list.monitors()[0].height),
            (1200, 1920)
        );
        assert_eq!(list.monitors()[0].rotation, Rotation::Degrees90);
        assert_eq!(list.monitors()[0].refresh_hz, 75);
        for bad in [
            "1",
            "5",
            "garbage",
            "100x100@0,0;100x100@1,1",
            "800x600@0,0*;800x600@800,0*",
            "800x600@0,0:r45;800x600@800,0",
        ] {
            assert!(
                matches!(parse_env(Some(bad), None), SimulatedLayoutEnv::Invalid(_)),
                "{bad}"
            );
        }
    }

    #[test]
    fn the_field_layout_requests_a_valid_topology_with_rotation() {
        let field = active(parse_env(Some("field"), None));
        let topology = requested_topology(&field).expect("topology");
        assert_eq!(topology.monitors().len(), 2);
        let rotated = active(parse_env(Some("2560x1600@0,0*;1200x1920@2560,0:r90"), None));
        let topology = requested_topology(&rotated).expect("topology");
        let portrait = topology.monitors()[1].monitor();
        assert_eq!((portrait.width_px, portrait.height_px), (1200, 1920));
        assert_eq!(portrait.rotation, Rotation::Degrees90);
    }

    #[test]
    fn windows_keep_the_layout_s_arrangement() {
        let field = active(parse_env(Some("field"), None));
        let display = DisplayBoundsPts {
            display_id: 1,
            origin_x: 0.0,
            origin_y: 0.0,
            width: 1800.0,
            height: 1169.0,
        };
        let tiles = tile_placements(&field, display, TilingInsets::default()).expect("tiles");
        // The secondary is left of and below the primary.
        assert!(tiles[1].outer_x < tiles[0].outer_x);
        assert!(tiles[1].outer_y > tiles[0].outer_y);
        assert!(tiles[0].outer_x + tiles[0].inner_width <= display.width);
        let tiny = DisplayBoundsPts {
            width: 300.0,
            height: 200.0,
            ..display
        };
        assert!(tile_placements(&field, tiny, TilingInsets::default()).is_err());
    }
}
