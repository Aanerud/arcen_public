//! Arcen wire protocol — the single source of truth for the on-wire
//! contract shared by Arcen Pier (Linux/Windows) and Arcen Deck (macOS).
//!
//! This crate replaces the legacy Python `common/messages.py` as the authority
//! for the wire. It is deliberately **host-agnostic**: no tokio, no transport,
//! no platform (macOS/Windows/Linux) crates, no I/O — only serde data types,
//! the binary media-framing codecs, the session state machine, and the
//! challenge/response auth hash. Every consumer (client + both hosts) depends on
//! this crate so the wire stays byte-identical across all three.
//!
//! Modules:
//! - [`wire`]     — binary media framing (video/audio headers) + [`wire::PROTOCOL_VERSION`].
//! - [`messages`] — JSON control messages (hellos, auth, health, input, …).
//! - [`fsm`]      — client + server session state-machine contract.
//! - [`auth`]     — SHA-256 challenge/response (`hash_password`, `generate_challenge`).
//!
//! Byte-compatibility and any change to the wire is governed by `WIRE.md` in this
//! crate. See it for the authoritative layout + dated changelog.

#![forbid(unsafe_code)]

pub mod auth;
pub mod clipboard;
pub mod fsm;
pub mod messages;
mod multi_monitor;
mod region_input;
pub mod wire;

pub use wire::{
    decode_audio_header, decode_clipboard_chunk, decode_hid_device_added,
    decode_hid_device_removed, decode_hid_report, decode_microphone_frame, decode_usb_urb_cancel,
    decode_usb_urb_complete, decode_usb_urb_submit, decode_video_header, encode_audio_header,
    encode_clipboard_chunk, encode_hid_device_added, encode_hid_device_removed, encode_hid_report,
    encode_microphone_header, encode_usb_urb_cancel, encode_usb_urb_complete,
    encode_usb_urb_submit, encode_video_header, AudioCodec, AudioHeader, ChromaSubsampling,
    ClipboardChunkHeader, FrameType, HidDeviceAddedHeader, MicrophoneHeader, ProtocolError,
    UsbUrbCompletionHeader, UsbUrbSubmitHeader, VideoCodec, VideoHeader, AUDIO_HEADER_SIZE,
    CHUNK_BYTES, CLIPBOARD_HEADER_SIZE, HARD_MAX_CLIPBOARD_BYTES, HID_DEVICE_ADDED_HEADER_SIZE,
    HID_MINIMAL_FRAME_SIZE, JPEG_HEADER_SIZE, MAX_MICROPHONE_OPUS_BYTES, MICROPHONE_HEADER_SIZE,
    MICROPHONE_PCM_BYTES, MICROPHONE_PROTOCOL_VERSION, PNG_MAGIC, PNG_MAX_BYTES, PROTOCOL_VERSION,
    REGION_VIDEO_HEADER_SIZE, USB_URB_CANCEL_SIZE, USB_URB_COMPLETE_HEADER_SIZE,
    USB_URB_SUBMIT_HEADER_SIZE, VIDEO_HEADER_SIZE, VIDEO_KEYFRAME_FLAG,
};

/// Canonical dormant secure-WebSocket compatibility capability.
#[cfg(feature = "wss-compat")]
pub const CAPABILITY_TRANSPORT_WSS: &str = "transport:wss-v1";
/// Canonical transport capability: QUIC profile.
pub const CAPABILITY_TRANSPORT_QUIC: &str = "transport:quic-v1";
/// Maximum transport capability identifiers accepted from one hello.
pub const MAX_TRANSPORT_CAPABILITIES: usize = 8;
/// Maximum byte length of a transport capability identifier.
pub const MAX_TRANSPORT_CAPABILITY_ID_BYTES: usize = 64;

fn is_known_transport_capability(capability: &str) -> bool {
    if capability == CAPABILITY_TRANSPORT_QUIC {
        return true;
    }
    #[cfg(feature = "wss-compat")]
    {
        capability == CAPABILITY_TRANSPORT_WSS
    }
    #[cfg(not(feature = "wss-compat"))]
    {
        false
    }
}

/// Returns a bounded, de-duplicated view of known transport capabilities.
#[must_use]
pub fn sanitize_transport_capabilities(capabilities: &[String]) -> Vec<&str> {
    let mut sanitized = Vec::with_capacity(capabilities.len().min(MAX_TRANSPORT_CAPABILITIES));
    for capability in capabilities {
        if capability.len() > MAX_TRANSPORT_CAPABILITY_ID_BYTES {
            continue;
        }
        let capability = capability.as_str();
        if !is_known_transport_capability(capability) {
            continue;
        }
        if !sanitized.contains(&capability) {
            sanitized.push(capability);
        }
        if sanitized.len() == MAX_TRANSPORT_CAPABILITIES {
            break;
        }
    }
    sanitized
}

