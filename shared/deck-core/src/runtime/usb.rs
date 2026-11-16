// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Native tablet seam: the platform's captured USB tablet, as the session
//! engine drives it (ADR 0011; macOS: the privileged helper; Windows: the
//! optional Arcen USB component).

use std::future::Future;
use std::pin::Pin;

use arcen_input::PenEvent;
use arcen_protocol::UsbUrbSubmitHeader;
use arcen_protocol::messages::UsbHardDeviceMsg;

/// A boxed, sendable future, so the seam stays object-safe.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// What submitting one URB produced.
#[derive(Debug)]
pub enum UsbSubmitResult {
    /// The completion frame, ready to send.
    Immediate(Vec<u8>),
    /// The completion arrives later through
    /// [`UsbTabletResponder::next_completion`].
    Pending,
}

/// The captured tablet that answers the Pier's URBs.
pub trait UsbTabletResponder: Send {
    /// The device the hello advertises (ids, speed, HID report descriptors).
    fn device(&self) -> UsbHardDeviceMsg;

    /// Feeds a typed pen sample to a synthetic (lab) device; real devices
    /// ignore it.
    fn update_pen(&mut self, event: PenEvent);

    /// Submits one URB from the Pier.
    ///
    /// # Errors
    ///
    /// A reason for the log; the session drops the bridge.
    fn submit<'a>(
        &'a mut self,
        header: UsbUrbSubmitHeader,
        payload: &'a [u8],
    ) -> BoxFuture<'a, Result<UsbSubmitResult, String>>;

    /// Cancels an outstanding URB; an immediate completion when the device
    /// answers synchronously.
    ///
    /// # Errors
    ///
    /// A reason for the log.
    fn cancel(
        &mut self,
        generation: arcen_usb_bridge::AttachmentGeneration,
        urb_id: arcen_usb_bridge::UrbId,
    ) -> BoxFuture<'_, Result<Option<Vec<u8>>, String>>;

    /// Whether completions arrive asynchronously.
    fn has_async_completions(&self) -> bool;

    /// The next asynchronous completion frame (pending forever when there
    /// are none).
    ///
    /// # Errors
    ///
    /// The capture ended.
    fn next_completion(&mut self) -> BoxFuture<'_, Result<Vec<u8>, String>>;

    /// Releases the device back to the operating system.
    fn shutdown(self: Box<Self>) -> BoxFuture<'static, ()>;
}
