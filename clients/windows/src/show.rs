// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! `arcen-deck show`: connect and show the host's desktop in a plain window.
//!
//! The proof of the Windows media composition without the GUI shell:
//! the session engine, Arcen's D3D11 decoder on the window's adapter, the
//! swapchain presenters in the window's composition tree and the shared
//! media worker, exactly as [`crate::media`] composes them for the shell.
//! View only: no input, no audio output (the WASAPI output is the platform
//! stream's), no prompts. Credentials come from standard input, never from
//! the command line.
//!
//! ```text
//! arcen-deck show <host> [port] [--pin-sha256 FP] [--credentials-stdin]
//!                 [--pipeline auto|speed|grading|hdr] [--exact-still-image]
//!                 [--seconds N]
//! ```

#![allow(
    unsafe_code,
    reason = "a plain Win32 window: class registration, the window procedure and the message loop"
)]

use std::io::Read;
use std::net::IpAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, PoisonError};
use std::time::{Duration, Instant};

use arcen_deck_core::DeckServices;
use arcen_deck_core::client_telemetry::ClientTelemetry;
use arcen_deck_core::clipboard::PlatformClipboard;
use arcen_deck_core::decode::DecodeCapabilities;
use arcen_deck_core::microphone::{
    MicrophoneCancel, MicrophoneCaptureError, MicrophoneFrameSink, MicrophoneStream,
};
use arcen_deck_core::network::{NetworkIdentityPermission, NetworkProbeResult, OutgoingInterface};
use arcen_deck_core::present::RepaintRequester;
use arcen_deck_core::runtime::audio_player::{AudioOutputDevice, PcmAudioPlayer, PlaybackSnapshot};
use arcen_deck_core::runtime::session::{
    AuthSubmission, ConnectOptions, SessionCommand, StreamProfile, spawn_session,
};
use arcen_deck_core::runtime::tls::{TlsTrustConfig, parse_fingerprint};
use arcen_input::CapabilityAvailability;
use arcen_media::video::PipelineId;
use arcen_protocol::messages::{
    BuildIdentityMsg, ClientMonitor, CursorMode, DisplayColorMsg, TabletModeMsg,
};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{MONITOR_DEFAULTTONEAREST, MonitorFromWindow};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW, DispatchMessageW,
    GetClientRect, IDC_ARROW, LoadCursorW, MSG, PM_REMOVE, PeekMessageW, PostQuitMessage,
    RegisterClassExW, SW_SHOW, ShowWindow, TranslateMessage, WM_DESTROY, WM_QUIT, WNDCLASSEXW,
    WS_EX_NOREDIRECTIONBITMAP, WS_OVERLAPPEDWINDOW,
};
use windows::core::w;
use zeroize::Zeroize;

use crate::media::{MediaSession, MediaStart, VideoSurface};
use crate::present::composition::CompositionTree;
use crate::present::{AdapterChoice, Gpu};

const USAGE: &str = "usage: arcen-deck show <host> [port] [--pin-sha256 FP] [--credentials-stdin] \
                     [--pipeline auto|speed|grading|hdr] [--exact-still-image] [--seconds N]";
/// Upper bound for `--credentials-stdin` (user name and password lines).
const MAX_CREDENTIALS_BYTES: usize = 4096;
const STATS_INTERVAL: Duration = Duration::from_secs(5);

/// What `show` was asked to do.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ShowArgs {
    host: String,
    port: u16,
    pin: Option<String>,
    credentials_stdin: bool,
    pipeline: Option<PipelineId>,
    exact_still_image: bool,
    seconds: Option<u64>,
}

