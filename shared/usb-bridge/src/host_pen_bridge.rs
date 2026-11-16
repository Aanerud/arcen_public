// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! One Native tablet attachment on a Pier that reads the bridged pen itself
//! (ADR 0022 §14, ADR 0023 amendment): the shared USB host driver
//! ([`HidHostAdapter`]) over the URB bridge, the pen's reader (dedicated or
//! generic, [`HostPenRoute`]), the Intuos mode switch and its restore, and
//! the samples the host turns into its own OS pen events.
//!
//! Sans-I/O, like the adapter. A host that also publishes HID devices of its
//! own (the macOS Pier's `IOHIDUserDevice`s) gets them as
//! [`HostPenBridgeOutput::Publish`] and their traffic as device outputs; a
//! host that publishes none ([`HidPublishPolicy::Nothing`], the Windows Pier,
//! which injects through Windows Ink) never sees one. The pen interface's
//! input reports are always read here and never handed to a published
//! device: one consumer, so a driver that binds the published device one day
//! cannot double the pen. A published pen interface (macOS) may still make
//! report requests on it, as its device owner always could; the bridge's own
//! mode switch and restore use tokens no device may borrow.

use crate::{
    AttachmentGeneration, BridgedHidDevice, HidHostAdapter, HidHostError, HidHostOutput,
    HidHostPhase, HidHostStats, HidHostUrb, HidPublishPolicy, HidReportFailure, HidReportKind,
    HidReportRequest, HostPenReader, HostPenRoute, HostPenSample, INTUOS_MODE_SWITCH,
    InterfaceReportDescriptor, UrbId, UsbDeviceId,
};
use std::collections::VecDeque;
use std::fmt::{Display, Formatter};
use std::time::Instant;

/// Report-request tokens the bridge uses for its own requests to the pen.
/// A published device's requests must stay below it; one at or above is
/// refused, so a reply never reaches the wrong side.
pub const HOST_PEN_TOKENS: u32 = u32::MAX - 2;
/// `GET_REPORT` of the mode the tablet was in before the switch.
const READ_MODE: u32 = u32::MAX - 2;
/// `SET_REPORT` of the reader's mode (Wacom mode).
const SET_MODE: u32 = u32::MAX - 1;
/// `SET_REPORT` of the mode read before, when the attachment ends.
const RESTORE_MODE: u32 = u32::MAX;
/// Samples waiting for the host; a host that falls this far behind loses the
/// oldest.
pub const HOST_PEN_BACKLOG: usize = 256;

/// What the platform must do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostPenBridgeOutput {
    /// Submit this URB to the bridged device.
    Submit(HidHostUrb),
    /// Cancel this URB.
    Cancel(UrbId),
    /// Publish these HID devices, then call
    /// [`HostPenBridge::devices_created`]. Never empty: with nothing to
    /// publish the bridge goes on by itself.
    Publish(Vec<BridgedHidDevice>),
    /// An input report for the published device at `index`.
    DeviceReport { index: u8, report: Vec<u8> },
    /// The answer to a published device's report request.
    DeviceReply {
        token: u32,
        result: Result<Vec<u8>, HidReportFailure>,
    },
}

/// Where the mode switch of a pen whose reader needs one stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostPenMode {
    /// Nothing sent yet, or the reader needs no switch.
    Untouched,
    /// The current mode is being read.
    Reading,
    /// The reader's mode is being set; the mode read before, when known.
    Switching(Option<u8>),
    /// The tablet accepted the switch.
    Switched(Option<u8>),
    /// The tablet refused the switch; nothing to restore.
    Refused,
}

/// Something about the pen worth a log line on the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostPenNote {
    /// The mode the pen was in before the switch (`None`: not readable).
    ModeRead {
        original: Option<u8>,
        failure: Option<HidReportFailure>,
    },
    /// The switch to the reader's mode was sent and answered.
    ModeSwitched {
        accepted: bool,
        failure: Option<HidReportFailure>,
    },
}

/// Why no attachment can start for an authorized tablet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoPenInterface;

impl Display for NoPenInterface {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("no interface of the captured tablet sends the pen this Pier reads")
    }
}

impl std::error::Error for NoPenInterface {}

