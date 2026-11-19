// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Rectangles around clusters of `Motion` blocks, stable enough to be
//! encoder surfaces.
//!
//! A surface that resizes every frame costs a keyframe every frame. Regions
//! are therefore quantised to 64-pixel cells, grow at once, shrink only after
//! the smaller shape has held for a while, and close only after motion has
//! been gone for a while. Surface policy (codec, scaling, rate) stays with
//! the planner; this is geometry.

use crate::{BLOCK_SIZE, BlockClass, BlockClassMap, BlockGrid, KeelError, PixelRect};

/// Region edge quantum in pixels.
pub const REGION_QUANTUM: usize = 64;
/// Most regions per monitor.
pub const MAX_MOTION_REGIONS: usize = 2;

const CELL_BLOCKS: usize = REGION_QUANTUM / BLOCK_SIZE;

/// Tunables for [`MotionRegions`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RegionConfig {
    /// Fewest `Motion` blocks a cluster needs to become a region; stray
    /// carets and clocks stay tiles.
    pub min_motion_blocks: usize,
    /// Updates a smaller shape must hold before a region shrinks to it.
    pub shrink_after: u16,
    /// Updates without motion before a region closes.
    pub close_after: u16,
    /// Most regions (at most [`MAX_MOTION_REGIONS`]).
    pub max_regions: usize,
}

impl Default for RegionConfig {
    fn default() -> Self {
        Self {
            min_motion_blocks: 16,
            shrink_after: 30,
            close_after: 30,
            max_regions: MAX_MOTION_REGIONS,
        }
    }
}

/// One stable motion rectangle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MotionRegion {
    /// Stable for the region's life; never reused within one
    /// [`MotionRegions`].
    pub id: u32,
    /// 64-pixel aligned, clipped to the frame.
    pub rect: PixelRect,
    /// The rect changed size or position in the latest update (a new
    /// region counts as changed).
    pub changed: bool,
}

#[derive(Clone, Copy, Debug)]
struct Tracked {
    region: MotionRegion,
    /// Union of targets seen while every target fit inside the region.
    shrink_to: Option<CellRect>,
    shrink_streak: u16,
    idle_streak: u16,
}

/// Inclusive cell bounds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CellRect {
    x0: usize,
    y0: usize,
    x1: usize,
    y1: usize,
}

impl CellRect {
    const fn union(self, other: Self) -> Self {
        Self {
            x0: if self.x0 < other.x0 {
                self.x0
            } else {
                other.x0
            },
            y0: if self.y0 < other.y0 {
                self.y0
            } else {
                other.y0
            },
            x1: if self.x1 > other.x1 {
                self.x1
            } else {
                other.x1
            },
            y1: if self.y1 > other.y1 {
                self.y1
            } else {
                other.y1
            },
        }
    }

    const fn contains(self, other: Self) -> bool {
        self.x0 <= other.x0 && self.y0 <= other.y0 && self.x1 >= other.x1 && self.y1 >= other.y1
    }

    const fn overlaps(self, other: Self) -> bool {
        self.x0 <= other.x1 && other.x0 <= self.x1 && self.y0 <= other.y1 && other.y0 <= self.y1
    }

    const fn area(self) -> usize {
        (self.x1 - self.x0 + 1) * (self.y1 - self.y0 + 1)
    }
}

#[derive(Clone, Copy, Debug)]
struct Cluster {
    cells: CellRect,
    blocks: usize,
}

/// Up to [`MAX_MOTION_REGIONS`] stable rectangles around `Motion` blocks.
/// No allocation after construction.
#[derive(Debug)]
pub struct MotionRegions {
    grid: BlockGrid,
    config: RegionConfig,
    cells_wide: usize,
    cells_tall: usize,
    counts: Vec<u16>,
    labels: Vec<u16>,
    stack: Vec<u32>,
    clusters: Vec<Cluster>,
    tracked: Vec<Tracked>,
    regions: Vec<MotionRegion>,
    next_id: u32,
}

impl MotionRegions {
    #[must_use]
    pub fn new(grid: BlockGrid, config: RegionConfig) -> Self {
        let cells_wide = grid.blocks_wide().div_ceil(CELL_BLOCKS);
        let cells_tall = grid.blocks_tall().div_ceil(CELL_BLOCKS);
        let cells = cells_wide * cells_tall;
        let max_regions = config.max_regions.clamp(1, MAX_MOTION_REGIONS);
        Self {
            grid,
            config: RegionConfig {
                max_regions,
                ..config
            },
            cells_wide,
            cells_tall,
            counts: vec![0; cells],
            labels: vec![0; cells],
            stack: Vec::with_capacity(cells),
            clusters: Vec::with_capacity(cells),
            tracked: Vec::with_capacity(MAX_MOTION_REGIONS * 2),
            regions: Vec::with_capacity(MAX_MOTION_REGIONS),
            next_id: 1,
        }
    }

