// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! A precision touchpad's two-finger scroll and pinch through
//! `DirectManipulation`, on a viewport nothing draws.
//!
//! Windows hands touchpad contacts to an application that asks for them on
//! `DM_POINTERHITTEST`; `DirectManipulation` then recognises the pan and the
//! pinch and keeps the system's inertia, and no wheel messages are made of
//! them. The viewport's content transform is read after each manual update
//! (a timer while a manipulation runs) and handed to the shared
//! [`TouchpadTracker`], which decides the phases. Without
//! `DirectManipulation` the touchpad keeps arriving as wheel messages.

use arcen_input::touchpad::{
    ContentTransform, ManipulationStatus, TouchpadOutput, TouchpadTracker,
};
use windows::Win32::Foundation::{HWND, RECT, WPARAM};
use windows::Win32::Graphics::DirectManipulation::{
    DIRECTMANIPULATION_BUILDING, DIRECTMANIPULATION_CONFIGURATION_INTERACTION,
    DIRECTMANIPULATION_CONFIGURATION_SCALING, DIRECTMANIPULATION_CONFIGURATION_TRANSLATION_INERTIA,
    DIRECTMANIPULATION_CONFIGURATION_TRANSLATION_X, DIRECTMANIPULATION_CONFIGURATION_TRANSLATION_Y,
    DIRECTMANIPULATION_DISABLED, DIRECTMANIPULATION_ENABLED, DIRECTMANIPULATION_INERTIA,
    DIRECTMANIPULATION_RUNNING, DIRECTMANIPULATION_STATUS, DIRECTMANIPULATION_SUSPENDED,
    DIRECTMANIPULATION_VIEWPORT_OPTIONS_MANUALUPDATE, DirectManipulationManager,
    IDirectManipulationContent, IDirectManipulationFrameInfoProvider, IDirectManipulationManager,
    IDirectManipulationUpdateManager, IDirectManipulationViewport,
};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
};
use windows::Win32::UI::Input::Pointer::GetPointerType;
use windows::Win32::UI::WindowsAndMessaging::{
    KillTimer, POINTER_INPUT_TYPE, PT_TOUCHPAD, SetTimer,
};

/// The window timer that drives manual updates while a manipulation runs.
pub(super) const TIMER_ID: usize = 0x4152_5450;
/// About one display frame.
const TIMER_MS: u32 = 16;
/// Ticks a contact may rest before it pans or pinches (two fingers laid
/// down, or one finger moving the cursor before a second joins), ~10 s.
const WAITING_TICKS: u32 = 600;
/// Ticks after a manipulation ended before the timer stops.
const ENDED_TICKS: u32 = 2;
/// The viewport's size; the content is far larger so a pan never meets an
/// edge, and it is put back in the middle after each manipulation.
const VIEW: i32 = 1000;
const CONTENT: i32 = 200_000;
const MIDDLE: f32 = 100_000.0;
const VIEW_F: f32 = 1000.0;

/// `DirectManipulation` on one session window.
pub(super) struct Touchpad {
    manager: IDirectManipulationManager,
    updates: IDirectManipulationUpdateManager,
    viewport: IDirectManipulationViewport,
    content: IDirectManipulationContent,
    /// The content transform after the last reset (the origin of a
    /// manipulation's travel).
    origin: [f32; 6],
    last: Option<[f32; 6]>,
    status: ManipulationStatus,
    tracker: TouchpadTracker,
    timer: bool,
    idle_ticks: u32,
    /// The contact's manipulation has run (panned or pinched).
    ran: bool,
}

impl std::fmt::Debug for Touchpad {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Touchpad")
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

impl Touchpad {
    /// `DirectManipulation` for `hwnd`: pan (with inertia) and pinch, updated
    /// by hand.
    ///
    /// # Errors
    ///
    /// `DirectManipulation` is unavailable; the touchpad stays a wheel.
    pub(super) fn new(hwnd: HWND) -> windows::core::Result<Self> {
        // SAFETY: COM on the window's thread (already initialised by the
        // shell; a second initialisation is counted), objects released on
        // drop.
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let manager: IDirectManipulationManager =
                CoCreateInstance(&DirectManipulationManager, None, CLSCTX_INPROC_SERVER)?;
            let updates: IDirectManipulationUpdateManager = manager.GetUpdateManager()?;
            let viewport: IDirectManipulationViewport =
                manager.CreateViewport(None::<&IDirectManipulationFrameInfoProvider>, hwnd)?;
            viewport.ActivateConfiguration(
                DIRECTMANIPULATION_CONFIGURATION_INTERACTION
                    | DIRECTMANIPULATION_CONFIGURATION_TRANSLATION_X
                    | DIRECTMANIPULATION_CONFIGURATION_TRANSLATION_Y
                    | DIRECTMANIPULATION_CONFIGURATION_TRANSLATION_INERTIA
                    | DIRECTMANIPULATION_CONFIGURATION_SCALING,
            )?;
            viewport.SetViewportOptions(DIRECTMANIPULATION_VIEWPORT_OPTIONS_MANUALUPDATE)?;
            let view = RECT {
                left: 0,
                top: 0,
                right: VIEW,
                bottom: VIEW,
            };
            viewport.SetViewportRect(&raw const view)?;
            let content: IDirectManipulationContent = viewport.GetPrimaryContent()?;
            let area = RECT {
                left: 0,
                top: 0,
                right: CONTENT,
                bottom: CONTENT,
            };
            content.SetContentRect(&raw const area)?;
            manager.Activate(hwnd)?;
            viewport.Enable()?;
            let mut touchpad = Self {
                manager,
                updates,
                viewport,
                content,
                origin: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
                last: None,
                status: ManipulationStatus::Ready,
                tracker: TouchpadTracker::new(),
                timer: false,
                idle_ticks: 0,
                ran: false,
            };
            touchpad.recentre();
            Ok(touchpad)
        }
    }

