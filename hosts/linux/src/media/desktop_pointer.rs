// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Where a desktop's pointer is, for the input lane: the shared
//! [`PenPointerHub`] (ADR 0023) published on a `tokio` watch channel.
//!
//! Every Linux desktop has one: an Xorg desktop's per attachment (its
//! cursor watcher reads the X pointer), a Wayland desktop's in its cursor
//! hub (capture reports the compositor's core pointer). A USB-bridged pen
//! feeds the same hub from its own evdev node (`usb_bridge::tablet_pointer`),
//! on X11 and Wayland alike.

use arcen_input::host_pointer::DesktopPointerSample;
use arcen_input::tablet_pointer::{PenMapping, PenPointerHub};
use tokio::sync::watch;

/// One desktop's pointer.
#[derive(Debug)]
pub struct DesktopPointer {
    hub: PenPointerHub,
    tx: watch::Sender<Option<DesktopPointerSample>>,
}

impl DesktopPointer {
    /// A pointer whose desktop maps a tablet as `mapping`.
    #[must_use]
    pub fn new(mapping: PenMapping) -> Self {
        let tx = watch::channel(None).0;
        let sink = tx.clone();
        Self {
            hub: PenPointerHub::new(mapping, move |sample| {
                sink.send_replace(Some(sample));
            }),
            tx,
        }
    }

    /// Follows the published positions.
    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<Option<DesktopPointerSample>> {
        self.tx.subscribe()
    }

    /// Whether anyone follows.
    #[must_use]
    pub fn is_followed(&self) -> bool {
        !self.tx.is_closed()
    }
}

impl std::ops::Deref for DesktopPointer {
    type Target = PenPointerHub;

    fn deref(&self) -> &PenPointerHub {
        &self.hub
    }
}
