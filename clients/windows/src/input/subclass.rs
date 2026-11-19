// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The session window's subclass: every message reaches [`WindowInput`]
//! before the window's own procedure (winit's), which sees only what input
//! did not answer.
//!
//! The procedure is replaced with `SetWindowLongPtrW` and the old one
//! chained with `CallWindowProcW`, so nothing depends on which common
//! controls version the executable's manifest selects (`SetWindowSubclass`
//! is exported by name only from version 6).

use std::cell::RefCell;

use windows::Win32::Foundation::{HANDLE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    CallWindowProcW, DefWindowProcW, GWLP_WNDPROC, GetPropW, GetWindowLongPtrW, RemovePropW,
    SetPropW, SetWindowLongPtrW, WM_NCDESTROY, WNDPROC,
};
use windows::core::{PCWSTR, w};

use super::{WindowInput, hook};

const INPUT_PROP: PCWSTR = w!("ArcenDeckInput");
const PREVIOUS_PROP: PCWSTR = w!("ArcenDeckInputPrevious");

/// `TABLET_DISABLE_PRESSANDHOLD | TABLET_DISABLE_PENTAPFEEDBACK |
/// TABLET_DISABLE_PENBARRELFEEDBACK | TABLET_DISABLE_FLICKS`: the pen draws
/// on the host, so Windows' own pen gestures and feedback stay off here.
const TABLET_PEN_SERVICE_FLAGS: usize = 0x0000_0001 | 0x0000_0008 | 0x0000_0010 | 0x0001_0000;

/// Subclasses `hwnd` so `input` sees its messages first.
///
/// # Errors
///
/// When Windows refuses to replace the window procedure, or the window is
/// already subclassed by this module.
pub fn attach(hwnd: HWND, mut input: WindowInput) -> windows::core::Result<()> {
    // SAFETY: reads a property of the caller's window.
    if !unsafe { GetPropW(hwnd, INPUT_PROP) }.is_invalid() {
        return Err(windows::core::Error::from_hresult(
            windows::Win32::Foundation::E_INVALIDARG,
        ));
    }
    input.sync_focus();
    let cell = Box::into_raw(Box::new(RefCell::new(input)));
    // SAFETY: `cell` lives until the procedure is restored (WM_NCDESTROY or
    // `detach`), which frees it; the window is the caller's and this thread's.
    unsafe {
        let _ = SetPropW(
            hwnd,
            w!("MicrosoftTabletPenServiceProperty"),
            HANDLE(TABLET_PEN_SERVICE_FLAGS as *mut core::ffi::c_void),
        );
        SetPropW(hwnd, INPUT_PROP, HANDLE(cell.cast()))?;
        let previous = SetWindowLongPtrW(hwnd, GWLP_WNDPROC, proc_address());
        if previous == 0 {
            let _ = RemovePropW(hwnd, INPUT_PROP);
            drop(Box::from_raw(cell));
            return Err(windows::core::Error::from_win32());
        }
        SetPropW(
            hwnd,
            PREVIOUS_PROP,
            HANDLE(previous as *mut core::ffi::c_void),
        )?;
    }
    Ok(())
}

/// Restores the window's own procedure and drops its input (the session
/// window goes away before `WM_NCDESTROY`).
pub fn detach(hwnd: HWND) {
    // SAFETY: the properties `attach` set; the data is the box it leaked.
    unsafe {
        let data = RemovePropW(hwnd, INPUT_PROP).unwrap_or_default();
        let previous = RemovePropW(hwnd, PREVIOUS_PROP).unwrap_or_default();
        if !previous.is_invalid() && GetWindowLongPtrW(hwnd, GWLP_WNDPROC) == proc_address() {
            SetWindowLongPtrW(hwnd, GWLP_WNDPROC, previous.0 as isize);
        }
        if !data.is_invalid() {
            let cell = Box::from_raw(data.0.cast::<RefCell<WindowInput>>());
            if let Ok(mut input) = cell.try_borrow_mut() {
                input.release_all("detach");
                input.close_touchpad();
            }
            drop(cell);
        }
    }
    hook::uninstall(hwnd);
}

/// Runs `apply` on the window's input (the shell's calls: a session began,
/// the image moved, the menu toggled the grab). `None` when the window has
/// no input attached or it is busy with a message.
pub fn with_input<T>(hwnd: HWND, apply: impl FnOnce(&mut WindowInput) -> T) -> Option<T> {
    // SAFETY: the box `attach` leaked, alive while the property is set.
    let cell = unsafe {
        let data = GetPropW(hwnd, INPUT_PROP);
        if data.is_invalid() {
            return None;
        }
        &*(data.0 as *const RefCell<WindowInput>)
    };
    let mut input = cell.try_borrow_mut().ok()?;
    Some(apply(&mut input))
}

fn proc_address() -> isize {
    (subclass_proc as *const () as usize).cast_signed()
}

fn previous_proc(hwnd: HWND) -> WNDPROC {
    // SAFETY: the window procedure `attach` replaced, stored as a property.
    unsafe {
        let previous = GetPropW(hwnd, PREVIOUS_PROP);
        if previous.is_invalid() {
            return None;
        }
        std::mem::transmute::<*mut core::ffi::c_void, WNDPROC>(previous.0)
    }
}

unsafe extern "system" fn subclass_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let previous = previous_proc(hwnd);
    let chain = |hwnd, msg, wparam, lparam| {
        // SAFETY: passes the message to the window's own procedure.
        unsafe {
            if previous.is_some() {
                CallWindowProcW(previous, hwnd, msg, wparam, lparam)
            } else {
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
        }
    };
    if msg == WM_NCDESTROY {
        detach(hwnd);
        return chain(hwnd, msg, wparam, lparam);
    }
    // SAFETY: the box `attach` leaked, alive while the property is set.
    let data = unsafe { GetPropW(hwnd, INPUT_PROP) };
    if !data.is_invalid() {
        // SAFETY: as above.
        let cell = unsafe { &*(data.0 as *const RefCell<WindowInput>) };
        // A message sent while input is already handling one (a nested
        // PeekMessage dispatching a sent message) goes straight on.
        if let Ok(mut input) = cell.try_borrow_mut()
            && let Some(result) = input.handle(msg, wparam, lparam)
        {
            return result;
        }
    }
    chain(hwnd, msg, wparam, lparam)
}
