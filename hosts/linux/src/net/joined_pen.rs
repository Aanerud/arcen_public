// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Native tablet on a joined Xorg console (ADR 0025): the host-decoded
//! route.
//!
//! A joined console's X server is not one Arcen starts, so the raw USB
//! bridge stays refused there (`arcen_usb_bridge::admit_hard_usb_session`).
//! Instead the Pier reads the Deck's tablet over the URB bridge itself
//! (`arcen_input::bridged_pen` with `HidPublishPolicy::Nothing`: no USB or
//! HID device appears on this host) and posts every sample through the
//! session's own uinput tablet-tool device, the one Tablet support uses,
//! named for the joined display. That device is offered to every reader on
//! `seat0`, so the route is taken only while the shared proof
//! (`arcen_usb_bridge::joined_pen`) holds over the facts
//! `input::joined_devices` reads: the joined desktop's X server has the
//! device open, nothing else but logind has any of its nodes open, and the
//! joined session is in front. The proof is repeated every
//! [`RECHECK`] while the pen is read; a gap ends Native tablet without
//! another event, and the session goes on with mouse compatibility.
//!
//! This module is glue: the decisions, the reader, the wording and the
//! attachment are shared.

use std::time::{Duration, Instant};

use arcen_input::bridged_pen::{AuthorizedTablet, NativeTabletAttachment, NativeTabletOutput};
use arcen_input::joined_input::JoinedDeviceGap;
use arcen_input::pen_emulation::PenCapabilityTruth;
use arcen_protocol::messages::{
    PenEventMsg, TabletModeMsg, TabletModeReason, TabletModeResultMsg, UsbHardDeviceMsg,
};
use arcen_usb_bridge::{HardUsbSessionSeat, HidPublishPolicy};

use crate::input::joined_devices::{self, DeviceNode, JoinedDesktop};
use crate::input::PierInput;
use crate::logging::target::INPUT;

/// This Pier publishes no device on the host-decoded route.
pub(crate) const POLICY: HidPublishPolicy = HidPublishPolicy::Nothing;
/// How this Pier names itself to a Deck whose pen it reads.
pub(crate) const HOST: &str = "Linux Pier";
/// How often the confinement proof is repeated while the pen is read.
pub(crate) const RECHECK: Duration = Duration::from_secs(2);
/// How long the handshake waits for the joined X server to add the device.
const SETTLE: Duration = Duration::from_secs(3);

/// What the handshake settled for a joined console's Native tablet request.
#[derive(Debug)]
pub(crate) struct JoinedPenOffer {
    /// Why the route is refused, in the Deck's words; `None` when admitted.
    pub refusal: Option<String>,
    /// The Deck's device, authorized for a host that publishes nothing;
    /// `None` when the route was refused first.
    pub authorization: Option<Result<AuthorizedTablet, String>>,
    nodes: Vec<DeviceNode>,
    joined: Option<JoinedDesktop>,
}

impl JoinedPenOffer {
    /// Whether this host offers the host-decoded route to this attachment.
    pub(crate) const fn admitted(&self) -> bool {
        self.refusal.is_none()
    }

    fn refused(reason: String) -> Self {
        Self {
            refusal: Some(reason),
            authorization: None,
            nodes: Vec::new(),
            joined: None,
        }
    }
}

/// The joined desktop a session's metadata names: its X display and its
/// logind session.
pub(crate) fn joined_desktop(
    metadata: &crate::session::lifecycle::SessionMetadata,
) -> Option<JoinedDesktop> {
    // The display the typed devices are named for (any local display).
    let identity =
        arcen_input::virtual_device::SessionInputIdentity::for_joined_console(&metadata.display)
            .ok()?;
    Some(JoinedDesktop {
        display: identity.display_number(),
        session_id: metadata.session_id.clone(),
        uid: metadata.uid,
    })
}

/// Settles a joined console's Native tablet request: enabled on this host,
/// the Deck's device authorized (a refused device keeps the route offered,
/// so the Deck hears the device's own reason), and the typed pen device
/// proved to reach the joined desktop's X server alone.
#[allow(
    clippy::disallowed_methods,
    reason = "plain /proc, sysfs and logind file reads that finish (bounded by SETTLE); awaited to completion"
)]
pub(crate) async fn offer(
    input: Option<&mut PierInput>,
    metadata: &crate::session::lifecycle::SessionMetadata,
    device: Option<&UsbHardDeviceMsg>,
) -> JoinedPenOffer {
    if let Some(reason) = crate::usb_bridge::host_decoded_unavailable_reason() {
        return JoinedPenOffer::refused(format!(
            "{}. The pen works as a mouse.",
            reason.trim_end_matches('.')
        ));
    }
    let synthetic = std::env::var("ARCEN_USB_HARD_SYNTHETIC").ok().as_deref() == Some("1");
    let authorization = arcen_input::bridged_pen::authorize(device, POLICY, synthetic);
    if authorization.is_err() {
        return JoinedPenOffer {
            refusal: None,
            authorization: Some(authorization),
            nodes: Vec::new(),
            joined: None,
        };
    }
    let seat = crate::usb_bridge::session_seat(Some(metadata));
    let nodes = match input.and_then(PierInput::tablet_nodes) {
        Some(Ok(nodes)) => Ok(nodes),
        Some(Err(error)) => {
            tracing::warn!(target: INPUT, %error, "the typed pen device's nodes could not be read");
            Err(JoinedDeviceGap::NoNodes)
        }
        None => Err(JoinedDeviceGap::NoDevice),
    };
    let joined = joined_desktop(metadata);
    let proof = match (nodes.as_ref(), joined.as_ref()) {
        (Err(gap), _) => Err(gap.clone()),
        (Ok(_), None) => Err(JoinedDeviceGap::NoJoinedServer),
        (Ok(nodes), Some(joined)) => {
            let (nodes, joined) = (nodes.clone(), joined.clone());
            match tokio::task::spawn_blocking(move || {
                joined_devices::prove_confined(&nodes, &joined, SETTLE)
            })
            .await
            {
                Ok(Ok(facts)) => {
                    tracing::info!(
                        target: INPUT,
                        readers = %joined_devices::readers_summary(&facts),
                        "Native tablet: the typed pen device reaches the joined desktop's X server alone"
                    );
                    Ok(())
                }
                Ok(Err(failed)) => {
                    let (gap, facts) = *failed;
                    tracing::warn!(
                        target: INPUT,
                        %gap,
                        readers = %joined_devices::readers_summary(&facts),
                        joined_server = facts.joined_server,
                        joined_in_front = facts.joined_in_front,
                        "Native tablet: the typed pen device is not proved confined to the joined desktop"
                    );
                    Err(gap)
                }
                Err(error) => {
                    tracing::warn!(target: INPUT, %error, "the typed pen confinement proof did not finish");
                    Err(JoinedDeviceGap::NoJoinedServer)
                }
            }
        }
    };
    match arcen_usb_bridge::native_tablet_route(seat, &proof.map_err(|gap| gap.pen_refusal())) {
        Ok(route) => {
            tracing::info!(target: INPUT, route = route.token(), "Native tablet route");
            JoinedPenOffer {
                refusal: None,
                authorization: Some(authorization),
                nodes: nodes.unwrap_or_default(),
                joined,
            }
        }
        Err(denial) => JoinedPenOffer::refused(denial.reason().to_owned()),
    }
}

