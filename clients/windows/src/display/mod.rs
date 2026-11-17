// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Displays (owner: `platform` stream).
//!
//! The Deck's monitors as protocol [`ClientMonitor`]s, the same roster the
//! macOS Deck builds from `CGDisplay`/`NSScreen`: primary first, then left
//! to right, with each display's arrangement and size in logical units
//! (pixels ÷ the Per-Monitor-V2 scale), its scale, refresh, name, EDID
//! identity and colour facts.
//!
//! Sources: `EnumDisplayMonitors`/`GetMonitorInfoW` (rectangles, primary,
//! GDI device name), `GetDpiForMonitor` (effective DPI, read under a
//! Per-Monitor-V2 thread context whatever the process declared),
//! `QueryDisplayConfig`/`DisplayConfigGetDeviceInfo` (friendly name, EDID
//! ids, refresh, advanced colour state, SDR white level) and DXGI
//! `IDXGIOutput6::GetDesc1` (bits per colour, colour space, primaries,
//! luminance). Colour facts become [`DisplayColorMsg`] through the shared
//! rule `arcen_media::display_color::WindowsOutputColor`.
//!
//! **UI thread** by contract (`DeckServices::displays`); the calls
//! themselves are thread-safe on Windows.

#![allow(
    unsafe_code,
    reason = "Win32 is a C API; every unsafe block states its contract"
)]

use arcen_deck_core::display::{DisplayMetrics, LogicalRect, SafeAreaInsets};
use arcen_media::Rotation;
use arcen_media::display_color::{WindowsOutputColor, windows_sdr_white_nits};
use arcen_protocol::messages::{ClientMonitor, DisplayColorMsg};
use windows::Win32::Devices::Display::{
    DISPLAYCONFIG_DEVICE_INFO_GET_ADVANCED_COLOR_INFO,
    DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL, DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
    DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME, DISPLAYCONFIG_DEVICE_INFO_HEADER,
    DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO, DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO,
    DISPLAYCONFIG_ROTATION, DISPLAYCONFIG_ROTATION_ROTATE90, DISPLAYCONFIG_ROTATION_ROTATE180,
    DISPLAYCONFIG_ROTATION_ROTATE270, DISPLAYCONFIG_SDR_WHITE_LEVEL,
    DISPLAYCONFIG_SOURCE_DEVICE_NAME, DISPLAYCONFIG_TARGET_DEVICE_NAME, DisplayConfigGetDeviceInfo,
    GetDisplayConfigBufferSizes, QDC_ONLY_ACTIVE_PATHS, QueryDisplayConfig,
};
use windows::Win32::Foundation::{BOOL, ERROR_SUCCESS, LPARAM, RECT};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G10_NONE_P709, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P2020, DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020,
    DXGI_COLOR_SPACE_TYPE,
};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1, IDXGIOutput6};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForMonitor, MDT_EFFECTIVE_DPI,
    SetThreadDpiAwarenessContext,
};
use windows::Win32::UI::WindowsAndMessaging::MONITORINFOF_PRIMARY;
use windows::core::Interface;

/// Everything Windows states about one active display.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowsDisplayFacts {
    /// `\\.\DISPLAYn`.
    pub gdi_name: String,
    /// The roster id: the `n` of the GDI name.
    pub id: u32,
    /// The monitor rectangle in physical pixels (virtual-screen space).
    pub left: i32,
    /// See `left`.
    pub top: i32,
    /// Physical width in pixels.
    pub width_px: u32,
    /// Physical height in pixels.
    pub height_px: u32,
    /// Effective DPI (96 = 100 %).
    pub dpi: u32,
    /// The primary display.
    pub primary: bool,
    /// Refresh rate in Hz, when stated.
    pub refresh_hz: Option<f64>,
    /// The desktop rotation (`DISPLAYCONFIG_PATH_TARGET_INFO::rotation`);
    /// the rectangle is already the rotated one.
    pub rotation: Rotation,
    /// The monitor's own name (`DELL U2723QE`), when stated.
    pub friendly_name: Option<String>,
    /// EDID manufacturer id (big-endian PNP letters as Windows reports).
    pub edid_manufacturer: u16,
    /// EDID product code.
    pub edid_product: u16,
    /// Whether the display can do Advanced Color (HDR or WCG).
    pub advanced_color_supported: Option<bool>,
    /// Whether Advanced Color is on ("Use HDR").
    pub advanced_color_enabled: Option<bool>,
    /// Wide colour is enforced (an SDR display in WCG mode).
    pub wide_color_enforced: Option<bool>,
    /// Advanced Color is force-disabled by policy or driver.
    pub advanced_color_force_disabled: Option<bool>,
    /// `DISPLAYCONFIG_COLOR_ENCODING` of the link.
    pub color_encoding: Option<i32>,
    /// Bits per colour channel the link carries.
    pub bits_per_color_channel: Option<u32>,
    /// DXGI's view of the output, when an output matched.
    pub dxgi: Option<DxgiOutputFacts>,
    /// The SDR white level in nits, when stated.
    pub sdr_white_nits: Option<f32>,
}

