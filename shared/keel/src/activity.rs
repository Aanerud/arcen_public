use crate::{
    BgraFrame, BlockGrid, DamageMap, DamageSummary, DamageTracker, HashKernel, KeelError,
    KernelPreference,
};

/// Number of post-baseline observations retained for the rolling dirty ratio.
pub const ACTIVITY_ROLLING_WINDOW: usize = 8;
/// Number of fixed-point units in a complete dirty ratio.
pub const DIRTY_RATIO_BASIS_POINTS: u16 = 10_000;

const FULL_MOTION_ROLLING_BASIS_POINTS: u16 = 6_000;
const FULL_MOTION_INSTANT_BASIS_POINTS: u16 = 7_500;
const SCROLL_MIN_DIRTY_BASIS_POINTS: u16 = 1_250;
const SCROLL_MIN_ROW_BASIS_POINTS: u16 = 5_000;

/// Coarse content activity used by pacing and aggregate schedulers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActivityClass {
    Idle,
    Sparse,
    Scroll,
    FullMotion,
}

/// Optional source knowledge that cannot be inferred reliably from hashes.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ActivityHint {
    #[default]
    None,
    /// The source reported scroll or moved-rectangle activity.
    Scroll,
}

/// Semantic cadence advice; consumers retain authority over concrete frame rates.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CadenceRecommendation {
    /// No changed content; retain only the product's bounded keepalive.
    Keepalive,
    /// A new baseline must be submitted without waiting for history.
    Immediate,
    /// Sparse interaction should remain latency-responsive.
    Responsive,
    /// Scroll-like activity benefits from smooth repeated service.
    Smooth,
    /// Sustained broad motion should receive continuous service.
    Continuous,
}

/// A dirty-block ratio in basis points (`10_000 == 100%`).
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub struct DirtyRatio(u16);

impl DirtyRatio {
    pub const ZERO: Self = Self(0);
    pub const FULL: Self = Self(DIRTY_RATIO_BASIS_POINTS);

    #[must_use]
    pub const fn from_basis_points(value: u16) -> Option<Self> {
        if value <= DIRTY_RATIO_BASIS_POINTS {
            Some(Self(value))
        } else {
            None
        }
    }

    #[must_use]
    pub const fn basis_points(self) -> u16 {
        self.0
    }

    #[must_use]
    pub fn as_fraction(self) -> f64 {
        f64::from(self.0) / f64::from(DIRTY_RATIO_BASIS_POINTS)
    }

    #[allow(clippy::cast_lossless)]
    fn from_counts(dirty: usize, total: usize) -> Self {
        if dirty == 0 || total == 0 {
            return Self::ZERO;
        }
        let dirty = dirty as u128;
        let total = total as u128;
        let scale = u128::from(DIRTY_RATIO_BASIS_POINTS);
        let rounded = (dirty * scale + total / 2) / total;
        Self(u16::try_from(rounded.min(scale)).unwrap_or(DIRTY_RATIO_BASIS_POINTS))
    }
}

/// Fixed-size activity state suitable for bounded aggregate diagnostics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ActivityDiagnostics {
    pub update_sequence: u64,
    pub summary: DamageSummary,
    pub current_dirty_ratio: DirtyRatio,
    pub rolling_dirty_ratio: DirtyRatio,
    pub class: ActivityClass,
    pub cadence: CadenceRecommendation,
    pub rolling_samples: u8,
    pub class_streak: u16,
    /// True when this update established a fresh full-damage baseline.
    pub baseline_refresh: bool,
}

impl ActivityDiagnostics {
    fn initial(grid: BlockGrid) -> Self {
        Self {
            update_sequence: 0,
            summary: DamageSummary {
                dirty_blocks: 0,
                total_blocks: grid.block_count(),
                dirty_block_rows: 0,
                total_block_rows: grid.blocks_tall(),
            },
            current_dirty_ratio: DirtyRatio::ZERO,
            rolling_dirty_ratio: DirtyRatio::ZERO,
            class: ActivityClass::Idle,
            cadence: CadenceRecommendation::Keepalive,
            rolling_samples: 0,
            class_streak: 0,
            baseline_refresh: false,
        }
    }
}

/// Reusable 16x16 damage and rolling activity state for one frame geometry.
#[derive(Debug)]
pub struct ActivityGrid {
    damage: DamageTracker,
    rolling: RollingRatios,
    diagnostics: ActivityDiagnostics,
    has_baseline: bool,
}

impl ActivityGrid {
    /// Allocates one reusable damage tracker and fixed-size activity window.
    ///
    /// # Errors
    ///
    /// Returns the geometry errors documented by [`BlockGrid::new`].
    pub fn new(
        width: usize,
        height: usize,
        preference: KernelPreference,
    ) -> Result<Self, KeelError> {
        Ok(Self::from_damage_tracker(DamageTracker::new(
            width, height, preference,
        )?))
    }

    /// Promotes an existing damage tracker without allocating more storage.
    #[must_use]
    pub fn from_damage_tracker(damage: DamageTracker) -> Self {
        let diagnostics = ActivityDiagnostics::initial(damage.grid());
        Self {
            damage,
            rolling: RollingRatios::new(),
            diagnostics,
            has_baseline: false,
        }
    }

    #[must_use]
    pub const fn grid(&self) -> BlockGrid {
        self.damage.grid()
    }

    #[must_use]
    pub const fn kernel(&self) -> HashKernel {
        self.damage.kernel()
    }

    #[must_use]
    pub const fn diagnostics(&self) -> ActivityDiagnostics {
        self.diagnostics
    }

