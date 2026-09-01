use arcen_protocol::messages::{AuthResponse, VideoSelectionIntent};
use arcen_protocol::{ChromaSubsampling, VideoCodec};
use arcen_telemetry::CorrelationId;
use serde::{Deserialize, Serialize};

use crate::ipc::PipeStream;
use crate::{ColorPolicy, HostConfig};

const AGENT_SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(10);

/// How long to wait for Windows to finish moving a session onto the physical
/// console after `WTSConnectSessionW` accepts the request.
pub const CONSOLE_MOVE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// How often to re-read the active console while that move is in flight.
const CONSOLE_MOVE_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct WindowsSessionIdentity {
    pub session_id: u32,
    pub user_sid: String,
    pub user: String,
    pub domain: String,
    pub state: String,
    pub launch_backend: String,
}

impl WindowsSessionIdentity {
    pub fn account_name(&self) -> String {
        if self.domain.is_empty() {
            self.user.clone()
        } else {
            format!(r"{}\{}", self.domain, self.user)
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentConfig {
    capenc_bin: String,
    global_output_index: Option<u32>,
    adapter_name: Option<String>,
    adapter_output_index: Option<u32>,
    codec: String,
    chroma: String,
    #[serde(default = "default_bit_depth")]
    bit_depth: String,
    #[serde(default = "default_color_range")]
    color_range: String,
    #[serde(default = "default_color_matrix")]
    color_matrix: String,
    #[serde(default = "default_color_policy")]
    color_policy: String,
    #[serde(default = "default_qp_map")]
    qp_map: String,
    #[serde(default)]
    video_selection: VideoSelectionIntent,
    #[serde(default)]
    codec_pinned: bool,
    #[serde(default)]
    variant_pinned: bool,
    #[serde(default)]
    auth_video_request: Option<arcen_protocol::messages::InitialVideoRequestMsg>,
    fps: u32,
    #[serde(default)]
    encoder: Option<String>,
    audio_enabled: bool,
    #[serde(default)]
    audio_compressed: bool,
    #[serde(default)]
    microphone_input_enabled: bool,
    clipboard_policy: arcen_media::clipboard::ClipboardPolicy,
    #[serde(default)]
    timezone_redirection: bool,
    #[serde(default)]
    qos_targets: arcen_telemetry::QosTargets,
    #[serde(default)]
    deskside: crate::deskside::DesksideConfig,
    #[serde(default)]
    iddcx: crate::config::WindowsIddCxConfig,
    #[serde(default)]
    multi_monitor: crate::config::WindowsMultiMonitorConfig,
}

fn default_bit_depth() -> String {
    arcen_media::BitDepth::Eight.token().to_string()
}

fn default_color_range() -> String {
    arcen_media::ColorRange::Limited.token().to_string()
}

fn default_color_matrix() -> String {
    arcen_media::ColorMatrix::Bt709.token().to_string()
}

fn default_color_policy() -> String {
    ColorPolicy::DefaultOff.token().to_string()
}

fn default_qp_map() -> String {
    arcen_media::video::QpMapPolicy::default()
        .token()
        .to_string()
}

impl AgentConfig {
    pub fn from_host(config: &HostConfig) -> Self {
        let (global_output_index, adapter_name, adapter_output_index) =
            match &config.output_selector {
                crate::display::OutputSelector::GlobalIndex(index) => (Some(*index), None, None),
                crate::display::OutputSelector::Adapter { name, output_index } => {
                    (None, Some(name.clone()), Some(*output_index))
                }
            };
        Self {
            capenc_bin: config.capenc_bin.clone(),
            global_output_index,
            adapter_name,
            adapter_output_index,
            codec: config.codec_name().to_string(),
            chroma: config.chroma_name().to_string(),
            bit_depth: config.bit_depth.token().to_string(),
            color_range: config.color_range.token().to_string(),
            color_matrix: config.color_matrix.token().to_string(),
            color_policy: config.color_policy.token().to_string(),
            qp_map: config.qp_map.token().to_string(),
            video_selection: config.video_selection,
            codec_pinned: config.codec_pinned,
            variant_pinned: config.variant_pinned,
            auth_video_request: config.auth_video_request.clone(),
            fps: config.fps,
            encoder: Some(config.encoder.name().to_string()),
            audio_enabled: config.audio_enabled,
            audio_compressed: config.audio_compressed,
            microphone_input_enabled: config.microphone_input_enabled,
            clipboard_policy: config.clipboard_policy,
            timezone_redirection: config.timezone_redirection,
            qos_targets: config.qos_targets,
            deskside: config.deskside.clone(),
            iddcx: config.iddcx.clone(),
            multi_monitor: config.multi_monitor.clone(),
        }
    }

    pub fn into_host(self) -> Result<HostConfig, String> {
        let codec = match self.codec.as_str() {
            "h264" => VideoCodec::H264,
            "h265" => VideoCodec::H265,
            "av1" => VideoCodec::Av1,
            other => return Err(format!("agent start has unsupported codec {other}")),
        };
        let chroma = match self.chroma.as_str() {
            "yuv420" => ChromaSubsampling::Yuv420,
            "yuv444" => ChromaSubsampling::Yuv444,
            other => return Err(format!("agent start has unsupported chroma {other}")),
        };
        if self.fps == 0 || self.fps > 240 {
            return Err("agent start FPS is outside 1..=240".to_string());
        }
        if chroma == ChromaSubsampling::Yuv444 && codec != VideoCodec::H265 {
            return Err(format!(
                "agent start yuv444 requires h265; {} supports only yuv420",
                crate::codec_name(codec)
            ));
        }
        let bit_depth = arcen_media::BitDepth::from_token(&self.bit_depth)
            .ok_or_else(|| format!("agent start has unsupported bit depth {}", self.bit_depth))?;
        let color_range =
            arcen_media::ColorRange::from_token(&self.color_range).ok_or_else(|| {
                format!(
                    "agent start has unsupported color range {}",
                    self.color_range
                )
            })?;
        let color_matrix =
            arcen_media::ColorMatrix::from_token(&self.color_matrix).ok_or_else(|| {
                format!(
                    "agent start has unsupported color matrix {}",
                    self.color_matrix
                )
            })?;
        let color_policy = ColorPolicy::from_token(&self.color_policy).ok_or_else(|| {
            format!(
                "agent start has unsupported color policy {}",
                self.color_policy
            )
        })?;
        let qp_map = arcen_media::video::QpMapPolicy::from_token(&self.qp_map)
            .ok_or_else(|| format!("agent start has unsupported qp-map policy {}", self.qp_map))?;
        let encoder = match self.encoder.as_deref() {
            None | Some("") | Some("auto") => crate::capenc::EncoderSelection::Auto,
            Some("nvenc") => crate::capenc::EncoderSelection::Nvenc,
            Some("software-h264") | Some("openh264") => {
                crate::capenc::EncoderSelection::SoftwareH264
            }
            Some(other) => return Err(format!("agent start has unsupported encoder {other}")),
        };
        if encoder == crate::capenc::EncoderSelection::SoftwareH264
            && (codec != VideoCodec::H264 || chroma != ChromaSubsampling::Yuv420)
            && (self.codec_pinned || self.variant_pinned)
        {
            return Err("agent start exact video pin is incompatible with OpenH264".to_string());
        }
        let output_selector = match (
            self.global_output_index,
            self.adapter_name,
            self.adapter_output_index,
        ) {
            (Some(index), None, None) => crate::display::OutputSelector::GlobalIndex(index),
            (None, Some(name), output) if !name.trim().is_empty() => {
                crate::display::OutputSelector::Adapter {
                    name,
                    output_index: output.unwrap_or(0),
                }
            }
            _ => return Err("agent start has an invalid output selector".to_string()),
        };
        self.deskside.validate()?;
        self.iddcx.validate(&self.multi_monitor)?;
        let mut config = HostConfig {
            capenc_bin: self.capenc_bin,
            output_selector,
            output_index: 0,
            codec,
            chroma,
            bit_depth,
            color_range,
            color_matrix,
            transfer: arcen_media::TransferCharacteristics::Bt709,
            color_primaries: arcen_media::ColorPrimaries::Bt709,
            color_policy,
            qp_map,
            video_selection: self.video_selection,
            codec_pinned: self.codec_pinned,
            variant_pinned: self.variant_pinned,
            auth_video_request: self.auth_video_request,
            fps: self.fps,
            encoder,
            audio_enabled: self.audio_enabled,
            audio_compressed: self.audio_compressed,
            microphone_input_enabled: self.microphone_input_enabled,
            clipboard_policy: self.clipboard_policy,
            timezone_redirection: self.timezone_redirection,
            reconnect_window_secs: 0,
            qos_targets: self.qos_targets,
            deskside: self.deskside,
            iddcx: self.iddcx,
            multi_monitor: self.multi_monitor,
        };
        config.apply_software_h264_backend(encoder)?;
        Ok(config)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentStart {
    #[serde(rename = "type")]
    pub msg_type: String,
    pub peer: String,
    pub config: AgentConfig,
    pub auth_response: AuthResponse,
    pub windows_session: WindowsSessionIdentity,
    pub agent_log_path: String,
    pub session_log_id: String,
    pub log_control: AgentControl,
    #[serde(default = "default_direct_transport")]
    pub transport_capability: String,
}

impl AgentStart {
    pub const TYPE: &'static str = "windows_session_agent_start";

    pub fn new(
        peer: String,
        config: &HostConfig,
        mut auth_response: AuthResponse,
        windows_session: WindowsSessionIdentity,
        agent_log_path: String,
        session_log_id: &CorrelationId,
        log_control: AgentControl,
        transport_capability: &str,
    ) -> Self {
        auth_response.credential.clear();
        Self {
            msg_type: Self::TYPE.to_string(),
            peer,
            config: AgentConfig::from_host(config),
            auth_response,
            windows_session,
            agent_log_path,
            session_log_id: session_log_id.to_string(),
            log_control,
            transport_capability: transport_capability.to_string(),
        }
    }
}

fn default_direct_transport() -> String {
    arcen_protocol::CAPABILITY_TRANSPORT_QUIC.to_string()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentReady {
    #[serde(rename = "type")]
    pub msg_type: String,
    pub process_id: u32,
    pub windows_session: WindowsSessionIdentity,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentControl {
    #[serde(rename = "type")]
    pub msg_type: String,
    pub sequence: u64,
    pub profile_level: u8,
    #[serde(default)]
    pub qos_targets: arcen_telemetry::QosTargets,
    pub use_configured_filter: bool,
    pub reopen_generation: u64,
}

impl AgentControl {
    pub const TYPE: &'static str = "arcen_private_agent_log_control";

    pub fn new(
        sequence: u64,
        profile: arcen_telemetry::OperationalProfile,
        qos_targets: arcen_telemetry::QosTargets,
        use_configured_filter: bool,
        reopen_generation: u64,
    ) -> Self {
        Self {
            msg_type: Self::TYPE.to_string(),
            sequence,
            profile_level: profile.into(),
            qos_targets,
            use_configured_filter,
            reopen_generation,
        }
    }

    pub fn decode(text: &str) -> Result<Option<Self>, String> {
        let value: serde_json::Value = match serde_json::from_str(text) {
            Ok(value) => value,
            Err(_) => return Ok(None),
        };
        if value.get("type").and_then(serde_json::Value::as_str) != Some(Self::TYPE) {
            return Ok(None);
        }
        let control: Self = serde_json::from_value(value)
            .map_err(|error| format!("decode private agent log control: {error}"))?;
        arcen_telemetry::OperationalProfile::try_from(control.profile_level)
            .map_err(|error| format!("private agent log control: {error}"))?;
        Ok(Some(control))
    }

    pub fn is_reserved(text: &str) -> bool {
        serde_json::from_str::<serde_json::Value>(text)
            .ok()
            .and_then(|value| {
                value
                    .get("type")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
            })
            .as_deref()
            == Some(Self::TYPE)
    }
}

impl AgentReady {
    pub const TYPE: &'static str = "windows_session_agent_ready";
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentStreamingReady {
    #[serde(rename = "type")]
    pub msg_type: String,
}

impl AgentStreamingReady {
    pub const TYPE: &'static str = "arcen_private_agent_streaming_ready";

    pub fn is(text: &str) -> bool {
        serde_json::from_str::<serde_json::Value>(text)
            .ok()
            .and_then(|value| {
                value
                    .get("type")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
            })
            .as_deref()
            == Some(Self::TYPE)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentAttachmentAction {
    Detach,
    Validate,
    Attach,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentAttachmentCommand {
    #[serde(rename = "type")]
    pub msg_type: String,
    pub action: AgentAttachmentAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_log_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport_capability: Option<String>,
}

impl AgentAttachmentCommand {
    pub const TYPE: &'static str = "arcen_private_agent_attachment";

    pub fn detach() -> Self {
        Self {
            msg_type: Self::TYPE.to_string(),
            action: AgentAttachmentAction::Detach,
            session_log_id: None,
            transport_capability: None,
        }
    }

    pub fn validate(session_log_id: &CorrelationId) -> Self {
        Self {
            msg_type: Self::TYPE.to_string(),
            action: AgentAttachmentAction::Validate,
            session_log_id: Some(session_log_id.to_string()),
            transport_capability: None,
        }
    }

    pub fn attach(session_log_id: &CorrelationId, transport_capability: &str) -> Self {
        Self {
            msg_type: Self::TYPE.to_string(),
            action: AgentAttachmentAction::Attach,
            session_log_id: Some(session_log_id.to_string()),
            transport_capability: Some(transport_capability.to_string()),
        }
    }

    pub fn decode(text: &str) -> Result<Option<Self>, String> {
        let value: serde_json::Value = match serde_json::from_str(text) {
            Ok(value) => value,
            Err(_) => return Ok(None),
        };
        if value.get("type").and_then(serde_json::Value::as_str) != Some(Self::TYPE) {
            return Ok(None);
        }
        serde_json::from_value(value)
            .map(Some)
            .map_err(|error| format!("decode private agent attachment command: {error}"))
    }

    pub fn is_reserved(text: &str) -> bool {
        serde_json::from_str::<serde_json::Value>(text)
            .ok()
            .and_then(|value| {
                value
                    .get("type")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
            })
            .as_deref()
            == Some(Self::TYPE)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentAttachmentState {
    Detached,
    Validated,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentAttachmentStatus {
    #[serde(rename = "type")]
    pub msg_type: String,
    pub state: AgentAttachmentState,
    pub success: bool,
}

impl AgentAttachmentStatus {
    pub const TYPE: &'static str = "arcen_private_agent_attachment_status";

    pub fn detached() -> Self {
        Self {
            msg_type: Self::TYPE.to_string(),
            state: AgentAttachmentState::Detached,
            success: true,
        }
    }

    pub fn validated(success: bool) -> Self {
        Self {
            msg_type: Self::TYPE.to_string(),
            state: AgentAttachmentState::Validated,
            success,
        }
    }

    pub fn decode(text: &str) -> Result<Option<Self>, String> {
        let value: serde_json::Value = match serde_json::from_str(text) {
            Ok(value) => value,
            Err(_) => return Ok(None),
        };
        if value.get("type").and_then(serde_json::Value::as_str) != Some(Self::TYPE) {
            return Ok(None);
        }
        serde_json::from_value(value)
            .map(Some)
            .map_err(|error| format!("decode private agent attachment status: {error}"))
    }
}

#[cfg(test)]
mod agent_control_tests {
    use super::*;

    #[test]
    fn private_log_controls_validate_profiles_and_reserved_type() {
        let control = AgentControl::new(
            7,
            arcen_telemetry::OperationalProfile::Debug,
            arcen_telemetry::QosTargets::default(),
            false,
            3,
        );
        let text = serde_json::to_string(&control).expect("control JSON");
        assert!(AgentControl::is_reserved(&text));
        assert_eq!(AgentControl::decode(&text), Ok(Some(control)));
        assert!(AgentStreamingReady::is(
            r#"{"type":"arcen_private_agent_streaming_ready"}"#
        ));

        let invalid = r#"{"type":"arcen_private_agent_log_control","sequence":8,"profile_level":4,"use_configured_filter":true,"reopen_generation":0,"qos_targets":{}}"#;
        assert!(AgentControl::decode(invalid).is_err());
        assert_eq!(AgentControl::decode(r#"{"type":"mouse_move"}"#), Ok(None));
        let attachment = AgentAttachmentCommand::detach();
        let text = serde_json::to_string(&attachment).unwrap();
        assert_eq!(AgentAttachmentCommand::decode(&text), Ok(Some(attachment)));
        assert!(AgentAttachmentCommand::is_reserved(&text));
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SessionState {
    Active,
    Connected,
    Disconnected,
    Unsupported(i32),
}

impl SessionState {
    fn rank(self) -> Option<u8> {
        match self {
            Self::Active => Some(0),
            Self::Connected => Some(1),
            Self::Disconnected => Some(2),
            Self::Unsupported(_) => None,
        }
    }

    fn label(self) -> String {
        match self {
            Self::Active => "active".to_string(),
            Self::Connected => "connected".to_string(),
            Self::Disconnected => "disconnected".to_string(),
            Self::Unsupported(value) => format!("unsupported-{value}"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AccountMatch {
    NoUser,
    Match,
    Other,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CpBindDecision {
    Bound(u32),
    Logon(u32),
    Unlock(u32),
    /// The authenticated account's only interactive session is being displayed
    /// over a remote protocol (RDP). Move `source` onto the physical console at
    /// `target` before binding. See [`classify_console_takeover`].
    Reconnect {
        source: u32,
        target: u32,
    },
    Reject(&'static str),
}

#[derive(Clone, Debug)]
struct SessionCandidate {
    id: u32,
    user: String,
    domain: String,
    state: SessionState,
    unlocked: bool,
    protocol: Option<u16>,
}

fn ordered_supported_candidates(mut candidates: Vec<SessionCandidate>) -> Vec<SessionCandidate> {
    candidates.retain(|candidate| !candidate.user.is_empty() && candidate.state.rank().is_some());
    candidates.sort_by_key(|candidate| (candidate.state.rank().unwrap_or(u8::MAX), candidate.id));
    candidates
}

fn is_unlocked_bound_candidate(candidate: &SessionCandidate) -> bool {
    candidate.state.rank().is_some() && candidate.unlocked
}

fn is_active_resumable_candidate(candidate: &SessionCandidate) -> bool {
    candidate.state == SessionState::Active && candidate.unlocked
}

fn is_known_local_wts_extra(candidate: &SessionCandidate, relation: AccountMatch) -> bool {
    let known_local_service = candidate.id == 0
        && candidate.state == SessionState::Disconnected
        && candidate.protocol == Some(0);
    let known_listener = matches!(candidate.id, 65_536 | 65_537)
        && candidate.state == SessionState::Unsupported(6)
        && candidate.protocol == Some(0);
    // WTS_CONNECTSTATE_CLASS 7 (WTSReset) and 8 (WTSDown) are teardown states.
    // Moving a session onto the console leaves the LogonUI session it replaced
    // in exactly this state for a few seconds -- measured on pier-windows.example.internal, where
    // the vacated session 2 read `unsupported-8` immediately after
    // WTSConnectSessionW succeeded, and blocking on it defeated the whole move.
    //
    // Deliberately narrow: no user, local protocol, and only the two states
    // that mean the session is going away. WTSInit (9) is not included, because
    // a session being created could be about to become somebody's desktop, and
    // the topology log now makes that a one-line diagnosis if it ever appears.
    let teardown = candidate.protocol == Some(0)
        && matches!(candidate.state, SessionState::Unsupported(7 | 8));
    relation == AccountMatch::NoUser
        && candidate.user.is_empty()
        && (known_local_service || known_listener || teardown)
}

/// Whether this candidate is a real interactive session that is parked with no
/// client attached to it.
///
/// Measured on two Windows 11 Pro lab hosts, both of which had a user sign in
/// over RDP and then close the client:
///
/// ```text
/// ID  STATION   PROTOCOL   note
/// 1   (none)    0          the RDP user's session, now WTSDisconnected
/// 2   Console   0          LogonUI, no user; WTSGetActiveConsoleSessionId()==2
/// ```
///
/// The important number is that **PROTOCOL is 0 on the disconnected session**.
/// `WTSClientProtocolType` describes the client currently attached, so once the
/// RDP client goes away it reads as 0 exactly like the console does. It cannot
/// be used to tell "this session is remote" apart from "this session is on the
/// glass", and the only reliable discriminator is the one Windows itself uses:
/// whether the session id equals `WTSGetActiveConsoleSessionId()`.
///
/// A parked session owns no display. Windows relies on this itself — RDP will
/// happily sign a second user in while the first is disconnected — so it must
/// not block the console.
fn is_parked_session(candidate: &SessionCandidate, relation: AccountMatch) -> bool {
    candidate.state == SessionState::Disconnected
        && !candidate.user.is_empty()
        && matches!(relation, AccountMatch::Match | AccountMatch::Other)
}

/// What the broker should do about the physical console, given everything else
/// that is signed in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConsoleApproach {
    /// Nothing else holds a desktop: sign the account in at the console.
    Logon,
    /// The account already has a desktop elsewhere; move it onto the console.
    Reconnect(u32),
    /// The topology is not safe to act on.
    Blocked,
}

/// Decide how to reach the physical console when it is sitting at LogonUI.
///
/// Windows serves one interactive desktop per station and DXGI Desktop
/// Duplication only works against the console's adapter, so an account whose
/// desktop lives in another session previously got a flat rejection and no way
/// forward: it cannot move its own session from inside that session and still
/// have a route back to the machine. `WTSConnectSessionW` is the documented
/// mechanism for moving a session onto the console, and `tscon <id>
/// /dest:console` is its command-line form.
///
/// The rules are deliberately narrow:
///
/// - an **attached** session other than the console (Active or Connected) means
///   somebody is really using a desktop we did not account for, so refuse;
/// - a session whose owner could not be verified means refuse, because the
///   whole point of this classifier is to never guess whose desktop it is;
/// - the account may own at most **one** parked session, since with two there is
///   no way to tell which desktop it means; and
/// - another account's parked session is ignored when it is not the console,
///   because it holds no display.
fn classify_console_takeover(
    candidates: &[(SessionCandidate, AccountMatch)],
    active_console_id: u32,
) -> ConsoleApproach {
    let mut source = None;
    for (candidate, relation) in candidates {
        if candidate.id == active_console_id || is_known_local_wts_extra(candidate, *relation) {
            continue;
        }
        if !is_parked_session(candidate, *relation) {
            return ConsoleApproach::Blocked;
        }
        if *relation == AccountMatch::Match && source.replace(candidate.id).is_some() {
            return ConsoleApproach::Blocked;
        }
    }
    source.map_or(ConsoleApproach::Logon, ConsoleApproach::Reconnect)
}

fn classify_cp_bind(
    candidates: &[(SessionCandidate, AccountMatch)],
    active_console_id: u32,
) -> CpBindDecision {
    let mut console_candidates = candidates
        .iter()
        .filter(|(candidate, _)| candidate.id == active_console_id);
    let Some((console, console_match)) = console_candidates.next() else {
        return CpBindDecision::Reject("active console session was not enumerated");
    };
    if console_candidates.next().is_some() || console.protocol != Some(0) {
        return CpBindDecision::Reject("active console identity is ambiguous or non-console");
    }

    // Everything else on the machine, judged once. `classify_console_takeover`
    // returns `Logon` when nothing competes for the console, which is exactly
    // the condition the old `has_non_benign_extra` flag was standing in for --
    // except that it also knows a parked session competes for nothing.
    let approach = classify_console_takeover(candidates, active_console_id);

    match console_match {
        AccountMatch::NoUser => {
            if !matches!(
                console.state,
                SessionState::Active | SessionState::Connected
            ) {
                return CpBindDecision::Reject(
                    "no-user console is disconnected or in an unsupported state",
                );
            }
            match approach {
                ConsoleApproach::Logon => CpBindDecision::Logon(active_console_id),
                // The account is already signed in somewhere else. Move that
                // desktop onto the console rather than signing it in twice.
                ConsoleApproach::Reconnect(source) => CpBindDecision::Reconnect {
                    source,
                    target: active_console_id,
                },
                ConsoleApproach::Blocked => CpBindDecision::Reject(
                    "another, stale, or unverifiable interactive session exists",
                ),
            }
        }
        // The console is already ours. A second session of *ours* elsewhere is
        // genuine ambiguity -- one account, two desktops, and no way to tell
        // which one the user means -- so `Reconnect` rejects here rather than
        // moving anything.
        AccountMatch::Match if approach != ConsoleApproach::Logon => {
            CpBindDecision::Reject("authenticated account session is ambiguous")
        }
        AccountMatch::Match if console.state == SessionState::Active && console.unlocked => {
            CpBindDecision::Bound(active_console_id)
        }
        AccountMatch::Match if console.state == SessionState::Active => {
            CpBindDecision::Unlock(active_console_id)
        }
        AccountMatch::Match => {
            CpBindDecision::Reject("matching session is not the active physical console")
        }
        AccountMatch::Other if console.state == SessionState::Disconnected => match approach {
            // Only a `Reconnect` can take over another account's console.
            //
            // The tempting arm here is `Logon(active_console_id)`, and it does
            // not work: that id belongs to the *other* account's session, and
            // Windows cannot log a second account into an existing session.
            // Worse, the logon path fronts the request by firing SAS at the
            // target session (`logon_activation::activate_console`), so an
            // unsolicited Ctrl-Alt-Del would land on somebody else's console
            // and then time out waiting for a `CPUS_LOGON` provider that can
            // never arm — a lock screen offers `CPUS_UNLOCK_WORKSTATION`, and
            // only for its own owner.
            //
            // So a takeover is only reachable when the admitted account
            // already owns a parked desktop that can be *moved* onto the
            // console with the documented `WTSConnectSessionW` operation.
            // Without one there is nothing to move, and refusing is the only
            // honest answer.
            ConsoleApproach::Reconnect(source) => CpBindDecision::Reconnect {
                source,
                target: active_console_id,
            },
            ConsoleApproach::Logon => CpBindDecision::Reject(
                "another account holds the console and this account has no session to move onto it",
            ),
            ConsoleApproach::Blocked => CpBindDecision::Reject(
                "another account holds the inactive console, but another, stale, or unverifiable \
                 interactive session exists",
            ),
        },
        AccountMatch::Other if console.state == SessionState::Active => {
            CpBindDecision::Reject("another account is actively using the physical console")
        }
        AccountMatch::Other => CpBindDecision::Reject(
            "another account owns the console in a state Arcen cannot take over",
        ),
        AccountMatch::Unknown => {
            CpBindDecision::Reject("active console account could not be verified")
        }
    }
}

pub struct LaunchedAgent {
    stream: Option<PipeStream>,
    pub process_id: u32,
    pub log_path: String,
    guard: platform::AgentGuard,
    log_grant: Option<platform::SessionLogGrant>,
}

impl LaunchedAgent {
    pub fn take_stream(&mut self) -> PipeStream {
        self.stream
            .take()
            .expect("session agent IPC stream can only be taken once")
    }

    pub(crate) fn take_log_grant(&mut self) -> platform::SessionLogGrant {
        self.log_grant
            .take()
            .expect("session agent log grant can only be transferred once")
    }

    pub async fn finish(self) {
        self.guard.finish().await;
    }
}

/// How the authenticated account currently relates to the console's WTS
/// sessions. Lets the broker choose between the unchanged existing-session
/// attach, a remote first-login (`CPUS_LOGON`), or a remote unlock
/// (`CPUS_UNLOCK_WORKSTATION`) without parsing error strings.
pub enum BindStatus {
    /// A SID-matching unlocked session exists; attach to it (unchanged path).
    Bound(WindowsSessionIdentity, platform::SelectedSession),
    /// No interactive session, or none matching the account: a first-login
    /// (logon-scenario) candidate.
    NoSession(u32),
    /// A matching session exists but is locked: an unlock-scenario candidate.
    Locked(u32),
    /// The account is signed in, but its desktop is being displayed over RDP
    /// rather than on the physical console. `source` must be moved onto the
    /// console at `target` before it can be captured.
    Reconnect { source: u32, target: u32 },
    /// The console is owned by another account, remote/stale, or otherwise
    /// ambiguous. Credential Provider dispatch is forbidden.
    Rejected(&'static str),
    /// A hard error (e.g. the broker lacks the privilege to query user tokens).
    Error(String),
}

/// Classify the account against existing WTS sessions. The `Bound` arm is the
/// unchanged attach path; the other arms drive the first-login / unlock flow.
pub fn classify_bind(account: &crate::auth::AuthenticatedAccount) -> Result<BindStatus, String> {
    platform::classify_bind_session(account)
}

/// Move a session the authenticated account already owns onto the physical
/// console, and wait for Windows to finish the move.
///
/// Only ever called with a `BindStatus::Reconnect` the classifier produced, so
/// the account's ownership of `source` has already been proved by token match.
pub fn move_to_console(source: u32, target: u32) -> Result<(), String> {
    platform::move_session_to_console(source, target, CONSOLE_MOVE_TIMEOUT)
}

/// A one-line, non-sensitive description of every WTS session on the machine,
/// for the log that accompanies a bind decision.
///
/// This exists because the field report that motivated the takeover work
/// produced a single terse reason string and nothing else, which made it
/// impossible to tell which of several topologies had been rejected without
/// guessing. User and domain names are deliberately reduced to a present/absent
/// flag: the shape is what is being diagnosed, not who is signed in.
pub fn topology_summary() -> String {
    platform::describe_topology()
}

/// Revalidate the exact console/session/scenario immediately before dispatching
/// a credential to a freshly connected Credential Provider.
pub fn validate_cp_target(
    account: &crate::auth::AuthenticatedAccount,
    usage: arcen_cp_ipc::UsageScenario,
    session_id: u32,
) -> Result<(), String> {
    platform::validate_cp_target_session(account, usage, session_id)
}

/// Re-enumerate every WTS candidate during post-logon stability polling and
/// apply the same strict classifier used before CP dispatch. Transitional
/// no-session/locked states return `None`; any ambiguity is a hard error.
pub fn reclassify_expected_console(
    account: &crate::auth::AuthenticatedAccount,
    session_id: u32,
) -> Result<Option<(WindowsSessionIdentity, platform::SelectedSession)>, String> {
    platform::reclassify_expected_console_session(account, session_id)
}

pub fn spawn(
    selected: platform::SelectedSession,
    session_log_id: &CorrelationId,
    profile: arcen_telemetry::OperationalProfile,
    inherit_iddcx_control: bool,
) -> Result<LaunchedAgent, String> {
    platform::spawn_agent(selected, session_log_id, profile, inherit_iddcx_control)
}

pub(crate) fn agent_log_registration(
    selected: &platform::SelectedSession,
    session_log_id: &CorrelationId,
) -> Result<(std::path::PathBuf, String), String> {
    platform::agent_log_registration(selected, session_log_id)
}

pub(crate) fn selected_user_sid(selected: &SelectedSession) -> &str {
    platform::selected_user_sid(selected)
}

pub(crate) fn observe_bound(
    identity: &WindowsSessionIdentity,
    expected_sid: &str,
) -> Result<(), String> {
    platform::observe_bound_session(identity, expected_sid)
}

pub(crate) fn observe_resumable_bound(
    identity: &WindowsSessionIdentity,
    expected_sid: &str,
) -> Result<(), String> {
    platform::observe_resumable_bound_session(identity, expected_sid)
}

pub use platform::SelectedSession;
/// The token-bearing handle to a bound WTS session. Opaque outside this module;
/// re-exported so the broker and the first-login coordinator can name it when
/// forwarding a bound session to [`spawn`].
pub(crate) use platform::{
    grant_session_log_access, reset_session_log_access, revoke_session_log_access, SessionLogGrant,
};

#[cfg(windows)]
mod platform {
    use std::ffi::c_void;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, IntoRawHandle, OwnedHandle};
    use std::path::{Path, PathBuf};

    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::Foundation::{
        SetHandleInformation, HANDLE, HANDLE_FLAG_INHERIT, WAIT_OBJECT_0, WAIT_TIMEOUT,
    };
    use windows::Win32::Security::{
        DuplicateTokenEx, GetTokenInformation, SecurityIdentification, TokenLinkedToken,
        TokenPrimary, SECURITY_ATTRIBUTES, TOKEN_ALL_ACCESS, TOKEN_LINKED_TOKEN,
    };
    use windows::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows::Win32::System::Pipes::CreatePipe;
    use windows::Win32::System::RemoteDesktop::{
        WTSActive, WTSClientProtocolType, WTSConnected, WTSDisconnected, WTSDomainName,
        WTSEnumerateSessionsW, WTSFreeMemory, WTSGetActiveConsoleSessionId,
        WTSQuerySessionInformationW, WTSQueryUserToken, WTSSessionInfoEx, WTSUserName, WTSINFOEXW,
        WTS_CURRENT_SERVER_HANDLE, WTS_SESSIONSTATE_UNLOCK, WTS_SESSION_INFOW,
    };
    use windows::Win32::System::Threading::{
        CreateProcessAsUserW, DeleteProcThreadAttributeList, InitializeProcThreadAttributeList,
        ResumeThread, TerminateProcess, UpdateProcThreadAttribute, WaitForSingleObject,
        CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT,
        EXTENDED_STARTUPINFO_PRESENT, LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_INFORMATION,
        PROC_THREAD_ATTRIBUTE_HANDLE_LIST, STARTF_USESTDHANDLES, STARTUPINFOEXW,
    };

    use super::{
        classify_cp_bind, is_active_resumable_candidate, is_unlocked_bound_candidate,
        ordered_supported_candidates, AccountMatch, CpBindDecision, LaunchedAgent, PipeStream,
        SessionCandidate, SessionState, WindowsSessionIdentity, AGENT_SHUTDOWN_GRACE,
    };

    fn raw(handle: &OwnedHandle) -> HANDLE {
        HANDLE(handle.as_raw_handle())
    }

    fn own(handle: HANDLE) -> OwnedHandle {
        // SAFETY: callers pass a newly-created uniquely-owned handle and transfer ownership here.
        unsafe { OwnedHandle::from_raw_handle(handle.0) }
    }

    fn elevated_linked_token(token: HANDLE) -> Option<OwnedHandle> {
        let mut linked = TOKEN_LINKED_TOKEN::default();
        let mut returned = 0u32;
        // SAFETY: token is a live WTS user token; linked and returned are valid
        // out-parameters sized exactly for TOKEN_LINKED_TOKEN.
        match unsafe {
            GetTokenInformation(
                token,
                TokenLinkedToken,
                Some((&mut linked as *mut TOKEN_LINKED_TOKEN).cast()),
                std::mem::size_of::<TOKEN_LINKED_TOKEN>() as u32,
                &mut returned,
            )
        } {
            Ok(()) if !linked.LinkedToken.is_invalid() => Some(own(linked.LinkedToken)),
            Ok(()) => None,
            Err(error) => {
                tracing::debug!(
                    target: crate::logging::SESSION,
                    %error,
                    "interactive user token has no elevated linked token"
                );
                None
            }
        }
    }

    struct WtsSessions(*mut WTS_SESSION_INFOW);

    impl Drop for WtsSessions {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: WTSEnumerateSessionsW allocated this buffer and it is freed exactly once.
                unsafe { WTSFreeMemory(self.0.cast()) };
            }
        }
    }

    struct EnvironmentBlock(*mut c_void);

    impl Drop for EnvironmentBlock {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: CreateEnvironmentBlock allocated this block and it is destroyed once.
                unsafe {
                    let _ = DestroyEnvironmentBlock(self.0);
                }
            }
        }
    }

    struct AttributeList {
        raw: LPPROC_THREAD_ATTRIBUTE_LIST,
        _storage: Vec<usize>,
        // UpdateProcThreadAttribute stores the POINTER to the handle array, not
        // a copy; the kernel reads it at CreateProcess* time, so the array must
        // live as long as the initialized list.
        _handles: Box<[HANDLE]>,
    }

    impl Drop for AttributeList {
        fn drop(&mut self) {
            if !self.raw.0.is_null() {
                // SAFETY: the list was successfully initialized and remains backed by storage.
                unsafe { DeleteProcThreadAttributeList(self.raw) };
            }
        }
    }

    pub struct SelectedSession {
        identity: WindowsSessionIdentity,
        token: OwnedHandle,
        user_sid: String,
    }

    pub struct AgentGuard {
        job: OwnedHandle,
        process: OwnedHandle,
        process_id: u32,
    }

    pub struct SessionLogGrant {
        path: PathBuf,
        user_sid: String,
    }

    impl SessionLogGrant {
        fn new(path: PathBuf, user_sid: String) -> Result<Self, String> {
            grant_session_log_access(&path, &user_sid)?;
            Ok(Self { path, user_sid })
        }
    }

    impl Drop for SessionLogGrant {
        fn drop(&mut self) {
            if let Err(error) = revoke_session_log_access(&self.path, &self.user_sid) {
                tracing::warn!(
                    target: crate::logging::SESSION,
                    %error,
                    path = %self.path.display(),
                    "could not revoke session log write access"
                );
            }
        }
    }

    impl AgentGuard {
        pub async fn finish(self) {
            let deadline = tokio::time::Instant::now() + AGENT_SHUTDOWN_GRACE;
            loop {
                // SAFETY: process is a live synchronizable process handle owned by this guard.
                let status = unsafe { WaitForSingleObject(raw(&self.process), 0) };
                if status == WAIT_OBJECT_0 {
                    return;
                }
                if status != WAIT_TIMEOUT || tokio::time::Instant::now() >= deadline {
                    tracing::warn!(
                        target: crate::logging::SESSION,
                        process_id = self.process_id,
                        "session agent did not exit during cleanup grace; terminating job"
                    );
                    // SAFETY: job is a live handle owned by this guard.
                    unsafe {
                        let _ = TerminateJobObject(raw(&self.job), 1);
                    }
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        }
    }

    impl Drop for AgentGuard {
        fn drop(&mut self) {
            // SAFETY: kill-on-close is the primary containment policy; explicit termination makes
            // cleanup deterministic when a future change retains another job handle.
            unsafe {
                let _ = TerminateJobObject(raw(&self.job), 1);
            }
        }
    }

    /// The outcome of one attempt to bind the authenticated account to an
    /// existing WTS session, preserving the precedence the attach-only path has
    /// always used (locked short-circuits; token errors accumulate).
    enum BindOutcome {
        Bound(WindowsSessionIdentity, SelectedSession),
        /// Exact physical-console LogonUI with no interactive user.
        NoSession(u32),
        /// A matching session exists but is locked / lock-state unknown.
        Locked(u32),
        /// The account owns a session that is currently displayed over RDP.
        /// Move it onto the physical console, then re-bind.
        Reconnect {
            source: u32,
            target: u32,
        },
        /// No CP scenario is safe for the observed session topology.
        Rejected(&'static str),
        /// No matching token could be queried (accumulated per-session errors).
        TokenErrors(Vec<String>),
    }

    /// Single source of truth for SID-to-WTS binding. Both the attach-only path
    /// and the first-login re-bind poll go through this so they can never
    /// disagree about what counts as a match.
    fn bind_matching_session(
        account: &crate::auth::AuthenticatedAccount,
    ) -> Result<BindOutcome, String> {
        let all_candidates = enumerate_sessions()?;
        let active_console_id = unsafe { WTSGetActiveConsoleSessionId() };
        if active_console_id == u32::MAX {
            return Ok(BindOutcome::Rejected(
                "Windows did not report an active physical console",
            ));
        }
        let candidates = ordered_supported_candidates(all_candidates.clone());

        let mut token_errors = Vec::new();
        let mut relations = std::collections::HashMap::new();
        let mut matching_tokens = std::collections::HashMap::new();
        for candidate in &all_candidates {
            if candidate.user.is_empty() {
                relations.insert(candidate.id, AccountMatch::NoUser);
            }
        }
        for candidate in candidates {
            let mut token = HANDLE::default();
            // SAFETY: token is a valid out-param and candidate ID came from WTS enumeration.
            if let Err(error) = unsafe { WTSQueryUserToken(candidate.id, &mut token) } {
                token_errors.push(format!("session {}: {error}", candidate.id));
                relations.insert(candidate.id, AccountMatch::Unknown);
                continue;
            }
            let token = own(token);
            if !account.matches_token(raw(&token))? {
                relations.insert(candidate.id, AccountMatch::Other);
                continue;
            }
            relations.insert(candidate.id, AccountMatch::Match);
            matching_tokens.insert(candidate.id, token);
        }

        let observed = all_candidates
            .iter()
            .cloned()
            .map(|candidate| {
                let relation = relations
                    .get(&candidate.id)
                    .copied()
                    .unwrap_or(AccountMatch::Unknown);
                (candidate, relation)
            })
            .collect::<Vec<_>>();
        match classify_cp_bind(&observed, active_console_id) {
            CpBindDecision::Bound(id) => {
                let candidate = all_candidates
                    .into_iter()
                    .find(|candidate| candidate.id == id)
                    .ok_or_else(|| "selected console session disappeared".to_string())?;
                let token = matching_tokens
                    .remove(&id)
                    .ok_or_else(|| "selected console token disappeared".to_string())?;
                let identity = WindowsSessionIdentity {
                    session_id: candidate.id,
                    user_sid: account.string_sid()?,
                    user: candidate.user,
                    domain: candidate.domain,
                    state: candidate.state.label(),
                    launch_backend: "wts-query-user-token-create-process-as-user".to_string(),
                };
                tracing::info!(
                    target: crate::logging::SESSION,
                    requested_account = account.requested_name(),
                    windows_session_id = identity.session_id,
                    windows_user = %identity.user,
                    windows_domain = %identity.domain,
                    windows_state = %identity.state,
                    "bound authenticated account to exact active console WTS session"
                );
                Ok(BindOutcome::Bound(
                    identity.clone(),
                    SelectedSession {
                        identity,
                        token,
                        user_sid: account.string_sid()?,
                    },
                ))
            }
            CpBindDecision::Logon(id) => Ok(BindOutcome::NoSession(id)),
            CpBindDecision::Unlock(id) => Ok(BindOutcome::Locked(id)),
            CpBindDecision::Reconnect { source, target } => {
                Ok(BindOutcome::Reconnect { source, target })
            }
            CpBindDecision::Reject(_reason) if !token_errors.is_empty() => {
                Ok(BindOutcome::TokenErrors(token_errors))
            }
            CpBindDecision::Reject(reason) => Ok(BindOutcome::Rejected(reason)),
        }
    }

    /// Classify the account against existing WTS sessions (see
    /// [`super::BindStatus`]). The attach path is unchanged; the other arms let
    /// the broker drive first-login or unlock.
    pub fn classify_bind_session(
        account: &crate::auth::AuthenticatedAccount,
    ) -> Result<super::BindStatus, String> {
        Ok(match bind_matching_session(account)? {
            BindOutcome::Bound(identity, selected) => super::BindStatus::Bound(identity, selected),
            BindOutcome::NoSession(id) => super::BindStatus::NoSession(id),
            BindOutcome::Locked(id) => super::BindStatus::Locked(id),
            BindOutcome::Reconnect { source, target } => {
                super::BindStatus::Reconnect { source, target }
            }
            BindOutcome::Rejected(reason) => super::BindStatus::Rejected(reason),
            BindOutcome::TokenErrors(errors) => super::BindStatus::Error(format!(
                "The broker could not query any matching WTS user token. Run the broker as LocalSystem with SeTcbPrivilege; details: {}",
                errors.join("; ")
            )),
        })
    }

    /// A one-line, non-sensitive description of every WTS session, plus the two
    /// values Windows uses to say where the physical display is.
    ///
    /// `glass` is `GlassSessionId`, which Microsoft documents as the correct
    /// discriminator when the ordinary remote-session checks misreport under
    /// RemoteFX/vGPU — exactly the NVIDIA vGPU case this product runs on. It is
    /// recorded next to `console` so a disagreement between the two is visible
    /// rather than something to be deduced later.
    pub fn describe_topology() -> String {
        // SAFETY: WTSGetActiveConsoleSessionId takes no arguments and cannot fail.
        let console = unsafe { WTSGetActiveConsoleSessionId() };
        let glass =
            glass_session_id().map_or_else(|| "absent".to_string(), |value| value.to_string());
        let sessions = match enumerate_sessions() {
            Ok(candidates) => candidates
                .iter()
                .map(|candidate| {
                    format!(
                        "{}:{}{}{}{}",
                        candidate.id,
                        candidate.state.label(),
                        if candidate.user.is_empty() {
                            ""
                        } else {
                            "/user"
                        },
                        if candidate.unlocked {
                            "/unlocked"
                        } else {
                            "/locked"
                        },
                        candidate
                            .protocol
                            .map_or_else(|| "/proto?".to_string(), |p| format!("/proto{p}")),
                    )
                })
                .collect::<Vec<_>>()
                .join(" "),
            Err(error) => format!("<enumeration failed: {error}>"),
        };
        format!("console={console} glass={glass} sessions=[{sessions}]")
    }

    pub fn validate_cp_target_session(
        account: &crate::auth::AuthenticatedAccount,
        usage: arcen_cp_ipc::UsageScenario,
        session_id: u32,
    ) -> Result<(), String> {
        let valid = match (bind_matching_session(account)?, usage) {
            (BindOutcome::NoSession(id), arcen_cp_ipc::UsageScenario::Logon) => id == session_id,
            // A locked console may legitimately be fronted by either screen.
            // LogonUI shows the welcome / switch-user screen -- CPUS_LOGON --
            // for an account that is signed in but not unlocked, and an
            // interactive logon for an account that already has a session is
            // exactly how Windows reconnects that session. The account's
            // ownership of this console was already proved by token match, and
            // the provider derives its LSA message type from the same scenario
            // being checked here.
            //
            // The converse stays exact: a console with no session has nothing
            // to unlock, so `NoSession` above accepts `Logon` only.
            (BindOutcome::Locked(id), _) => id == session_id,
            _ => false,
        };
        valid
            .then_some(())
            .ok_or_else(|| "console/session state changed before credential dispatch".to_string())
    }

    pub fn reclassify_expected_console_session(
        account: &crate::auth::AuthenticatedAccount,
        session_id: u32,
    ) -> Result<Option<(WindowsSessionIdentity, SelectedSession)>, String> {
        match bind_matching_session(account)? {
            BindOutcome::Bound(identity, selected) if identity.session_id == session_id => {
                Ok(Some((identity, selected)))
            }
            BindOutcome::Bound(_, _) => {
                Err("strict WTS reclassification bound an unexpected session".to_string())
            }
            BindOutcome::NoSession(_) | BindOutcome::Locked(_) => Ok(None),
            // The account's desktop left the console during post-login
            // stabilisation. That is a real topology change, not a transient
            // one, so surface it instead of letting the poll spin: the caller
            // must not start capture against a console it no longer owns.
            BindOutcome::Reconnect { source, target } => Err(format!(
                "strict WTS reclassification found the account's session on {source} rather than \
                 the console {target}"
            )),
            BindOutcome::Rejected(reason) => Err(format!(
                "strict WTS reclassification rejected post-login topology: {reason}"
            )),
            BindOutcome::TokenErrors(errors) => Err(format!(
                "strict WTS reclassification could not verify every candidate: {}",
                errors.join("; ")
            )),
        }
    }

    /// The current WTS state of one session, or `None` if it cannot be read.
    ///
    /// Used only as a last-moment guard before displacing a session (see
    /// `move_session_to_console`). `None` deliberately does not block the
    /// move: the classifier has already decided, and failing to re-read is
    /// not evidence that the target became occupied. It is a check against a
    /// state *change*, not a second authority.
    fn console_state_now(session_id: u32) -> Option<SessionState> {
        enumerate_sessions()
            .ok()?
            .into_iter()
            .find(|candidate| candidate.id == session_id)
            .map(|candidate| candidate.state)
    }

    fn enumerate_sessions() -> Result<Vec<SessionCandidate>, String> {
        let mut sessions = std::ptr::null_mut();
        let mut count = 0u32;
        // SAFETY: output pointers are valid and WTS owns the returned allocation.
        unsafe {
            WTSEnumerateSessionsW(WTS_CURRENT_SERVER_HANDLE, 0, 1, &mut sessions, &mut count)
        }
        .map_err(|error| format!("WTSEnumerateSessionsW: {error}"))?;
        let owner = WtsSessions(sessions);
        // SAFETY: WTS returned count initialized entries and owner keeps the allocation live.
        let entries = unsafe { std::slice::from_raw_parts(owner.0, count as usize) };
        entries
            .iter()
            .map(|entry| {
                let state = if entry.State == WTSActive {
                    SessionState::Active
                } else if entry.State == WTSConnected {
                    SessionState::Connected
                } else if entry.State == WTSDisconnected {
                    SessionState::Disconnected
                } else {
                    SessionState::Unsupported(entry.State.0)
                };
                Ok(SessionCandidate {
                    id: entry.SessionId,
                    user: query_session_string(entry.SessionId, WTSUserName)?,
                    domain: query_session_string(entry.SessionId, WTSDomainName)?,
                    state,
                    unlocked: query_session_unlocked(entry.SessionId)?,
                    protocol: query_session_protocol(entry.SessionId).ok(),
                })
            })
            .collect()
    }

    fn query_session_protocol(session_id: u32) -> Result<u16, String> {
        let mut buffer = PWSTR::null();
        let mut bytes = 0u32;
        unsafe {
            WTSQuerySessionInformationW(
                WTS_CURRENT_SERVER_HANDLE,
                session_id,
                WTSClientProtocolType,
                &mut buffer,
                &mut bytes,
            )
        }
        .map_err(|error| format!("WTSQuerySessionInformationW({session_id}, protocol): {error}"))?;
        if buffer.is_null() || bytes < std::mem::size_of::<u16>() as u32 {
            if !buffer.is_null() {
                unsafe { WTSFreeMemory(buffer.0.cast()) };
            }
            return Err(format!(
                "WTS session {session_id} did not report a protocol"
            ));
        }
        let protocol = unsafe { *buffer.as_ptr().cast::<u16>() };
        unsafe { WTSFreeMemory(buffer.0.cast()) };
        Ok(protocol)
    }

    fn query_session_unlocked(session_id: u32) -> Result<bool, String> {
        let mut buffer = PWSTR::null();
        let mut bytes = 0u32;
        // SAFETY: output pointers are valid and the returned WTS allocation is released below.
        unsafe {
            WTSQuerySessionInformationW(
                WTS_CURRENT_SERVER_HANDLE,
                session_id,
                WTSSessionInfoEx,
                &mut buffer,
                &mut bytes,
            )
        }
        .map_err(|error| format!("WTSQuerySessionInformationW({session_id}, info-ex): {error}"))?;
        if buffer.is_null() || bytes < std::mem::size_of::<WTSINFOEXW>() as u32 {
            if !buffer.is_null() {
                // SAFETY: WTS allocated this buffer and it is freed exactly once.
                unsafe { WTSFreeMemory(buffer.0.cast()) };
            }
            return Err(format!(
                "WTS session {session_id} did not provide a complete lock-state record"
            ));
        }
        // SAFETY: the buffer is at least WTSINFOEXW-sized and remains live through the copy.
        let info = unsafe { *buffer.as_ptr().cast::<WTSINFOEXW>() };
        // SAFETY: level 1 is the documented WTSSessionInfoEx representation.
        let flags = if info.Level == 1 {
            unsafe { info.Data.WTSInfoExLevel1.SessionFlags as u32 }
        } else {
            u32::MAX
        };
        // SAFETY: WTS allocated this buffer and it is freed exactly once.
        unsafe { WTSFreeMemory(buffer.0.cast()) };
        Ok(flags == WTS_SESSIONSTATE_UNLOCK)
    }

    /// The session Windows considers to be on the physical display ("the
    /// glass").
    ///
    /// Microsoft documents that the usual remote-session checks report a
    /// *local* session when RemoteFX vGPU is involved, and gives this registry
    /// value as the correct discriminator:
    /// <https://learn.microsoft.com/en-us/windows/win32/termserv/detecting-the-terminal-services-environment>
    ///
    /// That is precisely the NVIDIA vGPU case, where the adapter can be left
    /// bound to a phantom output after a remote session ends, so this is read as
    /// a cross-check on `WTSGetActiveConsoleSessionId` rather than a
    /// replacement for it. Absent or unreadable is not an error: the value does
    /// not exist on every Windows build, and treating a missing value as a
    /// failure would break hosts that work today.
    fn glass_session_id() -> Option<u32> {
        use windows::Win32::System::Registry::{
            RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_LOCAL_MACHINE, KEY_READ,
            REG_VALUE_TYPE,
        };

        let subkey = wide(r"SYSTEM\CurrentControlSet\Control\Terminal Server");
        let value = wide("GlassSessionId");
        let mut key = HKEY::default();
        // SAFETY: both strings are NUL-terminated wide buffers that outlive the call.
        unsafe {
            RegOpenKeyExW(
                HKEY_LOCAL_MACHINE,
                PCWSTR(subkey.as_ptr()),
                0,
                KEY_READ,
                &mut key,
            )
        }
        .ok()
        .ok()?;
        let mut data = 0u32;
        let mut size = std::mem::size_of::<u32>() as u32;
        let mut kind = REG_VALUE_TYPE::default();
        // SAFETY: data and size describe a live u32 buffer for the duration of the call.
        let status = unsafe {
            RegQueryValueExW(
                key,
                PCWSTR(value.as_ptr()),
                None,
                Some(&mut kind),
                Some(std::ptr::from_mut(&mut data).cast::<u8>()),
                Some(&mut size),
            )
        };
        // SAFETY: key was opened above and is closed exactly once.
        unsafe {
            let _ = RegCloseKey(key);
        }
        status.is_ok().then_some(data)
    }

    /// Move `source` onto the physical console at `target` and wait for Windows
    /// to finish the switch.
    ///
    /// `WTSConnectSessionW` reroutes a session's output to another station; with
    /// the console as the target this is the same operation as `tscon <source>
    /// /dest:console`. The session id does not change — only the station it is
    /// displayed on — so after this returns, the ordinary console detection
    /// (`WTSGetActiveConsoleSessionId`) sees the account's own session and the
    /// unchanged capture pipeline can bind it.
    ///
    /// <https://learn.microsoft.com/en-us/windows/win32/api/wtsapi32/nf-wtsapi32-wtsconnectsessionw>
    ///
    /// The password argument is an empty string: the broker runs as LocalSystem,
    /// which holds Connect permission on every local session. If a Windows build
    /// ever refuses that, the call fails and the caller falls back to the
    /// previous rejection — it must never silently proceed as though the move
    /// had happened.
    pub fn move_session_to_console(
        source: u32,
        target: u32,
        timeout: std::time::Duration,
    ) -> Result<(), String> {
        use windows::Win32::System::RemoteDesktop::WTSConnectSessionW;

        if source == target {
            return Err(format!(
                "session {source} is already the active console session"
            ));
        }
        // Re-read the target immediately before displacing it.
        //
        // The classifier's answer is a snapshot, and between taking it and
        // arriving here a human can sit down at the glass or an RDP client can
        // reconnect — either of which turns the target `Active`. Acting on the
        // stale snapshot would displace a session somebody is using, which is
        // the one guarantee this policy makes. The logon path already
        // re-checks the console three times around its dispatch for the same
        // reason; this move had no guard at all.
        //
        // Narrow on purpose: only an occupied *target* is rejected here. The
        // classifier remains the authority on everything else.
        if let Some(state) = console_state_now(target) {
            if state == SessionState::Active {
                return Err(format!(
                    "session {target} became active before the move; refusing to displace it"
                ));
            }
        }
        let mut password = [0u16; 1];
        // SAFETY: password is a NUL-terminated wide buffer that outlives the call, and
        // both session ids came from this process's own WTS enumeration.
        let connected = unsafe {
            WTSConnectSessionW(source, target, PWSTR(password.as_mut_ptr()), true).is_ok()
        };
        if !connected {
            return Err(format!(
                "WTSConnectSessionW({source} -> console {target}) failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        // WTSConnectSessionW returning success means the switch was accepted,
        // not that the console has finished moving. Poll the fact the binder
        // will actually use, so a caller can never act on a half-applied move.
        //
        // Only the console id is checked. `WTSClientProtocolType` is not: it was
        // measured to read 0 on a disconnected RDP session, so it cannot
        // distinguish "arrived at the glass" from "still parked".
        let deadline = std::time::Instant::now() + timeout;
        loop {
            // SAFETY: WTSGetActiveConsoleSessionId takes no arguments and cannot fail.
            let console = unsafe { WTSGetActiveConsoleSessionId() };
            if console == source {
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                return Err(format!(
                    "session {source} did not reach the physical console within {}s: \
                     the active console is still {console}",
                    timeout.as_secs()
                ));
            }
            std::thread::sleep(super::CONSOLE_MOVE_POLL_INTERVAL);
        }
    }

    fn query_session_string(
        session_id: u32,
        info_class: windows::Win32::System::RemoteDesktop::WTS_INFO_CLASS,
    ) -> Result<String, String> {
        let mut buffer = PWSTR::null();
        let mut bytes = 0u32;
        // SAFETY: output pointers are valid and the returned allocation is released below.
        unsafe {
            WTSQuerySessionInformationW(
                WTS_CURRENT_SERVER_HANDLE,
                session_id,
                info_class,
                &mut buffer,
                &mut bytes,
            )
        }
        .map_err(|error| format!("WTSQuerySessionInformationW({session_id}): {error}"))?;
        let result = if buffer.is_null() || bytes < 2 {
            String::new()
        } else {
            let units = bytes as usize / 2;
            // SAFETY: WTS returned a UTF-16 buffer of `bytes`; trim its trailing NUL.
            let value = unsafe { std::slice::from_raw_parts(buffer.as_ptr(), units) };
            String::from_utf16_lossy(value.strip_suffix(&[0]).unwrap_or(value))
        };
        if !buffer.is_null() {
            // SAFETY: WTS allocated this buffer and it is freed exactly once.
            unsafe { WTSFreeMemory(buffer.0.cast()) };
        }
        Ok(result)
    }

    pub fn spawn_agent(
        selected: SelectedSession,
        session_log_id: &arcen_telemetry::CorrelationId,
        profile: arcen_telemetry::OperationalProfile,
        inherit_iddcx_control: bool,
    ) -> Result<LaunchedAgent, String> {
        observe_session(&selected.identity)?;
        // WTSQueryUserToken returns the interactive user's filtered token under
        // UAC. NvAPI EDID mutation requires the authenticated administrator's
        // elevated linked token, while ordinary users legitimately have no
        // linked token and continue with their original session token.
        let elevated = elevated_linked_token(raw(&selected.token));
        let source_token = elevated
            .as_ref()
            .map(raw)
            .unwrap_or_else(|| raw(&selected.token));
        tracing::info!(
            target: crate::logging::SESSION,
            windows_session_id = selected.identity.session_id,
            elevated_linked_token = elevated.is_some(),
            "selected token for per-session host agent"
        );
        let mut primary = HANDLE::default();
        // SAFETY: selected token is valid; output is initialized by DuplicateTokenEx.
        unsafe {
            DuplicateTokenEx(
                source_token,
                TOKEN_ALL_ACCESS,
                None,
                SecurityIdentification,
                TokenPrimary,
                &mut primary,
            )
        }
        .map_err(|error| format!("DuplicateTokenEx(WTS user): {error}"))?;
        let primary = own(primary);

        let mut environment = std::ptr::null_mut();
        // SAFETY: primary is a valid user primary token and environment is a valid out-param.
        unsafe { CreateEnvironmentBlock(&mut environment, raw(&primary), false) }
            .map_err(|error| format!("CreateEnvironmentBlock: {error}"))?;
        let environment = EnvironmentBlock(environment);

        let security = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            bInheritHandle: true.into(),
            ..Default::default()
        };
        let (child_read, parent_write) = create_pipe_pair(&security)?;
        let (parent_read, child_write) = create_pipe_pair(&security)?;
        // SAFETY: parent endpoints are valid handles; clearing inheritance keeps them broker-only.
        unsafe {
            SetHandleInformation(
                raw(&parent_write),
                HANDLE_FLAG_INHERIT.0,
                Default::default(),
            )
            .map_err(|error| format!("make broker IPC write handle private: {error}"))?;
            SetHandleInformation(raw(&parent_read), HANDLE_FLAG_INHERIT.0, Default::default())
                .map_err(|error| format!("make broker IPC read handle private: {error}"))?;
        }

        let executable = std::env::current_exe()
            .map_err(|error| format!("resolve session agent executable: {error}"))?;
        let executable_text = executable
            .to_str()
            .ok_or_else(|| "session agent executable path is not valid UTF-8".to_string())?;
        let (log_path, log_user_sid) = agent_log_registration(&selected, session_log_id)?;
        let log_directory = log_path
            .parent()
            .ok_or_else(|| format!("session log has no parent: {}", log_path.display()))?;
        std::fs::create_dir_all(&log_directory)
            .map_err(|error| format!("create {}: {error}", log_directory.display()))?;
        let log_file = crate::service::open_shared_append(&log_path)?;
        let log_grant = SessionLogGrant::new(log_path.clone(), log_user_sid.clone())?;
        let log_handle = HANDLE(log_file.as_raw_handle());
        // SAFETY: log_file uniquely owns a valid file handle. Marking only this handle inheritable
        // lets STARTF_USESTDHANDLES provide durable child diagnostics without exposing other files.
        unsafe { SetHandleInformation(log_handle, HANDLE_FLAG_INHERIT.0, HANDLE_FLAG_INHERIT) }
            .map_err(|error| format!("make session agent log handle inheritable: {error}"))?;
        let iddcx_file = if inherit_iddcx_control {
            Some(crate::iddcx::open_inheritable_control_file()?)
        } else {
            None
        };
        let iddcx_handle = iddcx_file.as_ref().map(|file| HANDLE(file.as_raw_handle()));
        if let Some(handle) = iddcx_handle {
            // SAFETY: the broker owns this valid device handle until process
            // creation completes and explicitly opts only this handle in.
            unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT.0, HANDLE_FLAG_INHERIT) }
                .map_err(|error| format!("make IddCx control handle inheritable: {error}"))?;
        }
        let mut inherited_handles = vec![raw(&child_read), raw(&child_write), log_handle];
        if let Some(handle) = iddcx_handle {
            inherited_handles.push(handle);
        }
        let attribute_list = inherited_handle_list(inherited_handles)?;
        let iddcx_argument = iddcx_handle.map_or_else(String::new, |handle| {
            format!(" --iddcx-control {}", handle.0 as isize)
        });
        let command_line = format!(
            "{} session-agent --ipc-read {} --ipc-write {} --session-log-id {} \
             --log-path {} --profile {}{}",
            quote_windows_argument(executable_text),
            raw(&child_read).0 as isize,
            raw(&child_write).0 as isize,
            session_log_id,
            quote_windows_argument(
                log_path
                    .to_str()
                    .ok_or_else(|| "session agent log path is not valid UTF-8".to_string())?
            ),
            u8::from(profile),
            iddcx_argument,
        );
        let executable_wide = wide(executable_text);
        let mut command_wide = wide(&command_line);
        let current_dir = executable.parent().unwrap_or_else(|| Path::new("."));
        let current_dir_wide = wide(
            current_dir
                .to_str()
                .ok_or_else(|| "session agent directory is not valid UTF-8".to_string())?,
        );
        let mut desktop = wide(r"winsta0\default");
        let startup = STARTUPINFOEXW {
            StartupInfo: windows::Win32::System::Threading::STARTUPINFOW {
                cb: std::mem::size_of::<STARTUPINFOEXW>() as u32,
                lpDesktop: PWSTR(desktop.as_mut_ptr()),
                dwFlags: STARTF_USESTDHANDLES,
                hStdInput: raw(&child_read),
                hStdOutput: log_handle,
                hStdError: log_handle,
                ..Default::default()
            },
            lpAttributeList: attribute_list.raw,
        };
        let job = create_kill_on_close_job()?;
        let mut process_info = PROCESS_INFORMATION::default();
        // SAFETY: all buffers are initialized, mutable command storage is NUL-terminated, only the
        // handles named in the attribute list are inherited, and environment remains live.
        unsafe {
            CreateProcessAsUserW(
                raw(&primary),
                PCWSTR(executable_wide.as_ptr()),
                PWSTR(command_wide.as_mut_ptr()),
                None,
                None,
                true,
                CREATE_UNICODE_ENVIRONMENT
                    | CREATE_SUSPENDED
                    | CREATE_NO_WINDOW
                    | EXTENDED_STARTUPINFO_PRESENT,
                Some(environment.0),
                PCWSTR(current_dir_wide.as_ptr()),
                &startup.StartupInfo,
                &mut process_info,
            )
        }
        .map_err(|error| format!("CreateProcessAsUserW(session agent): {error}"))?;
        let process = own(process_info.hProcess);
        let thread = own(process_info.hThread);
        // SAFETY: both job and process handles are valid; assignment happens before resume.
        if let Err(error) = unsafe { AssignProcessToJobObject(raw(&job), raw(&process)) } {
            // SAFETY: the process is still suspended and has not entered the containment job.
            // Terminating it here closes the only post-creation/pre-job orphan window.
            unsafe {
                let _ = TerminateProcess(raw(&process), 1);
                let _ = WaitForSingleObject(raw(&process), 5_000);
            }
            return Err(format!("AssignProcessToJobObject(session agent): {error}"));
        }
        let guard = AgentGuard {
            job,
            process,
            process_id: process_info.dwProcessId,
        };
        // SAFETY: the primary thread is suspended and valid. u32::MAX indicates failure.
        if unsafe { ResumeThread(raw(&thread)) } == u32::MAX {
            return Err(format!(
                "ResumeThread(session agent): {}",
                std::io::Error::last_os_error()
            ));
        }
        drop(thread);
        drop(child_read);
        drop(child_write);
        drop(log_file);

        let parent_read = into_file(parent_read);
        let parent_write = into_file(parent_write);
        let log_path = log_path.to_string_lossy().into_owned();
        tracing::info!(
            target: crate::logging::SESSION,
            process_id = process_info.dwProcessId,
            windows_session_id = selected.identity.session_id,
            windows_user = %selected.identity.user,
            windows_domain = %selected.identity.domain,
            desktop = r"winsta0\default",
            agent_log = %log_path,
            "launched per-session host agent"
        );
        Ok(LaunchedAgent {
            stream: Some(PipeStream::new(parent_read, parent_write)),
            process_id: process_info.dwProcessId,
            log_path,
            guard,
            log_grant: Some(log_grant),
        })
    }

    pub fn agent_log_registration(
        selected: &SelectedSession,
        session_log_id: &arcen_telemetry::CorrelationId,
    ) -> Result<(PathBuf, String), String> {
        Ok((
            crate::paths::sessions_log_dir()
                .join(format!("arcen-session-agent-{session_log_id}.log")),
            selected.user_sid.clone(),
        ))
    }

    pub fn grant_session_log_access(path: &Path, user_sid: &str) -> Result<(), String> {
        // The session agent reopens this file under its own (impersonated,
        // non-admin) token to attach structured tracing, using an access mask
        // that includes READ_CONTROL (Rust's append-mode open requests the
        // FILE_GENERIC_WRITE-derived mask, which folds in STANDARD_RIGHTS_WRITE
        // == READ_CONTROL). The bare "(W)" simple permission excludes
        // READ_CONTROL, so the reopen fails with access denied even though the
        // grant itself reports success; "(RC)" closes that gap.
        let file_arguments = [
            std::ffi::OsString::from("/grant:r"),
            std::ffi::OsString::from(format!("*{user_sid}:(W,RC)")),
        ];
        // The sessions directory is locked to Administrators/SYSTEM only, and
        // NTFS "bypass traverse checking" does not rescue the session agent's
        // own CreateFile on its immediate containing directory in this
        // environment; without an explicit, scoped grant there the reopen
        // fails with access denied regardless of the file's own ACL. Grant
        // list/traverse only (no container-inherit flags), so this does not
        // extend to other sessions' log files, whose own ACLs remain the sole
        // gate on content access.
        if let Some(parent) = path.parent() {
            let directory_arguments = [
                std::ffi::OsString::from("/grant:r"),
                std::ffi::OsString::from(format!("*{user_sid}:(RX)")),
            ];
            run_icacls(
                parent,
                &directory_arguments,
                "grant session log directory traversal access",
            )?;
        }
        run_icacls(path, &file_arguments, "grant session log write access")
    }

    pub fn revoke_session_log_access(path: &Path, user_sid: &str) -> Result<(), String> {
        let arguments = [
            std::ffi::OsString::from("/remove:g"),
            std::ffi::OsString::from(format!("*{user_sid}")),
        ];
        let file_result = run_icacls(path, &arguments, "revoke session log write access");
        let directory_result = path.parent().map(|parent| {
            let directory_arguments = [
                std::ffi::OsString::from("/remove:g"),
                std::ffi::OsString::from(format!("*{user_sid}")),
            ];
            run_icacls(
                parent,
                &directory_arguments,
                "revoke session log directory traversal access",
            )
        });
        match (file_result, directory_result) {
            (Err(error), _) => Err(error),
            (Ok(()), Some(Err(error))) => Err(error),
            (Ok(()), _) => Ok(()),
        }
    }

    pub fn reset_session_log_access(path: &Path) -> Result<(), String> {
        run_icacls(
            path,
            &[std::ffi::OsString::from("/reset")],
            "reset inactive session log access",
        )
    }

    fn run_icacls(
        path: &Path,
        arguments: &[std::ffi::OsString],
        operation: &str,
    ) -> Result<(), String> {
        let system_root = std::env::var_os("SystemRoot")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
        let output = std::process::Command::new(system_root.join(r"System32\icacls.exe"))
            .arg(path)
            .args(arguments)
            .arg("/q")
            .output()
            .map_err(|error| format!("{operation} for {}: {error}", path.display()))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(format!(
                "{operation} for {} failed with {}: {}",
                path.display(),
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ))
        }
    }

    fn create_pipe_pair(
        security: &SECURITY_ATTRIBUTES,
    ) -> Result<(OwnedHandle, OwnedHandle), String> {
        let mut read = HANDLE::default();
        let mut write = HANDLE::default();
        // SAFETY: both outputs are valid and security lives for the synchronous call.
        unsafe { CreatePipe(&mut read, &mut write, Some(security), 1024 * 1024) }
            .map_err(|error| format!("CreatePipe(session agent IPC): {error}"))?;
        Ok((own(read), own(write)))
    }

    fn into_file(handle: OwnedHandle) -> std::fs::File {
        let raw = handle.into_raw_handle();
        // SAFETY: ownership was transferred from OwnedHandle and is consumed exactly once.
        unsafe { std::fs::File::from_raw_handle(raw) }
    }

    fn inherited_handle_list(handles: Vec<HANDLE>) -> Result<AttributeList, String> {
        let handles: Box<[HANDLE]> = handles.into_boxed_slice();
        let mut bytes = 0usize;
        // SAFETY: documented sizing call writes the required allocation size.
        let _ = unsafe {
            InitializeProcThreadAttributeList(
                LPPROC_THREAD_ATTRIBUTE_LIST(std::ptr::null_mut()),
                1,
                0,
                &mut bytes,
            )
        };
        if bytes == 0 {
            return Err(format!(
                "size process attribute list: {}",
                std::io::Error::last_os_error()
            ));
        }
        let words = bytes.div_ceil(std::mem::size_of::<usize>());
        let mut storage = vec![0usize; words];
        let raw_list = LPPROC_THREAD_ATTRIBUTE_LIST(storage.as_mut_ptr().cast());
        // SAFETY: storage has the exact size requested and remains live in AttributeList.
        unsafe { InitializeProcThreadAttributeList(raw_list, 1, 0, &mut bytes) }
            .map_err(|error| format!("initialize process attribute list: {error}"))?;
        let list = AttributeList {
            raw: raw_list,
            _storage: storage,
            _handles: handles,
        };
        // SAFETY: list is initialized; the handle array is owned by `list` (Box
        // heap memory is address-stable across moves) and outlives the list.
        unsafe {
            UpdateProcThreadAttribute(
                list.raw,
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                Some(list._handles.as_ptr().cast()),
                std::mem::size_of_val(&*list._handles),
                None,
                None,
            )
        }
        .map_err(|error| format!("set inherited IPC handle list: {error}"))?;
        Ok(list)
    }

    fn create_kill_on_close_job() -> Result<OwnedHandle, String> {
        // SAFETY: no security attributes or global name are supplied.
        let job = unsafe { CreateJobObjectW(None, PCWSTR::null()) }
            .map(own)
            .map_err(|error| format!("CreateJobObjectW(session agent): {error}"))?;
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags =
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_BREAKAWAY_OK;
        // SAFETY: limits is initialized and its exact size is supplied.
        unsafe {
            SetInformationJobObject(
                raw(&job),
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                std::mem::size_of_val(&limits) as u32,
            )
        }
        .map_err(|error| format!("SetInformationJobObject(kill-on-close): {error}"))?;
        Ok(job)
    }

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain([0]).collect()
    }

    fn quote_windows_argument(argument: &str) -> String {
        if !argument
            .chars()
            .any(|character| character.is_whitespace() || character == '"')
        {
            return argument.to_string();
        }
        let mut quoted = String::from("\"");
        let mut backslashes = 0usize;
        for character in argument.chars() {
            match character {
                '\\' => backslashes += 1,
                '"' => {
                    quoted.push_str(&"\\".repeat(backslashes * 2 + 1));
                    quoted.push('"');
                    backslashes = 0;
                }
                _ => {
                    quoted.push_str(&"\\".repeat(backslashes));
                    backslashes = 0;
                    quoted.push(character);
                }
            }
        }
        quoted.push_str(&"\\".repeat(backslashes * 2));
        quoted.push('"');
        quoted
    }

    pub fn observe_session(identity: &WindowsSessionIdentity) -> Result<(), String> {
        let user = query_session_string(identity.session_id, WTSUserName)?;
        let domain = query_session_string(identity.session_id, WTSDomainName)?;
        if user.is_empty() {
            return Err(format!(
                "Windows session {} logged off",
                identity.session_id
            ));
        }
        if !user.eq_ignore_ascii_case(&identity.user)
            || !domain.eq_ignore_ascii_case(&identity.domain)
        {
            return Err(format!(
                "Windows session {} changed identity from {} to {}",
                identity.session_id,
                identity.account_name(),
                if domain.is_empty() {
                    user
                } else {
                    format!(r"{domain}\{user}")
                }
            ));
        }
        if !query_session_unlocked(identity.session_id)? {
            return Err(format!(
                "Windows session {} locked; secure-desktop capture and input are unsupported",
                identity.session_id
            ));
        }
        Ok(())
    }

    pub fn selected_user_sid(selected: &SelectedSession) -> &str {
        &selected.user_sid
    }

    pub fn observe_bound_session(
        identity: &WindowsSessionIdentity,
        expected_sid: &str,
    ) -> Result<(), String> {
        observe_session(identity)?;
        let bound = enumerate_sessions()?
            .into_iter()
            .find(|candidate| candidate.id == identity.session_id)
            .is_some_and(|candidate| is_unlocked_bound_candidate(&candidate));
        if !bound {
            return Err("Windows session is no longer connected and unlocked".to_string());
        }
        observe_bound_sid(identity, expected_sid)
    }

    pub fn observe_resumable_bound_session(
        identity: &WindowsSessionIdentity,
        expected_sid: &str,
    ) -> Result<(), String> {
        observe_session(identity)?;
        let active = enumerate_sessions()?
            .into_iter()
            .find(|candidate| candidate.id == identity.session_id)
            .is_some_and(|candidate| is_active_resumable_candidate(&candidate));
        if !active {
            return Err("Windows resumable session is no longer active and unlocked".to_string());
        }
        observe_bound_sid(identity, expected_sid)
    }

    fn observe_bound_sid(
        identity: &WindowsSessionIdentity,
        expected_sid: &str,
    ) -> Result<(), String> {
        let mut token = HANDLE::default();
        // SAFETY: token is a valid out-param and the id is the observed WTS session.
        unsafe { WTSQueryUserToken(identity.session_id, &mut token) }
            .map_err(|error| format!("WTSQueryUserToken({}): {error}", identity.session_id))?;
        let token = own(token);
        let sid = crate::auth::token_string_sid(raw(&token))?;
        if !sid.eq_ignore_ascii_case(expected_sid) {
            return Err("Windows session SID changed".to_string());
        }
        Ok(())
    }

    #[cfg(test)]
    mod native_tests {
        use super::*;

        #[test]
        fn no_user_console_snapshot_is_local_and_logon_eligible() {
            let console_id = unsafe { WTSGetActiveConsoleSessionId() };
            assert_ne!(console_id, u32::MAX, "no active physical console");
            let candidates = enumerate_sessions().expect("enumerate WTS sessions");
            for candidate in &candidates {
                eprintln!(
                    "WTS_CANDIDATE session_id={} state={} protocol={:?} user_present={}",
                    candidate.id,
                    candidate.state.label(),
                    candidate.protocol,
                    !candidate.user.is_empty()
                );
            }
            let console = candidates
                .iter()
                .find(|candidate| candidate.id == console_id)
                .expect("active console must be present in WTS enumeration");
            // A hosted GitHub Actions runner always has an interactive session,
            // so this harness cannot run there. Skip rather than fail: the
            // assertions below describe the pre-login LogonUI console, and on a
            // machine that has already signed a user in there is nothing
            // meaningful left to check. The reason is printed so a skip on real
            // hardware — where this is expected to run — is visible in the log
            // instead of looking like a pass.
            if !console.user.is_empty() {
                eprintln!(
                    "SKIP[interactive_console_signed_in]: no-user LogonUI harness requires the active console before interactive sign-in"
                );
                return;
            }
            // The synthetic relations below label every session that has a user
            // as `Unknown`, because this harness has no account to match tokens
            // against. An unverifiable session correctly blocks the console, so
            // on a machine where anyone has signed in and disconnected — which
            // is the normal state of a lab host after any RDP use — the
            // assertion below cannot hold and says nothing about the code.
            // Guard on the real precondition rather than only on the console,
            // and print it, so this reads as a skip and not a silent pass.
            if candidates
                .iter()
                .any(|candidate| candidate.id != console_id && !candidate.user.is_empty())
            {
                eprintln!(
                    "SKIP[other_session_present]: no-user LogonUI harness requires no other session with a signed-in user"
                );
                return;
            }
            assert_eq!(
                console.protocol,
                Some(0),
                "active console must use the local WTS protocol"
            );
            assert!(
                matches!(
                    console.state,
                    SessionState::Active | SessionState::Connected
                ),
                "no-user LogonUI console must be active or connected"
            );

            let observed = candidates
                .into_iter()
                .filter(|candidate| candidate.state.rank().is_some())
                .map(|candidate| {
                    let relation = if candidate.user.is_empty() {
                        AccountMatch::NoUser
                    } else {
                        AccountMatch::Unknown
                    };
                    (candidate, relation)
                })
                .collect::<Vec<_>>();
            assert_eq!(
                classify_cp_bind(&observed, console_id),
                CpBindDecision::Logon(console_id)
            );
        }

        #[test]
        fn native_identityless_extras_match_only_exact_service_and_listener_whitelist() {
            let console_id = unsafe { WTSGetActiveConsoleSessionId() };
            let candidates = enumerate_sessions().expect("enumerate native WTS sessions");
            for candidate in candidates
                .iter()
                .filter(|candidate| candidate.id != console_id && candidate.user.is_empty())
            {
                let benign =
                    super::super::is_known_local_wts_extra(candidate, AccountMatch::NoUser);
                assert_eq!(
                    benign,
                    matches!(candidate.id, 0 | 65_536 | 65_537)
                        || matches!(candidate.state, SessionState::Unsupported(7 | 8)),
                    "unexpected identityless WTS candidate: {candidate:?}"
                );
            }
        }
    }
}

#[cfg(not(windows))]
mod platform {
    use super::{LaunchedAgent, WindowsSessionIdentity};

    pub struct SelectedSession;

    pub struct AgentGuard;
    pub struct SessionLogGrant;

    impl AgentGuard {
        pub async fn finish(self) {}
    }

    pub fn classify_bind_session(
        _account: &crate::auth::AuthenticatedAccount,
    ) -> Result<super::BindStatus, String> {
        Err("Windows WTS sessions are unavailable on this platform".to_string())
    }

    pub fn move_session_to_console(
        _source: u32,
        _target: u32,
        _timeout: std::time::Duration,
    ) -> Result<(), String> {
        Err("Windows WTS sessions are unavailable on this platform".to_string())
    }

    pub fn describe_topology() -> String {
        "console=unavailable glass=unavailable sessions=[]".to_string()
    }

    pub fn validate_cp_target_session(
        _account: &crate::auth::AuthenticatedAccount,
        _usage: arcen_cp_ipc::UsageScenario,
        _session_id: u32,
    ) -> Result<(), String> {
        Err("Windows WTS sessions are unavailable on this platform".to_string())
    }

    pub fn reclassify_expected_console_session(
        _account: &crate::auth::AuthenticatedAccount,
        _session_id: u32,
    ) -> Result<Option<(WindowsSessionIdentity, SelectedSession)>, String> {
        Err("Windows WTS sessions are unavailable on this platform".to_string())
    }

    pub fn spawn_agent(
        _selected: SelectedSession,
        _session_log_id: &arcen_telemetry::CorrelationId,
        _profile: arcen_telemetry::OperationalProfile,
        _inherit_iddcx_control: bool,
    ) -> Result<LaunchedAgent, String> {
        Err("Windows session agents are unavailable on this platform".to_string())
    }

    pub fn agent_log_registration(
        _selected: &SelectedSession,
        _session_log_id: &arcen_telemetry::CorrelationId,
    ) -> Result<(std::path::PathBuf, String), String> {
        Err("Windows session log registration is unavailable on this platform".to_string())
    }

    pub fn grant_session_log_access(
        _path: &std::path::Path,
        _user_sid: &str,
    ) -> Result<(), String> {
        Err("Windows session log ACLs are unavailable on this platform".to_string())
    }

    pub fn revoke_session_log_access(
        _path: &std::path::Path,
        _user_sid: &str,
    ) -> Result<(), String> {
        Err("Windows session log ACLs are unavailable on this platform".to_string())
    }

    pub fn reset_session_log_access(_path: &std::path::Path) -> Result<(), String> {
        Err("Windows session log ACLs are unavailable on this platform".to_string())
    }

    pub fn observe_session(_identity: &WindowsSessionIdentity) -> Result<(), String> {
        Err("Windows WTS sessions are unavailable on this platform".to_string())
    }

    pub fn selected_user_sid(_selected: &SelectedSession) -> &str {
        "S-1-0-0"
    }

    pub fn observe_bound_session(
        _identity: &WindowsSessionIdentity,
        _expected_sid: &str,
    ) -> Result<(), String> {
        Err("Windows WTS sessions are unavailable on this platform".to_string())
    }

    pub fn observe_resumable_bound_session(
        _identity: &WindowsSessionIdentity,
        _expected_sid: &str,
    ) -> Result<(), String> {
        Err("Windows WTS sessions are unavailable on this platform".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attachment_command_round_trips_quic_transport() {
        let sid = CorrelationId::from_uuid_v4_bytes([7; 16]);
        let command =
            AgentAttachmentCommand::attach(&sid, arcen_protocol::CAPABILITY_TRANSPORT_QUIC);
        let encoded = serde_json::to_string(&command).unwrap();
        let decoded = AgentAttachmentCommand::decode(&encoded).unwrap().unwrap();
        assert_eq!(decoded, command);
        assert_eq!(
            decoded.transport_capability.as_deref(),
            Some(arcen_protocol::CAPABILITY_TRANSPORT_QUIC)
        );
    }

    #[test]
    fn session_selection_prefers_active_then_connected_then_disconnected() {
        let ordered = ordered_supported_candidates(vec![
            SessionCandidate {
                id: 8,
                user: "artist".to_string(),
                domain: "STUDIO".to_string(),
                state: SessionState::Disconnected,
                unlocked: true,
                protocol: Some(0),
            },
            SessionCandidate {
                id: 4,
                user: "artist".to_string(),
                domain: "STUDIO".to_string(),
                state: SessionState::Active,
                unlocked: true,
                protocol: Some(0),
            },
            SessionCandidate {
                id: 2,
                user: "artist".to_string(),
                domain: "STUDIO".to_string(),
                state: SessionState::Connected,
                unlocked: true,
                protocol: Some(0),
            },
        ]);
        assert_eq!(
            ordered
                .iter()
                .map(|candidate| candidate.id)
                .collect::<Vec<_>>(),
            vec![4, 2, 8]
        );
    }

    #[test]
    fn unsupported_and_identityless_sessions_are_filtered_but_locked_identity_is_retained() {
        let ordered = ordered_supported_candidates(vec![
            SessionCandidate {
                id: 1,
                user: String::new(),
                domain: String::new(),
                state: SessionState::Active,
                unlocked: true,
                protocol: Some(0),
            },
            SessionCandidate {
                id: 2,
                user: "system".to_string(),
                domain: "NT AUTHORITY".to_string(),
                state: SessionState::Unsupported(6),
                unlocked: true,
                protocol: Some(0),
            },
            SessionCandidate {
                id: 3,
                user: "locked".to_string(),
                domain: "STUDIO".to_string(),
                state: SessionState::Active,
                unlocked: false,
                protocol: Some(0),
            },
        ]);
        assert_eq!(ordered.len(), 1);
        assert_eq!(ordered[0].id, 3);
        assert!(!ordered[0].unlocked);
    }

    #[test]
    fn ordinary_observation_accepts_unlocked_connected_and_disconnected_sessions() {
        for state in [
            SessionState::Active,
            SessionState::Connected,
            SessionState::Disconnected,
        ] {
            let candidate = SessionCandidate {
                id: 7,
                user: "artist".to_string(),
                domain: "STUDIO".to_string(),
                state,
                unlocked: true,
                protocol: Some(0),
            };
            assert!(is_unlocked_bound_candidate(&candidate));
            assert_eq!(
                is_active_resumable_candidate(&candidate),
                state == SessionState::Active
            );
        }
    }

    #[test]
    fn locked_or_unsupported_sessions_fail_both_observation_modes() {
        for candidate in [
            SessionCandidate {
                id: 7,
                user: "artist".to_string(),
                domain: "STUDIO".to_string(),
                state: SessionState::Connected,
                unlocked: false,
                protocol: Some(0),
            },
            SessionCandidate {
                id: 7,
                user: "artist".to_string(),
                domain: "STUDIO".to_string(),
                state: SessionState::Unsupported(6),
                unlocked: true,
                protocol: Some(0),
            },
        ] {
            assert!(!is_unlocked_bound_candidate(&candidate));
            assert!(!is_active_resumable_candidate(&candidate));
        }
    }

    fn cp_candidate(
        id: u32,
        user: &str,
        state: SessionState,
        unlocked: bool,
        protocol: Option<u16>,
        relation: AccountMatch,
    ) -> (SessionCandidate, AccountMatch) {
        (
            SessionCandidate {
                id,
                user: user.to_string(),
                domain: if user.is_empty() {
                    String::new()
                } else {
                    "STUDIO".to_string()
                },
                state,
                unlocked,
                protocol,
            },
            relation,
        )
    }

    #[test]
    fn cold_console_without_user_selects_logon() {
        let candidates = vec![cp_candidate(
            1,
            "",
            SessionState::Active,
            false,
            Some(0),
            AccountMatch::NoUser,
        )];
        assert_eq!(classify_cp_bind(&candidates, 1), CpBindDecision::Logon(1));
        assert_eq!(
            classify_cp_bind(&candidates, 1),
            CpBindDecision::Logon(1),
            "repeated cold-boot classification must not retain stale state"
        );
    }

    #[test]
    fn no_user_console_policy_table() {
        let cases = [
            (
                "active local LogonUI",
                vec![cp_candidate(
                    1,
                    "",
                    SessionState::Active,
                    false,
                    Some(0),
                    AccountMatch::NoUser,
                )],
                true,
            ),
            (
                "connected local LogonUI",
                vec![cp_candidate(
                    1,
                    "",
                    SessionState::Connected,
                    false,
                    Some(0),
                    AccountMatch::NoUser,
                )],
                true,
            ),
            (
                "disconnected local session",
                vec![cp_candidate(
                    1,
                    "",
                    SessionState::Disconnected,
                    false,
                    Some(0),
                    AccountMatch::NoUser,
                )],
                false,
            ),
            (
                "RDP LogonUI",
                vec![cp_candidate(
                    1,
                    "",
                    SessionState::Connected,
                    false,
                    Some(2),
                    AccountMatch::NoUser,
                )],
                false,
            ),
            (
                "another account attached elsewhere",
                vec![
                    cp_candidate(
                        1,
                        "",
                        SessionState::Connected,
                        false,
                        Some(0),
                        AccountMatch::NoUser,
                    ),
                    cp_candidate(
                        7,
                        "other",
                        SessionState::Active,
                        false,
                        Some(2),
                        AccountMatch::Other,
                    ),
                ],
                false,
            ),
            (
                // A disconnected session holds no display. Windows itself signs
                // a second user in over one, which is why the case is named for
                // what it is rather than "another interactive session": the
                // candidate is precisely not interactive.
                "another account parked",
                vec![
                    cp_candidate(
                        1,
                        "",
                        SessionState::Connected,
                        false,
                        Some(0),
                        AccountMatch::NoUser,
                    ),
                    cp_candidate(
                        7,
                        "other",
                        SessionState::Disconnected,
                        false,
                        Some(2),
                        AccountMatch::Other,
                    ),
                ],
                true,
            ),
            (
                "unverifiable interactive session",
                vec![
                    cp_candidate(
                        1,
                        "",
                        SessionState::Connected,
                        false,
                        Some(0),
                        AccountMatch::NoUser,
                    ),
                    cp_candidate(
                        8,
                        "unknown",
                        SessionState::Disconnected,
                        false,
                        Some(2),
                        AccountMatch::Unknown,
                    ),
                ],
                false,
            ),
        ];

        for (name, candidates, eligible) in cases {
            let decision = classify_cp_bind(&candidates, 1);
            assert_eq!(
                matches!(decision, CpBindDecision::Logon(1)),
                eligible,
                "{name}: {decision:?}"
            );
        }
    }

    #[test]
    fn exact_locked_console_account_selects_unlock() {
        let candidates = vec![cp_candidate(
            3,
            "artist",
            SessionState::Active,
            false,
            Some(0),
            AccountMatch::Match,
        )];
        assert_eq!(classify_cp_bind(&candidates, 3), CpBindDecision::Unlock(3));
    }

    #[test]
    fn console_owner_policy_table_distinguishes_refusal_and_takeover() {
        let cases = [
            (
                "other account active on console",
                vec![cp_candidate(
                    3,
                    "other",
                    SessionState::Active,
                    false,
                    Some(0),
                    AccountMatch::Other,
                )],
                CpBindDecision::Reject("another account is actively using the physical console"),
            ),
            (
                // Not a logon: that id belongs to the other account's session,
                // and the logon path would fire SAS at it before failing.
                "other account disconnected on console, nothing of ours to move",
                vec![cp_candidate(
                    3,
                    "other",
                    SessionState::Disconnected,
                    false,
                    Some(0),
                    AccountMatch::Other,
                )],
                CpBindDecision::Reject(
                    "another account holds the console and this account has no session to move \
                     onto it",
                ),
            ),
            (
                "other account disconnected on console permits moving our parked session",
                vec![
                    cp_candidate(
                        3,
                        "other",
                        SessionState::Disconnected,
                        false,
                        Some(0),
                        AccountMatch::Other,
                    ),
                    cp_candidate(
                        9,
                        "artist",
                        SessionState::Disconnected,
                        false,
                        Some(0),
                        AccountMatch::Match,
                    ),
                ],
                CpBindDecision::Reconnect {
                    source: 9,
                    target: 3,
                },
            ),
            (
                "same account active unlocked binds",
                vec![cp_candidate(
                    3,
                    "artist",
                    SessionState::Active,
                    true,
                    Some(0),
                    AccountMatch::Match,
                )],
                CpBindDecision::Bound(3),
            ),
            (
                "same account active locked unlocks",
                vec![cp_candidate(
                    3,
                    "artist",
                    SessionState::Active,
                    false,
                    Some(0),
                    AccountMatch::Match,
                )],
                CpBindDecision::Unlock(3),
            ),
            (
                "unverifiable console still refuses",
                vec![cp_candidate(
                    3,
                    "unknown",
                    SessionState::Active,
                    false,
                    Some(0),
                    AccountMatch::Unknown,
                )],
                CpBindDecision::Reject("active console account could not be verified"),
            ),
            (
                "same account ambiguity still refuses",
                vec![
                    cp_candidate(
                        3,
                        "artist",
                        SessionState::Active,
                        true,
                        Some(0),
                        AccountMatch::Match,
                    ),
                    cp_candidate(
                        9,
                        "artist",
                        SessionState::Disconnected,
                        false,
                        Some(0),
                        AccountMatch::Match,
                    ),
                ],
                CpBindDecision::Reject("authenticated account session is ambiguous"),
            ),
        ];

        for (name, candidates, expected) in cases {
            assert_eq!(classify_cp_bind(&candidates, 3), expected, "{name}");
        }
    }

    #[test]
    fn exact_console_precedence_and_unlock_ambiguity_are_order_independent() {
        let console = cp_candidate(
            1,
            "artist",
            SessionState::Active,
            false,
            Some(0),
            AccountMatch::Match,
        );
        let cases = [
            (
                "same-SID unlocked RDP",
                cp_candidate(
                    7,
                    "artist",
                    SessionState::Disconnected,
                    true,
                    Some(2),
                    AccountMatch::Match,
                ),
                false,
            ),
            (
                // A parked session holds no display, so it cannot make the
                // console ambiguous. Measured on pier-windows-software.example.internal, where two
                // accounts had each signed in and disconnected, and refusing
                // here meant neither of them could ever use the machine.
                "other user parked",
                cp_candidate(
                    8,
                    "other",
                    SessionState::Disconnected,
                    false,
                    Some(2),
                    AccountMatch::Other,
                ),
                true,
            ),
            (
                "other user attached elsewhere",
                cp_candidate(
                    8,
                    "other",
                    SessionState::Active,
                    true,
                    Some(2),
                    AccountMatch::Other,
                ),
                false,
            ),
            (
                "additional identityless connected session",
                cp_candidate(
                    9,
                    "",
                    SessionState::Connected,
                    false,
                    Some(0),
                    AccountMatch::NoUser,
                ),
                false,
            ),
            (
                "user-bearing unsupported session",
                cp_candidate(
                    10,
                    "unknown",
                    SessionState::Unsupported(6),
                    false,
                    Some(0),
                    AccountMatch::Unknown,
                ),
                false,
            ),
            (
                "identityless disconnected service session",
                cp_candidate(
                    0,
                    "",
                    SessionState::Disconnected,
                    false,
                    Some(0),
                    AccountMatch::NoUser,
                ),
                true,
            ),
            (
                "known local listener",
                cp_candidate(
                    65_536,
                    "",
                    SessionState::Unsupported(6),
                    false,
                    Some(0),
                    AccountMatch::NoUser,
                ),
                true,
            ),
            (
                "generic identityless disconnected session",
                cp_candidate(
                    11,
                    "",
                    SessionState::Disconnected,
                    false,
                    Some(0),
                    AccountMatch::NoUser,
                ),
                false,
            ),
            (
                "remote protocol service-shaped session",
                cp_candidate(
                    0,
                    "",
                    SessionState::Disconnected,
                    false,
                    Some(2),
                    AccountMatch::NoUser,
                ),
                false,
            ),
        ];

        for (name, extra, eligible) in cases {
            for candidates in [
                vec![console.clone(), extra.clone()],
                vec![extra.clone(), console.clone()],
            ] {
                let decision = classify_cp_bind(&candidates, 1);
                assert_eq!(
                    matches!(decision, CpBindDecision::Unlock(1)),
                    eligible,
                    "{name}: {decision:?}"
                );
            }
        }
    }

    #[test]
    fn stale_same_sid_never_outranks_exact_console_bind() {
        let active = cp_candidate(
            1,
            "artist",
            SessionState::Active,
            true,
            Some(0),
            AccountMatch::Match,
        );
        let stale = cp_candidate(
            7,
            "artist",
            SessionState::Disconnected,
            true,
            Some(2),
            AccountMatch::Match,
        );
        for candidates in [
            vec![active.clone(), stale.clone()],
            vec![stale.clone(), active.clone()],
        ] {
            assert!(matches!(
                classify_cp_bind(&candidates, 1),
                CpBindDecision::Reject(_)
            ));
        }
        assert_eq!(classify_cp_bind(&[active], 1), CpBindDecision::Bound(1));
    }

    #[test]
    fn other_remote_stale_and_ambiguous_sessions_never_unlock() {
        for candidates in [
            vec![cp_candidate(
                1,
                "other",
                SessionState::Active,
                false,
                Some(0),
                AccountMatch::Other,
            )],
            vec![cp_candidate(
                2,
                "artist",
                SessionState::Active,
                false,
                Some(2),
                AccountMatch::Match,
            )],
            vec![cp_candidate(
                4,
                "artist",
                SessionState::Disconnected,
                false,
                Some(0),
                AccountMatch::Match,
            )],
            vec![
                cp_candidate(
                    1,
                    "artist",
                    SessionState::Active,
                    false,
                    Some(0),
                    AccountMatch::Match,
                ),
                cp_candidate(
                    7,
                    "artist",
                    SessionState::Disconnected,
                    false,
                    Some(2),
                    AccountMatch::Match,
                ),
            ],
        ] {
            let console_id = candidates[0].0.id;
            assert!(matches!(
                classify_cp_bind(&candidates, console_id),
                CpBindDecision::Reject(_)
            ));
        }
    }

    #[test]
    fn console_we_own_binds_while_another_account_is_merely_parked() {
        // pier-windows-software.example.internal as measured after the account's session was moved onto
        // the console:
        //
        //   console=1 glass=1 sessions=[0:disconnected 1:active/user
        //   4:disconnected/user 65536:listen 65537:listen]
        //
        // Session 4 is a different account, parked. Treating it as competing
        // rejected a console the authenticated account already owned, which
        // meant a machine two people share could never be used by either of
        // them through Arcen once both had signed in once.
        let with_other_parked = vec![
            cp_candidate(
                0,
                "",
                SessionState::Disconnected,
                false,
                Some(0),
                AccountMatch::NoUser,
            ),
            cp_candidate(
                1,
                "artist.user",
                SessionState::Active,
                true,
                Some(0),
                AccountMatch::Match,
            ),
            cp_candidate(
                4,
                "artist.admin",
                SessionState::Disconnected,
                false,
                Some(0),
                AccountMatch::Other,
            ),
        ];
        assert_eq!(
            classify_cp_bind(&with_other_parked, 1),
            CpBindDecision::Bound(1)
        );

        // Locked instead of unlocked must still route through the credential
        // provider rather than binding, because Windows locks a session when it
        // is disconnected and a move can land either way.
        let locked = with_other_parked
            .iter()
            .cloned()
            .map(|(mut candidate, relation)| {
                if candidate.id == 1 {
                    candidate.unlocked = false;
                }
                (candidate, relation)
            })
            .collect::<Vec<_>>();
        assert_eq!(classify_cp_bind(&locked, 1), CpBindDecision::Unlock(1));

        // A second session belonging to the *same* account is still ambiguous:
        // one account cannot have two desktops and let us pick.
        let mut with_second_own = with_other_parked.clone();
        with_second_own.push(cp_candidate(
            7,
            "artist.user",
            SessionState::Disconnected,
            false,
            Some(0),
            AccountMatch::Match,
        ));
        assert!(matches!(
            classify_cp_bind(&with_second_own, 1),
            CpBindDecision::Reject(_)
        ));

        // And another account actually attached elsewhere still blocks.
        let mut with_other_attached = with_other_parked.clone();
        with_other_attached.push(cp_candidate(
            9,
            "someone",
            SessionState::Active,
            true,
            Some(2),
            AccountMatch::Other,
        ));
        assert!(matches!(
            classify_cp_bind(&with_other_attached, 1),
            CpBindDecision::Reject(_)
        ));
    }

    #[test]
    fn topology_measured_immediately_after_a_console_move_binds_the_account() {
        // The state pier-windows.example.internal reported the instant WTSConnectSessionW
        // returned, logged by the broker itself:
        //
        //   console=1 glass=1 sessions=[0:disconnected/proto0
        //   1:active/user/proto0 2:unsupported-8/proto0 65536:unsupported-6
        //   65537:unsupported-6]
        //
        // Session 2 is the LogonUI console the account just displaced, now in
        // WTSDown. Treating it as a competing session rejected the very move
        // that had just succeeded, which is the one failure mode a takeover
        // must not have.
        let candidates = vec![
            cp_candidate(
                0,
                "",
                SessionState::Disconnected,
                false,
                Some(0),
                AccountMatch::NoUser,
            ),
            cp_candidate(
                1,
                "admin",
                SessionState::Active,
                true,
                Some(0),
                AccountMatch::Match,
            ),
            cp_candidate(
                2,
                "",
                SessionState::Unsupported(8),
                false,
                Some(0),
                AccountMatch::NoUser,
            ),
            cp_candidate(
                65_536,
                "",
                SessionState::Unsupported(6),
                false,
                Some(0),
                AccountMatch::NoUser,
            ),
            cp_candidate(
                65_537,
                "",
                SessionState::Unsupported(6),
                false,
                Some(0),
                AccountMatch::NoUser,
            ),
        ];
        assert_eq!(classify_cp_bind(&candidates, 1), CpBindDecision::Bound(1));
    }

    #[test]
    fn measured_lab_topology_after_rdp_disconnect_moves_the_account_to_the_console() {
        // Reproduces pier-windows.example.internal exactly as measured on 2026-08-04, after the
        // owner signed in over RDP and closed the client:
        //
        //   WTSGetActiveConsoleSessionId() = 2, GlassSessionId = 2
        //   0     Services  proto 0  no user   Disconnected
        //   1               proto 0  admin     Disconnected   <- parked desktop
        //   2     Console   proto 0  no user   Connected      <- LogonUI
        //   65536/65537     proto 0  no user   Listen
        //
        // Before this change the broker refused with "another, stale, or
        // unverifiable interactive session exists" and the owner had no way to
        // reach the machine with Arcen at all.
        let candidates = vec![
            cp_candidate(
                0,
                "",
                SessionState::Disconnected,
                false,
                Some(0),
                AccountMatch::NoUser,
            ),
            cp_candidate(
                1,
                "admin",
                SessionState::Disconnected,
                false,
                Some(0),
                AccountMatch::Match,
            ),
            cp_candidate(
                2,
                "",
                SessionState::Connected,
                false,
                Some(0),
                AccountMatch::NoUser,
            ),
            cp_candidate(
                65_536,
                "",
                SessionState::Unsupported(6),
                false,
                Some(0),
                AccountMatch::NoUser,
            ),
            cp_candidate(
                65_537,
                "",
                SessionState::Unsupported(6),
                false,
                Some(0),
                AccountMatch::NoUser,
            ),
        ];
        assert_eq!(
            classify_cp_bind(&candidates, 2),
            CpBindDecision::Reconnect {
                source: 1,
                target: 2
            }
        );
    }

    #[test]
    fn measured_lab_topology_with_two_parked_users_moves_only_the_authenticated_one() {
        // pier-windows-software.example.internal as measured the same day: two users parked, console at
        // LogonUI in session 3, GlassSessionId 3. The other account's parked
        // session must be ignored rather than treated as competing, because a
        // disconnected session holds no display — that is why RDP itself lets a
        // second user sign in over one.
        let candidates = vec![
            cp_candidate(
                0,
                "",
                SessionState::Disconnected,
                false,
                Some(0),
                AccountMatch::NoUser,
            ),
            cp_candidate(
                1,
                "artist.user",
                SessionState::Disconnected,
                false,
                Some(0),
                AccountMatch::Match,
            ),
            cp_candidate(
                3,
                "",
                SessionState::Connected,
                false,
                Some(0),
                AccountMatch::NoUser,
            ),
            cp_candidate(
                4,
                "artist.admin",
                SessionState::Disconnected,
                false,
                Some(0),
                AccountMatch::Other,
            ),
        ];
        assert_eq!(
            classify_cp_bind(&candidates, 3),
            CpBindDecision::Reconnect {
                source: 1,
                target: 3
            }
        );
    }

    #[test]
    fn parked_matching_session_is_moved_to_the_console_not_signed_in_again() {
        // Measured on two Windows 11 Pro lab hosts: a user signs in over RDP,
        // closes the client, and is left with a WTSDisconnected session while
        // the console falls back to LogonUI in a different session id.
        //
        // This used to reject, which left the user with no way forward at all —
        // they cannot move their own session from inside it and still have a
        // route back to the machine. What must never happen is a *cold logon*
        // over the top, because that would leave one account owning two
        // desktops and the broker binding whichever it found first. Moving the
        // existing session onto the console preserves that invariant, so the
        // test asserts both halves.
        let candidates = vec![
            cp_candidate(
                1,
                "",
                SessionState::Active,
                false,
                Some(0),
                AccountMatch::NoUser,
            ),
            cp_candidate(
                6,
                "artist",
                SessionState::Disconnected,
                false,
                // 0, not 2: WTSClientProtocolType describes the attached client,
                // so a disconnected RDP session reads exactly like the console.
                Some(0),
                AccountMatch::Match,
            ),
        ];
        assert_eq!(
            classify_cp_bind(&candidates, 1),
            CpBindDecision::Reconnect {
                source: 6,
                target: 1
            }
        );
    }

    #[test]
    fn another_or_unverifiable_session_blocks_cp_dispatch() {
        // Another account that is actually attached to a desktop still blocks:
        // signing in at the console would displace a session somebody is using.
        let cold_with_other_attached = vec![
            cp_candidate(
                1,
                "",
                SessionState::Active,
                false,
                Some(0),
                AccountMatch::NoUser,
            ),
            cp_candidate(
                8,
                "other",
                SessionState::Active,
                false,
                Some(2),
                AccountMatch::Other,
            ),
        ];
        assert!(matches!(
            classify_cp_bind(&cold_with_other_attached, 1),
            CpBindDecision::Reject(_)
        ));

        // A session whose owner could not be verified always blocks, whatever
        // state it is in. This is the half of the rule that is a safety
        // invariant rather than a policy choice: the classifier must never act
        // on a desktop it cannot attribute.
        for state in [
            SessionState::Active,
            SessionState::Connected,
            SessionState::Disconnected,
        ] {
            let cold_with_unknown = vec![
                cp_candidate(
                    1,
                    "",
                    SessionState::Active,
                    false,
                    Some(0),
                    AccountMatch::NoUser,
                ),
                cp_candidate(8, "unknown", state, false, Some(2), AccountMatch::Unknown),
            ];
            assert!(
                matches!(
                    classify_cp_bind(&cold_with_unknown, 1),
                    CpBindDecision::Reject(_)
                ),
                "unverifiable {state:?} session must block"
            );
        }

        let unlock_with_unknown = vec![
            cp_candidate(
                1,
                "artist",
                SessionState::Active,
                false,
                Some(0),
                AccountMatch::Match,
            ),
            cp_candidate(
                9,
                "unknown",
                SessionState::Disconnected,
                false,
                Some(2),
                AccountMatch::Unknown,
            ),
        ];
        assert!(matches!(
            classify_cp_bind(&unlock_with_unknown, 1),
            CpBindDecision::Reject(_)
        ));
    }

    #[test]
    fn agent_config_rejects_false_444_h264_claim() {
        let config = AgentConfig {
            capenc_bin: "capenc".to_string(),
            global_output_index: Some(0),
            adapter_name: None,
            adapter_output_index: None,
            codec: "h264".to_string(),
            chroma: "yuv444".to_string(),
            bit_depth: default_bit_depth(),
            color_range: default_color_range(),
            color_matrix: default_color_matrix(),
            color_policy: default_color_policy(),
            qp_map: default_qp_map(),
            video_selection: VideoSelectionIntent::Exact,
            codec_pinned: false,
            variant_pinned: false,
            auth_video_request: None,
            fps: 60,
            encoder: None,
            audio_enabled: true,
            audio_compressed: false,
            microphone_input_enabled: false,
            clipboard_policy: arcen_media::clipboard::ClipboardPolicy::default(),
            timezone_redirection: false,
            qos_targets: arcen_telemetry::QosTargets::default(),
            deskside: crate::deskside::DesksideConfig::default(),
            iddcx: crate::config::WindowsIddCxConfig::default(),
            multi_monitor: crate::config::WindowsMultiMonitorConfig::default(),
        };
        assert!(config.into_host().is_err());
    }

    #[test]
    fn agent_config_rejects_non_h264_exact_openh264_claim() {
        let config = AgentConfig {
            capenc_bin: "capenc".to_string(),
            global_output_index: Some(0),
            adapter_name: None,
            adapter_output_index: None,
            codec: "h265".to_string(),
            chroma: "yuv420".to_string(),
            bit_depth: default_bit_depth(),
            color_range: default_color_range(),
            color_matrix: default_color_matrix(),
            color_policy: default_color_policy(),
            qp_map: default_qp_map(),
            video_selection: VideoSelectionIntent::Exact,
            codec_pinned: true,
            variant_pinned: false,
            auth_video_request: None,
            fps: 30,
            encoder: Some("software-h264".to_string()),
            audio_enabled: true,
            audio_compressed: false,
            microphone_input_enabled: false,
            clipboard_policy: arcen_media::clipboard::ClipboardPolicy::default(),
            timezone_redirection: false,
            qos_targets: arcen_telemetry::QosTargets::default(),
            deskside: crate::deskside::DesksideConfig::default(),
            iddcx: crate::config::WindowsIddCxConfig::default(),
            multi_monitor: crate::config::WindowsMultiMonitorConfig::default(),
        };
        assert!(config.into_host().is_err());
    }

    #[test]
    fn agent_config_accepts_shipped_openh264_backend() {
        let software = AgentConfig {
            capenc_bin: "capenc".to_string(),
            global_output_index: Some(0),
            adapter_name: None,
            adapter_output_index: None,
            codec: "h264".to_string(),
            chroma: "yuv420".to_string(),
            bit_depth: default_bit_depth(),
            color_range: default_color_range(),
            color_matrix: default_color_matrix(),
            color_policy: default_color_policy(),
            qp_map: default_qp_map(),
            video_selection: VideoSelectionIntent::Exact,
            codec_pinned: false,
            variant_pinned: false,
            auth_video_request: None,
            fps: 30,
            encoder: Some("software-h264".to_string()),
            audio_enabled: true,
            audio_compressed: false,
            microphone_input_enabled: false,
            clipboard_policy: arcen_media::clipboard::ClipboardPolicy::default(),
            timezone_redirection: false,
            qos_targets: arcen_telemetry::QosTargets::default(),
            deskside: crate::deskside::DesksideConfig::default(),
            iddcx: crate::config::WindowsIddCxConfig::default(),
            multi_monitor: crate::config::WindowsMultiMonitorConfig::default(),
        };
        assert_eq!(
            software.into_host().unwrap().encoder,
            crate::capenc::EncoderSelection::SoftwareH264
        );
    }
}