/// `IDXGIOutput6::GetDesc1`, with the adapter the output hangs off.
#[derive(Debug, Clone, PartialEq)]
pub struct DxgiOutputFacts {
    /// The adapter's description (`NVIDIA RTX6000-8Q`).
    pub adapter: String,
    /// The adapter's LUID (`HighPart << 32 | LowPart`), to match a device.
    pub adapter_luid: i64,
    /// The `HMONITOR` the output drives.
    pub monitor: isize,
    /// `DesktopCoordinates` (left, top, right, bottom).
    pub desktop: (i32, i32, i32, i32),
    /// `BitsPerColor`.
    pub bits_per_color: u32,
    /// `ColorSpace` (`DXGI_COLOR_SPACE_TYPE`).
    pub color_space: i32,
    /// Red, green, blue primaries (CIE xy).
    pub primaries: [[f32; 2]; 3],
    /// White point (CIE xy).
    pub white_point: [f32; 2],
    /// `MinLuminance` in nits.
    pub min_luminance: f32,
    /// `MaxLuminance` in nits.
    pub max_luminance: f32,
    /// `MaxFullFrameLuminance` in nits.
    pub max_full_frame_luminance: f32,
}

impl DxgiOutputFacts {
    /// The colour space's name, for logs and Colour Check.
    #[must_use]
    pub fn color_space_name(&self) -> String {
        color_space_name(DXGI_COLOR_SPACE_TYPE(self.color_space))
    }

    /// Whether Windows composites this output as HDR.
    #[must_use]
    pub fn is_hdr(&self) -> bool {
        DXGI_COLOR_SPACE_TYPE(self.color_space) == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020
    }
}

/// A DXGI colour space's name, for logs, Colour Check and the presenter.
#[must_use]
pub fn color_space_name(space: DXGI_COLOR_SPACE_TYPE) -> String {
    match space {
        DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709 => "RGB_FULL_G22_NONE_P709".into(),
        DXGI_COLOR_SPACE_RGB_FULL_G10_NONE_P709 => "RGB_FULL_G10_NONE_P709 (scRGB)".into(),
        DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020 => "RGB_FULL_G2084_NONE_P2020 (HDR10)".into(),
        DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P2020 => "RGB_FULL_G22_NONE_P2020".into(),
        other => format!("DXGI_COLOR_SPACE {}", other.0),
    }
}

impl WindowsDisplayFacts {
    /// The scale Windows applies (`dpi / 96`), at least 1:1 for an
    /// unreadable DPI.
    #[must_use]
    pub fn scale(&self) -> f32 {
        if self.dpi == 0 {
            1.0
        } else {
            #[allow(clippy::cast_precision_loss, reason = "a DPI is small")]
            let dpi = self.dpi as f32;
            dpi / 96.0
        }
    }

    /// The colour facts for the hello, or `None` when DXGI said nothing.
    #[must_use]
    pub fn color(&self) -> Option<DisplayColorMsg> {
        let dxgi = self.dxgi.as_ref()?;
        Some(
            WindowsOutputColor {
                hdr_enabled: dxgi.is_hdr(),
                primaries: dxgi.primaries,
                min_luminance: dxgi.min_luminance,
                max_luminance: dxgi.max_luminance,
                max_full_frame_luminance: dxgi.max_full_frame_luminance,
                sdr_white_nits: self.sdr_white_nits,
            }
            .to_msg(),
        )
    }

    /// The roster entry, as the macOS Deck builds it: arrangement and size
    /// in logical units, the size aligned like its `StreamExtent`
    /// (width a multiple of 4, height even), 60 Hz when the refresh is not
    /// stated.
    #[must_use]
    pub fn to_client_monitor(&self) -> ClientMonitor {
        let origin = (
            self.logical_extent(self.left),
            self.logical_extent(self.top),
        );
        self.to_client_monitor_at(origin)
    }

