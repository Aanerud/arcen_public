//! Fail-closed Windows deskside evidence and local-input ownership.

use std::collections::HashSet;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread::JoinHandle;
use std::time::Duration;

use arcen_session::deskside::{DesksidePolicy, DesksideRefusalReason, PhysicalHostEvidence};
#[cfg(windows)]
use arcen_session::deskside::{EvidenceStatus, PhysicalEvidenceSummary};
use arcen_session::restore_lease::StateFingerprint;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::display::ResolvedOutput;
#[cfg(windows)]
use crate::gpu_probe::{AdapterKind, HostCapabilityReport};
use crate::windows_session::WindowsSessionIdentity;

const MAX_MONITOR_PINS: usize = 16;
const HASH_HEX_BYTES: usize = 64;
const HOOK_START_TIMEOUT: Duration = Duration::from_secs(5);
const HOOK_CANARY_TIMEOUT: Duration = Duration::from_millis(250);
static INJECTION_MARKERS: OnceLock<InjectionMarkers> = OnceLock::new();
static KEYBOARD_CANARY_HEARTBEAT: AtomicU64 = AtomicU64::new(0);
static MOUSE_CANARY_HEARTBEAT: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy)]
struct InjectionMarkers {
    remote: usize,
    keyboard_canary: usize,
    mouse_canary: usize,
}

/// Disabled-by-default operator configuration.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct DesksideConfig {
    /// Require physical-workstation privacy controls for every session.
    pub enabled: bool,
    /// SHA-256 of normalized positive SMBIOS chassis facts.
    pub firmware_sha256: String,
    /// SHA-256 of the pinned indirect capture target identity.
    pub capture_sha256: String,
    /// Hash-only pins for every expected local physical monitor.
    pub monitors: Vec<PhysicalMonitorPin>,
}

impl DesksideConfig {
    /// Validates complete operator pinning before service readiness.
    pub fn validate(&self) -> Result<(), String> {
        if !self.enabled {
            if self.firmware_sha256.is_empty()
                && self.capture_sha256.is_empty()
                && self.monitors.is_empty()
            {
                return Ok(());
            }
            return Err(
                "platform.desktop.deskside pins must be empty while deskside is disabled"
                    .to_string(),
            );
        }
        validate_hash(&self.firmware_sha256, "firmware_sha256")?;
        validate_hash(&self.capture_sha256, "capture_sha256")?;
        if self.monitors.is_empty() || self.monitors.len() > MAX_MONITOR_PINS {
            return Err(format!(
                "platform.desktop.deskside requires 1..={MAX_MONITOR_PINS} monitor pins"
            ));
        }
        let mut identities = HashSet::with_capacity(self.monitors.len());
        let mut edids = HashSet::with_capacity(self.monitors.len());
        for pin in &self.monitors {
            validate_hash(&pin.identity_sha256, "identity_sha256")?;
            validate_hash(&pin.edid_sha256, "edid_sha256")?;
            if !identities.insert(pin.identity_sha256.as_str())
                || !edids.insert(pin.edid_sha256.as_str())
            {
                return Err(
                    "platform.desktop.deskside monitor identity and EDID hashes must be unique"
                        .to_string(),
                );
            }
        }
        Ok(())
    }

    /// Returns the shared operator policy.
    #[must_use]
    pub const fn policy(&self) -> DesksidePolicy {
        if self.enabled {
            DesksidePolicy::Required
        } else {
            DesksidePolicy::Disabled
        }
    }
}

/// Bounded monitor pin containing no raw device identity or EDID.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct PhysicalMonitorPin {
    /// SHA-256 of the normalized CCD monitor device identity.
    pub identity_sha256: String,
    /// SHA-256 of the CCD EDID manufacturer/product/connector tuple.
    pub edid_sha256: String,
}

/// Fresh pre-arm evidence and hash-only recovery metadata.
pub struct WindowsDesksideEvidence {
    physical: PhysicalHostEvidence,
    recovery: crate::recovery::DesksideRecoveryEntry,
    capture_binding: StateFingerprint,
}

impl WindowsDesksideEvidence {
    /// Returns the validated shared evidence value.
    #[must_use]
    pub const fn physical(&self) -> &PhysicalHostEvidence {
        &self.physical
    }

    /// Returns bounded recovery metadata for the existing display journal.
    #[must_use]
    pub fn recovery(&self) -> crate::recovery::DesksideRecoveryEntry {
        self.recovery.clone()
    }

    /// Returns the hash binding evidence to the selected capture output.
    #[must_use]
    pub const fn capture_binding(&self) -> StateFingerprint {
        self.capture_binding
    }
}

/// Collects fresh console, CCD, DXGI, and operator-pin evidence.
#[cfg(windows)]
pub fn collect_evidence(
    config: &DesksideConfig,
    session: &WindowsSessionIdentity,
    capture: &ResolvedOutput,
) -> Result<WindowsDesksideEvidence, String> {
    config.validate()?;
    if !config.enabled {
        return Err("deskside evidence requested while policy is disabled".to_string());
    }
    let console = crate::logon_activation::active_console_session()?;
    if session.session_id != console {
        return refuse(DesksideRefusalReason::RemoteEvidence);
    }
    if session_protocol(session.session_id)? != 0 {
        return refuse(DesksideRefusalReason::RemoteEvidence);
    }

    let report = crate::gpu_probe::probe()
        .map_err(|_| DesksideRefusalReason::UnknownEvidence.to_string())?;
    if report.topology_error.is_some() {
        return refuse(DesksideRefusalReason::UnknownEvidence);
    }
    let (physical, capture_binding) = collect_evidence_from_report(config, &report, capture)?;
    Ok(WindowsDesksideEvidence {
        recovery: crate::recovery::DesksideRecoveryEntry::armed(
            *physical.input_fingerprint().as_bytes(),
            *physical.display_fingerprint().as_bytes(),
            config.monitors.len(),
        ),
        physical,
        capture_binding,
    })
}

