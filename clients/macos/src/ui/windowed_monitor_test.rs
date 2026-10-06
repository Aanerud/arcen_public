//! Developer-only windowed multi-monitor live-session test mode.
//!
//! `ARCEN_DECK_WINDOWED_MONITORS=2|3|4` asks a real Pier for that many
//! monitors while presenting each negotiated monitor in an ordinary decorated,
//! resizable, non-fullscreen window tiled on one local display. This is
//! compiled only with `dev-tools`: ordinary release/package builds contain no
//! parser, environment variable, synthetic topology, or alternate window
//! placement.

use arcen_media::{
    Monitor, MonitorIdentity, RequestedMonitor, RequestedMonitorTopology, Rotation,
    SessionMonitorId,
};

use crate::display::metrics::{DisplayMetrics, LogicalRect, SafeAreaInsets};
use crate::protocol::messages::ClientMonitor;

pub const MONITORS_ENV_VAR: &str = "ARCEN_DECK_WINDOWED_MONITORS";
pub const MONITOR_SIZE_ENV_VAR: &str = "ARCEN_DECK_WINDOWED_MONITOR_SIZE";

const DEFAULT_MONITOR_WIDTH_PX: u32 = 1_920;
const DEFAULT_MONITOR_HEIGHT_PX: u32 = 1_080;
const MIN_MONITOR_SIDE_PX: u32 = 320;
const MAX_MONITOR_SIDE_PX: u32 = 8_192;
const SYNTHETIC_ID_BASE: u32 = 0xA7CE_0000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowedMonitorTestMode {
    pub monitor_count: usize,
    pub monitor_size_px: [u32; 2],
}

