// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! How often a Deck's UI asks for its next pass.
//!
//! egui shortens every delayed repaint by its predicted frame time
//! (`InputState::predicted_dt`, which eframe leaves at its 1/60 s default):
//! `request_repaint_after(16 ms)` becomes an immediate repaint. With display
//! sync on, the next drawable caps that at the refresh rate; with display
//! sync off (a multi-window session), nothing does, and the UI spins at
//! hundreds of passes a second on the main thread.

use std::time::Duration;

/// The delay to hand `egui::Context::request_repaint_after` so the next pass
/// runs `interval` from now: egui subtracts `predicted_dt` (seconds) again.
#[must_use]
pub fn egui_repaint_delay(interval: Duration, predicted_dt: f32) -> Duration {
    let predicted = Duration::try_from_secs_f32(predicted_dt).unwrap_or(Duration::ZERO);
    interval.saturating_add(predicted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_poll_interval_survives_eguis_predicted_frame_time() {
        // egui's default prediction: 1/60 s. Without compensation a 16 ms
        // poll reads as 16 - 16.67 ms = an immediate repaint.
        let predicted = 1.0 / 60.0;
        let delay = egui_repaint_delay(Duration::from_millis(16), predicted);
        let after_egui = delay.saturating_sub(Duration::from_secs_f32(predicted));
        assert_eq!(after_egui, Duration::from_millis(16));
    }

    #[test]
    fn a_nonsensical_prediction_adds_nothing() {
        assert_eq!(
            egui_repaint_delay(Duration::from_millis(250), f32::NAN),
            Duration::from_millis(250)
        );
        assert_eq!(
            egui_repaint_delay(Duration::from_millis(250), -1.0),
            Duration::from_millis(250)
        );
    }
}