/// One attachment (see the module docs).
#[derive(Debug)]
pub struct HostPenBridge {
    adapter: HidHostAdapter,
    reader: HostPenReader,
    /// The plan index of the pen interface, once planned.
    index: Option<u8>,
    /// Devices the platform publishes.
    published: usize,
    mode: HostPenMode,
    samples: VecDeque<HostPenSample>,
    last: Option<HostPenSample>,
    notes: Vec<HostPenNote>,
    dropped: u64,
}

impl HostPenBridge {
    /// A bridge for one authorized device read by `route`, publishing what
    /// `policy` allows.
    ///
    /// # Errors
    ///
    /// [`NoPenInterface`] when no authorized interface carries the pen the
    /// route reads.
    pub fn new(
        identity: UsbDeviceId,
        descriptors: Vec<InterfaceReportDescriptor>,
        route: &HostPenRoute,
        policy: HidPublishPolicy,
        generation: AttachmentGeneration,
    ) -> Result<Self, NoPenInterface> {
        let interface = route.read_interface(&descriptors).ok_or(NoPenInterface)?;
        let adapter = HidHostAdapter::new(identity, descriptors, generation)
            .with_publish_policy(policy)
            .with_host_read_interface(interface);
        Ok(Self {
            adapter,
            reader: route.reader(),
            index: None,
            published: 0,
            mode: HostPenMode::Untouched,
            samples: VecDeque::new(),
            last: None,
            notes: Vec::new(),
            dropped: 0,
        })
    }

    /// Starts enumeration.
    ///
    /// # Errors
    ///
    /// The adapter's error; the attachment cannot go on.
    pub fn start(&mut self, now: Instant) -> Result<Vec<HostPenBridgeOutput>, HidHostError> {
        let outputs = self.adapter.start(now)?;
        self.apply(outputs, now)
    }

    /// One completion from the bridged device.
    ///
    /// # Errors
    ///
    /// The adapter's error; the attachment cannot go on.
    pub fn on_completion(
        &mut self,
        urb: UrbId,
        status: crate::UrbStatus,
        payload: &[u8],
        now: Instant,
    ) -> Result<Vec<HostPenBridgeOutput>, HidHostError> {
        let outputs = self.adapter.on_completion(urb, status, payload, now)?;
        self.apply(outputs, now)
    }

    /// Enforces the deadlines and ends a pen that left by going quiet; call
    /// it about every 100 ms.
    ///
    /// # Errors
    ///
    /// The adapter's error; the attachment cannot go on.
    pub fn poll(&mut self, now: Instant) -> Result<Vec<HostPenBridgeOutput>, HidHostError> {
        let outputs = self.adapter.poll(now)?;
        if let Some(sample) = self.reader.expire(now) {
            self.push(sample);
        }
        self.apply(outputs, now)
    }

    /// Every device of a [`HostPenBridgeOutput::Publish`] exists: start
    /// polling, and switch a pen whose reader needs it into the mode it
    /// reads, after reading the mode it is in so it can be put back.
    ///
    /// # Errors
    ///
    /// The adapter's error; the attachment cannot go on.
    pub fn devices_created(
        &mut self,
        now: Instant,
    ) -> Result<Vec<HostPenBridgeOutput>, HidHostError> {
        let mut outputs = self.adapter.devices_created(now)?;
        // The vendor driver switches the tablet out of HID mouse mode at
        // probe; the host reading the pen does it here. A generic pen
        // reports in its power-on mode and is never switched.
        if let Some(index) = self.index
            && self.mode == HostPenMode::Untouched
            && let Some((report_id, _)) = self.reader.mode_switch()
        {
            self.mode = HostPenMode::Reading;
            outputs.extend(self.adapter.request_report(
                READ_MODE,
                HidReportRequest::Get {
                    index,
                    kind: HidReportKind::Feature,
                    report_id,
                    length: 2,
                },
                now,
            ));
        }
        self.apply(outputs, now)
    }

