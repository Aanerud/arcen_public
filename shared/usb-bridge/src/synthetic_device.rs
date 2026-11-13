// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! [`SyntheticDevice`]: a [`CapturedUsbDevice`] backed by an in-memory
//! [`EmulatedUsbFunction`] — the independently authored
//! [`SyntheticTabletDevice`] by default, or an emulated real tablet such as
//! [`crate::WacomIntuos5`] — for exercising the whole exporter engine on any
//! operating system without hardware (ADR 0019 §9, S1).
//!
//! It answers control requests from the function, holds interrupt IN
//! transfers until a report for that endpoint is pushed through its
//! [`SyntheticHandle`], and records every operation that reaches it, so a test
//! can assert both what the peer saw and what the device saw.

use crate::{
    AlternateSetting, BackendRefusal, BackendStatus, CapturedDeviceFacts, CapturedUsbDevice,
    ControlOperation, ControlResponse, DescriptorError, EndpointAddress, InterfaceNumber,
    PenSample, PendingCompletion, ReleaseOutcome, ReleaseReason, SetupPacket,
    SyntheticTabletDevice, UrbId,
};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

/// The synthetic tablet's only endpoint: interrupt IN 1.
pub const SYNTHETIC_INTERRUPT_IN: EndpointAddress = EndpointAddress(0x81);
/// Reports kept while nobody is polling; older ones are dropped.
pub const MAX_QUEUED_REPORTS: usize = 64;
/// How often [`SyntheticHandle::deliver_when_polled`] looks for a poll.
const POLL_WAIT: Duration = Duration::from_millis(1);

/// A USB function emulated in memory: its descriptors, its answers on the
/// default control pipe, and the interrupt IN endpoints reports arrive on.
///
/// Transport-agnostic by construction: the descriptors, control answers and
/// reports are what a real device would produce, so the same function can be
/// presented through a virtual host controller or, at HID level, through a
/// host's `uhid`.
pub trait EmulatedUsbFunction: Send + 'static {
    /// What a backend that captured the device would read from it.
    ///
    /// # Errors
    ///
    /// Returns the parser's error if the function's configuration descriptor
    /// is invalid.
    fn captured_facts(&self) -> Result<CapturedDeviceFacts, DescriptorError>;

    /// Answers one transfer on the default control pipe. `data` is the OUT
    /// data stage (empty for an IN request).
    fn control(&mut self, setup: SetupPacket, data: &[u8]) -> ControlResponse;

    /// The endpoint [`SyntheticHandle::push_report`] delivers to.
    fn primary_interrupt_in(&self) -> EndpointAddress;

    /// Whether `endpoint` is one of the function's interrupt IN endpoints.
    fn has_interrupt_in(&self, endpoint: EndpointAddress) -> bool;

    /// Whether `SET_INTERFACE` to this interface and alternate setting exists.
    fn has_alternate(&self, interface: InterfaceNumber, alternate: AlternateSetting) -> bool;
}

impl EmulatedUsbFunction for SyntheticTabletDevice {
    fn captured_facts(&self) -> Result<CapturedDeviceFacts, DescriptorError> {
        Self::captured_facts()
    }

    fn control(&mut self, setup: SetupPacket, _data: &[u8]) -> ControlResponse {
        self.handle_control(setup)
    }

    fn primary_interrupt_in(&self) -> EndpointAddress {
        SYNTHETIC_INTERRUPT_IN
    }

    fn has_interrupt_in(&self, endpoint: EndpointAddress) -> bool {
        endpoint == SYNTHETIC_INTERRUPT_IN
    }

    fn has_alternate(&self, interface: InterfaceNumber, alternate: AlternateSetting) -> bool {
        interface.0 == 0 && alternate.0 == 0
    }
}

