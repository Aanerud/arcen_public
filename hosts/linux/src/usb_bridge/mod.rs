//! Linux Hard USB bridge adapter.
//!
//! The privileged helper owns the virtual-HCD file descriptor. Network,
//! session policy, and authentication stay in Pier; raw ioctl structs never
//! cross this module boundary.

#[cfg(feature = "usb-hard-lab")]
mod vhci;

#[cfg(feature = "usb-hard-lab")]
use arcen_protocol::messages::UsbHardDeviceMsg;
#[cfg(feature = "usb-hard-lab")]
use arcen_protocol::{
    decode_usb_urb_complete, encode_usb_urb_cancel, encode_usb_urb_submit, UsbUrbSubmitHeader,
};
#[cfg(feature = "usb-hard-lab")]
use arcen_usb_bridge::{
    AttachmentGeneration, ControlResponse, PenSample, PenSwitch, PenSwitches, SetupPacket,
    SyntheticTabletDevice, TransferDirection, UrbId, UrbStatus, UsbSpeed,
};
#[cfg(feature = "usb-hard-lab")]
use std::collections::BTreeMap;
#[cfg(feature = "usb-hard-lab")]
use std::io::{Read, Write};
#[cfg(feature = "usb-hard-lab")]
use std::num::{NonZeroU32, NonZeroU64};
#[cfg(feature = "usb-hard-lab")]
use std::path::Path;
#[cfg(feature = "usb-hard-lab")]
use std::process::ExitCode;
#[cfg(not(feature = "usb-hard-lab"))]
use std::process::ExitCode;
#[cfg(feature = "usb-hard-lab")]
use std::time::{Duration, Instant};
#[cfg(feature = "usb-hard-lab")]
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
#[cfg(feature = "usb-hard-lab")]
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout};
#[cfg(feature = "usb-hard-lab")]
use vhci::{FetchedWork, PortState, VhciController, VhciUrb};

#[cfg(feature = "usb-hard-lab")]
const SELF_TEST_RUNTIME: Duration = Duration::from_secs(8);
#[cfg(feature = "usb-hard-lab")]
const MAX_HELPER_FRAME_BYTES: usize = arcen_usb_bridge::MAX_TRANSFER_BYTES + 64;
#[cfg(feature = "usb-hard-lab")]
const WACOM_VENDOR_ID: u16 = 0x056a;
#[cfg(feature = "usb-hard-lab")]
const WACOM_INTUOS5_TOUCH_L_PRODUCT_ID: u16 = 0x0317;
#[cfg(feature = "usb-hard-lab")]
const WACOM_INTUOS5_TOUCH_L_BCD_DEVICE: u16 = 0x0100;

/// Returns the speed for an exact host-authorized Hard USB lab device.
#[cfg(feature = "usb-hard-lab")]
#[must_use]
pub fn authorized_device_speed(device: Option<&UsbHardDeviceMsg>) -> Option<UsbSpeed> {
    let device = device?;
    if device.vendor_id == WACOM_VENDOR_ID
        && device.product_id == WACOM_INTUOS5_TOUCH_L_PRODUCT_ID
        && device.bcd_device == WACOM_INTUOS5_TOUCH_L_BCD_DEVICE
        && device.device_class == 0
        && device.speed == UsbSpeed::Full
    {
        return Some(device.speed);
    }
    let synthetic_enabled = std::env::var("ARCEN_USB_HARD_SYNTHETIC").ok().as_deref() == Some("1");
    (synthetic_enabled
        && device.vendor_id == arcen_usb_bridge::ARCEN_LAB_VENDOR_ID
        && device.product_id == arcen_usb_bridge::ARCEN_LAB_PRODUCT_ID
        && device.bcd_device == 0x0100
        && device.device_class == 0
        && device.speed == UsbSpeed::High)
        .then_some(device.speed)
}

#[cfg(not(feature = "usb-hard-lab"))]
#[must_use]
pub fn authorized_device_speed(
    _device: Option<&arcen_protocol::messages::UsbHardDeviceMsg>,
) -> Option<arcen_usb_bridge::UsbSpeed> {
    None
}

/// Whether this binary contains the lab importer and its device is available.
#[must_use]
pub fn runtime_available() -> bool {
    #[cfg(feature = "usb-hard-lab")]
    {
        use std::os::unix::fs::FileTypeExt;
        std::env::var("ARCEN_USB_HARD_LAB").ok().as_deref() == Some("1")
            && std::fs::metadata("/dev/usb-vhci")
                .is_ok_and(|metadata| metadata.file_type().is_char_device())
    }

    #[cfg(not(feature = "usb-hard-lab"))]
    {
        false
    }
}

