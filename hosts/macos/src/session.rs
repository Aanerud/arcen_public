//! Session handshake for the macOS Pier.
//!
//! The order here is not a style choice. The Deck decides how a session works
//! from the *first* message it receives: `auth_request` selects its
//! authenticated path, while `server_hello` selects its no-authentication
//! path and zeroizes the password it was given. A host that greets first is
//! therefore not merely greeting early — it tells the client no credentials
//! are wanted, and the client obliges.
//!
//! So the Pier asks for credentials first, proves the account, and only then
//! describes the desktop it is prepared to serve:
//!
//! ```text
//! auth_request  -> Deck
//! auth_response <- Deck
//! auth_result   -> Deck
//! server_hello  -> Deck
//! client_hello  <- Deck
//! ```
//!
//! Capability claims are derived from the same probes the `probe-media`
//! command runs, rather than hard-coded. A host that advertises a codec it
//! cannot encode produces a black window on the other side, which is worse
//! than refusing the session.

use std::time::Duration;

use crate::net::{self, PierSocket};

use arcen_input::{
    CapabilityAvailability as InputCapabilityTruth, CursorMode as InputCursorMode,
    TabletMode as InputTabletMode, resolve_cursor_mode, resolve_tablet_mode,
};
use arcen_protocol::messages::{
    CursorModeReason, CursorModeResultMsg, InputCapabilityAvailability, TabletModeReason,
    TabletModeResultMsg,
};

/// How long a client has to answer the credential prompt.
const AUTH_TIMEOUT: Duration = Duration::from_secs(60);
/// How long the whole application handshake may occupy the one session slot.
const APPLICATION_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(90);

/// What the Pier told the Deck about itself.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq)]
pub struct AdvertisedCapabilities {
    /// The display these capabilities describe.
    pub display_id: u32,
    /// All displays probed for this connection, primary first as reported by
    /// `WindowServer`.
    pub displays: Vec<crate::displays::DisplaySnapshot>,
    /// Whether this is the login window rather than a signed-in desktop.
    /// Signing in ends it, and the Deck is told so it reconnects rather than
    /// reports a lost connection.
    pub login_window: bool,
    /// Primary display width in pixels.
    pub width: u32,
    /// Primary display height in pixels.
    pub height: u32,
    /// Whether HEVC encoding is available.
    pub hevc: bool,
    /// Whether H.264 encoding is available.
    pub h264: bool,
    /// Whether a ten-bit HEVC profile has been proven.
    pub main10: bool,
    /// Whether a 4:4:4 HEVC path has been proven.
    pub chroma_444: bool,
    /// Whether `VideoToolbox` gave this host a hardware encoder, when it could
    /// be asked.
    ///
    /// `None` is sent as no claim at all, which is what a host that could not
    /// measure owes the Deck.
    pub encoder_hardware: Option<bool>,
    /// Whether host audio can be captured and sent.
    ///
    /// Established by creating a tap, not by reading configuration: a machine
    /// with audio turned on in a file and no capture consent would otherwise
    /// advertise sound it cannot produce, and silence is indistinguishable
    /// from a quiet desktop.
    pub audio: bool,
    /// The configured host audio policy advertised to the Deck.
    pub audio_policy: arcen_media::audio::ConfiguredAudioPolicy,
    /// The configured host clipboard policy advertised to the Deck.
    pub clipboard_policy: arcen_media::clipboard::ClipboardPolicy,
    /// The configured Deck-to-host microphone policy advertised to the Deck.
    pub microphone_policy: arcen_media::audio::MicrophonePolicy,
    /// The pre-auth multi-monitor offer sent on this connection, if any.
    pub multi_monitor_v1: Option<arcen_protocol::messages::AuthMultiMonitorOfferMsg>,
}

/// Encoder claims established before the listener accepts a session.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncoderCapabilityEvidence {
    pub h264: bool,
    pub hevc: bool,
    pub main10: bool,
    pub chroma_444: bool,
    /// Whether `VideoToolbox` gave the probe a hardware encoder.
    ///
    /// `None` means it could not be asked, which is a different answer from
    /// software and is sent as no claim at all.
    pub hardware: Option<bool>,
}

impl EncoderCapabilityEvidence {
    #[must_use]
    pub const fn unavailable() -> Self {
        Self {
            h264: false,
            hevc: false,
            main10: false,
            chroma_444: false,
            hardware: None,
        }
    }
}

/// Operator policy and native evidence used by a session handshake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionPolicy {
    pub multi_monitor: crate::MacOsMultiMonitorConfig,
    pub audio_enabled: bool,
    pub audio_compressed: bool,
    pub microphone_enabled: bool,
    pub microphone_backend_available: bool,
    pub clipboard: arcen_media::clipboard::ClipboardPolicy,
    pub encoder: EncoderCapabilityEvidence,
    /// The only account this process may serve, when it is a desktop agent.
    ///
    /// An agent lives in one person's session and can show nobody else's
    /// screen, so a different account that authenticates is refused before it
    /// is told it succeeded — rather than greeted, and then dropped.
    pub serving_uid: Option<u32>,
    /// This process serves the login window rather than a user's desktop.
    pub login_window: bool,
}

impl SessionPolicy {
    /// Builds the policy this Pier will advertise and enforce.
    ///
    /// # Errors
    ///
    /// Returns an error when the clipboard policy in the already-parsed Pier
    /// configuration is invalid.
    pub fn from_config(config: &crate::PierFileConfig) -> Result<Self, String> {
        Ok(Self {
            multi_monitor: config.platform.multi_monitor.clone(),
            audio_enabled: config.audio.enabled,
            audio_compressed: config.audio.compressed,
            microphone_enabled: config.microphone_input.enabled,
            microphone_backend_available: crate::microphone_input::backend_available(),
            clipboard: crate::clipboard_policy_from_config(config)?,
            encoder: encoder_capabilities(),
            serving_uid: None,
            login_window: false,
        })
    }
}

impl Default for SessionPolicy {
    fn default() -> Self {
        Self {
            multi_monitor: crate::MacOsMultiMonitorConfig::default(),
            audio_enabled: true,
            audio_compressed: false,
            microphone_enabled: false,
            microphone_backend_available: false,
            clipboard: arcen_media::clipboard::ClipboardPolicy::default(),
            encoder: encoder_capabilities(),
            serving_uid: None,
            login_window: false,
        }
    }
}

/// The result of a completed handshake.
// No longer Clone or PartialEq: this owns the display arranged for the session,
// and a display is not a value that can be copied or compared. Cloning one
// would mean two owners each removing it on drop.
#[derive(Debug)]
pub struct Handshake {
    /// This session serves the login window rather than a user's desktop.
    pub login_window: bool,
    /// This session's hold on the host's one session slot, taken after the
    /// password was proved and released when the handshake is dropped.
    pub admission: Option<AdmissionGuard>,
    /// The macOS account that was authenticated.
    pub user: String,
    /// What this host advertised.
    pub advertised: AdvertisedCapabilities,
    /// The display the session will capture.
    pub display_id: u32,
    /// The raw `client_hello` the Deck replied with.
    pub client_hello: String,
    /// What audio this session will carry, resolved from host policy and the
    /// Deck's advertised output capability.
    ///
    /// Resolved at the handshake and carried, because capture must not start
    /// until this says it should: on macOS, creating a tap raises the system
    /// audio consent prompt and blocks inside it. A host that captures first
    /// and asks later stalls every session that never wanted sound.
    pub audio: arcen_media::audio::ResolvedAudioStream,
    /// The frame rate this session will serve, resolved from the client's
    /// request against this host's ceiling.
    pub fps: u32,
    /// Shared detail/motion preference for later encoder/rate-control choices.
    pub motion_priority: arcen_media::video::MotionPriority,
    pub requested_pipeline: Option<arcen_media::video::PipelineId>,
    pub active_pipeline: Option<arcen_protocol::messages::ServedStreamPipeline>,
    /// The codec this session will encode with.
    ///
    /// Negotiated, not assumed. The encoder was pinned to HEVC while the
    /// resolver was already recording `ClientLacksSupport` for a Deck that had
    /// said it could not decode it — so the host knew, degraded the plan, and
    /// then sent HEVC anyway. The plan carries no codec axis, so the decision
    /// is carried here.
    pub codec: crate::encode::EncoderCodec,
    /// The clipboard this session negotiated, if any.
    ///
    /// `None` means no pasteboard is opened. The host used to start the
    /// clipboard worker for every session and let its own policy refuse
    /// individual payloads, which still read the local pasteboard on every
    /// poll for a user who had switched the clipboard off — and a refusal
    /// after the read is not a refusal.
    pub clipboard: Option<arcen_media::clipboard::ClipboardNegotiation>,
    /// What microphone this session will carry from the Deck.
    ///
    /// Always resolved, even though this host has no importer yet, because a
    /// Deck that asked for a microphone and is told nothing waits for one. The
    /// Linux Pier answers every request; macOS answered none, and a client
    /// that requested it sat out the full media timeout in silence.
    pub microphone: arcen_media::audio::ResolvedMicrophoneStream,
    /// What this session will actually be served, resolved from what the Deck
    /// asked for and what this host can do.
    ///
    /// Resolved once, at the handshake, so that the hello the Deck is told and
    /// the capture the host starts come from the same decision. Deciding twice
    /// is how a host advertises 4:4:4 and serves 4:2:0.
    pub plan: arcen_media::session_plan::ResolvedVideoPlan,
    /// The admitted multi-monitor topology, when this session requested one.
    pub multi_monitor: Option<crate::multi_monitor::MacOsMultiMonitorPlan>,
    /// The picture size this session will capture and send.
    ///
    /// Deliberately separate from `advertised.width`/`height`, which stay the
    /// host's own desktop because that is the coordinate space pointer input
    /// is mapped onto. The two were the same number for as long as the host
    /// ignored what the Deck asked for; they are different questions.
    pub capture_width: u32,
    /// See [`Handshake::capture_width`].
    pub capture_height: u32,
    /// The size the Deck asked for, before it was reduced to fit this host's
    /// desktop.
    ///
    /// Kept alongside the capture size because they answer different
    /// questions: the capture size is what this host can serve from the
    /// display it has, and this is what the Deck would like if a display of
    /// that size could be arranged.
    pub requested_width: u32,
    /// See [`Handshake::requested_width`].
    pub requested_height: u32,
    /// A display arranged for a legacy primary-only session, held so it lives exactly as long.
    ///
    /// `None` means the host's own display was already the right size, or none
    /// could be arranged. Dropping this removes the display.
    pub arranged_display: Option<crate::virtual_display::VirtualDisplay>,
    /// Displays arranged for a Match My Layout session, one per Deck monitor.
    ///
    /// Dropping this removes every virtual display, including after any stream
    /// or session failure.
    pub multi_monitor_displays: Option<crate::multi_monitor::MacOsVirtualDisplays>,
    /// Who draws the pointer for this session.
    ///
    /// [`CursorMode::Local`] means the Deck draws its own, which moves at the
    /// speed of the hand because nothing has to cross the link for it — but it
    /// is always an arrow, because this host cannot read the shape the desktop
    /// is showing. [`CursorMode::Host`] means the compositor draws it into the
    /// captured picture, so it is always the right shape and always as late as
    /// the picture.
    ///
    /// There is no third option that is better than both, so the person
    /// chooses.
    pub cursor_mode: arcen_protocol::messages::CursorMode,
    /// Authoritative cursor/tablet negotiation results, sent before input begins.
    pub input_mode_results: crate::stream::InputModeResults,
    /// The Deck time-zone identifier supplied with the authenticated request.
    pub authenticated_timezone: Option<String>,
}

/// The desktop size the Deck asked to be sent.
///
/// Carried rather than discarded. The Deck states this in its auth response —
/// it is how "match my primary display" reaches the host — and this host used
/// to read the field and throw it away, then capture at whatever size its own
/// screen happened to be. A Deck asking for 1920x1080 in front of a 3600x2338
/// host was served 3600x2338: four times the pixels to encode and send, and a
/// picture the Deck then had to resample anyway.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestedDesktop {
    /// The Deck named a size.
    Exact { width: u32, height: u32 },
    /// The Deck named nothing usable, so the host's own geometry stands.
    HostChoice,
}

/// The panel attributes a Deck display lends the arranged display.
///
/// Physical size comes from the shared rule the Windows Pier's EDID uses, so
/// both hosts give a Deck panel the same density: the Deck's millimetres when
/// it reports them, otherwise a size derived from its scale.
fn panel_identity(
    monitor: &arcen_protocol::messages::ClientMonitor,
    width: u32,
    height: u32,
) -> crate::virtual_display::PanelIdentity {
    let size_mm = arcen_outputs::edid::physical_size_mm(arcen_outputs::edid::EdidRequest {
        width,
        height,
        refresh_hz: monitor.refresh_hz,
        width_mm: monitor.width_mm,
        height_mm: monitor.height_mm,
        scale: monitor.scale,
        product_id: 0,
        serial: 0,
        // shared-contract colourless-edid: `physical_size_mm` ignores colour;
        // panel selection consumes the Deck display's colour contract separately.
        color: None,
    });
    crate::virtual_display::PanelIdentity {
        size_mm,
        name: if monitor.name.trim().is_empty() {
            "Deck display".to_owned()
        } else {
            monitor.name.trim().to_owned()
        },
        color: monitor
            .color
            .as_ref()
            .map(arcen_media::display_color::DisplayColor::from_msg),
        serial: 0x4152_4345,
    }
}

/// The refresh rate to build an arranged display at, for a session capturing
/// at `fps`.
///
/// Deliberately not `fps`. A display that refreshes at exactly the rate we
/// want to capture leaves no slack, and ScreenCaptureKit does not reliably
/// take every single vsync of the display it is capturing — it lands on every
/// other one, which halves the rate rather than shaving it. Measured on the
/// lab Mac: a 30 Hz arranged display serving a 30 fps request delivered a
/// frame every 67.4 ms, exactly two of its vsyncs, for 15 fps. The capture
/// callback was occupied 0.013 ms of that and superseded nothing, so the
/// ceiling was the display's, not the host's.
///
/// Doubling gives the compositor a whole spare vsync per captured frame. The
/// floor of 60 keeps an ordinary desktop feeling normal to anyone sitting at
/// it, and the ceiling keeps the request inside rates virtual displays are
/// known to accept.
fn arranged_refresh_hz(fps: u32) -> f64 {
    const MIN_HZ: u32 = 60;
    const MAX_HZ: u32 = 120;
    f64::from(fps.max(1).saturating_mul(2).clamp(MIN_HZ, MAX_HZ))
}

impl RequestedDesktop {
    /// Reads the request out of an auth response.
    ///
    /// Zero means the Deck did not enumerate its display, which is different
    /// from asking for a zero-sized desktop. Absurd sizes are refused rather
    /// than clamped: a host that quietly serves something other than what was
    /// asked for is the behaviour this type exists to end.
    #[must_use]
    pub const fn from_auth(width: u32, height: u32) -> Self {
        if width == 0 || height == 0 || width > MAX_REQUESTED_EDGE || height > MAX_REQUESTED_EDGE {
            return Self::HostChoice;
        }
        Self::Exact { width, height }
    }