/// One operation that reached the synthetic device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyntheticOp {
    Control(SetupPacket),
    SelectAlternateSetting {
        interface: InterfaceNumber,
        alternate: AlternateSetting,
    },
    ClearHalt(EndpointAddress),
    InterruptIn {
        endpoint: EndpointAddress,
        length: usize,
    },
    InterruptOut {
        endpoint: EndpointAddress,
        length: usize,
    },
    Cancel(UrbId),
    Unplug,
    Release(ReleaseReason),
    /// Dropped without [`CapturedUsbDevice::release`]; the drop released it.
    ReleasedByDrop,
}

#[derive(Debug)]
struct Held {
    endpoint: EndpointAddress,
    completion: PendingCompletion,
    length: usize,
}

#[derive(Debug)]
struct State<F> {
    function: F,
    held: VecDeque<Held>,
    reports: VecDeque<(EndpointAddress, Vec<u8>)>,
    ops: Vec<SyntheticOp>,
    releases: usize,
    unplugged: bool,
}

impl<F> State<F> {
    fn new(function: F) -> Self {
        Self {
            function,
            held: VecDeque::new(),
            reports: VecDeque::new(),
            ops: Vec::new(),
            releases: 0,
            unplugged: false,
        }
    }

    fn drain_held(&mut self) -> Vec<PendingCompletion> {
        self.held.drain(..).map(|held| held.completion).collect()
    }
}

fn lock<F>(state: &Mutex<State<F>>) -> MutexGuard<'_, State<F>> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A captured emulated USB device; the synthetic lab tablet by default.
#[derive(Debug)]
pub struct SyntheticDevice<F = SyntheticTabletDevice> {
    facts: CapturedDeviceFacts,
    state: Arc<Mutex<State<F>>>,
    released: bool,
}

/// The test's side of a [`SyntheticDevice`]: push reports, unplug, observe.
#[derive(Debug)]
pub struct SyntheticHandle<F = SyntheticTabletDevice> {
    state: Arc<Mutex<State<F>>>,
}

impl<F> Clone for SyntheticHandle<F> {
    fn clone(&self) -> Self {
        Self {
            state: Arc::clone(&self.state),
        }
    }
}

impl SyntheticDevice {
    /// A freshly captured synthetic tablet and the handle that drives it.
    ///
    /// # Errors
    ///
    /// Returns the parser's error if the fixture's descriptors were invalid.
    pub fn capture() -> Result<(Self, SyntheticHandle), DescriptorError> {
        Self::emulate(SyntheticTabletDevice::default())
    }
}

impl<F: EmulatedUsbFunction> SyntheticDevice<F> {
    /// A freshly captured emulated device and the handle that drives it.
    ///
    /// # Errors
    ///
    /// Returns the parser's error if the function's descriptors are invalid.
    pub fn emulate(function: F) -> Result<(Self, SyntheticHandle<F>), DescriptorError> {
        let facts = function.captured_facts()?;
        let state = Arc::new(Mutex::new(State::new(function)));
        Ok((
            Self {
                facts,
                state: Arc::clone(&state),
                released: false,
            },
            SyntheticHandle { state },
        ))
    }
}

impl SyntheticHandle {
    /// Sends one pen sample as an interrupt report.
    pub fn push_sample(&self, sample: PenSample) {
        self.push_report(sample.encode_report().to_vec());
    }

    /// The synthetic tablet's current `SET_IDLE` and protocol state, as the
    /// device sees it.
    #[must_use]
    pub fn tablet(&self) -> SyntheticTabletDevice {
        lock(&self.state).function.clone()
    }
}

impl<F: EmulatedUsbFunction> SyntheticHandle<F> {
    /// Sends one raw interrupt report on the function's primary interrupt IN
    /// endpoint (see [`Self::push_report_to`]).
    pub fn push_report(&self, report: Vec<u8>) {
        let endpoint = lock(&self.state).function.primary_interrupt_in();
        self.push_report_to(endpoint, report);
    }

