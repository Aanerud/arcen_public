use serde::de::{self, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt;

use crate::wire::AudioCodec;
use arcen_usb_bridge::UsbSpeed;

/// Close reason a host sends when a connection's requested multi-monitor
/// topology cannot be served by the persistent desktop it would reattach to.
///
/// A desktop's committed topology is frozen for its whole lifetime, so a
/// desktop created by a single-primary or capability-probe connect can never
/// grow into Match My Layout on a later reconnect. ADR-0009 forbids silently
/// serving the primary-only subset, so the host closes instead.
///
/// Shared as one exact literal rather than duplicated on each side: the
/// client recognises this reason to offer the user the two real recoveries
/// -- reconnect to the existing desktop as-is, or replace it via
/// `AuthResponse::replace_incompatible_desktop` -- and a drifting copy would
/// silently turn that choice back into the dead end it replaced. Must stay
/// within the 120-byte control-frame budget so it reaches the user verbatim.
pub const MULTI_MONITOR_TOPOLOGY_CONFLICT_REASON: &str =
    "This desktop was started without your layout. Reconnect to keep it, or start a fresh session.";

pub const CLIENT_HELLO: &str = "client_hello";
pub const SERVER_HELLO: &str = "server_hello";
pub const AUTH_REQUEST: &str = "auth_request";
pub const AUTH_RESPONSE: &str = "auth_response";
pub const AUTH_RESULT: &str = "auth_result";
pub const BROKER_HELLO: &str = "broker_hello";
pub const BROKER_ASSIGN: &str = "broker_assign";
pub const BROKER_MACHINE_REQUEST: &str = "broker_machine_request";
pub const REQUEST_FULL_FRAME: &str = "request_full_frame";
pub const HEALTH_PING: &str = "health_ping";
pub const HEALTH_PONG: &str = "health_pong";
pub const HEALTH_STATS: &str = "health_stats";
pub const MOUSE_SCROLL: &str = "mouse_scroll";
pub const MOUSE_MOVE_RELATIVE: &str = "mouse_move_relative";
pub const CURSOR_MODE_RESULT: &str = "cursor_mode_result";
pub const TABLET_MODE_RESULT: &str = "tablet_mode_result";
pub const DISPLAY_UPDATE: &str = "display_update";
pub const DISPLAY_UPDATE_RESULT: &str = "display_update_result";
pub const KEY_RESET_MODIFIERS: &str = "key_reset_modifiers";
pub const TEXT_COMMIT: &str = "text_commit";
pub const BUILD_IDENTITY_CAPABILITY: &str = "build_identity";
pub const CLIPBOARD_DATA: &str = "clipboard_data";
pub const AUDIO_STREAM_RESULT: &str = "audio_stream_result";
pub const MICROPHONE_STREAM_RESULT: &str = "microphone_stream_result";
pub const MICROPHONE_STREAM_STOP: &str = "microphone_stream_stop";
/// Negotiated typed-pen sample. Legal only once both peers have confirmed
/// `input_protocol_version >= 3` and `InputCapabilitiesMsg.pen = available`;
/// see [`PenEventMsg`].
pub const PEN_EVENT: &str = "pen_event";
pub use crate::region_input::{
    RegionInputMetadataMsg, RegionInputPositionMsg, RegionInputValidationError, RegionPenEventMsg,
    RegionPointerButtonMsg, RegionPointerEnterMsg, RegionPointerLeaveMsg, RegionPointerMotionMsg,
    RegionPointerScrollMsg, REGION_PEN_EVENT, REGION_POINTER_BUTTON, REGION_POINTER_ENTER,
    REGION_POINTER_LEAVE, REGION_POINTER_MOTION, REGION_POINTER_SCROLL,
};
/// Clipboard capability and framing sub-version.
pub const CLIPBOARD_PROTOCOL_VERSION: u16 = 1;
/// Audio-output capability and configuration sub-version.
pub const AUDIO_PROTOCOL_VERSION: u16 = 1;
/// Client-to-host microphone capability and framing sub-version.
pub const MICROPHONE_PROTOCOL_VERSION: u16 = 1;
/// Maximum codec entries accepted in one audio capability message.
pub const MAX_AUDIO_CODECS: usize = 2;
/// Input capability sub-version carried independently of the base protocol.
///
/// Bumped 2 -> 3 to add negotiated typed-pen support (`PenEventMsg`,
/// `InputCapabilitiesMsg` pen/pressure/tilt/rotation/eraser/proximity truth).
/// Bumped 3 -> 4 to make the region-scoped input-v1 replacement explicit:
/// both peers must advertise `region_input = available` before any
/// `region_pointer_*` or `region_pen_event` command is legal. Older input
/// versions remain valid for their own capabilities; `wire::PROTOCOL_VERSION`
/// does not change.
pub const INPUT_PROTOCOL_VERSION: u32 = 4;
/// Minimum input version for relative pointer commands.
pub const RELATIVE_POINTER_INPUT_PROTOCOL_VERSION: u32 = 2;
/// Minimum input version for typed pen commands.
pub const PEN_INPUT_PROTOCOL_VERSION: u32 = 3;
/// Minimum input version for region-scoped input-v1 commands.
pub const REGION_INPUT_PROTOCOL_VERSION: u32 = 4;
/// Maximum UTF-8 bytes allowed in a cursor negotiation reason.
pub const MAX_CURSOR_MODE_REASON_BYTES: usize = 160;
/// Maximum UTF-8 bytes allowed in a tablet-mode negotiation reason.
pub const MAX_TABLET_MODE_REASON_BYTES: usize = 240;
/// Direct-transport reconnect authentication method.
pub const AUTH_METHOD_RESUME: &str = "resume";
/// Maximum UTF-8 bytes allowed in a client network identity (SSID), aligned
/// with `arcen-telemetry::MAX_NETWORK_IDENTITY_BYTES`.
pub const MAX_NETWORK_IDENTITY_BYTES: usize = 64;
/// Device-capability key for additive multi-monitor-v1 negotiation metadata.
pub const MULTI_MONITOR_V1: &str = "multi_monitor_v1";
/// Maximum monitors supported by the first approved multi-monitor tranche.
pub const MAX_MULTI_MONITOR_COUNT: usize = 4;
/// Maximum UTF-8 bytes allowed in an opaque client display identifier.
pub const MAX_CLIENT_DISPLAY_ID_BYTES: usize = 64;

pub use crate::multi_monitor::{
    AdvertisedMultiMonitorOffer, AppliedMonitorDescriptorMsg, AppliedMonitorMediaPlanMsg,
    AppliedMonitorTopologyMsg, AuthMultiMonitorOfferMsg, AuthMultiMonitorRequestMsg,
    AuthRequestMultiMonitorOfferError, ClientDisplayId, ClientDisplayIdError,
    ClientMultiMonitorMsg, MonitorQualityIntentMsg, MultiMonitorCapabilityError,
    MultiMonitorCarrierMsg, MultiMonitorValidationError, RequestedMonitorDescriptorMsg,
    RequestedMonitorTopologyMsg, RotationMsg, SafeAreaPolicyMsg, ServerMultiMonitorMsg,
    TopologyBackendKindMsg,
};

/// Fixed audio bitrate tiers carried by audio-v1.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AudioBitrateTierMsg {
    Off,
    Kbps32,
    Kbps64,
    Kbps128,
    Kbps256,
    Kbps510,
}

/// Immutable product-build identity advertised during the handshake.
///
/// `artifact_sha256` is optional because source builds may not have a
/// packaging manifest. When present it is the hash of the exact executable
/// being run, not a trust anchor and not a replacement for code signing.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BuildIdentityMsg {
    pub product: String,
    pub version: String,
    pub build_id: String,
    pub source_revision: String,
    pub build_profile: String,
    pub feature_profile: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signing_state: Option<String>,
}

impl BuildIdentityMsg {
    #[must_use]
    pub fn development(product: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            product: product.into(),
            version: version.into(),
            build_id: "development".to_string(),
            source_revision: "unknown".to_string(),
            build_profile: "debug".to_string(),
            feature_profile: "default".to_string(),
            artifact_sha256: None,
            signing_state: None,
        }
    }
}

fn decode_build_identity(
    capabilities: &BTreeMap<String, Value>,
) -> Result<Option<BuildIdentityMsg>, serde_json::Error> {
    capabilities
        .get(BUILD_IDENTITY_CAPABILITY)
        .cloned()
        .map(serde_json::from_value)
        .transpose()
}

fn attach_build_identity(capabilities: &mut BTreeMap<String, Value>, identity: BuildIdentityMsg) {
    capabilities.insert(
        BUILD_IDENTITY_CAPABILITY.to_string(),
        serde_json::to_value(identity).expect("BuildIdentityMsg serializes"),
    );
}

/// Exact audio formats and codecs an endpoint can produce or consume.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AudioOutputCapabilitiesMsg {
    pub protocol_version: u16,
    #[serde(deserialize_with = "deserialize_audio_codecs")]
    pub codecs: Vec<AudioCodec>,
    pub sample_rate_hz: u32,
    pub channels: u8,
    pub frame_duration_ms: u16,
    #[serde(default)]
    pub fec: bool,
    #[serde(default)]
    pub dtx: bool,
}

impl ServerHelloMsg {
    #[must_use]
    pub fn with_build_identity(mut self, identity: BuildIdentityMsg) -> Self {
        attach_build_identity(&mut self.device_capabilities, identity);
        self
    }

    pub fn build_identity(&self) -> Result<Option<BuildIdentityMsg>, serde_json::Error> {
        decode_build_identity(&self.device_capabilities)
    }
}

fn deserialize_audio_codecs<'de, D>(deserializer: D) -> Result<Vec<AudioCodec>, D::Error>
where
    D: Deserializer<'de>,
{
    struct AudioCodecsVisitor;

    impl<'de> Visitor<'de> for AudioCodecsVisitor {
        type Value = Vec<AudioCodec>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(formatter, "at most {MAX_AUDIO_CODECS} audio codecs")
        }

        fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
        where
            A: SeqAccess<'de>,
        {
            let mut codecs = Vec::with_capacity(MAX_AUDIO_CODECS);
            while let Some(codec) = sequence.next_element()? {
                if codecs.len() == MAX_AUDIO_CODECS {
                    return Err(de::Error::invalid_length(MAX_AUDIO_CODECS + 1, &self));
                }
                codecs.push(codec);
            }
            Ok(codecs)
        }
    }

    deserializer.deserialize_seq(AudioCodecsVisitor)
}

impl AudioOutputCapabilitiesMsg {
    /// Returns whether this is a bounded fixed-format audio-v1 capability.
    #[must_use]
    pub fn is_valid_v1(&self) -> bool {
        self.protocol_version == AUDIO_PROTOCOL_VERSION
            && !self.codecs.is_empty()
            && self.codecs.len() <= MAX_AUDIO_CODECS
            && self.sample_rate_hz == 48_000
            && self.channels == 2
            && self.frame_duration_ms == 20
            && !self.fec
            && !self.dtx
    }
}

/// Exact fixed-format client microphone support.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MicrophoneCapabilitiesMsg {
    pub protocol_version: u16,
    #[serde(deserialize_with = "deserialize_audio_codecs")]
    pub codecs: Vec<AudioCodec>,
    pub sample_rate_hz: u32,
    pub channels: u8,
    pub frame_duration_ms: u16,
    #[serde(default)]
    pub fec: bool,
    #[serde(default)]
    pub dtx: bool,
}

impl MicrophoneCapabilitiesMsg {
    /// Returns whether this is a bounded fixed-format microphone-v1 capability.
    #[must_use]
    pub fn is_valid_v1(&self) -> bool {
        self.protocol_version == MICROPHONE_PROTOCOL_VERSION
            && !self.codecs.is_empty()
            && self.codecs.len() <= MAX_AUDIO_CODECS
            && self.sample_rate_hz == 48_000
            && self.channels == 1
            && self.frame_duration_ms == 20
            && !self.fec
            && !self.dtx
    }
}

/// Exact audio stream selected for one attachment.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct AudioStreamConfigMsg {
    pub protocol_version: u16,
    pub codec: AudioCodec,
    pub sample_rate_hz: u32,
    pub channels: u8,
    pub frame_duration_ms: u16,
    pub bitrate: AudioBitrateTierMsg,
    pub fec: bool,
    pub dtx: bool,
}

impl AudioStreamConfigMsg {
    /// Returns whether the selected configuration is valid for audio-v1.
    #[must_use]
    pub const fn is_valid_v1(self) -> bool {
        self.protocol_version == AUDIO_PROTOCOL_VERSION
            && self.sample_rate_hz == 48_000
            && self.channels == 2
            && self.frame_duration_ms == 20
            && !matches!(self.bitrate, AudioBitrateTierMsg::Off)
            && !self.fec
            && !self.dtx
    }
}

/// Exact microphone-v1 stream selected for one attachment.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct MicrophoneStreamConfigMsg {
    pub protocol_version: u16,
    pub codec: AudioCodec,
    pub sample_rate_hz: u32,
    pub channels: u8,
    pub frame_duration_ms: u16,
    /// Opus bitrate tier. Fixed PCM uses `Off` and declares its exact rate
    /// through `pcm_bitrate_kbps` so the wire never labels PCM as Opus.
    pub bitrate: AudioBitrateTierMsg,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pcm_bitrate_kbps: Option<u16>,
    pub generation: u32,
    pub fec: bool,
    pub dtx: bool,
}

impl MicrophoneStreamConfigMsg {
    #[must_use]
    pub const fn is_valid_v1(self) -> bool {
        self.protocol_version == MICROPHONE_PROTOCOL_VERSION
            && self.sample_rate_hz == 48_000
            && self.channels == 1
            && self.frame_duration_ms == 20
            && match self.codec {
                AudioCodec::Opus => {
                    !matches!(self.bitrate, AudioBitrateTierMsg::Off)
                        && self.pcm_bitrate_kbps.is_none()
                }
                AudioCodec::Pcm => {
                    matches!(self.bitrate, AudioBitrateTierMsg::Off)
                        && matches!(self.pcm_bitrate_kbps, Some(768))
                }
            }
            && self.generation != 0
            && !self.fec
            && !self.dtx
    }
}

/// Bounded, non-sensitive outcome for audio stream selection.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AudioStreamReason {
    Enabled,
    LegacyPcm,
    DisabledByPolicy,
    BelowMinimumBitrate,
    VersionMismatch,
    NoCommonCodec,
    InvalidCapabilities,
    CodecUnavailable,
    CodecFailure,
    CaptureUnavailable,
    #[default]
    NotNegotiated,
}

/// Bounded, non-sensitive microphone selection outcome.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MicrophoneStreamReason {
    Enabled,
    DisabledByOperator,
    DisabledByClient,
    VersionMismatch,
    NoCommonCodec,
    InvalidCapabilities,
    BackendUnavailable,
    PermissionDenied,
    CaptureFailure,
    #[default]
    NotNegotiated,
}

/// Host confirmation of the audio stream active for one attachment.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AudioStreamResultMsg {
    #[serde(rename = "type", default = "default_audio_stream_result_type")]
    pub msg_type: String,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<AudioStreamConfigMsg>,
    #[serde(default)]
    pub reason: AudioStreamReason,
}

impl AudioStreamResultMsg {
    #[must_use]
    pub fn enabled(config: AudioStreamConfigMsg, reason: AudioStreamReason) -> Self {
        Self {
            msg_type: default_audio_stream_result_type(),
            enabled: true,
            config: Some(config),
            reason,
        }
    }

    #[must_use]
    pub fn disabled(reason: AudioStreamReason) -> Self {
        Self {
            msg_type: default_audio_stream_result_type(),
            enabled: false,
            config: None,
            reason,
        }
    }
}

fn default_audio_stream_result_type() -> String {
    AUDIO_STREAM_RESULT.to_owned()
}

/// Host confirmation that authorizes one generation of upstream microphone frames.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MicrophoneStreamResultMsg {
    #[serde(rename = "type", default = "default_microphone_stream_result_type")]
    pub msg_type: String,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<MicrophoneStreamConfigMsg>,
    #[serde(default)]
    pub reason: MicrophoneStreamReason,
}

impl MicrophoneStreamResultMsg {
    #[must_use]
    pub fn enabled(config: MicrophoneStreamConfigMsg, reason: MicrophoneStreamReason) -> Self {
        Self {
            msg_type: default_microphone_stream_result_type(),
            enabled: true,
            config: Some(config),
            reason,
        }
    }

    #[must_use]
    pub fn disabled(reason: MicrophoneStreamReason) -> Self {
        Self {
            msg_type: default_microphone_stream_result_type(),
            enabled: false,
            config: None,
            reason,
        }
    }
}

fn default_microphone_stream_result_type() -> String {
    MICROPHONE_STREAM_RESULT.to_owned()
}

/// Client request to stop one authorized microphone generation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MicrophoneStreamStopMsg {
    #[serde(rename = "type", default = "default_microphone_stream_stop_type")]
    pub msg_type: String,
    pub protocol_version: u16,
    pub generation: u32,
    pub reason: MicrophoneStreamReason,
}

impl MicrophoneStreamStopMsg {
    #[must_use]
    pub fn new(generation: u32, reason: MicrophoneStreamReason) -> Self {
        Self {
            msg_type: default_microphone_stream_stop_type(),
            protocol_version: MICROPHONE_PROTOCOL_VERSION,
            generation,
            reason,
        }
    }

    #[must_use]
    pub fn is_valid(&self) -> bool {
        self.msg_type == MICROPHONE_STREAM_STOP
            && self.protocol_version == MICROPHONE_PROTOCOL_VERSION
            && self.generation != 0
            && matches!(
                self.reason,
                MicrophoneStreamReason::DisabledByClient
                    | MicrophoneStreamReason::PermissionDenied
                    | MicrophoneStreamReason::CaptureFailure
            )
    }
}

fn default_microphone_stream_stop_type() -> String {
    MICROPHONE_STREAM_STOP.to_owned()
}

/// How the host should interpret the video's codec field.
///
/// Colour axes remain explicit in every mode. Only `AdaptivePerformance`
/// authorizes the host to replace the preferred codec with the highest-ranked
/// usable hardware codec that the client can decode.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VideoSelectionIntent {
    /// Preserve the concrete codec and colour contract exactly where possible.
    /// This is the compatibility default for clients predating auth-time video
    /// intent and for diagnostic/probe-matrix requests.
    #[default]
    Exact,
    /// Ordinary 4:2:0 desktop session: rank hardware AV1, HEVC, then H.264
    /// without changing the independently requested colour axes.
    AdaptivePerformance,
    /// Prefer the supplied fidelity contract (normally HEVC 4:4:4) and report
    /// any fallback explicitly rather than substituting an ordinary AV1 tier.
    ColorFidelity,
}

const fn is_exact_video_selection(value: &VideoSelectionIntent) -> bool {
    matches!(value, VideoSelectionIntent::Exact)
}

/// Client decode capabilities needed before the host creates its encoder.
///
/// These mirror the video subset of `ClientHelloMsg`. They travel in the
/// authenticated setup request because protocol order requires the host to
/// create the real encoder before it can send an authoritative `ServerHello`.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClientVideoCapabilitiesMsg {
    #[serde(default)]
    pub h264: bool,
    #[serde(default)]
    pub h265: bool,
    #[serde(default)]
    pub av1: bool,
    #[serde(default)]
    pub yuv444: bool,
    #[serde(default)]
    pub main10: bool,
    #[serde(default)]
    pub main12: bool,
    #[serde(default)]
    pub full_range: bool,
    #[serde(default)]
    pub identity_matrix: bool,
    /// Client can consume BT.601 matrix coefficients.
    ///
    /// Absent/false retains the safe legacy answer for peers predating this
    /// additive capability bit.
    #[serde(default)]
    pub bt601_matrix: bool,
    /// Client can consume BT.2020 non-constant-luminance matrix coefficients.
    ///
    /// Absent/false retains the safe legacy answer for peers predating this
    /// additive capability bit.
    #[serde(default)]
    pub bt2020_ncl_matrix: bool,
}

impl ClientVideoCapabilitiesMsg {
    #[must_use]
    pub fn from_client_hello(hello: &ClientHelloMsg) -> Self {
        Self {
            h264: hello.supports_h264,
            h265: hello.supports_h265,
            av1: hello.supports_av1,
            yuv444: hello.supports_yuv444,
            main10: hello.supports_main10,
            main12: hello.supports_main12,
            full_range: hello.supports_full_range,
            identity_matrix: hello.supports_identity_matrix,
            bt601_matrix: hello.supports_bt601_matrix,
            bt2020_ncl_matrix: hello.supports_bt2020_ncl_matrix,
        }
    }
}

/// Auth-time video request used to create the first, authoritative media plan.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InitialVideoRequestMsg {
    pub quality: QualitySettings,
    pub capabilities: ClientVideoCapabilitiesMsg,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QualitySettings {
    #[serde(rename = "type")]
    pub msg_type: String,
    pub quality_bias: f64,
    pub max_fps: u32,
    pub max_bandwidth_mbps: f64,
    pub codec: String,
    pub chroma: String,
    /// Whether `codec` is exact, adaptive-performance preference, or a
    /// colour-fidelity target. Absent means exact for old clients.
    #[serde(default, skip_serializing_if = "is_exact_video_selection")]
    pub video_selection: VideoSelectionIntent,
    /// Requested coded component depth, as a `BitDepth` token (`8`/`10`/`12`).
    ///
    /// The host serves the deepest depth it can that is no deeper than this,
    /// and reports any reduction rather than applying it silently.
    #[serde(default = "default_bit_depth")]
    pub bit_depth: String,
    /// Requested coded sample range (`limited`/`full`).
    #[serde(default = "default_color_range")]
    pub color_range: String,
    /// Requested matrix coefficients (`bt709`/`identity`/`bt601`/`bt2020ncl`).
    #[serde(default = "default_color_matrix")]
    pub color_matrix: String,
    /// Requested transfer characteristics (`bt709`/`srgb`/`pq`/`hlg`).
    ///
    /// **This is the field that asks for HDR.** `pq` (SMPTE ST 2084) is the
    /// only value that means "compose, capture and encode this desktop wide":
    /// it is what makes a host apply an HDR EDID, enable Advanced Color, and
    /// take a wide capture format. Depth alone does not, because 10-bit BT.709
    /// SDR is an ordinary and useful thing to ask for -- banding headroom
    /// without any HDR involvement.
    ///
    /// Defaults to `bt709` so a client that predates the field, or one that
    /// simply does not want HDR, is unchanged.
    #[serde(default = "default_transfer")]
    pub transfer: String,
    /// Requested colour primaries (`bt709`/`bt2020`/`display_p3`).
    ///
    /// Carried beside `transfer` because HDR10 is both: PQ over BT.2020. A
    /// host that honoured one and not the other would produce a stream whose
    /// signalling contradicts its pixels.
    #[serde(default = "default_color_primaries")]
    pub color_primaries: String,
    /// What the encoder should optimise for (`interactive`/`quality`).
    ///
    /// `interactive` is latency-first and is what Arcen has always encoded.
    /// `quality` lets the encoder spend more per frame, for grading and VFX
    /// review where the image is judged rather than driven.
    #[serde(default = "default_encode_intent")]
    pub encode_intent: String,
    pub force_lossless: bool,
    pub intra_refresh: bool,
    pub enable_audio: bool,
    pub audio_bitrate_kbps: u32,
}

