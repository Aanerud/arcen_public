//! Persisted Windows Pier configuration.

use std::path::{Path, PathBuf};

pub use arcen_session::pier_config::DisclaimerConfig;
use arcen_session::pier_config::PierConfig;
use serde::{Deserialize, Serialize};

pub type PierFileConfig = PierConfig<WindowsPlatformConfig>;

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WindowsPlatformConfig {
    pub desktop: DesktopConfig,
    pub iddcx: WindowsIddCxConfig,
    pub logging: WindowsLoggingConfig,
    pub first_login_timeout_secs: Option<u64>,
    pub multi_monitor: WindowsMultiMonitorConfig,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WindowsLoggingConfig {
    pub rotate_mb: Option<u64>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DesktopConfig {
    /// Exact, case-insensitive DXGI adapter description.
    pub adapter: Option<String>,
    /// Adapter-local output ordinal (`EnumOutputs` index).
    pub output: Option<u32>,
    /// Legacy global desktop-attached output index.
    pub output_index: Option<u32>,
    /// Operator-enforced physical-workstation privacy.
    pub deskside: crate::deskside::DesksideConfig,
}

/// Original Arcen IddCx provider opt-in. The provider remains unavailable
/// unless this gate, the multi-monitor advertisement gate, the driver
/// capability gate, and exact render-adapter affinity all succeed.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct WindowsIddCxConfig {
    pub enabled: bool,
    pub render_adapter: WindowsIddCxRenderAdapterConfig,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct WindowsIddCxRenderAdapterConfig {
    /// Exact stable adapter id reported by `arcen-pier diagnose-host --json`.
    pub stable_id: Option<String>,
    /// Exact, case-insensitive DXGI description. Ambiguous descriptions fail.
    pub description: Option<String>,
}

impl WindowsIddCxConfig {
    pub fn validate(&self, multi_monitor: &WindowsMultiMonitorConfig) -> Result<(), String> {
        if !self.enabled {
            return Ok(());
        }
        if !multi_monitor.advertise_enabled {
            return Err(
                "platform.iddcx.enabled requires platform.multi_monitor.advertise_enabled"
                    .to_string(),
            );
        }
        let stable = bounded_selector(
            self.render_adapter.stable_id.as_deref(),
            "platform.iddcx.render_adapter.stable_id",
        )?;
        let description = bounded_selector(
            self.render_adapter.description.as_deref(),
            "platform.iddcx.render_adapter.description",
        )?;
        if stable.is_none() && description.is_none() {
            return Err(
                "platform.iddcx.enabled requires an exact render-adapter stable_id or description"
                    .to_string(),
            );
        }
        Ok(())
    }
}

fn bounded_selector<'a>(value: Option<&'a str>, name: &str) -> Result<Option<&'a str>, String> {
    let value = value.map(str::trim).filter(|value| !value.is_empty());
    if value.is_some_and(|value| value.len() > 512 || value.chars().any(char::is_control)) {
        return Err(format!("{name} must be a bounded printable selector"));
    }
    Ok(value)
}

/// Explicit operator opt-in for advertising `multi_monitor_v1` pre-auth.
/// Defaults to fully disabled. See `multi_monitor_gate::MultiMonitorGate`.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WindowsMultiMonitorConfig {
    /// Explicit operator opt-in. `false` unless set. Even when `true`, this
    /// host also requires a non-empty probed physical output inventory and
    /// the hardcoded carrier-ready gate before it ever advertises support
    /// (see `multi_monitor_gate`).
    pub advertise_enabled: bool,
    /// Exact case-insensitive DXGI adapter descriptions multi-monitor may
    /// consume. Empty inherits `platform.desktop.adapter`, so a host pinned
    /// to one GPU never silently borrows another GPU reserved for compute.
    pub allowed_adapters: Vec<String>,
    /// Optional operator ceiling on the advertised `max_monitors`, clamped to
    /// [`arcen_media::MAX_MULTI_MONITOR_COUNT`]. Native NVIDIA headless mode
    /// additionally clamps this to its hardware-validated safe provider limit.
    /// For ordinary attached-output mode, set this when session-zero cannot
    /// truthfully discover the interactive output count.
    pub max_monitors: Option<u8>,
    /// Optional operator ceiling for simultaneous hardware encode sessions.
    /// `None` means runtime measured admission opens the planned NVENC set and
    /// discovers the usable capacity; this is not a GPU-model lookup table.
    /// Requests above an explicit ceiling require an enabled software fallback
    /// policy or fail atomically.
    pub nvenc_session_limit: Option<u8>,
    /// Whether admission may replace non-full-color monitors with a supported
    /// software 4:2:0 plan when hardware sessions are exhausted.
    pub allow_software_fallback: bool,
    /// Provision missing monitors through spare NVIDIA display IDs on the one
    /// allowed streaming adapter. Default-off and mutually exclusive with the
    /// externally supplied IddCx backend.
    pub nvidia_headless_enabled: bool,
}

