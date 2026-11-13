// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The USB host side of a bridged HID tablet, for a host that publishes HID
//! devices rather than a virtual USB controller (ADR 0022).
//!
//! On Linux the host kernel enumerates a bridged device on `vhci-hcd` and its
//! own driver binds it. A macOS Pier has no virtual host controller it may
//! use, but it may publish HID devices (`IOHIDUserDevice`). [`HidHostAdapter`]
//! is the small USB host driver in between: it enumerates the bridged device
//! over the ordinary URB bridge, plans one HID device per HID interface, keeps
//! one interrupt IN transfer outstanding per interface and hands every
//! completed one over verbatim as an input report, and turns the host driver's
//! get/set report calls into HID class control transfers.
//!
//! It is sans-I/O: it produces [`HidHostUrb`] requests and consumes
//! completions; the platform turns them into wire frames and devices. Every
//! descriptor the device serves passes [`BridgedDeviceGuard`] before it is
//! used, so the device must keep matching what was authorized
//! ([`crate::authorize_hard_usb_tablet`]).

use crate::{
    AttachmentGeneration, BridgedDeviceGuard, BridgedDeviceViolation, EndpointAddress,
    HidPublishPolicy, HidReportKind, InterfaceReportDescriptor, MAX_CONFIGURATION_DESCRIPTOR_BYTES,
    MAX_INTERFACES, MAX_TRANSFER_BYTES, PublishRefusal, SetupPacket, TransferKind, UrbId,
    UrbStatus, UsbDeviceId, declared_report_lengths, hid_report_descriptor_request,
    parse_configuration_descriptor, parse_report_descriptor,
};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt::{Display, Formatter};
use std::num::NonZeroU32;
use std::time::{Duration, Instant};

/// Wire timeout of every control transfer (the bridge's maximum).
pub const HID_HOST_CONTROL_TIMEOUT_MS: u32 = 1_000;
/// How long the adapter waits for any one control transfer before it gives
/// up on it: the Deck answers within the wire timeout, so a longer silence
/// means the request or its answer was lost.
pub const HID_HOST_CONTROL_DEADLINE: Duration = Duration::from_secs(3);
/// How long the whole enumeration may take.
pub const HID_HOST_ENUMERATION_DEADLINE: Duration = Duration::from_secs(10);
/// Most report requests waiting for the control pipe.
pub const MAX_QUEUED_REPORT_REQUESTS: usize = 8;
/// Consecutive failed interrupt transfers on one interface before the
/// attachment is given up.
pub const MAX_INTERRUPT_ERRORS: u32 = 8;
/// Longest string property, in characters.
pub const MAX_PROPERTY_CHARS: usize = 126;
/// Fallback product name when the device has none.
pub const DEFAULT_PRODUCT_NAME: &str = "Arcen bridged tablet";
/// How long an interrupt IN transfer may stay outstanding before it is
/// cancelled and submitted afresh. A held transfer is normal (an idle tablet
/// reports nothing); a transfer whose submit or completion was lost is not,
/// and without this the interface would stay silent for the attachment.
pub const INTERRUPT_IDLE_RESUBMIT: Duration = Duration::from_secs(10);
/// High half of every [`BridgedHidDevice::location_id`].
pub const LOCATION_ID_BASE: u32 = 0xA2C0_0000;

const REQUEST_GET_DESCRIPTOR: u8 = 0x06;
const REQUEST_SET_CONFIGURATION: u8 = 0x09;
const HID_GET_REPORT: u8 = 0x01;
const HID_SET_IDLE: u8 = 0x0A;
const HID_SET_REPORT: u8 = 0x09;
const DESCRIPTOR_DEVICE: u8 = 0x01;
const DESCRIPTOR_CONFIGURATION: u8 = 0x02;
const DESCRIPTOR_STRING: u8 = 0x03;
const DESCRIPTOR_INTERFACE: u8 = 0x04;
const DESCRIPTOR_HID: u8 = 0x21;
const LANGUAGE_EN_US: u16 = 0x0409;
const MAX_STRING_DESCRIPTOR: u16 = 255;
const DEVICE_DESCRIPTOR_LENGTH: u16 = 18;
const CONFIGURATION_HEADER_LENGTH: u16 = 9;
const MAX_TOMBSTONES: usize = 32;
const CONTROL_IN: EndpointAddress = EndpointAddress(0x80);
const CONTROL_OUT: EndpointAddress = EndpointAddress(0x00);

/// One URB the adapter wants submitted to the bridged device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HidHostUrb {
    pub urb_id: UrbId,
    pub endpoint: EndpointAddress,
    pub transfer_kind: TransferKind,
    pub timeout_ms: u32,
    pub declared_length: u32,
    pub setup: Option<SetupPacket>,
    /// OUT data; empty for IN transfers.
    pub payload: Vec<u8>,
}

/// One HID device to publish, for one HID interface of the bridged device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgedHidDevice {
    /// Position in the plan; the handle the platform reports back with.
    pub index: u8,
    /// USB interface number.
    pub interface: u8,
    /// The interface's report descriptor, exactly as authorized and served.
    pub report_descriptor: Vec<u8>,
    pub vendor_id: u16,
    pub product_id: u16,
    /// `bcdDevice`.
    pub version: u16,
    /// `bCountryCode` of the interface's HID class descriptor.
    pub country_code: u8,
    /// Usage page and usage of the first application collection.
    pub primary_usage_page: u16,
    pub primary_usage: u16,
    /// Stable within one attachment, distinct across interfaces and
    /// attachments.
    pub location_id: u32,
    pub manufacturer: String,
    pub product: String,
    /// Session-scoped, never the device's own serial (ADR 0022 D3).
    pub serial: String,
    /// The interrupt IN endpoint its reports arrive on.
    pub input_endpoint: EndpointAddress,
    /// Length requested for each interrupt IN transfer.
    pub input_length: u16,
}

