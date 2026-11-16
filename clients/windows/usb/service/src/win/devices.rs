// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Device instances and their drivers, through `SetupAPI`: which tablets are
//! present, what drives them, and installing one INF on exactly one
//! instance (the per-instance path proven on Secure Boot: `DI_ENUMSINGLEINF` +
//! `SetupDiSetSelectedDriverW` + `DiInstallDevice`).

use std::path::Path;

use windows::Win32::Devices::DeviceAndDriverInstallation::{
    DI_ENUMSINGLEINF, DI_FLAGSEX_ALLOWEXCLUDEDDRVS, DIGCF_ALLCLASSES, DIGCF_DEVICEINTERFACE,
    DIGCF_PRESENT, DIINSTALLDEVICE_FLAGS, DiInstallDevice, HDEVINFO, SP_COPY_STYLE,
    SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W, SP_DEVINFO_DATA,
    SP_DEVINSTALL_PARAMS_W, SP_DRVINFO_DATA_V2_W, SPDIT_COMPATDRIVER, SPOST_PATH, SetupCopyOEMInfW,
    SetupDiBuildDriverInfoList, SetupDiCreateDeviceInfoListExW, SetupDiDestroyDeviceInfoList,
    SetupDiEnumDeviceInfo, SetupDiEnumDeviceInterfaces, SetupDiEnumDriverInfoW,
    SetupDiGetClassDevsW, SetupDiGetDeviceInstallParamsW, SetupDiGetDeviceInstanceIdW,
    SetupDiGetDeviceInterfaceDetailW, SetupDiGetDevicePropertyW, SetupDiOpenDeviceInfoW,
    SetupDiSetDeviceInstallParamsW, SetupDiSetSelectedDriverW, SetupGetInfDriverStoreLocationW,
};
use windows::Win32::Devices::Properties::{
    DEVPKEY_Device_DriverInfPath, DEVPKEY_Device_HardwareIds, DEVPROPTYPE,
};
use windows::Win32::Foundation::{BOOL, HWND};
use windows::core::{GUID, PCWSTR};

use super::wide;

/// A `SetupAPI` device information set, destroyed on drop.
struct DeviceSet(HDEVINFO);

impl Drop for DeviceSet {
    fn drop(&mut self) {
        // SAFETY: the set was returned by SetupAPI and is destroyed once.
        let _ = unsafe { SetupDiDestroyDeviceInfoList(self.0) };
    }
}

fn devinfo_data() -> SP_DEVINFO_DATA {
    SP_DEVINFO_DATA {
        cbSize: u32::try_from(size_of::<SP_DEVINFO_DATA>()).unwrap_or(0),
        ..Default::default()
    }
}

fn instance_id(set: &DeviceSet, data: &SP_DEVINFO_DATA) -> Option<String> {
    let mut buffer = [0_u16; 512];
    let mut required = 0_u32;
    // SAFETY: `data` belongs to `set`; the buffer is a valid slice.
    unsafe { SetupDiGetDeviceInstanceIdW(set.0, data, Some(&mut buffer), Some(&raw mut required)) }
        .ok()?;
    Some(super::from_wide(&buffer))
}

/// Instance IDs of every present device on the USB enumerator.
pub(crate) fn present_usb_instances() -> Result<Vec<String>, String> {
    let enumerator = wide("USB");
    // SAFETY: a null class with DIGCF_ALLCLASSES and a valid enumerator string.
    let set = DeviceSet(
        unsafe {
            SetupDiGetClassDevsW(
                None,
                PCWSTR(enumerator.as_ptr()),
                HWND::default(),
                DIGCF_PRESENT | DIGCF_ALLCLASSES,
            )
        }
        .map_err(|error| format!("list USB devices: {error}"))?,
    );
    let mut out = Vec::new();
    for index in 0.. {
        let mut data = devinfo_data();
        // SAFETY: enumerates the set into an initialised SP_DEVINFO_DATA.
        if unsafe { SetupDiEnumDeviceInfo(set.0, index, &raw mut data) }.is_err() {
            break;
        }
        if let Some(id) = instance_id(&set, &data) {
            out.push(id);
        }
    }
    Ok(out)
}

fn open(instance: &str) -> Result<(DeviceSet, SP_DEVINFO_DATA), String> {
    // SAFETY: an empty set on the local machine.
    let set = DeviceSet(
        unsafe { SetupDiCreateDeviceInfoListExW(None, HWND::default(), PCWSTR::null(), None) }
            .map_err(|error| format!("device set: {error}"))?,
    );
    let mut data = devinfo_data();
    let id = wide(instance);
    // SAFETY: opens one instance into the set.
    unsafe {
        SetupDiOpenDeviceInfoW(
            set.0,
            PCWSTR(id.as_ptr()),
            HWND::default(),
            0,
            Some(&raw mut data),
        )
    }
    .map_err(|error| format!("open device {instance}: {error}"))?;
    Ok((set, data))
}

