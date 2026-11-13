// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Linux Hard USB bridge adapter.
//!
//! The privileged helper owns the virtual host controller port. Network,
//! session policy, and authentication stay in Pier; the controller's wire
//! format (USB/IP for the in-tree `vhci-hcd`, ioctls for the lab `usb-vhci`)
//! never crosses this module boundary ([`backend`]).

#[cfg(feature = "native-tablet")]
pub mod backend;
pub mod kmod;
#[cfg_attr(not(feature = "native-tablet"), allow(dead_code))]
pub mod seat;
#[cfg(feature = "native-tablet")]
pub mod tablet_pointer;
#[cfg(feature = "native-tablet")]
mod usbip;
#[cfg(feature = "native-tablet")]
mod vhci;

#[cfg(feature = "native-tablet")]
use arcen_protocol::messages::UsbHardDeviceMsg;
#[cfg(feature = "native-tablet")]
use arcen_protocol::{
    decode_usb_urb_complete, encode_usb_urb_cancel, encode_usb_urb_submit, UsbUrbSubmitHeader,
};
#[cfg(feature = "native-tablet")]
use arcen_usb_bridge::{
    AttachmentGeneration, ControlResponse, PenSample, PenSwitch, PenSwitches,
    SyntheticTabletDevice, TransferDirection, TransferKind, UrbId, UrbStatus, UsbSpeed,
};
#[cfg(feature = "native-tablet")]
use backend::{HcdEvent, HcdUrb, VirtualHcd};
#[cfg(feature = "native-tablet")]
use std::collections::BTreeMap;
#[cfg(feature = "native-tablet")]
use std::io::{Read, Write};
#[cfg(feature = "native-tablet")]
use std::num::{NonZeroU32, NonZeroU64};
#[cfg(feature = "native-tablet")]
use std::path::Path;
#[cfg(feature = "native-tablet")]
use std::process::ExitCode;
#[cfg(not(feature = "native-tablet"))]
use std::process::ExitCode;
#[cfg(feature = "native-tablet")]
use std::time::{Duration, Instant};
#[cfg(feature = "native-tablet")]
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
#[cfg(feature = "native-tablet")]
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout};

#[cfg(feature = "native-tablet")]
const SELF_TEST_RUNTIME: Duration = Duration::from_secs(8);
/// The helper's one stderr line once its port is connected under its seat
/// rule: `ARCEN_USB_BRIDGE_ATTACHED devpath=<port path> seat=<seat>`.
#[cfg(feature = "native-tablet")]
const ATTACHED_LINE: &str = "ARCEN_USB_BRIDGE_ATTACHED";
#[cfg(feature = "native-tablet")]
const MAX_HELPER_FRAME_BYTES: usize = arcen_usb_bridge::MAX_TRANSFER_BYTES + 64;
/// A Hard USB device this host agreed to bridge.
#[derive(Debug, Clone)]
pub struct AuthorizedTablet {
    /// Bus speed the virtual port is connected at.
    pub speed: UsbSpeed,
    /// The identity the device descriptor must report once attached.
    pub identity: arcen_usb_bridge::UsbDeviceId,
    /// The classified report descriptors, checked again against what the
    /// device returns once it is attached.
    pub report_descriptors: Vec<arcen_usb_bridge::InterfaceReportDescriptor>,
    /// What the descriptors declared.
    pub profile: arcen_usb_bridge::PenTabletProfile,
}

/// Decides whether the Deck's captured device may become a virtual USB device
/// on this host.
///
/// The decision is vendor-neutral and independent of the Deck's own: the
/// device class and the HID report descriptors the Deck supplied are
/// classified here with `arcen_usb_bridge::authorize_hard_usb_tablet` before
/// anything reaches `usb-vhci`. Vendor and product ids are logged, never
/// trusted. A Deck that supplies no descriptors (0.15 and older) is refused
/// rather than admitted by vendor id.
///
/// The synthetic lab tablet is admitted only with `ARCEN_USB_HARD_SYNTHETIC=1`,
/// and goes through the same classification.
///
/// # Errors
///
/// Returns a bounded operator-facing reason.
#[cfg(feature = "native-tablet")]
pub fn authorize_device(device: Option<&UsbHardDeviceMsg>) -> Result<AuthorizedTablet, String> {
    let device = device.ok_or_else(|| "the Deck did not capture a USB device".to_owned())?;
    let synthetic = device.vendor_id == arcen_usb_bridge::ARCEN_LAB_VENDOR_ID
        && device.product_id == arcen_usb_bridge::ARCEN_LAB_PRODUCT_ID;
    if synthetic && std::env::var("ARCEN_USB_HARD_SYNTHETIC").ok().as_deref() != Some("1") {
        return Err("the synthetic lab tablet is not enabled on this host".to_owned());
    }
    let report_descriptors = device
        .report_descriptors()
        .map_err(|error| format!("the Deck's HID report descriptors are malformed: {error}"))?;
    let profile =
        arcen_usb_bridge::authorize_hard_usb_tablet(device.device_class, &report_descriptors)
            .map_err(|denial| format!("the captured USB device was refused: {denial}"))?;
    Ok(AuthorizedTablet {
        speed: device.speed,
        identity: arcen_usb_bridge::UsbDeviceId {
            vendor_id: device.vendor_id,
            product_id: device.product_id,
            bcd_device: device.bcd_device,
        },
        report_descriptors,
        profile,
    })
}

#[cfg(feature = "native-tablet")]
impl AuthorizedTablet {
    /// The guard that holds every descriptor the attached device serves to
    /// this authorization.
    #[must_use]
    pub fn guard(&self) -> arcen_usb_bridge::BridgedDeviceGuard {
        arcen_usb_bridge::BridgedDeviceGuard::new(self.identity, self.report_descriptors.clone())
    }
}

/// Without the Native tablet importer nothing is ever bridged.
///
/// # Errors
///
/// Always.
#[cfg(not(feature = "native-tablet"))]
pub fn authorize_device(
    _device: Option<&arcen_protocol::messages::UsbHardDeviceMsg>,
) -> Result<AuthorizedTablet, String> {
    Err(NOT_BUILT.to_owned())
}

#[cfg(not(feature = "native-tablet"))]
use arcen_usb_bridge::UsbSpeed;

/// The administrator's effective Native tablet decision, fixed at Pier start
/// ([`configure_native_tablet`]); the virtual host controller is loaded once,
/// then, so a change needs a Pier restart.
static NATIVE_TABLET: std::sync::OnceLock<arcen_session::pier_config::NativeTabletPolicy> =
    std::sync::OnceLock::new();

/// Fixes the Native tablet decision from `usb.native_tablet`. Call once,
/// before [`prepare_backend`].
pub fn configure_native_tablet(
    setting: Option<arcen_session::pier_config::NativeTabletSetting>,
) -> arcen_session::pier_config::NativeTabletPolicy {
    let policy = arcen_session::pier_config::NativeTabletPolicy::resolve(setting);
    *NATIVE_TABLET.get_or_init(|| policy)
}

/// The effective decision; unconfigured processes (tests, helpers) see the
/// configuration-less default.
#[must_use]
pub fn native_tablet_policy() -> arcen_session::pier_config::NativeTabletPolicy {
    *NATIVE_TABLET.get_or_init(|| arcen_session::pier_config::NativeTabletPolicy::resolve(None))
}

/// Whether this binary contains Native tablet, the administrator enabled it,
/// and a virtual host controller is usable: the in-tree `vhci-hcd` (loaded
/// at Pier start) or, as a development fallback, `/dev/usb-vhci`.
#[must_use]
pub fn runtime_available() -> bool {
    runtime_unavailable_reason().is_none()
}

/// Why [`runtime_available`] is false, for logs, `validate-config` and the
/// Deck.
#[must_use]
pub fn runtime_unavailable_reason() -> Option<String> {
    #[cfg(feature = "native-tablet")]
    {
        if let Some(reason) = native_tablet_policy().disabled_reason() {
            return Some(reason.to_owned());
        }
        // Cheap and non-blocking: the module was loaded (if it could be) at
        // Pier start by `prepare_backend`.
        backend::available_backend().err()
    }

    #[cfg(not(feature = "native-tablet"))]
    {
        Some(NOT_BUILT.to_owned())
    }
}

/// A build without the Native tablet importer (`native-tablet` feature).
#[cfg_attr(feature = "native-tablet", allow(dead_code))]
const NOT_BUILT: &str = "this Pier build does not contain Native tablet";

/// Loads Native tablet's virtual host controller once, at Pier start, so
/// that connections only check sysfs ([`runtime_available`] never blocks).
/// Nothing is loaded unless the administrator enabled Native tablet.
pub fn prepare_backend() {
    #[cfg(feature = "native-tablet")]
    if native_tablet_policy().enabled {
        match backend::prepare() {
            Ok(kind) => tracing::info!(
                target: "arcen::usb",
                backend = kind.label(),
                "Native tablet virtual host controller ready"
            ),
            Err(reason) => tracing::warn!(
                target: "arcen::usb",
                %reason,
                "Native tablet virtual host controller unavailable"
            ),
        }
    }
}

