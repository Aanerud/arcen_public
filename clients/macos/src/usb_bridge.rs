//! Lab-gated Hard USB responders.
//!
//! Physical capture lives in the separate privileged `arcen-usb-helper`
//! process, which Deck reaches over a local socket; Deck itself never claims
//! the device and never needs root. Synthetic mode remains available behind an
//! explicit environment opt-in for deterministic bridge tests.
//!
//! See `docs/adr/0011-macos-privileged-usb-helper.md`.

use arcen_input::{PenEvent, PenTool};
use arcen_protocol::messages::UsbHardDeviceMsg;
use arcen_protocol::{encode_usb_urb_complete, UsbUrbCompletionHeader, UsbUrbSubmitHeader};
use arcen_usb_bridge::{
    ControlResponse, PenSample, PenSwitch, PenSwitches, SyntheticTabletDevice, TransferDirection,
    TransferKind, UrbId, UrbStatus, UsbSpeed, ARCEN_LAB_PRODUCT_ID, ARCEN_LAB_VENDOR_ID,
};

/// Result of admitting one host URB.
pub enum SubmitResult {
    Immediate(Vec<u8>),
    Pending,
}

/// One negotiated Hard USB responder.
pub struct UsbHardResponder {
    mode: ResponderMode,
}

enum ResponderMode {
    Synthetic(LabUsbTablet),
    /// Physical capture, performed by the privileged helper process. Deck never
    /// touches the device itself; see docs/adr/0011-macos-privileged-usb-helper.md.
    Helper(Box<crate::usb_helper_client::HelperClient>),
}

impl UsbHardResponder {
    /// Connects to the privileged helper for physical capture, or starts the
    /// deterministic synthetic responder when `ARCEN_USB_HARD_SYNTHETIC=1`.
    ///
    /// Deck deliberately does not capture the device in-process: that would
    /// require running the whole app as root, which Apple rejected and which
    /// was measured corrupting Deck's own configuration ownership.
    pub async fn start() -> Result<Self, String> {
        if std::env::var("ARCEN_USB_HARD_SYNTHETIC").ok().as_deref() == Some("1") {
            return Ok(Self {
                mode: ResponderMode::Synthetic(LabUsbTablet::default()),
            });
        }
        let socket = std::env::var("ARCEN_USB_HELPER_SOCKET")
            .unwrap_or_else(|_| crate::usb_helper_client::DEFAULT_SOCKET.to_owned());

        // Make sure launchd owns the helper before trying to reach it. This is
        // what keeps `sudo` out of the user's workflow: registration prompts
        // for an administrator once, and launchd starts the helper as root for
        // every session after that.
        ensure_helper_installed(&socket)?;

        let client = crate::usb_helper_client::HelperClient::connect(&socket).await?;
        tracing::info!(
            target: crate::logging::target::USB,
            socket = %socket,
            vendor_id = client.device().vendor_id,
            product_id = client.device().product_id,
            "privileged USB helper captured the device for Hard USB"
        );
        Ok(Self {
            mode: ResponderMode::Helper(Box::new(client)),
        })
    }

    #[must_use]
    pub const fn device(&self) -> UsbHardDeviceMsg {
        match &self.mode {
            ResponderMode::Synthetic(_) => UsbHardDeviceMsg {
                vendor_id: ARCEN_LAB_VENDOR_ID,
                product_id: ARCEN_LAB_PRODUCT_ID,
                bcd_device: 0x0100,
                device_class: 0,
                speed: UsbSpeed::High,
            },
            ResponderMode::Helper(client) => client.device(),
        }
    }

    pub fn update_pen(&mut self, event: PenEvent) {
        if let ResponderMode::Synthetic(tablet) = &mut self.mode {
            tablet.update_pen(event);
        }
    }

    pub async fn submit(
        &mut self,
        header: UsbUrbSubmitHeader,
        payload: &[u8],
    ) -> Result<SubmitResult, String> {
        match &mut self.mode {
            ResponderMode::Synthetic(tablet) => tablet
                .complete(header, payload)
                .map(SubmitResult::Immediate)
                .map_err(|error| format!("synthetic USB completion failed: {error:?}")),
            ResponderMode::Helper(client) => {
                client.submit(header, payload).await?;
                Ok(SubmitResult::Pending)
            }
        }
    }

    /// Cancels one URB. Synthetic mode answers inline; the helper's terminal
    /// completion arrives asynchronously through [`Self::next_completion`], so
    /// `None` here means "already routed", not "ignored".
    pub async fn cancel(
        &mut self,
        generation: arcen_usb_bridge::AttachmentGeneration,
        urb_id: UrbId,
    ) -> Result<Option<Vec<u8>>, String> {
        match &mut self.mode {
            ResponderMode::Synthetic(_) => cancelled_completion(generation, urb_id).map(Some),
            ResponderMode::Helper(client) => {
                client.cancel(generation, urb_id).await?;
                Ok(None)
            }
        }
    }

    #[must_use]
    pub const fn has_async_completions(&self) -> bool {
        matches!(self.mode, ResponderMode::Helper(_))
    }

    pub async fn next_completion(&mut self) -> Result<Vec<u8>, String> {
        match &mut self.mode {
            ResponderMode::Helper(client) => client.next_completion().await,
            ResponderMode::Synthetic(_) => std::future::pending().await,
        }
    }

    pub async fn shutdown(self) {
        if let ResponderMode::Helper(client) = self.mode {
            client.shutdown().await;
        }
    }
}

