//! Root-owned Linux deskside evidence, evdev grabs, console display plans, and recovery.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use arcen_session::deskside::{
    DesksideControl, DesksideEffect, DesksideEvent, DesksideLeaseSpec, DesksideProtection,
    PhysicalHostEvidence,
};
use arcen_session::deskside::{DesksidePolicy, DesksideRefusalReason};
#[cfg(any(test, target_os = "linux"))]
use arcen_session::deskside::{EvidenceStatus, PhysicalEvidenceSummary};
use arcen_session::restore_lease::LeaseOwnerId;
use arcen_session::restore_lease::StateFingerprint;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const MAX_INPUT_PINS: usize = 32;
const MAX_OUTPUT_PINS: usize = 16;
const MAX_JOURNAL_BYTES: u64 = 64 * 1024;
const HASH_HEX_BYTES: usize = 64;
const XRANDR_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const XSET_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);
const LOGINCTL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);
const MAX_LOGINCTL_BYTES: usize = 64 * 1024;

#[derive(Debug)]
struct BoundedOutput {
    success: bool,
    stdout: Vec<u8>,
}

async fn command_output_bounded(
    mut command: tokio::process::Command,
    timeout: std::time::Duration,
    max_bytes: usize,
    description: &str,
) -> Result<BoundedOutput, String> {
    use std::process::Stdio;
    use tokio::io::AsyncReadExt as _;

    command
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|error| format!("spawn {description}: {error}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| format!("{description} stdout was not piped"))?;
    let reader = tokio::spawn(async move {
        let mut bytes = Vec::new();
        stdout
            .take((max_bytes + 1) as u64)
            .read_to_end(&mut bytes)
            .await
            .map(|_| bytes)
    });
    let status = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(result) => result.map_err(|error| format!("wait for {description}: {error}"))?,
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            reader.abort();
            let _ = reader.await;
            return Err(format!(
                "{description} exceeded {}ms and was killed and reaped",
                timeout.as_millis()
            ));
        }
    };
    let stdout = reader
        .await
        .map_err(|error| format!("join {description} output reader: {error}"))?
        .map_err(|error| format!("read {description} output: {error}"))?;
    if stdout.len() > max_bytes {
        return Err(format!("{description} output exceeds {max_bytes} bytes"));
    }
    Ok(BoundedOutput {
        success: status.success(),
        stdout,
    })
}

async fn command_status_bounded(
    mut command: tokio::process::Command,
    timeout: std::time::Duration,
    description: &str,
) -> Result<(), String> {
    use std::process::Stdio;

    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|error| format!("spawn {description}: {error}"))?;
    let status = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(result) => result.map_err(|error| format!("wait for {description}: {error}"))?,
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return Err(format!(
                "{description} exceeded {}ms and was killed and reaped",
                timeout.as_millis()
            ));
        }
    };
    status
        .success()
        .then_some(())
        .ok_or_else(|| format!("{description} exited unsuccessfully"))
}

/// Disabled-by-default physical-console policy passed to the root launcher.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LinuxDesksideConfig {
    pub enabled: bool,
    pub firmware_sha256: String,
    pub console_uid: Option<u32>,
    pub console_display: Option<String>,
    pub console_xauthority: Option<PathBuf>,
    pub input_devices: Vec<PathBuf>,
    pub outputs: Vec<PhysicalOutputPin>,
}

impl std::fmt::Debug for LinuxDesksideConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LinuxDesksideConfig")
            .field("enabled", &self.enabled)
            .field("console_uid", &self.console_uid)
            .field("console_display", &self.console_display)
            .field(
                "console_xauthority_configured",
                &self.console_xauthority.is_some(),
            )
            .field("input_device_count", &self.input_devices.len())
            .field("output_count", &self.outputs.len())
            .finish()
    }
}

impl LinuxDesksideConfig {
    pub fn validate(
        &self,
        pam_mode: bool,
        uinput_mode: bool,
        capture_display: &str,
        session_gpu_head: &str,
    ) -> Result<(), String> {
        if !self.enabled {
            if self.firmware_sha256.is_empty()
                && self.console_uid.is_none()
                && self.console_display.is_none()
                && self.console_xauthority.is_none()
                && self.input_devices.is_empty()
                && self.outputs.is_empty()
            {
                return Ok(());
            }
            return Err("deskside pins require --deskside".to_string());
        }
        if !pam_mode || !uinput_mode {
            return Err("deskside requires PAM and uinput session ownership".to_string());
        }
        validate_hash(&self.firmware_sha256, "firmware_sha256")?;
        let console_uid = self
            .console_uid
            .ok_or_else(|| "deskside requires --deskside-console-uid".to_string())?;
        if console_uid == 0 {
            return Err("deskside console UID must be a non-root user".to_string());
        }
        let console_display = self
            .console_display
            .as_deref()
            .ok_or_else(|| "deskside requires --deskside-console-display".to_string())?;
        if !valid_display(console_display) || console_display == capture_display {
            return Err(
                "deskside console DISPLAY must be valid and distinct from capture DISPLAY"
                    .to_string(),
            );
        }
        let xauthority = self
            .console_xauthority
            .as_deref()
            .ok_or_else(|| "deskside requires --deskside-console-xauthority".to_string())?;
        if !linux_absolute_path(xauthority) {
            return Err("deskside console Xauthority must be absolute".to_string());
        }
        if self.input_devices.is_empty() || self.input_devices.len() > MAX_INPUT_PINS {
            return Err(format!(
                "deskside requires 1..={MAX_INPUT_PINS} physical input pins"
            ));
        }
        let mut input_pins = HashSet::with_capacity(self.input_devices.len());
        for pin in &self.input_devices {
            if !pin.starts_with("/dev/input/by-id") || !input_pins.insert(pin) {
                return Err(
                    "deskside input pins must be unique absolute /dev/input/by-id paths"
                        .to_string(),
                );
            }
        }
        if self.outputs.is_empty() || self.outputs.len() > MAX_OUTPUT_PINS {
            return Err(format!(
                "deskside requires 1..={MAX_OUTPUT_PINS} physical output pins"
            ));
        }
        let mut output_names = HashSet::with_capacity(self.outputs.len());
        let mut drm_hashes = HashSet::with_capacity(self.outputs.len());
        let mut edid_hashes = HashSet::with_capacity(self.outputs.len());
        for output in &self.outputs {
            output.validate()?;
            if output.name == session_gpu_head
                || !output_names.insert(output.name.as_str())
                || !drm_hashes.insert(output.drm_sha256.as_str())
                || !edid_hashes.insert(output.edid_sha256.as_str())
            {
                return Err(
                    "deskside output pins must be unique and distinct from session GPU head"
                        .to_string(),
                );
            }
        }
        Ok(())
    }

    #[must_use]
    pub const fn policy(&self) -> DesksidePolicy {
        if self.enabled {
            DesksidePolicy::Required
        } else {
            DesksidePolicy::Disabled
        }
    }

    #[must_use]
    pub fn journal_path() -> PathBuf {
        PathBuf::from("/run/arcen/deskside-recovery.json")
    }
}

/// Operator pin for one physical console output.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PhysicalOutputPin {
    pub name: String,
    pub drm_sha256: String,
    pub edid_sha256: String,
}

impl PhysicalOutputPin {
    pub fn parse(value: &str) -> Result<Self, String> {
        let mut fields = value.split(',');
        let pin = Self {
            name: fields.next().unwrap_or_default().to_string(),
            drm_sha256: fields.next().unwrap_or_default().to_string(),
            edid_sha256: fields.next().unwrap_or_default().to_string(),
        };
        if fields.next().is_some() {
            return Err("--deskside-output expects NAME,DRM_SHA256,EDID_SHA256".to_string());
        }
        pin.validate()?;
        Ok(pin)
    }

    fn validate(&self) -> Result<(), String> {
        if self.name.is_empty()
            || self.name.len() > 32
            || !self
                .name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err("deskside output name is invalid".to_string());
        }
        validate_hash(&self.drm_sha256, "DRM_SHA256")?;
        validate_hash(&self.edid_sha256, "EDID_SHA256")
    }
}

trait InputLease: Send {
    fn is_complete(&self) -> bool;
    fn release(&mut self) -> Result<(), String>;
}

/// Root-owned composite. Drop schedules display-first cleanup; callers use `restore`.
pub struct LinuxDesksideGuard {
    protection: DesksideProtection,
    input: Option<Box<dyn InputLease>>,
    display: Option<ConsoleDisplayGuard>,
    input_monitor: Option<tokio::task::JoinHandle<()>>,
    failure_rx: Option<tokio::sync::mpsc::Receiver<()>>,
}