/// A get or set report call from the host's HID stack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HidReportRequest {
    /// `GET_REPORT` into a buffer of `length` bytes.
    Get {
        index: u8,
        kind: HidReportKind,
        report_id: u8,
        length: u16,
    },
    /// `SET_REPORT` with `data`, report id included when non-zero.
    Set {
        index: u8,
        kind: HidReportKind,
        report_id: u8,
        data: Vec<u8>,
    },
}

/// Why a report request did not succeed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HidReportFailure {
    /// The device refused it (a stall).
    Stall,
    /// No answer within [`HID_HOST_CONTROL_DEADLINE`].
    TimedOut,
    /// The transfer failed otherwise.
    Failed(UrbStatus),
    /// [`MAX_QUEUED_REPORT_REQUESTS`] were already waiting.
    QueueFull,
    /// The devices are not running (enumerating, failed or stopped).
    NotRunning,
    /// No device has that index.
    UnknownDevice,
    /// The report or buffer exceeds the bridge bound, or an input report was
    /// set.
    Invalid,
}

impl Display for HidReportFailure {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stall => formatter.write_str("the device refused the report request"),
            Self::TimedOut => formatter.write_str("the report request timed out"),
            Self::Failed(status) => write!(formatter, "the report request failed ({status:?})"),
            Self::QueueFull => formatter.write_str("too many report requests are waiting"),
            Self::NotRunning => formatter.write_str("the bridged device is not running"),
            Self::UnknownDevice => formatter.write_str("no such bridged HID device"),
            Self::Invalid => formatter.write_str("the report request is out of bounds"),
        }
    }
}

/// What the platform must do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HidHostOutput {
    /// Submit this URB to the bridged device.
    Submit(HidHostUrb),
    /// Cancel this URB.
    Cancel(UrbId),
    /// Enumeration is complete: publish these devices, then call
    /// [`HidHostAdapter::devices_created`].
    CreateDevices(Vec<BridgedHidDevice>),
    /// One input report for the device at `index`, verbatim.
    Report { index: u8, report: Vec<u8> },
    /// The answer to the report request `token`.
    ReportReply {
        token: u32,
        result: Result<Vec<u8>, HidReportFailure>,
    },
}

/// Why the attachment cannot go on. Every one is final.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HidHostError {
    /// A served descriptor contradicts the authorization.
    Guard(BridgedDeviceViolation),
    /// A served descriptor cannot be used.
    Descriptor(String),
    /// An enumeration request failed.
    Enumeration {
        step: &'static str,
        status: UrbStatus,
    },
    /// A completion for a transfer the adapter never submitted.
    UnknownCompletion(UrbId),
    /// A completion carried more data than was asked for.
    Oversized(UrbId),
    /// Enumeration did not finish within [`HID_HOST_ENUMERATION_DEADLINE`].
    EnumerationTimeout,
    /// An enumeration request got no answer within
    /// [`HID_HOST_CONTROL_DEADLINE`].
    ControlTimeout(&'static str),
    /// An interface's interrupt transfers kept failing.
    InterruptFailures { interface: u8 },
    /// The device went away.
    Disconnected,
    /// The platform called out of order.
    OutOfOrder(&'static str),
    /// The publish policy leaves no interface to publish.
    NothingPublishable,
}

impl Display for HidHostError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Guard(violation) => write!(
                formatter,
                "a descriptor the bridged device served contradicts its authorization: {violation}"
            ),
            Self::Descriptor(reason) => write!(formatter, "unusable descriptor: {reason}"),
            Self::Enumeration { step, status } => {
                write!(formatter, "enumeration failed at {step} ({status:?})")
            }
            Self::UnknownCompletion(urb) => {
                write!(formatter, "completion for unknown transfer {}", urb.get())
            }
            Self::Oversized(urb) => write!(
                formatter,
                "transfer {} returned more data than requested",
                urb.get()
            ),
            Self::EnumerationTimeout => formatter.write_str("enumeration did not finish in time"),
            Self::ControlTimeout(step) => write!(formatter, "no answer to {step}"),
            Self::InterruptFailures { interface } => write!(
                formatter,
                "interface {interface} kept failing its interrupt transfers"
            ),
            Self::Disconnected => formatter.write_str("the bridged device disconnected"),
            Self::OutOfOrder(what) => write!(formatter, "out of order: {what}"),
            Self::NothingPublishable => formatter
                .write_str("no interface of the bridged device may be published on this host"),
        }
    }
}

impl std::error::Error for HidHostError {}

/// Where the adapter is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HidHostPhase {
    /// Not started.
    Idle,
    /// Reading descriptors and configuring.
    Enumerating,
    /// Waiting for the platform to publish the devices.
    Planned,
    /// Polling and answering report requests.
    Running,
    /// Shut down.
    Stopped,
    /// Gave up; see the error that was returned.
    Failed,
}

/// Counters for logs and tests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HidHostStats {
    pub urbs_submitted: u64,
    pub reports: u64,
    pub report_bytes: u64,
    pub report_requests: u64,
    pub report_requests_failed: u64,
    pub interrupt_errors: u64,
    pub stalled_set_idle: u64,
    /// Interrupt transfers cancelled and renewed by the idle watchdog.
    pub interrupt_resubmits: u64,
    /// Reports a renewed transfer brought before its cancellation landed.
    pub late_reports: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EnumStep {
    Device,
    ConfigurationHeader,
    Configuration,
    Manufacturer,
    Product,
    SetConfiguration,
    SetIdle(usize),
    Report(usize),
}

