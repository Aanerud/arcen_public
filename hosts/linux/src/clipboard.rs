//! Dedicated-Xorg clipboard policy, bounded relay state, and user-agent supervision.

use arcen_media::clipboard::eligibility::{
    clipboard_eligibility, ClipboardHostFacts, ClipboardIneligible,
};
use arcen_media::clipboard::{ClipboardPolicy, ClipboardRequest, HARD_MAX_CLIPBOARD_BYTES};
use arcen_protocol::clipboard::{ClipboardCursor, ClipboardTransfer, ClipboardWireMessage};
use arcen_protocol::messages::{
    ClientHelloMsg, ClipboardContentKind, ClipboardDirectionMsg, ClipboardPolicyMsg,
};
use arcen_protocol::CHUNK_BYTES;
use arcen_telemetry::CorrelationId;
use futures_util::SinkExt;
use serde::{Deserialize, Serialize};
use std::fmt::{Debug, Formatter};
use std::path::Path;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout};
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::protocol::Role;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;
use zeroize::Zeroize;

pub use arcen_media::clipboard::ClipboardNegotiation;

use crate::cli::{AuthMode, Config};
use crate::session::identity::UserExecution;
use crate::session::lifecycle::SessionMetadata;

const IPC_READY_LIMIT: usize = 4096;
const IPC_READY_TIMEOUT: Duration = Duration::from_secs(5);
const IPC_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
pub(crate) const CLIPBOARD_POLICY_TYPE: &str = "arcen_clipboard_policy";

/// The largest WebSocket message on a clipboard bridge: one chunk frame.
pub(crate) const BRIDGE_FRAME_LIMIT: usize = arcen_protocol::CLIPBOARD_HEADER_SIZE + CHUNK_BYTES;

/// How the clipboard reaches a desktop session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardBridgeKind {
    /// A per-attachment `session-agent --clipboard-agent` child on the
    /// dedicated Xorg display.
    X11Child,
    /// The headless Wayland session agent's `arcen-clipboard.sock`.
    WaylandSession,
}

impl ClipboardBridgeKind {
    /// Stable token for logs.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::X11Child => "x11_child",
            Self::WaylandSession => "wayland_session",
        }
    }
}

/// The shared eligibility facts of this host's session.
///
/// Dedicated Xorg: authenticated through PAM on the configured session
/// display. Headless Wayland: authenticated through PAM, its own logind
/// session, and an agent that proved its compositor clipboard at start.
fn host_facts(cfg: &Config, session: Option<&SessionMetadata>) -> ClipboardHostFacts {
    let authenticated = cfg.auth_mode == AuthMode::Pam && session.is_some();
    let (dedicated_desktop, native_bridge) = match session {
        Some(session) if session.session_type == "x11" => {
            (session.display == cfg.session_display, true)
        }
        Some(session) if session.session_type == "wayland" => (
            session.wayland.is_some(),
            session
                .wayland
                .as_ref()
                .is_some_and(|ready| ready.clipboard.is_some()),
        ),
        _ => (false, false),
    };
    ClipboardHostFacts {
        authenticated,
        dedicated_desktop,
        unprivileged_user: session.is_some_and(|session| session.uid != 0),
        native_bridge,
    }
}

/// Whether this session may carry the clipboard, and through which bridge.
///
/// # Errors
///
/// The shared [`ClipboardIneligible`] reason.
pub fn eligibility(
    cfg: &Config,
    session: Option<&SessionMetadata>,
) -> Result<ClipboardBridgeKind, ClipboardIneligible> {
    clipboard_eligibility(host_facts(cfg, session))?;
    Ok(match session.map(|session| session.session_type.as_str()) {
        Some("wayland") => ClipboardBridgeKind::WaylandSession,
        _ => ClipboardBridgeKind::X11Child,
    })
}

/// Advertises the exact host policy only for an eligible session, and
/// disabled otherwise (no-auth, shared display, root, or a Wayland desktop
/// whose compositor clipboard was not proven).
#[must_use]
pub fn advertised_policy(cfg: &Config, session: Option<&SessionMetadata>) -> ClipboardPolicyMsg {
    let mut message = policy_message(cfg.clipboard_policy);
    if eligibility(cfg, session).is_err() {
        message.direction = ClipboardDirectionMsg::Disabled;
    }
    message
}

/// Renders a policy as the wire message that advertises it.
///
/// The translation itself lives in `arcen_media`, so every Pier advertises the
/// same policy for the same configuration.
pub use arcen_media::clipboard::policy_message;

/// The host policy narrowed by the client's hello, or `None` when this
/// session carries no clipboard.
#[must_use]
pub fn negotiate(
    policy: ClipboardPolicy,
    eligible: bool,
    hello: &ClientHelloMsg,
) -> Option<ClipboardNegotiation> {
    ClipboardNegotiation::resolve(policy, eligible, ClipboardRequest::from_hello(hello))
}

pub struct ClipboardItem {
    pub sequence: u64,
    pub kind: ClipboardContentKind,
    pub bytes: Vec<u8>,
    pub truncated: bool,
}

