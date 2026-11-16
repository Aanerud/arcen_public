// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Experimental raw-HID capture events (SEC-raw-hid; default-off feature
//! `experimental-raw-hid`, never in production builds). A platform's
//! capture thread sends these; the session engine frames them for the Pier.

/// Capacity of the capture-to-session channel.
pub const HID_EVENT_CHANNEL_CAPACITY: usize = 256;

/// Events a platform raw-HID capture produces.
#[derive(Debug)]
pub enum HidEvent {
    DeviceAdded {
        device_id: u8,
        vendor_id: u16,
        product_id: u16,
        descriptor: Vec<u8>,
    },
    DeviceRemoved {
        device_id: u8,
    },
    Report {
        device_id: u8,
        data: Vec<u8>,
    },
    PermissionGranted,
    Error {
        device_id: Option<u8>,
        reason_class: &'static str,
    },
}