impl EnumStep {
    const fn name(self) -> &'static str {
        match self {
            Self::Device => "GET_DESCRIPTOR(Device)",
            Self::ConfigurationHeader | Self::Configuration => "GET_DESCRIPTOR(Configuration)",
            Self::Manufacturer | Self::Product => "GET_DESCRIPTOR(String)",
            Self::SetConfiguration => "SET_CONFIGURATION",
            Self::SetIdle(_) => "SET_IDLE",
            Self::Report(_) => "GET_DESCRIPTOR(Report)",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ControlPurpose {
    Enumeration(EnumStep),
    Report { token: u32, get: bool },
}

#[derive(Debug, Clone, Copy)]
struct ControlInFlight {
    urb: UrbId,
    purpose: ControlPurpose,
    requested: u16,
    started: Instant,
}

/// Which list a tombstone is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pile {
    Buried,
    Retired,
}

/// A transfer the adapter cancelled and still expects the Cancelled answer
/// for (the Deck answers every cancel with exactly one, even for a transfer
/// that had already completed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Buried {
    /// A renewed interrupt IN of the interface at this plan index.
    Interrupt { index: usize, requested: u16 },
    /// A report request's control transfer that timed out.
    Control,
}

/// A cancelled transfer: awaiting its Cancelled answer in `tombstones`, or
/// answered and kept a little longer in `retired`, because the Deck may send
/// the transfer's own completion after the Cancelled (its helper sends them
/// from two threads).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Tombstone {
    urb: UrbId,
    buried: Buried,
    /// A late report was already delivered for it.
    delivered: bool,
}

#[derive(Debug, Clone)]
struct PlannedInterface {
    number: u8,
    endpoint: EndpointAddress,
    input_length: u16,
    country_code: u8,
    report_descriptor: Vec<u8>,
}

#[derive(Debug, Clone, Copy, Default)]
struct DeviceFacts {
    identity: Option<UsbDeviceId>,
    manufacturer_index: u8,
    product_index: u8,
    configuration_total: u16,
}

/// The USB host driver of one bridged HID device (see the module docs).
#[derive(Debug)]
pub struct HidHostAdapter {
    identity: UsbDeviceId,
    claims: Vec<InterfaceReportDescriptor>,
    generation: AttachmentGeneration,
    guard: BridgedDeviceGuard,
    policy: HidPublishPolicy,
    withheld: Vec<(u8, PublishRefusal)>,
    phase: HidHostPhase,
    started: Option<Instant>,
    next_urb: u32,
    facts: DeviceFacts,
    configuration_value: u8,
    interfaces: Vec<PlannedInterface>,
    manufacturer: Option<String>,
    product: Option<String>,
    control: Option<ControlInFlight>,
    queued: VecDeque<(u32, HidReportRequest)>,
    interrupts: BTreeMap<UrbId, usize>,
    interrupt_lengths: BTreeMap<UrbId, u16>,
    interrupt_submitted: BTreeMap<UrbId, Instant>,
    interrupt_errors: Vec<u32>,
    tombstones: VecDeque<Tombstone>,
    retired: VecDeque<Tombstone>,
    stats: HidHostStats,
}

impl HidHostAdapter {
    /// An adapter for one authorized device: the identity and report
    /// descriptors [`crate::authorize_hard_usb_tablet`] admitted.
    #[must_use]
    pub fn new(
        identity: UsbDeviceId,
        claims: Vec<InterfaceReportDescriptor>,
        generation: AttachmentGeneration,
    ) -> Self {
        Self {
            identity,
            guard: BridgedDeviceGuard::new(identity, claims.clone()),
            claims,
            generation,
            policy: HidPublishPolicy::AllAuthorized,
            withheld: Vec::new(),
            phase: HidHostPhase::Idle,
            started: None,
            next_urb: 0,
            facts: DeviceFacts::default(),
            configuration_value: 0,
            interfaces: Vec::new(),
            manufacturer: None,
            product: None,
            control: None,
            queued: VecDeque::new(),
            interrupts: BTreeMap::new(),
            interrupt_lengths: BTreeMap::new(),
            interrupt_submitted: BTreeMap::new(),
            interrupt_errors: Vec::new(),
            tombstones: VecDeque::new(),
            retired: VecDeque::new(),
            stats: HidHostStats::default(),
        }
    }

    /// Publishes only the interfaces `policy` allows (default: every
    /// authorized one). Withheld interfaces are enumerated with the device
    /// but get no HID device and are never polled.
    #[must_use]
    pub const fn with_publish_policy(mut self, policy: HidPublishPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Authorized interfaces the publish policy withheld, and why.
    #[must_use]
    pub fn withheld(&self) -> &[(u8, PublishRefusal)] {
        &self.withheld
    }

    /// The attachment generation every URB belongs to.
    #[must_use]
    pub const fn generation(&self) -> AttachmentGeneration {
        self.generation
    }

    /// Where the adapter is.
    #[must_use]
    pub const fn phase(&self) -> HidHostPhase {
        self.phase
    }

    /// Counters so far.
    #[must_use]
    pub const fn stats(&self) -> HidHostStats {
        self.stats
    }

    /// Transfers currently outstanding.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.interrupts.len() + usize::from(self.control.is_some())
    }

    /// Starts enumeration.
    ///
    /// # Errors
    ///
    /// [`HidHostError::OutOfOrder`] when already started.
    pub fn start(&mut self, now: Instant) -> Result<Vec<HidHostOutput>, HidHostError> {
        if self.phase != HidHostPhase::Idle {
            return Err(HidHostError::OutOfOrder("start"));
        }
        self.phase = HidHostPhase::Enumerating;
        self.started = Some(now);
        let mut out = Vec::new();
        self.submit_step(EnumStep::Device, now, &mut out)?;
        Ok(out)
    }

