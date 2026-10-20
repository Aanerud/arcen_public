//! The local socket between the Pier's network service and its desktop agents.
//!
//! The service runs as `_arcen` from boot, owns UDP 18444 and the TLS key, and
//! has no desktop. Each graphical session runs an agent, as that session's
//! user, which has a desktop and nothing else: no key, no listener. When a Deck
//! is admitted the service hands its session to the agent of the session on
//! the console and relays it, one QUIC lane per role, until either side
//! leaves ([`relay_lanes`]).
//!
//! The exchange itself is [`arcen_session::agent_relay`]. What lives here is
//! the macOS half: Unix sockets, the kernel's peer credentials, and which
//! executable the peer actually is.

#![allow(unsafe_code)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use arcen_session::agent_relay::{
    AgentMessage, AgentRegistration, ConsoleHolder, DesktopSessionKind, MAX_RELAY_LINE_BYTES,
    ParkedAgent, RouteRefusal, ServiceMessage, select_agent, validate_registration,
};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{UnixListener, UnixStream};

/// Directory holding the service's socket. Created by the installer, owned by
/// the service account, so nobody else can put a socket there.
///
/// Persistent rather than under `/var/run`, which is emptied at every boot:
/// the service runs unprivileged and could not recreate a directory there,
/// and a helper daemon that exists only to `mkdir` is a second thing to fail.
pub const SOCKET_DIRECTORY: &str = "/Library/Application Support/Arcen/run";
/// The socket agents connect to.
pub const AGENT_SOCKET: &str = "/Library/Application Support/Arcen/run/agent.sock";

/// How long a connecting agent has to say what it is.
const REGISTRATION_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a parked agent has to accept a Deck it was offered.
const ATTACH_TIMEOUT: Duration = Duration::from_secs(5);
/// How often the service looks again for an agent that is not there yet.
const AGENT_POLL: Duration = Duration::from_millis(250);
/// Copy buffer for the relay, in each direction.
///
/// Small on purpose. Every byte buffered between the agent and QUIC is a byte
/// the agent believes was sent: at 256 KiB this relay alone could hold half a
/// second of a 4 Mbit/s stream, invisible to the one process that can tell
/// audio from a stale picture.
const RELAY_BUFFER: usize = 16 * 1024;

/// Why the relay could not be used.
#[derive(Debug)]
pub enum RelayError {
    /// The socket failed.
    Io(std::io::Error),
    /// The peer sent something that is not this exchange.
    Protocol(String),
    /// The peer is not who it must be.
    Untrusted(String),
    /// The service refused this agent.
    Refused(String),
    /// The service went away.
    ServiceGone,
}

impl std::fmt::Display for RelayError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "agent socket: {error}"),
            Self::Protocol(detail) => write!(formatter, "agent relay protocol: {detail}"),
            Self::Untrusted(detail) => write!(formatter, "untrusted peer: {detail}"),
            Self::Refused(reason) => {
                write!(formatter, "the Pier service refused this agent: {reason}")
            }
            Self::ServiceGone => formatter.write_str("the Pier service closed the agent socket"),
        }
    }
}

impl std::error::Error for RelayError {}

impl From<std::io::Error> for RelayError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

/// Reads one control line, byte by byte.
///
/// Unbuffered on purpose: the byte after the newline belongs to the Deck's
/// stream, and a buffered reader would swallow it. The lines are short and
/// sent once per session, so the cost is nothing.
///
/// Returns `None` when the peer closed before sending anything.
async fn read_line(stream: &mut UnixStream) -> Result<Option<String>, RelayError> {
    let mut line = Vec::with_capacity(128);
    loop {
        let mut byte = [0_u8; 1];
        if stream.read(&mut byte).await? == 0 {
            return if line.is_empty() {
                Ok(None)
            } else {
                Err(RelayError::Protocol(
                    "line ended without a newline".to_owned(),
                ))
            };
        }
        if byte[0] == b'\n' {
            return String::from_utf8(line)
                .map(Some)
                .map_err(|_| RelayError::Protocol("line is not UTF-8".to_owned()));
        }
        line.push(byte[0]);
        if line.len() >= MAX_RELAY_LINE_BYTES {
            return Err(RelayError::Protocol("line is too long".to_owned()));
        }
    }
}

async fn write_line<T: serde::Serialize>(
    stream: &mut UnixStream,
    value: &T,
) -> Result<(), RelayError> {
    let mut line = serde_json::to_vec(value)
        .map_err(|error| RelayError::Protocol(format!("encode: {error}")))?;
    line.push(b'\n');
    stream.write_all(&line).await?;
    stream.flush().await?;
    Ok(())
}

/// A parked agent and its socket.
struct Parked {
    agent: ParkedAgent,
    stream: UnixStream,
    pid: Option<i32>,
}

/// The agents the service can hand a Deck to.
/// An attached session waiting for, or holding, its agent's audio channel.
struct AudioChannelSlot {
    uid: u32,
    deliver: tokio::sync::oneshot::Sender<UnixStream>,
}

pub struct AgentRegistry {
    parked: Mutex<Vec<Parked>>,
    sequence: AtomicU64,
    /// Attached sessions whose agent may still open an audio side channel.
    audio_channels: Mutex<std::collections::HashMap<u64, AudioChannelSlot>>,
    active_sessions: Arc<Mutex<std::collections::HashMap<u64, u32>>>,
    /// The service's own account, which is never handed a Deck.
    service_uid: Option<u32>,
    /// The only executable allowed to register, when known.
    expected_program: Option<PathBuf>,
}

impl std::fmt::Debug for AgentRegistry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AgentRegistry")
            .field("parked", &self.parked_count())
            .field("expected_program", &self.expected_program)
            .finish_non_exhaustive()
    }
}

impl AgentRegistry {
    /// Creates an empty registry.
    ///
    /// `expected_program` pins the executable an agent must be. `None` accepts
    /// any program running as a permitted account, which is only for a service
    /// run by hand out of a build tree.
    #[must_use]
    pub fn new(service_uid: Option<u32>, expected_program: Option<PathBuf>) -> Arc<Self> {
        std::sync::Arc::new(Self {
            parked: Mutex::new(Vec::new()),
            sequence: AtomicU64::new(1),
            audio_channels: Mutex::new(std::collections::HashMap::new()),
            active_sessions: Arc::new(Mutex::new(std::collections::HashMap::new())),
            service_uid,
            expected_program,
        })
    }

    /// How many agents are parked.
    #[must_use]
    pub fn parked_count(&self) -> usize {
        self.parked.lock().map_or(0, |parked| parked.len())
    }