fn parse_args(args: &[String]) -> Result<ShowArgs, String> {
    let mut positional = Vec::new();
    let mut parsed = ShowArgs {
        host: String::new(),
        port: 18_444,
        pin: None,
        credentials_stdin: false,
        pipeline: None,
        exact_still_image: false,
        seconds: None,
    };
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        let mut value = || {
            index += 1;
            args.get(index)
                .cloned()
                .ok_or_else(|| format!("{arg} needs a value\n{USAGE}"))
        };
        match arg {
            "--pin-sha256" => parsed.pin = Some(value()?),
            "--credentials-stdin" => parsed.credentials_stdin = true,
            "--exact-still-image" => parsed.exact_still_image = true,
            "--pipeline" => {
                parsed.pipeline = Some(match value()?.as_str() {
                    "auto" => PipelineId::Auto,
                    "speed" => PipelineId::Speed,
                    "grading" => PipelineId::Grading,
                    "hdr" => PipelineId::Hdr,
                    _ => {
                        return Err(format!(
                            "--pipeline must be auto, speed, grading or hdr\n{USAGE}"
                        ));
                    }
                });
            }
            "--seconds" => {
                parsed.seconds = Some(
                    value()?
                        .parse()
                        .map_err(|_| format!("--seconds needs a whole number\n{USAGE}"))?,
                );
            }
            flag if flag.starts_with("--") => {
                return Err(format!("unknown option {flag}\n{USAGE}"));
            }
            _ => positional.push(arg.to_owned()),
        }
        index += 1;
    }
    match positional.as_slice() {
        [host] => parsed.host.clone_from(host),
        [host, port] => {
            parsed.host.clone_from(host);
            parsed.port = port
                .parse()
                .ok()
                .filter(|port| *port != 0)
                .ok_or_else(|| format!("the port must be 1-65535\n{USAGE}"))?;
        }
        _ => return Err(USAGE.to_owned()),
    }
    Ok(parsed)
}

/// The stream profile of one of the four complete contracts (the macOS
/// Deck CLI's `--pipeline`).
fn stream_profile_for(pipeline: PipelineId) -> StreamProfile {
    let contract = arcen_media::video::pipeline_contract(pipeline);
    StreamProfile {
        codec: contract.colour.codec.token().to_owned(),
        chroma: contract.colour.chroma.token().to_owned(),
        video_selection: contract.selection,
        max_fps: contract.max_fps,
        bit_depth: contract.colour.bit_depth.token().to_owned(),
        color_range: contract.colour.range.token().to_owned(),
        color_matrix: contract.colour.matrix.token().to_owned(),
        transfer: contract.colour.transfer.token().to_owned(),
        color_primaries: contract.colour.primaries.token().to_owned(),
        encode_intent: contract.intent.token().to_owned(),
        motion_priority: contract.priority.token().to_owned(),
        pipeline: contract.id.request_wire(),
    }
}

/// Standard input without std's process-wide buffer, so the credentials
/// are not left in a buffer this command cannot wipe.
fn unbuffered_stdin() -> std::mem::ManuallyDrop<std::fs::File> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    // The handle stays owned by the process: ManuallyDrop never closes it.
    std::mem::ManuallyDrop::new(unsafe {
        std::fs::File::from_raw_handle(std::io::stdin().as_raw_handle())
    })
}

/// Reads "user name\npassword\n" from `reader`, bounded; the buffer is
/// wiped.
fn read_credentials(mut reader: impl Read) -> Result<(String, String), String> {
    let mut bytes = Vec::with_capacity(MAX_CREDENTIALS_BYTES + 1);
    let read = (&mut reader)
        .take(MAX_CREDENTIALS_BYTES as u64 + 1)
        .read_to_end(&mut bytes);
    let result = match read {
        Err(error) => Err(format!("cannot read the credentials: {error}")),
        Ok(_) if bytes.len() > MAX_CREDENTIALS_BYTES => {
            Err("the credentials on standard input are too long".to_owned())
        }
        Ok(_) => match std::str::from_utf8(&bytes) {
            Err(_) => Err("the credentials are not UTF-8".to_owned()),
            Ok(text) => {
                let mut lines = text.split('\n').map(|line| line.trim_end_matches('\r'));
                let user = lines.next().unwrap_or_default().to_owned();
                let password = lines.next().unwrap_or_default().to_owned();
                Ok((user, password))
            }
        },
    };
    bytes.zeroize();
    result
}

/// `show`'s services: this build's identity, the window adapter's proven
/// decode profiles, and nothing else (no displays, clipboard, microphone or
/// network facts: the platform stream's `WindowsDeckServices` has those).
#[derive(Debug)]
struct ShowServices {
    decode: DecodeCapabilities,
}

impl DeckServices for ShowServices {
    fn build_identity(&self) -> BuildIdentityMsg {
        arcen_protocol::build_identity::this_build("arcen-deck-windows", crate::VERSION)
    }

    fn platform_summary(&self) -> String {
        "windows".to_owned()
    }

