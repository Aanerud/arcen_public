//! Shared, platform-parameterized Pier configuration schema.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use arcen_telemetry::{OperationalProfile, QosTargets};

/// Common Pier configuration plus one required platform-specific section.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PierConfig<P> {
    #[serde(default)]
    pub listen: ListenConfig,
    #[serde(default)]
    pub tls: TlsConfig,
    #[serde(default)]
    pub capture: CaptureConfig,
    #[serde(default)]
    pub video: VideoConfig,
    pub audio: AudioConfig,
    pub microphone_input: MicrophoneInputConfig,
    #[serde(default)]
    pub clipboard: ClipboardConfig,
    #[serde(default)]
    pub auth: AuthConfig,
    #[serde(default)]
    pub redirection: RedirectionConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
    pub platform: P,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ListenConfig {
    pub host: Option<String>,
    /// Canonical direct-session QUIC UDP port.
    pub port: Option<u16>,
    /// Deprecated pre-QUIC-default alias retained for in-place config
    /// migration. When present, product Piers prefer this value over `port`.
    pub quic_port: Option<u16>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TlsConfig {
    pub mode: Option<String>,
    #[serde(alias = "certificate")]
    pub cert: Option<String>,
    #[serde(alias = "private_key")]
    pub key: Option<String>,
    pub minimum_version: Option<String>,
    pub disabled_cipher_suites: Vec<String>,
    pub expiry_warning_days: Option<u64>,
    pub expected_sans: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CaptureConfig {
    pub binary: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct VideoConfig {
    pub codec: Option<String>,
    pub chroma: Option<String>,
    pub bit_depth: Option<String>,
    pub color_range: Option<String>,
    pub color_matrix: Option<String>,
    pub color_policy: Option<String>,
    /// Damage-driven QP biasing: `off` (default), `on`, or `neutral`.
    ///
    /// Operator-owned rather than client-negotiated: it redistributes bits
    /// within a frame without changing the format a client decodes, so there
    /// is nothing to negotiate. See `docs/architecture/qp-maps.md`.
    pub qp_map: Option<QpMapConfig>,
    /// How the captured desktop's pixels are encoded where the platform
    /// cannot report it (Linux Xorg): `sdr` (default) or `rec2100-pq`, the
    /// operator's promise that a colour-managed application writes Rec.2100
    /// PQ code values into the desktop.
    pub desktop_encoding: Option<String>,
    pub variant: Option<String>,
    pub fps: Option<u32>,
    pub encoder: Option<String>,
}

/// Whether a remote session leaves the host's own speakers audible.
///
/// This is separate from whether audio is *transmitted*. Someone sitting at
/// the host machine hearing the remote user's audio is a privacy failure
/// regardless of what the network is carrying, so the default silences local
/// playback for the whole session even when redirection is switched off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalPlayback {
    /// Local speakers stay silent for the duration of the session.
    #[default]
    Muted,
    /// Local speakers keep working. The operator asked for this explicitly.
    Audible,
}

/// Operator QP-map policy, either one policy for every served pipeline or a
/// per-pipeline object whose missing entries fall back to the contract default.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum QpMapConfig {
    Single(String),
    PerPipeline(BTreeMap<String, String>),
}

impl QpMapConfig {
    const POLICIES: &'static [&'static str] = &["off", "on", "neutral"];
    const PIPELINES: &'static [&'static str] =
        &["auto", "speed", "grading", "hdr", "software", "custom"];

    /// Validates all configured keys/tokens and returns a selected token for
    /// the served pipeline. Missing object keys resolve to the pipeline
    /// contract default, which is currently `off` for every pipeline.
    ///
    /// # Errors
    ///
    /// Returns a config-shaped error naming the bad key/token and allowed
    /// values.
    pub fn effective_token(&self, served_pipeline: &str) -> Result<&str, String> {
        match self {
            Self::Single(token) => {
                let token = Self::normalize_policy(token, "video.qp_map")?;
                Ok(token)
            }
            Self::PerPipeline(map) => {
                for (key, value) in map {
                    Self::normalize_pipeline_key(key)?;
                    Self::normalize_policy(value, &format!("video.qp_map.{key}"))?;
                }
                let served = served_pipeline.to_ascii_lowercase();
                let Some(token) = map.get(served.as_str()) else {
                    return Ok("off");
                };
                let token = Self::normalize_policy(token, &format!("video.qp_map.{served}"))?;
                Ok(token)
            }
        }
    }

    fn normalize_policy<'a>(value: &'a str, key: &str) -> Result<&'a str, String> {
        let token = value.trim();
        if Self::POLICIES.contains(&token) {
            Ok(token)
        } else {
            Err(format!(
                "Pier config {key} {value:?}: expected one of {}",
                Self::POLICIES.join(", ")
            ))
        }
    }

    fn normalize_pipeline_key(key: &str) -> Result<(), String> {
        if Self::PIPELINES.contains(&key) {
            Ok(())
        } else {
            Err(format!(
                "Pier config video.qp_map key {key:?}: expected one of {}",
                Self::PIPELINES.join(", ")
            ))
        }
    }
}

