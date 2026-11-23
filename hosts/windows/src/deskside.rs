// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
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

/// `DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INDIRECT_WIRED` or `_INDIRECT_VIRTUAL`.
fn indirect_connector(value: i32) -> bool {
    matches!(value, 16 | 17)
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

/// Whether this console session's desktop is on the desk's monitors: the
/// active console session over the local protocol is; a session that is
/// not (a remote protocol, another session on the console) is not. A fact
/// that cannot be read counts as on the desk (fail closed).
#[cfg(windows)]
pub fn desk_exposure(
    session: &WindowsSessionIdentity,
) -> arcen_session::console_session::desk_privacy::DeskExposure {
    use arcen_session::console_session::desk_privacy::DeskExposure;
    let console = crate::logon_activation::active_console_session().ok();
    let remote = session_protocol(session.session_id)
        .ok()
        .map(|protocol| protocol != 0);
    if console.is_some_and(|console| console != session.session_id) || remote == Some(true) {
        DeskExposure::NotShown
    } else {
        DeskExposure::PhysicalDesk
    }
}

#[cfg(not(windows))]
pub fn desk_exposure(
    _session: &WindowsSessionIdentity,
) -> arcen_session::console_session::desk_privacy::DeskExposure {
    arcen_session::console_session::desk_privacy::DeskExposure::PhysicalDesk
}

/// The display topology desk privacy reads between proofs.
#[cfg(windows)]
type DeskTopology = crate::gpu_probe::DeskTopology;
#[cfg(not(windows))]
type DeskTopology = ();

/// The lit outputs a dark desk was proved with (adapter LUID, output), and
/// the topology read just before that proof.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DarkDeskProof {
    lit: std::collections::BTreeSet<(String, u32)>,
    topology: Option<DeskTopology>,
}

/// What is behind one lit output: nothing for a virtual display (an
/// indirect display whose adapter software enumerated: a virtual display
/// driver, the Pier's IddCx display); on an NVIDIA GPU, a monitor when any
/// display NVAPI reports active is physically connected (NVAPI's
/// per-display facts are not tied to a DXGI output here, so one lit monitor
/// counts for every output of its GPU); otherwise unknown. Windows cannot
/// darken a monitor, so the desk's monitors are detached from the desktop
/// while a Deck is attached and this proves nothing at the desk is lit
/// (shared `desk_outputs_dark`).
fn desk_monitor(
    virtual_display: bool,
    nvidia_gpu_lights_a_monitor: Option<bool>,
) -> arcen_session::console_session::desk_privacy::DeskMonitor {
    use arcen_session::console_session::desk_privacy::DeskMonitor;
    match (virtual_display, nvidia_gpu_lights_a_monitor) {
        (true, _) => DeskMonitor::Absent,
        (false, Some(true)) => DeskMonitor::Present,
        (false, Some(false)) => DeskMonitor::Absent,
        (false, None) => DeskMonitor::Unknown,
    }
}

#[cfg(windows)]
fn lit_outputs(report: &HostCapabilityReport) -> std::collections::BTreeSet<(String, u32)> {
    report
        .adapters
        .iter()
        .flat_map(|adapter| {
            adapter
                .outputs
                .iter()
                .filter(|output| output.attached_to_desktop && output.ccd_active == Some(true))
                .map(|output| (adapter.session_luid.clone(), output.adapter_output_index))
        })
        .collect()
}

/// NVAPI's `NV_VIRTUALIZATION_MODE_VGX`: a virtual GPU in a guest.
const NVAPI_VIRTUALIZATION_VGPU_GUEST: u32 = 2;

/// Whether NVAPI proved a monitor physically connected on `gpu` (only
/// active displays when `active_only`): `None` unless its display-id
/// enumeration succeeded, so a failed or missing call never reads as "no
/// monitor". A virtual GPU in a guest drives no monitor at a desk: its
/// heads report themselves physically connected (lab: the active head of a
/// GRID V100D-16Q guest), but nothing is behind them; who may watch a
/// virtual machine's console is the platform's to decide (issue #107).
fn nvidia_gpu_monitor(gpu: &crate::nvapi_inventory::GpuEntry, active_only: bool) -> Option<bool> {
    if gpu.virtualization_mode == Some(NVAPI_VIRTUALIZATION_VGPU_GUEST) {
        return Some(false);
    }
    let succeeded = |call: &str| {
        gpu.calls
            .iter()
            .any(|outcome| outcome.call == call && outcome.succeeded())
    };
    if !succeeded("NvAPI_GPU_GetAllDisplayIds")
        || !succeeded("NvAPI_GPU_GetConnectedDisplayIds(uncached)")
    {
        return None;
    }
    Some(gpu.displays.iter().any(|display| {
        display.flags.physically_connected && (display.flags.active || !active_only)
    }))
}

