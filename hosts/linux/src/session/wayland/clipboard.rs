//! The clipboard of a headless Wayland session, carried for the Pier.
//!
//! Mutter exposes the selection on a `RemoteDesktop` session:
//! `EnableClipboard`, `SetSelection`, `SelectionRead`, `SelectionWrite`,
//! `SelectionWriteDone`, and the `SelectionOwnerChanged` and
//! `SelectionTransfer` signals. The API is the same text from GNOME 40
//! through GNOME 51. The agent creates one `RemoteDesktop` session for the
//! clipboard alone and never starts it. Its lifetime is the desktop's, so
//! reconfiguring or re-recording outputs never touches the clipboard, and a
//! resize loses nothing. Mutter serves the clipboard on a session that has not
//! been started; this was proven on GNOME 49 and 40 before relying on it.
//!
//! The Pier connects to `arcen-clipboard.sock` and proves it the same way as
//! the input socket. It then speaks the same bridge protocol as the
//! dedicated-Xorg clipboard child:
//!
//! - a ready line;
//! - a WebSocket whose first message is the negotiated policy;
//! - `clipboard_data` offers and binary chunks in both directions.
//!
//! All clipboard decisions live in the shared
//! [`arcen_media::clipboard::bridge::SelectionBridge`]. What to read, what to
//! ignore, what to offer and what to serve are decided there. This module
//! only moves bytes between Mutter's descriptors and that state machine.
//!
//! The clipboard is enabled once, when the agent proves it, and is never
//! disabled. On GNOME 40 through 50, `DisableClipboard` leaves the session
//! marked as enabled while it disconnects the owner signal, so the clipboard
//! of that session could never be enabled again (fixed by Mutter MR !4767,
//! GNOME 51). Signals are subscribed only while a Pier is connected, and the
//! selection is read only then. When the connection ends, the bridge releases
//! the selection it took for the client. An owner that existed before a
//! connection is therefore never sent: only changes the connection sees are
//! carried, as on dedicated Xorg. One connection is served at a time, and a
//! newer one supersedes the older one.

use std::collections::HashMap;
use std::future::Future;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::pin::Pin;
use std::time::Duration;

use arcen_media::clipboard::bridge::{
    OwnerDecision, ReadOutcome, SelectionBridge, TransferDecision, SELECTION_PROGRESS_TIMEOUT,
};
use arcen_media::clipboard::ClipboardNegotiation;
use arcen_protocol::clipboard::ClipboardReassembler;
use arcen_protocol::decode_clipboard_chunk;
use arcen_protocol::messages::{ClipboardDataMsg, CLIPBOARD_DATA};
use futures_util::stream::{BoxStream, FuturesUnordered};
use futures_util::{SinkExt, StreamExt};
use tokio::io::unix::AsyncFd;
use tokio::io::AsyncWriteExt;
use tokio::net::{UnixListener, UnixStream};
use tokio_tungstenite::tungstenite::protocol::Role;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
use zbus::{Connection, Proxy};
use zeroize::Zeroize;

use crate::clipboard::{
    bridge_websocket_config, AgentPolicy, ClipboardBridgeReady, ClipboardItem, ClipboardWriterQueue,
};
use crate::logging::target;

use super::mutter::MutterError;

const REMOTE_DESKTOP: &str = "org.gnome.Mutter.RemoteDesktop";
const REMOTE_DESKTOP_PATH: &str = "/org/gnome/Mutter/RemoteDesktop";
/// The interface that carries the clipboard.
pub const CLIPBOARD_INTERFACE: &str = "org.gnome.Mutter.RemoteDesktop.Session";
const CLIPBOARD_METHODS: [&str; 6] = [
    "EnableClipboard",
    "DisableClipboard",
    "SetSelection",
    "SelectionWrite",
    "SelectionWriteDone",
    "SelectionRead",
];
const CLIPBOARD_SIGNALS: [&str; 2] = ["SelectionOwnerChanged", "SelectionTransfer"];
/// The backend token in the bridge's ready line.
pub const BACKEND: &str = "mutter_remote_desktop";

const POLICY_TIMEOUT: Duration = Duration::from_secs(5);
const CALL_TIMEOUT: Duration = Duration::from_secs(5);
const READ_CHUNK: usize = 64 * 1024;

/// One clipboard event from the compositor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipboardEvent {
    /// The selection changed owner.
    OwnerChanged {
        /// Names (MIME types) the new owner offers.
        names: Vec<String>,
        /// The new owner is this session's own selection.
        ours: bool,
    },
    /// A reader asked for one of our names.
    Transfer {
        /// The requested name.
        name: String,
        /// The request serial to answer.
        serial: u32,
    },
}

/// A compositor clipboard as the bridge drives it.
pub trait CompositorClipboard: Sync {
    /// Why the compositor refused a call.
    type Error: std::fmt::Display + Send;

