// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
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
    /// Linux headless Wayland desktop policy.
    #[serde(default)]
    pub wayland: WaylandConfig,
    /// Physical USB devices bridged from a Deck (Native tablet).
    #[serde(default)]
    pub usb: UsbConfig,
    /// What the desk shows and accepts while a Deck is attached.
    #[serde(default)]
    pub privacy: PrivacyConfig,
    pub platform: P,
}

/// Privacy policy (`privacy`).
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PrivacyConfig {
    /// `on`, or absent: while a Deck is attached to a desktop the desk's
    /// monitors show, those monitors are blanked and the desk's keyboard
    /// and mouse blocked, on every Pier; a desk that cannot be made private
    /// refuses the attachment. `off`: the desk stays live (logged at start
    /// and shown on the Deck). See
    /// [`crate::console_session::desk_privacy::DeskPrivacyPolicy`].
    pub desk: Option<crate::console_session::desk_privacy::DeskPrivacySetting>,
}

impl PrivacyConfig {
    /// The effective desk privacy policy.
    #[must_use]
    pub const fn desk_policy(self) -> crate::console_session::desk_privacy::DeskPrivacyPolicy {
        crate::console_session::desk_privacy::DeskPrivacyPolicy::resolve(self.desk)
    }
}

/// USB bridging policy (`usb`).
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UsbConfig {
    /// `enabled`: the Pier offers Native tablet — a Deck's physical pen
    /// tablet attached to the desktop as a virtual USB device — where the
    /// platform supports it (ADR 0020, ADR 0021). `disabled`, or absent:
    /// never offered. An administrator opt-in because it attaches a
    /// Deck-supplied USB device to the host kernel.
    pub native_tablet: Option<NativeTabletSetting>,
}

/// The administrator's Native tablet choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum NativeTabletSetting {
    Enabled,
    Disabled,
}

/// Where the effective Native tablet decision came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeTabletSource {
    /// `usb.native_tablet` in the Pier configuration.
    Config,
    /// Not configured: off.
    Default,
}

/// The effective Native tablet decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeTabletPolicy {
    pub enabled: bool,
    pub source: NativeTabletSource,
}

impl NativeTabletPolicy {
    /// Resolves `usb.native_tablet`: the configuration is the only switch
    /// (the 0.18 `ARCEN_USB_HARD_LAB=1` environment is gone since 0.19.0).
    #[must_use]
    pub const fn resolve(setting: Option<NativeTabletSetting>) -> Self {
        match setting {
            Some(setting) => Self {
                enabled: matches!(setting, NativeTabletSetting::Enabled),
                source: NativeTabletSource::Config,
            },
            None => Self {
                enabled: false,
                source: NativeTabletSource::Default,
            },
        }
    }

    /// Why Native tablet is off, in words shown to a Deck user and to the
    /// administrator; `None` when it is on.
    #[must_use]
    pub fn disabled_reason(self) -> Option<&'static str> {
        match (self.enabled, self.source) {
            (true, _) => None,
            (false, NativeTabletSource::Config) => Some(
                "Native tablet is disabled on this host (usb.native_tablet is \"disabled\" in \
                 the Pier configuration)",
            ),
            (false, _) => Some(
                "Native tablet is not enabled on this host; an administrator enables it with \
                 usb.native_tablet = \"enabled\" in the Pier configuration",
            ),
        }
    }
}

