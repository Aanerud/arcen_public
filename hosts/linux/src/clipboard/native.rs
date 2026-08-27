use super::{
    AgentPolicy, ClipboardAgentReady, ClipboardItem, ClipboardMailbox, ClipboardWriterQueue,
    CLIPBOARD_POLICY_TYPE,
};
use arcen_media::clipboard::{
    validate_png, ClipboardFlow, ClipboardKind, ClipboardPolicy, ImageLimits,
};
use arcen_protocol::clipboard::ClipboardReassembler;
use arcen_protocol::messages::{ClipboardContentKind, ClipboardDataMsg, CLIPBOARD_DATA};
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

    let mut config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default();
    config.max_message_size = Some(arcen_protocol::CLIPBOARD_HEADER_SIZE + CHUNK_BYTES);
    config.max_frame_size = Some(arcen_protocol::CLIPBOARD_HEADER_SIZE + CHUNK_BYTES);
    let io = StdioIo {
        stdin: tokio::io::stdin(),
        stdout,
    };
    let mut websocket = WebSocketStream::from_raw_socket(io, Role::Server, Some(config)).await;
    let policy = match websocket.next().await {
        Some(Ok(Message::Text(text))) => {
            let message: AgentPolicy = serde_json::from_str(&text)
                .map_err(|_| "clipboard policy message is invalid".to_string())?;
            if message.msg_type != CLIPBOARD_POLICY_TYPE || !message.policy.is_v1() {
                return Err("clipboard policy version is invalid".to_string());
            }
            wire_policy(message.policy)?
        }
        _ => return Err("clipboard policy message is missing".to_string()),
    };

    let remote = ClipboardMailbox::new();
    let local = ClipboardWriterQueue::new();
    let worker_remote = Arc::clone(&remote);
    let worker_local = Arc::clone(&local);
    let mut worker_task =
        tokio::task::spawn_blocking(move || worker.run(policy, worker_remote, worker_local));
    let mut worker_finished = None;
    let mut reassembler = ClipboardReassembler::new(policy.max_bytes)
        .map_err(|error| format!("clipboard child reassembler: {error}"))?;
    let result = loop {
        tokio::select! {
            incoming = websocket.next() => match incoming {
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
                    if policy
                        .check_size(
                            ClipboardFlow::ClientToHost,
                            media_kind(offer.kind),
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
                    if validate_item(policy, completed.kind, &completed.bytes) {
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
                Some(Ok(Message::Ping(payload))) => {
                    if websocket.send(Message::Pong(payload)).await.is_err() {
                        break Err("clipboard child pong failed".to_string());
                    }
                }
                Some(Ok(_)) => {}
                Some(Err(error)) => break Err(format!("clipboard child IPC receive: {error}")),
            },
            outbound = local.pop() => match outbound {
                Ok(Some(message)) => {
                    if websocket.send(message).await.is_err() {
                        break Err("clipboard child IPC send failed".to_string());
                    }
                }
                Ok(None) => break Ok(()),
                Err(error) => break Err(error),
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

fn wire_policy(
    message: arcen_protocol::messages::ClipboardPolicyMsg,
) -> Result<ClipboardPolicy, String> {
    use arcen_media::clipboard::{ClipboardContent, ClipboardDirection};
    let direction = match message.direction {
        arcen_protocol::messages::ClipboardDirectionMsg::Both => ClipboardDirection::Both,
        arcen_protocol::messages::ClipboardDirectionMsg::ClientToHost => {
            ClipboardDirection::ClientToHost
        }
        arcen_protocol::messages::ClipboardDirectionMsg::HostToClient => {
            ClipboardDirection::HostToClient
        }
        arcen_protocol::messages::ClipboardDirectionMsg::Disabled => ClipboardDirection::Disabled,
    };
    let content = match message.content {
        arcen_protocol::messages::ClipboardContentMsg::All => ClipboardContent::All,
        arcen_protocol::messages::ClipboardContentMsg::Text => ClipboardContent::Text,
        arcen_protocol::messages::ClipboardContentMsg::Image => ClipboardContent::Image,
    };
    ClipboardPolicy::new(
        direction,
        content,
        usize::try_from(message.max_bytes).map_err(|_| "clipboard max overflow")?,
    )
    .map_err(|error| error.to_string())
}

fn media_kind(kind: ClipboardContentKind) -> ClipboardKind {
    match kind {
        ClipboardContentKind::TextUtf8 => ClipboardKind::TextUtf8,
        ClipboardContentKind::ImagePng => ClipboardKind::ImagePng,
    }
}

fn validate_item(policy: ClipboardPolicy, kind: ClipboardContentKind, bytes: &[u8]) -> bool {
    if policy
        .check_size(ClipboardFlow::ClientToHost, media_kind(kind), bytes.len())
        .is_err()
    {
        return false;
    }
    match kind {
        ClipboardContentKind::TextUtf8 => std::str::from_utf8(bytes).is_ok(),
        ClipboardContentKind::ImagePng => validate_png(
            bytes,
            ImageLimits {
                max_encoded_bytes: policy.max_bytes,
                ..ImageLimits::default()
            },
        )
        .is_ok(),
    }
}

#[derive(Clone, Copy)]
struct Atoms {
    clipboard: Atom,
    targets: Atom,
    timestamp: Atom,
    utf8: Atom,
    text_plain: Atom,
    text: Atom,
    string: Atom,
    png: Atom,
    incr: Atom,
    property: Atom,
}

struct SendIncr {
    requestor: Window,
    property: Atom,
    target: Atom,
    transfer: super::IncrTransfer,
}

struct ReceiveIncr {
    target: Atom,
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
}

struct X11Worker {
    connection: RustConnection,
    window: Window,
    atoms: Atoms,
    owner_time: u32,
    remote_item: Option<ClipboardItem>,
    pending_target: Option<PendingConversion>,
    send_incr: Option<SendIncr>,
    receive_incr: Option<ReceiveIncr>,
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
            text_plain: intern(&connection, b"text/plain;charset=utf-8")?,
            text: intern(&connection, b"TEXT")?,
            string: AtomEnum::STRING.into(),
            png: intern(&connection, b"image/png")?,
            incr: intern(&connection, b"INCR")?,
            property: intern(&connection, b"_ARCEN_CLIPBOARD")?,
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
                next_sequence: 1,
                claim_pending: false,
            },
            version.major_version,
            version.minor_version,
        ))
    }

    fn run(
        mut self,
        policy: ClipboardPolicy,
        remote: Arc<ClipboardMailbox>,
        local: Arc<ClipboardWriterQueue>,
    ) -> Result<(), String> {
        loop {
            while let Some(event) = self
                .connection
                .poll_for_event()
                .map_err(|error| format!("poll X11 clipboard event: {error}"))?
            {
                self.handle_event(event, policy, &local)?;
            }
            if let Some(item) = remote.try_take() {
                self.claim_remote(item)?;
            }
            let now = Instant::now();
            if self
                .send_incr
                .as_ref()
                .is_some_and(|state| state.transfer.expired(now))
            {
                self.send_incr = None;
            }
            if self.receive_incr.as_ref().is_some_and(|state| {
                now.saturating_duration_since(state.last_progress) >= INCR_TIMEOUT
            }) {
                self.receive_incr = None;
            }
            if remote.is_closed() {
                break;
            }
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

    fn claim_remote(&mut self, item: ClipboardItem) -> Result<(), String> {
        self.remote_item = Some(item);
        self.pending_target = None;
        self.send_incr = None;
        self.receive_incr = None;
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
        policy: ClipboardPolicy,
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
                    self.receive_incr = None;
                    if event.owner != x11rb::NONE {
                        self.pending_target = Some(PendingConversion {
                            target: self.atoms.targets,
                            owner: event.owner,
                            time: event.timestamp,
                        });
                        self.connection
                            .convert_selection(
                                self.window,
                                self.atoms.clipboard,
                                self.atoms.targets,
                                self.atoms.property,
                                event.timestamp,
                            )
                            .map_err(|error| format!("request X11 TARGETS: {error}"))?;
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
                    self.receive_incr_chunk(policy, local)?;
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
        policy: ClipboardPolicy,
        local: &ClipboardWriterQueue,
    ) -> Result<(), String> {
        let Some(pending) = self.pending_target else {
            return Ok(());
        };
        if event.time != pending.time {
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
                self.pending_target = Some(PendingConversion { target, ..pending });
                self.connection
                    .convert_selection(
                        self.window,
                        self.atoms.clipboard,
                        target,
                        self.atoms.property,
                        pending.time,
                    )
                    .map_err(|error| format!("request X11 clipboard target: {error}"))?;
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
                .and_then(|value| usize::try_from(value).ok())
                .filter(|value| *value <= policy.max_bytes)
                .ok_or_else(|| "invalid X11 INCR declaration".to_string())?;
            self.receive_incr = Some(ReceiveIncr {
                target,
                declared,
                bytes: Vec::new(),
                last_progress: Instant::now(),
            });
            return Ok(());
        }
        if reply.type_ != target || reply.bytes_after != 0 {
            return Err("X11 selection property metadata mismatch".to_string());
        }
        self.finish_local(target, reply.value, policy, local)
    }

    fn receive_incr_chunk(
        &mut self,
        policy: ClipboardPolicy,
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
                self.atoms.property,
                AtomEnum::ANY,
                0,
                u32::try_from(CHUNK_BYTES / 4).unwrap_or(u32::MAX),
            )
            .map_err(|error| format!("read X11 INCR property: {error}"))?
            .reply()
            .map_err(|error| format!("reply X11 INCR property: {error}"))?;
        if reply.bytes_after != 0 {
            return Err("X11 INCR property chunk exceeds bound".to_string());
        }
        if reply.value.is_empty() {
            if receive.bytes.len() < receive.declared {
                return Err("X11 INCR size mismatch".to_string());
            }
            let bytes = std::mem::take(&mut receive.bytes);
            return self.finish_local(receive.target, bytes, policy, local);
        }
        let next = receive
            .bytes
            .len()
            .checked_add(reply.value.len())
            .ok_or_else(|| "X11 INCR size overflow".to_string())?;
        if reply.value.len() > CHUNK_BYTES || next > policy.max_bytes {
            return Err("X11 INCR receive exceeds bound".to_string());
        }
        receive
            .bytes
            .try_reserve(reply.value.len())
            .map_err(|_| "X11 INCR allocation failed".to_string())?;
        receive.bytes.extend_from_slice(&reply.value);
        receive.last_progress = Instant::now();
        self.receive_incr = Some(receive);
        Ok(())
    }

    fn finish_local(
        &mut self,
        target: Atom,
        bytes: Vec<u8>,
        policy: ClipboardPolicy,
        local: &ClipboardWriterQueue,
    ) -> Result<(), String> {
        let (kind, bytes) = if target == self.atoms.string {
            let text = bytes
                .iter()
                .map(|byte| char::from(*byte))
                .collect::<String>();
            (ClipboardContentKind::TextUtf8, text.into_bytes())
        } else if target == self.atoms.png {
            (ClipboardContentKind::ImagePng, bytes)
        } else {
            (ClipboardContentKind::TextUtf8, bytes)
        };
        if policy
            .check_size(ClipboardFlow::HostToClient, media_kind(kind), bytes.len())
            .is_err()
            || match kind {
                ClipboardContentKind::TextUtf8 => std::str::from_utf8(&bytes).is_err(),
                ClipboardContentKind::ImagePng => validate_png(
                    &bytes,
                    ImageLimits {
                        max_encoded_bytes: policy.max_bytes,
                        ..ImageLimits::default()
                    },
                )
                .is_err(),
            }
        {
            return Ok(());
        }
        let sequence = self.next_sequence;
        self.next_sequence = self
            .next_sequence
            .checked_add(1)
            .ok_or_else(|| "X11 clipboard sequence exhausted".to_string())?;
        if let Some(item) = ClipboardItem::new(sequence, kind, bytes, false) {
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
            let supported = match item.kind {
                ClipboardContentKind::TextUtf8 => {
                    request.target == self.atoms.utf8
                        || request.target == self.atoms.text_plain
                        || request.target == self.atoms.text
                        || (request.target == self.atoms.string
                            && latin1_from_utf8(&item.bytes).is_some())
                }
                ClipboardContentKind::ImagePng => request.target == self.atoms.png,
            };
            if supported {
                let payload = if request.target == self.atoms.string {
                    latin1_from_utf8(&item.bytes)
                        .ok_or_else(|| "X11 STRING conversion failed".to_string())?
                } else {
                    item.bytes.clone()
                };
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
            match item.kind {
                ClipboardContentKind::TextUtf8 => {
                    targets.extend([self.atoms.utf8, self.atoms.text_plain, self.atoms.text])
                }
                ClipboardContentKind::ImagePng => targets.push(self.atoms.png),
            }
            if item.kind == ClipboardContentKind::TextUtf8
                && latin1_from_utf8(&item.bytes).is_some()
            {
                targets.push(self.atoms.string);
            }
        }
        targets
    }
}

fn latin1_from_utf8(bytes: &[u8]) -> Option<Vec<u8>> {
    std::str::from_utf8(bytes)
        .ok()?
        .chars()
        .map(|character| u8::try_from(u32::from(character)).ok())
        .collect()
}

fn choose_target(policy: ClipboardPolicy, atoms: Atoms, targets: &[Atom]) -> Option<Atom> {
    if policy.allows(ClipboardFlow::HostToClient, ClipboardKind::TextUtf8) {
        for target in [atoms.utf8, atoms.text_plain, atoms.text, atoms.string] {
            if targets.contains(&target) {
                return Some(target);
            }
        }
    }
    (policy.allows(ClipboardFlow::HostToClient, ClipboardKind::ImagePng)
        && targets.contains(&atoms.png))
    .then_some(atoms.png)
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

        let policy = ClipboardPolicy::default();
        assert_eq!(
            choose_target(policy, worker.atoms, &[worker.atoms.png, worker.atoms.utf8]),
            Some(worker.atoms.utf8)
        );
    }
}