    fn logical_extent(&self, pixels: i32) -> i32 {
        #[allow(
            clippy::cast_possible_truncation,
            reason = "screen coordinates are small"
        )]
        let value = (f64::from(pixels) / f64::from(self.scale())).round() as i32;
        value
    }

    /// The roster entry at a logical `origin` (from [`logical_origins`]).
    #[must_use]
    pub fn to_client_monitor_at(&self, origin: (i32, i32)) -> ClientMonitor {
        let scale = self.scale();
        let logical = |pixels: f64| -> i64 {
            #[allow(
                clippy::cast_possible_truncation,
                reason = "screen coordinates are small"
            )]
            let value = (pixels / f64::from(scale)).round() as i64;
            value
        };
        let width = u32::try_from(logical(f64::from(self.width_px))).unwrap_or(0);
        let height = u32::try_from(logical(f64::from(self.height_px))).unwrap_or(0);
        let refresh_hz = self
            .refresh_hz
            .filter(|hz| hz.is_finite() && *hz >= 1.0)
            .map_or(60, |hz| {
                #[allow(
                    clippy::cast_possible_truncation,
                    clippy::cast_sign_loss,
                    reason = "a positive refresh rate fits"
                )]
                let rounded = hz.round() as u32;
                rounded
            });
        ClientMonitor {
            id: self.id,
            x: origin.0,
            y: origin.1,
            width_px: if width < 4 { 4 } else { width & !3 },
            height_px: if height < 2 { 2 } else { height & !1 },
            scale,
            refresh_hz,
            is_primary: self.primary,
            name: self
                .friendly_name
                .clone()
                .map(|name| name.trim().to_owned())
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| format!("Display {}", self.id)),
            width_mm: 0.0,
            height_mm: 0.0,
            vendor: u32::from(self.edid_manufacturer),
            model: u32::from(self.edid_product),
            serial: 0,
            edid: String::new(),
            color: self.color(),
            rotation_degrees: arcen_deck_core::connect_plan::rotation_degrees(self.rotation),
        }
    }
}

impl WindowsDisplayFacts {
    /// The shared per-display metrics (`arcen_deck_core::display`) at a
    /// logical `origin` (from [`logical_origins`]): the arrangement in
    /// logical units, backed by the monitor's physical pixels. A borderless
    /// fullscreen window covers the whole monitor (the taskbar included), so
    /// nothing is reserved. `None` for a rectangle Windows did not state.
    #[must_use]
    pub fn metrics_at(&self, origin: (i32, i32)) -> Option<DisplayMetrics> {
        let width = u32::try_from(self.logical_extent(i32::try_from(self.width_px).ok()?)).ok()?;
        let height =
            u32::try_from(self.logical_extent(i32::try_from(self.height_px).ok()?)).ok()?;
        let arrangement = LogicalRect::new(origin.0, origin.1, width, height).ok()?;
        DisplayMetrics::new(
            self.id,
            arrangement,
            self.width_px,
            self.height_px,
            self.rotation,
            SafeAreaInsets::ZERO,
        )
        .ok()
    }
}

/// Logical origins for `displays`, so neighbours keep sharing an edge.
///
/// Windows lays monitors out edge to edge in physical pixels, each with
/// its own scale; dividing every origin by its own monitor's scale would
/// open gaps or overlaps between monitors of different scales. The macOS
/// Deck reports one logical space where neighbours touch exactly
/// (`CGDisplayBounds`), and Piers place outputs by touching edges
/// (`arcen_media::topology_placement`). So: the primary is at (0, 0); a
/// monitor that touches a placed one starts at that neighbour's logical
/// edge, offset along the edge by the physical difference in the
/// neighbour's scale; a monitor that touches nothing falls back to its
/// physical origin in the primary's scale.
#[must_use]
pub fn logical_origins(displays: &[WindowsDisplayFacts]) -> Vec<(i32, i32)> {
    let size = |display: &WindowsDisplayFacts| {
        (
            display.logical_extent(i32::try_from(display.width_px).unwrap_or(i32::MAX)),
            display.logical_extent(i32::try_from(display.height_px).unwrap_or(i32::MAX)),
        )
    };
    let in_scale = |pixels: i32, scale: f32| -> i32 {
        #[allow(
            clippy::cast_possible_truncation,
            reason = "screen coordinates are small"
        )]
        let value = (f64::from(pixels) / f64::from(scale)).round() as i32;
        value
    };
    let right = |d: &WindowsDisplayFacts| {
        d.left
            .saturating_add(i32::try_from(d.width_px).unwrap_or(0))
    };
    let bottom = |d: &WindowsDisplayFacts| {
        d.top
            .saturating_add(i32::try_from(d.height_px).unwrap_or(0))
    };
    let overlaps = |a0: i32, a1: i32, b0: i32, b1: i32| a0 < b1 && b0 < a1;

    let mut placed: Vec<Option<(i32, i32)>> = vec![None; displays.len()];
    let anchor = displays
        .iter()
        .position(|display| display.primary)
        .unwrap_or(0);
    let Some(primary) = displays.get(anchor) else {
        return Vec::new();
    };
    placed[anchor] = Some((
        primary.logical_extent(primary.left),
        primary.logical_extent(primary.top),
    ));
    loop {
        let mut progressed = false;
        for (index, display) in displays.iter().enumerate() {
            if placed[index].is_some() {
                continue;
            }
            let (width, height) = size(display);
            let found = displays.iter().enumerate().find_map(|(other, neighbour)| {
                let (nx, ny) = placed[other]?;
                let (nw, nh) = size(neighbour);
                let scale = neighbour.scale();
                let vertical = overlaps(
                    display.top,
                    bottom(display),
                    neighbour.top,
                    bottom(neighbour),
                );
                let horizontal = overlaps(
                    display.left,
                    right(display),
                    neighbour.left,
                    right(neighbour),
                );
                let along_y = ny + in_scale(display.top - neighbour.top, scale);
                let along_x = nx + in_scale(display.left - neighbour.left, scale);
                if vertical && display.left == right(neighbour) {
                    Some((nx + nw, along_y))
                } else if vertical && right(display) == neighbour.left {
                    Some((nx - width, along_y))
                } else if horizontal && display.top == bottom(neighbour) {
                    Some((along_x, ny + nh))
                } else if horizontal && bottom(display) == neighbour.top {
                    Some((along_x, ny - height))
                } else {
                    None
                }
            });
            if let Some(origin) = found {
                placed[index] = Some(origin);
                progressed = true;
            }
        }
        if !progressed {
            break;
        }
    }
    let primary_scale = primary.scale();
    displays
        .iter()
        .zip(placed)
        .map(|(display, origin)| {
            origin.unwrap_or_else(|| {
                (
                    in_scale(display.left, primary_scale),
                    in_scale(display.top, primary_scale),
                )
            })
        })
        .collect()
}

