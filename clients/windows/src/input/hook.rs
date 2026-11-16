// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The low-level keyboard hook (`WH_KEYBOARD_LL`), installed only while a
//! session window has focus.
//!
//! The shared [`HookRouter`] decides each edge: the Deck's own chords are
//! always taken (Windows would act on Ctrl+Windows chords otherwise); with
//! the keyboard grab on, the session also takes the system shortcuts
//! (Windows key, Alt+Tab, Ctrl+Esc, Print Screen). A taken key is posted to
//! the session window as [`WM_HOOK_KEY`] and never reaches the system; a
//! release always goes where its press went. Windows+L and Ctrl+Alt+Del
//! cannot be taken, by design of Windows.
//!
//! The hook runs on the window's thread and must answer fast: it only reads
//! the shared reader and router and posts a message.

use std::cell::RefCell;

use arcen_input::keyboard::pc::PcKey;
use arcen_input::keyboard::windows::WindowsKeyReader;
use arcen_input::keyboard::{HookRoute, HookRouter};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput,
    VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, HC_ACTION, HHOOK, KBDLLHOOKSTRUCT, PostMessageW, SetWindowsHookExW,
    UnhookWindowsHookEx, WH_KEYBOARD_LL, WM_APP,
};

/// A key the hook took, posted to the session window: `wParam` is the
/// Virtual-Key in the low word and the hook flags above it, `lParam` the
/// scan code in its low half and the event time in its high half.
pub(super) const WM_HOOK_KEY: u32 = WM_APP + 0x4A0;
/// `LLKHF_EXTENDED`.
pub(super) const LLKHF_EXTENDED: u32 = 0x01;
/// `LLKHF_INJECTED`.
const LLKHF_INJECTED: u32 = 0x10;
/// `LLKHF_UP`.
pub(super) const LLKHF_UP: u32 = 0x80;
/// `dwExtraInfo` of keys the input lab injects to stand in for a hardware
/// keyboard (`examples/input_lab.rs`): the hook routes them as if typed.
/// Every other injected key passes, the Deck's own mask key included.
pub const LAB_KEY_MARK: usize = 0x4152_4C42;
/// An unassigned Virtual-Key, the Windows Pier's "menu mask" key: typed
/// between a Windows key's press and release, it keeps Start shut.
const VK_MENU_MASK: u16 = 0xE8;

struct HookState {
    hook: HHOOK,
    hwnd: HWND,
    grabbed: bool,
    router: HookRouter,
    reader: WindowsKeyReader,
}

thread_local! {
    static HOOK: RefCell<Option<HookState>> = const { RefCell::new(None) };
}

/// Installs the hook for `hwnd` (or updates its grab), with `held` the keys
/// the window already holds.
pub(super) fn install(hwnd: HWND, grabbed: bool, held: Vec<PcKey>) {
    HOOK.with(|cell| {
        let Ok(mut slot) = cell.try_borrow_mut() else {
            return;
        };
        if let Some(state) = slot.as_mut() {
            if state.hwnd == hwnd && state.grabbed == grabbed {
                return;
            }
            state.hwnd = hwnd;
            state.grabbed = grabbed;
            state.router.begin(held);
            return;
        }
        // SAFETY: a thread hook procedure of this module; the module handle
        // of the executable is valid for its lifetime.
        let hook = unsafe {
            GetModuleHandleW(None)
                .and_then(|module| SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), module, 0))
        };
        match hook {
            Ok(hook) => {
                let mut router = HookRouter::default();
                router.begin(held);
                *slot = Some(HookState {
                    hook,
                    hwnd,
                    grabbed,
                    router,
                    reader: WindowsKeyReader::default(),
                });
            }
            Err(error) => {
                eprintln!("arcen-deck: keyboard hook unavailable: {error}");
            }
        }
    });
}

/// Removes the hook (focus lost, session over).
pub(super) fn uninstall() {
    HOOK.with(|cell| {
        let Ok(mut slot) = cell.try_borrow_mut() else {
            return;
        };
        if let Some(state) = slot.take() {
            // SAFETY: the hook this module installed.
            let _ = unsafe { UnhookWindowsHookEx(state.hook) };
        }
    });
}

/// Types the mask key so a Windows key the system holds does not open Start
/// when it goes up.
pub(super) fn inject_mask_key() {
    let key = |flags| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(VK_MENU_MASK),
                dwFlags: flags,
                ..KEYBDINPUT::default()
            },
        },
    };
    let inputs = [key(KEYBD_EVENT_FLAGS::default()), key(KEYEVENTF_KEYUP)];
    // SAFETY: two well-formed keyboard inputs.
    let _ = unsafe {
        SendInput(
            &inputs,
            i32::try_from(std::mem::size_of::<INPUT>()).unwrap_or(0),
        )
    };
}

unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == i32::try_from(HC_ACTION).unwrap_or(0) {
        // SAFETY: for HC_ACTION, lParam points at the event's
        // KBDLLHOOKSTRUCT for the duration of the call.
        let event = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        if take(event) {
            return LRESULT(1);
        }
    }
    // SAFETY: passes the event on unchanged.
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

/// Whether the hook takes this event (and has posted it to the window).
fn take(event: &KBDLLHOOKSTRUCT) -> bool {
    let flags = event.flags.0;
    if flags & LLKHF_INJECTED != 0 && event.dwExtraInfo != LAB_KEY_MARK {
        // The mask key, and anything another program types.
        return false;
    }
    HOOK.with(|cell| {
        let Ok(mut slot) = cell.try_borrow_mut() else {
            return false;
        };
        let Some(state) = slot.as_mut() else {
            return false;
        };
        let pressed = flags & LLKHF_UP == 0;
        #[allow(clippy::cast_possible_truncation, reason = "Virtual-Keys are one byte")]
        let vk = event.vkCode as u16;
        let (scan, extended) =
            super::keyboard::key_position(vk, event.scanCode, flags & LLKHF_EXTENDED != 0);
        let key = state.reader.read(vk, scan, extended, pressed);
        let route = state.router.route(key, pressed, state.grabbed);
        if route == HookRoute::System {
            return false;
        }
        if route == HookRoute::SessionMaskingWindowsKey {
            inject_mask_key();
        }
        let packed = usize::from(vk) | ((flags as usize & 0xFF) << 16);
        let packed_scan = (u64::from(event.time) << 32) | u64::from(event.scanCode);
        #[allow(
            clippy::cast_possible_truncation,
            reason = "x64 only: isize is 64 bits"
        )]
        let scan = packed_scan.cast_signed() as isize;
        // SAFETY: posting to the session window; a failure loses the key,
        // which the window's focus loss then releases.
        unsafe { PostMessageW(state.hwnd, WM_HOOK_KEY, WPARAM(packed), LPARAM(scan)) }.is_ok()
    })
}
