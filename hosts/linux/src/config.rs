//! Persisted Linux Pier configuration.

use std::path::{Path, PathBuf};

use arcen_session::pier_config::PierConfig;
use serde::Deserialize;

pub type PierFileConfig = PierConfig<LinuxPlatformConfig>;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LinuxPlatformConfig {
    pub auth: LinuxAuthConfig,
    pub capture: LinuxCaptureConfig,
    pub session: LinuxSessionConfig,
    pub input: LinuxInputConfig,
    pub audio: LinuxAudioBackendConfig,
    pub logging: LinuxLoggingConfig,
    pub deskside: crate::deskside::LinuxDesksideConfig,
    pub multi_monitor: LinuxMultiMonitorConfig,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LinuxAuthConfig {
    pub mode: Option<String>,
    pub pam_service: Option<String>,
    /// Retired by SEC-001 and retained only so an existing `pier.json` still
    /// parses under `deny_unknown_fields`. A release build refuses to start if
    /// this is `true`; it is honoured only by an `insecure-lab-no-auth` build.
    pub unsafe_allow_remote_no_auth: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LinuxCaptureConfig {
    pub monitor: Option<u32>,
    pub display: Option<String>,
    pub xauthority: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LinuxSessionConfig {
    pub desktop: Option<String>,
    pub display: Option<String>,
    pub gpu_head: Option<String>,
    pub xorg_bin: Option<String>,
    pub xorg_config_template: Option<String>,
    pub runtime_root: Option<String>,
    pub agent_bin: Option<String>,
    pub launcher_bin: Option<String>,
    pub zoneinfo_root: Option<String>,
    pub disconnected_idle_timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LinuxInputConfig {
    pub mode: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LinuxAudioBackendConfig {
    pub capture_binary: Option<String>,
    pub user: Option<String>,
    pub pactl_binary: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LinuxLoggingConfig {
    pub managed_log: Option<String>,
}

/// Operator-facing configuration gate for the `multi_monitor_v1` capability.
/// Defaults to fully disabled (`advertise_enabled: false`, empty `heads`):
/// this host advertises and admits nothing until an operator explicitly sets
/// both fields, matching the "safe/off until target validation" requirement.
/// This gate is the sole production safety switch: the separate, hardcoded
/// `media::multi_capenc::MULTI_MONITOR_CARRIER_READY` gate is `true` now
/// that Carrier A is fully wired end to end (see `session::multi_monitor`),
/// so it no longer withholds anything on its own.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LinuxMultiMonitorConfig {
    /// Explicit operator opt-in. `false` unless set.
    pub advertise_enabled: bool,
    /// Ordered NVIDIA head tokens available to plan against, e.g.
    /// `["DFP-0", "DFP-1"]`. Empty (the default) means no heads are
    /// configured, which withholds the offer regardless of
    /// `advertise_enabled`.
    pub heads: Vec<String>,
    /// Optional operator ceiling for simultaneous NVENC sessions on the
    /// configured GPU.
    pub nvenc_session_limit: Option<u8>,
    /// Whether monitors that explicitly permit 4:2:0 may use the software
    /// encoder when the NVENC ceiling is exhausted.
    pub allow_software_fallback: bool,
}

pub struct LoadedConfig {
    pub path: PathBuf,
    pub value: PierFileConfig,
}

pub fn load(explicit_path: Option<PathBuf>) -> Result<Option<LoadedConfig>, String> {
    let (path, required) = explicit_path.map_or_else(
        || (PathBuf::from("/etc/arcen/pier.json"), false),
        |path| (path, true),
    );
    if !path.exists() {
        return if required {
            Err(format!("Pier config does not exist: {}", path.display()))
        } else {
            Ok(None)
        };
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
    resolve_optional_path(&mut value.platform.capture.xauthority, base);
    resolve_optional_path(&mut value.platform.session.xorg_bin, base);
    resolve_optional_path(&mut value.platform.session.xorg_config_template, base);
    resolve_optional_path(&mut value.platform.session.runtime_root, base);
    resolve_optional_path(&mut value.platform.session.agent_bin, base);
    resolve_optional_path(&mut value.platform.session.launcher_bin, base);
    resolve_optional_path(&mut value.platform.session.zoneinfo_root, base);
    resolve_optional_path(&mut value.platform.audio.capture_binary, base);
    resolve_optional_path(&mut value.platform.audio.pactl_binary, base);
    resolve_optional_path(&mut value.platform.logging.managed_log, base);
    resolve_optional_pathbuf(&mut value.platform.deskside.console_xauthority, base);
    for path in &mut value.platform.deskside.input_devices {
        if path.is_relative() {
            *path = base.join(&*path);
        }
    }
    Ok(Some(LoadedConfig { path, value }))
}

fn resolve_optional_pathbuf(value: &mut Option<PathBuf>, base: &Path) {
    if let Some(path) = value {
        if path.is_relative() {
            *path = base.join(&*path);
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unknown_or_incomplete_policy() {
        for document in [
            r#"{"audio":{"enabled":true,"compressed":false},"platform":{}}"#,
            r#"{"microphone_input":{"enabled":false},"platform":{}}"#,
            r#"{"audio":{"enabled":true,"compressed":false},"microphone_input":{"enabled":false},"platform":{},"surprise":true}"#,
        ] {
            assert!(serde_json::from_str::<PierFileConfig>(document).is_err());
        }
    }

    #[test]
    fn parses_common_and_linux_sections() {
        let parsed: PierFileConfig = serde_json::from_str(
            r#"{
                "listen":{"host":"0.0.0.0","port":18444},
                "audio":{"enabled":true,"compressed":false},
                "microphone_input":{"enabled":false},
                "platform":{
                    "auth":{"mode":"pam","pam_service":"login"},
                    "capture":{"monitor":1,"display":":11"},
                    "audio":{"user":"session"}
                }
            }"#,
        )
        .expect("config");
        assert_eq!(parsed.listen.port, Some(18_444));
        assert!(!parsed.audio.compressed);
        assert_eq!(parsed.platform.auth.mode.as_deref(), Some("pam"));
        assert_eq!(parsed.platform.capture.display.as_deref(), Some(":11"));
    }

    #[test]
    fn packaged_template_matches_the_strict_schema() {
        let parsed: PierFileConfig =
            serde_json::from_str(include_str!("../../../packaging/linux/arcen-pier.json"))
                .expect("packaged config");
        assert_eq!(parsed.video.codec, None);
        assert_eq!(parsed.video.chroma, None);
        assert_eq!(parsed.video.bit_depth.as_deref(), Some("10"));
        assert_eq!(parsed.video.color_range.as_deref(), Some("full"));
        assert_eq!(parsed.video.encoder.as_deref(), Some("auto"));
        assert!(parsed.audio.enabled);
        assert!(!parsed.audio.compressed);
        assert!(!parsed.microphone_input.enabled);
    }
}