/// Orders a roster primary first, then left to right, then top to bottom.
pub fn sort_roster(monitors: &mut [ClientMonitor]) {
    monitors.sort_by(|a, b| {
        b.is_primary
            .cmp(&a.is_primary)
            .then(a.x.cmp(&b.x))
            .then(a.y.cmp(&b.y))
    });
}

/// The roster id of a GDI device name: `\\.\DISPLAY3` is 3.
#[must_use]
pub fn gdi_display_number(gdi_name: &str) -> Option<u32> {
    gdi_name
        .strip_prefix(r"\\.\DISPLAY")
        .and_then(|number| number.parse().ok())
        .filter(|number| *number != 0)
}

/// **UI thread.** The active displays, primary first.
#[must_use]
pub fn enumerate() -> Vec<ClientMonitor> {
    roster(&facts())
}

/// The roster for `displays`, arranged in one logical space.
#[must_use]
pub fn roster(displays: &[WindowsDisplayFacts]) -> Vec<ClientMonitor> {
    let origins = logical_origins(displays);
    let mut monitors: Vec<ClientMonitor> = displays
        .iter()
        .zip(origins)
        .map(|(display, origin)| display.to_client_monitor_at(origin))
        .collect();
    sort_roster(&mut monitors);
    monitors
}

/// **UI thread.** The active displays, primary first, each with the shared
/// metrics the stream sizes and the multi-monitor topology come from.
#[must_use]
pub fn enumerate_with_metrics() -> Vec<(ClientMonitor, DisplayMetrics)> {
    roster_with_metrics(&facts())
}

/// The roster for `displays` paired with each display's metrics, in roster
/// order. A display whose metrics cannot be built is left out of neither
/// list silently: it is logged and dropped from both, so the two never
/// disagree.
#[must_use]
pub fn roster_with_metrics(
    displays: &[WindowsDisplayFacts],
) -> Vec<(ClientMonitor, DisplayMetrics)> {
    let origins = logical_origins(displays);
    let mut pairs: Vec<(ClientMonitor, DisplayMetrics)> = displays
        .iter()
        .zip(origins)
        .filter_map(|(facts, origin)| {
            let Some(metrics) = facts.metrics_at(origin) else {
                tracing::warn!(
                    target: "arcen::display",
                    display_id = facts.id,
                    width = facts.width_px,
                    height = facts.height_px,
                    dpi = facts.dpi,
                    "display geometry could not be read; left out of the layout",
                );
                return None;
            };
            Some((facts.to_client_monitor_at(origin), metrics))
        })
        .collect();
    pairs.sort_by(|(a, _), (b, _)| {
        b.is_primary
            .cmp(&a.is_primary)
            .then(a.x.cmp(&b.x))
            .then(a.y.cmp(&b.y))
    });
    pairs
}

/// **UI thread.** The colour facts of display `id` ([`ClientMonitor::id`]).
#[must_use]
pub fn display_color(id: u32) -> Option<DisplayColorMsg> {
    facts()
        .into_iter()
        .find(|display| display.id == id)
        .and_then(|display| display.color())
}

/// **UI thread.** The LUID of the adapter that drives display `id`
/// ([`DxgiOutputFacts::adapter_luid`]), for the decoder probe.
#[must_use]
pub fn adapter_luid(id: u32) -> Option<i64> {
    facts()
        .into_iter()
        .find(|display| display.id == id)
        .and_then(|display| display.dxgi.map(|dxgi| dxgi.adapter_luid))
}