    /// A report request from a published device. Refused for a token in the
    /// bridge's own range and for a device that is not published (the pen
    /// interface read here alone is never a device's to call).
    #[must_use]
    pub fn device_request(
        &mut self,
        token: u32,
        request: HidReportRequest,
        now: Instant,
    ) -> Vec<HostPenBridgeOutput> {
        let index = match &request {
            HidReportRequest::Get { index, .. } | HidReportRequest::Set { index, .. } => *index,
        };
        if token >= HOST_PEN_TOKENS || usize::from(index) >= self.published {
            return vec![HostPenBridgeOutput::DeviceReply {
                token,
                result: Err(HidReportFailure::UnknownDevice),
            }];
        }
        let outputs = self.adapter.request_report(token, request, now);
        // Report requests never fail the attachment.
        self.apply(outputs, now).unwrap_or_default()
    }

    /// The samples read since the last call, in order.
    pub fn take_samples(&mut self) -> Vec<HostPenSample> {
        std::mem::take(&mut self.samples).into()
    }

    /// Mode-switch results since the last call, for the host's log.
    pub fn take_notes(&mut self) -> Vec<HostPenNote> {
        std::mem::take(&mut self.notes)
    }

    /// What is still owed to the host when the attachment ends: with
    /// `release`, the waiting samples and a proximity exit for a pen left in
    /// proximity, so no press is stranded on the desktop; without (the
    /// desktop is no longer this session's), nothing.
    pub fn release(&mut self, release: bool) -> Vec<HostPenSample> {
        let mut samples: Vec<HostPenSample> = std::mem::take(&mut self.samples).into();
        if !release {
            return Vec::new();
        }
        if let Some(last) = samples.last().or(self.last.as_ref()).copied()
            && last.in_proximity
        {
            samples.push(HostPenSample {
                in_proximity: false,
                touching: false,
                pressure: 0.0,
                barrel_buttons: 0,
                ..last
            });
        }
        self.last = None;
        samples
    }

    /// Puts a pen the bridge switched back into the mode it read before, as
    /// the attachment ends while the Deck is still there: the outputs, and
    /// the request to leave running (not cancelled by [`Self::shutdown`]).
    /// Nothing when the mode was never switched, was not readable, or was
    /// already the reader's; the Deck's hand-back (a USB reset) restores the
    /// power-on mode in any case.
    pub fn restore_mode(&mut self, now: Instant) -> (Vec<HostPenBridgeOutput>, Option<UrbId>) {
        let (Some(index), HostPenMode::Switched(Some(original))) = (self.index, self.mode) else {
            return (Vec::new(), None);
        };
        if original == INTUOS_MODE_SWITCH.1[1] {
            return (Vec::new(), None);
        }
        let outputs = self.adapter.request_report(
            RESTORE_MODE,
            HidReportRequest::Set {
                index,
                kind: HidReportKind::Feature,
                report_id: INTUOS_MODE_SWITCH.0,
                data: vec![INTUOS_MODE_SWITCH.0, original],
            },
            now,
        );
        let keep = outputs.iter().find_map(|output| match output {
            HidHostOutput::Submit(urb) => Some(urb.urb_id),
            _ => None,
        });
        if keep.is_none() {
            return (Vec::new(), None);
        }
        // The restore is sent once; nothing to restore afterwards.
        self.mode = HostPenMode::Refused;
        match self.apply(outputs, now) {
            Ok(outputs) => (outputs, keep),
            Err(_) => (Vec::new(), None),
        }
    }

    /// Ends the attachment: the transfers to cancel, except `keep` (a mode
    /// restore left to finish). Later completions are ignored.
    #[must_use]
    pub fn shutdown(&mut self, keep: Option<UrbId>) -> Vec<UrbId> {
        self.adapter
            .shutdown()
            .into_iter()
            .filter_map(|output| match output {
                HidHostOutput::Cancel(urb) if Some(urb) != keep => Some(urb),
                _ => None,
            })
            .collect()
    }

    /// The attachment generation every URB belongs to.
    #[must_use]
    pub const fn generation(&self) -> AttachmentGeneration {
        self.adapter.generation()
    }

    /// Where the adapter is.
    #[must_use]
    pub const fn phase(&self) -> HidHostPhase {
        self.adapter.phase()
    }

    /// The adapter's counters.
    #[must_use]
    pub const fn stats(&self) -> HidHostStats {
        self.adapter.stats()
    }