    /// Binds the agent socket, replacing a stale one left by a crash.
    ///
    /// # Errors
    ///
    /// Returns the filesystem or socket error.
    pub fn bind(path: &Path) -> Result<UnixListener, RelayError> {
        use std::os::unix::fs::{FileTypeExt as _, PermissionsExt as _};

        if let Some(parent) = path.parent() {
            if !parent.exists() {
                std::fs::create_dir_all(parent)?;
                std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o755))?;
            }
        }
        // Only a socket is removed. Anything else at this path is not ours to
        // delete, and refusing to start says so.
        match std::fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_socket() => std::fs::remove_file(path)?,
            Ok(_) => {
                return Err(RelayError::Protocol(format!(
                    "{} exists and is not a socket",
                    path.display()
                )));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let listener = UnixListener::bind(path)?;
        // Every session's agent must be able to connect. Who it is gets
        // decided from the kernel's credentials, not from who could open it.
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666))?;
        Ok(listener)
    }

    /// Accepts agents until the listener fails.
    pub async fn run(self: std::sync::Arc<Self>, listener: UnixListener) {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let registry = std::sync::Arc::clone(&self);
                    tokio::spawn(async move {
                        if let Err(error) = registry.register(stream).await {
                            tracing::warn!(target: "arcen::relay", %error, "refused an agent");
                        }
                    });
                }
                Err(error) => {
                    tracing::error!(target: "arcen::relay", %error, "agent socket accept failed");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    }

    async fn register(&self, mut stream: UnixStream) -> Result<(), RelayError> {
        let credentials = stream.peer_cred()?;
        let uid = credentials.uid();
        let pid = credentials.pid();

        if let Some(expected) = self.expected_program.as_deref() {
            let actual = pid.and_then(process_path);
            if actual.as_deref() != Some(expected) {
                let detail = format!(
                    "pid {pid:?} is {}, not {}",
                    actual
                        .as_deref()
                        .map_or_else(|| "unknown".to_owned(), |p| p.display().to_string()),
                    expected.display()
                );
                let _ = write_line(
                    &mut stream,
                    &ServiceMessage::Refused {
                        reason: "only the installed Arcen Agent Helper may serve a desktop"
                            .to_owned(),
                    },
                )
                .await;
                return Err(RelayError::Untrusted(detail));
            }
        }

        let line = tokio::time::timeout(REGISTRATION_TIMEOUT, read_line(&mut stream))
            .await
            .map_err(|_| RelayError::Protocol("no registration in time".to_owned()))??
            .ok_or(RelayError::ServiceGone)?;
        let registration: AgentRegistration = serde_json::from_str(&line)
            .map_err(|error| RelayError::Protocol(format!("registration: {error}")))?;
        if let Some(session) = registration.audio_for_session {
            return self.accept_audio_channel(stream, uid, session).await;
        }
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed);
        let agent = match validate_registration(&registration, uid, self.service_uid, sequence) {
            Ok(agent) => agent,
            Err(error) => {
                let reason = error.to_string();
                let _ = write_line(
                    &mut stream,
                    &ServiceMessage::Refused {
                        reason: reason.clone(),
                    },
                )
                .await;
                return Err(RelayError::Refused(reason));
            }
        };
        write_line(
            &mut stream,
            &ServiceMessage::Registered {
                version: arcen_session::agent_relay::AGENT_RELAY_VERSION,
            },
        )
        .await?;
        tracing::info!(
            target: "arcen::relay",
            uid,
            pid = ?pid,
            kind = ?agent.kind,
            agent_version = %registration.agent_version,
            "desktop agent parked"
        );
        if let Ok(mut parked) = self.parked.lock() {
            parked.push(Parked { agent, stream, pid });
        }
        Ok(())
    }

    /// Hands an audio side channel to the session it names.
    ///
    /// Only the account the session's agent runs as may open it, so a local
    /// process cannot inject sound into somebody else's session.
    async fn accept_audio_channel(
        &self,
        mut stream: UnixStream,
        uid: u32,
        session: u64,
    ) -> Result<(), RelayError> {
        let slot = self.audio_channels.lock().ok().and_then(|mut slots| {
            let belongs = slots.get(&session).is_some_and(|slot| slot.uid == uid);
            if belongs {
                slots.remove(&session)
            } else {
                None
            }
        });
        let Some(slot) = slot else {
            let _ = write_line(
                &mut stream,
                &ServiceMessage::Refused {
                    reason: "no such session for this account".to_owned(),
                },
            )
            .await;
            return Err(RelayError::Untrusted(format!(
                "uid {uid} asked for the audio channel of session {session}"
            )));
        };
        write_line(
            &mut stream,
            &ServiceMessage::Registered {
                version: arcen_session::agent_relay::AGENT_RELAY_VERSION,
            },
        )
        .await?;
        let _ = slot.deliver.send(stream);
        Ok(())
    }

    /// Forgets a session's side channels once its relay ends.
    pub fn end_session(&self, session: u64) {
        if let Ok(mut slots) = self.audio_channels.lock() {
            slots.remove(&session);
        }
        if let Ok(mut sessions) = self.active_sessions.lock() {
            sessions.remove(&session);
        }
    }

    /// Takes the agent for the session on the console, discarding any that
    /// have gone away while parked.
    fn take(
        &self,
        console: ConsoleHolder,
    ) -> Result<(UnixStream, ParkedAgent, Option<i32>), RouteRefusal> {
        let Ok(mut parked) = self.parked.lock() else {
            return Err(match console {
                ConsoleHolder::User(uid) => RouteRefusal::UserAgentMissing { uid },
                ConsoleHolder::LoginWindow => RouteRefusal::LoginWindowAgentMissing,
            });
        };
        parked.retain(|entry| still_connected(&entry.stream));
        let agents: Vec<ParkedAgent> = parked.iter().map(|entry| entry.agent).collect();
        let index = select_agent(&agents, console)?;
        let entry = parked.swap_remove(index);
        Ok((entry.stream, entry.agent, entry.pid))
    }

    /// Hands a Deck to the agent of the session on the console.
    ///
    /// Waits up to `wait` for one to appear, because a Deck arriving seconds
    /// after a login, or after the service restarted, would otherwise be
    /// refused for a gap that closes by itself.
    ///
    /// # Errors
    ///
    /// Returns the reason no agent took the Deck, in words for the Deck.
    pub async fn attach(
        &self,
        console: impl Fn() -> ConsoleHolder,
        peer: &str,
        wait: Duration,
    ) -> Result<AttachedAgent, String> {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            match self.take(console()) {
                Ok((mut stream, agent, pid)) => {
                    let session = self.sequence.fetch_add(1, Ordering::Relaxed);
                    let (deliver, audio_channel) = tokio::sync::oneshot::channel();
                    if let Ok(mut slots) = self.audio_channels.lock() {
                        slots.insert(
                            session,
                            AudioChannelSlot {
                                uid: agent.uid,
                                deliver,
                            },
                        );
                    }
                    match offer(&mut stream, peer, session).await {
                        Ok(()) => {
                            if let Ok(mut sessions) = self.active_sessions.lock() {
                                sessions.insert(session, agent.uid);
                            }
                            return Ok(AttachedAgent {
                                stream,
                                agent,
                                session,
                                audio_channel,
                            });
                        }
                        Err(error) => {
                            tracing::warn!(
                                target: "arcen::relay",
                                uid = agent.uid,
                                pid = ?pid,
                                %error,
                                "a parked agent did not take the Deck; trying another"
                            );
                            self.end_session(session);
                        }
                    }
                }
                Err(refusal) => {
                    if tokio::time::Instant::now() >= deadline {
                        return Err(refusal.to_string());
                    }
                    tokio::time::sleep(AGENT_POLL).await;
                }
            }
        }
    }
}

