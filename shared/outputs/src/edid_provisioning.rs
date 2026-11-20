// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Whether a Pier may write a display's EDID to recreate the Deck's exact
//! display, and what a session gets when it may not.
//!
//! An NVIDIA host makes a display of exactly the Deck's size by writing that
//! display's EDID (`NvAPI_GPU_SetEDID`). NVIDIA permits the write only on
//! Quadro, RTX Pro and GRID GPUs; a `GeForce` answers `NVAPI_NOT_SUPPORTED`.
//! So the GPU's class is read first, while the host is only being
//! inspected, and a GPU that is not proved professional is never written to.
//! Such a host still serves the display it has, at the modes that display
//! offers, and says so. Only a configuration that requires the exact display,
//! or a GPU with no display at all, refuses, and then before anything on the
//! host changes. The Windows Pier reads the class
//! (`NvAPI_GPU_GetQuadroStatus`) and applies modes; the decisions are here.

/// Whether this host's GPU lets software write a display's EDID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EdidProvisioning {
    /// A Quadro, RTX Pro or GRID GPU.
    Permitted,
    /// Any other GPU (a `GeForce`).
    NotPermitted {
        /// The GPU's name, as the host reports it.
        gpu: String,
    },
    /// The class could not be read: treated as not permitted.
    ClassUnknown {
        /// The GPU's name, or what is known of it.
        gpu: String,
    },
}

impl EdidProvisioning {
    /// From the professional-class flag the driver reports (`None`: not
    /// read).
    #[must_use]
    pub fn from_professional_class(gpu: &str, professional: Option<bool>) -> Self {
        match professional {
            Some(true) => Self::Permitted,
            Some(false) => Self::NotPermitted {
                gpu: gpu.to_string(),
            },
            None => Self::ClassUnknown {
                gpu: gpu.to_string(),
            },
        }
    }

    /// Whether the host may write a display's EDID.
    #[must_use]
    pub const fn permits_writes(&self) -> bool {
        matches!(self, Self::Permitted)
    }

    /// Why the exact display is out of reach, or `None` when it is not.
    #[must_use]
    pub fn limitation(&self) -> Option<String> {
        match self {
            Self::Permitted => None,
            Self::NotPermitted { gpu } => Some(format!(
                "exact display needs a Quadro, RTX Pro or GRID GPU; this host has a {}",
                without_vendor(gpu)
            )),
            Self::ClassUnknown { gpu } => Some(format!(
                "exact display needs a Quadro, RTX Pro or GRID GPU, and the class of this \
                 host's {} could not be read",
                without_vendor(gpu)
            )),
        }
    }
}

fn without_vendor(gpu: &str) -> &str {
    gpu.get(..7)
        .filter(|prefix| prefix.eq_ignore_ascii_case("NVIDIA "))
        .map_or(gpu, |_| &gpu[7..])
}

/// Whether the operator's configuration requires the Deck's exact display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExactDisplayRequirement {
    /// Exact when the host can, otherwise the nearest mode, reported (the
    /// default).
    Preferred,
    /// The exact display or no session.
    Required {
        /// The setting that requires it, as the operator knows it.
        by: &'static str,
    },
}

/// Stable reason a session is served without the exact display.
pub const EXACT_DISPLAY_NEEDS_PROFESSIONAL_GPU: &str = "exact_display_needs_professional_gpu";

/// Why a session is served without the exact display, for the Deck.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExactDisplayLimit {
    /// [`EXACT_DISPLAY_NEEDS_PROFESSIONAL_GPU`].
    pub reason: &'static str,
    /// One sentence the Deck can show.
    pub explanation: String,
}

/// What a session does with a GPU, a requirement and the displays attached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProvisioningDecision {
    /// Write the EDID: the exact display.
    Provision,
    /// Write nothing; serve the attached display at the modes it offers
    /// (exact when it offers the Deck's size) and report the limit.
    ServeAttachedModes(ExactDisplayLimit),
    /// Write nothing and refuse, with this message, before any change.
    Refuse(String),
}

