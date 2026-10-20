//! The session-user side of a headless Wayland desktop.
//!
//! Runs inside the session's own logind scope as the authenticated user.
//! Starts the compositor stack through the shared output transaction,
//! verifies its virtual outputs, owns the paired Mutter `RemoteDesktop` and
//! `ScreenCast` sessions, proves the input target, reports the capture
//! hand-off, serves session-bound input, and tears everything down — the
//! D-Bus sessions first, then the stack — when the compositor dies, Mutter
//! closes the session, or the launcher asks it to stop.

use std::path::PathBuf;
use std::sync::Arc;

use arcen_outputs::{OutputContext, OutputProvider, OutputTransaction, OutputTransactionError};
use arcen_session::desktop_session::first_login::FirstLoginSetup;
use arcen_session::wayland_outputs::{PlacedOutput, VirtualOutputMode};
use arcen_session::wayland_session::{ReadinessError, WaylandSessionReadiness};
use arcen_telemetry::CorrelationId;
use futures_util::StreamExt;
use thiserror::Error;
use zeroize::Zeroizing;

use super::audio::WaylandSessionAudio;
use super::clipboard::{ClipboardService, MutterClipboard, CLIPBOARD_INTERFACE};
use super::input::{InputService, SessionFacts};
use super::mutter::{MutterError, MutterSessions};
use super::output::{MutterOutputBinding, MutterOutputError, MutterVirtualOutputProvider};
use super::reconfigure::{LiveOutputs, OutputService, OutputServiceParts, OutputSnapshot};
use super::remote::RemoteSet;
use super::session_services::{LogoutReceiver, SessionServicesConfig};
use super::stack::CompositorColour;
use super::{
    compositor_binary, facts, is_private_runtime_dir, parse_placed_output, CompositorChoiceError,
    InputBackendKind, InputSessionFacts, StreamCursorMode, WaylandAgentReady,
    AGENT_COMPOSITOR_PREFIX_FLAG, AGENT_CURSOR_MODE_FLAG, AGENT_FIDELITY_ROUTE_FLAG,
    AGENT_KEYRING_STDIN_FLAG, AGENT_LAB_INPUT_SEAT_FLAG, AGENT_NATIVE_SEAT_FLAG, AGENT_OUTPUT_FLAG,
    AGENT_RUNTIME_DIR_FLAG, AGENT_X11_FLAG, CLIPBOARD_SOCKET_NAME, FIRST_LOGIN_SETUP_FLAG,
    INITIAL_OUTPUT_GENERATION, INPUT_SOCKET_NAME, OUTPUT_SOCKET_NAME, WAYLAND_DISPLAY_NAME,
    WAYLAND_START_BUDGET,
};
use crate::logging::target;
use crate::session::agent::{emit_local_playback_refusal, AgentAudioArguments, AgentReady};
use arcen_session::compositor_prefix::CompositorPrefix;