/// An agent that took a Deck.
pub struct AttachedAgent {
    /// The session socket.
    pub stream: UnixStream,
    /// Who it is.
    pub agent: ParkedAgent,
    /// The session's id, which its audio side channel names.
    pub session: u64,
    /// Resolves when the agent opens that side channel.
    pub audio_channel: tokio::sync::oneshot::Receiver<UnixStream>,
}

impl std::fmt::Debug for AttachedAgent {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AttachedAgent")
            .field("agent", &self.agent)
            .field("session", &self.session)
            .finish_non_exhaustive()
    }
}

async fn offer(stream: &mut UnixStream, peer: &str, session: u64) -> Result<(), RelayError> {
    write_line(
        stream,
        &ServiceMessage::Attach {
            peer: peer.to_owned(),
            session,
        },
    )
    .await?;
    let line = tokio::time::timeout(ATTACH_TIMEOUT, read_line(stream))
        .await
        .map_err(|_| RelayError::Protocol("no answer to the offer".to_owned()))??
        .ok_or(RelayError::ServiceGone)?;
    match serde_json::from_str::<AgentMessage>(&line) {
        Ok(AgentMessage::Attached) => Ok(()),
        Err(error) => Err(RelayError::Protocol(format!("attach answer: {error}"))),
    }
}

/// Whether a parked socket is still open.
///
/// A parked agent sends nothing, so anything readable is either its close or
/// a protocol violation, and either way it is no longer a usable agent.
fn still_connected(stream: &UnixStream) -> bool {
    let mut probe = [0_u8; 1];
    match stream.try_read(&mut probe) {
        Err(error) => error.kind() == std::io::ErrorKind::WouldBlock,
        Ok(_) => false,
    }
}

/// Relays a Deck's stream to the agent that took it, until either leaves.
///
/// # Errors
///
/// Returns the error that ended the relay, when it was not a clean close.
pub async fn relay<D>(deck: &mut D, agent: &mut UnixStream) -> std::io::Result<(u64, u64)>
where
    D: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + ?Sized,
{
    tokio::io::copy_bidirectional_with_sizes(deck, agent, RELAY_BUFFER, RELAY_BUFFER).await
}

/// What a lane relay carried.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MediaRelayStats {
    /// Messages forwarded from the Deck to the agent.
    pub to_agent: u64,
    /// Messages the agent sent toward the Deck (every lane).
    pub to_deck: u64,
    /// Audio frames that arrived on the agent's audio side channel.
    pub side_channel_audio: u64,
    /// Per-lane relay counters, once the attachment was granted.
    pub lanes: arcen_transport::quic::LaneRelayStats,
}

