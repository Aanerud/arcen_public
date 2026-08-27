//! WS handshake: build the `server_hello` the client reads first.
//!
//! The host sends `server_hello` only after capenc has emitted authoritative
//! READY metadata. In no-auth mode it remains the first WebSocket message; in
//! PAM mode it follows authentication.

use crate::cli::{AuthMode, Config};
use crate::session::lifecycle::SessionMetadata;
use arcen_media::audio::{AudioPolicy, MicrophonePolicy};
use arcen_media::video::{EncoderBackend, ResolvedMediaPlan};
use arcen_media::BitDepth;
use arcen_media::ChromaSubsampling;
#[cfg(test)]
use arcen_media::{VideoCodec, VideoConfiguration};
#[cfg(test)]
use arcen_protocol::messages::REGION_INPUT_PROTOCOL_VERSION;
use arcen_protocol::messages::{
    InputCapabilitiesMsg, InputCapabilityAvailability, ServerColorCaps, ServerHelloMsg,
    TabletModeCapabilitiesMsg, INPUT_PROTOCOL_VERSION, SERVER_HELLO,
};
#[cfg(feature = "wss-compat")]
use arcen_protocol::CAPABILITY_TRANSPORT_WSS;
use arcen_protocol::{
    negotiate_transport, sanitize_transport_capabilities, CAPABILITY_TRANSPORT_QUIC,
};