/// Generates one nonzero attachment generation from OS randomness.
#[cfg(feature = "usb-hard-lab")]
pub fn fresh_attachment_generation() -> std::io::Result<AttachmentGeneration> {
    let mut bytes = [0_u8; 8];
    getrandom::getrandom(&mut bytes).map_err(std::io::Error::other)?;
    let value = NonZeroU64::new(u64::from_le_bytes(bytes)).unwrap_or(NonZeroU64::MIN);
    Ok(AttachmentGeneration::new(value))
}

/// Entry point for the fused privileged helper.
#[must_use]
pub fn helper_main(args: &[String]) -> ExitCode {
    #[cfg(not(feature = "usb-hard-lab"))]
    {
        let _ = args;
        eprintln!("usb-bridge-helper: this build does not contain usb-hard-lab");
        return ExitCode::FAILURE;
    }
    #[cfg(feature = "usb-hard-lab")]
    {
        helper_main_feature(args)
    }
}

/// Runs the real fused helper through its bounded protocol-frame IPC.
#[must_use]
pub fn ipc_self_test_main() -> ExitCode {
    #[cfg(not(feature = "usb-hard-lab"))]
    {
        eprintln!("usb-bridge-ipc-self-test: this build does not contain usb-hard-lab");
        ExitCode::FAILURE
    }
    #[cfg(feature = "usb-hard-lab")]
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

#[cfg(feature = "usb-hard-lab")]
async fn run_ipc_self_test() -> Result<u64, String> {
    let binary = crate::current_pier_exe()
        .ok_or_else(|| "current fused Pier binary is unavailable".to_owned())?;
    let generation = AttachmentGeneration::new(NonZeroU64::MIN);
    let mut process = BridgeProcess::spawn(&binary, generation, UsbSpeed::High)
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

#[cfg(feature = "usb-hard-lab")]
fn helper_main_feature(args: &[String]) -> ExitCode {
    let result = match args {
        [argument] if argument == "--self-test" => run_self_test(),
        [argument, generation, speed] if argument == "--bridge" => generation
            .parse::<u64>()
            .ok()
            .and_then(NonZeroU64::new)
            .ok_or_else(|| "bridge generation must be nonzero".to_owned())
            .and_then(|generation| {
                parse_speed(speed)
                    .and_then(|speed| run_bridge(AttachmentGeneration::new(generation), speed))
            }),
        _ => Err(
            "usage: arcen-pier usb-bridge-helper --self-test | --bridge GENERATION low|full|high"
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
#[cfg(feature = "usb-hard-lab")]
pub struct BridgeProcess {
    child: Child,
    stdin: ChildStdin,
    frames: tokio::sync::mpsc::Receiver<std::io::Result<Vec<u8>>>,
    reader_task: tokio::task::JoinHandle<()>,
    stderr: BufReader<ChildStderr>,
}

#[cfg(not(feature = "usb-hard-lab"))]
pub struct BridgeProcess;

#[cfg(not(feature = "usb-hard-lab"))]
impl BridgeProcess {
    pub async fn next_frame(&mut self) -> std::io::Result<Vec<u8>> {
        std::future::pending().await
    }

    pub async fn send_frame(&mut self, _frame: &[u8]) -> std::io::Result<()> {
        Err(std::io::Error::other(
            "this Pier build does not contain usb-hard-lab",
        ))
    }

    pub async fn diagnostic(&mut self) -> String {
        "this Pier build does not contain usb-hard-lab".to_owned()
    }

    pub async fn shutdown(self) {}
}

#[cfg(feature = "usb-hard-lab")]
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
    ) -> std::io::Result<Self> {
        let mut command = crate::command_for_helper(binary, "usb-bridge-helper");
        command
            .arg("--bridge")
            .arg(generation.to_string())
            .arg(speed_label(speed))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
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
            child,
            stdin,
            frames,
            reader_task,
            stderr: BufReader::new(stderr),
        })
    }

    /// Reads one exact protocol frame from helper stdout.
    ///
    /// # Errors
    ///
    /// Returns an I/O error on EOF, malformed length, or pipe failure.
    pub async fn next_frame(&mut self) -> std::io::Result<Vec<u8>> {
        self.frames.recv().await.unwrap_or_else(|| {
            Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "Hard USB helper frame reader ended",
            ))
        })
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
        let length = u32::try_from(frame.len())
            .map_err(|_| std::io::Error::other("Pier helper frame length overflow"))?;
        self.stdin.write_u32_le(length).await?;
        self.stdin.write_all(frame).await?;
        self.stdin.flush().await
    }

    /// Reads the bounded helper diagnostic after it exits.
    pub async fn diagnostic(&mut self) -> String {
        let mut diagnostic = String::new();
        let mut limited = (&mut self.stderr).take(4 * 1024);
        let _ = tokio::time::timeout(
            Duration::from_millis(200),
            limited.read_to_string(&mut diagnostic),
        )
        .await;
        diagnostic.trim().to_owned()
    }

    /// Terminates the helper and waits for process cleanup.
    pub async fn shutdown(mut self) {
        drop(self.stdin);
        let deadline = tokio::time::Duration::from_secs(2);
        if tokio::time::timeout(deadline, self.child.wait())
            .await
            .is_err()
        {
            let _ = self.child.kill().await;
            let _ = self.child.wait().await;
        }
        self.reader_task.abort();
        let _ = self.reader_task.await;
    }
}

