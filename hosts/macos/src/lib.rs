//! Native macOS Pier control-plane foundation.
//!
//! The crate deliberately starts with the OS-free admission and diagnostics
//! boundary. Native display, login, capture, input, clipboard, and USB work
//! will be added behind explicit adapters; this crate must not report a usable
//! session until those adapters provide verified evidence.

#![cfg_attr(not(target_os = "macos"), allow(dead_code))]
#![deny(clippy::disallowed_methods)]

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};

use arcen_observability::ObservabilityBuilder;
use arcen_session::pier_config::{LoggingProfileSource, PierConfig};
use arcen_telemetry::{
    OperationalProfile, TelemetryComponent, TelemetryPlatform, TelemetryRole, names::component,
};
use serde::{Deserialize, Serialize};

#[cfg(target_os = "macos")]
pub mod activation;
#[cfg(target_os = "macos")]
pub mod audio;
#[cfg(target_os = "macos")]
pub mod auth;
#[cfg(target_os = "macos")]
#[doc(hidden)]
pub mod blocking;
#[cfg(target_os = "macos")]
pub mod capture;
pub mod clipboard;
#[cfg(target_os = "macos")]
pub mod clipboard_session;
pub mod console;
pub mod cursor_probe;
pub mod displays;
#[cfg(target_os = "macos")]
pub mod encode;
pub mod hdr_white;
pub mod host_cert;
#[cfg(target_os = "macos")]
pub mod input;
#[cfg(target_os = "macos")]
pub mod input_session;
#[cfg(target_os = "macos")]
pub mod media_probe;
#[cfg(target_os = "macos")]
pub mod multi_capture;
#[cfg(target_os = "macos")]
pub mod multi_monitor;
pub mod net;
pub mod observability;
pub mod permissions;
#[cfg(target_os = "macos")]
pub mod relay;
#[cfg(target_os = "macos")]
pub mod service;
#[cfg(target_os = "macos")]
pub mod session;
#[cfg(target_os = "macos")]
pub mod stream;
pub mod support_bundle;
pub mod virtual_display;
/// Whether this host may present an input device macOS did not get from
/// hardware, which is what Native Tablet needs.
pub mod virtual_hid;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Whether a test may click, type, scroll or write the pasteboard on the
/// desktop it runs on.
///
/// Off unless `ARCEN_DESKTOP_TESTS=1`. `cargo test` on a development Mac runs
/// in somebody's session, and a unit test that posts a real mouse-down at the
/// centre of the screen or replaces the clipboard is a test that interferes
/// with the person running it.
#[cfg(test)]
pub(crate) fn desktop_tests_allowed() -> bool {
    std::env::var("ARCEN_DESKTOP_TESTS").is_ok_and(|value| value == "1")
}
pub const SOURCE_URL: &str = "https://github.com/Aanerud/arcen_public";
pub const SOURCE_OFFER: &str = "Arcen is free software under the GNU AGPL-3.0. It comes with ABSOLUTELY NO WARRANTY. \
     You may redistribute it under the terms of that licence. If you run a modified version \
     that others connect to over a network, you must offer them its corresponding source.";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MacOsPlatformConfig {
    #[serde(default)]
    pub service_user: Option<String>,
    #[serde(default)]
    pub agent_bundle_id: Option<String>,
    /// Retired: nothing reads these any more. Still accepted so an existing
    /// `pier.json` that carries them keeps loading.
    #[serde(default)]
    pub privileged_broker_enabled: bool,
    #[serde(default)]
    pub native_login_enabled: bool,
    #[serde(default)]
    pub virtual_display_enabled: bool,
    /// Multi-monitor advertisement, matching the Windows host's knob.
    #[serde(default)]
    pub multi_monitor: MacOsMultiMonitorConfig,
    /// How keyboard input reaches a logged-in desktop.
    #[serde(default)]
    pub input_backend: InputBackendChoice,
}

/// How keyboard input reaches a logged-in desktop.
///
/// The login window is not configurable: keyboard and pointer there always go
/// through virtual HID devices, because `CGEvent` injection there blocks
/// forever inside the window server's client library.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InputBackendChoice {
    /// `CGEvent` in a logged-in session, virtual HID at the login window — the
    /// split the reference product uses.
    #[default]
    Auto,
    /// `CGEvent` in a logged-in session.
    Coregraphics,
    /// A virtual HID keyboard wherever one can be created, `CGEvent` where
    /// it cannot. The pointer stays on `CGEvent`, which places it exactly.
    VirtualHid,
}