/// Classifies every output the desktop lights and returns them once no
/// monitor at the desk is behind any (shared `desk_outputs_dark`).
#[cfg(windows)]
fn dark_lit_outputs() -> Result<
    std::collections::BTreeSet<(String, u32)>,
    arcen_session::console_session::desk_privacy::DeskPrivacyRefusal,
> {
    use arcen_session::console_session::desk_privacy::{
        desk_outputs_dark, DeskOutputFact, DeskPrivacyRefusal,
    };
    // Only what the judgement needs: no D3D11 device, display-mode list,
    // Media Foundation enumeration or EDID read, which disturb a streaming
    // session's capture and encode on the same GPU.
    let report = crate::gpu_probe::probe_at(crate::gpu_probe::ProbeDepth::DeskTopology)
        .map_err(|_| DeskPrivacyRefusal::DeskMonitorUnknown)?;
    if report.topology_error.is_some() {
        return Err(DeskPrivacyRefusal::DeskMonitorUnknown);
    }
    let nvapi = crate::nvapi_inventory::desk_inventory().ok();
    let gpu_lights_a_monitor = |luid: &str| -> Option<bool> {
        let gpu = nvapi.as_ref()?.gpus.iter().find(|gpu| {
            gpu.adapter_luid.is_some_and(|gpu_luid| {
                format!(
                    "{:08x}:{:08x}",
                    gpu_luid.high_part as u32, gpu_luid.low_part
                ) == luid
            })
        })?;
        nvidia_gpu_monitor(gpu, true)
    };
    let mut facts = Vec::new();
    for adapter in &report.adapters {
        for output in &adapter.outputs {
            let lit = output.attached_to_desktop && output.ccd_active == Some(true);
            if !lit {
                continue;
            }
            let monitor = desk_monitor(
                virtual_display(output),
                gpu_lights_a_monitor(&adapter.session_luid),
            );
            tracing::debug!(
                target: crate::logging::SESSION,
                adapter = %adapter.description,
                output = output.adapter_output_index,
                monitor = ?monitor,
                "desk privacy: a lit output"
            );
            facts.push(DeskOutputFact { lit, monitor });
        }
    }
    desk_outputs_dark(&facts)?;
    Ok(lit_outputs(&report))
}

/// Proves that no monitor at the desk shows the desktop: every output the
/// desktop lights has nothing physically connected behind it.
///
/// # Errors
///
/// The shared refusal when a lit output drives a monitor, or when that
/// cannot be told.
/// A virtual display: an indirect display (`DISPLAYCONFIG_OUTPUT_TECHNOLOGY_
/// INDIRECT_*`) whose adapter software enumerated (`\\?\ROOT#…`: a virtual
/// display driver, the Pier's IddCx display). A hardware indirect adapter (a
/// USB display adapter, for example) drives a monitor at the desk.
fn virtual_display(output: &crate::gpu_probe::OutputCapability) -> bool {
    output.output_technology.is_some_and(indirect_connector)
        && output
            .target_adapter_path
            .as_deref()
            .is_some_and(software_enumerated_adapter)
        && output
            .edid_manufacture_id
            .is_some_and(viewerless_virtual_display_manufacturer)
}

/// EDID manufacturers of virtual display drivers that show their display
/// nowhere: Arcen's own (`ARN`) and the Virtual Display Driver the GeForce
/// guide uses (`MTT`, its default `MTT1337` monitor). Software-enumerated
/// indirect displays also include screen extenders whose whole purpose is to
/// show the desktop on a tablet at the desk, so any other one is not trusted
/// (`desk_monitor_unknown`).
fn viewerless_virtual_display_manufacturer(id: u16) -> bool {
    const MTT: u16 = edid_manufacturer(*b"MTT");
    [arcen_outputs::edid::EDID_MANUFACTURER_ID, MTT]
        .iter()
        .any(|known| id == *known || id.swap_bytes() == *known)
}

