//! Linux desktop-provider selection policy.
//!
//! A Linux Pier chooses its desktop provider from host facts, not from a
//! manual opt-in, and never from a distribution name:
//!
//! - an Xorg server is installed → the dedicated Xorg session (the existing
//!   path; an Xorg installation is never moved to Wayland);
//! - no Xorg server, and a headless Wayland compositor with an accessible
//!   DRM render node and a valid host qualification record
//!   ([`crate::wayland_qualification`]) proving every session fact →
//!   headless Wayland;
//! - no Xorg server and no DRM render node → refused, naming what to
//!   install (a GPU driver with DRM/KMS). Arcen never changes driver
//!   configuration itself.
//!
//! `capture.provider` is an operator override: `auto` (the default),
//! `xorg` or `wayland`. An override that cannot be honoured refuses to start
//! and names why; nothing ever falls back silently.
//!
//! `capture.provider` is validated twice, both times fail-closed: an unknown
//! token is a configuration load error (serde), and
//! [`validate_desktop_provider_for`] refuses a value its Pier cannot honour —
//! any value on Windows or macOS, and `wayland` on a Linux build without the
//! Wayland adapter.
//!
//! Pure policy. The Linux Pier and installer gather the facts; nothing here
//! probes a compositor, D-Bus, `PipeWire`, a render node or an input stack.

use std::fmt::{Display, Formatter};

use serde::{Deserialize, Serialize};

use crate::wayland_qualification::QualificationError;

/// Where an Xorg server is installed when the configured
/// `platform.session.xorg_bin` has none: the locations distributions ship it
/// at, checked as facts by the Pier at start and by the installer.
pub const WELL_KNOWN_XORG_SERVERS: &[&str] = &[
    "/usr/libexec/Xorg",
    "/usr/lib/xorg/Xorg",
    "/usr/lib/Xorg",
    "/usr/bin/Xorg",
];

/// Desktop provider a Linux Pier runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesktopProvider {
    /// Arcen-managed dedicated Xorg session.
    Xorg,
    /// Arcen-managed headless Wayland session.
    Wayland,
}

impl DesktopProvider {
    /// Every provider.
    pub const ALL: &'static [Self] = &[Self::Xorg, Self::Wayland];

    /// Stable token (the launcher's `--desktop-provider`).
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Xorg => "xorg",
            Self::Wayland => "wayland",
        }
    }

    /// Parses a token.
    #[must_use]
    pub fn from_token(value: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|provider| provider.token() == value)
    }
}

/// `capture.provider`: how the Linux Pier chooses its provider.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderPreference {
    /// From host facts (the default).
    #[default]
    Auto,
    /// Operator override: dedicated Xorg.
    Xorg,
    /// Operator override: headless Wayland.
    Wayland,
}

impl ProviderPreference {
    /// Every preference.
    pub const ALL: &'static [Self] = &[Self::Auto, Self::Xorg, Self::Wayland];

    /// Stable configuration token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Xorg => "xorg",
            Self::Wayland => "wayland",
        }
    }
}

/// The Pier product validating its configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PierPlatform {
    /// Linux Pier.
    Linux {
        /// Whether this build contains a working Wayland provider adapter.
        wayland_adapter_built: bool,
    },
    /// Windows Pier.
    Windows,
    /// macOS Pier.
    Macos,
}

/// A `capture.provider` value this Pier cannot honour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesktopProviderConfigError {
    /// The key is set on a Pier that has no desktop-provider choice.
    NotLinux {
        /// Configured value.
        configured: ProviderPreference,
    },
    /// `wayland` on a Linux Pier built without the Wayland adapter.
    WaylandNotBuilt,
}

impl Display for DesktopProviderConfigError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotLinux { configured } => write!(
                formatter,
                "capture.provider applies only to the Linux Pier; remove \"{}\" from this \
                 configuration",
                configured.token()
            ),
            Self::WaylandNotBuilt => formatter.write_str(
                "capture.provider is \"wayland\", but this Pier build has no Wayland provider; \
                 remove the key or set it to \"auto\" or \"xorg\"",
            ),
        }
    }
}

impl std::error::Error for DesktopProviderConfigError {}

