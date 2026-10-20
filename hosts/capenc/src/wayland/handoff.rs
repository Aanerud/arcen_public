//! The session-to-capenc hand-off for the Wayland provider.
//!
//! The Linux Pier's Wayland session adapter owns the compositor and its
//! `ScreenCast`/`RemoteDesktop` D-Bus sessions and hands capenc only what a
//! `PipeWire` consumer needs, through the environment of the spawned process.
//! The variable names and the one parser are the shared
//! [`arcen_media::video::capture_handoff`] contract; this module only reads
//! it from the process environment and refuses a missing provider. capenc
//! never talks D-Bus.

use std::fmt::{Display, Formatter};

pub use arcen_media::video::capture_handoff::{
    parse_pci_address, render_node_minor_from_name, CaptureHandoffError,
    PipeWireCaptureHandoff as WaylandHandoff, CAPTURE_PROVIDER_ENV,
};

/// Why the hand-off cannot be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandoffError {
    /// `ARCEN_CAPTURE_PROVIDER` is not set: this process was not started for
    /// a Wayland session.
    NotWayland,
    /// The shared contract refused a variable.
    Contract(CaptureHandoffError),
}

impl Display for HandoffError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotWayland => write!(formatter, "{CAPTURE_PROVIDER_ENV} is not set"),
            Self::Contract(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for HandoffError {}

/// Reads the hand-off through `lookup`, which returns a variable's value.
///
/// # Errors
///
/// [`HandoffError`] naming the missing provider or the first missing or
/// malformed variable.
pub fn from_lookup(
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<WaylandHandoff, HandoffError> {
    WaylandHandoff::from_env(lookup)
        .map_err(HandoffError::Contract)?
        .ok_or(HandoffError::NotWayland)
}

/// Reads the hand-off from the process environment.
///
/// # Errors
///
/// As [`from_lookup`].
pub fn from_process_environment() -> Result<WaylandHandoff, HandoffError> {
    from_lookup(|name| std::env::var(name).ok())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};

    use arcen_media::video::capture_handoff::{
        COMPOSITOR_RENDER_NODE_ENV, PIPEWIRE_NODE_ENV, RUNTIME_DIR_ENV,
    };
    use arcen_media::video::PciAddress;

    use super::*;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect();
        move |name| map.get(name).cloned()
    }

    /// Exactly what the Linux Pier's session adapter sets.
    const SESSION: &[(&str, &str)] = &[
        ("ARCEN_CAPTURE_PROVIDER", "wayland"),
        ("XDG_RUNTIME_DIR", "/run/user/1001/arcen-wayland-7"),
        ("ARCEN_PIPEWIRE_NODE", "47"),
        ("ARCEN_CAPTURE_OUTPUT", "Meta-0"),
        ("ARCEN_CAPTURE_OUTPUT_GENERATION", "3"),
        ("ARCEN_CAPTURE_EXPECTED_SIZE", "1920x1080"),
        ("ARCEN_CAPTURE_OUTPUT_POSITION", "0,0"),
        ("ARCEN_CAPTURE_CURSOR_MODE", "metadata"),
        ("ARCEN_CAPTURE_GPU_PCI", "0000:00:10.0"),
        ("ARCEN_COMPOSITOR_GPU_PCI", "0000:00:10.0"),
        ("ARCEN_COMPOSITOR_RENDER_NODE", "/dev/dri/renderD128"),
    ];

    #[test]
    fn the_session_adapters_handoff_parses() {
        let handoff = from_lookup(env(SESSION)).unwrap();
        let gpu = PciAddress::new(0, 0, 0x10, 0).unwrap();
        assert_eq!(handoff.node_id(), 47);
        assert_eq!(handoff.output().as_str(), "Meta-0");
        assert_eq!(handoff.generation().get(), 3);
        assert_eq!(
            handoff.pipewire_socket(),
            PathBuf::from("/run/user/1001/arcen-wayland-7/pipewire-0")
        );
        assert_eq!(handoff.encoder_gpu_pci(), Some(gpu));
        assert_eq!(handoff.compositor_gpu_pci(), Some(gpu));
        assert_eq!(
            handoff.compositor_render_node(),
            Some(Path::new("/dev/dri/renderD128"))
        );
        assert_eq!(handoff.size(), (1920, 1080));
    }

    #[test]
    fn missing_provider_and_bad_variables_are_refused() {
        let without_provider: Vec<_> = SESSION[1..].to_vec();
        assert_eq!(
            from_lookup(env(&without_provider)),
            Err(HandoffError::NotWayland)
        );
        for (name, value) in [
            (PIPEWIRE_NODE_ENV, "4294967295"),
            (RUNTIME_DIR_ENV, "relative/dir"),
            (COMPOSITOR_RENDER_NODE_ENV, "/dev/dri/card0"),
        ] {
            let pairs: Vec<_> = SESSION
                .iter()
                .map(|(key, current)| (*key, if *key == name { value } else { *current }))
                .collect();
            assert_eq!(
                from_lookup(env(&pairs)),
                Err(HandoffError::Contract(CaptureHandoffError::Invalid(name))),
                "{name}={value}"
            );
        }
        let pairs: Vec<_> = SESSION
            .iter()
            .filter(|(key, _)| *key != "ARCEN_CAPTURE_OUTPUT_GENERATION")
            .copied()
            .collect();
        assert_eq!(
            from_lookup(env(&pairs)),
            Err(HandoffError::Contract(CaptureHandoffError::Missing(
                "ARCEN_CAPTURE_OUTPUT_GENERATION"
            )))
        );
    }
}
