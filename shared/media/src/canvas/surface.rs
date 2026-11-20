// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Motion surfaces: the encoder seam and the surface lifecycle.
//!
//! A surface is one H.264 stream painted into part of a canvas (ADR 0028
//! §1). The planner decides where surfaces are, when they open, resize and
//! close; a [`SurfaceEncoder`] only turns pixels into access units. The Pier
//! implements the seam with the shared software H.264 session (and a later
//! backend behind the same trait), so nothing here depends on a codec.
//!
//! Lifecycle rules ([`SurfaceSet::plan`]):
//!
//! - A surface opens on a fresh decoder, and its first paint is an IDR.
//! - It keeps its rectangle while the motion it serves fits inside it and
//!   covers more than [`SHRINK_PERCENT`] of it: Keel's regions already grow
//!   fast and shrink slowly, and a surface does not reopen for every small
//!   shrink.
//! - A surface whose motion moved but kept its size moves without
//!   reopening (the encoder's prediction follows the content).
//! - Anything else, and a change of [`SurfaceScale`], reopens it (an IDR).

use crate::refine::tile::TileRect;
use arcen_keel::BgraFrame;
use arcen_protocol::canvas::{MAX_SURFACE_PIXELS, MAX_SURFACE_SIDE, MAX_SURFACES_PER_MONITOR};
use std::fmt::{Display, Formatter};

/// The canvas block, in pixels.
const BLOCK: u32 = arcen_protocol::canvas::CANVAS_BLOCK as u32;

/// A surface keeps its rectangle while its motion covers more than this
/// share of it.
pub const SHRINK_PERCENT: u64 = 50;

/// How far a surface is encoded below its destination size (design O4).
/// The Deck scales the picture into the destination.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum SurfaceScale {
    /// Source size.
    #[default]
    Full,
    /// Two thirds of each side.
    TwoThirds,
    /// Half of each side.
    Half,
}

impl SurfaceScale {
    /// Every scale, largest first.
    pub const ALL: [Self; 3] = [Self::Full, Self::TwoThirds, Self::Half];

    /// `numerator / denominator` of each side.
    #[must_use]
    pub const fn ratio(self) -> (u32, u32) {
        match self {
            Self::Full => (1, 1),
            Self::TwoThirds => (2, 3),
            Self::Half => (1, 2),
        }
    }

    /// Stable token for Stats and logs.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::TwoThirds => "two_thirds",
            Self::Half => "half",
        }
    }

    /// The next smaller scale, if any.
    #[must_use]
    pub const fn smaller(self) -> Option<Self> {
        match self {
            Self::Full => Some(Self::TwoThirds),
            Self::TwoThirds => Some(Self::Half),
            Self::Half => None,
        }
    }

    /// The next larger scale, if any.
    #[must_use]
    pub const fn larger(self) -> Option<Self> {
        match self {
            Self::Full => None,
            Self::TwoThirds => Some(Self::Full),
            Self::Half => Some(Self::TwoThirds),
        }
    }

    /// The coded size of a `width × height` destination: scaled, even,
    /// at least 2 × 2 and within the protocol's surface limits (a larger
    /// destination is scaled further to fit).
    #[must_use]
    pub fn coded(self, width: u32, height: u32) -> (u32, u32) {
        let (numerator, denominator) = self.ratio();
        let scale = |side: u32| side.max(1).saturating_mul(numerator).div_ceil(denominator);
        let even_up = |side: u32| side.saturating_add(1) & !1;
        let (w, h) = (even_up(scale(width)), even_up(scale(height)));
        let side = f64::from(MAX_SURFACE_SIDE);
        let (fw, fh) = (f64::from(w), f64::from(h));
        let fit = (side / fw)
            .min(side / fh)
            .min((f64::from(MAX_SURFACE_PIXELS) / (fw * fh)).sqrt());
        if fit >= 1.0 {
            return (w, h);
        }
        // Larger than the protocol allows: scale both sides down to fit.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let even_down = |value: f64| (value.floor() as u32 & !1).max(2);
        (even_down(fw * fit), even_down(fh * fit))
    }
}

