//! Applied multi-region capability assembly.
//!
//! Once [`crate::admission::admit_regions`] has admitted a request and every
//! region's encoder has actually started, both hosts assembled the same
//! applied capability the same way, twice: walk the planned regions in plan
//! order, join each one to the media the host resolved for it, join it again
//! to the negotiated media plan that named its bitrate budget, translate the
//! applied desktop origin to a non-negative origin, and refuse the whole
//! capability if any single region is missing either half.
//!
//! This module owns that join, its order, and the budget rule. It owns
//! nothing about the wire: the descriptor a host emits, the identifiers it
//! validates, and the error evidence it reports are all host-typed, because
//! the applied capability is a protocol message this crate deliberately
//! cannot name.
//!
//! # The budget rule
//!
//! An applied region's published bitrate is
//! [`arcen_media::RegionMediaPlan::bitrate_budget`], read verbatim. It is
//! never recomputed from geometry at assembly time and never a literal:
//! [`AppliedRegion::bitrate_kbps`] is the only way to obtain it, and it
//! forwards to [`arcen_media::RegionMediaPlan::applied_bitrate_kbps`]. A host
//! that re-derived a nominal budget here could publish a number its own
//! encoder admission never agreed to.

use core::fmt;

use arcen_media::{RegionMediaPlan, RegionMediaRoster, SessionMonitorId};

/// The translation an applied topology publishes to move its desktop origin
/// to a non-negative origin.
///
/// A host whose applied desktop already starts at or after `(0, 0)` — a
/// dedicated server-side topology, for instance — gets
/// [`Self::NONE`] and every coordinate passes through unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OriginTranslation {
    x: i64,
    y: i64,
}

impl OriginTranslation {
    /// The identity translation, for a desktop already at a non-negative
    /// origin.
    pub const NONE: Self = Self { x: 0, y: 0 };

    /// The translation that moves a desktop whose origin is
    /// `(desktop_x, desktop_y)` to a non-negative origin.
    ///
    /// Only a negative axis shifts; a non-negative one is left exactly where
    /// the host placed it, so an already-normalised desktop is never moved.
    #[must_use]
    pub const fn to_origin(desktop_x: i32, desktop_y: i32) -> Self {
        Self {
            x: if desktop_x < 0 {
                -(desktop_x as i64)
            } else {
                0
            },
            y: if desktop_y < 0 {
                -(desktop_y as i64)
            } else {
                0
            },
        }
    }

    /// The horizontal shift, in the units the applied topology publishes.
    #[must_use]
    pub const fn x(self) -> i64 {
        self.x
    }

    /// The vertical shift, in the units the applied topology publishes.
    #[must_use]
    pub const fn y(self) -> i64 {
        self.y
    }

    /// Translates a horizontal coordinate.
    ///
    /// # Errors
    ///
    /// Returns [`OriginTranslationOverflow`] when the translated coordinate
    /// leaves the published coordinate range, so a host fails the capability
    /// closed instead of wrapping a monitor to the far side of the desktop.
    pub const fn apply_x(self, x: i32) -> Result<i32, OriginTranslationOverflow> {
        Self::apply(x, self.x)
    }

    /// Translates a vertical coordinate.
    ///
    /// # Errors
    ///
    /// See [`Self::apply_x`].
    pub const fn apply_y(self, y: i32) -> Result<i32, OriginTranslationOverflow> {
        Self::apply(y, self.y)
    }

    const fn apply(coordinate: i32, shift: i64) -> Result<i32, OriginTranslationOverflow> {
        let translated = coordinate as i64 + shift;
        if translated < i32::MIN as i64 || translated > i32::MAX as i64 {
            return Err(OriginTranslationOverflow);
        }
        #[allow(clippy::cast_possible_truncation)]
        Ok(translated as i32)
    }
}

/// A translated coordinate left the published coordinate range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OriginTranslationOverflow;

impl fmt::Display for OriginTranslationOverflow {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("applied origin translation left the published coordinate range")
    }
}

impl std::error::Error for OriginTranslationOverflow {}

/// One planned region, joined to everything the applied capability publishes
/// about it.
#[derive(Debug, Clone, Copy)]
pub struct AppliedRegion<'a, M, R> {
    /// The host's own planned region, in plan order.
    pub region: &'a M,
    /// What this host's encoder actually resolved for the region. The applied
    /// capability reports this, not what was requested.
    pub resolved: &'a R,
    /// The negotiated media plan for the region. Its budget is the published
    /// bitrate; see [`Self::bitrate_kbps`].
    pub media: &'a RegionMediaPlan,
    /// The translation this applied topology publishes.
    pub translation: OriginTranslation,
}