impl ClipboardItem {
    #[must_use]
    pub fn new(
        sequence: u64,
        kind: ClipboardContentKind,
        bytes: Vec<u8>,
        truncated: bool,
    ) -> Option<Self> {
        if sequence == 0
            || bytes.is_empty()
            || bytes.len() > HARD_MAX_CLIPBOARD_BYTES
            || (truncated && kind != ClipboardContentKind::TextUtf8)
        {
            return None;
        }
        Some(Self {
            sequence,
            kind,
            bytes,
            truncated,
        })
    }

    fn scrub(&mut self) {
        self.bytes.zeroize();
        self.bytes.clear();
    }

    /// Converts this item into the shared transfer that knows how to send it.
    ///
    /// Takes the bytes rather than copying them so the payload exists once:
    /// the shared transfer scrubs it on drop exactly as this item does, and a
    /// second copy would be one this host could not scrub.
    ///
    /// Returns `None` only for a payload the wire cannot describe, which
    /// [`Self::new`] has already ruled out.
    fn into_transfer(mut self) -> Option<ClipboardTransfer> {
        let bytes = std::mem::take(&mut self.bytes);
        let truncated = self.truncated;
        ClipboardTransfer::new(self.sequence, self.kind, bytes)
            .ok()
            .map(|transfer| transfer.with_truncated(truncated))
    }
}

impl Debug for ClipboardItem {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ClipboardItem")
            .field("sequence", &self.sequence)
            .field("kind", &self.kind)
            .field("bytes", &self.bytes.len())
            .field("truncated", &self.truncated)
            .finish()
    }
}

impl Drop for ClipboardItem {
    fn drop(&mut self) {
        self.bytes.zeroize();
    }
}

/// One clipboard transfer in flight, as a cursor over its wire messages.
///
/// The ordering, the chunk boundary and the offset arithmetic live in
/// [`ClipboardCursor`], beside the reassembly that has to accept them. This
/// host only decides how the two message kinds reach its socket.
type Transfer = ClipboardCursor;

/// Maps one shared clipboard message onto this host's transport.
fn wire_message(message: ClipboardWireMessage) -> Result<Message, String> {
    match message {
        ClipboardWireMessage::Offer(offer) => serde_json::to_string(&offer)
            .map(|text| Message::Text(text.into()))
            .map_err(|error| format!("serialize clipboard offer: {error}")),
        ClipboardWireMessage::Chunk(frame) => Ok(Message::Binary(frame.into())),
    }
}

#[derive(Default)]
struct WriterState {
    latest: u64,
    active: Option<Transfer>,
    pending: Option<ClipboardItem>,
    closed: bool,
}

impl Drop for WriterState {
    fn drop(&mut self) {
        self.active = None;
        if let Some(mut pending) = self.pending.take() {
            pending.scrub();
        }
    }
}

pub struct ClipboardWriterQueue {
    state: Mutex<WriterState>,
    ready: Notify,
}

impl ClipboardWriterQueue {
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(WriterState::default()),
            ready: Notify::new(),
        })
    }

    pub fn enqueue(&self, mut item: ClipboardItem) -> bool {
        let mut state = self.lock();
        if state.closed || item.sequence <= state.latest {
            item.scrub();
            return false;
        }
        state.latest = item.sequence;
        if let Some(mut pending) = state.pending.take() {
            pending.scrub();
        }
        state.pending = Some(item);
        drop(state);
        self.ready.notify_one();
        true
    }

    pub async fn pop(&self) -> Result<Option<Message>, String> {
        Ok(self.pop_with_sequence().await?.map(|(message, _)| message))
    }

    /// Like [`Self::pop`], with the sequence of the transfer the message
    /// belongs to, so a writer that cannot deliver it can abandon exactly
    /// that transfer.
    pub async fn pop_with_sequence(&self) -> Result<Option<(Message, u64)>, String> {
        loop {
            {
                let mut state = self.lock();
                if state
                    .pending
                    .as_ref()
                    .zip(state.active.as_ref())
                    .is_some_and(|(pending, active)| {
                        pending.sequence > active.transfer().sequence()
                    })
                {
                    state.active = None;
                }
                if state.active.is_none() {
                    state.active = state
                        .pending
                        .take()
                        .and_then(ClipboardItem::into_transfer)
                        .map(ClipboardTransfer::into_cursor);
                }
                if let Some(active) = state.active.as_mut() {
                    let sequence = active.transfer().sequence();
                    let next = active
                        .next_message()
                        .transpose()
                        .map_err(|error| format!("frame clipboard transfer: {error}"))?;
                    if let Some(message) = next {
                        let message = wire_message(message)?;
                        if active.finished() {
                            state.active = None;
                        }
                        return Ok(Some((message, sequence)));
                    }
                    state.active = None;
                    continue;
                }
                if state.closed {
                    return Ok(None);
                }
            }
            self.ready.notified().await;
        }
    }

    /// Drops the rest of transfer `sequence` when it is still the active
    /// one. A newer pending item is kept.
    pub fn abandon(&self, sequence: u64) -> bool {
        let mut state = self.lock();
        if state
            .active
            .as_ref()
            .is_some_and(|active| active.transfer().sequence() == sequence)
        {
            state.active = None;
            return true;
        }
        false
    }

    pub fn close(&self) {
        let mut state = self.lock();
        state.closed = true;
        state.active = None;
        if let Some(mut pending) = state.pending.take() {
            pending.scrub();
        }
        drop(state);
        self.ready.notify_one();
    }

    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .closed
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, WriterState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[derive(Default)]
struct MailboxState {
    latest: u64,
    item: Option<ClipboardItem>,
    closed: bool,
}

