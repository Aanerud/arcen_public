// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The macOS Deck's event tap for the system shortcuts (⌘Tab, ⌘Space,
//! Mission Control): a thin adapter around the shared
//! [`arcen_input::keyboard::MacTapRouter`].
//!
//! macOS acts on these chords before any window sees them. An active
//! `CGEventTap` at the session level sees them first; it needs the
//! Accessibility permission, which the Deck never asks for by itself (only
//! Settings ▸ Input's button does), and it is used only while Settings says
//! so (off by default).
//!
//! - The tap exists only while it is wanted: the switch is on, the host has
//!   the keyboard (`KeyboardCapture`), the policy takes the system shortcuts
//!   and a session window has the keyboard focus. It runs on a thread of its
//!   own, so a busy UI pass never makes the system disable it; if the
//!   system does (`kCGEventTapDisabledByTimeout`), it is re-enabled at once.
//! - It reads only key-down and key-up events, and only their key code,
//!   autorepeat flag and modifier flags. A chord the shared router gives the
//!   host is queued for the Deck's own keyboard path (as if the session
//!   window had received it) and removed from the system's stream; every
//!   other event goes on untouched.
//! - Any doubt passes the event on: Secure Event Input on (a password field
//!   anywhere), an event aimed at another process, a busy lock, a panic.
//!   A key's release goes where its press went, so nothing is left half
//!   pressed on either side.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use arcen_input::keyboard::MacTapModifiers;

/// One key event the tap took for the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TappedKey {
    /// The Mac virtual key code.
    pub key_code: u16,
    /// Down or up.
    pub pressed: bool,
    /// An autorepeat of a held key.
    pub repeat: bool,
    /// `CGEventFlags`.
    pub flags: u64,
}

/// What the tap is doing, for the indicator and the logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TapState {
    /// Not wanted now (the switch is off, no session, this Mac has the
    /// keyboard, or no session window has focus).
    Off,
    /// Wanted, but the Deck does not hold the Accessibility permission.
    NeedsPermission,
    /// Wanted, but the system refused to create it.
    Failed,
    /// Taking the system shortcuts for the host.
    Active,
    /// Installed, but Secure Event Input is on: every key stays on the Mac.
    SecureInput,
}

impl TapState {
    /// Whether the system shortcuts reach the host now.
    pub const fn takes_shortcuts(self) -> bool {
        matches!(self, Self::Active)
    }
}

/// The egui key a tapped key code stands for (only the keys the shared
/// router can take).
fn egui_key(key_code: u16) -> Option<egui::Key> {
    use arcen_input::keyboard as k;
    Some(match key_code {
        k::MAC_KEY_TAB => egui::Key::Tab,
        k::MAC_KEY_SPACE => egui::Key::Space,
        k::MAC_KEY_GRAVE => egui::Key::Backtick,
        k::MAC_KEY_LEFT => egui::Key::ArrowLeft,
        k::MAC_KEY_RIGHT => egui::Key::ArrowRight,
        k::MAC_KEY_UP => egui::Key::ArrowUp,
        k::MAC_KEY_DOWN => egui::Key::ArrowDown,
        k::MAC_KEY_3 => egui::Key::Num3,
        k::MAC_KEY_4 => egui::Key::Num4,
        k::MAC_KEY_5 => egui::Key::Num5,
        k::MAC_KEY_D => egui::Key::D,
        _ => return None,
    })
}

/// The egui key event a tapped key becomes, so it takes the Deck's usual
/// keyboard path (the shared shortcut policy, ⌘Tab as the host's app
/// switcher, held-key tracking).
pub fn tapped_key_event(key: TappedKey) -> Option<egui::Event> {
    let egui_key = egui_key(key.key_code)?;
    let held = MacTapModifiers::from_cg_flags(key.flags);
    let modifiers = egui::Modifiers {
        alt: held.option,
        ctrl: held.control,
        shift: held.shift,
        mac_cmd: held.command,
        command: held.command,
    };
    Some(egui::Event::Key {
        key: egui_key,
        physical_key: Some(egui_key),
        pressed: key.pressed,
        repeat: key.repeat,
        modifiers,
    })
}