/// **UI thread.** Everything Windows states about each active display.
#[must_use]
pub fn facts() -> Vec<WindowsDisplayFacts> {
    let _dpi = ThreadDpiScope::per_monitor_v2();
    let config = display_config();
    let outputs = dxgi_outputs();
    monitors()
        .into_iter()
        .enumerate()
        .filter_map(|(index, monitor)| {
            let mut info = MONITORINFOEXW::default();
            info.monitorInfo.cbSize = u32::try_from(std::mem::size_of::<MONITORINFOEXW>()).ok()?;
            // SAFETY: `info` is a MONITORINFOEXW with cbSize set, which
            // GetMonitorInfoW accepts through its MONITORINFO prefix.
            unsafe {
                GetMonitorInfoW(monitor, std::ptr::from_mut(&mut info).cast::<MONITORINFO>())
            }
            .ok()
            .ok()?;
            let gdi_name = utf16_until_nul(&info.szDevice);
            let rect: RECT = info.monitorInfo.rcMonitor;
            let (mut dpi_x, mut dpi_y) = (0_u32, 0_u32);
            // SAFETY: `monitor` came from EnumDisplayMonitors; both
            // out-parameters are live.
            let dpi = unsafe {
                GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &raw mut dpi_x, &raw mut dpi_y)
            }
            .map_or(96, |()| dpi_x);
            let path = config.iter().find(|path| path.gdi_name == gdi_name);
            let dxgi = outputs
                .iter()
                .find(|(name, _)| *name == gdi_name)
                .map(|(_, facts)| facts.clone());
            Some(WindowsDisplayFacts {
                id: gdi_display_number(&gdi_name)
                    .unwrap_or_else(|| u32::try_from(index + 1).unwrap_or(u32::MAX)),
                gdi_name,
                left: rect.left,
                top: rect.top,
                width_px: u32::try_from(rect.right.saturating_sub(rect.left)).unwrap_or(0),
                height_px: u32::try_from(rect.bottom.saturating_sub(rect.top)).unwrap_or(0),
                dpi,
                primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
                refresh_hz: path.and_then(|path| path.refresh_hz),
                rotation: path.map_or(Rotation::Degrees0, |path| path.rotation),
                friendly_name: path.and_then(|path| path.friendly_name.clone()),
                edid_manufacturer: path.map_or(0, |path| path.edid_manufacturer),
                edid_product: path.map_or(0, |path| path.edid_product),
                advanced_color_supported: path
                    .and_then(|path| path.advanced_color)
                    .map(|color| color.supported),
                advanced_color_enabled: path
                    .and_then(|path| path.advanced_color)
                    .map(|color| color.enabled),
                wide_color_enforced: path
                    .and_then(|path| path.advanced_color)
                    .map(|color| color.wide_enforced),
                advanced_color_force_disabled: path
                    .and_then(|path| path.advanced_color)
                    .map(|color| color.force_disabled),
                color_encoding: path
                    .and_then(|path| path.advanced_color)
                    .map(|color| color.encoding),
                bits_per_color_channel: path
                    .and_then(|path| path.advanced_color)
                    .map(|color| color.bits)
                    .filter(|bits| *bits != 0),
                dxgi,
                sdr_white_nits: path.and_then(|path| path.sdr_white_nits),
            })
        })
        .collect()
}

/// Sets the calling thread's DPI awareness to Per-Monitor V2 for a scope,
/// so rectangles and DPI are physical whatever the process declared.
struct ThreadDpiScope {
    previous: windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT,
}

impl ThreadDpiScope {
    fn per_monitor_v2() -> Self {
        // SAFETY: a documented pseudo-handle constant; the previous context
        // is restored on drop.
        let previous =
            unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        Self { previous }
    }
}

impl Drop for ThreadDpiScope {
    fn drop(&mut self) {
        if !self.previous.is_invalid() {
            // SAFETY: restores the context this scope replaced.
            let _ = unsafe { SetThreadDpiAwarenessContext(self.previous) };
        }
    }
}

fn monitors() -> Vec<HMONITOR> {
    unsafe extern "system" fn collect(
        monitor: HMONITOR,
        _hdc: HDC,
        _rect: *mut RECT,
        data: LPARAM,
    ) -> BOOL {
        // SAFETY: `data` is the address of the Vec below, alive for the
        // whole synchronous enumeration.
        let monitors = unsafe { &mut *(data.0 as *mut Vec<HMONITOR>) };
        monitors.push(monitor);
        BOOL::from(monitors.len() < 64)
    }
    let mut monitors: Vec<HMONITOR> = Vec::new();
    // SAFETY: the callback only pushes into `monitors`, which outlives the
    // call; no clip rectangle or DC.
    let _ = unsafe {
        EnumDisplayMonitors(
            None,
            None,
            Some(collect),
            LPARAM(std::ptr::from_mut(&mut monitors) as isize),
        )
    };
    monitors
}

#[derive(Debug, Clone, Copy)]
#[allow(clippy::struct_excessive_bools, reason = "the bits Windows reports")]
struct AdvancedColor {
    supported: bool,
    enabled: bool,
    wide_enforced: bool,
    force_disabled: bool,
    encoding: i32,
    bits: u32,
}

struct PathFacts {
    gdi_name: String,
    refresh_hz: Option<f64>,
    rotation: Rotation,
    friendly_name: Option<String>,
    edid_manufacturer: u16,
    edid_product: u16,
    advanced_color: Option<AdvancedColor>,
    sdr_white_nits: Option<f32>,
}