/// Validates the configured preference for `platform`. Absent means `auto`
/// on Linux and is the only accepted value elsewhere.
///
/// # Errors
///
/// [`DesktopProviderConfigError`] for any value on Windows or macOS, and for
/// `wayland` on a Linux build without the adapter.
pub const fn validate_desktop_provider_for(
    platform: PierPlatform,
    configured: Option<ProviderPreference>,
) -> Result<ProviderPreference, DesktopProviderConfigError> {
    match (platform, configured) {
        (PierPlatform::Windows | PierPlatform::Macos, Some(configured)) => {
            Err(DesktopProviderConfigError::NotLinux { configured })
        }
        (
            PierPlatform::Linux {
                wayland_adapter_built: false,
            },
            Some(ProviderPreference::Wayland),
        ) => Err(DesktopProviderConfigError::WaylandNotBuilt),
        (_, Some(configured)) => Ok(configured),
        (_, None) => Ok(ProviderPreference::Auto),
    }
}

/// One capability fact, as the Pier or installer established it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CapabilityFact {
    /// Proven present and usable.
    Available,
    /// Proven absent or unusable.
    Missing,
    /// Not established. Never counts as available.
    #[default]
    Unknown,
}

/// The separate facts a headless Wayland provider needs. Package presence
/// alone establishes none of them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WaylandCapabilityFacts {
    /// A supported compositor that can run headless with virtual outputs.
    pub compositor: CapabilityFact,
    /// A per-user session service (runtime directory, D-Bus) Arcen can own.
    pub session_service: CapabilityFact,
    /// `PipeWire` screencast available to that session.
    pub pipewire: CapabilityFact,
    /// An unprivileged session can open a DRM render node: the GPU driver
    /// has DRM/KMS, so the compositor can render on the GPU.
    pub render_node_access: CapabilityFact,
    /// A truthful capture route (a labelled host copy or a GPU import)
    /// delivered an encoded first frame. This is what the eight-bit Auto and
    /// Speed contracts need; Speed's rate qualification is separate.
    pub capture_route: CapabilityFact,
    /// The driver imports compositor buffers for the encoder on the GPU
    /// (DMA-BUF modifiers, modesetting) without a host copy. Reported, not
    /// required for Auto/Speed; a zero-copy or fidelity claim needs it.
    pub gpu_import: CapabilityFact,
    /// Session-bound input injection into that compositor.
    pub input_injection: CapabilityFact,
}

/// A missing Wayland prerequisite.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaylandBlocker {
    /// No usable headless compositor.
    Compositor(CapabilityFact),
    /// No session service.
    SessionService(CapabilityFact),
    /// No `PipeWire` screencast.
    PipeWire(CapabilityFact),
    /// No DRM render node the session can open.
    RenderNodeAccess(CapabilityFact),
    /// No capture route has delivered an encoded frame.
    CaptureRoute(CapabilityFact),
    /// No session-bound input injection.
    InputInjection(CapabilityFact),
}

impl WaylandBlocker {
    /// Stable token for telemetry and installer output.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Compositor(_) => "compositor",
            Self::SessionService(_) => "session_service",
            Self::PipeWire(_) => "pipewire",
            Self::RenderNodeAccess(_) => "render_node_access",
            Self::CaptureRoute(_) => "capture_route",
            Self::InputInjection(_) => "input_injection",
        }
    }

    /// Whether the fact was proven missing rather than unknown.
    #[must_use]
    pub const fn proven_missing(self) -> bool {
        matches!(
            self,
            Self::Compositor(CapabilityFact::Missing)
                | Self::SessionService(CapabilityFact::Missing)
                | Self::PipeWire(CapabilityFact::Missing)
                | Self::RenderNodeAccess(CapabilityFact::Missing)
                | Self::CaptureRoute(CapabilityFact::Missing)
                | Self::InputInjection(CapabilityFact::Missing)
        )
    }

    /// Operator-facing explanation.
    #[must_use]
    pub const fn explanation(self) -> &'static str {
        match self {
            Self::Compositor(_) => "no supported compositor can run a headless session here",
            Self::SessionService(_) => "no per-user session service is available for Arcen",
            Self::PipeWire(_) => "PipeWire screen capture is not available to the session",
            Self::RenderNodeAccess(_) => DRM_HINT,
            Self::CaptureRoute(_) => {
                "no capture route has delivered an encoded frame from the compositor (run \
                 `arcen-pier wayland qualify --write`)"
            }
            Self::InputInjection(_) => {
                "no session-bound input injection is proven (run `arcen-pier wayland qualify \
                 --write`)"
            }
        }
    }
}