/// Linux headless Wayland desktop policy (`wayland`).
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WaylandConfig {
    /// `run` (default): GNOME's first-login assistant and welcome tour run
    /// as on a GDM login. `skip`: Arcen sets GNOME's own completion markers
    /// for the user before the desktop starts, so neither opens. See
    /// [`crate::desktop_session::first_login`].
    pub first_login_setup: crate::desktop_session::first_login::FirstLoginSetup,
    /// Lab only (default `false`): give each headless Wayland session a
    /// virtual tablet on a private udev seat, for a compositor built with
    /// the headless-input lab patch (see `arcen_input::seat_pen`). Not for
    /// production: it needs a patched Mutter and grants the session user
    /// its own virtual pen's event node.
    pub lab_seat_pen: bool,
    /// `auto` (default): a desktop runs its compositor natively on its own
    /// logind seat, with a real pen, when the host's qualification proved
    /// it and the seat can be built; otherwise headless, with the reason
    /// logged. `native_seat`: always native, or the desktop is refused.
    /// `headless`: never native. See [`crate::native_seat`].
    pub session_mode: crate::native_seat::SessionMode,
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
    /// Linux desktop provider: `auto` (default, chosen from host facts),
    /// or the operator override `xorg` or `wayland`. Unknown tokens fail
    /// configuration loading; every Pier must also call
    /// [`CaptureConfig::desktop_provider_for`] so a value it cannot honour is
    /// refused rather than ignored. See [`crate::desktop_provider`].
    pub provider: Option<crate::desktop_provider::ProviderPreference>,
    /// Linux headless Wayland: the qualification record this Pier reads and
    /// `arcen-pier wayland qualify` writes. Unset (the default) is the
    /// production record; a development Pier names its own so it never
    /// reads or overwrites the installed service's. See
    /// [`crate::wayland_qualification::QualificationTarget`].
    pub wayland_qualification_record: Option<String>,
    /// Linux headless Wayland: whether the Pier re-qualifies the host
    /// itself at start when an update made its record stale (default
    /// `true`); `false` only refuses, naming the command. See
    /// [`crate::wayland_qualification::auto_requalify_decision`].
    pub wayland_auto_requalify: Option<bool>,
    /// Linux headless Wayland: the prefix GNOME Shell or Mutter is installed
    /// in (`<prefix>/bin/gnome-shell`). Unset (the default) is the
    /// distribution's `/usr`; a side-by-side install such as
    /// `/opt/arcen-gnome/51` is used only by Arcen's sessions. See
    /// [`crate::compositor_prefix::CompositorPrefix`].
    pub wayland_compositor_prefix: Option<String>,
}

impl CaptureConfig {
    /// The configured provider preference, validated for `platform`.
    ///
    /// # Errors
    ///
    /// See [`crate::desktop_provider::validate_desktop_provider_for`].
    pub const fn desktop_provider_for(
        &self,
        platform: crate::desktop_provider::PierPlatform,
    ) -> Result<
        crate::desktop_provider::ProviderPreference,
        crate::desktop_provider::DesktopProviderConfigError,
    > {
        crate::desktop_provider::validate_desktop_provider_for(platform, self.provider)
    }
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
    /// The depth a Linux Xorg desktop is created at: `auto` (default; 30
    /// only for a desktop created by a ten-bit session or declared
    /// `rec2100-pq`, 24 otherwise), `24`, or `30`. Fixed for the desktop's
    /// whole life. Other hosts ignore it.
    pub x11_desktop_depth: Option<String>,
    /// Exact still image (lossless refinement, ADR 0016): `enabled` or
    /// absent lets the Pier refine a still picture to the source's exact
    /// pixels for a Deck that asks for it, on a contract that allows it;
    /// `disabled`: never. See [`LosslessRefinementSetting`].
    pub lossless_refinement: Option<LosslessRefinementSetting>,
    pub variant: Option<String>,
    pub fps: Option<u32>,
    pub encoder: Option<String>,
    /// How the software pipeline spends the host (`video.software`).
    pub software: Option<SoftwareVideoConfig>,
}

impl VideoConfig {
    /// `video.software.cpu_share`, when the administrator set it.
    #[must_use]
    pub fn software_cpu_share_percent(&self) -> Option<u8> {
        self.software
            .and_then(|software| software.cpu_share)
            .map(CpuSharePercent::percent)
    }
}

/// `video.software`: how the software (canvas) pipeline spends the host.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SoftwareVideoConfig {
    /// The share of the host's logical cores the software pipeline may
    /// spend, a whole percentage from 1 to 100. Absent: 50 (the shared
    /// default, `arcen_media::canvas::DEFAULT_CPU_SHARE_PERCENT`).
    pub cpu_share: Option<CpuSharePercent>,
}

/// A whole percentage from 1 to 100, refused at load otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CpuSharePercent(u8);

impl CpuSharePercent {
    /// The percentage.
    #[must_use]
    pub const fn percent(self) -> u8 {
        self.0
    }
}

impl<'de> Deserialize<'de> for CpuSharePercent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = u64::deserialize(deserializer)?;
        u8::try_from(value)
            .ok()
            .filter(|percent| (1..=100).contains(percent))
            .map(Self)
            .ok_or_else(|| {
                serde::de::Error::custom(format!(
                    "Pier config video.software.cpu_share {value}: expected a whole percentage from 1 to 100"
                ))
            })
    }
}

/// The administrator's exact-still-image choice (`video.lossless_refinement`).
///
/// On unless an administrator turns it off. It engages only when a Deck
/// asks for it and the shared plan (`arcen_media::refine::resolve_refine_plan`)
/// allows the session's contract (Grading or HDR, 4:4:4 ten-bit, a CPU
/// refine source); it spends spare bandwidth only while the picture is
/// still. Every other session reports why it is off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LosslessRefinementSetting {
    #[default]
    Enabled,
    Disabled,
}