    /// The platform published every planned device: start polling them.
    ///
    /// # Errors
    ///
    /// [`HidHostError::OutOfOrder`] unless the devices were just planned.
    pub fn devices_created(&mut self, now: Instant) -> Result<Vec<HidHostOutput>, HidHostError> {
        if self.phase != HidHostPhase::Planned {
            return Err(HidHostError::OutOfOrder("devices_created"));
        }
        self.phase = HidHostPhase::Running;
        self.interrupt_errors = vec![0; self.interfaces.len()];
        let mut out = Vec::new();
        for index in 0..self.interfaces.len() {
            self.submit_interrupt(index, now, &mut out);
        }
        self.pump_reports(now, &mut out)?;
        Ok(out)
    }

    /// A report call from the host's HID stack. The answer arrives later as
    /// [`HidHostOutput::ReportReply`] with the same `token`, or at once when
    /// the request cannot be made.
    #[must_use]
    pub fn request_report(
        &mut self,
        token: u32,
        request: HidReportRequest,
        now: Instant,
    ) -> Vec<HidHostOutput> {
        self.stats.report_requests += 1;
        let refuse = |failure| {
            vec![HidHostOutput::ReportReply {
                token,
                result: Err(failure),
            }]
        };
        if self.phase != HidHostPhase::Running {
            self.stats.report_requests_failed += 1;
            return refuse(HidReportFailure::NotRunning);
        }
        let (index, valid) = match &request {
            HidReportRequest::Get { index, length, .. } => (
                *index,
                *length > 0 && usize::from(*length) <= MAX_TRANSFER_BYTES,
            ),
            HidReportRequest::Set {
                index, kind, data, ..
            } => (
                *index,
                *kind != HidReportKind::Input && data.len() <= MAX_TRANSFER_BYTES,
            ),
        };
        if usize::from(index) >= self.interfaces.len() {
            self.stats.report_requests_failed += 1;
            return refuse(HidReportFailure::UnknownDevice);
        }
        if !valid {
            self.stats.report_requests_failed += 1;
            return refuse(HidReportFailure::Invalid);
        }
        if self.queued.len() >= MAX_QUEUED_REPORT_REQUESTS {
            self.stats.report_requests_failed += 1;
            return refuse(HidReportFailure::QueueFull);
        }
        self.queued.push_back((token, request));
        let mut out = Vec::new();
        // Running and the queue has room: submitting cannot fail.
        let _ = self.pump_reports(now, &mut out);
        out
    }

    /// One completion from the bridged device.
    ///
    /// # Errors
    ///
    /// A [`HidHostError`] ends the attachment: the platform must tear the
    /// devices down and stop forwarding.
    pub fn on_completion(
        &mut self,
        urb: UrbId,
        status: UrbStatus,
        payload: &[u8],
        now: Instant,
    ) -> Result<Vec<HidHostOutput>, HidHostError> {
        if matches!(self.phase, HidHostPhase::Stopped | HidHostPhase::Failed) {
            return Ok(Vec::new());
        }
        if let Some(position) = self.tombstones.iter().position(|stone| stone.urb == urb) {
            if status == UrbStatus::Cancelled {
                if let Some(stone) = self.tombstones.remove(position) {
                    if self.retired.len() >= MAX_TOMBSTONES {
                        self.retired.pop_front();
                    }
                    self.retired.push_back(stone);
                }
                return Ok(Vec::new());
            }
            return Ok(self.late_completion(Pile::Buried, position, status, payload));
        }
        if let Some(position) = self.retired.iter().position(|stone| stone.urb == urb) {
            if status == UrbStatus::Cancelled {
                return Ok(Vec::new());
            }
            return Ok(self.late_completion(Pile::Retired, position, status, payload));
        }
        let result = self.completion(urb, status, payload, now);
        if result.is_err() {
            self.phase = HidHostPhase::Failed;
        }
        result
    }

    /// A completion other than Cancelled for a transfer the adapter
    /// cancelled. A transfer that completed before the cancellation reached
    /// the Deck is answered twice — its own completion and Cancelled — in
    /// either order (the Deck's helper sends them from two threads), so
    /// neither retires the other. A renewed interrupt transfer's report is
    /// valid input and is delivered once: the device completes one
    /// endpoint's transfers in submission order and the renewed transfer was
    /// submitted after the cancel, so it still precedes anything the renewed
    /// transfer brings. A timed-out report request's late answer has already
    /// been failed and is dropped.
    fn late_completion(
        &mut self,
        pile: Pile,
        position: usize,
        status: UrbStatus,
        payload: &[u8],
    ) -> Vec<HidHostOutput> {
        let stones = match pile {
            Pile::Buried => &mut self.tombstones,
            Pile::Retired => &mut self.retired,
        };
        let Some(stone) = stones.get_mut(position) else {
            return Vec::new();
        };
        let Buried::Interrupt { index, requested } = stone.buried else {
            return Vec::new();
        };
        if stone.delivered
            || status != UrbStatus::Success
            || payload.is_empty()
            || payload.len() > usize::from(requested)
            || self.phase != HidHostPhase::Running
            || index >= self.interfaces.len()
        {
            return Vec::new();
        }
        stone.delivered = true;
        self.stats.reports += 1;
        self.stats.report_bytes += payload.len() as u64;
        self.stats.late_reports += 1;
        vec![HidHostOutput::Report {
            index: u8::try_from(index).unwrap_or(u8::MAX),
            report: payload.to_vec(),
        }]
    }