/// Decides what a session does when the host may not write an EDID.
///
/// `display_attached`: the GPU drives a connected display (a monitor or a
/// dummy plug) whose own modes can be set.
#[must_use]
pub fn decide(
    provisioning: &EdidProvisioning,
    requirement: ExactDisplayRequirement,
    display_attached: bool,
) -> ProvisioningDecision {
    let Some(limitation) = provisioning.limitation() else {
        return ProvisioningDecision::Provision;
    };
    if !display_attached {
        return ProvisioningDecision::Refuse(format!(
            "{limitation}, and no display is attached to it: attach a monitor or an HDMI or \
             DisplayPort dummy plug, or install a virtual display driver, then connect again; \
             nothing on the host was changed"
        ));
    }
    match requirement {
        ExactDisplayRequirement::Required { by } => ProvisioningDecision::Refuse(format!(
            "{limitation}, and {by} requires the exact display; nothing on the host was changed"
        )),
        ExactDisplayRequirement::Preferred => {
            ProvisioningDecision::ServeAttachedModes(ExactDisplayLimit {
                reason: EXACT_DISPLAY_NEEDS_PROFESSIONAL_GPU,
                explanation: format!(
                    "{limitation}, so it serves the modes its attached display offers"
                ),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geforce() -> EdidProvisioning {
        EdidProvisioning::from_professional_class("NVIDIA GeForce RTX 5090", Some(false))
    }

    #[test]
    fn only_a_proved_professional_gpu_is_written_to() {
        assert!(
            EdidProvisioning::from_professional_class("NVIDIA RTX A6000", Some(true))
                .permits_writes()
        );
        assert!(!geforce().permits_writes());
        assert!(
            !EdidProvisioning::from_professional_class("NVIDIA GeForce RTX 5090", None)
                .permits_writes()
        );
        assert_eq!(
            decide(
                &EdidProvisioning::Permitted,
                ExactDisplayRequirement::Required { by: "x" },
                false
            ),
            ProvisioningDecision::Provision
        );
    }

    #[test]
    fn a_geforce_with_a_display_is_served_at_that_displays_modes() {
        let ProvisioningDecision::ServeAttachedModes(limit) =
            decide(&geforce(), ExactDisplayRequirement::Preferred, true)
        else {
            panic!("the default serves the attached display");
        };
        assert_eq!(limit.reason, EXACT_DISPLAY_NEEDS_PROFESSIONAL_GPU);
        assert_eq!(
            limit.explanation,
            "exact display needs a Quadro, RTX Pro or GRID GPU; this host has a GeForce RTX \
             5090, so it serves the modes its attached display offers"
        );
    }

    #[test]
    fn a_strict_configuration_or_no_display_refuses_before_any_change() {
        let ProvisioningDecision::Refuse(strict) = decide(
            &geforce(),
            ExactDisplayRequirement::Required {
                by: "pinned deskside protection",
            },
            true,
        ) else {
            panic!("a required exact display is refused");
        };
        assert!(strict.starts_with(
            "exact display needs a Quadro, RTX Pro or GRID GPU; this host has a GeForce RTX 5090"
        ));
        assert!(strict.contains("pinned deskside protection requires the exact display"));
        assert!(strict.ends_with("nothing on the host was changed"));

        let ProvisioningDecision::Refuse(headless) =
            decide(&geforce(), ExactDisplayRequirement::Preferred, false)
        else {
            panic!("a GPU with no display cannot be served");
        };
        assert!(headless.contains("dummy plug"), "{headless}");

        let ProvisioningDecision::ServeAttachedModes(unknown) = decide(
            &EdidProvisioning::from_professional_class("NVIDIA GeForce RTX 5090", None),
            ExactDisplayRequirement::Preferred,
            true,
        ) else {
            panic!("an unknown class is served like a GeForce");
        };
        assert!(unknown.explanation.contains("could not be read"));
    }
}
