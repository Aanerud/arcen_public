use core::cmp::Ordering;
use std::error::Error;
use std::fmt::{Display, Formatter};

use arcen_keel::{
    ActivityDiagnostics, ActivityGrid, ActivityHint, BgraFrame, BlockGrid, DamageMap,
    DamageTracker, HashKernel, KeelError, KernelPreference,
};

use crate::{RegionGeneration, RegionId};

/// Identity of the region generation that owns one activity grid.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RegionActivityOwner {
    generation: RegionGeneration,
    region_id: RegionId,
}

impl RegionActivityOwner {
    #[must_use]
    pub const fn new(generation: RegionGeneration, region_id: RegionId) -> Self {
        Self {
            generation,
            region_id,
        }
    }

    #[must_use]
    pub const fn generation(self) -> RegionGeneration {
        self.generation
    }

    #[must_use]
    pub const fn region_id(self) -> RegionId {
        self.region_id
    }
}

/// Fixed-size region-scoped activity diagnostics for aggregate schedulers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RegionActivityDiagnostics {
    pub generation: RegionGeneration,
    pub region_id: RegionId,
    pub grid: BlockGrid,
    pub activity: ActivityDiagnostics,
}

/// Region ownership or damage-update failure.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum RegionActivityError {
    StaleGeneration {
        current: RegionGeneration,
        received: RegionGeneration,
    },
    UnexpectedGeneration {
        current: RegionGeneration,
        received: RegionGeneration,
    },
    RegionMismatch {
        expected: RegionId,
        received: RegionId,
    },
    Keel(KeelError),
}

impl Display for RegionActivityError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::StaleGeneration { current, received } => write!(
                formatter,
                "stale region generation {}, current generation is {}",
                received.get(),
                current.get()
            ),
            Self::UnexpectedGeneration { current, received } => write!(
                formatter,
                "region generation {} has not replaced current generation {}",
                received.get(),
                current.get()
            ),
            Self::RegionMismatch { expected, received } => write!(
                formatter,
                "region id {} does not match activity owner {}",
                received.get(),
                expected.get()
            ),
            Self::Keel(error) => Display::fmt(error, formatter),
        }
    }
}

impl Error for RegionActivityError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Keel(error) => Some(error),
            Self::StaleGeneration { .. }
            | Self::UnexpectedGeneration { .. }
            | Self::RegionMismatch { .. } => None,
        }
    }
}

impl From<KeelError> for RegionActivityError {
    fn from(error: KeelError) -> Self {
        Self::Keel(error)
    }
}

/// Region-owned 16x16 damage map, activity window, and cadence recommendation.
#[derive(Debug)]
pub struct RegionActivityGrid {
    owner: RegionActivityOwner,
    activity: ActivityGrid,
}

impl RegionActivityGrid {
    /// Creates a region-owned grid with one reusable [`DamageTracker`].
    ///
    /// # Errors
    ///
    /// Returns the geometry errors documented by [`BlockGrid::new`].
    pub fn new(
        owner: RegionActivityOwner,
        width: usize,
        height: usize,
        preference: KernelPreference,
    ) -> Result<Self, RegionActivityError> {
        Ok(Self {
            owner,
            activity: ActivityGrid::new(width, height, preference)?,
        })
    }

    /// Promotes an existing damage tracker into region-owned activity state.
    #[must_use]
    pub fn from_damage_tracker(owner: RegionActivityOwner, damage: DamageTracker) -> Self {
        Self {
            owner,
            activity: ActivityGrid::from_damage_tracker(damage),
        }
    }

    #[must_use]
    pub const fn owner(&self) -> RegionActivityOwner {
        self.owner
    }

    #[must_use]
    pub const fn grid(&self) -> BlockGrid {
        self.activity.grid()
    }

    #[must_use]
    pub const fn kernel(&self) -> HashKernel {
        self.activity.kernel()
    }

