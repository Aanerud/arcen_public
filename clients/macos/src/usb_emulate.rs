// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Lab-only emulated Hard USB device (`--usb-emulate wacom-intuos5`).
//!
//! Instead of asking the privileged helper to capture a physical tablet, the
//! Deck feeds an emulated Wacom Intuos5 (`arcen_usb_bridge::WacomIntuos5`,
//! real descriptors and control answers) through the same shared exporter
//! (`arcen_usb_bridge::UsbExporter`) into the ordinary Hard USB negotiation.
//! The Pier sees the device exactly as a captured Wacom. Once the host's
//! driver has switched the tablet into Wacom mode and polls the pen endpoint,
//! a scripted stroke (`arcen_usb_bridge::intuos5_scripted_stroke`) is played,
//! one report per host poll, so a host-side check can assert the pen's
//! position, pressure, proximity and buttons with no pen in hand.
//!
//! Compiled only with the `usb-emulate-lab` feature, which no packaging
//! script enables, and active only when a CLI smoke asks for it.

use arcen_protocol::messages::{UsbHardDeviceMsg, UsbHidReportDescriptorMsg};
use arcen_protocol::{encode_usb_urb_complete, UsbUrbCompletionHeader, UsbUrbSubmitHeader};
use arcen_usb_bridge::{
    intuos5_landing_stroke, intuos5_realistic_stroke, intuos5_resting_stroke,
    intuos5_scripted_stroke, with_upper_switch_held, AttachmentGeneration, ExportedCompletion,
    Intuos5PenReport, ReleaseReason, ScriptedReport, SyntheticDevice, SyntheticHandle, UrbId,
    UrbStatus, UrbSubmission, UsbExporter, WacomIntuos5, INTUOS5_PEN_IN, RESTING_HOLD,
    SCRIPTED_MINIMUM_INTERVAL,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Release packaging (`packaging/macos/build-deck-app.sh --release`) scans
/// the final binary for this marker and refuses to ship it, so a stale
/// `usb-emulate-lab` build cannot become a release even with `--no-build`.
#[used]
pub static ARCEN_DECK_USB_EMULATE_FEATURE_MARKER: &[u8] =
    b"ARCEN_DECK_USB_EMULATE_FEATURE_MARKER_DO_NOT_SHIP";

/// Devices the Deck can emulate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmulatedDevice {
    /// Wacom Intuos5 touch L (`056a:0317`; the Linux `wacom` driver names
    /// it "Intuos Pro L").
    WacomIntuos5,
}

impl EmulatedDevice {
    /// Parses the `--usb-emulate` value.
    ///
    /// # Errors
    ///
    /// Names the accepted values.
    pub fn parse(name: &str) -> Result<Self, String> {
        match name {
            "wacom-intuos5" => Ok(Self::WacomIntuos5),
            other => Err(format!(
                "invalid --usb-emulate '{other}'; accepted values: wacom-intuos5"
            )),
        }
    }

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::WacomIntuos5 => "wacom-intuos5",
        }
    }
}

/// How the scripted strokes are played.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Playback {
    /// Complete strokes to play.
    pub strokes: usize,
    /// Wait after the host first polls the pen, so a host-side recorder that
    /// opened the device is ready.
    pub lead_in: Duration,
    /// Pause between strokes.
    pub gap: Duration,
    /// Which stroke each turn plays.
    pub stroke: StrokeScript,
}

/// The stroke the emulated pen plays (`--usb-emulate-stroke`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StrokeScript {
    /// Hovers first, then touches down with a pressure ramp
    /// (`arcen_usb_bridge::intuos5_scripted_stroke`).
    #[default]
    Scripted,
    /// Its first pen packet already presses
    /// (`arcen_usb_bridge::intuos5_landing_stroke`).
    Landing,
    /// Odd strokes `Scripted`, even strokes `Landing`.
    Alternate,
    /// Shaped like a real hand on a bridged tablet: in-range packets, the
    /// KP-501E Grip Pen's tool id, a hovering descent, a landing already
    /// pressing, varying pressure with a held tilt
    /// (`arcen_usb_bridge::intuos5_realistic_stroke`).
    Realistic,
    /// `Scripted` with the pen tilted, sweeping both tilt axes across the
    /// stroke (see [`tilted`]): the scripted stroke itself holds the pen
    /// upright, so it cannot show that tilt reaches an application.
    Tilted,
    /// Lands and stays pressing, then holds for
    /// `arcen_usb_bridge::RESTING_HOLD` with no lift or exit before the
    /// playback is done (`arcen_usb_bridge::intuos5_resting_stroke`).
    Rest,
    /// The same, ending in a hover inside proximity.
    HoverRest,
    /// `Realistic` with the upper barrel button held in every packet
    /// (`arcen_usb_bridge::with_upper_switch_held`).
    UpperHeld,
}