/// The native facts for a tapped key's event, as AppKit's key monitor would
/// have recorded them: its virtual key code, so ⌘\` on an ISO keyboard is
/// the key beside left Shift, as without the tap.
pub fn tapped_key_native(
    key: TappedKey,
    event: &egui::Event,
) -> Option<crate::ui::keyboard::NativeKeyMetadata> {
    let egui::Event::Key { key: egui_key, .. } = event else {
        return None;
    };
    Some(crate::ui::keyboard::NativeKeyMetadata {
        key: *egui_key,
        qt_key: 0,
        pressed: key.pressed,
        keypad: false,
        key_code: key.key_code,
    })
}

/// What the UI thread and the tap thread share.
struct Shared {
    /// The tap may take chords (the UI wants it; the tap thread also
    /// checks Secure Event Input and the event's target).
    capturing: AtomicBool,
    /// The tap thread saw Secure Event Input on at its last event.
    secure_input: AtomicBool,
    /// Times the system disabled the tap and it was re-enabled.
    rearmed: AtomicU64,
    /// Key events taken for the host, in order.
    queue: Mutex<Vec<TappedKey>>,
    /// Wakes the UI after a key was queued.
    wake: Box<dyn Fn() + Send + Sync>,
}

/// The tap's owner on the UI thread.
pub struct SystemShortcutTap {
    shared: Arc<Shared>,
    worker: Option<platform::Worker>,
    state: TapState,
    /// When a missing permission was last checked (rechecked at most every
    /// two seconds while the tap is wanted).
    permission_checked: Option<Instant>,
    reported_rearms: u64,
}

impl SystemShortcutTap {
    /// No tap yet; `wake` repaints the UI after a key was taken.
    pub fn new(wake: impl Fn() + Send + Sync + 'static) -> Self {
        Self {
            shared: Arc::new(Shared {
                capturing: AtomicBool::new(false),
                secure_input: AtomicBool::new(false),
                rearmed: AtomicU64::new(0),
                queue: Mutex::new(Vec::new()),
                wake: Box::new(wake),
            }),
            worker: None,
            state: TapState::Off,
            permission_checked: None,
            reported_rearms: 0,
        }
    }

    /// What the tap is doing now.
    pub const fn state(&self) -> TapState {
        self.state
    }

    /// Installs the tap while `wanted`, removes it otherwise. Called once
    /// per UI pass; cheap when nothing changes.
    pub fn update(&mut self, wanted: bool) -> TapState {
        if !wanted {
            self.stop();
            self.state = TapState::Off;
            self.permission_checked = None;
            return self.state;
        }
        if self.worker.is_none() {
            let due = self
                .permission_checked
                .is_none_or(|at| at.elapsed() >= Duration::from_secs(2));
            if !due {
                return self.state;
            }
            self.permission_checked = Some(Instant::now());
            if !platform::accessibility_allowed() {
                self.state = TapState::NeedsPermission;
                return self.state;
            }
            self.shared.capturing.store(true, Ordering::SeqCst);
            match platform::Worker::start(Arc::clone(&self.shared)) {
                Some(worker) => {
                    tracing::info!(
                        target: crate::logging::target::INPUT,
                        "system shortcut tap installed"
                    );
                    self.worker = Some(worker);
                }
                None => {
                    self.shared.capturing.store(false, Ordering::SeqCst);
                    if self.state != TapState::Failed {
                        tracing::warn!(
                            target: crate::logging::target::INPUT,
                            "system shortcut tap refused by the system; system shortcuts stay on this Mac"
                        );
                    }
                    self.state = TapState::Failed;
                    return self.state;
                }
            }
        }
        self.shared.capturing.store(true, Ordering::SeqCst);
        let rearms = self.shared.rearmed.load(Ordering::SeqCst);
        if rearms != self.reported_rearms {
            tracing::info!(
                target: crate::logging::target::INPUT,
                rearms,
                "system shortcut tap re-enabled after the system disabled it"
            );
            self.reported_rearms = rearms;
        }
        let secure =
            platform::secure_input_enabled() || self.shared.secure_input.load(Ordering::SeqCst);
        self.state = if secure {
            TapState::SecureInput
        } else {
            TapState::Active
        };
        self.state
    }

    /// The keys taken for the host since the last call, in order.
    pub fn drain(&self) -> Vec<TappedKey> {
        self.shared
            .queue
            .lock()
            .map(|mut queue| std::mem::take(&mut *queue))
            .unwrap_or_default()
    }