/// Longest the X11 proof may take; it runs beside the rest of the start.
const X11_PROOF_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// A headless Wayland agent failure.
#[derive(Debug, Error)]
pub enum WaylandAgentError {
    #[error("Wayland agent arguments are invalid: {0}")]
    Arguments(&'static str),
    #[error("no compositor on this host can run a headless session with virtual monitors")]
    NoCompositor,
    #[error("installed compositor {0} could not be probed")]
    CompositorProbe(String),
    #[error("virtual outputs failed: {0}")]
    Outputs(String),
    #[error("Mutter session failed: {0}")]
    Mutter(#[from] MutterError),
    #[error("readiness evidence was refused: {0}")]
    Readiness(#[from] ReadinessError),
    #[error("output rollback failed: {0}")]
    Rollback(#[from] MutterOutputError),
    #[error("input socket failed: {0}")]
    Input(std::io::Error),
    #[error("ready message failed: {0}")]
    Ready(std::io::Error),
    #[error("the headless session did not start within {0:?}")]
    StartBudget(std::time::Duration),
    #[error("stop signals could not be installed: {0}")]
    Signals(std::io::Error),
    #[error("the headless session was stopped while it was starting")]
    StoppedWhileStarting,
    #[error("the host could not prove its own speakers silent")]
    LocalPlaybackRefused,
}

struct AgentArguments {
    runtime_dir: PathBuf,
    outputs: Vec<PlacedOutput>,
    cursor_mode: StreamCursorMode,
    keyring_stdin: bool,
    first_login: FirstLoginSetup,
    colour: CompositorColour,
    x11: bool,
    compositor_prefix: Option<CompositorPrefix>,
    lab_input_seat: Option<String>,
    native_seat: bool,
}

fn parse_arguments(args: &[String]) -> Result<AgentArguments, WaylandAgentError> {
    let mut runtime_dir = None;
    let mut outputs = Vec::new();
    let mut cursor_mode = None;
    let mut keyring_stdin = false;
    let mut first_login = FirstLoginSetup::Run;
    let mut colour = None;
    let mut x11 = false;
    let mut compositor_prefix = None;
    let mut lab_input_seat = None;
    let mut native_seat = false;
    let mut iter = args.iter();
    while let Some(argument) = iter.next() {
        if argument == AGENT_KEYRING_STDIN_FLAG {
            keyring_stdin = true;
        } else if argument == FIRST_LOGIN_SETUP_FLAG {
            first_login = iter
                .next()
                .and_then(|token| FirstLoginSetup::from_token(token))
                .ok_or(WaylandAgentError::Arguments("invalid first-login setup"))?;
        } else if argument == AGENT_RUNTIME_DIR_FLAG {
            if runtime_dir.is_some() {
                return Err(WaylandAgentError::Arguments("runtime directory repeated"));
            }
            runtime_dir = iter.next().map(PathBuf::from);
        } else if argument == AGENT_X11_FLAG {
            x11 = true;
        } else if argument == AGENT_NATIVE_SEAT_FLAG {
            native_seat = true;
        } else if argument == AGENT_LAB_INPUT_SEAT_FLAG {
            lab_input_seat = Some(
                iter.next()
                    .filter(|seat| arcen_input::seat_pen::is_desktop_seat(seat))
                    .cloned()
                    .ok_or(WaylandAgentError::Arguments("invalid lab input seat"))?,
            );
        } else if argument == AGENT_COMPOSITOR_PREFIX_FLAG {
            if compositor_prefix.is_some() {
                return Err(WaylandAgentError::Arguments("compositor prefix repeated"));
            }
            compositor_prefix = Some(
                iter.next()
                    .and_then(|value| CompositorPrefix::parse(value).ok())
                    .ok_or(WaylandAgentError::Arguments("invalid compositor prefix"))?,
            );
        } else if argument == AGENT_CURSOR_MODE_FLAG {
            if cursor_mode.is_some() {
                return Err(WaylandAgentError::Arguments("cursor mode repeated"));
            }
            cursor_mode = Some(
                iter.next()
                    .and_then(|value| StreamCursorMode::from_token(value))
                    .ok_or(WaylandAgentError::Arguments("invalid cursor mode"))?,
            );
        } else if argument == AGENT_FIDELITY_ROUTE_FLAG {
            if colour.is_some() {
                return Err(WaylandAgentError::Arguments("fidelity route repeated"));
            }
            // Only the forced-HDR route exists on today's compositors; an
            // output colour mode route needs a compositor that offers one on
            // virtual monitors.
            colour = match iter.next().map(String::as_str) {
                Some("force-hdr") => Some(CompositorColour::ForcedHdr),
                _ => return Err(WaylandAgentError::Arguments("invalid fidelity route")),
            };
        } else if argument == AGENT_OUTPUT_FLAG {
            outputs.push(
                iter.next()
                    .and_then(|value| parse_placed_output(value))
                    .ok_or(WaylandAgentError::Arguments("invalid virtual output"))?,
            );
        }
    }
    let runtime_dir = runtime_dir.ok_or(WaylandAgentError::Arguments("no runtime directory"))?;
    if outputs.is_empty() {
        return Err(WaylandAgentError::Arguments("no virtual output"));
    }
    // Several outputs named without positions sit side by side, in order.
    if outputs.len() > 1 && outputs.iter().all(|output| (output.x, output.y) == (0, 0)) {
        let requests = outputs
            .iter()
            .map(|output| output.request)
            .collect::<Vec<_>>();
        outputs = arcen_session::wayland_outputs::left_to_right(&requests);
    }
    Ok(AgentArguments {
        runtime_dir,
        outputs,
        cursor_mode: cursor_mode.unwrap_or_default(),
        keyring_stdin,
        first_login,
        colour: colour.unwrap_or_default(),
        x11,
        compositor_prefix,
        native_seat: native_seat && lab_input_seat.is_some(),
        lab_input_seat,
    })
}

/// How long the launcher's password may take to arrive (it writes it and
/// closes the pipe right after starting the agent).
const PASSWORD_READ_BUDGET: std::time::Duration = std::time::Duration::from_secs(5);

/// Reads the login password the launcher wrote to standard input (then
/// closed), straight from file descriptor 0 into one pre-allocated zeroizing
/// buffer: no standard-library or runtime stdin buffer ever holds it, and
/// the buffer is never reallocated (a reallocation would leave a copy
/// behind). Bounded in size and time; afterwards file descriptor 0 is
/// `/dev/null`. `None` when there is none or it is too long.
fn read_keyring_password() -> Option<Zeroizing<Vec<u8>>> {
    let limit = super::keyring::MAX_PASSWORD_BYTES;
    let mut buffer = Zeroizing::new(vec![0_u8; limit + 1]);
    let filled = read_fd_bounded(0, &mut buffer, PASSWORD_READ_BUDGET);
    redirect_stdin_to_null();
    match filled {
        Some(length) if length > 0 && length <= limit => {
            let mut password = Zeroizing::new(Vec::with_capacity(length));
            password.extend_from_slice(&buffer[..length]);
            Some(password)
        }
        _ => {
            tracing::info!(target: target::SESSION, "no login password reached the agent; the keyring stays locked");
            None
        }
    }
}

/// Reads `fd` to end of file into `buffer`, at most `buffer.len()` bytes and
/// until `budget` runs out; `None` on a read error or timeout.
fn read_fd_bounded(fd: i32, buffer: &mut [u8], budget: std::time::Duration) -> Option<usize> {
    let deadline = std::time::Instant::now() + budget;
    let mut filled = 0;
    while filled < buffer.len() {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        let mut poll = nix::libc::pollfd {
            fd,
            events: nix::libc::POLLIN,
            revents: 0,
        };
        let timeout_ms = i32::try_from(remaining.as_millis())
            .unwrap_or(i32::MAX)
            .max(1);
        // SAFETY: one valid pollfd on the stack, count 1.
        let ready = unsafe { nix::libc::poll(&raw mut poll, 1, timeout_ms) };
        if ready < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return None;
        }
        if ready == 0 {
            return None;
        }
        match nix::unistd::read(fd, &mut buffer[filled..]) {
            Ok(0) => break,
            Ok(read) => filled += read,
            Err(nix::errno::Errno::EINTR | nix::errno::Errno::EAGAIN) => {}
            Err(_) => return None,
        }
    }
    Some(filled)
}

/// Points file descriptor 0 at `/dev/null`, so nothing later reads the
/// launcher's pipe.
fn redirect_stdin_to_null() {
    if let Ok(null) = std::fs::File::open("/dev/null") {
        use std::os::fd::AsRawFd;
        let _ = nix::unistd::dup2(null.as_raw_fd(), 0);
    }
}

fn environment(name: &'static str) -> Result<String, WaylandAgentError> {
    std::env::var(name).map_err(|_| WaylandAgentError::Arguments(name))
}

/// Runs the headless Wayland desktop until it ends.
///
/// # Errors
///
/// [`WaylandAgentError`] when the session cannot be made ready, or its
/// outputs could not be proven released.
pub async fn run(args: &[String], session_log_id: &CorrelationId) -> Result<(), WaylandAgentError> {
    // First, before any await: the launcher's password must not wait in a
    // dumpable agent's stdin pipe while the audio and the desktop start.
    let arguments = parse_arguments(args)?;
    if let Some(seat) = arguments.lab_input_seat.as_deref() {
        super::set_lab_input_seat(seat);
        if arguments.native_seat {
            super::set_native_seat();
        }
    }
    if let Some(prefix) = arguments.compositor_prefix.clone() {
        super::set_compositor_prefix(prefix)
            .map_err(|_| WaylandAgentError::Arguments("compositor prefix already set"))?;
    }
    let keyring_password = seal_and_take_password(&arguments);
    let uid = nix::unistd::Uid::effective().as_raw();
    let username = environment("USER")?;
    let session_id = environment("XDG_SESSION_ID")?;
    let desktop = environment("XDG_SESSION_DESKTOP")?;
    if std::env::var("XDG_SESSION_TYPE").as_deref() != Ok("wayland") {
        return Err(WaylandAgentError::Arguments("XDG_SESSION_TYPE"));
    }
    if !is_private_runtime_dir(&arguments.runtime_dir, uid, &session_id) {
        return Err(WaylandAgentError::Arguments("runtime directory"));
    }
    let audio_arguments = AgentAudioArguments::from_arguments(args)
        .map_err(|_| WaylandAgentError::Arguments("local playback"))?;
    let pipewire_version = crate::media::pipewire_bridge::pipewire_version().await;
    let session_tag: String = session_log_id
        .as_str()
        .chars()
        .filter(char::is_ascii_hexdigit)
        .take(8)
        .collect();
    // Before the private graph exists: a lease on the user's own server
    // must be in place before a bridge tunnels into it.
    let mut audio = match WaylandSessionAudio::prepare(
        audio_arguments.local_playback,
        audio_arguments.pactl.clone(),
        &session_tag,
        pipewire_version,
    )
    .await
    {
        Ok(audio) => audio,
        Err(refused) => {
            emit_local_playback_refusal(refused.0);
            return Err(WaylandAgentError::LocalPlaybackRefused);
        }
    };
    let result = run_desktop(
        &arguments,
        session_log_id,
        uid,
        &username,
        &session_id,
        &desktop,
        &mut audio,
        keyring_password,
    )
    .await;
    audio.release().await;
    result
}

/// Makes the agent non-dumpable, so no other process of the user can read
/// its memory or descriptors, then takes the launcher's password off file
/// descriptor 0. Synchronous: nothing else of the agent runs in between.
fn seal_and_take_password(arguments: &AgentArguments) -> Option<Zeroizing<Vec<u8>>> {
    // The agent holds the login password until the keyring has it: never
    // let it be dumped or traced by another process of the user. Children
    // become dumpable again at their own exec.
    if let Err(error) = nix::sys::prctl::set_dumpable(false) {
        tracing::warn!(target: target::SESSION, %error, "the agent could not make itself non-dumpable");
    }
    if arguments.keyring_stdin {
        read_keyring_password()
    } else {
        None
    }
}

#[allow(clippy::too_many_arguments)] // One desktop's identity, arguments, audio and password.
async fn run_desktop(
    arguments: &AgentArguments,
    session_log_id: &CorrelationId,
    uid: u32,
    username: &str,
    session_id: &str,
    desktop: &str,
    audio: &mut WaylandSessionAudio,
    keyring_password: Option<Zeroizing<Vec<u8>>>,
) -> Result<(), WaylandAgentError> {
    // Before any stack member exists: from here on a stop (the launcher's
    // SIGTERM, its death through the parent-death signal, logind) is never
    // the default action that would kill the agent and orphan a member the
    // kernel no longer ties to it (the session bus changes SELinux domain).
    let mut stop = StopSignals::install()?;
    // One deadline for the whole start. A start cancelled here drops the
    // transaction, whose stack binding kills every member and sweeps the
    // scope synchronously.
    let deadline = tokio::time::Instant::now() + WAYLAND_START_BUDGET;
    let start = tokio::time::timeout_at(deadline, async {
        let login_environment = crate::session::login_env::import_login_environment(
            crate::session::login_env::LOGIN_SHELL_TIMEOUT,
        )
        .await
        .imported;
        let compositor =
            facts::choose_installed_compositor()
                .await
                .map_err(|error| match error {
                    CompositorChoiceError::NoneUsable => WaylandAgentError::NoCompositor,
                    CompositorChoiceError::ProbeFailed(compositor) => {
                        WaylandAgentError::CompositorProbe(compositor_binary(compositor))
                    }
                })?;
        let mode = facts::output_mode(compositor).await;
        let provider =
            MutterVirtualOutputProvider::new(compositor, mode, &arguments.runtime_dir, session_id)
                .with_audio(audio.stack().clone())
                .with_colour(arguments.colour)
                .with_cursor_mode(arguments.cursor_mode)
                .with_services(SessionServicesConfig {
                    keyring_password,
                    first_login: arguments.first_login,
                    login_environment,
                    session_id: session_id.to_owned(),
                })
                .with_x11(arguments.x11);
        if arguments.colour == CompositorColour::ForcedHdr {
            tracing::info!(
                target: target::SESSION,
                "the compositor starts with every output in an HDR (BT.2100) colour state for a Grading or HDR session"
            );
        }
        let mut transaction = OutputTransaction::acquire(
            provider,
            &arguments.outputs,
            &OutputContext::new(session_log_id.clone()),
        )
        .await
        .map_err(|error| WaylandAgentError::Outputs(transaction_error(error)))?;
        transaction
            .commit()
            .await
            .map_err(|error| WaylandAgentError::Outputs(transaction_error(error)))?;
        let committed = transaction
            .into_committed()
            .map_err(|_| WaylandAgentError::Outputs("transaction did not commit".to_owned()))?;
        let (provider, binding) = committed.into_parts();
        Ok::<_, WaylandAgentError>((compositor, provider, binding))
    });
    let (compositor, mut provider, mut binding) = tokio::select! {
        started = start => started.map_err(|_| WaylandAgentError::StartBudget(WAYLAND_START_BUDGET))??,
        () = stop.recv() => {
            tracing::info!(target: target::SESSION, "headless Wayland session stop requested while its outputs were starting");
            return Err(WaylandAgentError::StoppedWhileStarting);
        }
    };

    if let Err(refused) = audio.attach_private(&arguments.runtime_dir).await {
        emit_local_playback_refusal(refused.0);
        let _ = provider.rollback(&mut binding).await;
        return Err(WaylandAgentError::LocalPlaybackRefused);
    }

    let mut readiness = WaylandSessionReadiness::new();
    let served = async {
        readiness.record_compositor(compositor)?;
        readiness.record_outputs(binding.outputs().to_vec())?;
        serve(
            &mut binding,
            &mut readiness,
            arguments,
            deadline,
            &mut stop,
            uid,
            audio,
            Identity {
                username,
                session_id,
                desktop,
                compositor: compositor.token(),
                mode: provider.mode(),
                session_log_id,
            },
        )
        .await
    }
    .await;
    // Outputs are always released, whatever ended the session.
    let released = provider.rollback(&mut binding).await;
    served?;
    released?;
    Ok(())
}

struct Identity<'a> {
    username: &'a str,
    session_id: &'a str,
    desktop: &'a str,
    compositor: &'a str,
    mode: VirtualOutputMode,
    session_log_id: &'a CorrelationId,
}

async fn serve(
    binding: &mut MutterOutputBinding,
    readiness: &mut WaylandSessionReadiness,
    arguments: &AgentArguments,
    deadline: tokio::time::Instant,
    stop: &mut StopSignals,
    uid: u32,
    audio: &mut WaylandSessionAudio,
    identity: Identity<'_>,
) -> Result<(), WaylandAgentError> {
    let connection = binding
        .connection()
        .cloned()
        .ok_or(WaylandAgentError::Outputs(
            "no verified bus connection".to_owned(),
        ))?;
    // The X11 proof runs beside the D-Bus sessions' start and joins before
    // the ready message.
    let x11 = arguments.x11.then(|| {
        let stack = binding.stack_mut();
        let scope = stack.scope().to_path_buf();
        let compositor_pid = stack.compositor_pid();
        let displays = stack.x11_displays();
        let runtime_dir = arguments.runtime_dir.clone();
        let timeout = deadline
            .saturating_duration_since(tokio::time::Instant::now())
            .min(X11_PROOF_TIMEOUT);
        tokio::spawn(async move {
            super::xwayland::session_readiness(
                &scope,
                uid,
                compositor_pid,
                &runtime_dir,
                displays,
                timeout,
            )
            .await
        })
    });
    // A remote-monitor binding already owns the paired sessions that record
    // its outputs; startup monitors are recorded now.
    let set = match binding.take_remote() {
        Some(set) => set,
        None => {
            let connectors = binding
                .outputs()
                .iter()
                .map(|output| output.connector.clone())
                .collect::<Vec<_>>();
            let sessions = tokio::select! {
                started = tokio::time::timeout_at(
                    deadline,
                    MutterSessions::start(
                        &connection,
                        &connectors,
                        arguments.cursor_mode.screencast().mutter_value(),
                    ),
                ) => started.map_err(|_| WaylandAgentError::StartBudget(WAYLAND_START_BUDGET))??,
                () = stop.recv() => return Err(WaylandAgentError::StoppedWhileStarting),
            };
            RemoteSet::new(
                sessions,
                arguments.outputs.clone(),
                connectors,
                arguments.cursor_mode,
            )
        }
    };
    let live = LiveOutputs(Arc::new(tokio::sync::RwLock::new(set)));
    let result = async {
        let (streams, device_types, eis_offered, stylus, outputs) = {
            let set = live.0.read().await;
            for stream in set.sessions.streams() {
                readiness.record_stream(stream)?;
            }
            let input = tokio::select! {
                probed = tokio::time::timeout_at(deadline, set.sessions.probe_input()) => {
                    probed.map_err(|_| WaylandAgentError::StartBudget(WAYLAND_START_BUDGET))??
                }
                () = stop.recv() => return Err(WaylandAgentError::StoppedWhileStarting),
            };
            readiness.record_input(input)?;
            // Pen input needs an emulated stylus on the compositor's EIS
            // seat; ask rather than assume. Whether `ConnectToEIS` exists,
            // the connection and the probe are all bounded by the start
            // deadline, never fatal, and abandoned when the session is
            // stopped.
            let (eis_offered, stylus) = tokio::select! {
                found = async {
                    let eis_offered =
                        super::eis::eis_offered_within(deadline, set.sessions.eis_offered())
                            .await;
                    let stylus = super::eis::session_stylus_facts(eis_offered, deadline, || {
                        set.sessions.connect_to_eis()
                    })
                    .await;
                    (eis_offered, stylus)
                } => found,
                () = stop.recv() => return Err(WaylandAgentError::StoppedWhileStarting),
            };
            (
                set.ready_streams(),
                set.sessions.device_types(),
                eis_offered,
                stylus,
                set.ready_outputs(),
            )
        };
        debug_assert!(readiness.session_side_ready());
        let input_socket = arguments.runtime_dir.join(INPUT_SOCKET_NAME);
        // Committed text is checked against the compositor's own keymap,
        // read as a Wayland client for as long as the session runs.
        let session_text = Arc::new(super::text::SessionText::new(Some(Box::new(
            super::text::GsettingsInputSources,
        ))));
        super::wl_keymap::spawn(
            arguments.runtime_dir.join(WAYLAND_DISPLAY_NAME),
            Arc::clone(&session_text),
        );
        let input = Arc::new(
            InputService::bind(&input_socket, uid)
                .map_err(WaylandAgentError::Input)?
                .with_text(Arc::clone(&session_text))
                // Mutter may have restored Num Lock on as its seat started.
                .with_locks(super::LockState::at_session_start(
                    super::input::read_remembered_num_lock(),
                )),
        );
        // Characters the layout lacks need the compositor's `ei_text`
        // (libei 1.6); whether it offers one is read from its EIS seat.
        let eis_text = (eis_offered == Some(true)).then(|| {
            session_text.set_utf8(super::text::Utf8Route::NotOffered, None);
            tokio::spawn(follow_eis_text(live.clone(), Arc::clone(&session_text)))
        });
        let facts = Arc::new(SessionFacts::new(InputSessionFacts {
            generation: INITIAL_OUTPUT_GENERATION,
            streams: streams.clone(),
            device_types,
            remote_desktop_started: true,
            backend: InputBackendKind::RemoteDesktopDbus,
            eis_offered,
            stylus: Some(stylus.clone()),
            connection: 0,
            text: None,
        }));
        tracing::info!(
            target: target::INPUT,
            device_types,
            eis_offered = ?eis_offered,
            stylus = ?stylus.binding,
            stylus_unavailable = ?stylus.unavailable,
            ei_stylus = ?stylus.ei_stylus_version,
            backend = "remote_desktop_dbus",
            "session input served through Mutter RemoteDesktop; pen only through an EIS stylus"
        );
        let (placement, placement_desired) =
            tokio::sync::watch::channel(Some(live.0.read().await.placements()));
        let placement = Arc::new(placement);
        let snapshot = Arc::new(OutputSnapshot::new(
            INITIAL_OUTPUT_GENERATION,
            &*live.0.read().await,
        ));
        let output_service = if identity.mode.reconfigurable() {
            let output_socket = arguments.runtime_dir.join(OUTPUT_SOCKET_NAME);
            Some(
                OutputService::bind(
                    &output_socket,
                    OutputServiceParts {
                        uid,
                        connection: connection.clone(),
                        runtime_dir: arguments.runtime_dir.clone(),
                        mode: identity.mode,
                        live: live.clone(),
                        facts: Arc::clone(&facts),
                        input: Arc::clone(&input),
                        placement: Arc::clone(&placement),
                        session_log_id: identity.session_log_id.clone(),
                        snapshot: Arc::clone(&snapshot),
                        colour: arguments.colour,
                        cursor_mode: arguments.cursor_mode,
                    },
                )
                .map_err(WaylandAgentError::Input)?,
            )
        } else {
            None
        };
        let x11 = match x11 {
            Some(task) => Some(task.await.unwrap_or_else(|error| {
                super::ReadyX11::failed(format!("the X11 proof did not finish: {error}"), None)
            })),
            None => None,
        };
        if let Some(x11) = &x11 {
            if x11.ready {
                tracing::info!(
                    target: target::SESSION,
                    display = x11.display().as_deref(),
                    setup_display = x11.setup_display().as_deref(),
                    glamor = x11.glamor.token(),
                    "X11 applications served: the compositor's Xwayland accepted a client with the session cookie on a managed screen"
                );
            } else {
                tracing::warn!(
                    target: target::SESSION,
                    display = x11.display().as_deref(),
                    failure = x11.failure.as_deref(),
                    "X11 applications are not served in this session; Wayland applications are unaffected"
                );
            }
        }
        let clipboard =
            prove_clipboard(&connection, &arguments.runtime_dir, uid, identity.username).await;
        let compositor_gpu = binding.stack_mut().compositor_gpu();
        tracing::info!(
            target: target::SESSION,
            render_node = ?compositor_gpu.render_node,
            pci = ?compositor_gpu.pci,
            vendor = ?compositor_gpu.vendor.map(|vendor| format!("{vendor:#06x}")),
            "compositor GPU from its open render node"
        );
        let ready = AgentReady {
            pid: std::process::id(),
            uid,
            username: identity.username.to_owned(),
            display: String::new(),
            session_id: identity.session_id.to_owned(),
            session_type: "wayland".to_owned(),
            desktop: identity.desktop.to_owned(),
            wayland: Some(WaylandAgentReady {
                compositor: identity.compositor.to_owned(),
                runtime_dir: arguments.runtime_dir.clone(),
                wayland_display: WAYLAND_DISPLAY_NAME.to_owned(),
                outputs,
                streams: streams.clone(),
                input_device_types: device_types,
                input_socket,
                generation: INITIAL_OUTPUT_GENERATION,
                compositor_render_node: compositor_gpu.render_node,
                compositor_gpu_pci: compositor_gpu.pci,
                compositor_gpu_vendor: compositor_gpu.vendor,
                output_mode: identity.mode,
                output_socket: output_service
                    .as_ref()
                    .map(|_| arguments.runtime_dir.join(OUTPUT_SOCKET_NAME)),
                cursor_mode: arguments.cursor_mode,
                clipboard: clipboard.as_ref().map(|_| super::WaylandClipboardReady {
                    socket: arguments.runtime_dir.join(CLIPBOARD_SOCKET_NAME),
                    interface: CLIPBOARD_INTERFACE.to_owned(),
                    dedicated_session: true,
                }),
                x11,
            }),
            audio: Some(audio.ready()),
        };
        emit_ready(&ready)?;
        tracing::info!(
            target: target::SESSION,
            compositor = identity.compositor,
            output_mode = identity.mode.token(),
            pending = ?readiness.gaps().iter().map(|gap| gap.token()).collect::<Vec<_>>(),
            "headless Wayland session side ready: outputs verified, streams announced, input target accepted; the first frame is proven by the capture consumer"
        );

        let stack = binding.stack_mut();
        stack.desktop_ready();
        let mut logout = stack.logout_receiver();
        let outputs_served = async {
            match &output_service {
                Some(service) => {
                    tokio::join!(
                        service.serve(),
                        super::remote::keep_placement(connection.clone(), placement_desired)
                    );
                }
                None => std::future::pending::<()>().await,
            }
        };
        let clipboard_served = async {
            match &clipboard {
                Some((service, compositor)) => service.serve(compositor).await,
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            () = input.serve(&live, &facts) => {}
            () = clipboard_served => {}
            () = outputs_served => {}
            member = stack.wait_for_member_exit() => {
                tracing::warn!(target: target::SESSION, member, "headless stack member exited; ending the session");
            }
            () = recover_closed_streams(StreamRecovery {
                live: &live,
                connection: &connection,
                runtime_dir: &arguments.runtime_dir,
                mode: identity.mode,
                facts: &facts,
                input: &input,
                placement: &placement,
                snapshot: &snapshot,
            }) => {
                tracing::warn!(target: target::SESSION, "Mutter closed the RemoteDesktop/ScreenCast session and it could not be replaced; ending the session");
            }
            () = logout_confirmed(logout.as_mut()) => {
                tracing::info!(target: target::SESSION, reason = "user_logout", "the user logged out inside the session; ending the desktop");
            }
            () = stop.recv() => {
                tracing::info!(target: target::SESSION, "headless Wayland session stop requested");
            }
            () = audio.watch() => {}
        }
        input.release_held(&live).await;
        if let Some(service) = &output_service {
            service.stop_wide().await;
        }
        if let Some(eis_text) = eis_text {
            eis_text.abort();
        }
        Ok(())
    }
    .await;
    live.0.write().await.sessions.stop().await;
    drop(live);
    drop(connection);
    result
}

/// Resolves once a logout was confirmed; never without a session manager.
async fn logout_confirmed(logout: Option<&mut LogoutReceiver>) {
    match logout {
        Some(logout) => logout.confirmed().await,
        None => std::future::pending().await,
    }
}

/// How long the clipboard proof may take: a handful of bus calls.
const CLIPBOARD_PROOF_BUDGET: std::time::Duration = std::time::Duration::from_secs(10);

/// Creates and proves the clipboard's own `RemoteDesktop` session and binds
/// its socket. `None` (logged once) when the compositor offers no usable
/// clipboard: the session goes on, and the Pier advertises the clipboard
/// disabled for it.
async fn prove_clipboard(
    connection: &zbus::Connection,
    runtime_dir: &std::path::Path,
    uid: u32,
    username: &str,
) -> Option<(ClipboardService, MutterClipboard)> {
    let proven =
        tokio::time::timeout(CLIPBOARD_PROOF_BUDGET, MutterClipboard::create(connection)).await;
    let compositor = match proven {
        Ok(Ok(compositor)) => compositor,
        Ok(Err(error)) => {
            tracing::info!(target: target::SESSION, event = "clipboard_bridge_unavailable", %error, "compositor clipboard not proven; clipboard unavailable for this desktop");
            return None;
        }
        Err(_) => {
            tracing::info!(target: target::SESSION, event = "clipboard_bridge_unavailable", "compositor clipboard proof timed out; clipboard unavailable for this desktop");
            return None;
        }
    };
    match ClipboardService::bind(&runtime_dir.join(CLIPBOARD_SOCKET_NAME), uid, username) {
        Ok(service) => {
            tracing::info!(
                target: target::SESSION,
                event = "clipboard_bridge_proven",
                interface = CLIPBOARD_INTERFACE,
                dedicated_session = true,
                "compositor clipboard proven on its own RemoteDesktop session"
            );
            Some((service, compositor))
        }
        Err(error) => {
            tracing::warn!(target: target::SESSION, event = "clipboard_bridge_unavailable", %error, "clipboard socket could not be bound");
            None
        }
    }
}

/// Waits before each `ConnectToEIS` attempt for committed text; a seat
/// without `ei_text` ends the attempts.
const EIS_TEXT_BACKOFF: [std::time::Duration; 3] = [
    std::time::Duration::ZERO,
    std::time::Duration::from_secs(2),
    std::time::Duration::from_secs(10),
];

/// Keeps the session's EIS text device for as long as the compositor
/// offers one, reconnecting a bounded number of times when the connection
/// ends (the `RemoteDesktop` session was replaced, the compositor
/// restarted its EIS server).
async fn follow_eis_text(live: LiveOutputs, text: Arc<super::text::SessionText>) {
    for delay in EIS_TEXT_BACKOFF {
        tokio::time::sleep(delay).await;
        text.set_eis_error(Some("connecting".to_owned()));
        let socket = tokio::time::timeout(EIS_TEXT_CONNECT_TIMEOUT, async {
            let set = live.0.read().await;
            set.sessions.connect_to_eis().await
        })
        .await;
        let socket = match socket {
            Ok(Ok(socket)) => socket,
            Ok(Err(error)) => {
                tracing::warn!(target: target::INPUT, %error, "ConnectToEIS for committed text failed");
                text.set_eis_error(Some(format!("ConnectToEIS: {error}")));
                continue;
            }
            Err(_) => {
                text.set_eis_error(Some(format!(
                    "ConnectToEIS did not answer within {EIS_TEXT_CONNECT_TIMEOUT:?}"
                )));
                continue;
            }
        };
        text.set_eis_error(Some("handshaking".to_owned()));
        match super::eis_text::run(
            std::os::unix::net::UnixStream::from(socket),
            Arc::clone(&text),
        )
        .await
        {
            // The seat was read; its verdict (a text device, or why there
            // is none) is already recorded.
            Ok(()) => return,
            Err(error) => {
                tracing::info!(
                    target: target::INPUT,
                    %error,
                    "the EIS connection for committed text ended"
                );
                text.set_eis_error(Some(error.to_string()));
            }
        }
    }
    text.set_utf8(super::text::Utf8Route::NotOffered, None);
}

/// How long `ConnectToEIS` (with the session lock) may take.
const EIS_TEXT_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// What [`recover_closed_streams`] works on.
struct StreamRecovery<'a> {
    live: &'a LiveOutputs,
    connection: &'a zbus::Connection,
    runtime_dir: &'a std::path::Path,
    mode: VirtualOutputMode,
    facts: &'a SessionFacts,
    input: &'a InputService,
    placement: &'a tokio::sync::watch::Sender<super::remote::DesiredPlacement>,
    snapshot: &'a OutputSnapshot,
}

/// Replaces the paired sessions when Mutter closes them while the compositor
/// runs (a remote-monitor session only): the same outputs are recorded
/// again by a new session, with new `PipeWire` nodes, as the next output
/// generation, so the Pier's next capture epoch finds them through the
/// output socket. Returns when they are closed and cannot be replaced.
async fn recover_closed_streams(recovery: StreamRecovery<'_>) {
    let StreamRecovery {
        live,
        connection,
        runtime_dir,
        mode,
        facts,
        input,
        placement,
        snapshot,
    } = recovery;
    loop {
        live_closed(live).await;
        if !mode.reconfigurable() {
            return;
        }
        tracing::warn!(target: target::SESSION, "Mutter closed the RemoteDesktop/ScreenCast session; recording the outputs again");
        input.release_held(live).await;
        placement.send_replace(None);
        let mut set = live.0.write().await;
        let placed = set.placed.clone();
        let cursor_mode = set.cursor_mode;
        match super::remote::establish(connection, runtime_dir, &placed, cursor_mode).await {
            Ok(replacement) => {
                let mut previous = std::mem::replace(&mut *set, replacement);
                previous.sessions.stop().await;
                let generation = facts.snapshot().generation.saturating_add(1);
                facts.advance(generation, set.ready_streams());
                snapshot.update(generation, &set);
                placement.send_replace(Some(set.placements()));
                tracing::info!(
                    target: target::SESSION,
                    generation,
                    nodes = ?set.sessions.node_ids(),
                    "outputs recorded again by a new ScreenCast session"
                );
            }
            Err(error) => {
                tracing::warn!(target: target::SESSION, %error, "outputs could not be recorded again");
                return;
            }
        }
    }
}

/// Resolves when Mutter closes the live paired sessions. A set replaced by
/// an output reconfiguration closes too; that is not the end of the
/// session, so the wait moves on to its successor.
async fn live_closed(live: &LiveOutputs) {
    loop {
        let (id, signals) = {
            let set = live.0.read().await;
            (set.id, set.sessions.closed_signals().await)
        };
        let Some(mut signals) = signals else {
            return std::future::pending().await;
        };
        let _ = signals.next().await;
        if live.0.read().await.id == id {
            return;
        }
    }
}

fn emit_ready(ready: &AgentReady) -> Result<(), WaylandAgentError> {
    use std::io::Write as _;
    let line = serde_json::to_string(ready)
        .map_err(|error| WaylandAgentError::Ready(std::io::Error::other(error)))?;
    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "{line}").map_err(WaylandAgentError::Ready)?;
    stdout.flush().map_err(WaylandAgentError::Ready)
}

fn transaction_error(error: OutputTransactionError<MutterOutputError>) -> String {
    tracing::warn!(target: target::SESSION, %error, "headless Mutter output transaction failed closed");
    error.to_string()
}

/// Stop requests: the launcher's SIGTERM (or the parent-death SIGTERM when
/// the launcher dies), or logind ending the session (`loginctl
/// terminate-session`, logout), which sends SIGTERM and SIGHUP to every
/// process in the scope. Unhandled, either would kill the agent before it
/// could release its outputs and stack. Installed once, so a signal that
/// arrives while the session is still starting is kept for the next wait.
struct StopSignals {
    term: tokio::signal::unix::Signal,
    interrupt: tokio::signal::unix::Signal,
    hangup: tokio::signal::unix::Signal,
}

impl StopSignals {
    fn install() -> Result<Self, WaylandAgentError> {
        use tokio::signal::unix::{signal, SignalKind};

        Ok(Self {
            term: signal(SignalKind::terminate()).map_err(WaylandAgentError::Signals)?,
            interrupt: signal(SignalKind::interrupt()).map_err(WaylandAgentError::Signals)?,
            hangup: signal(SignalKind::hangup()).map_err(WaylandAgentError::Signals)?,
        })
    }