impl WindowedMonitorTestMode {
    #[must_use]
    pub const fn default_size(monitor_count: usize) -> Self {
        Self {
            monitor_count,
            monitor_size_px: [DEFAULT_MONITOR_WIDTH_PX, DEFAULT_MONITOR_HEIGHT_PX],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowedMonitorTestEnv {
    Off,
    Active(WindowedMonitorTestMode),
    Invalid(String),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DisplayBoundsPts {
    pub cg_display_id: u32,
    pub origin_x: f32,
    pub origin_y: f32,
    pub width: f32,
    pub height: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TilingInsets {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
    pub gutter: f32,
    pub title_bar: f32,
}

impl TilingInsets {
    pub const MACOS_DEFAULT: Self = Self {
        left: 24.0,
        top: 64.0,
        right: 24.0,
        bottom: 96.0,
        gutter: 24.0,
        title_bar: 28.0,
    };
}

impl Default for TilingInsets {
    fn default() -> Self {
        Self::MACOS_DEFAULT
    }
}

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowedMonitorTestError {
    InvalidMonitorCount(usize),
    InvalidMonitorSize(String),
    NoDisplay,
    DisplayTooSmall {
        monitor_count: usize,
        inner_width_pts: u32,
        inner_height_pts: u32,
    },
    DisplayMetrics(crate::display::metrics::DisplayMetricsError),
    Media(arcen_media::MediaContractError),
}

impl std::fmt::Display for WindowedMonitorTestError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidMonitorCount(count) => {
                write!(formatter, "{MONITORS_ENV_VAR}={count} is invalid; accepted values are 2, 3, or 4")
            }
            Self::InvalidMonitorSize(value) => {
                write!(formatter, "{MONITOR_SIZE_ENV_VAR}={value:?} is invalid; expected WxH with each side {MIN_MONITOR_SIDE_PX}..={MAX_MONITOR_SIDE_PX}")
            }
            Self::NoDisplay => formatter.write_str("no local display is available for windowed monitor tiling"),
            Self::DisplayTooSmall {
                monitor_count,
                inner_width_pts,
                inner_height_pts,
            } => write!(
                formatter,
                "local display is too small to tile {monitor_count} monitor windows ({inner_width_pts}x{inner_height_pts} pt content)"
            ),
            Self::DisplayMetrics(error) => write!(formatter, "{error}"),
            Self::Media(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for WindowedMonitorTestError {}

impl From<crate::display::metrics::DisplayMetricsError> for WindowedMonitorTestError {
    fn from(error: crate::display::metrics::DisplayMetricsError) -> Self {
        Self::DisplayMetrics(error)
    }
}

impl From<arcen_media::MediaContractError> for WindowedMonitorTestError {
    fn from(error: arcen_media::MediaContractError) -> Self {
        Self::Media(error)
    }
}

#[must_use]
pub fn parse_env(monitors: Option<&str>, size: Option<&str>) -> WindowedMonitorTestEnv {
    let Some(raw_count) = monitors else {
        return WindowedMonitorTestEnv::Off;
    };
    let monitor_count = match raw_count.parse::<usize>() {
        Ok(count @ 2..=4) => count,
        Ok(other) => {
            return WindowedMonitorTestEnv::Invalid(
                WindowedMonitorTestError::InvalidMonitorCount(other).to_string(),
            )
        }
        Err(_) => {
            return WindowedMonitorTestEnv::Invalid(format!(
                "{MONITORS_ENV_VAR}={raw_count:?} is invalid; accepted values are 2, 3, or 4"
            ))
        }
    };
    let monitor_size_px = match size {
        Some(raw) => match parse_size(raw) {
            Ok(size) => size,
            Err(error) => return WindowedMonitorTestEnv::Invalid(error.to_string()),
        },
        None => [DEFAULT_MONITOR_WIDTH_PX, DEFAULT_MONITOR_HEIGHT_PX],
    };
    WindowedMonitorTestEnv::Active(WindowedMonitorTestMode {
        monitor_count,
        monitor_size_px,
    })
}

#[must_use]
pub fn mode_from_process_env() -> WindowedMonitorTestEnv {
    parse_env(
        std::env::var(MONITORS_ENV_VAR).ok().as_deref(),
        std::env::var(MONITOR_SIZE_ENV_VAR).ok().as_deref(),
    )
}

fn parse_size(raw: &str) -> Result<[u32; 2], WindowedMonitorTestError> {
    let Some((width, height)) = raw.split_once('x') else {
        return Err(WindowedMonitorTestError::InvalidMonitorSize(
            raw.to_string(),
        ));
    };
    if width.contains('x') || height.contains('x') || width.is_empty() || height.is_empty() {
        return Err(WindowedMonitorTestError::InvalidMonitorSize(
            raw.to_string(),
        ));
    }
    let width = width
        .parse::<u32>()
        .map_err(|_| WindowedMonitorTestError::InvalidMonitorSize(raw.to_string()))?;
    let height = height
        .parse::<u32>()
        .map_err(|_| WindowedMonitorTestError::InvalidMonitorSize(raw.to_string()))?;
    if !(MIN_MONITOR_SIDE_PX..=MAX_MONITOR_SIDE_PX).contains(&width)
        || !(MIN_MONITOR_SIDE_PX..=MAX_MONITOR_SIDE_PX).contains(&height)
    {
        return Err(WindowedMonitorTestError::InvalidMonitorSize(
            raw.to_string(),
        ));
    }
    Ok([width, height])
}

#[must_use]
pub const fn synthetic_display_id(index: usize) -> u32 {
    SYNTHETIC_ID_BASE + index as u32 + 1
}

#[must_use]
pub fn root_window_title(mode: WindowedMonitorTestMode) -> String {
    window_title_for(
        SessionMonitorId::new(1).expect("monitor 1"),
        mode.monitor_count,
    )
}

#[must_use]
pub fn window_title_for(session_monitor_id: SessionMonitorId, total: usize) -> String {
    format!(
        "Arcen Deck — monitor {} of {}",
        session_monitor_id.get(),
        total
    )
}

pub fn synthetic_requested_topology(
    mode: WindowedMonitorTestMode,
) -> Result<RequestedMonitorTopology, WindowedMonitorTestError> {
    if !(2..=4).contains(&mode.monitor_count) {
        return Err(WindowedMonitorTestError::InvalidMonitorCount(
            mode.monitor_count,
        ));
    }
    let [width, height] = mode.monitor_size_px;
    let mut requested = Vec::with_capacity(mode.monitor_count);
    for index in 0..mode.monitor_count {
        let column = index % 2;
        let row = index / 2;
        let x = i32::try_from(column).unwrap_or(0) * i32::try_from(width).unwrap_or(i32::MAX);
        let y = i32::try_from(row).unwrap_or(0) * i32::try_from(height).unwrap_or(i32::MAX);
        let display_id = synthetic_display_id(index);
        let metrics = DisplayMetrics::new(
            display_id,
            LogicalRect::new(x, y, width, height)?,
            width,
            height,
            Rotation::Degrees0,
            SafeAreaInsets::ZERO,
        )?;
        let monitor = Monitor {
            identity: MonitorIdentity {
                id: display_id.to_string(),
                name: format!("Windowed test monitor {}", index + 1),
                vendor: 0,
                model: 0,
                serial: u32::try_from(index + 1).unwrap_or(0),
            },
            x,
            y,
            width_px: width,
            height_px: height,
            scale: metrics.scale().get(),
            refresh_hz: crate::display::PINNED_MAX_REFRESH_HZ,
            rotation: Rotation::Degrees0,
            primary: index == 0,
            width_mm: 530.0,
            height_mm: 300.0,
            color: None,
        };
        requested.push(RequestedMonitor::new(monitor, width, height)?);
    }
    RequestedMonitorTopology::new(requested).map_err(WindowedMonitorTestError::from)
}

#[must_use]
pub fn client_monitors_from_synthetic_topology(
    topology: &RequestedMonitorTopology,
) -> Vec<ClientMonitor> {
    topology
        .monitors()
        .iter()
        .map(|requested| {
            let monitor = requested.monitor();
            ClientMonitor {
                id: monitor.identity.id.parse().unwrap_or_default(),
                x: monitor.x,
                y: monitor.y,
                width_px: monitor.width_px,
                height_px: monitor.height_px,
                scale: monitor.scale,
                refresh_hz: monitor.refresh_hz,
                is_primary: monitor.primary,
                name: monitor.identity.name.clone(),
                width_mm: monitor.width_mm,
                height_mm: monitor.height_mm,
                vendor: monitor.identity.vendor,
                model: monitor.identity.model,
                serial: monitor.identity.serial,
                edid: String::new(),
                color: monitor.color,
            }
        })
        .collect()
}

pub fn tile_placements(
    mode: WindowedMonitorTestMode,
    display: DisplayBoundsPts,
    insets: TilingInsets,
) -> Result<Vec<TilePlacement>, WindowedMonitorTestError> {
    if !(2..=4).contains(&mode.monitor_count) {
        return Err(WindowedMonitorTestError::InvalidMonitorCount(
            mode.monitor_count,
        ));
    }
    let columns = 2usize;
    let rows = if mode.monitor_count <= 2 {
        1usize
    } else {
        2usize
    };
    let available_width = display.width - insets.left - insets.right;
    let available_height = display.height - insets.top - insets.bottom;
    let cell_width = (available_width - insets.gutter) / 2.0;
    let cell_height = if rows == 1 {
        available_height
    } else {
        (available_height - insets.gutter) / 2.0
    };
    let max_inner_height = cell_height - insets.title_bar;
    let aspect = mode.monitor_size_px[0] as f32 / mode.monitor_size_px[1].max(1) as f32;
    let inner_width = (max_inner_height * aspect).min(cell_width);
    let inner_height = (inner_width / aspect).min(max_inner_height);
    if !(inner_width >= 320.0 && inner_height >= 200.0) {
        return Err(WindowedMonitorTestError::DisplayTooSmall {
            monitor_count: mode.monitor_count,
            inner_width_pts: inner_width.max(0.0).round() as u32,
            inner_height_pts: inner_height.max(0.0).round() as u32,
        });
    }
    let mut placements = Vec::with_capacity(mode.monitor_count);
    for index in 0..mode.monitor_count {
        let column = index % columns;
        let row = index / columns;
        let outer_width_offset = (cell_width - inner_width).max(0.0) / 2.0;
        let outer_height_offset = (cell_height - (inner_height + insets.title_bar)).max(0.0) / 2.0;
        placements.push(TilePlacement {
            session_monitor_id: SessionMonitorId::new(
                u16::try_from(index + 1).unwrap_or(u16::MAX),
            )?,
            index,
            total: mode.monitor_count,
            outer_x: display.origin_x
                + insets.left
                + column as f32 * (cell_width + insets.gutter)
                + outer_width_offset,
            outer_y: display.origin_y
                + insets.top
                + row as f32 * (cell_height + insets.gutter)
                + outer_height_offset,
            inner_width,
            inner_height,
        });
    }
    Ok(placements)
}

pub fn placement_for_monitor(
    mode: WindowedMonitorTestMode,
    display: DisplayBoundsPts,
    monitor_id: SessionMonitorId,
) -> Result<TilePlacement, WindowedMonitorTestError> {
    tile_placements(mode, display, TilingInsets::default())?
        .into_iter()
        .find(|tile| tile.session_monitor_id == monitor_id)
        .ok_or(WindowedMonitorTestError::InvalidMonitorCount(
            mode.monitor_count,
        ))
}

#[must_use]
pub fn confirmation_observation(
    mut observation: crate::ui::multi_window_runtime::ViewportBindObservation,
    expected_virtual_display_id: u32,
    backing_display_id: u32,
) -> crate::ui::multi_window_runtime::ViewportBindObservation {
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

#[cfg(target_os = "macos")]
pub fn primary_display_bounds() -> Option<DisplayBoundsPts> {
    let info = crate::ui::multi_window_runtime::live_active_displays()
        .into_iter()
        .next()?;
    let bounds = core_graphics::display::CGDisplay::new(info.cg_display_id).bounds();
    Some(DisplayBoundsPts {
        cg_display_id: info.cg_display_id,
        origin_x: bounds.origin.x as f32,
        origin_y: bounds.origin.y as f32,
        width: bounds.size.width as f32,
        height: bounds.size.height as f32,
    })
}

#[cfg(not(target_os = "macos"))]
pub fn primary_display_bounds() -> Option<DisplayBoundsPts> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::multi_window_runtime::MultiWindowPlan;

    fn display() -> DisplayBoundsPts {
        DisplayBoundsPts {
            cg_display_id: 42,
            origin_x: 10.0,
            origin_y: 20.0,
            width: 1_800.0,
            height: 1_200.0,
        }
    }

    #[test]
    fn env_parser_is_exact_and_defaults_size() {
        assert_eq!(parse_env(None, None), WindowedMonitorTestEnv::Off);
        assert_eq!(
            parse_env(Some("2"), None),
            WindowedMonitorTestEnv::Active(WindowedMonitorTestMode::default_size(2))
        );
        assert!(matches!(
            parse_env(Some("1"), None),
            WindowedMonitorTestEnv::Invalid(_)
        ));
        assert!(matches!(
            parse_env(Some("true"), None),
            WindowedMonitorTestEnv::Invalid(_)
        ));
        assert!(matches!(
            parse_env(Some("2 "), None),
            WindowedMonitorTestEnv::Invalid(_)
        ));
    }

    #[test]
    fn env_parser_accepts_bounded_wxh_size() {
        assert_eq!(
            parse_env(Some("4"), Some("2560x1440")),
            WindowedMonitorTestEnv::Active(WindowedMonitorTestMode {
                monitor_count: 4,
                monitor_size_px: [2_560, 1_440],
            })
        );
        for value in ["0x1080", "1920", "1920X1080", "1920x1080x1"] {
            assert!(matches!(
                parse_env(Some("2"), Some(value)),
                WindowedMonitorTestEnv::Invalid(_)
            ));
        }
    }

    #[test]
    fn synthetic_layout_is_primary_first_with_unique_virtual_ids() {
        let mode = WindowedMonitorTestMode {
            monitor_count: 3,
            monitor_size_px: [1_280, 720],
        };
        let topology = synthetic_requested_topology(mode).expect("synthetic topology");
        assert_eq!(topology.monitors().len(), 3);
        assert!(topology.monitors()[0].monitor().primary);
        assert_eq!(topology.monitors()[1].monitor().x, 1_280);
        assert_eq!(topology.monitors()[2].monitor().y, 720);
        let ids: Vec<_> = topology
            .monitors()
            .iter()
            .map(|monitor| monitor.monitor().identity.id.clone())
            .collect();
        assert_eq!(
            ids,
            vec![
                synthetic_display_id(0).to_string(),
                synthetic_display_id(1).to_string(),
                synthetic_display_id(2).to_string(),
            ]
        );
    }

    #[test]
    fn tile_geometry_handles_two_three_and_four_windows() {
        let two = tile_placements(
            WindowedMonitorTestMode::default_size(2),
            display(),
            TilingInsets::default(),
        )
        .expect("two tiles");
        assert_eq!(two.len(), 2);
        assert_eq!(two[0].outer_y, two[1].outer_y);
        assert!(two[1].outer_x > two[0].outer_x);

        let three = tile_placements(
            WindowedMonitorTestMode::default_size(3),
            display(),
            TilingInsets::default(),
        )
        .expect("three tiles");
        assert_eq!(three.len(), 3);
        assert!(three[2].outer_y > three[0].outer_y);

        let four = tile_placements(
            WindowedMonitorTestMode::default_size(4),
            display(),
            TilingInsets::default(),
        )
        .expect("four tiles");
        assert_eq!(four.len(), 4);
        assert!(four[3].outer_x > four[2].outer_x);
        assert_eq!(four[2].outer_y, four[3].outer_y);
    }

    #[test]
    fn tile_geometry_preserves_the_requested_monitor_aspect() {
        let tiles = tile_placements(
            WindowedMonitorTestMode {
                monitor_count: 2,
                monitor_size_px: [1_920, 1_080],
            },
            display(),
            TilingInsets::default(),
        )
        .expect("tiles");
        for tile in tiles {
            let aspect = tile.inner_width / tile.inner_height;
            assert!(
                (aspect - (16.0 / 9.0)).abs() < 0.01,
                "tile should preserve 16:9, got {aspect}"
            );
        }
    }

    #[test]
    fn production_plan_still_rejects_same_display_windows() {
        let ids = [
            SessionMonitorId::new(1).unwrap(),
            SessionMonitorId::new(2).unwrap(),
        ];
        assert!(
            MultiWindowPlan::build(&ids, &[42, 42]).is_err(),
            "dev-tools windowed mode must not weaken production duplicate-display invariants"
        );
    }
}