/// Relays a Deck's session to its agent, one lane per role (ADR 0014).
///
/// The agent speaks the session over one local WebSocket (it is the server,
/// so this side reads it as a client) and, optionally, sends audio on its
/// own side channel. This relay is the network half: control messages stay
/// on the Deck's control lane, and once the agent has authenticated the Deck
/// (`auth_result` with `success`) it grants the attachment and hands every
/// other message to [`arcen_transport::quic::PierLaneRelay`], which puts
/// video, audio, cursor and bulk traffic on their own lanes and never lets
/// one wait for another. Deck input, microphone and bulk lanes come back to
/// the agent in order.
///
/// `path_signal` sends the connection's path truth to the agent once a
/// session has started; an in-process session samples the connection itself.
///
/// # Errors
///
/// Returns the first transport error when it was not a clean close.
pub async fn relay_lanes<A>(
    mut deck: arcen_transport::quic::SessionLaneSocket,
    agent: A,
    session: u64,
    audio_channel: Option<tokio::sync::oneshot::Receiver<UnixStream>>,
    path_signal: bool,
) -> Result<MediaRelayStats, String>
where
    A: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use arcen_protocol::lanes::SessionRecord;
    use arcen_transport::quic::{LaneRelayEvent, PierLaneRelay, RelayRoute};
    use futures_util::{SinkExt as _, StreamExt as _};
    use tokio_tungstenite::WebSocketStream;
    use tokio_tungstenite::tungstenite::protocol::Role;

    let lanes = deck.lanes().clone();
    let connection = lanes.connection().clone();
    // The agent is local and trusted to send large frames: a keyframe is
    // megabytes. Tungstenite's defaults already bound it.
    let agent_ws = WebSocketStream::from_raw_socket(agent, Role::Client, None).await;
    let (mut agent_tx, mut agent_rx) = agent_ws.split();
    let mut stats = MediaRelayStats::default();
    let mut relay: Option<PierLaneRelay> = None;

    // The agent's own audio channel, when it opens one: audio that never
    // shares a socket with video on the way here.
    let (side_tx, mut side_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
    let side_channel = audio_channel.map(|channel| {
        tokio::spawn(async move {
            if let Ok(channel) = channel.await {
                read_audio_channel(channel, side_tx).await;
            }
        })
    });

    let mut path_state = arcen_telemetry::PathSignalState::default();
    let path_started = tokio::time::Instant::now();
    let mut path_tick = tokio::time::interval(std::time::Duration::from_secs(1));
    path_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The agent authenticates the Deck over the control lane and takes
    // nothing but the Deck's own `auth_response` until that is done, so path
    // truth waits for the Deck's `client_hello`, which only follows a
    // successful authentication.
    let mut session_started = false;

    let result = loop {
        tokio::select! {
            biased;
            event = async {
                match relay.as_mut() {
                    Some(relay) => relay.next_event().await,
                    None => std::future::pending().await,
                }
            } => match event {
                LaneRelayEvent::ToSession(record) => {
                    if let Err(error) = agent_tx.send(record_message(record)).await {
                        break Err(format!("to agent: {error}"));
                    }
                    stats.to_agent += 1;
                }
                LaneRelayEvent::ToDeckControl(record) => {
                    if let Err(error) = deck.send(record).await {
                        break Err(format!("to Deck: {error}"));
                    }
                }
                LaneRelayEvent::Ended(terminal) => break Err(format!("lanes ended: {terminal}")),
            },
            message = deck.next() => {
                let record = match message {
                    None => break Ok(()),
                    Some(Ok(record)) => record,
                    Some(Err(exit)) => break Err(lane_exit_detail(&exit)),
                };
                if let SessionRecord::Text(text) = &record {
                    if serde_json::from_str::<serde_json::Value>(text)
                        .ok()
                        .and_then(|value| value.get("type")?.as_str().map(str::to_owned))
                        .is_some_and(|kind| ServiceMessage::is_service_type(&kind))
                    {
                        break Err("Deck sent a service-only message".to_string());
                    }
                    session_started |= is_client_hello(text);
                }
                let closing = matches!(record, SessionRecord::Close(_));
                if let Err(error) = agent_tx.send(record_message(record)).await {
                    break Err(format!("to agent: {error}"));
                }
                stats.to_agent += 1;
                if closing {
                    break Ok(());
                }
            }
            message = agent_rx.next() => {
                let message = match message {
                    None => break Ok(()),
                    Some(Ok(message)) => message,
                    Some(Err(error)) => break Err(format!("from agent: {error}")),
                };
                let Some(record) = message_record(message) else {
                    continue;
                };
                stats.to_deck += 1;
                let control = match relay.as_mut() {
                    Some(relay) => match relay.route_to_deck(record) {
                        RelayRoute::Control(record) => record,
                        RelayRoute::Queued(_) | RelayRoute::Dropped(_) => continue,
                    },
                    None if is_pre_attachment_control(&record) => record,
                    None => {
                        tracing::debug!(
                            target: "arcen::relay",
                            "agent sent lane traffic before the attachment; dropped"
                        );
                        continue;
                    }
                };
                // The grant follows a successful `auth_result` and precedes
                // `server_hello`, whichever the agent sends first.
                let grant_before = relay.is_none() && is_text_type(&control, SERVER_HELLO);
                let grant_after = relay.is_none() && is_successful_auth_result(&control);
                if grant_before {
                    match grant_lanes(&mut deck).await {
                        Ok(()) => relay = Some(PierLaneRelay::start(&lanes)),
                        Err(error) => break Err(error),
                    }
                }
                let closing = matches!(control, SessionRecord::Close(_));
                if let Err(error) = deck.send(control).await {
                    break Err(format!("to Deck: {error}"));
                }
                if closing {
                    break Ok(());
                }
                if grant_after {
                    match grant_lanes(&mut deck).await {
                        Ok(()) => relay = Some(PierLaneRelay::start(&lanes)),
                        Err(error) => break Err(error),
                    }
                }
            }
            frame = side_rx.recv() => {
                if let Some(frame) = frame {
                    stats.side_channel_audio += 1;
                    if let Some(relay) = relay.as_mut() {
                        let _ = relay.route_to_deck(SessionRecord::Binary(frame));
                    }
                }
            }
            _ = path_tick.tick(), if path_signal => {
                if connection.close_reason().is_some() {
                    break Ok(());
                }
                if !session_started {
                    continue;
                }
                let signal = arcen_transport::observe_quinn_path_signal(
                    &mut path_state,
                    path_started.elapsed(),
                    &connection,
                );
                let message = ServiceMessage::PathSignal { session, signal };
                let text = match serde_json::to_string(&message) {
                    Ok(text) => text,
                    Err(error) => break Err(format!("path signal encode: {error}")),
                };
                if let Err(error) = agent_tx
                    .send(tokio_tungstenite::tungstenite::Message::Text(text))
                    .await
                {
                    break Err(format!("to agent path signal: {error}"));
                }
            }
        }
    };
    if let Some(side_channel) = side_channel {
        side_channel.abort();
    }
    if let Some(relay) = relay.as_ref() {
        stats.lanes = relay.stats();
    }
    let _ = agent_tx.close().await;
    result.map(|()| stats)
}

/// Authorizes the attachment's lanes and tells the Deck, on the control
/// lane, before anything the agent sends after authentication.
async fn grant_lanes(deck: &mut arcen_transport::quic::SessionLaneSocket) -> Result<(), String> {
    use futures_util::SinkExt as _;

    let grant = arcen_transport::quic::grant_attachment(
        deck.lanes(),
        arcen_transport::quic::next_attachment_generation(),
        arcen_protocol::lanes::LaneRoleSet::all(),
    )
    .map_err(|error| format!("lane attachment: {error}"))?;
    let record = arcen_protocol::lanes::session_text_record(&grant)
        .ok_or_else(|| "lane attachment encode".to_owned())?;
    deck.send(record)
        .await
        .map_err(|error| format!("to Deck: {error}"))?;
    tracing::info!(target: "arcen::relay", "session runs on isolated lanes");
    Ok(())
}

/// Before the grant only the control lane exists: authentication, its
/// result, refusals and liveness.
fn is_pre_attachment_control(record: &arcen_protocol::lanes::SessionRecord) -> bool {
    use arcen_protocol::lanes::{LaneRole, SessionRecord, lane_for_session_text};
    match record {
        SessionRecord::Text(text) => lane_for_session_text(text) == LaneRole::Control,
        SessionRecord::Binary(_) => false,
        SessionRecord::Close(_) | SessionRecord::Ping(_) | SessionRecord::Pong(_) => true,
    }
}

const SERVER_HELLO: &str = arcen_protocol::messages::SERVER_HELLO;

fn text_json(
    record: &arcen_protocol::lanes::SessionRecord,
    msg_type: &str,
) -> Option<serde_json::Value> {
    let arcen_protocol::lanes::SessionRecord::Text(text) = record else {
        return None;
    };
    if !text.contains(msg_type) {
        return None;
    }
    serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .filter(|value| value.get("type").and_then(serde_json::Value::as_str) == Some(msg_type))
}

fn is_text_type(record: &arcen_protocol::lanes::SessionRecord, msg_type: &str) -> bool {
    text_json(record, msg_type).is_some()
}