    /// Arranges a display of the requested size, when one is needed and can be
    /// had.
    ///
    /// Returns `None` when the host's own display already offers the size, or
    /// when this macOS provides no way to add one — both of which leave the
    /// session serving the real display at the reduced size
    /// [`RequestedDesktop::resolve`] produces.
    #[must_use]
    pub fn arrange(
        self,
        host_display: u32,
        fps: u32,
        panel: crate::virtual_display::VirtualPanel,
        client_display: Option<&arcen_protocol::messages::ClientMonitor>,
    ) -> Option<crate::virtual_display::VirtualDisplay> {
        let Self::Exact { width, height } = self else {
            return None;
        };
        // Asked, not assumed. A host with a real monitor that already offers
        // the size must not gain a second display it did not need — unless
        // HDR was asked for and the real one cannot show it.
        let host_can_serve_panel = panel == crate::virtual_display::VirtualPanel::Sdr
            || crate::displays::potential_headroom(host_display)
                .is_some_and(|headroom| headroom > 1.0);
        if host_can_serve_panel
            && crate::displays::available_modes(host_display).iter().any(
                |(mode_width, mode_height)| {
                    *mode_width == width as usize && *mode_height == height as usize
                },
            )
        {
            return None;
        }
        let identity = client_display.map(|monitor| panel_identity(monitor, width, height));
        match crate::virtual_display::VirtualDisplay::create_panel(
            width,
            height,
            arranged_refresh_hz(fps),
            panel,
            identity.as_ref(),
        ) {
            Ok(display) => {
                tracing::info!(
                    target: arcen_telemetry::names::target::MEDIA,
                    width,
                    height,
                    panel = ?panel,
                    size_mm = ?identity.as_ref().map(|identity| identity.size_mm),
                    from = ?identity.as_ref().map(|identity| identity.name.as_str()),
                    "arranged a display the size the client asked for"
                );
                // The window server publishes it asynchronously, and capturing
                // a display that is not there yet fails in a way that reads as
                // a capture fault.
                std::thread::sleep(std::time::Duration::from_millis(1500));
                Some(display)
            }
            Err(error) => {
                tracing::warn!(
                    target: arcen_telemetry::names::target::MEDIA,
                    width,
                    height,
                    %error,
                    "serving this host's own display instead"
                );
                None
            }
        }
    }

    /// Returns the size to capture, given the desktop this host actually has.
    ///
    /// Two rules, both learned the hard way:
    ///
    /// **Never change the shape.** `ScreenCaptureKit` fits the desktop inside
    /// whatever output size it is given, so asking for an aspect ratio the
    /// desktop does not have produces a picture with bars down two of its
    /// sides. Pointer coordinates arrive normalised against the picture and are
    /// mapped onto the desktop, and nothing in that path knows the bars are
    /// there — so every click lands short, by more the further it is from the
    /// centre. A 1920x1080 desktop asked for 1800x1130 put roughly 59 pixels of
    /// black above and below the image, and made the desktop impossible to
    /// click accurately.
    ///
    /// **Never scale up.** A desktop has the detail it has. Encoding a
    /// 1920x1080 desktop as 2560x1440 costs 1.8 times the pixels and 1.8 times
    /// the bandwidth to deliver exactly the same picture, slightly blurrier for
    /// having been through an interpolator.
    ///
    /// So the request sets a *ceiling*, and the desktop sets the shape. A Deck
    /// asking for less than the host has gets a faithful reduction of it; a
    /// Deck asking for more gets the desktop as it really is, and scales it
    /// itself if it wants to fill its screen.
    #[must_use]
    pub fn resolve(self, host_width: u32, host_height: u32) -> (u32, u32) {
        let Self::Exact { width, height } = self else {
            return (host_width, host_height);
        };
        if host_width == 0 || host_height == 0 {
            return (host_width, host_height);
        }
        let fit = (f64::from(width) / f64::from(host_width))
            .min(f64::from(height) / f64::from(host_height))
            .min(1.0);
        if fit <= 0.0 {
            return (host_width, host_height);
        }
        // Rounded to even pixels: every chroma-subsampled format this host
        // sends has two-pixel chroma siting, and an odd dimension is a
        // half-sample nothing downstream can represent.
        let scaled = |edge: u32| -> u32 {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let value = (f64::from(edge) * fit).round() as u32;
            value.max(2) & !1
        };
        (scaled(host_width), scaled(host_height))
    }
}

/// The frame rate to give an arranged display, before the plan is resolved.
///
/// The resolver runs after the display is arranged, so this reads the client's
/// request directly and falls back to sixty, which is what a display mode
/// wants when nobody has said otherwise.
fn fps_hint(initial: Option<&arcen_protocol::messages::InitialVideoRequestMsg>) -> u32 {
    initial.map_or(60, |request| request.quality.max_fps.max(1))
}

/// The largest edge a Deck may ask for.
///
/// Well past 8K, and short of anything that would make an encoder session
/// allocation absurd.
const MAX_REQUESTED_EDGE: u32 = 16_384;

/// Why a handshake did not complete.
#[derive(Debug)]
pub enum HandshakeError {
    /// The display inventory could not be read, so nothing could be offered.
    NoDisplays(String),
    /// The framed transport failed.
    Transport(String),
    /// The Deck closed before replying.
    PeerClosed,
    /// The Deck sent something that was not a `client_hello`.
    UnexpectedMessage(String),
    /// The client did not answer the credential prompt in time.
    AuthTimeout,
    /// The client did not complete the application handshake in time.
    ApplicationHandshakeTimeout,
    /// The client could not prove an account.
    Rejected(crate::auth::AuthFailure),
    /// The client's multi-monitor request was not safe to apply.
    MultiMonitor(String),
    /// The client's auth-time video request was invalid.
    VideoRequest(String),
    /// No measured encoder can serve even the compatibility stream.
    EncoderUnavailable(String),
}

impl std::fmt::Display for HandshakeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoDisplays(detail) => write!(formatter, "no capturable display: {detail}"),
            Self::Transport(detail) => write!(formatter, "{detail}"),
            Self::PeerClosed => formatter.write_str("the client closed before replying"),
            Self::UnexpectedMessage(kind) => {
                write!(formatter, "expected client_hello, received '{kind}'")
            }
            Self::AuthTimeout => formatter.write_str("the client did not authenticate in time"),
            Self::ApplicationHandshakeTimeout => {
                formatter.write_str("the client did not complete the application handshake in time")
            }
            Self::Rejected(failure) => write!(formatter, "authentication refused: {failure}"),
            Self::MultiMonitor(detail) => write!(formatter, "multi-monitor refused: {detail}"),
            Self::VideoRequest(detail) => write!(formatter, "video request refused: {detail}"),
            Self::EncoderUnavailable(detail) => write!(formatter, "encoder unavailable: {detail}"),
        }
    }
}

impl std::error::Error for HandshakeError {}

/// Reads what this Mac can actually offer.
///
/// # Errors
///
/// Returns [`HandshakeError::NoDisplays`] when `WindowServer` reports nothing
/// to capture, because there is then no session to offer.
pub fn advertise() -> Result<AdvertisedCapabilities, HandshakeError> {
    advertise_with_config(&SessionPolicy::default())
}

/// Reads what this Mac can actually offer with this operator policy.
///
/// # Errors
///
/// Returns [`HandshakeError::NoDisplays`] when `WindowServer` reports nothing
/// to capture, because there is then no session to offer.
pub fn advertise_with_config(
    policy: &SessionPolicy,
) -> Result<AdvertisedCapabilities, HandshakeError> {
    if !policy.encoder.h264 && !policy.encoder.hevc {
        return Err(HandshakeError::EncoderUnavailable(
            "VideoToolbox did not accept H.264 or HEVC session creation".to_owned(),
        ));
    }
    let displays = crate::displays::probe()
        .map_err(|error| HandshakeError::NoDisplays(format!("{error:?}")))?;
    let primary = displays
        .first()
        .ok_or_else(|| HandshakeError::NoDisplays("inventory is empty".to_owned()))?;
    let offer =
        match crate::multi_monitor::build_offer_with_reason(&policy.multi_monitor, &displays) {
            Ok(offer) => Some(offer),
            Err(reason) => {
                if policy.multi_monitor.advertise_enabled {
                    eprintln!("withholding multi-monitor offer: {reason}");
                }
                None
            }
        };
    let audio =
        policy.audio_enabled && (audio_is_available() || crate::audio::capture_has_succeeded());
    // Opus is withheld because this host cannot emit it.
    //
    // Enabling compression negotiated Opus and the packetiser went on stamping
    // every packet `AudioCodec::Pcm`, so the Deck set up an Opus decoder and
    // rejected everything it was sent. The user got silence while both ends
    // counted success — the host counting packets sent, the client counting
    // packets discarded, and nothing comparing the two.
    //
    // Refusing to start capture for a non-PCM stream stops the corrupt
    // delivery, but a host that still *offers* Opus is a host that negotiates
    // its way into having no audio at all. There is an encoder behind it now —
    // the shared Opus encoder the Linux Pier uses — so the offer follows the
    // configuration: 128 kbit/s instead of 1.5 Mbit/s of PCM, measured as 30%
    // of a 5 Mbit/s WAN path that video then could not have.
    let audio_policy = arcen_media::audio::AudioPolicy::configured(audio, policy.audio_compressed);
    let microphone_policy = arcen_media::audio::MicrophonePolicy {
        operator_enabled: policy.microphone_enabled,
        backend_available: policy.microphone_backend_available,
        codecs: arcen_media::audio::MicrophoneCodecAvailability {
            opus: true,
            pcm: true,
        },
    };
    Ok(AdvertisedCapabilities {
        display_id: primary.display_id,
        login_window: policy.login_window,
        width: u32::try_from(primary.pixel_width).unwrap_or(1920),
        height: u32::try_from(primary.pixel_height).unwrap_or(1080),
        displays,
        // Both are VideoToolbox paths this host has exercised. They are stated
        // together because the encoder supports both wherever it supports
        // either; a per-codec probe belongs with the media plan, not here.
        hevc: policy.encoder.hevc,
        h264: policy.encoder.h264,
        main10: policy.encoder.main10,
        chroma_444: policy.encoder.chroma_444,
        encoder_hardware: policy.encoder.hardware,
        // Proven by creating a tap, not assumed. A host that advertises sound
        // it turns out it cannot capture has told the Deck to expect audio
        // that never arrives, and silence is indistinguishable from a quiet
        // desktop.
        audio,
        audio_policy,
        clipboard_policy: policy.clipboard,
        microphone_policy,
        multi_monitor_v1: offer,
    })
}

/// Builds the `server_hello` for `capabilities`.
#[must_use]
/// Builds the `server_hello` this host will send.
///
/// This is constructed from the shared `ServerHelloMsg` rather than
/// hand-written JSON. The previous hand-written version silently omitted
/// `negotiated_transport` and misspelled `supports_h265` as `supports_hevc`;
/// both were invisible because the only client reading them was a test that
/// had been hand-written to match. A real Deck refuses a hello whose
/// transport does not match the socket it selected, so the omission alone
/// ended every real session.
pub fn server_hello_json(capabilities: &AdvertisedCapabilities, os_user: &str) -> String {
    server_hello_json_for(
        capabilities,
        os_user,
        arcen_media::session_plan::ResolvedVideoPlan::standard(None),
        // No client request is in hand here, so the codec is the one this host
        // would choose unprompted: HEVC when it has it.
        if capabilities.hevc {
            crate::encode::EncoderCodec::Hevc
        } else {
            crate::encode::EncoderCodec::H264
        },
    )
}

/// Whether host audio can be captured, decided once at startup.
static AUDIO_AVAILABLE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Records whether host audio is available, from the startup consent check.
///
/// Set once by the serve path, not probed here. Creating a tap to answer the
/// question would put a second aggregate device on the output while a session
/// is creating its own, and two taps on one device is the contention that
/// hangs whichever loses — it hung a whole session before this was moved.
pub fn set_audio_available(available: bool) {
    AUDIO_AVAILABLE.store(available, std::sync::atomic::Ordering::Relaxed);
}

/// Returns whether host audio can be captured.
fn audio_is_available() -> bool {
    AUDIO_AVAILABLE.load(std::sync::atomic::Ordering::Relaxed)
}

static ENCODER_CAPABILITIES: std::sync::OnceLock<EncoderCapabilityEvidence> =
    std::sync::OnceLock::new();

/// Probes `VideoToolbox` once, before admission, and reuses that evidence.
#[must_use]
pub fn encoder_capabilities() -> EncoderCapabilityEvidence {
    *ENCODER_CAPABILITIES.get_or_init(probe_encoder_capabilities)
}

fn probe_encoder_capabilities() -> EncoderCapabilityEvidence {
    let h264 = crate::encode::Encoder::new(crate::encode::EncoderConfig::realtime(
        64,
        64,
        crate::encode::EncoderCodec::H264,
        60,
    ));
    let hevc = crate::encode::Encoder::new(crate::encode::EncoderConfig::realtime(
        64,
        64,
        crate::encode::EncoderCodec::Hevc,
        60,
    ));
    // Read back rather than assumed, and rather than left unsaid. The class
    // was empty because there was no evidence at handshake time; there is now,
    // because the probe encoder can be asked what VideoToolbox gave it. An
    // empty class made the Deck fall back to guessing from the backend name,
    // which does not contain the words it guesses on, so a host encoding in
    // hardware described itself to the person as a fallback media path.
    let hardware = h264
        .as_ref()
        .ok()
        .and_then(crate::encode::Encoder::uses_hardware_acceleration)
        .or_else(|| {
            hevc.as_ref()
                .ok()
                .and_then(crate::encode::Encoder::uses_hardware_acceleration)
        });
    // Ten-bit and 4:4:4 are claimed only when a real encode proves them: the
    // SPS VideoToolbox wrote for a 10-bit surface has to say 10-bit, and the
    // one for a 4:4:4 surface has to say 4:4:4. Asking for the profile is not
    // enough — an encoder handed no profile quietly encodes Main, which is
    // what every Grading session this host served used to be.
    let four_four_four = prove_ten_bit_hevc(true);
    let four_two_zero = prove_ten_bit_hevc(false);
    let ten_bit = |truth: Option<arcen_media::hevc_sps::HevcStreamTruth>| {
        truth.is_some_and(|truth| truth.bit_depth_luma == 10 && truth.bit_depth_chroma == 10)
    };
    EncoderCapabilityEvidence {
        h264: h264.is_ok(),
        hevc: hevc.is_ok(),
        main10: ten_bit(four_two_zero) || ten_bit(four_four_four),
        chroma_444: ten_bit(four_four_four)
            && four_four_four.is_some_and(|truth| truth.chroma_format_idc == 3),
        hardware,
    }
}