#[cfg(windows)]
fn collect_evidence_from_report(
    config: &DesksideConfig,
    report: &HostCapabilityReport,
    capture: &ResolvedOutput,
) -> Result<(PhysicalHostEvidence, StateFingerprint), String> {
    validate_bare_metal_evidence(
        config,
        report.hypervisor_present,
        report.firmware_sha256.as_deref(),
    )?;
    let facts = output_facts(report, capture);
    let capture_binding = validate_output_inventory(config, &facts)?;
    let input_fingerprint = StateFingerprint::new(b"windows-low-level-keyboard-mouse-hooks-v1")
        .map_err(|error| error.to_string())?;
    let mut display_material = config
        .monitors
        .iter()
        .flat_map(|pin| [pin.identity_sha256.as_bytes(), pin.edid_sha256.as_bytes()])
        .flatten()
        .copied()
        .collect::<Vec<_>>();
    display_material.sort_unstable();
    let display_fingerprint =
        StateFingerprint::new(&display_material).map_err(|error| error.to_string())?;
    let physical = PhysicalHostEvidence::validate(PhysicalEvidenceSummary {
        runtime_fresh: true,
        host: EvidenceStatus::Positive,
        console_session: EvidenceStatus::Positive,
        local_input: EvidenceStatus::Positive,
        local_displays: EvidenceStatus::Positive,
        active_resources_accounted: EvidenceStatus::Positive,
        capture_separation: EvidenceStatus::Positive,
        input_fingerprint: Some(input_fingerprint),
        display_fingerprint: Some(display_fingerprint),
    })
    .map_err(|reason| reason.to_string())?;
    Ok((physical, capture_binding))
}