impl LinuxDesksideGuard {
    #[cfg(target_os = "linux")]
    pub async fn arm(
        config: &LinuxDesksideConfig,
        session_id: &str,
        uid: u32,
        capture_display: &str,
        session_gpu_head: &str,
    ) -> Result<Self, String> {
        config.validate(true, true, capture_display, session_gpu_head)?;
        let console = discover_console_session(config, session_id, uid, capture_display).await?;
        validate_bare_metal(config)?;
        validate_console_xauthority(config, console.uid)?;

        let display_evidence = inspect_outputs(config, session_gpu_head).await?;
        let input_evidence = inspect_input_inventory(config)?;
        let evidence = PhysicalHostEvidence::validate(PhysicalEvidenceSummary {
            runtime_fresh: true,
            host: EvidenceStatus::Positive,
            console_session: EvidenceStatus::Positive,
            local_input: EvidenceStatus::Positive,
            local_displays: EvidenceStatus::Positive,
            active_resources_accounted: EvidenceStatus::Positive,
            capture_separation: EvidenceStatus::Positive,
            input_fingerprint: Some(input_evidence.fingerprint),
            display_fingerprint: Some(display_evidence.fingerprint),
        })
        .map_err(|reason| format!("deskside_refused:{}", reason.as_str()))?;
        let runtime: std::sync::Arc<dyn DisplayRuntime> =
            std::sync::Arc::new(NativeDisplayRuntime {
                config: config.clone(),
                session_gpu_head: session_gpu_head.to_string(),
            });
        let devices = input_evidence.devices;
        let monitored_input_fingerprint = input_evidence.fingerprint;
        let mut guard = Self::arm_with_components(
            config,
            session_id,
            evidence,
            display_evidence.snapshot,
            runtime,
            move || {
                InputGrabGuard::acquire(&devices)
                    .map(|input| Box::new(input) as Box<dyn InputLease>)
            },
        )
        .await?;
        let (failure_tx, failure_rx) = tokio::sync::mpsc::channel(1);
        guard.input_monitor = Some(tokio::spawn(monitor_inventories(
            config.clone(),
            session_gpu_head.to_string(),
            session_id.to_string(),
            uid,
            capture_display.to_string(),
            console,
            monitored_input_fingerprint,
            failure_tx,
        )));
        guard.failure_rx = Some(failure_rx);
        Ok(guard)
    }

    #[cfg(not(target_os = "linux"))]
    pub async fn arm(
        _config: &LinuxDesksideConfig,
        _session_id: &str,
        _uid: u32,
        _capture_display: &str,
        _session_gpu_head: &str,
    ) -> Result<Self, String> {
        Err("Linux deskside controls are unavailable on this platform".to_string())
    }

    async fn arm_with_components<F>(
        config: &LinuxDesksideConfig,
        owner_id: &str,
        evidence: PhysicalHostEvidence,
        snapshot: ConsoleSnapshot,
        runtime: std::sync::Arc<dyn DisplayRuntime>,
        acquire_input: F,
    ) -> Result<Self, String>
    where
        F: FnOnce() -> Result<Box<dyn InputLease>, String>,
    {
        let owner = LeaseOwnerId::new(owner_id.to_string()).map_err(|error| error.to_string())?;
        let input_original = StateFingerprint::new(b"linux-evdev-ungrabbed-v1")
            .map_err(|error| error.to_string())?;
        let display_target = StateFingerprint::new(b"linux-console-outputs-off-v1")
            .map_err(|error| error.to_string())?;
        let mut protection = DesksideProtection::new();
        if protection.begin_arm(
            config.policy().decide(Ok(&evidence)),
            owner,
            DesksideLeaseSpec {
                original: input_original,
                protected: evidence.input_fingerprint(),
            },
            DesksideLeaseSpec {
                original: evidence.display_fingerprint(),
                protected: display_target,
            },
        ) != Ok(DesksideEffect::Arm(DesksideControl::LocalInput))
        {
            return Err("deskside shared input arm sequencing failed".to_string());
        }
        let _ = protection.apply(DesksideEvent::ArmSucceeded(DesksideControl::LocalInput));
        let input = acquire_input()?;
        let _ = protection.apply(DesksideEvent::ApplySucceeded(DesksideControl::LocalInput));
        if !input.is_complete()
            || protection.apply(DesksideEvent::VerifySucceeded(DesksideControl::LocalInput))
                != DesksideEffect::Arm(DesksideControl::LocalDisplays)
        {
            return Err("deskside input verification failed".to_string());
        }
        let _ = protection.apply(DesksideEvent::ArmSucceeded(DesksideControl::LocalDisplays));
        let display = match ConsoleDisplayGuard::apply_with_runtime(runtime, snapshot).await {
            Ok(display) => display,
            Err(error) => {
                drop(input);
                return Err(error);
            }
        };
        let _ = protection.apply(DesksideEvent::ApplySucceeded(
            DesksideControl::LocalDisplays,
        ));
        if protection.apply(DesksideEvent::VerifySucceeded(
            DesksideControl::LocalDisplays,
        )) != DesksideEffect::ProtectionEstablished
        {
            return Err("deskside display verification failed".to_string());
        }
        Ok(Self {
            protection,
            input: Some(input),
            display: Some(display),
            input_monitor: None,
            failure_rx: None,
        })
    }

    /// Resolves only when physical input inventory changes or supervision fails.
    pub async fn wait_for_failure(&mut self) {
        match self.failure_rx.as_mut() {
            Some(receiver) => {
                let _ = receiver.recv().await;
            }
            None => std::future::pending::<()>().await,
        }
    }

    pub async fn restore(&mut self) -> Result<(), String> {
        if let Some(monitor) = self.input_monitor.take() {
            monitor.abort();
            let _ = monitor.await;
        }
        let _ = self.protection.apply(DesksideEvent::BeginDraining);
        let _ = self.protection.apply(DesksideEvent::RemoteInjectionStopped);
        let display = if let Some(display) = self.display.as_mut() {
            display.restore().await
        } else {
            Ok(())
        };
        let _ = self.protection.apply(if display.is_ok() {
            DesksideEvent::RestoreSucceeded(DesksideControl::LocalDisplays)
        } else {
            DesksideEvent::RestoreFailed(DesksideControl::LocalDisplays)
        });
        let input = if let Some(input) = self.input.as_mut() {
            input.release()
        } else {
            Ok(())
        };
        let _ = self.protection.apply(if input.is_ok() {
            DesksideEvent::RestoreSucceeded(DesksideControl::LocalInput)
        } else {
            DesksideEvent::RestoreFailed(DesksideControl::LocalInput)
        });
        self.input = None;
        self.display = None;
        match (display, input) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
            (Err(display), Err(input)) => {
                Err(format!("{display}; input release also failed: {input}"))
            }
        }
    }
}

impl Drop for LinuxDesksideGuard {
    fn drop(&mut self) {
        if let Some(monitor) = self.input_monitor.take() {
            monitor.abort();
        }
        let display = self.display.take();
        let input = self.input.take();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                if let Some(mut display) = display {
                    let _ = display.restore().await;
                }
                if let Some(mut input) = input {
                    let _ = input.release();
                }
            });
        } else {
            drop(display);
            drop(input);
        }
    }
}

#[cfg(target_os = "linux")]
async fn monitor_inventories(
    config: LinuxDesksideConfig,
    session_gpu_head: String,
    streaming_session_id: String,
    streaming_uid: u32,
    capture_display: String,
    expected_console: LogindSessionFact,
    expected: StateFingerprint,
    failure: tokio::sync::mpsc::Sender<()>,
) {
    let mut interval = tokio::time::interval(std::time::Duration::from_millis(250));
    interval.tick().await;
    loop {
        interval.tick().await;
        let input_config = config.clone();
        let observed =
            tokio::task::spawn_blocking(move || inspect_input_inventory(&input_config)).await;
        let input_healthy =
            matches!(observed, Ok(Ok(evidence)) if evidence.fingerprint == expected);
        let display_healthy = verify_console_protected(&config, &session_gpu_head)
            .await
            .is_ok();
        let session_healthy = match discover_logind_sessions().await {
            Ok(sessions) => select_console_session(
                &config,
                &streaming_session_id,
                streaming_uid,
                &capture_display,
                &sessions,
            )
            .is_ok_and(|console| {
                console == expected_console
                    && validate_console_xauthority(&config, console.uid).is_ok()
            }),
            Err(_) => false,
        };
        let healthy = input_healthy && display_healthy && session_healthy;
        if !healthy {
            let _ = failure.send(()).await;
            return;
        }
    }
}

#[cfg(target_os = "linux")]
struct InputEvidence {
    devices: Vec<InputDeviceBinding>,
    fingerprint: StateFingerprint,
}

#[cfg(target_os = "linux")]
#[derive(Clone)]
struct InputDeviceBinding {
    path: PathBuf,
    inode: u64,
    device: u64,
}