pub struct LoadedConfig {
    pub path: PathBuf,
    pub value: PierFileConfig,
}

pub fn load(explicit_path: Option<PathBuf>) -> Result<Option<LoadedConfig>, String> {
    let (path, required) = match explicit_path {
        Some(path) => (path, true),
        None => match default_path() {
            Some(path) => (path, false),
            None => return Ok(None),
        },
    };
    if !path.exists() {
        if required {
            return Err(format!("Pier config does not exist: {}", path.display()));
        }
        return Ok(None);
    }
    let bytes = std::fs::read(&path)
        .map_err(|error| format!("read Pier config {}: {error}", path.display()))?;
    let mut value: PierFileConfig = serde_json::from_slice(&bytes)
        .map_err(|error| format!("parse Pier config {}: {error}", path.display()))?;
    let base = path.parent().unwrap_or_else(|| Path::new("."));
    resolve_optional_path(&mut value.tls.cert, base);
    resolve_optional_path(&mut value.tls.key, base);
    resolve_optional_path(&mut value.capture.binary, base);
    resolve_optional_path(&mut value.auth.disclaimer.directory, base);
    Ok(Some(LoadedConfig { path, value }))
}

fn resolve_optional_path(value: &mut Option<String>, base: &Path) {
    let Some(raw) = value else {
        return;
    };
    let path = PathBuf::from(&*raw);
    if path.is_relative() {
        *raw = base.join(path).to_string_lossy().into_owned();
    }
}

#[cfg(windows)]
fn default_path() -> Option<PathBuf> {
    std::env::var_os("ProgramData")?;
    Some(crate::paths::config_path())
}