/// Ensures launchd owns the privileged helper before Deck tries to reach it.
///
/// Returns Ok when the helper is already usable — including the case where an
/// administrator started one manually, which is how the lab socket is used —
/// and otherwise registers the bundled daemon, raising one administrator
/// prompt. Errors carry the user-facing guidance for the state we ended in,
/// because "approval pending" and "not installed" need different actions and
/// neither is a bug.
fn ensure_helper_installed(socket: &str) -> Result<(), String> {
    use crate::usb_helper_install::{install_state, register, HelperInstallState};

    // An already-listening socket means a helper is running, whether launchd
    // started it or an administrator did. Don't register over that.
    if std::path::Path::new(socket).exists() {
        return Ok(());
    }

    let state = match install_state() {
        // macOS reports an unregistered daemon as NotFound (BTM "record not
        // found"), not NotRegistered, so both mean "register it".
        HelperInstallState::NotRegistered | HelperInstallState::NotFound => {
            tracing::info!(
                target: crate::logging::target::USB,
                "installing the privileged USB helper as a launchd daemon"
            );
            register()?
        }
        other => other,
    };

    if state.is_ready() {
        return Ok(());
    }
    if matches!(state, HelperInstallState::RequiresApproval) {
        crate::usb_helper_install::open_login_items_settings();
    }
    Err(state.guidance().to_owned())
}

fn cancelled_completion(
    generation: arcen_usb_bridge::AttachmentGeneration,
    urb_id: UrbId,
) -> Result<Vec<u8>, String> {
    encode_usb_urb_complete(
        UsbUrbCompletionHeader {
            generation,
            urb_id,
            status: UrbStatus::Cancelled,
            actual_length: 0,
        },
        &[],
    )
    .map_err(|error| format!("Hard USB cancellation failed: {error:?}"))
}

/// One session-scoped synthetic tablet.
struct LabUsbTablet {
    device: SyntheticTabletDevice,
    latest_report: [u8; 10],
}

impl Default for LabUsbTablet {
    fn default() -> Self {
        Self {
            device: SyntheticTabletDevice::default(),
            latest_report: [1, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        }
    }
}

impl LabUsbTablet {
    fn update_pen(&mut self, event: PenEvent) {
        let switches = PenSwitches::default()
            .with(PenSwitch::InRange, event.in_proximity)
            .with(PenSwitch::Touching, event.touching)
            .with(PenSwitch::Eraser, event.tool == PenTool::Eraser)
            .with(PenSwitch::Barrel, event.buttons & 1 != 0)
            .with(PenSwitch::SecondaryBarrel, event.buttons & 2 != 0);
        self.latest_report = PenSample {
            x: event.x,
            y: event.y,
            pressure: event.pressure,
            tilt_x_degrees: event.tilt_x_degrees,
            tilt_y_degrees: event.tilt_y_degrees,
            switches,
        }
        .encode_report();
    }

    fn complete(
        &mut self,
        header: UsbUrbSubmitHeader,
        payload: &[u8],
    ) -> Result<Vec<u8>, arcen_protocol::ProtocolError> {
        let (status, data) = match header.transfer_kind {
            TransferKind::Control => match self
                .device
                .handle_control(header.setup.expect("validated control URB has setup"))
            {
                ControlResponse::Ack => (UrbStatus::Success, Vec::new()),
                ControlResponse::Data(data) => (UrbStatus::Success, data),
                ControlResponse::Stall => (UrbStatus::Stall, Vec::new()),
            },
            TransferKind::Interrupt if header.endpoint.direction() == TransferDirection::In => {
                let maximum = usize::try_from(header.declared_length).unwrap_or(usize::MAX);
                (
                    UrbStatus::Success,
                    self.latest_report[..maximum.min(10)].to_vec(),
                )
            }
            TransferKind::Interrupt => (UrbStatus::Stall, Vec::new()),
        };
        let _ = payload;
        encode_usb_urb_complete(
            UsbUrbCompletionHeader {
                generation: header.generation,
                urb_id: header.urb_id,
                status,
                actual_length: u32::try_from(data.len()).unwrap_or(0),
            },
            &data,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcen_input::LowLatencyMetadata;
    use arcen_protocol::decode_usb_urb_complete;
    use arcen_usb_bridge::{AttachmentGeneration, EndpointAddress};
    use std::num::{NonZeroU32, NonZeroU64};

    #[test]
    fn live_pen_state_becomes_interrupt_report() {
        let mut tablet = LabUsbTablet::default();
        tablet.update_pen(PenEvent {
            x: 0.25,
            y: 0.75,
            pressure: 0.5,
            tilt_x_degrees: 10.0,
            tilt_y_degrees: -20.0,
            rotation_degrees: 0.0,
            tool: PenTool::Tip,
            in_proximity: true,
            touching: true,
            buttons: 1,
            metadata: LowLatencyMetadata {
                sequence: 1,
                timestamp_ns: 1,
                coalescable: true,
            },
        });
        let frame = tablet
            .complete(
                UsbUrbSubmitHeader {
                    generation: AttachmentGeneration::new(NonZeroU64::MIN),
                    urb_id: UrbId::new(NonZeroU32::MIN),
                    endpoint: EndpointAddress(0x81),
                    transfer_kind: TransferKind::Interrupt,
                    timeout_ms: 1_000,
                    declared_length: 10,
                    setup: None,
                },
                &[],
            )
            .unwrap();
        let (completion, report) = decode_usb_urb_complete(&frame).unwrap();
        assert_eq!(completion.status, UrbStatus::Success);
        assert_eq!(report.len(), 10);
        assert_eq!(report[1] & 0b1011, 0b1011);
    }

    // The physical-capture tests moved with the code they cover, to
    // `clients/macos/usb-helper/src/capture.rs`. Deck no longer captures.
}