    #[must_use]
    pub const fn config(&self) -> RegionConfig {
        self.config
    }

    /// Regions after the latest update.
    #[must_use]
    pub fn regions(&self) -> &[MotionRegion] {
        &self.regions
    }

    /// Closes every region.
    pub fn reset(&mut self) {
        self.tracked.clear();
        self.regions.clear();
    }

    /// Recomputes regions from the latest block classes.
    ///
    /// # Errors
    ///
    /// Returns [`KeelError::GeometryChanged`] without mutating state when the
    /// class map has another geometry.
    pub fn update(&mut self, classes: &BlockClassMap) -> Result<&[MotionRegion], KeelError> {
        if classes.grid() != self.grid {
            return Err(KeelError::GeometryChanged {
                expected: self.grid,
                actual: classes.grid(),
            });
        }
        self.count_cells(classes);
        self.cluster();
        self.track();
        self.regions.clear();
        for tracked in &self.tracked {
            self.regions.push(tracked.region);
        }
        Ok(&self.regions)
    }

    fn count_cells(&mut self, classes: &BlockClassMap) {
        self.counts.fill(0);
        let blocks_wide = self.grid.blocks_wide();
        for index in classes.active_blocks() {
            if classes.class(index) == Some(BlockClass::Motion) {
                let cell = (index / blocks_wide / CELL_BLOCKS) * self.cells_wide
                    + (index % blocks_wide) / CELL_BLOCKS;
                self.counts[cell] = self.counts[cell].saturating_add(1);
            }
        }
    }

    /// Connected components of cells with motion, joining cells up to one
    /// empty cell apart, then merged down to `max_regions` by least added
    /// area.
    fn cluster(&mut self) {
        self.labels.fill(0);
        self.clusters.clear();
        for start in 0..self.counts.len() {
            if self.counts[start] == 0 || self.labels[start] != 0 {
                continue;
            }
            let label = u16::try_from(self.clusters.len() + 1).unwrap_or(u16::MAX);
            let (x, y) = (start % self.cells_wide, start / self.cells_wide);
            let mut cluster = Cluster {
                cells: CellRect {
                    x0: x,
                    y0: y,
                    x1: x,
                    y1: y,
                },
                blocks: 0,
            };
            self.labels[start] = label;
            self.stack.clear();
            self.stack.push(u32::try_from(start).unwrap_or(u32::MAX));
            while let Some(cell) = self.stack.pop() {
                let cell = cell as usize;
                let (x, y) = (cell % self.cells_wide, cell / self.cells_wide);
                cluster.blocks += usize::from(self.counts[cell]);
                cluster.cells = cluster.cells.union(CellRect {
                    x0: x,
                    y0: y,
                    x1: x,
                    y1: y,
                });
                for ny in y.saturating_sub(2)..=(y + 2).min(self.cells_tall - 1) {
                    for nx in x.saturating_sub(2)..=(x + 2).min(self.cells_wide - 1) {
                        let neighbour = ny * self.cells_wide + nx;
                        if self.counts[neighbour] != 0 && self.labels[neighbour] == 0 {
                            self.labels[neighbour] = label;
                            self.stack
                                .push(u32::try_from(neighbour).unwrap_or(u32::MAX));
                        }
                    }
                }
            }
            self.clusters.push(cluster);
        }
        let min_blocks = self.config.min_motion_blocks;
        self.clusters.retain(|cluster| cluster.blocks >= min_blocks);
        while self.clusters.len() > self.config.max_regions {
            let mut best = (0, 1, usize::MAX);
            for a in 0..self.clusters.len() {
                for b in a + 1..self.clusters.len() {
                    let (left, right) = (self.clusters[a].cells, self.clusters[b].cells);
                    let added = left
                        .union(right)
                        .area()
                        .saturating_sub(left.area() + right.area());
                    if added < best.2 {
                        best = (a, b, added);
                    }
                }
            }
            let merged = self.clusters.swap_remove(best.1);
            let into = &mut self.clusters[best.0];
            into.cells = into.cells.union(merged.cells);
            into.blocks += merged.blocks;
        }
    }