/// Build the `server_hello` for this configuration. Advertises the host's
/// *actual* active codec/chroma so the client's badges and decoder setup match
/// the wire (the honesty directive — no ffmpeg-probe guesses).
///
/// `pen_available` must be the runtime-established truth from
/// `InputController::pen_available` (probed/created before this is called),
/// never an aspirational default — `false` when the tablet-tool uinput
/// device was not created, including when the input backend itself is off.
///
/// `region_input_available` must likewise come from
/// `InputController::region_input_available`; it is true only when this
/// attachment owns the shared region adapter for a committed Match My Layout
/// session.
pub fn build_server_hello(
    cfg: &Config,
    plan: &ResolvedMediaPlan,
    session: Option<&SessionMetadata>,
    supports_display_update: bool,
    microphone_backend_available: bool,
    pen_available: bool,
    region_input_available: bool,
) -> ServerHelloMsg {
    let pen_availability = if pen_available {
        InputCapabilityAvailability::Available
    } else {
        InputCapabilityAvailability::Unavailable
    };
    ServerHelloMsg {
        msg_type: SERVER_HELLO.to_string(),
        server_name: "Arcen Host (Rust, Linux)".to_string(),
        version: crate::VERSION.to_string(),
        os_user: session.map_or_else(String::new, |session| session.username.clone()),
        session_id: session.map_or_else(String::new, |session| session.session_id.clone()),
        session_type: session.map_or_else(String::new, |session| session.session_type.clone()),
        desktop: session.map_or_else(String::new, |session| session.desktop.clone()),
        screen_width: plan.width,
        screen_height: plan.height,
        monitors: Vec::new(),
        supports_h264: plan.supports_h264(),
        supports_h265: plan.supports_h265(),
        supports_av1: plan.supports_av1(),
        supports_yuv444: plan.supports_yuv444(),
        supports_audio: cfg.audio_enabled,
        audio_output: cfg
            .audio_enabled
            .then(|| AudioPolicy::configured(true, cfg.audio_compressed).capabilities()),
        microphone_input: MicrophonePolicy {
            operator_enabled: cfg.microphone_input_enabled,
            backend_available: microphone_backend_available,
            codecs: arcen_media::audio::MicrophoneCodecAvailability {
                opus: true,
                pcm: true,
            },
        }
        .capabilities(),
        supports_pen: false, // input arrives in Stage 4
        // SEC-raw-hid. Quarantined: true only when this host binary was built
        // with the default-off `experimental-raw-hid` Cargo feature AND an
        // operator explicitly opted in at runtime (see
        // `crate::input::experimental_raw_hid_runtime_enabled`). Old/default
        // hosts always advertise false, and a false-advertising host must
        // never admit raw HID frames regardless of what a client sends.
        experimental_raw_hid: crate::input::experimental_raw_hid_runtime_enabled(),
        usb_hard_v1: crate::usb_bridge::runtime_available(),
        // True only when this session actually holds a display guard, so the
        // client never sends display_update to a session that cannot resize.
        supports_display_update,
        requires_auth: cfg.auth_mode == AuthMode::Pam,
        encoder_backend: plan.backend.ready_token().to_string(),
        // Declared by the backend rather than guessed by the client from the
        // token above, so a future hardware vendor is not shown as a fallback.
        encoder_class: plan.backend.accelerator_class().token().to_string(),
        available_encoders: Default::default(),
        // Active codec so UDP-delivered frames (no in-band codec) would still
        // decode; also drives the client's codec badge.
        codec: plan.codec_token().to_string(),
        color_caps: ServerColorCaps {
            // Backend capability -- what this resolved backend *could*
            // serve -- not what is currently active; `active_*` below
            // carries the currently active truth. Previously hardcoded
            // `false`/`false` for every host, which made these decorative;
            // they now read the same probed capability sets `supports_h264`
            // &c. above already use, so they cannot drift from them.
            main10: plan.supports_main10(),
            main12: plan.bit_depths.contains(BitDepth::Twelve),
            chroma_422: plan.chroma.contains(ChromaSubsampling::Yuv422),
            chroma_444: plan.supports_yuv444(),
            full_range: plan.supports_full_range(),
            // Identity-matrix encode capability is a static per-backend
            // contract fact, not something the per-GPU probe narrows (see
            // `EncoderBackend::contract`'s doc: whether the *result* survives
            // a client decoder is a separate, measured probe-matrix
            // question).
            identity_matrix: plan.backend.contract().identity_matrix,
            active_bit_depth: plan.bit_depth_token().to_string(),
            active_range: plan.range_token().to_string(),
            active_matrix: plan.matrix_token().to_string(),
            active_primaries: plan.primaries_token().to_string(),
            active_transfer: plan.transfer_token().to_string(),
            advertised_pix_fmt: if plan.video.chroma == ChromaSubsampling::Yuv444 {
                "yuv444p".to_string()
            } else {
                "yuv420p".to_string()
            },
            negotiated_state: if plan.video.chroma == ChromaSubsampling::Yuv444 {
                "active".to_string()
            } else {
                "not_requested".to_string()
            },
        },
        input_protocol_version: INPUT_PROTOCOL_VERSION,
        input_capabilities: InputCapabilitiesMsg {
            absolute_pointer: InputCapabilityAvailability::Available,
            relative_pointer: InputCapabilityAvailability::Available,
            host_cursor: if plan.backend == EncoderBackend::NativeNvenc {
                InputCapabilityAvailability::Available
            } else {
                InputCapabilityAvailability::Unavailable
            },
            region_input: if region_input_available {
                InputCapabilityAvailability::Available
            } else {
                InputCapabilityAvailability::Unavailable
            },
            // Runtime-established truth only: `Available` exactly when the
            // separate tablet-tool uinput device was actually created before
            // this hello was built (see `InputController::pen_available`).
            // Pressure/tilt/eraser/proximity share that same probe because
            // they all live on the one tablet-tool device this backend
            // creates atomically. Rotation stays `Unavailable` even when the
            // tool device exists: this backend never advertises or emits a
            // rotation axis because no target here has proven the
            // kernel/libinput stack recognizes one as tablet rotation.
            pen: pen_availability,
            pen_pressure: pen_availability,
            pen_tilt: pen_availability,
            pen_rotation: InputCapabilityAvailability::Unavailable,
            pen_eraser: pen_availability,
            pen_proximity: pen_availability,
        },
        tablet_mode_capabilities: TabletModeCapabilitiesMsg {
            local_termination: pen_availability,
            wacom_usb_bridge: if crate::usb_bridge::runtime_available() {
                InputCapabilityAvailability::Available
            } else {
                InputCapabilityAvailability::Unavailable
            },
            disabled_mouse_compat: InputCapabilityAvailability::Available,
        },
        clipboard: Some(crate::clipboard::advertised_policy(cfg, session)),
        device_capabilities: Default::default(),
        negotiated_transport: None, // set from the active socket before transmission
    }
    .with_build_identity(crate::build_identity())
}

