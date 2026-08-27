//! Experimental raw-HID-over-IP passthrough for supported drawing tablets
//! (Wacom, Huion, XP-Pen, UC-Logic, Gaomon).
//!
//! # Quarantine (SEC-raw-hid)
//!
//! This is **not** enabled in production or default builds. The whole module
//! is compiled only when the crate is built with the `experimental-raw-hid`
//! Cargo feature. Even then, actually starting capture additionally requires
//! BOTH:
//!   1. an explicit local runtime opt-in (see
//!      [`experimental_raw_hid_client_opt_in`]), and
//!   2. a mutually negotiated `experimental_raw_hid` wire capability with the
//!      connected host (both peers must independently advertise/require it;
//!      see `arcen_protocol::messages::ClientHelloMsg`/`ServerHelloMsg`).
//!
//! Default builds and old peers must never send or accept raw HID data. This
//! module captures raw IOHID input reports directly from the local HID
//! subsystem — it is not, and must never be described as, "USB bridging":
//! no USB transport/enumeration is exposed or forwarded, only vendor-scoped
//! HID input reports for a small allow-listed set of tablet vendors.
#[cfg(target_os = "macos")]
pub mod iokit;
pub mod session;

pub use session::{HidEvent, HidSession, HID_EVENT_CHANNEL_CAPACITY};

/// Environment variable that can switch the experimental raw-HID capture
/// path *off* in a build that compiled it in. Set it to `"0"` (or `"false"`)
/// to decline; any other value, including leaving it unset, keeps it on.
///
/// It used to work the other way round -- the path stayed off unless this was
/// set to `"1"` -- which made the build-time feature and this variable two
/// gates for one decision. That second gate is unreachable through the normal
/// way anyone launches a Mac app: macOS starts apps through LaunchServices,
/// which does not pass the shell environment, so double-clicking the bundle
/// silently produced a build with the capability compiled in and switched
/// off, with nothing to say why.
///
/// Compiling the feature in is now the opt-in, which is a decision someone
/// has to make deliberately at build time and which no shipped build makes.
pub const EXPERIMENTAL_RAW_HID_ENV: &str = "ARCEN_EXPERIMENTAL_RAW_HID";

/// Returns whether this client may attempt the experimental raw-HID capture
/// path.
///
/// This only reflects the *client's own* local permission. Whether capture
/// is actually started additionally requires the connected host to have
/// advertised its own `experimental_raw_hid` capability in `ServerHelloMsg`
/// — see the negotiation performed in `transport::websocket`.
pub fn experimental_raw_hid_client_opt_in() -> bool {
    parse_experimental_raw_hid_opt_in(std::env::var(EXPERIMENTAL_RAW_HID_ENV).ok().as_deref())
}

/// Pure, testable core of [`experimental_raw_hid_client_opt_in`]: enabled
/// unless explicitly declined with `"0"` or `"false"` (case-insensitive).
/// Reaching this function at all already means the crate was built with
/// `experimental-raw-hid`.
fn parse_experimental_raw_hid_opt_in(value: Option<&str>) -> bool {
    !matches!(
        value.map(str::trim).map(str::to_ascii_lowercase).as_deref(),
        Some("0") | Some("false")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Compiling the feature in is the opt-in. Unset must mean *on*, because
    /// macOS cannot deliver an environment variable to a double-clicked app:
    /// LaunchServices does not pass the shell environment, so a required
    /// variable is unreachable through the normal way the app is launched.
    #[test]
    fn capture_is_enabled_unless_explicitly_declined() {
        assert!(parse_experimental_raw_hid_opt_in(None));
        assert!(parse_experimental_raw_hid_opt_in(Some("")));
        assert!(parse_experimental_raw_hid_opt_in(Some("1")));
        assert!(parse_experimental_raw_hid_opt_in(Some("true")));
        assert!(parse_experimental_raw_hid_opt_in(Some("yes")));

        // The escape hatch still works, and tolerates the spellings and
        // stray whitespace a human actually types.
        assert!(!parse_experimental_raw_hid_opt_in(Some("0")));
        assert!(!parse_experimental_raw_hid_opt_in(Some("false")));
        assert!(!parse_experimental_raw_hid_opt_in(Some("FALSE")));
        assert!(!parse_experimental_raw_hid_opt_in(Some(" 0 ")));
    }
}