#[cfg(not(windows))]
fn default_path() -> Option<PathBuf> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unknown_settings() {
        let result = serde_json::from_str::<PierFileConfig>(
            r#"{
                "audio":{"enabled":true,"compressed":false},
                "microphone_input":{"enabled":false},
                "platform":{},
                "surprise":true
            }"#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn parses_nested_settings() {
        let parsed: PierFileConfig = serde_json::from_str(
            r#"{
                "listen":{"port":18444},
                "video":{"codec":"h265","chroma":"yuv444","fps":60},
                "audio":{"enabled":false,"compressed":false},
                "microphone_input":{"enabled":false},
                "clipboard":{"direction":"both","content":"all","max_bytes":8388608},
                "auth":{
                    "disclaimer":{"enabled":true,"locale":"en_US","directory":"disclaimers"},
                    "reconnect_window_secs":1200
                },
                "redirection":{"timezone":true},
                "logging":{"verbosity":2,"retention_days":14},
                "platform":{
                    "desktop":{"adapter":"NVIDIA GRID V100D-16Q","output":0,
                        "deskside":{"enabled":false,"monitors":[]}},
                    "logging":{"rotate_mb":64},
                    "first_login_timeout_secs":600
                }
            }"#,
        )
        .expect("config");
        assert_eq!(parsed.listen.port, Some(18444));
        assert_eq!(
            parsed.platform.desktop.adapter.as_deref(),
            Some("NVIDIA GRID V100D-16Q")
        );
        assert_eq!(parsed.video.fps, Some(60));
        assert_eq!(parsed.redirection.timezone, Some(true));
        assert!(!parsed.platform.desktop.deskside.enabled);
        assert!(!parsed.audio.enabled);
        assert!(!parsed.audio.compressed);
        assert_eq!(parsed.clipboard.direction.as_deref(), Some("both"));
        assert_eq!(parsed.clipboard.content.as_deref(), Some("all"));
        assert_eq!(parsed.clipboard.max_bytes, Some(8_388_608));
        assert!(parsed.auth.disclaimer.enabled);
        assert_eq!(parsed.auth.reconnect_window_secs, Some(1_200));
        assert_eq!(
            parsed.auth.disclaimer.directory.as_deref(),
            Some("disclaimers")
        );
        assert_eq!(parsed.logging.verbosity, Some(2));
        assert_eq!(parsed.platform.logging.rotate_mb, Some(64));
        assert_eq!(parsed.logging.retention_days, Some(14));
        assert_eq!(parsed.platform.first_login_timeout_secs, Some(600));
    }

    #[test]
    fn canonical_level_and_legacy_verbosity_use_shared_profile_resolution() {
        let canonical: PierFileConfig = serde_json::from_str(
            r#"{"audio":{"enabled":true,"compressed":false},"microphone_input":{"enabled":false},"logging":{"level":0},"platform":{}}"#,
        )
        .expect("level");
        let resolved = canonical
            .logging
            .resolved_profile()
            .expect("resolved level");
        assert_eq!(
            resolved.profile,
            arcen_telemetry::OperationalProfile::Critical
        );
        assert_eq!(
            resolved.source,
            arcen_session::pier_config::LoggingProfileSource::Level
        );

        let legacy: PierFileConfig = serde_json::from_str(
            r#"{"audio":{"enabled":true,"compressed":false},"microphone_input":{"enabled":false},"logging":{"verbosity":1},"platform":{}}"#,
        )
        .expect("legacy verbosity");
        assert_eq!(
            legacy
                .logging
                .resolved_profile()
                .expect("legacy resolved")
                .profile,
            arcen_telemetry::OperationalProfile::Info
        );

        assert!(
            serde_json::from_str::<PierFileConfig>(
                r#"{"audio":{"enabled":true,"compressed":false},"microphone_input":{"enabled":false},"logging":{"level":2,"verbosity":1},"platform":{}}"#,
            )
            .is_err()
        );
    }

    #[test]
    fn iddcx_is_default_off_and_requires_both_gates_and_affinity() {
        let defaulted =
            serde_json::from_str::<WindowsPlatformConfig>(r#"{}"#).expect("default platform");
        assert!(!defaulted.iddcx.enabled);

        let enabled = WindowsIddCxConfig {
            enabled: true,
            render_adapter: WindowsIddCxRenderAdapterConfig {
                stable_id: Some("pci-10de-1eb8".to_string()),
                description: None,
            },
        };
        assert!(enabled
            .validate(&WindowsMultiMonitorConfig::default())
            .is_err());
        assert!(enabled
            .validate(&WindowsMultiMonitorConfig {
                advertise_enabled: true,
                ..WindowsMultiMonitorConfig::default()
            })
            .is_ok());
    }

    #[test]
    fn parses_explicit_iddcx_render_affinity() {
        let parsed: PierFileConfig = serde_json::from_str(
            r#"{
                "audio":{"enabled":true,"compressed":false},
                "microphone_input":{"enabled":false},
                "platform":{
                    "iddcx":{
                        "enabled":true,
                        "render_adapter":{"stable_id":"pci-10de-1eb8"}
                    },
                    "multi_monitor":{"advertise_enabled":true}
                }
            }"#,
        )
        .expect("IddCx config");
        parsed
            .platform
            .iddcx
            .validate(&parsed.platform.multi_monitor)
            .expect("valid gates");
        assert_eq!(
            parsed.platform.iddcx.render_adapter.stable_id.as_deref(),
            Some("pci-10de-1eb8")
        );
    }

    #[test]
    fn packaged_config_is_level_zero_with_valid_qos_targets() {
        let packaged: PierFileConfig =
            serde_json::from_str(include_str!("../../../packaging/windows/pier.json"))
                .expect("packaged Windows config");
        assert_eq!(
            packaged
                .logging
                .resolved_profile()
                .expect("packaged profile")
                .profile,
            arcen_telemetry::OperationalProfile::Critical
        );
        assert_eq!(
            packaged.logging.qos_targets,
            arcen_telemetry::QosTargets::default()
        );
    }

    #[test]
    fn packaged_config_leaves_codec_selection_automatic_with_full_colour_ceiling() {
        let packaged: PierFileConfig =
            serde_json::from_str(include_str!("../../../packaging/windows/pier.json"))
                .expect("packaged Windows config");
        assert_eq!(packaged.video.codec, None);
        assert_eq!(packaged.video.chroma, None);
        assert_eq!(packaged.video.bit_depth.as_deref(), Some("10"));
        assert_eq!(packaged.video.color_range.as_deref(), Some("full"));
        assert_eq!(packaged.video.fps, Some(60));
        assert_eq!(packaged.video.encoder.as_deref(), Some("auto"));
    }

    #[test]
    fn tls_old_and_new_settings_are_additive() {
        let old: PierFileConfig = serde_json::from_str(
            r#"{
                "tls":{"cert":"host.crt","key":"host.key"},
                "audio":{"enabled":true,"compressed":false},
                "microphone_input":{"enabled":false},
                "platform":{}
            }"#,
        )
        .expect("old config");
        assert_eq!(old.tls.cert.as_deref(), Some("host.crt"));
        assert_eq!(old.tls.key.as_deref(), Some("host.key"));
        assert!(old.tls.minimum_version.is_none());
        assert!(old.tls.disabled_cipher_suites.is_empty());
        assert!(old.tls.expected_sans.is_empty());

        let new: PierFileConfig = serde_json::from_str(
            r#"{
                "tls":{
                    "mode":"pem",
                    "certificate":"host.crt",
                    "private_key":"host.key",
                    "minimum_version":"TLS1.3",
                    "disabled_cipher_suites":["TLS13_CHACHA20_POLY1305_SHA256"],
                    "expiry_warning_days":45,
                    "expected_sans":["pier.example.test","192.0.2.10"]
                },
                "audio":{"enabled":true,"compressed":false},
                "microphone_input":{"enabled":false},
                "platform":{}
            }"#,
        )
        .expect("new config");
        assert_eq!(new.tls.mode.as_deref(), Some("pem"));
        assert_eq!(new.tls.cert.as_deref(), Some("host.crt"));
        assert_eq!(new.tls.key.as_deref(), Some("host.key"));
        assert_eq!(new.tls.minimum_version.as_deref(), Some("TLS1.3"));
        assert_eq!(new.tls.expiry_warning_days, Some(45));
        assert_eq!(new.tls.expected_sans.len(), 2);
    }

    #[test]
    fn unsupported_tls_source_fields_are_rejected() {
        for document in [
            r#"{"tls":{"mode":"windows_store"},"audio":{"enabled":true,"compressed":false},"microphone_input":{"enabled":false},"platform":{}}"#,
            r#"{"tls":{"windows_store":"LocalMachine"},"audio":{"enabled":true,"compressed":false},"microphone_input":{"enabled":false},"platform":{}}"#,
            r#"{"tls":{"cng_key":"provider/key"},"audio":{"enabled":true,"compressed":false},"microphone_input":{"enabled":false},"platform":{}}"#,
            r#"{"tls":{"self_signed_auto":true},"audio":{"enabled":true,"compressed":false},"microphone_input":{"enabled":false},"platform":{}}"#,
        ] {
            let parsed = serde_json::from_str::<PierFileConfig>(document);
            if document.contains("\"mode\"") {
                assert!(parsed.is_ok(), "closed mode is validated during merge");
            } else {
                assert!(parsed.is_err(), "unsupported source field was accepted");
            }
        }
    }

    #[test]
    fn logging_rejects_unknown_settings() {
        let result = serde_json::from_str::<PierFileConfig>(
            r#"{"logging":{"verbosity":1,"copytruncate":true},"audio":{"enabled":true,"compressed":false},"microphone_input":{"enabled":false},"platform":{}}"#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn disclaimer_rejects_unknown_settings() {
        let result = serde_json::from_str::<PierFileConfig>(
            r#"{"auth":{"disclaimer":{"enabled":true,"truncate":true}},"audio":{"enabled":true,"compressed":false},"microphone_input":{"enabled":false},"platform":{}}"#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn reconnect_window_uses_shared_inclusive_bounds() {
        assert_eq!(crate::validated_reconnect_window(0), Ok(0));
        assert_eq!(crate::validated_reconnect_window(7_200), Ok(7_200));
        assert!(crate::validated_reconnect_window(7_201).is_err());
    }

    #[test]
    fn deskside_rejects_incomplete_enabled_configuration() {
        let parsed: PierFileConfig = serde_json::from_str(
            r#"{
                "audio":{"enabled":true,"compressed":false},
                "microphone_input":{"enabled":false},
                "platform":{"desktop":{"deskside":{"enabled":true}}}
            }"#,
        )
        .expect("closed schema");
        assert!(parsed.platform.desktop.deskside.validate().is_err());
    }

    #[test]
    fn packaged_template_matches_the_shared_schema() {
        let parsed: PierFileConfig =
            serde_json::from_str(include_str!("../../../packaging/windows/pier.json"))
                .expect("packaged config");
        assert!(parsed.audio.enabled);
        assert!(!parsed.audio.compressed);
        assert!(!parsed.microphone_input.enabled);
    }
}