impl WaylandCapabilityFacts {
    /// Every prerequisite not proven available, in a fixed order.
    #[must_use]
    pub fn blockers(&self) -> Vec<WaylandBlocker> {
        [
            (self.compositor, WaylandBlocker::Compositor as fn(_) -> _),
            (self.session_service, WaylandBlocker::SessionService),
            (self.pipewire, WaylandBlocker::PipeWire),
            (self.render_node_access, WaylandBlocker::RenderNodeAccess),
            (self.capture_route, WaylandBlocker::CaptureRoute),
            (self.input_injection, WaylandBlocker::InputInjection),
        ]
        .into_iter()
        .filter(|(fact, _)| *fact != CapabilityFact::Available)
        .map(|(fact, blocker)| blocker(fact))
        .collect()
    }
}

/// What a session-less host offers, as the Pier or installer established it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostDesktopFacts {
    /// An Xorg server executable is installed.
    pub xorg_server: bool,
    /// This Pier build contains the headless Wayland provider.
    pub wayland_built: bool,
    /// The Wayland facts, with the session-proven ones taken from a valid
    /// qualification record.
    pub wayland: WaylandCapabilityFacts,
    /// Why no valid qualification record supplied those facts, if so.
    pub qualification: Option<QualificationError>,
}

/// Why the provider was chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionReason {
    /// `auto` on a host with an Xorg server.
    XorgServerInstalled,
    /// `auto` on a host without Xorg whose Wayland stack is qualified.
    WaylandQualified,
    /// `capture.provider` named the provider.
    OperatorOverride,
}

impl SelectionReason {
    /// Stable token for logs and the installer.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::XorgServerInstalled => "xorg_server_installed",
            Self::WaylandQualified => "wayland_qualified",
            Self::OperatorOverride => "operator_override",
        }
    }
}

/// Selection result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopProviderSelection {
    /// Provider to run.
    pub provider: DesktopProvider,
    /// Why.
    pub reason: SelectionReason,
}

/// What an operator does about a host with no DRM render node.
/// Any vendor qualifies; a host without a hardware encoder is served in
/// software.
const DRM_HINT: &str = "no DRM render node is available: install a GPU driver with DRM/KMS (Intel \
                        i915 or xe, AMD amdgpu, virtio-gpu, or for NVIDIA the nvidia-drm module \
                        with modeset=1; a GPU without a hardware encoder is served in software); \
                        Arcen does not change driver configuration";

/// Why no provider can run. Never answered with the other provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderRefusal {
    /// No Xorg server, and either `xorg` was configured or this build has
    /// no Wayland provider.
    XorgMissing {
        /// `capture.provider = "xorg"`.
        requested: bool,
    },
    /// `auto`, no Xorg server, and no DRM render node.
    NoDisplayStack {
        /// The render-node fact.
        render_node: CapabilityFact,
    },
    /// Wayland is needed (`auto` without Xorg, or `wayland` configured) and
    /// not proven.
    WaylandUnavailable {
        /// `capture.provider = "wayland"`.
        requested: bool,
        /// Every unproven prerequisite.
        blockers: Vec<WaylandBlocker>,
        /// Why the qualification record supplied nothing, if so.
        qualification: Option<QualificationError>,
    },
}

impl ProviderRefusal {
    /// Stable tokens for logs and the installer.
    #[must_use]
    pub fn tokens(&self) -> Vec<&'static str> {
        match self {
            Self::XorgMissing { .. } => vec!["xorg_server"],
            Self::NoDisplayStack { .. } => vec!["xorg_server", "render_node_access"],
            Self::WaylandUnavailable {
                blockers,
                qualification,
                ..
            } => qualification
                .iter()
                .map(QualificationError::token)
                .chain(blockers.iter().map(|blocker| blocker.token()))
                .collect(),
        }
    }
}

