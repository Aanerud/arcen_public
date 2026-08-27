//! Unified output capability semantics and the shared admission gate.
//!
//! Section 5 of ADR 0010: every field here is *semantic*. It states what the
//! provider can promise about the resulting desktop, never how it achieves
//! it. Backend names, device names, adapter LUIDs, target ids, journal
//! paths, compositor detection facts, and generation numbers are host facts;
//! they stay in the host and in [`OutputProvider::Evidence`].
//!
//! [`OutputProvider::Evidence`]: super::OutputProvider::Evidence

use core::fmt;

use arcen_media::MAX_MULTI_MONITOR_COUNT;

/// What the provider's output surface is, relative to the console desktop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum OutputSurface {
    /// Mutates outputs the console session also uses.
    SharedPhysical,
    /// Owns a head or display server dedicated to the remote session.
    DedicatedPhysical,
    /// Creates monitors that did not previously exist.
    Virtual,
}

impl fmt::Display for OutputSurface {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::SharedPhysical => "shared-physical",
            Self::DedicatedPhysical => "dedicated-physical",
            Self::Virtual => "virtual",
        })
    }
}

/// The strongest teardown guarantee a provider can prove.
///
/// Ordered weakest to strongest, so admission can compare a provided
/// guarantee against a required one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RollbackGuarantee {
    /// No teardown obligation is honoured. Only ever valid for a provider
    /// that mutates nothing.
    None,
    /// Teardown is attempted and may leave a topology the operator did not
    /// choose.
    BestEffort,
    /// Teardown leaves at least one active, usable output, either the exact
    /// pre-bind topology or a verified safe-primary topology.
    SafePrimary,
    /// Teardown restores the exact pre-bind topology, or releases every
    /// resource the provider created without disturbing the console
    /// topology.
    ExactRestore,
}

impl fmt::Display for RollbackGuarantee {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::None => "none",
            Self::BestEffort => "best-effort",
            Self::SafePrimary => "safe-primary",
            Self::ExactRestore => "exact-restore",
        })
    }
}

/// Rejection building an [`OutputCapabilities`] region range.
///
/// The checked constructor is what makes an out-of-range or inverted range
/// unrepresentable, so admission never has to defend against one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CapabilityRangeError {
    /// A provider that can serve zero regions cannot serve a session.
    ZeroMinimum,
    /// The minimum exceeds the maximum.
    Inverted { min: usize, max: usize },
    /// The maximum exceeds [`MAX_MULTI_MONITOR_COUNT`].
    ExceedsSupportedRegions { max: usize, limit: usize },
}

impl fmt::Display for CapabilityRangeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroMinimum => {
                formatter.write_str("an output provider must serve at least one region")
            }
            Self::Inverted { min, max } => write!(
                formatter,
                "output provider region range is inverted: {min}..={max}"
            ),
            Self::ExceedsSupportedRegions { max, limit } => write!(
                formatter,
                "output provider region maximum {max} exceeds the supported maximum of {limit}"
            ),
        }
    }
}

impl std::error::Error for CapabilityRangeError {}

/// What a provider promises about the desktop it produces.
///
/// The region range is private and built through [`OutputCapabilities::new`];
/// every semantic promise is a public field a provider sets directly.
// ADR 0010 freezes this field set. Each flag is an independent promise about
// the resulting desktop, and admission reads them independently, so collapsing
// them into two-variant enums or a state machine would only rename them.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputCapabilities {
    min_regions: usize,
    max_regions: usize,
    /// What the provider's surface is, relative to the console desktop.
    pub surface: OutputSurface,
    /// The applied mode equals the requested mode. No nearest-match
    /// substitution.
    pub exact_modes: bool,
    /// Negative desktop origins are supported.
    pub signed_desktop_coordinates: bool,
    /// The topology survives for the whole session rather than for one call.
    pub persistent_dedicated_desktop: bool,
    /// The provider can serve with no monitor physically attached.
    pub headless_capable: bool,
    /// Per-region rotation is applied and verified, not ignored.
    pub per_region_rotation: bool,
    /// A per-region scale other than a whole multiple of 120 is honoured.
    pub fractional_scale: bool,
    /// The strongest teardown guarantee this provider can prove.
    pub rollback: RollbackGuarantee,
}