/// Rate-control terms for one surface encode (degradation step b).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SurfaceQuality {
    /// Target bitrate of this surface.
    pub bitrate_bps: u64,
    /// How far the encoder raises its lowest QP above its own default
    /// (0 at full quality).
    pub min_qp_raise: u8,
    /// The rate surfaces paint at (degradation step a), so the encoder
    /// spreads the bitrate over the pictures it will get; 0 when unknown.
    pub fps: u32,
}

/// Why a surface encode failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurfaceEncodeError {
    /// The encoder could not open at that size.
    Open,
    /// The encoder failed or produced no access unit.
    Encode,
}

impl Display for SurfaceEncodeError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Open => "surface encoder could not open",
            Self::Encode => "surface encode failed",
        })
    }
}

impl std::error::Error for SurfaceEncodeError {}

/// One encode the planner asks for.
#[derive(Debug, Clone, Copy)]
pub struct SurfaceEncodeRequest<'a> {
    /// Surface id.
    pub surface: u8,
    /// The monitor's current picture.
    pub source: BgraFrame<'a>,
    /// The source rectangle the surface shows (its destination).
    pub rect: TileRect,
    /// Coded width (even); the source rectangle is scaled to it.
    pub coded_width: u32,
    /// Coded height (even).
    pub coded_height: u32,
    /// The access unit must be an IDR picture.
    pub idr: bool,
    /// Rate control.
    pub quality: SurfaceQuality,
}

/// Turns surface pixels into Annex-B H.264 access units (Constrained
/// Baseline, 4:2:0, every Deck decodes it). One encoder serves every
/// surface of one monitor; each surface id is its own stream.
pub trait SurfaceEncoder {
    /// Opens (or replaces) surface `surface` at a coded size; the next
    /// encode of it is an IDR.
    ///
    /// # Errors
    ///
    /// [`SurfaceEncodeError::Open`].
    fn open(
        &mut self,
        surface: u8,
        coded_width: u32,
        coded_height: u32,
    ) -> Result<(), SurfaceEncodeError>;

    /// Closes a surface (closing one that is not open does nothing).
    fn close(&mut self, surface: u8);

    /// Encodes one picture of an open surface.
    ///
    /// # Errors
    ///
    /// [`SurfaceEncodeError::Encode`].
    fn encode(&mut self, request: &SurfaceEncodeRequest<'_>)
    -> Result<Vec<u8>, SurfaceEncodeError>;
}

/// What a surface serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurfaceRole {
    /// A Keel motion region (its stable id).
    Region(u32),
    /// The whole monitor (design rule 7: sustained full-frame motion).
    FullMonitor,
    /// One picture of many-colour content, opened and closed in one frame
    /// (design rule 5).
    OneShot,
}

/// An open surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SurfaceSlot {
    /// Surface id.
    pub id: u8,
    /// What it serves.
    pub role: SurfaceRole,
    /// Its canvas destination (= the source rectangle it encodes).
    pub dest: TileRect,
    /// Coded width.
    pub coded_width: u32,
    /// Coded height.
    pub coded_height: u32,
    /// Its scale.
    pub scale: SurfaceScale,
    /// Opened, and not yet painted with its IDR.
    pub awaiting_idr: bool,
}

impl SurfaceSlot {
    fn open(id: u8, role: SurfaceRole, dest: TileRect, scale: SurfaceScale) -> Self {
        let (coded_width, coded_height) = scale.coded(dest.width, dest.height);
        Self {
            id,
            role,
            dest,
            coded_width,
            coded_height,
            scale,
            awaiting_idr: true,
        }
    }

    /// Coded pixels a picture of it costs.
    #[must_use]
    pub fn coded_pixels(&self) -> u64 {
        u64::from(self.coded_width) * u64::from(self.coded_height)
    }
}

/// A planned surface change, in command order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurfaceChange {
    /// Close a surface.
    Close(u8),
    /// Open (or reopen) a surface.
    Open(SurfaceSlot),
    /// Move a surface's destination without reopening it.
    Move {
        /// Surface id.
        id: u8,
        /// New destination (same size).
        dest: TileRect,
    },
}