    /// Enforces the deadlines; call it about every 100 ms.
    ///
    /// # Errors
    ///
    /// A [`HidHostError`] when enumeration, or one of its requests, ran out of
    /// time.
    pub fn poll(&mut self, now: Instant) -> Result<Vec<HidHostOutput>, HidHostError> {
        let mut out = Vec::new();
        if self.phase == HidHostPhase::Running {
            self.renew_idle_interrupts(now, &mut out);
        }
        if self.phase == HidHostPhase::Enumerating
            && self
                .started
                .is_some_and(|started| now.duration_since(started) > HID_HOST_ENUMERATION_DEADLINE)
        {
            self.phase = HidHostPhase::Failed;
            return Err(HidHostError::EnumerationTimeout);
        }
        let Some(control) = self.control else {
            return Ok(out);
        };
        if now.duration_since(control.started) <= HID_HOST_CONTROL_DEADLINE {
            return Ok(out);
        }
        match control.purpose {
            ControlPurpose::Enumeration(step) => {
                self.phase = HidHostPhase::Failed;
                Err(HidHostError::ControlTimeout(step.name()))
            }
            ControlPurpose::Report { token, .. } => {
                self.control = None;
                self.bury(control.urb, Buried::Control);
                out.push(HidHostOutput::Cancel(control.urb));
                self.stats.report_requests_failed += 1;
                out.push(HidHostOutput::ReportReply {
                    token,
                    result: Err(HidReportFailure::TimedOut),
                });
                self.pump_reports(now, &mut out)?;
                Ok(out)
            }
        }
    }

    /// Cancels and renews every interrupt transfer outstanding for longer
    /// than [`INTERRUPT_IDLE_RESUBMIT`]. The old transfer is buried until its
    /// Cancelled answer arrives; a report it brought first is delivered.
    fn renew_idle_interrupts(&mut self, now: Instant, out: &mut Vec<HidHostOutput>) {
        let stale: Vec<(UrbId, usize)> = self
            .interrupts
            .iter()
            .filter(|(urb, _)| {
                self.interrupt_submitted.get(urb).is_some_and(|submitted| {
                    now.duration_since(*submitted) >= INTERRUPT_IDLE_RESUBMIT
                })
            })
            .map(|(urb, index)| (*urb, *index))
            .collect();
        for (urb, index) in stale {
            self.interrupts.remove(&urb);
            let requested = self.interrupt_lengths.remove(&urb).unwrap_or(0);
            self.interrupt_submitted.remove(&urb);
            self.bury(urb, Buried::Interrupt { index, requested });
            out.push(HidHostOutput::Cancel(urb));
            self.stats.interrupt_resubmits += 1;
            self.submit_interrupt(index, now, out);
        }
    }

    /// Stops: cancels every outstanding transfer and fails every waiting
    /// report request. Later completions are ignored.
    #[must_use]
    pub fn shutdown(&mut self) -> Vec<HidHostOutput> {
        let mut out = Vec::new();
        if let Some(control) = self.control.take() {
            out.push(HidHostOutput::Cancel(control.urb));
            if let ControlPurpose::Report { token, .. } = control.purpose {
                out.push(HidHostOutput::ReportReply {
                    token,
                    result: Err(HidReportFailure::NotRunning),
                });
            }
        }
        for urb in std::mem::take(&mut self.interrupts).into_keys() {
            out.push(HidHostOutput::Cancel(urb));
        }
        self.interrupt_lengths.clear();
        self.interrupt_submitted.clear();
        for (token, _) in std::mem::take(&mut self.queued) {
            out.push(HidHostOutput::ReportReply {
                token,
                result: Err(HidReportFailure::NotRunning),
            });
        }
        self.phase = HidHostPhase::Stopped;
        out
    }

    fn completion(
        &mut self,
        urb: UrbId,
        status: UrbStatus,
        payload: &[u8],
        now: Instant,
    ) -> Result<Vec<HidHostOutput>, HidHostError> {
        let mut out = Vec::new();
        if let Some(index) = self.interrupts.remove(&urb) {
            let requested = self.interrupt_lengths.remove(&urb).unwrap_or(0);
            self.interrupt_submitted.remove(&urb);
            if payload.len() > usize::from(requested) {
                return Err(HidHostError::Oversized(urb));
            }
            self.interrupt_done(index, status, payload, now, &mut out)?;
            return Ok(out);
        }
        let Some(control) = self.control.filter(|control| control.urb == urb) else {
            return Err(HidHostError::UnknownCompletion(urb));
        };
        self.control = None;
        if payload.len() > usize::from(control.requested) {
            return Err(HidHostError::Oversized(urb));
        }
        let success = status == UrbStatus::Success;
        self.guard
            .check_completion(urb, success, payload)
            .map_err(HidHostError::Guard)?;
        match control.purpose {
            ControlPurpose::Enumeration(step) => {
                self.enumeration_done(step, status, payload, now, &mut out)?;
            }
            ControlPurpose::Report { token, get } => {
                let result = match status {
                    UrbStatus::Success => Ok(if get { payload.to_vec() } else { Vec::new() }),
                    UrbStatus::Stall => Err(HidReportFailure::Stall),
                    UrbStatus::TimedOut => Err(HidReportFailure::TimedOut),
                    UrbStatus::Disconnected => return Err(HidHostError::Disconnected),
                    other => Err(HidReportFailure::Failed(other)),
                };
                if result.is_err() {
                    self.stats.report_requests_failed += 1;
                }
                out.push(HidHostOutput::ReportReply { token, result });
            }
        }
        self.pump_reports(now, &mut out)?;
        Ok(out)
    }

    fn interrupt_done(
        &mut self,
        index: usize,
        status: UrbStatus,
        payload: &[u8],
        now: Instant,
        out: &mut Vec<HidHostOutput>,
    ) -> Result<(), HidHostError> {
        if self.phase != HidHostPhase::Running {
            return Ok(());
        }
        match status {
            UrbStatus::Success => {
                self.interrupt_errors[index] = 0;
                if !payload.is_empty() {
                    self.stats.reports += 1;
                    self.stats.report_bytes += payload.len() as u64;
                    out.push(HidHostOutput::Report {
                        index: u8::try_from(index).unwrap_or(u8::MAX),
                        report: payload.to_vec(),
                    });
                }
            }
            UrbStatus::Disconnected => return Err(HidHostError::Disconnected),
            _ => {
                self.stats.interrupt_errors += 1;
                self.interrupt_errors[index] += 1;
                if self.interrupt_errors[index] >= MAX_INTERRUPT_ERRORS {
                    return Err(HidHostError::InterruptFailures {
                        interface: self.interfaces[index].number,
                    });
                }
            }
        }
        self.submit_interrupt(index, now, out);
        Ok(())
    }