/// Whether a joined console's hello offers Native tablet, so a Deck
/// captures its tablet and asks: the route is enabled on this host and the
/// session's typed pen device really exists (`pen`, its capability truth).
/// Whether that device reaches the joined desktop alone is proved when a
/// Deck asks ([`offer`]), and a refusal then says why.
pub(crate) fn offered(
    session: Option<&crate::session::lifecycle::SessionMetadata>,
    pen: PenCapabilityTruth,
) -> bool {
    offered_on(
        crate::usb_bridge::session_seat(session),
        crate::usb_bridge::host_decoded_unavailable_reason().as_deref(),
        pen,
    )
}

fn offered_on(
    seat: HardUsbSessionSeat,
    unavailable: Option<&str>,
    pen: PenCapabilityTruth,
) -> bool {
    seat == HardUsbSessionSeat::JoinedConsole && unavailable.is_none() && pen.pen_available()
}

/// Whether a session on `seat` asking for `requested` takes the joined
/// console's route.
pub(crate) fn applies(seat: HardUsbSessionSeat, requested: TabletModeMsg) -> bool {
    seat == HardUsbSessionSeat::JoinedConsole && requested == TabletModeMsg::UsbTabletBridge
}

/// The tablet-mode result for a joined console, from the generic one
/// (`result`, negotiated with the route's availability) and the offer;
/// with the plan to start when Native tablet is active. `device` is the
/// typed pen device's own capability truth.
pub(crate) fn settle(
    result: TabletModeResultMsg,
    offer: JoinedPenOffer,
    device: PenCapabilityTruth,
) -> (TabletModeResultMsg, Option<JoinedPenPlan>) {
    if let Some(refusal) = offer.refusal {
        let mut result = result;
        if !result.accepted {
            result.reason = TabletModeReason::try_from(refusal).unwrap_or_default();
        }
        return (result, None);
    }
    let result = arcen_input::bridged_pen::host_decoded_tablet_result(
        result,
        offer.authorization.as_ref(),
        device,
        HOST,
    );
    let active = result.accepted && result.active == TabletModeMsg::UsbTabletBridge;
    let plan = match (active, offer.authorization, offer.joined) {
        (true, Some(Ok(tablet)), Some(joined)) => Some(JoinedPenPlan {
            tablet,
            nodes: offer.nodes,
            joined,
        }),
        _ => None,
    };
    (result, plan)
}

/// What a joined console's active Native tablet reads and where it goes.
#[derive(Debug)]
pub(crate) struct JoinedPenPlan {
    tablet: AuthorizedTablet,
    nodes: Vec<DeviceNode>,
    joined: JoinedDesktop,
}

/// Native tablet on a joined console, inside one attachment.
#[derive(Debug)]
pub(crate) struct JoinedNativePen {
    attachment: NativeTabletAttachment,
    nodes: Vec<DeviceNode>,
    joined: JoinedDesktop,
    /// Whether the joined session is in front now
    /// ([`joined_devices::joined_in_front`]).
    in_front: fn(&JoinedDesktop) -> bool,
    /// The last pen event handed out for posting: what a lift starts from.
    last_pen: Option<PenEventMsg>,
    /// Native tablet ended while the session was not in front with the pen
    /// in proximity: the lift is posted once the session is back in front
    /// (or the session's pen device releases it when the attachment ends).
    lift_owed: bool,
    /// The joined console's input gate (`net::joined_input`): when it
    /// withdraws the pen, Native tablet ends.
    pen_gate: Option<std::sync::Arc<crate::input::joined_devices::JoinedInputGate>>,
}