/// One operator line about Native tablet's virtual host controller, for
/// `validate-config` and Pier start: `None` when this build has no Hard USB
/// importer.
#[must_use]
pub fn native_tablet_status() -> Option<String> {
    if !cfg!(feature = "native-tablet") {
        return None;
    }
    let policy = native_tablet_policy();
    if let Some(reason) = policy.disabled_reason() {
        return Some(format!("Native tablet: off: {reason}"));
    }
    Some(native_tablet_line(&kmod::vhci_hcd_facts()))
}

/// The status line from the controller's facts. The module is loaded only
/// at Pier start, so "installed but not loaded" says how to load it.
fn native_tablet_line(facts: &arcen_usb_bridge::VhciHcdFacts) -> String {
    if facts.loaded {
        format!(
            "Native tablet: vhci-hcd is loaded for kernel {}",
            facts.running_kernel
        )
    } else if facts.module_for_running_kernel {
        format!(
            "Native tablet: vhci-hcd is installed for kernel {} but not loaded; the Pier loads it \
             when it starts: restart the Pier, or run `modprobe vhci-hcd`",
            facts.running_kernel
        )
    } else {
        format!(
            "Native tablet: unavailable: {}",
            arcen_usb_bridge::missing_vhci_hcd_reason(facts)
        )
    }
}

/// How the attached session receives physical input devices: a headless
/// Wayland session has no seat; a dedicated Xorg desktop hot-adds a bridged
/// device tagged for it alone (every other X server ignores it); without a
/// session there is only the console seat.
#[must_use]
pub fn session_seat(
    session: Option<&crate::session::lifecycle::SessionMetadata>,
) -> arcen_usb_bridge::HardUsbSessionSeat {
    match session {
        // A native Wayland desktop (ADR 0015) owns its logind seat: the
        // bridged device joins that seat and its compositor hot-adds it.
        Some(session)
            if (session.session_type == "wayland" || session.wayland.is_some())
                && session.native_seat.is_some() =>
        {
            arcen_usb_bridge::HardUsbSessionSeat::ArcenSeat
        }
        Some(session) if session.session_type == "wayland" || session.wayland.is_some() => {
            arcen_usb_bridge::HardUsbSessionSeat::Seatless
        }
        Some(_) => arcen_usb_bridge::HardUsbSessionSeat::X11Desktop,
        None => arcen_usb_bridge::HardUsbSessionSeat::ConsoleSeat,
    }
}

/// Whether Hard USB may be bridged into `session` at all; a seatless
/// (Wayland) session is refused before any helper or virtual controller is
/// touched, because the bridged device would reach the physical seat.
///
/// # Errors
///
/// The shared denial, whose reason is shown to the Deck.
pub fn session_admission(
    session: Option<&crate::session::lifecycle::SessionMetadata>,
) -> Result<(), arcen_usb_bridge::HardUsbSessionDenial> {
    arcen_usb_bridge::admit_hard_usb_session(session_seat(session))
}

/// A seat nobody runs, for the helper's self-tests: their synthetic device
/// reaches no session and no console.
///
/// # Errors
///
/// When the OS has no randomness.
#[cfg(feature = "native-tablet")]
fn self_test_seat() -> std::io::Result<arcen_usb_bridge::BridgeSeat> {
    let mut bytes = [0_u8; 8];
    getrandom::getrandom(&mut bytes).map_err(std::io::Error::other)?;
    let id: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    arcen_usb_bridge::BridgeSeat::parse(&format!("seat-arcen-pen-selftest-{id}"))
        .map_err(std::io::Error::other)
}

/// Whom a bridged device belongs to for `session`: a native Wayland
/// desktop's own `seat-arcen-pen-<id>` (its compositor hot-adds the device
/// through logind), otherwise the dedicated Xorg desktop on the session's
/// display (its server hot-adds the device by its Xorg tag).
///
/// # Errors
///
/// When the OS has no randomness, the native seat's name is not an Arcen
/// desktop seat, or the display is not a dedicated desktop's.
#[cfg(feature = "native-tablet")]
pub fn bridge_seat_for(
    session: Option<&crate::session::lifecycle::SessionMetadata>,
) -> std::io::Result<arcen_usb_bridge::BridgeSeat> {
    match session.and_then(|session| session.native_seat.as_deref()) {
        Some(seat) => match arcen_usb_bridge::BridgeSeat::parse(seat) {
            Ok(native @ arcen_usb_bridge::BridgeSeat::NativeSeat { .. }) => Ok(native),
            _ => Err(std::io::Error::other(format!(
                "{seat:?} is not a native desktop seat"
            ))),
        },
        None => {
            let display = session
                .and_then(|session| x11_display_number(&session.display))
                .ok_or_else(|| std::io::Error::other("not a dedicated Xorg desktop display"))?;
            let mut bytes = [0_u8; 8];
            getrandom::getrandom(&mut bytes).map_err(std::io::Error::other)?;
            arcen_usb_bridge::BridgeSeat::x11_session(display, bytes).map_err(std::io::Error::other)
        }
    }
}

/// The display number of a dedicated Xorg desktop (`:1`–`:99`).
#[must_use]
pub fn x11_display_number(display: &str) -> Option<u16> {
    let number = display.strip_prefix(':')?;
    let value = number.parse::<u16>().ok()?;
    (number == value.to_string()
        && (1..=arcen_usb_bridge::bridge_seat::MAX_X11_DISPLAY).contains(&value))
    .then_some(value)
}

/// Generates one nonzero attachment generation from OS randomness.
#[cfg(feature = "native-tablet")]
pub fn fresh_attachment_generation() -> std::io::Result<AttachmentGeneration> {
    let mut bytes = [0_u8; 8];
    getrandom::getrandom(&mut bytes).map_err(std::io::Error::other)?;
    let value = NonZeroU64::new(u64::from_le_bytes(bytes)).unwrap_or(NonZeroU64::MIN);
    Ok(AttachmentGeneration::new(value))
}

/// Entry point for the fused privileged helper.
#[must_use]
pub fn helper_main(args: &[String]) -> ExitCode {
    #[cfg(not(feature = "native-tablet"))]
    {
        let _ = args;
        eprintln!("usb-bridge-helper: {NOT_BUILT}");
        return ExitCode::FAILURE;
    }
    #[cfg(feature = "native-tablet")]
    {
        helper_main_feature(args)
    }
}

/// Runs the real fused helper through its bounded protocol-frame IPC.
#[must_use]
pub fn ipc_self_test_main() -> ExitCode {
    #[cfg(not(feature = "native-tablet"))]
    {
        eprintln!("usb-bridge-ipc-self-test: {NOT_BUILT}");
        ExitCode::FAILURE
    }
    #[cfg(feature = "native-tablet")]
    {
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(error) => {
                eprintln!("usb-bridge-ipc-self-test: create runtime: {error}");
                return ExitCode::FAILURE;
            }
        };
        match runtime.block_on(run_ipc_self_test()) {
            Ok(count) => {
                println!("USB_BRIDGE_IPC_SELF_TEST_OK completed_urbs={count}");
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("usb-bridge-ipc-self-test: {error}");
                ExitCode::FAILURE
            }
        }
    }
}