impl StrokeScript {
    /// Parses `scripted`, `landing`, `alternate`, `realistic`, `tilted`,
    /// `rest`, `hover-rest` or `upper-held`.
    ///
    /// # Errors
    ///
    /// Names the accepted values.
    pub fn parse(name: &str) -> Result<Self, String> {
        match name {
            "scripted" => Ok(Self::Scripted),
            "landing" => Ok(Self::Landing),
            "alternate" => Ok(Self::Alternate),
            "realistic" => Ok(Self::Realistic),
            "tilted" => Ok(Self::Tilted),
            "rest" => Ok(Self::Rest),
            "hover-rest" => Ok(Self::HoverRest),
            "upper-held" => Ok(Self::UpperHeld),
            other => Err(format!(
                "invalid --usb-emulate-stroke '{other}'; accepted values: scripted, landing, alternate, realistic, tilted, rest, hover-rest, upper-held"
            )),
        }
    }

    fn script(self, stroke: usize) -> Vec<ScriptedReport> {
        match self {
            Self::Scripted => intuos5_scripted_stroke(),
            Self::Landing => intuos5_landing_stroke(),
            Self::Alternate if stroke.is_multiple_of(2) => intuos5_landing_stroke(),
            Self::Alternate => intuos5_scripted_stroke(),
            Self::Realistic => intuos5_realistic_stroke(),
            Self::Tilted => tilted(intuos5_scripted_stroke()),
            Self::Rest => intuos5_resting_stroke(true),
            Self::HoverRest => intuos5_resting_stroke(false),
            Self::UpperHeld => with_upper_switch_held(intuos5_realistic_stroke()),
        }
    }

    /// Whether the stroke leaves the pen resting (no lift, no exit).
    const fn rests(self) -> bool {
        matches!(self, Self::Rest | Self::HoverRest)
    }
}

impl Default for Playback {
    fn default() -> Self {
        Self {
            strokes: 3,
            lead_in: Duration::from_secs(3),
            gap: Duration::from_millis(1_500),
            stroke: StrokeScript::Scripted,
        }
    }
}

/// Where the scripted playback is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlaybackStatus {
    NotStarted,
    WaitingForHost,
    Playing { stroke: usize },
    Done { strokes: usize, reports: usize },
    Failed(String),
}

static SELECTED: OnceLock<(EmulatedDevice, Playback)> = OnceLock::new();
static STATUS: Mutex<PlaybackStatus> = Mutex::new(PlaybackStatus::NotStarted);

/// Selects the device the next Hard USB negotiation emulates instead of
/// capturing a physical tablet. Called once, by a CLI smoke, before it
/// connects.
///
/// # Errors
///
/// Refuses a second selection.
pub fn select(device: EmulatedDevice, playback: Playback) -> Result<(), String> {
    SELECTED
        .set((device, playback))
        .map_err(|_| "an emulated USB device is already selected".to_owned())
}

/// The selected emulation, if a CLI smoke asked for one.
#[must_use]
pub fn selected() -> Option<(EmulatedDevice, Playback)> {
    SELECTED.get().copied()
}