#[cfg(not(windows))]
pub fn collect_evidence(
    _config: &DesksideConfig,
    _session: &WindowsSessionIdentity,
    _capture: &ResolvedOutput,
) -> Result<WindowsDesksideEvidence, String> {
    Err("Windows deskside evidence is unavailable on this platform".to_string())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AdapterEvidenceKind {
    Hardware,
    KnownNegative,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct OutputFact {
    capture: bool,
    active: bool,
    adapter: AdapterEvidenceKind,
    connector: Option<i32>,
    identity_sha256: Option<String>,
    edid_sha256: Option<String>,
    capture_sha256: Option<String>,
    capture_binding: Option<StateFingerprint>,
}

/// Verifies that every pinned physical output remains inactive and the exact
/// evidence-bound capture output remains active.
#[cfg(windows)]
pub fn verify_protected(
    config: &DesksideConfig,
    selector: &crate::display::OutputSelector,
    expected_capture: StateFingerprint,
) -> Result<(), String> {
    let capture = crate::display::resolve_output_selector(selector)?;
    let report = crate::gpu_probe::probe()
        .map_err(|_| DesksideRefusalReason::UnknownEvidence.to_string())?;
    if report.topology_error.is_some() {
        return refuse(DesksideRefusalReason::UnknownEvidence);
    }
    validate_protected_inventory(config, &output_facts(&report, &capture), expected_capture)
}

#[cfg(not(windows))]
pub fn verify_protected(
    _config: &DesksideConfig,
    _selector: &crate::display::OutputSelector,
    _expected_capture: StateFingerprint,
) -> Result<(), String> {
    Err("Windows deskside verification is unavailable on this platform".to_string())
}

#[cfg(windows)]
fn output_facts(report: &HostCapabilityReport, capture: &ResolvedOutput) -> Vec<OutputFact> {
    report
        .adapters
        .iter()
        .flat_map(|adapter| {
            adapter.outputs.iter().map(move |output| {
                let capture_output = adapter
                    .description
                    .eq_ignore_ascii_case(&capture.adapter_name)
                    && output.adapter_output_index == capture.adapter_output_index;
                let identity_sha256 = output
                    .monitor_device_path
                    .as_deref()
                    .map(normalized_identity_hash);
                let edid_sha256 = match (
                    output.edid_manufacture_id,
                    output.edid_product_code_id,
                    output.connector_instance,
                ) {
                    (Some(manufacturer), Some(product), Some(connector))
                        if manufacturer != 0 && product != 0 =>
                    {
                        Some(edid_tuple_hash(manufacturer, product, connector))
                    }
                    _ => None,
                };
                OutputFact {
                    capture: capture_output,
                    active: output.attached_to_desktop
                        && output.ccd_active == Some(true)
                        && output.target_available == Some(true),
                    adapter: if adapter.kind == AdapterKind::Hardware {
                        AdapterEvidenceKind::Hardware
                    } else {
                        AdapterEvidenceKind::KnownNegative
                    },
                    connector: output.output_technology,
                    identity_sha256,
                    edid_sha256,
                    capture_sha256: output.deskside_capture_sha256.clone(),
                    capture_binding: output
                        .deskside_capture_sha256
                        .as_deref()
                        .and_then(|pin| runtime_capture_binding(&adapter.session_luid, pin).ok()),
                }
            })
        })
        .collect()
}

fn validate_output_inventory(
    config: &DesksideConfig,
    facts: &[OutputFact],
) -> Result<StateFingerprint, String> {
    let mut capture_found = false;
    let mut capture_binding = None;
    let mut matched = vec![false; config.monitors.len()];
    for fact in facts.iter().filter(|fact| fact.active) {
        if fact.capture {
            capture_found = true;
            if fact.adapter != AdapterEvidenceKind::Hardware
                || fact.connector.is_none()
                || !fact.connector.is_some_and(approved_capture_connector)
                || fact.capture_sha256.as_deref() != Some(config.capture_sha256.as_str())
                || fact.capture_binding.is_none()
            {
                return Err(
                    "capture output is not provably distinct from physical panels".to_string(),
                );
            }
            capture_binding = fact.capture_binding;

            continue;
        }
        if fact.adapter != AdapterEvidenceKind::Hardware {
            return refuse(DesksideRefusalReason::VirtualEvidence);
        }
        if !fact.connector.is_some_and(physical_connector) {
            return refuse(DesksideRefusalReason::UnknownEvidence);
        }
        let (Some(identity), Some(edid)) = (&fact.identity_sha256, &fact.edid_sha256) else {
            return refuse(DesksideRefusalReason::UnknownEvidence);
        };
        let Some(index) = config
            .monitors
            .iter()
            .position(|pin| pin.identity_sha256 == *identity && pin.edid_sha256 == *edid)
        else {
            return refuse(DesksideRefusalReason::ConflictingEvidence);
        };
        if matched[index] {
            return refuse(DesksideRefusalReason::ConflictingEvidence);
        }
        matched[index] = true;
    }
    if !capture_found {
        return refuse(DesksideRefusalReason::UnknownEvidence);
    }
    if matched.iter().any(|matched| !matched) {
        return refuse(DesksideRefusalReason::MissingEvidence);
    }
    capture_binding.ok_or_else(|| DesksideRefusalReason::UnknownEvidence.to_string())
}

fn validate_protected_inventory(
    config: &DesksideConfig,
    facts: &[OutputFact],
    expected_capture: StateFingerprint,
) -> Result<(), String> {
    let capture = facts
        .iter()
        .filter(|fact| fact.capture && fact.active)
        .collect::<Vec<_>>();
    if capture.len() != 1
        || capture[0].adapter != AdapterEvidenceKind::Hardware
        || capture[0].connector.is_none()
        || !capture[0].connector.is_some_and(approved_capture_connector)
        || capture[0].capture_sha256.as_deref() != Some(config.capture_sha256.as_str())
        || capture[0].capture_binding != Some(expected_capture)
    {
        return refuse(DesksideRefusalReason::ConflictingEvidence);
    }
    if facts.iter().any(|fact| fact.active && !fact.capture) {
        return refuse(DesksideRefusalReason::ConflictingEvidence);
    }
    for pin in &config.monitors {
        let matches = facts
            .iter()
            .filter(|fact| {
                fact.adapter == AdapterEvidenceKind::Hardware
                    && fact.connector.is_some_and(physical_connector)
                    && fact.identity_sha256.as_ref() == Some(&pin.identity_sha256)
                    && fact.edid_sha256.as_ref() == Some(&pin.edid_sha256)
            })
            .collect::<Vec<_>>();
        if matches.len() != 1 || matches[0].active {
            return refuse(DesksideRefusalReason::ConflictingEvidence);
        }
    }
    Ok(())
}

fn physical_connector(value: i32) -> bool {
    matches!(value, 0..=14)
}

fn approved_capture_connector(value: i32) -> bool {
    value == 16
}

fn validate_hash(value: &str, field: &str) -> Result<(), String> {
    if value.len() != HASH_HEX_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(format!(
            "platform.desktop.deskside.monitors.{field} must be 64 lowercase hex characters"
        ));
    }
    Ok(())
}

pub(crate) fn normalized_identity_hash(value: &str) -> String {
    sha256_hex(value.trim().to_ascii_uppercase().as_bytes())
}

pub(crate) fn edid_tuple_hash(manufacturer: u16, product: u16, connector: u32) -> String {
    let mut normalized = [0_u8; 8];
    normalized[..2].copy_from_slice(&manufacturer.to_be_bytes());
    normalized[2..4].copy_from_slice(&product.to_be_bytes());
    normalized[4..].copy_from_slice(&connector.to_be_bytes());
    sha256_hex(&normalized)
}

pub(crate) fn capture_pin_hash(
    adapter_stable_id: &str,
    output_index: u32,
    device_name: &str,
    monitor_device_path: &str,
    output_technology: i32,
) -> Result<String, String> {
    if output_technology != 16
        || adapter_stable_id.is_empty()
        || device_name.is_empty()
        || monitor_device_path.is_empty()
    {
        return Err("capture target does not have complete indirect identity".to_string());
    }
    Ok(sha256_hex(
        format!(
            "{}|{}|{}|{}|{}",
            adapter_stable_id.trim().to_ascii_uppercase(),
            output_index,
            device_name.trim().to_ascii_uppercase(),
            monitor_device_path.trim().to_ascii_uppercase(),
            output_technology
        )
        .as_bytes(),
    ))
}

fn runtime_capture_binding(
    session_luid: &str,
    capture_pin_sha256: &str,
) -> Result<StateFingerprint, String> {
    let normalized = format!("{session_luid}|{capture_pin_sha256}");
    StateFingerprint::new(normalized.as_bytes()).map_err(|error| error.to_string())
}

pub(crate) fn cpuid_hypervisor_present() -> Option<bool> {
    #[cfg(target_arch = "x86")]
    {
        let leaf = std::arch::x86::__cpuid(1);
        return Some(leaf.ecx & (1 << 31) != 0);
    }
    #[cfg(target_arch = "x86_64")]
    {
        let leaf = std::arch::x86_64::__cpuid(1);
        return Some(leaf.ecx & (1 << 31) != 0);
    }
    #[allow(unreachable_code)]
    None
}

fn validate_bare_metal_evidence(
    config: &DesksideConfig,
    hypervisor_present: Option<bool>,
    firmware_sha256: Option<&str>,
) -> Result<(), String> {
    match hypervisor_present {
        Some(false) => {}
        Some(true) => return refuse(DesksideRefusalReason::VirtualEvidence),
        None => return refuse(DesksideRefusalReason::UnknownEvidence),
    }
    match firmware_sha256 {
        Some(observed) if observed == config.firmware_sha256 => Ok(()),
        Some(_) => refuse(DesksideRefusalReason::ConflictingEvidence),
        None => refuse(DesksideRefusalReason::UnknownEvidence),
    }
}

#[cfg(windows)]
pub(crate) fn positive_firmware_fingerprint() -> Result<String, String> {
    use windows::Win32::System::SystemInformation::{
        GetSystemFirmwareTable, FIRMWARE_TABLE_PROVIDER,
    };

    const RSMB: FIRMWARE_TABLE_PROVIDER = FIRMWARE_TABLE_PROVIDER(0x5253_4d42);
    // SAFETY: the size query supplies no output buffer.
    let size = unsafe { GetSystemFirmwareTable(RSMB, 0, None) };
    if !(16..=1024 * 1024).contains(&size) {
        return Err("SMBIOS table size is unavailable or outside bounds".to_string());
    }
    let mut bytes = vec![0_u8; size as usize];
    // SAFETY: bytes is a writable allocation of the size returned above.
    let written = unsafe { GetSystemFirmwareTable(RSMB, 0, Some(&mut bytes)) };
    if written != size {
        return Err("SMBIOS table changed during collection".to_string());
    }
    firmware_fingerprint_from_raw_smbios(&bytes)
}

#[cfg(not(windows))]
pub(crate) fn positive_firmware_fingerprint() -> Result<String, String> {
    Err("Windows SMBIOS collection is unavailable".to_string())
}

fn firmware_fingerprint_from_raw_smbios(raw: &[u8]) -> Result<String, String> {
    if raw.len() < 8 {
        return Err("raw SMBIOS header is truncated".to_string());
    }
    let table_len = u32::from_le_bytes([raw[4], raw[5], raw[6], raw[7]]) as usize;
    if table_len == 0 || table_len > raw.len().saturating_sub(8) {
        return Err("raw SMBIOS table length is invalid".to_string());
    }
    let table = &raw[8..8 + table_len];
    let mut offset = 0;
    let mut system = None;
    let mut chassis = None;
    while offset + 4 <= table.len() {
        let kind = table[offset];
        let length = table[offset + 1] as usize;
        if length < 4 || offset + length > table.len() {
            return Err("SMBIOS structure length is invalid".to_string());
        }
        let strings_start = offset + length;
        let strings_end = find_double_nul(table, strings_start)
            .ok_or_else(|| "SMBIOS string-set is unterminated".to_string())?;
        let strings = &table[strings_start..strings_end];
        if kind == 1 && length >= 8 {
            let manufacturer = smbios_string(strings, table[offset + 4])?;
            let product = smbios_string(strings, table[offset + 5])?;
            system = Some((manufacturer, product));
        } else if kind == 3 && length >= 6 {
            let manufacturer = smbios_string(strings, table[offset + 4])?;
            let chassis_type = table[offset + 5] & 0x7f;
            chassis = Some((manufacturer, chassis_type));
        }
        offset = strings_end + 2;
        if kind == 127 {
            break;
        }
    }
    let (system_manufacturer, product) =
        system.ok_or_else(|| "SMBIOS system identity is missing".to_string())?;
    let (chassis_manufacturer, chassis_type) =
        chassis.ok_or_else(|| "SMBIOS chassis identity is missing".to_string())?;
    if chassis_type <= 2
        || invalid_firmware_fact(&system_manufacturer)
        || invalid_firmware_fact(&product)
        || invalid_firmware_fact(&chassis_manufacturer)
    {
        return Err("SMBIOS physical chassis evidence is unknown or virtual".to_string());
    }
    let normalized = format!(
        "{}|{}|{}|{}|{}.{}",
        system_manufacturer.trim().to_ascii_uppercase(),
        product.trim().to_ascii_uppercase(),
        chassis_manufacturer.trim().to_ascii_uppercase(),
        chassis_type,
        raw[1],
        raw[2]
    );
    Ok(sha256_hex(normalized.as_bytes()))
}

fn find_double_nul(bytes: &[u8], start: usize) -> Option<usize> {
    (start..bytes.len().saturating_sub(1))
        .find(|index| bytes[*index] == 0 && bytes[*index + 1] == 0)
}

fn smbios_string(strings: &[u8], index: u8) -> Result<String, String> {
    if index == 0 {
        return Err("required SMBIOS string is absent".to_string());
    }
    strings
        .split(|byte| *byte == 0)
        .nth(index as usize - 1)
        .filter(|value| !value.is_empty() && value.len() <= 128 && value.is_ascii())
        .map(|value| String::from_utf8_lossy(value).into_owned())
        .ok_or_else(|| "required SMBIOS string is invalid".to_string())
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
    let upper = value.trim().to_ascii_uppercase();
    upper.is_empty() || MARKERS.iter().any(|marker| upper.contains(marker))
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

#[cfg(windows)]
fn session_protocol(session_id: u32) -> Result<u16, String> {
    use windows::core::PWSTR;
    use windows::Win32::System::RemoteDesktop::{
        WTSClientProtocolType, WTSFreeMemory, WTSQuerySessionInformationW,
        WTS_CURRENT_SERVER_HANDLE,
    };

    let mut buffer = PWSTR::null();
    let mut bytes = 0_u32;
    // SAFETY: output pointers are valid and WTS owns the returned allocation.
    unsafe {
        WTSQuerySessionInformationW(
            WTS_CURRENT_SERVER_HANDLE,
            session_id,
            WTSClientProtocolType,
            &mut buffer,
            &mut bytes,
        )
    }
    .map_err(|_| DesksideRefusalReason::UnknownEvidence.to_string())?;
    if buffer.is_null() || bytes < std::mem::size_of::<u16>() as u32 {
        if !buffer.is_null() {
            // SAFETY: WTS allocated the returned buffer.
            unsafe { WTSFreeMemory(buffer.0.cast()) };
        }
        return refuse(DesksideRefusalReason::UnknownEvidence);
    }
    // SAFETY: WTS reported at least one u16.
    let protocol = unsafe { *buffer.as_ptr().cast::<u16>() };
    // SAFETY: WTS allocated the returned buffer and it is freed once.
    unsafe { WTSFreeMemory(buffer.0.cast()) };
    Ok(protocol)
}

/// Returns the process-random marker attached to Arcen `SendInput` events.
pub(crate) fn injection_marker() -> usize {
    injection_markers().remote
}

fn injection_markers() -> InjectionMarkers {
    *INJECTION_MARKERS.get_or_init(|| {
        let mut seed = [0_u8; 32];
        if getrandom::getrandom(&mut seed).is_err() {
            return InjectionMarkers {
                remote: 0,
                keyboard_canary: 0,
                mouse_canary: 0,
            };
        }
        InjectionMarkers {
            remote: marker_from_seed(&seed, b"remote"),
            keyboard_canary: marker_from_seed(&seed, b"keyboard-canary"),
            mouse_canary: marker_from_seed(&seed, b"mouse-canary"),
        }
    })
}

fn marker_from_seed(seed: &[u8; 32], label: &[u8]) -> usize {
    let mut hasher = Sha256::new();
    hasher.update(seed);
    hasher.update(label);
    let digest = hasher.finalize();
    let mut bytes = [0_u8; std::mem::size_of::<usize>()];
    let length = bytes.len();
    bytes.copy_from_slice(&digest[..length]);
    usize::from_ne_bytes(bytes).max(1)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HookDecision {
    Swallow,
    PassRemote,
    KeyboardCanary,
    MouseCanary,
}

const fn keyboard_hook_decision(
    flags: u32,
    extra_info: usize,
    markers: InjectionMarkers,
) -> HookDecision {
    const INJECTED: u32 = 0x10;
    const LOWER_INTEGRITY_INJECTED: u32 = 0x02;
    if flags & (INJECTED | LOWER_INTEGRITY_INJECTED) == 0 {
        HookDecision::Swallow
    } else if markers.keyboard_canary != 0 && extra_info == markers.keyboard_canary {
        HookDecision::KeyboardCanary
    } else if markers.remote != 0 && extra_info == markers.remote {
        HookDecision::PassRemote
    } else {
        HookDecision::Swallow
    }
}

const fn mouse_hook_decision(
    flags: u32,
    extra_info: usize,
    markers: InjectionMarkers,
) -> HookDecision {
    const INJECTED: u32 = 0x01;
    const LOWER_INTEGRITY_INJECTED: u32 = 0x02;
    if flags & (INJECTED | LOWER_INTEGRITY_INJECTED) == 0 {
        HookDecision::Swallow
    } else if markers.mouse_canary != 0 && extra_info == markers.mouse_canary {
        HookDecision::MouseCanary
    } else if markers.remote != 0 && extra_info == markers.remote {
        HookDecision::PassRemote
    } else {
        HookDecision::Swallow
    }
}

/// Returns true when a low-level keyboard event must be swallowed.
#[must_use]
#[cfg(test)]
pub const fn swallow_keyboard_event(flags: u32, extra_info: usize, marker: usize) -> bool {
    !matches!(
        keyboard_hook_decision(
            flags,
            extra_info,
            InjectionMarkers {
                remote: marker,
                keyboard_canary: 0,
                mouse_canary: 0,
            },
        ),
        HookDecision::PassRemote
    )
}

/// Returns true when a low-level mouse event must be swallowed.
#[must_use]
#[cfg(test)]
pub const fn swallow_mouse_event(flags: u32, extra_info: usize, marker: usize) -> bool {
    !matches!(
        mouse_hook_decision(
            flags,
            extra_info,
            InjectionMarkers {
                remote: marker,
                keyboard_canary: 0,
                mouse_canary: 0,
            },
        ),
        HookDecision::PassRemote
    )
}

#[derive(Clone)]
pub struct HookProof {
    active: Arc<AtomicBool>,
}

impl HookProof {
    #[cfg(windows)]
    pub fn probe(&self) -> Result<(), String> {
        self.probe_with_sender(InputHookGuard::send_hook_canaries, HOOK_CANARY_TIMEOUT)
    }

    #[cfg(windows)]
    fn probe_with_sender<F>(&self, send: F, timeout: Duration) -> Result<(), String>
    where
        F: FnOnce() -> Result<(), String>,
    {
        if !self.active.load(Ordering::Acquire) {
            return Err("deskside hook thread exited".to_string());
        }
        let keyboard_before = KEYBOARD_CANARY_HEARTBEAT.load(Ordering::Acquire);
        let mouse_before = MOUSE_CANARY_HEARTBEAT.load(Ordering::Acquire);
        send()?;
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if Self::canaries_advanced(
                keyboard_before,
                mouse_before,
                KEYBOARD_CANARY_HEARTBEAT.load(Ordering::Acquire),
                MOUSE_CANARY_HEARTBEAT.load(Ordering::Acquire),
            ) {
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                return Err("deskside hook canary deadline expired".to_string());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    const fn canaries_advanced(
        keyboard_before: u64,
        mouse_before: u64,
        keyboard_after: u64,
        mouse_after: u64,
    ) -> bool {
        keyboard_after > keyboard_before && mouse_after > mouse_before
    }

    #[cfg(not(windows))]
    pub fn probe(&self) -> Result<(), String> {
        Err("Windows hook proof is unavailable on this platform".to_string())
    }
}

/// Process-owned low-level hooks running on a dedicated native message thread.
pub struct InputHookGuard {
    active: Arc<AtomicBool>,
    shutdown_requested: Arc<AtomicBool>,
    thread_id: u32,
    join: Option<JoinHandle<Result<(), String>>>,
}

impl std::fmt::Debug for InputHookGuard {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("InputHookGuard")
            .field("active", &self.active.load(Ordering::Acquire))
            .field("thread_id", &self.thread_id)
            .finish_non_exhaustive()
    }
}

impl InputHookGuard {
    /// Installs keyboard and mouse hooks with a bounded startup handshake.
    #[cfg(windows)]
    pub fn install() -> Result<Self, String> {
        let markers = injection_markers();
        if markers.remote == 0
            || markers.keyboard_canary == 0
            || markers.mouse_canary == 0
            || markers.remote == markers.keyboard_canary
            || markers.remote == markers.mouse_canary
            || markers.keyboard_canary == markers.mouse_canary
        {
            return Err("could not create distinct deskside injection markers".to_string());
        }
        let active = Arc::new(AtomicBool::new(false));
        let shutdown_requested = Arc::new(AtomicBool::new(false));
        let thread_active = Arc::clone(&active);
        let thread_shutdown = Arc::clone(&shutdown_requested);
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let join = std::thread::Builder::new()
            .name("arcen-deskside-hooks".to_string())
            .spawn(move || hook_thread(thread_active, thread_shutdown, ready_tx))
            .map_err(|error| format!("spawn deskside hook thread: {error}"))?;
        let thread_id = match ready_rx.recv_timeout(HOOK_START_TIMEOUT) {
            Ok(Ok(thread_id)) => thread_id,
            Ok(Err(error)) => {
                let _ = join.join();
                return Err(error);
            }
            Err(error) => {
                return Err(format!(
                    "deskside hook thread did not become ready within {}ms: {error}",
                    HOOK_START_TIMEOUT.as_millis()
                ));
            }
        };
        Ok(Self {
            active,
            shutdown_requested,
            thread_id,
            join: Some(join),
        })
    }

    #[cfg(not(windows))]
    pub fn install() -> Result<Self, String> {
        Err("Windows deskside hooks are unavailable on this platform".to_string())
    }

    /// Verifies that the hook message thread remains active.
    pub fn verify(&self) -> Result<(), String> {
        self.proof().probe()
    }

    /// Returns a continuous proof handle for attachment supervision.
    #[must_use]
    pub fn proof(&self) -> HookProof {
        HookProof {
            active: Arc::clone(&self.active),
        }
    }

    #[cfg(windows)]
    fn send_hook_canaries() -> Result<(), String> {
        use windows::Win32::UI::Input::KeyboardAndMouse::{
            SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_KEYUP,
            MOUSEEVENTF_MOVE, MOUSEINPUT, VIRTUAL_KEY,
        };

        let markers = injection_markers();
        if markers.keyboard_canary == 0 || markers.mouse_canary == 0 {
            return Err("deskside hook canary markers are unavailable".to_string());
        }
        let inputs = [
            INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        // VK_NONAME is inert if a removed hook ever misses the canary.
                        wVk: VIRTUAL_KEY(0xfc),
                        wScan: 0,
                        dwFlags: Default::default(),
                        time: 0,
                        dwExtraInfo: markers.keyboard_canary,
                    },
                },
            },
            INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: VIRTUAL_KEY(0xfc),
                        wScan: 0,
                        dwFlags: KEYEVENTF_KEYUP,
                        time: 0,
                        dwExtraInfo: markers.keyboard_canary,
                    },
                },
            },
            INPUT {
                r#type: INPUT_MOUSE,
                Anonymous: INPUT_0 {
                    mi: MOUSEINPUT {
                        dx: 0,
                        dy: 0,
                        mouseData: 0,
                        dwFlags: MOUSEEVENTF_MOVE,
                        time: 0,
                        dwExtraInfo: markers.mouse_canary,
                    },
                },
            },
        ];
        // SAFETY: inputs is a valid initialized slice and cbsize matches INPUT.
        let sent = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
        if sent as usize == inputs.len() {
            Ok(())
        } else {
            Err(format!(
                "deskside hook canary SendInput inserted {sent} of {} events",
                inputs.len()
            ))
        }
    }

    /// Releases hooks and joins their native message thread.
    pub fn shutdown(&mut self) -> Result<(), String> {
        let mut shutdown_error = None;
        #[cfg(windows)]
        if self.thread_id != 0 {
            use windows::Win32::Foundation::{LPARAM, WPARAM};
            use windows::Win32::UI::WindowsAndMessaging::{PostThreadMessageW, WM_QUIT};
            self.shutdown_requested.store(true, Ordering::Release);
            // SAFETY: thread_id belongs to the live hook thread and WM_QUIT has no pointers.
            if let Err(error) =
                unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) }
            {
                shutdown_error = Some(format!("post deskside hook shutdown: {error}"));
            }
            self.thread_id = 0;
        }
        if let Some(join) = self.join.take() {
            match join.join() {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    shutdown_error.get_or_insert(error);
                }
                Err(_) => {
                    shutdown_error
                        .get_or_insert_with(|| "deskside hook thread panicked".to_string());
                }
            };
        }
        self.active.store(false, Ordering::Release);
        shutdown_error.map_or(Ok(()), Err)
    }
}