/// The INF the instance's driver came from (`input.inf`, `oem12.inf`), or
/// `None` when it has no driver.
pub(crate) fn driver_inf(instance: &str) -> Result<Option<String>, String> {
    let (set, data) = open(instance)?;
    let key = DEVPKEY_Device_DriverInfPath;
    let mut kind = DEVPROPTYPE::default();
    let mut buffer = [0_u8; 520];
    let mut required = 0_u32;
    // SAFETY: reads one string property into a byte buffer of known size.
    let read = unsafe {
        SetupDiGetDevicePropertyW(
            set.0,
            &raw const data,
            &raw const key,
            &raw mut kind,
            Some(&mut buffer),
            Some(&raw mut required),
            0,
        )
    };
    if read.is_err() {
        return Ok(None);
    }
    let units: Vec<u16> = buffer
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    let name = super::from_wide(&units);
    Ok((!name.is_empty()).then_some(name))
}

/// The instance's hardware IDs (what Windows matches INFs against).
pub(crate) fn hardware_ids(instance: &str) -> Result<Vec<String>, String> {
    let (set, data) = open(instance)?;
    let key = DEVPKEY_Device_HardwareIds;
    let mut kind = DEVPROPTYPE::default();
    let mut buffer = vec![0_u8; 4096];
    let mut required = 0_u32;
    // SAFETY: reads one string-list property into a byte buffer of known size.
    unsafe {
        SetupDiGetDevicePropertyW(
            set.0,
            &raw const data,
            &raw const key,
            &raw mut kind,
            Some(&mut buffer),
            Some(&raw mut required),
            0,
        )
    }
    .map_err(|error| format!("hardware IDs of {instance}: {error}"))?;
    let units: Vec<u16> = buffer[..(required as usize).min(buffer.len())]
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    Ok(units
        .split(|&unit| unit == 0)
        .filter(|part| !part.is_empty())
        .map(String::from_utf16_lossy)
        .collect())
}

/// Installs the driver in `inf` (a full path) on exactly `instance`, even
/// when Windows ranks another driver higher.
pub(crate) fn install_on(instance: &str, inf: &Path) -> Result<String, String> {
    let (set, mut data) = open(instance)?;
    let mut params = SP_DEVINSTALL_PARAMS_W {
        cbSize: u32::try_from(size_of::<SP_DEVINSTALL_PARAMS_W>()).unwrap_or(0),
        ..Default::default()
    };
    // SAFETY: reads the instance's install parameters.
    unsafe { SetupDiGetDeviceInstallParamsW(set.0, Some(&raw const data), &raw mut params) }
        .map_err(|error| format!("install parameters: {error}"))?;
    let path = wide(&inf.to_string_lossy());
    if path.len() > params.DriverPath.len() {
        return Err("INF path too long".to_owned());
    }
    params.DriverPath = [0; 260];
    params.DriverPath[..path.len()].copy_from_slice(&path);
    params.Flags |= DI_ENUMSINGLEINF;
    params.FlagsEx |= DI_FLAGSEX_ALLOWEXCLUDEDDRVS;
    // SAFETY: writes the parameters back for this instance.
    unsafe { SetupDiSetDeviceInstallParamsW(set.0, Some(&raw const data), &raw const params) }
        .map_err(|error| format!("set install parameters: {error}"))?;
    // SAFETY: builds the compatible-driver list from that one INF.
    unsafe { SetupDiBuildDriverInfoList(set.0, Some(&raw mut data), SPDIT_COMPATDRIVER) }
        .map_err(|error| format!("driver list: {error}"))?;
    let mut driver = SP_DRVINFO_DATA_V2_W {
        cbSize: u32::try_from(size_of::<SP_DRVINFO_DATA_V2_W>()).unwrap_or(0),
        ..Default::default()
    };
    // SAFETY: the first (only) matching driver of that INF.
    unsafe {
        SetupDiEnumDriverInfoW(
            set.0,
            Some(&raw const data),
            SPDIT_COMPATDRIVER,
            0,
            &raw mut driver,
        )
    }
    .map_err(|error| format!("{} has no driver for {instance}: {error}", inf.display()))?;
    // SAFETY: selects it for the instance.
    unsafe { SetupDiSetSelectedDriverW(set.0, Some(&raw mut data), Some(&raw mut driver)) }
        .map_err(|error| format!("select driver: {error}"))?;
    let mut reboot = BOOL::default();
    // SAFETY: installs the selected driver on this one instance.
    unsafe {
        DiInstallDevice(
            HWND::default(),
            set.0,
            &raw const data,
            Some(&raw const driver),
            DIINSTALLDEVICE_FLAGS(0),
            Some(&raw mut reboot),
        )
    }
    .map_err(|error| format!("install driver on {instance}: {error}"))?;
    Ok(format!(
        "installed '{}' on {instance}{}",
        super::from_wide(&driver.Description),
        if reboot.as_bool() {
            " (reboot requested)"
        } else {
            ""
        }
    ))
}