impl Default for QualitySettings {
    fn default() -> Self {
        Self {
            msg_type: "quality_settings".to_string(),
            quality_bias: 0.5,
            max_fps: 60,
            max_bandwidth_mbps: 50.0,
            codec: "h264".to_string(),
            chroma: "yuv422".to_string(),
            video_selection: VideoSelectionIntent::Exact,
            bit_depth: default_bit_depth(),
            color_range: default_color_range(),
            color_matrix: default_color_matrix(),
            transfer: default_transfer(),
            color_primaries: default_color_primaries(),
            encode_intent: default_encode_intent(),
            force_lossless: false,
            intra_refresh: false,
            enable_audio: true,
            audio_bitrate_kbps: 128,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AuthRequest {
    #[serde(rename = "type")]
    pub msg_type: String,
    #[serde(default = "default_auth_methods")]
    pub auth_methods: Vec<String>,
    #[serde(default)]
    pub challenge: String,
    #[serde(default)]
    pub salt: String,
    #[serde(default)]
    pub auth_mode: Option<String>,
    /// Exact bounded host-provided disclaimer text, when operator policy enables it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disclaimer: Option<String>,
    /// Optional pre-auth host multi-monitor-v1 offer. Legacy hosts omit it,
    /// which means clients must remain primary-only during authentication.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub multi_monitor_v1: Option<AuthMultiMonitorOfferMsg>,
}

fn default_auth_methods() -> Vec<String> {
    vec!["password".to_string(), "token".to_string()]
}

impl AuthRequest {
    /// Attaches a validated pre-auth host multi-monitor-v1 offer.
    ///
    /// # Errors
    ///
    /// Returns an error when the advertised offer is internally inconsistent.
    pub fn with_multi_monitor_v1_offer(
        mut self,
        offer: AuthMultiMonitorOfferMsg,
    ) -> Result<Self, MultiMonitorValidationError> {
        crate::multi_monitor::attach_auth_request_multi_monitor_v1_offer(&mut self, offer)?;
        Ok(self)
    }

    /// Returns the optional pre-auth host multi-monitor-v1 offer.
    #[must_use]
    pub fn multi_monitor_v1_offer(&self) -> Option<&AuthMultiMonitorOfferMsg> {
        self.multi_monitor_v1.as_ref()
    }

    /// Returns validated evidence that the host advertised multi-monitor-v1
    /// support before authentication.
    ///
    /// # Errors
    ///
    /// Returns an error when the host did not advertise the offer or when the
    /// stored offer is internally inconsistent.
    pub fn required_multi_monitor_v1_offer(
        &self,
    ) -> Result<AdvertisedMultiMonitorOffer<'_>, AuthRequestMultiMonitorOfferError> {
        crate::multi_monitor::required_auth_request_multi_monitor_v1_offer(self)
    }

    /// Advertises direct-transport resume only when host policy supports it.
    #[must_use]
    pub fn with_resume_support(mut self, supported: bool) -> Self {
        self.auth_methods
            .retain(|method| method != AUTH_METHOD_RESUME);
        if supported {
            self.auth_methods.push(AUTH_METHOD_RESUME.to_owned());
        }
        self
    }

    /// Returns whether direct-transport resume was explicitly advertised.
    #[must_use]
    pub fn supports_resume(&self) -> bool {
        self.auth_methods
            .iter()
            .any(|method| method == AUTH_METHOD_RESUME)
    }
}

/// A single physical display on the client, reported to the host so it can
/// build a matching X session (native resolution, one virtual output per
/// monitor). Physical-pixel dimensions + a separate `scale` follow the
/// RustDesk `DisplayInfo` convention; the IronRDP validation rules (even
/// width, exactly one primary at 0,0) are enforced when this is populated.
///
/// The physical-size + identity fields (`width_mm`/`height_mm`/`vendor`/
/// `model`/`serial`) let the HOST synthesize a spec-correct EDID for the
/// virtual head: real millimetres give the right DPI (fonts scale correctly),
/// and the vendor/product identity can be surfaced to pro apps. We deliberately
/// do NOT ship a raw EDID blob from macOS clients — Apple Silicon exposes no
/// raw EDID at all (the DCP framebuffer path has no `IODisplayEDID`), so the
/// host synthesizes from these attributes instead. The `edid` field stays as an
/// optional passthrough for platforms that *do* expose a real blob (Intel Macs,
/// some external displays); empty ⇒ "host, please synthesize".
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ClientMonitor {
    /// Client-local display id (macOS CGDirectDisplayID). Session-local, not
    /// stable across reconnect — used only for the legacy compatibility
    /// roster. Multi-monitor-v1 uses [`ClientDisplayId`] instead.
    pub id: u32,
    /// Position of this monitor in the client's virtual desktop, in points.
    /// The primary monitor is at (0, 0).
    pub x: i32,
    pub y: i32,
    /// True framebuffer pixels (Retina-aware). Even by construction.
    pub width_px: u32,
    pub height_px: u32,
    /// Backing scale factor (1.0 non-Retina, 2.0 Retina, 1.5 some Intel modes).
    pub scale: f32,
    /// Nominal refresh rate in Hz (60 when the OS reports 0, e.g. built-in panels).
    pub refresh_hz: u32,
    /// Exactly one monitor in a layout is primary.
    pub is_primary: bool,
    /// Human-readable name for logs / the Displays panel.
    pub name: String,
    /// Physical size in millimetres (`CGDisplayScreenSize`). 0.0 when unknown;
    /// the host then falls back to deriving mm from `scale`.
    #[serde(default)]
    pub width_mm: f32,
    #[serde(default)]
    pub height_mm: f32,
    /// Display identity (`CGDisplay{Vendor,Model,Serial}Number`). 0 when unknown.
    /// The host may fold these into the synthesized EDID's manufacturer/product
    /// fields (external displays); built-in Apple panels keep the Flame-friendly
    /// Eizo identity by default.
    #[serde(default)]
    pub vendor: u32,
    #[serde(default)]
    pub model: u32,
    #[serde(default)]
    pub serial: u32,
    /// Optional raw EDID (base64). Empty on macOS/Apple-Silicon (no raw EDID
    /// exposed) ⇒ host synthesizes from the attributes above. Reserved for
    /// platforms that expose a real blob.
    #[serde(default)]
    pub edid: String,
}

#[non_exhaustive]
#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct AuthResponse {
    #[serde(rename = "type")]
    pub msg_type: String,
    pub method: String,
    pub username: String,
    pub credential: String,
    /// Primary display size in physical pixels. The host reads this from the
    /// auth response (before it creates the session) to size the session's X
    /// server — see `server/main.py` `create_session(width, height)`.
    pub screen_width: u32,
    pub screen_height: u32,
    /// Full client display layout. Empty when enumeration is unavailable, in
    /// which case the host falls back to its configured default size.
    #[serde(default)]
    pub monitors: Vec<ClientMonitor>,
    /// Additive multi-monitor-v1 auth-time request carrying the requested
    /// topology plus the client's ordered carrier support after the host
    /// explicitly advertised an offer. Old peers ignore it and remain
    /// primary-only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub multi_monitor_v1: Option<AuthMultiMonitorRequestMsg>,
    /// How the client wants displays handled: "match_layout" | "single_primary"
    /// | "windowed" | "pick".
    #[serde(default)]
    pub displays_mode: String,
    /// Video intent and decode limits available before host display/encoder
    /// creation. Old clients omit this and retain the previous host-default
    /// startup followed by `quality_settings` behavior.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_video: Option<InitialVideoRequestMsg>,
    /// Attempt-scoped diagnostic correlation UUID generated by the Deck.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_log_id: Option<String>,
    /// Lowercase SHA-256 acknowledgment of the exact displayed disclaimer text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disclaimer_acceptance_sha256: Option<String>,
    /// Client IANA time-zone identifier captured for this connection attempt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    /// Explicit opt-in requested during successful initial authentication.
    #[serde(default, skip_serializing_if = "is_false")]
    pub resume_requested: bool,
    /// 32-byte Deck holder nonce encoded by the integrating client.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_holder_nonce: Option<String>,
    /// Opaque direct-transport resume grant presented on a resume attempt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_grant: Option<String>,
    /// Requested cursor authority for this connection.
    #[serde(default, skip_serializing_if = "is_local_cursor_mode")]
    pub cursor_preference: CursorMode,
    /// Explicit client instruction to replace a persistent desktop whose
    /// committed multi-monitor topology cannot serve this request.
    ///
    /// A desktop's committed topology is fixed for its whole lifetime, so a
    /// desktop created without one can never grow into Match My Layout on a
    /// later reconnect. Replacing it is destructive -- the user's running
    /// remote applications are lost -- so the host must never infer this
    /// from the mismatch alone. The client sets it only after the user has
    /// explicitly chosen to start fresh. Absent and `false` for every
    /// existing peer, which keeps the previous refuse-and-explain behaviour.
    #[serde(default, skip_serializing_if = "is_false")]
    pub replace_incompatible_desktop: bool,
}

impl std::fmt::Debug for AuthResponse {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AuthResponse")
            .field("msg_type", &self.msg_type)
            .field("method", &self.method)
            .field("username", &self.username)
            .field("credential", &"<redacted>")
            .field("screen_width", &self.screen_width)
            .field("screen_height", &self.screen_height)
            .field("monitors", &self.monitors)
            .field("multi_monitor_v1", &self.multi_monitor_v1)
            .field("displays_mode", &self.displays_mode)
            .field("initial_video", &self.initial_video)
            .field("session_log_id", &self.session_log_id)
            .field(
                "disclaimer_acceptance_sha256",
                &self.disclaimer_acceptance_sha256,
            )
            .field("timezone", &self.timezone)
            .field("resume_requested", &self.resume_requested)
            .field(
                "resume_holder_nonce",
                &self.resume_holder_nonce.as_ref().map(|_| "<redacted>"),
            )
            .field(
                "resume_grant",
                &self.resume_grant.as_ref().map(|_| "<redacted>"),
            )
            .field("cursor_preference", &self.cursor_preference)
            .finish()
    }
}

impl AuthResponse {
    pub fn password(username: impl Into<String>, credential: impl Into<String>) -> Self {
        Self {
            msg_type: AUTH_RESPONSE.to_string(),
            method: "password".to_string(),
            username: username.into(),
            credential: credential.into(),
            screen_width: 0,
            screen_height: 0,
            monitors: Vec::new(),
            multi_monitor_v1: None,
            displays_mode: String::new(),
            initial_video: None,
            session_log_id: None,
            disclaimer_acceptance_sha256: None,
            timezone: None,
            resume_requested: false,
            resume_holder_nonce: None,
            resume_grant: None,
            cursor_preference: CursorMode::Local,
            replace_incompatible_desktop: false,
        }
    }

    pub fn pam(username: impl Into<String>, password: impl Into<String>) -> Self {
        Self {
            msg_type: AUTH_RESPONSE.to_string(),
            method: "pam".to_string(),
            username: username.into(),
            credential: password.into(),
            screen_width: 0,
            screen_height: 0,
            monitors: Vec::new(),
            multi_monitor_v1: None,
            displays_mode: String::new(),
            initial_video: None,
            session_log_id: None,
            disclaimer_acceptance_sha256: None,
            timezone: None,
            resume_requested: false,
            resume_holder_nonce: None,
            resume_grant: None,
            cursor_preference: CursorMode::Local,
            replace_incompatible_desktop: false,
        }
    }

    pub fn token(username: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            msg_type: AUTH_RESPONSE.to_string(),
            method: "token".to_string(),
            username: username.into(),
            credential: token.into(),
            screen_width: 0,
            screen_height: 0,
            monitors: Vec::new(),
            multi_monitor_v1: None,
            displays_mode: String::new(),
            initial_video: None,
            session_log_id: None,
            disclaimer_acceptance_sha256: None,
            timezone: None,
            resume_requested: false,
            resume_holder_nonce: None,
            resume_grant: None,
            cursor_preference: CursorMode::Local,
            replace_incompatible_desktop: false,
        }
    }

    /// Creates a credential-free response for optional no-auth metadata exchange.
    pub fn none() -> Self {
        Self {
            msg_type: AUTH_RESPONSE.to_string(),
            method: "none".to_string(),
            username: String::new(),
            credential: String::new(),
            screen_width: 0,
            screen_height: 0,
            monitors: Vec::new(),
            multi_monitor_v1: None,
            displays_mode: String::new(),
            initial_video: None,
            session_log_id: None,
            disclaimer_acceptance_sha256: None,
            timezone: None,
            resume_requested: false,
            resume_holder_nonce: None,
            resume_grant: None,
            cursor_preference: CursorMode::Local,
            replace_incompatible_desktop: false,
        }
    }

    /// Creates a credential-free direct-transport resume response.
    pub fn resume(holder_nonce: impl Into<String>, resume_grant: impl Into<String>) -> Self {
        Self {
            method: AUTH_METHOD_RESUME.to_owned(),
            resume_holder_nonce: Some(holder_nonce.into()),
            resume_grant: Some(resume_grant.into()),
            ..Self::none()
        }
    }

    /// Attach the enumerated client display layout. Sets `screen_width/height`
    /// from the primary monitor (falling back to the first monitor) so the host
    /// sizes the session's X server to the client's real primary resolution.
    pub fn with_displays(
        mut self,
        monitors: Vec<ClientMonitor>,
        displays_mode: impl Into<String>,
    ) -> Self {
        if let Some(primary) = monitors
            .iter()
            .find(|m| m.is_primary)
            .or_else(|| monitors.first())
        {
            self.screen_width = primary.width_px;
            self.screen_height = primary.height_px;
        }
        self.displays_mode = displays_mode.into();
        self.monitors = monitors;
        self
    }

    /// Attaches an additive multi-monitor-v1 request that was admitted against
    /// a validated pre-auth host offer, using the client's ordered
    /// carrier-support list to prove a pre-`ServerHello` intersection, and
    /// updates the legacy primary-compatible monitor roster and display mode.
    pub fn with_multi_monitor_v1(
        mut self,
        offer: AdvertisedMultiMonitorOffer<'_>,
        requested_topology: RequestedMonitorTopologyMsg,
        carriers: Vec<MultiMonitorCarrierMsg>,
    ) -> Result<Self, MultiMonitorValidationError> {
        crate::multi_monitor::attach_auth_response_multi_monitor_v1(
            &mut self,
            offer,
            requested_topology,
            carriers,
        )?;
        Ok(self)
    }

    /// Attaches the client IANA time-zone identifier.
    #[must_use]
    pub fn with_timezone(mut self, timezone: impl Into<String>) -> Self {
        self.timezone = Some(timezone.into());
        self
    }

    /// Attaches an optional client IANA time-zone identifier.
    #[must_use]
    pub fn with_optional_timezone(mut self, timezone: Option<String>) -> Self {
        self.timezone = timezone;
        self
    }

    /// Requests resume capability during initial authentication.
    #[must_use]
    pub fn with_resume_requested(mut self, holder_nonce: impl Into<String>) -> Self {
        self.resume_requested = true;
        self.resume_holder_nonce = Some(holder_nonce.into());
        self
    }

    /// Attaches the requested cursor authority.
    #[must_use]
    pub const fn with_cursor_preference(mut self, preference: CursorMode) -> Self {
        self.cursor_preference = preference;
        self
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AuthResult {
    #[serde(rename = "type")]
    pub msg_type: String,
    pub success: bool,
    #[serde(default)]
    pub message: String,
    /// Newly issued opaque direct-transport resume grant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_grant: Option<String>,
    /// Host-selected resume window in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_window_secs: Option<u32>,
    /// Whether this authentication attached to an existing session.
    #[serde(default, skip_serializing_if = "is_false")]
    pub resumed: bool,
    /// Stable machine-readable resume failure code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<ResumeErrorCode>,
}

impl std::fmt::Debug for AuthResult {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AuthResult")
            .field("msg_type", &self.msg_type)
            .field("success", &self.success)
            .field("message", &self.message)
            .field(
                "resume_grant",
                &self.resume_grant.as_ref().map(|_| "<redacted>"),
            )
            .field("resume_window_secs", &self.resume_window_secs)
            .field("resumed", &self.resumed)
            .field("error_code", &self.error_code)
            .finish()
    }
}

/// Stable direct-resume failure codes carried by protocol v3.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ResumeErrorCode {
    /// Host does not support direct resume.
    Unsupported,
    /// Presented resume authority expired.
    Expired,
    /// Presented grant generation/nonce was already consumed.
    Replayed,
    /// Native OS principal or native session changed.
    NativeIdentityChanged,
    /// Host identity or direct topology changed.
    TopologyChanged,
    /// Bound active session no longer exists.
    SessionGone,
    /// Host failed without exposing internal details.
    InternalFailure,
}

const fn is_false(value: &bool) -> bool {
    !*value
}

/// Pointer coordinate transport used by an input message.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PointerMotionMode {
    #[default]
    Absolute,
    Relative,
}

/// Requested or active cursor rendering authority.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CursorMode {
    #[default]
    Local,
    Host,
}

/// Requested or active tablet mode.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TabletModeMsg {
    /// AppKit/local-termination typed pen path over input-v3.
    #[default]
    LocalTermination,
    /// Full USB bridge path for host-native Wacom stack/device semantics.
    WacomUsbBridge,
    /// Disable tablet redirection and keep mouse compatibility behavior.
    DisabledMouseCompat,
}

const fn is_absolute_motion_mode(mode: &PointerMotionMode) -> bool {
    matches!(mode, PointerMotionMode::Absolute)
}

const fn is_local_cursor_mode(mode: &CursorMode) -> bool {
    matches!(mode, CursorMode::Local)
}

/// Proven availability of one input capability.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InputCapabilityAvailability {
    Available,
    Unavailable,
    #[default]
    Unknown,
}

/// Typed input capability truth carried in client and server hellos.
///
/// The `pen*` fields were added in input v3 and mirror
/// `arcen_input::InputCapabilities`'s pen truth exactly. `#[serde(default)]`
/// means every field is `Unknown` on a peer that predates it, and `Unknown`
/// never authorizes typed pen — both peers must prove `Available` (and input
/// v3) before either sends or expects `PenEventMsg`/`PEN_EVENT`. Input v4 adds
/// the independent `region_input` proof required by every Region* command.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct InputCapabilitiesMsg {
    #[serde(default)]
    pub absolute_pointer: InputCapabilityAvailability,
    #[serde(default)]
    pub relative_pointer: InputCapabilityAvailability,
    #[serde(default)]
    pub host_cursor: InputCapabilityAvailability,
    /// Region-scoped pointer and pen DTOs (`region_pointer_*`,
    /// `region_pen_event`). Legal only with input protocol v4+.
    #[serde(default)]
    pub region_input: InputCapabilityAvailability,
    /// Pen digitizer availability.
    #[serde(default)]
    pub pen: InputCapabilityAvailability,
    /// Pen pressure availability.
    #[serde(default)]
    pub pen_pressure: InputCapabilityAvailability,
    /// Pen X/Y tilt availability.
    #[serde(default)]
    pub pen_tilt: InputCapabilityAvailability,
    /// Pen barrel rotation availability.
    #[serde(default)]
    pub pen_rotation: InputCapabilityAvailability,
    /// Pen eraser-end availability.
    #[serde(default)]
    pub pen_eraser: InputCapabilityAvailability,
    /// Pen proximity/hover availability.
    #[serde(default)]
    pub pen_proximity: InputCapabilityAvailability,
}

/// Whether one peer has explicitly advertised region-input-v1 support.
#[must_use]
pub const fn supports_region_input_v1(
    input_protocol_version: u32,
    capabilities: InputCapabilitiesMsg,
) -> bool {
    input_protocol_version >= REGION_INPUT_PROTOCOL_VERSION
        && matches!(
            capabilities.region_input,
            InputCapabilityAvailability::Available
        )
}

/// Host/client truth for tablet-mode availability.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct TabletModeCapabilitiesMsg {
    #[serde(default)]
    pub local_termination: InputCapabilityAvailability,
    #[serde(default)]
    pub wacom_usb_bridge: InputCapabilityAvailability,
    #[serde(default)]
    pub disabled_mouse_compat: InputCapabilityAvailability,
}

impl Default for TabletModeCapabilitiesMsg {
    fn default() -> Self {
        Self {
            local_termination: InputCapabilityAvailability::Unknown,
            wacom_usb_bridge: InputCapabilityAvailability::Unknown,
            disabled_mouse_compat: InputCapabilityAvailability::Available,
        }
    }
}

fn is_default_tablet_mode(mode: &TabletModeMsg) -> bool {
    matches!(mode, TabletModeMsg::LocalTermination)
}

fn is_default_tablet_mode_capabilities(capabilities: &TabletModeCapabilitiesMsg) -> bool {
    *capabilities == TabletModeCapabilitiesMsg::default()
}

/// Clipboard v1 payload kind.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ClipboardContentKind {
    TextUtf8,
    ImagePng,
}

/// Host-authoritative clipboard direction advertised during setup.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ClipboardDirectionMsg {
    Both,
    ClientToHost,
    HostToClient,
    Disabled,
}

/// Host-authoritative clipboard content policy advertised during setup.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ClipboardContentMsg {
    All,
    Text,
    Image,
}

/// Exact clipboard subprotocol and host policy.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClipboardPolicyMsg {
    pub protocol_version: u16,
    pub direction: ClipboardDirectionMsg,
    pub content: ClipboardContentMsg,
    pub max_bytes: u32,
}

impl ClipboardPolicyMsg {
    /// Returns true only for the one supported clipboard subprotocol version.
    #[must_use]
    pub const fn is_v1(self) -> bool {
        self.protocol_version == CLIPBOARD_PROTOCOL_VERSION
    }
}

/// Clipboard offer metadata. Payload bytes follow in binary clipboard chunks.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClipboardDataMsg {
    #[serde(rename = "type")]
    pub msg_type: String,
    pub sequence: u64,
    pub kind: ClipboardContentKind,
    pub size_bytes: u32,
    #[serde(default, skip_serializing_if = "is_false")]
    pub truncated: bool,
}

impl ClipboardDataMsg {
    #[must_use]
    pub fn new(
        sequence: u64,
        kind: ClipboardContentKind,
        size_bytes: u32,
        truncated: bool,
    ) -> Self {
        Self {
            msg_type: CLIPBOARD_DATA.to_owned(),
            sequence,
            kind,
            size_bytes,
            truncated,
        }
    }
}

