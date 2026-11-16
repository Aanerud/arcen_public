// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The Windows half of the service: `SetupAPI`, trust, `WinUSB`, the pipe and
//! the service control manager. Everything decided is shared
//! (`arcen_usb_bridge::{winusb_package, component_session, component_ipc}`).
//!
//! This module is the crate's only FFI; every `unsafe` block names what it
//! relies on.
#![allow(unsafe_code)]

pub(crate) mod claim;
pub(crate) mod common;
pub(crate) mod devices;
pub(crate) mod pipe;
pub(crate) mod runtime;
pub(crate) mod scm;
pub(crate) mod trust;
pub(crate) mod winusb;

pub(crate) use common::{from_wide, secure_component_root, system_root, wide};

/// Where `uninstall` leaves its leftover list when it fails: beside, not
/// inside, the component directory it tried to remove.
pub(crate) fn uninstall_report_path() -> std::path::PathBuf {
    let arcen = std::env::var_os("ProgramData")
        .map_or_else(
            || std::path::PathBuf::from(r"C:\ProgramData"),
            std::path::PathBuf::from,
        )
        .join("Arcen");
    if arcen.is_dir() {
        arcen.join("usb-component-uninstall.txt")
    } else {
        system_root()
            .unwrap_or_else(|| std::path::PathBuf::from(r"C:\Windows"))
            .join("Temp")
            .join("arcen-usb-component-uninstall.txt")
    }
}
