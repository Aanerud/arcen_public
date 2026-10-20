//! Which kind of Wayland desktop a host runs: a compositor on its own logind
//! seat (native) or a seatless headless one (`wayland.session_mode`).
//!
//! Stock Mutter runs libinput only in native mode, so only a native desktop
//! gets a real pen: Arcen's virtual tablet on the desktop's seat reaches
//! applications through `zwp_tablet_v2` and Xwayland. Native mode needs a
//! logind session on a seat that holds at least one KMS device with a
//! connector (Mutter refuses with "No GPUs with outputs found"). A data
//! centre GPU or vGPU has none, so the seat gets the in-tree `vkms` card as
//! its display head and the NVIDIA card node for rendering. A card node
//! belongs to one seat and the kernel offers one `vkms` device, so a host
//! runs at most one native desktop; the others are headless.
//!
//! This module is the pure part: the mode, what the host must have, what
//! qualification proved, and the decision for one desktop with the reason
//! when it falls back or is refused. The host gathers the facts and acts
//! (`hosts/linux/src/input/seat_pen.rs`).

use serde::{Deserialize, Serialize};

use crate::polkit_remote::{
    Admission, DesktopSeat, PolkitState, PolkitUnprotected, refusal_before_launch,
};

/// `wayland.session_mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionMode {
    /// A native desktop when the host's qualification proved one and its
    /// seat can be built now; otherwise headless, with the reason logged.
    #[default]
    Auto,
    /// Always native; a desktop that cannot have its seat is refused.
    NativeSeat,
    /// Always headless (seatless), as before native desktops existed.
    Headless,
}

impl SessionMode {
    /// Stable token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::NativeSeat => "native_seat",
            Self::Headless => "headless",
        }
    }
}

/// Why a desktop is not native.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeSeatUnavailable {
    /// `wayland.session_mode = headless`.
    Configured,
    /// The qualification record has no native-seat proof (an older record,
    /// or qualification did not run it).
    NotQualified,
    /// Qualification ran it and it failed.
    QualificationFailed,
    /// No NVIDIA DRM card node (`nvidia_drm modeset=1` is required).
    NoGpuCard,
    /// The GPU has connectors: it may drive a console on `seat0`, which a
    /// native desktop would take away.
    GpuHasConnectors,
    /// The GPU's card node is open (an Xorg server or a display manager on
    /// `seat0`).
    GpuInUse,
    /// The `vkms` kernel module is not installed.
    VkmsMissing,
    /// `vkms` is already loaded: another native desktop, or someone else's.
    VkmsInUse,
    /// A session (a display manager's greeter) was on the seat before the
    /// desktop's own.
    GreeterOnSeat,
    /// Building the seat failed (udev, logind or the module).
    SeatSetupFailed,
    /// polkit older than 0.106 has no JavaScript rules, so a remote user on
    /// a seat cannot be kept from the grants polkit gives local users
    /// ([`PolkitUnprotected::NoRules`]).
    PolkitNoRules,
    /// Arcen's polkit rule ([`crate::polkit_remote::RULE_PATH`]) is not
    /// installed ([`PolkitUnprotected::RuleMissing`]).
    PolkitRuleMissing,
    /// A process in the desktop's session was still authorized for an
    /// action polkit grants only local, active users
    /// ([`PolkitUnprotected::Granted`]).
    PolkitRuleIneffective,
    /// polkit's tools did not answer, so the desktop's session could not be
    /// proven remote to polkit ([`PolkitUnprotected::Unverified`]).
    PolkitUnverified,
}