/// `DISPLAYCONFIG_ROTATION` as the shared rotation (clockwise).
fn rotation_of(rotation: DISPLAYCONFIG_ROTATION) -> Rotation {
    match rotation {
        DISPLAYCONFIG_ROTATION_ROTATE90 => Rotation::Degrees90,
        DISPLAYCONFIG_ROTATION_ROTATE180 => Rotation::Degrees180,
        DISPLAYCONFIG_ROTATION_ROTATE270 => Rotation::Degrees270,
        _ => Rotation::Degrees0,
    }
}

fn display_config() -> Vec<PathFacts> {
    let (mut path_count, mut mode_count) = (0_u32, 0_u32);
    // SAFETY: both out-parameters are live.
    if unsafe {
        GetDisplayConfigBufferSizes(
            QDC_ONLY_ACTIVE_PATHS,
            &raw mut path_count,
            &raw mut mode_count,
        )
    } != ERROR_SUCCESS
    {
        return Vec::new();
    }
    let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); path_count.min(64) as usize];
    let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); mode_count.min(128) as usize];
    let (mut path_count, mut mode_count) = (
        u32::try_from(paths.len()).unwrap_or(0),
        u32::try_from(modes.len()).unwrap_or(0),
    );
    // SAFETY: both arrays are writable for the counts passed.
    if unsafe {
        QueryDisplayConfig(
            QDC_ONLY_ACTIVE_PATHS,
            &raw mut path_count,
            paths.as_mut_ptr(),
            &raw mut mode_count,
            modes.as_mut_ptr(),
            None,
        )
    } != ERROR_SUCCESS
    {
        return Vec::new();
    }
    paths.truncate(path_count as usize);
    paths
        .iter()
        .filter_map(|path| {
            let mut source = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
                header: header::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>(
                    DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
                    path.sourceInfo.adapterId,
                    path.sourceInfo.id,
                ),
                ..DISPLAYCONFIG_SOURCE_DEVICE_NAME::default()
            };
            device_info(&mut source.header)?;
            let gdi_name = utf16_until_nul(&source.viewGdiDeviceName);

            let target_header = |kind| (kind, path.targetInfo.adapterId, path.targetInfo.id);
            let mut target = DISPLAYCONFIG_TARGET_DEVICE_NAME::default();
            let (kind, adapter, id) = target_header(DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME);
            target.header = header::<DISPLAYCONFIG_TARGET_DEVICE_NAME>(kind, adapter, id);
            let named = device_info(&mut target.header).is_some();

            let mut color = DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO::default();
            let (kind, adapter, id) =
                target_header(DISPLAYCONFIG_DEVICE_INFO_GET_ADVANCED_COLOR_INFO);
            color.header = header::<DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO>(kind, adapter, id);
            let advanced_color = device_info(&mut color.header).map(|()| {
                // SAFETY: the union's only view is the bitfield word.
                let bits = unsafe { color.Anonymous.value };
                AdvancedColor {
                    supported: bits & 0x1 != 0,
                    enabled: bits & 0x2 != 0,
                    wide_enforced: bits & 0x4 != 0,
                    force_disabled: bits & 0x8 != 0,
                    encoding: color.colorEncoding.0,
                    bits: color.bitsPerColorChannel,
                }
            });

            let mut white = DISPLAYCONFIG_SDR_WHITE_LEVEL::default();
            let (kind, adapter, id) = target_header(DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL);
            white.header = header::<DISPLAYCONFIG_SDR_WHITE_LEVEL>(kind, adapter, id);
            let sdr_white_nits = device_info(&mut white.header)
                .and_then(|()| windows_sdr_white_nits(white.SDRWhiteLevel));

            let rate = path.targetInfo.refreshRate;
            let refresh_hz = (rate.Denominator != 0 && rate.Numerator != 0)
                .then(|| f64::from(rate.Numerator) / f64::from(rate.Denominator));
            Some(PathFacts {
                gdi_name,
                refresh_hz,
                rotation: rotation_of(path.targetInfo.rotation),
                friendly_name: named
                    .then(|| utf16_until_nul(&target.monitorFriendlyDeviceName))
                    .filter(|name| !name.is_empty()),
                edid_manufacturer: if named { target.edidManufactureId } else { 0 },
                edid_product: if named { target.edidProductCodeId } else { 0 },
                advanced_color,
                sdr_white_nits,
            })
        })
        .collect()
}

fn header<T>(
    kind: windows::Win32::Devices::Display::DISPLAYCONFIG_DEVICE_INFO_TYPE,
    adapter: windows::Win32::Foundation::LUID,
    id: u32,
) -> DISPLAYCONFIG_DEVICE_INFO_HEADER {
    DISPLAYCONFIG_DEVICE_INFO_HEADER {
        r#type: kind,
        size: u32::try_from(std::mem::size_of::<T>()).unwrap_or(0),
        adapterId: adapter,
        id,
    }
}