impl JoinedNativePen {
    /// Starts reading the plan's tablet; `result` is what the Deck was sent.
    pub(crate) fn start(
        plan: JoinedPenPlan,
        result: &TabletModeResultMsg,
    ) -> (Self, NativeTabletOutput) {
        let since_epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let generation = arcen_input::bridged_pen::fresh_generation(
            since_epoch,
            arcen_transport::quic::next_attachment_generation().get(),
        );
        tracing::info!(
            target: INPUT,
            generation = generation.get(),
            vendor_id = plan.tablet.identity.vendor_id,
            product_id = plan.tablet.identity.product_id,
            pen_reader = plan.tablet.host_pen.kind(),
            pen_interface = plan.tablet.pen_interface,
            display = plan.joined.display,
            "Native tablet: reading the bridged pen on this Pier and posting it through the \
             joined desktop's typed pen device; nothing is published on the host"
        );
        let (attachment, output) = NativeTabletAttachment::start(
            Some(&plan.tablet),
            result,
            POLICY,
            generation,
            Instant::now(),
        );
        (
            Self {
                attachment,
                nodes: plan.nodes,
                joined: plan.joined,
                in_front: joined_devices::joined_in_front,
                last_pen: None,
                lift_owed: false,
                pen_gate: None,
            },
            output,
        )
    }

    pub(crate) const fn is_active(&self) -> bool {
        self.attachment.is_active()
    }

    pub(crate) fn takes(&self, bytes: &[u8]) -> bool {
        self.attachment.takes(bytes)
    }

    /// Follows the joined console's input gate.
    pub(crate) fn follow_gate(
        &mut self,
        gate: std::sync::Arc<crate::input::joined_devices::JoinedInputGate>,
    ) {
        self.pen_gate = Some(gate);
    }

    /// Ends Native tablet when the joined console's input guard withdrew the
    /// pen. Nothing is posted and no lift is owed: the guard destroyed the
    /// pen device, which released what it held.
    fn withdrawn_or(&mut self, output: NativeTabletOutput) -> NativeTabletOutput {
        let withdrawn = self.pen_gate.as_ref().is_some_and(|gate| {
            !gate.admitted(arcen_input::virtual_device::VirtualInputRole::TabletPen)
        });
        if !withdrawn || !self.is_active() {
            return output;
        }
        tracing::warn!(
            target: INPUT,
            "Native tablet: the joined console's input guard withdrew the pen"
        );
        let mut ended = self.end(
            arcen_usb_bridge::JoinedPenRefusal::NotConfined
                .denial()
                .reason(),
            false,
        );
        ended.pens.clear();
        self.lift_owed = false;
        let mut frames = output.frames;
        frames.append(&mut ended.frames);
        ended.frames = frames;
        ended
    }

    pub(crate) fn on_deck_frame(&mut self, bytes: &[u8]) -> NativeTabletOutput {
        let output = self.attachment.on_deck_frame(bytes, Instant::now());
        let output = self.withdrawn_or(output);
        let output = self.in_front_or_end(output);
        self.handed_out(output)
    }

    /// Whether the dispatcher's poll still has work: a running attachment,
    /// or a lift owed to a session that is not in front.
    pub(crate) const fn needs_poll(&self) -> bool {
        self.is_active() || self.lift_owed
    }

    /// Deadlines and a pen that went quiet while running; afterwards, the
    /// owed lift once the session is back in front.
    pub(crate) fn poll(&mut self) -> NativeTabletOutput {
        if !self.is_active() {
            return self.owed_lift().unwrap_or_default();
        }
        let output = self.attachment.poll(Instant::now());
        let output = self.withdrawn_or(output);
        let output = self.in_front_or_end(output);
        self.handed_out(output)
    }

    /// Records the last pen event handed out for posting.
    fn handed_out(&mut self, output: NativeTabletOutput) -> NativeTabletOutput {
        if let Some(last) = output.pens.last() {
            self.last_pen = Some(last.clone());
        }
        output
    }

    /// The event that lifts the pen last handed out, when it is still in
    /// proximity: proximity out, tip up, no button, no pressure, where it
    /// was. Harmless to any reader: it releases, never presses or moves.
    fn lift(&self) -> Option<PenEventMsg> {
        self.last_pen
            .as_ref()
            .filter(|last| last.in_proximity || last.touching || last.buttons != 0)
            .map(|last| PenEventMsg {
                in_proximity: false,
                touching: false,
                pressure: 0.0,
                buttons: 0,
                ..last.clone()
            })
    }

    /// Ends Native tablet after a lapse (`gap`).
    fn end_lapsed(&mut self, gap: &JoinedDeviceGap) -> NativeTabletOutput {
        self.end_lifting(gap.pen_refusal().denial().reason())
    }

    /// Ends Native tablet for `reason` while the Deck is still there. No
    /// sample read but not yet posted is posted; a pen left down is lifted
    /// at once while the session is in front, otherwise once it is back in
    /// front, exactly once (when the attachment ends first, the session's
    /// pen device releases what it holds as it goes).
    pub(crate) fn end_lifting(&mut self, reason: &str) -> NativeTabletOutput {
        let mut output = self.end(reason, false);
        let lift = self.lift();
        if lift.is_some() && (self.in_front)(&self.joined) {
            output.pens = lift.into_iter().collect();
            self.handed_out(output)
        } else {
            self.lift_owed = lift.is_some();
            output.pens.clear();
            output
        }
    }

    /// The owed lift, once the session is back in front.
    fn owed_lift(&mut self) -> Option<NativeTabletOutput> {
        if !self.lift_owed || !(self.in_front)(&self.joined) {
            return None;
        }
        self.lift_owed = false;
        tracing::info!(
            target: INPUT,
            "Native tablet: the joined session is back in front; the pen left down is lifted"
        );
        let output = NativeTabletOutput {
            pens: self.lift().into_iter().collect(),
            ..NativeTabletOutput::default()
        };
        Some(self.handed_out(output))
    }

