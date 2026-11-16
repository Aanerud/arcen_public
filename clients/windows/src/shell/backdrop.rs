// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The window's own surface under the composition tree.
//!
//! The window keeps its redirection surface, because user32 draws the menu
//! bar with GDI into it (with `WS_EX_NOREDIRECTIONBITMAP` the bar is never
//! drawn and the desktop shows through). The composition tree sits on top of
//! that surface, so its client part only shows where the tree is clear: the
//! letterbox bands of a session (black, as the Mac's) and the edges a resize
//! uncovers before the next frame. Winit's window class has no background
//! brush, so this paints it.

use std::sync::atomic::{AtomicU32, Ordering};

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    DC_BRUSH, FillRect, GetStockObject, HBRUSH, HDC, InvalidateRect, SetDCBrushColor,
};
use windows::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{GetClientRect, WM_ERASEBKGND};

const SUBCLASS_ID: usize = 0x4152_4244; // "ARBD"

/// The fill as a `COLORREF` (`0x00BBGGRR`).
static FILL: AtomicU32 = AtomicU32::new(0);

/// Paints the client part of the window's redirection surface.
#[derive(Debug)]
pub(super) struct Backdrop {
    hwnd: HWND,
    fill: egui::Color32,
}

fn colorref(colour: egui::Color32) -> u32 {
    u32::from(colour.r()) | u32::from(colour.g()) << 8 | u32::from(colour.b()) << 16
}

impl Backdrop {
    /// Starts painting `hwnd`'s background black.
    pub(super) fn install(hwnd: HWND) -> Result<Self, String> {
        FILL.store(colorref(egui::Color32::BLACK), Ordering::Relaxed);
        // SAFETY: `backdrop_proc` only reads `FILL`; the subclass lives as
        // long as the window.
        if !unsafe { SetWindowSubclass(hwnd, Some(backdrop_proc), SUBCLASS_ID, 0) }.as_bool() {
            return Err("SetWindowSubclass(backdrop) failed".to_owned());
        }
        Ok(Self {
            hwnd,
            fill: egui::Color32::BLACK,
        })
    }

    /// The colour under the tree: black around a session's picture, the
    /// screen's own fill otherwise. Repaints only on a change.
    pub(super) fn set_fill(&mut self, fill: egui::Color32) {
        if fill == self.fill {
            return;
        }
        self.fill = fill;
        FILL.store(colorref(fill), Ordering::Relaxed);
        // SAFETY: the window is alive and on this thread.
        let _ = unsafe { InvalidateRect(self.hwnd, None, true) };
    }
}

unsafe extern "system" fn backdrop_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _subclass: usize,
    _data: usize,
) -> LRESULT {
    if message == WM_ERASEBKGND {
        let hdc = HDC(wparam.0 as *mut std::ffi::c_void);
        let mut client = RECT::default();
        // SAFETY: `hdc` is the window's paint DC for this message; the stock
        // DC brush needs no release.
        unsafe {
            if GetClientRect(hwnd, &raw mut client).is_ok() {
                let _ = SetDCBrushColor(hdc, COLORREF(FILL.load(Ordering::Relaxed)));
                let _ = FillRect(hdc, &raw const client, HBRUSH(GetStockObject(DC_BRUSH).0));
                return LRESULT(1);
            }
        }
    }
    // SAFETY: forwarding the message unchanged down the subclass chain.
    unsafe { DefSubclassProc(hwnd, message, wparam, lparam) }
}