fn device_info(header: &mut DISPLAYCONFIG_DEVICE_INFO_HEADER) -> Option<()> {
    // SAFETY: `header` is the first field of a request structure whose
    // full size the header states.
    (unsafe { DisplayConfigGetDeviceInfo(header) } == 0).then_some(())
}

fn dxgi_outputs() -> Vec<(String, DxgiOutputFacts)> {
    let mut outputs = Vec::new();
    // SAFETY: plain factory creation.
    let Ok(factory) = (unsafe { CreateDXGIFactory1::<IDXGIFactory1>() }) else {
        return outputs;
    };
    for adapter_index in 0..16 {
        // SAFETY: enumeration stops at the first missing adapter.
        let Ok(adapter) = (unsafe { factory.EnumAdapters1(adapter_index) }) else {
            break;
        };
        // SAFETY: GetDesc1 fills a plain structure.
        let Ok(adapter_desc) = (unsafe { adapter.GetDesc1() }) else {
            continue;
        };
        let adapter_name = utf16_until_nul(&adapter_desc.Description);
        let adapter_luid = (i64::from(adapter_desc.AdapterLuid.HighPart) << 32)
            | i64::from(adapter_desc.AdapterLuid.LowPart);
        for output_index in 0..16 {
            // SAFETY: as above, for outputs.
            let Ok(dxgi_output) = (unsafe { adapter.EnumOutputs(output_index) }) else {
                break;
            };
            let Ok(described) = dxgi_output.cast::<IDXGIOutput6>() else {
                continue;
            };
            // SAFETY: GetDesc1 fills a plain structure.
            let Ok(desc) = (unsafe { described.GetDesc1() }) else {
                continue;
            };
            outputs.push((
                utf16_until_nul(&desc.DeviceName),
                DxgiOutputFacts {
                    adapter: adapter_name.clone(),
                    adapter_luid,
                    monitor: desc.Monitor.0 as isize,
                    desktop: (
                        desc.DesktopCoordinates.left,
                        desc.DesktopCoordinates.top,
                        desc.DesktopCoordinates.right,
                        desc.DesktopCoordinates.bottom,
                    ),
                    bits_per_color: desc.BitsPerColor,
                    color_space: desc.ColorSpace.0,
                    primaries: [desc.RedPrimary, desc.GreenPrimary, desc.BluePrimary],
                    white_point: desc.WhitePoint,
                    min_luminance: desc.MinLuminance,
                    max_luminance: desc.MaxLuminance,
                    max_full_frame_luminance: desc.MaxFullFrameLuminance,
                },
            ));
        }
    }
    outputs
}

