/// Canonical location of the corresponding source.
///
/// Arcen is AGPL-3.0 free software. Surfacing the source location from inside
/// the running program means a user who only ever receives a built binary can
/// still find the corresponding source.
///
/// This must point at the repository the public can actually reach. Development
/// happens in a private repository; publication is a one-way export to the
/// public one, and it is the public one that satisfies the offer.
pub const SOURCE_URL: &str = "https://github.com/Aanerud/arcen_public";

/// AGPL-3.0 section 13 source offer.
///
/// The Deck is the party that connects *to* a Pier over a network, so section
/// 13's obligation mostly runs the other way. It is surfaced here anyway,
/// because a user who received only this binary is entitled to the same
/// pointer, and because the About box and `--help` are the two places a person
/// looks for it.
pub const SOURCE_OFFER: &str =
    "Arcen is free software under the GNU AGPL-3.0. It comes with ABSOLUTELY NO WARRANTY. \
     You may redistribute it under the terms of that licence. If you run a modified version \
     that others connect to over a network, you must offer them its corresponding source.";

/// Release packaging scans the final binary for this marker and refuses to
/// ship it. It must stay compiled into every `dev-tools` binary, including
/// `--no-build` package assemblies, so stale local artefacts cannot become a
/// release.
#[cfg(feature = "dev-tools")]
#[used]
pub static ARCEN_DECK_DEV_TOOLS_FEATURE_MARKER: &[u8] =
    b"ARCEN_DECK_DEV_TOOLS_FEATURE_MARKER_DO_NOT_SHIP";

pub mod build_identity;
pub mod clipboard;
pub mod credentials;
pub mod display;
#[cfg(target_os = "macos")]
pub mod gestures;
#[cfg(feature = "experimental-raw-hid")]
pub mod hid;
pub mod logging;
pub mod microphone;
pub mod netinfo;
pub mod observability;
pub mod pipeline;
#[cfg(feature = "dev-tools")]
pub mod probe_matrix;
pub mod protocol;
pub mod reconnect;
pub mod tablet;
pub mod timezone;
pub mod transport;
pub mod ui;
#[cfg(feature = "usb-hard-lab")]
pub mod usb_bridge;
#[cfg(feature = "usb-hard-lab")]
pub mod usb_helper_client;
#[cfg(feature = "usb-hard-lab")]
pub mod usb_helper_install;
