//! Shared, platform-parameterized Pier configuration schema.

use serde::Deserialize;

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
    pub qp_map: Option<String>,
    pub variant: Option<String>,
    pub fps: Option<u32>,
    pub encoder: Option<String>,
}

/// Required host authority for host-to-Deck audio.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AudioConfig {
    pub enabled: bool,
    /// `true` forces the documented Opus policy; `false` forces PCM.
    pub compressed: bool,
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
