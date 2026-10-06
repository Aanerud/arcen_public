//! AppKit gesture capture for Deck.

use std::collections::VecDeque;
use std::ptr::NonNull;
use std::sync::{Arc, Mutex};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_app_kit::{NSEvent, NSEventMask, NSEventPhase, NSEventType};
use objc2_foundation::MainThreadMarker;

use crate::protocol::messages::{ScrollPhaseMsg, SwipeDirectionMsg};

const CAPACITY: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GestureSample {
    Magnify {
        scale_delta: f64,
        phase: ScrollPhaseMsg,
    },
    Rotate {
        degrees_delta: f64,
        phase: ScrollPhaseMsg,
    },
    SmartZoom,
    Swipe {
        direction: SwipeDirectionMsg,
        fingers: u8,
        phase: ScrollPhaseMsg,
    },
}

#[derive(Default)]
struct GestureQueue {
    samples: VecDeque<GestureSample>,
}

impl GestureQueue {
    fn push(&mut self, sample: GestureSample) {
        if self.samples.len() == CAPACITY {
            self.samples.pop_front();
        }
        self.samples.push_back(sample);
    }

    fn drain(&mut self) -> Vec<GestureSample> {
        self.samples.drain(..).collect()
    }
}

struct Shared {
    queue: Mutex<GestureQueue>,
}

pub struct GestureRuntime {
    monitor: Retained<AnyObject>,
    shared: Arc<Shared>,
    _mtm: MainThreadMarker,
}

impl GestureRuntime {
    /// `wake` runs on the main thread after each queued sample, so the UI
    /// drains it promptly instead of on its next idle poll.
    #[must_use]
    pub fn install(wake: impl Fn() + 'static) -> Option<Self> {
        let mtm = MainThreadMarker::new()?;
        let shared = Arc::new(Shared {
            queue: Mutex::new(GestureQueue::default()),
        });
        let callback_shared = Arc::clone(&shared);
        let handler = RcBlock::new(move |event: NonNull<NSEvent>| -> *mut NSEvent {
            // SAFETY: AppKit invokes a local event monitor synchronously with
            // a live NSEvent that remains valid only for this callback. The
            // pointer is borrowed for scalar getters and returned unchanged.
            let event_ref = unsafe { event.as_ref() };
            if let Some(sample) = decode(event_ref) {
                let mut queue = callback_shared
                    .queue
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                queue.push(sample);
                drop(queue);
                wake();
            }
            event.as_ptr()
        });
        // SAFETY: the block captures only `Arc<Shared>` and returns the
        // original live event pointer unchanged, as required by AppKit.
        let monitor = unsafe {
            NSEvent::addLocalMonitorForEventsMatchingMask_handler(
                NSEventMask::Magnify
                    | NSEventMask::Rotate
                    | NSEventMask::Swipe
                    | NSEventMask::SmartMagnify,
                &handler,
            )
        }?;
        Some(Self {
            monitor,
            shared,
            _mtm: mtm,
        })
    }

    #[must_use]
    pub fn drain(&self) -> Vec<GestureSample> {
        self.shared
            .queue
            .lock()
            .map(|mut queue| queue.drain())
            .unwrap_or_default()
    }
}

impl Drop for GestureRuntime {
    fn drop(&mut self) {
        // SAFETY: `monitor` is the opaque token returned by AppKit for this
        // guard and has not previously been removed.
        unsafe {
            NSEvent::removeMonitor(&self.monitor);
        }
    }
}

fn decode(event: &NSEvent) -> Option<GestureSample> {
    let phase = event_phase(event);
    match event.r#type() {
        NSEventType::Magnify => Some(GestureSample::Magnify {
            scale_delta: event.magnification(),
            phase,
        }),
        NSEventType::Rotate => Some(GestureSample::Rotate {
            degrees_delta: f64::from(event.rotation()),
            phase,
        }),
        NSEventType::SmartMagnify => Some(GestureSample::SmartZoom),
        NSEventType::Swipe => {
            let dx = event.deltaX();
            let dy = event.deltaY();
            let direction = if dx.abs() >= dy.abs() {
                if dx.is_sign_negative() {
                    SwipeDirectionMsg::Left
                } else {
                    SwipeDirectionMsg::Right
                }
            } else if dy.is_sign_negative() {
                SwipeDirectionMsg::Down
            } else {
                SwipeDirectionMsg::Up
            };
            Some(GestureSample::Swipe {
                direction,
                fingers: 3,
                phase,
            })
        }
        _ => None,
    }
}

fn event_phase(event: &NSEvent) -> ScrollPhaseMsg {
    let phase = event.phase();
    if phase.contains(NSEventPhase::Began) {
        ScrollPhaseMsg::Began
    } else if phase.contains(NSEventPhase::Ended) {
        ScrollPhaseMsg::Ended
    } else if phase.contains(NSEventPhase::Cancelled) {
        ScrollPhaseMsg::Cancelled
    } else {
        ScrollPhaseMsg::Changed
    }
}
