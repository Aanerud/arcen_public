//! The Deck's lane adapter (ADR 0014): the control lane as the session's
//! message `Sink`/`Stream`, and one worker per other lane.
//!
//! - Video: its own reader feeding the video inbox only.
//! - Audio output: its own reader feeding the audio worker's queue only.
//! - Input/cursor: opened by the Deck; input records go out here with
//!   `try_send` (an ordered outbox keeps every edge, replaces only a
//!   trailing position of the same kind and sums trailing relative deltas),
//!   cursor messages come back here.
//! - Bulk: opened by the Deck; clipboard, HID and USB in both directions.
//! - Microphone: opened by the Deck when capture starts.
//!
//! None of these workers waits on another lane, and none runs inside the
//! session's main loop.

use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use arcen_protocol::lanes::{
    lane_for_binary_message, LaneAttachmentMsg, LaneRole, SessionClose, SessionRecord,
    LANE_ATTACHMENT_MSG_TYPE,
};
use arcen_transport::lanes::LaneFailure;
use arcen_transport::quic::{
    accept_attachment, follow_lane_exit, session_record_bytes, LaneConnection, LaneRecoveryStep,
    LaneSendError, LaneTerminal, LaneWriteOutcome, LocalLaneWriter, PeerLaneEvent, PeerLaneReader,
    SessionLane, SessionLaneSocket,
};
use bytes::Bytes;
use futures_util::{Sink, Stream};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::{Error as WebSocketError, Message};

use super::websocket::SessionEvent;
use crate::pipeline::frame_queue::IncomingMediaSender;

/// Bulk messages waiting for the bulk lane worker, in either direction.
const BULK_CHANNEL: usize = 64;
/// How often a full input lane is retried.
const INPUT_RETRY: Duration = Duration::from_millis(5);

/// Converts a session message to its lane record. `None` for raw frames.
pub(crate) fn message_to_record(message: Message) -> Option<SessionRecord> {
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

/// Converts a received lane record to the session message it carries.
pub(crate) fn record_to_message(record: SessionRecord) -> Message {
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

/// The lane a session message belongs on; `None` for ping, pong and close,
/// which belong to whichever lane carries them.
pub(crate) fn message_lane(message: &Message) -> Option<LaneRole> {
    match message {
        Message::Text(text) => Some(arcen_protocol::lanes::lane_for_session_text(text)),
        Message::Binary(bytes) => Some(lane_for_binary_message(bytes).unwrap_or(LaneRole::Control)),
        Message::Ping(_) | Message::Pong(_) | Message::Close(_) | Message::Frame(_) => None,
    }
}

fn lane_io_error(kind: std::io::ErrorKind, detail: String) -> WebSocketError {
    WebSocketError::Io(std::io::Error::new(kind, detail))
}

/// A peer that speaks another transport layout, as the error the session
/// reports to the user.
fn exit_error(exit: &arcen_transport::quic::LaneExit) -> WebSocketError {
    match exit {
        arcen_transport::quic::LaneExit::ConnectionLost {
            mismatch: Some(mismatch),
        } => lane_io_error(std::io::ErrorKind::Unsupported, mismatch.message()),
        other => lane_io_error(
            std::io::ErrorKind::ConnectionAborted,
            format!("control lane ended: {other:?}"),
        ),
    }
}

/// The control lane of a Deck connection. Receiving `lane_attachment`
/// authorizes the attachment's lanes; it is consumed here and never reaches
/// the session code.
pub(crate) struct DeckLaneSocket {
    socket: SessionLaneSocket,
    // Keeps the client endpoint (and its UDP socket) alive with the session.
    _endpoint: quinn::Endpoint,
}

impl DeckLaneSocket {
    pub(crate) fn new(socket: SessionLaneSocket, endpoint: quinn::Endpoint) -> Self {
        Self {
            socket,
            _endpoint: endpoint,
        }
    }

    pub(crate) fn lanes(&self) -> &LaneConnection {
        self.socket.lanes()
    }

    fn authorize(&self, text: &str) -> Result<(), WebSocketError> {
        let message: LaneAttachmentMsg = serde_json::from_str(text).map_err(|error| {
            lane_io_error(
                std::io::ErrorKind::InvalidData,
                format!("invalid lane_attachment: {error}"),
            )
        })?;
        accept_attachment(self.socket.lanes(), &message).map_err(|error| {
            lane_io_error(
                std::io::ErrorKind::InvalidData,
                format!("lane_attachment refused: {error}"),
            )
        })?;
        tracing::info!(
            target: crate::logging::target::TRANSPORT,
            roles = ?message.roles,
            "lane attachment authorized"
        );
        Ok(())
    }
}

fn is_lane_attachment(text: &str) -> bool {
    #[derive(serde::Deserialize)]
    struct Typed<'a> {
        #[serde(rename = "type", borrow)]
        msg_type: Option<std::borrow::Cow<'a, str>>,
    }
    serde_json::from_str::<Typed<'_>>(text)
        .ok()
        .and_then(|typed| typed.msg_type)
        .is_some_and(|msg_type| msg_type == LANE_ATTACHMENT_MSG_TYPE)
}

impl Stream for DeckLaneSocket {
    type Item = Result<Message, WebSocketError>;

    fn poll_next(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            match std::task::ready!(Pin::new(&mut this.socket).poll_next(context)) {
                None => return Poll::Ready(None),
                Some(Err(exit)) => return Poll::Ready(Some(Err(exit_error(&exit)))),
                Some(Ok(SessionRecord::Text(text))) if is_lane_attachment(&text) => {
                    if let Err(error) = this.authorize(&text) {
                        return Poll::Ready(Some(Err(error)));
                    }
                }
                Some(Ok(record)) => return Poll::Ready(Some(Ok(record_to_message(record)))),
            }
        }
    }
}

