// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Lab only (`--features lab`): a headless Windows Deck session that asks a
//! Pier for Native tablet and bridges the tablet through the Arcen USB
//! component, exactly as the Deck's session engine will: the shared
//! `arcen-deck-core` session, `DeckServices::start_usb_tablet_responder` →
//! [`arcen_usb_client::start_responder`] → the verified pipe → the service.
//!
//! ```text
//! forward_lab <host> [port] --pin-sha256 FP [--seconds N] [--state]
//! ```
//!
//! Credentials: two lines (user, password) on standard input, never on the
//! command line. Video is requested as H.264 and dropped undecoded (no
//! decoder is involved in a tablet proof). `--state` prints the component
//! state the settings screen would show and exits.

#[cfg(windows)]
fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "--state") {
        let state = arcen_usb_client::component_state();
        let platform = arcen_usb_client::PLATFORM;
        println!("facts: {:?}", arcen_usb_client::component_facts());
        println!("state: {state:?}");
        println!("label: {}", state.action_label(platform));
        println!("guidance: {}", state.guidance(platform));
        println!("action: {:?}", state.action());
        return std::process::ExitCode::SUCCESS;
    }
    // The session engine's own log lines, on standard error (RUST_LOG).
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .try_init();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("forward_lab: runtime: {error}");
            return std::process::ExitCode::FAILURE;
        }
    };
    match runtime.block_on(lab::run(&args)) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("forward_lab failed: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("forward_lab runs on Windows only");
}

#[cfg(windows)]
mod lab {
    use std::net::IpAddr;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;

    use arcen_deck_core::DeckServices;
    use arcen_deck_core::clipboard::PlatformClipboard;
    use arcen_deck_core::decode::{DecodeCapabilities, DecoderBackendFamily};
    use arcen_deck_core::microphone::{
        MicrophoneCancel, MicrophoneCaptureError, MicrophoneFrameSink, MicrophoneStream,
    };
    use arcen_deck_core::network::{
        NetworkIdentityPermission, NetworkProbeResult, OutgoingInterface,
    };
    use arcen_deck_core::runtime::session::{
        AuthSubmission, ConnectOptions, SessionCommand, SessionEvent, StreamProfile, spawn_session,
    };
    use arcen_deck_core::runtime::tls::{TlsTrustConfig, parse_fingerprint};
    use arcen_deck_core::runtime::usb::{BoxFuture, UsbTabletResponder};
    use arcen_input::CapabilityAvailability;
    use arcen_protocol::messages::{
        BuildIdentityMsg, ClientMonitor, CursorMode, DisplayColorMsg, TABLET_MODE_RESULT,
        TabletModeMsg, TabletModeResultMsg, msg_type,
    };

    /// Only what a tablet proof needs: one 1920×1080 display, H.264
    /// advertised (frames are dropped), the component for Native tablet.
    #[derive(Debug)]
    struct LabServices;