#[cfg(feature = "usb-hard-lab")]
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

#[cfg(feature = "usb-hard-lab")]
fn run_bridge(generation: AttachmentGeneration, speed: UsbSpeed) -> Result<(), String> {
    if !nix::unistd::geteuid().is_root() {
        return Err("must run as root".to_owned());
    }
    let mut controller = VhciController::open_and_register(1).map_err(|error| error.to_string())?;
    controller
        .connect(1, speed)
        .map_err(|error| error.to_string())?;

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

    let mut stdout = std::io::stdout().lock();
    let mut next_urb_id = NonZeroU32::MIN;
    let mut handles = BTreeMap::<UrbId, u64>::new();
    let mut cancelled = BTreeMap::<UrbId, u8>::new();
    let mut input_closed = false;
    loop {
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
                    let Some(handle) = handles.remove(&completion.urb_id) else {
                        if let Some(remaining) = cancelled.get_mut(&completion.urb_id) {
                            *remaining = remaining.saturating_sub(1);
                            if *remaining == 0 {
                                cancelled.remove(&completion.urb_id);
                            }
                            continue;
                        }
                        return Err("completion references unknown URB".to_owned());
                    };
                    controller
                        .giveback(handle, status_to_errno(completion.status), payload)
                        .map_err(|error| error.to_string())?;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    input_closed = true;
                    break;
                }
            }
        }

        match controller.fetch_work(Duration::from_millis(20)) {
            Ok(FetchedWork::Port(state)) => {
                handle_port_state(&mut controller, state).map_err(|error| error.to_string())?;
            }
            Ok(FetchedWork::Cancel { handle }) => {
                if let Some((&urb_id, _)) =
                    handles.iter().find(|(_, candidate)| **candidate == handle)
                {
                    handles.remove(&urb_id);
                    if cancelled.len() >= arcen_usb_bridge::MAX_IN_FLIGHT_URBS {
                        return Err("helper cancellation tombstone limit exceeded".to_owned());
                    }
                    // The submit response and the explicit cancel response can
                    // race. Both are terminal from Deck's perspective; ignore
                    // at most those two stale completions after owning the
                    // kernel giveback here.
                    cancelled.insert(urb_id, 2);
                    let frame = encode_usb_urb_cancel(generation, urb_id);
                    write_sync_frame(&mut stdout, &frame)?;
                    controller
                        .giveback(handle, -nix::libc::ECONNRESET, &[])
                        .map_err(|error| error.to_string())?;
                }
            }
            Ok(FetchedWork::Urb(urb)) => {
                let transfer_kind = urb.transfer_kind().map_err(|error| error.to_string())?;
                if transfer_kind == arcen_usb_bridge::TransferKind::Interrupt {
                    std::thread::sleep(urb.interrupt_cadence());
                }
                let urb_id = UrbId::new(next_urb_id);
                next_urb_id =
                    NonZeroU32::new(next_urb_id.get().wrapping_add(1)).unwrap_or(NonZeroU32::MIN);
                if handles.insert(urb_id, urb.handle).is_some() {
                    return Err("helper generated duplicate URB id".to_owned());
                }
                if handles.len() > arcen_usb_bridge::MAX_IN_FLIGHT_URBS {
                    return Err("helper in-flight URB limit exceeded".to_owned());
                }
                let payload = if urb.direction() == TransferDirection::Out && urb.buffer_length > 0
                {
                    controller
                        .fetch_out_data(urb.handle, urb.buffer_length)
                        .map_err(|error| error.to_string())?
                } else {
                    Vec::new()
                };
                let frame = encode_usb_urb_submit(
                    UsbUrbSubmitHeader {
                        generation,
                        urb_id,
                        endpoint: arcen_usb_bridge::EndpointAddress(urb.endpoint()),
                        transfer_kind,
                        timeout_ms: 1_000,
                        declared_length: u32::try_from(urb.buffer_length)
                            .map_err(|_| "URB length exceeds u32".to_owned())?,
                        setup: (transfer_kind == arcen_usb_bridge::TransferKind::Control)
                            .then_some(SetupPacket {
                                request_type: urb.setup.request_type,
                                request: urb.setup.request,
                                value: urb.setup.value,
                                index: urb.setup.index,
                                length: urb.setup.length,
                            }),
                    },
                    &payload,
                )
                .map_err(|error| format!("{error:?}"))?;
                write_sync_frame(&mut stdout, &frame)?;
            }
            Err(vhci::VhciError::TimedOut) => {
                if input_closed {
                    break;
                }
            }
            Err(error) => return Err(error.to_string()),
        }
    }
    let _ = controller.disconnect(1);
    Ok(())
}

