// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Windows left on a display the local user cannot see.
//!
//! A session that isolates its display makes Windows gather every window
//! there. Returning the topology re-activates the user's other displays, but
//! Windows does not move windows back onto a display it re-activates: with a
//! virtual display driver or a dummy dongle as the session display, the local
//! user then sees an empty monitor and every window on a screen nobody can
//! look at. This module records the windows before the display changes and,
//! once the displays are back, moves what the shared
//! `arcen_outputs::display_lease::plan_window_repatriation` decides.
//!
//! The record is also written next to the display recovery journal, so the
//! crash watchdog and a later recovery move the windows too. Nothing here
//! ever blocks a session: a failure is logged and the windows stay put.

use std::path::{Path, PathBuf};

use arcen_outputs::display_lease::{
    plan_window_repatriation, DesktopArea, LocalDisplay, RepatriationInput, WindowBefore,
    WindowMove, WindowNow,
};
use serde::{Deserialize, Serialize};

use crate::logging::DISPLAY;

const RECORD_VERSION: u32 = 1;
const MAX_RECORD_BYTES: u64 = 1024 * 1024;
const RECORD_FILE_NAME: &str = "display-windows.json";

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct Area {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}

impl From<DesktopArea> for Area {
    fn from(area: DesktopArea) -> Self {
        Self {
            x: area.x,
            y: area.y,
            width: area.width,
            height: area.height,
        }
    }
}

impl From<Area> for DesktopArea {
    fn from(area: Area) -> Self {
        Self::new(area.x, area.y, area.width, area.height)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct RecordedWindow {
    id: u64,
    display: String,
    normal: Area,
}

/// The windows and the primary display before a session changed the
/// displays, and the displays the session then served.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct WindowLayoutRecord {
    version: u32,
    pre_session_primary: Option<String>,
    session_displays: Vec<String>,
    windows: Vec<RecordedWindow>,
}

impl WindowLayoutRecord {
    fn plan(&self, displays_now: &[LocalDisplay], windows_now: &[WindowNow]) -> Vec<WindowMove> {
        let windows_before = self
            .windows
            .iter()
            .map(|window| WindowBefore {
                id: window.id,
                display: window.display.clone(),
                normal: window.normal.into(),
            })
            .collect::<Vec<_>>();
        plan_window_repatriation(&RepatriationInput {
            session_displays: &self.session_displays,
            pre_session_primary: self.pre_session_primary.as_deref(),
            windows_before: &windows_before,
            displays_now,
            windows_now,
        })
    }
}

/// Where the window record of the display journal at `journal` lives.
pub(crate) fn record_path(journal: &Path) -> PathBuf {
    journal.with_file_name(RECORD_FILE_NAME)
}

fn write_record(path: &Path, record: &WindowLayoutRecord) -> Result<(), String> {
    let payload = serde_json::to_vec(record)
        .map_err(|error| format!("serialise window layout record: {error}"))?;
    crate::recovery::write_atomic_bytes(path, &payload, MAX_RECORD_BYTES, "window layout record")
}

fn read_record(path: &Path) -> Result<Option<WindowLayoutRecord>, String> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("read window layout record {path:?}: {error}")),
    };
    if bytes.len() as u64 > MAX_RECORD_BYTES {
        return Err(format!("window layout record {path:?} is too large"));
    }
    let record: WindowLayoutRecord = serde_json::from_slice(&bytes)
        .map_err(|error| format!("parse window layout record {path:?}: {error}"))?;
    if record.version != RECORD_VERSION {
        return Err(format!(
            "window layout record version {} is unsupported",
            record.version
        ));
    }
    Ok(Some(record))
}

/// The windows recorded for one session's display lease.
pub(crate) struct SessionWindows {
    record: WindowLayoutRecord,
    path: PathBuf,
}