/// Whether the agent accepted the Deck: the point at which the attachment's
/// lanes are granted.
fn is_successful_auth_result(record: &arcen_protocol::lanes::SessionRecord) -> bool {
    text_json(record, arcen_protocol::messages::AUTH_RESULT).is_some_and(|value| {
        value.get("success").and_then(serde_json::Value::as_bool) == Some(true)
    })
}

fn message_record(
    message: tokio_tungstenite::tungstenite::Message,
) -> Option<arcen_protocol::lanes::SessionRecord> {
    use arcen_protocol::lanes::{SessionClose, SessionRecord};
    use tokio_tungstenite::tungstenite::Message;
    Some(match message {
        Message::Text(text) => SessionRecord::Text(text),
        Message::Binary(bytes) => SessionRecord::Binary(bytes),
        Message::Ping(bytes) => SessionRecord::Ping(bytes),
        Message::Pong(bytes) => SessionRecord::Pong(bytes),
        Message::Close(frame) => SessionRecord::Close(frame.map(|frame| SessionClose {
            code: u16::from(frame.code),
            reason: frame.reason.into_owned(),
        })),
        Message::Frame(_) => return None,
    })
}

fn record_message(
    record: arcen_protocol::lanes::SessionRecord,
) -> tokio_tungstenite::tungstenite::Message {
    use arcen_protocol::lanes::SessionRecord;
    use tokio_tungstenite::tungstenite::Message;
    use tokio_tungstenite::tungstenite::protocol::CloseFrame;
    use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
    match record {
        SessionRecord::Text(text) => Message::Text(text),
        SessionRecord::Binary(bytes) => Message::Binary(bytes),
        SessionRecord::Ping(bytes) => Message::Ping(bytes),
        SessionRecord::Pong(bytes) => Message::Pong(bytes),
        SessionRecord::Close(close) => Message::Close(close.map(|close| CloseFrame {
            code: CloseCode::from(close.code),
            reason: close.reason.into(),
        })),
    }
}

/// Names a lane exit, including a Deck that speaks another transport layout.
fn lane_exit_detail(exit: &arcen_transport::quic::LaneExit) -> String {
    match exit {
        arcen_transport::quic::LaneExit::ConnectionLost {
            mismatch: Some(mismatch),
        } => mismatch.message(),
        other => format!("control lane ended: {other:?}"),
    }
}

/// Whether a Deck text message is a `client_hello`.
fn is_client_hello(text: &str) -> bool {
    text.contains(arcen_protocol::messages::CLIENT_HELLO)
        && serde_json::from_str::<serde_json::Value>(text).is_ok_and(|value| {
            value.get("type").and_then(serde_json::Value::as_str)
                == Some(arcen_protocol::messages::CLIENT_HELLO)
        })
}

/// Which agent executable the service accepts.
///
/// The installed service pairs with the installed helper under
/// `/Library/PrivilegedHelperTools`. A bundle anywhere else (a build output
/// such as `dist/macos/`) pairs with the helper beside it. Returns `None` when
/// the service is not running from a bundle, which is a build tree.
#[must_use]
pub fn expected_agent_program(service_executable: &Path) -> Option<PathBuf> {
    if service_executable == Path::new(crate::service::PIER_PROGRAM) {
        return Some(PathBuf::from(crate::service::AGENT_PROGRAM));
    }
    let macos = service_executable.parent()?;
    let contents = macos.parent()?;
    let bundle = contents.parent()?;
    if bundle.extension()? != "app" || contents.file_name()? != "Contents" {
        return None;
    }
    Some(
        bundle
            .parent()?
            .join("Arcen Agent Helper.app/Contents/MacOS/arcen-agent-helper"),
    )
}

/// The executable a process is running.
#[cfg(target_os = "macos")]
fn process_path(pid: i32) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt as _;

    // `PROC_PIDPATHINFO_MAXSIZE` in `<libproc.h>`.
    const MAX_PATH: usize = 4 * 1024;
    #[link(name = "proc")]
    unsafe extern "C" {
        fn proc_pidpath(pid: i32, buffer: *mut std::ffi::c_void, size: u32) -> i32;
    }
    let mut buffer = vec![0_u8; MAX_PATH];
    // SAFETY: `buffer` is `MAX_PATH` bytes and the kernel writes at most that.
    let written = unsafe {
        proc_pidpath(
            pid,
            buffer.as_mut_ptr().cast(),
            u32::try_from(buffer.len()).unwrap_or(u32::MAX),
        )
    };
    let written = usize::try_from(written).ok().filter(|&length| length > 0)?;
    buffer.truncate(written);
    Some(PathBuf::from(std::ffi::OsStr::from_bytes(&buffer)))
}

#[cfg(not(target_os = "macos"))]
fn process_path(_pid: i32) -> Option<PathBuf> {
    None
}

/// Parks this agent with the service and waits for a Deck.
///
/// Refuses a service that is not the Pier's own account: the socket path is in
/// a directory only that account can write, and checking the peer as well is
/// what stops a process that raced the service to the path from collecting
/// somebody's password.
///
/// Returns the Deck's stream and its network address.
///
/// # Errors
///
/// Returns [`RelayError`] when the service is absent, untrusted, refuses this
/// agent, or goes away while it waits.
pub async fn park(
    path: &Path,
    kind: DesktopSessionKind,
    service_uid: Option<u32>,
) -> Result<AttachedDeck, RelayError> {
    let mut stream = UnixStream::connect(path).await?;
    let credentials = stream.peer_cred()?;
    let trusted = credentials.uid() == 0 || Some(credentials.uid()) == service_uid;
    if !trusted {
        return Err(RelayError::Untrusted(format!(
            "the agent socket is served by uid {}, not the Pier service",
            credentials.uid()
        )));
    }
    write_line(&mut stream, &AgentRegistration::new(kind, crate::VERSION)).await?;
    match read_service_message(&mut stream).await? {
        ServiceMessage::Registered { .. } => {}
        ServiceMessage::PathSignal { .. } => {}
        ServiceMessage::Refused { reason } => return Err(RelayError::Refused(reason)),
        ServiceMessage::Attach { .. } => {
            return Err(RelayError::Protocol(
                "offered a Deck before registering".to_owned(),
            ));
        }
    }
    match read_service_message(&mut stream).await? {
        ServiceMessage::Attach { peer, session } => {
            write_line(&mut stream, &AgentMessage::Attached).await?;
            Ok(AttachedDeck {
                stream,
                peer,
                session,
            })
        }
        ServiceMessage::Refused { reason } => Err(RelayError::Refused(reason)),
        ServiceMessage::PathSignal { .. } => Err(RelayError::Protocol(
            "received path signal before Deck attachment".to_owned(),
        )),
        ServiceMessage::Registered { .. } => {
            Err(RelayError::Protocol("registered twice".to_owned()))
        }
    }
}