pub struct ClipboardMailbox {
    state: Mutex<MailboxState>,
    ready: Notify,
}

impl ClipboardMailbox {
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(MailboxState::default()),
            ready: Notify::new(),
        })
    }

    pub fn replace(&self, mut item: ClipboardItem) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.closed || item.sequence <= state.latest {
            item.scrub();
            return false;
        }
        state.latest = item.sequence;
        if let Some(mut old) = state.item.take() {
            old.scrub();
        }
        state.item = Some(item);
        drop(state);
        self.ready.notify_one();
        true
    }

    pub async fn take(&self) -> Option<ClipboardItem> {
        loop {
            {
                let mut state = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some(item) = state.item.take() {
                    return Some(item);
                }
                if state.closed {
                    return None;
                }
            }
            self.ready.notified().await;
        }
    }

    pub fn try_take(&self) -> Option<ClipboardItem> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .item
            .take()
    }

    pub fn close(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.closed = true;
        if let Some(mut item) = state.item.take() {
            item.scrub();
        }
        drop(state);
        self.ready.notify_one();
    }

    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .closed
    }
}

/// Pure one-direction ICCCM INCR state with a progress deadline.
#[derive(Debug)]
pub struct IncrTransfer {
    bytes: Vec<u8>,
    offset: usize,
    last_progress: Instant,
}

impl IncrTransfer {
    #[must_use]
    pub fn new(bytes: Vec<u8>, now: Instant) -> Option<Self> {
        (!bytes.is_empty() && bytes.len() <= HARD_MAX_CLIPBOARD_BYTES).then_some(Self {
            bytes,
            offset: 0,
            last_progress: now,
        })
    }

    pub fn next_chunk(&mut self, now: Instant) -> Option<&[u8]> {
        if self.offset == self.bytes.len() {
            return None;
        }
        let start = self.offset;
        let end = start
            .checked_add(CHUNK_BYTES)
            .unwrap_or(self.bytes.len())
            .min(self.bytes.len());
        self.offset = end;
        self.last_progress = now;
        Some(&self.bytes[start..end])
    }

    #[must_use]
    pub fn expired(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.last_progress) >= Duration::from_secs(5)
    }
}

impl Drop for IncrTransfer {
    fn drop(&mut self) {
        self.bytes.zeroize();
    }
}

/// How long one bridge message (an offer or one chunk) may wait for the
/// helper to take it before the Pier stops waiting.
pub const BRIDGE_SEND_TIMEOUT: Duration = Duration::from_secs(5);

/// Why the Pier's side of a clipboard bridge stopped carrying an item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipboardBridgeFailure {
    /// The helper took nothing for [`BRIDGE_SEND_TIMEOUT`]; the item was
    /// abandoned.
    Stalled { sequence: u64 },
    /// The bridge socket failed.
    Closed(String),
}

/// The Pier's end of one clipboard bridge.
///
/// Reading and writing are separate. Client items go to a writer task
/// through a latest-wins mailbox, so the attachment's main loop never waits
/// on the helper. The helper may at the same moment be writing a host item
/// that the main loop must keep reading. With both ends only writing, the
/// local socket buffer fills and neither side reads again.
pub struct ClipboardBridgeLink {
    stream: futures_util::stream::SplitStream<WebSocketStream<ClipboardAgentIo>>,
    outbound: Arc<ClipboardMailbox>,
    failures: tokio::sync::mpsc::Receiver<ClipboardBridgeFailure>,
    writer: JoinHandle<()>,
    kind: ClipboardBridgeKind,
}

impl ClipboardBridgeLink {
    /// Splits `websocket` and starts its writer.
    #[must_use]
    pub fn new(websocket: WebSocketStream<ClipboardAgentIo>, kind: ClipboardBridgeKind) -> Self {
        Self::with_timeout(websocket, kind, BRIDGE_SEND_TIMEOUT)
    }

    /// As [`Self::new`] with an explicit per-message timeout.
    #[must_use]
    pub fn with_timeout(
        websocket: WebSocketStream<ClipboardAgentIo>,
        kind: ClipboardBridgeKind,
        timeout: Duration,
    ) -> Self {
        use futures_util::StreamExt;
        let (sink, stream) = websocket.split();
        let outbound = ClipboardMailbox::new();
        let (failed, failures) = tokio::sync::mpsc::channel(4);
        let writer = tokio::spawn(bridge_writer(
            sink,
            Arc::clone(&outbound),
            failed,
            kind,
            timeout,
        ));
        Self {
            stream,
            outbound,
            failures,
            writer,
            kind,
        }
    }