fn send_error(error: LaneSendError) -> WebSocketError {
    lane_io_error(
        std::io::ErrorKind::BrokenPipe,
        format!("control lane: {error}"),
    )
}

impl Sink<Message> for DeckLaneSocket {
    type Error = WebSocketError;

    fn poll_ready(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.get_mut().socket)
            .poll_ready(context)
            .map_err(send_error)
    }

    fn start_send(self: Pin<&mut Self>, item: Message) -> Result<(), Self::Error> {
        match message_to_record(item) {
            Some(record) => Pin::new(&mut self.get_mut().socket)
                .start_send(record)
                .map_err(send_error),
            None => Ok(()),
        }
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.get_mut().socket)
            .poll_flush(context)
            .map_err(send_error)
    }

    fn poll_close(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.get_mut().socket)
            .poll_close(context)
            .map_err(send_error)
    }
}

/// Milliseconds since `epoch` of the newest media-lane arrival, for the
/// session's stall detector.
#[derive(Clone)]
pub(crate) struct LaneActivity {
    epoch: Instant,
    last_ms: Arc<AtomicU64>,
}

impl LaneActivity {
    pub(crate) fn new() -> Self {
        Self {
            epoch: Instant::now(),
            last_ms: Arc::new(AtomicU64::new(0)),
        }
    }