impl Display for ProviderRefusal {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::XorgMissing { requested: true } => formatter.write_str(
                "capture.provider is \"xorg\" but no Xorg server is installed; install one or \
                 set capture.provider to \"auto\"",
            ),
            Self::XorgMissing { requested: false } => formatter.write_str(
                "no Xorg server is installed and this Pier build has no Wayland provider; \
                 install an Xorg server",
            ),
            Self::NoDisplayStack { .. } => write!(
                formatter,
                "no Xorg server is installed and {DRM_HINT}; with an Xorg server Arcen serves a \
                 dedicated X11 desktop, with DRM it serves a headless Wayland desktop"
            ),
            Self::WaylandUnavailable {
                requested,
                blockers,
                qualification,
            } => {
                if *requested {
                    formatter.write_str("capture.provider is \"wayland\" but")?;
                } else {
                    formatter.write_str("no Xorg server is installed, and")?;
                }
                formatter.write_str(" the headless Wayland desktop is unavailable:")?;
                if let Some(error) = qualification {
                    write!(formatter, " {error};")?;
                }
                for blocker in blockers {
                    if qualification.is_some()
                        && matches!(
                            blocker,
                            WaylandBlocker::CaptureRoute(_) | WaylandBlocker::InputInjection(_)
                        )
                    {
                        continue;
                    }
                    write!(formatter, " {};", blocker.explanation())?;
                }
                formatter.write_str(" Arcen does not fall back to another provider on its own")
            }
        }
    }
}

impl std::error::Error for ProviderRefusal {}