    /// Which bridge this is.
    #[must_use]
    pub const fn kind(&self) -> ClipboardBridgeKind {
        self.kind
    }

    /// The next message from the helper.
    pub async fn next(&mut self) -> Option<Result<Message, tokio_tungstenite::tungstenite::Error>> {
        use futures_util::StreamExt;
        self.stream.next().await
    }

    /// Hands a client item to the writer, replacing one it has not started.
    /// Never waits.
    pub fn offer(&self, item: ClipboardItem) -> bool {
        self.outbound.replace(item)
    }

    /// The next writer failure. Pending forever while the writer is healthy.
    pub async fn failure(&mut self) -> Option<ClipboardBridgeFailure> {
        self.failures.recv().await
    }

    /// Whichever comes first: a message from the helper, or a writer
    /// failure.
    pub async fn next_event(&mut self) -> ClipboardBridgeEvent {
        use futures_util::StreamExt;
        tokio::select! {
            message = self.stream.next() => ClipboardBridgeEvent::Message(message),
            Some(failure) = self.failures.recv() => ClipboardBridgeEvent::Failure(failure),
        }
    }
}

/// What the Pier's side of a clipboard bridge has to handle next.
#[derive(Debug)]
pub enum ClipboardBridgeEvent {
    /// A message from the helper; `None` when its stream ended.
    Message(Option<Result<Message, tokio_tungstenite::tungstenite::Error>>),
    /// The writer stopped carrying an item.
    Failure(ClipboardBridgeFailure),
}

impl Drop for ClipboardBridgeLink {
    fn drop(&mut self) {
        self.outbound.close();
        self.writer.abort();
    }
}

async fn bridge_writer(
    mut sink: futures_util::stream::SplitSink<WebSocketStream<ClipboardAgentIo>, Message>,
    outbound: Arc<ClipboardMailbox>,
    failed: tokio::sync::mpsc::Sender<ClipboardBridgeFailure>,
    kind: ClipboardBridgeKind,
    timeout: Duration,
) {
    while let Some(item) = outbound.take().await {
        let sequence = item.sequence;
        let Some(transfer) = item.into_transfer() else {
            continue;
        };
        let mut cursor = transfer.into_cursor();
        let outcome = loop {
            let message = match cursor.next_message() {
                None => break Ok(()),
                Some(Err(error)) => break Err(ClipboardBridgeFailure::Closed(error.to_string())),
                Some(Ok(message)) => match wire_message(message) {
                    Ok(message) => message,
                    Err(error) => break Err(ClipboardBridgeFailure::Closed(error)),
                },
            };
            match tokio::time::timeout(timeout, sink.send(message)).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => break Err(ClipboardBridgeFailure::Closed(error.to_string())),
                Err(_) => break Err(ClipboardBridgeFailure::Stalled { sequence }),
            }
        };
        drop(cursor);
        if let Err(failure) = outcome {
            let ends = matches!(failure, ClipboardBridgeFailure::Closed(_))
                || kind == ClipboardBridgeKind::WaylandSession;
            let _ = failed.send(failure).await;
            if ends {
                return;
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClipboardAgentReady {
    pub ready: bool,
    pub pid: u32,
    pub uid: u32,
    pub username: String,
    pub display: String,
    pub xfixes_major: u32,
    pub xfixes_minor: u32,
}

/// The first message on every clipboard bridge: the policy the helper must
/// enforce on its side too, and, for a bridge that reads the desktop
/// clipboard on its own, what the client asked for in each direction.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct AgentPolicy {
    #[serde(rename = "type")]
    pub(crate) msg_type: String,
    pub(crate) policy: ClipboardPolicyMsg,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) directions: Option<AgentDirections>,
}

/// Which kinds may cross in which direction after negotiation.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct AgentDirections {
    pub(crate) text_c2h: bool,
    pub(crate) text_h2c: bool,
    pub(crate) image_c2h: bool,
    pub(crate) image_h2c: bool,
}

impl AgentPolicy {
    pub(crate) fn new(policy: ClipboardPolicy) -> Self {
        Self {
            msg_type: CLIPBOARD_POLICY_TYPE.to_string(),
            policy: policy_message(policy),
            directions: None,
        }
    }

    /// The whole negotiation, so the helper never reads a kind the client
    /// did not ask for.
    pub(crate) fn negotiated(negotiation: ClipboardNegotiation) -> Self {
        use arcen_media::clipboard::{ClipboardFlow, ClipboardKind};
        let allows = |flow, kind| negotiation.allows(flow, kind);
        Self {
            directions: Some(AgentDirections {
                text_c2h: allows(ClipboardFlow::ClientToHost, ClipboardKind::TextUtf8),
                text_h2c: allows(ClipboardFlow::HostToClient, ClipboardKind::TextUtf8),
                image_c2h: allows(ClipboardFlow::ClientToHost, ClipboardKind::ImagePng),
                image_h2c: allows(ClipboardFlow::HostToClient, ClipboardKind::ImagePng),
            }),
            ..Self::new(negotiation.policy())
        }
    }

