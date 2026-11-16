// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The `arcen-protocol` URB frames the Deck forwards verbatim, decoded and
//! encoded for the shared component session.

use arcen_protocol::{
    UsbUrbCompletionHeader, decode_usb_urb_cancel, decode_usb_urb_submit, encode_usb_urb_complete,
};
use arcen_usb_bridge::component_session::UrbCodec;
use arcen_usb_bridge::{AttachmentGeneration, ExportedCompletion, UrbId, UrbSubmission};

/// [`UrbCodec`] over `arcen-protocol`.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ProtocolUrbCodec;

impl UrbCodec for ProtocolUrbCodec {
    fn decode_submit<'a>(&self, frame: &'a [u8]) -> Result<(UrbSubmission, &'a [u8]), String> {
        let (header, body) =
            decode_usb_urb_submit(frame).map_err(|error| format!("invalid submit: {error:?}"))?;
        Ok((
            UrbSubmission {
                generation: header.generation,
                urb_id: header.urb_id,
                endpoint: header.endpoint,
                transfer_kind: header.transfer_kind,
                timeout_ms: header.timeout_ms,
                declared_length: header.declared_length,
                setup: header.setup,
            },
            body,
        ))
    }

    fn decode_cancel(&self, frame: &[u8]) -> Result<(AttachmentGeneration, UrbId), String> {
        decode_usb_urb_cancel(frame).map_err(|error| format!("invalid cancel: {error:?}"))
    }

    fn encode_completion(&self, completion: &ExportedCompletion) -> Result<Vec<u8>, String> {
        let actual_length = u32::try_from(completion.data.len())
            .map_err(|_| "completion length overflow".to_owned())?;
        encode_usb_urb_complete(
            UsbUrbCompletionHeader {
                generation: completion.generation,
                urb_id: completion.urb_id,
                status: completion.status,
                actual_length,
            },
            &completion.data,
        )
        .map_err(|error| format!("encode completion: {error:?}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcen_protocol::{UsbUrbSubmitHeader, encode_usb_urb_cancel, encode_usb_urb_submit};
    use arcen_usb_bridge::{EndpointAddress, TransferKind, UrbStatus};
    use std::num::{NonZeroU32, NonZeroU64};

    #[test]
    fn the_decks_frames_round_trip_through_the_codec() {
        let generation = AttachmentGeneration::new(NonZeroU64::new(3).expect("non-zero"));
        let urb = UrbId::new(NonZeroU32::new(9).expect("non-zero"));
        let frame = encode_usb_urb_submit(
            UsbUrbSubmitHeader {
                generation,
                urb_id: urb,
                endpoint: EndpointAddress(0x81),
                transfer_kind: TransferKind::Interrupt,
                timeout_ms: 1000,
                declared_length: 64,
                setup: None,
            },
            &[],
        )
        .expect("encodes");
        let (submission, body) = ProtocolUrbCodec.decode_submit(&frame).expect("decodes");
        assert_eq!(submission.urb_id, urb);
        assert_eq!(submission.endpoint, EndpointAddress(0x81));
        assert!(body.is_empty());
        assert_eq!(
            ProtocolUrbCodec
                .decode_cancel(&encode_usb_urb_cancel(generation, urb))
                .expect("decodes"),
            (generation, urb)
        );
        let completion = ProtocolUrbCodec
            .encode_completion(&ExportedCompletion {
                generation,
                urb_id: urb,
                status: UrbStatus::Success,
                data: vec![1, 2, 3],
            })
            .expect("encodes");
        assert!(completion.ends_with(&[1, 2, 3]));
        assert!(ProtocolUrbCodec.decode_submit(&[0; 3]).is_err());
        assert!(ProtocolUrbCodec.decode_cancel(&[0; 3]).is_err());
    }
}
