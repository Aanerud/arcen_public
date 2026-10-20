use super::{
    AgentPolicy, ClipboardAgentReady, ClipboardItem, ClipboardMailbox, ClipboardWriterQueue,
};
use arcen_media::clipboard::formats::{
    choose_host_format, decode_host_payload, host_format, offered_names, render_for_host,
    validate_payload, IMAGE_PNG, TEXT_PLAIN, TEXT_PLAIN_UTF8, UTF8_STRING, X11_STRING, X11_TEXT,
};
use arcen_media::clipboard::{ClipboardFlow, ClipboardNegotiation};
use arcen_protocol::clipboard::ClipboardReassembler;
use arcen_protocol::messages::{ClipboardDataMsg, CLIPBOARD_DATA};
use arcen_protocol::{decode_clipboard_chunk, CHUNK_BYTES};
use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_tungstenite::tungstenite::protocol::Role;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;
use x11rb::connection::Connection;
use x11rb::protocol::xfixes::{ConnectionExt as _, SelectionEventMask};
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ChangeWindowAttributesAux, ConnectionExt as _, CreateWindowAux, EventMask,
    PropMode, Property, SelectionNotifyEvent, SelectionRequestEvent, Window, WindowClass,
    SELECTION_NOTIFY_EVENT,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::{COPY_FROM_PARENT, CURRENT_TIME};
use zeroize::Zeroize;

const INCR_TIMEOUT: Duration = Duration::from_secs(5);
const X11_POLL: Duration = Duration::from_millis(10);

struct StdioIo {
    stdin: tokio::io::Stdin,
    stdout: tokio::io::Stdout,
}

impl AsyncRead for StdioIo {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.stdin).poll_read(context, buffer)
    }
}

impl AsyncWrite for StdioIo {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
        buffer: &[u8],
    ) -> std::task::Poll<Result<usize, std::io::Error>> {
        std::pin::Pin::new(&mut self.stdout).poll_write(context, buffer)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), std::io::Error>> {
        std::pin::Pin::new(&mut self.stdout).poll_flush(context)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), std::io::Error>> {
        std::pin::Pin::new(&mut self.stdout).poll_shutdown(context)
    }
}