    /// The policy, when this is a v1 policy message.
    pub(crate) fn into_policy(self) -> Option<ClipboardPolicy> {
        (self.msg_type == CLIPBOARD_POLICY_TYPE)
            .then(|| arcen_media::clipboard::policy_from_message(self.policy).ok())
            .flatten()
    }

    /// The negotiation this message carries. Without directions (an older
    /// parent) every kind the policy allows is assumed requested.
    pub(crate) fn into_negotiation(self) -> Option<ClipboardNegotiation> {
        use arcen_media::clipboard::ClipboardDirections;
        let directions = self.directions;
        let policy = self.into_policy()?;
        let request = directions.map_or(
            ClipboardRequest {
                protocol_version: arcen_protocol::messages::CLIPBOARD_PROTOCOL_VERSION,
                text: ClipboardDirections::both(),
                image: ClipboardDirections::both(),
            },
            |directions| ClipboardRequest {
                protocol_version: arcen_protocol::messages::CLIPBOARD_PROTOCOL_VERSION,
                text: ClipboardDirections {
                    client_to_host: directions.text_c2h,
                    host_to_client: directions.text_h2c,
                },
                image: ClipboardDirections {
                    client_to_host: directions.image_c2h,
                    host_to_client: directions.image_h2c,
                },
            },
        );
        ClipboardNegotiation::resolve(policy, true, request)
    }
}

/// The line a Wayland session's clipboard bridge writes before its
/// WebSocket starts: who is serving, and through what.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClipboardBridgeReady {
    pub ready: bool,
    pub pid: u32,
    pub uid: u32,
    pub username: String,
    /// `mutter_remote_desktop`.
    pub backend: String,
}

/// The byte stream under one clipboard bridge's WebSocket.
pub enum ClipboardAgentIo {
    /// A dedicated-Xorg `--clipboard-agent` child's stdio.
    Child {
        stdin: ChildStdin,
        stdout: ChildStdout,
    },
    /// A Wayland session agent's `arcen-clipboard.sock`.
    Session(tokio::net::UnixStream),
}

impl AsyncRead for ClipboardAgentIo {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Child { stdout, .. } => Pin::new(stdout).poll_read(cx, buffer),
            Self::Session(stream) => Pin::new(stream).poll_read(cx, buffer),
        }
    }
}

impl AsyncWrite for ClipboardAgentIo {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        match self.get_mut() {
            Self::Child { stdin, .. } => Pin::new(stdin).poll_write(cx, buffer),
            Self::Session(stream) => Pin::new(stream).poll_write(cx, buffer),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), std::io::Error>> {
        match self.get_mut() {
            Self::Child { stdin, .. } => Pin::new(stdin).poll_flush(cx),
            Self::Session(stream) => Pin::new(stream).poll_flush(cx),
        }
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        match self.get_mut() {
            Self::Child { stdin, .. } => Pin::new(stdin).poll_shutdown(cx),
            Self::Session(stream) => Pin::new(stream).poll_shutdown(cx),
        }
    }
}

pub(crate) fn bridge_websocket_config() -> tokio_tungstenite::tungstenite::protocol::WebSocketConfig
{
    let mut config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default();
    config.max_message_size = Some(BRIDGE_FRAME_LIMIT);
    config.max_frame_size = Some(BRIDGE_FRAME_LIMIT);
    config
}

async fn send_policy(
    websocket: &mut WebSocketStream<ClipboardAgentIo>,
    policy: AgentPolicy,
) -> Result<(), String> {
    websocket
        .send(Message::Text(
            serde_json::to_string(&policy)
                .map_err(|error| format!("serialize clipboard bridge policy: {error}"))?
                .into(),
        ))
        .await
        .map_err(|error| format!("send clipboard bridge policy: {error}"))
}