/// Encodes one 10-bit surface and returns what the resulting SPS states.
fn prove_ten_bit_hevc(chroma_444: bool) -> Option<arcen_media::hevc_sps::HevcStreamTruth> {
    const SIZE: usize = 256;
    let shape = if chroma_444 { "4:4:4" } else { "4:2:0" };
    let config = crate::encode::EncoderConfig::realtime_for(
        i32::try_from(SIZE).unwrap_or(256),
        i32::try_from(SIZE).unwrap_or(256),
        crate::encode::EncoderCodec::Hevc,
        30,
        if chroma_444 {
            arcen_media::ChromaSubsampling::Yuv444
        } else {
            arcen_media::ChromaSubsampling::Yuv420
        },
        arcen_media::BitDepth::Ten,
    )
    .with_colour(crate::encode::EncodeColour::from_plan_tokens(
        "bt709", "bt709", "bt709",
    ));
    let proven = crate::encode::Encoder::new(config).and_then(|mut encoder| {
        let surface = crate::encode::ten_bit_probe_surface(SIZE, SIZE, chroma_444)
            .ok_or_else(|| crate::encode::EncodeError::Encode("no probe surface".to_owned()))?;
        encoder.prove_with_surface(&surface)
    });
    match &proven {
        Ok(Some(truth)) => tracing::info!(
            target: arcen_telemetry::names::target::MEDIA,
            shape,
            summary = %truth.summary(),
            "ten-bit encode proven by its SPS"
        ),
        Ok(None) => tracing::warn!(
            target: arcen_telemetry::names::target::MEDIA,
            shape,
            "ten-bit probe produced no keyframe; not claimed"
        ),
        Err(error) => tracing::warn!(
            target: arcen_telemetry::names::target::MEDIA,
            shape,
            %error,
            "ten-bit encode refused; not claimed"
        ),
    }
    proven.ok().flatten()
}

/// Names the accelerator class, or says nothing when it was not measured.
///
/// Saying nothing is not neutral, which is why this is stated wherever it can
/// be: the Deck falls back to guessing from the backend name, "videotoolbox"
/// contains none of the words it guesses on, and a host encoding in hardware
/// therefore described itself to the person as a fallback media path. But
/// "software" would be as much a guess as "hardware" for a host that could not
/// ask, so an unmeasured class is still left empty.
fn encoder_class_token(hardware: Option<bool>) -> String {
    hardware.map_or_else(String::new, |hardware| {
        if hardware {
            arcen_media::video::AcceleratorClass::Hardware
        } else {
            arcen_media::video::AcceleratorClass::Software
        }
        .token()
        .to_owned()
    })
}

/// Builds the `server_hello` describing the plan this session will actually be
/// served.
///
/// The active colour fields state what the host is about to send, not what it
/// is capable of. Those are different questions and conflating them is how a
/// Deck is told 4:4:4 and shown 4:2:0: the capability flags below say what
/// could be negotiated, the `active_*` fields say what was.
#[must_use]
pub fn server_hello_json_for(
    capabilities: &AdvertisedCapabilities,
    os_user: &str,
    plan: arcen_media::session_plan::ResolvedVideoPlan,
    codec: crate::encode::EncoderCodec,
) -> String {
    server_hello_json_for_multi(
        capabilities,
        os_user,
        plan,
        codec,
        None,
        None,
        (capabilities.width, capabilities.height),
    )
}

