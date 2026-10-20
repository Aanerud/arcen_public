//! A test Deck that speaks the lane transport (ADR 0014) the way the real
//! Deck does, and presents the session as one message stream so the
//! end-to-end tests read like a session: control on the control lane, video
//! and audio from their own lanes, input and bulk on the lanes the Deck
//! opens, microphone on its own lane.

#![allow(dead_code, clippy::expect_used)]

use std::pin::Pin;
use std::task::{Context, Poll};

use arcen_protocol::lanes::{
    LaneAttachmentMsg, LaneEndpoint, LaneRole, SessionClose, SessionRecord,
    lane_for_binary_message, lane_for_session_text,
};
use arcen_transport::quic::{
    LaneConnection, LaneRuntimeConfig, LaneSender, LocalLaneWriter, PeerLaneEvent, PeerLaneReader,
    SessionLane, SessionLaneReader, SessionLaneSocket, accept_attachment, connect_session_lanes,
    session_record_bytes,
};
use futures_util::{Sink, SinkExt as _, Stream, StreamExt as _};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::{Error as WebSocketError, Message};

type Incoming = mpsc::UnboundedSender<Result<Message, WebSocketError>>;

/// The session as the tests see it.
pub struct LaneClient {
    outgoing: mpsc::UnboundedSender<Message>,
    incoming: mpsc::UnboundedReceiver<Result<Message, WebSocketError>>,
    lanes: LaneConnection,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for LaneClient {
    fn drop(&mut self) {
        self.task.abort();
        self.lanes
            .connection()
            .close(0_u32.into(), b"test client closed");
    }
}

impl LaneClient {
    /// Dials the Pier and starts the client's lane workers.
    ///
    /// # Panics
    ///
    /// When the connection or its control lane cannot be made.
    pub async fn connect(
        endpoint: quinn::Endpoint,
        client_config: quinn::ClientConfig,
        remote_addr: std::net::SocketAddr,
    ) -> Self {
        let (lanes, control) = connect_session_lanes(
            arcen_transport::quic::DirectQuicDialParams {
                endpoint: endpoint.clone(),
                client_config,
                remote_addr,
                server_name: "localhost",
            },
            LaneRuntimeConfig::new(LaneEndpoint::Deck),
        )
        .await
        .expect("connect over lanes");
        let socket = SessionLaneSocket::new(lanes.clone(), control);
        let (outgoing_tx, outgoing_rx) = mpsc::unbounded_channel();
        let (incoming_tx, incoming_rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(run(socket, endpoint, outgoing_rx, incoming_tx));
        Self {
            outgoing: outgoing_tx,
            incoming: incoming_rx,
            lanes,
            task,
        }
    }

    /// The connection's lanes.
    pub fn lanes(&self) -> &LaneConnection {
        &self.lanes
    }

    /// Closes the session on the control lane, as a WebSocket close does.
    ///
    /// # Errors
    ///
    /// When the client task has stopped.
    pub async fn close(
        &mut self,
        frame: Option<CloseFrame<'static>>,
    ) -> Result<(), WebSocketError> {
        self.send(Message::Close(frame)).await
    }
}

fn record_message(record: SessionRecord) -> Message {
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

fn message_record(message: Message) -> Option<SessionRecord> {
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

fn io_error(detail: String) -> WebSocketError {
    WebSocketError::Io(std::io::Error::new(
        std::io::ErrorKind::ConnectionAborted,
        detail,
    ))
}

/// Lanes the client opened once the attachment was granted.
struct Granted {
    input: LaneSender,
    bulk: LaneSender,
    microphone: Option<LocalLaneWriter>,
}

async fn run(
    mut socket: SessionLaneSocket,
    _endpoint: quinn::Endpoint,
    mut outgoing: mpsc::UnboundedReceiver<Message>,
    incoming: Incoming,
) {
    let lanes = socket.lanes().clone();
    let mut granted: Option<Granted> = None;
    let mut held: Vec<Message> = Vec::new();
    let mut workers = Workers(Vec::new());
    loop {
        tokio::select! {
            record = socket.next() => match record {
                None => return,
                Some(Err(exit)) => {
                    let detail = match &exit {
                        arcen_transport::quic::LaneExit::ConnectionLost { mismatch: Some(mismatch) } => {
                            mismatch.message()
                        }
                        other => format!("control lane ended: {other:?}"),
                    };
                    let _ = incoming.send(Err(io_error(detail)));
                    return;
                }
                Some(Ok(SessionRecord::Text(text)))
                    if text.contains(arcen_protocol::lanes::LANE_ATTACHMENT_MSG_TYPE) =>
                {
                    let message: LaneAttachmentMsg =
                        serde_json::from_str(&text).expect("lane_attachment");
                    accept_attachment(&lanes, &message).expect("attachment accepted");
                    for role in [LaneRole::Video, LaneRole::AudioOutput] {
                        workers.0.push(tokio::spawn(media_reader(lanes.clone(), role, incoming.clone())));
                    }
                    let input = open(&lanes, LaneRole::InputCursor).await;
                    let bulk = open(&lanes, LaneRole::BulkAux).await;
                    workers.0.push(tokio::spawn(session_reader(input.reader, incoming.clone())));
                    workers.0.push(tokio::spawn(session_reader(bulk.reader, incoming.clone())));
                    let mut lanes_granted = Granted {
                        input: input.sender,
                        bulk: bulk.sender,
                        microphone: None,
                    };
                    for message in held.drain(..) {
                        send_on_lanes(&mut lanes_granted, &mut socket, message).await;
                    }
                    granted = Some(lanes_granted);
                }
                Some(Ok(record)) => {
                    let _ = incoming.send(Ok(record_message(record)));
                }
            },
            message = outgoing.recv() => {
                let Some(message) = message else {
                    return;
                };
                match granted.as_mut() {
                    Some(granted) => send_on_lanes(granted, &mut socket, message).await,
                    None if is_control(&message) => {
                        if let Some(record) = message_record(message) {
                            let _ = socket.send(record).await;
                        }
                    }
                    None => held.push(message),
                }
            }
        }
    }
}

/// Aborts the client's lane workers when its task ends or is aborted.
struct Workers(Vec<tokio::task::JoinHandle<()>>);

impl Drop for Workers {
    fn drop(&mut self) {
        for worker in &self.0 {
            worker.abort();
        }
    }
}

fn is_control(message: &Message) -> bool {
    match message {
        Message::Text(text) => lane_for_session_text(text) == LaneRole::Control,
        Message::Binary(bytes) => {
            lane_for_binary_message(bytes).unwrap_or(LaneRole::Control) == LaneRole::Control
        }
        _ => true,
    }
}

async fn open(lanes: &LaneConnection, role: LaneRole) -> SessionLane {
    SessionLane::split(lanes.open_lane(role).await.expect("open lane")).expect("session lane")
}

async fn send_on_lanes(granted: &mut Granted, socket: &mut SessionLaneSocket, message: Message) {
    let role = match &message {
        Message::Text(text) => lane_for_session_text(text),
        Message::Binary(bytes) => lane_for_binary_message(bytes).unwrap_or(LaneRole::Control),
        _ => LaneRole::Control,
    };
    let Some(record) = message_record(message) else {
        return;
    };
    match role {
        LaneRole::InputCursor => {
            let _ = granted.input.send(session_record_bytes(&record)).await;
        }
        LaneRole::BulkAux => {
            let _ = granted.bulk.send(session_record_bytes(&record)).await;
        }
        LaneRole::Microphone => {
            let SessionRecord::Binary(frame) = record else {
                return;
            };
            if granted.microphone.is_none() {
                granted.microphone = LocalLaneWriter::open(socket.lanes(), LaneRole::Microphone)
                    .await
                    .ok();
            }
            if let Some(writer) = granted.microphone.as_mut() {
                let _ = writer.try_send(frame.into()).await;
            }
        }
        _ => {
            let _ = socket.send(record).await;
        }
    }
}

async fn session_reader(mut reader: SessionLaneReader, incoming: Incoming) {
    while let Ok(record) = reader.next_record().await {
        if incoming.send(Ok(record_message(record))).is_err() {
            return;
        }
    }
}

async fn media_reader(lanes: LaneConnection, role: LaneRole, incoming: Incoming) {
    let mut reader = PeerLaneReader::new(&lanes, role);
    while let Ok(event) = reader.next().await {
        if let PeerLaneEvent::Frame(frame) = event {
            if incoming.send(Ok(Message::Binary(frame.to_vec()))).is_err() {
                return;
            }
        }
    }
}

impl Stream for LaneClient {
    type Item = Result<Message, WebSocketError>;

    fn poll_next(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.get_mut().incoming.poll_recv(context)
    }
}

impl Sink<Message> for LaneClient {
    type Error = WebSocketError;

    fn poll_ready(
        self: Pin<&mut Self>,
        _context: &mut Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn start_send(self: Pin<&mut Self>, item: Message) -> Result<(), Self::Error> {
        self.outgoing
            .send(item)
            .map_err(|_| io_error("test client stopped".to_owned()))
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        _context: &mut Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(
        self: Pin<&mut Self>,
        _context: &mut Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }
}