impl NativeSeatUnavailable {
    /// Stable token, for logs, records and the installer.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Configured => "configured_headless",
            Self::NotQualified => "not_qualified",
            Self::QualificationFailed => "qualification_failed",
            Self::NoGpuCard => "no_gpu_card",
            Self::GpuHasConnectors => "gpu_has_connectors",
            Self::GpuInUse => "gpu_in_use",
            Self::VkmsMissing => "vkms_missing",
            Self::VkmsInUse => "vkms_in_use",
            Self::GreeterOnSeat => "greeter_on_seat",
            Self::SeatSetupFailed => "seat_setup_failed",
            Self::PolkitNoRules => PolkitUnprotected::NoRules.token(),
            Self::PolkitRuleMissing => PolkitUnprotected::RuleMissing.token(),
            Self::PolkitRuleIneffective => PolkitUnprotected::Granted.token(),
            Self::PolkitUnverified => PolkitUnprotected::Unverified.token(),
        }
    }

    /// One line for an operator.
    #[must_use]
    pub const fn explanation(self) -> &'static str {
        match self {
            Self::Configured => "wayland.session_mode is headless",
            Self::NotQualified => {
                "the host qualification record has no native-seat proof; run `arcen-pier wayland qualify --write`"
            }
            Self::QualificationFailed => {
                "qualification could not prove a native desktop with a pen on this host"
            }
            Self::NoGpuCard => "no NVIDIA DRM card node (nvidia_drm modeset=1 is required)",
            Self::GpuHasConnectors => {
                "the GPU has display connectors and may drive the console; native desktops are for connector-less GPUs"
            }
            Self::GpuInUse => "the GPU's card node is open (an Xorg server or a display manager)",
            Self::VkmsMissing => "the vkms kernel module is not installed",
            Self::VkmsInUse => "vkms is already loaded (one native desktop per host)",
            Self::GreeterOnSeat => "a display manager put a login screen on the desktop's seat",
            Self::SeatSetupFailed => "the desktop's seat could not be built",
            Self::PolkitNoRules => PolkitUnprotected::NoRules.explanation(),
            Self::PolkitRuleMissing => PolkitUnprotected::RuleMissing.explanation(),
            Self::PolkitRuleIneffective => PolkitUnprotected::Granted.explanation(),
            Self::PolkitUnverified => PolkitUnprotected::Unverified.explanation(),
        }
    }
}

impl From<PolkitUnprotected> for NativeSeatUnavailable {
    fn from(reason: PolkitUnprotected) -> Self {
        match reason {
            PolkitUnprotected::NoRules => Self::PolkitNoRules,
            PolkitUnprotected::RuleMissing => Self::PolkitRuleMissing,
            PolkitUnprotected::Granted => Self::PolkitRuleIneffective,
            PolkitUnprotected::Unverified => Self::PolkitUnverified,
        }
    }
}

/// Why polkit's state alone keeps a native desktop from being built: its
/// logind session sits on an Arcen seat ([`DesktopSeat::ArcenSeat`]), so it
/// needs Arcen's rule (polkit >= 0.106) before launch and a `pkcheck` proof
/// after ([`crate::polkit_remote::admission`]). Tools that do not answer
/// keep it headless too: the proof after launch could not pass.
#[must_use]
pub fn polkit_blocker(state: PolkitState) -> Option<NativeSeatUnavailable> {
    match state {
        PolkitState::Unknown => Some(NativeSeatUnavailable::PolkitUnverified),
        state => refusal_before_launch(state, DesktopSeat::ArcenSeat).map(Into::into),
    }
}

/// The `vkms` kernel module on this host.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum VkmsState {
    /// Not installed.
    #[default]
    Missing,
    /// Installed, not loaded.
    Available,
    /// Loaded (by another native desktop, or by someone else).
    Loaded,
}

/// What the host has for a native desktop, read live (sysfs, modules,
/// `/proc`); nothing here is proven by a session.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NativeSeatHostFacts {
    /// The NVIDIA card's udev `ID_PATH` (`pci-…`), when there is one.
    pub gpu_id_path: Option<String>,
    /// Connectors the NVIDIA card exposes.
    pub gpu_connectors: u32,
    /// Whether any process holds the card node open.
    pub gpu_open: bool,
    /// The `vkms` module.
    pub vkms: VkmsState,
    /// polkit and Arcen's rule ([`crate::polkit_remote`]).
    pub polkit: PolkitState,
    /// Whether a display manager (`display-manager.service`, GDM) runs: it
    /// greets every new seat, so a native seat is built in an order that
    /// makes its greeter fail, and a display manager restart while a native
    /// desktop runs would take that desktop's seat.
    pub display_manager_active: bool,
}

impl NativeSeatHostFacts {
    /// Why this host cannot build a native seat now, if it cannot.
    #[must_use]
    pub fn blocker(&self) -> Option<NativeSeatUnavailable> {
        if self.gpu_id_path.is_none() {
            Some(NativeSeatUnavailable::NoGpuCard)
        } else if self.gpu_connectors > 0 {
            Some(NativeSeatUnavailable::GpuHasConnectors)
        } else if self.vkms == VkmsState::Missing {
            Some(NativeSeatUnavailable::VkmsMissing)
        } else if self.vkms == VkmsState::Loaded {
            Some(NativeSeatUnavailable::VkmsInUse)
        } else if self.gpu_open {
            Some(NativeSeatUnavailable::GpuInUse)
        } else {
            polkit_blocker(self.polkit)
        }
    }
}