    fn enumeration_done(
        &mut self,
        step: EnumStep,
        status: UrbStatus,
        payload: &[u8],
        now: Instant,
        out: &mut Vec<HidHostOutput>,
    ) -> Result<(), HidHostError> {
        let tolerated = matches!(
            step,
            EnumStep::Manufacturer | EnumStep::Product | EnumStep::SetIdle(_)
        );
        if status == UrbStatus::Disconnected {
            return Err(HidHostError::Disconnected);
        }
        if status == UrbStatus::Success {
            self.absorb(step, payload)?;
        } else {
            if !tolerated {
                return Err(HidHostError::Enumeration {
                    step: step.name(),
                    status,
                });
            }
            if matches!(step, EnumStep::SetIdle(_)) {
                self.stats.stalled_set_idle += 1;
            }
        }
        if let Some(next) = self.next_step(step) {
            return self.submit_step(next, now, out);
        }
        self.phase = HidHostPhase::Planned;
        out.push(HidHostOutput::CreateDevices(self.plan()));
        Ok(())
    }

    fn absorb(&mut self, step: EnumStep, payload: &[u8]) -> Result<(), HidHostError> {
        match step {
            EnumStep::Device => {
                // The guard checks what a prefix carries; here the whole
                // descriptor is required, and it must name exactly the
                // authorized device, which is the only identity published.
                let [
                    18,
                    DESCRIPTOR_DEVICE,
                    _,
                    _,
                    _,
                    _,
                    _,
                    _,
                    vendor_low,
                    vendor_high,
                    product_low,
                    product_high,
                    release_low,
                    release_high,
                    manufacturer,
                    product,
                    _,
                    _,
                ] = *payload
                else {
                    return Err(HidHostError::Descriptor(format!(
                        "a {}-byte device descriptor",
                        payload.len()
                    )));
                };
                let served = UsbDeviceId {
                    vendor_id: u16::from_le_bytes([vendor_low, vendor_high]),
                    product_id: u16::from_le_bytes([product_low, product_high]),
                    bcd_device: u16::from_le_bytes([release_low, release_high]),
                };
                if served != self.identity {
                    return Err(HidHostError::Descriptor(
                        "the device descriptor names another device".to_owned(),
                    ));
                }
                self.facts.identity = Some(self.identity);
                self.facts.manufacturer_index = manufacturer;
                self.facts.product_index = product;
            }
            EnumStep::ConfigurationHeader => {
                let [9, DESCRIPTOR_CONFIGURATION, total_low, total_high, ..] = *payload else {
                    return Err(HidHostError::Descriptor(format!(
                        "a {}-byte configuration header",
                        payload.len()
                    )));
                };
                if payload.len() != usize::from(CONFIGURATION_HEADER_LENGTH) {
                    return Err(HidHostError::Descriptor(format!(
                        "a {}-byte configuration header",
                        payload.len()
                    )));
                }
                let total = u16::from_le_bytes([total_low, total_high]);
                if usize::from(total) < usize::from(CONFIGURATION_HEADER_LENGTH)
                    || usize::from(total) > MAX_CONFIGURATION_DESCRIPTOR_BYTES
                {
                    return Err(HidHostError::Descriptor(format!(
                        "configuration wTotalLength {total} is out of bounds"
                    )));
                }
                self.facts.configuration_total = total;
            }
            EnumStep::Configuration => self.absorb_configuration(payload)?,
            EnumStep::Manufacturer => self.manufacturer = string_descriptor(payload),
            EnumStep::Product => self.product = string_descriptor(payload),
            EnumStep::SetConfiguration | EnumStep::SetIdle(_) | EnumStep::Report(_) => {}
        }
        Ok(())
    }

    fn absorb_configuration(&mut self, payload: &[u8]) -> Result<(), HidHostError> {
        let parsed = parse_configuration_descriptor(payload)
            .map_err(|error| HidHostError::Descriptor(error.to_string()))?;
        let countries = hid_country_codes(payload);
        let mut interfaces = Vec::new();
        let mut seen = BTreeSet::new();
        for interface in parsed
            .interfaces
            .iter()
            .filter(|interface| interface.alternate_setting.0 == 0)
        {
            let number = interface.number.0;
            if !seen.insert(number) {
                continue;
            }
            let Some(claim) = self.claims.iter().find(|claim| claim.interface == number) else {
                return Err(HidHostError::Descriptor(format!(
                    "interface {number} was not authorized"
                )));
            };
            if let Err(refusal) = crate::publishable(self.policy, &claim.descriptor) {
                self.withheld.push((number, refusal));
                continue;
            }
            let Some(endpoint) = interface
                .endpoints
                .iter()
                .find(|endpoint| endpoint.address.0 & 0x80 != 0)
            else {
                return Err(HidHostError::Descriptor(format!(
                    "interface {number} has no interrupt IN endpoint"
                )));
            };
            let declared = declared_report_lengths(&claim.descriptor, HidReportKind::Input)
                .into_values()
                .max()
                .unwrap_or(0);
            let length = declared
                .max(usize::from(endpoint.max_packet_size & 0x07ff))
                .clamp(1, MAX_TRANSFER_BYTES);
            interfaces.push(PlannedInterface {
                number,
                endpoint: endpoint.address,
                input_length: u16::try_from(length).unwrap_or(u16::MAX),
                country_code: countries.get(&number).copied().unwrap_or(0),
                report_descriptor: claim.descriptor.clone(),
            });
        }
        if interfaces.is_empty() && !self.withheld.is_empty() {
            return Err(HidHostError::NothingPublishable);
        }
        if interfaces.is_empty() || interfaces.len() > MAX_INTERFACES {
            return Err(HidHostError::Descriptor(
                "the configuration has no usable HID interface".to_owned(),
            ));
        }
        self.configuration_value = parsed.configuration_value;
        self.interfaces = interfaces;
        Ok(())
    }

