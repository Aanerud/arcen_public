// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Native tablet on a Pier that reads the bridged pen itself and posts its
//! own OS pen events (the macOS Pier through tablet `CGEvent`s, the Windows
//! Pier through its synthetic Windows Ink pen; ADR 0022 §14, ADR 0023
//! amendment): everything but the OS calls.
//!
//! - [`authorize`]: whether the Deck's captured device may be presented,
//!   with the same vendor-neutral checks as the Linux Pier, and which reader
//!   reads its pen.
//! - [`tablet_result`] / [`ended_notice`]: what the Deck is told.
//! - [`BridgedPenSession`]: one attachment over the URB bridge, wire frames
//!   in and out ([`arcen_usb_bridge::HostPenBridge`] underneath).
//! - [`host_pen_event`]: a read sample as the same [`PenEventMsg`] a Deck
//!   sends in Tablet support, so one injector serves both modes.
//! - [`host_pen_sample`] / [`HostPenPointer`]: the host pointer the pen
//!   moves, through the one pen position path (ADR 0023), for a Deck that
//!   draws its cursor at host positions.
//!
//! A platform adapter checks that the session's desktop is still the one on
//! screen, hands frames to the Deck's bulk lane, injects the events, and
//! nothing more.

use crate::CapabilityAvailability;
use crate::pen_emulation::PenCapabilityTruth;
use crate::tablet_pointer::{AxisRange, PenMapping, PenPointerLead, PenSample, PenTool};
use arcen_protocol::messages::{
    MAX_TABLET_MODE_REASON_BYTES, PenEventMsg, PenToolMsg, TabletModeMsg, TabletModeReason,
    TabletModeResultMsg, UsbHardDeviceMsg,
};
use arcen_protocol::{
    FrameType, UsbUrbSubmitHeader, decode_usb_urb_complete, encode_usb_urb_cancel,
    encode_usb_urb_submit,
};
use arcen_usb_bridge::{
    AttachmentGeneration, BridgedHidDevice, HidPublishPolicy, HidReportFailure, HidReportRequest,
    HostPenBridge, HostPenBridgeOutput, HostPenNote, HostPenRoute, HostPenSample,
    InterfaceReportDescriptor, NoPenInterface, PenReportLayout, PenTabletProfile, PublishRefusal,
    UrbId, UsbDeviceId,
};
use std::time::{Duration, Instant};

/// How often a session polls its attachment ([`BridgedPenSession::poll`]).
pub const BRIDGED_PEN_POLL: Duration = Duration::from_millis(100);

/// A device a host agreed to present.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorizedTablet {
    pub identity: UsbDeviceId,
    /// Every authorized interface: the guard holds the device to all of
    /// them.
    pub report_descriptors: Vec<InterfaceReportDescriptor>,
    pub profile: PenTabletProfile,
    /// The interfaces the host's publish policy publishes.
    pub published: Vec<u8>,
    /// The interfaces it does not, and why.
    pub withheld: Vec<(u8, PublishRefusal)>,
    /// How the host reads the pen: a dedicated reader for a model it knows,
    /// otherwise the generic reader from the pen interface's own report
    /// descriptor.
    pub host_pen: HostPenRoute,
    /// The USB interface the pen is read on.
    pub pen_interface: u8,
}