impl<M, R> AppliedRegion<'_, M, R> {
    /// This region's published bitrate, read verbatim off the negotiated
    /// media plan.
    ///
    /// Never recomputed from geometry: the budget was decided once, when the
    /// region's encoder set was admitted.
    #[must_use]
    pub const fn bitrate_kbps(&self) -> u32 {
        self.media.applied_bitrate_kbps()
    }

    /// This region's published media stream epoch.
    #[must_use]
    pub const fn stream_epoch(&self) -> u64 {
        self.media.stream_epoch.get()
    }
}

/// The host-shaped half of applied-capability assembly.
///
/// [`assemble_applied_regions`] drives it; a host implements it and never
/// re-states the join, its order, or the budget rule.
pub trait AppliedRegionAssembler {
    /// The host's own planned region.
    type Region;
    /// What the host's encoder resolved for a region.
    type Resolved;
    /// The per-region descriptor this host puts on the wire.
    type Descriptor;
    /// The host's own typed assembly failure.
    type Error;

    /// The session monitor id both joins match on.
    fn session_monitor_id(region: &Self::Region) -> SessionMonitorId;

    /// This host's evidence that a planned region has no media plan.
    ///
    /// Raised when the region has no resolved media, or no negotiated plan:
    /// in both cases the region's encoder did not reach a state the applied
    /// capability can describe.
    fn missing_media_plan(&self, region: &Self::Region) -> Self::Error;

    /// Builds this host's wire descriptor for one fully joined region.
    ///
    /// # Errors
    ///
    /// Returns this host's typed failure when the region cannot be described
    /// on the wire.
    fn describe(
        &self,
        region: AppliedRegion<'_, Self::Region, Self::Resolved>,
    ) -> Result<Self::Descriptor, Self::Error>;
}

/// Joins every planned region to its resolved media and its negotiated media
/// plan, in plan order, and describes each one.
///
/// Assembly is all-or-nothing, exactly like admission: the first region that
/// cannot be joined or described fails the whole capability, so a host never
/// publishes an applied topology describing a subset of the regions it
/// committed to.
///
/// # Errors
///
/// Returns [`AppliedRegionAssembler::missing_media_plan`] for the first
/// planned region that has no resolved media or no negotiated media plan, or
/// whatever [`AppliedRegionAssembler::describe`] refused with.
pub fn assemble_applied_regions<A: AppliedRegionAssembler + ?Sized>(
    assembler: &A,
    regions: &[A::Region],
    resolved: &[(SessionMonitorId, A::Resolved)],
    negotiated: &RegionMediaRoster,
    translation: OriginTranslation,
) -> Result<Vec<A::Descriptor>, A::Error> {
    let mut descriptors = Vec::with_capacity(regions.len());
    for region in regions {
        let monitor_id = A::session_monitor_id(region);
        let Some((_, resolved)) = resolved.iter().find(|(id, _)| *id == monitor_id) else {
            return Err(assembler.missing_media_plan(region));
        };
        let Some(media) = negotiated.plan(monitor_id) else {
            return Err(assembler.missing_media_plan(region));
        };
        descriptors.push(assembler.describe(AppliedRegion {
            region,
            resolved,
            media: &media,
            translation,
        })?);
    }
    Ok(descriptors)
}

#[cfg(test)]
mod tests {
    use arcen_media::video::EncoderBackend;
    use arcen_media::{
        BitrateBudgetKbps, ChromaSubsampling, MediaStreamEpoch, RegionMediaPlan, RegionMediaRoster,
        SessionMonitorId, VideoCodec, VideoConfiguration,
    };