    /// Applies hysteresis: match clusters to tracked regions, grow at once,
    /// shrink and close late.
    fn track(&mut self) {
        for tracked in &mut self.tracked {
            tracked.region.changed = false;
        }
        let mut matched = [false; MAX_MOTION_REGIONS * 2];
        for cluster in &self.clusters {
            let target = cluster.cells;
            let found = self
                .tracked
                .iter()
                .position(|tracked| self.cells_of(tracked.region.rect).overlaps(target));
            match found {
                Some(slot) => {
                    matched[slot] = true;
                    let current = self.cells_of(self.tracked[slot].region.rect);
                    let tracked = &mut self.tracked[slot];
                    tracked.idle_streak = 0;
                    if current.contains(target) {
                        let pending = tracked
                            .shrink_to
                            .map_or(target, |pending| pending.union(target));
                        tracked.shrink_to = Some(pending);
                        tracked.shrink_streak = tracked.shrink_streak.saturating_add(1);
                        if tracked.shrink_streak >= self.config.shrink_after {
                            if pending != current {
                                tracked.region.rect = pixel_rect(self.grid, pending);
                                tracked.region.changed = true;
                            }
                            tracked.shrink_to = None;
                            tracked.shrink_streak = 0;
                        }
                    } else {
                        tracked.region.rect = pixel_rect(self.grid, current.union(target));
                        tracked.region.changed = true;
                        tracked.shrink_to = None;
                        tracked.shrink_streak = 0;
                    }
                }
                None if self.tracked.len() < self.config.max_regions => {
                    let id = self.next_id;
                    self.next_id = self.next_id.wrapping_add(1).max(1);
                    matched[self.tracked.len()] = true;
                    self.tracked.push(Tracked {
                        region: MotionRegion {
                            id,
                            rect: pixel_rect(self.grid, target),
                            changed: true,
                        },
                        shrink_to: None,
                        shrink_streak: 0,
                        idle_streak: 0,
                    });
                }
                None => {
                    // More clusters than free regions: grow the nearest.
                    if let Some(slot) = (0..self.tracked.len()).min_by_key(|slot| {
                        let cells = self.cells_of(self.tracked[*slot].region.rect);
                        cells.union(target).area() - cells.area()
                    }) {
                        matched[slot] = true;
                        let cells = self.cells_of(self.tracked[slot].region.rect);
                        let tracked = &mut self.tracked[slot];
                        tracked.region.rect = pixel_rect(self.grid, cells.union(target));
                        tracked.region.changed = true;
                        tracked.idle_streak = 0;
                        tracked.shrink_to = None;
                        tracked.shrink_streak = 0;
                    }
                }
            }
        }
        let close_after = self.config.close_after;
        let mut slot = 0;
        while slot < self.tracked.len() {
            if !matched[slot] {
                self.tracked[slot].idle_streak = self.tracked[slot].idle_streak.saturating_add(1);
            }
            if self.tracked[slot].idle_streak >= close_after {
                self.tracked.remove(slot);
                matched.copy_within(slot + 1.., slot);
            } else {
                slot += 1;
            }
        }
        self.merge_overlapping();
    }

    /// Two regions that grew into each other become one (the older id).
    fn merge_overlapping(&mut self) {
        if self.tracked.len() == 2 {
            let (a, b) = (
                self.cells_of(self.tracked[0].region.rect),
                self.cells_of(self.tracked[1].region.rect),
            );
            if a.overlaps(b) {
                let keep = usize::from(self.tracked[1].region.id < self.tracked[0].region.id);
                let rect = pixel_rect(self.grid, a.union(b));
                self.tracked.swap_remove(1 - keep);
                let tracked = &mut self.tracked[0];
                tracked.region.rect = rect;
                tracked.region.changed = true;
                tracked.shrink_to = None;
                tracked.shrink_streak = 0;
                tracked.idle_streak = 0;
            }
        }
    }

    fn cells_of(&self, rect: PixelRect) -> CellRect {
        CellRect {
            x0: rect.x / REGION_QUANTUM,
            y0: rect.y / REGION_QUANTUM,
            x1: (rect.right().saturating_sub(1) / REGION_QUANTUM).min(self.cells_wide - 1),
            y1: (rect.bottom().saturating_sub(1) / REGION_QUANTUM).min(self.cells_tall - 1),
        }
    }
}