fn area(rect: TileRect) -> u64 {
    u64::from(rect.width) * u64::from(rect.height)
}

fn contains(outer: TileRect, inner: TileRect) -> bool {
    outer.x <= inner.x
        && outer.y <= inner.y
        && outer.x + outer.width >= inner.x + inner.width
        && outer.y + outer.height >= inner.y + inner.height
}

/// The least room a region surface gets around its motion when it opens,
/// on each side (half a motion-region quantum), so a region that grows by
/// one quantum still fits. A larger region gets an eighth of its size on
/// each side, so a region that keeps growing (a dragged window) reopens
/// a few times, not every tick.
pub const REGION_HEADROOM: u32 = 32;

/// A `width × height` rectangle on the grid, inside `bounds`, that holds
/// `rect`; `None` when there is none.
///
/// The canvas takes a surface destination only on the block grid: whole
/// blocks, unless a side ends at the canvas's edge. A side that is not
/// whole blocks opened against that edge and can sit only there; moving
/// it anywhere else would leave a destination the Deck refuses (and the
/// planner's own mirror refuses first, which restarts the canvas).
fn place(width: u32, height: u32, rect: TileRect, bounds: TileRect) -> Option<TileRect> {
    let axis = |start: u32, length: u32, low: u32, high: u32| -> Option<u32> {
        let latest = high.checked_sub(length)?;
        let origin = if length % BLOCK != 0 {
            latest
        } else if start + length <= high {
            // Keep the start where it is, else push it back inside the
            // bounds, on the block grid.
            start
        } else {
            latest & !(BLOCK - 1)
        };
        (origin >= low && origin <= start && origin + length <= high && origin % BLOCK == 0)
            .then_some(origin)
    };
    let x = axis(rect.x, width, bounds.x, bounds.x + bounds.width)?;
    let y = axis(rect.y, height, bounds.y, bounds.y + bounds.height)?;
    let placed = TileRect {
        x,
        y,
        width,
        height,
    };
    contains(placed, rect).then_some(placed)
}

/// A region's rectangle with [`REGION_HEADROOM`] on each side, on the
/// block grid and inside `bounds`. Other roles open as they are.
fn with_headroom(role: SurfaceRole, rect: TileRect, bounds: TileRect) -> TileRect {
    if !matches!(role, SurfaceRole::Region(_)) {
        return rect;
    }
    let right = bounds.x + bounds.width;
    let bottom = bounds.y + bounds.height;
    let room_x = (rect.width / 8).max(REGION_HEADROOM);
    let room_y = (rect.height / 8).max(REGION_HEADROOM);
    let x = rect.x.saturating_sub(room_x).max(bounds.x) & !(BLOCK - 1);
    let y = rect.y.saturating_sub(room_y).max(bounds.y) & !(BLOCK - 1);
    let x = x.max(bounds.x);
    let y = y.max(bounds.y);
    let end_x = (rect.x + rect.width + room_x).min(right);
    let end_y = (rect.y + rect.height + room_y).min(bottom);
    // The far edge on the grid, unless it is the canvas's edge.
    let end_x = if end_x == right {
        right
    } else {
        end_x & !(BLOCK - 1)
    };
    let end_y = if end_y == bottom {
        bottom
    } else {
        end_y & !(BLOCK - 1)
    };
    let grown = TileRect {
        x,
        y,
        width: end_x.saturating_sub(x),
        height: end_y.saturating_sub(y),
    };
    if contains(grown, rect) { grown } else { rect }
}

/// The surfaces of one monitor.
#[derive(Debug, Clone)]
pub struct SurfaceSet {
    slots: [Option<SurfaceSlot>; MAX_SURFACES_PER_MONITOR as usize],
    limit: u8,
}

impl SurfaceSet {
    /// A set of at most `limit` surfaces (the Deck's
    /// `max_surfaces_per_monitor`, at most the protocol's).
    #[must_use]
    pub fn new(limit: u8) -> Self {
        Self {
            slots: [None; MAX_SURFACES_PER_MONITOR as usize],
            limit: limit.min(MAX_SURFACES_PER_MONITOR),
        }
    }