const fn edid_manufacturer(letters: [u8; 3]) -> u16 {
    (((letters[0] - b'@') as u16) << 10)
        | (((letters[1] - b'@') as u16) << 5)
        | ((letters[2] - b'@') as u16)
}

fn software_enumerated_adapter(path: &str) -> bool {
    path.get(..9)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(r"\\?\ROOT#"))
}

/// The output to serve a desk-private session on when the configured one
/// shows on a monitor at the desk: the first virtual display on the same
/// adapter, whose encoder was already chosen (`None`: keep the configured
/// output). Windows cannot darken a monitor, so a session shown on one
/// could not be private.
#[cfg(windows)]
pub fn virtual_output_instead_of(
    configured: &crate::display::OutputSelector,
) -> Option<crate::display::OutputSelector> {
    let resolved = crate::display::resolve_output_selector(configured).ok()?;
    let report = crate::gpu_probe::probe().ok()?;
    let configured_is_virtual = report.adapters.iter().any(|adapter| {
        adapter
            .outputs
            .iter()
            .any(|output| output.device_name == resolved.device_name && virtual_display(output))
    });
    if configured_is_virtual {
        return None;
    }
    report
        .adapters
        .iter()
        .filter(|adapter| {
            adapter
                .description
                .eq_ignore_ascii_case(&resolved.adapter_name)
        })
        .find_map(|adapter| {
            adapter
                .outputs
                .iter()
                .find(|output| output.attached_to_desktop && virtual_display(output))
                .map(|output| crate::display::OutputSelector::Adapter {
                    name: adapter.description.clone(),
                    output_index: output.adapter_output_index,
                })
        })
}

#[cfg(not(windows))]
pub fn virtual_output_instead_of(
    _configured: &crate::display::OutputSelector,
) -> Option<crate::display::OutputSelector> {
    None
}

#[cfg(windows)]
pub fn prove_desk_dark(
) -> Result<DarkDeskProof, arcen_session::console_session::desk_privacy::DeskPrivacyRefusal> {
    let topology = read_desk_topology();
    dark_lit_outputs().map(|lit| DarkDeskProof { lit, topology })
}

#[cfg(windows)]
fn read_desk_topology() -> Option<DeskTopology> {
    crate::gpu_probe::desk_topology().ok()
}

#[cfg(not(windows))]
fn read_desk_topology() -> Option<DeskTopology> {
    None
}

#[cfg(not(windows))]
pub fn prove_desk_dark(
) -> Result<DarkDeskProof, arcen_session::console_session::desk_privacy::DeskPrivacyRefusal> {
    Err(arcen_session::console_session::desk_privacy::DeskPrivacyRefusal::DeskMonitorUnknown)
}

static DARK_DESK: std::sync::Mutex<
    Option<arcen_session::console_session::desk_privacy::DeskWatch>,
> = std::sync::Mutex::new(None);

/// Holds the proof the agent's supervision checks the desk against while
/// the Deck is attached and held (one session per agent process), and
/// starts (`Some`) or stops (`None`) the shared watcher thread that keeps
/// proving it off the session loop and the media path
/// ([`DeskWatch`](arcen_session::console_session::desk_privacy::DeskWatch)).
pub fn hold_dark_desk(proof: Option<DarkDeskProof>) {
    use arcen_session::console_session::desk_privacy::{DeskWatch, DESK_TOPOLOGY_READ};
    let watch = proof.map(|proof| {
        let topology = proof.topology.clone();
        DeskWatch::start(
            DESK_TOPOLOGY_READ,
            topology,
            read_desk_topology,
            move || verify_desk_dark(&proof),
            |report| {
                let took_ms = u64::try_from(report.took.as_millis()).unwrap_or(u64::MAX);
                match report.outcome {
                    Ok(()) => tracing::info!(
                        target: crate::logging::SESSION,
                        reason = report.reason.token(),
                        topology_reads = report.topology_reads,
                        took_ms,
                        "desk privacy: the desk was proved dark again off the media path"
                    ),
                    Err(refusal) => tracing::error!(
                        target: crate::logging::SESSION,
                        reason = report.reason.token(),
                        refusal = refusal.token(),
                        took_ms,
                        "desk privacy: the desk is no longer proved dark; the session ends"
                    ),
                }
            },
            // Raised after the watch stored its refusal (also when the
            // watcher thread panicked), so the woken loop reads it.
            || desk_alarm().notify_one(),
        )
    });
    let previous = std::mem::replace(
        &mut *DARK_DESK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        watch,
    );
    drop(previous);
}