    fn touch(&self) {
        let now = u64::try_from(self.epoch.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.last_ms.fetch_max(now, Ordering::Relaxed);
    }

    /// When a media lane last delivered a frame.
    pub(crate) fn last(&self) -> Instant {
        self.epoch + Duration::from_millis(self.last_ms.load(Ordering::Relaxed))
    }
}

/// The attachment's lane workers. Dropping it aborts every worker.
pub(crate) struct DeckLaneWorkers {
    tasks: Vec<JoinHandle<()>>,
    /// Clipboard, HID and USB messages to send on the bulk lane.
    pub(crate) bulk_out: mpsc::Sender<Message>,
    /// Clipboard and USB messages the Pier sent on the bulk lane.
    pub(crate) bulk_in: mpsc::Receiver<Message>,
    /// Newest media-lane arrival.
    pub(crate) activity: LaneActivity,
    /// Why a worker ended the attachment, when one did.
    pub(crate) ended: mpsc::Receiver<LaneTerminal>,
}

impl Drop for DeckLaneWorkers {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

/// Starts the attachment's lane workers once the control lane authorized it.
pub(crate) fn spawn_deck_lane_workers(
    lanes: &LaneConnection,
    media: IncomingMediaSender,
    events: mpsc::UnboundedSender<SessionEvent>,
    input: mpsc::UnboundedReceiver<Value>,
) -> DeckLaneWorkers {
    let activity = LaneActivity::new();
    let (bulk_out_tx, bulk_out_rx) = mpsc::channel(BULK_CHANNEL);
    let (bulk_in_tx, bulk_in_rx) = mpsc::channel(BULK_CHANNEL);
    let (ended_tx, ended_rx) = mpsc::channel(4);
    let tasks = vec![
        tokio::spawn(video_lane_reader(
            lanes.clone(),
            media.clone(),
            events.clone(),
            activity.clone(),
            ended_tx.clone(),
        )),
        tokio::spawn(audio_lane_reader(
            lanes.clone(),
            media,
            activity.clone(),
            ended_tx.clone(),
        )),
        tokio::spawn(input_lane_worker(
            lanes.clone(),
            input,
            events,
            ended_tx.clone(),
        )),
        tokio::spawn(bulk_lane_worker(
            lanes.clone(),
            bulk_out_rx,
            bulk_in_tx,
            ended_tx,
        )),
    ];
    DeckLaneWorkers {
        tasks,
        bulk_out: bulk_out_tx,
        bulk_in: bulk_in_rx,
        activity,
        ended: ended_rx,
    }
}

/// Tells the session loop how a lane worker stopped. A disabled role is
/// reported too: the loop turns that feature off for the session (bulk:
/// clipboard and Hard USB) instead of ending it.
async fn report_end(ended: &mpsc::Sender<LaneTerminal>, role: LaneRole, terminal: LaneTerminal) {
    match terminal {
        LaneTerminal::Disabled(_) => {
            tracing::warn!(
                target: crate::logging::target::TRANSPORT,
                lane = %role,
                "lane exhausted its recovery; this feature is off for the session"
            );
        }
        other => {
            tracing::info!(
                target: crate::logging::target::TRANSPORT,
                lane = %role,
                reason = %other,
                "lane ended"
            );
        }
    }
    let _ = ended.send(terminal).await;
}

/// After the bulk lane is disabled, bulk messages are refused here, visibly
/// and without failing their sender: the session goes on without clipboard
/// and Hard USB.
async fn refuse_bulk(mut outgoing: mpsc::Receiver<Message>) {
    let mut refused = 0_u64;
    while outgoing.recv().await.is_some() {
        refused += 1;
        if refused == 1 || refused.is_power_of_two() {
            tracing::warn!(
                target: crate::logging::target::TRANSPORT,
                refused,
                "bulk lane disabled; clipboard/USB message not sent"
            );
        }
    }
}

/// Video frames into the video inbox, nothing else. A replacement lane
/// starts a new prediction chain, so presentation recovery is requested.
async fn video_lane_reader(
    lanes: LaneConnection,
    media: IncomingMediaSender,
    events: mpsc::UnboundedSender<SessionEvent>,
    activity: LaneActivity,
    ended: mpsc::Sender<LaneTerminal>,
) {
    let mut reader = PeerLaneReader::new(&lanes, LaneRole::Video);
    loop {
        match reader.next().await {
            Ok(PeerLaneEvent::Opened {
                lane_generation,
                request_keyframe,
                ..
            }) => {
                tracing::debug!(
                    target: crate::logging::target::TRANSPORT,
                    lane_generation,
                    "video lane opened"
                );
                if request_keyframe {
                    let _ = events.send(SessionEvent::PresentationRecoveryRequested);
                }
            }
            Ok(PeerLaneEvent::Lost {
                request_keyframe, ..
            }) => {
                if request_keyframe {
                    let _ = events.send(SessionEvent::PresentationRecoveryRequested);
                }
            }
            Ok(PeerLaneEvent::Frame(frame)) => {
                activity.touch();
                if lane_for_binary_message(&frame) != Some(LaneRole::Video) {
                    tracing::warn!(
                        target: crate::logging::target::TRANSPORT,
                        "non-video frame on the video lane"
                    );
                    reader.fail(LaneFailure::MalformedFrame);
                    continue;
                }
                let outcome = match media.enqueue_bytes(&frame) {
                    Ok(outcome) => outcome,
                    Err(error) => media.record_malformed(&frame, format!("{error:?}")),
                };
                if outcome.notify {
                    let _ = events.send(SessionEvent::MediaReady);
                }
            }
            Err(terminal) => {
                report_end(&ended, LaneRole::Video, terminal).await;
                return;
            }
        }
    }
}

/// Audio frames into the audio worker's queue, nothing else: never waits on
/// video, the video inbox or the session loop.
async fn audio_lane_reader(
    lanes: LaneConnection,
    media: IncomingMediaSender,
    activity: LaneActivity,
    ended: mpsc::Sender<LaneTerminal>,
) {
    let mut reader = PeerLaneReader::new(&lanes, LaneRole::AudioOutput);
    let audio = media.audio_lane().clone();
    loop {
        match reader.next().await {
            Ok(PeerLaneEvent::Opened { .. }) => {
                tracing::info!(
                    target: crate::logging::target::AUDIO,
                    "audio arrives on its own lane"
                );
            }
            Ok(PeerLaneEvent::Lost { .. }) => {}
            Ok(PeerLaneEvent::Frame(frame)) => {
                activity.touch();
                if audio.push_frame(&frame).is_err() {
                    tracing::debug!(
                        target: crate::logging::target::AUDIO,
                        "malformed audio frame dropped"
                    );
                }
            }
            Err(terminal) => {
                report_end(&ended, LaneRole::AudioOutput, terminal).await;
                return;
            }
        }
    }
}

async fn open_session_lane(
    lanes: &LaneConnection,
    role: LaneRole,
) -> Result<SessionLane, LaneTerminal> {
    let lane = match lanes.open_lane(role).await {
        Ok(lane) => lane,
        Err(arcen_transport::quic::LaneOpenFailure::Stream { action, .. }) => {
            let exit = arcen_transport::quic::LaneExit::Failed {
                failure: LaneFailure::WriteFailed,
                action,
            };
            match follow_lane_exit(lanes, &exit).await {
                LaneRecoveryStep::Reopened { lane, .. } => lane,
                step => return Err(terminal_of(lanes, role, step)),
            }
        }
        Err(arcen_transport::quic::LaneOpenFailure::Registry(_)) => {
            return Err(lanes
                .ended()
                .map_or(LaneTerminal::Disabled(role), LaneTerminal::Ended));
        }
    };
    SessionLane::split(lane).map_err(|_| LaneTerminal::Disabled(role))
}

fn terminal_of(lanes: &LaneConnection, role: LaneRole, step: LaneRecoveryStep) -> LaneTerminal {
    match step {
        LaneRecoveryStep::Disabled(role) => LaneTerminal::Disabled(role),
        LaneRecoveryStep::Ended(reason) => LaneTerminal::Ended(reason),
        LaneRecoveryStep::ConnectionLost(mismatch) => LaneTerminal::ConnectionLost(mismatch),
        LaneRecoveryStep::Finished => LaneTerminal::Finished,
        _ => lanes
            .ended()
            .map_or(LaneTerminal::Disabled(role), LaneTerminal::Ended),
    }
}

/// Follows a Deck-opened session lane's exit: the replacement lane, or why
/// the role stopped.
async fn reopen_session_lane(
    lanes: &LaneConnection,
    role: LaneRole,
    exit: &arcen_transport::quic::LaneExit,
) -> Result<SessionLane, LaneTerminal> {
    match follow_lane_exit(lanes, exit).await {
        LaneRecoveryStep::Reopened { lane, .. } => {
            SessionLane::split(lane).map_err(|_| LaneTerminal::Disabled(role))
        }
        // A cancelled lane of a role this Deck opens is reopened by it.
        LaneRecoveryStep::Superseded if lanes.ended().is_none() => {
            open_session_lane(lanes, role).await
        }
        step => Err(terminal_of(lanes, role, step)),
    }
}

/// Sends what the input lane has room for, in order.
fn flush_input(
    sender: &arcen_transport::quic::LaneSender,
    outbox: &mut arcen_input::InputLaneOutbox,
) {
    while let Some(message) = outbox.front() {
        let record = session_record_bytes(&SessionRecord::Text(message.to_string()));
        match sender.try_send(record) {
            Ok(()) => {
                outbox.pop_front();
            }
            Err(LaneSendError::Oversize { len, .. }) => {
                tracing::warn!(
                    target: crate::logging::target::INPUT,
                    len,
                    "oversize input record dropped"
                );
                outbox.pop_front();
            }
            Err(LaneSendError::Full(_) | LaneSendError::Closed(_)) => return,
        }
    }
}

/// Deck input out, host cursor messages in, on the input/cursor lane.
async fn input_lane_worker(
    lanes: LaneConnection,
    mut input: mpsc::UnboundedReceiver<Value>,
    events: mpsc::UnboundedSender<SessionEvent>,
    ended: mpsc::Sender<LaneTerminal>,
) {
    let mut lane = match open_session_lane(&lanes, LaneRole::InputCursor).await {
        Ok(lane) => lane,
        Err(terminal) => {
            report_end(&ended, LaneRole::InputCursor, terminal).await;
            return;
        }
    };
    let mut outbox = arcen_input::InputLaneOutbox::default();
    let mut retry = tokio::time::interval(INPUT_RETRY);
    retry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            biased;
            value = input.recv() => {
                let Some(value) = value else {
                    lane.sender.finish();
                    return;
                };
                outbox.push(value);
                flush_input(&lane.sender, &mut outbox);
            }
            record = lane.reader.next_record() => match record {
                Ok(SessionRecord::Text(text)) => {
                    if let Ok(value) = serde_json::from_str::<Value>(&text) {
                        let _ = events.send(SessionEvent::Json(value));
                    }
                }
                Ok(SessionRecord::Binary(bytes))
                    if bytes.first() == Some(&(crate::protocol::FrameType::CursorUpdate as u8)) =>
                {
                    let _ = events.send(SessionEvent::CursorUpdate(bytes));
                }
                Ok(_) => {}
                Err(exit) => match reopen_session_lane(&lanes, LaneRole::InputCursor, &exit).await {
                    Ok(replacement) => {
                        tracing::info!(
                            target: crate::logging::target::INPUT,
                            "input lane reopened"
                        );
                        // The host forgets which cursor images this Deck
                        // holds when the lane is replaced; so does the Deck.
                        let _ = events.send(SessionEvent::CursorLaneReset);
                        lane = replacement;
                        flush_input(&lane.sender, &mut outbox);
                    }
                    Err(terminal) => {
                        report_end(&ended, LaneRole::InputCursor, terminal).await;
                        return;
                    }
                },
            },
            _ = retry.tick(), if !outbox.is_empty() => flush_input(&lane.sender, &mut outbox),
        }
    }
}