/// Stages a signed package in the driver store; returns its published name
/// (`oem9.inf`). Already-staged identical packages return the same name.
pub(crate) fn stage(inf: &Path) -> Result<String, String> {
    let source = wide(&inf.to_string_lossy());
    let mut destination = [0_u16; 260];
    let mut required = 0_u32;
    // SAFETY: valid source path, output buffer and size out-parameter.
    unsafe {
        SetupCopyOEMInfW(
            PCWSTR(source.as_ptr()),
            PCWSTR::null(),
            SPOST_PATH,
            SP_COPY_STYLE(0),
            Some(&mut destination),
            Some(&raw mut required),
            None,
        )
    }
    .map_err(|error| format!("stage {}: {error}", inf.display()))?;
    let full = super::from_wide(&destination);
    Path::new(&full)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .ok_or_else(|| format!("staging returned no published name ({full})"))
}

/// The driver-store folder name of a published INF
/// (`arcen_usb_056a_0317.inf_amd64_…`), whose prefix is the original name.
pub(crate) fn store_folder(published_inf: &str) -> Option<String> {
    let name = wide(published_inf);
    let mut buffer = [0_u16; 520];
    // SAFETY: valid INF name and output buffer.
    unsafe {
        SetupGetInfDriverStoreLocationW(
            PCWSTR(name.as_ptr()),
            None,
            PCWSTR::null(),
            &mut buffer,
            None,
        )
    }
    .ok()?;
    let path = super::from_wide(&buffer);
    Path::new(&path)
        .parent()
        .and_then(Path::file_name)
        .map(|folder| folder.to_string_lossy().into_owned())
}

/// Published names (`oemNN.inf`) of every staged package whose original
/// name starts with `original_prefix` (case-insensitive).
pub(crate) fn published_with_prefix(original_prefix: &str) -> Vec<String> {
    let Some(windows) = super::system_root() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(windows.join("INF")) else {
        return Vec::new();
    };
    let prefix = original_prefix.to_ascii_lowercase();
    entries
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| arcen_usb_bridge::winusb_package::is_published_oem_inf(name))
        .filter(|name| {
            store_folder(name)
                .is_some_and(|folder| folder.to_ascii_lowercase().starts_with(&prefix))
        })
        .collect()
}

/// The device path of the present interface `interface` that belongs to
/// `instance`, if Windows has published it yet.
pub(crate) fn interface_path(interface: &GUID, instance: &str) -> Option<String> {
    // SAFETY: lists present interfaces of one class.
    let set = DeviceSet(
        unsafe {
            SetupDiGetClassDevsW(
                Some(interface),
                PCWSTR::null(),
                HWND::default(),
                DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
            )
        }
        .ok()?,
    );
    for index in 0.. {
        let mut data = SP_DEVICE_INTERFACE_DATA {
            cbSize: u32::try_from(size_of::<SP_DEVICE_INTERFACE_DATA>()).unwrap_or(0),
            ..Default::default()
        };
        // SAFETY: enumerates interfaces of the set.
        if unsafe { SetupDiEnumDeviceInterfaces(set.0, None, interface, index, &raw mut data) }
            .is_err()
        {
            return None;
        }
        // A detail structure with room for a long path; cbSize is the fixed
        // part, as the API requires.
        let mut storage = vec![0_u64; 512];
        let detail = storage
            .as_mut_ptr()
            .cast::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>();
        // SAFETY: `storage` is 4 KiB, aligned for the structure.
        unsafe {
            (*detail).cbSize =
                u32::try_from(size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>()).unwrap_or(0);
        }
        let mut info = devinfo_data();
        // SAFETY: the buffer size passed matches `storage`.
        let detailed = unsafe {
            SetupDiGetDeviceInterfaceDetailW(
                set.0,
                &raw const data,
                Some(detail),
                4096,
                None,
                Some(&raw mut info),
            )
        };
        if detailed.is_err() {
            continue;
        }
        if instance_id(&set, &info).is_some_and(|id| id.eq_ignore_ascii_case(instance)) {
            // SAFETY: DevicePath is a NUL-terminated string inside `storage`.
            let path = unsafe { PCWSTR((*detail).DevicePath.as_ptr()).to_string() }.ok()?;
            return Some(path);
        }
    }
    None
}