fn pixel_rect(grid: BlockGrid, cells: CellRect) -> PixelRect {
    let x = cells.x0 * REGION_QUANTUM;
    let y = cells.y0 * REGION_QUANTUM;
    let right = ((cells.x1 + 1) * REGION_QUANTUM).min(grid.width());
    let bottom = ((cells.y1 + 1) * REGION_QUANTUM).min(grid.height());
    PixelRect {
        x,
        y,
        width: right - x,
        height: bottom - y,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use core::time::Duration;

    use super::*;
    use crate::{BlockHistory, ClassConfig, ExternalDamage};

    struct Rig {
        history: BlockHistory,
        classes: BlockClassMap,
        regions: MotionRegions,
        now: Duration,
    }

    impl Rig {
        fn new(config: RegionConfig) -> Self {
            let grid = BlockGrid::new(640, 360).unwrap();
            Self {
                history: BlockHistory::new(grid),
                classes: BlockClassMap::new(grid, ClassConfig::default()),
                regions: MotionRegions::new(grid, config),
                now: Duration::ZERO,
            }
        }

        fn step(&mut self, rects: &[PixelRect]) -> Vec<MotionRegion> {
            let mut damage = ExternalDamage::new(640, 360).unwrap();
            for rect in rects {
                damage.mark_rect(*rect);
            }
            self.now += Duration::from_millis(16);
            self.history.observe(damage.damage_map(), self.now).unwrap();
            self.classes
                .update(&self.history, damage.damage_map(), &[])
                .unwrap();
            self.regions.update(&self.classes).unwrap().to_vec()
        }
    }

    const VIDEO: PixelRect = PixelRect {
        x: 100,
        y: 70,
        width: 200,
        height: 120,
    };

    #[test]
    fn a_video_becomes_one_quantised_region_that_holds_still() {
        let mut rig = Rig::new(RegionConfig::default());
        let mut regions = Vec::new();
        for _ in 0..10 {
            regions = rig.step(&[VIDEO]);
        }
        let [region] = regions.as_slice() else {
            panic!("one region expected, got {regions:?}");
        };
        assert_eq!(
            region.rect,
            PixelRect {
                x: 64,
                y: 64,
                width: 256,
                height: 128
            }
        );
        assert!(!region.changed, "a steady video does not resize its region");
    }

    #[test]
    fn regions_grow_at_once_shrink_late_and_close_late() {
        let config = RegionConfig {
            shrink_after: 5,
            close_after: 4,
            ..RegionConfig::default()
        };
        let mut rig = Rig::new(config);
        for _ in 0..4 {
            rig.step(&[VIDEO]);
        }
        let wider = PixelRect {
            width: 400,
            ..VIDEO
        };
        let mut grown = Vec::new();
        for _ in 0..6 {
            grown = rig.step(&[wider]);
        }
        assert!(
            grown[0].rect.width >= 320,
            "grows on the first frame: {grown:?}"
        );
        let id = grown[0].id;
        let mut sizes = Vec::new();
        for _ in 0..30 {
            sizes.push(rig.step(&[VIDEO])[0].rect.width);
        }
        assert!(sizes[..4].iter().all(|width| *width >= 320), "{sizes:?}");
        assert_eq!(*sizes.last().unwrap(), 256, "{sizes:?}");
        assert!(rig.regions.regions().iter().all(|region| region.id == id));
        let mut closed_after = None;
        for step in 0..40 {
            if rig.step(&[]).is_empty() {
                closed_after = Some(step);
                break;
            }
        }
        assert!(
            closed_after.is_some_and(|step| step >= 4),
            "{closed_after:?}"
        );
    }

    #[test]
    fn at_most_two_regions_and_strays_are_ignored() {
        let mut rig = Rig::new(RegionConfig::default());
        let caret = PixelRect {
            x: 600,
            y: 300,
            width: 2,
            height: 14,
        };
        let second = PixelRect {
            x: 400,
            y: 250,
            width: 120,
            height: 80,
        };
        let third = PixelRect {
            x: 500,
            y: 20,
            width: 120,
            height: 40,
        };
        let mut regions = Vec::new();
        for _ in 0..6 {
            regions = rig.step(&[VIDEO, second, third, caret]);
        }
        assert!(regions.len() <= MAX_MOTION_REGIONS, "{regions:?}");
        for region in &regions {
            assert_eq!(region.rect.x % REGION_QUANTUM, 0);
            assert_eq!(region.rect.y % REGION_QUANTUM, 0);
        }
        let covers = |rect: PixelRect| {
            regions.iter().any(|region| {
                region.rect.x <= rect.x
                    && region.rect.y <= rect.y
                    && region.rect.right() >= rect.right()
                    && region.rect.bottom() >= rect.bottom()
            })
        };
        assert!(
            covers(VIDEO) && covers(second) && covers(third),
            "{regions:?}"
        );
    }
}