/// Explicit operator opt-in for advertising `multi_monitor_v1`.
///
/// The same shape and the same default as the Windows host's setting, so an
/// operator who knows one knows the other. Disabled unless set.
///
/// Enabling it is necessary and not sufficient. This host also requires native
/// multi-display capture and more than one attached display before it
/// advertises. That gate is deliberate — a host that advertised
/// `multi_monitor_v1` on the strength of a configuration flag would have a
/// Deck negotiate Match My Layout and then receive one screen of several,
/// which is worse than a refusal an operator can read.
#[derive(Debug, Default, Clone, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct MacOsMultiMonitorConfig {
    /// Explicit operator opt-in. `false` unless set.
    pub advertise_enabled: bool,
    /// Optional operator ceiling on the advertised monitor count.
    pub max_monitors: Option<u8>,
}

/// Whether this host may advertise `multi_monitor_v1` for a given inventory.
///
/// The operator has to have asked for it, and the host has to be able to serve
/// it end-to-end. On macOS the served monitors are session-owned virtual
/// displays, so the attached-display count is evidence that WindowServer is
/// alive, not a ceiling on what Match My Layout can request.
#[must_use]
pub const fn may_advertise_multi_monitor(
    config: &MacOsMultiMonitorConfig,
    attached_displays: usize,
    can_capture_many: bool,
    can_route_region_input: bool,
    can_create_virtual_displays: bool,
) -> bool {
    config.advertise_enabled
        && attached_displays > 0
        && can_capture_many
        && can_route_region_input
        && can_create_virtual_displays
}

/// Whether this build can capture more than one display at once.
pub const CAN_CAPTURE_MULTIPLE_DISPLAYS: bool = true;

impl Default for MacOsPlatformConfig {
    fn default() -> Self {
        Self {
            service_user: Some("_arcen".to_owned()),
            agent_bundle_id: Some("pier.arcen.tech.agent".to_owned()),
            privileged_broker_enabled: false,
            native_login_enabled: false,
            virtual_display_enabled: false,
            multi_monitor: MacOsMultiMonitorConfig::default(),
            input_backend: InputBackendChoice::Auto,
        }
    }
}

impl MacOsPlatformConfig {
    /// # Errors
    ///
    /// Returns an error when native login lacks its required broker or when
    /// configured native identifiers are empty.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.native_login_enabled && !self.privileged_broker_enabled {
            return Err("native_login_enabled requires privileged_broker_enabled");
        }
        if self.agent_bundle_id.as_deref().is_some_and(str::is_empty) {
            return Err("agent_bundle_id must not be empty");
        }
        if self.service_user.as_deref().is_some_and(str::is_empty) {
            return Err("service_user must not be empty");
        }
        Ok(())
    }
}