impl OutputCapabilities {
    /// Builds capabilities with a checked region range and every semantic
    /// promise cleared.
    ///
    /// Callers set the public promise fields afterwards. Everything defaults
    /// to "not promised", so a provider that forgets a field is refused
    /// admission rather than silently admitted.
    ///
    /// # Errors
    ///
    /// Returns [`CapabilityRangeError`] when the range starts at zero, is
    /// inverted, or exceeds [`MAX_MULTI_MONITOR_COUNT`].
    pub const fn new(
        min_regions: usize,
        max_regions: usize,
        surface: OutputSurface,
        rollback: RollbackGuarantee,
    ) -> Result<Self, CapabilityRangeError> {
        if min_regions == 0 {
            return Err(CapabilityRangeError::ZeroMinimum);
        }
        if min_regions > max_regions {
            return Err(CapabilityRangeError::Inverted {
                min: min_regions,
                max: max_regions,
            });
        }
        if max_regions > MAX_MULTI_MONITOR_COUNT {
            return Err(CapabilityRangeError::ExceedsSupportedRegions {
                max: max_regions,
                limit: MAX_MULTI_MONITOR_COUNT,
            });
        }
        Ok(Self {
            min_regions,
            max_regions,
            surface,
            exact_modes: false,
            signed_desktop_coordinates: false,
            persistent_dedicated_desktop: false,
            headless_capable: false,
            per_region_rotation: false,
            fractional_scale: false,
            rollback,
        })
    }

    /// Smallest region count this provider can serve atomically.
    #[must_use]
    pub const fn min_regions(&self) -> usize {
        self.min_regions
    }

    /// Largest region count this provider can serve atomically.
    #[must_use]
    pub const fn max_regions(&self) -> usize {
        self.max_regions
    }

    /// Whether `regions` falls inside this provider's atomic region range.
    #[must_use]
    pub const fn serves_region_count(&self, regions: usize) -> bool {
        regions >= self.min_regions && regions <= self.max_regions
    }

    /// The teardown guarantee admission requires of this surface.
    ///
    /// A [`OutputSurface::SharedPhysical`] provider mutates the console
    /// desktop, so it must be able to prove at least
    /// [`RollbackGuarantee::SafePrimary`] — the ADR 0009 non-headless
    /// invariant, expressed as an admission rule.
    #[must_use]
    pub const fn required_rollback(surface: OutputSurface) -> RollbackGuarantee {
        match surface {
            OutputSurface::SharedPhysical => RollbackGuarantee::SafePrimary,
            OutputSurface::DedicatedPhysical | OutputSurface::Virtual => {
                RollbackGuarantee::BestEffort
            }
        }
    }
}

/// What one host plan needs from a provider, in the shared semantic
/// vocabulary.
///
/// The provider translates its own host plan into this; the shared crate
/// never reads a plan.
// ADR 0010 freezes this field set; see the note on `OutputCapabilities`.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OutputDemand {
    /// How many regions the plan applies.
    pub regions: usize,
    /// The plan places at least one region at a negative desktop origin.
    pub negative_coordinates: bool,
    /// The plan requires the applied mode to equal the requested mode.
    pub exact_modes: bool,
    /// The plan requires a desktop that survives the whole session.
    pub persistent_desktop: bool,
    /// The plan must be served with no monitor physically attached.
    pub headless: bool,
    /// The plan rotates at least one region.
    pub rotation: bool,
    /// The plan scales at least one region fractionally.
    pub fractional_scale: bool,
}

impl OutputDemand {
    /// A demand for `regions` regions with no additional requirement.
    #[must_use]
    pub const fn new(regions: usize) -> Self {
        Self {
            regions,
            negative_coordinates: false,
            exact_modes: false,
            persistent_desktop: false,
            headless: false,
            rotation: false,
            fractional_scale: false,
        }
    }
}