    /// The compositor's clipboard events from now on.
    fn events(
        &self,
    ) -> impl Future<Output = Result<BoxStream<'static, ClipboardEvent>, Self::Error>> + Send;
    /// Drops the selection this session took for the client, if it still
    /// owns it; another owner is left alone.
    fn release(&self) -> impl Future<Output = Result<(), Self::Error>> + Send;
    /// Takes the selection with these names.
    fn set_selection(&self, names: &[&str])
        -> impl Future<Output = Result<(), Self::Error>> + Send;
    /// A descriptor the owner writes `name` to.
    fn read(&self, name: &str) -> impl Future<Output = Result<OwnedFd, Self::Error>> + Send;
    /// A descriptor to write the answer to request `serial` into.
    fn write(&self, serial: u32) -> impl Future<Output = Result<OwnedFd, Self::Error>> + Send;
    /// Reports how the answer to `serial` ended.
    fn write_done(
        &self,
        serial: u32,
        ok: bool,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

/// The clipboard's own, never-started `RemoteDesktop` session. A session
/// that was never started cannot be stopped; Mutter closes it when the
/// agent's bus connection goes away.
#[derive(Debug)]
pub struct MutterClipboard {
    session: Proxy<'static>,
}

impl MutterClipboard {
    /// Creates the clipboard's `RemoteDesktop` session on the session bus and
    /// proves it: every clipboard method and signal is introspected, and the
    /// clipboard is enabled (for the session's lifetime; see the module
    /// documentation for why it is never disabled).
    ///
    /// # Errors
    ///
    /// [`MutterError`] when the session cannot be created, or the clipboard
    /// is missing or refused.
    pub async fn create(connection: &Connection) -> Result<Self, MutterError> {
        let root = Proxy::new(
            connection,
            REMOTE_DESKTOP,
            REMOTE_DESKTOP_PATH,
            REMOTE_DESKTOP,
        )
        .await?;
        let path: OwnedObjectPath = root.call("CreateSession", &()).await?;
        let introspectable = zbus::fdo::IntrospectableProxy::builder(connection)
            .destination(REMOTE_DESKTOP)?
            .path(path.clone())?
            .build()
            .await?;
        let xml = introspectable
            .introspect()
            .await
            .map_err(zbus::Error::from)?;
        let missing = CLIPBOARD_METHODS
            .iter()
            .map(|method| (method, format!("method name=\"{method}\"")))
            .chain(
                CLIPBOARD_SIGNALS
                    .iter()
                    .map(|signal| (signal, format!("signal name=\"{signal}\""))),
            )
            .find(|(_, needle)| !xml.contains(needle.as_str()))
            .map(|(name, _)| *name);
        let session = Proxy::new(connection, REMOTE_DESKTOP, path, CLIPBOARD_INTERFACE).await?;
        let clipboard = Self { session };
        if let Some(name) = missing {
            return Err(MutterError::Missing(name));
        }
        let options: HashMap<&str, Value<'_>> = HashMap::new();
        clipboard.call("EnableClipboard", &(options,)).await?;
        Ok(clipboard)
    }

    async fn call<B>(&self, method: &'static str, body: &B) -> Result<zbus::Message, MutterError>
    where
        B: serde::Serialize + zbus::zvariant::DynamicType,
    {
        tokio::time::timeout(CALL_TIMEOUT, self.session.call_method(method, body))
            .await
            .map_err(|_| MutterError::CallTimeout(method))?
            .map_err(MutterError::from)
    }

    async fn call_fd<B>(&self, method: &'static str, body: &B) -> Result<OwnedFd, MutterError>
    where
        B: serde::Serialize + zbus::zvariant::DynamicType,
    {
        let reply = self.call(method, body).await?;
        let (fd,): (zbus::zvariant::OwnedFd,) = reply.body().deserialize()?;
        Ok(fd.into())
    }
}

fn options(names: &[&str]) -> HashMap<&'static str, Value<'static>> {
    let mut options = HashMap::new();
    options.insert(
        "mime-types",
        Value::from(
            names
                .iter()
                .map(|name| (*name).to_owned())
                .collect::<Vec<_>>(),
        ),
    );
    options
}

/// The names in a `mime-types` option. Mutter sends them as a one-element
/// structure, `(as)`, although its interface documentation says `as`; both
/// are accepted.
#[must_use]
pub fn mime_types(value: Value<'_>) -> Option<Vec<String>> {
    let value = match value {
        Value::Value(inner) => *inner,
        other => other,
    };
    if let Value::Structure(structure) = &value {
        let fields = structure.fields();
        if let [only] = fields {
            return mime_types(only.try_clone().ok()?);
        }
        return None;
    }
    Vec::<String>::try_from(value).ok()
}

fn owner_changed(message: &zbus::Message) -> Option<ClipboardEvent> {
    let (options,): (HashMap<String, OwnedValue>,) = message.body().deserialize().ok()?;
    let value = |key: &str| {
        options
            .get(key)
            .and_then(|value| value.try_clone().ok())
            .map(Value::from)
    };
    let names = value("mime-types").and_then(mime_types).unwrap_or_default();
    let ours = value("session-is-owner")
        .and_then(|value| bool::try_from(value).ok())
        .unwrap_or(false);
    Some(ClipboardEvent::OwnerChanged { names, ours })
}

impl CompositorClipboard for MutterClipboard {
    type Error = MutterError;

    async fn events(&self) -> Result<BoxStream<'static, ClipboardEvent>, MutterError> {
        let owner = self
            .session
            .receive_signal("SelectionOwnerChanged")
            .await?
            .filter_map(|message| async move { owner_changed(&message) });
        let transfer = self
            .session
            .receive_signal("SelectionTransfer")
            .await?
            .filter_map(|message| async move {
                let (name, serial): (String, u32) = message.body().deserialize().ok()?;
                Some(ClipboardEvent::Transfer { name, serial })
            });
        Ok(futures_util::stream::select(owner, transfer).boxed())
    }

    async fn release(&self) -> Result<(), MutterError> {
        // `SetSelection` without names unsets this session's own source
        // only.
        let options: HashMap<&str, Value<'_>> = HashMap::new();
        self.call("SetSelection", &(options,)).await.map(|_| ())
    }

    async fn set_selection(&self, names: &[&str]) -> Result<(), MutterError> {
        self.call("SetSelection", &(options(names),))
            .await
            .map(|_| ())
    }

    async fn read(&self, name: &str) -> Result<OwnedFd, MutterError> {
        self.call_fd("SelectionRead", &(name,)).await
    }

    async fn write(&self, serial: u32) -> Result<OwnedFd, MutterError> {
        self.call_fd("SelectionWrite", &(serial,)).await
    }

    async fn write_done(&self, serial: u32, ok: bool) -> Result<(), MutterError> {
        self.call("SelectionWriteDone", &(serial, ok))
            .await
            .map(|_| ())
    }
}

/// A descriptor read or written without blocking the runtime.
struct Pipe(AsyncFd<OwnedFd>);

impl Pipe {
    fn new(fd: OwnedFd) -> std::io::Result<Self> {
        let flags = nix::fcntl::fcntl(fd.as_raw_fd(), nix::fcntl::FcntlArg::F_GETFL)?;
        let flags = nix::fcntl::OFlag::from_bits_truncate(flags) | nix::fcntl::OFlag::O_NONBLOCK;
        nix::fcntl::fcntl(fd.as_raw_fd(), nix::fcntl::FcntlArg::F_SETFL(flags))?;
        Ok(Self(AsyncFd::new(fd)?))
    }

    async fn read(&self, buffer: &mut [u8]) -> std::io::Result<usize> {
        loop {
            let mut ready = self.0.readable().await?;
            match ready.try_io(|fd| {
                nix::unistd::read(fd.as_raw_fd(), buffer).map_err(std::io::Error::from)
            }) {
                Ok(result) => return result,
                Err(_would_block) => {}
            }
        }
    }

    async fn write(&self, buffer: &[u8]) -> std::io::Result<usize> {
        loop {
            let mut ready = self.0.writable().await?;
            match ready
                .try_io(|fd| nix::unistd::write(fd.get_ref(), buffer).map_err(std::io::Error::from))
            {
                Ok(result) => return result,
                Err(_would_block) => {}
            }
        }
    }
}

/// Reads up to `limit + 1` bytes, each read within the progress timeout.
async fn read_selection(fd: OwnedFd, limit: usize) -> ReadOutcome {
    let Ok(pipe) = Pipe::new(fd) else {
        return ReadOutcome::Failed;
    };
    let mut bytes = Vec::new();
    let mut buffer = vec![0u8; READ_CHUNK];
    let outcome = loop {
        match tokio::time::timeout(SELECTION_PROGRESS_TIMEOUT, pipe.read(&mut buffer)).await {
            Err(_) => break ReadOutcome::TimedOut,
            Ok(Err(_)) => break ReadOutcome::Failed,
            Ok(Ok(0)) => break ReadOutcome::Complete(std::mem::take(&mut bytes)),
            Ok(Ok(read)) => {
                if bytes.len().saturating_add(read) > limit {
                    break ReadOutcome::Overflow;
                }
                if bytes.try_reserve(read).is_err() {
                    break ReadOutcome::Failed;
                }
                bytes.extend_from_slice(&buffer[..read]);
            }
        }
    };
    bytes.zeroize();
    buffer.zeroize();
    outcome
}

/// Writes `bytes` and closes, each write within the progress timeout.
async fn write_selection(fd: OwnedFd, bytes: &[u8]) -> bool {
    let Ok(pipe) = Pipe::new(fd) else {
        return false;
    };
    let mut written = 0;
    while written < bytes.len() {
        let end = bytes.len().min(written.saturating_add(READ_CHUNK));
        match tokio::time::timeout(SELECTION_PROGRESS_TIMEOUT, pipe.write(&bytes[written..end]))
            .await
        {
            Ok(Ok(count)) if count > 0 => written += count,
            _ => return false,
        }
    }
    true
}

/// The agent's `arcen-clipboard.sock`.
#[derive(Debug)]
pub struct ClipboardService {
    listener: UnixListener,
    uid: u32,
    username: String,
}

type Pending<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

impl ClipboardService {
    /// Binds the socket at `path`, readable and writable by its owner only.
    ///
    /// # Errors
    ///
    /// When the socket cannot be created.
    pub fn bind(path: &Path, uid: u32, username: &str) -> std::io::Result<Self> {
        let listener = UnixListener::bind(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        Ok(Self {
            listener,
            uid,
            username: username.to_owned(),
        })
    }

    async fn accept(&self) -> UnixStream {
        loop {
            let Ok((stream, _)) = self.listener.accept().await else {
                continue;
            };
            let peer = stream.peer_cred().ok().map(|credentials| credentials.uid());
            if matches!(peer, Some(uid) if uid == 0 || uid == self.uid) {
                return stream;
            }
            tracing::warn!(
                target: target::SESSION,
                peer_uid = ?peer,
                "refused a clipboard connection from another user"
            );
        }
    }

    /// Serves connections until the future is dropped. A newer connection
    /// supersedes the current one. Whenever a connection ends, the selection
    /// it took for the client is released; the desktop's clipboard is read
    /// only while a Pier is connected.
    pub async fn serve<C: CompositorClipboard>(&self, compositor: &C) {
        let mut number = 0u64;
        let mut current: Option<Pending<'_, Result<(), String>>> = None;
        loop {
            tokio::select! {
                biased;
                stream = self.accept() => {
                    number += 1;
                    if current.take().is_some() {
                        tracing::info!(target: target::SESSION, connection = number, "a newer clipboard connection superseded the current one");
                        release(compositor).await;
                    }
                    current = Some(Box::pin(serve_connection(stream, compositor, &self.username, self.uid)));
                }
                ended = async {
                    match current.as_mut() {
                        Some(connection) => connection.await,
                        None => std::future::pending().await,
                    }
                } => {
                    current = None;
                    release(compositor).await;
                    match ended {
                        Ok(()) => tracing::info!(target: target::SESSION, "clipboard connection ended"),
                        Err(error) => tracing::warn!(target: target::SESSION, %error, "clipboard connection failed"),
                    }
                }
            }
        }
    }
}

async fn release<C: CompositorClipboard>(compositor: &C) {
    if let Err(error) = compositor.release().await {
        tracing::debug!(target: target::SESSION, %error, "clipboard release reported an error");
    }
}

/// One Pier attachment's clipboard.
async fn serve_connection<C: CompositorClipboard>(
    mut stream: UnixStream,
    compositor: &C,
    username: &str,
    uid: u32,
) -> Result<(), String> {
    let ready = ClipboardBridgeReady {
        ready: true,
        pid: std::process::id(),
        uid,
        username: username.to_owned(),
        backend: BACKEND.to_owned(),
    };
    let mut line = serde_json::to_vec(&ready).map_err(|error| error.to_string())?;
    line.push(b'\n');
    stream
        .write_all(&line)
        .await
        .map_err(|error| format!("write clipboard READY: {error}"))?;
    let mut websocket =
        WebSocketStream::from_raw_socket(stream, Role::Server, Some(bridge_websocket_config()))
            .await;
    let negotiation = match tokio::time::timeout(POLICY_TIMEOUT, websocket.next()).await {
        Ok(Some(Ok(Message::Text(text)))) => serde_json::from_str::<AgentPolicy>(&text)
            .ok()
            .and_then(AgentPolicy::into_negotiation)
            .ok_or_else(|| "clipboard policy message is invalid".to_string())?,
        _ => return Err("clipboard policy message is missing".to_string()),
    };
    let events = compositor
        .events()
        .await
        .map_err(|error| format!("subscribe to clipboard signals: {error}"))?;
    tracing::info!(
        target: target::SESSION,
        event = "clipboard_bridge_enabled",
        backend = BACKEND,
        max_bytes = negotiation.policy().max_bytes,
        "clipboard carried for the connected Pier"
    );
    bridge(websocket, events, compositor, negotiation).await
}

#[allow(clippy::too_many_lines)]
async fn bridge<C: CompositorClipboard>(
    websocket: WebSocketStream<UnixStream>,
    mut events: BoxStream<'static, ClipboardEvent>,
    compositor: &C,
    negotiation: ClipboardNegotiation,
) -> Result<(), String> {
    let mut selection = SelectionBridge::new(negotiation);
    let limit = selection.read_limit();
    let mut reassembler = ClipboardReassembler::new(negotiation.policy().max_bytes)
        .map_err(|error| format!("clipboard reassembler: {error}"))?;
    let outbound = ClipboardWriterQueue::new();
    let mut read: Option<(u64, Pending<'_, ReadOutcome>)> = None;
    let mut writes: FuturesUnordered<Pending<'_, (u32, bool)>> = FuturesUnordered::new();
    let mut expiry = tokio::time::interval(Duration::from_secs(1));
    expiry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Reading and writing are separate futures. A host item streams to the
    // Pier while a client item arrives from it; if one loop did both, each
    // side would block writing into a full socket buffer and neither would
    // read again.
    let (mut sink, mut incoming_stream) = websocket.split();
    let writer = async {
        loop {
            match outbound.pop().await {
                Ok(Some(message)) => {
                    if sink.send(message).await.is_err() {
                        return Err("clipboard bridge send failed".to_string());
                    }
                }
                Ok(None) => return Ok(()),
                Err(error) => return Err(error),
            }
        }
    };
    let reader = async {
        loop {
            tokio::select! {
                incoming = incoming_stream.next() => match incoming {
                    Some(Ok(Message::Text(text))) => {
                        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else { continue };
                        if arcen_protocol::messages::msg_type(&value) != Some(CLIPBOARD_DATA) {
                            continue;
                        }
                        if let Ok(offer) = serde_json::from_value::<ClipboardDataMsg>(value) {
                            let _ = reassembler.begin(offer);
                        }
                    }
                    Some(Ok(Message::Binary(bytes))) => {
                        let Ok((header, payload)) = decode_clipboard_chunk(&bytes) else {
                            reassembler.abort();
                            continue;
                        };
                        let mut completed = match reassembler.push(header, payload) {
                            Ok(Some(completed)) => completed,
                            Ok(None) => continue,
                            Err(_) => {
                                reassembler.abort();
                                continue;
                            }
                        };
                        let kind = completed.kind;
                        match selection.remote_item(kind.into(), completed.take_bytes()) {
                            Ok(names) => {
                                if let Err(error) = compositor.set_selection(&names).await {
                                    tracing::warn!(target: target::SESSION, event = "clipboard_client_item_refused", reason = "compositor", %error, "client clipboard item not owned by the desktop");
                                }
                            }
                            Err(error) => tracing::warn!(
                                target: target::SESSION,
                                event = "clipboard_client_item_refused",
                                reason = error.token(),
                                kind = ?kind,
                                "client clipboard item not owned by the desktop"
                            ),
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break Ok(()),
                    Some(Ok(_)) => {}
                    // A Pier attachment that ends drops its socket without a
                    // closing handshake; that is an ordinary end.
                    Some(Err(tokio_tungstenite::tungstenite::Error::Protocol(
                        tokio_tungstenite::tungstenite::error::ProtocolError::ResetWithoutClosingHandshake,
                    ))) => break Ok(()),
                    Some(Err(error)) => break Err(format!("clipboard bridge receive: {error}")),
                },
                event = events.next() => match event {
                    Some(ClipboardEvent::OwnerChanged { names, ours }) => {
                        // Signals are subscribed when the connection starts, so
                        // the owner present before it never arrives here.
                        let OwnerDecision { cancel, read: start } =
                            selection.owner_changed(&names, ours, false);
                        if cancel.is_some() {
                            // Dropping the descriptor ends Mutter's transfer, so
                            // the next read is not refused as parallel.
                            read = None;
                        }
                        if let Some((token, name)) = start {
                            read = Some((token, Box::pin(async move {
                                match compositor.read(&name).await {
                                    Ok(fd) => read_selection(fd, limit).await,
                                    Err(error) => {
                                        tracing::debug!(target: target::SESSION, %error, "selection read refused");
                                        ReadOutcome::Failed
                                    }
                                }
                            })));
                        }
                    }
                    Some(ClipboardEvent::Transfer { name, serial }) => {
                        match selection.transfer_requested(&name, serial) {
                            TransferDecision::Write { serial, bytes } => {
                                writes.push(Box::pin(async move {
                                    let ok = match compositor.write(serial).await {
                                        Ok(fd) => write_selection(fd, &bytes).await,
                                        Err(_) => false,
                                    };
                                    let _ = compositor.write_done(serial, ok).await;
                                    (serial, ok)
                                }));
                            }
                            TransferDecision::Refuse { serial, reason } => {
                                tracing::debug!(target: target::SESSION, serial, reason, "clipboard transfer refused");
                                let _ = compositor.write_done(serial, false).await;
                            }
                        }
                    }
                    None => break Err("the compositor's clipboard signals ended".to_string()),
                },
                (token, outcome) = async {
                    match read.as_mut() {
                        Some((token, pending)) => (*token, pending.await),
                        None => std::future::pending().await,
                    }
                } => {
                    read = None;
                    let failed = match &outcome {
                        ReadOutcome::Complete(_) => None,
                        ReadOutcome::Overflow => Some("too_large"),
                        ReadOutcome::TimedOut => Some("timed_out"),
                        ReadOutcome::Failed => Some("read_failed"),
                    };
                    if let Some(reason) = failed {
                        tracing::warn!(target: target::SESSION, event = "clipboard_host_item_refused", reason, backend = BACKEND, "host clipboard item not sent");
                    }
                    let before = selection.stats();
                    match selection.read_finished(token, outcome) {
                        Some(mut item) => {
                            let (sequence, kind) = (item.sequence, item.kind);
                            if let Some(item) = ClipboardItem::new(sequence, kind.into(), item.take_bytes(), false) {
                                let _ = outbound.enqueue(item);
                            }
                        }
                        None if failed.is_none() => {
                            let after = selection.stats();
                            let reason = if after.too_large > before.too_large {
                                "too_large"
                            } else if after.malformed > before.malformed {
                                "malformed"
                            } else {
                                "policy"
                            };
                            tracing::warn!(target: target::SESSION, event = "clipboard_host_item_refused", reason, backend = BACKEND, "host clipboard item not sent");
                        }
                        None => {}
                    }
                }
                Some((serial, ok)) = writes.next(), if !writes.is_empty() => {
                    selection.transfer_finished(serial, ok);
                }
                _ = expiry.tick() => {
                    let _ = reassembler.expire(std::time::Instant::now());
                }
            }
        }
    };
    let result = tokio::select! {
        result = reader => result,
        result = writer => result,
    };
    drop(read);
    drop(writes);
    selection.clear();
    outbound.close();
    let stats = selection.stats();
    tracing::info!(
        target: target::SESSION,
        event = "clipboard_bridge_summary",
        sent = stats.sent,
        received = stats.received,
        refused = stats.refused,
        too_large = stats.too_large,
        malformed = stats.malformed,
        unsupported = stats.unsupported,
        timed_out = stats.timed_out,
        failed = stats.failed,
        served = stats.served,
        refused_transfers = stats.refused_transfers,
        "clipboard connection summary"
    );
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clipboard::ClipboardBridgeReady;
    use arcen_media::clipboard::{
        ClipboardContent, ClipboardDirection, ClipboardDirections, ClipboardPolicy,
        ClipboardRequest,
    };
    use std::sync::Mutex;
    use tokio::io::AsyncBufReadExt;

    /// A compositor whose descriptors are socket pairs the test holds.
    #[derive(Default)]
    struct Fake {
        calls: Mutex<Vec<String>>,
        events: Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<ClipboardEvent>>>,
        host: Mutex<Vec<UnixStream>>,
    }

    impl Fake {
        fn record(&self, call: String) {
            self.calls.lock().unwrap().push(call);
        }

        fn pair(&self) -> OwnedFd {
            let (ours, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
            theirs.set_nonblocking(true).unwrap();
            self.host
                .lock()
                .unwrap()
                .push(UnixStream::from_std(theirs).unwrap());
            OwnedFd::from(ours)
        }
    }

    impl CompositorClipboard for Fake {
        type Error = String;

        async fn events(&self) -> Result<BoxStream<'static, ClipboardEvent>, String> {
            Ok(stream_of(self.events.lock().unwrap().take().unwrap()))
        }

        async fn release(&self) -> Result<(), String> {
            self.record("release".into());
            Ok(())
        }

        async fn set_selection(&self, names: &[&str]) -> Result<(), String> {
            self.record(format!("set_selection {}", names.join(",")));
            Ok(())
        }

        async fn read(&self, name: &str) -> Result<OwnedFd, String> {
            self.record(format!("read {name}"));
            Ok(self.pair())
        }

        async fn write(&self, serial: u32) -> Result<OwnedFd, String> {
            self.record(format!("write {serial}"));
            Ok(self.pair())
        }

        async fn write_done(&self, serial: u32, ok: bool) -> Result<(), String> {
            self.record(format!("write_done {serial} {ok}"));
            Ok(())
        }
    }

    fn stream_of(
        receiver: tokio::sync::mpsc::UnboundedReceiver<ClipboardEvent>,
    ) -> BoxStream<'static, ClipboardEvent> {
        futures_util::stream::unfold(receiver, |mut receiver| async move {
            receiver.recv().await.map(|event| (event, receiver))
        })
        .boxed()
    }

    #[test]
    fn mime_types_are_read_as_mutter_sends_them_and_as_documented() {
        let names = vec!["text/plain".to_owned(), "image/png".to_owned()];
        assert_eq!(mime_types(Value::from(names.clone())), Some(names.clone()));
        let structure = zbus::zvariant::StructureBuilder::new()
            .add_field(names.clone())
            .build()
            .unwrap();
        assert_eq!(mime_types(Value::from(structure)), Some(names.clone()));
        assert_eq!(mime_types(Value::from(7u32)), None);
    }

    fn negotiation() -> ClipboardNegotiation {
        ClipboardNegotiation::resolve(
            ClipboardPolicy::new(ClipboardDirection::Both, ClipboardContent::All, 64).unwrap(),
            true,
            ClipboardRequest {
                protocol_version: arcen_protocol::messages::CLIPBOARD_PROTOCOL_VERSION,
                text: ClipboardDirections::both(),
                image: ClipboardDirections::both(),
            },
        )
        .unwrap()
    }

    async fn wait_for(fake: &Fake, call: &str) {
        for _ in 0..200 {
            if fake.calls.lock().unwrap().iter().any(|made| made == call) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("{call} never happened: {:?}", fake.calls.lock().unwrap());
    }

    fn scratch() -> std::path::PathBuf {
        let dir = std::env::current_exe()
            .unwrap()
            .with_file_name(format!("arcen-clipboard-service-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn both_directions_through_the_socket_and_release_at_the_end() {
        let dir = scratch();
        let path = dir.join("arcen-clipboard.sock");
        let uid = nix::unistd::Uid::effective().as_raw();
        let service = ClipboardService::bind(&path, uid, "user").unwrap();
        let (events_tx, events_rx) = tokio::sync::mpsc::unbounded_channel();
        let fake = Fake::default();
        *fake.events.lock().unwrap() = Some(events_rx);
        let serving = service.serve(&fake);
        let test = async {
            let mut stream = UnixStream::connect(&path).await.unwrap();
            let mut line = String::new();
            {
                let mut reader = tokio::io::BufReader::new(&mut stream);
                reader.read_line(&mut line).await.unwrap();
            }
            let ready: ClipboardBridgeReady = serde_json::from_str(line.trim()).unwrap();
            assert_eq!((ready.uid, ready.backend.as_str()), (uid, BACKEND));
            let mut websocket = WebSocketStream::from_raw_socket(stream, Role::Client, None).await;
            websocket
                .send(Message::Text(
                    serde_json::to_string(&AgentPolicy::negotiated(negotiation()))
                        .unwrap()
                        .into(),
                ))
                .await
                .unwrap();
            // A host copy is read and sent.
            events_tx
                .send(ClipboardEvent::OwnerChanged {
                    names: vec!["text/html".into(), "UTF8_STRING".into()],
                    ours: false,
                })
                .unwrap();
            wait_for(&fake, "read UTF8_STRING").await;
            assert_eq!(
                fake.calls
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|call| call.starts_with("read"))
                    .count(),
                1
            );
            let mut host = fake.host.lock().unwrap().pop().unwrap();
            host.write_all("héllo".as_bytes()).await.unwrap();
            drop(host);
            let Some(Ok(Message::Text(offer))) = websocket.next().await else {
                panic!("an offer");
            };
            let offer: ClipboardDataMsg = serde_json::from_str(&offer).unwrap();
            assert_eq!((offer.sequence, offer.size_bytes), (1, 6));
            let Some(Ok(Message::Binary(chunk))) = websocket.next().await else {
                panic!("a chunk");
            };
            let (_, payload) = decode_clipboard_chunk(&chunk).unwrap();
            assert_eq!(payload, "héllo".as_bytes());
            // A client item becomes the desktop's selection and is served.
            let transfer = arcen_protocol::clipboard::ClipboardTransfer::new(
                5,
                arcen_protocol::messages::ClipboardContentKind::TextUtf8,
                b"from deck".to_vec(),
            )
            .unwrap();
            let mut cursor = transfer.into_cursor();
            while let Some(message) = cursor.next_message() {
                let message = match message.unwrap() {
                    arcen_protocol::clipboard::ClipboardWireMessage::Offer(offer) => {
                        Message::Text(serde_json::to_string(&offer).unwrap().into())
                    }
                    arcen_protocol::clipboard::ClipboardWireMessage::Chunk(frame) => {
                        Message::Binary(frame.into())
                    }
                };
                websocket.send(message).await.unwrap();
            }
            wait_for(
                &fake,
                "set_selection text/plain;charset=utf-8,UTF8_STRING,text/plain,TEXT,STRING",
            )
            .await;
            events_tx
                .send(ClipboardEvent::Transfer {
                    name: "STRING".into(),
                    serial: 3,
                })
                .unwrap();
            wait_for(&fake, "write 3").await;
            let mut reader = fake.host.lock().unwrap().pop().unwrap();
            let mut served = Vec::new();
            tokio::io::AsyncReadExt::read_to_end(&mut reader, &mut served)
                .await
                .unwrap();
            assert_eq!(served, b"from deck");
            wait_for(&fake, "write_done 3 true").await;
            // Our own ownership is not read back.
            events_tx
                .send(ClipboardEvent::OwnerChanged {
                    names: vec!["text/plain;charset=utf-8".into()],
                    ours: true,
                })
                .unwrap();
            drop(websocket);
            wait_for(&fake, "release").await;
            assert_eq!(
                fake.calls
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|call| call.starts_with("read"))
                    .count(),
                1
            );
        };
        tokio::select! {
            () = serving => panic!("the service never ends on its own"),
            () = test => {}
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn large_items_cross_both_ways_at_once_without_a_deadlock() {
        use crate::clipboard::{
            bridge_websocket_config, ClipboardAgentIo, ClipboardBridgeKind, ClipboardBridgeLink,
        };
        const SIZE: usize = 4 * 1024 * 1024;
        let negotiation = ClipboardNegotiation::resolve(
            ClipboardPolicy::new(
                ClipboardDirection::Both,
                ClipboardContent::All,
                8 * 1024 * 1024,
            )
            .unwrap(),
            true,
            ClipboardRequest {
                protocol_version: arcen_protocol::messages::CLIPBOARD_PROTOCOL_VERSION,
                text: ClipboardDirections::both(),
                image: ClipboardDirections::both(),
            },
        )
        .unwrap();
        let (pier, agent) = UnixStream::pair().unwrap();
        let agent_ws =
            WebSocketStream::from_raw_socket(agent, Role::Server, Some(bridge_websocket_config()))
                .await;
        let pier_ws = WebSocketStream::from_raw_socket(
            ClipboardAgentIo::Session(pier),
            Role::Client,
            Some(bridge_websocket_config()),
        )
        .await;
        let mut link = ClipboardBridgeLink::new(pier_ws, ClipboardBridgeKind::WaylandSession);
        let (events_tx, events_rx) = tokio::sync::mpsc::unbounded_channel();
        let fake = Fake::default();
        let bridged = bridge(agent_ws, stream_of(events_rx), &fake, negotiation);
        let test = async {
            // A host copy starts streaming to the Pier...
            events_tx
                .send(ClipboardEvent::OwnerChanged {
                    names: vec!["text/plain;charset=utf-8".into()],
                    ours: false,
                })
                .unwrap();
            wait_for(&fake, "read text/plain;charset=utf-8").await;
            let mut host = fake.host.lock().unwrap().pop().unwrap();
            tokio::spawn(async move {
                let _ = host.write_all(&vec![b'h'; SIZE]).await;
            });
            // ...while a client item of the same size goes to the agent.
            assert!(link.offer(
                ClipboardItem::new(
                    1,
                    arcen_protocol::messages::ClipboardContentKind::TextUtf8,
                    vec![b'd'; SIZE],
                    false,
                )
                .unwrap()
            ));
            let mut received = 0usize;
            let mut offered = None;
            while received < SIZE {
                match link.next().await {
                    Some(Ok(Message::Text(text))) => {
                        let offer: ClipboardDataMsg = serde_json::from_str(&text).unwrap();
                        offered = Some(offer.size_bytes);
                    }
                    Some(Ok(Message::Binary(frame))) => {
                        received += decode_clipboard_chunk(&frame).unwrap().1.len();
                    }
                    other => panic!("bridge ended: {other:?}"),
                }
            }
            assert_eq!(offered, Some(u32::try_from(SIZE).unwrap()));
            wait_for(
                &fake,
                "set_selection text/plain;charset=utf-8,UTF8_STRING,text/plain,TEXT,STRING",
            )
            .await;
        };
        tokio::time::timeout(Duration::from_secs(30), async {
            tokio::select! {
                result = bridged => panic!("the bridge ended: {result:?}"),
                () = test => {}
            }
        })
        .await
        .expect("both directions completed without a deadlock");
    }

    #[tokio::test]
    async fn an_oversized_host_copy_is_refused_and_the_bridge_goes_on() {
        let (events_tx, events_rx) = tokio::sync::mpsc::unbounded_channel();
        let fake = Fake::default();
        let (pier, agent) = UnixStream::pair().unwrap();
        let agent_ws = WebSocketStream::from_raw_socket(agent, Role::Server, None).await;
        let mut pier_ws = WebSocketStream::from_raw_socket(pier, Role::Client, None).await;
        let bridged = bridge(agent_ws, stream_of(events_rx), &fake, negotiation());
        let test = async {
            events_tx
                .send(ClipboardEvent::OwnerChanged {
                    names: vec!["text/plain".into()],
                    ours: false,
                })
                .unwrap();
            wait_for(&fake, "read text/plain").await;
            let mut host = fake.host.lock().unwrap().pop().unwrap();
            // 65 bytes against a 64-byte policy: refused, not truncated.
            let _ = host.write_all(&[b'a'; 65]).await;
            drop(host);
            events_tx
                .send(ClipboardEvent::OwnerChanged {
                    names: vec!["text/plain".into()],
                    ours: false,
                })
                .unwrap();
            for _ in 0..200 {
                if fake
                    .calls
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|call| call.starts_with("read"))
                    .count()
                    == 2
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            let mut host = fake.host.lock().unwrap().pop().unwrap();
            host.write_all(b"fits").await.unwrap();
            drop(host);
            let Some(Ok(Message::Text(offer))) = pier_ws.next().await else {
                panic!("the next copy still arrives");
            };
            let offer: ClipboardDataMsg = serde_json::from_str(&offer).unwrap();
            assert_eq!((offer.sequence, offer.size_bytes), (1, 4));
        };
        tokio::select! {
            result = bridged => panic!("the bridge ended: {result:?}"),
            () = test => {}
        }
    }
}
