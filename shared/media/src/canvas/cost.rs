// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The planner's cost model: Keel's [`WorkMeter`] measurements, with a
//! prior for each kind of work until the meter has its first sample.
//!
//! Every estimate the planner and the governor make goes through
//! [`CostModel::predict`], so a simulation that seeds the meter with fixed
//! rates is the same arithmetic the Pier runs on measured ones.

use arcen_keel::{WorkKind, WorkMeter};
use std::time::Duration;

/// Milliseconds per megapixel assumed for each [`WorkKind`] before the
/// meter has measured it, in [`WorkKind::ALL`] order: hash, classify,
/// convert, tile, surface, compose.
///
/// Conservative x86 VM figures (about 2.5 times an Apple M4 Pro's
/// single-thread measurements: a 4K hash 1.35 ms, a 640 × 368 surface
/// 2.0 ms, forty dirty blocks of tiles 0.03 ms). The meter replaces each
/// one after its first 64 Ki pixels of real work.
///
/// A surface's cost is its [`WorkKind::Surface`] rate alone: the encoder
/// seam converts and encodes in one call, so the measured time includes
/// the conversion, and so does this prior.
pub const DEFAULT_PRIORS_MS_PER_MPX: [f64; WorkKind::ALL.len()] = [0.4, 0.3, 1.5, 4.0, 13.5, 0.2];

/// Predicted cost of work on this host.
#[derive(Debug, Clone, Copy)]
pub struct CostModel {
    meter: WorkMeter,
    priors: [f64; WorkKind::ALL.len()],
}

impl Default for CostModel {
    fn default() -> Self {
        Self::new(WorkMeter::default())
    }
}

impl CostModel {
    /// A model over `meter` with the default priors.
    #[must_use]
    pub const fn new(meter: WorkMeter) -> Self {
        Self {
            meter,
            priors: DEFAULT_PRIORS_MS_PER_MPX,
        }
    }

    /// Replaces the prior of `kind` (milliseconds per megapixel; a
    /// negative or non-finite value is ignored).
    #[must_use]
    pub fn with_prior(mut self, kind: WorkKind, ms_per_mpx: f64) -> Self {
        if ms_per_mpx.is_finite() && ms_per_mpx >= 0.0 {
            self.priors[kind as usize] = ms_per_mpx;
        }
        self
    }

    /// The measurements.
    #[must_use]
    pub const fn meter(&self) -> &WorkMeter {
        &self.meter
    }

    /// Records measured work (see [`WorkMeter::record`]).
    pub fn record(&mut self, kind: WorkKind, pixels: u64, elapsed: Duration) {
        self.meter.record(kind, pixels, elapsed);
    }

    /// Milliseconds per megapixel of `kind`: measured, else the prior.
    #[must_use]
    pub fn ms_per_mpx(&self, kind: WorkKind) -> f64 {
        self.meter
            .ms_per_mpx(kind)
            .unwrap_or(self.priors[kind as usize])
    }

    /// Predicted time for `pixels` of `kind`.
    #[must_use]
    pub fn predict(&self, kind: WorkKind, pixels: u64) -> Duration {
        #[allow(clippy::cast_precision_loss)]
        let ms = self.ms_per_mpx(kind) * (pixels as f64 / 1_000_000.0);
        Duration::from_secs_f64((ms / 1_000.0).max(0.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn priors_hold_until_the_meter_measures() {
        let mut model = CostModel::default().with_prior(WorkKind::Tile, 10.0);
        assert_eq!(
            model.predict(WorkKind::Tile, 500_000),
            Duration::from_millis(5)
        );
        model.record(WorkKind::Tile, 1_000_000, Duration::from_millis(2));
        assert_eq!(
            model.predict(WorkKind::Tile, 500_000),
            Duration::from_millis(1)
        );
        assert!((model.ms_per_mpx(WorkKind::Hash) - 0.4).abs() < f64::EPSILON);
    }
}