/// Selects the provider from the preference and host facts.
///
/// # Errors
///
/// [`ProviderRefusal`] naming what is missing; never the other provider.
pub fn select_desktop_provider(
    preference: ProviderPreference,
    facts: &HostDesktopFacts,
) -> Result<DesktopProviderSelection, ProviderRefusal> {
    let wayland = |requested: bool| {
        let blockers = facts.wayland.blockers();
        if blockers.is_empty() && facts.qualification.is_none() {
            Ok(DesktopProviderSelection {
                provider: DesktopProvider::Wayland,
                reason: if requested {
                    SelectionReason::OperatorOverride
                } else {
                    SelectionReason::WaylandQualified
                },
            })
        } else {
            Err(ProviderRefusal::WaylandUnavailable {
                requested,
                blockers,
                qualification: facts.qualification.clone(),
            })
        }
    };
    match preference {
        ProviderPreference::Xorg if facts.xorg_server => Ok(DesktopProviderSelection {
            provider: DesktopProvider::Xorg,
            reason: SelectionReason::OperatorOverride,
        }),
        ProviderPreference::Xorg => Err(ProviderRefusal::XorgMissing { requested: true }),
        ProviderPreference::Wayland => wayland(true),
        ProviderPreference::Auto if facts.xorg_server => Ok(DesktopProviderSelection {
            provider: DesktopProvider::Xorg,
            reason: SelectionReason::XorgServerInstalled,
        }),
        ProviderPreference::Auto if !facts.wayland_built => {
            Err(ProviderRefusal::XorgMissing { requested: false })
        }
        ProviderPreference::Auto
            if facts.wayland.render_node_access != CapabilityFact::Available =>
        {
            Err(ProviderRefusal::NoDisplayStack {
                render_node: facts.wayland.render_node_access,
            })
        }
        ProviderPreference::Auto => wayland(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_AVAILABLE: WaylandCapabilityFacts = WaylandCapabilityFacts {
        compositor: CapabilityFact::Available,
        session_service: CapabilityFact::Available,
        pipewire: CapabilityFact::Available,
        render_node_access: CapabilityFact::Available,
        capture_route: CapabilityFact::Available,
        gpu_import: CapabilityFact::Available,
        input_injection: CapabilityFact::Available,
    };

    fn host(xorg_server: bool, wayland: WaylandCapabilityFacts) -> HostDesktopFacts {
        HostDesktopFacts {
            xorg_server,
            wayland_built: true,
            wayland,
            qualification: None,
        }
    }

    #[test]
    fn auto_is_the_default_and_validated_per_platform() {
        let linux = PierPlatform::Linux {
            wayland_adapter_built: true,
        };
        assert_eq!(
            validate_desktop_provider_for(linux, None),
            Ok(ProviderPreference::Auto)
        );
        for preference in ProviderPreference::ALL {
            assert_eq!(
                validate_desktop_provider_for(linux, Some(*preference)),
                Ok(*preference)
            );
        }
        let without = PierPlatform::Linux {
            wayland_adapter_built: false,
        };
        let error =
            validate_desktop_provider_for(without, Some(ProviderPreference::Wayland)).unwrap_err();
        assert_eq!(error, DesktopProviderConfigError::WaylandNotBuilt);
        assert!(error.to_string().contains("no Wayland provider"));
        for preference in [ProviderPreference::Auto, ProviderPreference::Xorg] {
            assert_eq!(
                validate_desktop_provider_for(without, Some(preference)),
                Ok(preference)
            );
        }
    }

    #[test]
    fn other_piers_reject_any_provider_value() {
        for platform in [PierPlatform::Windows, PierPlatform::Macos] {
            assert_eq!(
                validate_desktop_provider_for(platform, None),
                Ok(ProviderPreference::Auto)
            );
            for configured in ProviderPreference::ALL {
                let error = validate_desktop_provider_for(platform, Some(*configured)).unwrap_err();
                assert!(error.to_string().contains("only to the Linux Pier"));
            }
        }
    }

    #[test]
    fn an_installed_xorg_server_keeps_x11_even_when_wayland_is_qualified() {
        let selection =
            select_desktop_provider(ProviderPreference::Auto, &host(true, ALL_AVAILABLE)).unwrap();
        assert_eq!(selection.provider, DesktopProvider::Xorg);
        assert_eq!(selection.reason, SelectionReason::XorgServerInstalled);
        // Nothing about Wayland matters then.
        let selection = select_desktop_provider(
            ProviderPreference::Auto,
            &host(true, WaylandCapabilityFacts::default()),
        )
        .unwrap();
        assert_eq!(selection.provider, DesktopProvider::Xorg);
    }

    #[test]
    fn without_xorg_a_qualified_drm_wayland_stack_is_selected() {
        let selection =
            select_desktop_provider(ProviderPreference::Auto, &host(false, ALL_AVAILABLE)).unwrap();
        assert_eq!(selection.provider, DesktopProvider::Wayland);
        assert_eq!(selection.reason, SelectionReason::WaylandQualified);
        assert_eq!(selection.reason.token(), "wayland_qualified");
    }

    #[test]
    fn without_xorg_or_drm_the_pier_refuses_and_names_the_driver_fix() {
        for render_node in [CapabilityFact::Missing, CapabilityFact::Unknown] {
            let facts = WaylandCapabilityFacts {
                render_node_access: render_node,
                ..ALL_AVAILABLE
            };
            let refusal =
                select_desktop_provider(ProviderPreference::Auto, &host(false, facts)).unwrap_err();
            assert_eq!(refusal, ProviderRefusal::NoDisplayStack { render_node });
            let message = refusal.to_string();
            assert!(message.contains("no Xorg server"), "{message}");
            assert!(
                message.contains("nvidia-drm module with modeset=1"),
                "{message}"
            );
            assert!(
                message.contains("does not change driver configuration"),
                "{message}"
            );
            assert_eq!(refusal.tokens(), vec!["xorg_server", "render_node_access"]);
        }
    }

    #[test]
    fn without_xorg_and_without_a_record_wayland_is_refused_naming_the_command() {
        let facts = HostDesktopFacts {
            wayland: WaylandCapabilityFacts {
                capture_route: CapabilityFact::Unknown,
                input_injection: CapabilityFact::Unknown,
                ..ALL_AVAILABLE
            },
            qualification: Some(QualificationError::Missing {
                path: "/var/lib/arcen/wayland-qualification.json".into(),
            }),
            ..host(false, ALL_AVAILABLE)
        };
        let refusal = select_desktop_provider(ProviderPreference::Auto, &facts).unwrap_err();
        let message = refusal.to_string();
        assert!(
            message.contains("arcen-pier wayland qualify --write"),
            "{message}"
        );
        assert!(message.contains("does not fall back"), "{message}");
        assert_eq!(
            refusal.tokens(),
            vec!["missing", "capture_route", "input_injection"]
        );
        // Even a record-less host with every fact would not run unqualified.
        let refusal = select_desktop_provider(
            ProviderPreference::Auto,
            &HostDesktopFacts {
                qualification: Some(QualificationError::UnsupportedSchema { found: 3 }),
                ..host(false, ALL_AVAILABLE)
            },
        )
        .unwrap_err();
        assert!(matches!(
            refusal,
            ProviderRefusal::WaylandUnavailable { .. }
        ));
    }

    #[test]
    fn a_build_without_wayland_on_a_host_without_xorg_says_so() {
        let refusal = select_desktop_provider(
            ProviderPreference::Auto,
            &HostDesktopFacts {
                wayland_built: false,
                ..host(false, ALL_AVAILABLE)
            },
        )
        .unwrap_err();
        assert_eq!(refusal, ProviderRefusal::XorgMissing { requested: false });
        assert!(refusal.to_string().contains("no Wayland provider"));
    }

    #[test]
    fn overrides_are_honoured_or_refused_loudly() {
        let selection =
            select_desktop_provider(ProviderPreference::Xorg, &host(true, ALL_AVAILABLE)).unwrap();
        assert_eq!(
            (selection.provider, selection.reason),
            (DesktopProvider::Xorg, SelectionReason::OperatorOverride)
        );
        let refusal =
            select_desktop_provider(ProviderPreference::Xorg, &host(false, ALL_AVAILABLE))
                .unwrap_err();
        assert_eq!(refusal, ProviderRefusal::XorgMissing { requested: true });

        // `wayland` with an Xorg server installed: honoured when proven.
        let selection =
            select_desktop_provider(ProviderPreference::Wayland, &host(true, ALL_AVAILABLE))
                .unwrap();
        assert_eq!(
            (selection.provider, selection.reason),
            (DesktopProvider::Wayland, SelectionReason::OperatorOverride)
        );
        let facts = WaylandCapabilityFacts {
            pipewire: CapabilityFact::Missing,
            render_node_access: CapabilityFact::Missing,
            ..ALL_AVAILABLE
        };
        let refusal =
            select_desktop_provider(ProviderPreference::Wayland, &host(true, facts)).unwrap_err();
        let ProviderRefusal::WaylandUnavailable { blockers, .. } = &refusal else {
            panic!("{refusal:?}");
        };
        assert_eq!(
            blockers,
            &vec![
                WaylandBlocker::PipeWire(CapabilityFact::Missing),
                WaylandBlocker::RenderNodeAccess(CapabilityFact::Missing),
            ]
        );
        assert!(blockers.iter().all(|blocker| blocker.proven_missing()));
        let message = refusal.to_string();
        assert!(
            message.contains("capture.provider is \"wayland\""),
            "{message}"
        );
        assert!(message.contains("PipeWire"), "{message}");
    }

    #[test]
    fn a_host_copy_route_is_enough_for_auto_and_speed_without_gpu_import() {
        for gpu_import in [CapabilityFact::Unknown, CapabilityFact::Missing] {
            let facts = WaylandCapabilityFacts {
                gpu_import,
                ..ALL_AVAILABLE
            };
            assert!(facts.blockers().is_empty(), "{gpu_import:?}");
        }
        let no_route = WaylandCapabilityFacts {
            capture_route: CapabilityFact::Unknown,
            ..ALL_AVAILABLE
        };
        assert_eq!(
            no_route.blockers(),
            vec![WaylandBlocker::CaptureRoute(CapabilityFact::Unknown)]
        );
    }

    #[test]
    fn session_proven_blockers_name_the_qualification_command() {
        for blocker in [
            WaylandBlocker::CaptureRoute(CapabilityFact::Unknown),
            WaylandBlocker::InputInjection(CapabilityFact::Unknown),
        ] {
            assert!(
                blocker
                    .explanation()
                    .contains("arcen-pier wayland qualify --write"),
                "{blocker:?}"
            );
        }
    }

    #[test]
    fn capability_facts_use_stable_lowercase_tokens() {
        for (fact, token) in [
            (CapabilityFact::Available, "\"available\""),
            (CapabilityFact::Missing, "\"missing\""),
            (CapabilityFact::Unknown, "\"unknown\""),
        ] {
            assert_eq!(serde_json::to_string(&fact).unwrap(), token);
            assert_eq!(serde_json::from_str::<CapabilityFact>(token).unwrap(), fact);
        }
        assert!(serde_json::from_str::<CapabilityFact>("\"maybe\"").is_err());
    }

    #[test]
    fn unknown_facts_never_count_as_available() {
        let facts = WaylandCapabilityFacts::default();
        assert_eq!(facts.blockers().len(), 6);
        assert!(
            facts
                .blockers()
                .iter()
                .all(|blocker| !blocker.explanation().is_empty())
        );
        assert!(select_desktop_provider(ProviderPreference::Wayland, &host(true, facts)).is_err());
    }
}
