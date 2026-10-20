//! Cross-platform-safe wrapper around the macOS-only [`super::monitor`].
//!
//! `ui/app.rs` is otherwise OS-agnostic (its few genuinely macOS-only needs
//! — the native menu bar, the keypad monitor — are each pushed behind a
//! small wrapper with a no-op stub on other platforms, never a `cfg` block
//! sprinkled through the UI logic itself). This module gives the tablet
//! integration the same shape: [`TabletRuntime`] always exists as a type,
//! `install()` only ever succeeds on macOS (the only platform local typed
//! tablet capture exists for at all in this crate), and every other method
//! degrades to an inert, empty result elsewhere instead of requiring `cfg`
//! at every call site in `ui/app.rs`.
#![forbid(unsafe_code)]

use super::sample::{DispatchAgeSummary, TabletMonitorEvent};

/// Owns the platform tablet event monitor's RAII lifetime plus a handle to
/// drain its bounded sample queue. Constructing one is the only way to
/// receive samples; dropping it uninstalls the monitor.
pub struct TabletRuntime {
    // Never read directly: held purely for its RAII `Drop` lifetime, which
    // uninstalls the AppKit monitor when `TabletRuntime` drops.
    #[cfg(target_os = "macos")]
    #[allow(dead_code)]
    monitor: super::monitor::TabletEventMonitor,
    #[cfg(target_os = "macos")]
    sample_handle: super::monitor::TabletSampleHandle,
}

impl TabletRuntime {
    /// Install the platform tablet monitor. Must be called from the AppKit
    /// main thread (the same thread egui's `logic()` runs on). Returns
    /// `None` if this is not macOS, or if AppKit refuses to install the
    /// monitor (rare — e.g. no run loop yet), or if called off the main
    /// thread; callers should treat `None` as "no typed pen capture this
    /// run", not as an error to surface to the user (the tablet driver's mouse-emulation
    /// fallback remains fully functional either way).
    ///
    /// `sink` is offered every event on the main thread as AppKit dispatches
    /// it and returns whether it took it. `wake` runs after an event had to
    /// be queued instead, so the UI drains it promptly.
    #[must_use]
    #[cfg(target_os = "macos")]
    pub fn install(
        sink: impl Fn(TabletMonitorEvent) -> bool + 'static,
        wake: impl Fn() + 'static,
    ) -> Option<Self> {
        let mtm = objc2_foundation::MainThreadMarker::new()?;
        let (monitor, sample_handle) = super::monitor::TabletEventMonitor::start(mtm, sink, wake)?;
        Some(Self {
            monitor,
            sample_handle,
        })
    }

    #[must_use]
    #[cfg(not(target_os = "macos"))]
    pub fn install(
        _sink: impl Fn(TabletMonitorEvent) -> bool + 'static,
        _wake: impl Fn() + 'static,
    ) -> Option<Self> {
        None
    }

    /// Drain every event the sink could not take, oldest first.
    #[must_use]
    #[cfg(target_os = "macos")]
    pub fn drain(&self) -> Vec<TabletMonitorEvent> {
        self.sample_handle.drain()
    }

    #[must_use]
    #[cfg(not(target_os = "macos"))]
    pub fn drain(&self) -> Vec<TabletMonitorEvent> {
        Vec::new()
    }

    /// How long monitored input waited for AppKit dispatch since the last
    /// call.
    #[must_use]
    #[cfg(target_os = "macos")]
    pub fn take_dispatch_ages(&self) -> DispatchAgeSummary {
        self.sample_handle.take_dispatch_ages()
    }

    #[must_use]
    #[cfg(not(target_os = "macos"))]
    pub fn take_dispatch_ages(&self) -> DispatchAgeSummary {
        DispatchAgeSummary::default()
    }

    /// Samples dropped so far because the bounded producer-side queue was
    /// full and the consumer had not drained it in time (see
    /// [`super::sample::BoundedSampleQueue`]). This is the producer-side
    /// drop-oldest counter — distinct from, and reported alongside,
    /// `arcen_input::PenForwardingDispatcher::overflow_count`, which
    /// tracks the separate, edge-preserving consumer-side bound.
    #[must_use]
    #[cfg(target_os = "macos")]
    pub fn dropped_count(&self) -> u64 {
        self.sample_handle.dropped_count()
    }

    #[must_use]
    #[cfg(not(target_os = "macos"))]
    pub fn dropped_count(&self) -> u64 {
        0
    }
}

/// Holds AppKit mouse-event coalescing off while a session window is
/// active (see `monitor::MouseCoalescingGuard`); inert off macOS.
#[derive(Default)]
pub struct MouseCoalescing {
    #[cfg(target_os = "macos")]
    guard: Option<super::monitor::MouseCoalescingGuard>,
}

impl MouseCoalescing {
    /// Applies the wanted state. Returns whether the setting changed. Must
    /// run on the main thread; elsewhere it does nothing.
    pub fn set_session_active(&mut self, active: bool) -> bool {
        #[cfg(target_os = "macos")]
        {
            if self.guard.is_none() {
                let mtm = objc2_foundation::MainThreadMarker::new();
                self.guard = mtm.map(super::monitor::MouseCoalescingGuard::new);
            }
            self.guard
                .as_mut()
                .is_some_and(|guard| guard.set_session_active(active))
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = active;
            false
        }
    }

    /// Whether coalescing is currently held off.
    #[must_use]
    pub fn is_disabled(&self) -> bool {
        #[cfg(target_os = "macos")]
        {
            self.guard
                .as_ref()
                .is_some_and(super::monitor::MouseCoalescingGuard::is_disabled)
        }
        #[cfg(not(target_os = "macos"))]
        {
            false
        }
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn install_off_the_main_thread_reports_none() {
        // Test harnesses run off the AppKit main thread, so `install()`
        // deterministically returns `None` here rather than installing a
        // real monitor — mirrors `monitor::tests::constructing_off_the_main_thread_is_reported_as_none`.
        assert!(TabletRuntime::install(|_| true, || {}).is_none());
    }
}