/// What a qualification desktop on its own seat showed a Wayland client of
/// its compositor (`zwp_tablet_manager_v2`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TabletObservation {
    /// Every `zwp_tablet_v2.name` announced.
    pub tablets: Vec<String>,
    /// Tool types announced after the pen came into proximity
    /// (`zwp_tablet_tool_v2.type`, 0x140 = pen).
    pub tool_types: Vec<u32>,
    /// Capabilities of those tools (`zwp_tablet_tool_v2.capability`).
    pub tool_capabilities: Vec<u32>,
}

/// `zwp_tablet_tool_v2.type` pen.
pub const TABLET_TOOL_TYPE_PEN: u32 = 0x140;
/// `zwp_tablet_tool_v2.capability` tilt.
pub const TABLET_TOOL_CAPABILITY_TILT: u32 = 1;
/// `zwp_tablet_tool_v2.capability` pressure.
pub const TABLET_TOOL_CAPABILITY_PRESSURE: u32 = 2;

/// The native-seat section of the qualification record. Its three evidence
/// bits and the display-manager fact are separate record fields on purpose.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct NativeSeatProof {
    /// The compositor showed the desktop's tablet, by its own name, to a
    /// client.
    pub tablet: bool,
    /// The pen tool appeared with pressure.
    pub pressure: bool,
    /// The pen tool appeared with tilt.
    pub tilt: bool,
    /// The shared polkit admission admitted the desktop's session on its
    /// Arcen seat: Arcen's rule was installed and `pkcheck` found a process
    /// in the session refused the actions polkit grants local, active users
    /// (or the host has no polkit). Absent in records written before the
    /// check existed, which therefore prove nothing.
    #[serde(default)]
    pub polkit_effective: bool,
    /// The card the seat rendered on (`pci-…`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu_id_path: Option<String>,
    /// Whether a display manager ran while it was proven.
    pub display_manager_active: bool,
    /// Why it is not proven, when it is not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unavailable: Option<NativeSeatUnavailable>,
}

impl NativeSeatProof {
    /// The proof from what the client saw of the desktop's tablet
    /// `pen_name`, and the shared polkit admission of the desktop's session
    /// on its Arcen seat ([`crate::polkit_remote::admission`] with
    /// [`DesktopSeat::ArcenSeat`]): only [`Admission::Admit`] is effective.
    #[must_use]
    pub fn from_observation(
        observation: &TabletObservation,
        pen_name: &str,
        polkit: Admission,
        gpu_id_path: Option<String>,
        display_manager_active: bool,
    ) -> Self {
        let tablet = observation.tablets.iter().any(|name| name == pen_name);
        let pen = observation.tool_types.contains(&TABLET_TOOL_TYPE_PEN);
        let has = |capability| pen && observation.tool_capabilities.contains(&capability);
        let pressure = has(TABLET_TOOL_CAPABILITY_PRESSURE);
        let tilt = has(TABLET_TOOL_CAPABILITY_TILT);
        let polkit_effective = polkit == Admission::Admit;
        let unavailable =
            if let Admission::Refuse(reason) | Admission::AdmitUnprotected(reason) = polkit {
                Some(reason.into())
            } else if !polkit_effective {
                Some(NativeSeatUnavailable::PolkitUnverified)
            } else if !(tablet && pressure && tilt) {
                Some(NativeSeatUnavailable::QualificationFailed)
            } else {
                None
            };
        Self {
            tablet,
            pressure,
            tilt,
            polkit_effective,
            gpu_id_path,
            display_manager_active,
            unavailable,
        }
    }

    /// A proof that was not run, with the reason.
    #[must_use]
    pub fn unavailable(reason: NativeSeatUnavailable, display_manager_active: bool) -> Self {
        Self {
            tablet: false,
            pressure: false,
            tilt: false,
            polkit_effective: false,
            gpu_id_path: None,
            display_manager_active,
            unavailable: Some(reason),
        }
    }

    /// A native desktop with a pressure- and tilt-capable pen was proven.
    #[must_use]
    pub const fn proven(&self) -> bool {
        self.tablet
            && self.pressure
            && self.tilt
            && self.polkit_effective
            && self.unavailable.is_none()
    }
}

