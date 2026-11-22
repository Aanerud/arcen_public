// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Keeps the Deck's UI passes running while Windows runs a modal loop of its
//! own on the window: a menu open, the menu bar in keyboard menu mode (a
//! menu closed with Esc, or Alt pressed), or the window moved or sized.
//!
//! winit runs no `about_to_wait` until such a loop ends, and only there does
//! the shell turn a due repaint into a UI pass; the launcher notices a
//! session's end, a failed resume or a Disconnect only in a pass. The video
//! keeps playing on its presenter threads meanwhile, so a session that ended
//! looked frozen on its last picture until the loop ended. Posted and
//! timer messages still reach the window during the loop (a redraw asked for
//! brings no `WM_PAINT` there), so a timer ticks the shell, which runs a due
//! pass itself.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{
    KillTimer, SetTimer, WM_ENTERMENULOOP, WM_ENTERSIZEMOVE, WM_EXITMENULOOP, WM_EXITSIZEMOVE,
    WM_TIMER,
};

const SUBCLASS_ID: usize = 0x4152_4d4c; // "ARML"
/// The tick's timer id on the window (its own, beside the subclass's).
const TIMER_ID: usize = SUBCLASS_ID;
/// How often the shell is ticked during a modal loop: a repaint asked for
/// later than "now" (a countdown, the reconnect overlay) is due by then.
const TICK_MS: u32 = 50;

struct State {
    active: AtomicBool,
    tick: Box<dyn Fn() + Send + Sync>,
}

/// The modal-loop watch on one window, removed when dropped.
pub(super) struct ModalLoop {
    hwnd: HWND,
    state: Arc<State>,
}

impl std::fmt::Debug for ModalLoop {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ModalLoop")
            .field("active", &self.active())
            .finish_non_exhaustive()
    }
}

impl ModalLoop {
    /// Watches `hwnd` (on its own thread): `tick` is called on the window's
    /// thread as a modal loop begins and then every [`TICK_MS`] until it
    /// ends.
    pub(super) fn install(
        hwnd: HWND,
        tick: impl Fn() + Send + Sync + 'static,
    ) -> Result<Self, String> {
        let state = Arc::new(State {
            active: AtomicBool::new(false),
            tick: Box::new(tick),
        });
        let data = Arc::into_raw(Arc::clone(&state)) as usize;
        // SAFETY: `modal_proc` reads `data` as the `State` it was made from;
        // the subclass is removed before that reference is released (drop).
        if !unsafe { SetWindowSubclass(hwnd, Some(modal_proc), SUBCLASS_ID, data) }.as_bool() {
            // SAFETY: the reference `into_raw` made just above, never shared.
            drop(unsafe { Arc::from_raw(data as *const State) });
            return Err("SetWindowSubclass(modal loop) failed".to_owned());
        }
        Ok(Self { hwnd, state })
    }

    /// Whether Windows is running a modal loop on the window now.
    pub(super) fn active(&self) -> bool {
        self.state.active.load(Ordering::Acquire)
    }
}

impl Drop for ModalLoop {
    fn drop(&mut self) {
        // SAFETY: the timer and subclass `install` and `modal_proc` set on
        // this window, on its thread.
        let removed = unsafe {
            let _ = KillTimer(self.hwnd, TIMER_ID);
            RemoveWindowSubclass(self.hwnd, Some(modal_proc), SUBCLASS_ID).as_bool()
        };
        if removed {
            // SAFETY: the subclass's reference from `install`; the
            // subclass is gone, so nothing reads it any more.
            drop(unsafe { Arc::from_raw(Arc::as_ptr(&self.state)) });
        }
    }
}

