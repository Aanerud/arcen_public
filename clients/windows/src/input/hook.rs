// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The low-level keyboard hook (`WH_KEYBOARD_LL`), installed only while a
//! session window has focus, and taking a press only while that window is
//! the foreground window.
//!
//! The shared [`HookRouter`] decides each edge: the Deck's own chords are
//! always taken (Ctrl+Alt+Home gives the keyboard to this PC); while the
//! host has the keyboard and the policy takes the system shortcuts (by
//! default whenever a session window has focus,
//! `KeyboardCapturePolicy::Host`), the session also takes them (Windows
//! key, Alt+Tab, Alt+Esc, Ctrl+Esc, Print Screen). A taken key is posted to
//! the session window as [`WM_HOOK_KEY`] and never reaches the system; a
//! release always goes where its press went. Windows+L and Ctrl+Alt+Del
//! cannot be taken, by design of Windows. While this PC has the keyboard
//! the hook is not installed at all. A press the hooks installed before
//! this one block (a Windows Pier's desk privacy, when this Deck runs
//! inside a Pier's session) is not taken either: it goes nowhere. Only the
//! release of a press the session already has still reaches it, so no key
//! stays down on the host ([`HookRouter::blocked_below`]).
//!
//! The hook runs on a thread of its own that only pumps messages, so it
//! always answers within Windows' hook timeout however busy the window's
//! thread is; it reads the shared reader and router and posts a message.

use std::sync::{Mutex, OnceLock, PoisonError};

use arcen_input::keyboard::pc::PcKey;
use arcen_input::keyboard::windows::WindowsKeyReader;
use arcen_input::keyboard::{HookRoute, HookRouter, KeyboardSwitchChord};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput,
    VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetForegroundWindow, GetMessageW, HC_ACTION, HHOOK, KBDLLHOOKSTRUCT, MSG,
    PM_NOREMOVE, PeekMessageW, PostMessageW, PostThreadMessageW, SetWindowsHookExW,
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
/// Developer builds only (`dev-tools`): a release Deck routes no injected
/// key, so no other program can type system shortcuts into a session.
#[cfg(feature = "dev-tools")]
pub const LAB_KEY_MARK: usize = 0x4152_4C42;

/// Whether an injected key carries the input lab's mark.
#[cfg(feature = "dev-tools")]
const fn lab_marked(extra: usize) -> bool {
    extra == LAB_KEY_MARK
}

/// Release builds route no injected key.
#[cfg(not(feature = "dev-tools"))]
const fn lab_marked(_extra: usize) -> bool {
    false
}
/// An unassigned Virtual-Key, the Windows Pier's "menu mask" key: typed
/// between a Windows key's press and release, it keeps Start shut.
const VK_MENU_MASK: u16 = 0xE8;

/// The session window the hook routes for, while one has focus.
struct HookState {
    /// The window's handle (`HWND` is not `Send`).
    hwnd: isize,
    grabbed: bool,
    router: HookRouter,
    reader: WindowsKeyReader,
    counts: HookCounts,
}

/// What the hook did for one focus of a session window, logged when the
/// window lets go of it (no key is named).
#[derive(Debug, Default, Clone, Copy)]
struct HookCounts {
    /// Every keyboard event the hook was called with.
    events: u64,
    /// Typed by another program (not the input lab): passed on.
    injected: u64,
    /// A press while another window was in front: passed on.
    elsewhere: u64,
    /// Taken for the session.
    taken: u64,
    /// Passed on to the system by the routing.
    passed: u64,
    /// Taken for the session, but blocked by a hook installed before this
    /// one: dropped, unless the release of a press the session has.
    blocked: u64,
}

/// The routing the hook thread reads, set by the session window's thread.
static STATE: Mutex<Option<HookState>> = Mutex::new(None);
/// The keyboard switch's chord (Settings ▸ Input ▸ Keyboard switch).
static SWITCH: Mutex<KeyboardSwitchChord> = Mutex::new(KeyboardSwitchChord::DEFAULT);
/// The hook thread's id, once it runs (`None` if it could not start).
static THREAD: OnceLock<Option<u32>> = OnceLock::new();
/// Posted to the hook thread: install the hook (`WM_HOOK_ON`) or remove
/// it (`WM_HOOK_OFF`).
const WM_HOOK_ON: u32 = WM_APP + 0x4A1;
const WM_HOOK_OFF: u32 = WM_APP + 0x4A2;

