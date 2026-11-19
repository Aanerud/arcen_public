// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Measured cost of each kind of work, in milliseconds per megapixel.
//!
//! The governor spends a per-tick CPU budget. It needs to know what a block
//! of each kind costs on this host, now, not on the machine a constant was
//! tuned on. `WorkMeter` keeps an exponentially weighted average per kind.
//! The policy that spends the budget lives with the planner; this is only
//! the measurement.

use core::time::Duration;

/// Pixels folded into one average sample: smaller jobs accumulate first, so
/// a one-block tile does not swing the average with timer noise.
pub const MIN_SAMPLE_PIXELS: u64 = 64 * 1024;

/// Kinds of per-pixel work a streaming tick performs. Codec-free: the
/// planner decides which of its stages map to which kind.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum WorkKind {
    /// Damage hashing.
    Hash = 0,
    /// Block traits and classification.
    Classify = 1,
    /// Colour conversion.
    Convert = 2,
    /// Exact tile encoding.
    Tile = 3,
    /// Lossy surface encoding.
    Surface = 4,
    /// Applying copies and fills to a mirror.
    Compose = 5,
}

impl WorkKind {
    pub const ALL: [Self; 6] = [
        Self::Hash,
        Self::Classify,
        Self::Convert,
        Self::Tile,
        Self::Surface,
        Self::Compose,
    ];
}

#[derive(Clone, Copy, Debug, Default)]
struct Average {
    ms_per_mpx: Option<f64>,
    pending_pixels: u64,
    pending_ms: f64,
    samples: u64,
}

/// EWMA cost per [`WorkKind`].
#[derive(Clone, Copy, Debug)]
pub struct WorkMeter {
    weight: f64,
    averages: [Average; WorkKind::ALL.len()],
}

impl Default for WorkMeter {
    fn default() -> Self {
        Self::new(0.125)
    }
}

impl WorkMeter {
    /// `weight` is the share each new sample takes, clamped to `(0, 1]`.
    #[must_use]
    pub fn new(weight: f64) -> Self {
        let weight = if weight.is_finite() {
            weight.clamp(f64::MIN_POSITIVE, 1.0)
        } else {
            0.125
        };
        Self {
            weight,
            averages: [Average::default(); WorkKind::ALL.len()],
        }
    }

    /// Forgets every measurement.
    pub fn reset(&mut self) {
        self.averages = [Average::default(); WorkKind::ALL.len()];
    }

    /// Records `pixels` of `kind` that took `elapsed`. Zero-pixel records are
    /// ignored.
    pub fn record(&mut self, kind: WorkKind, pixels: u64, elapsed: Duration) {
        if pixels == 0 {
            return;
        }
        let average = &mut self.averages[kind as usize];
        average.pending_pixels = average.pending_pixels.saturating_add(pixels);
        average.pending_ms += elapsed.as_secs_f64() * 1_000.0;
        if average.pending_pixels < MIN_SAMPLE_PIXELS {
            return;
        }
        #[allow(clippy::cast_precision_loss)]
        let sample = average.pending_ms / (average.pending_pixels as f64 / 1_000_000.0);
        average.ms_per_mpx = Some(match average.ms_per_mpx {
            Some(previous) => previous + self.weight * (sample - previous),
            None => sample,
        });
        average.samples += 1;
        average.pending_pixels = 0;
        average.pending_ms = 0.0;
    }

    /// Average milliseconds per megapixel, once one sample is complete.
    #[must_use]
    pub fn ms_per_mpx(&self, kind: WorkKind) -> Option<f64> {
        self.averages[kind as usize].ms_per_mpx
    }

    /// Completed samples for `kind`.
    #[must_use]
    pub fn samples(&self, kind: WorkKind) -> u64 {
        self.averages[kind as usize].samples
    }

    /// Predicted time for `pixels` of `kind`.
    #[must_use]
    pub fn predict(&self, kind: WorkKind, pixels: u64) -> Option<Duration> {
        #[allow(clippy::cast_precision_loss)]
        let ms = self.ms_per_mpx(kind)? * (pixels as f64 / 1_000_000.0);
        Some(Duration::from_secs_f64((ms / 1_000.0).max(0.0)))
    }

    /// How many pixels of `kind` fit in `budget`, or `None` before the
    /// first sample.
    #[must_use]
    pub fn affordable_pixels(&self, kind: WorkKind, budget: Duration) -> Option<u64> {
        let rate = self.ms_per_mpx(kind)?;
        if rate <= 0.0 {
            return Some(u64::MAX);
        }
        let pixels = budget.as_secs_f64() * 1_000.0 / rate * 1_000_000.0;
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss
        )]
        Some(if pixels >= u64::MAX as f64 {
            u64::MAX
        } else {
            pixels as u64
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_jobs_accumulate_into_one_sample() {
        let mut meter = WorkMeter::default();
        meter.record(WorkKind::Tile, 256, Duration::from_micros(10));
        assert_eq!(meter.ms_per_mpx(WorkKind::Tile), None);
        for _ in 0..255 {
            meter.record(WorkKind::Tile, 256, Duration::from_micros(10));
        }
        let rate = meter.ms_per_mpx(WorkKind::Tile).unwrap_or_default();
        assert!((rate - 39.0625).abs() < 1e-6, "{rate}");
        assert_eq!(meter.samples(WorkKind::Tile), 1);
        assert_eq!(meter.ms_per_mpx(WorkKind::Hash), None);
    }

    #[test]
    fn average_moves_by_the_weight_and_predicts() {
        let mut meter = WorkMeter::new(0.5);
        meter.record(WorkKind::Surface, 1_000_000, Duration::from_millis(10));
        meter.record(WorkKind::Surface, 1_000_000, Duration::from_millis(20));
        let rate = meter.ms_per_mpx(WorkKind::Surface).unwrap_or_default();
        assert!((rate - 15.0).abs() < 1e-9, "{rate}");
        let predicted = meter
            .predict(WorkKind::Surface, 2_000_000)
            .unwrap_or_default();
        assert!(predicted.abs_diff(Duration::from_millis(30)) < Duration::from_micros(1));
        assert_eq!(
            meter.affordable_pixels(WorkKind::Surface, Duration::from_millis(30)),
            Some(2_000_000)
        );
        meter.reset();
        assert_eq!(meter.predict(WorkKind::Surface, 1), None);
    }

    #[test]
    fn weight_is_clamped() {
        let mut meter = WorkMeter::new(f64::NAN);
        meter.record(WorkKind::Hash, 1_000_000, Duration::from_millis(1));
        assert_eq!(meter.ms_per_mpx(WorkKind::Hash), Some(1.0));
        let mut meter = WorkMeter::new(7.0);
        meter.record(WorkKind::Hash, 1_000_000, Duration::from_millis(1));
        meter.record(WorkKind::Hash, 1_000_000, Duration::from_millis(3));
        assert_eq!(meter.ms_per_mpx(WorkKind::Hash), Some(3.0));
    }
}