    async fn recv(&mut self) {
        tokio::select! {
            _ = self.term.recv() => {}
            _ = self.interrupt.recv() => {}
            _ = self.hangup.recv() => {}
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_password_is_sealed_before_the_agent_awaits_anything() {
        let source = include_str!("agent.rs");
        let start = source
            .find("pub async fn run(args: &[String]")
            .expect("run is in this file");
        let body = &source[start..];
        let body =
            &body[body.find('{').expect("body of run")..body.find("\n}\n").expect("end of run")];
        let sealed = body
            .find("seal_and_take_password(&arguments)")
            .expect("run seals the agent");
        let first_await = body.find(".await").expect("run awaits");
        assert!(sealed < first_await, "an await precedes the password read");
        assert!(
            !body[..sealed].contains("async"),
            "nothing asynchronous starts before the password read"
        );
    }

    use super::*;

    #[test]
    fn the_password_pipe_is_read_straight_from_its_descriptor_and_bounded() {
        use std::os::fd::AsRawFd;
        let (reader, writer) = nix::unistd::pipe().expect("pipe");
        nix::unistd::write(&writer, b"s3cret").expect("write");
        drop(writer);
        let mut buffer = vec![0_u8; 16];
        assert_eq!(
            read_fd_bounded(
                reader.as_raw_fd(),
                &mut buffer,
                std::time::Duration::from_secs(1)
            ),
            Some(6)
        );
        assert_eq!(&buffer[..6], b"s3cret");

        // More than the buffer holds: the read stops at its size, which the
        // caller refuses as too long.
        let (reader, writer) = nix::unistd::pipe().expect("pipe");
        nix::unistd::write(&writer, &[b'x'; 32]).expect("write");
        drop(writer);
        let mut buffer = vec![0_u8; 9];
        assert_eq!(
            read_fd_bounded(
                reader.as_raw_fd(),
                &mut buffer,
                std::time::Duration::from_secs(1)
            ),
            Some(9)
        );

        // A writer that never writes nor closes: bounded by the budget.
        let (reader, _writer) = nix::unistd::pipe().expect("pipe");
        let started = std::time::Instant::now();
        assert_eq!(
            read_fd_bounded(
                reader.as_raw_fd(),
                &mut buffer,
                std::time::Duration::from_millis(200)
            ),
            None
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
    }

    fn args(extra: &[&str]) -> Vec<String> {
        [
            AGENT_RUNTIME_DIR_FLAG,
            "/run/user/1001/arcen-wayland-c9",
            AGENT_OUTPUT_FLAG,
            "1920x1080@60",
        ]
        .iter()
        .chain(extra)
        .map(|argument| (*argument).to_owned())
        .collect()
    }

    #[test]
    fn the_cursor_mode_flag_is_optional_and_single() {
        let parsed = parse_arguments(&args(&[])).unwrap();
        assert_eq!(parsed.cursor_mode, StreamCursorMode::Embedded);
        for mode in [StreamCursorMode::Metadata, StreamCursorMode::Hidden] {
            assert_eq!(
                parse_arguments(&args(&[AGENT_CURSOR_MODE_FLAG, mode.token()]))
                    .unwrap()
                    .cursor_mode,
                mode
            );
        }
        for refused in [
            &[AGENT_CURSOR_MODE_FLAG][..],
            &[AGENT_CURSOR_MODE_FLAG, "none"],
            &[
                AGENT_CURSOR_MODE_FLAG,
                "metadata",
                AGENT_CURSOR_MODE_FLAG,
                "embedded",
            ],
        ] {
            assert!(parse_arguments(&args(refused)).is_err(), "{refused:?}");
        }
    }

    #[test]
    fn the_compositor_prefix_flag_is_optional_single_and_validated() {
        assert_eq!(parse_arguments(&args(&[])).unwrap().compositor_prefix, None);
        assert_eq!(
            parse_arguments(&args(&[
                AGENT_COMPOSITOR_PREFIX_FLAG,
                "/opt/arcen-gnome/51"
            ]))
            .unwrap()
            .compositor_prefix
            .map(|prefix| prefix.to_string()),
            Some("/opt/arcen-gnome/51".to_owned())
        );
        for refused in [
            &[AGENT_COMPOSITOR_PREFIX_FLAG][..],
            &[AGENT_COMPOSITOR_PREFIX_FLAG, "relative/prefix"],
            &[AGENT_COMPOSITOR_PREFIX_FLAG, "/opt/../tmp"],
            &[
                AGENT_COMPOSITOR_PREFIX_FLAG,
                "/opt/a",
                AGENT_COMPOSITOR_PREFIX_FLAG,
                "/opt/b",
            ],
        ] {
            assert!(parse_arguments(&args(refused)).is_err(), "{refused:?}");
        }
    }

    #[test]
    fn a_native_seat_needs_its_desktop_seat() {
        assert!(
            !parse_arguments(&args(&[AGENT_NATIVE_SEAT_FLAG]))
                .unwrap()
                .native_seat
        );
        assert!(
            parse_arguments(&args(&[
                AGENT_LAB_INPUT_SEAT_FLAG,
                "seat-arcen-pen-0123456789abcdef",
                AGENT_NATIVE_SEAT_FLAG
            ]))
            .unwrap()
            .native_seat
        );
    }

    #[test]
    fn the_lab_input_seat_is_a_plain_seat_name_and_never_seat0() {
        assert_eq!(parse_arguments(&args(&[])).unwrap().lab_input_seat, None);
        assert_eq!(
            parse_arguments(&args(&[
                AGENT_LAB_INPUT_SEAT_FLAG,
                "seat-arcen-pen-0123456789abcdef"
            ]))
            .unwrap()
            .lab_input_seat
            .as_deref(),
            Some("seat-arcen-pen-0123456789abcdef")
        );
        for refused in [
            &[AGENT_LAB_INPUT_SEAT_FLAG][..],
            &[AGENT_LAB_INPUT_SEAT_FLAG, "seat0"],
            &[AGENT_LAB_INPUT_SEAT_FLAG, ""],
            &[AGENT_LAB_INPUT_SEAT_FLAG, "seat;rm"],
            &[AGENT_LAB_INPUT_SEAT_FLAG, "seat-arcen-pen"],
        ] {
            assert!(parse_arguments(&args(refused)).is_err(), "{refused:?}");
        }
    }

    #[test]
    fn the_fidelity_route_flag_selects_the_compositors_colour() {
        assert_eq!(
            parse_arguments(&args(&[])).unwrap().colour,
            CompositorColour::Default
        );
        assert_eq!(
            parse_arguments(&args(&[AGENT_FIDELITY_ROUTE_FLAG, "force-hdr"]))
                .unwrap()
                .colour,
            CompositorColour::ForcedHdr
        );
        for refused in [
            &[AGENT_FIDELITY_ROUTE_FLAG][..],
            &[AGENT_FIDELITY_ROUTE_FLAG, "colour-mode"],
            &[
                AGENT_FIDELITY_ROUTE_FLAG,
                "force-hdr",
                AGENT_FIDELITY_ROUTE_FLAG,
                "force-hdr",
            ],
        ] {
            assert!(parse_arguments(&args(refused)).is_err(), "{refused:?}");
        }
    }
}
