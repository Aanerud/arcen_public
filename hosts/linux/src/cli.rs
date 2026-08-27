//! Command-line configuration for `arcen-host`.
//!
//! Manual argument parsing (no `clap`), matching the client's dependency-light
//! style. Stage 1 supports the transport/codec/capenc flags; PAM auth,
//! resolution ingest, and native display control arrive in later stages.

use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use arcen_media::clipboard::{
    ClipboardContent, ClipboardDirection, ClipboardPolicy, HARD_MAX_CLIPBOARD_BYTES,
};
use arcen_protocol::messages::{InitialVideoRequestMsg, VideoSelectionIntent};
use arcen_transport::tls::{CertificateTimePolicy, RingCipherSuite, TlsPosture, TlsVersionFloor};

use crate::display::topology::{HeadCapability, HeadInventory};
use crate::media::capenc::{parse_encoder_selection, CapencConfig, EncoderSelection};
use crate::session::identity::UserExecution;

#[cfg(feature = "wss-compat")]
const TLS_VERSION_REQUIREMENT: &str = "TLS1.2 or TLS1.3";
#[cfg(not(feature = "wss-compat"))]
const TLS_VERSION_REQUIREMENT: &str = "TLS1.3";

/// Message returned whenever a release build is asked to disable authentication.
/// Kept as one constant so the CLI, the config loader, and the startup guard all
/// refuse with identical, greppable wording.
const NO_AUTH_REFUSED: &str = "refusing to disable authentication: this build has no \
     unauthenticated mode. `--no-auth`, `--unsafe-allow-remote-no-auth`, \
     `--auth-mode none`, and the `platform.auth.unsafe_allow_remote_no_auth` config \
     key were removed by SEC-001. An isolated-lab build must be compiled with the \
     `insecure-lab-no-auth` Cargo feature, which is never enabled in a release \
     artifact.";

static IGNORED_HELPER_PATH_WARNINGS: OnceLock<Mutex<std::collections::BTreeSet<&'static str>>> =
    OnceLock::new();

fn warn_ignored_helper_path(source: &'static str) {
    let warnings = IGNORED_HELPER_PATH_WARNINGS.get_or_init(|| Mutex::new(Default::default()));
    let mut warnings = warnings
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if warnings.insert(source) {
        eprintln!(
            "warning: SEC-101/SEC-151: ignoring {source}; helpers are dispatched through the current arcen-pier executable"
        );
    }
}

/// Authentication mode for the Pier's listener.
///
/// `Pam` is the only mode a release build can reach. `None` disables the
/// product's single trust boundary and exists solely for an isolated lab
/// network; it requires the `insecure-lab-no-auth` Cargo feature, which is off
/// in every shipped build. See SEC-001.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AuthMode {
    /// No authentication. Compiled out unless `insecure-lab-no-auth` is enabled.
    None,
    /// PAM, the only mode a release build can select.
    #[default]
    Pam,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputMode {
    None,
    Uinput,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioUserMode {
    Session,
    Host,
}

pub use arcen_media::video::ColorPolicy;

/// Resolved host configuration.
#[derive(Debug, Clone)]
pub struct Config {
    pub config_path: Option<PathBuf>,
    pub host: String,
    pub port: u16,
    pub tls_cert: Option<PathBuf>,
    pub tls_key: Option<PathBuf>,
    pub tls_posture: TlsPosture,
    pub tls_time_policy: CertificateTimePolicy,
    pub tls_expected_sans: Vec<String>,
    /// Normalized to `"h264"` or `"h265"`.
    pub codec: String,
    pub encoder: EncoderSelection,
    /// Normalized to `"yuv420"` or `"yuv444"`; unsupported chroma fails closed.
    pub chroma: String,
    /// Ceiling coded component depth; unsupported tokens fail closed.
    /// `video.variant`, when set, overrides this together with `codec`,
    /// `chroma`, `color_range` and `color_matrix`.
    pub bit_depth: arcen_media::BitDepth,
    /// Ceiling coded sample range. Defaults to `limited` so an existing
    /// deployment's wire output does not change until an operator opts in.
    pub color_range: arcen_media::ColorRange,
    /// Ceiling matrix coefficients used to derive luma/chroma from RGB.
    pub color_matrix: arcen_media::ColorMatrix,
    /// Governs how far a negotiating client may deviate from `bit_depth`/
    /// `color_range`/`color_matrix`. See [`ColorPolicy::resolve_bit_depth`].
    pub color_policy: ColorPolicy,
    /// Session codec-selection intent. Host config starts exact; an auth-time
    /// request may opt an unpinned session into adaptive performance.
    pub video_selection: VideoSelectionIntent,
    /// Whether an administrator explicitly pinned `video.codec`/`--codec`.
    /// The internal default codec is only a legacy-client fallback and is not
    /// a pin.
    pub codec_pinned: bool,
    /// Damage-driven QP biasing for this host's encoders.
    ///
    /// Operator-owned rather than client-negotiated, deliberately. It changes
    /// how bits are distributed *within* a frame, never the format the client
    /// decodes, so there is nothing for a client to agree to — and while it is
    /// unproven it should be an experiment an operator opts into on one host,
    /// not something a connecting Deck can switch on for a fleet.
    ///
    /// See `docs/architecture/qp-maps.md` for how to benchmark it.
    pub qp_map: arcen_media::video::QpMapPolicy,
    /// An explicit `video.variant` is an operator pin and cannot be replaced by
    /// a client's adaptive request.
    pub variant_pinned: bool,
    /// Exact auth-time request retained for the post-hello consistency echo.
    pub auth_video_request: Option<InitialVideoRequestMsg>,
    pub fps: u32,
    /// 1-based monitor index (runtime convention); `output_index` subtracts 1.
    pub monitor: u32,
    /// `DISPLAY` the host captures (the host owns `:0`).
    pub display: String,
    /// `XAUTHORITY` for the capture child, if the X server needs one.
    pub xauthority: Option<String>,
    /// Retained only to accept and warn about legacy external-helper settings.
    pub capenc_bin: Option<PathBuf>,
    pub auth_mode: AuthMode,
    pub pam_service: String,
    /// Process-local direct-QUIC resume window. Zero disables resume.
    pub reconnect_window_secs: u32,
    /// Optional Linux-only lifetime for disconnected persistent desktops.
    pub disconnected_idle_lifetime: Option<Duration>,
    /// Opt-in per-desktop process time-zone redirection.
    pub timezone_redirection: bool,
    /// Trusted system time-zone database used for semantic validation.
    pub zoneinfo_root: PathBuf,
    pub disclaimer: Option<Arc<arcen_identity::PreparedDisclaimer>>,
    pub(crate) disclaimer_settings: arcen_session::pier_config::DisclaimerConfig,
    /// User-side executable that launches and supervises the graphical desktop.
    pub session_agent_bin: Option<PathBuf>,
    /// Privileged PAM/logind/session supervisor executable.
    pub session_launcher_bin: Option<PathBuf>,
    /// GNOME session token passed to `/usr/bin/gnome-session`.
    pub desktop_session: String,
    /// Dedicated X display created for PAM sessions.
    pub session_display: String,
    /// Physical NVIDIA head assigned to the dedicated X server.
    pub session_gpu_head: String,
    pub xorg_bin: PathBuf,
    pub xorg_config_template: PathBuf,
    pub session_runtime_root: PathBuf,
    pub input_mode: InputMode,
    pub audio_enabled: bool,
    pub audio_compressed: bool,
    /// Retained only to accept and warn about legacy external-helper settings.
    pub audiocap_bin: Option<PathBuf>,
    pub audio_user_mode: AudioUserMode,
    /// Operator policy for Deck-to-Pier microphone publication.
    pub microphone_input_enabled: bool,
    /// Session-user PulseAudio/PipeWire-Pulse control binary.
    pub pactl_bin: PathBuf,
    pub logging: arcen_session::pier_config::LoggingConfig,
    pub managed_log: Option<PathBuf>,
    pub clipboard_policy: ClipboardPolicy,
    /// Operator-enforced physical-console privacy, disabled by default.
    pub deskside: crate::deskside::LinuxDesksideConfig,
    /// Explicit `multi_monitor_v1` advertisement gate, fully disabled by
    /// default. See `session::multi_monitor::MultiMonitorGate` — this is now
    /// the sole production safety switch, since the separate hardcoded
    /// `media::multi_capenc::MULTI_MONITOR_CARRIER_READY` gate is `true`
    /// (Carrier A is fully wired end to end).
    pub multi_monitor: crate::config::LinuxMultiMonitorConfig,
    /// Explicit acknowledgement that a no-auth host will accept remote clients.
    pub unsafe_allow_remote_no_auth: bool,
    /// Advertised screen size in the server hello (informational until Stage 3
    /// queries the live X mode). 0 ⇒ send a 1920x1080 placeholder.
    pub width: u32,
    pub height: u32,
    /// Deprecated legacy alias for the QUIC UDP port. Product configurations
    /// use `port`; an existing `quic_port` value wins during migration.
    pub quic_port: Option<u16>,
    /// Dormant compatibility listener. This field and its CLI flag do not
    /// exist in product binaries.
    #[cfg(feature = "wss-compat")]
    pub wss_port: Option<u16>,
}

impl Config {
    pub(crate) fn requested_encode_intent(&self) -> arcen_media::EncodeIntent {
        self.auth_video_request
            .as_ref()
            .and_then(|request| {
                arcen_media::EncodeIntent::from_token(&request.quality.encode_intent)
            })
            .unwrap_or_default()
    }

    pub fn exact_pins_allow_software_h264(&self) -> bool {
        if self.codec_pinned && self.codec != "h264" {
            return false;
        }
        !self.variant_pinned
            || (self.codec == "h264"
                && !self.wire_yuv444()
                && self.bit_depth == arcen_media::BitDepth::Eight
                && !self.color_matrix.is_identity()
                && self.fps
                    <= arcen_media::video::EncoderBackend::OpenH264
                        .contract()
                        .max_fps)
    }

    /// Effective direct-session QUIC UDP port.
    pub fn direct_quic_port(&self) -> u16 {
        self.quic_port.unwrap_or(self.port)
    }

    /// 0-based NvFBC desktop-output index (`monitor_index - 1`).
    pub fn output_index(&self) -> u32 {
        self.monitor.saturating_sub(1)
    }

    /// True when the wire chroma is 4:4:4 (drives the `yuv444` capenc token).
    pub fn wire_yuv444(&self) -> bool {
        self.chroma == "yuv444"
    }

    /// TLS is enabled only when BOTH a cert and key are supplied.
    pub fn tls_enabled(&self) -> bool {
        self.tls_cert.is_some() && self.tls_key.is_some()
    }

    pub fn unsafe_remote_no_auth(&self) -> bool {
        self.auth_mode == AuthMode::None
            && self.unsafe_allow_remote_no_auth
            && !is_loopback_bind(&self.host)
    }

    /// Resolve the fused Pier binary used for the `capenc` subcommand.
    pub fn resolve_capenc_binary(&self) -> Option<PathBuf> {
        crate::current_pier_exe()
    }

    /// Build the capenc spawn config for this host configuration.
    pub fn capenc_config(
        &self,
        binary: PathBuf,
        execution: Option<UserExecution>,
        session_log_id: arcen_telemetry::CorrelationId,
        cursor_mode: arcen_protocol::messages::CursorMode,
    ) -> CapencConfig {
        self.capenc_config_for_size(
            binary,
            execution,
            session_log_id,
            cursor_mode,
            self.width,
            self.height,
        )
    }

    pub fn capenc_config_for_size(
        &self,
        binary: PathBuf,
        execution: Option<UserExecution>,
        session_log_id: arcen_telemetry::CorrelationId,
        cursor_mode: arcen_protocol::messages::CursorMode,
        width: u32,
        height: u32,
    ) -> CapencConfig {
        CapencConfig {
            binary,
            output_index: self.output_index(),
            codec: self.codec.clone(),
            encoder: self.encoder,
            fps: self.fps,
            yuv444: self.wire_yuv444(),
            // No client has been heard from yet at this call site (it runs
            // before `server_hello`, let alone `quality_settings`), so the
            // colour policy is resolved against an absent client request —
            // exactly the ceiling/baseline default `ColorPolicy::resolve_*`
            // defines for that case. A later client request that differs is
            // honoured by the `quality_settings` respawn in `net::server`.
            bit_depth: self.color_policy.resolve_bit_depth(self.bit_depth, None),
            color_range: self
                .color_policy
                .resolve_color_range(self.color_range, None),
            color_matrix: self
                .color_policy
                .resolve_color_matrix(self.color_matrix, None),
            video_selection: self.video_selection,
            codec_pinned: self.codec_pinned,
            variant_pinned: self.variant_pinned,
            intent: self.requested_encode_intent(),
            qp_map: self.qp_map,
            width,
            height,
            cursor_mode,
            display: Some(self.display.clone()),
            xauthority: match execution.as_ref() {
                Some(user) => user.environment.get("XAUTHORITY").map(str::to_string),
                None => self.xauthority.clone(),
            },
            execution,
            session_log_id,
        }
    }

    /// Resolve the Deck's auth-time video intent before display mutation or
    /// encoder creation, so the first `ServerHello` describes the final plan.
    pub fn apply_initial_video_request(
        &mut self,
        request: &InitialVideoRequestMsg,
    ) -> Result<(), String> {
        let client = arcen_media::video::resolve_client_video_request(request)
            .map_err(|error| format!("initial video request: {error}"))?;
        let current = arcen_media::VideoConfiguration {
            codec: arcen_media::VideoCodec::from_token(&self.codec)
                .ok_or_else(|| format!("configured codec {:?} is invalid", self.codec))?,
            chroma: arcen_media::ChromaSubsampling::from_token(&self.chroma)
                .ok_or_else(|| format!("configured chroma {:?} is invalid", self.chroma))?,
            bit_depth: self.bit_depth,
            range: self.color_range,
            matrix: self.color_matrix,
            primaries: arcen_media::ColorPrimaries::Bt709,
            transfer: arcen_media::TransferCharacteristics::Bt709,
        };
        let resolved = arcen_media::video::resolve_host_initial_video(
            client,
            arcen_media::video::HostInitialVideoPolicy {
                current,
                color_policy: self.color_policy,
                codec_pinned: self.codec_pinned,
                variant_pinned: self.variant_pinned,
                max_fps: self.fps,
            },
        )
        .map_err(|error| format!("initial video request: {error}"))?;
        self.auth_video_request = Some(request.clone());
        self.fps = resolved.max_fps;
        self.codec = resolved.video.codec.token().to_string();
        self.chroma = resolved.video.chroma.token().to_string();
        self.bit_depth = resolved.video.bit_depth;
        self.color_range = resolved.video.range;
        self.color_matrix = resolved.video.matrix;
        if !self.variant_pinned {
            self.color_policy = ColorPolicy::AlwaysOn;
        }
        self.video_selection = resolved.selection;
        Ok(())
    }

    pub fn resolve_session_agent_binary(&self) -> Option<PathBuf> {
        crate::current_pier_exe()
    }

    pub fn resolve_session_launcher_binary(&self) -> Option<PathBuf> {
        crate::current_pier_exe()
    }

    pub fn resolve_audiocap_binary(&self) -> Option<PathBuf> {
        crate::current_pier_exe()
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            config_path: None,
            host: "127.0.0.1".to_string(),
            port: 18_444,
            tls_cert: None,
            tls_key: None,
            tls_posture: TlsPosture::default(),
            tls_time_policy: CertificateTimePolicy::default(),
            tls_expected_sans: Vec::new(),
            codec: "h264".to_string(),
            encoder: EncoderSelection::Auto,
            chroma: "yuv420".to_string(),
            bit_depth: arcen_media::BitDepth::Eight,
            color_range: arcen_media::ColorRange::Limited,
            color_matrix: arcen_media::ColorMatrix::Bt709,
            color_policy: ColorPolicy::DefaultOff,
            video_selection: VideoSelectionIntent::Exact,
            codec_pinned: false,
            qp_map: arcen_media::video::QpMapPolicy::default(),
            variant_pinned: false,
            auth_video_request: None,
            fps: 60,
            monitor: 1,
            display: ":0".to_string(),
            xauthority: None,
            capenc_bin: None,
            auth_mode: AuthMode::Pam,
            pam_service: "login".to_string(),
            reconnect_window_secs:
                arcen_session::direct_reconnect::DEFAULT_RECONNECT_WINDOW_SECONDS,
            disconnected_idle_lifetime: None,
            timezone_redirection: false,
            zoneinfo_root: PathBuf::from("/usr/share/zoneinfo"),
            disclaimer: None,
            disclaimer_settings: arcen_session::pier_config::DisclaimerConfig::default(),
            session_agent_bin: None,
            session_launcher_bin: None,
            desktop_session: "gnome".to_string(),
            session_display: ":10".to_string(),
            session_gpu_head: "DFP-1".to_string(),
            xorg_bin: PathBuf::from("/usr/libexec/Xorg"),
            xorg_config_template: PathBuf::from("/run/arcen/xorg.conf"),
            session_runtime_root: PathBuf::from("/run/arcen/sessions"),
            input_mode: InputMode::None,
            audio_enabled: false,
            audio_compressed: false,
            audiocap_bin: None,
            audio_user_mode: AudioUserMode::Session,
            microphone_input_enabled: false,
            pactl_bin: PathBuf::from("/usr/bin/pactl"),
            logging: arcen_session::pier_config::LoggingConfig::default(),
            managed_log: None,
            clipboard_policy: ClipboardPolicy::default(),
            deskside: crate::deskside::LinuxDesksideConfig::default(),
            multi_monitor: crate::config::LinuxMultiMonitorConfig::default(),
            unsafe_allow_remote_no_auth: false,
            width: 0,
            height: 0,
            quic_port: None,
            #[cfg(feature = "wss-compat")]
            wss_port: None,
        }
    }
}