/// How one new Wayland desktop is created.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesktopSeatPlan {
    /// Try a native seat; when it cannot be built, `required` refuses the
    /// desktop, otherwise it falls back to headless with the reason.
    Native { required: bool },
    /// Headless; `fallback` says why when the mode asked for native.
    Headless {
        fallback: Option<NativeSeatUnavailable>,
    },
    /// The mode requires native and the host cannot have it.
    Refuse(NativeSeatUnavailable),
}

/// The plan for a new desktop from the mode and the qualification record's
/// proof (`None`: the record has no native-seat section).
#[must_use]
pub fn plan_desktop_seat(mode: SessionMode, proof: Option<&NativeSeatProof>) -> DesktopSeatPlan {
    let unproven = || match proof {
        None => NativeSeatUnavailable::NotQualified,
        Some(proof) => proof
            .unavailable
            .unwrap_or(NativeSeatUnavailable::QualificationFailed),
    };
    let proven = proof.is_some_and(NativeSeatProof::proven);
    match (mode, proven) {
        (SessionMode::Headless, _) => DesktopSeatPlan::Headless { fallback: None },
        (SessionMode::Auto, true) => DesktopSeatPlan::Native { required: false },
        (SessionMode::Auto, false) => DesktopSeatPlan::Headless {
            fallback: Some(unproven()),
        },
        (SessionMode::NativeSeat, true) => DesktopSeatPlan::Native { required: true },
        (SessionMode::NativeSeat, false) => DesktopSeatPlan::Refuse(unproven()),
    }
}

/// What happens when building the seat of a `Native` plan failed with
/// `reason`: a required one refuses the desktop, otherwise it is headless.
#[must_use]
pub const fn after_seat_failure(required: bool, reason: NativeSeatUnavailable) -> DesktopSeatPlan {
    if required {
        DesktopSeatPlan::Refuse(reason)
    } else {
        DesktopSeatPlan::Headless {
            fallback: Some(reason),
        }
    }
}