    /// Sends one raw interrupt report on `endpoint`: it completes the oldest
    /// interrupt IN transfer held on that endpoint, or waits for the next one.
    pub fn push_report_to(&self, endpoint: EndpointAddress, report: Vec<u8>) {
        let ready = {
            let mut state = lock(&self.state);
            if state.unplugged {
                return;
            }
            if let Some(index) = state.held.iter().position(|held| held.endpoint == endpoint) {
                state.held.remove(index).map(|held| (held, report))
            } else {
                if state.reports.len() >= MAX_QUEUED_REPORTS {
                    state.reports.pop_front();
                }
                state.reports.push_back((endpoint, report));
                None
            }
        };
        if let Some((held, mut report)) = ready {
            report.truncate(held.length);
            held.completion.complete(BackendStatus::Completed, report);
        }
    }

    /// Waits until the host polls `endpoint`, then answers that poll with
    /// `report` — the pace a host that keeps one interrupt IN transfer in
    /// flight (Linux `usbhid`) can take without a report being dropped.
    ///
    /// Returns `false` when no poll arrived within `timeout`, or the device
    /// was unplugged or released.
    #[must_use]
    pub fn deliver_when_polled(
        &self,
        endpoint: EndpointAddress,
        report: Vec<u8>,
        timeout: Duration,
    ) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let held = {
                let mut state = lock(&self.state);
                if state.unplugged || state.releases > 0 {
                    return false;
                }
                state
                    .held
                    .iter()
                    .position(|held| held.endpoint == endpoint)
                    .and_then(|index| state.held.remove(index))
            };
            if let Some(held) = held {
                let mut report = report;
                report.truncate(held.length);
                held.completion.complete(BackendStatus::Completed, report);
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(POLL_WAIT);
        }
    }

    /// Interrupt IN transfers currently held on `endpoint`.
    #[must_use]
    pub fn held_on(&self, endpoint: EndpointAddress) -> usize {
        lock(&self.state)
            .held
            .iter()
            .filter(|held| held.endpoint == endpoint)
            .count()
    }

    /// Reads the emulated function's state.
    pub fn inspect<R>(&self, read: impl FnOnce(&F) -> R) -> R {
        read(&lock(&self.state).function)
    }
}

impl<F> SyntheticHandle<F> {
    /// Simulates pulling the cable: every held transfer and every later one
    /// completes as disconnected.
    pub fn unplug(&self) {
        let held: Vec<_> = {
            let mut state = lock(&self.state);
            state.unplugged = true;
            state.ops.push(SyntheticOp::Unplug);
            state.drain_held()
        };
        for completion in held {
            completion.complete(BackendStatus::Disconnected, Vec::new());
        }
    }

    /// Every operation that reached the device, in order.
    #[must_use]
    pub fn ops(&self) -> Vec<SyntheticOp> {
        lock(&self.state).ops.clone()
    }

    /// How many times the device was released (by `release` or by drop).
    #[must_use]
    pub fn releases(&self) -> usize {
        lock(&self.state).releases
    }

    /// Interrupt IN transfers currently held waiting for a report.
    #[must_use]
    pub fn held(&self) -> usize {
        lock(&self.state).held.len()
    }
}

impl<F> SyntheticDevice<F> {
    fn begin(&self, op: SyntheticOp) -> Option<MutexGuard<'_, State<F>>> {
        let mut state = lock(&self.state);
        state.ops.push(op);
        if state.unplugged { None } else { Some(state) }
    }
}

impl<F: EmulatedUsbFunction> CapturedUsbDevice for SyntheticDevice<F> {
    fn facts(&self) -> &CapturedDeviceFacts {
        &self.facts
    }