/// Decides whether the Deck's captured device may be presented on a host
/// that publishes what `policy` allows and reads the pen itself, with the
/// same vendor-neutral checks as the Linux Pier
/// ([`arcen_usb_bridge::authorize_hard_usb_tablet`]). Vendor and product ids
/// are logged by the caller, never trusted. The synthetic lab tablet needs
/// `synthetic_enabled`. Any tablet by default (Q-16): a dedicated reader
/// where one exists, the generic reader otherwise; only a pen in a vendor
/// format no reader knows is refused, and the refusal says so.
///
/// # Errors
///
/// A bounded reason, in words a Deck shows its user.
pub fn authorize(
    device: Option<&UsbHardDeviceMsg>,
    policy: HidPublishPolicy,
    synthetic_enabled: bool,
) -> Result<AuthorizedTablet, String> {
    let device = device.ok_or_else(|| "the Deck did not capture a USB device".to_owned())?;
    let synthetic = device.vendor_id == arcen_usb_bridge::ARCEN_LAB_VENDOR_ID
        && device.product_id == arcen_usb_bridge::ARCEN_LAB_PRODUCT_ID;
    if synthetic && !synthetic_enabled {
        return Err("the synthetic lab tablet is not enabled on this host".to_owned());
    }
    let report_descriptors = device
        .report_descriptors()
        .map_err(|error| format!("the Deck's HID report descriptors are malformed: {error}"))?;
    let profile =
        arcen_usb_bridge::authorize_hard_usb_tablet(device.device_class, &report_descriptors)
            .map_err(|denial| format!("the captured USB device was refused: {denial}"))?;
    let (mut published, mut withheld) = (Vec::new(), Vec::new());
    for claim in &report_descriptors {
        match arcen_usb_bridge::publishable(policy, &claim.descriptor) {
            Ok(()) => published.push(claim.interface),
            Err(refusal) => withheld.push((claim.interface, refusal)),
        }
    }
    let identity = UsbDeviceId {
        vendor_id: device.vendor_id,
        product_id: device.product_id,
        bcd_device: device.bcd_device,
    };
    let host_pen = arcen_usb_bridge::host_pen_route(identity, &report_descriptors)
        .map_err(|refusal| format!("{refusal}; use Tablet support"))?;
    let pen_interface = host_pen
        .read_interface(&report_descriptors)
        .ok_or_else(|| format!("{NoPenInterface}; use Tablet support"))?;
    Ok(AuthorizedTablet {
        identity,
        report_descriptors,
        profile,
        published,
        withheld,
        host_pen,
        pen_interface,
    })
}

/// What a host tells a Deck whose tablet it accepted, by reader.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostPenNotices {
    /// A model with a dedicated reader.
    pub dedicated: &'static str,
    /// Any other pen, read by its own HID reports.
    pub generic: &'static str,
}

fn tablet_reason(text: &str) -> TabletModeReason {
    let mut text: String = text.chars().filter(|c| !c.is_control()).collect();
    while text.len() > MAX_TABLET_MODE_REASON_BYTES {
        text.pop();
    }
    TabletModeReason::try_from(text).unwrap_or_default()
}

/// The tablet-mode result for a Native tablet request, given whether this
/// host offers it (`offer`), what the Deck brought (`authorization`, `None`
/// when nothing was authorized) and the host's acceptance wording.
///
/// Refusals carry the precise reason rather than the generic one; an
/// accepted result says how the pen is read. Another requested mode is
/// returned unchanged.
#[must_use]
pub fn tablet_result(
    mut result: TabletModeResultMsg,
    offer: &Result<(), String>,
    authorization: Option<&Result<AuthorizedTablet, String>>,
    notices: HostPenNotices,
) -> TabletModeResultMsg {
    if result.requested != TabletModeMsg::UsbTabletBridge {
        return result;
    }
    match (offer, authorization) {
        (Err(why), _) => {
            if !result.accepted {
                result.reason =
                    tablet_reason(&format!("Native tablet (USB bridged) unavailable: {why}"));
            }
        }
        (Ok(()), Some(Err(why))) => {
            result.accepted = false;
            if result.active == TabletModeMsg::UsbTabletBridge {
                result.active = TabletModeMsg::DisabledMouseCompat;
            }
            result.reconnect_required = true;
            result.reason = tablet_reason(&format!(
                "Native tablet (USB bridged) refused: {why}. Use Tablet support instead."
            ));
        }
        (Ok(()), Some(Ok(tablet))) if result.accepted => {
            result.reason = tablet_reason(match tablet.host_pen {
                HostPenRoute::Dedicated(_) => notices.dedicated,
                HostPenRoute::Generic { .. } => notices.generic,
            });
        }
        _ => {}
    }
    result
}

