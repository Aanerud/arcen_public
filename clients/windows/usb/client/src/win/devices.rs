// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Which pen tablets are attached, as an ordinary user can see it: every
//! present HID interface whose top-level collection names a pen digitizer
//! (`arcen_usb_bridge::names_pen_digitizer`: Digitizer, Pen or Light Pen on
//! the Digitizers page or its vendor mirror `0xFF0D`), whoever made it
//! (Q-16), walked up the device tree to the USB device it belongs to. The
//! service decides admission on the descriptors it reads itself; this only
//! tells the Deck what to offer.

use arcen_usb_bridge::winusb_package::{AttachedTablet, TabletModel, is_device_instance_id};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_GET_DEVICE_INTERFACE_LIST_PRESENT, CM_Get_Device_IDW, CM_Get_Device_Interface_List_SizeW,
    CM_Get_Device_Interface_ListW, CM_Get_Device_Interface_PropertyW, CM_Get_Parent,
    CM_LOCATE_DEVNODE_NORMAL, CM_Locate_DevNodeW, CR_SUCCESS,
};
use windows::Win32::Devices::HumanInterfaceDevice::{
    HIDP_CAPS, HidD_FreePreparsedData, HidD_GetPreparsedData, HidP_GetCaps, PHIDP_PREPARSED_DATA,
};
use windows::Win32::Devices::Properties::{DEVPKEY_Device_InstanceId, DEVPROPTYPE};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::core::{GUID, PCWSTR};

use super::{OwnedHandle, from_wide, wide};

/// `GUID_DEVINTERFACE_HID`.
const HID_INTERFACE: GUID = GUID::from_u128(0x4d1e_55b2_f16f_11cf_88cb_0011_1100_0030);
/// Device-tree levels walked from a HID collection to its USB device.
const MAX_DEPTH: usize = 4;

/// Present tablets, one per USB device, in device-tree order.
pub(crate) fn attached_tablets() -> Vec<AttachedTablet> {
    let mut tablets: Vec<AttachedTablet> = Vec::new();
    for interface in hid_interfaces() {
        if !is_digitizer(&interface) {
            continue;
        }
        let Some(tablet) = usb_device_of(&interface) else {
            continue;
        };
        if !tablets
            .iter()
            .any(|known| known.instance_id.eq_ignore_ascii_case(&tablet.instance_id))
        {
            tablets.push(tablet);
        }
    }
    tablets
}

/// Paths of every present HID interface.
fn hid_interfaces() -> Vec<Vec<u16>> {
    let mut length = 0_u32;
    // SAFETY: a size query for the present HID interfaces.
    let status = unsafe {
        CM_Get_Device_Interface_List_SizeW(
            &raw mut length,
            &HID_INTERFACE,
            PCWSTR::null(),
            CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
        )
    };
    if status != CR_SUCCESS || length == 0 {
        return Vec::new();
    }
    let mut buffer = vec![0_u16; usize::try_from(length).unwrap_or(0)];
    // SAFETY: the buffer holds `length` UTF-16 units.
    let status = unsafe {
        CM_Get_Device_Interface_ListW(
            &HID_INTERFACE,
            PCWSTR::null(),
            &mut buffer,
            CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
        )
    };
    if status != CR_SUCCESS {
        return Vec::new();
    }
    buffer
        .split(|&unit| unit == 0)
        .filter(|path| !path.is_empty())
        .map(|path| path.iter().copied().chain(std::iter::once(0)).collect())
        .collect()
}

/// Whether the interface's top-level collection is a pen digitizer. Opened with
/// no access rights, which is enough for the collection's capabilities and
/// works on devices another driver holds exclusively.
fn is_digitizer(path: &[u16]) -> bool {
    // SAFETY: a NUL-terminated interface path; zero access, shared.
    let Ok(handle) = (unsafe {
        CreateFileW(
            PCWSTR(path.as_ptr()),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            HANDLE::default(),
        )
    }) else {
        return false;
    };
    let handle = OwnedHandle(handle);
    let mut data = PHIDP_PREPARSED_DATA::default();
    // SAFETY: a HID collection handle; the data is freed below.
    if !unsafe { HidD_GetPreparsedData(handle.0, &raw mut data) }.as_bool() {
        return false;
    }
    let mut caps = HIDP_CAPS::default();
    // SAFETY: valid preparsed data from the call above.
    let ok = unsafe { HidP_GetCaps(data, &raw mut caps) }.is_ok();
    // SAFETY: frees the preparsed data once.
    let _ = unsafe { HidD_FreePreparsedData(data) };
    ok && arcen_usb_bridge::names_pen_digitizer(caps.UsagePage, caps.Usage)
}

/// The USB device (not an interface: no `&MI_`) a HID collection belongs to.
fn usb_device_of(path: &[u16]) -> Option<AttachedTablet> {
    let mut kind = DEVPROPTYPE::default();
    let mut buffer = [0_u16; 512];
    let mut size = u32::try_from(size_of_val(&buffer)).ok()?;
    // SAFETY: reads one string property into a buffer of `size` bytes.
    let status = unsafe {
        CM_Get_Device_Interface_PropertyW(
            PCWSTR(path.as_ptr()),
            &DEVPKEY_Device_InstanceId,
            &raw mut kind,
            Some(buffer.as_mut_ptr().cast()),
            &raw mut size,
            0,
        )
    };
    if status != CR_SUCCESS {
        return None;
    }
    let id = wide(&from_wide(&buffer));
    let mut node = 0_u32;
    // SAFETY: locates the collection's device node by its instance ID.
    if unsafe { CM_Locate_DevNodeW(&raw mut node, PCWSTR(id.as_ptr()), CM_LOCATE_DEVNODE_NORMAL) }
        != CR_SUCCESS
    {
        return None;
    }
    for _ in 0..MAX_DEPTH {
        let mut parent = 0_u32;
        // SAFETY: walks one level up from a located node.
        if unsafe { CM_Get_Parent(&raw mut parent, node, 0) } != CR_SUCCESS {
            return None;
        }
        node = parent;
        let mut id = [0_u16; 512];
        // SAFETY: the buffer is a valid slice.
        if unsafe { CM_Get_Device_IDW(node, &mut id, 0) } != CR_SUCCESS {
            return None;
        }
        let id = from_wide(&id);
        if let Some(model) = TabletModel::from_device_id(&id)
            && is_device_instance_id(&id)
        {
            return Some(AttachedTablet {
                instance_id: id,
                model,
            });
        }
    }
    None
}