impl SessionWindows {
    /// Records the windows before the session changes the displays. Never
    /// fails the session: an unreadable desktop yields `None`.
    pub(crate) fn capture(journal: &Path) -> Option<Self> {
        let path = record_path(journal);
        let (displays, windows) = match platform::snapshot() {
            Ok(snapshot) => snapshot,
            Err(error) => {
                tracing::warn!(
                    target: DISPLAY,
                    %error,
                    "could not record the desktop's windows; they will not be moved back after the session"
                );
                return None;
            }
        };
        let record = WindowLayoutRecord {
            version: RECORD_VERSION,
            pre_session_primary: displays
                .iter()
                .find(|display| display.primary)
                .map(|display| display.key.clone()),
            session_displays: Vec::new(),
            windows: windows
                .into_iter()
                .filter_map(|window| {
                    Some(RecordedWindow {
                        id: window.id,
                        display: window.display?,
                        normal: window.normal.into(),
                    })
                })
                .collect(),
        };
        if let Err(error) = write_record(&path, &record) {
            tracing::warn!(target: DISPLAY, %error, "could not persist the window layout record");
        }
        tracing::info!(
            target: DISPLAY,
            windows = record.windows.len(),
            primary = record.pre_session_primary.as_deref().unwrap_or("none"),
            "recorded the desktop's windows before the display lease"
        );
        Some(Self { record, path })
    }

    /// Names the displays the session served once the lease applied them.
    pub(crate) fn set_session_displays(&mut self, displays: Vec<String>) {
        self.record.session_displays = displays;
        if let Err(error) = write_record(&self.path, &self.record) {
            tracing::warn!(target: DISPLAY, %error, "could not persist the window layout record");
        }
    }

    /// The lease changed nothing: forget the record.
    pub(crate) fn discard(self) {
        let _ = crate::recovery::remove_file(&self.path, "window layout record");
    }

    /// Moves the windows the session stranded, after the displays are back.
    pub(crate) fn repatriate(self) {
        repatriate_record(&self.record);
        let _ = crate::recovery::remove_file(&self.path, "window layout record");
    }
}

/// Moves the windows a session stranded, from the record next to the display
/// journal at `journal`, once that journal's restore has succeeded (crash
/// watchdog, a later session's recovery, `restore-display`).
pub(crate) fn repatriate_pending(journal: &Path) {
    let path = record_path(journal);
    match read_record(&path) {
        Ok(Some(record)) => repatriate_record(&record),
        Ok(None) => return,
        Err(error) => tracing::warn!(target: DISPLAY, %error, "window layout record ignored"),
    }
    let _ = crate::recovery::remove_file(&path, "window layout record");
}

/// Forgets the window record next to the display journal at `journal` (its
/// windows belonged to a desktop that is gone).
pub(crate) fn discard_pending(journal: &Path) {
    let _ = crate::recovery::remove_file(&record_path(journal), "window layout record");
}

fn repatriate_record(record: &WindowLayoutRecord) {
    let (displays, windows) = match platform::snapshot() {
        Ok(snapshot) => snapshot,
        Err(error) => {
            tracing::warn!(
                target: DISPLAY,
                %error,
                "could not read the desktop's windows after the display restore"
            );
            return;
        }
    };
    let moves = record.plan(&displays, &windows);
    let mut moved = 0usize;
    let mut failed = 0usize;
    for planned in &moves {
        match platform::move_window(planned, &displays) {
            Ok(()) => moved += 1,
            Err(error) => {
                failed += 1;
                tracing::warn!(
                    target: DISPLAY,
                    window = planned.id,
                    to = %planned.to,
                    reason = ?planned.reason,
                    %error,
                    "could not move a window back to a visible display"
                );
            }
        }
    }
    tracing::info!(
        target: DISPLAY,
        session_displays = ?record.session_displays,
        primary = record.pre_session_primary.as_deref().unwrap_or("none"),
        planned = moves.len(),
        moved,
        failed,
        "windows the session left on its display moved back"
    );
}