    /// Pen reports read, reports that produced nothing, and samples lost to
    /// a host that fell [`HOST_PEN_BACKLOG`] behind.
    #[must_use]
    pub const fn pen_counts(&self) -> (u64, u64, u64) {
        let (read, ignored) = self.reader.counts();
        (read, ignored, self.dropped)
    }

    /// `dedicated` or `generic`.
    #[must_use]
    pub const fn reader_kind(&self) -> &'static str {
        self.reader.kind()
    }

    /// The plan index of the pen interface, once planned.
    #[must_use]
    pub const fn pen_index(&self) -> Option<u8> {
        self.index
    }

    /// Devices the platform publishes.
    #[must_use]
    pub const fn published(&self) -> usize {
        self.published
    }

    /// The mode switch so far.
    #[must_use]
    pub const fn mode(&self) -> HostPenMode {
        self.mode
    }

    fn push(&mut self, sample: HostPenSample) {
        if self.samples.len() >= HOST_PEN_BACKLOG {
            self.samples.pop_front();
            self.dropped += 1;
        }
        self.last = Some(sample);
        self.samples.push_back(sample);
    }

    fn apply(
        &mut self,
        outputs: Vec<HidHostOutput>,
        now: Instant,
    ) -> Result<Vec<HostPenBridgeOutput>, HidHostError> {
        let mut out = Vec::new();
        for output in outputs {
            match output {
                HidHostOutput::Submit(urb) => out.push(HostPenBridgeOutput::Submit(urb)),
                HidHostOutput::Cancel(urb) => out.push(HostPenBridgeOutput::Cancel(urb)),
                HidHostOutput::CreateDevices(plan) => {
                    self.index = self.adapter.host_read_index();
                    self.published = plan.len();
                    if plan.is_empty() {
                        out.extend(self.devices_created(now)?);
                    } else {
                        out.push(HostPenBridgeOutput::Publish(plan));
                    }
                }
                HidHostOutput::Report { index, report } if Some(index) == self.index => {
                    if let Some(sample) = self.reader.read(&report) {
                        self.push(sample);
                    }
                }
                HidHostOutput::Report { index, report } => {
                    out.push(HostPenBridgeOutput::DeviceReport { index, report });
                }
                HidHostOutput::ReportReply { token, result } if token >= HOST_PEN_TOKENS => {
                    out.extend(self.on_own_reply(token, result, now));
                }
                HidHostOutput::ReportReply { token, result } => {
                    out.push(HostPenBridgeOutput::DeviceReply { token, result });
                }
            }
        }
        Ok(out)
    }

    /// The answer to one of the bridge's own requests to the pen.
    fn on_own_reply(
        &mut self,
        token: u32,
        result: Result<Vec<u8>, HidReportFailure>,
        now: Instant,
    ) -> Vec<HostPenBridgeOutput> {
        match (token, self.mode, self.index) {
            (READ_MODE, HostPenMode::Reading, Some(index)) => {
                let original = result.as_deref().ok().and_then(read_pen_mode);
                self.notes.push(HostPenNote::ModeRead {
                    original,
                    failure: result.err(),
                });
                self.mode = HostPenMode::Switching(original);
                let (report_id, data) = INTUOS_MODE_SWITCH;
                let outputs = self.adapter.request_report(
                    SET_MODE,
                    HidReportRequest::Set {
                        index,
                        kind: HidReportKind::Feature,
                        report_id,
                        data: data.to_vec(),
                    },
                    now,
                );
                self.apply(outputs, now).unwrap_or_default()
            }
            (SET_MODE, HostPenMode::Switching(original), _) => {
                self.mode = if result.is_ok() {
                    HostPenMode::Switched(original)
                } else {
                    HostPenMode::Refused
                };
                self.notes.push(HostPenNote::ModeSwitched {
                    accepted: result.is_ok(),
                    failure: result.err(),
                });
                Vec::new()
            }
            _ => Vec::new(),
        }
    }
}

/// The mode a `GET_REPORT(Feature 2)` answer names: `[id, mode, ...]`.
#[must_use]
pub fn read_pen_mode(answer: &[u8]) -> Option<u8> {
    match answer {
        [id, mode, ..] if *id == INTUOS_MODE_SWITCH.0 => Some(*mode),
        _ => None,
    }
}

#[cfg(test)]
#[path = "host_pen_bridge_tests.rs"]
mod tests;