    #[must_use]
    pub fn damage_map(&self) -> DamageMap<'_> {
        self.damage.damage_map()
    }

    /// Clears the baseline, rolling history, class, and cadence state in place.
    pub fn reset(&mut self) {
        self.damage.reset();
        self.rolling.reset();
        self.diagnostics = ActivityDiagnostics::initial(self.damage.grid());
        self.has_baseline = false;
    }

    /// Updates damage and activity without an external motion hint.
    ///
    /// # Errors
    ///
    /// Returns [`KeelError::GeometryChanged`] without changing activity state
    /// when the frame geometry differs from this grid.
    pub fn update(&mut self, frame: BgraFrame<'_>) -> Result<ActivityDiagnostics, KeelError> {
        self.update_with_hint(frame, ActivityHint::None)
    }

    /// Updates damage and activity with optional source motion knowledge.
    ///
    /// The first update after construction or [`Self::reset`] establishes the
    /// damage baseline. It is reported as immediate full motion but excluded
    /// from the rolling ratio so startup and generation changes do not bias the
    /// following scheduling window.
    ///
    /// # Errors
    ///
    /// Returns [`KeelError::GeometryChanged`] without changing activity state
    /// when the frame geometry differs from this grid.
    pub fn update_with_hint(
        &mut self,
        frame: BgraFrame<'_>,
        hint: ActivityHint,
    ) -> Result<ActivityDiagnostics, KeelError> {
        let summary = self.damage.update(frame)?;
        let baseline_refresh = !self.has_baseline;
        self.has_baseline = true;

        let current_dirty_ratio =
            DirtyRatio::from_counts(summary.dirty_blocks, summary.total_blocks);
        if !baseline_refresh {
            self.rolling.push(current_dirty_ratio);
        }
        let rolling_dirty_ratio = self.rolling.ratio();
        let class = if baseline_refresh {
            ActivityClass::FullMotion
        } else {
            classify(summary, current_dirty_ratio, rolling_dirty_ratio, hint)
        };
        let cadence = if baseline_refresh {
            CadenceRecommendation::Immediate
        } else {
            cadence_for_class(class)
        };
        let class_streak =
            if self.diagnostics.update_sequence != 0 && self.diagnostics.class == class {
                self.diagnostics.class_streak.saturating_add(1)
            } else {
                1
            };

        self.diagnostics = ActivityDiagnostics {
            update_sequence: self.diagnostics.update_sequence.saturating_add(1),
            summary,
            current_dirty_ratio,
            rolling_dirty_ratio,
            class,
            cadence,
            rolling_samples: self.rolling.len(),
            class_streak,
            baseline_refresh,
        };
        Ok(self.diagnostics)
    }
}

const fn cadence_for_class(class: ActivityClass) -> CadenceRecommendation {
    match class {
        ActivityClass::Idle => CadenceRecommendation::Keepalive,
        ActivityClass::Sparse => CadenceRecommendation::Responsive,
        ActivityClass::Scroll => CadenceRecommendation::Smooth,
        ActivityClass::FullMotion => CadenceRecommendation::Continuous,
    }
}

fn classify(
    summary: DamageSummary,
    current_dirty_ratio: DirtyRatio,
    rolling_dirty_ratio: DirtyRatio,
    hint: ActivityHint,
) -> ActivityClass {
    if summary.is_clean() {
        return ActivityClass::Idle;
    }
    if hint == ActivityHint::Scroll {
        return ActivityClass::Scroll;
    }
    if current_dirty_ratio.basis_points() >= FULL_MOTION_INSTANT_BASIS_POINTS
        || rolling_dirty_ratio.basis_points() >= FULL_MOTION_ROLLING_BASIS_POINTS
    {
        return ActivityClass::FullMotion;
    }

    let dirty_row_ratio =
        DirtyRatio::from_counts(summary.dirty_block_rows, summary.total_block_rows);
    if current_dirty_ratio.basis_points() >= SCROLL_MIN_DIRTY_BASIS_POINTS
        && dirty_row_ratio.basis_points() >= SCROLL_MIN_ROW_BASIS_POINTS
    {
        ActivityClass::Scroll
    } else {
        ActivityClass::Sparse
    }
}

#[derive(Debug)]
struct RollingRatios {
    values: [u16; ACTIVITY_ROLLING_WINDOW],
    sum: u32,
    next: usize,
    len: u8,
}

impl RollingRatios {
    const fn new() -> Self {
        Self {
            values: [0; ACTIVITY_ROLLING_WINDOW],
            sum: 0,
            next: 0,
            len: 0,
        }
    }

    fn reset(&mut self) {
        *self = Self::new();
    }

    fn push(&mut self, ratio: DirtyRatio) {
        if usize::from(self.len) == ACTIVITY_ROLLING_WINDOW {
            self.sum -= u32::from(self.values[self.next]);
        } else {
            self.len += 1;
        }
        self.values[self.next] = ratio.basis_points();
        self.sum += u32::from(ratio.basis_points());
        self.next = (self.next + 1) % ACTIVITY_ROLLING_WINDOW;
    }

    const fn len(&self) -> u8 {
        self.len
    }

    fn ratio(&self) -> DirtyRatio {
        if self.len == 0 {
            return DirtyRatio::ZERO;
        }
        let count = u32::from(self.len);
        let rounded = (self.sum + count / 2) / count;
        DirtyRatio(u16::try_from(rounded).unwrap_or(DIRTY_RATIO_BASIS_POINTS))
    }
}
