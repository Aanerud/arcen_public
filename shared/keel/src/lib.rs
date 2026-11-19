// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Arcen Keel: deterministic, platform-free content damage intelligence.
//!
//! Keel consumes borrowed pixels and produces reusable damage metadata. Capture,
//! conversion, encoding, transport, and wire contracts remain in their owning
//! adapters and crates.

#![forbid(unsafe_code)]

mod activity;
mod cadence;
mod classes;
mod damage;
mod external;
mod grid;
mod hash;
mod history;
mod ledger;
mod motion;
mod pacing;
mod regions;
mod traits;
mod work;

pub mod scenario;

pub use activity::{
    ACTIVITY_ROLLING_WINDOW, ActivityClass, ActivityDiagnostics, ActivityGrid, ActivityHint,
    CadenceRecommendation, DIRTY_RATIO_BASIS_POINTS, DirtyRatio,
};
pub use cadence::{EmitMode, IdleCadence, SubmissionGate, SubmissionMode};
pub use classes::{BlockClass, BlockClassMap, ClassConfig, ClassSummary};
pub use damage::{
    DEFAULT_AUDIT_PERIOD, DamageMap, DamageSummary, DamageTracker, DirtyBlockRows, DirtyBlocks,
};
pub use external::{
    DamageMetadataStatus, ExternalDamage, MAX_MOVE_RECTS, MoveRect, PendingExternalDamage,
    PixelRect, coalesced_rect_metadata_status, report_only_dirty_regions_status,
};
pub use grid::{BLOCK_SIZE, BgraFrame, BlockBounds, BlockGrid, KeelError};
pub use hash::{HashKernel, KernelPreference};
pub use history::{BlockHistory, BlockPaint, BlockRecord, HISTORY_WINDOW, PaintKind};
pub use ledger::{Inserted, LedgerError, SlotId, TileKey, TileLedger};
pub use motion::{
    DEFAULT_MAX_SCROLL, MAX_ESTIMATED_MOVES, MotionConfig, MotionEstimator, verify_move,
};
pub use pacing::ArrivalPacer;
pub use regions::{MAX_MOTION_REGIONS, MotionRegion, MotionRegions, REGION_QUANTUM, RegionConfig};
pub use traits::{BlockTraits, BlockTraitsMap, MAX_COUNTED_COLOURS, SHARP_EDGE_LUMA_STEP};
pub use work::{MIN_SAMPLE_PIXELS, WorkKind, WorkMeter};