    #[must_use]
    pub fn damage_map(&self) -> DamageMap<'_> {
        self.activity.damage_map()
    }

    #[must_use]
    pub const fn diagnostics(&self) -> RegionActivityDiagnostics {
        RegionActivityDiagnostics {
            generation: self.owner.generation,
            region_id: self.owner.region_id,
            grid: self.activity.grid(),
            activity: self.activity.diagnostics(),
        }
    }

    /// Clears retained damage and activity state without changing ownership.
    pub fn reset(&mut self) {
        self.activity.reset();
    }

    /// Rebinds this allocation to a strictly newer region generation.
    ///
    /// The 16x16 geometry is retained. Construct a new grid when region
    /// dimensions change.
    ///
    /// # Errors
    ///
    /// Returns [`RegionActivityError::StaleGeneration`] unless `owner` belongs
    /// to a strictly newer generation.
    pub fn rebind(&mut self, owner: RegionActivityOwner) -> Result<(), RegionActivityError> {
        if owner.generation <= self.owner.generation {
            return Err(RegionActivityError::StaleGeneration {
                current: self.owner.generation,
                received: owner.generation,
            });
        }
        self.owner = owner;
        self.activity.reset();
        Ok(())
    }

    /// Updates activity after validating the owning generation and region id.
    ///
    /// # Errors
    ///
    /// Returns an ownership error before touching damage state, or a Keel
    /// geometry error without changing activity state.
    pub fn update(
        &mut self,
        owner: RegionActivityOwner,
        frame: BgraFrame<'_>,
    ) -> Result<RegionActivityDiagnostics, RegionActivityError> {
        self.update_with_hint(owner, frame, ActivityHint::None)
    }

    /// Updates activity with source-provided scroll knowledge.
    ///
    /// # Errors
    ///
    /// Returns an ownership error before touching damage state, or a Keel
    /// geometry error without changing activity state.
    pub fn update_with_hint(
        &mut self,
        owner: RegionActivityOwner,
        frame: BgraFrame<'_>,
        hint: ActivityHint,
    ) -> Result<RegionActivityDiagnostics, RegionActivityError> {
        self.validate_owner(owner)?;
        self.activity.update_with_hint(frame, hint)?;
        Ok(self.diagnostics())
    }

    fn validate_owner(&self, received: RegionActivityOwner) -> Result<(), RegionActivityError> {
        match received.generation.cmp(&self.owner.generation) {
            Ordering::Less => {
                return Err(RegionActivityError::StaleGeneration {
                    current: self.owner.generation,
                    received: received.generation,
                });
            }
            Ordering::Greater => {
                return Err(RegionActivityError::UnexpectedGeneration {
                    current: self.owner.generation,
                    received: received.generation,
                });
            }
            Ordering::Equal => {}
        }
        if received.region_id != self.owner.region_id {
            return Err(RegionActivityError::RegionMismatch {
                expected: self.owner.region_id,
                received: received.region_id,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcen_keel::{ActivityClass, CadenceRecommendation};

    fn owner(generation: u64, region_id: u32) -> RegionActivityOwner {
        RegionActivityOwner::new(
            RegionGeneration::new(generation).expect("generation"),
            RegionId::new(region_id).expect("region id"),
        )
    }

    fn frame(pixels: &[u8], width: usize, height: usize) -> BgraFrame<'_> {
        BgraFrame::new(pixels, width, height, width * 4).expect("frame")
    }

    #[test]
    fn stale_and_future_owners_do_not_mutate_activity() {
        const WIDTH: usize = 32;
        const HEIGHT: usize = 32;
        let pixels = vec![0u8; WIDTH * HEIGHT * 4];
        let current = owner(2, 7);
        let mut grid =
            RegionActivityGrid::new(current, WIDTH, HEIGHT, KernelPreference::Xxh3).expect("grid");
        grid.update(current, frame(&pixels, WIDTH, HEIGHT))
            .expect("baseline");
        let before = grid.diagnostics();

        assert!(matches!(
            grid.update(owner(1, 7), frame(&pixels, WIDTH, HEIGHT)),
            Err(RegionActivityError::StaleGeneration { .. })
        ));
        assert!(matches!(
            grid.update(owner(3, 7), frame(&pixels, WIDTH, HEIGHT)),
            Err(RegionActivityError::UnexpectedGeneration { .. })
        ));
        assert!(matches!(
            grid.update(owner(2, 8), frame(&pixels, WIDTH, HEIGHT)),
            Err(RegionActivityError::RegionMismatch { .. })
        ));
        assert_eq!(grid.diagnostics(), before);
    }

    #[test]
    fn reset_and_rebind_clear_history_and_force_a_new_baseline() {
        const WIDTH: usize = 32;
        const HEIGHT: usize = 32;
        let mut pixels = vec![0u8; WIDTH * HEIGHT * 4];
        let first = owner(4, 9);
        let mut grid =
            RegionActivityGrid::new(first, WIDTH, HEIGHT, KernelPreference::Xxh3).expect("grid");
        grid.update(first, frame(&pixels, WIDTH, HEIGHT))
            .expect("baseline");
        pixels[0] = 1;
        grid.update(first, frame(&pixels, WIDTH, HEIGHT))
            .expect("activity");
        assert_ne!(
            grid.diagnostics().activity.rolling_dirty_ratio,
            arcen_keel::DirtyRatio::ZERO
        );

        grid.reset();
        let reset = grid.diagnostics();
        assert_eq!(reset.activity.update_sequence, 0);
        assert_eq!(reset.activity.class, ActivityClass::Idle);
        assert_eq!(reset.activity.cadence, CadenceRecommendation::Keepalive);
        assert_eq!(
            reset.activity.rolling_dirty_ratio,
            arcen_keel::DirtyRatio::ZERO
        );
        let refreshed = grid
            .update(first, frame(&pixels, WIDTH, HEIGHT))
            .expect("refreshed baseline");
        assert!(refreshed.activity.baseline_refresh);
        assert_eq!(refreshed.activity.cadence, CadenceRecommendation::Immediate);

        let second = owner(5, 10);
        grid.rebind(second).expect("new generation");
        assert_eq!(grid.owner(), second);
        assert_eq!(grid.diagnostics().activity.update_sequence, 0);
        assert!(matches!(
            grid.update(first, frame(&pixels, WIDTH, HEIGHT)),
            Err(RegionActivityError::StaleGeneration { .. })
        ));
        assert!(matches!(
            grid.rebind(second),
            Err(RegionActivityError::StaleGeneration { .. })
        ));
    }
}