#[cfg(feature = "usb-hard-lab")]
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

#[cfg(feature = "usb-hard-lab")]
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

#[cfg(feature = "usb-hard-lab")]
const fn status_to_errno(status: UrbStatus) -> i32 {
    match status {
        UrbStatus::Success => 0,
        UrbStatus::Cancelled => -nix::libc::ECONNRESET,
        UrbStatus::TimedOut => -nix::libc::ETIMEDOUT,
        UrbStatus::Stall => -nix::libc::EPIPE,
        UrbStatus::Disconnected => -nix::libc::ENODEV,
        UrbStatus::Protocol => -nix::libc::EPROTO,
        UrbStatus::Io => -nix::libc::EIO,
    }
}

#[cfg(feature = "usb-hard-lab")]
const fn speed_label(speed: UsbSpeed) -> &'static str {
    match speed {
        UsbSpeed::Low => "low",
        UsbSpeed::Full => "full",
        UsbSpeed::High => "high",
    }
}

#[cfg(feature = "usb-hard-lab")]
fn parse_speed(value: &str) -> Result<UsbSpeed, String> {
    match value {
        "low" => Ok(UsbSpeed::Low),
        "full" => Ok(UsbSpeed::Full),
        "high" => Ok(UsbSpeed::High),
        other => Err(format!("unsupported USB bridge speed {other:?}")),
    }
}

#[cfg(feature = "usb-hard-lab")]
fn run_self_test() -> Result<(), String> {
    if !nix::unistd::geteuid().is_root() {
        return Err("must run as root".to_owned());
    }
    let mut controller = VhciController::open_and_register(1).map_err(|error| error.to_string())?;
    controller
        .connect(1, UsbSpeed::High)
        .map_err(|error| error.to_string())?;
    let mut tablet = SyntheticTabletDevice::default();
    let deadline = Instant::now() + SELF_TEST_RUNTIME;
    let mut configured_at = None;
    let mut completed = 0_u64;

    while Instant::now() < deadline {
        match controller.fetch_work(Duration::from_millis(200)) {
            Ok(FetchedWork::Port(state)) => {
                handle_port_state(&mut controller, state).map_err(|error| error.to_string())?;
            }
            Ok(FetchedWork::Cancel { handle }) => {
                controller
                    .giveback(handle, -nix::libc::ECONNRESET, &[])
                    .map_err(|error| error.to_string())?;
            }
            Ok(FetchedWork::Urb(urb)) => {
                complete_urb(&mut controller, &mut tablet, urb)
                    .map_err(|error| error.to_string())?;
                completed = completed.saturating_add(1);
                if tablet.configuration() == 1 && configured_at.is_none() {
                    configured_at = Some(Instant::now());
                    println!(
                        "USB_BRIDGE_READY bus={} port=1 product=\"Arcen USB Bridge Lab Tablet\"",
                        controller.bus_number()
                    );
                }
            }
            Err(vhci::VhciError::TimedOut) => {}
            Err(error) => return Err(error.to_string()),
        }
        if configured_at.is_some_and(|configured| configured.elapsed() >= Duration::from_secs(2)) {
            break;
        }
    }

    controller
        .disconnect(1)
        .map_err(|error| error.to_string())?;
    if configured_at.is_none() {
        return Err(format!(
            "kernel did not configure the synthetic tablet; completed_urbs={completed}"
        ));
    }
    println!(
        "USB_BRIDGE_SELF_TEST_OK bus={} completed_urbs={completed}",
        controller.bus_number()
    );
    Ok(())
}