/// Physical or explicitly synthetic USB device bound to one Hard USB
/// attachment. The host uses these facts only as policy input; they never
/// authorize a device on their own.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct UsbHardDeviceMsg {
    pub vendor_id: u16,
    pub product_id: u16,
    pub bcd_device: u16,
    pub device_class: u8,
    pub speed: UsbSpeed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ClientHelloMsg {
    #[serde(rename = "type")]
    pub msg_type: String,
    pub client_name: String,
    pub version: String,
    pub screen_width: u32,
    pub screen_height: u32,
    pub supports_h264: bool,
    pub supports_h265: bool,
    pub supports_av1: bool,
    pub supports_yuv444: bool,
    /// Client can decode ten-bit. Probed at runtime, not assumed: on macOS
    /// there is no API that answers this per profile, so the only truth is a
    /// real decode session.
    #[serde(default)]
    pub supports_main10: bool,
    /// Client can decode twelve-bit.
    #[serde(default)]
    pub supports_main12: bool,
    /// Client can consume full-range coded samples.
    #[serde(default)]
    pub supports_full_range: bool,
    /// Client can consume an identity (GBR) matrix stream.
    #[serde(default)]
    pub supports_identity_matrix: bool,
    /// Client can consume a BT.601 matrix stream.
    #[serde(default)]
    pub supports_bt601_matrix: bool,
    /// Client can consume a BT.2020 non-constant-luminance matrix stream.
    #[serde(default)]
    pub supports_bt2020_ncl_matrix: bool,
    pub supports_audio: bool,
    /// Exact audio-output support. Absent means legacy PCM behavior.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_output: Option<AudioOutputCapabilitiesMsg>,
    /// Client microphone producer support. Absent means no upstream audio.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub microphone_output: Option<MicrophoneCapabilitiesMsg>,
    pub supports_pen: bool,
    /// Explicit, negotiated opt-in to the quarantined experimental-raw-hid
    /// vendor passthrough (Wacom/Huion/XP-Pen/UC-Logic/Gaomon). `true` only
    /// when the client was built with the `experimental-raw-hid` Cargo
    /// feature AND an operator explicitly enabled it at runtime. Absent/false
    /// on every old peer and on all default production builds — this is not
    /// USB bridging and must never imply it. The host must still apply its
    /// own independent runtime opt-in and bounds before admitting anything.
    #[serde(default)]
    pub experimental_raw_hid: bool,
    /// Explicit client support for Hard USB bridge v1. Host policy remains
    /// authoritative and old/default clients send false.
    #[serde(default)]
    pub usb_hard_v1: bool,
    /// Exact device facts for the captured Hard USB attachment. Absent on old
    /// clients and whenever capture was not completed before `client_hello`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usb_hard_device: Option<UsbHardDeviceMsg>,
    pub decoder_backend: String,
    pub capture_mode: String,
    pub picked_monitor_id: i32,
    pub picked_monitor_name: String,
    /// Exact clipboard subprotocol version. Zero means disabled.
    #[serde(default, skip_serializing_if = "is_zero_u16")]
    pub clipboard_protocol_version: u16,
    #[serde(default)]
    pub clipboard_text_c2s: bool,
    #[serde(default)]
    pub clipboard_text_s2c: bool,
    #[serde(default)]
    pub clipboard_image_c2s: bool,
    #[serde(default)]
    pub clipboard_image_s2c: bool,
    #[serde(default)]
    pub input_protocol_version: u32,
    #[serde(default, skip_serializing_if = "is_default_input_capabilities")]
    pub input_capabilities: InputCapabilitiesMsg,
    #[serde(default, skip_serializing_if = "is_local_cursor_mode")]
    pub cursor_preference: CursorMode,
    /// Tablet mode requested by the client for this connection.
    #[serde(default, skip_serializing_if = "is_default_tablet_mode")]
    pub tablet_mode_requested: TabletModeMsg,
    /// Client-side mode capability truth (local monitor/backend/runtime).
    #[serde(default, skip_serializing_if = "is_default_tablet_mode_capabilities")]
    pub tablet_mode_capabilities: TabletModeCapabilitiesMsg,
    pub device_capabilities: BTreeMap<String, Value>,
    /// Attempt-scoped diagnostic correlation UUID. Hosts that already consumed
    /// it from `AuthResponse` use this only as a consistency echo.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_log_id: Option<String>,
    /// Client IANA time-zone identifier captured for this connection attempt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    /// Initial client network-path snapshot. Absent means unavailable/legacy;
    /// subsequent snapshots ride `HealthPingMsg.client_telemetry.network`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network_snapshot: Option<ClientNetworkSnapshotMsg>,
    /// Transport identifier for the socket carrying this handshake.
    ///
    /// The current direct client sends exactly one `CAPABILITY_TRANSPORT_*`
    /// value from `arcen-transport` (`"transport:quic-v1"` in product builds).
    /// The host verifies that it matches the already accepted socket. Dormant
    /// compatibility builds may additionally accept their feature-gated value.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub transport_capabilities: Vec<String>,
}

impl Default for ClientHelloMsg {
    fn default() -> Self {
        Self {
            msg_type: CLIENT_HELLO.to_string(),
            client_name: "Arcen Deck".to_string(),
            version: "3.0.0".to_string(),
            screen_width: 1920,
            screen_height: 1080,
            supports_h264: true,
            supports_h265: true,
            supports_av1: true,
            supports_yuv444: true,
            supports_main10: false,
            supports_main12: false,
            supports_full_range: false,
            supports_identity_matrix: false,
            supports_bt601_matrix: false,
            supports_bt2020_ncl_matrix: false,
            supports_audio: true,
            audio_output: None,
            microphone_output: None,
            supports_pen: true,
            experimental_raw_hid: false,
            usb_hard_v1: false,
            usb_hard_device: None,
            decoder_backend: String::new(),
            capture_mode: "mirror_all".to_string(),
            picked_monitor_id: -1,
            picked_monitor_name: String::new(),
            clipboard_protocol_version: 0,
            clipboard_text_c2s: false,
            clipboard_text_s2c: false,
            clipboard_image_c2s: false,
            clipboard_image_s2c: false,
            input_protocol_version: INPUT_PROTOCOL_VERSION,
            input_capabilities: InputCapabilitiesMsg::default(),
            cursor_preference: CursorMode::Local,
            tablet_mode_requested: TabletModeMsg::LocalTermination,
            tablet_mode_capabilities: TabletModeCapabilitiesMsg::default(),
            device_capabilities: BTreeMap::new(),
            session_log_id: None,
            timezone: None,
            network_snapshot: None,
            transport_capabilities: Vec::new(),
        }
    }
}

impl ClientHelloMsg {
    #[must_use]
    pub fn with_build_identity(mut self, identity: BuildIdentityMsg) -> Self {
        attach_build_identity(&mut self.device_capabilities, identity);
        self
    }

    pub fn build_identity(&self) -> Result<Option<BuildIdentityMsg>, serde_json::Error> {
        decode_build_identity(&self.device_capabilities)
    }

    /// Returns the client's effective tablet-mode capability truth.
    ///
    /// PR #108 clients predate the explicit mode fields but already advertise
    /// input-v3 pen support. Preserve that local-termination compatibility only
    /// when the new field is absent (`Unknown`); an explicit `Unavailable`
    /// remains authoritative.
    #[must_use]
    pub fn effective_tablet_mode_capabilities(&self) -> TabletModeCapabilitiesMsg {
        let mut capabilities = self.tablet_mode_capabilities;
        if capabilities.local_termination == InputCapabilityAvailability::Unknown
            && self.input_protocol_version >= PEN_INPUT_PROTOCOL_VERSION
            && self.input_capabilities.pen == InputCapabilityAvailability::Available
        {
            capabilities.local_termination = InputCapabilityAvailability::Available;
        }
        capabilities
    }

    /// Overwrite the placeholder `screen_width/height` with the client's real
    /// primary display size. The host also re-reads screen size from this
    /// message (after the auth response), so a stale 1920x1080 here would
    /// clobber the honest value ingested at auth time. Keep both in agreement.
    pub fn with_display_size(mut self, monitors: &[ClientMonitor]) -> Self {
        if let Some(primary) = monitors
            .iter()
            .find(|m| m.is_primary)
            .or_else(|| monitors.first())
        {
            self.screen_width = primary.width_px;
            self.screen_height = primary.height_px;
        }
        self
    }

    /// Attaches the client IANA time-zone identifier.
    #[must_use]
    pub fn with_timezone(mut self, timezone: impl Into<String>) -> Self {
        self.timezone = Some(timezone.into());
        self
    }

    /// Attaches an optional client IANA time-zone identifier.
    #[must_use]
    pub fn with_optional_timezone(mut self, timezone: Option<String>) -> Self {
        self.timezone = timezone;
        self
    }

    /// Attaches the same cursor preference sent during authentication.
    #[must_use]
    pub const fn with_cursor_preference(mut self, preference: CursorMode) -> Self {
        self.cursor_preference = preference;
        self
    }

    /// Attaches an optional initial client network-path snapshot.
    #[must_use]
    pub fn with_network_snapshot(mut self, snapshot: Option<ClientNetworkSnapshotMsg>) -> Self {
        self.network_snapshot = snapshot;
        self
    }

    /// Attaches additive multi-monitor-v1 capabilities and requested-roster
    /// echo inside `device_capabilities`, and keeps the legacy primary screen
    /// size coherent when a requested topology is present.
    ///
    /// # Errors
    ///
    /// Returns an error when the capability object is invalid or cannot be
    /// serialized.
    pub fn with_multi_monitor_v1(
        mut self,
        capability: &ClientMultiMonitorMsg,
    ) -> Result<Self, MultiMonitorCapabilityError> {
        crate::multi_monitor::attach_client_hello_multi_monitor_v1(&mut self, capability)?;
        Ok(self)
    }

    /// Reads additive multi-monitor-v1 capability metadata from
    /// `device_capabilities`.
    ///
    /// # Errors
    ///
    /// Returns an error when the stored capability object cannot be parsed.
    pub fn multi_monitor_v1(&self) -> Result<Option<ClientMultiMonitorMsg>, serde_json::Error> {
        crate::multi_monitor::decode_client_hello_multi_monitor_v1(self)
    }
}

fn is_default_input_capabilities(capabilities: &InputCapabilitiesMsg) -> bool {
    *capabilities == InputCapabilitiesMsg::default()
}

/// Host colour capability advertised in `server_hello`.
///
/// This previously existed but was hardcoded to `main10: false` by every host,
/// which made it a decorative field. It now carries the resolved backend's real
/// capability so a client can tell what the host could serve, separately from
/// what it *is* serving.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ServerColorCaps {
    /// Host can encode ten-bit.
    #[serde(default)]
    pub main10: bool,
    /// Host can encode twelve-bit. Software backends only.
    #[serde(default)]
    pub main12: bool,
    #[serde(default)]
    pub chroma_422: bool,
    #[serde(default)]
    pub chroma_444: bool,
    /// Host can emit full-range coded samples.
    #[serde(default)]
    pub full_range: bool,
    /// Host can encode with an identity (GBR) matrix.
    #[serde(default)]
    pub identity_matrix: bool,
    /// Resolved bit depth actually in use, as a `BitDepth` token.
    #[serde(default = "default_bit_depth")]
    pub active_bit_depth: String,
    /// Resolved colour range actually in use.
    #[serde(default = "default_color_range")]
    pub active_range: String,
    /// Resolved matrix coefficients actually in use.
    #[serde(default = "default_color_matrix")]
    pub active_matrix: String,
    /// Resolved colour primaries actually in use.
    #[serde(default = "default_color_primaries")]
    pub active_primaries: String,
    /// Resolved transfer characteristics actually in use.
    #[serde(default = "default_transfer")]
    pub active_transfer: String,
    #[serde(default = "default_pix_fmt")]
    pub advertised_pix_fmt: String,
    #[serde(default = "default_negotiated_state")]
    pub negotiated_state: String,
}

fn default_bit_depth() -> String {
    "8".to_string()
}

fn default_color_range() -> String {
    "limited".to_string()
}

fn default_color_matrix() -> String {
    "bt709".to_string()
}

/// Latency-first, matching what Arcen has always encoded.
fn default_encode_intent() -> String {
    "interactive".to_string()
}

fn default_color_primaries() -> String {
    "bt709".to_string()
}

fn default_transfer() -> String {
    "bt709".to_string()
}

fn default_pix_fmt() -> String {
    "p010le".to_string()
}

fn default_negotiated_state() -> String {
    "not_supported".to_string()
}

impl Default for ServerColorCaps {
    fn default() -> Self {
        Self {
            main10: false,
            main12: false,
            chroma_422: false,
            chroma_444: false,
            full_range: false,
            identity_matrix: false,
            active_bit_depth: default_bit_depth(),
            active_range: default_color_range(),
            active_matrix: default_color_matrix(),
            active_primaries: default_color_primaries(),
            active_transfer: default_transfer(),
            advertised_pix_fmt: default_pix_fmt(),
            negotiated_state: default_negotiated_state(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ServerHelloMsg {
    #[serde(rename = "type")]
    pub msg_type: String,
    #[serde(default = "default_server_name")]
    pub server_name: String,
    #[serde(default = "default_version")]
    pub version: String,
    /// Canonical authenticated OS username. Empty when no OS session exists.
    #[serde(default)]
    pub os_user: String,
    /// logind/session-manager identifier. Empty when unavailable.
    #[serde(default)]
    pub session_id: String,
    /// Session transport type such as `x11`. Empty when unavailable.
    #[serde(default)]
    pub session_type: String,
    /// Authenticated graphical desktop such as `gnome-classic`. Empty when unavailable.
    #[serde(default)]
    pub desktop: String,
    #[serde(default = "default_width")]
    pub screen_width: u32,
    #[serde(default = "default_height")]
    pub screen_height: u32,
    /// Legacy host-defined monitor roster preserved byte-for-byte in its
    /// existing schema. Rich applied multi-monitor-v1 rosters stay inside the
    /// additive `multi_monitor_v1` capability instead of redefining this
    /// field.
    #[serde(default)]
    pub monitors: Vec<Value>,
    #[serde(default = "default_true")]
    pub supports_h264: bool,
    #[serde(default)]
    pub supports_h265: bool,
    #[serde(default)]
    pub supports_av1: bool,
    #[serde(default = "default_true")]
    pub supports_yuv444: bool,
    #[serde(default = "default_true")]
    pub supports_audio: bool,
    /// Exact host audio-output support. Absent means legacy PCM behavior.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_output: Option<AudioOutputCapabilitiesMsg>,
    /// Host microphone-consumer support. Absent means upstream audio is forbidden.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub microphone_input: Option<MicrophoneCapabilitiesMsg>,
    #[serde(default = "default_true")]
    pub supports_pen: bool,
    /// Explicit, negotiated host support for the quarantined
    /// experimental-raw-hid vendor passthrough. `true` only when the host was
    /// built with the `experimental-raw-hid` Cargo feature AND an operator
    /// explicitly enabled it at runtime. Absent/false on every old host and
    /// on all default production builds. A client must still see this AND
    /// its own local opt-in before ever starting raw HID capture, and a host
    /// must still see the client's own `experimental_raw_hid` opt-in before
    /// admitting any descriptor or report. Not USB bridging.
    #[serde(default)]
    pub experimental_raw_hid: bool,
    /// Additive Hard USB bridge v1 capability. This means only that the host
    /// has a policy/backend eligible for negotiation; it never authorizes a
    /// device by itself.
    #[serde(default)]
    pub usb_hard_v1: bool,
    /// Host accepts mid-session `display_update` stream-resize requests.
    /// Absent (false) on hosts that predate the message or sessions without
    /// display control — clients must never send `display_update` unless this
    /// is true.
    #[serde(default)]
    pub supports_display_update: bool,
    #[serde(default)]
    pub requires_auth: bool,
    #[serde(default)]
    pub encoder_backend: String,
    /// Whether `encoder_backend` runs on dedicated silicon or the CPU.
    ///
    /// `"hardware"` or `"software"`. Empty from hosts that predate this field,
    /// in which case a client may fall back to guessing from
    /// `encoder_backend`'s name. New backends must set it: a name-based guess
    /// cannot classify a vendor it has never heard of.
    #[serde(default)]
    pub encoder_class: String,
    #[serde(default)]
    pub available_encoders: BTreeMap<String, Value>,
    #[serde(default = "default_codec")]
    pub codec: String,
    #[serde(default)]
    pub color_caps: ServerColorCaps,
    #[serde(default)]
    pub input_protocol_version: u32,
    #[serde(default)]
    pub input_capabilities: InputCapabilitiesMsg,
    /// Host-side mode capability truth (per-session/runtime-real).
    #[serde(default, skip_serializing_if = "is_default_tablet_mode_capabilities")]
    pub tablet_mode_capabilities: TabletModeCapabilitiesMsg,
    /// Host policy is absent for disabled, ineligible, and legacy sessions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clipboard: Option<ClipboardPolicyMsg>,
    #[serde(default)]
    pub device_capabilities: BTreeMap<String, Value>,
    /// Transport capability of the socket carrying this handshake.
    ///
    /// Protocol ordering requires the host to send `ServerHello` before it
    /// receives `ClientHello`, so this reports the already accepted QUIC socket
    /// rather than an in-band upgrade decision. The client must reject a value
    /// that differs from its selected socket. Absence is invalid in product
    /// builds. The value is a `CAPABILITY_TRANSPORT_*` identifier from
    /// `arcen-transport` (`"transport:quic-v1"` in product builds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub negotiated_transport: Option<String>,
}

impl ServerHelloMsg {
    /// Attaches additive multi-monitor-v1 capability metadata while preserving
    /// the existing top-level `server_hello.monitors` schema unchanged.
    ///
    /// # Errors
    ///
    /// Returns an error when the capability object is invalid or cannot be
    /// serialized.
    pub fn with_multi_monitor_v1(
        mut self,
        capability: &ServerMultiMonitorMsg,
    ) -> Result<Self, MultiMonitorCapabilityError> {
        crate::multi_monitor::attach_server_hello_multi_monitor_v1(&mut self, capability)?;
        Ok(self)
    }

    /// Reads additive multi-monitor-v1 capability metadata from
    /// `device_capabilities`.
    ///
    /// # Errors
    ///
    /// Returns an error when the stored capability object cannot be parsed.
    pub fn multi_monitor_v1(&self) -> Result<Option<ServerMultiMonitorMsg>, serde_json::Error> {
        crate::multi_monitor::decode_server_hello_multi_monitor_v1(self)
    }
}

const fn is_zero_u16(value: &u16) -> bool {
    *value == 0
}

/// Bounded host explanation for a cursor negotiation result.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(try_from = "String", into = "String")]
pub struct CursorModeReason(String);

impl CursorModeReason {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for CursorModeReason {
    type Error = CursorModeReasonError;

    fn try_from(reason: String) -> Result<Self, Self::Error> {
        if reason.len() > MAX_CURSOR_MODE_REASON_BYTES {
            return Err(CursorModeReasonError::TooLong);
        }
        if reason.chars().any(char::is_control) {
            return Err(CursorModeReasonError::ControlCharacter);
        }
        Ok(Self(reason))
    }
}

impl From<CursorModeReason> for String {
    fn from(reason: CursorModeReason) -> Self {
        reason.0
    }
}

/// Invalid cursor result reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorModeReasonError {
    TooLong,
    ControlCharacter,
}

impl std::fmt::Display for CursorModeReasonError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLong => formatter.write_str("cursor mode reason is too long"),
            Self::ControlCharacter => {
                formatter.write_str("cursor mode reason contains a control character")
            }
        }
    }
}

impl std::error::Error for CursorModeReasonError {}

/// Bounded host explanation for a tablet-mode negotiation result.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(try_from = "String", into = "String")]
pub struct TabletModeReason(String);

impl TabletModeReason {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for TabletModeReason {
    type Error = TabletModeReasonError;

    fn try_from(reason: String) -> Result<Self, Self::Error> {
        if reason.len() > MAX_TABLET_MODE_REASON_BYTES {
            return Err(TabletModeReasonError::TooLong);
        }
        if reason.chars().any(char::is_control) {
            return Err(TabletModeReasonError::ControlCharacter);
        }
        Ok(Self(reason))
    }
}

impl From<TabletModeReason> for String {
    fn from(reason: TabletModeReason) -> Self {
        reason.0
    }
}

/// Invalid tablet mode result reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabletModeReasonError {
    TooLong,
    ControlCharacter,
}

impl std::fmt::Display for TabletModeReasonError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLong => formatter.write_str("tablet mode reason is too long"),
            Self::ControlCharacter => {
                formatter.write_str("tablet mode reason contains a control character")
            }
        }
    }
}

impl std::error::Error for TabletModeReasonError {}

/// Host confirmation of the one tablet mode active for this connection.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TabletModeResultMsg {
    #[serde(rename = "type", default = "default_tablet_mode_result_type")]
    pub msg_type: String,
    #[serde(default)]
    pub requested: TabletModeMsg,
    #[serde(default)]
    pub active: TabletModeMsg,
    #[serde(default)]
    pub accepted: bool,
    #[serde(default)]
    pub reason: TabletModeReason,
    /// Policy reminder surfaced by hosts when changing mode requires reconnect.
    #[serde(default)]
    pub reconnect_required: bool,
}

fn default_tablet_mode_result_type() -> String {
    TABLET_MODE_RESULT.to_owned()
}

impl Default for TabletModeResultMsg {
    fn default() -> Self {
        Self {
            msg_type: default_tablet_mode_result_type(),
            requested: TabletModeMsg::LocalTermination,
            active: TabletModeMsg::LocalTermination,
            accepted: false,
            reason: TabletModeReason::default(),
            reconnect_required: false,
        }
    }
}

/// Host confirmation of the one cursor authority active for this connection.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CursorModeResultMsg {
    #[serde(rename = "type", default = "default_cursor_mode_result_type")]
    pub msg_type: String,
    #[serde(default)]
    pub requested: CursorMode,
    #[serde(default)]
    pub active: CursorMode,
    #[serde(default)]
    pub accepted: bool,
    #[serde(default)]
    pub reason: CursorModeReason,
}

fn default_cursor_mode_result_type() -> String {
    CURSOR_MODE_RESULT.to_owned()
}

impl Default for CursorModeResultMsg {
    fn default() -> Self {
        Self {
            msg_type: default_cursor_mode_result_type(),
            requested: CursorMode::Local,
            active: CursorMode::Local,
            accepted: false,
            reason: CursorModeReason::default(),
        }
    }
}

/// Host→client: the host cursor shape changed. Sent by the host whenever the
/// OS cursor shape changes while `cursor=local` authority is active, so the
/// client can mirror the correct cursor (e.g. resize arrows at window edges)
/// without waiting for the next video frame. Ignored by clients that do not
/// support cursor shape streaming; the client always falls back to its
/// default cursor shape (`CursorShapeKind::Default`) when this message has
/// not yet been received or when it has not been received for this connection.
///
/// Only legal while `CursorMode::Local` is active. When host cursor mode is
/// active the cursor is already embedded in the video stream; the host must
/// not send this message in that case.
pub const CURSOR_SHAPE: &str = "cursor_shape";

/// Named host OS cursor shapes. Covers the resize-edge, text, hand, and
/// common wait/progress shapes that matter most for remote desktop use.
/// Pixel bitmaps (custom application cursors) are a future extension.
///
/// Unknown variants received from a newer host are deserialised as
/// `CursorShapeKind::Default` via the custom `Deserialize` impl, so an
/// older client never fails to parse a `cursor_shape` message that carries
/// a shape it does not know about.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CursorShapeKind {
    /// The host OS default arrow cursor.
    #[default]
    Default,
    /// Text insertion (I-beam).
    Text,
    /// Resize cursor — vertical (north/south).
    ResizeNs,
    /// Resize cursor — horizontal (east/west).
    ResizeEw,
    /// Resize cursor — diagonal (north-west/south-east).
    ResizeNwse,
    /// Resize cursor — diagonal (north-east/south-west).
    ResizeNesw,
    /// All-direction move cursor.
    ResizeAll,
    /// Pointing hand / hyperlink.
    Pointer,
    /// Precision crosshair (e.g. graphics application).
    Crosshair,
    /// Grab/open hand.
    Grab,
    /// Grabbing/closed hand.
    Grabbing,
    /// Zoom in.
    ZoomIn,
    /// Zoom out.
    ZoomOut,
    /// Spinner / hour glass — blocked operation.
    Wait,
    /// Progress (busy, but UI still interactive).
    Progress,
    /// Help (question mark arrow).
    Help,
    /// Not allowed / no-entry.
    NotAllowed,
    /// The host cursor is hidden (full-screen application, game, etc.).
    /// The client should hide its local cursor too while this is active.
    Hidden,
}

impl<'de> serde::Deserialize<'de> for CursorShapeKind {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Ok(match s.as_str() {
            "default" => Self::Default,
            "text" => Self::Text,
            "resize_ns" => Self::ResizeNs,
            "resize_ew" => Self::ResizeEw,
            "resize_nwse" => Self::ResizeNwse,
            "resize_nesw" => Self::ResizeNesw,
            "resize_all" => Self::ResizeAll,
            "pointer" => Self::Pointer,
            "crosshair" => Self::Crosshair,
            "grab" => Self::Grab,
            "grabbing" => Self::Grabbing,
            "zoom_in" => Self::ZoomIn,
            "zoom_out" => Self::ZoomOut,
            "wait" => Self::Wait,
            "progress" => Self::Progress,
            "help" => Self::Help,
            "not_allowed" => Self::NotAllowed,
            "hidden" => Self::Hidden,
            // Any variant from a newer host that this build does not know
            // about falls back to Default rather than failing to parse the
            // whole message.
            _ => Self::Default,
        })
    }
}

/// Host→client cursor shape notification.
///
/// The host sends this whenever the cursor shape changes while
/// `cursor=local` authority is active. The client updates its displayed
/// cursor to match. Old clients that do not recognise this message type
/// ignore it silently (unknown messages are already discarded).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CursorShapeMsg {
    #[serde(rename = "type", default = "default_cursor_shape_type")]
    pub msg_type: String,
    /// Current cursor shape.
    #[serde(default)]
    pub shape: CursorShapeKind,
    /// Host-monotonic sequence, starting at 1. The client ignores any
    /// message whose sequence is not greater than the last one applied,
    /// so a reordered or late-arriving message cannot regress the shape.
    #[serde(default)]
    pub sequence: u64,
}