/// The number of connectors in a DRM card's sysfs directory listing:
/// entries named `cardN-<connector>` for that card.
#[must_use]
pub fn count_connectors<'a>(card: &str, entries: impl IntoIterator<Item = &'a str>) -> u32 {
    let prefix = format!("{card}-");
    let count = entries
        .into_iter()
        .filter(|entry| entry.starts_with(&prefix))
        .count();
    u32::try_from(count).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proof(tablet: bool, pressure: bool, tilt: bool) -> NativeSeatProof {
        NativeSeatProof::from_observation(
            &TabletObservation {
                tablets: if tablet {
                    vec!["Arcen Seat Pen 0123456789abcdef".into()]
                } else {
                    vec!["Some Other Tablet".into()]
                },
                tool_types: vec![TABLET_TOOL_TYPE_PEN],
                tool_capabilities: [
                    pressure.then_some(TABLET_TOOL_CAPABILITY_PRESSURE),
                    tilt.then_some(TABLET_TOOL_CAPABILITY_TILT),
                ]
                .into_iter()
                .flatten()
                .collect(),
            },
            "Arcen Seat Pen 0123456789abcdef",
            Admission::Admit,
            Some("pci-0000:00:10.0".into()),
            true,
        )
    }

    #[test]
    fn only_the_desktops_own_pen_with_pressure_and_tilt_proves_a_native_seat() {
        assert!(proof(true, true, true).proven());
        assert!(!proof(false, true, true).proven(), "another tablet");
        assert!(!proof(true, false, true).proven(), "no pressure");
        assert!(!proof(true, true, false).proven(), "no tilt");
        let eraser_only = NativeSeatProof::from_observation(
            &TabletObservation {
                tablets: vec!["Arcen Seat Pen 0123456789abcdef".into()],
                tool_types: vec![0x141],
                tool_capabilities: vec![1, 2],
            },
            "Arcen Seat Pen 0123456789abcdef",
            Admission::Admit,
            None,
            false,
        );
        assert!(!eraser_only.proven());
        let with_polkit = |polkit| {
            NativeSeatProof::from_observation(
                &TabletObservation {
                    tablets: vec!["Arcen Seat Pen 0123456789abcdef".into()],
                    tool_types: vec![TABLET_TOOL_TYPE_PEN],
                    tool_capabilities: vec![1, 2],
                },
                "Arcen Seat Pen 0123456789abcdef",
                polkit,
                None,
                false,
            )
        };
        for (polkit, reason) in [
            (
                Admission::Refuse(PolkitUnprotected::Granted),
                NativeSeatUnavailable::PolkitRuleIneffective,
            ),
            (
                Admission::Refuse(PolkitUnprotected::Unverified),
                NativeSeatUnavailable::PolkitUnverified,
            ),
            (
                Admission::Refuse(PolkitUnprotected::NoRules),
                NativeSeatUnavailable::PolkitNoRules,
            ),
            (
                Admission::Refuse(PolkitUnprotected::RuleMissing),
                NativeSeatUnavailable::PolkitRuleMissing,
            ),
            (
                Admission::AdmitUnprotected(PolkitUnprotected::NoRules),
                NativeSeatUnavailable::PolkitNoRules,
            ),
            (
                Admission::AdmitUnverified,
                NativeSeatUnavailable::PolkitUnverified,
            ),
        ] {
            let leaky = with_polkit(polkit);
            assert!(
                !leaky.proven(),
                "a pen without the polkit proof is no native seat ({polkit:?})"
            );
            assert!(!leaky.polkit_effective);
            assert_eq!(leaky.unavailable, Some(reason));
        }
        assert!(with_polkit(Admission::Admit).proven());
        let old_record: NativeSeatProof = serde_json::from_str(
            r#"{"tablet":true,"pressure":true,"tilt":true,"display_manager_active":true}"#,
        )
        .unwrap();
        assert!(
            !old_record.proven(),
            "a record without the polkit proof proves nothing"
        );
        assert_eq!(
            proof(true, false, true).unavailable,
            Some(NativeSeatUnavailable::QualificationFailed)
        );
    }

    #[test]
    fn auto_is_native_only_when_proven_and_falls_back_visibly() {
        let proven = proof(true, true, true);
        let failed = proof(true, false, true);
        assert_eq!(
            plan_desktop_seat(SessionMode::Auto, Some(&proven)),
            DesktopSeatPlan::Native { required: false }
        );
        assert_eq!(
            plan_desktop_seat(SessionMode::Auto, None),
            DesktopSeatPlan::Headless {
                fallback: Some(NativeSeatUnavailable::NotQualified)
            }
        );
        assert_eq!(
            plan_desktop_seat(SessionMode::Auto, Some(&failed)),
            DesktopSeatPlan::Headless {
                fallback: Some(NativeSeatUnavailable::QualificationFailed)
            }
        );
        let host = NativeSeatProof::unavailable(NativeSeatUnavailable::GpuHasConnectors, false);
        assert_eq!(
            plan_desktop_seat(SessionMode::Auto, Some(&host)),
            DesktopSeatPlan::Headless {
                fallback: Some(NativeSeatUnavailable::GpuHasConnectors)
            }
        );
    }

    #[test]
    fn explicit_modes_are_obeyed() {
        let proven = proof(true, true, true);
        assert_eq!(
            plan_desktop_seat(SessionMode::Headless, Some(&proven)),
            DesktopSeatPlan::Headless { fallback: None }
        );
        assert_eq!(
            plan_desktop_seat(SessionMode::NativeSeat, Some(&proven)),
            DesktopSeatPlan::Native { required: true }
        );
        assert_eq!(
            plan_desktop_seat(SessionMode::NativeSeat, None),
            DesktopSeatPlan::Refuse(NativeSeatUnavailable::NotQualified)
        );
        assert_eq!(
            after_seat_failure(true, NativeSeatUnavailable::VkmsInUse),
            DesktopSeatPlan::Refuse(NativeSeatUnavailable::VkmsInUse)
        );
        assert_eq!(
            after_seat_failure(false, NativeSeatUnavailable::GreeterOnSeat),
            DesktopSeatPlan::Headless {
                fallback: Some(NativeSeatUnavailable::GreeterOnSeat)
            }
        );
    }

    #[test]
    fn host_blockers_come_in_a_fixed_order() {
        let ready = NativeSeatHostFacts {
            gpu_id_path: Some("pci-0000:00:10.0".into()),
            gpu_connectors: 0,
            gpu_open: false,
            vkms: VkmsState::Available,
            polkit: PolkitState::RuleInstalled,
            display_manager_active: true,
        };
        assert_eq!(
            ready.blocker(),
            None,
            "a display manager alone is no blocker"
        );
        assert_eq!(
            NativeSeatHostFacts::default().blocker(),
            Some(NativeSeatUnavailable::NoGpuCard)
        );
        for (facts, reason) in [
            (
                NativeSeatHostFacts {
                    gpu_connectors: 2,
                    ..ready.clone()
                },
                NativeSeatUnavailable::GpuHasConnectors,
            ),
            (
                NativeSeatHostFacts {
                    vkms: VkmsState::Missing,
                    ..ready.clone()
                },
                NativeSeatUnavailable::VkmsMissing,
            ),
            (
                NativeSeatHostFacts {
                    vkms: VkmsState::Loaded,
                    ..ready.clone()
                },
                NativeSeatUnavailable::VkmsInUse,
            ),
            (
                NativeSeatHostFacts {
                    gpu_open: true,
                    ..ready.clone()
                },
                NativeSeatUnavailable::GpuInUse,
            ),
            (
                NativeSeatHostFacts {
                    polkit: PolkitState::NoRules((0, 105)),
                    ..ready.clone()
                },
                NativeSeatUnavailable::PolkitNoRules,
            ),
            (
                NativeSeatHostFacts {
                    polkit: PolkitState::RuleMissing,
                    ..ready.clone()
                },
                NativeSeatUnavailable::PolkitRuleMissing,
            ),
            (
                NativeSeatHostFacts {
                    polkit: PolkitState::Unknown,
                    ..ready.clone()
                },
                NativeSeatUnavailable::PolkitUnverified,
            ),
        ] {
            assert_eq!(facts.blocker(), Some(reason));
        }
        assert_eq!(
            NativeSeatHostFacts {
                polkit: PolkitState::Absent,
                ..ready.clone()
            }
            .blocker(),
            None,
            "no polkit grants nothing"
        );
    }

    #[test]
    fn polkit_reasons_are_the_shared_mechanism_s() {
        for reason in [
            PolkitUnprotected::NoRules,
            PolkitUnprotected::RuleMissing,
            PolkitUnprotected::Granted,
            PolkitUnprotected::Unverified,
        ] {
            let native = NativeSeatUnavailable::from(reason);
            assert_eq!(native.token(), reason.token());
            assert_eq!(native.explanation(), reason.explanation());
        }
        assert!(
            NativeSeatUnavailable::PolkitRuleMissing
                .explanation()
                .contains(crate::polkit_remote::RULE_PATH)
        );
        assert_eq!(polkit_blocker(PolkitState::RuleInstalled), None);
        assert_eq!(
            polkit_blocker(PolkitState::NoRules((0, 105))),
            Some(NativeSeatUnavailable::PolkitNoRules),
            "polkit < 0.106: no native seat, the desktop is headless"
        );
    }

    #[test]
    fn modes_tokens_and_the_record_section_round_trip() {
        for (mode, token) in [
            (SessionMode::Auto, "auto"),
            (SessionMode::NativeSeat, "native_seat"),
            (SessionMode::Headless, "headless"),
        ] {
            assert_eq!(mode.token(), token);
            let parsed: SessionMode = serde_json::from_str(&format!("\"{token}\"")).unwrap();
            assert_eq!(parsed, mode);
        }
        assert!(serde_json::from_str::<SessionMode>("\"native\"").is_err());
        assert_eq!(SessionMode::default(), SessionMode::Auto);
        let proof = proof(true, true, true);
        let json = serde_json::to_string(&proof).unwrap();
        assert_eq!(
            serde_json::from_str::<NativeSeatProof>(&json).unwrap(),
            proof
        );
        assert!(!json.contains("unavailable"));
        let refused = NativeSeatProof::unavailable(NativeSeatUnavailable::VkmsMissing, false);
        assert!(
            serde_json::to_string(&refused)
                .unwrap()
                .contains("\"unavailable\":\"vkms_missing\"")
        );
    }

    #[test]
    fn connectors_are_counted_per_card() {
        let entries = [
            "card0",
            "card1",
            "card1-Virtual-1",
            "card1-Writeback-1",
            "renderD128",
            "card10-HDMI-A-1",
        ];
        assert_eq!(count_connectors("card0", entries), 0);
        assert_eq!(count_connectors("card1", entries), 2);
        assert_eq!(count_connectors("card10", entries), 1);
    }
}