/// What a pen read on the host brings to the desktop, axis by axis: what the
/// reader decodes from this tablet's reports, within what the host's pen
/// device (`device`, its own capability truth) carries. A generic reader
/// decodes only the fields the pen interface's descriptor declares; pad
/// buttons, touch and strips are never read, so they are never claimed.
#[must_use]
pub fn host_decoded_capabilities(
    tablet: &AuthorizedTablet,
    device: PenCapabilityTruth,
) -> PenCapabilityTruth {
    let availability = |present: bool| {
        if present {
            CapabilityAvailability::Available
        } else {
            CapabilityAvailability::Unavailable
        }
    };
    let decoded = match &tablet.host_pen {
        // The dedicated readers decode pressure, tilt, both side switches,
        // the eraser end and hover; the models they know report no barrel
        // rotation.
        HostPenRoute::Dedicated(_) => PenCapabilityTruth {
            pen: CapabilityAvailability::Available,
            pressure: CapabilityAvailability::Available,
            tilt: CapabilityAvailability::Available,
            rotation: CapabilityAvailability::Unavailable,
            eraser: CapabilityAvailability::Available,
            proximity: CapabilityAvailability::Available,
            barrel_buttons: CapabilityAvailability::Available,
        },
        HostPenRoute::Generic { layout, .. } => {
            let any = |field: fn(&PenReportLayout) -> bool| {
                availability(layout.reports.iter().any(field))
            };
            PenCapabilityTruth {
                pen: CapabilityAvailability::Available,
                pressure: any(|report| report.pressure.is_some()),
                tilt: any(|report| report.tilt_x.is_some() || report.tilt_y.is_some()),
                rotation: any(|report| report.twist.is_some()),
                eraser: any(|report| report.eraser.is_some() || report.invert.is_some()),
                proximity: any(|report| report.in_range.is_some()),
                barrel_buttons: any(|report| {
                    report.barrel.is_some() || report.secondary_barrel.is_some()
                }),
            }
        }
    };
    if device.pen != CapabilityAvailability::Available {
        return PenCapabilityTruth::UNAVAILABLE;
    }
    let both = |decoded: CapabilityAvailability, carried: CapabilityAvailability| {
        availability(
            decoded == CapabilityAvailability::Available
                && carried == CapabilityAvailability::Available,
        )
    };
    PenCapabilityTruth {
        pen: CapabilityAvailability::Available,
        pressure: both(decoded.pressure, device.pressure),
        tilt: both(decoded.tilt, device.tilt),
        rotation: both(decoded.rotation, device.rotation),
        eraser: both(decoded.eraser, device.eraser),
        proximity: both(decoded.proximity, device.proximity),
        barrel_buttons: both(decoded.barrel_buttons, device.barrel_buttons),
    }
}

/// The pen features `capabilities` delivers, in words, in a fixed order.
#[must_use]
pub fn pen_feature_words(capabilities: PenCapabilityTruth) -> Vec<&'static str> {
    [
        (capabilities.pressure, "pressure"),
        (capabilities.tilt, "tilt"),
        (capabilities.rotation, "barrel rotation"),
        (capabilities.eraser, "eraser"),
        (capabilities.proximity, "hover"),
        (capabilities.barrel_buttons, "barrel buttons"),
    ]
    .into_iter()
    .filter(|(availability, _)| *availability == CapabilityAvailability::Available)
    .map(|(_, word)| word)
    .collect()
}

/// What a host that reads the pen itself tells a Deck it accepted: how the
/// pen is read and exactly what reaches the desktop (`capabilities`, from
/// [`host_decoded_capabilities`]). `host` names the host ("Linux Pier").
#[must_use]
pub fn host_decoded_notice(
    tablet: &AuthorizedTablet,
    capabilities: PenCapabilityTruth,
    host: &str,
) -> String {
    let reader = match tablet.host_pen {
        HostPenRoute::Dedicated(_) => "with its model's reader",
        HostPenRoute::Generic { .. } => "from its HID reports",
    };
    let features = pen_feature_words(capabilities);
    let features = match features.split_last() {
        None => "position and tip only".to_owned(),
        Some((last, [])) => (*last).to_owned(),
        Some((last, rest)) => format!("{} and {last}", rest.join(", ")),
    };
    format!(
        "Native tablet is active: this {host} reads the pen {reader} and gives your desktop \
         {features}. Pad buttons, touch and the maker's driver settings are not used."
    )
}