    fn native_config_dir(&self) -> Option<PathBuf> {
        None
    }

    fn native_log_dir(&self) -> Option<PathBuf> {
        None
    }

    fn decode_capabilities(&self) -> DecodeCapabilities {
        self.decode
    }

    fn displays(&self) -> Vec<ClientMonitor> {
        Vec::new()
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
}

/// No audio output in `show`: the audio lane is received and decoded, the
/// statistics say why nothing plays.
struct NoAudioOutput;

impl AudioOutputDevice for NoAudioOutput {
    fn open() -> Result<Self, String> {
        Err("show plays no audio (the WASAPI output is the platform stream's)".to_owned())
    }

    fn backend(&self) -> &'static str {
        "none"
    }

    fn push_s16le(&self, _payload: &[u8]) -> usize {
        0
    }

    fn push_samples(&self, _samples: &[i16]) -> usize {
        0
    }

    fn reset(&self) {}

    fn snapshot(&self) -> PlaybackSnapshot {
        PlaybackSnapshot::default()
    }
}

/// The message loop polls; nothing to wake.
struct LoopPolls;

impl RepaintRequester for LoopPolls {
    fn request_repaint(&self) {}
}

extern "system" fn window_procedure(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_DESTROY {
        unsafe { PostQuitMessage(0) };
        return LRESULT(0);
    }
    unsafe { DefWindowProcW(window, message, wparam, lparam) }
}

fn open_window() -> Result<HWND, String> {
    let instance =
        unsafe { GetModuleHandleW(None) }.map_err(|error| format!("GetModuleHandleW: {error}"))?;
    let class = WNDCLASSEXW {
        cbSize: u32::try_from(std::mem::size_of::<WNDCLASSEXW>()).unwrap_or(u32::MAX),
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(window_procedure),
        hInstance: instance.into(),
        hCursor: unsafe { LoadCursorW(None, IDC_ARROW) }.unwrap_or_default(),
        lpszClassName: w!("ArcenDeckShow"),
        ..WNDCLASSEXW::default()
    };
    if unsafe { RegisterClassExW(&raw const class) } == 0 {
        return Err("RegisterClassExW failed".to_owned());
    }
    let window = unsafe {
        CreateWindowExW(
            WS_EX_NOREDIRECTIONBITMAP,
            w!("ArcenDeckShow"),
            w!("Arcen Deck"),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            1280,
            800,
            None,
            None,
            instance,
            None,
        )
    }
    .map_err(|error| format!("CreateWindowExW: {error}"))?;
    let _ = unsafe { ShowWindow(window, SW_SHOW) };
    Ok(window)
}

fn client_size(window: HWND) -> (u32, u32) {
    let mut rect = RECT::default();
    if unsafe { GetClientRect(window, &raw mut rect) }.is_err() {
        return (1, 1);
    }
    (
        u32::try_from(rect.right - rect.left).unwrap_or(1).max(1),
        u32::try_from(rect.bottom - rect.top).unwrap_or(1).max(1),
    )
}

