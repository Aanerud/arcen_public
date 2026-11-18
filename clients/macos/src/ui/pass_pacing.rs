// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The root window's UI passes, paced to the root display's refresh at the
//! event loop (the shared `arcen_deck_core::ui_pacing::PassPacer`).
//!
//! egui asks for two more passes after every pass that carried input. In a
//! Match My Layout session the root layer presents without display sync, so
//! nothing capped them: a mouse moving over the root window at 55 events a
//! second ran 165 root passes a second. eframe offers no hook between a
//! repaint request and the pass, so [`PacedRootPasses`] sits between winit
//! and eframe: a root `RedrawRequested` that comes sooner than one refresh
//! after the previous root pass began is held, and runs when it is due,
//! carrying whatever input arrived meanwhile. Input events themselves reach
//! egui at once. A pass the media worker asks for ([`MediaWake`]) is never
//! held: in Match My Layout the root pass hands out every window's decoded
//! picture. Secondary windows pace themselves and pass straight through.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use arcen_deck_core::ui_pacing::PassPacer;
use eframe::UserEvent;
use winit::application::ApplicationHandler;
use winit::event::{DeviceEvent, DeviceId, StartCause, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoopProxy};
use winit::window::WindowId;

/// The root display's refresh period in nanoseconds, 0 while unknown.
static ROOT_REFRESH_PERIOD_NS: AtomicU64 = AtomicU64::new(0);

/// The media worker asked for a pass since the last root pass began: a
/// decoded picture (root's or a secondary window's, both handed out by the
/// root pass in Match My Layout) or a session event is waiting.
static MEDIA_WAKE: AtomicBool = AtomicBool::new(false);

/// Root's close was asked for (its close button or the Deck's Quit).
static ROOT_CLOSE_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Wakes the loop for [`MediaWake`]. egui tells eframe about a repaint
/// only when it is sooner than the one already pending, and a held root
/// pass is one, so the media worker wakes the loop itself.
static MEDIA_PROXY: OnceLock<Mutex<EventLoopProxy<UserEvent>>> = OnceLock::new();

/// The pass number that marks [`MediaWake`]'s own event. eframe would drop
/// it as outdated; [`PacedRootPasses`] takes it before eframe sees it.
const MEDIA_WAKE_PASS_NR: u64 = u64::MAX;

pub(crate) fn install_media_proxy(proxy: EventLoopProxy<UserEvent>) {
    let _ = MEDIA_PROXY.set(Mutex::new(proxy));
}

/// The media worker's repaint requester: a root pass it asks for runs at
/// once, so no picture waits for the pacer or is replaced before it is
/// shown. It asks for root's pass whichever viewport egui is drawing.
pub(crate) struct MediaWake(pub(crate) egui::Context);

impl arcen_deck_core::present::RepaintRequester for MediaWake {
    fn request_repaint(&self) {
        MEDIA_WAKE.store(true, Ordering::Release);
        self.0.request_repaint_of(egui::ViewportId::ROOT);
        if let Some(proxy) = MEDIA_PROXY.get() {
            if let Ok(proxy) = proxy.lock() {
                let _ = proxy.send_event(UserEvent::RequestRepaint {
                    viewport_id: egui::ViewportId::ROOT,
                    when: Instant::now(),
                    cumulative_pass_nr: MEDIA_WAKE_PASS_NR,
                });
            }
        }
    }
}

fn is_media_wake(event: &UserEvent) -> bool {
    matches!(
        event,
        UserEvent::RequestRepaint {
            cumulative_pass_nr: MEDIA_WAKE_PASS_NR,
            ..
        }
    )
}

/// Notes that root's close was asked for, so the loop's end is the Deck's
/// own and not eframe giving up.
pub(crate) fn note_root_close_requested() {
    ROOT_CLOSE_REQUESTED.store(true, Ordering::Relaxed);
}

/// Whether root's close was asked for since [`forget_root_close`].
pub(crate) fn root_close_requested() -> bool {
    ROOT_CLOSE_REQUESTED.load(Ordering::Relaxed)
}

pub(crate) fn forget_root_close() {
    ROOT_CLOSE_REQUESTED.store(false, Ordering::Relaxed);
}

/// Publishes the refresh period of the display under the root window
/// (`ArcenApp::keep_root_display_sync` reads it).
pub(crate) fn publish_root_refresh_period(period: Option<Duration>) {
    let nanos = period.map_or(0, |period| {
        u64::try_from(period.as_nanos()).unwrap_or(u64::MAX)
    });
    ROOT_REFRESH_PERIOD_NS.store(nanos, Ordering::Relaxed);
}

fn root_refresh_period() -> Option<Duration> {
    let nanos = ROOT_REFRESH_PERIOD_NS.load(Ordering::Relaxed);
    (nanos != 0).then(|| Duration::from_nanos(nanos))
}

/// eframe's application with root passes paced.
pub(crate) struct PacedRootPasses<A> {
    inner: A,
    /// eframe opens the root window first; secondary viewports open later.
    root: Option<WindowId>,
    pacer: PassPacer,
    /// A pass forwarded during the redraw phase asked for an immediate
    /// redraw, which winit dispatches only on the next loop iteration.
    inner_wants_poll: bool,
}

impl<A> PacedRootPasses<A> {
    pub(crate) fn new(inner: A) -> Self {
        Self {
            inner,
            root: None,
            pacer: PassPacer::default(),
            inner_wants_poll: false,
        }
    }
}

impl<A: ApplicationHandler<UserEvent>> PacedRootPasses<A> {
    fn forward_redraw(&mut self, event_loop: &ActiveEventLoop, window_id: WindowId) {
        self.inner
            .window_event(event_loop, window_id, WindowEvent::RedrawRequested);
        if event_loop.control_flow() == ControlFlow::Poll {
            self.inner_wants_poll = true;
        }
    }

