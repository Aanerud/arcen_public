//! The local socket between the Pier's network service and its desktop agents.
//!
//! The service runs as `_arcen` from boot, owns UDP 18444 and the TLS key, and
//! has no desktop. Each graphical session runs an agent, as that session's
//! user, which has a desktop and nothing else: no key, no listener. When a Deck
//! is admitted the service hands its stream to the agent of the session on the
//! console and relays bytes until either side leaves.
//!
//! The exchange itself is [`arcen_session::agent_relay`]. What lives here is
//! the macOS half: Unix sockets, the kernel's peer credentials, and which
//! executable the peer actually is.

#![allow(unsafe_code)]

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
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
    pub fn new(
        service_uid: Option<u32>,
        expected_program: Option<PathBuf>,
    ) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            parked: Mutex::new(Vec::new()),
            sequence: AtomicU64::new(1),
            audio_channels: Mutex::new(std::collections::HashMap::new()),
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

    /// Forgets a session's audio slot once its relay ends.
    pub fn end_session(&self, session: u64) {
        if let Ok(mut slots) = self.audio_channels.lock() {
            slots.remove(&session);
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

/// What a media relay carried.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MediaRelayStats {
    /// Messages forwarded from the Deck to the agent.
    pub to_agent: u64,
    /// Messages forwarded from the agent to the Deck on the session stream.
    pub to_deck: u64,
    /// Audio frames sent on the priority stream instead.
    pub priority_audio: u64,
}

/// Relays a Deck's session to its agent message by message, sending audio on
/// the Deck's priority stream once the Deck has said it accepts one.
///
/// A byte relay cannot do that: it does not know where one message ends, let
/// alone which messages are sound. So this reads WebSocket messages from both
/// sides and forwards them unchanged — the agent speaks as the server, so this
/// side reads it as a client — except that an audio frame goes to
/// [`arcen_transport::quic::open_audio_priority_stream`] once the Deck's
/// `client_hello` carries `audio_priority_stream_v1`.
///
/// # Errors
///
/// Returns the first transport error when it was not a clean close.
pub async fn relay_media(
    deck: arcen_transport::quic::DirectQuicStream,
    agent: UnixStream,
    audio_channel: tokio::sync::oneshot::Receiver<UnixStream>,
) -> Result<MediaRelayStats, String> {
    use futures_util::{SinkExt as _, StreamExt as _};
    use tokio_tungstenite::WebSocketStream;
    use tokio_tungstenite::tungstenite::protocol::{Message, Role, WebSocketConfig};

    let priority = std::sync::Arc::new(tokio::sync::Mutex::new(PriorityAudio::new(
        deck.connection_handle(),
    )));
    let deck_config = WebSocketConfig {
        max_message_size: Some(crate::net::MAX_INBOUND_MESSAGE),
        max_frame_size: Some(crate::net::MAX_INBOUND_MESSAGE),
        ..Default::default()
    };
    // The agent is local and trusted to send large frames: a keyframe is
    // megabytes. Tungstenite's defaults already bound it.
    let deck_ws = WebSocketStream::from_raw_socket(deck, Role::Server, Some(deck_config)).await;
    let agent_ws = WebSocketStream::from_raw_socket(agent, Role::Client, None).await;
    let (mut deck_tx, mut deck_rx) = deck_ws.split();
    let (mut agent_tx, mut agent_rx) = agent_ws.split();

    let wants_priority = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut stats = MediaRelayStats::default();

    // The agent's own audio channel, when it opens one: audio that never
    // shares a socket with video on the way here.
    let side_priority = std::sync::Arc::clone(&priority);
    let side_channel = tokio::spawn(async move {
        match audio_channel.await {
            Ok(channel) => forward_audio_channel(channel, side_priority).await,
            Err(_) => 0,
        }
    });

    let inbound_flag = std::sync::Arc::clone(&wants_priority);
    let inbound = async {
        let mut forwarded = 0_u64;
        while let Some(message) = deck_rx.next().await {
            let message = message.map_err(|error| format!("from Deck: {error}"))?;
            if let Message::Text(text) = &message {
                if deck_accepts_priority_audio(text) {
                    inbound_flag.store(true, std::sync::atomic::Ordering::Relaxed);
                }
            }
            let closing = matches!(message, Message::Close(_));
            agent_tx
                .send(message)
                .await
                .map_err(|error| format!("to agent: {error}"))?;
            forwarded += 1;
            if closing {
                break;
            }
        }
        let _ = agent_tx.close().await;
        Ok::<u64, String>(forwarded)
    };

    let outbound_priority = std::sync::Arc::clone(&priority);
    let outbound = async {
        let mut to_deck = 0_u64;
        let mut priority_audio = 0_u64;
        while let Some(message) = agent_rx.next().await {
            let message = message.map_err(|error| format!("from agent: {error}"))?;
            if let Message::Binary(bytes) = &message {
                let is_audio =
                    bytes.first().copied() == Some(arcen_protocol::wire::FrameType::Audio as u8);
                if is_audio && wants_priority.load(std::sync::atomic::Ordering::Relaxed) {
                    let mut priority = outbound_priority.lock().await;
                    if priority.usable() && priority.send(bytes).await {
                        priority_audio += 1;
                        continue;
                    }
                }
            }
            let closing = matches!(message, Message::Close(_));
            deck_tx
                .send(message)
                .await
                .map_err(|error| format!("to Deck: {error}"))?;
            to_deck += 1;
            if closing {
                break;
            }
        }
        let _ = deck_tx.close().await;
        Ok::<(u64, u64), String>((to_deck, priority_audio))
    };

    tokio::pin!(inbound);
    tokio::pin!(outbound);
    // Either side ending ends the session; the other is dropped, which
    // closes its socket.
    let ended = tokio::select! {
        result = &mut inbound => result.map(|to_agent| stats.to_agent = to_agent),
        result = &mut outbound => result.map(|(to_deck, priority_audio)| {
            stats.to_deck = to_deck;
            stats.priority_audio = priority_audio;
        }),
    };
    side_channel.abort();
    priority.lock().await.finish();
    ended.map(|()| stats)
}

/// Whether a Deck text message is a `client_hello` opting in to the audio
/// priority stream.
fn deck_accepts_priority_audio(text: &str) -> bool {
    if !text.contains(arcen_protocol::messages::CLIENT_HELLO) {
        return false;
    }
    serde_json::from_str::<arcen_protocol::messages::ClientHelloMsg>(text)
        .is_ok_and(|hello| hello.audio_priority_stream_v1)
}

/// Where the agent executable sits beside the service's own bundle.
///
/// `/Applications/Arcen Pier.app/Contents/MacOS/arcen-pier-macos` pairs with
/// `/Applications/Arcen Agent Helper.app/Contents/MacOS/arcen-agent-helper`.
/// Returns `None` when the service is not running from a bundle, which is a
/// build tree.
#[must_use]
pub fn expected_agent_program(service_executable: &Path) -> Option<PathBuf> {
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
/// Audio sent here reaches the Deck's priority stream without waiting behind
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
        ServiceMessage::Refused { reason } => Err(RelayError::Refused(reason)),
        ServiceMessage::Attach { .. } => Err(RelayError::Protocol(
            "offered a Deck on an audio channel".to_owned(),
        )),
    }
}