    /// Queues a key as the tap would have.
    #[cfg(test)]
    pub fn queue_for_test(&self, key: TappedKey) {
        self.shared.queue.lock().expect("queue").push(key);
    }

    /// Removes the tap (the queue is dropped: the Deck releases every host
    /// key itself when capture ends).
    fn stop(&mut self) {
        self.shared.capturing.store(false, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            worker.stop();
            tracing::info!(
                target: crate::logging::target::INPUT,
                "system shortcut tap removed"
            );
        }
        let _ = self.drain();
        self.shared.secure_input.store(false, Ordering::SeqCst);
    }
}

impl Drop for SystemShortcutTap {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Whether the Deck holds the Accessibility permission the tap needs.
pub fn accessibility_allowed() -> bool {
    platform::accessibility_allowed()
}

/// Asks the system for the Accessibility permission (its own prompt, which
/// leads to System Settings). Only from an explicit click in Settings.
pub fn request_accessibility() {
    platform::request_accessibility();
}

#[cfg(target_os = "macos")]
mod platform {
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicPtr, Ordering};
    use std::sync::{Arc, Mutex};

    use arcen_input::keyboard::{MacTapEvent, MacTapModifiers, MacTapRouter, TapRoute};

    use super::{Shared, TappedKey};

    type CFTypeRef = *mut c_void;
    type CGEventRef = *mut c_void;
    type CGEventTapCallBack =
        unsafe extern "C" fn(*mut c_void, u32, CGEventRef, *mut c_void) -> CGEventRef;