#[cfg(feature = "native-tablet")]
async fn run_ipc_self_test() -> Result<u64, String> {
    let binary = crate::current_pier_exe()
        .ok_or_else(|| "current fused Pier binary is unavailable".to_owned())?;
    let generation = AttachmentGeneration::new(NonZeroU64::MIN);
    let mut process = BridgeProcess::spawn(
        &binary,
        generation,
        UsbSpeed::High,
        arcen_usb_bridge::BridgedDeviceGuard::new(
            SyntheticTabletDevice::identity(),
            vec![arcen_usb_bridge::InterfaceReportDescriptor {
                interface: 0,
                descriptor: arcen_usb_bridge::SYNTHETIC_TABLET_REPORT_DESCRIPTOR.to_vec(),
            }],
        ),
        &self_test_seat().map_err(|error| error.to_string())?,
    )
    .await
    .map_err(|error| error.to_string())?;
    let mut tablet = SyntheticTabletDevice::default();
    let report = PenSample {
        x: 0.5,
        y: 0.5,
        pressure: 0.25,
        tilt_x_degrees: 10.0,
        tilt_y_degrees: -10.0,
        switches: PenSwitches::default().with(PenSwitch::InRange, true),
    }
    .encode_report();
    let deadline = tokio::time::Instant::now() + SELF_TEST_RUNTIME;
    let mut configured_at: Option<tokio::time::Instant> = None;
    let mut completed = 0_u64;
    while tokio::time::Instant::now() < deadline {
        if configured_at.is_some_and(|configured| configured.elapsed() >= Duration::from_secs(2)) {
            process.shutdown().await;
            return Ok(completed);
        }
        let frame =
            match tokio::time::timeout(Duration::from_millis(250), process.next_frame()).await {
                Ok(result) => result.map_err(|error| error.to_string())?,
                Err(_) => continue,
            };
        if frame.first().copied() == Some(arcen_protocol::FrameType::UsbBridgeUrbCancel as u8) {
            let (generation, urb_id) = arcen_protocol::decode_usb_urb_cancel(&frame)
                .map_err(|error| format!("{error:?}"))?;
            let completion = arcen_protocol::encode_usb_urb_complete(
                arcen_protocol::UsbUrbCompletionHeader {
                    generation,
                    urb_id,
                    status: UrbStatus::Cancelled,
                    actual_length: 0,
                },
                &[],
            )
            .map_err(|error| format!("{error:?}"))?;
            process
                .send_frame(&completion)
                .await
                .map_err(|error| error.to_string())?;
            continue;
        }
        let (header, payload) =
            arcen_protocol::decode_usb_urb_submit(&frame).map_err(|error| format!("{error:?}"))?;
        let (status, response) = match header.transfer_kind {
            arcen_usb_bridge::TransferKind::Control => match tablet.handle_control(
                header
                    .setup
                    .ok_or_else(|| "control URB has no setup".to_owned())?,
            ) {
                ControlResponse::Ack => (UrbStatus::Success, Vec::new()),
                ControlResponse::Data(data) => (UrbStatus::Success, data),
                ControlResponse::Stall => (UrbStatus::Stall, Vec::new()),
            },
            arcen_usb_bridge::TransferKind::Interrupt
                if header.endpoint.direction() == TransferDirection::In =>
            {
                let length = usize::try_from(header.declared_length).unwrap_or(usize::MAX);
                (
                    UrbStatus::Success,
                    report[..length.min(report.len())].to_vec(),
                )
            }
            arcen_usb_bridge::TransferKind::Interrupt => (UrbStatus::Stall, Vec::new()),
        };
        let _ = payload;
        let completion = arcen_protocol::encode_usb_urb_complete(
            arcen_protocol::UsbUrbCompletionHeader {
                generation,
                urb_id: header.urb_id,
                status,
                actual_length: u32::try_from(response.len()).unwrap_or(0),
            },
            &response,
        )
        .map_err(|error| format!("{error:?}"))?;
        process
            .send_frame(&completion)
            .await
            .map_err(|error| error.to_string())?;
        completed = completed.saturating_add(1);
        if tablet.configuration() == 1 && configured_at.is_none() {
            configured_at = Some(tokio::time::Instant::now());
        }
    }
    process.shutdown().await;
    Err(format!(
        "kernel did not configure the synthetic tablet; completed_urbs={completed}"
    ))
}

