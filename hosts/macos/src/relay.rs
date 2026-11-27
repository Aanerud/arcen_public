// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
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
    /// The service asked this parked agent to sign its session out, and the
    /// request was made (`status`); the agent parks again.
    SignOutRequested(i32),
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
            Self::SignOutRequested(status) => {
                write!(
                    formatter,
                    "asked for this session's logout (status {status})"
                )
            }
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
    /// This desktop's change-of-account ledger and attachment epoch.
    handover: Arc<crate::handover::HandoverDesk>,
    /// Where `SESSION_HANDOVER` records go, once the service has a sink.
    telemetry: std::sync::OnceLock<crate::observability::HostTelemetry>,
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
            handover: Arc::new(crate::handover::HandoverDesk::new()),
            telemetry: std::sync::OnceLock::new(),
        })
    }

    /// This desktop's change-of-account ledger.
    #[must_use]
    pub fn handover(&self) -> &Arc<crate::handover::HandoverDesk> {
        &self.handover
    }

    /// Sends `SESSION_HANDOVER` records through `telemetry` from now on.
    pub fn set_telemetry(&self, telemetry: crate::observability::HostTelemetry) {
        let _ = self.telemetry.set(telemetry);
    }

    /// Records a change-of-account step for `peer`.
    fn handover_event(
        &self,
        reason: arcen_session::session_handover::HandoverReason,
        kind: Option<arcen_session::session_handover::displays::AttachmentKind>,
        client: Option<arcen_session::session_handover::RemoteClient>,
        peer: &str,
    ) {
        tracing::info!(
            target: arcen_telemetry::names::target::SESSION,
            "session handover: {} ({peer})",
            reason.token()
        );
        let (Some(telemetry), Some(fields)) = (self.telemetry.get(), reason.fields(kind, client))
        else {
            return;
        };
        telemetry.emit(
            arcen_telemetry::LifecycleEventKind::SessionHandover,
            &crate::observability::SessionScope {
                sid: arcen_telemetry::CorrelationId::from_uuid_v4_bytes(
                    crate::observability::random_correlation_bytes(),
                ),
                user: None,
                peer: Some(peer.to_owned()),
            },
            fields,
            arcen_telemetry::names::target::SESSION,
            "session handover",
        );
    }

    /// Whether an agent of `uid`'s own session is parked, so it can be
    /// asked for its logout.
    #[must_use]
    pub fn has_parked_user_agent(&self, uid: u32) -> bool {
        self.parked.lock().is_ok_and(|mut parked| {
            parked.retain(|entry| still_connected(&entry.stream));
            parked
                .iter()
                .any(|entry| entry.agent.uid == uid && entry.agent.kind == DesktopSessionKind::User)
        })
    }

    /// Asks a parked agent of `uid`'s own session to request that
    /// session's graceful logout ([`ServiceMessage::SignOut`]). Returns the
    /// native status of the request (0: delivered), never proof of a
    /// logout.
    ///
    /// # Errors
    ///
    /// Returns why no agent of that account could be asked.
    pub async fn request_sign_out(&self, uid: u32, session: u64) -> Result<i32, String> {
        let taken = self.parked.lock().ok().and_then(|mut parked| {
            parked.retain(|entry| still_connected(&entry.stream));
            let index = parked.iter().position(|entry| {
                entry.agent.uid == uid && entry.agent.kind == DesktopSessionKind::User
            })?;
            Some(parked.swap_remove(index))
        });
        let Some(mut entry) = taken else {
            return Err(format!(
                "no parked agent of uid {uid} to sign its session out"
            ));
        };
        write_line(&mut entry.stream, &ServiceMessage::SignOut { session })
            .await
            .map_err(|error| error.to_string())?;
        let line = tokio::time::timeout(ATTACH_TIMEOUT, read_line(&mut entry.stream))
            .await
            .map_err(|_| "no answer to the sign-out request".to_owned())?
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "the agent left without answering".to_owned())?;
        match serde_json::from_str::<AgentMessage>(&line) {
            Ok(AgentMessage::SignOutRequested { status }) => Ok(status),
            Ok(other) => Err(format!(
                "unexpected answer to the sign-out request: {other:?}"
            )),
            Err(error) => Err(format!("sign-out answer: {error}")),
        }
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
        Ok(other) => Err(RelayError::Protocol(format!(
            "attach answer: unexpected {other:?}"
        ))),
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
/// With it, the agent's `health_stats` also leave here carrying this
/// connection's measured path, which the agent cannot see.
///
/// `refine_allowed` is whether this host may refine at all (the build
/// contains the producer and `video.lossless_refinement` is on): the grant
/// then includes the `refine` lane for a Deck whose `auth_response` asked
/// for Y′CbCr tiles (ADR 0016 R7), and the relay carries refinement.
///
/// # Errors
///
/// Returns the first transport error when it was not a clean close.
pub async fn relay_lanes<A>(
    deck: arcen_transport::quic::SessionLaneSocket,
    agent: A,
    session: u64,
    audio_channel: Option<tokio::sync::oneshot::Receiver<UnixStream>>,
    path_signal: bool,
    refine_allowed: bool,
) -> Result<MediaRelayStats, String>
where
    A: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    relay_lanes_gated(
        deck,
        agent,
        session,
        audio_channel,
        path_signal,
        refine_allowed,
        None,
    )
    .await
}

/// The network service's own back-off on failed sign-ins.
///
/// The password is checked in the desktop agent, and the login window and
/// each signed-in account have their own agent process; a throttle in each
/// let a guesser who failed against one count afresh against the next. The
/// service relays every sign-in, so it keeps the one throttle for the whole
/// Mac (the shared policy, keyed by source and account), as the Linux and
/// Windows Piers do in their one network process. An agent still keeps its
/// own, for `serve`, where there is no service.
pub static SERVICE_AUTH_THROTTLE: std::sync::LazyLock<
    arcen_session::auth_throttle::HostAuthThrottle,
> = std::sync::LazyLock::new(arcen_session::auth_throttle::HostAuthThrottle::default);

/// One relayed sign-in, as the service's throttle sees it: the account the
/// Deck named (never its credential) and how the agent answered.
#[derive(Debug)]
pub struct SignInGate {
    throttle: &'static arcen_session::auth_throttle::HostAuthThrottle,
    source: Option<std::net::IpAddr>,
    attempt: Option<arcen_session::auth_throttle::AuthAttempt<'static>>,
    decided: bool,
}

impl SignInGate {
    /// A gate for sign-ins from `source` against `throttle`.
    #[must_use]
    pub const fn new(
        throttle: &'static arcen_session::auth_throttle::HostAuthThrottle,
        source: Option<std::net::IpAddr>,
    ) -> Self {
        Self {
            throttle,
            source,
            attempt: None,
            decided: false,
        }
    }