    const SESSION_EVENT_TAP: u32 = 1;
    const HEAD_INSERT_EVENT_TAP: u32 = 0;
    const TAP_OPTION_DEFAULT: u32 = 0;
    const EVENT_KEY_DOWN: u32 = 10;
    const EVENT_KEY_UP: u32 = 11;
    const EVENT_TAP_DISABLED_BY_TIMEOUT: u32 = 0xFFFF_FFFE;
    const EVENT_TAP_DISABLED_BY_USER_INPUT: u32 = 0xFFFF_FFFF;
    const FIELD_AUTOREPEAT: u32 = 8;
    const FIELD_KEYCODE: u32 = 9;
    const FIELD_TARGET_PID: u32 = 40;

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGEventTapCreate(
            tap: u32,
            place: u32,
            options: u32,
            events_of_interest: u64,
            callback: CGEventTapCallBack,
            user_info: *mut c_void,
        ) -> CFTypeRef;
        fn CGEventTapEnable(tap: CFTypeRef, enable: bool);
        fn CGEventGetIntegerValueField(event: CGEventRef, field: u32) -> i64;
        fn CGEventGetFlags(event: CGEventRef) -> u64;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        static kCFRunLoopCommonModes: CFTypeRef;
        static kCFRunLoopDefaultMode: CFTypeRef;
        fn CFMachPortCreateRunLoopSource(
            allocator: CFTypeRef,
            port: CFTypeRef,
            order: isize,
        ) -> CFTypeRef;
        fn CFMachPortInvalidate(port: CFTypeRef);
        fn CFRunLoopGetCurrent() -> CFTypeRef;
        fn CFRunLoopAddSource(run_loop: CFTypeRef, source: CFTypeRef, mode: CFTypeRef);
        fn CFRunLoopRemoveSource(run_loop: CFTypeRef, source: CFTypeRef, mode: CFTypeRef);
        fn CFRunLoopRunInMode(mode: CFTypeRef, seconds: f64, return_after_source: u8) -> i32;
        fn CFRunLoopStop(run_loop: CFTypeRef);
        fn CFRetain(cf: *const c_void) -> *const c_void;
        fn CFRelease(cf: *const c_void);
    }

    #[link(name = "Carbon", kind = "framework")]
    unsafe extern "C" {
        fn IsSecureEventInputEnabled() -> u8;
    }

    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        fn AXIsProcessTrusted() -> u8;
        fn AXIsProcessTrustedWithOptions(options: CFTypeRef) -> u8;
    }

    pub(super) fn accessibility_allowed() -> bool {
        // SAFETY: a plain query of this process's trust.
        unsafe { AXIsProcessTrusted() != 0 }
    }

    pub(super) fn secure_input_enabled() -> bool {
        // SAFETY: a plain query of the session's state.
        unsafe { IsSecureEventInputEnabled() != 0 }
    }

    pub(super) fn request_accessibility() {
        use objc2::runtime::AnyObject;
        use objc2_foundation::{NSDictionary, NSNumber, NSString};
        let key = NSString::from_str("AXTrustedCheckOptionPrompt");
        let yes = NSNumber::new_bool(true);
        let options: objc2::rc::Retained<NSDictionary<NSString, AnyObject>> =
            NSDictionary::from_slices(&[&*key], &[yes.as_ref() as &AnyObject]);
        // SAFETY: an NSDictionary is toll-free bridged to CFDictionary and
        // lives across the call.
        let trusted = unsafe {
            AXIsProcessTrustedWithOptions(objc2::rc::Retained::as_ptr(&options) as CFTypeRef)
        };
        tracing::info!(
            target: crate::logging::target::INPUT,
            trusted = trusted != 0,
            "accessibility permission requested for the system shortcut tap"
        );
    }

    /// The tap thread's context: the shared state, the router and the tap
    /// port (to re-enable it). Lives on the tap thread's stack for as long
    /// as the run loop runs.
    struct TapContext {
        shared: Arc<Shared>,
        router: MacTapRouter,
        port: AtomicPtr<c_void>,
        pid: i64,
    }

    /// A run loop published to the UI thread to stop it; whoever takes it
    /// out of the mutex owns its retain (as in `hid::session`).
    struct RunLoopHandle(CFTypeRef);
    // SAFETY: the pointer is only ever passed to `CFRunLoopStop`, which is
    // documented as callable from any thread, and to `CFRelease`.
    unsafe impl Send for RunLoopHandle {}

    pub(super) struct Worker {
        run_loop: Arc<Mutex<Option<RunLoopHandle>>>,
        stop: Arc<std::sync::atomic::AtomicBool>,
        thread: std::thread::JoinHandle<()>,
    }

    impl Worker {
        /// Starts the tap thread; `None` when the system refused the tap.
        pub(super) fn start(shared: Arc<Shared>) -> Option<Self> {
            let run_loop: Arc<Mutex<Option<RunLoopHandle>>> = Arc::new(Mutex::new(None));
            let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<bool>(1);
            let thread = {
                let run_loop = Arc::clone(&run_loop);
                let stop = Arc::clone(&stop);
                std::thread::Builder::new()
                    .name("arcen-shortcut-tap".into())
                    .spawn(move || run_tap(shared, &run_loop, &stop, &ready_tx))
                    .ok()?
            };
            let created = ready_rx.recv().unwrap_or(false);
            let worker = Self {
                run_loop,
                stop,
                thread,
            };
            if created {
                Some(worker)
            } else {
                worker.stop();
                None
            }
        }

        /// Stops the run loop, removes the tap and joins the thread.
        pub(super) fn stop(self) {
            self.stop.store(true, Ordering::SeqCst);
            let taken = match self.run_loop.lock() {
                Ok(mut guard) => guard.take(),
                Err(poisoned) => poisoned.into_inner().take(),
            };
            if let Some(handle) = taken {
                // SAFETY: the retain published by the tap thread is ours
                // now; the run loop is alive until we release it.
                unsafe {
                    CFRunLoopStop(handle.0);
                    CFRelease(handle.0);
                }
            }
            let _ = self.thread.join();
        }
    }

    fn run_tap(
        shared: Arc<Shared>,
        published: &Mutex<Option<RunLoopHandle>>,
        stop: &std::sync::atomic::AtomicBool,
        ready: &std::sync::mpsc::SyncSender<bool>,
    ) {
        let mut context = Box::new(TapContext {
            shared,
            router: MacTapRouter::new(),
            port: AtomicPtr::new(std::ptr::null_mut()),
            pid: i64::from(std::process::id()),
        });
        let mask = (1u64 << EVENT_KEY_DOWN) | (1u64 << EVENT_KEY_UP);
        let user_info = std::ptr::from_mut::<TapContext>(&mut context).cast::<c_void>();
        // SAFETY: the callback and its context outlive the tap: the port is
        // invalidated and released below, before `context` is dropped.
        let port = unsafe {
            CGEventTapCreate(
                SESSION_EVENT_TAP,
                HEAD_INSERT_EVENT_TAP,
                TAP_OPTION_DEFAULT,
                mask,
                tap_callback,
                user_info,
            )
        };
        if port.is_null() {
            let _ = ready.send(false);
            return;
        }
        context.port.store(port, Ordering::SeqCst);
        // SAFETY: a valid port; the source and run loop are used on this
        // thread only, apart from the published `CFRunLoopStop`.
        unsafe {
            let source = CFMachPortCreateRunLoopSource(std::ptr::null_mut(), port, 0);
            if source.is_null() {
                CFMachPortInvalidate(port);
                CFRelease(port);
                let _ = ready.send(false);
                return;
            }
            let run_loop = CFRunLoopGetCurrent();
            CFRunLoopAddSource(run_loop, source, kCFRunLoopCommonModes);
            CGEventTapEnable(port, true);
            let _ = ready.send(true);
            let run = {
                let mut guard = match published.lock() {
                    Ok(guard) => guard,
                    Err(poisoned) => poisoned.into_inner(),
                };
                if stop.load(Ordering::SeqCst) {
                    false
                } else {
                    *guard = Some(RunLoopHandle(CFRetain(run_loop).cast_mut()));
                    true
                }
            };
            // In slices, so a stop that lands before the loop starts (which
            // `CFRunLoopStop` would miss) is still seen within one slice.
            while run && !stop.load(Ordering::SeqCst) {
                CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.25, 0);
            }
            CGEventTapEnable(port, false);
            CFRunLoopRemoveSource(run_loop, source, kCFRunLoopCommonModes);
            CFMachPortInvalidate(port);
            CFRelease(source);
            CFRelease(port);
            // Whoever still holds the published handle releases it: here if
            // the UI thread never took it.
            let leftover = match published.lock() {
                Ok(mut guard) => guard.take(),
                Err(poisoned) => poisoned.into_inner().take(),
            };
            if let Some(handle) = leftover {
                CFRelease(handle.0);
            }
        }
        context.port.store(std::ptr::null_mut(), Ordering::SeqCst);
        drop(context);
    }

    unsafe extern "C" fn tap_callback(
        _proxy: *mut c_void,
        event_type: u32,
        event: CGEventRef,
        user_info: *mut c_void,
    ) -> CGEventRef {
        if user_info.is_null() || event.is_null() {
            return event;
        }
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // SAFETY: `user_info` is the `TapContext` that `run_tap` keeps
            // alive, and only this thread's run loop calls back.
            let context = unsafe { &mut *user_info.cast::<TapContext>() };
            // SAFETY: `event` is the event the system handed this callback.
            unsafe { route(context, event_type, event) }
        }));
        match outcome {
            Ok(true) => std::ptr::null_mut(),
            _ => event,
        }
    }

    /// `true`: the event was taken for the host and leaves the stream.
    unsafe fn route(context: &mut TapContext, event_type: u32, event: CGEventRef) -> bool {
        let shared = &context.shared;
        match event_type {
            EVENT_TAP_DISABLED_BY_TIMEOUT | EVENT_TAP_DISABLED_BY_USER_INPUT => {
                let port = context.port.load(Ordering::SeqCst);
                if !port.is_null() && shared.capturing.load(Ordering::SeqCst) {
                    // SAFETY: the port is valid while the run loop runs.
                    unsafe { CGEventTapEnable(port, true) };
                    shared.rearmed.fetch_add(1, Ordering::SeqCst);
                }
                return false;
            }
            EVENT_KEY_DOWN | EVENT_KEY_UP => {}
            _ => return false,
        }
        // SAFETY: plain reads of the event's own fields.
        let (key_code, repeat, flags, target) = unsafe {
            (
                CGEventGetIntegerValueField(event, FIELD_KEYCODE),
                CGEventGetIntegerValueField(event, FIELD_AUTOREPEAT) != 0,
                CGEventGetFlags(event),
                CGEventGetIntegerValueField(event, FIELD_TARGET_PID),
            )
        };
        let Ok(key_code) = u16::try_from(key_code) else {
            return false;
        };
        let secure = secure_input_enabled();
        shared.secure_input.store(secure, Ordering::SeqCst);
        // An event the system aims at another process is never taken (0:
        // not known; the UI's focus decided the tap may take chords).
        let ours = target == 0 || target == context.pid;
        let capturing = shared.capturing.load(Ordering::SeqCst) && !secure && ours;
        let tap_event = MacTapEvent {
            key_code,
            pressed: event_type == EVENT_KEY_DOWN,
            repeat,
            modifiers: MacTapModifiers::from_cg_flags(flags),
        };
        if context.router.route(capturing, tap_event) != TapRoute::Host {
            return false;
        }
        let Ok(mut queue) = shared.queue.lock() else {
            return false;
        };
        queue.push(TappedKey {
            key_code,
            pressed: tap_event.pressed,
            repeat,
            flags,
        });
        drop(queue);
        (shared.wake)();
        true
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    use std::sync::Arc;

    use super::Shared;

    pub(super) struct Worker;

    impl Worker {
        pub(super) fn start(_shared: Arc<Shared>) -> Option<Self> {
            None
        }

        pub(super) fn stop(self) {}
    }

    pub(super) const fn accessibility_allowed() -> bool {
        false
    }

    pub(super) const fn secure_input_enabled() -> bool {
        false
    }

    pub(super) fn request_accessibility() {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tapped_chord_becomes_the_key_event_the_window_would_have_had() {
        let event = tapped_key_event(TappedKey {
            key_code: arcen_input::keyboard::MAC_KEY_TAB,
            pressed: true,
            repeat: false,
            flags: MacTapModifiers::CG_COMMAND | MacTapModifiers::CG_SHIFT,
        })
        .expect("Tab is a tapped key");
        let egui::Event::Key {
            key,
            physical_key,
            pressed,
            repeat,
            modifiers,
        } = event
        else {
            panic!("a key event");
        };
        assert_eq!(key, egui::Key::Tab);
        assert_eq!(physical_key, Some(egui::Key::Tab));
        assert!(pressed && !repeat);
        assert!(modifiers.mac_cmd && modifiers.command && modifiers.shift);
        assert!(!modifiers.ctrl && !modifiers.alt);
        let arrow = tapped_key_event(TappedKey {
            key_code: arcen_input::keyboard::MAC_KEY_UP,
            pressed: false,
            repeat: false,
            flags: MacTapModifiers::CG_CONTROL | 0x0080_0000,
        });
        assert!(matches!(
            arrow,
            Some(egui::Event::Key {
                key: egui::Key::ArrowUp,
                pressed: false,
                modifiers: egui::Modifiers {
                    ctrl: true,
                    mac_cmd: false,
                    ..
                },
                ..
            })
        ));
        // A key the router never takes has no event.
        assert_eq!(
            tapped_key_event(TappedKey {
                key_code: arcen_input::keyboard::MAC_KEY_Q,
                pressed: true,
                repeat: false,
                flags: 0,
            }),
            None
        );
    }

    /// Live: with the Accessibility permission (a process tree that holds
    /// it), the tap installs, reports Active and is removed and joined
    /// cleanly, many times over. Posts no event. Run by hand:
    /// `cargo test -p arcen-deck-macos --lib -- --ignored live_tap`.
    #[test]
    #[ignore = "installs a real event tap; needs the Accessibility permission"]
    fn live_tap_installs_and_removes_cleanly() {
        assert!(
            accessibility_allowed(),
            "this process holds no Accessibility"
        );
        let mut tap = SystemShortcutTap::new(|| {});
        for round in 0..20 {
            let started = Instant::now();
            let state = tap.update(true);
            assert!(
                matches!(state, TapState::Active | TapState::SecureInput),
                "round {round}: {state:?}"
            );
            assert!(tap.worker.is_some());
            std::thread::sleep(Duration::from_millis(20));
            assert_eq!(tap.update(false), TapState::Off);
            assert!(tap.worker.is_none());
            assert!(
                started.elapsed() < Duration::from_millis(600),
                "round {round}: {:?}",
                started.elapsed()
            );
        }
        assert!(tap.drain().is_empty(), "no key was taken");
    }

    #[test]
    fn the_tap_is_off_until_wanted_and_never_installed_without_permission() {
        let mut tap = SystemShortcutTap::new(|| {});
        assert_eq!(tap.update(false), TapState::Off);
        assert!(!tap.state().takes_shortcuts());
        assert!(tap.drain().is_empty());
        if !accessibility_allowed() {
            assert_eq!(tap.update(true), TapState::NeedsPermission);
            assert!(tap.worker.is_none());
        }
        assert_eq!(tap.update(false), TapState::Off);
        assert!(tap.worker.is_none());
    }
}