/// The hook lives on a thread of its own that does nothing but pump
/// messages, and is installed there only while a session window has focus.
/// Windows calls a low-level hook through its thread's message queue and
/// skips it when that thread does not answer within `LowLevelHooksTimeout`
/// (and, since Windows 7, may remove it silently); the window's thread is
/// busy drawing and waiting on the GPU, so a hook installed there may be
/// skipped once the stream runs (Alt+Tab and Print Screen then reach the
/// Deck's computer).
fn hook_thread() -> Option<u32> {
    *THREAD.get_or_init(|| {
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("arcen-keyboard-hook".to_owned())
            .spawn(move || {
                let mut msg = MSG::default();
                // SAFETY: a valid out-parameter; creates this thread's
                // message queue before its id is handed out.
                let _ = unsafe { PeekMessageW(&raw mut msg, None, 0, 0, PM_NOREMOVE) };
                // SAFETY: no arguments.
                let _ = ready_tx.send(unsafe { GetCurrentThreadId() });
                let mut hook: Option<HHOOK> = None;
                // SAFETY: a valid out-parameter; this thread's own queue.
                while unsafe { GetMessageW(&raw mut msg, None, 0, 0) }.0 > 0 {
                    match msg.message {
                        WM_HOOK_ON if hook.is_none() => {
                            // SAFETY: a hook procedure of this module, on
                            // this thread, which pumps messages while it is
                            // installed.
                            let installed = unsafe {
                                GetModuleHandleW(None).and_then(|module| {
                                    SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), module, 0)
                                })
                            };
                            match installed {
                                Ok(handle) => hook = Some(handle),
                                Err(error) => tracing::warn!(
                                    target: arcen_deck_core::log_target::INPUT,
                                    %error,
                                    "keyboard hook unavailable: system shortcuts stay on this PC"
                                ),
                            }
                        }
                        WM_HOOK_OFF => {
                            if let Some(handle) = hook.take() {
                                // SAFETY: the hook this thread installed.
                                let _ = unsafe { UnhookWindowsHookEx(handle) };
                            }
                        }
                        _ => {}
                    }
                }
            });
        spawned.ok()?;
        ready_rx.recv().ok()
    })
}

fn post_to_hook_thread(message: u32) {
    if let Some(thread) = hook_thread() {
        // SAFETY: a thread message without pointers; the thread's queue
        // exists before its id is known.
        let _ = unsafe { PostThreadMessageW(thread, message, WPARAM(0), LPARAM(0)) };
    }
}

fn state() -> std::sync::MutexGuard<'static, Option<HookState>> {
    STATE.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Installs the hook for `hwnd` (or updates its grab), with `held` the keys
/// the window already holds.
pub(super) fn install(hwnd: HWND, grabbed: bool, held: Vec<PcKey>) {
    let hwnd = hwnd.0 as isize;
    let mut slot = state();
    if let Some(state) = slot.as_mut() {
        if state.hwnd == hwnd && state.grabbed == grabbed {
            return;
        }
        state.hwnd = hwnd;
        state.grabbed = grabbed;
        state.router.begin(held);
        drop(slot);
    } else {
        let mut router = HookRouter::default();
        router.set_switch_chord(*SWITCH.lock().unwrap_or_else(PoisonError::into_inner));
        router.begin(held);
        *slot = Some(HookState {
            hwnd,
            grabbed,
            router,
            reader: WindowsKeyReader::default(),
            counts: HookCounts::default(),
        });
        drop(slot);
        post_to_hook_thread(WM_HOOK_ON);
    }
    tracing::info!(
        target: arcen_deck_core::log_target::INPUT,
        grabbed,
        "keyboard hook: routing for the session window, grab {}",
        if grabbed { "on" } else { "off" }
    );
}

/// The keyboard switch's chord the hook keeps for the Deck.
pub(super) fn set_switch_chord(switch: KeyboardSwitchChord) {
    *SWITCH.lock().unwrap_or_else(PoisonError::into_inner) = switch;
    if let Some(state) = state().as_mut() {
        state.router.set_switch_chord(switch);
    }
}

/// Removes the hook when `hwnd` is the window it routes for (that window
/// lost focus, or its session is over): every key goes to the system.
/// Another session window's call leaves the focused window's routing alone.
pub(super) fn uninstall(hwnd: HWND) {
    let hwnd = hwnd.0 as isize;
    let Some(state) = take_if_routing_for(&mut state(), hwnd) else {
        return;
    };
    post_to_hook_thread(WM_HOOK_OFF);
    let counts = state.counts;
    tracing::info!(
        target: arcen_deck_core::log_target::INPUT,
        events = counts.events,
        taken = counts.taken,
        passed = counts.passed,
        blocked = counts.blocked,
        elsewhere = counts.elsewhere,
        injected = counts.injected,
        "keyboard hook: routing stopped"
    );
}