/// Where the scripted playback is.
#[must_use]
pub fn status() -> PlaybackStatus {
    STATUS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

fn set_status(status: PlaybackStatus) {
    *STATUS.lock().unwrap_or_else(PoisonError::into_inner) = status;
}

/// How long the host may take to bind the tablet and switch it into Wacom
/// mode, and then to open it and start polling the pen.
const HOST_READY_TIMEOUT: Duration = Duration::from_secs(60);
/// How long one scripted report may wait for the host's next poll.
const POLL_TIMEOUT: Duration = Duration::from_secs(5);
const PUMP_WAIT: Duration = Duration::from_millis(100);

/// One emulated device bridged through the shared exporter.
pub struct EmulatedResponder {
    exporter: Arc<UsbExporter>,
    device: UsbHardDeviceMsg,
    completions: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}

impl EmulatedResponder {
    /// Builds the emulated device, admits it with the shared policy exactly
    /// as a captured one, and starts the completion pump and the player.
    ///
    /// # Errors
    ///
    /// Returns a message when the shared policy refuses the device.
    pub fn start(device: EmulatedDevice, playback: Playback) -> Result<Self, String> {
        let EmulatedDevice::WacomIntuos5 = device;
        let (emulated, handle) = SyntheticDevice::emulate(WacomIntuos5::new())
            .map_err(|error| format!("emulated {}: {error:?}", device.name()))?;
        let exporter = UsbExporter::new(Box::new(emulated)).map_err(|(refusal, _)| {
            format!(
                "emulated {} refused by the shared policy: {refusal}",
                device.name()
            )
        })?;
        let device_msg = device_message(&exporter);
        let exporter = Arc::new(exporter);
        let stop = Arc::new(AtomicBool::new(false));
        let (sender, completions) = tokio::sync::mpsc::unbounded_channel();
        let pump = {
            let exporter = Arc::clone(&exporter);
            let stop = Arc::clone(&stop);
            std::thread::Builder::new()
                .name("arcen-usb-emulate-pump".to_owned())
                .spawn(move || pump(&exporter, &stop, &sender))
                .map_err(|error| format!("start the emulated USB completion pump: {error}"))?
        };
        let player = {
            let stop = Arc::clone(&stop);
            std::thread::Builder::new()
                .name("arcen-usb-emulate-player".to_owned())
                .spawn(move || play(&handle, playback, &stop))
                .map_err(|error| format!("start the emulated pen player: {error}"))?
        };
        tracing::info!(
            target: crate::logging::target::USB,
            device = device.name(),
            vendor_id = device_msg.vendor_id,
            product_id = device_msg.product_id,
            strokes = playback.strokes,
            "emulated USB device bridged for Hard USB (lab; no physical capture)"
        );
        Ok(Self {
            exporter,
            device: device_msg,
            completions,
            stop,
            threads: vec![pump, player],
        })
    }

    #[must_use]
    pub fn device(&self) -> UsbHardDeviceMsg {
        self.device.clone()
    }

    /// Admits one URB.
    ///
    /// # Errors
    ///
    /// Returns the exporter's refusal, which ends the bridge.
    pub fn submit(&self, header: UsbUrbSubmitHeader, payload: &[u8]) -> Result<(), String> {
        self.exporter
            .submit(
                UrbSubmission {
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
            .map_err(|error| error.to_string())
    }

    /// Cancels one URB and returns its encoded terminal completion.
    ///
    /// # Errors
    ///
    /// Returns the exporter's refusal.
    pub fn cancel(
        &self,
        generation: AttachmentGeneration,
        urb_id: UrbId,
    ) -> Result<Vec<u8>, String> {
        let cancelled = self
            .exporter
            .cancel(generation, urb_id)
            .map_err(|error| error.to_string())?;
        encode(&cancelled)
    }

    /// The next encoded completion.
    ///
    /// # Errors
    ///
    /// Returns a message once the pump has stopped.
    pub async fn next_completion(&mut self) -> Result<Vec<u8>, String> {
        self.completions
            .recv()
            .await
            .ok_or_else(|| "emulated USB device stopped".to_owned())
    }

    /// Stops the player and the pump and releases the device.
    pub async fn shutdown(self) {
        self.stop.store(true, Ordering::Release);
        let exporter = Arc::clone(&self.exporter);
        let threads = self.threads;
        #[allow(
            clippy::disallowed_methods,
            reason = "awaited to completion; the release drains for at most SHUTDOWN_DRAIN and both threads stop within one poll of the stop flag"
        )]
        let _ = tokio::task::spawn_blocking(move || {
            let _ = exporter.release(ReleaseReason::SessionEnded);
            for thread in threads {
                let _ = thread.join();
            }
        })
        .await;
    }
}

fn device_message(exporter: &UsbExporter) -> UsbHardDeviceMsg {
    let facts = exporter.facts();
    UsbHardDeviceMsg {
        vendor_id: facts.snapshot.id.vendor_id,
        product_id: facts.snapshot.id.product_id,
        bcd_device: facts.snapshot.id.bcd_device,
        device_class: facts.snapshot.device_class,
        speed: facts.speed,
        hid_report_descriptors: facts
            .report_descriptors
            .iter()
            .map(|report| UsbHidReportDescriptorMsg::new(report.interface, &report.descriptor))
            .collect(),
    }
}

fn encode(completion: &ExportedCompletion) -> Result<Vec<u8>, String> {
    let actual_length = u32::try_from(completion.data.len())
        .map_err(|_| "emulated USB completion length overflow".to_owned())?;
    encode_usb_urb_complete(
        UsbUrbCompletionHeader {
            generation: completion.generation,
            urb_id: completion.urb_id,
            status: completion.status,
            actual_length,
        },
        &completion.data,
    )
    .map_err(|error| format!("encode emulated USB completion: {error:?}"))
}

fn pump(
    exporter: &UsbExporter,
    stop: &AtomicBool,
    sender: &tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
) {
    while !stop.load(Ordering::Acquire) {
        match exporter.next_completion_within(PUMP_WAIT) {
            Ok(Some(completion)) => {
                if completion.status != UrbStatus::Success {
                    tracing::debug!(
                        target: crate::logging::target::USB,
                        urb = completion.urb_id.get(),
                        status = ?completion.status,
                        "emulated USB transfer ended without success"
                    );
                }
                let Ok(frame) = encode(&completion) else {
                    return;
                };
                if sender.send(frame).is_err() {
                    return;
                }
            }
            Ok(None) => {}
            Err(_) => return,
        }
    }
}

/// Sleeps for `duration` unless stopped; `true` when still running.
fn pause(stop: &AtomicBool, duration: Duration) -> bool {
    let _ = wait_until(stop, duration, || false);
    !stop.load(Ordering::Acquire)
}

/// Waits for `ready`, polling `stop`; `false` on timeout or stop.
fn wait_until(stop: &AtomicBool, timeout: Duration, ready: impl Fn() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while !stop.load(Ordering::Acquire) {
        if ready() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

fn play(handle: &SyntheticHandle<WacomIntuos5>, playback: Playback, stop: &AtomicBool) {
    set_status(PlaybackStatus::WaitingForHost);
    match play_strokes(handle, playback, stop) {
        Ok(reports) => {
            tracing::info!(
                target: crate::logging::target::USB,
                strokes = playback.strokes,
                reports,
                "emulated pen playback complete"
            );
            set_status(PlaybackStatus::Done {
                strokes: playback.strokes,
                reports,
            });
        }
        Err(error) => {
            tracing::warn!(
                target: crate::logging::target::USB,
                %error,
                "emulated pen playback stopped"
            );
            set_status(PlaybackStatus::Failed(error));
        }
    }
}

fn play_strokes(
    handle: &SyntheticHandle<WacomIntuos5>,
    playback: Playback,
    stop: &AtomicBool,
) -> Result<usize, String> {
    if !wait_until(stop, HOST_READY_TIMEOUT, || {
        handle.inspect(WacomIntuos5::wacom_mode)
    }) {
        return Err("the host never switched the tablet into Wacom mode".to_owned());
    }
    tracing::info!(
        target: crate::logging::target::USB,
        "host switched the emulated tablet into Wacom mode"
    );
    if !wait_until(stop, HOST_READY_TIMEOUT, || {
        handle.held_on(INTUOS5_PEN_IN) > 0
    }) {
        return Err(
            "the host never polled the pen endpoint (nobody opened the pen device)".to_owned(),
        );
    }
    tracing::info!(
        target: crate::logging::target::USB,
        lead_in_ms = playback.lead_in.as_millis(),
        "host polls the emulated pen; playing after the lead-in"
    );
    if !pause(stop, playback.lead_in) {
        return Err("stopped before playback".to_owned());
    }
    let mut reports = 0;
    for stroke in 1..=playback.strokes {
        set_status(PlaybackStatus::Playing { stroke });
        let started = Instant::now();
        let script = playback.stroke.script(stroke);
        for step in &script {
            if stop.load(Ordering::Acquire) {
                return Err(format!("stopped during stroke {stroke}"));
            }
            if !handle.deliver_when_polled(
                INTUOS5_PEN_IN,
                step.report.encode().to_vec(),
                POLL_TIMEOUT,
            ) {
                return Err(format!(
                    "stroke {stroke}: the host stopped polling the pen at phase {}",
                    step.phase
                ));
            }
            reports += 1;
            std::thread::sleep(SCRIPTED_MINIMUM_INTERVAL);
        }
        tracing::info!(
            target: crate::logging::target::USB,
            stroke,
            script = ?playback.stroke,
            reports = script.len(),
            elapsed_ms = started.elapsed().as_millis(),
            "emulated pen stroke delivered"
        );
        if playback.stroke.rests() {
            tracing::info!(
                target: crate::logging::target::USB,
                hold_ms = RESTING_HOLD.as_millis(),
                "emulated pen rests; holding before the playback ends"
            );
            if !pause(stop, RESTING_HOLD) {
                return Err("stopped while the pen rested".to_owned());
            }
            break;
        }
        if stroke < playback.strokes && !pause(stop, playback.gap) {
            return Err("stopped between strokes".to_owned());
        }
    }
    Ok(reports)
}

/// Tilts every pen sample of `script`: X sweeps from +40 to -30 and Y from
/// -25 to +20 (the driver's `ABS_TILT_*` units, which the host reads as
/// degrees), so a reader sees both signs on both axes.
fn tilted(mut script: Vec<ScriptedReport>) -> Vec<ScriptedReport> {
    let last = script.len().saturating_sub(1).max(1);
    for (index, step) in script.iter_mut().enumerate() {
        if let Intuos5PenReport::Pen(state) = &mut step.report {
            let sweep = |from: i32, to: i32| {
                let value = from
                    + (to - from) * i32::try_from(index).unwrap_or(0)
                        / i32::try_from(last).unwrap_or(1);
                i8::try_from(value.clamp(-64, 63)).unwrap_or(0)
            };
            state.tilt_x = sweep(40, -30);
            state.tilt_y = sweep(-25, 20);
        }
    }
    script
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcen_protocol::decode_usb_urb_complete;
    use arcen_usb_bridge::{EndpointAddress, SetupPacket, TransferKind};
    use std::num::{NonZeroU32, NonZeroU64};

    fn generation() -> AttachmentGeneration {
        AttachmentGeneration::new(NonZeroU64::MIN)
    }

    fn header(
        urb: u32,
        endpoint: u8,
        kind: TransferKind,
        length: u32,
        setup: Option<SetupPacket>,
    ) -> UsbUrbSubmitHeader {
        UsbUrbSubmitHeader {
            generation: generation(),
            urb_id: UrbId::new(NonZeroU32::new(urb).unwrap()),
            endpoint: EndpointAddress(endpoint),
            transfer_kind: kind,
            timeout_ms: 1_000,
            declared_length: length,
            setup,
        }
    }

    async fn next(responder: &mut EmulatedResponder) -> (UsbUrbCompletionHeader, Vec<u8>) {
        let frame = tokio::time::timeout(Duration::from_secs(10), responder.next_completion())
            .await
            .unwrap()
            .unwrap();
        let (header, data) = decode_usb_urb_complete(&frame).unwrap();
        (header, data.to_vec())
    }

    #[test]
    fn the_parser_names_the_accepted_devices() {
        assert_eq!(
            EmulatedDevice::parse("wacom-intuos5"),
            Ok(EmulatedDevice::WacomIntuos5)
        );
        assert!(EmulatedDevice::parse("gamepad")
            .unwrap_err()
            .contains("wacom-intuos5"));
    }

    /// The two branches' new strokes live side by side: every name parses
    /// to its own script, and a wrong name lists them all.
    #[test]
    fn every_stroke_name_parses_to_its_own_script() {
        let names = ["scripted", "landing", "alternate", "realistic", "tilted"];
        for name in names {
            let script = StrokeScript::parse(name).expect(name);
            assert!(!script.script(1).is_empty(), "{name}");
        }
        assert_eq!(
            StrokeScript::Realistic.script(1),
            intuos5_realistic_stroke()
        );
        assert_ne!(
            StrokeScript::Tilted.script(1),
            StrokeScript::Scripted.script(1)
        );
        let refused = StrokeScript::parse("wobbly").unwrap_err();
        for name in names {
            assert!(refused.contains(name), "{refused}");
        }
    }

    #[test]
    fn the_tilted_stroke_sweeps_both_tilt_axes_through_both_signs() {
        assert_eq!(StrokeScript::parse("tilted"), Ok(StrokeScript::Tilted));
        let tilts: Vec<(i8, i8)> = tilted(intuos5_scripted_stroke())
            .iter()
            .filter_map(|step| match step.report {
                Intuos5PenReport::Pen(state) => Some((state.tilt_x, state.tilt_y)),
                _ => None,
            })
            .collect();
        assert!(tilts.iter().any(|(x, _)| *x > 20) && tilts.iter().any(|(x, _)| *x < -20));
        assert!(tilts.iter().any(|(_, y)| *y > 10) && tilts.iter().any(|(_, y)| *y < -10));
    }

    /// The hello carries the emulated Wacom's identity and the report
    /// descriptors of all three interfaces; control requests are answered
    /// from the descriptors; the mode switch is acknowledged and echoed; and
    /// the scripted stroke arrives on the pen endpoint once the host polls.
    #[tokio::test]
    async fn the_emulated_wacom_answers_like_a_captured_one_and_plays_its_stroke() {
        let mut responder = EmulatedResponder::start(
            EmulatedDevice::WacomIntuos5,
            Playback {
                strokes: 1,
                lead_in: Duration::ZERO,
                gap: Duration::ZERO,
                stroke: StrokeScript::Scripted,
            },
        )
        .unwrap();
        let device = responder.device();
        assert_eq!((device.vendor_id, device.product_id), (0x056a, 0x0317));
        assert_eq!(device.hid_report_descriptors.len(), 3);

        let setup = |request_type, request, value, index, length| SetupPacket {
            request_type,
            request,
            value,
            index,
            length,
        };
        responder
            .submit(
                header(
                    1,
                    0x80,
                    TransferKind::Control,
                    18,
                    Some(setup(0x80, 6, 0x0100, 0, 18)),
                ),
                &[],
            )
            .unwrap();
        let (done, data) = next(&mut responder).await;
        assert_eq!(done.status, UrbStatus::Success);
        assert_eq!(&data[8..12], &[0x6a, 0x05, 0x17, 0x03]);

        responder
            .submit(
                header(
                    2,
                    0x00,
                    TransferKind::Control,
                    0,
                    Some(setup(0x21, 0x09, 0x0302, 0, 2)),
                ),
                &[2, 2],
            )
            .unwrap();
        let (done, _) = next(&mut responder).await;
        assert_eq!(done.status, UrbStatus::Success);
        responder
            .submit(
                header(
                    3,
                    0x80,
                    TransferKind::Control,
                    2,
                    Some(setup(0xa1, 0x01, 0x0302, 0, 2)),
                ),
                &[],
            )
            .unwrap();
        let (_, data) = next(&mut responder).await;
        assert_eq!(data, [2, 2]);

        let script = intuos5_scripted_stroke();
        for (index, step) in script.iter().enumerate() {
            let urb = 10 + u32::try_from(index).unwrap();
            responder
                .submit(header(urb, 0x81, TransferKind::Interrupt, 10, None), &[])
                .unwrap();
            let (done, data) = next(&mut responder).await;
            assert_eq!(done.urb_id.get(), urb);
            assert_eq!(data, step.report.encode(), "{}", step.phase);
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while !matches!(status(), PlaybackStatus::Done { .. }) && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            status(),
            PlaybackStatus::Done {
                strokes: 1,
                reports: script.len()
            }
        );

        // A cancelled held poll answers at once.
        responder
            .submit(header(500, 0x82, TransferKind::Interrupt, 64, None), &[])
            .unwrap();
        let cancelled = responder
            .cancel(generation(), UrbId::new(NonZeroU32::new(500).unwrap()))
            .unwrap();
        let (done, _) = decode_usb_urb_complete(&cancelled).unwrap();
        assert_eq!(done.status, UrbStatus::Cancelled);
        responder.shutdown().await;
    }
}