/// Selects the best common transport capability from the client's offered list
/// and the host's supported list.
///
/// Returns `Some(capability_id)` for the highest-priority common entry, or
/// `None` when the lists are disjoint.
///
/// Product builds negotiate only `"transport:quic-v1"`. The dormant
/// `wss-compat` feature adds `"transport:wss-v1"` as a lower-priority option
/// for explicit compatibility builds.
///
/// # Example
///
/// ```rust
/// use arcen_protocol::negotiate_transport;
/// let negotiated = negotiate_transport(
///     &["transport:quic-v1"],  // client offers
///     &["transport:quic-v1"],  // host supports
/// );
/// assert_eq!(negotiated, Some("transport:quic-v1"));
/// ```
#[must_use]
pub fn negotiate_transport<'a>(
    client_capabilities: &[&'a str],
    host_capabilities: &[&'a str],
) -> Option<&'a str> {
    if client_capabilities.contains(&CAPABILITY_TRANSPORT_QUIC)
        && host_capabilities.contains(&CAPABILITY_TRANSPORT_QUIC)
    {
        return Some(CAPABILITY_TRANSPORT_QUIC);
    }
    #[cfg(feature = "wss-compat")]
    if client_capabilities.contains(&CAPABILITY_TRANSPORT_WSS)
        && host_capabilities.contains(&CAPABILITY_TRANSPORT_WSS)
    {
        return Some(CAPABILITY_TRANSPORT_WSS);
    }
    None
}

#[cfg(test)]
mod negotiate_tests {
    use super::*;

    #[test]
    fn prefers_quic_when_both_support_it() {
        #[cfg(feature = "wss-compat")]
        assert_eq!(
            negotiate_transport(
                &[CAPABILITY_TRANSPORT_WSS, CAPABILITY_TRANSPORT_QUIC],
                &[CAPABILITY_TRANSPORT_QUIC, CAPABILITY_TRANSPORT_WSS],
            ),
            Some(CAPABILITY_TRANSPORT_QUIC)
        );
        #[cfg(not(feature = "wss-compat"))]
        assert_eq!(
            negotiate_transport(&[CAPABILITY_TRANSPORT_QUIC], &[CAPABILITY_TRANSPORT_QUIC],),
            Some(CAPABILITY_TRANSPORT_QUIC)
        );
    }

    #[test]
    #[cfg(feature = "wss-compat")]
    fn falls_back_to_wss_when_client_has_no_quic() {
        assert_eq!(
            negotiate_transport(
                &[CAPABILITY_TRANSPORT_WSS],
                &[CAPABILITY_TRANSPORT_QUIC, CAPABILITY_TRANSPORT_WSS],
            ),
            Some(CAPABILITY_TRANSPORT_WSS)
        );
    }

    #[test]
    #[cfg(feature = "wss-compat")]
    fn returns_none_when_no_common_capability() {
        assert_eq!(
            negotiate_transport(&[CAPABILITY_TRANSPORT_QUIC], &[CAPABILITY_TRANSPORT_WSS]),
            None
        );
    }

    #[test]
    fn legacy_client_empty_list_returns_none() {
        #[cfg(feature = "wss-compat")]
        assert_eq!(
            negotiate_transport(&[], &[CAPABILITY_TRANSPORT_WSS, CAPABILITY_TRANSPORT_QUIC]),
            None
        );
        #[cfg(not(feature = "wss-compat"))]
        assert_eq!(negotiate_transport(&[], &[CAPABILITY_TRANSPORT_QUIC]), None);
    }

    #[test]
    fn sanitize_transport_capabilities_deduplicates_and_filters_unknown() {
        #[cfg(feature = "wss-compat")]
        let offered = vec![
            CAPABILITY_TRANSPORT_WSS.to_string(),
            "transport:unknown".to_string(),
            CAPABILITY_TRANSPORT_WSS.to_string(),
            CAPABILITY_TRANSPORT_QUIC.to_string(),
        ];
        #[cfg(not(feature = "wss-compat"))]
        let offered = vec![
            "transport:wss-v1".to_string(),
            "transport:unknown".to_string(),
            CAPABILITY_TRANSPORT_QUIC.to_string(),
            CAPABILITY_TRANSPORT_QUIC.to_string(),
        ];
        #[cfg(feature = "wss-compat")]
        assert_eq!(
            sanitize_transport_capabilities(&offered),
            vec![CAPABILITY_TRANSPORT_WSS, CAPABILITY_TRANSPORT_QUIC]
        );
        #[cfg(not(feature = "wss-compat"))]
        assert_eq!(
            sanitize_transport_capabilities(&offered),
            vec![CAPABILITY_TRANSPORT_QUIC]
        );
    }

    #[test]
    fn sanitize_transport_capabilities_rejects_oversize_ids() {
        let offered = vec!["x".repeat(MAX_TRANSPORT_CAPABILITY_ID_BYTES + 1)];
        assert!(sanitize_transport_capabilities(&offered).is_empty());
    }
}
