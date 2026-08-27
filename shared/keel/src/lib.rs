//! Arcen Keel: deterministic, platform-free content damage intelligence.
//!
//! Keel consumes borrowed pixels and produces reusable damage metadata. Capture,
//! conversion, encoding, transport, and wire contracts remain in their owning
//! adapters and crates.

#![forbid(unsafe_code)]

mod activity;
mod cadence;
mod damage;
mod external;
mod grid;
mod hash;

pub mod scenario;

pub use activity::{
    ACTIVITY_ROLLING_WINDOW, ActivityClass, ActivityDiagnostics, ActivityGrid, ActivityHint,
    CadenceRecommendation, DIRTY_RATIO_BASIS_POINTS, DirtyRatio,
};
pub use cadence::{EmitMode, IdleCadence};
pub use damage::{DamageMap, DamageSummary, DamageTracker, DirtyBlockRows, DirtyBlocks};
pub use external::{ExternalDamage, PixelRect};
pub use grid::{BLOCK_SIZE, BgraFrame, BlockBounds, BlockGrid, KeelError};
pub use hash::{HashKernel, KernelPreference};