/// The routing, taken out of `slot`, when it is `hwnd`'s.
fn take_if_routing_for(slot: &mut Option<HookState>, hwnd: isize) -> Option<HookState> {
    slot.take_if(|state| state.hwnd == hwnd)
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
        if let Some(taken) = take(event) {
            // The hooks installed before this one see the key first. One
            // of them may block the keyboard (a Windows Pier keeping its
            // desk private while this Deck runs in its session, a kiosk
            // lock): the Deck is never a way around it, so a press the
            // rest of the chain blocks goes nowhere. The shared router
            // still lets the release of a press the session has through.
            // SAFETY: passes the event on unchanged.
            let below = unsafe { CallNextHookEx(None, code, wparam, lparam) };
            if below.0 == 0 || blocked_below(&taken) {
                deliver(&taken);
            }
            return if below.0 == 0 { LRESULT(1) } else { below };
        }
    }
    // SAFETY: passes the event on unchanged.
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

/// A key the session takes, to post to its window.
struct Taken {
    hwnd: isize,
    key: PcKey,
    pressed: bool,
    route: HookRoute,
    packed: usize,
    scan: isize,
}

/// Whether the hook takes this event for the session window.
fn take(event: &KBDLLHOOKSTRUCT) -> Option<Taken> {
    let flags = event.flags.0;
    let mut slot = state();
    let state = slot.as_mut()?;
    state.counts.events += 1;
    if flags & LLKHF_INJECTED != 0 && !lab_marked(event.dwExtraInfo) {
        // The mask key, and anything another program types.
        state.counts.injected += 1;
        return None;
    }
    let pressed = flags & LLKHF_UP == 0;
    #[allow(clippy::cast_possible_truncation, reason = "Virtual-Keys are one byte")]
    let vk = event.vkCode as u16;
    let (scan, extended) =
        super::keyboard::key_position(vk, event.scanCode, flags & LLKHF_EXTENDED != 0);
    let key = state.reader.read(vk, scan, extended, pressed);
    // The hook is global: a press is the session's only while its window
    // is in front. Focus may already be elsewhere before the window
    // hears of it (and stops the routing); such a key is never taken.
    // SAFETY: no arguments; returns a handle or null.
    let foreground = unsafe { GetForegroundWindow() }.0 as isize;
    if pressed && foreground != state.hwnd {
        state.counts.elsewhere += 1;
        return None;
    }
    let route = state.router.route(key, pressed, state.grabbed);
    if route == HookRoute::System {
        state.counts.passed += 1;
        return None;
    }
    state.counts.taken += 1;
    let packed = usize::from(vk) | ((flags as usize & 0xFF) << 16);
    let packed_scan = (u64::from(event.time) << 32) | u64::from(event.scanCode);
    #[allow(
        clippy::cast_possible_truncation,
        reason = "x64 only: isize is 64 bits"
    )]
    let scan = packed_scan.cast_signed() as isize;
    Some(Taken {
        hwnd: state.hwnd,
        key,
        pressed,
        route,
        packed,
        scan,
    })
}

/// A taken key the rest of the hook chain blocked: whether the session
/// still gets it (the release of a press it has).
fn blocked_below(taken: &Taken) -> bool {
    let mut slot = state();
    let Some(state) = slot.as_mut().filter(|state| state.hwnd == taken.hwnd) else {
        return false;
    };
    state.counts.blocked += 1;
    state.router.blocked_below(taken.key, taken.pressed)
}