/// The tablet-mode result for a Native tablet request on a host whose
/// host-decoded route was admitted (the Linux Pier on a joined console,
/// `arcen_usb_bridge::joined_pen`): a refused device keeps mouse
/// compatibility and says why; an accepted one says how the pen is read and
/// exactly what reaches the desktop ([`host_decoded_notice`] over
/// [`host_decoded_capabilities`] with the host's pen device, `device`).
#[must_use]
pub fn host_decoded_tablet_result(
    result: TabletModeResultMsg,
    authorization: Option<&Result<AuthorizedTablet, String>>,
    device: PenCapabilityTruth,
    host: &str,
) -> TabletModeResultMsg {
    let notices = HostPenNotices {
        dedicated: "",
        generic: "",
    };
    let mut result = tablet_result(result, &Ok(()), authorization, notices);
    if result.accepted && result.active == TabletModeMsg::UsbTabletBridge {
        if let Some(Ok(tablet)) = authorization {
            let capabilities = host_decoded_capabilities(tablet, device);
            result.reason = tablet_reason(&host_decoded_notice(tablet, capabilities, host));
        }
    }
    result
}

/// The tablet-mode result (JSON) that tells the Deck Native tablet ended for
/// `reason`, so it gives the tablet back to its own computer.
#[must_use]
pub fn ended_notice(result: &TabletModeResultMsg, reason: &str) -> Option<String> {
    let mut result = result.clone();
    result.accepted = false;
    result.active = TabletModeMsg::DisabledMouseCompat;
    result.reconnect_required = true;
    result.reason = tablet_reason(&format!("Native tablet (USB bridged) ended: {reason}"));
    serde_json::to_string(&result).ok()
}

/// The tablet event for one sample a host read, in the same message the
/// Deck sends in Tablet support, so one injector serves both.
#[must_use]
pub fn host_pen_event(sample: &HostPenSample) -> PenEventMsg {
    PenEventMsg {
        x: sample.x.clamp(0.0, 1.0),
        y: sample.y.clamp(0.0, 1.0),
        pressure: sample.pressure.clamp(0.0, 1.0),
        tilt_x_degrees: sample.tilt_x_degrees.clamp(-90.0, 90.0),
        tilt_y_degrees: sample.tilt_y_degrees.clamp(-90.0, 90.0),
        rotation_degrees: sample.rotation_degrees.clamp(0.0, 360.0),
        tool: if sample.eraser {
            PenToolMsg::Eraser
        } else {
            PenToolMsg::Tip
        },
        in_proximity: sample.in_proximity,
        touching: sample.touching,
        buttons: sample.barrel_buttons,
        ..PenEventMsg::default()
    }
}

/// The shared pen sample (ADR 0023) for one read: the reader's device units
/// over the pen's whole range, the tool, and when it was read. The injector
/// and the position use the same scaling.
#[must_use]
pub fn host_pen_sample(sample: &HostPenSample, at: Instant) -> PenSample {
    let axis = |maximum: u32| AxisRange {
        minimum: 0,
        maximum: i32::try_from(maximum).unwrap_or(i32::MAX),
    };
    let unit = |value: u32| i32::try_from(value).unwrap_or(i32::MAX);
    PenSample {
        tool: sample.in_proximity.then_some(if sample.eraser {
            PenTool::Eraser
        } else {
            PenTool::Pen
        }),
        x: unit(sample.device_x),
        y: unit(sample.device_y),
        x_range: axis(sample.device_max_x),
        y_range: axis(sample.device_max_y),
        at,
    }
}

/// The host pointer a pen read on the host moves, reported to a Deck that
/// draws its cursor at host positions (`cursor_position_v1`).
///
/// A bridged tablet moves only the host's pointer, so without these records
/// the person drawing sees no cursor on the Deck. The pen events the host
/// posts land on the captured display at the samples' own fractions, so the
/// position is known without reading the pointer back. Glue only: which
/// source leads and where the pen points are the shared [`PenPointerLead`]'s,
/// echo suppression, rate and the record the shared
/// [`crate::host_pointer::HostPointerFeed`]'s.
#[derive(Debug)]
pub struct HostPenPointer {
    lead: PenPointerLead,
    feed: crate::host_pointer::HostPointerFeed,
}

