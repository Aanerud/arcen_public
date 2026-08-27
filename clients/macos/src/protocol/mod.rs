//! Client-side protocol facade.
//!
//! The wire types now live in the standalone, host-agnostic
//! [`arcen_protocol`] crate (the single source of truth shared with the
//! native hosts). This module is a thin re-export shim so existing client call
//! sites keep using `crate::protocol::…` unchanged, plus it houses the one
//! genuinely client-side concern that is NOT a wire type: [`keymap`] (the
//! platform key → Linux evdev scancode translation and Flame chord table).
//!
//! To evolve the wire, edit `crates/arcen-protocol`, not this file.

pub mod keymap;

pub use arcen_protocol::{auth, fsm, messages, wire};

pub use arcen_protocol::{
    decode_audio_header, decode_clipboard_chunk, decode_video_header, encode_audio_header,
    encode_clipboard_chunk, encode_video_header, AudioCodec, AudioHeader, ChromaSubsampling,
    ClipboardChunkHeader, FrameType, ProtocolError, VideoCodec, VideoHeader, AUDIO_HEADER_SIZE,
    CHUNK_BYTES, CLIPBOARD_HEADER_SIZE, HARD_MAX_CLIPBOARD_BYTES, JPEG_HEADER_SIZE, PNG_MAGIC,
    PNG_MAX_BYTES, PROTOCOL_VERSION, REGION_VIDEO_HEADER_SIZE, VIDEO_HEADER_SIZE,
    VIDEO_KEYFRAME_FLAG,
};