#[cfg(windows)]
mod platform {
    use super::{DesktopArea, LocalDisplay, WindowMove, WindowNow};
    use windows::Win32::Foundation::{BOOL, HWND, LPARAM, POINT, RECT, TRUE};
    use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
    use windows::Win32::Graphics::Gdi::{
        EnumDisplayMonitors, GetMonitorInfoW, MonitorFromWindow, HDC, HMONITOR, MONITORINFO,
        MONITORINFOEXW, MONITOR_DEFAULTTONULL,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetClassNameW, GetWindow, GetWindowLongPtrW, GetWindowPlacement, IsIconic,
        IsWindowVisible, IsZoomed, SetWindowPlacement, GWL_EXSTYLE, GW_OWNER, MONITORINFOF_PRIMARY,
        SW_SHOWMAXIMIZED, SW_SHOWMINNOACTIVE, SW_SHOWNOACTIVATE, WINDOWPLACEMENT,
        WPF_ASYNCWINDOWPLACEMENT, WS_EX_TOOLWINDOW,
    };

    struct Monitor {
        display: LocalDisplay,
        monitor_rect: RECT,
    }

    const SHELL_CLASSES: [&str; 4] = [
        "Progman",
        "WorkerW",
        "Shell_TrayWnd",
        "Shell_SecondaryTrayWnd",
    ];

    fn area(rect: RECT) -> DesktopArea {
        DesktopArea::new(
            rect.left,
            rect.top,
            u32::try_from(rect.right.saturating_sub(rect.left)).unwrap_or(0),
            u32::try_from(rect.bottom.saturating_sub(rect.top)).unwrap_or(0),
        )
    }

    fn monitor_name(monitor: HMONITOR) -> Option<(String, MONITORINFO)> {
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        // SAFETY: `info` is a correctly sized MONITORINFOEXW whose header
        // announces the extended size.
        let ok = unsafe { GetMonitorInfoW(monitor, (&raw mut info).cast()) }.as_bool();
        if !ok {
            return None;
        }
        let units = info
            .szDevice
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(info.szDevice.len());
        Some((
            String::from_utf16_lossy(&info.szDevice[..units]),
            info.monitorInfo,
        ))
    }