impl HostPenPointer {
    /// A reporter for a single captured display of `width` × `height`
    /// pixels; `None` for a size a record cannot carry.
    #[must_use]
    pub fn new(width: usize, height: usize) -> Option<Self> {
        let width = u16::try_from(width).ok().filter(|width| *width > 0)?;
        let height = u16::try_from(height).ok().filter(|height| *height > 0)?;
        let mut lead = PenPointerLead::new(PenMapping::WholeArea);
        let _ = lead.set_desktop_size(u32::from(width), u32::from(height));
        Some(Self {
            lead,
            feed: crate::host_pointer::HostPointerFeed::new(Some(
                crate::host_pointer::HostPointerLayout::single(
                    u32::from(width),
                    u32::from(height),
                    width,
                    height,
                ),
            )),
        })
    }

    /// The pen samples the host just read, in order.
    pub fn observe(&mut self, samples: &[PenSample]) {
        for sample in samples {
            if let Some(position) = self.lead.pen(*sample) {
                self.feed.observe(&position);
            }
        }
    }

    /// When the waiting record may be sent; `None` when none waits.
    #[must_use]
    pub fn due_at(&self) -> Option<Instant> {
        self.feed.due_at()
    }

    /// The record to send now, if one waits and the rate allows it; it
    /// counts as sent.
    pub fn take_record(
        &mut self,
        now: Instant,
    ) -> Option<[u8; arcen_protocol::cursor_position::CURSOR_POSITION_SIZE]> {
        self.feed.take_record(now)
    }

    /// Records sent so far.
    #[must_use]
    pub const fn reports(&self) -> u64 {
        self.feed.reports()
    }
}

/// A generation no earlier attachment of this or a previous agent used:
/// the wall clock (`since_epoch`) mixed with the process's own attachment
/// counter (`arcen_transport::quic::next_attachment_generation`), never
/// zero.
#[must_use]
pub fn fresh_generation(since_epoch: Duration, counter: u64) -> AttachmentGeneration {
    let nanos = u64::try_from(since_epoch.as_nanos()).unwrap_or(u64::MAX);
    let value = std::num::NonZeroU64::new(nanos.wrapping_add(counter) | 1)
        .unwrap_or(std::num::NonZeroU64::MIN);
    AttachmentGeneration::new(value)
}

/// What a session must do after one attachment step.
#[derive(Debug, Default)]
pub struct BridgedPenStep {
    /// Wire frames for the Deck's bulk lane, in order.
    pub frames: Vec<Vec<u8>>,
    /// Tablet events read from the pen, for the host's injector, in order.
    pub pens: Vec<PenEventMsg>,
    /// The same reads as shared pen samples, for the pointer they move
    /// ([`HostPenPointer`]).
    pub positions: Vec<PenSample>,
    /// HID devices to publish, then [`BridgedPenSession::devices_created`]
    /// (a host whose policy publishes nothing never gets any).
    pub publish: Option<Vec<BridgedHidDevice>>,
    /// Input reports for published devices.
    pub device_reports: Vec<(u8, Vec<u8>)>,
    /// Answers to published devices' report requests.
    pub device_replies: Vec<(u32, Result<Vec<u8>, HidReportFailure>)>,
    /// Mode-switch results worth a log line.
    pub notes: Vec<HostPenNote>,
}

/// One Native tablet attachment as wire frames: the Deck's URB completions
/// in, URB submissions and cancellations out, the pen read in between.
#[derive(Debug)]
pub struct BridgedPenSession {
    bridge: HostPenBridge,
}

impl BridgedPenSession {
    /// Starts enumerating `tablet` for a host publishing what `policy`
    /// allows; returns the first frames for the Deck.
    ///
    /// # Errors
    ///
    /// Why the attachment could not start.
    pub fn start(
        tablet: &AuthorizedTablet,
        policy: HidPublishPolicy,
        generation: AttachmentGeneration,
        now: Instant,
    ) -> Result<(Self, BridgedPenStep), String> {
        let bridge = HostPenBridge::new(
            tablet.identity,
            tablet.report_descriptors.clone(),
            &tablet.host_pen,
            policy,
            generation,
        )
        .map_err(|error| error.to_string())?;
        let mut session = Self { bridge };
        let outputs = session
            .bridge
            .start(now)
            .map_err(|error| error.to_string())?;
        let step = session.step(outputs, now)?;
        Ok((session, step))
    }