    /// The open surfaces, by id.
    pub fn slots(&self) -> impl Iterator<Item = &SurfaceSlot> {
        self.slots.iter().flatten()
    }

    /// An open surface.
    #[must_use]
    pub fn get(&self, id: u8) -> Option<&SurfaceSlot> {
        self.slots.get(usize::from(id)).and_then(Option::as_ref)
    }

    /// Forgets every surface (a reset closed them).
    pub fn clear(&mut self) {
        self.slots = [None; MAX_SURFACES_PER_MONITOR as usize];
    }

    /// The changes that serve `targets` (in priority order; a target
    /// without a free id is not served) at `scale`, on a canvas of
    /// `bounds`.
    ///
    /// Reopening costs an IDR and a new encoder, so a surface is kept
    /// through small changes: it stays while its motion fits and covers
    /// more than [`SHRINK_PERCENT`] of it, moves (no IDR) to cover motion
    /// that fits its size, and a region surface that has to reopen gets
    /// [`REGION_HEADROOM`] around its motion, so the next small growth
    /// fits.
    #[must_use]
    pub fn plan(
        &self,
        targets: &[(SurfaceRole, TileRect)],
        scale: SurfaceScale,
        bounds: TileRect,
    ) -> Vec<SurfaceChange> {
        let mut changes = Vec::new();
        let mut used = [false; MAX_SURFACES_PER_MONITOR as usize];
        let mut unplaced = Vec::new();
        for &(role, rect) in targets {
            let existing = self
                .slots()
                .find(|slot| slot.role == role && !used[usize::from(slot.id)]);
            let Some(slot) = existing else {
                unplaced.push((role, rect));
                continue;
            };
            used[usize::from(slot.id)] = true;
            if slot.scale == scale
                && contains(slot.dest, rect)
                && area(rect) * 100 > area(slot.dest) * SHRINK_PERCENT
            {
                continue;
            }
            if slot.scale == scale
                && area(rect) * 100 > area(slot.dest) * SHRINK_PERCENT
                && let Some(dest) = place(slot.dest.width, slot.dest.height, rect, bounds)
            {
                if dest != slot.dest {
                    changes.push(SurfaceChange::Move { id: slot.id, dest });
                }
                continue;
            }
            changes.push(SurfaceChange::Open(SurfaceSlot::open(
                slot.id,
                role,
                with_headroom(role, rect, bounds),
                scale,
            )));
        }
        let mut closes = Vec::new();
        for slot in self.slots() {
            if !used[usize::from(slot.id)] {
                closes.push(SurfaceChange::Close(slot.id));
            }
        }
        // Closed ids are free for new surfaces in the same frame: a close
        // and an open of the same id is a replacement.
        let free: Vec<u8> = (0..self.limit)
            .filter(|id| !used[usize::from(*id)])
            .collect();
        for ((role, rect), id) in unplaced.into_iter().zip(free) {
            closes.retain(|change| *change != SurfaceChange::Close(id));
            changes.push(SurfaceChange::Open(SurfaceSlot::open(
                id,
                role,
                with_headroom(role, rect, bounds),
                scale,
            )));
        }
        closes.extend(changes);
        closes
    }

    /// A surface id `changes` leave free, for a one-shot surface.
    #[must_use]
    pub fn spare_id(&self, changes: &[SurfaceChange]) -> Option<u8> {
        (0..self.limit).find(|id| {
            let opened = changes
                .iter()
                .any(|change| matches!(change, SurfaceChange::Open(slot) if slot.id == *id));
            let closed = changes.contains(&SurfaceChange::Close(*id));
            !opened && (closed || self.get(*id).is_none())
        })
    }

    /// Applies planned changes.
    pub fn apply(&mut self, changes: &[SurfaceChange]) {
        for change in changes {
            match *change {
                SurfaceChange::Close(id) => {
                    if let Some(slot) = self.slots.get_mut(usize::from(id)) {
                        *slot = None;
                    }
                }
                SurfaceChange::Open(opened) => {
                    if let Some(slot) = self.slots.get_mut(usize::from(opened.id)) {
                        *slot = Some(opened);
                    }
                }
                SurfaceChange::Move { id, dest } => {
                    if let Some(Some(slot)) = self.slots.get_mut(usize::from(id)) {
                        slot.dest = dest;
                    }
                }
            }
        }
    }