impl LocalPlayback {
    /// Returns whether the host must hold a mute lease for the session.
    #[must_use]
    pub const fn requires_mute(self) -> bool {
        matches!(self, Self::Muted)
    }
}

/// Required host authority for host-to-Deck audio.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AudioConfig {
    pub enabled: bool,
    /// `true` forces the documented Opus policy; `false` forces PCM.
    pub compressed: bool,
    /// Whether the host's own speakers stay audible during a session.
    ///
    /// Absent in existing configurations, which is why it carries a default
    /// rather than becoming a required field: an operator upgrading a host
    /// must not have it refuse to start. The default is the safe direction.
    #[serde(default)]
    pub local_playback: LocalPlayback,
}

/// Required host authority for Deck-to-host microphone publication.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MicrophoneInputConfig {
    pub enabled: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ClipboardConfig {
    pub direction: Option<String>,
    pub content: Option<String>,
    pub max_bytes: Option<usize>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AuthConfig {
    pub disclaimer: DisclaimerConfig,
    pub reconnect_window_secs: Option<u32>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DisclaimerConfig {
    pub enabled: bool,
    pub locale: Option<String>,
    pub directory: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RedirectionConfig {
    pub timezone: Option<bool>,
}

#[derive(Debug, Clone, Default)]
pub struct LoggingConfig {
    /// Canonical cumulative operational profile.
    pub level: Option<OperationalProfile>,
    /// One-release compatibility input using the old numeric semantics.
    pub verbosity: Option<u8>,
    pub retention_days: Option<u16>,
    pub qos_targets: QosTargets,
}

impl LoggingConfig {
    /// Resolves canonical, migrated legacy, or production-default policy.
    ///
    /// # Errors
    ///
    /// Returns an error for manually constructed conflicting or invalid legacy
    /// fields. Deserialization rejects these states before construction.
    pub const fn resolved_profile(&self) -> Result<ResolvedLoggingProfile, LoggingProfileError> {
        if self.level.is_some() && self.verbosity.is_some() {
            return Err(LoggingProfileError::ConflictingFields);
        }
        if let Some(level) = self.level {
            Ok(ResolvedLoggingProfile {
                profile: level,
                source: LoggingProfileSource::Level,
            })
        } else if let Some(verbosity) = self.verbosity {
            let profile = match verbosity {
                0 => OperationalProfile::Error,
                1 => OperationalProfile::Info,
                2 | 3 => OperationalProfile::Debug,
                _ => return Err(LoggingProfileError::InvalidLegacyVerbosity(verbosity)),
            };
            Ok(ResolvedLoggingProfile {
                profile,
                source: LoggingProfileSource::LegacyVerbosity,
            })
        } else {
            Ok(ResolvedLoggingProfile {
                profile: OperationalProfile::Critical,
                source: LoggingProfileSource::ProductionDefault,
            })
        }
    }
}

impl<'de> Deserialize<'de> for LoggingConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = LoggingConfigRaw::deserialize(deserializer)?;
        if raw.level.is_some() && raw.verbosity.is_some() {
            return Err(serde::de::Error::custom(
                "logging.level and legacy logging.verbosity are mutually exclusive",
            ));
        }
        let level = raw
            .level
            .map(OperationalProfile::try_from)
            .transpose()
            .map_err(serde::de::Error::custom)?;
        if raw.verbosity.is_some_and(|value| value > 3) {
            return Err(serde::de::Error::custom(
                "legacy logging.verbosity is outside 0..=3",
            ));
        }
        Ok(Self {
            level,
            verbosity: raw.verbosity,
            retention_days: raw.retention_days,
            qos_targets: raw.qos_targets,
        })
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct LoggingConfigRaw {
    level: Option<u8>,
    verbosity: Option<u8>,
    retention_days: Option<u16>,
    qos_targets: QosTargets,
}

/// Origin of the effective logging profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoggingProfileSource {
    /// Canonical `logging.level`.
    Level,
    /// Migrated one-release `logging.verbosity`.
    LegacyVerbosity,
    /// Built-in production Level 0.
    ProductionDefault,
}

#[cfg(test)]
mod qp_map_tests {
    use super::*;

    #[test]
    fn qp_map_string_form_applies_to_every_pipeline() {
        let config: QpMapConfig = serde_json::from_str(r#""on""#).unwrap();
        assert_eq!(config.effective_token("auto"), Ok("on"));
        assert_eq!(config.effective_token("hdr"), Ok("on"));
    }

    #[test]
    fn qp_map_object_form_resolves_served_pipeline_and_defaults_missing_to_off() {
        let config: QpMapConfig =
            serde_json::from_str(r#"{"auto":"on","speed":"neutral","hdr":"off"}"#).unwrap();
        assert_eq!(config.effective_token("auto"), Ok("on"));
        assert_eq!(config.effective_token("speed"), Ok("neutral"));
        assert_eq!(config.effective_token("hdr"), Ok("off"));
        assert_eq!(config.effective_token("grading"), Ok("off"));
    }

    #[test]
    fn qp_map_bad_token_and_unknown_key_name_allowed_values() {
        let bad_token: QpMapConfig = serde_json::from_str(r#"{"auto":"maybe"}"#).unwrap();
        assert!(
            bad_token
                .effective_token("auto")
                .unwrap_err()
                .contains("video.qp_map.auto")
        );
        let bad_key: QpMapConfig = serde_json::from_str(r#"{"cinema":"on"}"#).unwrap();
        assert!(
            bad_key
                .effective_token("cinema")
                .unwrap_err()
                .contains("expected one of auto, speed, grading, hdr, software, custom")
        );
    }
}

/// Effective profile and its configuration source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedLoggingProfile {
    /// Effective cumulative profile.
    pub profile: OperationalProfile,
    /// Configuration source.
    pub source: LoggingProfileSource,
}

/// Invalid manually constructed logging profile fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoggingProfileError {
    /// Canonical and legacy fields were both supplied.
    ConflictingFields,
    /// Legacy numeric value was outside `0..=3`.
    InvalidLegacyVerbosity(u8),
}

impl std::fmt::Display for LoggingProfileError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ConflictingFields => {
                formatter.write_str("logging.level and logging.verbosity conflict")
            }
            Self::InvalidLegacyVerbosity(value) => {
                write!(
                    formatter,
                    "legacy logging.verbosity {value} is outside 0..=3"
                )
            }
        }
    }
}

impl std::error::Error for LoggingProfileError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct TestPlatform {
        name: String,
    }

    #[test]
    fn audio_and_microphone_choices_are_required() {
        for document in [
            r#"{"microphone_input":{"enabled":false},"platform":{"name":"test"}}"#,
            r#"{"audio":{"enabled":true,"compressed":false},"platform":{"name":"test"}}"#,
        ] {
            assert!(serde_json::from_str::<PierConfig<TestPlatform>>(document).is_err());
        }
    }

    #[test]
    fn local_playback_defaults_to_muted_for_configurations_that_predate_it() {
        // An operator upgrading a host must not have it refuse to start, and
        // the value it silently acquires must be the safe one: someone at the
        // host machine hearing the remote user is a privacy failure whether or
        // not the operator has heard of this setting.
        let config: PierConfig<TestPlatform> = serde_json::from_str(
            r#"{"audio":{"enabled":true,"compressed":false},
                "microphone_input":{"enabled":false},
                "platform":{"name":"test"}}"#,
        )
        .expect("an existing configuration still parses");
        assert_eq!(config.audio.local_playback, LocalPlayback::Muted);
        assert!(config.audio.local_playback.requires_mute());
    }

    #[test]
    fn local_playback_is_host_authoritative_and_explicit() {
        let audible: PierConfig<TestPlatform> = serde_json::from_str(
            r#"{"audio":{"enabled":false,"compressed":false,"local_playback":"audible"},
                "microphone_input":{"enabled":false},
                "platform":{"name":"test"}}"#,
        )
        .expect("parse");
        assert_eq!(audible.audio.local_playback, LocalPlayback::Audible);
        assert!(!audible.audio.local_playback.requires_mute());

        // Mute is required even when nothing is transmitted, which is the
        // whole point of keeping it separate from `enabled`.
        let muted_but_disabled: PierConfig<TestPlatform> = serde_json::from_str(
            r#"{"audio":{"enabled":false,"compressed":false,"local_playback":"muted"},
                "microphone_input":{"enabled":false},
                "platform":{"name":"test"}}"#,
        )
        .expect("parse");
        assert!(!muted_but_disabled.audio.enabled);
        assert!(muted_but_disabled.audio.local_playback.requires_mute());
    }

    #[test]
    fn an_unrecognised_local_playback_value_is_refused() {
        // Silently treating a typo as "audible" would leave a host audible
        // when its operator asked for silence.
        assert!(
            serde_json::from_str::<PierConfig<TestPlatform>>(
                r#"{"audio":{"enabled":true,"compressed":false,"local_playback":"quiet"},
                "microphone_input":{"enabled":false},
                "platform":{"name":"test"}}"#,
            )
            .is_err()
        );
    }

    #[test]
    fn common_schema_is_strict_and_platform_parameterized() {
        let config: PierConfig<TestPlatform> = serde_json::from_str(
            r#"{
                "listen":{"host":"0.0.0.0","port":18444},
                "video":{
                    "codec":"h265",
                    "chroma":"yuv444",
                    "bit_depth":"10",
                    "color_range":"full",
                    "color_matrix":"bt709",
                    "color_policy":"always-on",
                    "variant":"hevc-444-10-full-bt709",
                    "fps":60,
                    "encoder":"nvenc"
                },
                "audio":{"enabled":true,"compressed":false},
                "microphone_input":{"enabled":false},
                "platform":{"name":"test"}
            }"#,
        )
        .expect("valid config");
        assert_eq!(config.listen.port, Some(18_444));
        assert_eq!(config.listen.quic_port, None);
        assert_eq!(config.video.bit_depth.as_deref(), Some("10"));
        assert_eq!(config.video.color_range.as_deref(), Some("full"));
        assert_eq!(config.video.color_matrix.as_deref(), Some("bt709"));
        assert_eq!(config.video.color_policy.as_deref(), Some("always-on"));
        assert_eq!(
            config.video.variant.as_deref(),
            Some("hevc-444-10-full-bt709")
        );
        assert!(config.audio.enabled);
        assert!(!config.audio.compressed);
        assert_eq!(config.platform.name, "test");

        assert!(
            serde_json::from_str::<PierConfig<TestPlatform>>(
                r#"{
                    "audio":{"enabled":true,"compressed":false},
                    "microphone_input":{"enabled":false},
                    "surprise":true,
                    "platform":{"name":"test"}
                }"#
            )
            .is_err()
        );
    }

    #[test]
    fn canonical_level_defaults_to_production_critical() {
        let config: LoggingConfig = serde_json::from_str("{}").expect("default logging config");
        assert_eq!(
            config.resolved_profile(),
            Ok(ResolvedLoggingProfile {
                profile: OperationalProfile::Critical,
                source: LoggingProfileSource::ProductionDefault,
            })
        );
        let config: LoggingConfig = serde_json::from_str(r#"{"level":2,"retention_days":30}"#)
            .expect("canonical logging config");
        assert_eq!(
            config.resolved_profile(),
            Ok(ResolvedLoggingProfile {
                profile: OperationalProfile::Info,
                source: LoggingProfileSource::Level,
            })
        );
        assert_eq!(config.retention_days, Some(30));
    }

    #[test]
    fn legacy_verbosity_migrates_without_reinterpreting_numbers() {
        let expected = [
            OperationalProfile::Error,
            OperationalProfile::Info,
            OperationalProfile::Debug,
            OperationalProfile::Debug,
        ];
        for (legacy, profile) in expected.into_iter().enumerate() {
            let config: LoggingConfig =
                serde_json::from_str(&format!(r#"{{"verbosity":{legacy}}}"#))
                    .expect("legacy logging config");
            assert_eq!(config.verbosity, Some(legacy as u8));
            assert_eq!(
                config.resolved_profile(),
                Ok(ResolvedLoggingProfile {
                    profile,
                    source: LoggingProfileSource::LegacyVerbosity,
                })
            );
        }
    }

    #[test]
    fn logging_rejects_both_forms_and_invalid_qos_targets() {
        assert!(serde_json::from_str::<LoggingConfig>(r#"{"level":0,"verbosity":0}"#).is_err());
        assert!(serde_json::from_str::<LoggingConfig>(r#"{"level":4}"#).is_err());
        assert!(serde_json::from_str::<LoggingConfig>(r#"{"verbosity":4}"#).is_err());
        assert!(
            serde_json::from_str::<LoggingConfig>(
                r#"{"qos_targets":{"rtt_degraded_ms":200,"rtt_critical_ms":100}}"#
            )
            .is_err()
        );
    }
}