    fn submit_control(
        &self,
        operation: ControlOperation,
        _timeout: Duration,
        completion: PendingCompletion,
    ) {
        let op = match &operation {
            ControlOperation::Transfer { setup, .. } => SyntheticOp::Control(*setup),
            ControlOperation::SelectAlternateSetting {
                interface,
                alternate,
            } => SyntheticOp::SelectAlternateSetting {
                interface: *interface,
                alternate: *alternate,
            },
            ControlOperation::ClearHalt { endpoint } => SyntheticOp::ClearHalt(*endpoint),
        };
        let Some(mut state) = self.begin(op) else {
            completion.refuse(BackendRefusal::Disconnected);
            return;
        };
        let (status, data) = match operation {
            ControlOperation::Transfer {
                setup,
                data,
                in_length,
            } => match state.function.control(setup, &data) {
                ControlResponse::Ack => (BackendStatus::Completed, Vec::new()),
                ControlResponse::Data(mut data) => {
                    data.truncate(in_length);
                    (BackendStatus::Completed, data)
                }
                ControlResponse::Stall => (BackendStatus::Stall, Vec::new()),
            },
            ControlOperation::SelectAlternateSetting {
                interface,
                alternate,
            } if state.function.has_alternate(interface, alternate) => {
                (BackendStatus::Completed, Vec::new())
            }
            ControlOperation::ClearHalt { endpoint }
                if state.function.has_interrupt_in(endpoint) =>
            {
                (BackendStatus::Completed, Vec::new())
            }
            ControlOperation::SelectAlternateSetting { .. }
            | ControlOperation::ClearHalt { .. } => (BackendStatus::Stall, Vec::new()),
        };
        drop(state);
        completion.complete(status, data);
    }

    fn submit_interrupt_in(
        &self,
        endpoint: EndpointAddress,
        length: usize,
        completion: PendingCompletion,
    ) {
        let Some(mut state) = self.begin(SyntheticOp::InterruptIn { endpoint, length }) else {
            completion.refuse(BackendRefusal::Disconnected);
            return;
        };
        if !state.function.has_interrupt_in(endpoint) {
            drop(state);
            completion.refuse(BackendRefusal::UnknownTarget);
            return;
        }
        let queued = state
            .reports
            .iter()
            .position(|(queued, _)| *queued == endpoint)
            .and_then(|index| state.reports.remove(index));
        match queued {
            Some((_, mut report)) => {
                drop(state);
                report.truncate(length);
                completion.complete(BackendStatus::Completed, report);
            }
            None => state.held.push_back(Held {
                endpoint,
                completion,
                length,
            }),
        }
    }

    fn submit_interrupt_out(
        &self,
        endpoint: EndpointAddress,
        data: Vec<u8>,
        _timeout: Duration,
        completion: PendingCompletion,
    ) {
        let op = SyntheticOp::InterruptOut {
            endpoint,
            length: data.len(),
        };
        let refusal = if self.begin(op).is_some() {
            BackendRefusal::UnknownTarget
        } else {
            BackendRefusal::Disconnected
        };
        completion.refuse(refusal);
    }

    fn cancel(&self, urb: UrbId) {
        let cancelled = {
            let mut state = lock(&self.state);
            state.ops.push(SyntheticOp::Cancel(urb));
            state
                .held
                .iter()
                .position(|held| held.completion.urb() == urb)
                .and_then(|index| state.held.remove(index))
        };
        if let Some(held) = cancelled {
            held.completion
                .complete(BackendStatus::Cancelled, Vec::new());
        }
    }

    fn release(mut self: Box<Self>, reason: ReleaseReason) -> ReleaseOutcome {
        self.released = true;
        let held: Vec<_> = {
            let mut state = lock(&self.state);
            state.ops.push(SyntheticOp::Release(reason));
            state.releases += 1;
            state.drain_held()
        };
        for completion in held {
            completion.complete(BackendStatus::Disconnected, Vec::new());
        }
        ReleaseOutcome::Restored
    }
}

impl<F> Drop for SyntheticDevice<F> {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        let held: Vec<_> = {
            let mut state = lock(&self.state);
            state.ops.push(SyntheticOp::ReleasedByDrop);
            state.releases += 1;
            state.drain_held()
        };
        for completion in held {
            completion.complete(BackendStatus::Disconnected, Vec::new());
        }
    }
}

#[cfg(test)]
#[path = "synthetic_device_tests.rs"]
mod tests;