impl LosslessRefinementSetting {
    /// Resolves the optional key: absent means [`Self::Enabled`].
    #[must_use]
    pub fn resolve(setting: Option<Self>) -> bool {
        matches!(setting.unwrap_or_default(), Self::Enabled)
    }
}

/// Whether a remote session leaves the host's own speakers audible.
///
/// This is separate from whether audio is *transmitted*. Someone sitting at
/// the host machine hearing the remote user's audio is a privacy failure
/// regardless of what the network is carrying, so the default silences local
/// playback for the whole session even when redirection is switched off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
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

    /// The configuration token, also used on helper command lines.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Muted => "muted",
            Self::Audible => "audible",
        }
    }

    /// Parses [`Self::token`].
    #[must_use]
    pub fn from_token(token: &str) -> Option<Self> {
        match token {
            "muted" => Some(Self::Muted),
            "audible" => Some(Self::Audible),
            _ => None,
        }
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
    fn desk_privacy_is_on_by_default_and_only_on_or_off_parse() {
        let base = r#""audio":{"enabled":false,"compressed":false},
            "microphone_input":{"enabled":false},"platform":{"name":"test"}"#;
        let default: PierConfig<TestPlatform> =
            serde_json::from_str(&format!("{{{base}}}")).expect("config without privacy");
        assert!(default.privacy.desk_policy().enabled);
        let off: PierConfig<TestPlatform> =
            serde_json::from_str(&format!(r#"{{"privacy":{{"desk":"off"}},{base}}}"#))
                .expect("privacy off");
        assert!(!off.privacy.desk_policy().enabled);
        for bad in [
            r#"{"desk":"false"}"#,
            r#"{"desk":false}"#,
            r#"{"desk":"dim"}"#,
            r#"{"monitors":"off"}"#,
        ] {
            assert!(
                serde_json::from_str::<PierConfig<TestPlatform>>(&format!(
                    r#"{{"privacy":{bad},{base}}}"#
                ))
                .is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn first_login_setup_defaults_to_run_and_rejects_unknown_tokens() {
        use crate::desktop_session::first_login::FirstLoginSetup;
        let base = r#""audio":{"enabled":false,"compressed":false},
            "microphone_input":{"enabled":false},"platform":{"name":"test"}"#;
        let default: PierConfig<TestPlatform> =
            serde_json::from_str(&format!("{{{base}}}")).expect("config without wayland");
        assert_eq!(default.wayland.first_login_setup, FirstLoginSetup::Run);
        let skip: PierConfig<TestPlatform> = serde_json::from_str(&format!(
            r#"{{"wayland":{{"first_login_setup":"skip"}},{base}}}"#
        ))
        .expect("skip");
        assert_eq!(skip.wayland.first_login_setup, FirstLoginSetup::Skip);
        for bad in [
            r#"{"first_login_setup":"never"}"#,
            r#"{"first_login":"skip"}"#,
        ] {
            assert!(
                serde_json::from_str::<PierConfig<TestPlatform>>(&format!(
                    r#"{{"wayland":{bad},{base}}}"#
                ))
                .is_err()
            );
        }
    }

    #[test]
    fn capture_provider_is_validated_at_load_and_per_platform() {
        use crate::desktop_provider::{PierPlatform, ProviderPreference};
        let base = r#""audio":{"enabled":false,"compressed":false},
            "microphone_input":{"enabled":false},"platform":{"name":"test"}"#;
        let linux = PierPlatform::Linux {
            wayland_adapter_built: false,
        };
        let default: PierConfig<TestPlatform> =
            serde_json::from_str(&format!("{{{base}}}")).expect("config without provider");
        assert_eq!(
            default.capture.desktop_provider_for(linux),
            Ok(ProviderPreference::Auto)
        );
        assert!(
            default
                .capture
                .desktop_provider_for(PierPlatform::Windows)
                .is_ok()
        );
        let wayland: PierConfig<TestPlatform> =
            serde_json::from_str(&format!(r#"{{"capture":{{"provider":"wayland"}},{base}}}"#))
                .expect("config with provider");
        assert_eq!(wayland.capture.provider, Some(ProviderPreference::Wayland));
        assert!(wayland.capture.desktop_provider_for(linux).is_err());
        assert!(
            wayland
                .capture
                .desktop_provider_for(PierPlatform::Macos)
                .is_err()
        );
        for token in ["auto", "xorg"] {
            let configured: PierConfig<TestPlatform> =
                serde_json::from_str(&format!(r#"{{"capture":{{"provider":"{token}"}},{base}}}"#))
                    .expect("config with provider");
            assert_eq!(
                configured
                    .capture
                    .desktop_provider_for(linux)
                    .map(ProviderPreference::token),
                Ok(token)
            );
        }
        for typo in ["waylnd", "Wayland", "Auto", "x11", ""] {
            assert!(
                serde_json::from_str::<PierConfig<TestPlatform>>(&format!(
                    r#"{{"capture":{{"provider":"{typo}"}},{base}}}"#
                ))
                .is_err(),
                "{typo:?} must fail configuration loading"
            );
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
    fn native_tablet_is_an_administrator_opt_in_that_outranks_the_old_variable() {
        let parse = |usb: &str| {
            serde_json::from_str::<PierConfig<TestPlatform>>(&format!(
                r#"{{{usb}"audio":{{"enabled":false,"compressed":false}},
                    "microphone_input":{{"enabled":false}},
                    "platform":{{"name":"test"}}}}"#
            ))
        };
        let absent = parse("").expect("a configuration without usb parses");
        assert_eq!(absent.usb.native_tablet, None);
        let enabled = parse(r#""usb":{"native_tablet":"enabled"},"#).expect("enabled");
        assert_eq!(
            enabled.usb.native_tablet,
            Some(NativeTabletSetting::Enabled)
        );
        for typo in [
            r#""usb":{"native_tablet":"on"},"#,
            r#""usb":{"native_tablet":true},"#,
            r#""usb":{"native_tablets":"enabled"},"#,
        ] {
            assert!(
                parse(typo).is_err(),
                "{typo} must fail configuration loading"
            );
        }

        let resolve = NativeTabletPolicy::resolve;
        assert_eq!(
            resolve(None),
            NativeTabletPolicy {
                enabled: false,
                source: NativeTabletSource::Default
            }
        );
        assert!(
            resolve(None)
                .disabled_reason()
                .is_some_and(|reason| reason.contains("usb.native_tablet"))
        );
        let disabled = resolve(Some(NativeTabletSetting::Disabled));
        assert!(!disabled.enabled);
        assert_eq!(disabled.source, NativeTabletSource::Config);
        assert!(
            disabled
                .disabled_reason()
                .is_some_and(|reason| reason.contains("\"disabled\""))
        );
        let on = resolve(Some(NativeTabletSetting::Enabled));
        assert!(on.enabled);
        assert_eq!(on.disabled_reason(), None);
    }

    #[test]
    fn exact_still_image_is_on_unless_the_administrator_turns_it_off() {
        let parse = |video: &str| {
            serde_json::from_str::<PierConfig<TestPlatform>>(&format!(
                r#"{{{video}"audio":{{"enabled":false,"compressed":false}},
                    "microphone_input":{{"enabled":false}},
                    "platform":{{"name":"test"}}}}"#
            ))
        };
        let absent = parse("").expect("absent");
        assert!(LosslessRefinementSetting::resolve(
            absent.video.lossless_refinement
        ));
        let no_key = parse(r#""video":{},"#).expect("no key");
        assert!(LosslessRefinementSetting::resolve(
            no_key.video.lossless_refinement
        ));
        let on = parse(r#""video":{"lossless_refinement":"enabled"},"#).expect("enabled");
        assert!(LosslessRefinementSetting::resolve(
            on.video.lossless_refinement
        ));
        let off = parse(r#""video":{"lossless_refinement":"disabled"},"#).expect("disabled");
        assert!(!LosslessRefinementSetting::resolve(
            off.video.lossless_refinement
        ));
        for typo in [
            r#""video":{"lossless_refinement":"on"},"#,
            r#""video":{"lossless_refinement":true},"#,
        ] {
            assert!(
                parse(typo).is_err(),
                "{typo} must fail configuration loading"
            );
        }
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
    fn software_cpu_share_is_a_percentage_and_absent_by_default() {
        let parse = |video: &str| {
            serde_json::from_str::<PierConfig<TestPlatform>>(&format!(
                r#"{{"video":{video},"audio":{{"enabled":true,"compressed":false}},
                    "microphone_input":{{"enabled":false}},"platform":{{"name":"test"}}}}"#
            ))
        };
        let unset = parse("{}").expect("valid config");
        assert_eq!(unset.video.software_cpu_share_percent(), None);
        let set = parse(r#"{"software":{"cpu_share":25}}"#).expect("valid config");
        assert_eq!(set.video.software_cpu_share_percent(), Some(25));
        for refused in [
            r#"{"software":{"cpu_share":0}}"#,
            r#"{"software":{"cpu_share":101}}"#,
            r#"{"software":{"cpu_share":0.5}}"#,
            r#"{"software":{"cpu_shares":50}}"#,
        ] {
            assert!(parse(refused).is_err(), "{refused}");
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
