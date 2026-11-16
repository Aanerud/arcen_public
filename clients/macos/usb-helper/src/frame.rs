// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Length-prefixed framing for the Deck↔helper privilege boundary.
//!
//! The framing, its bounds and the handshake are shared with the Deck and
//! with the Windows USB component (`arcen_usb_bridge::component_ipc`), so the
//! two sides of every privilege boundary parse the same bytes the same way.
//! The URB payloads are the existing `arcen-protocol` frames, forwarded
//! verbatim. See `docs/adr/0011-macos-privileged-usb-helper.md`.

pub use arcen_usb_bridge::component_ipc::{
    encode_error, encode_hello, encode_info, hello_fits, read_frame, write_frame, ComponentHello,
    TAG_CANCEL, TAG_COMPLETE, TAG_ERROR, TAG_HELLO, TAG_INFO, TAG_SUBMIT,
};