#[cfg(feature = "usb-hard-lab")]
fn handle_port_state(
    controller: &mut VhciController,
    state: PortState,
) -> Result<(), vhci::VhciError> {
    if state.reset_requested() {
        controller.complete_reset(state.index)
    } else if state.resume_requested() {
        controller.complete_resume(state.index)
    } else {
        Ok(())
    }
}

#[cfg(feature = "usb-hard-lab")]
fn complete_urb(
    controller: &mut VhciController,
    tablet: &mut SyntheticTabletDevice,
    urb: VhciUrb,
) -> Result<(), vhci::VhciError> {
    match urb.transfer_kind() {
        Ok(arcen_usb_bridge::TransferKind::Control) => {
            let setup = SetupPacket {
                request_type: urb.setup.request_type,
                request: urb.setup.request,
                value: urb.setup.value,
                index: urb.setup.index,
                length: urb.setup.length,
            };
            if urb.direction() == TransferDirection::Out && urb.buffer_length > 0 {
                let _ = controller.fetch_out_data(urb.handle, urb.buffer_length)?;
            }
            match tablet.handle_control(setup) {
                ControlResponse::Ack => controller.giveback(urb.handle, 0, &[]),
                ControlResponse::Data(data) => controller.giveback(urb.handle, 0, &data),
                ControlResponse::Stall => controller.giveback(urb.handle, -nix::libc::EPIPE, &[]),
            }
        }
        Ok(arcen_usb_bridge::TransferKind::Interrupt)
            if urb.direction() == TransferDirection::In =>
        {
            std::thread::sleep(urb.interrupt_cadence());
            let report = PenSample {
                x: 0.5,
                y: 0.5,
                pressure: 0.0,
                tilt_x_degrees: 0.0,
                tilt_y_degrees: 0.0,
                switches: PenSwitches::default().with(PenSwitch::InRange, true),
            }
            .encode_report();
            controller.giveback(urb.handle, 0, &report)
        }
        _ => controller.giveback(urb.handle, -nix::libc::EPIPE, &[]),
    }
}

#[cfg(all(test, feature = "usb-hard-lab"))]
mod tests {
    use super::*;

    #[test]
    fn physical_wacom_profile_selects_full_speed() {
        assert_eq!(
            authorized_device_speed(Some(&UsbHardDeviceMsg {
                vendor_id: WACOM_VENDOR_ID,
                product_id: WACOM_INTUOS5_TOUCH_L_PRODUCT_ID,
                bcd_device: WACOM_INTUOS5_TOUCH_L_BCD_DEVICE,
                device_class: 0,
                speed: UsbSpeed::Full,
            })),
            Some(UsbSpeed::Full)
        );
    }

    #[test]
    fn wrong_identity_or_speed_is_denied() {
        for device in [
            UsbHardDeviceMsg {
                vendor_id: WACOM_VENDOR_ID,
                product_id: WACOM_INTUOS5_TOUCH_L_PRODUCT_ID,
                bcd_device: WACOM_INTUOS5_TOUCH_L_BCD_DEVICE,
                device_class: 0,
                speed: UsbSpeed::High,
            },
            UsbHardDeviceMsg {
                vendor_id: WACOM_VENDOR_ID,
                product_id: 0xffff,
                bcd_device: WACOM_INTUOS5_TOUCH_L_BCD_DEVICE,
                device_class: 0,
                speed: UsbSpeed::Full,
            },
        ] {
            assert_eq!(authorized_device_speed(Some(&device)), None);
        }
    }

    #[test]
    fn speed_labels_round_trip() {
        for speed in [UsbSpeed::Low, UsbSpeed::Full, UsbSpeed::High] {
            assert_eq!(parse_speed(speed_label(speed)).unwrap(), speed);
        }
    }
}