/// Why a provider cannot serve a demand.
///
/// `#[non_exhaustive]` so new mismatches are additive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CapabilityMismatch {
    /// The demanded region count falls outside the provider's atomic range.
    RegionCount {
        requested: usize,
        min: usize,
        max: usize,
    },
    /// The plan requires exact modes and the provider substitutes.
    ExactModesUnsupported,
    /// The plan places a region at a negative origin and the provider cannot.
    SignedCoordinatesUnsupported,
    /// The plan needs a session-lifetime desktop and the provider cannot
    /// hold one.
    PersistentDesktopUnsupported,
    /// The plan must be served with no attached monitor and the provider
    /// cannot.
    HeadlessUnsupported,
    /// The plan rotates a region and the provider ignores rotation.
    RotationUnsupported,
    /// The plan scales a region fractionally and the provider cannot honour
    /// it.
    FractionalScaleUnsupported,
    /// The provider's teardown guarantee is too weak for its surface.
    RollbackGuaranteeInsufficient {
        required: RollbackGuarantee,
        provided: RollbackGuarantee,
    },
}

impl fmt::Display for CapabilityMismatch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RegionCount {
                requested,
                min,
                max,
            } => write!(
                formatter,
                "output provider serves {min}..={max} regions, but the plan applies {requested}"
            ),
            Self::ExactModesUnsupported => {
                formatter.write_str("output provider cannot guarantee exact region modes")
            }
            Self::SignedCoordinatesUnsupported => {
                formatter.write_str("output provider does not support signed desktop coordinates")
            }
            Self::PersistentDesktopUnsupported => {
                formatter.write_str("output provider cannot hold a persistent dedicated desktop")
            }
            Self::HeadlessUnsupported => {
                formatter.write_str("output provider cannot serve a headless region")
            }
            Self::RotationUnsupported => {
                formatter.write_str("output provider does not apply per-region rotation")
            }
            Self::FractionalScaleUnsupported => {
                formatter.write_str("output provider does not honour fractional region scale")
            }
            Self::RollbackGuaranteeInsufficient { required, provided } => write!(
                formatter,
                "output provider guarantees {provided} rollback, but its surface requires {required}"
            ),
        }
    }
}

impl std::error::Error for CapabilityMismatch {}