#[cfg(feature = "native-tablet")]
fn helper_main_feature(args: &[String]) -> ExitCode {
    let result = match args {
        [argument] if argument == "--self-test" => run_self_test(),
        [argument, generation, speed, seat_flag, seat]
            if argument == "--bridge" && seat_flag == "--seat" =>
        {
            generation
                .parse::<u64>()
                .ok()
                .and_then(NonZeroU64::new)
                .ok_or_else(|| "bridge generation must be nonzero".to_owned())
                .and_then(|generation| {
                    let seat = arcen_usb_bridge::BridgeSeat::parse(seat)
                        .map_err(|error| format!("--seat: {error}"))?;
                    parse_speed(speed).and_then(|speed| {
                        run_bridge(AttachmentGeneration::new(generation), speed, &seat)
                    })
                })
        }
        _ => Err(
            "usage: arcen-pier usb-bridge-helper --self-test | --bridge GENERATION low|full|high --seat SEAT"
                .to_owned(),
        ),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("usb-bridge-helper: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Async Pier-side handle for the isolated privileged helper.
#[cfg(feature = "native-tablet")]
pub struct BridgeProcess {
    /// `None` only once `shutdown` or `Drop` has taken it.
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    frames: tokio::sync::mpsc::Receiver<std::io::Result<Vec<u8>>>,
    reader_task: tokio::task::JoinHandle<()>,
    stderr: BufReader<ChildStderr>,
    /// Checks every device, configuration and report descriptor the device
    /// returns against what was authorized before the device was created.
    guard: arcen_usb_bridge::BridgedDeviceGuard,
    generation: AttachmentGeneration,
    /// Helper diagnostics read while waiting for its attached line.
    early_diagnostic: String,
}

#[cfg(not(feature = "native-tablet"))]
pub struct BridgeProcess;

/// Without the Native tablet importer an attachment never holds a bridge.
#[cfg(not(feature = "native-tablet"))]
pub type BridgeAttachment = BridgeProcess;
/// What one attachment holds of a Hard USB bridge.
#[cfg(feature = "native-tablet")]
pub enum BridgeAttachment {
    /// The attachment's own bridge, which ends with the attachment: the
    /// desktop's compositor or X server adds and removes the tablet itself.
    PerAttachment(Box<BridgeProcess>),
}

#[cfg(feature = "native-tablet")]
impl BridgeAttachment {
    /// The next frame for the Deck.
    ///
    /// # Errors
    ///
    /// When the bridge ended or its device contradicted its authorization.
    pub async fn next_frame(&mut self) -> std::io::Result<Vec<u8>> {
        match self {
            Self::PerAttachment(process) => process.next_frame().await,
        }
    }

    /// Passes one Deck frame to the bridge.
    ///
    /// # Errors
    ///
    /// When the frame is out of bound, refused by the guard, or the bridge
    /// ended.
    pub async fn send_frame(&mut self, frame: &[u8]) -> std::io::Result<()> {
        match self {
            Self::PerAttachment(process) => process.send_frame(frame).await,
        }
    }

    /// The helper's diagnostic.
    pub async fn diagnostic(&mut self) -> String {
        match self {
            Self::PerAttachment(process) => process.diagnostic().await,
        }
    }

    /// Ends this attachment and its bridge.
    pub async fn shutdown(self) {
        match self {
            Self::PerAttachment(process) => (*process).shutdown().await,
        }
    }
}

/// Where a bridge's device is: its port and seat, from which its verified
/// input nodes are found.
#[cfg(feature = "native-tablet")]
#[derive(Debug, Clone)]
pub struct BridgePort {
    pub prefix: String,
    pub seat: arcen_usb_bridge::BridgeSeat,
}

#[cfg(feature = "native-tablet")]
impl BridgePort {
    /// Reports the bridged pen's position into `pointer` until the returned
    /// reader is dropped or the device goes away: the one Linux pen adapter,
    /// for Xorg and Wayland desktops alike (ADR 0023).
    #[must_use]
    pub fn report_pen_position(
        &self,
        pointer: std::sync::Arc<crate::media::desktop_pointer::DesktopPointer>,
    ) -> Option<tablet_pointer::TabletPointerReader> {
        tablet_pointer::spawn(
            self.prefix.clone(),
            self.seat.clone(),
            DISCOVERY_TIMEOUT,
            pointer,
        )
        .map_err(|error| {
            tracing::warn!(target: "arcen::usb", %error, "bridged pen position reader failed to start");
        })
        .ok()
    }
}

/// A bridge for one attachment to a native-seat Wayland desktop, on that
/// desktop's own seat, with the port it connected once its seat rule is in
/// force.
///
/// # Errors
///
/// The reason shown to the Deck.
#[cfg(feature = "native-tablet")]
pub async fn attach_native_seat(
    binary: &Path,
    session: &crate::session::lifecycle::SessionMetadata,
    tablet: &AuthorizedTablet,
) -> Result<(BridgeAttachment, BridgePort), String> {
    let generation = fresh_attachment_generation().map_err(|error| error.to_string())?;
    let seat = bridge_seat_for(Some(session)).map_err(|error| error.to_string())?;
    let mut process = BridgeProcess::spawn(binary, generation, tablet.speed, tablet.guard(), &seat)
        .await
        .map_err(|error| format!("Hard USB helper failed to start: {error}"))?;
    let prefix = match process.wait_attached(ATTACH_TIMEOUT).await {
        Ok(prefix) => prefix,
        Err(error) => {
            process.shutdown().await;
            return Err(format!("Hard USB bridge failed: {error}"));
        }
    };
    tracing::info!(
        target: "arcen::usb",
        bridge = %seat.seat_name(),
        port = %prefix,
        vendor_id = tablet.identity.vendor_id,
        product_id = tablet.identity.product_id,
        "Hard USB bridge up on this native desktop's own seat"
    );
    Ok((
        BridgeAttachment::PerAttachment(Box::new(process)),
        BridgePort { prefix, seat },
    ))
}

/// How long the helper may take to connect its port under its rule.
#[cfg(feature = "native-tablet")]
const ATTACH_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a new tablet may take to present stable, verified nodes.
#[cfg(feature = "native-tablet")]
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(15);

/// A bridge for one attachment to a dedicated Xorg desktop (ADR 0021). The
/// tablet stays on seat0, tagged for this desktop's X server, which hot-adds
/// it the moment it connects; every other X server ignores it. Before
/// anything starts, the host must prove that: the desktop's server re-admits
/// its tag, and every other X server loaded the system-wide ignore class or
/// ignores every foreign device. The receiver yields once the tablet's nodes
/// are verified (`Ok`) or refused (`Err(reason)`: the caller then ends the
/// bridge and tells the Deck).
///
/// # Errors
///
/// The reason shown to the Deck.
#[cfg(feature = "native-tablet")]
pub async fn attach_x11_desktop(
    binary: &Path,
    session: &crate::session::lifecycle::SessionMetadata,
    tablet: &AuthorizedTablet,
) -> Result<
    (
        BridgeAttachment,
        tokio::sync::mpsc::Receiver<Result<(), String>>,
        BridgePort,
    ),
    String,
> {
    use arcen_usb_bridge::HardUsbSessionDenial;
    let seat = bridge_seat_for(Some(session)).map_err(|error| error.to_string())?;
    let arcen_usb_bridge::BridgeSeat::X11Session {
        display: x_display, ..
    } = seat
    else {
        return Err("not a dedicated Xorg desktop".to_owned());
    };
    if !seat::wacom_x_driver_installed() {
        return Err(HardUsbSessionDenial::X11TabletUnsupported
            .reason()
            .to_owned());
    }
    // On seat0 the kernel's console keyboard handler (text VTs, SysRq) binds
    // every device with keyboard keys: pens, pads and touch only.
    if tablet
        .report_descriptors
        .iter()
        .any(|interface| arcen_usb_bridge::bridge_seat::declares_keyboard(&interface.descriptor))
    {
        return Err(HardUsbSessionDenial::X11KeyboardInterface
            .reason()
            .to_owned());
    }
    if let Err(gap) = seat::x11_isolation(x_display) {
        tracing::warn!(
            target: "arcen::usb",
            x_display,
            ?gap,
            "Native tablet refused: a bridged tablet would not be confined to this desktop"
        );
        return Err(match gap {
            arcen_usb_bridge::bridge_seat::X11IsolationGap::DesktopPredatesBridgeClass => {
                HardUsbSessionDenial::X11DesktopPredatesBridge
            }
            _ => HardUsbSessionDenial::X11IsolationNotInForce,
        }
        .reason()
        .to_owned());
    }
    let generation = fresh_attachment_generation().map_err(|error| error.to_string())?;
    let mut process = BridgeProcess::spawn(binary, generation, tablet.speed, tablet.guard(), &seat)
        .await
        .map_err(|error| format!("Hard USB helper failed to start: {error}"))?;
    let prefix = match process.wait_attached(ATTACH_TIMEOUT).await {
        Ok(prefix) => prefix,
        Err(error) => {
            process.shutdown().await;
            return Err(format!("Hard USB bridge failed: {error}"));
        }
    };
    tracing::info!(
        target: "arcen::usb",
        bridge = %seat.seat_name(),
        port = %prefix,
        x_display,
        vendor_id = tablet.identity.vendor_id,
        product_id = tablet.identity.product_id,
        "Hard USB bridge up for this X11 desktop only (seat0, tagged; every other X server ignores it)"
    );
    let port = BridgePort {
        prefix: prefix.clone(),
        seat: seat.clone(),
    };
    let (verdict, receiver) = tokio::sync::mpsc::channel(1);
    tokio::spawn(async move {
        #[allow(
            clippy::disallowed_methods,
            reason = "awaited to completion; bounded by DISCOVERY_TIMEOUT plus one udevadm settle"
        )]
        let found = tokio::task::spawn_blocking(move || {
            seat::discover_nodes(&prefix, &seat, DISCOVERY_TIMEOUT).map(|nodes| {
                let keyboard = nodes.iter().any(seat::reaches_keyboard_handler);
                (nodes, keyboard)
            })
        })
        .await
        .unwrap_or_else(|error| Err(error.to_string()));
        let outcome = match found {
            Ok((nodes, true)) => {
                tracing::warn!(
                    target: "arcen::usb",
                    x_display,
                    nodes = ?nodes.iter().map(|node| node.devnode.as_str()).collect::<Vec<_>>(),
                    "a bridged node has keyboard keys, which the console keyboard handler takes; refused on X11"
                );
                Err(HardUsbSessionDenial::X11KeyboardInterface
                    .reason()
                    .to_owned())
            }
            Ok((nodes, false))
                if arcen_usb_bridge::bridge_seat::x11_hotplug_capable(
                    &nodes,
                    seat::wacom_x_driver_installed(),
                ) =>
            {
                tracing::info!(
                    target: "arcen::usb",
                    x_display,
                    nodes = ?nodes.iter().map(|node| (node.devnode.as_str(), node.name.as_str(), node.kernel_driver.as_str())).collect::<Vec<_>>(),
                    "bridged tablet nodes verified: tagged for this desktop only, no uaccess, ignored by libinput"
                );
                Ok(())
            }
            Ok((nodes, false)) => {
                tracing::warn!(
                    target: "arcen::usb",
                    x_display,
                    drivers = ?nodes.iter().map(|node| node.kernel_driver.as_str()).collect::<Vec<_>>(),
                    "no pen node of this tablet is served by the kernel wacom driver; refused on X11"
                );
                Err(HardUsbSessionDenial::X11TabletUnsupported
                    .reason()
                    .to_owned())
            }
            Err(reason) => {
                tracing::warn!(
                    target: "arcen::usb",
                    x_display,
                    %reason,
                    "bridged tablet nodes failed verification; refused"
                );
                Err(HardUsbSessionDenial::X11DeviceNotVerified
                    .reason()
                    .to_owned())
            }
        };
        let _ = verdict.send(outcome).await;
    });
    Ok((
        BridgeAttachment::PerAttachment(Box::new(process)),
        receiver,
        port,
    ))
}

/// Ends every Hard USB bridge at Pier stop, waiting (bounded) for each
/// helper to disconnect its device and remove its seat rule.
pub async fn shutdown_bridges() {
    #[cfg(feature = "native-tablet")]
    {
        let killed = drain_dropped_helpers(&DROPPED_HELPERS, Duration::from_secs(10)).await;
        if killed > 0 {
            tracing::warn!(
                target: "arcen::usb",
                killed,
                "Hard USB helpers did not end at Pier stop and were killed; their seat rules are removed at the next start"
            );
        }
    }
}

#[cfg(not(feature = "native-tablet"))]
impl BridgeProcess {
    pub async fn next_frame(&mut self) -> std::io::Result<Vec<u8>> {
        std::future::pending().await
    }

    pub async fn send_frame(&mut self, _frame: &[u8]) -> std::io::Result<()> {
        Err(std::io::Error::other(NOT_BUILT))
    }

    pub async fn diagnostic(&mut self) -> String {
        NOT_BUILT.to_owned()
    }

    pub async fn shutdown(self) {}
}

#[cfg(feature = "native-tablet")]
impl BridgeProcess {
    /// Starts one helper bound to an immutable attachment generation.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if the fused helper cannot be started with piped
    /// stdin/stdout.
    pub async fn spawn(
        binary: &Path,
        generation: AttachmentGeneration,
        speed: UsbSpeed,
        guard: arcen_usb_bridge::BridgedDeviceGuard,
        seat: &arcen_usb_bridge::BridgeSeat,
    ) -> std::io::Result<Self> {
        let mut command = crate::command_for_helper(binary, "usb-bridge-helper");
        command
            .arg("--bridge")
            .arg(generation.to_string())
            .arg(speed_label(speed))
            .arg("--seat")
            .arg(seat.seat_name())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            // Never SIGKILL a helper: killed between detaching its port and
            // removing its seat rule, it would leave the rule behind. A
            // dropped handle closes the helper's input instead, which ends
            // it cleanly (see `Drop` and `drain_dropped_helpers`).
            .kill_on_drop(false);
        let mut child = command.spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| std::io::Error::other("usb bridge helper stdin unavailable"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| std::io::Error::other("usb bridge helper stdout unavailable"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| std::io::Error::other("usb bridge helper stderr unavailable"))?;
        let (frame_tx, frames) = tokio::sync::mpsc::channel(128);
        let reader_task = tokio::spawn(read_helper_frames(BufReader::new(stdout), frame_tx));
        Ok(Self {
            child: Some(child),
            stdin: Some(stdin),
            frames,
            reader_task,
            stderr: BufReader::new(stderr),
            guard,
            generation,
            early_diagnostic: String::new(),
        })
    }

    /// The generation every URB of this helper carries.
    #[must_use]
    pub const fn generation(&self) -> AttachmentGeneration {
        self.generation
    }

    /// Waits (bounded) for the helper's attached line and returns the port's
    /// sysfs path: by then the seat rule is in force and the port connected.
    ///
    /// # Errors
    ///
    /// When the helper exits, says something else, or times out.
    pub async fn wait_attached(&mut self, timeout: Duration) -> std::io::Result<String> {
        use tokio::io::AsyncBufReadExt;
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let mut line = String::new();
            let read = tokio::time::timeout_at(
                deadline,
                (&mut self.stderr).take(4 * 1024).read_line(&mut line),
            )
            .await
            .map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "the Hard USB helper did not connect its port in time",
                )
            })??;
            if read == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    format!(
                        "the Hard USB helper exited before connecting its port: {}",
                        self.early_diagnostic.trim()
                    ),
                ));
            }
            if let Some(rest) = line.trim().strip_prefix(ATTACHED_LINE) {
                let devpath = rest
                    .split_whitespace()
                    .find_map(|field| field.strip_prefix("devpath="))
                    .ok_or_else(|| std::io::Error::other("attached line without devpath"))?;
                arcen_usb_bridge::bridge_seat::validate_port_prefix(devpath)
                    .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
                return Ok(devpath.to_owned());
            }
            if self.early_diagnostic.len() < 4 * 1024 {
                self.early_diagnostic.push_str(&line);
            }
        }
    }

    /// Reads one exact protocol frame from helper stdout.
    ///
    /// # Errors
    ///
    /// Returns an I/O error on EOF, malformed length, or pipe failure.
    pub async fn next_frame(&mut self) -> std::io::Result<Vec<u8>> {
        let frame = self.frames.recv().await.unwrap_or_else(|| {
            Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "Hard USB helper frame reader ended",
            ))
        })?;
        self.observe_outbound(&frame)?;
        Ok(frame)
    }

    /// Remembers descriptor requests and cancellations travelling to the
    /// Deck, so their completions can be checked on the way back.
    fn observe_outbound(&mut self, frame: &[u8]) -> std::io::Result<()> {
        match frame.first().copied() {
            Some(byte) if byte == arcen_protocol::FrameType::UsbBridgeUrbSubmit as u8 => {
                let (header, _) = arcen_protocol::decode_usb_urb_submit(frame)
                    .map_err(|error| std::io::Error::other(format!("{error:?}")))?;
                self.guard
                    .observe_submit(header.urb_id, header.setup)
                    .map_err(|violation| {
                        std::io::Error::new(std::io::ErrorKind::InvalidData, violation)
                    })
            }
            Some(byte) if byte == arcen_protocol::FrameType::UsbBridgeUrbCancel as u8 => {
                let (_, urb_id) = arcen_protocol::decode_usb_urb_cancel(frame)
                    .map_err(|error| std::io::Error::other(format!("{error:?}")))?;
                self.guard.observe_cancel(urb_id);
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// Sends one exact protocol frame to helper stdin.
    ///
    /// # Errors
    ///
    /// Returns an I/O error for an oversized frame or pipe failure.
    pub async fn send_frame(&mut self, frame: &[u8]) -> std::io::Result<()> {
        if frame.is_empty() || frame.len() > MAX_HELPER_FRAME_BYTES {
            return Err(std::io::Error::other("Pier helper frame outside bound"));
        }
        if frame.first().copied() == Some(arcen_protocol::FrameType::UsbBridgeUrbComplete as u8) {
            let (completion, payload) = decode_usb_urb_complete(frame)
                .map_err(|error| std::io::Error::other(format!("{error:?}")))?;
            // A device that was authorized as a pen tablet and now describes
            // itself differently (another class, another interface, another
            // report descriptor) never reaches the host kernel: the frame is
            // dropped here and the caller tears the attachment down.
            self.guard
                .check_completion(
                    completion.urb_id,
                    completion.status == UrbStatus::Success,
                    payload,
                )
                .map_err(|violation| {
                    std::io::Error::new(std::io::ErrorKind::InvalidData, violation)
                })?;
        }
        let length = u32::try_from(frame.len())
            .map_err(|_| std::io::Error::other("Pier helper frame length overflow"))?;
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| std::io::Error::other("Hard USB helper input closed"))?;
        stdin.write_u32_le(length).await?;
        stdin.write_all(frame).await?;
        stdin.flush().await
    }

    /// Reads the bounded helper diagnostic after it exits.
    pub async fn diagnostic(&mut self) -> String {
        let mut diagnostic = std::mem::take(&mut self.early_diagnostic);
        let mut limited = (&mut self.stderr).take(4 * 1024);
        let _ = tokio::time::timeout(
            Duration::from_millis(200),
            limited.read_to_string(&mut diagnostic),
        )
        .await;
        diagnostic.trim().to_owned()
    }

    /// Closes the helper's input and waits for it to disconnect its device
    /// and remove its seat rule (bounded), then kills it.
    pub async fn shutdown(mut self) {
        drop(self.stdin.take());
        if let Some(mut child) = self.child.take() {
            let deadline = tokio::time::Duration::from_secs(8);
            if tokio::time::timeout(deadline, child.wait()).await.is_err() {
                let _ = child.kill().await;
                let _ = child.wait().await;
            }
        }
        self.reader_task.abort();
    }
}

/// Helpers whose handle was dropped without `shutdown` (an attachment task
/// aborted, for instance at Pier stop). Their input is closed, so each ends
/// on its own after detaching its port and removing its seat rule; they are
/// reaped here, and Pier stop waits for them.
#[cfg(feature = "native-tablet")]
static DROPPED_HELPERS: std::sync::Mutex<Vec<(Child, tokio::time::Instant)>> =
    std::sync::Mutex::new(Vec::new());

/// How long a dropped helper may take to end before it is killed.
#[cfg(feature = "native-tablet")]
const DROPPED_HELPER_GRACE: Duration = Duration::from_secs(30);

#[cfg(feature = "native-tablet")]
impl Drop for BridgeProcess {
    fn drop(&mut self) {
        drop(self.stdin.take());
        if let Some(child) = self.child.take() {
            keep_dropped_helper(&DROPPED_HELPERS, child, DROPPED_HELPER_GRACE);
        }
    }
}

/// Keeps a dropped helper until it has ended, reaping the ones that have and
/// killing any that overran `grace` (bounded, and a leftover rule is removed
/// by the next bridge on that port or the next Pier start).
#[cfg(feature = "native-tablet")]
fn keep_dropped_helper(
    list: &std::sync::Mutex<Vec<(Child, tokio::time::Instant)>>,
    child: Child,
    grace: Duration,
) {
    let Ok(mut helpers) = list.lock() else {
        return;
    };
    helpers.push((child, tokio::time::Instant::now()));
    helpers.retain_mut(|(child, since)| {
        if !matches!(child.try_wait(), Ok(None)) {
            return false;
        }
        if since.elapsed() > grace {
            let _ = child.start_kill();
        }
        true
    });
}

/// Waits until every dropped helper has ended, killing the ones still
/// running at `deadline`. Returns how many had to be killed.
#[cfg(feature = "native-tablet")]
async fn drain_dropped_helpers(
    list: &std::sync::Mutex<Vec<(Child, tokio::time::Instant)>>,
    timeout: Duration,
) -> usize {
    let helpers = list
        .lock()
        .map(|mut helpers| std::mem::take(&mut *helpers))
        .unwrap_or_default();
    let deadline = tokio::time::Instant::now() + timeout;
    let mut killed = 0;
    for (mut child, _) in helpers {
        if tokio::time::timeout_at(deadline, child.wait())
            .await
            .is_err()
        {
            killed += 1;
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
    }
    killed
}

#[cfg(feature = "native-tablet")]
async fn read_helper_frames(
    mut stdout: BufReader<ChildStdout>,
    sender: tokio::sync::mpsc::Sender<std::io::Result<Vec<u8>>>,
) {
    loop {
        let result = async {
            let length = stdout.read_u32_le().await?;
            let length = usize::try_from(length)
                .map_err(|_| std::io::Error::other("helper frame length overflow"))?;
            if length == 0 || length > MAX_HELPER_FRAME_BYTES {
                return Err(std::io::Error::other("helper frame length outside bound"));
            }
            let mut frame = vec![0_u8; length];
            stdout.read_exact(&mut frame).await?;
            Ok(frame)
        }
        .await;
        let terminal = result.is_err();
        if sender.send(result).await.is_err() || terminal {
            break;
        }
    }
}

/// `SIGTERM`, `SIGINT` and `SIGHUP` end the helper cleanly: the serve loop
/// sees them, then the port is detached, the device leaves, and the seat rule
/// is removed, as on any other exit. The signals are blocked before any
/// thread starts (threads inherit the mask) and read from a non-blocking
/// signalfd, so no handler and no unsafe code is involved.
#[cfg(feature = "native-tablet")]
struct TerminationWatch {
    signals: nix::sys::signalfd::SignalFd,
    requested: bool,
}

#[cfg(feature = "native-tablet")]
impl TerminationWatch {
    fn install() -> Result<Self, String> {
        use nix::sys::signal::{SigSet, Signal};
        use nix::sys::signalfd::{SfdFlags, SignalFd};
        let mut mask = SigSet::empty();
        for signal in [Signal::SIGTERM, Signal::SIGINT, Signal::SIGHUP] {
            mask.add(signal);
        }
        mask.thread_block()
            .map_err(|error| format!("block termination signals: {error}"))?;
        let signals = SignalFd::with_flags(&mask, SfdFlags::SFD_NONBLOCK | SfdFlags::SFD_CLOEXEC)
            .map_err(|error| format!("signalfd: {error}"))?;
        Ok(Self {
            signals,
            requested: false,
        })
    }

    /// Whether a termination signal has arrived (sticky).
    fn requested(&mut self) -> bool {
        if !self.requested {
            if let Ok(Some(info)) = self.signals.read_signal() {
                eprintln!(
                    "usb-bridge-helper: signal {} received; detaching",
                    info.ssi_signo
                );
                self.requested = true;
            }
        }
        self.requested
    }
}

/// Reserves a port, puts its subtree on `seat` before anything can appear
/// there, attaches, runs `serve`, then detaches and removes the seat rule
/// once the device is gone.
#[cfg(feature = "native-tablet")]
fn with_attached_port<T>(
    speed: UsbSpeed,
    seat: &arcen_usb_bridge::BridgeSeat,
    termination: &mut TerminationWatch,
    serve: impl FnOnce(&mut dyn VirtualHcd, &mut TerminationWatch) -> Result<T, String>,
) -> Result<T, String> {
    if !nix::unistd::geteuid().is_root() {
        return Err("must run as root".to_owned());
    }
    let kind = backend::select_backend()?;
    let mut hcd = backend::open(kind, speed)?;
    // The port's devices belong to `seat` before the port is connected, so
    // the device never appears on seat0, not even for one udev event.
    let prefix = hcd.devpath_prefix().to_owned();
    let rule = seat::SeatRule::install(&prefix, seat)
        .map_err(|error| format!("bridge seat rule: {error}"))?;
    hcd.attach()?;
    eprintln!(
        "{ATTACHED_LINE} devpath={prefix} seat={} backend={}",
        seat.seat_name(),
        kind.label()
    );
    let result = serve(hcd.as_mut(), termination);
    // Every exit, clean or not, takes the device away before its seat rule:
    // the port is detached and the controller closed first.
    let detached = hcd.detach();
    drop(hcd);
    if !seat::wait_subtree_gone(&prefix, Duration::from_secs(5)) {
        eprintln!("usb-bridge-helper: the port's devices outlived the disconnect");
    }
    drop(rule);
    let value = result?;
    detached?;
    Ok(value)
}

#[cfg(feature = "native-tablet")]
fn run_bridge(
    generation: AttachmentGeneration,
    speed: UsbSpeed,
    seat: &arcen_usb_bridge::BridgeSeat,
) -> Result<(), String> {
    // Before the stdin thread exists, so it inherits the blocked signals.
    let mut termination = TerminationWatch::install()?;
    let (completion_tx, completion_rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(128);
    std::thread::Builder::new()
        .name("arcen-usb-bridge-stdin".to_owned())
        .spawn(move || {
            let mut stdin = std::io::stdin().lock();
            while let Ok(frame) = read_sync_frame(&mut stdin) {
                if completion_tx.send(frame).is_err() {
                    break;
                }
            }
        })
        .map_err(|error| format!("spawn helper stdin reader: {error}"))?;
    with_attached_port(speed, seat, &mut termination, |hcd, termination| {
        serve_bridge(hcd, generation, &completion_rx, termination)
    })
}

/// One URB forwarded to the Deck and not yet answered.
#[cfg(feature = "native-tablet")]
#[derive(Debug, Clone, Copy)]
struct ForwardedUrb {
    handle: u64,
}

#[cfg(feature = "native-tablet")]
fn serve_bridge(
    hcd: &mut dyn VirtualHcd,
    generation: AttachmentGeneration,
    completion_rx: &std::sync::mpsc::Receiver<Vec<u8>>,
    termination: &mut TerminationWatch,
) -> Result<(), String> {
    let mut stdout = std::io::stdout().lock();
    let mut next_urb_id = NonZeroU32::MIN;
    let mut forwarded = BTreeMap::<UrbId, ForwardedUrb>::new();
    let mut cancelled = BTreeMap::<UrbId, u8>::new();
    let mut input_closed = false;
    loop {
        if termination.requested() {
            break;
        }
        loop {
            match completion_rx.try_recv() {
                Ok(frame) => {
                    let (completion, payload) =
                        decode_usb_urb_complete(&frame).map_err(|error| format!("{error:?}"))?;
                    if completion.generation != generation {
                        return Err(
                            "completion generation differs from helper generation".to_owned()
                        );
                    }
                    let Some(urb) = forwarded.remove(&completion.urb_id) else {
                        if let Some(remaining) = cancelled.get_mut(&completion.urb_id) {
                            *remaining = remaining.saturating_sub(1);
                            if *remaining == 0 {
                                cancelled.remove(&completion.urb_id);
                            }
                            continue;
                        }
                        return Err("completion references unknown URB".to_owned());
                    };
                    let out_actual =
                        usize::try_from(completion.actual_length).unwrap_or(usize::MAX);
                    hcd.giveback(urb.handle, completion.status, out_actual, payload)?;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    input_closed = true;
                    break;
                }
            }
        }
        // The Pier is gone or done with this attachment: no completion can
        // arrive any more, so detach now rather than serve the kernel on.
        if input_closed {
            break;
        }

        match hcd.next_event(Duration::from_millis(20))? {
            Some(HcdEvent::Cancel { handle }) => {
                if let Some((&urb_id, _)) = forwarded
                    .iter()
                    .find(|(_, candidate)| candidate.handle == handle)
                {
                    forwarded.remove(&urb_id);
                    if cancelled.len() >= arcen_usb_bridge::MAX_IN_FLIGHT_URBS {
                        return Err("helper cancellation tombstone limit exceeded".to_owned());
                    }
                    // The submit response and the explicit cancel response can
                    // race. Both are terminal from Deck's perspective; ignore
                    // at most those two stale completions after owning the
                    // kernel's answer here.
                    cancelled.insert(urb_id, 2);
                    let frame = encode_usb_urb_cancel(generation, urb_id);
                    write_sync_frame(&mut stdout, &frame)?;
                }
                hcd.acknowledge_cancel(handle)?;
            }
            Some(HcdEvent::Submit(urb)) => {
                if urb.kind == TransferKind::Interrupt {
                    std::thread::sleep(urb.cadence);
                }
                let urb_id = UrbId::new(next_urb_id);
                next_urb_id =
                    NonZeroU32::new(next_urb_id.get().wrapping_add(1)).unwrap_or(NonZeroU32::MIN);
                if forwarded
                    .insert(urb_id, ForwardedUrb { handle: urb.handle })
                    .is_some()
                {
                    return Err("helper generated duplicate URB id".to_owned());
                }
                if forwarded.len() > arcen_usb_bridge::MAX_IN_FLIGHT_URBS {
                    return Err("helper in-flight URB limit exceeded".to_owned());
                }
                let frame = encode_usb_urb_submit(
                    UsbUrbSubmitHeader {
                        generation,
                        urb_id,
                        endpoint: arcen_usb_bridge::EndpointAddress(urb.endpoint),
                        transfer_kind: urb.kind,
                        timeout_ms: 1_000,
                        declared_length: u32::try_from(urb.length)
                            .map_err(|_| "URB length exceeds u32".to_owned())?,
                        setup: urb.setup,
                    },
                    &urb.out,
                )
                .map_err(|error| format!("{error:?}"))?;
                write_sync_frame(&mut stdout, &frame)?;
            }
            None => {}
        }
    }
    Ok(())
}

#[cfg(feature = "native-tablet")]
fn read_sync_frame(reader: &mut impl Read) -> Result<Vec<u8>, String> {
    let mut length = [0_u8; 4];
    reader
        .read_exact(&mut length)
        .map_err(|error| error.to_string())?;
    let length = usize::try_from(u32::from_le_bytes(length))
        .map_err(|_| "helper frame length overflow".to_owned())?;
    if length == 0 || length > MAX_HELPER_FRAME_BYTES {
        return Err("helper frame length outside bound".to_owned());
    }
    let mut frame = vec![0_u8; length];
    reader
        .read_exact(&mut frame)
        .map_err(|error| error.to_string())?;
    Ok(frame)
}

#[cfg(feature = "native-tablet")]
fn write_sync_frame(writer: &mut impl Write, frame: &[u8]) -> Result<(), String> {
    if frame.is_empty() || frame.len() > MAX_HELPER_FRAME_BYTES {
        return Err("helper output frame outside bound".to_owned());
    }
    let length =
        u32::try_from(frame.len()).map_err(|_| "helper output length overflow".to_owned())?;
    writer
        .write_all(&length.to_le_bytes())
        .and_then(|()| writer.write_all(frame))
        .and_then(|()| writer.flush())
        .map_err(|error| error.to_string())
}

#[cfg(feature = "native-tablet")]
const fn speed_label(speed: UsbSpeed) -> &'static str {
    match speed {
        UsbSpeed::Low => "low",
        UsbSpeed::Full => "full",
        UsbSpeed::High => "high",
    }
}

#[cfg(feature = "native-tablet")]
fn parse_speed(value: &str) -> Result<UsbSpeed, String> {
    match value {
        "low" => Ok(UsbSpeed::Low),
        "full" => Ok(UsbSpeed::Full),
        "high" => Ok(UsbSpeed::High),
        other => Err(format!("unsupported USB bridge speed {other:?}")),
    }
}

#[cfg(feature = "native-tablet")]
fn run_self_test() -> Result<(), String> {
    // Even the self-test's device never reaches seat0.
    let seat = self_test_seat().map_err(|error| error.to_string())?;
    let mut termination = TerminationWatch::install()?;
    with_attached_port(
        UsbSpeed::High,
        &seat,
        &mut termination,
        |hcd, termination| {
            let mut tablet = SyntheticTabletDevice::default();
            let deadline = Instant::now() + SELF_TEST_RUNTIME;
            let mut configured_at = None;
            let mut completed = 0_u64;
            while Instant::now() < deadline && !termination.requested() {
                match hcd.next_event(Duration::from_millis(200))? {
                    Some(HcdEvent::Cancel { handle }) => hcd.acknowledge_cancel(handle)?,
                    Some(HcdEvent::Submit(urb)) => {
                        complete_urb(hcd, &mut tablet, &urb)?;
                        completed = completed.saturating_add(1);
                        if tablet.configuration() == 1 && configured_at.is_none() {
                            configured_at = Some(Instant::now());
                            println!(
                            "USB_BRIDGE_READY devpath={} product=\"Arcen USB Bridge Lab Tablet\"",
                            hcd.devpath_prefix()
                        );
                        }
                    }
                    None => {}
                }
                if configured_at
                    .is_some_and(|configured| configured.elapsed() >= Duration::from_secs(2))
                {
                    break;
                }
            }
            if configured_at.is_none() {
                return Err(format!(
                    "kernel did not configure the synthetic tablet; completed_urbs={completed}"
                ));
            }
            println!(
                "USB_BRIDGE_SELF_TEST_OK backend={} devpath={} completed_urbs={completed}",
                hcd.kind().label(),
                hcd.devpath_prefix()
            );
            Ok(())
        },
    )
}

#[cfg(feature = "native-tablet")]
fn complete_urb(
    hcd: &mut dyn VirtualHcd,
    tablet: &mut SyntheticTabletDevice,
    urb: &HcdUrb,
) -> Result<(), String> {
    match (urb.kind, urb.setup) {
        (TransferKind::Control, Some(setup)) => match tablet.handle_control(setup) {
            ControlResponse::Ack => {
                hcd.giveback(urb.handle, UrbStatus::Success, urb.out.len(), &[])
            }
            ControlResponse::Data(data) => {
                let length = data.len().min(urb.length);
                hcd.giveback(urb.handle, UrbStatus::Success, 0, &data[..length])
            }
            ControlResponse::Stall => hcd.giveback(urb.handle, UrbStatus::Stall, 0, &[]),
        },
        (TransferKind::Interrupt, _) if urb.direction() == TransferDirection::In => {
            std::thread::sleep(urb.cadence);
            let report = PenSample {
                x: 0.5,
                y: 0.5,
                pressure: 0.0,
                tilt_x_degrees: 0.0,
                tilt_y_degrees: 0.0,
                switches: PenSwitches::default().with(PenSwitch::InRange, true),
            }
            .encode_report();
            let length = report.len().min(urb.length);
            hcd.giveback(urb.handle, UrbStatus::Success, 0, &report[..length])
        }
        _ => hcd.giveback(urb.handle, UrbStatus::Stall, 0, &[]),
    }
}

#[cfg(all(test, feature = "native-tablet"))]
mod tests {
    use super::*;
    use arcen_usb_bridge::SetupPacket;

    fn device(vendor_id: u16, device_class: u8, descriptors: &[&[u8]]) -> UsbHardDeviceMsg {
        UsbHardDeviceMsg {
            vendor_id,
            product_id: 0x0317,
            bcd_device: 0x0100,
            device_class,
            speed: UsbSpeed::Full,
            hid_report_descriptors: descriptors
                .iter()
                .zip(0_u8..)
                .map(|(descriptor, interface)| {
                    arcen_protocol::messages::UsbHidReportDescriptorMsg::new(interface, descriptor)
                })
                .collect(),
        }
    }

    const KEYBOARD: &[u8] = &[
        0x05, 0x01, 0x09, 0x06, 0xA1, 0x01, 0x05, 0x07, 0x19, 0xE0, 0x29, 0xE7, 0x15, 0x00, 0x25,
        0x01, 0x75, 0x01, 0x95, 0x08, 0x81, 0x02, 0xC0,
    ];

    #[test]
    fn any_vendors_classified_pen_tablet_is_authorized_at_its_own_speed() {
        for vendor_id in [0x056a_u16, 0x256c, 0x28bd, 0x1234] {
            let authorized = authorize_device(Some(&device(
                vendor_id,
                0,
                &[
                    arcen_usb_bridge::SYNTHETIC_TABLET_REPORT_DESCRIPTOR,
                    arcen_usb_bridge::descriptor_fixtures::INTUOS5_STYLE_MOUSE_INTERFACE,
                ],
            )))
            .expect("pen tablet is authorized");
            assert_eq!(authorized.speed, UsbSpeed::Full);
            assert_eq!(authorized.report_descriptors.len(), 2);
            assert_eq!(
                authorized.profile.capabilities.pressure_levels(),
                Some(1024)
            );
        }
    }

    #[test]
    fn an_intuos5_style_tablet_is_authorized_and_its_vendor_interface_alone_is_not() {
        use arcen_usb_bridge::descriptor_fixtures::{
            BOOT_KEYBOARD, INTUOS5_STYLE_MOUSE_INTERFACE, INTUOS5_STYLE_PEN_INTERFACE,
            INTUOS5_STYLE_VENDOR_CONTROL_INTERFACE,
        };
        let authorized = authorize_device(Some(&device(
            0x056a,
            0,
            &[
                INTUOS5_STYLE_PEN_INTERFACE,
                INTUOS5_STYLE_VENDOR_CONTROL_INTERFACE,
                INTUOS5_STYLE_MOUSE_INTERFACE,
            ],
        )))
        .expect("Intuos5-style tablet is authorized");
        assert_eq!(authorized.report_descriptors.len(), 3);
        assert_eq!(authorized.profile.capabilities.pressure_levels(), None);
        assert!(authorize_device(Some(&device(
            0x056a,
            0,
            &[INTUOS5_STYLE_VENDOR_CONTROL_INTERFACE]
        )))
        .is_err());
        assert!(authorize_device(Some(&device(
            0x056a,
            0,
            &[INTUOS5_STYLE_PEN_INTERFACE, BOOT_KEYBOARD]
        )))
        .is_ok());
        assert!(authorize_device(Some(&device(0x056a, 0, &[BOOT_KEYBOARD]))).is_err());
    }

    /// The reviewed attack: a Deck claims a pen tablet, then the attached
    /// device enumerates as mass storage. The guard built from the
    /// authorization refuses the configuration before it reaches usb-vhci.
    #[test]
    fn the_authorized_guard_refuses_a_device_that_enumerates_as_mass_storage() {
        let authorized = authorize_device(Some(&device(
            0x1234,
            0,
            &[arcen_usb_bridge::SYNTHETIC_TABLET_REPORT_DESCRIPTOR],
        )))
        .expect("pen tablet is authorized");
        let mut guard = authorized.guard();
        let urb = UrbId::new(NonZeroU32::MIN);
        guard
            .observe_submit(
                urb,
                Some(SetupPacket {
                    request_type: 0x80,
                    request: 0x06,
                    value: 0x0200,
                    index: 0,
                    length: 255,
                }),
            )
            .unwrap();
        let storage = [
            9, 2, 25, 0, 1, 1, 0, 0x80, 50, // configuration
            9, 4, 0, 0, 1, 0x08, 0x06, 0x50, 0, // mass storage interface
            7, 5, 0x81, 0x03, 64, 0, 1, // interrupt IN
        ];
        assert!(guard.check_completion(urb, true, &storage).is_err());
    }

    /// The reviewed bypass through the Linux adapter: the configuration
    /// advertises only a keyboard prefix of the authorized keyboard+pen
    /// report descriptor, so the kernel would never read the pen.
    #[test]
    fn the_authorized_guard_binds_the_advertised_report_length() {
        let mut composite = KEYBOARD.to_vec();
        let keyboard_length = u8::try_from(composite.len()).unwrap();
        composite.extend_from_slice(arcen_usb_bridge::SYNTHETIC_TABLET_REPORT_DESCRIPTOR);
        let authorized = authorize_device(Some(&device(0x1234, 0, &[&composite])))
            .expect("keyboard+pen composite is a pen tablet");
        let mut guard = authorized.guard();
        let urb = UrbId::new(NonZeroU32::MIN);
        guard
            .observe_submit(
                urb,
                Some(SetupPacket {
                    request_type: 0x80,
                    request: 0x06,
                    value: 0x0200,
                    index: 0,
                    length: 255,
                }),
            )
            .unwrap();
        let short = [
            9,
            2,
            34,
            0,
            1,
            1,
            0,
            0x80,
            50, // configuration
            9,
            4,
            0,
            0,
            1,
            3,
            0,
            0,
            0, // HID interface
            9,
            0x21,
            0x11,
            0x01,
            0,
            1,
            0x22,
            keyboard_length,
            0, // keyboard prefix only
            7,
            5,
            0x81,
            3,
            16,
            0,
            1, // interrupt IN
        ];
        assert!(guard.check_completion(urb, true, &short).is_err());
    }

    /// Express keys presented as a keyboard interface are bridged with the
    /// pen; a game controller is not a tablet function and is refused.
    #[test]
    fn express_keys_are_bridged_and_game_controllers_are_not() {
        assert!(authorize_device(Some(&device(
            0x1234,
            0,
            &[
                arcen_usb_bridge::SYNTHETIC_TABLET_REPORT_DESCRIPTOR,
                KEYBOARD
            ],
        )))
        .is_ok());
        let refused = authorize_device(Some(&device(
            0x1234,
            0,
            &[
                arcen_usb_bridge::SYNTHETIC_TABLET_REPORT_DESCRIPTOR,
                arcen_usb_bridge::descriptor_fixtures::GAMEPAD,
            ],
        )));
        assert!(refused.is_err_and(|reason| reason.contains("never bridged")));
    }

    #[test]
    fn non_tablets_and_legacy_claims_are_denied() {
        assert!(authorize_device(None).is_err());
        // A 0.15 Deck sends identity only: refused, not admitted by vendor id.
        assert!(authorize_device(Some(&device(0x056a, 0, &[]))).is_err());
        assert!(authorize_device(Some(&device(0x056a, 0, &[KEYBOARD]))).is_err());
        assert!(authorize_device(Some(&device(
            0x1234,
            0x08,
            &[arcen_usb_bridge::SYNTHETIC_TABLET_REPORT_DESCRIPTOR]
        )))
        .is_err());
        let mut malformed = device(0x1234, 0, &[]);
        malformed.hid_report_descriptors.push(
            arcen_protocol::messages::UsbHidReportDescriptorMsg {
                interface: 0,
                descriptor: "zz".to_owned(),
            },
        );
        assert!(authorize_device(Some(&malformed)).is_err());
    }

    #[test]
    fn the_synthetic_lab_tablet_needs_its_opt_in() {
        let mut synthetic = device(
            arcen_usb_bridge::ARCEN_LAB_VENDOR_ID,
            0,
            &[arcen_usb_bridge::SYNTHETIC_TABLET_REPORT_DESCRIPTOR],
        );
        synthetic.product_id = arcen_usb_bridge::ARCEN_LAB_PRODUCT_ID;
        if std::env::var("ARCEN_USB_HARD_SYNTHETIC").ok().as_deref() != Some("1") {
            assert!(authorize_device(Some(&synthetic)).is_err());
        }
    }

    #[test]
    fn speed_labels_round_trip() {
        for speed in [UsbSpeed::Low, UsbSpeed::Full, UsbSpeed::High] {
            assert_eq!(parse_speed(speed_label(speed)).unwrap(), speed);
        }
    }

    /// A stand-in helper: `script` runs with piped stdin, like the real one.
    fn helper_child(script: &str) -> (Child, ChildStdin) {
        let mut child = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(script)
            .stdin(std::process::Stdio::piped())
            .kill_on_drop(false)
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        (child, stdin)
    }

    #[tokio::test]
    async fn a_dropped_helper_ends_on_its_own_and_pier_stop_waits_for_it() {
        let list = std::sync::Mutex::new(Vec::new());
        // Like the real helper: serves until its input closes, then needs a
        // moment for detach and rule removal.
        let (child, stdin) = helper_child("cat >/dev/null; sleep 0.3");
        drop(stdin);
        keep_dropped_helper(&list, child, Duration::from_secs(30));
        let started = tokio::time::Instant::now();
        assert_eq!(
            drain_dropped_helpers(&list, Duration::from_secs(5)).await,
            0
        );
        assert!(started.elapsed() >= Duration::from_millis(250));
        assert!(list.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_helper_that_does_not_end_is_killed_at_the_stop_deadline() {
        let list = std::sync::Mutex::new(Vec::new());
        let (child, stdin) = helper_child("sleep 30");
        drop(stdin);
        keep_dropped_helper(&list, child, Duration::from_secs(30));
        let started = tokio::time::Instant::now();
        assert_eq!(
            drain_dropped_helpers(&list, Duration::from_millis(200)).await,
            1
        );
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn ended_helpers_are_reaped_and_overdue_ones_killed_when_another_is_dropped() {
        let list = std::sync::Mutex::new(Vec::new());
        let (ended, stdin) = helper_child("cat >/dev/null");
        drop(stdin);
        keep_dropped_helper(&list, ended, Duration::from_secs(30));
        tokio::time::sleep(Duration::from_millis(300)).await;
        let (stuck, _stuck_stdin) = helper_child("sleep 30");
        keep_dropped_helper(&list, stuck, Duration::ZERO);
        // The ended one is reaped; the overdue one is killed, and kept until
        // it is reaped.
        assert_eq!(list.lock().unwrap().len(), 1);
        tokio::time::sleep(Duration::from_millis(300)).await;
        let (next, stdin) = helper_child("cat >/dev/null");
        drop(stdin);
        tokio::time::sleep(Duration::from_millis(300)).await;
        keep_dropped_helper(&list, next, Duration::from_secs(30));
        assert!(list.lock().unwrap().is_empty());
    }
}

#[cfg(test)]
mod native_tablet_status_tests {
    use super::*;

    fn facts(loaded: bool, installed: bool) -> arcen_usb_bridge::VhciHcdFacts {
        arcen_usb_bridge::VhciHcdFacts {
            running_kernel: "5.14.0-611.55.1.el9_7.x86_64".into(),
            loaded,
            module_for_running_kernel: installed,
            module_for_other_kernels: Vec::new(),
            rhel_family: true,
        }
    }

    #[test]
    fn the_status_says_loaded_or_how_to_load_or_what_to_install() {
        assert!(native_tablet_line(&facts(true, true)).contains("is loaded for kernel"));
        let unloaded = native_tablet_line(&facts(false, true));
        assert!(unloaded.contains("installed"), "{unloaded}");
        assert!(unloaded.contains("restart the Pier"), "{unloaded}");
        assert!(unloaded.contains("modprobe vhci-hcd"), "{unloaded}");
        let missing = native_tablet_line(&facts(false, false));
        assert!(
            missing.starts_with("Native tablet: unavailable: "),
            "{missing}"
        );
        assert!(missing.contains("kernel-modules-internal"), "{missing}");
    }
}

#[cfg(test)]
mod session_admission_tests {
    use super::*;
    use crate::session::lifecycle::SessionMetadata;
    use arcen_protocol::messages::TabletModeReason;

    fn session(session_type: &str) -> SessionMetadata {
        SessionMetadata {
            username: "artist".into(),
            uid: 1001,
            session_id: "c9".into(),
            session_type: session_type.into(),
            desktop: "gnome".into(),
            display: String::new(),
            agent_pid: 4242,
            generation: 1,
            reconnected: false,
            timezone: None,
            multi_monitor_plan: None,
            multi_monitor_carrier: None,
            audio: None,
            wayland: None,
            requested_outputs: None,
            x11_desktop_depth: None,
            native_seat: None,
        }
    }

    #[test]
    fn a_wayland_session_is_seatless_and_refused_hard_usb() {
        let wayland = session("wayland");
        assert_eq!(
            session_seat(Some(&wayland)),
            arcen_usb_bridge::HardUsbSessionSeat::Seatless
        );
        let denial = session_admission(Some(&wayland)).unwrap_err();
        // The reason reaches the Deck in the tablet-mode result.
        assert!(TabletModeReason::try_from(denial.reason().to_owned()).is_ok());
    }

    #[test]
    fn an_xorg_desktops_bridge_is_tagged_for_its_own_display() {
        assert_eq!(x11_display_number(":58"), Some(58));
        for bad in [":0", ":100", ":058", "58", ":58.0", ""] {
            assert_eq!(x11_display_number(bad), None, "{bad}");
        }
        #[cfg(feature = "native-tablet")]
        {
            let mut desktop = session("x11");
            desktop.display = ":58".to_owned();
            let seat = bridge_seat_for(Some(&desktop)).expect("x11 bridge");
            assert!(matches!(
                seat,
                arcen_usb_bridge::BridgeSeat::X11Session { display: 58, .. }
            ));
            desktop.display = ":0".to_owned();
            assert!(bridge_seat_for(Some(&desktop)).is_err());
            assert!(bridge_seat_for(None).is_err());
        }
    }

    #[test]
    fn a_native_seat_wayland_desktop_is_admitted_on_its_own_seat() {
        let mut native = session("wayland");
        native.native_seat = Some("seat-arcen-pen-0123456789abcdef".to_owned());
        assert_eq!(
            session_seat(Some(&native)),
            arcen_usb_bridge::HardUsbSessionSeat::ArcenSeat
        );
        assert_eq!(session_admission(Some(&native)), Ok(()));
        #[cfg(feature = "native-tablet")]
        {
            assert_eq!(
                bridge_seat_for(Some(&native)).expect("native seat"),
                arcen_usb_bridge::BridgeSeat::NativeSeat {
                    seat: "seat-arcen-pen-0123456789abcdef".to_owned()
                }
            );
            native.native_seat = Some("seat0".to_owned());
            assert!(bridge_seat_for(Some(&native)).is_err());
        }
    }

    #[test]
    fn an_xorg_desktop_is_admitted_by_its_tag_and_no_session_is_refused() {
        assert_eq!(
            session_seat(Some(&session("x11"))),
            arcen_usb_bridge::HardUsbSessionSeat::X11Desktop
        );
        assert_eq!(session_admission(Some(&session("x11"))), Ok(()));
        // Without a desktop there is only the console seat: a bridged
        // device would reach the login screen.
        let denial = session_admission(None).unwrap_err();
        assert_eq!(
            denial,
            arcen_usb_bridge::HardUsbSessionDenial::ConsoleSeatSession
        );
        assert!(TabletModeReason::try_from(denial.reason().to_owned()).is_ok());
    }
}