    /// A root pass now for the media worker, unless one already took its
    /// wake.
    fn run_media_pass(&mut self, event_loop: &ActiveEventLoop) {
        let Some(root) = self.root else {
            return;
        };
        if !MEDIA_WAKE.swap(false, Ordering::AcqRel) {
            return;
        }
        self.pacer.release();
        if self.pacer.admit(Instant::now(), root_refresh_period()) {
            self.forward_redraw(event_loop, root);
        }
    }

    fn run_held_root_pass(&mut self, event_loop: &ActiveEventLoop) {
        let Some(root) = self.root else {
            return;
        };
        let period = root_refresh_period();
        let now = Instant::now();
        if self.pacer.held_due(now, period) && self.pacer.admit(now, period) {
            self.forward_redraw(event_loop, root);
        }
    }
}

/// The loop's control flow with a held root pass: wake when it is due. eframe
/// sets `Poll` only while it has a redraw it just asked for, and winit has
/// dispatched every such redraw before `about_to_wait`, except one asked for
/// by a pass in the redraw phase (`inner_wants_poll`).
fn paced_control_flow(
    current: ControlFlow,
    held_until: Option<Instant>,
    inner_wants_poll: bool,
) -> ControlFlow {
    let Some(until) = held_until else {
        return current;
    };
    match current {
        ControlFlow::Poll if inner_wants_poll => ControlFlow::Poll,
        ControlFlow::Poll | ControlFlow::Wait => ControlFlow::WaitUntil(until),
        ControlFlow::WaitUntil(at) => ControlFlow::WaitUntil(at.min(until)),
    }
}

impl<A: ApplicationHandler<UserEvent>> ApplicationHandler<UserEvent> for PacedRootPasses<A> {
    fn new_events(&mut self, event_loop: &ActiveEventLoop, cause: StartCause) {
        self.inner_wants_poll = false;
        self.inner.new_events(event_loop, cause);
        self.run_held_root_pass(event_loop);
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        self.inner.resumed(event_loop);
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        if is_media_wake(&event) {
            self.run_media_pass(event_loop);
        } else {
            self.inner.user_event(event_loop, event);
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        let root = *self.root.get_or_insert(window_id);
        match event {
            WindowEvent::RedrawRequested if window_id == root => {
                if MEDIA_WAKE.swap(false, Ordering::AcqRel) {
                    self.pacer.release();
                }
                if self.pacer.admit(Instant::now(), root_refresh_period()) {
                    self.forward_redraw(event_loop, window_id);
                }
            }
            WindowEvent::RedrawRequested => self.forward_redraw(event_loop, window_id),
            event => {
                if window_id == root
                    && matches!(
                        event,
                        WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. }
                    )
                {
                    self.pacer.release();
                }
                self.inner.window_event(event_loop, window_id, event);
            }
        }
    }

    fn device_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        device_id: DeviceId,
        event: DeviceEvent,
    ) {
        self.inner.device_event(event_loop, device_id, event);
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        self.inner.about_to_wait(event_loop);
        event_loop.set_control_flow(paced_control_flow(
            event_loop.control_flow(),
            self.pacer.held_until(root_refresh_period()),
            self.inner_wants_poll,
        ));
    }

    fn suspended(&mut self, event_loop: &ActiveEventLoop) {
        self.inner.suspended(event_loop);
    }

    fn exiting(&mut self, event_loop: &ActiveEventLoop) {
        self.inner.exiting(event_loop);
    }

    fn memory_warning(&mut self, event_loop: &ActiveEventLoop) {
        self.inner.memory_warning(event_loop);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_held_root_pass_wakes_the_loop_when_it_is_due() {
        let now = Instant::now();
        let due = now + Duration::from_millis(8);
        // eframe asked for the redraw this pacer held: no busy loop.
        assert_eq!(
            paced_control_flow(ControlFlow::Poll, Some(due), false),
            ControlFlow::WaitUntil(due)
        );
        assert_eq!(
            paced_control_flow(ControlFlow::Wait, Some(due), false),
            ControlFlow::WaitUntil(due)
        );
        // An earlier wake (another window's repaint) stays.
        let sooner = now + Duration::from_millis(2);
        assert_eq!(
            paced_control_flow(ControlFlow::WaitUntil(sooner), Some(due), false),
            ControlFlow::WaitUntil(sooner)
        );
        assert_eq!(
            paced_control_flow(
                ControlFlow::WaitUntil(due + due.duration_since(now)),
                Some(due),
                false
            ),
            ControlFlow::WaitUntil(due)
        );
        // A redraw a pass asked for during the redraw phase still runs.
        assert_eq!(
            paced_control_flow(ControlFlow::Poll, Some(due), true),
            ControlFlow::Poll
        );
    }

    #[test]
    fn only_the_media_wake_is_taken_from_eframes_events() {
        let event = |cumulative_pass_nr| UserEvent::RequestRepaint {
            viewport_id: egui::ViewportId::ROOT,
            when: Instant::now(),
            cumulative_pass_nr,
        };
        assert!(is_media_wake(&event(MEDIA_WAKE_PASS_NR)));
        assert!(!is_media_wake(&event(0)));
        assert!(!is_media_wake(&event(41)));
    }

    #[test]
    fn nothing_held_leaves_the_loop_as_eframe_set_it() {
        for flow in [
            ControlFlow::Poll,
            ControlFlow::Wait,
            ControlFlow::WaitUntil(Instant::now()),
        ] {
            assert_eq!(paced_control_flow(flow, None, false), flow);
        }
    }
}
