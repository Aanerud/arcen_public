// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! A session's input, from a Deck's portable input events to the wire.
//!
//! Platform adapters read their window system through the shared policies
//! in `arcen-input` and emit [`arcen_input::deck_events::DeckInputEvent`]s;
//! [`region_runtime`] owns the region-authoritative pointer state and wire
//! encoding (shared by every Deck); [`pointer_drag`] follows a held-button
//! drag across Match My Layout's windows; [`viewport_pointer`] turns each
//! pointer event of a further window into a sample of its own.

pub mod overlay_pointer;
pub mod pointer_drag;
pub mod region_runtime;
#[cfg(feature = "runtime")]
mod session_input;
pub mod viewport_pointer;

#[cfg(feature = "runtime")]
pub use session_input::{RELATIVE_UNAVAILABLE, SessionInput};

/// What a session's input needs to know of its host, from its hello.
#[must_use]
pub fn host_input_facts(
    hello: &arcen_protocol::messages::ServerHelloMsg,
) -> arcen_input::deck_events::HostInputFacts {
    use arcen_protocol::messages::{
        InputCapabilityAvailability, supports_extended_keys_v1, supports_text_commit_v1,
    };
    let capabilities = hello.input_capabilities;
    arcen_input::deck_events::HostInputFacts {
        platform: hello.host_platform(),
        extended_keys: supports_extended_keys_v1(capabilities),
        precise_scroll: hello.precise_scroll_v1,
        text_commit: supports_text_commit_v1(capabilities),
        unicode_text: capabilities.text_commit_unicode == InputCapabilityAvailability::Available,
    }
}