/// Validates that the client advertised the transport carrying this session.
///
/// Returns `Some(capability_string)` on success, `None` when there is no common
/// transport (caller must disconnect the client).
pub(crate) fn negotiate_client_transport(
    client_capabilities: &[String],
    active_transport: &'static str,
) -> Option<String> {
    let active_transport_known = active_transport == CAPABILITY_TRANSPORT_QUIC;
    #[cfg(feature = "wss-compat")]
    let active_transport_known =
        active_transport_known || active_transport == CAPABILITY_TRANSPORT_WSS;
    if !active_transport_known {
        return None;
    }
    if client_capabilities.is_empty() {
        #[cfg(feature = "wss-compat")]
        if active_transport == CAPABILITY_TRANSPORT_WSS {
            return Some(CAPABILITY_TRANSPORT_WSS.to_string());
        }
        return None;
    }
    let sanitized = sanitize_transport_capabilities(client_capabilities);
    let server_caps = &[active_transport];
    negotiate_transport(&sanitized, server_caps).map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn native_plan(codec: VideoCodec, yuv444: bool) -> ResolvedMediaPlan {
        ResolvedMediaPlan {
            backend: EncoderBackend::NativeNvenc,
            video: VideoConfiguration {
                codec,
                chroma: if yuv444 {
                    ChromaSubsampling::Yuv444
                } else {
                    ChromaSubsampling::Yuv420
                },
                bit_depth: BitDepth::Eight,
                range: arcen_media::ColorRange::Limited,
                matrix: arcen_media::ColorMatrix::Bt709,
                primaries: arcen_media::ColorPrimaries::Bt709,
                transfer: arcen_media::TransferCharacteristics::Bt709,
            },
            width: 1920,
            height: 1080,
            fps: 60,
            codecs: EncoderBackend::NativeNvenc.contract().codecs,
            chroma: EncoderBackend::NativeNvenc.contract().chroma,
            bit_depths: EncoderBackend::NativeNvenc.contract().bit_depths,
            ranges: EncoderBackend::NativeNvenc.contract().ranges,
            cursor_mode: arcen_protocol::messages::CursorMode::Local,
            cursor_in_video: false,
        }
    }

    #[test]
    fn hello_reports_active_codec_and_chroma() {
        let cfg = Config {
            codec: "h265".to_string(),
            chroma: "yuv444".to_string(),
            ..Config::default()
        };
        let plan = native_plan(VideoCodec::H265, true);
        let hello = build_server_hello(&cfg, &plan, None, false, false, false, false);
        assert_eq!(hello.msg_type, SERVER_HELLO);
        assert_eq!(hello.codec, "h265");
        assert!(hello.color_caps.chroma_444, "444 must be advertised active");
        // SEC-001: the default is now PAM, so the hello must advertise that the
        // host requires authentication.
        assert!(
            hello.requires_auth,
            "default mode is PAM and must require auth"
        );
        assert_eq!(hello.input_protocol_version, INPUT_PROTOCOL_VERSION);
        assert_eq!(
            hello.input_capabilities.relative_pointer,
            InputCapabilityAvailability::Available
        );
        assert_eq!(
            hello.input_capabilities.host_cursor,
            InputCapabilityAvailability::Available
        );
        assert_eq!(
            hello.input_capabilities.region_input,
            InputCapabilityAvailability::Unavailable
        );
        // No pen backend was probed for this hello (`pen_available = false`);
        // every pen field must stay honestly `Unavailable`, never a default
        // `Unknown` that could look like a peer that never checked, nor an
        // aspirational `Available`.
        assert_eq!(
            hello.input_capabilities.pen,
            InputCapabilityAvailability::Unavailable
        );
        assert_eq!(
            hello.input_capabilities.pen_rotation,
            InputCapabilityAvailability::Unavailable
        );
    }

    #[test]
    fn hello_authorizes_region_input_only_when_the_runtime_adapter_exists() {
        let cfg = Config::default();
        let plan = native_plan(VideoCodec::H264, false);

        let unavailable = build_server_hello(&cfg, &plan, None, false, false, false, false);
        assert_eq!(
            unavailable.input_capabilities.region_input,
            InputCapabilityAvailability::Unavailable
        );

        let available = build_server_hello(&cfg, &plan, None, false, false, false, true);
        assert_eq!(
            available.input_protocol_version,
            REGION_INPUT_PROTOCOL_VERSION
        );
        assert_eq!(
            available.input_capabilities.region_input,
            InputCapabilityAvailability::Available
        );
        let json = serde_json::to_value(available).expect("ServerHello must serialize");
        assert_eq!(
            json["input_protocol_version"],
            REGION_INPUT_PROTOCOL_VERSION
        );
        assert_eq!(json["input_capabilities"]["region_input"], "available");
    }

    #[test]
    fn hello_advertises_pen_truth_only_when_the_tablet_backend_was_established() {
        let cfg = Config::default();
        let plan = native_plan(VideoCodec::H264, false);

        let unavailable = build_server_hello(&cfg, &plan, None, false, false, false, false);
        for availability in [
            unavailable.input_capabilities.pen,
            unavailable.input_capabilities.pen_pressure,
            unavailable.input_capabilities.pen_tilt,
            unavailable.input_capabilities.pen_eraser,
            unavailable.input_capabilities.pen_proximity,
        ] {
            assert_eq!(availability, InputCapabilityAvailability::Unavailable);
        }

        let available = build_server_hello(&cfg, &plan, None, false, false, true, false);
        for availability in [
            available.input_capabilities.pen,
            available.input_capabilities.pen_pressure,
            available.input_capabilities.pen_tilt,
            available.input_capabilities.pen_eraser,
            available.input_capabilities.pen_proximity,
        ] {
            assert_eq!(availability, InputCapabilityAvailability::Available);
        }
    }

    /// Rotation must never be advertised even when the tablet device was
    /// created and every other pen capability is `Available`: this backend
    /// has not proven a kernel/libinput axis is recognized as tablet
    /// rotation on any target, and the honest `Unavailable` default is
    /// preferable to an unproven claim.
    #[test]
    fn hello_never_advertises_pen_rotation_regardless_of_tablet_availability() {
        let cfg = Config::default();
        let plan = native_plan(VideoCodec::H264, false);
        for pen_available in [false, true] {
            let hello = build_server_hello(&cfg, &plan, None, false, false, pen_available, false);
            assert_eq!(
                hello.input_capabilities.pen_rotation,
                InputCapabilityAvailability::Unavailable
            );
        }
    }

    #[test]
    fn hello_advertises_only_the_configured_audio_codec() {
        let plan = native_plan(VideoCodec::H265, true);
        for (compressed, expected) in [
            (false, arcen_protocol::AudioCodec::Pcm),
            (true, arcen_protocol::AudioCodec::Opus),
        ] {
            let cfg = Config {
                audio_enabled: true,
                audio_compressed: compressed,
                ..Config::default()
            };
            assert_eq!(
                build_server_hello(&cfg, &plan, None, false, false, false, false)
                    .audio_output
                    .unwrap()
                    .codecs,
                vec![expected]
            );
        }
    }

    #[test]
    fn resolved_plan_overrides_requested_config_truthfully() {
        let cfg = Config {
            codec: "h265".to_string(),
            chroma: "yuv444".to_string(),
            ..Config::default()
        };
        let plan = ResolvedMediaPlan {
            backend: EncoderBackend::OpenH264,
            video: VideoConfiguration {
                codec: VideoCodec::H264,
                chroma: ChromaSubsampling::Yuv420,
                bit_depth: BitDepth::Eight,
                range: arcen_media::ColorRange::Limited,
                matrix: arcen_media::ColorMatrix::Bt709,
                primaries: arcen_media::ColorPrimaries::Bt709,
                transfer: arcen_media::TransferCharacteristics::Bt709,
            },
            width: 1920,
            height: 1080,
            fps: 30,
            codecs: arcen_media::CodecSet::from_slice(&[arcen_media::VideoCodec::H264]),
            chroma: arcen_media::ChromaSet::from_slice(&[arcen_media::ChromaSubsampling::Yuv420]),
            bit_depths: EncoderBackend::OpenH264.contract().bit_depths,
            ranges: EncoderBackend::OpenH264.contract().ranges,
            cursor_mode: arcen_protocol::messages::CursorMode::Local,
            cursor_in_video: false,
        };
        let hello = build_server_hello(&cfg, &plan, None, false, false, false, false);
        assert_eq!(hello.encoder_backend, "openh264-sw-h264");
        assert_eq!(hello.codec, "h264");
        assert!(!hello.color_caps.chroma_444);
        assert!(!hello.supports_h265);
        assert!(!hello.supports_yuv444);
    }

    /// `ServerColorCaps` used to hardcode `main10`/`main12`/`chroma_422`/
    /// `full_range`/`identity_matrix` to `false` on every host, which made
    /// them decorative. They now read the resolved backend's real
    /// capability, and the `active_*` fields separately report what the
    /// plan is actually serving right now -- distinct concepts that must
    /// not be conflated (see `ServerColorCaps`'s own doc).
    #[test]
    fn color_caps_report_real_backend_capability_not_hardcoded_false() {
        let cfg = Config::default();
        let plan = native_plan(VideoCodec::H265, true);
        let hello = build_server_hello(&cfg, &plan, None, false, false, false, false);
        // NativeNvenc's contract offers ten-bit, 4:4:4, an identity matrix,
        // and full range -- all real capability, not whatever `native_plan`'s
        // active axes happen to be set to.
        assert!(hello.color_caps.main10);
        assert!(hello.color_caps.chroma_444);
        assert!(hello.color_caps.full_range);
        assert!(hello.color_caps.identity_matrix);
        // NVENC has no twelve-bit mode at all.
        assert!(!hello.color_caps.main12);
        // 4:2:2 is absent, and this is the assertion that proves the contract
        // fix reaches the client. Blackwell silicon encodes 4:2:2, but the
        // vendored NVENCAPI 12.1 bindings have no surface format to name it
        // with, so this build cannot. Until this test was written the other
        // way round, every ServerHello told every client 4:2:2 was available
        // and the encoder then refused it at init.
        // See docs/architecture/nvenc-sdk13-blackwell.md.
        assert!(!hello.color_caps.chroma_422);
        // `native_plan(H265, true)`'s active axes are 8-bit/limited/BT.709.
        assert_eq!(hello.color_caps.active_bit_depth, "8");
        assert_eq!(hello.color_caps.active_range, "limited");
        assert_eq!(hello.color_caps.active_matrix, "bt709");
        assert_eq!(hello.color_caps.active_primaries, "bt709");
        assert_eq!(hello.color_caps.active_transfer, "bt709");
    }

    #[test]
    fn hello_serializes_with_type_field() {
        let cfg = Config::default();
        let plan = native_plan(VideoCodec::H264, false);
        let json = serde_json::to_value(build_server_hello(
            &cfg, &plan, None, false, false, false, false,
        ))
        .unwrap();
        assert_eq!(json["type"], "server_hello");
        assert_eq!(json["codec"], "h264");
    }

    #[test]
    fn hello_advertises_display_update_only_when_display_is_held() {
        let cfg = Config::default();
        let plan = native_plan(VideoCodec::H264, false);
        assert!(
            !build_server_hello(&cfg, &plan, None, false, false, false, false)
                .supports_display_update
        );
        assert!(
            build_server_hello(&cfg, &plan, None, true, false, false, false)
                .supports_display_update
        );
    }

    #[test]
    fn pam_hello_reports_authentication_requirement() {
        let cfg = Config {
            auth_mode: AuthMode::Pam,
            ..Config::default()
        };
        let plan = native_plan(VideoCodec::H264, false);
        assert!(build_server_hello(&cfg, &plan, None, false, false, false, false).requires_auth);
    }

    #[test]
    fn hello_reports_authenticated_os_session_without_claiming_isolation() {
        let metadata = SessionMetadata {
            username: "artist".into(),
            uid: 1001,
            session_id: "c7".into(),
            session_type: "x11".into(),
            desktop: "gnome-classic".into(),
            display: ":0".into(),
            agent_pid: 42,
            generation: 1,
            reconnected: false,
            timezone: None,
            multi_monitor_plan: None,
            multi_monitor_carrier: None,
        };
        let plan = native_plan(VideoCodec::H264, false);
        let hello = build_server_hello(
            &Config::default(),
            &plan,
            Some(&metadata),
            false,
            false,
            false,
            false,
        );
        assert_eq!(hello.os_user, "artist");
        assert_eq!(hello.session_id, "c7");
        assert_eq!(hello.session_type, "x11");
        assert_eq!(hello.desktop, "gnome-classic");
        assert!(!hello.device_capabilities.contains_key("session"));
    }

    #[test]
    #[cfg(feature = "wss-compat")]
    fn transport_negotiation_sanitizes_unknown_entries_and_keeps_wss() {
        let negotiated = negotiate_client_transport(
            &[
                "unknown:cap".to_string(),
                CAPABILITY_TRANSPORT_WSS.to_string(),
                CAPABILITY_TRANSPORT_QUIC.to_string(),
                "transport:bogus-v9".to_string(),
            ],
            CAPABILITY_TRANSPORT_WSS,
        );
        assert_eq!(negotiated.as_deref(), Some(CAPABILITY_TRANSPORT_WSS));
    }

    #[test]
    #[cfg(feature = "wss-compat")]
    fn transport_negotiation_returns_none_for_no_common_capability() {
        let negotiated = negotiate_client_transport(
            &[CAPABILITY_TRANSPORT_QUIC.to_string()],
            CAPABILITY_TRANSPORT_WSS,
        );
        assert!(negotiated.is_none());
    }

    #[test]
    fn quic_transport_requires_and_accepts_the_quic_capability() {
        assert_eq!(
            negotiate_client_transport(
                &[CAPABILITY_TRANSPORT_QUIC.to_string()],
                CAPABILITY_TRANSPORT_QUIC,
            ),
            Some(CAPABILITY_TRANSPORT_QUIC.to_string())
        );
        assert!(negotiate_client_transport(&[], CAPABILITY_TRANSPORT_QUIC).is_none());
    }

    #[test]
    #[cfg(feature = "wss-compat")]
    fn legacy_client_without_transport_capabilities_uses_wss() {
        assert_eq!(
            negotiate_client_transport(&[], CAPABILITY_TRANSPORT_WSS),
            Some(CAPABILITY_TRANSPORT_WSS.to_string())
        );
    }
}