/// Parse argv (excluding the verbosity/help flags handled in `main`) into a
/// [`Config`]. Returns a human-readable error string on invalid input.
pub fn parse(args: &[String]) -> Result<Config, String> {
    validate_known_arguments(args)?;
    let config_request = find_config_request(args)?;
    let mut cfg = Config::default();
    if !config_request.disabled {
        if let Some(loaded) = crate::config::load(config_request.path)? {
            cfg.config_path = Some(loaded.path);
            apply_file_config(&mut cfg, loaded.value)?;
        }
    }

    if let Some(v) = flag_value(args, "--host") {
        cfg.host = v;
    }
    if let Some(v) = flag_value(args, "--port") {
        cfg.port = v.parse().map_err(|_| format!("invalid --port: {v}"))?;
    }
    if let Some(v) = flag_value(args, "--tls-cert") {
        cfg.tls_cert = Some(PathBuf::from(v));
    }
    if let Some(v) = flag_value(args, "--tls-key") {
        cfg.tls_key = Some(PathBuf::from(v));
    }
    let minimum_version = optional_flag_value(args, "--tls-minimum-version")?
        .map(|value| {
            value
                .parse::<TlsVersionFloor>()
                .map_err(|_| format!("--tls-minimum-version must be {TLS_VERSION_REQUIREMENT}"))
        })
        .transpose()?
        .unwrap_or_else(|| cfg.tls_posture.version_floor());
    let disabled_overrides = flag_values(args, "--tls-disabled-cipher-suite")?;
    let disabled_suites = if disabled_overrides.is_empty() {
        RingCipherSuite::ALL
            .iter()
            .copied()
            .filter(|suite| !cfg.tls_posture.enabled_suites().contains(suite))
            .collect()
    } else {
        disabled_overrides
            .into_iter()
            .map(|value| {
                value
                    .parse::<RingCipherSuite>()
                    .map_err(|_| format!("unsupported --tls-disabled-cipher-suite: {value}"))
            })
            .collect::<Result<Vec<_>, _>>()?
    };
    cfg.tls_posture = TlsPosture::new(minimum_version, disabled_suites)
        .map_err(|_| "TLS version/cipher posture leaves no usable cipher suite".to_string())?;
    let expiry_warning_days = optional_flag_value(args, "--tls-expiry-warning-days")?
        .map(|value| {
            value
                .parse::<u64>()
                .map_err(|_| format!("invalid --tls-expiry-warning-days: {value}"))
        })
        .transpose()?
        .unwrap_or(cfg.tls_time_policy.warning_window_secs / (24 * 60 * 60));
    if expiry_warning_days > 3650 {
        return Err("--tls-expiry-warning-days must be between 0 and 3650".to_string());
    }
    cfg.tls_time_policy = CertificateTimePolicy {
        warning_window_secs: expiry_warning_days
            .checked_mul(24 * 60 * 60)
            .ok_or_else(|| "--tls-expiry-warning-days overflows".to_string())?,
    };
    let expected_sans = flag_values(args, "--tls-expected-san")?;
    if !expected_sans.is_empty() {
        cfg.tls_expected_sans = expected_sans;
    }
    validate_expected_sans(&cfg.tls_expected_sans)?;
    if let Some(v) = flag_value(args, "--codec") {
        cfg.codec = normalize_codec(&v)?;
        cfg.codec_pinned = true;
    }
    if let Some(v) = flag_value(args, "--encoder") {
        cfg.encoder = parse_encoder_selection(&v)?;
    }
    if let Some(v) = flag_value(args, "--chroma") {
        cfg.chroma = normalize_chroma(&v)?;
    }
    if let Some(v) = flag_value(args, "--bit-depth") {
        cfg.bit_depth = normalize_bit_depth(&v)?;
    }
    if let Some(v) = flag_value(args, "--color-range") {
        cfg.color_range = normalize_color_range(&v)?;
    }
    if let Some(v) = flag_value(args, "--color-matrix") {
        cfg.color_matrix = normalize_color_matrix(&v)?;
    }
    if let Some(v) = flag_value(args, "--color-policy") {
        cfg.color_policy = parse_color_policy(&v)?;
    }
    if let Some(v) = flag_value(args, "--qp-map") {
        cfg.qp_map = arcen_media::video::QpMapPolicy::from_token(&v.to_ascii_lowercase())
            .ok_or_else(|| {
                let known = arcen_media::video::QpMapPolicy::ALL
                    .iter()
                    .map(|policy| policy.token())
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("invalid --qp-map {v:?}: expected one of {known}")
            })?;
    }
    if let Some(v) = flag_value(args, "--variant") {
        apply_variant(&mut cfg, &v)?;
    }
    if let Some(v) = flag_value(args, "--fps") {
        cfg.fps = v.parse().map_err(|_| format!("invalid --fps: {v}"))?;
    }
    if let Some(v) = flag_value(args, "--monitor") {
        cfg.monitor = v.parse().map_err(|_| format!("invalid --monitor: {v}"))?;
    }
    if let Some(v) = flag_value(args, "--display") {
        cfg.display = v;
    }
    if let Some(v) = flag_value(args, "--xauthority") {
        cfg.xauthority = Some(v);
    }
    if let Some(v) = flag_value(args, "--capenc-bin") {
        warn_ignored_helper_path("--capenc-bin");
        cfg.capenc_bin = Some(PathBuf::from(v));
    }
    if let Some(v) = flag_value(args, "--width") {
        cfg.width = v.parse().map_err(|_| format!("invalid --width: {v}"))?;
    }
    if let Some(v) = flag_value(args, "--height") {
        cfg.height = v.parse().map_err(|_| format!("invalid --height: {v}"))?;
    }
    if let Some(v) = flag_value(args, "--quic-port") {
        cfg.quic_port = Some(v.parse().map_err(|_| format!("invalid --quic-port: {v}"))?);
    }
    #[cfg(feature = "wss-compat")]
    if let Some(v) = flag_value(args, "--wss-port") {
        cfg.wss_port = Some(v.parse().map_err(|_| format!("invalid --wss-port: {v}"))?);
    }

    if args.iter().any(|a| a == "--no-auth") {
        if cfg!(feature = "insecure-lab-no-auth") {
            cfg.auth_mode = AuthMode::None;
        } else {
            return Err(NO_AUTH_REFUSED.to_string());
        }
    }
    if args.iter().any(|a| a == "--unsafe-allow-remote-no-auth") {
        if cfg!(feature = "insecure-lab-no-auth") {
            cfg.unsafe_allow_remote_no_auth = true;
        } else {
            return Err(NO_AUTH_REFUSED.to_string());
        }
    }
    if let Some(v) = flag_value(args, "--auth-mode") {
        cfg.auth_mode = parse_auth_mode(&v)?;
    }
    if let Some(v) = flag_value(args, "--pam-service") {
        validate_pam_service(&v)?;
        cfg.pam_service = v;
    }
    if let Some(v) = flag_value_last(args, "--reconnect-window-secs") {
        let seconds = v
            .parse::<u32>()
            .map_err(|_| format!("invalid --reconnect-window-secs: {v}"))?;
        cfg.reconnect_window_secs = arcen_session::direct_reconnect::ReconnectPolicy::new(seconds)
            .map_err(|error| format!("invalid --reconnect-window-secs: {error}"))?
            .window_secs();
    }
    if let Some(enabled) =
        last_boolean_override(args, "--timezone-redirection", "--no-timezone-redirection")
    {
        cfg.timezone_redirection = enabled;
    }
    if let Some(v) = flag_value_last(args, "--zoneinfo-root") {
        cfg.zoneinfo_root = PathBuf::from(v);
    }
    let disclaimer_override = last_boolean_override(args, "--disclaimer", "--no-disclaimer");
    let disclaimer_directory = flag_value(args, "--disclaimer-dir");
    let disclaimer_locale = flag_value(args, "--disclaimer-locale");
    if let Some(enabled) = disclaimer_override {
        cfg.disclaimer_settings.enabled = enabled;
    }
    if let Some(directory) = disclaimer_directory {
        cfg.disclaimer_settings.directory = Some(directory);
    }
    if let Some(locale) = disclaimer_locale {
        cfg.disclaimer_settings.locale = Some(locale);
    }
    if !cfg.disclaimer_settings.enabled
        && (cfg.disclaimer_settings.directory.is_some() || cfg.disclaimer_settings.locale.is_some())
        && disclaimer_override == Some(false)
    {
        cfg.disclaimer_settings.directory = None;
        cfg.disclaimer_settings.locale = None;
    } else if !cfg.disclaimer_settings.enabled
        && (flag_value(args, "--disclaimer-dir").is_some()
            || flag_value(args, "--disclaimer-locale").is_some())
    {
        return Err("--disclaimer-dir and --disclaimer-locale require --disclaimer".to_string());
    }
    if let Some(v) = flag_value(args, "--session-agent-bin") {
        warn_ignored_helper_path("--session-agent-bin");
        cfg.session_agent_bin = Some(PathBuf::from(v));
    }
    if let Some(v) = flag_value(args, "--session-launcher-bin") {
        warn_ignored_helper_path("--session-launcher-bin");
        cfg.session_launcher_bin = Some(PathBuf::from(v));
    }
    if let Some(v) = flag_value(args, "--desktop-session") {
        cfg.desktop_session = parse_desktop_session(&v)?;
    }
    if let Some(v) = flag_value(args, "--session-display") {
        validate_session_display(&v)?;
        cfg.session_display = v;
    }
    if let Some(v) = flag_value(args, "--session-gpu-head") {
        validate_gpu_head(&v)?;
        cfg.session_gpu_head = v;
    }
    if let Some(v) = flag_value(args, "--xorg-bin") {
        cfg.xorg_bin = PathBuf::from(v);
    }
    if let Some(v) = flag_value(args, "--xorg-config-template") {
        cfg.xorg_config_template = PathBuf::from(v);
    }
    if let Some(v) = flag_value(args, "--session-runtime-root") {
        cfg.session_runtime_root = PathBuf::from(v);
    }
    if let Some(enabled) = last_boolean_override(args, "--deskside", "--no-deskside") {
        cfg.deskside.enabled = enabled;
    }
    if let Some(value) = optional_flag_value(args, "--deskside-firmware-sha256")? {
        cfg.deskside.firmware_sha256 = value;
    }
    if let Some(value) = optional_flag_value(args, "--deskside-console-uid")? {
        cfg.deskside.console_uid = Some(
            value
                .parse::<u32>()
                .map_err(|_| "--deskside-console-uid must be a numeric UID".to_string())?,
        );
    }
    if let Some(value) = optional_flag_value(args, "--deskside-console-display")? {
        cfg.deskside.console_display = Some(value);
    }
    if let Some(value) = optional_flag_value(args, "--deskside-console-xauthority")? {
        cfg.deskside.console_xauthority = Some(PathBuf::from(value));
    }
    let input_devices = flag_values(args, "--deskside-input")?;
    if !input_devices.is_empty() {
        cfg.deskside.input_devices = input_devices.into_iter().map(PathBuf::from).collect();
    }
    let outputs = flag_values(args, "--deskside-output")?;
    if !outputs.is_empty() {
        cfg.deskside.outputs = outputs
            .into_iter()
            .map(|value| crate::deskside::PhysicalOutputPin::parse(&value))
            .collect::<Result<Vec<_>, _>>()?;
    }
    if let Some(enabled) = last_boolean_override(args, "--multi-monitor", "--no-multi-monitor") {
        cfg.multi_monitor.advertise_enabled = enabled;
    }
    let multi_monitor_heads = flag_values(args, "--multi-monitor-head")?;
    if !multi_monitor_heads.is_empty() {
        for head in &multi_monitor_heads {
            validate_gpu_head(head).map_err(|_| {
                format!("--multi-monitor-head must be DFP-0, DFP-1, DFP-2, or DFP-3, got {head}")
            })?;
        }
        cfg.multi_monitor.heads = multi_monitor_heads;
    }
    if let Some(v) = flag_value(args, "--input-mode") {
        cfg.input_mode = parse_input_mode(&v)?;
    }
    if let Some(enabled) = last_boolean_override(args, "--audio", "--no-audio") {
        cfg.audio_enabled = enabled;
    }
    if let Some(compressed) =
        last_boolean_override(args, "--audio-compressed", "--audio-uncompressed")
    {
        cfg.audio_compressed = compressed;
    }
    if let Some(v) = flag_value(args, "--audiocap-bin") {
        warn_ignored_helper_path("--audiocap-bin");
        cfg.audiocap_bin = Some(PathBuf::from(v));
    }
    if let Some(v) = flag_value(args, "--audio-user") {
        cfg.audio_user_mode = parse_audio_user_mode(&v)?;
    }
    if let Some(enabled) =
        last_boolean_override(args, "--microphone-input", "--no-microphone-input")
    {
        cfg.microphone_input_enabled = enabled;
    }
    if let Some(value) = flag_value(args, "--pactl-bin") {
        cfg.pactl_bin = PathBuf::from(value);
    }
    if let Some(value) = flag_value_last(args, "--clipboard-direction") {
        cfg.clipboard_policy.direction = parse_clipboard_direction(&value)?;
    }
    if let Some(value) = flag_value_last(args, "--clipboard-content") {
        cfg.clipboard_policy.content = parse_clipboard_content(&value)?;
    }
    if let Some(value) = flag_value_last(args, "--clipboard-max-bytes") {
        let maximum = value
            .parse::<usize>()
            .map_err(|_| format!("invalid --clipboard-max-bytes: {value}"))?;
        if !(1024 * 1024..=HARD_MAX_CLIPBOARD_BYTES).contains(&maximum) {
            return Err("--clipboard-max-bytes must be from 1 MiB through 20 MiB".to_string());
        }
        cfg.clipboard_policy = ClipboardPolicy::new(
            cfg.clipboard_policy.direction,
            cfg.clipboard_policy.content,
            maximum,
        )
        .map_err(|error| format!("invalid clipboard policy: {error}"))?;
    }
    if args.iter().any(|argument| argument == "--no-clipboard") {
        cfg.clipboard_policy.direction = ClipboardDirection::Disabled;
    }

    cfg.disclaimer = if cfg.disclaimer_settings.enabled {
        Some(load_disclaimer(
            cfg.auth_mode,
            cfg.disclaimer_settings.locale.clone(),
            cfg.disclaimer_settings.directory.clone(),
        )?)
    } else {
        None
    };

    // SEC-001. Authentication is the product's only trust boundary, so its
    // absence must deny rather than permit. A release build cannot reach
    // `AuthMode::None` at all: the parser rejects it above. This guard is the
    // second line of defence for lab builds, and it refuses every bind rather
    // than only non-loopback ones, because loopback is not a trust boundary on
    // a multi-user host: any local user could otherwise watch the console
    // user's screen without authenticating.
    if cfg.auth_mode == AuthMode::None && !cfg!(feature = "insecure-lab-no-auth") {
        return Err(NO_AUTH_REFUSED.to_string());
    }
    if cfg.auth_mode == AuthMode::None && !cfg.unsafe_allow_remote_no_auth {
        return Err(format!(
            "refusing unauthenticated bind {}: an isolated-lab build must also pass \
             --unsafe-allow-remote-no-auth to serve any client, loopback included",
            cfg.host
        ));
    }
    if cfg.input_mode == InputMode::Uinput && cfg.auth_mode != AuthMode::Pam {
        return Err("--input-mode uinput requires --auth-mode pam".to_string());
    }
    cfg.deskside.validate(
        cfg.auth_mode == AuthMode::Pam,
        cfg.input_mode == InputMode::Uinput,
        &cfg.session_display,
        &cfg.session_gpu_head,
    )?;
    if cfg.tls_cert.is_some() != cfg.tls_key.is_some() {
        return Err("--tls-cert and --tls-key must be provided together".to_string());
    }
    if cfg.wire_yuv444() && cfg.codec != "h265" {
        return Err(format!(
            "yuv444 requires --codec h265; configured codec {} supports only yuv420 on this host",
            cfg.codec
        ));
    }
    if cfg.bit_depth == arcen_media::BitDepth::Twelve
        && cfg.encoder == EncoderSelection::NativeNvenc
    {
        return Err(
            "video.bit_depth 12 cannot work with --encoder nvenc: NVENC has no 12-bit mode at \
             any subsampling; use the software tier for 12-bit"
                .to_string(),
        );
    }
    if !(1..=240).contains(&cfg.fps) {
        return Err("--fps must be between 1 and 240".to_string());
    }
    if cfg.encoder == EncoderSelection::SoftwareH264 {
        if (cfg.codec_pinned || cfg.variant_pinned) && !cfg.exact_pins_allow_software_h264() {
            return Err(format!(
                "exact administrator video pin {} {} {}-bit @ {} fps is incompatible with software-h264",
                cfg.codec,
                if cfg.wire_yuv444() {
                    "yuv444"
                } else {
                    "yuv420"
                },
                cfg.bit_depth.token(),
                cfg.fps,
            ));
        }
        if cfg.codec != "h264" || cfg.wire_yuv444() {
            eprintln!(
                "warning: software-h264 cannot encode {} {}; the session will be served as h264 yuv420 and the applied plan is logged at session start",
                cfg.codec,
                if cfg.wire_yuv444() {
                    "yuv444"
                } else {
                    "yuv420"
                }
            );
        }
    }
    if cfg.direct_quic_port() == 0 {
        return Err("direct QUIC requires a nonzero UDP port".to_string());
    }
    if !cfg.tls_enabled() {
        return Err("direct QUIC requires --tls-cert and --tls-key".to_string());
    }
    #[cfg(feature = "wss-compat")]
    if cfg.wss_port == Some(cfg.direct_quic_port()) {
        return Err("dormant WSS compatibility cannot share the QUIC UDP port".to_string());
    }
    if !cfg.multi_monitor.heads.is_empty() {
        // Every individual head token is already validated above
        // (`validate_gpu_head`, both the `--multi-monitor-head` and
        // `platform.multi_monitor.heads` paths); building the full
        // `HeadInventory` here additionally rejects a duplicated head
        // immediately at config-load/CLI-parse time (`validate-config`
        // fails, startup refuses), rather than deferring to
        // `net::server::multi_monitor_gate`'s runtime construction, which
        // otherwise silently downgrades to a fully disabled gate on any
        // `HeadInventory` error.
        let heads = cfg
            .multi_monitor
            .heads
            .iter()
            .cloned()
            .map(HeadCapability::new)
            .collect();
        HeadInventory::new(heads)
            .map_err(|error| format!("platform.multi_monitor.heads is invalid: {error}"))?;
    }
    if cfg.multi_monitor.advertise_enabled
        && matches!(
            cfg.encoder,
            EncoderSelection::Auto | EncoderSelection::WindowsMediaFoundation
        )
    {
        return Err(format!(
            "platform.multi_monitor.advertise_enabled requires an explicit encoder pin \
             (nvenc or software-h264); {} is not permitted for a multi-monitor session, because \
             its own per-attempt fallback could silently select a different backend/geometry per \
             monitor after the Xorg multi-head commit",
            cfg.encoder.as_arg()
        ));
    }
    if cfg.multi_monitor.nvenc_session_limit == Some(0) {
        return Err("platform.multi_monitor.nvenc_session_limit must be at least 1".to_string());
    }

    Ok(cfg)
}

