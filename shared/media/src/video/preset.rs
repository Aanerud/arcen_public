use crate::EncodeIntent;

use super::pipeline::{PipelineContract, PipelineId, pipeline_contract};

/// User-facing streaming presets are complete trade-off contracts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamingPreset {
    Auto,
    Speed,
    Grading,
    Hdr,
}

impl StreamingPreset {
    #[must_use]
    pub const fn pipeline(self) -> PipelineId {
        match self {
            Self::Auto => PipelineId::Auto,
            Self::Speed => PipelineId::Speed,
            Self::Grading => PipelineId::Grading,
            Self::Hdr => PipelineId::Hdr,
        }
    }

    #[must_use]
    pub const fn pipeline_contract(self) -> PipelineContract {
        pipeline_contract(self.pipeline())
    }
}

/// What the session preserves first when the link cannot carry every frame
/// at the requested quality.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MotionPriority {
    /// Keep per-frame detail and let frame rate fall.
    #[default]
    Detail,
    /// Keep motion cadence and let picture detail soften.
    Motion,
}

impl MotionPriority {
    /// Every priority in the stable vocabulary.
    pub const ALL: &'static [Self] = &[Self::Detail, Self::Motion];

    /// Stable wire/argv token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Detail => "detail",
            Self::Motion => "motion",
        }
    }

    /// Parse a stable token.
    #[must_use]
    pub fn from_token(value: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|priority| priority.token() == value)
    }
}

/// The explicit trade-off behind one streaming preset.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PresetContract {
    /// Frame-rate ceiling requested from the host. Not a delivered-fps promise.
    pub max_fps: u32,
    /// Which side of the quality/motion trade-off is preserved first.
    pub priority: MotionPriority,
    /// Encoder effort requested from the host.
    pub intent: EncodeIntent,
    /// VBV buffer size in frames for encoders whose rate-control policy uses it.
    pub encoder_buffer_frames: f64,
    /// What selecting this preset favours.
    pub favours: &'static str,
    /// What selecting this preset trades away.
    pub gives_up: &'static str,
    /// User-facing summary of the trade.
    pub summary: &'static str,
}

/// Shared VBV policy for a resolved preset priority and encoder intent.
#[must_use]
pub const fn encoder_buffer_frames(priority: MotionPriority, intent: EncodeIntent) -> f64 {
    match priority {
        MotionPriority::Motion => 1.0,
        MotionPriority::Detail => match intent {
            EncodeIntent::Interactive => 2.0,
            EncodeIntent::Quality => 8.0,
        },
    }
}

/// Returns the shared contract for a user-facing streaming preset.
#[must_use]
pub const fn contract(preset: StreamingPreset) -> PresetContract {
    let pipeline = preset.pipeline_contract();
    PresetContract {
        max_fps: pipeline.max_fps,
        priority: pipeline.priority,
        intent: pipeline.intent,
        encoder_buffer_frames: pipeline.encoder_buffer_frames,
        favours: pipeline.favours,
        gives_up: pipeline.gives_up,
        summary: pipeline.summary,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_preset_states_a_tradeoff() {
        for preset in [
            StreamingPreset::Auto,
            StreamingPreset::Speed,
            StreamingPreset::Grading,
            StreamingPreset::Hdr,
        ] {
            let contract = contract(preset);
            assert!(!contract.favours.is_empty());
            assert!(!contract.gives_up.is_empty());
            assert!(!contract.summary.is_empty());
        }
    }

    #[test]
    fn preset_contracts_pin_priority_intent_buffer_and_ceiling() {
        let auto = contract(StreamingPreset::Auto);
        assert_eq!(auto.max_fps, 30);
        assert_eq!(auto.priority, MotionPriority::Detail);
        assert_eq!(auto.intent, EncodeIntent::Interactive);

        let speed = contract(StreamingPreset::Speed);
        assert_eq!(speed.max_fps, 60);
        assert_eq!(speed.priority, MotionPriority::Motion);
        assert!((speed.encoder_buffer_frames - 1.0).abs() < f64::EPSILON);

        for preset in [StreamingPreset::Grading, StreamingPreset::Hdr] {
            let contract = contract(preset);
            assert_eq!(contract.max_fps, 30);
            assert_eq!(contract.intent, EncodeIntent::Quality);
            assert!((contract.encoder_buffer_frames - 8.0).abs() < f64::EPSILON);
        }
    }
}