/// Connects to a headless Wayland session's clipboard bridge after proving
/// it is the session's: the socket is private to the session user inside the
/// session's runtime directory, the peer is the session's agent, and its
/// ready line names the same user and process.
///
/// # Errors
///
/// A bounded reason when the bridge is absent or not proven.
#[cfg(target_os = "linux")]
pub async fn connect_wayland_clipboard(
    session: &SessionMetadata,
    negotiation: ClipboardNegotiation,
) -> Result<WebSocketStream<ClipboardAgentIo>, String> {
    use crate::session::wayland::{is_private_runtime_dir, socket_proof};
    let ready = session
        .wayland
        .as_ref()
        .ok_or_else(|| "not a Wayland session".to_string())?;
    let bridge = ready
        .clipboard
        .as_ref()
        .ok_or_else(|| "the session agent proved no compositor clipboard".to_string())?;
    if !ready.is_consistent()
        || !is_private_runtime_dir(&ready.runtime_dir, session.uid, &session.session_id)
    {
        return Err("clipboard socket is not in the session's runtime directory".to_string());
    }
    tokio::time::timeout(IPC_READY_TIMEOUT, async {
        socket_proof::verify_private_socket(&ready.runtime_dir, &bridge.socket, session.uid)
            .map_err(|error| error.to_string())?;
        let stream = tokio::net::UnixStream::connect(&bridge.socket)
            .await
            .map_err(|error| format!("connect clipboard socket: {error}"))?;
        socket_proof::verify_peer(&stream, session.uid, Some(session.agent_pid))
            .map_err(|error| error.to_string())?;
        let mut io = ClipboardAgentIo::Session(stream);
        let line = read_ready_line(&mut io).await?;
        let hello: ClipboardBridgeReady = serde_json::from_slice(&line)
            .map_err(|_| "clipboard bridge READY is invalid".to_string())?;
        if !hello.ready
            || hello.pid != session.agent_pid
            || hello.uid != session.uid
            || hello.username != session.username
        {
            return Err("clipboard bridge READY identity mismatch".to_string());
        }
        let mut websocket =
            WebSocketStream::from_raw_socket(io, Role::Client, Some(bridge_websocket_config()))
                .await;
        send_policy(&mut websocket, AgentPolicy::negotiated(negotiation)).await?;
        Ok(websocket)
    })
    .await
    .map_err(|_| "clipboard bridge readiness timed out".to_string())?
}

pub struct ClipboardAgentProcess {
    child: Child,
    stderr: JoinHandle<()>,
}

impl ClipboardAgentProcess {
    pub async fn shutdown(mut self) {
        terminate_child(&mut self.child).await;
        finish_stderr(&mut self.stderr).await;
    }
}

impl Drop for ClipboardAgentProcess {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
        self.stderr.abort();
    }
}

pub async fn spawn_clipboard_agent(
    binary: &Path,
    execution: &UserExecution,
    negotiation: ClipboardNegotiation,
    session_log_id: &CorrelationId,
) -> Result<(ClipboardAgentProcess, WebSocketStream<ClipboardAgentIo>), String> {
    let mut command = crate::command_for_helper(binary, "session-agent");
    command
        .arg("--clipboard-agent")
        .arg("--session-log-id")
        .arg(session_log_id.as_str())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        command.as_std_mut().process_group(0);
    }
    execution
        .configure(&mut command)
        .map_err(|error| format!("clipboard agent identity: {error}"))?;
    let mut child = command
        .spawn()
        .map_err(|error| format!("spawn clipboard agent: {error}"))?;
    let pid = child
        .id()
        .ok_or_else(|| "clipboard agent exited before readiness".to_string())?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| "clipboard agent stdin unavailable".to_string())?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| "clipboard agent stdout unavailable".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "clipboard agent stderr unavailable".to_string())?;
    let stderr = tokio::spawn(read_stderr(stderr));
    let line = tokio::time::timeout(IPC_READY_TIMEOUT, read_ready_line(&mut stdout))
        .await
        .map_err(|_| "clipboard agent readiness timed out".to_string())??;
    let ready: ClipboardAgentReady = serde_json::from_slice(&line)
        .map_err(|_| "clipboard agent READY is invalid".to_string())?;
    if !ready.ready
        || ready.pid != pid
        || ready.uid != execution.identity.uid
        || ready.username != execution.identity.username
        || ready.display != execution.environment.display()
        || ready.xfixes_major < 5
    {
        return Err("clipboard agent READY identity/XFixes mismatch".to_string());
    }
    let io = ClipboardAgentIo::Child { stdin, stdout };
    let mut websocket =
        WebSocketStream::from_raw_socket(io, Role::Client, Some(bridge_websocket_config())).await;
    send_policy(&mut websocket, AgentPolicy::negotiated(negotiation)).await?;
    Ok((ClipboardAgentProcess { child, stderr }, websocket))
}

async fn read_ready_line<R: AsyncRead + Unpin>(stdout: &mut R) -> Result<Vec<u8>, String> {
    use tokio::io::AsyncReadExt;
    let mut line = Vec::new();
    while line.len() < IPC_READY_LIMIT {
        let byte = stdout
            .read_u8()
            .await
            .map_err(|error| format!("read clipboard READY: {error}"))?;
        if byte == b'\n' {
            return Ok(line);
        }
        line.push(byte);
    }
    Err("clipboard READY exceeds bound".to_string())
}

async fn read_stderr(stderr: ChildStderr) {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        tracing::info!(
            target: crate::logging::target::SESSION,
            clipboard_agent = true,
            "{line}"
        );
    }
}

async fn finish_stderr(task: &mut JoinHandle<()>) {
    if tokio::time::timeout(Duration::from_secs(1), &mut *task)
        .await
        .is_err()
    {
        task.abort();
        let _ = task.await;
    }
}