    /// `DM_POINTERHITTEST`: a touchpad contact goes to `DirectManipulation`.
    /// `true` when it took the contact.
    pub(super) fn hit_test(&mut self, hwnd: HWND, wparam: WPARAM) -> bool {
        #[allow(
            clippy::cast_possible_truncation,
            reason = "the low word is the pointer id"
        )]
        let id = u32::from(wparam.0 as u16);
        let mut kind = POINTER_INPUT_TYPE::default();
        // SAFETY: reads the type of a pointer Windows just reported.
        if unsafe { GetPointerType(id, &raw mut kind) }.is_err() || kind != PT_TOUCHPAD {
            return false;
        }
        if !matches!(
            self.status,
            ManipulationStatus::Running | ManipulationStatus::Inertia
        ) {
            // Travel no poll saw (the timer stopped) is not this contact's.
            self.recentre();
        }
        // SAFETY: the viewport is this window's.
        if unsafe { self.viewport.SetContact(id) }.is_err() {
            return false;
        }
        self.ran = false;
        self.start_timer(hwnd);
        true
    }

    /// The timer: one manual update, and what it produced.
    pub(super) fn tick(&mut self, hwnd: HWND) -> Vec<TouchpadOutput> {
        let mut out = Vec::new();
        // SAFETY: the manager, viewport and content are this window's.
        let (status, transform) = unsafe {
            let _ = self
                .updates
                .Update(None::<&IDirectManipulationFrameInfoProvider>);
            let status = self
                .viewport
                .GetStatus()
                .map_or(ManipulationStatus::Ready, status);
            let mut matrix = [0.0_f32; 6];
            let transform = self
                .content
                .GetContentTransform(&mut matrix)
                .ok()
                .map(|()| matrix);
            (status, transform)
        };
        let moving = matches!(
            status,
            ManipulationStatus::Running | ManipulationStatus::Inertia
        );
        if status != self.status {
            // Content that moved before the status changed belongs to the
            // old stage (the last pan step before the fingers lifted).
            if let Some(matrix) = transform
                && !moving
            {
                out.extend(self.content_moved(matrix));
            }
            self.status = status;
            out.extend(self.tracker.status(status));
            if !moving {
                self.recentre();
            }
        }
        if moving {
            self.idle_ticks = 0;
            self.ran = true;
            if let Some(matrix) = transform {
                out.extend(self.content_moved(matrix));
            }
        } else {
            self.idle_ticks += 1;
            let limit = if self.ran { ENDED_TICKS } else { WAITING_TICKS };
            if self.idle_ticks > limit {
                self.stop_timer(hwnd);
            }
        }
        out
    }

    fn content_moved(&mut self, matrix: [f32; 6]) -> Vec<TouchpadOutput> {
        if self.last == Some(matrix) {
            return Vec::new();
        }
        self.last = Some(matrix);
        let origin = self.origin;
        let scale = if origin[0] > 0.0 {
            f64::from(matrix[0] / origin[0])
        } else {
            1.0
        };
        self.tracker.content(ContentTransform {
            scale,
            x: f64::from(matrix[4] - origin[4]),
            y: f64::from(matrix[5] - origin[5]),
        })
    }

    /// Puts the content back in the middle, unscaled, and takes that as the
    /// next manipulation's origin.
    fn recentre(&mut self) {
        // SAFETY: the viewport and content are this window's.
        unsafe {
            let _ =
                self.viewport
                    .ZoomToRect(MIDDLE, MIDDLE, MIDDLE + VIEW_F, MIDDLE + VIEW_F, false);
            let _ = self
                .updates
                .Update(None::<&IDirectManipulationFrameInfoProvider>);
            let mut matrix = [0.0_f32; 6];
            if self.content.GetContentTransform(&mut matrix).is_ok() {
                self.origin = matrix;
                self.last = Some(matrix);
            }
        }
    }

    fn start_timer(&mut self, hwnd: HWND) {
        self.idle_ticks = 0;
        if !self.timer {
            // SAFETY: a timer on this thread's window; killed on stop/drop.
            self.timer = unsafe { SetTimer(hwnd, TIMER_ID, TIMER_MS, None) } != 0;
        }
    }

    fn stop_timer(&mut self, hwnd: HWND) {
        if self.timer {
            // SAFETY: the timer `start_timer` set.
            let _ = unsafe { KillTimer(hwnd, TIMER_ID) };
            self.timer = false;
        }
    }

    /// Ends `DirectManipulation` for the window (the input is detached).
    pub(super) fn close(&mut self, hwnd: HWND) {
        self.stop_timer(hwnd);
        // SAFETY: as in `new`.
        unsafe {
            let _ = self.viewport.Stop();
            let _ = self.viewport.Abandon();
            let _ = self.manager.Deactivate(hwnd);
        }
    }
}

fn status(status: DIRECTMANIPULATION_STATUS) -> ManipulationStatus {
    match status {
        DIRECTMANIPULATION_BUILDING => ManipulationStatus::Building,
        DIRECTMANIPULATION_ENABLED => ManipulationStatus::Enabled,
        DIRECTMANIPULATION_DISABLED => ManipulationStatus::Disabled,
        DIRECTMANIPULATION_RUNNING => ManipulationStatus::Running,
        DIRECTMANIPULATION_INERTIA => ManipulationStatus::Inertia,
        DIRECTMANIPULATION_SUSPENDED => ManipulationStatus::Suspended,
        _ => ManipulationStatus::Ready,
    }
}