/// Clipboard, HID and USB out and in on the bulk lane.
async fn bulk_lane_worker(
    lanes: LaneConnection,
    mut outgoing: mpsc::Receiver<Message>,
    incoming: mpsc::Sender<Message>,
    ended: mpsc::Sender<LaneTerminal>,
) {
    let mut lane = match open_session_lane(&lanes, LaneRole::BulkAux).await {
        Ok(lane) => lane,
        Err(terminal) => {
            let disabled = matches!(terminal, LaneTerminal::Disabled(_));
            report_end(&ended, LaneRole::BulkAux, terminal).await;
            if disabled {
                refuse_bulk(outgoing).await;
            }
            return;
        }
    };
    let mut pending: Option<Bytes> = None;
    loop {
        let sender = &lane.sender;
        let reader = &mut lane.reader;
        let exit = tokio::select! {
            sent = async {
                match pending.clone() {
                    Some(record) => sender.send(record).await,
                    None => std::future::pending().await,
                }
            } => match sent {
                Ok(()) => {
                    pending = None;
                    continue;
                }
                Err(LaneSendError::Oversize { len, max }) => {
                    tracing::warn!(
                        target: crate::logging::target::TRANSPORT,
                        len,
                        max,
                        "bulk record larger than the bulk lane carries; dropped"
                    );
                    pending = None;
                    continue;
                }
                // The reader sees the lane's exit next.
                Err(_) => continue,
            },
            message = outgoing.recv(), if pending.is_none() => {
                let Some(message) = message else {
                    lane.sender.finish();
                    return;
                };
                pending = message_to_record(message).map(|record| session_record_bytes(&record));
                continue;
            }
            record = reader.next_record() => match record {
                Ok(record @ (SessionRecord::Text(_) | SessionRecord::Binary(_))) => {
                    if record.lane() != LaneRole::BulkAux {
                        tracing::warn!(
                            target: crate::logging::target::TRANSPORT,
                            "non-bulk message on the bulk lane"
                        );
                        let _ = reader.fail(LaneFailure::MalformedFrame);
                        continue;
                    }
                    if incoming.send(record_to_message(record)).await.is_err() {
                        return;
                    }
                    continue;
                }
                Ok(_) => continue,
                Err(exit) => exit,
            },
        };
        match reopen_session_lane(&lanes, LaneRole::BulkAux, &exit).await {
            Ok(replacement) => lane = replacement,
            Err(terminal) => {
                let disabled = matches!(terminal, LaneTerminal::Disabled(_));
                report_end(&ended, LaneRole::BulkAux, terminal).await;
                if disabled {
                    refuse_bulk(outgoing).await;
                }
                return;
            }
        }
    }
}