/// Whether the held dark desk is still proved, without waiting for any
/// probe: `Ok` when no desk is held dark; otherwise the shared watcher's
/// verdict (its refusal, or
/// [`DeskMonitorUnknown`](arcen_session::console_session::desk_privacy::DeskPrivacyRefusal::DeskMonitorUnknown)
/// when it completed no check within
/// [`DESK_CHECK_STALE`](arcen_session::console_session::desk_privacy::DESK_CHECK_STALE)).
///
/// # Errors
///
/// The refusal that ends the Deck's session.
pub fn desk_watch_verdict(
) -> Result<(), arcen_session::console_session::desk_privacy::DeskPrivacyRefusal> {
    DARK_DESK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .map_or(Ok(()), |watch| watch.verdict(std::time::Instant::now()))
}

/// Woken when the desk watcher refuses, so a session loop ends at once
/// rather than at its next supervision tick (read
/// [`desk_watch_verdict`] when it fires: a wake-up may be left over from
/// an earlier watch).
pub fn desk_alarm() -> &'static tokio::sync::Notify {
    static ALARM: std::sync::OnceLock<tokio::sync::Notify> = std::sync::OnceLock::new();
    ALARM.get_or_init(tokio::sync::Notify::new)
}

/// Proves the desk dark again: still no monitor behind any lit output, and
/// the desktop still lights exactly the outputs it was proved with.
///
/// # Errors
///
/// The shared refusal of [`prove_desk_dark`], or
/// [`DeskPrivacyRefusal::DeskOutputsChanged`](arcen_session::console_session::desk_privacy::DeskPrivacyRefusal::DeskOutputsChanged)
/// when the lit outputs changed.
#[cfg(windows)]
pub fn verify_desk_dark(
    proof: &DarkDeskProof,
) -> Result<(), arcen_session::console_session::desk_privacy::DeskPrivacyRefusal> {
    use arcen_session::console_session::desk_privacy::DeskPrivacyRefusal;
    // The whole proof again (a monitor plugged into a lit head changes no
    // lit output), and the same outputs.
    if dark_lit_outputs()? != proof.lit {
        return Err(DeskPrivacyRefusal::DeskOutputsChanged);
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn verify_desk_dark(
    _proof: &DarkDeskProof,
) -> Result<(), arcen_session::console_session::desk_privacy::DeskPrivacyRefusal> {
    Err(arcen_session::console_session::desk_privacy::DeskPrivacyRefusal::DeskOutputsChanged)
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

/// A marker that survives both hook channels: 32 bits, never zero.
///
/// A low-level mouse hook sees only the low 32 bits of a mouse event's
/// `dwExtraInfo` (the raw mouse input carries a `ULONG`): measured on the
/// lab Pier, a hook read `0xae5540d2` for the marker `0x9efa8cd6ae5540d2`
/// while the keyboard hook read it whole. A wider marker made desk privacy
/// swallow every move, button and wheel Arcen injected.
fn marker_from_seed(seed: &[u8; 32], label: &[u8]) -> usize {
    let mut hasher = Sha256::new();
    hasher.update(seed);
    hasher.update(label);
    let digest = hasher.finalize();
    let mut bytes = [0_u8; 4];
    bytes.copy_from_slice(&digest[..4]);
    let marker = u32::from_ne_bytes(bytes).max(1);
    usize::try_from(marker).unwrap_or(1)
}

/// The part of `dwExtraInfo` a low-level mouse hook is given (see
/// [`marker_from_seed`]).
const fn mouse_extra_info(extra_info: usize) -> u32 {
    // Truncation is the point: Windows hands the mouse hook 32 bits.
    #[allow(clippy::cast_possible_truncation)]
    let low = extra_info as u32;
    low
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
    let extra = mouse_extra_info(extra_info);
    if flags & (INJECTED | LOWER_INTEGRITY_INJECTED) == 0 {
        HookDecision::Swallow
    } else if markers.mouse_canary != 0 && extra == mouse_extra_info(markers.mouse_canary) {
        HookDecision::MouseCanary
    } else if markers.remote != 0 && extra == mouse_extra_info(markers.remote) {
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
    /// The mouse canary must arrive too (pinned deskside). Desk privacy's
    /// default proves the hook thread with the keyboard canary alone.
    require_mouse: bool,
}

impl HookProof {
    #[cfg(windows)]
    pub fn probe(&self) -> Result<(), String> {
        self.probe_with_sender(InputHookGuard::send_hook_canaries, HOOK_CANARY_TIMEOUT)
    }

    #[cfg(windows)]
    fn probe_with_sender<F>(&self, send: F, timeout: Duration) -> Result<(), String>
    where
        F: FnOnce(bool) -> Result<(), String>,
    {
        if !self.active.load(Ordering::Acquire) {
            return Err("deskside hook thread exited".to_string());
        }
        let keyboard_before = KEYBOARD_CANARY_HEARTBEAT.load(Ordering::Acquire);
        let mouse_before = MOUSE_CANARY_HEARTBEAT.load(Ordering::Acquire);
        let mouse_required = self.require_mouse;
        send(mouse_required)?;
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if Self::canaries_advanced(
                keyboard_before,
                mouse_before,
                KEYBOARD_CANARY_HEARTBEAT.load(Ordering::Acquire),
                MOUSE_CANARY_HEARTBEAT.load(Ordering::Acquire),
                mouse_required,
            ) {
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                return Err(format!(
                    "deskside hook canary deadline expired (keyboard {}, mouse {}, mouse required {})",
                    if KEYBOARD_CANARY_HEARTBEAT.load(Ordering::Acquire) > keyboard_before {
                        "seen"
                    } else {
                        "missing"
                    },
                    if MOUSE_CANARY_HEARTBEAT.load(Ordering::Acquire) > mouse_before {
                        "seen"
                    } else {
                        "missing"
                    },
                    mouse_required,
                ));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    const fn canaries_advanced(
        keyboard_before: u64,
        mouse_before: u64,
        keyboard_after: u64,
        mouse_after: u64,
        mouse_required: bool,
    ) -> bool {
        keyboard_after > keyboard_before && (mouse_after > mouse_before || !mouse_required)
    }

    #[cfg(not(windows))]
    pub fn probe(&self) -> Result<(), String> {
        Err("Windows hook proof is unavailable on this platform".to_string())
    }
}

/// Process-owned low-level hooks running on a dedicated native message thread.
pub struct InputHookGuard {
    active: Arc<AtomicBool>,
    require_mouse: bool,
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
            require_mouse: true,
            shutdown_requested,
            thread_id,
            join: Some(join),
        })
    }

    /// [`Self::install`] for desk privacy's default: its liveness proof is
    /// the keyboard canary (see [`HookProof`]).
    pub fn install_keyboard_proved() -> Result<Self, String> {
        Self::install().map(|mut guard| {
            guard.require_mouse = false;
            guard
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
            require_mouse: self.require_mouse,
        }
    }

    #[cfg(windows)]
    fn send_hook_canaries(with_mouse: bool) -> Result<(), String> {
        use windows::Win32::Foundation::POINT;
        use windows::Win32::UI::Input::KeyboardAndMouse::{
            SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_KEYUP,
            MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_MOVE, MOUSEEVENTF_VIRTUALDESK, MOUSEINPUT,
            VIRTUAL_KEY,
        };
        use windows::Win32::UI::WindowsAndMessaging::{
            GetCursorPos, GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN,
            SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
        };

        let markers = injection_markers();
        if markers.keyboard_canary == 0 || markers.mouse_canary == 0 {
            return Err("deskside hook canary markers are unavailable".to_string());
        }
        let mut cursor = POINT::default();
        // SAFETY: cursor is a valid, writable POINT; a failure leaves it at
        // the origin, which is still a valid absolute target.
        let _ = unsafe { GetCursorPos(&mut cursor) };
        // SAFETY: GetSystemMetrics has no pointer arguments.
        let (left, top, width, height) = unsafe {
            (
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_YVIRTUALSCREEN),
                GetSystemMetrics(SM_CXVIRTUALSCREEN).max(2),
                GetSystemMetrics(SM_CYVIRTUALSCREEN).max(2),
            )
        };
        let normalize = |value: i32, origin: i32, extent: i32| -> i32 {
            let offset = i64::from((value - origin).clamp(0, extent - 1));
            i32::try_from(offset * 65_535 / i64::from(extent - 1)).unwrap_or(0)
        };
        // One pixel beside the cursor: a move to where it already is never
        // reached the hook (lab vGPU guest); the hook swallows this one.
        let canary_x = normalize(cursor.x.saturating_add(1), left, width);
        let canary_y = normalize(cursor.y, top, height);
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
                        dx: canary_x,
                        dy: canary_y,
                        mouseData: 0,
                        dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                        time: 0,
                        dwExtraInfo: markers.mouse_canary,
                    },
                },
            },
        ];
        // Without the mouse canary nothing moves, even where Windows never
        // calls the mouse hook for an injected move.
        let inputs = if with_mouse {
            &inputs[..]
        } else {
            &inputs[..2]
        };
        // SAFETY: inputs is a valid initialized slice and cbsize matches INPUT.
        let sent = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
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

    #[test]
    fn only_a_software_enumerated_indirect_display_is_virtual() {
        assert!(software_enumerated_adapter(
            r"\\?\ROOT#DISPLAY#0000#{5b45201d-f2f2-4f3b-85bb-30ff1f953599}"
        ));
        assert!(software_enumerated_adapter(r"\\?\root#mttvdd#0000#{x}"));
        assert!(!software_enumerated_adapter(
            r"\\?\USB#VID_17E9&PID_6006#0001#{5b45201d-f2f2-4f3b-85bb-30ff1f953599}"
        ));
        assert!(!software_enumerated_adapter(
            r"\\?\PCI#VEN_10DE&DEV_2704#4&1#{5b45201d}"
        ));
        assert!(!software_enumerated_adapter("short"));
        // Only virtual displays that show nowhere: Arcen's and the GeForce
        // guide's driver, in either byte order.
        let mtt = edid_manufacturer(*b"MTT");
        assert!(viewerless_virtual_display_manufacturer(mtt));
        assert!(viewerless_virtual_display_manufacturer(mtt.swap_bytes()));
        assert!(viewerless_virtual_display_manufacturer(
            arcen_outputs::edid::EDID_MANUFACTURER_ID
        ));
        assert!(!viewerless_virtual_display_manufacturer(edid_manufacturer(
            *b"DEL"
        )));
    }

    #[test]
    fn only_the_pier_s_own_display_or_a_gpu_with_nothing_connected_is_dark() {
        use arcen_session::console_session::desk_privacy::DeskMonitor;
        assert_eq!(desk_monitor(true, Some(true)), DeskMonitor::Absent);
        assert_eq!(desk_monitor(true, None), DeskMonitor::Absent);
        assert_eq!(desk_monitor(false, Some(true)), DeskMonitor::Present);
        assert_eq!(desk_monitor(false, Some(false)), DeskMonitor::Absent);
        assert_eq!(desk_monitor(false, None), DeskMonitor::Unknown);
    }

    #[test]
    fn a_gpu_whose_display_enumeration_failed_is_never_read_as_having_no_monitor() {
        use crate::nvapi_inventory::{
            CallOutcome, DisplayIdEntry, DisplayIdFlags, EdidProbe, GpuEntry,
        };
        let display = |active, physically_connected| DisplayIdEntry {
            display_id: 1,
            connector_type: 0,
            flags: DisplayIdFlags {
                active,
                connected: true,
                physically_connected,
                ..DisplayIdFlags::default()
            },
            sources: Vec::new(),
            output_id: None,
            edid: EdidProbe::default(),
            in_nvapi_display_config: false,
        };
        let gpu = |displays: Vec<DisplayIdEntry>, calls: Vec<CallOutcome>| GpuEntry {
            index: 0,
            full_name: None,
            gpu_type: None,
            system_type: None,
            quadro: None,
            virtualization_mode: None,
            virtualization_mode_name: None,
            board_number: None,
            vbios_version: None,
            pci: None,
            physical_framebuffer_kib: None,
            adapter_luid: None,
            all_outputs_mask: None,
            connected_outputs_mask: None,
            active_outputs_mask: None,
            outputs: Vec::new(),
            displays,
            calls,
        };
        let proved = || {
            vec![
                CallOutcome::ok("NvAPI_GPU_GetAllDisplayIds"),
                CallOutcome::ok("NvAPI_GPU_GetConnectedDisplayIds(uncached)"),
            ]
        };
        assert_eq!(
            nvidia_gpu_monitor(&gpu(Vec::new(), proved()), true),
            Some(false)
        );
        assert_eq!(
            nvidia_gpu_monitor(&gpu(vec![display(true, false)], proved()), true),
            Some(false)
        );
        assert_eq!(
            nvidia_gpu_monitor(&gpu(vec![display(true, true)], proved()), true),
            Some(true)
        );
        // An inactive desk monitor is dark while attached, but would light
        // on a sign-in that moves the session to the console.
        assert_eq!(
            nvidia_gpu_monitor(&gpu(vec![display(false, true)], proved()), true),
            Some(false)
        );
        assert_eq!(
            nvidia_gpu_monitor(&gpu(vec![display(false, true)], proved()), false),
            Some(true)
        );
        let failed = vec![
            CallOutcome::failed("NvAPI_GPU_GetAllDisplayIds", -1, "error"),
            CallOutcome::ok("NvAPI_GPU_GetConnectedDisplayIds(uncached)"),
        ];
        assert_eq!(nvidia_gpu_monitor(&gpu(Vec::new(), failed), true), None);
        assert_eq!(nvidia_gpu_monitor(&gpu(Vec::new(), Vec::new()), true), None);
        // A virtual GPU in a guest: its heads report a physical connection
        // with nothing at a desk behind them.
        let mut vgpu = gpu(vec![display(true, true)], Vec::new());
        vgpu.virtualization_mode = Some(2);
        assert_eq!(nvidia_gpu_monitor(&vgpu, true), Some(false));
        vgpu.virtualization_mode = Some(1);
        assert_eq!(nvidia_gpu_monitor(&vgpu, true), None);
    }
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
    fn a_mouse_hook_given_the_low_half_of_the_marker_passes_arcen_input() {
        // What the lab Pier's mouse hook read for the marker
        // 0x9efa8cd6ae5540d2: Windows hands it the low 32 bits only.
        let seed = [7_u8; 32];
        let markers = InjectionMarkers {
            remote: marker_from_seed(&seed, b"remote"),
            keyboard_canary: marker_from_seed(&seed, b"keyboard-canary"),
            mouse_canary: marker_from_seed(&seed, b"mouse-canary"),
        };
        for marker in [
            markers.remote,
            markers.keyboard_canary,
            markers.mouse_canary,
        ] {
            assert_ne!(marker, 0);
            assert!(
                u32::try_from(marker).is_ok(),
                "{marker:#x} must fit 32 bits"
            );
        }
        let seen_by_the_mouse_hook = |marker: usize| marker & 0xffff_ffff;
        assert_eq!(
            mouse_hook_decision(0x01, seen_by_the_mouse_hook(markers.remote), markers),
            HookDecision::PassRemote
        );
        assert_eq!(
            mouse_hook_decision(0x01, seen_by_the_mouse_hook(markers.mouse_canary), markers),
            HookDecision::MouseCanary
        );
        assert_eq!(
            keyboard_hook_decision(0x10, markers.remote, markers),
            HookDecision::PassRemote
        );
        // Even a 64-bit marker (0.20.8 made those, and its mouse hook
        // swallowed every Arcen move, button and wheel) is recognised from
        // the half the mouse hook is given.
        let wide = InjectionMarkers {
            remote: 0x9efa_8cd6_ae55_40d2_u64 as usize,
            ..markers
        };
        assert_eq!(
            mouse_hook_decision(0x01, 0xae55_40d2, wide),
            HookDecision::PassRemote
        );
        // A desk mouse is never remote, marker or not.
        assert_eq!(
            mouse_hook_decision(0, markers.remote, markers),
            HookDecision::Swallow
        );
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
        assert!(!HookProof::canaries_advanced(1, 1, 2, 1, true));
        assert!(HookProof::canaries_advanced(1, 1, 2, 2, true));
        assert!(HookProof::canaries_advanced(1, 1, 2, 1, false));
        assert!(!HookProof::canaries_advanced(1, 1, 1, 2, false));

        #[cfg(windows)]
        {
            let inactive = HookProof {
                active: Arc::new(AtomicBool::new(false)),
                require_mouse: true,
            };
            assert!(inactive
                .probe_with_sender(|_| Ok(()), Duration::from_millis(1))
                .unwrap_err()
                .contains("exited"));
            let active = HookProof {
                active: Arc::new(AtomicBool::new(true)),
                require_mouse: true,
            };
            assert!(active
                .probe_with_sender(
                    |_| Err("SendInput failed".to_string()),
                    Duration::from_millis(1)
                )
                .unwrap_err()
                .contains("SendInput"));
            assert!(active
                .probe_with_sender(|_| Ok(()), Duration::from_millis(1))
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
            target_adapter_path: None,
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
            media_foundation_hardware: Default::default(),
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