    /// Reads a Deck message before the attachment: whether to forward it,
    /// answer it with a refusal instead, or end the relay (a second sign-in
    /// on one connection, which only a client probing the gate sends).
    fn on_deck_text(&mut self, text: &str) -> DeckVerdict {
        #[derive(serde::Deserialize)]
        struct Account {
            #[serde(rename = "type")]
            msg_type: String,
            #[serde(default)]
            username: String,
        }
        // Decided on the parsed type, as the agent parses it: a type written
        // with JSON escapes is the same message to the agent. Only the
        // account is read; the credential beside it is skipped unparsed and
        // never copied here.
        let Some(account) = serde_json::from_str::<Account>(text)
            .ok()
            .filter(|account| account.msg_type == arcen_protocol::messages::AUTH_RESPONSE)
        else {
            return DeckVerdict::Forward;
        };
        if self.decided {
            return DeckVerdict::Close;
        }
        self.decided = true;
        // The agent checks no password for an empty name.
        if account.username.is_empty() {
            return DeckVerdict::Forward;
        }
        match self.throttle.begin(self.source, &account.username) {
            Ok(attempt) => {
                self.attempt = Some(attempt);
                DeckVerdict::Forward
            }
            Err(refusal) => {
                tracing::warn!(
                    target: arcen_telemetry::names::target::AUTH,
                    peer = ?self.source,
                    retry_after_secs = refusal.retry_after.as_secs(),
                    "sign-in refused by the service without asking the agent: too many recent \
                     failures"
                );
                serde_json::to_string(&arcen_protocol::messages::AuthResult {
                    msg_type: arcen_protocol::messages::AUTH_RESULT.to_owned(),
                    success: false,
                    message: crate::auth::AuthFailure::TooManyAttempts.to_string(),
                    resume_grant: None,
                    resume_window_secs: None,
                    resumed: false,
                    error_code: None,
                    session_setup_failed: false,
                })
                .map_or(DeckVerdict::Close, DeckVerdict::Refuse)
            }
        }
    }

    /// The agent proved the Deck's password, for another account than its
    /// own: a sign-in the throttle counts as accepted.
    fn on_switch_candidate(&mut self) {
        if let Some(attempt) = self.attempt.take() {
            attempt.finish(arcen_session::auth_throttle::AttemptOutcome::Accepted);
        }
    }

    /// Reads an agent message before the attachment: its `auth_result`
    /// ends the attempt with what the agent learnt.
    fn on_agent_text(&mut self, text: &str) {
        if self.attempt.is_none() || !text.contains(arcen_protocol::messages::AUTH_RESULT) {
            return;
        }
        let Some(result) = serde_json::from_str::<arcen_protocol::messages::AuthResult>(text)
            .ok()
            .filter(|result| result.msg_type == arcen_protocol::messages::AUTH_RESULT)
        else {
            return;
        };
        if let Some(attempt) = self.attempt.take() {
            attempt.finish(sign_in_outcome(&result));
        }
    }
}

/// What the service does with a Deck message before the attachment.
#[derive(Debug, PartialEq, Eq)]
enum DeckVerdict {
    /// Pass it to the agent.
    Forward,
    /// Answer with this `auth_result` and end the relay.
    Refuse(String),
    /// End the relay without an answer.
    Close,
}

/// What an agent's `auth_result` taught the throttle. The agent words a
/// refused credential exactly as [`crate::auth::AuthFailure`] does (same
/// binary); any other refusal said nothing about the credential.
fn sign_in_outcome(
    result: &arcen_protocol::messages::AuthResult,
) -> arcen_session::auth_throttle::AttemptOutcome {
    use crate::auth::AuthFailure;
    use arcen_session::auth_throttle::AttemptOutcome;
    if result.success || result.session_setup_failed {
        return AttemptOutcome::Accepted;
    }
    let refused = [
        AuthFailure::InvalidCredentials,
        AuthFailure::AccountNotPermitted,
    ]
    .iter()
    .any(|failure| failure.to_string() == result.message);
    if refused {
        AttemptOutcome::Refused
    } else {
        AttemptOutcome::Undecided
    }
}

/// [`relay_lanes`], with the service's sign-in throttle in front of the
/// agent (`gate`).
///
/// # Errors
///
/// Returns the first transport error when it was not a clean close.
pub async fn relay_lanes_gated<A>(
    deck: arcen_transport::quic::SessionLaneSocket,
    agent: A,
    session: u64,
    audio_channel: Option<tokio::sync::oneshot::Receiver<UnixStream>>,
    path_signal: bool,
    refine_allowed: bool,
    gate: Option<SignInGate>,
) -> Result<MediaRelayStats, String>
where
    A: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    match relay_lanes_handover(
        deck,
        agent,
        session,
        audio_channel,
        path_signal,
        refine_allowed,
        gate,
        None,
    )
    .await
    {
        RelayOutcome::Ended(result) => result,
        RelayOutcome::SignIn { .. } => Ok(MediaRelayStats::default()),
    }
}

/// The service's part in a relayed Deck's change of account
/// (`crate::handover`): the desktop's ledger, the agent the Deck reached,
/// and the connection the ledger knows the Deck by.
pub struct HandoverRelay {
    /// The registry, its ledger and its parked agents.
    pub registry: Arc<AgentRegistry>,
    /// The account the agent serves; `None` for the login window's.
    pub agent_uid: Option<u32>,
    /// The Deck's connection, kept across its hand-over to the login window.
    pub connection: u64,
    /// The Deck's address, for the records.
    pub peer: String,
    /// The Deck's own `auth_response`, given again to the login window's
    /// agent after a completed sign-out. It carries the password and is
    /// wiped when dropped.
    pub replay: Option<zeroize::Zeroizing<String>>,
}

impl std::fmt::Debug for HandoverRelay {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HandoverRelay")
            .field("agent_uid", &self.agent_uid)
            .field("connection", &self.connection)
            .field("peer", &self.peer)
            .field("replay", &self.replay.as_ref().map(|_| "<redacted>"))
            .finish_non_exhaustive()
    }
}

/// How a relayed Deck ended.
pub enum RelayOutcome {
    /// The relay ended.
    Ended(Result<MediaRelayStats, String>),
    /// The other account was signed out: the Deck signs in at the login
    /// window next, with the `auth_response` it already sent.
    SignIn {
        /// The Deck, still connected.
        deck: arcen_transport::quic::SessionLaneSocket,
        /// Its `auth_response`.
        auth_response: zeroize::Zeroizing<String>,
    },
}

impl std::fmt::Debug for RelayOutcome {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ended(result) => formatter.debug_tuple("Ended").field(result).finish(),
            Self::SignIn { .. } => formatter.write_str("SignIn { .. }"),
        }
    }
}