struct ConfigRequest {
    path: Option<PathBuf>,
    disabled: bool,
}

fn validate_known_arguments(args: &[String]) -> Result<(), String> {
    const VALUE_FLAGS: &[&str] = &[
        "--config",
        "--host",
        "--port",
        "--tls-cert",
        "--tls-key",
        "--tls-minimum-version",
        "--tls-disabled-cipher-suite",
        "--tls-expiry-warning-days",
        "--tls-expected-san",
        "--codec",
        "--encoder",
        "--chroma",
        "--bit-depth",
        "--color-range",
        "--color-matrix",
        "--color-policy",
        "--variant",
        "--fps",
        "--monitor",
        "--display",
        "--xauthority",
        "--capenc-bin",
        "--width",
        "--height",
        "--quic-port",
        #[cfg(feature = "wss-compat")]
        "--wss-port",
        "--auth-mode",
        "--pam-service",
        "--reconnect-window-secs",
        "--zoneinfo-root",
        "--disclaimer-dir",
        "--disclaimer-locale",
        "--session-agent-bin",
        "--session-launcher-bin",
        "--desktop-session",
        "--session-display",
        "--session-gpu-head",
        "--xorg-bin",
        "--xorg-config-template",
        "--session-runtime-root",
        "--deskside-firmware-sha256",
        "--deskside-console-uid",
        "--deskside-console-display",
        "--deskside-console-xauthority",
        "--deskside-input",
        "--deskside-output",
        "--multi-monitor-head",
        "--input-mode",
        "--audiocap-bin",
        "--audio-user",
        "--pactl-bin",
        "--clipboard-direction",
        "--clipboard-content",
        "--clipboard-max-bytes",
        "--managed-log",
        "--verbosity",
    ];
    const BOOLEAN_FLAGS: &[&str] = &[
        "--no-config",
        "--no-auth",
        "--unsafe-allow-remote-no-auth",
        "--timezone-redirection",
        "--no-timezone-redirection",
        "--disclaimer",
        "--no-disclaimer",
        "--deskside",
        "--no-deskside",
        "--multi-monitor",
        "--no-multi-monitor",
        "--audio",
        "--no-audio",
        "--audio-compressed",
        "--audio-uncompressed",
        "--microphone-input",
        "--no-microphone-input",
        "--no-clipboard",
        "--version",
        "-V",
        "--help",
        "-h",
        "--quiet",
        "-q",
        "--verbose",
        "-v",
        "--trace",
        "-vv",
    ];
    let mut index = 0;
    while index < args.len() {
        let argument = &args[index];
        if VALUE_FLAGS.contains(&argument.as_str()) {
            let Some(value) = args.get(index + 1) else {
                return Err(format!("{argument} requires a value"));
            };
            if value.starts_with('-') {
                return Err(format!("{argument} requires a value"));
            }
            index += 2;
            continue;
        }
        if !argument.starts_with('-') {
            return Err(format!("unexpected positional argument: {argument}"));
        }
        if !BOOLEAN_FLAGS.contains(&argument.as_str()) {
            return Err(format!("unknown option: {argument}"));
        }
        index += 1;
    }
    Ok(())
}