/// A Deck this agent was handed.
#[derive(Debug)]
pub struct AttachedDeck {
    /// The Deck's session stream, relayed by the service.
    pub stream: UnixStream,
    /// The Deck's network address.
    pub peer: String,
    /// The session id the service assigned, which the audio side channel names.
    pub session: u64,
}

/// Opens the audio side channel for an attached session.
///
/// Audio sent here reaches the Deck's audio lane without waiting behind
/// video on the session socket. Frames are u32-big-endian length prefixed.
///
/// # Errors
///
/// Returns [`RelayError`] when the service is absent, untrusted, or refuses.
pub async fn open_audio_channel(
    path: &Path,
    kind: DesktopSessionKind,
    service_uid: Option<u32>,
    session: u64,
) -> Result<UnixStream, RelayError> {
    let mut stream = UnixStream::connect(path).await?;
    let credentials = stream.peer_cred()?;
    if !(credentials.uid() == 0 || Some(credentials.uid()) == service_uid) {
        return Err(RelayError::Untrusted(format!(
            "the agent socket is served by uid {}, not the Pier service",
            credentials.uid()
        )));
    }
    write_line(
        &mut stream,
        &AgentRegistration::audio_channel(kind, crate::VERSION, session),
    )
    .await?;
    match read_service_message(&mut stream).await? {
        ServiceMessage::Registered { .. } => Ok(stream),
        ServiceMessage::PathSignal { .. } => Ok(stream),
        ServiceMessage::Refused { reason } => Err(RelayError::Refused(reason)),
        ServiceMessage::Attach { .. } => Err(RelayError::Protocol(
            "offered a Deck on an audio channel".to_owned(),
        )),
    }
}

/// Reads an agent's audio side channel (u32 big-endian length-prefixed
/// frames) into the relay. A full queue drops the frame: newer audio is
/// worth more than older audio.
async fn read_audio_channel(mut channel: UnixStream, frames: tokio::sync::mpsc::Sender<Vec<u8>>) {
    loop {
        let mut length = [0_u8; 4];
        if channel.read_exact(&mut length).await.is_err() {
            return;
        }
        let length = usize::try_from(u32::from_be_bytes(length)).unwrap_or(usize::MAX);
        if length > arcen_protocol::AUDIO_PRIORITY_MAX_FRAME_BYTES {
            tracing::warn!(target: "arcen::relay", length, "oversized audio frame; channel closed");
            return;
        }
        let mut frame = vec![0_u8; length];
        if channel.read_exact(&mut frame).await.is_err() {
            return;
        }
        if !arcen_protocol::wire::is_audio_frame(&frame) {
            tracing::warn!(target: "arcen::relay", "non-audio frame on the audio channel; closed");
            return;
        }
        match frames.try_send(frame) {
            Ok(()) | Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {}
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => return,
        }
    }
}