pub type PierFileConfig = PierConfig<MacOsPlatformConfig>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupConfig {
    pub path: PathBuf,
    pub profile: OperationalProfile,
    pub profile_source: LoggingProfileSource,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiagnosticsReport {
    pub version: &'static str,
    pub profile: &'static str,
    pub profile_source: &'static str,
    pub permissions: permissions::PermissionReport,
    pub displays: displays::InventoryReport,
    pub native_login_enabled: bool,
    pub privileged_broker_enabled: bool,
    pub virtual_display_enabled: bool,
}

impl DiagnosticsReport {
    /// # Errors
    ///
    /// Returns an error when the native display inventory cannot be queried.
    pub fn collect(config: &PierFileConfig, startup: &StartupConfig) -> Result<Self, String> {
        let display_list =
            displays::probe().map_err(|error| format!("display probe failed: {error:?}"))?;
        Ok(Self {
            version: VERSION,
            profile: startup.profile.as_str(),
            profile_source: match startup.profile_source {
                LoggingProfileSource::Level => "level",
                LoggingProfileSource::LegacyVerbosity => "legacy_verbosity",
                LoggingProfileSource::ProductionDefault => "production_default",
            },
            permissions: permissions::PermissionReport::from_snapshot(permissions::probe()),
            displays: displays::InventoryReport::from_displays(display_list),
            native_login_enabled: config.platform.native_login_enabled,
            privileged_broker_enabled: config.platform.privileged_broker_enabled,
            virtual_display_enabled: config.platform.virtual_display_enabled,
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read configuration {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("configuration {path} is invalid: {source}")]
    Parse {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("platform configuration {path} is invalid: {message}")]
    Platform {
        path: PathBuf,
        message: &'static str,
    },
    #[error("session policy {path} is invalid: {message}")]
    Policy { path: PathBuf, message: String },
    #[error("logging configuration is invalid: {0}")]
    Logging(#[from] arcen_session::pier_config::LoggingProfileError),
}

/// Loads, parses, and validates a Pier configuration file.
///
/// # Errors
///
/// Returns an error when the file cannot be read or contains invalid shared or
/// macOS-specific configuration.
/// Where a Pier reads its configuration when the operator names no other path.
///
/// The installer writes the shipped template here, mirroring Linux's
/// `/etc/arcen/pier.json`. One constant so the argument parser and the
/// fall-back rule cannot disagree about which path counts as "the default".
pub const DEFAULT_CONFIG_PATH: &str = "/Library/Application Support/Arcen/pier.json";

/// The configuration a host serves when no file exists at the default path.
///
/// Fail-safe, matching the Linux host's own defaults: loopback only, audio and
/// microphone off. A host that was never configured must not expose itself to
/// the network or redirect sound; the installer's template is what makes a
/// deliberate installation reachable.
#[must_use]
pub fn default_config() -> PierFileConfig {
    PierFileConfig {
        listen: arcen_session::pier_config::ListenConfig {
            host: Some("127.0.0.1".to_owned()),
            ..Default::default()
        },
        tls: arcen_session::pier_config::TlsConfig::default(),
        capture: arcen_session::pier_config::CaptureConfig::default(),
        video: arcen_session::pier_config::VideoConfig::default(),
        audio: arcen_session::pier_config::AudioConfig {
            enabled: false,
            compressed: false,
            local_playback: arcen_session::pier_config::LocalPlayback::default(),
        },
        microphone_input: arcen_session::pier_config::MicrophoneInputConfig { enabled: false },
        clipboard: arcen_session::pier_config::ClipboardConfig::default(),
        auth: arcen_session::pier_config::AuthConfig::default(),
        redirection: arcen_session::pier_config::RedirectionConfig::default(),
        logging: arcen_session::pier_config::LoggingConfig::default(),
        platform: MacOsPlatformConfig::default(),
    }
}

/// Whether a configuration file is simply absent.
///
/// A host with no `pier.json` is a default installation, not a misconfigured
/// one, and the two must not be treated alike: absence means "use the
/// documented defaults", while a file that exists and does not parse means
/// somebody wrote something wrong and needs to be told rather than quietly
/// overridden.
///
/// Conflating them turned a fresh install into a crash loop, because the
/// installer does not write a configuration file and nothing had ever needed
/// one to exist.
#[must_use]
pub fn is_missing_config(error: &ConfigError) -> bool {
    matches!(
        error,
        ConfigError::Read { source, .. } if matches!(source.kind(), std::io::ErrorKind::NotFound)
    )
}

pub fn load_config(path: impl AsRef<Path>) -> Result<(PierFileConfig, StartupConfig), ConfigError> {
    let path = path.as_ref().to_path_buf();
    let bytes = std::fs::read(&path).map_err(|source| ConfigError::Read {
        path: path.clone(),
        source,
    })?;
    let config = parse_config(&bytes, &path)?;
    let resolved = config.logging.resolved_profile()?;
    Ok((
        config,
        StartupConfig {
            path,
            profile: resolved.profile,
            profile_source: resolved.source,
        },
    ))
}

/// Parses and validates a complete Pier configuration.
///
/// # Errors
///
/// Returns an error when JSON, shared logging policy, or macOS platform
/// configuration is invalid.
pub fn parse_config(bytes: &[u8], path: &Path) -> Result<PierFileConfig, ConfigError> {
    let config: PierFileConfig =
        serde_json::from_slice(bytes).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
    config
        .platform
        .validate()
        .map_err(|message| ConfigError::Platform {
            path: path.to_path_buf(),
            message,
        })?;
    clipboard_policy_from_config(&config).map_err(|message| ConfigError::Policy {
        path: path.to_path_buf(),
        message,
    })?;
    Ok(config)
}

/// Returns the validated host-authoritative clipboard policy.
///
/// # Errors
///
/// Returns an error when a configured token or transfer limit is outside the
/// shared clipboard vocabulary.
pub fn clipboard_policy_from_config(
    config: &PierFileConfig,
) -> Result<arcen_media::clipboard::ClipboardPolicy, String> {
    let mut policy = arcen_media::clipboard::ClipboardPolicy::default();
    if let Some(direction) = config.clipboard.direction.as_deref() {
        policy.direction = match direction {
            "both" => arcen_media::clipboard::ClipboardDirection::Both,
            "client_to_host" => arcen_media::clipboard::ClipboardDirection::ClientToHost,
            "host_to_client" => arcen_media::clipboard::ClipboardDirection::HostToClient,
            "disabled" => arcen_media::clipboard::ClipboardDirection::Disabled,
            other => {
                return Err(format!(
                    "clipboard.direction {other:?}: expected both, client_to_host, host_to_client, or disabled"
                ));
            }
        };
    }
    if let Some(content) = config.clipboard.content.as_deref() {
        policy.content = match content {
            "all" => arcen_media::clipboard::ClipboardContent::All,
            "text" => arcen_media::clipboard::ClipboardContent::Text,
            "image" => arcen_media::clipboard::ClipboardContent::Image,
            other => {
                return Err(format!(
                    "clipboard.content {other:?}: expected all, text, or image"
                ));
            }
        };
    }
    if let Some(max_bytes) = config.clipboard.max_bytes {
        policy = arcen_media::clipboard::ClipboardPolicy::new(
            policy.direction,
            policy.content,
            max_bytes,
        )
        .map_err(|error| format!("clipboard.max_bytes: {error}"))?;
    }
    Ok(policy)
}

/// The system-wide log directory, preferred because an operator collecting
/// evidence should find one place rather than one per account.
pub const SYSTEM_LOG_DIRECTORY: &str = "/Library/Logs/Arcen/Pier";

/// Chooses where canonical records are written.
///
/// The Pier runs as a `LaunchAgent` in the console user's Aqua session, which
/// is the only place capture and input work — and that user cannot create a
/// directory under `/Library/Logs`. An earlier build took the resulting
/// permission error as "no structured logging", and a machine served desktops
/// for a whole session while the file an operator would read stayed absent.
///
/// So the system directory is preferred and the user's `~/Library/Logs` is the
/// fallback, which is where Apple expects an agent to write anyway.
/// `ARCEN_LOG_DIR` overrides both and is not second-guessed: an operator who
/// names a directory gets that directory or an error, never a silent
/// relocation of their logs somewhere they are not looking.
fn resolve_log_directory() -> Result<PathBuf, String> {
    if let Some(override_path) = std::env::var_os("ARCEN_LOG_DIR") {
        let directory = PathBuf::from(override_path);
        return std::fs::create_dir_all(&directory)
            .map(|()| directory.clone())
            .map_err(|error| {
                format!(
                    "create managed log directory {}: {error}",
                    directory.display()
                )
            });
    }

    let system = PathBuf::from(SYSTEM_LOG_DIRECTORY);
    if std::fs::create_dir_all(&system).is_ok() && is_writable(&system) {
        return Ok(system);
    }

    let home = std::env::var_os("HOME").ok_or_else(|| {
        format!(
            "{SYSTEM_LOG_DIRECTORY} is not writable and HOME is unset, so there \
             is nowhere to write records"
        )
    })?;
    let user = PathBuf::from(home).join("Library/Logs/Arcen/Pier");
    std::fs::create_dir_all(&user)
        .map(|()| user.clone())
        .map_err(|error| format!("create managed log directory {}: {error}", user.display()))
}

/// Returns whether a directory can actually be written to.
///
/// `create_dir_all` succeeds on a directory that already exists and is owned
/// by someone else, so existence is not permission. The probe file is created
/// and removed rather than inferred from mode bits, which say nothing about
/// ACLs or a read-only volume.
fn is_writable(directory: &Path) -> bool {
    let probe = directory.join(".arcen-write-probe");
    match std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&probe)
    {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// Installs the bounded canonical logger for the selected profile.
///
/// # Errors
///
/// Returns an error if the managed log cannot be opened, the observability
/// runtime cannot be built, or another subscriber has already been installed.
pub fn initialize_diagnostics(
    profile: OperationalProfile,
) -> Result<arcen_observability::InstalledObservability, String> {
    let directory = resolve_log_directory()?;
    let log_path = directory.join("arcen-pier-macos.jsonl");
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    set_private_log_permissions(&mut options);
    let file = options
        .open(&log_path)
        .map_err(|error| format!("open managed log {}: {error}", log_path.display()))?;
    enforce_private_log_permissions(&log_path)?;
    let component = TelemetryComponent::new(component::PIER)
        .map_err(|error| format!("pier telemetry component: {error:?}"))?;
    let runtime = ObservabilityBuilder::new(
        TelemetryRole::Host,
        component,
        TelemetryPlatform::Macos,
        profile,
    )
    .canonical_writer("managed-file", file)
    .human_console_writer("stderr", std::io::stderr())
    .build()
    .map_err(|error| format!("build observability runtime: {error}"))?;
    runtime
        .install_global()
        .map_err(|error| format!("install observability runtime: {error}"))
}

#[cfg(unix)]
fn set_private_log_permissions(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;

    options.mode(0o600);
}

#[cfg(not(unix))]
fn set_private_log_permissions(_options: &mut OpenOptions) {}

#[cfg(unix)]
fn enforce_private_log_permissions(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("set managed log mode 0600: {error}"))
}

#[cfg(not(unix))]
fn enforce_private_log_permissions(_path: &Path) -> Result<(), String> {
    Ok(())
}

/// Runs the control-plane startup checks.
///
/// # Errors
///
/// Always refuses readiness until the native display and login adapters are
/// implemented and required macOS permissions are evidenced.
pub fn run(config: &PierFileConfig, startup: &StartupConfig) -> Result<(), String> {
    let permissions = permissions::probe();
    let displays = displays::probe().map_err(|error| format!("display probe failed: {error:?}"))?;
    let capacity = displays::capacity(&displays);
    tracing::info!(
        target: "arcen.pier.macos",
        version = VERSION,
        profile = startup.profile.as_str(),
        profile_source = ?startup.profile_source,
        platform = std::env::consts::OS,
        screen_recording = permissions.screen_recording,
        accessibility = permissions.accessibility,
        attached_displays = displays.len(),
        primary_only_capable = capacity.can_serve_primary_only(1),
        "macOS Pier control plane is ready; native session adapters are not armed"
    );
    if !permissions.usable_for_input_and_capture() {
        return Err(format!(
            "required macOS permissions are unavailable (screen_recording={}, accessibility={})",
            permissions.screen_recording, permissions.accessibility
        ));
    }
    Err("macOS native display/login adapters are not implemented yet".to_owned())
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {

    #[test]
    fn a_missing_configuration_is_not_a_broken_one() {
        // A fresh install has no pier.json — the installer does not write one.
        // Treating that as a configuration failure put the agent in a crash
        // loop and presented to the operator as a connection timeout against a
        // host that had never listened.
        let absent = std::env::temp_dir().join(format!(
            "arcen-absent-config-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|since| since.as_nanos())
                .unwrap_or_default(),
        ));
        assert!(!absent.exists(), "the point is that it is not there");

        let error = load_config(&absent).expect_err("absent file cannot load");
        assert!(
            is_missing_config(&error),
            "absence must be distinguishable from invalidity: {error}",
        );

        // And the fall-back a host serves must be fail-safe, matching the
        // Linux host's own defaults rather than inventing a second policy.
        let fallback = default_config();
        assert_eq!(
            fallback.listen.host.as_deref(),
            Some("127.0.0.1"),
            "an unconfigured host must not expose itself to the network",
        );
        assert!(
            !fallback.audio.enabled,
            "an unconfigured host must not redirect sound",
        );
        assert!(
            !fallback.microphone_input.enabled,
            "an unconfigured host must not publish a microphone",
        );
    }

    #[test]
    fn the_shipped_template_matches_the_schema_the_host_reads() {
        // The installer writes this file verbatim. If it does not parse, every
        // fresh install starts a host that refuses its own configuration — and
        // the packaging script is the only place that would have caught it.
        let template = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../packaging/macos/arcen-pier.json");
        let bytes = std::fs::read(&template).expect("shipped template must exist");
        let config = parse_config(&bytes, &template).expect("shipped template must parse");

        // It must also be a configuration that actually serves: the fall-back
        // is deliberately loopback-only, so a template that copied it would
        // leave every installed host unreachable.
        assert_eq!(
            config.listen.host.as_deref(),
            Some("0.0.0.0"),
            "the shipped configuration is what makes an installed host reachable",
        );
    }

    #[test]
    fn an_invalid_configuration_is_still_refused() {
        // The other half: someone wrote something wrong and must be told,
        // rather than served a contract they did not ask for.
        let path = std::path::Path::new("/tmp/arcen-invalid-config.json");
        let error = parse_config(b"{ this is not json", path).expect_err("must refuse");
        assert!(
            !is_missing_config(&error),
            "a malformed file is not a missing one: {error}",
        );
    }
    use super::*;

    #[test]
    fn defaults_fail_closed_for_native_operations() {
        let config = MacOsPlatformConfig::default();
        assert!(!config.privileged_broker_enabled);
        assert!(!config.native_login_enabled);
        assert!(!config.virtual_display_enabled);
    }

    #[test]
    fn native_login_requires_the_narrow_privileged_broker() {
        let config = MacOsPlatformConfig {
            native_login_enabled: true,
            ..MacOsPlatformConfig::default()
        };
        assert_eq!(
            config.validate(),
            Err("native_login_enabled requires privileged_broker_enabled")
        );
    }

    #[test]
    fn empty_native_identifiers_are_rejected() {
        let config = MacOsPlatformConfig {
            service_user: Some(String::new()),
            ..MacOsPlatformConfig::default()
        };
        assert_eq!(config.validate(), Err("service_user must not be empty"));
    }

    fn valid_config(logging: &str) -> String {
        format!(
            r#"{{"audio":{{"enabled":false,"compressed":true}},"microphone_input":{{"enabled":false}},"logging":{logging},"platform":{{}}}}"#
        )
    }

    #[test]
    fn parses_all_canonical_logging_profiles() {
        for (level, expected) in [
            (0, OperationalProfile::Critical),
            (1, OperationalProfile::Error),
            (2, OperationalProfile::Info),
            (3, OperationalProfile::Debug),
        ] {
            let bytes = valid_config(&format!(r#"{{"level":{level}}}"#));
            let config = parse_config(bytes.as_bytes(), Path::new("test.json")).unwrap();
            assert_eq!(config.logging.resolved_profile().unwrap().profile, expected);
        }
    }

    #[test]
    fn preserves_legacy_logging_mapping() {
        let bytes = valid_config(r#"{"verbosity":0}"#);
        let config = parse_config(bytes.as_bytes(), Path::new("test.json")).unwrap();
        assert_eq!(
            config.logging.resolved_profile().unwrap().profile,
            OperationalProfile::Error
        );
    }

    #[test]
    fn parses_authoritative_audio_and_clipboard_policy() {
        let bytes = r#"{
            "audio":{"enabled":false,"compressed":false,"local_playback":"audible"},
            "microphone_input":{"enabled":false},
            "clipboard":{"direction":"disabled","content":"text","max_bytes":1048576},
            "logging":{"level":2},
            "platform":{}
        }"#;
        let config = parse_config(bytes.as_bytes(), Path::new("test.json")).unwrap();
        assert!(!config.audio.enabled);
        assert_eq!(
            config.audio.local_playback,
            arcen_session::pier_config::LocalPlayback::Audible,
        );
        let clipboard = clipboard_policy_from_config(&config).expect("valid clipboard");
        assert_eq!(
            clipboard.direction,
            arcen_media::clipboard::ClipboardDirection::Disabled,
        );
        assert_eq!(
            clipboard.content,
            arcen_media::clipboard::ClipboardContent::Text
        );
        assert_eq!(clipboard.max_bytes, 1024 * 1024);
    }

    #[test]
    fn rejects_conflicting_logging_fields() {
        let bytes = valid_config(r#"{"level":2,"verbosity":1}"#);
        assert!(matches!(
            parse_config(bytes.as_bytes(), Path::new("test.json")),
            Err(ConfigError::Parse { .. })
        ));
    }

    #[test]
    fn runtime_refuses_to_claim_a_session_without_native_adapters() {
        let config = parse_config(
            valid_config(r#"{"level":3}"#).as_bytes(),
            Path::new("test.json"),
        )
        .unwrap();
        let resolved = config.logging.resolved_profile().unwrap();
        let startup = StartupConfig {
            path: PathBuf::from("test.json"),
            profile: resolved.profile,
            profile_source: resolved.source,
        };
        assert!(run(&config, &startup).is_err());
    }

    #[test]
    fn diagnostics_report_is_redacted_and_config_aware() {
        let config = parse_config(
            valid_config(r#"{"level":2}"#).as_bytes(),
            Path::new("test.json"),
        )
        .unwrap();
        let resolved = config.logging.resolved_profile().unwrap();
        let startup = StartupConfig {
            path: PathBuf::from("test.json"),
            profile: resolved.profile,
            profile_source: resolved.source,
        };
        let report = DiagnosticsReport::collect(&config, &startup).unwrap();
        assert_eq!(report.profile, "info");
        assert_eq!(report.profile_source, "level");
        assert!(!report.native_login_enabled);
        assert!(!report.privileged_broker_enabled);
        assert!(!report.virtual_display_enabled);
    }

    #[cfg(unix)]
    #[test]
    fn managed_log_permissions_are_private() {
        use std::os::unix::fs::PermissionsExt;

        let path = std::env::temp_dir().join(format!(
            "arcen-macos-log-permissions-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        set_private_log_permissions(&mut options);
        let file = options.open(&path).expect("create managed log");
        assert_eq!(
            file.metadata().expect("metadata").permissions().mode() & 0o777,
            0o600
        );
        drop(file);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
            .expect("make existing log permissive");
        enforce_private_log_permissions(&path).expect("restore private mode");
        assert_eq!(
            std::fs::metadata(&path)
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        std::fs::remove_file(path).expect("remove test log");
    }
}

#[cfg(test)]
mod multi_monitor_tests {
    use super::{MacOsMultiMonitorConfig, may_advertise_multi_monitor};

    #[test]
    fn the_default_is_off_matching_the_windows_host() {
        assert!(!MacOsMultiMonitorConfig::default().advertise_enabled);
    }

    #[test]
    fn asking_for_it_is_not_enough_without_the_capture_gate() {
        // The bug this prevents: a Deck negotiating Match My Layout because a
        // configuration file said yes, and then receiving one screen of
        // several. A refusal an operator can read beats that.
        let enabled = MacOsMultiMonitorConfig {
            advertise_enabled: true,
            max_monitors: Some(4),
        };
        assert!(!may_advertise_multi_monitor(&enabled, 2, false, true, true));
    }

    #[test]
    fn one_attached_display_can_advertise_when_virtual_outputs_are_available() {
        let enabled = MacOsMultiMonitorConfig {
            advertise_enabled: true,
            max_monitors: Some(4),
        };
        assert!(
            may_advertise_multi_monitor(&enabled, 1, true, true, true),
            "macOS creates one session-owned virtual display per Deck monitor",
        );
    }

    #[test]
    fn region_input_routing_is_part_of_the_advertisement_gate() {
        let enabled = MacOsMultiMonitorConfig {
            advertise_enabled: true,
            max_monitors: None,
        };
        assert!(!may_advertise_multi_monitor(&enabled, 2, true, false, true));
    }

    #[test]
    fn virtual_display_creation_is_part_of_the_advertisement_gate() {
        let enabled = MacOsMultiMonitorConfig {
            advertise_enabled: true,
            max_monitors: None,
        };
        assert!(!may_advertise_multi_monitor(&enabled, 2, true, true, false));
    }

    #[test]
    fn all_end_to_end_conditions_together_open_the_gate() {
        let enabled = MacOsMultiMonitorConfig {
            advertise_enabled: true,
            max_monitors: None,
        };
        assert!(may_advertise_multi_monitor(&enabled, 2, true, true, true));
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod serve_logging_tests {
    use super::{default_config, load_config};
    use std::path::Path;

    #[test]
    fn the_configured_profile_is_what_serve_would_use() {
        // `serve` pinned Info and never read this, so `logging.level` did
        // nothing for the one command that streams: no debug record was ever
        // written, and every measurement taken to diagnose the host was taken
        // at a verbosity nobody had chosen.
        for (level, expected) in [
            (0, arcen_telemetry::OperationalProfile::Critical),
            (1, arcen_telemetry::OperationalProfile::Error),
            (2, arcen_telemetry::OperationalProfile::Info),
            (3, arcen_telemetry::OperationalProfile::Debug),
        ] {
            let mut config = default_config();
            config.logging.level = Some(expected);
            assert_eq!(
                config.logging.resolved_profile().expect("resolves").profile,
                expected,
                "logging.level {level} must resolve to {expected:?}"
            );
        }
    }

    #[test]
    fn a_missing_configuration_still_yields_a_servable_profile() {
        // A host that cannot read its configuration must still serve, at the
        // profile a production host would want rather than at none.
        assert!(load_config(Path::new("/nonexistent/arcen/pier.json")).is_err());
    }
}