impl Drop for InputHookGuard {
    fn drop(&mut self) {
        if let Err(error) = self.shutdown() {
            tracing::error!(target: crate::logging::INPUT, %error, "deskside hook cleanup failed");
        }
    }
}

#[cfg(windows)]
fn hook_thread(
    active: Arc<AtomicBool>,
    shutdown_requested: Arc<AtomicBool>,
    ready: std::sync::mpsc::SyncSender<Result<u32, String>>,
) -> Result<(), String> {
    use windows::Win32::Foundation::{HINSTANCE, HWND};
    use windows::Win32::System::Threading::GetCurrentThreadId;
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, GetMessageW, PeekMessageW, SetWindowsHookExW, TranslateMessage,
        UnhookWindowsHookEx, HHOOK, MSG, PM_NOREMOVE, WH_KEYBOARD_LL, WH_MOUSE_LL,
    };

    struct Hooks {
        keyboard: HHOOK,
        mouse: HHOOK,
    }
    impl Drop for Hooks {
        fn drop(&mut self) {
            // SAFETY: each handle is owned by this thread and unhooked once.
            unsafe {
                let _ = UnhookWindowsHookEx(self.mouse);
                let _ = UnhookWindowsHookEx(self.keyboard);
            }
        }
    }

    let mut message = MSG::default();
    // SAFETY: this creates the current thread's message queue without consuming messages.
    unsafe {
        let _ = PeekMessageW(&mut message, HWND::default(), 0, 0, PM_NOREMOVE);
    }
    // SAFETY: callbacks have the required ABI and remain valid for the process lifetime.
    let keyboard =
        unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook), HINSTANCE::default(), 0) }
            .map_err(|error| format!("install WH_KEYBOARD_LL: {error}"))?;
    // SAFETY: callback has the required ABI and remains valid for the process lifetime.
    let mouse = match unsafe {
        SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), HINSTANCE::default(), 0)
    } {
        Ok(mouse) => mouse,
        Err(error) => {
            // SAFETY: keyboard is uniquely owned and has not been unhooked.
            unsafe {
                let _ = UnhookWindowsHookEx(keyboard);
            }
            let error = format!("install WH_MOUSE_LL: {error}");
            let _ = ready.send(Err(error.clone()));
            return Err(error);
        }
    };
    let _hooks = Hooks { keyboard, mouse };
    active.store(true, Ordering::Release);
    // SAFETY: this returns the numeric identifier of the calling thread.
    if ready.send(Ok(unsafe { GetCurrentThreadId() })).is_err() {
        active.store(false, Ordering::Release);
        return Err("deskside hook startup receiver closed".to_string());
    }

    loop {
        // SAFETY: message is writable and this thread owns its message queue.
        let status = unsafe { GetMessageW(&mut message, HWND::default(), 0, 0) };
        if status.0 == 0 {
            if shutdown_requested.load(Ordering::Acquire) {
                break;
            }
            continue;
        }
        if status.0 == -1 {
            active.store(false, Ordering::Release);
            return Err(format!(
                "deskside hook message pump failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        // SAFETY: GetMessageW initialized this message.
        unsafe {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
    active.store(false, Ordering::Release);
    Ok(())
}

#[cfg(windows)]
/// Windows low-level keyboard callback.
///
/// # Safety
///
/// Windows must invoke this only with the documented hook ABI. For
/// `HC_ACTION`, `lparam` must point to a live, aligned `KBDLLHOOKSTRUCT` for the
/// duration of the call.
unsafe extern "system" fn keyboard_hook(
    code: i32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    use windows::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, HC_ACTION, HHOOK, KBDLLHOOKSTRUCT,
    };

    if code == HC_ACTION as i32 {
        // SAFETY: Windows supplies a valid KBDLLHOOKSTRUCT for HC_ACTION.
        let event = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        match keyboard_hook_decision(event.flags.0, event.dwExtraInfo, injection_markers()) {
            HookDecision::PassRemote => {}
            HookDecision::KeyboardCanary => {
                KEYBOARD_CANARY_HEARTBEAT.fetch_add(1, Ordering::Release);
                return windows::Win32::Foundation::LRESULT(1);
            }
            HookDecision::Swallow | HookDecision::MouseCanary => {
                return windows::Win32::Foundation::LRESULT(1);
            }
        }
    }
    // SAFETY: forwarding with the original hook parameters is required by the hook contract.
    unsafe { CallNextHookEx(HHOOK::default(), code, wparam, lparam) }
}

#[cfg(windows)]
/// Windows low-level mouse callback.
///
/// # Safety
///
/// Windows must invoke this only with the documented hook ABI. For
/// `HC_ACTION`, `lparam` must point to a live, aligned `MSLLHOOKSTRUCT` for the
/// duration of the call.
unsafe extern "system" fn mouse_hook(
    code: i32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    use windows::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, HC_ACTION, HHOOK, MSLLHOOKSTRUCT,
    };

    if code == HC_ACTION as i32 {
        // SAFETY: Windows supplies a valid MSLLHOOKSTRUCT for HC_ACTION.
        let event = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
        match mouse_hook_decision(event.flags, event.dwExtraInfo, injection_markers()) {
            HookDecision::PassRemote => {}
            HookDecision::MouseCanary => {
                MOUSE_CANARY_HEARTBEAT.fetch_add(1, Ordering::Release);
                return windows::Win32::Foundation::LRESULT(1);
            }
            HookDecision::Swallow | HookDecision::KeyboardCanary => {
                return windows::Win32::Foundation::LRESULT(1);
            }
        }
    }
    // SAFETY: forwarding with the original hook parameters is required by the hook contract.
    unsafe { CallNextHookEx(HHOOK::default(), code, wparam, lparam) }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(windows)]
    use super::{collect_evidence_from_report, output_facts};
    #[cfg(windows)]
    use crate::gpu_probe::HostCapabilityReport;

    fn pin(identity: &str, edid: &str) -> PhysicalMonitorPin {
        PhysicalMonitorPin {
            identity_sha256: normalized_identity_hash(identity),
            edid_sha256: edid_tuple_hash(u16::from_be_bytes([edid.as_bytes()[0], 1]), 2, 3),
        }
    }

    #[test]
    fn disabled_is_default_and_rejects_latent_pins() {
        let disabled = DesksideConfig::default();
        assert!(!disabled.enabled);
        assert!(disabled.validate().is_ok());
        let configured = DesksideConfig {
            monitors: vec![pin("monitor-a", "a")],
            ..DesksideConfig::default()
        };
        assert!(configured.validate().is_err());
    }

    #[test]
    fn enabled_requires_complete_unique_hash_pins() {
        let valid = DesksideConfig {
            enabled: true,
            firmware_sha256: "1".repeat(HASH_HEX_BYTES),
            capture_sha256: "2".repeat(HASH_HEX_BYTES),
            monitors: vec![pin("monitor-a", "a"), pin("monitor-b", "b")],
        };
        assert!(valid.validate().is_ok());
        let duplicate = DesksideConfig {
            enabled: true,
            firmware_sha256: "1".repeat(HASH_HEX_BYTES),
            capture_sha256: "2".repeat(HASH_HEX_BYTES),
            monitors: vec![pin("monitor-a", "a"), pin("monitor-a", "b")],
        };
        assert!(duplicate.validate().is_err());
        let malformed = DesksideConfig {
            enabled: true,
            firmware_sha256: "1".repeat(HASH_HEX_BYTES),
            capture_sha256: "2".repeat(HASH_HEX_BYTES),
            monitors: vec![PhysicalMonitorPin {
                identity_sha256: "ABC".to_string(),
                edid_sha256: "0".repeat(HASH_HEX_BYTES),
            }],
        };
        assert!(malformed.validate().is_err());
    }

    #[test]
    fn hook_predicates_pass_every_injected_flag() {
        let marker = 42;
        assert!(swallow_keyboard_event(0, marker, marker));
        assert!(!swallow_keyboard_event(0x10, marker, marker));
        assert!(!swallow_keyboard_event(0x02, marker, marker));
        assert!(swallow_keyboard_event(0x10, marker + 1, marker));
        assert!(swallow_mouse_event(0, marker, marker));
        assert!(!swallow_mouse_event(0x01, marker, marker));
        assert!(!swallow_mouse_event(0x02, marker, marker));
        assert!(swallow_mouse_event(0x01, marker + 1, marker));
        let markers = InjectionMarkers {
            remote: 42,
            keyboard_canary: 43,
            mouse_canary: 44,
        };
        assert_eq!(
            keyboard_hook_decision(0x10, 43, markers),
            HookDecision::KeyboardCanary
        );
        assert_eq!(
            mouse_hook_decision(0x01, 44, markers),
            HookDecision::MouseCanary
        );
        assert_eq!(
            keyboard_hook_decision(0x10, 42, markers),
            HookDecision::PassRemote
        );
        assert_eq!(
            mouse_hook_decision(0x01, 42, markers),
            HookDecision::PassRemote
        );
        assert_eq!(
            keyboard_hook_decision(0x10, 99, markers),
            HookDecision::Swallow
        );
        assert!(!HookProof::canaries_advanced(1, 1, 2, 1));
        assert!(HookProof::canaries_advanced(1, 1, 2, 2));

        #[cfg(windows)]
        {
            let inactive = HookProof {
                active: Arc::new(AtomicBool::new(false)),
            };
            assert!(inactive
                .probe_with_sender(|| Ok(()), Duration::from_millis(1))
                .unwrap_err()
                .contains("exited"));
            let active = HookProof {
                active: Arc::new(AtomicBool::new(true)),
            };
            assert!(active
                .probe_with_sender(
                    || Err("SendInput failed".to_string()),
                    Duration::from_millis(1)
                )
                .unwrap_err()
                .contains("SendInput"));
            assert!(active
                .probe_with_sender(|| Ok(()), Duration::from_millis(1))
                .unwrap_err()
                .contains("deadline"));
        }
    }

    #[cfg(windows)]
    fn output(
        output_index: u32,
        device_name: &str,
        technology: i32,
        path: &str,
        active: bool,
        manufacturer: u16,
        product: u16,
    ) -> crate::gpu_probe::OutputCapability {
        crate::gpu_probe::OutputCapability {
            adapter_output_index: output_index,
            attached_global_index: active.then_some(output_index),
            device_name: device_name.to_string(),
            attached_to_desktop: active,
            primary: output_index == 0,
            monitor_handle: "redacted".to_string(),
            desktop_rect: crate::gpu_probe::RectCapability {
                left: 0,
                top: 0,
                width: 1920,
                height: 1080,
            },
            current_mode: None,
            supported_modes: Vec::new(),
            target_available: Some(true),
            ccd_active: Some(active),
            target_id: Some(output_index),
            monitor_device_path: Some(path.to_string()),
            monitor_friendly_name: None,
            output_technology: Some(technology),
            edid_manufacture_id: Some(manufacturer),
            edid_product_code_id: Some(product),
            connector_instance: Some(output_index),
            deskside_identity_sha256: Some(normalized_identity_hash(path)),
            deskside_edid_sha256: Some(edid_tuple_hash(manufacturer, product, output_index)),
            deskside_capture_sha256: capture_pin_hash(
                "pci-adapter-1",
                output_index,
                device_name,
                path,
                technology,
            )
            .ok(),
        }
    }

    #[cfg(windows)]
    fn mixed_report(
        physical_active: bool,
        software: bool,
    ) -> (HostCapabilityReport, ResolvedOutput, DesksideConfig) {
        let outputs = vec![
            output(0, r"\\.\DISPLAY1", 16, "INDIRECT#ARCEN", true, 1, 1),
            output(
                1,
                r"\\.\DISPLAY2",
                5,
                "MONITOR#PHYSICAL",
                physical_active,
                2,
                2,
            ),
        ];
        let kind = crate::gpu_probe::classify_adapter(0x10de, software, "NVIDIA RTX", &outputs);
        let report = HostCapabilityReport {
            schema_version: 1,
            topology_error: None,
            nvenc_runtime_dll: true,
            openh264_compiled: true,
            vmware_resolution_tool: None,
            adapters: vec![crate::gpu_probe::AdapterCapability {
                stable_id: "pci-adapter-1".to_string(),
                device_path: None,
                dxgi_index: 0,
                description: "NVIDIA RTX".to_string(),
                kind,
                vendor_id: 0x10de,
                device_id: 1,
                subsystem_id: 1,
                revision: 1,
                dedicated_video_memory_bytes: 1,
                shared_system_memory_bytes: 1,
                session_luid: "00000001:00000002".to_string(),
                software,
                d3d11_feature_level: Some("0xb000".to_string()),
                d3d11_video_device: true,
                direct_nvenc_candidate: true,
                outputs: outputs.clone(),
            }],
            recommendation: None,
            hypervisor_present: Some(false),
            firmware_sha256: Some("f".repeat(HASH_HEX_BYTES)),
        };
        let capture = ResolvedOutput {
            global_index: 0,
            adapter_name: "NVIDIA RTX".to_string(),
            adapter_output_index: 0,
            device_name: r"\\.\DISPLAY1".to_string(),
            vendor_id: 0x10de,
            desktop_rect: crate::display::DesktopRect {
                left: 0,
                top: 0,
                width: 1920,
                height: 1080,
            },
        };
        let config = DesksideConfig {
            enabled: true,
            firmware_sha256: "f".repeat(HASH_HEX_BYTES),
            capture_sha256: outputs[0]
                .deskside_capture_sha256
                .clone()
                .expect("capture pin"),
            monitors: vec![PhysicalMonitorPin {
                identity_sha256: normalized_identity_hash("MONITOR#PHYSICAL"),
                edid_sha256: edid_tuple_hash(2, 2, 1),
            }],
        };
        (report, capture, config)
    }

    #[test]
    #[cfg(windows)]
    fn production_chain_accepts_mixed_physical_and_indirect_capture() {
        let (report, capture, config) = mixed_report(true, false);
        assert_eq!(
            report.adapters[0].kind,
            crate::gpu_probe::AdapterKind::Hardware
        );
        let (evidence, binding) =
            collect_evidence_from_report(&config, &report, &capture).expect("mixed evidence");
        assert_ne!(evidence.input_fingerprint(), evidence.display_fingerprint());

        let (protected_report, capture, config) = mixed_report(false, false);
        assert!(validate_protected_inventory(
            &config,
            &output_facts(&protected_report, &capture),
            binding,
        )
        .is_ok());
    }

    #[test]
    #[cfg(windows)]
    fn production_chain_refuses_every_negative_boundary() {
        let (mut report, mut capture, mut config) = mixed_report(true, false);
        report.hypervisor_present = Some(true);
        assert!(collect_evidence_from_report(&config, &report, &capture).is_err());
        report.hypervisor_present = Some(false);
        report.firmware_sha256 = None;
        assert!(collect_evidence_from_report(&config, &report, &capture).is_err());
        report.firmware_sha256 = Some("f".repeat(HASH_HEX_BYTES));
        config.capture_sha256 = "0".repeat(HASH_HEX_BYTES);
        assert!(collect_evidence_from_report(&config, &report, &capture).is_err());
        config = mixed_report(true, false).2;
        report.adapters[0].outputs[1].monitor_device_path = Some("UNPINNED".to_string());
        assert!(collect_evidence_from_report(&config, &report, &capture).is_err());

        let (software, software_capture, software_config) = mixed_report(true, true);
        assert!(
            collect_evidence_from_report(&software_config, &software, &software_capture).is_err()
        );

        let (mut physical_capture, _, physical_config) = mixed_report(true, false);
        physical_capture.adapters[0].outputs[0].output_technology = Some(5);
        assert!(
            collect_evidence_from_report(&physical_config, &physical_capture, &capture).is_err()
        );
        capture.adapter_name = "missing".to_string();
        assert!(collect_evidence_from_report(&config, &report, &capture).is_err());

        let (mut indirect_only, indirect_capture, indirect_config) = mixed_report(true, false);
        indirect_only.adapters[0].outputs.truncate(1);
        indirect_only.adapters[0].kind = crate::gpu_probe::classify_adapter(
            0x10de,
            false,
            "NVIDIA RTX",
            &indirect_only.adapters[0].outputs,
        );
        assert_eq!(
            indirect_only.adapters[0].kind,
            crate::gpu_probe::AdapterKind::RemoteOrIndirect
        );
        assert!(
            collect_evidence_from_report(&indirect_config, &indirect_only, &indirect_capture)
                .is_err()
        );

        let (mut indirect_virtual, virtual_capture, virtual_config) = mixed_report(true, false);
        indirect_virtual.adapters[0].outputs[1].output_technology = Some(17);
        indirect_virtual.adapters[0].kind = crate::gpu_probe::classify_adapter(
            0x10de,
            false,
            "NVIDIA RTX",
            &indirect_virtual.adapters[0].outputs,
        );
        assert_eq!(
            indirect_virtual.adapters[0].kind,
            crate::gpu_probe::AdapterKind::RemoteOrIndirect
        );
        assert!(
            collect_evidence_from_report(&virtual_config, &indirect_virtual, &virtual_capture)
                .is_err()
        );
    }

    fn raw_smbios(product: &str) -> Vec<u8> {
        let mut table = Vec::new();
        table.extend_from_slice(&[1, 8, 0, 0, 1, 2, 0, 0]);
        table.extend_from_slice(b"Dell\0");
        table.extend_from_slice(product.as_bytes());
        table.extend_from_slice(b"\0\0");
        table.extend_from_slice(&[3, 6, 0, 0, 1, 3]);
        table.extend_from_slice(b"Dell\0\0");
        table.extend_from_slice(&[127, 4, 0, 0, 0, 0]);
        let mut raw = vec![0, 3, 5, 0];
        raw.extend_from_slice(&(table.len() as u32).to_le_bytes());
        raw.extend_from_slice(&table);
        raw
    }

    #[test]
    fn firmware_collector_requires_positive_nonvirtual_chassis() {
        let first =
            firmware_fingerprint_from_raw_smbios(&raw_smbios("Precision 7960")).expect("physical");
        let second =
            firmware_fingerprint_from_raw_smbios(&raw_smbios("Precision 7960")).expect("stable");
        assert_eq!(first, second);
        assert!(
            firmware_fingerprint_from_raw_smbios(&raw_smbios("VMware Virtual Platform")).is_err()
        );
        let mut corrupt = raw_smbios("Precision 7960");
        corrupt[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(firmware_fingerprint_from_raw_smbios(&corrupt).is_err());
    }
}