    /// Between full proofs, every batch of pen events is posted only while
    /// the joined session is still in front of its seat (one small read of
    /// logind's seat record): when another session came forward its display
    /// server would read the device, so the events are dropped and Native
    /// tablet ends without another one.
    fn in_front_or_end(&mut self, mut output: NativeTabletOutput) -> NativeTabletOutput {
        if output.pens.is_empty() || (self.in_front)(&self.joined) {
            return output;
        }
        if !self.is_active() {
            // The step itself ended Native tablet: its notice stands, its
            // events are not posted into a session not in front; the lift
            // is owed.
            output.pens.clear();
            self.lift_owed = self.lift().is_some();
            return output;
        }
        tracing::warn!(
            target: INPUT,
            dropped = output.pens.len(),
            "Native tablet: the joined session left the front of its seat; pen events dropped"
        );
        let mut ended = self.end_lapsed(&JoinedDeviceGap::NotInFront);
        let mut frames = output.frames;
        frames.append(&mut ended.frames);
        ended.frames = frames;
        ended
    }

    /// Ends Native tablet for `reason`; `release` lifts a pen left in
    /// proximity.
    pub(crate) fn end(&mut self, reason: &str, release: bool) -> NativeTabletOutput {
        self.attachment.end(reason, release, Instant::now())
    }

    /// Stops without telling the Deck (its link is going away).
    pub(crate) fn shutdown(&mut self) {
        if let Some(counts) = self.attachment.shutdown() {
            tracing::info!(
                target: INPUT,
                pen_packets = counts.pen_packets,
                pen_ignored = counts.pen_ignored,
                pen_dropped = counts.pen_dropped,
                elapsed_ms = counts.elapsed.as_millis(),
                "Native tablet stopped with the attachment"
            );
        }
    }

    /// Starts proving confinement again, off the dispatcher: the scan reads
    /// every process's descriptors. `None` once Native tablet ended.
    #[allow(
        clippy::disallowed_methods,
        reason = "plain /proc and logind file reads that finish on their own; the dispatcher polls the handle"
    )]
    pub(crate) fn start_recheck(&self) -> Option<RecheckTask> {
        if !self.is_active() {
            return None;
        }
        let (nodes, joined) = (self.nodes.clone(), self.joined.clone());
        Some(tokio::task::spawn_blocking(move || {
            joined_devices::prove_confined(&nodes, &joined, Duration::ZERO)
                .map(|_| ())
                .map_err(|failed| (failed.0, joined_devices::readers_summary(&failed.1)))
        }))
    }

    /// A finished recheck: a gap ends Native tablet without another event
    /// (the device may now reach another desktop).
    pub(crate) fn finish_recheck(
        &mut self,
        proof: Result<RecheckResult, tokio::task::JoinError>,
    ) -> Option<NativeTabletOutput> {
        let gap = match proof {
            Ok(Ok(())) => return None,
            Ok(Err((gap, readers))) => {
                tracing::warn!(
                    target: INPUT,
                    %gap,
                    %readers,
                    "Native tablet: the typed pen device is no longer proved confined to the joined desktop"
                );
                gap
            }
            Err(error) => {
                tracing::warn!(target: INPUT, %error, "the typed pen confinement recheck did not finish");
                JoinedDeviceGap::NoJoinedServer
            }
        };
        self.is_active().then(|| self.end_lapsed(&gap))
    }
}

/// A recheck's outcome: the gap and who read which node.
pub(crate) type RecheckResult = Result<(), (JoinedDeviceGap, String)>;
/// A recheck running off the dispatcher.
pub(crate) type RecheckTask = tokio::task::JoinHandle<RecheckResult>;