/// Pumps the window's messages; `false` once the window closed.
fn pump_messages() -> bool {
    let mut message = MSG::default();
    while unsafe { PeekMessageW(&raw mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
        if message.message == WM_QUIT {
            return false;
        }
        unsafe {
            let _ = TranslateMessage(&raw const message);
            DispatchMessageW(&raw const message);
        }
    }
    true
}

/// Runs `show` with its arguments (after the subcommand).
#[must_use]
pub fn run(args: &[String]) -> ExitCode {
    match show(args) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(error) => {
            eprintln!("show failed: {error}");
            ExitCode::FAILURE
        }
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "one linear lab command: window, device, session, loop, verdict"
)]
fn show(args: &[String]) -> Result<bool, String> {
    let args = parse_args(args)?;
    // With ARCEN_LOG_DIR the shared log records the session as the window
    // shell's does (QUIC path snapshots, socket buffers, decode truth).
    let _log_dir = arcen_deck_core::logging::init(crate::logging::log_init(
        arcen_deck_core::logging::startup_profile(),
        arcen_deck_core::directories::DeckDirectories::from_process_env(None, None).logs,
        &crate::platform::build::DeckBuildFacts::running(),
    ));
    let tls = match &args.pin {
        Some(pin) => TlsTrustConfig::pinned(
            parse_fingerprint(pin).map_err(|error| format!("--pin-sha256: {error}"))?,
        ),
        None => TlsTrustConfig::system_ca(),
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("arcen-deck")
        .build()
        .map_err(|error| format!("cannot start the runtime: {error}"))?;
    let _context = runtime.enter();

    let window = open_window()?;
    let monitor = unsafe { MonitorFromWindow(window, MONITOR_DEFAULTTONEAREST) };
    let gpu = Gpu::create(&AdapterChoice::Monitor(monitor.0 as isize))
        .or_else(|_| Gpu::create(&AdapterChoice::HighPerformance))?;
    let probe = crate::decode::probe_report_for_adapter(gpu.luid);
    println!(
        "adapter={} decode h264={} h265={} main10={} yuv444={} refused={:?}",
        gpu.adapter_name,
        probe.capabilities.h264,
        probe.capabilities.h265,
        probe.capabilities.main10,
        probe.capabilities.yuv444,
        probe.refusals
    );
    let mut tree = CompositionTree::new(&gpu, window)?;
    tree.set_client_size(client_size(window))?;
    let surface = VideoSurface {
        gpu,
        tree: Arc::new(std::sync::Mutex::new(tree)),
    };

    let telemetry = Arc::new(ClientTelemetry::default());
    // Read last, so no early return leaves them in memory; ConnectOptions
    // wipes the password when it is dropped.
    let (username, password) = if args.credentials_stdin {
        read_credentials(&*unbuffered_stdin())?
    } else {
        (String::new(), String::new())
    };
    let options = ConnectOptions {
        display_refresh: None,
        services: Arc::new(ShowServices {
            decode: probe.capabilities,
        }),
        host: args.host.clone(),
        port: args.port,
        use_tls: true,
        // The credentials answer the host's sign-in request (below), as
        // the macOS Deck's CLI does; they never sit in the options.
        username: String::new(),
        password: String::new(),
        timeout: Duration::from_secs(15),
        tls,
        profile: args
            .pipeline
            .map_or_else(StreamProfile::default, stream_profile_for),
        monitors: Vec::new(),
        displays_mode: "single_primary".to_owned(),
        multi_monitor_topology: None,
        replace_incompatible_desktop: false,
        timezone: None,
        cursor_preference: CursorMode::Local,
        clipboard_enabled: false,
        microphone_enabled: false,
        tablet_input_enabled: false,
        tablet_mode_requested: TabletModeMsg::LocalTermination,
        telemetry: Arc::clone(&telemetry),
        exact_still_image: args.exact_still_image,
        keep_desk_private: true,
        canvas: arcen_deck_core::canvas::CanvasRequest::Unavailable,
        // A tool that cannot answer keeps the host's refusal.
        account_switch: arcen_deck_core::account_switch::AccountSwitchMode::Off,
    };
    let mut auth = Some(AuthSubmission { username, password });
    let requested_video = arcen_deck_core::connect_plan::profile_video(&options.profile);
    let handle = spawn_session(options);
    let mut session = MediaSession::start(
        handle,
        &surface,
        Arc::new(LoopPolls),
        MediaStart {
            generation: 1,
            resuming: false,
            telemetry,
            audio_player: Box::new(|| {
                Box::new(PcmAudioPlayer::<NoAudioOutput>::awaiting_stream_result())
            }),
            requested_video,
        },
    )?;

    let started = Instant::now();
    let deadline = args
        .seconds
        .map(|seconds| started + Duration::from_secs(seconds));
    let mut last_stats = Instant::now();
    let mut last_size = client_size(window);
    let mut window_open = true;
    while window_open && !session.is_closed() {
        window_open = pump_messages();
        let size = client_size(window);
        if size != last_size {
            last_size = size;
            let _ = surface
                .tree
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .set_client_size(size);
        }
        let _ = session.poll();
        let request = session
            .shared()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pending_auth
            .take();
        if let Some(request) = request {
            if request.disclaimer.is_some() {
                println!("show: the host asks to accept a disclaimer; show does not accept it");
                break;
            }
            let Some(submission) = auth.take() else {
                println!("show: the host asked to sign in again (wrong user name or password?)");
                break;
            };
            if session
                .commands()
                .send(SessionCommand::SubmitAuth(submission))
                .is_err()
            {
                break;
            }
        }
        if last_stats.elapsed() >= STATS_INTERVAL {
            last_stats = Instant::now();
            print_stats(&session, started, args.exact_still_image);
        }
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            break;
        }
        std::thread::sleep(Duration::from_millis(8));
    }
    let _ = session.poll();
    print_stats(&session, started, args.exact_still_image);
    // On glass, not only handed over: a presenter thread confirmed a present.
    let on_glass = session.has_presented_picture();
    let (presented, reason) = {
        let state = session
            .shared()
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let presented = state.video_frames_decoded > 0 && on_glass;
        let reason = state
            .error
            .clone()
            .or_else(|| state.decoder_refusal.clone())
            .or_else(|| state.last_decode_error.clone());
        (presented, reason)
    };
    session.close();
    drop(session);
    if presented {
        println!("show complete: video decoded and presented");
    } else {
        println!(
            "show ended without presented video: {}",
            reason.as_deref().unwrap_or("no frame arrived")
        );
    }
    Ok(presented)
}