    fn next_step(&self, step: EnumStep) -> Option<EnumStep> {
        let after_strings = EnumStep::SetConfiguration;
        Some(match step {
            EnumStep::Device => EnumStep::ConfigurationHeader,
            EnumStep::ConfigurationHeader => EnumStep::Configuration,
            EnumStep::Configuration if self.facts.manufacturer_index != 0 => EnumStep::Manufacturer,
            EnumStep::Configuration | EnumStep::Manufacturer if self.facts.product_index != 0 => {
                EnumStep::Product
            }
            EnumStep::Configuration | EnumStep::Manufacturer | EnumStep::Product => after_strings,
            EnumStep::SetConfiguration => EnumStep::SetIdle(0),
            EnumStep::SetIdle(index) => EnumStep::Report(index),
            EnumStep::Report(index) if index + 1 < self.interfaces.len() => {
                EnumStep::SetIdle(index + 1)
            }
            EnumStep::Report(_) => return None,
        })
    }

    fn setup_for(&self, step: EnumStep) -> SetupPacket {
        let standard_in = |value: u16, index: u16, length: u16| SetupPacket {
            request_type: 0x80,
            request: REQUEST_GET_DESCRIPTOR,
            value,
            index,
            length,
        };
        let string = |index: u8| {
            standard_in(
                (u16::from(DESCRIPTOR_STRING) << 8) | u16::from(index),
                LANGUAGE_EN_US,
                MAX_STRING_DESCRIPTOR,
            )
        };
        match step {
            EnumStep::Device => standard_in(
                u16::from(DESCRIPTOR_DEVICE) << 8,
                0,
                DEVICE_DESCRIPTOR_LENGTH,
            ),
            EnumStep::ConfigurationHeader => standard_in(
                u16::from(DESCRIPTOR_CONFIGURATION) << 8,
                0,
                CONFIGURATION_HEADER_LENGTH,
            ),
            EnumStep::Configuration => standard_in(
                u16::from(DESCRIPTOR_CONFIGURATION) << 8,
                0,
                self.facts.configuration_total,
            ),
            EnumStep::Manufacturer => string(self.facts.manufacturer_index),
            EnumStep::Product => string(self.facts.product_index),
            EnumStep::SetConfiguration => SetupPacket {
                request_type: 0x00,
                request: REQUEST_SET_CONFIGURATION,
                value: u16::from(self.configuration_value),
                index: 0,
                length: 0,
            },
            EnumStep::SetIdle(index) => SetupPacket {
                request_type: 0x21,
                request: HID_SET_IDLE,
                value: 0,
                index: u16::from(self.interfaces[index].number),
                length: 0,
            },
            EnumStep::Report(index) => {
                let interface = &self.interfaces[index];
                hid_report_descriptor_request(
                    interface.number,
                    u16::try_from(interface.report_descriptor.len()).unwrap_or(u16::MAX),
                )
            }
        }
    }

    fn submit_step(
        &mut self,
        step: EnumStep,
        now: Instant,
        out: &mut Vec<HidHostOutput>,
    ) -> Result<(), HidHostError> {
        let setup = self.setup_for(step);
        self.submit_control(
            setup,
            Vec::new(),
            ControlPurpose::Enumeration(step),
            now,
            out,
        )
    }

    fn submit_control(
        &mut self,
        setup: SetupPacket,
        payload: Vec<u8>,
        purpose: ControlPurpose,
        now: Instant,
        out: &mut Vec<HidHostOutput>,
    ) -> Result<(), HidHostError> {
        if self.control.is_some() {
            return Err(HidHostError::OutOfOrder("a second control transfer"));
        }
        let urb = self.next_urb_id();
        self.guard
            .observe_submit(urb, Some(setup))
            .map_err(HidHostError::Guard)?;
        let direction_in = setup.request_type & 0x80 != 0;
        let requested = if direction_in { setup.length } else { 0 };
        self.control = Some(ControlInFlight {
            urb,
            purpose,
            requested,
            started: now,
        });
        self.stats.urbs_submitted += 1;
        out.push(HidHostOutput::Submit(HidHostUrb {
            urb_id: urb,
            endpoint: if direction_in {
                CONTROL_IN
            } else {
                CONTROL_OUT
            },
            transfer_kind: TransferKind::Control,
            timeout_ms: HID_HOST_CONTROL_TIMEOUT_MS,
            declared_length: u32::from(setup.length),
            setup: Some(setup),
            payload,
        }));
        Ok(())
    }

    fn submit_interrupt(&mut self, index: usize, now: Instant, out: &mut Vec<HidHostOutput>) {
        let interface = &self.interfaces[index];
        let (endpoint, length) = (interface.endpoint, interface.input_length);
        let urb = self.next_urb_id();
        self.interrupts.insert(urb, index);
        self.interrupt_lengths.insert(urb, length);
        self.interrupt_submitted.insert(urb, now);
        self.stats.urbs_submitted += 1;
        out.push(HidHostOutput::Submit(HidHostUrb {
            urb_id: urb,
            endpoint,
            transfer_kind: TransferKind::Interrupt,
            timeout_ms: HID_HOST_CONTROL_TIMEOUT_MS,
            declared_length: u32::from(length),
            setup: None,
            payload: Vec::new(),
        }));
    }