/// Logs what a Native tablet step says about the pen and the attachment.
pub(crate) fn log_output(output: &NativeTabletOutput) {
    for note in &output.notes {
        match note {
            arcen_usb_bridge::HostPenNote::ModeRead { original, failure } => tracing::info!(
                target: INPUT,
                original = ?original,
                failure = ?failure,
                "Native tablet: the pen's mode before this Pier switches it"
            ),
            arcen_usb_bridge::HostPenNote::ModeSwitched { accepted, failure } => tracing::info!(
                target: INPUT,
                accepted,
                failure = ?failure,
                "Native tablet: this Pier switched the pen to tablet mode"
            ),
        }
    }
    if let Some((reason, counts)) = output.ended.as_ref() {
        tracing::warn!(
            target: INPUT,
            event = "usb_hard_device_refused",
            %reason,
            pen_packets = counts.pen_packets,
            pen_ignored = counts.pen_ignored,
            pen_dropped = counts.pen_dropped,
            elapsed_ms = counts.elapsed.as_millis(),
            "Native tablet ended; the session goes on with mouse compatibility"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcen_protocol::messages::UsbHidReportDescriptorMsg;

    fn console(display: &str) -> crate::session::lifecycle::SessionMetadata {
        crate::session::lifecycle::SessionMetadata {
            username: "artist".into(),
            uid: 1000,
            session_id: "c7".into(),
            session_type: "x11".into(),
            desktop: "gnome".into(),
            display: display.into(),
            agent_pid: 1,
            generation: 1,
            reconnected: false,
            timezone: None,
            multi_monitor_plan: None,
            multi_monitor_carrier: None,
            wayland: None,
            audio: None,
            requested_outputs: None,
            x11_desktop_depth: None,
            native_seat: None,
            console: true,
            console_guest: false,
            console_shown_outputs: None,
            console_desk_privacy: None,
        }
    }

    fn requested(accepted: bool) -> TabletModeResultMsg {
        TabletModeResultMsg {
            requested: TabletModeMsg::UsbTabletBridge,
            active: if accepted {
                TabletModeMsg::UsbTabletBridge
            } else {
                TabletModeMsg::DisabledMouseCompat
            },
            accepted,
            ..TabletModeResultMsg::default()
        }
    }

    fn generic_pen() -> UsbHardDeviceMsg {
        UsbHardDeviceMsg {
            vendor_id: 0x256c,
            product_id: 0x0042,
            bcd_device: 0x0100,
            device_class: 0,
            speed: arcen_usb_bridge::UsbSpeed::Full,
            hid_report_descriptors: vec![UsbHidReportDescriptorMsg::new(
                0,
                arcen_usb_bridge::descriptor_fixtures::GAOMON_STYLE_PEN,
            )],
        }
    }

    #[test]
    fn the_joined_desktop_is_its_display_and_logind_session_at_any_display_number() {
        for (display, number) in [(":0", 0), (":1", 1), (":100", 100)] {
            assert_eq!(
                joined_desktop(&console(display)),
                Some(JoinedDesktop {
                    display: number,
                    session_id: "c7".into(),
                    uid: 1000,
                })
            );
        }
        assert_eq!(joined_desktop(&console("host:1")), None);
        assert_eq!(joined_desktop(&console(":010")), None);
        assert!(applies(
            HardUsbSessionSeat::JoinedConsole,
            TabletModeMsg::UsbTabletBridge
        ));
        assert!(!applies(
            HardUsbSessionSeat::JoinedConsole,
            TabletModeMsg::LocalTermination
        ));
        assert!(!applies(
            HardUsbSessionSeat::X11Desktop,
            TabletModeMsg::UsbTabletBridge
        ));
    }

    /// The hello offers Native tablet on a joined console only when it is
    /// enabled and the typed pen device exists; never elsewhere by this
    /// route.
    #[test]
    fn a_joined_console_offers_native_tablet_only_with_a_real_pen_device() {
        let pen = PenCapabilityTruth::LINUX_UINPUT_TABLET;
        let joined = HardUsbSessionSeat::JoinedConsole;
        assert!(offered_on(joined, None, pen));
        assert!(!offered_on(joined, Some("not enabled"), pen));
        assert!(!offered_on(joined, None, PenCapabilityTruth::UNAVAILABLE));
        for seat in [
            HardUsbSessionSeat::X11Desktop,
            HardUsbSessionSeat::ArcenSeat,
            HardUsbSessionSeat::Seatless,
            HardUsbSessionSeat::ConsoleSeat,
        ] {
            assert!(!offered_on(seat, None, pen), "{seat:?}");
        }
    }

    /// An admitted route with an accepted device is active and names what
    /// the pen brings; a refused route keeps mouse compatibility with the
    /// route's own reason; a refused device keeps the device's reason.
    #[test]
    fn the_deck_is_told_which_route_is_active_and_why_not() {
        let device = PenCapabilityTruth::LINUX_UINPUT_TABLET;
        let joined = JoinedDesktop {
            display: 1,
            session_id: "c7".into(),
            uid: 1000,
        };
        let accepted = JoinedPenOffer {
            refusal: None,
            authorization: Some(arcen_input::bridged_pen::authorize(
                Some(&generic_pen()),
                POLICY,
                false,
            )),
            nodes: Vec::new(),
            joined: Some(joined.clone()),
        };
        let (result, plan) = settle(requested(true), accepted, device);
        assert!(result.accepted && plan.is_some());
        assert_eq!(result.active, TabletModeMsg::UsbTabletBridge);
        let reason = String::from(result.reason);
        assert!(reason.contains("this Linux Pier reads the pen"), "{reason}");
        assert!(
            reason.contains("pressure, hover and barrel buttons"),
            "{reason}"
        );

        let refused = JoinedPenOffer::refused(
            arcen_usb_bridge::HardUsbSessionDenial::JoinedConsolePenNotConfined
                .reason()
                .to_owned(),
        );
        let (result, plan) = settle(requested(false), refused, device);
        assert!(!result.accepted && plan.is_none());
        assert_eq!(result.active, TabletModeMsg::DisabledMouseCompat);
        assert!(String::from(result.reason).contains("could not be kept to your session"));

        let unknown = UsbHardDeviceMsg {
            hid_report_descriptors: vec![UsbHidReportDescriptorMsg::new(
                0,
                arcen_usb_bridge::descriptor_fixtures::GAMEPAD,
            )],
            ..generic_pen()
        };
        let device_refused = JoinedPenOffer {
            refusal: None,
            authorization: Some(arcen_input::bridged_pen::authorize(
                Some(&unknown),
                POLICY,
                false,
            )),
            nodes: Vec::new(),
            joined: Some(joined),
        };
        let (result, plan) = settle(requested(true), device_refused, device);
        assert!(!result.accepted && result.reconnect_required && plan.is_none());
        assert_eq!(result.active, TabletModeMsg::DisabledMouseCompat);
        assert!(String::from(result.reason).contains("refused"));
    }

    /// The display the native tests name their devices for: unused on the
    /// lab (`:1..:99`, no X server).
    #[cfg(target_os = "linux")]
    const LAB_DISPLAY: &str = ":88";

    /// Native: the facts the proof is decided on, read from the kernel for
    /// a real uinput tablet-tool device. Creates devices, injects nothing.
    /// Without an X server on `:88` the joined server is not found; any
    /// console X server that hot-added the device is named as a foreign
    /// reader. With `ARCEN_I1_JOINED_DISPLAY=N` and
    /// `ARCEN_I1_JOINED_SESSION=ID` (the console's own X display and logind
    /// session) the same device is judged as that desktop's.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires /dev/uinput and /proc access; creates virtual devices without injecting input"]
    fn native_device_facts_are_read_from_the_kernel() {
        let identity =
            arcen_input::virtual_device::SessionInputIdentity::for_x_display(LAB_DISPLAY)
                .expect("lab display");
        let (controller, _) =
            crate::input::InputController::new(1920, 1080, None, identity).expect("uinput devices");
        let mut input = PierInput::Uinput(controller);
        let nodes = input
            .tablet_nodes()
            .expect("a tablet-tool device")
            .expect("its sysfs nodes");
        eprintln!("typed pen nodes: {nodes:?}");
        assert!(nodes.iter().any(|node| node.name.starts_with("event")));
        // Let any console X server add it, as it would on a joined desktop.
        std::thread::sleep(Duration::from_millis(1500));
        let lab = JoinedDesktop {
            display: 88,
            session_id: String::new(),
            uid: u32::MAX,
        };
        let facts = joined_devices::device_facts(&nodes, &lab);
        eprintln!(
            "judged for :88: joined_server={} in_front={} readers: {}; proof: {:?}",
            facts.joined_server,
            facts.joined_in_front,
            joined_devices::readers_summary(&facts),
            arcen_input::joined_input::prove_joined_device_confinement(&facts)
        );
        assert!(!facts.joined_server, "no X server listens as :88");
        assert!(arcen_input::joined_input::prove_joined_device_confinement(&facts).is_err());
        for node in &facts.nodes {
            assert!(
                !node
                    .readers
                    .contains(&arcen_input::joined_input::DeviceNodeReader::JoinedDisplayServer),
                "nothing is the joined server of :88"
            );
        }
        if let (Ok(display), Ok(session_id)) = (
            std::env::var("ARCEN_I1_JOINED_DISPLAY"),
            std::env::var("ARCEN_I1_JOINED_SESSION"),
        ) {
            let console = JoinedDesktop {
                display: display.parse().expect("display number"),
                uid: joined_devices::session_uid(&session_id).expect("the session's account"),
                session_id,
            };
            let judged = joined_devices::prove_confined(&nodes, &console, Duration::from_secs(2));
            match &judged {
                Ok(facts) => eprintln!(
                    "judged for :{}: joined_server={} in_front={} readers: {}; proof: Ok",
                    console.display,
                    facts.joined_server,
                    facts.joined_in_front,
                    joined_devices::readers_summary(facts),
                ),
                Err(failed) => eprintln!(
                    "judged for :{}: joined_server={} in_front={} readers: {}; proof: {:?}",
                    console.display,
                    failed.1.joined_server,
                    failed.1.joined_in_front,
                    joined_devices::readers_summary(&failed.1),
                    failed.0
                ),
            }
            let facts = match &judged {
                Ok(facts) => facts,
                Err(failed) => &failed.1,
            };
            assert!(
                facts.joined_server,
                "the console's X server listens as :{display}"
            );
        }
    }

    /// Native: the decode-to-inject path end to end on a real kernel. The
    /// Deck's exporter serves an emulated Intuos5 (`--usb-emulate
    /// wacom-intuos5`); the joined console's Native tablet reads it over
    /// real wire frames and posts every sample through a real uinput
    /// tablet-tool device, read back from its event node. The devices are
    /// named for `:88` and put on a seat nobody runs by a volatile udev rule
    /// written first, so no X server (the console's included) ever adds
    /// them: nothing reaches anyone's desktop. Needs root and
    /// `ARCEN_I1_LAB_SEAT=1`.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "lab only: root, /dev/uinput, udev; devices on a seat nobody runs"]
    fn native_an_emulated_intuos5_is_decoded_and_posted_through_the_typed_pen() {
        use evdev::{AbsoluteAxisCode, EventType, KeyCode};
        assert_eq!(
            std::env::var("ARCEN_I1_LAB_SEAT").as_deref(),
            Ok("1"),
            "lab only"
        );
        let rule = crate::input::joined_devices::lab_seat::LabSeatRule::install(LAB_DISPLAY);
        let identity =
            arcen_input::virtual_device::SessionInputIdentity::for_x_display(LAB_DISPLAY)
                .expect("lab display");
        let (controller, _) =
            crate::input::InputController::new(1920, 1080, None, identity).expect("uinput devices");
        let mut input = PierInput::Uinput(controller);
        let nodes = input.tablet_nodes().expect("device").expect("nodes");
        std::thread::sleep(Duration::from_millis(1500));
        let lab = JoinedDesktop {
            display: 88,
            session_id: String::new(),
            uid: u32::MAX,
        };
        // Nobody reads the isolated device: no X server added it.
        let facts = joined_devices::device_facts(&nodes, &lab);
        eprintln!(
            "isolated device readers: {}",
            joined_devices::readers_summary(&facts)
        );
        assert!(
            facts.nodes.iter().all(|node| node.readers.is_empty()),
            "an X server added a device on a seat nobody runs"
        );
        let event_node = nodes
            .iter()
            .find(|node| node.name.starts_with("event"))
            .expect("event node");
        let reader =
            evdev::Device::open(format!("/dev/input/{}", event_node.name)).expect("open node");
        reader.set_nonblocking(true).expect("nonblocking");
        let mut reader = reader;

        let (exporter, handle, claimed) = emulated_intuos5();
        let (mut pen, first) = accepted_pen(&claimed, nodes.clone(), lab);
        // The device is on a seat nobody runs: whichever session is in
        // front, nothing reaches a desktop.
        pen.in_front = |_| true;
        let mut posted = 0_usize;
        let mut touched = false;
        let mut left = false;
        let mut seen: Vec<(u16, u16, i32)> = Vec::new();
        drive(&exporter, &handle, &mut pen, first.frames, |output| {
            assert!(output.publish.is_none(), "nothing is published");
            assert!(output.ended.is_none(), "{:?}", output.ended);
            for event in &output.pens {
                input.pen_event(event).expect("posted");
                posted += 1;
                touched |= event.touching;
                left |= touched && !event.in_proximity;
            }
            if let Ok(events) = reader.fetch_events() {
                seen.extend(
                    events
                        .filter(|event| {
                            matches!(event.event_type(), EventType::KEY | EventType::ABSOLUTE)
                        })
                        .map(|event| (event.event_type().0, event.code(), event.value())),
                );
            }
            // The stroke is over once the pen touched and then left.
            left
        });
        let output = pen.end("lab test done", true);
        for event in &output.pens {
            input.pen_event(event).expect("posted");
        }
        drop(rule);
        let key = |code: KeyCode, value: i32| {
            seen.iter()
                .any(|(kind, c, v)| *kind == EventType::KEY.0 && *c == code.0 && *v == value)
        };
        let max_abs = |code: AbsoluteAxisCode| {
            seen.iter()
                .filter(|(kind, c, _)| *kind == EventType::ABSOLUTE.0 && *c == code.0)
                .map(|(_, _, v)| *v)
                .max()
        };
        eprintln!(
            "decoded and posted {posted} pen events; read back {} key/abs events; \
             max pressure {:?}, x range seen up to {:?}",
            seen.len(),
            max_abs(AbsoluteAxisCode::ABS_PRESSURE),
            max_abs(AbsoluteAxisCode::ABS_X)
        );
        assert!(posted > 20, "the stroke was decoded: {posted}");
        assert!(key(KeyCode::BTN_TOOL_PEN, 1), "the pen came into proximity");
        assert!(key(KeyCode::BTN_TOUCH, 1), "the pen touched");
        assert!(key(KeyCode::BTN_TOUCH, 0), "the pen lifted");
        assert!(key(KeyCode::BTN_TOOL_PEN, 0), "the pen left proximity");
        assert!(
            key(KeyCode::BTN_STYLUS, 1) || key(KeyCode::BTN_STYLUS2, 1),
            "the barrel button reached the device"
        );
        assert!(
            max_abs(AbsoluteAxisCode::ABS_PRESSURE).unwrap_or(0) > 0,
            "pressure reached the device"
        );
    }

    /// An emulated Intuos5 behind the Deck's shared exporter (`--usb-emulate
    /// wacom-intuos5`) and the device it claims.
    fn emulated_intuos5() -> (
        arcen_usb_bridge::UsbExporter,
        arcen_usb_bridge::SyntheticHandle<arcen_usb_bridge::WacomIntuos5>,
        UsbHardDeviceMsg,
    ) {
        let (device, handle) =
            arcen_usb_bridge::SyntheticDevice::emulate(arcen_usb_bridge::WacomIntuos5::new())
                .expect("emulate");
        let exporter = arcen_usb_bridge::UsbExporter::new(Box::new(device))
            .map_err(|(refusal, _)| refusal)
            .expect("exporter");
        let facts = exporter.facts();
        let claimed = UsbHardDeviceMsg {
            vendor_id: facts.snapshot.id.vendor_id,
            product_id: facts.snapshot.id.product_id,
            bcd_device: facts.snapshot.id.bcd_device,
            device_class: facts.snapshot.device_class,
            speed: facts.speed,
            hid_report_descriptors: facts
                .report_descriptors
                .iter()
                .map(|r| UsbHidReportDescriptorMsg::new(r.interface, &r.descriptor))
                .collect(),
        };
        (exporter, handle, claimed)
    }

    /// The joined console's route accepted for `claimed`, started.
    fn accepted_pen(
        claimed: &UsbHardDeviceMsg,
        nodes: Vec<DeviceNode>,
        joined: JoinedDesktop,
    ) -> (JoinedNativePen, NativeTabletOutput) {
        let offer = JoinedPenOffer {
            refusal: None,
            authorization: Some(arcen_input::bridged_pen::authorize(
                Some(claimed),
                POLICY,
                false,
            )),
            nodes,
            joined: Some(joined),
        };
        let (result, plan) = settle(
            requested(true),
            offer,
            PenCapabilityTruth::LINUX_UINPUT_TABLET,
        );
        eprintln!(
            "tablet-mode result: {}",
            String::from(result.reason.clone())
        );
        JoinedNativePen::start(plan.expect("active"), &result)
    }

    /// Runs the attachment against the exporter over real wire frames,
    /// playing the scripted stroke once the pen is polled in Wacom mode,
    /// until `step` says stop or 20 s passed.
    fn drive(
        exporter: &arcen_usb_bridge::UsbExporter,
        handle: &arcen_usb_bridge::SyntheticHandle<arcen_usb_bridge::WacomIntuos5>,
        pen: &mut JoinedNativePen,
        mut frames: Vec<Vec<u8>>,
        mut step: impl FnMut(&NativeTabletOutput) -> bool,
    ) {
        let stroke = arcen_usb_bridge::intuos5_scripted_stroke();
        let mut pushed = false;
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            for frame in std::mem::take(&mut frames) {
                if frame.first() == Some(&(arcen_protocol::FrameType::UsbBridgeUrbSubmit as u8)) {
                    let (header, payload) =
                        arcen_protocol::decode_usb_urb_submit(&frame).expect("submit");
                    exporter
                        .submit(
                            arcen_usb_bridge::UrbSubmission {
                                generation: header.generation,
                                urb_id: header.urb_id,
                                endpoint: header.endpoint,
                                transfer_kind: header.transfer_kind,
                                timeout_ms: header.timeout_ms,
                                declared_length: header.declared_length,
                                setup: header.setup,
                            },
                            payload,
                        )
                        .expect("exporter admits the URB");
                }
            }
            if !pushed
                && handle.inspect(arcen_usb_bridge::WacomIntuos5::wacom_mode)
                && handle.held_on(arcen_usb_bridge::INTUOS5_PEN_IN) > 0
            {
                for script in &stroke {
                    handle.push_report_to(
                        arcen_usb_bridge::INTUOS5_PEN_IN,
                        script.report.encode().to_vec(),
                    );
                }
                pushed = true;
            }
            if let Ok(Some(done)) = exporter.next_completion_within(Duration::from_millis(20)) {
                let frame = arcen_protocol::encode_usb_urb_complete(
                    arcen_protocol::UsbUrbCompletionHeader {
                        generation: done.generation,
                        urb_id: done.urb_id,
                        status: done.status,
                        actual_length: u32::try_from(done.data.len()).expect("length"),
                    },
                    &done.data,
                )
                .expect("completion");
                if !pen.takes(&frame) {
                    return;
                }
                let output = pen.on_deck_frame(&frame);
                frames.extend(output.frames.iter().cloned());
                if step(&output) && pushed {
                    return;
                }
            }
        }
    }

    /// The pen read while the joined session is not in front is never
    /// posted: the batch is dropped and Native tablet ends, telling the
    /// Deck, with the mode restored and the transfers cancelled.
    #[test]
    fn pen_events_are_dropped_and_native_tablet_ends_when_the_joined_session_leaves_the_front() {
        let (exporter, handle, claimed) = emulated_intuos5();
        let joined = JoinedDesktop {
            display: 1,
            session_id: "c7".into(),
            uid: 1000,
        };
        let (mut pen, first) = accepted_pen(&claimed, Vec::new(), joined);
        pen.in_front = |_| false;
        let mut outputs = Vec::new();
        drive(&exporter, &handle, &mut pen, first.frames, |output| {
            outputs.push((
                output.pens.len(),
                output.notice.clone(),
                output.ended.is_some(),
            ));
            output.ended.is_some()
        });
        assert!(
            outputs.iter().all(|(pens, _, _)| *pens == 0),
            "no pen event is posted: {outputs:?}"
        );
        let (_, notice, ended) = outputs.last().expect("the stroke was read").clone();
        assert!(ended, "Native tablet ended");
        let notice: TabletModeResultMsg =
            serde_json::from_str(&notice.expect("the Deck is told")).expect("result");
        assert!(!notice.accepted && notice.reconnect_required);
        assert_eq!(notice.active, TabletModeMsg::DisabledMouseCompat);
        assert!(String::from(notice.reason).contains("not the one on the host's monitors"));
        assert!(!pen.is_active());
    }

    /// Drives an accepted pen until it touches; the events handed out.
    fn touching_pen(in_front: fn(&JoinedDesktop) -> bool) -> (JoinedNativePen, Vec<PenEventMsg>) {
        let (exporter, handle, claimed) = emulated_intuos5();
        let joined = JoinedDesktop {
            display: 1,
            session_id: "c7".into(),
            uid: 1000,
        };
        let (mut pen, first) = accepted_pen(&claimed, Vec::new(), joined);
        pen.in_front = in_front;
        let mut posted = Vec::new();
        drive(&exporter, &handle, &mut pen, first.frames, |output| {
            posted.extend(output.pens.iter().cloned());
            posted.iter().any(|pen| pen.touching)
        });
        assert!(posted.iter().any(|pen| pen.touching), "the pen touched");
        assert!(posted.last().is_some_and(|pen| pen.in_proximity));
        (pen, posted)
    }

    fn assert_lift(events: &[PenEventMsg]) {
        assert_eq!(events.len(), 1, "exactly one lift: {events:?}");
        let lift = &events[0];
        assert!(!lift.in_proximity && !lift.touching, "{lift:?}");
        assert_eq!(lift.buttons, 0);
        assert!(lift.pressure.abs() < f32::EPSILON);
    }

    /// A recheck that fails mid-stroke while the session is still in front
    /// (another reader opened the node) lifts the pen on the joined desktop:
    /// one event, proximity out, tip up, no button. Nothing else is posted
    /// and nothing is owed afterwards.
    #[test]
    fn a_lapse_mid_stroke_in_front_posts_only_the_lift_once() {
        let (mut pen, _) = touching_pen(|_| true);
        let gap = JoinedDeviceGap::ForeignReader {
            node: "event8".to_owned(),
            reader: arcen_input::joined_input::DeviceNodeReader::OtherAccount {
                pid: 31,
                uid: 1001,
                comm: "evtest".to_owned(),
            },
        };
        let output = pen
            .finish_recheck(Ok(Err((gap.clone(), String::new()))))
            .expect("Native tablet ends");
        assert!(output.notice.is_some(), "the Deck is told");
        assert_lift(&output.pens);
        assert!(!pen.is_active() && !pen.needs_poll());
        assert!(pen.poll().pens.is_empty(), "no second lift");
        assert!(pen.finish_recheck(Ok(Err((gap, String::new())))).is_none());
    }

    static FRONT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

    /// A lapse while the session is not in front posts nothing into the
    /// background; the lift follows once, when the session is back in front.
    #[test]
    fn a_lapse_out_of_front_posts_nothing_then_lifts_once_on_return() {
        use std::sync::atomic::Ordering;
        FRONT.store(true, Ordering::SeqCst);
        let (mut pen, _) = touching_pen(|_| FRONT.load(Ordering::SeqCst));
        FRONT.store(false, Ordering::SeqCst);
        let output = pen
            .finish_recheck(Ok(Err((JoinedDeviceGap::NotInFront, String::new()))))
            .expect("Native tablet ends");
        assert!(output.notice.is_some(), "the Deck is told");
        assert!(
            output.pens.is_empty(),
            "nothing reaches a session not in front"
        );
        assert!(pen.needs_poll(), "a lift is owed");
        assert!(pen.poll().pens.is_empty(), "still not in front");
        FRONT.store(true, Ordering::SeqCst);
        assert_lift(&pen.poll().pens);
        assert!(!pen.needs_poll());
        assert!(pen.poll().pens.is_empty(), "exactly once");
    }
}