    /// Whether `bytes` is a USB completion an attachment handles.
    #[must_use]
    pub fn takes(bytes: &[u8]) -> bool {
        bytes.first() == Some(&(FrameType::UsbBridgeUrbComplete as u8))
    }

    /// One `UsbBridgeUrbComplete` frame from the Deck.
    ///
    /// # Errors
    ///
    /// Why the attachment ended.
    pub fn on_deck_frame(&mut self, bytes: &[u8], now: Instant) -> Result<BridgedPenStep, String> {
        let (header, payload) = decode_usb_urb_complete(bytes)
            .map_err(|error| format!("malformed USB completion: {error:?}"))?;
        if header.generation != self.bridge.generation() {
            return Err("a USB completion for another attachment".to_owned());
        }
        let outputs = self
            .bridge
            .on_completion(header.urb_id, header.status, payload, now)
            .map_err(|error| error.to_string())?;
        self.step(outputs, now)
    }

    /// Deadlines and a pen that left by going quiet; call it every
    /// [`BRIDGED_PEN_POLL`].
    ///
    /// # Errors
    ///
    /// Why the attachment ended.
    pub fn poll(&mut self, now: Instant) -> Result<BridgedPenStep, String> {
        let outputs = self.bridge.poll(now).map_err(|error| error.to_string())?;
        self.step(outputs, now)
    }

    /// The host published every device of [`BridgedPenStep::publish`].
    ///
    /// # Errors
    ///
    /// Why the attachment ended.
    pub fn devices_created(&mut self, now: Instant) -> Result<BridgedPenStep, String> {
        let outputs = self
            .bridge
            .devices_created(now)
            .map_err(|error| error.to_string())?;
        self.step(outputs, now)
    }

    /// A report request from a published device.
    ///
    /// # Errors
    ///
    /// Why the attachment ended.
    pub fn device_request(
        &mut self,
        token: u32,
        request: HidReportRequest,
        now: Instant,
    ) -> Result<BridgedPenStep, String> {
        let outputs = self.bridge.device_request(token, request, now);
        self.step(outputs, now)
    }

    /// Ends the attachment while the Deck is still there: the frames to send
    /// (a mode restore, then the cancellations) and, with `release`, the
    /// events still owed to the injector, ending with a proximity exit for a
    /// pen left in proximity. Without `release` (the desktop is no longer
    /// this session's) no event at all. Calling it again yields nothing.
    pub fn end(&mut self, release: bool, now: Instant) -> (Vec<Vec<u8>>, Vec<PenEventMsg>) {
        let pens = self
            .bridge
            .release(release)
            .iter()
            .map(host_pen_event)
            .collect();
        let (outputs, keep) = self.bridge.restore_mode(now);
        let mut frames = self
            .step(outputs, now)
            .map(|step| step.frames)
            .unwrap_or_default();
        frames.extend(self.cancel_frames(keep));
        (frames, pens)
    }

    /// The cancellations of an attachment dropped without [`Self::end`].
    #[must_use]
    pub fn shutdown(&mut self) -> Vec<Vec<u8>> {
        self.cancel_frames(None)
    }

    /// The bridge, for counters and log lines.
    #[must_use]
    pub const fn bridge(&self) -> &HostPenBridge {
        &self.bridge
    }

    fn cancel_frames(&mut self, keep: Option<UrbId>) -> Vec<Vec<u8>> {
        let generation = self.bridge.generation();
        self.bridge
            .shutdown(keep)
            .into_iter()
            .map(|urb| encode_usb_urb_cancel(generation, urb).to_vec())
            .collect()
    }