    fn pump_reports(
        &mut self,
        now: Instant,
        out: &mut Vec<HidHostOutput>,
    ) -> Result<(), HidHostError> {
        if self.phase != HidHostPhase::Running || self.control.is_some() {
            return Ok(());
        }
        let Some((token, request)) = self.queued.pop_front() else {
            return Ok(());
        };
        let (setup, payload, get) = match request {
            HidReportRequest::Get {
                index,
                kind,
                report_id,
                length,
            } => (
                SetupPacket {
                    request_type: 0xA1,
                    request: HID_GET_REPORT,
                    value: (u16::from(kind.request_value()) << 8) | u16::from(report_id),
                    index: u16::from(self.interfaces[usize::from(index)].number),
                    length,
                },
                Vec::new(),
                true,
            ),
            HidReportRequest::Set {
                index,
                kind,
                report_id,
                data,
            } => (
                SetupPacket {
                    request_type: 0x21,
                    request: HID_SET_REPORT,
                    value: (u16::from(kind.request_value()) << 8) | u16::from(report_id),
                    index: u16::from(self.interfaces[usize::from(index)].number),
                    length: u16::try_from(data.len()).unwrap_or(u16::MAX),
                },
                data,
                false,
            ),
        };
        self.submit_control(
            setup,
            payload,
            ControlPurpose::Report { token, get },
            now,
            out,
        )
    }

    fn plan(&self) -> Vec<BridgedHidDevice> {
        let identity = self.facts.identity.unwrap_or(self.identity);
        let generation = self.generation.get();
        self.interfaces
            .iter()
            .enumerate()
            .map(|(index, interface)| {
                let (primary_usage_page, primary_usage) =
                    parse_report_descriptor(&interface.report_descriptor)
                        .ok()
                        .and_then(|summary| {
                            summary
                                .collections
                                .iter()
                                .find(|collection| !collection.nested)
                                .map(|collection| (collection.usage_page, collection.usage))
                        })
                        .unwrap_or((0, 0));
                BridgedHidDevice {
                    index: u8::try_from(index).unwrap_or(u8::MAX),
                    interface: interface.number,
                    report_descriptor: interface.report_descriptor.clone(),
                    vendor_id: identity.vendor_id,
                    product_id: identity.product_id,
                    version: identity.bcd_device,
                    country_code: interface.country_code,
                    primary_usage_page,
                    primary_usage,
                    location_id: location_id(self.generation, interface.number),
                    manufacturer: self.manufacturer.clone().unwrap_or_default(),
                    product: self
                        .product
                        .clone()
                        .unwrap_or_else(|| DEFAULT_PRODUCT_NAME.to_owned()),
                    serial: format!("arcen-{generation:016x}-{}", interface.number),
                    input_endpoint: interface.endpoint,
                    input_length: interface.input_length,
                }
            })
            .collect()
    }

    fn next_urb_id(&mut self) -> UrbId {
        self.next_urb = self.next_urb.wrapping_add(1).max(1);
        UrbId::new(NonZeroU32::new(self.next_urb).unwrap_or(NonZeroU32::MIN))
    }

    /// Records a cancelled transfer until its Cancelled answer arrives, then
    /// keeps it retired for a while in case its own completion follows. At
    /// most [`MAX_TOMBSTONES`] of each are kept, oldest dropped first, so a
    /// cancel whose answer was lost cannot pin memory.
    fn bury(&mut self, urb: UrbId, buried: Buried) {
        self.guard.observe_cancel(urb);
        if self.tombstones.len() >= MAX_TOMBSTONES {
            self.tombstones.pop_front();
        }
        self.tombstones.push_back(Tombstone {
            urb,
            buried,
            delivered: false,
        });
    }
}

/// `LocationID` of one bridged interface: [`LOCATION_ID_BASE`], twelve bits
/// of the attachment generation, then the interface number.
#[must_use]
pub fn location_id(generation: AttachmentGeneration, interface: u8) -> u32 {
    let low = u32::try_from(generation.get() & 0xFFF).unwrap_or(0);
    LOCATION_ID_BASE | (low << 8) | u32::from(interface)
}

/// Whether a `LocationID` is one [`location_id`] produced: the twelve
/// generation bits reach into the upper half-word, so the prefix is the top
/// twelve bits, not sixteen.
#[must_use]
pub const fn is_bridged_location_id(location: u32) -> bool {
    location & 0xFFF0_0000 == LOCATION_ID_BASE
}

/// Decodes a USB string descriptor into a property value: UTF-16LE, control
/// characters removed, at most [`MAX_PROPERTY_CHARS`] characters, trimmed.
/// `None` for a malformed or empty one.
#[must_use]
pub fn string_descriptor(payload: &[u8]) -> Option<String> {
    let length = usize::from(*payload.first()?);
    if payload.get(1) != Some(&DESCRIPTOR_STRING) || length < 2 || length > payload.len() {
        return None;
    }
    let units: Vec<u16> = payload[2..length]
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    let text: String = char::decode_utf16(units)
        .map(|unit| unit.unwrap_or(char::REPLACEMENT_CHARACTER))
        .filter(|character| !character.is_control())
        .take(MAX_PROPERTY_CHARS)
        .collect();
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

/// `bCountryCode` of each interface's HID class descriptor (alternate
/// setting 0), by interface number.
#[must_use]
pub fn hid_country_codes(configuration: &[u8]) -> BTreeMap<u8, u8> {
    let mut codes = BTreeMap::new();
    let mut current: Option<u8> = None;
    let mut offset = 0_usize;
    while offset + 2 <= configuration.len() {
        let length = usize::from(configuration[offset]);
        if length < 2 || offset + length > configuration.len() {
            break;
        }
        let descriptor = &configuration[offset..offset + length];
        match descriptor[1] {
            DESCRIPTOR_INTERFACE if length >= 9 => {
                current = (descriptor[3] == 0).then_some(descriptor[2]);
            }
            DESCRIPTOR_HID if length >= 6 => {
                if let Some(interface) = current {
                    codes.entry(interface).or_insert(descriptor[4]);
                }
            }
            _ => {}
        }
        offset += length;
    }
    codes
}

#[cfg(test)]
#[path = "hid_host_tests.rs"]
mod tests;
