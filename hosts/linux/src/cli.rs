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

/// Highest refresh a dedicated Xorg desktop's head is created at (the
/// launcher's own bound on a served display).
const MAX_XORG_HEAD_REFRESH_HZ: u32 = 240;

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

/// Lab lever, default off: the named pipe a colour-managed compositor
/// capture helper writes PQ / BT.2020 ten-bit frames into.
///
/// When set, an HDR request is served from that pipe instead of being
/// resolved to Grading, so the headless Wayland HDR spike reaches a real
/// Deck. The pipe carries the lab's headless session, not the user's Xorg
/// desktop, and input still goes to Xorg: it measures the future Wayland
/// provider's video path and is not a product path.
pub(crate) fn experimental_rgb10_pipe() -> Option<std::path::PathBuf> {
    std::env::var_os("ARCEN_EXPERIMENTAL_RGB10_PIPE")
        .filter(|value| !value.is_empty())
        .map(std::path::PathBuf::from)
}

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
    /// Transfer characteristics this session will encode and signal.
    ///
    /// Resolved from the Deck's request, then constrained by the active Linux
    /// desktop provider. Dedicated Xorg has no HDR composition space, so PQ
    /// and HLG requests are truthfully reduced to BT.709; a future
    /// color-managed Wayland provider may retain them.
    pub transfer: arcen_media::TransferCharacteristics,
    /// Colour primaries this session will encode and signal.
    pub color_primaries: arcen_media::ColorPrimaries,
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
    pub qp_map_config: Option<arcen_session::pier_config::QpMapConfig>,
    /// `video.desktop_encoding`: what the Xorg desktop's code values mean.
    /// Xorg cannot say, so only the operator can; `rec2100-pq` lets an HDR
    /// request keep PQ / BT.2020 (a colour-managed application such as Flame
    /// in HDR UI mode writes PQ into the desktop). Default `sdr`.
    pub desktop_encoding: arcen_media::video::DesktopSignalEncoding,
    /// An explicit `video.variant` is an operator pin and cannot be replaced by
    /// a client's adaptive request.
    pub variant_pinned: bool,
    /// Exact auth-time request retained for the post-hello consistency echo.
    pub auth_video_request: Option<InitialVideoRequestMsg>,
    pub requested_pipeline: Option<arcen_media::video::PipelineId>,
    /// This sign-in's headless Wayland desktop serves a committed
    /// multi-monitor topology: the record's wide-composition proof is a
    /// single-output one, so Grading and HDR are served as Auto
    /// (`fidelity_multi_output_unqualified`).
    pub wayland_multi_output: bool,
    /// `video.x11_desktop_depth`: the depth a new Xorg desktop is created
    /// at. Fixed for that desktop's whole life.
    pub x11_desktop_depth: arcen_media::video::DesktopDepthPolicy,
    /// The verified depth of the Xorg desktop this sign-in attached to, once
    /// known: a ten-bit request on an eight-bit desktop is served visibly as
    /// Auto. `None` before attachment and for every Wayland desktop.
    pub attached_desktop_depth: Option<arcen_media::video::DesktopDepth>,
    /// This sign-in asked for more bits than its attached desktop has and is
    /// served visibly as Auto (`eight_bit_source`); the Deck is told how to
    /// get a ten-bit desktop.
    pub desktop_depth_downgraded: bool,
    pub active_pipeline: Option<arcen_protocol::messages::ServedStreamPipeline>,
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
    /// `capture.provider`: `auto` (from host facts) unless an operator
    /// override names a provider.
    pub provider_preference: arcen_session::desktop_provider::ProviderPreference,
    /// The desktop provider start-up selection resolved from the
    /// preference and host facts; dedicated Xorg until it has run.
    pub desktop_provider: arcen_session::desktop_provider::DesktopProvider,
    /// `capture.wayland_qualification_record`: the qualification record this
    /// Pier reads (and `wayland qualify --config` writes); the production
    /// record unless a development Pier names its own.
    pub wayland_qualification_target: arcen_session::wayland_qualification::QualificationTarget,
    /// `wayland.first_login_setup`: whether GNOME's first-login assistant
    /// and welcome tour run in a headless Wayland desktop.
    pub first_login_setup: arcen_session::desktop_session::first_login::FirstLoginSetup,
    /// `wayland.lab_seat_pen` (lab, default off): a virtual tablet on a
    /// private udev seat for a headless compositor with the lab input patch.
    pub wayland_lab_seat_pen: bool,
    /// `wayland.session_mode`: native desktops on their own seat (real
    /// pen), headless ones, or native when the host proved it (`auto`).
    pub wayland_session_mode: arcen_session::native_seat::SessionMode,
    /// `capture.wayland_auto_requalify` (default on): at start, re-qualify
    /// a record an update made stale instead of only refusing.
    pub wayland_auto_requalify: bool,
    /// `capture.wayland_compositor_prefix`: the side-by-side prefix the
    /// headless Wayland compositor runs from; `None` is the distribution's
    /// `/usr`. Syntax is checked here; ownership when the Pier or `wayland
    /// qualify` applies it (`session::wayland::apply_configured_compositor_prefix`).
    pub wayland_compositor_prefix: Option<String>,
    /// The host's validated Wayland qualification record, set at start when
    /// Wayland was selected. Its measured source rates are the only
    /// `capture-rate` evidence a Wayland Speed request is admitted on.
    pub wayland_qualification: Option<Arc<arcen_session::wayland_qualification::QualifiedHost>>,
    /// The headless Wayland session's output refresh rate, set per
    /// attachment: Speed's qualified rate is per size *and* refresh.
    pub wayland_output_refresh_hz: Option<u32>,
    /// `platform.session.wayland_x11_apps`: whether Wayland sessions run the
    /// compositor's Xwayland for X11 applications.
    pub wayland_x11_apps: arcen_session::wayland_x11::X11AppsSetting,
    /// What start-up decided from that setting, the installed Xwayland and
    /// the qualification record; `None` until Wayland was selected.
    pub wayland_x11_plan: Option<arcen_session::wayland_x11::X11AppsPlan>,
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
    /// Whether the host's own speakers stay audible during a session
    /// (`audio.local_playback`, default `muted`).
    pub local_playback: arcen_session::pier_config::LocalPlayback,
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
    /// `multi_monitor_v1` advertisement gate. Defaults to advertised when
    /// startup NVIDIA-head discovery succeeds; `advertise_enabled: false`
    /// remains the administrator off switch.
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
    pub(crate) fn effective_qp_map_policy(&self) -> Option<arcen_media::video::QpMapPolicy> {
        let config = self.qp_map_config.as_ref()?;
        let served_pipeline;
        let served = if let Some(active) = self.active_pipeline.as_ref() {
            active.token()
        } else {
            served_pipeline = self.requested_pipeline?.served_wire();
            served_pipeline.token()
        };
        let token = config.effective_token(served).ok()?;
        arcen_media::video::QpMapPolicy::from_token(token)
    }

    pub(crate) fn requested_encode_intent(&self) -> arcen_media::EncodeIntent {
        if let Some(active) = self.active_pipeline.as_ref() {
            if let Some(pipeline) = arcen_media::video::PipelineId::from_served_wire(active) {
                return arcen_media::video::pipeline_contract(pipeline).intent;
            }
        } else if let Some(pipeline) = self.requested_pipeline {
            return arcen_media::video::pipeline_contract(pipeline).intent;
        }
        self.auth_video_request
            .as_ref()
            .and_then(|request| {
                arcen_media::EncodeIntent::from_token(&request.quality.encode_intent)
            })
            .unwrap_or_default()
    }

    pub(crate) fn requested_motion_priority(&self) -> arcen_media::video::MotionPriority {
        if let Some(active) = self.active_pipeline.as_ref() {
            if let Some(pipeline) = arcen_media::video::PipelineId::from_served_wire(active) {
                return arcen_media::video::pipeline_contract(pipeline).priority;
            }
        } else if let Some(pipeline) = self.requested_pipeline {
            return arcen_media::video::pipeline_contract(pipeline).priority;
        }
        self.auth_video_request
            .as_ref()
            .and_then(|request| {
                arcen_media::video::MotionPriority::from_token(&request.quality.motion_priority)
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
        let capture_rates_by_codec =
            self.wayland_speed_capture_rates(execution.as_ref(), width, height);
        CapencConfig {
            binary,
            output_index: self.output_index(),
            codec: self.codec.clone(),
            encoder: self.session_encoder(),
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
            transfer: self.transfer,
            color_primaries: self.color_primaries,
            desktop_encoding: self.desktop_encoding,
            video_selection: self.video_selection,
            codec_pinned: self.codec_pinned,
            variant_pinned: self.variant_pinned,
            intent: self.requested_encode_intent(),
            motion_priority: self.requested_motion_priority(),
            qp_map: self.effective_qp_map_policy().unwrap_or(self.qp_map),
            debug_diagnostics: self.logging.resolved_profile().is_ok_and(|resolved| {
                resolved
                    .profile
                    .includes(arcen_telemetry::OperationalProfile::Debug)
            }),
            encoder_max_bitrate_bps: self
                .active_pipeline
                .as_ref()
                .and_then(arcen_media::video::PipelineId::from_served_wire)
                .and_then(|pipeline| {
                    arcen_media::video::pipeline_contract(pipeline).encoder_ceiling_bps()
                }),
            width,
            height,
            cursor_mode,
            capture_rate: None,
            capture_rate_evidence: arcen_media::video::CaptureRateEvidence::Measured,
            capture_rates_by_codec,
            speed_runtime_shortfall: false,
            gpu_import_verified: self.wayland_gpu_import_verified(),
            wayland_fidelity_args: self.wayland_fidelity_args(),
            display: Some(self.display.clone()),
            xauthority: match execution.as_ref() {
                Some(user) => user.environment.get("XAUTHORITY").map(str::to_string),
                None => self.xauthority.clone(),
            },
            execution,
            session_log_id,
        }
    }

    /// Whether this Pier's Wayland sessions run the compositor's Xwayland:
    /// only what start-up resolved, never the raw setting.
    pub fn wayland_x11_apps_enabled(&self) -> bool {
        self.wayland_x11_plan
            .is_some_and(arcen_session::wayland_x11::X11AppsPlan::enabled)
    }

    /// The Wayland Speed decision for a stream of `width`x`height` with
    /// `codec`, on this host's record: measured on exactly this output, or
    /// inferred from a larger qualifying one (see
    /// `arcen_session::wayland_qualification::resolve_speed`). `refreshes`
    /// are the output refresh rates the session may run at.
    #[must_use]
    pub fn wayland_speed_decision(
        &self,
        width: u32,
        height: u32,
        codec: &str,
        refreshes: &[u32],
    ) -> Option<arcen_session::wayland_qualification::SpeedDecision> {
        use arcen_media::video::{pipeline_contract, PipelineId};
        let qualified = self.wayland_qualification.as_ref()?;
        let ceiling_fps = pipeline_contract(PipelineId::Speed).max_fps;
        qualified.speed_decision(&arcen_session::wayland_qualification::SpeedRequest {
            width,
            height,
            codec,
            route: qualified.serving_route(),
            candidate_refreshes: refreshes,
            ceiling_fps,
            minimum_fps: arcen_session::wayland_qualification::speed_minimum_fps(ceiling_fps),
        })
    }

    /// The Speed decision for the session's stream size (the capture
    /// hand-off's, else `width`×`height`) at its output refresh rate. An
    /// unknown refresh decides nothing. capenc ignores the rate outside a
    /// Wayland session.
    pub fn wayland_speed_capture_decision(
        &self,
        execution: Option<&UserExecution>,
        width: u32,
        height: u32,
    ) -> Option<arcen_session::wayland_qualification::SpeedDecision> {
        self.wayland_speed_capture_decision_for(execution, width, height, &self.codec)
    }

    /// [`Self::wayland_speed_capture_decision`] for `codec`.
    pub fn wayland_speed_capture_decision_for(
        &self,
        execution: Option<&UserExecution>,
        width: u32,
        height: u32,
        codec: &str,
    ) -> Option<arcen_session::wayland_qualification::SpeedDecision> {
        let refresh_hz = self.wayland_output_refresh_hz?;
        let (width, height) = execution
            .and_then(|execution| {
                arcen_media::video::capture_handoff::PipeWireCaptureHandoff::from_env(|name| {
                    execution.environment.get(name).map(str::to_owned)
                })
                .ok()
                .flatten()
            })
            .map_or((width, height), |handoff| handoff.size());
        self.wayland_speed_decision(width, height, codec, &[refresh_hz])
    }

    /// The Wayland Speed evidence for the session's stream at its output
    /// refresh, per codec a session may end up encoding with (the Deck's
    /// request decides the codec after this configuration is built). An
    /// inferred rate is logged as such.
    pub fn wayland_speed_capture_rates(
        &self,
        execution: Option<&UserExecution>,
        width: u32,
        height: u32,
    ) -> Vec<(String, u32, arcen_media::video::CaptureRateEvidence)> {
        ["h265", "h264", "av1"]
            .into_iter()
            .filter_map(|codec| {
                let decision =
                    self.wayland_speed_capture_decision_for(execution, width, height, codec)?;
                let evidence = match decision.evidence {
                    arcen_session::wayland_qualification::RateEvidence::Inferred {
                        anchor_width,
                        anchor_height,
                    } => {
                        tracing::info!(
                            target: crate::logging::target::MEDIA,
                            width,
                            height,
                            codec,
                            output_refresh_hz = decision.output_refresh_hz,
                            capture_rate = decision.capture_rate,
                            measured_fps = decision.measured_fps,
                            evidence = %decision.evidence,
                            "Wayland Speed rate is inferred from a larger qualified output"
                        );
                        arcen_media::video::CaptureRateEvidence::Inferred {
                            width: anchor_width,
                            height: anchor_height,
                        }
                    }
                    arcen_session::wayland_qualification::RateEvidence::Measured => {
                        arcen_media::video::CaptureRateEvidence::Measured
                    }
                };
                Some((codec.to_owned(), decision.capture_rate, evidence))
            })
            .collect()
    }

    /// The qualified Wayland source rate (`capture-rate=`) a Speed request
    /// is admitted on at the session's output; see
    /// [`Self::wayland_speed_capture_decision`].
    pub fn wayland_speed_capture_rate(
        &self,
        execution: Option<&UserExecution>,
        width: u32,
        height: u32,
    ) -> Option<u32> {
        self.wayland_speed_capture_decision(execution, width, height)
            .map(|decision| decision.capture_rate)
    }

    /// The refresh rate a headless Wayland session's virtual output is
    /// created at for a display of `display_refresh_hz` at `width`x`height`.
    ///
    /// A session whose authenticated request resolved to Speed gets the
    /// lowest refresh the host's record qualifies Speed at for this size
    /// (measured or inferred, for the session's codec, else any measured
    /// codec): the display's own when that holds Speed (Mutter 49's
    /// `RecordVirtual` outputs do), the shared `paced_output_refresh_hz`
    /// (120 Hz for a 60 Hz display) when only that does. Every other
    /// request, provider or host keeps the display's rate. The output is
    /// created once per session; a later Speed request on it is admitted
    /// only on evidence for that refresh.
    pub fn wayland_session_refresh_hz(
        &self,
        width: u32,
        height: u32,
        display_refresh_hz: u32,
    ) -> u32 {
        let Some(refreshes) = self.wayland_speed_refreshes(display_refresh_hz) else {
            return display_refresh_hz;
        };
        std::iter::once(self.codec.as_str())
            .chain(arcen_session::wayland_qualification::QUALIFY_CODECS)
            .find_map(|codec| self.wayland_speed_decision(width, height, codec, &refreshes))
            .map_or(display_refresh_hz, |decision| decision.output_refresh_hz)
    }

    /// The output refreshes a Speed session on a `display_refresh_hz`
    /// display may run at, or `None` when the session is not a Wayland Speed
    /// session: the display's own, then the shared paced refresh.
    fn wayland_speed_refreshes(&self, display_refresh_hz: u32) -> Option<[u32; 2]> {
        use arcen_media::video::{paced_output_refresh_hz, pipeline_contract, PipelineId};
        use arcen_session::desktop_provider::DesktopProvider;
        if self.desktop_provider != DesktopProvider::Wayland
            || self.requested_pipeline != Some(PipelineId::Speed)
        {
            return None;
        }
        let paced = paced_output_refresh_hz(
            display_refresh_hz,
            pipeline_contract(PipelineId::Speed).max_fps,
        )
        .min(arcen_session::wayland_session::MAX_VIRTUAL_OUTPUT_REFRESH_HZ);
        Some([display_refresh_hz, paced])
    }

    /// The refresh a dedicated Xorg desktop's head is created at for a
    /// display of `display_refresh_hz` at `width`x`height`.
    ///
    /// NvFBC and the compositor deliver on the head's refresh clock. A head
    /// refreshing at exactly the served rate lets arrival jitter put two
    /// frames into one encoder interval (measured: a 60 Hz head serving Speed
    /// encoded 40 of its 60 frames a second), so the head gets the same
    /// interval of headroom a headless Wayland output gets, from the shared
    /// `paced_output_refresh_hz` for the requested preset's rate: 120 Hz for
    /// Speed on a 60 Hz display, 60 Hz for a 30 fps preset on a 30 Hz one.
    /// The display's own rate is kept when it is already high enough, when
    /// an EDID cannot describe the paced timing at this size, for a request
    /// that names no preset, and for Wayland sessions (see
    /// [`Self::wayland_session_refresh_hz`]). A desktop keeps the refresh it
    /// was created at across reattachments.
    pub fn xorg_session_refresh_hz(&self, width: u32, height: u32, display_refresh_hz: u32) -> u32 {
        use arcen_media::video::{paced_output_refresh_hz, pipeline_contract};
        use arcen_session::desktop_provider::DesktopProvider;
        if self.desktop_provider != DesktopProvider::Xorg {
            return display_refresh_hz;
        }
        let Some(pipeline) = self.requested_pipeline else {
            return display_refresh_hz;
        };
        let paced =
            paced_output_refresh_hz(display_refresh_hz, pipeline_contract(pipeline).max_fps)
                .min(MAX_XORG_HEAD_REFRESH_HZ);
        if paced > display_refresh_hz
            && arcen_outputs::edid::cvt_reduced_blanking(width, height, paced).is_ok()
        {
            paced
        } else {
            display_refresh_hz
        }
    }

    /// Whether this is a Wayland Speed session the host's record has no
    /// measured or inferred rate for with any codec it may encode with: the
    /// display a qualification should measure next.
    #[must_use]
    pub fn wayland_speed_unqualified(
        &self,
        width: u32,
        height: u32,
        display_refresh_hz: u32,
    ) -> bool {
        self.wayland_qualification.is_some()
            && self
                .wayland_speed_refreshes(display_refresh_hz)
                .is_some_and(|refreshes| {
                    std::iter::once(self.codec.as_str())
                        .chain(arcen_session::wayland_qualification::QUALIFY_CODECS)
                        .all(|codec| {
                            self.wayland_speed_decision(width, height, codec, &refreshes)
                                .is_none()
                        })
                })
    }

    /// Whether every headless Wayland session on this host is served by the
    /// software encoder, which is known before a session exists: the
    /// operator pinned `software-h264`, or `auto` on a host whose
    /// qualification record proves no native encoder opened (an Intel or
    /// AMD host, a GPU without an encoder). An `nvenc` pin stays native and
    /// fails visibly, as on Xorg. Always false for Xorg sessions, whose
    /// display is fitted before its modeset instead.
    pub fn wayland_software_sessions(&self) -> bool {
        use arcen_session::desktop_provider::{CapabilityFact, DesktopProvider};
        if self.desktop_provider != DesktopProvider::Wayland {
            return false;
        }
        match self.encoder {
            EncoderSelection::SoftwareH264 => true,
            EncoderSelection::Auto => self.wayland_qualification.as_ref().is_some_and(|host| {
                host.record().proven_native_encoder() == CapabilityFact::Missing
            }),
            _ => false,
        }
    }

    /// The encoder every capenc of this host's sessions is asked for: the
    /// configured one, except that a Wayland host without a native encoder
    /// asks for software directly instead of failing each native attempt.
    pub fn session_encoder(&self) -> EncoderSelection {
        if self.wayland_software_sessions() {
            EncoderSelection::SoftwareH264
        } else {
            self.encoder
        }
    }

    /// The size a headless Wayland session's output is created at for a
    /// served display of `width`x`height`: the display's own, fitted to the
    /// software encoder's limits when software serves every session here
    /// (a Wayland output is created once, before any encoder exists).
    pub fn wayland_session_size(&self, width: u32, height: u32) -> (u32, u32) {
        if self.wayland_software_sessions() {
            crate::media::capenc::fit_to_encoder_limits(
                EncoderSelection::SoftwareH264,
                width,
                height,
            )
        } else {
            (width, height)
        }
    }

    /// Whether the host's qualification record proves capenc's GPU import
    /// (a `gpu-convert` route whose frames matched the host-copy reference
    /// and were encoded). capenc ignores it outside a Wayland session.
    pub fn wayland_gpu_import_verified(&self) -> bool {
        self.wayland_qualification
            .as_ref()
            .is_some_and(|qualified| {
                qualified.record().proven_gpu_import()
                    == arcen_session::desktop_provider::CapabilityFact::Available
            })
    }

    /// Whether the host's qualification record proves cursor metadata (a
    /// sprite bitmap on a `metadata` cursor-mode stream), which a desktop
    /// needs before it records its cursor for the Deck to draw.
    pub fn wayland_cursor_metadata_proven(&self) -> bool {
        self.wayland_qualification
            .as_ref()
            .is_some_and(|qualified| {
                qualified.record().proven_cursor_metadata()
                    == arcen_session::desktop_provider::CapabilityFact::Available
            })
    }

    /// How a headless Wayland desktop opened for `response` records its
    /// cursor (`resolve_stream_cursor_mode`), logged with its reasons.
    pub fn wayland_stream_cursor_mode(
        &self,
        response: &arcen_protocol::messages::AuthResponse,
    ) -> crate::session::wayland::StreamCursorMode {
        let proven = self.wayland_cursor_metadata_proven();
        let mode = arcen_media::video::capture_handoff::resolve_stream_cursor_mode(
            response.cursor_preference,
            response.cursor_image_v1,
            proven,
        );
        let mode =
            crate::session::wayland::StreamCursorMode::from_screencast(mode).unwrap_or_default();
        if self.desktop_provider == arcen_session::desktop_provider::DesktopProvider::Wayland {
            tracing::info!(
                target: crate::logging::target::SESSION,
                cursor_preference = ?response.cursor_preference,
                deck_cursor_images = response.cursor_image_v1,
                cursor_metadata_proven = proven,
                stream_cursor_mode = mode.token(),
                "cursor mode for a Wayland desktop this connection opens (a reattached desktop keeps its own)"
            );
        }
        mode
    }

    /// The host's wide-composition proof, on a Wayland host whose record
    /// carries one.
    fn wayland_fidelity_proof(
        &self,
    ) -> Option<&arcen_session::wayland_qualification::FidelityProof> {
        use arcen_session::desktop_provider::DesktopProvider;
        if self.desktop_provider != DesktopProvider::Wayland {
            return None;
        }
        self.wayland_qualification
            .as_ref()
            .and_then(|qualified| qualified.record().fidelity())
    }

    /// How a new Wayland desktop gets its seat (`wayland.session_mode` and
    /// the record's native-seat proof). An Xorg desktop is never native.
    #[must_use]
    pub fn desktop_seat_plan(&self) -> arcen_session::native_seat::DesktopSeatPlan {
        use arcen_session::native_seat::{plan_desktop_seat, DesktopSeatPlan};
        if self.desktop_provider != arcen_session::desktop_provider::DesktopProvider::Wayland {
            return DesktopSeatPlan::Headless { fallback: None };
        }
        plan_desktop_seat(
            self.wayland_session_mode,
            self.wayland_qualification
                .as_deref()
                .and_then(arcen_session::wayland_qualification::QualifiedHost::native_seat),
        )
    }

    /// The route to a wide composition a new headless Wayland session
    /// starts with: only when the authenticated request resolved to Grading
    /// or HDR and the host's record proves that preset on this route
    /// (`force-hdr` on Mutter 50 and later). Every other session keeps the
    /// compositor's default colour state, so Auto and Speed are untouched.
    pub fn wayland_fidelity_route(&self) -> Option<&str> {
        use arcen_media::video::PipelineId;
        use arcen_session::desktop_provider::CapabilityFact;
        let facts = &self.wayland_qualification.as_ref()?.record().facts;
        let proven = match self.requested_pipeline? {
            PipelineId::Grading => facts.grading,
            PipelineId::Hdr => facts.hdr,
            _ => return None,
        };
        // `requested_pipeline` is what `wayland_fidelity_fallback` left: an
        // eight-bit pin was already served as Auto there.
        (proven == CapabilityFact::Available)
            .then(|| self.wayland_fidelity_proof()?.route.as_deref())
            .flatten()
    }

    /// The capenc tokens that carry the record's wide-composition proof to
    /// a ten-bit Wayland epoch (`fidelity-route=`, `wide-proven=`,
    /// `sdr-white=`): nothing the record does not prove.
    pub fn wayland_fidelity_args(&self) -> Vec<String> {
        let Some(proof) = self.wayland_fidelity_proof() else {
            return Vec::new();
        };
        let mut args = Vec::new();
        if let Some(route) = proof.route.as_deref() {
            args.push(format!("fidelity-route={route}"));
        }
        // Format and memory together: capenc offers only these formats and
        // trusts a frame's precision only for its own pair.
        let mut pairs = Vec::new();
        for ramp in proof.proven() {
            let pair = format!("{}@{}", ramp.format, ramp.memory);
            if !pairs.contains(&pair) {
                pairs.push(pair);
            }
        }
        if !pairs.is_empty() {
            args.push(format!("wide-proven={}", pairs.join(",")));
        }
        if let Some(nits) = proof.sdr_white_nits {
            args.push(format!("sdr-white={nits}"));
        }
        args
    }

    /// A headless Wayland session serves Grading and HDR only when the
    /// host's qualification record proves them (a wide composition route,
    /// a ramp read back with native-wide precision through the wide
    /// capture, SDR white at 203 nits). Otherwise the request is served as
    /// the best proven contract, visibly — HDR as Grading when only Grading
    /// is proven, else Auto — with the record's reason (for example
    /// `compositor_lacks_hdr_screencast` on Mutter 49, which has no HDR
    /// screen sharing), never failing the session.
    fn wayland_fidelity_fallback(
        &self,
        resolved: arcen_media::video::ResolvedHostInitialVideo,
    ) -> arcen_media::video::ResolvedHostInitialVideo {
        use arcen_media::video::{pipeline_contract, PipelineId};
        use arcen_session::desktop_provider::{CapabilityFact, DesktopProvider};
        if self.desktop_provider != DesktopProvider::Wayland {
            return resolved;
        }
        // An administrator's pinned eight-bit variant cannot be served as a
        // ten-bit contract: no wide route, no PQ desktop, the pin kept and
        // the session visibly Auto.
        if matches!(
            resolved.pipeline,
            Some(PipelineId::Grading | PipelineId::Hdr)
        ) && resolved.video.bit_depth != arcen_media::BitDepth::Ten
        {
            tracing::info!(
                target: crate::logging::target::MEDIA,
                requested = resolved.pipeline.map_or("none", PipelineId::token),
                served = PipelineId::Auto.token(),
                decision = "downgrade",
                reason = "variant_pinned_eight_bit",
                pinned_variant = %arcen_media::video::VideoVariant::new(resolved.video).id(),
                "the administrator's pinned eight-bit variant keeps this Wayland session out of the ten-bit Grading and HDR contracts; served visibly as Auto"
            );
            return arcen_media::video::ResolvedHostInitialVideo {
                pipeline: Some(PipelineId::Auto),
                ..resolved
            };
        }
        let facts = self
            .wayland_qualification
            .as_ref()
            .map(|qualified| &qualified.record().facts);
        // A fact without the proof's route (a record written without the
        // fidelity section) satisfies nothing.
        let routed = self
            .wayland_fidelity_proof()
            .is_some_and(|proof| proof.route.is_some());
        let proven =
            |fact: Option<CapabilityFact>| routed && fact == Some(CapabilityFact::Available);
        let gap = self
            .wayland_fidelity_proof()
            .and_then(|proof| proof.gap.clone());
        let (requested, served_pipeline, reason) = match resolved.pipeline {
            // The proof covers one output; nothing proves several at once.
            Some(pipeline @ (PipelineId::Grading | PipelineId::Hdr))
                if self.wayland_multi_output =>
            {
                (
                    pipeline,
                    PipelineId::Auto,
                    "fidelity_multi_output_unqualified".to_owned(),
                )
            }
            Some(PipelineId::Grading) if !proven(facts.map(|facts| facts.grading)) => (
                PipelineId::Grading,
                PipelineId::Auto,
                gap.unwrap_or_else(|| "eight_bit_source".to_owned()),
            ),
            Some(PipelineId::Hdr) if !proven(facts.map(|facts| facts.hdr)) => {
                if proven(facts.map(|facts| facts.grading)) {
                    (
                        PipelineId::Hdr,
                        PipelineId::Grading,
                        "hdr_output_not_proven".to_owned(),
                    )
                } else {
                    (
                        PipelineId::Hdr,
                        PipelineId::Auto,
                        gap.unwrap_or_else(|| "hdr_on_eight_bit_source".to_owned()),
                    )
                }
            }
            _ => return resolved,
        };
        let contract = pipeline_contract(served_pipeline);
        let served = arcen_media::video::ResolvedHostInitialVideo {
            video: arcen_media::VideoConfiguration {
                codec: resolved.video.codec,
                ..contract.colour
            },
            selection: contract.selection,
            pipeline: Some(served_pipeline),
            encode_intent: contract.intent,
            max_fps: resolved.max_fps.min(contract.max_fps),
        };
        tracing::warn!(
            target: crate::logging::target::MEDIA,
            requested = requested.token(),
            served = served_pipeline.token(),
            decision = "downgrade",
            reason = %reason,
            detail = self
                .wayland_fidelity_proof()
                .and_then(|proof| proof.detail.as_deref())
                .unwrap_or("the host's qualification record proves no Wayland wide composition"),
            requested_variant = %arcen_media::video::VideoVariant::new(resolved.video).id(),
            served_variant = %arcen_media::video::VideoVariant::new(served.video).id(),
            fps = served.max_fps,
            "the headless Wayland session cannot serve the requested preset; served visibly as another"
        );
        served
    }

    /// A ten-bit plan on an attached eight-bit Xorg desktop is served
    /// visibly as Auto: the desktop's depth is fixed for its life, and its
    /// samples cannot honestly fill a ten-bit stream.
    fn fixed_desktop_depth_fallback(
        &self,
        resolved: arcen_media::video::ResolvedHostInitialVideo,
    ) -> (arcen_media::video::ResolvedHostInitialVideo, bool) {
        let Some(depth) = self.attached_desktop_depth else {
            return (resolved, false);
        };
        match arcen_media::video::serve_on_fixed_depth(depth, resolved) {
            arcen_media::video::FixedDepthOutcome::Served(resolved) => (resolved, false),
            arcen_media::video::FixedDepthOutcome::Downgraded {
                served,
                requested,
                reason,
            } => {
                tracing::warn!(
                    target: crate::logging::target::MEDIA,
                    requested = requested.map_or("custom", arcen_media::video::PipelineId::token),
                    served = arcen_media::video::PipelineId::Auto.token(),
                    decision = "downgrade",
                    reason = reason.token(),
                    desktop_depth = depth.x11_depth(),
                    requested_variant = %arcen_media::video::VideoVariant::new(resolved.video).id(),
                    served_variant = %arcen_media::video::VideoVariant::new(served.video).id(),
                    remedy = "sign out of the remote desktop and reconnect with Grading or HDR, or set video.x11_desktop_depth to 30",
                    "this persistent Xorg desktop was started at eight bits and cannot serve a ten-bit preset; served visibly as Auto"
                );
                (served, true)
            }
        }
    }

    /// The depth a new Xorg desktop created by this sign-in starts at, from
    /// the plan it resolved (`apply_initial_video_request` first).
    #[must_use]
    pub fn new_x11_desktop_depth(&self) -> arcen_media::video::DepthDecision {
        arcen_media::video::choose_desktop_depth(
            self.x11_desktop_depth,
            self.session_bit_depth(),
            self.desktop_encoding,
        )
    }

    /// Whether this sign-in, attached to a desktop of `depth`, resolved a
    /// plan that desktop cannot serve.
    #[must_use]
    pub fn exceeds_desktop_depth(&self, depth: arcen_media::video::DesktopDepth) -> bool {
        self.session_bit_depth() > depth.max_bit_depth()
    }

    /// The component depth this sign-in's first capture is started at: the
    /// same resolution `capenc_config` applies.
    fn session_bit_depth(&self) -> arcen_media::BitDepth {
        self.color_policy.resolve_bit_depth(self.bit_depth, None)
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
            primaries: self.color_primaries,
            transfer: self.transfer,
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
        let resolved = self.wayland_fidelity_fallback(resolved);
        let (resolved, desktop_depth_downgraded) = self.fixed_desktop_depth_fallback(resolved);
        self.desktop_depth_downgraded = desktop_depth_downgraded;
        self.auth_video_request = Some(request.clone());
        self.requested_pipeline = resolved.pipeline;
        // The lab pipe carries compositor-tagged PQ, so it vouches for itself,
        // but only PQ sessions read it; every other session reads Xorg.
        // A Grading or HDR session on a qualified Wayland host is composed by
        // the compositor in BT.2100 and screencast as tagged PQ / BT.2020:
        // HDR keeps PQ, Grading is converted to BT.709 by capenc.
        let desktop = if (experimental_rgb10_pipe().is_some()
            && resolved.video.transfer == arcen_media::TransferCharacteristics::Pq)
            || self.wayland_fidelity_route().is_some()
        {
            arcen_media::video::DesktopSignalEncoding::Rec2100Pq
        } else {
            self.desktop_encoding
        };
        let plan = arcen_media::video::resolve_desktop_plan(resolved.video, desktop)
            .map_err(|error| error.to_string())?;
        self.active_pipeline = Some(arcen_media::video::served_pipeline(
            self.requested_pipeline,
            plan.video,
            resolved.max_fps,
            client.motion_priority,
            arcen_media::video::ServedPipelineContext {
                backend: Some(arcen_media::video::AcceleratorClass::Hardware),
                exact_or_admin_override: self.codec_pinned
                    || self.variant_pinned
                    || self.requested_pipeline.is_none(),
            },
        ));
        if plan.conversion != arcen_media::video::Rgb10Signal::Direct {
            tracing::info!(
                target: crate::logging::target::MEDIA,
                desktop_encoding = plan.source.token(),
                conversion = ?plan.conversion,
                "the desktop carries Rec.2100 PQ; this SDR session converts it to BT.709"
            );
        }
        // This per-session copy carries the source the plan was resolved
        // against, which is what capenc must be told.
        self.desktop_encoding = plan.source;
        let video = plan.video;
        self.fps = resolved.max_fps;
        self.codec = video.codec.token().to_string();
        self.chroma = video.chroma.token().to_string();
        self.bit_depth = video.bit_depth;
        self.color_range = video.range;
        self.color_matrix = video.matrix;
        // Written back like every other axis. Omitting these is what made the
        // host serve BT.709 for a PQ request even once the request carried it.
        self.transfer = video.transfer;
        self.color_primaries = video.primaries;
        if !self.variant_pinned {
            self.color_policy = ColorPolicy::AlwaysOn;
        }
        self.video_selection = resolved.selection;
        Ok(())
    }

    /// What a Pier serving headless Wayland sessions can serve as virtual
    /// outputs, from the compositor version and the concurrent outputs its
    /// valid qualification record proved; `None` on an Xorg Pier or without
    /// a record.
    #[must_use]
    pub fn multi_monitor_virtual_outputs(
        &self,
    ) -> Option<crate::session::multi_monitor::VirtualOutputGate> {
        if self.desktop_provider != arcen_session::desktop_provider::DesktopProvider::Wayland {
            return None;
        }
        let host = self.wayland_qualification.as_ref()?;
        let mode = arcen_session::wayland_outputs::VirtualOutputMode::for_compositor_major(
            arcen_session::wayland_outputs::parse_compositor_major(
                &host.record().host.stack.compositor_version,
            ),
        );
        Some(crate::session::multi_monitor::VirtualOutputGate::new(
            mode,
            host.max_concurrent_outputs(),
        ))
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

    pub fn resolved_multi_monitor_encoder(&self) -> EncoderSelection {
        if self.multi_monitor_virtual_outputs().is_some() {
            return self.wayland_multi_monitor_encoder();
        }
        if self.multi_monitor.advertise_enabled
            && self.encoder == EncoderSelection::Auto
            && !self.multi_monitor.heads.is_empty()
        {
            EncoderSelection::NativeNvenc
        } else {
            self.encoder
        }
    }

    /// The concrete encoder a Wayland multi-monitor roster is planned with:
    /// the operator's pin, or for `auto` the encoder this host's Wayland
    /// sessions encode with ([`Self::session_encoder`]: software when the
    /// qualification record proves no native encoder, native NVENC
    /// otherwise). Every multi-monitor plan needs a concrete encoder (`auto`
    /// could fall back per monitor); the capacity and admission seam
    /// (`media::encoder_admission`, with the software rules
    /// `session::multi_monitor` applies to `SoftwareH264`) takes whatever
    /// this returns.
    #[must_use]
    pub fn wayland_multi_monitor_encoder(&self) -> EncoderSelection {
        match self.session_encoder() {
            EncoderSelection::Auto => EncoderSelection::NativeNvenc,
            concrete => concrete,
        }
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
            transfer: arcen_media::TransferCharacteristics::Bt709,
            color_primaries: arcen_media::ColorPrimaries::Bt709,
            color_policy: ColorPolicy::DefaultOff,
            video_selection: VideoSelectionIntent::Exact,
            codec_pinned: false,
            qp_map: arcen_media::video::QpMapPolicy::default(),
            qp_map_config: None,
            desktop_encoding: arcen_media::video::DesktopSignalEncoding::default(),
            variant_pinned: false,
            auth_video_request: None,
            requested_pipeline: None,
            wayland_multi_output: false,
            x11_desktop_depth: arcen_media::video::DesktopDepthPolicy::default(),
            attached_desktop_depth: None,
            desktop_depth_downgraded: false,
            active_pipeline: None,
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
            provider_preference: arcen_session::desktop_provider::ProviderPreference::Auto,
            desktop_provider: arcen_session::desktop_provider::DesktopProvider::Xorg,
            wayland_qualification_target:
                arcen_session::wayland_qualification::QualificationTarget::production(),
            first_login_setup: arcen_session::desktop_session::first_login::FirstLoginSetup::Run,
            wayland_lab_seat_pen: false,
            wayland_session_mode: arcen_session::native_seat::SessionMode::Auto,
            wayland_auto_requalify: true,
            wayland_compositor_prefix: None,
            wayland_qualification: None,
            wayland_output_refresh_hz: None,
            wayland_x11_apps: arcen_session::wayland_x11::X11AppsSetting::Auto,
            wayland_x11_plan: None,
            session_display: ":10".to_string(),
            session_gpu_head: "DFP-1".to_string(),
            xorg_bin: PathBuf::from("/usr/libexec/Xorg"),
            xorg_config_template: PathBuf::from("/run/arcen/xorg.conf"),
            session_runtime_root: PathBuf::from("/run/arcen/sessions"),
            input_mode: InputMode::None,
            audio_enabled: false,
            audio_compressed: false,
            local_playback: arcen_session::pier_config::LocalPlayback::default(),
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
        cfg.qp_map_config = None;
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
            validate_gpu_head(head)
                .map_err(|_| format!("--multi-monitor-head must be DFP-N, got {head}"))?;
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
        && cfg.encoder == EncoderSelection::WindowsMediaFoundation
    {
        return Err(format!(
            "platform.multi_monitor.advertise_enabled requires an explicit encoder pin \
            (nvenc or software-h264) or auto with discovered NVIDIA heads; {} is not permitted for a multi-monitor session, because \
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

    cfg.provider_preference = file
        .capture
        .desktop_provider_for(arcen_session::desktop_provider::PierPlatform::Linux {
            wayland_adapter_built: crate::session::wayland::WAYLAND_ADAPTER_BUILT,
        })
        .map_err(|error| format!("Pier config: {error}"))?;
    cfg.wayland_qualification_target =
        arcen_session::wayland_qualification::QualificationTarget::from_config(
            file.capture.wayland_qualification_record.as_deref(),
        )
        .map_err(|error| format!("Pier config capture.wayland_qualification_record: {error}"))?;
    cfg.first_login_setup = file.wayland.first_login_setup;
    cfg.wayland_lab_seat_pen = file.wayland.lab_seat_pen;
    cfg.wayland_session_mode = file.wayland.session_mode;
    cfg.wayland_auto_requalify = file.capture.wayland_auto_requalify.unwrap_or(true);
    if let Some(prefix) = file.capture.wayland_compositor_prefix.as_deref() {
        arcen_session::compositor_prefix::CompositorPrefix::parse(prefix)
            .map_err(|error| format!("Pier config capture.wayland_compositor_prefix: {error}"))?;
    }
    cfg.wayland_compositor_prefix = file.capture.wayland_compositor_prefix;
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
        let effective = value.effective_token("auto")?;
        cfg.qp_map = arcen_media::video::QpMapPolicy::from_token(effective).unwrap_or_default();
        cfg.qp_map_config = Some(value);
    }
    if let Some(value) = file.video.desktop_encoding {
        cfg.desktop_encoding =
            arcen_media::video::DesktopSignalEncoding::from_token(&value.to_ascii_lowercase())
                .ok_or_else(|| {
                    let known = arcen_media::video::DesktopSignalEncoding::ALL
                        .iter()
                        .map(|encoding| encoding.token())
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!("Pier config video.desktop_encoding {value:?}: expected one of {known}")
                })?;
    }
    if let Some(value) = file.video.x11_desktop_depth {
        cfg.x11_desktop_depth =
            arcen_media::video::DesktopDepthPolicy::from_token(&value.to_ascii_lowercase())
                .ok_or_else(|| {
                    let known = arcen_media::video::DesktopDepthPolicy::TOKENS.join(", ");
                    format!(
                        "Pier config video.x11_desktop_depth {value:?}: expected one of {known}"
                    )
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
    cfg.local_playback = file.audio.local_playback;
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
    if let Some(value) = platform.session.wayland_x11_apps {
        cfg.wayland_x11_apps = value;
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
        validate_gpu_head(head)
            .map_err(|_| format!("platform.multi_monitor.heads must be DFP-N, got {head}"))?;
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
    arcen_outputs::is_nvidia_dfp_head_token(head)
        .then_some(())
        .ok_or_else(|| "--session-gpu-head must be DFP-N".to_string())
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

    #[test]
    fn a_wayland_speed_decision_rests_on_measured_or_inferred_evidence() {
        use arcen_session::desktop_provider::CapabilityFact;
        use arcen_session::wayland_qualification::{
            CaptureRoute, GpuDriver, HostIdentity, OsRelease, PierBuild, QualifiedFacts,
            RateEvidence, SourceRateMeasurement, StackVersions, WaylandQualificationRecord,
            SCHEMA_VERSION,
        };
        let rate = |width, height, refresh_hz, fps| SourceRateMeasurement {
            width,
            height,
            display_refresh_hz: 60,
            refresh_hz,
            codec: "h265".into(),
            route: CaptureRoute::HostCopy,
            sustained_source_fps: Some(fps),
            seconds: 8,
            evidence: RateEvidence::Measured,
        };
        let host = HostIdentity {
            pier: PierBuild {
                version: "0.17.0".into(),
                revision: "test".into(),
                executable_sha256: "ab".repeat(32),
            },
            stack: StackVersions {
                compositor: arcen_session::wayland_session::HeadlessCompositor::GnomeShell,
                compositor_version: "49.4".into(),
                compositor_prefix: None,
                pipewire_version: "1.4.11".into(),
                gpu_driver: Some(GpuDriver {
                    vendor: "nvidia".into(),
                    version: "580.105.08".into(),
                    drm: true,
                    modeset: Some(true),
                }),
            },
        };
        let record = WaylandQualificationRecord {
            schema_version: SCHEMA_VERSION,
            qualified_at_unix_s: 0,
            host: host.clone(),
            os: OsRelease {
                id: "test".into(),
                version_id: "1".into(),
                kernel: "6".into(),
            },
            facts: QualifiedFacts {
                capture_route: None,
                input: None,
                source_rates: vec![rate(1920, 1080, 120, 59), rate(2560, 1600, 60, 60)],
                audio_monitor: CapabilityFact::Unknown,
                grading: CapabilityFact::Missing,
                hdr: CapabilityFact::Missing,
                x11_apps: None,
                cursor_metadata: CapabilityFact::Unknown,
                native_seat: None,
                multi_output: None,
                fidelity: None,
                encoder: None,
            },
        };
        {
            use crate::session::wayland::StreamCursorMode;
            use arcen_protocol::messages::{AuthResponse, CursorMode};
            let mut proven = record.clone();
            proven.facts.cursor_metadata = CapabilityFact::Available;
            let wayland = |record: &WaylandQualificationRecord| Config {
                wayland_qualification: Some(Arc::new(record.clone().validate(&host).unwrap())),
                desktop_provider: arcen_session::desktop_provider::DesktopProvider::Wayland,
                ..Config::default()
            };
            let deck = AuthResponse::none().with_cursor_image_v1(true);
            assert!(wayland(&proven).wayland_cursor_metadata_proven());
            assert_eq!(
                wayland(&proven).wayland_stream_cursor_mode(&deck),
                StreamCursorMode::Metadata
            );
            assert_eq!(
                wayland(&record).wayland_stream_cursor_mode(&deck),
                StreamCursorMode::Hidden,
                "unproven metadata leaves the cursor to the Deck, out of the video"
            );
            assert_eq!(
                wayland(&proven).wayland_stream_cursor_mode(&AuthResponse::none()),
                StreamCursorMode::Hidden,
                "a Deck without cursor images draws its own, never a second in the video"
            );
            assert_eq!(
                wayland(&proven).wayland_stream_cursor_mode(
                    &deck.clone().with_cursor_preference(CursorMode::Host)
                ),
                StreamCursorMode::Embedded
            );
        }
        let config = |refresh_hz: Option<u32>| Config {
            wayland_qualification: Some(Arc::new(record.clone().validate(&host).unwrap())),
            wayland_output_refresh_hz: refresh_hz,
            codec: "h265".into(),
            ..Config::default()
        };
        // 59 holds Speed's floor: the contract's 60 fps is admitted.
        assert_eq!(
            config(Some(120)).wayland_speed_capture_rate(None, 1920, 1080),
            Some(60)
        );
        // At 60 Hz this size was not measured; the larger 2560x1600 output
        // was, at 60 Hz, so it is inferred and says so.
        let inferred = config(Some(60))
            .wayland_speed_capture_decision(None, 1920, 1080)
            .unwrap();
        assert_eq!(inferred.evidence.token(), "inferred:2560x1600");
        // Never inferred upward or for another codec.
        assert_eq!(
            config(Some(60)).wayland_speed_capture_rate(None, 3840, 2160),
            None
        );
        assert_eq!(
            Config {
                codec: "h264".into(),
                ..config(Some(60))
            }
            .wayland_speed_capture_rate(None, 1920, 1080),
            None
        );
        // Without the session's refresh nothing qualifies.
        assert_eq!(
            config(None).wayland_speed_capture_rate(None, 1920, 1080),
            None
        );

        // The session's output: the lowest refresh with evidence, for a
        // Speed request on Wayland; a measurement beats an inference.
        use arcen_media::video::PipelineId;
        use arcen_session::desktop_provider::DesktopProvider;
        let session = |provider, pipeline| Config {
            desktop_provider: provider,
            requested_pipeline: pipeline,
            ..config(None)
        };
        let speed = session(DesktopProvider::Wayland, Some(PipelineId::Speed));
        assert_eq!(speed.wayland_session_refresh_hz(1920, 1080, 60), 120);
        assert_eq!(speed.wayland_session_refresh_hz(1800, 1130, 60), 60);
        assert_eq!(speed.wayland_session_refresh_hz(1920, 1080, 144), 144);
        // No evidence at this size: the display's rate, and it is the
        // display the next qualification measures.
        assert_eq!(speed.wayland_session_refresh_hz(3840, 2160, 60), 60);
        assert!(speed.wayland_speed_unqualified(3840, 2160, 60));
        assert!(!speed.wayland_speed_unqualified(1800, 1130, 60));
        assert!(!session(DesktopProvider::Wayland, Some(PipelineId::Auto))
            .wayland_speed_unqualified(3840, 2160, 60));
        for (provider, pipeline) in [
            (DesktopProvider::Wayland, Some(PipelineId::Auto)),
            (DesktopProvider::Wayland, None),
            (DesktopProvider::Xorg, Some(PipelineId::Speed)),
        ] {
            assert_eq!(
                session(provider, pipeline).wayland_session_refresh_hz(1920, 1080, 60),
                60,
                "{provider:?} {pipeline:?}"
            );
        }
        let unqualified = Config {
            wayland_qualification: None,
            ..speed
        };
        assert_eq!(unqualified.wayland_session_refresh_hz(1920, 1080, 60), 60);
        // GPU import is proven only by a verified GPU route.
        assert!(!config(None).wayland_gpu_import_verified());

        // A record that proves no native encoder: every Wayland session is
        // software, its output fitted to OpenH264 before it exists.
        let mut software_record = record.clone();
        software_record.facts.encoder = Some(arcen_session::wayland_qualification::EncoderProof {
            native: CapabilityFact::Missing,
            capture_route_encoder: Some("openh264-sw-h264".into()),
        });
        let software_host = Config {
            desktop_provider: DesktopProvider::Wayland,
            wayland_qualification: Some(Arc::new(software_record.validate(&host).unwrap())),
            ..Config::default()
        };
        assert_eq!(software_host.encoder, EncoderSelection::Auto);
        assert!(software_host.wayland_software_sessions());
        assert_eq!(
            software_host.session_encoder(),
            EncoderSelection::SoftwareH264
        );
        assert_eq!(software_host.wayland_session_size(2560, 1600), (1920, 1200));
        assert_eq!(software_host.wayland_session_size(1800, 1130), (1800, 1130));
        // Its multi-monitor roster is planned on the same encoder.
        assert_eq!(
            software_host.wayland_multi_monitor_encoder(),
            EncoderSelection::SoftwareH264
        );
        assert_eq!(
            config(None).wayland_multi_monitor_encoder(),
            EncoderSelection::NativeNvenc
        );
        // An nvenc pin stays native (and fails visibly), as on Xorg.
        let pinned = Config {
            encoder: EncoderSelection::NativeNvenc,
            ..software_host.clone()
        };
        assert!(!pinned.wayland_software_sessions());
        assert_eq!(
            pinned.wayland_multi_monitor_encoder(),
            EncoderSelection::NativeNvenc
        );
        assert_eq!(pinned.wayland_session_size(2560, 1600), (2560, 1600));
        // A software pin on Wayland fits too; on Xorg nothing changes here.
        let wayland_pin = Config {
            encoder: EncoderSelection::SoftwareH264,
            desktop_provider: DesktopProvider::Wayland,
            ..Config::default()
        };
        assert!(wayland_pin.wayland_software_sessions());
        let xorg_pin = Config {
            encoder: EncoderSelection::SoftwareH264,
            desktop_provider: DesktopProvider::Xorg,
            ..Config::default()
        };
        assert!(!xorg_pin.wayland_software_sessions());
        assert_eq!(xorg_pin.wayland_session_size(2560, 1600), (2560, 1600));
        assert_eq!(xorg_pin.session_encoder(), EncoderSelection::SoftwareH264);
        // A record with a native encoder, or none read yet, stays native.
        let native = Config {
            desktop_provider: DesktopProvider::Wayland,
            ..config(None)
        };
        assert!(!native.wayland_software_sessions());
        assert_eq!(native.session_encoder(), EncoderSelection::Auto);
    }

    #[test]
    fn a_new_xorg_head_runs_at_the_paced_refresh_for_the_requested_preset() {
        use arcen_media::video::PipelineId;
        use arcen_session::desktop_provider::DesktopProvider;
        let session = |provider, pipeline| Config {
            desktop_provider: provider,
            requested_pipeline: pipeline,
            ..Config::default()
        };
        let speed = session(DesktopProvider::Xorg, Some(PipelineId::Speed));
        assert_eq!(speed.xorg_session_refresh_hz(1800, 1168, 60), 120);
        assert_eq!(speed.xorg_session_refresh_hz(2560, 1440, 30), 120);
        // Already fast enough: the display's own.
        assert_eq!(speed.xorg_session_refresh_hz(1920, 1080, 144), 144);
        // An EDID cannot describe 4K at 120 Hz: the display's rate.
        assert_eq!(speed.xorg_session_refresh_hz(3840, 2160, 60), 60);
        // A 30 fps preset on a 30 Hz display gets 60 Hz; on 60 Hz, 60.
        for pipeline in [PipelineId::Auto, PipelineId::Grading] {
            let session = session(DesktopProvider::Xorg, Some(pipeline));
            assert_eq!(session.xorg_session_refresh_hz(2560, 1440, 30), 60);
            assert_eq!(session.xorg_session_refresh_hz(1800, 1168, 60), 60);
        }
        // No preset named, or a Wayland session: the display's rate.
        assert_eq!(
            session(DesktopProvider::Xorg, None).xorg_session_refresh_hz(1800, 1168, 60),
            60
        );
        assert_eq!(
            session(DesktopProvider::Wayland, Some(PipelineId::Speed))
                .xorg_session_refresh_hz(1800, 1168, 60),
            60
        );
    }

    #[test]
    fn a_wayland_session_serves_grading_and_hdr_as_auto() {
        use arcen_media::video::{pipeline_contract, PipelineId, ResolvedHostInitialVideo};
        let requested = |pipeline: PipelineId| ResolvedHostInitialVideo {
            video: pipeline_contract(pipeline).colour,
            selection: pipeline_contract(pipeline).selection,
            pipeline: Some(pipeline),
            encode_intent: pipeline_contract(pipeline).intent,
            max_fps: pipeline_contract(pipeline).max_fps,
        };
        let xorg = Config::default();
        let wayland = Config {
            desktop_provider: arcen_session::desktop_provider::DesktopProvider::Wayland,
            ..Config::default()
        };
        let auto = pipeline_contract(PipelineId::Auto);
        for pipeline in [PipelineId::Grading, PipelineId::Hdr] {
            let served = wayland.wayland_fidelity_fallback(requested(pipeline));
            assert_eq!(served.pipeline, Some(PipelineId::Auto), "{pipeline:?}");
            assert_eq!(served.video.bit_depth, auto.colour.bit_depth);
            assert_eq!(served.video.transfer, auto.colour.transfer);
            assert_eq!(served.video.chroma, auto.colour.chroma);
            assert_eq!(served.video.codec, requested(pipeline).video.codec);
            assert!(served.max_fps <= auto.max_fps);
            // Xorg keeps its own Grading and HDR paths.
            assert_eq!(
                xorg.wayland_fidelity_fallback(requested(pipeline)),
                requested(pipeline)
            );
        }
        for pipeline in [PipelineId::Auto, PipelineId::Speed] {
            assert_eq!(
                wayland.wayland_fidelity_fallback(requested(pipeline)),
                requested(pipeline)
            );
        }
    }

    #[test]
    fn a_qualified_wayland_host_serves_grading_and_hdr_on_its_route() {
        use arcen_media::video::{pipeline_contract, PipelineId, ResolvedHostInitialVideo};
        use arcen_session::desktop_provider::{CapabilityFact, DesktopProvider};
        use arcen_session::wayland_qualification::{
            FidelityProof, HostIdentity, OsRelease, PierBuild, QualifiedFacts, RampProof,
            StackVersions, WaylandQualificationRecord, SCHEMA_VERSION,
        };
        let host = HostIdentity {
            pier: PierBuild {
                version: "0.17.0".into(),
                revision: "test".into(),
                executable_sha256: "ab".repeat(32),
            },
            stack: StackVersions {
                compositor: arcen_session::wayland_session::HeadlessCompositor::GnomeShell,
                compositor_version: "50.6".into(),
                compositor_prefix: None,
                pipewire_version: "1.6.2".into(),
                gpu_driver: None,
            },
        };
        let record = |grading, hdr, fidelity| WaylandQualificationRecord {
            schema_version: SCHEMA_VERSION,
            qualified_at_unix_s: 0,
            host: host.clone(),
            os: OsRelease {
                id: "test".into(),
                version_id: "1".into(),
                kernel: "6".into(),
            },
            facts: QualifiedFacts {
                capture_route: None,
                input: None,
                source_rates: Vec::new(),
                audio_monitor: CapabilityFact::Unknown,
                grading,
                hdr,
                fidelity,
                multi_output: None,
                x11_apps: None,
                encoder: None,
                cursor_metadata: CapabilityFact::Unknown,
                native_seat: None,
            },
        };
        let proof = FidelityProof {
            route: Some("force-hdr".into()),
            gap: None,
            detail: None,
            ramps: vec![
                RampProof {
                    format: "xbgr2101010".into(),
                    memory: "dmabuf".into(),
                    expected_codes: 1024,
                    distinct_codes: 1024,
                    max_error_codes: 0,
                    native_wide: true,
                    channels_verified: true,
                },
                RampProof {
                    format: "rgba-f16".into(),
                    memory: "dmabuf".into(),
                    expected_codes: 1024,
                    distinct_codes: 986,
                    max_error_codes: 1,
                    native_wide: true,
                    channels_verified: true,
                },
                // Measured but not proven: never offered or trusted.
                RampProof {
                    format: "xrgb2101010".into(),
                    memory: "host-shared".into(),
                    expected_codes: 1024,
                    distinct_codes: 256,
                    max_error_codes: 3,
                    native_wide: false,
                    channels_verified: false,
                },
            ],
            sdr_white_nits: Some(203),
        };
        let session = |record: WaylandQualificationRecord, pipeline| Config {
            desktop_provider: DesktopProvider::Wayland,
            wayland_qualification: Some(Arc::new(record.validate(&host).unwrap())),
            requested_pipeline: Some(pipeline),
            ..Config::default()
        };
        let requested = |pipeline: PipelineId| ResolvedHostInitialVideo {
            video: pipeline_contract(pipeline).colour,
            selection: pipeline_contract(pipeline).selection,
            pipeline: Some(pipeline),
            encode_intent: pipeline_contract(pipeline).intent,
            max_fps: pipeline_contract(pipeline).max_fps,
        };
        let proven = record(
            CapabilityFact::Available,
            CapabilityFact::Available,
            Some(proof.clone()),
        );
        for pipeline in [PipelineId::Grading, PipelineId::Hdr] {
            let config = session(proven.clone(), pipeline);
            assert_eq!(
                config.wayland_fidelity_fallback(requested(pipeline)),
                requested(pipeline)
            );
            assert_eq!(config.wayland_fidelity_route(), Some("force-hdr"));
            assert_eq!(
                config.wayland_fidelity_args(),
                [
                    "fidelity-route=force-hdr",
                    "wide-proven=xbgr2101010@dmabuf,rgba-f16@dmabuf",
                    "sdr-white=203"
                ]
            );
        }
        // Auto and Speed sessions keep the compositor's default colour state.
        for pipeline in [PipelineId::Auto, PipelineId::Speed] {
            assert_eq!(
                session(proven.clone(), pipeline).wayland_fidelity_route(),
                None
            );
        }
        // The proof covers one output: a multi-monitor desktop serves
        // Grading and HDR as Auto, eight-bit.
        for pipeline in [PipelineId::Grading, PipelineId::Hdr] {
            let multi = Config {
                wayland_multi_output: true,
                ..session(proven.clone(), pipeline)
            };
            let served = multi.wayland_fidelity_fallback(requested(pipeline));
            assert_eq!(served.pipeline, Some(PipelineId::Auto), "{pipeline:?}");
            assert_eq!(
                served.video.bit_depth,
                pipeline_contract(PipelineId::Auto).colour.bit_depth
            );
        }
        // Only Grading proven: HDR is served as Grading on the same route.
        let grading_only = record(
            CapabilityFact::Available,
            CapabilityFact::Missing,
            Some(proof.clone()),
        );
        let served = session(grading_only.clone(), PipelineId::Hdr)
            .wayland_fidelity_fallback(requested(PipelineId::Hdr));
        assert_eq!(served.pipeline, Some(PipelineId::Grading));
        assert_eq!(
            session(grading_only, PipelineId::Grading).wayland_fidelity_route(),
            Some("force-hdr")
        );
        // An administrator's pinned eight-bit variant on a proven host: the
        // Grading or HDR request is served visibly as Auto on the pin, with no
        // route and no PQ desktop, and the session is not refused.
        for pipeline in [
            arcen_protocol::messages::StreamPipeline::Grading,
            arcen_protocol::messages::StreamPipeline::Hdr,
        ] {
            let mut pinned = Config {
                variant_pinned: true,
                codec_pinned: true,
                codec: "h265".into(),
                chroma: "yuv420".into(),
                bit_depth: arcen_media::BitDepth::Eight,
                ..session(proven.clone(), PipelineId::Auto)
            };
            pinned.requested_pipeline = None;
            let mut request = initial_video(
                VideoSelectionIntent::ColorFidelity,
                "h265",
                "yuv444",
                "10",
                "full",
            );
            request.pipeline = Some(pipeline);
            pinned
                .apply_initial_video_request(&request)
                .expect("an eight-bit pin does not refuse the session");
            assert_eq!(pinned.requested_pipeline, Some(PipelineId::Auto));
            assert_eq!(pinned.wayland_fidelity_route(), None);
            assert_eq!(pinned.bit_depth, arcen_media::BitDepth::Eight);
            assert_eq!(
                pinned.desktop_encoding,
                arcen_media::video::DesktopSignalEncoding::Sdr
            );
        }
        // Facts without the fidelity section prove nothing.
        let unproven = record(CapabilityFact::Available, CapabilityFact::Available, None);
        for pipeline in [PipelineId::Grading, PipelineId::Hdr] {
            let config = session(unproven.clone(), pipeline);
            assert_eq!(
                config
                    .wayland_fidelity_fallback(requested(pipeline))
                    .pipeline,
                Some(PipelineId::Auto)
            );
            assert_eq!(config.wayland_fidelity_route(), None);
        }
        // Mutter 49: no route, the gap travels as the reason, Auto is served.
        let gap = FidelityProof {
            route: None,
            gap: Some("compositor_lacks_hdr_screencast".into()),
            detail: Some("Mutter 49.4 has no HDR screen sharing".into()),
            ramps: Vec::new(),
            sdr_white_nits: None,
        };
        let refused = session(
            record(CapabilityFact::Missing, CapabilityFact::Missing, Some(gap)),
            PipelineId::Hdr,
        );
        assert_eq!(
            refused
                .wayland_fidelity_fallback(requested(PipelineId::Hdr))
                .pipeline,
            Some(PipelineId::Auto)
        );
        assert_eq!(refused.wayland_fidelity_route(), None);
        assert!(refused.wayland_fidelity_args().is_empty());
    }

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
            pipeline: None,
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
    fn speed_pipeline_contract_drives_motion_priority() {
        let mut request = initial_video(
            VideoSelectionIntent::AdaptivePerformance,
            "h264",
            "yuv420",
            "8",
            "limited",
        );
        request.pipeline = Some(arcen_protocol::messages::StreamPipeline::Speed);
        request.quality.max_fps = 60;
        request.quality.motion_priority = "detail".to_string();
        let mut config = Config::default();
        config.apply_initial_video_request(&request).unwrap();
        config.active_pipeline = Some(arcen_protocol::messages::ServedStreamPipeline::Speed);
        assert_eq!(
            config.requested_motion_priority(),
            arcen_media::video::MotionPriority::Motion,
            "served Speed uses the shared contract's motion priority"
        );
        assert_eq!(
            config.requested_encode_intent(),
            arcen_media::EncodeIntent::Interactive
        );
        let capenc = config.capenc_config(
            PathBuf::from("arcen-pier"),
            None,
            arcen_telemetry::CorrelationId::from_uuid_v4_bytes([2; 16]),
            arcen_protocol::messages::CursorMode::Local,
        );
        assert_eq!(
            capenc.motion_priority,
            arcen_media::video::MotionPriority::Motion
        );
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
        assert_eq!(
            pinned.requested_pipeline,
            Some(arcen_media::video::PipelineId::Grading)
        );
        assert_eq!(
            pinned.active_pipeline,
            Some(arcen_protocol::messages::ServedStreamPipeline::Custom),
            "a pinned exact/custom variant is not a served product pipeline"
        );
    }

    #[test]
    fn a_ten_bit_request_on_an_eight_bit_desktop_is_served_visibly_as_auto() {
        use arcen_media::video::{DepthReason, DesktopDepth, PipelineId};
        let grading = initial_video(
            VideoSelectionIntent::ColorFidelity,
            "h265",
            "yuv444",
            "10",
            "full",
        );
        let host = Config {
            bit_depth: arcen_media::BitDepth::Ten,
            color_range: arcen_media::ColorRange::Full,
            chroma: "yuv444".to_string(),
            ..Config::default()
        };

        // A sign-in that creates the desktop: ten bits for Grading.
        let mut created = host.clone();
        created.apply_initial_video_request(&grading).unwrap();
        assert_eq!(created.requested_pipeline, Some(PipelineId::Grading));
        assert!(!created.desktop_depth_downgraded);
        let decision = created.new_x11_desktop_depth();
        assert_eq!(decision.depth, DesktopDepth::Ten);
        assert_eq!(decision.reason, DepthReason::TenBitSession);
        assert!(!created.exceeds_desktop_depth(DesktopDepth::Ten));
        assert!(created.exceeds_desktop_depth(DesktopDepth::Eight));

        // Reattaching to a ten-bit desktop changes nothing.
        let mut on_ten = Config {
            attached_desktop_depth: Some(DesktopDepth::Ten),
            ..host.clone()
        };
        on_ten.apply_initial_video_request(&grading).unwrap();
        assert_eq!(on_ten.requested_pipeline, Some(PipelineId::Grading));
        assert_eq!(on_ten.bit_depth, arcen_media::BitDepth::Ten);
        assert!(!on_ten.desktop_depth_downgraded);

        // Reattaching to an eight-bit desktop: Auto, eight bits, flagged.
        let mut on_eight = Config {
            attached_desktop_depth: Some(DesktopDepth::Eight),
            ..host.clone()
        };
        on_eight.apply_initial_video_request(&grading).unwrap();
        assert_eq!(on_eight.requested_pipeline, Some(PipelineId::Auto));
        assert_eq!(on_eight.bit_depth, arcen_media::BitDepth::Eight);
        assert_eq!(
            on_eight.active_pipeline,
            Some(arcen_protocol::messages::ServedStreamPipeline::Auto)
        );
        assert!(on_eight.desktop_depth_downgraded);
        assert!(!on_eight.exceeds_desktop_depth(DesktopDepth::Eight));

        // An eight-bit request is never touched by the desktop's depth.
        let auto = initial_video(
            VideoSelectionIntent::AdaptivePerformance,
            "h265",
            "yuv420",
            "8",
            "limited",
        );
        let mut auto_on_eight = Config {
            attached_desktop_depth: Some(DesktopDepth::Eight),
            ..host.clone()
        };
        auto_on_eight.apply_initial_video_request(&auto).unwrap();
        assert!(!auto_on_eight.desktop_depth_downgraded);
        let mut auto_created = host;
        auto_created.apply_initial_video_request(&auto).unwrap();
        assert_eq!(
            auto_created.new_x11_desktop_depth().depth,
            DesktopDepth::Eight
        );
    }

    #[test]
    fn x11_desktop_depth_is_read_from_the_pier_file_and_unknown_tokens_fail() {
        use arcen_media::video::{DesktopDepth, DesktopDepthPolicy};
        let file_with = |token: &str| -> crate::config::PierFileConfig {
            serde_json::from_str(&format!(
                r#"{{
                    "audio":{{"enabled":true,"compressed":false}},
                    "microphone_input":{{"enabled":false}},
                    "video":{{"x11_desktop_depth":"{token}"}},
                    "platform": {{}}
                }}"#
            ))
            .expect("config")
        };
        assert_eq!(
            Config::default().x11_desktop_depth,
            DesktopDepthPolicy::Auto
        );
        for (token, policy) in [
            ("auto", DesktopDepthPolicy::Auto),
            ("AUTO", DesktopDepthPolicy::Auto),
            ("24", DesktopDepthPolicy::Pinned(DesktopDepth::Eight)),
            ("30", DesktopDepthPolicy::Pinned(DesktopDepth::Ten)),
        ] {
            let mut cfg = Config::default();
            apply_file_config(&mut cfg, file_with(token)).expect("apply file config");
            assert_eq!(cfg.x11_desktop_depth, policy, "{token}");
        }
        let error = apply_file_config(&mut Config::default(), file_with("10"))
            .expect_err("unknown token must fail");
        assert!(error.contains("video.x11_desktop_depth"), "{error}");
        assert!(error.contains("auto, 24, 30"), "{error}");

        // A pin of 30 gives every new desktop ten bits, even for Auto.
        let pinned = Config {
            x11_desktop_depth: DesktopDepthPolicy::Pinned(DesktopDepth::Ten),
            ..Config::default()
        };
        assert_eq!(pinned.new_x11_desktop_depth().depth, DesktopDepth::Ten);
        // A declared PQ desktop is ten-bit under `auto`.
        let pq = Config {
            desktop_encoding: arcen_media::video::DesktopSignalEncoding::Rec2100Pq,
            ..Config::default()
        };
        assert_eq!(pq.new_x11_desktop_depth().depth, DesktopDepth::Ten);
    }

    #[test]
    fn xorg_downgrades_hdr_transfer_but_keeps_the_ten_bit_grading_path() {
        let mut hdr = initial_video(
            VideoSelectionIntent::ColorFidelity,
            "h265",
            "yuv444",
            "10",
            "full",
        );
        hdr.quality.color_matrix = "bt2020ncl".to_string();
        hdr.quality.color_primaries = "bt2020".to_string();
        hdr.quality.transfer = "pq".to_string();
        hdr.quality.max_fps = 30;
        let mut config = Config {
            bit_depth: arcen_media::BitDepth::Ten,
            color_range: arcen_media::ColorRange::Full,
            color_matrix: arcen_media::ColorMatrix::Bt2020Ncl,
            chroma: "yuv444".to_string(),
            ..Config::default()
        };
        config.apply_initial_video_request(&hdr).unwrap();

        assert_eq!(config.bit_depth, arcen_media::BitDepth::Ten);
        assert_eq!(config.chroma, "yuv444");
        assert_eq!(config.color_range, arcen_media::ColorRange::Full);
        assert_eq!(config.color_matrix, arcen_media::ColorMatrix::Bt709);
        assert_eq!(config.color_primaries, arcen_media::ColorPrimaries::Bt709);
        assert_eq!(config.transfer, arcen_media::TransferCharacteristics::Bt709);
        assert_eq!(
            config.requested_pipeline,
            Some(arcen_media::video::PipelineId::Hdr)
        );
        assert_eq!(
            config.active_pipeline,
            Some(arcen_protocol::messages::ServedStreamPipeline::Grading)
        );
        assert_eq!(
            config.auth_video_request.as_ref().unwrap().quality.transfer,
            "pq",
            "the original request stays available for degradation telemetry"
        );
    }

    #[test]
    fn declared_pq_desktop_keeps_an_hdr_request_and_tells_capenc() {
        let mut hdr = initial_video(
            VideoSelectionIntent::ColorFidelity,
            "h265",
            "yuv444",
            "10",
            "full",
        );
        hdr.quality.color_matrix = "bt2020ncl".to_string();
        hdr.quality.color_primaries = "bt2020".to_string();
        hdr.quality.transfer = "pq".to_string();
        hdr.quality.max_fps = 30;
        let mut config = Config {
            bit_depth: arcen_media::BitDepth::Ten,
            color_range: arcen_media::ColorRange::Full,
            color_matrix: arcen_media::ColorMatrix::Bt2020Ncl,
            chroma: "yuv444".to_string(),
            desktop_encoding: arcen_media::video::DesktopSignalEncoding::Rec2100Pq,
            ..Config::default()
        };
        config.apply_initial_video_request(&hdr).unwrap();
        assert_eq!(config.transfer, arcen_media::TransferCharacteristics::Pq);
        assert_eq!(config.color_primaries, arcen_media::ColorPrimaries::Bt2020);
        assert_eq!(config.color_matrix, arcen_media::ColorMatrix::Bt2020Ncl);
        assert_eq!(
            config.active_pipeline,
            Some(arcen_protocol::messages::ServedStreamPipeline::Hdr)
        );
        let argv = config
            .capenc_config(
                PathBuf::from("arcen-pier"),
                None,
                arcen_telemetry::CorrelationId::from_uuid_v4_bytes([1; 16]),
                arcen_protocol::messages::CursorMode::Local,
            )
            .argv();
        assert!(argv.iter().any(|arg| arg == "transfer=pq"), "{argv:?}");
        assert!(
            argv.iter().any(|arg| arg == "desktop-encoding=rec2100-pq"),
            "{argv:?}"
        );

        let mut grading = initial_video(
            VideoSelectionIntent::ColorFidelity,
            "h265",
            "yuv444",
            "10",
            "full",
        );
        grading.quality.color_matrix = "bt709".to_string();
        let mut sdr = Config {
            bit_depth: arcen_media::BitDepth::Ten,
            color_range: arcen_media::ColorRange::Full,
            chroma: "yuv444".to_string(),
            desktop_encoding: arcen_media::video::DesktopSignalEncoding::Rec2100Pq,
            ..Config::default()
        };
        sdr.apply_initial_video_request(&grading).unwrap();
        assert_eq!(sdr.transfer, arcen_media::TransferCharacteristics::Bt709);
        let argv = sdr
            .capenc_config(
                PathBuf::from("arcen-pier"),
                None,
                arcen_telemetry::CorrelationId::from_uuid_v4_bytes([1; 16]),
                arcen_protocol::messages::CursorMode::Local,
            )
            .argv();
        assert!(
            argv.iter().any(|arg| arg == "desktop-encoding=rec2100-pq"),
            "an SDR session on a PQ desktop still tells capenc its source, so the \
             pixels are converted rather than relabelled: {argv:?}"
        );
        assert!(
            !argv.iter().any(|arg| arg.starts_with("transfer=")),
            "{argv:?}"
        );

        // An eight-bit session cannot be served truthfully from a PQ desktop.
        let auto = initial_video(
            VideoSelectionIntent::AdaptivePerformance,
            "h264",
            "yuv420",
            "8",
            "limited",
        );
        let mut eight = Config {
            desktop_encoding: arcen_media::video::DesktopSignalEncoding::Rec2100Pq,
            ..Config::default()
        };
        let error = eight.apply_initial_video_request(&auto).unwrap_err();
        assert!(error.contains("Choose Grading"), "{error}");

        // An SDR desktop never sends the token.
        let mut plain = Config {
            bit_depth: arcen_media::BitDepth::Ten,
            color_range: arcen_media::ColorRange::Full,
            chroma: "yuv444".to_string(),
            ..Config::default()
        };
        plain.apply_initial_video_request(&grading).unwrap();
        let argv = plain
            .capenc_config(
                PathBuf::from("arcen-pier"),
                None,
                arcen_telemetry::CorrelationId::from_uuid_v4_bytes([1; 16]),
                arcen_protocol::messages::CursorMode::Local,
            )
            .argv();
        assert!(
            !argv.iter().any(|arg| arg.starts_with("desktop-encoding=")),
            "{argv:?}"
        );
    }

    #[test]
    fn the_wayland_qualification_record_is_read_from_the_pier_file() {
        let file_with = |capture: &str| -> crate::config::PierFileConfig {
            serde_json::from_str(&format!(
                r#"{{
                    "audio":{{"enabled":true,"compressed":false}},
                    "microphone_input":{{"enabled":false}},
                    "capture":{capture},
                    "platform": {{}}
                }}"#
            ))
            .expect("config")
        };
        let mut cfg = Config::default();
        assert!(cfg.wayland_qualification_target.is_production());
        apply_file_config(&mut cfg, file_with("{}")).expect("apply file config");
        assert!(cfg.wayland_qualification_target.is_production());
        apply_file_config(
            &mut cfg,
            file_with(r#"{"wayland_qualification_record":"/var/lib/arcen-dev/speed/q.json"}"#),
        )
        .expect("apply file config");
        assert_eq!(
            cfg.wayland_qualification_target.record_path(),
            "/var/lib/arcen-dev/speed/q.json"
        );
        assert_ne!(
            cfg.wayland_qualification_target.temporary_user(),
            arcen_session::wayland_qualification::QUALIFY_TEMPORARY_USER
        );
        let error = apply_file_config(
            &mut Config::default(),
            file_with(r#"{"wayland_qualification_record":"relative.json"}"#),
        )
        .expect_err("a relative record path must fail");
        assert!(
            error.contains("capture.wayland_qualification_record"),
            "{error}"
        );
    }

    #[test]
    fn the_compositor_prefix_defaults_to_the_system_and_is_validated() {
        let file_with = |capture: &str| -> crate::config::PierFileConfig {
            serde_json::from_str(&format!(
                r#"{{
                    "audio":{{"enabled":true,"compressed":false}},
                    "microphone_input":{{"enabled":false}},
                    "capture":{capture},
                    "platform": {{}}
                }}"#
            ))
            .expect("config")
        };
        let mut cfg = Config::default();
        apply_file_config(&mut cfg, file_with("{}")).expect("apply file config");
        assert_eq!(cfg.wayland_compositor_prefix, None);
        apply_file_config(
            &mut cfg,
            file_with(r#"{"wayland_compositor_prefix":"/opt/arcen-gnome/51"}"#),
        )
        .expect("apply file config");
        assert_eq!(
            cfg.wayland_compositor_prefix.as_deref(),
            Some("/opt/arcen-gnome/51")
        );
        for refused in [
            r#""opt/gnome""#,
            r#""/opt/../tmp""#,
            r#""/""#,
            r#""/opt/a:b""#,
        ] {
            let error = apply_file_config(
                &mut Config::default(),
                file_with(&format!(r#"{{"wayland_compositor_prefix":{refused}}}"#)),
            )
            .expect_err("a malformed prefix must fail");
            assert!(
                error.contains("capture.wayland_compositor_prefix"),
                "{error}"
            );
        }
    }

    #[test]
    fn x11_apps_in_wayland_sessions_default_to_auto_and_follow_only_the_resolved_plan() {
        use arcen_session::wayland_x11::{X11AppsDisabledReason, X11AppsPlan, X11AppsSetting};
        let file_with = |session: &str| {
            serde_json::from_str::<crate::config::PierFileConfig>(&format!(
                r#"{{
                    "audio":{{"enabled":true,"compressed":false}},
                    "microphone_input":{{"enabled":false}},
                    "platform":{{"session":{session}}}
                }}"#
            ))
        };
        let mut cfg = Config::default();
        apply_file_config(&mut cfg, file_with("{}").expect("config")).expect("apply");
        assert_eq!(cfg.wayland_x11_apps, X11AppsSetting::Auto);
        assert!(!cfg.wayland_x11_apps_enabled());
        for (token, setting) in [
            ("on", X11AppsSetting::On),
            ("off", X11AppsSetting::Off),
            ("auto", X11AppsSetting::Auto),
        ] {
            let mut cfg = Config::default();
            apply_file_config(
                &mut cfg,
                file_with(&format!(r#"{{"wayland_x11_apps":"{token}"}}"#)).expect("config"),
            )
            .expect("apply");
            assert_eq!(cfg.wayland_x11_apps, setting);
        }
        assert!(file_with(r#"{"wayland_x11_apps":"yes"}"#).is_err());
        cfg.wayland_x11_plan = Some(X11AppsPlan::Disabled(X11AppsDisabledReason::NotQualified));
        assert!(!cfg.wayland_x11_apps_enabled());
        cfg.wayland_x11_plan = Some(X11AppsPlan::Enabled);
        assert!(cfg.wayland_x11_apps_enabled());
    }

    #[test]
    fn desktop_encoding_is_read_from_the_pier_file_and_unknown_tokens_fail() {
        let file_with = |token: &str| -> crate::config::PierFileConfig {
            serde_json::from_str(&format!(
                r#"{{
                    "audio":{{"enabled":true,"compressed":false}},
                    "microphone_input":{{"enabled":false}},
                    "video":{{"desktop_encoding":"{token}"}},
                    "platform": {{}}
                }}"#
            ))
            .expect("config")
        };
        let mut cfg = Config::default();
        assert_eq!(
            cfg.desktop_encoding,
            arcen_media::video::DesktopSignalEncoding::Sdr
        );
        apply_file_config(&mut cfg, file_with("rec2100-pq")).expect("apply file config");
        assert_eq!(
            cfg.desktop_encoding,
            arcen_media::video::DesktopSignalEncoding::Rec2100Pq
        );

        let error = apply_file_config(&mut Config::default(), file_with("hdr"))
            .expect_err("unknown token must fail");
        assert!(error.contains("video.desktop_encoding"), "{error}");
    }

    #[test]
    fn qp_map_object_uses_the_served_pipeline() {
        let file: crate::config::PierFileConfig = serde_json::from_str(
            r#"{
                "audio":{"enabled":true,"compressed":false},
                "microphone_input":{"enabled":false},
                "video":{"qp_map":{"speed":"on","grading":"neutral"}},
                "platform": {}
            }"#,
        )
        .expect("config");
        let mut cfg = Config::default();
        apply_file_config(&mut cfg, file).expect("apply file config");
        assert_eq!(cfg.qp_map, arcen_media::video::QpMapPolicy::default());
        cfg.active_pipeline = Some(arcen_protocol::messages::ServedStreamPipeline::Speed);
        assert_eq!(
            cfg.effective_qp_map_policy(),
            Some(arcen_media::video::QpMapPolicy::On)
        );
        cfg.active_pipeline = Some(arcen_protocol::messages::ServedStreamPipeline::Grading);
        assert_eq!(
            cfg.effective_qp_map_policy(),
            Some(arcen_media::video::QpMapPolicy::Neutral)
        );
        cfg.active_pipeline = Some(arcen_protocol::messages::ServedStreamPipeline::Hdr);
        assert_eq!(
            cfg.effective_qp_map_policy(),
            Some(arcen_media::video::QpMapPolicy::Off)
        );
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
        assert!(config.multi_monitor.advertise_enabled);
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
    fn multi_monitor_advertise_enabled_with_auto_encoder_is_allowed_until_discovery() {
        let config = parse(&[
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
        .expect("auto is resolved once startup NVIDIA discovery has selected heads");
        assert_eq!(config.encoder, EncoderSelection::Auto);
        assert_eq!(config.multi_monitor.heads, ["DFP-0"]);
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
        // Any NVIDIA DFP-N output may host the session, not only DFP-0..3.
        assert!(validate_gpu_head("DFP-4").is_ok());
        assert!(parse(&["--session-gpu-head".into(), "HDMI-0".into()]).is_err());
    }
}