/// The microphone lane, opened when capture starts. Frames are queued with
/// `try_send`: a full queue drops the frame (the Pier conceals it); the
/// session loop never waits on this lane.
#[derive(Default)]
pub(crate) struct MicrophoneLane {
    writer: Option<LocalLaneWriter>,
}

impl MicrophoneLane {
    /// Queues one microphone frame.
    ///
    /// # Errors
    ///
    /// [`LaneTerminal`] once the microphone lane cannot continue.
    pub(crate) async fn send(
        &mut self,
        lanes: &LaneConnection,
        frame: Bytes,
    ) -> Result<LaneWriteOutcome, LaneTerminal> {
        if self.writer.is_none() {
            self.writer = Some(LocalLaneWriter::open(lanes, LaneRole::Microphone).await?);
        }
        match self.writer.as_mut() {
            Some(writer) => writer.try_send(frame).await,
            None => Err(LaneTerminal::Disabled(LaneRole::Microphone)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_messages_round_trip_through_lane_records() {
        let messages = [
            Message::Text("{\"type\":\"health_ping\"}".to_owned()),
            Message::Binary(vec![0x20, 1]),
            Message::Ping(vec![1]),
            Message::Pong(vec![]),
            Message::Close(None),
            Message::Close(Some(CloseFrame {
                code: CloseCode::Normal,
                reason: "bye".into(),
            })),
        ];
        for message in messages {
            let record = message_to_record(message.clone()).expect("record");
            assert_eq!(record_to_message(record), message);
        }
    }

    #[test]
    fn lane_attachment_is_recognised_by_type_only() {
        assert!(is_lane_attachment(
            "{\"type\":\"lane_attachment\",\"lane_protocol_version\":1}"
        ));
        assert!(!is_lane_attachment("{\"type\":\"server_hello\"}"));
        assert!(!is_lane_attachment("not json"));
    }

    #[test]
    fn input_and_media_messages_route_off_the_control_lane() {
        assert_eq!(
            message_lane(&Message::Text(
                "{\"type\":\"key_event\",\"scan_code\":4}".to_owned()
            )),
            Some(LaneRole::InputCursor)
        );
        assert_eq!(
            message_lane(&Message::Binary(vec![FRAME_CLIPBOARD, 0])),
            Some(LaneRole::BulkAux)
        );
        assert_eq!(
            message_lane(&Message::Text("{\"type\":\"health_ping\"}".to_owned())),
            Some(LaneRole::Control)
        );
        assert_eq!(message_lane(&Message::Ping(vec![])), None);
    }

    const FRAME_CLIPBOARD: u8 = arcen_protocol::FrameType::Clipboard as u8;

    mod isolation {
        //! Deck-side lane isolation on a real QUIC loopback against a
        //! Pier-side lane connection.

        use std::net::SocketAddr;
        use std::num::NonZeroU64;

        use arcen_protocol::lanes::{LaneEndpoint, LaneRoleSet};
        use arcen_transport::quic::{
            accept_session_lanes, connect_session_lanes, grant_attachment,
            lane_transport_config_arc, DirectQuicDialParams, LaneRuntimeConfig, LaneSender,
        };
        use rcgen::{CertificateParams, ExtendedKeyUsagePurpose, KeyPair};
        use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

        use super::super::*;
        use crate::pipeline::frame_queue::incoming_media_inbox;
        use crate::protocol::{encode_audio_header, AudioCodec, AudioHeader};

        const STEP: Duration = Duration::from_secs(5);

        struct Pair {
            _server: quinn::Endpoint,
            _client: quinn::Endpoint,
            pier: LaneConnection,
            deck: LaneConnection,
            _pier_control: SessionLane,
            _deck_control: SessionLane,
        }

        async fn pair() -> Pair {
            let _ = rustls::crypto::ring::default_provider().install_default();
            let key_pair = KeyPair::generate().unwrap();
            let mut params = CertificateParams::new(vec!["localhost".to_string()]).unwrap();
            params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
            let certificate = params.self_signed(&key_pair).unwrap();
            let cert: CertificateDer<'static> = certificate.der().clone();
            let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_pair.serialize_der()));
            let policy = arcen_transport::BoundedTransportPolicy::default();
            let mut server_tls = rustls::ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(vec![cert.clone()], key)
                .unwrap();
            server_tls.alpn_protocols = vec![b"arcen-lanes-test".to_vec()];
            let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(
                quinn::crypto::rustls::QuicServerConfig::try_from(server_tls).unwrap(),
            ));
            server_config.transport_config(lane_transport_config_arc(&policy, LaneEndpoint::Pier));
            let mut roots = rustls::RootCertStore::empty();
            roots.add(cert).unwrap();
            let mut client_tls = rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth();
            client_tls.alpn_protocols = vec![b"arcen-lanes-test".to_vec()];
            let mut client_config = quinn::ClientConfig::new(Arc::new(
                quinn::crypto::rustls::QuicClientConfig::try_from(client_tls).unwrap(),
            ));
            client_config.transport_config(lane_transport_config_arc(&policy, LaneEndpoint::Deck));
            let server = quinn::Endpoint::server(
                server_config,
                "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
            )
            .unwrap();
            let address = server.local_addr().unwrap();
            let client = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
            let accepting = server.clone();
            let accept = tokio::spawn(async move {
                let connection = accepting.accept().await.unwrap().await.unwrap();
                accept_session_lanes(connection, LaneRuntimeConfig::new(LaneEndpoint::Pier), STEP)
                    .await
                    .unwrap()
            });
            let (deck, deck_control) = connect_session_lanes(
                DirectQuicDialParams {
                    endpoint: client.clone(),
                    client_config,
                    remote_addr: address,
                    server_name: "localhost",
                },
                LaneRuntimeConfig::new(LaneEndpoint::Deck),
            )
            .await
            .unwrap();
            let (pier, pier_control) = accept.await.unwrap();
            let grant =
                grant_attachment(&pier, NonZeroU64::new(1).unwrap(), LaneRoleSet::all()).unwrap();
            accept_attachment(&deck, &grant).unwrap();
            Pair {
                _server: server,
                _client: client,
                pier,
                deck,
                _pier_control: pier_control,
                _deck_control: deck_control,
            }
        }

        fn audio_frame(timestamp_ms: u32) -> Bytes {
            let mut bytes = encode_audio_header(AudioHeader {
                codec: AudioCodec::Pcm,
                timestamp_ms,
            })
            .to_vec();
            bytes.extend(std::iter::repeat_n(3_u8, 640));
            Bytes::from(bytes)
        }

        async fn open_writer(pier: &LaneConnection, role: LaneRole) -> LaneSender {
            let lane = tokio::time::timeout(STEP, pier.open_lane(role))
                .await
                .unwrap()
                .unwrap();
            lane.split_default().0.unwrap()
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn audio_and_input_flow_while_the_video_consumer_is_stalled() {
            let pair = pair().await;
            // The video inbox is never drained: a stalled decoder.
            let (media, receiver) = incoming_media_inbox();
            let audio_queue = receiver.audio_lane().clone();
            let (events_tx, mut events) = mpsc::unbounded_channel();
            let (input_tx, input_rx) = mpsc::unbounded_channel();
            let workers = spawn_deck_lane_workers(&pair.deck, media, events_tx, input_rx);

            let video = open_writer(&pair.pier, LaneRole::Video).await;
            let audio = open_writer(&pair.pier, LaneRole::AudioOutput).await;
            let mut pier_input = SessionLane::split(
                tokio::time::timeout(STEP, pair.pier.next_peer_lane(LaneRole::InputCursor))
                    .await
                    .unwrap()
                    .unwrap(),
            )
            .unwrap();
            // A video flood: far more than the inbox keeps.
            let flood = tokio::spawn(async move {
                let frame = Bytes::from(
                    [
                        vec![arcen_protocol::FrameType::VideoH264 as u8],
                        vec![0; 512 * 1024],
                    ]
                    .concat(),
                );
                for _ in 0..200 {
                    if video.send(frame.clone()).await.is_err() {
                        break;
                    }
                }
                video
            });

            let mut worst_audio = Duration::ZERO;
            let mut worst_input = Duration::ZERO;
            let mut worst_cursor = Duration::ZERO;
            for sequence in 0..25_u32 {
                let sent = Instant::now();
                audio.try_send(audio_frame(sequence * 20)).unwrap();
                let batch = tokio::task::spawn_blocking({
                    let audio_queue = audio_queue.clone();
                    move || {
                        let deadline = Instant::now() + STEP;
                        loop {
                            let batch = audio_queue.recv_batch(Duration::from_millis(50));
                            if batch.as_ref().is_some_and(|batch| !batch.is_empty())
                                || Instant::now() > deadline
                            {
                                return batch;
                            }
                        }
                    }
                })
                .await
                .unwrap()
                .unwrap();
                worst_audio = worst_audio.max(sent.elapsed());
                assert!(!batch.is_empty());

                let sent = Instant::now();
                input_tx
                    .send(serde_json::json!({
                        "type": "key_event",
                        "scan_code": 4,
                        "pressed": sequence % 2 == 0,
                        "sequence": sequence + 1,
                    }))
                    .unwrap();
                let record = tokio::time::timeout(STEP, pier_input.reader.next_record())
                    .await
                    .unwrap()
                    .unwrap();
                worst_input = worst_input.max(sent.elapsed());
                assert!(matches!(record, SessionRecord::Text(text) if text.contains("key_event")));

                let sent = Instant::now();
                pier_input
                    .sender
                    .try_send(session_record_bytes(&SessionRecord::Text(
                        "{\"type\":\"cursor_shape\",\"shape\":\"arrow\"}".to_owned(),
                    )))
                    .unwrap();
                loop {
                    let event = tokio::time::timeout(STEP, events.recv())
                        .await
                        .unwrap()
                        .unwrap();
                    if matches!(&event, SessionEvent::Json(value) if crate::protocol::messages::msg_type(value) == Some("cursor_shape"))
                    {
                        break;
                    }
                }
                worst_cursor = worst_cursor.max(sent.elapsed());
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            println!(
                "video stalled: worst audio {:.1} ms, input {:.1} ms, cursor {:.1} ms",
                worst_audio.as_secs_f64() * 1e3,
                worst_input.as_secs_f64() * 1e3,
                worst_cursor.as_secs_f64() * 1e3
            );
            assert!(worst_audio < Duration::from_millis(250), "{worst_audio:?}");
            assert!(worst_input < Duration::from_millis(250), "{worst_input:?}");
            assert!(
                worst_cursor < Duration::from_millis(250),
                "{worst_cursor:?}"
            );
            // The video inbox overflowed and dropped instead of growing.
            assert!(receiver.snapshot().video_dropped_packets > 0 || !flood.is_finished());
            flood.abort();
            drop(workers);
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn a_disabled_bulk_lane_turns_bulk_off_without_failing_its_senders() {
            let pair = pair().await;
            let (media, _receiver) = incoming_media_inbox();
            let (events_tx, _events) = mpsc::unbounded_channel();
            let (_input_tx, input_rx) = mpsc::unbounded_channel();
            let mut workers = spawn_deck_lane_workers(&pair.deck, media, events_tx, input_rx);
            // The Pier fails every bulk lane the Deck opens until the role's
            // recovery budget is spent.
            let mut pier_bulk = PeerLaneReader::new(&pair.pier, LaneRole::BulkAux);
            let failing = tokio::spawn(async move {
                while let Ok(event) = pier_bulk.next().await {
                    if matches!(event, PeerLaneEvent::Opened { .. }) {
                        pier_bulk.fail(LaneFailure::MalformedFrame);
                    }
                }
            });
            let terminal = tokio::time::timeout(STEP, workers.ended.recv())
                .await
                .expect("the bulk lane is disabled in time")
                .expect("a terminal");
            failing.abort();
            assert_eq!(terminal, LaneTerminal::Disabled(LaneRole::BulkAux));
            // Clipboard and USB sends are refused quietly, never as an error
            // that would end the session.
            for _ in 0..(BULK_CHANNEL * 2) {
                tokio::time::timeout(
                    STEP,
                    workers.bulk_out.send(Message::Text(
                        "{\"type\":\"clipboard_data\",\"sequence\":1}".to_owned(),
                    )),
                )
                .await
                .expect("never blocks")
                .expect("never fails");
            }
            assert!(workers.ended.try_recv().is_err());
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn a_replaced_video_lane_requests_recovery_and_audio_keeps_flowing() {
            let pair = pair().await;
            let (media, receiver) = incoming_media_inbox();
            let audio_queue = receiver.audio_lane().clone();
            let (events_tx, mut events) = mpsc::unbounded_channel();
            let (_input_tx, input_rx) = mpsc::unbounded_channel();
            let _workers = spawn_deck_lane_workers(&pair.deck, media, events_tx, input_rx);
            let video = open_writer(&pair.pier, LaneRole::Video).await;
            let audio = open_writer(&pair.pier, LaneRole::AudioOutput).await;
            let failed = video.lane_generation();
            // The Deck must hold the first lane before it fails: a lane the
            // Deck never saw replaces nothing and owes no recovery.
            video
                .try_send(Bytes::from(vec![
                    arcen_protocol::FrameType::VideoH264 as u8,
                    0,
                ]))
                .unwrap();
            let deadline = Instant::now() + STEP;
            while pair.deck.lane_state(LaneRole::Video)
                != arcen_transport::lanes::LaneState::Open(failed)
            {
                assert!(
                    Instant::now() < deadline,
                    "the Deck never saw the video lane"
                );
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            let _ = pair.pier.report_failure(
                LaneRole::Video,
                failed,
                arcen_transport::lanes::LaneFailure::WriteFailed,
            );
            let _replacement = open_writer(&pair.pier, LaneRole::Video).await;
            loop {
                let event = tokio::time::timeout(STEP, events.recv())
                    .await
                    .unwrap()
                    .unwrap();
                if matches!(event, SessionEvent::PresentationRecoveryRequested) {
                    break;
                }
            }
            audio.try_send(audio_frame(0)).unwrap();
            let batch = tokio::task::spawn_blocking(move || {
                let deadline = Instant::now() + STEP;
                loop {
                    let batch = audio_queue.recv_batch(Duration::from_millis(50));
                    if batch.as_ref().is_some_and(|batch| !batch.is_empty())
                        || Instant::now() > deadline
                    {
                        return batch;
                    }
                }
            })
            .await
            .unwrap()
            .unwrap();
            assert_eq!(batch.len(), 1);
            assert_eq!(
                pair.deck.lane_state(LaneRole::AudioOutput),
                arcen_transport::lanes::LaneState::Open(audio.lane_generation())
            );
        }
    }
}