/// The one shared admission gate, run by
/// [`OutputTransaction::acquire`](super::OutputTransaction::acquire) before
/// any provider code.
///
/// The provider supplies both sides — only it can read its own host plan —
/// but it does not decide the outcome. The comparison rules, the check
/// order, and the resulting [`CapabilityMismatch`] are the shared crate's, so
/// two hosts cannot drift into refusing the same topology for differently
/// worded reasons.
///
/// The check order is frozen and tested: region count, exact modes, signed
/// coordinates, persistent desktop, headless, rotation, fractional scale,
/// then the surface's required rollback guarantee.
///
/// # Errors
///
/// Returns the first [`CapabilityMismatch`] in that order.
pub fn admits(
    capabilities: &OutputCapabilities,
    demand: &OutputDemand,
) -> Result<(), CapabilityMismatch> {
    if !capabilities.serves_region_count(demand.regions) {
        return Err(CapabilityMismatch::RegionCount {
            requested: demand.regions,
            min: capabilities.min_regions,
            max: capabilities.max_regions,
        });
    }
    if demand.exact_modes && !capabilities.exact_modes {
        return Err(CapabilityMismatch::ExactModesUnsupported);
    }
    if demand.negative_coordinates && !capabilities.signed_desktop_coordinates {
        return Err(CapabilityMismatch::SignedCoordinatesUnsupported);
    }
    if demand.persistent_desktop && !capabilities.persistent_dedicated_desktop {
        return Err(CapabilityMismatch::PersistentDesktopUnsupported);
    }
    if demand.headless && !capabilities.headless_capable {
        return Err(CapabilityMismatch::HeadlessUnsupported);
    }
    if demand.rotation && !capabilities.per_region_rotation {
        return Err(CapabilityMismatch::RotationUnsupported);
    }
    if demand.fractional_scale && !capabilities.fractional_scale {
        return Err(CapabilityMismatch::FractionalScaleUnsupported);
    }
    let required = OutputCapabilities::required_rollback(capabilities.surface);
    if capabilities.rollback < required {
        return Err(CapabilityMismatch::RollbackGuaranteeInsufficient {
            required,
            provided: capabilities.rollback,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        CapabilityMismatch, CapabilityRangeError, MAX_MULTI_MONITOR_COUNT, OutputCapabilities,
        OutputDemand, OutputSurface, RollbackGuarantee, admits,
    };

    fn full_capabilities() -> OutputCapabilities {
        let mut capabilities = OutputCapabilities::new(
            1,
            MAX_MULTI_MONITOR_COUNT,
            OutputSurface::DedicatedPhysical,
            RollbackGuarantee::ExactRestore,
        )
        .expect("valid region range");
        capabilities.exact_modes = true;
        capabilities.signed_desktop_coordinates = true;
        capabilities.persistent_dedicated_desktop = true;
        capabilities.headless_capable = true;
        capabilities.per_region_rotation = true;
        capabilities.fractional_scale = true;
        capabilities
    }

    #[test]
    fn region_range_construction_is_checked() {
        assert_eq!(
            OutputCapabilities::new(
                0,
                2,
                OutputSurface::Virtual,
                RollbackGuarantee::ExactRestore
            ),
            Err(CapabilityRangeError::ZeroMinimum)
        );
        assert_eq!(
            OutputCapabilities::new(
                3,
                2,
                OutputSurface::Virtual,
                RollbackGuarantee::ExactRestore
            ),
            Err(CapabilityRangeError::Inverted { min: 3, max: 2 })
        );
        assert_eq!(
            OutputCapabilities::new(
                1,
                MAX_MULTI_MONITOR_COUNT + 1,
                OutputSurface::Virtual,
                RollbackGuarantee::ExactRestore
            ),
            Err(CapabilityRangeError::ExceedsSupportedRegions {
                max: MAX_MULTI_MONITOR_COUNT + 1,
                limit: MAX_MULTI_MONITOR_COUNT,
            })
        );
    }

    #[test]
    fn new_capabilities_promise_nothing_until_the_provider_sets_them() {
        let capabilities = OutputCapabilities::new(
            1,
            2,
            OutputSurface::Virtual,
            RollbackGuarantee::ExactRestore,
        )
        .expect("valid region range");
        assert!(!capabilities.exact_modes);
        assert!(!capabilities.signed_desktop_coordinates);
        assert!(!capabilities.persistent_dedicated_desktop);
        assert!(!capabilities.headless_capable);
        assert!(!capabilities.per_region_rotation);
        assert!(!capabilities.fractional_scale);
        assert_eq!(
            admits(
                &capabilities,
                &OutputDemand {
                    regions: 1,
                    exact_modes: true,
                    ..OutputDemand::new(1)
                }
            ),
            Err(CapabilityMismatch::ExactModesUnsupported)
        );
    }

    #[test]
    fn admits_every_region_count_inside_the_range_and_rejects_the_boundaries() {
        let capabilities = full_capabilities();
        for regions in 1..=MAX_MULTI_MONITOR_COUNT {
            assert_eq!(admits(&capabilities, &OutputDemand::new(regions)), Ok(()));
        }
        for regions in [0, MAX_MULTI_MONITOR_COUNT + 1] {
            assert_eq!(
                admits(&capabilities, &OutputDemand::new(regions)),
                Err(CapabilityMismatch::RegionCount {
                    requested: regions,
                    min: 1,
                    max: MAX_MULTI_MONITOR_COUNT,
                })
            );
        }
    }

    /// Turns one promise off so the matching demand becomes unmet.
    type Revoke = fn(&mut OutputCapabilities);

    #[test]
    fn every_unmet_promise_has_its_own_typed_mismatch() {
        let cases: [(Revoke, OutputDemand, CapabilityMismatch); 6] = [
            (
                |capabilities| capabilities.exact_modes = false,
                OutputDemand {
                    exact_modes: true,
                    ..OutputDemand::new(2)
                },
                CapabilityMismatch::ExactModesUnsupported,
            ),
            (
                |capabilities| capabilities.signed_desktop_coordinates = false,
                OutputDemand {
                    negative_coordinates: true,
                    ..OutputDemand::new(2)
                },
                CapabilityMismatch::SignedCoordinatesUnsupported,
            ),
            (
                |capabilities| capabilities.persistent_dedicated_desktop = false,
                OutputDemand {
                    persistent_desktop: true,
                    ..OutputDemand::new(2)
                },
                CapabilityMismatch::PersistentDesktopUnsupported,
            ),
            (
                |capabilities| capabilities.headless_capable = false,
                OutputDemand {
                    headless: true,
                    ..OutputDemand::new(2)
                },
                CapabilityMismatch::HeadlessUnsupported,
            ),
            (
                |capabilities| capabilities.per_region_rotation = false,
                OutputDemand {
                    rotation: true,
                    ..OutputDemand::new(2)
                },
                CapabilityMismatch::RotationUnsupported,
            ),
            (
                |capabilities| capabilities.fractional_scale = false,
                OutputDemand {
                    fractional_scale: true,
                    ..OutputDemand::new(2)
                },
                CapabilityMismatch::FractionalScaleUnsupported,
            ),
        ];

        for (break_promise, demand, expected) in cases {
            let mut capabilities = full_capabilities();
            break_promise(&mut capabilities);
            assert_eq!(admits(&capabilities, &demand), Err(expected));
            assert_eq!(admits(&full_capabilities(), &demand), Ok(()));
        }
    }

    #[test]
    fn a_shared_physical_surface_must_prove_at_least_safe_primary_rollback() {
        for guarantee in [RollbackGuarantee::None, RollbackGuarantee::BestEffort] {
            let mut capabilities = full_capabilities();
            capabilities.surface = OutputSurface::SharedPhysical;
            capabilities.rollback = guarantee;
            assert_eq!(
                admits(&capabilities, &OutputDemand::new(2)),
                Err(CapabilityMismatch::RollbackGuaranteeInsufficient {
                    required: RollbackGuarantee::SafePrimary,
                    provided: guarantee,
                })
            );
        }
        for guarantee in [
            RollbackGuarantee::SafePrimary,
            RollbackGuarantee::ExactRestore,
        ] {
            let mut capabilities = full_capabilities();
            capabilities.surface = OutputSurface::SharedPhysical;
            capabilities.rollback = guarantee;
            assert_eq!(admits(&capabilities, &OutputDemand::new(2)), Ok(()));
        }
    }

    #[test]
    fn a_dedicated_or_virtual_surface_still_needs_a_teardown_obligation() {
        for surface in [OutputSurface::DedicatedPhysical, OutputSurface::Virtual] {
            let mut capabilities = full_capabilities();
            capabilities.surface = surface;
            capabilities.rollback = RollbackGuarantee::None;
            assert_eq!(
                admits(&capabilities, &OutputDemand::new(1)),
                Err(CapabilityMismatch::RollbackGuaranteeInsufficient {
                    required: RollbackGuarantee::BestEffort,
                    provided: RollbackGuarantee::None,
                })
            );
            capabilities.rollback = RollbackGuarantee::BestEffort;
            assert_eq!(admits(&capabilities, &OutputDemand::new(1)), Ok(()));
        }
    }

    #[test]
    fn mismatch_order_is_frozen_region_count_first_and_rollback_last() {
        let mut capabilities = full_capabilities();
        capabilities.surface = OutputSurface::SharedPhysical;
        capabilities.rollback = RollbackGuarantee::None;
        capabilities.exact_modes = false;
        capabilities.headless_capable = false;

        let demand = OutputDemand {
            regions: MAX_MULTI_MONITOR_COUNT + 1,
            exact_modes: true,
            headless: true,
            ..OutputDemand::new(MAX_MULTI_MONITOR_COUNT + 1)
        };
        assert!(matches!(
            admits(&capabilities, &demand),
            Err(CapabilityMismatch::RegionCount { .. })
        ));

        let demand = OutputDemand {
            exact_modes: true,
            headless: true,
            ..OutputDemand::new(2)
        };
        assert_eq!(
            admits(&capabilities, &demand),
            Err(CapabilityMismatch::ExactModesUnsupported)
        );

        capabilities.exact_modes = true;
        assert_eq!(
            admits(&capabilities, &demand),
            Err(CapabilityMismatch::HeadlessUnsupported)
        );

        capabilities.headless_capable = true;
        assert!(matches!(
            admits(&capabilities, &demand),
            Err(CapabilityMismatch::RollbackGuaranteeInsufficient { .. })
        ));
    }

    #[test]
    fn rollback_guarantees_are_ordered_weakest_to_strongest() {
        assert!(RollbackGuarantee::None < RollbackGuarantee::BestEffort);
        assert!(RollbackGuarantee::BestEffort < RollbackGuarantee::SafePrimary);
        assert!(RollbackGuarantee::SafePrimary < RollbackGuarantee::ExactRestore);
    }
}
