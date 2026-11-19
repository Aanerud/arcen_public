// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The pen ("Tablet support"): `WM_POINTER` messages of `PT_PEN` pointers,
//! every coalesced sample from `GetPointerPenInfoHistory`, oldest first,
//! through the shared Windows Ink mapper to the typed pen wire.
//!
//! A pen message is answered here, so the default procedure never turns it
//! into mouse messages: while the pen is in range it owns the pointer, as on
//! the macOS Deck. Touch is not captured (Windows makes it mouse input).
//!
//! Barrel buttons: the Windows Ink barrel (the button the tablet driver set
//! to Right Click) goes through the session's `PenButtonMap`; a click the
//! driver performs as a mouse click while the pen is in range (a Wacom side
//! switch set to Middle Click) arrives as a mouse message and is routed by
//! `arcen_input::deck_pen::route_mouse_while_pen` ([`Pen::mouse_message`]).

use std::time::Instant;

use arcen_input::deck_pen::{
    ImageRect, PenMouseRoute, WindowsInkMapper, WindowsInkSample, route_mouse_while_pen,
};
use arcen_input::pen_buttons::PenButtonMap;
use arcen_input::{LowLatencyMetadata, PenEvent};
use windows::Win32::Foundation::{HWND, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::ScreenToClient;
use windows::Win32::UI::Input::Pointer::{
    GetPointerPenInfo, GetPointerPenInfoHistory, GetPointerType, POINTER_PEN_INFO,
};
use windows::Win32::UI::WindowsAndMessaging::{POINTER_INPUT_TYPE, PT_PEN};

const WM_POINTERUPDATE: u32 = 0x0245;
const WM_POINTERDOWN: u32 = 0x0246;
const WM_POINTERUP: u32 = 0x0247;
const WM_POINTERENTER: u32 = 0x0249;
const WM_POINTERLEAVE: u32 = 0x024A;
const WM_POINTERCAPTURECHANGED: u32 = 0x024C;

/// Samples read per message at most; a pen reports a few hundred a second.
const MAX_HISTORY: u32 = 64;

/// The session window's pen.
#[derive(Debug, Default)]
pub(super) struct Pen {
    mapper: WindowsInkMapper,
    sequence: u64,
    last: (f64, f64),
}

impl Pen {
    pub(super) fn handle(
        &mut self,
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        image: ImageRect,
        epoch: Instant,
        mut emit: impl FnMut(PenEvent),
    ) -> Option<LRESULT> {
        if !matches!(
            msg,
            WM_POINTERUPDATE
                | WM_POINTERDOWN
                | WM_POINTERUP
                | WM_POINTERENTER
                | WM_POINTERLEAVE
                | WM_POINTERCAPTURECHANGED
        ) {
            return None;
        }
        #[allow(clippy::cast_possible_truncation, reason = "GET_POINTERID_WPARAM")]
        let id = u32::from(wparam.0 as u16);
        let mut kind = POINTER_INPUT_TYPE::default();
        // SAFETY: a plain query for this message's pointer.
        if unsafe { GetPointerType(id, &raw mut kind) }.is_err() || kind != PT_PEN {
            return None;
        }
        // A pen used this run is present (Tablet Monitor, diagnostics).
        crate::platform::pen::note_pen_observed();
        if matches!(msg, WM_POINTERLEAVE | WM_POINTERCAPTURECHANGED) {
            if let Some(event) = self.leave(epoch) {
                emit(event);
            }
            return Some(LRESULT(0));
        }
        for info in pen_history(id) {
            let mut point = info.pointerInfo.ptPixelLocation;
            // SAFETY: converts a point of this window's screen.
            let _ = unsafe { ScreenToClient(hwnd, &raw mut point) };
            let sample = WindowsInkSample {
                x: f64::from(point.x),
                y: f64::from(point.y),
                pointer_flags: info.pointerInfo.pointerFlags.0,
                pen_flags: info.penFlags,
                pen_mask: info.penMask,
                pressure: info.pressure,
                rotation: info.rotation,
                tilt_x: info.tiltX,
                tilt_y: info.tiltY,
            };
            let metadata = self.metadata(epoch);
            if let Some(event) = self.mapper.on_sample(sample, image, metadata) {
                self.last = (event.x, event.y);
                emit(event);
            }
        }
        Some(LRESULT(0))
    }

    /// The session's pen-button mapping (its settings and its host).
    pub(super) const fn set_button_map(&mut self, map: PenButtonMap) {
        self.mapper.set_button_map(map);
    }

    /// A mouse message while a pen is in range (it then owns the pointer,
    /// as on the macOS Deck, `PenInput::holds_authority`): `Some` when the pen takes
    /// it (its driver's right or middle click, sent as the pen's) or it is
    /// dropped (movement: the pen owns the pointer); `None` when it is the
    /// mouse's (a hovering left click, a release the pen did not press, the
    /// wheel), or no pen is in range.
    pub(super) fn mouse_message(
        &mut self,
        msg: u32,
        epoch: Instant,
        mut emit: impl FnMut(PenEvent),
    ) -> Option<LRESULT> {
        if !self.mapper.in_proximity() {
            return None;
        }
        match route_mouse_while_pen(msg, &self.mapper) {
            PenMouseRoute::Mouse => None,
            PenMouseRoute::Drop => Some(LRESULT(0)),
            PenMouseRoute::PenClick(click, pressed) => {
                let metadata = self.metadata(epoch);
                if let Some(event) = self.mapper.on_driver_click(click, pressed, metadata) {
                    tracing::debug!(
                        target: arcen_deck_core::log_target::INPUT,
                        ?click,
                        pressed,
                        buttons = event.buttons,
                        "pen button from the tablet driver's mouse click"
                    );
                    emit(event);
                }
                Some(LRESULT(0))
            }
        }
    }

    /// Whether the pen holds a click its tablet driver performed: its
    /// release must reach the pen wherever the pointer is.
    pub(super) const fn holds_driver_click(&self) -> bool {
        use arcen_input::pen_buttons::DriverClick;
        self.mapper.driver_held(DriverClick::Right) || self.mapper.driver_held(DriverClick::Middle)
    }

    /// The pen left (or the window lost it): a clean leave, if one was in
    /// range.
    pub(super) fn leave(&mut self, epoch: Instant) -> Option<PenEvent> {
        let metadata = self.metadata(epoch);
        self.mapper.leave_at(self.last.0, self.last.1, metadata)
    }

    fn metadata(&mut self, epoch: Instant) -> LowLatencyMetadata {
        self.sequence = self.sequence.wrapping_add(1);
        LowLatencyMetadata {
            sequence: self.sequence,
            timestamp_ns: u64::try_from(epoch.elapsed().as_nanos()).unwrap_or(u64::MAX),
            coalescable: false,
        }
    }
}

/// The pointer's samples since the last message, oldest first.
fn pen_history(id: u32) -> Vec<POINTER_PEN_INFO> {
    let mut latest = POINTER_PEN_INFO::default();
    // SAFETY: a plain query into a local struct.
    if unsafe { GetPointerPenInfo(id, &raw mut latest) }.is_err() {
        return Vec::new();
    }
    let mut count = latest.pointerInfo.historyCount.clamp(1, MAX_HISTORY);
    if count == 1 {
        return vec![latest];
    }
    let mut history = vec![POINTER_PEN_INFO::default(); count as usize];
    // SAFETY: `history` holds `count` entries; Windows writes at most that
    // many and says how many it wrote.
    if unsafe { GetPointerPenInfoHistory(id, &raw mut count, Some(history.as_mut_ptr())) }.is_err()
    {
        return vec![latest];
    }
    history.truncate(count as usize);
    // Windows lists the newest first.
    history.reverse();
    history
}