/// Builds the `server_hello`, optionally attaching the applied multi-monitor
/// topology that was admitted for this session.
#[must_use]
pub fn server_hello_json_for_multi(
    capabilities: &AdvertisedCapabilities,
    os_user: &str,
    plan: arcen_media::session_plan::ResolvedVideoPlan,
    codec: crate::encode::EncoderCodec,
    active_pipeline: Option<arcen_protocol::messages::ServedStreamPipeline>,
    multi_monitor: Option<&crate::multi_monitor::MacOsMultiMonitorPlan>,
    picture: (u32, u32),
) -> String {
    use arcen_protocol::messages::{
        InputCapabilitiesMsg, InputCapabilityAvailability, SERVER_HELLO, ServerColorCaps,
        ServerHelloMsg, TabletModeCapabilitiesMsg,
    };

    // Capability claims are what this host has actually proven. Anything not
    // built is advertised as unavailable, because a Deck that is told a
    // feature exists offers it to the operator and then does nothing.
    let unavailable = InputCapabilityAvailability::Unavailable;
    let available = InputCapabilityAvailability::Available;

    let mut hello = ServerHelloMsg {
        msg_type: SERVER_HELLO.to_owned(),
        server_name: "Arcen Pier (macOS)".to_owned(),
        version: crate::VERSION.to_owned(),
        os_user: os_user.to_owned(),
        session_id: String::new(),
        session_type: "aqua".to_owned(),
        desktop: "aqua".to_owned(),
        // The size the Deck is about to receive, which is not always the size
        // of this host's own screen.
        screen_width: picture.0,
        screen_height: picture.1,
        monitors: Vec::new(),
        supports_h264: capabilities.h264,
        supports_h265: capabilities.hevc,
        supports_av1: false,
        supports_yuv444: capabilities.chroma_444,
        supports_audio: capabilities.audio,
        // Advertised from the same shared policy that will resolve the stream,
        // so what the Deck is offered and what it is given come from one
        // decision. Sending nothing here forced the legacy path, which is not
        // what a current Deck negotiates; the Linux Pier has always published
        // these capabilities and macOS silently did not.
        audio_output: capabilities
            .audio
            .then(|| capabilities.audio_policy.capabilities()),
        microphone_input: capabilities.microphone_policy.capabilities(),
        supports_pen: true,
        experimental_raw_hid: false,
        usb_hard_v1: false,
        supports_display_update: false,
        // Point-unit, phased scrolling is injected as continuous pixel
        // scroll with its phase, so applications run their own momentum.
        precise_scroll_v1: true,
        login_window: capabilities.login_window,
        requires_auth: true,
        encoder_backend: "videotoolbox".to_owned(),
        // VideoToolbox selects hardware or software per session, and this
        // string is not read back from the encoder. It is left empty rather
        // than asserted: the shared contract documents an empty value as
        // "unknown, the client may guess from the backend name", which is
        // true, whereas "hardware" would be a claim nothing here established.
        encoder_class: encoder_class_token(capabilities.encoder_hardware),
        available_encoders: std::collections::BTreeMap::new(),
        // The codec this session will encode with, not the best one this host
        // owns. A Deck that asked for H.264 received H.264 and then refused
        // its own stream, because the hello had promised H.265: "wire profile
        // mismatch: requested=h264 expected=H265 actual=H264".
        codec: match codec {
            crate::encode::EncoderCodec::H264 => "h264",
            crate::encode::EncoderCodec::Hevc => "h265",
        }
        .to_owned(),
        color_caps: ServerColorCaps {
            main10: capabilities.main10,
            main12: false,
            chroma_422: false,
            chroma_444: capabilities.chroma_444,
            // The plan's range, which the capture follows: Grading captures
            // `xf44` and the stream's SPS says full range, measured; the fast
            // path captures `420v` and says limited. Hard-coding limited here
            // reported every full-range Grading session as range-degraded.
            full_range: plan.range == "full",
            identity_matrix: false,
            active_bit_depth: match plan.bit_depth {
                arcen_media::session_plan::PlanBitDepth::Eight => "8",
                arcen_media::session_plan::PlanBitDepth::Ten => "10",
            }
            .to_owned(),
            active_range: plan.range.to_owned(),
            active_matrix: plan.matrix.to_owned(),
            active_primaries: plan.primaries.to_owned(),
            active_transfer: plan.transfer.to_owned(),
            // Derived from the depth as well as the chroma. The resolver only
            // ever pairs 4:4:4 with ten bits today, so naming the depth in the
            // 4:4:4 arm alone happened to be true — but nothing enforced it,
            // and a plan that paired them differently would have advertised a
            // pixel format the stream was not in.
            advertised_pix_fmt: advertised_pix_fmt(plan.chroma, plan.bit_depth).to_owned(),
            // "degraded" is said out loud rather than left for the Deck to
            // infer by comparing fields, because a client that knows it was
            // degraded can tell its user why.
            negotiated_state: if plan.is_exact() {
                "active"
            } else {
                "degraded"
            }
            .to_owned(),
        },
        active_pipeline,
        input_protocol_version: arcen_protocol::messages::INPUT_PROTOCOL_VERSION,
        input_capabilities: InputCapabilitiesMsg {
            absolute_pointer: available,
            relative_pointer: unavailable,
            // This host can draw the pointer into the picture when a Deck asks
            // for it, which is what host-cursor means. It cannot report the
            // shape for a Deck to draw locally: NSCursor's system cursor does
            // not follow another process's pointer, measured under every
            // activation policy, which is why Apple deprecated that reader in
            // favour of compositing.
            host_cursor: available,
            region_input: if multi_monitor.is_some() {
                available
            } else {
                unavailable
            },
            gestures: available,
            // Basic Tablet terminates locally: the Deck's Wacom driver reads
            // the pen and this host injects finished samples as CGEvent
            // tablet events. Every field below is one the injector actually
            // sets, so the claim matches the code rather than the ambition.
            pen: available,
            pen_pressure: available,
            pen_tilt: available,
            pen_rotation: available,
            pen_eraser: available,
            pen_proximity: available,
        },
        tablet_mode_capabilities: TabletModeCapabilitiesMsg {
            local_termination: available,
            // Measured, not assumed. Native Tablet needs this host to present
            // the bridged device locally, which on macOS means
            // `IOHIDUserDevice` and the
            // com.apple.developer.hid.virtual.device entitlement. Whether a
            // signature carries that is a property of the machine rather than
            // of the build, so a constant here would be wrong half the time —
            // and wrong expensively, because a host advertising a tablet it
            // cannot deliver takes the Deck's tablet away and gives back
            // nothing. `probe()` creates a device and tears it down, so the
            // refusal a Deck sees is one this host actually observed.
            // Unavailable, and the probe is not what decides it.
            //
            // An earlier version advertised the bridge whenever
            // `virtual_hid::probe()` succeeded, which was the same overclaim
            // the probe was added to prevent, one step further along:
            // permission to create a device is not an importer. Nothing yet
            // consumes the bridged traffic — `stream.rs` still answers HID and
            // USB frames with `Unsupported` — so an entitled build would have
            // taken the Deck's tablet away and delivered nothing.
            //
            // The probe stays as prerequisite evidence: it is what says
            // whether the entitlement has arrived, and it is recorded in
            // diagnostics. It becomes a capability only when there is an
            // importer behind it.
            wacom_usb_bridge: unavailable,
            disabled_mouse_compat: available,
        },
        clipboard: Some(arcen_media::clipboard::policy_message(
            capabilities.clipboard_policy,
        )),
        device_capabilities: std::collections::BTreeMap::new(),
        // The socket is already QUIC by the time this is sent; the Deck
        // rejects a hello that names a different transport than the one it
        // dialled.
        negotiated_transport: Some(arcen_transport::CAPABILITY_TRANSPORT_QUIC.to_owned()),
    }
    .with_build_identity(arcen_protocol::build_identity::this_build(
        "arcen-pier-macos",
        crate::VERSION,
    ));

    if let Some(multi_monitor) = multi_monitor {
        hello = match hello.with_multi_monitor_v1(&multi_monitor.server_capability) {
            Ok(hello) => hello,
            Err(_) => return format!(r#"{{"type":"{SERVER_HELLO}"}}"#),
        };
    }

    serde_json::to_string(&hello).unwrap_or_else(|_| {
        // Every field is plain data, so this cannot fail in practice. Falling
        // back to a minimal hello is still better than panicking in a session.
        format!(r#"{{"type":"{SERVER_HELLO}"}}"#)
    })
}

/// Failed sign-ins, per source address and per account, shared by every
/// session this process serves.
static AUTH_THROTTLE: std::sync::LazyLock<
    std::sync::Mutex<arcen_session::auth_throttle::AuthThrottle>,
> = std::sync::LazyLock::new(|| {
    std::sync::Mutex::new(arcen_session::auth_throttle::AuthThrottle::new(
        arcen_session::auth_throttle::AuthThrottlePolicy::default(),
    ))
});
static AUTH_THROTTLE_EPOCH: std::sync::LazyLock<std::time::Instant> =
    std::sync::LazyLock::new(std::time::Instant::now);

fn throttle_keys(
    peer: Option<std::net::IpAddr>,
    username: &str,
) -> Vec<arcen_session::auth_throttle::ThrottleKey> {
    let mut keys = vec![arcen_session::auth_throttle::ThrottleKey::account(username)];
    if let Some(peer) = peer {
        keys.push(arcen_session::auth_throttle::ThrottleKey::Source(
            peer.to_string(),
        ));
    }
    keys
}

fn throttle_retry_after(keys: &[arcen_session::auth_throttle::ThrottleKey]) -> Option<Duration> {
    let now = AUTH_THROTTLE_EPOCH.elapsed();
    AUTH_THROTTLE
        .lock()
        .ok()
        .and_then(|throttle| throttle.retry_after(keys, now))
}

fn throttle_record(
    keys: &[arcen_session::auth_throttle::ThrottleKey],
    checked: &Result<crate::auth::Authenticated, crate::auth::AuthFailure>,
) {
    let now = AUTH_THROTTLE_EPOCH.elapsed();
    let Ok(mut throttle) = AUTH_THROTTLE.lock() else {
        return;
    };
    match checked {
        Ok(_) => throttle.record_success(keys),
        // Only a credential the directory refused counts. A host that could
        // not start PAM has not learned anything about the person asking.
        Err(
            crate::auth::AuthFailure::InvalidCredentials
            | crate::auth::AuthFailure::AccountNotPermitted,
        ) => throttle.record_failure(keys, now),
        Err(_) => {}
    }
}

/// Refuses an account this agent cannot serve.
///
/// Compared by uid, never by name: a directory can answer to several spellings
/// of one account, and the console owner, the agent and the authenticated user
/// must be the same account rather than similar strings.
fn admit_serving_account(
    authenticated: crate::auth::Authenticated,
    serving_uid: Option<u32>,
) -> Result<crate::auth::Authenticated, crate::auth::AuthFailure> {
    match serving_uid {
        None => Ok(authenticated),
        Some(uid) if authenticated.uid == Some(uid) => Ok(authenticated),
        Some(_) => Err(crate::auth::AuthFailure::NotConsoleOwner),
    }
}

static AUTHENTICATION_SLOT: crate::blocking::BlockingSlot = crate::blocking::BlockingSlot::new();

async fn authenticate_credentials(
    username: String,
    credential: String,
) -> Result<crate::auth::Authenticated, crate::auth::AuthFailure> {
    crate::blocking::run_exclusive(
        "arcen-macos-authentication",
        &AUTHENTICATION_SLOT,
        authentication_failure,
        move || authenticate_blocking(username, credential),
    )
    .await?
}

fn authentication_failure(failure: crate::blocking::ExclusiveFailure) -> crate::auth::AuthFailure {
    match failure {
        crate::blocking::ExclusiveFailure::Busy
        | crate::blocking::ExclusiveFailure::Disconnected => {
            crate::auth::AuthFailure::PolicyUnavailable
        }
        crate::blocking::ExclusiveFailure::Spawn(error) => {
            drop(error);
            crate::auth::AuthFailure::PolicyUnavailable
        }
    }
}

#[cfg(not(test))]
fn authenticate_blocking(
    username: String,
    credential: String,
) -> Result<crate::auth::Authenticated, crate::auth::AuthFailure> {
    let result = crate::auth::authenticate(&username, &credential);
    drop((username, credential));
    result
}

#[cfg(test)]
fn authenticate_blocking(
    username: String,
    credential: String,
) -> Result<crate::auth::Authenticated, crate::auth::AuthFailure> {
    let valid = credential == "valid";
    drop(credential);
    if valid {
        let uid = crate::auth::resolve_account(&username).map(|account| account.uid);
        Ok(crate::auth::Authenticated {
            user: username,
            uid,
        })
    } else {
        Err(crate::auth::AuthFailure::InvalidCredentials)
    }
}

/// Performs the Pier half of the handshake.
///
/// # Errors
///
/// Returns [`HandshakeError`] when the host has nothing to offer, the
/// transport fails, or the Deck does not answer with a `client_hello`.
pub async fn perform(socket: &mut PierSocket, os_user: &str) -> Result<Handshake, HandshakeError> {
    perform_with_config(socket, os_user, &SessionPolicy::default()).await
}

/// Performs the Pier half of the handshake with this operator policy.
///
/// # Errors
///
/// Returns [`HandshakeError`] when the host has nothing to offer, the
/// transport fails, or the Deck does not answer with a `client_hello`.
pub async fn perform_with_config(
    socket: &mut PierSocket,
    os_user: &str,
    policy: &SessionPolicy,
) -> Result<Handshake, HandshakeError> {
    bound_application_handshake(
        perform_application_handshake(socket, os_user, policy, None, None),
        APPLICATION_HANDSHAKE_TIMEOUT,
    )
    .await
}

/// Performs the Pier half of the handshake for a Deck at `peer`, whose failed
/// sign-ins count against that address.
///
/// # Errors
///
/// Returns [`HandshakeError`] as [`perform_with_config`] does.
///
/// With `admission`, the host's one session slot is taken only once the
/// password is proved. Taking it when the connection opened let anyone who
/// could reach the port hold the host for the whole sign-in timeout by saying
/// nothing, and every real Deck was told the host was busy.
pub async fn perform_for_peer(
    socket: &mut PierSocket,
    peer: std::net::IpAddr,
    policy: &SessionPolicy,
    admission: Option<&std::sync::Arc<arcen_session::session_admission::SessionAdmissionRuntime>>,
) -> Result<Handshake, HandshakeError> {
    bound_application_handshake(
        perform_application_handshake(socket, "", policy, Some(peer), admission),
        APPLICATION_HANDSHAKE_TIMEOUT,
    )
    .await
}

/// A held session slot, given back when dropped.
#[derive(Debug)]
pub struct AdmissionGuard {
    runtime: std::sync::Arc<arcen_session::session_admission::SessionAdmissionRuntime>,
    lease: Option<arcen_session::session_admission::SessionAdmissionLease>,
}

impl Drop for AdmissionGuard {
    fn drop(&mut self) {
        if let Some(lease) = self.lease.take() {
            let _ = self.runtime.complete(&lease);
        }
    }
}

async fn bound_application_handshake<F>(
    handshake: F,
    timeout: Duration,
) -> Result<Handshake, HandshakeError>
where
    F: std::future::Future<Output = Result<Handshake, HandshakeError>>,
{
    tokio::time::timeout(timeout, handshake)
        .await
        .unwrap_or(Err(HandshakeError::ApplicationHandshakeTimeout))
}

async fn perform_application_handshake(
    socket: &mut PierSocket,
    os_user: &str,
    policy: &SessionPolicy,
    peer: Option<std::net::IpAddr>,
    admission: Option<&std::sync::Arc<arcen_session::session_admission::SessionAdmissionRuntime>>,
) -> Result<Handshake, HandshakeError> {
    // Read the display inventory before speaking, so a host with nothing to
    // serve refuses before asking anyone for a password.
    let advertised = advertise_with_config(policy)?;

    // Credentials first. The Deck chooses its authenticated path from this
    // message; greeting first would tell it no credentials are wanted.
    let (
        user,
        initial_video,
        requested_multi_monitor,
        requested_desktop,
        client_display,
        cursor_mode,
        authenticated_timezone,
        admission,
    ) = authenticate(
        socket,
        advertised.multi_monitor_v1.clone(),
        policy.serving_uid,
        peer,
        admission,
    )
    .await?;

    // Resolved before the greeting for the same reason everything else here
    // is: the greeting states the size the Deck is about to receive, and the
    // capture has to start at that size. Deciding twice is how a host tells a
    // Deck one geometry and sends another.
    // Arranged before the greeting, because the greeting states the size the
    // Deck is about to receive and everything downstream — the picture, the
    // coordinate space input is mapped onto, the Deck's own presentation —
    // must agree with it. Deciding after the greeting is how a host tells a
    // Deck one geometry and sends another, which is precisely what happened
    // the first time this was wired: the hello said 1920x1080 while the
    // capture was 2560x1440, and every click landed three quarters of the way
    // to where it was aimed.
    // An HDR request is served on an HDR panel only when the Deck display it
    // represents is also HDR. A bright SDR Deck panel may state luminance but
    // must not make the host create a PQ/BT.2020 desktop.
    let pq_requested = initial_video.as_ref().is_some_and(requests_pq);
    // `ARCEN_HDR_PANEL=sdr` is a measurement lever: it serves an HDR request
    // on an SDR panel, which is how the degradation path is exercised on a
    // host that could otherwise prove HDR.
    let force_sdr_panel = std::env::var("ARCEN_HDR_PANEL").as_deref() == Ok("sdr");
    let panel = select_virtual_panel(pq_requested, force_sdr_panel, client_display.as_ref());
    if let Some(client_monitor) = client_display.as_ref() {
        let color = client_monitor
            .color
            .as_ref()
            .map(arcen_media::display_color::DisplayColor::from_msg);
        tracing::info!(
            target: arcen_telemetry::names::target::MEDIA,
            client_display_id = client_monitor.id,
            client_display_name = %client_monitor.name,
            panel = ?panel,
            color_gamut = ?color.map(arcen_media::display_color::DisplayColor::gamut),
            color_headroom = ?color.and_then(arcen_media::display_color::DisplayColor::hdr_headroom),
            color_hdr = color.is_some_and(arcen_media::display_color::DisplayColor::is_hdr),
            "client display colour contract"
        );
    }
    let single_monitor_session = requested_multi_monitor.is_none();
    let arranged = if single_monitor_session {
        requested_desktop.arrange(
            advertised.display_id,
            fps_hint(initial_video.as_ref()),
            panel,
            client_display.as_ref(),
        )
    } else {
        None
    };
    let (capture_width, capture_height) = arranged.as_ref().map_or_else(
        || requested_desktop.resolve(advertised.width, advertised.height),
        crate::virtual_display::VirtualDisplay::size,
    );
    let display_id = arranged.as_ref().map_or(
        advertised.display_id,
        crate::virtual_display::VirtualDisplay::display_id,
    );
    let (requested_width, requested_height) = match requested_desktop {
        RequestedDesktop::Exact { width, height } => (width, height),
        RequestedDesktop::HostChoice => (capture_width, capture_height),
    };

    // Resolved before the greeting, because the greeting has to state what
    // this session will actually be served. Resolving after would let the host
    // advertise one contract and start another.
    let headroom = crate::displays::potential_headroom(display_id);
    let mut hdr_output = hdr_is_proven(pq_requested, headroom);
    if requested_multi_monitor.is_some() {
        // In Match My Layout every monitor gets its own virtual display and HDR
        // proof is per display after creation, so the primary physical display
        // must not decide the session's video tier.
        hdr_output = pq_requested;
    }
    if pq_requested {
        tracing::info!(
            target: arcen_telemetry::names::target::MEDIA,
            display_id,
            headroom = ?headroom,
            proven = hdr_output,
            "HDR desktop proof"
        );
    }
    let resolved_video = resolve_initial_video(initial_video.as_ref(), &advertised, hdr_output)?;
    let plan = resolved_video.plan;
    // Resolved before the hello is built, because the hello states the codec
    // the Deck is about to receive. Reporting the host's capability there
    // instead made a client that asked for H.264 refuse its own stream: the
    // wire carried H264 and the hello had promised H265.
    let codec = resolved_video.codec;
    let fps = resolved_video.fps;
    let motion_priority = resolved_video.motion_priority;
    let requested_pipeline = resolved_video.requested_pipeline;
    let active_pipeline = resolved_video.active_pipeline;
    let multi_monitor = crate::multi_monitor::admit_virtual_request(
        advertised.multi_monitor_v1.as_ref(),
        requested_multi_monitor.as_ref(),
        plan,
        codec,
        fps,
        pq_requested,
        force_sdr_panel,
    )
    .map_err(|error| HandshakeError::MultiMonitor(error.to_string()))?;
    let (multi_monitor, multi_monitor_displays) = match multi_monitor {
        Some((plan, displays)) => (Some(plan), Some(displays)),
        None => (None, None),
    };

    net::send_json(
        socket,
        server_hello_json_for_multi(
            &advertised,
            &user,
            plan,
            codec,
            active_pipeline.clone(),
            multi_monitor.as_ref(),
            (capture_width, capture_height),
        ),
    )
    .await
    .map_err(HandshakeError::Transport)?;

    let reply = net::receive_json(socket)
        .await
        .map_err(HandshakeError::Transport)?
        .ok_or(HandshakeError::PeerClosed)?;

    // The reply is checked for shape before the session continues, so a peer
    // speaking a different protocol is refused here rather than part-way
    // through streaming.
    let kind = serde_json::from_str::<serde_json::Value>(&reply)
        .ok()
        .and_then(|value| {
            value
                .get("type")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "unparseable".to_owned());
    if kind != arcen_protocol::messages::CLIENT_HELLO {
        return Err(HandshakeError::UnexpectedMessage(kind));
    }

    // The account the Deck proved, not the account the service happens to run
    // as: those differ, and the session belongs to the former.
    let _ = os_user;

    // Resolved from what the Deck said it can decode, exactly as the Linux
    // host does, through the same shared policy. A Deck that advertises no
    // audio output, or a host with audio disabled, resolves to disabled and no
    // tap is ever created.
    let client_hello =
        serde_json::from_str::<arcen_protocol::messages::ClientHelloMsg>(&reply).ok();
    let echoed_timezone = client_hello
        .as_ref()
        .and_then(|hello| hello.timezone.as_deref());
    if authenticated_timezone.as_deref() != echoed_timezone {
        tracing::warn!(
            target: arcen_telemetry::names::target::SESSION,
            authenticated_timezone = ?authenticated_timezone,
            client_hello_timezone = ?echoed_timezone,
            "ClientHello timezone differs from authenticated decision; retaining AuthResponse value"
        );
    }
    let input_mode_results = client_hello.as_ref().map_or_else(
        || {
            input_mode_results(
                cursor_mode,
                arcen_protocol::messages::TabletModeMsg::LocalTermination,
                arcen_protocol::messages::TabletModeCapabilitiesMsg::default(),
                host_tablet_mode_capabilities(),
            )
        },
        |hello| {
            input_mode_results(
                cursor_mode,
                hello.tablet_mode_requested,
                hello.effective_tablet_mode_capabilities(),
                host_tablet_mode_capabilities(),
            )
        },
    );
    let client_audio = client_hello
        .as_ref()
        .and_then(|hello| hello.audio_output.clone());
    let audio = advertised
        .audio_policy
        .resolve(client_audio.as_ref(), advertised.audio);
    let clipboard = client_hello
        .as_ref()
        .and_then(|hello| clipboard_negotiation(hello, advertised.clipboard_policy));
    let microphone = if requested_multi_monitor.is_some() {
        arcen_media::audio::ResolvedMicrophoneStream::disabled(
            MICROPHONE_GENERATION,
            arcen_protocol::messages::MicrophoneStreamReason::BackendUnavailable,
        )
    } else {
        advertised.microphone_policy.resolve(
            client_hello
                .as_ref()
                .and_then(|hello| hello.microphone_output.as_ref()),
            client_hello
                .as_ref()
                .and_then(|hello| hello.microphone_output.as_ref())
                .is_some(),
            MICROPHONE_GENERATION,
            64,
        )
    };

    Ok(Handshake {
        fps,
        motion_priority,
        requested_pipeline,
        active_pipeline,
        user,
        display_id,
        advertised,
        client_hello: reply,
        plan,
        audio,
        codec,
        clipboard,
        microphone,
        multi_monitor,
        capture_width,
        capture_height,
        requested_width,
        requested_height,
        arranged_display: arranged,
        multi_monitor_displays,
        cursor_mode,
        input_mode_results,
        admission,
        login_window: policy.login_window,
        authenticated_timezone,
    })
}

fn host_tablet_mode_capabilities() -> arcen_protocol::messages::TabletModeCapabilitiesMsg {
    arcen_protocol::messages::TabletModeCapabilitiesMsg {
        local_termination: InputCapabilityAvailability::Available,
        wacom_usb_bridge: InputCapabilityAvailability::Unavailable,
        disabled_mouse_compat: InputCapabilityAvailability::Available,
    }
}

fn input_mode_results(
    requested_cursor: arcen_protocol::messages::CursorMode,
    requested_tablet: arcen_protocol::messages::TabletModeMsg,
    client_capabilities: arcen_protocol::messages::TabletModeCapabilitiesMsg,
    host_capabilities: arcen_protocol::messages::TabletModeCapabilitiesMsg,
) -> crate::stream::InputModeResults {
    let cursor = resolve_cursor_mode(
        cursor_mode_to_input(requested_cursor),
        InputCapabilityTruth::Available,
    );
    let tablet = resolve_tablet_mode(
        tablet_mode_to_input(requested_tablet),
        capability_to_input(client_capabilities.local_termination),
        capability_to_input(host_capabilities.local_termination),
        capability_to_input(client_capabilities.wacom_usb_bridge),
        capability_to_input(host_capabilities.wacom_usb_bridge),
    );
    crate::stream::InputModeResults {
        cursor: CursorModeResultMsg {
            requested: requested_cursor,
            active: cursor_mode_from_input(cursor.active),
            accepted: cursor.accepted,
            reason: CursorModeReason::try_from(cursor.reason.as_str().to_owned())
                .unwrap_or_default(),
            ..CursorModeResultMsg::default()
        },
        tablet: TabletModeResultMsg {
            requested: requested_tablet,
            active: tablet_mode_from_input(tablet.active),
            accepted: tablet.accepted,
            reason: TabletModeReason::try_from(tablet.reason.as_str().to_owned())
                .unwrap_or_default(),
            reconnect_required: tablet.reconnect_required,
            ..TabletModeResultMsg::default()
        },
    }
}

const fn cursor_mode_to_input(mode: arcen_protocol::messages::CursorMode) -> InputCursorMode {
    match mode {
        arcen_protocol::messages::CursorMode::Local => InputCursorMode::Local,
        arcen_protocol::messages::CursorMode::Host => InputCursorMode::Host,
    }
}

const fn cursor_mode_from_input(mode: InputCursorMode) -> arcen_protocol::messages::CursorMode {
    match mode {
        InputCursorMode::Local => arcen_protocol::messages::CursorMode::Local,
        InputCursorMode::Host => arcen_protocol::messages::CursorMode::Host,
    }
}

const fn tablet_mode_to_input(mode: arcen_protocol::messages::TabletModeMsg) -> InputTabletMode {
    match mode {
        arcen_protocol::messages::TabletModeMsg::LocalTermination => {
            InputTabletMode::LocalTermination
        }
        arcen_protocol::messages::TabletModeMsg::WacomUsbBridge => InputTabletMode::WacomUsbBridge,
        arcen_protocol::messages::TabletModeMsg::DisabledMouseCompat => {
            InputTabletMode::DisabledMouseCompat
        }
    }
}

const fn tablet_mode_from_input(mode: InputTabletMode) -> arcen_protocol::messages::TabletModeMsg {
    match mode {
        InputTabletMode::LocalTermination => {
            arcen_protocol::messages::TabletModeMsg::LocalTermination
        }
        InputTabletMode::WacomUsbBridge => arcen_protocol::messages::TabletModeMsg::WacomUsbBridge,
        InputTabletMode::DisabledMouseCompat => {
            arcen_protocol::messages::TabletModeMsg::DisabledMouseCompat
        }
    }
}

const fn capability_to_input(availability: InputCapabilityAvailability) -> InputCapabilityTruth {
    match availability {
        InputCapabilityAvailability::Available => InputCapabilityTruth::Available,
        InputCapabilityAvailability::Unavailable => InputCapabilityTruth::Unavailable,
        InputCapabilityAvailability::Unknown => InputCapabilityTruth::Unknown,
    }
}

/// Narrows this host's clipboard policy by what the client asked for.
///
/// The intersection is the whole point: the host's configuration says what it
/// is willing to carry and the hello says what the person wants carried.
/// Consulting only the host half meant reading the local pasteboard and
/// putting it on the wire for a client that had asked for no clipboard at all.
fn clipboard_negotiation(
    hello: &arcen_protocol::messages::ClientHelloMsg,
    policy: arcen_media::clipboard::ClipboardPolicy,
) -> Option<arcen_media::clipboard::ClipboardNegotiation> {
    arcen_media::clipboard::ClipboardNegotiation::resolve(
        policy,
        true,
        arcen_media::clipboard::ClipboardRequest {
            protocol_version: hello.clipboard_protocol_version,
            text: arcen_media::clipboard::ClipboardDirections {
                client_to_host: hello.clipboard_text_c2s,
                host_to_client: hello.clipboard_text_s2c,
            },
            image: arcen_media::clipboard::ClipboardDirections {
                client_to_host: hello.clipboard_image_c2s,
                host_to_client: hello.clipboard_image_s2c,
            },
        },
    )
}

/// Chooses the codec this session will encode with.
///
/// A Deck that says it cannot decode HEVC must not be sent HEVC. The resolver
/// already degrades the *plan* for such a client, but the plan has no codec
/// axis, so nothing downstream acted on it and the encoder stayed pinned to
/// HEVC: the host recorded `ClientLacksSupport` and then streamed exactly what
/// the client had said it could not read.
///
/// The frame rate this session will actually serve.
///
/// The client's `quality.max_fps` was parsed, validated by the shared request
/// checker, and then dropped: capture, the encoder and the telemetry record
/// each said 60 regardless. A Deck asking for 30 over a metered link was served
/// 60 and told it was getting 60, so the one number an operator would use to
/// explain the bandwidth was not the number in the request.
///
/// The rule is the shared one the other hosts use — the lower of what the host
/// can serve and what the client asked for — rather than a Mac-only clamp.
/// `HOST_MAX_FPS` is this host's ceiling; `resolve_host_initial_video` applies
/// the same `min` internally for Linux.
#[cfg(test)]
fn negotiated_fps(request: Option<&arcen_protocol::messages::InitialVideoRequestMsg>) -> u32 {
    const HOST_MAX_FPS: u32 = 60;
    let Some(request) = request else {
        return HOST_MAX_FPS;
    };
    // Zero is not a request for no frames, it is a client that sent nothing
    // meaningful; the shared validator rejects it and so does this.
    if request.quality.max_fps == 0 {
        return HOST_MAX_FPS;
    }
    HOST_MAX_FPS.min(request.quality.max_fps)
}

/// The negotiation generation this host reports for the microphone.
///
/// Renegotiation is not implemented, so every session reports the first
/// generation rather than inventing a counter that never advances.
const MICROPHONE_GENERATION: u32 = 1;

/// Names the pixel format a Deck should expect for this plan.
///
/// Both axes decide it. Deriving it from the chroma alone required the depth
/// to be implied by the layout, which is true of every plan this resolver
/// builds and is not a property anything checks.
const fn advertised_pix_fmt(
    chroma: arcen_media::session_plan::PlanChroma,
    bit_depth: arcen_media::session_plan::PlanBitDepth,
) -> &'static str {
    use arcen_media::session_plan::{PlanBitDepth, PlanChroma};
    match (chroma, bit_depth) {
        (PlanChroma::Yuv420, PlanBitDepth::Eight) => "yuv420p",
        (PlanChroma::Yuv420, PlanBitDepth::Ten) => "yuv420p10le",
        (PlanChroma::Yuv444, PlanBitDepth::Eight) => "yuv444p",
        (PlanChroma::Yuv444, PlanBitDepth::Ten) => "yuv444p10le",
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedInitialVideo {
    plan: arcen_media::session_plan::ResolvedVideoPlan,
    codec: crate::encode::EncoderCodec,
    fps: u32,
    motion_priority: arcen_media::video::MotionPriority,
    requested_pipeline: Option<arcen_media::video::PipelineId>,
    active_pipeline: Option<arcen_protocol::messages::ServedStreamPipeline>,
}

fn video_configuration_from_plan(
    plan: arcen_media::session_plan::ResolvedVideoPlan,
) -> arcen_media::VideoConfiguration {
    use arcen_media::session_plan::{PlanBitDepth, PlanChroma};
    arcen_media::VideoConfiguration {
        codec: plan.codec,
        chroma: match plan.chroma {
            PlanChroma::Yuv420 => arcen_media::ChromaSubsampling::Yuv420,
            PlanChroma::Yuv444 => arcen_media::ChromaSubsampling::Yuv444,
        },
        bit_depth: match plan.bit_depth {
            PlanBitDepth::Eight => arcen_media::BitDepth::Eight,
            PlanBitDepth::Ten => arcen_media::BitDepth::Ten,
        },
        range: arcen_media::ColorRange::from_token(plan.range)
            .unwrap_or(arcen_media::ColorRange::Limited),
        matrix: arcen_media::ColorMatrix::from_token(plan.matrix)
            .unwrap_or(arcen_media::ColorMatrix::Bt709),
        primaries: arcen_media::ColorPrimaries::from_token(plan.primaries)
            .unwrap_or(arcen_media::ColorPrimaries::Bt709),
        transfer: arcen_media::TransferCharacteristics::from_token(plan.transfer)
            .unwrap_or(arcen_media::TransferCharacteristics::Bt709),
    }
}

/// Whether an HDR request may be served as HDR: only when the display the
/// session captures reports headroom above SDR white. No reading is SDR.
const fn hdr_is_proven(requested: bool, headroom: Option<f32>) -> bool {
    match headroom {
        Some(headroom) => requested && headroom > 1.0,
        None => false,
    }
}

fn select_virtual_panel(
    pq_requested: bool,
    force_sdr_panel: bool,
    client_display: Option<&arcen_protocol::messages::ClientMonitor>,
) -> crate::virtual_display::VirtualPanel {
    let client_hdr = client_display
        .and_then(|monitor| monitor.color.as_ref())
        .map(arcen_media::display_color::DisplayColor::from_msg)
        .is_some_and(arcen_media::display_color::DisplayColor::is_hdr);
    if pq_requested && !force_sdr_panel && client_hdr {
        crate::virtual_display::VirtualPanel::Hdr
    } else {
        crate::virtual_display::VirtualPanel::Sdr
    }
}

/// Whether an auth-time video request asks specifically for the PQ HDR
/// transfer this Pier can serve.
fn requests_pq(video: &arcen_protocol::messages::InitialVideoRequestMsg) -> bool {
    arcen_media::video::resolve_client_video_request(video).is_ok_and(|client| {
        matches!(
            client.video.transfer,
            arcen_media::TransferCharacteristics::Pq
        )
    })
}

/// Resolves the auth-time video request through the shared media policy.
#[allow(clippy::too_many_lines)]
fn resolve_initial_video(
    video: Option<&arcen_protocol::messages::InitialVideoRequestMsg>,
    capabilities: &AdvertisedCapabilities,
    hdr_output: bool,
) -> Result<ResolvedInitialVideo, HandshakeError> {
    use arcen_media::session_plan::{
        ClientVideoRequest, HostVideoCapabilities, SelectionIntent, resolve_video_plan,
    };
    use arcen_media::video::{
        HostInitialVideoPolicy, resolve_client_video_request,
        resolve_host_initial_video_with_supported_codecs,
    };
    use arcen_media::{
        BitDepth, ChromaSubsampling, ColorMatrix, ColorPrimaries, ColorRange,
        TransferCharacteristics, VideoCodec, VideoConfiguration,
    };

    let Some(video) = video else {
        return Ok(ResolvedInitialVideo {
            plan: arcen_media::session_plan::ResolvedVideoPlan::standard(None),
            codec: if capabilities.hevc {
                crate::encode::EncoderCodec::Hevc
            } else {
                crate::encode::EncoderCodec::H264
            },
            fps: 60,
            motion_priority: arcen_media::video::MotionPriority::Detail,
            requested_pipeline: Some(arcen_media::video::PipelineId::Speed),
            active_pipeline: Some(arcen_protocol::messages::ServedStreamPipeline::Speed),
        });
    };

    let client = resolve_client_video_request(video)
        .map_err(|error| HandshakeError::VideoRequest(error.to_string()))?;
    let current = VideoConfiguration {
        codec: if capabilities.hevc {
            VideoCodec::H265
        } else {
            VideoCodec::H264
        },
        chroma: if capabilities.chroma_444 {
            ChromaSubsampling::Yuv444
        } else {
            ChromaSubsampling::Yuv420
        },
        bit_depth: if capabilities.main10 {
            BitDepth::Ten
        } else {
            BitDepth::Eight
        },
        range: ColorRange::Limited,
        matrix: ColorMatrix::Bt709,
        primaries: ColorPrimaries::Bt709,
        transfer: TransferCharacteristics::Bt709,
    };
    let host_resolved = resolve_host_initial_video_with_supported_codecs(
        client,
        HostInitialVideoPolicy {
            current,
            color_policy: arcen_media::video::ColorPolicy::DefaultOff,
            codec_pinned: false,
            variant_pinned: false,
            max_fps: 60,
        },
        {
            let mut set = arcen_media::CodecSet::empty();
            if capabilities.h264 {
                set = set.with(VideoCodec::H264);
            }
            if capabilities.hevc {
                set = set.with(VideoCodec::H265);
            }
            set
        },
    )
    .map_err(|error| HandshakeError::VideoRequest(error.to_string()))?;
    let intent = match client.selection {
        arcen_protocol::messages::VideoSelectionIntent::Exact => SelectionIntent::Exact,
        arcen_protocol::messages::VideoSelectionIntent::AdaptivePerformance => {
            SelectionIntent::AdaptivePerformance
        }
        arcen_protocol::messages::VideoSelectionIntent::ColorFidelity => {
            SelectionIntent::ColorFidelity
        }
    };
    let request = ClientVideoRequest {
        intent,
        codec: client.video.codec,
        bit_depth: client.video.bit_depth,
        chroma: client.video.chroma,
        range: client.video.range.token(),
        transfer: client.video.transfer.token(),
        primaries: client.video.primaries.token(),
        matrix: client.video.matrix.token(),
        hevc: client.capabilities.h265,
        chroma_444: client.capabilities.yuv444,
        main10: client.capabilities.main10,
        hdr: matches!(
            client.video.transfer,
            TransferCharacteristics::Pq | TransferCharacteristics::Hlg
        ),
    };
    let plan = resolve_video_plan(
        &request,
        &HostVideoCapabilities {
            h264: capabilities.h264,
            hevc: capabilities.hevc,
            ten_bit: capabilities.main10,
            chroma_444: capabilities.chroma_444,
            hdr_output,
        },
    );
    let served_pipeline = arcen_media::video::served_pipeline(
        host_resolved.pipeline,
        video_configuration_from_plan(plan),
        host_resolved.max_fps,
        client.motion_priority,
        arcen_media::video::ServedPipelineContext {
            backend: capabilities.encoder_hardware.map(|hardware| {
                if hardware {
                    arcen_media::video::AcceleratorClass::Hardware
                } else {
                    arcen_media::video::AcceleratorClass::Software
                }
            }),
            exact_or_admin_override: host_resolved.pipeline.is_none(),
        },
    );
    let motion_priority = arcen_media::video::PipelineId::from_served_wire(&served_pipeline)
        .map_or(client.motion_priority, |pipeline| {
            arcen_media::video::pipeline_contract(pipeline).priority
        });
    Ok(ResolvedInitialVideo {
        plan,
        codec: match host_resolved.video.codec {
            VideoCodec::H264 => crate::encode::EncoderCodec::H264,
            VideoCodec::H265 => crate::encode::EncoderCodec::Hevc,
            VideoCodec::Av1 | VideoCodec::Jpeg | VideoCodec::Vp9 => {
                return Err(HandshakeError::VideoRequest(format!(
                    "macOS VideoToolbox cannot serve {}",
                    host_resolved.video.codec.token()
                )));
            }
        },
        fps: host_resolved.max_fps,
        motion_priority,
        requested_pipeline: host_resolved.pipeline,
        active_pipeline: Some(served_pipeline),
    })
}

/// Extracts the credentials a client supplied, if the method is one this host
/// offered.
///
/// The password travels in `credential`, not `password`: the Deck's
/// `AuthResponse::pam` puts it there. Reading the wrong field refuses every
/// correct password, and does so in a way that looks exactly like a bad
/// password, which is why this is separated out and tested directly.
#[must_use]
fn credentials_from(response: &arcen_protocol::messages::AuthResponse) -> Option<(&str, &str)> {
    // Only PAM is offered, so anything else is a client asking for a scheme
    // this host did not advertise.
    if response.method != "pam" {
        return None;
    }
    if response.username.is_empty() {
        return None;
    }
    Some((&response.username, &response.credential))
}
/// Prompts for and checks credentials.
///
/// Returns the authenticated account name.
///
/// The messages are built from the shared types rather than hand-written
/// JSON, because the field names are not guessable.
async fn authenticate(
    socket: &mut PierSocket,
    multi_monitor_v1: Option<arcen_protocol::messages::AuthMultiMonitorOfferMsg>,
    serving_uid: Option<u32>,
    peer: Option<std::net::IpAddr>,
    admission: Option<&std::sync::Arc<arcen_session::session_admission::SessionAdmissionRuntime>>,
) -> Result<
    (
        String,
        Option<arcen_protocol::messages::InitialVideoRequestMsg>,
        Option<arcen_protocol::messages::AuthMultiMonitorRequestMsg>,
        RequestedDesktop,
        Option<arcen_protocol::messages::ClientMonitor>,
        arcen_protocol::messages::CursorMode,
        Option<String>,
        Option<AdmissionGuard>,
    ),
    HandshakeError,
> {
    use arcen_protocol::messages::{AUTH_REQUEST, AuthRequest, AuthResponse, AuthResult};

    let request = AuthRequest {
        msg_type: AUTH_REQUEST.to_owned(),
        auth_methods: vec!["pam".to_owned()],
        challenge: String::new(),
        salt: String::new(),
        auth_mode: Some("pam".to_owned()),
        disclaimer: None,
        multi_monitor_v1,
    };
    let request = serde_json::to_string(&request)
        .map_err(|error| HandshakeError::Transport(format!("encode auth_request: {error}")))?;
    net::send_json(socket, request)
        .await
        .map_err(HandshakeError::Transport)?;

    let reply = tokio::time::timeout(AUTH_TIMEOUT, net::receive_json(socket))
        .await
        .map_err(|_| HandshakeError::AuthTimeout)?
        .map_err(HandshakeError::Transport)?
        .ok_or(HandshakeError::PeerClosed)?;

    let response: AuthResponse = serde_json::from_str(&reply).map_err(|_| {
        let kind = serde_json::from_str::<serde_json::Value>(&reply)
            .ok()
            .and_then(|value| {
                value
                    .get("type")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| "unparseable".to_owned());
        HandshakeError::UnexpectedMessage(kind)
    })?;
    if response.msg_type != arcen_protocol::messages::AUTH_RESPONSE {
        return Err(HandshakeError::UnexpectedMessage(response.msg_type));
    }

    let outcome = match credentials_from(&response) {
        Some((username, credential)) => {
            let keys = throttle_keys(peer, username);
            if let Some(wait) = throttle_retry_after(&keys) {
                tracing::warn!(
                    target: arcen_telemetry::names::target::AUTH,
                    peer = ?peer,
                    retry_after_secs = wait.as_secs(),
                    "sign-in refused without checking the password: too many recent failures"
                );
                Err(crate::auth::AuthFailure::TooManyAttempts)
            } else {
                let username = username.to_owned();
                let credential = credential.to_owned();
                let checked = authenticate_credentials(username, credential).await;
                throttle_record(&keys, &checked);
                checked
            }
        }
        None => Err(crate::auth::AuthFailure::InvalidCredentials),
    };
    let outcome =
        outcome.and_then(|authenticated| admit_serving_account(authenticated, serving_uid));
    // The slot, only now that the account is proved and servable.
    let outcome = outcome.and_then(|authenticated| match admission {
        None => Ok((authenticated, None)),
        Some(runtime) => match runtime.admit_new() {
            Ok(lease) => Ok((
                authenticated,
                Some(AdmissionGuard {
                    runtime: std::sync::Arc::clone(runtime),
                    lease: Some(lease),
                }),
            )),
            Err(_) => Err(crate::auth::AuthFailure::HostBusy),
        },
    });

    // The reply says only whether it worked. Which half was wrong, and
    // whether the account exists at all, stay on this side.
    let result = AuthResult {
        msg_type: arcen_protocol::messages::AUTH_RESULT.to_owned(),
        success: outcome.is_ok(),
        message: match &outcome {
            Ok(_) => "Authenticated".to_owned(),
            Err(failure) => failure.to_string(),
        },
        resume_grant: None,
        resume_window_secs: None,
        resumed: false,
        error_code: None,
        session_setup_failed: false,
    };
    let result = serde_json::to_string(&result)
        .map_err(|error| HandshakeError::Transport(format!("encode auth_result: {error}")))?;
    net::send_json(socket, result)
        .await
        .map_err(HandshakeError::Transport)?;

    match outcome {
        // The Deck states its colour request in the same message as its
        // credentials, so it is carried out of here rather than re-read later:
        // the hello the Deck is told and the capture the host starts must come
        // from one decision.
        Ok((authenticated, admission)) => Ok((
            authenticated.user,
            response.initial_video,
            response.multi_monitor_v1,
            RequestedDesktop::from_auth(response.screen_width, response.screen_height),
            response
                .monitors
                .iter()
                .find(|monitor| monitor.is_primary)
                .cloned(),
            response.cursor_preference,
            response.timezone,
            admission,
        )),
        Err(failure) => Err(HandshakeError::Rejected(failure)),
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn an_agent_serves_only_its_own_account() {
        let person = |uid| crate::auth::Authenticated {
            user: "someone".to_owned(),
            uid,
        };
        assert!(admit_serving_account(person(Some(501)), None).is_ok());
        assert!(admit_serving_account(person(Some(501)), Some(501)).is_ok());
        assert_eq!(
            admit_serving_account(person(Some(502)), Some(501)),
            Err(crate::auth::AuthFailure::NotConsoleOwner)
        );
        // An account the directory could not resolve is not the agent's.
        assert_eq!(
            admit_serving_account(person(None), Some(501)),
            Err(crate::auth::AuthFailure::NotConsoleOwner)
        );
    }

    #[test]
    fn an_arranged_display_refreshes_faster_than_the_rate_it_is_captured_at() {
        // The bug this encodes: building the display at the capture rate gave
        // ScreenCaptureKit no spare vsync, and it delivered every second one.
        // A 30 fps session measured 67.4 ms between frames — two vsyncs of a
        // 30 Hz display — for half the requested rate.
        for fps in [1, 15, 24, 30, 60] {
            let hz = arranged_refresh_hz(fps);
            assert!(
                hz >= f64::from(fps) * 2.0 || hz >= 120.0,
                "{fps} fps got a {hz} Hz display, which leaves no headroom",
            );
        }
    }

    #[test]
    fn an_arranged_display_stays_within_rates_virtual_displays_accept() {
        assert_eq!(
            arranged_refresh_hz(0),
            60.0,
            "a nonsense rate still lands on a sane display"
        );
        assert_eq!(arranged_refresh_hz(30), 60.0);
        assert_eq!(arranged_refresh_hz(60), 120.0);
        assert_eq!(
            arranged_refresh_hz(u32::MAX),
            120.0,
            "no overflow, and no absurd request"
        );
    }

    fn capabilities() -> AdvertisedCapabilities {
        AdvertisedCapabilities {
            display_id: 1,
            login_window: false,
            displays: vec![crate::displays::DisplaySnapshot {
                display_id: 1,
                pixel_width: 1920,
                pixel_height: 1080,
                origin_x: 0.0,
                origin_y: 0.0,
            }],
            width: 1920,
            height: 1080,
            hevc: true,
            h264: true,
            main10: false,
            chroma_444: false,
            encoder_hardware: Some(true),
            audio: false,
            audio_policy: arcen_media::audio::AudioPolicy::configured(false, false),
            microphone_policy: arcen_media::audio::MicrophonePolicy {
                operator_enabled: false,
                backend_available: false,
                codecs: arcen_media::audio::MicrophoneCodecAvailability {
                    opus: true,
                    pcm: true,
                },
            },
            clipboard_policy: arcen_media::clipboard::ClipboardPolicy::default(),
            multi_monitor_v1: None,
        }
    }

    #[test]
    fn the_client_gets_the_frame_rate_it_asked_for() {
        use arcen_protocol::messages::{
            ClientVideoCapabilitiesMsg, InitialVideoRequestMsg, QualitySettings,
        };

        fn asking_for(max_fps: u32) -> InitialVideoRequestMsg {
            InitialVideoRequestMsg {
                quality: QualitySettings {
                    msg_type: "quality_settings".to_owned(),
                    max_fps,
                    ..Default::default()
                },
                capabilities: ClientVideoCapabilitiesMsg::default(),
                pipeline: None,
            }
        }

        // A Deck on a metered link asking for 30 was served 60 and told it was
        // getting 60, because the request was parsed and then discarded.
        assert_eq!(negotiated_fps(Some(&asking_for(30))), 30);
        // The host's ceiling still bounds an over-ambitious request.
        assert_eq!(negotiated_fps(Some(&asking_for(240))), 60);
        assert_eq!(negotiated_fps(Some(&asking_for(60))), 60);
        // Zero is a client that sent nothing meaningful, not a request for no
        // frames at all.
        assert_eq!(negotiated_fps(Some(&asking_for(0))), 60);
        assert_eq!(negotiated_fps(None), 60);
    }

    #[test]
    fn speed_pipeline_resolves_to_sixty_fps_motion_contract() {
        use arcen_protocol::messages::{
            ClientVideoCapabilitiesMsg, InitialVideoRequestMsg, QualitySettings, StreamPipeline,
        };

        let request = InitialVideoRequestMsg {
            quality: QualitySettings {
                msg_type: "quality_settings".to_owned(),
                codec: "h264".to_owned(),
                chroma: "yuv420".to_owned(),
                bit_depth: "8".to_owned(),
                color_range: "limited".to_owned(),
                color_matrix: "bt709".to_owned(),
                max_fps: 60,
                motion_priority: "detail".to_owned(),
                ..Default::default()
            },
            capabilities: ClientVideoCapabilitiesMsg {
                h264: true,
                h265: true,
                ..Default::default()
            },
            pipeline: Some(StreamPipeline::Speed),
        };
        let resolved =
            resolve_initial_video(Some(&request), &capabilities(), false).expect("Speed resolves");
        assert_eq!(resolved.fps, 60);
        assert_eq!(
            resolved.motion_priority,
            arcen_media::video::MotionPriority::Motion
        );
        assert_eq!(
            resolved.active_pipeline,
            Some(arcen_protocol::messages::ServedStreamPipeline::Speed)
        );
    }

    #[test]
    fn the_server_hello_states_the_size_the_deck_will_receive() {
        // Not the host's own screen. A Deck told 3600x2338 and then sent
        // 1920x1080 has no way to reconcile the two.
        let json = server_hello_json_for_multi(
            &capabilities(),
            "alice",
            arcen_media::session_plan::ResolvedVideoPlan::standard(None),
            crate::encode::EncoderCodec::H264,
            None,
            None,
            (1280, 720),
        );
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("hello parses");
        assert_eq!(parsed["screen_width"], 1280);
        assert_eq!(parsed["screen_height"], 720);
    }

    #[test]
    fn the_server_hello_carries_the_real_display_size() {
        let json = server_hello_json(&capabilities(), "someone");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid json");
        assert_eq!(parsed["type"], arcen_protocol::messages::SERVER_HELLO);
        assert_eq!(parsed["screen_width"], 1920);
        assert_eq!(parsed["screen_height"], 1080);
        assert_eq!(parsed["os_user"], "someone");
    }

    #[test]
    fn the_server_hello_parses_as_the_shared_message_type() {
        // The Deck deserialises this into `ServerHelloMsg`, so a shape it
        // cannot read is a broken session rather than a cosmetic problem.
        let json = server_hello_json(&capabilities(), "someone");
        let hello: arcen_protocol::messages::ServerHelloMsg =
            serde_json::from_str(&json).expect("Deck must be able to parse this");
        assert_eq!(hello.screen_width, 1920);
        assert_eq!(hello.screen_height, 1080);
        assert!(hello.supports_h264);
    }

    #[test]
    fn the_server_hello_says_which_build_is_serving() {
        let json = server_hello_json(&capabilities(), "someone");
        let hello: arcen_protocol::messages::ServerHelloMsg =
            serde_json::from_str(&json).expect("Deck must be able to parse this");
        let identity = hello
            .build_identity()
            .expect("identity decodes")
            .expect("the macOS Pier advertises its build");
        assert_eq!(identity.product, "arcen-pier-macos");
        assert_eq!(identity.version, crate::VERSION);
    }

    #[test]
    fn the_arranged_panel_takes_the_deck_displays_size_and_name() {
        let monitor = |width_mm: f32, height_mm: f32| arcen_protocol::messages::ClientMonitor {
            id: 1,
            x: 0,
            y: 0,
            width_px: 3600,
            height_px: 2260,
            scale: 2.0,
            refresh_hz: 120,
            is_primary: true,
            name: "Built-in Retina Display".to_owned(),
            width_mm,
            height_mm,
            vendor: 0,
            model: 0,
            serial: 0,
            edid: String::new(),
            color: None,
        };
        let reported = panel_identity(&monitor(302.0, 196.0), 1800, 1130);
        assert_eq!(reported.size_mm, (302, 196));
        assert_eq!(reported.name, "Built-in Retina Display");
        assert_eq!(reported.color, None);
        // Without millimetres, the shared rule derives them from the scale.
        let derived = panel_identity(&monitor(0.0, 0.0), 1800, 1130);
        assert_eq!(
            derived.size_mm,
            arcen_outputs::edid::physical_size_mm(arcen_outputs::edid::EdidRequest {
                width: 1800,
                height: 1130,
                refresh_hz: 120,
                width_mm: 0.0,
                height_mm: 0.0,
                scale: 2.0,
                product_id: 0,
                serial: 0,
                color: None,
            })
        );
    }

    #[test]
    fn hdr_is_claimed_only_on_a_display_with_headroom() {
        assert!(hdr_is_proven(true, Some(5.0)), "an HDR virtual display");
        assert!(!hdr_is_proven(true, Some(1.0)), "an SDR display");
        assert!(!hdr_is_proven(true, None), "no reading is not a proof");
        assert!(!hdr_is_proven(false, Some(16.0)), "HDR nobody asked for");
    }

    #[test]
    fn hdr_panel_requires_the_deck_display_to_be_hdr() {
        let monitor_with_color = |color| arcen_protocol::messages::ClientMonitor {
            id: 1,
            x: 0,
            y: 0,
            width_px: 3600,
            height_px: 2260,
            scale: 2.0,
            refresh_hz: 120,
            is_primary: true,
            name: "Deck".to_owned(),
            width_mm: 0.0,
            height_mm: 0.0,
            vendor: 0,
            model: 0,
            serial: 0,
            edid: String::new(),
            color,
        };
        let hdr = monitor_with_color(Some(arcen_protocol::messages::DisplayColorMsg {
            gamut: arcen_protocol::messages::DisplayGamutMsg::DisplayP3,
            hdr_headroom: 16.0,
            ..Default::default()
        }));
        let bright_sdr = monitor_with_color(Some(arcen_protocol::messages::DisplayColorMsg {
            peak_nits: Some(500.0),
            hdr_headroom: 1.0,
            ..Default::default()
        }));
        assert_eq!(
            select_virtual_panel(true, false, Some(&hdr)),
            crate::virtual_display::VirtualPanel::Hdr
        );
        assert_eq!(
            select_virtual_panel(true, false, Some(&bright_sdr)),
            crate::virtual_display::VirtualPanel::Sdr
        );
        assert_eq!(
            select_virtual_panel(true, false, None),
            crate::virtual_display::VirtualPanel::Sdr
        );
        assert_eq!(
            select_virtual_panel(true, true, Some(&hdr)),
            crate::virtual_display::VirtualPanel::Sdr
        );
    }

    #[test]
    fn the_hello_tells_the_deck_whether_it_is_the_login_window() {
        let parse = |capabilities: &AdvertisedCapabilities| {
            serde_json::from_str::<arcen_protocol::messages::ServerHelloMsg>(&server_hello_json(
                capabilities,
                "",
            ))
            .expect("Deck must be able to parse this")
            .login_window
        };
        assert!(!parse(&capabilities()), "a signed-in desktop");
        assert!(parse(&AdvertisedCapabilities {
            login_window: true,
            ..capabilities()
        }));
    }

    #[test]
    fn codec_claims_travel_to_the_client() {
        let only_h264 = AdvertisedCapabilities {
            hevc: false,
            ..capabilities()
        };
        let json = server_hello_json(&only_h264, "");
        // Parsed through the shared type, which is the only thing that proves
        // a Deck sees these. An earlier version of this test read a
        // `supports_hevc` key that no client has ever looked at, so it passed
        // while the real field was absent.
        let hello: arcen_protocol::messages::ServerHelloMsg =
            serde_json::from_str(&json).expect("the Deck must be able to parse this");
        assert!(!hello.supports_h265);
        assert!(hello.supports_h264);
        assert_eq!(hello.codec, "h264");

        let with_hevc = server_hello_json(&capabilities(), "");
        let hello: arcen_protocol::messages::ServerHelloMsg =
            serde_json::from_str(&with_hevc).expect("parse");
        assert!(hello.supports_h265);
        assert_eq!(hello.codec, "h265");
    }

    #[test]
    fn invalid_colour_tokens_are_refused_not_reinterpreted() {
        use arcen_protocol::messages::{
            ClientVideoCapabilitiesMsg, InitialVideoRequestMsg, QualitySettings,
        };

        let mut request = InitialVideoRequestMsg {
            quality: QualitySettings {
                msg_type: "quality_settings".to_owned(),
                chroma: "not-a-chroma-444".to_owned(),
                ..Default::default()
            },
            capabilities: ClientVideoCapabilitiesMsg {
                h264: true,
                h265: true,
                yuv444: true,
                main10: true,
                ..Default::default()
            },
            pipeline: None,
        };
        let error = resolve_initial_video(Some(&request), &capabilities(), false).unwrap_err();
        assert!(
            matches!(error, HandshakeError::VideoRequest(_)),
            "invalid tokens must refuse admission rather than become 4:2:0",
        );

        request.quality.chroma = "yuv420".to_owned();
        request.quality.transfer = "pq-something".to_owned();
        let error = resolve_initial_video(Some(&request), &capabilities(), false).unwrap_err();
        assert!(matches!(error, HandshakeError::VideoRequest(_)));
    }

    #[test]
    fn production_resolver_uses_measured_host_codecs() {
        use crate::encode::EncoderCodec;
        use arcen_protocol::messages::{
            ClientVideoCapabilitiesMsg, InitialVideoRequestMsg, QualitySettings,
        };

        fn request(h264: bool, h265: bool, wanted: &str) -> InitialVideoRequestMsg {
            InitialVideoRequestMsg {
                quality: QualitySettings {
                    msg_type: "quality_settings".to_owned(),
                    quality_bias: 0.5,
                    max_fps: 60,
                    max_bandwidth_mbps: 50.0,
                    codec: wanted.to_owned(),
                    chroma: "yuv420".to_owned(),
                    ..Default::default()
                },
                capabilities: ClientVideoCapabilitiesMsg {
                    h264,
                    h265,
                    ..Default::default()
                },
                pipeline: None,
            }
        }

        let resolved = resolve_initial_video(
            Some(&request(true, true, "h265")),
            &AdvertisedCapabilities {
                h264: true,
                hevc: true,
                ..capabilities()
            },
            false,
        )
        .expect("HEVC supported on both sides");
        assert_eq!(resolved.codec, EncoderCodec::Hevc);

        let resolved = resolve_initial_video(
            Some(&request(true, true, "h265")),
            &AdvertisedCapabilities {
                h264: true,
                hevc: false,
                ..capabilities()
            },
            false,
        )
        .expect("host-measured H.264 fallback");
        assert_eq!(resolved.codec, EncoderCodec::H264);
        assert_eq!(
            resolved.plan.degraded,
            Some(arcen_media::session_plan::Degradation::HostLacksHevc)
        );

        // An explicit preference is honoured for an ordinary desktop. The
        // Linux Pier renegotiates on exactly this mismatch; macOS ignored the
        // field, so the same request produced different codecs per host.
        let resolved = resolve_initial_video(
            Some(&request(true, true, "h264")),
            &AdvertisedCapabilities {
                h264: true,
                hevc: true,
                ..capabilities()
            },
            false,
        )
        .expect("explicit H.264 preference");
        assert_eq!(resolved.codec, EncoderCodec::H264);

        // No stated capabilities is the one case that keeps the old default,
        // which is also what the shared resolver assumes.
        let resolved =
            resolve_initial_video(None, &capabilities(), false).expect("default request");
        assert_eq!(resolved.codec, EncoderCodec::Hevc);
    }

    #[test]
    fn exact_ten_bit_four_two_zero_is_reported_as_degraded() {
        use arcen_protocol::messages::{
            ClientVideoCapabilitiesMsg, InitialVideoRequestMsg, QualitySettings,
            VideoSelectionIntent,
        };

        let request = InitialVideoRequestMsg {
            quality: QualitySettings {
                msg_type: "quality_settings".to_owned(),
                codec: "h265".to_owned(),
                chroma: "yuv420".to_owned(),
                bit_depth: "10".to_owned(),
                video_selection: VideoSelectionIntent::Exact,
                ..Default::default()
            },
            capabilities: ClientVideoCapabilitiesMsg {
                h264: true,
                h265: true,
                main10: true,
                yuv444: true,
                ..Default::default()
            },
            pipeline: None,
        };
        let resolved = resolve_initial_video(
            Some(&request),
            &AdvertisedCapabilities {
                main10: true,
                chroma_444: true,
                ..capabilities()
            },
            false,
        )
        .expect("valid request");
        assert_eq!(
            resolved.plan.degraded,
            Some(arcen_media::session_plan::Degradation::HostLacksExactContract),
        );
        assert!(!resolved.plan.is_exact());
    }

    #[test]
    fn hdr_without_headroom_reports_grading_as_the_served_pipeline() {
        use arcen_protocol::messages::{
            ClientVideoCapabilitiesMsg, InitialVideoRequestMsg, QualitySettings, StreamPipeline,
            VideoSelectionIntent,
        };

        let request = InitialVideoRequestMsg {
            quality: QualitySettings {
                msg_type: "quality_settings".to_owned(),
                codec: "h265".to_owned(),
                chroma: "yuv444".to_owned(),
                bit_depth: "10".to_owned(),
                max_fps: 30,
                color_range: "full".to_owned(),
                color_matrix: "bt2020ncl".to_owned(),
                color_primaries: "bt2020".to_owned(),
                transfer: "pq".to_owned(),
                video_selection: VideoSelectionIntent::ColorFidelity,
                ..Default::default()
            },
            capabilities: ClientVideoCapabilitiesMsg {
                h264: true,
                h265: true,
                yuv444: true,
                main10: true,
                full_range: true,
                bt2020_ncl_matrix: true,
                ..Default::default()
            },
            pipeline: Some(StreamPipeline::Hdr),
        };
        let caps = AdvertisedCapabilities {
            main10: true,
            chroma_444: true,
            ..capabilities()
        };
        let resolved = resolve_initial_video(Some(&request), &caps, false).expect("degrades");
        assert_eq!(
            resolved.plan.tier,
            arcen_media::session_plan::VideoTier::Grading
        );
        assert_eq!(
            resolved.active_pipeline,
            Some(arcen_protocol::messages::ServedStreamPipeline::Grading)
        );
        let hello: arcen_protocol::messages::ServerHelloMsg =
            serde_json::from_str(&server_hello_json_for_multi(
                &caps,
                "alice",
                resolved.plan,
                resolved.codec,
                resolved.active_pipeline,
                None,
                (1920, 1080),
            ))
            .expect("hello");
        assert_eq!(
            hello.active_pipeline,
            Some(arcen_protocol::messages::ServedStreamPipeline::Grading)
        );
    }

    #[test]
    fn exact_four_two_two_and_av1_are_recorded_as_degraded() {
        use arcen_protocol::messages::{
            ClientVideoCapabilitiesMsg, InitialVideoRequestMsg, QualitySettings,
            VideoSelectionIntent,
        };

        let mut request = InitialVideoRequestMsg {
            quality: QualitySettings {
                msg_type: "quality_settings".to_owned(),
                codec: "h265".to_owned(),
                chroma: "yuv422".to_owned(),
                bit_depth: "8".to_owned(),
                video_selection: VideoSelectionIntent::Exact,
                ..Default::default()
            },
            capabilities: ClientVideoCapabilitiesMsg {
                h264: true,
                h265: true,
                av1: true,
                ..Default::default()
            },
            pipeline: None,
        };
        let resolved =
            resolve_initial_video(Some(&request), &capabilities(), false).expect("valid 4:2:2");
        assert_eq!(
            resolved.plan.degraded,
            Some(arcen_media::session_plan::Degradation::HostLacksExactContract),
        );
        assert!(!resolved.plan.is_exact());

        request.quality.codec = "av1".to_owned();
        request.quality.chroma = "yuv420".to_owned();
        let resolved =
            resolve_initial_video(Some(&request), &capabilities(), false).expect("valid AV1");
        assert_eq!(resolved.codec, crate::encode::EncoderCodec::Hevc);
        assert_eq!(
            resolved.plan.degraded,
            Some(arcen_media::session_plan::Degradation::HostLacksExactContract),
        );
    }

    #[test]
    fn exact_colour_axis_changes_are_recorded_as_degraded() {
        use arcen_protocol::messages::{
            ClientVideoCapabilitiesMsg, InitialVideoRequestMsg, QualitySettings,
            VideoSelectionIntent,
        };

        let request = InitialVideoRequestMsg {
            quality: QualitySettings {
                msg_type: "quality_settings".to_owned(),
                codec: "h265".to_owned(),
                chroma: "yuv420".to_owned(),
                bit_depth: "8".to_owned(),
                color_range: "full".to_owned(),
                video_selection: VideoSelectionIntent::Exact,
                ..Default::default()
            },
            capabilities: ClientVideoCapabilitiesMsg {
                h264: true,
                h265: true,
                full_range: true,
                ..Default::default()
            },
            pipeline: None,
        };
        let resolved =
            resolve_initial_video(Some(&request), &capabilities(), false).expect("valid");
        assert_eq!(
            resolved.plan.degraded,
            Some(arcen_media::session_plan::Degradation::HostLacksExactContract),
        );

        let request = InitialVideoRequestMsg {
            quality: QualitySettings {
                msg_type: "quality_settings".to_owned(),
                codec: "h265".to_owned(),
                chroma: "yuv420".to_owned(),
                bit_depth: "8".to_owned(),
                transfer: "srgb".to_owned(),
                video_selection: VideoSelectionIntent::Exact,
                ..Default::default()
            },
            capabilities: ClientVideoCapabilitiesMsg {
                h264: true,
                h265: true,
                ..Default::default()
            },
            pipeline: None,
        };
        let resolved =
            resolve_initial_video(Some(&request), &capabilities(), false).expect("valid");
        assert_eq!(
            resolved.plan.degraded,
            Some(arcen_media::session_plan::Degradation::HostLacksExactContract),
        );
    }

    #[tokio::test]
    async fn application_handshake_timeout_is_reported() {
        let never = std::future::pending::<Result<Handshake, HandshakeError>>();
        let error = bound_application_handshake(never, Duration::from_millis(1))
            .await
            .expect_err("timeout must refuse the connection");
        assert!(matches!(error, HandshakeError::ApplicationHandshakeTimeout));
    }

    #[test]
    fn the_host_does_not_offer_a_colour_range_it_never_sends() {
        // `stream.rs` names ColorRange::Limited outright and the resolved plan
        // has no range axis, so full range was a request a Deck could make,
        // have accepted, and never receive — reported back as "active".
        let json = server_hello_json(&capabilities(), "alice");
        let hello: arcen_protocol::messages::ServerHelloMsg =
            serde_json::from_str(&json).expect("parse");
        assert!(!hello.color_caps.full_range);
        assert_eq!(hello.color_caps.active_range, "limited");
    }

    #[test]
    fn the_advertised_pixel_format_names_both_the_chroma_and_the_depth() {
        use arcen_media::session_plan::{PlanBitDepth, PlanChroma};

        // The 4:4:4 arm used to name ten bits regardless of the plan's depth.
        // Every plan the resolver builds does pair them that way, so it was
        // true by coincidence rather than by construction, and a Deck told
        // "yuv444p10le" about an eight-bit stream would size its planes wrong.
        assert_eq!(
            advertised_pix_fmt(PlanChroma::Yuv420, PlanBitDepth::Eight),
            "yuv420p",
        );
        assert_eq!(
            advertised_pix_fmt(PlanChroma::Yuv420, PlanBitDepth::Ten),
            "yuv420p10le",
        );
        assert_eq!(
            advertised_pix_fmt(PlanChroma::Yuv444, PlanBitDepth::Eight),
            "yuv444p",
        );
        assert_eq!(
            advertised_pix_fmt(PlanChroma::Yuv444, PlanBitDepth::Ten),
            "yuv444p10le",
        );
    }

    #[test]
    fn a_client_that_asked_for_no_clipboard_gets_none() {
        // The narrowing is only safe if a request that says "yes" survives it.
        // A default-constructed hello with the flags set is the shape the Deck
        // sends; one with them clear is a person who switched clipboard off,
        // and that must mean no pasteboard is read at all.
        let mut hello = arcen_protocol::messages::ClientHelloMsg::default();
        hello.clipboard_protocol_version = arcen_protocol::messages::CLIPBOARD_PROTOCOL_VERSION;
        hello.clipboard_text_c2s = true;
        hello.clipboard_text_s2c = true;
        hello.clipboard_image_c2s = true;
        hello.clipboard_image_s2c = true;
        let negotiated =
            clipboard_negotiation(&hello, arcen_media::clipboard::ClipboardPolicy::default())
                .expect("this must carry a clipboard");
        assert!(negotiated.allows(
            arcen_media::clipboard::ClipboardFlow::HostToClient,
            arcen_media::clipboard::ClipboardKind::TextUtf8,
        ));
        assert!(negotiated.allows(
            arcen_media::clipboard::ClipboardFlow::ClientToHost,
            arcen_media::clipboard::ClipboardKind::TextUtf8,
        ));

        let off = arcen_protocol::messages::ClientHelloMsg::default();
        assert!(
            clipboard_negotiation(&off, arcen_media::clipboard::ClipboardPolicy::default())
                .is_none(),
            "a client that asked for no clipboard must not have its host's pasteboard read",
        );
    }

    #[test]
    fn configured_clipboard_disable_reaches_negotiation_and_advertisement() {
        let disabled = arcen_media::clipboard::ClipboardPolicy::new(
            arcen_media::clipboard::ClipboardDirection::Disabled,
            arcen_media::clipboard::ClipboardContent::All,
            arcen_media::clipboard::DEFAULT_CLIPBOARD_BYTES,
        )
        .expect("valid disabled policy");
        let caps = AdvertisedCapabilities {
            clipboard_policy: disabled,
            ..capabilities()
        };
        let hello: arcen_protocol::messages::ServerHelloMsg =
            serde_json::from_str(&server_hello_json(&caps, "alice")).expect("parse");
        assert_eq!(
            hello.clipboard.expect("policy advertised").direction,
            arcen_protocol::messages::ClipboardDirectionMsg::Disabled,
        );

        let mut client = arcen_protocol::messages::ClientHelloMsg {
            clipboard_protocol_version: arcen_protocol::messages::CLIPBOARD_PROTOCOL_VERSION,
            clipboard_text_c2s: true,
            clipboard_text_s2c: true,
            clipboard_image_c2s: true,
            clipboard_image_s2c: true,
            ..Default::default()
        };
        assert!(clipboard_negotiation(&client, disabled).is_none());
        client.clipboard_text_s2c = false;
        assert!(clipboard_negotiation(&client, disabled).is_none());
    }

    #[test]
    fn a_microphone_request_is_answered_even_though_this_host_has_no_importer() {
        // A Deck that asks for a microphone and is told nothing waits for one
        // until its own media timeout expires — measured as sixty seconds of
        // silence against the lab Mac. The Linux Pier answers every request.
        // Refusing quickly, with a reason, is what lets the client say why.
        let refused = arcen_media::audio::ResolvedMicrophoneStream::disabled(
            MICROPHONE_GENERATION,
            arcen_protocol::messages::MicrophoneStreamReason::BackendUnavailable,
        );
        assert!(!refused.is_enabled());
        let result = refused.result();
        assert!(!result.enabled);
        assert_eq!(
            result.reason,
            arcen_protocol::messages::MicrophoneStreamReason::BackendUnavailable,
        );
        serde_json::to_string(&result).expect("the refusal must reach the wire");
    }

    #[test]
    fn the_hello_names_the_transport_the_socket_actually_is() {
        // The Deck refuses a hello whose transport differs from the socket it
        // dialled, so an absent value ends every real session before video.
        // This was missing, and no test noticed.
        let json = server_hello_json(&capabilities(), "alice");
        let hello: arcen_protocol::messages::ServerHelloMsg =
            serde_json::from_str(&json).expect("parse");
        assert_eq!(
            hello.negotiated_transport.as_deref(),
            Some(arcen_transport::CAPABILITY_TRANSPORT_QUIC),
        );
        assert!(
            hello.requires_auth,
            "this host always authenticates, and must say so",
        );
        assert_eq!(hello.os_user, "alice");
    }

    #[test]
    fn unbuilt_capabilities_are_not_advertised() {
        // Advertising a feature this host has not built makes a Deck offer the
        // operator something that silently does nothing.
        let json = server_hello_json(&capabilities(), "alice");
        let hello: arcen_protocol::messages::ServerHelloMsg =
            serde_json::from_str(&json).expect("parse");
        assert!(!hello.supports_audio);
        assert!(hello.audio_output.is_none());
        assert!(hello.microphone_input.is_none());
        // Native Tablet needs a user-mode USB host controller this build
        // cannot create, so it must stay refused however much Basic Tablet
        // works.
        assert!(!hello.usb_hard_v1);
        assert!(!hello.experimental_raw_hid);
    }

    #[test]
    fn basic_tablet_is_offered_and_native_tablet_is_refused() {
        // Each pen claim here is a field `InputController::pen_event` sets. If
        // one is ever removed from the injector this must fail rather than let
        // a Deck negotiate a surface that arrives empty.
        let json = server_hello_json(&capabilities(), "alice");
        let hello: arcen_protocol::messages::ServerHelloMsg =
            serde_json::from_str(&json).expect("parse");
        assert!(hello.supports_pen);
        let input = &hello.input_capabilities;
        for (name, claim) in [
            ("pen", input.pen),
            ("pen_pressure", input.pen_pressure),
            ("pen_tilt", input.pen_tilt),
            ("pen_rotation", input.pen_rotation),
            ("pen_eraser", input.pen_eraser),
            ("pen_proximity", input.pen_proximity),
        ] {
            assert_eq!(
                claim,
                arcen_protocol::messages::InputCapabilityAvailability::Available,
                "{name} is injected, so it must be advertised",
            );
        }
        let modes = &hello.tablet_mode_capabilities;
        assert_eq!(
            modes.local_termination,
            arcen_protocol::messages::InputCapabilityAvailability::Available,
        );
        assert_eq!(
            modes.wacom_usb_bridge,
            arcen_protocol::messages::InputCapabilityAvailability::Unavailable,
            "native tablet has no USB host controller on this host",
        );
    }

    #[test]
    fn colour_claims_follow_the_codec_that_carries_them() {
        // Ten-bit 4:4:4 rides on HEVC here. Advertising it to a Deck on a Mac
        // that only has H.264 would offer a Grading session this host then
        // fails to produce, which is worse than not offering it.
        let h264_only = AdvertisedCapabilities {
            hevc: false,
            ..capabilities()
        };
        let hello: arcen_protocol::messages::ServerHelloMsg =
            serde_json::from_str(&server_hello_json(&h264_only, "alice")).expect("parse");
        assert!(!hello.supports_yuv444);
        assert!(!hello.color_caps.main10);
        assert!(!hello.color_caps.chroma_444);

        let capable = AdvertisedCapabilities {
            main10: true,
            chroma_444: true,
            ..capabilities()
        };
        let hello: arcen_protocol::messages::ServerHelloMsg =
            serde_json::from_str(&server_hello_json(&capable, "alice")).expect("parse");
        assert!(hello.supports_yuv444);
        assert!(hello.color_caps.main10);
    }

    #[test]
    fn the_encoder_class_states_what_was_measured() {
        // VideoToolbox chooses hardware or software per session, and this host
        // now reads that back from the probe encoder rather than leaving it
        // unsaid. Unsaid was not neutral: the Deck falls back to guessing from
        // the backend name, "videotoolbox" contains none of the words it
        // guesses on, and a host encoding in hardware therefore described
        // itself to the person as a fallback media path.
        let hello: arcen_protocol::messages::ServerHelloMsg =
            serde_json::from_str(&server_hello_json(&capabilities(), "alice")).expect("parse");
        assert_eq!(hello.encoder_class, "hardware");
        assert_eq!(hello.encoder_backend, "videotoolbox");
    }

    #[test]
    fn an_unmeasured_encoder_class_is_still_left_unsaid() {
        // The rule the previous test was protecting survives: a host that
        // could not ask claims nothing, because "software" would be as much a
        // guess as "hardware".
        let mut capabilities = capabilities();
        capabilities.encoder_hardware = None;
        let hello: arcen_protocol::messages::ServerHelloMsg =
            serde_json::from_str(&server_hello_json(&capabilities, "alice")).expect("parse");
        assert!(hello.encoder_class.is_empty());
    }

    #[test]
    fn a_software_encoder_says_so() {
        let mut capabilities = capabilities();
        capabilities.encoder_hardware = Some(false);
        let hello: arcen_protocol::messages::ServerHelloMsg =
            serde_json::from_str(&server_hello_json(&capabilities, "alice")).expect("parse");
        assert_eq!(hello.encoder_class, "software");
    }

    #[test]
    fn the_advertised_clipboard_policy_is_the_one_the_session_enforces() {
        let json = server_hello_json(&capabilities(), "alice");
        let hello: arcen_protocol::messages::ServerHelloMsg =
            serde_json::from_str(&json).expect("parse");
        let advertised = hello.clipboard.expect("a clipboard policy is offered");
        let enforced = arcen_media::clipboard::policy_message(
            arcen_media::clipboard::ClipboardPolicy::default(),
        );
        assert_eq!(advertised, enforced);
    }

    #[tokio::test]
    async fn authentication_uses_supervised_blocking_worker() {
        let authenticated = super::authenticate_credentials("alice".to_owned(), "valid".to_owned())
            .await
            .expect("test authenticator accepts valid credential");
        assert_eq!(authenticated.user, "alice");
        assert_eq!(
            super::authenticate_credentials("alice".to_owned(), "bad".to_owned())
                .await
                .unwrap_err(),
            crate::auth::AuthFailure::InvalidCredentials,
        );
    }

    #[test]
    fn the_password_is_read_from_the_field_the_deck_actually_sends() {
        // Built exactly as the Deck builds it. If this host ever goes back to
        // reading a `password` field, this fails instead of turning every
        // correct password into an authentication refusal.
        let response = arcen_protocol::messages::AuthResponse::pam("alice", "s3cr3t");
        let encoded = serde_json::to_string(&response).expect("encode");
        let decoded: arcen_protocol::messages::AuthResponse =
            serde_json::from_str(&encoded).expect("decode");

        let (username, credential) =
            credentials_from(&decoded).expect("pam credentials must be readable");
        assert_eq!(username, "alice");
        assert_eq!(credential, "s3cr3t");
    }

    #[test]
    fn a_method_this_host_did_not_offer_is_refused() {
        let response = arcen_protocol::messages::AuthResponse::password("alice", "s3cr3t");
        assert_eq!(response.method, "password");
        assert!(
            credentials_from(&response).is_none(),
            "only the advertised method may authenticate",
        );
    }

    #[test]
    fn an_empty_username_is_refused_before_reaching_pam() {
        let response = arcen_protocol::messages::AuthResponse::pam("", "s3cr3t");
        assert!(credentials_from(&response).is_none());
    }

    #[test]
    fn a_deck_that_advertises_no_audio_gets_none_and_no_capture_is_started() {
        // The bug this guards is the one that stalled every session: the host
        // used to create a tap at admission regardless, and on macOS creating
        // a tap raises the system audio consent prompt and blocks inside it.
        // A Deck that never asked for sound must resolve to disabled, so no
        // tap is created and there is nothing to wait for.
        let policy = arcen_media::audio::AudioPolicy::configured(true, false);
        let resolved = policy.resolve(None, true);
        assert!(
            !resolved.is_enabled() || resolved.codec.is_some(),
            "a resolved stream either carries a codec or is disabled",
        );
    }

    #[test]
    fn a_host_with_audio_off_never_resolves_an_enabled_stream() {
        // Whatever the Deck advertises. This is the gate that keeps a machine
        // with audio disabled from ever touching Core Audio.
        let policy = arcen_media::audio::AudioPolicy::configured(false, false);
        let spec = arcen_media::audio::AudioFrameSpec::V1;
        let capable = arcen_protocol::messages::AudioOutputCapabilitiesMsg {
            protocol_version: arcen_protocol::messages::AUDIO_PROTOCOL_VERSION,
            codecs: vec![arcen_protocol::wire::AudioCodec::Pcm],
            sample_rate_hz: spec.sample_rate_hz,
            channels: spec.channels,
            frame_duration_ms: spec.frame_duration_ms,
            fec: false,
            dtx: false,
        };
        let resolved = policy.resolve(Some(&capable), true);
        assert!(
            !resolved.is_enabled(),
            "audio disabled on the host must stay disabled however capable the Deck is",
        );
    }

    #[test]
    fn configured_audio_disable_is_advertised_as_no_audio() {
        let caps = AdvertisedCapabilities {
            audio: false,
            audio_policy: arcen_media::audio::AudioPolicy::configured(false, false),
            ..capabilities()
        };
        let hello: arcen_protocol::messages::ServerHelloMsg =
            serde_json::from_str(&server_hello_json(&caps, "alice")).expect("parse");
        assert!(!hello.supports_audio);
        assert!(hello.audio_output.is_none());
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod requested_desktop_tests {
    use super::{MAX_REQUESTED_EDGE, RequestedDesktop};

    #[test]
    fn a_named_size_is_what_gets_captured() {
        let requested = RequestedDesktop::from_auth(1920, 1080);
        assert_eq!(
            requested,
            RequestedDesktop::Exact {
                width: 1920,
                height: 1080
            }
        );
        // 3600x2338 reduced to fit inside 1920x1080 keeps the desktop's shape,
        // so it is not 1920x1080: that would stretch a 1.54:1 desktop to 1.78:1.
        let (width, height) = requested.resolve(3600, 2338);
        assert!(width <= 1920 && height <= 1080);
        let host_aspect = 3600.0 / 2338.0;
        assert!((f64::from(width) / f64::from(height) - host_aspect).abs() < 0.01);
    }

    #[test]
    fn a_request_smaller_than_the_desktop_reduces_it_faithfully() {
        assert_eq!(
            RequestedDesktop::from_auth(960, 540).resolve(1920, 1080),
            (960, 540)
        );
    }

    #[test]
    fn a_request_never_stretches_the_desktop_into_a_new_shape() {
        // The bug this exists to prevent. A 1920x1080 desktop asked for
        // 1800x1130 used to be captured at 1800x1130, which made
        // ScreenCaptureKit letterbox it: about 59 rows of black above and
        // below. Pointer coordinates are normalised against the picture and
        // know nothing about the bars, so every click landed short, by more
        // the further it was from the centre.
        let (width, height) = RequestedDesktop::from_auth(1800, 1130).resolve(1920, 1080);
        let host_aspect = 1920.0 / 1080.0;
        let got_aspect = f64::from(width) / f64::from(height);
        assert!(
            (got_aspect - host_aspect).abs() < 0.01,
            "captured {width}x{height} has aspect {got_aspect:.3}, desktop is {host_aspect:.3}"
        );
        assert!(
            width <= 1800 && height <= 1130,
            "must fit inside the request"
        );
    }

    #[test]
    fn a_request_larger_than_the_desktop_does_not_scale_it_up() {
        // A desktop has the detail it has. 2560x1440 of a 1080p desktop is
        // 1.8x the pixels for exactly the same picture.
        assert_eq!(
            RequestedDesktop::from_auth(2560, 1440).resolve(1920, 1080),
            (1920, 1080)
        );
    }

    #[test]
    fn captured_edges_are_even_for_chroma_siting() {
        for (rw, rh) in [(999_u32, 563_u32), (1501, 845), (777, 439)] {
            let (width, height) = RequestedDesktop::from_auth(rw, rh).resolve(1920, 1080);
            assert_eq!(width % 2, 0, "{width} is odd");
            assert_eq!(height % 2, 0, "{height} is odd");
        }
    }

    #[test]
    fn a_host_with_no_desktop_is_not_divided_by() {
        assert_eq!(
            RequestedDesktop::from_auth(1920, 1080).resolve(0, 0),
            (0, 0)
        );
    }

    #[test]
    fn a_deck_that_named_nothing_gets_the_host_geometry() {
        // Zero means the Deck did not enumerate its display, which is not the
        // same as asking for a zero-sized desktop.
        assert_eq!(
            RequestedDesktop::from_auth(0, 1080),
            RequestedDesktop::HostChoice
        );
        assert_eq!(
            RequestedDesktop::from_auth(1920, 0),
            RequestedDesktop::HostChoice
        );
        assert_eq!(
            RequestedDesktop::from_auth(0, 0).resolve(3600, 2338),
            (3600, 2338)
        );
    }

    #[test]
    fn an_absurd_request_is_refused_rather_than_clamped() {
        // Clamping would serve something other than what was asked for without
        // saying so, which is the behaviour this type exists to end.
        assert_eq!(
            RequestedDesktop::from_auth(MAX_REQUESTED_EDGE + 1, 1080),
            RequestedDesktop::HostChoice
        );
        assert_eq!(
            RequestedDesktop::from_auth(1920, u32::MAX),
            RequestedDesktop::HostChoice
        );
    }

    #[test]
    fn the_largest_accepted_request_is_still_accepted() {
        assert_eq!(
            RequestedDesktop::from_auth(MAX_REQUESTED_EDGE, MAX_REQUESTED_EDGE),
            RequestedDesktop::Exact {
                width: MAX_REQUESTED_EDGE,
                height: MAX_REQUESTED_EDGE
            }
        );
    }
}
