// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The Win32 half of the Deck's component client: the service control
//! manager, the HID device scan, elevation and the pipe. Everything decided
//! is in the crate root or shared (`arcen_usb_bridge`).
//!
//! This module is the crate's only FFI; every `unsafe` block names what it
//! relies on.
#![allow(unsafe_code)]

pub(crate) mod devices;
pub(crate) mod elevate;
pub(crate) mod pipe;
pub(crate) mod scm;

use std::path::PathBuf;
use std::time::Duration;

use arcen_usb_bridge::NativeComponentFacts;
use arcen_usb_bridge::winusb_package::{TabletModel, TabletPreparation};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW};
use windows::core::PCWSTR;

use crate::NativeComponentAction;

/// The component installer's file name (`packaging/windows-deck`).
const INSTALLER_NAME: &str = "Arcen USB Component.msi";
/// The prepare tool, installed beside the service.
const PREPARE_TOOL: &str = "arcen-usb-prepare.exe";
/// How long an elevated prepare may take, the approval prompt included.
const PREPARE_TIMEOUT: Duration = Duration::from_secs(120);

pub(crate) fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

pub(crate) fn from_wide(buffer: &[u16]) -> String {
    let end = buffer
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..end])
}

/// A kernel handle, closed on drop.
pub(crate) struct OwnedHandle(pub(crate) HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: owned and closed once.
            let _ = unsafe { CloseHandle(self.0) };
        }
    }
}

/// `CurrentBuildNumber`; `RtlGetVersion` would need the WDK bindings and
/// `GetVersionEx` lies to an unmanifested process.
fn build_number() -> Option<u32> {
    let key = wide(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion");
    let value = wide("CurrentBuildNumber");
    let mut buffer = [0_u16; 32];
    let mut size = u32::try_from(size_of_val(&buffer)).ok()?;
    // SAFETY: reads one REG_SZ into a buffer of `size` bytes.
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(key.as_ptr()),
            PCWSTR(value.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            Some(buffer.as_mut_ptr().cast()),
            Some(&raw mut size),
        )
    };
    if status.is_err() {
        return None;
    }
    from_wide(&buffer).trim().parse().ok()
}

/// `%ProgramData%\Arcen\USB\packages`: readable by users, written only by
/// `arcen-usb-prepare` (elevated) and the service.
fn packages_dir() -> PathBuf {
    std::env::var_os("ProgramData")
        .map_or_else(|| PathBuf::from(r"C:\ProgramData"), PathBuf::from)
        .join("Arcen")
        .join("USB")
        .join("packages")
}

fn prepared_models() -> Vec<TabletModel> {
    let Ok(entries) = std::fs::read_dir(packages_dir()) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| TabletModel::from_package_name(&entry.file_name().to_string_lossy()))
        .collect()
}

fn tablet() -> TabletPreparation {
    TabletPreparation::assess(&devices::attached_tablets(), &prepared_models())
}

pub(crate) fn facts() -> NativeComponentFacts {
    let os = crate::os_supported(build_number(), cfg!(target_arch = "x86_64"));
    crate::facts_from(os, scm::service_facts(), &tablet())
}

pub(crate) fn run_action(action: NativeComponentAction) -> Result<(), String> {
    match action {
        NativeComponentAction::Install => install(),
        NativeComponentAction::Start => scm::start(),
        NativeComponentAction::Prepare => prepare(),
        other => crate::nothing_to_do(other),
    }
}

/// The installer shipped beside the Deck (a release folder), or the release
/// page. Windows Installer asks for the administrator approval itself.
fn install() -> Result<(), String> {
    let beside = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join(INSTALLER_NAME)))
        .filter(|path| path.is_file());
    match beside {
        Some(installer) => elevate::open(&installer.to_string_lossy()),
        None => elevate::open(&format!(
            "{}/releases/latest",
            arcen_deck_core::about::SOURCE_URL
        )),
    }
}

/// Runs the installed prepare tool for the attached tablet, elevated.
fn prepare() -> Result<(), String> {
    let instance = match tablet() {
        TabletPreparation::NeedsPreparing { instance_id } => instance_id,
        TabletPreparation::Prepared { .. } => return Ok(()),
        TabletPreparation::NoTablet => {
            return Err("Connect the tablet to this PC, then prepare it.".to_owned());
        }
    };
    let parameters = crate::prepare_parameters(&instance)?;
    let tool = scm::service_directory()?.join(PREPARE_TOOL);
    if !tool.is_file() {
        return Err(format!(
            "{PREPARE_TOOL} is missing beside the Arcen USB component; repair the component"
        ));
    }
    match elevate::run_elevated(&tool, &parameters, PREPARE_TIMEOUT)? {
        0 => Ok(()),
        code => Err(format!(
            "Preparing the tablet failed (exit code {code}). Run {} {parameters} from an \
             administrator prompt to see why.",
            tool.display()
        )),
    }
}