    fn monitors() -> Result<Vec<Monitor>, String> {
        unsafe extern "system" fn collect(
            monitor: HMONITOR,
            _hdc: HDC,
            _rect: *mut RECT,
            data: LPARAM,
        ) -> BOOL {
            // SAFETY: `data` is the address of the Vec passed below, alive for
            // the synchronous enumeration.
            let list = unsafe { &mut *(data.0 as *mut Vec<HMONITOR>) };
            list.push(monitor);
            TRUE
        }
        let mut handles: Vec<HMONITOR> = Vec::new();
        // SAFETY: the callback only appends to `handles`, which outlives the call.
        let ok = unsafe {
            EnumDisplayMonitors(
                HDC::default(),
                None,
                Some(collect),
                LPARAM(&raw mut handles as isize),
            )
        };
        if !ok.as_bool() {
            return Err(format!(
                "EnumDisplayMonitors: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(handles
            .into_iter()
            .filter_map(|handle| {
                let (key, info) = monitor_name(handle)?;
                Some(Monitor {
                    display: LocalDisplay {
                        key,
                        work_area: area(info.rcWork),
                        primary: info.dwFlags & MONITORINFOF_PRIMARY != 0,
                    },
                    monitor_rect: info.rcMonitor,
                })
            })
            .collect())
    }

    /// Top-level windows a user sees in the task bar: visible, unowned, not
    /// tool windows, not cloaked (another virtual desktop, a suspended app),
    /// not the shell's own.
    fn candidate_windows() -> Vec<HWND> {
        unsafe extern "system" fn collect(window: HWND, data: LPARAM) -> BOOL {
            // SAFETY: `data` is the address of the Vec passed below.
            let list = unsafe { &mut *(data.0 as *mut Vec<HWND>) };
            list.push(window);
            TRUE
        }
        let mut all: Vec<HWND> = Vec::new();
        // SAFETY: the callback only appends to `all`, which outlives the call.
        let _ = unsafe { EnumWindows(Some(collect), LPARAM(&raw mut all as isize)) };
        all.into_iter()
            .filter(|window| {
                // SAFETY: plain queries on a window handle; a window that
                // vanished in between only makes them fail.
                unsafe {
                    if !IsWindowVisible(*window).as_bool() {
                        return false;
                    }
                    if GetWindow(*window, GW_OWNER).is_ok_and(|owner| !owner.is_invalid()) {
                        return false;
                    }
                    let ex_style = GetWindowLongPtrW(*window, GWL_EXSTYLE) as u32;
                    if ex_style & WS_EX_TOOLWINDOW.0 != 0 {
                        return false;
                    }
                    let mut cloaked = 0u32;
                    if DwmGetWindowAttribute(
                        *window,
                        DWMWA_CLOAKED,
                        (&raw mut cloaked).cast(),
                        std::mem::size_of::<u32>() as u32,
                    )
                    .is_ok()
                        && cloaked != 0
                    {
                        return false;
                    }
                    let mut class = [0u16; 64];
                    let length = usize::try_from(GetClassNameW(*window, &mut class)).unwrap_or(0);
                    let class = String::from_utf16_lossy(&class[..length.min(class.len())]);
                    !SHELL_CLASSES.contains(&class.as_str())
                }
            })
            .collect()
    }

    /// `rcNormalPosition` of a window without `WS_EX_TOOLWINDOW` is in
    /// workspace coordinates: screen coordinates less the primary monitor's
    /// work-area offset.
    fn workspace_offset(monitors: &[Monitor]) -> (i32, i32) {
        monitors
            .iter()
            .find(|monitor| monitor.display.primary)
            .map_or((0, 0), |monitor| {
                (
                    monitor.display.work_area.x - monitor.monitor_rect.left,
                    monitor.display.work_area.y - monitor.monitor_rect.top,
                )
            })
    }

    pub(super) fn snapshot() -> Result<(Vec<LocalDisplay>, Vec<WindowNow>), String> {
        crate::input::initialize_process_dpi_awareness();
        let monitors = monitors()?;
        if monitors.is_empty() {
            return Err("the desktop reports no monitor".to_string());
        }
        let (offset_x, offset_y) = workspace_offset(&monitors);
        let mut windows = Vec::new();
        for window in candidate_windows() {
            let mut placement = WINDOWPLACEMENT {
                length: std::mem::size_of::<WINDOWPLACEMENT>() as u32,
                ..Default::default()
            };
            // SAFETY: `placement` is initialised with its length.
            if unsafe { GetWindowPlacement(window, &mut placement) }.is_err() {
                continue;
            }
            let mut normal = area(placement.rcNormalPosition);
            normal.x += offset_x;
            normal.y += offset_y;
            if normal.width == 0 || normal.height == 0 {
                continue;
            }
            // SAFETY: a plain query on a window handle.
            let monitor = unsafe { MonitorFromWindow(window, MONITOR_DEFAULTTONULL) };
            let display = if monitor.is_invalid() {
                None
            } else {
                monitor_name(monitor).map(|(name, _)| name)
            };
            windows.push(WindowNow {
                id: window.0 as usize as u64,
                display,
                normal,
            });
        }
        Ok((
            monitors
                .into_iter()
                .map(|monitor| monitor.display)
                .collect(),
            windows,
        ))
    }

    pub(super) fn move_window(
        planned: &WindowMove,
        _displays: &[LocalDisplay],
    ) -> Result<(), String> {
        let monitors = monitors()?;
        let (offset_x, offset_y) = workspace_offset(&monitors);
        let window = HWND(planned.id as usize as *mut core::ffi::c_void);
        let mut placement = WINDOWPLACEMENT {
            length: std::mem::size_of::<WINDOWPLACEMENT>() as u32,
            ..Default::default()
        };
        // SAFETY: `placement` is initialised with its length; a stale handle
        // only makes the call fail.
        unsafe { GetWindowPlacement(window, &mut placement) }
            .map_err(|error| format!("GetWindowPlacement: {error}"))?;
        let normal = planned.normal;
        let left = normal.x - offset_x;
        let top = normal.y - offset_y;
        placement.rcNormalPosition = RECT {
            left,
            top,
            right: left.saturating_add(i32::try_from(normal.width).unwrap_or(i32::MAX)),
            bottom: top.saturating_add(i32::try_from(normal.height).unwrap_or(i32::MAX)),
        };
        placement.ptMaxPosition = POINT { x: -1, y: -1 };
        placement.ptMinPosition = POINT { x: -1, y: -1 };
        // SAFETY: plain queries on a window handle.
        let (minimised, maximised) =
            unsafe { (IsIconic(window).as_bool(), IsZoomed(window).as_bool()) };
        placement.showCmd = if minimised {
            SW_SHOWMINNOACTIVE.0 as u32
        } else if maximised {
            SW_SHOWMAXIMIZED.0 as u32
        } else {
            SW_SHOWNOACTIVATE.0 as u32
        };
        placement.flags |= WPF_ASYNCWINDOWPLACEMENT;
        // SAFETY: `placement` is a complete WINDOWPLACEMENT read from this
        // window and adjusted; the asynchronous flag keeps a hung window from
        // blocking the caller.
        unsafe { SetWindowPlacement(window, &placement) }
            .map_err(|error| format!("SetWindowPlacement: {error}"))
    }
}

#[cfg(not(windows))]
mod platform {
    use super::{LocalDisplay, WindowMove, WindowNow};

    pub(super) fn snapshot() -> Result<(Vec<LocalDisplay>, Vec<WindowNow>), String> {
        Err("window layout is only available on Windows".to_string())
    }

    pub(super) fn move_window(
        _planned: &WindowMove,
        _displays: &[LocalDisplay],
    ) -> Result<(), String> {
        Err("window layout is only available on Windows".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_record_survives_the_journal_directory_and_drives_the_shared_plan() {
        let directory = std::env::temp_dir().join(format!(
            "arcen-window-layout-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let journal = directory.join("display-recovery.json");
        let record = WindowLayoutRecord {
            version: RECORD_VERSION,
            pre_session_primary: Some(r"\\.\DISPLAY1".to_string()),
            session_displays: vec![r"\\.\DISPLAY2".to_string()],
            windows: vec![RecordedWindow {
                id: 0x10_0a2e,
                display: r"\\.\DISPLAY1".to_string(),
                normal: Area {
                    x: 40,
                    y: 30,
                    width: 900,
                    height: 700,
                },
            }],
        };
        write_record(&record_path(&journal), &record).unwrap();
        let read = read_record(&record_path(&journal)).unwrap().unwrap();
        assert_eq!(read, record);

        let displays = [
            LocalDisplay {
                key: r"\\.\DISPLAY1".to_string(),
                work_area: DesktopArea::new(0, 0, 2560, 1400),
                primary: true,
            },
            LocalDisplay {
                key: r"\\.\DISPLAY2".to_string(),
                work_area: DesktopArea::new(2560, 0, 1920, 1080),
                primary: false,
            },
        ];
        let now = [WindowNow {
            id: 0x10_0a2e,
            display: Some(r"\\.\DISPLAY2".to_string()),
            normal: DesktopArea::new(2600, 30, 900, 700),
        }];
        let moves = read.plan(&displays, &now);
        assert_eq!(moves.len(), 1);
        assert_eq!(moves[0].to, r"\\.\DISPLAY1");
        assert_eq!(moves[0].normal, DesktopArea::new(40, 30, 900, 700));
        std::fs::remove_dir_all(&directory).unwrap();
    }
}