    fn step(
        &mut self,
        outputs: Vec<HostPenBridgeOutput>,
        now: Instant,
    ) -> Result<BridgedPenStep, String> {
        let generation = self.bridge.generation();
        let mut step = BridgedPenStep::default();
        for output in outputs {
            match output {
                HostPenBridgeOutput::Submit(urb) => step.frames.push(
                    encode_usb_urb_submit(
                        UsbUrbSubmitHeader {
                            generation,
                            urb_id: urb.urb_id,
                            endpoint: urb.endpoint,
                            transfer_kind: urb.transfer_kind,
                            timeout_ms: urb.timeout_ms,
                            declared_length: urb.declared_length,
                            setup: urb.setup,
                        },
                        &urb.payload,
                    )
                    .map_err(|error| format!("encode a USB request: {error:?}"))?,
                ),
                HostPenBridgeOutput::Cancel(urb) => {
                    step.frames
                        .push(encode_usb_urb_cancel(generation, urb).to_vec());
                }
                HostPenBridgeOutput::Publish(plan) => step.publish = Some(plan),
                HostPenBridgeOutput::DeviceReport { index, report } => {
                    step.device_reports.push((index, report));
                }
                HostPenBridgeOutput::DeviceReply { token, result } => {
                    step.device_replies.push((token, result));
                }
            }
        }
        for sample in self.bridge.take_samples() {
            step.pens.push(host_pen_event(&sample));
            step.positions.push(host_pen_sample(&sample, now));
        }
        step.notes = self.bridge.take_notes();
        Ok(step)
    }
}

/// Runs `step`, turning a panic into an error: a fault in the Native tablet
/// code ends Native tablet, never the session that streams the desktop.
///
/// # Errors
///
/// `step`'s own error, or the panic as one.
pub fn contained<T>(step: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(step))
        .unwrap_or_else(|_| Err("an internal fault in the Native tablet adapter".to_owned()))
}

/// Counters of an attachment, for the host's closing log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeTabletCounts {
    pub stats: arcen_usb_bridge::HidHostStats,
    /// Pen reports read.
    pub pen_packets: u64,
    /// Pen reports that produced no sample.
    pub pen_ignored: u64,
    /// Samples lost to a host that fell behind.
    pub pen_dropped: u64,
    /// How long the attachment ran.
    pub elapsed: Duration,
}

/// What a session must send, inject and log after one Native tablet step.
#[derive(Debug, Default)]
pub struct NativeTabletOutput {
    /// Wire frames for the Deck's bulk lane, in order.
    pub frames: Vec<Vec<u8>>,
    /// The tablet-mode result (JSON) that tells the Deck Native tablet
    /// ended; the session goes on.
    pub notice: Option<String>,
    /// Pen events for the host's injector, in order.
    pub pens: Vec<PenEventMsg>,
    /// The same reads as shared pen samples, for the host pointer.
    pub positions: Vec<PenSample>,
    /// HID devices to publish (a host whose policy publishes nothing never
    /// gets any).
    pub publish: Option<Vec<BridgedHidDevice>>,
    /// Input reports for published devices.
    pub device_reports: Vec<(u8, Vec<u8>)>,
    /// Answers to published devices' report requests.
    pub device_replies: Vec<(u32, Result<Vec<u8>, HidReportFailure>)>,
    /// Mode-switch results worth a log line.
    pub notes: Vec<HostPenNote>,
    /// Why the attachment ended, and its counters, when it ended in this
    /// step.
    pub ended: Option<(String, NativeTabletCounts)>,
}

impl From<BridgedPenStep> for NativeTabletOutput {
    fn from(step: BridgedPenStep) -> Self {
        Self {
            frames: step.frames,
            pens: step.pens,
            positions: step.positions,
            publish: step.publish,
            device_reports: step.device_reports,
            device_replies: step.device_replies,
            notes: step.notes,
            ..Self::default()
        }
    }
}

/// Native tablet inside one streaming session: inactive unless the
/// handshake accepted it; ends itself (and tells the Deck) on any failure,
/// while the session goes on. A panic in the attachment ends Native tablet,
/// never the session.
#[derive(Debug)]
pub struct NativeTabletAttachment {
    session: Option<BridgedPenSession>,
    result: TabletModeResultMsg,
    started: Instant,
}

impl NativeTabletAttachment {
    /// Starts reading `tablet`, if any, for a host publishing what `policy`
    /// allows; `result` is the tablet-mode result the Deck was sent.
    #[must_use]
    pub fn start(
        tablet: Option<&AuthorizedTablet>,
        result: &TabletModeResultMsg,
        policy: HidPublishPolicy,
        generation: AttachmentGeneration,
        now: Instant,
    ) -> (Self, NativeTabletOutput) {
        let mut attachment = Self {
            session: None,
            result: result.clone(),
            started: now,
        };
        let Some(tablet) = tablet else {
            return (attachment, NativeTabletOutput::default());
        };
        match contained(|| BridgedPenSession::start(tablet, policy, generation, now)) {
            Ok((session, step)) => {
                attachment.session = Some(session);
                (attachment, step.into())
            }
            Err(reason) => {
                let output = attachment.ended_output(reason, None);
                (attachment, output)
            }
        }
    }