/// Posts a taken key to the session window.
fn deliver(taken: &Taken) {
    if taken.route == HookRoute::SessionMaskingWindowsKey {
        inject_mask_key();
    }
    let hwnd = HWND(taken.hwnd as *mut core::ffi::c_void);
    // SAFETY: posting to the session window; a failure loses the key,
    // which the window's focus loss then releases.
    let _ = unsafe { PostMessageW(hwnd, WM_HOOK_KEY, WPARAM(taken.packed), LPARAM(taken.scan)) };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn routing_for(hwnd: isize) -> HookState {
        HookState {
            hwnd,
            grabbed: true,
            router: HookRouter::default(),
            reader: WindowsKeyReader::default(),
            counts: HookCounts::default(),
        }
    }

    #[test]
    fn only_the_routed_window_stops_the_routing() {
        let mut slot = Some(routing_for(1));
        assert!(take_if_routing_for(&mut slot, 2).is_none());
        assert_eq!(slot.as_ref().map(|state| state.hwnd), Some(1));
        assert!(take_if_routing_for(&mut slot, 1).is_some());
        assert!(slot.is_none());
        assert!(take_if_routing_for(&mut slot, 1).is_none());
    }

    /// A Windows Pier keeping its desk private while this Deck runs in its
    /// session: its keyboard hook, installed first, blocks every key the
    /// desk types. The Deck's hook, newer and so called first, must not
    /// take such a key for its own session. Typed keys are stood in for by
    /// the input lab's marked keys (`dev-tools`), which the Deck routes as
    /// typed and a real Pier's hook blocks too. Run from a
    /// signed-in console: `arcen_deck_windows-*.exe --ignored --exact
    /// input::hook::tests::a_key_a_hook_below_blocks_is_not_taken`.
    #[cfg(feature = "dev-tools")]
    #[test]
    #[ignore = "needs an interactive session (foreground window, low-level hooks)"]
    #[allow(
        clippy::too_many_lines,
        reason = "one live scenario: the blocking hook, the window, the key"
    )]
    fn a_key_a_hook_below_blocks_is_not_taken() {
        use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
        use std::time::{Duration, Instant};

        use windows::Win32::UI::Input::KeyboardAndMouse::KEYEVENTF_EXTENDEDKEY;
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetForegroundWindow,
            PM_REMOVE, RegisterClassW, SetForegroundWindow, TranslateMessage, WINDOW_EX_STYLE,
            WM_QUIT, WNDCLASSW, WS_POPUP, WS_VISIBLE,
        };
        use windows::core::w;

        static BLOCKING: AtomicBool = AtomicBool::new(false);
        static BLOCKED: AtomicUsize = AtomicUsize::new(0);
        static PIER_THREAD: AtomicU32 = AtomicU32::new(0);

        unsafe extern "system" fn pier(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
            if code == i32::try_from(HC_ACTION).unwrap_or(0) {
                // SAFETY: for HC_ACTION, lParam points at the event.
                let event = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
                if event.dwExtraInfo == LAB_KEY_MARK && BLOCKING.load(Ordering::SeqCst) {
                    BLOCKED.fetch_add(1, Ordering::SeqCst);
                    return LRESULT(1);
                }
            }
            // SAFETY: passes the event on unchanged.
            unsafe { CallNextHookEx(None, code, wparam, lparam) }
        }

        unsafe extern "system" fn window_proc(
            hwnd: HWND,
            message: u32,
            wparam: WPARAM,
            lparam: LPARAM,
        ) -> LRESULT {
            // SAFETY: the default procedure.
            unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
        }

        fn send(vk: u16, flags: KEYBD_EVENT_FLAGS, extra: usize) {
            let input = INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: VIRTUAL_KEY(vk),
                        dwFlags: flags,
                        dwExtraInfo: extra,
                        ..KEYBDINPUT::default()
                    },
                },
            };
            // SAFETY: one well-formed keyboard input.
            let _ = unsafe {
                SendInput(
                    &[input],
                    i32::try_from(std::mem::size_of::<INPUT>()).unwrap_or(0),
                )
            };
        }

        /// Pumps the window's thread; how many keys the hook posted to it.
        fn pump(duration: Duration) -> usize {
            let mut posted = 0;
            let until = Instant::now() + duration;
            let mut msg = MSG::default();
            while Instant::now() < until {
                // SAFETY: this thread's queue; a valid out-parameter.
                while unsafe { PeekMessageW(&raw mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
                    if msg.message == WM_HOOK_KEY {
                        posted += 1;
                    }
                    // SAFETY: a message just removed from this thread's queue.
                    unsafe {
                        let _ = TranslateMessage(&raw const msg);
                        DispatchMessageW(&raw const msg);
                    }
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            posted
        }

        // The "Pier": a blocking hook installed before the Deck's.
        let pier_thread = std::thread::spawn(|| {
            let mut msg = MSG::default();
            // SAFETY: creates this thread's queue; a valid out-parameter.
            let _ = unsafe { PeekMessageW(&raw mut msg, None, 0, 0, PM_NOREMOVE) };
            // SAFETY: a hook procedure of this module on this pumping thread.
            let handle = unsafe {
                GetModuleHandleW(None)
                    .and_then(|module| SetWindowsHookExW(WH_KEYBOARD_LL, Some(pier), module, 0))
            };
            // SAFETY: no arguments.
            PIER_THREAD.store(unsafe { GetCurrentThreadId() }, Ordering::SeqCst);
            let Ok(handle) = handle else {
                return;
            };
            // SAFETY: this thread's own queue.
            while unsafe { GetMessageW(&raw mut msg, None, 0, 0) }.0 > 0 {}
            // SAFETY: the hook this thread installed.
            let _ = unsafe { UnhookWindowsHookEx(handle) };
        });
        while PIER_THREAD.load(Ordering::SeqCst) == 0 {
            std::thread::sleep(Duration::from_millis(5));
        }

        // SAFETY: a class and a popup window of this module, on this thread.
        let hwnd = unsafe {
            let instance = GetModuleHandleW(None).unwrap_or_default();
            let class = WNDCLASSW {
                lpfnWndProc: Some(window_proc),
                hInstance: instance.into(),
                lpszClassName: w!("ArcenHookChainTest"),
                ..WNDCLASSW::default()
            };
            let _ = RegisterClassW(&raw const class);
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("ArcenHookChainTest"),
                w!("Arcen hook chain test"),
                WS_POPUP | WS_VISIBLE,
                100,
                100,
                640,
                360,
                None,
                None,
                instance,
                None,
            )
        };
        let Ok(hwnd) = hwnd else {
            panic!("could not create the test window");
        };
        // Test set-up only: an Alt tap lets this process take the
        // foreground (nothing here reads raw keyboard input).
        send(0x12, KEYBD_EVENT_FLAGS::default(), 0);
        send(0x12, KEYEVENTF_KEYUP, 0);
        // SAFETY: this thread's window.
        let _ = unsafe { SetForegroundWindow(hwnd) };
        pump(Duration::from_millis(300));
        // SAFETY: no arguments.
        let in_front = unsafe { GetForegroundWindow() } == hwnd;

        install(hwnd, true, Vec::new());
        pump(Duration::from_millis(300));

        // Print Screen, which the grab takes for the session; the "Pier"
        // blocks it.
        BLOCKING.store(true, Ordering::SeqCst);
        send(0x2C, KEYEVENTF_EXTENDEDKEY, LAB_KEY_MARK);
        send(0x2C, KEYEVENTF_EXTENDEDKEY | KEYEVENTF_KEYUP, LAB_KEY_MARK);
        let posted = pump(Duration::from_millis(300));
        let counts = state().as_ref().map(|state| state.counts);
        let blocked = BLOCKED.load(Ordering::SeqCst);

        // The block begins while the session holds the key: its release
        // still reaches the session, so the host does not keep it down.
        BLOCKING.store(false, Ordering::SeqCst);
        send(0x2C, KEYEVENTF_EXTENDEDKEY, LAB_KEY_MARK);
        let held_press = pump(Duration::from_millis(300));
        BLOCKING.store(true, Ordering::SeqCst);
        send(0x2C, KEYEVENTF_EXTENDEDKEY | KEYEVENTF_KEYUP, LAB_KEY_MARK);
        let held_release = pump(Duration::from_millis(300));
        BLOCKING.store(false, Ordering::SeqCst);
        let late_counts = state().as_ref().map(|state| state.counts);

        uninstall(hwnd);
        // SAFETY: this thread's window; a thread message without pointers.
        unsafe {
            let _ = DestroyWindow(hwnd);
            let _ = PostThreadMessageW(
                PIER_THREAD.load(Ordering::SeqCst),
                WM_QUIT,
                WPARAM(0),
                LPARAM(0),
            );
        }
        let _ = pier_thread.join();

        assert!(in_front, "the test window did not come to the front");
        let counts = counts.unwrap_or_default();
        // The blocked press is taken and dropped; its release then goes
        // down the chain with it.
        assert_eq!(
            (counts.taken, counts.passed),
            (1, 1),
            "the grab did not take Print Screen: {counts:?}"
        );
        assert_eq!(
            blocked, 2,
            "the blocking hook did not see both edges: {counts:?}"
        );
        assert_eq!(
            posted, 0,
            "the Deck took keys a hook below it blocked: {counts:?}"
        );
        assert_eq!(counts.blocked, 1, "{counts:?}");
        let late_counts = late_counts.unwrap_or_default();
        assert_eq!(held_press, 1, "the unblocked press: {late_counts:?}");
        assert_eq!(
            held_release, 1,
            "the release of a press the session has was dropped: {late_counts:?}"
        );
        assert_eq!(late_counts.blocked, 2, "{late_counts:?}");
    }
}