async fn terminate_child(child: &mut Child) {
    #[cfg(target_os = "linux")]
    if let Some(pid) = child.id() {
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(i32::try_from(pid).unwrap_or(i32::MAX)),
            nix::sys::signal::Signal::SIGTERM,
        );
    }
    if tokio::time::timeout(IPC_SHUTDOWN_TIMEOUT, child.wait())
        .await
        .is_err()
    {
        #[cfg(target_os = "linux")]
        if let Some(pid) = child.id() {
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(i32::try_from(pid).unwrap_or(i32::MAX)),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
        let _ = child.kill().await;
    }
}

#[cfg(target_os = "linux")]
pub async fn run_clipboard_child() -> Result<(), String> {
    native::run().await
}

#[cfg(target_os = "linux")]
fn effective_uid() -> u32 {
    nix::unistd::Uid::effective().as_raw()
}

#[cfg(not(target_os = "linux"))]
pub async fn run_clipboard_child() -> Result<(), String> {
    Err("clipboard agent is available only on Linux".to_string())
}

#[cfg(target_os = "linux")]
mod native;

#[cfg(test)]
mod tests {
    use super::*;
    use arcen_media::clipboard::{ClipboardContent, ClipboardDirection};
    use arcen_protocol::messages::CLIPBOARD_PROTOCOL_VERSION;

    fn policy() -> ClipboardPolicy {
        ClipboardPolicy::new(
            ClipboardDirection::Both,
            ClipboardContent::All,
            2 * 1024 * 1024,
        )
        .unwrap()
    }

    #[test]
    fn no_auth_and_shared_display_advertise_disabled() {
        let cfg = Config::default();
        assert_eq!(
            advertised_policy(&cfg, None).direction,
            ClipboardDirectionMsg::Disabled
        );
    }

    #[test]
    fn negotiation_requires_exact_v1() {
        let mut hello = ClientHelloMsg {
            clipboard_protocol_version: CLIPBOARD_PROTOCOL_VERSION,
            clipboard_text_c2s: true,
            ..ClientHelloMsg::default()
        };
        assert!(negotiate(policy(), true, &hello).is_some());
        hello.clipboard_protocol_version = 0;
        assert!(negotiate(policy(), true, &hello).is_none());
        hello.clipboard_protocol_version = CLIPBOARD_PROTOCOL_VERSION;
        assert!(negotiate(policy(), false, &hello).is_none());
    }

    fn session(session_type: &str, display: &str, uid: u32) -> SessionMetadata {
        SessionMetadata {
            username: "user".into(),
            uid,
            session_id: "7".into(),
            session_type: session_type.into(),
            desktop: "gnome".into(),
            display: display.into(),
            agent_pid: 42,
            generation: 1,
            reconnected: false,
            timezone: None,
            multi_monitor_plan: None,
            multi_monitor_carrier: None,
            audio: None,
            wayland: None,
            requested_outputs: None,
            x11_desktop_depth: None,
        }
    }

    fn wayland_session(clipboard: bool) -> SessionMetadata {
        use crate::session::wayland::{
            WaylandAgentReady, WaylandClipboardReady, CLIPBOARD_SOCKET_NAME, INPUT_SOCKET_NAME,
            WAYLAND_DISPLAY_NAME,
        };
        let runtime_dir = std::path::PathBuf::from("/run/user/1000/arcen-wayland-7");
        let mut metadata = session("wayland", "", 1000);
        metadata.wayland = Some(WaylandAgentReady {
            compositor: "gnome-shell".into(),
            input_socket: runtime_dir.join(INPUT_SOCKET_NAME),
            wayland_display: WAYLAND_DISPLAY_NAME.into(),
            outputs: Vec::new(),
            streams: Vec::new(),
            input_device_types: 7,
            generation: 1,
            compositor_render_node: None,
            compositor_gpu_pci: None,
            compositor_gpu_vendor: None,
            output_mode: arcen_session::wayland_outputs::VirtualOutputMode::StartupMonitors,
            output_socket: None,
            clipboard: clipboard.then(|| WaylandClipboardReady {
                socket: runtime_dir.join(CLIPBOARD_SOCKET_NAME),
                interface: "org.gnome.Mutter.RemoteDesktop.Session".into(),
                dedicated_session: true,
            }),
            x11: None,
            cursor_mode: crate::session::wayland::StreamCursorMode::Embedded,
            runtime_dir,
        });
        metadata
    }

    #[test]
    fn x11_eligibility_is_unchanged() {
        let cfg = Config {
            auth_mode: AuthMode::Pam,
            ..Config::default()
        };
        let own = session("x11", &cfg.session_display, 1000);
        assert_eq!(
            eligibility(&cfg, Some(&own)),
            Ok(ClipboardBridgeKind::X11Child)
        );
        assert_eq!(
            advertised_policy(&cfg, Some(&own)),
            policy_message(cfg.clipboard_policy)
        );
        let other_display = session("x11", ":4242", 1000);
        assert_eq!(
            eligibility(&cfg, Some(&other_display)),
            Err(ClipboardIneligible::SharedDesktop)
        );
        let root = session("x11", &cfg.session_display, 0);
        assert_eq!(
            eligibility(&cfg, Some(&root)),
            Err(ClipboardIneligible::PrivilegedUser)
        );
        let no_auth = Config {
            auth_mode: AuthMode::None,
            ..Config::default()
        };
        assert_eq!(
            advertised_policy(&no_auth, Some(&own)).direction,
            ClipboardDirectionMsg::Disabled
        );
    }