    /// Whether an attachment is running.
    #[must_use]
    pub const fn is_active(&self) -> bool {
        self.session.is_some()
    }

    /// Whether `bytes` is a USB completion this session must handle.
    #[must_use]
    pub fn takes(&self, bytes: &[u8]) -> bool {
        self.is_active() && BridgedPenSession::takes(bytes)
    }

    /// One USB completion from the Deck.
    #[must_use]
    pub fn on_deck_frame(&mut self, bytes: &[u8], now: Instant) -> NativeTabletOutput {
        self.run(now, |session| session.on_deck_frame(bytes, now))
    }

    /// Deadlines and a pen that left by going quiet; call it every
    /// [`BRIDGED_PEN_POLL`] while active.
    #[must_use]
    pub fn poll(&mut self, now: Instant) -> NativeTabletOutput {
        self.run(now, |session| session.poll(now))
    }

    /// The host published every device of [`NativeTabletOutput::publish`].
    #[must_use]
    pub fn devices_created(&mut self, now: Instant) -> NativeTabletOutput {
        self.run(now, |session| session.devices_created(now))
    }

    /// A report request from a published device.
    #[must_use]
    pub fn device_request(
        &mut self,
        token: u32,
        request: HidReportRequest,
        now: Instant,
    ) -> NativeTabletOutput {
        self.run(now, |session| session.device_request(token, request, now))
    }

    /// Ends the attachment for `reason`. With `release` a pen left in
    /// proximity is lifted, so no press is stranded on the desktop; without
    /// (the desktop on screen is no longer this session's) no event at all.
    /// The pen's mode is put back and the transfers cancelled either way:
    /// that touches only the Deck's tablet. The Deck is told.
    #[must_use]
    pub fn end(&mut self, reason: &str, release: bool, now: Instant) -> NativeTabletOutput {
        let Some(mut session) = self.session.take() else {
            return NativeTabletOutput::default();
        };
        let (frames, pens) = contained(|| Ok(session.end(release, now))).unwrap_or_default();
        let counts = self.counts(&session);
        NativeTabletOutput {
            frames,
            pens,
            ..self.ended_output(reason.to_owned(), Some(counts))
        }
    }

    /// Stops without telling the Deck (its link is going away): the
    /// counters, when an attachment was running.
    pub fn shutdown(&mut self) -> Option<NativeTabletCounts> {
        let mut session = self.session.take()?;
        let _ = contained(|| Ok(session.shutdown()));
        Some(self.counts(&session))
    }

    fn run(
        &mut self,
        now: Instant,
        step: impl FnOnce(&mut BridgedPenSession) -> Result<BridgedPenStep, String>,
    ) -> NativeTabletOutput {
        let Some(session) = self.session.as_mut() else {
            return NativeTabletOutput::default();
        };
        match contained(|| step(session)) {
            Ok(step) => step.into(),
            Err(reason) => self.end(&reason, true, now),
        }
    }

    fn counts(&self, session: &BridgedPenSession) -> NativeTabletCounts {
        let bridge = session.bridge();
        let (pen_packets, pen_ignored, pen_dropped) = bridge.pen_counts();
        NativeTabletCounts {
            stats: bridge.stats(),
            pen_packets,
            pen_ignored,
            pen_dropped,
            elapsed: self.started.elapsed(),
        }
    }

    fn ended_output(
        &self,
        reason: String,
        counts: Option<NativeTabletCounts>,
    ) -> NativeTabletOutput {
        let counts = counts.unwrap_or(NativeTabletCounts {
            stats: arcen_usb_bridge::HidHostStats::default(),
            pen_packets: 0,
            pen_ignored: 0,
            pen_dropped: 0,
            elapsed: self.started.elapsed(),
        });
        NativeTabletOutput {
            notice: ended_notice(&self.result, &reason),
            ended: Some((reason, counts)),
            ..NativeTabletOutput::default()
        }
    }
}

#[cfg(test)]
#[path = "bridged_pen_tests.rs"]
mod tests;