/// [`relay_lanes_gated`], with the service's part in a change of account
/// (`handover`): an agent's `switch_candidate` is answered by the service
/// (offer, confirmation, the target's own graceful logout, the exit
/// proof), and every sign-in an agent accepts is checked against the
/// desktop's ledger before the Deck is attached.
///
/// # Errors
///
/// Returns the first transport error when it was not a clean close.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub async fn relay_lanes_handover<A>(
    mut deck: arcen_transport::quic::SessionLaneSocket,
    agent: A,
    session: u64,
    audio_channel: Option<tokio::sync::oneshot::Receiver<UnixStream>>,
    path_signal: bool,
    refine_allowed: bool,
    mut gate: Option<SignInGate>,
    mut handover: Option<HandoverRelay>,
) -> RelayOutcome
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
    let mut side_open = true;
    let side_channel = audio_channel.map(|channel| {
        tokio::spawn(async move {
            if let Ok(channel) = channel.await {
                read_audio_channel(channel, side_tx).await;
            }
        })
    });

    let mut path_state = arcen_telemetry::PathSignalState::default();
    let mut path_quality = arcen_transport::PathQualityTracker::default();
    let path_started = tokio::time::Instant::now();
    let mut path_tick = tokio::time::interval(std::time::Duration::from_secs(1));
    path_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The agent authenticates the Deck over the control lane and takes
    // nothing but the Deck's own `auth_response` until that is done, so path
    // truth waits for the Deck's `client_hello`, which only follows a
    // successful authentication.
    let mut session_started = false;
    // Whether the grant carries the refine lane, from the Deck's own
    // `auth_response` (it travels through here to the agent).
    let mut refine_grant = false;
    // The relay's running count of video frames it is done with, reported
    // to the agent once it streams (`ServiceMessage::VideoReleased`), so the
    // agent knows when video still waits here (`RateHints::video_backlog`).
    let mut video_released: Option<tokio::sync::watch::Receiver<u64>> = None;
    let mut video_released_reported = 0_u64;
    // The Deck's `auth_response`, kept only while a change of account may
    // still need it, and whether it can confirm one.
    let mut credentials = SwitchCredentials::default();
    let mut deck_account_switch = false;
    let mut resend_auth = handover
        .as_mut()
        .and_then(|handover| handover.replay.take());
    let replayed_sign_in = resend_auth.is_some();
    let mut signed_out = false;

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
                    if relay.is_none()
                        && handover.is_some()
                        && let Some(switch) = credentials
                            .on_deck_text(text, crate::handover::GRACEFUL_LOGOUT_ROUTE)
                    {
                        deck_account_switch = switch;
                    }
                    if relay.is_none() {
                        refine_grant |= refine_allowed && asks_for_ycbcr_refinement(text);
                        match gate.as_mut().map_or(DeckVerdict::Forward, |gate| gate.on_deck_text(text)) {
                            DeckVerdict::Forward => {}
                            DeckVerdict::Refuse(refusal) => {
                                credentials.release();
                                let _ = deck.send(SessionRecord::Text(refusal)).await;
                                break Ok(());
                            }
                            DeckVerdict::Close => break Ok(()),
                        }
                    }
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
                let Some(mut record) = message_record(message) else {
                    continue;
                };
                if path_signal
                    && let SessionRecord::Text(text) = &record
                    && let Some(filled) = arcen_observability::health_stats_with_path(text, || {
                        path_quality.sample(path_started.elapsed(), &connection)
                    })
                {
                    record = SessionRecord::Text(filled);
                }
                stats.to_deck += 1;
                if relay.is_none()
                    && let (Some(handover), SessionRecord::Text(text)) = (handover.as_ref(), &record)
                {
                    // The login window's agent asks for the credentials the
                    // Deck already gave: it gets them, and the Deck, which
                    // waits for `auth_result`, sees no second prompt.
                    if is_text_type(&record, arcen_protocol::messages::AUTH_REQUEST)
                        && let Some(stored) = resend_auth.take()
                    {
                        let resend = tokio_tungstenite::tungstenite::Message::Text(
                            stored.as_str().to_owned(),
                        );
                        drop(stored);
                        if let Err(error) = agent_tx.send(resend).await {
                            break Err(format!("to agent: {error}"));
                        }
                        continue;
                    }
                    if let Some(candidate) = switch_candidate(text) {
                        if let Some(gate) = gate.as_mut() {
                            gate.on_switch_candidate();
                        }
                        signed_out =
                            run_switch(&mut deck, handover, session, candidate, deck_account_switch)
                                .await;
                        break Ok(());
                    }
                    // The sign-in ended another way: the credentials are not
                    // needed again.
                    if is_text_type(&record, arcen_protocol::messages::AUTH_RESULT) {
                        credentials.release();
                    }
                    if is_successful_auth_result(&record) {
                        let admitted = handover
                            .registry
                            .handover()
                            .admit(handover.connection, handover.agent_uid);
                        if replayed_sign_in {
                            let _ = handover
                                .registry
                                .handover()
                                .login_finished(handover.connection, admitted.is_ok());
                        }
                        if let Err(refusal) = admitted {
                            handover.registry.handover_event(
                                refusal.reason(),
                                None,
                                None,
                                &handover.peer,
                            );
                            let refused =
                                handover_refusal_result(refusal, deck_account_switch, None);
                            let _ = deck.send(SessionRecord::Text(refused)).await;
                            break Ok(());
                        }
                    } else if replayed_sign_in && is_text_type(&record, arcen_protocol::messages::AUTH_RESULT) {
                        let _ = handover
                            .registry
                            .handover()
                            .login_finished(handover.connection, false);
                    }
                }
                if relay.is_none()
                    && let (Some(gate), SessionRecord::Text(text)) = (gate.as_mut(), &record)
                {
                    gate.on_agent_text(text);
                }
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
                    match grant_lanes(&mut deck, refine_grant).await {
                        Ok(()) => {
                            let started = PierLaneRelay::start_with(&lanes, refine_grant);
                            video_released = Some(started.video_released());
                            relay = Some(started);
                            credentials.release();
                            attached_to_desktop(handover.as_ref(), session);
                        }
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
                    match grant_lanes(&mut deck, refine_grant).await {
                        Ok(()) => {
                            let started = PierLaneRelay::start_with(&lanes, refine_grant);
                            video_released = Some(started.video_released());
                            relay = Some(started);
                            credentials.release();
                            attached_to_desktop(handover.as_ref(), session);
                        }
                        Err(error) => break Err(error),
                    }
                }
            }
            // A closed channel (none opened, or it ended) is ready at once
            // forever: polled first it would spin this loop and starve the
            // arms below it.
            frame = side_rx.recv(), if side_open => {
                let Some(frame) = frame else {
                    side_open = false;
                    continue;
                };
                stats.side_channel_audio += 1;
                if let Some(relay) = relay.as_mut() {
                    let _ = relay.route_to_deck(SessionRecord::Binary(frame));
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
            changed = async {
                match video_released.as_mut() {
                    Some(released) => released.changed().await,
                    None => std::future::pending().await,
                }
            }, if session_started => {
                let (Ok(()), Some(released)) = (changed, video_released.as_mut()) else {
                    video_released = None;
                    continue;
                };
                let total = *released.borrow_and_update();
                if total <= video_released_reported {
                    continue;
                }
                video_released_reported = total;
                let message = ServiceMessage::VideoReleased { session, released: total };
                let text = match serde_json::to_string(&message) {
                    Ok(text) => text,
                    Err(error) => break Err(format!("video released encode: {error}")),
                };
                if let Err(error) = agent_tx
                    .send(tokio_tungstenite::tungstenite::Message::Text(text))
                    .await
                {
                    break Err(format!("to agent video released: {error}"));
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
    if let Some(handover) = handover.as_ref() {
        let ledger_desk = handover.registry.handover();
        ledger_desk.detached(session);
        if !signed_out && let Some(reason) = ledger_desk.requester_left(handover.connection) {
            handover
                .registry
                .handover_event(reason, None, None, &handover.peer);
        }
    }
    match (signed_out, credentials.take()) {
        (true, Some(auth_response)) => RelayOutcome::SignIn {
            deck,
            auth_response,
        },
        _ => RelayOutcome::Ended(result.map(|()| stats)),
    }
}

/// A Deck's lanes were granted: one more attachment to this desktop.
fn attached_to_desktop(handover: Option<&HandoverRelay>, session: u64) {
    let Some(handover) = handover else {
        return;
    };
    if let Some(returned) = handover.registry.handover().attached(session)
        && returned != arcen_session::session_handover::consent::ClientReturn::NoOffer
    {
        tracing::info!(
            target: arcen_telemetry::names::target::SESSION,
            ?returned,
            "a Deck attached while a change of account was open"
        );
    }
}

/// The Deck's `auth_response`, which carries its password, held only while
/// a change of account may still need it: when this Pier offers one
/// (`route`) and the Deck can confirm it (`account_switch_v1`), and only
/// until the sign-in ends another way.
#[derive(Default)]
struct SwitchCredentials {
    held: Option<zeroize::Zeroizing<String>>,
}

impl SwitchCredentials {
    /// Reads a Deck message before the attachment. For its `auth_response`,
    /// keeps it when a change of account could follow and returns whether
    /// the Deck can confirm one.
    fn on_deck_text(&mut self, text: &str, route: bool) -> Option<bool> {
        let switch = account_switch_request(text)?;
        self.held = (route && switch).then(|| zeroize::Zeroizing::new(text.to_owned()));
        Some(switch)
    }

    /// The sign-in ended without a change of account: an `auth_result`, a
    /// refusal or granted lanes.
    fn release(&mut self) {
        self.held = None;
    }

    /// Hands the credentials to the sign-in at the login window.
    fn take(&mut self) -> Option<zeroize::Zeroizing<String>> {
        self.held.take()
    }
}

/// Whether `text` is the Deck's `auth_response`, and then whether it can
/// confirm a change of account. Only those two fields are read.
fn account_switch_request(text: &str) -> Option<bool> {
    #[derive(serde::Deserialize)]
    struct Request {
        #[serde(rename = "type")]
        msg_type: String,
        #[serde(default)]
        account_switch_v1: bool,
    }
    serde_json::from_str::<Request>(text)
        .ok()
        .filter(|request| request.msg_type == arcen_protocol::messages::AUTH_RESPONSE)
        .map(|request| request.account_switch_v1)
}

/// The agent's [`AgentMessage::SwitchCandidate`], when `text` is one.
fn switch_candidate(text: &str) -> Option<(u32, String, bool)> {
    match serde_json::from_str::<AgentMessage>(text).ok()? {
        AgentMessage::SwitchCandidate {
            uid,
            user,
            logout_route,
        } => Some((uid, user, logout_route)),
        AgentMessage::Attached | AgentMessage::SignOutRequested { .. } => None,
    }
}

/// The `auth_result` that refuses a sign-in for `refusal`, worded by the
/// shared code (`console_refusal`): a Deck without `account_switch_v1`
/// never sees a code that came with it. Another account's session is
/// named, as this Pier always did.
fn handover_refusal_result(
    refusal: arcen_session::session_handover::HandoverRefusal,
    deck_account_switch: bool,
    owner: Option<&str>,
) -> String {
    use arcen_session::console_session::{ConsoleOrigin, ConsoleRefusal};
    // LoginWindow is the macOS display manager: every session is its.
    let code = refusal.console_refusal(ConsoleOrigin::DisplayManager, deck_account_switch);
    let message = if code == ConsoleRefusal::OtherUser {
        crate::console::other_user_refusal(owner)
    } else {
        code.deck_reason().to_owned()
    };
    serde_json::to_string(&arcen_protocol::messages::AuthResult {
        msg_type: arcen_protocol::messages::AUTH_RESULT.to_owned(),
        success: false,
        message,
        resume_grant: None,
        resume_window_secs: None,
        resumed: false,
        error_code: None,
        session_setup_failed: false,
    })
    .unwrap_or_default()
}

/// Sends `message` to the Deck as one control text.
async fn send_deck_json<T: serde::Serialize>(
    deck: &mut arcen_transport::quic::SessionLaneSocket,
    message: &T,
) -> bool {
    use futures_util::SinkExt as _;
    match serde_json::to_string(message) {
        Ok(text) => deck
            .send(arcen_protocol::lanes::SessionRecord::Text(text))
            .await
            .is_ok(),
        Err(_) => false,
    }
}

/// The one native read of the desktop's facts at a time; a second waits
/// for nothing and reads as unknown, which refuses (fail closed).
static HANDOVER_FACTS: crate::blocking::BlockingSlot = crate::blocking::BlockingSlot::new();

/// How long a read of the facts may take before it counts as unknown.
const FACTS_TIMEOUT: Duration = Duration::from_secs(10);

/// Reads the desktop's facts off the async runtime.
async fn read_occupant(
    desk: &Arc<crate::handover::HandoverDesk>,
    session: u64,
) -> (
    arcen_session::session_handover::ConsoleOccupant,
    Option<crate::handover::ConsoleRecord>,
) {
    let desk = Arc::clone(desk);
    let unknown = || {
        (
            arcen_session::session_handover::ConsoleOccupant::Unknown,
            None,
        )
    };
    tokio::time::timeout(
        FACTS_TIMEOUT,
        crate::blocking::run_exclusive(
            "arcen-handover-facts",
            &HANDOVER_FACTS,
            |_| (),
            move || desk.read_occupant(session),
        ),
    )
    .await
    .ok()
    .and_then(Result::ok)
    .unwrap_or_else(unknown)
}

/// Reads whether `target` still exists, off the async runtime.
async fn read_presence(
    target: &arcen_session::session_handover::NativeSessionIdentity,
) -> arcen_session::session_handover::logout::SessionPresence {
    let target = target.clone();
    tokio::time::timeout(
        FACTS_TIMEOUT,
        crate::blocking::run_exclusive(
            "arcen-handover-facts",
            &HANDOVER_FACTS,
            |_| (),
            move || {
                let records = crate::handover::console_records();
                crate::handover::presence_in(
                    records.as_deref(),
                    crate::handover::boot_id().as_ref(),
                    &target,
                )
            },
        ),
    )
    .await
    .ok()
    .and_then(Result::ok)
    .unwrap_or(arcen_session::session_handover::logout::SessionPresence::Unknown)
}

/// The service's half of a change of account (ADR 0025 §13), after the
/// agent proved the Deck's account and found it is not its own: decide,
/// offer, read the answer, have the target's own agent ask for its
/// graceful logout, and prove the exact session ended. Returns whether it
/// did, and the Deck signs in at the login window next; otherwise the Deck
/// has been told why not.
#[allow(clippy::too_many_lines)]
async fn run_switch(
    deck: &mut arcen_transport::quic::SessionLaneSocket,
    handover: &HandoverRelay,
    session: u64,
    (uid, user, logout_route): (u32, String, bool),
    deck_account_switch: bool,
) -> bool {
    use arcen_protocol::lanes::SessionRecord;
    use arcen_protocol::messages::{
        ACCOUNT_SWITCH_REPLY, AccountSwitchChoiceMsg, AccountSwitchOfferMsg,
        AccountSwitchOutcomeMsg, AccountSwitchReplyMsg, AccountSwitchResultMsg,
    };
    use arcen_session::session_handover::consent::{
        ConnectionId, DEFAULT_OFFER_LIFETIME_SECS, ReplyRejection, SwitchNonce,
    };
    use arcen_session::session_handover::logout::{
        LOGOUT_COMPLETION_TIMEOUT, LogoutBlock, LogoutOutcome, LogoutVerdict, SessionExitProof,
        SessionPresence,
    };
    use arcen_session::session_handover::{
        AuthenticatedPrincipal, HandoverDecision, HandoverReason, HandoverRefusal, SwitchRoute,
    };
    use futures_util::{SinkExt as _, StreamExt as _};

    let registry = &handover.registry;
    let switch_desk = registry.handover();
    let connection = ConnectionId::new(handover.connection);
    let peer = handover.peer.as_str();
    let refuse = |refusal: HandoverRefusal, owner: Option<&str>| {
        registry.handover_event(refusal.reason(), None, None, peer);
        handover_refusal_result(refusal, deck_account_switch, owner)
    };
    // Only a user's agent proves another account, and never its own.
    let Some(agent_uid) = handover.agent_uid.filter(|agent| *agent != uid) else {
        return false;
    };
    let Some(requester) = crate::handover::principal(uid)
        .filter(|_| crate::handover::account_name(uid).as_deref() == Some(user.as_str()))
        .map(AuthenticatedPrincipal::after_authentication)
    else {
        let text = refuse(HandoverRefusal::OccupantUnknown, None);
        let _ = deck.send(SessionRecord::Text(text)).await;
        return false;
    };
    let (occupant, record) = read_occupant(switch_desk, session).await;
    let owner = record.as_ref().map(|record| record.name.clone());
    // The agent that proved the Deck must be the desktop's own.
    if record
        .as_ref()
        .is_some_and(|record| record.uid != agent_uid)
    {
        let text = refuse(HandoverRefusal::OccupantUnknown, None);
        let _ = deck.send(SessionRecord::Text(text)).await;
        return false;
    }
    let route = SwitchRoute::from_capabilities(
        deck_account_switch,
        logout_route
            && crate::handover::GRACEFUL_LOGOUT_ROUTE
            && registry.has_parked_user_agent(agent_uid),
    );
    let now = switch_desk.now();
    let Some(decided) = switch_desk
        .with_ledger(|ledger| ledger.decide(connection, &requester, &occupant, route, now))
    else {
        return false;
    };
    if let Some(lapsed) = decided.lapsed {
        registry.handover_event(lapsed.reason(), None, None, peer);
    }
    let candidate = match decided.decision {
        HandoverDecision::OfferSwitch(candidate) => candidate,
        HandoverDecision::Refuse(refusal) => {
            let client = match refusal {
                HandoverRefusal::Busy(client) => Some(client),
                _ => None,
            };
            registry.handover_event(refusal.reason(), None, client, peer);
            let text = handover_refusal_result(refusal, deck_account_switch, owner.as_deref());
            let _ = deck.send(SessionRecord::Text(text)).await;
            return false;
        }
        // The console changed under the agent (the login window, or the
        // account itself): this Deck starts again.
        HandoverDecision::SignInFresh
        | HandoverDecision::JoinOwnSession { .. }
        | HandoverDecision::JoinOwnSessionAfterIncumbent(_) => {
            let text = refuse(HandoverRefusal::SigningOut, None);
            let _ = deck.send(SessionRecord::Text(text)).await;
            return false;
        }
    };
    let target_uid = agent_uid;

    let mut entropy = [0_u8; arcen_session::session_handover::consent::SWITCH_NONCE_BYTES];
    let Some(nonce) = getrandom::getrandom(&mut entropy)
        .ok()
        .and_then(|()| SwitchNonce::from_random(entropy))
    else {
        return false;
    };
    let opened = switch_desk.with_ledger(|ledger| {
        ledger.open(
            candidate,
            connection,
            nonce,
            switch_desk.now(),
            DEFAULT_OFFER_LIFETIME_SECS,
        )
    });
    let ticket = match opened {
        Some(Ok(opened)) => {
            if let Some(lapsed) = opened.lapsed {
                registry.handover_event(lapsed.reason(), None, None, peer);
            }
            opened.ticket
        }
        Some(Err(refusal)) => {
            registry.handover_event(refusal.reason(), None, None, peer);
            if let Some(refusal) = refusal.handover_refusal() {
                let text = handover_refusal_result(refusal, deck_account_switch, None);
                let _ = deck.send(SessionRecord::Text(text)).await;
            }
            return false;
        }
        None => return false,
    };
    let offer_id = ticket.nonce.to_hex();
    let Ok(offer) = AccountSwitchOfferMsg::new(offer_id.clone(), ticket.expires_in_secs) else {
        let _ = switch_desk.with_ledger(|ledger| ledger.requester_left(connection));
        return false;
    };
    if !send_deck_json(deck, &offer).await {
        let _ = switch_desk.with_ledger(|ledger| ledger.requester_left(connection));
        return false;
    }
    registry.handover_event(HandoverReason::SwitchOffered, None, None, peer);
    let result =
        |outcome: AccountSwitchOutcomeMsg| AccountSwitchResultMsg::new(offer_id.clone(), outcome);

    // The answer, before the offer expires.
    let deadline = tokio::time::Instant::now()
        + std::time::Duration::from_secs(u64::from(ticket.expires_in_secs));
    let (nonce, choice) = loop {
        let next = tokio::time::timeout_at(deadline, deck.next()).await;
        let record = match next {
            Err(_) => {
                let lapsed = switch_desk
                    .with_ledger(|ledger| ledger.expire(switch_desk.now()))
                    .flatten();
                registry.handover_event(
                    lapsed.map_or(
                        HandoverReason::SwitchExpired,
                        arcen_session::session_handover::consent::Lapse::reason,
                    ),
                    None,
                    None,
                    peer,
                );
                let _ = send_deck_json(deck, &result(AccountSwitchOutcomeMsg::Expired)).await;
                return false;
            }
            Ok(None | Some(Err(_))) => {
                if let Some(reason) = switch_desk
                    .with_ledger(|ledger| ledger.requester_left(connection))
                    .flatten()
                {
                    registry.handover_event(reason, None, None, peer);
                }
                return false;
            }
            Ok(Some(Ok(record))) => record,
        };
        let SessionRecord::Text(text) = record else {
            continue;
        };
        let Ok(reply) = serde_json::from_str::<AccountSwitchReplyMsg>(&text) else {
            continue;
        };
        if reply.msg_type != ACCOUNT_SWITCH_REPLY {
            continue;
        }
        let Some(nonce) = reply
            .validate()
            .ok()
            .and_then(|()| SwitchNonce::parse_hex(&reply.offer_id))
        else {
            registry.handover_event(HandoverReason::SwitchRejected, None, None, peer);
            let _ = send_deck_json(deck, &result(AccountSwitchOutcomeMsg::Rejected)).await;
            let _ = switch_desk.with_ledger(|ledger| ledger.requester_left(connection));
            return false;
        };
        break (nonce, reply.choice);
    };

    let rejected = |rejection: ReplyRejection| match rejection {
        ReplyRejection::Expired => AccountSwitchOutcomeMsg::Expired,
        ReplyRejection::Stale(_) => AccountSwitchOutcomeMsg::Stale,
        ReplyRejection::NoOffer
        | ReplyRejection::Replayed
        | ReplyRejection::WrongConnection
        | ReplyRejection::Mismatched => AccountSwitchOutcomeMsg::Rejected,
    };
    if choice == AccountSwitchChoiceMsg::Cancel {
        let cancelled =
            switch_desk.with_ledger(|ledger| ledger.cancel(connection, &nonce, switch_desk.now()));
        let outcome = match cancelled {
            Some(Ok(())) => {
                registry.handover_event(HandoverReason::SwitchCancelled, None, None, peer);
                AccountSwitchOutcomeMsg::Cancelled
            }
            Some(Err(rejection)) => {
                registry.handover_event(rejection.reason(), None, None, peer);
                rejected(rejection)
            }
            None => AccountSwitchOutcomeMsg::Rejected,
        };
        let _ = send_deck_json(deck, &result(outcome)).await;
        return false;
    }

    // Confirmed: the facts again, immediately before anything changes.
    let (fresh, _) = read_occupant(switch_desk, session).await;
    let confirmed = switch_desk
        .with_ledger(|ledger| ledger.confirm(connection, &nonce, switch_desk.now(), &fresh));
    let authority = match confirmed {
        Some(Ok(authority)) => authority,
        Some(Err(rejection)) => {
            registry.handover_event(rejection.reason(), None, None, peer);
            let _ = send_deck_json(deck, &result(rejected(rejection))).await;
            return false;
        }
        None => return false,
    };
    registry.handover_event(HandoverReason::LogoutRequested, None, None, peer);
    let target = authority.target().clone();
    let outcome = match registry.request_sign_out(target_uid, session).await {
        Ok(0) => {
            let deadline = tokio::time::Instant::now() + LOGOUT_COMPLETION_TIMEOUT;
            let mut requester_here = true;
            loop {
                let presence = read_presence(&target).await;
                if let Some(proof) = SessionExitProof::read_back(&target, presence) {
                    break LogoutOutcome::Completed(proof);
                }
                if tokio::time::Instant::now() >= deadline {
                    // Still there at the deadline: an application or the
                    // person at the desk kept it. Never forced.
                    break if presence == SessionPresence::Present {
                        LogoutOutcome::Vetoed
                    } else {
                        LogoutOutcome::TimedOut
                    };
                }
                // The logout runs on whether or not the requester stays;
                // one who left is never signed in afterwards.
                if requester_here {
                    tokio::select! {
                        next = deck.next() => if matches!(next, None | Some(Err(_))) {
                            requester_here = false;
                            let _ = switch_desk
                                .with_ledger(|ledger| ledger.requester_left(connection));
                        },
                        () = tokio::time::sleep(crate::handover::EXIT_POLL) => {}
                    }
                } else {
                    tokio::time::sleep(crate::handover::EXIT_POLL).await;
                }
            }
        }
        Ok(status) => {
            eprintln!("the target session's agent could not ask for its logout: status {status}");
            LogoutOutcome::Failed
        }
        Err(error) => {
            eprintln!("the target session's agent could not be asked for its logout: {error}");
            LogoutOutcome::Failed
        }
    };
    let verdict = switch_desk
        .with_ledger(|ledger| ledger.finish_logout(authority, outcome, switch_desk.now()));
    let Some(verdict) = verdict else {
        return false;
    };
    registry.handover_event(verdict.reason(), None, None, peer);
    match verdict {
        LogoutVerdict::Completed { permit, .. } => {
            switch_desk.hold_permit(permit);
            if send_deck_json(deck, &result(AccountSwitchOutcomeMsg::SignedOut)).await {
                true
            } else {
                let _ = switch_desk.login_finished(handover.connection, false);
                false
            }
        }
        LogoutVerdict::CompletedRequesterGone { .. } => false,
        LogoutVerdict::Blocked(block) => {
            let outcome = match block {
                LogoutBlock::Vetoed => AccountSwitchOutcomeMsg::LogoutVetoed,
                LogoutBlock::TimedOut => AccountSwitchOutcomeMsg::LogoutTimedOut,
                LogoutBlock::Failed | LogoutBlock::ExitUnproven | LogoutBlock::NotCommitting => {
                    AccountSwitchOutcomeMsg::LogoutFailed
                }
            };
            let _ = send_deck_json(deck, &result(outcome)).await;
            false
        }
    }
}

/// Authorizes the attachment's lanes and tells the Deck, on the control
/// lane, before anything the agent sends after authentication.
async fn grant_lanes(
    deck: &mut arcen_transport::quic::SessionLaneSocket,
    refine: bool,
) -> Result<(), String> {
    use futures_util::SinkExt as _;

    let grant = arcen_transport::quic::grant_attachment(
        deck.lanes(),
        arcen_transport::quic::next_attachment_generation(),
        arcen_protocol::lanes::LaneRoleSet::for_grant(refine),
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

/// Whether a Deck's `auth_response` asked for exact still image in the
/// Y′CbCr domain this host refines in (`lossless_refine_v1`).
/// Only the two fields are read: the credentials beside them are skipped
/// unparsed, never copied here.
fn asks_for_ycbcr_refinement(text: &str) -> bool {
    #[derive(serde::Deserialize)]
    struct RefineAsk {
        #[serde(rename = "type")]
        msg_type: String,
        #[serde(default)]
        lossless_refine_v1: Option<arcen_protocol::refine::LosslessRefineCapability>,
    }
    if !text.contains("lossless_refine_v1") {
        return false;
    }
    serde_json::from_str::<RefineAsk>(text)
        .ok()
        .filter(|ask| ask.msg_type == arcen_protocol::messages::AUTH_RESPONSE)
        .and_then(|ask| ask.lossless_refine_v1)
        .is_some_and(|capability| capability.accepts(arcen_protocol::refine::RefineDomain::YCbCr))
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
        ServiceMessage::PathSignal { .. } | ServiceMessage::VideoReleased { .. } => {}
        ServiceMessage::Refused { reason } => return Err(RelayError::Refused(reason)),
        ServiceMessage::Attach { .. } | ServiceMessage::SignOut { .. } => {
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
        ServiceMessage::SignOut { session } => {
            let status = crate::handover::sign_out_on_request();
            eprintln!(
                "a confirmed change of account asked for this session's logout (Deck {session}, \
                 status {status})"
            );
            write_line(&mut stream, &AgentMessage::SignOutRequested { status }).await?;
            Err(RelayError::SignOutRequested(status))
        }
        ServiceMessage::Refused { reason } => Err(RelayError::Refused(reason)),
        ServiceMessage::PathSignal { .. } => Err(RelayError::Protocol(
            "received path signal before Deck attachment".to_owned(),
        )),
        ServiceMessage::VideoReleased { .. } => Err(RelayError::Protocol(
            "received a video release before Deck attachment".to_owned(),
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
        ServiceMessage::PathSignal { .. } | ServiceMessage::VideoReleased { .. } => Ok(stream),
        ServiceMessage::Refused { reason } => Err(RelayError::Refused(reason)),
        ServiceMessage::Attach { .. } | ServiceMessage::SignOut { .. } => Err(
            RelayError::Protocol("offered a Deck on an audio channel".to_owned()),
        ),
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

    #[test]
    fn the_service_throttles_sign_ins_across_every_agent_it_relays_to() {
        let throttle: &'static arcen_session::auth_throttle::HostAuthThrottle = Box::leak(
            Box::new(arcen_session::auth_throttle::HostAuthThrottle::default()),
        );
        let ask = r#"{"type":"auth_response","method":"pam","username":"guessed","credential":"nope","screen_width":1,"screen_height":1}"#;
        let refused = serde_json::to_string(&arcen_protocol::messages::AuthResult {
            msg_type: arcen_protocol::messages::AUTH_RESULT.to_owned(),
            success: false,
            message: crate::auth::AuthFailure::InvalidCredentials.to_string(),
            resume_grant: None,
            resume_window_secs: None,
            resumed: false,
            error_code: None,
            session_setup_failed: false,
        })
        .unwrap();
        let source: std::net::IpAddr = "203.0.113.7".parse().unwrap();
        // Each sign-in is its own relay, whichever agent (login window or a
        // signed-in account) answers it; the free failures are spent once.
        let policy = arcen_session::auth_throttle::AuthThrottlePolicy::default();
        for _ in 0..policy.free_failures {
            let mut gate = SignInGate::new(throttle, Some(source));
            assert_eq!(gate.on_deck_text(ask), DeckVerdict::Forward);
            gate.on_agent_text(&refused);
        }
        let mut gate = SignInGate::new(throttle, Some(source));
        assert_eq!(gate.on_deck_text(ask), DeckVerdict::Forward);
        gate.on_agent_text(&refused);
        // Backing off now: refused by the service, from another address too
        // (the account is a key of its own), without reaching an agent.
        let other: std::net::IpAddr = "203.0.113.8".parse().unwrap();
        // Written with an escape, the type is still the agent's
        // `auth_response`.
        let escaped = ask.replace("auth_response", "auth\\u005fresponse");
        let DeckVerdict::Refuse(refusal) =
            SignInGate::new(throttle, Some(other)).on_deck_text(&escaped)
        else {
            panic!("throttled");
        };
        let refusal: arcen_protocol::messages::AuthResult = serde_json::from_str(&refusal).unwrap();
        assert!(!refusal.success);
        assert_eq!(
            refusal.message,
            arcen_session::auth_throttle::THROTTLED_MESSAGE
        );
        // One sign-in per connection: a second ends the relay.
        let fresh: &'static arcen_session::auth_throttle::HostAuthThrottle = Box::leak(Box::new(
            arcen_session::auth_throttle::HostAuthThrottle::default(),
        ));
        let mut gate = SignInGate::new(fresh, Some(source));
        assert_eq!(gate.on_deck_text(ask), DeckVerdict::Forward);
        assert_eq!(gate.on_deck_text(ask), DeckVerdict::Close);
    }

    #[test]
    fn the_service_reads_only_the_switch_capability_and_never_forwards_a_candidate() {
        use arcen_protocol::messages::AuthResponse;
        let response = |switch| {
            serde_json::to_string(
                &AuthResponse::password("someone", "secret").with_account_switch_v1(switch),
            )
            .expect("encode")
        };
        assert_eq!(account_switch_request(&response(true)), Some(true));
        assert_eq!(account_switch_request(&response(false)), Some(false));
        assert_eq!(account_switch_request(r#"{"type":"client_hello"}"#), None);
        let candidate = serde_json::to_string(&AgentMessage::SwitchCandidate {
            uid: 502,
            user: "someone".to_owned(),
            logout_route: true,
        })
        .expect("encode");
        assert_eq!(
            switch_candidate(&candidate),
            Some((502, "someone".to_owned(), true))
        );
        assert_eq!(switch_candidate(r#"{"type":"auth_result"}"#), None);
        // A Deck can never forge the service's request to sign out.
        assert!(ServiceMessage::is_service_type("sign_out"));
    }

    #[test]
    fn the_password_is_kept_only_for_a_change_of_account_the_pier_offers() {
        use arcen_protocol::messages::AuthResponse;
        let response = |switch| {
            serde_json::to_string(
                &AuthResponse::password("someone", "secret").with_account_switch_v1(switch),
            )
            .expect("encode")
        };
        // The route off, or a Deck that cannot confirm: nothing is kept,
        // and the Deck's capability is still read.
        let mut credentials = SwitchCredentials::default();
        assert_eq!(credentials.on_deck_text(&response(true), false), Some(true));
        assert!(credentials.take().is_none());
        assert_eq!(
            credentials.on_deck_text(&response(false), true),
            Some(false)
        );
        assert!(credentials.take().is_none());
        assert_eq!(
            credentials.on_deck_text(r#"{"type":"client_hello"}"#, true),
            None
        );
        assert!(credentials.take().is_none());
        // Kept for a possible change of account, gone once the sign-in
        // ended another way.
        assert_eq!(credentials.on_deck_text(&response(true), true), Some(true));
        credentials.release();
        assert!(credentials.take().is_none());
        assert_eq!(credentials.on_deck_text(&response(true), true), Some(true));
        assert!(
            credentials
                .take()
                .is_some_and(|kept| kept.contains("secret"))
        );
        assert!(credentials.take().is_none());
    }

    #[test]
    fn an_older_deck_never_sees_a_code_that_came_with_the_switch() {
        use arcen_session::session_handover::HandoverRefusal;
        let message =
            |refusal, switch| {
                serde_json::from_str::<arcen_protocol::messages::AuthResult>(
                    &handover_refusal_result(refusal, switch, Some("alice")),
                )
                .expect("auth_result")
                .message
            };
        assert!(message(HandoverRefusal::SigningOut, true).starts_with("console_signing_out"));
        assert!(message(HandoverRefusal::SigningOut, false).starts_with("console_session_changed"));
        assert!(
            message(HandoverRefusal::SwitchInProgress, true)
                .starts_with("console_switch_in_progress")
        );
        // Another account's session is named, as this Pier always did.
        let other = message(HandoverRefusal::PierCannotSignOut, true);
        assert!(other.starts_with("console_other_user: alice"), "{other}");
        assert!(
            message(HandoverRefusal::AttachmentUnknown, false).starts_with("console_other_user:")
        );
    }

    #[test]
    fn only_a_refused_credential_counts_against_a_sign_in() {
        use arcen_session::auth_throttle::AttemptOutcome;
        let result = |success, setup, message: String| arcen_protocol::messages::AuthResult {
            msg_type: arcen_protocol::messages::AUTH_RESULT.to_owned(),
            success,
            message,
            resume_grant: None,
            resume_window_secs: None,
            resumed: false,
            error_code: None,
            session_setup_failed: setup,
        };
        use crate::auth::AuthFailure;
        assert_eq!(
            sign_in_outcome(&result(true, false, "Authenticated".into())),
            AttemptOutcome::Accepted
        );
        // A session the host could not start: the password was right.
        assert_eq!(
            sign_in_outcome(&result(false, true, "video_request_refused: x".into())),
            AttemptOutcome::Accepted
        );
        for counted in [
            AuthFailure::InvalidCredentials,
            AuthFailure::AccountNotPermitted,
        ] {
            assert_eq!(
                sign_in_outcome(&result(false, false, counted.to_string())),
                AttemptOutcome::Refused
            );
        }
        for not_counted in [
            AuthFailure::PolicyUnavailable,
            AuthFailure::NotConsoleOwner,
            AuthFailure::HostBusy,
            AuthFailure::HostLockRefused,
            AuthFailure::TooManyAttempts,
        ] {
            assert_eq!(
                sign_in_outcome(&result(false, false, not_counted.to_string())),
                AttemptOutcome::Undecided
            );
        }
    }

    #[test]
    fn the_refine_lane_is_granted_only_to_a_deck_asking_for_ycbcr_tiles() {
        let ask = |domains: &str| {
            format!(
                r#"{{"type":"auth_response","username":"u","password":"p","lossless_refine_v1":{{"domains":{domains},"codecs":["delta-zstd1"]}}}}"#
            )
        };
        assert!(asks_for_ycbcr_refinement(&ask(r#"["rgb","ycbcr"]"#)));
        assert!(!asks_for_ycbcr_refinement(&ask(r#"["rgb"]"#)));
        assert!(!asks_for_ycbcr_refinement(
            r#"{"type":"auth_response","username":"u","password":"p"}"#
        ));
        assert!(!asks_for_ycbcr_refinement(
            &ask(r#"["ycbcr"]"#).replace("auth_response", "client_hello")
        ));
    }

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
        lane_rig_with(name, false).await
    }

    async fn lane_rig_with(name: &str, path_signal: bool) -> LaneRig {
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
        std::fs::set_permissions(
            dir.join("host.key"),
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o600),
        )
        .expect("key mode");
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
        tokio::spawn(relay_lanes(pier, relay_end, 1, None, path_signal, false));
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

    #[tokio::test]
    async fn the_deck_receives_the_agent_s_health_stats_with_the_service_s_path() {
        use futures_util::SinkExt as _;
        use tokio_tungstenite::tungstenite::Message;

        let mut rig = lane_rig_with("health", true).await;
        for text in [
            r#"{"type":"auth_result","success":true}"#,
            r#"{"type":"server_hello"}"#,
        ] {
            rig.agent
                .send(Message::Text(text.into()))
                .await
                .expect("agent control");
        }
        assert!(deck_text(&mut rig).await.contains("auth_result"));
        let grant = deck_text(&mut rig).await;
        arcen_transport::quic::accept_attachment(
            &rig.deck,
            &serde_json::from_str(&grant).expect("grant"),
        )
        .expect("accepted");
        assert!(deck_text(&mut rig).await.contains("server_hello"));
        let agent_stats = arcen_protocol::messages::HealthStatsMsg {
            frames_sent: 57,
            ..Default::default()
        };
        assert!(agent_stats.path.is_none(), "the agent cannot see the path");
        rig.agent
            .send(Message::Text(
                serde_json::to_string(&agent_stats).expect("json").into(),
            ))
            .await
            .expect("health_stats");
        let received: arcen_protocol::messages::HealthStatsMsg =
            serde_json::from_str(&deck_text(&mut rig).await).expect("health_stats");
        assert_eq!(received.frames_sent, 57);
        let path = received.path.expect("the service measured the path");
        assert!(path.congestion_control_token().is_some(), "{path:?}");
        assert!(path.rtt_us > 0 && path.sent_packets > 0, "{path:?}");
        std::fs::remove_dir_all(&rig.dir).ok();
    }

    #[tokio::test]
    async fn the_agent_hears_how_many_video_frames_the_relay_is_done_with() {
        use arcen_protocol::lanes::{LaneRole, SessionRecord};
        use arcen_transport::quic::{PeerLaneEvent, PeerLaneReader};
        use futures_util::{SinkExt as _, StreamExt as _};
        use tokio_tungstenite::tungstenite::Message;

        let mut rig = lane_rig("released").await;
        for text in [
            r#"{"type":"auth_result","success":true}"#,
            r#"{"type":"server_hello"}"#,
        ] {
            rig.agent
                .send(Message::Text(text.into()))
                .await
                .expect("agent control");
        }
        assert!(deck_text(&mut rig).await.contains("auth_result"));
        let grant = deck_text(&mut rig).await;
        arcen_transport::quic::accept_attachment(
            &rig.deck,
            &serde_json::from_str(&grant).expect("grant"),
        )
        .expect("accepted");
        assert!(deck_text(&mut rig).await.contains("server_hello"));
        let hello = serde_json::to_string(&arcen_protocol::messages::ClientHelloMsg::default())
            .expect("hello");
        rig.deck_control
            .sender
            .send(arcen_transport::quic::session_record_bytes(
                &SessionRecord::Text(hello),
            ))
            .await
            .expect("client_hello");
        loop {
            let message = tokio::time::timeout(Duration::from_secs(5), rig.agent.next())
                .await
                .expect("in time")
                .expect("message")
                .expect("valid");
            if matches!(&message, Message::Text(text) if is_client_hello(text)) {
                break;
            }
        }

        let mut video_lane = PeerLaneReader::new(&rig.deck, LaneRole::Video);
        rig.agent
            .send(Message::Binary(video(true)))
            .await
            .expect("video");
        loop {
            match tokio::time::timeout(Duration::from_secs(5), video_lane.next())
                .await
                .expect("video in time")
                .expect("video lane")
            {
                PeerLaneEvent::Frame(_) => break,
                PeerLaneEvent::Opened { .. } | PeerLaneEvent::Lost { .. } => {}
            }
        }
        let released = loop {
            let message = tokio::time::timeout(Duration::from_secs(5), rig.agent.next())
                .await
                .expect("a release report in time")
                .expect("message")
                .expect("valid");
            if let Message::Text(text) = message
                && let Ok(ServiceMessage::VideoReleased { released, .. }) =
                    serde_json::from_str::<ServiceMessage>(&text)
            {
                break released;
            }
        };
        assert_eq!(released, 1, "the one frame the relay sent into the lane");
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
    async fn a_parked_agent_of_the_account_is_asked_for_its_logout_and_answers() {
        let path = scratch_socket("signout");
        let listener = AgentRegistry::bind(&path).expect("bind");
        let own = crate::auth::resolve_account(
            &std::env::var("USER").unwrap_or_else(|_| "root".to_owned()),
        )
        .map(|account| account.uid);
        let registry = AgentRegistry::new(None, None);
        tokio::spawn(std::sync::Arc::clone(&registry).run(listener));
        let uid = own.expect("own uid");
        // Another account's agent is never asked.
        assert!(registry.request_sign_out(uid + 1, 7).await.is_err());

        let agent_path = path.clone();
        let agent =
            tokio::spawn(async move { park(&agent_path, DesktopSessionKind::User, own).await });
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !registry.has_parked_user_agent(uid) {
            assert!(tokio::time::Instant::now() < deadline, "the agent parked");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        // While this Pier offers no change of account the agent refuses the
        // request itself and asks nobody.
        let expected = if crate::handover::GRACEFUL_LOGOUT_ROUTE {
            0
        } else {
            crate::handover::SIGN_OUT_REFUSED
        };
        assert_eq!(registry.request_sign_out(uid, 7).await, Ok(expected));
        assert!(matches!(
            agent.await.expect("agent"),
            Err(RelayError::SignOutRequested(status)) if status == expected
        ));
        // The slot was used up; the agent parks a new one.
        assert!(!registry.has_parked_user_agent(uid));
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