    use super::{
        AppliedRegion, AppliedRegionAssembler, OriginTranslation, OriginTranslationOverflow,
        assemble_applied_regions,
    };

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct Region {
        id: SessionMonitorId,
        x: i32,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct Resolved {
        width: u32,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct Descriptor {
        id: u16,
        x: i32,
        width: u32,
        bitrate_kbps: u32,
        stream_epoch: u64,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Error {
        MissingMediaPlan(u16),
        Overflow,
    }

    struct Assembler;

    impl AppliedRegionAssembler for Assembler {
        type Region = Region;
        type Resolved = Resolved;
        type Descriptor = Descriptor;
        type Error = Error;

        fn session_monitor_id(region: &Region) -> SessionMonitorId {
            region.id
        }

        fn missing_media_plan(&self, region: &Region) -> Error {
            Error::MissingMediaPlan(region.id.get())
        }

        fn describe(
            &self,
            region: AppliedRegion<'_, Region, Resolved>,
        ) -> Result<Descriptor, Error> {
            Ok(Descriptor {
                id: region.region.id.get(),
                x: region
                    .translation
                    .apply_x(region.region.x)
                    .map_err(|OriginTranslationOverflow| Error::Overflow)?,
                width: region.resolved.width,
                bitrate_kbps: region.bitrate_kbps(),
                stream_epoch: region.stream_epoch(),
            })
        }
    }

    fn monitor_id(value: u16) -> SessionMonitorId {
        SessionMonitorId::new(value).expect("nonzero monitor id")
    }

    fn media(id: u16, budget: u32) -> RegionMediaPlan {
        RegionMediaPlan::new(
            monitor_id(id),
            MediaStreamEpoch::new(7).expect("nonzero epoch"),
            EncoderBackend::NativeNvenc,
            VideoConfiguration {
                codec: VideoCodec::H264,
                chroma: ChromaSubsampling::Yuv420,
                ..VideoConfiguration::legacy_h264()
            },
            1_920,
            1_080,
            60,
            BitrateBudgetKbps::new(budget).expect("in-band budget"),
        )
        .expect("valid region media plan")
    }

    #[test]
    fn an_already_normalised_desktop_is_never_translated() {
        assert_eq!(OriginTranslation::to_origin(0, 0), OriginTranslation::NONE);
        assert_eq!(
            OriginTranslation::to_origin(1_920, 64),
            OriginTranslation::NONE
        );
        assert_eq!(OriginTranslation::NONE.apply_x(-5), Ok(-5));
    }

    #[test]
    fn only_a_negative_axis_shifts() {
        let translation = OriginTranslation::to_origin(-1_920, 32);
        assert_eq!(translation.x(), 1_920);
        assert_eq!(translation.y(), 0);
        assert_eq!(translation.apply_x(-1_920), Ok(0));
        assert_eq!(translation.apply_y(600), Ok(600));
    }

    #[test]
    fn a_translation_that_leaves_the_published_range_is_refused() {
        let translation = OriginTranslation::to_origin(i32::MIN, 0);
        assert_eq!(
            translation.apply_x(i32::MAX),
            Err(OriginTranslationOverflow)
        );
    }

    #[test]
    fn every_region_is_joined_in_plan_order_and_publishes_its_negotiated_budget() {
        let regions = [
            Region {
                id: monitor_id(2),
                x: -1_920,
            },
            Region {
                id: monitor_id(1),
                x: 0,
            },
        ];
        let resolved = [
            (monitor_id(1), Resolved { width: 1_920 }),
            (monitor_id(2), Resolved { width: 1_280 }),
        ];
        let negotiated =
            RegionMediaRoster::new(vec![media(1, 8_000), media(2, 3_500)]).expect("roster");

        let descriptors = assemble_applied_regions(
            &Assembler,
            &regions,
            &resolved,
            &negotiated,
            OriginTranslation::to_origin(-1_920, 0),
        )
        .expect("every region joins");

        assert_eq!(
            descriptors,
            vec![
                Descriptor {
                    id: 2,
                    x: 0,
                    width: 1_280,
                    bitrate_kbps: 3_500,
                    stream_epoch: 7,
                },
                Descriptor {
                    id: 1,
                    x: 1_920,
                    width: 1_920,
                    bitrate_kbps: 8_000,
                    stream_epoch: 7,
                },
            ],
            "regions keep plan order and each publishes its own negotiated budget"
        );
    }

    #[test]
    fn a_region_without_resolved_media_fails_the_whole_capability() {
        let regions = [
            Region {
                id: monitor_id(1),
                x: 0,
            },
            Region {
                id: monitor_id(2),
                x: 1_920,
            },
        ];
        let resolved = [(monitor_id(1), Resolved { width: 1_920 })];
        let negotiated =
            RegionMediaRoster::new(vec![media(1, 8_000), media(2, 3_500)]).expect("roster");

        assert_eq!(
            assemble_applied_regions(
                &Assembler,
                &regions,
                &resolved,
                &negotiated,
                OriginTranslation::NONE,
            ),
            Err(Error::MissingMediaPlan(2))
        );
    }

    #[test]
    fn a_region_without_a_negotiated_plan_fails_the_whole_capability() {
        let regions = [
            Region {
                id: monitor_id(1),
                x: 0,
            },
            Region {
                id: monitor_id(2),
                x: 1_920,
            },
        ];
        let resolved = [
            (monitor_id(1), Resolved { width: 1_920 }),
            (monitor_id(2), Resolved { width: 1_280 }),
        ];
        let negotiated = RegionMediaRoster::new(vec![media(1, 8_000)]).expect("roster");

        assert_eq!(
            assemble_applied_regions(
                &Assembler,
                &regions,
                &resolved,
                &negotiated,
                OriginTranslation::NONE,
            ),
            Err(Error::MissingMediaPlan(2))
        );
    }
}