fn default_cursor_shape_type() -> String {
    CURSOR_SHAPE.to_owned()
}

impl Default for CursorShapeMsg {
    fn default() -> Self {
        Self {
            msg_type: default_cursor_shape_type(),
            shape: CursorShapeKind::Default,
            sequence: 0,
        }
    }
}

pub const MIN_STREAM_WIDTH: u32 = 320;
pub const MIN_STREAM_HEIGHT: u32 = 240;
pub const MAX_STREAM_WIDTH: u32 = 16384;
pub const MAX_STREAM_HEIGHT: u32 = 8640;

/// Client→host: retarget the single active stream surface mid-session.
///
/// Legal only after `client_hello`, and only when the host advertised
/// `supports_display_update` in `server_hello`. The host answers every
/// request with a [`DisplayUpdateResultMsg`] carrying the same `sequence`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DisplayUpdateMsg {
    #[serde(rename = "type", default = "default_display_update_type")]
    pub msg_type: String,
    /// Client-monotonic, starting at 1. The host ignores any request whose
    /// sequence is not greater than the last one it applied, so a stale
    /// in-flight resize can never override a newer one.
    #[serde(default)]
    pub sequence: u64,
    /// Requested stream size in pixels. Must be even and within the
    /// `MIN_STREAM_*`/`MAX_STREAM_*` bounds; the client pre-clamps.
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
    /// Backing scale the client applied (1.0 logical, 2.0 HiDPI). Diagnostic
    /// only — the host acts solely on `width`/`height`.
    #[serde(default)]
    pub scale: f32,
    /// Why the client asked: "connect_fit", "fullscreen", "resize",
    /// "retina_toggle". Logging only; free-form.
    #[serde(default)]
    pub reason: String,
}

fn default_display_update_type() -> String {
    DISPLAY_UPDATE.to_owned()
}

impl Default for DisplayUpdateMsg {
    fn default() -> Self {
        Self {
            msg_type: default_display_update_type(),
            sequence: 0,
            width: 0,
            height: 0,
            scale: 0.0,
            reason: String::new(),
        }
    }
}

/// Host→client answer to a [`DisplayUpdateMsg`]. `width`/`height` always
/// report the size actually streaming after the request was processed — the
/// new size when accepted, the unchanged current size when rejected.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DisplayUpdateResultMsg {
    #[serde(rename = "type", default = "default_display_update_result_type")]
    pub msg_type: String,
    #[serde(default)]
    pub sequence: u64,
    #[serde(default)]
    pub accepted: bool,
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
    /// Bounded human-readable reason on rejection; empty when accepted.
    #[serde(default)]
    pub message: String,
}

fn default_display_update_result_type() -> String {
    DISPLAY_UPDATE_RESULT.to_owned()
}

impl Default for DisplayUpdateResultMsg {
    fn default() -> Self {
        Self {
            msg_type: default_display_update_result_type(),
            sequence: 0,
            accepted: false,
            width: 0,
            height: 0,
            message: String::new(),
        }
    }
}

/// Bounded, control-character-free, non-empty client Wi-Fi identity (SSID)
/// safe to carry into host logs. Mirrors `arcen-telemetry`'s
/// `MAX_NETWORK_IDENTITY_BYTES` bound.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(try_from = "String", into = "String")]
pub struct NetworkIdentityMsg(String);

impl NetworkIdentityMsg {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for NetworkIdentityMsg {
    type Error = NetworkIdentityError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.is_empty() {
            return Err(NetworkIdentityError::Empty);
        }
        if value.len() > MAX_NETWORK_IDENTITY_BYTES {
            return Err(NetworkIdentityError::TooLong);
        }
        if value.chars().any(char::is_control) {
            return Err(NetworkIdentityError::ControlCharacter);
        }
        Ok(Self(value))
    }
}

impl From<NetworkIdentityMsg> for String {
    fn from(identity: NetworkIdentityMsg) -> Self {
        identity.0
    }
}

/// Invalid client network identity (SSID).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkIdentityError {
    Empty,
    TooLong,
    ControlCharacter,
}

impl std::fmt::Display for NetworkIdentityError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => formatter.write_str("network identity is empty"),
            Self::TooLong => formatter.write_str("network identity is too long"),
            Self::ControlCharacter => {
                formatter.write_str("network identity contains a control character")
            }
        }
    }
}

impl std::error::Error for NetworkIdentityError {}

/// Client network interface family, mirroring
/// `arcen-telemetry::InterfaceKind` exactly so both ends of a session share
/// one vocabulary. Always present when the containing snapshot is present;
/// the snapshot itself being absent is what means unavailable/legacy.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum NetworkInterfaceKind {
    Ethernet,
    Wifi,
    Cellular,
    Vpn,
    Loopback,
    Other,
}

/// Endpoint network scope, mirroring `arcen-telemetry::NetworkScope` exactly.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum NetworkScopeMsg {
    /// Private, loopback, link-local, or unique-local addressing.
    Lan,
    /// Public addressing.
    Wan,
}

/// Client network-path facts, validated against the same bounds as
/// `arcen-telemetry::NetworkSnapshot`. Sent once in `client_hello`;
/// subsequent snapshots ride `HealthPingMsg.client_telemetry.network` so host
/// logs can see path changes without a dedicated channel.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(
    try_from = "ClientNetworkSnapshotWire",
    into = "ClientNetworkSnapshotWire"
)]
pub struct ClientNetworkSnapshotMsg {
    interface_kind: NetworkInterfaceKind,
    scope: NetworkScopeMsg,
    link_mbps: Option<u32>,
    rssi_dbm: Option<i32>,
    mtu: Option<u32>,
    ssid: Option<NetworkIdentityMsg>,
}

impl ClientNetworkSnapshotMsg {
    /// Creates a validated client network snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error for a zero link rate, an MTU or RSSI outside the
    /// supported range, or Wi-Fi-only facts (SSID/RSSI) attached to a
    /// non-Wi-Fi interface. `ssid` is already a validated, non-empty,
    /// bounded, control-character-free [`NetworkIdentityMsg`].
    pub fn new(
        interface_kind: NetworkInterfaceKind,
        scope: NetworkScopeMsg,
        link_mbps: Option<u32>,
        rssi_dbm: Option<i32>,
        mtu: Option<u32>,
        ssid: Option<NetworkIdentityMsg>,
    ) -> Result<Self, ClientNetworkSnapshotError> {
        if link_mbps == Some(0) {
            return Err(ClientNetworkSnapshotError::InvalidLinkRate);
        }
        if mtu.is_some_and(|value| !(MIN_NETWORK_MTU..=MAX_NETWORK_MTU).contains(&value)) {
            return Err(ClientNetworkSnapshotError::InvalidMtu);
        }
        if rssi_dbm.is_some_and(|value| !(MIN_RSSI_DBM..=MAX_RSSI_DBM).contains(&value)) {
            return Err(ClientNetworkSnapshotError::InvalidRssi);
        }
        if interface_kind != NetworkInterfaceKind::Wifi && (ssid.is_some() || rssi_dbm.is_some()) {
            return Err(ClientNetworkSnapshotError::WifiFactsOnOtherInterface);
        }
        Ok(Self {
            interface_kind,
            scope,
            link_mbps,
            rssi_dbm,
            mtu,
            ssid,
        })
    }

    #[must_use]
    pub const fn interface_kind(&self) -> NetworkInterfaceKind {
        self.interface_kind
    }

    #[must_use]
    pub const fn scope(&self) -> NetworkScopeMsg {
        self.scope
    }

    #[must_use]
    pub const fn link_mbps(&self) -> Option<u32> {
        self.link_mbps
    }

    #[must_use]
    pub const fn rssi_dbm(&self) -> Option<i32> {
        self.rssi_dbm
    }

    #[must_use]
    pub const fn mtu(&self) -> Option<u32> {
        self.mtu
    }

    #[must_use]
    pub fn ssid(&self) -> Option<&str> {
        self.ssid.as_ref().map(NetworkIdentityMsg::as_str)
    }
}

/// Minimum supported network MTU: the smallest IPv4-mandated MTU
/// (RFC 791/1122), well below any standard Ethernet/Wi-Fi link.
pub const MIN_NETWORK_MTU: u32 = 576;
/// Maximum supported network MTU. This intentionally exceeds the 16-bit
/// `u16::MAX = 65,535` Ethernet/jumbo-frame ceiling because real loopback
/// interfaces report a larger, still-bounded value: Linux `lo` and current
/// Windows loopback adapters both default to an MTU of exactly 65,536 bytes.
/// A defensive `u32` field lets the wire represent that real, observed value
/// instead of rejecting a truthful loopback fact or silently clamping it.
pub const MAX_NETWORK_MTU: u32 = 65_536;
/// Minimum plausible Wi-Fi RSSI in dBm, aligned with `arcen-telemetry`.
pub const MIN_RSSI_DBM: i32 = -127;
/// Maximum plausible Wi-Fi RSSI in dBm (0 = strongest), aligned with
/// `arcen-telemetry`.
pub const MAX_RSSI_DBM: i32 = 0;

/// Untrusted wire shape for [`ClientNetworkSnapshotMsg`]; every value is
/// validated by [`ClientNetworkSnapshotMsg::new`] before it becomes a value
/// this crate hands to callers.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct ClientNetworkSnapshotWire {
    interface_kind: NetworkInterfaceKind,
    scope: NetworkScopeMsg,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    link_mbps: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rssi_dbm: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mtu: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ssid: Option<NetworkIdentityMsg>,
}

impl TryFrom<ClientNetworkSnapshotWire> for ClientNetworkSnapshotMsg {
    type Error = ClientNetworkSnapshotError;

    fn try_from(wire: ClientNetworkSnapshotWire) -> Result<Self, Self::Error> {
        Self::new(
            wire.interface_kind,
            wire.scope,
            wire.link_mbps,
            wire.rssi_dbm,
            wire.mtu,
            wire.ssid,
        )
    }
}

impl From<ClientNetworkSnapshotMsg> for ClientNetworkSnapshotWire {
    fn from(value: ClientNetworkSnapshotMsg) -> Self {
        Self {
            interface_kind: value.interface_kind,
            scope: value.scope,
            link_mbps: value.link_mbps,
            rssi_dbm: value.rssi_dbm,
            mtu: value.mtu,
            ssid: value.ssid,
        }
    }
}

/// Invalid client network snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientNetworkSnapshotError {
    InvalidLinkRate,
    InvalidMtu,
    InvalidRssi,
    WifiFactsOnOtherInterface,
}

impl std::fmt::Display for ClientNetworkSnapshotError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidLinkRate => {
                formatter.write_str("network link rate must be nonzero when available")
            }
            Self::InvalidMtu => formatter.write_str(&format!(
                "network MTU is outside {MIN_NETWORK_MTU}..={MAX_NETWORK_MTU}"
            )),
            Self::InvalidRssi => formatter.write_str(&format!(
                "network RSSI is outside {MIN_RSSI_DBM}..={MAX_RSSI_DBM} dBm"
            )),
            Self::WifiFactsOnOtherInterface => {
                formatter.write_str("SSID or RSSI was attached to a non-Wi-Fi interface")
            }
        }
    }
}

impl std::error::Error for ClientNetworkSnapshotError {}

/// Three-state operational health, mirroring `arcen-telemetry`'s health
/// vocabulary without depending on that crate. Carried only inside an
/// `Option`; the containing field being absent means "not assessed", not
/// `Ok`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum HealthStateMsg {
    Ok,
    Degraded,
    Critical,
}

/// Minimum QoS/health sample aggregation window, in seconds.
pub const MIN_SAMPLE_WINDOW_SECS: u32 = 1;
/// Maximum QoS/health sample aggregation window, in seconds (one hour) —
/// well above the fastest (2 s Debug) and slowest (60 s proof-of-life)
/// cadences in the observability contract.
pub const MAX_SAMPLE_WINDOW_SECS: u32 = 3_600;

/// Validated, nonzero, bounded QoS/health sample aggregation window.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(try_from = "u32", into = "u32")]
pub struct SampleWindowSecs(u32);

impl SampleWindowSecs {
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

impl TryFrom<u32> for SampleWindowSecs {
    type Error = SampleWindowSecsError;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        if !(MIN_SAMPLE_WINDOW_SECS..=MAX_SAMPLE_WINDOW_SECS).contains(&value) {
            return Err(SampleWindowSecsError);
        }
        Ok(Self(value))
    }
}

impl From<SampleWindowSecs> for u32 {
    fn from(value: SampleWindowSecs) -> Self {
        value.0
    }
}

/// Invalid QoS/health sample aggregation window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SampleWindowSecsError;

impl std::fmt::Display for SampleWindowSecsError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "sample window must be {MIN_SAMPLE_WINDOW_SECS}..={MAX_SAMPLE_WINDOW_SECS} seconds"
        )
    }
}

impl std::error::Error for SampleWindowSecsError {}

/// Bounded client-experience QoS sample. Every field is `Option`; absence
/// means the client did not have that fact for this window, never zero.
/// Timing fields are whole milliseconds (never negative/NaN by
/// construction) so this type stays `Eq`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClientQosSampleMsg {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frames_received: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frames_decoded: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frames_presented: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frames_dropped: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decode_time_ms: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_time_ms: Option<u32>,
    /// Client-observed time to hand an input event to its transport.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_send_time_ms: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_health: Option<HealthStateMsg>,
    /// Aggregation window this sample covers, in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_window_secs: Option<SampleWindowSecs>,
    /// Age of this sample relative to when the ping was sent, in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_age_ms: Option<u64>,
}

/// Bounded client telemetry carried on the existing 5-second health ping so a
/// host can (at Debug) show client experience/network facts end-to-end
/// without controlling the client's local log profile. No log-level, log
/// profile, or remote logging-control field is ever included here.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClientTelemetrySnapshotMsg {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qos: Option<ClientQosSampleMsg>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<ClientNetworkSnapshotMsg>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HealthPongMsg {
    #[serde(rename = "type")]
    pub msg_type: String,
    #[serde(default)]
    pub ping_timestamp_ms: u64,
    #[serde(default)]
    pub sequence: u64,
    #[serde(default)]
    pub server_timestamp_ms: u64,
    #[serde(default)]
    pub server_state: String,
}