fn utf16_until_nul(units: &[u16]) -> String {
    let end = units
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(units.len());
    String::from_utf16_lossy(&units[..end])
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use arcen_protocol::messages::DisplayGamutMsg;

    fn facts(dpi: u32, primary: bool, left: i32) -> WindowsDisplayFacts {
        WindowsDisplayFacts {
            gdi_name: r"\\.\DISPLAY2".to_owned(),
            id: 2,
            left,
            top: 0,
            width_px: 3840,
            height_px: 2160,
            dpi,
            primary,
            refresh_hz: Some(59.94),
            rotation: Rotation::Degrees0,
            friendly_name: Some("DELL U2723QE".to_owned()),
            edid_manufacturer: 0x10AC,
            edid_product: 0x4247,
            advanced_color_supported: Some(true),
            advanced_color_enabled: Some(true),
            wide_color_enforced: Some(false),
            advanced_color_force_disabled: Some(false),
            color_encoding: Some(0),
            bits_per_color_channel: Some(10),
            dxgi: Some(DxgiOutputFacts {
                adapter: "Test GPU".to_owned(),
                adapter_luid: 1,
                monitor: 0,
                desktop: (0, 0, 3840, 2160),
                bits_per_color: 10,
                color_space: DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020.0,
                primaries: [[0.68, 0.32], [0.265, 0.69], [0.15, 0.06]],
                white_point: [0.3127, 0.329],
                min_luminance: 0.1,
                max_luminance: 600.0,
                max_full_frame_luminance: 400.0,
            }),
            sdr_white_nits: Some(200.0),
        }
    }

    #[test]
    fn a_scaled_4k_display_is_reported_in_logical_units() {
        let monitor = facts(144, true, 0).to_client_monitor();
        assert!((monitor.scale - 1.5).abs() < f32::EPSILON);
        assert_eq!((monitor.width_px, monitor.height_px), (2560, 1440));
        assert_eq!(monitor.refresh_hz, 60);
        assert_eq!(monitor.name, "DELL U2723QE");
        assert_eq!((monitor.vendor, monitor.model), (0x10AC, 0x4247));
        let color = monitor.color.expect("colour facts");
        assert_eq!(color.gamut, DisplayGamutMsg::DisplayP3);
        assert!((color.hdr_headroom - 3.0).abs() < 1e-4);
        assert_eq!(color.peak_nits, Some(600.0));
    }

    #[test]
    fn sizes_are_aligned_like_the_mac_stream_extent_and_names_fall_back() {
        let mut odd = facts(96, false, 1920);
        odd.width_px = 1366;
        odd.height_px = 767;
        odd.friendly_name = None;
        odd.refresh_hz = None;
        odd.dxgi = None;
        let monitor = odd.to_client_monitor();
        assert_eq!((monitor.width_px, monitor.height_px), (1364, 766));
        assert_eq!((monitor.x, monitor.y), (1920, 0));
        assert_eq!(monitor.name, "Display 2");
        assert_eq!(monitor.refresh_hz, 60);
        assert!(monitor.color.is_none());
    }

    #[test]
    fn the_roster_is_primary_first_then_left_to_right() {
        let mut roster = vec![
            facts(96, false, 3840).to_client_monitor(),
            facts(96, false, -1920).to_client_monitor(),
            facts(96, true, 0).to_client_monitor(),
        ];
        sort_roster(&mut roster);
        assert!(roster[0].is_primary);
        assert_eq!(roster[1].x, -1920);
        assert_eq!(roster[2].x, 3840);
    }

    fn placed(
        left: i32,
        top: i32,
        width: u32,
        height: u32,
        dpi: u32,
        primary: bool,
    ) -> WindowsDisplayFacts {
        WindowsDisplayFacts {
            left,
            top,
            width_px: width,
            height_px: height,
            dpi,
            primary,
            dxgi: None,
            ..facts(dpi, primary, left)
        }
    }

    #[test]
    fn mixed_scale_neighbours_still_touch_in_logical_space() {
        // 1080p at 100 % with a 4K at 200 % to its right.
        let origins = logical_origins(&[
            placed(0, 0, 1920, 1080, 96, true),
            placed(1920, 0, 3840, 2160, 192, false),
        ]);
        assert_eq!(origins, vec![(0, 0), (1920, 0)]);
        // A 4K laptop at 150 % with a 1080p at 100 % to its right, offset down.
        let origins = logical_origins(&[
            placed(0, 0, 3840, 2160, 144, true),
            placed(3840, 300, 1920, 1080, 96, false),
        ]);
        assert_eq!(origins, vec![(0, 0), (2560, 200)]);
        // Left of and below the primary, and a detached monitor.
        let origins = logical_origins(&[
            placed(0, 0, 2560, 1440, 96, true),
            placed(-3840, 0, 3840, 2160, 192, false),
            placed(0, 1440, 1920, 1080, 144, false),
            placed(9000, 9000, 1920, 1080, 96, false),
        ]);
        assert_eq!(origins, vec![(0, 0), (-1920, 0), (0, 1440), (9000, 9000)]);
        let roster = roster(&[
            placed(0, 0, 1920, 1080, 96, true),
            placed(1920, 0, 3840, 2160, 192, false),
        ]);
        assert_eq!((roster[1].x, roster[1].width_px), (1920, 1920));
    }

    #[test]
    fn a_scaled_display_streams_logical_or_physical_pixels_through_the_shared_policy() {
        // A 4K panel at 150 %: point-size fullscreen is the logical desktop,
        // HiDPI ("Full resolution") the panel's own pixels, 1:1.
        let metrics = facts(144, true, 0).metrics_at((0, 0)).expect("metrics");
        assert_eq!(metrics.arrangement().size(), (2560, 1440));
        assert_eq!(
            metrics.presentation_stream_extent(false, false).as_array(),
            [2560, 1440]
        );
        assert_eq!(
            metrics.presentation_stream_extent(false, true).as_array(),
            [3840, 2160]
        );
        // 1366 x 768 at 125 %: the axes round differently but stay one scale.
        let laptop = placed(0, 0, 1366, 768, 120, true)
            .metrics_at((0, 0))
            .expect("a 125 % laptop panel has metrics");
        assert_eq!(laptop.backing_size(), (1366, 768));
        // The roster and the metrics come out in the same order.
        let pairs = roster_with_metrics(&[
            placed(1920, 0, 3840, 2160, 192, false),
            placed(0, 0, 1920, 1080, 96, true),
        ]);
        assert!(pairs[0].0.is_primary);
        assert_eq!(pairs[1].1.arrangement().x(), 1920);
        assert_eq!(pairs[1].1.backing_size(), (3840, 2160));
    }

    #[test]
    fn gdi_names_give_the_roster_id() {
        assert_eq!(gdi_display_number(r"\\.\DISPLAY3"), Some(3));
        assert_eq!(gdi_display_number(r"\\.\DISPLAY0"), None);
        assert_eq!(gdi_display_number("Display3"), None);
    }

    #[test]
    fn this_machine_has_a_primary_display() {
        let roster = enumerate();
        if roster.is_empty() {
            // A service session without a desktop has no monitors.
            return;
        }
        assert!(roster[0].is_primary);
        assert!(roster.iter().filter(|monitor| monitor.is_primary).count() == 1);
    }
}