    impl DeckServices for LabServices {
        fn build_identity(&self) -> BuildIdentityMsg {
            arcen_protocol::build_identity::this_build(
                "arcen-usb-client-lab",
                env!("CARGO_PKG_VERSION"),
            )
        }
        fn platform_summary(&self) -> String {
            "Windows (arcen-usb-client forward_lab)".to_owned()
        }
        fn native_config_dir(&self) -> Option<PathBuf> {
            None
        }
        fn native_log_dir(&self) -> Option<PathBuf> {
            None
        }
        fn decode_capabilities(&self) -> DecodeCapabilities {
            DecodeCapabilities {
                backend: DecoderBackendFamily::D3d11Video,
                h264: true,
                ..DecodeCapabilities::default()
            }
        }
        fn displays(&self) -> Vec<ClientMonitor> {
            vec![monitor()]
        }
        fn display_color(&self, _display_id: u32) -> Option<DisplayColorMsg> {
            None
        }
        fn captures_typed_pen(&self) -> bool {
            false
        }
        fn tablet_presence(&self) -> CapabilityAvailability {
            CapabilityAvailability::Unknown
        }
        fn open_clipboard(&self) -> Option<Box<dyn PlatformClipboard>> {
            None
        }
        fn start_microphone(
            &self,
            _sink: Arc<dyn MicrophoneFrameSink>,
            _cancel: MicrophoneCancel,
        ) -> Result<Box<dyn MicrophoneStream>, MicrophoneCaptureError> {
            Err(MicrophoneCaptureError::UnsupportedPlatform)
        }
        fn probe_network(&self, _host: &str, _port: u16) -> NetworkProbeResult {
            NetworkProbeResult {
                snapshot: None,
                permission: NetworkIdentityPermission::NotRequested,
            }
        }
        fn outgoing_interface(&self, _peer: IpAddr) -> OutgoingInterface {
            OutgoingInterface::default()
        }
        fn timezone(&self) -> Option<String> {
            None
        }
        fn native_tablet_supported(&self) -> bool {
            true
        }
        fn start_usb_tablet_responder(
            &self,
        ) -> BoxFuture<'static, Result<Box<dyn UsbTabletResponder>, String>> {
            Box::pin(async {
                let started = std::time::Instant::now();
                let outcome = arcen_usb_client::start_responder().await;
                match &outcome {
                    Ok(responder) => {
                        let device = responder.device();
                        println!(
                            "component: captured {:04x}:{:04x} ({} report descriptor(s)) in {} ms",
                            device.vendor_id,
                            device.product_id,
                            device.hid_report_descriptors.len(),
                            started.elapsed().as_millis()
                        );
                    }
                    Err(error) => println!("component: refused: {error}"),
                }
                outcome
            })
        }
    }

    fn monitor() -> ClientMonitor {
        ClientMonitor {
            id: 1,
            x: 0,
            y: 0,
            width_px: 1920,
            height_px: 1080,
            scale: 1.0,
            refresh_hz: 60,
            is_primary: true,
            name: "forward_lab".to_owned(),
            width_mm: 0.0,
            height_mm: 0.0,
            vendor: 0,
            model: 0,
            serial: 0,
            edid: String::new(),
            color: None,
            rotation_degrees: 0,
        }
    }

    fn flag(args: &[String], name: &str) -> Option<String> {
        args.iter()
            .position(|arg| arg == name)
            .and_then(|index| args.get(index + 1).cloned())
    }

    fn credentials() -> Result<AuthSubmission, String> {
        let mut text = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut text)
            .map_err(|error| format!("read credentials: {error}"))?;
        let mut lines = text.lines().map(str::trim_end);
        match (lines.next(), lines.next()) {
            (Some(username), Some(password)) if !username.is_empty() && !password.is_empty() => {
                Ok(AuthSubmission {
                    username: username.to_owned(),
                    password: password.to_owned(),
                })
            }
            _ => Err("standard input must hold two lines: user, password".to_owned()),
        }
    }

    fn connect_options(
        host: String,
        port: u16,
        pin: &str,
        tablet_mode: TabletModeMsg,
    ) -> Result<ConnectOptions, String> {
        Ok(ConnectOptions {
            services: Arc::new(LabServices),
            host,
            port,
            use_tls: true,
            username: String::new(),
            password: String::new(),
            timeout: Duration::from_secs(30),
            tls: TlsTrustConfig::pinned(parse_fingerprint(pin).map_err(|error| error.to_string())?),
            profile: StreamProfile::default(),
            monitors: vec![monitor()],
            displays_mode: "single_primary".to_owned(),
            multi_monitor_topology: None,
            replace_incompatible_desktop: false,
            timezone: None,
            cursor_preference: CursorMode::Local,
            clipboard_enabled: false,
            microphone_enabled: false,
            tablet_input_enabled: true,
            tablet_mode_requested: tablet_mode,
            telemetry: Arc::new(arcen_deck_core::client_telemetry::ClientTelemetry::default()),
            exact_still_image: false,
        })
    }

    async fn close(session: &mut arcen_deck_core::runtime::session::SessionHandle) {
        let _ = session.commands.send(SessionCommand::Close);
        let _ = tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(event) = session.events.recv().await {
                if matches!(event, SessionEvent::Ended(_)) {
                    break;
                }
            }
        })
        .await;
    }

    /// `<host> [port] --pin-sha256 FP [--seconds N]`.
    fn parse(args: &[String]) -> Result<(String, u16, String, u64), String> {
        let host = args
            .first()
            .cloned()
            .ok_or("usage: forward_lab <host> [port] --pin-sha256 FP")?;
        let port = args
            .get(1)
            .filter(|value| !value.starts_with("--"))
            .map(|value| {
                value
                    .parse::<u16>()
                    .map_err(|_| format!("bad port {value}"))
            })
            .transpose()?
            .unwrap_or(18444);
        let pin = flag(args, "--pin-sha256").ok_or("--pin-sha256 is required")?;
        let seconds = flag(args, "--seconds")
            .map(|value| {
                value
                    .parse::<u64>()
                    .map_err(|_| format!("bad --seconds {value}"))
            })
            .transpose()?
            .unwrap_or(30);
        Ok((host, port, pin, seconds))
    }

    pub(crate) async fn run(args: &[String]) -> Result<(), String> {
        let (host, port, pin, seconds) = parse(args)?;
        let mut auth = Some(credentials()?);
        // `--light`: a control run with Tablet support instead of Native.
        let tablet_mode = if args.iter().any(|arg| arg == "--light") {
            TabletModeMsg::LocalTermination
        } else {
            TabletModeMsg::UsbTabletBridge
        };
        let options = connect_options(host, port, &pin, tablet_mode)?;
        let started = std::time::Instant::now();
        let mut session = spawn_session(options);
        let mut accepted = None;
        let mut positions = 0_u64;
        let mut first_position = None;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(seconds);
        let outcome = loop {
            let wait = tokio::time::timeout_at(deadline, session.events.recv()).await;
            let Ok(event) = wait else { break Ok(()) };
            let Some(event) = event else {
                break Err("session ended".to_owned());
            };
            match event {
                SessionEvent::AuthRequired(request) => {
                    if request.disclaimer.is_some() {
                        break Err(
                            "the Pier shows a disclaimer; accept it interactively".to_owned()
                        );
                    }
                    let Some(submission) = auth.take() else {
                        break Err("the Pier asked for credentials twice".to_owned());
                    };
                    let _ = session
                        .commands
                        .send(SessionCommand::SubmitAuth(submission));
                }
                SessionEvent::Authenticated(_) => {
                    let _ = session.commands.send(SessionCommand::AcceptAuthentication);
                }
                SessionEvent::CertificateUntrusted(_) => {
                    break Err("the Pier's certificate does not match the pin".to_owned());
                }
                SessionEvent::ServerHello(hello) => {
                    println!(
                        "+{} ms server hello: usb_hard_v1={}",
                        started.elapsed().as_millis(),
                        hello.usb_hard_v1
                    );
                }
                SessionEvent::Json(value) if msg_type(&value) == Some(TABLET_MODE_RESULT) => {
                    let result: TabletModeResultMsg =
                        serde_json::from_value(value).map_err(|error| error.to_string())?;
                    println!(
                        "+{} ms tablet mode: accepted={} active={:?} reason={}",
                        started.elapsed().as_millis(),
                        result.accepted,
                        result.active,
                        result.reason.as_str()
                    );
                    accepted =
                        Some(result.accepted && result.active == TabletModeMsg::UsbTabletBridge);
                }
                SessionEvent::CursorPosition(_) => {
                    positions += 1;
                    first_position.get_or_insert(started.elapsed());
                }
                SessionEvent::MediaReady => {
                    let _ = session.media.take_batch();
                }
                SessionEvent::Ended(end) => break Err(format!("session ended: {}", end.message)),
                _ => {}
            }
        };
        close(&mut session).await;
        println!(
            "forward_lab done: native_tablet_accepted={} host_pointer_positions={positions} first_position_ms={}",
            accepted.map_or_else(|| "none".to_owned(), |value| value.to_string()),
            first_position.map_or_else(|| "-".to_owned(), |at| at.as_millis().to_string())
        );
        outcome?;
        match accepted {
            Some(true) => Ok(()),
            Some(false) => Err("the Pier did not accept Native tablet".to_owned()),
            None => Err("the Pier never answered the tablet mode request".to_owned()),
        }
    }
}