async fn read_service_message(stream: &mut UnixStream) -> Result<ServiceMessage, RelayError> {
    let line = read_line(stream).await?.ok_or(RelayError::ServiceGone)?;
    serde_json::from_str(&line)
        .map_err(|error| RelayError::Protocol(format!("service line: {error}")))
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    fn scratch_socket(name: &str) -> PathBuf {
        // Short: a Unix socket path is limited to about a hundred bytes.
        PathBuf::from(format!(
            "/tmp/arcen-relay-{}-{name}.sock",
            std::process::id()
        ))
    }

    /// A Deck, the service's listener and a fake agent behind the relay.
    struct LaneRig {
        deck: arcen_transport::quic::LaneConnection,
        deck_control: arcen_transport::quic::SessionLane,
        agent: tokio_tungstenite::WebSocketStream<tokio::io::DuplexStream>,
        _endpoints: (crate::net::Listener, quinn::Endpoint),
        dir: PathBuf,
    }

    async fn lane_rig(name: &str) -> LaneRig {
        use arcen_protocol::lanes::LaneEndpoint;
        let _ = rustls::crypto::ring::default_provider().install_default();
        let dir = std::env::current_dir()
            .expect("current dir")
            .join("target")
            .join(format!("arcen-relay-lanes-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("test dir");
        let key_pair = rcgen::KeyPair::generate().expect("key");
        let certificate = rcgen::CertificateParams::new(vec!["localhost".to_owned()])
            .expect("params")
            .self_signed(&key_pair)
            .expect("cert");
        std::fs::write(dir.join("host.crt"), certificate.pem()).expect("write cert");
        std::fs::write(dir.join("host.key"), key_pair.serialize_pem()).expect("write key");
        let listener = crate::net::Listener::bind(
            "127.0.0.1:0".parse().expect("addr"),
            crate::net::server_config(&dir.join("host.crt"), &dir.join("host.key"))
                .expect("config"),
        )
        .expect("listener");
        let address = listener.local_addr().expect("address");
        let mut roots = rustls::RootCertStore::empty();
        roots.add(certificate.der().clone()).expect("root");
        let mut crypto = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .expect("versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
        crypto.alpn_protocols = vec![arcen_transport::quic::DIRECT_QUIC_ALPN_PROTOCOL.to_vec()];
        let mut client_config = quinn::ClientConfig::new(Arc::new(
            quinn::crypto::rustls::QuicClientConfig::try_from(crypto).expect("crypto"),
        ));
        client_config.transport_config(arcen_transport::quic::lane_transport_config_arc(
            &arcen_transport::BoundedTransportPolicy::default(),
            LaneEndpoint::Deck,
        ));
        let client = quinn::Endpoint::client("127.0.0.1:0".parse().expect("addr")).expect("client");
        let connect = arcen_transport::quic::connect_session_lanes(
            arcen_transport::quic::DirectQuicDialParams {
                endpoint: client.clone(),
                client_config,
                remote_addr: address,
                server_name: "localhost",
            },
            arcen_transport::quic::LaneRuntimeConfig::new(LaneEndpoint::Deck),
        );
        let (accepted, connected) = tokio::join!(listener.accept_raw(), connect);
        let (pier, _peer) = accepted.expect("accept");
        let (deck, deck_control) = connected.expect("connect");
        let (agent_end, relay_end) = tokio::io::duplex(64 * 1024);
        tokio::spawn(relay_lanes(pier, relay_end, 1, None, false));
        let agent = tokio_tungstenite::WebSocketStream::from_raw_socket(
            agent_end,
            tokio_tungstenite::tungstenite::protocol::Role::Server,
            None,
        )
        .await;
        LaneRig {
            deck,
            deck_control,
            agent,
            _endpoints: (listener, client),
            dir,
        }
    }

    async fn deck_text(rig: &mut LaneRig) -> String {
        match tokio::time::timeout(
            Duration::from_secs(5),
            rig.deck_control.reader.next_record(),
        )
        .await
        .expect("in time")
        .expect("record")
        {
            arcen_protocol::lanes::SessionRecord::Text(text) => text,
            other => panic!("expected text, got {other:?}"),
        }
    }

    fn video(keyframe: bool) -> Vec<u8> {
        use arcen_protocol::wire::{
            ChromaSubsampling, FrameType, VIDEO_KEYFRAME_FLAG, VideoCodec, VideoHeader,
            encode_video_header,
        };
        let mut frame = encode_video_header(VideoHeader {
            frame_type: FrameType::VideoH265,
            codec: VideoCodec::H265,
            chroma: ChromaSubsampling::Yuv420,
            flags: if keyframe { VIDEO_KEYFRAME_FLAG } else { 0 },
            timestamp_ms: 0,
            monitor_id: 0,
            topology_generation: 0,
            stream_epoch: 0,
        });
        frame.resize(frame.len() + 256 * 1024, 0x42);
        frame
    }

    #[tokio::test]
    async fn the_relay_grants_lanes_after_authentication_and_audio_outruns_a_stalled_video_lane() {
        use arcen_protocol::lanes::{LaneRole, SessionRecord};
        use arcen_transport::quic::{PeerLaneEvent, PeerLaneReader, SessionLane};
        use futures_util::{SinkExt as _, StreamExt as _};
        use tokio_tungstenite::tungstenite::Message;

        let mut rig = lane_rig("grant").await;
        rig.agent
            .send(Message::Text(
                r#"{"type":"auth_request","auth_methods":["pam"]}"#.into(),
            ))
            .await
            .expect("auth_request");
        assert!(deck_text(&mut rig).await.contains("auth_request"));
        // No lane exists before authentication: the attachment is not granted.
        assert!(rig.deck.open_lane(LaneRole::InputCursor).await.is_err());

        rig.agent
            .send(Message::Text(
                r#"{"type":"auth_result","success":true}"#.into(),
            ))
            .await
            .expect("auth_result");
        rig.agent
            .send(Message::Text(r#"{"type":"server_hello"}"#.into()))
            .await
            .expect("server_hello");
        assert!(deck_text(&mut rig).await.contains("auth_result"));
        let grant = deck_text(&mut rig).await;
        assert!(grant.contains("lane_attachment"), "{grant}");
        arcen_transport::quic::accept_attachment(
            &rig.deck,
            &serde_json::from_str(&grant).expect("grant"),
        )
        .expect("accepted");
        assert!(deck_text(&mut rig).await.contains("server_hello"));

        // Deck input reaches the agent on its own lane.
        let input = SessionLane::split(
            rig.deck
                .open_lane(LaneRole::InputCursor)
                .await
                .expect("input"),
        )
        .expect("session lane");
        input
            .sender
            .send(arcen_transport::quic::session_record_bytes(
                &SessionRecord::Text(r#"{"type":"key_event","key":4,"down":true}"#.to_owned()),
            ))
            .await
            .expect("key");
        let delivered = tokio::time::timeout(Duration::from_secs(5), rig.agent.next())
            .await
            .expect("in time")
            .expect("message")
            .expect("valid");
        assert!(matches!(&delivered, Message::Text(text) if text.contains("key_event")));

        // The Deck never reads video; audio still arrives, frame after frame.
        let mut audio = PeerLaneReader::new(&rig.deck, LaneRole::AudioOutput);
        rig.agent
            .send(Message::Binary(video(true)))
            .await
            .expect("video");
        for index in 0..120_u32 {
            rig.agent
                .send(Message::Binary(video(false)))
                .await
                .expect("video");
            if index % 6 == 0 {
                let mut frame =
                    arcen_protocol::wire::encode_audio_header(arcen_protocol::wire::AudioHeader {
                        codec: arcen_protocol::wire::AudioCodec::Opus,
                        timestamp_ms: index,
                    })
                    .to_vec();
                frame.extend_from_slice(&[7; 60]);
                rig.agent.send(Message::Binary(frame)).await.expect("audio");
                let event = loop {
                    match tokio::time::timeout(Duration::from_secs(2), audio.next())
                        .await
                        .expect("audio is not held behind video")
                        .expect("audio lane")
                    {
                        PeerLaneEvent::Frame(frame) => break frame,
                        PeerLaneEvent::Opened { .. } | PeerLaneEvent::Lost { .. } => {}
                    }
                };
                assert!(arcen_protocol::wire::is_audio_frame(&event));
            }
        }
        // The relay asked the agent for a keyframe after dropping video.
        let mut asked = false;
        while let Ok(Some(Ok(message))) =
            tokio::time::timeout(Duration::from_millis(500), rig.agent.next()).await
        {
            if matches!(&message, Message::Text(text) if text.contains("request_full_frame")) {
                asked = true;
                break;
            }
        }
        assert!(asked, "a dropped video frame must ask for a keyframe");
        std::fs::remove_dir_all(&rig.dir).ok();
    }

    #[test]
    fn path_truth_waits_for_an_authenticated_session() {
        let hello = serde_json::to_string(&arcen_protocol::messages::ClientHelloMsg::default())
            .expect("hello");
        assert!(is_client_hello(&hello));
        // The Deck's credentials must reach the agent before anything else.
        assert!(!is_client_hello(
            r#"{"type":"auth_response","username":"client_hello"}"#
        ));
        assert!(!is_client_hello("not json client_hello"));
    }

    #[test]
    fn the_agent_is_the_installed_helper_or_the_sibling_bundle() {
        assert_eq!(
            expected_agent_program(Path::new(
                "/Applications/Arcen Pier.app/Contents/MacOS/arcen-pier-macos"
            )),
            Some(PathBuf::from(
                "/Library/PrivilegedHelperTools/Arcen Agent Helper.app/Contents/MacOS/arcen-agent-helper"
            ))
        );
        assert_eq!(
            expected_agent_program(Path::new(
                "/Users/dev/arcen/dist/macos/Arcen Pier.app/Contents/MacOS/arcen-pier-macos"
            )),
            Some(PathBuf::from(
                "/Users/dev/arcen/dist/macos/Arcen Agent Helper.app/Contents/MacOS/arcen-agent-helper"
            ))
        );
        assert_eq!(
            expected_agent_program(Path::new(
                "/Users/dev/arcen/target/release/arcen-pier-macos"
            )),
            None,
            "a build tree has no bundle to pin"
        );
    }

    #[test]
    fn this_process_path_is_readable() {
        let path = process_path(i32::try_from(std::process::id()).unwrap()).expect("own path");
        assert_eq!(
            path,
            std::env::current_exe().unwrap().canonicalize().unwrap()
        );
    }

    #[tokio::test]
    async fn a_parked_agent_receives_the_deck_stream_verbatim() {
        let path = scratch_socket("verbatim");
        let listener = AgentRegistry::bind(&path).expect("bind");
        let own = crate::auth::resolve_account(
            &std::env::var("USER").unwrap_or_else(|_| "root".to_owned()),
        )
        .map(|account| account.uid);
        let registry = AgentRegistry::new(None, None);
        tokio::spawn(std::sync::Arc::clone(&registry).run(listener));

        let agent_path = path.clone();
        let agent = tokio::spawn(async move {
            let AttachedDeck {
                mut stream, peer, ..
            } = park(&agent_path, DesktopSessionKind::User, own)
                .await
                .expect("park");
            let mut greeting = [0_u8; 5];
            stream.read_exact(&mut greeting).await.expect("read");
            stream.write_all(b"world").await.expect("write");
            (peer, greeting)
        });

        let uid = own.expect("own uid");
        let AttachedAgent {
            stream: mut offered,
            agent: parked,
            ..
        } = registry
            .attach(
                || ConsoleHolder::User(uid),
                "203.0.113.7:50000",
                Duration::from_secs(5),
            )
            .await
            .expect("attach");
        assert_eq!(parked.uid, uid);
        offered.write_all(b"hello").await.expect("send");
        let mut reply = [0_u8; 5];
        offered.read_exact(&mut reply).await.expect("reply");
        assert_eq!(&reply, b"world");
        let (peer, greeting) = agent.await.expect("agent");
        assert_eq!(peer, "203.0.113.7:50000");
        assert_eq!(&greeting, b"hello");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn an_audio_channel_reaches_only_the_session_it_names() {
        let path = scratch_socket("audio");
        let listener = AgentRegistry::bind(&path).expect("bind");
        let own = crate::auth::resolve_account(
            &std::env::var("USER").unwrap_or_else(|_| "root".to_owned()),
        )
        .map(|account| account.uid);
        let uid = own.expect("own uid");
        let registry = AgentRegistry::new(None, None);
        tokio::spawn(std::sync::Arc::clone(&registry).run(listener));

        let agent_path = path.clone();
        let agent = tokio::spawn(async move {
            park(&agent_path, DesktopSessionKind::User, own)
                .await
                .expect("park")
        });
        let attached = registry
            .attach(
                || ConsoleHolder::User(uid),
                "203.0.113.7:1",
                Duration::from_secs(5),
            )
            .await
            .expect("attach");
        let deck = agent.await.expect("agent");
        assert_eq!(deck.session, attached.session);

        let wrong = open_audio_channel(
            &path,
            DesktopSessionKind::User,
            own,
            attached.session + 1000,
        )
        .await;
        assert!(matches!(wrong, Err(RelayError::Refused(_))), "{wrong:?}");

        let mut channel = open_audio_channel(&path, DesktopSessionKind::User, own, deck.session)
            .await
            .expect("own session");
        let mut delivered = tokio::time::timeout(Duration::from_secs(2), attached.audio_channel)
            .await
            .expect("in time")
            .expect("delivered");
        channel.write_all(b"ping").await.expect("write");
        let mut read = [0_u8; 4];
        delivered.read_exact(&mut read).await.expect("read");
        assert_eq!(&read, b"ping");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn a_deck_for_another_session_is_refused_by_name() {
        let path = scratch_socket("other");
        let listener = AgentRegistry::bind(&path).expect("bind");
        let registry = AgentRegistry::new(None, None);
        tokio::spawn(std::sync::Arc::clone(&registry).run(listener));
        let refused = registry
            .attach(
                || ConsoleHolder::User(u32::MAX - 1),
                "203.0.113.7:1",
                Duration::from_millis(300),
            )
            .await
            .expect_err("nobody parked for that account");
        assert!(refused.contains("no Arcen agent"), "{refused}");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn an_agent_that_left_is_not_offered_a_deck() {
        let path = scratch_socket("left");
        let listener = AgentRegistry::bind(&path).expect("bind");
        let own = crate::auth::resolve_account(
            &std::env::var("USER").unwrap_or_else(|_| "root".to_owned()),
        )
        .map(|account| account.uid)
        .expect("own uid");
        let registry = AgentRegistry::new(None, None);
        tokio::spawn(std::sync::Arc::clone(&registry).run(listener));

        // Register by hand and hang up while parked.
        let mut stream = UnixStream::connect(&path).await.expect("connect");
        write_line(
            &mut stream,
            &AgentRegistration::new(DesktopSessionKind::User, "test"),
        )
        .await
        .expect("register");
        let _ = read_line(&mut stream).await.expect("registered");
        drop(stream);
        tokio::time::sleep(Duration::from_millis(100)).await;

        assert!(
            registry
                .attach(
                    || ConsoleHolder::User(own),
                    "203.0.113.7:1",
                    Duration::from_millis(300)
                )
                .await
                .is_err(),
            "a closed agent must be discarded, not offered"
        );
        assert_eq!(registry.parked_count(), 0);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn an_agent_refuses_a_socket_served_by_somebody_else() {
        // Served by this test's own account, which is neither root nor the
        // service account the agent was told to expect.
        let path = scratch_socket("impostor");
        let listener = UnixListener::bind(&path).expect("bind");
        tokio::spawn(async move {
            let _held = listener.accept().await;
            tokio::time::sleep(Duration::from_secs(2)).await;
        });
        if nix_is_root() {
            let _ = std::fs::remove_file(&path);
            return;
        }
        let refused = park(&path, DesktopSessionKind::User, Some(u32::MAX - 2))
            .await
            .expect_err("impostor");
        assert!(matches!(refused, RelayError::Untrusted(_)), "{refused}");
        let _ = std::fs::remove_file(&path);
    }

    fn nix_is_root() -> bool {
        crate::service::is_root()
    }

    #[test]
    fn a_stale_non_socket_is_left_alone() {
        let path = scratch_socket("file");
        std::fs::write(&path, b"not a socket").expect("file");
        assert!(AgentRegistry::bind(&path).is_err());
        assert_eq!(std::fs::read(&path).expect("kept"), b"not a socket");
        let _ = std::fs::remove_file(&path);
    }
}