fn print_stats(session: &MediaSession, started: Instant, exact_requested: bool) {
    let on_glass = session.has_presented_picture();
    let state = session
        .shared()
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let hello = state.server_hello.as_ref();
    let (submitted, confirmed) = session.presented_frames();
    println!(
        "show elapsed_ms={} presented_submitted={submitted} presented_confirmed={confirmed} \
         on_glass={} server={:?} pipeline={:?} encoder={:?} frames_seen={} frames_decoded={} \
         decoder={} hardware={:?} eight_bit={:?} ten_bit={:?} waiting_for_keyframe={} \
         refusal={:?} decode_error={:?} audio_frames={} closed={} error={:?}",
        started.elapsed().as_millis(),
        on_glass,
        hello.map(|hello| hello.server_name.as_str()),
        hello.and_then(|hello| hello.active_pipeline.as_ref()),
        hello.map(|hello| hello.encoder_backend.as_str()),
        state.video_frames_seen,
        state.video_frames_decoded,
        state.decoder_backend_name,
        state.decoder_hardware_accelerated,
        state.eight_bit_presentation_status,
        state.ten_bit_presentation_status,
        state.waiting_for_keyframe,
        state.decoder_refusal,
        state.last_decode_error,
        state.audio_frames_seen,
        state.closed,
        state.error,
    );
    // The Stream Statistics line, as the shell and the macOS Deck word it.
    let exact = arcen_deck_core::screens::statistics::exact_status_line(
        exact_requested,
        hello.and_then(|hello| hello.lossless_refine.as_ref()),
        || {
            session
                .refine()
                .status(arcen_media::refine::PRIMARY_MONITOR)
                .text
        },
    );
    println!(
        "show exact elapsed_ms={} {exact}",
        started.elapsed().as_millis()
    );
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "tests")]
mod tests {
    use super::*;

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| (*arg).to_owned()).collect()
    }

    #[test]
    fn show_takes_a_host_an_optional_port_and_its_options() {
        let args = parse_args(&strings(&[
            "203.0.113.82",
            "18444",
            "--pin-sha256",
            "AA:BB",
            "--credentials-stdin",
            "--pipeline",
            "grading",
            "--exact-still-image",
            "--seconds",
            "30",
        ]))
        .expect("valid");
        assert_eq!(args.host, "203.0.113.82");
        assert_eq!(args.port, 18_444);
        assert_eq!(args.pin.as_deref(), Some("AA:BB"));
        assert!(args.credentials_stdin);
        assert_eq!(args.pipeline, Some(PipelineId::Grading));
        assert!(args.exact_still_image);
        assert_eq!(args.seconds, Some(30));
        assert!(parse_args(&strings(&[])).is_err());
        assert!(parse_args(&strings(&["host", "0"])).is_err());
        assert!(parse_args(&strings(&["host", "--password", "x"])).is_err());
    }

    #[test]
    fn credentials_come_from_two_lines_and_are_bounded() {
        let (user, password) =
            read_credentials("lab-user\r\nsecret\n".as_bytes()).expect("two lines");
        assert_eq!(user, "lab-user");
        assert_eq!(password, "secret");
        let long = vec![b'a'; MAX_CREDENTIALS_BYTES + 1];
        assert!(read_credentials(long.as_slice()).is_err());
    }
}