#[cfg(target_os = "linux")]
fn inspect_input_inventory(config: &LinuxDesksideConfig) -> Result<InputEvidence, String> {
    use evdev::Device;
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::fs::MetadataExt as _;

    let mut configured = Vec::with_capacity(config.input_devices.len());
    let mut configured_targets = HashSet::with_capacity(config.input_devices.len());
    let mut fingerprint_material = Vec::with_capacity(config.input_devices.len() * 8);
    for pin in &config.input_devices {
        let target = std::fs::canonicalize(pin)
            .map_err(|_| DesksideRefusalReason::MissingEvidence.to_string())?;
        if target.parent() != Some(Path::new("/dev/input"))
            || !target
                .file_name()
                .and_then(|value| value.to_str())
                .is_some_and(|name| name.starts_with("event"))
            || !configured_targets.insert(target.clone())
        {
            return refuse(DesksideRefusalReason::ConflictingEvidence);
        }
        let metadata = std::fs::metadata(&target)
            .map_err(|_| DesksideRefusalReason::UnknownEvidence.to_string())?;
        let device = Device::open(&target)
            .map_err(|_| DesksideRefusalReason::UnknownEvidence.to_string())?;
        let id = device.input_id();
        let disposition = classify_input_device(
            id.bus_type().0,
            id.vendor(),
            id.product(),
            device.name(),
            relevant_input(&device),
        );
        if disposition != InputDeviceDisposition::ArcenVirtual && has_absolute_axes(&device) {
            return refuse(DesksideRefusalReason::UnknownEvidence);
        }
        match disposition {
            InputDeviceDisposition::Physical => {}
            InputDeviceDisposition::ArcenVirtual
            | InputDeviceDisposition::Virtual
            | InputDeviceDisposition::Irrelevant => {
                return refuse(DesksideRefusalReason::VirtualEvidence);
            }
            InputDeviceDisposition::Unknown => {
                return refuse(DesksideRefusalReason::UnknownEvidence);
            }
        }
        fingerprint_material.extend_from_slice(&id.bus_type().0.to_be_bytes());
        fingerprint_material.extend_from_slice(&id.vendor().to_be_bytes());
        fingerprint_material.extend_from_slice(&id.product().to_be_bytes());
        fingerprint_material.extend_from_slice(&id.version().to_be_bytes());
        fingerprint_material.extend_from_slice(target.as_os_str().as_bytes());
        fingerprint_material.extend_from_slice(&metadata.ino().to_be_bytes());
        fingerprint_material.extend_from_slice(&metadata.rdev().to_be_bytes());
        configured.push(InputDeviceBinding {
            path: target,
            inode: metadata.ino(),
            device: metadata.rdev(),
        });
    }

    for entry in std::fs::read_dir("/dev/input")
        .map_err(|_| DesksideRefusalReason::UnknownEvidence.to_string())?
    {
        let path = entry
            .map_err(|_| DesksideRefusalReason::UnknownEvidence.to_string())?
            .path();
        if !path
            .file_name()
            .and_then(|value| value.to_str())
            .is_some_and(|name| name.starts_with("event"))
        {
            continue;
        }
        let Ok(device) = Device::open(&path) else {
            return refuse(DesksideRefusalReason::UnknownEvidence);
        };
        let id = device.input_id();
        let disposition = classify_input_device(
            id.bus_type().0,
            id.vendor(),
            id.product(),
            device.name(),
            relevant_input(&device),
        );
        if disposition != InputDeviceDisposition::ArcenVirtual && has_absolute_axes(&device) {
            return refuse(DesksideRefusalReason::UnknownEvidence);
        }
        match disposition {
            InputDeviceDisposition::ArcenVirtual | InputDeviceDisposition::Irrelevant => {}
            InputDeviceDisposition::Physical if configured_targets.contains(&path) => {}
            InputDeviceDisposition::Physical => {
                return refuse(DesksideRefusalReason::ConflictingEvidence);
            }
            InputDeviceDisposition::Virtual => {
                return refuse(DesksideRefusalReason::VirtualEvidence);
            }
            InputDeviceDisposition::Unknown => {
                return refuse(DesksideRefusalReason::UnknownEvidence);
            }
        }
    }
    if fingerprint_material.is_empty() {
        return refuse(DesksideRefusalReason::MissingEvidence);
    }
    let fingerprint =
        StateFingerprint::new(&fingerprint_material).map_err(|error| error.to_string())?;
    Ok(InputEvidence {
        devices: configured,
        fingerprint,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InputDeviceDisposition {
    Irrelevant,
    ArcenVirtual,
    Physical,
    Virtual,
    Unknown,
}

fn classify_input_device(
    bus: u16,
    vendor: u16,
    product: u16,
    name: Option<&str>,
    relevant: bool,
) -> InputDeviceDisposition {
    if !relevant {
        return InputDeviceDisposition::Irrelevant;
    }
    if bus == 0x06 && vendor == 0xA2CE && product == 0x0001 && name == Some("Arcen Virtual Input") {
        return InputDeviceDisposition::ArcenVirtual;
    }
    if bus == 0x06 {
        return InputDeviceDisposition::Virtual;
    }
    if matches!(bus, 0x03 | 0x05 | 0x11 | 0x18 | 0x19 | 0x1C | 0x1D | 0x1F) {
        InputDeviceDisposition::Physical
    } else {
        InputDeviceDisposition::Unknown
    }
}

#[cfg(target_os = "linux")]
fn relevant_input(device: &evdev::Device) -> bool {
    let keys = device
        .supported_keys()
        .is_some_and(|keys| keys.iter().next().is_some());
    let relative = device
        .supported_relative_axes()
        .is_some_and(|axes| axes.iter().next().is_some());
    let absolute = device
        .supported_absolute_axes()
        .is_some_and(|axes| axes.iter().next().is_some());
    keys || relative || absolute
}

#[cfg(target_os = "linux")]
fn has_absolute_axes(device: &evdev::Device) -> bool {
    device
        .supported_absolute_axes()
        .is_some_and(|axes| axes.iter().next().is_some())
}

#[cfg(target_os = "linux")]
struct InputGrabGuard {
    devices: Vec<evdev::Device>,
}

#[cfg(target_os = "linux")]
impl InputGrabGuard {
    fn acquire(bindings: &[InputDeviceBinding]) -> Result<Self, String> {
        use std::os::unix::fs::MetadataExt as _;

        let mut devices: Vec<evdev::Device> = Vec::with_capacity(bindings.len());
        for binding in bindings {
            let metadata = std::fs::metadata(&binding.path)
                .map_err(|_| "deskside input identity changed before grab".to_string())?;
            if metadata.ino() != binding.inode || metadata.rdev() != binding.device {
                return Err("deskside input identity changed before grab".to_string());
            }

            let mut device = evdev::Device::open(&binding.path)
                .map_err(|_| "deskside input open failed".to_string())?;
            if let Err(error) = device.grab() {
                for grabbed in &mut devices {
                    let _ = grabbed.ungrab();
                }
                return Err(format!("deskside input grab failed: {error}"));
            }
            devices.push(device);
        }
        Ok(Self { devices })
    }

    fn is_complete(&self) -> bool {
        !self.devices.is_empty()
    }

    fn release(&mut self) -> Result<(), String> {
        let mut failures = 0_usize;
        for device in &mut self.devices {
            if device.ungrab().is_err() {
                failures += 1;
            }
        }
        self.devices.clear();
        if failures == 0 {
            Ok(())
        } else {
            Err(format!(
                "{failures} deskside input devices failed to ungrab"
            ))
        }
    }
}

#[cfg(target_os = "linux")]
impl InputLease for InputGrabGuard {
    fn is_complete(&self) -> bool {
        InputGrabGuard::is_complete(self)
    }

    fn release(&mut self) -> Result<(), String> {
        InputGrabGuard::release(self)
    }
}

#[cfg(target_os = "linux")]
impl Drop for InputGrabGuard {
    fn drop(&mut self) {
        let _ = self.release();
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct OutputSnapshot {
    name: String,
    mode: String,
    x: i32,
    y: i32,
    primary: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct ConsoleSnapshot {
    outputs: Vec<OutputSnapshot>,
    dpms_enabled: bool,
}

struct DisplayEvidence {
    snapshot: ConsoleSnapshot,
    fingerprint: StateFingerprint,
}

type DisplayFuture<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>>;

trait DisplayRuntime: Send + Sync {
    fn write_initial(&self, journal: &RecoveryJournal) -> Result<(), String>;
    fn mark_stage(&self, stage: RecoveryStage) -> Result<(), String>;
    fn remove_journal(&self) -> Result<(), String>;
    fn run_xrandr<'a>(&'a self, plan: &'a XrandrPlan) -> DisplayFuture<'a>;
    fn run_xset(&self, args: &'static [&'static str]) -> DisplayFuture<'_>;
    fn verify_protected(&self) -> DisplayFuture<'_>;
    fn verify_restored<'a>(&'a self, snapshot: &'a ConsoleSnapshot) -> DisplayFuture<'a>;
}

#[cfg(target_os = "linux")]
async fn inspect_outputs(
    config: &LinuxDesksideConfig,
    session_gpu_head: &str,
) -> Result<DisplayEvidence, String> {
    let display = config
        .console_display
        .as_deref()
        .ok_or_else(|| DesksideRefusalReason::MissingEvidence.to_string())?;
    let xauthority = config
        .console_xauthority
        .as_deref()
        .ok_or_else(|| DesksideRefusalReason::MissingEvidence.to_string())?;
    let mut command = tokio::process::Command::new("/usr/bin/xrandr");
    command
        .args(["--display", display, "--query"])
        .env("XAUTHORITY", xauthority);
    let query = command_output_bounded(
        command,
        XRANDR_TIMEOUT,
        MAX_JOURNAL_BYTES as usize,
        "console xrandr query",
    )
    .await?;
    if !query.success {
        return refuse(DesksideRefusalReason::UnknownEvidence);
    }
    let parsed = parse_xrandr_query(&String::from_utf8_lossy(&query.stdout))?;
    let mut snapshot = Vec::with_capacity(config.outputs.len());
    let mut material = Vec::with_capacity(config.outputs.len() * HASH_HEX_BYTES * 2);
    for pin in &config.outputs {
        let state = parsed
            .iter()
            .find(|state| state.name == pin.name && !state.mode.is_empty())
            .ok_or_else(|| DesksideRefusalReason::MissingEvidence.to_string())?;
        let drm = find_drm_connector(&pin.name)?;
        if pin.name == session_gpu_head
            || hash_drm_connector(&drm)? != pin.drm_sha256
            || hash_file(&drm.join("edid"))? != pin.edid_sha256
        {
            return refuse(DesksideRefusalReason::ConflictingEvidence);
        }
        material.extend_from_slice(pin.drm_sha256.as_bytes());
        material.extend_from_slice(pin.edid_sha256.as_bytes());
        snapshot.push(state.clone());
    }
    for connector in connected_drm_connectors()? {
        let name = connector_name(&connector)?;
        if name != session_gpu_head && !config.outputs.iter().any(|pin| pin.name == name) {
            return refuse(DesksideRefusalReason::ConflictingEvidence);
        }
    }
    material.sort_unstable();
    let fingerprint = StateFingerprint::new(&material).map_err(|error| error.to_string())?;
    Ok(DisplayEvidence {
        snapshot: ConsoleSnapshot {
            outputs: snapshot,
            dpms_enabled: query_dpms(config).await?,
        },
        fingerprint,
    })
}

#[cfg(not(target_os = "linux"))]
async fn inspect_outputs(
    _config: &LinuxDesksideConfig,
    _session_gpu_head: &str,
) -> Result<DisplayEvidence, String> {
    Err("Linux display evidence is unavailable on this platform".to_string())
}

struct ConsoleDisplayGuard {
    runtime: std::sync::Arc<dyn DisplayRuntime>,
    snapshot: ConsoleSnapshot,
    restored: bool,
    spawn_cleanup: bool,
}

impl ConsoleDisplayGuard {
    #[cfg(target_os = "linux")]
    async fn apply(
        config: &LinuxDesksideConfig,
        snapshot: ConsoleSnapshot,
        session_gpu_head: &str,
    ) -> Result<Self, String> {
        Self::apply_with_runtime(
            std::sync::Arc::new(NativeDisplayRuntime {
                config: config.clone(),
                session_gpu_head: session_gpu_head.to_string(),
            }),
            snapshot,
        )
        .await
    }

    async fn apply_with_runtime(
        runtime: std::sync::Arc<dyn DisplayRuntime>,
        snapshot: ConsoleSnapshot,
    ) -> Result<Self, String> {
        runtime.write_initial(&RecoveryJournal {
            version: 1,
            stage: RecoveryStage::Armed,
            console_display: String::new(),
            console_xauthority: PathBuf::from("/"),
            snapshot: snapshot.clone(),
        })?;
        let mut guard = Self {
            runtime,
            snapshot,
            restored: false,
            spawn_cleanup: true,
        };
        if let Err(error) = guard.runtime.run_xrandr(&blank_plan(&guard.snapshot)).await {
            let restore = guard.restore().await;
            return Err(combine_apply_rollback(error, restore));
        }
        if let Err(error) = guard.runtime.run_xset(&["dpms", "force", "off"]).await {
            let restore = guard.restore().await;
            return Err(combine_apply_rollback(error, restore));
        }
        if let Err(error) = guard.runtime.verify_protected().await {
            let restore = guard.restore().await;
            return Err(combine_apply_rollback(error, restore));
        }
        if let Err(error) = guard.runtime.mark_stage(RecoveryStage::Protected) {
            let restore = guard.restore().await;
            return Err(combine_apply_rollback(
                format!("commit protected recovery stage: {error}"),
                restore,
            ));
        }
        Ok(guard)
    }

    async fn restore(&mut self) -> Result<(), String> {
        if self.restored {
            return Ok(());
        }
        let mut errors = Vec::new();
        if let Err(error) = self.runtime.mark_stage(RecoveryStage::Restoring) {
            errors.push(format!("journal restoring stage: {error}"));
        }
        let mut physical_ok = true;
        if let Err(error) = self.runtime.run_xrandr(&restore_plan(&self.snapshot)).await {
            physical_ok = false;
            errors.push(format!("restore xrandr: {error}"));
        }
        if let Err(error) = self.runtime.run_xset(&["dpms", "force", "on"]).await {
            physical_ok = false;
            errors.push(format!("restore DPMS on: {error}"));
        }
        if !self.snapshot.dpms_enabled {
            if let Err(error) = self.runtime.run_xset(&["-dpms"]).await {
                physical_ok = false;
                errors.push(format!("restore DPMS disabled policy: {error}"));
            }
        }
        if let Err(error) = self.runtime.verify_restored(&self.snapshot).await {
            physical_ok = false;
            errors.push(format!("verify restored console: {error}"));
        }
        if physical_ok {
            self.restored = true;
            if errors.is_empty() {
                if let Err(error) = self.runtime.remove_journal() {
                    errors.push(format!("remove restored journal: {error}"));
                }
            }
        } else {
            if let Err(error) = self.runtime.mark_stage(RecoveryStage::RestoreFailed) {
                errors.push(format!("journal restore-failed stage: {error}"));
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }
}

impl Drop for ConsoleDisplayGuard {
    fn drop(&mut self) {
        if self.restored || !self.spawn_cleanup {
            return;
        }
        let runtime = std::sync::Arc::clone(&self.runtime);
        let snapshot = self.snapshot.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let mut cleanup = ConsoleDisplayGuard {
                    runtime,
                    snapshot,
                    restored: false,
                    spawn_cleanup: false,
                };
                let _ = cleanup.restore().await;
            });
        }
    }
}

fn combine_apply_rollback(error: String, rollback: Result<(), String>) -> String {
    match rollback {
        Ok(()) => error,
        Err(rollback) => format!("{error}; rollback failed: {rollback}"),
    }
}

#[cfg(target_os = "linux")]
struct NativeDisplayRuntime {
    config: LinuxDesksideConfig,
    session_gpu_head: String,
}

#[cfg(target_os = "linux")]
impl DisplayRuntime for NativeDisplayRuntime {
    fn write_initial(&self, journal: &RecoveryJournal) -> Result<(), String> {
        let path = LinuxDesksideConfig::journal_path();
        if path.exists() {
            return Err(
                "deskside recovery journal is pending; startup recovery must complete first"
                    .to_string(),
            );
        }
        let mut journal = journal.clone();
        journal.console_display = self.config.console_display.clone().unwrap_or_default();
        journal.console_xauthority = self.config.console_xauthority.clone().unwrap_or_default();
        write_journal(&path, &journal)
    }

    fn mark_stage(&self, stage: RecoveryStage) -> Result<(), String> {
        update_journal_stage(stage)
    }

    fn remove_journal(&self) -> Result<(), String> {
        remove_journal(&LinuxDesksideConfig::journal_path())
    }

    fn run_xrandr<'a>(&'a self, plan: &'a XrandrPlan) -> DisplayFuture<'a> {
        Box::pin(run_xrandr_plan(&self.config, plan))
    }

    fn run_xset(&self, args: &'static [&'static str]) -> DisplayFuture<'_> {
        Box::pin(run_xset(&self.config, args))
    }

    fn verify_protected(&self) -> DisplayFuture<'_> {
        Box::pin(verify_console_protected(
            &self.config,
            &self.session_gpu_head,
        ))
    }

    fn verify_restored<'a>(&'a self, snapshot: &'a ConsoleSnapshot) -> DisplayFuture<'a> {
        Box::pin(verify_console_restored(&self.config, snapshot))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct XrandrPlan {
    args: Vec<String>,
}

fn blank_plan(snapshot: &ConsoleSnapshot) -> XrandrPlan {
    let mut args = Vec::with_capacity(snapshot.outputs.len() * 3);
    for output in &snapshot.outputs {
        args.extend([
            "--output".to_string(),
            output.name.clone(),
            "--off".to_string(),
        ]);
    }
    XrandrPlan { args }
}

fn restore_plan(snapshot: &ConsoleSnapshot) -> XrandrPlan {
    let mut args = Vec::with_capacity(snapshot.outputs.len() * 8);
    for output in &snapshot.outputs {
        args.extend([
            "--output".to_string(),
            output.name.clone(),
            "--mode".to_string(),
            output.mode.clone(),
            "--pos".to_string(),
            format!("{}x{}", output.x, output.y),
        ]);
        if output.primary {
            args.push("--primary".to_string());
        }
    }
    XrandrPlan { args }
}

fn parse_xrandr_query(value: &str) -> Result<Vec<OutputSnapshot>, String> {
    if value.len() > MAX_JOURNAL_BYTES as usize {
        return Err("xrandr output exceeds bound".to_string());
    }
    let mut outputs = Vec::new();
    for line in value.lines() {
        let mut fields = line.split_whitespace();
        let Some(name) = fields.next() else {
            continue;
        };
        if fields.next() != Some("connected") {
            continue;
        }
        if name.len() > 32 || outputs.len() >= MAX_OUTPUT_PINS {
            return Err("xrandr output inventory exceeds bound".to_string());
        }
        let remaining = fields.collect::<Vec<_>>();
        let primary = remaining.contains(&"primary");
        let geometry = remaining.iter().find_map(|field| parse_geometry(field));
        let (mode, x, y) = geometry.unwrap_or_else(|| (String::new(), 0, 0));
        outputs.push(OutputSnapshot {
            name: name.to_string(),
            mode,
            x,
            y,
            primary,
        });
    }
    Ok(outputs)
}

fn parse_geometry(value: &str) -> Option<(String, i32, i32)> {
    let plus = value.find('+')?;
    let mode = value[..plus].to_string();
    if !mode.contains('x') || mode.len() > 24 {
        return None;
    }
    let coordinates = &value[plus + 1..];
    let (x, y) = coordinates.split_once('+')?;
    Some((mode, x.parse().ok()?, y.parse().ok()?))
}

#[cfg(target_os = "linux")]
async fn run_xrandr_plan(config: &LinuxDesksideConfig, plan: &XrandrPlan) -> Result<(), String> {
    let mut command = tokio::process::Command::new("/usr/bin/xrandr");
    command
        .args(&plan.args)
        .env(
            "DISPLAY",
            config.console_display.as_deref().unwrap_or_default(),
        )
        .env(
            "XAUTHORITY",
            config
                .console_xauthority
                .as_deref()
                .unwrap_or_else(|| Path::new("")),
        );
    command_status_bounded(command, XRANDR_TIMEOUT, "console xrandr plan").await
}

#[cfg(target_os = "linux")]
async fn run_xset(config: &LinuxDesksideConfig, args: &[&str]) -> Result<(), String> {
    let mut command = tokio::process::Command::new("/usr/bin/xset");
    command
        .args(args)
        .env(
            "DISPLAY",
            config.console_display.as_deref().unwrap_or_default(),
        )
        .env(
            "XAUTHORITY",
            config
                .console_xauthority
                .as_deref()
                .unwrap_or_else(|| Path::new("")),
        );
    command_status_bounded(command, XSET_TIMEOUT, "console DPMS plan").await
}

#[cfg(target_os = "linux")]
async fn query_xrandr(config: &LinuxDesksideConfig) -> Result<String, String> {
    let mut command = tokio::process::Command::new("/usr/bin/xrandr");
    command
        .arg("--query")
        .env(
            "DISPLAY",
            config.console_display.as_deref().unwrap_or_default(),
        )
        .env(
            "XAUTHORITY",
            config
                .console_xauthority
                .as_deref()
                .unwrap_or_else(|| Path::new("")),
        );
    let output = command_output_bounded(
        command,
        XRANDR_TIMEOUT,
        MAX_JOURNAL_BYTES as usize,
        "console xrandr query",
    )
    .await?;
    if !output.success {
        return Err("query console xrandr state failed".to_string());
    }
    String::from_utf8(output.stdout).map_err(|_| "console xrandr output is not UTF-8".to_string())
}

#[cfg(target_os = "linux")]
async fn verify_console_protected(
    config: &LinuxDesksideConfig,
    session_gpu_head: &str,
) -> Result<(), String> {
    let states = parse_xrandr_query(&query_xrandr(config).await?)?;
    if !protected_xrandr_states(config, &states) {
        return Err("deskside console xrandr inventory is not fully blank".to_string());
    }
    verify_drm_pins(config, session_gpu_head)?;
    if !query_xset(config).await?.contains("Monitor is Off") {
        return Err("deskside console DPMS state is not off".to_string());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
async fn verify_console_restored(
    config: &LinuxDesksideConfig,
    snapshot: &ConsoleSnapshot,
) -> Result<(), String> {
    let states = parse_xrandr_query(&query_xrandr(config).await?)?;
    for expected in &snapshot.outputs {
        let observed = states
            .iter()
            .find(|state| state.name == expected.name)
            .ok_or_else(|| "restored console output is missing".to_string())?;
        if observed.mode != expected.mode
            || observed.x != expected.x
            || observed.y != expected.y
            || observed.primary != expected.primary
        {
            return Err("restored console output does not match snapshot".to_string());
        }
    }
    if query_xset(config).await?.contains("Monitor is Off") {
        return Err("restored console monitor remains off".to_string());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
async fn query_xset(config: &LinuxDesksideConfig) -> Result<String, String> {
    let mut command = tokio::process::Command::new("/usr/bin/xset");
    command
        .arg("q")
        .env(
            "DISPLAY",
            config.console_display.as_deref().unwrap_or_default(),
        )
        .env(
            "XAUTHORITY",
            config
                .console_xauthority
                .as_deref()
                .unwrap_or_else(|| Path::new("")),
        );
    let output = command_output_bounded(
        command,
        XSET_TIMEOUT,
        MAX_JOURNAL_BYTES as usize,
        "console DPMS query",
    )
    .await?;
    if !output.success {
        return Err("console DPMS query failed".to_string());
    }
    String::from_utf8(output.stdout).map_err(|_| "console DPMS output is not UTF-8".to_string())
}

fn protected_xrandr_states(config: &LinuxDesksideConfig, states: &[OutputSnapshot]) -> bool {
    states.iter().all(|state| state.mode.is_empty())
        && config.outputs.iter().all(|pin| {
            states
                .iter()
                .any(|state| state.name == pin.name && state.mode.is_empty())
        })
}

#[cfg(target_os = "linux")]
fn verify_drm_pins(config: &LinuxDesksideConfig, session_gpu_head: &str) -> Result<(), String> {
    for pin in &config.outputs {
        let connector = find_drm_connector(&pin.name)?;
        if hash_drm_connector(&connector)? != pin.drm_sha256
            || hash_file(&connector.join("edid"))? != pin.edid_sha256
        {
            return refuse(DesksideRefusalReason::ConflictingEvidence);
        }
    }
    for connector in connected_drm_connectors()? {
        let name = connector_name(&connector)?;
        if name != session_gpu_head && !config.outputs.iter().any(|pin| pin.name == name) {
            return refuse(DesksideRefusalReason::ConflictingEvidence);
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
async fn query_dpms(config: &LinuxDesksideConfig) -> Result<bool, String> {
    Ok(query_xset(config).await?.contains("DPMS is Enabled"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RecoveryStage {
    Armed,
    Protected,
    Restoring,
    RestoreFailed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecoveryJournal {
    version: u32,
    stage: RecoveryStage,
    console_display: String,
    console_xauthority: PathBuf,
    snapshot: ConsoleSnapshot,
}

#[cfg(target_os = "linux")]
fn write_journal(path: &Path, journal: &RecoveryJournal) -> Result<(), String> {
    use std::io::Write as _;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

    let payload = serde_json::to_vec(journal)
        .map_err(|error| format!("serialize deskside recovery journal: {error}"))?;
    if payload.len() as u64 > MAX_JOURNAL_BYTES {
        return Err("deskside recovery journal exceeds bound".to_string());
    }
    let parent = path
        .parent()
        .ok_or_else(|| "deskside recovery journal has no parent".to_string())?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("create deskside recovery directory: {error}"))?;
    std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("protect deskside recovery directory: {error}"))?;
    let parent_metadata = std::fs::symlink_metadata(parent)
        .map_err(|error| format!("inspect deskside recovery directory: {error}"))?;
    if !parent_metadata.is_dir()
        || parent_metadata.file_type().is_symlink()
        || parent_metadata.uid() != 0
        || parent_metadata.mode() & 0o077 != 0
    {
        return Err("deskside recovery directory is not root-only".to_string());
    }
    if path.exists() {
        validate_journal_metadata(path)?;
    }
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(&temporary)
        .map_err(|error| format!("create deskside recovery journal: {error}"))?;
    let result = (|| {
        file.write_all(&payload)
            .map_err(|error| format!("write deskside recovery journal: {error}"))?;
        file.sync_all()
            .map_err(|error| format!("sync deskside recovery journal: {error}"))?;
        std::fs::rename(&temporary, path)
            .map_err(|error| format!("publish deskside recovery journal: {error}"))?;
        std::fs::File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| format!("sync deskside recovery directory: {error}"))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

#[cfg(target_os = "linux")]
fn read_journal(path: &Path) -> Result<RecoveryJournal, String> {
    use std::io::Read as _;
    use std::os::unix::fs::OpenOptionsExt;

    validate_journal_metadata(path)?;
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(path)
        .map_err(|error| format!("open deskside recovery journal: {error}"))?;
    let mut payload = Vec::new();
    file.by_ref()
        .take(MAX_JOURNAL_BYTES + 1)
        .read_to_end(&mut payload)
        .map_err(|error| format!("read deskside recovery journal: {error}"))?;
    if payload.len() as u64 > MAX_JOURNAL_BYTES {
        return Err("deskside recovery journal exceeds bound".to_string());
    }
    let journal: RecoveryJournal = serde_json::from_slice(&payload)
        .map_err(|error| format!("parse deskside recovery journal: {error}"))?;
    if !valid_recovery_journal(&journal) {
        return Err("deskside recovery journal is invalid".to_string());
    }
    Ok(journal)
}

fn valid_recovery_journal(journal: &RecoveryJournal) -> bool {
    journal.version == 1
        && valid_display(&journal.console_display)
        && linux_absolute_path(&journal.console_xauthority)
        && (1..=MAX_OUTPUT_PINS).contains(&journal.snapshot.outputs.len())
        && journal.snapshot.outputs.iter().all(|output| {
            !output.name.is_empty()
                && output.name.len() <= 32
                && output
                    .name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
                && valid_mode(&output.mode)
                && (-32_768..=32_768).contains(&output.x)
                && (-32_768..=32_768).contains(&output.y)
        })
}

fn valid_mode(mode: &str) -> bool {
    let Some((width, height)) = mode.split_once('x') else {
        return false;
    };
    mode.len() <= 24
        && width
            .parse::<u32>()
            .is_ok_and(|width| (1..=16_384).contains(&width))
        && height
            .parse::<u32>()
            .is_ok_and(|height| (1..=8_640).contains(&height))
}

#[cfg(target_os = "linux")]
fn validate_journal_metadata(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("inspect deskside recovery journal: {error}"))?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.uid() != 0
        || metadata.permissions().mode() & 0o077 != 0
        || metadata.len() > MAX_JOURNAL_BYTES
    {
        return Err("deskside recovery journal is not a bounded root-only file".to_string());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn update_journal_stage(stage: RecoveryStage) -> Result<(), String> {
    let path = LinuxDesksideConfig::journal_path();
    let mut journal = read_journal(&path)?;
    journal.stage = stage;
    write_journal(&path, &journal)
}

#[cfg(target_os = "linux")]
fn remove_journal(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("remove deskside recovery journal: {error}")),
    }
}

#[cfg(target_os = "linux")]
pub async fn recover_pending_display() -> Result<(), String> {
    let path = LinuxDesksideConfig::journal_path();
    if !path.exists() {
        return Ok(());
    }
    let journal = read_journal(&path)?;
    let config = LinuxDesksideConfig {
        enabled: true,
        firmware_sha256: String::new(),
        console_uid: None,
        console_display: Some(journal.console_display.clone()),
        console_xauthority: Some(journal.console_xauthority.clone()),
        input_devices: Vec::new(),
        outputs: journal
            .snapshot
            .outputs
            .iter()
            .map(|output| PhysicalOutputPin {
                name: output.name.clone(),
                drm_sha256: "0".repeat(HASH_HEX_BYTES),
                edid_sha256: "0".repeat(HASH_HEX_BYTES),
            })
            .collect(),
    };
    let mut guard = ConsoleDisplayGuard {
        runtime: std::sync::Arc::new(NativeDisplayRuntime {
            config,
            session_gpu_head: String::new(),
        }),
        snapshot: journal.snapshot,
        restored: false,
        spawn_cleanup: false,
    };
    guard.restore().await
}

#[cfg(not(target_os = "linux"))]
pub async fn recover_pending_display() -> Result<(), String> {
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LogindSessionFact {
    id: String,
    active: bool,
    remote: bool,
    seat: String,
    uid: u32,
    display: String,
}

fn select_console_session(
    config: &LinuxDesksideConfig,
    streaming_session_id: &str,
    streaming_uid: u32,
    capture_display: &str,
    sessions: &[LogindSessionFact],
) -> Result<LogindSessionFact, String> {
    let streaming = sessions
        .iter()
        .find(|session| session.id == streaming_session_id)
        .ok_or_else(|| DesksideRefusalReason::UnknownEvidence.to_string())?;
    if !streaming.active
        || !streaming.remote
        || streaming.uid != streaming_uid
        || streaming.display != capture_display
    {
        return refuse(DesksideRefusalReason::RemoteEvidence);
    }
    let expected_uid = config
        .console_uid
        .ok_or_else(|| DesksideRefusalReason::MissingEvidence.to_string())?;
    let expected_display = config
        .console_display
        .as_deref()
        .ok_or_else(|| DesksideRefusalReason::MissingEvidence.to_string())?;
    let matches = sessions
        .iter()
        .filter(|session| {
            session.id != streaming_session_id
                && session.active
                && !session.remote
                && session.seat == "seat0"
                && session.uid == expected_uid
                && session.display == expected_display
        })
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [session] => Ok((*session).clone()),
        [] => refuse(DesksideRefusalReason::UnknownEvidence),
        _ => refuse(DesksideRefusalReason::ConflictingEvidence),
    }
}

#[cfg(target_os = "linux")]
async fn discover_console_session(
    config: &LinuxDesksideConfig,
    streaming_session_id: &str,
    streaming_uid: u32,
    capture_display: &str,
) -> Result<LogindSessionFact, String> {
    let sessions = discover_logind_sessions().await?;
    select_console_session(
        config,
        streaming_session_id,
        streaming_uid,
        capture_display,
        &sessions,
    )
}

#[cfg(target_os = "linux")]
async fn discover_logind_sessions() -> Result<Vec<LogindSessionFact>, String> {
    let mut list = tokio::process::Command::new("/usr/bin/loginctl");
    list.args(["list-sessions", "--no-legend", "--no-pager"]);
    let listed =
        command_output_bounded(list, LOGINCTL_TIMEOUT, MAX_LOGINCTL_BYTES, "loginctl list").await?;
    if !listed.success {
        return refuse(DesksideRefusalReason::UnknownEvidence);
    }
    let text = String::from_utf8(listed.stdout)
        .map_err(|_| DesksideRefusalReason::UnknownEvidence.to_string())?;
    let ids = text
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .filter(|id| safe_session_id(id))
        .take(65)
        .map(str::to_string)
        .collect::<Vec<_>>();
    if ids.is_empty() || ids.len() > 64 {
        return refuse(DesksideRefusalReason::UnknownEvidence);
    }
    let mut sessions = Vec::with_capacity(ids.len());
    for id in ids {
        let mut show = tokio::process::Command::new("/usr/bin/loginctl");
        show.args([
            "show-session",
            &id,
            "-p",
            "Active",
            "-p",
            "Remote",
            "-p",
            "Seat",
            "-p",
            "User",
            "-p",
            "Display",
        ]);
        let shown = command_output_bounded(
            show,
            LOGINCTL_TIMEOUT,
            MAX_LOGINCTL_BYTES,
            "loginctl show-session",
        )
        .await?;
        if !shown.success {
            return refuse(DesksideRefusalReason::UnknownEvidence);
        }
        sessions.push(parse_logind_session(
            &id,
            &String::from_utf8(shown.stdout)
                .map_err(|_| DesksideRefusalReason::UnknownEvidence.to_string())?,
        )?);
    }
    Ok(sessions)
}

fn parse_logind_session(id: &str, properties: &str) -> Result<LogindSessionFact, String> {
    let property = |name: &str| {
        properties.lines().find_map(|line| {
            line.strip_prefix(name)
                .and_then(|value| value.strip_prefix('='))
        })
    };
    let boolean = |name: &str| match property(name) {
        Some("yes") => Ok(true),
        Some("no") => Ok(false),
        _ => Err(DesksideRefusalReason::UnknownEvidence.to_string()),
    };
    Ok(LogindSessionFact {
        id: id.to_string(),
        active: boolean("Active")?,
        remote: boolean("Remote")?,
        seat: property("Seat").unwrap_or_default().to_string(),
        uid: property("User")
            .and_then(|value| value.parse().ok())
            .ok_or_else(|| DesksideRefusalReason::UnknownEvidence.to_string())?,
        display: property("Display").unwrap_or_default().to_string(),
    })
}

fn safe_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

#[cfg(target_os = "linux")]
fn validate_console_xauthority(
    config: &LinuxDesksideConfig,
    console_uid: u32,
) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt as _;

    let path = config
        .console_xauthority
        .as_deref()
        .ok_or_else(|| DesksideRefusalReason::MissingEvidence.to_string())?;
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|_| DesksideRefusalReason::UnknownEvidence.to_string())?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.uid() != console_uid
        || metadata.mode() & 0o077 != 0
    {
        return refuse(DesksideRefusalReason::ConflictingEvidence);
    }
    Ok(())
}

fn cpuid_hypervisor_present() -> Option<bool> {
    #[cfg(target_arch = "x86")]
    {
        return Some(std::arch::x86::__cpuid(1).ecx & (1 << 31) != 0);
    }
    #[cfg(target_arch = "x86_64")]
    {
        return Some(std::arch::x86_64::__cpuid(1).ecx & (1 << 31) != 0);
    }
    #[allow(unreachable_code)]
    None
}

#[cfg(target_os = "linux")]
fn validate_bare_metal(config: &LinuxDesksideConfig) -> Result<(), String> {
    let observed = linux_firmware_fingerprint(
        &read_bounded_fact("/sys/class/dmi/id/sys_vendor")?,
        &read_bounded_fact("/sys/class/dmi/id/product_name")?,
        &read_bounded_fact("/sys/class/dmi/id/chassis_vendor")?,
        &read_bounded_fact("/sys/class/dmi/id/chassis_type")?,
    )?;
    validate_bare_metal_facts(
        &config.firmware_sha256,
        cpuid_hypervisor_present(),
        Some(&observed),
    )
}

fn validate_bare_metal_facts(
    pinned_firmware: &str,
    hypervisor_present: Option<bool>,
    observed_firmware: Option<&str>,
) -> Result<(), String> {
    match hypervisor_present {
        Some(false) => {}
        Some(true) => return refuse(DesksideRefusalReason::VirtualEvidence),
        None => return refuse(DesksideRefusalReason::UnknownEvidence),
    }
    match observed_firmware {
        Some(observed) if observed == pinned_firmware => Ok(()),
        Some(_) => refuse(DesksideRefusalReason::ConflictingEvidence),
        None => refuse(DesksideRefusalReason::UnknownEvidence),
    }
}

#[cfg(target_os = "linux")]
fn read_bounded_fact(path: &str) -> Result<String, String> {
    let value = std::fs::read_to_string(path)
        .map_err(|_| DesksideRefusalReason::UnknownEvidence.to_string())?;
    let value = value.trim();
    if value.is_empty() || value.len() > 128 || !value.is_ascii() {
        return refuse(DesksideRefusalReason::UnknownEvidence);
    }
    Ok(value.to_string())
}

fn linux_firmware_fingerprint(
    system_vendor: &str,
    product_name: &str,
    chassis_vendor: &str,
    chassis_type: &str,
) -> Result<String, String> {
    let chassis_type = chassis_type
        .trim()
        .parse::<u8>()
        .map_err(|_| DesksideRefusalReason::UnknownEvidence.to_string())?;
    if chassis_type <= 2
        || invalid_firmware_fact(system_vendor)
        || invalid_firmware_fact(product_name)
        || invalid_firmware_fact(chassis_vendor)
    {
        return refuse(DesksideRefusalReason::VirtualEvidence);
    }
    Ok(sha256_hex(
        format!(
            "{}|{}|{}|{}",
            system_vendor.trim().to_ascii_uppercase(),
            product_name.trim().to_ascii_uppercase(),
            chassis_vendor.trim().to_ascii_uppercase(),
            chassis_type
        )
        .as_bytes(),
    ))
}

fn invalid_firmware_fact(value: &str) -> bool {
    const MARKERS: [&str; 13] = [
        "QEMU",
        "KVM",
        "VMWARE",
        "VIRTUALBOX",
        "XEN",
        "HYPER-V",
        "PARALLELS",
        "BOCHS",
        "TO BE FILLED",
        "DEFAULT STRING",
        "SYSTEM PRODUCT",
        "UNKNOWN",
        "NONE",
    ];
    let value = value.trim().to_ascii_uppercase();
    value.is_empty() || MARKERS.iter().any(|marker| value.contains(marker))
}

#[cfg(target_os = "linux")]
fn connected_drm_connectors() -> Result<Vec<PathBuf>, String> {
    let mut connectors = Vec::new();
    for entry in std::fs::read_dir("/sys/class/drm")
        .map_err(|_| DesksideRefusalReason::UnknownEvidence.to_string())?
    {
        let path = entry
            .map_err(|_| DesksideRefusalReason::UnknownEvidence.to_string())?
            .path();
        if path.join("status").is_file()
            && std::fs::read_to_string(path.join("status"))
                .is_ok_and(|status| status.trim() == "connected")
        {
            connectors.push(path);
        }
    }
    Ok(connectors)
}

#[cfg(target_os = "linux")]
fn find_drm_connector(output: &str) -> Result<PathBuf, String> {
    connected_drm_connectors()?
        .into_iter()
        .find(|path| connector_name(path).is_ok_and(|name| name == output))
        .ok_or_else(|| DesksideRefusalReason::MissingEvidence.to_string())
}

#[cfg(target_os = "linux")]
fn connector_name(path: &Path) -> Result<String, String> {
    let full = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| DesksideRefusalReason::UnknownEvidence.to_string())?;
    let (_, name) = full
        .split_once('-')
        .ok_or_else(|| DesksideRefusalReason::UnknownEvidence.to_string())?;
    Ok(name.to_string())
}

#[cfg(target_os = "linux")]
fn hash_drm_connector(path: &Path) -> Result<String, String> {
    let name = connector_name(path)?;
    let connector_id = std::fs::read_to_string(path.join("connector_id"))
        .map_err(|_| DesksideRefusalReason::UnknownEvidence.to_string())?;
    Ok(sha256_hex(
        format!("{}:{}", name.to_ascii_uppercase(), connector_id.trim()).as_bytes(),
    ))
}

#[cfg(target_os = "linux")]
fn hash_file(path: &Path) -> Result<String, String> {
    let bytes =
        std::fs::read(path).map_err(|_| DesksideRefusalReason::UnknownEvidence.to_string())?;
    if bytes.is_empty() || bytes.len() > 1024 {
        return refuse(DesksideRefusalReason::UnknownEvidence);
    }
    Ok(sha256_hex(&bytes))
}

fn validate_hash(value: &str, field: &str) -> Result<(), String> {
    if value.len() != HASH_HEX_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(format!("{field} must be 64 lowercase hex characters"));
    }
    Ok(())
}

fn valid_display(value: &str) -> bool {
    value
        .strip_prefix(':')
        .and_then(|number| number.parse::<u16>().ok())
        .is_some_and(|number| number <= 99)
}

fn linux_absolute_path(path: &Path) -> bool {
    path.to_str().is_some_and(|value| value.starts_with('/'))
}

fn sha256_hex(value: &[u8]) -> String {
    let digest = Sha256::digest(value);
    let mut encoded = String::with_capacity(HASH_HEX_BYTES);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(encoded, "{byte:02x}");
    }
    encoded
}

fn refuse<T>(reason: DesksideRefusalReason) -> Result<T, String> {
    Err(format!("deskside_refused:{}", reason.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    fn output(name: &str) -> PhysicalOutputPin {
        PhysicalOutputPin {
            name: name.to_string(),
            drm_sha256: sha256_hex(format!("{name}-drm").as_bytes()),
            edid_sha256: sha256_hex(format!("{name}-edid").as_bytes()),
        }
    }

    fn config() -> LinuxDesksideConfig {
        LinuxDesksideConfig {
            enabled: true,
            firmware_sha256: linux_firmware_fingerprint("Dell", "Precision", "Dell", "3")
                .expect("firmware"),
            console_uid: Some(1_000),
            console_display: Some(":0".to_string()),
            console_xauthority: Some(PathBuf::from("/run/arcen/console.Xauthority")),
            input_devices: vec![
                PathBuf::from("/dev/input/by-id/keyboard-event-kbd"),
                PathBuf::from("/dev/input/by-id/mouse-event-mouse"),
            ],
            outputs: vec![output("DP-1")],
        }
    }

    #[test]
    fn configuration_is_disabled_by_default_and_complete_when_enabled() {
        assert!(LinuxDesksideConfig::default()
            .validate(true, true, ":10", "DFP-1")
            .is_ok());
        assert!(config().validate(true, true, ":10", "DFP-1").is_ok());
        assert!(config().validate(false, true, ":10", "DFP-1").is_err());
        assert!(config().validate(true, false, ":10", "DFP-1").is_err());
    }

    #[test]
    fn configuration_refuses_console_capture_overlap_and_incomplete_pins() {
        assert!(config().validate(true, true, ":0", "DFP-1").is_err());
        let mut missing = config();
        missing.input_devices.clear();
        assert!(missing.validate(true, true, ":10", "DFP-1").is_err());
        let mut overlap = config();
        overlap.outputs[0].name = "DFP-1".to_string();
        assert!(overlap.validate(true, true, ":10", "DFP-1").is_err());
    }

    #[test]
    fn output_pin_parser_is_bounded_and_closed() {
        let pin = output("DP-1");
        let encoded = format!("{},{},{}", pin.name, pin.drm_sha256, pin.edid_sha256);
        assert_eq!(PhysicalOutputPin::parse(&encoded), Ok(pin));
        assert!(PhysicalOutputPin::parse("DP-1,abc,def").is_err());
        assert!(PhysicalOutputPin::parse("DP-1,a,b,c").is_err());
    }

    #[test]
    fn xrandr_parser_and_plans_are_bounded_and_capture_neutral() {
        let parsed = parse_xrandr_query(
            "DP-1 connected primary 1920x1080+0+0 normal\n\
             HDMI-1 connected (normal left inverted right x axis y axis)\n",
        )
        .expect("parse");
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].mode, "1920x1080");
        assert!(parsed[0].primary);
        let snapshot = ConsoleSnapshot {
            outputs: vec![parsed[0].clone()],
            dpms_enabled: true,
        };
        let blank = blank_plan(&snapshot);
        assert_eq!(blank.args, ["--output", "DP-1", "--off"]);
        assert!(!blank.args.iter().any(|argument| argument == ":10"));
        let restore = restore_plan(&snapshot);
        assert!(restore
            .args
            .windows(2)
            .any(|pair| pair == ["--mode", "1920x1080"]));
        let mut blank = parsed;
        blank[0].mode.clear();
        assert!(protected_xrandr_states(&config(), &blank));
        blank[1].mode = "1280x720".to_string();
        assert!(!protected_xrandr_states(&config(), &blank));
    }

    #[test]
    fn xrandr_parser_rejects_unbounded_output() {
        assert!(parse_xrandr_query(&"x".repeat(MAX_JOURNAL_BYTES as usize + 1)).is_err());
    }

    #[test]
    fn input_classifier_excludes_only_exact_arcen_virtual_identity() {
        assert_eq!(
            classify_input_device(0x06, 0xA2CE, 0x0001, Some("Arcen Virtual Input"), true),
            InputDeviceDisposition::ArcenVirtual
        );
        assert_eq!(
            classify_input_device(0x06, 1, 2, Some("spoof"), true),
            InputDeviceDisposition::Virtual
        );
        assert_eq!(
            classify_input_device(0x03, 1, 2, Some("keyboard"), true),
            InputDeviceDisposition::Physical
        );
        assert_eq!(
            classify_input_device(0, 1, 2, Some("unknown"), true),
            InputDeviceDisposition::Unknown
        );
    }

    #[test]
    fn recovery_journal_validation_rejects_corrupt_or_unbounded_state() {
        let mut journal = RecoveryJournal {
            version: 1,
            stage: RecoveryStage::Protected,
            console_display: ":0".to_string(),
            console_xauthority: PathBuf::from("/run/arcen/console.Xauthority"),
            snapshot: ConsoleSnapshot {
                outputs: vec![OutputSnapshot {
                    name: "DP-1".to_string(),
                    mode: "1920x1080".to_string(),
                    x: 0,
                    y: 0,
                    primary: true,
                }],
                dpms_enabled: true,
            },
        };
        assert!(valid_recovery_journal(&journal));
        journal.snapshot.outputs[0].mode = "--auto".to_string();
        assert!(!valid_recovery_journal(&journal));
        journal.snapshot.outputs[0].mode = "1920x1080".to_string();
        journal.console_xauthority = PathBuf::from("relative");
        assert!(!valid_recovery_journal(&journal));
    }

    #[test]
    fn selects_one_distinct_local_console_for_remote_streaming_session() {
        let config = config();
        let sessions = vec![
            LogindSessionFact {
                id: "remote".to_string(),
                active: true,
                remote: true,
                seat: "seat0".to_string(),
                uid: 2_000,
                display: ":10".to_string(),
            },
            LogindSessionFact {
                id: "console".to_string(),
                active: true,
                remote: false,
                seat: "seat0".to_string(),
                uid: 1_000,
                display: ":0".to_string(),
            },
        ];
        assert_eq!(
            select_console_session(&config, "remote", 2_000, ":10", &sessions)
                .expect("console")
                .id,
            "console"
        );
        let mut ambiguous = sessions.clone();
        ambiguous.push(LogindSessionFact {
            id: "console-2".to_string(),
            ..sessions[1].clone()
        });
        assert!(select_console_session(&config, "remote", 2_000, ":10", &ambiguous).is_err());
        let mut wrong_remote = sessions;
        wrong_remote[0].remote = false;
        assert!(select_console_session(&config, "remote", 2_000, ":10", &wrong_remote).is_err());
        assert!(parse_logind_session(
            "remote",
            "Active=yes\nRemote=\nSeat=seat0\nUser=2000\nDisplay=:10"
        )
        .is_err());
        assert!(parse_logind_session(
            "remote",
            "Active=yes\nRemote=yes\nSeat=seat0\nUser=2000\nDisplay=:10"
        )
        .is_ok());
    }

    #[test]
    fn bare_metal_firmware_requires_positive_pinned_chassis_facts() {
        let physical =
            linux_firmware_fingerprint("Dell", "Precision 7960", "Dell", "3").expect("physical");
        assert_eq!(physical.len(), HASH_HEX_BYTES);
        assert!(linux_firmware_fingerprint("QEMU", "Virtual Machine", "QEMU", "3").is_err());
        assert!(linux_firmware_fingerprint("Dell", "Precision", "Dell", "2").is_err());
        assert!(linux_firmware_fingerprint("", "Precision", "Dell", "3").is_err());
        assert!(validate_bare_metal_facts(&physical, Some(false), Some(&physical)).is_ok());
        assert!(validate_bare_metal_facts(&physical, Some(true), Some(&physical)).is_err());
        assert!(validate_bare_metal_facts(&physical, None, Some(&physical)).is_err());
        assert!(validate_bare_metal_facts(&physical, Some(false), None).is_err());
    }

    #[derive(Default)]
    struct FakeDisplayRuntime {
        calls: Mutex<Vec<String>>,
        failures: Mutex<HashSet<String>>,
    }

    impl FakeDisplayRuntime {
        fn call(&self, name: &str) -> Result<(), String> {
            self.calls.lock().expect("calls").push(name.to_string());
            if self.failures.lock().expect("failures").contains(name) {
                Err(format!("{name} failed"))
            } else {
                Ok(())
            }
        }

        fn fail(&self, name: &str) {
            self.failures
                .lock()
                .expect("failures")
                .insert(name.to_string());
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().expect("calls").clone()
        }
    }

    impl DisplayRuntime for FakeDisplayRuntime {
        fn write_initial(&self, _journal: &RecoveryJournal) -> Result<(), String> {
            self.call("journal_armed")
        }

        fn mark_stage(&self, stage: RecoveryStage) -> Result<(), String> {
            self.call(match stage {
                RecoveryStage::Armed => "stage_armed",
                RecoveryStage::Protected => "stage_protected",
                RecoveryStage::Restoring => "stage_restoring",
                RecoveryStage::RestoreFailed => "stage_restore_failed",
            })
        }

        fn remove_journal(&self) -> Result<(), String> {
            self.call("journal_remove")
        }

        fn run_xrandr<'a>(&'a self, plan: &'a XrandrPlan) -> DisplayFuture<'a> {
            let name = if plan.args.iter().any(|arg| arg == "--off") {
                "xrandr_blank"
            } else {
                "xrandr_restore"
            };
            Box::pin(async move { self.call(name) })
        }

        fn run_xset(&self, args: &'static [&'static str]) -> DisplayFuture<'_> {
            let name = if args.contains(&"off") {
                "dpms_off"
            } else if args.contains(&"on") {
                "dpms_on"
            } else {
                "dpms_policy"
            };
            Box::pin(async move { self.call(name) })
        }

        fn verify_protected(&self) -> DisplayFuture<'_> {
            Box::pin(async move { self.call("verify_protected") })
        }

        fn verify_restored<'a>(&'a self, _snapshot: &'a ConsoleSnapshot) -> DisplayFuture<'a> {
            Box::pin(async move { self.call("verify_restored") })
        }
    }

    struct FakeInputLease {
        runtime: Arc<FakeDisplayRuntime>,
        complete: bool,
    }

    impl InputLease for FakeInputLease {
        fn is_complete(&self) -> bool {
            self.complete
        }

        fn release(&mut self) -> Result<(), String> {
            self.runtime.call("input_release")
        }
    }

    fn snapshot() -> ConsoleSnapshot {
        ConsoleSnapshot {
            outputs: vec![OutputSnapshot {
                name: "DP-1".to_string(),
                mode: "1920x1080".to_string(),
                x: 0,
                y: 0,
                primary: true,
            }],
            dpms_enabled: true,
        }
    }

    fn physical_evidence() -> PhysicalHostEvidence {
        PhysicalHostEvidence::validate(PhysicalEvidenceSummary {
            runtime_fresh: true,
            host: EvidenceStatus::Positive,
            console_session: EvidenceStatus::Positive,
            local_input: EvidenceStatus::Positive,
            local_displays: EvidenceStatus::Positive,
            active_resources_accounted: EvidenceStatus::Positive,
            capture_separation: EvidenceStatus::Positive,
            input_fingerprint: Some(StateFingerprint::new(b"input").expect("input")),
            display_fingerprint: Some(StateFingerprint::new(b"display").expect("display")),
        })
        .expect("evidence")
    }

    #[tokio::test]
    async fn real_arm_orchestration_orders_input_display_and_cleanup() {
        let runtime = Arc::new(FakeDisplayRuntime::default());
        let acquire_runtime = runtime.clone();
        let mut guard = LinuxDesksideGuard::arm_with_components(
            &config(),
            "remote-session",
            physical_evidence(),
            snapshot(),
            runtime.clone(),
            move || {
                acquire_runtime.call("input_acquire")?;
                Ok(Box::new(FakeInputLease {
                    runtime: acquire_runtime,
                    complete: true,
                }))
            },
        )
        .await
        .expect("arm");
        guard.restore().await.expect("restore");
        let calls = runtime.calls();
        let input_acquire = calls
            .iter()
            .position(|call| call == "input_acquire")
            .expect("input acquire");
        let display_blank = calls
            .iter()
            .position(|call| call == "xrandr_blank")
            .expect("display blank");
        let display_restore = calls
            .iter()
            .position(|call| call == "xrandr_restore")
            .expect("display restore");
        let input_release = calls
            .iter()
            .position(|call| call == "input_release")
            .expect("input release");
        assert!(input_acquire < display_blank);
        assert!(display_restore < input_release);
        assert_eq!(
            guard.protection.state(),
            arcen_session::deskside::DesksideState::Inactive
        );
    }

    #[tokio::test]
    async fn protected_journal_failure_immediately_restores_console() {
        let runtime = Arc::new(FakeDisplayRuntime::default());
        runtime.fail("stage_protected");
        let result = ConsoleDisplayGuard::apply_with_runtime(runtime.clone(), snapshot()).await;
        assert!(result.is_err());
        let calls = runtime.calls();
        assert!(calls.contains(&"xrandr_restore".to_string()));
        assert!(calls.contains(&"dpms_on".to_string()));
        assert!(calls.contains(&"verify_restored".to_string()));
    }

    #[tokio::test]
    async fn armed_journal_failure_mutates_nothing() {
        let runtime = Arc::new(FakeDisplayRuntime::default());
        runtime.fail("journal_armed");
        assert!(
            ConsoleDisplayGuard::apply_with_runtime(runtime.clone(), snapshot())
                .await
                .is_err()
        );
        assert_eq!(runtime.calls(), vec!["journal_armed".to_string()]);
    }

    #[tokio::test]
    async fn restoring_journal_failure_never_short_circuits_unblank() {
        let runtime = Arc::new(FakeDisplayRuntime::default());
        let mut guard = ConsoleDisplayGuard::apply_with_runtime(runtime.clone(), snapshot())
            .await
            .expect("arm");
        runtime.fail("stage_restoring");
        let result = guard.restore().await;
        assert!(result.is_err());
        assert!(guard.restored);
        let calls = runtime.calls();
        assert!(calls.contains(&"xrandr_restore".to_string()));
        assert!(calls.contains(&"dpms_on".to_string()));
        assert!(!calls.contains(&"journal_remove".to_string()));
    }

    #[tokio::test]
    async fn restore_attempts_every_control_and_drop_spawns_retry() {
        let runtime = Arc::new(FakeDisplayRuntime::default());
        let mut guard = ConsoleDisplayGuard::apply_with_runtime(runtime.clone(), snapshot())
            .await
            .expect("arm");
        runtime.fail("xrandr_restore");
        assert!(guard.restore().await.is_err());
        let calls = runtime.calls();
        assert!(calls.contains(&"dpms_on".to_string()));
        assert!(calls.contains(&"verify_restored".to_string()));
        assert!(calls.contains(&"stage_restore_failed".to_string()));
        drop(guard);
        tokio::task::yield_now().await;
        assert!(
            runtime
                .calls()
                .iter()
                .filter(|call| call.as_str() == "xrandr_restore")
                .count()
                >= 2
        );
    }

    #[tokio::test]
    async fn restore_failed_and_remove_journal_errors_remain_observable() {
        let failed = Arc::new(FakeDisplayRuntime::default());
        let mut guard = ConsoleDisplayGuard::apply_with_runtime(failed.clone(), snapshot())
            .await
            .expect("arm");
        failed.fail("xrandr_restore");
        failed.fail("stage_restore_failed");
        let error = guard.restore().await.expect_err("restore failure");
        assert!(error.contains("restore xrandr"));
        assert!(error.contains("journal restore-failed"));
        assert!(failed.calls().contains(&"dpms_on".to_string()));

        let remove = Arc::new(FakeDisplayRuntime::default());
        let mut guard = ConsoleDisplayGuard::apply_with_runtime(remove.clone(), snapshot())
            .await
            .expect("arm");
        remove.fail("journal_remove");
        let error = guard.restore().await.expect_err("remove failure");
        assert!(error.contains("remove restored journal"));
        assert!(guard.restored);
    }

    #[tokio::test]
    async fn bounded_command_kills_and_reaps_hanging_child() {
        let marker =
            std::env::temp_dir().join(format!("arcen-deskside-timeout-{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        #[cfg(windows)]
        let command = {
            let mut command = tokio::process::Command::new("powershell.exe");
            command.args([
                "-NoProfile",
                "-Command",
                &format!(
                    "Start-Sleep -Seconds 5; Set-Content -Path '{}' -Value late",
                    marker.display()
                ),
            ]);
            command
        };
        #[cfg(not(windows))]
        let command = {
            let mut command = tokio::process::Command::new("/bin/sh");
            command.args([
                "-c",
                &format!("sleep 5; printf late > '{}'", marker.display()),
            ]);
            command
        };
        let started = std::time::Instant::now();
        assert!(
            command_status_bounded(command, Duration::from_millis(100), "hanging fake",)
                .await
                .is_err()
        );
        assert!(started.elapsed() < Duration::from_secs(3));
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!marker.exists());
    }
}