pub async fn run() -> Result<(), String> {
    use tokio::io::AsyncWriteExt;

    let (worker, major, minor) = X11Worker::connect()?;
    let ready = ClipboardAgentReady {
        ready: true,
        pid: std::process::id(),
        uid: super::effective_uid(),
        username: std::env::var("USER").map_err(|_| "USER is missing".to_string())?,
        display: std::env::var("DISPLAY").map_err(|_| "DISPLAY is missing".to_string())?,
        xfixes_major: major,
        xfixes_minor: minor,
    };
    let mut stdout = tokio::io::stdout();
    stdout
        .write_all(
            format!(
                "{}\n",
                serde_json::to_string(&ready)
                    .map_err(|error| format!("serialize clipboard READY: {error}"))?
            )
            .as_bytes(),
        )
        .await
        .map_err(|error| format!("write clipboard READY: {error}"))?;
    stdout
        .flush()
        .await
        .map_err(|error| format!("flush clipboard READY: {error}"))?;

    let io = StdioIo {
        stdin: tokio::io::stdin(),
        stdout,
    };
    let mut websocket =
        WebSocketStream::from_raw_socket(io, Role::Server, Some(super::bridge_websocket_config()))
            .await;
    let negotiation = match websocket.next().await {
        Some(Ok(Message::Text(text))) => serde_json::from_str::<AgentPolicy>(&text)
            .ok()
            .and_then(AgentPolicy::into_negotiation)
            .ok_or_else(|| "clipboard policy message is invalid".to_string())?,
        _ => return Err("clipboard policy message is missing".to_string()),
    };

    let remote = ClipboardMailbox::new();
    let local = ClipboardWriterQueue::new();
    let worker_remote = Arc::clone(&remote);
    let worker_local = Arc::clone(&local);
    let mut worker_task =
        tokio::task::spawn_blocking(move || worker.run(negotiation, worker_remote, worker_local));
    let mut worker_finished = None;
    let policy = negotiation.policy();
    let mut reassembler = ClipboardReassembler::new(policy.max_bytes)
        .map_err(|error| format!("clipboard child reassembler: {error}"))?;
    // Writing never holds up reading: a host item streaming to the Pier while
    // a client item arrives would otherwise leave both ends blocked writing
    // into full pipes.
    let (mut sink, mut incoming_stream) = websocket.split();
    let writer = async {
        loop {
            match local.pop().await {
                Ok(Some(message)) => {
                    if sink.send(message).await.is_err() {
                        return Err("clipboard child IPC send failed".to_string());
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
                        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
                            continue;
                        };
                        if arcen_protocol::messages::msg_type(&value) != Some(CLIPBOARD_DATA) {
                            continue;
                        }
                        let Ok(offer) = serde_json::from_value::<ClipboardDataMsg>(value) else {
                            continue;
                        };
                        if negotiation.allows(ClipboardFlow::ClientToHost, offer.kind.into())
                            && policy
                            .check_size(
                                ClipboardFlow::ClientToHost,
                                offer.kind.into(),
                                usize::try_from(offer.size_bytes).unwrap_or(usize::MAX),
                            )
                            .is_ok()
                        {
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
                        if validate_payload(
                            policy,
                            ClipboardFlow::ClientToHost,
                            completed.kind.into(),
                            &completed.bytes,
                        )
                        .is_ok()
                        {
                            if let Some(item) = ClipboardItem::new(
                                completed.sequence,
                                completed.kind,
                                completed.take_bytes(),
                                completed.truncated,
                            ) {
                                let _ = remote.replace(item);
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break Ok(()),
                    Some(Ok(_)) => {}
                    // The Pier ends an attachment by dropping the pipe; that is
                    // an ordinary end, not a failure.
                    Some(Err(tokio_tungstenite::tungstenite::Error::Protocol(
                        tokio_tungstenite::tungstenite::error::ProtocolError::ResetWithoutClosingHandshake,
                    ))) => break Ok(()),
                    Some(Err(error)) => break Err(format!("clipboard child IPC receive: {error}")),
                },
                worker = &mut worker_task => {
                    worker_finished = Some(
                        worker
                            .map_err(|error| format!("X11 clipboard worker join: {error}"))
                            .and_then(|result| result)
                    );
                    break Err("X11 clipboard worker ended during IPC".to_string());
                },
            }
        }
    };
    let result = tokio::select! {
        result = reader => result,
        result = writer => result,
    };
    remote.close();
    local.close();
    let worker_result = match worker_finished {
        Some(result) => result,
        None => worker_task
            .await
            .map_err(|error| format!("X11 clipboard worker join: {error}"))
            .and_then(|result| result),
    };
    result.and(worker_result)
}

#[derive(Clone, Copy)]
struct Atoms {
    clipboard: Atom,
    targets: Atom,
    timestamp: Atom,
    utf8: Atom,
    text_plain: Atom,
    text_plain_bare: Atom,
    text: Atom,
    string: Atom,
    png: Atom,
    incr: Atom,
}

impl Atoms {
    /// The shared format-table name of a selection target this child knows.
    fn name(self, atom: Atom) -> Option<&'static str> {
        [
            (self.utf8, UTF8_STRING),
            (self.text_plain, TEXT_PLAIN_UTF8),
            (self.text_plain_bare, TEXT_PLAIN),
            (self.text, X11_TEXT),
            (self.string, X11_STRING),
            (self.png, IMAGE_PNG),
        ]
        .into_iter()
        .find(|(known, _)| *known == atom)
        .map(|(_, name)| name)
    }

    /// The atom of a shared format-table name.
    fn atom(self, name: &str) -> Option<Atom> {
        [
            (UTF8_STRING, self.utf8),
            (TEXT_PLAIN_UTF8, self.text_plain),
            (TEXT_PLAIN, self.text_plain_bare),
            (X11_TEXT, self.text),
            (X11_STRING, self.string),
            (IMAGE_PNG, self.png),
        ]
        .into_iter()
        .find(|(known, _)| *known == name)
        .map(|(_, atom)| atom)
    }
}

/// Why the child declined a host copy. A desktop application offering too
/// much, or offering it oddly, is never a reason to end the attachment.
fn refuse_host_copy(reason: &'static str) {
    tracing::warn!(
        event = "clipboard_host_item_refused",
        reason,
        backend = "x11",
        "host clipboard item not sent"
    );
}

struct SendIncr {
    requestor: Window,
    property: Atom,
    target: Atom,
    transfer: super::IncrTransfer,
}

struct ReceiveIncr {
    target: Atom,
    property: Atom,
    declared: usize,
    bytes: Vec<u8>,
    last_progress: Instant,
}

impl Drop for ReceiveIncr {
    fn drop(&mut self) {
        self.bytes.zeroize();
    }
}

#[derive(Clone, Copy)]
struct PendingConversion {
    target: Atom,
    owner: Window,
    time: u32,
    /// The property this conversion was asked to land on. Each conversion
    /// gets its own, so a late chunk from an earlier owner can never be read
    /// as this one.
    property: Atom,
}

/// An INCR transfer the child no longer wants (refused, superseded or timed
/// out). Its owner keeps sending after each delete, so the child keeps
/// deleting until the zero-length end or the progress deadline; the bytes are
/// never read.
struct Draining {
    property: Atom,
    last_progress: Instant,
}

/// How many conversion properties rotate. A property is reused only when no
/// transfer on it is pending, receiving or draining.
const PROPERTY_RING: usize = 32;

struct X11Worker {
    connection: RustConnection,
    window: Window,
    atoms: Atoms,
    owner_time: u32,
    remote_item: Option<ClipboardItem>,
    pending_target: Option<PendingConversion>,
    send_incr: Option<SendIncr>,
    receive_incr: Option<ReceiveIncr>,
    draining: Vec<Draining>,
    properties: Vec<Atom>,
    next_property: usize,
    next_sequence: u64,
    claim_pending: bool,
}

impl X11Worker {
    fn connect() -> Result<(Self, u32, u32), String> {
        let (connection, screen_index) =
            x11rb::connect(None).map_err(|error| format!("connect X11 clipboard: {error}"))?;
        let version = connection
            .xfixes_query_version(5, 0)
            .map_err(|error| format!("query XFixes: {error}"))?
            .reply()
            .map_err(|error| format!("read XFixes version: {error}"))?;
        let screen = &connection.setup().roots[screen_index];
        let window = connection
            .generate_id()
            .map_err(|error| format!("allocate clipboard window: {error}"))?;
        connection
            .create_window(
                u8::try_from(COPY_FROM_PARENT).unwrap_or(0),
                window,
                screen.root,
                0,
                0,
                1,
                1,
                0,
                WindowClass::INPUT_OUTPUT,
                0,
                &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
            )
            .map_err(|error| format!("create clipboard window: {error}"))?
            .check()
            .map_err(|error| format!("check clipboard window: {error}"))?;
        let atoms = Atoms {
            clipboard: intern(&connection, b"CLIPBOARD")?,
            targets: intern(&connection, b"TARGETS")?,
            timestamp: intern(&connection, b"TIMESTAMP")?,
            utf8: intern(&connection, b"UTF8_STRING")?,
            text_plain: intern(&connection, TEXT_PLAIN_UTF8.as_bytes())?,
            text_plain_bare: intern(&connection, TEXT_PLAIN.as_bytes())?,
            text: intern(&connection, b"TEXT")?,
            string: AtomEnum::STRING.into(),
            png: intern(&connection, b"image/png")?,
            incr: intern(&connection, b"INCR")?,
        };
        connection
            .xfixes_select_selection_input(
                window,
                atoms.clipboard,
                SelectionEventMask::SET_SELECTION_OWNER
                    | SelectionEventMask::SELECTION_WINDOW_DESTROY
                    | SelectionEventMask::SELECTION_CLIENT_CLOSE,
            )
            .map_err(|error| format!("select XFixes clipboard events: {error}"))?
            .check()
            .map_err(|error| format!("check XFixes clipboard events: {error}"))?;
        connection
            .flush()
            .map_err(|error| format!("flush X11 clipboard setup: {error}"))?;
        Ok((
            Self {
                connection,
                window,
                atoms,
                owner_time: CURRENT_TIME,
                remote_item: None,
                pending_target: None,
                send_incr: None,
                receive_incr: None,
                draining: Vec::new(),
                properties: Vec::new(),
                next_property: 0,
                next_sequence: 1,
                claim_pending: false,
            },
            version.major_version,
            version.minor_version,
        ))
    }

    fn run(
        mut self,
        policy: ClipboardNegotiation,
        remote: Arc<ClipboardMailbox>,
        local: Arc<ClipboardWriterQueue>,
    ) -> Result<(), String> {
        while !remote.is_closed() {
            self.step(policy, &remote, &local, Instant::now())?;
            std::thread::sleep(X11_POLL);
        }
        let owner = self
            .connection
            .get_selection_owner(self.atoms.clipboard)
            .map_err(|error| format!("query X11 clipboard owner: {error}"))?
            .reply()
            .map_err(|error| format!("read X11 clipboard owner: {error}"))?;
        if owner.owner == self.window {
            self.connection
                .set_selection_owner(x11rb::NONE, self.atoms.clipboard, self.owner_time)
                .map_err(|error| format!("release X11 clipboard: {error}"))?;
        }
        self.connection
            .flush()
            .map_err(|error| format!("flush X11 clipboard release: {error}"))
    }

    /// One pass: every queued X event, a client item to own, deadlines.
    fn step(
        &mut self,
        policy: ClipboardNegotiation,
        remote: &ClipboardMailbox,
        local: &ClipboardWriterQueue,
        now: Instant,
    ) -> Result<(), String> {
        while let Some(event) = self
            .connection
            .poll_for_event()
            .map_err(|error| format!("poll X11 clipboard event: {error}"))?
        {
            self.handle_event(event, policy, local)?;
        }
        if let Some(item) = remote.try_take() {
            self.claim_remote(item)?;
        }
        if self
            .send_incr
            .as_ref()
            .is_some_and(|state| state.transfer.expired(now))
        {
            self.send_incr = None;
        }
        if self
            .receive_incr
            .as_ref()
            .is_some_and(|state| now.saturating_duration_since(state.last_progress) >= INCR_TIMEOUT)
        {
            self.abandon_receive();
        }
        self.draining
            .retain(|state| now.saturating_duration_since(state.last_progress) < INCR_TIMEOUT);
        self.connection
            .flush()
            .map_err(|error| format!("flush X11 clipboard step: {error}"))
    }

    /// A property for the next conversion: never one that a pending,
    /// receiving or draining transfer still uses. `None` when every ring
    /// property is busy; an error only when the X connection fails.
    fn next_property(&mut self) -> Result<Option<Atom>, String> {
        for _ in 0..PROPERTY_RING {
            let index = self.next_property % PROPERTY_RING;
            self.next_property = self.next_property.wrapping_add(1);
            if self.properties.len() <= index {
                let name = format!("_ARCEN_CLIPBOARD_{index}");
                let atom = intern(&self.connection, name.as_bytes())?;
                self.properties.push(atom);
            }
            let atom = self.properties[index];
            if !self.property_in_use(atom) {
                return Ok(Some(atom));
            }
        }
        Ok(None)
    }

    fn property_in_use(&self, atom: Atom) -> bool {
        self.pending_target
            .is_some_and(|pending| pending.property == atom)
            || self
                .receive_incr
                .as_ref()
                .is_some_and(|receive| receive.property == atom)
            || self.draining.iter().any(|state| state.property == atom)
    }

    /// Stops receiving the current INCR transfer; its owner is drained.
    fn abandon_receive(&mut self) {
        if let Some(receive) = self.receive_incr.take() {
            self.drain(receive.property);
        }
    }

    fn drain(&mut self, property: Atom) {
        if !self.draining.iter().any(|state| state.property == property) {
            self.draining.push(Draining {
                property,
                last_progress: Instant::now(),
            });
        }
    }

    /// Deletes one chunk of a drained transfer, unread; a zero-length chunk
    /// ends it.
    fn drain_chunk(&mut self, property: Atom) -> Result<(), String> {
        let reply = self
            .connection
            .get_property(false, self.window, property, AtomEnum::ANY, 0, 0)
            .map_err(|error| format!("size X11 drained property: {error}"))?
            .reply()
            .map_err(|error| format!("reply X11 drained property: {error}"))?;
        self.connection
            .delete_property(self.window, property)
            .map_err(|error| format!("delete X11 drained property: {error}"))?;
        if reply.bytes_after == 0 {
            self.draining.retain(|state| state.property != property);
        } else if let Some(state) = self
            .draining
            .iter_mut()
            .find(|state| state.property == property)
        {
            state.last_progress = Instant::now();
        }
        Ok(())
    }

    /// Asks the owner to convert the selection to `target` onto a fresh
    /// property.
    fn request_conversion(&mut self, target: Atom, owner: Window, time: u32) -> Result<(), String> {
        let Some(property) = self.next_property()? else {
            // Every property still drains an interrupted transfer (a burst of
            // large copies, or a hostile client): this copy is refused, the
            // session goes on, and the ring frees up within INCR_TIMEOUT.
            refuse_host_copy("busy");
            self.pending_target = None;
            return Ok(());
        };
        self.pending_target = Some(PendingConversion {
            target,
            owner,
            time,
            property,
        });
        self.connection
            .convert_selection(self.window, self.atoms.clipboard, target, property, time)
            .map_err(|error| format!("request X11 clipboard conversion: {error}"))?;
        Ok(())
    }

    fn claim_remote(&mut self, item: ClipboardItem) -> Result<(), String> {
        self.remote_item = Some(item);
        self.pending_target = None;
        self.send_incr = None;
        self.abandon_receive();
        self.claim_pending = true;
        self.connection
            .set_selection_owner(self.window, self.atoms.clipboard, CURRENT_TIME)
            .map_err(|error| format!("claim X11 clipboard: {error}"))?
            .check()
            .map_err(|error| format!("check X11 clipboard claim: {error}"))?;
        self.connection
            .flush()
            .map_err(|error| format!("flush X11 clipboard claim: {error}"))
    }

    fn handle_event(
        &mut self,
        event: Event,
        policy: ClipboardNegotiation,
        local: &ClipboardWriterQueue,
    ) -> Result<(), String> {
        match event {
            Event::XfixesSelectionNotify(event) if event.selection == self.atoms.clipboard => {
                if event.owner == self.window {
                    self.claim_pending = false;
                    self.owner_time = event.selection_timestamp;
                } else {
                    if self.claim_pending {
                        let actual = self
                            .connection
                            .get_selection_owner(self.atoms.clipboard)
                            .map_err(|error| format!("query X11 clipboard owner: {error}"))?
                            .reply()
                            .map_err(|error| format!("read X11 clipboard owner: {error}"))?;
                        if actual.owner == self.window {
                            return Ok(());
                        }
                        self.claim_pending = false;
                    }
                    self.owner_time = event.selection_timestamp;
                    self.remote_item = None;
                    self.send_incr = None;
                    self.pending_target = None;
                    self.abandon_receive();
                    if event.owner != x11rb::NONE {
                        self.request_conversion(self.atoms.targets, event.owner, event.timestamp)?;
                    }
                }
            }
            Event::SelectionNotify(event) if event.requestor == self.window => {
                self.handle_selection_notify(event, policy, local)?;
            }
            Event::SelectionRequest(event) => self.handle_selection_request(event)?,
            Event::SelectionClear(_) => {
                let actual = self
                    .connection
                    .get_selection_owner(self.atoms.clipboard)
                    .map_err(|error| format!("query X11 clipboard owner: {error}"))?
                    .reply()
                    .map_err(|error| format!("read X11 clipboard owner: {error}"))?;
                if actual.owner != self.window {
                    self.claim_pending = false;
                    self.remote_item = None;
                    self.send_incr = None;
                }
            }
            Event::PropertyNotify(event) if event.window == self.window => {
                if event.state == Property::NEW_VALUE {
                    if self
                        .receive_incr
                        .as_ref()
                        .is_some_and(|receive| receive.property == event.atom)
                    {
                        self.receive_incr_chunk(policy, local)?;
                    } else if self
                        .draining
                        .iter()
                        .any(|state| state.property == event.atom)
                    {
                        self.drain_chunk(event.atom)?;
                    }
                }
            }
            Event::PropertyNotify(event) if event.state == Property::DELETE => {
                self.send_incr_chunk(event.window, event.atom)?;
            }
            _ => {}
        }
        self.connection
            .flush()
            .map_err(|error| format!("flush X11 clipboard event: {error}"))
    }

    fn handle_selection_notify(
        &mut self,
        event: x11rb::protocol::xproto::SelectionNotifyEvent,
        policy: ClipboardNegotiation,
        local: &ClipboardWriterQueue,
    ) -> Result<(), String> {
        let Some(pending) = self.pending_target else {
            self.discard_stray_notify(event)?;
            return Ok(());
        };
        if event.time != pending.time
            || (event.property != x11rb::NONE && event.property != pending.property)
        {
            self.discard_stray_notify(event)?;
            return Ok(());
        }
        let owner = self
            .connection
            .get_selection_owner(self.atoms.clipboard)
            .map_err(|error| format!("query X11 clipboard owner: {error}"))?
            .reply()
            .map_err(|error| format!("read X11 clipboard owner: {error}"))?;
        if owner.owner != pending.owner {
            return Ok(());
        }
        if event.property == x11rb::NONE {
            self.pending_target = None;
            return Ok(());
        }
        let reply = self
            .connection
            .get_property(
                true,
                self.window,
                event.property,
                AtomEnum::ANY,
                0,
                u32::try_from(CHUNK_BYTES / 4).unwrap_or(u32::MAX),
            )
            .map_err(|error| format!("read X11 selection property: {error}"))?
            .reply()
            .map_err(|error| format!("reply X11 selection property: {error}"))?;
        if pending.target == self.atoms.targets {
            let targets = reply
                .value32()
                .map(|values| values.collect::<Vec<_>>())
                .unwrap_or_default();
            let target = choose_target(policy, self.atoms, &targets);
            if let Some(target) = target {
                self.pending_target = None;
                self.request_conversion(target, pending.owner, pending.time)?;
            } else {
                self.pending_target = None;
            }
            return Ok(());
        }
        self.pending_target = None;
        let target = pending.target;
        if reply.type_ == self.atoms.incr {
            let declared = reply
                .value32()
                .and_then(|mut values| values.next())
                .and_then(|value| usize::try_from(value).ok());
            let Some(declared) = declared.filter(|value| *value <= policy.policy().max_bytes)
            else {
                refuse_host_copy(if declared.is_some() {
                    "too_large"
                } else {
                    "invalid_incr"
                });
                // Deleting the INCR header told the owner to start sending.
                self.drain(pending.property);
                return Ok(());
            };
            self.receive_incr = Some(ReceiveIncr {
                target,
                property: pending.property,
                declared,
                bytes: Vec::new(),
                last_progress: Instant::now(),
            });
            return Ok(());
        }
        if reply.type_ != target {
            refuse_host_copy("type_mismatch");
            return Ok(());
        }
        if reply.bytes_after != 0 {
            // More than one chunk without INCR: larger than a single read
            // bounds, so refused rather than truncated. A partial read does
            // not delete the property.
            refuse_host_copy("too_large");
            self.connection
                .delete_property(self.window, pending.property)
                .map_err(|error| format!("delete X11 oversized property: {error}"))?;
            return Ok(());
        }
        self.finish_local(target, reply.value, policy, local)
    }

    /// A conversion answer this child no longer waits for: whatever landed
    /// on our property is deleted unread, and an INCR start is drained.
    fn discard_stray_notify(&mut self, event: SelectionNotifyEvent) -> Result<(), String> {
        if event.property == x11rb::NONE || !self.properties.contains(&event.property) {
            return Ok(());
        }
        if self.property_in_use(event.property) {
            return Ok(());
        }
        let reply = self
            .connection
            .get_property(true, self.window, event.property, AtomEnum::ANY, 0, 0)
            .map_err(|error| format!("read stray X11 property: {error}"))?
            .reply()
            .map_err(|error| format!("reply stray X11 property: {error}"))?;
        self.connection
            .delete_property(self.window, event.property)
            .map_err(|error| format!("delete stray X11 property: {error}"))?;
        if reply.type_ == self.atoms.incr {
            self.drain(event.property);
        }
        Ok(())
    }

    fn receive_incr_chunk(
        &mut self,
        policy: ClipboardNegotiation,
        local: &ClipboardWriterQueue,
    ) -> Result<(), String> {
        let Some(mut receive) = self.receive_incr.take() else {
            return Ok(());
        };
        let reply = self
            .connection
            .get_property(
                true,
                self.window,
                receive.property,
                AtomEnum::ANY,
                0,
                u32::try_from(CHUNK_BYTES / 4).unwrap_or(u32::MAX),
            )
            .map_err(|error| format!("read X11 INCR property: {error}"))?
            .reply()
            .map_err(|error| format!("reply X11 INCR property: {error}"))?;
        if reply.bytes_after != 0 {
            refuse_host_copy("incr_chunk_too_large");
            self.connection
                .delete_property(self.window, receive.property)
                .map_err(|error| format!("delete X11 INCR property: {error}"))?;
            self.drain(receive.property);
            return Ok(());
        }
        if reply.value.is_empty() {
            if receive.bytes.len() < receive.declared {
                refuse_host_copy("incr_size_mismatch");
                return Ok(());
            }
            let bytes = std::mem::take(&mut receive.bytes);
            return self.finish_local(receive.target, bytes, policy, local);
        }
        let next = receive.bytes.len().saturating_add(reply.value.len());
        if reply.value.len() > CHUNK_BYTES || next > policy.policy().max_bytes {
            // The owner sent more than it declared or than the policy
            // allows: drain the rest of this transfer, keep the session.
            refuse_host_copy("too_large");
            self.drain(receive.property);
            return Ok(());
        }
        if receive.bytes.try_reserve(reply.value.len()).is_err() {
            refuse_host_copy("allocation");
            self.drain(receive.property);
            return Ok(());
        }
        receive.bytes.extend_from_slice(&reply.value);
        receive.last_progress = Instant::now();
        self.receive_incr = Some(receive);
        Ok(())
    }

    fn finish_local(
        &mut self,
        target: Atom,
        bytes: Vec<u8>,
        policy: ClipboardNegotiation,
        local: &ClipboardWriterQueue,
    ) -> Result<(), String> {
        let Some(format) = self.atoms.name(target).and_then(host_format) else {
            refuse_host_copy("unsupported");
            return Ok(());
        };
        let (kind, bytes) = match decode_host_payload(policy.policy(), format, bytes) {
            Ok(decoded) => decoded,
            Err(error) => {
                refuse_host_copy(error.token());
                return Ok(());
            }
        };
        let sequence = self.next_sequence;
        self.next_sequence = self
            .next_sequence
            .checked_add(1)
            .ok_or_else(|| "X11 clipboard sequence exhausted".to_string())?;
        if let Some(item) = ClipboardItem::new(sequence, kind.into(), bytes, false) {
            let _ = local.enqueue(item);
        }
        Ok(())
    }

    fn handle_selection_request(&mut self, request: SelectionRequestEvent) -> Result<(), String> {
        let property = if request.property == x11rb::NONE {
            request.target
        } else {
            request.property
        };
        let mut success = false;
        if request.target == self.atoms.targets {
            let targets = self.supported_targets();
            self.connection
                .change_property32(
                    PropMode::REPLACE,
                    request.requestor,
                    property,
                    AtomEnum::ATOM,
                    &targets,
                )
                .map_err(|error| format!("write X11 TARGETS: {error}"))?;
            success = true;
        } else if request.target == self.atoms.timestamp {
            self.connection
                .change_property32(
                    PropMode::REPLACE,
                    request.requestor,
                    property,
                    AtomEnum::INTEGER,
                    &[self.owner_time],
                )
                .map_err(|error| format!("write X11 TIMESTAMP: {error}"))?;
            success = true;
        } else if let Some(item) = self.remote_item.as_ref() {
            let rendered = self.atoms.name(request.target).and_then(|name| {
                render_for_host(name, item.kind.into(), &item.bytes)
                    .map(std::borrow::Cow::into_owned)
            });
            if let Some(payload) = rendered {
                if payload.len() > CHUNK_BYTES && self.send_incr.is_some() {
                    // One outbound INCR is the hard bound. Leave `success`
                    // false so this requestor receives property NONE.
                } else if payload.len() <= CHUNK_BYTES {
                    self.connection
                        .change_property8(
                            PropMode::REPLACE,
                            request.requestor,
                            property,
                            request.target,
                            &payload,
                        )
                        .map_err(|error| format!("write X11 selection: {error}"))?;
                    success = true;
                } else {
                    self.connection
                        .change_window_attributes(
                            request.requestor,
                            &ChangeWindowAttributesAux::new()
                                .event_mask(EventMask::PROPERTY_CHANGE),
                        )
                        .map_err(|error| format!("watch X11 INCR requestor: {error}"))?;
                    self.connection
                        .change_property32(
                            PropMode::REPLACE,
                            request.requestor,
                            property,
                            self.atoms.incr,
                            &[u32::try_from(payload.len())
                                .map_err(|_| "X11 INCR payload exceeds u32")?],
                        )
                        .map_err(|error| format!("announce X11 INCR: {error}"))?;
                    self.send_incr = Some(SendIncr {
                        requestor: request.requestor,
                        property,
                        target: request.target,
                        transfer: super::IncrTransfer::new(payload, Instant::now())
                            .ok_or_else(|| "invalid X11 INCR payload".to_string())?,
                    });
                    success = true;
                }
            }
        }
        let notify = SelectionNotifyEvent {
            response_type: SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: request.time,
            requestor: request.requestor,
            selection: request.selection,
            target: request.target,
            property: if success { property } else { x11rb::NONE },
        };
        self.connection
            .send_event(false, request.requestor, EventMask::NO_EVENT, notify)
            .map_err(|error| format!("send X11 selection notify: {error}"))?;
        Ok(())
    }

    fn send_incr_chunk(&mut self, window: Window, property: Atom) -> Result<(), String> {
        let Some(mut state) = self.send_incr.take() else {
            return Ok(());
        };
        if state.requestor != window || state.property != property {
            self.send_incr = Some(state);
            return Ok(());
        }
        let chunk = state.transfer.next_chunk(Instant::now()).unwrap_or(&[]);
        self.connection
            .change_property8(
                PropMode::REPLACE,
                state.requestor,
                state.property,
                state.target,
                chunk,
            )
            .map_err(|error| format!("send X11 INCR chunk: {error}"))?;
        if !chunk.is_empty() {
            self.send_incr = Some(state);
        }
        Ok(())
    }

    fn supported_targets(&self) -> Vec<Atom> {
        let mut targets = vec![self.atoms.targets, self.atoms.timestamp];
        if let Some(item) = self.remote_item.as_ref() {
            targets.extend(
                offered_names(item.kind.into(), &item.bytes)
                    .into_iter()
                    .filter_map(|name| self.atoms.atom(name)),
            );
        }
        targets
    }
}

fn choose_target(policy: ClipboardNegotiation, atoms: Atoms, targets: &[Atom]) -> Option<Atom> {
    let names = targets.iter().filter_map(|atom| atoms.name(*atom));
    choose_host_format(
        |kind| policy.allows(ClipboardFlow::HostToClient, kind),
        names,
    )
    .and_then(|(name, _)| atoms.atom(name))
}

fn intern(connection: &RustConnection, name: &[u8]) -> Result<Atom, String> {
    connection
        .intern_atom(false, name)
        .map_err(|error| format!("intern X11 clipboard atom: {error}"))?
        .reply()
        .map(|reply| reply.atom)
        .map_err(|error| format!("read X11 clipboard atom: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn negotiation(max_bytes: usize) -> ClipboardNegotiation {
        ClipboardNegotiation::resolve(
            arcen_media::clipboard::ClipboardPolicy::new(
                arcen_media::clipboard::ClipboardDirection::Both,
                arcen_media::clipboard::ClipboardContent::All,
                max_bytes,
            )
            .unwrap(),
            true,
            arcen_media::clipboard::ClipboardRequest {
                protocol_version: arcen_protocol::messages::CLIPBOARD_PROTOCOL_VERSION,
                text: arcen_media::clipboard::ClipboardDirections::both(),
                image: arcen_media::clipboard::ClipboardDirections::both(),
            },
        )
        .unwrap()
    }

    /// A second X client that owns CLIPBOARD and answers UTF8_STRING with an
    /// INCR transfer of `chunks` × `chunk` bytes, or, when `chunks` is zero,
    /// with `small` in one property. It serves every requestor separately:
    /// on a real desktop the session's clipboard manager converts each new
    /// owner too.
    struct Owner {
        connection: RustConnection,
        window: Window,
        clipboard: Atom,
        targets: Atom,
        utf8: Atom,
        incr: Atom,
        sending: std::collections::HashMap<(Window, Atom), usize>,
        finished: std::collections::HashSet<Window>,
    }

    impl Owner {
        fn new() -> Self {
            let (connection, screen_index) = x11rb::connect(None).expect("owner X11 client");
            let screen = connection.setup().roots[screen_index].clone();
            let window = connection.generate_id().unwrap();
            connection
                .create_window(
                    u8::try_from(COPY_FROM_PARENT).unwrap_or(0),
                    window,
                    screen.root,
                    0,
                    0,
                    1,
                    1,
                    0,
                    WindowClass::INPUT_OUTPUT,
                    0,
                    &CreateWindowAux::new(),
                )
                .unwrap()
                .check()
                .unwrap();
            let clipboard = intern(&connection, b"CLIPBOARD").unwrap();
            let targets = intern(&connection, b"TARGETS").unwrap();
            let utf8 = intern(&connection, b"UTF8_STRING").unwrap();
            let incr = intern(&connection, b"INCR").unwrap();
            Self {
                connection,
                window,
                clipboard,
                targets,
                utf8,
                incr,
                sending: std::collections::HashMap::new(),
                finished: std::collections::HashSet::new(),
            }
        }

        fn own(&self) {
            self.connection
                .set_selection_owner(self.window, self.clipboard, CURRENT_TIME)
                .unwrap()
                .check()
                .unwrap();
            self.connection.flush().unwrap();
        }

        fn serve(&mut self, chunks: usize, chunk: usize, small: &[u8]) {
            while let Some(event) = self.connection.poll_for_event().unwrap() {
                match event {
                    Event::SelectionRequest(request) => {
                        let property = request.property;
                        if request.target == self.targets {
                            self.connection
                                .change_property32(
                                    PropMode::REPLACE,
                                    request.requestor,
                                    property,
                                    AtomEnum::ATOM,
                                    &[self.targets, self.utf8],
                                )
                                .unwrap();
                        } else if chunks == 0 {
                            self.connection
                                .change_property8(
                                    PropMode::REPLACE,
                                    request.requestor,
                                    property,
                                    self.utf8,
                                    small,
                                )
                                .unwrap();
                        } else {
                            self.connection
                                .change_window_attributes(
                                    request.requestor,
                                    &ChangeWindowAttributesAux::new()
                                        .event_mask(EventMask::PROPERTY_CHANGE),
                                )
                                .unwrap();
                            self.connection
                                .change_property32(
                                    PropMode::REPLACE,
                                    request.requestor,
                                    property,
                                    self.incr,
                                    &[u32::try_from(chunks * chunk).unwrap()],
                                )
                                .unwrap();
                            self.sending.insert((request.requestor, property), 0);
                        }
                        let notify = SelectionNotifyEvent {
                            response_type: SELECTION_NOTIFY_EVENT,
                            sequence: 0,
                            time: request.time,
                            requestor: request.requestor,
                            selection: request.selection,
                            target: request.target,
                            property,
                        };
                        self.connection
                            .send_event(false, request.requestor, EventMask::NO_EVENT, notify)
                            .unwrap();
                    }
                    Event::PropertyNotify(event) if event.state == Property::DELETE => {
                        let key = (event.window, event.atom);
                        if let Some(sent) = self.sending.get(&key).copied() {
                            let data = if sent < chunks {
                                vec![b'B'; chunk]
                            } else {
                                Vec::new()
                            };
                            self.connection
                                .change_property8(
                                    PropMode::REPLACE,
                                    event.window,
                                    event.atom,
                                    self.utf8,
                                    &data,
                                )
                                .unwrap();
                            if data.is_empty() {
                                self.sending.remove(&key);
                                self.finished.insert(event.window);
                            } else {
                                self.sending.insert(key, sent + 1);
                            }
                        }
                    }
                    _ => {}
                }
            }
            self.connection.flush().unwrap();
        }
    }

    fn drain_queue(queue: &ClipboardWriterQueue) -> Vec<Vec<u8>> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut payloads = Vec::new();
        runtime.block_on(async {
            let mut current = Vec::new();
            while let Ok(Ok(Some(message))) =
                tokio::time::timeout(Duration::from_millis(200), queue.pop()).await
            {
                if let Message::Binary(frame) = message {
                    let (header, payload) = decode_clipboard_chunk(&frame).unwrap();
                    current.extend_from_slice(payload);
                    if current.len() == header.total_size as usize {
                        payloads.push(std::mem::take(&mut current));
                    }
                }
            }
        });
        payloads
    }

    #[test]
    #[ignore = "requires an X server (DISPLAY)"]
    fn a_refused_incr_is_drained_and_never_leaks_into_the_next_copy() {
        const CHUNK: usize = 64 * 1024;
        let policy = negotiation(CHUNK);
        let (mut worker, _, _) = X11Worker::connect().expect("worker X11 client");
        let remote = ClipboardMailbox::new();
        let local = ClipboardWriterQueue::new();
        let mut owner = Owner::new();
        // A copy declared four times the policy: refused at the INCR header.
        owner.own();
        let deadline = Instant::now() + Duration::from_secs(5);
        while (!owner.finished.contains(&worker.window) || !worker.draining.is_empty())
            && Instant::now() < deadline
        {
            worker
                .step(policy, &remote, &local, Instant::now())
                .unwrap();
            owner.serve(4, CHUNK, b"");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            owner.finished.contains(&worker.window),
            "the refused transfer is drained to its zero-length end"
        );
        assert!(worker.draining.is_empty());
        // The next copy is small and arrives alone, unmixed.
        let small = b"small copy after a refused one";
        owner.own();
        let deadline = Instant::now() + Duration::from_secs(5);
        while worker.next_sequence == 1 && Instant::now() < deadline {
            worker
                .step(policy, &remote, &local, Instant::now())
                .unwrap();
            owner.serve(0, CHUNK, small);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(drain_queue(&local), vec![small.to_vec()]);
    }

    #[test]
    #[ignore = "requires an X server (DISPLAY)"]
    fn a_full_property_ring_refuses_the_copy_and_the_worker_lives_on() {
        let policy = negotiation(64 * 1024);
        let (mut worker, _, _) = X11Worker::connect().expect("worker X11 client");
        let remote = ClipboardMailbox::new();
        let local = ClipboardWriterQueue::new();
        // Every ring property is still draining an interrupted transfer.
        for _ in 0..PROPERTY_RING {
            let property = worker.next_property().unwrap().expect("a free property");
            worker.drain(property);
        }
        assert_eq!(worker.next_property().unwrap(), None);
        // A further owner change is refused, not fatal.
        let mut owner = Owner::new();
        owner.own();
        let deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < deadline {
            worker
                .step(policy, &remote, &local, Instant::now())
                .expect("a full ring never ends the worker");
            owner.serve(0, 0, b"refused while busy");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(worker.pending_target.is_none());
        assert_eq!(worker.next_sequence, 1);
        // Once the drains expire the ring frees up and copies flow again.
        let later = Instant::now() + INCR_TIMEOUT;
        worker.step(policy, &remote, &local, later).unwrap();
        assert!(worker.draining.is_empty());
        let small = b"after the ring freed up";
        owner.own();
        let deadline = Instant::now() + Duration::from_secs(5);
        while worker.next_sequence == 1 && Instant::now() < deadline {
            worker
                .step(policy, &remote, &local, Instant::now())
                .unwrap();
            owner.serve(0, 0, small);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(drain_queue(&local), vec![small.to_vec()]);
    }

    #[test]
    #[ignore = "requires an X server (DISPLAY)"]
    fn an_owner_change_mid_incr_drains_the_old_transfer() {
        const CHUNK: usize = 64 * 1024;
        let policy = negotiation(8 * CHUNK);
        let (mut worker, _, _) = X11Worker::connect().expect("worker X11 client");
        let remote = ClipboardMailbox::new();
        let local = ClipboardWriterQueue::new();
        let mut stale = Owner::new();
        stale.own();
        // Accepted INCR: let one chunk through, then a new owner takes over.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !stale
            .sending
            .iter()
            .any(|((window, _), sent)| *window == worker.window && *sent >= 1)
            && Instant::now() < deadline
        {
            worker
                .step(policy, &remote, &local, Instant::now())
                .unwrap();
            stale.serve(6, CHUNK, b"");
            std::thread::sleep(Duration::from_millis(5));
        }
        let mut fresh = Owner::new();
        let small = b"the new owner's copy";
        fresh.own();
        let deadline = Instant::now() + Duration::from_secs(5);
        while (worker.next_sequence == 1
            || !stale.finished.contains(&worker.window)
            || !worker.draining.is_empty())
            && Instant::now() < deadline
        {
            worker
                .step(policy, &remote, &local, Instant::now())
                .unwrap();
            stale.serve(6, CHUNK, b"");
            fresh.serve(0, CHUNK, small);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            stale.finished.contains(&worker.window),
            "the abandoned transfer was drained"
        );
        assert!(worker.draining.is_empty());
        assert_eq!(drain_queue(&local), vec![small.to_vec()]);
    }

    #[test]
    #[ignore = "requires xvfb-run"]
    fn x11_two_client_owner_notification_and_bounded_target_choice() {
        let (worker, major, _) = X11Worker::connect().expect("first X11 client");
        assert!(major >= 5);
        let (other, screen_index) = x11rb::connect(None).expect("second X11 client");
        let screen = &other.setup().roots[screen_index];
        let owner = other.generate_id().expect("owner id");
        other
            .create_window(
                u8::try_from(COPY_FROM_PARENT).unwrap_or(0),
                owner,
                screen.root,
                0,
                0,
                1,
                1,
                0,
                WindowClass::INPUT_OUTPUT,
                0,
                &CreateWindowAux::new(),
            )
            .expect("create owner")
            .check()
            .expect("owner checked");
        other
            .set_selection_owner(owner, worker.atoms.clipboard, CURRENT_TIME)
            .expect("set owner")
            .check()
            .expect("owner checked");
        other.flush().expect("owner flush");

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(Event::XfixesSelectionNotify(event)) =
                worker.connection.poll_for_event().expect("poll event")
            {
                assert_eq!(event.owner, owner);
                break;
            }
            assert!(Instant::now() < deadline, "owner notification timed out");
            std::thread::sleep(Duration::from_millis(10));
        }

        let policy = ClipboardNegotiation::resolve(
            arcen_media::clipboard::ClipboardPolicy::default(),
            true,
            arcen_media::clipboard::ClipboardRequest {
                protocol_version: arcen_protocol::messages::CLIPBOARD_PROTOCOL_VERSION,
                text: arcen_media::clipboard::ClipboardDirections::both(),
                image: arcen_media::clipboard::ClipboardDirections::both(),
            },
        )
        .unwrap();
        assert_eq!(
            choose_target(policy, worker.atoms, &[worker.atoms.png, worker.atoms.utf8]),
            Some(worker.atoms.utf8)
        );
    }
}