unsafe extern "system" fn modal_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _subclass: usize,
    data: usize,
) -> LRESULT {
    // SAFETY: `data` is the `State` `install` handed the subclass, alive
    // until the subclass is removed.
    let state = unsafe { &*(data as *const State) };
    match message {
        WM_ENTERMENULOOP | WM_ENTERSIZEMOVE => {
            state.active.store(true, Ordering::Release);
            // SAFETY: a timer on this thread's window; killed as the loop
            // ends or the watch is dropped.
            let _ = unsafe { SetTimer(hwnd, TIMER_ID, TICK_MS, None) };
            (state.tick)();
        }
        WM_EXITMENULOOP | WM_EXITSIZEMOVE => {
            state.active.store(false, Ordering::Release);
            // SAFETY: the timer set as the loop began.
            let _ = unsafe { KillTimer(hwnd, TIMER_ID) };
        }
        WM_TIMER if wparam.0 == TIMER_ID => {
            if state.active.load(Ordering::Acquire) {
                (state.tick)();
            }
            return LRESULT(0);
        }
        _ => {}
    }
    // SAFETY: forwarding the message unchanged down the subclass chain
    // (winit sees the loop's messages too).
    unsafe { DefSubclassProc(hwnd, message, wparam, lparam) }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, DispatchMessageW, MSG, PM_REMOVE, PeekMessageW,
        SendMessageW, TranslateMessage, WINDOW_EX_STYLE, WM_ENTERMENULOOP, WM_ENTERSIZEMOVE,
        WM_EXITMENULOOP, WM_EXITSIZEMOVE, WS_POPUP,
    };
    use windows::core::w;

    use super::ModalLoop;

    struct TestWindow(HWND);

    impl Drop for TestWindow {
        fn drop(&mut self) {
            // SAFETY: the window the test created, on this thread.
            let _ = unsafe { DestroyWindow(self.0) };
        }
    }

    fn hidden_window() -> TestWindow {
        // SAFETY: a plain hidden popup of the system's static class, on
        // this thread, destroyed by the guard.
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("STATIC"),
                w!("modal loop test"),
                WS_POPUP,
                0,
                0,
                200,
                100,
                None,
                None,
                None,
                None,
            )
        }
        .expect("a test window");
        TestWindow(hwnd)
    }

    /// Dispatches this thread's messages for `span`, as a modal loop does.
    fn pump(span: Duration) {
        let until = Instant::now() + span;
        while Instant::now() < until {
            let mut message = MSG::default();
            // SAFETY: a valid out-parameter; messages of this thread.
            while unsafe { PeekMessageW(&raw mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
                // SAFETY: the message just removed from this thread's queue.
                unsafe {
                    let _ = TranslateMessage(&raw const message);
                    DispatchMessageW(&raw const message);
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn send(window: &TestWindow, message: u32) {
        // SAFETY: a notification to the test window on its thread.
        let _ = unsafe { SendMessageW(window.0, message, WPARAM(0), LPARAM(0)) };
    }

    #[test]
    fn a_menu_or_move_loop_ticks_the_shell_until_it_ends() {
        let window = hidden_window();
        let ticks = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&ticks);
        let watch = ModalLoop::install(window.0, move || {
            counted.fetch_add(1, Ordering::SeqCst);
        })
        .expect("watch");
        pump(Duration::from_millis(200));
        assert_eq!(ticks.load(Ordering::SeqCst), 0, "no loop, no ticks");
        assert!(!watch.active());

        for (enter, exit) in [
            (WM_ENTERMENULOOP, WM_EXITMENULOOP),
            (WM_ENTERSIZEMOVE, WM_EXITSIZEMOVE),
        ] {
            ticks.store(0, Ordering::SeqCst);
            send(&window, enter);
            assert!(watch.active());
            // At once, as the loop begins: a pass already due runs.
            assert_eq!(ticks.load(Ordering::SeqCst), 1);
            pump(Duration::from_millis(400));
            let during = ticks.load(Ordering::SeqCst);
            assert!(during >= 4, "ticks during the loop: {during}");

            send(&window, exit);
            assert!(!watch.active());
            let ended = ticks.load(Ordering::SeqCst);
            pump(Duration::from_millis(200));
            assert_eq!(
                ticks.load(Ordering::SeqCst),
                ended,
                "no ticks after the loop"
            );
        }

        // Dropped mid-loop: the timer and subclass go with it.
        send(&window, WM_ENTERMENULOOP);
        drop(watch);
        let left = ticks.load(Ordering::SeqCst);
        pump(Duration::from_millis(200));
        assert_eq!(ticks.load(Ordering::SeqCst), left);
        assert_eq!(
            Arc::strong_count(&ticks),
            1,
            "the tick closure was released"
        );
    }
}