impl Default for HealthPongMsg {
    fn default() -> Self {
        Self {
            msg_type: HEALTH_PONG.to_string(),
            ping_timestamp_ms: 0,
            sequence: 0,
            server_timestamp_ms: 0,
            server_state: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HealthPingMsg {
    #[serde(rename = "type", default = "default_health_ping_type")]
    pub msg_type: String,
    #[serde(default)]
    pub timestamp_ms: u64,
    /// Echoed by `HealthPongMsg.sequence` for application RTT. Reused as the
    /// one sequence mechanism for both ping/pong RTT and telemetry framing;
    /// no separate telemetry sequence is added.
    #[serde(default)]
    pub sequence: u64,
    #[serde(default)]
    pub client_state: String,
    /// Bounded client QoS/network facts. Absent on legacy clients or windows
    /// with nothing new to report; never a stand-in for zero/healthy values.
    /// Carries no log-level, log-profile, or remote logging-control field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_telemetry: Option<ClientTelemetrySnapshotMsg>,
}

fn default_health_ping_type() -> String {
    HEALTH_PING.to_string()
}

impl Default for HealthPingMsg {
    fn default() -> Self {
        Self {
            msg_type: default_health_ping_type(),
            timestamp_ms: 0,
            sequence: 0,
            client_state: String::new(),
            client_telemetry: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HealthStatsMsg {
    #[serde(rename = "type")]
    pub msg_type: String,
    #[serde(default)]
    pub rtt_ms: f64,
    #[serde(default)]
    pub fps_actual: f64,
    #[serde(default)]
    pub fps_target: f64,
    #[serde(default)]
    pub bandwidth_mbps: f64,
    #[serde(default)]
    pub frames_sent: u64,
    #[serde(default)]
    pub frames_dropped: u64,
    #[serde(default)]
    pub encode_time_ms: f64,
    #[serde(default)]
    pub capture_time_ms: f64,
    #[serde(default)]
    pub input_latency_ms: f64,
    #[serde(default)]
    pub input_events: u64,
    #[serde(default)]
    pub last_input_sequence: u64,
    #[serde(default)]
    pub last_input_type: String,
    #[serde(default)]
    pub transmit_time_ms: f64,
    #[serde(default)]
    pub decode_time_ms: f64,
    #[serde(default)]
    pub display_time_ms: f64,
    #[serde(default)]
    pub keyframe_requested: u64,
    #[serde(default)]
    pub keyframe_emitted: u64,
    #[serde(default = "default_codec")]
    pub codec: String,
    #[serde(default = "default_chroma")]
    pub chroma: String,
    #[serde(default = "default_resolution")]
    pub resolution: String,
    #[serde(default)]
    pub clients_connected: u64,
    /// Host-assessed operational health for this session. Absent on hosts
    /// that predate this field; never a stand-in for `Ok`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health_state: Option<HealthStateMsg>,
    /// Aggregation window these stats cover, in seconds. Absent means unknown
    /// cadence, not zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_window_secs: Option<SampleWindowSecs>,
}

fn default_chroma() -> String {
    "yuv444".to_string()
}

fn default_resolution() -> String {
    "1920x1080".to_string()
}

impl Default for HealthStatsMsg {
    fn default() -> Self {
        Self {
            msg_type: HEALTH_STATS.to_string(),
            rtt_ms: 0.0,
            fps_actual: 0.0,
            fps_target: 0.0,
            bandwidth_mbps: 0.0,
            frames_sent: 0,
            frames_dropped: 0,
            encode_time_ms: 0.0,
            capture_time_ms: 0.0,
            input_latency_ms: 0.0,
            input_events: 0,
            last_input_sequence: 0,
            last_input_type: String::new(),
            transmit_time_ms: 0.0,
            decode_time_ms: 0.0,
            display_time_ms: 0.0,
            keyframe_requested: 0,
            keyframe_emitted: 0,
            codec: default_codec(),
            chroma: default_chroma(),
            resolution: default_resolution(),
            clients_connected: 0,
            health_state: None,
            sample_window_secs: None,
        }
    }
}

fn default_server_name() -> String {
    "Arcen Pier".to_string()
}

fn default_version() -> String {
    "3.0.0".to_string()
}

fn default_width() -> u32 {
    1920
}

fn default_height() -> u32 {
    1080
}

fn default_codec() -> String {
    "h264".to_string()
}

const fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RequestFullFrameMsg {
    #[serde(rename = "type", default = "default_request_full_frame_type")]
    pub msg_type: String,
}

fn default_request_full_frame_type() -> String {
    REQUEST_FULL_FRAME.to_string()
}

impl Default for RequestFullFrameMsg {
    fn default() -> Self {
        Self {
            msg_type: default_request_full_frame_type(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MouseMoveMsg {
    #[serde(rename = "type")]
    pub msg_type: String,
    pub x: f64,
    pub y: f64,
    /// Legacy mapped desktop coordinate. New region-aware adapters use
    /// `RegionPointerMotionMsg.logical_x`.
    #[serde(default = "default_server_coordinate")]
    pub server_x: i32,
    /// Legacy mapped desktop coordinate. New region-aware adapters use
    /// `RegionPointerMotionMsg.logical_y`.
    #[serde(default = "default_server_coordinate")]
    pub server_y: i32,
    #[serde(default)]
    pub sequence: u64,
    #[serde(default)]
    pub timestamp_ns: u64,
    #[serde(default = "default_true")]
    pub coalescable: bool,
}

impl Default for MouseMoveMsg {
    fn default() -> Self {
        Self {
            msg_type: "mouse_move".to_string(),
            x: 0.0,
            y: 0.0,
            server_x: -1,
            server_y: -1,
            sequence: 0,
            timestamp_ns: 0,
            coalescable: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MouseMoveRelativeMsg {
    #[serde(rename = "type", default = "default_mouse_move_relative_type")]
    pub msg_type: String,
    #[serde(default)]
    pub dx: i32,
    #[serde(default)]
    pub dy: i32,
    #[serde(default)]
    pub sequence: u64,
    #[serde(default)]
    pub timestamp_ns: u64,
    #[serde(default = "default_true")]
    pub coalescable: bool,
}

fn default_mouse_move_relative_type() -> String {
    MOUSE_MOVE_RELATIVE.to_owned()
}

impl Default for MouseMoveRelativeMsg {
    fn default() -> Self {
        Self {
            msg_type: default_mouse_move_relative_type(),
            dx: 0,
            dy: 0,
            sequence: 0,
            timestamp_ns: 0,
            coalescable: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MouseButtonMsg {
    #[serde(rename = "type")]
    pub msg_type: String,
    pub x: f64,
    pub y: f64,
    pub button: u8,
    pub pressed: bool,
    /// Legacy mapped desktop coordinate retained for deployed products.
    #[serde(default = "default_server_coordinate")]
    pub server_x: i32,
    /// Legacy mapped desktop coordinate retained for deployed products.
    #[serde(default = "default_server_coordinate")]
    pub server_y: i32,
    #[serde(default)]
    pub sequence: u64,
    #[serde(default)]
    pub timestamp_ns: u64,
    #[serde(default)]
    pub coalescable: bool,
    #[serde(default, skip_serializing_if = "is_absolute_motion_mode")]
    pub motion_mode: PointerMotionMode,
}

impl Default for MouseButtonMsg {
    fn default() -> Self {
        Self {
            msg_type: "mouse_button".to_string(),
            x: 0.0,
            y: 0.0,
            button: 1,
            pressed: false,
            server_x: -1,
            server_y: -1,
            sequence: 0,
            timestamp_ns: 0,
            coalescable: false,
            motion_mode: PointerMotionMode::Absolute,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MouseScrollMsg {
    #[serde(rename = "type", default = "default_mouse_scroll_type")]
    pub msg_type: String,
    #[serde(default)]
    pub x: f64,
    #[serde(default)]
    pub y: f64,
    #[serde(default)]
    pub dx: f64,
    #[serde(default)]
    pub dy: f64,
    /// Legacy mapped desktop coordinate retained for deployed products.
    #[serde(default = "default_server_coordinate")]
    pub server_x: i32,
    /// Legacy mapped desktop coordinate retained for deployed products.
    #[serde(default = "default_server_coordinate")]
    pub server_y: i32,
    #[serde(default)]
    pub sequence: u64,
    #[serde(default)]
    pub timestamp_ns: u64,
    #[serde(default)]
    pub coalescable: bool,
    #[serde(default, skip_serializing_if = "is_absolute_motion_mode")]
    pub motion_mode: PointerMotionMode,
}

fn default_mouse_scroll_type() -> String {
    MOUSE_SCROLL.to_string()
}

const fn default_server_coordinate() -> i32 {
    -1
}

impl Default for MouseScrollMsg {
    fn default() -> Self {
        Self {
            msg_type: default_mouse_scroll_type(),
            x: 0.0,
            y: 0.0,
            dx: 0.0,
            dy: 0.0,
            server_x: default_server_coordinate(),
            server_y: default_server_coordinate(),
            sequence: 0,
            timestamp_ns: 0,
            coalescable: false,
            motion_mode: PointerMotionMode::Absolute,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct KeyEventMsg {
    #[serde(rename = "type")]
    pub msg_type: String,
    /// Legacy protocol-v3 name: this value is a Qt key identifier, not an
    /// evdev scan code. Deployed Python hosts either consume it directly
    /// (XTest) or translate it to the platform-native code before injection.
    pub scan_code: u32,
    pub pressed: bool,
    /// Compact wire modifier mask after destination policy is applied:
    /// Shift=0x01, Ctrl=0x02, Alt=0x04, Meta=0x08, Keypad=0x10.
    #[serde(default)]
    pub modifiers: u32,
    /// `None` means the client cannot observe this lock state. Omitting unknown
    /// values prevents a host from treating "unknown" as a false/off claim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caps_lock_on: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub num_lock_on: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scroll_lock_on: Option<bool>,
    /// Legacy mapped desktop coordinate retained for deployed products.
    #[serde(default = "default_server_coordinate")]
    pub server_x: i32,
    /// Legacy mapped desktop coordinate retained for deployed products.
    #[serde(default = "default_server_coordinate")]
    pub server_y: i32,
    #[serde(default)]
    pub sequence: u64,
    #[serde(default)]
    pub timestamp_ns: u64,
    #[serde(default)]
    pub coalescable: bool,
}

impl Default for KeyEventMsg {
    fn default() -> Self {
        Self {
            msg_type: "key_event".to_string(),
            scan_code: 0,
            pressed: false,
            modifiers: 0,
            caps_lock_on: None,
            num_lock_on: None,
            scroll_lock_on: None,
            server_x: -1,
            server_y: -1,
            sequence: 0,
            timestamp_ns: 0,
            coalescable: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KeyResetModifiersMsg {
    #[serde(rename = "type", default = "default_key_reset_modifiers_type")]
    pub msg_type: String,
    #[serde(default = "default_reset_reason")]
    pub reason: String,
}

fn default_key_reset_modifiers_type() -> String {
    KEY_RESET_MODIFIERS.to_string()
}

fn default_reset_reason() -> String {
    "unknown".to_string()
}

impl Default for KeyResetModifiersMsg {
    fn default() -> Self {
        Self {
            msg_type: default_key_reset_modifiers_type(),
            reason: default_reset_reason(),
        }
    }
}

/// Wire tool identifier for a negotiated typed-pen sample. Mirrors
/// `arcen_input::PenTool` exactly; this crate does not depend on
/// `arcen-input`, so product adapters perform the checked conversion.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PenToolMsg {
    /// Pen tip.
    Tip,
    /// Eraser end.
    Eraser,
}

/// Negotiated typed-pen sample (`PEN_EVENT` / `"pen_event"`).
///
/// Legal only once both peers have confirmed `input_protocol_version >= 3`
/// and `InputCapabilitiesMsg.pen = available` (see `WIRE.md`). This is a wire
/// DTO: it carries no interpretation of the surrounding session, and
/// products convert it with checked bounds to the canonical
/// `arcen_input::PenEvent` rather than this crate depending on `arcen-input`.
///
/// Field ranges (checked by [`PenEventMsg::validate`], not by `serde` itself,
/// so a malformed or out-of-range payload still deserializes but must be
/// rejected by `validate()` before a product advances its input sequence or
/// injects native input):
/// - `x`, `y`: normalized, inclusive `0.0..=1.0`.
/// - `pressure`: inclusive `0.0..=1.0`.
/// - `tilt_x_degrees`, `tilt_y_degrees`: inclusive `-90.0..=90.0`.
/// - `rotation_degrees`: inclusive `0.0..=360.0` (`0` and `360` both denote
///   the same physical angle; both are accepted so a sender's boundary
///   convention never fails validation).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PenEventMsg {
    #[serde(rename = "type", default = "default_pen_event_type")]
    pub msg_type: String,
    /// Normalized horizontal coordinate.
    pub x: f64,
    /// Normalized vertical coordinate.
    pub y: f64,
    /// Legacy mapped server horizontal coordinate; `-1` means unmapped.
    /// New region-aware adapters use `RegionPenEventMsg.logical_x`.
    #[serde(default = "default_server_coordinate")]
    pub server_x: i32,
    /// Legacy mapped server vertical coordinate; `-1` means unmapped.
    /// New region-aware adapters use `RegionPenEventMsg.logical_y`.
    #[serde(default = "default_server_coordinate")]
    pub server_y: i32,
    /// Normalized pressure.
    pub pressure: f32,
    /// X tilt in degrees.
    #[serde(default)]
    pub tilt_x_degrees: f32,
    /// Y tilt in degrees.
    #[serde(default)]
    pub tilt_y_degrees: f32,
    /// Barrel rotation in degrees.
    #[serde(default)]
    pub rotation_degrees: f32,
    /// Tip or eraser tool.
    pub tool: PenToolMsg,
    /// Whether the tool is in digitizer proximity.
    #[serde(default)]
    pub in_proximity: bool,
    /// Whether the tip is touching the surface.
    #[serde(default)]
    pub touching: bool,
    /// Bitset of barrel/auxiliary buttons.
    #[serde(default)]
    pub buttons: u16,
    /// Sequence participating in the one globally ordered input stream
    /// shared with keyboard, mouse, and pen events. Zero remains legacy and
    /// unsequenced.
    #[serde(default)]
    pub sequence: u64,
    #[serde(default)]
    pub timestamp_ns: u64,
    #[serde(default = "default_true")]
    pub coalescable: bool,
}

fn default_pen_event_type() -> String {
    PEN_EVENT.to_string()
}

impl Default for PenEventMsg {
    fn default() -> Self {
        Self {
            msg_type: default_pen_event_type(),
            x: 0.0,
            y: 0.0,
            server_x: default_server_coordinate(),
            server_y: default_server_coordinate(),
            pressure: 0.0,
            tilt_x_degrees: 0.0,
            tilt_y_degrees: 0.0,
            rotation_degrees: 0.0,
            tool: PenToolMsg::Tip,
            in_proximity: false,
            touching: false,
            buttons: 0,
            sequence: 0,
            timestamp_ns: 0,
            coalescable: true,
        }
    }
}

impl PenEventMsg {
    /// Validates finiteness and physical ranges before a product converts
    /// this DTO, advances its input sequence, or injects native input.
    /// Deserialization alone accepts an out-of-range or non-finite payload
    /// (there is no custom `Deserialize`); callers must call this first.
    ///
    /// # Errors
    ///
    /// Returns the first invalid field.
    pub fn validate(&self) -> Result<(), PenEventValidationError> {
        validate_unit_f64("x", self.x)?;
        validate_unit_f64("y", self.y)?;
        validate_range_f32("pressure", self.pressure, 0.0, 1.0)?;
        validate_range_f32("tilt_x_degrees", self.tilt_x_degrees, -90.0, 90.0)?;
        validate_range_f32("tilt_y_degrees", self.tilt_y_degrees, -90.0, 90.0)?;
        validate_range_f32("rotation_degrees", self.rotation_degrees, 0.0, 360.0)
    }
}

fn validate_unit_f64(field: &'static str, value: f64) -> Result<(), PenEventValidationError> {
    if value.is_finite() && (0.0..=1.0).contains(&value) {
        Ok(())
    } else {
        Err(PenEventValidationError::OutOfRange(field))
    }
}

fn validate_range_f32(
    field: &'static str,
    value: f32,
    minimum: f32,
    maximum: f32,
) -> Result<(), PenEventValidationError> {
    if value.is_finite() && (minimum..=maximum).contains(&value) {
        Ok(())
    } else {
        Err(PenEventValidationError::OutOfRange(field))
    }
}

/// Malformed, non-finite, or out-of-range `PenEventMsg` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PenEventValidationError {
    OutOfRange(&'static str),
}

impl std::fmt::Display for PenEventValidationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OutOfRange(field) => {
                write!(formatter, "pen event {field} is outside its allowed range")
            }
        }
    }
}

impl std::error::Error for PenEventValidationError {}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TextCommitMsg {
    #[serde(rename = "type", default = "default_text_commit_type")]
    pub msg_type: String,
    #[serde(default)]
    pub text: String,
}

fn default_text_commit_type() -> String {
    TEXT_COMMIT.to_string()
}

impl Default for TextCommitMsg {
    fn default() -> Self {
        Self {
            msg_type: default_text_commit_type(),
            text: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BrokerMachineRequest {
    #[serde(rename = "type")]
    pub msg_type: String,
    pub machine_name: String,
}

impl Default for BrokerMachineRequest {
    fn default() -> Self {
        Self {
            msg_type: BROKER_MACHINE_REQUEST.to_string(),
            machine_name: String::new(),
        }
    }
}

pub fn msg_type(value: &Value) -> Option<&str> {
    value.get("type").and_then(Value::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_hello_serializes_python_wire_type_field() {
        let json = serde_json::to_value(ClientHelloMsg::default()).unwrap();
        assert_eq!(json.get("type").unwrap(), CLIENT_HELLO);
        assert_eq!(json.get("capture_mode").unwrap(), "mirror_all");
        assert_eq!(json.get("clipboard_text_c2s").unwrap(), false);
        assert!(json.get("clipboard_protocol_version").is_none());
        assert!(json.get("audio_output").is_none());
        assert_eq!(
            json.get("supports_bt601_matrix"),
            Some(&serde_json::json!(false))
        );
        assert_eq!(
            json.get("supports_bt2020_ncl_matrix"),
            Some(&serde_json::json!(false))
        );
    }

    #[test]
    fn client_matrix_capabilities_are_additive_and_default_false_for_old_peers() {
        let legacy: ClientVideoCapabilitiesMsg = serde_json::from_value(serde_json::json!({
            "h264": true,
            "h265": true,
        }))
        .expect("pre-matrix capability shape remains decodable");
        assert!(!legacy.bt601_matrix);
        assert!(!legacy.bt2020_ncl_matrix);

        let legacy_hello: ClientHelloMsg =
            serde_json::from_value(serde_json::to_value(ClientHelloMsg::default()).unwrap())
                .expect("default client hello remains decodable");
        assert!(!legacy_hello.supports_bt601_matrix);
        assert!(!legacy_hello.supports_bt2020_ncl_matrix);
    }

    /// The field that asks for HDR. Depth alone must not imply it: 10-bit
    /// BT.709 SDR is an ordinary request for banding headroom, and a host that
    /// treated it as HDR would apply an EDID and tone map for no reason.
    #[test]
    fn hdr_is_requested_by_transfer_not_by_depth() {
        let sdr_ten_bit = QualitySettings {
            bit_depth: "10".to_string(),
            ..QualitySettings::default()
        };
        assert_eq!(sdr_ten_bit.transfer, "bt709");
        assert_eq!(sdr_ten_bit.color_primaries, "bt709");

        let hdr10 = QualitySettings {
            bit_depth: "10".to_string(),
            transfer: "pq".to_string(),
            color_primaries: "bt2020".to_string(),
            color_matrix: "bt2020ncl".to_string(),
            ..QualitySettings::default()
        };
        let wire = serde_json::to_value(&hdr10).expect("serialize");
        assert_eq!(wire.get("transfer").and_then(|v| v.as_str()), Some("pq"));
        assert_eq!(
            wire.get("color_primaries").and_then(|v| v.as_str()),
            Some("bt2020")
        );
        let decoded: QualitySettings = serde_json::from_value(wire).expect("round trip");
        assert_eq!(decoded, hdr10);
    }

    /// A client that predates the fields must still decode, and must read as
    /// SDR rather than as an accidental HDR request.
    #[test]
    fn a_client_without_the_hdr_fields_reads_as_sdr() {
        let mut wire = serde_json::to_value(QualitySettings::default()).expect("serialize");
        let object = wire.as_object_mut().expect("object");
        object.remove("transfer");
        object.remove("color_primaries");

        let decoded: QualitySettings = serde_json::from_value(wire).expect("legacy decode");
        assert_eq!(decoded.transfer, "bt709");
        assert_eq!(decoded.color_primaries, "bt709");
    }

    #[test]
    fn auth_time_video_intent_is_additive_and_legacy_exact() {
        let legacy_quality = serde_json::to_value(QualitySettings::default()).unwrap();
        assert!(
            legacy_quality.get("video_selection").is_none(),
            "the exact compatibility default stays absent on the wire"
        );
        let decoded: QualitySettings = serde_json::from_value(legacy_quality).unwrap();
        assert_eq!(decoded.video_selection, VideoSelectionIntent::Exact);

        let mut response = AuthResponse::pam("artist", "secret");
        assert!(
            serde_json::to_value(&response)
                .unwrap()
                .get("initial_video")
                .is_none(),
            "old-client shape stays unchanged until the client opts in"
        );
        response.initial_video = Some(InitialVideoRequestMsg {
            quality: QualitySettings {
                video_selection: VideoSelectionIntent::AdaptivePerformance,
                codec: "h264".to_string(),
                chroma: "yuv420".to_string(),
                ..QualitySettings::default()
            },
            capabilities: ClientVideoCapabilitiesMsg {
                h264: true,
                h265: true,
                av1: true,
                ..ClientVideoCapabilitiesMsg::default()
            },
        });
        let json = serde_json::to_value(&response).unwrap();
        assert_eq!(
            json["initial_video"]["quality"]["video_selection"],
            "adaptive_performance"
        );
        let round_trip: AuthResponse = serde_json::from_value(json).unwrap();
        assert_eq!(
            round_trip
                .initial_video
                .expect("initial video request")
                .quality
                .video_selection,
            VideoSelectionIntent::AdaptivePerformance
        );

        let hello = ClientHelloMsg {
            supports_h264: true,
            supports_h265: true,
            supports_av1: false,
            supports_yuv444: true,
            supports_main10: true,
            supports_full_range: true,
            ..ClientHelloMsg::default()
        };
        assert_eq!(
            ClientVideoCapabilitiesMsg::from_client_hello(&hello),
            ClientVideoCapabilitiesMsg {
                h264: true,
                h265: true,
                av1: false,
                yuv444: true,
                main10: true,
                main12: false,
                full_range: true,
                identity_matrix: false,
                bt601_matrix: false,
                bt2020_ncl_matrix: false,
            }
        );
    }

    #[test]
    fn build_identity_is_typed_and_legacy_optional_on_both_hellos() {
        assert_eq!(ClientHelloMsg::default().build_identity().unwrap(), None);
        let legacy_server: ServerHelloMsg =
            serde_json::from_value(serde_json::json!({"type": SERVER_HELLO})).unwrap();
        assert_eq!(legacy_server.build_identity().unwrap(), None);

        let identity = BuildIdentityMsg {
            product: "arcen-deck-macos".to_string(),
            version: "0.1.0".to_string(),
            build_id: "ci-42".to_string(),
            source_revision: "abcdef1".to_string(),
            build_profile: "release".to_string(),
            feature_profile: "quic".to_string(),
            artifact_sha256: Some("12".repeat(32)),
            signing_state: Some("notarized".to_string()),
        };
        let client = ClientHelloMsg::default().with_build_identity(identity.clone());
        let server = legacy_server.with_build_identity(identity.clone());
        assert_eq!(client.build_identity().unwrap(), Some(identity.clone()));
        assert_eq!(server.build_identity().unwrap(), Some(identity));
        assert!(serde_json::to_value(client).unwrap()["device_capabilities"]
            .get(BUILD_IDENTITY_CAPABILITY)
            .is_some());
    }

    #[test]
    fn audio_v1_is_explicit_typed_and_legacy_compatible() {
        let legacy: ClientHelloMsg =
            serde_json::from_value(serde_json::to_value(ClientHelloMsg::default()).unwrap())
                .unwrap();
        assert_eq!(legacy.audio_output, None);

        let capabilities = AudioOutputCapabilitiesMsg {
            protocol_version: AUDIO_PROTOCOL_VERSION,
            codecs: vec![AudioCodec::Opus, AudioCodec::Pcm],
            sample_rate_hz: 48_000,
            channels: 2,
            frame_duration_ms: 20,
            fec: false,
            dtx: false,
        };
        assert!(capabilities.is_valid_v1());
        let hello = ClientHelloMsg {
            audio_output: Some(capabilities.clone()),
            ..ClientHelloMsg::default()
        };
        assert_eq!(
            serde_json::from_value::<ClientHelloMsg>(serde_json::to_value(&hello).unwrap())
                .unwrap(),
            hello
        );

        let config = AudioStreamConfigMsg {
            protocol_version: AUDIO_PROTOCOL_VERSION,
            codec: AudioCodec::Opus,
            sample_rate_hz: 48_000,
            channels: 2,
            frame_duration_ms: 20,
            bitrate: AudioBitrateTierMsg::Kbps128,
            fec: false,
            dtx: false,
        };
        assert!(config.is_valid_v1());
        let result = AudioStreamResultMsg::enabled(config, AudioStreamReason::Enabled);
        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["type"], AUDIO_STREAM_RESULT);
        assert_eq!(
            serde_json::from_value::<AudioStreamResultMsg>(json).unwrap(),
            result
        );
    }

    #[test]
    fn microphone_v1_defaults_absent_and_requires_exact_mono_shape() {
        let legacy_client: ClientHelloMsg =
            serde_json::from_str(r#"{"type":"client_hello","client_name":"old","version":"3","screen_width":1,"screen_height":1,"supports_h264":false,"supports_h265":false,"supports_av1":false,"supports_yuv444":false,"supports_audio":false,"supports_pen":false,"decoder_backend":"","capture_mode":"","picked_monitor_id":-1,"picked_monitor_name":"","device_capabilities":{}}"#)
                .unwrap();
        assert_eq!(legacy_client.microphone_output, None);

        let capabilities = MicrophoneCapabilitiesMsg {
            protocol_version: MICROPHONE_PROTOCOL_VERSION,
            codecs: vec![AudioCodec::Opus, AudioCodec::Pcm],
            sample_rate_hz: 48_000,
            channels: 1,
            frame_duration_ms: 20,
            fec: false,
            dtx: false,
        };
        assert!(capabilities.is_valid_v1());
        let mut invalid = capabilities.clone();
        invalid.channels = 2;
        assert!(!invalid.is_valid_v1());

        let config = MicrophoneStreamConfigMsg {
            protocol_version: MICROPHONE_PROTOCOL_VERSION,
            codec: AudioCodec::Opus,
            sample_rate_hz: 48_000,
            channels: 1,
            frame_duration_ms: 20,
            bitrate: AudioBitrateTierMsg::Kbps64,
            pcm_bitrate_kbps: None,
            generation: 9,
            fec: false,
            dtx: false,
        };
        let result = MicrophoneStreamResultMsg::enabled(config, MicrophoneStreamReason::Enabled);
        assert!(result.config.unwrap().is_valid_v1());
        assert!(MicrophoneStreamConfigMsg {
            codec: AudioCodec::Pcm,
            bitrate: AudioBitrateTierMsg::Off,
            pcm_bitrate_kbps: Some(768),
            ..config
        }
        .is_valid_v1());
        assert!(!MicrophoneStreamConfigMsg {
            codec: AudioCodec::Pcm,
            bitrate: AudioBitrateTierMsg::Kbps510,
            pcm_bitrate_kbps: Some(768),
            ..config
        }
        .is_valid_v1());
        assert_eq!(
            serde_json::from_value::<MicrophoneStreamResultMsg>(
                serde_json::to_value(&result).unwrap()
            )
            .unwrap(),
            result
        );
        let stop = MicrophoneStreamStopMsg::new(9, MicrophoneStreamReason::PermissionDenied);
        assert!(stop.is_valid());
        assert_eq!(
            serde_json::from_value::<MicrophoneStreamStopMsg>(serde_json::to_value(&stop).unwrap())
                .unwrap(),
            stop
        );
        assert!(
            !MicrophoneStreamStopMsg::new(0, MicrophoneStreamReason::CaptureFailure).is_valid()
        );
        assert!(!MicrophoneStreamStopMsg::new(9, MicrophoneStreamReason::Enabled).is_valid());
        let mut wrong_version =
            MicrophoneStreamStopMsg::new(9, MicrophoneStreamReason::CaptureFailure);
        wrong_version.protocol_version += 1;
        assert!(!wrong_version.is_valid());
    }

    #[test]
    fn audio_v1_rejects_unbounded_or_non_fixed_capabilities() {
        let too_many = AudioOutputCapabilitiesMsg {
            protocol_version: AUDIO_PROTOCOL_VERSION,
            codecs: vec![AudioCodec::Pcm; MAX_AUDIO_CODECS + 1],
            sample_rate_hz: 48_000,
            channels: 2,
            frame_duration_ms: 20,
            fec: false,
            dtx: false,
        };
        assert!(!too_many.is_valid_v1());
        let too_many_json = serde_json::to_value(&too_many).unwrap();
        assert!(serde_json::from_value::<AudioOutputCapabilitiesMsg>(too_many_json).is_err());
        assert!(!AudioOutputCapabilitiesMsg {
            sample_rate_hz: 44_100,
            ..too_many
        }
        .is_valid_v1());
    }

    #[test]
    fn clipboard_v1_is_explicit_and_legacy_booleans_cannot_enable_it() {
        let legacy: ClientHelloMsg = serde_json::from_value(serde_json::json!({
            "type": "client_hello",
            "client_name": "legacy",
            "version": "3",
            "screen_width": 1,
            "screen_height": 1,
            "supports_h264": true,
            "supports_h265": false,
            "supports_av1": false,
            "supports_yuv444": false,
            "supports_audio": false,
            "supports_pen": false,
            "decoder_backend": "",
            "capture_mode": "mirror_all",
            "picked_monitor_id": -1,
            "picked_monitor_name": "",
            "clipboard_text_c2s": true,
            "clipboard_text_s2c": true,
            "clipboard_image_c2s": true,
            "clipboard_image_s2c": true,
            "device_capabilities": {}
        }))
        .unwrap();
        assert_eq!(legacy.clipboard_protocol_version, 0);
        assert!(legacy.clipboard_text_c2s);

        let policy = ClipboardPolicyMsg {
            protocol_version: CLIPBOARD_PROTOCOL_VERSION,
            direction: ClipboardDirectionMsg::Both,
            content: ClipboardContentMsg::All,
            max_bytes: 8 * 1024 * 1024,
        };
        assert!(policy.is_v1());
        let offer = ClipboardDataMsg::new(7, ClipboardContentKind::TextUtf8, 3, true);
        let json = serde_json::to_value(&offer).unwrap();
        assert_eq!(json["type"], CLIPBOARD_DATA);
        assert_eq!(
            serde_json::from_value::<ClipboardDataMsg>(json).unwrap(),
            offer
        );
    }

    #[test]
    fn legacy_auth_response_without_session_log_id_still_parses() {
        let response: AuthResponse = serde_json::from_str(
            r#"{"type":"auth_response","method":"pam","username":"artist","credential":"secret","screen_width":1920,"screen_height":1080}"#,
        )
        .unwrap();
        assert_eq!(response.session_log_id, None);
        assert_eq!(response.disclaimer_acceptance_sha256, None);
        assert_eq!(response.timezone, None);
        assert_eq!(response.multi_monitor_v1, None);
        assert!(!response.resume_requested);
        assert_eq!(response.resume_holder_nonce, None);
        assert_eq!(response.resume_grant, None);
        assert_eq!(response.cursor_preference, CursorMode::Local);
    }

    #[test]
    fn legacy_auth_json_is_unchanged_when_resume_is_off() {
        let legacy_response = r#"{"type":"auth_response","method":"pam","username":"artist","credential":"secret","screen_width":0,"screen_height":0,"monitors":[],"displays_mode":""}"#;
        let response = AuthResponse::pam("artist", "secret");
        assert_eq!(serde_json::to_string(&response).unwrap(), legacy_response);

        let legacy_result = r#"{"type":"auth_result","success":true,"message":"ok"}"#;
        let result: AuthResult = serde_json::from_str(legacy_result).unwrap();
        assert_eq!(result.resume_grant, None);
        assert_eq!(result.resume_window_secs, None);
        assert!(!result.resumed);
        assert_eq!(result.error_code, None);
        assert_eq!(serde_json::to_string(&result).unwrap(), legacy_result);
    }

    #[test]
    fn cursor_preference_defaults_local_and_matches_auth_and_hello() {
        let auth: AuthResponse = serde_json::from_str(
            r#"{"type":"auth_response","method":"none","username":"","credential":"","screen_width":0,"screen_height":0}"#,
        )
        .unwrap();
        let hello: ClientHelloMsg =
            serde_json::from_value(serde_json::to_value(ClientHelloMsg::default()).unwrap())
                .unwrap();
        assert_eq!(auth.cursor_preference, CursorMode::Local);
        assert_eq!(hello.cursor_preference, CursorMode::Local);

        let auth = AuthResponse::none().with_cursor_preference(CursorMode::Host);
        let hello = ClientHelloMsg::default().with_cursor_preference(CursorMode::Host);
        assert_eq!(auth.cursor_preference, hello.cursor_preference);
        assert_eq!(
            serde_json::to_value(auth).unwrap()["cursor_preference"],
            "host"
        );
    }

    #[test]
    fn resume_advertisement_opt_in_and_attempt_round_trip() {
        let request: AuthRequest = serde_json::from_str(
            r#"{"type":"auth_request","auth_methods":["pam"],"challenge":"","salt":"","auth_mode":null}"#,
        )
        .unwrap();
        assert!(!request.supports_resume());
        let request = request.with_resume_support(true);
        assert!(request.supports_resume());
        assert_eq!(
            request
                .auth_methods
                .iter()
                .filter(|method| method.as_str() == AUTH_METHOD_RESUME)
                .count(),
            1
        );
        assert!(!request.with_resume_support(false).supports_resume());

        let opt_in = AuthResponse::pam("artist", "secret").with_resume_requested("holder-nonce");
        let opt_in: AuthResponse =
            serde_json::from_value(serde_json::to_value(opt_in).unwrap()).unwrap();
        assert!(opt_in.resume_requested);
        assert_eq!(opt_in.resume_holder_nonce.as_deref(), Some("holder-nonce"));
        assert_eq!(opt_in.resume_grant, None);

        let attempt = AuthResponse::resume("holder-nonce", "opaque-sensitive-grant");
        assert_eq!(attempt.method, AUTH_METHOD_RESUME);
        assert!(attempt.username.is_empty());
        assert!(attempt.credential.is_empty());
        assert!(!attempt.resume_requested);
        assert_eq!(
            serde_json::from_value::<AuthResponse>(serde_json::to_value(&attempt).unwrap())
                .unwrap(),
            attempt
        );
    }

    #[test]
    fn resume_result_round_trips_and_debug_redacts_grants() {
        let result = AuthResult {
            msg_type: AUTH_RESULT.to_owned(),
            success: true,
            message: "resumed".to_owned(),
            resume_grant: Some("opaque-sensitive-grant".to_owned()),
            resume_window_secs: Some(1_200),
            resumed: true,
            error_code: None,
        };
        assert_eq!(
            serde_json::from_value::<AuthResult>(serde_json::to_value(&result).unwrap()).unwrap(),
            result
        );
        let debug = format!("{result:?}");
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("opaque-sensitive-grant"));

        let attempt = AuthResponse::resume("holder-sensitive", "grant-sensitive");
        let debug = format!("{attempt:?}");
        assert!(!debug.contains("holder-sensitive"));
        assert!(!debug.contains("grant-sensitive"));
        assert!(debug.contains("<redacted>"));
    }

    #[test]
    fn resume_error_codes_have_exact_stable_wire_values() {
        for (code, wire) in [
            (ResumeErrorCode::Unsupported, "\"unsupported\""),
            (ResumeErrorCode::Expired, "\"expired\""),
            (ResumeErrorCode::Replayed, "\"replayed\""),
            (
                ResumeErrorCode::NativeIdentityChanged,
                "\"native_identity_changed\"",
            ),
            (ResumeErrorCode::TopologyChanged, "\"topology_changed\""),
            (ResumeErrorCode::SessionGone, "\"session_gone\""),
            (ResumeErrorCode::InternalFailure, "\"internal_failure\""),
        ] {
            assert_eq!(serde_json::to_string(&code).unwrap(), wire);
            assert_eq!(serde_json::from_str::<ResumeErrorCode>(wire).unwrap(), code);
        }
    }

    #[test]
    fn legacy_consumers_ignore_all_resume_additions() {
        #[derive(Deserialize)]
        struct LegacyAuthResult {
            #[serde(rename = "type")]
            msg_type: String,
            success: bool,
        }

        let legacy: LegacyAuthResult = serde_json::from_str(
            r#"{"type":"auth_result","success":false,"resume_grant":"opaque","resume_window_secs":1200,"resumed":false,"error_code":"expired"}"#,
        )
        .unwrap();
        assert_eq!(legacy.msg_type, AUTH_RESULT);
        assert!(!legacy.success);
    }

    #[test]
    fn disclaimer_auth_fields_are_additive_and_feature_off_json_is_unchanged() {
        let legacy_request = r#"{"type":"auth_request","auth_methods":["pam"],"challenge":"c","salt":"s","auth_mode":"pam"}"#;
        let request: AuthRequest = serde_json::from_str(legacy_request).unwrap();
        assert_eq!(request.disclaimer, None);
        assert_eq!(request.multi_monitor_v1_offer(), None);
        assert_eq!(serde_json::to_string(&request).unwrap(), legacy_request);

        let legacy_response = AuthResponse::pam("artist", "secret");
        let json = serde_json::to_value(&legacy_response).unwrap();
        assert!(!json
            .as_object()
            .unwrap()
            .contains_key("disclaimer_acceptance_sha256"));

        let request = AuthRequest {
            disclaimer: Some("Exact text\n".to_owned()),
            ..request
        };
        let request_json = serde_json::to_value(&request).unwrap();
        assert_eq!(request_json["disclaimer"], "Exact text\n");

        let mut response = legacy_response;
        response.disclaimer_acceptance_sha256 = Some("ab".repeat(32));
        let response: AuthResponse =
            serde_json::from_value(serde_json::to_value(response).unwrap()).unwrap();
        assert_eq!(
            response.disclaimer_acceptance_sha256.as_deref(),
            Some(&*"ab".repeat(32))
        );
    }

    #[test]
    fn legacy_request_consumer_ignores_disclaimer_field() {
        #[derive(Deserialize)]
        struct LegacyAuthRequest {
            #[serde(rename = "type")]
            msg_type: String,
        }

        let legacy: LegacyAuthRequest =
            serde_json::from_str(r#"{"type":"auth_request","disclaimer":"Exact text"}"#).unwrap();
        assert_eq!(legacy.msg_type, AUTH_REQUEST);
    }

    #[test]
    fn auth_request_multi_monitor_v1_offer_is_additive_and_legacy_shape_still_parses() {
        let legacy_request = r#"{"type":"auth_request","auth_methods":["pam"],"challenge":"c","salt":"s","auth_mode":"pam"}"#;
        let request: AuthRequest = serde_json::from_str(legacy_request).unwrap();
        assert_eq!(request.multi_monitor_v1_offer(), None);
        assert_eq!(
            request.required_multi_monitor_v1_offer(),
            Err(AuthRequestMultiMonitorOfferError::Missing)
        );
        assert_eq!(serde_json::to_string(&request).unwrap(), legacy_request);

        let offer = AuthMultiMonitorOfferMsg::new(
            4,
            vec![
                RotationMsg::Degrees0,
                RotationMsg::Degrees90,
                RotationMsg::Degrees180,
                RotationMsg::Degrees270,
            ],
            vec![
                MultiMonitorCarrierMsg::MuxedReliableStream,
                MultiMonitorCarrierMsg::PerMonitorReliableStream,
            ],
        )
        .expect("offer");
        let offered = serde_json::from_str::<AuthRequest>(legacy_request)
            .unwrap()
            .with_multi_monitor_v1_offer(offer.clone())
            .expect("offer");
        let json = serde_json::to_value(&offered).unwrap();
        assert_eq!(json["multi_monitor_v1"]["max_monitors"], 4);
        assert_eq!(
            json["multi_monitor_v1"]["supported_rotations"][1],
            "degrees90"
        );
        assert_eq!(
            json["multi_monitor_v1"]["carriers"][1],
            "per_monitor_reliable_stream"
        );

        let round_trip: AuthRequest = serde_json::from_value(json).unwrap();
        assert_eq!(round_trip.multi_monitor_v1_offer(), Some(&offer));
        assert_eq!(
            round_trip
                .required_multi_monitor_v1_offer()
                .expect("validated offer")
                .max_monitors(),
            4
        );
    }

    #[test]
    fn session_log_id_is_additive_on_auth_and_client_hello() {
        let sid = "01234567-89ab-4def-8123-456789abcdef".to_string();
        let mut auth = AuthResponse::pam("artist", "secret");
        auth.session_log_id = Some(sid.clone());
        let auth: AuthResponse =
            serde_json::from_value(serde_json::to_value(auth).unwrap()).unwrap();
        assert_eq!(auth.session_log_id.as_deref(), Some(sid.as_str()));

        let hello = ClientHelloMsg {
            session_log_id: Some(sid.clone()),
            ..ClientHelloMsg::default()
        };
        let hello: ClientHelloMsg =
            serde_json::from_value(serde_json::to_value(hello).unwrap()).unwrap();
        assert_eq!(hello.session_log_id.as_deref(), Some(sid.as_str()));
    }

    #[test]
    fn timezone_is_additive_on_auth_and_client_hello() {
        let auth = AuthResponse::pam("artist", "secret").with_timezone("Europe/Oslo");
        let auth: AuthResponse =
            serde_json::from_value(serde_json::to_value(auth).unwrap()).unwrap();
        assert_eq!(auth.timezone.as_deref(), Some("Europe/Oslo"));

        let hello = ClientHelloMsg::default().with_timezone("Europe/Oslo");
        let hello: ClientHelloMsg =
            serde_json::from_value(serde_json::to_value(hello).unwrap()).unwrap();
        assert_eq!(hello.timezone.as_deref(), Some("Europe/Oslo"));
    }

    #[test]
    fn absent_timezone_is_omitted_and_old_client_hello_parses() {
        let auth_json = serde_json::to_value(AuthResponse::none()).unwrap();
        assert!(!auth_json.as_object().unwrap().contains_key("timezone"));

        let hello_json = serde_json::to_value(ClientHelloMsg::default()).unwrap();
        assert!(!hello_json.as_object().unwrap().contains_key("timezone"));
        let hello: ClientHelloMsg = serde_json::from_value(hello_json).unwrap();
        assert_eq!(hello.timezone, None);
    }

    #[test]
    fn legacy_consumers_ignore_timezone_fields() {
        #[derive(Deserialize)]
        struct LegacyMessage {
            #[serde(rename = "type")]
            msg_type: String,
        }

        let auth: LegacyMessage =
            serde_json::from_str(r#"{"type":"auth_response","timezone":"Europe/Oslo"}"#).unwrap();
        assert_eq!(auth.msg_type, AUTH_RESPONSE);

        let hello: LegacyMessage =
            serde_json::from_str(r#"{"type":"client_hello","timezone":"Europe/Oslo"}"#).unwrap();
        assert_eq!(hello.msg_type, CLIENT_HELLO);
    }

    #[test]
    fn no_auth_metadata_response_has_no_credential() {
        let response = AuthResponse::none();
        assert_eq!(response.method, "none");
        assert!(response.username.is_empty());
        assert!(response.credential.is_empty());
    }

    #[test]
    fn parses_server_hello_with_python_defaults() {
        let hello: ServerHelloMsg = serde_json::from_str(
            r#"{"type":"server_hello","server_name":"Linux Host","codec":"h264"}"#,
        )
        .unwrap();
        assert_eq!(hello.msg_type, SERVER_HELLO);
        assert_eq!(hello.screen_width, 1920);
        assert!(hello.supports_h264);
        assert!(hello.os_user.is_empty());
        assert!(hello.session_id.is_empty());
        assert!(hello.session_type.is_empty());
        assert!(hello.desktop.is_empty());
        assert_eq!(hello.input_protocol_version, 0);
        assert_eq!(hello.input_capabilities, InputCapabilitiesMsg::default());
    }

    #[test]
    fn server_hello_session_identity_is_additive_v3_wire_data() {
        let hello: ServerHelloMsg = serde_json::from_str(
            r#"{
                "type":"server_hello",
                "os_user":"artist",
                "session_id":"c17",
                "session_type":"x11",
                "desktop":"gnome-classic"
            }"#,
        )
        .unwrap();
        let json = serde_json::to_value(&hello).unwrap();
        assert_eq!(json["os_user"], "artist");
        assert_eq!(json["session_id"], "c17");
        assert_eq!(json["session_type"], "x11");
        assert_eq!(json["desktop"], "gnome-classic");
    }

    #[test]
    fn auth_response_uses_type_key() {
        let msg = AuthResponse::pam("artist", "secret");
        let json = serde_json::to_value(msg).unwrap();
        assert_eq!(json.get("type").unwrap(), AUTH_RESPONSE);
        assert_eq!(json.get("method").unwrap(), "pam");
    }

    #[test]
    fn auth_response_debug_redacts_credential() {
        let msg = AuthResponse::pam("artist", "dummy-password").with_timezone("Europe/Oslo");
        let debug = format!("{msg:?}");
        assert!(debug.contains("<redacted>"));
        assert!(debug.contains("Europe/Oslo"));
        assert!(!debug.contains("dummy-password"));
    }

    #[test]
    fn client_hello_and_auth_response_report_the_same_primary_size() {
        // Regression: the host re-reads screen size from BOTH the auth response
        // and the (later) client_hello. Before the fix, client_hello sent a
        // hardcoded 1920x1080 that clobbered the honest auth-response value on
        // the host. Both builders must agree on the real primary size.
        let monitors = vec![
            ClientMonitor {
                id: 1,
                x: 0,
                y: 0,
                width_px: 3600,
                height_px: 2338,
                scale: 2.0,
                refresh_hz: 120,
                is_primary: true,
                name: "Built-in Display".to_string(),
                width_mm: 301.8,
                height_mm: 196.0,
                vendor: 0x610,
                model: 0xa05e,
                serial: 0xfd626d62,
                edid: String::new(),
            },
            ClientMonitor {
                id: 2,
                x: 3600,
                y: 0,
                width_px: 2560,
                height_px: 1440,
                scale: 1.0,
                refresh_hz: 60,
                is_primary: false,
                name: "Display 2".to_string(),
                ..Default::default()
            },
        ];
        let hello = ClientHelloMsg::default().with_display_size(&monitors);
        assert_eq!(hello.screen_width, 3600);
        assert_eq!(hello.screen_height, 2338);

        let auth = AuthResponse::password("jc", "x").with_displays(monitors, "match_layout");
        assert_eq!(hello.screen_width, auth.screen_width);
        assert_eq!(hello.screen_height, auth.screen_height);
    }

    #[test]
    fn client_hello_display_size_keeps_default_when_no_monitors() {
        let hello = ClientHelloMsg::default().with_display_size(&[]);
        assert_eq!(hello.screen_width, 1920);
        assert_eq!(hello.screen_height, 1080);
    }

    #[test]
    fn parses_health_stats_with_input_counters() {
        let stats: HealthStatsMsg = serde_json::from_str(
            r#"{
                "type":"health_stats",
                "fps_actual":59.8,
                "input_events":12,
                "last_input_sequence":44,
                "last_input_type":"mouse_move"
            }"#,
        )
        .unwrap();
        assert_eq!(stats.msg_type, HEALTH_STATS);
        assert_eq!(stats.fps_actual, 59.8);
        assert_eq!(stats.input_events, 12);
        assert_eq!(stats.last_input_sequence, 44);
        assert_eq!(stats.last_input_type, "mouse_move");
        assert_eq!(stats.codec, "h264");
        assert_eq!(stats.chroma, "yuv444");
    }

    #[test]
    fn key_event_parses_legacy_boolean_lock_payload() {
        let msg: KeyEventMsg = serde_json::from_str(
            r#"{
                "type":"key_event",
                "scan_code":65,
                "pressed":true,
                "caps_lock_on":false,
                "num_lock_on":true,
                "scroll_lock_on":false,
                "server_x":-1,
                "server_y":-1,
                "sequence":7,
                "timestamp_ns":9,
                "coalescable":false
            }"#,
        )
        .unwrap();
        assert_eq!(msg.scan_code, 65);
        assert_eq!(msg.modifiers, 0);
        assert_eq!(msg.caps_lock_on, Some(false));
        assert_eq!(msg.num_lock_on, Some(true));
        assert_eq!(msg.scroll_lock_on, Some(false));
    }

    #[test]
    fn key_event_missing_lock_state_is_unknown_and_omitted() {
        let msg: KeyEventMsg = serde_json::from_str(
            r#"{
                "type":"key_event",
                "scan_code":65,
                "pressed":true,
                "server_x":-1,
                "server_y":-1,
                "sequence":7,
                "timestamp_ns":9,
                "coalescable":false
            }"#,
        )
        .unwrap();
        assert_eq!(msg.modifiers, 0);
        assert_eq!(msg.caps_lock_on, None);
        assert_eq!(msg.num_lock_on, None);
        assert_eq!(msg.scroll_lock_on, None);

        let json = serde_json::to_value(msg).unwrap();
        assert!(!json.as_object().unwrap().contains_key("caps_lock_on"));
        assert!(!json.as_object().unwrap().contains_key("num_lock_on"));
        assert!(!json.as_object().unwrap().contains_key("scroll_lock_on"));
    }

    #[test]
    fn mouse_move_parses_legacy_json_with_python_defaults() {
        let msg: MouseMoveMsg =
            serde_json::from_str(r#"{"type":"mouse_move","x":0.25,"y":0.75}"#).unwrap();
        assert_eq!(
            msg,
            MouseMoveMsg {
                x: 0.25,
                y: 0.75,
                ..MouseMoveMsg::default()
            }
        );
        assert_eq!(msg.server_x, -1);
        assert_eq!(msg.server_y, -1);
        assert_eq!(msg.sequence, 0);
        assert_eq!(msg.timestamp_ns, 0);
        assert!(msg.coalescable);
    }

    #[test]
    fn mouse_button_parses_legacy_json_with_python_defaults() {
        let msg: MouseButtonMsg = serde_json::from_str(
            r#"{"type":"mouse_button","x":0.25,"y":0.75,"button":3,"pressed":true}"#,
        )
        .unwrap();
        assert_eq!(
            msg,
            MouseButtonMsg {
                x: 0.25,
                y: 0.75,
                button: 3,
                pressed: true,
                ..MouseButtonMsg::default()
            }
        );
        assert_eq!(msg.server_x, -1);
        assert_eq!(msg.server_y, -1);
        assert_eq!(msg.sequence, 0);
        assert_eq!(msg.timestamp_ns, 0);
        assert!(!msg.coalescable);
        assert_eq!(msg.motion_mode, PointerMotionMode::Absolute);
    }

    #[test]
    fn relative_motion_has_stable_input_v2_json() {
        let msg = MouseMoveRelativeMsg {
            dx: -17,
            dy: 9,
            sequence: 22,
            timestamp_ns: 44,
            ..MouseMoveRelativeMsg::default()
        };
        assert_eq!(
            serde_json::to_string(&msg).unwrap(),
            r#"{"type":"mouse_move_relative","dx":-17,"dy":9,"sequence":22,"timestamp_ns":44,"coalescable":true}"#
        );
        assert_eq!(
            serde_json::from_str::<MouseMoveRelativeMsg>(
                r#"{"type":"mouse_move_relative","dx":1,"unknown":true}"#
            )
            .unwrap(),
            MouseMoveRelativeMsg {
                dx: 1,
                ..MouseMoveRelativeMsg::default()
            }
        );
        assert_eq!(INPUT_PROTOCOL_VERSION, 4);
        // Bumped to 4 when the video header gained bit depth, colour range and
        // matrix coefficients in its flags byte.
        assert_eq!(crate::wire::PROTOCOL_VERSION, 4);
    }

    #[test]
    fn relative_edge_and_scroll_emit_motion_mode_while_absolute_stays_legacy() {
        let absolute = serde_json::to_value(MouseButtonMsg::default()).unwrap();
        assert!(!absolute.as_object().unwrap().contains_key("motion_mode"));

        let relative = MouseScrollMsg {
            motion_mode: PointerMotionMode::Relative,
            ..MouseScrollMsg::default()
        };
        assert_eq!(
            serde_json::to_value(relative).unwrap()["motion_mode"],
            "relative"
        );
    }

    #[test]
    fn typed_input_capabilities_default_unknown_and_round_trip() {
        let old: InputCapabilitiesMsg = serde_json::from_str("{}").unwrap();
        assert_eq!(old.absolute_pointer, InputCapabilityAvailability::Unknown);
        assert_eq!(old.relative_pointer, InputCapabilityAvailability::Unknown);
        assert_eq!(old.host_cursor, InputCapabilityAvailability::Unknown);
        assert_eq!(old.region_input, InputCapabilityAvailability::Unknown);
        assert_eq!(old.pen, InputCapabilityAvailability::Unknown);
        assert_eq!(old.pen_pressure, InputCapabilityAvailability::Unknown);
        assert_eq!(old.pen_tilt, InputCapabilityAvailability::Unknown);
        assert_eq!(old.pen_rotation, InputCapabilityAvailability::Unknown);
        assert_eq!(old.pen_eraser, InputCapabilityAvailability::Unknown);
        assert_eq!(old.pen_proximity, InputCapabilityAvailability::Unknown);

        let hello = ClientHelloMsg {
            input_capabilities: InputCapabilitiesMsg {
                absolute_pointer: InputCapabilityAvailability::Available,
                relative_pointer: InputCapabilityAvailability::Available,
                host_cursor: InputCapabilityAvailability::Unavailable,
                ..InputCapabilitiesMsg::default()
            },
            ..ClientHelloMsg::default()
        };
        assert_eq!(
            serde_json::from_value::<ClientHelloMsg>(serde_json::to_value(&hello).unwrap())
                .unwrap(),
            hello
        );
    }

    #[test]
    fn region_input_requires_v4_and_explicit_availability() {
        let available = InputCapabilitiesMsg {
            region_input: InputCapabilityAvailability::Available,
            ..InputCapabilitiesMsg::default()
        };
        assert!(!supports_region_input_v1(
            REGION_INPUT_PROTOCOL_VERSION - 1,
            available
        ));
        assert!(supports_region_input_v1(
            REGION_INPUT_PROTOCOL_VERSION,
            available
        ));
        assert!(!supports_region_input_v1(
            REGION_INPUT_PROTOCOL_VERSION,
            InputCapabilitiesMsg::default()
        ));
    }

    /// Input-v3 pen capability truth mirrors the same additive-default and
    /// round-trip contract as the pre-existing pointer/cursor capabilities.
    #[test]
    fn typed_pen_capabilities_default_unknown_and_round_trip() {
        let capabilities = InputCapabilitiesMsg {
            pen: InputCapabilityAvailability::Available,
            pen_pressure: InputCapabilityAvailability::Available,
            pen_tilt: InputCapabilityAvailability::Available,
            pen_rotation: InputCapabilityAvailability::Unavailable,
            pen_eraser: InputCapabilityAvailability::Available,
            pen_proximity: InputCapabilityAvailability::Available,
            ..InputCapabilitiesMsg::default()
        };
        let json = serde_json::to_value(capabilities).unwrap();
        assert_eq!(json["pen"], "available");
        assert_eq!(json["pen_rotation"], "unavailable");
        assert_eq!(
            serde_json::from_value::<InputCapabilitiesMsg>(json).unwrap(),
            capabilities
        );

        // A server_hello that predates input v3 carries none of these keys;
        // they must default to Unknown, never Available.
        let legacy: InputCapabilitiesMsg =
            serde_json::from_str(r#"{"absolute_pointer":"available","host_cursor":"available"}"#)
                .unwrap();
        assert_eq!(legacy.pen, InputCapabilityAvailability::Unknown);
        assert_eq!(legacy.pen_pressure, InputCapabilityAvailability::Unknown);
        assert_eq!(legacy.pen_tilt, InputCapabilityAvailability::Unknown);
        assert_eq!(legacy.pen_rotation, InputCapabilityAvailability::Unknown);
        assert_eq!(legacy.pen_eraser, InputCapabilityAvailability::Unknown);
        assert_eq!(legacy.pen_proximity, InputCapabilityAvailability::Unknown);
    }

    #[test]
    fn cursor_result_reason_is_bounded_and_control_free() {
        let reason = CursorModeReason::try_from("wgc unavailable".to_owned()).unwrap();
        let result = CursorModeResultMsg {
            requested: CursorMode::Host,
            active: CursorMode::Local,
            accepted: false,
            reason,
            ..CursorModeResultMsg::default()
        };
        assert_eq!(
            serde_json::to_value(&result).unwrap()["type"],
            CURSOR_MODE_RESULT
        );
        assert_eq!(
            serde_json::from_value::<CursorModeResultMsg>(serde_json::to_value(&result).unwrap())
                .unwrap(),
            result
        );
        assert!(CursorModeReason::try_from("x".repeat(MAX_CURSOR_MODE_REASON_BYTES + 1)).is_err());
        assert!(serde_json::from_str::<CursorModeReason>("\"bad\\nreason\"").is_err());
    }

    #[test]
    fn key_event_parses_legacy_json_with_python_defaults() {
        let msg: KeyEventMsg =
            serde_json::from_str(r#"{"type":"key_event","scan_code":65,"pressed":true}"#).unwrap();
        assert_eq!(
            msg,
            KeyEventMsg {
                scan_code: 65,
                pressed: true,
                ..KeyEventMsg::default()
            }
        );
        assert_eq!(msg.modifiers, 0);
        assert_eq!(msg.caps_lock_on, None);
        assert_eq!(msg.num_lock_on, None);
        assert_eq!(msg.scroll_lock_on, None);
        assert_eq!(msg.server_x, -1);
        assert_eq!(msg.server_y, -1);
        assert_eq!(msg.sequence, 0);
        assert_eq!(msg.timestamp_ns, 0);
        assert!(!msg.coalescable);
    }

    #[test]
    fn key_event_round_trips_qt_key_modifiers_and_known_lock_state() {
        let msg = KeyEventMsg {
            scan_code: 0x0100_0021,
            pressed: true,
            modifiers: 0x02,
            caps_lock_on: Some(false),
            ..KeyEventMsg::default()
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["scan_code"], 0x0100_0021_u32);
        assert_eq!(json["modifiers"], 0x02_u32);
        assert_eq!(json["caps_lock_on"], false);
        assert!(!json.as_object().unwrap().contains_key("num_lock_on"));
        assert_eq!(serde_json::from_value::<KeyEventMsg>(json).unwrap(), msg);
    }

    #[test]
    fn request_full_frame_round_trips_with_type_field() {
        let msg = RequestFullFrameMsg::default();
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["type"], REQUEST_FULL_FRAME);
        assert_eq!(
            serde_json::from_value::<RequestFullFrameMsg>(json).unwrap(),
            msg
        );
    }

    #[test]
    fn request_full_frame_parses_minimal_json_with_python_defaults() {
        let msg: RequestFullFrameMsg = serde_json::from_str("{}").unwrap();
        assert_eq!(msg, RequestFullFrameMsg::default());
        assert_eq!(msg.msg_type, REQUEST_FULL_FRAME);
    }

    #[test]
    fn health_ping_round_trips_with_type_field() {
        let msg = HealthPingMsg {
            timestamp_ms: 1_700_000_000_000,
            sequence: 7,
            client_state: "streaming".to_string(),
            ..Default::default()
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["type"], HEALTH_PING);
        assert_eq!(json["timestamp_ms"], 1_700_000_000_000_u64);
        assert_eq!(json["sequence"], 7_u64);
        assert_eq!(json["client_state"], "streaming");
        assert_eq!(serde_json::from_value::<HealthPingMsg>(json).unwrap(), msg);
    }

    #[test]
    fn health_ping_parses_minimal_json_with_python_defaults() {
        let msg: HealthPingMsg = serde_json::from_str(r#"{"type":"health_ping"}"#).unwrap();
        assert_eq!(msg, HealthPingMsg::default());
        assert_eq!(msg.msg_type, HEALTH_PING);
        assert_eq!(msg.timestamp_ms, 0);
        assert_eq!(msg.sequence, 0);
        assert_eq!(msg.client_state, "");
        assert_eq!(msg.client_telemetry, None);
    }

    fn sample_network_snapshot() -> ClientNetworkSnapshotMsg {
        ClientNetworkSnapshotMsg::new(
            NetworkInterfaceKind::Wifi,
            NetworkScopeMsg::Lan,
            Some(866),
            Some(-52),
            Some(1_500),
            Some(NetworkIdentityMsg::try_from("HomeWifi-5G".to_string()).unwrap()),
        )
        .unwrap()
    }

    #[test]
    fn network_identity_rejects_empty_oversized_and_control_characters() {
        let ok = NetworkIdentityMsg::try_from("HomeWifi-5G".to_string()).unwrap();
        assert_eq!(ok.as_str(), "HomeWifi-5G");
        assert_eq!(
            NetworkIdentityMsg::try_from(String::new()),
            Err(NetworkIdentityError::Empty)
        );
        assert_eq!(
            NetworkIdentityMsg::try_from("x".repeat(MAX_NETWORK_IDENTITY_BYTES + 1)),
            Err(NetworkIdentityError::TooLong)
        );
        assert_eq!(
            NetworkIdentityMsg::try_from("bad\nssid".to_string()),
            Err(NetworkIdentityError::ControlCharacter)
        );
        assert!(serde_json::from_str::<NetworkIdentityMsg>("\"bad\\nssid\"").is_err());
        assert!(serde_json::from_str::<NetworkIdentityMsg>("\"\"").is_err());
    }

    #[test]
    fn client_network_snapshot_new_enforces_bounds_and_interface_semantics() {
        assert!(ClientNetworkSnapshotMsg::new(
            NetworkInterfaceKind::Wifi,
            NetworkScopeMsg::Lan,
            Some(866),
            Some(-52),
            Some(1_500),
            Some(NetworkIdentityMsg::try_from("HomeWifi-5G".to_string()).unwrap()),
        )
        .is_ok());

        // Wi-Fi-only facts (SSID, RSSI) are impossible on a non-Wi-Fi
        // interface and must be rejected, not silently accepted.
        assert_eq!(
            ClientNetworkSnapshotMsg::new(
                NetworkInterfaceKind::Ethernet,
                NetworkScopeMsg::Lan,
                Some(1_000),
                None,
                Some(1_500),
                Some(NetworkIdentityMsg::try_from("not-wifi".to_string()).unwrap()),
            ),
            Err(ClientNetworkSnapshotError::WifiFactsOnOtherInterface)
        );
        assert_eq!(
            ClientNetworkSnapshotMsg::new(
                NetworkInterfaceKind::Ethernet,
                NetworkScopeMsg::Lan,
                Some(1_000),
                Some(-40),
                Some(1_500),
                None,
            ),
            Err(ClientNetworkSnapshotError::WifiFactsOnOtherInterface)
        );
        // Zero is not a valid link rate; it must stay absent (`None`) instead.
        assert_eq!(
            ClientNetworkSnapshotMsg::new(
                NetworkInterfaceKind::Wifi,
                NetworkScopeMsg::Lan,
                Some(0),
                None,
                None,
                None,
            ),
            Err(ClientNetworkSnapshotError::InvalidLinkRate)
        );
        assert_eq!(
            ClientNetworkSnapshotMsg::new(
                NetworkInterfaceKind::Wifi,
                NetworkScopeMsg::Lan,
                None,
                None,
                Some(100),
                None,
            ),
            Err(ClientNetworkSnapshotError::InvalidMtu)
        );
        assert_eq!(
            ClientNetworkSnapshotMsg::new(
                NetworkInterfaceKind::Wifi,
                NetworkScopeMsg::Lan,
                None,
                Some(5),
                None,
                None,
            ),
            Err(ClientNetworkSnapshotError::InvalidRssi)
        );
    }

    #[test]
    fn client_network_snapshot_accepts_real_loopback_mtu_and_rejects_out_of_bounds() {
        // Linux `lo` and current Windows loopback adapters both report an
        // MTU of exactly 65,536 bytes — one past the 16-bit Ethernet/jumbo
        // ceiling. This must be accepted, not rejected or silently clamped.
        assert!(ClientNetworkSnapshotMsg::new(
            NetworkInterfaceKind::Loopback,
            NetworkScopeMsg::Lan,
            None,
            None,
            Some(65_536),
            None,
        )
        .is_ok());
        assert_eq!(
            ClientNetworkSnapshotMsg::new(
                NetworkInterfaceKind::Loopback,
                NetworkScopeMsg::Lan,
                None,
                None,
                Some(MAX_NETWORK_MTU + 1),
                None,
            ),
            Err(ClientNetworkSnapshotError::InvalidMtu)
        );
        assert_eq!(
            ClientNetworkSnapshotMsg::new(
                NetworkInterfaceKind::Wifi,
                NetworkScopeMsg::Lan,
                None,
                None,
                Some(MIN_NETWORK_MTU - 1),
                None,
            ),
            Err(ClientNetworkSnapshotError::InvalidMtu)
        );
        assert!(ClientNetworkSnapshotMsg::new(
            NetworkInterfaceKind::Wifi,
            NetworkScopeMsg::Lan,
            None,
            None,
            Some(MIN_NETWORK_MTU),
            None,
        )
        .is_ok());
    }

    #[test]
    fn client_network_snapshot_rejects_malformed_untrusted_json() {
        let zero_link_rate = r#"{"interface_kind":"wifi","scope":"lan","link_mbps":0}"#;
        assert!(serde_json::from_str::<ClientNetworkSnapshotMsg>(zero_link_rate).is_err());

        let bad_mtu = r#"{"interface_kind":"ethernet","scope":"lan","mtu":100}"#;
        assert!(serde_json::from_str::<ClientNetworkSnapshotMsg>(bad_mtu).is_err());

        let mtu_over_bounds = format!(
            r#"{{"interface_kind":"loopback","scope":"lan","mtu":{}}}"#,
            MAX_NETWORK_MTU + 1
        );
        assert!(serde_json::from_str::<ClientNetworkSnapshotMsg>(&mtu_over_bounds).is_err());

        let loopback_real_mtu = r#"{"interface_kind":"loopback","scope":"lan","mtu":65536}"#;
        assert!(serde_json::from_str::<ClientNetworkSnapshotMsg>(loopback_real_mtu).is_ok());

        let bad_rssi = r#"{"interface_kind":"wifi","scope":"lan","rssi_dbm":5}"#;
        assert!(serde_json::from_str::<ClientNetworkSnapshotMsg>(bad_rssi).is_err());

        let wifi_facts_on_ethernet = r#"{"interface_kind":"ethernet","scope":"lan","ssid":"Home"}"#;
        assert!(serde_json::from_str::<ClientNetworkSnapshotMsg>(wifi_facts_on_ethernet).is_err());

        let empty_ssid = r#"{"interface_kind":"wifi","scope":"lan","ssid":""}"#;
        assert!(serde_json::from_str::<ClientNetworkSnapshotMsg>(empty_ssid).is_err());

        let oversized_ssid = format!(
            r#"{{"interface_kind":"wifi","scope":"lan","ssid":"{}"}}"#,
            "x".repeat(MAX_NETWORK_IDENTITY_BYTES + 1)
        );
        assert!(serde_json::from_str::<ClientNetworkSnapshotMsg>(&oversized_ssid).is_err());

        // `interface_kind`/`scope` are mandatory once a snapshot is present at
        // all: a partially populated snapshot is malformed, not defaulted.
        let missing_required_fields = r#"{"scope":"lan"}"#;
        assert!(serde_json::from_str::<ClientNetworkSnapshotMsg>(missing_required_fields).is_err());

        // Unknown interface-kind/scope vocabulary (e.g. the pre-review
        // `"internet"` scope) must not silently coerce to a fallback.
        let unknown_scope = r#"{"interface_kind":"wifi","scope":"internet"}"#;
        assert!(serde_json::from_str::<ClientNetworkSnapshotMsg>(unknown_scope).is_err());
    }

    #[test]
    fn sample_window_secs_rejects_zero_and_out_of_range() {
        assert_eq!(SampleWindowSecs::try_from(0), Err(SampleWindowSecsError));
        assert_eq!(
            SampleWindowSecs::try_from(MAX_SAMPLE_WINDOW_SECS + 1),
            Err(SampleWindowSecsError)
        );
        assert!(SampleWindowSecs::try_from(5).is_ok());

        assert!(serde_json::from_str::<ClientQosSampleMsg>(r#"{"sample_window_secs":0}"#).is_err());
        assert!(serde_json::from_str::<HealthStatsMsg>(
            r#"{"type":"health_stats","sample_window_secs":0}"#
        )
        .is_err());
    }

    #[test]
    fn client_network_snapshot_round_trips_on_client_hello_and_health_ping() {
        let snapshot = sample_network_snapshot();

        let hello = ClientHelloMsg::default().with_network_snapshot(Some(snapshot.clone()));
        let hello_json = serde_json::to_value(&hello).unwrap();
        assert_eq!(hello_json["network_snapshot"]["interface_kind"], "wifi");
        assert_eq!(hello_json["network_snapshot"]["scope"], "lan");
        assert_eq!(hello_json["network_snapshot"]["ssid"], "HomeWifi-5G");
        assert!(!hello_json["network_snapshot"]
            .as_object()
            .unwrap()
            .contains_key("status"));
        let round_tripped: ClientHelloMsg = serde_json::from_value(hello_json).unwrap();
        assert_eq!(round_tripped.network_snapshot, Some(snapshot.clone()));

        let ping = HealthPingMsg {
            sequence: 9,
            client_telemetry: Some(ClientTelemetrySnapshotMsg {
                qos: Some(ClientQosSampleMsg {
                    frames_received: Some(150),
                    frames_decoded: Some(149),
                    frames_presented: Some(148),
                    frames_dropped: Some(1),
                    decode_time_ms: Some(4),
                    display_time_ms: Some(2),
                    input_send_time_ms: Some(1),
                    client_health: Some(HealthStateMsg::Ok),
                    sample_window_secs: Some(SampleWindowSecs::try_from(5).unwrap()),
                    sample_age_ms: Some(120),
                }),
                network: Some(snapshot.clone()),
            }),
            ..HealthPingMsg::default()
        };
        let ping_json = serde_json::to_value(&ping).unwrap();
        assert_eq!(ping_json["sequence"], 9_u64);
        assert_eq!(
            ping_json["client_telemetry"]["qos"]["frames_received"],
            150_u64
        );
        assert_eq!(ping_json["client_telemetry"]["qos"]["client_health"], "ok");
        assert_eq!(
            ping_json["client_telemetry"]["network"]["ssid"],
            "HomeWifi-5G"
        );
        let round_tripped_ping: HealthPingMsg = serde_json::from_value(ping_json).unwrap();
        assert_eq!(round_tripped_ping, ping);
    }

    #[test]
    fn absent_client_telemetry_and_network_snapshot_are_omitted_never_zero() {
        let hello_json = serde_json::to_value(ClientHelloMsg::default()).unwrap();
        assert!(!hello_json
            .as_object()
            .unwrap()
            .contains_key("network_snapshot"));

        let ping_json = serde_json::to_value(HealthPingMsg::default()).unwrap();
        assert!(!ping_json
            .as_object()
            .unwrap()
            .contains_key("client_telemetry"));

        // Every fact in a present-but-empty QoS sample must stay absent, never
        // fabricate zero/healthy values.
        let sparse_qos = ClientQosSampleMsg::default();
        assert_eq!(
            serde_json::to_value(&sparse_qos).unwrap(),
            serde_json::json!({})
        );

        let stats_json = serde_json::to_value(HealthStatsMsg::default()).unwrap();
        assert!(!stats_json.as_object().unwrap().contains_key("health_state"));
        assert!(!stats_json
            .as_object()
            .unwrap()
            .contains_key("sample_window_secs"));
    }

    /// Exact `ClientHelloMsg` shape as it stood immediately before this PR
    /// added `network_snapshot`, reconstructed field-for-field so
    /// compatibility tests exercise the real old wire shape rather than a
    /// hand-written approximation.
    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
    struct PreObservabilityClientHelloMsg {
        #[serde(rename = "type")]
        msg_type: String,
        client_name: String,
        version: String,
        screen_width: u32,
        screen_height: u32,
        supports_h264: bool,
        supports_h265: bool,
        supports_av1: bool,
        supports_yuv444: bool,
        supports_audio: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        audio_output: Option<AudioOutputCapabilitiesMsg>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        microphone_output: Option<MicrophoneCapabilitiesMsg>,
        supports_pen: bool,
        decoder_backend: String,
        capture_mode: String,
        picked_monitor_id: i32,
        picked_monitor_name: String,
        #[serde(default, skip_serializing_if = "is_zero_u16")]
        clipboard_protocol_version: u16,
        #[serde(default)]
        clipboard_text_c2s: bool,
        #[serde(default)]
        clipboard_text_s2c: bool,
        #[serde(default)]
        clipboard_image_c2s: bool,
        #[serde(default)]
        clipboard_image_s2c: bool,
        #[serde(default)]
        input_protocol_version: u32,
        #[serde(default, skip_serializing_if = "is_default_input_capabilities")]
        input_capabilities: InputCapabilitiesMsg,
        #[serde(default, skip_serializing_if = "is_local_cursor_mode")]
        cursor_preference: CursorMode,
        device_capabilities: BTreeMap<String, Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_log_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timezone: Option<String>,
    }

    impl From<ClientHelloMsg> for PreObservabilityClientHelloMsg {
        fn from(hello: ClientHelloMsg) -> Self {
            Self {
                msg_type: hello.msg_type,
                client_name: hello.client_name,
                version: hello.version,
                screen_width: hello.screen_width,
                screen_height: hello.screen_height,
                supports_h264: hello.supports_h264,
                supports_h265: hello.supports_h265,
                supports_av1: hello.supports_av1,
                supports_yuv444: hello.supports_yuv444,
                supports_audio: hello.supports_audio,
                audio_output: hello.audio_output,
                microphone_output: hello.microphone_output,
                supports_pen: hello.supports_pen,
                decoder_backend: hello.decoder_backend,
                capture_mode: hello.capture_mode,
                picked_monitor_id: hello.picked_monitor_id,
                picked_monitor_name: hello.picked_monitor_name,
                clipboard_protocol_version: hello.clipboard_protocol_version,
                clipboard_text_c2s: hello.clipboard_text_c2s,
                clipboard_text_s2c: hello.clipboard_text_s2c,
                clipboard_image_c2s: hello.clipboard_image_c2s,
                clipboard_image_s2c: hello.clipboard_image_s2c,
                input_protocol_version: hello.input_protocol_version,
                input_capabilities: hello.input_capabilities,
                cursor_preference: hello.cursor_preference,
                device_capabilities: hello.device_capabilities,
                session_log_id: hello.session_log_id,
                timezone: hello.timezone,
            }
        }
    }

    #[test]
    fn pre_pr3_client_hello_shape_is_bidirectionally_compatible() {
        // Old client -> new host: an old wire payload carrying none of this
        // PR's fields still parses, with the new field absent (never
        // fabricated).
        let old = PreObservabilityClientHelloMsg::from(ClientHelloMsg::default());
        let old_json = serde_json::to_string(&old).unwrap();
        assert!(!old_json.contains("network_snapshot"));
        let upgraded: ClientHelloMsg = serde_json::from_str(&old_json).unwrap();
        assert_eq!(upgraded.network_snapshot, None);
        assert_eq!(upgraded.client_name, old.client_name);
        assert_eq!(upgraded.device_capabilities, old.device_capabilities);

        // New client -> old host: a new wire payload with `network_snapshot`
        // populated still parses against the exact pre-PR3 shape, which
        // silently drops the unknown field per current unknown-field policy.
        let new = ClientHelloMsg::default().with_network_snapshot(Some(sample_network_snapshot()));
        let new_json = serde_json::to_string(&new).unwrap();
        assert!(new_json.contains("network_snapshot"));
        let downgraded: PreObservabilityClientHelloMsg = serde_json::from_str(&new_json).unwrap();
        assert_eq!(downgraded.client_name, new.client_name);
        assert_eq!(downgraded.device_capabilities, new.device_capabilities);
    }

    /// Exact `HealthPingMsg` shape as it stood immediately before this PR
    /// added `client_telemetry`.
    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    struct PreObservabilityHealthPingMsg {
        #[serde(rename = "type", default = "default_health_ping_type")]
        msg_type: String,
        #[serde(default)]
        timestamp_ms: u64,
        #[serde(default)]
        sequence: u64,
        #[serde(default)]
        client_state: String,
    }

    impl Default for PreObservabilityHealthPingMsg {
        fn default() -> Self {
            Self {
                msg_type: default_health_ping_type(),
                timestamp_ms: 0,
                sequence: 0,
                client_state: String::new(),
            }
        }
    }

    #[test]
    fn pre_pr3_health_ping_shape_is_bidirectionally_compatible() {
        // Old client -> new host.
        let old = PreObservabilityHealthPingMsg {
            timestamp_ms: 1_700_000_000_000,
            sequence: 42,
            client_state: "streaming".to_string(),
            ..PreObservabilityHealthPingMsg::default()
        };
        let old_json = serde_json::to_string(&old).unwrap();
        let upgraded: HealthPingMsg = serde_json::from_str(&old_json).unwrap();
        assert_eq!(upgraded.client_telemetry, None);
        assert_eq!(upgraded.sequence, old.sequence);
        assert_eq!(upgraded.timestamp_ms, old.timestamp_ms);
        assert_eq!(upgraded.client_state, old.client_state);

        // New client -> old host: unknown `client_telemetry` is dropped, and
        // the shared `sequence` (reused for RTT) still round-trips.
        let new = HealthPingMsg {
            sequence: 21,
            client_telemetry: Some(ClientTelemetrySnapshotMsg {
                qos: Some(ClientQosSampleMsg {
                    frames_decoded: Some(3),
                    ..ClientQosSampleMsg::default()
                }),
                network: None,
            }),
            ..HealthPingMsg::default()
        };
        let new_json = serde_json::to_string(&new).unwrap();
        assert!(new_json.contains("client_telemetry"));
        let downgraded: PreObservabilityHealthPingMsg = serde_json::from_str(&new_json).unwrap();
        assert_eq!(downgraded.sequence, new.sequence);
        assert_eq!(downgraded.msg_type, HEALTH_PING);
    }

    /// Exact `HealthStatsMsg` shape as it stood immediately before this PR
    /// added `health_state`/`sample_window_secs`.
    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
    struct PreObservabilityHealthStatsMsg {
        #[serde(rename = "type")]
        msg_type: String,
        #[serde(default)]
        rtt_ms: f64,
        #[serde(default)]
        fps_actual: f64,
        #[serde(default)]
        fps_target: f64,
        #[serde(default)]
        bandwidth_mbps: f64,
        #[serde(default)]
        frames_sent: u64,
        #[serde(default)]
        frames_dropped: u64,
        #[serde(default)]
        encode_time_ms: f64,
        #[serde(default)]
        capture_time_ms: f64,
        #[serde(default)]
        input_latency_ms: f64,
        #[serde(default)]
        input_events: u64,
        #[serde(default)]
        last_input_sequence: u64,
        #[serde(default)]
        last_input_type: String,
        #[serde(default)]
        transmit_time_ms: f64,
        #[serde(default)]
        decode_time_ms: f64,
        #[serde(default)]
        display_time_ms: f64,
        #[serde(default)]
        keyframe_requested: u64,
        #[serde(default)]
        keyframe_emitted: u64,
        #[serde(default = "default_codec")]
        codec: String,
        #[serde(default = "default_chroma")]
        chroma: String,
        #[serde(default = "default_resolution")]
        resolution: String,
        #[serde(default)]
        clients_connected: u64,
    }

    impl Default for PreObservabilityHealthStatsMsg {
        fn default() -> Self {
            Self {
                msg_type: HEALTH_STATS.to_string(),
                rtt_ms: 0.0,
                fps_actual: 0.0,
                fps_target: 0.0,
                bandwidth_mbps: 0.0,
                frames_sent: 0,
                frames_dropped: 0,
                encode_time_ms: 0.0,
                capture_time_ms: 0.0,
                input_latency_ms: 0.0,
                input_events: 0,
                last_input_sequence: 0,
                last_input_type: String::new(),
                transmit_time_ms: 0.0,
                decode_time_ms: 0.0,
                display_time_ms: 0.0,
                keyframe_requested: 0,
                keyframe_emitted: 0,
                codec: default_codec(),
                chroma: default_chroma(),
                resolution: default_resolution(),
                clients_connected: 0,
            }
        }
    }

    #[test]
    fn pre_pr3_health_stats_shape_is_bidirectionally_compatible() {
        // Old host -> new client.
        let old = PreObservabilityHealthStatsMsg {
            fps_actual: 59.8,
            input_events: 12,
            ..PreObservabilityHealthStatsMsg::default()
        };
        let old_json = serde_json::to_string(&old).unwrap();
        let upgraded: HealthStatsMsg = serde_json::from_str(&old_json).unwrap();
        assert_eq!(upgraded.health_state, None);
        assert_eq!(upgraded.sample_window_secs, None);
        assert_eq!(upgraded.fps_actual, old.fps_actual);
        assert_eq!(upgraded.input_events, old.input_events);

        // New host -> old client: unknown `health_state`/`sample_window_secs`
        // are dropped, shared fields still round-trip.
        let new = HealthStatsMsg {
            health_state: Some(HealthStateMsg::Degraded),
            sample_window_secs: Some(SampleWindowSecs::try_from(10).unwrap()),
            ..HealthStatsMsg::default()
        };
        let new_json = serde_json::to_string(&new).unwrap();
        assert!(new_json.contains("health_state"));
        let downgraded: PreObservabilityHealthStatsMsg = serde_json::from_str(&new_json).unwrap();
        assert_eq!(downgraded.codec, new.codec);
        assert_eq!(downgraded.msg_type, HEALTH_STATS);
    }

    #[test]
    fn health_stats_health_state_and_sample_window_are_additive() {
        let stats = HealthStatsMsg {
            health_state: Some(HealthStateMsg::Degraded),
            sample_window_secs: Some(SampleWindowSecs::try_from(10).unwrap()),
            ..HealthStatsMsg::default()
        };
        let json = serde_json::to_value(&stats).unwrap();
        assert_eq!(json["health_state"], "degraded");
        assert_eq!(json["sample_window_secs"], 10);
        let round_tripped: HealthStatsMsg = serde_json::from_value(json).unwrap();
        assert_eq!(round_tripped, stats);

        // Old host JSON without these fields still parses with `None`, never
        // a fabricated `Ok`/zero window.
        let legacy: HealthStatsMsg = serde_json::from_str(r#"{"type":"health_stats"}"#).unwrap();
        assert_eq!(legacy.health_state, None);
        assert_eq!(legacy.sample_window_secs, None);
    }

    #[test]
    fn health_ping_reuses_single_sequence_for_rtt_and_telemetry() {
        let ping = HealthPingMsg {
            sequence: 55,
            client_telemetry: Some(ClientTelemetrySnapshotMsg::default()),
            ..HealthPingMsg::default()
        };
        let json = serde_json::to_value(&ping).unwrap();
        let obj = json.as_object().unwrap();
        assert_eq!(obj["sequence"], 55_u64);
        // Exactly one sequence field exists: no duplicate telemetry sequence
        // mechanism at the top level or nested inside the telemetry payload.
        assert!(!obj.contains_key("telemetry_sequence"));
        if let Some(telemetry) = obj.get("client_telemetry") {
            assert!(!telemetry.as_object().unwrap().contains_key("sequence"));
        }

        let pong = HealthPongMsg {
            sequence: ping.sequence,
            ..HealthPongMsg::default()
        };
        assert_eq!(pong.sequence, ping.sequence);

        // `HealthPingMsg` retains its `Eq` contract even with client
        // telemetry attached, since every new field is integer/enum-typed.
        fn assert_eq_impl<T: Eq>() {}
        assert_eq_impl::<HealthPingMsg>();
    }

    #[test]
    fn no_remote_log_control_field_crosses_the_wire() {
        fn assert_no_log_control_keys(value: &serde_json::Value) {
            match value {
                serde_json::Value::Object(map) => {
                    for (key, nested) in map {
                        let lower = key.to_lowercase();
                        assert!(
                            !lower.contains("log_level")
                                && !lower.contains("loglevel")
                                && !lower.contains("log_profile")
                                && !lower.contains("verbosity")
                                && !lower.contains("logging"),
                            "unexpected logging-control field `{key}`"
                        );
                        assert_no_log_control_keys(nested);
                    }
                }
                serde_json::Value::Array(items) => {
                    for item in items {
                        assert_no_log_control_keys(item);
                    }
                }
                _ => {}
            }
        }

        let hello =
            ClientHelloMsg::default().with_network_snapshot(Some(sample_network_snapshot()));
        assert_no_log_control_keys(&serde_json::to_value(&hello).unwrap());

        let ping = HealthPingMsg {
            client_telemetry: Some(ClientTelemetrySnapshotMsg {
                qos: Some(ClientQosSampleMsg {
                    client_health: Some(HealthStateMsg::Critical),
                    ..ClientQosSampleMsg::default()
                }),
                network: Some(sample_network_snapshot()),
            }),
            ..HealthPingMsg::default()
        };
        assert_no_log_control_keys(&serde_json::to_value(&ping).unwrap());

        let stats = HealthStatsMsg {
            health_state: Some(HealthStateMsg::Ok),
            sample_window_secs: Some(SampleWindowSecs::try_from(60).unwrap()),
            ..HealthStatsMsg::default()
        };
        assert_no_log_control_keys(&serde_json::to_value(&stats).unwrap());
    }

    #[test]
    fn mouse_scroll_round_trips_with_type_field() {
        let msg = MouseScrollMsg {
            x: 0.25,
            y: 0.75,
            dx: 1.0,
            dy: -2.0,
            server_x: 960,
            server_y: 540,
            sequence: 8,
            timestamp_ns: 123,
            ..Default::default()
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["type"], MOUSE_SCROLL);
        assert_eq!(json["server_x"], 960);
        assert_eq!(json["server_y"], 540);
        assert_eq!(serde_json::from_value::<MouseScrollMsg>(json).unwrap(), msg);
    }

    #[test]
    fn mouse_scroll_parses_minimal_json_with_python_defaults() {
        let msg: MouseScrollMsg = serde_json::from_str(r#"{"type":"mouse_scroll"}"#).unwrap();
        assert_eq!(msg, MouseScrollMsg::default());
        assert_eq!(msg.msg_type, MOUSE_SCROLL);
        assert_eq!(msg.server_x, -1);
        assert_eq!(msg.server_y, -1);
        assert!(!msg.coalescable);
    }

    #[test]
    fn key_reset_modifiers_round_trips_with_type_field() {
        let msg = KeyResetModifiersMsg {
            reason: "reconnect".to_string(),
            ..Default::default()
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["type"], KEY_RESET_MODIFIERS);
        assert_eq!(json["reason"], "reconnect");
        assert_eq!(
            serde_json::from_value::<KeyResetModifiersMsg>(json).unwrap(),
            msg
        );
    }

    #[test]
    fn key_reset_modifiers_parses_minimal_json_with_python_defaults() {
        let msg: KeyResetModifiersMsg =
            serde_json::from_str(r#"{"type":"key_reset_modifiers"}"#).unwrap();
        assert_eq!(msg, KeyResetModifiersMsg::default());
        assert_eq!(msg.msg_type, KEY_RESET_MODIFIERS);
        assert_eq!(msg.reason, "unknown");
    }

    #[test]
    fn text_commit_round_trips_with_type_field() {
        let msg = TextCommitMsg {
            text: "あ".to_string(),
            ..Default::default()
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["type"], TEXT_COMMIT);
        assert_eq!(json["text"], "あ");
        assert_eq!(serde_json::from_value::<TextCommitMsg>(json).unwrap(), msg);
    }

    #[test]
    fn text_commit_parses_minimal_json_with_python_defaults() {
        let msg: TextCommitMsg = serde_json::from_str(r#"{"type":"text_commit"}"#).unwrap();
        assert_eq!(msg, TextCommitMsg::default());
        assert_eq!(msg.msg_type, TEXT_COMMIT);
        assert_eq!(msg.text, "");
    }

    #[test]
    fn display_update_round_trips_with_type_field() {
        let msg = DisplayUpdateMsg {
            sequence: 7,
            width: 1512,
            height: 944,
            scale: 2.0,
            reason: "fullscreen".to_string(),
            ..Default::default()
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["type"], DISPLAY_UPDATE);
        assert_eq!(json["sequence"], 7);
        assert_eq!(json["width"], 1512);
        assert_eq!(json["height"], 944);
        assert_eq!(
            serde_json::from_value::<DisplayUpdateMsg>(json).unwrap(),
            msg
        );
    }

    #[test]
    fn display_update_parses_minimal_json() {
        let msg: DisplayUpdateMsg = serde_json::from_str(r#"{"type":"display_update"}"#).unwrap();
        assert_eq!(msg, DisplayUpdateMsg::default());
        assert_eq!(msg.sequence, 0);
        assert_eq!(msg.width, 0);
    }

    #[test]
    fn display_update_result_round_trips() {
        let msg = DisplayUpdateResultMsg {
            sequence: 7,
            accepted: false,
            width: 1512,
            height: 982,
            message: "display retarget failed".to_string(),
            ..Default::default()
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["type"], DISPLAY_UPDATE_RESULT);
        assert_eq!(json["accepted"], false);
        assert_eq!(
            serde_json::from_value::<DisplayUpdateResultMsg>(json).unwrap(),
            msg
        );
    }

    #[test]
    fn legacy_server_hello_defaults_display_update_capability_off() {
        // A hello from a host that predates the field must parse with the
        // capability off so clients never send display_update to old hosts.
        let hello: ServerHelloMsg = serde_json::from_str(r#"{"type":"server_hello"}"#).unwrap();
        assert!(!hello.supports_display_update);
    }

    #[test]
    fn stream_bounds_are_even_and_ordered() {
        assert_eq!(MIN_STREAM_WIDTH % 2, 0);
        assert_eq!(MIN_STREAM_HEIGHT % 2, 0);
        assert_eq!(MAX_STREAM_WIDTH % 2, 0);
        assert_eq!(MAX_STREAM_HEIGHT % 2, 0);
        assert!(std::hint::black_box(MIN_STREAM_WIDTH) < MAX_STREAM_WIDTH);
        assert!(std::hint::black_box(MIN_STREAM_HEIGHT) < MAX_STREAM_HEIGHT);
    }

    fn sample_pen_event() -> PenEventMsg {
        PenEventMsg {
            x: 0.5,
            y: 0.25,
            server_x: 960,
            server_y: 270,
            pressure: 0.75,
            tilt_x_degrees: -12.0,
            tilt_y_degrees: 10.0,
            rotation_degrees: 180.0,
            tool: PenToolMsg::Tip,
            in_proximity: true,
            touching: true,
            buttons: 0b01,
            sequence: 42,
            timestamp_ns: 123_456,
            coalescable: false,
            ..PenEventMsg::default()
        }
    }

    /// Golden wire shape: field names, order-independent JSON, and the exact
    /// `"pen_event"` type discriminator are locked.
    #[test]
    fn pen_event_golden_json_round_trips() {
        let msg = sample_pen_event();
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["type"], PEN_EVENT);
        assert_eq!(json["x"], 0.5);
        assert_eq!(json["y"], 0.25);
        assert_eq!(json["server_x"], 960);
        assert_eq!(json["server_y"], 270);
        assert_eq!(json["tool"], "tip");
        assert_eq!(json["in_proximity"], true);
        assert_eq!(json["touching"], true);
        assert_eq!(json["buttons"], 1);
        assert_eq!(json["sequence"], 42);
        assert_eq!(json["coalescable"], false);
        assert_eq!(serde_json::from_value::<PenEventMsg>(json).unwrap(), msg);
        assert!(msg.validate().is_ok());
    }

    /// An eraser sample round-trips with the same shape as the tip.
    #[test]
    fn pen_event_eraser_tool_round_trips() {
        let msg = PenEventMsg {
            tool: PenToolMsg::Eraser,
            ..sample_pen_event()
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["tool"], "eraser");
        assert_eq!(serde_json::from_value::<PenEventMsg>(json).unwrap(), msg);
    }

    /// A minimal/legacy-shaped payload (only the required fields) must still
    /// parse, with every additive field defaulting exactly like
    /// `MouseMoveMsg`/`MouseButtonMsg`: `-1` server sentinel, zero
    /// sequence/timestamp/tilt/rotation/buttons, `false` proximity/touching,
    /// and `coalescable = true`.
    #[test]
    fn pen_event_parses_minimal_json_with_safe_defaults() {
        let msg: PenEventMsg = serde_json::from_str(
            r#"{"type":"pen_event","x":0.1,"y":0.9,"pressure":0.3,"tool":"tip"}"#,
        )
        .unwrap();
        assert_eq!(
            msg,
            PenEventMsg {
                x: 0.1,
                y: 0.9,
                pressure: 0.3,
                ..PenEventMsg::default()
            }
        );
        assert_eq!(msg.server_x, -1);
        assert_eq!(msg.server_y, -1);
        assert_eq!(msg.tilt_x_degrees, 0.0);
        assert_eq!(msg.tilt_y_degrees, 0.0);
        assert_eq!(msg.rotation_degrees, 0.0);
        assert!(!msg.in_proximity);
        assert!(!msg.touching);
        assert_eq!(msg.buttons, 0);
        assert_eq!(msg.sequence, 0);
        assert_eq!(msg.timestamp_ns, 0);
        assert!(msg.coalescable);
        assert!(msg.validate().is_ok());
    }

    /// Unknown/future fields do not break deserialization (unknown-field
    /// tolerance, matching every other message type in this file).
    #[test]
    fn pen_event_ignores_unknown_fields() {
        let msg: PenEventMsg = serde_json::from_str(
            r#"{"type":"pen_event","x":0.1,"y":0.9,"pressure":0.3,"tool":"tip","future_field":true}"#,
        )
        .unwrap();
        assert_eq!(msg.x, 0.1);
    }

    /// Non-finite and out-of-range fields must fail `validate()` before a
    /// product ever advances its input sequence or injects native input.
    /// Deserialization itself does not reject them — only `validate()` does.
    #[test]
    fn pen_event_validate_rejects_non_finite_and_out_of_range_fields() {
        let base = sample_pen_event();

        let mut nan_pressure = base.clone();
        nan_pressure.pressure = f32::NAN;
        assert_eq!(
            nan_pressure.validate(),
            Err(PenEventValidationError::OutOfRange("pressure"))
        );

        let mut infinite_x = base.clone();
        infinite_x.x = f64::INFINITY;
        assert_eq!(
            infinite_x.validate(),
            Err(PenEventValidationError::OutOfRange("x"))
        );

        let mut over_pressure = base.clone();
        over_pressure.pressure = 1.5;
        assert_eq!(
            over_pressure.validate(),
            Err(PenEventValidationError::OutOfRange("pressure"))
        );

        let mut bad_tilt = base.clone();
        bad_tilt.tilt_x_degrees = 91.0;
        assert_eq!(
            bad_tilt.validate(),
            Err(PenEventValidationError::OutOfRange("tilt_x_degrees"))
        );

        let mut bad_rotation = base.clone();
        bad_rotation.rotation_degrees = -0.1;
        assert_eq!(
            bad_rotation.validate(),
            Err(PenEventValidationError::OutOfRange("rotation_degrees"))
        );

        // Rotation's inclusive upper bound (360) is accepted: 0 and 360 both
        // denote the same physical angle, and a sender may use either
        // boundary convention.
        let mut rotation_360 = base;
        rotation_360.rotation_degrees = 360.0;
        assert!(rotation_360.validate().is_ok());
    }

    /// A round-tripped `PenEventMsg` still deserializes even when malformed
    /// numerically (JSON has no NaN/Infinity, but an untrusted peer can send
    /// an out-of-range finite value); `validate()` is the required gate.
    #[test]
    fn pen_event_deserializes_out_of_range_value_but_validate_rejects_it() {
        let msg: PenEventMsg = serde_json::from_str(
            r#"{"type":"pen_event","x":0.1,"y":0.9,"pressure":2.0,"tool":"tip"}"#,
        )
        .unwrap();
        assert_eq!(msg.pressure, 2.0);
        assert_eq!(
            msg.validate(),
            Err(PenEventValidationError::OutOfRange("pressure"))
        );
    }

    /// An input-v2 peer never advertises pen capability truth, so a host
    /// must never treat it as authorized for typed pen — the same
    /// unknown-never-authorizes rule already proven for pointer/cursor
    /// capabilities.
    #[test]
    fn input_v2_peer_capabilities_never_authorize_pen() {
        // Exact ClientHelloMsg JSON shape as an input-v2 (pre-pen) peer would
        // have sent it: input_protocol_version = 2 and no pen capability
        // keys at all.
        let legacy_v2_hello: ClientHelloMsg = serde_json::from_str(
            r#"{
                "type":"client_hello","client_name":"old","version":"3",
                "screen_width":1,"screen_height":1,
                "supports_h264":false,"supports_h265":false,"supports_av1":false,
                "supports_yuv444":false,"supports_audio":false,"supports_pen":false,
                "decoder_backend":"","capture_mode":"","picked_monitor_id":-1,
                "picked_monitor_name":"","device_capabilities":{},
                "input_protocol_version":2,
                "input_capabilities":{"absolute_pointer":"available","relative_pointer":"available"}
            }"#,
        )
        .unwrap();
        assert_eq!(legacy_v2_hello.input_protocol_version, 2);
        assert_eq!(
            legacy_v2_hello.input_capabilities.pen,
            InputCapabilityAvailability::Unknown
        );
        assert_eq!(
            legacy_v2_hello.input_capabilities.region_input,
            InputCapabilityAvailability::Unknown
        );
        assert_ne!(
            legacy_v2_hello.input_protocol_version,
            INPUT_PROTOCOL_VERSION
        );
        assert_eq!(INPUT_PROTOCOL_VERSION, 4);
    }

    /// Adding pen fields must not disturb the already-locked mouse/keyboard
    /// legacy compatibility contract.
    #[test]
    fn pen_wire_contract_does_not_change_mouse_or_key_event_defaults() {
        let mouse: MouseMoveMsg =
            serde_json::from_str(r#"{"type":"mouse_move","x":0.1,"y":0.2}"#).unwrap();
        assert_eq!(mouse.server_x, -1);
        assert_eq!(mouse.server_y, -1);
        assert!(mouse.coalescable);

        let key: KeyEventMsg =
            serde_json::from_str(r#"{"type":"key_event","scan_code":65,"pressed":true}"#).unwrap();
        assert_eq!(key.modifiers, 0);
        assert!(!key.coalescable);
    }

    #[test]
    fn tablet_mode_defaults_are_backward_compatible() {
        let hello = ClientHelloMsg::default();
        let json = serde_json::to_value(&hello).unwrap();
        assert!(json.get("tablet_mode_requested").is_none());
        assert!(json.get("tablet_mode_capabilities").is_none());
        assert!(json.get("usb_hard_device").is_none());

        let legacy: ClientHelloMsg = serde_json::from_value(json).unwrap();
        assert_eq!(
            legacy.tablet_mode_requested,
            TabletModeMsg::LocalTermination
        );
    }

    #[test]
    fn hard_usb_device_metadata_is_additive_and_exact() {
        let hello = ClientHelloMsg {
            usb_hard_v1: true,
            usb_hard_device: Some(UsbHardDeviceMsg {
                vendor_id: 0x056a,
                product_id: 0x0317,
                bcd_device: 0x0100,
                device_class: 0,
                speed: UsbSpeed::Full,
            }),
            ..ClientHelloMsg::default()
        };
        let json = serde_json::to_value(&hello).unwrap();
        assert_eq!(json["usb_hard_device"]["vendor_id"], 0x056a);
        assert_eq!(json["usb_hard_device"]["speed"], "full");
        assert_eq!(
            serde_json::from_value::<ClientHelloMsg>(json)
                .unwrap()
                .usb_hard_device,
            hello.usb_hard_device
        );
    }

    #[test]
    fn legacy_input_v3_pen_capability_authorizes_only_local_termination() {
        let legacy = ClientHelloMsg {
            input_protocol_version: PEN_INPUT_PROTOCOL_VERSION,
            input_capabilities: InputCapabilitiesMsg {
                pen: InputCapabilityAvailability::Available,
                ..InputCapabilitiesMsg::default()
            },
            tablet_mode_capabilities: TabletModeCapabilitiesMsg::default(),
            ..ClientHelloMsg::default()
        };
        let effective = legacy.effective_tablet_mode_capabilities();
        assert_eq!(
            effective.local_termination,
            InputCapabilityAvailability::Available
        );
        assert_eq!(
            effective.wacom_usb_bridge,
            InputCapabilityAvailability::Unknown
        );

        let explicit_unavailable = ClientHelloMsg {
            tablet_mode_capabilities: TabletModeCapabilitiesMsg {
                local_termination: InputCapabilityAvailability::Unavailable,
                ..TabletModeCapabilitiesMsg::default()
            },
            ..legacy
        };
        assert_eq!(
            explicit_unavailable
                .effective_tablet_mode_capabilities()
                .local_termination,
            InputCapabilityAvailability::Unavailable
        );
    }

    #[test]
    fn tablet_mode_reason_rejects_overlong_strings() {
        let too_long = "x".repeat(MAX_TABLET_MODE_REASON_BYTES + 1);
        assert_eq!(
            TabletModeReason::try_from(too_long),
            Err(TabletModeReasonError::TooLong)
        );
    }

    #[test]
    fn cursor_shape_msg_default_type_field_is_cursor_shape() {
        let msg = CursorShapeMsg::default();
        assert_eq!(msg.msg_type, CURSOR_SHAPE);
        assert_eq!(msg.shape, CursorShapeKind::Default);
        assert_eq!(msg.sequence, 0);
    }

    #[test]
    fn cursor_shape_msg_round_trips_through_json() {
        let msg = CursorShapeMsg {
            msg_type: CURSOR_SHAPE.to_owned(),
            shape: CursorShapeKind::ResizeNs,
            sequence: 7,
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["type"], CURSOR_SHAPE);
        assert_eq!(json["shape"], "resize_ns");
        assert_eq!(json["sequence"], 7);
        let round_tripped: CursorShapeMsg = serde_json::from_value(json).unwrap();
        assert_eq!(round_tripped, msg);
    }

    #[test]
    fn cursor_shape_kind_defaults_to_default_on_unknown_variant() {
        // An older client receiving a shape added by a newer host must fall
        // back to Default rather than failing to deserialise the whole message.
        let json = serde_json::json!({"type": "cursor_shape", "shape": "totally_new_shape_2099", "sequence": 1});
        let msg: CursorShapeMsg = serde_json::from_value(json).unwrap();
        assert_eq!(msg.shape, CursorShapeKind::Default);
    }

    #[test]
    fn cursor_shape_kind_all_variants_serialise_as_snake_case() {
        let pairs = [
            (CursorShapeKind::Default, "default"),
            (CursorShapeKind::Text, "text"),
            (CursorShapeKind::ResizeNs, "resize_ns"),
            (CursorShapeKind::ResizeEw, "resize_ew"),
            (CursorShapeKind::ResizeNwse, "resize_nwse"),
            (CursorShapeKind::ResizeNesw, "resize_nesw"),
            (CursorShapeKind::ResizeAll, "resize_all"),
            (CursorShapeKind::Pointer, "pointer"),
            (CursorShapeKind::Crosshair, "crosshair"),
            (CursorShapeKind::Grab, "grab"),
            (CursorShapeKind::Grabbing, "grabbing"),
            (CursorShapeKind::ZoomIn, "zoom_in"),
            (CursorShapeKind::ZoomOut, "zoom_out"),
            (CursorShapeKind::Wait, "wait"),
            (CursorShapeKind::Progress, "progress"),
            (CursorShapeKind::Help, "help"),
            (CursorShapeKind::NotAllowed, "not_allowed"),
            (CursorShapeKind::Hidden, "hidden"),
        ];
        for (kind, expected) in pairs {
            let json = serde_json::to_value(kind).unwrap();
            assert_eq!(json, expected, "{kind:?} did not serialise as {expected}");
            let round_tripped: CursorShapeKind = serde_json::from_value(json).unwrap();
            assert_eq!(round_tripped, kind);
        }
    }
}