    /// A surface was painted (its IDR, if it awaited one, is sent).
    pub fn painted(&mut self, id: u8) {
        if let Some(Some(slot)) = self.slots.get_mut(usize::from(id)) {
            slot.awaiting_idr = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: u32, y: u32, width: u32, height: u32) -> TileRect {
        TileRect {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn coded_sizes_are_even_scaled_and_within_the_protocol_limits() {
        assert_eq!(SurfaceScale::Full.coded(640, 360), (640, 360));
        assert_eq!(SurfaceScale::Full.coded(650, 371), (650, 372));
        assert_eq!(SurfaceScale::TwoThirds.coded(640, 360), (428, 240));
        assert_eq!(SurfaceScale::Half.coded(3840, 2160), (1920, 1080));
        let (w, h) = SurfaceScale::Full.coded(16_384, 16_384);
        assert!(w <= u32::from(MAX_SURFACE_SIDE) && h <= u32::from(MAX_SURFACE_SIDE));
        assert!(w * h <= MAX_SURFACE_PIXELS && w % 2 == 0 && h % 2 == 0);
        assert_eq!(SurfaceScale::Full.coded(1, 1), (2, 2));
    }

    const BOUNDS: TileRect = TileRect {
        x: 0,
        y: 0,
        width: 1920,
        height: 1080,
    };

    #[test]
    fn a_surface_opens_with_headroom_then_keeps_its_rect_through_small_changes() {
        let plan = |set: &SurfaceSet, rect| {
            set.plan(
                &[(SurfaceRole::Region(7), rect)],
                SurfaceScale::Full,
                BOUNDS,
            )
        };
        let mut set = SurfaceSet::new(4);
        // An eighth of its size on each side, on the block grid.
        let changes = plan(&set, rect(64, 64, 640, 384));
        assert!(matches!(
            changes.as_slice(),
            [SurfaceChange::Open(slot)]
                if slot.id == 0 && slot.awaiting_idr && slot.dest == rect(0, 16, 784, 480)
        ));
        set.apply(&changes);
        set.painted(0);
        // Small changes inside it, and shrinking to just over half its
        // area, keep it.
        assert!(plan(&set, rect(0, 64, 784, 432)).is_empty());
        assert!(plan(&set, rect(64, 64, 608, 320)).is_empty());
        // Motion that fits its size elsewhere moves it, no IDR.
        assert_eq!(
            plan(&set, rect(256, 256, 640, 384)),
            vec![SurfaceChange::Move {
                id: 0,
                dest: rect(256, 256, 784, 480)
            }]
        );
        // At the canvas's edge it is placed back inside the canvas.
        assert_eq!(
            plan(&set, rect(1280, 512, 640, 384)),
            vec![SurfaceChange::Move {
                id: 0,
                dest: rect(1136, 512, 784, 480)
            }]
        );
        // Shrinking below half reopens it (an IDR).
        let small = rect(64, 64, 256, 256);
        assert!(matches!(
            plan(&set, small).as_slice(),
            [SurfaceChange::Open(slot)] if slot.id == 0 && contains(slot.dest, small) && slot.awaiting_idr
        ));
        // Growing out of it reopens it.
        let wide = rect(0, 64, 1280, 720);
        assert!(matches!(
            plan(&set, wide).as_slice(),
            [SurfaceChange::Open(slot)] if contains(slot.dest, wide)
        ));
    }

    /// The canvas state's rule for a surface destination: whole blocks,
    /// unless a side ends at the canvas's edge.
    fn on_the_grid(dest: TileRect, bounds: TileRect) -> bool {
        dest.x % BLOCK == 0
            && dest.y % BLOCK == 0
            && (dest.width % BLOCK == 0 || dest.x + dest.width == bounds.x + bounds.width)
            && (dest.height % BLOCK == 0 || dest.y + dest.height == bounds.y + bounds.height)
    }

    #[test]
    fn a_surface_opened_against_a_partial_block_edge_never_moves_off_it() {
        // 1080 rows end half way through a block row, and a 1512-wide
        // canvas half way through a block column: a surface that opens
        // against that edge has a partial-block side, which is valid only
        // while it ends there.
        for bounds in [BOUNDS, rect(0, 0, 1512, 950)] {
            let plan = |set: &SurfaceSet, rect| {
                set.plan(
                    &[(SurfaceRole::Region(3), rect)],
                    SurfaceScale::Full,
                    bounds,
                )
            };
            let mut set = SurfaceSet::new(2);
            let near_corner = rect(
                (bounds.width - 690) & !(BLOCK - 1),
                (bounds.height - 180) & !(BLOCK - 1),
                640,
                160,
            );
            let changes = plan(&set, near_corner);
            let [SurfaceChange::Open(slot)] = changes.as_slice() else {
                panic!("{changes:?}");
            };
            assert!(on_the_grid(slot.dest, bounds), "{:?}", slot.dest);
            assert_eq!(
                slot.dest.y + slot.dest.height,
                bounds.height,
                "the test opens against the bottom edge"
            );
            set.apply(&changes);
            set.painted(0);
            // Scrolling text whose motion climbs the screen: every move or
            // reopen stays on the grid.
            for step in 1..=24 {
                let moved = rect(
                    near_corner.x - step * BLOCK,
                    near_corner.y - step * BLOCK,
                    640,
                    160,
                );
                let changes = plan(&set, moved);
                for change in &changes {
                    match change {
                        SurfaceChange::Move { dest, .. } => {
                            assert!(on_the_grid(*dest, bounds), "move to {dest:?} in {bounds:?}");
                            assert!(contains(*dest, moved));
                        }
                        SurfaceChange::Open(slot) => {
                            assert!(on_the_grid(slot.dest, bounds), "open {:?}", slot.dest);
                        }
                        SurfaceChange::Close(_) => {}
                    }
                }
                set.apply(&changes);
                set.painted(0);
            }
        }
    }

    #[test]
    fn a_new_scale_reopens_and_a_whole_monitor_surface_gets_no_headroom() {
        let mut set = SurfaceSet::new(2);
        let video = rect(64, 64, 640, 384);
        set.apply(&set.plan(
            &[(SurfaceRole::Region(1), video)],
            SurfaceScale::Full,
            BOUNDS,
        ));
        assert!(matches!(
            set.plan(&[(SurfaceRole::Region(1), video)], SurfaceScale::Half, BOUNDS).as_slice(),
            [SurfaceChange::Open(slot)] if slot.scale == SurfaceScale::Half
                && (slot.coded_width, slot.coded_height) == (392, 240)
        ));
        let whole = SurfaceSet::new(1).plan(
            &[(SurfaceRole::FullMonitor, BOUNDS)],
            SurfaceScale::Full,
            BOUNDS,
        );
        assert!(matches!(
            whole.as_slice(),
            [SurfaceChange::Open(slot)] if slot.dest == BOUNDS
        ));
    }

    #[test]
    fn surfaces_close_with_their_regions_and_respect_the_decks_limit() {
        let mut set = SurfaceSet::new(1);
        let a = rect(0, 0, 128, 128);
        let b = rect(256, 0, 128, 128);
        let changes = set.plan(
            &[(SurfaceRole::Region(1), a), (SurfaceRole::Region(2), b)],
            SurfaceScale::Full,
            BOUNDS,
        );
        assert_eq!(changes.len(), 1, "one surface allowed");
        set.apply(&changes);
        assert_eq!(set.spare_id(&[]), None);
        // Region 1 ends and region 2 takes its id in the same frame.
        let changes = set.plan(&[(SurfaceRole::Region(2), b)], SurfaceScale::Full, BOUNDS);
        assert!(matches!(
            changes.as_slice(),
            [SurfaceChange::Open(slot)] if slot.id == 0 && slot.role == SurfaceRole::Region(2)
        ));
        set.apply(&changes);
        assert_eq!(
            set.plan(&[], SurfaceScale::Full, BOUNDS),
            vec![SurfaceChange::Close(0)]
        );
        assert_eq!(set.spare_id(&[SurfaceChange::Close(0)]), Some(0));
    }
}