/// Forwards an agent's audio side channel onto the Deck's priority stream.
///
/// Returns how many frames it carried.
pub async fn forward_audio_channel(
    mut channel: UnixStream,
    priority: std::sync::Arc<tokio::sync::Mutex<PriorityAudio>>,
) -> u64 {
    let mut forwarded = 0_u64;
    loop {
        let mut length = [0_u8; 4];
        if channel.read_exact(&mut length).await.is_err() {
            return forwarded;
        }
        let length = usize::try_from(u32::from_be_bytes(length)).unwrap_or(usize::MAX);
        if length > arcen_protocol::AUDIO_PRIORITY_MAX_FRAME_BYTES {
            tracing::warn!(target: "arcen::relay", length, "oversized audio frame; channel closed");
            return forwarded;
        }
        let mut frame = vec![0_u8; length];
        if channel.read_exact(&mut frame).await.is_err() {
            return forwarded;
        }
        if priority.lock().await.send(&frame).await {
            forwarded += 1;
        }
    }
}

/// The Deck's audio priority stream, opened on first use and shared by
/// whichever path carries audio to it.
pub struct PriorityAudio {
    connection: quinn::Connection,
    stream: Option<quinn::SendStream>,
    failed: bool,
}

impl PriorityAudio {
    /// A priority stream not yet opened.
    #[must_use]
    pub const fn new(connection: quinn::Connection) -> Self {
        Self {
            connection,
            stream: None,
            failed: false,
        }
    }

    /// Whether the stream can still be used.
    #[must_use]
    pub const fn usable(&self) -> bool {
        !self.failed
    }

    /// Sends one frame, opening the stream on first use. Returns whether it
    /// was sent; once it fails it stays failed and the caller falls back.
    pub async fn send(&mut self, frame: &[u8]) -> bool {
        if self.failed {
            return false;
        }
        if self.stream.is_none() {
            match arcen_transport::quic::open_audio_priority_stream(&self.connection).await {
                Ok(stream) => {
                    tracing::info!(target: "arcen::relay", "audio moved to its own priority stream");
                    self.stream = Some(stream);
                }
                Err(error) => {
                    tracing::warn!(target: "arcen::relay", %error, "no audio priority stream");
                    self.failed = true;
                    return false;
                }
            }
        }
        let Some(stream) = self.stream.as_mut() else {
            return false;
        };
        match arcen_transport::quic::write_priority_frame(stream, frame).await {
            Ok(()) => true,
            Err(error) => {
                tracing::warn!(target: "arcen::relay", %error, "audio priority stream failed");
                self.stream = None;
                self.failed = true;
                false
            }
        }
    }

    /// Finishes the stream.
    pub fn finish(&mut self) {
        if let Some(mut stream) = self.stream.take() {
            let _ = stream.finish();
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

    #[test]
    fn only_an_opted_in_client_hello_moves_audio() {
        let hello = |flag: bool| {
            serde_json::to_string(&arcen_protocol::messages::ClientHelloMsg {
                audio_priority_stream_v1: flag,
                ..arcen_protocol::messages::ClientHelloMsg::default()
            })
            .expect("hello")
        };
        assert!(deck_accepts_priority_audio(&hello(true)));
        assert!(!deck_accepts_priority_audio(&hello(false)));
        assert!(!deck_accepts_priority_audio(r#"{"type":"auth_response"}"#));
        assert!(!deck_accepts_priority_audio("not json client_hello"));
    }

    #[test]
    fn the_agent_is_the_sibling_bundle() {
        assert_eq!(
            expected_agent_program(Path::new(
                "/Applications/Arcen Pier.app/Contents/MacOS/arcen-pier-macos"
            )),
            Some(PathBuf::from(
                "/Applications/Arcen Agent Helper.app/Contents/MacOS/arcen-agent-helper"
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