    #[test]
    fn a_wayland_session_carries_the_clipboard_only_when_its_agent_proved_it() {
        let cfg = Config {
            auth_mode: AuthMode::Pam,
            ..Config::default()
        };
        assert_eq!(
            eligibility(&cfg, Some(&wayland_session(true))),
            Ok(ClipboardBridgeKind::WaylandSession)
        );
        assert_eq!(
            eligibility(&cfg, Some(&wayland_session(false))),
            Err(ClipboardIneligible::NoNativeBridge)
        );
        assert_eq!(
            advertised_policy(&cfg, Some(&wayland_session(false))).direction,
            ClipboardDirectionMsg::Disabled
        );
        assert_eq!(
            eligibility(&cfg, Some(&session("wayland", "", 1000))),
            Err(ClipboardIneligible::SharedDesktop)
        );
    }

    #[tokio::test]
    async fn a_helper_that_stops_reading_never_blocks_the_pier() {
        let (pier, helper) = tokio::net::UnixStream::pair().unwrap();
        let websocket = WebSocketStream::from_raw_socket(
            ClipboardAgentIo::Session(pier),
            Role::Client,
            Some(bridge_websocket_config()),
        )
        .await;
        let mut link = ClipboardBridgeLink::with_timeout(
            websocket,
            ClipboardBridgeKind::X11Child,
            Duration::from_millis(200),
        );
        // The helper end is never read: its buffer fills.
        let started = Instant::now();
        assert!(link.offer(
            ClipboardItem::new(
                7,
                ClipboardContentKind::TextUtf8,
                vec![b'x'; 8 * CHUNK_BYTES],
                false
            )
            .unwrap()
        ));
        assert!(
            started.elapsed() < Duration::from_millis(50),
            "offer never waits"
        );
        let failure = tokio::time::timeout(Duration::from_secs(5), link.failure())
            .await
            .expect("the stall is reported");
        assert_eq!(
            failure,
            Some(ClipboardBridgeFailure::Stalled { sequence: 7 })
        );
        drop(helper);
    }

    #[tokio::test]
    async fn an_undeliverable_transfer_is_abandoned_but_a_newer_one_is_kept() {
        let queue = ClipboardWriterQueue::new();
        assert!(queue.enqueue(
            ClipboardItem::new(
                1,
                ClipboardContentKind::TextUtf8,
                vec![b'a'; 2 * CHUNK_BYTES],
                false
            )
            .unwrap()
        ));
        let (_, sequence) = queue.pop_with_sequence().await.unwrap().unwrap();
        assert_eq!(sequence, 1);
        let (_, sequence) = queue.pop_with_sequence().await.unwrap().unwrap();
        assert_eq!(sequence, 1);
        assert!(queue.abandon(1));
        assert!(!queue.abandon(1));
        assert!(queue.enqueue(
            ClipboardItem::new(2, ClipboardContentKind::TextUtf8, b"b".to_vec(), false).unwrap()
        ));
        let (Message::Text(_), 2) = queue.pop_with_sequence().await.unwrap().unwrap() else {
            panic!("the newer transfer starts with its offer");
        };
    }

    #[test]
    fn incr_chunks_and_deadline_are_bounded() {
        let start = Instant::now();
        let mut incr = IncrTransfer::new(vec![7; CHUNK_BYTES + 1], start).unwrap();
        assert_eq!(incr.next_chunk(start).unwrap().len(), CHUNK_BYTES);
        assert_eq!(incr.next_chunk(start).unwrap().len(), 1);
        assert!(incr.next_chunk(start).is_none());
        assert!(!incr.expired(start + Duration::from_millis(4_999)));
        assert!(incr.expired(start + Duration::from_secs(5)));
    }

    #[tokio::test]
    async fn writer_is_latest_bounded_and_schedules_one_chunk() {
        let queue = ClipboardWriterQueue::new();
        assert!(queue.enqueue(
            ClipboardItem::new(
                1,
                ClipboardContentKind::TextUtf8,
                vec![b'a'; CHUNK_BYTES + 1],
                false,
            )
            .unwrap()
        ));
        assert!(matches!(queue.pop().await.unwrap(), Some(Message::Text(_))));
        assert!(queue.enqueue(
            ClipboardItem::new(2, ClipboardContentKind::TextUtf8, b"new".to_vec(), false).unwrap()
        ));
        assert!(matches!(queue.pop().await.unwrap(), Some(Message::Text(_))));
        let Some(Message::Binary(frame)) = queue.pop().await.unwrap() else {
            panic!("new payload chunk");
        };
        assert_eq!(frame.len(), arcen_protocol::CLIPBOARD_HEADER_SIZE + 3);
    }
}