fn find_config_request(args: &[String]) -> Result<ConfigRequest, String> {
    let paths = flag_values(args, "--config")?;
    if paths.len() > 1 {
        return Err("--config may be supplied only once".to_string());
    }
    let disabled_count = args
        .iter()
        .filter(|argument| argument.as_str() == "--no-config")
        .count();
    if disabled_count > 1 {
        return Err("--no-config may be supplied only once".to_string());
    }
    if !paths.is_empty() && disabled_count != 0 {
        return Err("--config and --no-config are mutually exclusive".to_string());
    }
    Ok(ConfigRequest {
        path: paths.into_iter().next().map(PathBuf::from),
        disabled: disabled_count != 0,
    })
}

fn apply_file_config(cfg: &mut Config, file: crate::config::PierFileConfig) -> Result<(), String> {
    if let Some(value) = file.listen.host {
        cfg.host = value;
    }
    if let Some(value) = file.listen.port {
        cfg.port = value;
    }
    if let Some(value) = file.listen.quic_port {
        cfg.quic_port = Some(value);
    }
    if file
        .tls
        .mode
        .as_deref()
        .is_some_and(|mode| !mode.eq_ignore_ascii_case("pem"))
    {
        return Err("Pier config tls.mode supports only \"pem\"".to_string());
    }
    cfg.tls_cert = file.tls.cert.map(PathBuf::from);
    cfg.tls_key = file.tls.key.map(PathBuf::from);
    let minimum_version = file
        .tls
        .minimum_version
        .as_deref()
        .map(str::parse::<TlsVersionFloor>)
        .transpose()
        .map_err(|_| format!("Pier config tls.minimum_version must be {TLS_VERSION_REQUIREMENT}"))?
        .unwrap_or_default();
    let disabled_suites = file
        .tls
        .disabled_cipher_suites
        .into_iter()
        .map(|value| {
            value
                .parse::<RingCipherSuite>()
                .map_err(|_| format!("Pier config tls.disabled_cipher_suites contains {value:?}"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    cfg.tls_posture = TlsPosture::new(minimum_version, disabled_suites)
        .map_err(|_| "Pier config TLS posture leaves no usable cipher suite".to_string())?;
    let expiry_warning_days = file.tls.expiry_warning_days.unwrap_or(30);
    if expiry_warning_days > 3_650 {
        return Err("Pier config tls.expiry_warning_days must be between 0 and 3650".to_string());
    }
    cfg.tls_time_policy = CertificateTimePolicy {
        warning_window_secs: expiry_warning_days
            .checked_mul(24 * 60 * 60)
            .ok_or_else(|| "Pier config tls.expiry_warning_days overflows".to_string())?,
    };
    cfg.tls_expected_sans = file.tls.expected_sans;
    validate_expected_sans(&cfg.tls_expected_sans)?;

    if file.capture.binary.is_some() {
        warn_ignored_helper_path("capture.binary");
    }
    cfg.capenc_bin = file.capture.binary.map(PathBuf::from);
    if let Some(value) = file.video.codec {
        cfg.codec = normalize_codec(&value)?;
        cfg.codec_pinned = true;
    }
    if let Some(value) = file.video.encoder {
        cfg.encoder = parse_encoder_selection(&value)?;
    }
    if let Some(value) = file.video.chroma {
        cfg.chroma = normalize_chroma(&value)?;
    }
    if let Some(value) = file.video.bit_depth {
        cfg.bit_depth = normalize_bit_depth(&value)
            .map_err(|error| format!("Pier config video.bit_depth: {error}"))?;
    }
    if let Some(value) = file.video.color_range {
        cfg.color_range = normalize_color_range(&value)
            .map_err(|error| format!("Pier config video.color_range: {error}"))?;
    }
    if let Some(value) = file.video.color_matrix {
        cfg.color_matrix = normalize_color_matrix(&value)
            .map_err(|error| format!("Pier config video.color_matrix: {error}"))?;
    }
    if let Some(value) = file.video.color_policy {
        cfg.color_policy = parse_color_policy(&value)
            .map_err(|error| format!("Pier config video.color_policy: {error}"))?;
    }
    if let Some(value) = file.video.qp_map {
        cfg.qp_map = arcen_media::video::QpMapPolicy::from_token(&value.to_ascii_lowercase())
            .ok_or_else(|| {
                let known = arcen_media::video::QpMapPolicy::ALL
                    .iter()
                    .map(|policy| policy.token())
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("Pier config video.qp_map {value:?}: expected one of {known}")
            })?;
    }
    if let Some(value) = file.video.variant {
        apply_variant(cfg, &value)
            .map_err(|error| format!("Pier config video.variant: {error}"))?;
    }
    if let Some(value) = file.video.fps {
        cfg.fps = value;
    }
    cfg.audio_enabled = file.audio.enabled;
    cfg.audio_compressed = file.audio.compressed;
    cfg.microphone_input_enabled = file.microphone_input.enabled;

    if let Some(value) = file.clipboard.direction {
        cfg.clipboard_policy.direction = parse_clipboard_direction(&value)?;
    }
    if let Some(value) = file.clipboard.content {
        cfg.clipboard_policy.content = parse_clipboard_content(&value)?;
    }
    if let Some(value) = file.clipboard.max_bytes {
        cfg.clipboard_policy = ClipboardPolicy::new(
            cfg.clipboard_policy.direction,
            cfg.clipboard_policy.content,
            value,
        )
        .map_err(|error| format!("Pier config clipboard: {error}"))?;
    }
    if let Some(value) = file.auth.reconnect_window_secs {
        cfg.reconnect_window_secs = arcen_session::direct_reconnect::ReconnectPolicy::new(value)
            .map_err(|error| format!("Pier config auth.reconnect_window_secs: {error}"))?
            .window_secs();
    }
    if let Some(value) = file.redirection.timezone {
        cfg.timezone_redirection = value;
    }
    // `LoggingConfig::deserialize` already validated the canonical/legacy
    // fields; `resolved_profile` is called here only as defense-in-depth so
    // a manually constructed conflict fails fast with a clear message.
    file.logging
        .resolved_profile()
        .map_err(|error| format!("Pier config logging: {error}"))?;
    cfg.logging = file.logging;

    let platform = file.platform;
    if let Some(value) = platform.auth.mode {
        cfg.auth_mode = parse_auth_mode(&value)?;
    }
    if let Some(value) = platform.auth.pam_service {
        validate_pam_service(&value)?;
        cfg.pam_service = value;
    }
    if let Some(value) = platform.auth.unsafe_allow_remote_no_auth {
        // SEC-001. The key survives in the schema so an existing pier.json still
        // parses under `deny_unknown_fields`, but a release build refuses to
        // honour it. Silently ignoring it would leave an operator believing the
        // host is reachable unauthenticated when it is not, or the reverse.
        if value && !cfg!(feature = "insecure-lab-no-auth") {
            return Err(NO_AUTH_REFUSED.to_string());
        }
        cfg.unsafe_allow_remote_no_auth = value;
    }
    if let Some(value) = platform.capture.monitor {
        cfg.monitor = value;
    }
    if let Some(value) = platform.capture.display {
        cfg.display = value;
    }
    cfg.xauthority = platform.capture.xauthority;
    if let Some(value) = platform.capture.width {
        cfg.width = value;
    }
    if let Some(value) = platform.capture.height {
        cfg.height = value;
    }
    if let Some(value) = platform.session.desktop {
        cfg.desktop_session = parse_desktop_session(&value)?;
    }
    if let Some(value) = platform.session.display {
        validate_session_display(&value)?;
        cfg.session_display = value;
    }
    if let Some(value) = platform.session.gpu_head {
        validate_gpu_head(&value)?;
        cfg.session_gpu_head = value;
    }
    if let Some(value) = platform.session.xorg_bin {
        cfg.xorg_bin = PathBuf::from(value);
    }
    if let Some(value) = platform.session.xorg_config_template {
        cfg.xorg_config_template = PathBuf::from(value);
    }
    if let Some(value) = platform.session.runtime_root {
        cfg.session_runtime_root = PathBuf::from(value);
    }
    if platform.session.agent_bin.is_some() {
        warn_ignored_helper_path("platform.session.agent_bin");
    }
    cfg.session_agent_bin = platform.session.agent_bin.map(PathBuf::from);
    if platform.session.launcher_bin.is_some() {
        warn_ignored_helper_path("platform.session.launcher_bin");
    }
    cfg.session_launcher_bin = platform.session.launcher_bin.map(PathBuf::from);
    if let Some(value) = platform.session.zoneinfo_root {
        cfg.zoneinfo_root = PathBuf::from(value);
    }
    if let Some(value) = platform.session.disconnected_idle_timeout_secs {
        cfg.disconnected_idle_lifetime = Some(validate_disconnected_idle_timeout(value)?);
    }
    if let Some(value) = platform.input.mode {
        cfg.input_mode = parse_input_mode(&value)?;
    }
    if platform.audio.capture_binary.is_some() {
        warn_ignored_helper_path("platform.audio.capture_binary");
    }
    cfg.audiocap_bin = platform.audio.capture_binary.map(PathBuf::from);
    if let Some(value) = platform.audio.user {
        cfg.audio_user_mode = parse_audio_user_mode(&value)?;
    }
    if let Some(value) = platform.audio.pactl_binary {
        cfg.pactl_bin = PathBuf::from(value);
    }
    cfg.managed_log = platform.logging.managed_log.map(PathBuf::from);
    cfg.deskside = platform.deskside;
    for head in &platform.multi_monitor.heads {
        validate_gpu_head(head).map_err(|_| {
            format!(
                "platform.multi_monitor.heads must be DFP-0, DFP-1, DFP-2, or DFP-3, got {head}"
            )
        })?;
    }
    cfg.multi_monitor = platform.multi_monitor;

    cfg.disclaimer_settings = file.auth.disclaimer;
    Ok(())
}

fn validate_disconnected_idle_timeout(seconds: u64) -> Result<Duration, String> {
    if seconds == 0 {
        return Err(
            "Pier config platform.session.disconnected_idle_timeout_secs must be greater than 0; omit the key to disable"
                .to_string(),
        );
    }
    Ok(Duration::from_secs(seconds))
}

fn read_bounded_disclaimer(path: &std::path::Path) -> Result<Vec<u8>, String> {
    use std::io::Read;

    let file = std::fs::File::open(path)
        .map_err(|error| format!("read disclaimer {}: {error}", path.display()))?;
    let mut bytes = Vec::with_capacity(arcen_identity::MAX_DISCLAIMER_CONTENT_BYTES + 1);
    file.take((arcen_identity::MAX_DISCLAIMER_CONTENT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("read disclaimer {}: {error}", path.display()))?;
    Ok(bytes)
}

fn is_loopback_bind(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    host.parse::<IpAddr>()
        .map(|address| address.is_loopback())
        .unwrap_or(false)
}

fn validate_session_display(display: &str) -> Result<(), String> {
    display
        .strip_prefix(':')
        .and_then(|value| value.parse::<u16>().ok())
        .filter(|number| (1..=99).contains(number))
        .map(|_| ())
        .ok_or_else(|| "--session-display must be :1 through :99".to_string())
}

fn validate_gpu_head(head: &str) -> Result<(), String> {
    match head {
        "DFP-0" | "DFP-1" | "DFP-2" | "DFP-3" => Ok(()),
        _ => Err("--session-gpu-head must be DFP-0, DFP-1, DFP-2, or DFP-3".to_string()),
    }
}

fn parse_auth_mode(value: &str) -> Result<AuthMode, String> {
    match value {
        "none" if cfg!(feature = "insecure-lab-no-auth") => Ok(AuthMode::None),
        "none" => Err(NO_AUTH_REFUSED.to_string()),
        "pam" => Ok(AuthMode::Pam),
        other if cfg!(feature = "insecure-lab-no-auth") => {
            Err(format!("invalid auth mode: {other} (expected none|pam)"))
        }
        other => Err(format!("invalid auth mode: {other} (expected pam)")),
    }
}

fn validate_pam_service(value: &str) -> Result<(), String> {
    if value.is_empty()
        || !value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        Err("PAM service must contain only ASCII letters, digits, '-' or '_'".to_string())
    } else {
        Ok(())
    }
}

fn parse_desktop_session(value: &str) -> Result<String, String> {
    match value {
        "gnome" | "gnome-classic" => Ok(value.to_string()),
        other => Err(format!(
            "invalid desktop session: {other} (expected gnome|gnome-classic)"
        )),
    }
}

fn parse_input_mode(value: &str) -> Result<InputMode, String> {
    match value {
        "none" => Ok(InputMode::None),
        "uinput" => Ok(InputMode::Uinput),
        other => Err(format!(
            "invalid input mode: {other} (expected none|uinput)"
        )),
    }
}

fn parse_audio_user_mode(value: &str) -> Result<AudioUserMode, String> {
    match value {
        "session" => Ok(AudioUserMode::Session),
        "host" => Ok(AudioUserMode::Host),
        other => Err(format!(
            "invalid audio user: {other} (expected session|host)"
        )),
    }
}

fn parse_clipboard_direction(value: &str) -> Result<ClipboardDirection, String> {
    match value {
        "both" => Ok(ClipboardDirection::Both),
        "client_to_host" => Ok(ClipboardDirection::ClientToHost),
        "host_to_client" => Ok(ClipboardDirection::HostToClient),
        "disabled" => Ok(ClipboardDirection::Disabled),
        _ => Err(
            "clipboard direction must be both, client_to_host, host_to_client, or disabled"
                .to_string(),
        ),
    }
}

fn parse_clipboard_content(value: &str) -> Result<ClipboardContent, String> {
    match value {
        "all" => Ok(ClipboardContent::All),
        "text" => Ok(ClipboardContent::Text),
        "image" => Ok(ClipboardContent::Image),
        _ => Err("clipboard content must be all, text, or image".to_string()),
    }
}

fn load_disclaimer(
    auth_mode: AuthMode,
    locale: Option<String>,
    directory: Option<String>,
) -> Result<Arc<arcen_identity::PreparedDisclaimer>, String> {
    if auth_mode != AuthMode::Pam {
        return Err("disclaimer requires PAM authentication".to_string());
    }
    let locale =
        arcen_identity::DisclaimerLocale::new(locale.unwrap_or_else(|| "en_US".to_string()))
            .map_err(|error| format!("invalid disclaimer locale: {error}"))?;
    let directory = directory
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/etc/arcen/disclaimers"));
    let path = directory.join(format!("{}.txt", locale.as_str()));
    let bytes = read_bounded_disclaimer(&path)?;
    Ok(Arc::new(
        arcen_identity::PreparedDisclaimer::from_bytes(locale, &bytes)
            .map_err(|error| format!("validate disclaimer {}: {error}", path.display()))?,
    ))
}

fn normalize_codec(v: &str) -> Result<String, String> {
    match v.to_ascii_lowercase().as_str() {
        "h264" => Ok("h264".to_string()),
        "h265" | "hevc" => Ok("h265".to_string()),
        "av1" => Ok("av1".to_string()),
        other => Err(format!(
            "unsupported --codec: {other} (expected h264|h265|av1)"
        )),
    }
}

fn normalize_chroma(v: &str) -> Result<String, String> {
    match v.to_ascii_lowercase().as_str() {
        "yuv420" | "420" => Ok("yuv420".to_string()),
        "yuv444" | "444" => Ok("yuv444".to_string()),
        "yuv422" | "422" => Err("unsupported chroma: yuv422 (expected yuv420|yuv444)".to_string()),
        other => Err(format!(
            "unsupported --chroma: {other} (expected yuv420|yuv444)"
        )),
    }
}

fn normalize_bit_depth(v: &str) -> Result<arcen_media::BitDepth, String> {
    arcen_media::BitDepth::from_token(v)
        .ok_or_else(|| format!("unsupported bit depth {v:?} (expected 8|10|12)"))
}

fn normalize_color_range(v: &str) -> Result<arcen_media::ColorRange, String> {
    arcen_media::ColorRange::from_token(v)
        .ok_or_else(|| format!("unsupported colour range {v:?} (expected limited|full)"))
}

fn normalize_color_matrix(v: &str) -> Result<arcen_media::ColorMatrix, String> {
    arcen_media::ColorMatrix::from_token(v).ok_or_else(|| {
        format!("unsupported colour matrix {v:?} (expected identity|bt709|bt601|bt2020ncl)")
    })
}

fn parse_color_policy(v: &str) -> Result<ColorPolicy, String> {
    ColorPolicy::from_token(v).ok_or_else(|| {
        format!(
            "unsupported colour policy {v:?} (expected always-on|always-off|default-on|default-off)"
        )
    })
}

/// Apply a `video.variant` id, overriding `codec`, `chroma`, `bit_depth`,
/// `color_range` and `color_matrix` together. This is how an operator pins a
/// host to one exact row of the probe matrix (see `arcen_media::video::PROBE_MATRIX`)
/// instead of setting five keys by hand. Precedence: this always wins over
/// whatever the individual keys set, regardless of which was applied first.
fn apply_variant(cfg: &mut Config, id: &str) -> Result<(), String> {
    let variant = arcen_media::video::VideoVariant::from_id(id)
        .map_err(|error| format!("unsupported variant {id:?}: {error}"))?;
    // Resolve every component before mutating `cfg`, so a variant that is
    // coherent (offered by `arcen_media`) but not yet wired into this host's
    // own codec/chroma pipeline (for example a 4:2:2 row) leaves the
    // prior configuration completely untouched instead of half-applied, and
    // fails clearly now rather than confusingly at session start.
    let codec = normalize_codec(variant.video.codec.token())
        .map_err(|error| format!("variant {id:?} selects a codec this host cannot run: {error}"))?;
    let chroma = normalize_chroma(variant.video.chroma.token()).map_err(|error| {
        format!("variant {id:?} selects a chroma this host cannot run: {error}")
    })?;
    cfg.codec = codec;
    cfg.chroma = chroma;
    cfg.bit_depth = variant.video.bit_depth;
    cfg.color_range = variant.video.range;
    cfg.color_matrix = variant.video.matrix;
    // `video.variant` is an exact operator pin, not merely a ceiling. Letting
    // the default-off policy rewrite it before the client quality request
    // made ServerHello report limited range while the later respawn and frame
    // flags correctly carried full range.
    cfg.color_policy = ColorPolicy::AlwaysOn;
    cfg.video_selection = VideoSelectionIntent::Exact;
    cfg.variant_pinned = true;
    Ok(())
}

fn validate_expected_sans(names: &[String]) -> Result<(), String> {
    if names.len() > 64 {
        return Err("--tls-expected-san may be supplied at most 64 times".to_string());
    }
    if names.iter().any(|name| {
        name.is_empty()
            || name.len() > 253
            || !name.is_ascii()
            || name
                .chars()
                .any(|character| character.is_ascii_control() || character.is_ascii_whitespace())
    }) {
        return Err(
            "--tls-expected-san must be a bounded DNS name or IP address without whitespace"
                .to_string(),
        );
    }
    Ok(())
}

fn optional_flag_value(args: &[String], flag: &str) -> Result<Option<String>, String> {
    let values = flag_values(args, flag)?;
    if values.len() > 1 {
        return Err(format!("{flag} may be supplied only once"));
    }
    Ok(values.into_iter().next())
}

fn flag_values(args: &[String], flag: &str) -> Result<Vec<String>, String> {
    let mut values = Vec::new();
    for (index, argument) in args.iter().enumerate() {
        if argument == flag {
            let value = args
                .get(index + 1)
                .filter(|value| !value.starts_with("--"))
                .ok_or_else(|| format!("{flag} requires a value"))?;
            values.push(value.clone());
        }
    }
    Ok(values)
}

/// Return the value following `--flag`, if present.
fn flag_value(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn flag_value_last(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .enumerate()
        .rev()
        .find(|(_, argument)| *argument == flag)
        .and_then(|(index, _)| args.get(index + 1))
        .cloned()
}

fn last_boolean_override(args: &[String], enabled: &str, disabled: &str) -> Option<bool> {
    args.iter().rev().find_map(|argument| {
        if argument == enabled {
            Some(true)
        } else if argument == disabled {
            Some(false)
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[String]) -> Result<Config, String> {
        let mut isolated = Vec::with_capacity(args.len() + 1);
        if !args
            .iter()
            .any(|argument| argument == "--config" || argument == "--no-config")
        {
            isolated.push("--no-config".to_string());
        }
        isolated.extend_from_slice(args);
        super::parse(&isolated)
    }

    fn parse_ok(args: &[&str]) -> Config {
        let mut arguments = args.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        if !arguments.iter().any(|argument| argument == "--tls-cert") {
            arguments.extend([
                "--tls-cert".to_string(),
                "/tmp/arcen-test.crt".to_string(),
                "--tls-key".to_string(),
                "/tmp/arcen-test.key".to_string(),
            ]);
        }
        parse(&arguments).unwrap()
    }

    fn initial_video(
        selection: VideoSelectionIntent,
        codec: &str,
        chroma: &str,
        bit_depth: &str,
        range: &str,
    ) -> InitialVideoRequestMsg {
        InitialVideoRequestMsg {
            quality: arcen_protocol::messages::QualitySettings {
                codec: codec.to_string(),
                chroma: chroma.to_string(),
                bit_depth: bit_depth.to_string(),
                color_range: range.to_string(),
                color_matrix: "bt709".to_string(),
                max_fps: 60,
                video_selection: selection,
                ..arcen_protocol::messages::QualitySettings::default()
            },
            capabilities: arcen_protocol::messages::ClientVideoCapabilitiesMsg {
                h264: true,
                h265: true,
                av1: true,
                yuv444: true,
                main10: true,
                main12: false,
                full_range: true,
                identity_matrix: false,
                bt601_matrix: true,
                bt2020_ncl_matrix: true,
            },
        }
    }

    #[test]
    fn auth_time_adaptive_video_prefers_av1_then_hevc_without_changing_colour_axes() {
        let mut config = Config::default();
        let request = initial_video(
            VideoSelectionIntent::AdaptivePerformance,
            "h264",
            "yuv420",
            "8",
            "limited",
        );
        config.apply_initial_video_request(&request).unwrap();
        assert_eq!(config.codec, "av1");
        assert_eq!(config.chroma, "yuv420");
        assert_eq!(config.bit_depth, arcen_media::BitDepth::Eight);
        assert_eq!(config.color_range, arcen_media::ColorRange::Limited);
        assert_eq!(
            config.video_selection,
            VideoSelectionIntent::AdaptivePerformance
        );
        assert_eq!(config.color_policy, ColorPolicy::AlwaysOn);
        assert!(config.auth_video_request.is_some());

        let mut no_av1 = Config::default();
        let mut request = request;
        request.capabilities.av1 = false;
        no_av1.apply_initial_video_request(&request).unwrap();
        assert_eq!(no_av1.codec, "h265");
    }

    #[test]
    fn auth_time_colour_fidelity_uses_the_grading_contract_and_variant_pin_wins() {
        let mut grading = initial_video(
            VideoSelectionIntent::ColorFidelity,
            "h265",
            "yuv444",
            "10",
            "full",
        );
        grading.quality.encode_intent = "quality".to_string();
        let mut config = Config {
            bit_depth: arcen_media::BitDepth::Ten,
            color_range: arcen_media::ColorRange::Full,
            chroma: "yuv444".to_string(),
            ..Config::default()
        };
        config.apply_initial_video_request(&grading).unwrap();
        assert_eq!(config.codec, "h265");
        assert_eq!(config.chroma, "yuv444");
        assert_eq!(config.bit_depth, arcen_media::BitDepth::Ten);
        assert_eq!(config.color_range, arcen_media::ColorRange::Full);
        assert_eq!(config.video_selection, VideoSelectionIntent::ColorFidelity);
        assert_eq!(
            config
                .capenc_config(
                    PathBuf::from("arcen-pier"),
                    None,
                    arcen_telemetry::CorrelationId::from_uuid_v4_bytes([1; 16]),
                    arcen_protocol::messages::CursorMode::Local,
                )
                .intent,
            arcen_media::EncodeIntent::Quality
        );

        let mut pinned = parse_ok(&["--variant", "av1-420-8-full-bt709"]);
        pinned.apply_initial_video_request(&grading).unwrap();
        assert_eq!(pinned.codec, "av1");
        assert_eq!(pinned.chroma, "yuv420");
        assert_eq!(pinned.color_range, arcen_media::ColorRange::Full);
        assert!(pinned.auth_video_request.is_some());
    }

    #[test]
    fn explicit_codec_pin_beats_adaptive_selection_and_rejects_incompatible_grading() {
        let mut pinned = Config {
            codec: "h265".to_string(),
            codec_pinned: true,
            ..Config::default()
        };
        let standard = initial_video(
            VideoSelectionIntent::AdaptivePerformance,
            "h264",
            "yuv420",
            "8",
            "limited",
        );
        pinned.apply_initial_video_request(&standard).unwrap();
        assert_eq!(pinned.codec, "h265");
        assert_eq!(pinned.video_selection, VideoSelectionIntent::Exact);

        let mut incompatible = Config {
            codec: "av1".to_string(),
            codec_pinned: true,
            bit_depth: arcen_media::BitDepth::Ten,
            color_range: arcen_media::ColorRange::Full,
            color_policy: ColorPolicy::AlwaysOn,
            ..Config::default()
        };
        let grading = initial_video(
            VideoSelectionIntent::ColorFidelity,
            "h265",
            "yuv444",
            "10",
            "full",
        );
        assert!(incompatible.apply_initial_video_request(&grading).is_err());
    }

    #[test]
    fn defaults_are_safe_dev_values() {
        let c = Config::default();
        assert_eq!(c.port, 18444);
        assert_eq!(c.host, "127.0.0.1", "no-auth defaults to loopback");
        assert_eq!(c.codec, "h264");
        assert_eq!(c.chroma, "yuv420");
        assert_eq!(c.encoder, EncoderSelection::Auto);
        assert_eq!(c.bit_depth, arcen_media::BitDepth::Eight);
        assert_eq!(c.color_range, arcen_media::ColorRange::Limited);
        assert_eq!(c.color_matrix, arcen_media::ColorMatrix::Bt709);
        assert_eq!(
            c.color_policy,
            ColorPolicy::DefaultOff,
            "an existing deployment must not change behaviour by default"
        );
        assert!(!c.tls_enabled(), "material remains operator-supplied");
        assert_eq!(c.tls_posture.version_floor(), TlsVersionFloor::default());
        assert_eq!(c.tls_posture.enabled_suites(), RingCipherSuite::ALL);
        assert_eq!(c.tls_time_policy.warning_window_secs, 30 * 24 * 60 * 60);
        assert!(c.tls_expected_sans.is_empty());
        assert_eq!(c.output_index(), 0, "monitor 1 → output 0");
        assert!(!c.timezone_redirection);
        assert_eq!(c.zoneinfo_root, PathBuf::from("/usr/share/zoneinfo"));
        assert_eq!(c.reconnect_window_secs, 180);
        assert_eq!(c.disconnected_idle_lifetime, None);
        assert!(!c.audio_compressed);
        assert!(!c.microphone_input_enabled);
        assert_eq!(
            c.clipboard_policy,
            ClipboardPolicy::new(
                ClipboardDirection::Both,
                ClipboardContent::All,
                8 * 1024 * 1024
            )
            .unwrap()
        );
    }

    #[test]
    fn disconnected_idle_lifetime_is_linux_config_only_and_opt_in() {
        let mut cfg = Config::default();
        let file: crate::config::PierFileConfig = serde_json::from_str(
            r#"{
                "audio":{"enabled":true,"compressed":false},
                "microphone_input":{"enabled":false},
                "platform":{"session":{"disconnected_idle_timeout_secs":300}}
            }"#,
        )
        .expect("config");

        apply_file_config(&mut cfg, file).expect("apply config");
        assert_eq!(
            cfg.disconnected_idle_lifetime,
            Some(Duration::from_secs(300))
        );
    }

    #[test]
    fn disconnected_idle_lifetime_zero_is_rejected() {
        let mut cfg = Config::default();
        let file: crate::config::PierFileConfig = serde_json::from_str(
            r#"{
                "audio":{"enabled":true,"compressed":false},
                "microphone_input":{"enabled":false},
                "platform":{"session":{"disconnected_idle_timeout_secs":0}}
            }"#,
        )
        .expect("config");

        let error = apply_file_config(&mut cfg, file).expect_err("zero must fail validation");
        assert!(
            error.contains("platform.session.disconnected_idle_timeout_secs"),
            "{error}"
        );
    }

    #[test]
    fn audio_compression_and_microphone_policy_are_explicit() {
        let config = parse_ok(&["--audio", "--audio-compressed", "--microphone-input"]);
        assert!(config.audio_enabled);
        assert!(config.audio_compressed);
        assert!(config.microphone_input_enabled);
    }

    #[test]
    fn unified_json_loads_before_cli_overrides() {
        let directory = std::env::temp_dir().join(format!(
            "arcen-linux-pier-config-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&directory).expect("config dir");
        let path = directory.join("pier.json");
        std::fs::write(
            &path,
            r#"{
                "listen":{"host":"127.0.0.1","port":18443,"quic_port":18444},
                "tls":{"cert":"host.crt","key":"host.key"},
                "video":{"codec":"h264","chroma":"yuv420","fps":30},
                "audio":{"enabled":true,"compressed":false},
                "microphone_input":{"enabled":true},
                "platform":{"auth":{"mode":"pam"}}
            }"#,
        )
        .expect("write config");

        let config = parse(&[
            "--config".to_string(),
            path.display().to_string(),
            "--audio-compressed".to_string(),
            "--no-microphone-input".to_string(),
        ])
        .expect("merged config");
        assert_eq!(config.config_path.as_deref(), Some(path.as_path()));
        assert!(config.audio_enabled);
        assert!(config.audio_compressed);
        assert!(!config.microphone_input_enabled);
        assert_eq!(config.auth_mode, AuthMode::Pam);
        assert_eq!(config.quic_port, Some(18_444));
        assert_eq!(
            config.tls_cert.as_deref(),
            Some(directory.join("host.crt").as_path())
        );
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_dir(directory);
    }

    #[test]
    fn capenc_config_preserves_unspecified_and_applied_geometry() {
        let config = Config::default();
        let session_log_id =
            arcen_telemetry::CorrelationId::parse_uuid("01234567-89ab-4def-8123-456789abcdef")
                .unwrap();
        let unspecified = config.capenc_config(
            PathBuf::from("arcen-capenc"),
            None,
            session_log_id.clone(),
            arcen_protocol::messages::CursorMode::Local,
        );
        assert_eq!((unspecified.width, unspecified.height), (0, 0));

        let applied = config.capenc_config_for_size(
            PathBuf::from("arcen-capenc"),
            None,
            session_log_id,
            arcen_protocol::messages::CursorMode::Local,
            1512,
            944,
        );
        assert_eq!((applied.width, applied.height), (1512, 944));
    }

    #[test]
    fn clipboard_policy_overrides_are_strict() {
        let config = parse_ok(&[
            "--auth-mode",
            "pam",
            "--clipboard-direction",
            "host_to_client",
            "--clipboard-content",
            "text",
            "--clipboard-max-bytes",
            "1048576",
        ]);
        assert_eq!(
            config.clipboard_policy.direction,
            ClipboardDirection::HostToClient
        );
        assert_eq!(config.clipboard_policy.content, ClipboardContent::Text);
        assert_eq!(config.clipboard_policy.max_bytes, 1024 * 1024);
        assert!(parse(&[
            "--tls-cert".into(),
            "cert".into(),
            "--tls-key".into(),
            "key".into(),
            "--clipboard-max-bytes".into(),
            "1048575".into(),
        ])
        .is_err());
    }

    #[test]
    fn reconnect_window_uses_shared_inclusive_bounds() {
        assert_eq!(
            parse_ok(&["--reconnect-window-secs", "0"]).reconnect_window_secs,
            0
        );
        assert_eq!(
            parse_ok(&["--reconnect-window-secs", "7200"]).reconnect_window_secs,
            7_200
        );
        assert!(parse(&[
            "--tls-cert".into(),
            "cert".into(),
            "--tls-key".into(),
            "key".into(),
            "--reconnect-window-secs".into(),
            "7201".into(),
        ])
        .is_err());
    }

    #[test]
    fn timezone_redirection_is_opt_in_with_last_override_winning() {
        assert!(parse_ok(&["--timezone-redirection"]).timezone_redirection);
        assert!(
            !parse_ok(&["--timezone-redirection", "--no-timezone-redirection"])
                .timezone_redirection
        );
        assert!(
            parse_ok(&["--no-timezone-redirection", "--timezone-redirection"]).timezone_redirection
        );
    }

    #[test]
    fn zoneinfo_root_uses_the_last_configured_path() {
        let config = parse_ok(&[
            "--zoneinfo-root",
            "first-zoneinfo",
            "--zoneinfo-root",
            "second-zoneinfo",
        ]);
        assert_eq!(config.zoneinfo_root, PathBuf::from("second-zoneinfo"));
    }

    #[test]
    fn h265_yuv444_is_accepted() {
        let c = parse_ok(&["--codec", "h265", "--chroma", "yuv444"]);
        assert_eq!(c.codec, "h265");
        assert!(c.wire_yuv444());
    }

    #[test]
    fn h264_yuv444_is_rejected() {
        let err = parse(&[
            "--codec".into(),
            "h264".into(),
            "--chroma".into(),
            "yuv444".into(),
        ])
        .unwrap_err();
        assert!(err.contains("h265"), "should point to HEVC: {err}");
    }

    #[test]
    fn av1_yuv420_is_accepted_and_av1_yuv444_is_rejected() {
        let config = parse_ok(&["--codec", "av1", "--chroma", "yuv420"]);
        assert_eq!(config.codec, "av1");
        assert!(!config.wire_yuv444());

        let error = parse(&[
            "--codec".into(),
            "av1".into(),
            "--chroma".into(),
            "yuv444".into(),
        ])
        .unwrap_err();
        assert!(error.contains("av1"), "error must name the codec: {error}");
        assert!(
            error.contains("yuv420"),
            "error must name the ceiling: {error}"
        );
    }

    #[test]
    fn explicit_encoder_modes_are_parsed_and_software_requires_h264_420() {
        assert_eq!(
            parse_ok(&["--encoder", "nvenc"]).encoder,
            EncoderSelection::NativeNvenc
        );
        assert_eq!(
            parse_ok(&["--encoder", "software-h264"]).encoder,
            EncoderSelection::SoftwareH264
        );
        assert!(parse(&[
            "--encoder".into(),
            "software-h264".into(),
            "--codec".into(),
            "h265".into(),
        ])
        .is_err());
        assert!(parse(&["--encoder".into(), "unknown".into()]).is_err());
    }

    #[test]
    fn color_keys_are_parsed_from_cli_flags() {
        let c = parse_ok(&[
            "--bit-depth",
            "10",
            "--color-range",
            "full",
            "--color-matrix",
            "bt2020ncl",
            "--color-policy",
            "always-on",
        ]);
        assert_eq!(c.bit_depth, arcen_media::BitDepth::Ten);
        assert_eq!(c.color_range, arcen_media::ColorRange::Full);
        assert_eq!(c.color_matrix, arcen_media::ColorMatrix::Bt2020Ncl);
        assert_eq!(c.color_policy, ColorPolicy::AlwaysOn);
    }

    #[test]
    fn bad_color_values_are_rejected_not_silently_defaulted() {
        let bit_depth_error = parse(&["--bit-depth".into(), "9".into()]).unwrap_err();
        assert!(
            bit_depth_error.contains('9'),
            "error must name the offending value: {bit_depth_error}"
        );
        let range_error = parse(&["--color-range".into(), "studio".into()]).unwrap_err();
        assert!(
            range_error.contains("studio"),
            "error must name the offending value: {range_error}"
        );
        let matrix_error = parse(&["--color-matrix".into(), "bt2020c".into()]).unwrap_err();
        assert!(
            matrix_error.contains("bt2020c"),
            "error must name the offending value: {matrix_error}"
        );
        let policy_error = parse(&["--color-policy".into(), "sometimes".into()]).unwrap_err();
        assert!(
            policy_error.contains("sometimes"),
            "error must name the offending value: {policy_error}"
        );
    }

    #[test]
    fn variant_overrides_individual_keys() {
        let c = parse_ok(&[
            "--codec",
            "h264",
            "--chroma",
            "yuv420",
            "--bit-depth",
            "8",
            "--color-range",
            "limited",
            "--variant",
            "hevc-444-10-full-bt709",
        ]);
        assert_eq!(c.codec, "h265");
        assert_eq!(c.chroma, "yuv444");
        assert_eq!(c.bit_depth, arcen_media::BitDepth::Ten);
        assert_eq!(c.color_range, arcen_media::ColorRange::Full);
        assert_eq!(c.color_matrix, arcen_media::ColorMatrix::Bt709);
        assert_eq!(c.color_policy, ColorPolicy::AlwaysOn);
    }

    #[test]
    fn variant_rejects_an_unknown_id() {
        let error = parse(&["--variant".into(), "hevc-444-10".into()]).unwrap_err();
        assert!(
            error.contains("hevc-444-10"),
            "error must name the offending variant id: {error}"
        );
    }

    #[test]
    fn variant_accepts_hardware_av1_420_but_rejects_unwired_chroma_without_partial_apply() {
        let av1 = parse_ok(&["--variant", "av1-420-8-full-bt709"]);
        assert_eq!(av1.codec, "av1");
        assert_eq!(av1.chroma, "yuv420");
        assert_eq!(av1.color_policy, ColorPolicy::AlwaysOn);

        // AV1 4:4:4 is a coherent software variant, but NVENC exposes only
        // Main-profile 4:2:0 and this Pier has no software-AV1 backend.
        let av1_error = parse(&["--variant".into(), "av1-444-10-full-bt709".into()]).unwrap_err();
        assert!(
            av1_error.contains("av1"),
            "error must name av1: {av1_error}"
        );

        // 4:2:2 is a probe-only row (Blackwell NVENC), not a shipped chroma.
        let chroma_error =
            parse(&["--variant".into(), "hevc-422-10-full-bt709".into()]).unwrap_err();
        assert!(
            chroma_error.contains("yuv422"),
            "error must name yuv422: {chroma_error}"
        );

        // A rejected variant must not half-apply: codec/chroma/bit_depth stay
        // exactly as configured before the attempt.
        let mut cfg = Config::default();
        let before = (cfg.codec.clone(), cfg.chroma.clone(), cfg.bit_depth);
        assert!(apply_variant(&mut cfg, "hevc-422-10-full-bt709").is_err());
        assert_eq!((cfg.codec, cfg.chroma, cfg.bit_depth), before);
    }

    #[test]
    fn twelve_bit_with_explicit_nvenc_is_rejected() {
        let error = parse(&[
            "--bit-depth".into(),
            "12".into(),
            "--encoder".into(),
            "nvenc".into(),
        ])
        .unwrap_err();
        assert!(
            error.contains("12") && error.contains("nvenc"),
            "error must explain the NVENC/12-bit conflict: {error}"
        );
        // Twelve-bit is fine when the encoder is pinned to the software tier.
        assert_eq!(
            parse_ok(&["--bit-depth", "12", "--encoder", "software-h264"]).bit_depth,
            arcen_media::BitDepth::Twelve
        );
    }

    #[test]
    fn color_policy_always_on_forces_the_ceiling_regardless_of_client_request() {
        let ceiling = arcen_media::BitDepth::Ten;
        assert_eq!(
            ColorPolicy::AlwaysOn.resolve_bit_depth(ceiling, None),
            ceiling
        );
        assert_eq!(
            ColorPolicy::AlwaysOn.resolve_bit_depth(ceiling, Some(arcen_media::BitDepth::Eight)),
            ceiling,
            "a client asking for less must be ignored"
        );
        assert_eq!(
            ColorPolicy::AlwaysOn.resolve_bit_depth(ceiling, Some(arcen_media::BitDepth::Twelve)),
            ceiling,
            "a client asking for more must be capped at the configured ceiling"
        );
    }

    #[test]
    fn color_policy_always_off_forces_the_conservative_baseline_regardless_of_client_request() {
        let ceiling = arcen_media::BitDepth::Ten;
        assert_eq!(
            ColorPolicy::AlwaysOff.resolve_bit_depth(ceiling, None),
            arcen_media::BitDepth::Eight
        );
        assert_eq!(
            ColorPolicy::AlwaysOff.resolve_bit_depth(ceiling, Some(arcen_media::BitDepth::Twelve)),
            arcen_media::BitDepth::Eight,
            "a client asking for more must be ignored"
        );
    }

    #[test]
    fn color_policy_default_on_defaults_high_but_honours_an_explicit_client_request() {
        let ceiling = arcen_media::BitDepth::Ten;
        assert_eq!(
            ColorPolicy::DefaultOn.resolve_bit_depth(ceiling, None),
            ceiling,
            "no client preference defaults to the ceiling"
        );
        assert_eq!(
            ColorPolicy::DefaultOn.resolve_bit_depth(ceiling, Some(arcen_media::BitDepth::Eight)),
            arcen_media::BitDepth::Eight,
            "an explicit lower request is honoured"
        );
        assert_eq!(
            ColorPolicy::DefaultOn.resolve_bit_depth(ceiling, Some(arcen_media::BitDepth::Twelve)),
            ceiling,
            "a request above the ceiling is capped, not honoured verbatim"
        );
    }

    #[test]
    fn color_policy_default_off_defaults_conservative_but_lets_a_client_negotiate_up() {
        let ceiling = arcen_media::BitDepth::Ten;
        assert_eq!(
            ColorPolicy::DefaultOff.resolve_bit_depth(ceiling, None),
            arcen_media::BitDepth::Eight,
            "no client preference defaults to the conservative baseline"
        );
        assert_eq!(
            ColorPolicy::DefaultOff.resolve_bit_depth(ceiling, Some(arcen_media::BitDepth::Ten)),
            ceiling,
            "an explicit request up to the ceiling is honoured"
        );
        assert_eq!(
            ColorPolicy::DefaultOff.resolve_bit_depth(ceiling, Some(arcen_media::BitDepth::Twelve)),
            ceiling,
            "a request above the ceiling is still capped"
        );
    }

    #[test]
    fn color_policy_resolves_color_range_with_the_same_ceiling_semantics() {
        use arcen_media::ColorRange;
        let ceiling = ColorRange::Full;
        assert_eq!(
            ColorPolicy::AlwaysOn.resolve_color_range(ceiling, None),
            ColorRange::Full
        );
        assert_eq!(
            ColorPolicy::AlwaysOff.resolve_color_range(ceiling, Some(ColorRange::Full)),
            ColorRange::Limited
        );
        assert_eq!(
            ColorPolicy::DefaultOn.resolve_color_range(ceiling, None),
            ColorRange::Full
        );
        assert_eq!(
            ColorPolicy::DefaultOff.resolve_color_range(ceiling, None),
            ColorRange::Limited
        );
        assert_eq!(
            ColorPolicy::DefaultOff.resolve_color_range(ceiling, Some(ColorRange::Full)),
            ColorRange::Full
        );
        // A ceiling of `limited` means full range is never reachable, even if
        // a client would otherwise prefer it.
        let limited_ceiling = ColorRange::Limited;
        assert_eq!(
            ColorPolicy::DefaultOff.resolve_color_range(limited_ceiling, Some(ColorRange::Full)),
            ColorRange::Limited
        );
    }

    #[test]
    fn color_policy_resolves_matrix_without_clamping_a_client_choice() {
        use arcen_media::ColorMatrix;
        let ceiling = ColorMatrix::Bt2020Ncl;
        assert_eq!(
            ColorPolicy::AlwaysOn.resolve_color_matrix(ceiling, Some(ColorMatrix::Bt601)),
            ceiling,
            "always-on ignores the client entirely"
        );
        assert_eq!(
            ColorPolicy::AlwaysOff.resolve_color_matrix(ceiling, Some(ColorMatrix::Bt601)),
            ColorMatrix::Bt709,
            "always-off ignores the client entirely"
        );
        assert_eq!(
            ColorPolicy::DefaultOn.resolve_color_matrix(ceiling, None),
            ceiling
        );
        assert_eq!(
            ColorPolicy::DefaultOn.resolve_color_matrix(ceiling, Some(ColorMatrix::Bt601)),
            ColorMatrix::Bt601,
            "matrix is not a fidelity axis, so an explicit request is honoured verbatim"
        );
        assert_eq!(
            ColorPolicy::DefaultOff.resolve_color_matrix(ceiling, None),
            ColorMatrix::Bt709
        );
    }

    #[test]
    fn color_policy_tokens_round_trip() {
        for policy in ColorPolicy::ALL {
            assert_eq!(ColorPolicy::from_token(policy.token()), Some(*policy));
        }
        assert_eq!(ColorPolicy::from_token("sometimes"), None);
    }

    #[test]
    fn color_keys_are_applied_from_file_config() {
        let mut config = Config::default();
        let file: crate::config::PierFileConfig = serde_json::from_str(
            r#"{
                "audio":{"enabled":true,"compressed":false},
                "microphone_input":{"enabled":false},
                "video":{
                    "bit_depth":"10",
                    "color_range":"full",
                    "color_matrix":"bt2020ncl",
                    "color_policy":"always-on"
                },
                "platform": {}
            }"#,
        )
        .expect("config");
        apply_file_config(&mut config, file).expect("apply file config");
        assert_eq!(config.bit_depth, arcen_media::BitDepth::Ten);
        assert_eq!(config.color_range, arcen_media::ColorRange::Full);
        assert_eq!(config.color_matrix, arcen_media::ColorMatrix::Bt2020Ncl);
        assert_eq!(config.color_policy, ColorPolicy::AlwaysOn);
    }

    #[test]
    fn variant_file_config_overrides_individual_file_keys() {
        let mut config = Config::default();
        let file: crate::config::PierFileConfig = serde_json::from_str(
            r#"{
                "audio":{"enabled":true,"compressed":false},
                "microphone_input":{"enabled":false},
                "video":{
                    "codec":"h264",
                    "chroma":"yuv420",
                    "bit_depth":"8",
                    "variant":"hevc-444-10-full-bt709"
                },
                "platform": {}
            }"#,
        )
        .expect("config");
        apply_file_config(&mut config, file).expect("apply file config");
        assert_eq!(config.codec, "h265");
        assert_eq!(config.chroma, "yuv444");
        assert_eq!(config.bit_depth, arcen_media::BitDepth::Ten);
        assert_eq!(config.color_range, arcen_media::ColorRange::Full);
        assert_eq!(config.color_matrix, arcen_media::ColorMatrix::Bt709);
    }

    #[test]
    fn variant_file_config_rejects_bad_id() {
        let mut config = Config::default();
        let file: crate::config::PierFileConfig = serde_json::from_str(
            r#"{
                "audio":{"enabled":true,"compressed":false},
                "microphone_input":{"enabled":false},
                "video":{"variant":"not-a-real-variant"},
                "platform": {}
            }"#,
        )
        .expect("config");
        let error = apply_file_config(&mut config, file).expect_err("bad variant id must fail");
        assert!(
            error.contains("variant"),
            "error must name the key: {error}"
        );
    }

    #[test]
    fn fps_must_match_capenc_effective_range() {
        assert!(parse(&["--fps".into(), "0".into()]).is_err());
        assert!(parse(&["--fps".into(), "241".into()]).is_err());
        assert_eq!(parse_ok(&["--fps", "240"]).fps, 240);
    }

    #[test]
    fn pam_mode_is_accepted_without_unsafe_remote_flag() {
        let config = parse_ok(&[
            "--auth-mode",
            "pam",
            "--host",
            "0.0.0.0",
            "--tls-cert",
            "/tmp/cert",
            "--tls-key",
            "/tmp/key",
        ]);
        assert_eq!(config.auth_mode, AuthMode::Pam);
        assert!(!config.unsafe_allow_remote_no_auth);
    }

    #[test]
    fn remote_pam_requires_tls() {
        let error = parse(&[
            "--auth-mode".into(),
            "pam".into(),
            "--host".into(),
            "0.0.0.0".into(),
        ])
        .unwrap_err();
        assert!(error.contains("direct QUIC requires"));
        assert!(parse(&["--auth-mode".into(), "pam".into(),]).is_err());
    }

    #[test]
    fn pam_service_is_validated() {
        let config = parse_ok(&["--auth-mode", "pam", "--pam-service", "arcen_login"]);
        assert_eq!(config.pam_service, "arcen_login");
        assert!(parse(&["--pam-service".into(), "../login".into()]).is_err());
    }

    #[test]
    fn disclaimer_is_prepared_once_from_the_selected_locale_file() {
        let root = std::env::temp_dir().join(format!(
            "arcen-linux-disclaimer-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("en_US.txt"), b"Authorized use only.").unwrap();
        let config = parse_ok(&[
            "--auth-mode",
            "pam",
            "--disclaimer",
            "--disclaimer-dir",
            root.to_str().unwrap(),
            "--disclaimer-locale",
            "en_US",
        ]);
        let disclaimer = config.disclaimer.expect("prepared disclaimer");
        assert_eq!(disclaimer.text(), "Authorized use only.");
        assert_eq!(disclaimer.locale().as_str(), "en_US");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn disclaimer_is_prepared_after_cli_overrides() {
        let root = std::env::temp_dir().join(format!(
            "arcen-linux-disclaimer-precedence-test-{}",
            std::process::id()
        ));
        let disclaimer_dir = root.join("disclaimers");
        std::fs::create_dir_all(&disclaimer_dir).expect("disclaimer dir");
        std::fs::write(disclaimer_dir.join("en_US.txt"), b"Authorized use only.")
            .expect("disclaimer");
        let path = root.join("pier.json");
        std::fs::write(
            &path,
            r#"{
                "tls":{"cert":"host.crt","key":"host.key"},
                "audio":{"enabled":true,"compressed":false},
                "microphone_input":{"enabled":false},
                "auth":{"disclaimer":{"enabled":true,"directory":"disclaimers"}},
                "platform":{"auth":{"mode":"pam"}}
            }"#,
        )
        .expect("config");

        let configured = parse(&[
            "--config".to_string(),
            path.display().to_string(),
            "--auth-mode".to_string(),
            "pam".to_string(),
        ])
        .expect("CLI auth override");
        assert_eq!(
            configured.disclaimer.as_ref().unwrap().text(),
            "Authorized use only."
        );

        std::fs::remove_file(disclaimer_dir.join("en_US.txt")).expect("remove disclaimer");
        assert!(parse(&[
            "--config".to_string(),
            path.display().to_string(),
            "--no-disclaimer".to_string(),
        ])
        .is_ok());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn disclaimer_configuration_fails_closed() {
        assert!(parse(&["--disclaimer".into()]).is_err());
        assert!(parse(&[
            "--auth-mode".into(),
            "pam".into(),
            "--disclaimer-dir".into(),
            "/tmp".into(),
        ])
        .is_err());
        assert!(parse(&[
            "--auth-mode".into(),
            "pam".into(),
            "--disclaimer".into(),
            "--disclaimer-locale".into(),
            "../en_US".into(),
        ])
        .is_err());

        let root = std::env::temp_dir().join(format!(
            "arcen-linux-invalid-disclaimer-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let parse_from_root = || {
            parse(&[
                "--auth-mode".into(),
                "pam".into(),
                "--disclaimer".into(),
                "--disclaimer-dir".into(),
                root.to_string_lossy().into_owned(),
            ])
        };
        assert!(parse_from_root().is_err());
        std::fs::write(root.join("en_US.txt"), []).unwrap();
        assert!(parse_from_root().is_err());
        std::fs::write(root.join("en_US.txt"), [0xff]).unwrap();
        assert!(parse_from_root().is_err());
        std::fs::write(
            root.join("en_US.txt"),
            vec![b'x'; arcen_identity::MAX_DISCLAIMER_CONTENT_BYTES + 1],
        )
        .unwrap();
        assert!(parse_from_root().is_err());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn desktop_session_is_allowlisted() {
        let config = parse_ok(&["--auth-mode", "pam", "--desktop-session", "gnome"]);
        assert_eq!(config.desktop_session, "gnome");
        assert!(parse(&[
            "--auth-mode".into(),
            "pam".into(),
            "--desktop-session".into(),
            "../../tmp/evil".into(),
        ])
        .is_err());
    }

    #[test]
    fn tls_requires_both_cert_and_key() {
        let err = parse(&["--tls-cert".into(), "/tmp/c.pem".into()]).unwrap_err();
        assert!(err.contains("together"));
    }

    #[test]
    fn direct_transport_requires_tls_on_any_nonzero_udp_port() {
        let config = parse(&[
            "--tls-cert".into(),
            "/tmp/c.pem".into(),
            "--tls-key".into(),
            "/tmp/k.pem".into(),
            "--port".into(),
            "19000".into(),
        ])
        .expect("custom QUIC UDP port");
        assert_eq!(config.direct_quic_port(), 19_000);
        assert!(parse(&[]).unwrap_err().contains("direct QUIC requires"));
    }

    #[test]
    fn tls_policy_options_are_repeatable_and_validated() {
        let config = parse_ok(&[
            "--tls-minimum-version",
            "TLS1.3",
            "--tls-disabled-cipher-suite",
            "TLS13_AES_128_GCM_SHA256",
            "--tls-disabled-cipher-suite",
            "TLS13_AES_256_GCM_SHA384",
            "--tls-expiry-warning-days",
            "14",
            "--tls-expected-san",
            "pier.example",
            "--tls-expected-san",
            "192.0.2.10",
        ]);
        assert_eq!(config.tls_posture.version_floor(), TlsVersionFloor::Tls13);
        assert_eq!(config.tls_posture.disabled_suites().len(), 2);
        assert_eq!(config.tls_time_policy.warning_window_secs, 14 * 86_400);
        assert_eq!(config.tls_expected_sans, ["pier.example", "192.0.2.10"]);

        assert!(parse(&["--tls-minimum-version".into(), "TLS1.1".into()]).is_err());
        assert!(parse(&["--tls-disabled-cipher-suite".into(), "not-a-suite".into()]).is_err());
        assert!(parse(&["--tls-expiry-warning-days".into(), "3651".into()]).is_err());
        assert!(parse(&["--tls-expected-san".into(), "bad name".into()]).is_err());
    }

    #[test]
    fn yuv422_is_rejected_instead_of_silently_coerced() {
        assert!(parse(&["--chroma".into(), "yuv422".into()]).is_err());
    }

    #[test]
    fn config_file_omitting_auth_mode_resolves_to_pam() {
        // SEC-001, the actual regression. The shipped packaging JSON sets
        // `platform.auth.mode`, so the defect only bites when a key is deleted
        // or mistyped. Before the fix that silently produced an unauthenticated
        // Pier; now the absent key means the secure choice.
        let root =
            std::env::temp_dir().join(format!("arcen-sec001-absent-key-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("test dir");
        let path = root.join("pier.json");
        std::fs::write(
            &path,
            br#"{
                "tls":{"cert":"host.crt","key":"host.key"},
                "audio":{"enabled":true,"compressed":false},
                "microphone_input":{"enabled":false},
                "platform":{"auth":{"pam_service":"login"}}
            }"#,
        )
        .expect("config");
        let config = parse(&["--config".to_string(), path.display().to_string()])
            .expect("config with no auth mode must load");
        assert_eq!(config.auth_mode, AuthMode::Pam);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    #[cfg(not(feature = "insecure-lab-no-auth"))]
    fn config_file_requesting_no_auth_is_refused() {
        // SEC-001. Both routes an operator has through the config file must
        // fail closed, and the retired key must fail loudly rather than being
        // silently ignored, because an operator who set it believes it applies.
        let root = std::env::temp_dir().join(format!(
            "arcen-sec001-config-refusal-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).expect("test dir");
        for (name, body) in [
            (
                "mode-none.json",
                br#"{
                    "audio":{"enabled":true,"compressed":false},
                    "microphone_input":{"enabled":false},
                    "platform":{"auth":{"mode":"none"}}
                }"#
                .as_slice(),
            ),
            (
                "unsafe-true.json",
                br#"{
                    "audio":{"enabled":true,"compressed":false},
                    "microphone_input":{"enabled":false},
                    "platform":{"auth":{"unsafe_allow_remote_no_auth":true}}
                }"#
                .as_slice(),
            ),
        ] {
            let path = root.join(name);
            std::fs::write(&path, body).expect("config");
            let error = parse(&["--config".to_string(), path.display().to_string()])
                .expect_err(&format!("{name} must be refused"));
            assert!(
                error.contains("refusing to disable authentication"),
                "{name} produced the wrong error: {error}"
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn default_auth_mode_is_pam() {
        // SEC-001. The single most important assertion in this file: a Config
        // that nobody configured must authenticate. Before SEC-001 this was
        // `AuthMode::None`, so omitting one JSON key served an unauthenticated
        // desktop.
        assert_eq!(Config::default().auth_mode, AuthMode::Pam);
    }

    #[test]
    #[cfg(not(feature = "insecure-lab-no-auth"))]
    fn no_auth_flags_are_refused_in_a_release_build() {
        // SEC-001. Each of these used to disable the product's only trust
        // boundary at run time. In a build without `insecure-lab-no-auth` they
        // must all fail, and fail with the same greppable message.
        for argv in [
            vec!["--no-auth".to_string()],
            vec!["--unsafe-allow-remote-no-auth".to_string()],
            vec!["--auth-mode".to_string(), "none".to_string()],
        ] {
            let error = parse(&argv).expect_err(&format!("{argv:?} must be refused"));
            assert!(
                error.contains("refusing to disable authentication"),
                "{argv:?} produced the wrong error: {error}"
            );
        }
    }

    #[test]
    fn remote_bind_no_longer_needs_an_unsafe_acknowledgement() {
        // SEC-001 removed the whole no-auth remote path, so a normal remote
        // bind is now simply a PAM bind and needs TLS, nothing more.
        let config = parse_ok(&[
            "--host",
            "0.0.0.0",
            "--tls-cert",
            "host.crt",
            "--tls-key",
            "host.key",
        ]);
        assert_eq!(config.host, "0.0.0.0");
        assert_eq!(config.auth_mode, AuthMode::Pam);
        assert!(!config.unsafe_allow_remote_no_auth);
    }

    #[test]
    fn uinput_mode_is_explicitly_enabled() {
        assert_eq!(
            parse_ok(&["--auth-mode", "pam", "--input-mode", "uinput",]).input_mode,
            InputMode::Uinput
        );
        assert!(parse(&["--input-mode".into(), "uinput".into()]).is_err());
    }

    #[test]
    fn deskside_cli_requires_complete_explicit_pins() {
        let output = format!("DP-1,{},{}", "1".repeat(64), "2".repeat(64));
        let config = parse_ok(&[
            "--auth-mode",
            "pam",
            "--input-mode",
            "uinput",
            "--deskside",
            "--deskside-firmware-sha256",
            "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
            "--deskside-console-uid",
            "1000",
            "--deskside-console-display",
            ":0",
            "--deskside-console-xauthority",
            "/run/arcen/console.Xauthority",
            "--deskside-input",
            "/dev/input/by-id/keyboard-event-kbd",
            "--deskside-input",
            "/dev/input/by-id/mouse-event-mouse",
            "--deskside-output",
            &output,
        ]);
        assert!(config.deskside.enabled);
        assert_eq!(config.deskside.input_devices.len(), 2);
        assert_eq!(config.deskside.outputs.len(), 1);

        assert!(parse(&[
            "--tls-cert".into(),
            "/tmp/cert".into(),
            "--tls-key".into(),
            "/tmp/key".into(),
            "--deskside-console-display".into(),
            ":0".into(),
        ])
        .is_err());
    }

    #[test]
    fn multi_monitor_gate_defaults_to_disabled_with_no_heads() {
        let config = parse_ok(&["--auth-mode", "pam"]);
        assert!(!config.multi_monitor.advertise_enabled);
        assert!(config.multi_monitor.heads.is_empty());
    }

    #[test]
    fn multi_monitor_cli_flags_enable_the_gate_and_collect_repeated_heads() {
        let config = parse_ok(&[
            "--auth-mode",
            "pam",
            "--encoder",
            "nvenc",
            "--multi-monitor",
            "--multi-monitor-head",
            "DFP-0",
            "--multi-monitor-head",
            "DFP-1",
        ]);
        assert!(config.multi_monitor.advertise_enabled);
        assert_eq!(config.multi_monitor.heads, vec!["DFP-0", "DFP-1"]);
    }

    #[test]
    fn multi_monitor_cli_last_boolean_override_wins() {
        let config = parse_ok(&[
            "--auth-mode",
            "pam",
            "--multi-monitor",
            "--no-multi-monitor",
        ]);
        assert!(!config.multi_monitor.advertise_enabled);
    }

    #[test]
    fn multi_monitor_cli_rejects_an_invalid_head_token() {
        assert!(parse(&[
            "--tls-cert".into(),
            "/tmp/cert".into(),
            "--tls-key".into(),
            "/tmp/key".into(),
            "--auth-mode".into(),
            "pam".into(),
            "--multi-monitor-head".into(),
            "HDMI-0".into(),
        ])
        .is_err());
    }

    #[test]
    fn multi_monitor_cli_rejects_a_duplicate_head_token() {
        let error = parse(&[
            "--tls-cert".into(),
            "/tmp/cert".into(),
            "--tls-key".into(),
            "/tmp/key".into(),
            "--auth-mode".into(),
            "pam".into(),
            "--encoder".into(),
            "nvenc".into(),
            "--multi-monitor-head".into(),
            "DFP-0".into(),
            "--multi-monitor-head".into(),
            "DFP-0".into(),
        ])
        .expect_err("a duplicated --multi-monitor-head must fail parse/validate-config");
        assert!(
            error.contains("DFP-0"),
            "error must name the duplicated head: {error}"
        );
    }

    #[test]
    fn multi_monitor_advertise_enabled_with_auto_encoder_fails_validate_config() {
        let error = parse(&[
            "--tls-cert".into(),
            "/tmp/cert".into(),
            "--tls-key".into(),
            "/tmp/key".into(),
            "--auth-mode".into(),
            "pam".into(),
            "--multi-monitor".into(),
            "--multi-monitor-head".into(),
            "DFP-0".into(),
        ])
        .expect_err(
            "advertise_enabled with the default auto encoder must fail, not silently withhold",
        );
        assert!(
            error.contains("encoder"),
            "error must mention the encoder policy conflict: {error}"
        );
    }

    #[test]
    fn multi_monitor_config_file_section_is_applied() {
        let mut config = Config::default();
        let file: crate::config::PierFileConfig = serde_json::from_str(
            r#"{
                "audio":{"enabled":true,"compressed":false},
                "microphone_input":{"enabled":false},
                "platform": {
                    "multi_monitor": {
                        "advertise_enabled": true,
                        "heads": ["DFP-0", "DFP-2"]
                    }
                }
            }"#,
        )
        .expect("config");
        apply_file_config(&mut config, file).expect("apply file config");
        assert!(config.multi_monitor.advertise_enabled);
        assert_eq!(config.multi_monitor.heads, vec!["DFP-0", "DFP-2"]);
    }

    #[test]
    fn multi_monitor_config_file_rejects_an_invalid_head_token() {
        let mut config = Config::default();
        let file: crate::config::PierFileConfig = serde_json::from_str(
            r#"{
                "audio":{"enabled":true,"compressed":false},
                "microphone_input":{"enabled":false},
                "platform": {
                    "multi_monitor": {
                        "advertise_enabled": true,
                        "heads": ["HDMI-0"]
                    }
                }
            }"#,
        )
        .expect("config");
        assert!(apply_file_config(&mut config, file).is_err());
    }

    #[test]
    fn multi_monitor_config_file_rejects_duplicate_heads_through_full_parse() {
        let directory = std::env::temp_dir().join(format!(
            "arcen-linux-pier-config-dup-heads-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&directory).expect("config dir");
        let path = directory.join("pier.json");
        std::fs::write(
            &path,
            r#"{
                "video":{"encoder":"nvenc"},
                "audio":{"enabled":true,"compressed":false},
                "microphone_input":{"enabled":false},
                "platform": {
                    "auth": {"mode": "pam"},
                    "multi_monitor": {
                        "advertise_enabled": true,
                        "heads": ["DFP-0", "DFP-0"]
                    }
                }
            }"#,
        )
        .expect("write config");

        let error = parse(&[
            "--tls-cert".into(),
            "/tmp/cert".into(),
            "--tls-key".into(),
            "/tmp/key".into(),
            "--config".into(),
            path.display().to_string(),
        ])
        .expect_err("a duplicated configured head must fail parse/validate-config");
        assert!(
            error.contains("DFP-0"),
            "error must name the duplicated head: {error}"
        );

        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_dir(directory);
    }

    #[test]
    fn native_audio_is_explicitly_enabled() {
        let config = parse_ok(&["--audio", "--audiocap-bin", "/tmp/audiocap"]);
        assert!(config.audio_enabled);
        assert_eq!(config.audiocap_bin, Some(PathBuf::from("/tmp/audiocap")));
    }

    #[test]
    fn host_audio_mode_remains_available_for_migration() {
        let config = parse_ok(&["--audio-user", "host"]);
        assert_eq!(config.audio_user_mode, AudioUserMode::Host);
    }

    #[test]
    fn pam_uses_a_dedicated_nonzero_display_and_gpu_head() {
        let config = parse_ok(&[
            "--auth-mode",
            "pam",
            "--session-display",
            ":12",
            "--session-gpu-head",
            "DFP-3",
        ]);
        assert_eq!(config.session_display, ":12");
        assert_eq!(config.session_gpu_head, "DFP-3");
        assert!(parse(&["--session-display".into(), ":0".into()]).is_err());
        assert!(parse(&["--session-gpu-head".into(), "DFP-4".into()]).is_err());
    }
}
